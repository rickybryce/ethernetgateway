//! The Add-Ons list page (Second Menu `A`) and the bridge that runs a
//! terminal add-on for the caller.  `crate::addons` holds what every
//! surface shares: the manifest, discovery and the containment check.
//!
//! **The bridge is the gateways' bridge with a program at the far end.**
//! The same double-ESC to leave (`EscHold`, `is_gateway_leave_key`), the same
//! key translation for a Commodore (`gateway_input_for_remote`) and the same
//! output filter (`filter_gateway_output`), so an add-on behaves like the SSH
//! and Telnet gateways a caller already knows.  What differs is the PETSCII
//! choice, which is the add-on's own (`petscii` in its manifest) rather than
//! the server's, and the line handling a pipe needs (`input`; see
//! `crate::addons::Input`).

use super::*;
use crate::addons::{AddOn, Input, Manifest, Petscii};
use super::gateway::{
    filter_gateway_output, gateway_input_for_remote, gateway_window, is_gateway_leave_key,
    normalize_gateway_input,
    read_gateway_event, EscHold, GatewayInboundEvent, GatewayOutState, GW_FILTER_FLUSH,
};

/// The most typed-ahead input held for an add-on that is not keeping up;
/// beyond it keys are dropped, since a program this far behind is stalled.
const ADDON_BACKLOG_MAX: usize = 64 * 1024;

/// How a run of an add-on ended, for the line that says so.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ended {
    /// The caller pressed ESC twice.
    Left,
    /// The add-on closed its output (it exited).
    ByItself,
    /// No traffic either way for the idle limit (seconds).
    Idle(u64),
    /// Its `max_minutes` ran out.
    TimeUp(u32),
    /// The caller's connection went.
    Disconnected,
}

/// Newlines for a terminal: a lone `\n` (what a program on a pipe writes)
/// becomes `\r\n`, so each line starts at the left margin.  `last_cr`
/// carries a `\r` that ended the previous read.
fn crlf(data: &[u8], last_cr: &mut bool, out: &mut Vec<u8>) {
    for &b in data {
        if b == b'\n' && !*last_cr {
            out.push(b'\r');
        }
        out.push(b);
        *last_cr = b == b'\r';
    }
}

impl TelnetSession {
    /// Add-ons listed per page: four rows of title, then these, then the page
    /// line, a blank, the key line and the prompt -- 20 of the 22.
    pub(in crate::telnet) const ADDONS_SHOWN: usize = 12;

    /// The geometry an add-on is told (`EG_COLUMNS`/`EG_ROWS`) and its
    /// `min_columns` is checked against: the same answer the SSH and Telnet
    /// gateways give a remote host, operator override included.
    pub(in crate::telnet) fn addon_window(&self) -> (u16, u16) {
        let cfg = config::get_config();
        gateway_window(
            self.terminal_type,
            (self.window_width, self.window_height),
            cfg.gateway_term_width,
            cfg.gateway_term_height,
        )
    }

    /// Why this add-on cannot run for this caller, or `None` when it can.
    pub(in crate::telnet) fn addon_refusal(&self, m: &Manifest, cols: u16) -> Option<String> {
        if !m.mode.has_terminal() {
            return Some("It opens a window; not offered here yet.".into());
        }
        let (ok, kind) = match self.terminal_type {
            TerminalType::Petscii => (m.terminals.petscii, "PETSCII"),
            TerminalType::Ascii => (m.terminals.ascii, "ASCII"),
            TerminalType::Ansi => (m.terminals.ansi, "ANSI"),
        };
        if !ok {
            return Some(format!("Not for {kind} terminals."));
        }
        if m.min_columns > cols {
            return Some(format!("Needs {} columns; this has {}.", m.min_columns, cols));
        }
        None
    }

    /// Every row of one page of the list, title to key line (the prompt is
    /// drawn after).  A function of its inputs so the screen-fit tests
    /// measure the real page.
    pub(in crate::telnet) fn addons_list_rows(&self, addons: &[AddOn], page: usize, cols: u16) -> Vec<String> {
        let sep = self.separator();
        let mut rows = vec![sep.clone(), format!("  {}", self.yellow("ADD-ONS")), sep, String::new()];
        let total = addons.len();
        let pages = total.div_ceil(Self::ADDONS_SHOWN).max(1);
        let page = page.min(pages - 1);
        let first = page * Self::ADDONS_SHOWN;
        let width = if self.terminal_type == TerminalType::Petscii { PETSCII_WIDTH } else { 80 } - 1;
        for (i, a) in addons.iter().enumerate().skip(first).take(Self::ADDONS_SHOWN) {
            let m = &a.manifest;
            let mark = match m.mode {
                crate::addons::Mode::Terminal => "",
                crate::addons::Mode::Gui => " [GUI]",
                crate::addons::Mode::Both => " [TERM+GUI]",
            };
            let prefix = format!("  {:>2}. ", i + 1);
            let room = width.saturating_sub(prefix.len() + mark.len());
            let name = crate::webbrowser::truncate_to_width(&self.remote_text(&m.name), room);
            let line = format!("{prefix}{name}{mark}");
            rows.push(if self.addon_refusal(m, cols).is_some() { self.dim(&line) } else { line });
        }
        if pages > 1 {
            rows.push(format!(
                "  Page {}/{} {} {}",
                page + 1,
                pages,
                self.action_prompt("N", "Next"),
                self.action_prompt("P", "Prev"),
            ));
        }
        rows.push(String::new());
        rows.push(format!(
            "  {} {} {}",
            self.dim("#=Start"),
            self.action_prompt("Q", "Back"),
            self.action_prompt("H", "Help"),
        ));
        rows
    }

    /// The list page: pick an add-on by number.
    pub(in crate::telnet) async fn addons_page(&mut self) -> Result<(), std::io::Error> {
        let mut page = 0usize;
        loop {
            // Found again on every draw (see `crate::addons`).
            let addons = crate::addons::discover();
            if addons.is_empty() {
                self.show_error("No add-ons are installed.").await?;
                return Ok(());
            }
            let pages = addons.len().div_ceil(Self::ADDONS_SHOWN);
            page = page.min(pages - 1);
            let (cols, _) = self.addon_window();
            self.clear_screen().await?;
            for row in self.addons_list_rows(&addons, page, cols) {
                self.send_line(&row).await?;
            }
            let prompt = format!("{}> ", self.cyan("gateway/addons"));
            self.send(&prompt).await?;
            self.flush().await?;
            let input = match self.get_line_input().await? {
                Some(s) if !s.trim().is_empty() => s.trim().to_ascii_lowercase(),
                _ => return Ok(()),
            };
            match input.as_str() {
                "q" => return Ok(()),
                "h" => self.show_help_page("ADD-ONS HELP", Self::addons_help_lines()).await?,
                "n" if pages > 1 => page = (page + 1) % pages,
                "p" if pages > 1 => page = (page + pages - 1) % pages,
                _ => match input.parse::<usize>() {
                    Ok(n) if (1..=addons.len()).contains(&n) => {
                        let a = addons[n - 1].clone();
                        match self.addon_refusal(&a.manifest, cols) {
                            Some(why) => self.show_error(&why).await?,
                            None => self.run_terminal_addon(&a).await?,
                        }
                    }
                    Ok(_) => self.show_error("Invalid number.").await?,
                    Err(_) => self.show_error("Enter a number, or Q.").await?,
                },
            }
        }
    }

    /// The `EG_*` values an add-on is told about this caller, and `TERM`.
    pub(in crate::telnet) fn addon_env(&self, a: &AddOn, petscii_native: bool) -> Vec<(String, String)> {
        let cfg = config::get_config();
        let (cols, rows) = self.addon_window();
        let abs = |p: &std::path::Path| {
            p.canonicalize().unwrap_or_else(|_| p.to_path_buf()).to_string_lossy().into_owned()
        };
        let (terminal, term) = match self.terminal_type {
            TerminalType::Petscii if petscii_native => ("petscii", "dumb"),
            // Translated: the add-on writes ANSI and the gateway turns it into
            // PETSCII, so ANSI is what it should write.
            TerminalType::Petscii => ("petscii", "ansi"),
            TerminalType::Ascii => ("ascii", "dumb"),
            TerminalType::Ansi => ("ansi", "ansi"),
        };
        let connection = if self.is_serial {
            "serial"
        } else if self.is_relay {
            "relay"
        } else if self.is_ssh {
            "ssh"
        } else {
            "telnet"
        };
        let m = &a.manifest;
        [
            ("EG_TERMINAL", terminal.to_string()),
            ("EG_PETSCII", if petscii_native { "native" } else { "translate" }.to_string()),
            ("EG_INPUT", if m.input == Input::Raw { "raw" } else { "line" }.to_string()),
            ("EG_COLUMNS", cols.to_string()),
            ("EG_ROWS", rows.to_string()),
            ("EG_COLOR", if self.color_enabled { "1" } else { "0" }.to_string()),
            // The gateway login, when there was one: never its password.
            (
                "EG_USER",
                if self.authenticated && cfg.security_enabled { cfg.username.clone() } else { String::new() },
            ),
            ("EG_REMOTE_ADDR", self.peer_addr.map(|a| a.to_string()).unwrap_or_default()),
            ("EG_CONNECTION", connection.to_string()),
            ("EG_ADDON_DIR", abs(&a.dir)),
            ("EG_DATA_DIR", abs(std::path::Path::new(config::DATA_DIR))),
            ("EG_TRANSFER_DIR", abs(std::path::Path::new(&cfg.transfer_dir))),
            ("EG_GATEWAY_VERSION", env!("CARGO_PKG_VERSION").to_string()),
            ("TERM", term.to_string()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
    }

    /// Run a terminal add-on, bridged to this caller, until it exits or the
    /// caller leaves; then say how it ended.
    pub(in crate::telnet) async fn run_terminal_addon(&mut self, a: &AddOn) -> Result<(), std::io::Error> {
        let m = a.manifest.clone();
        let name = self.remote_text(&m.name);
        let _solo = if m.single_instance {
            match crate::addons::SingleInstance::claim(&m.id) {
                Some(g) => Some(g),
                None => {
                    self.show_error(&format!("{name} is already in use.")).await?;
                    return Ok(());
                }
            }
        } else {
            None
        };
        let native = self.terminal_type == TerminalType::Petscii && m.petscii == Petscii::Native;
        let filter = match self.terminal_type {
            TerminalType::Petscii if native => GatewayFilter::Raw,
            TerminalType::Petscii => GatewayFilter::Petscii,
            TerminalType::Ascii => GatewayFilter::Ascii,
            TerminalType::Ansi => GatewayFilter::Ansi,
        };
        let env = crate::addons::add_on_env(&self.addon_env(a, native));
        let Some(cmd) = m.command.as_deref() else {
            return Ok(());
        };
        // The containment check again, at the step that relies on it.
        let spawned = crate::addons::command(&a.dir, cmd, &m.args, &env)
            .and_then(|mut c| crate::addons::Tree::spawn(&mut c).map_err(|e| e.to_string()));
        let (mut child, mut tree) = match spawned {
            Ok(c) => c,
            Err(e) => {
                glog!("Add-on \"{}\" could not start: {}", m.id, e);
                self.show_error(&format!("{name} could not start.")).await?;
                return Ok(());
            }
        };
        glog!(
            "Add-on \"{}\" started for {}",
            m.id,
            self.peer_addr.map(|a| a.to_string()).unwrap_or_else(|| "a local session".into())
        );
        let (Some(mut stdin), Some(mut stdout)) = (child.stdin.take(), child.stdout.take()) else {
            tree.kill(&mut child);
            return Ok(());
        };
        // **The caller's keys go to the add-on from a task of their own.**
        // Written inside the loop, a write to an add-on that had stopped
        // reading blocked once its pipe filled -- and with it the whole loop:
        // its output, the timeouts, and the ESC ESC that should have got the
        // caller out.  Keys for an add-on that is not reading are dropped once
        // the queue is full; a stalled program cannot use them anyway.
        let (to_addon_tx, mut to_addon_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let feeder = tokio::spawn(async move {
            while let Some(bytes) = to_addon_rx.recv().await {
                if stdin.write_all(&bytes).await.is_err() {
                    break;
                }
                let _ = stdin.flush().await;
            }
        });
        // What the add-on writes to stderr goes to the gateway's log, so a
        // failing add-on can be diagnosed; capped so a chatty one cannot fill it.
        if let Some(err) = child.stderr.take() {
            let id = m.id.clone();
            tokio::spawn(async move {
                use tokio::io::AsyncBufReadExt;
                let mut lines = tokio::io::BufReader::new(err).lines();
                let mut n = 0u32;
                while let Ok(Some(line)) = lines.next_line().await {
                    n += 1;
                    if n <= 200 {
                        let line: String = line.chars().take(200).collect();
                        crate::logger::log(format!("Add-on \"{id}\": {line}"));
                    } else if n == 201 {
                        crate::logger::log(format!("Add-on \"{id}\": (more output not logged)"));
                    }
                }
            });
        }

        let esc_label = if self.terminal_type == TerminalType::Petscii { "<-" } else { "ESC" };
        self.send_line(&format!("  {}", self.green(&format!("Starting {name}.")))).await?;
        // The manifest's one line about it, the only place a caller sees it:
        // two rows at most, wrapped to the screen.
        if !m.description.trim().is_empty() {
            let width = if self.terminal_type == TerminalType::Petscii { PETSCII_WIDTH } else { 80 } - 3;
            let desc = self.remote_text(m.description.trim());
            for row in crate::aichat::wrap_line(&desc, width).into_iter().take(2) {
                self.send_line(&format!("  {}", self.dim(&row))).await?;
            }
        }
        self.send_line(&format!("  Press {} twice quickly to leave.", self.cyan(esc_label))).await?;
        self.send_line("").await?;
        self.flush().await?;

        // `idle_minutes = 0` leaves the session's own idle timeout in force:
        // an add-on cannot switch off what keeps a half-open caller from
        // holding a session slot for ever.
        let idle = if m.idle_minutes > 0 {
            std::time::Duration::from_secs(60 * u64::from(m.idle_minutes))
        } else {
            self.idle_timeout
        };
        let max_at = (m.max_minutes > 0).then(|| {
            tokio::time::Instant::now() + std::time::Duration::from_secs(60 * u64::from(m.max_minutes))
        });
        let local_telnet = self.speaks_telnet();
        let is_petscii = self.terminal_type == TerminalType::Petscii;
        let erase_char = self.erase_char;
        let reader = &mut self.reader;
        let writer = &self.writer;
        let mut inbound = InboundTelnet::default();
        let mut esc = EscHold::new();
        let mut ansi_state = GatewayOutState::new(filter);
        let mut hold_deadline: Option<tokio::time::Instant> = None;
        let mut out_buf: Vec<u8> = Vec::new();
        let mut nl_buf: Vec<u8> = Vec::new();
        let mut out_last_cr = false;
        let mut in_last_cr = false;
        // Line mode only: 1 after an ESC, 2 inside its CSI/SS3 sequence.
        let mut esc_seq: u8 = 0;
        // Keys the queue had no room for yet (see the `try_send` below).
        let mut backlog: Vec<u8> = Vec::new();
        let mut line: Vec<u8> = Vec::new();
        let mut buf = [0u8; 4096];
        // What Enter sends to the add-on: a newline for a program reading
        // lines, a CR for one that speaks PETSCII itself.
        let enter: u8 = if native { b'\r' } else { b'\n' };

        let ended = loop {
            let flush_at = hold_deadline.unwrap_or_else(|| tokio::time::Instant::now() + GW_FILTER_FLUSH);
            let max_at_or_far = max_at.unwrap_or_else(|| tokio::time::Instant::now() + idle.max(GW_FILTER_FLUSH));
            tokio::select! {
                _ = tokio::time::sleep(idle), if !idle.is_zero() => break Ended::Idle(idle.as_secs()),
                _ = tokio::time::sleep_until(max_at_or_far), if max_at.is_some() => break Ended::TimeUp(m.max_minutes),
                // Room in the queue again: hand over what was held back.
                permit = to_addon_tx.reserve(), if !backlog.is_empty() => {
                    if let Ok(permit) = permit {
                        permit.send(std::mem::take(&mut backlog));
                    }
                }
                _ = tokio::time::sleep_until(flush_at), if hold_deadline.is_some() => {
                    hold_deadline = None;
                    out_buf.clear();
                    ansi_state.flush_pending(&mut out_buf);
                    if !out_buf.is_empty() {
                        let mut w = writer.lock().await;
                        if write_local_data(&mut **w, &out_buf, local_telnet).await.is_err() { break Ended::Disconnected; }
                        if w.flush().await.is_err() { break Ended::Disconnected; }
                    }
                }
                event = read_gateway_event(reader, local_telnet, &mut inbound) => {
                    let b = match event {
                        Ok(GatewayInboundEvent::Data(b)) => b,
                        // A pipe has no window to resize.
                        Ok(GatewayInboundEvent::NawsResize(..)) => continue,
                        Ok(GatewayInboundEvent::Eof) | Err(_) => break Ended::Disconnected,
                    };
                    if is_gateway_leave_key(b, is_petscii) {
                        if esc.press(tokio::time::Instant::now()) { break Ended::Left; }
                    } else {
                        esc.other();
                    }
                    let mut to_addon: Vec<u8> = Vec::new();
                    let mut echo: Vec<u8> = Vec::new();
                    match m.input {
                        Input::Raw => {
                            if let Some(b) = normalize_gateway_input(b, &mut in_last_cr) {
                                gateway_input_for_remote(b, filter, erase_char, &mut to_addon);
                            }
                        }
                        Input::Line => {
                            // An escape sequence is a key, not text: an arrow
                            // key's `ESC [ A` used to put `[A` in the line.
                            if esc_seq == 1 {
                                if b == b'[' || b == b'O' {
                                    esc_seq = 2;
                                    continue;
                                }
                                // Not a sequence: a lone ESC (ESC is the leave
                                // key, so pressed once by habit) must not cost
                                // the caller the next key they type.
                                esc_seq = 0;
                            }
                            if esc_seq == 2 {
                                if (0x40..=0x7E).contains(&b) {
                                    esc_seq = 0;
                                }
                                continue;
                            }
                            if b == 0x1B {
                                esc_seq = 1;
                                continue;
                            }
                            // The leave key is never text in a line: a C64's
                            // back-arrow was echoed as `_` while the caller
                            // was half-way through leaving (seen live).
                            if is_gateway_leave_key(b, is_petscii) {
                                continue;
                            }
                            if (b == b'\n' || b == 0) && in_last_cr {
                                in_last_cr = false;
                                continue;
                            }
                            in_last_cr = b == b'\r';
                            // A bare LF is Enter too, as it is at every prompt
                            // of the gateway's own (`get_line_input`).
                            if b == b'\r' || b == b'\n' {
                                echo.extend_from_slice(if is_petscii { b"\r" } else { b"\r\n" });
                                for &k in &line {
                                    gateway_input_for_remote(k, filter, 0, &mut to_addon);
                                }
                                to_addon.push(enter);
                                line.clear();
                            } else if is_backspace_key(b, erase_char) {
                                // A character, not a byte: a UTF-8 `é` is two.
                                if pop_typed_char(&mut line, !is_petscii) > 0 {
                                    echo.extend_from_slice(if is_petscii { &[0x9D, 0x20, 0x9D] } else { &[0x08, 0x20, 0x08] });
                                }
                            } else if b >= 0x20
                                && b != 0x7F
                                && !(is_petscii && (0x80..=0x9F).contains(&b))
                                && line.len() < MAX_INPUT_LENGTH
                            {
                                // The byte as typed, so the caller's own
                                // encoding draws it.
                                line.push(b);
                                echo.push(b);
                            }
                        }
                    }
                    if !echo.is_empty() {
                        let mut w = writer.lock().await;
                        if write_local_data(&mut **w, &echo, local_telnet).await.is_err() { break Ended::Disconnected; }
                        if w.flush().await.is_err() { break Ended::Disconnected; }
                    }
                    // Never waited on: see `feeder`.  Kept in `backlog` when the
                    // queue is full rather than dropped -- in Raw mode each key
                    // is a message, and a paste read in one pass filled the
                    // queue before the feeder had run at all.
                    if !to_addon.is_empty() {
                        backlog.extend_from_slice(&to_addon);
                        backlog.truncate(ADDON_BACKLOG_MAX);
                        if let Err(tokio::sync::mpsc::error::TrySendError::Full(v)) =
                            to_addon_tx.try_send(std::mem::take(&mut backlog))
                        {
                            backlog = v;
                        }
                    }
                }
                n = stdout.read(&mut buf) => {
                    let n = match n { Ok(0) | Err(_) => break Ended::ByItself, Ok(n) => n };
                    let data: &[u8] = if filter == GatewayFilter::Raw {
                        &buf[..n]
                    } else {
                        nl_buf.clear();
                        crlf(&buf[..n], &mut out_last_cr, &mut nl_buf);
                        &nl_buf
                    };
                    out_buf.clear();
                    filter_gateway_output(data, &mut ansi_state, &mut out_buf);
                    if !out_buf.is_empty() {
                        let mut w = writer.lock().await;
                        if write_local_data(&mut **w, &out_buf, local_telnet).await.is_err() { break Ended::Disconnected; }
                        if w.flush().await.is_err() { break Ended::Disconnected; }
                    }
                    hold_deadline = ansi_state
                        .has_pending()
                        .then(|| tokio::time::Instant::now() + GW_FILTER_FLUSH);
                }
            }
        };

        // Output the filter was still holding (a lone ESC, a title being
        // weighed) is the caller's: released, as the gateway bridges do.
        if ended != Ended::Disconnected {
            out_buf.clear();
            ansi_state.flush_pending(&mut out_buf);
            if !out_buf.is_empty() {
                let mut w = self.writer.lock().await;
                let _ = write_local_data(&mut **w, &out_buf, local_telnet).await;
                let _ = w.flush().await;
            }
        }
        // A telnet command the caller was half-way through must not be read
        // as the key that answers "Press any key" below.
        inbound.finish(&mut *self.reader).await;

        // An add-on that closed its output is given a moment to exit with its
        // own status; then it, and anything it started, is ended regardless
        // (`Tree` holds the group from the start, so a reaped add-on's
        // background programs are reached too).
        drop(to_addon_tx);
        feeder.abort();
        let status = if ended == Ended::ByItself {
            tokio::time::timeout(std::time::Duration::from_secs(2), child.wait()).await.ok().and_then(Result::ok)
        } else {
            None
        };
        tree.kill(&mut child);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), child.wait()).await;
        glog!("Add-on \"{}\" ended ({:?})", m.id, ended);
        if ended == Ended::Disconnected {
            return Ok(());
        }

        let said = match ended {
            Ended::Left => format!("Left {name}."),
            Ended::Idle(secs) if secs >= 60 => format!("{name} ended: idle {} min.", secs / 60),
            Ended::Idle(secs) => format!("{name} ended: idle {secs} s."),
            Ended::TimeUp(min) => format!("{name} ended: its {min} min ran out."),
            _ => match status.and_then(|s| s.code()) {
                Some(code) if code != 0 => format!("{name} ended with error {code}."),
                _ => format!("{name} ended."),
            },
        };
        self.send_line("").await?;
        self.send_line(&format!("  {}", self.yellow(&said))).await?;
        self.send("  Press any key to continue.").await?;
        self.flush().await?;
        self.wait_for_key().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::crlf;

    #[test]
    fn test_a_programs_newlines_reach_the_terminal_as_crlf() {
        let mut out = Vec::new();
        let mut cr = false;
        crlf(b"a\nb\r\nc\r", &mut cr, &mut out);
        crlf(b"\nd", &mut cr, &mut out);
        assert_eq!(out, b"a\r\nb\r\nc\r\nd", "an existing CRLF doubled, or a split one was missed");
    }
}
