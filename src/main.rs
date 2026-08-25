//! Ethernet Gateway
//!
//! Standalone telnet/SSH gateway for retro hardware (Commodore 64, CP/M,
//! RC2014, AltairDuino) and modern terminals.  Bundles XMODEM/YMODEM/ZMODEM
//! and Kermit file transfer, an SSH server + outbound SSH proxy, a Hayes-
//! compatible serial-modem emulator, a text-mode web browser, an AI chat
//! client, and a weather service — all driven from a single telnet menu
//! that auto-detects PETSCII/ANSI/ASCII terminals.
//!
//! Author: Ricky Bryce

// `collapsible_if` suggests folding `if a { if let Some(x) = b { .. } }` into a
// let chain.  It is pure style — the code is correct either way — and it is off
// until the declared `rust-version` allows let chains, so it appeared in 55
// places across 23 files the moment the MSRV was corrected from 1.87 to 1.88.
// (1.87 could not build this crate at all: the let chains already in it need
// 1.88.  See Cargo.toml.)
//
// Silenced rather than applied, for the same reason `cargo fmt` is not part of
// this workflow: taking it would re-indent the body of 55 hand-formatted blocks
// in one mechanical sweep, which is exactly the kind of mass rewrite this repo
// avoids on purpose.  Nothing is being hidden — the let-chain form is used
// freely where it was written that way, and this only declines to convert the
// blocks that were not.
//
// `cargo clippy --fix` was run to price this rather than guessed at: 22 files,
// 164 deletions, and the result is *worse* formatted than what it replaced,
// because clippy swaps `{ if` for `&&` and drops the closing braces without
// reformatting — it assumes `cargo fmt` follows, and here nothing does.  The
// `&&` continuations cascade one level deeper each time while the body keeps
// its old indentation, and in `cpm/fs.rs` it orphaned a comment from the block
// it explains.  So the real price is hand-reformatting 55 sites, not running a
// command.
//
// WHAT THE ALLOW COSTS, stated plainly: `collapsible_if` also covers plain
// nested booleans (`if a { if b { } }`), which was enabled and clean before the
// MSRV moved, so new nesting of that kind now goes unflagged too.  If that is
// worth reclaiming, collapse these by hand as you happen to touch the
// surrounding code and delete this attribute when the count reaches zero —
// there is no need for a sweep.
#![allow(clippy::collapsible_if)]

mod aichat;
mod bindwatch;
mod config;
mod cpm;
mod gui;
mod instance;
#[cfg(test)]
mod interop;
mod kermit;
mod logger;
mod portcheck;
mod punter;
mod relay;
mod resolve;
mod router;
mod serial;
mod ssh;
mod telnet;
mod tnio;
mod webbrowser;
mod webserver;
mod xmodem;
mod zmodem;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use logger::glog;

/// Shared slot the GUI fills with its `egui::Context` on startup so the
/// signal watcher can wake the event loop on Ctrl-C — without this nudge,
/// winit may sit idle waiting for a platform event that never arrives
/// and miss the shutdown flag transition.
type GuiCtxSlot = Arc<Mutex<Option<eframe::egui::Context>>>;

fn main() {
    logger::init();

    glog!("Ethernet Gateway v{}", env!("CARGO_PKG_VERSION"));
    glog!("Author: Ricky Bryce");
    glog!();

    // Shutdown and restart coordination (persist across restart cycles)
    let shutdown = Arc::new(AtomicBool::new(false));
    let restart = Arc::new(AtomicBool::new(false));
    let shutdown_notify = Arc::new(tokio::sync::Notify::new());
    let gui_ctx: GuiCtxSlot = Arc::new(Mutex::new(None));

    // Register POSIX signal handlers (SIGINT, SIGTERM, SIGHUP)
    register_signal_handlers(
        shutdown.clone(),
        restart.clone(),
        shutdown_notify.clone(),
        gui_ctx.clone(),
    );

    // Everything this program creates lives one level down, in
    // `ethernetgateway-data` (see `config::DATA_DIR`).  It has to exist before
    // the config is read, because the config is *in* it and a first launch
    // writes it there -- and before the logger is armed, for the same reason.
    //
    // A failure here is fatal rather than a warning.  Every other path leads
    // somewhere worse: `load_or_create_config` would fail to write and fall
    // back to insecure defaults on a directory it cannot create, the log would
    // silently go nowhere, and the SSH host key would be regenerated on every
    // launch so clients would see the identity change each time.  Better to
    // say which directory and why, once.
    if let Err(e) = config::ensure_data_dir() {
        let mut lines = vec![
            "       Everything the gateway writes lives there — the configuration,".to_string(),
            "       the log, the SSH host key and the transfer directory.".to_string(),
            "       Check that the directory this was launched from is writable:".to_string(),
            // **Named absolutely, because from a desktop icon nobody knows what
            // it is.** The working directory is whatever the launcher handed us
            // -- the desktop-entry spec leaves it undefined without `Path=` --
            // so "the directory this was launched from" is not a place the
            // operator can go and look at without being told which one.
            format!(
                "           {}",
                std::env::current_dir()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| "(unknown)".to_string())
            ),
        ];
        // The launch directory is the one that refused us, so that is what the
        // ownership question is about -- not the folder we failed to make.
        let (uid, name) = config::current_owner_identity();
        lines.extend(config::data_dir_ownership_lines(
            e.kind(),
            "The launch directory",
            config::file_owner_uid("."),
            uid,
            name.as_deref(),
        ));
        let headline =
            format!("FATAL: could not create the data directory '{}': {}", config::DATA_DIR, e);
        glog!("{}", headline);
        for line in &lines {
            glog!("{}", line);
        }
        // **And say it where it can be read.** From a desktop icon there is no
        // terminal, so every line above lands in the session journal and the
        // operator sees a program that does nothing when double-clicked.
        gui::show_startup_failure(&headline, &lines, shutdown.clone(), gui_ctx.clone());
        std::process::exit(1);
    }

    // Which data directory, absolutely, once.  The path is resolved against the
    // *launch* directory and a desktop launch does not define one, so the same
    // AppImage can land on two different trees -- and every other message names
    // it relatively, which cannot tell them apart.  See
    // `config::data_dir_display`.
    glog!("Data directory: {}", config::data_dir_display());

    // ── One gateway per directory ─────────────────────────────
    // Settled before a single listener is started, because the alternative is
    // what shipped until now: a second copy comes up fully, opens a window,
    // binds nothing, and then edits a config the serving copy never re-reads.
    //
    // The lock is held for the life of the process, so it is bound to a name
    // here; `let _ = ` would drop it at once and let a second copy straight in.
    let _instance_lock = match instance::acquire() {
        Ok(instance::Instance::Acquired(lock)) => {
            // Ours cleanly, so any request file on disk was left by a copy that
            // died mid-handover and is asking nobody.
            instance::clear_stale_handover_request();
            lock
        }
        Ok(instance::Instance::Busy { pid }) => {
            // The config is needed to answer one question -- is there a window
            // to ask in -- and reading it costs nothing we would not pay in a
            // moment anyway.
            let cfg = config::load_or_create_config();
            let who = match pid {
                Some(p) => format!("another copy (process {p})"),
                None => "another copy".to_string(),
            };
            glog!("The gateway is already running in this directory — {} holds the ports.", who);
            if !cfg.enable_console {
                // **A headless launch must not take over by itself.** With no
                // window there is nobody to ask, and a service restarted by
                // hand, or a second unit file started by accident, would
                // otherwise stop a working gateway and drop its sessions on
                // the strength of a double-click nobody made.
                glog!("       No console window is enabled, so there is nobody to ask whether");
                glog!("       to take over — refusing rather than stopping a running gateway.");
                glog!("       Stop the running copy first; its process id is above.");
                // **`enable_console = true` is not advice for a service.** This
                // is the message a systemd operator reads in the journal, and
                // that key only offers the choice where there is a desktop to
                // draw the question on -- with no display the launch would fail
                // a different way instead. So the condition is stated rather
                // than the key being recommended flatly.
                glog!("       Setting enable_console = true offers the choice instead, but only");
                glog!("       on a launch with a desktop to draw the question on.");
                std::process::exit(1);
            }
            // Ask, in a window with no server behind it.
            let take_over = Arc::new(AtomicBool::new(false));
            let asked = gui::run(
                cfg,
                shutdown.clone(),
                restart.clone(),
                gui_ctx.clone(),
                Some(gui::HandoverAsk { holder_pid: pid, take_over: take_over.clone() }),
            );
            // **The answer is read before the attempt is judged.** If the
            // window ran long enough to be clicked, that click stands even if
            // the event loop then died on the way out -- `take_over` is the
            // operator's instruction, and `asked` only says whether there was
            // an opportunity to give one.
            if !take_over.load(Ordering::SeqCst) && !asked {
                // **Nobody was asked, so nothing may be assumed.** With
                // `enable_console = true` on a machine with no display, winit
                // refuses the event loop -- and this used to fall through to
                // "Left the running copy alone", the line a deliberate Quit
                // prints, and exit 0. A service manager or a script then reads
                // success from a launch that did nothing at all, which is worse
                // than the headless refusal one config key away (measured
                // 2026-08-20).
                glog!("FATAL: no window could be opened, so there was nobody to ask whether");
                glog!("       to take over. The running copy has not been touched.");
                glog!("       Stop it first if you meant to replace it, or set");
                glog!("       enable_console = false to be refused without the attempt.");
                std::process::exit(1);
            }
            if !take_over.load(Ordering::SeqCst) {
                glog!("Left the running copy alone.");
                return;
            }
            glog!("Asking the running copy to stand down...");
            match instance::request_handover() {
                Ok(Some(lock)) => {
                    // The window was closed to end the ask; clear the flag it
                    // was closed with, or the server we are about to start
                    // would shut down the moment it came up.
                    shutdown.store(false, Ordering::SeqCst);
                    restart.store(false, Ordering::SeqCst);
                    glog!("Took over — this copy now holds the ports.");
                    lock
                }
                Ok(None) => {
                    glog!("FATAL: the running copy did not stand down in time.");
                    glog!("       It may be wedged. Stop it and start this one again.");
                    std::process::exit(1);
                }
                Err(e) => {
                    glog!("FATAL: could not ask the running copy to stand down: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Err(e) => {
            // **This is not the two-copies case, and must not read like it.**
            // A second copy from this directory arrives as `Busy` and is
            // offered a handover; reaching here means the lock *file* could not
            // be opened at all. The cause that dominates is ownership: a single
            // `sudo` run leaves a root-owned lock, and the launch used to die
            // on "could not claim the data directory", which names no cause and
            // sends the operator hunting a process that does not exist. The
            // diagnosis written for exactly this trap was two commits old and
            // unreachable in it, because the lock is opened before the config
            // is read (measured 2026-08-20).
            let lock = instance::lock_path();
            let mut lines = vec![
                "       This is not another copy holding the ports — one started here".to_string(),
                "       would have been offered a handover instead of this. The lock".to_string(),
                "       file itself could not be opened.".to_string(),
            ];
            let (uid, name) = config::current_owner_identity();
            // The file when it exists, the directory when it does not: a
            // missing lock means the refusal was the directory's.
            let (subject, owner) = if lock.exists() {
                ("The lock file", config::file_owner_uid(&lock))
            } else {
                ("The data directory", config::file_owner_uid(config::DATA_DIR))
            };
            let ownership =
                config::data_dir_ownership_lines(e.kind(), subject, owner, uid, name.as_deref());
            // **Always leave the operator something to do.** When ownership is
            // not the explanation there is still a mode, a read-only mount or a
            // full disk, and a message that names a cause it cannot confirm and
            // nothing else is worse than the blunt line it replaced.
            if ownership.is_empty() {
                lines.push(
                    "       Check the file's permissions and that the volume is writable."
                        .to_string(),
                );
            } else {
                lines.extend(ownership);
            }
            let headline = format!("FATAL: could not open '{}': {}", lock.display(), e);
            glog!("{}", headline);
            for line in &lines {
                glog!("{}", line);
            }
            gui::show_startup_failure(&headline, &lines, shutdown.clone(), gui_ctx.clone());
            std::process::exit(1);
        }
    };

    // Now that we own the directory, stand down for a later copy that asks.
    instance::spawn_handover_watcher(shutdown.clone());

    loop {
        // Load or create config (re-read from disk on each restart)
        let cfg = config::load_or_create_config();
        // File logging can only be armed once the config exists, so the three
        // banner lines above reach stderr only.  Re-applied on every restart
        // cycle because the config is re-read here; `configure_file_logging` is
        // idempotent and keeps the file open when the policy is unchanged.
        logger::configure_file_logging(logger::file_policy_from(&cfg));
        // Asks the policy, not `cfg.log_to_file`: a blank `log_file` disables
        // file logging too, and this line claimed "Logging to " with an empty
        // path when it consulted only the flag.  It also names the version,
        // because the file is appended to across restarts and the build that
        // wrote a given stretch of it is the first thing a reader needs.
        if logger::file_logging_enabled(&cfg) {
            // **Wanted is not open.** `configure_file_logging` has already
            // warned if the file could not be opened, and announcing the path
            // anyway put a flat contradiction on the next line: `could not open
            // log file ... Permission denied` followed by `Logging to ...`.
            // The sink itself is asked, so the banner cannot disagree with it.
            if logger::file_logging_is_paused() {
                glog!(
                    "Not logging to {} yet — it could not be opened; stderr and the console \
                     hold the log until a retry succeeds.",
                    cfg.log_file.trim(),
                );
            } else {
                glog!(
                    "Logging to {} — v{} (rotate at {} KB, keep {} old, max {} KB on disk)",
                    cfg.log_file.trim(),
                    env!("CARGO_PKG_VERSION"),
                    cfg.log_max_size_kb,
                    cfg.log_max_files,
                    logger::max_disk_kb(cfg.log_max_size_kb, cfg.log_max_files),
                );
            }
        }
        glog!("Config: telnet={}, port={}, security={}, transfer_dir={}",
            cfg.telnet_enabled, cfg.telnet_port, cfg.security_enabled, cfg.transfer_dir);
        // Warn a root session about the ownership it is about to impose on this
        // directory, while it can still be avoided — the same trap
        // `unreadable_config_diagnosis` explains a day later, when the files
        // are already root's and the operator has concluded the gateway needs
        // root.  Logged here as well as shown in the GUI banner because a root
        // run need not have a window, and both render the one list so they
        // cannot come to disagree.
        {
            let (is_root, sudo_user) = config::detect_elevation();
            for line in config::elevation_warning_lines(
                is_root,
                sudo_user.as_deref(),
                config::serial_access_group(),
            ) {
                glog!("NOTE: {}", line);
            }
        }
        if !cfg.telnet_enabled && !cfg.ssh_enabled {
            glog!("WARNING: Both telnet and SSH are disabled. No network access is possible.");
            glog!("         Enable at least one service in {}.", config::CONFIG_FILE);
        } else {
            if !cfg.telnet_enabled {
                glog!("Info: Telnet server is disabled. Enable it in {} if needed.", config::CONFIG_FILE);
            }
            if !cfg.ssh_enabled {
                glog!("Info: SSH server is disabled. Enable it in {} if needed.", config::CONFIG_FILE);
            }
        }
        if cfg.security_enabled && cfg.password == config::DEFAULT_PASSWORD {
            glog!("WARNING: Security is enabled with the default password. Change it in {}.", config::CONFIG_FILE);
        }
        if cfg.disable_ip_safety && !cfg.security_enabled {
            glog!("WARNING: disable_ip_safety=true with security_enabled=false — an");
            glog!("         unauthenticated session is reachable from ANY IP address.");
            glog!("         Enable security or restore IP safety in {}.", config::CONFIG_FILE);
        }

        // Master/Slave relay sanity checks — surface "silently armed but
        // inert" misconfigurations instead of failing quietly.
        if cfg.relays_blocked_by_ssh_off() {
            glog!("WARNING: gateway_role=master and master_accept_relays=true, but ssh_enabled=false.");
            glog!("         Relays ride the SSH server, so NO slave can connect until SSH is enabled.");
        }
        if cfg.relay_transport == "raw" && cfg.gateway_role != "standalone" {
            glog!("WARNING: relay_transport=raw is not yet implemented; the relay still uses SSH.");
        }

        // Ask the OS which address is this network's router, on a background
        // thread, so the answer is cached before the first connection arrives.
        // Only the `disable_gateway_connections` rule uses it, and that rule
        // falls back to the historical x.x.x.1 assumption until (or unless)
        // this lands — so nothing waits on it.
        router::probe_in_background();

        // Create transfer directory if it doesn't exist.
        //
        // The third path a single root run breaks: the default transfer
        // directory lives inside the data directory now, so this fails for the
        // same reason the lock and the config do, and it used to fail with no
        // cause named and nothing on screen from a desktop launch.
        if let Err(e) = std::fs::create_dir_all(&cfg.transfer_dir) {
            let headline = format!(
                "FATAL: could not create the transfer directory '{}': {}",
                cfg.transfer_dir, e
            );
            let (uid, name) = config::current_owner_identity();
            // The parent is what refused us -- the directory we could not make
            // has no owner to ask about.
            let parent = std::path::Path::new(&cfg.transfer_dir)
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .map(std::path::Path::to_path_buf)
                .unwrap_or_else(|| std::path::PathBuf::from("."));
            let mut lines = config::data_dir_ownership_lines(
                e.kind(),
                "Its parent directory",
                config::file_owner_uid(&parent),
                uid,
                name.as_deref(),
            );
            // As with the lock: never only a cause, and never no advice.
            if lines.is_empty() {
                lines.push(
                    "       Check that its parent directory is writable by this account."
                        .to_string(),
                );
            }
            glog!("{}", headline);
            for line in &lines {
                glog!("{}", line);
            }
            gui::show_startup_failure(&headline, &lines, shutdown.clone(), gui_ctx.clone());
            std::process::exit(1);
        }

        // With the emulator enabled, lay out its container now rather than on
        // someone's first session: the drive folders are where an operator puts
        // the software they want to run, and CPM/images is where they put a
        // disk image.  Both are things you want to do before a first session,
        // not after.  Nothing here overwrites, so it is safe on every start.
        if cfg.cpm_emu_enabled {
            if let Err(e) = cpm::layout::ensure_cpm_tree(&cfg.transfer_dir) {
                // Not fatal: the emulator recreates what it needs on launch,
                // and a gateway must still come up for its other services.
                glog!("Warning: could not create the CP/M folders: {}", e);
            }
            // And bring `cpm_mounts` up, here, at startup.
            //
            // This used to happen only when somebody first entered the
            // emulator, which left the mount table *empty* on a gateway nobody
            // had used yet — and an empty table is indistinguishable from "the
            // operator unmounted everything".  Every configuration screen
            // persists the live table, so one Save from the web page on a
            // freshly started gateway rewrote `cpm_mounts` as empty and the
            // operator's drives were gone for good.  No boot, no race and no
            // concurrency needed: restart, press Save.
            //
            // Doing it here also makes the documented behaviour true — the
            // module comment has always said `apply_config_mounts` brings
            // mounts up at startup.
            let base = cpm::layout::cpm_dir(&cfg.transfer_dir);
            cpm::image::apply_config_mounts(&base, &cfg.cpm_mounts);
        }

        // The bundled terminals, for the same reason the folders are laid out
        // at start-up rather than on someone's first session: erasing the
        // transfer directory and restarting used to recreate the drive folders
        // with no terminal in any of them, because the only caller was the CP/M
        // session path.
        //
        // Deliberately OUTSIDE the `cpm_emu_enabled` block above.  The loose
        // transfer-directory copy exists precisely so the file-transfer menus
        // can send a terminal to real hardware *without* the emulator, and
        // `place_bundled_terminals` is the key that says whether to write it --
        // so an operator who shut the emulator door and left that key on used to
        // get no copy anywhere, including the one whose whole point is to work
        // without the emulator.  It sat inside that block only because the call
        // was added next to `ensure_cpm_tree`, which does need the gate.
        //
        // Drive A: still does: its folder is only laid out when the emulator is
        // on, so the emulator's own setting is passed in rather than assumed.
        // Never overwrites, whichever destination.
        telnet::place_bundled_terminals(
            &cfg.transfer_dir,
            cfg.place_bundled_terminals,
            // Distinct type, so this cannot be swapped with the flag above.
            if cfg.cpm_emu_enabled { telnet::DriveA::Include } else { telnet::DriveA::Skip },
        );

        // Start tokio runtime on a worker thread so the main thread is free for the GUI.
        let shutdown_rt = shutdown.clone();
        let restart_rt = restart.clone();
        let notify_rt = shutdown_notify.clone();
        let gui_cfg = cfg.clone();
        let server_handle = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("Failed to create tokio runtime");

            let serial_handles = runtime.block_on(async move {
                let session_writers: telnet::SessionWriters =
                    Arc::new(tokio::sync::Mutex::new(Vec::new()));
                // One shared lockout map across telnet + SSH so an
                // attacker can't bounce between protocols to reset their
                // attempt counter.
                let lockouts: telnet::LockoutMap = Arc::new(
                    std::sync::Mutex::new(std::collections::HashMap::new()),
                );
                // Fresh slate for this server cycle: each listener registers
                // below and reports whether it bound, and the watcher then says
                // out loud if none of them did (see bindwatch).
                bindwatch::reset();
                // Cleared with it: the ports can change across a restart, so a
                // result kept from the last cycle would redden a port that was
                // never tested.
                portcheck::reset();
                telnet::start_server(
                    shutdown_rt.clone(),
                    restart_rt.clone(),
                    notify_rt.clone(),
                    session_writers.clone(),
                    lockouts.clone(),
                );
                ssh::start_ssh_server(
                    shutdown_rt.clone(),
                    restart_rt.clone(),
                    notify_rt.clone(),
                    session_writers.clone(),
                    lockouts.clone(),
                );
                let serial_handles = serial::start_serial(shutdown_rt.clone(), restart_rt.clone());
                // On a slave, offer the CP/M endpoint to the master for the whole
                // server lifetime, the way Ports A and B are offered — not only
                // while someone has the emulator open.  No-ops unless the role,
                // the master host, and the emulator's virtual modem all apply.
                serial::spawn_cpm_slave_announcer(shutdown_rt.clone());
                telnet::start_kermit_server(
                    shutdown_rt.clone(),
                    notify_rt.clone(),
                );
                webserver::start_web_server(
                    shutdown_rt.clone(),
                    restart_rt.clone(),
                    notify_rt.clone(),
                    lockouts,
                );
                // Every listener has registered by now (registration is
                // synchronous; only the bind itself is spawned), so the watcher
                // knows the full roster.  3 s is far longer than a bind takes
                // and is only an upper bound on how long it waits.
                bindwatch::spawn_watch(3_000);
                // And once they have settled, find out whether anything can
                // actually reach them.  At startup because the red labels are
                // the only signal, and somebody whose port is blocked is by
                // definition not getting the connection that would prompt them
                // to go looking.  Background, so it delays nothing.
                portcheck::spawn_startup_check(3_000);

                // Wait for shutdown signal
                loop {
                    if shutdown_rt.load(Ordering::SeqCst) {
                        glog!("\nShutdown signal received, stopping server...");
                        break;
                    }
                    // Bounded wait: `notify_waiters()` (used by every notifier)
                    // stores NO permit, so a notify landing in the gap between
                    // the load() above and this await would be missed — and
                    // since notifiers set `shutdown` before notifying, we could
                    // park here forever with shutdown already true, wedging the
                    // whole teardown (the "stuck after Ctrl-C" failure the
                    // surrounding paths guard against).  Re-checking every 200 ms
                    // turns any missed wakeup into <=200 ms latency, never a hang.
                    let _ = tokio::time::timeout(
                        std::time::Duration::from_millis(200),
                        notify_rt.notified(),
                    )
                    .await;
                }

                // Broadcast the goodbye to every live async session (telnet,
                // SSH, and master/slave relay all register their writer),
                // centrally so it fires for any combination of enabled
                // servers — including SSH-only, where the old telnet-accept-
                // loop broadcast never ran.  Serial sessions emit their own
                // notice from the serial thread on the shutdown flag.
                let goodbye = format!("\r\n\r\n{}\r\n", telnet::SHUTDOWN_GOODBYE);
                telnet::broadcast_to_sessions(&session_writers, goodbye.as_bytes(), true).await;

                // Give sessions a moment to receive the shutdown message
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;

                // Hand the serial-manager thread handles back to this (server)
                // thread so we can join them BEFORE the runtime is dropped
                // below — see the bounded-join note there.
                serial_handles
            });

            glog!("Server stopped.");

            // Join the detached serial-manager threads (bounded) BEFORE
            // dropping the runtime.  They `block_on` this runtime's handle, so
            // one still running when the runtime is dropped would panic on its
            // next `block_on` (e.g. an in-flight dial across a SIGHUP restart).
            // Their blocking work is abort-aware (100 ms read timeouts; the
            // dial connect is raced against the shutdown/restart flag), so they
            // exit within ~100 ms of shutdown; wait up to 3 s, then give up on
            // any straggler (leaving it detached — the prior behavior) rather
            // than wedging teardown.
            let serial_join_deadline =
                std::time::Instant::now() + std::time::Duration::from_secs(3);
            for h in serial_handles {
                while !h.is_finished() && std::time::Instant::now() < serial_join_deadline {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                if h.is_finished() {
                    let _ = h.join();
                }
            }

            // Cap runtime teardown at 2s so a stuck spawn_blocking task
            // (sync serialport read, slow filesystem flush, peer that
            // never closes its socket) can't wedge the process.  The
            // runtime's default Drop implementation waits indefinitely
            // for blocking tasks to complete — fine for clean exit, but
            // exactly the path that hangs the shell after Ctrl-C when a
            // long-lived blocking call is parked.  shutdown_timeout
            // gives the runtime a chance to finish work tidily and then
            // proceeds whether or not it did, so the spawned thread
            // always exits and `server_handle.join()` below always
            // returns.  Symptom we're closing: user hits Ctrl-C, sees
            // "Server stopped." once (this print) but never gets a
            // shell prompt because the join blocks on runtime drop.
            runtime.shutdown_timeout(std::time::Duration::from_secs(2));
        });

        if gui_cfg.enable_console {
            // GUI blocks the main thread until the window is closed.  The
            // return value says whether a window ran, which only the handover
            // ask above needs: here a failed GUI is not fatal -- the server is
            // already up, and the launch falls through to the headless wait.
            let _ = gui::run(gui_cfg, shutdown.clone(), restart.clone(), gui_ctx.clone(), None);
            if gui::window_closed_was_a_detach(
                restart.load(Ordering::SeqCst),
                shutdown.load(Ordering::SeqCst),
            ) {
                // Window closed manually — fall through to headless wait so the server
                // keeps running in the background until Ctrl-C / SIGTERM.
                //
                // **What to press depends on how this was launched, so ask.**
                // The line here used to say "Ctrl-C to stop" unconditionally,
                // which is right from a shell and unreachable from a desktop
                // icon: the AppImage's own desktop entry sets `Terminal=false`,
                // so the process inherits the graphical VT and there is no
                // shell anywhere to press it in.  Printing it anyway left
                // closing the window and relaunching as the only apparent
                // move, and that stacks copies which bind nothing (see
                // `bindwatch`).  `gui::window_closed_note` holds both
                // branches, beside the dialog that says the same thing before
                // the choice is made.
                glog!("{}", gui::window_closed_note(std::io::IsTerminal::is_terminal(
                    &std::io::stdout()
                )));
            }
        }

        // Headless mode — park the main thread until shutdown signal.
        loop {
            if shutdown.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }

        // Wait for the server thread to finish.
        shutdown.store(true, Ordering::SeqCst);
        shutdown_notify.notify_waiters();
        let _ = server_handle.join();

        if restart.load(Ordering::SeqCst) {
            // Reset flags and loop back to start fresh
            restart.store(false, Ordering::SeqCst);
            shutdown.store(false, Ordering::SeqCst);
            glog!("Restarting server...");
            glog!();
            continue;
        }

        break;
    }

    glog!("Server stopped.");
}

/// Register handlers for SIGINT, SIGTERM, and SIGHUP using signal-hook.
fn register_signal_handlers(
    shutdown: Arc<AtomicBool>,
    restart: Arc<AtomicBool>,
    notify: Arc<tokio::sync::Notify>,
    gui_ctx: GuiCtxSlot,
) {
    use signal_hook::consts::{SIGINT, SIGTERM};

    // signal-hook's flag::register sets the AtomicBool on signal delivery
    signal_hook::flag::register(SIGINT, shutdown.clone())
        .expect("Failed to register SIGINT handler");
    signal_hook::flag::register(SIGTERM, shutdown.clone())
        .expect("Failed to register SIGTERM handler");

    // SIGHUP is a *reload* request, not a shutdown: systemd's ExecReload
    // (`kill -HUP`) sends it and expects the service to come back up with
    // fresh config.  Route it to a dedicated flag that the watcher below
    // turns into a restart (arm `restart`, then trip `shutdown` to unwind
    // the current server cycle — the main loop then re-reads egateway.conf
    // and starts fresh).  Registering SIGHUP straight to `shutdown` — as we
    // used to — exits cleanly with code 0, so `systemctl reload` would
    // silently stop the gateway and `Restart=on-failure` would leave it
    // down.
    let sighup = Arc::new(AtomicBool::new(false));
    #[cfg(unix)]
    {
        use signal_hook::consts::SIGHUP;
        signal_hook::flag::register(SIGHUP, sighup.clone())
            .expect("Failed to register SIGHUP handler");
    }

    // Spawn a thread that watches the flags and fires the Notify.
    // Loops to survive server restarts (flag resets to false between cycles).
    let shutdown_watch = shutdown.clone();
    std::thread::spawn(move || {
        loop {
            while !shutdown_watch.load(Ordering::SeqCst) && !sighup.load(Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            // A SIGHUP reload arms the restart path before we unwind the
            // server cycle; a plain SIGINT/SIGTERM leaves `restart` unset,
            // so the main loop exits instead of looping back.
            if sighup.swap(false, Ordering::SeqCst) {
                restart.store(true, Ordering::SeqCst);
                shutdown_watch.store(true, Ordering::SeqCst);
            }
            notify.notify_waiters();
            // Force the GUI event loop to repaint AND queue the Close
            // command directly.  Two prior bugs we've hit here:
            //
            // 1. winit can sit idle waiting for a platform event and
            //    the close command queued by `update()` never gets a
            //    chance to fire — `request_repaint()` posts a wakeup
            //    UserEvent that drains the queue.
            // 2. When the window is minimized to the taskbar, some
            //    window managers throttle or pause repaint delivery
            //    entirely.  `update()` never runs, so even though the
            //    shutdown flag is set, the close command queued
            //    inside `update()` never lands.  The user's symptom:
            //    `Ctrl-C`, "Server stopped." prints once (from the
            //    server thread), no shell prompt, no GUI on screen
            //    until the user restores the window from the taskbar
            //    — only then does winit wake up, run `update()`,
            //    notice the flag, and finally exit.
            //
            // Calling `send_viewport_cmd_to(ROOT, Close)` from this
            // signal-watcher thread enqueues the close command in
            // egui's command buffer directly.  Combined with the
            // repaint nudge, the GUI exits as soon as winit drains
            // its queue once — which it does promptly even when
            // minimized, because the UserEvent itself is the wakeup.
            if let Some(ctx) = gui_ctx.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                ctx.send_viewport_cmd_to(
                    eframe::egui::ViewportId::ROOT,
                    eframe::egui::ViewportCommand::Close,
                );
                ctx.request_repaint();
            }
            // Wait for the flag to be reset (restart) before watching again
            while shutdown_watch.load(Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shutdown_flag_default() {
        let flag = Arc::new(AtomicBool::new(false));
        assert!(!flag.load(Ordering::SeqCst));
    }

    #[test]
    fn test_shutdown_flag_set() {
        let flag = Arc::new(AtomicBool::new(false));
        flag.store(true, Ordering::SeqCst);
        assert!(flag.load(Ordering::SeqCst));
    }

    #[test]
    fn test_signal_handlers_register() {
        // Verify that signal registration doesn't panic
        let shutdown = Arc::new(AtomicBool::new(false));
        let restart = Arc::new(AtomicBool::new(false));
        let notify = Arc::new(tokio::sync::Notify::new());
        let gui_ctx: GuiCtxSlot = Arc::new(Mutex::new(None));
        // This should not panic — signals can be registered multiple times
        register_signal_handlers(shutdown, restart, notify, gui_ctx);
    }

    #[test]
    fn test_transfer_dir_creation() {
        let dir = std::env::temp_dir().join("xmodem_test_transfer_dir_main");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!dir.exists());
        std::fs::create_dir_all(&dir).unwrap();
        assert!(dir.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
