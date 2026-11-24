//! GUI add-ons: a program opened on the gateway machine's own screen.
//!
//! **The only screen there is.**  A caller on telnet is somewhere else, but a
//! window has to open somewhere, and the gateway has one display: its own, or
//! the one the operator named in `addon_display` for a gateway that has none
//! (a headless Pi started from a service).  With neither, the add-on is
//! refused with a line saying so -- never started into nothing.
//!
//! **A window belongs to the gateway, not to the caller.**  It is started
//! here, remembered in [`OPEN`], and ended when it closes, when someone closes
//! it, or when the gateway stops ([`close_all`]) -- never left behind.  One
//! window per add-on: a second caller is told it is already open rather than
//! stacking copies on a screen nobody may be watching.
//!
//! Std processes and a thread per window rather than tokio: the desktop
//! window starts these from its own thread, which has no runtime.

use super::{AddOn, Tree};
use std::collections::HashMap;
use std::sync::Mutex;

/// The gateway's variables a window needs to reach the desktop, given to a
/// GUI add-on on top of [`super::PASSED_ENV`].  The terminal side never gets
/// them: it has no window to open.
pub(crate) const GUI_ENV: &[&str] = &[
    "DISPLAY", "WAYLAND_DISPLAY", "XAUTHORITY", "XDG_RUNTIME_DIR", "XDG_SESSION_TYPE",
    "DBUS_SESSION_BUS_ADDRESS",
    // Windows: where a program keeps its settings.
    "APPDATA", "LOCALAPPDATA", "PROGRAMDATA",
];

/// Where a window would open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Screen {
    /// The gateway's own graphical session.
    Own,
    /// The `addon_display` the operator named (Linux only).
    Display(String),
}

/// The screen a window would open on, or `None` when there is none.
///
/// A function of its inputs so every case is tested: `has_own` is
/// [`crate::gui::have_graphical_session`] in use, and `addon_display` is
/// consulted only when there is no session of its own -- naming `:0` on a
/// desktop must not move windows off the desktop the gateway is on.
pub(crate) fn screen_for(has_own: bool, addon_display: &str) -> Option<Screen> {
    if has_own {
        return Some(Screen::Own);
    }
    let display = addon_display.trim();
    if cfg!(all(unix, not(target_os = "macos"))) && !display.is_empty() {
        return Some(Screen::Display(display.to_string()));
    }
    None
}

/// [`screen_for`] for this gateway as it is configured now.
pub(crate) fn screen() -> Option<Screen> {
    screen_for(crate::gui::have_graphical_session(), &crate::config::get_config().addon_display)
}

/// The environment a window is started with: the terminal side's (see
/// [`super::add_on_env`]), the [`GUI_ENV`] the gateway has, and for a named
/// display `DISPLAY` set to it, with `WAYLAND_DISPLAY` removed so a toolkit
/// that prefers Wayland does not go looking for a compositor that is not
/// there.
pub(crate) fn gui_env(extra: &[(String, String)], screen: &Screen) -> Vec<(String, String)> {
    let mut env = super::add_on_env(extra);
    for k in GUI_ENV {
        if let Ok(v) = std::env::var(k)
            && !env.iter().any(|(ek, _)| ek == k)
        {
            env.push((k.to_string(), v));
        }
    }
    if let Screen::Display(d) = screen {
        env.retain(|(k, _)| k != "DISPLAY" && k != "WAYLAND_DISPLAY");
        env.push(("DISPLAY".into(), d.clone()));
    }
    env
}

/// What the GUI display setting is for, said where it is set (web page and
/// desktop window) on a gateway that has a desktop of its own: there it is
/// not used at all, and nothing else would tell the operator so.
pub(crate) const DISPLAY_UNUSED_HINT: &str = "Not used here: this gateway runs on a desktop, so GUI \
     add-ons open on its screen. The setting matters only to a gateway with no desktop, such as \
     one started as a service or over SSH.";

/// The same, on a gateway with no desktop, where the setting decides.  The
/// remote example is the one measured: a headless gateway's window drawn on
/// another machine's X server that listened on TCP and allowed it by `xhost`.
pub(crate) const DISPLAY_HINT: &str = "This gateway has no desktop, so GUI add-ons open on this X \
     display: :0 for this computer's own screen, or another computer's, such as 192.168.1.50:0 \
     (its X server must accept network connections and allow this gateway, e.g. \
     xhost +<gateway address>). Empty: GUI add-ons are refused.";

/// How the telnet side names where a window went: "the gateway's screen
/// (pi5)", or just "the gateway's screen" when the machine has no name to
/// give; or, for a display the operator named, "display 192.168.1.50:0" --
/// which need not be on the gateway at all.  Callers are elsewhere by
/// definition, and this is how they know which screen to walk over to.
pub(crate) fn where_label(host: &str, screen: &Screen) -> String {
    if let Screen::Display(d) = screen {
        return format!("display {d}");
    }
    let host = host.trim();
    if host.is_empty() {
        "the gateway's screen".into()
    } else {
        format!("the gateway's screen ({host})")
    }
}

/// The `EG_*` set for a window opened from the desktop window: no caller,
/// so no terminal, user or address -- the folders, the version, and
/// `EG_CONNECTION=desktop`.
pub(crate) fn desktop_env(a: &AddOn) -> Vec<(String, String)> {
    let cfg = crate::config::get_config();
    let abs = |p: &std::path::Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf()).to_string_lossy().into_owned();
    vec![
        ("EG_CONNECTION".into(), "desktop".into()),
        ("EG_ADDON_DIR".into(), abs(&a.dir)),
        ("EG_DATA_DIR".into(), abs(std::path::Path::new(crate::config::DATA_DIR))),
        ("EG_TRANSFER_DIR".into(), abs(std::path::Path::new(&cfg.transfer_dir))),
        ("EG_GATEWAY_VERSION".into(), env!("CARGO_PKG_VERSION").into()),
    ]
}

/// One open window.
struct Running {
    tree: Tree,
    /// Which launch this entry belongs to, so the thread waiting on an old
    /// window never removes a newer one of the same add-on.
    generation: u64,
}

/// The windows open now, by add-on id.
static OPEN: Mutex<Option<HashMap<String, Running>>> = Mutex::new(None);

fn open_windows() -> std::sync::MutexGuard<'static, Option<HashMap<String, Running>>> {
    OPEN.lock().unwrap_or_else(|e| e.into_inner())
}

/// How a window ended: its exit code (`None` when a signal ended it) and the
/// last line it wrote to stderr, which is usually the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Closed {
    pub(crate) code: Option<i32>,
    pub(crate) last_line: Option<String>,
}

impl Closed {
    /// It ended with an error of its own (not by a signal, which is how
    /// `close` ends one).
    pub(crate) fn failed(&self) -> bool {
        self.code.is_some_and(|c| c != 0)
    }
}

/// A window that was started.
#[derive(Debug)]
pub(crate) struct Opened {
    /// `None` while it is open, then how it closed.
    pub(crate) done: tokio::sync::watch::Receiver<Option<Closed>>,
    /// Where it was sent.
    pub(crate) screen: Screen,
}

/// How long a just-started window is given to fail before the caller is
/// told it opened.  **A start is not an open.**  A window sent to a display
/// that refuses it ("Can't open display") exits within milliseconds, and the
/// caller was told it had opened -- on the wrong machine's screen, too.  A
/// second is long enough for that and short enough not to be noticed.
pub(crate) const SETTLE: std::time::Duration = std::time::Duration::from_millis(1000);

/// How the window ended, if it ended within `within`; `None` when it is
/// still open.
pub(crate) async fn closed_within(
    done: &mut tokio::sync::watch::Receiver<Option<Closed>>,
    within: std::time::Duration,
) -> Option<Closed> {
    let _ = tokio::time::timeout(within, done.wait_for(|d| d.is_some())).await;
    done.borrow().clone()
}

/// What became of a launch.
#[derive(Debug)]
pub(crate) enum Launch {
    /// It was started; see [`Opened`].
    Opened(Opened),
    /// That add-on's window is open already (one per add-on).
    AlreadyOpen,
    /// There is no screen to open it on.
    NoScreen,
    /// It could not be started: the reason, for the log and the operator.
    Failed(String),
}

/// Open `a`'s window.  `extra` is the `EG_*` set (the caller's, or the
/// desktop window's); `who` says who asked, for the log.
pub(crate) fn launch(a: &AddOn, extra: &[(String, String)], who: &str) -> Launch {
    match screen() {
        Some(s) => launch_on(a, extra, who, &s),
        None => Launch::NoScreen,
    }
}

/// [`launch`] on a screen already decided.
pub(crate) fn launch_on(a: &AddOn, extra: &[(String, String)], who: &str, screen: &Screen) -> Launch {
    static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let m = &a.manifest;
    let Some(cmd) = m.gui_command.as_deref().filter(|_| m.mode.has_gui()) else {
        return Launch::Failed("it has no window to open".into());
    };
    let env = gui_env(extra, screen);
    // Held from the check to the insert, so two callers choosing it in the
    // same instant cannot both start one.
    let mut open = open_windows();
    let open = open.get_or_insert_with(HashMap::new);
    if open.contains_key(&m.id) {
        return Launch::AlreadyOpen;
    }
    // The containment check again, at the step that relies on it.
    let mut c = match super::std_command(&a.dir, cmd, &m.gui_args, &env) {
        Ok(c) => c,
        Err(e) => return Launch::Failed(e),
    };
    c.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    let mut child = match c.spawn() {
        Ok(c) => c,
        Err(e) => return Launch::Failed(e.to_string()),
    };
    let tree = Tree::of_std(&child);
    let generation = GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    open.insert(m.id.clone(), Running { tree, generation });
    crate::logger::log(format!("Add-on \"{}\" opened its window for {who}", m.id));

    // The last thing it wrote, for the caller when it fails at once; and a
    // line to say the reader has finished, so the waiter reads it after the
    // last line and not before.
    let last_line = std::sync::Arc::new(Mutex::new(None::<String>));
    let (read_tx, read_rx) = std::sync::mpsc::channel::<()>();
    if let Some(err) = child.stderr.take() {
        let id = m.id.clone();
        let last = last_line.clone();
        std::thread::spawn(move || {
            log_stderr(&id, err, &last);
            let _ = read_tx.send(());
        });
    }
    let (tx, rx) = tokio::sync::watch::channel(None);
    let id = m.id.clone();
    std::thread::spawn(move || {
        let status = child.wait();
        // Bounded: something it started can hold its stderr open after it
        // has gone, and the reason is worth a moment, not a hang.  A second,
        // not less: the reader is normally done the instant the program
        // exits, but on a busy machine 300 ms lost the reason under load
        // (a full test run on a Pi), and the reason is the whole point.
        let _ = read_rx.recv_timeout(std::time::Duration::from_secs(1));
        // Ended as a tree: what it started goes with it, as on the
        // terminal side.
        if let Some(mut r) = take_if(&id, generation) {
            r.tree.kill_all();
        }
        let code = status.ok().and_then(|s| s.code());
        let how = match code {
            Some(0) | None => "closed".to_string(),
            Some(code) => format!("closed with error {code}"),
        };
        crate::logger::log(format!("Add-on \"{id}\" window {how}"));
        let last_line = last_line.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let _ = tx.send(Some(Closed { code, last_line }));
    });
    Launch::Opened(Opened { done: rx, screen: screen.clone() })
}

/// Remove `id`'s entry if it is still the launch numbered `generation`.
fn take_if(id: &str, generation: u64) -> Option<Running> {
    let mut open = open_windows();
    let open = open.as_mut()?;
    if open.get(id).is_some_and(|r| r.generation == generation) {
        open.remove(id)
    } else {
        None
    }
}

/// What a window writes to stderr, to the log: capped as on the terminal
/// side, so a chatty one cannot fill it.
fn log_stderr(id: &str, err: impl std::io::Read, last: &Mutex<Option<String>>) {
    use std::io::BufRead;
    let mut n = 0u32;
    for line in std::io::BufReader::new(err).lines() {
        let Ok(line) = line else { break };
        if !line.trim().is_empty() {
            *last.lock().unwrap_or_else(|e| e.into_inner()) = Some(line.trim().chars().take(200).collect());
        }
        n += 1;
        if n <= 200 {
            let line: String = line.chars().take(200).collect();
            crate::logger::log(format!("Add-on \"{id}\": {line}"));
        } else if n == 201 {
            crate::logger::log(format!("Add-on \"{id}\": (more output not logged)"));
        }
    }
}

/// Whether `id`'s window is open.
pub(crate) fn is_open(id: &str) -> bool {
    open_windows().as_ref().is_some_and(|o| o.contains_key(id))
}

/// Close `id`'s window and whatever it started.  `false` when none was open.
pub(crate) fn close(id: &str) -> bool {
    let taken = open_windows().as_mut().and_then(|o| o.remove(id));
    match taken {
        Some(mut r) => {
            r.tree.kill_all();
            true
        }
        None => false,
    }
}

/// Close every window: the gateway is stopping, and a window it opened must
/// not outlive it.
pub(crate) fn close_all() {
    let all: Vec<Running> = open_windows().as_mut().map(|o| o.drain().map(|(_, r)| r).collect()).unwrap_or_default();
    for mut r in all {
        r.tree.kill_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The open windows are one table for the whole process, and
    /// [`close_all`] empties it: a test that starts real windows holds this,
    /// or another one's `close_all` ends its windows mid-test.
    #[cfg(unix)]
    static WINDOWS: Mutex<()> = Mutex::new(());

    #[test]
    fn test_a_window_opens_on_the_gateways_own_screen_first() {
        assert_eq!(screen_for(true, ""), Some(Screen::Own));
        // A named display does not move windows off a desktop the gateway has.
        assert_eq!(screen_for(true, ":1"), Some(Screen::Own));
    }

    #[test]
    fn test_with_no_screen_a_window_needs_addon_display() {
        assert_eq!(screen_for(false, ""), None);
        assert_eq!(screen_for(false, "   "), None);
        let named = screen_for(false, " :0 ");
        if cfg!(all(unix, not(target_os = "macos"))) {
            assert_eq!(named, Some(Screen::Display(":0".into())));
        } else {
            // Linux only: elsewhere a gateway always has its own screen.
            assert_eq!(named, None);
        }
    }

    /// Where a caller is told the window went: the gateway's own screen by
    /// its machine's name, or the display named -- which may be another
    /// computer's, so it is never called the gateway's.
    #[test]
    fn test_a_window_is_said_to_open_where_it_went() {
        assert_eq!(where_label("pi5", &Screen::Own), "the gateway's screen (pi5)");
        assert_eq!(where_label(" ", &Screen::Own), "the gateway's screen");
        assert_eq!(where_label("pi5", &Screen::Display("192.168.1.50:0".into())), "display 192.168.1.50:0");
    }

    /// **A start is not an open.**  A window whose display refuses it exits
    /// at once with an error; the caller must hear that, with the program's
    /// own reason, not "opened".  One still running after the wait has opened.
    /// No display is needed -- the programs are scripts.
    #[cfg(unix)]
    #[test]
    fn test_a_window_that_fails_at_once_is_reported_with_its_reason() {
        let _one = WINDOWS.lock().unwrap_or_else(|e| e.into_inner());
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("egw-gui-fail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let make = |name: &str, body: &str| {
            let d = dir.join(name);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("addon.conf"), format!("name = {name}\nmode = gui\ngui_command = w.sh\n")).unwrap();
            std::fs::write(d.join("w.sh"), format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(d.join("w.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
            crate::addons::load(&d).unwrap()
        };
        let bad = make("bad", "echo starting >&2\necho \"Error: Can't open display: 10.0.0.9:0\" >&2\nexit 1");
        let good = make("good", "exec sleep 300");
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();

        let screen = Screen::Display("10.0.0.9:0".into());
        let Launch::Opened(Opened { mut done, screen: went }) = launch_on(&bad, &[], "a test", &screen) else { panic!("not started") };
        assert_eq!(went, screen);
        let closed = rt.block_on(closed_within(&mut done, std::time::Duration::from_secs(5))).expect("it failed at once");
        assert!(closed.failed());
        assert_eq!(closed.last_line.as_deref(), Some("Error: Can't open display: 10.0.0.9:0"));

        let Launch::Opened(Opened { mut done, .. }) = launch_on(&good, &[], "a test", &Screen::Own) else { panic!("not started") };
        assert_eq!(rt.block_on(closed_within(&mut done, std::time::Duration::from_millis(300))), None, "still open");
        assert!(close(&good.manifest.id));
        // Ended by `close` -- a signal, not an error of its own.
        let closed = rt.block_on(closed_within(&mut done, std::time::Duration::from_secs(5))).expect("closed");
        assert!(!closed.failed(), "{closed:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_a_named_display_replaces_the_gateways_own() {
        let env = gui_env(&[("EG_TERMINAL".into(), "ansi".into())], &Screen::Display(":7".into()));
        let displays: Vec<&String> = env.iter().filter(|(k, _)| k == "DISPLAY").map(|(_, v)| v).collect();
        assert_eq!(displays, vec![":7"]);
        assert!(!env.iter().any(|(k, _)| k == "WAYLAND_DISPLAY"));
        assert!(env.iter().any(|(k, v)| k == "EG_TERMINAL" && v == "ansi"));
    }

    #[test]
    fn test_a_window_gets_no_secrets_either() {
        // The allowlist still holds on the GUI side: GUI_ENV adds desktop
        // plumbing, not the gateway's own variables.
        for k in GUI_ENV {
            assert!(!k.contains("KEY") && !k.contains("TOKEN") && !k.contains("PASS"), "{k}");
        }
        let env = gui_env(&[], &Screen::Own);
        for (k, _) in &env {
            assert!(
                super::super::PASSED_ENV.contains(&k.as_str()) || GUI_ENV.contains(&k.as_str()),
                "{k} reached a window"
            );
        }
    }

    /// A real window side, started and ended: one copy at a time, `close`
    /// ends it and what it started, and one that exits by itself frees its
    /// slot and says so.  No display is needed -- the program is a script.
    #[cfg(unix)]
    #[test]
    fn test_a_window_is_one_copy_and_closes_with_its_children() {
        let _one = WINDOWS.lock().unwrap_or_else(|e| e.into_inner());
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("egw-gui-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pidfile = dir.join("child.pid");
        std::fs::write(dir.join("addon.conf"), "name = Win\nmode = gui\ngui_command = w.sh\n").unwrap();
        std::fs::write(
            dir.join("w.sh"),
            format!("#!/bin/sh\nsleep 300 &\necho $! > {}\nexec sleep 300\n", pidfile.display()),
        )
        .unwrap();
        std::fs::set_permissions(dir.join("w.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let a = crate::addons::load(&dir).unwrap();
        let id = a.manifest.id.clone();

        let Launch::Opened(Opened { mut done, .. }) = launch_on(&a, &[], "a test", &Screen::Own) else { panic!("not opened") };
        assert!(is_open(&id));
        assert!(matches!(launch_on(&a, &[], "a test", &Screen::Own), Launch::AlreadyOpen));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let child: i32 = loop {
            if let Some(p) = std::fs::read_to_string(&pidfile).ok().and_then(|s| s.trim().parse().ok()) {
                break p;
            }
            assert!(std::time::Instant::now() < deadline, "the window never started");
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        assert!(close(&id));
        assert!(!is_open(&id));
        // The waiter sees it go and says so.
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(5), done.wait_for(|d| d.is_some())).await.expect("never said closed").unwrap();
        });
        // Its background program went with it.
        let gone = (0..100).any(|_| {
            // SAFETY: signal 0 only asks whether the pid exists.
            let alive = unsafe { libc::kill(child, 0) } == 0
                && std::fs::read_to_string(format!("/proc/{child}/stat")).map(|s| !s.contains(") Z ")).unwrap_or(false);
            if alive {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            !alive
        });
        assert!(gone, "the window's background program outlived it");
        // And the slot is free again.
        let Launch::Opened(_) = launch_on(&a, &[], "a test", &Screen::Own) else { panic!("not reopened") };
        close_all();
        assert!(!is_open(&id));
        let _ = std::fs::remove_dir_all(&dir);
    }

}
