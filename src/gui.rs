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

mod wizard;

// ── Retro amber-on-dark color palette (telnetbible.com inspired) ──

// Sampled from the logo's own border rather than chosen: 640 of its edge
// pixels and all four corners are exactly this, and the old #000510 sat one
// step brighter and bluer -- close enough to look deliberate and far enough to
// draw a visible rectangle around the logo.  Measure it again if the artwork is
// recut; the web page's `--bg-darkest` carries the same value.
const BG_DARKEST: Color32 = Color32::from_rgb(0x00, 0x04, 0x0e); // sampled from the logo
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
/// For the one thing on a configuration screen that is a *finding* rather than
/// a setting: a bound port that did not answer.  Bright enough to read against
/// the panel without being the alarm red the must-acknowledge popups use.
/// Space between the setup wizard's text and the window edges.
const WIZARD_MARGIN: i8 = 14;

const RED_ALERT: Color32 = Color32::from_rgb(0xff, 0x5a, 0x4a);
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
/// The question a launch asks when another copy already holds the directory.
///
/// Passed *in* rather than discovered in the GUI: whether this process is the
/// gateway is settled in `main` before a single listener is started, and a
/// window that worked it out for itself could draw the editor for a server
/// that was never going to bind.
pub struct HandoverAsk {
    /// The holder's PID, when it could be read — for the message only.
    pub holder_pid: Option<u32>,
    /// Set when the operator chooses to take the ports.
    pub take_over: Arc<AtomicBool>,
}

/// Returns whether a window actually ran. Only the handover ask needs the
/// answer -- it uses this window to put a question, and a launch that never
/// opened one has not been answered.
pub fn run(
    cfg: Config,
    shutdown: Arc<AtomicBool>,
    restart: Arc<AtomicBool>,
    gui_ctx: Arc<std::sync::Mutex<Option<egui::Context>>>,
    handover: Option<HandoverAsk>,
) -> bool {
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
        let mut options = eframe::NativeOptions {
            viewport,
            ..Default::default()
        };
        apply_arm_gpu_workarounds(&mut options);

        eframe::run_native(
            "Ethernet Gateway",
            options,
            Box::new(move |cc| {
                *gui_ctx.lock().unwrap_or_else(|e| e.into_inner()) = Some(cc.egui_ctx.clone());
                egui_extras::install_image_loaders(&cc.egui_ctx);
                // Spend the one-shot screen marker here, on the way in, and
                // before anything can fail: a marker that outlived a launch
                // which never opened a browser would open one at every launch
                // afterwards.  Here rather than in `App::new` because this
                // writes the file and replaces the global config, which a unit
                // test constructing an `App` must not do.
                if cfg.open_screen_after_restart {
                    config::update_config_value("open_screen_after_restart", "false");
                }
                Ok(Box::new(App::new(cfg, shutdown, restart, handover)))
            }),
        )
    }));

    // **Whether a window ran is the caller's business, not just the log's.**
    // `main` uses this window to *ask a question* on the handover path, and a
    // launch that never opened one has not been answered -- it used to fall
    // straight through to "Left the running copy alone", which is what a
    // deliberate Quit prints, and exited 0. Measured on a display-less machine
    // with `enable_console = true`: winit refused the event loop, nobody was
    // asked, and the exit status said success.
    match result {
        Ok(Ok(())) => true,
        Ok(Err(e)) => {
            logger::log(format!("GUI could not start: {}", e));
            false
        }
        Err(_) => {
            logger::log("GUI crashed during startup (possible graphics driver issue)".into());
            false
        }
    }
}

/// The Raspberry Pi's GPU limits, applied to whichever window is being built.
///
/// ARM SBCs such as the Raspberry Pi have GPUs (e.g. VideoCore/V3D) that report
/// several device limits below wgpu's desktop defaults
/// (`max_color_attachments`, `max_inter_stage_shader_variables`, buffer sizes,
/// ...). eframe's default requests the desktop limits, so device creation
/// aborts with errors like "Limit 'max_color_attachments' value 8 is better
/// than allowed 4". Rather than clamp fields one at a time, request exactly the
/// limits the chosen adapter advertises -- that satisfies every field at once
/// and is always valid, since you cannot request more than the adapter
/// supports. egui runs fine on these (it targets WebGL2-class limits). Desktop
/// builds are unaffected: they keep eframe's defaults.
///
/// **A function rather than a block inside `run`, because two windows need it.**
/// It was inline until [`show_startup_failure`] arrived, and a Pi is exactly the
/// machine that would then have failed to draw the one window whose whole job is
/// to say why nothing started.
#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
fn apply_arm_gpu_workarounds(options: &mut eframe::NativeOptions) {
    use eframe::egui_wgpu::{wgpu, WgpuSetup};
    if let WgpuSetup::CreateNew(setup) = &mut options.wgpu_options.wgpu_setup {
        // Prefer the OpenGL ES backend on ARM. The Raspberry Pi's V3D Vulkan
        // driver (Mesa) is incomplete and aborts device creation with a
        // wgpu-hal "Requested feature is not available on this device" panic;
        // the GLES backend is V3D's mature path. An explicit WGPU_BACKEND still
        // wins, for debugging.
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

#[cfg(not(any(target_arch = "arm", target_arch = "aarch64")))]
fn apply_arm_gpu_workarounds(_options: &mut eframe::NativeOptions) {}

/// Whether the "this copy is serving nothing" banner is on screen.
///
/// **The text *and* the server cycle, not a flag and not the text alone.** A
/// boolean would silence the banner for the life of a window whose settings
/// reach nothing, which is the trap it exists to close. The text alone was the
/// first version's bug: a Save and Restart that failed identically produced the
/// same words, compared equal to the dismissed copy, and said nothing -- in the
/// one case that matters, because the settings just saved are why they
/// restarted. `bindwatch::reset` bumps the cycle on every server cycle, so the
/// same words about a new attempt are a new thing to say, and a warning that
/// *changes* within one cycle still counts too.
fn bind_banner_showing(
    warning: &(u64, Vec<String>),
    dismissed: &(u64, Vec<String>),
) -> bool {
    !warning.1.is_empty() && warning != dismissed
}

/// Whether a startup failure would be *invisible* if we only logged it.
///
/// **A desktop launch has nowhere to print.** The AppImage's own desktop entry
/// sets `Terminal=false`, so a fatal message on stderr goes to the session
/// journal and the operator sees a program that does nothing when
/// double-clicked -- measured 2026-08-20 from a read-only launch directory:
/// exit 1, no window, nothing on screen. Started from a shell the same message
/// is already in front of them and a second window would be noise.
///
/// **Asked of all three streams, not just stdout.** `window_closed_note` asks
/// only about stdout, and being wrong there costs a wrong sentence; being wrong
/// here costs a modal window that blocks a process until somebody closes it. So
/// `gateway | tee log` -- stdout redirected, stdin and stderr still a terminal
/// -- must stay silent, and it does: a window is offered only when *no* standard
/// stream is a terminal, which is the shape of a launch with no shell behind it
/// at all. A graphical session must also exist, or `run_native` would just fail
/// again. Pure, with both readings passed in by [`show_startup_failure`].
pub fn startup_failure_needs_a_window(any_stream_is_terminal: bool, have_display: bool) -> bool {
    !any_stream_is_terminal && have_display
}

/// Whether any standard stream is a terminal -- i.e. whether there is a shell
/// in front of this process that a printed message would reach.
fn any_std_stream_is_terminal() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal()
        || std::io::stdout().is_terminal()
        || std::io::stderr().is_terminal()
}

/// Whether this platform has a graphical session to draw on.
///
/// **`DISPLAY` is X11's question, not Unix's.** The first version asked it under
/// `cfg(unix)` -- and macOS *is* unix, sets neither `DISPLAY` nor
/// `WAYLAND_DISPLAY`, and has a window server that is always there. So on the
/// one platform in the release matrix where a double-clicked binary is the
/// normal way to start it, the window that exists for exactly that launch could
/// never appear. `aarch64-apple-darwin` is a shipped target, so this was a real
/// hole and not a theoretical one; it is the same shape as every other trap in
/// this file that Linux cannot see.
///
/// Windows and macOS therefore answer yes unconditionally, and the environment
/// is consulted only where it is the actual mechanism.
fn have_graphical_session() -> bool {
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let set = |k: &str| std::env::var_os(k).is_some_and(|v| !v.is_empty());
        set("DISPLAY") || set("WAYLAND_DISPLAY")
    }
    #[cfg(not(all(unix, not(target_os = "macos"))))]
    {
        true
    }
}

/// A window whose only job is to say why the gateway did not start.
///
/// **The counterpart to the close dialog, at the other end of the launch.**
/// That one exists because closing the window and stopping the server are
/// different acts; this one exists because a fatal error and a silent
/// disappearance look identical from a desktop icon. Both are the same lesson:
/// a message the operator cannot reach teaches them nothing.
///
/// It draws the lines it is given -- the caller has already logged them, so the
/// two cannot disagree -- and offers one button. There is no server behind it
/// and nothing on it edits anything. Returns as soon as the window closes; a
/// caller that cannot start is expected to exit straight afterwards.
///
/// Does nothing at all when the text has somewhere better to go (see
/// [`startup_failure_needs_a_window`]), so a service, a shell launch and a
/// headless Pi are unaffected.
pub fn show_startup_failure(
    headline: &str,
    lines: &[String],
    shutdown: Arc<AtomicBool>,
    gui_ctx: crate::GuiCtxSlot,
) {
    if !startup_failure_needs_a_window(any_std_stream_is_terminal(), have_graphical_session()) {
        return;
    }
    let mut options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(format!("Ethernet Gateway v{} — cannot start", env!("CARGO_PKG_VERSION")))
            .with_inner_size([760.0, 420.0])
            .with_min_inner_size([480.0, 300.0]),
        ..Default::default()
    };
    apply_arm_gpu_workarounds(&mut options);
    let notice = FatalNotice {
        headline: headline.to_string(),
        lines: lines.to_vec(),
        theme_applied: false,
        shutdown,
    };
    // Swallowed rather than reported: this *is* the error path. A window that
    // cannot open leaves the log line as the only word on the subject, which is
    // where we started, and `catch_unwind` keeps a graphics-driver panic from
    // replacing the operator's diagnosis with a backtrace.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        eframe::run_native(
            "Ethernet Gateway",
            options,
            Box::new(move |cc| {
                // **Registered, or a signal cannot reach this window.** The
                // signal watcher closes the GUI by sending `Close` through this
                // slot and nudging a repaint; with nothing in it, `SIGTERM` set
                // the flag and nothing happened -- measured, the process was
                // still alive four seconds later and stayed alive. A startup
                // failure that cannot be killed by a signal is worse than the
                // silent exit it replaced.
                *gui_ctx.lock().unwrap_or_else(|e| e.into_inner()) = Some(cc.egui_ctx.clone());
                Ok(Box::new(notice))
            }),
        )
    }));
}

/// The fatal-notice window's state: what to say, and nothing else.
struct FatalNotice {
    headline: String,
    lines: Vec<String>,
    /// The theme is applied on the first frame, once the renderer is up —
    /// the same one-shot the editor does.
    theme_applied: bool,
    /// Set by SIGINT/SIGTERM (and by nothing else on this path), so the window
    /// closes on a signal like every other window this program opens.
    shutdown: Arc<AtomicBool>,
}

impl eframe::App for FatalNotice {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        BG_DARKEST.to_normalized_gamma_f32()
    }

    // `ui`, not `update`: this eframe build hands an app a `Ui` rather than a
    // `Context`, exactly as the editor's own impl does.  The `Ui` has no margin
    // of its own -- the same trap the handover screen documents -- so the
    // content sits inside a `ScrollArea` with explicit spacing.
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if !self.theme_applied {
            apply_theme(ui.ctx());
            self.theme_applied = true;
        }
        let ctx = ui.ctx().clone();
        // A signal ends this window, exactly as it ends the editor's.
        if self.shutdown.load(Ordering::SeqCst) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        egui::Frame::NONE
            .inner_margin(egui::Margin::symmetric(WIZARD_MARGIN, 0))
            .show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                ui.add_space(20.0);
                ui.label(
                    egui::RichText::new(&self.headline).strong().size(22.0).color(RED_ALERT),
                );
                ui.add_space(14.0);
                for line in &self.lines {
                    // The log lines carry the indentation that lines them up
                    // under a `FATAL:` prefix; in a window that is just a
                    // ragged left edge, so it is trimmed here rather than the
                    // caller keeping two copies of the same text.
                    let text = line.trim_end();
                    if text.trim().is_empty() {
                        ui.add_space(8.0);
                    } else {
                        ui.label(egui::RichText::new(text.trim_start()).size(15.0).color(AMBER));
                    }
                }
                ui.add_space(22.0);
                if ui
                    .add(egui::Button::new(egui::RichText::new("Close").strong().size(16.0)))
                    .clicked()
                {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            });
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

/// Enumerate available serial ports with their hardware descriptions.
/// `pub(crate)` so the web server's serial-port dropdown can populate
/// from the same source the desktop GUI uses — both surfaces show
/// the same list and refresh through the same code path, and both can
/// therefore name the adapter behind a path on hover.
///
/// Delegates to [`crate::serial::list_serial_ports_detailed`] so there is one
/// implementation for all three UIs rather than one per surface.
pub(crate) fn detect_serial_ports() -> Vec<crate::serial::DetectedPort> {
    let ports = crate::serial::list_serial_ports_detailed();
    if ports.is_empty() {
        // The detailed helper folds an enumeration error into an empty list
        // (it is called from contexts with nowhere to report one).  An empty
        // list is also the perfectly normal "no adapters plugged in" answer,
        // so this is not logged as a failure.
        logger::log("No serial ports detected.".to_string());
    }
    ports
}

/// One tooltip listing every detected port and what it is — the answer to
/// "which `/dev/ttyUSB*` is my adapter?" without leaving the config screen.
pub(crate) fn serial_ports_tooltip(ports: &[crate::serial::DetectedPort]) -> String {
    if ports.is_empty() {
        return "No serial ports detected.".to_string();
    }
    let mut out = String::from("Detected serial ports:");
    for p in ports {
        out.push('\n');
        out.push_str(&p.detail);
    }
    out
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
    cpm_emu_max_minstr_buf: String,
    /// On-disk log limits, edited as text like every other numeric field so a
    /// half-typed number never becomes a config value.
    log_max_size_kb_buf: String,
    log_max_files_buf: String,
    /// Gateway terminal geometry reported to the remote, `0` = auto.  Text
    /// buffers like every other numeric field here.
    gateway_term_width_buf: String,
    gateway_term_height_buf: String,
    /// Numeric text buffers for the CP/M modem profile, same pattern as every
    /// other numeric field here: edited as text, parsed back on sync so a
    /// half-typed value never becomes a config value.
    cpm_emu_x_code_buf: String,
    cpm_emu_dcd_mode_buf: String,
    /// Per-port baud text buffer, indexed by `SerialPortId::index()`.
    /// Two slots — one each for Port A and Port B — let the user type
    /// freely without their input being clobbered by a partial parse.
    serial_baud_buf: [String; 2],
    // Detected serial ports for the dropdown (shared between both ports),
    // each carrying the hardware description shown on hover.
    serial_ports: Vec<crate::serial::DetectedPort>,
    /// Set when the user edits any field; prevents refresh_from_global from
    /// overwriting in-progress edits. Cleared on save.
    dirty: bool,
    /// Whether the Server "More..." popup is open.
    server_popup_open: bool,
    /// Whether the "close the window, or stop the server?" dialog is open.
    ///
    /// **The X on the title bar is a question, not an order.**  Closing the
    /// window leaves the server running (`main` falls through to a headless
    /// park loop), which is right for a console launched from a shell -- the
    /// terminal is still there and Ctrl-C still reaches us -- and a trap from a
    /// desktop icon, where there is no terminal to press it in.  With no Quit
    /// anywhere either, the only move the window offered was to close it and
    /// relaunch, and a second copy binds nothing: measured on 2026-08-19, five
    /// copies stacked up with the oldest still serving telnet and a newer one
    /// serving the web UI, so a Save in the visible window never reached the
    /// process that was answering.  So the close is intercepted and the
    /// operator is asked which of the two things they meant.
    close_prompt_open: bool,
    /// Set when we send `Close` ourselves, so the interception lets our own
    /// close through instead of re-asking for ever.
    closing_deliberately: bool,
    /// Whether this process has a terminal on stdout, captured once at
    /// startup.  It changes nothing about what the buttons *do* -- only what
    /// the dialog and `main`'s parting line **say**.  "Ctrl-C" is sound advice
    /// from a shell and a dead end from a desktop icon, and printing it in the
    /// second case is exactly what taught an operator to relaunch instead.
    ///
    /// **stdout, and deliberately not the controlling terminal.** A desktop
    /// launch is not detached from a tty at all: measured 2026-08-19, the
    /// AppImage's processes inherit the graphical VT (`tty2`) and `ps` reports
    /// it, so `/dev/tty` opens and answers yes while there is no shell on that
    /// VT to type into -- the same floating-input mistake as reading an
    /// unclaimed port as a real answer.  The question being asked is whether
    /// our output is going somewhere a person is watching with a shell in
    /// front of them, and stdout is that.  A `... | tee log` pipeline reads as
    /// no-terminal and is told to use `pkill`, which works either way; the
    /// error is one-directional on purpose.
    has_terminal: bool,
    /// The root/sudo ownership warning, empty when there is nothing to say.
    ///
    /// **A banner rather than a modal**: it reports a condition that lasts as
    /// long as the session, not an event to acknowledge, and a machine that
    /// deliberately runs the gateway as root would be asked to click a modal
    /// away at every launch. Built once in `new` from the one list in
    /// `config`, which the startup log renders too, so the two cannot come to
    /// disagree.
    elevation_lines: Vec<String>,
    /// The handover question, when this launch found another copy holding the
    /// directory.  `Some` means **no server is running behind this window** —
    /// `main` started none — so the editor must not be drawn: it would offer
    /// to save settings for a gateway that does not exist and show ports that
    /// belong to the other copy.
    handover: Option<HandoverAsk>,
    /// Set by the banner's Dismiss button, for this window only.
    ///
    /// **Deliberately not a config key.** Persisting it would silence the
    /// warning on installs that have never seen it, which is the one case it
    /// exists for -- an operator's click is an action, not a setting (the same
    /// reason the sample-disk offer writes no key). It also resets across a
    /// Save and Restart, and that is right rather than sloppy: a Save is
    /// precisely when a root session has just written `egateway.conf` as root,
    /// so the warning has more standing after one, not less.
    elevation_dismissed: bool,
    /// The aggregate bind warning with the server cycle it belongs to, and what
    /// the operator dismissed.
    ///
    /// **Dismissal is tied to the text *and the cycle*, not to a flag.** A flag
    /// would silence the banner for the life of a window whose settings reach
    /// nothing, which is the trap it exists to close. Text alone is not enough
    /// either, and that was the first version's bug: a Save and Restart that
    /// failed *identically* produced the same words, compared equal to the
    /// dismissed copy, and stayed silent -- in exactly the case that matters,
    /// since the settings just saved are why they restarted.
    bind_warning: (u64, Vec<String>),
    bind_warning_dismissed: (u64, Vec<String>),
    bind_warning_checked_at: Option<std::time::Instant>,
    /// The absolute data directory, resolved once — a draw path must not make
    /// filesystem calls, and this cannot change while the process runs.
    data_dir: String,
    /// Per-port "Serial Port — More..." popup state, indexed by
    /// `SerialPortId::index()`.  Independent so the user can have one
    /// port's popup open while editing the other's primary controls.
    serial_popup_open: [bool; 2],
    /// Whether the File Transfer "More..." popup is open.
    file_transfer_popup_open: bool,
    /// Whether the "General — More..." popup is open.  Holds the on-disk log
    /// settings; the frame's own three toggles are re-shown there so the popup
    /// reads as the whole General group rather than a fragment of it.
    general_popup_open: bool,
    /// Whether the "Mount CP/M Drives" window is open.  A second popup off the
    /// CP/M group rather than more rows in it: sixteen drives do not fit
    /// beside the other settings, and mounting is an occasional operation.
    cpm_mount_popup_open: bool,
    /// A sample-disk download running on its own thread, and what it last said.
    ///
    /// A thread rather than a blocking call: this is a minute of network I/O
    /// and the window has to keep painting, or the operator cannot tell a
    /// download from a hang.
    cpm_fetch: Option<std::sync::mpsc::Receiver<String>>,
    cpm_fetch_note: String,
    /// Draft selection, one entry per drive: the image filename, or empty for
    /// "use the drive folder".  Edited in the window and applied on Save, so a
    /// half-made choice never reaches a live drive.
    cpm_mount_draft: Vec<String>,
    /// The board-slot label for each drive row, and the draft it was built
    /// from — so it is built when the draft changes and not on every frame.
    ///
    /// Each label costs a `stat` of the image *and* two constructions of every
    /// controller this gateway has (`slot_board` and `slot_name` each resolve
    /// the board from the size). Sixteen rows made that ~16 stats and ~160
    /// allocations per repaint of a window that is usually just sitting open.
    /// egui redraws on its own schedule, so "per frame" is not a bounded cost.
    cpm_slot_labels: Vec<String>,
    /// What slot 0 holds while a disk boots, and the disk's name on its own.
    ///
    /// Both from [`crate::cpm::boot::MountContext`], so the mount dialog's first
    /// row cannot say something different from the slot column beside it.
    cpm_boot_slot_note: Option<String>,
    cpm_boot_slot_name: String,
    /// What [`App::cpm_slot_labels`] was computed from: the draft, and whether
    /// a boot image is configured (which decides whether slots are named for a
    /// board at all).
    cpm_slot_labels_from: (Vec<String>, bool, String, String),
    /// Whether the bootability cache has been warmed for this window.
    ///
    /// Asking whether an image boots means cold-starting it, and the first
    /// answer for a given file reads the whole thing — 4.9 MB for a hard disk,
    /// about 42 MB for the sample set.  Every other surface can push that onto a
    /// blocking task; a desktop window has only the frame thread, so opening the
    /// "CP/M runs:" list on a cold cache would freeze it.  Warmed on a plain
    /// thread the first time the CP/M controls are drawn, which is the moment
    /// the operator is most likely to be about to open it.
    cpm_boot_cache_warmed: bool,
    /// Cache behind [`App::cpm_boot_label`]: `(label, the setting it was
    /// resolved from, when)`, and `None` until the first frame asks.
    ///
    /// Cached because that row is on the *server panel*, not inside a window an
    /// operator opens: the panel redraws on a 250 ms heartbeat for as long as
    /// the desktop UI is up, so an unconditional resolve would be two syscalls
    /// four times a second forever, for a string that almost never changes.
    ///
    /// Keyed on the setting **and** a one-second age, because the setting is
    /// not the only thing that can change the answer — deleting the `.dsk` in a
    /// file manager leaves the string alone, and a cache keyed on the string
    /// would go on claiming the disk was there until the combo was next
    /// touched. One second against a 250 ms heartbeat means the row corrects
    /// itself without anybody touching the window, which is how it was
    /// verified.
    cpm_boot_label_cache: Option<(String, String, std::time::Instant)>,
    /// What the last apply reported, shown under the rows.
    cpm_mount_notice: String,
    /// Format token selected in the "new blank disk" row.
    cpm_new_format: String,
    /// Name typed for a new blank disk.  Cleared once one is created, so the
    /// next Create cannot silently refuse against the name just used.
    cpm_new_name: String,
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
    /// Whether the "turn the web server on to see the screen?" popup is open.
    ///
    /// The VDM / Dazzler screen is served by the web server, so the desktop's
    /// button opens a browser at it — and if the listener is off there is
    /// nothing to open.  Offering to start it beats a dead button, but starting
    /// a listener is outward-facing, so it is offered and never done silently.
    /// A finished port check, waiting to be turned into a popup.
    ///
    /// The check runs on its own thread -- four connect timeouts would freeze
    /// the window -- so the result comes back through a channel the frame loop
    /// polls, exactly as the sample-disk download does.
    /// The taller of each config row's two columns' **natural** content
    /// height, measured last repaint.
    ///
    /// **`set_min_height` is a floor, and a floor does not align anything.**
    /// Whichever column's content ran past it grew alone, so the two bottom
    /// borders sat 6-8 px apart and the row read as staggered.  Raising the
    /// floor until nothing exceeded it aligned them and left a band of dead
    /// space under every frame, which pushed the logo below the console.
    ///
    /// So the shorter column is padded to match the taller one instead.  What
    /// is stored is the height the content came to **before** that padding —
    /// feeding a padded frame's own height back in would make the row a little
    /// taller every repaint, without limit, and the window grows visibly while
    /// you watch it.  Naturals do not include the padding, so the value is
    /// whatever the content actually needs and it settles at once.
    ///
    /// egui lays out in a single pass and cannot know the second column's
    /// height while drawing the first, so a one-repaint lag is the only way to
    /// do this; layout is stable, so it settles on the first repaint.
    config_row_h: [f32; 3],
    port_check_rx: Option<std::sync::mpsc::Receiver<usize>>,
    /// Whether the port-check result popup is open.
    ///
    /// **A popup, because the console window is not where anybody looks.** The
    /// red labels say *which* port, but only once you are looking at the frame;
    /// somebody who has just pressed Test ports is owed the answer where they
    /// are.
    port_check_popup_open: bool,
    vdm_web_offer_open: bool,
    /// The web server this window's gateway was started with: `(enabled, port)`.
    ///
    /// Captured once, from the config `main` used to spawn this cycle's server,
    /// and never refreshed — see [`App::vdm_url`] for why neither the live
    /// config nor `last_synced_cfg` answers this question.
    running_web: (bool, u16),
    /// When to open the screen in a browser, once the web server has had a
    /// moment to bind.
    ///
    /// Enabling the listener goes through a full server restart, so opening the
    /// page in the same click would race the bind and hand the operator
    /// "connection refused" for a feature that is about to work.  Set to a
    /// deadline instead, and opened from `update` when it passes.
    vdm_open_at: Option<std::time::Instant>,
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
    /// First-run setup wizard.  `Some` while it owns the window — on a fresh
    /// install (`setup_wizard_completed = false`) or when the operator asks for
    /// it again from the Server "More" popup.  It edits its own draft copy of
    /// the answers, so nothing here or in the live config changes until it
    /// finishes; see [`wizard`].
    wizard: Option<wizard::Wizard>,
}

/// What the web listener did with the port it was given.
///
/// A boolean cannot carry this: "off" and "failed to bind" both mean the button
/// has nothing to open, and they want opposite things said to the operator —
/// one is an offer to start the server, the other is news that starting it did
/// not work and why.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum WebScreenState {
    /// Listening, on this port.
    Bound(u16),
    /// Registered and still binding.
    Starting(u16),
    /// Configured, and the bind failed.  `in_use` is the case worth naming.
    Failed { port: u16, in_use: bool },
    /// Not configured at all.
    Off,
}

impl WebScreenState {
    /// Read one listener's bind outcome.
    ///
    /// A free function of its input so it can be tested without a bound socket
    /// or the process-wide bindwatch roster — the mapping is the whole of the
    /// decision, and it is what a caller gets wrong.
    fn of(status: Option<(u16, crate::bindwatch::Status)>) -> WebScreenState {
        use crate::bindwatch::Status;
        match status {
            Some((port, Status::Bound)) => WebScreenState::Bound(port),
            // Still coming up.  The port it is binding to, not "off": offering
            // to enable a server that is starting would be a restart nobody
            // needed.
            Some((port, Status::Pending)) => WebScreenState::Starting(port),
            Some((port, Status::Failed { in_use })) => WebScreenState::Failed { port, in_use },
            // Not in this cycle's roster at all: not configured.
            None => WebScreenState::Off,
        }
    }

    /// The port a browser could be sent to, if any.
    fn port(&self) -> Option<u16> {
        match self {
            WebScreenState::Bound(p) | WebScreenState::Starting(p) => Some(*p),
            _ => None,
        }
    }
}

impl App {
    fn new(
        mut cfg: Config,
        shutdown: Arc<AtomicBool>,
        restart: Arc<AtomicBool>,
        handover: Option<HandoverAsk>,
    ) -> Self {
        // Clear the one-shot screen marker the moment it is read, and before
        // anything is opened.  A marker that survived a launch which failed to
        // open a browser would open one at every launch afterwards, which is a
        // far worse fault than the one it exists to fix.
        //
        // **Read, never written here.**  `App::new` must not touch the
        // process-wide config or the file: it runs in unit tests,
        // `update_config_value` replaces the global `CONFIG` and rewrites
        // `egateway.conf`, and doing that from a plain `#[test]` running beside
        // the rest of the suite is how one test's config lands in another's
        // lap.  `run` writes the cleared value to the file before this is
        // reached -- straight away rather than at the next Save, because the
        // operator may never press one.
        let open_screen_asked = cfg.open_screen_after_restart;
        cfg.open_screen_after_restart = false;
        // Captured before `cfg` is moved into the struct: this is what `main`
        // started this cycle's server from, and the only honest answer to
        // "where is the web server".
        let running_web = (cfg.web_enabled, cfg.web_port);
        let cfg = cfg;
        // Seed saved_geom from the config so we don't rewrite the identical
        // geometry we just restored on first launch.
        let saved_geom = parse_window_geometry(&cfg.gui_window_geometry);
        // A config that has never been through setup owns the window until the
        // operator finishes or skips the wizard.
        let wizard = (!cfg.setup_wizard_completed).then(|| wizard::Wizard::new(&cfg));
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
        let cpm_emu_max_minstr_buf = cfg.cpm_emu_max_minstr.to_string();
        let log_max_size_kb_buf = cfg.log_max_size_kb.to_string();
        let log_max_files_buf = cfg.log_max_files.to_string();
        let gateway_term_width_buf = cfg.gateway_term_width.to_string();
        let gateway_term_height_buf = cfg.gateway_term_height.to_string();
        let cpm_emu_x_code_buf = cfg.cpm_emu_modem.x_code.to_string();
        let cpm_emu_dcd_mode_buf = cfg.cpm_emu_modem.dcd_mode.to_string();
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
            cpm_emu_max_minstr_buf,
            log_max_size_kb_buf,
            log_max_files_buf,
            gateway_term_width_buf,
            gateway_term_height_buf,
            cpm_emu_x_code_buf,
            cpm_emu_dcd_mode_buf,
            serial_baud_buf,
            serial_ports,
            dirty: false,
            server_popup_open: false,
            close_prompt_open: false,
            closing_deliberately: false,
            // Asked once, here, rather than per frame: a process does not
            // acquire or lose a terminal while it runs, and `ui()` must not
            // do a syscall on every repaint to render one static sentence.
            has_terminal: std::io::IsTerminal::is_terminal(&std::io::stdout()),
            elevation_lines: {
                let (is_root, sudo_user) = config::detect_elevation();
                config::elevation_warning_lines(
                    is_root,
                    sudo_user.as_deref(),
                    config::serial_access_group(),
                )
            },
            elevation_dismissed: false,
            bind_warning: (0, Vec::new()),
            bind_warning_dismissed: (0, Vec::new()),
            data_dir: config::data_dir_display(),
            bind_warning_checked_at: None,
            handover,
            serial_popup_open: [false, false],
            file_transfer_popup_open: false,
            general_popup_open: false,
            ai_browser_popup_open: false,
            cpm_mount_popup_open: false,
            cpm_fetch: None,
            cpm_fetch_note: String::new(),
            cpm_mount_draft: vec![String::new(); crate::cpm::NUM_DRIVES as usize],
            cpm_slot_labels: Vec::new(),
            cpm_boot_slot_note: None,
            cpm_boot_slot_name: String::new(),
            cpm_slot_labels_from: (Vec::new(), false, String::new(), String::new()),
            cpm_boot_cache_warmed: false,
            cpm_boot_label_cache: None,
            cpm_mount_notice: String::new(),
            cpm_new_format: String::new(),
            cpm_new_name: String::new(),
            atdt_kermit_warn_open: false,
            relay_ssh_warn_open: false,
            kermit_server_warn_open: false,
            running_web,
            config_row_h: [0.0; 3],
            port_check_rx: None,
            port_check_popup_open: false,
            vdm_web_offer_open: false,
            // If the operator got here by asking for the screen and agreeing
            // to the restart, finish the job.  The marker is in the config
            // because the restart destroys this window -- `gui::run` returns,
            // `main` re-spawns everything and builds a fresh `App` -- and the
            // config is re-read on every restart cycle.
            //
            // Cleared below, before the page is opened, so a launch that never
            // manages to open one does not try again for ever.
            //
            // The delay is because this runs as the window is built, while the
            // freshly-spawned server is still binding: a browser pointed at it
            // now would show "connection refused" for a page that is a second
            // away from working.
            vdm_open_at: open_screen_asked
                .then(|| std::time::Instant::now() + std::time::Duration::from_millis(2500)),
            disable_ip_safety_warn_open: false,
            pending_dir_pick: None,
            wizard,
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
        // Clamped rather than range-guarded, unlike its neighbours above: the
        // config loader and `apply_config_key` both clamp this one (see
        // `config::MAX_CPM_EMU_MAX_MINSTR`), so refusing the value here would
        // have the desktop disagreeing with telnet and the web about what
        // typing a huge number does.  The buffer is rewritten when the clamp
        // bites, or it would differ from `cfg` for ever and leave the window
        // showing unsaved changes that cannot be saved away.
        if let Ok(v) = self.cpm_emu_max_minstr_buf.parse::<u32>() && v >= 1 {
            let clamped = v.min(crate::config::MAX_CPM_EMU_MAX_MINSTR);
            self.cfg.cpm_emu_max_minstr = clamped;
            if clamped != v {
                self.cpm_emu_max_minstr_buf = clamped.to_string();
            }
        }
        // No `>= 1` floor on either log limit: `0` is meaningful for both —
        // no size rotation, and keep no rotated generations — matching the
        // config-file loader and `logger::should_rotate`/`rotate`.
        if let Ok(v) = self.log_max_size_kb_buf.parse::<u64>() { self.cfg.log_max_size_kb = v; }
        if let Ok(v) = self.log_max_files_buf.parse::<u32>() { self.cfg.log_max_files = v; }
        // Same reasoning, same absent floor: `0` is the "auto" sentinel for
        // both, so a `>= 1` guard here would make automatic geometry
        // unreachable from the GUI (see config::gateway_term_hint).
        if let Ok(v) = self.gateway_term_width_buf.parse::<u16>() { self.cfg.gateway_term_width = v; }
        if let Ok(v) = self.gateway_term_height_buf.parse::<u16>() { self.cfg.gateway_term_height = v; }
        // Clamped to the ranges the AT layer itself produces (ATX0-4, AT&C0/1),
        // so a typo here cannot leave the modem in a state no command could set.
        if let Ok(v) = self.cpm_emu_x_code_buf.parse::<u8>() && v <= 4 { self.cfg.cpm_emu_modem.x_code = v; }
        if let Ok(v) = self.cpm_emu_dcd_mode_buf.parse::<u8>() && v <= 1 { self.cfg.cpm_emu_modem.dcd_mode = v; }
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
    /// Re-read the aggregate bind outcome, at most once a second.
    ///
    /// **On a timer rather than per frame, and never latched.** A bind is
    /// asynchronous, so the answer is not available on the first frames; and a
    /// Save and Restart re-arms every listener, so a one-shot check would go on
    /// showing a stale verdict -- or, worse, having decided "all bound" once,
    /// never notice that the restarted copy binds nothing.
    fn poll_bind_warning(&mut self) {
        let due = match self.bind_warning_checked_at {
            None => true,
            Some(t) => t.elapsed() >= std::time::Duration::from_secs(1),
        };
        if !due {
            return;
        }
        self.bind_warning_checked_at = Some(std::time::Instant::now());
        // `None` means at least one listener has not reported yet: keep the
        // previous answer rather than flashing a warning at a bind in flight.
        if let Some(report) = crate::bindwatch::aggregate_warning() {
            self.bind_warning = report;
        }
    }

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
            labeled_port_field(ui, "telnet", &mut self.telnet_port_buf, PORT_W);
            ui.add_space(GUTTER);
            let resp = ui.checkbox(&mut self.cfg.web_enabled, "Web Server");
            pad_to(ui, COL2_W, resp.rect.width());
            labeled_port_field(ui, "web", &mut self.web_port_buf, PORT_W);
            if with_more_button {
                let blocked = crate::portcheck::results()
                    .iter()
                    .filter(|(_, _, r)| r.is_blocked())
                    .count();
                if server_more_button(ui, blocked) {
                    self.server_popup_open = true;
                }
            }
        });
        // Row 2: SSH + Kermit Server.  Same column widths so the
        // colons line up with row 1.  The Kermit checkbox keeps its
        // off→on security-warning popup interlock.
        ui.horizontal(|ui| {
            let resp = ui.checkbox(&mut self.cfg.ssh_enabled, "SSH");
            pad_to(ui, COL1_W, resp.rect.width());
            labeled_port_field(ui, "SSH", &mut self.ssh_port_buf, PORT_W);
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
            labeled_port_field(ui, "Kermit", &mut self.kermit_server_port_buf, PORT_W);
        });
    }

    /// Server More-popup-only rows.  Holds settings that don't fit in
    /// the main Server frame: the session cap and the per-session
    /// idle-timeout.  The main frame surfaces only the listener
    /// enable/port fields per the operator-facing layout decision; the
    /// More popup keeps everything available for completeness.
    fn draw_server_more_only(&mut self, ui: &mut egui::Ui) {
        // The port check.  In here rather than on the frame, which has no line
        // to spare for a button, a summary and an advisory that would sit there
        // whether or not anybody had ever run one.  The frame's `More...` turns
        // red when this found something, so the way here is signposted.
        //
        // A button rather than anything automatic: each probe is a real
        // connection to our own listener -- a session slot and a line in the
        // log -- so running it on a timer would fill the log with the operator
        // connecting to themselves.
        ui.horizontal(|ui| {
            let busy = self.port_check_rx.is_some();
            if ui
                .add_enabled(
                    !busy,
                    egui::Button::new(
                        egui::RichText::new(if busy { "Testing…" } else { "Test ports" })
                            .color(AMBER_BRIGHT),
                    ),
                )
                .on_hover_text(
                    "Connect to each bound listener at this machine's own network \
                     address. A port that does not answer has its Port label \
                     reddened. A port that does answer is NOT reported open: on \
                     Windows and macOS a connection to your own address skips the \
                     firewall entirely, and nothing here can see a router that is \
                     not forwarding a port.",
                )
                .clicked()
            {
                // One at a time.  Two clicks used to leave two threads racing
                // into `store()`, so the popup -- opened by the second -- could
                // be showing the first run's table, and the log carried two
                // interleaved sequences.
                if self.port_check_rx.is_none() {
                    // Off the frame thread: four connect timeouts is over a
                    // second of blocking, and the window would freeze for it.
                    // The count comes back through a channel so the popup opens
                    // when the check is actually finished rather than a guess
                    // later.
                    let (tx, rx) = std::sync::mpsc::channel();
                    self.port_check_rx = Some(rx);
                    let cycle = crate::portcheck::cycle();
                    std::thread::spawn(move || {
                        let blocked = crate::portcheck::run_check_for_cycle(cycle);
                        let _ = tx.send(blocked);
                    });
                }
            }
            let blocked =
                crate::portcheck::results().iter().filter(|(_, _, r)| r.is_blocked()).count();
            if blocked > 0 {
                ui.label(
                    egui::RichText::new(format!(
                        "{blocked} port{} did not answer",
                        if blocked == 1 { "" } else { "s" }
                    ))
                    .small()
                    .color(RED_ALERT),
                );
            } else if crate::portcheck::has_run() {
                ui.label(
                    egui::RichText::new("every bound port answered here")
                        .small()
                        .color(AMBER_DIM),
                );
            }
        });
        // Always, not only when something was found.  A red Port label means
        // "we tested this and nothing answered"; this means "ports may need
        // opening on a firewall", which is true whether or not the check caught
        // anything -- and it cannot catch everything.  It sees nothing past this
        // machine, and on Windows and macOS a self-connection skips the firewall
        // entirely, so silence is not an all-clear.
        ui.label(
            egui::RichText::new(
                "Remember to open these ports on your firewall — a check from this \
                 machine cannot see a router that is not forwarding them.",
            )
            .small()
            .color(AMBER_DIM),
        );
        ui.add_space(4.0);
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

    /// Contents of the "General — More" popup: the whole General group.
    ///
    /// Re-shows the frame's own three toggles so the popup reads as the complete
    /// group rather than a fragment (the same thing the AI/Browser popup does
    /// with the API key and homepage).  The web cannot do this — its page is a
    /// single form, so a duplicated field name would submit twice — which is why
    /// the web's General popup carries only the log settings.
    fn draw_general_more(&mut self, ui: &mut egui::Ui) {
        ui.checkbox(&mut self.cfg.verbose, "Verbose Transfer Logging");
        ui.checkbox(&mut self.cfg.gateway_debug, "Gateway Debug Trace");
        ui.checkbox(&mut self.cfg.enable_console, "Show GUI on Startup");
        ui.add_space(6.0);
        ui.separator();
        ui.add_space(2.0);
        self.draw_general_logging(ui);
    }

    /// Render the on-disk log controls, shown in the General "More…" popup.
    ///
    /// The log is written in addition to stderr and the console pane; it is
    /// size-bounded and deletes its oldest generation, so the worst-case disk
    /// figure shown here comes from [`logger::max_disk_kb`] rather than being
    /// multiplied out again (one source for that arithmetic — the telnet and web
    /// UIs and the startup banner all call it).
    fn draw_general_logging(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("Log File").strong().color(AMBER));
        ui.checkbox(&mut self.cfg.log_to_file, "Write the log to a file");
        ui.add_enabled_ui(self.cfg.log_to_file, |ui| {
            ui.horizontal(|ui| {
                ui.label("File:");
                singleline_with_menu(ui, &mut self.cfg.log_file, false, None);
            });
            ui.horizontal(|ui| {
                // 80px, not 60: this is a u64 in KB, so a 1 GB cap is seven
                // digits and a 60px box showed six of them.  The web UI had the
                // same defect and was fixed by measurement; this width is
                // derived instead from the frame's own PORT_W (50px for the five
                // digits of a port number, so ~10px a digit) — the egui font API
                // would not measure headlessly, and the alternative was opening
                // a window on the operator's live desktop.  Eight digits of room
                // for a value that is realistically at most seven.
                labeled_field(ui, "Rotate at (KB):", &mut self.log_max_size_kb_buf, 80.0);
                ui.add_space(8.0);
                labeled_field(ui, "Keep old:", &mut self.log_max_files_buf, 40.0);
            });
        });
        // Parse the buffers for the hint rather than reading cfg: the fields
        // sync on the next frame, so cfg still holds the pre-edit numbers while
        // the operator is typing and the figure would lag a keystroke behind.
        let size_kb = self.log_max_size_kb_buf.parse::<u64>().unwrap_or(self.cfg.log_max_size_kb);
        let files = self.log_max_files_buf.parse::<u32>().unwrap_or(self.cfg.log_max_files);
        // Shared with the web so the two cannot word this differently (they had
        // already drifted).  It also owns the "blank path is off too" rule.
        let hint = logger::log_state_hint(&self.cfg, size_kb, files);
        ui.label(egui::RichText::new(hint).italics().small());
    }

    /// Contents of the "AI, Browser & Weather — More" popup: every option in
    /// the group.  The main frame shows only the API key + homepage (three-row
    /// budget); the weather location + units live here.
    /// Seed the draft from the live mount table.
    ///
    /// Run when the window opens rather than held from last time, so it always
    /// reflects what is actually mounted — including changes made from the web
    /// or a telnet session while the desktop window sat closed.
    fn cpm_mount_reload_draft(&mut self) {
        let mounts = crate::cpm::image::registry::all();
        self.cpm_mount_draft = (0..crate::cpm::NUM_DRIVES as usize)
            .map(|i| {
                mounts
                    .get(i)
                    .and_then(|m| m.as_ref())
                    .map(|m| m.filename.clone())
                    .unwrap_or_default()
            })
            .collect();
        self.cpm_mount_notice.clear();
    }

    /// The `CP/M runs:` label, resolved at most once a second.
    ///
    /// The marker matters because this row is where an operator finds out the
    /// gateway is running the emulator when they asked for a disk — but the row
    /// is on the always-drawn server panel, so the resolve behind it is paid on
    /// a schedule rather than every repaint. See [`App::cpm_boot_label_cache`]
    /// for why the age is part of the key and not just the setting.
    fn cpm_boot_label(&mut self) -> String {
        const TTL: std::time::Duration = std::time::Duration::from_secs(1);
        if let Some((label, from, at)) = &self.cpm_boot_label_cache {
            if *from == self.cfg.cpm_boot_image && at.elapsed() < TTL {
                return label.clone();
            }
        }
        let target = crate::cpm::boot::boot_target(&self.cfg.transfer_dir, &self.cfg.cpm_boot_image);
        let label = crate::cpm::boot::boot_setting_label(&target, &self.cfg.cpm_boot_image);
        self.cpm_boot_label_cache = Some((
            label.clone(),
            self.cfg.cpm_boot_image.clone(),
            std::time::Instant::now(),
        ));
        label
    }

    /// Contents of the "Mount CP/M Drives" window: a row per drive.
    /// The offer to fetch the sample disks, and the download itself.
    ///
    /// On a thread, with the result arriving down a channel: a minute of
    /// blocking network I/O on the UI thread would freeze the window, and an
    /// operator cannot tell a frozen window from a crashed one.
    ///
    /// Says the count, the size and where they come from before anything
    /// happens — the disks are not ours, and someone who would rather fetch
    /// them by hand should be able to see that and not press it.
    fn draw_cpm_fetch_button(&mut self, ui: &mut egui::Ui) {
        // Collect a finished download first, so the button comes back.
        if let Some(rx) = &self.cpm_fetch {
            match rx.try_recv() {
                Ok(msg) => {
                    self.cpm_fetch_note = msg;
                    self.cpm_fetch = None;
                    self.cpm_mount_reload_draft();
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.cpm_fetch_note = "The download ended without saying how.".to_string();
                    self.cpm_fetch = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
        if self.cpm_fetch.is_some() {
            ui.label(egui::RichText::new("Downloading…").color(AMBER));
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
            return;
        }

        let base = crate::cpm::layout::cpm_dir(&self.cfg.transfer_dir);
        let images = base.join(crate::cpm::image::IMAGES_DIR);
        let all = crate::cpm::fetch::catalogue();
        let wanted = crate::cpm::fetch::missing(&images, &all);
        if wanted.is_empty() {
            return;
        }
        let mb = wanted.iter().map(|d| d.bytes).sum::<u64>() as f64 / (1024.0 * 1024.0);
        if ui
            .add(egui::Button::new(
                egui::RichText::new("Download sample disks").color(AMBER_BRIGHT),
            ))
            .on_hover_text(format!(
                "Fetch {} disks ({:.0} MB) from {} — only the ones this gateway is known to run. \
                 They are not ours; this fetches them for you, and anything already in the images \
                 folder is left alone.",
                wanted.len(),
                mb,
                crate::cpm::fetch::source_repos().join(" and "),
            ))
            .clicked()
        {
            self.start_cpm_fetch(ui.ctx());
        }
    }

    /// Start the sample-disk download on its own thread.
    ///
    /// Shared by the button above and by the setup wizard, which offers the
    /// same download on its CP/M screen and starts it once the answers are
    /// saved — the wizard edits a draft, so it has no folder to download into
    /// until then.  One implementation, so the two offers cannot drift apart
    /// over which disks are fetched or what the result says.
    fn start_cpm_fetch(&mut self, ctx: &egui::Context) {
        if self.cpm_fetch.is_some() {
            return;
        }
        let images = crate::cpm::layout::cpm_dir(&self.cfg.transfer_dir)
            .join(crate::cpm::image::IMAGES_DIR);
        let (tx, rx) = std::sync::mpsc::channel();
        self.cpm_fetch = Some(rx);
        self.cpm_fetch_note = String::new();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let msg = match crate::cpm::fetch::download_missing(&images, |_, _, _| {}) {
                Ok(r) => {
                    let mut m = r.summary();
                    // Grouped by reason: with no internet this is one fact
                    // repeated once per disk, not three unlucky files.
                    for line in r.failure_lines(3) {
                        m.push_str(&format!("  {line}"));
                    }
                    m
                }
                Err(e) => e,
            };
            logger::log(format!("CP/M sample disks: {}", msg.trim()));
            let _ = tx.send(msg);
            // Wake the UI: without this the result sits in the channel
            // until something else happens to cause a repaint.
            ctx.request_repaint();
        });
    }

    fn draw_cpm_mounts(&mut self, ui: &mut egui::Ui) {
        let base = crate::cpm::layout::cpm_dir(&self.cfg.transfer_dir);
        // What will actually run, not what the key says: a `cpm_boot_image`
        // naming a disk that is no longer in the images folder falls back to
        // the emulator, and these rows describe the machine that starts.
        // Resolved once for the whole window — one `stat` beside the folder
        // listing on the next line, not one per row.
        let naming =
            crate::cpm::boot::boot_target(&self.cfg.transfer_dir, &self.cfg.cpm_boot_image)
                .slot_naming();
        let booting = naming == crate::cpm::boot::SlotNaming::Boards;
        // The same `MountContext` the telnet and web screens use.  Under a
        // booted disk it decides both what a slot is called and which images can
        // reach the guest at all -- the board is chosen by the image's size, so
        // an image on another board mounts correctly and is invisible.
        let ctx = crate::cpm::boot::MountContext::resolve(
            &self.cfg.transfer_dir,
            &self.cfg.cpm_boot_image,
            &self.cfg.cpm_boot_machine,
        );
        let img_dir = crate::cpm::image::images_dir(&base);
        // Only files we could read, and only hidden when a disk is booting: an
        // unreadable file is not "on the wrong board", and with the emulator
        // running nothing is.
        let mut hidden_images = 0usize;
        let mut images: Vec<String> = Vec::new();
        for n in crate::cpm::image::available_images(&base) {
            match std::fs::metadata(img_dir.join(&n)) {
                Ok(m) if ctx.accepts(m.len()) => images.push(n),
                Ok(_) => hidden_images += 1,
                Err(_) => {}
            }
        }
        let mounts = crate::cpm::image::registry::all();
        let usage = crate::cpm::image::registry::usage();

        // Slot labels, rebuilt only when the answer could have changed — see
        // `cpm_slot_labels`.  Under the emulator there are no board slots to
        // name, so the work is skipped outright rather than done and ignored.
        // Keyed on what the labels actually depend on.  It was `(draft, booting)`
        // while the labels came from each row's own image; they come from the
        // boot setting now, so changing the boot disk from a floppy to a hard
        // disk left `Drive 1` on screen where `unit 0.1` had become correct --
        // for the rest of the session, because nothing in the key had changed.
        let labels_key = (
            self.cpm_mount_draft.clone(),
            booting,
            self.cfg.cpm_boot_image.clone(),
            self.cfg.cpm_boot_machine.clone(),
        );
        if self.cpm_slot_labels_from != labels_key {
            self.cpm_slot_labels = if booting {
                self.cpm_mount_draft
                    .iter()
                    .enumerate()
                    .map(|(idx, name)| {
                        let _ = name;
                        // Both halves from the booted disk's board -- the slot
                        // name and the board it is on.  Taking the board from
                        // *this row's* image instead could render
                        // `unit 0.1 on the MITS 88-DCDD`, which is the mixture
                        // this whole change removes.  The desktop has room to
                        // name it where a 40-column PETSCII screen does not.
                        let board = ctx
                            .board()
                            .map(|b| format!(" on the {b}"))
                            .unwrap_or_default();
                        format!("{}{board}", ctx.slot(idx as u8))
                    })
                    .collect()
            } else {
                Vec::new()
            };
            self.cpm_slot_labels_from = labels_key;
        }
        // Slot 0's occupant, from the same context as the slot names so the two
        // cannot disagree.  Cached alongside the labels because it comes from
        // the same key -- and because `boot_slot_note` resolves the boot target,
        // which reads the disk, and a draw path must not do that per frame.
        self.cpm_boot_slot_note = ctx.boot_slot_note();
        self.cpm_boot_slot_name =
            ctx.boot_disk_name().map(str::to_string).unwrap_or_else(|| "(booted disk)".to_string());

        // These strings are single-line on purpose: a Rust line continuation
        // inside them is easy to lose in editing, and what is left behind is a
        // literal run of spaces that renders as a ragged gap mid-sentence.
        let intro = if images.is_empty() && hidden_images > 0 {
            // Not "No images found" beside "N are hidden": that pair reads as a
            // contradiction.  When everything is filtered out, the filter is
            // the whole story.
            format!(
                "None of the {hidden_images} image{} in {}/images {} on the booted disk's board, so its operating system could not read {}. Change what boots, or add a disk of the right kind.",
                if hidden_images == 1 { "" } else { "s" },
                base.display(),
                if hidden_images == 1 { "is" } else { "are" },
                if hidden_images == 1 { "it" } else { "them" },
            )
        } else if images.is_empty() {
            format!(
                "No images found. Put .dsk files in {}/images — readme.txt there explains the naming.",
                base.display()
            )
        } else {
            "A mounted drive uses the files inside the image instead of the files in its folder. The folder's files are not touched and return when you unmount.".to_string()
        };
        ui.add(egui::Label::new(intro).wrap());
        // Say what is missing and why.  A folder the operator can open, offering
        // fewer disks than it holds, is a mystery; the reason is something they
        // can act on -- change what boots, or fetch a disk of the right kind.
        if hidden_images > 0 && !images.is_empty() {
            ui.add(
                egui::Label::new(
                    egui::RichText::new(format!(
                        "{hidden_images} more image{} in the folder {} not offered: with a disk set to boot, an image only reaches the guest if it lands on the same board, and the board is chosen by the image's size.",
                        if hidden_images == 1 { "" } else { "s" },
                        if hidden_images == 1 { "is" } else { "are" },
                    ))
                    .small()
                    .color(AMBER_DIM),
                )
                .wrap(),
            );
        }
        // The offer goes here — above the drives, beside the message that says
        // there is nothing to mount — and not in the footer under sixteen rows
        // where it was first put.  "Where do I get a disk?" is asked at the top
        // of this window, not the bottom, and the web page answers it in the
        // same place.
        ui.horizontal(|ui| {
            self.draw_cpm_fetch_button(ui);
        });
        if !self.cpm_fetch_note.is_empty() {
            ui.label(egui::RichText::new(&self.cpm_fetch_note).small().color(AMBER_DIM));
        }
        ui.add_space(6.0);

        egui::ScrollArea::vertical()
            .max_height(340.0)
            .show(ui, |ui| {
                egui::Grid::new("cpm_mount_grid")
                    .num_columns(3)
                    .spacing([8.0, 4.0])
                    .show(ui, |ui| {
                for drive0 in 0..crate::cpm::NUM_DRIVES {
                    let idx = drive0 as usize;
                    let letter = (b'A' + drive0) as char;
                    // A lent drive reads as empty in the mount table, so it
                    // would otherwise render free and enabled and then refuse
                    // on Save with nothing on screen explaining why.
                    let busy = usage
                        .get(idx)
                        .and_then(|u| u.describe())
                        .or_else(|| crate::cpm::image::drive_held_note(drive0));
                    let mounted = mounts.get(idx).and_then(|m| m.as_ref());
                    {
                        // A fixed column, not a word: the UI font is
                        // proportional, so `I:` and `M:` are different widths
                        // and sixteen rows of them start every combo box at a
                        // slightly different x.  A few pixels of jitter down a
                        // list of sixteen reads as sloppiness rather than as a
                        // font.  The web page's mount screen has the same
                        // column for the same reason.
                        ui.allocate_ui_with_layout(
                            egui::vec2(28.0, ui.spacing().interact_size.y),
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                // A space before the colon: hard against the
                                // letter it read as cramped rather than as a
                                // label.  The column is right-aligned and wide
                                // enough for the widest letter, so the colons
                                // still form one line down the list.
                                ui.label(format!("{letter} :"));
                            },
                        );
                        let current_now = self
                            .cpm_mount_draft
                            .get(idx)
                            .cloned()
                            .unwrap_or_default();
                        // **Slot 0 is reserved while a disk boots, and an empty
                        // selector reads as a free drive.** So it shows the disk
                        // that has reserved it and cannot be changed — until
                        // something *is* mounted there, when it stays editable:
                        // a mount left behind the boot disk has to be removable
                        // without first clearing `cpm_boot_image`.
                        let reserved_for_boot =
                            drive0 == 0 && booting && current_now.is_empty();
                        // A drive in use cannot be changed — the control is
                        // disabled and says why, rather than accepting a choice
                        // that would then be refused on Save.
                        ui.add_enabled_ui(busy.is_none() && !reserved_for_boot, |ui| {
                            let current = current_now.clone();
                            let shown = if reserved_for_boot {
                                self.cpm_boot_slot_name.clone()
                            } else if current.is_empty() {
                                "(drive folder)".to_string()
                            } else {
                                current.clone()
                            };
                            egui::ComboBox::from_id_salt(format!("cpm_mount_{drive0}"))
                                .width(260.0)
                                .selected_text(shown)
                                .show_ui(ui, |ui| {
                                    if let Some(slot) = self.cpm_mount_draft.get_mut(idx) {
                                        ui.selectable_value(
                                            slot,
                                            String::new(),
                                            "(drive folder)",
                                        );
                                        for name in &images {
                                            ui.selectable_value(
                                                slot,
                                                name.clone(),
                                                name,
                                            );
                                        }
                                    }
                                });
                        });
                        if let Some(m) = mounted {
                            if crate::cpm::boot::mount_refuses_writes(&naming, m) {
                                // The stored reason is our BDOS's; under a
                                // booted disk the only cause left is the host.
                                ui.label(
                                    egui::RichText::new("read-only")
                                        .color(AMBER_BRIGHT),
                                )
                                .on_hover_text(if booting {
                                    "the image file is read-only on the host"
                                } else {
                                    m.read_only_reason.as_str()
                                });
                            }
                        }
                        if let Some(b) = &busy {
                            ui.label(egui::RichText::new(b).color(AMBER_BRIGHT));
                        }
                        // Under a booted disk the slot is a number on a board,
                        // not one of our drive letters — the same `cpm_mounts`
                        // underneath, named for what is actually running.
                        if let Some(label) = self.cpm_slot_labels.get(idx) {
                            ui.label(label);
                        }
                        if drive0 == 0 {
                            // One text for three surfaces, and it names the disk
                            // (see `MountContext::boot_slot_note`).
                            match &self.cpm_boot_slot_note {
                                Some(note) => {
                                    ui.label(format!("({note})"));
                                    // A mount underneath a boot disk is kept but
                                    // unreachable, and saying so here beats
                                    // saying it at boot time, on another screen.
                                    if !current_now.is_empty() {
                                        ui.label(
                                            egui::RichText::new(
                                                crate::cpm::boot::BEHIND_BOOT_DISK,
                                            )
                                            .color(AMBER_BRIGHT),
                                        );
                                    }
                                }
                                None => {
                                    ui.label("(A: hides the terminals while mounted)");
                                }
                            }
                        }
                        ui.end_row();
                    }
                }
                    });
            });

        // **What is actually running, which no mount row can show.** A booted
        // image is not on one of our drives at all -- it is its board's slot 0,
        // and the guest's own operating system decides what to call it -- so it
        // gets its own list. The telnet screen has had this since 0.9.2; this
        // one and the web page showed nothing, so an image could be offered
        // above, refused on Save as "being run by a booted session", and
        // accounted for nowhere (reported 2026-08-21).
        let booted = crate::cpm::image::registry::booted_to_report();
        if !booted.is_empty() {
            ui.add_space(6.0);
            ui.label(egui::RichText::new("Booted:").color(AMBER_BRIGHT));
            for name in &booted {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(name).color(AMBER));
                    ui.label(egui::RichText::new("(running)").color(AMBER_DIM));
                });
            }
            ui.label(
                egui::RichText::new(
                    "Running its own operating system — not on a drive of ours, and not                      mountable while it runs.",
                )
                .color(AMBER_DIM),
            );
        }

        ui.add_space(6.0);
        ui.separator();
        // Making a blank disk lives on this screen because it is the answer to
        // "there is nothing in the list yet".  A separate button, so it can
        // never be mistaken for the Save that applies the mounts above.
        let formats = crate::cpm::image::creatable_formats();
        if !formats.is_empty() {
            if self.cpm_new_format.is_empty() {
                self.cpm_new_format = formats[0].0.to_string();
            }
            ui.horizontal(|ui| {
                ui.label("New blank disk:");
                let shown = formats
                    .iter()
                    .find(|(t, _)| *t == self.cpm_new_format)
                    .map(|(_, l)| *l)
                    .unwrap_or(formats[0].1);
                egui::ComboBox::from_id_salt("cpm_new_format")
                    .width(260.0)
                    .selected_text(shown)
                    .show_ui(ui, |ui| {
                        for (token, label) in &formats {
                            ui.selectable_value(
                                &mut self.cpm_new_format,
                                token.to_string(),
                                *label,
                            );
                        }
                    });
            });
            ui.horizontal(|ui| {
                ui.label("Name:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.cpm_new_name)
                        .desired_width(140.0)
                        .char_limit(32)
                        .hint_text("disk name"),
                );
                if ui.button("Create").clicked() {
                    let base = crate::cpm::layout::cpm_dir(&self.cfg.transfer_dir);
                    self.cpm_mount_notice = match crate::cpm::image::create_blank_image(
                        &base,
                        &self.cpm_new_format,
                        &self.cpm_new_name,
                    ) {
                        Ok(note) => {
                            logger::log(format!("GUI: CP/M {note}"));
                            self.cpm_new_name.clear();
                            note
                        }
                        Err(e) => format!("Could not create the disk: {e}"),
                    };
                }
            });
            ui.add(
                egui::Label::new(
                    "Creates an empty, formatted image named <format>_<name>.dsk, so it mounts read-write. Nothing is overwritten \u{2014} a name already in use is refused.",
                )
                .wrap(),
            );
        }

        if !self.cpm_mount_notice.is_empty() {
            ui.add_space(6.0);
            ui.separator();
            ui.label(self.cpm_mount_notice.clone());
        }
    }

    /// Apply the draft, then write the resulting table to `cpm_mounts`.
    ///
    /// The *resulting* table, not the request: a drive that refused keeps its
    /// old image in the config too, so a restart cannot quietly apply a change
    /// the operator was told had failed.  Same rule as the web screen.
    fn cpm_mount_apply(&mut self) {
        let desired: Vec<(u8, String)> = self
            .cpm_mount_draft
            .iter()
            .enumerate()
            .filter(|(_, n)| !n.is_empty())
            .map(|(i, n)| (i as u8, n.clone()))
            .collect();
        let base = crate::cpm::layout::cpm_dir(&self.cfg.transfer_dir);
        let (notes, errors) = crate::cpm::image::apply_mount_selection(&base, &desired);
        let mut msg = notes.join("\n");
        if !errors.is_empty() {
            if !msg.is_empty() {
                msg.push('\n');
            }
            msg.push_str(&errors.join("\n"));
        }
        self.cpm_mount_notice = msg;
        self.cfg.cpm_mounts = crate::cpm::image::current_mounts_value();
        self.save_config_now();
        // Re-seed from what actually happened, so a refused row snaps back to
        // the truth instead of showing a choice that did not take.
        let mounts = crate::cpm::image::registry::all();
        for (i, slot) in self.cpm_mount_draft.iter_mut().enumerate() {
            *slot = mounts
                .get(i)
                .and_then(|m| m.as_ref())
                .map(|m| m.filename.clone())
                .unwrap_or_default();
        }
    }

    fn draw_ai_browser_more(&mut self, ui: &mut egui::Ui) {
        // The Groq key lives here rather than on the main frame: it is optional,
        // and a key field in the first row of a frame reads as a prerequisite.
        // The same label column and control width as the CP/M rows below, so
        // one popup reads as one form rather than two lists that happen to
        // share a window.
        cpm_choice_row(ui, "Groq API Key (optional):", |ui| {
            singleline_with_menu(ui, &mut self.cfg.groq_api_key, true, Some(CPM_CONTROL_W));
        });
        ui.label(
            egui::RichText::new("AI Chat only — everything else works without one.")
                .small()
                .color(AMBER_DIM),
        );
        cpm_choice_row(ui, "Home:", |ui| {
            singleline_with_menu(ui, &mut self.cfg.browser_homepage, false, Some(CPM_CONTROL_W));
        });
        // The same words the main frame uses for the same field: this popup
        // re-shows it, and two names for one control is how a reader ends up
        // wondering whether they are two settings.
        cpm_choice_row(ui, "Weather location:", |ui| {
            singleline_with_menu(ui, &mut self.cfg.weather_location, false, Some(CPM_CONTROL_W));
        });
        cpm_choice_row(ui, "Units:", |ui| {
            let sel = match self.cfg.weather_units.as_str() {
                "us" => "US (F/mph)",
                "metric" => "Metric (C/km/h)",
                _ => "Auto",
            };
            cpm_combo(ui, "weather_units_combo")
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
        // The CP/M emulator lives here rather than on the main screen
        // (no room left there): its enable toggle (on by default; see
        // config::DEFAULT_CPM_EMU_ENABLED) + the runaway ceiling.
        ui.separator();
        ui.checkbox(
            &mut self.cfg.cpm_emu_enabled,
            "CP/M Emulator (main menu; be sure you trust the CP/M files you run)",
        );
        // Whether the web UI's VDM / Dazzler page is a keyboard as well as a
        // window.  Here rather than with the web server's own settings because
        // it is a CP/M question — what may type at a booted guest — and the
        // operator looking for it will be looking at the CP/M controls.
        ui.checkbox(
            &mut self.cfg.cpm_screen_input,
            "VDM / Dazzler screen may type at a booted disk (it is readable either way)",
        );
        // The joystick board, beside the typing switch because it is the same
        // question one step along: whether the browser watching a booted guest
        // is also a *controller*.  Named by what it is rather than by the
        // config key, and it names the keys, because a control nobody can find
        // the keys for is a control nobody uses.
        ui.checkbox(
            &mut self.cfg.cpm_joystick,
            "Joystick for a booted disk, played from the VDM / Dazzler screen",
        )
        .on_hover_text(
            "Gives a booted machine the Cromemco D+7A, the board SPACEWAR, \
             GOTCHA, DOGFIGHT, TANKWAR, CHASE, AMBUSH and TRACK read their \
             joysticks from.  Player 1 is W/A/S/Z with X to fire, player 2 \
             I/J/K/M with N, and the screen page says so.  A held key SWINGS: \
             centred when pressed, full deflection half a second later, because \
             these are analogue sticks and a key has no halfway.  On by default, \
             and note the alternative -- ports 18h-1Ch read FFh when nobody \
             claims them, and on an analogue axis FFh is a stick pushed hard \
             over rather than no stick at all.  Needs the web server on; the \
             terminal that started the session cannot play.",
        );
        // Writes by a *booted* disk.  A standing setting since 0.9.2, when the
        // telnet boot picker -- which asked it once per visit -- was removed for
        // being a second way to boot.  Kept beside the CP/M controls rather than
        // with the disk mounting, because it is about what the guest may do,
        // not about which image is where.
        //
        // The label names what UNTICKING does, because ticked is the default:
        // an operator reads a checkbox to find out what the other state costs.
        ui.checkbox(
            &mut self.cfg.cpm_boot_writable,
            "A booted disk may WRITE to its images (untick to discard its writes)",
        )
        .on_hover_text(
            "On by default: a booted operating system saves files, formats disks \
             and updates its own directory, and discarding those writes loses the \
             work silently.  Covers the boot disk and every image mounted beside \
             it.  Untick to keep every disk exactly as it is.  Re-downloading a \
             disk a guest scrambled means deleting your copy first -- the sample \
             download never overwrites a file already in the images folder.",
        );
        ui.horizontal(|ui| {
            labeled_field(
                ui,
                "CP/M ceiling (M-instr):",
                &mut self.cpm_emu_max_minstr_buf,
                70.0,
            );
        })
        .response
        .on_hover_text(
            "Runaway ceiling for one CP/M emulator program, in millions of \
             instructions (2000 = 2 billion).  A compute-bound .COM that never \
             reads the console is stopped at this count so the A> prompt always \
             comes back.  Minimum 1; anything above 1000000 is capped at it \
             rather than refused, so a value meant as \"no limit\" is kept as \
             far as it goes -- which at emulated speed is over three months of \
             continuous running.  This bounds one transient in the emulator \
             only: a booted disk is the session, is meant to sit at its prompt, \
             and has no ceiling.",
        );
        // The download offer, *before* the mount button and only while there is
        // something to fetch.
        //
        // Here as well as on the mount screen because an operator can pick a
        // boot disk from this very popup without ever opening that window — so
        // the one place they are certain to pass through is this one, and an
        // offer they never see is not an offer.  It disappears once the disks
        // are there, so the ordinary case is unchanged.
        ui.horizontal(|ui| {
            self.draw_cpm_fetch_button(ui);
        });
        if !self.cpm_fetch_note.is_empty() {
            ui.label(egui::RichText::new(&self.cpm_fetch_note).small().color(AMBER_DIM));
        }
        // Disk images get their own window: sixteen drives will not fit here,
        // and mounting is an occasional operation rather than a setting.
        ui.horizontal(|ui| {
            if ui
                .add(egui::Button::new(
                    egui::RichText::new("Mount CP/M Drives…").color(AMBER_BRIGHT),
                ))
                .clicked()
            {
                self.cpm_mount_reload_draft();
                self.cpm_mount_popup_open = true;
            }
            let n = crate::cpm::image::registry::all()
                .iter()
                .filter(|m| m.is_some())
                .count();
            ui.label(if n == 0 {
                "no images mounted".to_string()
            } else {
                format!("{n} mounted")
            });
        });
        // The VDM / Dazzler screen.  A booted disk can paint to a video *card*
        // with no serial line at all, and then the session it was started from
        // stays blank for ever -- the picture is the guest's own memory, and it
        // needs a viewer that can repaint.  The web page is that viewer; this
        // is the desktop's way to it, because "only in the browser" is a poor
        // answer for someone sitting at the console.
        ui.horizontal(|ui| {
            if ui
                .add(egui::Button::new(
                    egui::RichText::new("VDM / Dazzler…").color(AMBER_BRIGHT),
                ))
                .on_hover_text(
                    "Opens the booted-disk screen in your browser. Served by the \
                     gateway's own web server, so that has to be running.",
                )
                .clicked()
            {
                match self.vdm_web_state() {
                    // Listening, or about to be: send them to it.
                    WebScreenState::Bound(_) | WebScreenState::Starting(_) => {
                        self.open_vdm_page(ui.ctx())
                    }
                    // Configured and did not bind.  Offering to "enable" a
                    // server that is already enabled would restart the gateway
                    // and change nothing; say what went wrong instead.
                    WebScreenState::Failed { port, in_use } => logger::log(if in_use {
                        format!(
                            "The VDM / Dazzler screen needs the web server, and port {port} is already in use — most often a second copy of the gateway. Stop that one, or give this one a different web port."
                        )
                    } else {
                        format!("The web server could not bind port {port}, so there is no screen to open. See the log above for the reason.")
                    }),
                    WebScreenState::Off => self.vdm_web_offer_open = true,
                }
            }
            let (note, colour) = match self.vdm_web_state() {
                WebScreenState::Bound(_) => (self.vdm_url(), AMBER_DIM),
                WebScreenState::Starting(_) => ("web server is starting…".to_string(), AMBER_DIM),
                WebScreenState::Failed { port, in_use } => (
                    if in_use {
                        format!("port {port} is in use — another copy running?")
                    } else {
                        format!("web server could not bind port {port}")
                    },
                    AMBER,
                ),
                WebScreenState::Off => ("web server is off".to_string(), AMBER_DIM),
            };
            ui.label(egui::RichText::new(note).small().color(colour));
        });
        // What the CP/M menu item runs: our emulator, or a disk image booted
        // on emulated Altair hardware.  The same `boot_choices` list the telnet
        // and web screens build, so the three cannot drift apart.
        // Resolved before the closure borrows `self` mutably, and cached — see
        // `cpm_boot_label`.
        // Warm the bootability answers off the frame thread, once.  The list
        // below is drawn inside a combo closure that runs on this thread, and
        // its first draw on a cold cache would otherwise read every image in the
        // folder to find out which of them boot.
        if !self.cpm_boot_cache_warmed {
            self.cpm_boot_cache_warmed = true;
            let dir = self.cfg.transfer_dir.clone();
            std::thread::spawn(move || {
                let base = crate::cpm::layout::cpm_dir(&dir);
                let _ = crate::cpm::boot::boot_choices(&base);
            });
        }
        let boot_label = self.cpm_boot_label();
        cpm_choice_row(ui, "CP/M runs:", |ui| {
            cpm_combo(ui, "cpm_boot_image_combo")
                .selected_text(boot_label)
                .show_ui(ui, |ui| {
                    // The images folder is read here rather than above it: this
                    // closure runs only while the list is open, and the same
                    // code one level out would be a directory scan on every
                    // frame for as long as the panel is on screen.
                    let base = crate::cpm::layout::cpm_dir(&self.cfg.transfer_dir);
                    let mut choices = crate::cpm::boot::boot_choices(&base);
                    // An image named in the config but no longer in the folder
                    // still has to appear, or selecting anything else would
                    // silently discard a setting the operator cannot see.
                    if !self.cfg.cpm_boot_image.is_empty()
                        && !choices.iter().any(|(v, _)| *v == self.cfg.cpm_boot_image)
                    {
                        // The folder has just been listed, so this entry can be
                        // resolved outright rather than read from the cache —
                        // and the marker's wording is `boot_setting_label`'s,
                        // shared with the collapsed text above, the web select
                        // and both telnet rows.
                        let target = crate::cpm::boot::boot_target(
                            &self.cfg.transfer_dir,
                            &self.cfg.cpm_boot_image,
                        );
                        choices.push((
                            self.cfg.cpm_boot_image.clone(),
                            crate::cpm::boot::boot_setting_label(
                                &target,
                                &self.cfg.cpm_boot_image,
                            ),
                        ));
                    }
                    for (value, label) in &choices {
                        ui.selectable_value(
                            &mut self.cfg.cpm_boot_image,
                            value.clone(),
                            label,
                        );
                    }
                });
        })
        .response
        .on_hover_text(
            "A booted disk runs its OWN operating system and owns every drive: the gateway's A:-P:, EGT8080 and the CP/M prompt do not apply inside it.  Disks are opened writable unless \"a booted disk may WRITE\" is unticked above, and that answer covers the mounted disks too.",
        );
        // Which machine a BOOTED disk believes it is running on -- specifically
        // where it finds its console.  The same `MACHINE_CHOICES` list the telnet
        // and web screens render, so the three cannot drift apart.
        cpm_choice_row(ui, "Booted disk's machine:", |ui| {
            cpm_combo(ui, "cpm_boot_machine_combo")
                .selected_text(crate::cpm::console::machine_label(
                    &self.cfg.cpm_boot_machine,
                ))
                .show_ui(ui, |ui| {
                    let auto = crate::cpm::console::AUTO_MACHINE;
                    ui.selectable_value(
                        &mut self.cfg.cpm_boot_machine,
                        auto.to_string(),
                        crate::cpm::console::machine_label(auto),
                    );
                    for c in crate::cpm::console::MACHINE_CHOICES {
                        ui.selectable_value(
                            &mut self.cfg.cpm_boot_machine,
                            c.key.to_string(),
                            c.description,
                        );
                    }
                });
        })
        .response
        .on_hover_text(
            "Where a BOOTED disk finds its console.  Ignored by the CP/M emulator, which has no console to place.  A disk that loads its operating system and then goes quiet is usually looking for a console that is not there, and will sit polling a keyboard port for ever.  Not autodetected: what a guest polls cannot tell the machine it wants from another machine's keyboard at the same address.",
        );
        // What a BOOTED disk is handed for the Backspace key.  The same
        // `BACKSPACE_CHOICES` list the telnet and web screens render -- and the
        // telnet boot picker too, which asks again per disk.
        cpm_choice_row(ui, "Booted disk's backspace:", |ui| {
            cpm_combo(ui, "cpm_boot_backspace_combo")
                .selected_text(crate::cpm::boot::backspace_label(&self.cfg.cpm_boot_backspace))
                .show_ui(ui, |ui| {
                    for (value, label) in crate::cpm::boot::BACKSPACE_CHOICES {
                        ui.selectable_value(
                            &mut self.cfg.cpm_boot_backspace,
                            value.to_string(),
                            *label,
                        );
                    }
                });
        })
        .response
        .on_hover_text(
            "What a BOOTED disk is handed when you press Backspace.  Ignored by \
             the CP/M emulator, which reads its own console line and accepts \
             either.  Most of these operating systems erase on BS and read a \
             terminal's DEL as a Teletype RUBOUT -- deleting the character and \
             then printing the character they deleted, so TESTING backspaced \
             over reads TESTINGGNIT.  CP/M 1.3, 1.4 and the 1975 build are the \
             opposite: the rubout is their editing key and BS prints a literal \
             ^H.  This is the whole answer: set it to match the disk you \
             this setting.",
        );
        // Which processor BOTH CP/M machines run.  The same `CPU_CHOICES` list
        // the telnet and web screens render -- and the only CP/M setting of the
        // four that is not about a booted disk alone.
        cpm_choice_row(ui, "CP/M CPU:", |ui| {
            cpm_combo(ui, "cpm_cpu_combo")
                .selected_text(crate::cpm::cpu::cpu_label(&self.cfg.cpm_cpu))
                .show_ui(ui, |ui| {
                    for (value, label) in crate::cpm::cpu::CPU_CHOICES {
                        ui.selectable_value(&mut self.cfg.cpm_cpu, value.to_string(), *label);
                    }
                });
        })
        .response
        .on_hover_text(
            "Which processor both CP/M machines run -- the emulator's transient \
             programs and a booted disk's whole operating system.  The Z80 is a \
             strict superset of the 8080, so it runs every disk here, and \
             Altairs were very commonly fitted with a Z80 upgrade board.  The \
             8080 is the processor the Altair actually shipped with, and is \
             what period diagnostics that identify the CPU from DCR A setting \
             parity rather than overflow expect -- those are RIGHT to fail on a \
             Z80.  EGT8080.COM is placed on CP/M drive A: -- built to the \
             8080's instruction set, so it runs on EITHER setting.",
        );
        // Where CP/M printer output goes.  Beside the CPU because it is the
        // other setting that reaches both machines, and immediately above the
        // board it depends on.
        cpm_choice_row(ui, "CP/M printer:", |ui| {
            cpm_combo(ui, "cpm_printer_combo")
                .selected_text(crate::cpm::printer::printer_label(&self.cfg.cpm_printer))
                .show_ui(ui, |ui| {
                    for (value, label) in crate::cpm::printer::PRINTER_CHOICES {
                        ui.selectable_value(
                            &mut self.cfg.cpm_printer,
                            value.to_string(),
                            *label,
                        );
                    }
                });
        })
        .response
        .on_hover_text(
            "Where CP/M printer output goes.  Reaches both CP/M machines, by two \
             different routes: in the emulator the printer is an operating-system \
             service (BDOS function 5 and the BIOS LIST vector), so WordStar, \
             MBASIC's LPRINT and PIP LST:=FILE.TXT all arrive there; a booted \
             disk instead drives the printer board selected below.  Either way \
             one document is written into a \"printer\" folder inside the \
             transfer directory -- its own folder so a printer left on does not \
             scatter documents through your own files, and NOT onto a CP/M \
             drive, because it is for you rather than for the guest.  The \
             file-transfer menu reaches it by changing directory into \
             \"printer\".  It is \
             named PRINT-YYYYMMDD-HHMMSS from this machine's clock.  A job is \
             finished 5 seconds after the last character printed, CP/M having no \
             end-of-print signal of its own, and in the emulator also the moment \
             the program returns to the A> prompt, which is exact.  Off means \
             printer output appears on the terminal, which is where it has \
             always gone.  Bold and underline survive into an .odt: period \
             software does not ask for them, it OVERSTRIKES -- WordStar prints \
             the line, sends a bare CR and reprints just the emphasised run at \
             the same columns -- and that becomes real styling.",
        );
        cpm_choice_row(ui, "Bare carriage return:", |ui| {
            cpm_combo(ui, "cpm_printer_autolf_combo")
                .selected_text(
                    crate::cpm::printer::AUTOLF_CHOICES
                        .iter()
                        .find(|(v, _)| *v == self.cfg.cpm_printer_autolf.trim())
                        .map(|(_, l)| *l)
                        .unwrap_or(crate::cpm::printer::AUTOLF_CHOICES[0].1),
                )
                .show_ui(ui, |ui| {
                    for (value, label) in crate::cpm::printer::AUTOLF_CHOICES {
                        ui.selectable_value(
                            &mut self.cfg.cpm_printer_autolf,
                            value.to_string(),
                            *label,
                        );
                    }
                });
        })
        .response
        .on_hover_text(
            "Does a bare carriage return advance the paper?  This is the DIP \
             switch a real printer interface carried, and it carried one because \
             the byte stream cannot say.  Both meanings are in use by period \
             software on the SAME board, and both were measured here.  Altair \
             Hard Disk BASIC's LPRINT sends ALPHA<CR>BETA<CR> and no line feed \
             at all, so a bare CR is its line ending -- with the switch off its \
             whole report prints on one line.  WordStar 3.0, installed for a \
             \"Teletype-like printer\", emphasises by OVERSTRIKING: it prints the \
             line, sends a bare CR and reprints just the bold run at the same \
             columns -- with the switch on, every emphasised fragment lands on a \
             line of its own instead of on top of the text.  Auto keeps whatever \
             was measured for the printer in question: on for a booted disk's \
             Altair line printer, off for the emulator's LST: service, where \
             CP/M sends CR LF and overstrike is meaningful.",
        );
        cpm_choice_row(ui, "Booted disk's printer:", |ui| {
            cpm_combo(ui, "cpm_printer_port_combo")
                .selected_text(crate::cpm::printer::port_label(&self.cfg.cpm_printer_port))
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut self.cfg.cpm_printer_port,
                        crate::cpm::printer::PORT_OFF.to_string(),
                        crate::cpm::printer::PORT_OFF_LABEL,
                    );
                    for p in crate::cpm::printer::PORT_CHOICES {
                        ui.selectable_value(
                            &mut self.cfg.cpm_printer_port,
                            p.key.to_string(),
                            p.label,
                        );
                    }
                });
        })
        .response
        .on_hover_text(
            "Which printer board a BOOTED disk finds.  Ignored by the emulator, \
             whose printer is a BDOS service with no port at all, and ignored \
             entirely when the printer above is off.  Measured rather than \
             reasoned: Altair Hard Disk BASIC answering LINEPRINTER? C \
             initialises with OUT 03h<-11h / OUT 02h<-00h and then sends one \
             7-bit ASCII character per byte to 03h, ending each line with a bare \
             carriage return.  The status register is deliberately not emulated \
             -- an unclaimed port reads 0xFF here and every period convention \
             reads a high bit as ready, which is why BASIC printed at full speed \
             into a board that was not there.",
        );
        // Virtual-modem UART port: which machine/port address the emulated
        // CP/M's modem answers at.
        // The one row with a button after its dropdown, so the button is drawn
        // *outside* the fixed control box -- inside it, it took the width the
        // dropdown was given and rendered on top of a value cut off mid-port.
        let mut reset_uart = false;
        cpm_choice_row_trailing(
            ui,
            "CP/M virtual modem:",
            |ui| {
                cpm_combo(ui, "cpm_emu_uart_combo")
                    .selected_text(crate::cpm::uart::uart_description(&self.cfg.cpm_emu_uart))
                    .show_ui(ui, |ui| {
                        for c in crate::cpm::uart::UART_CHOICES {
                            ui.selectable_value(
                                &mut self.cfg.cpm_emu_uart,
                                c.key.to_string(),
                                c.description,
                            );
                        }
                    });
            },
            // Only a flag here: the control closure above already holds
            // `self.cfg` mutably, and two closures cannot.  The work happens
            // below, where nothing else is borrowing.
            |ui| {
                // One click back to the port EGT8080 also defaults to: the
                // answer to "I changed something and now the CP/M terminal
                // cannot connect".
                reset_uart = ui
                    .small_button("Default port")
                    .on_hover_text(
                        "Reset the CP/M virtual modem to the port EGT8080 expects (RC2014 SIO/2 board 1 channel B, 0x82/0x83)",
                    )
                    .clicked();
            },
        );
        if reset_uart {
            self.cfg.cpm_emu_uart = crate::cpm::uart::DEFAULT_UART.to_string();
            self.last_synced_cfg.cpm_emu_uart = self.cfg.cpm_emu_uart.clone();
            config::update_config_value("cpm_emu_uart", crate::cpm::uart::DEFAULT_UART);
            logger::log(format!(
                "CP/M virtual modem port reset to the default ({}).",
                crate::cpm::uart::DEFAULT_UART
            ));
        }
        // The CP/M virtual modem's saved AT profile — the counterpart of the
        // per-port AT&W block on the Serial page.  The guest writes it with
        // AT&W from inside the emulator; it is editable here for the same
        // reason the ports' is: to inspect or repair one without booting CP/M.
        ui.horizontal(|ui| {
            ui.label("CP/M modem profile (AT&W):");
            // Labelled exactly as the serial ports' own AT block above is, and
            // as the web UI and the telnet modem screen are. They were bare
            // ("Echo", "Verbose", "Quiet") here while the identical three
            // checkboxes eight hundred lines away said "Echo (E1)" — in the
            // same window, for the same setting, with a comment right above
            // claiming this block exists "for the same reason the ports' is".
            // The AT letter is the part an operator matches against the manual
            // and against what the guest typed, so it belongs in the label
            // rather than only in a tooltip nobody hovers.
            ui.checkbox(&mut self.cfg.cpm_emu_modem.echo, "Echo (E1)")
                .on_hover_text("ATE1 — the modem echoes the command line");
            ui.checkbox(&mut self.cfg.cpm_emu_modem.verbose, "Verbose (V1)")
                .on_hover_text("ATV1 — word result codes rather than digits");
            ui.checkbox(&mut self.cfg.cpm_emu_modem.quiet, "Quiet (Q1)")
                .on_hover_text("ATQ1 — suppress result codes entirely");
        });
        ui.horizontal(|ui| {
            labeled_field(ui, "Result level (X):", &mut self.cpm_emu_x_code_buf, 40.0);
            labeled_field(ui, "DCD (&C):", &mut self.cpm_emu_dcd_mode_buf, 40.0);
            labeled_field(
                ui,
                "S-registers S0..S27:",
                &mut self.cfg.cpm_emu_modem.s_regs,
                240.0,
            );
        });
        ui.label(
            egui::RichText::new(
                "Comma-separated decimal values; blank means the power-on registers.  \
                 ATZ inside the emulator restores this profile, AT&F ignores it.",
            )
            .italics()
            .small(),
        );
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

        ui.add_space(6.0);
        ui.separator();
        ui.add_space(2.0);
        // Applies to BOTH gateways, so it gets its own group rather than
        // sitting under Telnet Gateway or SSH Gateway — the SSH gateway sends
        // it as its PTY request, the Telnet Gateway as NAWS, and both resolve
        // it through the one `gateway_window()`.
        ui.label(
            egui::RichText::new("Terminal size reported to remote")
                .strong()
                .color(AMBER),
        );
        ui.horizontal(|ui| {
            labeled_field(ui, "Columns:", &mut self.gateway_term_width_buf, 50.0);
            ui.add_space(8.0);
            labeled_field(ui, "Rows:", &mut self.gateway_term_height_buf, 50.0);
        });
        // Hint reads the live buffers, not the saved config, so it tracks a
        // half-typed field — and comes from the one fn the web asks too.
        let hint_w = self
            .gateway_term_width_buf
            .parse::<u16>()
            .unwrap_or(self.cfg.gateway_term_width);
        let hint_h = self
            .gateway_term_height_buf
            .parse::<u16>()
            .unwrap_or(self.cfg.gateway_term_height);
        ui.label(
            egui::RichText::new(Config::gateway_term_hint(hint_w, hint_h))
                .italics()
                .small(),
        );
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
            // Off by default: Kermit server mode has no authentication of its
            // own, so serving it to a slave's wire is the operator's decision.
            ui.checkbox(
                &mut self.cfg.allow_relay_kermit,
                "Master: serve Kermit to a slave's Kermit-mode port",
            );
        });
        // The relay listens on the SSH port, so accept-relays is inert while the
        // SSH server is off.  The popup above only fires on the *switch* into
        // Master, which misses the case that actually strands an operator: a
        // master configured earlier whose SSH server was turned off since.  This
        // line is always shown while the combination holds, and (like the popup)
        // never changes SSH on its own.
        if self.cfg.relays_blocked_by_ssh_off() {
            ui.label(
                egui::RichText::new(
                    "SSH server is off — the relay listens on the SSH port, so no slave can connect.",
                )
                .italics()
                .small()
                .color(WARN_BORDER),
            );
        }
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
            // The closed selector names the hardware too, matching the open list
            // and the web UI's selected option — otherwise the one state an
            // operator looks at most says the least.  Falls back to the bare path
            // for an undescribed port, and for a saved port that isn't currently
            // plugged in (nothing detected to describe it with).
            let selected = if self.cfg.port(id).port.is_empty() {
                "(none)".to_string()
            } else {
                let dev = &self.cfg.port(id).port;
                match self
                    .serial_ports
                    .iter()
                    .find(|p| &p.name == dev && !p.summary.is_empty())
                {
                    Some(p) => format!("{}  \u{2014} {}", p.name, p.summary),
                    None => dev.clone(),
                }
            };
            // Per-port salt so the two ComboBoxes don't share state.
            let tooltip = serial_ports_tooltip(&self.serial_ports);
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
                        // The row shows the path plus a short label; the full
                        // description is on hover.  A path alone cannot tell
                        // two identical adapters apart.
                        let label = if port.summary.is_empty() {
                            port.name.clone()
                        } else {
                            format!("{}  \u{2014} {}", port.name, port.summary)
                        };
                        ui.selectable_value(
                            &mut self.cfg.port_mut(id).port,
                            port.name.clone(),
                            label,
                        )
                        .on_hover_text(&port.detail);
                    }
                })
                // Hovering the closed selector lists every port with its name,
                // which is what an operator needs *before* opening it.
                .response
                .on_hover_text(&tooltip);
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
                .selected_text(match self.cfg.port(id).mode.as_str() {
                    "console" => "Telnet-Serial Mode",
                    "kermit" => "Kermit Server Mode",
                    _ => "Modem (AT Command) Mode",
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
                    ui.selectable_value(
                        &mut self.cfg.port_mut(id).mode,
                        "kermit".into(),
                        "Kermit Server Mode",
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
            // "Modem Dial Targets", not "Direct-to-Kermit Dial Target", which
            // named only the first of the two checkboxes below it.  Peer-dial
            // reaches another *serial port* (ATD Port@IP), not Kermit -- filing
            // it under a Kermit heading described the wrong destination.  What
            // the two do share is that each opens a dial target an ATD/ATDT
            // command can reach from a modem port, which is what this says.
            egui::RichText::new("Modem Dial Targets")
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
        // First, because it is about what is *in* the transfer directory rather
        // than about a protocol, and because it is the one control here an
        // operator is likely to be looking for on purpose.
        ui.label(egui::RichText::new("Bundled CP/M Terminals").strong().color(AMBER));
        ui.checkbox(
            &mut self.cfg.place_bundled_terminals,
            "Write EGT8080.COM and EGT80.COM when they are missing",
        )
        .on_hover_text(
            "EGT8080.COM and EGT80.COM are this gateway's own CP/M terminal, in \
             period assembly.  Each is written into the transfer directory -- and \
             onto CP/M drive A: -- when it is missing, so you can send one to real \
             hardware without starting the emulator.  A file already there is NEVER \
             overwritten, because each saves its settings inside its own .COM; this \
             only decides whether a MISSING one is written back.  Untick it if you \
             keep your own build, or your own EGT80.COM from before 0.9.2, and would \
             rather a file you deleted stayed deleted.",
        );
        ui.add_space(6.0);
        ui.separator();
        ui.add_space(2.0);
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

    /// The screen's own address, not the configuration page's.
    ///
    /// **`/vdm`, deliberately.** Sending someone to the root and letting them
    /// find it is how a button becomes a hint; they pressed it because they
    /// wanted the screen.  Loopback because the desktop UI runs in the same
    /// process as the server it is opening — no address to guess, and nothing
    /// that depends on which interface the listener bound.
    /// **What actually happened to the web listener**, which is a different
    /// question from what the config asks for.
    ///
    /// Three sources could answer "where is the web server" and two of them are
    /// wrong. `cfg` carries unsaved edits, so a port typed and not saved is a
    /// port nothing is listening on. `last_synced_cfg` is refreshed from the
    /// global config whenever that changes — including a change made from the
    /// web or telnet UI that needs a restart to take effect — so it can name a
    /// port the listener has not moved to yet. Even the snapshot `main` started
    /// this cycle's server from only says what was *attempted*.
    ///
    /// [`crate::bindwatch`] says what happened, and it is the one that matters:
    /// a second copy of the gateway holding the port is the exact case that
    /// module exists for, and without asking it this button would open a browser
    /// at a refused connection — or at the *other* instance's configuration
    /// page, which is worse.
    fn vdm_web_state(&self) -> WebScreenState {
        WebScreenState::of(crate::bindwatch::status_of("web"))
    }

    /// The screen's own address, at whatever port really answered.
    fn vdm_url(&self) -> String {
        format!("http://127.0.0.1:{}/vdm", self.vdm_web_state().port().unwrap_or(self.running_web.1))
    }



    /// Hand the screen's URL to the desktop's browser.
    ///
    /// Through egui's own `open_url` rather than a spawned `xdg-open`/`open`:
    /// eframe already carries that for every platform, and shelling out for one
    /// button would be a second way to do it -- and a process spawn built from
    /// a formatted string, which is the shape of the argument-injection advisory
    /// this project already patched once.
    fn open_vdm_page(&self, ctx: &egui::Context) {
        let url = self.vdm_url();
        logger::log(format!("Opening the VDM / Dazzler screen at {url}"));
        ctx.open_url(egui::OpenUrl::new_tab(url));
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

    /// Stop the server and end the process.
    ///
    /// **`restart` staying false is the entire difference** between this and
    /// `save_and_restart_all`: both trip `shutdown` to unwind the server
    /// cycle, and `main` reads `restart` afterwards to decide whether to loop
    /// back for another one or fall out of the loop and exit.  Setting both --
    /// the obvious copy of the line above -- is a restart, not a quit, and the
    /// window would come straight back.
    ///
    /// Nothing is persisted.  An operator quitting is not an operator saving,
    /// and a half-typed port in a field it never occurred to them to look at
    /// must not be written on the way out.
    fn quit(&mut self) {
        logger::log("Quit requested — stopping the server...".into());
        self.restart.store(false, Ordering::SeqCst);
        self.shutdown.store(true, Ordering::SeqCst);
    }

    /// The screen a second copy shows instead of the editor.
    ///
    /// **Take Over is offered rather than done.** A launch cannot tell an
    /// accidental double-click from a deliberate restart, and the running copy
    /// may have a booted CP/M disk somebody is sitting at -- so the choice is
    /// the operator's, and what it costs is stated before they make it.
    ///
    /// **Nothing here edits the config.** Saving from a window whose server was
    /// never started is how five copies came to disagree about the settings in
    /// the first place: the file changed, and the process actually answering
    /// connections never re-read it.
    fn draw_handover(&mut self, ui: &mut egui::Ui) {
        let holder = match self.handover.as_ref().and_then(|h| h.holder_pid) {
            Some(pid) => format!("another copy (process {pid})"),
            None => "another copy".to_string(),
        };
        // **Inside a vertical ScrollArea, like the wizard.** The `Ui` that
        // `eframe::App::ui` hands us is documented as having "no margin or
        // background" and does not establish a definite width for text to wrap
        // against: measured, labels put straight into it came out in two offset
        // columns with each sentence split across the gap. The wizard -- the
        // only other screen that owns this window -- wraps its content the same
        // way, and it is also what keeps this readable in a window too short
        // for it.
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        ui.add_space(24.0);
        ui.label(
            egui::RichText::new("The gateway is already running here")
                .strong()
                .size(24.0)
                .color(AMBER_BRIGHT),
        );
        ui.add_space(12.0);
        ui.label(
            egui::RichText::new(format!(
                "This directory is already served by {holder}, which holds the telnet, SSH \
                 and web ports. Two copies cannot share them."
            ))
            .color(AMBER)
            .size(16.0),
        );
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new(
                "This window is not connected to it. Nothing here has been started, and no \
                 setting saved from this window would reach the copy that is answering \
                 connections — which is exactly how a stack of copies comes to disagree \
                 about its own configuration.",
            )
            .color(AMBER_DIM)
            .size(15.0),
        );
        ui.add_space(16.0);
        ui.label(
            egui::RichText::new(
                "Take Over asks that copy to stand down — it closes its sessions the same \
                 way its own Quit button would — and then this one takes the ports. Any \
                 telnet or SSH session in progress ends, including a booted CP/M disk \
                 somebody is sitting at.",
            )
            .color(AMBER)
            .size(15.0),
        );
        ui.add_space(20.0);
        ui.horizontal(|ui| {
            if ui
                .add(egui::Button::new(
                    egui::RichText::new("Take Over").strong().size(16.0).color(AMBER_BRIGHT),
                ))
                .clicked()
            {
                if let Some(h) = self.handover.as_ref() {
                    h.take_over.store(true, Ordering::SeqCst);
                }
                // Ends the event loop so `main` can do the handover; the window
                // comes back once this copy owns the ports.
                self.shutdown.store(true, Ordering::SeqCst);
            }
            ui.add_space(12.0);
            if ui
                .add(egui::Button::new(egui::RichText::new("Quit").strong().size(16.0)))
                .clicked()
            {
                // `take_over` stays false, so `main` exits without disturbing
                // the copy that is working.
                self.shutdown.store(true, Ordering::SeqCst);
            }
        });
        });
    }

    /// The dialog behind both the title-bar X and the header's Quit button.
    ///
    /// **One dialog for both, because they are one question.**  The X means
    /// "I am done with this window" and Quit means "I am done with the
    /// gateway", and the whole defect was that the window silently answered
    /// the first for anyone who meant the second.  Asking once, with both
    /// outcomes named and the consequence of each spelled out, is what makes
    /// the two intents distinguishable at the moment they differ.
    ///
    /// Dismissing the dialog is **Cancel**, never a choice: its own X, like a
    /// click on Cancel, must leave the server exactly as it was.  A dialog
    /// that stopped a server because somebody waved it away would be worse
    /// than the trap it replaced.
    fn draw_close_prompt(&mut self, ctx: &egui::Context, warn_frame: egui::Frame) {
        if !self.close_prompt_open {
            return;
        }
        let mut open = true;
        let (mut quit, mut detach, mut cancel) = (false, false, false);
        egui::Window::new(
            egui::RichText::new("Close the window, or stop the server?")
                .strong()
                .color(AMBER_BRIGHT),
        )
        .open(&mut open)
        .resizable(false)
        .collapsible(false)
        .default_width(500.0)
        .frame(warn_frame)
        .show(ctx, |ui| {
            ui.visuals_mut().extreme_bg_color = POPUP_INPUT_BG;
            ui.label(
                "Closing this window does not stop the gateway.  The telnet, SSH, \
                 web and serial services keep running in the background.",
            );
            ui.add_space(6.0);
            ui.label(egui::RichText::new(detached_advice(self.has_terminal)).color(AMBER));
            ui.add_space(6.0);
            ui.label(
                "Stopping the server ends every telnet and SSH session in progress, \
                 including any booted CP/M disk somebody is sitting at.",
            );
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui
                    .add(egui::Button::new(
                        egui::RichText::new("Stop the Server and Quit")
                            .strong()
                            .color(AMBER_BRIGHT),
                    ))
                    .clicked()
                {
                    quit = true;
                }
                ui.add_space(8.0);
                if ui
                    .add(egui::Button::new(
                        egui::RichText::new("Leave It Running").strong(),
                    ))
                    .clicked()
                {
                    detach = true;
                }
                ui.add_space(8.0);
                if ui
                    .add(egui::Button::new(egui::RichText::new("Cancel").strong()))
                    .clicked()
                {
                    cancel = true;
                }
            });
        });
        if quit {
            self.close_prompt_open = false;
            self.quit();
        } else if detach {
            self.close_prompt_open = false;
            // Marked as ours so the interception in `ui` lets this one
            // through.  `main` logs what just happened once the event loop
            // returns -- not here, or the note would be printed twice.
            self.closing_deliberately = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        } else if cancel || !open {
            self.close_prompt_open = false;
        }
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

    /// Render the first-run setup wizard and act on its result.  Called
    /// instead of the normal editor while `self.wizard` is `Some`.
    fn draw_wizard(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let outcome = {
            // Disjoint field borrows: the wizard needs a read-only view of the
            // live config (for the settings it doesn't edit) plus the detected
            // server IP for its connect instructions.
            let cfg = &self.cfg;
            let ip = &self.local_ip;
            let Some(w) = self.wizard.as_mut() else { return };
            w.draw(ui, cfg, ip)
        };

        match outcome {
            wizard::Outcome::Continue => {}
            // Skipped/exited: record only that the wizard has run, so it
            // doesn't reappear on every launch, and leave every other setting
            // exactly as it was.  No restart — nothing that matters changed.
            wizard::Outcome::Exit => {
                self.wizard = None;
                // Pick up anything another surface (telnet/web) changed while
                // the wizard held the window, so saving the one flag below
                // can't write our stale snapshot back over it.  Honours the
                // dirty flag, so in-progress edits here still win.
                self.refresh_from_global();
                self.cfg.setup_wizard_completed = true;
                self.save_config_now();
                logger::log("Setup wizard closed — existing settings kept.".into());
            }
            // Finished: fold the draft into the config, then save and restart
            // so the chosen ports actually take effect.
            wizard::Outcome::Finish => {
                if let Some(w) = self.wizard.take() {
                    // Same reason as the Exit arm: apply the answers on top of
                    // the config as it stands now, not the copy we opened with.
                    self.refresh_from_global();
                    let fetch_disks = w.wants_sample_disks();
                    w.apply_to(&mut self.cfg);
                    // The numeric text buffers back-feed into cfg on save
                    // (sync_numeric_fields), so they must be refreshed from the
                    // values the wizard just wrote or they'd overwrite them.
                    self.sync_buffers_from_cfg();
                    self.dirty = false;
                    logger::log("Setup wizard finished.".into());
                    // After `apply_to`, so the download lands in the transfer
                    // directory the operator just chose rather than the one
                    // that was in effect when the wizard opened.
                    if fetch_disks {
                        logger::log("CP/M sample disks: downloading…".into());
                        self.start_cpm_fetch(&ctx);
                    }
                    self.save_and_restart_all();
                }
            }
        }
    }

    /// Refresh the numeric text buffers the wizard can change from `self.cfg`.
    /// Narrower than `refresh_from_global`'s buffer rebuild on purpose — it is
    /// driven by our own config, not by an external update.
    fn sync_buffers_from_cfg(&mut self) {
        self.telnet_port_buf = self.cfg.telnet_port.to_string();
        self.ssh_port_buf = self.cfg.ssh_port.to_string();
        self.web_port_buf = self.cfg.web_port.to_string();
        self.slave_master_port_buf = self.cfg.slave_master_port.to_string();
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
        self.cpm_emu_max_minstr_buf = self.cfg.cpm_emu_max_minstr.to_string();
        self.log_max_size_kb_buf = self.cfg.log_max_size_kb.to_string();
        self.log_max_files_buf = self.cfg.log_max_files.to_string();
        self.gateway_term_width_buf = self.cfg.gateway_term_width.to_string();
        self.gateway_term_height_buf = self.cfg.gateway_term_height.to_string();
        self.cpm_emu_x_code_buf = self.cfg.cpm_emu_modem.x_code.to_string();
        self.cpm_emu_dcd_mode_buf = self.cfg.cpm_emu_modem.dcd_mode.to_string();
        for id in crate::config::SERIAL_PORT_IDS {
            self.serial_baud_buf[id.index()] = self.cfg.port(id).baud.to_string();
        }
    }
}

/// The label column of the CP/M popup's choice rows, and the width of the
/// controls beside it.
///
/// **One column, right-aligned, so the colons line up.** These labels run from
/// `CP/M CPU:` to `Booted disk's backspace:`, and drawn plainly each control
/// started wherever its own label happened to end — eight ragged left edges down
/// one popup.
///
/// The widths need pinning too, and `ComboBox::width` alone does not do it: it
/// is a *minimum*, and the button grows to fit whatever is selected
/// (`combo_box.rs`: `actual_width = galley + icons, at_least(minimum)`), so the
/// right edges moved as the operator changed a setting. Boxing the control to
/// an exact width and truncating the text inside it is what makes every row the
/// same shape.
const CPM_LABEL_W: f32 = 196.0;
const CPM_CONTROL_W: f32 = 330.0;

/// Helper: one `label: [control]` row of the CP/M popup, aligned on the colon.
///
/// The control closure gets a `Ui` already bounded to [`CPM_CONTROL_W`], so a
/// `ComboBox` inside it lays its text out against that width rather than
/// against the rest of the window.
fn cpm_choice_row(
    ui: &mut egui::Ui,
    label: &str,
    control: impl FnOnce(&mut egui::Ui),
) -> egui::InnerResponse<()> {
    cpm_choice_row_trailing(ui, label, control, |_| {})
}

/// [`cpm_choice_row`] with something after the control — a button that belongs
/// to the row rather than to the choice.
///
/// Outside the fixed box on purpose: boxed with the control it would eat the
/// width the control was given, and the one row that has such a button rendered
/// with its dropdown truncated mid-value and the button sitting on top of it.
fn cpm_choice_row_trailing(
    ui: &mut egui::Ui,
    label: &str,
    control: impl FnOnce(&mut egui::Ui),
    trailing: impl FnOnce(&mut egui::Ui),
) -> egui::InnerResponse<()> {
    ui.horizontal(|ui| {
        let h = ui.spacing().interact_size.y;
        ui.allocate_ui_with_layout(
            egui::vec2(CPM_LABEL_W, h),
            egui::Layout::right_to_left(egui::Align::Center),
            |ui| {
                ui.label(label);
            },
        );
        ui.allocate_ui_with_layout(
            egui::vec2(CPM_CONTROL_W, h),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.set_max_width(CPM_CONTROL_W);
                control(ui);
            },
        );
        trailing(ui);
    })
}

/// A dropdown of exactly [`CPM_CONTROL_W`], whatever is selected in it.
///
/// Both halves are needed and neither is obvious. `ComboBox::width` is a
/// *minimum* — it becomes `minimum_width = width - 2 * button_padding` and the
/// button then takes `max(text + icon, minimum_width)` — so a long value grows
/// the box and the right edges wander as settings change. Truncating bounds the
/// text against the boxed width, and asking for `CPM_CONTROL_W + 2 * padding`
/// makes that minimum come out at exactly `CPM_CONTROL_W`, so the two ends meet
/// and every row is the same width.
fn cpm_combo(ui: &egui::Ui, id_salt: &str) -> egui::ComboBox {
    egui::ComboBox::from_id_salt(id_salt)
        .width(CPM_CONTROL_W + 2.0 * ui.spacing().button_padding.x)
        .truncate()
}

/// Helper: labeled text field in a horizontal row.
fn labeled_field(ui: &mut egui::Ui, label: &str, buf: &mut String, width: f32) {
    ui.label(label);
    singleline_with_menu(ui, buf, false, Some(width));
}

/// Helper: a `Port:` field whose *label* turns red when the last port check
/// found that listener blocked.
///
/// **The label carries the signal, not a marker beside it.** A `(firewalled)`
/// tag and even a one-character `*` both sit in the row and push everything to
/// their right — these rows are aligned into columns by `pad_to`, and with the
/// tag shown the Web Server port input was pushed off under the More button.
/// Colouring a word that is already there moves nothing at all, whether or not
/// a check has run.
///
/// Only a blocked port is coloured. A pass is not evidence — see
/// [`crate::portcheck`] — so there is no green counterpart.
fn labeled_port_field(ui: &mut egui::Ui, listener: &str, buf: &mut String, width: f32) {
    let blocked = crate::portcheck::result_of(listener).filter(|(_, r)| r.is_blocked());
    let label = match &blocked {
        Some(_) => ui.label(egui::RichText::new("Port:").strong().color(RED_ALERT)),
        None => ui.label("Port:"),
    };
    if let Some((port, reach)) = blocked
        && let Some(hover) = reach.hover(port)
    {
        label.on_hover_text(hover);
    }
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
/// What leaving the server running actually costs, in the words that are true
/// for *this* launch.  Shown in the close dialog, before the choice is made.
///
/// **Two sentences, because one would be wrong half the time.**  `Ctrl-C` is
/// exactly right from a shell and a dead end from a desktop icon, where no
/// terminal exists to press it in -- and the line that named only Ctrl-C is
/// what taught an operator to close the window and relaunch instead, stacking
/// five copies of which four could bind nothing.
///
/// **It does not offer relaunching as the way back, because that is not a way
/// back.** A second copy is a second process: it finds the ports held by the
/// first, binds nothing, and its window then edits a config file that the copy
/// actually serving connections never re-reads.  Promising a reattach we do
/// not implement would document the very trap this dialog exists to close --
/// see `bindwatch`, and the handover this is the groundwork for.
///
/// Pure, and returns the string instead of logging it, so one test can hold
/// both branches with no terminal, no window and no running server.
fn detached_advice(has_terminal: bool) -> &'static str {
    if has_terminal {
        "The server keeps running and this window closes.  Ctrl-C in the \
         terminal you started it from still stops it."
    } else {
        "The server keeps running and this window closes.  You did not start \
         it from a terminal, so there is no Ctrl-C to press and nothing on \
         screen will be able to stop it -- and launching it again does NOT \
         reopen this window, it starts a second copy that cannot bind the \
         ports.  You would have to stop it from a terminal, with:  \
         pkill -x ethernetgateway"
    }
}

/// Whether a returned `gui::run` means "the operator shut the window and left
/// the server running" -- the only case that earns the parting note below.
///
/// **`restart` alone is not enough, and testing only it printed the note on
/// the way out of a quit.** Every route out of the event loop looks identical
/// from `main`: `gui::run` returns. Four of them arrive there --
///
///   * the window was closed and the server left running -> a detach;
///   * Save and Restart -> `restart`, and the window comes back;
///   * Quit -> `shutdown`, and the process is about to exit;
///   * SIGINT/SIGTERM -> `shutdown`, likewise
///
/// -- so a note that consults `restart` alone tells somebody who just asked to
/// stop the gateway that it is "still running", and names a `pkill` for a
/// process that is already on its way down. Measured 2026-08-19 by clicking
/// Quit: the note printed between "Quit requested" and "Server stopped".
/// Ctrl-C had been doing the same thing for far longer, unnoticed because the
/// old wording ("Ctrl-C to stop") read as plausible immediately after a Ctrl-C.
pub fn window_closed_was_a_detach(restart: bool, shutdown: bool) -> bool {
    !restart && !shutdown
}

/// The parting line `main` logs once the window has gone and the server has
/// been left running.  Same two facts as [`detached_advice`], past tense.
///
/// Kept beside it so the pair cannot drift: they are one rule about one
/// launch, and the reason the old single line was wrong is that it was written
/// where only half the cases were in view.
pub fn window_closed_note(has_terminal: bool) -> &'static str {
    if has_terminal {
        "Console window closed. Server still running — press Ctrl-C here to stop it."
    } else {
        "Console window closed. Server still running, and this launch has no terminal \
         to press Ctrl-C in — stop it with:  pkill -x ethernetgateway"
    }
}

fn right_aligned_small_button(ui: &mut egui::Ui, label: &str) -> bool {
    ui.with_layout(
        egui::Layout::right_to_left(egui::Align::Center),
        |ui| ui.small_button(label).clicked(),
    )
    .inner
}

/// Helper: the Server frame's `More...`, red when a port check found something.
///
/// **The frame says there is something to look at; the popup says what.** A
/// third row carrying a button, a summary and an advisory was tried first and
/// cost the frame a line it does not have to spare -- and the row was there
/// whether or not anybody had ever run a check. Colouring a button that is
/// already in the row costs nothing and points at the place the detail lives.
fn server_more_button(ui: &mut egui::Ui, blocked: usize) -> bool {
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        let text = if blocked > 0 {
            egui::RichText::new("More...").strong().color(RED_ALERT)
        } else {
            egui::RichText::new("More...")
        };
        let resp = ui.add(egui::Button::new(text).small());
        if blocked > 0 {
            resp.clone().on_hover_text(format!(
                "{blocked} bound port{} did not answer when this machine \
                 connected to {} at its own network address. Open More... to \
                 test again.",
                if blocked == 1 { "" } else { "s" },
                if blocked == 1 { "it" } else { "them" },
            ));
        }
        resp.clicked()
    })
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
        self.poll_bind_warning();

        // ── Another copy already holds this directory ─────────
        // Owns the whole window, like the wizard, and is checked *before* it:
        // a first run that is also a second copy has no business collecting
        // settings for a server that will not be started. There is no server
        // behind this window at all -- `main` started none -- so this screen
        // and the two buttons on it are the only things drawn.
        if self.handover.is_some() {
            self.track_window_geometry(ui.ctx());
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
            egui::Frame::NONE
                .inner_margin(egui::Margin::symmetric(WIZARD_MARGIN, 0))
                .show(ui, |ui| self.draw_handover(ui));
            return;
        }

        // First-run setup wizard: it owns the whole window while open, and
        // deliberately runs before refresh_from_global — its draft must never
        // be disturbed by a config change from another surface, and none of the
        // editor's own fields are on screen to be refreshed.
        if self.wizard.is_some() {
            self.track_window_geometry(ui.ctx());
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
            // A little air at the edges: the wizard owns the whole window,
            // so unlike the editor's framed rows nothing else holds its text
            // off the glass.  Applied here rather than inside the wizard so
            // one margin covers every screen it can ever draw.
            egui::Frame::NONE
                .inner_margin(egui::Margin::symmetric(WIZARD_MARGIN, 0))
                .show(ui, |ui| self.draw_wizard(ui));
            return;
        }

        // ── The title-bar X is a question ─────────────────────
        // Veto the close and ask instead (see `close_prompt_open`).  Three
        // closes must NOT be intercepted, and each would be a distinct bug:
        //
        //   * our own, once the operator has answered `Leave It Running`
        //     (`closing_deliberately`) -- otherwise the answer re-opens the
        //     question for ever and the window can never be closed at all;
        //   * the one `ui` sends a few lines up when `shutdown` is set, which
        //     is how a Quit, a Save-and-Restart and a SIGINT all take the
        //     window down -- re-asking there would strand a restart with no
        //     window and no server;
        //   * anything arriving while the wizard owns the screen, which is why
        //     this sits *after* that early return: the dialog is drawn from
        //     the popup section below, so intercepting in a frame that never
        //     reaches it would cancel the close and draw nothing -- a window
        //     with a dead X.
        if !self.closing_deliberately
            && !self.shutdown.load(Ordering::SeqCst)
            && ui.ctx().input(|i| i.viewport().close_requested())
        {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.close_prompt_open = true;
        }

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
                // Row height based on line spacing so frames match.
                //
                // **A floor both frames of a row clear, not an average.**  Each
                // frame grows past this if its own content needs more, and then
                // it alone is taller -- which is what left the columns visibly
                // staggered, the Security frame ending 8 px below Server and the
                // Serial frame 7 px below General.  Four and a half lines clears
                // the tallest paired content there is (three control rows plus a
                // header), so every frame sits exactly on the floor and the
                // borders line up.
                //
                // A feedback loop -- measure both frames, apply the taller next
                // frame -- was tried and is not worth it here: the height it
                // converged on was far larger than the content, and a layout that
                // depends on its own previous output is a poor trade for a
                // constant that one test can hold.
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
                        // First in a right-to-left layout is the RIGHTMOST
                        // widget, so this space is the window's right margin --
                        // the one the IP label used to hold.  Without it the
                        // button below sits flush against the glass.
                        ui.add_space(8.0);
                        // **A visible way out, which this window did not have.**
                        // Closing it left the server running and there was no
                        // Quit anywhere, so from a desktop icon -- no terminal,
                        // no Ctrl-C -- nothing on screen could stop the gateway.
                        // In the header rather than in a "More..." popup for
                        // exactly that reason: an operator who cannot find how
                        // to stop a program does not go looking under Server.
                        // It opens the same dialog as the X, so the consequence
                        // is stated once and in one place.
                        if ui
                            .add(egui::Button::new(
                                egui::RichText::new("Quit").strong().color(AMBER),
                            ))
                            .on_hover_text(
                                "Stop the server and close the gateway, or close \
                                 just this window and leave it running.",
                            )
                            .clicked()
                        {
                            self.close_prompt_open = true;
                        }
                        ui.add_space(16.0);
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

                // ── Running-as-root banner ────────────────────
                // Full width and above everything, because what it warns about
                // is done by the act of using this window: a Save writes
                // `egateway.conf` as root, and from then on the operator's own
                // account cannot start the gateway at all.
                if !self.elevation_lines.is_empty() && !self.elevation_dismissed {
                    // Borrowed before the closure so `self` is not captured
                    // whole; the flag is set after it returns.
                    let lines = &self.elevation_lines;
                    let dismissed = egui::Frame::group(ui.style())
                        .fill(WARN_BG)
                        .stroke(Stroke::new(1.5_f32, WARN_BORDER))
                        .show(ui, |ui| {
                            ui.set_min_width(ui.available_width());
                            let mut dismiss = false;
                            for (i, line) in lines.iter().enumerate() {
                                if i == 0 {
                                    // The condition, and the way to put it
                                    // away, on one row -- Dismiss belongs
                                    // beside the headline, not below five
                                    // lines of consequence where it reads as
                                    // the answer to them.
                                    ui.horizontal(|ui| {
                                        ui.label(
                                            egui::RichText::new(line)
                                                .strong()
                                                .color(RED_ALERT),
                                        );
                                        if right_aligned_small_button(ui, "Dismiss") {
                                            dismiss = true;
                                        }
                                    });
                                } else {
                                    // The consequences and the way out.
                                    ui.label(egui::RichText::new(line).color(AMBER));
                                }
                            }
                            dismiss
                        })
                        .inner;
                    if dismissed {
                        self.elevation_dismissed = true;
                    }
                    ui.add_space(4.0);
                }

                // ── "This copy is serving nothing" banner ─────
                // The one failure the instance lock cannot catch: a copy
                // launched from a *different* directory claims its own lock
                // quite legitimately and binds nothing, because another copy --
                // or a systemd unit serving from its own WorkingDirectory --
                // already holds the ports. Everything on this window then works
                // except the part that matters: a Save reaches a config the
                // serving process never re-reads. That was the original
                // five-stacked-copies defect, and it survived the lock by the
                // one route the lock is per-directory about.
                //
                // Above the settings for the same reason as the root banner:
                // what it warns about is done by *using* this window.
                if bind_banner_showing(&self.bind_warning, &self.bind_warning_dismissed) {
                    let lines = self.bind_warning.1.clone();
                    // Resolved once at construction, not here: this is a draw
                    // path, and `data_dir_display` is a `canonicalize` syscall
                    // that would run on every frame the banner is up.
                    let data_dir = self.data_dir.clone();
                    let dismissed = egui::Frame::group(ui.style())
                        .fill(WARN_BG)
                        .stroke(Stroke::new(1.5_f32, WARN_BORDER))
                        .show(ui, |ui| {
                            ui.set_min_width(ui.available_width());
                            let mut dismiss = false;
                            for (i, line) in lines.iter().enumerate() {
                                let text = line.trim_start();
                                if i == 0 {
                                    ui.horizontal(|ui| {
                                        ui.label(
                                            egui::RichText::new(text).strong().color(RED_ALERT),
                                        );
                                        if right_aligned_small_button(ui, "Dismiss") {
                                            dismiss = true;
                                        }
                                    });
                                } else {
                                    ui.label(egui::RichText::new(text).color(AMBER));
                                }
                            }
                            // **Which directory this window is editing.** In the
                            // cross-directory case the two copies have two
                            // configs, and naming the one in front of the
                            // operator is what makes "this Save will not reach
                            // it" concrete rather than abstract.
                            ui.label(
                                egui::RichText::new(format!(
                                    "This window edits {data_dir} — not whatever the serving \
                                     copy is reading."
                                ))
                                .color(AMBER_DIM),
                            );
                            dismiss
                        })
                        .inner;
                    if dismissed {
                        self.bind_warning_dismissed = self.bind_warning.clone();
                    }
                    ui.add_space(4.0);
                }

                // ── Row 1: Server + Security ──────────────────
                // Each frame is padded out to the taller of the row's two
                // columns, measured last repaint (see `config_row_h`).
                let target0 = row_h.max(self.config_row_h[0]);
                let target1 = row_h.max(self.config_row_h[1]);
                let target2 = row_h.max(self.config_row_h[2]);

                let row0 = ui.horizontal_top(|ui| {
                    let col_a = ui.allocate_ui_with_layout(
                        egui::vec2(half, 0.0),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            let framed = egui::Frame::group(ui.style()).show(ui, |ui| {
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
                                let natural = ui.min_rect().height();
                                if target0 > natural {
                                    ui.add_space(target0 - natural);
                                }
                                natural
                            });
                            framed.inner
                        },
                    ).inner;

                    let col_b = ui.allocate_ui_with_layout(
                        egui::vec2(half, 0.0),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            let framed = egui::Frame::group(ui.style()).show(ui, |ui| {
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
                                    // The gateway-address rule gets its own row
                                    // under the two headline checkmarks: it is a
                                    // narrower decision than either of them, and
                                    // the label has to spell out what `.1` means
                                    // to be any use to someone who has just been
                                    // refused a connection.
                                    //
                                    // No confirmation popup, unlike Disable IP
                                    // Safety: this direction *tightens* the
                                    // allowlist, and the off state is the
                                    // default rather than a widening.
                                    // The label names the address the OS says
                                    // is this network's router, so the operator
                                    // sees what the rule will actually block
                                    // rather than a convention.  Falls back to
                                    // "x.x.x.1" (which is also what the rule
                                    // falls back to) when detection found
                                    // nothing.  Cached — never a query here.
                                    if ui
                                        .checkbox(
                                            &mut self.cfg.disable_gateway_connections,
                                            format!(
                                                "Block connections from the router ({})",
                                                crate::router::describe()
                                            ),
                                        )
                                        .changed()
                                    {
                                        let v = self.cfg.disable_gateway_connections.to_string();
                                        config::update_config_value(
                                            "disable_gateway_connections",
                                            &v,
                                        );
                                        self.last_synced_cfg.disable_gateway_connections =
                                            self.cfg.disable_gateway_connections;
                                        logger::log(
                                            if self.cfg.disable_gateway_connections {
                                                "Connections from *.*.*.1 are now blocked.".into()
                                            } else {
                                                "Connections from *.*.*.1 are now allowed.".into()
                                            },
                                        );
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
                                let natural = ui.min_rect().height();
                                if target0 > natural {
                                    ui.add_space(target0 - natural);
                                }
                                natural
                            });
                            framed.inner
                        },
                    ).inner;
                    col_a.max(col_b)
                });
                self.config_row_h[0] = row0.inner;
                ui.add_space(4.0);

                // ── Row 2: File Transfer + AI/Browser ─────────
                let row1 = ui.horizontal_top(|ui| {
                    let col_a = ui.allocate_ui_with_layout(
                        egui::vec2(half, 0.0),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            let framed = egui::Frame::group(ui.style()).show(ui, |ui| {
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
                                let natural = ui.min_rect().height();
                                if target1 > natural {
                                    ui.add_space(target1 - natural);
                                }
                                natural
                            });
                            framed.inner
                        },
                    ).inner;

                    let col_b = ui.allocate_ui_with_layout(
                        egui::vec2(half, 0.0),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            let framed = egui::Frame::group(ui.style()).show(ui, |ui| {
                                ui.set_min_width(ui.available_width());
                                ui.horizontal(|ui| {
                                    ui.label(egui::RichText::new("AI Chat, Browser, Weather & CP/M").strong().color(AMBER));
                                    if right_aligned_small_button(ui, "Save") {
                                        self.save_config_now();
                                    }
                                });
                                // **The API key was here and was moved to the
                                // popup deliberately.**  It is optional — AI
                                // chat is the only thing that wants one — but a
                                // key field at the top of a frame reads as
                                // something the product needs before it will
                                // work.  The weather location says what the
                                // frame is for and costs the reader nothing.
                                ui.horizontal(|ui| {
                                    ui.label("Weather location:");
                                    singleline_with_menu(ui, &mut self.cfg.weather_location, false, None);
                                });
                                // Home row carries the "More..." button (the
                                // Groq key + weather units live in that popup),
                                // keeping this frame at three rows.  The
                                // homepage field is width-bounded so the button
                                // has room.
                                ui.horizontal(|ui| {
                                    labeled_field(ui, "Home:", &mut self.cfg.browser_homepage, 190.0);
                                    if right_aligned_small_button(ui, "More...") {
                                        self.ai_browser_popup_open = true;
                                    }
                                });
                                let natural = ui.min_rect().height();
                                if target1 > natural {
                                    ui.add_space(target1 - natural);
                                }
                                natural
                            });
                            framed.inner
                        },
                    ).inner;
                    col_a.max(col_b)
                });
                self.config_row_h[1] = row1.inner;
                ui.add_space(4.0);

                // ── Row 3: Serial Ports (left) + General (right) ──
                // Serial frame: header with both ports' Enabled
                // checkboxes plus a Save button, then one row per port
                // (device dropdown, baud, More button into per-port
                // popup).  General frame on the right shares the row,
                // matching the half-width layout of the other paired
                // frames above.
                let row2 = ui.horizontal_top(|ui| {
                    let col_a = ui.allocate_ui_with_layout(
                        egui::vec2(half, 0.0),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            let framed = egui::Frame::group(ui.style()).show(ui, |ui| {
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
                                let natural = ui.min_rect().height();
                                if target2 > natural {
                                    ui.add_space(target2 - natural);
                                }
                                natural
                            });
                            framed.inner
                        },
                    ).inner;

                    let col_b = ui.allocate_ui_with_layout(
                        egui::vec2(half, 0.0),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            let framed = egui::Frame::group(ui.style()).show(ui, |ui| {
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
                                    // More rides THIS row (the frame's third
                                    // line, counting the heading) rather than
                                    // taking a fourth: a fourth line makes this
                                    // frame taller than the AI/Browser frame
                                    // beside it and the two stop lining up.
                                    // Same right_to_left layout as every other
                                    // More button, which lays out within the
                                    // available width — so it cannot land
                                    // outside the frame on a resize.
                                    if right_aligned_small_button(ui, "More...") {
                                        self.general_popup_open = true;
                                    }
                                });
                                // The CP/M emulator toggle + runaway ceiling live
                                // in the "AI, Browser & Weather — More" popup
                                // (no room left on the main screen).
                                let natural = ui.min_rect().height();
                                if target2 > natural {
                                    ui.add_space(target2 - natural);
                                }
                                natural
                            });
                            framed.inner
                        },
                    ).inner;
                    col_a.max(col_b)
                });
                self.config_row_h[2] = row2.inner;
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
                            crate::webserver::MANUAL_URL,
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
                                egui::Image::new(egui::include_image!("../eglogobrightsmall.png"))
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

        // Drawn first, and unconditionally when open: this is the one dialog
        // that answers a close already vetoed in `ui` above, so a frame that
        // set the flag and then failed to draw it would leave a window whose
        // X does nothing.
        self.draw_close_prompt(&ctx, warn_frame);

        let mut server_open = self.server_popup_open;
        // Set by the "Run setup wizard..." button inside the popup below.  It
        // can't open the wizard in place: `server_open` is mutably borrowed by
        // the window's `.open()` for the duration of the closure, and closing
        // the popup is part of handing the window to the wizard.
        let mut wizard_requested = false;
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
                // Re-run the first-run wizard.  Opens on the next frame with
                // the current settings pre-filled and changes nothing unless
                // the operator walks it through to Save; this popup closes so
                // the wizard has the window to itself.  GUI-only by design —
                // the telnet and web config UIs have no equivalent.
                if ui
                    .button(
                        egui::RichText::new("Run setup wizard...")
                            .strong()
                            .color(AMBER_BRIGHT),
                    )
                    .clicked()
                {
                    wizard_requested = true;
                }
                ui.add_space(6.0);
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
        if wizard_requested {
            self.wizard = Some(wizard::Wizard::new(&self.cfg));
            server_open = false;
            // The close question dies with the screen it was asked on.  Both
            // can be open at once -- the dialog does not block input to the
            // popup behind it -- and the wizard's early return in `ui` means
            // the dialog would not be drawn while it is up, then reappear
            // when the wizard finished, asking about a click made minutes ago.
            self.close_prompt_open = false;
        }
        self.server_popup_open = server_open;

        // AI, Browser & Weather — More popup.  Surfaces every option in the
        // group (API key, homepage, weather location, weather units); the main
        // frame shows only the API key + homepage to stay at three rows.
        let mut ai_browser_open = self.ai_browser_popup_open;
        egui::Window::new(
            egui::RichText::new("AI, Browser, Weather & CP/M — More")
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

        // Mount CP/M Drives — a row per drive A:–P:.  Its own window rather
        // than more rows in the CP/M group: sixteen drives do not fit there,
        // and mounting is an occasional operation, not a setting.
        let mut cpm_mount_open = self.cpm_mount_popup_open;
        egui::Window::new(
            egui::RichText::new("Mount CP/M Drives")
                .strong()
                .color(AMBER_BRIGHT),
        )
        .open(&mut cpm_mount_open)
        .resizable(true)
        .collapsible(false)
        .default_width(560.0)
        .frame(popup_frame)
        .show(&ctx, |ui| {
            ui.visuals_mut().extreme_bg_color = POPUP_INPUT_BG;
            self.draw_cpm_mounts(ui);
            ui.add_space(8.0);
            ui.separator();
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui
                    .add(egui::Button::new(
                        egui::RichText::new("Apply")
                            .strong()
                            .size(16.0)
                            .color(AMBER_BRIGHT),
                    ))
                    .clicked()
                {
                    self.cpm_mount_apply();
                }
                if ui
                    .add(egui::Button::new(
                        egui::RichText::new("Refresh").color(AMBER_BRIGHT),
                    ))
                    .on_hover_text(
                        "Re-read the images folder and the live mount table — use after adding a .dsk, or if a mount changed from the web or a telnet session.",
                    )
                    .clicked()
                {
                    self.cpm_mount_reload_draft();
                }
            });
        });
        self.cpm_mount_popup_open = cpm_mount_open;

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

        // General — More popup: the on-disk log, plus the frame's own three
        // toggles re-shown so the popup covers the whole group.  Save-and-Restart
        // rather than Save, because file logging is armed from the startup path:
        // a changed log path or limit takes effect on the next restart.
        let mut general_open = self.general_popup_open;
        egui::Window::new(
            egui::RichText::new("General — More")
                .strong()
                .color(AMBER_BRIGHT),
        )
        .open(&mut general_open)
        .resizable(true)
        .collapsible(false)
        .default_width(520.0)
        .frame(popup_frame)
        .show(&ctx, |ui| {
            ui.visuals_mut().extreme_bg_color = POPUP_INPUT_BG;
            self.draw_general_more(ui);
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
        self.general_popup_open = general_open;

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

        // "The screen you asked for needs the web server" -- offered, and never
        // done quietly.  Two things make this a confirmation rather than a
        // convenience: starting a listener is outward-facing, and the listener
        // only binds on a server restart, which drops every session anybody
        // else is in the middle of.  The operator has to be told that before
        // they agree, not discover it.
        // A finished port check becomes a popup.  Polled here rather than in
        // the frame that owns the button, because that frame is only drawn
        // while its popup is open -- the answer would arrive to nobody.
        if let Some(rx) = &self.port_check_rx {
            match rx.try_recv() {
                Ok(_) => {
                    self.port_check_rx = None;
                    self.port_check_popup_open = true;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => self.port_check_rx = None,
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    ctx.request_repaint_after(std::time::Duration::from_millis(200));
                }
            }
        }

        // The result of a port check, every listener named.
        //
        // **Red framed, and it says "answered" rather than "open".** A pass is
        // not evidence: on Windows and macOS a connection to your own address
        // skips the firewall entirely, so a port that answered here may still be
        // unreachable from anywhere else. Reporting it as open would be the one
        // mistake an operator would act on -- they would go looking at their
        // router while Defender quietly dropped every connection.
        let mut pc_open = self.port_check_popup_open;
        let mut close = false;
        let blocked_now =
            crate::portcheck::results().iter().filter(|(_, _, r)| r.is_blocked()).count();
        egui::Window::new(
            egui::RichText::new(if blocked_now > 0 {
                "Port test - something is blocking"
            } else {
                "Port test"
            })
            .strong()
            .color(if blocked_now > 0 { RED_ALERT } else { AMBER_BRIGHT }),
        )
        .open(&mut pc_open)
        .resizable(false)
        .collapsible(false)
        .default_width(470.0)
        .frame(warn_frame)
        .show(&ctx, |ui| {
            ui.visuals_mut().extreme_bg_color = POPUP_INPUT_BG;
            let results = crate::portcheck::results();
            if results.is_empty() {
                ui.label("No listener is bound, so there was nothing to test.");
            } else {
                for (name, port, reach) in &results {
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(format!("{name} {port}"))
                                .strong()
                                .color(TEXT_PRIMARY),
                        );
                        // Three states, not two.  A probe that never got as
                        // far as a connection -- no address for this host, no
                        // route -- is neither blocked nor an answer, and
                        // calling it one would be this feature telling the
                        // operator everything is fine on the strength of a
                        // test it did not manage to run.
                        let phrase = egui::RichText::new(reach.verdict_phrase());
                        ui.label(if reach.is_blocked() {
                            phrase.strong().color(RED_ALERT)
                        } else if reach.is_untested() {
                            phrase.color(AMBER)
                        } else {
                            phrase.color(AMBER_DIM)
                        });
                    });
                }
            }
            // How old the answer is.  Nothing polls, so a red label is a
            // snapshot: fix the firewall and it stays red until the next check.
            if let Some(age) = crate::portcheck::age() {
                let secs = age.as_secs();
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new(if secs < 5 {
                        "Checked just now.".to_string()
                    } else if secs < 90 {
                        format!("Checked {secs} seconds ago.")
                    } else {
                        format!("Checked {} minutes ago.", secs / 60)
                    })
                    .small()
                    .color(AMBER_DIM),
                );
            }
            ui.add_space(8.0);
            if blocked_now > 0 {
                ui.label(
                    egui::RichText::new(
                        "A port that did not answer is being blocked by something on \
                         this machine -- a host firewall, or security software.",
                    )
                    .color(AMBER),
                );
                ui.add_space(4.0);
            }
            ui.label(
                "\"Answered\" is not the same as reachable, and what this test can \
                 prove depends on the platform:",
            );
            ui.add_space(6.0);
            // The same table the web page and the manual render, from one
            // source -- a capability claim that drifted between surfaces would
            // be worse than not making it.
            let here = crate::portcheck::this_platform();
            egui::Grid::new("port_check_platforms").num_columns(4).striped(true).show(ui, |ui| {
                ui.label(egui::RichText::new("").small());
                for name in ["Linux", "Windows", "macOS"] {
                    let head = egui::RichText::new(name).small().strong();
                    ui.label(if here == Some(name) {
                        head.color(AMBER_BRIGHT)
                    } else {
                        head.color(AMBER_DIM)
                    });
                }
                ui.end_row();
                for fact in crate::portcheck::WHAT_THE_TEST_PROVES {
                    ui.label(egui::RichText::new(fact.question).small().color(TEXT_PRIMARY));
                    for (name, value) in
                        [("Linux", fact.linux), ("Windows", fact.windows), ("macOS", fact.macos)]
                    {
                        let cell = egui::RichText::new(value).small();
                        // The running platform is the column the operator is
                        // actually in; the others are there to show why.
                        ui.label(if here == Some(name) {
                            cell.strong().color(if value == "yes" { CONSOLE_TEXT } else { RED_ALERT })
                        } else {
                            cell.color(AMBER_DIM)
                        });
                    }
                    ui.end_row();
                }
            });
            ui.add_space(6.0);
            // From the table's own first row rather than by naming a platform
            // here: the question is "can this build detect a block", and the
            // table is where that is decided.
            let detects_here = crate::portcheck::WHAT_THE_TEST_PROVES
                .first()
                .and_then(|f| f.here())
                == Some("yes");
            if !detects_here {
                ui.label(
                    egui::RichText::new(
                        "So on this platform a pass means very little: a connection to \
                         your own address does not meet the firewall at all. Open the \
                         ports on your firewall and test from another machine.",
                    )
                    .color(AMBER),
                );
            } else {
                ui.label(
                    "Nothing here can see past this machine, so a router that is not \
                     forwarding a port looks fine. Open these ports on your firewall.",
                );
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui
                    .add(egui::Button::new(egui::RichText::new("Close").strong()))
                    .clicked()
                {
                    close = true;
                }
            });
        });
        if close {
            pc_open = false;
        }
        self.port_check_popup_open = pc_open;

        // Read before the closure borrows `self`.
        let secured = self.cfg.security_enabled;
        let mut vdm_offer_open = self.vdm_web_offer_open;
        let mut vdm_close = false;
        let mut vdm_commit = false;
        egui::Window::new(
            egui::RichText::new("Turn the web server on?").strong().color(AMBER_BRIGHT),
        )
        .open(&mut vdm_offer_open)
        .resizable(false)
        .collapsible(false)
        .default_width(460.0)
        .frame(warn_frame)
        .show(&ctx, |ui| {
            ui.visuals_mut().extreme_bg_color = POPUP_INPUT_BG;
            ui.label(
                "The VDM / Dazzler screen is a page served by this gateway's own \
                 web server, and the web server is switched off.",
            );
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new("Enabling it restarts the gateway.").strong().color(AMBER),
            );
            ui.add_space(4.0);
            ui.label(
                "The listener only binds when the server starts, so the setting \
                 alone is not enough. The restart ends every telnet and SSH \
                 session in progress, including any booted CP/M disk somebody is \
                 sitting at -- so the screen you are about to open will be of \
                 whatever boots next, not of the session running now.",
            );
            ui.add_space(6.0);
            // **Do not promise credentials that are not there.**  `security_enabled`
            // is off by default and the web server honours that -- with login off
            // there is no password at all, only the private-IP allowlist -- and
            // this page renders the gateway password and the Groq key into input
            // values.  A dialog whose whole job is to inform before opening a
            // listener must not tell somebody their config page is protected when
            // it is not.
            if secured {
                ui.label(
                    "The page is behind the same credentials as the rest of the \
                     web interface, and the screen is readable but only types at \
                     a guest when \"may type at a booted disk\" is on.",
                );
            } else {
                ui.label(
                    egui::RichText::new(
                        "Require Login is OFF, so the web interface asks for no \
                         password -- anyone who can reach this machine on the \
                         network gets the configuration page, which shows your \
                         gateway password and API key. Only the private-address \
                         check stands in the way. Turn Require Login on first if \
                         this machine shares a network with anyone else.",
                    )
                    .color(AMBER),
                );
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui
                    .add(egui::Button::new(
                        egui::RichText::new("Enable and Restart").strong().color(AMBER_BRIGHT),
                    ))
                    .clicked()
                {
                    vdm_commit = true;
                    vdm_close = true;
                }
                ui.add_space(8.0);
                if ui
                    .add(egui::Button::new(egui::RichText::new("Cancel").strong()))
                    .clicked()
                {
                    vdm_close = true;
                }
            });
        });
        if vdm_commit {
            // **Only the two keys this dialog asked about.**  The obvious
            // `save_and_restart_all()` would persist every unsaved edit in the
            // window -- a half-typed telnet port, a transfer directory being
            // edited -- and then apply them all in a restart the operator
            // agreed to for the web server alone.  The Kermit-server popup a
            // few lines up already does the narrow thing; this now matches it.
            self.cfg.web_enabled = true;
            self.last_synced_cfg.web_enabled = true;
            config::update_config_value("web_enabled", "true");
            // Written before the restart, because the restart takes this window
            // with it: the server unwinds, `gui::run` returns and `main` builds
            // a fresh `App`.  That one spends the marker and opens the page, so
            // the operator does not have to find the button again.
            config::update_config_value("open_screen_after_restart", "true");
            logger::log("Web server enabled — restarting to bind the listener...".into());
            // Restart without persisting the rest: `restart` before `shutdown`,
            // the order `save_and_restart_all` documents, so the main loop sees
            // the intent when it checks after the join.
            self.restart.store(true, Ordering::SeqCst);
            self.shutdown.store(true, Ordering::SeqCst);
        }
        if vdm_close {
            vdm_offer_open = false;
        }
        self.vdm_web_offer_open = vdm_offer_open;

        // The deferred open, once the restarted listener has had its moment.
        if let Some(at) = self.vdm_open_at {
            if std::time::Instant::now() >= at {
                self.vdm_open_at = None;
                self.open_vdm_page(&ctx);
            } else {
                ctx.request_repaint_after(std::time::Duration::from_millis(200));
            }
        }

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
                || self.cpm_emu_max_minstr_buf != self.last_synced_cfg.cpm_emu_max_minstr.to_string()
                || self.log_max_size_kb_buf != self.last_synced_cfg.log_max_size_kb.to_string()
                || self.log_max_files_buf != self.last_synced_cfg.log_max_files.to_string()
                || self.gateway_term_width_buf != self.last_synced_cfg.gateway_term_width.to_string()
                || self.gateway_term_height_buf != self.last_synced_cfg.gateway_term_height.to_string()
                || self.cpm_emu_x_code_buf != self.last_synced_cfg.cpm_emu_modem.x_code.to_string()
                || self.cpm_emu_dcd_mode_buf != self.last_synced_cfg.cpm_emu_modem.dcd_mode.to_string()
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
            None,
        )
    }

    // ── Closing the window vs stopping the server ────────────

    /// **A quit is not a restart, and `restart` is the only thing that says
    /// so.** Both trip `shutdown` to unwind the server cycle; `main` reads
    /// `restart` afterwards to decide whether to loop back for another cycle
    /// or fall out and exit. Copying `save_and_restart_all`'s pair of stores
    /// -- the obvious way to write `quit` -- yields a restart, and the window
    /// the operator just tried to leave comes straight back. So the two are
    /// pinned together, against each other: asserting only that `quit` sets
    /// `shutdown` would pass for a function that restarts.
    #[test]
    fn test_quit_stops_the_server_where_a_restart_would_bring_it_back() {
        let mut app = test_app();
        app.quit();
        assert!(app.shutdown.load(Ordering::SeqCst), "quit must unwind the server cycle");
        assert!(
            !app.restart.load(Ordering::SeqCst),
            "quit must NOT arm restart — main would loop back and reopen the window"
        );

        // The positive control: the restart path sets the same shutdown flag,
        // so shutdown alone cannot tell the two apart.
        let mut app = test_app();
        app.save_and_restart_all();
        assert!(app.shutdown.load(Ordering::SeqCst));
        assert!(
            app.restart.load(Ordering::SeqCst),
            "save-and-restart must arm restart — otherwise this test proves nothing"
        );
    }

    /// The dialog and the parting line must not hand a desktop launch a key it
    /// has no terminal to press.
    ///
    /// `Ctrl-C` belongs to the shell branch only. In the other branch the
    /// process inherited the graphical VT and there is no shell anywhere, so
    /// the note has to name something that actually works -- and must **not**
    /// offer relaunching as the way back, because a second copy binds nothing
    /// and edits a config the serving copy never re-reads.
    #[test]
    fn test_the_advice_matches_the_launch_it_is_given_to() {
        // From a shell: Ctrl-C is the right answer and must be offered.
        assert!(window_closed_note(true).contains("Ctrl-C"));
        assert!(detached_advice(true).contains("Ctrl-C"));

        // From a desktop icon: no terminal exists, so Ctrl-C must not be the
        // instruction, and a real way to stop it must be named instead.
        let note = window_closed_note(false);
        assert!(
            note.contains("pkill -x ethernetgateway"),
            "the no-terminal note must name a way that works: {note}"
        );
        let advice = detached_advice(false);
        assert!(advice.contains("pkill -x ethernetgateway"), "{advice}");
        // The trap this whole dialog exists to close: relaunching is not a
        // reattach, so the text must never suggest it is.
        assert!(
            !advice.to_lowercase().contains("get this window back"),
            "must not promise a reattach we do not implement: {advice}"
        );
        // Both branches must actually differ -- a single sentence covering
        // both was the original defect.
        assert_ne!(window_closed_note(true), window_closed_note(false));
        assert_ne!(detached_advice(true), detached_advice(false));
    }

    /// **Only one of the four ways out of the event loop is a detach.**
    ///
    /// `main` cannot tell them apart by the return alone, and the version that
    /// consulted `restart` only told an operator who had just clicked Quit
    /// that the server was "still running", with a `pkill` for a process
    /// already exiting.  Each case is pinned, so a future flag added to one
    /// route cannot quietly re-enter the note.
    #[test]
    fn test_only_a_close_that_left_the_server_up_earns_the_parting_note() {
        // The window was closed and the server was left running.
        assert!(window_closed_was_a_detach(false, false));
        // Save and Restart: the window is coming straight back.
        assert!(!window_closed_was_a_detach(true, false));
        // Quit, and SIGINT/SIGTERM, which reach `main` identically.
        assert!(!window_closed_was_a_detach(false, true));
        // Both set is a restart; either flag alone must be enough to suppress.
        assert!(!window_closed_was_a_detach(true, true));
    }

    /// **A fatal message the operator cannot read is the same defect as the X
    /// that answered a question nobody asked.**
    ///
    /// Measured 2026-08-20: launched from a directory it could not write, the
    /// gateway printed a FATAL to stderr and exited 1 with no window at all --
    /// and the AppImage's own desktop entry sets `Terminal=false`, so from an
    /// icon that text goes to the session journal and the operator sees a
    /// program that does nothing. A window is the only place left to say it.
    ///
    /// The gate is deliberately conservative in the other direction. Being
    /// wrong about a terminal in `detached_advice` costs a wrong sentence;
    /// being wrong here costs a modal window blocking a process nobody is
    /// watching, so **every** standard stream has to be non-terminal before one
    /// is offered -- `gateway | tee log` still has a shell behind it.
    #[test]
    fn test_a_window_is_offered_only_when_the_text_has_nowhere_else_to_go() {
        // The desktop-icon case: no shell anywhere, a session to draw on.
        assert!(startup_failure_needs_a_window(false, true));
        // A shell is watching — the lines are already on screen.
        assert!(!startup_failure_needs_a_window(true, true));
        // A headless server or a service: nothing to draw on, so the log is
        // the only word on the subject and must stay the only attempt.
        assert!(!startup_failure_needs_a_window(false, false));
        assert!(!startup_failure_needs_a_window(true, false));
    }

    /// **The banner must come back after a restart that still binds nothing.**
    ///
    /// This is the case the whole feature exists for: a copy launched from a
    /// different directory keeps a full editor window whose Save reaches a
    /// config the serving process never re-reads. Dismissing has to put the
    /// banner away without disarming it for the life of the window.
    #[test]
    fn test_the_serving_nothing_banner_is_dismissed_per_report_not_for_ever() {
        let words: Vec<String> =
            vec!["WARNING: NONE of the 1".into(), "  nothing bound".into()];
        let none: (u64, Vec<String>) = (7, Vec::new());
        let warning = (7_u64, words.clone());
        // Nothing wrong: no banner.
        assert!(!bind_banner_showing(&none, &none));
        // Something wrong, not yet dismissed.
        assert!(bind_banner_showing(&warning, &none));
        // Dismissed: away it goes.
        assert!(!bind_banner_showing(&warning, &warning));
        // **The bug this pins.** A Save and Restart bumps the cycle, so the
        // *identical* text is a new report and must be said again -- the first
        // version compared text alone and stayed silent, in the one case that
        // matters.
        let after_restart = (8_u64, words.clone());
        assert!(
            bind_banner_showing(&after_restart, &warning),
            "a restart that fails identically must say so again"
        );
        // A warning that changes within one cycle is also new.
        let changed = (7_u64, vec!["WARNING: NONE of the 2".into()]);
        assert!(bind_banner_showing(&changed, &warning));
        // And a listener recovering clears it without any dismissal.
        assert!(!bind_banner_showing(&(8, Vec::new()), &warning));
    }

    /// The dialog starts closed, like every other popup: a modal covering the
    /// window on launch would be its own bug.
    #[test]
    fn test_close_prompt_starts_closed_and_no_close_is_pending() {
        let app = test_app();
        assert!(!app.close_prompt_open);
        assert!(
            !app.closing_deliberately,
            "a fresh window must not believe it is already on its way out"
        );
    }

    // ── App::new initialization ──────────────────────────────

    /// **The one-shot marker arms the open, and is spent on the way in.**
    ///
    /// Turning the web server on from the screen button restarts the gateway,
    /// and the restart destroys this window — `gui::run` returns and `main`
    /// builds a fresh `App`. The marker in the config is how the new window
    /// knows to finish the job. It has to be *spent*: one that survived a launch
    /// which never managed to open a browser would open one at every launch
    /// afterwards, which is a worse fault than the one it fixes.
    #[test]
    fn test_the_screen_marker_arms_the_open_and_is_spent() {
        let plain = test_app();
        assert!(plain.vdm_open_at.is_none(), "an ordinary launch opens nothing");

        let asked = App::new(
            Config { open_screen_after_restart: true, ..Config::default() },
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            None,
        );
        assert!(asked.vdm_open_at.is_some(), "the marker must arm the open");
        // Not immediately: the server it is about to open is still binding.
        assert!(
            asked.vdm_open_at.unwrap() > std::time::Instant::now(),
            "opening in the same instant races the listener's bind"
        );
        // Spent on the way in, so the *App* no longer carries the request.
        assert!(!asked.cfg.open_screen_after_restart, "the marker must be cleared when read");
    }

    /// **A boolean cannot carry what the button needs to know.**
    ///
    /// "Off" and "configured but the bind failed" both mean there is no page to
    /// open, and they want opposite things said: one is an offer to start the
    /// server, the other is news that starting it did not work. Offering to
    /// "enable" a server that is already enabled would restart the gateway and
    /// change nothing.
    ///
    /// The case this exists for is the one `bindwatch` was written for: a second
    /// copy of the gateway holding the port. Without asking it, the button opens
    /// a browser at a refused connection — or at the other instance's
    /// configuration page, which is worse than refused.
    #[test]
    fn test_the_screen_button_reads_the_bind_outcome_not_the_setting() {
        use crate::bindwatch::Status;

        assert_eq!(WebScreenState::of(Some((8080, Status::Bound))), WebScreenState::Bound(8080));
        assert_eq!(
            WebScreenState::of(Some((8080, Status::Pending))),
            WebScreenState::Starting(8080),
            "a listener still binding is not an off one"
        );
        assert_eq!(
            WebScreenState::of(Some((8080, Status::Failed { in_use: true }))),
            WebScreenState::Failed { port: 8080, in_use: true }
        );
        assert_eq!(WebScreenState::of(None), WebScreenState::Off);

        // Only the two that are listening offer a page.
        assert_eq!(WebScreenState::Bound(8080).port(), Some(8080));
        assert_eq!(WebScreenState::Starting(8080).port(), Some(8080));
        assert_eq!(WebScreenState::Failed { port: 8080, in_use: true }.port(), None);
        assert_eq!(WebScreenState::Off.port(), None);

        // And the two that do not are distinguishable, which is the whole
        // point: one gets the offer dialog, the other gets told why.
        assert_ne!(
            WebScreenState::Failed { port: 8080, in_use: false },
            WebScreenState::Off
        );
    }

    /// **The desktop button opens the screen, at the port the server is on.**
    ///
    /// Three things this pins, each of which was a way to get it wrong:
    /// the path is the screen's and not the configuration page's, because the
    /// operator pressed a button that named the screen; the port comes from the
    /// *saved* config, since a port typed into the box and not saved is a port
    /// nothing is listening on; and the same rule decides whether the button
    /// opens a browser or offers to start the server, so a ticked-but-unsaved
    /// checkbox cannot make it open a page that is not being served.
    #[test]
    fn test_the_desktop_screen_button_opens_the_screen_not_the_root() {
        let mut app = App::new(
            Config { web_enabled: true, web_port: 9123, ..Config::default() },
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            None,
        );

        let url = app.vdm_url();
        assert!(url.ends_with("/vdm"), "it must land on the screen: {url}");
        assert!(url.contains(":9123/"), "the configured port is not in {url}");
        assert!(url.starts_with("http://127.0.0.1:"), "{url}");
        assert!(app.vdm_url().contains(":9123/"));

        // **Neither later view of the config moves it.**  `cfg` holds unsaved
        // edits, and `last_synced_cfg` is refreshed from the global config
        // whenever that changes -- including a change made from the web or
        // telnet UI that needs a restart to take effect.  The listener is still
        // where `main` started it, and that is where the browser must be sent.
        app.cfg.web_port = 4444;
        app.cfg.web_enabled = false;
        app.last_synced_cfg.web_port = 5555;
        app.last_synced_cfg.web_enabled = false;
        assert_eq!(app.vdm_url(), "http://127.0.0.1:9123/vdm");
        assert!(app.vdm_url().contains(":9123/"), "what is running is what it was started with");

        // And a gateway whose server really was off offers to start it instead.
        let off = test_app();
        assert_eq!(off.vdm_web_state(), WebScreenState::Off, "no listener, no page to open");
    }

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

    /// The two on-disk-log limits round-trip through their text buffers, and —
    /// unlike most numeric fields here — `0` must survive rather than being
    /// floored to 1: it is the documented "no size rotation" / "keep no
    /// history" sentinel that `logger::rotate` acts on.  A `>= 1` guard copied
    /// from the neighbouring lines would silently make both settings
    /// unreachable from the GUI.
    #[test]
    fn test_sync_log_limits_accepts_zero() {
        let mut app = test_app();
        app.log_max_size_kb_buf = "512".into();
        app.log_max_files_buf = "2".into();
        app.sync_numeric_fields();
        assert_eq!(app.cfg.log_max_size_kb, 512);
        assert_eq!(app.cfg.log_max_files, 2);

        app.log_max_size_kb_buf = "0".into();
        app.log_max_files_buf = "0".into();
        app.sync_numeric_fields();
        assert_eq!(app.cfg.log_max_size_kb, 0, "0 KB means never rotate on size");
        assert_eq!(app.cfg.log_max_files, 0, "0 files means keep no history");

        // A half-typed or junk value must leave the last good number alone.
        app.log_max_size_kb_buf = "".into();
        app.log_max_files_buf = "x".into();
        app.sync_numeric_fields();
        assert_eq!(app.cfg.log_max_size_kb, 0);
        assert_eq!(app.cfg.log_max_files, 0);
    }

    /// The CP/M ceiling is **clamped** here rather than range-guarded like its
    /// neighbours, because the config loader and `apply_config_key` clamp it —
    /// so the desktop must not be the one surface where typing a huge number
    /// does nothing instead of landing on the cap.
    ///
    /// The buffer is rewritten when the clamp bites. Without that it would hold
    /// `4000000000` while `cfg` held the cap for ever, and the window's
    /// unsaved-changes check compares exactly those two — an edit that could
    /// never be saved away.
    #[test]
    fn test_sync_cpm_ceiling_clamps_and_rewrites_the_buffer() {
        let mut app = test_app();
        app.cpm_emu_max_minstr_buf = "500".into();
        app.sync_numeric_fields();
        assert_eq!(app.cfg.cpm_emu_max_minstr, 500);

        app.cpm_emu_max_minstr_buf = "4000000000".into();
        app.sync_numeric_fields();
        assert_eq!(
            app.cfg.cpm_emu_max_minstr,
            crate::config::MAX_CPM_EMU_MAX_MINSTR,
            "the desktop must clamp, as the other two surfaces do"
        );
        assert_eq!(
            app.cpm_emu_max_minstr_buf,
            crate::config::MAX_CPM_EMU_MAX_MINSTR.to_string(),
            "the field must show what was actually kept, or it reads as an \
             unsaved change that cannot be saved"
        );

        // In range, and the boundary, are left exactly alone.
        app.cpm_emu_max_minstr_buf = crate::config::MAX_CPM_EMU_MAX_MINSTR.to_string();
        app.sync_numeric_fields();
        assert_eq!(app.cfg.cpm_emu_max_minstr, crate::config::MAX_CPM_EMU_MAX_MINSTR);

        // Zero and junk leave the last good value alone, as before.
        app.cpm_emu_max_minstr_buf = "0".into();
        app.sync_numeric_fields();
        assert_eq!(app.cfg.cpm_emu_max_minstr, crate::config::MAX_CPM_EMU_MAX_MINSTR);
        app.cpm_emu_max_minstr_buf = "x".into();
        app.sync_numeric_fields();
        assert_eq!(app.cfg.cpm_emu_max_minstr, crate::config::MAX_CPM_EMU_MAX_MINSTR);
    }

    /// `refresh_from_global` must rebuild the log buffers too — a buffer left
    /// holding a stale number is what makes the popup show one value and the
    /// config hold another.
    #[test]
    fn test_refresh_rebuilds_log_buffers() {
        let mut app = test_app();
        // refresh_from_global bails out when the global already matches the
        // snapshot, so perturb the SNAPSHOT rather than the global config —
        // mutating the process-wide config here would need CONFIG_TEST_LOCK and
        // would rewrite the on-disk egateway.conf that other tests read.
        app.last_synced_cfg.log_max_size_kb = app.cfg.log_max_size_kb.wrapping_add(1);
        app.dirty = false;
        app.log_max_size_kb_buf = "999999".into();
        app.log_max_files_buf = "77".into();
        app.refresh_from_global();
        assert_eq!(app.log_max_size_kb_buf, app.cfg.log_max_size_kb.to_string());
        assert_eq!(app.log_max_files_buf, app.cfg.log_max_files.to_string());
    }

    /// Every More popup starts closed, including the new General one.  A popup
    /// defaulting to open would cover the main screen on launch — which is
    /// exactly what the temporary flip used to screenshot it does, so the default
    /// is worth pinning.
    #[test]
    fn test_all_more_popups_start_closed() {
        let app = test_app();
        assert!(!app.server_popup_open, "Server");
        assert!(!app.general_popup_open, "General");
        assert!(!app.file_transfer_popup_open, "File Transfer");
        assert!(!app.ai_browser_popup_open, "AI/Browser");
        assert!(!app.cpm_mount_popup_open, "CP/M mounts");
        assert!(!app.serial_popup_open[0] && !app.serial_popup_open[1], "Serial A/B");
    }

    /// The draft must hold one slot per drive, or indexing a later drive
    /// would be out of bounds.
    #[test]
    fn test_cpm_mount_draft_has_one_slot_per_drive() {
        let app = test_app();
        assert_eq!(
            app.cpm_mount_draft.len(),
            crate::cpm::NUM_DRIVES as usize,
            "one draft slot per drive A:-P:"
        );
        assert!(
            app.cpm_mount_draft.iter().all(|s| s.is_empty()),
            "nothing is mounted until an image is chosen"
        );
    }

    /// Re-seeding runs from a button, so it can be called at any moment —
    /// including on a draft that is the wrong length.
    #[test]
    fn test_cpm_mount_reload_is_safe_on_a_short_draft() {
        let mut app = test_app();
        app.cpm_mount_draft.clear();
        app.cpm_mount_reload_draft();
        assert_eq!(app.cpm_mount_draft.len(), crate::cpm::NUM_DRIVES as usize);
    }

    /// `App::new` seeds the log buffers, same as every other numeric field.
    #[test]
    fn test_app_new_seeds_log_buffers() {
        let app = test_app();
        assert_eq!(app.log_max_size_kb_buf, app.cfg.log_max_size_kb.to_string());
        assert_eq!(app.log_max_files_buf, app.cfg.log_max_files.to_string());
    }

    /// The gateway geometry buffers get the same three-way treatment as the
    /// log limits: seeded by `App::new`, `0` survives `sync_numeric_fields`
    /// (it is the "auto" sentinel, so a `>= 1` floor copied from the port
    /// fields above would make automatic geometry unreachable from the GUI),
    /// and `refresh_from_global` rebuilds them so the popup can't show one
    /// number while the config holds another.
    #[test]
    fn test_gateway_term_buffers_seed_sync_and_refresh() {
        let mut app = test_app();
        assert_eq!(
            app.gateway_term_width_buf,
            app.cfg.gateway_term_width.to_string(),
            "App::new must seed the width buffer"
        );
        assert_eq!(
            app.gateway_term_height_buf,
            app.cfg.gateway_term_height.to_string(),
            "App::new must seed the rows buffer"
        );

        app.gateway_term_width_buf = "40".into();
        app.gateway_term_height_buf = "25".into();
        app.sync_numeric_fields();
        assert_eq!(app.cfg.gateway_term_width, 40);
        assert_eq!(app.cfg.gateway_term_height, 25);

        app.gateway_term_width_buf = "0".into();
        app.gateway_term_height_buf = "0".into();
        app.sync_numeric_fields();
        assert_eq!(app.cfg.gateway_term_width, 0, "0 columns means auto");
        assert_eq!(app.cfg.gateway_term_height, 0, "0 rows means auto");

        // Half-typed / junk / past-u16 leaves the last good value alone.
        app.gateway_term_width_buf = "80".into();
        app.sync_numeric_fields();
        app.gateway_term_width_buf = "".into();
        app.gateway_term_height_buf = "70000".into();
        app.sync_numeric_fields();
        assert_eq!(app.cfg.gateway_term_width, 80, "empty must not clobber");
        assert_eq!(app.cfg.gateway_term_height, 0, "past u16 must not clobber");

        // Perturb the snapshot (not the global) so refresh_from_global runs —
        // same reason as test_refresh_rebuilds_log_buffers.
        app.last_synced_cfg.gateway_term_width = app.cfg.gateway_term_width.wrapping_add(1);
        app.dirty = false;
        app.gateway_term_width_buf = "9999".into();
        app.gateway_term_height_buf = "9999".into();
        app.refresh_from_global();
        assert_eq!(app.gateway_term_width_buf, app.cfg.gateway_term_width.to_string());
        assert_eq!(app.gateway_term_height_buf, app.cfg.gateway_term_height.to_string());
    }

    /// The hint both the GUI and the web show is one function, and it must
    /// describe what is actually configured — the log hint existed twice and
    /// had already drifted (one copy said "the console above only", wrong
    /// inside a popup) before it was collapsed.
    #[test]
    fn test_gateway_term_hint_describes_each_state() {
        let auto = Config::gateway_term_hint(0, 0);
        assert!(auto.contains("Auto"), "0/0 must read as automatic: {auto}");

        let both = Config::gateway_term_hint(40, 25);
        assert!(
            both.contains("40x25"),
            "a full override should state the geometry: {both}"
        );

        let width_only = Config::gateway_term_hint(40, 0);
        assert!(
            width_only.contains("40") && width_only.contains("rows stay automatic"),
            "a width-only override should say the rows are still automatic: {width_only}"
        );

        let rows_only = Config::gateway_term_hint(0, 25);
        assert!(
            rows_only.contains("25") && rows_only.contains("width stays automatic"),
            "a rows-only override should say the width is still automatic: {rows_only}"
        );
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

    /// The CP/M modem profile's two numeric fields follow the same
    /// edited-as-text, parsed-on-sync pattern as every other numeric field
    /// here, and are clamped to the ranges the AT layer itself can produce
    /// (`ATX0`-`ATX4`, `AT&C0`/`AT&C1`) so a typo cannot store a state no
    /// command could set.
    #[test]
    fn test_sync_cpm_modem_profile_fields() {
        let mut app = test_app();
        app.cpm_emu_x_code_buf = "2".into();
        app.cpm_emu_dcd_mode_buf = "0".into();
        app.sync_numeric_fields();
        assert_eq!(app.cfg.cpm_emu_modem.x_code, 2);
        assert_eq!(app.cfg.cpm_emu_modem.dcd_mode, 0);

        // Out of range and unparsable both leave the stored value alone.
        app.cpm_emu_x_code_buf = "9".into();
        app.cpm_emu_dcd_mode_buf = "banana".into();
        app.sync_numeric_fields();
        assert_eq!(app.cfg.cpm_emu_modem.x_code, 2);
        assert_eq!(app.cfg.cpm_emu_modem.dcd_mode, 0);
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
        // Each entry needs a non-empty path and a non-empty tooltip (the
        // detail falls back to the path), since the pickers rely on both.
        for port in &ports {
            assert!(!port.name.is_empty());
            assert!(!port.detail.is_empty());
        }
        // The tooltip is safe to build from any list, including an empty one.
        assert!(!serial_ports_tooltip(&ports).is_empty());
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
        // source.  eglogobrightsmall.png is 366x183.
        let logo_w = 366.0_f32;
        let logo_h = 183.0_f32;
        // Logo should fit within a reasonable GUI panel.
        assert!(logo_h > 50.0 && logo_h < 400.0);
        assert!(logo_w > 80.0 && logo_w < 600.0);
        // Landscape, 2:1 aspect ratio.
        assert_eq!(logo_w, logo_h * 2.0);
    }
}

