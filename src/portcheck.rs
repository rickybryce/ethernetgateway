//! Is a listener that bound successfully actually *reachable*?
//!
//! [`crate::bindwatch`] answers the first half of "why can nobody connect" — it
//! knows whether each listener took its port, and names the case where a second
//! copy of the gateway is holding it. This answers the other half: the port was
//! taken, and something is still refusing the connection.
//!
//! # The evidence is asymmetric, and only one direction is reported
//!
//! The probe connects to one of this machine's own non-loopback addresses. What
//! that proves depends entirely on the platform, and it is not symmetric:
//!
//! * **A failed probe is strong evidence.** If the gateway cannot reach its own
//!   listener at its own address, something is blocking it — on every platform.
//!   That is worth telling the operator, in red.
//!
//! * **A successful probe is no evidence on any of the three platforms.** On
//!   **Linux** a connection to your own non-loopback address is routed over the
//!   loopback interface (`ip route get <own address>` answers `local ... dev
//!   lo`), and the common firewalls accept `lo` before any port rule -- ufw's
//!   `-i lo -j ACCEPT` in `before.rules`, firewalld's `iifname "lo" accept`, and
//!   most hand-written rulesets -- so a port they block for the network still
//!   answers here. Only a ruleset that does *not* exempt loopback is caught,
//!   and then as a failure. On **Windows** the Filtering Platform
//!   exempts traffic a machine sends to its own address, so a port blocked by
//!   Defender probes as reachable. On **macOS** the application firewall is
//!   per-application rather than per-port and does not filter self-traffic
//!   either. On those two, a pass would be a false all-clear on the platform
//!   where a blocking firewall is *most* likely — Defender is on by default and prompts
//!   the first time a listener appears.
//!
//! So a pass says **nothing at all** here: [`Reach::Answered`] is recorded but no
//! surface turns it into "not firewalled". Saying "reachable" when we cannot know
//! is the failure mode this module is shaped to avoid, and it is the one an
//! operator would act on — they would go looking at their router while Defender
//! quietly dropped every connection.
//!
//! None of this sees past the machine. A router that is not forwarding a port,
//! or an upstream firewall, is invisible from in here; answering *that* needs a
//! probe from somewhere else, which means a third party, no offline operation,
//! and the operator's address and port leaving the machine. Not worth it, and
//! not this project's posture.
//!
//! # It costs a real connection
//!
//! There is no way to test a port without connecting to it, so each probe is an
//! ordinary inbound connection to our own listener: a session slot for as long as
//! it takes to close, and a line in the log. That is why this runs **on demand**
//! and never on a timer — a per-frame check would fill the log with the operator
//! connecting to themselves.

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use crate::logger::glog;

/// How long to wait for our own machine to answer.
///
/// Generous for a connection that never leaves the host, and deliberately short
/// enough that checking four listeners cannot hang a configuration screen. A
/// firewall that DROPs rather than REJECTs gives no answer at all, so the
/// timeout *is* the detection in the commonest case.
const PROBE_TIMEOUT: Duration = Duration::from_millis(1200);

/// What one probe found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reach {
    /// The listener answered. **Reported to nobody** — see the module comment.
    Answered,
    /// Nothing answered. The strong direction: something is blocking this port.
    Blocked {
        /// `true` when the connection was refused outright rather than timing
        /// out. A REJECT and a DROP look different to the operator's firewall
        /// tooling, so the distinction is kept.
        refused: bool,
    },
    /// The probe could not be made, so nothing is known.
    ///
    /// Not a failure of the port: no usable address for this host, or the
    /// listener never bound in the first place. Kept apart from `Blocked`
    /// because "we could not look" must never render as "it is firewalled".
    Unknown(String),
}

impl Reach {
    /// Should a surface mark this port red?
    ///
    /// Only the strong direction. `Answered` and `Unknown` both mean "say
    /// nothing", for different reasons: one is evidence we do not trust, the
    /// other is no evidence at all.
    pub fn is_blocked(&self) -> bool {
        matches!(self, Reach::Blocked { .. })
    }

    /// Was the probe unable to say anything at all?
    ///
    /// The third state, and the one a two-way rendering loses: a check that
    /// never reached a connection attempt is neither a block nor an answer.
    pub fn is_untested(&self) -> bool {
        matches!(self, Reach::Unknown(_))
    }

    /// How this result reads in a results list.
    ///
    /// **Here rather than in each surface**, because the desktop popup and the
    /// web modal print the same three phrases and a surface that lost the third
    /// one would report "answered on this machine" for a probe that never ran --
    /// an all-clear this check did not earn. Callers pick the colour from
    /// [`Reach::is_blocked`] and [`Reach::is_untested`]; the words are one
    /// implementation.
    pub fn verdict_phrase(&self) -> String {
        match self {
            Reach::Blocked { .. } => "did not answer".to_string(),
            Reach::Unknown(why) => format!("could not be tested — {why}"),
            Reach::Answered => "answered on this machine".to_string(),
        }
    }

    /// What to show when hovering, for a surface that has room.
    pub fn hover(&self, port: u16) -> Option<String> {
        match self {
            Reach::Blocked { refused } => Some(format!(
                "Port {port} is bound, but this machine could not connect to it \
                 at its own network address — the connection was {}. Something \
                 on this machine is blocking it: a host firewall, or security \
                 software. Nothing here can see past this machine, so a router \
                 that is not forwarding the port looks fine from in here.",
                if *refused { "refused" } else { "not answered before the timeout" }
            )),
            _ => None,
        }
    }
}

/// One row of "what this test actually proves", per platform.
///
/// **In one table because two surfaces render it**, and a capability claim that
/// drifted between the desktop and the web page would be worse than not making
/// it. The values are the module comment's argument in a form an operator can
/// read at the moment they are looking at a result.
pub struct PlatformFact {
    pub question: &'static str,
    pub linux: &'static str,
    pub windows: &'static str,
    pub macos: &'static str,
}

/// What a port test can and cannot tell you, by platform.
///
/// The middle row is the one that makes the feature worth having at all: it
/// never cries wolf anywhere. The first row is the one that stops an operator
/// on Windows reading silence as an all-clear.
pub const WHAT_THE_TEST_PROVES: &[PlatformFact] = &[
    PlatformFact {
        question: "Finds a firewall on this machine",
        // A connection to your own non-loopback address is routed over `lo`,
        // and ufw, firewalld and most hand rulesets accept `lo` before any port
        // rule -- so only a ruleset that does not exempt loopback is caught.
        // Measured: `ip route get <own LAN address>` -> `local ... dev lo`.
        linux: "rarely",
        // The Filtering Platform exempts traffic a machine sends to its own
        // address, so a port Defender is blocking answers anyway.
        windows: "no",
        // The application firewall is per-application, not per-port, and does
        // not filter self-traffic.
        macos: "no",
    },
    PlatformFact {
        question: "Can raise a false alarm",
        linux: "no",
        windows: "no",
        macos: "no",
    },
    PlatformFact {
        question: "Sees a router not forwarding",
        linux: "no",
        windows: "no",
        macos: "no",
    },
];

/// The platform this build is running on, as the table names it.
///
/// `None` for anything not in the table, which is honest rather than guessing
/// a column: a BSD is not Linux for this purpose even though the probe may well
/// behave the same.
pub fn this_platform() -> Option<&'static str> {
    if cfg!(target_os = "linux") {
        Some("Linux")
    } else if cfg!(target_os = "windows") {
        Some("Windows")
    } else if cfg!(target_os = "macos") {
        Some("macOS")
    } else {
        None
    }
}

impl PlatformFact {
    /// This row's answer for the platform we are running on.
    pub fn here(&self) -> Option<&'static str> {
        match this_platform()? {
            "Linux" => Some(self.linux),
            "Windows" => Some(self.windows),
            "macOS" => Some(self.macos),
            _ => None,
        }
    }
}

#[derive(Default)]
struct State {
    /// listener name -> (port, what the last probe found)
    results: BTreeMap<String, (u16, Reach)>,
    /// Has a check ever been run this cycle?  Distinguishes "nothing is
    /// blocked" from "nobody has looked", which are different answers.
    ran: bool,
    /// Which server cycle this table belongs to.
    ///
    /// A check started just before a restart can still be inside
    /// `connect_timeout` when the new cycle calls [`reset`] -- the desktop runs
    /// it on a plain thread, which the runtime shutdown does not reach. Without
    /// this it would then store the *previous* cycle's ports over a cleared
    /// table, reddening a port the new cycle never tested: precisely the stale
    /// result `reset` exists to prevent.
    cycle: u64,
    /// When the last check ran, so a surface can say how old its answer is.
    ///
    /// A red label is not live -- nothing here polls -- so it means "the last
    /// time anybody looked, this did not answer". Fix the firewall and it stays
    /// red until the next check. Saying when is cheaper than pretending
    /// otherwise.
    ran_at: Option<std::time::Instant>,
}

fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(State::default()))
}

fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    let mut guard = state().lock().unwrap_or_else(|e| e.into_inner());
    f(&mut guard)
}

/// What the last check found for one listener, if it has been run.
pub fn result_of(name: &str) -> Option<(u16, Reach)> {
    with(|s| s.results.get(name).cloned())
}

/// Every result, for a surface that lists them.
pub fn results() -> Vec<(String, u16, Reach)> {
    with(|s| s.results.iter().map(|(n, (p, r))| (n.clone(), *p, r.clone())).collect())
}

/// Has a check been run since this server cycle started?
pub fn has_run() -> bool {
    with(|s| s.ran)
}

/// How long ago the last check ran.
pub fn age() -> Option<Duration> {
    with(|s| s.ran_at.map(|t| t.elapsed()))
}

/// Clear the slate for a new server cycle.
///
/// **Results do not survive a restart, and letting them would be a lie with a
/// number attached.** The ports themselves can change across a restart -- that
/// is one of the things a restart is *for* -- so a stale `blocked` would redden
/// a port that had never been tested. `bindwatch` is reset beside this for the
/// same reason.
pub fn reset() {
    with(|s| {
        s.results.clear();
        s.ran = false;
        s.ran_at = None;
        s.cycle = s.cycle.wrapping_add(1);
    });
}

/// The current server cycle, for a caller that will store a result later.
pub fn cycle() -> u64 {
    with(|s| s.cycle)
}

/// Check every bound listener, and remember what was found -- unless the
/// server restarted after `cycle` was taken, in which case the answers belong
/// to listeners that no longer exist and are thrown away.
///
/// **There is no version of this that does not name its cycle.** There was:
/// `run_check()`, which the web page and the telnet `F` key called, and which
/// stored unconditionally. Both run the probes on a blocking thread that a
/// restart does not stop -- the web page's `POST /save` is itself a
/// `spawn_blocking`, and a Save and Restart from another surface can land
/// while it is still inside four connect timeouts -- so their check could
/// store the old cycle's answers (refused, because those listeners had just
/// closed) over the new cycle's cleared table, and redden ports the new cycle
/// never tested. The guard existed for the desktop and the startup check and
/// was not used by the other two; taking the unguarded entry point away is
/// what stops a fifth caller repeating that.
///
/// Take `cycle` **before** handing the work to another thread, so the window
/// it guards includes the hand-off.
///
/// Blocking, and meant to be: callers run it off their event loop. Returns the
/// number found blocked, so a caller can say something without re-reading the
/// table.
pub fn run_check_for_cycle(cycle: u64) -> usize {
    run_check_inner(cycle)
}

/// Run a check once the listeners have settled, in the background.
///
/// **At startup, because the operator should not have to know to ask.** The
/// red labels are the whole signal, and a signal nobody goes looking for is no
/// signal -- somebody whose port is blocked is, by definition, not getting the
/// connection that would prompt them to investigate.
///
/// It waits for the bind roster to stop saying `Pending` rather than sleeping a
/// fixed time, because probing a listener that has not bound yet would report
/// it blocked and be wrong in the one direction this module is careful about.
pub fn spawn_startup_check(settle_ms: u64) {
    // The cycle this check belongs to, taken *now*: a Save and Restart that
    // lands while it is still probing must not have the old cycle's answers --
    // refused, because its listeners just closed -- stored over the new one's.
    // `spawn_blocking` is not cancelled by the runtime shutdown, so this is
    // the only thing that stops it.
    let mine = cycle();
    tokio::spawn(async move {
        let step = Duration::from_millis(50);
        let deadline = std::time::Instant::now() + Duration::from_millis(settle_ms);
        while std::time::Instant::now() < deadline {
            let pending = crate::bindwatch::any_pending();
            if !pending {
                break;
            }
            tokio::time::sleep(step).await;
        }
        // Off the runtime: `run_check` blocks on connect timeouts.
        let _ = tokio::task::spawn_blocking(move || run_check_for_cycle(mine)).await;
    });
}

/// The ports a check would look at: the ones that really bound.
///
/// From [`crate::bindwatch`] rather than from the config, for the same reason
/// the desktop's screen button reads it — a listener that failed to bind is not
/// a firewall problem and must not be reported as one.
pub fn bound_listeners() -> Vec<(String, u16)> {
    crate::bindwatch::bound_listeners()
}

/// Probe one address, returning what it found.
///
/// Split out so the decision can be tested against a socket the test controls,
/// rather than against whatever the developer's own firewall happens to do.
pub fn probe_addr(addr: SocketAddr, timeout: Duration) -> Reach {
    match TcpStream::connect_timeout(&addr, timeout) {
        Ok(stream) => {
            // Closed at once.  We are talking to our own listener and have
            // nothing to say to it; holding the connection open would keep a
            // session slot for no reason.
            let _ = stream.shutdown(std::net::Shutdown::Both);
            Reach::Answered
        }
        Err(e) => match e.kind() {
            ErrorKind::ConnectionRefused => Reach::Blocked { refused: true },
            ErrorKind::TimedOut | ErrorKind::WouldBlock => Reach::Blocked { refused: false },
            // Anything else is a fault in the asking, not an answer about the
            // port: no route, an address we cannot use, a permission problem.
            _ => Reach::Unknown(e.to_string()),
        },
    }
}

/// Replace the remembered results with a fresh set, returning how many were
/// blocked.
///
/// **Replace, never merge.** A port that was blocked and now answers has to
/// stop being red — the operator opened their firewall and pressed the button
/// again, and a leftover entry would tell them it had not worked. Every surface
/// reads `is_blocked` straight from this table, so clearing here is what turns
/// the labels and the More button back to their ordinary colour.
/// Refuses to write if `cycle` is not the current one.
fn store_for(found: Vec<(String, u16, Reach)>, cycle: u64) -> usize {
    let blocked = found.iter().filter(|(_, _, r)| r.is_blocked()).count();
    with(|s| {
        if cycle != s.cycle {
            // The server restarted while this check was in flight.  Its ports
            // belong to a machine that no longer exists.
            return;
        }
        s.results = found.into_iter().map(|(n, p, r)| (n, (p, r))).collect();
        s.ran = true;
        s.ran_at = Some(std::time::Instant::now());
    });
    blocked
}

fn run_check_inner(cycle: u64) -> usize {
    let host = crate::serial::primary_local_ip();
    let listeners = bound_listeners();
    // **Loopback is not an address we can learn anything from.**
    // `primary_local_ip` falls back to 127.0.0.1 when the host has no
    // non-loopback IPv4 -- an IPv6-only machine, or one that started the gateway
    // before DHCP finished, which the startup check can easily beat. Probing
    // that answers unconditionally, and every surface would then say "every
    // bound port answered" on the strength of a test that looked under the wrong
    // lamp. "We could not look" is a different answer from "nothing is blocked",
    // and this module exists to keep them apart.
    let usable_host = host
        .parse::<std::net::IpAddr>()
        .map(|ip| !ip.is_loopback())
        .unwrap_or(true);
    if !usable_host {
        glog!(
            "Port check: this machine has no network address of its own yet (only {host}), \
             so there is nothing to test against. Try again once it has one."
        );
        store_for(
            listeners
                .into_iter()
                .map(|(n, p)| {
                    (n, p, Reach::Unknown("no non-loopback address on this host".into()))
                })
                .collect(),
            cycle,
        );
        return 0;
    }
    if listeners.is_empty() {
        with(|s| {
            if cycle != s.cycle {
                return;
            }
            s.results.clear();
            s.ran = true;
            s.ran_at = Some(std::time::Instant::now());
        });
        glog!("Port check: no listener is bound, so there is nothing to test.");
        return 0;
    }

    glog!(
        "Port check: connecting to this gateway's own {} listener(s) at {host} — \
         each shows up below as a connection from ourselves.",
        listeners.len()
    );

    let mut found = Vec::new();
    for (name, port) in listeners {
        // Resolved rather than parsed, so an IPv6 host address works too.
        let reach = match (host.as_str(), port).to_socket_addrs() {
            Ok(mut addrs) => match addrs.next() {
                Some(addr) => probe_addr(addr, PROBE_TIMEOUT),
                None => Reach::Unknown(format!("no address for {host}")),
            },
            Err(e) => Reach::Unknown(e.to_string()),
        };
        if reach.is_blocked() {
            glog!(
                "Port check: {name} is bound to port {port} but did not answer at {host} — \
                 something on this machine is blocking it."
            );
        }
        found.push((name, port, reach));
    }

    let blocked = store_for(found, cycle);
    if blocked == 0 {
        // Deliberately not "all ports are open".  A pass proves next to nothing
        // on any platform -- self-traffic rides loopback on Linux, which the
        // usual firewalls accept, and skips the firewall on Windows and macOS.
        glog!(
            "Port check: every bound listener answered on this machine. That is not \
             evidence the firewall is open: a connection to your own address skips the \
             firewall's port rules (on Linux it arrives on loopback, which firewalls \
             accept), and no check here can see a router that is not forwarding a port."
        );
    }
    blocked
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// **A live socket answers, and a dead one is reported blocked.**
    ///
    /// Against a listener the test owns rather than against the developer's own
    /// machine, so the result does not depend on whatever firewall happens to be
    /// running where this is built.
    #[test]
    fn test_a_probe_tells_a_live_port_from_a_dead_one() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
        let addr = listener.local_addr().unwrap();
        assert_eq!(probe_addr(addr, Duration::from_millis(500)), Reach::Answered);

        // Dropping the listener frees the port; nothing is listening now.
        drop(listener);
        let dead = probe_addr(addr, Duration::from_millis(500));
        assert!(
            dead.is_blocked() || matches!(dead, Reach::Unknown(_)),
            "a closed port must not read as answered: {dead:?}"
        );
    }

    /// **Only the strong direction is reported.**
    ///
    /// The whole shape of this module: a pass is not evidence, because on
    /// Windows and macOS a self-connection does not meet the firewall at all.
    /// A surface that rendered `Answered` as "not firewalled" would be telling a
    /// Windows operator their blocked port is fine.
    #[test]
    fn test_a_pass_is_never_reported_as_open() {
        assert!(!Reach::Answered.is_blocked());
        assert!(Reach::Answered.hover(2323).is_none(), "a pass says nothing");

        // "We could not look" is not "it is blocked" either.
        let unknown = Reach::Unknown("no route".into());
        assert!(!unknown.is_blocked());
        assert!(unknown.hover(2323).is_none());

        // Only this one speaks.
        for refused in [true, false] {
            let blocked = Reach::Blocked { refused };
            assert!(blocked.is_blocked());
            let hover = blocked.hover(2323).expect("a blocked port explains itself");
            assert!(hover.contains("2323"), "{hover}");
            assert!(
                hover.contains("this machine"),
                "it must scope the claim to what we can see: {hover}"
            );
        }
    }

    /// **A result that could not be got is not a pass.**
    ///
    /// Both popups list every listener with a phrase beside it. They rendered
    /// two states for a while, so an `Unknown` -- no address for this host, no
    /// route -- came out as "answered on this machine": the one sentence this
    /// module exists to avoid printing when it does not know.
    #[test]
    fn test_an_untested_port_never_reads_as_an_answer() {
        let answered = Reach::Answered.verdict_phrase();
        let untested = Reach::Unknown("no route".into()).verdict_phrase();
        let blocked = Reach::Blocked { refused: true }.verdict_phrase();

        assert_ne!(untested, answered);
        assert_ne!(untested, blocked);
        assert!(untested.contains("no route"), "it must say why: {untested}");

        // And the three predicates partition the three states, so a surface
        // choosing a colour cannot land on the wrong one.
        assert!(!Reach::Answered.is_blocked() && !Reach::Answered.is_untested());
        assert!(Reach::Unknown(String::new()).is_untested());
        assert!(!Reach::Unknown(String::new()).is_blocked());
        assert!(Reach::Blocked { refused: false }.is_blocked());
        assert!(!Reach::Blocked { refused: false }.is_untested());
    }

    /// **The capability table answers for the platform this build is on.**
    ///
    /// Both graphical surfaces and the manual render this table, so it is the
    /// one place a claim about what the test proves can be made — and the row
    /// that matters is the running platform's. The detection row has **no**
    /// `yes`: Linux said so until it was measured -- a self-connection there is
    /// routed over `lo`, which ufw and firewalld accept before any port rule --
    /// so it is `rarely`, and every surface treats anything but `yes` as "a
    /// pass is not evidence".
    #[test]
    fn test_the_capability_table_answers_for_this_platform() {
        let facts = WHAT_THE_TEST_PROVES;
        assert!(facts.len() >= 3, "the table lost a row");

        // Every row answers for every column, and for here.
        for f in facts {
            for v in [f.linux, f.windows, f.macos] {
                assert!(
                    v == "yes" || v == "no" || v == "rarely",
                    "{}: {v:?} is not an answer",
                    f.question
                );
            }
            if this_platform().is_some() {
                assert!(f.here().is_some(), "{}: no answer for this platform", f.question);
            }
        }

        // The detection row: yes nowhere.  If this ever flips, it is because
        // somebody measured something new — and both popups say so from this
        // table, so they follow automatically (the manual is hand-written).
        let detect = &facts[0];
        assert_eq!(detect.linux, "rarely", "self-traffic rides `lo`, which firewalls accept");
        assert_eq!(detect.windows, "no", "self-connections skip the Windows firewall");
        assert_eq!(detect.macos, "no", "the macOS application firewall is per-application");

        // And the promise that makes the feature worth shipping at all: it
        // never cries wolf, anywhere.
        let false_alarm = facts.iter().find(|f| f.question.contains("false alarm")).expect("row");
        for v in [false_alarm.linux, false_alarm.windows, false_alarm.macos] {
            assert_eq!(v, "no", "a red label must always mean something real");
        }

        #[cfg(target_os = "linux")]
        assert_eq!(detect.here(), Some("rarely"));
    }

    /// **A passing re-check clears the red.**
    ///
    /// The operator opens their firewall and presses the button again. If a
    /// leftover entry survived, every surface would still be red and would be
    /// telling them their fix had not worked — the labels and the `More...`
    /// button are drawn straight from this table.
    #[test]
    fn test_a_second_check_replaces_the_first_rather_than_merging() {
        let _g = tests_lock();
        reset();

        assert_eq!(
            store_for(vec![
                ("telnet".into(), 2323, Reach::Blocked { refused: false }),
                ("web".into(), 8080, Reach::Answered),
            ], cycle()),
            1
        );
        assert!(result_of("telnet").unwrap().1.is_blocked());
        assert!(has_run());

        // Firewall opened, checked again: nothing is blocked, and nothing is
        // left over to keep a surface red.
        assert_eq!(
            store_for(
                vec![
                    ("telnet".into(), 2323, Reach::Answered),
                    ("web".into(), 8080, Reach::Answered),
                ],
                cycle(),
            ),
            0
        );
        assert!(!result_of("telnet").unwrap().1.is_blocked(), "the red must clear");
        assert_eq!(results().iter().filter(|(_, _, r)| r.is_blocked()).count(), 0);

        // A listener that has gone away leaves no trace either.
        store_for(vec![("web".into(), 8080, Reach::Answered)], cycle());
        assert!(result_of("telnet").is_none(), "a listener that is gone is gone");

        // And a reset puts it back to "nobody has looked", which is a third
        // state -- not the same as "nothing is blocked".
        reset();
        assert!(!has_run());
        assert!(results().is_empty());
        assert!(age().is_none());
    }

    /// **A check from a previous cycle stores nothing.**  The startup check
    /// runs on a blocking thread the runtime shutdown does not stop, so a Save
    /// and Restart can land mid-probe; the old cycle's listeners then refuse
    /// and, stored, would redden ports the new cycle never tested.  The guard
    /// existed and the startup path did not use it -- so it is pinned too.
    #[test]
    fn test_a_stale_cycles_check_stores_nothing() {
        let _g = tests_lock();
        reset();
        let old = cycle();
        reset(); // the restart
        store_for(vec![("telnet".into(), 2323, Reach::Blocked { refused: true })], old);
        assert!(result_of("telnet").is_none(), "a stale answer reached the new cycle");
        assert!(!has_run(), "and the new cycle still reads as unchecked");
        // Positive control: the current cycle's answer is stored.
        store_for(vec![("telnet".into(), 2323, Reach::Answered)], cycle());
        assert!(result_of("telnet").is_some());
        reset();

        let src = include_str!("portcheck.rs").replace('\r', "");
        let body = &src[src.find("pub fn spawn_startup_check(").unwrap()..];
        let body = &body[..body.find("\n}\n").unwrap()];
        assert!(body.contains("let mine = cycle();"), "the startup check takes its cycle up front");
        assert!(body.contains("run_check_for_cycle(mine)"), "and checks against it");
    }

    /// **Every caller names the cycle its check belongs to** -- the web page
    /// and the telnet `F` key did not, and could store a dead cycle's answers
    /// over a fresh table. The unguarded entry point is gone, so the compiler
    /// holds most of this; the scan holds the rest: that the telnet key takes
    /// its cycle *before* the hand-off to the blocking pool, which is the
    /// window it exists to cover.
    #[test]
    fn test_every_port_check_caller_names_its_cycle() {
        let callers = [
            ("gui.rs", include_str!("gui.rs")),
            ("webserver.rs", include_str!("webserver.rs")),
            ("telnet/config_ui.rs", include_str!("telnet/config_ui.rs")),
        ];
        let needle = ["portcheck::", "run_check"].concat();
        for (name, src) in callers {
            let calls: Vec<&str> = src.match_indices(needle.as_str()).map(|(i, _)| &src[i..]).collect();
            assert!(!calls.is_empty(), "{name} no longer runs a port check -- update this list");
            for call in calls {
                assert!(
                    call[needle.len()..].starts_with("_for_cycle("),
                    "{name}: a port check that does not name its cycle: {:?}",
                    &call[..call.len().min(60)]
                );
            }
        }
        let telnet = include_str!("telnet/config_ui.rs").replace('\r', "");
        let at = telnet.find(&needle).unwrap();
        let before = &telnet[telnet[..at].rfind("\"f\" => {").unwrap()..at];
        assert!(
            before.contains("let cycle = crate::portcheck::cycle();")
                && before.find("let cycle").unwrap() < before.find("spawn_blocking").unwrap(),
            "the telnet check must take its cycle before the blocking hand-off"
        );
        // ...and hand *that* cycle over, not one read again on the far side.
        assert!(
            telnet[at + needle.len()..].starts_with("_for_cycle(cycle)"),
            "the telnet check reads its cycle after the hand-off"
        );
    }

    /// Serialises the tests that touch the process-wide table.
    fn tests_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The two ways nothing answers read differently, because the operator's
    /// firewall tooling shows them differently — a REJECT refuses, a DROP times
    /// out.
    #[test]
    fn test_refused_and_dropped_are_both_blocked_but_distinguishable() {
        let refused = Reach::Blocked { refused: true };
        let dropped = Reach::Blocked { refused: false };
        assert!(refused.is_blocked() && dropped.is_blocked());
        assert_ne!(refused, dropped);
        assert!(refused.hover(80).unwrap().contains("refused"));
        assert!(dropped.hover(80).unwrap().contains("timeout"));
    }
}
