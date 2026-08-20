//! Did the listeners actually come up?
//!
//! Each listener already logs its own bind failure, but a single line among the
//! startup chatter is easy to miss — and the process keeps running afterwards,
//! quietly serving nothing.  The failure mode that costs real time is a second
//! copy of the gateway started without stopping the first: the old process
//! holds the ports, the new one binds nothing, and everything you connect to is
//! still being served by the old binary.  It looks exactly like "my settings
//! changed nothing".
//!
//! So the listeners report here as they bind, and a watcher says out loud what
//! the individual lines only imply: *none of your listeners came up, something
//! else is holding the ports.*  Diagnostics only — nothing here changes what
//! the gateway does.

use std::collections::BTreeMap;
use std::io;
use std::sync::{Mutex, OnceLock};

use crate::logger::glog;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Status {
    /// Registered, but hasn't reported yet.
    Pending,
    Bound,
    /// `in_use` distinguishes "someone else has the port" — the case worth
    /// naming — from a permission error or a bad address.
    Failed { in_use: bool },
}

#[derive(Default)]
struct State {
    /// name -> (port, status).  Ordered so the message reads the same way twice.
    listeners: BTreeMap<&'static str, (u16, Status)>,
    /// The summary is logged at most once per server cycle.
    reported: bool,
    /// Which server cycle these outcomes belong to; bumped by [`reset`].
    ///
    /// **A Save and Restart produces the same text about a different attempt.**
    /// The desktop banner is dismissed by the operator, and if dismissal were
    /// tied to the text alone then a restart that failed *identically* would
    /// stay silent -- which is the one case that matters, since the settings
    /// they just saved are the reason they restarted. Comparing the cycle as
    /// well makes "the same words about a new attempt" a new thing to say.
    cycle: u64,
}

fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(State::default()))
}

fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    let mut guard = state().lock().unwrap_or_else(|e| e.into_inner());
    f(&mut guard)
}

/// Clear the slate for a new server cycle (startup, and every restart).
pub(crate) fn reset() {
    with(|s| {
        s.listeners.clear();
        s.reported = false;
        s.cycle = s.cycle.wrapping_add(1);
    });
}

/// A listener is configured and about to try binding.  Called synchronously,
/// before the task is spawned, so the watcher knows the full roster up front
/// and can't decide "everything reported" while a bind is still in flight.
pub(crate) fn expect(name: &'static str, port: u16) {
    with(|s| {
        s.listeners.insert(name, (port, Status::Pending));
    });
}

/// What became of one listener's bind, for a caller that needs the *outcome*
/// rather than the intention.
///
/// **The config says what was asked for; this says what happened.** The desktop
/// UI's screen button is the caller: it opens a browser at the web server, and
/// `web_enabled = true` with the port already taken by a second copy of the
/// gateway would otherwise send somebody to a refused connection — or, worse, to
/// the *other* instance's configuration page. That second-copy case is the one
/// this module was written for, so it already knows the answer; nothing was
/// asking it.
///
/// `None` when the listener is not in this cycle's roster at all, which is how
/// "not configured" reads.
pub(crate) fn status_of(name: &str) -> Option<(u16, Status)> {
    with(|s| s.listeners.get(name).copied())
}

/// Is any listener still waiting to report?
///
/// For the startup port check: probing a listener that has not bound yet would
/// report it blocked, which is wrong in the one direction that module is
/// careful about.
pub(crate) fn any_pending() -> bool {
    with(|s| s.listeners.values().any(|(_, st)| *st == Status::Pending))
}

/// Every listener that really took its port, as `(name, port)`.
///
/// For the port check: a listener that failed to bind is not a firewall problem
/// and must not be probed as one -- the answer would be "nothing answered",
/// which is true and would be reported as the wrong cause.
pub(crate) fn bound_listeners() -> Vec<(String, u16)> {
    with(|s| {
        s.listeners
            .iter()
            .filter(|(_, (_, st))| *st == Status::Bound)
            .map(|(name, (port, _))| ((*name).to_string(), *port))
            .collect()
    })
}

pub(crate) fn bound(name: &'static str) {
    with(|s| {
        if let Some(entry) = s.listeners.get_mut(name) {
            entry.1 = Status::Bound;
        }
    });
}

pub(crate) fn failed(name: &'static str, err: &io::Error) {
    let in_use = err.kind() == io::ErrorKind::AddrInUse;
    with(|s| {
        if let Some(entry) = s.listeners.get_mut(name) {
            entry.1 = Status::Failed { in_use };
        }
    });
}

/// Watch until every configured listener has reported (or `timeout_ms` passes,
/// so one that never answers can't suppress the summary), then log it once.
///
/// Spawned on the tokio runtime by `main` after the listeners are started.
pub(crate) fn spawn_watch(timeout_ms: u64) {
    tokio::spawn(async move {
        let step = std::time::Duration::from_millis(50);
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
        loop {
            let done = with(|s| {
                s.listeners
                    .values()
                    .all(|(_, st)| *st != Status::Pending)
            });
            if done || std::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(step).await;
        }
        let lines = with(|s| {
            if s.reported {
                return Vec::new();
            }
            s.reported = true;
            let entries: Vec<(&str, u16, Status)> = s
                .listeners
                .iter()
                .map(|(name, (port, st))| (*name, *port, *st))
                .collect();
            summarize(&entries)
        });
        for line in lines {
            glog!("{}", line);
        }
    });
}

/// The aggregate warning as lines, or empty when there is nothing to say.
///
/// **The same renderer the log uses, so a banner cannot disagree with it.** The
/// GUI needs this because the *cross-directory* case survives everything the
/// instance lock fixed: a copy launched from a different directory (a desktop
/// icon while a systemd unit serves from `/var/lib/ethernetgateway`, say) claims
/// its own lock quite legitimately, comes up with a full editor window, and
/// binds nothing -- which is exactly the "editing a config the serving copy
/// never re-reads" trap, reached the one way the lock cannot catch.
///
/// Answers `None` while any listener is still deciding, because a bind is
/// asynchronous and a banner drawn a frame too early would accuse a listener
/// that was about to succeed.
///
/// The `u64` is the server cycle (see `State::cycle`), so a caller can tell the
/// same words about a *new* attempt from the ones it has already shown.
pub(crate) fn aggregate_warning() -> Option<(u64, Vec<String>)> {
    let guard = state().lock().unwrap_or_else(|e| e.into_inner());
    // Asked under the same lock as the roster it is about: a separate
    // `any_pending()` call would answer about a state this one no longer sees.
    if guard.listeners.values().any(|(_, st)| *st == Status::Pending) {
        return None;
    }
    let entries: Vec<(&str, u16, Status)> = guard
        .listeners
        .iter()
        .map(|(name, (port, st))| (*name, *port, *st))
        .collect();
    Some((guard.cycle, summarize(&entries)))
}

/// Decide what to say about a set of listener outcomes.  Pure, so the wording
/// and — more importantly — the "when do we shout" rule are testable without
/// binding a socket.  Returns the lines to log, empty when all is well.
fn summarize(entries: &[(&str, u16, Status)]) -> Vec<String> {
    // Nothing configured is a legitimate setup (serial-only). main.rs already
    // warns about telnet+SSH being off; this module stays quiet.
    if entries.is_empty() {
        return Vec::new();
    }
    let failed: Vec<&(&str, u16, Status)> = entries
        .iter()
        .filter(|(_, _, st)| matches!(st, Status::Failed { .. }))
        .collect();
    if failed.is_empty() {
        return Vec::new();
    }
    let describe = |list: &[&(&str, u16, Status)]| {
        list.iter()
            .map(|(name, port, _)| format!("{} {}", name, port))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let any_in_use = failed
        .iter()
        .any(|(_, _, st)| matches!(st, Status::Failed { in_use: true }));

    let mut out = Vec::new();
    if failed.len() == entries.len() {
        out.push(format!(
            "WARNING: NONE of the {} configured listener(s) could bind ({}).",
            entries.len(),
            describe(&failed)
        ));
        out.push(
            "         This process is serving no network connections at all.".to_string(),
        );
        if any_in_use {
            // **A second copy started HERE can no longer reach a bind at all.**
            // The instance lock (see `instance`) catches it first and offers a
            // handover, so the old wording -- "another copy is almost certainly
            // still running" -- now names the one cause this can no longer be.
            // What is left is a copy running from a *different* directory, since
            // the lock is per-directory and two of them know nothing about each
            // other, or a program that is not this one at all.
            out.push(
                "         The ports are already in use, and it is not another copy started"
                    .to_string(),
            );
            out.push(
                "         from this directory — that one would have been offered a handover"
                    .to_string(),
            );
            out.push(
                "         instead of getting this far. Most likely a copy launched from a"
                    .to_string(),
            );
            out.push(
                "         DIFFERENT directory (each one claims its own), or another program"
                    .to_string(),
            );
            out.push(
                "         entirely. Whatever holds them is what your clients are reaching."
                    .to_string(),
            );
            out.extend(how_to_check());
        }
    } else {
        out.push(format!(
            "WARNING: {} of {} listeners could not bind ({}); the rest are running.",
            failed.len(),
            entries.len(),
            describe(&failed)
        ));
        if any_in_use {
            out.push(
                "         The port is already in use — check for a gateway started from a"
                    .to_string(),
            );
            out.push(
                "         different directory, or another program holding that port."
                    .to_string(),
            );
            out.extend(how_to_check());
        }
    }
    out
}

/// The commands that actually answer "who has my port?".
///
/// `windows` is a parameter rather than a `cfg!` so both wordings are reachable
/// from a test on any host — the alternative cost a red Windows CI run for a
/// message only Windows produces, which no amount of local testing would have
/// caught.
fn how_to_check_for(windows: bool) -> Vec<String> {
    if windows {
        vec![
            "         Check:  netstat -ano | findstr :<port>".to_string(),
            "         Find it:  tasklist | findstr ethernetgateway".to_string(),
            "         Stop it:  taskkill /IM ethernetgateway.exe   then start this one again."
                .to_string(),
        ]
    } else {
        vec![
            "         Check:  pgrep -a -x ethernetgateway     (and: ss -ltnp | grep <port>)"
                .to_string(),
            "         Stop it with:  pkill -x ethernetgateway   then start this one again."
                .to_string(),
        ]
    }
}

fn how_to_check() -> Vec<String> {
    how_to_check_for(cfg!(windows))
}

#[cfg(test)]
mod tests {
    /// **A banner drawn while a bind is still in flight would accuse a listener
    /// that was about to succeed.** The GUI polls this every second from the
    /// first frame, long before the listeners have reported, so "not yet" has to
    /// be a different answer from "all well".
    #[test]
    fn test_the_aggregate_warning_withholds_an_answer_while_a_bind_is_pending() {
        // **The registry is global, so this test has to hold the same guard the
        // other mutating test does.** Without it two tests race the map and one
        // reads a key the other just cleared -- which is exactly how CI failed
        // on macOS once while Linux passed on scheduling luck. See
        // `registry_guard`.
        let _guard = registry_guard();
        reset();
        expect("telnet", 2323);
        expect("ssh", 2222);
        assert!(aggregate_warning().is_none(), "two listeners pending");
        bound("telnet");
        assert!(aggregate_warning().is_none(), "one listener still pending");
        failed("ssh", &io::Error::from(io::ErrorKind::AddrInUse));
        let (cycle, lines) = aggregate_warning().expect("both have reported");
        // A partial failure is worth saying, and is not the total-failure text.
        let all = lines.join("\n");
        assert!(!all.is_empty(), "a failed listener must be reported");
        assert!(!all.contains("NONE of the"), "one of two bound: {all}");

        // **A restart is a new attempt, even when it fails identically.** The
        // desktop banner is dismissed by hand, so without this the same failure
        // after a Save and Restart would stay silent -- and that is the case
        // that matters, because the saved settings are why they restarted.
        reset();
        expect("telnet", 2323);
        expect("ssh", 2222);
        bound("telnet");
        failed("ssh", &io::Error::from(io::ErrorKind::AddrInUse));
        let (cycle2, lines2) = aggregate_warning().expect("reported again");
        assert_eq!(lines2, lines, "the same failure produces the same words");
        assert_ne!(cycle2, cycle, "...but a different cycle, or a dismissal sticks");

        reset();
        // Nothing configured: an answer, and an empty one.
        assert_eq!(aggregate_warning().map(|(_, l)| l.len()), Some(0));
    }

    use super::*;

    const IN_USE: Status = Status::Failed { in_use: true };
    const DENIED: Status = Status::Failed { in_use: false };

    #[test]
    fn test_silent_when_everything_bound_or_nothing_configured() {
        assert!(summarize(&[]).is_empty(), "serial-only setups must stay quiet");
        assert!(summarize(&[("telnet", 2323, Status::Bound)]).is_empty());
        assert!(summarize(&[
            ("telnet", 2323, Status::Bound),
            ("SSH", 2222, Status::Bound),
        ])
        .is_empty());
    }

    /// The case this module exists for: something else holding every port.
    ///
    /// **The cause it names had to change when the instance lock landed.** A
    /// second copy started from *this* directory can no longer reach a bind --
    /// `instance::acquire` catches it and offers a handover -- so blaming that
    /// would send the operator hunting for a process that cannot exist. What
    /// remains is a copy launched from a different directory, since the lock is
    /// per-directory, or a program that is not this one. The test pins the new
    /// cause *and* that the ruled-out one is explicitly ruled out, because a
    /// message that merely stopped mentioning it would read as vaguer rather
    /// than more accurate.
    #[test]
    fn test_all_listeners_in_use_names_the_other_copy() {
        let lines = summarize(&[
            ("Kermit", 2424, IN_USE),
            ("SSH", 2222, IN_USE),
            ("telnet", 2323, IN_USE),
            ("web", 8080, IN_USE),
        ]);
        let text = lines.join("\n");
        assert!(text.contains("NONE of the 4"), "{text}");
        assert!(text.contains("serving no network connections"), "{text}");
        // Rules out the case the lock now prevents...
        assert!(text.contains("not another copy started"), "{text}");
        assert!(text.contains("this directory"), "{text}");
        // ...and names the two that remain.
        assert!(text.contains("DIFFERENT directory"), "{text}");
        assert!(text.contains("another program"), "{text}");
        // Every failing port is named, so the operator can see which is which.
        for port in ["2222", "2323", "2424", "8080"] {
            assert!(text.contains(port), "port {port} missing from: {text}");
        }
        // And it says how to find the culprit — in this host's own idiom.
        assert!(text.contains("ethernetgateway"), "{text}");
    }

    /// Both platform wordings must name this program, so the operator can find
    /// the other copy.  Parameterised precisely because a `cfg!`-only version of
    /// this check passes on the host that wrote it and fails on the other one —
    /// which is exactly how this shipped red the first time.
    #[test]
    fn test_how_to_check_names_the_program_on_both_platforms() {
        for windows in [false, true] {
            let lines = how_to_check_for(windows);
            let text = lines.join("\n");
            assert!(
                text.contains("ethernetgateway"),
                "windows={windows}: {text}"
            );
            assert!(!lines.is_empty());
            // Each platform names a way to look and a way to stop it.
            let (look, stop) = if windows {
                ("netstat", "taskkill")
            } else {
                ("pgrep", "pkill")
            };
            assert!(text.contains(look), "windows={windows}: {text}");
            assert!(text.contains(stop), "windows={windows}: {text}");
        }
    }

    /// A total failure that is NOT address-in-use (say, ports below 1024
    /// without root) must still warn, but must not blame a second copy.
    #[test]
    fn test_all_failed_but_not_in_use_does_not_blame_another_copy() {
        let lines = summarize(&[("telnet", 23, DENIED), ("SSH", 22, DENIED)]);
        let text = lines.join("\n");
        assert!(text.contains("NONE of the 2"), "{text}");
        assert!(!text.contains("another copy"), "{text}");
        assert!(!text.contains("pkill"), "{text}");
    }

    #[test]
    fn test_partial_failure_is_reported_without_the_shout() {
        let lines = summarize(&[
            ("telnet", 2323, Status::Bound),
            ("web", 8080, IN_USE),
        ]);
        let text = lines.join("\n");
        assert!(text.contains("1 of 2"), "{text}");
        assert!(text.contains("web 8080"), "{text}");
        assert!(text.contains("the rest are running"), "{text}");
        assert!(!text.contains("NONE"), "{text}");
    }

    /// Serialises the tests that drive the process-wide registry.
    ///
    /// `reset()` / `expect()` / `bound()` / `failed()` all mutate one global
    /// `State`, and cargo runs tests on parallel threads — so without this, one
    /// test's `reset()` lands between another's `expect()` and its read, and the
    /// read panics with "no entry found for key". That is precisely how CI failed
    /// on **macOS** at `9f72b85` (1582 passed, 1 failed) while ubuntu and Windows
    /// passed on scheduling luck, which is the trap the release checklist warns
    /// about: a local run cannot see it.
    ///
    /// Production has no such race — `reset()` and `expect()` are called from the
    /// single startup path, synchronously, before any listener task is spawned.
    /// This is test isolation, not a fix to the registry's own locking.
    fn registry_guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn test_registry_records_and_resets() {
        let _guard = registry_guard();
        reset();
        expect("telnet", 2323);
        expect("web", 8080);
        with(|s| {
            assert_eq!(s.listeners.len(), 2);
            assert_eq!(s.listeners["telnet"], (2323, Status::Pending));
        });
        bound("telnet");
        failed(
            "web",
            &io::Error::new(io::ErrorKind::AddrInUse, "address in use"),
        );
        with(|s| {
            assert_eq!(s.listeners["telnet"], (2323, Status::Bound));
            assert_eq!(s.listeners["web"], (8080, IN_USE));
        });
        // A restart starts from a clean slate, or the previous cycle's failures
        // would be re-reported forever.
        reset();
        with(|s| {
            assert!(s.listeners.is_empty());
            assert!(!s.reported);
        });
    }

    #[test]
    fn test_a_late_failure_supersedes_an_optimistic_bound() {
        let _guard = registry_guard();
        reset();
        expect("SSH", 2222);
        bound("SSH");
        failed(
            "SSH",
            &io::Error::new(io::ErrorKind::AddrInUse, "address in use"),
        );
        with(|s| assert_eq!(s.listeners["SSH"], (2222, IN_USE)));
        reset();
    }
}
