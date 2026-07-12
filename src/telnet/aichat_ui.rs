//! AI chat client UI: Groq prompt/response loop and paginated answer
//! display.
//!
//! Split out of `telnet/mod.rs`; behaviour unchanged.

use super::*;

impl TelnetSession {
    // ─── AI CHAT ────────────────────────────────────────────

    /// Lines of answer content per page (screen minus header/footer).
    pub(in crate::telnet) const PAGE_CONTENT_LINES: usize = 14;

    pub(in crate::telnet) async fn ai_chat(&mut self, api_key: &str) -> Result<(), std::io::Error> {
        let content_width = if self.terminal_type == TerminalType::Petscii {
            PETSCII_WIDTH - 2
        } else {
            78
        };

        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;
        self.send_line(&format!("  {}", self.yellow("AI CHAT")))
            .await?;
        self.send_line(&sep).await?;
        self.send_line("").await?;
        self.send_line(&format!(
            "  {}",
            self.dim("Type a question, or Q to exit.")
        ))
        .await?;
        self.send_line("").await?;
        self.send(&format!("  {}: ", self.cyan("Q")))
            .await?;
        self.flush().await?;

        let mut question = match self.get_line_input().await? {
            Some(s) if !s.is_empty() && !s.eq_ignore_ascii_case("q") => s,
            _ => return Ok(()),
        };

        loop {
            // Inline "Thinking..." on the current screen rather than
            // doing a full clear + banner redraw — at 1200 baud the
            // extra wipe is a visible flicker before the answer page
            // (which does its own clear) replaces it anyway.
            self.send_line(&format!("  {}...", self.dim("Thinking")))
                .await?;
            self.flush().await?;

            let key = api_key.to_string();
            let q = question.clone();
            let result = tokio::task::spawn_blocking(move || {
                crate::aichat::ask(&key, &q)
            })
            .await
            .map_err(|e| {
                std::io::Error::other(e.to_string())
            })?;

            match result {
                Ok(answer) => {
                    // Normalize CR / CRLF to LF first — `.lines()` splits on
                    // \n and \r\n but leaves a bare \r mid-string, where a
                    // prompt-injected reply could use it to overwrite the
                    // prompt on ANSI terminals.  Then strip control bytes,
                    // ESC, and IAC per line so the LLM can't smuggle cursor
                    // moves, screen wipes, or telnet commands through the
                    // chat surface.
                    let normalized = answer.replace("\r\n", "\n").replace('\r', "\n");
                    let lines: Vec<String> = normalized
                        .lines()
                        .map(crate::aichat::sanitize_for_terminal)
                        .flat_map(|line| crate::aichat::wrap_line(&line, content_width))
                        .collect();

                    match self.ai_show_answer(&question, &lines).await? {
                        Some(next_q) => question = next_q,
                        None => return Ok(()),
                    }
                }
                Err(e) => {
                    let max_w = if self.terminal_type == TerminalType::Petscii {
                        30
                    } else {
                        50
                    };
                    self.show_error(&truncate_to_width(&e, max_w)).await?;
                    return Ok(());
                }
            }
        }
    }

    /// Display a paginated AI answer. Returns `Some(question)` if the user
    /// typed a new question, or `None` to exit.
    pub(in crate::telnet) async fn ai_show_answer(
        &mut self,
        question: &str,
        lines: &[String],
    ) -> Result<Option<String>, std::io::Error> {
        let page_h = Self::PAGE_CONTENT_LINES;
        let content_max = if self.terminal_type == TerminalType::Petscii {
            PETSCII_WIDTH - 2
        } else {
            78
        };
        let mut scroll = 0usize;

        loop {
            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;

            let max_q = if self.terminal_type == TerminalType::Petscii {
                34
            } else {
                52
            };
            let q_display = truncate_to_width(question, max_q);
            self.send_line(&format!(
                "  {}",
                self.yellow(&format!("Q: {}", q_display))
            ))
            .await?;
            self.send_line(&sep).await?;

            let total = lines.len();
            let end = (scroll + page_h).min(total);
            let page_lines = &lines[scroll..end];
            for line in page_lines {
                let safe = truncate_to_width(line, content_max);
                self.send_line(&format!("  {}", safe)).await?;
            }
            for _ in (end - scroll)..page_h {
                self.send_line("").await?;
            }

            let has_prev = scroll > 0;
            let has_next = end < total;
            self.send_line(&format!(
                "  {}",
                self.dim(&format!("({}-{} of {})", scroll + 1, end, total))
            ))
            .await?;
            let mut parts = Vec::new();
            if has_prev {
                parts.push(self.action_prompt("P", "Pv"));
            }
            if has_next {
                parts.push(self.action_prompt("N", "Nx"));
            }
            parts.push(self.action_prompt("Q", "Done"));
            parts.push(self.action_prompt("H", "Help"));
            self.send_line(&format!("  {}", parts.join(" ")))
                .await?;
            self.send(&format!("  {}: ", self.cyan(">")))
                .await?;
            self.flush().await?;

            // Read a full line before acting.  A lone command letter
            // (Q/N/P/H by itself, then Enter) navigates; anything longer
            // is sent to the AI as a new question — so a follow-up that
            // merely starts with a command letter (e.g. "Quantum...") is
            // no longer swallowed by the menu.  ESC / disconnect → None.
            let input = match self.get_line_input().await? {
                Some(s) => s,
                None => return Ok(None),
            };
            if input.is_empty() {
                continue;
            }
            // Only a one-character line CAN be a command, and only when
            // it would actually do something — `n` on the last page or
            // `p` on the first page falls through to the question path
            // instead of silently no-op'ing.  Q and H always act.
            let cmd = if input.chars().count() == 1 {
                let c = input.chars().next().unwrap().to_ascii_lowercase();
                match c {
                    'q' | 'h' => c,
                    'n' if has_next => c,
                    'p' if has_prev => c,
                    _ => '\0',
                }
            } else {
                '\0'
            };

            match cmd {
                'n' => { if has_next { scroll += page_h; } }
                'p' => { if has_prev { scroll = scroll.saturating_sub(page_h); } }
                'q' => { return Ok(None); }
                'h' => {
                    self.show_help_page("AI CHAT HELP", Self::ai_chat_help_lines())
                        .await?;
                }
                _ => {
                    // Not a navigation command — send the whole line to
                    // the AI as a new question.
                    return Ok(Some(input));
                }
            }
        }
    }
}
