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
    labeled_field, singleline_with_hint, singleline_with_menu, spawn_folder_picker, AMBER, AMBER_BRIGHT, AMBER_DIM,
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
    /// Every listener on one screen: Telnet, SSH, Web, Kermit and FTP, each
    /// with its own Enable box and port (Ricky, 2026-11-22: one table instead
    /// of a screen per server, and the Kermit and FTP servers asked about at
    /// all).
    Servers,
    Security,
    /// The two serial ports: enable, device, baud, with a Refresh.
    Serial,
    TransferDir,
    Cpm,
    /// Optional: where the Weather menu item looks.
    Weather,
    /// Optional: the Groq API key AI Chat needs.
    AiKey,
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
    Step::Servers,
    Step::Security,
    Step::Serial,
    Step::TransferDir,
    Step::Cpm,
    Step::Weather,
    Step::AiKey,
    Step::Role,
    Step::SlaveMaster,
    Step::Finish,
];

/// The background answer to "which add-ons does the repository offer?":
/// `None` until it arrives, then the offers' labels or why there are none.
type AddonLookup = std::sync::Arc<std::sync::Mutex<Option<Result<Vec<String>, String>>>>;

/// Draft state for the whole wizard.  Ports are held as text (like every other
/// numeric field in this GUI) so a half-typed value never becomes a config
/// value; they are parsed on the Next click that leaves their screen.
pub(super) struct Wizard {
    step: Step,
    /// Started on a configuration that has already been through setup, so
    /// the Welcome screen's Exit keeps current settings rather than defaults.
    rerun: bool,
    /// Already a master with its accept-relays gate off: kept off on Finish,
    /// so the review screen says so.
    relays_off_on_master: bool,
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

    kermit_enabled: bool,
    kermit_port: String,
    ftp_enabled: bool,
    ftp_port: String,
    /// `ftp_passive_ports` as typed: `low-high`, one port, or blank.
    ftp_passive: String,

    /// Port A and Port B.
    serial: [SerialDraft; 2],
    /// What the Serial screen's pickers offer; refreshed by its button.
    serial_ports: Vec<crate::serial::DetectedPort>,

    weather_location: String,
    /// A newly typed Groq key.  Blank means keep the stored one -- a stored
    /// secret is never echoed into a field (the master password's rule).
    groq_key: String,
    /// Whether a key is already stored, so the screen can say blank keeps it.
    groq_key_stored: bool,
    show_groq_key: bool,
    /// On the final screen when CP/M is on: open the CP/M setup wizard once
    /// these answers are saved.
    run_cpm_wizard: bool,

    transfer_dir: String,
    /// Folder picker runs on a background thread (same pattern as the main
    /// editor's transfer-dir button); `Some` while a dialog is open.
    pending_dir_pick: Option<Receiver<Option<PathBuf>>>,

    cpm_enabled: bool,
    /// Maps to `cpm_emu_uart`: on = the default UART profile, off = `"off"`.
    cpm_dialout: bool,
    /// Fetch the sample disks once the answers are saved.
    ///
    /// Not a config key and never written to one: the wizard edits a draft, so
    /// there is no settled transfer directory to download into while it is
    /// open.  The editor reads it through [`Wizard::wants_sample_disks`] on
    /// Finish and starts the same download its own button starts.
    cpm_fetch_disks: bool,
    /// Download the Ethernet Gateway add-ons once the answers are saved
    /// (`crate::addons::update::check_and_install`).  Not a config key, like
    /// `cpm_fetch_disks`.
    fetch_addons: bool,
    /// What the repository offers this gateway, looked up in the background
    /// so the box can name it: `None` until [`Wizard::look_up_addons`] is
    /// called, then empty until the answer arrives.  Never in `new`, so a
    /// test that builds a wizard never reaches the network.
    addons_offered: Option<AddonLookup>,

    /// "standalone" | "master" | "slave" — same values as `gateway_role`.
    role: String,
    master_host: String,
    master_port: String,
    master_username: String,
    master_password: String,
    /// How this slave logs into its master as the wizard opened -- the box
    /// above starts empty whatever is stored, so this is what says why.
    master_password_state: crate::relay::MasterPasswordState,
}

/// "34 disks (42 MB) from …" — what the sample-disk offer costs, said before
/// anything is fetched.  Read from the catalogue rather than from the images
/// folder: on a first run the folder does not exist yet, and the transfer
/// directory is still a draft the operator can change on an earlier screen.
fn sample_disk_offer() -> String {
    let all = crate::cpm::fetch::catalogue();
    let mb = all.iter().map(|d| d.bytes).sum::<u64>() as f64 / (1024.0 * 1024.0);
    format!(
        "{} sample disks ({:.0} MB) can be fetched from {} — only the ones this gateway is \
         known to run",
        all.len(),
        mb,
        crate::cpm::fetch::source_repos().join(" and "),
    )
}

impl Wizard {
    /// Whether the operator asked for the sample disks on the CP/M screen.
    /// Read by the editor on Finish, once `apply_to` has settled the transfer
    /// directory the download needs.
    pub(super) fn wants_sample_disks(&self) -> bool {
        self.cpm_enabled && self.cpm_fetch_disks
    }

    /// Seed the draft from the config in effect.  On a true first run that is
    /// the freshly written defaults; on a re-run it is what the operator
    /// already has, so nothing is silently reset.
    pub(super) fn new(cfg: &Config) -> Self {
        Self {
            step: Step::Welcome,
            rerun: cfg.setup_wizard_completed,
            relays_off_on_master: cfg.gateway_role == "master" && !cfg.master_accept_relays,
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
            kermit_enabled: cfg.kermit_server_enabled,
            kermit_port: cfg.kermit_server_port.to_string(),
            ftp_enabled: cfg.ftp_enabled,
            ftp_port: cfg.ftp_port.to_string(),
            ftp_passive: cfg.ftp_passive_ports.clone(),
            serial: [SerialDraft::from(&cfg.serial_a), SerialDraft::from(&cfg.serial_b)],
            serial_ports: super::detect_serial_ports(),
            weather_location: cfg.weather_location.clone(),
            groq_key: String::new(),
            groq_key_stored: !cfg.groq_api_key.trim().is_empty(),
            show_groq_key: false,
            run_cpm_wizard: true,
            transfer_dir: cfg.transfer_dir.clone(),
            pending_dir_pick: None,
            cpm_enabled: cfg.cpm_emu_enabled,
            cpm_dialout: cfg.cpm_emu_uart != "off",
            // Ticked: a CP/M emulator with no disks is the state nearly
            // every new operator has to get themselves out of, and the
            // screen says the count, the size and the repositories above
            // the box, so accepting it is informed rather than silent.
            // Untick it and nothing is fetched.
            cpm_fetch_disks: true,
            fetch_addons: true,
            addons_offered: None,
            role: cfg.gateway_role.clone(),
            master_host: cfg.slave_master_host.clone(),
            master_port: cfg.slave_master_port.to_string(),
            master_username: cfg.slave_master_username.clone(),
            // Never the stored one, for the reason `password` is blank above:
            // a stored password is never echoed into a field, and this box has
            // a "show" toggle.  Blank means unchanged -- `apply_to` writes it
            // only when something was typed.
            master_password: String::new(),
            master_password_state: crate::relay::master_password_state(&cfg.slave_master_password),
        }
    }

    /// Write the finished draft into `cfg`.  Only called from the final
    /// screen's Save button, and only after every screen has validated, so the
    /// port parses here cannot fail — an unexpected one keeps the existing
    /// value rather than substituting a default.
    pub(super) fn apply_to(&self, cfg: &mut Config) {
        cfg.username = self.username.trim().to_string();
        // **Plaintext into the draft, never a hash.**  The draft is the
        // desktop editor's own `cfg`, and Finish saves it through
        // `persist_config`, whose `password_for_save` hashes whatever was
        // typed -- so hashing here as well stored hash(hash(password)) and
        // refused every login after the wizard.  One place hashes.
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
        cfg.kermit_server_enabled = self.kermit_enabled;
        if let Some(p) = parse_port(&self.kermit_port) {
            cfg.kermit_server_port = p;
        }
        cfg.ftp_enabled = self.ftp_enabled;
        if let Some(p) = parse_port(&self.ftp_port) {
            cfg.ftp_port = p;
        }
        if crate::config::parse_passive_ports(&self.ftp_passive).is_some() {
            cfg.ftp_passive_ports = self.ftp_passive.trim().to_string();
        }

        for (draft, port) in self.serial.iter().zip([&mut cfg.serial_a, &mut cfg.serial_b]) {
            port.enabled = draft.enabled;
            port.port = draft.port.clone();
            if let Ok(b) = draft.baud.trim().parse::<u32>()
                && b > 0
            {
                port.baud = b;
            }
        }

        // The cleaned form the Weather screen's check returns (control
        // characters gone), as the telnet Weather screen saves it.
        let loc = self.weather_location.trim();
        cfg.weather_location = if loc.is_empty() {
            String::new()
        } else {
            crate::telnet::weather::validate_weather_location(loc).unwrap_or_else(|_| loc.to_string())
        };
        let key = self.groq_key.trim();
        if !key.is_empty() {
            cfg.groq_api_key = key.to_string();
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

        // Read before it is overwritten: the gate below is armed on the
        // *change* into Master, as the editor (gui.rs) and telnet do.
        let was_master = cfg.gateway_role == "master";
        cfg.gateway_role = self.role.clone();
        match self.role.as_str() {
            // A master exists to accept slaves, so arm the accept-relays gate
            // when the role becomes Master.  A re-run on a master that has the
            // gate deliberately off keeps it off -- the Finish screen promises
            // that settings this wizard didn't ask about keep their values.
            // The SSH server it needs is NOT forced on here:
            // the Role screen explains why a master wants it and offers a
            // button, and the review screen warns if it is still off — an
            // operator who declines keeps their choice (main.rs logs the same
            // warning at startup).
            "master" => {
                if !was_master {
                    cfg.master_accept_relays = true;
                }
            }
            "slave" => {
                cfg.slave_master_host = self.master_host.trim().to_string();
                if let Some(p) = parse_port(&self.master_port) {
                    cfg.slave_master_port = p;
                }
                cfg.slave_master_username = self.master_username.trim().to_string();
                // **Lands in the draft, never in the file.**  `persist_config`
                // routes a non-empty value to `relay::set_pending_master_
                // password` and carries the stored one forward, so the
                // master's password is held in memory for the one login that
                // enrols this slave's key and `egateway.conf` never gains it.
                // One rule there covers this wizard and the editor's own Pass
                // box alike -- see `App::persist_config`.
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

    /// Validate the screen we're leaving.  Every listener is now one of the
    /// wizard's own answers (the Servers screen), so `cfg` is no longer read
    /// for them.
    fn validate(&self, cfg: &Config) -> Result<(), String> {
        match self.step {
            Step::Credentials => {
                if let Some(why) = crate::credential::username_problem(&self.username) {
                    return Err(why.into());
                }
                if self.password.is_empty() {
                    return Err("Password cannot be empty.".into());
                }
                if self.password != self.password_confirm {
                    return Err("The passwords do not match — please retype them.".into());
                }
                Ok(())
            }
            Step::Servers => {
                if self.ftp_enabled && crate::config::parse_passive_ports(&self.ftp_passive).is_none() {
                    return Err("FTP passive ports: use low-high, both 1024-65535, one port, or leave it empty.".into());
                }
                self.check_all_ports(cfg)
            }
            Step::Serial => self.check_serial(),
            Step::Weather => {
                let loc = self.weather_location.trim();
                if !loc.is_empty() {
                    crate::telnet::weather::validate_weather_location(loc)?;
                }
                Ok(())
            }
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
    fn check_port(&self, which: Listener, _cfg: &Config) -> Result<(), String> {
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
        for other in Listener::ALL {
            if other == which {
                continue;
            }
            let (other_enabled, other_value) = self.listener_field(other);
            if other_enabled && parse_port(other_value) == Some(port) {
                return Err(collision_msg(port, other.label()));
            }
        }
        // And FTP's passive data ports: a listener inside that range would make
        // the save at Finish refuse, with the answers left unsaved.
        if self.ftp_enabled
            && let Some(Some(range)) = crate::config::parse_passive_ports(&self.ftp_passive)
            && range.contains(&port)
        {
            return Err(collision_msg(port, "the FTP passive port range"));
        }
        Ok(())
    }

    /// Every listener this configuration brings up, checked as `validate` checks
    /// the screen that set it.  Run once more on Finish, because a listener can
    /// be switched on after its own screen was passed: the role screens offer an
    /// "Enable SSH" button, and an SSH port left in an unticked box was never
    /// checked against the web port chosen after it.
    fn check_all_ports(&self, cfg: &Config) -> Result<(), String> {
        for which in Listener::ALL {
            self.check_port(which, cfg)?;
        }
        Ok(())
    }

    /// The role screens' "Enable SSH" buttons.  Checked like the SSH screen's
    /// Next, so the button cannot turn on a port another listener already holds
    /// -- or one that does not parse; refused, SSH stays off and says why.
    fn enable_ssh(&mut self, cfg: &Config) -> Result<(), String> {
        self.ssh_enabled = true;
        if let Err(e) = self.check_port(Listener::Ssh, cfg) {
            self.ssh_enabled = false;
            return Err(format!("{e} Change the SSH port on the Servers step."));
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
            Listener::Kermit => (self.kermit_enabled, &self.kermit_port),
            Listener::Ftp => (self.ftp_enabled, &self.ftp_port),
        }
    }

    /// The Serial screen's Next: an enabled port needs a device and a baud
    /// rate, and the two ports cannot share one device.
    fn check_serial(&self) -> Result<(), String> {
        for (d, name) in self.serial.iter().zip(["Port A", "Port B"]) {
            if !d.enabled {
                continue;
            }
            if d.port.is_empty() {
                return Err(format!("{name} is enabled: choose its device, or untick it."));
            }
            if !d.baud.trim().parse::<u32>().is_ok_and(|b| b > 0) {
                return Err(format!("{name}'s baud rate must be a number, such as 9600."));
            }
        }
        if self.serial.iter().all(|d| d.enabled) && self.serial[0].port == self.serial[1].port {
            return Err("Port A and Port B cannot use the same device.".into());
        }
        Ok(())
    }

    /// Telnet, SSH and the web page are the three ways in that do not depend
    /// on this window or on a device name staying put.
    fn no_network_config(&self) -> bool {
        !self.telnet_enabled && !self.ssh_enabled && !self.web_enabled
    }

    /// Ask the repository, in the background, which add-ons it offers this
    /// gateway, so the box names them.  **The same signed list as every
    /// Check**, read now rather than pinned into the build: an add-on Ricky
    /// publishes after this release is offered here too (2026-11-24).
    pub(super) fn look_up_addons(&mut self) {
        let slot: AddonLookup = std::sync::Arc::new(std::sync::Mutex::new(None));
        let out = slot.clone();
        std::thread::spawn(move || {
            let found = crate::addons::update::check(&crate::addons::addons_root())
                .map(|offers| offers.iter().map(crate::addons::update::Offer::label).collect());
            *out.lock().unwrap_or_else(|e| e.into_inner()) = Some(found);
        });
        self.addons_offered = Some(slot);
    }

    /// The text under the add-ons box: what the list offers, that it is
    /// still being looked up, or why it could not be.
    fn addons_offer_text(&self) -> (String, bool) {
        let from = crate::addons::fetch::REPO;
        let found = self.addons_offered.as_ref().map(|s| s.lock().unwrap_or_else(|e| e.into_inner()).clone());
        match found {
            Some(Some(Ok(names))) if names.is_empty() => {
                (format!("Every add-on published at github.com/{from} is already here."), false)
            }
            Some(Some(Ok(names))) => (
                format!(
                    "Programs your callers can run from the Second Menu (2, then A): {}. From \
                     github.com/{from}, in a list signed by the gateway's author, each file \
                     checked. An add-on of your own is left alone.",
                    names.join(", ")
                ),
                false,
            ),
            Some(Some(Err(e))) => (
                format!(
                    "Could not get the list of add-ons ({e}). Leave this ticked to try again when \
                     you finish, or install them later from Add-Ons\u{2026} in the General frame."
                ),
                false,
            ),
            // Asked, no answer yet: the caller repaints until there is one.
            Some(None) => (format!("Looking up the add-ons published at github.com/{from}\u{2026}"), true),
            None => (
                format!(
                    "Programs your callers can run from the Second Menu (2, then A), from \
                     github.com/{from}, in a list signed by the gateway's author."
                ),
                false,
            ),
        }
    }

    /// Read by the editor on Finish: download the add-ons after the save.
    pub(super) fn wants_addons(&self) -> bool {
        self.fetch_addons
    }

    /// Read by the editor on Finish: open the CP/M setup wizard next.
    pub(super) fn wants_cpm_wizard(&self) -> bool {
        self.cpm_enabled && self.run_cpm_wizard
    }

    /// The inbound TCP ports this configuration will listen on, with what each
    /// one is for — the firewall list on the final screen, from the Servers
    /// screen's answers.  The SSH port is listed only when the SSH
    /// checkbox is ticked, master or not: `apply_to` deliberately never turns
    /// SSH on for a master, and `warnings` says so instead.
    fn inbound_ports(&self, _cfg: &Config) -> Vec<(u16, &'static str)> {
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
        if self.kermit_enabled {
            if let Some(p) = parse_port(&self.kermit_port) {
                out.push((p, "standalone Kermit server"));
            }
        }
        if self.ftp_enabled {
            if let Some(p) = parse_port(&self.ftp_port) {
                out.push((p, "FTP server"));
            }
        }
        out
    }

    /// Advisory notes for the final screen — things worth saying out loud but
    /// never worth blocking Save over.
    fn warnings(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.no_network_config() {
            out.push(NO_NETWORK_CONFIG_WARNING.into());
        }
        if !self.telnet_enabled && !self.ssh_enabled && !self.no_network_config() {
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
            (self.kermit_enabled, &self.kermit_port, "The Kermit server"),
            (self.ftp_enabled, &self.ftp_port, "The FTP server"),
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
        // A re-run keeps a master's relay gate as it is (see `apply_to`), so
        // one that has it off has to be told -- picking Master here does not
        // turn it back on.
        if self.role == "master" && self.relays_off_on_master {
            out.push(
                "This master is set NOT to accept relay connections from slaves, and \
                 finishing keeps that. Turn on \"Master: accept relay connections from \
                 slaves\" in the editor to let slaves connect."
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
                    Step::Servers => self.draw_servers(ui, local_ip),
                    Step::Security => self.draw_security(ui),
                    Step::Serial => self.draw_serial(ui),
                    Step::TransferDir => self.draw_transfer_dir(ui),
                    Step::Cpm => self.draw_cpm(ui),
                    Step::Weather => self.draw_weather(ui),
                    Step::AiKey => self.draw_ai_key(ui),
                    Step::Role => self.draw_role(ui, cfg),
                    Step::SlaveMaster => self.draw_slave_master(ui, cfg),
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
            // "Use defaults" only on a first run: a re-run starts from the
            // operator's current settings, and Exit keeps those.
            let exit_label = if self.step == Step::Welcome && !self.rerun {
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
                        match self.check_all_ports(cfg) {
                            Ok(()) => outcome = Outcome::Finish,
                            Err(e) => self.error = Some(e),
                        }
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
            Step::Servers => "Servers and ports",
            Step::Security => "Access control",
            Step::Serial => "Serial ports",
            Step::Weather => "Weather location (optional)",
            Step::AiKey => "AI chat key (optional)",
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
             essentials: a login, which servers to start, any serial ports, where transferred \
             files go, and whether this machine stands alone or is part of a master/slave \
             pair — and, if you like, the weather location and an AI chat key.",
        );
        ui.add_space(6.0);
        note(ui, "What's inside, once you're set up:");
        for line in [
            "File transfer — XMODEM, XMODEM-1K, YMODEM, ZMODEM, Kermit and Punter, for \
             everything from a Commodore 64 to a modern terminal.",
            "Gateways — dial out to SSH or telnet hosts, or bridge a real serial port.",
            "Serial modem emulator — Hayes AT commands over a real UART, so vintage \
             terminal software can \"dial\" the internet.",
            "CP/M emulator — runs real Z80 or 8080 .COM software, and ships with our \
             own EGT8080 terminal program, which runs on either.",
            "Gateway Shell — a CP/M-style file manager over the transfer directory.",
            "FTP server — the same transfer directory for any FTP client; this wizard \
             asks whether to turn it on.",
            "Add-Ons — programs your callers run from the Second Menu, like BBS doors; \
             this wizard can download ready-made ones.",
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
            "The password is stored as a PBKDF2 hash in egateway.conf, so it cannot be \
             read back out of the file — note it down if you are also configuring a slave, \
             which needs the password itself. The Groq API key in that file is not hashed, \
             so keep it readable only by you.",
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

    fn draw_servers(&mut self, ui: &mut egui::Ui, local_ip: &str) {
        body(
            ui,
            "The servers this gateway runs, and the port each one listens on. Every \
             server needs its own port.",
        );
        ui.add_space(6.0);
        egui::Grid::new("wizard_servers").num_columns(3).spacing([18.0, 6.0]).show(ui, |ui| {
            for h in ["Server", "Enable", "Port"] {
                ui.label(egui::RichText::new(h).color(AMBER));
            }
            ui.end_row();
            let rows: [(&str, &mut bool, &mut String, u16); 5] = [
                ("Telnet", &mut self.telnet_enabled, &mut self.telnet_port, crate::config::DEFAULT_TELNET_PORT),
                ("SSH", &mut self.ssh_enabled, &mut self.ssh_port, crate::config::DEFAULT_SSH_PORT),
                ("Web configuration", &mut self.web_enabled, &mut self.web_port, crate::config::DEFAULT_WEB_PORT),
                ("Kermit server", &mut self.kermit_enabled, &mut self.kermit_port, crate::config::DEFAULT_KERMIT_SERVER_PORT),
                ("FTP", &mut self.ftp_enabled, &mut self.ftp_port, crate::config::DEFAULT_FTP_PORT),
            ];
            for (name, enabled, port, default) in rows {
                ui.label(name);
                ui.checkbox(enabled, "");
                ui.add_enabled_ui(*enabled, |ui| {
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(port).desired_width(70.0));
                        ui.label(egui::RichText::new(format!("(default {default})")).italics().color(AMBER_DIM));
                    });
                });
                ui.end_row();
            }
        });
        ui.add_enabled_ui(self.ftp_enabled, |ui| {
            ui.horizontal(|ui| {
                labeled_field(ui, "FTP passive ports:", &mut self.ftp_passive, 110.0);
                ui.label(egui::RichText::new("(low-high, or empty for any)").italics().color(AMBER_DIM));
            });
        });
        ui.add_space(6.0);
        indent(
            ui,
            "Telnet — how vintage hardware and retro terminal software reach the gateway; \
             it detects PETSCII, ANSI and ASCII terminals.\n\
             SSH — the same menu, encrypted, always behind the username and password.\n\
             Web configuration — every setting from a browser.\n\
             Kermit server — a Kermit file server on its own port, with no menu.\n\
             FTP — the transfer folder for any FTP client.",
        );
        if self.web_enabled
            && let Some(port) = parse_port(&self.web_port)
        {
            note(ui, &format!("The web page will be at http://{}:{}/", local_ip, port));
        }
        if self.telnet_enabled || self.ftp_enabled {
            warn(
                ui,
                "Telnet and FTP are unencrypted: logins and everything sent cross the network \
                 as plain text. Fine on a trusted LAN, which is what this gateway is built \
                 for; don't expose them to the internet. Use SSH for anything else.",
            );
        }
        if self.no_network_config() {
            ui.add_space(4.0);
            warn(ui, NO_NETWORK_CONFIG_WARNING);
        }
    }

    fn draw_serial(&mut self, ui: &mut egui::Ui) {
        body(
            ui,
            "Optional: a serial port lets a vintage computer or terminal dial into the \
             gateway through its Hayes modem emulator. Leave both off if you have none.",
        );
        ui.add_space(6.0);
        if ui.button("Refresh").on_hover_text("Look again for serial devices — after plugging one in.").clicked() {
            self.serial_ports = super::detect_serial_ports();
        }
        ui.add_space(4.0);
        let ports = self.serial_ports.clone();
        egui::Grid::new("wizard_serial").num_columns(4).spacing([14.0, 6.0]).show(ui, |ui| {
            for h in ["Port", "Enable", "Device", "Baud"] {
                ui.label(egui::RichText::new(h).color(AMBER));
            }
            ui.end_row();
            for (i, name) in ["Port A", "Port B"].into_iter().enumerate() {
                let d = &mut self.serial[i];
                ui.label(name);
                ui.checkbox(&mut d.enabled, "");
                let shown = if d.port.is_empty() {
                    "(none)".to_string()
                } else {
                    match ports.iter().find(|p| p.name == d.port && !p.summary.is_empty()) {
                        Some(p) => format!("{}  \u{2014} {}", p.name, p.summary),
                        None => d.port.clone(),
                    }
                };
                let before = d.port.clone();
                egui::ComboBox::from_id_salt(format!("wizard_serial_{i}"))
                    .width(240.0)
                    .selected_text(shown)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut d.port, String::new(), "(none)");
                        for p in &ports {
                            let label = if p.summary.is_empty() {
                                p.name.clone()
                            } else {
                                format!("{}  \u{2014} {}", p.name, p.summary)
                            };
                            ui.selectable_value(&mut d.port, p.name.clone(), label).on_hover_text(&p.detail);
                        }
                    });
                // Choosing a device is wanting the port, as in the main window.
                if d.port != before && !d.port.is_empty() {
                    d.enabled = true;
                }
                ui.add(egui::TextEdit::singleline(&mut d.baud).desired_width(70.0));
                ui.end_row();
            }
        });
        if ports.is_empty() {
            ui.add_space(4.0);
            note(ui, "No serial devices were found. Plug one in and press Refresh.");
        }
        ui.add_space(6.0);
        indent(
            ui,
            "A device's name can change — a USB adapter on another socket, or after a \
             reboot — so a serial port is not a dependable way to reach the settings.",
        );
    }

    fn draw_weather(&mut self, ui: &mut egui::Ui) {
        body(
            ui,
            "Optional: the place the Weather menu item reports on. A city or a postal code, \
             anywhere in the world.",
        );
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            labeled_field(ui, "Location:", &mut self.weather_location, 260.0);
        });
        ui.add_space(4.0);
        indent(
            ui,
            "For example: Dallas · London, GB · 75201 · Paris, FR. Leave it empty to be \
             asked each time. Forecasts come from Open-Meteo, which needs no account.",
        );
    }

    fn draw_ai_key(&mut self, ui: &mut egui::Ui) {
        body(
            ui,
            "Optional: AI Chat answers questions through Groq, which needs an API key. \
             Everything else works without one — skip this if you don't want AI Chat.",
        );
        ui.add_space(6.0);
        note(ui, "To get a key (free):");
        // One row each, the same indent, so the link in the first does not
        // leave a gap before the other two.
        ui.horizontal(|ui| {
            ui.add_space(24.0);
            ui.label("1. Go to");
            ui.hyperlink_to("console.groq.com", "https://console.groq.com");
            ui.label("and sign in (or sign up).");
        });
        for step in [
            "2. Open API Keys and press Create API Key.",
            "3. Copy the key — it starts with gsk_ — and paste it below.",
        ] {
            ui.horizontal(|ui| {
                ui.add_space(24.0);
                ui.label(step);
            });
        }
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.label("Groq API key:");
            ui.add(
                egui::TextEdit::singleline(&mut self.groq_key)
                    .password(!self.show_groq_key)
                    .desired_width(300.0),
            );
            ui.checkbox(&mut self.show_groq_key, "show");
        });
        if self.groq_key_stored {
            indent(ui, "A key is already set. Leave this empty to keep it.");
        }
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
        ui.add_enabled_ui(self.cpm_enabled, |ui| {
            ui.checkbox(
                &mut self.cpm_fetch_disks,
                "Download the sample CP/M disks when I finish",
            );
        });
        note(
            ui,
            &format!(
                "{}. They are not ours — this fetches them for you when you finish, into \
                 {}/CPM/images, and anything already there is left alone. You can do it \
                 later instead from Mount CP/M Drives. A disk you go on to BOOT may write \
                 to these images, which is how a vintage operating system saves your work; \
                 untick \"a booted disk may WRITE to its images\" under More... on the \
                 AI Chat, Browser, Weather & CP/M panel if you would rather they never \
                 changed.",
                sample_disk_offer(),
                self.transfer_dir.trim(),
            ),
        );
        ui.add_space(6.0);
        note(
            ui,
            &format!(
                "EGT8080.COM — our own CP/M terminal program, with XMODEM in both \
                 directions — is placed on drive A: for you ({}/CPM/A). It runs on either \
                 processor. It saves its settings inside its own .COM file, so it is never \
                 overwritten once it's there.",
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

    fn draw_role(&mut self, ui: &mut egui::Ui, cfg: &Config) {
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
                "The SSH server is off (the Servers step), but a master needs it: a slave \
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
                    self.error = self.enable_ssh(cfg).err();
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

    fn draw_slave_master(&mut self, ui: &mut egui::Ui, cfg: &Config) {
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
            ui.label(
                egui::RichText::new(format!("(default {})", crate::config::DEFAULT_SLAVE_MASTER_PORT))
                    .italics()
                    .color(AMBER_DIM),
            );
        });
        ui.horizontal(|ui| {
            ui.label("Master username:");
            singleline_with_menu(ui, &mut self.master_username, false, Some(180.0));
        });
        let mask = !self.show_passwords;
        ui.horizontal(|ui| {
            ui.label("Master password:");
            // The editor's Pass-box placeholder, for the editor's reason: a
            // blank box with nothing in it reads as a setting nobody filled in.
            singleline_with_hint(
                ui,
                &mut self.master_password,
                mask,
                Some(180.0),
                self.master_password_state.label(),
            );
        });
        if let Some(why) = master_password_note(self.master_password_state) {
            indent(ui, why);
        }
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
                    self.error = self.enable_ssh(cfg).err();
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
        if self.cpm_enabled && self.cpm_fetch_disks {
            summary(ui, "Sample disks", "download when this finishes");
        }
        if self.fetch_addons {
            summary(ui, "Add-ons", "download when this finishes");
        }
        for (d, name) in self.serial.iter().zip(["Serial Port A", "Serial Port B"]) {
            if d.enabled {
                summary(ui, name, &format!("{} at {} baud", d.port, d.baud.trim()));
            }
        }
        let loc = self.weather_location.trim();
        summary(ui, "Weather location", if loc.is_empty() { "ask each time" } else { loc });
        summary(
            ui,
            "AI chat key",
            if !self.groq_key.trim().is_empty() {
                "entered"
            } else if self.groq_key_stored {
                "kept"
            } else {
                "none"
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
        // From the Servers screen's answers, as the lines above are, not the
        // stored config: a port changed on that screen is the one to dial.
        if self.kermit_enabled
            && let Some(p) = parse_port(&self.kermit_port)
        {
            connect_line(ui, &format!("Kermit server on port {p}"));
            any = true;
        }
        if self.ftp_enabled
            && let Some(p) = parse_port(&self.ftp_port)
        {
            connect_line(ui, &format!("ftp {} {}", local_ip, p));
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
            // The passive data ports are inbound too; a client's PASV
            // connection fails without them.
            let mut answers = cfg.clone();
            self.apply_to(&mut answers);
            if let Some(line) = passive_firewall_line(&answers) {
                connect_line(ui, &line);
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
            "A new serial port starts as a Hayes modem emulator. Its mode, flow control and \
             the rest are in the Serial Port frames of this window, or over telnet in the \
             Serial Configuration menu.",
        );
        ui.add_space(8.0);
        ui.checkbox(&mut self.fetch_addons, "Download the Ethernet Gateway add-ons");
        let (offer, waiting) = self.addons_offer_text();
        indent(ui, &offer);
        if waiting {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
        }
        if self.cpm_enabled {
            ui.add_space(8.0);
            ui.checkbox(&mut self.run_cpm_wizard, "Run the CP/M setup wizard next");
            indent(
                ui,
                "It asks what CP/M runs, the disk to boot and the machine to boot it on, \
                 and mounts the disks to go with it. You can run it later from the CP/M \
                 panel of this window.",
            );
        }

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
    Kermit,
    Ftp,
}

impl Listener {
    /// Every listener the Servers screen sets -- the five `config::listeners`.
    const ALL: [Listener; 5] = [Listener::Telnet, Listener::Ssh, Listener::Web, Listener::Kermit, Listener::Ftp];

    /// Reads as the subject of "The ___ port must be a number...".
    fn subject(self) -> &'static str {
        match self {
            Listener::Telnet => "Telnet",
            Listener::Ssh => "SSH",
            Listener::Web => "web server",
            Listener::Kermit => "Kermit server",
            Listener::Ftp => "FTP",
        }
    }

    /// Reads as the object of "...is already used by ___".
    fn label(self) -> &'static str {
        match self {
            Listener::Telnet => "Telnet",
            Listener::Ssh => "SSH",
            Listener::Web => "the web server",
            Listener::Kermit => "the Kermit server",
            Listener::Ftp => "the FTP server",
        }
    }
}

/// The Servers screen's warning, and the review screen's, when Telnet, SSH and
/// the web page are all off (Ricky, 2026-11-22).
const NO_NETWORK_CONFIG_WARNING: &str =
    "Telnet, SSH and the web page are all off. This window would then be the only way to \
     change settings: if it is ever turned off (Show GUI on Startup), there is no reliable \
     way back in, because serial port assignments can change — a USB adapter can come back \
     as a different device after a reboot or on another socket.";

/// One serial port's answers, as the Serial screen holds them.
#[derive(Clone)]
struct SerialDraft {
    enabled: bool,
    /// Device path; empty is "(none)".
    port: String,
    /// Typed, like every number in this GUI.
    baud: String,
}

impl From<&crate::config::SerialPortConfig> for SerialDraft {
    fn from(c: &crate::config::SerialPortConfig) -> Self {
        SerialDraft { enabled: c.enabled, port: c.port.clone(), baud: c.baud.to_string() }
    }
}

/// What a blank Master password box means, when it does not mean "none":
/// `None` when nothing is stored, where blank really is blank.
fn master_password_note(state: crate::relay::MasterPasswordState) -> Option<&'static str> {
    use crate::relay::MasterPasswordState as S;
    match state {
        S::Stored | S::Entered => Some("A password is already set. Leave this blank to keep it."),
        S::UsingKey => Some("Not needed: this slave already logs in to the master with its key."),
        S::Missing => None,
    }
}

/// The firewall line for the FTP passive data ports, when the server is on and
/// a range is set (with none, the system picks a port and no rule can name it).
fn passive_firewall_line(cfg: &Config) -> Option<String> {
    if !cfg.ftp_enabled {
        return None;
    }
    let range = crate::config::parse_passive_ports(&cfg.ftp_passive_ports)??;
    Some(format!("TCP {}-{}   — FTP passive data ports", range.start(), range.end()))
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

    /// **The wizard's Finish, end to end: the password logs in.**  `apply_to`
    /// writes the draft and Finish saves it through `persist_config`; each was
    /// tested alone, and with both hashing, the stored value was
    /// hash(hash(password)) and every login after the wizard was refused.
    /// Chained here through the same two steps, with a hash-shaped password
    /// so the `$ecret$2024` case is covered on this path too.
    #[test]
    fn test_a_password_set_in_the_wizard_logs_in_after_finish() {
        let _rounds = crate::credential::CheapRounds::new();
        let stored = crate::credential::hash_for_test("old");
        for typed in ["hunter2", "$ecret$2024"] {
            let mut draft = Config::default();
            let mut w = Wizard::new(&draft);
            w.password = typed.into();
            w.password_confirm = typed.into();
            w.apply_to(&mut draft);
            let saved = super::super::password_for_save(&draft.password, &stored);
            assert!(crate::credential::verify(&saved, typed), "{typed:?} cannot log in after Finish");
        }
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
        // The web UI's Basic auth splits at the first colon, so this name
        // could never log in there.
        w.username = "ops:pi".into();
        assert!(w.validate(&cfg).unwrap_err().contains("':'"));
    }

    #[test]
    fn test_port_collision_is_rejected_only_when_enabled() {
        let cfg = Config::default();
        let mut w = wiz();
        w.step = Step::Servers;
        w.ssh_enabled = true;
        w.ssh_port = w.telnet_port.clone(); // both 2323
        assert!(w.validate(&cfg).unwrap_err().contains("already used by SSH"));

        // Same clash, but telnet is off, so its port is inert.
        w.telnet_enabled = false;
        assert!(w.validate(&cfg).is_ok());

        // A disabled listener's own port isn't validated at all.
        w.ssh_enabled = false;
        w.ssh_port = "not-a-port".into();
        assert!(w.validate(&cfg).is_ok());
    }

    /// **The "Enable SSH" buttons are checked, and so is Finish.**  Untick SSH
    /// (port 2222), give the web server 2222 -- legal, SSH is off -- then press
    /// the role screen's "Enable the SSH server on port 2222".  The button used
    /// to set the flag unchecked and Finish did not look again, so the wizard
    /// saved two listeners on one port and one failed to bind after the restart.
    #[test]
    fn test_enabling_ssh_late_cannot_reuse_a_port() {
        let cfg = Config::default();
        let mut w = wiz();
        w.ssh_enabled = false;
        w.ssh_port = "2222".into();
        w.web_enabled = true;
        w.web_port = "2222".into();
        assert!(w.check_all_ports(&cfg).is_ok(), "SSH off: its port is inert");

        let e = w.enable_ssh(&cfg).unwrap_err();
        assert!(e.contains("already used by the web server"), "{e}");
        assert!(!w.ssh_enabled, "a refused button leaves SSH off");

        // Had the flag been set some other way, Finish still catches it.
        w.ssh_enabled = true;
        assert!(w.check_all_ports(&cfg).unwrap_err().contains("already used"));

        // And a port that does not parse is refused, not saved as the old one.
        w.ssh_enabled = false;
        w.web_port = "8080".into();
        w.ssh_port = "junk".into();
        assert!(w.enable_ssh(&cfg).unwrap_err().contains("must be a number"));
        assert!(!w.ssh_enabled);

        w.ssh_port = "2222".into();
        assert!(w.enable_ssh(&cfg).is_ok());
        assert!(w.ssh_enabled);
        assert!(w.check_all_ports(&cfg).is_ok());
    }

    /// The two checks above are wired where they act: Finish runs
    /// `check_all_ports`, and neither "Enable SSH" button sets the flag itself.
    #[test]
    fn test_finish_and_the_ssh_buttons_go_through_the_checks() {
        let src = include_str!("wizard.rs").replace("\r\n", "\n");
        let code = &src[..src.find("#[cfg(test)]").expect("the tests")];
        let nav = &code[code.find("fn draw_nav(").expect("draw_nav")..];
        let nav = &nav[..nav.find("\n    fn ").expect("a following fn")];
        assert!(nav.contains("match self.check_all_ports(cfg) {"), "Finish no longer re-checks the ports");
        assert!(nav.contains("Ok(()) => outcome = Outcome::Finish,"), "Finish is reached another way");
        assert_eq!(nav.matches("Outcome::Finish").count(), 1, "Finish is reached another way");
        assert_eq!(code.matches("self.error = self.enable_ssh(cfg).err();").count(), 2);
        assert_eq!(code.matches("self.ssh_enabled = true;").count(), 1, "only enable_ssh sets it");

        // The Master password box shows the stored state as its placeholder,
        // and the re-run builds the wizard from the saved config, which still
        // holds the password the editor's `cfg` has blanked.
        let slave = &code[code.find("fn draw_slave_master(").expect("draw_slave_master")..];
        let slave = &slave[..slave.find("\n    fn ").expect("a following fn")];
        assert!(slave.contains("self.master_password_state.label(),"), "the box lost its placeholder");
        assert!(slave.contains("master_password_note(self.master_password_state)"), "the note is gone");
        let gui = include_str!("../gui.rs");
        assert!(gui.contains("self.wizard = Some(wizard::Wizard::new(&self.last_synced_cfg));"));
        assert!(!gui.contains("wizard::Wizard::new(&self.cfg)"), "a re-run built from the blanked cfg");
    }

    #[test]
    fn test_kermit_server_port_participates_in_collision_check() {
        let cfg = Config::default();
        let mut w = wiz();
        w.step = Step::Servers;
        w.kermit_enabled = true;
        w.telnet_port = w.kermit_port.clone();
        assert!(w
            .validate(&cfg)
            .unwrap_err()
            .contains("already used by the Kermit server"));

        w.kermit_enabled = false;
        assert!(w.validate(&cfg).is_ok());
    }

    /// **The Servers screen asks about all five listeners** -- Telnet, SSH,
    /// Web, Kermit and FTP, the set `config::listeners` names -- and checks
    /// each port against the other four and FTP's passive range.
    #[test]
    fn test_the_servers_screen_covers_every_listener() {
        let cfg = Config::default();
        assert_eq!(
            Listener::ALL.len(),
            crate::config::listeners(&cfg).len(),
            "a listener the Servers screen does not ask about"
        );
        let mut w = wiz();
        w.step = Step::Servers;
        w.ftp_enabled = true;
        w.web_enabled = true;
        w.ftp_port = w.web_port.clone();
        assert!(w.validate(&cfg).unwrap_err().contains("already used by"), "FTP and Web on one port");
        w.ftp_port = "2121".into();
        w.ftp_passive = "2300-2400".into(); // takes in telnet's 2323
        assert!(w.validate(&cfg).unwrap_err().contains("FTP passive"), "a listener in the passive range");
        w.ftp_passive = "oops".into();
        assert!(w.validate(&cfg).unwrap_err().contains("passive ports"), "a bad range passed");
        w.ftp_passive = "50000-50100".into();
        assert!(w.validate(&cfg).is_ok(), "{:?}", w.validate(&cfg));
        // And they are written.
        w.kermit_enabled = true;
        w.kermit_port = "2525".into();
        let mut out = Config::default();
        w.apply_to(&mut out);
        assert!(out.ftp_enabled && out.kermit_server_enabled);
        assert_eq!((out.ftp_port, out.kermit_server_port), (2121, 2525));
        assert_eq!(out.ftp_passive_ports, "50000-50100");
    }

    /// **All three ways in over the network off is warned about**, on the
    /// Servers screen and the review: with the window ever turned off, a
    /// serial device's name is not a dependable way back in.
    #[test]
    fn test_no_network_configuration_is_warned_about() {
        let mut w = wiz();
        w.telnet_enabled = true;
        w.ssh_enabled = false;
        w.web_enabled = false;
        assert!(!w.no_network_config());
        assert!(!w.warnings().iter().any(|m| m == NO_NETWORK_CONFIG_WARNING));
        w.telnet_enabled = false;
        assert!(w.no_network_config());
        assert!(w.warnings().iter().any(|m| m == NO_NETWORK_CONFIG_WARNING), "not on the review screen");
        w.web_enabled = true;
        assert!(!w.no_network_config(), "the web page alone is a way in");
    }

    /// The Serial screen: an enabled port needs a device and a baud rate, the
    /// two cannot share a device, and the answers are written.
    #[test]
    fn test_the_serial_screen_checks_and_writes_its_ports() {
        let cfg = Config::default();
        let mut w = wiz();
        w.step = Step::Serial;
        w.serial[0] = SerialDraft { enabled: true, port: String::new(), baud: "9600".into() };
        assert!(w.validate(&cfg).unwrap_err().contains("choose its device"));
        w.serial[0].port = "/dev/ttyUSB0".into();
        w.serial[0].baud = "fast".into();
        assert!(w.validate(&cfg).unwrap_err().contains("baud"));
        w.serial[0].baud = "2400".into();
        w.serial[1] = SerialDraft { enabled: true, port: "/dev/ttyUSB0".into(), baud: "9600".into() };
        assert!(w.validate(&cfg).unwrap_err().contains("same device"));
        w.serial[1].port = "/dev/ttyUSB1".into();
        assert!(w.validate(&cfg).is_ok());
        let mut out = Config::default();
        w.apply_to(&mut out);
        assert!(out.serial_a.enabled && out.serial_b.enabled);
        assert_eq!((out.serial_a.port.as_str(), out.serial_a.baud), ("/dev/ttyUSB0", 2400));
        assert_eq!(out.serial_b.port, "/dev/ttyUSB1");
    }

    /// Weather and the Groq key are optional: blank passes, a stored key is
    /// kept when the box is left empty, a typed one is written, and a bad
    /// location is refused as the telnet Weather screen refuses it.
    #[test]
    fn test_weather_and_ai_key_are_optional_and_written() {
        let cfg = Config { groq_api_key: "gsk_stored".into(), ..Config::default() };
        let mut w = Wizard::new(&cfg);
        assert!(w.groq_key.is_empty(), "a stored key was echoed into the box");
        w.step = Step::Weather;
        w.weather_location = String::new();
        assert!(w.validate(&cfg).is_ok(), "an empty location is allowed");
        w.weather_location = "x".repeat(500);
        assert!(w.validate(&cfg).is_err(), "an impossible location passed");
        w.weather_location = "Dallas".into();
        assert!(w.validate(&cfg).is_ok());
        let mut out = cfg.clone();
        w.apply_to(&mut out);
        assert_eq!(out.groq_api_key, "gsk_stored", "an empty box replaced the stored key");
        assert_eq!(out.weather_location, "Dallas");
        w.groq_key = "  gsk_new  ".into();
        w.apply_to(&mut out);
        assert_eq!(out.groq_api_key, "gsk_new");
    }

    /// The review screen's CP/M offer: only with CP/M on, ticked by default.
    #[test]
    fn test_the_cpm_wizard_is_offered_only_with_cpm_on() {
        let mut w = wiz();
        w.cpm_enabled = true;
        assert!(w.wants_cpm_wizard(), "not offered by default");
        w.run_cpm_wizard = false;
        assert!(!w.wants_cpm_wizard());
        w.run_cpm_wizard = true;
        w.cpm_enabled = false;
        assert!(!w.wants_cpm_wizard(), "offered with CP/M off");
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
        assert_eq!(w.step_title(), "Step 1 of 9 — Username and password");
        w.step = Step::Role;
        assert_eq!(w.step_title(), "Step 9 of 9 — Gateway role");

        // The slave path has one screen more, and both it and Role must carry
        // distinct numbers out of the larger total.
        w.role = "slave".into();
        w.step = Step::Role;
        assert_eq!(w.step_title(), "Step 9 of 10 — Gateway role");
        w.step = Step::SlaveMaster;
        assert_eq!(w.step_title(), "Step 10 of 10 — Master connection (slave)");

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
        let mut w = wiz();
        w.telnet_enabled = true;
        w.ssh_enabled = true;
        w.web_enabled = true;
        w.kermit_enabled = true;
        w.ftp_enabled = true;
        w.step = Step::Servers;
        assert!(w.validate(&cfg).is_ok(), "a listener rejected its own default port: {:?}", w.validate(&cfg));
    }

    #[test]
    fn test_unparseable_port_is_reported_for_the_right_listener() {
        let cfg = Config::default();
        let mut w = wiz();
        w.step = Step::Servers;
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
        // Plaintext in the draft: the save that follows hashes it, once.
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
        // Telnet and SSH off with the web page on: no logins, but a way in.
        let mut w = wiz();
        w.telnet_enabled = false;
        w.ssh_enabled = false;
        w.web_enabled = true;
        assert!(w.warnings().iter().any(|s| s.contains("no network logins")));
        // All three off: the stronger warning instead, not both.
        w.web_enabled = false;
        let warnings = w.warnings();
        assert!(warnings.iter().any(|s| s == NO_NETWORK_CONFIG_WARNING), "{warnings:?}");
        assert!(!warnings.iter().any(|s| s.contains("no network logins")), "{warnings:?}");

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

    /// **The FTP passive range is a wizard port too.**  A listener chosen
    /// inside it is refused at its step (Finish would otherwise refuse the
    /// save and leave the answers unsaved), and the final screen's firewall
    /// list names the range -- a PASV connection fails without it.  Neither
    /// applies with FTP off or no range set.
    #[test]
    fn test_the_ftp_passive_range_is_checked_and_listed() {
        let mut cfg = Config { ftp_enabled: true, ftp_passive_ports: "50000-50100".into(), ..Config::default() };
        let mut w = wiz();
        w.ftp_enabled = true;
        w.ftp_passive = "50000-50100".into();
        w.telnet_enabled = true;
        w.telnet_port = "50050".into();
        let err = w.check_port(Listener::Telnet, &cfg).expect_err("a port inside the passive range passed");
        assert!(err.contains("FTP passive"), "{err}");
        w.telnet_port = "50101".into();
        assert!(w.check_port(Listener::Telnet, &cfg).is_ok(), "just outside the range is fine");
        w.telnet_port = "50000".into();
        assert!(w.check_port(Listener::Telnet, &cfg).is_err(), "the range's own ends count");
        assert_eq!(passive_firewall_line(&cfg).as_deref(), Some("TCP 50000-50100   — FTP passive data ports"));

        cfg.ftp_enabled = false;
        w.ftp_enabled = false;
        assert!(w.check_port(Listener::Telnet, &cfg).is_ok(), "FTP off: no range to clash with");
        assert_eq!(passive_firewall_line(&cfg), None);
        cfg.ftp_enabled = true;
        cfg.ftp_passive_ports = String::new();
        assert_eq!(passive_firewall_line(&cfg), None, "no range, no rule to name");
    }

    #[test]
    fn test_inbound_firewall_ports_track_the_answers() {
        let cfg = Config::default();
        let mut w = wiz();
        w.telnet_enabled = true;
        w.telnet_port = "2323".into();
        w.ssh_enabled = false;
        w.web_enabled = false;
        assert_eq!(w.inbound_ports(&cfg), vec![(2323, "telnet server")]);

        // A disabled listener contributes nothing...
        w.telnet_enabled = false;
        assert!(w.inbound_ports(&cfg).is_empty());

        // ...the Kermit server does, now asked about on the Servers screen.
        w.kermit_enabled = true;
        w.kermit_port = "2424".into();
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

    /// The accept-relays gate is armed on the *change* into Master, as the
    /// editor and telnet arm it.  A re-run on a master that turned the gate off
    /// keeps it off: the Finish screen says what the wizard didn't ask about
    /// keeps its value, and this is not something it asks about.
    #[test]
    fn test_a_rerun_on_a_master_keeps_relays_off() {
        let current = Config {
            gateway_role: "master".into(),
            master_accept_relays: false,
            setup_wizard_completed: true,
            ..Config::default()
        };
        let w = Wizard::new(&current);
        assert_eq!(w.role, "master", "a re-run starts from the current role");
        let mut cfg = current.clone();
        w.apply_to(&mut cfg);
        assert!(!cfg.master_accept_relays, "a re-run turned relay acceptance back on");
        assert!(
            w.warnings().iter().any(|s| s.contains("NOT to accept relay connections")),
            "and the review screen says it stays off"
        );
        let on = Wizard::new(&Config { master_accept_relays: true, ..current.clone() });
        assert!(!on.warnings().iter().any(|s| s.contains("NOT to accept relay")), "only when off");

        // Positive control: becoming a master does arm it.
        let mut cfg = Config { master_accept_relays: false, ..Config::default() };
        assert_ne!(cfg.gateway_role, "master");
        w.apply_to(&mut cfg);
        assert!(cfg.master_accept_relays, "the change into Master arms the gate");
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
        // With no listener at all (the web page is off by default), the
        // no-network warning is also correct now.
        assert!(warnings.iter().any(|s| s == NO_NETWORK_CONFIG_WARNING), "{warnings:?}");
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
    fn test_the_sample_disk_offer_is_opt_in_and_dies_with_the_emulator() {
        // Ticked to begin with — an emulator with no disks is the state a
        // new operator would otherwise have to dig themselves out of.
        let mut w = wiz();
        w.cpm_enabled = true;
        assert!(w.wants_sample_disks(), "the offer starts ticked");

        // And it is an offer: unticking it is respected.
        w.cpm_fetch_disks = false;
        assert!(!w.wants_sample_disks());
        w.cpm_fetch_disks = true;

        // Turning the emulator off after ticking the box leaves nothing to
        // download into: the disks are only reachable through it.
        w.cpm_enabled = false;
        assert!(!w.wants_sample_disks());

        // It is an action, not a setting — `apply_to` must leave no trace of
        // it in the config, or an upgrade would re-download every restart.
        // Both sides differ in the tick and in nothing else.
        let mut ticked = wiz();
        ticked.cpm_enabled = true;
        ticked.cpm_fetch_disks = true;
        let mut cfg = Config::default();
        ticked.apply_to(&mut cfg);
        let mut plain = Config::default();
        let mut untick = wiz();
        untick.cpm_enabled = true;
        untick.cpm_fetch_disks = false;
        untick.apply_to(&mut plain);
        assert_eq!(
            cfg, plain,
            "asking for the disks must not change a single config key"
        );
    }

    #[test]
    fn test_the_sample_disk_offer_says_the_count_and_the_size() {
        let offer = sample_disk_offer();
        let n = crate::cpm::fetch::catalogue().len();
        assert!(offer.contains(&n.to_string()), "{offer}");
        assert!(offer.contains("MB"), "{offer}");
        for repo in crate::cpm::fetch::source_repos() {
            assert!(offer.contains(&repo), "{offer} is missing {repo}");
        }
    }

    #[test]
    fn test_new_seeds_from_config_and_never_prefills_the_password() {
        let cfg = Config {
            username: "ricky".into(),
            password: "onDisk".into(),
            telnet_port: 9999,
            cpm_emu_uart: "off".into(),
            gateway_role: "slave".into(),
            slave_master_password: "masterOnDisk".into(),
            ..Config::default()
        };
        let w = Wizard::new(&cfg);
        assert_eq!(w.username, "ricky");
        assert!(w.password.is_empty(), "never echo the stored password back");
        assert!(w.password_confirm.is_empty());
        assert!(w.master_password.is_empty(), "never echo the stored master password back");
        // ...but say one is there, or the blank box reads as "none set".
        assert_ne!(
            w.master_password_state,
            crate::relay::MasterPasswordState::Missing,
            "a stored master password must not read as missing"
        );
        assert!(master_password_note(w.master_password_state).is_some());
        let none = Wizard::new(&Config { gateway_role: "slave".into(), ..Config::default() });
        if !crate::relay::relay_authenticated_by_key() {
            assert_eq!(none.master_password_state, crate::relay::MasterPasswordState::Missing);
            assert_eq!(master_password_note(none.master_password_state), None);
        }
        assert_eq!(w.telnet_port, "9999");
        assert!(!w.cpm_dialout);
        assert_eq!(w.role, "slave");
    }

    /// **The add-ons box names what the repository offers now.**  The
    /// wizard reads the same signed list as every Check, so an add-on
    /// published after this release is named and installed too.  Each state
    /// of the background lookup says something true: looking, the offers by
    /// name, nothing new, or why the list could not be had.  And building a
    /// wizard never starts a lookup -- only the window does -- so no test
    /// reaches the network.
    #[test]
    fn test_the_add_ons_box_names_what_is_published_now() {
        let mut w = Wizard::new(&Config::default());
        assert!(w.addons_offered.is_none(), "a lookup started in new()");
        let set = |w: &mut Wizard, v: Option<Result<Vec<String>, String>>| {
            w.addons_offered = Some(std::sync::Arc::new(std::sync::Mutex::new(v)));
        };
        set(&mut w, None);
        let (text, waiting) = w.addons_offer_text();
        assert!(waiting && text.contains("Looking up"), "{text}");
        set(&mut w, Some(Ok(vec!["guess 1.0 (new)".into(), "mad 1.1 (new)".into()])));
        let (text, waiting) = w.addons_offer_text();
        assert!(!waiting && text.contains("guess 1.0 (new), mad 1.1 (new)") && text.contains("signed"), "{text}");
        set(&mut w, Some(Ok(Vec::new())));
        assert!(w.addons_offer_text().0.contains("already here"));
        set(&mut w, Some(Err("no route to host".into())));
        let (text, _) = w.addons_offer_text();
        assert!(text.contains("no route to host") && text.contains("Add-Ons"), "{text}");

        // Finish installs from the signed list, not a list built in.
        let gui = include_str!("../gui.rs").replace('\r', "");
        let at = gui.find("if std::mem::take(&mut self.wizard_addons_pending) {").expect("the finish step");
        let step = &gui[at..at + gui[at..].find("\n        }\n").expect("its end")];
        assert!(step.contains("crate::addons::update::check_and_install("), "{step}");
        // The window asks for the list where a test's App cannot.
        assert!(gui.contains("if let Some(w) = app.wizard.as_mut() {\n                    w.look_up_addons();"));
    }
}
