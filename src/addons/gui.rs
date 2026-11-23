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

/// How the telnet side names where a window went: "the gateway's screen
/// (pi5)", or just "the gateway's screen" when the machine has no name to
/// give.  Callers are elsewhere by definition, and the name is how they know
/// which machine's screen to walk over to.
pub(crate) fn where_label(host: &str) -> String {
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

/// What became of a launch.
#[derive(Debug)]
pub(crate) enum Launch {
    /// It is open; `done` turns `true` when its window has closed.
    Opened(tokio::sync::watch::Receiver<bool>),
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

    if let Some(err) = child.stderr.take() {
        let id = m.id.clone();
        std::thread::spawn(move || log_stderr(&id, err));
    }
    let (tx, rx) = tokio::sync::watch::channel(false);
    let id = m.id.clone();
    std::thread::spawn(move || {
        let status = child.wait();
        // Ended as a tree: what it started goes with it, as on the
        // terminal side.
        if let Some(mut r) = take_if(&id, generation) {
            r.tree.kill_all();
        }
        let how = match status.ok().and_then(|s| s.code()) {
            Some(0) | None => "closed".to_string(),
            Some(code) => format!("closed with error {code}"),
        };
        crate::logger::log(format!("Add-on \"{id}\" window {how}"));
        let _ = tx.send(true);
    });
    Launch::Opened(rx)
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
fn log_stderr(id: &str, err: impl std::io::Read) {
    use std::io::BufRead;
    let mut n = 0u32;
    for line in std::io::BufReader::new(err).lines() {
        let Ok(line) = line else { break };
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

        let Launch::Opened(mut done) = launch_on(&a, &[], "a test", &Screen::Own) else { panic!("not opened") };
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
            tokio::time::timeout(std::time::Duration::from_secs(5), done.wait_for(|d| *d)).await.expect("never said closed").unwrap();
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
