//! GUI console and configuration editor using egui/eframe.
//!
//! When `enable_console = true` in the config, this window is shown on startup.
//! Closing the window does NOT stop the server — it continues running headless.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use eframe::egui;
use egui::text::{CCursor, CCursorRange};
use egui::widgets::text_edit::TextEditState;
use egui::{Color32, Stroke};

use crate::config::{self, Config};
use crate::logger;

// ── Retro amber-on-dark color palette (telnetbible.com inspired) ──

const BG_DARKEST: Color32 = Color32::from_rgb(0x00, 0x05, 0x10); // matches logo background
const BG_DARK: Color32 = Color32::from_rgb(0x10, 0x1c, 0x3a);   // panel/frame bg
const BG_MID: Color32 = Color32::from_rgb(0x18, 0x28, 0x48);    // input fields
const BG_LIGHT: Color32 = Color32::from_rgb(0x22, 0x36, 0x5a);  // hover
const BORDER: Color32 = Color32::from_rgb(0x30, 0x45, 0x70);    // blue-gold border
const AMBER: Color32 = Color32::from_rgb(0xe6, 0xb4, 0x22);
const AMBER_BRIGHT: Color32 = Color32::from_rgb(0xff, 0xd7, 0x00);
const AMBER_DIM: Color32 = Color32::from_rgb(0x8b, 0x7a, 0x3a);
const TEXT_PRIMARY: Color32 = Color32::from_rgb(0xd4, 0xc5, 0x90);
const TEXT_INPUT: Color32 = Color32::from_rgb(0xe8, 0xdc, 0xb0);
#[cfg(test)]
const GREEN: Color32 = Color32::from_rgb(0x33, 0xff, 0x33);
const CONSOLE_TEXT: Color32 = Color32::from_rgb(0x33, 0xcc, 0x33);
const SCRIPTURE: Color32 = Color32::from_rgb(0xc0, 0xaa, 0x60);  // lighter amber for verse
const CONSOLE_BG: Color32 = Color32::from_rgb(0x08, 0x12, 0x28); // deeper blue for console
const SELECTION: Color32 = Color32::from_rgb(0x26, 0x4f, 0x78);
const POPUP_BG: Color32 = Color32::from_rgb(0x04, 0x18, 0x0a);      // deep forest green — popup panel
const POPUP_INPUT_BG: Color32 = Color32::from_rgb(0x1c, 0x46, 0x2a); // brighter green — text entry on popups
const WARN_BG: Color32 = Color32::from_rgb(0x33, 0x06, 0x06);      // dark red — WARNING popup panel (must-acknowledge)
const WARN_BORDER: Color32 = Color32::from_rgb(0xe0, 0x3a, 0x3a);  // red border for warning popups

/// Launch the GUI window.  Blocks the calling thread until the window is closed.
/// If the GUI fails to start (e.g. missing graphics drivers), logs the error and
/// returns so the server continues running headless.
///
/// `gui_ctx` is a shared slot the app fills with its `egui::Context` on startup
/// so the signal watcher can wake the event loop on Ctrl-C.
pub fn run(
    cfg: Config,
    shutdown: Arc<AtomicBool>,
    restart: Arc<AtomicBool>,
    gui_ctx: Arc<std::sync::Mutex<Option<egui::Context>>>,
) {
    // The console window renders into the desktop's X session. Launched as
    // a boot-time service, the process can start before that session has
    // finished writing its display auth cookie, so a premature connect
    // fails ("Invalid MIT-MAGIC-COOKIE-1" / "Failed to open connection to X
    // server") and the GUI would be lost to headless fallback.
    //
    // We can't just retry eframe::run_native: winit sets a process-global
    // "event loop created" flag on the FIRST build() attempt and never
    // resets it on Unix (winit event_loop.rs), so any second attempt returns
    // RecreationAttempt even once X is ready. So instead we wait until the
    // display is actually reachable *and authenticated* before the single
    // run_native call below. This is adaptive, not a fixed delay: a manual
    // launch into a live desktop passes the very first probe and starts with
    // no wait — only the boot race waits, and only as long as the session
    // needs. The server already runs on its own thread (spawned in main
    // before this call), so this never delays telnet/SSH/serial.
    wait_for_display(&shutdown);

    // A monitor that advertises a high DPI — e.g. a large `Xft.dpi` on this
    // display's X session — makes winit choose a big scale factor, so both the
    // window *and* its egui content come up oversized and the window can spill
    // off the screen edges (winit 0.30 uses `Xft.dpi / 96` when
    // `WINIT_X11_SCALE_FACTOR` is unset).  When the operator pins `gui_zoom` to
    // a number, enforce it at the windowing layer via that variable so the
    // window is sized at that scale too — not just the content.  The per-frame
    // `set_pixels_per_point` in `ui()` still scales the content and covers
    // Wayland, where this X11-only override is ignored.  Leave "auto" to read
    // the display as before, and don't clobber an operator-set value.
    if let Some(z) = cfg.gui_zoom_factor() {
        if std::env::var_os("WINIT_X11_SCALE_FACTOR").is_none() {
            // SAFETY: set once here at GUI startup, before `run_native` below
            // creates the winit event loop that reads it; no other thread
            // reads or writes this variable.
            unsafe {
                std::env::set_var("WINIT_X11_SCALE_FACTOR", format!("{z}"));
            }
        }
    }

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // `mut` is only needed on ARM, where we patch wgpu limits below.
        // Restore the saved window geometry (position + inner size) so the GUI
        // reopens where the operator last left it; otherwise fall back to the
        // default size + window-manager placement.  On Wayland with_position is
        // ignored by the compositor (harmless — geometry just isn't restored).
        let mut viewport = egui::ViewportBuilder::default()
            .with_title(format!("Ethernet Gateway v{}", env!("CARGO_PKG_VERSION")))
            .with_min_inner_size([640.0, 480.0]);
        viewport = match parse_window_geometry(&cfg.gui_window_geometry) {
            Some((x, y, w, h)) => viewport
                .with_position([x as f32, y as f32])
                .with_inner_size([w as f32, h as f32]),
            None => viewport.with_inner_size([1120.0, 810.0]),
        };
        #[allow(unused_mut)]
        let mut options = eframe::NativeOptions {
            viewport,
            ..Default::default()
        };

        // ARM SBCs such as the Raspberry Pi have GPUs (e.g. VideoCore/V3D)
        // that report several device limits below wgpu's desktop defaults
        // (max_color_attachments, max_inter_stage_shader_variables, buffer
        // sizes, ...). eframe's default requests the desktop limits, so
        // device creation aborts with errors like:
        //   "Limit 'max_color_attachments' value 8 is better than allowed 4".
        // Rather than clamp fields one at a time, request exactly the limits
        // the chosen adapter advertises — that satisfies every field at once
        // and is always valid, since you can't request more than the adapter
        // supports. egui runs fine on these (it targets WebGL2-class limits).
        // Desktop builds are unaffected — they keep eframe's defaults.
        #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
        {
            use eframe::egui_wgpu::{wgpu, WgpuSetup};
            if let WgpuSetup::CreateNew(setup) = &mut options.wgpu_options.wgpu_setup {
                // Prefer the OpenGL ES backend on ARM. The Raspberry Pi's V3D
                // Vulkan driver (Mesa) is incomplete and aborts device creation
                // with a wgpu-hal "Requested feature is not available on this
                // device" panic; the GLES backend is V3D's mature path. An
                // explicit WGPU_BACKEND still wins, for debugging.
                setup.instance_descriptor.backends =
                    wgpu::Backends::from_env().unwrap_or(wgpu::Backends::GL);
                // Request exactly the limits the adapter advertises (see above).
                setup.device_descriptor = Arc::new(|adapter| wgpu::DeviceDescriptor {
                    label: Some("egui wgpu device (arm)"),
                    required_limits: adapter.limits(),
                    ..Default::default()
                });
            }
        }

        eframe::run_native(
            "Ethernet Gateway",
            options,
            Box::new(move |cc| {
                *gui_ctx.lock().unwrap_or_else(|e| e.into_inner()) = Some(cc.egui_ctx.clone());
                egui_extras::install_image_loaders(&cc.egui_ctx);
                Ok(Box::new(App::new(cfg, shutdown, restart)))
            }),
        )
    }));

    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => logger::log(format!("GUI could not start: {}", e)),
        Err(_) => logger::log("GUI crashed during startup (possible graphics driver issue)".into()),
    }
}

/// Wait until the X display is reachable and our auth cookie is accepted, so
/// the one-shot `eframe::run_native` doesn't race a not-yet-ready session at
/// boot. Bounded and degrades safely:
///   * no `DISPLAY` (headless, or a pure-Wayland session) -> return at once,
///     letting run_native use Wayland/headless directly;
///   * `xset` not installed -> can't probe, return at once (preserves the
///     original immediate-attempt behavior);
///   * X reachable -> return as soon as the probe authenticates;
///   * still not ready after 60s -> give up waiting and let run_native try
///     anyway (it logs if it can't start).
fn wait_for_display(shutdown: &Arc<AtomicBool>) {
    // The probe (xset) is X11-specific, so only gate on an X11 DISPLAY.
    if std::env::var_os("DISPLAY").is_none() {
        return;
    }

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let mut announced = false;
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return;
        }
        match probe_display() {
            Some(true) => {
                // X is up + authenticated. Now also wait (bounded) for a
                // window manager to be managing the display before we open the
                // decorated console window: at boot the X server can accept us
                // before the WM/panel takes over, and a window mapped in that
                // gap comes up undecorated (no title bar/min/close) or placed
                // with its title bar under the panel. Only the window waits —
                // the server is already running.
                wait_for_wm(shutdown);
                return;
            }
            None => return, // can't probe (no xset): attempt directly
            Some(false) => {
                if !announced {
                    logger::log("GUI: waiting for the display session to be ready…".into());
                    announced = true;
                }
            }
        }
        if std::time::Instant::now() >= deadline {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}

/// Probe the X display the same way eframe will: `Some(true)` if an
/// authenticated query succeeds, `Some(false)` if X is configured but not
/// answering yet, `None` if we have no way to probe (`xset` not installed).
/// `xset` inherits DISPLAY/XAUTHORITY from our environment, so it performs
/// the identical connection + cookie authentication eframe relies on.
fn probe_display() -> Option<bool> {
    use std::process::{Command, Stdio};
    match Command::new("xset")
        .arg("q")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(status) => Some(status.success()),
        Err(_) => None,
    }
}

/// After the X server is reachable, wait (bounded) for an EWMH window manager
/// to be managing the display. This closes the boot gap where X accepts us but
/// the WM/panel hasn't taken over yet, which left the console window
/// undecorated or with its title bar tucked under the desktop panel. Degrades
/// safely, mirroring `wait_for_display`:
///   * `xprop` not installed -> can't probe, return at once;
///   * WM present -> return as soon as it is detected;
///   * no WM after 15s -> give up waiting and open anyway (a bare X server or a
///     non-EWMH WM is never worse off than the previous open-immediately path).
///
/// Only the GUI window waits here; the server started earlier and is unaffected.
fn wait_for_wm(shutdown: &Arc<AtomicBool>) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let mut announced = false;
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return;
        }
        match probe_wm() {
            Some(true) => return, // a WM is managing the display: safe to open
            None => return,       // can't probe (no xprop): open directly
            Some(false) => {
                if !announced {
                    logger::log("GUI: waiting for the window manager to be ready…".into());
                    announced = true;
                }
            }
        }
        if std::time::Instant::now() >= deadline {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
}

/// Probe for an EWMH-compliant window manager on the current X display:
/// `Some(true)` if `_NET_SUPPORTING_WM_CHECK` is set on the root window (a WM
/// is managing), `Some(false)` if it is not yet, `None` if we cannot probe
/// (`xprop` not installed). `xprop` inherits DISPLAY/XAUTHORITY from our
/// environment, so it authenticates the same way eframe will. Note `xprop`
/// exits 0 whether or not the property exists (printing "not found." when it
/// does not), so we key on the printed value, not the exit status.
fn probe_wm() -> Option<bool> {
    use std::process::{Command, Stdio};
    match Command::new("xprop")
        .args(["-root", "_NET_SUPPORTING_WM_CHECK"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    {
        Ok(out) if out.status.success() => {
            Some(String::from_utf8_lossy(&out.stdout).contains("window id #"))
        }
        Ok(_) => Some(false), // xprop ran but couldn't query: treat as not-ready
        Err(_) => None,       // xprop not installed: can't probe
    }
}

/// Parse a saved `x,y,width,height` window geometry string (as written to
/// `gui_window_geometry`).  Returns `None` for empty/malformed input or an
/// implausible size, so a bad value harmlessly falls back to the defaults.
fn parse_window_geometry(s: &str) -> Option<(i32, i32, i32, i32)> {
    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() != 4 {
        return None;
    }
    let x = parts[0].trim().parse::<i32>().ok()?;
    let y = parts[1].trim().parse::<i32>().ok()?;
    let w = parts[2].trim().parse::<i32>().ok()?;
    let h = parts[3].trim().parse::<i32>().ok()?;
    if !(320..=16384).contains(&w) || !(240..=16384).contains(&h) {
        return None;
    }
    Some((x, y, w, h))
}

fn apply_theme(ctx: &egui::Context) {
    // Set absolute font sizes (avoids compounding if theme is re-applied)
    let mut style = (*ctx.global_style()).clone();
    for (text_style, font_id) in style.text_styles.iter_mut() {
        font_id.size = match text_style {
            egui::TextStyle::Small => 13.2,
            egui::TextStyle::Body => 16.8,
            egui::TextStyle::Monospace => 16.8,
            egui::TextStyle::Button => 16.8,
            egui::TextStyle::Heading => 24.0,
            egui::TextStyle::Name(_) => font_id.size,
        };
    }
    ctx.set_global_style(style);

    // Apply retro amber-on-dark visuals
    let mut vis = egui::Visuals::dark();
    vis.dark_mode = true;
    vis.override_text_color = Some(TEXT_PRIMARY);
    vis.selection.bg_fill = SELECTION;
    vis.selection.stroke = Stroke::new(1.0_f32, AMBER);

    vis.window_fill = BG_DARKEST;
    vis.panel_fill = BG_DARKEST;
    vis.faint_bg_color = BG_DARKEST;
    vis.extreme_bg_color = BG_MID; // text input backgrounds

    // Non-interactive widgets (labels, frames)
    vis.widgets.noninteractive.bg_fill = BG_DARK;
    vis.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, TEXT_PRIMARY);
    vis.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, BORDER);

    // Inactive widgets (buttons, checkboxes, text inputs at rest)
    vis.widgets.inactive.bg_fill = BG_MID;
    vis.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, TEXT_INPUT);
    vis.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, BORDER);

    // Hovered widgets
    vis.widgets.hovered.bg_fill = BG_LIGHT;
    vis.widgets.hovered.fg_stroke = Stroke::new(1.5_f32, AMBER_BRIGHT);
    vis.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, AMBER);

    // Active (clicked) widgets
    vis.widgets.active.bg_fill = BG_LIGHT;
    vis.widgets.active.fg_stroke = Stroke::new(2.0_f32, AMBER_BRIGHT);
    vis.widgets.active.bg_stroke = Stroke::new(1.0_f32, AMBER_BRIGHT);

    // Open widgets (e.g. combo box when expanded)
    vis.widgets.open.bg_fill = BG_MID;
    vis.widgets.open.fg_stroke = Stroke::new(1.0_f32, AMBER);
    vis.widgets.open.bg_stroke = Stroke::new(1.0_f32, AMBER_DIM);

    vis.window_stroke = Stroke::new(1.0_f32, BORDER);

    ctx.set_visuals(vis);
}

/// Get the first non-loopback private IP address of this machine.
fn local_ip() -> String {
    if let Ok(ifaces) = if_addrs::get_if_addrs() {
        for iface in &ifaces {
            if iface.is_loopback() {
                continue;
            }
            let ip = iface.ip();
            if ip.is_ipv4() {
                return ip.to_string();
            }
        }
    }
    "unknown".into()
}

/// Shared tokio runtime used by the folder-picker.  Creating and dropping
/// a fresh runtime for each pick caused the XDG portal's D-Bus connection
/// to go stale, so subsequent dialogs never resolved and the button
/// stayed disabled forever.  A single long-lived runtime avoids that.
static PICKER_RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> =
    std::sync::OnceLock::new();

fn picker_runtime() -> &'static tokio::runtime::Runtime {
    PICKER_RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .thread_name("folder-picker")
            .build()
            .expect("folder-picker runtime")
    })
}

/// Launch a native folder-picker dialog on the shared picker runtime so
/// it does not block the egui event loop.  Returns the receiver end of
/// an mpsc channel; the App polls it each frame and updates
/// `transfer_dir` when the user has chosen a folder (or clears the
/// in-flight marker if the user cancels).
fn spawn_folder_picker(
    current_dir: &str,
) -> std::sync::mpsc::Receiver<Option<std::path::PathBuf>> {
    let start = {
        let p = std::path::PathBuf::from(current_dir);
        if p.is_dir() {
            p
        } else {
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
        }
    };
    let (tx, rx) = std::sync::mpsc::channel();
    picker_runtime().spawn(async move {
        let result = rfd::AsyncFileDialog::new()
            .set_title("Select transfer directory")
            .set_directory(&start)
            .pick_folder()
            .await
            .map(|h| h.path().to_path_buf());
        let _ = tx.send(result);
    });
    rx
}

/// Enumerate available serial ports, returning their device paths.
/// `pub(crate)` so the web server's serial-port dropdown can populate
/// from the same source the desktop GUI uses — both surfaces show
/// the same list and refresh through the same code path.
pub(crate) fn detect_serial_ports() -> Vec<String> {
    match serialport::available_ports() {
        Ok(ports) => ports.into_iter().map(|p| p.port_name).collect(),
        Err(e) => {
            logger::log(format!("Could not detect serial ports: {}", e));
            Vec::new()
        }
    }
}

struct App {
    cfg: Config,
    /// Snapshot of the global config at last sync.  When the global singleton
    /// diverges from this (e.g. a telnet session changed a setting), we know
    /// an external update happened and refresh the GUI fields.
    last_synced_cfg: Config,
    console_lines: Vec<String>,
    theme_applied: bool,
    local_ip: String,
    shutdown: Arc<AtomicBool>,
    restart: Arc<AtomicBool>,
    // Window-geometry persistence (auto-managed → gui_window_geometry in the
    // config; no UI surface).  last_seen tracks the live rect, geom_changed_at
    // debounces writes, saved holds what's on disk to avoid redundant writes.
    last_seen_geom: Option<(i32, i32, i32, i32)>,
    geom_changed_at: f64,
    saved_geom: Option<(i32, i32, i32, i32)>,
    // String buffers for numeric fields so the user can type freely
    telnet_port_buf: String,
    ssh_port_buf: String,
    kermit_server_port_buf: String,
    web_port_buf: String,
    slave_master_port_buf: String,
    max_sessions_buf: String,
    idle_timeout_buf: String,
    negotiation_timeout_buf: String,
    block_timeout_buf: String,
    max_retries_buf: String,
    negotiation_retry_interval_buf: String,
    zmodem_negotiation_timeout_buf: String,
    zmodem_frame_timeout_buf: String,
    zmodem_max_retries_buf: String,
    zmodem_negotiation_retry_interval_buf: String,
    kermit_negotiation_timeout_buf: String,
    kermit_packet_timeout_buf: String,
    kermit_idle_timeout_buf: String,
    kermit_max_retries_buf: String,
    kermit_resume_max_age_hours_buf: String,
    kermit_max_packet_length_buf: String,
    kermit_window_size_buf: String,
    kermit_block_check_type_buf: String,
    punter_block_size_buf: String,
    punter_negotiation_timeout_buf: String,
    punter_block_timeout_buf: String,
    punter_max_retries_buf: String,
    punter_max_bad_rounds_buf: String,
    punter_negotiation_retry_interval_buf: String,
    /// Per-port baud text buffer, indexed by `SerialPortId::index()`.
    /// Two slots — one each for Port A and Port B — let the user type
    /// freely without their input being clobbered by a partial parse.
    serial_baud_buf: [String; 2],
    // Detected serial ports for the dropdown (shared between both ports).
    serial_ports: Vec<String>,
    /// Set when the user edits any field; prevents refresh_from_global from
    /// overwriting in-progress edits. Cleared on save.
    dirty: bool,
    /// Whether the Server "More..." popup is open.
    server_popup_open: bool,
    /// Per-port "Serial Port — More..." popup state, indexed by
    /// `SerialPortId::index()`.  Independent so the user can have one
    /// port's popup open while editing the other's primary controls.
    serial_popup_open: [bool; 2],
    /// Whether the File Transfer "More..." popup is open.
    file_transfer_popup_open: bool,
    /// Whether the "AI, Browser & Weather — More..." popup is open.  Holds the
    /// weather location + units (and re-shows the API key / homepage) so the
    /// main frame stays at three rows.
    ai_browser_popup_open: bool,
    /// Whether the security-warning popup for `Allow ATDT KERMIT` is
    /// open.  Shown when the operator first ticks the checkbox; gated
    /// behind explicit confirmation because enabling the feature
    /// bypasses the telnet menu's auth gate.
    atdt_kermit_warn_open: bool,
    /// Warn-only popup shown when the role is switched to Master while the SSH
    /// server is off (the relay listens on the SSH port).  Never toggles SSH.
    relay_ssh_warn_open: bool,
    /// Whether the security-warning popup for the standalone Kermit
    /// server listener is open.  Shown when the operator first ticks
    /// the "Kermit Server" checkbox in the Server frame or its More
    /// popup.  Confirming flips `kermit_server_enabled`; cancelling
    /// leaves it false because the click never reached `cfg`.
    kermit_server_warn_open: bool,
    /// Whether the security-warning popup for `Disable IP Safety` is
    /// open.  Same posture as `kermit_server_warn_open` — off→on opens
    /// the popup, the visible checkbox stays unchecked until the
    /// operator clicks Enable.  Cancel leaves `disable_ip_safety` at
    /// its prior false value.
    disable_ip_safety_warn_open: bool,
    /// When the user clicks the folder-browse button, the native dialog
    /// runs on a background OS thread so it can't block the egui event
    /// loop.  This channel carries back the chosen path (or None if
    /// cancelled).  While `Some`, the button is disabled to prevent
    /// spawning duplicate pickers.
    pending_dir_pick: Option<std::sync::mpsc::Receiver<Option<std::path::PathBuf>>>,
}

impl App {
    fn new(cfg: Config, shutdown: Arc<AtomicBool>, restart: Arc<AtomicBool>) -> Self {
        // Seed saved_geom from the config so we don't rewrite the identical
        // geometry we just restored on first launch.
        let saved_geom = parse_window_geometry(&cfg.gui_window_geometry);
        let telnet_port_buf = cfg.telnet_port.to_string();
        let ssh_port_buf = cfg.ssh_port.to_string();
        let kermit_server_port_buf = cfg.kermit_server_port.to_string();
        let web_port_buf = cfg.web_port.to_string();
        let slave_master_port_buf = cfg.slave_master_port.to_string();
        let max_sessions_buf = cfg.max_sessions.to_string();
        let idle_timeout_buf = cfg.idle_timeout_secs.to_string();
        let negotiation_timeout_buf = cfg.xmodem_negotiation_timeout.to_string();
        let block_timeout_buf = cfg.xmodem_block_timeout.to_string();
        let max_retries_buf = cfg.xmodem_max_retries.to_string();
        let negotiation_retry_interval_buf =
            cfg.xmodem_negotiation_retry_interval.to_string();
        let zmodem_negotiation_timeout_buf = cfg.zmodem_negotiation_timeout.to_string();
        let zmodem_frame_timeout_buf = cfg.zmodem_frame_timeout.to_string();
        let zmodem_max_retries_buf = cfg.zmodem_max_retries.to_string();
        let zmodem_negotiation_retry_interval_buf =
            cfg.zmodem_negotiation_retry_interval.to_string();
        let kermit_negotiation_timeout_buf = cfg.kermit_negotiation_timeout.to_string();
        let kermit_packet_timeout_buf = cfg.kermit_packet_timeout.to_string();
        let kermit_idle_timeout_buf = cfg.kermit_idle_timeout.to_string();
        let kermit_max_retries_buf = cfg.kermit_max_retries.to_string();
        let kermit_resume_max_age_hours_buf = cfg.kermit_resume_max_age_hours.to_string();
        let kermit_max_packet_length_buf = cfg.kermit_max_packet_length.to_string();
        let kermit_window_size_buf = cfg.kermit_window_size.to_string();
        let kermit_block_check_type_buf = cfg.kermit_block_check_type.to_string();
        let punter_block_size_buf = cfg.punter_block_size.to_string();
        let punter_negotiation_timeout_buf = cfg.punter_negotiation_timeout.to_string();
        let punter_block_timeout_buf = cfg.punter_block_timeout.to_string();
        let punter_max_retries_buf = cfg.punter_max_retries.to_string();
        let punter_max_bad_rounds_buf = cfg.punter_max_bad_rounds.to_string();
        let punter_negotiation_retry_interval_buf =
            cfg.punter_negotiation_retry_interval.to_string();
        let serial_baud_buf = [
            cfg.serial_a.baud.to_string(),
            cfg.serial_b.baud.to_string(),
        ];
        let serial_ports = detect_serial_ports();
        let last_synced_cfg = cfg.clone();
        Self {
            cfg,
            last_synced_cfg,
            console_lines: Vec::new(),
            theme_applied: false,
            local_ip: local_ip(),
            shutdown,
            restart,
            last_seen_geom: None,
            geom_changed_at: 0.0,
            saved_geom,
            telnet_port_buf,
            ssh_port_buf,
            kermit_server_port_buf,
            web_port_buf,
            slave_master_port_buf,
            max_sessions_buf,
            idle_timeout_buf,
            negotiation_timeout_buf,
            block_timeout_buf,
            max_retries_buf,
            negotiation_retry_interval_buf,
            zmodem_negotiation_timeout_buf,
            zmodem_frame_timeout_buf,
            zmodem_max_retries_buf,
            zmodem_negotiation_retry_interval_buf,
            kermit_negotiation_timeout_buf,
            kermit_packet_timeout_buf,
            kermit_idle_timeout_buf,
            kermit_max_retries_buf,
            kermit_resume_max_age_hours_buf,
            kermit_max_packet_length_buf,
            kermit_window_size_buf,
            kermit_block_check_type_buf,
            punter_block_size_buf,
            punter_negotiation_timeout_buf,
            punter_block_timeout_buf,
            punter_max_retries_buf,
            punter_max_bad_rounds_buf,
            punter_negotiation_retry_interval_buf,
            serial_baud_buf,
            serial_ports,
            dirty: false,
            server_popup_open: false,
            serial_popup_open: [false, false],
            file_transfer_popup_open: false,
            ai_browser_popup_open: false,
            atdt_kermit_warn_open: false,
            relay_ssh_warn_open: false,
            kermit_server_warn_open: false,
            disable_ip_safety_warn_open: false,
            pending_dir_pick: None,
        }
    }

    fn sync_numeric_fields(&mut self) {
        if let Ok(v) = self.telnet_port_buf.parse::<u16>() && v >= 1 { self.cfg.telnet_port = v; }
        if let Ok(v) = self.ssh_port_buf.parse::<u16>() && v >= 1 { self.cfg.ssh_port = v; }
        if let Ok(v) = self.kermit_server_port_buf.parse::<u16>() && v >= 1 { self.cfg.kermit_server_port = v; }
        if let Ok(v) = self.web_port_buf.parse::<u16>() && v >= 1 { self.cfg.web_port = v; }
        if let Ok(v) = self.slave_master_port_buf.parse::<u16>() && v >= 1 { self.cfg.slave_master_port = v; }
        if let Ok(v) = self.max_sessions_buf.parse::<usize>() && v >= 1 { self.cfg.max_sessions = v; }
        if let Ok(v) = self.idle_timeout_buf.parse() { self.cfg.idle_timeout_secs = v; }
        if let Ok(v) = self.negotiation_timeout_buf.parse::<u64>() && v >= 1 { self.cfg.xmodem_negotiation_timeout = v; }
        if let Ok(v) = self.block_timeout_buf.parse::<u64>() && v >= 1 { self.cfg.xmodem_block_timeout = v; }
        if let Ok(v) = self.max_retries_buf.parse::<usize>() && v >= 1 { self.cfg.xmodem_max_retries = v; }
        if let Ok(v) = self.negotiation_retry_interval_buf.parse::<u64>() && v >= 1 { self.cfg.xmodem_negotiation_retry_interval = v; }
        if let Ok(v) = self.zmodem_negotiation_timeout_buf.parse::<u64>() && v >= 1 { self.cfg.zmodem_negotiation_timeout = v; }
        if let Ok(v) = self.zmodem_frame_timeout_buf.parse::<u64>() && v >= 1 { self.cfg.zmodem_frame_timeout = v; }
        if let Ok(v) = self.zmodem_max_retries_buf.parse::<u32>() && v >= 1 { self.cfg.zmodem_max_retries = v; }
        if let Ok(v) = self.zmodem_negotiation_retry_interval_buf.parse::<u64>() && v >= 1 { self.cfg.zmodem_negotiation_retry_interval = v; }
        if let Ok(v) = self.kermit_negotiation_timeout_buf.parse::<u64>() && v >= 1 { self.cfg.kermit_negotiation_timeout = v; }
        if let Ok(v) = self.kermit_packet_timeout_buf.parse::<u64>() && v >= 1 { self.cfg.kermit_packet_timeout = v; }
        // No `>= 1` floor on idle timeout — `0` is the explicit
        // "disable" sentinel matching the config-file loader.
        if let Ok(v) = self.kermit_idle_timeout_buf.parse::<u64>() { self.cfg.kermit_idle_timeout = v; }
        if let Ok(v) = self.kermit_max_retries_buf.parse::<u32>() && v >= 1 { self.cfg.kermit_max_retries = v; }
        if let Ok(v) = self.kermit_resume_max_age_hours_buf.parse::<u32>() && v >= 1 { self.cfg.kermit_resume_max_age_hours = v; }
        if let Ok(v) = self.kermit_max_packet_length_buf.parse::<u16>() && (10..=9024).contains(&v) { self.cfg.kermit_max_packet_length = v; }
        if let Ok(v) = self.kermit_window_size_buf.parse::<u8>() && (1..=31).contains(&v) { self.cfg.kermit_window_size = v; }
        if let Ok(v) = self.kermit_block_check_type_buf.parse::<u8>() && matches!(v, 1..=3) { self.cfg.kermit_block_check_type = v; }
        if let Ok(v) = self.punter_block_size_buf.parse::<u16>() && (8..=255).contains(&v) { self.cfg.punter_block_size = v; }
        if let Ok(v) = self.punter_negotiation_timeout_buf.parse::<u64>() && v >= 1 { self.cfg.punter_negotiation_timeout = v; }
        if let Ok(v) = self.punter_block_timeout_buf.parse::<u64>() && v >= 1 { self.cfg.punter_block_timeout = v; }
        if let Ok(v) = self.punter_max_retries_buf.parse::<u32>() && v >= 1 { self.cfg.punter_max_retries = v; }
        if let Ok(v) = self.punter_max_bad_rounds_buf.parse::<u32>() && v >= 1 { self.cfg.punter_max_bad_rounds = v; }
        if let Ok(v) = self.punter_negotiation_retry_interval_buf.parse::<u64>() && v >= 1 { self.cfg.punter_negotiation_retry_interval = v; }
        for id in crate::config::SERIAL_PORT_IDS {
            if let Ok(v) = self.serial_baud_buf[id.index()].parse::<u32>()
                && v >= 300
            {
                self.cfg.port_mut(id).baud = v;
            }
        }
    }

    fn poll_logs(&mut self) {
        let new_lines = logger::drain();
        if !new_lines.is_empty() {
            self.console_lines.extend(new_lines);
            if self.console_lines.len() > 2000 {
                let excess = self.console_lines.len() - 2000;
                self.console_lines.drain(..excess);
            }
        }
    }

    /// Check whether a backgrounded folder-picker has delivered a result.
    /// If the user chose a folder, copy it into `transfer_dir`; if they
    /// cancelled (or the picker failed), just drop the pending state.
    fn poll_dir_pick(&mut self) {
        let Some(rx) = &self.pending_dir_pick else { return };
        match rx.try_recv() {
            Ok(Some(path)) => {
                self.cfg.transfer_dir = path.display().to_string();
                self.pending_dir_pick = None;
            }
            Ok(None) | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.pending_dir_pick = None;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
    }

    /// Render the Server frame's primary field rows (telnet, SSH, and
    /// the standalone Kermit-server listener).  Shared between the main
    /// layout and the Server popup.  When `with_more_button` is true, a
    /// right-aligned "More..." button is appended to the Kermit row;
    /// the popup passes false since it's already the More view.
    ///
    /// The Kermit checkbox is bound to a local copy so we can intercept
    /// the off→on transition and gate it behind the security-warning
    /// popup (the standalone listener bypasses authentication AND the
    /// private-IP allowlist).  On→off is one-click safe — tightening
    /// security never needs a confirmation; the change persists
    /// immediately.
    fn draw_server_controls(&mut self, ui: &mut egui::Ui, with_more_button: bool) {
        // Fixed-width checkbox columns so the two `Port:` labels line
        // up between rows — the same colon-alignment the web frame
        // gets from CSS Grid.  Sized just wider than the longest
        // label in each column (Telnet in col 1, Kermit Server in
        // col 2) so the row total + the right-floated More button
        // fits inside the half-width frame.  An earlier pass set
        // these too generously (110 / 170) and the More button
        // overlapped the Web port input — there's a limit to how
        // wide the cols can be before the More button collides
        // with the input.
        //
        // Implementation note: `allocate_ui_with_layout(vec2(W, 0))`
        // does NOT actually reserve W of horizontal space — it caps
        // the *maximum* width but lets the cursor advance by the
        // smaller of (desired, actual).  So a short label like "SSH"
        // would shrink its slot back down, breaking the alignment.
        // Render the checkbox, measure its rect, then pad up to the
        // column width with `add_space` — that always advances by
        // the exact requested amount.
        const COL1_W: f32 = 100.0;
        const COL2_W: f32 = 140.0;
        const PORT_W: f32 = 50.0;
        const GUTTER: f32 = 12.0;

        // Row 1: Telnet + Web Server + right-aligned More button.
        // More moved up to row 1 to mirror the web layout — the
        // upper row carries the button, the lower row stays clean.
        ui.horizontal(|ui| {
            let resp = ui.checkbox(&mut self.cfg.telnet_enabled, "Telnet");
            pad_to(ui, COL1_W, resp.rect.width());
            labeled_field(ui, "Port:", &mut self.telnet_port_buf, PORT_W);
            ui.add_space(GUTTER);
            let resp = ui.checkbox(&mut self.cfg.web_enabled, "Web Server");
            pad_to(ui, COL2_W, resp.rect.width());
            labeled_field(ui, "Port:", &mut self.web_port_buf, PORT_W);
            if with_more_button && right_aligned_small_button(ui, "More...") {
                self.server_popup_open = true;
            }
        });
        // Row 2: SSH + Kermit Server.  Same column widths so the
        // colons line up with row 1.  The Kermit checkbox keeps its
        // off→on security-warning popup interlock.
        ui.horizontal(|ui| {
            let resp = ui.checkbox(&mut self.cfg.ssh_enabled, "SSH");
            pad_to(ui, COL1_W, resp.rect.width());
            labeled_field(ui, "Port:", &mut self.ssh_port_buf, PORT_W);
            ui.add_space(GUTTER);
            let mut local = self.cfg.kermit_server_enabled;
            let prev = local;
            let resp = ui.checkbox(&mut local, "Kermit Server");
            pad_to(ui, COL2_W, resp.rect.width());
            if resp.changed() && !self.kermit_server_warn_open {
                if local && !prev {
                    // Off → on: revert visible state, open the
                    // confirmation popup; the popup's Enable button
                    // commits the change if the operator confirms.
                    self.kermit_server_warn_open = true;
                } else if !local && prev {
                    // On → off: commit immediately, no popup.
                    self.cfg.kermit_server_enabled = false;
                    self.last_synced_cfg.kermit_server_enabled = false;
                    config::update_config_value("kermit_server_enabled", "false");
                    logger::log("Kermit server disabled.".into());
                }
            }
            labeled_field(ui, "Port:", &mut self.kermit_server_port_buf, PORT_W);
        });
    }

    /// Server More-popup-only rows.  Holds settings that don't fit in
    /// the main Server frame: the session cap and the per-session
    /// idle-timeout.  The main frame surfaces only the listener
    /// enable/port fields per the operator-facing layout decision; the
    /// More popup keeps everything available for completeness.
    fn draw_server_more_only(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            labeled_field(ui, "Sessions:", &mut self.max_sessions_buf, 50.0);
            ui.add_space(8.0);
            labeled_field(ui, "Idle (s):", &mut self.idle_timeout_buf, 50.0);
        });
        ui.add_space(4.0);
        // Display scale for THIS console window. "Auto" follows the monitor's
        // reported DPI; a fixed percentage pins the size so a display that
        // over-reports its scale factor doesn't blow the window up.
        ui.horizontal(|ui| {
            ui.label("Display scale:");
            let selected = match self.cfg.gui_zoom_factor() {
                None => "Auto".to_string(),
                Some(z) => format!("{}%", (z * 100.0).round() as i32),
            };
            egui::ComboBox::from_id_salt("gui_zoom_combo")
                .width(90.0)
                .selected_text(selected)
                .show_ui(ui, |ui| {
                    for (label, val) in [
                        ("Auto", "auto"),
                        ("75%", "0.75"),
                        ("100%", "1.0"),
                        ("125%", "1.25"),
                        ("150%", "1.5"),
                        ("200%", "2.0"),
                    ] {
                        ui.selectable_value(&mut self.cfg.gui_zoom, val.to_string(), label);
                    }
                });
        });
    }

    /// Contents of the "AI, Browser & Weather — More" popup: every option in
    /// the group.  The main frame shows only the API key + homepage (three-row
    /// budget); the weather location + units live here.
    fn draw_ai_browser_more(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("API Key:");
            singleline_with_menu(ui, &mut self.cfg.groq_api_key, true, None);
        });
        ui.horizontal(|ui| {
            ui.label("Home:");
            singleline_with_menu(ui, &mut self.cfg.browser_homepage, false, None);
        });
        ui.horizontal(|ui| {
            labeled_field(ui, "Location:", &mut self.cfg.weather_location, 160.0);
        });
        ui.horizontal(|ui| {
            ui.label("Units:");
            let sel = match self.cfg.weather_units.as_str() {
                "us" => "US (F/mph)",
                "metric" => "Metric (C/km/h)",
                _ => "Auto",
            };
            egui::ComboBox::from_id_salt("weather_units_combo")
                .width(160.0)
                .selected_text(sel)
                .show_ui(ui, |ui| {
                    for (label, val) in [
                        ("Auto", "auto"),
                        ("US (F/mph)", "us"),
                        ("Metric (C/km/h)", "metric"),
                    ] {
                        ui.selectable_value(
                            &mut self.cfg.weather_units,
                            val.to_string(),
                            label,
                        );
                    }
                });
        });
    }

    /// Render the Server frame's advanced options — outbound Telnet and
    /// SSH gateway mode choices.  Shown only in the popup.  These are
    /// persisted server-wide so the gateway menus no longer prompt the
    /// operator for mode/auth on every connect.
    fn draw_server_advanced(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("Telnet Gateway").strong().color(AMBER));
        ui.horizontal(|ui| {
            ui.label("Mode:");
            let current = if self.cfg.telnet_gateway_raw {
                "Raw TCP"
            } else {
                "Telnet"
            };
            egui::ComboBox::from_id_salt("telnet_gateway_mode")
                .width(120.0)
                .selected_text(current)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.cfg.telnet_gateway_raw, false, "Telnet");
                    ui.selectable_value(&mut self.cfg.telnet_gateway_raw, true, "Raw TCP");
                });
        });
        ui.add_enabled_ui(!self.cfg.telnet_gateway_raw, |ui| {
            ui.checkbox(
                &mut self.cfg.telnet_gateway_negotiate,
                "Negotiate TTYPE / NAWS with remote (Telnet mode only)",
            );
        });

        ui.add_space(6.0);
        ui.separator();
        ui.add_space(2.0);
        ui.label(egui::RichText::new("SSH Gateway").strong().color(AMBER));
        ui.horizontal(|ui| {
            ui.label("Auth:");
            let display = match self.cfg.ssh_gateway_auth.as_str() {
                "password" => "Password",
                _ => "Key",
            };
            egui::ComboBox::from_id_salt("ssh_gateway_auth")
                .width(120.0)
                .selected_text(display)
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut self.cfg.ssh_gateway_auth,
                        "key".to_string(),
                        "Key",
                    );
                    ui.selectable_value(
                        &mut self.cfg.ssh_gateway_auth,
                        "password".to_string(),
                        "Password",
                    );
                });
        });
        if self.cfg.ssh_gateway_auth != "password" {
            ui.add_space(2.0);
            ui.label(
                egui::RichText::new(
                    "Gateway public key (paste into remote ~/.ssh/authorized_keys):",
                )
                .italics()
                .small(),
            );
            let pubkey = match crate::ssh::client_public_key_openssh() {
                Ok(s) => s,
                Err(e) => format!("<could not load key: {}>", e),
            };
            let mut key_display = pubkey;
            multiline_with_menu(ui, &mut key_display, 2);
        }
    }

    /// Render the Master/Slave serial-extender (relay) options.  Role is a
    /// dropdown; the master gate is a checkbox; the slave's master host /
    /// port / credentials are text fields (password masked).  Changing
    /// these takes effect on the next server restart (the relay listener /
    /// slave client start at boot from `gateway_role`).  Shown only in the
    /// Server "More…" popup.  (No transport control — SSH is the only
    /// implemented relay transport.)
    fn draw_server_relay(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("Master / Slave").strong().color(AMBER));
        let prev_role = self.cfg.gateway_role.clone();
        ui.horizontal(|ui| {
            ui.label("Role:");
            let display = match self.cfg.gateway_role.as_str() {
                "master" => "Master",
                "slave" => "Slave",
                _ => "Standalone",
            };
            egui::ComboBox::from_id_salt("gateway_role")
                .width(120.0)
                .selected_text(display)
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut self.cfg.gateway_role,
                        "standalone".to_string(),
                        "Standalone",
                    );
                    ui.selectable_value(
                        &mut self.cfg.gateway_role,
                        "master".to_string(),
                        "Master",
                    );
                    ui.selectable_value(
                        &mut self.cfg.gateway_role,
                        "slave".to_string(),
                        "Slave",
                    );
                });
        });
        let is_master = self.cfg.gateway_role == "master";
        let is_slave = self.cfg.gateway_role == "slave";
        // On the transition into Master: default the accept-relays gate ON (a
        // master with it off can't accept slaves), and arm a warn-only popup if
        // the SSH server — which the relay listens on — is off.  Never toggles
        // SSH (operator's choice); fires once because prev_role gates it.
        if is_master && prev_role != "master" {
            self.cfg.master_accept_relays = true;
            if !self.cfg.ssh_enabled {
                self.relay_ssh_warn_open = true;
            }
        }
        // "Accept relays" applies to Master only; the master host/port/user/
        // pass apply to Slave only.  Grey out the fields that don't apply.
        ui.add_enabled_ui(is_master, |ui| {
            ui.checkbox(
                &mut self.cfg.master_accept_relays,
                "Master: accept relay connections from slaves",
            );
        });
        ui.add_enabled_ui(is_slave, |ui| {
            ui.horizontal(|ui| {
                labeled_field(ui, "Master host:", &mut self.cfg.slave_master_host, 150.0);
                labeled_field(ui, "Port:", &mut self.slave_master_port_buf, 50.0);
            });
            ui.horizontal(|ui| {
                labeled_field(ui, "User:", &mut self.cfg.slave_master_username, 120.0);
                labeled_password(ui, "Pass:", &mut self.cfg.slave_master_password);
            });
        });
        // No transport control: SSH is the only implemented relay
        // transport; the raw alternative will add one when it lands.
    }

    /// Render the primary row for one port on the main Serial Port
    /// frame: port-device dropdown, baud field, and a "More..." button
    /// that opens this port's advanced popup.  The full bits/parity/
    /// stop/flow row plus AT/S-register state moved into the popup so
    /// the main frame fits both ports plus the header in three rows.
    fn draw_serial_primary_row(
        &mut self,
        ui: &mut egui::Ui,
        id: crate::config::SerialPortId,
    ) {
        let idx = id.index();
        ui.horizontal(|ui| {
            ui.label(format!("Port {}:", id.label()));
            let selected = if self.cfg.port(id).port.is_empty() {
                "(none)".to_string()
            } else {
                self.cfg.port(id).port.clone()
            };
            // Per-port salt so the two ComboBoxes don't share state.
            egui::ComboBox::from_id_salt(format!("serial_port_{}", id.label()))
                .width(180.0)
                .selected_text(&selected)
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut self.cfg.port_mut(id).port,
                        String::new(),
                        "(none)",
                    );
                    for port in &self.serial_ports {
                        ui.selectable_value(
                            &mut self.cfg.port_mut(id).port,
                            port.clone(),
                            port,
                        );
                    }
                });
            if ui
                .small_button("\u{21bb}")
                .on_hover_text("Refresh ports")
                .clicked()
            {
                self.serial_ports = detect_serial_ports();
            }
            ui.add_space(4.0);
            labeled_field(ui, "Baud:", &mut self.serial_baud_buf[idx], 70.0);
            if right_aligned_small_button(ui, "More...") {
                self.serial_popup_open[idx] = true;
            }
        });
    }

    /// Render the framing/flow row inside one port's "More..." popup.
    /// (Used to share the main-layout slot with the primary row before
    /// the dual-port redesign moved framing/flow exclusively to the
    /// popup.)
    fn draw_serial_more_framing_row(
        &mut self,
        ui: &mut egui::Ui,
        id: crate::config::SerialPortId,
    ) {
        ui.horizontal(|ui| {
            ui.label("Bits:");
            egui::ComboBox::from_id_salt(format!("databits_{}", id.label()))
                .width(36.0)
                .selected_text(self.cfg.port(id).databits.to_string())
                .show_ui(ui, |ui| {
                    for b in [5u8, 6, 7, 8] {
                        ui.selectable_value(
                            &mut self.cfg.port_mut(id).databits,
                            b,
                            b.to_string(),
                        );
                    }
                });
            ui.label("Par:");
            egui::ComboBox::from_id_salt(format!("parity_{}", id.label()))
                .width(56.0)
                .selected_text(&self.cfg.port(id).parity)
                .show_ui(ui, |ui| {
                    for p in ["none", "odd", "even"] {
                        ui.selectable_value(
                            &mut self.cfg.port_mut(id).parity,
                            p.to_string(),
                            p,
                        );
                    }
                });
            ui.label("Stop:");
            egui::ComboBox::from_id_salt(format!("stopbits_{}", id.label()))
                .width(36.0)
                .selected_text(self.cfg.port(id).stopbits.to_string())
                .show_ui(ui, |ui| {
                    for s in [1u8, 2] {
                        ui.selectable_value(
                            &mut self.cfg.port_mut(id).stopbits,
                            s,
                            s.to_string(),
                        );
                    }
                });
            ui.label("Flow:");
            egui::ComboBox::from_id_salt(format!("flow_{}", id.label()))
                .width(72.0)
                .selected_text(&self.cfg.port(id).flowcontrol)
                .show_ui(ui, |ui| {
                    for f in ["none", "hardware", "software"] {
                        ui.selectable_value(
                            &mut self.cfg.port_mut(id).flowcontrol,
                            f.to_string(),
                            f,
                        );
                    }
                });
        });
    }

    /// Render the per-port "Mode" selector inside the More popup.
    fn draw_serial_mode_row(
        &mut self,
        ui: &mut egui::Ui,
        id: crate::config::SerialPortId,
    ) {
        ui.horizontal(|ui| {
            ui.label("Mode:");
            egui::ComboBox::from_id_salt(format!("mode_{}", id.label()))
                .width(220.0)
                .selected_text(if self.cfg.port(id).mode == "console" {
                    "Telnet-Serial Mode"
                } else {
                    "Modem (AT Command) Mode"
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut self.cfg.port_mut(id).mode,
                        "modem".into(),
                        "Modem (AT Command) Mode",
                    );
                    ui.selectable_value(
                        &mut self.cfg.port_mut(id).mode,
                        "console".into(),
                        "Telnet-Serial Mode",
                    );
                });
        });
    }

    /// Render the Serial Port frame's advanced options — Hayes AT
    /// saved state, S-registers, and stored phone-number slots.  Shown
    /// only in the popup.  The advanced state is only meaningful when
    /// the port is in `modem` mode; in `console` mode the values are
    /// still persisted but unused.
    fn draw_serial_advanced(
        &mut self,
        ui: &mut egui::Ui,
        id: crate::config::SerialPortId,
    ) {
        ui.label(egui::RichText::new("Hayes AT Saved State").strong().color(AMBER));
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.cfg.port_mut(id).echo, "Echo (E1)");
            ui.add_space(8.0);
            ui.checkbox(&mut self.cfg.port_mut(id).verbose, "Verbose (V1)");
            ui.add_space(8.0);
            ui.checkbox(&mut self.cfg.port_mut(id).quiet, "Quiet (Q1)");
        });
        ui.horizontal(|ui| {
            ui.label("Result level (X):");
            egui::ComboBox::from_id_salt(format!("x_code_{}", id.label()))
                .width(36.0)
                .selected_text(self.cfg.port(id).x_code.to_string())
                .show_ui(ui, |ui| {
                    for x in 0u8..=4 {
                        ui.selectable_value(
                            &mut self.cfg.port_mut(id).x_code,
                            x,
                            x.to_string(),
                        );
                    }
                });
            ui.add_space(8.0);
            ui.label("DTR (&D):");
            egui::ComboBox::from_id_salt(format!("dtr_mode_{}", id.label()))
                .width(36.0)
                .selected_text(self.cfg.port(id).dtr_mode.to_string())
                .show_ui(ui, |ui| {
                    for d in 0u8..=3 {
                        ui.selectable_value(
                            &mut self.cfg.port_mut(id).dtr_mode,
                            d,
                            d.to_string(),
                        );
                    }
                });
            ui.add_space(8.0);
            ui.label("Flow (&K):");
            egui::ComboBox::from_id_salt(format!("flow_mode_{}", id.label()))
                .width(36.0)
                .selected_text(self.cfg.port(id).flow_mode.to_string())
                .show_ui(ui, |ui| {
                    for f in 0u8..=4 {
                        ui.selectable_value(
                            &mut self.cfg.port_mut(id).flow_mode,
                            f,
                            f.to_string(),
                        );
                    }
                });
            ui.add_space(8.0);
            ui.label("DCD (&C):");
            egui::ComboBox::from_id_salt(format!("dcd_mode_{}", id.label()))
                .width(36.0)
                .selected_text(self.cfg.port(id).dcd_mode.to_string())
                .show_ui(ui, |ui| {
                    for c in 0u8..=1 {
                        ui.selectable_value(
                            &mut self.cfg.port_mut(id).dcd_mode,
                            c,
                            c.to_string(),
                        );
                    }
                });
        });
        ui.horizontal(|ui| {
            ui.checkbox(
                &mut self.cfg.port_mut(id).petscii_translate,
                "PETSCII translation (AT+PETSCII)",
            )
            .on_hover_text(
                "Text only. Disable before XMODEM/YMODEM/ZMODEM/Kermit/Punter \
                 transfers over the same TCP session — the translator will \
                 corrupt the binary payload otherwise.",
            );
            ui.label(
                egui::RichText::new("(C64/PET direct-TCP dials)")
                    .small()
                    .color(AMBER),
            );
        });
        ui.horizontal(|ui| {
            ui.checkbox(
                &mut self.cfg.port_mut(id).drive_carrier,
                "Drive carrier (DCD)",
            )
            .on_hover_text(
                "Drive DTR as a carrier proxy: asserted on CONNECT, dropped \
                 on NO CARRIER, per AT&C (&C0 = always on, &C1 = follows \
                 carrier). Wire DTR->DCD via a null-modem cable. Off (default) \
                 = the gateway never touches the modem-control lines, so a \
                 port without DCD wiring is unaffected. Modem mode only.",
            );
            ui.label(
                egui::RichText::new("(DTR->DCD, modem mode)")
                    .small()
                    .color(AMBER),
            );
        });

        ui.add_space(6.0);
        ui.separator();
        ui.add_space(2.0);
        ui.label(egui::RichText::new("S-Registers").strong().color(AMBER));
        ui.label(
            egui::RichText::new(
                "Comma-separated decimal values for S0..S26 (ATSn=v sets, ATSn? reads).",
            )
            .italics()
            .small(),
        );
        multiline_with_menu(ui, &mut self.cfg.port_mut(id).s_regs, 2);

        ui.add_space(6.0);
        ui.separator();
        ui.add_space(2.0);
        ui.label(
            egui::RichText::new("Stored Phone Numbers (AT&Zn=s / ATDSn)")
                .strong()
                .color(AMBER),
        );
        for (i, slot) in self.cfg.port_mut(id).stored_numbers.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                ui.label(format!("&Z{} =", i));
                singleline_with_menu(ui, slot, false, Some(f32::INFINITY));
            });
        }

        ui.add_space(6.0);
        ui.separator();
        ui.add_space(2.0);
        ui.label(
            egui::RichText::new("Direct-to-Kermit Dial Target")
                .strong()
                .color(AMBER),
        );
        // Bind the checkbox to a local copy and detect a change against
        // the saved state.  This lets us intercept the off→on transition
        // and gate it behind a confirmation popup before persisting.
        // On→off is one-click safe (tightening security never needs a
        // confirmation) — persist immediately.  Toggling against the
        // popup-open state is suppressed so a second click while the
        // popup is up doesn't double-fire.
        ui.horizontal(|ui| {
            let mut local = self.cfg.allow_atdt_kermit;
            let prev = local;
            let resp = ui.checkbox(&mut local, "Allow ATDT KERMIT");
            ui.label(
                egui::RichText::new("(bypasses security)")
                    .small()
                    .color(AMBER),
            );
            if resp.changed() && !self.atdt_kermit_warn_open {
                if local && !prev {
                    // Off → on: revert the visible state, open the
                    // confirmation popup; the popup's Enable button
                    // will commit the change if the operator confirms.
                    self.atdt_kermit_warn_open = true;
                } else if !local && prev {
                    // On → off: commit immediately, no popup.
                    self.cfg.allow_atdt_kermit = false;
                    self.last_synced_cfg.allow_atdt_kermit = false;
                    config::update_config_value("allow_atdt_kermit", "false");
                    logger::log("ATDT KERMIT disabled.".into());
                }
            }
        });
        // Peer-dial toggle — a plain checkbox (no popup): it lets a modem
        // port dial another port directly (ATD Port@IP) or ring a modem
        // port picked from the Serial Gateway menu, instead of always
        // landing on the gateway menu.  Persisted immediately either way.
        ui.horizontal(|ui| {
            if ui
                .checkbox(&mut self.cfg.allow_peer_dial, "Allow peer-dial")
                .changed()
            {
                self.last_synced_cfg.allow_peer_dial = self.cfg.allow_peer_dial;
                config::update_config_value(
                    "allow_peer_dial",
                    if self.cfg.allow_peer_dial { "true" } else { "false" },
                );
            }
            ui.label(
                egui::RichText::new("(ATD Port@IP / ring modem ports)")
                    .small()
                    .color(AMBER),
            );
        });
    }

    /// Render the File Transfer frame's primary rows.  The main layout
    /// shows the transfer directory plus a quick-glance timeouts row
    /// (Negotiate / Block / Retries) carrying the XMODEM-family values;
    /// the popup shows only the directory row because the timeouts are
    /// repeated in the per-protocol advanced section just below it.
    ///
    /// When `with_more_button` is true, a right-aligned "More..." button
    /// is appended to the timeouts row; the popup passes false (no More
    /// button needed once you're already in the More view).
    fn draw_file_transfer_controls(&mut self, ui: &mut egui::Ui, with_more_button: bool) {
        ui.horizontal(|ui| {
            ui.label("Dir:");
            let btn_w = 32.0;
            let text_w = (ui.available_width() - btn_w - 4.0).max(60.0);
            singleline_with_menu(ui, &mut self.cfg.transfer_dir, false, Some(text_w));
            let browse = ui.add_enabled(
                self.pending_dir_pick.is_none(),
                egui::Button::new("\u{1F4C1}").small(),
            );
            if browse.on_hover_text("Browse for folder").clicked() {
                self.pending_dir_pick = Some(spawn_folder_picker(&self.cfg.transfer_dir));
            }
        });
        if with_more_button {
            ui.horizontal(|ui| {
                labeled_field(ui, "Negotiate:", &mut self.negotiation_timeout_buf, 40.0);
                labeled_field(ui, "Block:", &mut self.block_timeout_buf, 40.0);
                labeled_field(ui, "Retries:", &mut self.max_retries_buf, 40.0);
                if right_aligned_small_button(ui, "More...") {
                    self.file_transfer_popup_open = true;
                }
            });
        }
    }

    /// Render the File Transfer frame's advanced options — a per-
    /// protocol breakdown with XMODEM/YMODEM/ZMODEM sections.  Shown
    /// only in the File Transfer popup.  XMODEM and YMODEM share the
    /// same `xmodem_*` keys since they use the same protocol code
    /// path in `xmodem.rs`; ZMODEM has its own independent timeouts
    /// defined in `config.rs`.
    fn draw_file_transfer_advanced(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("XMODEM / XMODEM-1K / YMODEM").strong().color(AMBER));
        ui.label(
            egui::RichText::new(
                "Shared timeouts — XMODEM, XMODEM-1K, and YMODEM all use the same code path.",
            )
            .italics()
            .small(),
        );
        ui.horizontal(|ui| {
            labeled_field(ui, "Negotiate (s):", &mut self.negotiation_timeout_buf, 50.0);
            labeled_field(ui, "Block (s):", &mut self.block_timeout_buf, 50.0);
            labeled_field(ui, "Retries:", &mut self.max_retries_buf, 50.0);
        });
        ui.horizontal(|ui| {
            labeled_field(
                ui,
                "Retry interval (s):",
                &mut self.negotiation_retry_interval_buf,
                50.0,
            );
            ui.label(
                egui::RichText::new(
                    "(seconds between C/NAK pokes during handshake; spec suggests ~10)",
                )
                .italics()
                .small(),
            );
        });

        ui.add_space(6.0);
        ui.separator();
        ui.add_space(2.0);
        ui.label(egui::RichText::new("ZMODEM").strong().color(AMBER));
        ui.label(
            egui::RichText::new(
                "Independent ZMODEM tunables (handshake budget, per-frame read timeout, retry cap).",
            )
            .italics()
            .small(),
        );
        ui.horizontal(|ui| {
            labeled_field(
                ui,
                "Negotiate (s):",
                &mut self.zmodem_negotiation_timeout_buf,
                50.0,
            );
            labeled_field(
                ui,
                "Frame (s):",
                &mut self.zmodem_frame_timeout_buf,
                50.0,
            );
            labeled_field(ui, "Retries:", &mut self.zmodem_max_retries_buf, 50.0);
        });
        ui.horizontal(|ui| {
            labeled_field(
                ui,
                "Retry interval (s):",
                &mut self.zmodem_negotiation_retry_interval_buf,
                50.0,
            );
            ui.label(
                egui::RichText::new("(ZRINIT / ZRQINIT re-send gap; default 5)")
                    .italics()
                    .small(),
            );
        });

        ui.add_space(6.0);
        ui.separator();
        ui.add_space(2.0);
        ui.label(egui::RichText::new("KERMIT").strong().color(AMBER));
        ui.label(
            egui::RichText::new(
                "Full-spec Kermit — auto-negotiates with the peer's CAPAS bits. \
                 Streaming is a big speed win on TCP/SSH; turn it off only when \
                 bridging into an unreliable serial line.",
            )
            .italics()
            .small(),
        );
        ui.horizontal(|ui| {
            labeled_field(
                ui,
                "Negotiate (s):",
                &mut self.kermit_negotiation_timeout_buf,
                50.0,
            );
            labeled_field(
                ui,
                "Packet (s):",
                &mut self.kermit_packet_timeout_buf,
                50.0,
            );
            labeled_field(ui, "Retries:", &mut self.kermit_max_retries_buf, 50.0);
        });
        ui.horizontal(|ui| {
            labeled_field(
                ui,
                "Idle timeout (s, 0=disabled):",
                &mut self.kermit_idle_timeout_buf,
                50.0,
            );
        });
        ui.horizontal(|ui| {
            labeled_field(
                ui,
                "Max packet:",
                &mut self.kermit_max_packet_length_buf,
                60.0,
            );
            labeled_field(ui, "Window:", &mut self.kermit_window_size_buf, 40.0);
            labeled_field(
                ui,
                "Check (1/2/3):",
                &mut self.kermit_block_check_type_buf,
                40.0,
            );
        });
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.cfg.kermit_long_packets, "Long packets");
            ui.checkbox(&mut self.cfg.kermit_sliding_windows, "Sliding window");
            ui.checkbox(&mut self.cfg.kermit_streaming, "Streaming");
        });
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.cfg.kermit_attribute_packets, "Attribute pkts");
            ui.checkbox(&mut self.cfg.kermit_repeat_compression, "Repeat compress");
        });
        ui.horizontal(|ui| {
            ui.label("8-bit quote:");
            egui::ComboBox::from_id_salt("kermit_8bit_quote_combo")
                .selected_text(&self.cfg.kermit_8bit_quote)
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut self.cfg.kermit_8bit_quote,
                        "auto".into(),
                        "auto",
                    );
                    ui.selectable_value(
                        &mut self.cfg.kermit_8bit_quote,
                        "on".into(),
                        "on",
                    );
                    ui.selectable_value(
                        &mut self.cfg.kermit_8bit_quote,
                        "off".into(),
                        "off",
                    );
                });
        });
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.cfg.kermit_locking_shifts, "Locking shifts");
            ui.checkbox(&mut self.cfg.kermit_resume_partial, "Resume partial uploads");
        });
        ui.horizontal(|ui| {
            ui.checkbox(
                &mut self.cfg.kermit_wait_for_receiver,
                "Wait for receiver NAK (download)",
            );
        });
        ui.horizontal(|ui| {
            labeled_field(
                ui,
                "Resume max age (h):",
                &mut self.kermit_resume_max_age_hours_buf,
                50.0,
            );
        });

        ui.add_space(6.0);
        ui.separator();
        ui.add_space(2.0);
        ui.label(egui::RichText::new("PUNTER").strong().color(AMBER));
        ui.label(
            egui::RichText::new(
                "Punter C1 — the protocol CCGMS / Novaterm speak on Commodore BBSes. \
                 Block size 255 is the native max (248-byte payload); lower it toward \
                 40 for noisy lines.",
            )
            .italics()
            .small(),
        );
        ui.horizontal(|ui| {
            labeled_field(
                ui,
                "Block size (8-255):",
                &mut self.punter_block_size_buf,
                50.0,
            );
            labeled_field(
                ui,
                "Negotiate (s):",
                &mut self.punter_negotiation_timeout_buf,
                50.0,
            );
        });
        ui.horizontal(|ui| {
            labeled_field(
                ui,
                "Block (s):",
                &mut self.punter_block_timeout_buf,
                50.0,
            );
            labeled_field(ui, "Retries:", &mut self.punter_max_retries_buf, 50.0);
            labeled_field(ui, "Bad rounds:", &mut self.punter_max_bad_rounds_buf, 50.0);
            labeled_field(
                ui,
                "Retry interval (s):",
                &mut self.punter_negotiation_retry_interval_buf,
                50.0,
            );
        });
        // Bound directly to cfg; the frame-level dirty check (cfg !=
        // last_synced_cfg) detects the toggle and persists it on save.
        ui.checkbox(
            &mut self.cfg.punter_hangup_on_failure,
            "Hang up (drop carrier) on a failed transfer \u{2014} frees a stranded C64 (C1 has no in-band abort)",
        );
    }

    /// Flush numeric text buffers into `cfg`, persist to disk, refresh
    /// the sync snapshot, and clear the dirty flag.  Shared prefix for
    /// every Save action; callers follow it with a log line and any
    /// restart signals they need.
    fn persist_config(&mut self) -> Result<(), String> {
        self.sync_numeric_fields();
        let result = config::save_config(&self.cfg);
        self.last_synced_cfg = self.cfg.clone();
        self.dirty = false;
        result
    }

    /// Persist config; leaves the server running (no restart).  Used by
    /// the popup Save buttons and the per-frame Save buttons on frames
    /// whose fields are all runtime-safe.
    fn save_config_now(&mut self) {
        match self.persist_config() {
            Ok(()) => logger::log("Configuration saved.".into()),
            Err(e) => logger::log(format!("Configuration NOT saved: {}", e)),
        }
    }

    /// Persist config and trigger a full server restart.  Used by the
    /// Server frame's Save and Restart button.
    fn save_and_restart_all(&mut self) {
        match self.persist_config() {
            Ok(()) => logger::log("Configuration saved — restarting server...".into()),
            Err(e) => {
                logger::log(format!("Configuration NOT saved: {} — restarting anyway...", e))
            }
        }
        // Set restart BEFORE shutdown so the main loop sees the intent to
        // restart when it checks after join().
        self.restart.store(true, Ordering::SeqCst);
        self.shutdown.store(true, Ordering::SeqCst);
    }

    /// Persist config and signal both serial managers to reopen their
    /// ports with the new settings.  Leaves telnet/SSH sessions
    /// untouched.  The GUI Save button is the only call site, and it
    /// might have changed either or both ports — restarting both is
    /// cheaper than diffing config slices and avoids the bug where a
    /// saved change is silently ignored.
    fn save_and_restart_serial(&mut self) {
        let saved = self.persist_config();
        crate::serial::restart_all_serial();
        match saved {
            Ok(()) => logger::log("Configuration saved — serial ports reconfigured.".into()),
            Err(e) => logger::log(format!(
                "Configuration NOT saved ({}) — serial ports reconfigured.",
                e
            )),
        }
    }

    /// Render the console panel as a single read-only multiline `TextEdit`.
    /// Doing this (instead of one label per line) gives us native mouse-drag
    /// selection plus our standard right-click menu — including the
    /// selection-restore-on-right-click fix.  The buffer is rebuilt from
    /// `console_lines` every frame, so any user keystrokes that slip in
    /// (the `TextEdit` is technically editable) are silently discarded.
    fn draw_console_textedit(&mut self, ui: &mut egui::Ui) {
        let mut text = self.console_lines.join("\n");
        let row_count = self.console_lines.len().max(1);

        let id = ui.next_auto_id();
        let prev_range = TextEditState::load(ui.ctx(), id)
            .and_then(|s| s.cursor.char_range());

        let te = egui::TextEdit::multiline(&mut text)
            .font(egui::TextStyle::Monospace)
            .text_color(CONSOLE_TEXT)
            .desired_width(f32::INFINITY)
            .desired_rows(row_count)
            .frame(egui::Frame::NONE);

        let mut output = te.show(ui);
        restore_selection_after_right_click(
            ui.ctx(),
            id,
            &output.response.response,
            &mut output.state,
            prev_range,
        );

        let cursor_range = output.state.cursor.char_range();
        let response = output.response.response.clone();
        let mut state = output.state;
        let ctx = ui.ctx().clone();
        let lines_joined = self.console_lines.join("\n");

        response.context_menu(move |ui| {
            let has_selection = cursor_range.is_some_and(|r| !r.is_empty());
            ui.add_enabled_ui(has_selection, |ui| {
                if ui.button("Copy").clicked() {
                    if let Some(range) = cursor_range {
                        let [start, end] = range.sorted_cursors();
                        let (s, e) = (start.index, end.index);
                        let selected: String =
                            text.chars().skip(s).take(e.saturating_sub(s)).collect();
                        ctx.copy_text(selected);
                    }
                    ui.close();
                }
            });
            if ui.button("Copy all").clicked() {
                ctx.copy_text(lines_joined);
                ui.close();
            }
            ui.separator();
            if ui.button("Select All").clicked() {
                let len = text.chars().count();
                state.cursor.set_char_range(Some(CCursorRange::two(
                    CCursor::new(0),
                    CCursor::new(len),
                )));
                state.clone().store(&ctx, id);
                ctx.memory_mut(|mem| mem.request_focus(id));
                ui.close();
            }
        });
    }

    /// Pull the global config singleton and, if it changed since our last
    /// sync (i.e. a telnet/SSH session persisted a setting), refresh every
    /// GUI field to match.
    /// Remember the window's position + inner size so the next launch reopens
    /// it where the operator left it.  Auto-managed: writes straight to
    /// `gui_window_geometry` in the config, with no UI surface.  Debounced so a
    /// drag doesn't rewrite the file on every frame.  On Wayland the compositor
    /// doesn't report an outer position (`outer_rect` is None) — we skip, so
    /// geometry simply isn't remembered there.
    fn track_window_geometry(&mut self, ctx: &egui::Context) {
        let (pos, size, now) = ctx.input(|i| {
            let vp = i.viewport();
            (
                vp.outer_rect.map(|r| r.min),
                vp.inner_rect.map(|r| r.size()),
                i.time,
            )
        });
        let (Some(pos), Some(size)) = (pos, size) else {
            return;
        };
        let geom = (
            pos.x.round() as i32,
            pos.y.round() as i32,
            size.x.round() as i32,
            size.y.round() as i32,
        );
        // Ignore bogus / minimized rects.
        if geom.2 < 320 || geom.3 < 240 {
            return;
        }
        if Some(geom) != self.last_seen_geom {
            self.last_seen_geom = Some(geom);
            self.geom_changed_at = now;
        }
        // Persist once the geometry has settled (~1.5 s after the last move or
        // resize) so dragging the window doesn't rewrite the config each frame.
        if now - self.geom_changed_at > 1.5 && Some(geom) != self.saved_geom {
            self.saved_geom = Some(geom);
            let val = format!("{},{},{},{}", geom.0, geom.1, geom.2, geom.3);
            // Keep cfg + last_synced in sync so refresh_from_global doesn't see
            // this self-write as an external change.
            self.cfg.gui_window_geometry = val.clone();
            self.last_synced_cfg.gui_window_geometry = val.clone();
            config::update_config_value("gui_window_geometry", &val);
        }
    }

    fn refresh_from_global(&mut self) {
        if self.dirty {
            return; // Don't overwrite fields the user is actively editing.
        }
        let global = config::get_config();
        if global == self.last_synced_cfg {
            return;
        }
        self.cfg = global.clone();
        self.last_synced_cfg = global;
        // Rebuild the string buffers that back numeric text fields.
        self.telnet_port_buf = self.cfg.telnet_port.to_string();
        self.ssh_port_buf = self.cfg.ssh_port.to_string();
        self.kermit_server_port_buf = self.cfg.kermit_server_port.to_string();
        self.web_port_buf = self.cfg.web_port.to_string();
        self.slave_master_port_buf = self.cfg.slave_master_port.to_string();
        self.max_sessions_buf = self.cfg.max_sessions.to_string();
        self.idle_timeout_buf = self.cfg.idle_timeout_secs.to_string();
        self.negotiation_timeout_buf = self.cfg.xmodem_negotiation_timeout.to_string();
        self.block_timeout_buf = self.cfg.xmodem_block_timeout.to_string();
        self.max_retries_buf = self.cfg.xmodem_max_retries.to_string();
        self.negotiation_retry_interval_buf =
            self.cfg.xmodem_negotiation_retry_interval.to_string();
        self.zmodem_negotiation_timeout_buf = self.cfg.zmodem_negotiation_timeout.to_string();
        self.zmodem_frame_timeout_buf = self.cfg.zmodem_frame_timeout.to_string();
        self.zmodem_max_retries_buf = self.cfg.zmodem_max_retries.to_string();
        self.zmodem_negotiation_retry_interval_buf =
            self.cfg.zmodem_negotiation_retry_interval.to_string();
        self.kermit_negotiation_timeout_buf =
            self.cfg.kermit_negotiation_timeout.to_string();
        self.kermit_packet_timeout_buf = self.cfg.kermit_packet_timeout.to_string();
        self.kermit_idle_timeout_buf = self.cfg.kermit_idle_timeout.to_string();
        self.kermit_max_retries_buf = self.cfg.kermit_max_retries.to_string();
        self.kermit_resume_max_age_hours_buf =
            self.cfg.kermit_resume_max_age_hours.to_string();
        self.kermit_max_packet_length_buf =
            self.cfg.kermit_max_packet_length.to_string();
        self.kermit_window_size_buf = self.cfg.kermit_window_size.to_string();
        self.kermit_block_check_type_buf =
            self.cfg.kermit_block_check_type.to_string();
        self.punter_block_size_buf = self.cfg.punter_block_size.to_string();
        self.punter_negotiation_timeout_buf =
            self.cfg.punter_negotiation_timeout.to_string();
        self.punter_block_timeout_buf = self.cfg.punter_block_timeout.to_string();
        self.punter_max_retries_buf = self.cfg.punter_max_retries.to_string();
        self.punter_max_bad_rounds_buf = self.cfg.punter_max_bad_rounds.to_string();
        self.punter_negotiation_retry_interval_buf =
            self.cfg.punter_negotiation_retry_interval.to_string();
        for id in crate::config::SERIAL_PORT_IDS {
            self.serial_baud_buf[id.index()] = self.cfg.port(id).baud.to_string();
        }
    }
}

/// Helper: labeled text field in a horizontal row.
fn labeled_field(ui: &mut egui::Ui, label: &str, buf: &mut String, width: f32) {
    ui.label(label);
    singleline_with_menu(ui, buf, false, Some(width));
}

/// Helper: pad the horizontal cursor so the just-rendered widget
/// occupies exactly `target_w` total width.  `used_w` is the widget's
/// actual width from its Response.rect.  Used by the Server frame's
/// listener rows to align the `Port:` labels between rows even when
/// the preceding checkbox labels differ in length (Telnet vs. SSH,
/// Web Server vs. Kermit Server).
fn pad_to(ui: &mut egui::Ui, target_w: f32, used_w: f32) {
    let remaining = target_w - used_w;
    if remaining > 0.0 {
        ui.add_space(remaining);
    }
}

/// Helper: render a small button right-aligned in the current horizontal
/// row.  Returns true if the button was clicked this frame.
fn right_aligned_small_button(ui: &mut egui::Ui, label: &str) -> bool {
    ui.with_layout(
        egui::Layout::right_to_left(egui::Align::Center),
        |ui| ui.small_button(label).clicked(),
    )
    .inner
}

/// Helper: labeled password field in a horizontal row.
fn labeled_password(ui: &mut egui::Ui, label: &str, buf: &mut String) {
    ui.label(label);
    singleline_with_menu(ui, buf, true, None);
}

/// A singleline `TextEdit` with a Cut/Copy/Paste/Select All right-click menu.
/// When `password` is true, Cut/Copy are disabled so the password text is
/// never written to the clipboard.
fn singleline_with_menu(
    ui: &mut egui::Ui,
    buf: &mut String,
    password: bool,
    desired_width: Option<f32>,
) -> egui::Response {
    let id = ui.next_auto_id();
    let prev_range = TextEditState::load(ui.ctx(), id)
        .and_then(|s| s.cursor.char_range());

    let mut te = egui::TextEdit::singleline(buf).password(password);
    if let Some(w) = desired_width {
        te = te.desired_width(w);
    }
    let mut output = te.show(ui);
    restore_selection_after_right_click(
        ui.ctx(),
        id,
        &output.response.response,
        &mut output.state,
        prev_range,
    );
    attach_text_edit_menu(ui.ctx(), &output.response.response, output.state, buf, password);
    output.response.response
}

/// A multiline (full-width) `TextEdit` with a Cut/Copy/Paste/Select All
/// right-click menu.
fn multiline_with_menu(
    ui: &mut egui::Ui,
    buf: &mut String,
    desired_rows: usize,
) -> egui::Response {
    let id = ui.next_auto_id();
    let prev_range = TextEditState::load(ui.ctx(), id)
        .and_then(|s| s.cursor.char_range());

    let te = egui::TextEdit::multiline(buf)
        .desired_rows(desired_rows)
        .desired_width(f32::INFINITY);
    let mut output = te.show(ui);
    restore_selection_after_right_click(
        ui.ctx(),
        id,
        &output.response.response,
        &mut output.state,
        prev_range,
    );
    attach_text_edit_menu(ui.ctx(), &output.response.response, output.state, buf, false);
    output.response.response
}

/// Egui's `TextEdit` collapses any active selection on every mouse *press*,
/// including the secondary (right) press that summons our context menu — so
/// by the time the menu opens, the selection is gone and Copy is not useful.
///
/// We have to act on the **press** frame (when the selection was actually
/// cleared) rather than the click/release frame: by release the persisted
/// state is already empty, so `prev_range` would be empty too.  We detect a
/// secondary press over this widget, then restore the selection that was
/// captured from the *previous* frame's state.
fn restore_selection_after_right_click(
    ctx: &egui::Context,
    id: egui::Id,
    response: &egui::Response,
    state: &mut TextEditState,
    prev_range: Option<CCursorRange>,
) {
    let secondary_press_on_widget = response.contains_pointer()
        && ctx.input(|i| i.pointer.button_pressed(egui::PointerButton::Secondary));
    if !secondary_press_on_widget {
        return;
    }
    let Some(prev) = prev_range else { return };
    if prev.is_empty() {
        return;
    }
    let cleared = state.cursor.char_range().is_none_or(|r| r.is_empty());
    if cleared {
        state.cursor.set_char_range(Some(prev));
        state.clone().store(ctx, id);
    }
}

/// Attach a right-click context menu (Cut / Copy / Paste / Select All) to a
/// `TextEdit` that has already been rendered.  The freshly-loaded `state` is
/// re-stored after any cursor or buffer mutation so the next frame picks up
/// the change.
fn attach_text_edit_menu(
    ctx: &egui::Context,
    response: &egui::Response,
    mut state: TextEditState,
    buf: &mut String,
    password: bool,
) {
    let cursor_range = state.cursor.char_range();
    let id = response.id;
    let ctx = ctx.clone();

    response.context_menu(move |ui| {
        let has_selection = cursor_range.is_some_and(|r| !r.is_empty());

        ui.add_enabled_ui(has_selection && !password, |ui| {
            if ui.button("Cut").clicked() {
                if let Some(range) = cursor_range {
                    let [start, end] = range.sorted_cursors();
                    let (s, e) = (start.index, end.index);
                    let selected: String =
                        buf.chars().skip(s).take(e.saturating_sub(s)).collect();
                    ctx.copy_text(selected);
                    let mut new_buf = String::with_capacity(buf.len());
                    new_buf.extend(buf.chars().take(s));
                    new_buf.extend(buf.chars().skip(e));
                    *buf = new_buf;
                    state.cursor.set_char_range(Some(CCursorRange::one(CCursor::new(s))));
                    state.clone().store(&ctx, id);
                }
                ui.close();
            }
            if ui.button("Copy").clicked() {
                if let Some(range) = cursor_range {
                    let [start, end] = range.sorted_cursors();
                    let (s, e) = (start.index, end.index);
                    let selected: String =
                        buf.chars().skip(s).take(e.saturating_sub(s)).collect();
                    ctx.copy_text(selected);
                }
                ui.close();
            }
        });
        if ui.button("Paste").clicked() {
            if let Ok(mut cb) = arboard::Clipboard::new()
                && let Ok(text) = cb.get_text()
            {
                let (s, e) = match cursor_range {
                    Some(range) => {
                        let [start, end] = range.sorted_cursors();
                        (start.index, end.index)
                    }
                    None => {
                        let n = buf.chars().count();
                        (n, n)
                    }
                };
                let mut new_buf = String::with_capacity(buf.len() + text.len());
                new_buf.extend(buf.chars().take(s));
                new_buf.push_str(&text);
                new_buf.extend(buf.chars().skip(e));
                *buf = new_buf;
                let new_pos = s + text.chars().count();
                state.cursor.set_char_range(Some(CCursorRange::one(CCursor::new(new_pos))));
                state.clone().store(&ctx, id);
            }
            ui.close();
        }
        ui.separator();
        if ui.button("Select All").clicked() {
            let len = buf.chars().count();
            state.cursor.set_char_range(Some(CCursorRange::two(
                CCursor::new(0),
                CCursor::new(len),
            )));
            state.clone().store(&ctx, id);
            // Focus the field so the selection is visible.
            ctx.memory_mut(|mem| mem.request_focus(id));
            ui.close();
        }
    });
}

impl eframe::App for App {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        BG_DARKEST.to_normalized_gamma_f32()
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Apply theme on first frame (after renderer is fully initialized)
        if !self.theme_applied {
            apply_theme(ui.ctx());
            self.theme_applied = true;
        }

        // Pin the display scale when the operator set an explicit `gui_zoom`.
        // `None` ("auto") leaves egui following the monitor's own scale factor;
        // a number overrides pixels-per-point absolutely so a display that
        // reports an inflated DPI doesn't render the console oversized.  egui
        // only repaints on an actual change, and the guard keeps us from
        // requesting one every frame once the value has settled.
        if let Some(ppp) = self.cfg.gui_zoom_factor() {
            if (ui.ctx().pixels_per_point() - ppp).abs() > f32::EPSILON {
                ui.ctx().set_pixels_per_point(ppp);
            }
        }

        // Close the GUI window when the server shuts down
        if self.shutdown.load(Ordering::SeqCst) {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        }

        self.poll_logs();
        self.poll_dir_pick();
        self.refresh_from_global();
        self.track_window_geometry(ui.ctx());

        ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));

        // ── Console panel (bottom) ────────────────────────────
        egui::Panel::bottom("console_panel")
            .resizable(true)
            .min_size(140.0)
            .default_size(240.0)
            .show_inside(ui, |ui| {
                egui::Frame::NONE.fill(CONSOLE_BG).show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new("Console Output").size(16.0).strong().color(AMBER));
                    ui.separator();
                    egui::ScrollArea::vertical()
                        .stick_to_bottom(true)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            self.draw_console_textedit(ui);
                        });
                });
            });

        // ── Config editor (remaining space) ───────────────────
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let avail = ui.available_width();
                let half = (avail - 16.0) / 2.0;
                // Row height based on line spacing so frames match
                let line_h = ui.text_style_height(&egui::TextStyle::Body);
                let row_h = line_h * 3.5 + 16.0;

                ui.horizontal(|ui| {
                    ui.heading(
                        egui::RichText::new(format!(
                            "Ethernet Gateway v{}",
                            env!("CARGO_PKG_VERSION")
                        ))
                        .strong()
                        .color(AMBER_BRIGHT),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_space(8.0);
                        ui.label(
                            egui::RichText::new(&self.local_ip)
                                .color(AMBER)
                                .monospace()
                                .size(16.0),
                        );
                        ui.label(
                            egui::RichText::new("Server IP:")
                                .color(AMBER)
                                .monospace()
                                .size(16.0),
                        );
                    });
                });
                ui.add_space(4.0);

                // ── Row 1: Server + Security ──────────────────
                ui.horizontal_top(|ui| {
                    ui.allocate_ui_with_layout(
                        egui::vec2(half, 0.0),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            egui::Frame::group(ui.style()).show(ui, |ui| {
                                ui.set_min_height(row_h);
                                ui.set_min_width(ui.available_width());
                                ui.horizontal(|ui| {
                                    ui.label(egui::RichText::new("Server").strong().color(AMBER));
                                    ui.label(
                                        egui::RichText::new("(Changes Require Restart)")
                                            .italics()
                                            .color(AMBER_DIM),
                                    );
                                    if right_aligned_small_button(ui, "Save and Restart") {
                                        self.save_and_restart_all();
                                    }
                                });
                                self.draw_server_controls(ui, true);
                            });
                        },
                    );

                    ui.allocate_ui_with_layout(
                        egui::vec2(half, 0.0),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            egui::Frame::group(ui.style()).show(ui, |ui| {
                                ui.set_min_height(row_h);
                                ui.set_min_width(ui.available_width());
                                ui.horizontal(|ui| {
                                    ui.label(egui::RichText::new("Security").strong().color(AMBER));
                                    ui.add_space(8.0);
                                    ui.checkbox(&mut self.cfg.security_enabled, "Require Login");
                                    ui.add_space(12.0);
                                    // Disable-IP-safety binds to a local
                                    // copy so the off→on transition can
                                    // be intercepted by the
                                    // confirmation popup.  On→off is
                                    // safe (re-tightens the allowlist).
                                    let mut local_dis = self.cfg.disable_ip_safety;
                                    let prev_dis = local_dis;
                                    let resp = ui.checkbox(&mut local_dis, "Disable IP Safety");
                                    if resp.changed() && !self.disable_ip_safety_warn_open {
                                        if local_dis && !prev_dis {
                                            self.disable_ip_safety_warn_open = true;
                                        } else if !local_dis && prev_dis {
                                            self.cfg.disable_ip_safety = false;
                                            self.last_synced_cfg.disable_ip_safety = false;
                                            config::update_config_value("disable_ip_safety", "false");
                                            logger::log("IP-safety allowlist re-enabled.".into());
                                        }
                                    }
                                    if right_aligned_small_button(ui, "Save") {
                                        self.save_config_now();
                                    }
                                });
                                ui.horizontal(|ui| {
                                    // Telnet, SSH, and the web UI share the
                                    // same credential pair now — one User
                                    // and one Pass field cover all three.
                                    // Earlier the frame rendered separate
                                    // Telnet and SSH rows; the dimmed
                                    // "Login" label preserves the visual
                                    // weight of the leading row label.
                                    ui.label(egui::RichText::new("Login").color(AMBER_DIM));
                                    labeled_field(ui, "User:", &mut self.cfg.username, 70.0);
                                    labeled_password(ui, "Pass:", &mut self.cfg.password);
                                });
                                // Spacer row replaces the dropped SSH row
                                // so the Security frame retains the same
                                // height as the adjacent Server frame.
                                // Without this the Security frame would
                                // shrink and break the side-by-side
                                // row-pair layout.
                                ui.allocate_space(egui::vec2(0.0, line_h));
                            });
                        },
                    );
                });
                ui.add_space(4.0);

                // ── Row 2: File Transfer + AI/Browser ─────────
                ui.horizontal_top(|ui| {
                    ui.allocate_ui_with_layout(
                        egui::vec2(half, 0.0),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            egui::Frame::group(ui.style()).show(ui, |ui| {
                                ui.set_min_height(row_h);
                                ui.set_min_width(ui.available_width());
                                ui.horizontal(|ui| {
                                    ui.label(egui::RichText::new("File Transfer (XMODEM)").strong().color(AMBER));
                                    ui.label(
                                        egui::RichText::new("(More for others)")
                                            .italics()
                                            .color(AMBER_DIM),
                                    );
                                    if right_aligned_small_button(ui, "Save") {
                                        self.save_config_now();
                                    }
                                });
                                self.draw_file_transfer_controls(ui, true);
                            });
                        },
                    );

                    ui.allocate_ui_with_layout(
                        egui::vec2(half, 0.0),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            egui::Frame::group(ui.style()).show(ui, |ui| {
                                ui.set_min_height(row_h);
                                ui.set_min_width(ui.available_width());
                                ui.horizontal(|ui| {
                                    ui.label(egui::RichText::new("AI Chat, Browser, and Weather").strong().color(AMBER));
                                    if right_aligned_small_button(ui, "Save") {
                                        self.save_config_now();
                                    }
                                });
                                ui.horizontal(|ui| {
                                    ui.label("API Key:");
                                    singleline_with_menu(ui, &mut self.cfg.groq_api_key, true, None);
                                });
                                // Home row carries the "More..." button (weather
                                // location + units live in that popup), keeping
                                // this frame at three rows.  The homepage field
                                // is width-bounded so the button has room.
                                ui.horizontal(|ui| {
                                    labeled_field(ui, "Home:", &mut self.cfg.browser_homepage, 190.0);
                                    if right_aligned_small_button(ui, "More...") {
                                        self.ai_browser_popup_open = true;
                                    }
                                });
                            });
                        },
                    );
                });
                ui.add_space(4.0);

                // ── Row 3: Serial Ports (left) + General (right) ──
                // Serial frame: header with both ports' Enabled
                // checkboxes plus a Save button, then one row per port
                // (device dropdown, baud, More button into per-port
                // popup).  General frame on the right shares the row,
                // matching the half-width layout of the other paired
                // frames above.
                ui.horizontal_top(|ui| {
                    ui.allocate_ui_with_layout(
                        egui::vec2(half, 0.0),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            egui::Frame::group(ui.style()).show(ui, |ui| {
                                ui.set_min_width(ui.available_width());
                                ui.horizontal(|ui| {
                                    ui.label(egui::RichText::new("Serial Port A").strong().color(AMBER));
                                    ui.checkbox(&mut self.cfg.serial_a.enabled, "Enabled");
                                    ui.add_space(12.0);
                                    ui.label(egui::RichText::new("Serial Port B").strong().color(AMBER));
                                    ui.checkbox(&mut self.cfg.serial_b.enabled, "Enabled");
                                    if right_aligned_small_button(ui, "Save") {
                                        self.save_and_restart_serial();
                                    }
                                });
                                self.draw_serial_primary_row(ui, crate::config::SerialPortId::A);
                                self.draw_serial_primary_row(ui, crate::config::SerialPortId::B);
                            });
                        },
                    );

                    ui.allocate_ui_with_layout(
                        egui::vec2(half, 0.0),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            egui::Frame::group(ui.style()).show(ui, |ui| {
                                ui.set_min_width(ui.available_width());
                                ui.horizontal(|ui| {
                                    ui.label(egui::RichText::new("General").strong().color(AMBER));
                                    if right_aligned_small_button(ui, "Save") {
                                        self.save_config_now();
                                    }
                                });
                                ui.checkbox(&mut self.cfg.verbose, "Verbose Transfer Logging");
                                ui.horizontal(|ui| {
                                    ui.checkbox(&mut self.cfg.gateway_debug, "Gateway Debug Trace");
                                    ui.add_space(40.0);
                                    ui.checkbox(&mut self.cfg.enable_console, "Show GUI on Startup");
                                });
                            });
                        },
                    );
                });
                ui.add_space(6.0);

                // ── User Manual button ────────────────────────
                ui.horizontal(|ui| {
                    if ui
                        .add(egui::Button::new(
                            egui::RichText::new("User Manual")
                                .strong()
                                .size(16.0)
                                .color(AMBER_BRIGHT),
                        ))
                        .clicked()
                    {
                        ui.ctx().open_url(egui::OpenUrl::new_tab(
                            "https://github.com/rickybryce/ethernetgateway/blob/master/usermanual.pdf",
                        ));
                    }
                });
                ui.add_space(20.0);
                // ── Scripture (left) + Logo (right) ──────────
                // The PNG ships at exactly the logical-pixel display
                // size (366x183) so on a 1.0x-DPI display the GPU does
                // a 1:1 blit — no minification, no filtering artifacts.
                // Earlier builds resized 1024x512 down to ~366x183 and
                // that minification (even at Linear with mipmaps off)
                // had a faint mauve cast on the dark-blue gradients.
                // On HiDPI displays the GPU still magnifies to physical
                // pixels; Linear filtering keeps the magnified result
                // smooth without introducing the mipmap-bleed problem.
                let logo_w = 366.0_f32;
                let logo_h = 183.0_f32;
                ui.horizontal_top(|ui| {
                    ui.allocate_ui_with_layout(
                        egui::vec2(half, logo_h),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            ui.label(
                                egui::RichText::new(
                                    "\u{201c}For God so loved the world, that he gave \
                                     his only begotten Son, that whosoever believeth in \
                                     him should not perish, but have everlasting life.\u{201d}"
                                )
                                .italics()
                                .strong()
                                .size(17.0)
                                .color(SCRIPTURE),
                            );
                            ui.label(
                                egui::RichText::new("\u{2014} John 3:16, KJV")
                                    .italics()
                                    .strong()
                                    .size(15.0)
                                    .color(SCRIPTURE),
                            );
                        },
                    );

                    ui.allocate_ui_with_layout(
                        egui::vec2(half, logo_h + 32.0),
                        egui::Layout::top_down(egui::Align::Max),
                        |ui| {
                            ui.add_space(-32.0);
                            ui.add(
                                egui::Image::new(egui::include_image!("../ethernetgatewaylogo_small.png"))
                                    .texture_options(egui::TextureOptions {
                                        magnification: egui::TextureFilter::Linear,
                                        minification: egui::TextureFilter::Linear,
                                        mipmap_mode: None,
                                        ..Default::default()
                                    })
                                    .fit_to_exact_size(egui::vec2(logo_w, logo_h)),
                            );
                        },
                    );
                });
                ui.add_space(20.0);
            });

        // ── Advanced-options popups ──────────────────────────
        // Drawn after the scroll area so they float above the main
        // layout.  Each popup mirrors the primary controls and adds
        // per-frame advanced fields, with its own Save button.
        let ctx = ui.ctx().clone();
        // Dark-burgundy frame so popups read as distinct from the
        // navy main panels.  Derived from the window style so corner
        // radius, shadow, and inner margin stay consistent.
        let popup_frame = egui::Frame::window(&ctx.global_style())
            .fill(POPUP_BG)
            .stroke(Stroke::new(1.0_f32, AMBER));
        // Warning popups get a dark-red panel + red border so they read as
        // clearly distinct from ordinary (green) popups — making it obvious the
        // modal must be acknowledged before the next click lands.
        let warn_frame = egui::Frame::window(&ctx.global_style())
            .fill(WARN_BG)
            .stroke(Stroke::new(1.5_f32, WARN_BORDER));

        let mut server_open = self.server_popup_open;
        egui::Window::new(egui::RichText::new("Server — More").strong().color(AMBER_BRIGHT))
            .open(&mut server_open)
            .resizable(true)
            .collapsible(false)
            // 462 ≈ 440 × 1.05.  The previous 440-wide window clipped
            // the trailing digit of 4-digit port values inside the
            // listener-grid input boxes; widening by ~5 % gives the
            // port inputs visible padding without bumping the popup
            // big enough to look misplaced against the half-width
            // frame underneath.
            .default_width(462.0)
            .frame(popup_frame)
            .show(&ctx, |ui| {
                // Lighter-green text-entry backgrounds scoped to this popup.
                ui.visuals_mut().extreme_bg_color = POPUP_INPUT_BG;
                self.draw_server_controls(ui, false);
                ui.add_space(6.0);
                ui.separator();
                ui.add_space(4.0);
                self.draw_server_more_only(ui);
                ui.add_space(6.0);
                ui.separator();
                ui.add_space(4.0);
                self.draw_server_advanced(ui);
                ui.add_space(6.0);
                ui.separator();
                ui.add_space(4.0);
                self.draw_server_relay(ui);
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(4.0);
                if ui
                    .add(egui::Button::new(
                        egui::RichText::new("Save and Restart")
                            .strong()
                            .size(16.0)
                            .color(AMBER_BRIGHT),
                    ))
                    .clicked()
                {
                    self.save_and_restart_all();
                }
            });
        self.server_popup_open = server_open;

        // AI, Browser & Weather — More popup.  Surfaces every option in the
        // group (API key, homepage, weather location, weather units); the main
        // frame shows only the API key + homepage to stay at three rows.
        let mut ai_browser_open = self.ai_browser_popup_open;
        egui::Window::new(
            egui::RichText::new("AI, Browser & Weather — More")
                .strong()
                .color(AMBER_BRIGHT),
        )
        .open(&mut ai_browser_open)
        .resizable(true)
        .collapsible(false)
        .default_width(420.0)
        .frame(popup_frame)
        .show(&ctx, |ui| {
            ui.visuals_mut().extreme_bg_color = POPUP_INPUT_BG;
            self.draw_ai_browser_more(ui);
            ui.add_space(8.0);
            ui.separator();
            ui.add_space(4.0);
            if ui
                .add(egui::Button::new(
                    egui::RichText::new("Save")
                        .strong()
                        .size(16.0)
                        .color(AMBER_BRIGHT),
                ))
                .clicked()
            {
                self.save_config_now();
            }
        });
        self.ai_browser_popup_open = ai_browser_open;

        // One independent popup per port — each shows that port's
        // mode selector, framing/flow row, AT/S-register state, stored
        // numbers, and a Save button.  Both can be open simultaneously
        // so the operator can compare settings side-by-side.
        for id in crate::config::SERIAL_PORT_IDS {
            let idx = id.index();
            let mut serial_open = self.serial_popup_open[idx];
            let title = format!("Serial Port {} — More", id.label());
            egui::Window::new(
                egui::RichText::new(&title).strong().color(AMBER_BRIGHT),
            )
            .id(egui::Id::new(format!("serial_popup_{}", id.label())))
            .open(&mut serial_open)
            .resizable(true)
            .collapsible(false)
            .default_width(520.0)
            .frame(popup_frame)
            .show(&ctx, |ui| {
                ui.visuals_mut().extreme_bg_color = POPUP_INPUT_BG;
                self.draw_serial_mode_row(ui, id);
                ui.add_space(4.0);
                self.draw_serial_more_framing_row(ui, id);
                ui.add_space(6.0);
                ui.separator();
                ui.add_space(4.0);
                self.draw_serial_advanced(ui, id);
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(4.0);
                if ui
                    .add(egui::Button::new(
                        egui::RichText::new("Save")
                            .strong()
                            .size(16.0)
                            .color(AMBER_BRIGHT),
                    ))
                    .clicked()
                {
                    self.save_and_restart_serial();
                }
            });
            self.serial_popup_open[idx] = serial_open;
        }

        let mut ft_open = self.file_transfer_popup_open;
        egui::Window::new(
            egui::RichText::new("File Transfer — More")
                .strong()
                .color(AMBER_BRIGHT),
        )
        .open(&mut ft_open)
        .resizable(true)
        .collapsible(false)
        .default_width(520.0)
        .frame(popup_frame)
        .show(&ctx, |ui| {
            ui.visuals_mut().extreme_bg_color = POPUP_INPUT_BG;
            self.draw_file_transfer_controls(ui, false);
            ui.add_space(6.0);
            ui.separator();
            ui.add_space(4.0);
            self.draw_file_transfer_advanced(ui);
            ui.add_space(8.0);
            ui.separator();
            ui.add_space(4.0);
            if ui
                .add(egui::Button::new(
                    egui::RichText::new("Save")
                        .strong()
                        .size(16.0)
                        .color(AMBER_BRIGHT),
                ))
                .clicked()
            {
                self.save_config_now();
            }
        });
        self.file_transfer_popup_open = ft_open;

        // ATDT KERMIT enable-confirmation popup.  Shown when the
        // operator first ticks the checkbox in the Serial — More popup;
        // requires explicit Enable click to actually flip the bit
        // because the feature bypasses the telnet auth gate.  Cancel
        // (or closing the X) leaves `allow_atdt_kermit` at its prior
        // false value — the checkbox snaps back automatically because
        // we never wrote the change to `cfg`.
        let mut warn_open = self.atdt_kermit_warn_open;
        let mut close_warn = false;
        let mut commit_enable = false;
        egui::Window::new(
            egui::RichText::new("Enable ATDT KERMIT?")
                .strong()
                .color(AMBER_BRIGHT),
        )
        .open(&mut warn_open)
        .resizable(false)
        .collapsible(false)
        .default_width(440.0)
        .frame(warn_frame)
        .show(&ctx, |ui| {
            ui.visuals_mut().extreme_bg_color = POPUP_INPUT_BG;
            ui.label(
                egui::RichText::new("Security warning")
                    .strong()
                    .color(AMBER),
            );
            ui.add_space(4.0);
            ui.label(
                "Enabling this lets anyone who can dial the serial \
                 modem reach Kermit server mode directly — bypassing \
                 the telnet menu's username/password gate. There is \
                 no auth on this dial path.",
            );
            ui.add_space(6.0);
            ui.label(
                "If your gateway is configured with security_enabled = \
                 true and you need every caller to authenticate, leave \
                 this OFF and have callers go through the telnet menu \
                 instead: F (File Transfer) then K (Kermit Server \
                 Mode). That path runs the auth prompt before handing \
                 off to Kermit.",
            );
            ui.add_space(6.0);
            ui.label(
                "Enable only when the serial line itself is trusted \
                 (private cable, isolated lab, single-user setup).",
            );
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui
                    .add(egui::Button::new(
                        egui::RichText::new("Enable")
                            .strong()
                            .color(AMBER_BRIGHT),
                    ))
                    .clicked()
                {
                    commit_enable = true;
                    close_warn = true;
                }
                ui.add_space(8.0);
                if ui
                    .add(egui::Button::new(
                        egui::RichText::new("Cancel").strong(),
                    ))
                    .clicked()
                {
                    close_warn = true;
                }
            });
        });
        if commit_enable {
            self.cfg.allow_atdt_kermit = true;
            self.last_synced_cfg.allow_atdt_kermit = true;
            config::update_config_value("allow_atdt_kermit", "true");
            logger::log("ATDT KERMIT enabled.".into());
        }
        if close_warn {
            warn_open = false;
        }
        self.atdt_kermit_warn_open = warn_open;

        // Master-needs-SSH popup (warn-only — never toggles SSH, per the
        // operator's choice).  Armed in draw_server_relay when the role is
        // switched to Master while the SSH server is off.  Just an OK to
        // dismiss; the message points the operator at the SSH setting.
        let mut ssh_warn_open = self.relay_ssh_warn_open;
        let mut ssh_warn_close = false;
        egui::Window::new(
            egui::RichText::new("Master needs SSH")
                .strong()
                .color(AMBER_BRIGHT),
        )
        .open(&mut ssh_warn_open)
        .resizable(false)
        .collapsible(false)
        .default_width(440.0)
        .frame(warn_frame)
        .show(&ctx, |ui| {
            ui.visuals_mut().extreme_bg_color = POPUP_INPUT_BG;
            ui.label(
                "Slaves connect to a master over the SSH server, which is \
                 currently disabled. Enable SSH (Server settings) and Save & \
                 Restart, otherwise slaves cannot connect.",
            );
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new("SSH is not changed automatically.").color(AMBER),
            );
            ui.add_space(10.0);
            if ui
                .add(egui::Button::new(
                    egui::RichText::new("OK").strong().color(AMBER_BRIGHT),
                ))
                .clicked()
            {
                ssh_warn_close = true;
            }
        });
        if ssh_warn_close {
            ssh_warn_open = false;
        }
        self.relay_ssh_warn_open = ssh_warn_open;

        // Kermit server enable-confirmation popup.  Same posture as
        // the ATDT KERMIT popup: the off→on transition arms the popup;
        // the visible checkbox state is left at false until the
        // operator clicks Enable.  Cancelling (or closing the X) leaves
        // `kermit_server_enabled` false because no commit ran.  The
        // standalone listener bypasses both authentication AND the
        // private-IP allowlist that the telnet/SSH listeners apply
        // when `security_enabled` is off, so we want the operator's
        // intent on record before binding the port.
        let mut ks_warn_open = self.kermit_server_warn_open;
        let mut ks_close = false;
        let mut ks_commit = false;
        egui::Window::new(
            egui::RichText::new("Enable Kermit server?")
                .strong()
                .color(AMBER_BRIGHT),
        )
        .open(&mut ks_warn_open)
        .resizable(false)
        .collapsible(false)
        .default_width(440.0)
        .frame(warn_frame)
        .show(&ctx, |ui| {
            ui.visuals_mut().extreme_bg_color = POPUP_INPUT_BG;
            ui.label(
                egui::RichText::new("Security warning")
                    .strong()
                    .color(AMBER),
            );
            ui.add_space(4.0);
            ui.label(
                "Enabling this opens a dedicated TCP port that drops \
                 every accepted connection straight into Kermit \
                 server mode — no telnet menu, no username, no \
                 password, no private-IP filter.",
            );
            ui.add_space(6.0);
            ui.label(
                "Anyone who can reach the listener can read and write \
                 files in your transfer directory. The standalone \
                 listener does not consult security_enabled or any \
                 lockout state.",
            );
            ui.add_space(6.0);
            ui.label(
                "Enable only when the network path itself is trusted \
                 (LAN you control, isolated lab, single-user setup). \
                 Restart the server after saving for the listener to \
                 bind.",
            );
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui
                    .add(egui::Button::new(
                        egui::RichText::new("Enable")
                            .strong()
                            .color(AMBER_BRIGHT),
                    ))
                    .clicked()
                {
                    ks_commit = true;
                    ks_close = true;
                }
                ui.add_space(8.0);
                if ui
                    .add(egui::Button::new(
                        egui::RichText::new("Cancel").strong(),
                    ))
                    .clicked()
                {
                    ks_close = true;
                }
            });
        });
        if ks_commit {
            self.cfg.kermit_server_enabled = true;
            self.last_synced_cfg.kermit_server_enabled = true;
            config::update_config_value("kermit_server_enabled", "true");
            logger::log("Kermit server enabled.".into());
        }
        if ks_close {
            ks_warn_open = false;
        }
        self.kermit_server_warn_open = ks_warn_open;

        // Disable-IP-safety enable-confirmation popup.  Off→on arms the
        // popup; the checkbox visible state stays false until the
        // operator clicks Enable.  Cancel leaves `disable_ip_safety`
        // unchanged (the change never reached `cfg`).  Removing the
        // private-IP allowlist is the riskiest single toggle the GUI
        // exposes when `security_enabled` is off, so the operator's
        // intent goes on record before the listener accepts public-IP
        // connections.
        let mut dis_warn_open = self.disable_ip_safety_warn_open;
        let mut dis_close = false;
        let mut dis_commit = false;
        egui::Window::new(
            egui::RichText::new("Disable IP safety?")
                .strong()
                .color(AMBER_BRIGHT),
        )
        .open(&mut dis_warn_open)
        .resizable(false)
        .collapsible(false)
        .default_width(440.0)
        .frame(warn_frame)
        .show(&ctx, |ui| {
            ui.visuals_mut().extreme_bg_color = POPUP_INPUT_BG;
            ui.label(
                egui::RichText::new("Security warning")
                    .strong()
                    .color(AMBER),
            );
            ui.add_space(4.0);
            ui.label(
                "When Require Login is off, the telnet listener accepts \
                 connections only from private/loopback/link-local \
                 addresses, and rejects gateway-style *.*.*.1 \
                 addresses. That allowlist is the only thing standing \
                 between a public IP and an unauthenticated session.",
            );
            ui.add_space(6.0);
            ui.label(
                "Enabling this checkbox removes the allowlist entirely. \
                 Anyone on the public internet who can reach your \
                 telnet port will be able to connect — and without \
                 Require Login, they will not need a password.",
            );
            ui.add_space(6.0);
            ui.label(
                "Enable only when you have a different control in front \
                 of the listener (LAN-only firewall rule, VPN, port \
                 not exposed to the internet) or when you are about to \
                 turn Require Login on. The change takes effect on the \
                 next inbound connection.",
            );
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui
                    .add(egui::Button::new(
                        egui::RichText::new("Enable")
                            .strong()
                            .color(AMBER_BRIGHT),
                    ))
                    .clicked()
                {
                    dis_commit = true;
                    dis_close = true;
                }
                ui.add_space(8.0);
                if ui
                    .add(egui::Button::new(
                        egui::RichText::new("Cancel").strong(),
                    ))
                    .clicked()
                {
                    dis_close = true;
                }
            });
        });
        if dis_commit {
            self.cfg.disable_ip_safety = true;
            self.last_synced_cfg.disable_ip_safety = true;
            config::update_config_value("disable_ip_safety", "true");
            logger::log("IP-safety allowlist disabled.".into());
        }
        if dis_close {
            dis_warn_open = false;
        }
        self.disable_ip_safety_warn_open = dis_warn_open;

        // Detect whether the user has unsaved edits.  Compare bound
        // config fields against the last-synced snapshot so that
        // refresh_from_global will not overwrite in-progress changes.
        if !self.dirty {
            self.dirty = self.cfg != self.last_synced_cfg
                || self.telnet_port_buf != self.last_synced_cfg.telnet_port.to_string()
                || self.ssh_port_buf != self.last_synced_cfg.ssh_port.to_string()
                || self.kermit_server_port_buf != self.last_synced_cfg.kermit_server_port.to_string()
                || self.web_port_buf != self.last_synced_cfg.web_port.to_string()
                || self.slave_master_port_buf != self.last_synced_cfg.slave_master_port.to_string()
                || self.max_sessions_buf != self.last_synced_cfg.max_sessions.to_string()
                || self.idle_timeout_buf != self.last_synced_cfg.idle_timeout_secs.to_string()
                || self.negotiation_timeout_buf != self.last_synced_cfg.xmodem_negotiation_timeout.to_string()
                || self.block_timeout_buf != self.last_synced_cfg.xmodem_block_timeout.to_string()
                || self.max_retries_buf != self.last_synced_cfg.xmodem_max_retries.to_string()
                || self.negotiation_retry_interval_buf != self.last_synced_cfg.xmodem_negotiation_retry_interval.to_string()
                || self.zmodem_negotiation_timeout_buf != self.last_synced_cfg.zmodem_negotiation_timeout.to_string()
                || self.zmodem_frame_timeout_buf != self.last_synced_cfg.zmodem_frame_timeout.to_string()
                || self.zmodem_max_retries_buf != self.last_synced_cfg.zmodem_max_retries.to_string()
                || self.zmodem_negotiation_retry_interval_buf != self.last_synced_cfg.zmodem_negotiation_retry_interval.to_string()
                || self.kermit_negotiation_timeout_buf != self.last_synced_cfg.kermit_negotiation_timeout.to_string()
                || self.kermit_packet_timeout_buf != self.last_synced_cfg.kermit_packet_timeout.to_string()
                || self.kermit_idle_timeout_buf != self.last_synced_cfg.kermit_idle_timeout.to_string()
                || self.kermit_max_retries_buf != self.last_synced_cfg.kermit_max_retries.to_string()
                || self.kermit_resume_max_age_hours_buf != self.last_synced_cfg.kermit_resume_max_age_hours.to_string()
                || self.kermit_max_packet_length_buf != self.last_synced_cfg.kermit_max_packet_length.to_string()
                || self.kermit_window_size_buf != self.last_synced_cfg.kermit_window_size.to_string()
                || self.kermit_block_check_type_buf != self.last_synced_cfg.kermit_block_check_type.to_string()
                || self.punter_block_size_buf != self.last_synced_cfg.punter_block_size.to_string()
                || self.punter_negotiation_timeout_buf != self.last_synced_cfg.punter_negotiation_timeout.to_string()
                || self.punter_block_timeout_buf != self.last_synced_cfg.punter_block_timeout.to_string()
                || self.punter_max_retries_buf != self.last_synced_cfg.punter_max_retries.to_string()
                || self.punter_max_bad_rounds_buf != self.last_synced_cfg.punter_max_bad_rounds.to_string()
                || self.punter_negotiation_retry_interval_buf != self.last_synced_cfg.punter_negotiation_retry_interval.to_string()
                || self.serial_baud_buf[0] != self.last_synced_cfg.serial_a.baud.to_string()
                || self.serial_baud_buf[1] != self.last_synced_cfg.serial_b.baud.to_string();
        }
    }
}

// ─── Tests ─────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a test App with default config and fresh shutdown/restart flags.
    fn test_app() -> App {
        App::new(
            Config::default(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        )
    }

    // ── App::new initialization ──────────────────────────────

    #[test]
    fn test_parse_window_geometry() {
        assert_eq!(parse_window_geometry("100,120,1280,900"), Some((100, 120, 1280, 900)));
        assert_eq!(parse_window_geometry(" -50 , 0 , 800 , 600 "), Some((-50, 0, 800, 600)));
        assert_eq!(parse_window_geometry(""), None); // unset
        assert_eq!(parse_window_geometry("1,2,3"), None); // too few
        assert_eq!(parse_window_geometry("1,2,3,4,5"), None); // too many
        assert_eq!(parse_window_geometry("a,b,c,d"), None); // non-numeric
        assert_eq!(parse_window_geometry("0,0,100,100"), None); // below min size
        assert_eq!(parse_window_geometry("0,0,99999,99999"), None); // above max
    }

    #[test]
    fn test_app_new_buffers_match_config() {
        let app = test_app();
        assert_eq!(app.telnet_port_buf, app.cfg.telnet_port.to_string());
        assert_eq!(app.ssh_port_buf, app.cfg.ssh_port.to_string());
        assert_eq!(
            app.kermit_server_port_buf,
            app.cfg.kermit_server_port.to_string()
        );
        assert_eq!(app.web_port_buf, app.cfg.web_port.to_string());
        assert_eq!(
            app.slave_master_port_buf,
            app.cfg.slave_master_port.to_string()
        );
        assert_eq!(app.max_sessions_buf, app.cfg.max_sessions.to_string());
        assert_eq!(app.idle_timeout_buf, app.cfg.idle_timeout_secs.to_string());
        assert_eq!(app.negotiation_timeout_buf, app.cfg.xmodem_negotiation_timeout.to_string());
        assert_eq!(app.block_timeout_buf, app.cfg.xmodem_block_timeout.to_string());
        assert_eq!(app.max_retries_buf, app.cfg.xmodem_max_retries.to_string());
        assert_eq!(
            app.negotiation_retry_interval_buf,
            app.cfg.xmodem_negotiation_retry_interval.to_string()
        );
        assert_eq!(
            app.zmodem_negotiation_timeout_buf,
            app.cfg.zmodem_negotiation_timeout.to_string()
        );
        assert_eq!(app.zmodem_frame_timeout_buf, app.cfg.zmodem_frame_timeout.to_string());
        assert_eq!(app.zmodem_max_retries_buf, app.cfg.zmodem_max_retries.to_string());
        assert_eq!(
            app.zmodem_negotiation_retry_interval_buf,
            app.cfg.zmodem_negotiation_retry_interval.to_string()
        );
        assert_eq!(
            app.kermit_negotiation_timeout_buf,
            app.cfg.kermit_negotiation_timeout.to_string()
        );
        assert_eq!(
            app.kermit_packet_timeout_buf,
            app.cfg.kermit_packet_timeout.to_string()
        );
        assert_eq!(
            app.kermit_idle_timeout_buf,
            app.cfg.kermit_idle_timeout.to_string()
        );
        assert_eq!(
            app.kermit_max_retries_buf,
            app.cfg.kermit_max_retries.to_string()
        );
        assert_eq!(
            app.kermit_resume_max_age_hours_buf,
            app.cfg.kermit_resume_max_age_hours.to_string()
        );
        assert_eq!(
            app.kermit_max_packet_length_buf,
            app.cfg.kermit_max_packet_length.to_string()
        );
        assert_eq!(
            app.kermit_window_size_buf,
            app.cfg.kermit_window_size.to_string()
        );
        assert_eq!(
            app.kermit_block_check_type_buf,
            app.cfg.kermit_block_check_type.to_string()
        );
        assert_eq!(app.serial_baud_buf[0], app.cfg.serial_a.baud.to_string());
        assert_eq!(app.serial_baud_buf[1], app.cfg.serial_b.baud.to_string());
    }

    #[test]
    fn test_app_new_defaults() {
        let app = test_app();
        assert!(app.console_lines.is_empty());
        assert!(!app.theme_applied);
        assert!(!app.shutdown.load(Ordering::SeqCst));
        assert!(!app.restart.load(Ordering::SeqCst));
        assert!(!app.local_ip.is_empty());
    }

    // ── sync_numeric_fields ──────────────────────────────────

    #[test]
    fn test_sync_valid_values() {
        let mut app = test_app();
        app.telnet_port_buf = "8080".into();
        app.ssh_port_buf = "3333".into();
        app.kermit_server_port_buf = "2525".into();
        app.web_port_buf = "9090".into();
        app.max_sessions_buf = "100".into();
        app.idle_timeout_buf = "1800".into();
        app.negotiation_timeout_buf = "60".into();
        app.block_timeout_buf = "30".into();
        app.max_retries_buf = "5".into();
        app.negotiation_retry_interval_buf = "9".into();
        app.zmodem_negotiation_timeout_buf = "90".into();
        app.zmodem_frame_timeout_buf = "45".into();
        app.zmodem_max_retries_buf = "7".into();
        app.zmodem_negotiation_retry_interval_buf = "8".into();
        app.kermit_negotiation_timeout_buf = "55".into();
        app.kermit_packet_timeout_buf = "11".into();
        app.kermit_max_retries_buf = "6".into();
        app.kermit_resume_max_age_hours_buf = "72".into();
        app.kermit_max_packet_length_buf = "2048".into();
        app.kermit_window_size_buf = "8".into();
        app.kermit_block_check_type_buf = "2".into();
        app.serial_baud_buf = ["115200".into(), "57600".into()];
        app.sync_numeric_fields();
        assert_eq!(app.cfg.telnet_port, 8080);
        assert_eq!(app.cfg.ssh_port, 3333);
        assert_eq!(app.cfg.kermit_server_port, 2525);
        assert_eq!(app.cfg.web_port, 9090);
        assert_eq!(app.cfg.max_sessions, 100);
        assert_eq!(app.cfg.idle_timeout_secs, 1800);
        assert_eq!(app.cfg.xmodem_negotiation_timeout, 60);
        assert_eq!(app.cfg.xmodem_block_timeout, 30);
        assert_eq!(app.cfg.xmodem_max_retries, 5);
        assert_eq!(app.cfg.xmodem_negotiation_retry_interval, 9);
        assert_eq!(app.cfg.zmodem_negotiation_timeout, 90);
        assert_eq!(app.cfg.zmodem_frame_timeout, 45);
        assert_eq!(app.cfg.zmodem_max_retries, 7);
        assert_eq!(app.cfg.zmodem_negotiation_retry_interval, 8);
        assert_eq!(app.cfg.kermit_negotiation_timeout, 55);
        assert_eq!(app.cfg.kermit_packet_timeout, 11);
        assert_eq!(app.cfg.kermit_max_retries, 6);
        assert_eq!(app.cfg.kermit_resume_max_age_hours, 72);
        assert_eq!(app.cfg.kermit_max_packet_length, 2048);
        assert_eq!(app.cfg.kermit_window_size, 8);
        assert_eq!(app.cfg.kermit_block_check_type, 2);
        assert_eq!(app.cfg.serial_a.baud, 115200);
        assert_eq!(app.cfg.serial_b.baud, 57600);
    }

    #[test]
    fn test_kermit_window_clamps_to_range() {
        let mut app = test_app();
        let orig_window = app.cfg.kermit_window_size;
        // Out-of-range values should leave config untouched.
        app.kermit_window_size_buf = "0".into();
        app.sync_numeric_fields();
        assert_eq!(app.cfg.kermit_window_size, orig_window);
        app.kermit_window_size_buf = "32".into();
        app.sync_numeric_fields();
        assert_eq!(app.cfg.kermit_window_size, orig_window);
        // In-range value should apply.
        app.kermit_window_size_buf = "31".into();
        app.sync_numeric_fields();
        assert_eq!(app.cfg.kermit_window_size, 31);
    }

    #[test]
    fn test_kermit_max_packet_length_clamps() {
        let mut app = test_app();
        let orig = app.cfg.kermit_max_packet_length;
        // Below MIN (10)
        app.kermit_max_packet_length_buf = "9".into();
        app.sync_numeric_fields();
        assert_eq!(app.cfg.kermit_max_packet_length, orig);
        // Above MAX (9024)
        app.kermit_max_packet_length_buf = "9025".into();
        app.sync_numeric_fields();
        assert_eq!(app.cfg.kermit_max_packet_length, orig);
        // Boundary — 10 and 9024 both accepted.
        app.kermit_max_packet_length_buf = "10".into();
        app.sync_numeric_fields();
        assert_eq!(app.cfg.kermit_max_packet_length, 10);
        app.kermit_max_packet_length_buf = "9024".into();
        app.sync_numeric_fields();
        assert_eq!(app.cfg.kermit_max_packet_length, 9024);
    }

    #[test]
    fn test_kermit_block_check_type_clamps() {
        let mut app = test_app();
        let orig = app.cfg.kermit_block_check_type;
        for bad in &["0", "4", "abc", "-1"] {
            app.kermit_block_check_type_buf = (*bad).into();
            app.sync_numeric_fields();
            assert_eq!(app.cfg.kermit_block_check_type, orig);
        }
        for good in &["1", "2", "3"] {
            app.kermit_block_check_type_buf = (*good).into();
            app.sync_numeric_fields();
            assert_eq!(
                app.cfg.kermit_block_check_type,
                good.parse::<u8>().unwrap()
            );
        }
    }

    #[test]
    fn test_sync_invalid_leaves_original() {
        let mut app = test_app();
        let orig_port = app.cfg.telnet_port;
        let orig_baud_a = app.cfg.serial_a.baud;
        let orig_baud_b = app.cfg.serial_b.baud;
        app.telnet_port_buf = "not_a_number".into();
        app.serial_baud_buf = ["".into(), "".into()];
        app.sync_numeric_fields();
        assert_eq!(app.cfg.telnet_port, orig_port);
        assert_eq!(app.cfg.serial_a.baud, orig_baud_a);
        assert_eq!(app.cfg.serial_b.baud, orig_baud_b);
    }

    /// Invalid or zero ZMODEM buffers must not clobber the existing
    /// config values.  Matches the xmodem_* buffer guarantees so the
    /// two families behave identically for bad input.
    #[test]
    fn test_sync_zmodem_invalid_leaves_original() {
        let mut app = test_app();
        let orig_neg = app.cfg.zmodem_negotiation_timeout;
        let orig_frame = app.cfg.zmodem_frame_timeout;
        let orig_retries = app.cfg.zmodem_max_retries;
        let orig_retry = app.cfg.zmodem_negotiation_retry_interval;
        app.zmodem_negotiation_timeout_buf = "nope".into();
        app.zmodem_frame_timeout_buf = "0".into(); // below min
        app.zmodem_max_retries_buf = "-3".into(); // negative parse-fails as u32
        app.zmodem_negotiation_retry_interval_buf = "0".into(); // below min
        app.sync_numeric_fields();
        assert_eq!(app.cfg.zmodem_negotiation_timeout, orig_neg);
        assert_eq!(app.cfg.zmodem_frame_timeout, orig_frame);
        assert_eq!(app.cfg.zmodem_max_retries, orig_retries);
        assert_eq!(app.cfg.zmodem_negotiation_retry_interval, orig_retry);
    }

    #[test]
    fn test_sync_boundary_values() {
        let mut app = test_app();
        let orig_ssh = app.cfg.ssh_port;
        // u16 max for ports
        app.telnet_port_buf = "65535".into();
        app.ssh_port_buf = "0".into(); // port 0 is rejected (minimum is 1)
        app.sync_numeric_fields();
        assert_eq!(app.cfg.telnet_port, 65535);
        assert_eq!(app.cfg.ssh_port, orig_ssh);
    }

    #[test]
    fn test_sync_overflow_leaves_original() {
        let mut app = test_app();
        let orig = app.cfg.telnet_port;
        // u16 overflow
        app.telnet_port_buf = "70000".into();
        app.sync_numeric_fields();
        assert_eq!(app.cfg.telnet_port, orig);
    }

    #[test]
    fn test_sync_negative_leaves_unsigned() {
        let mut app = test_app();
        let orig_port = app.cfg.telnet_port;
        let orig_sessions = app.cfg.max_sessions;
        app.telnet_port_buf = "-1".into();
        app.max_sessions_buf = "-5".into();
        app.sync_numeric_fields();
        assert_eq!(app.cfg.telnet_port, orig_port);
        assert_eq!(app.cfg.max_sessions, orig_sessions);
    }

    #[test]
    fn test_sync_partial_invalid() {
        let mut app = test_app();
        // Valid port, invalid baud on both serial ports — only port should update
        app.telnet_port_buf = "9999".into();
        let orig_baud_a = app.cfg.serial_a.baud;
        let orig_baud_b = app.cfg.serial_b.baud;
        app.serial_baud_buf = ["abc".into(), "abc".into()];
        app.sync_numeric_fields();
        assert_eq!(app.cfg.telnet_port, 9999);
        assert_eq!(app.cfg.serial_a.baud, orig_baud_a);
        assert_eq!(app.cfg.serial_b.baud, orig_baud_b);
    }

    /// Updating Port A's baud buffer doesn't bleed into Port B and
    /// vice versa.  Direct guard for the per-port buffer indexing.
    #[test]
    fn test_sync_baud_isolated_per_port() {
        let mut app = test_app();
        app.serial_baud_buf[0] = "57600".into();
        app.serial_baud_buf[1] = "115200".into();
        app.sync_numeric_fields();
        assert_eq!(app.cfg.serial_a.baud, 57600);
        assert_eq!(app.cfg.serial_b.baud, 115200);
    }

    // ── poll_logs buffer cap ─────────────────────────────────

    // The logger is a process-global buffer, so these two tests would
    // otherwise race: one test's poll_logs() (a drain) can swallow the
    // line the other test just logged. Serialize them and clear residue
    // up front so each sees only its own entries.
    static LOG_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn test_poll_logs_caps_at_2000() {
        let _guard = LOG_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        logger::init();
        let _ = logger::drain(); // discard any residue from other tests
        let mut app = test_app();
        // Pre-fill with 1990 lines
        for i in 0..1990 {
            app.console_lines.push(format!("line {}", i));
        }
        // Push 20 more through the logger
        for i in 0..20 {
            logger::log(format!("new {}", i));
        }
        app.poll_logs();
        assert!(app.console_lines.len() <= 2000);
    }

    #[test]
    fn test_poll_logs_trims_oldest() {
        let _guard = LOG_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        logger::init();
        let _ = logger::drain(); // discard any residue from other tests
        let mut app = test_app();
        // Fill to exactly 2000
        for i in 0..2000 {
            app.console_lines.push(format!("old {}", i));
        }
        // Add one more through logger
        logger::log("newest".into());
        app.poll_logs();
        assert!(app.console_lines.len() <= 2000);
        assert_eq!(app.console_lines.last().expect("should contain newest"), "newest");
    }

    // ── local_ip ─────────────────────────────────────────────

    #[test]
    fn test_local_ip_returns_string() {
        let ip = local_ip();
        // Must return either a valid IPv4 address or "unknown"
        assert!(
            ip == "unknown" || ip.parse::<std::net::Ipv4Addr>().is_ok(),
            "local_ip() returned unexpected value: {}",
            ip
        );
    }

    // ── detect_serial_ports ──────────────────────────────────

    #[test]
    fn test_detect_serial_ports_returns_vec() {
        // Should not panic regardless of hardware present
        let ports = detect_serial_ports();
        // Each entry should be a non-empty path
        for port in &ports {
            assert!(!port.is_empty());
        }
    }

    // ── Color palette constants ──────────────────────────────

    #[test]
    fn test_palette_colors_are_opaque() {
        // All theme colors should be fully opaque (alpha = 255)
        let colors = [
            BG_DARKEST, BG_DARK, BG_MID, BG_LIGHT, BORDER,
            AMBER, AMBER_BRIGHT, AMBER_DIM,
            TEXT_PRIMARY, TEXT_INPUT,
            GREEN, CONSOLE_TEXT, SCRIPTURE, CONSOLE_BG, SELECTION,
        ];
        for (i, color) in colors.iter().enumerate() {
            assert_eq!(color.a(), 255, "Color index {} is not fully opaque", i);
        }
    }

    #[test]
    fn test_palette_bg_gradient_ordering() {
        // Background colors should get progressively lighter
        fn luminance(c: Color32) -> u16 {
            c.r() as u16 + c.g() as u16 + c.b() as u16
        }
        assert!(luminance(BG_DARKEST) < luminance(BG_DARK));
        assert!(luminance(BG_DARK) < luminance(BG_MID));
        assert!(luminance(BG_MID) < luminance(BG_LIGHT));
    }

    #[test]
    fn test_amber_brightness_ordering() {
        fn luminance(c: Color32) -> u16 {
            c.r() as u16 + c.g() as u16 + c.b() as u16
        }
        assert!(luminance(AMBER_DIM) < luminance(AMBER));
        assert!(luminance(AMBER) < luminance(AMBER_BRIGHT));
    }

    // ── Restart / shutdown coordination ────────────────────────

    #[test]
    fn test_restart_sets_both_flags() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let restart = Arc::new(AtomicBool::new(false));
        // Simulate what the restart button does
        restart.store(true, Ordering::SeqCst);
        shutdown.store(true, Ordering::SeqCst);
        assert!(restart.load(Ordering::SeqCst));
        assert!(shutdown.load(Ordering::SeqCst));
    }

    #[test]
    fn test_restart_flag_reset_cycle() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let restart = Arc::new(AtomicBool::new(false));
        // Trigger restart
        restart.store(true, Ordering::SeqCst);
        shutdown.store(true, Ordering::SeqCst);
        // Simulate main loop reset after restart
        restart.store(false, Ordering::SeqCst);
        shutdown.store(false, Ordering::SeqCst);
        assert!(!restart.load(Ordering::SeqCst));
        assert!(!shutdown.load(Ordering::SeqCst));
    }

    // ── Logo sizing constants ────────────────────────────────

    #[test]
    fn test_logo_dimensions_match_source_png() {
        // The display size must match the source PNG exactly so the
        // GPU does a 1:1 blit on a 1.0x-DPI display, avoiding the
        // mauve-cast gradient issue we hit when minifying a larger
        // source.  ethernetgatewaylogo_small.png is 366x183.
        let logo_w = 366.0_f32;
        let logo_h = 183.0_f32;
        // Logo should fit within a reasonable GUI panel.
        assert!(logo_h > 50.0 && logo_h < 400.0);
        assert!(logo_w > 80.0 && logo_w < 600.0);
        // Landscape, 2:1 aspect ratio.
        assert_eq!(logo_w, logo_h * 2.0);
    }
}
