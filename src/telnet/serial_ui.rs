//! Serial/modem configuration UI: dialup mapping, the Hayes modem
//! emulator settings screens (port/baud/data/flow/ring), serial-mode
//! toggle, and serial-console help.
//!
//! Split out of `telnet/mod.rs`; behaviour unchanged.

use super::*;

impl TelnetSession {
    // ─── MODEM EMULATOR ──────────────────────────────────────

    // ─── Dialup Mapping ────────────────────────────────────

    pub(in crate::telnet) async fn dialup_mapping(&mut self) -> Result<(), std::io::Error> {
        loop {
            let entries = config::load_dialup_mappings();

            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            self.send_line(&format!("  {}", self.yellow("DIALUP MAPPING")))
                .await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;

            // Built-in gateway entry (not deletable)
            self.send_line(&format!(
                "     {} = {}",
                self.cyan("1001000"),
                self.amber("ethernet-gateway")
            ))
            .await?;

            if entries.is_empty() {
                self.send_line("").await?;
                self.send_line("  No other mappings defined.").await?;
            } else {
                // Show up to 9 user entries to fit the screen
                let max_show = 9;
                for (i, entry) in entries.iter().take(max_show).enumerate() {
                    let num_col = self.cyan(&entry.number);
                    let target = format!("{}:{}", entry.host, entry.port);
                    let line = format!(
                        "  {}. {} = {}",
                        i + 1,
                        num_col,
                        self.amber(&target)
                    );
                    self.send_line(&line).await?;
                }
                if entries.len() > max_show {
                    self.send_line(&format!(
                        "  ... and {} more",
                        entries.len() - max_show
                    ))
                    .await?;
                }
            }
            self.send_line("").await?;

            self.send_line(&format!(
                "  {}  Add mapping",
                self.cyan("A")
            ))
            .await?;
            if !entries.is_empty() {
                self.send_line(&format!(
                    "  {}  Delete mapping",
                    self.cyan("D")
                ))
                .await?;
            }
            self.send_line("").await?;
            self.send_line(&format!(
                "  {}  {}",
                self.action_prompt("Q", "Back"),
                self.action_prompt("H", "Help")
            ))
            .await?;

            let prompt = format!("{}> ", self.cyan("ethernet/dialup"));
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
                "h" => {
                    self.show_help_page("DIALUP MAPPING HELP", Self::dialup_help_lines())
                        .await?;
                }
                "q" => return Ok(()),
                _ => {
                    if entries.is_empty() {
                        self.show_error("Press A, H, or Q.").await?;
                    } else {
                        self.show_error("Press A, D, H, or Q.").await?;
                    }
                }
            }
        }
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

        let mut entries = config::load_dialup_mappings();

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

        let mut entries = entries.to_vec();
        let removed = entries.remove(idx);
        config::save_dialup_mappings(&entries);
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
                    self.send_line(&format!(
                        "      {} {}",
                        self.amber(&truncate_to_width(&port.port, 23)),
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
            let prompt = format!("{}> ", self.cyan("ethernet/serial"));
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
            "  flag, role (Modem Emulator or Serial",
            "  Console), device path, baud rate, and",
            "  AT/S-register state.",
            "",
            "  Pick A or B to configure that port.",
            "  Inside, press T to toggle between",
            "  Modem and Console mode for the port",
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
        let original_cfg = config::get_config();

        loop {
            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;

            let cfg = config::get_config();
            let port = cfg.port(id).clone();
            let console_mode = port.mode == "console";
            let title = if console_mode {
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
            let mode_label = if console_mode {
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
            let port_display = if port.port.is_empty() {
                "(not set)".to_string()
            } else {
                port.port.clone()
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
            if console_mode {
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
            if console_mode {
                self.send_line(&format!(
                    "  Flow:   {}",
                    self.amber(&port.flowcontrol)
                ))
                .await?;
            } else {
                let petscii_state = if port.petscii_translate { "on" } else { "off" };
                self.send_line(&format!(
                    "  Flow:   {}   PETSCII: {}",
                    self.amber(&port.flowcontrol),
                    self.amber(petscii_state)
                ))
                .await?;
            }
            if port.enabled && !console_mode {
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
            // T moved here from the Configuration menu so each port's
            // mode toggle lives next to the rest of its settings.
            // Hidden only when the caller is dialed in on THIS port —
            // flipping their own port to console mid-session would
            // tear down their connection before they could confirm.
            // Hiding T for the OTHER port would be over-conservative:
            // restarting Port B from a Port A serial session is safe.
            let toggling_own_port = self.is_serial && self.serial_port_id == Some(id);
            if !toggling_own_port {
                self.send_line(&format!(
                    "  {}  Toggle Modem/Console mode",
                    self.cyan("T")
                ))
                .await?;
            }
            self.send_line(&format!(
                "  {}  Select serial port",
                self.cyan("S")
            ))
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
            // menu within the 22-row PETSCII budget.
            if console_mode {
                self.send_line(&format!(
                    "  {}  Set flow control",
                    self.cyan("F")
                ))
                .await?;
            } else {
                self.send_line(&format!(
                    "  {}  Set flow control   {}  PETSCII",
                    self.cyan("F"),
                    self.cyan("X")
                ))
                .await?;
            }
            // Dialup mapping and ring emulator are modem-emulator
            // features only — they don't apply to a raw console bridge.
            if !console_mode {
                self.send_line(&format!(
                    "  {}  Dialup Mapping   {}  Carrier",
                    self.cyan("D"),
                    self.cyan("C")
                ))
                .await?;
                // Hide Ring on the port the caller is dialed in on
                // (ringing yourself isn't useful) but allow it on the
                // OTHER port — a Port-A serial session can ring Port B's
                // wire if there's separate hardware listening over there.
                let ringing_own_port = self.is_serial && self.serial_port_id == Some(id);
                if !ringing_own_port {
                    self.send_line(&format!(
                        "  {}  Ring emulator",
                        self.cyan("I")
                    ))
                    .await?;
                }
            }
            self.send_line("").await?;
            self.send_line(&format!(
                "  {}  {}",
                self.action_prompt("Q", "Back"),
                self.action_prompt("H", "Help")
            ))
            .await?;

            let prompt_label = if console_mode {
                format!("ethernet/console-{}", id.label().to_ascii_lowercase())
            } else {
                format!("ethernet/modem-{}", id.label().to_ascii_lowercase())
            };
            let prompt = format!("{}> ", self.cyan(&prompt_label));
            self.send(&prompt).await?;
            self.flush().await?;

            let input = match self.get_menu_input(false).await? {
                Some(s) if !s.is_empty() => s,
                _ => {
                    self.modem_apply_settings(id, &original_cfg).await?;
                    return Ok(());
                }
            };

            match input.as_str() {
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
                "t" if !(self.is_serial && self.serial_port_id == Some(id)) => {
                    self.toggle_serial_mode(id).await?;
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
                "x" if !console_mode => {
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
                "c" if !console_mode => {
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
                "d" if !console_mode => {
                    self.dialup_mapping().await?;
                }
                "i" if !(console_mode
                    || self.is_serial && self.serial_port_id == Some(id)) =>
                {
                    self.modem_ring_emulator(id).await?;
                }
                "h" => {
                    self.modem_show_help(id).await?;
                }
                "q" => {
                    self.modem_apply_settings(id, &original_cfg).await?;
                    return Ok(());
                }
                _ => {
                    // T and I are hidden only when the caller is
                    // dialed in on THIS port (toggling/ringing your
                    // own port isn't useful).  Any other combination
                    // shows the full menu.
                    let on_own_port = self.is_serial && self.serial_port_id == Some(id);
                    let msg = match (console_mode, on_own_port) {
                        (true, true) => "Press E, S, B, P, F, H, or Q.",
                        (true, false) => "Press E, T, S, B, P, F, H, or Q.",
                        (false, true) => "Press E, S, B, P, C, D, F, X, H, or Q.",
                        (false, false) => "Press E, T, S, B, P, C, D, F, X, I, H, or Q.",
                    };
                    self.show_error(msg).await?;
                }
            }
        }
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
        let new_port = new_cfg.port(id);
        let old_port = original_cfg.port(id);
        let changed = new_port.enabled != old_port.enabled
            || new_port.mode != old_port.mode
            || new_port.port != old_port.port
            || new_port.baud != old_port.baud
            || new_port.databits != old_port.databits
            || new_port.parity != old_port.parity
            || new_port.stopbits != old_port.stopbits
            || new_port.flowcontrol != old_port.flowcontrol
            || new_port.petscii_translate != old_port.petscii_translate
            || new_port.drive_carrier != old_port.drive_carrier;

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
    pub(in crate::telnet) async fn toggle_serial_mode(
        &mut self,
        id: crate::config::SerialPortId,
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

        let original_cfg = config::get_config();
        let new_mode = if original_cfg.port(id).mode == "console" {
            "modem"
        } else {
            "console"
        };
        let v = new_mode.to_string();
        let key = config::serial_key(id, "mode");
        tokio::task::spawn_blocking(move || {
            config::update_config_value(&key, &v);
        })
        .await
        .ok();
        self.modem_apply_settings(id, &original_cfg).await
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
            ]);
        })
        .await;
    }

    pub(in crate::telnet) async fn modem_select_port(
        &mut self,
        id: crate::config::SerialPortId,
    ) -> Result<(), std::io::Error> {
        let title = format!("PORT {} - DEVICE", id.label());
        loop {
            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            self.send_line(&format!("  {}", self.yellow(&title))).await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;
            self.send_line(&format!("  {}...", self.dim("Detecting ports"))).await?;
            self.flush().await?;

            let ports = tokio::task::spawn_blocking(crate::serial::list_serial_ports)
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
                match input.as_str() {
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
            self.clear_screen().await?;
            self.send_line(&sep).await?;
            self.send_line(&format!("  {}", self.yellow(&title))).await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;
            let max_w = if self.terminal_type == TerminalType::Petscii {
                30
            } else {
                50
            };
            for (i, port) in ports.iter().enumerate() {
                self.send_line(&format!(
                    "  {:>2}. {}",
                    i + 1,
                    truncate_to_width(port, max_w)
                ))
                .await?;
            }
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
            self.send_line(&format!(
                "  {}",
                self.dim("Enter #, R, N, or type a path.")
            )).await?;
            self.send_line(&format!("  {}", self.action_prompt("Q", "Back"))).await?;
            self.send(&format!("  {} ", self.cyan("Port:"))).await?;
            self.flush().await?;

            let input = match self.get_line_input().await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(()),
            };

            let port_key = config::serial_key(id, "port");
            match input.as_str() {
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
                "q" => return Ok(()),
                _ => {}
            }

            if let Ok(idx) = input.parse::<usize>() {
                if idx >= 1 && idx <= ports.len() {
                    let port_name = ports[idx - 1].clone();
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
        let reader = &mut self.reader;
        let writer = &self.writer;
        let is_petscii = self.terminal_type == TerminalType::Petscii;
        let mut answered = false;
        let mut serial_error = false;
        let timeout = tokio::time::sleep(std::time::Duration::from_secs(15));
        tokio::pin!(timeout);

        loop {
            tokio::select! {
                event = rx.recv() => {
                    match event {
                        Some(0) => {
                            // RING — reset timeout on each ring
                            timeout.as_mut().reset(tokio::time::Instant::now()
                                + std::time::Duration::from_secs(15));
                            let mut w = writer.lock().await;
                            let _ = w.write_all(b"  RING...\r\n").await;
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
                byte = read_byte_iac_filtered(reader, true) => {
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
                    serial_error = true;
                    break;
                }
            }
        }

        // Drop the receiver to signal cancellation if we broke out early,
        // and clear the slot in case the serial thread never picked it up.
        drop(rx);
        crate::serial::cancel_ring_request(id);

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
        let console_mode = config::get_config().port(id).mode == "console";
        if console_mode {
            return self.console_show_help().await;
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
                "  Dialing:",
                "  ATDT ethernet-gateway",
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
                "  Dialing:",
                "  ATDT ethernet-gateway",
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
                "             (drives DTR->DCD when the port's drive-carrier opt-in is enabled)",
                "  AT&Dn      DTR handling 0-3",
                "  AT&Kn      Flow control 0-4",
                "  AT+PETSCII=n  PETSCII translation on direct-",
                "             TCP dials (0=off, 1=on; persists)",
                "  ATSn=v     Set S-register n to v",
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
                "  Settings on this menu:",
                "  E  Open or close the device",
                "  S  Pick the serial device path",
                "  B  Match the baud rate of the",
                "     attached hardware",
                "  P  Data bits, parity, stop bits",
                "  F  Flow control: none, software",
                "     (XON/XOFF), or hardware",
                "     (RTS/CTS)",
                "",
                "  Using the bridge:",
                "  Pick Serial Gateway from the",
                "  main menu to enter the bridge.",
                "  Press <- <- (PETSCII) or",
                "  ESC ESC (ANSI/ASCII) to leave.",
                "  A single ESC is forwarded so",
                "  editors like vi keep working.",
                "",
                "  Switching modes:",
                "  Press T in this menu to return",
                "  to Modem Emulator mode.  Each",
                "  port toggles independently.",
            ]
        } else {
            &[
                "  This menu configures this serial port as a",
                "  raw telnet-serial bridge.  No AT commands,",
                "  no dialing - just byte passthrough between",
                "  the telnet session and the connected",
                "  hardware.",
                "",
                "  Settings on this menu:",
                "  E  Open or close the device file",
                "  S  Pick the serial device (/dev/ttyUSB0,",
                "     COM3, etc.)",
                "  B  Match the baud rate of the attached",
                "     hardware",
                "  P  Data bits, parity, stop bits",
                "  F  Flow control: none, software (XON/XOFF),",
                "     or hardware (RTS/CTS)",
                "",
                "  Using the bridge:",
                "  Pick \"Serial Gateway\" from the main menu",
                "  to enter the bridge.  Press ESC ESC to",
                "  disconnect (a single ESC is forwarded to",
                "  the wire so editors like vi keep working).",
                "",
                "  Switching modes:",
                "  Press T in this menu to return to Modem",
                "  Emulator mode.  Each port (A, B) toggles",
                "  independently.",
            ]
        }
    }
}
