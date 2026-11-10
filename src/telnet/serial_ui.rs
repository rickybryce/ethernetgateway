//! Serial/modem configuration UI: dialup mapping, the Hayes modem
//! emulator settings screens (port/baud/data/flow/ring), serial-mode
//! toggle, and serial-console help.
//!
//! Split out of `telnet/mod.rs`; behaviour unchanged.

use super::*;

/// Column (0-based) where a second key begins on the port settings screen.
///
/// The screen is at its 22-row PETSCII budget, so four of its entries ride on
/// another row's right-hand side rather than taking one of their own.  Those
/// rows are built by different `if` branches -- raw/console/modem mode each
/// draw a different pair -- and they were each hand-spaced with three spaces
/// after a label of a different length, which put `G` at column 26, `X` and
/// `K` at 24 and `C` at 22.  Reported 2026-09-14 from a live Port B screen:
/// three right-hand keys in three different columns, which reads as a
/// misprint rather than a column.
///
/// The longest left label is `Select serial port`: `"  S  Select serial port"`
/// is 23 characters, so its last one sits at index 22 and two spaces would
/// reach column 25.  26 gives that row three spaces and every other row more
/// -- the narrowest column is 25, and this is one past it, which keeps the
/// widest row (`Gw PETSCII`) at 39 columns exactly.  The widest row it produces is the `Gw PETSCII` one
/// at 39 columns, which is the same budget `separator()` uses -- a row that
/// exactly fills a C64's 40 auto-wraps and then takes the trailing CR/LF,
/// costing two rows of the twenty-two.
pub(in crate::telnet) const SERIAL_MENU_SECOND_COL: usize = 26;

/// Dialup mappings per page; see `dialup_rows`.
pub(in crate::telnet) const DIALUP_PAGE: usize = 9;

/// Pages of the dialup mapping screen for `n` user entries.
pub(in crate::telnet) fn dialup_pages(n: usize) -> usize {
    n.div_ceil(DIALUP_PAGE).max(1)
}

/// Ports per page of the device picker, once it pages; see `port_list_rows`.
pub(in crate::telnet) const PORT_LIST_PAGE: usize = 10;

/// How many pages the device picker needs for `n` ports: one while they all
/// fit beside the fixed rows (11 on a C64), else pages of [`PORT_LIST_PAGE`],
/// whose `M` row costs the one port's room.
pub(in crate::telnet) fn port_list_pages(n: usize) -> usize {
    if n <= PORT_LIST_PAGE + 1 { 1 } else { n.div_ceil(PORT_LIST_PAGE) }
}

/// The keys the port settings screen answers, for one port mode, as lowercase
/// letters in alphabetical order.
///
/// `on_own_port` is a caller dialled in on this very port: `T` and `I` are
/// hidden from them -- flipping the mode would tear down the connection they
/// are typing on, and a port takes a ring only while its line is free
/// (`serial::line_free_for_incoming`), which it is not while their own call is
/// up, so the ring screen would wait for nothing.  Any mode other than `modem` is a
/// raw wire and loses every Hayes-only key; only `console` gains the erase key.
///
/// **This is the one statement of the rule**, and both the screen's wrong-key
/// hint and its key arms are measured against it.  The hint used to be four
/// hand-written strings keyed on `console_mode` alone, so Kermit mode was told
/// to press six keys that did nothing, `G` and `K` were never named, and the
/// full modem line printed at 47 columns on a 40-column C64.
pub(in crate::telnet) fn serial_menu_keys(mode: &str, on_own_port: bool) -> Vec<char> {
    let modem = mode == "modem";
    let mut keys = vec!['b', 'e', 'f', 'h', 'p', 'q', 's'];
    if !on_own_port {
        keys.push('t');
    }
    if mode == "console" {
        keys.push('k');
    }
    if modem {
        keys.extend(['a', 'c', 'd', 'g', 'x']);
        if !on_own_port {
            keys.push('i');
        }
    }
    keys.sort_unstable();
    keys
}

/// The wrong-key hint for [`serial_menu_keys`]: alphabetical, with a run of
/// three or more adjacent letters written as a range, so the busiest screen --
/// thirteen keys in modem mode -- still prints inside a C64's 38 columns after
/// `show_error`'s indent (`Press B-I, P, Q, S, T, or X.`).  Same idiom as the
/// main menu's `A-C` and `R-T`.
pub(in crate::telnet) fn serial_menu_key_hint(keys: &[char]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut i = 0;
    while i < keys.len() {
        let mut j = i;
        while j + 1 < keys.len() && keys[j + 1] as u32 == keys[j] as u32 + 1 {
            j += 1;
        }
        let first = keys[i].to_ascii_uppercase();
        if j - i >= 2 {
            parts.push(format!("{}-{}", first, keys[j].to_ascii_uppercase()));
            i = j + 1;
        } else {
            parts.push(first.to_string());
            i += 1;
        }
    }
    match parts.split_last() {
        Some((last, rest)) if !rest.is_empty() => {
            format!("Press {}, or {}.", rest.join(", "), last)
        }
        Some((last, _)) => format!("Press {}.", last),
        None => String::new(),
    }
}

impl TelnetSession {
    // ─── MODEM EMULATOR ──────────────────────────────────────

    /// One port-settings row: a key and its label, optionally with a second
    /// key and label starting at [`SERIAL_MENU_SECOND_COL`].
    ///
    /// **The padding is computed from the plain text, then colour is applied
    /// around it.**  `cyan()` wraps the key in bytes a terminal does not draw,
    /// so aligning on the formatted string would align the invisible ones and
    /// leave the visible columns exactly as ragged as before -- on ANSI, where
    /// the codes are five bytes, and not on ASCII, where there are none.
    ///
    /// A label too long to reach the column keeps a single space rather than
    /// none, so a row can never run two words together -- but that is damage
    /// control, not the answer: such a row is both mis-aligned and over the
    /// screen's width.  `test_the_port_settings_rows_align_their_second_key`
    /// reads the real labels **out of this file** and holds every one of them
    /// short enough that the clamp is never reached.  It used to hold a
    /// hand-copied list instead and could not see a label added or widened at
    /// a call site; widening `Dialup Mapping` by twenty characters left the
    /// whole suite green while the drawn row reached 50 columns on a C64.
    pub(in crate::telnet) fn serial_menu_row(
        &self,
        key: &str,
        label: &str,
        second: Option<(&str, &str)>,
    ) -> String {
        let Some((key2, label2)) = second else {
            return format!("  {}  {}", self.cyan(key), label);
        };
        // Measured on a deliberately uncoloured copy of the same row, so the
        // count cannot accidentally be taken from the coloured one.  Writing
        // it as `5 + label.len()` would be the same number and would invite
        // exactly that mistake the next time the prefix changes.
        let drawn = format!("  {}  {}", key, label).chars().count();
        let pad = SERIAL_MENU_SECOND_COL.saturating_sub(drawn).max(1);
        format!(
            "  {}  {}{}{}  {}",
            self.cyan(key),
            label,
            " ".repeat(pad),
            self.cyan(key2),
            label2,
        )
    }

    // ─── Dialup Mapping ────────────────────────────────────

    pub(in crate::telnet) async fn dialup_mapping(&mut self) -> Result<(), std::io::Error> {
        let mut page = 0usize;
        loop {
            let entries = config::load_dialup_mappings();
            let pages = dialup_pages(entries.len());
            page = page.min(pages - 1);
            self.clear_screen().await?;
            for row in self.dialup_rows(&entries, page) {
                self.send_line(&row).await?;
            }
            let prompt = format!("{}> ", self.cyan("gateway/dialup"));
            self.send(&prompt).await?;
            self.flush().await?;

            let input = match self.get_menu_input(false).await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(()),
            };

            match input.as_str() {
                "a" => {
                    self.dialup_add_entry().await?;
                }
                "d" if !entries.is_empty() => {
                    self.dialup_delete_entry(&entries).await?;
                }
                "m" if pages > 1 => page = (page + 1) % pages,
                "h" => {
                    self.show_help_page("DIALUP MAPPING HELP", Self::dialup_help_lines())
                        .await?;
                }
                "q" => return Ok(()),
                _ => {
                    if entries.is_empty() {
                        self.show_error("Press A, H, or Q.").await?;
                    } else if pages > 1 {
                        self.show_error("Press A, D, H, M, or Q.").await?;
                    } else {
                        self.show_error("Press A, D, H, or Q.").await?;
                    }
                }
            }
        }
    }

    /// The dialup mapping screen, every row but the prompt.
    ///
    /// **Paged past nine**, where it used to stop with "... and N more" and
    /// nothing anywhere showed the rest -- though `D` would delete them, by
    /// numbers the operator could not see.  The `M` row takes the place of that
    /// line, so the screen is the same size; the numbers are the list's own.
    /// A long host is cut to fit, not wrapped onto a second C64 row.
    pub(in crate::telnet) fn dialup_rows(
        &self,
        entries: &[config::DialupEntry],
        page: usize,
    ) -> Vec<String> {
        let sep = self.separator();
        let mut rows = vec![sep.clone(), format!("  {}", self.yellow("DIALUP MAPPING")), sep, String::new()];
        // Built-in gateway entry (not deletable)
        rows.push(format!("     {} = {}", self.cyan("1001000"), self.amber("ethernetgateway")));
        let pages = dialup_pages(entries.len());
        let page = page.min(pages - 1);
        if entries.is_empty() {
            rows.push(String::new());
            rows.push("  No other mappings defined.".into());
        } else {
            let width: usize = if self.terminal_type == TerminalType::Petscii { 39 } else { 79 };
            for (i, entry) in entries.iter().enumerate().skip(page * DIALUP_PAGE).take(DIALUP_PAGE) {
                let number = truncate_to_width(&entry.number, 12);
                let prefix = format!("  {}. {} = ", i + 1, number);
                let room = width.saturating_sub(prefix.chars().count()).max(8);
                let target = truncate_to_width(&format!("{}:{}", entry.host, entry.port), room);
                rows.push(format!("  {}. {} = {}", i + 1, self.cyan(&number), self.amber(&target)));
            }
            if pages > 1 {
                rows.push(format!("  {}  More mappings ({}/{})", self.cyan("M"), page + 1, pages));
            }
        }
        rows.push(String::new());
        rows.push(format!("  {}  Add mapping", self.cyan("A")));
        if !entries.is_empty() {
            rows.push(format!("  {}  Delete mapping", self.cyan("D")));
        }
        rows.push(String::new());
        rows.push(format!("  {}  {}", self.action_prompt("Q", "Back"), self.action_prompt("H", "Help")));
        rows
    }

    pub(in crate::telnet) async fn dialup_add_entry(&mut self) -> Result<(), std::io::Error> {
        self.send_line("").await?;

        self.send(&format!("  {} ", self.cyan("Phone number:")))
            .await?;
        self.flush().await?;
        let number = match self.get_line_input().await? {
            Some(s) if !s.is_empty() => s,
            _ => return Ok(()),
        };

        // Validate: must contain at least one digit
        if !number.chars().any(|c| c.is_ascii_digit()) {
            self.show_error("Number must contain digits.").await?;
            return Ok(());
        }

        self.send(&format!("  {} ", self.cyan("Host:")))
            .await?;
        self.flush().await?;
        let host = match self.get_line_input().await? {
            Some(s) if !s.is_empty() => s,
            _ => return Ok(()),
        };

        self.send(&format!("  {} ", self.cyan("Port (23):")))
            .await?;
        self.flush().await?;
        let port: u16 = match self.get_line_input().await? {
            Some(s) if s.is_empty() => 23,
            Some(s) => match s.parse::<u16>() {
                Ok(p) if p > 0 => p,
                _ => {
                    self.show_error("Invalid port number.").await?;
                    return Ok(());
                }
            },
            None => return Ok(()),
        };

        // Checked load: this path writes the whole file back, so a read
        // failure must abort rather than degrade to an empty list — saving
        // from that would delete every other mapping to add this one.
        let mut entries = match config::try_load_dialup_mappings() {
            Ok(e) => e,
            Err(e) => {
                glog!("Dialup: refusing to save — could not read mappings: {}", e);
                // show_error_lines, not show_error: the latter emits one
                // unwrapped line, and this doesn't fit 40-column PETSCII.
                self.show_error_lines(&[
                    "Could not read the dialup mapping",
                    "file, so it cannot be updated",
                    "without losing the entries it",
                    "already holds.",
                    "",
                    "Check its permissions.",
                ])
                .await?;
                return Ok(());
            }
        };

        // Remove any existing entry with the same normalized number
        let new_norm = config::normalize_phone_number(&number);
        entries.retain(|e| config::normalize_phone_number(&e.number) != new_norm);

        entries.push(config::DialupEntry {
            number,
            host,
            port,
        });
        config::save_dialup_mappings(&entries);

        self.send_line("").await?;
        self.send_line("  Mapping saved.").await?;
        self.send_line("").await?;
        self.send("  Press any key to continue.").await?;
        self.flush().await?;
        self.wait_for_key().await?;
        Ok(())
    }

    pub(in crate::telnet) async fn dialup_delete_entry(
        &mut self,
        entries: &[config::DialupEntry],
    ) -> Result<(), std::io::Error> {
        self.send_line("").await?;
        self.send(&format!(
            "  {} ",
            self.cyan("Entry # to delete:")
        ))
        .await?;
        self.flush().await?;

        let input = match self.get_line_input().await? {
            Some(s) if !s.is_empty() => s,
            _ => return Ok(()),
        };

        let idx: usize = match input.parse::<usize>() {
            Ok(n) if n >= 1 && n <= entries.len() => n - 1,
            _ => {
                self.show_error("Invalid entry number.").await?;
                return Ok(());
            }
        };

        // **The entry shown, from the file as it is now.**  This removed by
        // index from the list read when the screen was drawn and saved that
        // whole list back, so a mapping added or changed meanwhile -- from the
        // web page, the desktop editor or another session -- was erased by a
        // delete of something else.  Read again, checked so a failed read
        // cannot save an empty list, and only the entry the operator saw goes.
        let shown = entries[idx].clone();
        let mut current = match config::try_load_dialup_mappings() {
            Ok(e) => e,
            Err(e) => {
                glog!("Dialup: refusing to delete — could not read mappings: {}", e);
                self.show_error_lines(&[
                    "Could not read the dialup mapping",
                    "file, so nothing was deleted.",
                ])
                .await?;
                return Ok(());
            }
        };
        let Some(at) = current.iter().position(|e| *e == shown) else {
            self.show_error_lines(&[
                "That mapping changed while this",
                "screen was open; nothing deleted.",
            ])
            .await?;
            return Ok(());
        };
        let removed = current.remove(at);
        config::save_dialup_mappings(&current);
        self.send_line(&format!(
            "  Removed: {} = {}:{}",
            removed.number, removed.host, removed.port
        ))
        .await?;
        self.send_line("").await?;
        self.send("  Press any key to continue.").await?;
        self.flush().await?;
        self.wait_for_key().await?;
        Ok(())
    }

    // ─── Modem settings ───────────────────────────────────

    /// Render the Serial Configuration submenu (the new entry point
    /// from Configuration → M).  Lists both ports with their status
    /// and lets the user pick one to drop into `modem_settings`.
    pub(in crate::telnet) async fn serial_configuration_menu(&mut self) -> Result<(), std::io::Error> {
        use crate::config::{SerialPortId, SERIAL_PORT_IDS};

        loop {
            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            self.send_line(&format!("  {}", self.yellow("SERIAL CONFIGURATION")))
                .await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;

            let cfg = config::get_config();
            for id in SERIAL_PORT_IDS {
                let port = cfg.port(id);
                // Two-line per-port entry so the role + device path +
                // baud never overflow the 40-col PETSCII budget.  Line
                // 1: role label; line 2 (when configured): path + baud.
                let label = format!("[{}] Port {}", id.label(), id.label());
                let role_colored = if !port.enabled {
                    self.red("Disabled")
                } else if port.mode == "console" {
                    self.green("Console mode")
                } else if port.mode == "kermit" {
                    self.green("Kermit server")
                } else {
                    self.amber("Modem mode")
                };
                self.send_line(&format!(
                    "  {} - {}",
                    self.cyan(&label),
                    role_colored
                ))
                .await?;
                if !port.port.is_empty() {
                    // **The tail, not the front.**  A device path names its
                    // device at the end (`/dev/serial/by-id/usb-FTDI_..._A10K-
                    // if00-port0`), and cutting the end off showed every long
                    // path as the same folder.  The port's own screen elides
                    // the front for the same reason.
                    let w = if self.terminal_type == TerminalType::Petscii { 23 } else { 60 };
                    self.send_line(&format!(
                        "      {} {}",
                        self.amber(&crate::webbrowser::truncate_path_to_width(&port.port, w)),
                        port.baud
                    ))
                    .await?;
                }
            }
            self.send_line("").await?;
            let dbg_state = if cfg.gateway_debug {
                self.green("ON")
            } else {
                self.red("OFF")
            };
            self.send_line(&format!(
                "  {} - Gateway debug trace: {}",
                self.cyan("[D]"),
                dbg_state
            ))
            .await?;
            let peer_state = if cfg.allow_peer_dial {
                self.green("ON")
            } else {
                self.red("OFF")
            };
            self.send_line(&format!(
                "  {} - Peer-dial (Port@IP): {}",
                self.cyan("[P]"),
                peer_state
            ))
            .await?;
            self.send_line("").await?;
            self.send_line(&format!(
                "  {}  {}  {}  {}",
                self.action_prompt("D", "Debug"),
                self.action_prompt("P", "Peer-dial"),
                self.action_prompt("Q", "Back"),
                self.action_prompt("H", "Help")
            ))
            .await?;
            let prompt = format!("{}> ", self.cyan("gateway/serial"));
            self.send(&prompt).await?;
            self.flush().await?;

            let input = match self.get_menu_input(false).await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(()),
            };
            match input.as_str() {
                "a" => self.modem_settings(SerialPortId::A).await?,
                "b" => self.modem_settings(SerialPortId::B).await?,
                "d" => {
                    let v = (!cfg.gateway_debug).to_string();
                    tokio::task::spawn_blocking(move || {
                        config::update_config_value("gateway_debug", &v);
                    })
                    .await
                    .ok();
                }
                "p" => {
                    let v = (!cfg.allow_peer_dial).to_string();
                    tokio::task::spawn_blocking(move || {
                        config::update_config_value("allow_peer_dial", &v);
                    })
                    .await
                    .ok();
                }
                "h" => self.serial_configuration_help().await?,
                "q" => return Ok(()),
                _ => {
                    self.show_error("Press A, B, D, P, H, or Q.").await?;
                }
            }
        }
    }

    pub(in crate::telnet) async fn serial_configuration_help(&mut self) -> Result<(), std::io::Error> {
        self.show_help_page("SERIAL CONFIGURATION HELP", Self::serial_config_help_lines())
            .await
    }

    /// Serial-configuration submenu help (single width — fits 40 so it serves
    /// PETSCII too).  Associated fn so a unit test asserts it fits 40 cols.
    pub(in crate::telnet) fn serial_config_help_lines() -> &'static [&'static str] {
        &[
            "  Each serial port has its own enabled",
            "  flag, role (Modem Emulator, Serial",
            "  Console, or Kermit Server), device",
            "  path, baud rate, and AT/S-register",
            "  state.",
            "",
            "  Pick A or B to configure that port.",
            "  Inside, press T to cycle Modem ->",
            "  Console -> Kermit mode for the port",
            "  you're editing.",
            "",
            "  Press D to toggle the gateway debug",
            "  trace (byte-level logging of SSH/",
            "  Telnet gateway sessions). Takes effect",
            "  on the next gateway session.",
            "",
            "  Press P to toggle peer-dial: a modem",
            "  port may dial another port directly",
            "  (ATD Port@IP) or ring a modem port",
            "  picked from the Serial Gateway menu,",
            "  instead of the gateway menu.",
        ]
    }

    pub(in crate::telnet) async fn modem_settings(
        &mut self,
        id: crate::config::SerialPortId,
    ) -> Result<(), std::io::Error> {
        // Snapshot current config so we can detect changes and revert if needed.
        // The screen moves it forward when T applies a change on the spot, so
        // this apply sees only what is still pending.
        let mut original_cfg = config::get_config();
        let shown = self.modem_settings_screen(id, &mut original_cfg).await;
        // Applied however the screen ended, not only on Q: a connection that
        // drops inside a sub-screen (parity, flow, ...) has already saved what
        // it changed there, and leaving that unapplied would hide it -- the
        // next visit snapshots it as the starting point and sees no change.
        // On the caller's own port, the apply finds the link gone and reverts.
        let applied = self.modem_apply_settings(id, &original_cfg).await;
        shown.and(applied)
    }

    /// The per-port settings screen.  Saves as it goes; [`modem_settings`]
    /// applies the result when it returns, by whatever route.
    ///
    /// [`modem_settings`]: Self::modem_settings
    pub(in crate::telnet) async fn modem_settings_screen(
        &mut self,
        id: crate::config::SerialPortId,
        baseline: &mut config::Config,
    ) -> Result<(), std::io::Error> {
        loop {
            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;

            let cfg = config::get_config();
            let port = cfg.port(id).clone();
            let console_mode = port.mode == "console";
            let kermit_mode = port.mode == "kermit";
            // Both console and Kermit-server modes are "raw wire" modes:
            // neither runs the Hayes AT emulator, so both hide the modem-
            // only surfaces (dialup mapping, ring, carrier, PETSCII xlate).
            let raw_mode = port.mode != "modem";
            // Which keys this screen answers.  Every optional row below is
            // drawn from this set and every key arm is gated by it, so a key
            // cannot be drawn where it is refused, nor answered where it is
            // not drawn, nor missing from the wrong-key hint.
            let on_own_port = self.is_serial && self.serial_port_id == Some(id);
            let keys = serial_menu_keys(&port.mode, on_own_port);
            let title = if kermit_mode {
                format!("PORT {} - KERMIT SERVER", id.label())
            } else if console_mode {
                format!("PORT {} - SERIAL CONSOLE", id.label())
            } else {
                format!("PORT {} - MODEM EMULATOR", id.label())
            };
            self.send_line(&format!("  {}", self.yellow(&title)))
                .await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;

            let status = if port.enabled {
                self.green("ENABLED")
            } else {
                self.red("Disabled")
            };
            let mode_label = if kermit_mode {
                self.green("Kermit")
            } else if console_mode {
                self.green("Console")
            } else {
                self.amber("Modem")
            };
            // Status + Mode share one line to keep the menu under
            // the 22-row PETSCII budget when ATD + Dialup + Ring
            // are all visible.
            self.send_line(&format!(
                "  Status: {}  Mode: {}",
                status, mode_label
            ))
            .await?;
            // A device path is the one value on this screen with no length
            // limit -- `/dev/serial/by-id/usb-FTDI_...-if00-port0` is ordinary
            // -- and a row that wraps on a C64 costs two of the twenty-two.
            // The tail names the device, so it is the front that is elided.
            let port_display = if port.port.is_empty() {
                "(not set)".to_string()
            } else {
                let w = if self.terminal_type == TerminalType::Petscii { 29 } else { 69 };
                crate::webbrowser::truncate_path_to_width(&port.port, w)
            };
            self.send_line(&format!(
                "  Port:   {}",
                self.amber(&port_display)
            ))
            .await?;
            self.send_line(&format!(
                "  Baud:   {}",
                self.amber(&port.baud.to_string())
            ))
            .await?;
            let data_str = format!(
                "{}-{}-{}",
                port.databits,
                port.parity.chars().next().unwrap_or('N').to_uppercase(),
                port.stopbits
            );
            // Drive-carrier (DCD proxy) is a modem-emulator feature, so —
            // like PETSCII — it shares an existing row rather than spending
            // one of the 22-row PETSCII budget.  The Data value is stable-
            // width (X-Y-Z), so appending the carrier state here always fits
            // 40 columns.
            if raw_mode {
                self.send_line(&format!("  Data:   {}", self.amber(&data_str)))
                    .await?;
            } else {
                let carrier_state = if port.drive_carrier { "on" } else { "off" };
                self.send_line(&format!(
                    "  Data:   {}   Carrier: {}",
                    self.amber(&data_str),
                    self.amber(carrier_state)
                ))
                .await?;
            }
            // PETSCII xlate is a modem-emulator feature (direct-TCP dials
            // only), so it rides on the Flow line in modem mode rather
            // than spending a row of the 22-row PETSCII budget.
            if raw_mode {
                self.send_line(&format!(
                    "  Flow:   {}",
                    self.amber(&port.flowcontrol)
                ))
                .await?;
            } else {
                self.send_line(&self.modem_flow_row(
                    &port.flowcontrol,
                    port.petscii_translate,
                    &port.gateway_petscii,
                ))
                .await?;
            }
            if port.enabled && !raw_mode {
                self.send_line(&format!(
                    "  {}",
                    self.amber("ATD ETHERNET-GATEWAY")
                ))
                .await?;
            }
            self.send_line("").await?;
            self.send_line(&format!(
                "  {}  Toggle enabled/disabled",
                self.cyan("E")
            ))
            .await?;
            // Shown directly above the Mode line, because Mode is what decides
            // whether this applies at all -- and omitted outright in the other
            // two modes rather than greyed, which a 40-column screen cannot
            // draw. The web page and the desktop editor put it beside their Mode
            // control for the same reason. It is deliberately NOT under the
            // Hayes AT settings: Hayes answers "which byte is the erase key?"
            // with S5, on the AT command line, and this is the console wire.
            if keys.contains(&'k') {
                let w = if self.terminal_type == TerminalType::Petscii { 24 } else { 58 };
                self.send_line(&format!(
                    "  Erase key:  {}",
                    self.amber(&truncate_to_width(
                        crate::serial::backspace_label(&port.backspace),
                        w
                    ))
                ))
                .await?;
            }
            // T moved here from the Configuration menu so each port's
            // mode toggle lives next to the rest of its settings.
            // Hidden only when the caller is dialed in on THIS port —
            // flipping their own port to console mid-session would
            // tear down their connection before they could confirm.
            // Hiding T for the OTHER port would be over-conservative:
            // restarting Port B from a Port A serial session is safe.
            if keys.contains(&'t') {
                self.send_line(&format!(
                    "  {}  Mode: Modem/Console/Kermit",
                    self.cyan("T")
                ))
                .await?;
            }
            // `G` rides this row rather than taking one of its own: this
            // screen is at the 22-row PETSCII budget, the same reason the
            // erase key and PETSCII already share rows here.  Hidden with the
            // rest of the modem-only settings on a raw console/Kermit wire,
            // where no gateway session can arrive.
            let gw = if keys.contains(&'g') { Some(("G", "Gw PETSCII")) } else { None };
            self.send_line(&self.serial_menu_row("S", "Select serial port", gw))
                .await?;
            self.send_line(&format!(
                "  {}  Set baud rate",
                self.cyan("B")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Set data/parity/stop",
                self.cyan("P")
            ))
            .await?;
            // X (PETSCII xlate) shares this row in modem mode to keep the
            // menu within the 22-row PETSCII budget.  The erase key shares it
            // in console mode for the same reason.  Offered only in console
            // mode, because that is the only mode it applies to -- the Kermit
            // server's wire carries 0x08 and 0x7F as packet data, where
            // rewriting them would corrupt a transfer, and a setting that does
            // nothing is worse than no setting.
            let second = if keys.contains(&'x') {
                Some(("X", "PETSCII"))
            } else if keys.contains(&'k') {
                Some(("K", "Erase key"))
            } else {
                None
            };
            self.send_line(&self.serial_menu_row("F", "Set flow control", second))
                .await?;
            // Dialup mapping and ring emulator are modem-emulator
            // features only — they don't apply to a raw console bridge
            // or the Kermit server.
            if keys.contains(&'d') {
                let carrier = if keys.contains(&'c') { Some(("C", "Carrier")) } else { None };
                self.send_line(&self.serial_menu_row("D", "Dialup Mapping", carrier))
                    .await?;
            }
            // Hide Ring on the port the caller is dialed in on
            // (ringing yourself isn't useful) but allow it on the
            // OTHER port — a Port-A serial session can ring Port B's
            // wire if there's separate hardware listening over there.
            //
            // `A`, the saved AT profile, rides the Ring row -- this screen is at
            // the 22-row budget -- and takes a row of its own on the caller's
            // own port, where Ring (and T) are hidden and the row is free.
            if keys.contains(&'a') {
                let row = if keys.contains(&'i') {
                    self.serial_menu_row("I", "Ring emulator", Some(("A", "AT profile")))
                } else {
                    self.serial_menu_row("A", "AT profile", None)
                };
                self.send_line(&row).await?;
            }
            self.send_line("").await?;
            self.send_line(&format!(
                "  {}  {}",
                self.action_prompt("Q", "Back"),
                self.action_prompt("H", "Help")
            ))
            .await?;

            let prompt_label = if kermit_mode {
                format!("gateway/kermit-{}", id.label().to_ascii_lowercase())
            } else if console_mode {
                format!("gateway/console-{}", id.label().to_ascii_lowercase())
            } else {
                format!("gateway/modem-{}", id.label().to_ascii_lowercase())
            };
            let prompt = format!("{}> ", self.cyan(&prompt_label));
            self.send(&prompt).await?;
            self.flush().await?;

            let input = match self.get_menu_input(false).await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(()),
            };

            // The key set gates every arm below, so an arm cannot answer a key
            // the hint does not name, nor the hint name one no arm answers.
            let accepted = matches!(input.as_bytes(), [b] if keys.contains(&(*b as char)));
            match if accepted { input.as_str() } else { "" } {
                "e" => {
                    let new_val = if port.enabled { "false" } else { "true" };
                    let v = new_val.to_string();
                    let key = config::serial_key(id, "enabled");
                    tokio::task::spawn_blocking(move || {
                        config::update_config_value(&key, &v);
                    })
                    .await
                    .ok();
                }
                "k" => {
                    // Cycled in the shared list's own order, like every other
                    // choice here.  A hand-edited value lands on the first
                    // choice next, which is `passthrough` -- the end that
                    // changes nothing.
                    let choices = crate::serial::BACKSPACE_CHOICES;
                    let idx = choices
                        .iter()
                        .position(|(v, _)| *v == port.backspace.trim().to_ascii_lowercase())
                        .map(|i| (i + 1) % choices.len())
                        .unwrap_or(0);
                    let next = choices[idx].0.to_string();
                    // **Restarted, so it takes effect at once** -- the pump
                    // reads the setting when a bridge starts, and on a slave
                    // the master learns it only when the port registers (it
                    // warns before a transfer from it).  But a restart drops
                    // whoever is connected through the port, so with someone
                    // there it is asked first, and "no" changes nothing.
                    let running = port.enabled && !port.port.is_empty();
                    // **The restart applies everything saved, so the question
                    // says so.**  A baud or framing change made earlier on this
                    // screen waits for Q; K's restart applied it too, under a
                    // question that named only the erase key, and then moved
                    // the baseline so Q never asked about it either.
                    let others = port_settings_changed(baseline.port(id), &port);
                    if running
                        && crate::serial::port_session_live(id)
                        && !self.confirm_port_restart(id, "the erase key", others).await?
                    {
                        continue;
                    }
                    let key = config::serial_key(id, "backspace");
                    tokio::task::spawn_blocking(move || {
                        config::update_config_value(&key, &next);
                    })
                    .await
                    .ok();
                    if running {
                        crate::serial::restart_serial(id);
                        // The restart applies everything saved, so the screen's
                        // baseline moves forward with it, as after T -- or Q
                        // applies a change made earlier on this screen a
                        // second time.
                        *baseline = config::get_config();
                    }
                }
                "g" => {
                    // Cycled in the shared list's own order, like every other
                    // choice on this screen.  A hand-edited value lands on the
                    // first choice next, which is `default` -- the end that
                    // changes nothing.
                    let choices = crate::serial::GW_PETSCII_CHOICES;
                    let idx = choices
                        .iter()
                        .position(|(v, _)| *v == port.gateway_petscii.trim().to_ascii_lowercase())
                        .map(|i| (i + 1) % choices.len())
                        .unwrap_or(0);
                    let next = choices[idx].0.to_string();
                    let key = config::serial_key(id, "gateway_petscii");
                    tokio::task::spawn_blocking(move || {
                        config::update_config_value(&key, &next);
                    })
                    .await
                    .ok();
                }
                "t" => {
                    self.toggle_serial_mode(id, baseline).await?;
                }
                "s" => {
                    self.modem_select_port(id).await?;
                }
                "b" => {
                    self.modem_set_baud(id).await?;
                }
                "p" => {
                    self.modem_set_data_params(id).await?;
                }
                "f" => {
                    self.modem_set_flow(id).await?;
                }
                "x" => {
                    // Toggle PETSCII translation and persist immediately —
                    // it's a sticky per-port preference, the same field the
                    // AT+PETSCII command and the web/GUI surfaces write.
                    let new_val = if port.petscii_translate { "false" } else { "true" };
                    let v = new_val.to_string();
                    let key = config::serial_key(id, "petscii_translate");
                    tokio::task::spawn_blocking(move || {
                        config::update_config_value(&key, &v);
                    })
                    .await
                    .ok();
                }
                "c" => {
                    // Toggle the drive-carrier (DCD proxy) opt-in and
                    // persist immediately — same per-port field the web and
                    // GUI surfaces write.  Takes effect on the next port
                    // restart (modem_apply_settings triggers one via the
                    // diff below).
                    let new_val = if port.drive_carrier { "false" } else { "true" };
                    let v = new_val.to_string();
                    let key = config::serial_key(id, "drive_carrier");
                    tokio::task::spawn_blocking(move || {
                        config::update_config_value(&key, &v);
                    })
                    .await
                    .ok();
                }
                "d" => {
                    self.dialup_mapping().await?;
                }
                "i" => {
                    self.modem_ring_emulator(id).await?;
                }
                "a" => {
                    self.modem_at_profile(id).await?;
                }
                "h" => {
                    self.modem_show_help(id).await?;
                }
                "q" => {
                    return Ok(());
                }
                _ => {
                    self.show_error(&serial_menu_key_hint(&keys)).await?;
                }
            }
        }
    }

    /// The port's saved Hayes profile -- the `AT&W` state.
    ///
    /// **Parity with the web page and the desktop editor**, which have always
    /// shown and edited these; telnet only wrote them when a modem user typed
    /// `AT&W`.  A profile is what an operator needs to repair from outside: a
    /// terminal that stored `ATQ1` sees no result codes after its next `ATZ`,
    /// and the way back was to know that and undo it from the terminal, or to
    /// hand-edit the config file.
    ///
    /// Saved at once and **never restarts the port**: the modem reads the saved
    /// profile at `ATZ` and when the port starts, exactly as it reads one the
    /// other two surfaces save, so a caller on the line is not dropped for it.
    /// The ranges are the AT parser's own (`apply_config_key` enforces them
    /// too), so nothing set here is a value a modem user could not have set.
    pub(in crate::telnet) async fn modem_at_profile(
        &mut self,
        id: crate::config::SerialPortId,
    ) -> Result<(), std::io::Error> {
        let set = |key: String, value: String| async move {
            tokio::task::spawn_blocking(move || {
                config::update_config_value(&key, &value);
            })
            .await
            .ok();
        };
        loop {
            let cfg = config::get_config();
            let p = cfg.port(id).clone();

            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            self.send_line(&format!(
                "  {}",
                self.yellow(&format!("PORT {} - AT PROFILE", id.label()))
            ))
            .await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;

            // The CP/M modem profile's layout: a 14-column label puts every
            // value in one column on a 40-column screen.
            let on = |b: bool| if b { "ON" } else { "off" };
            let row = |s: &Self, key: &str, label: &str, value: String| {
                format!("  {}  {:<14}{}", s.cyan(key), label, value)
            };
            self.send_line(&row(self, "E", "Echo (E1)", self.amber(on(p.echo)))).await?;
            self.send_line(&row(self, "V", "Verbose (V1)", self.amber(on(p.verbose)))).await?;
            // `T` for quiet, because `Q` is Back -- as on the CP/M profile.
            self.send_line(&row(self, "T", "Quiet (Q1)", self.amber(on(p.quiet)))).await?;
            self.send_line(&row(self, "X", "Result (X)", self.amber(&p.x_code.to_string())))
                .await?;
            self.send_line(&row(self, "D", "DTR (&D)", self.amber(&p.dtr_mode.to_string())))
                .await?;
            self.send_line(&row(self, "K", "Flow (&K)", self.amber(&p.flow_mode.to_string())))
                .await?;
            self.send_line(&row(self, "C", "DCD (&C)", self.amber(&p.dcd_mode.to_string())))
                .await?;
            // 20 on a C64 for the same reason as the CP/M profile's row: the
            // prefix and label are 19, and a row that fills column 40 wraps.
            let w = if self.terminal_type == TerminalType::Petscii { 20 } else { 55 };
            let sregs = if p.s_regs.trim().is_empty() {
                self.dim("(power-on)")
            } else {
                self.amber(&truncate_to_width(&p.s_regs, w))
            };
            self.send_line(&row(self, "S", "S-registers", sregs)).await?;
            for (i, slot) in p.stored_numbers.iter().enumerate() {
                let shown = if slot.is_empty() {
                    self.dim("(empty)")
                } else {
                    self.amber(&truncate_to_width(slot, w))
                };
                self.send_line(&row(self, &i.to_string(), &format!("Stored &Z{i}"), shown))
                    .await?;
            }
            self.send_line("").await?;
            self.send_line(&format!("  {}", self.dim("Saved now; the modem uses it")))
                .await?;
            self.send_line(&format!("  {}", self.dim("at ATZ or when the port starts.")))
                .await?;
            self.send_line("").await?;
            self.send_line(&format!("  {}", self.action_prompt("Q", "Back"))).await?;

            let prompt = format!(
                "{}> ",
                self.cyan(&format!("gateway/modem-{}/at", id.label().to_ascii_lowercase()))
            );
            self.send(&prompt).await?;
            self.flush().await?;

            let input = match self.get_menu_input(true).await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(()),
            };
            let key = |suffix: &str| config::serial_key(id, suffix);
            match input.as_str() {
                "e" => set(key("echo"), (!p.echo).to_string()).await,
                "v" => set(key("verbose"), (!p.verbose).to_string()).await,
                "t" => set(key("quiet"), (!p.quiet).to_string()).await,
                "x" => set(key("x_code"), ((p.x_code + 1) % 5).to_string()).await,
                "d" => set(key("dtr_mode"), ((p.dtr_mode + 1) % 4).to_string()).await,
                "k" => set(key("flow_mode"), ((p.flow_mode + 1) % 5).to_string()).await,
                "c" => set(key("dcd_mode"), ((p.dcd_mode + 1) % 2).to_string()).await,
                "s" => {
                    self.modem_at_text_field(
                        "S-registers (- = power-on)",
                        key("s_regs"),
                        &p.s_regs,
                    )
                    .await?;
                }
                "0" | "1" | "2" | "3" => {
                    let i: usize = input.parse().unwrap_or(0);
                    self.modem_at_text_field(
                        &format!("Stored &Z{i} (- clears)"),
                        key(&format!("stored_{i}")),
                        &p.stored_numbers[i],
                    )
                    .await?;
                }
                "q" => return Ok(()),
                _ => {
                    self.show_error("Press C-E, K, S, T, V, X, 0-3, or Q.").await?;
                }
            }
        }
    }

    /// One free-text value of the AT profile.  An empty answer keeps the old
    /// value, as on every prompt here, so `-` is how a slot or the S-registers
    /// are emptied -- there is no other way to type "nothing" at a prompt.
    async fn modem_at_text_field(
        &mut self,
        label: &str,
        key: String,
        current: &str,
    ) -> Result<(), std::io::Error> {
        let shown = if current.is_empty() { "(empty)" } else { current };
        let Some(v) = self.other_prompt_value(label, shown, false).await? else {
            return Ok(());
        };
        let v = self.typed_text(&v);
        let v = if v.trim() == "-" { String::new() } else { v.trim().to_string() };
        tokio::task::spawn_blocking(move || {
            config::update_config_value(&key, &v);
        })
        .await
        .ok();
        Ok(())
    }

    /// Ask before a restart that will drop a live connection on port `id`.
    /// `what` names the setting being changed ("the erase key").  True only on
    /// an explicit Y.
    ///
    /// `others`: other changes made on the screen are still waiting to be
    /// applied, and this restart applies them too, so it says so.
    pub(in crate::telnet) async fn confirm_port_restart(
        &mut self,
        id: crate::config::SerialPortId,
        what: &str,
        others: bool,
    ) -> Result<bool, std::io::Error> {
        let label = id.label();
        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;
        self.send_line(&format!("  {}", self.yellow(&format!("PORT {label} - RESTART")))).await?;
        self.send_line(&sep).await?;
        self.send_line("").await?;
        self.send_line(&format!("  Changing {what} restarts")).await?;
        self.send_line(&format!("  Port {label}, and someone is connected")).await?;
        self.send_line("  through it right now.").await?;
        if others {
            self.send_line("  It also applies the other changes").await?;
            self.send_line("  made on this screen.").await?;
        }
        self.send_line("").await?;
        self.send_line(&format!("  {}", self.red("Their connection will be lost."))).await?;
        self.send_line("").await?;
        self.send(&format!("  Restart Port {label} now? (Y/N) ")).await?;
        self.flush().await?;
        self.cpm_confirm().await
    }

    /// The modem-mode Flow status row, which carries the two PETSCII settings
    /// as well.  The gateway's own PETSCII choice rides here, abbreviated, for
    /// the same reason `PETSCII` does: this screen is at the 22-row budget and
    /// a fourth status row would push it over.
    ///
    /// Single spaces between the three, because two made `hardware` / `off` /
    /// `dflt` exactly 40 columns -- a row that fills a C64's 40 auto-wraps and
    /// costs two rows of the twenty-two.
    pub(in crate::telnet) fn modem_flow_row(
        &self,
        flow: &str,
        petscii_translate: bool,
        gateway_petscii: &str,
    ) -> String {
        let petscii_state = if petscii_translate { "on" } else { "off" };
        let gw_state = match gateway_petscii {
            crate::serial::GW_PETSCII_TRANSLATE => "xlat",
            crate::serial::GW_PETSCII_PASSTHROUGH => "raw",
            _ => "dflt",
        };
        format!(
            "  Flow: {} PETSCII: {} Gw: {}",
            self.amber(flow),
            self.amber(petscii_state),
            self.amber(gw_state)
        )
    }

    /// Apply modem settings changes for a specific port.  For serial
    /// users, ask for acknowledgement and revert if no response within
    /// 60 seconds.  Diff is per-port — saving Port A's changes leaves
    /// any in-flight Port B activity alone.
    pub(in crate::telnet) async fn modem_apply_settings(
        &mut self,
        id: crate::config::SerialPortId,
        original_cfg: &config::Config,
    ) -> Result<(), std::io::Error> {
        let new_cfg = config::get_config();
        let changed = port_settings_changed(original_cfg.port(id), new_cfg.port(id));

        if !changed {
            return Ok(());
        }

        // The warn-+-revert flow is only meaningful when the caller's
        // own modem session is the one being reconfigured: changing
        // baud / framing / port-device underneath them would tear
        // down their connection mid-edit, so we ask for explicit
        // Y+Enter confirmation against a 60-s deadline.  When a
        // serial-side caller is editing the OTHER port (e.g. dialed
        // in on Port A, editing Port B), the restart only affects the
        // other manager and the caller's connection is unaffected —
        // skip the warn-+-revert and just apply.
        let editing_own_port = self.is_serial && self.serial_port_id == Some(id);
        if !editing_own_port {
            // Someone else may be on that port.  Restarting it drops them, so
            // with a live session the restart is asked for first; a "no" puts
            // the port's settings back, because a change left saved but not
            // applied is the state `modem_settings` exists to prevent.
            //
            // An answer that never comes -- the connection asking it dropped --
            // is a "no": the revert must still run, so no `?` until it has.
            if crate::serial::port_session_live(id)
                && !matches!(self.confirm_port_restart(id, "these settings", false).await, Ok(true))
            {
                Self::revert_serial_config(id, original_cfg).await;
                let _ = self.send_line(&format!("  {}", self.yellow("Changes undone."))).await;
                let _ = self.send("  Press any key to continue.").await;
                let _ = self.flush().await;
                let _ = self.wait_for_key().await;
                return Ok(());
            }
            crate::serial::restart_serial(id);
            return Ok(());
        }

        // Serial user editing their own port: warn before applying new
        // settings, then require Y+Enter acknowledgement.  Random
        // bytes from a baud mismatch must not count as confirmation.
        // I/O errors during the prompt are non-fatal — we still need
        // to reach the revert logic.
        let _ = self.send_line("").await;
        let _ = self.send_line(&format!(
            "  {}",
            self.yellow("New settings will be applied.")
        )).await;
        let _ = self.send_line(&format!(
            "  {}",
            self.yellow("You have 60 seconds to adjust")
        )).await;
        let _ = self.send_line(&format!(
            "  {}",
            self.yellow("your terminal and type Y then")
        )).await;
        let _ = self.send_line(&format!(
            "  {}",
            self.yellow("Enter, or settings will revert.")
        )).await;
        let _ = self.send_line("").await;
        let _ = self.flush().await;

        // Apply the new serial settings now.
        crate::serial::restart_serial(id);

        let deadline = tokio::time::Instant::now()
            + tokio::time::Duration::from_secs(60);
        let mut next_remind = tokio::time::Instant::now()
            + tokio::time::Duration::from_secs(5);
        let mut got_y = false;

        loop {
            let wait_until = std::cmp::min(next_remind, deadline);
            let remaining = wait_until.saturating_duration_since(tokio::time::Instant::now());

            match tokio::time::timeout(remaining, self.read_byte_filtered()).await {
                Ok(Ok(Some(byte))) => {
                    if got_y {
                        if byte == b'\r' || byte == b'\n' {
                            // Y + Enter — confirmed
                            let _ = self.send_line("").await;
                            let _ = self.send_line(&format!(
                                "  {}",
                                self.green("Settings confirmed.")
                            )).await;
                            let _ = self.send_line("").await;
                            let _ = self.send("  Press any key to continue.").await;
                            let _ = self.flush().await;
                            let _ = self.wait_for_key().await;
                            return Ok(());
                        }
                        // Y followed by non-Enter — noise, reset
                        got_y = false;
                    } else if byte == b'Y' || byte == b'y' {
                        got_y = true;
                    }
                    // Ignore other bytes (likely noise from baud mismatch)
                }
                Ok(Ok(None)) | Ok(Err(_)) => {
                    // Connection lost — revert
                    break;
                }
                Err(_) => {
                    // Timeout interval
                    if tokio::time::Instant::now() >= deadline {
                        break;
                    }
                    let secs_left = deadline
                        .saturating_duration_since(tokio::time::Instant::now())
                        .as_secs();
                    let _ = self.send_line(&format!(
                        "  Type Y+Enter to confirm. ({}s left)",
                        secs_left
                    )).await;
                    let _ = self.flush().await;
                    next_remind += tokio::time::Duration::from_secs(5);
                }
            }
        }

        // No acknowledgement — revert
        let _ = self.send_line("").await;
        let _ = self.send_line(&format!(
            "  {}",
            self.red("No response. Reverting settings.")
        )).await;
        let _ = self.flush().await;

        Self::revert_serial_config(id, original_cfg).await;
        crate::serial::restart_serial(id);
        Ok(())
    }

    /// Toggle one port's mode between "modem" and "console".  Refuses
    /// the toggle when the caller is dialed in over THIS PORT'S modem
    /// — switching that port to console mode would tear down their
    /// own connection before they could acknowledge, and the 60 s
    /// Y+Enter recovery in `modem_apply_settings` cannot be reached
    /// once the modem session is gone.  A serial-side caller toggling
    /// the OTHER port's mode is fine — that restart doesn't affect
    /// their connection.  Console-mode sessions are raw passthroughs
    /// that don't run TelnetSession, so they never reach this code.
    ///
    /// **Applied against the screen's `baseline`, which it then moves
    /// forward.**  It used to apply against a snapshot of its own and leave
    /// the screen's untouched, so leaving with Q saw the mode as still changed
    /// and applied it a second time: a second restart, or -- with someone
    /// connected by then -- "Restart Port B now?" about a change already made,
    /// where N reverted the file while the port kept running the new mode.
    /// Against the baseline, anything else changed on this screen goes with
    /// the same restart, and an N puts back the state the screen opened with.
    pub(in crate::telnet) async fn toggle_serial_mode(
        &mut self,
        id: crate::config::SerialPortId,
        baseline: &mut config::Config,
    ) -> Result<(), std::io::Error> {
        if self.is_serial && self.serial_port_id == Some(id) {
            self.show_error_lines(&[
                "Cannot toggle THIS port's mode",
                "from a modem-side session on it.",
                "Switching to Console would drop",
                "this connection before it could",
                "confirm.",
                "",
                "Connect via telnet, SSH, or the",
                "system console and press T from",
                "the per-port settings menu there.",
            ])
            .await?;
            return Ok(());
        }

        // Cycle Modem -> Console -> Kermit -> Modem.
        let new_mode = match config::get_config().port(id).mode.as_str() {
            "modem" => "console",
            "console" => "kermit",
            _ => "modem",
        };
        let v = new_mode.to_string();
        let key = config::serial_key(id, "mode");
        tokio::task::spawn_blocking(move || {
            config::update_config_value(&key, &v);
        })
        .await
        .ok();
        let applied = self.modem_apply_settings(id, baseline).await;
        // Applied or reverted, the file now matches the running port.
        *baseline = config::get_config();
        applied
    }

    /// Revert one port's config to a previous snapshot using a single
    /// batch write.  Port-scoped — never touches the other port.
    pub(in crate::telnet) async fn revert_serial_config(
        id: crate::config::SerialPortId,
        cfg: &config::Config,
    ) {
        let port = cfg.port(id).clone();
        let _ = tokio::task::spawn_blocking(move || {
            let enabled_key = config::serial_key(id, "enabled");
            let mode_key = config::serial_key(id, "mode");
            let port_key = config::serial_key(id, "port");
            let baud_key = config::serial_key(id, "baud");
            let databits_key = config::serial_key(id, "databits");
            let parity_key = config::serial_key(id, "parity");
            let stopbits_key = config::serial_key(id, "stopbits");
            let flow_key = config::serial_key(id, "flowcontrol");
            // These two are in `modem_apply_settings`' change test, so a
            // change to either one asks for Y+Enter -- and must be undone
            // here when it does not get it.
            let petscii_key = config::serial_key(id, "petscii_translate");
            let carrier_key = config::serial_key(id, "drive_carrier");
            let baud_str = port.baud.to_string();
            let databits_str = port.databits.to_string();
            let stopbits_str = port.stopbits.to_string();
            config::update_config_values(&[
                (enabled_key.as_str(), if port.enabled { "true" } else { "false" }),
                (mode_key.as_str(), port.mode.as_str()),
                (port_key.as_str(), port.port.as_str()),
                (baud_key.as_str(), baud_str.as_str()),
                (databits_key.as_str(), databits_str.as_str()),
                (parity_key.as_str(), port.parity.as_str()),
                (stopbits_key.as_str(), stopbits_str.as_str()),
                (flow_key.as_str(), port.flowcontrol.as_str()),
                (petscii_key.as_str(), if port.petscii_translate { "true" } else { "false" }),
                (carrier_key.as_str(), if port.drive_carrier { "true" } else { "false" }),
            ]);
        })
        .await;
    }

    pub(in crate::telnet) async fn modem_select_port(
        &mut self,
        id: crate::config::SerialPortId,
    ) -> Result<(), std::io::Error> {
        let title = format!("PORT {} - DEVICE", id.label());
        // Which page of a long port list is shown; kept across a redraw, reset
        // by a refresh.
        let mut next_page = 0usize;
        loop {
            let page = std::mem::take(&mut next_page);
            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            self.send_line(&format!("  {}", self.yellow(&title))).await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;
            self.send_line(&format!("  {}...", self.dim("Detecting ports"))).await?;
            self.flush().await?;

            // Detailed, so each row can name the hardware ("-- FTDI") next to
            // the path.  Two identical-looking `/dev/ttyUSB*` entries are
            // otherwise impossible to tell apart from here, and picking wrong
            // yields a port that is simply silent.
            let ports = tokio::task::spawn_blocking(crate::serial::list_serial_ports_detailed)
                .await
                .unwrap_or_default();

            if ports.is_empty() {
                self.clear_screen().await?;
                self.send_line(&sep).await?;
                self.send_line(&format!("  {}", self.yellow(&title))).await?;
                self.send_line(&sep).await?;
                self.send_line("").await?;
                self.send_line(&format!("  {}", self.red("No serial ports detected.")))
                    .await?;
                self.send_line("").await?;
                self.send_line(&format!(
                    "  {}  Refresh port list",
                    self.cyan("R")
                ))
                .await?;
                self.send_line(&format!(
                    "  {}  None (clear port)",
                    self.cyan("N")
                ))
                .await?;
                self.send_line("").await?;
                self.send_line(&format!("  {}", self.action_prompt("Q", "Back")))
                    .await?;
                self.send(&format!("  {} ", self.cyan("Port:"))).await?;
                self.flush().await?;

                let input = match self.get_line_input().await? {
                    Some(s) if !s.is_empty() => s,
                    _ => return Ok(()),
                };
                let port_key = config::serial_key(id, "port");
                // The screen asks for R, N or Q in capitals, so they are taken
                // in either case -- a capital letter falling through would be
                // saved as the device path.  The path itself keeps its case.
                match input.to_ascii_lowercase().as_str() {
                    "r" => continue,
                    "n" => {
                        let k = port_key.clone();
                        tokio::task::spawn_blocking(move || {
                            config::update_config_value(&k, "");
                        })
                        .await
                        .ok();
                        return Ok(());
                    }
                    "q" | "" => return Ok(()),
                    _ => {
                        // Allow typing a port path directly even with no ports detected
                        let port_name = input;
                        let k = port_key.clone();
                        tokio::task::spawn_blocking(move || {
                            config::update_config_value(&k, &port_name);
                        })
                        .await
                        .ok();
                        return Ok(());
                    }
                }
            }

            // Redraw with port list
            let pages = port_list_pages(ports.len());
            let page = page.min(pages - 1);
            self.clear_screen().await?;
            for row in self.port_list_rows(&title, &ports, page) {
                self.send_line(&row).await?;
            }
            self.send(&format!("  {} ", self.cyan("Port:"))).await?;
            self.flush().await?;

            let input = match self.get_line_input().await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(()),
            };

            let port_key = config::serial_key(id, "port");
            // Either case, as above.
            match input.to_ascii_lowercase().as_str() {
                "r" => continue,
                "m" if pages > 1 => {
                    next_page = (page + 1) % pages;
                    continue;
                }
                "n" => {
                    let k = port_key.clone();
                    tokio::task::spawn_blocking(move || {
                        config::update_config_value(&k, "");
                    })
                    .await
                    .ok();
                    return Ok(());
                }
                "q" => return Ok(()),
                _ => {}
            }

            if let Ok(idx) = input.parse::<usize>() {
                if idx >= 1 && idx <= ports.len() {
                    // The *path* is what gets saved — the row's description is
                    // for reading only.
                    let port_name = ports[idx - 1].name.clone();
                    let k = port_key.clone();
                    tokio::task::spawn_blocking(move || {
                        config::update_config_value(&k, &port_name);
                    })
                    .await
                    .ok();
                } else {
                    self.show_error("Invalid selection.").await?;
                    continue;
                }
            } else {
                // Allow typing a port path directly
                let port_name = input;
                let k = port_key.clone();
                tokio::task::spawn_blocking(move || {
                    config::update_config_value(&k, &port_name);
                })
                .await
                .ok();
            }
            return Ok(());
        }
    }

    /// The device picker's screen, every row but the prompt.
    ///
    /// **Paged once the ports outnumber the room.**  The fixed rows take 11 of
    /// a C64's 22, so a twelfth port pushed the list past the screen -- a
    /// machine with a few USB adapters and the board's own UARTs gets there.
    /// Past 11 the list shows [`PORT_LIST_PAGE`] at a time with an `M` row to
    /// page; the numbers stay the list's own, so a number typed from any page
    /// picks the port it names.
    pub(in crate::telnet) fn port_list_rows(
        &self,
        title: &str,
        ports: &[crate::serial::DetectedPort],
        page: usize,
    ) -> Vec<String> {
        let sep = self.separator();
        let mut rows = vec![sep.clone(), format!("  {}", self.yellow(title)), sep, String::new()];
        let max_w = if self.terminal_type == TerminalType::Petscii { 30 } else { 50 };
        let pages = port_list_pages(ports.len());
        let per = if pages > 1 { PORT_LIST_PAGE } else { ports.len() };
        let first = page.min(pages - 1) * per;
        for (i, port) in ports.iter().enumerate().skip(first).take(per) {
            rows.push(format!(
                "  {:>2}. {}",
                i + 1,
                crate::serial::port_row(&port.name, &port.summary, max_w)
            ));
        }
        rows.push(String::new());
        rows.push(format!("  {}  Refresh port list", self.cyan("R")));
        rows.push(format!("  {}  None (clear port)", self.cyan("N")));
        if pages > 1 {
            rows.push(format!(
                "  {}  More ports ({}/{})",
                self.cyan("M"),
                page.min(pages - 1) + 1,
                pages
            ));
        }
        rows.push(String::new());
        let hint = if pages > 1 { "Enter #, R, N, M, or a path." } else { "Enter #, R, N, or type a path." };
        rows.push(format!("  {}", self.dim(hint)));
        rows.push(format!("  {}", self.action_prompt("Q", "Back")));
        rows
    }

    pub(in crate::telnet) async fn modem_set_baud(
        &mut self,
        id: crate::config::SerialPortId,
    ) -> Result<(), std::io::Error> {
        let bauds = [
            "300", "1200", "2400", "4800", "9600", "19200", "38400",
            "57600", "115200",
        ];
        let title = format!("PORT {} - BAUD RATE", id.label());
        loop {
            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            self.send_line(&format!("  {}", self.yellow(&title))).await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;
            for (i, b) in bauds.iter().enumerate() {
                self.send_line(&format!(
                    "  {}  {}",
                    self.cyan(&(i + 1).to_string()),
                    b
                ))
                .await?;
            }
            self.send_line("").await?;
            self.send_line(&format!("  {}", self.action_prompt("Q", "Back"))).await?;
            let prompt = format!("{}> ", self.cyan("baud"));
            self.send(&prompt).await?;
            self.flush().await?;

            let input = match self.get_menu_input(true).await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(()),
            };

            match input.as_str() {
                "q" => return Ok(()),
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" => {
                    // Safe: the match arm only accepts single ASCII digits 1-9.
                    let idx_v = (input.as_bytes()[0] - b'1') as usize;
                    let baud_str = bauds[idx_v].to_string();
                    let key = config::serial_key(id, "baud");
                    tokio::task::spawn_blocking(move || {
                        config::update_config_value(&key, &baud_str);
                    })
                    .await
                    .ok();
                    return Ok(());
                }
                _ => {
                    self.show_error("Press 1-9 or Q.").await?;
                }
            }
        }
    }

    pub(in crate::telnet) async fn modem_set_data_params(
        &mut self,
        id: crate::config::SerialPortId,
    ) -> Result<(), std::io::Error> {
        let title = format!("PORT {} - DATA BITS", id.label());
        // Data bits
        loop {
            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            self.send_line(&format!("  {}", self.yellow(&title))).await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;
            self.send_line(&format!("  {}  5 bits", self.cyan("5"))).await?;
            self.send_line(&format!("  {}  6 bits", self.cyan("6"))).await?;
            self.send_line(&format!("  {}  7 bits", self.cyan("7"))).await?;
            self.send_line(&format!("  {}  8 bits", self.cyan("8"))).await?;
            self.send_line("").await?;
            self.send_line(&format!("  {}", self.action_prompt("Q", "Back"))).await?;
            let prompt = format!("{}> ", self.cyan("data"));
            self.send(&prompt).await?;
            self.flush().await?;

            let input = match self.get_menu_input(true).await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(()),
            };

            match input.as_str() {
                "q" => return Ok(()),
                "5" | "6" | "7" | "8" => {
                    let v = input.clone();
                    let key = config::serial_key(id, "databits");
                    tokio::task::spawn_blocking(move || {
                        config::update_config_value(&key, &v);
                    })
                    .await
                    .ok();
                    break;
                }
                _ => {
                    self.show_error("Press 5-8 or Q.").await?;
                }
            }
        }

        // Parity
        let parity_title = format!("PORT {} - PARITY", id.label());
        loop {
            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            self.send_line(&format!("  {}", self.yellow(&parity_title))).await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;
            self.send_line(&format!("  {}  None", self.cyan("1"))).await?;
            self.send_line(&format!("  {}  Odd", self.cyan("2"))).await?;
            self.send_line(&format!("  {}  Even", self.cyan("3"))).await?;
            self.send_line("").await?;
            self.send_line(&format!("  {}", self.action_prompt("Q", "Back"))).await?;
            let prompt = format!("{}> ", self.cyan("parity"));
            self.send(&prompt).await?;
            self.flush().await?;

            let input = match self.get_menu_input(true).await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(()),
            };

            let parity = match input.as_str() {
                "1" => "none",
                "2" => "odd",
                "3" => "even",
                "q" => return Ok(()),
                _ => {
                    self.show_error("Press 1-3 or Q.").await?;
                    continue;
                }
            };
            let p = parity.to_string();
            let key = config::serial_key(id, "parity");
            tokio::task::spawn_blocking(move || {
                config::update_config_value(&key, &p);
            })
            .await
            .ok();
            break;
        }

        // Stop bits
        let stop_title = format!("PORT {} - STOP BITS", id.label());
        loop {
            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            self.send_line(&format!("  {}", self.yellow(&stop_title))).await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;
            self.send_line(&format!("  {}  1 stop bit", self.cyan("1"))).await?;
            self.send_line(&format!("  {}  2 stop bits", self.cyan("2"))).await?;
            self.send_line("").await?;
            self.send_line(&format!("  {}", self.action_prompt("Q", "Back"))).await?;
            let prompt = format!("{}> ", self.cyan("stop"));
            self.send(&prompt).await?;
            self.flush().await?;

            let input = match self.get_menu_input(true).await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(()),
            };

            match input.as_str() {
                "q" => return Ok(()),
                "1" | "2" => {
                    let v = input.clone();
                    let key = config::serial_key(id, "stopbits");
                    tokio::task::spawn_blocking(move || {
                        config::update_config_value(&key, &v);
                    })
                    .await
                    .ok();
                    return Ok(());
                }
                _ => {
                    self.show_error("Press 1-2 or Q.").await?;
                }
            }
        }
    }

    pub(in crate::telnet) async fn modem_set_flow(
        &mut self,
        id: crate::config::SerialPortId,
    ) -> Result<(), std::io::Error> {
        let title = format!("PORT {} - FLOW CONTROL", id.label());
        loop {
            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            self.send_line(&format!("  {}", self.yellow(&title))).await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;
            self.send_line(&format!("  {}  None", self.cyan("1"))).await?;
            self.send_line(&format!("  {}  Hardware (RTS/CTS)", self.cyan("2"))).await?;
            self.send_line(&format!("  {}  Software (XON/XOFF)", self.cyan("3"))).await?;
            self.send_line("").await?;
            self.send_line(&format!("  {}", self.action_prompt("Q", "Back"))).await?;
            let prompt = format!("{}> ", self.cyan("flow"));
            self.send(&prompt).await?;
            self.flush().await?;

            let input = match self.get_menu_input(true).await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(()),
            };

            let flow = match input.as_str() {
                "1" => "none",
                "2" => "hardware",
                "3" => "software",
                "q" => return Ok(()),
                _ => {
                    self.show_error("Press 1-3 or Q.").await?;
                    continue;
                }
            };
            let f = flow.to_string();
            let key = config::serial_key(id, "flowcontrol");
            tokio::task::spawn_blocking(move || {
                config::update_config_value(&key, &f);
            })
            .await
            .ok();
            return Ok(());
        }
    }

    pub(in crate::telnet) async fn modem_ring_emulator(
        &mut self,
        id: crate::config::SerialPortId,
    ) -> Result<(), std::io::Error> {
        let cfg = config::get_config();
        let port = cfg.port(id).clone();

        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;
        self.send_line(&format!(
            "  {}",
            self.yellow(&format!("PORT {} - RING EMULATOR", id.label()))
        ))
        .await?;
        self.send_line(&sep).await?;
        self.send_line("").await?;

        // Check if serial port is enabled
        if !port.enabled || port.port.is_empty() {
            self.send_line(&format!(
                "  {}",
                self.red("Serial port is not enabled.")
            ))
            .await?;
            self.send_line("").await?;
            self.send("  Press any key to continue.").await?;
            self.flush().await?;
            self.wait_for_key().await?;
            return Ok(());
        }

        // Create progress channel
        let (tx, mut rx) = tokio::sync::mpsc::channel::<u8>(16);

        if !crate::serial::request_ring(id, tx) {
            self.send_line(&format!(
                "  {}",
                self.red("A ring is already in progress.")
            ))
            .await?;
            self.send_line("").await?;
            self.send("  Press any key to continue.").await?;
            self.flush().await?;
            self.wait_for_key().await?;
            return Ok(());
        }

        self.send_line(&format!(
            "  Calling {}...",
            self.amber(&port.port)
        ))
        .await?;
        self.send_line("").await?;
        self.send_line(&format!("  {}", self.action_prompt("Q", "Cancel")))
            .await?;
        self.flush().await?;

        // Show rings as they happen.  Q or ESC cancels (drops rx
        // which signals the serial thread to abort).  Timeout if the
        // serial thread never picks up the request.
        let local_telnet = self.speaks_telnet();
        // Telnet command state across the select! below (see `InboundTelnet`).
        let mut inbound = InboundTelnet::default();
        // Encoded before `self` is split below, so the line gets the same
        // PETSCII case swap as everything else on this screen.
        let ring_line = self.screen_bytes("  RING...\r\n").into_owned();
        let reader = &mut self.reader;
        let writer = &self.writer;
        let is_petscii = self.terminal_type == TerminalType::Petscii;
        let mut answered = false;
        let mut serial_error = false;
        // The timeout is its own outcome, not a port error: with no ring
        // ever sent, the serial side never took the request -- the line is
        // busy (a call parked by `+++`, or not in command mode; see
        // `line_free_for_incoming`) or no modem thread is running because
        // the device will not open.  With rings, nobody answered.
        let mut timed_out = false;
        let mut rang = false;
        let timeout = tokio::time::sleep(std::time::Duration::from_secs(15));
        tokio::pin!(timeout);

        loop {
            tokio::select! {
                event = rx.recv() => {
                    match event {
                        Some(0) => {
                            // RING — reset timeout on each ring
                            rang = true;
                            timeout.as_mut().reset(tokio::time::Instant::now()
                                + std::time::Duration::from_secs(15));
                            let mut w = writer.lock().await;
                            let _ = w.write_all(&ring_line).await;
                            let _ = w.flush().await;
                        }
                        Some(1) => {
                            // Answered
                            answered = true;
                            break;
                        }
                        Some(2) => {
                            // Serial port error
                            serial_error = true;
                            break;
                        }
                        _ => break, // channel closed
                    }
                }
                byte = read_byte_iac_filtered(reader, local_telnet, &mut inbound) => {
                    match byte {
                        Ok(Some(b)) if is_esc_key(b, is_petscii)
                            || b == b'q' || b == b'Q' =>
                        {
                            break;
                        }
                        Ok(None) | Err(_) => break,
                        _ => {} // ignore other keys
                    }
                }
                _ = &mut timeout => {
                    timed_out = true;
                    break;
                }
            }
        }

        // Drop the receiver to signal cancellation if we broke out early,
        // and clear the slot in case the serial thread never picked it up.
        drop(rx);
        crate::serial::cancel_ring_request(id);
        // A command the user's client was half-way through stays ours, not
        // the "Press any key" prompt's (see `InboundTelnet::finish`).
        inbound.finish(&mut *self.reader).await;

        self.send_line("").await?;
        if answered {
            self.send_line(&format!(
                "  {}",
                self.green("Remote machine connected.")
            ))
            .await?;
        } else if serial_error {
            self.send_line(&format!(
                "  {}",
                self.red("Serial connection failed.")
            ))
            .await?;
        } else if timed_out && rang {
            self.send_line(&format!("  {}", self.yellow("No answer."))).await?;
        } else if timed_out {
            // Nobody took the request.  The screen cannot tell a held line
            // from a port the manager is still trying to reopen (an adapter
            // unplugged), so it names both rather than guessing.
            self.send_line(&format!(
                "  {}",
                self.yellow("No ring was sent: the line is busy")
            ))
            .await?;
            self.send_line(&format!("  {}", self.yellow("or the port is not open.")))
                .await?;
        } else {
            self.send_line(&format!(
                "  {}",
                self.yellow("Ring cancelled.")
            ))
            .await?;
        }
        self.send_line("").await?;
        self.send("  Press any key to continue.").await?;
        self.flush().await?;
        self.wait_for_key().await?;
        Ok(())
    }

    pub(in crate::telnet) async fn modem_show_help(
        &mut self,
        id: crate::config::SerialPortId,
    ) -> Result<(), std::io::Error> {
        let mode = config::get_config().port(id).mode.clone();
        if mode == "console" {
            return self.console_show_help().await;
        }
        if mode == "kermit" {
            return self.kermit_mode_show_help().await;
        }
        let lines = Self::modem_help_lines(self.terminal_type == TerminalType::Petscii);
        self.show_help_page("MODEM EMULATOR HELP", lines).await
    }

    /// Hayes modem-emulator help, split by terminal width.  Associated fn so a
    /// unit test asserts the REAL lines fit 40 cols (see `punter_help_lines`).
    pub(in crate::telnet) fn modem_help_lines(petscii: bool) -> &'static [&'static str] {
        if petscii {
            &[
                "  This server emulates a Hayes-",
                "  compatible modem on this serial",
                "  port. Connect retro hardware",
                "  and use AT commands.  The other",
                "  port is configured separately.",
                "",
                "  Port settings:",
                "  E  Open or close the device",
                "  T  Mode: Modem/Console/Kermit",
                "     (not offered on the port you",
                "     are connected through)",
                "  S  Pick the serial device path",
                "  B  Match the baud rate of the",
                "     attached hardware",
                "  P  Data bits, parity, stop bits",
                "  F  Flow control: none, software",
                "     (XON/XOFF), or hardware",
                "     (RTS/CTS)",
                "",
                "  Modem settings:",
                "  G  Gateway PETSCII for Telnet/",
                "     SSH Gateway calls: default",
                "     (server setting), translate,",
                "     or pass through",
                "  X  PETSCII translation on ATDT",
                "     host dials (AT+PETSCII)",
                "  D  Dialup mapping: numbers to",
                "     hosts, shared by both ports",
                "  C  Drive DTR as a carrier (DCD)",
                "     signal for the terminal",
                "  I  Ring this port, as if a call",
                "     were coming in (not offered",
                "     on the port you are",
                "     connected through)",
                "",
                "  A  The saved AT profile (AT&W):",
                "     E/V/Q, X, &D, &K, &C, S-regs",
                "     and stored numbers",
                "",
                "  Dialing:",
                "  ATDT ethernetgateway",
                "    Connect to this gateway",
                "  ATDT host:port",
                "    Dial a remote telnet host",
                "  ATDL     Redial last number",
                "",
                "  Stored numbers:",
                "  AT&Zn=s  Store number in slot",
                "  ATDSn    Dial stored slot 0-3",
                "",
                "  Control:",
                "  ATH      Hang up",
                "  +++      Return to cmd mode",
                "  ATO      Return online",
                "  A/       Repeat last command",
                "",
                "  Information:",
                "  ATIn     Info 0-7 (model, ROM)",
                "  AT&V     Show settings",
                "  ATSn?    Query S-register n",
                "",
                "  Configuration:",
                "  ATXn     Result-code level 0-4",
                "  AT&Cn    DCD mode (0-1)",
                "    (DTR->DCD if opt-in on)",
                "  AT&Dn    DTR handling (0-3)",
                "  AT&Kn    Flow control (0-4)",
                "  AT+PETSCII=n  PETSCII xlate 0/1",
                "  AT&W     Save settings",
                "  ATZ      Reload saved settings",
                "  AT&F     Reset to gateway",
                "           defaults",
                "",
                "  Gateway-friendly defaults:",
                "  S7=15  (50 s Hayes; faster",
                "         failed-dial recovery)",
                "  &D0    (ignore DTR; retro",
                "         clients often don't",
                "         wire it correctly)",
                "  &K0    (no modem flow control;",
                "         port-level serial flow",
                "         is still honored)",
                "",
                "  Override any of these with the",
                "  matching AT command and AT&W.",
            ]
        } else {
            &[
                "  This server emulates a Hayes-compatible",
                "  modem on this serial port.  Connect",
                "  retro hardware (Commodore 64, CP/M,",
                "  Altair, RC2014, etc.) and drive it",
                "  with standard AT commands.",
                "",
                "  Port settings:",
                "  E  Open or close the device file",
                "  T  Mode: Modem/Console/Kermit (not offered",
                "     on the port you are connected through)",
                "  S  Pick the serial device (/dev/ttyUSB0,",
                "     COM3, etc.)",
                "  B  Match the baud rate of the attached",
                "     hardware",
                "  P  Data bits, parity, stop bits",
                "  F  Flow control: none, software (XON/XOFF),",
                "     or hardware (RTS/CTS)",
                "",
                "  Modem settings:",
                "  G  Gateway PETSCII for Telnet/SSH Gateway",
                "     calls: default (the server-wide setting),",
                "     translate, or pass through",
                "  X  PETSCII translation on direct ATDT host",
                "     dials (the same switch as AT+PETSCII)",
                "  D  Dialup mapping: numbers to hosts, shared",
                "     by both ports",
                "  C  Drive DTR as a carrier (DCD) signal for",
                "     a terminal wired DTR->DCD",
                "  I  Ring this port, as if a call were coming",
                "     in (not offered on the port you are",
                "     connected through)",
                "  A  The saved AT profile (AT&W): E/V/Q, X,",
                "     &D, &K, &C, S-registers, stored numbers",
                "",
                "  Dialing:",
                "  ATDT ethernetgateway",
                "    Connect to this gateway's menus",
                "  ATDT host:port",
                "    Dial a remote telnet host",
                "  ATDL       Redial the last number",
                "  ATDP ...   Same as ATDT (no pulse/tone",
                "             distinction on TCP)",
                "",
                "  Stored numbers (4 slots, persistent):",
                "  AT&Zn=str  Store number/host in slot n",
                "  ATDSn      Dial stored slot 0-3",
                "  AT&V       Shows the active table",
                "",
                "  Control:",
                "  ATH        Hang up the active connection",
                "  +++        Return to command mode with",
                "             S2/S12 Hayes guard-time timing",
                "  ATO        Return to online mode",
                "  A/         Repeat the last AT command",
                "             (no CR needed)",
                "",
                "  Information queries:",
                "  ATIn       0-7: model, config, ROM sum,",
                "             ROM test, firmware, OEM, etc.",
                "  AT&V       Show every current setting",
                "  ATSn?      Query S-register n",
                "",
                "  Configuration:",
                "  ATEn       Echo off/on (E0 / E1)",
                "  ATVn       Numeric/verbose result codes",
                "  ATQn       Quiet (Q1 suppresses results)",
                "  ATXn       Result-code level 0-4 (see",
                "             README for the table)",
                "  AT&Cn      DCD: 0=always on, 1=carrier",
                "             (drives DTR->DCD when C is on)",
                "  AT&Dn      DTR handling 0-3",
                "  AT&Kn      Flow control 0-4",
                "  AT+PETSCII=n  PETSCII translation on direct-",
                "             TCP dials (0=off, 1=on; persists)",
                "  ATSn=v     Set S-register n to v",
                "",
                "  Saving:",
                "  AT&W       Save settings to egateway.conf",
                "  ATZ        Reload saved settings",
                "  AT&F       Reset to gateway defaults",
                "",
                "  Gateway-friendly default deviations:",
                "  S7=15      Wait-for-carrier (Hayes: 50 s).",
                "             Keeps failed TCP dials snappy.",
                "  &D0        Ignore DTR (Hayes: &D2 hangs up",
                "             on DTR drop).  Retro clients",
                "             often don't drive DTR correctly,",
                "             which would cause spurious",
                "             disconnects.",
                "  &K0        No modem-level flow control",
                "             (Hayes: &K3 RTS/CTS).  Port-level",
                "             flow is still honored via this",
                "             port's serial_<x>_flowcontrol key",
                "             in egateway.conf.",
                "",
                "  Override any of these with the matching AT",
                "  command and AT&W to persist.",
                "",
                "  Commands the emulator can't meaningfully",
                "  implement on TCP (ATB, ATC, ATL, ATM,",
                "  AT&B/&G/&J/&S/&T/&Y) return OK so legacy",
                "  init strings run to completion.",
            ]
        }
    }

    pub(in crate::telnet) async fn console_show_help(&mut self) -> Result<(), std::io::Error> {
        let lines = Self::console_help_lines(self.terminal_type == TerminalType::Petscii);
        self.show_help_page("SERIAL CONSOLE HELP", lines).await
    }

    /// Serial-console (telnet-serial bridge) help, split by terminal width.
    /// Associated fn so a unit test asserts the REAL lines fit 40 cols.
    pub(in crate::telnet) fn console_help_lines(petscii: bool) -> &'static [&'static str] {
        if petscii {
            &[
                "  This menu configures the serial",
                "  port as a raw telnet-serial",
                "  bridge.  No AT commands, no",
                "  dialing - just byte passthrough",
                "  between the telnet session and",
                "  the connected hardware.",
                "",
                "  Port settings:",
                "  E  Open or close the device",
                "  T  Mode: Modem/Console/Kermit",
                "     (not offered on the port you",
                "     are connected through)",
                "  S  Pick the serial device path",
                "  B  Match the baud rate of the",
                "     attached hardware",
                "  P  Data bits, parity, stop bits",
                "  F  Flow control: none, software",
                "     (XON/XOFF), or hardware",
                "     (RTS/CTS)",
                "",
                "  Console setting:",
                "  K  Erase key: pass through, or",
                "     fold to backspace or rubout",
                "     (leave on pass through when",
                "     a transfer uses this line)",
                "",
                "  Using the bridge:",
                "  Pick Serial Gateway from the",
                "  main menu to enter the bridge.",
                "  Press <- <- (PETSCII) or ESC ESC",
                "  (ANSI/ASCII) quickly to leave.",
                "  A single ESC is forwarded so",
                "  editors like vi keep working.",
            ]
        } else {
            &[
                "  This menu configures this serial port as a",
                "  raw telnet-serial bridge.  No AT commands,",
                "  no dialing - just byte passthrough between",
                "  the telnet session and the connected",
                "  hardware.",
                "",
                "  Port settings:",
                "  E  Open or close the device file",
                "  T  Mode: Modem/Console/Kermit (not offered",
                "     on the port you are connected through)",
                "  S  Pick the serial device (/dev/ttyUSB0,",
                "     COM3, etc.)",
                "  B  Match the baud rate of the attached",
                "     hardware",
                "  P  Data bits, parity, stop bits",
                "  F  Flow control: none, software (XON/XOFF),",
                "     or hardware (RTS/CTS)",
                "",
                "  Console setting:",
                "  K  Erase key: pass through what you type, or",
                "     fold it to backspace or rubout (leave it",
                "     on pass through when a file transfer such",
                "     as PCGET uses this line)",
                "",
                "  Using the bridge:",
                "  Pick \"Serial Gateway\" from the main menu",
                "  to enter the bridge.  Press ESC ESC quickly",
                "  to disconnect (a single ESC is forwarded to",
                "  the wire so editors like vi keep working).",
            ]
        }
    }

    pub(in crate::telnet) async fn kermit_mode_show_help(&mut self) -> Result<(), std::io::Error> {
        let lines = Self::kermit_mode_help_lines(self.terminal_type == TerminalType::Petscii);
        self.show_help_page("KERMIT SERVER HELP", lines).await
    }

    /// Serial Kermit-server-mode help, split by terminal width.
    /// Associated fn so a unit test asserts the REAL lines fit 40 cols.
    pub(in crate::telnet) fn kermit_mode_help_lines(petscii: bool) -> &'static [&'static str] {
        if petscii {
            &[
                "  This menu configures the serial",
                "  port as an always-on Kermit",
                "  server.  As soon as the port is",
                "  enabled it listens for Kermit",
                "  packets - no AT commands, no",
                "  dialing, no menu.",
                "",
                "  Point a Kermit client at the",
                "  wire and use its server commands",
                "  (SEND, GET, DIR, FINISH, BYE).",
                "  Received files land in the",
                "  gateway's transfer directory.",
                "",
                "  Port settings:",
                "  E  Open or close the device",
                "  T  Mode: Modem/Console/Kermit",
                "     (not offered on the port you",
                "     are connected through)",
                "  S  Pick the serial device path",
                "  B  Match the baud rate of the",
                "     attached hardware",
                "  P  Data bits, parity, stop bits",
                "  F  Flow control: none, software",
                "     (XON/XOFF), or hardware",
                "     (RTS/CTS)",
            ]
        } else {
            &[
                "  This menu configures this serial port as an",
                "  always-on Kermit server.  As soon as the",
                "  port is enabled it listens for Kermit",
                "  packets - no AT commands, no dialing, no",
                "  menu.  It is the same server ATDT KERMIT",
                "  reaches, but always on.",
                "",
                "  Point a Kermit client at the wire and use",
                "  its server commands (SEND, GET, DIR,",
                "  FINISH, BYE).  Received files land in the",
                "  gateway's transfer directory; auth and the",
                "  telnet menu are bypassed by design.",
                "",
                "  Port settings:",
                "  E  Open or close the device file",
                "  T  Mode: Modem/Console/Kermit (not offered",
                "     on the port you are connected through)",
                "  S  Pick the serial device (/dev/ttyUSB0,",
                "     COM3, etc.)",
                "  B  Match the baud rate of the attached",
                "     hardware",
                "  P  Data bits, parity, stop bits",
                "  F  Flow control: none, software (XON/XOFF),",
                "     or hardware (RTS/CTS)",
            ]
        }
    }
}

/// Has anything changed between two snapshots of a port that only a restart
/// applies?  One test for the screen's Q and its K, which used to differ.
fn port_settings_changed(old: &config::SerialPortConfig, new: &config::SerialPortConfig) -> bool {
    new.enabled != old.enabled
        || new.mode != old.mode
        || new.port != old.port
        || new.baud != old.baud
        || new.databits != old.databits
        || new.parity != old.parity
        || new.stopbits != old.stopbits
        || new.flowcontrol != old.flowcontrol
        || new.petscii_translate != old.petscii_translate
        || new.drive_carrier != old.drive_carrier
}
