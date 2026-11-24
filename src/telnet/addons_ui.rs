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

/// Keys read from the caller and not yet taken by the run (see
/// [`read_caller_keys`]).  The run is this far behind only during a long
/// write to a slow caller with a large paste arriving; the reader then waits.
const ADDON_KEYS_MAX: usize = 8 * 1024;

/// The caller's side of a run, as the key reader last saw it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CallerState {
    Here,
    /// ESC twice, timed as the keys arrived.
    Left,
    /// The connection closed.
    Gone,
}

/// Read the caller's keys for an add-on's run, on a task of its own, and
/// hand the reader back (with its telnet state) when told to stop.
///
/// **Why a task: ESC twice must work while the run is writing.**  The run
/// used to read keys in the same loop that wrote the add-on's output, so an
/// add-on printing without end to a slow caller (a C64 at 1200 baud) kept
/// that loop inside one write for tens of seconds.  The two ESCs then were
/// read far apart, never inside the 500 ms that makes them a pair, and the
/// only way out was to hang up.  Here they are timed as they arrive, and
/// `Left` is raised at once; the run gives up the write it is in.
async fn read_caller_keys(
    mut reader: Box<dyn tokio::io::AsyncRead + Unpin + Send>,
    telnet: bool,
    is_petscii: bool,
    keys: tokio::sync::mpsc::Sender<u8>,
    state: tokio::sync::watch::Sender<CallerState>,
    mut stop: tokio::sync::oneshot::Receiver<()>,
) -> (Box<dyn tokio::io::AsyncRead + Unpin + Send>, InboundTelnet) {
    let mut inbound = InboundTelnet::default();
    let mut esc = EscHold::new();
    loop {
        tokio::select! {
            biased;
            _ = &mut stop => break,
            event = read_gateway_event(&mut *reader, telnet, &mut inbound) => match event {
                Ok(GatewayInboundEvent::Data(b)) => {
                    if is_gateway_leave_key(b, is_petscii) {
                        if esc.press(tokio::time::Instant::now()) {
                            let _ = state.send(CallerState::Left);
                            break;
                        }
                    } else {
                        esc.other();
                    }
                    // Straight in while there is room, which there is but
                    // for a paste of thousands of keys during a long write.
                    // Then this waits rather than drop keys -- a paste must
                    // arrive whole -- and the ESCs go untimed only for that.
                    match keys.try_send(b) {
                        Ok(()) => {}
                        Err(tokio::sync::mpsc::error::TrySendError::Full(b)) => tokio::select! {
                            biased;
                            _ = &mut stop => break,
                            sent = keys.send(b) => if sent.is_err() { break },
                        },
                        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => break,
                    }
                }
                // A pipe has no window to resize.
                Ok(GatewayInboundEvent::NawsResize(..)) => {}
                Ok(GatewayInboundEvent::Eof) | Err(_) => {
                    let _ = state.send(CallerState::Gone);
                    break;
                }
            },
        }
    }
    (reader, inbound)
}

/// How a write to the caller during a run turned out.
#[derive(Debug, PartialEq, Eq)]
enum Sent {
    Done,
    /// The connection failed, or took nothing for the whole stall limit.
    Gone,
    /// The caller pressed ESC twice while it was under way: abandoned.
    Left,
}

/// `fut`, or `None` once `limit` passes; a zero limit is no limit.
async fn within<T>(limit: std::time::Duration, fut: impl std::future::Future<Output = T>) -> Option<T> {
    if limit.is_zero() {
        Some(fut.await)
    } else {
        tokio::time::timeout(limit, fut).await.ok()
    }
}

/// Write `bytes` to the caller, IAC-escaped on a telnet link.
///
/// **A write cannot hold the run.**  With `leave` it is abandoned the moment
/// the caller leaves.  And it fails as `Gone` when the caller has taken
/// nothing for `stall` (the session's idle timeout): a terminal that stopped
/// reading -- paused, or crashed with the connection still open -- otherwise
/// left the run inside the write, where no timer could reach it.  The limit
/// is on progress, not on the whole write, so a slow line that is still
/// taking bytes is never cut off.
async fn send_to_caller(
    writer: &SharedWriter,
    bytes: &[u8],
    telnet: bool,
    stall: std::time::Duration,
    leave: Option<&mut tokio::sync::watch::Receiver<CallerState>>,
) -> Sent {
    let wire: std::borrow::Cow<[u8]> = if telnet && bytes.contains(&IAC) {
        let mut v = Vec::with_capacity(bytes.len() + 8);
        for &b in bytes {
            v.push(b);
            if b == IAC {
                v.push(IAC);
            }
        }
        v.into()
    } else {
        bytes.into()
    };
    let push = async {
        let mut w = writer.lock().await;
        let mut off = 0;
        while off < wire.len() {
            match within(stall, w.write(&wire[off..])).await {
                Some(Ok(n)) if n > 0 => off += n,
                _ => return Sent::Gone,
            }
        }
        match within(stall, w.flush()).await {
            Some(Ok(())) => Sent::Done,
            _ => Sent::Gone,
        }
    };
    match leave {
        None => push.await,
        Some(state) => tokio::select! {
            biased;
            s = async { state.wait_for(|s| *s != CallerState::Here).await.map(|s| *s) } => match s {
                Ok(CallerState::Left) => Sent::Left,
                _ => Sent::Gone,
            },
            sent = push => sent,
        },
    }
}

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
    ///
    /// A window-only add-on is judged on the gateway's screen, not on this
    /// terminal: it draws nothing here.  One with both sides is judged on the
    /// terminal, since that side is what the caller came for; its window is
    /// a bonus that opens when there is somewhere to open it.
    pub(in crate::telnet) fn addon_refusal(&self, m: &Manifest, cols: u16) -> Option<String> {
        Self::addon_refusal_on(self.terminal_type, m, cols, crate::addons::gui::screen().is_some())
    }

    /// [`Self::addon_refusal`] with the gateway's screen as an input, so the
    /// headless case is tested on a machine with a desktop.
    pub(in crate::telnet) fn addon_refusal_on(
        terminal: TerminalType,
        m: &Manifest,
        cols: u16,
        has_screen: bool,
    ) -> Option<String> {
        if !m.mode.has_terminal() {
            return (!has_screen).then(|| "No screen on the gateway for its window.".into());
        }
        let (ok, kind) = match terminal {
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
        if addons.is_empty() {
            rows.push(format!("  {}", self.dim("No add-ons are installed.")));
            rows.push(format!("  {}", self.dim("C checks for new ones.")));
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
            "  {} {} {} {}",
            self.dim("#=Start"),
            self.action_prompt("C", "Check"),
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
            // An empty list is still a page: its C (Check) is how the first
            // add-on arrives.
            let addons = crate::addons::offered();
            let pages = addons.len().div_ceil(Self::ADDONS_SHOWN).max(1);
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
                "c" => self.addons_update_page().await?,
                "n" if pages > 1 => page = (page + 1) % pages,
                "p" if pages > 1 => page = (page + pages - 1) % pages,
                _ => match input.parse::<usize>() {
                    Ok(n) if (1..=addons.len()).contains(&n) => {
                        let a = addons[n - 1].clone();
                        match self.addon_refusal(&a.manifest, cols) {
                            Some(why) => self.show_error(&why).await?,
                            None => self.run_addon(&a).await?,
                        }
                    }
                    Ok(_) => self.show_error("Invalid number.").await?,
                    Err(_) => self.show_error("Enter a number, C, or Q.").await?,
                },
            }
        }
    }

    /// Start `a` as its mode says: its window, its terminal side, or both.
    pub(in crate::telnet) async fn run_addon(&mut self, a: &AddOn) -> Result<(), std::io::Error> {
        let m = &a.manifest;
        match (m.mode.has_gui(), m.mode.has_terminal()) {
            (true, false) => self.run_gui_addon(a).await,
            (true, true) => {
                // The window first, quietly: a caller who came for the
                // terminal side is not stopped by its window being open
                // already or having nowhere to go.
                let env = self.addon_env(a, false);
                if let crate::addons::gui::Launch::Failed(e) =
                    crate::addons::gui::launch(a, &env, &self.addon_who())
                {
                    glog!("Add-on \"{}\" window could not open: {}", m.id, e);
                }
                self.run_terminal_addon(a).await
            }
            _ => self.run_terminal_addon(a).await,
        }
    }

    /// Who started an add-on, for the log.
    fn addon_who(&self) -> String {
        self.peer_addr.map(|a| a.to_string()).unwrap_or_else(|| "a local session".into())
    }

    /// Open a window-only add-on on the gateway's screen and say where it
    /// went: the caller is, by definition, not looking at that screen.
    pub(in crate::telnet) async fn run_gui_addon(&mut self, a: &AddOn) -> Result<(), std::io::Error> {
        use crate::addons::gui::{self, Launch};
        let m = a.manifest.clone();
        let name = self.remote_text(&m.name);
        let env = self.addon_env(a, false);
        let gui::Opened { mut done, screen } = match gui::launch(a, &env, &self.addon_who()) {
            Launch::Opened(o) => o,
            Launch::AlreadyOpen => {
                return self.show_error(&format!("{name} is already open on the gateway's screen.")).await;
            }
            Launch::NoScreen => {
                return self.show_error(&format!("This gateway has no screen to open {name} on.")).await;
            }
            Launch::Failed(e) => {
                glog!("Add-on \"{}\" could not start: {}", m.id, e);
                return self.show_error(&format!("{name} could not start.")).await;
            }
        };
        let host = gui::where_label(&crate::relay::hostname_label(), &screen);
        self.clear_screen().await?;
        // Wrapped: a 32-character host name and a long add-on name do not
        // fit a C64 row together.
        let width = if self.terminal_type == TerminalType::Petscii { PETSCII_WIDTH } else { 80 } - 3;
        // Started is not open: a display that refuses the window ends it
        // within milliseconds, so the caller hears which before being told.
        for row in crate::aichat::wrap_line(&format!("Opening {name}..."), width) {
            self.send_line(&format!("  {row}")).await?;
        }
        self.flush().await?;
        if let Some(closed) = gui::closed_within(&mut done, gui::SETTLE).await {
            let said = if closed.failed() {
                let why = closed.last_line.as_deref().map(|l| format!(": {}", self.remote_text(l))).unwrap_or_default();
                format!("{name} could not open on {host}{why}")
            } else {
                format!("{name} ran on {host} and has closed.")
            };
            self.send_line("").await?;
            for row in crate::aichat::wrap_line(&said, width) {
                let row = if closed.failed() { self.red(&row) } else { self.yellow(&row) };
                self.send_line(&format!("  {row}")).await?;
            }
            self.send_line("").await?;
            self.send("  Press any key to continue.").await?;
            self.flush().await?;
            self.wait_for_key().await?;
            return Ok(());
        }
        self.send_line("").await?;
        for row in crate::aichat::wrap_line(&format!("{name} opened on {host}."), width) {
            self.send_line(&format!("  {}", self.green(&row))).await?;
        }
        if !m.gui_wait {
            self.send_line("").await?;
            self.send("  Press any key to continue.").await?;
            self.flush().await?;
            self.wait_for_key().await?;
            return Ok(());
        }
        let esc_label = if self.terminal_type == TerminalType::Petscii { "<-" } else { "ESC" };
        self.send_line("  Waiting for its window to close.").await?;
        self.send_line(&format!("  Press {} twice to close it.", self.cyan(esc_label))).await?;
        self.flush().await?;
        let is_petscii = self.terminal_type == TerminalType::Petscii;
        let local_telnet = self.speaks_telnet();
        let mut inbound = InboundTelnet::default();
        let mut esc = EscHold::new();
        let closed_by_caller = loop {
            tokio::select! {
                changed = done.changed() => {
                    // A dropped sender is a waiter thread that has gone,
                    // which only happens once the window has.
                    let _ = changed;
                    if done.borrow().is_some() || changed.is_err() { break false; }
                }
                event = read_gateway_event(&mut self.reader, local_telnet, &mut inbound) => {
                    match event {
                        Ok(GatewayInboundEvent::Data(b)) => {
                            if is_gateway_leave_key(b, is_petscii) {
                                if esc.press(tokio::time::Instant::now()) { break true; }
                            } else {
                                esc.other();
                            }
                        }
                        Ok(GatewayInboundEvent::NawsResize(..)) => {}
                        // The window is the gateway's, not the caller's: a
                        // caller who hangs up leaves it open.
                        Ok(GatewayInboundEvent::Eof) | Err(_) => return Ok(()),
                    }
                }
            }
        };
        inbound.finish(&mut *self.reader).await;
        if closed_by_caller {
            gui::close(&m.id);
        }
        let said = if closed_by_caller { format!("Closed {name}.") } else { format!("{name} closed.") };
        self.send_line("").await?;
        self.send_line(&format!("  {}", self.yellow(&said))).await?;
        self.send("  Press any key to continue.").await?;
        self.flush().await?;
        self.wait_for_key().await?;
        Ok(())
    }

    /// Add-ons per page on the settings screen: header 3, blank, the two
    /// setting rows, blank, these, the page line, blank, keys, prompt = 21.
    pub(in crate::telnet) const ADDON_SETTINGS_SHOWN: usize = 10;

    /// Every row of the settings screen, title to key line.  The setting
    /// values are inputs, so the fit test draws the longest of each.
    pub(in crate::telnet) fn addons_settings_rows(
        &self,
        installed: &[AddOn],
        page: usize,
        enabled: bool,
        display: &str,
        disabled: &str,
        has_own_screen: bool,
    ) -> Vec<String> {
        let sep = self.separator();
        let mut rows = vec![sep.clone(), format!("  {}", self.yellow("ADD-ONS SETTINGS")), sep, String::new()];
        let width = if self.terminal_type == TerminalType::Petscii { PETSCII_WIDTH } else { 80 } - 1;
        rows.push(format!("  {}  Offer add-ons: {}", self.cyan("E"), self.amber(if enabled { "Yes" } else { "No" })));
        if cfg!(all(unix, not(target_os = "macos"))) {
            let shown = if display.is_empty() { "(none)".to_string() } else { display.to_string() };
            // On a gateway with a desktop the setting is not used, and the
            // row says so rather than leave a value that changes nothing.
            let unused = if has_own_screen { " (unused)" } else { "" };
            let room = width.saturating_sub("  D  GUI display: ".len() + unused.len());
            rows.push(format!(
                "  {}  GUI display: {}{}",
                self.cyan("D"),
                self.amber(&crate::webbrowser::truncate_to_width(&shown, room)),
                self.dim(unused)
            ));
        }
        rows.push(String::new());
        let off = crate::addons::disabled_ids(disabled);
        let total = installed.len();
        let pages = total.div_ceil(Self::ADDON_SETTINGS_SHOWN).max(1);
        let page = page.min(pages - 1);
        if total == 0 {
            rows.push(format!("  {}", self.dim("No add-ons are installed.")));
        }
        for (i, a) in installed.iter().enumerate().skip(page * Self::ADDON_SETTINGS_SHOWN).take(Self::ADDON_SETTINGS_SHOWN) {
            let on = !off.contains(&a.manifest.id.as_str());
            let prefix = format!("  {:>2}. ", i + 1);
            let state = if on { " On" } else { " Off" };
            let room = width.saturating_sub(prefix.len() + 4);
            let name = crate::webbrowser::truncate_to_width(&self.remote_text(&a.manifest.name), room);
            let pad = room.saturating_sub(name.chars().count());
            let state = if on { self.green(state) } else { self.dim(state) };
            rows.push(format!("{prefix}{name}{}{state}", " ".repeat(pad)));
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
            "  {} {} {} {}",
            self.dim("#=On/Off"),
            self.action_prompt("C", "Check"),
            self.action_prompt("Q", "Back"),
            self.action_prompt("H", "Help"),
        ));
        rows
    }

    /// Configuration > Add-Ons: offer them at all, the display for GUI
    /// add-ons on a gateway with no screen, and each add-on on or off.
    pub(in crate::telnet) async fn addons_settings(&mut self) -> Result<(), std::io::Error> {
        let mut page = 0usize;
        loop {
            let cfg = config::get_config();
            // Every installed add-on, offered or not: this is where one is
            // switched back on.
            let installed = crate::addons::discover();
            let pages = installed.len().div_ceil(Self::ADDON_SETTINGS_SHOWN).max(1);
            page = page.min(pages - 1);
            self.clear_screen().await?;
            let rows =
                self.addons_settings_rows(
                    &installed,
                    page,
                    cfg.addons_enabled,
                    &cfg.addon_display,
                    &cfg.addons_disabled,
                    crate::gui::have_graphical_session(),
                );
            for row in rows {
                self.send_line(&row).await?;
            }
            self.send(&format!("{}> ", self.cyan("gateway/addons"))).await?;
            self.flush().await?;
            let input = match self.get_line_input().await? {
                Some(s) if !s.trim().is_empty() => s.trim().to_ascii_lowercase(),
                _ => return Ok(()),
            };
            match input.as_str() {
                "q" => return Ok(()),
                "h" => self.show_help_page("ADD-ONS SETTINGS HELP", Self::addons_settings_help_lines()).await?,
                "c" => self.addons_update_page().await?,
                "n" if pages > 1 => page = (page + 1) % pages,
                "p" if pages > 1 => page = (page + pages - 1) % pages,
                "e" => {
                    let v = if cfg.addons_enabled { "false" } else { "true" };
                    save_addon_setting("addons_enabled", v).await;
                }
                "d" if cfg!(all(unix, not(target_os = "macos"))) => self.addon_display_prompt(&cfg.addon_display).await?,
                _ => match input.parse::<usize>() {
                    Ok(n) if (1..=installed.len()).contains(&n) => {
                        let id = &installed[n - 1].manifest.id;
                        let on = crate::addons::disabled_ids(&cfg.addons_disabled).contains(&id.as_str());
                        let v = crate::addons::with_switched(&cfg.addons_disabled, id, on);
                        save_addon_setting("addons_disabled", &v).await;
                    }
                    Ok(_) => self.show_error("Invalid number.").await?,
                    Err(_) => {
                        // `D` only where the screen offers it.
                        let keys = if cfg!(all(unix, not(target_os = "macos"))) { "E, D, C" } else { "E, C" };
                        self.show_error(&format!("Press {keys}, a number, H, or Q.")).await?
                    }
                },
            }
        }
    }

    /// Offers listed on the Check screen; the rest are counted.
    pub(in crate::telnet) const ADDON_OFFERS_SHOWN: usize = 10;

    /// Every row of the Check screen once the list has arrived, title to
    /// key line.  `can_install` is whether this session may install.
    pub(in crate::telnet) fn addons_offer_rows(&self, offers: &[crate::addons::update::Offer], can_install: bool) -> Vec<String> {
        let sep = self.separator();
        let mut rows = vec![sep.clone(), format!("  {}", self.yellow("NEW AND UPDATED ADD-ONS")), sep, String::new()];
        let width = if self.terminal_type == TerminalType::Petscii { PETSCII_WIDTH } else { 80 } - 3;
        if offers.is_empty() {
            rows.push(format!("  {}", self.green("Everything is up to date.")));
            return rows;
        }
        for (i, o) in offers.iter().take(Self::ADDON_OFFERS_SHOWN).enumerate() {
            let prefix = format!("{:>2}. ", i + 1);
            let label = crate::webbrowser::truncate_to_width(&self.remote_text(&o.label()), width - prefix.len());
            rows.push(format!("  {prefix}{label}"));
        }
        if offers.len() > Self::ADDON_OFFERS_SHOWN {
            rows.push(format!("  {}", self.dim(&format!("and {} more (A installs all)", offers.len() - Self::ADDON_OFFERS_SHOWN))));
        }
        rows.push(String::new());
        if can_install {
            rows.push(format!(
                "  {} {} {}",
                self.dim("#=Install"),
                self.action_prompt("A", "All"),
                self.action_prompt("Q", "Back"),
            ));
        } else {
            for row in crate::aichat::wrap_line(
                "Installing needs a logged-in session: the operator can install them from the web or desktop settings.",
                width,
            ) {
                rows.push(format!("  {}", self.dim(&row)));
            }
        }
        rows
    }

    /// Check for new and updated add-ons, and install them when this session
    /// may.  **Anyone may look; only a session that proved who it is may
    /// install** -- an add-on is a program that runs on this machine, and a
    /// gateway with security off would otherwise let any caller choose what
    /// it runs (Ricky, 2026-11-23).
    pub(in crate::telnet) async fn addons_update_page(&mut self) -> Result<(), std::io::Error> {
        self.clear_screen().await?;
        self.send_line(&format!("  {}", self.cyan("Checking for add-ons..."))).await?;
        self.flush().await?;
        let root = crate::addons::addons_root();
        let checked = {
            let root = root.clone();
            tokio::task::spawn_blocking(move || crate::addons::update::check(&root))
                .await
                .unwrap_or_else(|e| Err(e.to_string()))
        };
        let offers = match checked {
            Ok(o) => o,
            Err(e) => {
                glog!("Add-on check failed: {}", e);
                return self.show_error("Could not check for add-ons.").await;
            }
        };
        let can_install = self.authenticated;
        self.clear_screen().await?;
        for row in self.addons_offer_rows(&offers, can_install) {
            self.send_line(&row).await?;
        }
        if offers.is_empty() || !can_install {
            self.send_line("").await?;
            self.send("  Press any key to continue.").await?;
            self.flush().await?;
            return self.wait_for_key().await;
        }
        self.send(&format!("{}> ", self.cyan("gateway/addons"))).await?;
        self.flush().await?;
        let input = match self.get_line_input().await? {
            Some(s) if !s.trim().is_empty() => s.trim().to_ascii_lowercase(),
            _ => return Ok(()),
        };
        let chosen: Vec<crate::addons::update::Offer> = match input.as_str() {
            "q" => return Ok(()),
            "a" => offers.clone(),
            other => match other.parse::<usize>() {
                Ok(n) if (1..=offers.len().min(Self::ADDON_OFFERS_SHOWN)).contains(&n) => vec![offers[n - 1].clone()],
                _ => return self.show_error("Not one of the numbers shown.").await,
            },
        };
        self.send_line(&format!("  {}", self.cyan("Installing..."))).await?;
        self.flush().await?;
        glog!("Add-on install asked for by {}", self.addon_who());
        let report = tokio::task::spawn_blocking(move || crate::addons::update::install(&root, &chosen))
            .await
            .unwrap_or_default();
        self.send_line("").await?;
        let width = if self.terminal_type == TerminalType::Petscii { PETSCII_WIDTH } else { 80 } - 3;
        for o in &report.fetched {
            self.send_line(&format!("  {}", self.green(&format!("Installed {}", self.remote_text(o))))).await?;
        }
        for (id, why) in &report.failed {
            for row in crate::aichat::wrap_line(&format!("{id} failed: {why}"), width) {
                self.send_line(&format!("  {}", self.red(&self.remote_text(&row)))).await?;
            }
        }
        self.send_line("").await?;
        self.send("  Press any key to continue.").await?;
        self.flush().await?;
        self.wait_for_key().await
    }

    /// The GUI display screen above its prompt: what the setting is for,
    /// both kinds of value -- this computer's screen and another computer's
    /// X server -- and, on a gateway with a desktop, that it is not used
    /// there at all.  Fits a C64 with the prompt row.
    pub(in crate::telnet) fn addon_display_rows(&self, current: &str, has_own_screen: bool) -> Vec<String> {
        let sep = self.separator();
        let mut rows = vec![sep.clone(), format!("  {}", self.yellow("GUI DISPLAY")), sep, String::new()];
        for row in [
            "  Where GUI add-ons open when this",
            "  gateway has no desktop of its own",
            "  (a service, or started over SSH):",
            "  an X display.",
            "",
            "  :0  this computer's own screen",
            "  192.168.1.50:0  another computer's",
            "      (its X server must accept",
            "      network connections and allow",
            "      this gateway: xhost +<address>)",
        ] {
            rows.push(row.to_string());
        }
        if has_own_screen {
            rows.push(String::new());
            rows.push(format!("  {}", self.dim("This gateway has a desktop, so")));
            rows.push(format!("  {}", self.dim("this is not used here.")));
        }
        rows.push(String::new());
        rows.push("  Enter alone keeps it; - clears it.".to_string());
        let shown = if current.is_empty() { "(none)" } else { current };
        let room = if self.terminal_type == TerminalType::Petscii { PETSCII_WIDTH } else { 80 } - 1 - "  Current: ".len();
        rows.push(format!("  Current: {}", self.amber(&crate::webbrowser::truncate_to_width(shown, room))));
        rows
    }

    /// Ask for `addon_display`.  Enter alone keeps it; `-` clears it.
    async fn addon_display_prompt(&mut self, current: &str) -> Result<(), std::io::Error> {
        self.clear_screen().await?;
        for row in self.addon_display_rows(current, crate::gui::have_graphical_session()) {
            self.send_line(&row).await?;
        }
        self.send(&format!("  {}", self.cyan("New value: "))).await?;
        self.flush().await?;
        let Some(v) = self.get_line_input().await? else { return Ok(()) };
        let v = v.trim();
        if v.is_empty() {
            return Ok(());
        }
        let v = if v == "-" { "" } else { v };
        if !config::valid_addon_display(v) {
            return self.show_error("Not a display: use a name like :0.").await;
        }
        save_addon_setting("addon_display", v).await;
        self.other_saved_notice("GUI display updated.").await
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
        // Seen by the updater, which will not replace a folder in use.
        let _in_use = crate::addons::InUse::enter(&m.id);
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
        // A fresh screen: the add-on starts at the top, not under the menu.
        self.clear_screen().await?;
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
        let stall = self.idle_timeout;
        // The caller's keys, read on a task of their own for the run (see
        // `read_caller_keys`); the reader comes back when the run ends.
        let caller = std::mem::replace(&mut self.reader, Box::new(tokio::io::empty()));
        let (key_tx, mut key_rx) = tokio::sync::mpsc::channel::<u8>(ADDON_KEYS_MAX);
        let (state_tx, mut state_rx) = tokio::sync::watch::channel(CallerState::Here);
        let mut leave_w = state_rx.clone();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let keys = tokio::spawn(read_caller_keys(caller, local_telnet, is_petscii, key_tx, state_tx, stop_rx));
        let writer = &self.writer;
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
                // The key reader saw ESC twice, or the connection close.
                // Copied out inside the future: the guard `wait_for` returns
                // is not `Send` and must not live in the select's state.
                s = async { state_rx.wait_for(|s| *s != CallerState::Here).await.map(|s| *s) } => {
                    break match s {
                        Ok(CallerState::Left) => Ended::Left,
                        _ => Ended::Disconnected,
                    };
                }
                _ = tokio::time::sleep_until(flush_at), if hold_deadline.is_some() => {
                    hold_deadline = None;
                    out_buf.clear();
                    ansi_state.flush_pending(&mut out_buf);
                    if !out_buf.is_empty() {
                        match send_to_caller(writer, &out_buf, local_telnet, stall, Some(&mut leave_w)).await {
                            Sent::Done => {}
                            Sent::Gone => break Ended::Disconnected,
                            Sent::Left => break Ended::Left,
                        }
                    }
                }
                key = key_rx.recv() => {
                    // A closed queue is a reader that has finished: why is
                    // in its state (the ESC pair that ended it, or a hang-up).
                    let Some(b) = key else {
                        // `leave_w`, not `state_rx`: the branch above still
                        // holds that one.  Both see the same value.
                        break if *leave_w.borrow() == CallerState::Left { Ended::Left } else { Ended::Disconnected };
                    };
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
                        match send_to_caller(writer, &echo, local_telnet, stall, Some(&mut leave_w)).await {
                            Sent::Done => {}
                            Sent::Gone => break Ended::Disconnected,
                            Sent::Left => break Ended::Left,
                        }
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
                        match send_to_caller(writer, &out_buf, local_telnet, stall, Some(&mut leave_w)).await {
                            Sent::Done => {}
                            Sent::Gone => break Ended::Disconnected,
                            Sent::Left => break Ended::Left,
                        }
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
            // Then the terminal put back as the gateway draws it: an add-on
            // ended mid-colour (or one that hid the cursor) must not leave
            // the menus in its colours.
            match self.terminal_type {
                TerminalType::Ansi => out_buf.extend_from_slice(b"\x1b[0m\x1b[?25h"),
                TerminalType::Petscii if self.color_enabled => out_buf.extend_from_slice(&[0x92, PETSCII_DEFAULT]),
                TerminalType::Petscii => out_buf.push(0x92),
                TerminalType::Ascii => {}
            }
            if !out_buf.is_empty() {
                let _ = send_to_caller(&self.writer, &out_buf, local_telnet, stall, None).await;
            }
        }
        // The reader back from its task.  A task that failed has lost it:
        // the session then reads end-of-file and closes, which is the honest
        // outcome for a connection whose state is unknown.
        let _ = stop_tx.send(());
        match keys.await {
            Ok((reader, mut inbound)) => {
                self.reader = reader;
                // A telnet command the caller was half-way through must not
                // be read as the key that answers "Press any key" below.
                inbound.finish(&mut *self.reader).await;
            }
            Err(e) => glog!("Add-on \"{}\": the key reader failed: {}", m.id, e),
        }

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

/// Save one add-on setting off the runtime, as every telnet settings
/// screen does (it writes the config file).
async fn save_addon_setting(key: &'static str, value: &str) {
    let v = value.to_string();
    let _ = tokio::task::spawn_blocking(move || {
        config::update_config_value(key, &v);
    })
    .await;
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
