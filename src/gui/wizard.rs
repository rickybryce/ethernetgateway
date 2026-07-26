//! First-run setup wizard — desktop GUI only.
//!
//! Shown by [`super::App`] while `setup_wizard_completed = false` (a genuinely
//! fresh install, or an operator who asked to run it again from the Server
//! "More" window).  It is a *draft* editor: nothing here touches the live
//! config or the running server until the last screen's Save button, and the
//! only key it writes on an exit/skip is `setup_wizard_completed` itself.  The
//! telnet and web configuration UIs deliberately have no equivalent — this is
//! initial setup at the console, not another config surface to keep in sync.
//!
//! Screen order is fixed (see [`ORDER`]); the master-credentials screen is
//! skipped unless the Slave role is selected.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError};

use eframe::egui;

use super::{
    labeled_field, singleline_with_menu, spawn_folder_picker, AMBER, AMBER_BRIGHT, AMBER_DIM,
    SCRIPTURE, TEXT_PRIMARY, WARN_BORDER,
};
use crate::config::Config;

/// What the wizard is asking its host to do after this frame.
pub(super) enum Outcome {
    /// Still on screen — nothing for the host to do.
    Continue,
    /// The operator exited/skipped: mark the wizard done and keep the current
    /// settings.  No restart.
    Exit,
    /// The operator finished: apply the draft, save, restart.
    Finish,
}

/// One wizard screen.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    Welcome,
    Credentials,
    Telnet,
    Ssh,
    Security,
    Web,
    TransferDir,
    Cpm,
    Role,
    /// Slave-only: where the master is and how to log into it.
    SlaveMaster,
    Finish,
}

/// Fixed screen order.  `SlaveMaster` is present here but skipped by
/// [`Wizard::neighbour`] unless the Slave role is selected, so Back from the
/// final screen lands on Role for a standalone/master gateway and on
/// SlaveMaster for a slave.
const ORDER: &[Step] = &[
    Step::Welcome,
    Step::Credentials,
    Step::Telnet,
    Step::Ssh,
    Step::Security,
    Step::Web,
    Step::TransferDir,
    Step::Cpm,
    Step::Role,
    Step::SlaveMaster,
    Step::Finish,
];

/// Draft state for the whole wizard.  Ports are held as text (like every other
/// numeric field in this GUI) so a half-typed value never becomes a config
/// value; they are parsed on the Next click that leaves their screen.
pub(super) struct Wizard {
    step: Step,
    /// Validation message for the current screen, cleared on any navigation.
    error: Option<String>,

    username: String,
    password: String,
    password_confirm: String,
    show_passwords: bool,

    telnet_enabled: bool,
    telnet_port: String,

    ssh_enabled: bool,
    ssh_port: String,

    security_enabled: bool,
    disable_ip_safety: bool,
    disable_gateway_connections: bool,

    web_enabled: bool,
    web_port: String,

    transfer_dir: String,
    /// Folder picker runs on a background thread (same pattern as the main
    /// editor's transfer-dir button); `Some` while a dialog is open.
    pending_dir_pick: Option<Receiver<Option<PathBuf>>>,

    cpm_enabled: bool,
    /// Maps to `cpm_emu_uart`: on = the default UART profile, off = `"off"`.
    cpm_dialout: bool,

    /// "standalone" | "master" | "slave" — same values as `gateway_role`.
    role: String,
    master_host: String,
    master_port: String,
    master_username: String,
    master_password: String,
}

impl Wizard {
    /// Seed the draft from the config in effect.  On a true first run that is
    /// the freshly written defaults; on a re-run it is what the operator
    /// already has, so nothing is silently reset.
    pub(super) fn new(cfg: &Config) -> Self {
        Self {
            step: Step::Welcome,
            error: None,
            username: cfg.username.clone(),
            password: String::new(),
            password_confirm: String::new(),
            show_passwords: false,
            telnet_enabled: cfg.telnet_enabled,
            telnet_port: cfg.telnet_port.to_string(),
            ssh_enabled: cfg.ssh_enabled,
            ssh_port: cfg.ssh_port.to_string(),
            security_enabled: cfg.security_enabled,
            disable_ip_safety: cfg.disable_ip_safety,
            disable_gateway_connections: cfg.disable_gateway_connections,
            web_enabled: cfg.web_enabled,
            web_port: cfg.web_port.to_string(),
            transfer_dir: cfg.transfer_dir.clone(),
            pending_dir_pick: None,
            cpm_enabled: cfg.cpm_emu_enabled,
            cpm_dialout: cfg.cpm_emu_uart != "off",
            role: cfg.gateway_role.clone(),
            master_host: cfg.slave_master_host.clone(),
            master_port: cfg.slave_master_port.to_string(),
            master_username: cfg.slave_master_username.clone(),
            master_password: cfg.slave_master_password.clone(),
        }
    }

    /// Write the finished draft into `cfg`.  Only called from the final
    /// screen's Save button, and only after every screen has validated, so the
    /// port parses here cannot fail — an unexpected one keeps the existing
    /// value rather than substituting a default.
    pub(super) fn apply_to(&self, cfg: &mut Config) {
        cfg.username = self.username.trim().to_string();
        cfg.password = self.password.clone();

        cfg.telnet_enabled = self.telnet_enabled;
        if let Some(p) = parse_port(&self.telnet_port) {
            cfg.telnet_port = p;
        }
        cfg.ssh_enabled = self.ssh_enabled;
        if let Some(p) = parse_port(&self.ssh_port) {
            cfg.ssh_port = p;
        }
        cfg.web_enabled = self.web_enabled;
        if let Some(p) = parse_port(&self.web_port) {
            cfg.web_port = p;
        }

        cfg.security_enabled = self.security_enabled;
        cfg.disable_ip_safety = self.disable_ip_safety;
        cfg.disable_gateway_connections = self.disable_gateway_connections;

        let dir = self.transfer_dir.trim();
        if !dir.is_empty() {
            cfg.transfer_dir = dir.to_string();
        }

        cfg.cpm_emu_enabled = self.cpm_enabled;
        // Preserve a non-default UART profile the operator may already have
        // chosen elsewhere: only flip between "off" and a working profile.
        cfg.cpm_emu_uart = match (self.cpm_dialout, cfg.cpm_emu_uart.as_str()) {
            (false, _) => "off".to_string(),
            (true, "off") => crate::cpm::uart::DEFAULT_UART.to_string(),
            (true, existing) => existing.to_string(),
        };

        cfg.gateway_role = self.role.clone();
        match self.role.as_str() {
            // A master exists to accept slaves, so arm the accept-relays gate
            // with the role.  The SSH server it needs is NOT forced on here:
            // the Role screen explains why a master wants it and offers a
            // button, and the review screen warns if it is still off — an
            // operator who declines keeps their choice (main.rs logs the same
            // warning at startup).
            "master" => {
                cfg.master_accept_relays = true;
            }
            "slave" => {
                cfg.slave_master_host = self.master_host.trim().to_string();
                if let Some(p) = parse_port(&self.master_port) {
                    cfg.slave_master_port = p;
                }
                cfg.slave_master_username = self.master_username.trim().to_string();
                cfg.slave_master_password = self.master_password.clone();
            }
            _ => {}
        }

        cfg.setup_wizard_completed = true;
    }

    /// The step before/after the current one, skipping the slave-only screen
    /// when the role isn't Slave.  `None` at either end.
    fn neighbour(&self, forward: bool) -> Option<Step> {
        let idx = ORDER.iter().position(|s| *s == self.step)?;
        let mut i = idx as isize;
        loop {
            i += if forward { 1 } else { -1 };
            if i < 0 || i as usize >= ORDER.len() {
                return None;
            }
            let candidate = ORDER[i as usize];
            if candidate == Step::SlaveMaster && self.role != "slave" {
                continue;
            }
            return Some(candidate);
        }
    }

    /// Validate the screen we're leaving.  `cfg` supplies the settings the
    /// wizard doesn't edit but still has to reason about (the standalone
    /// Kermit-server listener, whose port must not collide).
    fn validate(&self, cfg: &Config) -> Result<(), String> {
        match self.step {
            Step::Credentials => {
                if self.username.trim().is_empty() {
                    return Err("Username cannot be empty.".into());
                }
                if self.username.trim().contains(char::is_whitespace) {
                    return Err("Username cannot contain spaces.".into());
                }
                if self.password.is_empty() {
                    return Err("Password cannot be empty.".into());
                }
                if self.password != self.password_confirm {
                    return Err("The passwords do not match — please retype them.".into());
                }
                Ok(())
            }
            Step::Telnet => self.check_port(Listener::Telnet, cfg),
            Step::Ssh => self.check_port(Listener::Ssh, cfg),
            Step::Web => self.check_port(Listener::Web, cfg),
            Step::TransferDir => {
                if self.transfer_dir.trim().is_empty() {
                    return Err("The transfer directory cannot be empty.".into());
                }
                Ok(())
            }
            Step::SlaveMaster => {
                if self.master_host.trim().is_empty() {
                    return Err("Enter the master's hostname or IP address.".into());
                }
                if parse_port(&self.master_port).is_none() {
                    return Err("The master's SSH port must be a number from 1 to 65535.".into());
                }
                if self.master_username.trim().is_empty() {
                    return Err("Enter the username this slave logs into the master with.".into());
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Shared listener-port check for the screen being left: the port parses,
    /// and no other listener this gateway will bring up already claims it.  A
    /// disabled listener isn't checked at all (its port is inert), which also
    /// lets the operator leave a stale value in a box they've just unticked.
    fn check_port(&self, which: Listener, cfg: &Config) -> Result<(), String> {
        let (enabled, value) = self.listener_field(which);
        if !enabled {
            return Ok(());
        }
        let Some(port) = parse_port(value) else {
            return Err(format!(
                "The {} port must be a number from 1 to 65535.",
                which.subject()
            ));
        };
        for other in [Listener::Telnet, Listener::Ssh, Listener::Web] {
            if other == which {
                continue;
            }
            let (other_enabled, other_value) = self.listener_field(other);
            if other_enabled && parse_port(other_value) == Some(port) {
                return Err(collision_msg(port, other.label()));
            }
        }
        // The standalone Kermit listener isn't part of this wizard, but it is a
        // port this process will bind — so a clash with it has to be caught here
        // or the operator only finds out from a bind error in the log.
        if cfg.kermit_server_enabled && cfg.kermit_server_port == port {
            return Err(collision_msg(port, "the Kermit server"));
        }
        Ok(())
    }

    /// The enabled flag and port text backing one listener.  Keyed off the
    /// `Listener` enum rather than a name string so the "skip myself" test in
    /// `check_port` can't be broken by an edit to a user-facing label.
    fn listener_field(&self, which: Listener) -> (bool, &str) {
        match which {
            Listener::Telnet => (self.telnet_enabled, &self.telnet_port),
            Listener::Ssh => (self.ssh_enabled, &self.ssh_port),
            Listener::Web => (self.web_enabled, &self.web_port),
        }
    }

    /// The inbound TCP ports this configuration will listen on, with what each
    /// one is for — the firewall list on the final screen.  Includes listeners
    /// the wizard doesn't ask about (the standalone Kermit server) so the
    /// operator isn't left guessing, and covers the master role's SSH port even
    /// when the SSH checkbox was left clear, because `apply_to` turns it on.
    fn inbound_ports(&self, cfg: &Config) -> Vec<(u16, &'static str)> {
        let mut out = Vec::new();
        if self.telnet_enabled {
            if let Some(p) = parse_port(&self.telnet_port) {
                out.push((p, "telnet server"));
            }
        }
        if self.ssh_enabled {
            if let Some(p) = parse_port(&self.ssh_port) {
                out.push((
                    p,
                    if self.role == "master" {
                        "SSH server (also carries the slave links)"
                    } else {
                        "SSH server"
                    },
                ));
            }
        }
        if self.web_enabled {
            if let Some(p) = parse_port(&self.web_port) {
                out.push((p, "web configuration page"));
            }
        }
        if cfg.kermit_server_enabled {
            out.push((cfg.kermit_server_port, "standalone Kermit server"));
        }
        out
    }

    /// Advisory notes for the final screen — things worth saying out loud but
    /// never worth blocking Save over.
    fn warnings(&self) -> Vec<String> {
        let mut out = Vec::new();
        if !self.telnet_enabled && !self.ssh_enabled {
            out.push(
                "Telnet and SSH are both off — no network logins will be possible. \
                 (Serial ports, if you enable them, still work.)"
                    .into(),
            );
        }
        if self.disable_ip_safety && !self.security_enabled {
            out.push(
                "IP safety is off and login is not required — anyone who can reach this \
                 machine gets an unauthenticated session."
                    .into(),
            );
        }
        for (enabled, port, what) in [
            (self.telnet_enabled, &self.telnet_port, "Telnet"),
            (self.ssh_enabled, &self.ssh_port, "SSH"),
            (self.web_enabled, &self.web_port, "The web server"),
        ] {
            if enabled && parse_port(port).is_some_and(|p| p < 1024) {
                out.push(format!(
                    "{} is set to port {} — ports below 1024 require running as root.",
                    what,
                    port.trim()
                ));
            }
        }
        if self.role == "master" && !self.ssh_enabled {
            out.push(
                "Master role with the SSH server off — slave links ride the SSH port, so NO \
                 slave will be able to connect until you enable it. Go back to the role \
                 screen to turn it on."
                    .into(),
            );
        }
        out
    }

    /// Render the current screen.  Returns what the host should do next.
    pub(super) fn draw(&mut self, ui: &mut egui::Ui, cfg: &Config, local_ip: &str) -> Outcome {
        self.poll_dir_pick();
        apply_type_scale(ui);

        let mut outcome = Outcome::Continue;

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add_space(8.0);
                ui.heading(
                    egui::RichText::new(format!(
                        "Ethernet Gateway v{} — Setup",
                        env!("CARGO_PKG_VERSION")
                    ))
                    .strong()
                    .color(AMBER_BRIGHT),
                );
                ui.label(
                    egui::RichText::new(self.step_title())
                        .size(HEADING)
                        .strong()
                        .color(AMBER),
                );
                ui.add_space(6.0);
                ui.separator();
                ui.add_space(6.0);

                match self.step {
                    Step::Welcome => self.draw_welcome(ui),
                    Step::Credentials => self.draw_credentials(ui),
                    Step::Telnet => self.draw_telnet(ui),
                    Step::Ssh => self.draw_ssh(ui),
                    Step::Security => self.draw_security(ui),
                    Step::Web => self.draw_web(ui, local_ip),
                    Step::TransferDir => self.draw_transfer_dir(ui),
                    Step::Cpm => self.draw_cpm(ui),
                    Step::Role => self.draw_role(ui),
                    Step::SlaveMaster => self.draw_slave_master(ui),
                    Step::Finish => self.draw_finish(ui, cfg, local_ip),
                }

                ui.add_space(8.0);
                if let Some(msg) = &self.error {
                    ui.label(
                        egui::RichText::new(format!("⚠  {}", msg))
                            .strong()
                            .color(WARN_BORDER),
                    );
                    ui.add_space(4.0);
                }
                ui.separator();
                ui.add_space(4.0);
                outcome = self.draw_nav(ui, cfg);
                ui.add_space(8.0);
            });

        outcome
    }

    /// Footer: Exit on the left, Back/Next (or Save and Restart) on the right.
    fn draw_nav(&mut self, ui: &mut egui::Ui, cfg: &Config) -> Outcome {
        let mut outcome = Outcome::Continue;
        ui.horizontal(|ui| {
            let exit_label = if self.step == Step::Welcome {
                "Skip setup — use defaults"
            } else {
                "Exit setup — keep current settings"
            };
            if ui.button(exit_label).clicked() {
                outcome = Outcome::Exit;
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // Keep the rightmost button off the frame edge.
                ui.add_space(6.0);
                if self.step == Step::Finish {
                    if ui
                        .add(egui::Button::new(
                            egui::RichText::new("Save and Restart Server")
                                .strong()
                                .size(WIDGET + 2.0)
                                .color(AMBER_BRIGHT),
                        ))
                        .clicked()
                    {
                        outcome = Outcome::Finish;
                    }
                } else {
                    let next_label = if self.step == Step::Welcome {
                        "Start Setup  >"
                    } else {
                        "Next  >"
                    };
                    if ui
                        .add(egui::Button::new(
                            egui::RichText::new(next_label).strong().color(AMBER_BRIGHT),
                        ))
                        .clicked()
                    {
                        match self.validate(cfg) {
                            Ok(()) => {
                                self.error = None;
                                if let Some(next) = self.neighbour(true) {
                                    self.step = next;
                                }
                            }
                            Err(e) => self.error = Some(e),
                        }
                    }
                }
                let back = self.neighbour(false);
                if ui
                    .add_enabled(back.is_some(), egui::Button::new("<  Back"))
                    .clicked()
                {
                    if let Some(prev) = back {
                        self.error = None;
                        self.step = prev;
                    }
                }
            });
        });
        outcome
    }

    /// Heading for the current screen.  The step number is *computed* from the
    /// screens actually in play rather than written into each label, because the
    /// slave path has one screen more than the others — hand-written numbers
    /// gave two different screens the same "8 of 8".
    fn step_title(&self) -> String {
        let name = match self.step {
            Step::Welcome => return "Welcome".to_string(),
            Step::Finish => return "Review and finish".to_string(),
            Step::Credentials => "Username and password",
            Step::Telnet => "Telnet server",
            Step::Ssh => "SSH server",
            Step::Security => "Access control",
            Step::Web => "Web configuration server",
            Step::TransferDir => "File transfer directory",
            Step::Cpm => "CP/M emulator",
            Step::Role => "Gateway role",
            Step::SlaveMaster => "Master connection (slave)",
        };
        let numbered: Vec<Step> = ORDER
            .iter()
            .copied()
            .filter(|s| !matches!(s, Step::Welcome | Step::Finish))
            .filter(|s| *s != Step::SlaveMaster || self.role == "slave")
            .collect();
        match numbered.iter().position(|s| *s == self.step) {
            Some(i) => format!("Step {} of {} — {}", i + 1, numbered.len(), name),
            // Unreachable in practice (every numbered screen is in ORDER); fall
            // back to the bare name rather than inventing a number.
            None => name.to_string(),
        }
    }

    // ── Screens ───────────────────────────────────────────────

    fn draw_welcome(&mut self, ui: &mut egui::Ui) {
        body(
            ui,
            "This looks like the first time this gateway has run, so let's set up the \
             essentials: a login, which servers to start, where transferred files go, and \
             whether this machine stands alone or is part of a master/slave pair.",
        );
        ui.add_space(6.0);
        note(ui, "What's inside, once you're set up:");
        for line in [
            "File transfer — XMODEM, XMODEM-1K, YMODEM, ZMODEM, Kermit and Punter, for \
             everything from a Commodore 64 to a modern terminal.",
            "Gateways — dial out to SSH or telnet hosts, or bridge a real serial port.",
            "Serial modem emulator — Hayes AT commands over a real UART, so vintage \
             terminal software can \"dial\" the internet.",
            "CP/M emulator — runs real Z80 .COM software, and ships with our own EGT80 \
             terminal program.",
            "Gateway Shell — a CP/M-style file manager over the transfer directory.",
            "Extras — a text-mode web browser, a weather service and an AI chat client.",
        ] {
            bullet(ui, line);
        }
        ui.add_space(6.0);
        note(
            ui,
            "Nothing is saved until the last screen, and you can re-run this wizard later \
             from the Server \"More...\" window.",
        );
    }

    fn draw_credentials(&mut self, ui: &mut egui::Ui) {
        body(
            ui,
            "These are the gateway's credentials — used for telnet logins (when you require \
             one), the SSH server, and the web configuration page.",
        );
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label("Username:");
            singleline_with_menu(ui, &mut self.username, false, Some(180.0));
        });
        let mask = !self.show_passwords;
        ui.horizontal(|ui| {
            ui.label("Password:");
            singleline_with_menu(ui, &mut self.password, mask, Some(180.0));
        });
        ui.horizontal(|ui| {
            ui.label("Retype password:");
            singleline_with_menu(ui, &mut self.password_confirm, mask, Some(180.0));
        });
        ui.checkbox(&mut self.show_passwords, "Show the passwords I'm typing");
        ui.add_space(6.0);
        warn(
            ui,
            "The password is stored in plain text in egateway.conf — it is not hashed. \
             Keep that file readable only by you, and don't reuse a password that matters \
             elsewhere.",
        );
    }

    fn draw_telnet(&mut self, ui: &mut egui::Ui) {
        body(
            ui,
            "Telnet is how vintage hardware and most retro terminal software reach the \
             gateway. It auto-detects PETSCII, ANSI and ASCII terminals.",
        );
        ui.add_space(6.0);
        ui.checkbox(&mut self.telnet_enabled, "Enable the telnet server");
        ui.add_enabled_ui(self.telnet_enabled, |ui| {
            ui.horizontal(|ui| {
                labeled_field(ui, "Port:", &mut self.telnet_port, 70.0);
                ui.label(egui::RichText::new("(default 2323)").italics().color(AMBER_DIM));
            });
        });
        ui.add_space(6.0);
        warn(
            ui,
            "Telnet is unencrypted: the login and everything typed during the session cross \
             the network as plain text. That's fine on a trusted LAN — which is what this \
             gateway is built for — but don't expose the port to the internet. Use the SSH \
             server on the next screen for anything else.",
        );
    }

    fn draw_ssh(&mut self, ui: &mut egui::Ui) {
        body(
            ui,
            "The SSH server offers the same menu as telnet, encrypted, for modern clients. \
             An Ed25519 host key is generated automatically on first use.",
        );
        ui.add_space(6.0);
        ui.checkbox(&mut self.ssh_enabled, "Enable the SSH server");
        ui.add_enabled_ui(self.ssh_enabled, |ui| {
            ui.horizontal(|ui| {
                labeled_field(ui, "Port:", &mut self.ssh_port, 70.0);
                ui.label(egui::RichText::new("(default 2222)").italics().color(AMBER_DIM));
            });
        });
        ui.add_space(6.0);
        note(
            ui,
            "Log in with the username and password you entered on the first screen. SSH \
             always authenticates, whether or not you require a login for telnet.",
        );
    }

    fn draw_security(&mut self, ui: &mut egui::Ui) {
        body(ui, "Three access-control settings. The defaults suit a home LAN.");
        ui.add_space(6.0);

        ui.checkbox(&mut self.security_enabled, "Require login");
        indent(
            ui,
            "On: a telnet session must enter the username and password before it reaches the \
             menu, and three bad tries ban that IP for five minutes. Off: telnet sessions go \
             straight to the menu. The SSH server and the web page always require the \
             password either way.",
        );
        ui.add_space(6.0);

        ui.checkbox(&mut self.disable_ip_safety, "Disable IP safety");
        indent(
            ui,
            "With login not required, the telnet listener only accepts private/LAN \
             addresses (192.168.x.x, 10.x.x.x and the like) plus loopback — that allowlist \
             is the only thing standing between a public address and an unauthenticated \
             session, so ticking this box removes it. The web page keeps the allowlist \
             whether or not login is required, and this setting is the one and only way to \
             lift it. Leave it clear unless you know you need it.",
        );
        ui.add_space(6.0);

        let router = crate::router::describe();
        ui.checkbox(
            &mut self.disable_gateway_connections,
            format!("Block connections from the router ({})", router),
        );
        indent(
            ui,
            &format!(
                "Traffic forwarded in from outside your LAN often appears to come from the \
                 router, so refusing it closes that path; but so does traffic hairpinned \
                 from inside your own network, and an administrator working from the \
                 router's address. Off by default. {}",
                if router == "x.x.x.1" {
                    "This machine could not tell us its router's address, so the rule falls \
                     back to refusing any address ending in .1 — the usual convention."
                } else {
                    "That address came from this machine's own routing table, so it is the \
                     real router, not a guess."
                }
            ),
        );
    }

    fn draw_web(&mut self, ui: &mut egui::Ui, local_ip: &str) {
        body(
            ui,
            "The web server lets you change every gateway setting from a browser, so you \
             don't need this console window to reconfigure the machine.",
        );
        ui.add_space(6.0);
        ui.checkbox(&mut self.web_enabled, "Enable the web configuration server");
        ui.add_enabled_ui(self.web_enabled, |ui| {
            ui.horizontal(|ui| {
                labeled_field(ui, "Port:", &mut self.web_port, 70.0);
                ui.label(egui::RichText::new("(default 8080)").italics().color(AMBER_DIM));
            });
        });
        ui.add_space(6.0);
        if self.web_enabled {
            if let Some(port) = parse_port(&self.web_port) {
                note(ui, &format!("You'll reach it at http://{}:{}/", local_ip, port));
            }
        }
        warn(
            ui,
            "It is served over plain HTTP. Because the page shows secrets (the password, \
             the API key), it always asks for the username and password — even with \
             \"Require login\" off — and it keeps the private-address restriction whether or \
             not login is required. Only \"Disable IP safety\" lifts that restriction.",
        );
    }

    fn draw_transfer_dir(&mut self, ui: &mut egui::Ui) {
        body(
            ui,
            "Every upload lands in this directory, and every download is offered from it. \
             A relative path is taken from the gateway's working directory.",
        );
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label("Transfer directory:");
            singleline_with_menu(ui, &mut self.transfer_dir, false, Some(240.0));
            let picking = self.pending_dir_pick.is_some();
            if ui
                .add_enabled(!picking, egui::Button::new("Browse..."))
                .clicked()
            {
                self.pending_dir_pick = Some(spawn_folder_picker(&self.transfer_dir));
            }
        });
        ui.add_space(6.0);
        note(
            ui,
            "It is created if it doesn't exist. The CP/M emulator's drives live in a CPM \
             subdirectory of it, and the Gateway Shell treats it as drive A:.",
        );
    }

    fn draw_cpm(&mut self, ui: &mut egui::Ui) {
        body(
            ui,
            "The CP/M emulator is a real CP/M 2.2 environment on an emulated Z80 — it runs \
             genuine .COM software from the menu.",
        );
        ui.add_space(6.0);
        ui.checkbox(&mut self.cpm_enabled, "Enable the CP/M emulator");
        ui.add_enabled_ui(self.cpm_enabled, |ui| {
            ui.checkbox(
                &mut self.cpm_dialout,
                "Give the emulator a virtual modem it can dial out with",
            );
        });
        ui.add_space(6.0);
        note(
            ui,
            &format!(
                "EGT80.COM — our own CP/M terminal program, with XMODEM in both directions — \
                 is placed on drive A: for you ({}/CPM/A). It saves its settings inside its \
                 own .COM file, so it is never overwritten once it's there.",
                self.transfer_dir.trim()
            ),
        );
        ui.add_space(4.0);
        note(
            ui,
            "The virtual modem is what lets CP/M software reach the outside world with AT \
             commands. Clearing it leaves the emulator fully usable but with no way off the \
             machine. Either way the emulator can only touch files under the CPM \
             subdirectory, and a runaway program is stopped by an instruction ceiling.",
        );
    }

    fn draw_role(&mut self, ui: &mut egui::Ui) {
        body(
            ui,
            "Most installations are standalone. The master/slave pair exists to put serial \
             ports where the hardware is, while the files and the menus stay on one machine.",
        );
        ui.add_space(6.0);
        for (value, label, explanation) in [
            (
                "standalone",
                "Standalone server",
                "One gateway, on its own. Its own serial ports, its own transfer directory. \
                 Pick this if you're not sure.",
            ),
            (
                "master",
                "Master",
                "Accepts links from slave gateways elsewhere on the network and presents \
                 their serial ports as if they were local. Transferred files land here. \
                 Needs the SSH server running, because slave links ride the SSH port.",
            ),
            (
                "slave",
                "Slave",
                "Has no menus of its own — it extends its serial ports to a master over SSH. \
                 Put one next to the vintage machine in another room; files land on the \
                 master.",
            ),
        ] {
            ui.radio_value(&mut self.role, value.to_string(), label);
            indent(ui, explanation);
            ui.add_space(4.0);
        }

        // A master with no SSH server is inert — it will accept relay channels
        // that no slave can ever open.  Say so where the choice is made, and
        // offer the fix, rather than silently switching SSH on (which would
        // reopen a port the operator deliberately left closed on the SSH
        // screen) or letting them find out from a startup warning.
        if self.role == "master" && !self.ssh_enabled {
            ui.add_space(2.0);
            warn(
                ui,
                "You turned the SSH server off on step 3, but a master needs it: a slave \
                 links to its master by logging into the master's SSH server, so with SSH \
                 off no slave can ever connect. Nothing else about the master role uses \
                 that port — it is not a second way into the menus beyond the SSH access \
                 you already chose.",
            );
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.add_space(24.0);
                if ui
                    .button(
                        egui::RichText::new(format!(
                            "Enable the SSH server on port {}  (recommended)",
                            self.ssh_port.trim()
                        ))
                        .strong()
                        .color(AMBER_BRIGHT),
                    )
                    .clicked()
                {
                    self.ssh_enabled = true;
                }
            });
            ui.add_space(2.0);
            indent(
                ui,
                "Leave it off and the role is still saved — the gateway will just log the \
                 same warning at every startup until SSH is enabled.",
            );
        }
    }

    fn draw_slave_master(&mut self, ui: &mut egui::Ui) {
        body(
            ui,
            "Tell this slave where its master is. The link is an SSH connection to the \
             master's SSH server, so these credentials must match the username and password \
             configured on the master.",
        );
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label("Master host or IP:");
            singleline_with_menu(ui, &mut self.master_host, false, Some(200.0));
        });
        ui.horizontal(|ui| {
            labeled_field(ui, "Master SSH port:", &mut self.master_port, 70.0);
            ui.label(egui::RichText::new("(default 2222)").italics().color(AMBER_DIM));
        });
        ui.horizontal(|ui| {
            ui.label("Master username:");
            singleline_with_menu(ui, &mut self.master_username, false, Some(180.0));
        });
        let mask = !self.show_passwords;
        ui.horizontal(|ui| {
            ui.label("Master password:");
            singleline_with_menu(ui, &mut self.master_password, mask, Some(180.0));
        });
        ui.checkbox(&mut self.show_passwords, "Show the password I'm typing");
        ui.add_space(6.0);
        note(
            ui,
            "The master must have its SSH server running and \"accept relay connections\" \
             enabled — its own setup wizard offers both when you pick the Master role there.",
        );

        // The mirror of the master's SSH offer — but for the opposite reason,
        // and the difference matters enough to spell out.  The link itself is
        // outbound (this slave logs into the master), so this machine's own SSH
        // server plays no part in it.  What it does buy is a way to administer
        // the slave from your desk, which is worth offering to someone who is,
        // by definition, putting this box in another room.
        if !self.ssh_enabled {
            ui.add_space(6.0);
            body(
                ui,
                "This slave's own SSH server is off. The link to the master does not need \
                 it — the slave dials out to the master's SSH server, so nothing has to \
                 listen here for that.",
            );
            indent(
                ui,
                "It is still worth having: a slave keeps its own menus, so its SSH server \
                 is how you change this machine's settings, or reach the gateway menu on \
                 it, without walking over to it. That is the whole reason to turn it on \
                 here.",
            );
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.add_space(24.0);
                if ui
                    .button(
                        egui::RichText::new(format!(
                            "Enable this slave's SSH server on port {}",
                            self.ssh_port.trim()
                        ))
                        .strong()
                        .color(AMBER_BRIGHT),
                    )
                    .clicked()
                {
                    self.ssh_enabled = true;
                }
            });
            ui.add_space(2.0);
            indent(
                ui,
                "Leave it off and the slave still works exactly as described — you will \
                 just configure it at the machine, or over telnet if you enabled that.",
            );
        }
    }

    fn draw_finish(&mut self, ui: &mut egui::Ui, cfg: &Config, local_ip: &str) {
        body(ui, "Here's what will be saved:");
        ui.add_space(4.0);

        let role_label = match self.role.as_str() {
            "master" => "Master (accepts slave links)",
            "slave" => "Slave (extends its serial ports to a master)",
            _ => "Standalone",
        };
        summary(ui, "Role", role_label);
        summary(ui, "Login user", self.username.trim());
        summary(
            ui,
            "Require login",
            if self.security_enabled { "yes" } else { "no" },
        );
        summary(ui, "Transfer directory", self.transfer_dir.trim());
        summary(
            ui,
            "CP/M emulator",
            match (self.cpm_enabled, self.cpm_dialout) {
                (false, _) => "off",
                (true, true) => "on, with virtual modem dial-out",
                (true, false) => "on, no dial-out",
            },
        );
        if self.role == "slave" {
            summary(
                ui,
                "Master",
                &format!(
                    "{}:{} as {}",
                    self.master_host.trim(),
                    self.master_port.trim(),
                    self.master_username.trim()
                ),
            );
        }

        ui.add_space(8.0);
        note(ui, "How to connect once the server restarts:");
        let mut any = false;
        if self.telnet_enabled {
            if let Some(p) = parse_port(&self.telnet_port) {
                connect_line(ui, &format!("telnet {} {}", local_ip, p));
                any = true;
            }
        }
        // Only offer commands that will actually work: a master with SSH still
        // off has no ssh line, and the role-screen block + the warning below
        // are what tell the operator about it.
        if self.ssh_enabled {
            if let Some(p) = parse_port(&self.ssh_port) {
                connect_line(
                    ui,
                    &format!("ssh -p {} {}@{}", p, self.username.trim(), local_ip),
                );
                any = true;
            }
        }
        if self.web_enabled {
            if let Some(p) = parse_port(&self.web_port) {
                connect_line(ui, &format!("http://{}:{}/", local_ip, p));
                any = true;
            }
        }
        if cfg.kermit_server_enabled {
            connect_line(
                ui,
                &format!(
                    "Kermit server on port {} (already enabled)",
                    cfg.kermit_server_port
                ),
            );
            any = true;
        }
        if !any {
            indent(ui, "No network listeners are enabled.");
        }

        // Firewall: the inbound TCP ports the operator has to allow.  Built
        // from the same answers as the connect lines above so the two can't
        // disagree.
        ui.add_space(8.0);
        let inbound = self.inbound_ports(cfg);
        if inbound.is_empty() {
            note(ui, "Firewall: no inbound ports need to be opened.");
            if self.role == "slave" {
                indent(
                    ui,
                    "A slave only makes an outbound connection to its master, so it needs no \
                     inbound rule of its own — the master's SSH port is the one that must be \
                     reachable.",
                );
            }
        } else {
            note(
                ui,
                "Firewall: allow these inbound TCP ports on this machine (and forward them \
                 only if you really intend to reach the gateway from outside your network):",
            );
            for (port, what) in &inbound {
                connect_line(ui, &format!("TCP {}   — {}", port, what));
            }
            indent(
                ui,
                "All of them are TCP; the gateway listens on no UDP ports. Serial-port \
                 features use no network ports at all.",
            );
            if self.role == "slave" {
                indent(
                    ui,
                    "As a slave this gateway also needs to reach its master's SSH port \
                     outbound.",
                );
            }
        }

        ui.add_space(8.0);
        note(
            ui,
            "Serial ports are not set up here. If you want to use Port A or Port B — as a \
             Hayes modem emulator, a console bridge, or a Kermit server on the wire — enable \
             and configure them in the Serial Port frames of this window, or over telnet in \
             the Serial Configuration menu.",
        );

        let warnings = self.warnings();
        if !warnings.is_empty() {
            ui.add_space(8.0);
            for w in warnings {
                warn(ui, &w);
                ui.add_space(2.0);
            }
        }

        ui.add_space(6.0);
        note(
            ui,
            "Saving restarts the gateway's servers so the new ports take effect. Any settings \
             this wizard didn't ask about keep their current values, and everything here can \
             be changed later from this window, the web page or the telnet menu.",
        );
    }

    /// Collect the folder dialog's answer, if it has one yet.
    fn poll_dir_pick(&mut self) {
        let Some(rx) = &self.pending_dir_pick else { return };
        match rx.try_recv() {
            Ok(Some(path)) => {
                self.transfer_dir = path.display().to_string();
                self.pending_dir_pick = None;
            }
            Ok(None) | Err(TryRecvError::Disconnected) => self.pending_dir_pick = None,
            Err(TryRecvError::Empty) => {}
        }
    }
}

/// The listeners this wizard can turn on.  An enum, not a name string: the
/// collision check has to know which entry is "me", and identity by label would
/// break silently the moment a label is reworded.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Listener {
    Telnet,
    Ssh,
    Web,
}

impl Listener {
    /// Reads as the subject of "The ___ port must be a number...".
    fn subject(self) -> &'static str {
        match self {
            Listener::Telnet => "Telnet",
            Listener::Ssh => "SSH",
            Listener::Web => "web server",
        }
    }

    /// Reads as the object of "...is already used by ___".
    fn label(self) -> &'static str {
        match self {
            Listener::Telnet => "Telnet",
            Listener::Ssh => "SSH",
            Listener::Web => "the web server",
        }
    }
}

fn collision_msg(port: u16, other: &str) -> String {
    format!(
        "Port {} is already used by {} — every listener needs its own port.",
        port, other
    )
}

// ── Type scale ─────────────────────────────────────────────────
//
// The wizard is read once, at arm's length from a console that may be across
// the room, by someone who has not used this program before — so it runs a
// larger scale than the configuration editor, which is a dense grid of controls
// for someone who already knows what they mean.  Every size below is derived
// from BODY so the whole wizard can be re-scaled from one number.

/// Body text: sentences the operator is expected to actually read.
const BODY: f32 = 16.0;
/// Secondary text — indented explanations, notes, warnings, summary rows.
const SMALL: f32 = 15.0;
/// The screen heading under the window title.
const HEADING: f32 = 18.0;
/// Widget text (checkbox and radio labels, buttons, text fields).  Set on the
/// wizard's own `Ui` style so egui's widgets scale with the prose instead of
/// staying at the editor's default.
const WIDGET: f32 = 16.0;

/// Raise the text styles for everything drawn inside the wizard's `Ui`.  Child
/// `Ui`s (the scroll area, every row) inherit this style, so one call covers
/// the whole screen; the editor's own style is untouched.
fn apply_type_scale(ui: &mut egui::Ui) {
    use egui::{FontFamily, FontId, TextStyle};
    let styles = &mut ui.style_mut().text_styles;
    styles.insert(TextStyle::Body, FontId::new(WIDGET, FontFamily::Proportional));
    styles.insert(TextStyle::Button, FontId::new(WIDGET, FontFamily::Proportional));
    styles.insert(TextStyle::Monospace, FontId::new(SMALL, FontFamily::Monospace));
}

/// Parse a listener port: a number in 1..=65535.  Port 0 is rejected because
/// it means "any free port" to the OS, which is never what an operator wants
/// for a service they have to connect back to.
fn parse_port(s: &str) -> Option<u16> {
    s.trim().parse::<u16>().ok().filter(|p| *p >= 1)
}

// ── Small text helpers, so every screen reads the same ─────────

fn body(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).color(TEXT_PRIMARY).size(BODY));
}

fn note(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).color(SCRIPTURE).size(SMALL));
}

fn warn(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(format!("⚠  {}", text)).color(AMBER_BRIGHT).size(SMALL));
}

/// An indented, *wrapping* paragraph.  The wrap is the point: a plain label in
/// a `horizontal` layout is laid out against infinite width, so a long
/// explanation runs past the window edge and — worse — widens the whole scroll
/// area, which pushes the right-aligned Back/Next buttons out of view.
/// Allocating an explicit width for a nested top-down layout is the same
/// pattern the main editor uses for its half-width frames.
fn wrapped_at(ui: &mut egui::Ui, indent: f32, text: egui::RichText) {
    ui.horizontal(|ui| {
        ui.add_space(indent);
        let w = ui.available_width().max(120.0);
        ui.allocate_ui_with_layout(
            egui::vec2(w, 0.0),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                ui.label(text);
            },
        );
    });
}

fn indent(ui: &mut egui::Ui, text: &str) {
    wrapped_at(
        ui,
        24.0,
        egui::RichText::new(text).color(TEXT_PRIMARY).size(SMALL),
    );
}

fn bullet(ui: &mut egui::Ui, text: &str) {
    wrapped_at(
        ui,
        12.0,
        egui::RichText::new(format!("• {}", text))
            .color(TEXT_PRIMARY)
            .size(SMALL),
    );
}

fn summary(ui: &mut egui::Ui, label: &str, value: &str) {
    // horizontal_top, not horizontal: the value below sits in a nested top-down
    // layout, and centre alignment would drop it half a line below its label.
    ui.horizontal_top(|ui| {
        ui.add_space(12.0);
        ui.label(egui::RichText::new(format!("{}:", label)).color(AMBER).size(SMALL));
        // The value can be long (a full filesystem path), so give it the rest of
        // the row as a wrapping block rather than letting it run off the edge.
        let w = ui.available_width().max(120.0);
        ui.allocate_ui_with_layout(
            egui::vec2(w, 0.0),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                ui.label(
                    egui::RichText::new(value)
                        .color(TEXT_PRIMARY)
                        .monospace()
                        .size(SMALL),
                );
            },
        );
    });
}

fn connect_line(ui: &mut egui::Ui, text: &str) {
    ui.horizontal(|ui| {
        ui.add_space(24.0);
        ui.label(egui::RichText::new(text).color(AMBER_BRIGHT).monospace().size(BODY));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wiz() -> Wizard {
        let mut w = Wizard::new(&Config::default());
        w.password = "hunter2".into();
        w.password_confirm = "hunter2".into();
        w
    }

    #[test]
    fn test_parse_port_rejects_zero_and_junk() {
        assert_eq!(parse_port("2323"), Some(2323));
        assert_eq!(parse_port("  2222 "), Some(2222));
        assert_eq!(parse_port("0"), None);
        assert_eq!(parse_port("65536"), None);
        assert_eq!(parse_port("-1"), None);
        assert_eq!(parse_port(""), None);
        assert_eq!(parse_port("80x"), None);
    }

    #[test]
    fn test_credentials_validation() {
        let cfg = Config::default();
        let mut w = wiz();
        w.step = Step::Credentials;
        assert!(w.validate(&cfg).is_ok());

        w.password_confirm = "hunter3".into();
        assert!(w.validate(&cfg).unwrap_err().contains("do not match"));

        w.password_confirm = w.password.clone();
        w.password = String::new();
        w.password_confirm = String::new();
        assert!(w.validate(&cfg).unwrap_err().contains("cannot be empty"));

        let mut w = wiz();
        w.step = Step::Credentials;
        w.username = "  ".into();
        assert!(w.validate(&cfg).is_err());
        w.username = "two words".into();
        assert!(w.validate(&cfg).unwrap_err().contains("spaces"));
    }

    #[test]
    fn test_port_collision_is_rejected_only_when_enabled() {
        let cfg = Config::default();
        let mut w = wiz();
        w.step = Step::Ssh;
        w.ssh_enabled = true;
        w.ssh_port = w.telnet_port.clone(); // both 2323
        assert!(w.validate(&cfg).unwrap_err().contains("already used by Telnet"));

        // Same clash, but telnet is off, so its port is inert.
        w.telnet_enabled = false;
        assert!(w.validate(&cfg).is_ok());

        // A disabled listener's own port isn't validated at all.
        w.ssh_enabled = false;
        w.ssh_port = "not-a-port".into();
        assert!(w.validate(&cfg).is_ok());
    }

    #[test]
    fn test_kermit_server_port_participates_in_collision_check() {
        let mut cfg = Config { kermit_server_enabled: true, ..Config::default() };
        let mut w = wiz();
        w.step = Step::Telnet;
        w.telnet_port = cfg.kermit_server_port.to_string();
        assert!(w
            .validate(&cfg)
            .unwrap_err()
            .contains("already used by the Kermit server"));

        cfg.kermit_server_enabled = false;
        assert!(w.validate(&cfg).is_ok());
    }

    #[test]
    fn test_slave_screen_is_skipped_unless_slave() {
        let mut w = wiz();
        w.step = Step::Role;
        w.role = "standalone".into();
        assert!(matches!(w.neighbour(true), Some(Step::Finish)));
        w.role = "slave".into();
        assert!(matches!(w.neighbour(true), Some(Step::SlaveMaster)));

        // ...and Back from the end mirrors it.
        w.step = Step::Finish;
        assert!(matches!(w.neighbour(false), Some(Step::SlaveMaster)));
        w.role = "master".into();
        assert!(matches!(w.neighbour(false), Some(Step::Role)));
    }

    #[test]
    fn test_step_numbering_counts_the_screens_actually_shown() {
        let mut w = wiz();
        w.role = "standalone".into();
        w.step = Step::Credentials;
        assert_eq!(w.step_title(), "Step 1 of 8 — Username and password");
        w.step = Step::Role;
        assert_eq!(w.step_title(), "Step 8 of 8 — Gateway role");

        // The slave path has one screen more, and both it and Role must carry
        // distinct numbers out of the larger total.
        w.role = "slave".into();
        w.step = Step::Role;
        assert_eq!(w.step_title(), "Step 8 of 9 — Gateway role");
        w.step = Step::SlaveMaster;
        assert_eq!(w.step_title(), "Step 9 of 9 — Master connection (slave)");

        // The bookend screens aren't numbered.
        w.step = Step::Welcome;
        assert_eq!(w.step_title(), "Welcome");
        w.step = Step::Finish;
        assert_eq!(w.step_title(), "Review and finish");
    }

    /// Each listener has to be excluded from its own collision check — an
    /// identity test done by user-facing label (as this once was) breaks the
    /// moment a label is reworded, and every port would report clashing with
    /// itself.
    #[test]
    fn test_each_listener_validates_against_its_own_default_port() {
        let cfg = Config::default();
        for (step, label) in [
            (Step::Telnet, "telnet"),
            (Step::Ssh, "ssh"),
            (Step::Web, "web"),
        ] {
            let mut w = wiz();
            w.telnet_enabled = true;
            w.ssh_enabled = true;
            w.web_enabled = true;
            w.step = step;
            assert!(
                w.validate(&cfg).is_ok(),
                "{} rejected its own default port: {:?}",
                label,
                w.validate(&cfg)
            );
        }
    }

    #[test]
    fn test_unparseable_port_is_reported_for_the_right_listener() {
        let cfg = Config::default();
        let mut w = wiz();
        w.step = Step::Web;
        w.web_enabled = true;
        w.web_port = "eighty".into();
        assert_eq!(
            w.validate(&cfg).unwrap_err(),
            "The web server port must be a number from 1 to 65535."
        );
    }

    #[test]
    fn test_navigation_ends_are_bounded() {
        let mut w = wiz();
        w.step = Step::Welcome;
        assert!(w.neighbour(false).is_none());
        w.step = Step::Finish;
        assert!(w.neighbour(true).is_none());
    }

    #[test]
    fn test_apply_writes_every_answer_and_marks_completed() {
        let mut w = wiz();
        w.username = " bob ".into();
        w.telnet_enabled = true;
        w.telnet_port = "2400".into();
        w.ssh_enabled = true;
        w.ssh_port = "2201".into();
        w.security_enabled = true;
        w.disable_ip_safety = true;
        w.disable_gateway_connections = true;
        w.web_enabled = true;
        w.web_port = "8081".into();
        w.transfer_dir = "/srv/files".into();
        w.cpm_enabled = true;
        w.cpm_dialout = true;
        w.role = "standalone".into();

        let mut cfg = Config::default();
        assert!(!cfg.setup_wizard_completed);
        w.apply_to(&mut cfg);

        assert_eq!(cfg.username, "bob"); // trimmed
        assert_eq!(cfg.password, "hunter2");
        assert!(cfg.telnet_enabled && cfg.telnet_port == 2400);
        assert!(cfg.ssh_enabled && cfg.ssh_port == 2201);
        assert!(cfg.security_enabled);
        assert!(cfg.disable_ip_safety);
        assert!(cfg.disable_gateway_connections);
        assert!(cfg.web_enabled && cfg.web_port == 8081);
        assert_eq!(cfg.transfer_dir, "/srv/files");
        assert!(cfg.cpm_emu_enabled);
        assert_eq!(cfg.gateway_role, "standalone");
        assert!(cfg.setup_wizard_completed);
    }

    #[test]
    fn test_apply_dialout_toggle_maps_to_uart_key() {
        let mut w = wiz();
        let mut cfg = Config::default();

        w.cpm_dialout = false;
        w.apply_to(&mut cfg);
        assert_eq!(cfg.cpm_emu_uart, "off");

        // Off -> on restores the default profile...
        w.cpm_dialout = true;
        w.apply_to(&mut cfg);
        assert_eq!(cfg.cpm_emu_uart, crate::cpm::uart::DEFAULT_UART);

        // ...but an operator's existing non-default profile survives.
        cfg.cpm_emu_uart = "hbios_1".into();
        w.apply_to(&mut cfg);
        assert_eq!(cfg.cpm_emu_uart, "hbios_1");
    }

    #[test]
    fn test_slave_role_writes_master_details() {
        let mut w = wiz();
        w.role = "slave".into();
        w.master_host = " 192.168.1.5 ".into();
        w.master_port = "2222".into();
        w.master_username = " admin ".into();
        w.master_password = "secret".into();
        let mut cfg = Config::default();
        w.apply_to(&mut cfg);
        assert_eq!(cfg.gateway_role, "slave");
        assert_eq!(cfg.slave_master_host, "192.168.1.5");
        assert_eq!(cfg.slave_master_port, 2222);
        assert_eq!(cfg.slave_master_username, "admin");
        assert_eq!(cfg.slave_master_password, "secret");
        // A slave must not silently become a relay-accepting master.
        assert!(!cfg.master_accept_relays);
        // Nor is its own SSH server switched on for it: the relay link is
        // outbound (this slave logs into the master), so nothing needs to
        // listen here.  The slave screen offers it as a convenience for
        // administering the box, and that offer is the operator's to take.
        assert!(!cfg.ssh_enabled);
    }

    #[test]
    fn test_apply_keeps_transfer_dir_when_left_blank() {
        let mut w = wiz();
        w.transfer_dir = "   ".into();
        let mut cfg = Config::default();
        let before = cfg.transfer_dir.clone();
        w.apply_to(&mut cfg);
        assert_eq!(cfg.transfer_dir, before);
    }

    #[test]
    fn test_warnings_fire_on_the_risky_combinations() {
        let mut w = wiz();
        w.telnet_enabled = false;
        w.ssh_enabled = false;
        assert!(w.warnings().iter().any(|s| s.contains("no network logins")));

        let mut w = wiz();
        w.disable_ip_safety = true;
        w.security_enabled = false;
        assert!(w.warnings().iter().any(|s| s.contains("unauthenticated")));
        w.security_enabled = true;
        assert!(!w.warnings().iter().any(|s| s.contains("unauthenticated")));

        let mut w = wiz();
        w.telnet_enabled = true;
        w.telnet_port = "23".into();
        assert!(w.warnings().iter().any(|s| s.contains("require running as root")));

        let mut w = wiz();
        w.role = "master".into();
        w.ssh_enabled = false;
        assert!(w.warnings().iter().any(|s| s.contains("NO slave")));
        w.ssh_enabled = true;
        assert!(!w.warnings().iter().any(|s| s.contains("NO slave")));
    }

    /// The review screen must not print a command that cannot work.
    #[test]
    fn test_master_with_ssh_off_advertises_no_ssh_anywhere() {
        let mut w = wiz();
        w.role = "master".into();
        w.telnet_enabled = false;
        w.web_enabled = false;
        w.ssh_enabled = false;
        // inbound_ports is the firewall list; the connect lines in draw_finish
        // are gated on the same flag, so this is the testable half of the pair.
        assert!(w.inbound_ports(&Config::default()).is_empty());
        w.ssh_enabled = true;
        assert!(!w.inbound_ports(&Config::default()).is_empty());
    }

    #[test]
    fn test_inbound_firewall_ports_track_the_answers() {
        let mut cfg = Config::default();
        let mut w = wiz();
        w.telnet_enabled = true;
        w.telnet_port = "2323".into();
        w.ssh_enabled = false;
        w.web_enabled = false;
        assert_eq!(w.inbound_ports(&cfg), vec![(2323, "telnet server")]);

        // A disabled listener contributes nothing...
        w.telnet_enabled = false;
        assert!(w.inbound_ports(&cfg).is_empty());

        // ...the Kermit server does, even though the wizard never asks about it.
        cfg.kermit_server_enabled = true;
        cfg.kermit_server_port = 2424;
        assert_eq!(
            w.inbound_ports(&cfg),
            vec![(2424, "standalone Kermit server")]
        );

        // A master with SSH still off has no port to advertise — the wizard no
        // longer enables it behind the operator's back.
        let mut w = wiz();
        w.telnet_enabled = false;
        w.web_enabled = false;
        w.ssh_enabled = false;
        w.ssh_port = "2222".into();
        w.role = "master".into();
        assert!(w.inbound_ports(&Config::default()).is_empty());

        // Once SSH is on, the master's port is labelled for what it carries.
        w.ssh_enabled = true;
        let ports = w.inbound_ports(&Config::default());
        assert_eq!(ports.len(), 1);
        assert_eq!(ports[0].0, 2222);
        assert!(ports[0].1.contains("slave links"));

        // A slave needs no inbound rule at all.
        let mut w = wiz();
        w.telnet_enabled = false;
        w.ssh_enabled = false;
        w.web_enabled = false;
        w.role = "slave".into();
        assert!(w.inbound_ports(&Config::default()).is_empty());
    }

    /// A Master needs SSH but is no longer given it silently: the role screen
    /// explains and offers the switch, the review screen warns, and the
    /// operator's choice survives.
    #[test]
    fn test_master_role_never_silently_enables_ssh() {
        let mut w = wiz();
        w.telnet_enabled = false;
        w.ssh_enabled = false;
        w.role = "master".into();

        let warnings = w.warnings();
        assert!(
            warnings.iter().any(|s| s.contains("NO slave")),
            "the review screen must say no slave can connect: {:?}",
            warnings
        );
        // With no listener at all, the both-off warning is also correct now.
        assert!(warnings.iter().any(|s| s.contains("no network logins")));
        // An SSH port nobody will bind is not advertised to the firewall.
        assert!(w.inbound_ports(&Config::default()).is_empty());

        let mut cfg = Config {
            ssh_enabled: false,
            master_accept_relays: false,
            ..Config::default()
        };
        w.apply_to(&mut cfg);
        assert!(cfg.master_accept_relays, "the role's own gate is still armed");
        assert!(
            !cfg.ssh_enabled,
            "the operator turned SSH off; the wizard must not turn it back on"
        );

        // Once they accept the offer, everything follows.
        w.ssh_enabled = true;
        assert!(!w.warnings().iter().any(|s| s.contains("NO slave")));
        let ports = w.inbound_ports(&Config::default());
        assert_eq!(ports.len(), 1);
        assert!(ports[0].1.contains("slave links"));
        w.apply_to(&mut cfg);
        assert!(cfg.ssh_enabled);
    }

    #[test]
    fn test_new_seeds_from_config_and_never_prefills_the_password() {
        let cfg = Config {
            username: "ricky".into(),
            password: "onDisk".into(),
            telnet_port: 9999,
            cpm_emu_uart: "off".into(),
            gateway_role: "slave".into(),
            ..Config::default()
        };
        let w = Wizard::new(&cfg);
        assert_eq!(w.username, "ricky");
        assert!(w.password.is_empty(), "never echo the stored password back");
        assert!(w.password_confirm.is_empty());
        assert_eq!(w.telnet_port, "9999");
        assert!(!w.cpm_dialout);
        assert_eq!(w.role, "slave");
    }
}
