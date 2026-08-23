//! Problems an operator can fix, and the offer to fix them.
//!
//! Some failures are not the gateway's to decide. The one this exists for is a
//! master's SSH host key changing: a slave pins the key on first contact and
//! refuses a changed one, because a key that changed because the master was
//! reinstalled and a key that changed because somebody is sitting in the middle
//! **look identical from here** — which is the entire point of pinning. So the
//! gateway cannot silently re-pin, and until now it printed a line into the log
//! telling the operator to edit `gateway_hosts` by hand. On a slave in another
//! room, running headless, reached over telnet from a C64, that is not a fix.
//!
//! # What this is
//!
//! A small registry of *pending, resolvable* problems. Something that hits one
//! reports it; the three configuration surfaces show it and offer the remedy;
//! taking the remedy runs it and clears the entry. Nothing here fixes anything
//! on its own — every entry needs a human to say yes, and the security-relevant
//! ones say what they mean before asking.
//!
//! # Why a registry rather than a log line
//!
//! Three reasons, all learned elsewhere in this project. A log line is invisible
//! on a surface that has no log (the telnet menus, a C64). It cannot be
//! *withdrawn* when the problem goes away, so an operator reading it hours later
//! cannot tell whether it still applies — the [`clear`] path is what makes this
//! honest. And the same problem hit fifty times in a reconnect loop is fifty
//! lines saying one thing; keyed entries collapse to one.
//!
//! # Deliberately not a general error log
//!
//! An entry earns its place by having an **action attached**. Anything the
//! operator cannot act on from a configuration screen belongs in the log, where
//! the rest of the diagnostics already are.

use std::sync::Mutex;

/// A problem waiting for an operator's decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    /// A master's SSH host key does not match the one this slave pinned.
    ///
    /// Carries the host and port because the remedy is per-host: forgetting one
    /// master's key must not forget another's.
    MasterHostKeyChanged { host: String, port: u16 },
}

impl Problem {
    /// The key an entry is stored under, so the same problem reported on every
    /// reconnect attempt is one entry rather than hundreds.
    pub fn id(&self) -> String {
        match self {
            Problem::MasterHostKeyChanged { host, port } => {
                format!("hostkey:{}:{}", host, port)
            }
        }
    }

    /// One line naming the problem, for a list on a 40-column screen.
    ///
    /// Kept inside 36 characters so it fits a C64 with the two-space indent the
    /// menus use — checked by `test_every_problem_fits_a_narrow_screen`.
    pub fn title(&self) -> String {
        match self {
            Problem::MasterHostKeyChanged { .. } => "Master host key changed".to_string(),
        }
    }

    /// What happened and what it might mean, as lines already fitted to a
    /// narrow screen.
    ///
    /// **The MITM sentence is not optional.** The operator is about to discard
    /// the evidence that something is wrong, and the honest presentation says so
    /// before offering the button — the same posture as the `cpm_boot_writable`
    /// argument, where the question is which failure an operator would rather
    /// have and they are told which they are choosing.
    pub fn explain(&self) -> Vec<String> {
        match self {
            Problem::MasterHostKeyChanged { host, port } => vec![
                format!("The master at {}:{}", host, port),
                "presents a different SSH host key".to_string(),
                "than the one this slave pinned.".to_string(),
                String::new(),
                "This is normal if the master was".to_string(),
                "reinstalled or its key was".to_string(),
                "regenerated. It is also what a".to_string(),
                "man-in-the-middle looks like, and".to_string(),
                "nothing here can tell the two".to_string(),
                "apart -- that is what pinning is".to_string(),
                "for.".to_string(),
                String::new(),
                "Only clear it if you know why the".to_string(),
                "key changed.".to_string(),
            ],
        }
    }

    /// What the remedy does, in the imperative, for a button or a menu row.
    pub fn action(&self) -> String {
        match self {
            Problem::MasterHostKeyChanged { .. } => {
                "Forget the old key and pin the new one".to_string()
            }
        }
    }

    /// Do it. `Ok` carries a sentence for the surface to show.
    ///
    /// Runs the remedy and nothing else: the entry is removed by [`resolve`], so
    /// a remedy that fails leaves the problem listed rather than silently gone.
    fn apply(&self) -> Result<String, String> {
        match self {
            Problem::MasterHostKeyChanged { host, port } => {
                match crate::telnet::forget_known_host(host, *port) {
                    // Re-pinning happens on the next connection, not here --
                    // saying "pinned" now would claim something that has not
                    // happened yet.
                    Ok(true) => Ok(format!(
                        "Forgot the pinned key for {}:{}. The next connection will pin \
                         whatever it presents.",
                        host, port
                    )),
                    Ok(false) => Ok(format!(
                        "Nothing was pinned for {}:{} -- it will pin on the next connection.",
                        host, port
                    )),
                    Err(e) => Err(format!("Could not update gateway_hosts: {}", e)),
                }
            }
        }
    }
}

/// The pending problems, keyed by [`Problem::id`].
///
/// A `Vec` rather than a map so the order an operator sees is the order the
/// problems appeared, which is the order they are likely to matter in. The list
/// is short by construction — one entry per distinct problem — so the linear
/// scans cost nothing.
static PENDING: Mutex<Vec<Problem>> = Mutex::new(Vec::new());

fn lock() -> std::sync::MutexGuard<'static, Vec<Problem>> {
    PENDING.lock().unwrap_or_else(|e| e.into_inner())
}

/// Note a problem, or leave the existing entry alone if it is already listed.
///
/// Called from a reconnect loop, so it must be cheap and idempotent: a slave
/// retrying a master every few seconds reports the same problem indefinitely.
pub fn report(problem: Problem) {
    let mut pending = lock();
    if !pending.iter().any(|p| p.id() == problem.id()) {
        crate::glog!("Resolvable problem: {}", problem.title());
        pending.push(problem);
    }
}

/// Withdraw a problem because it no longer applies.
///
/// **The half that makes the list trustworthy.** A master whose key was put back
/// by hand, or a slave whose relay is now connecting, must not go on being
/// listed — an entry nobody can clear is worse than no entry, because the
/// operator learns to ignore the screen.
pub fn clear(id: &str) {
    let mut pending = lock();
    let before = pending.len();
    pending.retain(|p| p.id() != id);
    if pending.len() != before {
        crate::glog!("Resolvable problem cleared: {}", id);
    }
}

/// Everything currently pending, in the order it appeared.
pub fn list() -> Vec<Problem> {
    lock().clone()
}

/// Is there anything to resolve?
///
/// Its own function because it is what the surfaces gate on, and three copies of
/// `!list().is_empty()` would each clone the list to answer a yes/no.
pub fn any() -> bool {
    !lock().is_empty()
}

/// Apply one problem's remedy and, if it worked, forget the problem.
///
/// The entry survives a failed remedy on purpose: the operator pressed the
/// button and it did not work, so the offer must still be there.
pub fn resolve(id: &str) -> Result<String, String> {
    let problem = lock().iter().find(|p| p.id() == id).cloned();
    let Some(problem) = problem else {
        return Err("That problem is no longer listed.".to_string());
    };
    let outcome = problem.apply()?;
    clear(id);
    crate::glog!("Resolved: {} -- {}", problem.title(), outcome);
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_problem() -> Problem {
        Problem::MasterHostKeyChanged { host: "192.168.1.178".to_string(), port: 2222 }
    }

    /// The registry's whole contract: report once however many times it is
    /// reported, list in order, clear by id.
    #[test]
    fn test_a_repeated_report_is_one_entry() {
        let p = Problem::MasterHostKeyChanged { host: "10.9.9.9".to_string(), port: 2222 };
        clear(&p.id());
        // A reconnect loop reports the same failure every few seconds.
        for _ in 0..50 {
            report(p.clone());
        }
        assert_eq!(
            list().iter().filter(|q| q.id() == p.id()).count(),
            1,
            "fifty reports of one problem must be one entry"
        );
        clear(&p.id());
        assert!(!list().iter().any(|q| q.id() == p.id()), "clear must withdraw it");
    }

    /// Two masters are two problems: the remedy is per-host, so the entries must
    /// not collapse.
    #[test]
    fn test_two_hosts_are_two_problems() {
        let a = Problem::MasterHostKeyChanged { host: "10.1.1.1".to_string(), port: 2222 };
        let b = Problem::MasterHostKeyChanged { host: "10.1.1.2".to_string(), port: 2222 };
        assert_ne!(a.id(), b.id());
        // And the port is part of the identity — one host can be two masters.
        let c = Problem::MasterHostKeyChanged { host: "10.1.1.1".to_string(), port: 2223 };
        assert_ne!(a.id(), c.id());
    }

    /// Resolving something that is not listed is an error, not a silent success:
    /// two operators on two surfaces can press the same button.
    #[test]
    fn test_resolving_an_unlisted_problem_says_so() {
        let err = resolve("hostkey:nowhere:1").expect_err("nothing to resolve");
        assert!(err.contains("no longer listed"), "{err}");
    }

    /// **The explanation must say what clearing it costs.** An operator about to
    /// discard the evidence that something is wrong has to be told that is what
    /// they are doing.
    #[test]
    fn test_the_host_key_explanation_names_the_risk() {
        let text = key_problem().explain().join(" ");
        assert!(text.contains("man-in-the-middle"), "the risk must be named: {text}");
        assert!(text.contains("reinstalled"), "and the innocent explanation: {text}");
        assert!(text.contains("192.168.1.178:2222"), "and which master: {text}");
        // The action says what it does, in the imperative.
        assert!(key_problem().action().starts_with("Forget"), "{}", key_problem().action());
    }

    /// Every line of every problem has to fit a 40-column screen with the
    /// two-space indent the menus use. Iterated over the real variants so a
    /// problem added later is measured too.
    #[test]
    fn test_every_problem_fits_a_narrow_screen() {
        for p in [key_problem()] {
            assert!(
                p.title().chars().count() <= 36,
                "title {:?} is {} columns",
                p.title(),
                p.title().chars().count()
            );
            for line in p.explain() {
                assert!(
                    line.chars().count() <= 36,
                    "explain line {line:?} is {} columns",
                    line.chars().count()
                );
            }
            // The action is drawn on a wider surface (a button, a menu row with
            // its own key) so it gets the 80-column budget.
            assert!(p.action().chars().count() <= 60, "action {:?} is too long", p.action());
        }
    }
}
