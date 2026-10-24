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

    /// Content lines per page, out of the 22-row screen: the header (3), the
    /// status row (1), the command rows (1 on 80 columns, 2 on 40) and the
    /// prompt (1).  It was 13 everywhere, with a blank row and the URL on a
    /// row of its own -- Wikipedia's C64 article was 169 screens long.
    /// `test_the_browser_page_fills_the_screen_and_no_more` counts the rows
    /// actually drawn.
    pub(in crate::telnet) const WEB_PAGE_HEIGHT_WIDE: usize = 16;
    pub(in crate::telnet) const WEB_PAGE_HEIGHT_PETSCII: usize = 15;
    /// How many bookmarks the bookmarks screen lists per page.
    pub(in crate::telnet) const WEB_BOOKMARKS_SHOWN: usize = 13;
    /// Rows per page of `web_pick_from_list` (forms, a drop-down's options:
    /// 3 header + 14 + the page row + blank + prompt = 20) and of the form
    /// editor's field list (the same plus its hint row, 21).
    pub(in crate::telnet) const WEB_LIST_SHOWN: usize = 14;
    /// Most lines one Gopher+ `AskL` answer takes.
    pub(in crate::telnet) const WEB_ASK_MAX_LINES: usize = 50;

    pub(in crate::telnet) fn web_page_height(&self) -> usize {
        if self.terminal_type == TerminalType::Petscii {
            Self::WEB_PAGE_HEIGHT_PETSCII
        } else {
            Self::WEB_PAGE_HEIGHT_WIDE
        }
    }

    /// Fetched text as this terminal can show it.  A Commodore draws a byte
    /// above 0x7F as a graphics character and a 7-bit terminal cannot draw
    /// UTF-8 at all, so for those two an accented letter becomes its plain
    /// letter and an emoji goes; an ANSI client -- a modern terminal, as a
    /// rule -- keeps the text as it was.
    pub(in crate::telnet) fn web_text(&self, s: &str) -> String {
        match self.terminal_type {
            TerminalType::Ansi => s.to_string(),
            _ => crate::aichat::fold_to_ascii(s),
        }
    }

    /// Text from the internet, as this terminal can show it: cleaned by
    /// `display_for_terminal` (control bytes, typographic punctuation), then
    /// folded by [`Self::web_text`] (accents to plain letters for PETSCII and
    /// ASCII).  The three surfaces that print remote text -- the browser, AI
    /// Chat and weather -- had the first step and only the browser had the
    /// second, so "Zurich" with an umlaut reached a C64 as a graphics glyph.
    /// Both steps are idempotent, so text cleaned earlier is safe to pass.
    pub(in crate::telnet) fn remote_text(&self, s: &str) -> String {
        self.web_text(&crate::aichat::display_for_terminal(s))
    }

    /// Content width for HTML rendering.
    /// Slightly narrower than the display to leave room for link number suffixes
    /// like `[12]` that are appended after html2text wraps.
    pub(in crate::telnet) fn web_content_width(&self) -> usize {
        if self.terminal_type == TerminalType::Petscii {
            32 // 40 - 1 (the last column wraps) - 2 indent - 5 for "[NNN]"
        } else {
            72 // 80 - 1 - 2 indent - 5 for "[NNN]"
        }
    }

    pub(in crate::telnet) async fn render_web_browser(&mut self) -> Result<(), std::io::Error> {
        // Auto-load homepage on first visit if configured.
        //
        // `web_home_tried` is what makes it *once*: a failed fetch leaves
        // `web_lines` and `web_url` both empty, which is the same state as
        // "never visited", so an unreachable homepage otherwise started a
        // fresh blocking fetch on every redraw and made the user wait out the
        // timeout each time.  Cleared by `web_reset`, so leaving the browser
        // and coming back does retry.
        if !self.web_home_tried && self.web_lines.is_empty() && self.web_url.is_none() {
            let cfg = config::get_config();
            if !cfg.browser_homepage.is_empty() {
                self.web_home_tried = true;
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
                    crate::webbrowser::truncate_to_width(&self.web_text(t), max_w)
                }
                None => "Web Browser".to_string(),
            };
            self.send_line(&format!("  {}", self.yellow(&title_display))).await?;
            self.send_line(&sep).await?;

            let page_h = self.web_page_height();
            let total = self.web_lines.len();
            // Defensive clamp: never let a scroll position index past the
            // current page — guarantees the page_lines slice below can't
            // panic regardless of how web_scroll was set.
            let start = self.web_scroll.min(total.saturating_sub(1));
            let end = (start + page_h).min(total);

            // The width the page was laid out for (see `rewrap_for_screen`);
            // one short of the screen, since a row that fills it wraps on a
            // C64 (see `separator`).
            let content_max = self.web_content_width() + crate::webbrowser::LINK_MARKER_ROOM;
            let page_lines: Vec<String> = self.web_lines[start..end].to_vec();
            for line in &page_lines {
                let safe = crate::webbrowser::truncate_to_width(&self.web_text(line), content_max);
                let colored = self.colorize_link_markers(&safe);
                self.send_line(&format!("  {}", colored)).await?;
            }

            // Status: where we are, then the forms or the address, one row.
            let has_prev = start > 0;
            let has_next = end < total;
            let position = format!("({}-{} of {})", start + 1, end, total);
            let room = content_max.saturating_sub(position.chars().count() + 2);
            let tail = if !self.web_forms.is_empty() {
                let n = self.web_forms.len();
                let hint = if n == 1 { "1 form - F".to_string() } else { format!("{n} forms - F") };
                self.amber(&crate::webbrowser::truncate_to_width(&hint, room))
            } else {
                let url = self.web_text(self.web_url.as_deref().unwrap_or(""));
                self.dim(&crate::webbrowser::truncate_to_width(&url, room))
            };
            self.send_line(&format!("  {}  {}", self.dim(&position), tail)).await?;

            // Commands: one row on an 80-column screen, two on a 40-column one.
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
            if self.web_url.as_deref().is_some_and(|u| u.starts_with("gopher://")) {
                act.push(self.action_prompt("I", "In"));
            }
            act.push(self.action_prompt("K", "Bm"));
            act.push(self.action_prompt("H", "?"));
            if !self.web_history.is_empty() {
                act.push(self.action_prompt("B", "Bk"));
            }
            act.push(self.action_prompt("Q", "X"));
            if is_petscii {
                self.send_line(&format!("  {}", nav.join(" "))).await?;
                self.send_line(&format!("  {}", act.join(" "))).await?;
            } else {
                nav.extend(act);
                self.send_line(&format!("  {}", nav.join(" "))).await?;
            }
        }
        Ok(())
    }

    pub(in crate::telnet) async fn handle_web_browser_command(&mut self, input: &str) -> Result<bool, std::io::Error> {
        // More than one character is not a command — it is somewhere to go.
        // `normalize_url` decides which: a scheme or a dot makes it a URL,
        // anything else becomes a search.  A *single* character stays a
        // command, so `s` is still Find rather than a search for "s".
        //
        // Done before either menu's match so it works on the browser home
        // screen and on a loaded page alike — on the home screen it is the
        // whole point, since it means you can type an address without pressing
        // G first.
        let typed = input.trim();
        if typed.chars().count() > 1 {
            let url = crate::webbrowser::normalize_url(typed);
            self.web_fetch_page(&url, true).await?;
            return Ok(true);
        }
        // The line reader does not fold case the way the one-key reader did,
        // and every arm below is written lower-case.
        let lowered = typed.to_ascii_lowercase();
        let input = lowered.as_str();

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
                    let page_h = self.web_page_height();
                    let total = self.web_lines.len();
                    if self.web_scroll + page_h < total {
                        self.web_scroll += page_h;
                    } else {
                        self.show_error("End of page.").await?;
                    }
                }
                "p" => {
                    if self.web_scroll > 0 {
                        let page_h = self.web_page_height();
                        self.web_scroll = self.web_scroll.saturating_sub(page_h);
                    } else {
                        self.show_error("Top of page.").await?;
                    }
                }
                "t" => {
                    self.web_scroll = 0;
                }
                "e" => {
                    let page_h = self.web_page_height();
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
                "i" => {
                    self.web_gopher_info().await?;
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
        // No blank row first, here or at any prompt below the page: the page
        // fills the screen, and a C64 has three rows under it before its
        // header scrolls away -- the typed command's own row, the prompt, and
        // "Loading...".
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
            // A CSO phone book (type 2) is asked for a name, not words.
            let phone_book = crate::webbrowser::GopherUrl::parse(url.trim_end_matches("?search")).is_ok_and(|g| g.ty == '2');
            let label = if phone_book { "Look up" } else { "Search" };
            self.send(&format!("  {}: ", self.cyan(label))).await?;
            self.flush().await?;
            let query = match self.get_line_input().await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(false),
            };
            crate::webbrowser::build_gopher_search_url(url, &query)
        } else {
            url.to_string()
        };

        let is_gopher = url.starts_with("gopher://");
        // A gopher item that is not a page: a file to save, or a telnet
        // session to hand over to (RFC 1436 types 5/9/g/I..., 8 and T).
        if is_gopher {
            match crate::webbrowser::gopher_action(&url) {
                crate::webbrowser::GopherAction::Page => {}
                crate::webbrowser::GopherAction::Download { name } => {
                    self.web_gopher_download(&url, &name).await?;
                    return Ok(false);
                }
                crate::webbrowser::GopherAction::Telnet { host, port, login, tn3270 } => {
                    self.web_gopher_telnet(&host, port, &login, tn3270).await?;
                    return Ok(false);
                }
            }
        }

        self.send_line(&format!("  {}...", self.dim("Loading"))).await?;
        self.flush().await?;

        let width = self.web_content_width();
        let url_owned = url.clone();

        let result = tokio::task::spawn_blocking(move || {
            if is_gopher {
                crate::webbrowser::fetch_gopher(&url_owned, width)
            } else {
                crate::webbrowser::fetch_and_render(&url_owned, width)
            }
        })
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?;

        let shown = self.web_apply_result(result, push_history).await?;
        // A Gopher+ form opens on its questions.
        if shown && self.web_gopher_form(0) {
            self.web_fill_ask_files().await;
            if !self.web_forms[0].fields.is_empty() {
                self.web_edit_form(0).await?;
            }
        }
        Ok(shown)
    }

    /// RFC 1436 file items (and Gopher+ views that are files): asked, then
    /// saved in the transfer folder under the selector's last name, made
    /// safe and numbered rather than overwriting, for XMODEM, ZMODEM or
    /// Kermit to send on.  At most what those can send (`MAX_FILE_SIZE`).
    pub(in crate::telnet) async fn web_gopher_download(&mut self, url: &str, name: &str) -> Result<(), std::io::Error> {
        let name = Self::safe_upload_name(name).unwrap_or_else(|| "gopher.bin".to_string());
        // The menu chose the host: shown as internet text is.
        let host = crate::webbrowser::GopherUrl::parse(url).map(|g| g.host).unwrap_or_default();
        let host = crate::aichat::display_for_terminal(&host);
        // A safe name is up to 64 characters; a C64 has 40.
        let room = if self.terminal_type == TerminalType::Petscii { 24 } else { 60 };
        let shown_name = crate::webbrowser::truncate_to_width(&self.web_text(&name), room);
        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;
        self.send_line(&format!("  {}", self.yellow("GOPHER DOWNLOAD"))).await?;
        self.send_line(&sep).await?;
        self.send_line("").await?;
        self.send_line(&format!("  File: {}", self.amber(&shown_name))).await?;
        let from = crate::webbrowser::truncate_to_width(&self.web_text(&host), 30);
        self.send_line(&format!("  From: {}", from)).await?;
        self.send_line("").await?;
        self.send_line("  It is saved in the transfer folder,").await?;
        self.send_line("  to send on from File Transfer.").await?;
        self.send_line("").await?;
        self.send("  Download it? (Y/N) ").await?;
        self.flush().await?;
        if !self.cpm_confirm().await? {
            return Ok(());
        }
        self.send_line(&format!("  {}...", self.dim("Downloading"))).await?;
        self.flush().await?;
        let url_owned = url.to_string();
        let max = crate::tnio::MAX_FILE_SIZE as usize;
        let fetched = tokio::task::spawn_blocking(move || crate::webbrowser::fetch_gopher_file(&url_owned, max))
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let data = match fetched {
            Ok(d) => d,
            Err(e) => {
                let safe = self.web_text(&crate::aichat::sanitize_for_terminal(&e));
                self.show_error(&safe).await?;
                return Ok(());
            }
        };
        self.ensure_transfer_dir().await?;
        if !self.verify_transfer_path() {
            self.show_error("The transfer folder is not available.").await?;
            return Ok(());
        }
        let dir = self.transfer_path();
        let size = data.len() as u64;
        let saved = tokio::task::spawn_blocking(move || {
            Self::save_received_file_collision_safe(&dir, &name, &data, None, false)
        })
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?;
        match saved {
            Ok(as_name) => {
                let room = if self.terminal_type == TerminalType::Petscii { 16 } else { 52 };
                self.send_line(&format!(
                    "  {} {} ({})",
                    self.green("Saved as"),
                    crate::webbrowser::truncate_to_width(&self.web_text(&as_name), room),
                    Self::format_file_size(size)
                ))
                .await?;
                self.send_line("").await?;
                self.send("  Press any key to continue.").await?;
                self.flush().await?;
                self.wait_for_key().await?;
            }
            Err(_) => self.show_error("Could not save the file.").await?,
        }
        Ok(())
    }

    /// RFC 1436 telnet (`8`) and tn3270 (`T`) items: the session is handed
    /// to the telnet gateway, after a screen that says where to and the name
    /// to log in as.  The address is checked as a gopher fetch's is: a menu
    /// chose it.  A tn3270 host wants an IBM 3270 terminal, which the
    /// gateway cannot be; it is offered over plain telnet, and said so.
    pub(in crate::telnet) async fn web_gopher_telnet(&mut self, host: &str, port: u16, login: &str, tn3270: bool) -> Result<(), std::io::Error> {
        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;
        let title = if tn3270 { "TN3270 SESSION" } else { "TELNET SESSION" };
        self.send_line(&format!("  {}", self.yellow(title))).await?;
        self.send_line(&sep).await?;
        self.send_line("").await?;
        // Host and login are the menu's: shown as internet text is.
        let shown = crate::aichat::display_for_terminal(&format!("{host}:{port}"));
        let at = crate::webbrowser::truncate_to_width(&self.web_text(&shown), 30);
        self.send_line(&format!("  Host: {}", self.amber(&at))).await?;
        if !login.trim().is_empty() {
            let login = crate::aichat::display_for_terminal(login);
            let name = crate::webbrowser::truncate_to_width(&self.web_text(&login), 26);
            self.send_line(&format!("  Log in as: {}", self.amber(&name))).await?;
        }
        if tn3270 {
            self.send_line("").await?;
            self.send_line("  This host expects an IBM 3270").await?;
            self.send_line("  terminal: plain telnet may not").await?;
            self.send_line("  work.").await?;
        }
        self.send_line("").await?;
        self.send("  Connect? (Y/N) ").await?;
        self.flush().await?;
        if !self.cpm_confirm().await? {
            return Ok(());
        }
        // Every address the name has is checked, and the one dialled is the
        // first of them: a connect by name would try the others too, and
        // resolve again -- a LAN address behind a public one, or rebound.
        let (h, p) = (host.to_string(), port);
        let checked = tokio::task::spawn_blocking(move || {
            use std::net::ToSocketAddrs;
            let addrs: Vec<std::net::SocketAddr> =
                (h.as_str(), p).to_socket_addrs().map_err(|e| format!("DNS error: {e}"))?.collect();
            crate::webbrowser::gopher_dial_addr(&addrs, &h)
        })
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?;
        let addr = match checked {
            Ok(a) => a,
            Err(e) => {
                let safe = self.web_text(&crate::aichat::sanitize_for_terminal(&e));
                self.show_error(&safe).await?;
                return Ok(());
            }
        };
        self.gateway_telnet_dial(host, port, Some(addr)).await
    }

    /// Whether form `idx` is a Gopher+ `+ASK` form: made from one (its
    /// method), on a gopher page, sent to a gopher item.  All three, because
    /// such a form can send a file from the transfer folder -- an HTML page
    /// must never be able to make one.
    pub(in crate::telnet) fn web_gopher_form(&self, idx: usize) -> bool {
        self.web_forms.get(idx).is_some_and(|f| self.web_is_gopher_form(f))
    }

    /// `web_gopher_form`'s rule, for a form already in hand.
    fn web_is_gopher_form(&self, f: &crate::webbrowser::WebForm) -> bool {
        self.web_url.as_deref().is_some_and(|u| u.starts_with("gopher://"))
            && f.method == crate::webbrowser::ASK_METHOD
            && f.action.starts_with("gopher://")
    }

    /// A Gopher+ form's `ChooseF` question picks a file from the transfer
    /// folder to send: its choices are that folder's files.
    pub(in crate::telnet) async fn web_fill_ask_files(&mut self) {
        let wants = (0..self.web_forms.len()).any(|i| {
            self.web_gopher_form(i)
                && self.web_forms[i].fields.iter().any(|x| {
                    matches!(x, crate::webbrowser::FormField::Select { name, .. } if name == crate::webbrowser::ASK_SEND_FILE)
                })
        });
        if !wants || self.ensure_transfer_dir().await.is_err() || !self.verify_transfer_path() {
            return;
        }
        let files: Vec<(String, String)> = Self::list_transfer_entries_in(&self.transfer_path())
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|(_, size, dir)| !dir && *size <= crate::tnio::MAX_FILE_SIZE)
            .map(|(name, _, _)| (name.clone(), name))
            .collect();
        for i in 0..self.web_forms.len() {
            if !self.web_gopher_form(i) {
                continue;
            }
            for field in self.web_forms[i].fields.iter_mut() {
                // Nothing is chosen until the user picks (`parse_ask`): no
                // file is sent that was not picked.
                if let crate::webbrowser::FormField::Select { name, options, .. } = field
                    && name == crate::webbrowser::ASK_SEND_FILE
                {
                    *options = files.clone();
                }
            }
        }
    }

    /// Send a Gopher+ form: its `ChooseF` file read from the transfer
    /// folder, and the reply also saved there when an `AskF` names a file.
    pub(in crate::telnet) async fn web_submit_gopher_ask(&mut self, form_idx: usize) -> Result<(), std::io::Error> {
        use crate::webbrowser::FormField;
        let form = self.web_forms[form_idx].clone();
        let chosen = form.fields.iter().find_map(|f| match f {
            FormField::Select { name, options, selected, .. } if name == crate::webbrowser::ASK_SEND_FILE => {
                options.get(*selected).map(|(v, _)| v.clone())
            }
            _ => None,
        });
        let file = match chosen {
            Some(name) => {
                // A name from the folder's own listing, checked again: the
                // folder may have changed since.
                if Self::validate_filename(&name).is_err() || !self.verify_transfer_path() {
                    self.show_error("That file cannot be sent.").await?;
                    return Ok(());
                }
                let path = self.transfer_path().join(&name);
                if Self::is_symlink(&path).await {
                    self.show_error("That file cannot be sent.").await?;
                    return Ok(());
                }
                match tokio::fs::read(&path).await {
                    Ok(d) if d.len() as u64 <= crate::tnio::MAX_FILE_SIZE => Some(d),
                    _ => {
                        self.show_error("That file cannot be read.").await?;
                        return Ok(());
                    }
                }
            }
            None => None,
        };
        let save_as = form.fields.iter().find_map(|f| match f {
            FormField::Text { input_type, value, .. } if input_type == crate::webbrowser::ASK_SAVE_AS && !value.trim().is_empty() => {
                Self::safe_upload_name(value.trim())
            }
            _ => None,
        });
        self.send_line("").await?;
        self.send_line(&format!("  {}...", self.dim("Sending"))).await?;
        self.flush().await?;
        let width = self.web_content_width();
        let result = tokio::task::spawn_blocking(move || crate::webbrowser::submit_gopher_ask(&form, file.as_deref(), width))
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let page = match result {
            Ok((mut page, raw)) => {
                if let Some(name) = save_as {
                    let note = match self.web_save_reply(&name, raw).await {
                        Some(as_name) => format!("Reply saved as {as_name} in the transfer folder."),
                        None => "The reply could not be saved.".to_string(),
                    };
                    let note = crate::aichat::sanitize_for_terminal(&note);
                    let mut lines = crate::aichat::wrap_line(&note, width);
                    lines.push(String::new());
                    lines.append(&mut page.lines);
                    page.lines = lines;
                }
                Ok(page)
            }
            Err(e) => Err(e),
        };
        self.web_apply_result(page, true).await?;
        Ok(())
    }

    /// Save a Gopher+ reply for an `AskF` question; the name it got.
    async fn web_save_reply(&mut self, name: &str, data: Vec<u8>) -> Option<String> {
        if data.is_empty() || self.ensure_transfer_dir().await.is_err() || !self.verify_transfer_path() {
            return None;
        }
        let dir = self.transfer_path();
        let name = name.to_string();
        tokio::task::spawn_blocking(move || Self::save_received_file_collision_safe(&dir, &name, &data, None, false).ok())
            .await
            .ok()
            .flatten()
    }

    /// `I`: a gopher item's Gopher+ attributes -- a link's, or with no
    /// number this page's (a menu's `$`, every item in it at once).
    pub(in crate::telnet) async fn web_gopher_info(&mut self) -> Result<(), std::io::Error> {
        let Some(page) = self.web_url.clone().filter(|u| u.starts_with("gopher://")) else {
            self.show_error("Item info is for Gopher pages.").await?;
            return Ok(());
        };
        self.send(&format!("  {} ", self.cyan("Info on link # (Enter=page):"))).await?;
        self.flush().await?;
        let input = match self.get_line_input().await? {
            Some(s) => s.trim().to_string(),
            None => return Ok(()),
        };
        self.drain_input().await;
        let target = if input.is_empty() {
            match crate::webbrowser::GopherUrl::parse(&page) {
                // A phone book's results: the same rule as a link to one.
                Ok(g) if matches!(g.ty, '8' | 'T' | '2') => {
                    self.show_error("That item is not served by Gopher.").await?;
                    return Ok(());
                }
                Ok(g) => {
                    let all = if g.ty == '1' { "$" } else { "!" };
                    g.with_plus(all).to_url()
                }
                Err(e) => {
                    self.show_error(&e).await?;
                    return Ok(());
                }
            }
        } else {
            let link = match input.parse::<usize>().ok().and_then(|n| n.checked_sub(1)).and_then(|i| self.web_links.get(i)) {
                Some(l) => l.clone(),
                None => {
                    self.show_error("No such link.").await?;
                    return Ok(());
                }
            };
            let link = crate::webbrowser::resolve_url(&page, &link);
            match crate::webbrowser::GopherUrl::parse(link.trim_end_matches("?search")) {
                // A telnet session or a phone book is not served by a gopher
                // server, so it has no Gopher+ information to ask for.
                Ok(g) if matches!(g.ty, '8' | 'T' | '2') => {
                    self.show_error("That item is not served by Gopher.").await?;
                    return Ok(());
                }
                Ok(g) if link.starts_with("gopher://") => g.with_plus("!").to_url(),
                _ => {
                    self.show_error("That link is not a Gopher item.").await?;
                    return Ok(());
                }
            }
        };
        self.web_fetch_page(&target, true).await?;
        Ok(())
    }

    /// Through the pager the other help screens use: the page-view help
    /// ran to 24 rows on a C64, two past the screen, and a sum of numbers in
    /// a test said it fit.
    pub(in crate::telnet) async fn web_show_help(&mut self, page_view: bool) -> Result<(), std::io::Error> {
        let lines = if page_view {
            Self::browser_page_help_lines(self.terminal_type == TerminalType::Petscii)
        } else {
            Self::browser_menu_help_lines()
        };
        self.show_help_page("BROWSER HELP", lines).await
    }

    pub(in crate::telnet) async fn web_save_bookmark(&mut self) -> Result<(), std::io::Error> {
        if let Some(url) = &self.web_url {
            let title = self.web_title.as_deref().unwrap_or("Untitled");
            if crate::webbrowser::add_bookmark(url, title) {
                // Two rows, no blank first: this is below a full page (see
                // `web_prompt_url`).
                self.send_line(&format!("  {}", self.green("Bookmark saved."))).await?;
                self.send("  Press any key to continue.").await?;
                self.flush().await?;
                self.wait_for_key().await?;
            } else {
                // Two rows, like the success above.
                self.send_line(&format!("  {}", self.red("Already bookmarked (or full)."))).await?;
                self.send("  Press any key to continue.").await?;
                self.flush().await?;
                self.wait_for_key().await?;
            }
        } else {
            self.show_error("No page to bookmark.").await?;
        }
        Ok(())
    }

    /// The bookmark list, a page of `WEB_BOOKMARKS_SHOWN` at a time.  Paged
    /// because a list cut at the screen's height left bookmarks 14 to 100
    /// (`MAX_BOOKMARKS`) saved but unreachable: neither openable nor
    /// deletable, since only the numbers drawn were accepted.  Numbers are the
    /// bookmark's place in the whole list, so any of them works from any page.
    pub(in crate::telnet) async fn web_show_bookmarks(&mut self) -> Result<(), std::io::Error> {
        let mut page = 0usize;
        loop {
            let bookmarks = crate::webbrowser::load_bookmarks();
            if bookmarks.is_empty() {
                self.show_error("No bookmarks saved.").await?;
                return Ok(());
            }
            let total = bookmarks.len();
            let pages = total.div_ceil(Self::WEB_BOOKMARKS_SHOWN);
            page = page.min(pages - 1);
            let first = page * Self::WEB_BOOKMARKS_SHOWN;
            let last = (first + Self::WEB_BOOKMARKS_SHOWN).min(total);

            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            self.send_line(&format!("  {}", self.yellow("BOOKMARKS"))).await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;

            let max_title = if self.terminal_type == TerminalType::Petscii { 30 } else { 60 };
            for (i, bm) in bookmarks.iter().enumerate().take(last).skip(first) {
                let title = crate::webbrowser::truncate_to_width(&self.web_text(&bm.title), max_title);
                self.send_line(&format!("  {:>2}. {}", i + 1, title)).await?;
            }
            if pages > 1 {
                self.send_line(&format!(
                    "  Page {}/{} {} {}",
                    page + 1,
                    pages,
                    self.action_prompt("N", "Next"),
                    self.action_prompt("P", "Prev"),
                ))
                .await?;
            }

            self.send_line("").await?;
            self.send_line(&format!("  {} {} {}",
                self.dim("#=Open"),
                self.action_prompt("D", "Delete"),
                self.action_prompt("H", "Help"),
            )).await?;
            self.send(&format!("  {}: ", self.cyan("#/D"))).await?;
            self.flush().await?;

            // Folded, as `handle_web_browser_command` folds: the line reader
            // does not, and the prompt above shows the letters in capitals.
            let input = match self.get_line_input().await? {
                Some(s) if !s.is_empty() => s.trim().to_ascii_lowercase(),
                _ => return Ok(()),
            };

            if input == "n" && pages > 1 {
                page = (page + 1) % pages;
                continue;
            } else if input == "p" && pages > 1 {
                page = (page + pages - 1) % pages;
                continue;
            } else if input == "h" {
                self.show_help_page("BOOKMARKS HELP", Self::bookmarks_help_lines())
                    .await?;
            } else if input == "d" {
                self.send(&format!("  {} (1-{}): ", self.cyan("Delete #"), total)).await?;
                self.flush().await?;
                let del_input = match self.get_line_input().await? {
                    Some(s) if !s.is_empty() => s,
                    _ => return Ok(()),
                };
                match del_input.trim().parse::<usize>() {
                    // A refusal -- the number is past the end of the list as
                    // `remove_bookmark` re-reads it (another session deleted
                    // some since this was drawn), or the save failed -- is
                    // said rather than reported as "Deleted.".
                    Ok(num) if (1..=total).contains(&num) => {
                        if crate::webbrowser::remove_bookmark(num - 1) {
                            self.send_line(&format!("  {}", self.green("Deleted."))).await?;
                            self.send_line("").await?;
                            self.send("  Press any key to continue.").await?;
                            self.flush().await?;
                            self.wait_for_key().await?;
                        } else {
                            self.show_error("Could not delete it.").await?;
                        }
                    }
                    Ok(_) => self.show_error("Invalid number.").await?,
                    Err(_) => self.show_error("Enter a number.").await?,
                }
            } else if let Ok(num) = input.parse::<usize>() {
                if (1..=total).contains(&num) {
                    let url = bookmarks[num - 1].url.clone();
                    self.web_fetch_page(&url, true).await?;
                } else {
                    self.show_error("Invalid number.").await?;
                }
            }
            return Ok(());
        }
    }

    pub(in crate::telnet) async fn web_search_in_page(&mut self) -> Result<(), std::io::Error> {
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
            // The text as shown: a C64 user who sees "Bahia" searches for it.
            if self.web_text(&self.web_lines[idx]).to_ascii_lowercase().contains(&query) {
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

        let labels: Vec<String> =
            self.web_forms.iter().map(|form| self.web_text(&form.label)).collect();
        if let Some(i) = self.web_pick_from_list("FORMS", "Form #", &labels, None).await? {
            self.web_edit_form(i).await?;
        }
        Ok(())
    }

    /// Pick one of `items` on a screen of its own, `WEB_LIST_SHOWN` a page
    /// with N/P between pages.  A page can carry any number of forms and a
    /// drop-down any number of options (a country list runs to 200); printed
    /// in one go they scrolled a C64's screen away, leaving the user to pick
    /// a number they could not see.  `current` is marked `>`.  Returns the
    /// index picked, or `None` for a blank answer or a bad number (said).
    pub(in crate::telnet) async fn web_pick_from_list(
        &mut self,
        title: &str,
        prompt: &str,
        items: &[String],
        current: Option<usize>,
    ) -> Result<Option<usize>, std::io::Error> {
        let total = items.len();
        if total == 0 {
            return Ok(None);
        }
        let pages = total.div_ceil(Self::WEB_LIST_SHOWN);
        // Open on the page holding the current choice.
        // Past the end (a drop-down with nothing chosen) is page one.
        let mut page = current.filter(|&c| c < total).map_or(0, |c| c / Self::WEB_LIST_SHOWN);
        let max_item = if self.terminal_type == TerminalType::Petscii { 30 } else { 60 };
        loop {
            let first = page * Self::WEB_LIST_SHOWN;
            let last = (first + Self::WEB_LIST_SHOWN).min(total);
            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            let heading = crate::webbrowser::truncate_to_width(title, 34);
            self.send_line(&format!("  {}", self.yellow(&heading))).await?;
            self.send_line(&sep).await?;
            for (i, item) in items.iter().enumerate().take(last).skip(first) {
                let marker = if current == Some(i) { ">" } else { " " };
                self.send_line(&format!(
                    "  {}{}.{}",
                    marker,
                    i + 1,
                    crate::webbrowser::truncate_to_width(item, max_item),
                ))
                .await?;
            }
            if pages > 1 {
                self.send_line(&format!(
                    "  Page {}/{} {} {}",
                    page + 1,
                    pages,
                    self.action_prompt("N", "Next"),
                    self.action_prompt("P", "Prev"),
                ))
                .await?;
            }
            self.send_line("").await?;
            self.send(&format!("  {} (1-{}): ", self.cyan(prompt), total)).await?;
            self.flush().await?;
            let input = match self.get_line_input().await? {
                Some(s) if !s.trim().is_empty() => s.trim().to_ascii_lowercase(),
                _ => return Ok(None),
            };
            if pages > 1 && input == "n" {
                page = (page + 1) % pages;
                continue;
            }
            if pages > 1 && input == "p" {
                page = (page + pages - 1) % pages;
                continue;
            }
            return match input.parse::<usize>() {
                Ok(n) if (1..=total).contains(&n) => Ok(Some(n - 1)),
                Ok(_) => {
                    self.show_error("Invalid number.").await?;
                    Ok(None)
                }
                Err(_) => {
                    self.show_error("Enter a number.").await?;
                    Ok(None)
                }
            };
        }
    }

    pub(in crate::telnet) async fn web_edit_form(&mut self, form_idx: usize) -> Result<(), std::io::Error> {
        let mut form = self.web_forms[form_idx].clone();

        // If the form has no visible fields (only hidden), submit immediately
        let has_visible = form.fields.iter().any(|f| !matches!(f, crate::webbrowser::FormField::Hidden { .. }));
        if !has_visible {
            self.web_forms[form_idx] = form;
            return self.web_submit_form(form_idx).await;
        }

        let mut field_page = 0usize;
        loop {
            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            let title = crate::webbrowser::truncate_to_width(&self.web_text(&form.label), 34);
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
                        let secret = matches!(field, crate::webbrowser::FormField::Text { input_type, .. } if input_type == "password");
                        // Sanitize the value for display only — the stored
                        // `value` is submitted verbatim, so we must not strip
                        // control bytes from it (M-8; labels/option-text are
                        // already sanitized in WebPage::sanitize).  A password
                        // is shown as stars: it was typed unseen.
                        let display_val = if value.is_empty() {
                            "(empty)".to_string()
                        } else if secret {
                            "*".repeat(value.chars().count().min(12))
                        } else {
                            crate::aichat::sanitize_for_terminal(value)
                        };
                        Some(format!("  {}.{}: {}",
                            field_num,
                            crate::webbrowser::truncate_to_width(&self.web_text(label), max_label),
                            crate::webbrowser::truncate_to_width(&self.web_text(&display_val), max_val),
                        ))
                    }
                    crate::webbrowser::FormField::Select { label, options, selected, .. } => {
                        field_num += 1;
                        // Past the end: nothing chosen (a disabled placeholder).
                        let chosen = options.get(*selected).map(|(_, t)| t.as_str()).unwrap_or("(none)");
                        Some(format!("  {}.{}: {}",
                            field_num,
                            crate::webbrowser::truncate_to_width(&self.web_text(label), max_label),
                            crate::webbrowser::truncate_to_width(&self.web_text(chosen), max_val),
                        ))
                    }
                    crate::webbrowser::FormField::Checkbox { label, checked, .. } => {
                        field_num += 1;
                        let mark = if *checked { "[X]" } else { "[ ]" };
                        Some(format!("  {}.{}: {}",
                            field_num,
                            crate::webbrowser::truncate_to_width(&self.web_text(label), max_label),
                            mark,
                        ))
                    }
                    crate::webbrowser::FormField::Radio { label, checked, .. } => {
                        field_num += 1;
                        let mark = if *checked { "(X)" } else { "( )" };
                        Some(format!("  {}.{}: {}",
                            field_num,
                            crate::webbrowser::truncate_to_width(&self.web_text(label), max_label),
                            mark,
                        ))
                    }
                }
            }).collect();

            // A page of fields at a time: a long form printed whole scrolled
            // its own title and the first fields off a C64.  Field numbers
            // stay the field's place in the whole form.
            let pages = display_lines.len().div_ceil(Self::WEB_LIST_SHOWN).max(1);
            field_page = field_page.min(pages - 1);
            for line in display_lines.iter().skip(field_page * Self::WEB_LIST_SHOWN).take(Self::WEB_LIST_SHOWN) {
                self.send_line(line).await?;
            }
            if pages > 1 {
                self.send_line(&format!(
                    "  Page {}/{} {} {}",
                    field_page + 1,
                    pages,
                    self.action_prompt("N", "Next"),
                    self.action_prompt("P", "Prev"),
                ))
                .await?;
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

            // Folded -- see the bookmarks screen.
            let input = match self.get_line_input().await? {
                Some(s) if !s.is_empty() => s.trim().to_ascii_lowercase(),
                _ => return Ok(()),
            };

            match input.as_str() {
                // Only with more than one page, wrapping as the bookmarks and
                // the picker do; on one page they get the error below.
                "n" if pages > 1 => field_page = (field_page + 1) % pages,
                "p" if pages > 1 => field_page = (field_page + pages - 1) % pages,
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
                        let hint = if pages > 1 {
                            "Enter S, Q, H, N, P or field #."
                        } else {
                            "Enter S, Q, H, or a field #."
                        };
                        self.show_error(hint).await?;
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

        // A Gopher+ `AskL` question takes several lines, a blank one ending
        // them.
        if let FormField::TextArea { name, .. } = &form.fields[idx]
            && name.ends_with(":askl")
            && self.web_is_gopher_form(form)
        {
            self.send_line("").await?;
            self.send_line(&format!("  {}", self.cyan(&self.web_text(&label_str)))).await?;
            self.send_line(&format!("  {}", self.dim("A blank line ends it."))).await?;
            let mut rows: Vec<String> = Vec::new();
            while rows.len() < Self::WEB_ASK_MAX_LINES {
                self.send("  > ").await?;
                self.flush().await?;
                match self.get_line_input().await? {
                    Some(line) if !line.is_empty() => rows.push(line),
                    Some(_) => break,
                    // ESC: the answer is left as it was.
                    None => return Ok(()),
                }
            }
            if let FormField::TextArea { value, .. } = &mut form.fields[idx] {
                *value = rows.join("\n");
            }
            return Ok(());
        }
        if is_text {
            self.send_line("").await?;
            self.send(&format!("  {}: ", self.cyan(&self.web_text(&label_str)))).await?;
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
            let (title, items, current) = if let FormField::Select { label, options, selected, .. } = &form.fields[idx] {
                (
                    self.web_text(label),
                    options.iter().map(|(_, display)| self.web_text(display)).collect::<Vec<_>>(),
                    *selected,
                )
            } else {
                (String::new(), Vec::new(), 0)
            };
            // A drop-down with no label still gets a heading.
            let title = if title.trim().is_empty() { "OPTIONS".to_string() } else { title };
            if let Some(n) = self.web_pick_from_list(&title, "Pick", &items, Some(current)).await?
                && n < opt_count
                && let FormField::Select { selected, .. } = &mut form.fields[idx]
            {
                *selected = n;
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
        if self.web_gopher_form(form_idx) {
            return self.web_submit_gopher_ask(form_idx).await;
        }
        // A page that came over plain HTTP because HTTPS failed resolves its
        // relative form actions to `http://`, so the form would go out in the
        // clear with no HTTPS attempt -- on the very site whose HTTPS was just
        // broken, perhaps on purpose.  Not refused: an HTTP-only site's
        // search box is what the fallback is for.  Asked, on an explicit Y,
        // for any page of that site this visit (`web_fallback_hosts`).
        let action = {
            let form = &self.web_forms[form_idx];
            let base = self.web_url.clone().unwrap_or_default();
            if form.action.is_empty() { base } else { crate::webbrowser::resolve_url(&base, &form.action) }
        };
        let fell_back = |url: &str| {
            crate::webbrowser::http_site(url).is_some_and(|site| self.web_fallback_hosts.contains(&site))
        };
        let page = self.web_url.clone().unwrap_or_default();
        if action.starts_with("http://") && (fell_back(&action) || fell_back(&page)) {
            // A screen of its own: the form screen below is full.
            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            self.send_line(&format!("  {}", self.yellow("PLAIN HTTP"))).await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;
            self.send_line(&format!("  {}", self.red("HTTPS failed for this page."))).await?;
            self.send_line("").await?;
            self.send_line("  This form would be sent over plain").await?;
            self.send_line("  HTTP, readable on the way.").await?;
            self.send_line("").await?;
            self.send("  Send it anyway? (Y/N) ").await?;
            self.flush().await?;
            if !self.cpm_confirm().await? {
                return Ok(());
            }
        }
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
                for site in page.fallback_sites {
                    if !self.web_fallback_hosts.contains(&site) {
                        self.web_fallback_hosts.push(site);
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
                //
                // Wrapped, not cut: the end of a message is where it says
                // why (`...certificate expir...` lost the word that mattered).
                let width = if self.terminal_type == TerminalType::Petscii { 36 } else { 76 };
                let safe = self.web_text(&crate::aichat::sanitize_for_terminal(&e));
                for line in crate::aichat::wrap_line(&safe, width) {
                    self.send_line(&format!("  {}", self.red(&line))).await?;
                }
                self.send_line("").await?;
                self.send("  Press any key to continue.").await?;
                self.flush().await?;
                self.wait_for_key().await?;
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
        self.web_fallback_hosts.clear();
        // Leaving the browser ends the visit, so the homepage is auto-loaded
        // again next time -- the once-only guard is per visit, not per session.
        self.web_home_tried = false;
    }
}
