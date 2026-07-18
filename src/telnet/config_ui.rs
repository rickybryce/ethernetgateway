//! Configuration menus: the top-level CONFIGURATION menu, server-address
//! banner, Other/Security/Server/Master-Slave/Gateway config, the
//! per-protocol transfer settings (XMODEM/YMODEM/ZMODEM/Punter/Kermit),
//! and Troubleshooting.
//!
//! Split out of `telnet/mod.rs`; behaviour unchanged.

use super::*;

impl TelnetSession {
    // ─── CONFIGURATION ──────────────────────────────────────

    /// Render the "Server addresses:" banner — the gateway's reachable
    /// IPs (capped at `SERVER_ADDR_DISPLAY_CAP`) plus a sample
    /// `ATD <ip>:<port>` dial string.  Shown at the top of the
    /// CONFIGURATION menu as a "how to reach this gateway" banner.
    /// (Relocated here off the Server Configuration screen in the
    /// master/slave work to free a row for the `M Master/Slave` entry —
    /// §4.7 of the design note.)  No-op when no addresses are detected.
    pub(in crate::telnet) async fn render_server_address_block(&mut self) -> Result<(), std::io::Error> {
        let cfg = config::get_config();
        let addrs = get_server_addresses();
        if addrs.is_empty() {
            return Ok(());
        }
        self.send_line(&format!("  {}", self.dim("Server addresses:")))
            .await?;
        let max_w = if self.terminal_type == TerminalType::Petscii {
            36 // 40 - 4 chars indent
        } else {
            52 // 56 - 4 chars indent
        };
        for addr in addrs.iter().take(SERVER_ADDR_DISPLAY_CAP) {
            let display = truncate_to_width(addr, max_w);
            self.send_line(&format!("    {}", display)).await?;
        }
        if cfg.telnet_enabled {
            let example = format!("ATD {}:{}", addrs[0], cfg.telnet_port);
            let max_example = if self.terminal_type == TerminalType::Petscii {
                38 // 40 - 2 chars indent
            } else {
                54 // 56 - 2 chars indent
            };
            let example = truncate_to_width(&example, max_example);
            self.send_line(&format!("  {}", self.amber(&example))).await?;
        }
        Ok(())
    }

    pub(in crate::telnet) async fn configuration(&mut self) -> Result<(), std::io::Error> {
        loop {
            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            self.send_line(&format!("  {}", self.yellow("CONFIGURATION")))
                .await?;
            self.send_line(&sep).await?;

            // "How to reach this gateway" banner — relocated here from the
            // Server Configuration screen (§4.7) so that screen has room
            // for the M Master/Slave entry.
            self.render_server_address_block().await?;
            self.send_line("").await?;

            // Per-port mode/status is shown under Serial Configuration (M),
            // so the top-level menu no longer duplicates it here.
            self.send_line(&format!(
                "  {}  Security",
                self.cyan("E")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Gateway Configuration",
                self.cyan("G")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Serial Configuration",
                self.cyan("M")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Server Configuration",
                self.cyan("S")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  File Transfer",
                self.cyan("F")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Other Settings",
                self.cyan("O")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Reset Defaults",
                self.cyan("R")
            ))
            .await?;
            self.send_line("").await?;
            self.send_line(&format!(
                "  {}  {}",
                self.action_prompt("Q", "Back"),
                self.action_prompt("H", "Help")
            ))
            .await?;

            let prompt = format!("{}> ", self.cyan("ethernet/config"));
            self.send(&prompt).await?;
            self.flush().await?;

            let input = match self.get_menu_input(false).await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(()),
            };

            match input.as_str() {
                "e" => {
                    self.security_settings().await?;
                }
                "g" => {
                    self.gateway_configuration().await?;
                }
                "m" => {
                    self.serial_configuration_menu().await?;
                }
                "o" => {
                    self.other_settings().await?;
                }
                "s" => {
                    self.server_configuration().await?;
                }
                "f" => {
                    self.file_transfer_settings().await?;
                }
                "r" => {
                    self.config_reset_defaults().await?;
                }
                "h" => {
                    let lines = Self::config_submenu_help_lines(
                        self.terminal_type == TerminalType::Petscii,
                    );
                    self.show_help_page("CONFIGURATION HELP", lines).await?;
                }
                "q" => return Ok(()),
                _ => {
                    self.show_error("Press E, F, G, M, O, R, S, H, or Q.").await?;
                }
            }
        }
    }

    // ─── OTHER SETTINGS ──────────────────────────────────────

    pub(in crate::telnet) async fn other_settings(&mut self) -> Result<(), std::io::Error> {
        loop {
            let cfg = config::get_config();

            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            self.send_line(&format!("  {}", self.yellow("OTHER SETTINGS")))
                .await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;

            let key_display = if cfg.groq_api_key.is_empty() {
                self.red("(not set)")
            } else {
                self.green("(set)")
            };
            self.send_line(&format!("  AI API key:  {}", key_display))
                .await?;
            self.send_line(&format!(
                "  Homepage:    {}",
                self.amber(&cfg.browser_homepage)
            ))
            .await?;
            // Truncate the location to what fits on one line — a saved value
            // can be up to 60 chars, which on a 40-col PETSCII screen would
            // wrap and push this exactly-22-row menu past the budget (the
            // prompt would scroll off a C64).  Width leaves room for the
            // "  Weather:     " prefix and the " [units]" suffix.
            let loc_display = if cfg.weather_location.is_empty() {
                self.dim("(not set)")
            } else {
                let max_loc = if self.terminal_type == TerminalType::Petscii { 16 } else { 48 };
                self.amber(&truncate_to_width(&cfg.weather_location, max_loc))
            };
            // Show the units alongside the location so this menu mirrors the
            // web/GUI (which place the units control next to the location).
            self.send_line(&format!(
                "  Weather:     {} [{}]",
                loc_display,
                self.dim(&cfg.weather_units)
            ))
            .await?;

            // Verbose + GUI share one status row, and Gateway-debug + CP/M
            // share the next, so the added CP/M emulator status keeps this
            // menu inside the 22-row PETSCII budget.
            let verbose_status = if cfg.verbose {
                self.green("ON")
            } else {
                self.dim("off")
            };
            let gui_status = if cfg.enable_console {
                self.green("ON")
            } else {
                self.dim("off")
            };
            self.send_line(&format!(
                "  Verbose: {}   GUI: {}",
                verbose_status, gui_status
            ))
            .await?;

            let gw_dbg_status = if cfg.gateway_debug {
                self.green("ON")
            } else {
                self.dim("off")
            };
            let cpm_status = if cfg.cpm_emu_enabled {
                self.green("ON")
            } else {
                self.dim("off")
            };
            self.send_line(&format!(
                "  Gw dbg: {}   CP/M: {}",
                gw_dbg_status, cpm_status
            ))
            .await?;
            self.send_line("").await?;

            self.send_line(&format!(
                "  {}  Set AI API key (Groq)",
                self.cyan("A")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Set browser homepage",
                self.cyan("B")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Set weather location",
                self.cyan("W")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Cycle weather units",
                self.cyan("U")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Toggle verbose transfer logging",
                self.cyan("V")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Toggle GUI on startup",
                self.cyan("G")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Toggle gateway debug trace",
                self.cyan("D")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Toggle CP/M emulator",
                self.cyan("E")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Restart server",
                self.cyan("R")
            ))
            .await?;
            self.send_line("").await?;
            self.send_line(&format!(
                "  {}  {}",
                self.action_prompt("Q", "Back"),
                self.action_prompt("H", "Help")
            ))
            .await?;

            let prompt = format!("{}> ", self.cyan("ethernet/config/other"));
            self.send(&prompt).await?;
            self.flush().await?;

            let input = match self.get_menu_input(false).await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(()),
            };

            match input.as_str() {
                "a" => {
                    self.other_set_field(
                        "AI API key",
                        "groq_api_key",
                        if cfg.groq_api_key.is_empty() { "(not set)" } else { "(hidden)" },
                        true,
                    )
                    .await?;
                }
                "b" => {
                    self.other_set_field(
                        "Browser homepage",
                        "browser_homepage",
                        &cfg.browser_homepage,
                        false,
                    )
                    .await?;
                }
                "w" => {
                    self.other_set_field(
                        "Weather location",
                        "weather_location",
                        &cfg.weather_location,
                        false,
                    )
                    .await?;
                }
                "u" => {
                    // Cycle auto -> us -> metric -> auto (mirrors the weather
                    // screen's own toggle and the web/GUI picker).
                    let next = match cfg.weather_units.as_str() {
                        "auto" => "us",
                        "us" => "metric",
                        _ => "auto",
                    }
                    .to_string();
                    tokio::task::spawn_blocking(move || {
                        config::update_config_value("weather_units", &next);
                    })
                    .await
                    .ok();
                }
                "v" => {
                    let new_val = if cfg.verbose { "false" } else { "true" };
                    let v = new_val.to_string();
                    tokio::task::spawn_blocking(move || {
                        config::update_config_value("verbose", &v);
                    })
                    .await
                    .ok();
                }
                "g" => {
                    let new_val = if cfg.enable_console { "false" } else { "true" };
                    let v = new_val.to_string();
                    tokio::task::spawn_blocking(move || {
                        config::update_config_value("enable_console", &v);
                    })
                    .await
                    .ok();
                    self.config_restart_notice().await?;
                }
                "d" => {
                    let v = (!cfg.gateway_debug).to_string();
                    tokio::task::spawn_blocking(move || {
                        config::update_config_value("gateway_debug", &v);
                    })
                    .await
                    .ok();
                }
                "e" => {
                    // Toggle the CP/M emulator (Flavor B) main-menu item.
                    // Default-off; runs arbitrary Z80 code once built out.
                    // Takes effect for new menu renders — no restart needed.
                    let v = (!cfg.cpm_emu_enabled).to_string();
                    tokio::task::spawn_blocking(move || {
                        config::update_config_value("cpm_emu_enabled", &v);
                    })
                    .await
                    .ok();
                }
                "r" => {
                    self.config_restart_server().await?;
                }
                "h" => {
                    self.other_show_help().await?;
                }
                "q" => return Ok(()),
                _ => {
                    // Generic hint: this menu now has too many keys (incl. the
                    // CP/M-emulator toggle E) to list within the 40-col budget.
                    self.show_error("Press a letter from the menu.").await?;
                }
            }
        }
    }

    /// Prompt for a free-form (or secret) config string and persist it.
    /// Returns `true` if the value was changed/saved, `false` if the user
    /// cancelled with empty input — so a caller whose setting needs a
    /// server restart can show the restart notice only on an actual change.
    pub(in crate::telnet) async fn other_set_field(
        &mut self,
        label: &str,
        key: &str,
        current_display: &str,
        is_secret: bool,
    ) -> Result<bool, std::io::Error> {
        self.send_line("").await?;
        self.send_line(&format!(
            "  Current {}: {}",
            label.to_lowercase(),
            if is_secret {
                self.dim(current_display)
            } else {
                self.amber(current_display)
            }
        ))
        .await?;
        self.send(&format!("  New {}: ", label.to_lowercase())).await?;
        self.flush().await?;

        let input = if is_secret {
            self.get_password_input().await?
        } else {
            self.get_line_input().await?
        };

        let input = match input {
            Some(s) if !s.is_empty() => s,
            _ => return Ok(false),
        };

        let k = key.to_string();
        let v = input;
        let saved_label = label.to_string();
        tokio::task::spawn_blocking(move || {
            config::update_config_value(&k, &v);
        })
        .await
        .ok();
        self.send_line("").await?;
        self.send_line(&format!(
            "  {}",
            self.green(&format!("{} updated.", saved_label))
        ))
        .await?;
        self.send_line("").await?;
        self.send("  Press any key to continue.").await?;
        self.flush().await?;
        self.wait_for_key().await?;
        Ok(true)
    }

    pub(in crate::telnet) async fn other_show_help(&mut self) -> Result<(), std::io::Error> {
        let lines = Self::other_help_lines(self.terminal_type == TerminalType::Petscii);
        self.show_help_page("OTHER SETTINGS HELP", lines).await
    }

    /// Other-settings help, split by terminal width.  Associated fn so a unit
    /// test asserts the REAL lines fit 40 cols (see `punter_help_lines`).
    pub(in crate::telnet) fn other_help_lines(petscii: bool) -> &'static [&'static str] {
        if petscii {
            &[
                "  A  Groq API key for AI Chat",
                "     (get one free at groq.com)",
                "  B  Default homepage URL for",
                "     the built-in web browser",
                "  W  Weather location (city or",
                "     postal code, worldwide)",
                "  U  Cycle weather units",
                "     (auto / us / metric)",
                "  V  Toggle verbose transfer log",
                "  G  Toggle GUI on startup",
                "     (requires restart)",
                "  D  Toggle gateway debug trace",
                "  E  Toggle CP/M emulator menu",
                "     item (off by default)",
                "  R  Restart the server",
            ]
        } else {
            &[
                "  A  Groq API key for AI Chat (get one",
                "     free at console.groq.com)",
                "  B  Default homepage URL for the",
                "     built-in web browser",
                "  W  Weather location (city or postal code)",
                "  U  Cycle weather units (auto / us / metric)",
                "  V  Toggle verbose transfer logging",
                "  G  Toggle GUI on startup (requires",
                "     a server restart)",
                "  D  Toggle gateway debug trace",
                "  E  Toggle CP/M emulator menu item (off",
                "     by default; runs arbitrary Z80 code)",
                "  R  Restart the server",
            ]
        }
    }

    // ─── SECURITY SETTINGS ───────────────────────────────────

    pub(in crate::telnet) async fn security_settings(&mut self) -> Result<(), std::io::Error> {
        loop {
            let cfg = config::get_config();

            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            self.send_line(&format!("  {}", self.yellow("SECURITY")))
                .await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;

            let login_status = if cfg.security_enabled {
                self.green("ENABLED")
            } else {
                self.red("Disabled")
            };
            self.send_line(&format!("  Require login: {}", login_status))
                .await?;
            self.send_line("").await?;

            // One credential pair now covers telnet, SSH, and the web
            // UI; the earlier per-protocol user/pass lines collapsed
            // into a single Username / Password display.
            self.send_line(&format!(
                "  Username: {}",
                self.amber(&cfg.username)
            ))
            .await?;
            self.send_line(&format!(
                "  Password: {}",
                self.dim("(hidden)")
            ))
            .await?;
            self.send_line("").await?;

            self.send_line(&format!(
                "  {}  Toggle require login",
                self.cyan("L")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Set username",
                self.cyan("U")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Set password",
                self.cyan("P")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Restart server",
                self.cyan("R")
            ))
            .await?;
            self.send_line("").await?;
            self.send_line(&format!(
                "  {}  {}",
                self.action_prompt("Q", "Back"),
                self.action_prompt("H", "Help")
            ))
            .await?;

            let prompt = format!("{}> ", self.cyan("ethernet/config/security"));
            self.send(&prompt).await?;
            self.flush().await?;

            let input = match self.get_menu_input(false).await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(()),
            };

            match input.as_str() {
                "l" => {
                    let new_val = if cfg.security_enabled { "false" } else { "true" };
                    let v = new_val.to_string();
                    tokio::task::spawn_blocking(move || {
                        config::update_config_value("security_enabled", &v);
                    })
                    .await
                    .ok();
                    self.config_restart_notice().await?;
                }
                "u" => {
                    self.security_set_field("Username", "username", &cfg.username, false).await?;
                }
                "p" => {
                    self.security_set_field("Password", "password", &cfg.password, true).await?;
                }
                "r" => {
                    self.config_restart_server().await?;
                }
                "h" => {
                    self.security_show_help().await?;
                }
                "q" => return Ok(()),
                _ => {
                    self.show_error("Press L, U, P, R, H, or Q.").await?;
                }
            }
        }
    }

    pub(in crate::telnet) async fn security_set_field(
        &mut self,
        label: &str,
        key: &str,
        current: &str,
        is_password: bool,
    ) -> Result<(), std::io::Error> {
        self.send_line("").await?;
        if is_password {
            self.send_line(&format!(
                "  Current {}: {}",
                label.to_lowercase(),
                self.dim("(hidden)")
            ))
            .await?;
        } else {
            self.send_line(&format!(
                "  Current {}: {}",
                label.to_lowercase(),
                self.amber(current)
            ))
            .await?;
        }
        self.send(&format!("  New {}: ", label.to_lowercase())).await?;
        self.flush().await?;

        let input = if is_password {
            self.get_password_input().await?
        } else {
            self.get_line_input().await?
        };

        let input = match input {
            Some(s) if !s.is_empty() => s,
            _ => return Ok(()),
        };

        let k = key.to_string();
        let v = input;
        tokio::task::spawn_blocking(move || {
            config::update_config_value(&k, &v);
        })
        .await
        .ok();
        self.config_restart_notice().await?;
        Ok(())
    }

    pub(in crate::telnet) async fn security_show_help(&mut self) -> Result<(), std::io::Error> {
        let lines = Self::security_help_lines(self.terminal_type == TerminalType::Petscii);
        self.show_help_page("SECURITY HELP", lines).await
    }

    /// Login-security settings help, split by terminal width.  Associated fn so a
    /// unit test asserts the REAL lines fit 40 cols (see `punter_help_lines`).
    pub(in crate::telnet) fn security_help_lines(petscii: bool) -> &'static [&'static str] {
        if petscii {
            &[
                "  Configure login security.",
                "",
                "  Menu items:",
                "  L  Toggle login requirement",
                "  U  Set the login username",
                "  P  Set the login password",
                "  R  Restart the server",
                "",
                "  Credentials:",
                "  One username/password covers",
                "  telnet, SSH, and the web UI.",
                "  Stored in plaintext in",
                "  egateway.conf - don't reuse",
                "  sensitive passwords here.",
                "",
                "  When security is OFF:",
                "  Only private-range IPs can",
                "  connect (RFC 1918, loopback,",
                "  link-local, IPv6 unique-local).",
                "  Public IPs are refused, and",
                "  gateway addresses (*.*.*.1)",
                "  are rejected defensively.",
                "",
                "  When security is ON:",
                "  Any IP may connect, but must",
                "  authenticate. 3 failed logins",
                "  from the same IP triggers a",
                "  5-minute lockout for that IP.",
                "",
                "  Telnet transmits credentials",
                "  in cleartext. Use SSH for any",
                "  non-local access.",
                "",
                "  Changes are saved immediately",
                "  but require a server restart.",
            ]
        } else {
            &[
                "  Configure login security.",
                "",
                "  Menu items:",
                "  L  Toggle whether a login is required",
                "  U  Set the login username",
                "  P  Set the login password",
                "  R  Restart the server",
                "",
                "  Credentials:",
                "  One username/password pair covers telnet,",
                "  SSH, and the web configuration UI.  Stored",
                "  in plaintext in egateway.conf - don't reuse",
                "  sensitive passwords on this server.",
                "",
                "  When security is OFF (default):",
                "  Only private-range IPs are allowed to",
                "  connect (RFC 1918 10/172.16/192.168,",
                "  loopback 127.0.0.0/8, link-local",
                "  169.254.0.0/16, IPv6 ::1, fe80::/10,",
                "  and fd00::/8). Public IPs get a refusal",
                "  message, and gateway addresses (those",
                "  ending in .1) are rejected to guard",
                "  against accidental router exposure.",
                "",
                "  When security is ON:",
                "  Any IP may connect but must authenticate.",
                "  After 3 failed login attempts from the",
                "  same IP, that address is locked out for",
                "  5 minutes. Credentials are compared in",
                "  constant time to resist timing attacks.",
                "",
                "  Telnet transmits every byte (including",
                "  the password) in cleartext. For any",
                "  non-local access, use the SSH interface",
                "  instead (Configuration > Server > S).",
                "",
                "  Changes are saved immediately but",
                "  require a server restart to take effect.",
            ]
        }
    }

    // ─── SERVER CONFIGURATION ───────────────────────────────

    pub(in crate::telnet) async fn server_configuration(&mut self) -> Result<(), std::io::Error> {
        loop {
            let cfg = config::get_config();

            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            self.send_line(&format!("  {}", self.yellow("SERVER CONFIGURATION")))
                .await?;
            self.send_line(&sep).await?;

            let telnet_status = if cfg.telnet_enabled {
                self.green("ENABLED")
            } else {
                self.red("Disabled")
            };
            self.send_line(&format!(
                "  Telnet: {} (port {})",
                telnet_status, cfg.telnet_port
            ))
            .await?;
            let ssh_status = if cfg.ssh_enabled {
                self.green("ENABLED")
            } else {
                self.red("Disabled")
            };
            self.send_line(&format!(
                "  SSH:    {} (port {})",
                ssh_status, cfg.ssh_port
            ))
            .await?;
            let kermit_status = if cfg.kermit_server_enabled {
                self.green("ENABLED")
            } else {
                self.red("Disabled")
            };
            self.send_line(&format!(
                "  Kermit: {} (port {})",
                kermit_status, cfg.kermit_server_port
            ))
            .await?;
            let web_status = if cfg.web_enabled {
                self.green("ENABLED")
            } else {
                self.red("Disabled")
            };
            self.send_line(&format!(
                "  Web:    {} (port {})",
                web_status, cfg.web_port
            ))
            .await?;
            let ip_safety_status = if cfg.disable_ip_safety {
                self.red("DISABLED")
            } else {
                self.green("Enabled")
            };
            self.send_line(&format!(
                "  IP safety: {}",
                ip_safety_status
            ))
            .await?;
            self.send_line("").await?;

            // (The "Server addresses:" banner now lives at the top of the
            // CONFIGURATION menu — see render_server_address_block / §4.7.)

            self.send_line(&format!(
                "  {}  Toggle telnet    {}  Set telnet port",
                self.cyan("T"),
                self.cyan("P")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Toggle SSH       {}  Set SSH port",
                self.cyan("S"),
                self.cyan("O")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Toggle Kermit    {}  Set Kermit port",
                self.cyan("K"),
                self.cyan("J")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Toggle Web       {}  Set Web port",
                self.cyan("W"),
                self.cyan("B")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  IP safety        {}  Restart server",
                self.cyan("I"),
                self.cyan("R")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Session cap      {}  Idle timeout",
                self.cyan("C"),
                self.cyan("D")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Master/Slave",
                self.cyan("M")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  {}",
                self.action_prompt("Q", "Back"),
                self.action_prompt("H", "Help")
            ))
            .await?;

            let prompt = format!("{}> ", self.cyan("ethernet/config/server"));
            self.send(&prompt).await?;
            self.flush().await?;

            let input = match self.get_menu_input(false).await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(()),
            };

            match input.as_str() {
                "t" => {
                    let new_val = if cfg.telnet_enabled { "false" } else { "true" };
                    let v = new_val.to_string();
                    tokio::task::spawn_blocking(move || {
                        config::update_config_value("telnet_enabled", &v);
                    })
                    .await
                    .ok();
                    self.config_restart_notice().await?;
                }
                "p" => {
                    self.config_set_port("Telnet", "telnet_port", cfg.telnet_port).await?;
                }
                "s" => {
                    let new_val = if cfg.ssh_enabled { "false" } else { "true" };
                    let v = new_val.to_string();
                    tokio::task::spawn_blocking(move || {
                        config::update_config_value("ssh_enabled", &v);
                    })
                    .await
                    .ok();
                    self.config_restart_notice().await?;
                }
                "o" => {
                    self.config_set_port("SSH", "ssh_port", cfg.ssh_port).await?;
                }
                "k" => {
                    self.kermit_server_toggle(cfg.kermit_server_enabled).await?;
                }
                "j" => {
                    self.config_set_port(
                        "Kermit server",
                        "kermit_server_port",
                        cfg.kermit_server_port,
                    )
                    .await?;
                }
                "w" => {
                    let new_val = if cfg.web_enabled { "false" } else { "true" };
                    let v = new_val.to_string();
                    tokio::task::spawn_blocking(move || {
                        config::update_config_value("web_enabled", &v);
                    })
                    .await
                    .ok();
                    self.config_restart_notice().await?;
                }
                "b" => {
                    self.config_set_port("Web", "web_port", cfg.web_port).await?;
                }
                "i" => {
                    self.disable_ip_safety_toggle(cfg.disable_ip_safety).await?;
                }
                "c" => {
                    self.config_set_count(
                        "session cap",
                        "max_sessions",
                        cfg.max_sessions as u64,
                        1,
                        "New session cap (1 or more)",
                    )
                    .await?;
                }
                "d" => {
                    self.config_set_count(
                        "idle timeout",
                        "idle_timeout_secs",
                        cfg.idle_timeout_secs,
                        0,
                        "New idle timeout in seconds (0 = off)",
                    )
                    .await?;
                }
                "m" => {
                    self.master_slave_config().await?;
                }
                "r" => {
                    self.config_restart_server().await?;
                }
                "h" => {
                    self.config_show_help().await?;
                }
                "q" => return Ok(()),
                _ => {
                    // Keep this short — show_error indents by 2 chars
                    // and PETSCII tops out at 40 cols.  The expanded
                    // "Press T, P, S, O, ..." form blew the limit once
                    // W and B were added, so we now point to the menu.
                    self.show_error("Press a letter from the menu.").await?;
                }
            }
        }
    }

    // ─── MASTER / SLAVE (relay) sub-screen ───────────────────

    /// Master/Slave serial-extender settings (§4.7).  Its own fresh
    /// 22-row budget.  Shows the role and the relevant master/slave
    /// fields, and lets the operator change them.  Role / relay changes
    /// take effect on the next server restart (the relay listener and the
    /// slave client are started at boot from `gateway_role`), so changes
    /// here surface a restart notice.
    pub(in crate::telnet) async fn master_slave_config(&mut self) -> Result<(), std::io::Error> {
        loop {
            let cfg = config::get_config();

            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            self.send_line(&format!("  {}", self.yellow("MASTER / SLAVE")))
                .await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;

            let role_disp = match cfg.gateway_role.as_str() {
                "master" => self.green("MASTER"),
                "slave" => self.cyan("SLAVE"),
                _ => self.dim("STANDALONE"),
            };
            self.send_line(&format!("  Role: {}", role_disp)).await?;

            let is_master = cfg.gateway_role == "master";
            let is_slave = cfg.gateway_role == "slave";

            // Accept-relays applies to a MASTER only; grey it out in the other
            // roles so the operator isn't led to toggle a field that is inert.
            if is_master {
                let accept_disp = if cfg.master_accept_relays {
                    self.green("ENABLED")
                } else {
                    self.red("Disabled")
                };
                self.send_line(&format!("  Accept relays: {}", accept_disp))
                    .await?;
            } else {
                self.send_line(&format!(
                    "  {}",
                    self.dim("Accept relays: (master only)")
                ))
                .await?;
            }

            // Master host/user/pass point this gateway at its master, so they
            // apply to a SLAVE only; grey them out in the other roles.
            if is_slave {
                let host_disp = if cfg.slave_master_host.is_empty() {
                    self.dim("(not set)")
                } else {
                    self.amber(&format!(
                        "{}:{}",
                        cfg.slave_master_host, cfg.slave_master_port
                    ))
                };
                self.send_line(&format!("  Master: {}", host_disp)).await?;
                let user_disp = if cfg.slave_master_username.is_empty() {
                    self.dim("(not set)")
                } else {
                    self.amber(&cfg.slave_master_username)
                };
                self.send_line(&format!("  User:   {}", user_disp)).await?;
                let pass_disp = if cfg.slave_master_password.is_empty() {
                    self.dim("(not set)")
                } else {
                    self.green("(set)")
                };
                self.send_line(&format!("  Pass:   {}", pass_disp)).await?;
            } else {
                self.send_line(&format!(
                    "  {}",
                    self.dim("Master/User/Pass: (slave only)")
                ))
                .await?;
            }
            self.send_line("").await?;

            // Live relay status (§9 #10), read-only.  A master lists the
            // remote console ports slaves have registered right now; a slave
            // shows each console port's link state to the master — so an
            // operator can confirm connectivity without grepping logs.  The
            // Serial Gateway picker remains where a master user actually
            // bridges to a remote port; this is a compact summary (capped to
            // keep the screen inside the 22-row PETSCII budget).
            match cfg.gateway_role.as_str() {
                "master" => {
                    let ports = crate::relay::list_remote_ports();
                    self.send_line(&format!(
                        "  {} ({})",
                        self.dim("Registered remote ports:"),
                        ports.len()
                    ))
                    .await?;
                    const RELAY_STATUS_CAP: usize = 3;
                    for (ip, label) in ports.iter().take(RELAY_STATUS_CAP) {
                        self.send_line(&format!("    {}@{}", self.amber(label), ip))
                            .await?;
                    }
                    if ports.len() > RELAY_STATUS_CAP {
                        self.send_line(&format!(
                            "    {}",
                            self.dim(&format!("+{} more", ports.len() - RELAY_STATUS_CAP))
                        ))
                        .await?;
                    }
                    self.send_line("").await?;
                }
                "slave" => {
                    for id in [
                        crate::config::SerialPortId::A,
                        crate::config::SerialPortId::B,
                    ] {
                        let p = cfg.port(id);
                        if p.enabled && p.mode == "console" {
                            let st = crate::relay::slave_link_state(id.index());
                            self.send_line(&format!(
                                "  Link {}: {}",
                                id.label(),
                                self.amber(st.label())
                            ))
                            .await?;
                        }
                    }
                    self.send_line("").await?;
                }
                _ => {}
            }

            self.send_line(&format!(
                "  {}  Cycle role       {}  Accept relays",
                self.cyan("R"),
                self.cyan("A")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Master host      {}  Master port",
                self.cyan("M"),
                self.cyan("P")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Master user      {}  Master pass",
                self.cyan("U"),
                self.cyan("W")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  {}",
                self.action_prompt("Q", "Back"),
                self.action_prompt("H", "Help")
            ))
            .await?;

            let prompt = format!("{}> ", self.cyan("ethernet/config/relay"));
            self.send(&prompt).await?;
            self.flush().await?;

            let input = match self.get_menu_input(false).await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(()),
            };

            match input.as_str() {
                "r" => {
                    let next = match cfg.gateway_role.as_str() {
                        "standalone" => "master",
                        "master" => "slave",
                        _ => "standalone",
                    };
                    let became_master = next == "master";
                    let v = next.to_string();
                    tokio::task::spawn_blocking(move || {
                        config::update_config_value("gateway_role", &v);
                        // A master with relays off can't accept slaves, so
                        // default the accept-relays gate ON when entering
                        // master (the operator can still turn it off with A).
                        if became_master {
                            config::update_config_value("master_accept_relays", "true");
                        }
                    })
                    .await
                    .ok();
                    // The relay listens on the SSH port, so a master needs the
                    // SSH server enabled. Warn if it's off — never toggle it.
                    if became_master && !config::get_config().ssh_enabled {
                        self.relay_ssh_needed_notice().await?;
                    }
                    self.config_restart_notice().await?;
                }
                "a" => {
                    if cfg.gateway_role != "master" {
                        self.relay_field_not_applicable(
                            "Accept relays: Master role only.",
                        )
                        .await?;
                    } else {
                        let v = (!cfg.master_accept_relays).to_string();
                        tokio::task::spawn_blocking(move || {
                            config::update_config_value("master_accept_relays", &v);
                        })
                        .await
                        .ok();
                        self.config_restart_notice().await?;
                    }
                }
                "m" => {
                    if cfg.gateway_role != "slave" {
                        self.relay_field_not_applicable(
                            "Master settings: Slave role only.",
                        )
                        .await?;
                    } else if self
                        // `other_set_field` only persists on a non-empty entry;
                        // show the restart notice only when it actually changed.
                        .other_set_field(
                            "Master host",
                            "slave_master_host",
                            &cfg.slave_master_host,
                            false,
                        )
                        .await?
                    {
                        self.config_restart_notice().await?;
                    }
                }
                "p" => {
                    if cfg.gateway_role != "slave" {
                        self.relay_field_not_applicable(
                            "Master settings: Slave role only.",
                        )
                        .await?;
                    } else {
                        // config_set_port shows its own restart notice on a
                        // successful change, so this branch must not add one.
                        self.config_set_port(
                            "Master",
                            "slave_master_port",
                            cfg.slave_master_port,
                        )
                        .await?;
                    }
                }
                "u" => {
                    if cfg.gateway_role != "slave" {
                        self.relay_field_not_applicable(
                            "Master settings: Slave role only.",
                        )
                        .await?;
                    } else if self
                        .other_set_field(
                            "Master user",
                            "slave_master_username",
                            &cfg.slave_master_username,
                            false,
                        )
                        .await?
                    {
                        self.config_restart_notice().await?;
                    }
                }
                "w" => {
                    if cfg.gateway_role != "slave" {
                        self.relay_field_not_applicable(
                            "Master settings: Slave role only.",
                        )
                        .await?;
                    } else if self
                        .other_set_field(
                            "Master pass",
                            "slave_master_password",
                            if cfg.slave_master_password.is_empty() {
                                "(not set)"
                            } else {
                                "(set)"
                            },
                            true,
                        )
                        .await?
                    {
                        self.config_restart_notice().await?;
                    }
                }
                "h" => {
                    self.show_help_page(
                        "MASTER / SLAVE HELP",
                        Self::master_slave_help_lines(
                            self.terminal_type == TerminalType::Petscii,
                        ),
                    )
                    .await?;
                }
                "q" => return Ok(()),
                _ => {
                    self.show_error("Press a letter from the menu.").await?;
                }
            }
        }
    }

    /// Help lines for the Master/Slave sub-screen.  Kept in a function so
    /// the help-fit tests can iterate them (see CLAUDE.md testing notes).
    /// One table that fits the 40-col PETSCII budget (so it also fits the
    /// 80-col ANSI budget); `petscii` is accepted for signature parity
    /// with the other `*_help_lines` and the `all_help_line_groups` table.
    pub(in crate::telnet) fn master_slave_help_lines(_petscii: bool) -> &'static [&'static str] {
        &[
            "  Role / relay settings.",
            "",
            "  Standalone: normal gateway.",
            "  Master: accepts slave relays",
            "    (also enable Accept relays).",
            "  Slave: bridges its serial ports",
            "    to the master over SSH.",
            "",
            "  R Cycle role   A Accept relays",
            "  M Host  P Port  U User  W Pass",
            "",
            "  Slave logs in with the master's",
            "  username/password.  Restart to",
            "  apply.",
        ]
    }

    /// Toggle `disable_ip_safety`.  Off→on shows a full-screen security
    /// warning (the listener will accept connections from any source IP,
    /// including public addresses, while `security_enabled` is false)
    /// and prompts Y/N — same posture as `kermit_server_toggle`.  On→off
    /// is one-click safe (re-tightens the allowlist).  Either outcome
    /// falls through and returns to the Server Configuration screen via
    /// the surrounding `loop` in `server_configuration`.  The change is
    /// effective immediately because the accept loop reads the live
    /// config on each connection.
    pub(in crate::telnet) async fn disable_ip_safety_toggle(
        &mut self,
        currently_disabled: bool,
    ) -> Result<(), std::io::Error> {
        if currently_disabled {
            tokio::task::spawn_blocking(move || {
                config::update_config_value("disable_ip_safety", "false");
            })
            .await
            .ok();
            self.send_line("").await?;
            self.send_line(&format!(
                "  {}",
                self.green("IP-safety allowlist re-enabled.")
            ))
            .await?;
            self.send_line("").await?;
            self.send("  Press any key to continue.").await?;
            self.flush().await?;
            self.wait_for_key().await?;
            return Ok(());
        }

        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;
        self.send_line(&format!(
            "  {}",
            self.yellow("DISABLE IP SAFETY — SECURITY WARNING")
        ))
        .await?;
        self.send_line(&sep).await?;
        self.send_line("").await?;
        self.send_line(&format!(
            "  {}",
            self.amber("This removes the private-IP allowlist.")
        ))
        .await?;
        self.send_line("").await?;
        self.send_line(
            "  When Require Login is off, the telnet listener",
        )
        .await?;
        self.send_line(
            "  normally accepts only private/loopback/link-local",
        )
        .await?;
        self.send_line(
            "  addresses, and rejects gateway-style *.*.*.1",
        )
        .await?;
        self.send_line(
            "  addresses. That allowlist is the only thing",
        )
        .await?;
        self.send_line(
            "  standing between a public IP and an unauthenticated",
        )
        .await?;
        self.send_line("  session.").await?;
        self.send_line("").await?;
        self.send_line(
            "  Disabling it accepts every source IP. Anyone who",
        )
        .await?;
        self.send_line(
            "  can reach your telnet port will be able to connect.",
        )
        .await?;
        self.send_line("").await?;
        self.send_line(
            "  Disable only when a firewall, VPN, or other network",
        )
        .await?;
        self.send_line(
            "  control sits in front of the listener, or when you",
        )
        .await?;
        self.send_line(
            "  are about to enable Require Login. The change takes",
        )
        .await?;
        self.send_line(
            "  effect on the next inbound connection.",
        )
        .await?;
        self.send_line("").await?;
        self.send(&format!(
            "  Disable IP safety? ({}/{}): ",
            self.cyan("Y"),
            self.cyan("N")
        ))
        .await?;
        self.flush().await?;

        let answer = self.get_menu_input(false).await?;
        let confirmed = matches!(
            answer.as_deref().map(|s| s.trim()),
            Some("y") | Some("Y")
        );

        self.send_line("").await?;
        if confirmed {
            tokio::task::spawn_blocking(move || {
                config::update_config_value("disable_ip_safety", "true");
            })
            .await
            .ok();
            self.send_line(&format!(
                "  {}",
                self.green("IP-safety allowlist disabled.")
            ))
            .await?;
        } else {
            self.send_line(&format!(
                "  {}",
                self.dim("IP safety left enabled.")
            ))
            .await?;
        }
        self.send_line("").await?;
        self.send("  Press any key to continue.").await?;
        self.flush().await?;
        self.wait_for_key().await?;
        Ok(())
    }

    /// Toggle `kermit_server_enabled`.  Off→on shows a full-screen
    /// security warning (the listener bypasses authentication AND the
    /// private-IP allowlist) and prompts Y/N — same posture as
    /// `kermit_toggle_atdt_kermit`.  On→off is one-click safe.  Either
    /// outcome falls through and returns to the Server Configuration
    /// screen via the surrounding `loop` in `server_configuration`.
    pub(in crate::telnet) async fn kermit_server_toggle(
        &mut self,
        currently_enabled: bool,
    ) -> Result<(), std::io::Error> {
        if currently_enabled {
            tokio::task::spawn_blocking(move || {
                config::update_config_value("kermit_server_enabled", "false");
            })
            .await
            .ok();
            self.send_line("").await?;
            self.send_line(&format!(
                "  {}",
                self.green("Kermit server disabled.")
            ))
            .await?;
            self.config_restart_notice().await?;
            return Ok(());
        }

        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;
        self.send_line(&format!(
            "  {}",
            self.yellow("ENABLE KERMIT SERVER — SECURITY WARNING")
        ))
        .await?;
        self.send_line(&sep).await?;
        self.send_line("").await?;
        self.send_line(&format!(
            "  {}",
            self.amber("This bypasses ALL gateway security.")
        ))
        .await?;
        self.send_line("").await?;
        self.send_line(
            "  When enabled, the gateway opens a dedicated TCP",
        )
        .await?;
        self.send_line(
            "  listener that drops every accepted connection",
        )
        .await?;
        self.send_line(
            "  straight into Kermit server mode — no telnet menu,",
        )
        .await?;
        self.send_line(
            "  no username, no password, no private-IP filter.",
        )
        .await?;
        self.send_line("").await?;
        self.send_line(
            "  Anyone who can reach the listener can read and",
        )
        .await?;
        self.send_line(
            "  write files in your transfer directory.",
        )
        .await?;
        self.send_line("").await?;
        self.send_line(
            "  Enable only when the network path is trusted",
        )
        .await?;
        self.send_line(
            "  (LAN you control, isolated lab, single-user setup).",
        )
        .await?;
        self.send_line("").await?;
        self.send(&format!(
            "  Enable Kermit server? ({}/{}): ",
            self.cyan("Y"),
            self.cyan("N")
        ))
        .await?;
        self.flush().await?;

        let answer = self.get_menu_input(false).await?;
        let confirmed = matches!(
            answer.as_deref().map(|s| s.trim()),
            Some("y") | Some("Y")
        );

        self.send_line("").await?;
        if confirmed {
            tokio::task::spawn_blocking(move || {
                config::update_config_value("kermit_server_enabled", "true");
            })
            .await
            .ok();
            self.send_line(&format!(
                "  {}",
                self.green("Kermit server enabled.")
            ))
            .await?;
            self.config_restart_notice().await?;
        } else {
            self.send_line(&format!(
                "  {}",
                self.dim("Kermit server left disabled.")
            ))
            .await?;
            self.send_line("").await?;
            self.send("  Press any key to continue.").await?;
            self.flush().await?;
            self.wait_for_key().await?;
        }
        Ok(())
    }

    // ─── GATEWAY CONFIGURATION ──────────────────────────────
    //
    // Submenu of Server Configuration.  Edits the two persistent
    // outbound-gateway modes so the user doesn't have to touch the GUI
    // or `egateway.conf` for these settings.  Changes take effect on the
    // next gateway connection — no server restart needed.
    pub(in crate::telnet) async fn gateway_configuration(&mut self) -> Result<(), std::io::Error> {
        loop {
            let cfg = config::get_config();

            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            self.send_line(&format!("  {}", self.yellow("GATEWAY CONFIGURATION")))
                .await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;

            let telnet_mode = if cfg.telnet_gateway_raw {
                self.red("Raw TCP")
            } else {
                self.green("Telnet")
            };
            self.send_line(&format!("  Telnet mode: {}", telnet_mode))
                .await?;
            let coop = if cfg.telnet_gateway_negotiate {
                self.green("On")
            } else {
                self.red("Off")
            };
            self.send_line(&format!("  Cooperative: {}", coop))
                .await?;
            let ssh_auth = if cfg.ssh_gateway_auth == "password" {
                self.yellow("Password")
            } else {
                self.green("Key")
            };
            self.send_line(&format!("  SSH auth:    {}", ssh_auth))
                .await?;
            self.send_line("").await?;

            self.send_line(&format!(
                "  {}  Toggle telnet mode (Telnet/Raw)",
                self.cyan("T")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Toggle cooperative (TTYPE/NAWS)",
                self.cyan("C")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Toggle SSH auth (Key/Password)",
                self.cyan("S")
            ))
            .await?;
            self.send_line("").await?;
            self.send_line(&format!(
                "  {}  {}",
                self.action_prompt("Q", "Back"),
                self.action_prompt("H", "Help")
            ))
            .await?;

            let prompt = format!("{}> ", self.cyan("ethernet/config/server/gateway"));
            self.send(&prompt).await?;
            self.flush().await?;

            let input = match self.get_menu_input(false).await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(()),
            };

            match input.as_str() {
                "t" => {
                    let new_val = if cfg.telnet_gateway_raw { "false" } else { "true" };
                    let v = new_val.to_string();
                    tokio::task::spawn_blocking(move || {
                        config::update_config_value("telnet_gateway_raw", &v);
                    })
                    .await
                    .ok();
                }
                "c" => {
                    let new_val = if cfg.telnet_gateway_negotiate {
                        "false"
                    } else {
                        "true"
                    };
                    let v = new_val.to_string();
                    tokio::task::spawn_blocking(move || {
                        config::update_config_value("telnet_gateway_negotiate", &v);
                    })
                    .await
                    .ok();
                }
                "s" => {
                    let new_val = if cfg.ssh_gateway_auth == "password" {
                        "key"
                    } else {
                        "password"
                    };
                    let v = new_val.to_string();
                    tokio::task::spawn_blocking(move || {
                        config::update_config_value("ssh_gateway_auth", &v);
                    })
                    .await
                    .ok();
                }
                "h" => {
                    self.gateway_config_show_help().await?;
                }
                "q" => return Ok(()),
                _ => {
                    self.show_error("Press T, C, S, H, or Q.").await?;
                }
            }
        }
    }

    pub(in crate::telnet) async fn gateway_config_show_help(&mut self) -> Result<(), std::io::Error> {
        let lines = Self::gateway_config_help_lines(self.terminal_type == TerminalType::Petscii);
        self.show_help_page("GATEWAY CONFIG HELP", lines).await
    }

    /// Telnet/SSH-gateway configuration help, split by terminal width.
    /// Associated fn so a unit test asserts the REAL lines fit 40 cols.
    pub(in crate::telnet) fn gateway_config_help_lines(petscii: bool) -> &'static [&'static str] {
        if petscii {
            &[
                "  Configure the outbound Telnet",
                "  and SSH Gateway menus (the S",
                "  and T main-menu items that",
                "  proxy to remote servers).",
                "",
                "  Telnet mode:",
                "    Telnet - parse IAC option",
                "             negotiation; works",
                "             with real telnet",
                "             servers. Default.",
                "    Raw    - raw TCP byte stream,",
                "             no IAC. Use for MUDs",
                "             and hand-rolled BBS",
                "             software that don't",
                "             speak telnet.",
                "",
                "  Telnet mode options:",
                "    Cooperative - proactively offers",
                "      TTYPE, NAWS, DO ECHO so BBSes",
                "      that wait for the client to",
                "      ask first still get full-",
                "      screen behavior. Enable for",
                "      cooperative telnet servers;",
                "      disable for raw-TCP services.",
                "",
                "  SSH auth:",
                "    Key      - offer the gateway's",
                "               Ed25519 client key.",
                "               Paste the public half",
                "               into the remote's",
                "               ~/.ssh/authorized_keys",
                "               first. Passwordless.",
                "    Password - prompt for the remote",
                "               account's password on",
                "               each connect.",
                "",
                "  Both settings are saved to",
                "  egateway.conf and take effect on",
                "  the next gateway connection.",
                "  No server restart is required.",
            ]
        } else {
            &[
                "  Configure the outbound Telnet and SSH",
                "  Gateway menus (the S and T items on the",
                "  main menu that proxy to remote servers).",
                "",
                "  Telnet mode:",
                "    Telnet  - parse IAC option negotiation",
                "              (default; works with every real",
                "              telnet server). IAC bytes in",
                "              data are escaped as IAC IAC.",
                "    Raw     - raw TCP byte stream, no IAC.",
                "              Use for MUDs and hand-rolled",
                "              BBS software that aren't telnet.",
                "              Bytes pass through unmodified.",
                "",
                "  Cooperative mode (Telnet only):",
                "    When on, the gateway sends WILL TTYPE,",
                "    WILL NAWS, and DO ECHO proactively so",
                "    BBSes that wait for the client to ask",
                "    first still get echo cooperation,",
                "    terminal-type adaptation, and full-screen",
                "    window sizing. Off by default so raw-TCP",
                "    services aren't spammed with IAC bytes",
                "    they can't parse.",
                "",
                "  SSH auth:",
                "    Key      - offer the gateway's Ed25519",
                "               client key. Copy the public",
                "               half (shown under Server >",
                "               More in the GUI) into the",
                "               remote's authorized_keys file.",
                "               Passwordless once installed.",
                "    Password - prompt for the remote account's",
                "               password on each connect. No",
                "               key is offered.",
                "",
                "  Host keys:",
                "    On first dial, the gateway displays the",
                "    remote's SHA-256 fingerprint and asks",
                "    whether to trust it (TOFU). Accepted",
                "    fingerprints are saved to gateway_hosts;",
                "    a changed key triggers a prominent",
                "    HOST KEY CHANGED warning.",
                "",
                "  Changes are saved immediately and take",
                "  effect on the next gateway connection.",
                "  No server restart is required.",
            ]
        }
    }

    pub(in crate::telnet) async fn config_set_port(
        &mut self,
        label: &str,
        key: &str,
        current: u16,
    ) -> Result<(), std::io::Error> {
        self.send_line("").await?;
        self.send_line(&format!(
            "  Current {} port: {}",
            label,
            self.amber(&current.to_string())
        ))
        .await?;
        self.send("  New port (1-65535): ").await?;
        self.flush().await?;

        let input = match self.get_line_input().await? {
            Some(s) if !s.is_empty() => s,
            _ => return Ok(()),
        };

        if let Ok(port) = input.parse::<u16>() {
            if port >= 1 {
                let k = key.to_string();
                let v = port.to_string();
                tokio::task::spawn_blocking(move || {
                    config::update_config_value(&k, &v);
                })
                .await
                .ok();
                self.config_restart_notice().await?;
            } else {
                self.show_error("Invalid port number.").await?;
            }
        } else {
            self.show_error("Invalid port number.").await?;
        }
        Ok(())
    }

    /// Prompt for an integer server setting (session cap / idle timeout)
    /// and persist it.  Shows the current value, reads a line, and accepts
    /// values `>= min` — `min = 1` floors the session cap, `min = 0` lets
    /// the idle timeout be disabled (and renders the current `0` as
    /// "0 (disabled)").  Non-numeric or out-of-range input is rejected.
    /// Like `config_set_port`, the change needs a server restart, so it
    /// ends on the shared restart notice.
    pub(in crate::telnet) async fn config_set_count(
        &mut self,
        label: &str,
        key: &str,
        current: u64,
        min: u64,
        prompt: &str,
    ) -> Result<(), std::io::Error> {
        self.send_line("").await?;
        let shown = if current == 0 && min == 0 {
            "0 (disabled)".to_string()
        } else {
            current.to_string()
        };
        self.send_line(&format!("  Current {}: {}", label, self.amber(&shown)))
            .await?;
        self.send(&format!("  {}: ", prompt)).await?;
        self.flush().await?;

        let input = match self.get_line_input().await? {
            Some(s) if !s.is_empty() => s,
            _ => return Ok(()),
        };

        if let Ok(v) = input.parse::<u64>() {
            if v >= min {
                let k = key.to_string();
                let val = v.to_string();
                tokio::task::spawn_blocking(move || {
                    config::update_config_value(&k, &val);
                })
                .await
                .ok();
                self.config_restart_notice().await?;
            } else {
                self.show_error("Value out of range.").await?;
            }
        } else {
            self.show_error("Enter a whole number.").await?;
        }
        Ok(())
    }

    pub(in crate::telnet) async fn config_restart_notice(&mut self) -> Result<(), std::io::Error> {
        self.send_line("").await?;
        self.send_line(&format!(
            "  {}",
            self.yellow("Restart the server for changes")
        ))
        .await?;
        self.send_line(&format!(
            "  {}",
            self.yellow("to take effect.")
        ))
        .await?;
        self.send_line("").await?;
        self.send("  Press any key to continue.").await?;
        self.flush().await?;
        self.wait_for_key().await?;
        Ok(())
    }

    /// Brief "this field doesn't apply in the current role" notice for the
    /// Master/Slave menu, so a greyed option explains itself instead of
    /// silently doing nothing when its key is pressed.
    pub(in crate::telnet) async fn relay_field_not_applicable(
        &mut self,
        msg: &str,
    ) -> Result<(), std::io::Error> {
        self.send_line("").await?;
        self.send_line(&format!("  {}", self.yellow(msg))).await?;
        self.send_line(&format!("  {}", self.dim("Change Role (R) first.")))
            .await?;
        self.send_line("").await?;
        self.send("  Press any key to continue.").await?;
        self.flush().await?;
        self.wait_for_key().await?;
        Ok(())
    }

    /// Warn (only) that switching to Master needs the SSH server, which is
    /// currently off — the relay listens on the SSH port.  Per the operator's
    /// choice this never toggles SSH; it just points the way.
    pub(in crate::telnet) async fn relay_ssh_needed_notice(&mut self) -> Result<(), std::io::Error> {
        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;
        self.send_line(&format!("  {}", self.yellow("MASTER NEEDS SSH")))
            .await?;
        self.send_line(&sep).await?;
        self.send_line("").await?;
        self.send_line("  Slaves connect to a master over").await?;
        self.send_line("  the SSH server, which is now OFF.").await?;
        self.send_line("").await?;
        self.send_line(&format!(
            "  {}",
            self.dim("Enable SSH in Server settings and")
        ))
        .await?;
        self.send_line(&format!(
            "  {}",
            self.dim("restart, or slaves can't connect.")
        ))
        .await?;
        self.send_line("").await?;
        self.send("  Press any key to continue.").await?;
        self.flush().await?;
        self.wait_for_key().await?;
        Ok(())
    }

    pub(in crate::telnet) async fn config_restart_server(&mut self) -> Result<(), std::io::Error> {
        self.send_line("").await?;
        self.send_line(&format!(
            "  {}",
            self.red("WARNING: All active sessions")
        ))
        .await?;
        self.send_line(&format!(
            "  {}",
            self.red("will be disconnected.")
        ))
        .await?;
        self.send_line("").await?;
        self.send("  Restart the server? (Y/N) ").await?;
        self.flush().await?;

        let input = match self.get_menu_input(false).await? {
            Some(s) if !s.is_empty() => s,
            _ => return Ok(()),
        };

        if input == "y" {
            self.send_line("").await?;
            self.send_line(&format!(
                "  {}",
                self.yellow("Restarting server...")
            ))
            .await?;
            self.flush().await?;
            self.restart.store(true, Ordering::SeqCst);
            self.shutdown.store(true, Ordering::SeqCst);
        }
        Ok(())
    }

    pub(in crate::telnet) async fn config_reset_defaults(&mut self) -> Result<(), std::io::Error> {
        self.send_line("").await?;
        self.send_line(&format!(
            "  {}",
            self.red("WARNING: This will reset ALL")
        ))
        .await?;
        self.send_line(&format!(
            "  {}",
            self.red("settings to factory defaults.")
        ))
        .await?;
        self.send_line(&format!(
            "  {}",
            self.red("The API key will be cleared.")
        ))
        .await?;
        self.send_line("").await?;
        self.send("  Reset all settings? (Y/N) ").await?;
        self.flush().await?;

        let input = match self.get_menu_input(false).await? {
            Some(s) if !s.is_empty() => s,
            _ => return Ok(()),
        };

        if input == "y" {
            let defaults = config::Config::default();
            let saved = tokio::task::spawn_blocking(move || config::save_config(&defaults))
                .await
                .unwrap_or_else(|e| Err(format!("save task panicked: {e}")));
            self.send_line("").await?;
            match saved {
                Ok(()) => {
                    self.send_line(&format!(
                        "  {}",
                        self.green("All settings reset to defaults.")
                    ))
                    .await?;
                }
                Err(e) => {
                    self.send_line(&format!(
                        "  {}",
                        self.amber(&format!("Reset applied in memory but NOT saved: {}", e))
                    ))
                    .await?;
                }
            }
            self.config_restart_notice().await?;
        }
        Ok(())
    }

    pub(in crate::telnet) async fn config_show_help(&mut self) -> Result<(), std::io::Error> {
        let lines = Self::config_help_lines(self.terminal_type == TerminalType::Petscii);
        self.show_help_page("SERVER CONFIGURATION HELP", lines).await
    }

    /// Server-configuration settings help, split by terminal width.  Associated
    /// fn so a unit test asserts the REAL lines fit 40 cols (see `punter_help_lines`).
    pub(in crate::telnet) fn config_help_lines(petscii: bool) -> &'static [&'static str] {
        if petscii {
            &[
                "  Change settings for THIS server.",
                "",
                "  T  Enable or disable the telnet",
                "     server listener",
                "  P  Change the telnet port",
                "  S  Enable or disable the SSH",
                "     server listener",
                "  O  Change the SSH port",
                "  K  Toggle the standalone Kermit",
                "     server listener (bypasses",
                "     auth and private-IP filter)",
                "  J  Change the Kermit server port",
                "  W  Toggle the configuration web",
                "     server (HTTP/Basic auth on",
                "     the same login as telnet)",
                "  B  Change the web server port",
                "  I  Toggle IP safety. When OFF,",
                "     telnet accepts every source",
                "     IP (no private-IP filter).",
                "     Effective immediately.",
                "  R  Restart the server",
                "  C  Set the max concurrent",
                "     sessions (1 or more)",
                "  D  Set the idle-disconnect",
                "     timeout in seconds; 0 keeps",
                "     sessions open indefinitely",
                "  M  Master/Slave settings (relay",
                "     serial ports to/from another",
                "     gateway over SSH)",
                "",
                "  Most changes are saved at once",
                "  but require a server restart;",
                "  IP safety applies immediately.",
            ]
        } else {
            &[
                "  Change settings for THIS server.",
                "",
                "  T  Enable or disable the telnet server",
                "  P  Change the telnet listening port",
                "  S  Enable or disable the SSH server",
                "  O  Change the SSH listening port",
                "  K  Toggle the standalone Kermit server",
                "     (bypasses auth and the private-IP filter)",
                "  J  Change the Kermit server listening port",
                "  W  Toggle the configuration web server.  Renders",
                "     the same settings page the GUI does in a",
                "     browser; uses the unified login credentials",
                "     under Security when login is required.",
                "  B  Change the web server listening port",
                "  I  Toggle IP safety. When ON (default), and",
                "     login is not required, the telnet listener",
                "     only accepts private/loopback addresses",
                "     and rejects *.*.*.1 gateways. When OFF, every",
                "     source IP is accepted. Takes effect on the",
                "     next inbound connection (no restart needed).",
                "  R  Restart the server now",
                "  C  Set the maximum number of concurrent sessions",
                "     (1 or more)",
                "  D  Set the idle-disconnect timeout in seconds; 0",
                "     keeps idle sessions connected indefinitely",
                "  M  Master/Slave settings (relay serial ports",
                "     to/from another gateway over SSH)",
                "",
                "  Most changes are saved to the config file",
                "  immediately but require a server restart to",
                "  take effect; IP safety is the exception and",
                "  applies on the next connection.",
            ]
        }
    }

    // ─── FILE TRANSFER SETTINGS ─────────────────────────────
    //
    // Top-level submenu under Configuration > File Transfer.  Holds
    // the shared transfer-directory setting plus a per-protocol
    // selector that drills into XMODEM / YMODEM / ZMODEM settings
    // pages.  Each protocol page edits only the keys that apply to
    // that protocol; XMODEM and YMODEM share the `xmodem_*` keys
    // because they share a single protocol code path in `xmodem.rs`.

    pub(in crate::telnet) async fn file_transfer_settings(&mut self) -> Result<(), std::io::Error> {
        loop {
            let cfg = config::get_config();

            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            self.send_line(&format!("  {}", self.yellow("FILE TRANSFER")))
                .await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;

            self.send_line(&format!(
                "  Transfer dir:  {}",
                self.amber(&cfg.transfer_dir)
            ))
            .await?;
            self.send_line("").await?;

            self.send_line(&format!(
                "  {}  Change transfer directory",
                self.cyan("D")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  XMODEM settings",
                self.cyan("X")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  YMODEM settings",
                self.cyan("Y")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  ZMODEM settings",
                self.cyan("Z")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  KERMIT settings",
                self.cyan("K")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  PUNTER settings",
                self.cyan("P")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Restart server",
                self.cyan("R")
            ))
            .await?;
            self.send_line("").await?;
            self.send_line(&format!(
                "  {}  {}",
                self.action_prompt("Q", "Back"),
                self.action_prompt("H", "Help")
            ))
            .await?;

            let prompt = format!("{}> ", self.cyan("ethernet/config/xfer"));
            self.send(&prompt).await?;
            self.flush().await?;

            let input = match self.get_menu_input(false).await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(()),
            };

            match input.as_str() {
                "d" => {
                    self.xmodem_set_dir(&cfg.transfer_dir).await?;
                }
                "x" => {
                    self.xmodem_settings().await?;
                }
                "y" => {
                    self.ymodem_settings().await?;
                }
                "z" => {
                    self.zmodem_settings().await?;
                }
                "k" => {
                    self.kermit_settings().await?;
                }
                "p" => {
                    self.punter_settings().await?;
                }
                "r" => {
                    self.config_restart_server().await?;
                }
                "h" => {
                    self.file_transfer_show_help().await?;
                }
                "q" => return Ok(()),
                _ => {
                    self.show_error("Press D, X, Y, Z, K, P, R, H, or Q.").await?;
                }
            }
        }
    }

    pub(in crate::telnet) async fn file_transfer_show_help(&mut self) -> Result<(), std::io::Error> {
        let lines = Self::file_transfer_help_lines(self.terminal_type == TerminalType::Petscii);
        self.show_help_page("FILE TRANSFER HELP", lines).await
    }

    /// File-transfer settings help, split by terminal width.  Associated fn so a
    /// unit test asserts the REAL lines fit 40 cols (see `punter_help_lines`).
    pub(in crate::telnet) fn file_transfer_help_lines(petscii: bool) -> &'static [&'static str] {
        if petscii {
            &[
                "  Configure file-transfer options.",
                "",
                "  D  Transfer directory: where",
                "     uploads land and downloads",
                "     are served from",
                "  X  XMODEM settings",
                "  Y  YMODEM settings",
                "  Z  ZMODEM settings",
                "  K  KERMIT settings",
                "  P  PUNTER settings",
                "  R  Restart the server",
                "",
                "  XMODEM, XMODEM-1K, and YMODEM",
                "  share the same timeouts.",
                "  ZMODEM, Kermit, and Punter",
                "  each have their own.",
            ]
        } else {
            &[
                "  Configure file-transfer options.",
                "",
                "  D  Transfer directory: where uploads",
                "     land and downloads are served from",
                "  X  XMODEM settings (XMODEM + XMODEM-1K)",
                "  Y  YMODEM settings (shared with XMODEM)",
                "  Z  ZMODEM settings",
                "  K  KERMIT settings",
                "  P  PUNTER settings",
                "  R  Restart the server",
                "",
                "  XMODEM, XMODEM-1K, and YMODEM share",
                "  the same timeouts because they share",
                "  the same protocol code path. ZMODEM,",
                "  Kermit, and Punter each have their own",
                "  independent tunables.",
            ]
        }
    }

    // ─── XMODEM SETTINGS ────────────────────────────────────
    //
    // These settings also govern XMODEM-1K and YMODEM because all
    // three protocols share the same `xmodem_*` config keys and the
    // same send/receive code path in `xmodem.rs`.

    pub(in crate::telnet) async fn xmodem_settings(&mut self) -> Result<(), std::io::Error> {
        self.xmodem_family_settings(
            "XMODEM SETTINGS",
            "ethernet/config/xfer/xmodem",
            "XMODEM family",
        )
        .await
    }

    pub(in crate::telnet) async fn ymodem_settings(&mut self) -> Result<(), std::io::Error> {
        self.xmodem_family_settings(
            "YMODEM SETTINGS",
            "ethernet/config/xfer/ymodem",
            "XMODEM family (shared)",
        )
        .await
    }

    /// Shared renderer for the XMODEM / YMODEM settings pages.  Both
    /// protocols edit the same `xmodem_*` config keys, so the page
    /// differs only in its heading and breadcrumb.  A note under the
    /// status block calls out the shared-family behavior so operators
    /// aren't surprised when editing either page changes the other.
    pub(in crate::telnet) async fn xmodem_family_settings(
        &mut self,
        header: &str,
        breadcrumb: &str,
        applies_to: &str,
    ) -> Result<(), std::io::Error> {
        loop {
            let cfg = config::get_config();

            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            self.send_line(&format!("  {}", self.yellow(header))).await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;

            self.send_line(&format!(
                "  Negotiate:      {} s",
                self.amber(&cfg.xmodem_negotiation_timeout.to_string())
            ))
            .await?;
            self.send_line(&format!(
                "  Retry interval: {} s",
                self.amber(&cfg.xmodem_negotiation_retry_interval.to_string())
            ))
            .await?;
            self.send_line(&format!(
                "  Block timeout:  {} s",
                self.amber(&cfg.xmodem_block_timeout.to_string())
            ))
            .await?;
            self.send_line(&format!(
                "  Max retries:    {}",
                self.amber(&cfg.xmodem_max_retries.to_string())
            ))
            .await?;
            self.send_line(&format!("  Applies to:     {}", self.dim(applies_to)))
                .await?;
            self.send_line("").await?;

            self.send_line(&format!(
                "  {}  Set negotiation timeout",
                self.cyan("N")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Set retry interval",
                self.cyan("I")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Set block timeout",
                self.cyan("B")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Set max retries",
                self.cyan("M")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Restart server",
                self.cyan("R")
            ))
            .await?;
            self.send_line("").await?;
            self.send_line(&format!(
                "  {}  {}",
                self.action_prompt("Q", "Back"),
                self.action_prompt("H", "Help")
            ))
            .await?;

            let prompt = format!("{}> ", self.cyan(breadcrumb));
            self.send(&prompt).await?;
            self.flush().await?;

            let input = match self.get_menu_input(false).await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(()),
            };

            match input.as_str() {
                "n" => {
                    self.xmodem_set_numeric(
                        "Negotiation timeout",
                        "xmodem_negotiation_timeout",
                        cfg.xmodem_negotiation_timeout,
                        1,
                        300,
                        "seconds",
                    )
                    .await?;
                }
                "i" => {
                    self.xmodem_set_numeric(
                        "Retry interval",
                        "xmodem_negotiation_retry_interval",
                        cfg.xmodem_negotiation_retry_interval,
                        1,
                        60,
                        "seconds",
                    )
                    .await?;
                }
                "b" => {
                    self.xmodem_set_numeric(
                        "Block timeout",
                        "xmodem_block_timeout",
                        cfg.xmodem_block_timeout,
                        1,
                        120,
                        "seconds",
                    )
                    .await?;
                }
                "m" => {
                    self.xmodem_set_numeric(
                        "Max retries",
                        "xmodem_max_retries",
                        cfg.xmodem_max_retries as u64,
                        1,
                        100,
                        "retries",
                    )
                    .await?;
                }
                "r" => {
                    self.config_restart_server().await?;
                }
                "h" => {
                    self.xmodem_show_help().await?;
                }
                "q" => return Ok(()),
                _ => {
                    self.show_error("Press N, I, B, M, R, H, or Q.").await?;
                }
            }
        }
    }

    // ─── ZMODEM SETTINGS ────────────────────────────────────

    pub(in crate::telnet) async fn zmodem_settings(&mut self) -> Result<(), std::io::Error> {
        loop {
            let cfg = config::get_config();

            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            self.send_line(&format!("  {}", self.yellow("ZMODEM SETTINGS")))
                .await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;

            self.send_line(&format!(
                "  Negotiate:      {} s",
                self.amber(&cfg.zmodem_negotiation_timeout.to_string())
            ))
            .await?;
            self.send_line(&format!(
                "  Retry interval: {} s",
                self.amber(&cfg.zmodem_negotiation_retry_interval.to_string())
            ))
            .await?;
            self.send_line(&format!(
                "  Frame timeout:  {} s",
                self.amber(&cfg.zmodem_frame_timeout.to_string())
            ))
            .await?;
            self.send_line(&format!(
                "  Max retries:    {}",
                self.amber(&cfg.zmodem_max_retries.to_string())
            ))
            .await?;
            self.send_line("").await?;

            self.send_line(&format!(
                "  {}  Set negotiation timeout",
                self.cyan("N")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Set retry interval",
                self.cyan("I")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Set frame timeout",
                self.cyan("F")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Set max retries",
                self.cyan("M")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Restart server",
                self.cyan("R")
            ))
            .await?;
            self.send_line("").await?;
            self.send_line(&format!(
                "  {}  {}",
                self.action_prompt("Q", "Back"),
                self.action_prompt("H", "Help")
            ))
            .await?;

            let prompt = format!("{}> ", self.cyan("ethernet/config/xfer/zmodem"));
            self.send(&prompt).await?;
            self.flush().await?;

            let input = match self.get_menu_input(false).await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(()),
            };

            match input.as_str() {
                "n" => {
                    self.xmodem_set_numeric(
                        "Negotiation timeout",
                        "zmodem_negotiation_timeout",
                        cfg.zmodem_negotiation_timeout,
                        1,
                        300,
                        "seconds",
                    )
                    .await?;
                }
                "i" => {
                    self.xmodem_set_numeric(
                        "Retry interval",
                        "zmodem_negotiation_retry_interval",
                        cfg.zmodem_negotiation_retry_interval,
                        1,
                        60,
                        "seconds",
                    )
                    .await?;
                }
                "f" => {
                    self.xmodem_set_numeric(
                        "Frame timeout",
                        "zmodem_frame_timeout",
                        cfg.zmodem_frame_timeout,
                        1,
                        120,
                        "seconds",
                    )
                    .await?;
                }
                "m" => {
                    self.xmodem_set_numeric(
                        "Max retries",
                        "zmodem_max_retries",
                        cfg.zmodem_max_retries as u64,
                        1,
                        100,
                        "retries",
                    )
                    .await?;
                }
                "r" => {
                    self.config_restart_server().await?;
                }
                "h" => {
                    self.zmodem_show_help().await?;
                }
                "q" => return Ok(()),
                _ => {
                    self.show_error("Press N, I, F, M, R, H, or Q.").await?;
                }
            }
        }
    }

    pub(in crate::telnet) async fn zmodem_show_help(&mut self) -> Result<(), std::io::Error> {
        let lines = Self::zmodem_help_lines(self.terminal_type == TerminalType::Petscii);
        self.show_help_page("ZMODEM SETTINGS HELP", lines).await
    }

    /// ZMODEM settings help, split by terminal width.  Associated fn so a unit
    /// test asserts the REAL lines fit 40 columns (see `punter_help_lines`).
    pub(in crate::telnet) fn zmodem_help_lines(petscii: bool) -> &'static [&'static str] {
        if petscii {
            &[
                "  Configure ZMODEM file transfer",
                "  settings.",
                "",
                "  N  Negotiation timeout: how",
                "     long to wait for ZRQINIT /",
                "     ZRINIT handshake",
                "  I  Retry interval: ZRINIT/",
                "     ZRQINIT re-send gap (def 5)",
                "  F  Frame timeout: per-frame",
                "     read timeout in transfer",
                "  M  Max retries for ZRQINIT /",
                "     ZRPOS / ZDATA frames",
                "  R  Restart the server",
                "",
                "  Takes effect on next transfer.",
            ]
        } else {
            &[
                "  Configure ZMODEM file transfer",
                "  settings.",
                "",
                "  N  Negotiation timeout: how long to",
                "     wait for the ZRQINIT / ZRINIT",
                "     handshake",
                "  I  Retry interval: seconds between",
                "     ZRINIT / ZRQINIT re-sends (def 5)",
                "  F  Frame timeout: per-frame read",
                "     timeout once a transfer is live",
                "  M  Max retries: retry cap for ZRQINIT,",
                "     ZRPOS, and ZDATA frames",
                "  R  Restart the server",
                "",
                "  Takes effect on next transfer.",
            ]
        }
    }

    // ─── PUNTER SETTINGS ────────────────────────────────────

    pub(in crate::telnet) async fn punter_settings(&mut self) -> Result<(), std::io::Error> {
        loop {
            let cfg = config::get_config();

            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            self.send_line(&format!("  {}", self.yellow("PUNTER SETTINGS")))
                .await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;

            self.send_line(&format!(
                "  Block size:     {} bytes",
                self.amber(&cfg.punter_block_size.to_string())
            ))
            .await?;
            self.send_line(&format!(
                "  Negotiate:      {} s",
                self.amber(&cfg.punter_negotiation_timeout.to_string())
            ))
            .await?;
            self.send_line(&format!(
                "  Retry interval: {} s",
                self.amber(&cfg.punter_negotiation_retry_interval.to_string())
            ))
            .await?;
            self.send_line(&format!(
                "  Block timeout:  {} s",
                self.amber(&cfg.punter_block_timeout.to_string())
            ))
            .await?;
            self.send_line(&format!(
                "  Max retries:    {}",
                self.amber(&cfg.punter_max_retries.to_string())
            ))
            .await?;
            self.send_line(&format!(
                "  Bad-blk limit:  {} rounds",
                self.amber(&cfg.punter_max_bad_rounds.to_string())
            ))
            .await?;
            self.send_line("").await?;

            self.send_line(&format!(
                "  {}  Set block size",
                self.cyan("B")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Set negotiation timeout",
                self.cyan("N")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Set retry interval",
                self.cyan("I")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Set block timeout",
                self.cyan("F")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Set max retries",
                self.cyan("M")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Set bad-block limit",
                self.cyan("G")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Hangup on fail: {}",
                self.cyan("D"),
                self.amber(if cfg.punter_hangup_on_failure { "on" } else { "off" })
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Restart server",
                self.cyan("R")
            ))
            .await?;
            self.send_line("").await?;
            self.send_line(&format!(
                "  {}  {}",
                self.action_prompt("Q", "Back"),
                self.action_prompt("H", "Help")
            ))
            .await?;

            let prompt = format!("{}> ", self.cyan("ethernet/config/xfer/punter"));
            self.send(&prompt).await?;
            self.flush().await?;

            let input = match self.get_menu_input(false).await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(()),
            };

            match input.as_str() {
                "b" => {
                    self.xmodem_set_numeric(
                        "Block size",
                        "punter_block_size",
                        cfg.punter_block_size as u64,
                        8,
                        255,
                        "bytes",
                    )
                    .await?;
                }
                "n" => {
                    self.xmodem_set_numeric(
                        "Negotiation timeout",
                        "punter_negotiation_timeout",
                        cfg.punter_negotiation_timeout,
                        1,
                        300,
                        "seconds",
                    )
                    .await?;
                }
                "i" => {
                    self.xmodem_set_numeric(
                        "Retry interval",
                        "punter_negotiation_retry_interval",
                        cfg.punter_negotiation_retry_interval,
                        1,
                        60,
                        "seconds",
                    )
                    .await?;
                }
                "f" => {
                    self.xmodem_set_numeric(
                        "Block timeout",
                        "punter_block_timeout",
                        cfg.punter_block_timeout,
                        1,
                        120,
                        "seconds",
                    )
                    .await?;
                }
                "m" => {
                    self.xmodem_set_numeric(
                        "Max retries",
                        "punter_max_retries",
                        cfg.punter_max_retries as u64,
                        1,
                        100,
                        "retries",
                    )
                    .await?;
                }
                "g" => {
                    self.xmodem_set_numeric(
                        "Bad-block limit",
                        "punter_max_bad_rounds",
                        cfg.punter_max_bad_rounds as u64,
                        1,
                        1000,
                        "rounds",
                    )
                    .await?;
                }
                "d" => {
                    // Shared generic bool-toggle helper (despite the name).
                    self.kermit_toggle_bool(
                        "Hangup on failure",
                        "punter_hangup_on_failure",
                        cfg.punter_hangup_on_failure,
                    )
                    .await?;
                }
                "r" => {
                    self.config_restart_server().await?;
                }
                "h" => {
                    self.punter_show_help().await?;
                }
                "q" => return Ok(()),
                _ => {
                    self.show_error("Press B, N, I, F, M, G, D, R, H, or Q.").await?;
                }
            }
        }
    }

    pub(in crate::telnet) async fn punter_show_help(&mut self) -> Result<(), std::io::Error> {
        let lines = Self::punter_help_lines(self.terminal_type == TerminalType::Petscii);
        self.show_help_page("PUNTER SETTINGS HELP", lines).await
    }

    /// Punter settings help text, split by terminal width.  An associated fn
    /// (no `self`) so a unit test can assert the PETSCII variant fits 40
    /// columns against the real lines — no duplicated copy to drift from.
    pub(in crate::telnet) fn punter_help_lines(petscii: bool) -> &'static [&'static str] {
        if petscii {
            &[
                "  Configure PUNTER (C1) file",
                "  transfer settings.  C1 is the",
                "  protocol CCGMS and Novaterm",
                "  speak on Commodore BBSes.",
                "",
                "  B  Block size in bytes (8-255).",
                "     255 = native max; lower for",
                "     noisy lines (40 floor)",
                "  N  Negotiation timeout: wait for",
                "     the peer's first code",
                "  I  Retry interval: code re-send",
                "     gap during negotiation",
                "  F  Block timeout: per-block read",
                "     timeout in transfer",
                "  M  Max retries per code / block",
                "  G  Bad-block limit: how many",
                "     corrupt-block resends before",
                "     giving up (vs M, per-code)",
                "  D  Hang up on failure: drop",
                "     carrier so a stranded C64",
                "     exits (C1 has no abort)",
                "  R  Restart the server",
                "",
                "  Takes effect on next transfer.",
            ]
        } else {
            &[
                "  Configure PUNTER (C1) file transfer",
                "  settings.  C1 is the protocol CCGMS /",
                "  Novaterm speak on Commodore BBSes.",
                "",
                "  B  Block size in bytes (8-255). 255 is",
                "     the native max; lower it toward 40",
                "     for noisy lines",
                "  N  Negotiation timeout: how long to",
                "     wait for the peer's first code",
                "  I  Retry interval: seconds between",
                "     handshake-code re-sends",
                "  F  Block timeout: per-block read",
                "     timeout once a transfer is live",
                "  M  Max retries: retry cap per code / block",
                "  G  Bad-block limit: consecutive corrupt-block",
                "     resends tolerated before giving up (kept higher",
                "     than M; a real C64 peer never caps these, so a",
                "     low value makes the gateway quit and strand it)",
                "  D  Hang up on failure: drop carrier when a transfer",
                "     gives up so a stranded C64 exits (C1 has no",
                "     in-band abort). Ends the whole session.",
                "  R  Restart the server",
                "",
                "  Takes effect on next transfer.",
            ]
        }
    }

    // ─── KERMIT SETTINGS ────────────────────────────────────
    //
    // Kermit has the largest configuration surface of any of the
    // file-transfer protocols.  We split it across three pages of
    // status (timeouts/retries, packet/window/check, capability bits)
    // since not all of it fits in PETSCII's 22 rows.

    /// Top-level Kermit settings entry point.  The screen is split into
    /// two pages so each fits within the PETSCII 22-row × 40-col budget:
    /// a read-only Status page and an editable Settings menu.  `M` on
    /// the Status page jumps to Settings; `V` on the Settings menu jumps
    /// back to Status; `Q` on either exits to File Transfer.
    pub(in crate::telnet) async fn kermit_settings(&mut self) -> Result<(), std::io::Error> {
        let mut on_status = true;
        loop {
            let nav = if on_status {
                self.kermit_status_page().await?
            } else {
                self.kermit_settings_menu_page().await?
            };
            match nav {
                KermitPageNav::Switch => on_status = !on_status,
                KermitPageNav::Back => return Ok(()),
            }
        }
    }

    /// Render the read-only Kermit status page.  Returns `Switch` when
    /// the operator presses `M` (jump to the editable Settings menu),
    /// `Back` on `Q`.  `H` shows help and re-renders.  Designed to fit
    /// PETSCII 22×40 with all values at their realistic max widths
    /// (5-digit timeouts, 4-digit max-packet, 2-digit window, etc.).
    pub(in crate::telnet) async fn kermit_status_page(&mut self) -> Result<KermitPageNav, std::io::Error> {
        loop {
            let cfg = config::get_config();
            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            self.send_line(&format!("  {}", self.yellow("KERMIT STATUS")))
                .await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;

            let idle_display = if cfg.kermit_idle_timeout == 0 {
                "off".to_string()
            } else {
                format!("{} s", cfg.kermit_idle_timeout)
            };
            self.send_line(&format!(
                "  Negotiate: {} s",
                self.amber(&cfg.kermit_negotiation_timeout.to_string())
            ))
            .await?;
            self.send_line(&format!(
                "  Packet: {} s    Retries: {}",
                self.amber(&cfg.kermit_packet_timeout.to_string()),
                self.amber(&cfg.kermit_max_retries.to_string()),
            ))
            .await?;
            self.send_line(&format!(
                "  Idle: {}",
                self.amber(&idle_display)
            ))
            .await?;
            self.send_line(&format!(
                "  Max packet: {}   Window: {}",
                self.amber(&cfg.kermit_max_packet_length.to_string()),
                self.amber(&cfg.kermit_window_size.to_string()),
            ))
            .await?;
            self.send_line(&format!(
                "  Block check: {}    Long: {}",
                self.amber(&cfg.kermit_block_check_type.to_string()),
                self.amber(if cfg.kermit_long_packets { "on" } else { "off" }),
            ))
            .await?;
            self.send_line(&format!(
                "  Sliding: {}    Streaming: {}",
                self.amber(if cfg.kermit_sliding_windows { "on" } else { "off" }),
                self.amber(if cfg.kermit_streaming { "on" } else { "off" }),
            ))
            .await?;
            self.send_line(&format!(
                "  Attributes: {}    Repeat: {}",
                self.amber(if cfg.kermit_attribute_packets { "on" } else { "off" }),
                self.amber(if cfg.kermit_repeat_compression { "on" } else { "off" }),
            ))
            .await?;
            self.send_line(&format!(
                "  8-bit quote: {}",
                self.amber(&cfg.kermit_8bit_quote)
            ))
            .await?;
            self.send_line(&format!(
                "  Locking: {}    Resume: {}",
                self.amber(if cfg.kermit_locking_shifts { "on" } else { "off" }),
                self.amber(if cfg.kermit_resume_partial { "on" } else { "off" }),
            ))
            .await?;
            self.send_line(&format!(
                "  Resume age: {} h",
                self.amber(&cfg.kermit_resume_max_age_hours.to_string())
            ))
            .await?;
            self.send_line(&format!(
                "  Wait for rx: {}",
                self.amber(if cfg.kermit_wait_for_receiver { "on" } else { "off" }),
            ))
            .await?;
            self.send_line(&format!(
                "  ATDT KERMIT: {}",
                self.amber(if cfg.allow_atdt_kermit { "enabled" } else { "disabled" })
            ))
            .await?;
            self.send_line("").await?;
            self.send_line(&format!(
                "  {}  Settings   {}  {}",
                self.cyan("M"),
                self.action_prompt("Q", "Back"),
                self.action_prompt("H", "Help")
            ))
            .await?;

            let prompt = format!("{}> ", self.cyan("ethernet/config/xfer/kermit"));
            self.send(&prompt).await?;
            self.flush().await?;

            let input = match self.get_menu_input(false).await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(KermitPageNav::Back),
            };

            match input.as_str() {
                "m" => return Ok(KermitPageNav::Switch),
                "q" => return Ok(KermitPageNav::Back),
                "h" => self.kermit_show_help().await?,
                _ => self.show_error("Press M, Q, or H.").await?,
            }
        }
    }

    /// Render the editable Kermit settings menu.  Returns `Switch` when
    /// the operator presses `V` (jump back to the Status page), `Back`
    /// on `Q`.  Action keys (N/P/X/M/W/C/L/S/T/A/E/I/8/R/K) dispatch to
    /// the same setters the original combined screen used.  Labels are
    /// abbreviated to fit PETSCII 40-col with the standard column-22
    /// two-keys-per-row alignment.
    pub(in crate::telnet) async fn kermit_settings_menu_page(
        &mut self,
    ) -> Result<KermitPageNav, std::io::Error> {
        loop {
            let cfg = config::get_config();
            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            self.send_line(&format!("  {}", self.yellow("KERMIT SETTINGS")))
                .await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;

            self.send_line(&format!(
                "  {}  Negotiate        {}  Packet timeout",
                self.cyan("N"),
                self.cyan("P")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Max retries      {}  Max length",
                self.cyan("X"),
                self.cyan("M")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Window size      {}  Block check",
                self.cyan("W"),
                self.cyan("C")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Long packets     {}  Sliding wins",
                self.cyan("L"),
                self.cyan("S")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Streaming        {}  Attributes",
                self.cyan("T"),
                self.cyan("A")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Repeat compr     {}  Idle timeout",
                self.cyan("E"),
                self.cyan("I"),
            ))
            .await?;
            self.send_line(&format!(
                "  {}  8-bit quote      {}  Restart server",
                self.cyan("8"),
                self.cyan("R")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Locking shifts   {}  Resume uploads",
                self.cyan("F"),
                self.cyan("U")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Resume max age   {}  Toggle ATDT KERMIT",
                self.cyan("D"),
                self.cyan("K"),
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Wait for rx",
                self.cyan("G"),
            ))
            .await?;
            self.send_line("").await?;
            self.send_line(&format!(
                "  {}  Status   {}  {}",
                self.cyan("V"),
                self.action_prompt("Q", "Back"),
                self.action_prompt("H", "Help")
            ))
            .await?;

            let prompt = format!("{}> ", self.cyan("ethernet/config/xfer/kermit"));
            self.send(&prompt).await?;
            self.flush().await?;

            let input = match self.get_menu_input(false).await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(KermitPageNav::Back),
            };

            match input.as_str() {
                "v" => return Ok(KermitPageNav::Switch),
                "q" => return Ok(KermitPageNav::Back),
                "n" => {
                    self.xmodem_set_numeric(
                        "Negotiation timeout",
                        "kermit_negotiation_timeout",
                        cfg.kermit_negotiation_timeout,
                        1,
                        300,
                        "seconds",
                    )
                    .await?;
                }
                "p" => {
                    self.xmodem_set_numeric(
                        "Packet timeout",
                        "kermit_packet_timeout",
                        cfg.kermit_packet_timeout,
                        1,
                        120,
                        "seconds",
                    )
                    .await?;
                }
                "x" => {
                    self.xmodem_set_numeric(
                        "Max retries",
                        "kermit_max_retries",
                        cfg.kermit_max_retries as u64,
                        1,
                        20,
                        "retries",
                    )
                    .await?;
                }
                "m" => {
                    self.xmodem_set_numeric(
                        "Max packet length",
                        "kermit_max_packet_length",
                        cfg.kermit_max_packet_length as u64,
                        10,
                        9024,
                        "bytes",
                    )
                    .await?;
                }
                "w" => {
                    self.xmodem_set_numeric(
                        "Window size",
                        "kermit_window_size",
                        cfg.kermit_window_size as u64,
                        1,
                        31,
                        "packets",
                    )
                    .await?;
                }
                "c" => {
                    self.xmodem_set_numeric(
                        "Block check type",
                        "kermit_block_check_type",
                        cfg.kermit_block_check_type as u64,
                        1,
                        3,
                        "(1/2/3)",
                    )
                    .await?;
                }
                "l" => {
                    self.kermit_toggle_bool(
                        "Long packets",
                        "kermit_long_packets",
                        cfg.kermit_long_packets,
                    )
                    .await?;
                }
                "s" => {
                    self.kermit_toggle_bool(
                        "Sliding windows",
                        "kermit_sliding_windows",
                        cfg.kermit_sliding_windows,
                    )
                    .await?;
                }
                "t" => {
                    self.kermit_toggle_bool(
                        "Streaming",
                        "kermit_streaming",
                        cfg.kermit_streaming,
                    )
                    .await?;
                }
                "a" => {
                    self.kermit_toggle_bool(
                        "Attribute packets",
                        "kermit_attribute_packets",
                        cfg.kermit_attribute_packets,
                    )
                    .await?;
                }
                "e" => {
                    self.kermit_toggle_bool(
                        "Repeat compression",
                        "kermit_repeat_compression",
                        cfg.kermit_repeat_compression,
                    )
                    .await?;
                }
                "i" => {
                    // 0 disables; 86400 (1 day) is a generous upper
                    // bound that still bounds memory growth from any
                    // peer-supplied state we might accumulate per
                    // session.
                    self.xmodem_set_numeric(
                        "Idle timeout",
                        "kermit_idle_timeout",
                        cfg.kermit_idle_timeout,
                        0,
                        86400,
                        "seconds (0 = disabled)",
                    )
                    .await?;
                }
                "8" => {
                    let next = match cfg.kermit_8bit_quote.as_str() {
                        "auto" => "on",
                        "on" => "off",
                        _ => "auto",
                    };
                    let key = "kermit_8bit_quote".to_string();
                    let v = next.to_string();
                    tokio::task::spawn_blocking(move || {
                        config::update_config_value(&key, &v);
                    })
                    .await
                    .ok();
                    self.send_line("").await?;
                    self.send_line(&format!(
                        "  {}",
                        self.green(&format!("8-bit quote set to {}.", next))
                    ))
                    .await?;
                    self.send_line("").await?;
                    self.send("  Press any key to continue.").await?;
                    self.flush().await?;
                    self.wait_for_key().await?;
                }
                "r" => {
                    self.config_restart_server().await?;
                }
                "k" => {
                    self.kermit_toggle_atdt_kermit(cfg.allow_atdt_kermit).await?;
                }
                "f" => {
                    self.kermit_toggle_bool(
                        "Locking shifts",
                        "kermit_locking_shifts",
                        cfg.kermit_locking_shifts,
                    )
                    .await?;
                }
                "u" => {
                    self.kermit_toggle_bool(
                        "Resume partial uploads",
                        "kermit_resume_partial",
                        cfg.kermit_resume_partial,
                    )
                    .await?;
                }
                "g" => {
                    self.kermit_toggle_bool(
                        "Wait for receiver NAK on download",
                        "kermit_wait_for_receiver",
                        cfg.kermit_wait_for_receiver,
                    )
                    .await?;
                }
                "d" => {
                    self.xmodem_set_numeric(
                        "Resume max age",
                        "kermit_resume_max_age_hours",
                        cfg.kermit_resume_max_age_hours as u64,
                        1,
                        8760,
                        "hours",
                    )
                    .await?;
                }
                "h" => {
                    self.kermit_show_help().await?;
                }
                _ => {
                    self.show_error("Press a listed key, V, R, K, H, or Q.")
                        .await?;
                }
            }
        }
    }

    /// Toggle `allow_atdt_kermit`.  When enabling, show a full-screen
    /// security warning and prompt for explicit Y/N confirmation —
    /// flipping this on lets serial callers reach Kermit server mode
    /// without going through the telnet auth gate, so we want the
    /// operator's intent on the record.  Disabling is one-click safe
    /// (no popup): tightening security never needs a confirmation.
    /// On confirmation (or unconditional disable), persist immediately
    /// via `update_config_value` so the change takes effect for the
    /// next ATDT without a server restart.
    pub(in crate::telnet) async fn kermit_toggle_atdt_kermit(
        &mut self,
        currently_enabled: bool,
    ) -> Result<(), std::io::Error> {
        if currently_enabled {
            // Disable path — no confirmation needed.
            tokio::task::spawn_blocking(move || {
                config::update_config_value("allow_atdt_kermit", "false");
            })
            .await
            .ok();
            self.send_line("").await?;
            self.send_line(&format!(
                "  {}",
                self.green("ATDT KERMIT disabled.")
            ))
            .await?;
            self.send_line("").await?;
            self.send("  Press any key to continue.").await?;
            self.flush().await?;
            self.wait_for_key().await?;
            return Ok(());
        }

        // Enable path — full-screen warning, Y/N prompt.
        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;
        self.send_line(&format!(
            "  {}",
            self.yellow("ENABLE ATDT KERMIT — SECURITY WARNING")
        ))
        .await?;
        self.send_line(&sep).await?;
        self.send_line("").await?;
        self.send_line(&format!(
            "  {}",
            self.amber("This bypasses telnet authentication.")
        ))
        .await?;
        self.send_line("").await?;
        self.send_line(
            "  When enabled, anyone who can reach a serial port",
        )
        .await?;
        self.send_line(
            "  can dial ATDT KERMIT and land directly in Kermit",
        )
        .await?;
        self.send_line(
            "  server mode — no username, no password, no menu.",
        )
        .await?;
        self.send_line("").await?;
        self.send_line(
            "  If your gateway has security_enabled = true and you",
        )
        .await?;
        self.send_line(
            "  need every caller to authenticate, leave this OFF",
        )
        .await?;
        self.send_line(&format!(
            "  and have callers go via the {} menu's {} entry",
            self.cyan("File Transfer"),
            self.cyan("K")
        ))
        .await?;
        self.send_line(&format!(
            "  (main menu {} then {}) — that path runs the auth",
            self.cyan("F"),
            self.cyan("K")
        ))
        .await?;
        self.send_line("  prompt before handing off to Kermit.").await?;
        self.send_line("").await?;
        self.send_line(
            "  Enable only when the serial line itself is trusted",
        )
        .await?;
        self.send_line(
            "  (private cable, isolated lab, single-user setup).",
        )
        .await?;
        self.send_line("").await?;
        self.send(&format!(
            "  Enable ATDT KERMIT? ({}/{}): ",
            self.cyan("Y"),
            self.cyan("N")
        ))
        .await?;
        self.flush().await?;

        let answer = self.get_menu_input(false).await?;
        let confirmed = matches!(
            answer.as_deref().map(|s| s.trim()),
            Some("y") | Some("Y")
        );

        self.send_line("").await?;
        if confirmed {
            tokio::task::spawn_blocking(move || {
                config::update_config_value("allow_atdt_kermit", "true");
            })
            .await
            .ok();
            self.send_line(&format!(
                "  {}",
                self.green("ATDT KERMIT enabled.")
            ))
            .await?;
        } else {
            self.send_line(&format!(
                "  {}",
                self.dim("ATDT KERMIT left disabled.")
            ))
            .await?;
        }
        self.send_line("").await?;
        self.send("  Press any key to continue.").await?;
        self.flush().await?;
        self.wait_for_key().await?;
        Ok(())
    }

    /// Helper: flip a Kermit boolean config key, persist, and confirm.
    pub(in crate::telnet) async fn kermit_toggle_bool(
        &mut self,
        label: &str,
        key: &str,
        current: bool,
    ) -> Result<(), std::io::Error> {
        let next = !current;
        let k = key.to_string();
        let v = next.to_string();
        tokio::task::spawn_blocking(move || {
            config::update_config_value(&k, &v);
        })
        .await
        .ok();
        self.send_line("").await?;
        self.send_line(&format!(
            "  {}",
            self.green(&format!(
                "{} {}.",
                label,
                if next { "enabled" } else { "disabled" }
            ))
        ))
        .await?;
        self.send_line("").await?;
        self.send("  Press any key to continue.").await?;
        self.flush().await?;
        self.wait_for_key().await?;
        Ok(())
    }

    pub(in crate::telnet) async fn kermit_show_help(&mut self) -> Result<(), std::io::Error> {
        let lines = Self::kermit_help_lines(self.terminal_type == TerminalType::Petscii);
        self.show_help_page("KERMIT SETTINGS HELP", lines).await
    }

    /// Kermit settings help, split by terminal width.  Associated fn so a unit
    /// test asserts the REAL lines fit 40 columns (see `punter_help_lines`).
    pub(in crate::telnet) fn kermit_help_lines(petscii: bool) -> &'static [&'static str] {
        if petscii {
            &[
                "  Configure Kermit transfer",
                "  parameters.  Negotiated with",
                "  the peer at session start.",
                "",
                "  N  Negotiate timeout (45 s)",
                "  P  Per-packet timeout",
                "  X  Max retries per packet",
                "  M  Max packet length",
                "  W  Sliding window size",
                "  C  Block check type 1/2/3",
                "  L/S/T/A/E/I  toggles",
                "  8  cycle 8-bit quote mode",
                "  F  locking-shift toggle",
                "  U  resume partial uploads",
                "  D  resume max age (hours)",
                "  K  ATDT KERMIT toggle",
                "     (bypasses security)",
                "",
                "  Streaming auto-degrades to",
                "  sliding/stop-and-wait when",
                "  the peer can't do it.",
            ]
        } else {
            &[
                "  Configure Kermit transfer parameters.",
                "  These are advertised in our Send-Init;",
                "  the peer's response narrows the session",
                "  to the intersection of capabilities.",
                "",
                "  N  Negotiate timeout (Send-Init handshake)",
                "  P  Per-packet read timeout",
                "  X  Max retries per packet (NAK / timeout)",
                "  M  Max packet length we'll advertise",
                "  W  Sliding-window size (1=stop-and-wait)",
                "  C  Block check type: 1=6-bit, 2=12-bit, 3=CRC-16",
                "  L  Long-packet capability",
                "  S  Sliding-window capability",
                "  T  Streaming capability",
                "  A  Attribute-packet capability",
                "  E  Repeat-count compression",
                "  I  Telnet IAC escape during transfer",
                "  8  8-bit quote: auto / on / off",
                "  F  Locking-shift (SO/SI) capability for",
                "     8-bit data over 7-bit links",
                "  U  Resume partial uploads (disposition R):",
                "     append to a matching on-disk partial",
                "  D  Resume max age in hours: ignore on-disk",
                "     partials older than this when resuming",
                "  K  Allow ATDT KERMIT from either serial",
                "     port's modem (bypasses security_enabled",
                "     auth gate; prompts for explicit Y/N",
                "     before enabling)",
                "",
                "  Streaming requires a reliable transport.",
                "  Disable when bridging to flaky serial.",
            ]
        }
    }

    pub(in crate::telnet) async fn xmodem_set_dir(&mut self, current: &str) -> Result<(), std::io::Error> {
        self.send_line("").await?;
        self.send_line(&format!(
            "  Current directory: {}",
            self.amber(current)
        ))
        .await?;
        self.send("  New directory: ").await?;
        self.flush().await?;

        let input = match self.get_line_input().await? {
            Some(s) if !s.is_empty() => s,
            _ => return Ok(()),
        };

        let v = input.clone();
        tokio::task::spawn_blocking(move || {
            config::update_config_value("transfer_dir", &v);
        })
        .await
        .ok();
        self.send_line("").await?;
        self.send_line(&format!(
            "  {}",
            self.green(&format!("Transfer dir set to: {}", input))
        ))
        .await?;
        self.send_line("").await?;
        self.send("  Press any key to continue.").await?;
        self.flush().await?;
        self.wait_for_key().await?;
        Ok(())
    }

    pub(in crate::telnet) async fn xmodem_set_numeric(
        &mut self,
        label: &str,
        key: &str,
        current: u64,
        min: u64,
        max: u64,
        unit: &str,
    ) -> Result<(), std::io::Error> {
        self.send_line("").await?;
        self.send_line(&format!(
            "  Current {}: {}",
            label.to_lowercase(),
            self.amber(&current.to_string())
        ))
        .await?;
        self.send(&format!("  New value ({}-{}): ", min, max)).await?;
        self.flush().await?;

        let input = match self.get_line_input().await? {
            Some(s) if !s.is_empty() => s,
            _ => return Ok(()),
        };

        if let Ok(val) = input.parse::<u64>() {
            if val >= min && val <= max {
                let k = key.to_string();
                let v = val.to_string();
                tokio::task::spawn_blocking(move || {
                    config::update_config_value(&k, &v);
                })
                .await
                .ok();
                self.send_line("").await?;
                self.send_line(&format!(
                    "  {}",
                    self.green(&format!("{} set to {} {}.", label, val, unit))
                ))
                .await?;
                self.send_line("").await?;
                self.send("  Press any key to continue.").await?;
                self.flush().await?;
                self.wait_for_key().await?;
            } else {
                self.show_error(&format!("Value must be {}-{}.", min, max)).await?;
            }
        } else {
            self.show_error("Invalid number.").await?;
        }
        Ok(())
    }

    pub(in crate::telnet) async fn xmodem_show_help(&mut self) -> Result<(), std::io::Error> {
        let lines = Self::xmodem_help_lines(self.terminal_type == TerminalType::Petscii);
        self.show_help_page("XMODEM SETTINGS HELP", lines).await
    }

    /// XMODEM-family settings help, split by terminal width.  An associated fn
    /// (no `self`) so a unit test asserts the REAL lines fit 40 columns —
    /// matching `punter_help_lines`, with no duplicated copy to drift.
    pub(in crate::telnet) fn xmodem_help_lines(petscii: bool) -> &'static [&'static str] {
        if petscii {
            &[
                "  Configure XMODEM family transfer",
                "  settings. Shared with XMODEM-1K",
                "  and YMODEM.",
                "",
                "  N  Negotiation timeout: how",
                "     long to wait for transfer",
                "     to begin",
                "  I  Retry interval: C/NAK poke",
                "     gap (spec ~10 s, def 7 s)",
                "  B  Block timeout: how long to",
                "     wait for each block",
                "  M  Max retries per block",
                "  R  Restart the server",
                "",
                "  Takes effect on next transfer.",
            ]
        } else {
            &[
                "  Configure XMODEM family transfer",
                "  settings. Shared with XMODEM-1K and",
                "  YMODEM (same protocol code path).",
                "",
                "  N  Negotiation timeout: how long to",
                "     wait for a transfer to begin",
                "  I  Retry interval: seconds between",
                "     C/NAK pokes during the handshake",
                "     (spec suggests ~10, default 7)",
                "  B  Block timeout: how long to wait",
                "     for each data block",
                "  M  Max retries: retry limit per block",
                "  R  Restart the server",
                "",
                "  Takes effect on next transfer.",
            ]
        }
    }

    // ─── TROUBLESHOOTING ────────────────────────────────────

    pub(in crate::telnet) fn client_type_label(&self) -> &'static str {
        if self.is_relay {
            "Relay (slave)"
        } else if self.is_ssh {
            "SSH"
        } else if self.is_serial {
            "Serial modem"
        } else if self.telnet_negotiated {
            "Telnet"
        } else {
            "Raw TCP"
        }
    }

    pub(in crate::telnet) fn terminal_type_label(&self) -> &'static str {
        match self.terminal_type {
            TerminalType::Petscii => "PETSCII",
            TerminalType::Ansi => "ANSI",
            TerminalType::Ascii => "ASCII",
        }
    }

    pub(in crate::telnet) async fn troubleshooting(&mut self) -> Result<(), std::io::Error> {
        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;
        self.send_line(&format!(
            "  {}",
            self.yellow("CHARACTER TROUBLESHOOTING")
        ))
        .await?;
        self.send_line(&sep).await?;
        self.send_line("").await?;
        self.send_line(&format!(
            "  Client:   {}",
            self.cyan(self.client_type_label())
        ))
        .await?;
        self.send_line(&format!(
            "  Terminal: {}",
            self.cyan(self.terminal_type_label())
        ))
        .await?;
        self.send_line(&format!(
            "  IAC esc:  {}",
            self.cyan(if self.xmodem_iac { "On" } else { "Off" })
        ))
        .await?;
        self.send_line("").await?;
        self.send_line("  Press any key to see its hex value.")
            .await?;
        let esc_label = match self.terminal_type {
            TerminalType::Petscii => "<-",
            _ => "ESC",
        };
        self.send_line(&format!(
            "  Press {} twice to return to menu.",
            self.cyan(esc_label)
        ))
        .await?;
        self.send_line("").await?;
        // PETSCII width minus 1 — same auto-wrap reason as `separator()`.
        self.send_line(&self.yellow(&"-".repeat(
            if self.terminal_type == TerminalType::Petscii { PETSCII_WIDTH - 1 } else { 56 }
        )))
        .await?;
        self.send_line("").await?;
        self.flush().await?;

        let mut last_was_esc = false;

        loop {
            let byte = match self.read_byte_filtered().await? {
                Some(b) => b,
                None => return Ok(()),
            };

            let name = match byte {
                0x00 => "NUL",
                0x01 => "SOH",
                0x02 => "STX",
                0x03 => "ETX",
                0x04 => "EOT",
                0x05 => "ENQ",
                0x06 => "ACK",
                0x07 => "BEL",
                0x08 => "BS",
                0x09 => "TAB",
                0x0A => "LF",
                0x0B => "VT",
                0x0C => "FF",
                0x0D => "CR",
                0x0E => "SO",
                0x0F => "SI",
                0x10 => "DLE",
                0x11 => "DC1",
                0x12 => "DC2",
                0x13 => "DC3",
                0x14 => "DC4/C64-DEL",
                0x15 => "NAK",
                0x16 => "SYN",
                0x17 => "ETB",
                0x18 => "CAN",
                0x19 => "EM",
                0x1A => "SUB",
                0x1B => "ESC",
                0x1C => "FS",
                0x1D => "GS/C64-RIGHT",
                0x1E => "RS",
                0x1F => "US",
                0x7F => "DEL",
                0x91 => "C64-UP",
                0x93 => "C64-CLR",
                0x9D => "C64-LEFT",
                _ => "",
            };

            let display = if !name.is_empty() {
                format!("  Key: {} ({:3}) = {}",
                    self.cyan(&format!("0x{:02X}", byte)), byte, name)
            } else if (0x20..=0x7E).contains(&byte) {
                format!("  Key: {} ({:3}) = '{}'",
                    self.cyan(&format!("0x{:02X}", byte)), byte, byte as char)
            } else {
                format!("  Key: {} ({:3})",
                    self.cyan(&format!("0x{:02X}", byte)), byte)
            };
            self.send_line(&display).await?;
            self.flush().await?;

            if is_esc_key(byte, self.terminal_type == TerminalType::Petscii) {
                if last_was_esc {
                    self.send_line("").await?;
                    self.send_line("  Returning to main menu...").await?;
                    self.flush().await?;
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    return Ok(());
                }
                last_was_esc = true;
            } else {
                last_was_esc = false;
            }
        }
    }
}
