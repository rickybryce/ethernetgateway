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
mod kermit;
mod logger;
mod portcheck;
mod punter;
mod relay;
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
            glog!(
                "Logging to {} — v{} (rotate at {} KB, keep {} old, max {} KB on disk)",
                cfg.log_file.trim(),
                env!("CARGO_PKG_VERSION"),
                cfg.log_max_size_kb,
                cfg.log_max_files,
                logger::max_disk_kb(cfg.log_max_size_kb, cfg.log_max_files),
            );
        }
        glog!("Config: telnet={}, port={}, security={}, transfer_dir={}",
            cfg.telnet_enabled, cfg.telnet_port, cfg.security_enabled, cfg.transfer_dir);
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

        // Create transfer directory if it doesn't exist
        if let Err(e) = std::fs::create_dir_all(&cfg.transfer_dir) {
            glog!("Error: could not create transfer directory '{}': {}", cfg.transfer_dir, e);
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
            // And the bundled terminals, for the same reason the folders are
            // laid out here rather than on someone's first session: erasing the
            // transfer directory and restarting used to recreate the drive
            // folders with no terminal in any of them, because the only caller
            // was the CP/M session path.  The loose transfer-directory copies
            // exist precisely so you can send a terminal to real hardware
            // *without* starting the emulator, so requiring a session to create
            // them defeated the feature.  Never overwrites.
            telnet::place_bundled_terminals(&cfg.transfer_dir, cfg.place_bundled_terminals);
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
            // GUI blocks the main thread until the window is closed.
            gui::run(gui_cfg, shutdown.clone(), restart.clone(), gui_ctx.clone());
            if !restart.load(Ordering::SeqCst) {
                // Window closed manually — fall through to headless wait so the server
                // keeps running in the background until Ctrl-C / SIGTERM.
                glog!("Console window closed. Server still running (Ctrl-C to stop).");
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
