//! Session lifecycle: terminal-type detection, authentication, the main
//! menu loop + main menu, farewell, and shared help/error display
//! helpers.
//!
//! Split out of `telnet/mod.rs`; behaviour unchanged.

use super::*;

/// Map a TTYPE name reported by the client (via `IAC SB TTYPE IS ...`)
/// to one of our TerminalType variants. Returns None for names we don't
/// recognize so the caller falls back to the BACKSPACE-press detection.
/// Names arrive uppercase per RFC 1091, but we match case-insensitively
/// to be tolerant of non-compliant clients.
pub(crate) fn match_terminal_name(name: &str) -> Option<TerminalType> {
    let upper = name.trim().to_ascii_uppercase();
    if upper.is_empty() {
        return None;
    }
    // PETSCII clients: C64, C128, and explicit PETSCII names.
    if upper == "C64"
        || upper == "C128"
        || upper == "COMMODORE"
        || upper.starts_with("PETSCII")
        || upper.starts_with("C64")
        || upper.starts_with("C128")
    {
        return Some(TerminalType::Petscii);
    }
    // ANSI-capable: xterm family, vt100+, ansi*, linux console, screen/tmux.
    if upper.starts_with("XTERM")
        || upper.starts_with("VT")
        || upper.starts_with("ANSI")
        || upper.starts_with("LINUX")
        || upper.starts_with("SCREEN")
        || upper.starts_with("TMUX")
        || upper.starts_with("RXVT")
        || upper.starts_with("KONSOLE")
        || upper.starts_with("ALACRITTY")
        || upper.starts_with("WEZTERM")
        || upper == "CYGWIN"
        || upper == "PUTTY"
    {
        return Some(TerminalType::Ansi);
    }
    // Dumb/unknown terminals: fall back to plain ASCII (no color).
    if upper == "DUMB" || upper == "UNKNOWN" || upper == "NETWORK" {
        return Some(TerminalType::Ascii);
    }
    None
}

pub(crate) fn is_backspace_key(byte: u8, erase_char: u8) -> bool {
    byte == erase_char || byte == 0x08 || byte == 0x7F || byte == 0x14
}

impl TelnetSession {

    // ─── Terminal detection ─────────────────────────────────

    pub(in crate::telnet) async fn detect_terminal_type(&mut self) -> Result<(), std::io::Error> {
        // Serial callers don't speak the telnet protocol — dialing
        // ATDT ETHERNET-GATEWAY puts a raw byte stream on the wire, so
        // IAC bytes (0xFF) would render as garbage characters on the
        // C64/CP/M terminal.  Skip option negotiation and go straight
        // to the BACKSPACE prompt for serial.
        if !self.is_serial {
            // Advertise server-side echo + char-at-a-time mode, and request
            // terminal type + window size from the client. Mark the DOs as
            // sent so a client-initiated WILL TTYPE / WILL NAWS is treated
            // as an acknowledgement instead of triggering a duplicate DO.
            self.send_telnet_protocol(&[
                IAC, WILL, OPT_ECHO,
                IAC, WILL, OPT_SGA,
                IAC, DO, OPT_SGA,
                IAC, DO, OPT_TTYPE,
                IAC, DO, OPT_NAWS,
            ])
            .await?;
            self.neg_sent_will[OPT_ECHO as usize] = true;
            self.neg_sent_will[OPT_SGA as usize] = true;
            self.neg_sent_do[OPT_SGA as usize] = true;
            self.neg_sent_do[OPT_TTYPE as usize] = true;
            self.neg_sent_do[OPT_NAWS as usize] = true;
            self.flush().await?;

            // Give the client a moment to respond, then process negotiation
            // replies (including any TTYPE IS / NAWS subnegotiations).
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            self.drain_input().await;
        }

        // If TTYPE already identified the client, skip the manual prompt.
        // `detect_method` records how the terminal type was decided, for
        // the gateway-debug terminal diagnostic emitted below.
        let detect_method;
        if self.ttype_matched {
            self.erase_char = match self.terminal_type {
                TerminalType::Petscii => 0x14,
                _ => 0x7F,
            };
            detect_method = format!(
                "telnet TTYPE \"{}\"",
                self.ttype_raw.as_deref().unwrap_or("?")
            );
        } else {
            self.send_raw(b"\r\nPress BACKSPACE to detect terminal: ")
                .await?;
            self.flush().await?;

            let byte = match tokio::time::timeout(
                std::time::Duration::from_secs(60),
                self.read_byte_filtered(),
            )
            .await
            {
                Ok(result) => match result? {
                    Some(b) => b,
                    None => return Ok(()),
                },
                Err(_) => {
                    self.send_raw(b"\r\n\r\n  Disconnected: idle timeout.\r\n\r\n")
                        .await?;
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "idle timeout during terminal detection",
                    ));
                }
            };

            self.erase_char = byte;
            self.terminal_type = match byte {
                0x14 => TerminalType::Petscii,
                0x08 | 0x7F => TerminalType::Ansi,
                _ => TerminalType::Ascii,
            };
            detect_method = format!("BACKSPACE key 0x{:02x}", byte);
        }

        let type_name = match self.terminal_type {
            TerminalType::Petscii => "PETSCII (Commodore 64)",
            TerminalType::Ansi => "ANSI",
            TerminalType::Ascii => "ASCII",
        };
        self.send(&format!("\r\nTerminal detected: {}\r\n", type_name))
            .await?;
        self.flush().await?;
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        self.drain_input().await;

        // Color preference — user must explicitly choose Y or N
        let color_label = match self.terminal_type {
            TerminalType::Petscii => "PETSCII",
            _ => "ANSI",
        };
        self.send(&format!(
            "Use {} color? (Y/N): ",
            color_label
        ))
        .await?;
        self.flush().await?;

        let accepted = loop {
            let color_byte = match tokio::time::timeout(
                std::time::Duration::from_secs(60),
                self.read_byte_filtered(),
            )
            .await
            {
                Ok(result) => match result? {
                    Some(b) => b,
                    None => return Ok(()),
                },
                Err(_) => {
                    self.send_raw(b"\r\n\r\n  Disconnected: idle timeout.\r\n\r\n")
                        .await?;
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "idle timeout during color selection",
                    ));
                }
            };

            let choice = if self.terminal_type == TerminalType::Petscii {
                petscii_to_ascii_byte(color_byte)
            } else {
                color_byte
            };

            match choice {
                b'y' | b'Y' => {
                    self.send_raw(&[color_byte]).await?;
                    self.send_raw(b"\r\n").await?;
                    self.flush().await?;
                    break true;
                }
                b'n' | b'N' => {
                    self.send_raw(&[color_byte]).await?;
                    self.send_raw(b"\r\n").await?;
                    self.flush().await?;
                    break false;
                }
                _ => continue, // ignore other keys
            }
        };

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        self.drain_input().await;

        // Color is tracked independently of the terminal encoding, so
        // declining it never discards the layout.  Previously "no color"
        // was implemented by forcing TerminalType::Ascii, which dropped a
        // C64 caller out of PETSCII (40 columns, case-swap, gateway ANSI-
        // strip) into 80-column ASCII — visibly wrong on real hardware.
        self.color_enabled = accepted;
        if accepted {
            // A dumb/unknown (ASCII) terminal that opts into color has no
            // color encoding of its own, so treat it as ANSI.  PETSCII and
            // ANSI keep their detected type.
            if self.terminal_type == TerminalType::Ascii {
                self.terminal_type = TerminalType::Ansi;
            }
            self.send_raw(b"Color enabled.\r\n").await?;
        } else {
            self.send_raw(b"Color disabled.\r\n").await?;
        }

        self.send_raw(b"\r\n").await?;
        self.flush().await?;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        self.drain_input().await;

        self.log_terminal_diagnostic(&detect_method, accepted);

        Ok(())
    }

    /// Emit a one-shot, human-readable terminal diagnostic to the gateway
    /// log — but only when gateway-debug tracing is on (the `gateway_debug`
    /// config flag, toggleable from the GUI / web console / Serial Config
    /// menu, or the `EGATEWAY_GATEWAY_DEBUG` env var).  This is the single
    /// place that explains *why a caller did or didn't get color*: the
    /// detected terminal type and how it was decided, the raw TTYPE the
    /// client announced (matched or not), the color choice, the NAWS window
    /// size, the telnet options we advertised, what we'll advertise onward
    /// to a remote host, and — for serial callers — the dialed port's baud
    /// and PETSCII-translate state.  PETSCII translate strips ANSI color
    /// sequences before they reach the caller, which is the most common
    /// reason ANSI color goes missing on a serial line, so it's called out
    /// explicitly.  Costs nothing when the flag and env var are both unset.
    pub(in crate::telnet) fn log_terminal_diagnostic(&self, detect_method: &str, color_answer: bool) {
        if !gw_debug_enabled(config::get_gateway_debug()) {
            return;
        }

        // Only telnet/serial callers reach here — `detect_terminal_type`
        // (the sole caller) is gated behind `!self.is_ssh`.
        let session = if self.is_serial {
            match self.serial_port_id {
                Some(id) => format!("serial port {}", id.label()),
                None => "serial".to_string(),
            }
        } else {
            "telnet (TCP)".to_string()
        };

        let tt = match self.terminal_type {
            TerminalType::Petscii => "PETSCII",
            TerminalType::Ansi => "ANSI",
            TerminalType::Ascii => "ASCII",
        };

        let color = match (self.terminal_type, color_answer) {
            (TerminalType::Ascii, _) => "DISABLED — plain text",
            (_, true) => "ENABLED — caller answered Y",
            (_, false) => "DISABLED — caller answered N",
        };

        let ttype_line = match &self.ttype_raw {
            Some(name) => {
                let matched = match match_terminal_name(name) {
                    Some(TerminalType::Petscii) => "recognized as PETSCII",
                    Some(TerminalType::Ansi) => "recognized as ANSI",
                    Some(TerminalType::Ascii) => "recognized as ASCII",
                    None => "UNRECOGNIZED -> fell back to BACKSPACE probe",
                };
                format!("\"{}\" ({})", name, matched)
            }
            None if self.is_serial => {
                "<none — serial connections skip telnet negotiation>".to_string()
            }
            None => "<none — client sent no TERMINAL-TYPE>".to_string(),
        };

        // What we advertised for each key option, as a will/do summary.
        // For serial these are all "-" (no telnet negotiation happens).
        let opt_state = |opt: u8| -> &'static str {
            let i = opt as usize;
            let willed = self.neg_sent_will[i] && !self.neg_sent_wont[i];
            let doed = self.neg_sent_do[i] && !self.neg_sent_dont[i];
            match (willed, doed) {
                (true, true) => "will+do",
                (true, false) => "will",
                (false, true) => "do",
                (false, false) => "-",
            }
        };

        let window = match (self.window_width, self.window_height) {
            (Some(w), Some(h)) => format!("{}x{}", w, h),
            (Some(w), None) => format!("{}x?", w),
            (None, Some(h)) => format!("?x{}", h),
            (None, None) => "<not negotiated>".to_string(),
        };

        let ssh_term = match self.terminal_type {
            TerminalType::Petscii => "dumb (40x25)",
            TerminalType::Ascii => "dumb (80x24)",
            TerminalType::Ansi => "xterm (80x24)",
        };

        let cfg = config::get_config();

        glog!("[gw-diag] ----- terminal diagnostic ----------------------------");
        glog!("[gw-diag] session:         {}", session);
        glog!("[gw-diag] terminal type:   {}  (via {})", tt, detect_method);
        glog!("[gw-diag] color:           {}", color);
        glog!("[gw-diag] erase char:      0x{:02x}", self.erase_char);
        glog!("[gw-diag] TTYPE reported:  {}", ttype_line);
        glog!(
            "[gw-diag] telnet opts:     ECHO={} SGA={} TTYPE={} NAWS={}  (peer spoke telnet: {})",
            opt_state(OPT_ECHO),
            opt_state(OPT_SGA),
            opt_state(OPT_TTYPE),
            opt_state(OPT_NAWS),
            if self.telnet_negotiated { "yes" } else { "no" },
        );
        glog!("[gw-diag] window (NAWS):   {}", window);
        glog!(
            "[gw-diag] onward advertise: telnet TTYPE=\"{}\"  |  ssh TERM={}",
            gateway_terminal_name(self.terminal_type),
            ssh_term,
        );
        glog!(
            "[gw-diag] config:          telnet_gateway_negotiate={}",
            cfg.telnet_gateway_negotiate,
        );

        if self.is_serial
            && let Some(id) = self.serial_port_id
        {
            let p = cfg.port(id);
            glog!(
                "[gw-diag] serial port {}:   baud={} petscii_translate={}",
                id.label(),
                p.baud,
                if p.petscii_translate { "ON" } else { "off" },
            );
            if p.petscii_translate && self.terminal_type == TerminalType::Ansi {
                glog!(
                    "[gw-diag] *** PETSCII translate is ON: ANSI color sequences are STRIPPED"
                );
                glog!(
                    "[gw-diag] *** before reaching the caller — that produces black & white output."
                );
                glog!(
                    "[gw-diag] *** For ANSI color on this port, turn PETSCII translate OFF",
                );
                glog!(
                    "[gw-diag] *** (AT+PETSCII=0, or the Serial Configuration menu)."
                );
            }
        }

        glog!("[gw-diag] ------------------------------------------------------");
    }

    // ─── Authentication ─────────────────────────────────────

    pub(in crate::telnet) async fn authenticate(&mut self) -> Result<bool, std::io::Error> {
        if let Some(ip) = self.peer_addr
            && is_locked_out(&self.lockouts, ip)
        {
            glog!("Telnet: auth rejected for {} (locked out)", ip);
            self.send_line("").await?;
            self.send_line(&format!(
                "  {}",
                self.red("Too many attempts. Try later.")
            ))
            .await?;
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            return Ok(false);
        }

        let cfg = config::get_config();
        let idle_timeout = std::time::Duration::from_secs(cfg.idle_timeout_secs);
        let sep = self.separator();
        self.clear_screen().await?;
        self.send_line(&sep).await?;
        self.send_line(&format!("  {}", self.yellow("ETHERNET GATEWAY")))
            .await?;
        self.send_line(&sep).await?;
        self.send_line("").await?;

        for attempt in 1..=MAX_AUTH_ATTEMPTS {
            self.send(&format!("  {} ", self.cyan("Username:")))
                .await?;
            self.flush().await?;
            let username = if idle_timeout.is_zero() {
                match self.get_line_input().await {
                    Ok(Some(s)) => s,
                    Ok(None) => return Ok(false),
                    Err(e) => return Err(e),
                }
            } else {
                match tokio::time::timeout(idle_timeout, self.get_line_input()).await {
                    Ok(Ok(Some(s))) => s,
                    Ok(Ok(None)) => return Ok(false),
                    Ok(Err(e)) => return Err(e),
                    Err(_) => {
                        let _ = self
                            .send_line("\r\nDisconnected: idle timeout.")
                            .await;
                        return Ok(false);
                    }
                }
            };

            self.send(&format!("  {} ", self.cyan("Password:")))
                .await?;
            self.flush().await?;
            let password = if idle_timeout.is_zero() {
                match self.get_password_input().await {
                    Ok(Some(s)) => s,
                    Ok(None) => return Ok(false),
                    Err(e) => return Err(e),
                }
            } else {
                match tokio::time::timeout(idle_timeout, self.get_password_input()).await {
                    Ok(Ok(Some(s))) => s,
                    Ok(Ok(None)) => return Ok(false),
                    Ok(Err(e)) => return Err(e),
                    Err(_) => {
                        let _ = self
                            .send_line("\r\nDisconnected: idle timeout.")
                            .await;
                        return Ok(false);
                    }
                }
            };

            // Evaluate BOTH comparisons before combining (no `&&`
            // short-circuit): short-circuiting skips the password compare when
            // the username is wrong, so the response time would leak whether
            // the username was valid.  Mirrors `ssh::auth_password`.
            let user_ok = constant_time_eq(username.as_bytes(), cfg.username.as_bytes());
            let pass_ok = constant_time_eq(password.as_bytes(), cfg.password.as_bytes());
            if user_ok && pass_ok {
                if let Some(ip) = self.peer_addr {
                    clear_lockout(&self.lockouts, ip);
                }
                return Ok(true);
            }

            if let Some(ip) = self.peer_addr {
                let count = record_auth_failure(&self.lockouts, ip);
                if count >= MAX_AUTH_ATTEMPTS {
                    glog!("Telnet: {} locked out after {} failures", ip, count);
                    self.send_line(&format!(
                        "  {}",
                        self.red("Too many failed attempts.")
                    ))
                    .await?;
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    return Ok(false);
                }
            }

            let remaining = MAX_AUTH_ATTEMPTS - attempt;
            if remaining > 0 {
                self.send_line(&format!(
                    "  {} ({} {} remaining)",
                    self.red("Login incorrect."),
                    remaining,
                    if remaining == 1 {
                        "attempt"
                    } else {
                        "attempts"
                    },
                ))
                .await?;
                self.send_line("").await?;
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            } else {
                self.send_line(&format!(
                    "  {}",
                    self.red("Too many failed attempts.")
                ))
                .await?;
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }
        Ok(false)
    }

    // ─── Main session loop ──────────────────────────────────

    pub(crate) async fn run(&mut self) -> Result<(), std::io::Error> {
        let cfg = config::get_config();

        if !self.is_ssh {
            self.detect_terminal_type().await?;

            // Auto-set the IAC-escaping default based on
            // whether the client actually speaks the telnet protocol
            // (RFC 854/856).  detect_terminal_type() has already sent
            // our opening WILL/DO batch and drained the reply window,
            // so session_read_byte has flipped telnet_negotiated on
            // iff the peer answered with any option-negotiation or
            // subnegotiation bytes.  Real telnet clients (PuTTY, Tera
            // Term, C-Kermit, SecureCRT) always negotiate and need
            // 0xFF escaped; raw TCP clients (netcat, IMP8, CCGMS,
            // StrikeTerm, AltairDuino firmware) stay silent and get a
            // transparent byte stream.  Serial sessions skip the
            // negotiation entirely (no IAC), so telnet_negotiated
            // stays false and xmodem_iac is left off — matching the
            // raw byte stream a serial modem caller expects.  The I
            // key on the File Transfer menu still lets the user
            // override per-session.
            self.xmodem_iac = self.telnet_negotiated;

            // Serial sessions don't authenticate — they arrived via
            // ATDT on a physical port, which is its own trust boundary.
            if !self.is_serial
                && cfg.security_enabled
                && !self.authenticate().await?
            {
                return Ok(());
            }
        }

        // The main menu render does its own clear + banner; emitting a
        // separate welcome banner here would just flash on screen before
        // being wiped, which is especially painful at 1200 baud on a C64.
        match self.run_menu_loop().await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                let _ = self
                    .send_line("\r\n\r\nDisconnected: idle timeout.")
                    .await;
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Inner menu loop, separated so that idle timeout errors from any
    /// sub-menu propagate up and are handled uniformly in `run()`.
    pub(in crate::telnet) async fn run_menu_loop(&mut self) -> Result<(), std::io::Error> {
        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                self.send_line("\r\nServer shutting down. Goodbye.")
                    .await?;
                break;
            }

            match self.current_menu {
                Menu::Main => self.render_main_menu().await?,
                Menu::FileTransfer => self.render_file_transfer().await?,
                Menu::Browser => self.render_web_browser().await?,
            }

            let prompt = self.prompt_str();
            self.send(&prompt).await?;
            self.flush().await?;

            let input = self.get_menu_input(true).await?;

            let input = match input {
                Some(s) if !s.is_empty() => s,
                _ => {
                    // ESC pressed — go to main menu or stay
                    if self.current_menu == Menu::Browser {
                        self.web_reset();
                    }
                    self.current_menu = Menu::Main;
                    continue;
                }
            };

            match self.current_menu.clone() {
                Menu::Main => {
                    if !self.handle_main_command(&input).await? {
                        break;
                    }
                }
                Menu::FileTransfer => {
                    self.handle_file_transfer_command(&input).await?;
                }
                Menu::Browser => {
                    self.handle_web_browser_command(&input).await?;
                }
            }
        }

        Ok(())
    }

    // ─── Main menu ──────────────────────────────────────────

    pub(in crate::telnet) async fn render_main_menu(&mut self) -> Result<(), std::io::Error> {
        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;
        self.send_line(&format!("  {}", self.yellow("ETHERNET GATEWAY")))
            .await?;
        self.send_line(&sep).await?;
        self.send_line("").await?;

        // Slave-mode notice (§9 #13).  Shown only on a slave's own inbound
        // menu (never on the master or on a relay session, whose config is
        // the master's).  The slave still serves its own menu, but its
        // serial ports relay to the master, so point the operator there.
        // Costs 3 rows in slave mode only; the main menu is 16/22 rows so
        // a slave lands at ~19, still inside the PETSCII budget.
        {
            let cfg = config::get_config();
            if cfg.gateway_role == "slave" {
                self.send_line(&format!(
                    "  {}",
                    self.amber("SLAVE mode: ports relay to master.")
                ))
                .await?;
                let max_host = if self.terminal_type == TerminalType::Petscii {
                    28 // 40 - "  Master: " - margin
                } else {
                    66
                };
                let host = if cfg.slave_master_host.is_empty() {
                    "(not configured)".to_string()
                } else {
                    truncate_to_width(&cfg.slave_master_host, max_host)
                };
                self.send_line(&format!("  Master: {}", self.amber(&host)))
                    .await?;
                self.send_line("").await?;
            }
        }

        self.send_line(&format!(
            "  {}  AI Chat",
            self.cyan("A")
        ))
        .await?;
        self.send_line(&format!(
            "  {}  Simple Browser",
            self.cyan("B")
        ))
        .await?;
        self.send_line(&format!(
            "  {}  Configuration",
            self.cyan("C")
        ))
        .await?;
        self.send_line(&format!(
            "  {}  File Transfer",
            self.cyan("F")
        ))
        .await?;
        // Always shown.  Eligibility — and the own-port loopback reject
        // for a serial-arrived session — is enforced by the picker, the
        // single source of truth, which explains *why* a port is
        // unavailable rather than silently hiding the entry.  A
        // serial-arrived user can still legitimately bridge to a
        // *different* port (e.g. Port A's device to Port B's), so the
        // item must not be hidden for them.  Keeping it always-present
        // also avoids a menu that flickers as console targets come and
        // go (relevant once remote ports register at runtime).
        self.send_line(&format!(
            "  {}  Serial Gateway",
            self.cyan("G")
        ))
        .await?;
        // CP/M emulator (Flavor B) — gated behind `cpm_emu_enabled`
        // (default-off, runs arbitrary Z80 code).  Hidden when disabled;
        // the `k` handler and the error hint are gated the same way.
        if config::get_config().cpm_emu_enabled {
            self.send_line(&format!(
                "  {}  CP/M System",
                self.cyan("K")
            ))
            .await?;
        }
        self.send_line(&format!(
            "  {}  Troubleshooting",
            self.cyan("R")
        ))
        .await?;
        self.send_line(&format!(
            "  {}  SSH Gateway",
            self.cyan("S")
        ))
        .await?;
        self.send_line(&format!(
            "  {}  Telnet Gateway",
            self.cyan("T")
        ))
        .await?;
        self.send_line(&format!(
            "  {}  Weather",
            self.cyan("W")
        ))
        .await?;
        self.send_line(&format!("  {}  Exit", self.cyan("X")))
            .await?;
        self.send_line("").await?;
        self.send_line(&format!("  {}", self.action_prompt("H", "Help")))
            .await?;
        Ok(())
    }

    pub(in crate::telnet) async fn handle_main_command(&mut self, input: &str) -> Result<bool, std::io::Error> {
        match input {
            "h" => {
                self.show_help_page("HELP", Self::main_help_lines()).await?;
            }
            "r" => {
                self.troubleshooting().await?;
            }
            "w" => {
                self.weather().await?;
            }
            "a" => {
                let cfg = config::get_config();
                if cfg.groq_api_key.is_empty() {
                    self.show_error_lines(&[
                        "No API key configured.",
                        "",
                        "To enable AI Chat:",
                        "1. Visit https://console.groq.com",
                        "2. Create a free account",
                        "3. Generate an API key",
                        "4. Configuration > Other Settings",
                        "   and set the AI API key",
                    ]).await?;
                } else {
                    self.ai_chat(&cfg.groq_api_key).await?;
                }
            }
            "b" => {
                self.current_menu = Menu::Browser;
            }
            "c" => {
                self.configuration().await?;
            }
            "f" => {
                self.current_menu = Menu::FileTransfer;
            }
            "g" => {
                self.gateway_serial().await?;
            }
            // Gated: when `cpm_emu_enabled` is off, `k` falls through to the
            // generic error arm (item hidden, key rejected).
            "k" if config::get_config().cpm_emu_enabled => {
                self.cpm_emulator().await?;
            }
            "s" => {
                self.gateway_ssh().await?;
            }
            "t" => {
                self.gateway_telnet().await?;
            }
            "x" => {
                self.send_farewell().await?;
                return Ok(false);
            }
            _ => {
                // The valid-key hint gains `K` only when the CP/M emulator
                // item is enabled (both variants fit the 40-col budget).
                let hint = if config::get_config().cpm_emu_enabled {
                    "Press A-C, F, G, K, R, S, T, W, X, or H."
                } else {
                    "Press A-C, F, G, R, S, T, W, X, or H."
                };
                self.show_error(hint).await?;
            }
        }
        Ok(true)
    }

    /// Print John 3:16 (KJV) on a fresh page when the user quits from
    /// the main menu, then block long enough for every byte to clock
    /// out on even a 1200 baud link before the caller drops the
    /// connection.  A 1200 baud 8N1 link carries 120 bytes/sec; we
    /// tally the bytes we emit and sleep `bytes / 120 s + 1 s` so the
    /// closing `TCP FIN` / SSH EOF doesn't truncate the final line on
    /// slow retro terminals.
    pub(in crate::telnet) async fn send_farewell(&mut self) -> Result<(), std::io::Error> {
        self.clear_screen().await?;

        // Wrap width leaves a two-char indent on both layouts.  36/76
        // rather than 38/78 keeps room for color-code padding without
        // risking an overflow wrap on narrow PETSCII screens.
        let wrap_width = if self.terminal_type == TerminalType::Petscii {
            36
        } else {
            76
        };
        let verse = "For God so loved the world, that he gave his only \
                     begotten Son, that whosoever believeth in him \
                     should not perish, but have everlasting life.";

        // `byte_count` is a running tally of everything we send after
        // the clear-screen, so the transmit-delay calculation reflects
        // what actually went down the wire.  The clear-screen prefix
        // itself is a handful of bytes (ANSI ESC[2J ESC[H, PETSCII 0x93,
        // or blank for ASCII); 16 is a safe ceiling.
        let mut byte_count: usize = 16;

        self.send_line("").await?;
        byte_count += 2;

        let header = format!("  {}", self.yellow("John 3:16 (KJV)"));
        byte_count += header.len() + 2;
        self.send_line(&header).await?;

        self.send_line("").await?;
        byte_count += 2;

        for line in crate::aichat::wrap_line(verse, wrap_width) {
            let out = format!("  {}", line);
            byte_count += out.len() + 2;
            self.send_line(&out).await?;
        }

        self.send_line("").await?;
        byte_count += 2;
        self.flush().await?;

        // transmit_ms = bytes / 120 s, rounded up.  Adding 1 s of
        // quiet before disconnect lets the final stop-bit settle
        // before we close the socket.
        let transmit_ms = (byte_count as u64).saturating_mul(1000).div_ceil(120);
        tokio::time::sleep(std::time::Duration::from_millis(
            transmit_ms.saturating_add(1000),
        ))
        .await;
        Ok(())
    }
}
