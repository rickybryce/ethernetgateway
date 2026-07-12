//! Text-mode web browser UI: page render, link/URL navigation,
//! bookmarks, in-page search, and HTML form fill/submit.
//!
//! Split out of `telnet/mod.rs`; behaviour unchanged. The shared
//! `*_help_lines()` fns (including the browser help screens) remain in
//! `telnet/mod.rs` as the centralized help cluster.

use super::*;

impl TelnetSession {
    // ─── WEB BROWSER ────────────────────────────────────────

    pub(in crate::telnet) const WEB_MAX_HISTORY: usize = 50;

    /// Number of content lines per page.
    /// Total screen budget is 22 rows: header (sep + title + sep = 3) +
    /// content + blank (1) + footer (position + url + nav1 + nav2 = 4) + prompt (1) = 9 overhead.
    /// 22 - 9 = 13 content lines.
    pub(in crate::telnet) const WEB_PAGE_HEIGHT: usize = 13;

    /// Content width for HTML rendering.
    /// Slightly narrower than the display to leave room for link number suffixes
    /// like `[12]` that are appended after html2text wraps.
    pub(in crate::telnet) fn web_content_width(&self) -> usize {
        if self.terminal_type == TerminalType::Petscii {
            33 // 40 - 2 indent - 5 for "[NNN]"
        } else {
            73 // 80 - 2 indent - 5 for "[NNN]"
        }
    }

    pub(in crate::telnet) async fn render_web_browser(&mut self) -> Result<(), std::io::Error> {
        // Auto-load homepage on first visit if configured
        if self.web_lines.is_empty() && self.web_url.is_none() {
            let cfg = config::get_config();
            if !cfg.browser_homepage.is_empty() {
                let url = crate::webbrowser::normalize_url(&cfg.browser_homepage);
                self.web_fetch_page(&url, false).await?;
            }
        }

        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;

        if self.web_lines.is_empty() {
            // Home screen — no page loaded
            self.send_line(&format!("  {}", self.yellow("WEB BROWSER"))).await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;
            self.send_line(&format!("  {}", self.dim("Try:"))).await?;
            self.send_line(&format!("  {}",
                self.dim("  http://telnetbible.com")
            )).await?;
            self.send_line(&format!("  {}",
                self.dim("  gopher://gopher.floodgap.com")
            )).await?;
            self.send_line("").await?;
            self.send_line(&format!("  {} {} {} {}",
                self.action_prompt("G", "Go/Search"),
                self.action_prompt("K", "Bookmarks"),
                self.action_prompt("Q", "Back"),
                self.action_prompt("H", "Help"),
            )).await?;
        } else {
            // Page view — show title + paginated content
            let title_display = match &self.web_title {
                Some(t) => {
                    let max_w = if self.terminal_type == TerminalType::Petscii { 34 } else { 52 };
                    crate::webbrowser::truncate_to_width(t, max_w)
                }
                None => "Web Browser".to_string(),
            };
            self.send_line(&format!("  {}", self.yellow(&title_display))).await?;
            self.send_line(&sep).await?;

            let page_h = Self::WEB_PAGE_HEIGHT;
            let total = self.web_lines.len();
            // Defensive clamp: never let a scroll position index past the
            // current page — guarantees the page_lines slice below can't
            // panic regardless of how web_scroll was set.
            let start = self.web_scroll.min(total.saturating_sub(1));
            let end = (start + page_h).min(total);

            let content_max = if self.terminal_type == TerminalType::Petscii {
                PETSCII_WIDTH - 2
            } else {
                78
            };
            let page_lines: Vec<String> = self.web_lines[start..end].to_vec();
            for line in &page_lines {
                let safe = crate::webbrowser::truncate_to_width(line, content_max);
                let colored = self.colorize_link_markers(&safe);
                self.send_line(&format!("  {}", colored)).await?;
            }
            self.send_line("").await?;

            // Status line
            let has_prev = start > 0;
            let has_next = end < total;
            let url_display = match &self.web_url {
                Some(u) => {
                    let max_w = if self.terminal_type == TerminalType::Petscii { 36 } else { 54 };
                    crate::webbrowser::truncate_to_width(u, max_w)
                }
                None => String::new(),
            };
            self.send_line(&format!("  {}", self.dim(&format!("({}-{} of {})", start + 1, end, total)))).await?;
            if !self.web_forms.is_empty() {
                let form_count = self.web_forms.len();
                let form_hint = if form_count == 1 {
                    "1 form on this page (F to edit)".to_string()
                } else {
                    format!("{} forms on this page (F to edit)", form_count)
                };
                self.send_line(&format!("  {}", self.amber(&form_hint))).await?;
            } else {
                self.send_line(&format!("  {}", self.dim(&url_display))).await?;
            }

            // Navigation footer — two rows to fit all commands
            let is_petscii = self.terminal_type == TerminalType::Petscii;
            let has_forms = !self.web_forms.is_empty();
            // Row 1: navigation
            let mut nav = Vec::new();
            if has_prev { nav.push(self.action_prompt("P", "Pv")); }
            if has_next { nav.push(self.action_prompt("N", "Nx")); }
            nav.push(self.action_prompt("T", "Top"));
            nav.push(self.action_prompt("E", "End"));
            nav.push(self.action_prompt("S", "Find"));
            if !is_petscii {
                nav.push(self.action_prompt("G", "Go"));
            }
            self.send_line(&format!("  {}", nav.join(" "))).await?;
            // Row 2: actions
            let mut act = Vec::new();
            if is_petscii {
                act.push(self.action_prompt("G", "Go"));
            }
            if !self.web_links.is_empty() {
                act.push(self.action_prompt("L", "Lk"));
            }
            if has_forms {
                act.push(self.action_prompt("F", "Fm"));
            }
            act.push(self.action_prompt("K", "Bm"));
            act.push(self.action_prompt("H", "?"));
            if !self.web_history.is_empty() {
                act.push(self.action_prompt("B", "Bk"));
            }
            act.push(self.action_prompt("Q", "X"));
            self.send_line(&format!("  {}", act.join(" "))).await?;
        }
        Ok(())
    }

    pub(in crate::telnet) async fn handle_web_browser_command(&mut self, input: &str) -> Result<bool, std::io::Error> {
        if self.web_lines.is_empty() {
            // Home screen commands
            match input {
                "g" => {
                    self.web_prompt_url().await?;
                }
                "k" => {
                    self.web_show_bookmarks().await?;
                }
                "h" => {
                    self.web_show_help(false).await?;
                }
                "q" => {
                    self.web_reset();
                    self.current_menu = Menu::Main;
                }
                "r" => {} // just redraw
                _ => {
                    self.show_error("Press G, K, H, or Q.").await?;
                }
            }
        } else {
            // Page view commands
            match input {
                "q" => {
                    // Close page, return to browser home
                    self.web_lines.clear();
                    self.web_scroll = 0;
                }
                "r" => {
                    if let Some(url) = self.web_url.clone() {
                        self.web_fetch_page(&url, false).await?;
                    }
                }
                "n" => {
                    let page_h = Self::WEB_PAGE_HEIGHT;
                    let total = self.web_lines.len();
                    if self.web_scroll + page_h < total {
                        self.web_scroll += page_h;
                    } else {
                        self.show_error("End of page.").await?;
                    }
                }
                "p" => {
                    if self.web_scroll > 0 {
                        let page_h = Self::WEB_PAGE_HEIGHT;
                        self.web_scroll = self.web_scroll.saturating_sub(page_h);
                    } else {
                        self.show_error("Top of page.").await?;
                    }
                }
                "t" => {
                    self.web_scroll = 0;
                }
                "e" => {
                    let page_h = Self::WEB_PAGE_HEIGHT;
                    let total = self.web_lines.len();
                    if total > page_h {
                        self.web_scroll = total - page_h;
                    } else {
                        self.web_scroll = 0;
                    }
                }
                "g" => {
                    self.web_prompt_url().await?;
                }
                "l" => {
                    self.web_prompt_link().await?;
                }
                "s" => {
                    self.web_search_in_page().await?;
                }
                "k" => {
                    self.web_save_bookmark().await?;
                }
                "f" => {
                    self.web_show_forms().await?;
                }
                "h" => {
                    self.web_show_help(true).await?;
                }
                "b" => {
                    if let Some((prev_url, prev_scroll)) = self.web_history.last().cloned() {
                        if self.web_fetch_page(&prev_url, false).await? {
                            // Clamp: the re-fetched page may be shorter than
                            // it was when we saved prev_scroll (dynamic pages),
                            // and an out-of-range scroll panics the render slice.
                            self.web_scroll =
                                prev_scroll.min(self.web_lines.len().saturating_sub(1));
                            self.web_history.pop();
                        }
                    } else {
                        self.show_error("No history.").await?;
                    }
                }
                _ => {
                    self.show_error("Unknown command.").await?;
                }
            }
        }
        Ok(true)
    }

    pub(in crate::telnet) async fn web_prompt_url(&mut self) -> Result<(), std::io::Error> {
        self.send_line("").await?;
        self.send(&format!("  {}: ", self.cyan("URL/Search"))).await?;
        self.flush().await?;

        let url_input = match self.get_line_input().await? {
            Some(s) if !s.is_empty() => s,
            _ => return Ok(()),
        };

        let url = crate::webbrowser::normalize_url(&url_input);
        self.web_fetch_page(&url, true).await?;
        Ok(())
    }

    pub(in crate::telnet) async fn web_prompt_link(&mut self) -> Result<(), std::io::Error> {
        if self.web_links.is_empty() {
            self.show_error("No links on this page.").await?;
            return Ok(());
        }

        self.send_line("").await?;
        self.send(&format!("  {} (1-{}): ", self.cyan("Link #"), self.web_links.len())).await?;
        self.flush().await?;

        let input = match self.get_line_input().await? {
            Some(s) if !s.is_empty() => s,
            _ => return Ok(()),
        };

        // Drain any stray bytes (e.g. NUL from telnet CR+NUL) before following
        self.drain_input().await;

        if let Ok(num) = input.parse::<usize>() {
            self.web_follow_link(num).await?;
        } else {
            self.show_error("Enter a number.").await?;
        }
        Ok(())
    }

    pub(in crate::telnet) async fn web_follow_link(&mut self, num: usize) -> Result<(), std::io::Error> {
        if num >= 1 && num <= self.web_links.len() {
            let link = self.web_links[num - 1].clone();
            let resolved = match &self.web_url {
                Some(base) => crate::webbrowser::resolve_url(base, &link),
                None => crate::webbrowser::normalize_url(&link),
            };
            self.web_fetch_page(&resolved, true).await?;
        } else {
            self.show_error(&format!("Link {} not found.", num)).await?;
        }
        Ok(())
    }

    pub(in crate::telnet) async fn web_fetch_page(&mut self, url: &str, push_history: bool) -> Result<bool, std::io::Error> {
        // Gopher search URLs need a query term before fetching
        let url = if crate::webbrowser::is_gopher_search(url) {
            self.send_line("").await?;
            self.send(&format!("  {}: ", self.cyan("Search"))).await?;
            self.flush().await?;
            let query = match self.get_line_input().await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(false),
            };
            crate::webbrowser::build_gopher_search_url(url, &query)
        } else {
            url.to_string()
        };

        self.send_line("").await?;
        self.send_line(&format!("  {}...", self.dim("Loading"))).await?;
        self.flush().await?;

        let width = self.web_content_width();
        let url_owned = url.clone();
        let is_gopher = url.starts_with("gopher://");

        let result = tokio::task::spawn_blocking(move || {
            if is_gopher {
                crate::webbrowser::fetch_gopher(&url_owned, width)
            } else {
                crate::webbrowser::fetch_and_render(&url_owned, width)
            }
        })
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?;

        self.web_apply_result(result, push_history).await
    }

    pub(in crate::telnet) async fn web_show_help(&mut self, page_view: bool) -> Result<(), std::io::Error> {
        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;
        self.send_line(&format!("  {}", self.yellow("BROWSER HELP"))).await?;
        self.send_line(&sep).await?;

        if page_view {
            let is_petscii = self.terminal_type == TerminalType::Petscii;
            // Intro (dim): link-number explanation, width-specific wording.
            if is_petscii {
                self.send_line(&format!("  {}",
                    self.dim("[1] [2] etc. next to text")
                )).await?;
                self.send_line(&format!("  {}",
                    self.dim("are links to other pages.")
                )).await?;
            } else {
                self.send_line(&format!("  {}",
                    self.dim("[1], [2], etc. next to text are links")
                )).await?;
                self.send_line(&format!("  {}",
                    self.dim("to other pages.")
                )).await?;
            }
            self.send_line("").await?;
            for line in Self::browser_page_help_lines(is_petscii) {
                self.send_line(line).await?;
            }
        } else {
            for line in Self::browser_menu_help_lines() {
                self.send_line(line).await?;
            }
            self.send_line("").await?;
            self.send_line(&format!("  {}",
                self.dim("Examples:")
            )).await?;
            self.send_line(&format!("  {}",
                self.dim("  http://telnetbible.com")
            )).await?;
            self.send_line(&format!("  {}",
                self.dim("  gopher://gopher.floodgap.com")
            )).await?;
            self.send_line(&format!("  {}",
                self.dim("  rust programming (search)")
            )).await?;
        }

        self.send_line("").await?;
        self.send("  Press any key to continue.").await?;
        self.flush().await?;
        self.wait_for_key().await?;
        Ok(())
    }

    pub(in crate::telnet) async fn web_save_bookmark(&mut self) -> Result<(), std::io::Error> {
        if let Some(url) = &self.web_url {
            let title = self.web_title.as_deref().unwrap_or("Untitled");
            if crate::webbrowser::add_bookmark(url, title) {
                self.send_line("").await?;
                self.send_line(&format!("  {}", self.green("Bookmark saved."))).await?;
                self.send_line("").await?;
                self.send("  Press any key to continue.").await?;
                self.flush().await?;
                self.wait_for_key().await?;
            } else {
                self.show_error("Already bookmarked (or full).").await?;
            }
        } else {
            self.show_error("No page to bookmark.").await?;
        }
        Ok(())
    }

    pub(in crate::telnet) async fn web_show_bookmarks(&mut self) -> Result<(), std::io::Error> {
        let bookmarks = crate::webbrowser::load_bookmarks();
        if bookmarks.is_empty() {
            self.show_error("No bookmarks saved.").await?;
            return Ok(());
        }

        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;
        self.send_line(&format!("  {}", self.yellow("BOOKMARKS"))).await?;
        self.send_line(&sep).await?;
        self.send_line("").await?;

        let max_title = if self.terminal_type == TerminalType::Petscii { 30 } else { 60 };
        let display_max = bookmarks.len().min(Self::WEB_PAGE_HEIGHT);
        for (i, bm) in bookmarks.iter().take(display_max).enumerate() {
            let title = crate::webbrowser::truncate_to_width(&bm.title, max_title);
            self.send_line(&format!("  {:>2}. {}", i + 1, title)).await?;
        }
        if bookmarks.len() > display_max {
            self.send_line(&format!("  {} more...", bookmarks.len() - display_max)).await?;
        }

        self.send_line("").await?;
        self.send_line(&format!("  {} {} {}",
            self.dim("#=Open"),
            self.action_prompt("D", "Delete"),
            self.action_prompt("H", "Help"),
        )).await?;
        self.send(&format!("  {}: ", self.cyan("#/D"))).await?;
        self.flush().await?;

        let input = match self.get_line_input().await? {
            Some(s) if !s.is_empty() => s,
            _ => return Ok(()),
        };

        if input == "h" {
            self.show_help_page("BOOKMARKS HELP", Self::bookmarks_help_lines())
                .await?;
        } else if input == "d" {
            // Delete mode
            self.send(&format!("  {} (1-{}): ", self.cyan("Delete #"), display_max)).await?;
            self.flush().await?;
            let del_input = match self.get_line_input().await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(()),
            };
            if let Ok(num) = del_input.parse::<usize>() {
                if num >= 1 && num <= display_max {
                    crate::webbrowser::remove_bookmark(num - 1);
                    self.send_line(&format!("  {}", self.green("Deleted."))).await?;
                    self.send_line("").await?;
                    self.send("  Press any key to continue.").await?;
                    self.flush().await?;
                    self.wait_for_key().await?;
                } else {
                    self.show_error("Invalid number.").await?;
                }
            }
        } else if let Ok(num) = input.parse::<usize>() {
            if num >= 1 && num <= display_max {
                let url = bookmarks[num - 1].url.clone();
                self.web_fetch_page(&url, true).await?;
            } else {
                self.show_error("Invalid number.").await?;
            }
        }
        Ok(())
    }

    pub(in crate::telnet) async fn web_search_in_page(&mut self) -> Result<(), std::io::Error> {
        self.send_line("").await?;
        self.send(&format!("  {}: ", self.cyan("Find"))).await?;
        self.flush().await?;

        let query = match self.get_line_input().await? {
            Some(s) if !s.is_empty() => s.to_ascii_lowercase(),
            _ => return Ok(()),
        };

        // Search from line after current scroll position, then wrap around
        let total = self.web_lines.len();
        let start_line = self.web_scroll + 1;
        for offset in 0..total {
            let idx = (start_line + offset) % total;
            if self.web_lines[idx].to_ascii_lowercase().contains(&query) {
                // Scroll to put the match at the top of the page
                self.web_scroll = idx;
                return Ok(());
            }
        }

        self.show_error("Not found.").await?;
        Ok(())
    }

    pub(in crate::telnet) async fn web_show_forms(&mut self) -> Result<(), std::io::Error> {
        if self.web_forms.is_empty() {
            self.show_error("No forms on this page.").await?;
            return Ok(());
        }

        if self.web_forms.len() == 1 {
            return self.web_edit_form(0).await;
        }

        self.send_line("").await?;
        self.send_line(&format!("  {}", self.yellow("FORMS"))).await?;
        let forms_snapshot: Vec<String> = self.web_forms.iter().enumerate().map(|(i, form)| {
            let label = crate::webbrowser::truncate_to_width(&form.label, 30);
            format!("  {}. {}", i + 1, label)
        }).collect();
        for line in &forms_snapshot {
            self.send_line(line).await?;
        }
        self.send_line("").await?;
        self.send(&format!("  {} (1-{}): ", self.cyan("Form #"), self.web_forms.len())).await?;
        self.flush().await?;

        let input = match self.get_line_input().await? {
            Some(s) if !s.is_empty() => s,
            _ => return Ok(()),
        };

        if let Ok(num) = input.parse::<usize>() {
            if num >= 1 && num <= self.web_forms.len() {
                self.web_edit_form(num - 1).await?;
            } else {
                self.show_error("Invalid form number.").await?;
            }
        } else {
            self.show_error("Enter a number.").await?;
        }
        Ok(())
    }

    pub(in crate::telnet) async fn web_edit_form(&mut self, form_idx: usize) -> Result<(), std::io::Error> {
        let mut form = self.web_forms[form_idx].clone();

        // If the form has no visible fields (only hidden), submit immediately
        let has_visible = form.fields.iter().any(|f| !matches!(f, crate::webbrowser::FormField::Hidden { .. }));
        if !has_visible {
            self.web_forms[form_idx] = form;
            return self.web_submit_form(form_idx).await;
        }

        loop {
            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            let title = crate::webbrowser::truncate_to_width(&form.label, 34);
            self.send_line(&format!("  {}", self.yellow(&title))).await?;
            self.send_line(&sep).await?;

            let mut field_num = 0usize;
            let is_petscii = self.terminal_type == TerminalType::Petscii;
            let max_label = if is_petscii { 12 } else { 20 };
            let max_val = if is_petscii { 18 } else { 40 };

            let display_lines: Vec<String> = form.fields.iter().filter_map(|field| {
                match field {
                    crate::webbrowser::FormField::Hidden { .. } => None,
                    crate::webbrowser::FormField::Text { label, value, .. }
                    | crate::webbrowser::FormField::TextArea { label, value, .. } => {
                        field_num += 1;
                        // Sanitize the value for display only — the stored
                        // `value` is submitted verbatim, so we must not strip
                        // control bytes from it (M-8; labels/option-text are
                        // already sanitized in WebPage::sanitize).
                        let display_val = if value.is_empty() {
                            "(empty)".to_string()
                        } else {
                            crate::aichat::sanitize_for_terminal(value)
                        };
                        Some(format!("  {}.{}: {}",
                            field_num,
                            crate::webbrowser::truncate_to_width(label, max_label),
                            crate::webbrowser::truncate_to_width(&display_val, max_val),
                        ))
                    }
                    crate::webbrowser::FormField::Select { label, options, selected, .. } => {
                        field_num += 1;
                        let chosen = options.get(*selected).map(|(_, t)| t.as_str()).unwrap_or("?");
                        Some(format!("  {}.{}: {}",
                            field_num,
                            crate::webbrowser::truncate_to_width(label, max_label),
                            crate::webbrowser::truncate_to_width(chosen, max_val),
                        ))
                    }
                    crate::webbrowser::FormField::Checkbox { label, checked, .. } => {
                        field_num += 1;
                        let mark = if *checked { "[X]" } else { "[ ]" };
                        Some(format!("  {}.{}: {}",
                            field_num,
                            crate::webbrowser::truncate_to_width(label, max_label),
                            mark,
                        ))
                    }
                    crate::webbrowser::FormField::Radio { label, checked, .. } => {
                        field_num += 1;
                        let mark = if *checked { "(X)" } else { "( )" };
                        Some(format!("  {}.{}: {}",
                            field_num,
                            crate::webbrowser::truncate_to_width(label, max_label),
                            mark,
                        ))
                    }
                }
            }).collect();

            for line in &display_lines {
                self.send_line(line).await?;
            }

            self.send_line("").await?;
            self.send_line(&format!("  {} {} {} {}",
                self.action_prompt("S", "Submit"),
                self.dim("#=Edit"),
                self.action_prompt("Q", "Cancel"),
                self.action_prompt("H", "Help"),
            )).await?;
            self.send(&format!("  {}: ", self.cyan("#/S/Q"))).await?;
            self.flush().await?;

            let input = match self.get_line_input().await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(()),
            };

            match input.as_str() {
                "s" => {
                    self.web_forms[form_idx] = form;
                    return self.web_submit_form(form_idx).await;
                }
                "q" => return Ok(()),
                "h" => {
                    self.show_help_page("FORM HELP", Self::form_help_lines())
                        .await?;
                }
                other => {
                    if let Ok(num) = other.parse::<usize>() {
                        if let Some(real_idx) = crate::webbrowser::visible_field_index(&form.fields, num) {
                            self.web_edit_field(&mut form, real_idx).await?;
                        } else {
                            self.show_error("Invalid field number.").await?;
                        }
                    } else {
                        self.show_error("Enter S, Q, H, or a field #.").await?;
                    }
                }
            }
        }
    }

    pub(in crate::telnet) async fn web_edit_field(&mut self, form: &mut crate::webbrowser::WebForm, idx: usize) -> Result<(), std::io::Error> {
        use crate::webbrowser::FormField;

        let (is_text, is_password, is_select, is_checkbox, is_radio, label_str, opt_count) = {
            let field = &form.fields[idx];
            match field {
                FormField::Text { label, input_type, .. } => {
                    (true, input_type == "password", false, false, false, label.clone(), 0)
                }
                FormField::TextArea { label, .. } => {
                    (true, false, false, false, false, label.clone(), 0)
                }
                FormField::Select { options, .. } => {
                    (false, false, true, false, false, String::new(), options.len())
                }
                FormField::Checkbox { .. } => {
                    (false, false, false, true, false, String::new(), 0)
                }
                FormField::Radio { name, .. } => {
                    (false, false, false, false, true, name.clone(), 0)
                }
                FormField::Hidden { .. } => {
                    return Ok(());
                }
            }
        };

        if is_text {
            self.send_line("").await?;
            self.send(&format!("  {}: ", self.cyan(&label_str))).await?;
            self.flush().await?;
            let input = if is_password {
                self.get_password_input().await?
            } else {
                self.get_line_input().await?
            };
            if let Some(new_val) = input {
                match &mut form.fields[idx] {
                    FormField::Text { value, .. } | FormField::TextArea { value, .. } => {
                        *value = new_val;
                    }
                    _ => {}
                }
            }
        } else if is_select {
            self.send_line("").await?;
            let opts_snapshot: Vec<(String, bool)> = if let FormField::Select { options, selected, .. } = &form.fields[idx] {
                options.iter().enumerate().map(|(i, (_, display))| {
                    (display.clone(), i == *selected)
                }).collect()
            } else {
                Vec::new()
            };
            for (i, (display, is_sel)) in opts_snapshot.iter().enumerate() {
                let marker = if *is_sel { ">" } else { " " };
                self.send_line(&format!("  {}{}.{}",
                    marker, i + 1,
                    crate::webbrowser::truncate_to_width(display, 30),
                )).await?;
            }
            self.send(&format!("  {} (1-{}): ", self.cyan("Pick"), opt_count)).await?;
            self.flush().await?;
            if let Some(input) = self.get_line_input().await?
                && let Ok(n) = input.parse::<usize>()
                    && n >= 1 && n <= opt_count
                        && let FormField::Select { selected, .. } = &mut form.fields[idx] {
                            *selected = n - 1;
                        }
        } else if is_checkbox {
            if let FormField::Checkbox { checked, .. } = &mut form.fields[idx] {
                *checked = !*checked;
            }
        } else if is_radio {
            let radio_name = label_str;
            for f in form.fields.iter_mut() {
                if let FormField::Radio { name, checked, .. } = f
                    && *name == radio_name {
                        *checked = false;
                    }
            }
            if let FormField::Radio { checked, .. } = &mut form.fields[idx] {
                *checked = true;
            }
        }
        Ok(())
    }

    pub(in crate::telnet) async fn web_submit_form(&mut self, form_idx: usize) -> Result<(), std::io::Error> {
        self.send_line("").await?;
        self.send_line(&format!("  {}...", self.dim("Submitting"))).await?;
        self.flush().await?;

        let form = self.web_forms[form_idx].clone();
        let base = self.web_url.clone().unwrap_or_default();
        let width = self.web_content_width();

        let result = tokio::task::spawn_blocking(move || {
            crate::webbrowser::submit_form(&base, &form, width)
        })
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?;

        self.web_apply_result(result, true).await?;
        Ok(())
    }

    pub(in crate::telnet) async fn web_apply_result(
        &mut self,
        result: Result<crate::webbrowser::WebPage, String>,
        push_history: bool,
    ) -> Result<bool, std::io::Error> {
        match result {
            Ok(page) => {
                if push_history
                    && let Some(old_url) = self.web_url.as_ref() {
                        self.web_history.push((old_url.clone(), self.web_scroll));
                        if self.web_history.len() > Self::WEB_MAX_HISTORY {
                            self.web_history.remove(0);
                        }
                    }
                self.web_url = Some(page.url);
                self.web_title = page.title;
                self.web_lines = page.lines;
                self.web_links = page.links;
                self.web_forms = page.forms;
                self.web_scroll = 0;
                Ok(true)
            }
            Err(e) => {
                // Sanitize before display: a fetch error can echo remote-derived
                // bytes (e.g. "Bad URL: <href>" from a page link, or a network
                // error carrying a remote host string), which would otherwise
                // reach the terminal raw — the same escape-injection risk M-8
                // closes on the page-render path.
                let max_w = if self.terminal_type == TerminalType::Petscii { 30 } else { 50 };
                let safe = crate::aichat::sanitize_for_terminal(&e);
                self.show_error(&crate::webbrowser::truncate_to_width(&safe, max_w)).await?;
                Ok(false)
            }
        }
    }

    pub(in crate::telnet) fn web_reset(&mut self) {
        self.web_lines.clear();
        self.web_scroll = 0;
        self.web_links.clear();
        self.web_forms.clear();
        self.web_history.clear();
        self.web_url = None;
        self.web_title = None;
    }
}
