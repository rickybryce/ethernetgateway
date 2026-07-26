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
            out.push(
                "         The ports are already in use — another copy of the gateway is"
                    .to_string(),
            );
            out.push(
                "         almost certainly still running and holding them, which means"
                    .to_string(),
            );
            out.push(
                "         anything you connect to is being served by that older copy."
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
                "         The port is already in use — check for another copy of the gateway."
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

    /// The case this module exists for: a second copy of the gateway holding
    /// every port.  The message has to name the cause, not just the symptom.
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
        assert!(text.contains("another copy of the gateway"), "{text}");
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

    #[test]
    fn test_registry_records_and_resets() {
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
