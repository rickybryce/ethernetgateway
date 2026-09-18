//! The main menu's second page — restarting and shutting down the computer
//! the gateway runs on.
//!
//! **Unix only.** The whole module is `#[cfg(unix)]`, along with the `2` entry
//! on the main menu, its key handler, its half of the valid-key hint and its
//! lines in the main help. There
//! is no Windows equivalent of "run one command as another user with a
//! password typed down a telnet session": `shutdown /r` needs the *process* to
//! hold the privilege already, and the gateway deliberately does not run
//! elevated (see [`crate::config::elevation_warning_lines`] — a single run
//! under `sudo` leaves root-owned files across the data directory, which this
//! project treats as a defect to warn about rather than a mode to support).
//! So the entries are compiled out there rather than shown and then refused.
//! When an item that *does* work on Windows lands on this page, the gate moves
//! from the module to the power items.
//!
//! **And on Unix the same items are hidden at run time** where the kernel's
//! `no_new_privs` forbids elevation -- see [`available`].  Four surfaces carry
//! that: the rows, the `2` entry, that key's arm, and both the menu hint and
//! the main help screen.  A fifth would be a key documented and refused.
//!
//! **The gateway asks; it is never elevated.** Every path here shells out to
//! `sudo` with the operator's own password, and the password reaches us
//! through [`TelnetSession::get_password_input`] — the masked reader, which is
//! also what suppresses the `gateway_debug` byte trace for the length of the
//! prompt. Reading the password any other way would put it in the log buffer
//! one byte per line, and that buffer is written to disk and served at
//! `/logs`.
//!
//! **A Commodore cannot type an underscore into this prompt.**  The masked
//! reader treats `is_esc_key` as cancel, and for a PETSCII terminal that
//! accepts the back-arrow -- which is `0x5F`, ASCII underscore.  The mechanism
//! is old and the gateway's own password shares it, but the exposure here is
//! new and worse: this password is set outside the gateway, so an operator
//! cannot work around it by changing the credential, and the failure looks
//! exactly like a dropped key.  Said in the manual beside the feature, because
//! a user who hits it has no way to work it out.
//!
//! **`shutdown`, not `systemctl`.** `cfg(unix)` includes macOS, which has no
//! `systemctl`; `/sbin/shutdown` is on both, and on a systemd box it is
//! systemd's own binary. One command, no per-OS branch.

use super::*;

/// How long any one `sudo` here may take before the session gives up on it.
///
/// **The first subprocess timeout in this codebase, and deliberately scoped to
/// this module.**  Everything else that shells out -- the live CP/M gates, the
/// lrzsz and C-Kermit harnesses, the router probe -- either runs under a test
/// or cannot block on a human-facing dependency.  These three calls can:
/// `sudo` runs the machine's PAM stack, and a PAM stack that reaches a network
/// directory blocks for as long as that lookup takes.  A blocked call here
/// hangs the whole session's task with no key working, on a page whose entire
/// design is that no screen promises what the next step cannot deliver.
///
/// Thirty seconds, chosen from what the two ends need rather than from taste:
/// a healthy local PAM answers in milliseconds and even an unhappy LDAP lookup
/// is seconds, so nothing working is cut off; and it is far inside the 900 s
/// session idle timeout, so the operator gets the error rather than a silent
/// disconnect.  It bounds each call separately, which is the useful unit --
/// the operator is shown a screen between them.
pub(in crate::telnet) const SUDO_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(30);

/// The one text for a `sudo` that never came back.
///
/// Shared by the probe and [`run_elevated`] so the two cannot describe the
/// same event differently, and written to fit the 40-column screen it lands
/// on.  It says *did not answer* rather than *failed*: the command may well
/// still be running, and telling an operator their shutdown was refused when
/// it may yet happen is the one wrong thing this page could say.
pub(in crate::telnet) fn sudo_timed_out_line() -> String {
    format!("sudo did not answer in {}s.", SUDO_TIMEOUT.as_secs())
}

/// The same event as an [`std::io::Error`], for [`run_elevated`], whose
/// callers already render one.
///
/// `TimedOut` rather than `other`, so a caller that ever wants to tell this
/// apart from a spawn failure can, without matching on the message -- the same
/// reason `probe_elevation` reads `NotFound` instead of sudo's English.
fn sudo_timed_out_error() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::TimedOut, sudo_timed_out_line())
}

/// Which way the machine is being taken down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::telnet) enum PowerAction {
    Restart,
    Shutdown,
}

impl PowerAction {
    /// Screen title for the confirmation page.
    pub(in crate::telnet) fn title(self) -> &'static str {
        match self {
            PowerAction::Restart => "RESTART COMPUTER",
            PowerAction::Shutdown => "SHUT DOWN COMPUTER",
        }
    }

    /// The question the operator answers Y/N to.
    pub(in crate::telnet) fn question(self) -> &'static str {
        match self {
            PowerAction::Restart => "Restart the computer?",
            PowerAction::Shutdown => "Shut down the computer?",
        }
    }

    /// What is said once the command has been accepted.
    pub(in crate::telnet) fn going_down(self) -> &'static str {
        match self {
            PowerAction::Restart => "Restarting now.",
            PowerAction::Shutdown => "Shutting down now.",
        }
    }

    /// What is said when the operator answers N.
    pub(in crate::telnet) fn cancelled(self) -> &'static str {
        match self {
            PowerAction::Restart => "Restart cancelled.",
            PowerAction::Shutdown => "Shutdown cancelled.",
        }
    }

    /// The command and its arguments.
    ///
    /// `shutdown`, not `systemctl`: `cfg(unix)` includes macOS, which has no
    /// `systemctl`, and `/sbin/shutdown` is on every target here -- on a
    /// systemd box it is systemd's own binary.
    ///
    /// **`-h` is measured on Linux and reasoned on macOS**, and the difference
    /// is worth writing down rather than glossing.  On Linux `-h` powers the
    /// machine off and `-P` (capital) is the Linux-only spelling of the same
    /// thing.  macOS's `shutdown(8)` documents `-h` as *halt* and `-p` as
    /// "halted and the power is turned off" -- two flags where Linux has one,
    /// with `-h` the weaker of them on paper.  `-h` is still the choice,
    /// because it is the one spelling both accept, but nobody here has run it
    /// on a Mac: if this is ever measured and a Mac is left halted rather than
    /// off, the fix is a `cfg(target_os = "macos")` arm using `-p`.
    pub(in crate::telnet) fn argv(self) -> &'static [&'static str] {
        match self {
            PowerAction::Restart => &["shutdown", "-r", "now"],
            PowerAction::Shutdown => &["shutdown", "-h", "now"],
        }
    }
}

/// The body of the confirmation page, per action.
///
/// Held as data rather than written inline so
/// `test_power_confirmation_lines_fit_petscii` can measure the real lines.
///
/// **Every line is inside 37 columns, because it is printed with a two-space
/// indent.**  37 + 2 is 39, which is `separator()`'s budget and for its
/// reason: a PETSCII terminal auto-wraps at 40 and *then* takes the trailing
/// CR/LF, so a row that exactly fills the screen costs two rows of a
/// twenty-two-row page.  The first version of this comment reasoned from 39
/// and then wrote 38, which permits a printed 40 -- the case it was quoting.
///
/// (The two `show_error` hints are budgeted at 38 instead.  That is the
/// convention `test_show_error_literals_fit_petscii` has always held the rest
/// of the program to -- `2 + literal <= 40` -- and one screen quietly using a
/// stricter rule than its neighbours is worse than the one wasted column.)
///
/// Both bodies say *whole computer* first.  A telnet user reached this menu
/// from somewhere else entirely, and "restart" on a gateway's menu reads as
/// "restart the gateway" -- which is what the Server configuration page's `R`
/// actually does.  The two must not be confusable.
pub(in crate::telnet) fn confirm_body(action: PowerAction) -> &'static [&'static str] {
    match action {
        PowerAction::Restart => &[
            "This restarts the whole computer,",
            "not just the gateway.",
            "",
            "Every session drops, this one too,",
            "and transfers in progress are lost.",
        ],
        PowerAction::Shutdown => &[
            "This shuts down the whole computer,",
            "not just the gateway.",
            "",
            "Every session drops, this one too,",
            "and transfers in progress are lost.",
            "",
            "It will need switching on by hand.",
        ],
    }
}

/// How this process has to ask the system to change power state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::telnet) enum Elevate {
    /// Already root — run the command directly, no `sudo` in the picture.
    Direct,
    /// `sudo` answers without a password, because a **NOPASSWD sudoers rule**
    /// says so.
    ///
    /// Deliberately not "or a live timestamp": the probe clears the timestamp
    /// before it asks, for the reason [`probe_elevation`] spends a paragraph
    /// on.  A cached credential earned in the operator's own shell is not a
    /// statement that this page may skip the password.
    SudoQuiet,
    /// `sudo` wants the operator's password.
    SudoPassword,
    /// **Nothing can elevate here, and no password would change that.**  The
    /// kernel's `no_new_privs` flag is set on this process, so a setuid binary
    /// -- which is what `sudo` is -- cannot gain privilege however correct the
    /// password is.  systemd sets it from `NoNewPrivileges=yes`, which the
    /// gateway's own shipped unit turns on, so this is the *ordinary* state of
    /// a packaged installation rather than an exotic one.
    ///
    /// It is a separate answer from [`Elevate::SudoPassword`] because the page
    /// must not ask: a password prompt is a promise that a right answer
    /// restarts the machine, and here no answer can.  That is the same rule
    /// the order of the steps exists to keep.
    Blocked,
}

/// Whether this installation can change the computer's power state at all.
///
/// **Cached, because it is a property of the process and cannot change.**
/// `no_new_privs` is set at exec and is one-way; the main menu asks this on
/// every render, and reading `/proc/self/status` each time would be a syscall
/// per menu draw for an answer that is fixed at startup.
///
/// Deliberately *not* `sudo -n -l`: that is a process spawn, which the menu
/// cannot afford, and its answer can change under the operator's feet when
/// they edit sudoers.  So this covers only what is cheap and certain; a
/// machine where sudo is absent or refuses still reaches the refusal at the
/// moment of use, which is where it can be reported accurately.
pub(in crate::telnet) fn available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    // **The order is the probe's order, and it has to be.**  `no_new_privs`
    // stops a *setuid* binary gaining privilege, which is why `sudo` cannot
    // work under it -- but a gateway already running as root gains nothing
    // and needs no `sudo`, so `probe_elevation_within` answers the root
    // question first and returns `Elevate::Direct`.  Gating this on
    // `no_new_privs` alone therefore hid a working feature: under a root unit
    // with `NoNewPrivileges=yes` -- an ordinary hardened container -- the `2`
    // entry, both rows, their key arms, the MORE hint and the help screen all
    // vanished for a machine that would have restarted perfectly well.
    //
    // A gate that disagrees with the probe is the same defect either way
    // round: one direction hides a feature that works, the other offers one
    // that cannot.  So both read the two conditions in the same order.
    *AVAILABLE.get_or_init(|| power_is_possible(config::detect_elevation().0, no_new_privs()))
}

/// The rule [`available`] caches, as a function of the two facts it reads.
///
/// Separated because `available` is a `OnceLock` over the *live* machine and
/// so can only ever be tested on whatever host the suite runs on -- which is
/// one of the four combinations, and never the one that was wrong.
pub(in crate::telnet) fn power_is_possible(is_root: bool, no_new_privs: bool) -> bool {
    // Root needs no `sudo`, so `no_new_privs` -- which only stops a *setuid*
    // binary gaining privilege -- does not bear on it.  Same order as
    // `probe_elevation_within`, deliberately: see the comment there.
    is_root || !no_new_privs
}


/// Whether the second page has anything on it.
///
/// **The page outlives its current contents.**  Restart and shutdown are all
/// it carries today, so this is `available()` — but the page is where the next
/// thing that does not fit the main menu will go, and when that lands this
/// becomes `available() || that_thing`, one line, rather than a rediscovery of
/// why the `2` entry is gated on a power setting at all.
///
/// Offering `2` to an empty page is worse than not offering it: the operator
/// spends a keypress to learn there was nothing there.
pub(in crate::telnet) fn second_page_has_items() -> bool {
    available()
}

/// What the page says when nothing here can elevate.
///
/// A named seam so the width guard and the wording test read the real text
/// rather than a copy, exactly as `more_menu_hint` and `confirm_body` do.
/// It names `NoNewPrivileges` because that is the string an operator greps
/// for in their unit file; "the system refused" would send them to their
/// password instead, which is where they were already looking.
pub(in crate::telnet) fn blocked_lines() -> Vec<&'static str> {
    vec![
        "This gateway cannot restart or shut",
        "down the computer: it runs under a",
        "sandbox that forbids gaining",
        "privileges, so no password can work.",
        "",
        "It is NoNewPrivileges=yes in the",
        "service unit, not your password.",
    ]
}

/// Why an unauthenticated session is refused when no password will be asked.
///
/// **The one path where nothing at all is proved.**  `Elevate::SudoPassword`
/// asks for the operator's system password, so that session proves something
/// before the machine moves.  `Elevate::Direct` (already root) and
/// `Elevate::SudoQuiet` (a NOPASSWD sudoers rule) ask for nothing -- correctly,
/// there being nothing to ask -- and `security_enabled` is **off by default**,
/// so on such a machine any peer that reached the telnet port could restart
/// the computer having presented no credential whatsoever.
///
/// Both no-password paths are covered, not just root: a NOPASSWD rule is the
/// same hole by a different route, and covering only the one that was
/// reported would leave the other to be rediscovered.
///
/// **Names the remedy without asserting the present state.**  Two wrongs are
/// available here.  Saying "Turn on security_enabled" asserts that it is off,
/// which this path never reads -- a session can be unauthenticated with it on,
/// having begun before the operator switched it on, which is the very reason
/// the flag is recorded at the door rather than derived.  But dropping the
/// instruction with the assertion left "reconnect on a listener that asks who
/// you are", and on the default install -- telnet only, SSH off -- there is no
/// such listener to reconnect on, so the screen named no action at all.
/// "Set X and reconnect" is an instruction, not a claim about X.
pub(in crate::telnet) fn unverified_lines() -> Vec<&'static str> {
    vec![
        "This computer needs no password to",
        "restart, so this page needs a login",
        "and this session did not have one.",
        "",
        "Set security_enabled and reconnect,",
        "and the gateway will ask who you are.",
    ]
}

/// Whether this process carries the kernel's `no_new_privs` flag.
///
/// **Read from the kernel, not from sudo's English.**  `probe_elevation`'s own
/// comment declines to tell sudo's refusals apart by reading its messages
/// because they are locale-dependent, and that reasoning holds here -- but the
/// flag itself is a number in `/proc/self/status`, so it can be asked directly.
/// Measured on the Pi: under `NoNewPrivileges=yes` sudo says *The "no new
/// privileges" flag is set, which prevents sudo from running as root*, and the
/// same command in a unit with `NoNewPrivileges=no` elevates.
///
/// Linux only in practice: the file does not exist on macOS or the BSDs, where
/// `read_to_string` fails and this answers `false`, leaving their behaviour
/// exactly as it was.
fn no_new_privs() -> bool {
    std::fs::read_to_string("/proc/self/status")
        .as_deref()
        .map(parse_no_new_privs)
        .unwrap_or(false)
}

/// The `NoNewPrivs:` field of a `/proc/<pid>/status`, or `false` if absent.
///
/// Split out so the rule is testable: the reading itself depends on the
/// process the suite happens to run in, which is not a fixture anyone can set.
/// **Absent means false**, which is the safe direction here -- a kernel too old
/// to report the field is a kernel that does not enforce it either, and
/// answering `true` would switch the feature off on a machine where it works.
pub(in crate::telnet) fn parse_no_new_privs(status: &str) -> bool {
    status
        .lines()
        .find_map(|l| l.strip_prefix("NoNewPrivs:"))
        .is_some_and(|v| v.trim() == "1")
}

/// Work out which of the three we are in, without changing anything.
///
/// `sudo -n -l -- <the real argv>`: `-l` asks whether this command is
/// permitted and runs nothing, `-n` never prompts — so a machine that would
/// ask for a password fails it at once rather than hanging on a terminal that
/// is not there.  Asking first, rather than running `shutdown` and reading the
/// failure, is what lets the confirmation, the password prompt and the goodbye
/// happen in an order that is true at every step.
///
/// **The probe must name the command it is standing in for.**  Probing with
/// `sudo -n true` — the obvious version, and the first one written here —
/// answers a different question, and gets it wrong in both directions.  The
/// *least-privilege* sudoers line an operator would write for this feature is
/// scoped to one command:
///
/// ```text
/// ricky ALL=(root) NOPASSWD: /sbin/shutdown
/// ```
///
/// Under that line `sudo -n true` fails, because `true` is not on the list, so
/// the probe would demand a password for an account that has none — the
/// feature permanently unusable on exactly the configuration it was set up
/// for.  The other way round, a cached timestamp makes `true` succeed while
/// saying nothing about whether `shutdown` is permitted, and *that* failure
/// lands after the goodbye, where there is no screen left to report it on.
///
/// **`-k` first, because a cached timestamp is not a sudoers rule.**  `sudo`
/// records a successful authentication in `/run/sudo/ts/<uid>` -- **one file
/// per uid**, because neither a gateway session nor a systemd service has a
/// tty for sudo's tty-scoped default to key on, so they all share the
/// operator's record.  Without `-k` this probe answered [`Elevate::SudoQuiet`]
/// for `timestamp_timeout` minutes after the operator ran *any* `sudo`
/// anywhere on the machine, and the page then took the computer down **with no
/// password at all**.  Measured on the Pi 2026-09-16 with a three-session
/// control: a fresh session is refused, `sudo -v` in a *second* session makes a
/// *third* one succeed.  It matters because `security_enabled` is off by
/// default, so the menu this sits on asks for no credential of its own either
/// -- for those fifteen minutes a visitor on the LAN reboots the machine
/// having proved nothing.
///
/// `-k` makes this invocation ignore that record, so only a genuine NOPASSWD
/// rule still answers quietly.
///
/// **And it costs the operator nothing, which was measured rather than
/// assumed.**  `-k` *without* a command deletes the cached credential; `-k`
/// **in conjunction with a command** -- which is what this is -- only causes
/// sudo to ignore it, and sudo(8) is explicit that it "will not update the
/// user's cached credentials" either.  So opening this page neither clears the
/// credential the operator earned in their own shell nor extends it.  This
/// file claimed the opposite for a while, as a deliberate trade; the trade was
/// imaginary.  Measured on the Pi (sudo 1.9.16p2) by alternating the two forms
/// five times against one live credential: the bare `-n -l` succeeded every
/// time and the `-k -n -l` refused every time, so the record was neither
/// consumed nor refreshed by either.
///
/// It is self-consistent for the same reason: the probe cannot write a
/// timestamp, so the feature can never come to depend on one it created.
///
/// `Err` carries a line to show the operator.  Two cases: a machine with no
/// `sudo` at all, which is a fact about the installation and not something a
/// retry will fix, and a probe that never came back -- see [`SUDO_TIMEOUT`].
pub(in crate::telnet) async fn probe_elevation(argv: &[&str]) -> Result<Elevate, String> {
    probe_elevation_within(argv, SUDO_TIMEOUT).await
}

/// [`probe_elevation`] with its bound supplied.
///
/// Split for the same reason [`run_elevated_within`] is, and **not** driven by
/// a test here: a call shells out to the real `sudo` on every `cargo test`,
/// which on a machine whose user is not in sudoers logs an authentication
/// failure per run.  That was the older guard's reason too, and it is the only
/// one -- the `-k` itself is harmless to the developer's credential cache, for
/// the reason [`probe_elevation`] gives.  The shape is held by a source scan
/// instead.
async fn probe_elevation_within(
    argv: &[&str],
    budget: std::time::Duration,
) -> Result<Elevate, String> {
    if config::detect_elevation().0 {
        // **Root gets no help from sudo, including finding the binary.**  The
        // two sudo branches resolve `shutdown` through sudoers' `secure_path`,
        // which carries the sbin directories; this branch inherits the
        // gateway's own `PATH`, and a service `PATH` need not.  Measured on
        // this machine: `which shutdown` finds nothing for an ordinary
        // account, while `sudo -n -l -- shutdown -r now` resolves it.
        //
        // Answered here rather than at the spawn, because the spawn happens
        // *after* the goodbye: without this, a root gateway with a short PATH
        // says "Restarting now.", clocks out the farewell, drops the session
        // and does nothing, leaving one ERROR line as the only trace.  That is
        // the one place the order-of-steps rule would not hold.
        return match direct_program(argv[0]) {
            Some(_) => Ok(Elevate::Direct),
            None => Err(format!("{} is not on this computer's PATH.", argv[0])),
        };
    }
    // Before sudo is consulted at all: a setuid binary cannot raise privilege
    // under `no_new_privs`, so every answer below would be a guess at a
    // question already settled.  After the root branch above, because a root
    // session execs the command directly and the flag does not bear on that.
    if no_new_privs() {
        return Ok(Elevate::Blocked);
    }
    let mut probe = tokio::process::Command::new("sudo");
    probe
        .args(["-k", "-n", "-l", "--"])
        .args(argv)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        // Or a probe abandoned by the timeout below leaves a `sudo` behind
        // holding whatever it was blocked on.
        .kill_on_drop(true);
    let probe = match tokio::time::timeout(budget, probe.status()).await {
        Ok(r) => r,
        Err(_) => return Err(sudo_timed_out_line()),
    };
    match probe {
        Ok(st) if st.success() => Ok(Elevate::SudoQuiet),
        // Two different refusals arrive as the same non-zero status — "a
        // password is required" and "you may not run this at all" — and
        // telling them apart means reading sudo's English.  Both are answered
        // by asking for the password: the first is right, and the second gets
        // a refusal the operator can read instead of a guess we made for them.
        Ok(_) => Ok(Elevate::SudoPassword),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err("sudo is not installed here.".to_string())
        }
        // An `io::Error` renders to whatever the OS says, which is unbounded;
        // the screen this lands on is 40 columns.  Cut here rather than at the
        // one call site, so a second caller cannot inherit an uncut version --
        // `sudo_error_line` is given a width for the same reason.
        Err(e) => Err(crate::webbrowser::truncate_to_width(
            &format!("Could not run sudo: {}", e),
            PETSCII_WIDTH - 2,
        )),
    }
}

/// Where a root session will find `argv[0]`, or `None`.
///
/// The `sbin` directories first and by absolute path, because they are where
/// `shutdown` actually lives and a root process's `PATH` is whatever its
/// launcher gave it.  The bare name last, so an unusual installation still
/// works through the ordinary lookup.
fn direct_program(program: &str) -> Option<String> {
    for dir in ["/sbin", "/usr/sbin", "/bin", "/usr/bin"] {
        let path = format!("{}/{}", dir, program);
        if std::path::Path::new(&path).exists() {
            return Some(path);
        }
    }
    // Not found by absolute path -- fall back to the PATH lookup the OS would
    // do, and let the spawn answer.  `which` is not used: it is one more
    // binary that need not be installed.
    std::env::var_os("PATH")
        .map(|p| {
            std::env::split_paths(&p)
                .map(|d| d.join(program))
                .find(|c| c.exists())
        })?
        .map(|c| c.to_string_lossy().into_owned())
}

/// Run one power command, elevating the way `elev` says.
///
/// The password is fed on stdin with `-S` and an empty prompt (`-p ""`), so
/// nothing of it is echoed and sudo's own prompt does not land in the captured
/// stderr we may show back.  It is passed by reference and never copied into a
/// log, an error string or a config value.
pub(in crate::telnet) async fn run_elevated(
    elev: Elevate,
    password: Option<&str>,
    argv: &[&str],
) -> std::io::Result<std::process::Output> {
    run_elevated_within(elev, password, argv, SUDO_TIMEOUT).await
}

/// [`run_elevated`] with its bound supplied, so the give-up can be measured
/// rather than waited out.
///
/// The same shape as the printer's five-second idle close, which is tested
/// against an injected clock: a guard that proves a thirty-second timeout by
/// sitting through thirty seconds is a guard nobody will keep running.
pub(in crate::telnet) async fn run_elevated_within(
    elev: Elevate,
    password: Option<&str>,
    argv: &[&str],
    budget: std::time::Duration,
) -> std::io::Result<std::process::Output> {
    use tokio::io::AsyncWriteExt;

    debug_assert!(
        !argv.is_empty(),
        "run_elevated needs a command; `argv[0]` and `argv[1..]` below index it",
    );
    let mut cmd = match elev {
        Elevate::Direct => {
            // Resolved rather than taken bare, for the reason `probe_elevation`
            // gives: this branch gets no `secure_path` from sudo.
            let program = direct_program(argv[0]).unwrap_or_else(|| argv[0].to_string());
            let mut c = tokio::process::Command::new(program);
            c.args(&argv[1..]);
            c
        }
        // **`-k` on every one of these, so no path here can ride a cached
        // credential.**  It is not a tidiness rule -- without it on the
        // `SudoPassword` branch the page asks for a password and then does not
        // check it.  `power_action` verifies the typed password with `sudo -v`
        // before it says goodbye, and `sudo -v` is satisfied by a live
        // timestamp without ever reading stdin, so with the operator's own
        // credential cached *any typed string* passed and the machine went
        // down.  Measured on the Pi (sudo 1.9.16p2): `sudo -S -p "" -v` fed
        // `not-the-password-xyzzy` was **ACCEPTED**, and the same call with
        // `-k` was refused.
        //
        // That is worse than the hole it would otherwise have left, which is
        // why it is here rather than only on the probe: a page that skips the
        // prompt is at least honest about it, while a prompt that accepts
        // anything is a screen promising a check that never happened -- the
        // one thing the order of the steps on this page exists to prevent.
        //
        // `SudoQuiet` takes it too.  That branch only runs when the `-k` probe
        // said NOPASSWD, so nothing there needs a cache -- but an invariant
        // that holds on every path is one a later reader cannot breach by
        // adding a fourth, and the two must not be able to disagree about
        // whether a timestamp counts.
        Elevate::SudoQuiet => {
            let mut c = tokio::process::Command::new("sudo");
            c.args(["-k", "-n"]);
            c.args(argv);
            c
        }
        Elevate::SudoPassword => {
            let mut c = tokio::process::Command::new("sudo");
            c.args(["-k", "-S", "-p", ""]);
            c.args(argv);
            c
        }
        // Unreachable: `power_action` reports and returns before it gets here.
        // An error rather than a panic, and rather than quietly running the
        // command unprivileged -- which on a `shutdown` would fail anyway, but
        // for a reason the operator would have to guess at.
        Elevate::Blocked => {
            return Err(std::io::Error::other(
                "this process cannot elevate (no_new_privs)",
            ));
        }
    };
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // **Load-bearing with the timeout below, not tidiness.**  When the
        // timeout fires the `Child` is dropped, and without this the process
        // survives -- an abandoned `sudo` still holding the operator's
        // password on a stdin nobody will ever close.
        .kill_on_drop(true);

    // Bounded as one unit: the spawn, the password, and the wait.  Splitting
    // it would let a child that accepted its stdin and then blocked run out a
    // fresh budget, which is the case the bound exists for.
    let run = async {
        let mut child = cmd.spawn()?;
        {
            // Dropped (and so closed) before the wait, or `sudo -S` sits on an
            // open stdin waiting for a password that has already been sent.
            let mut stdin = child.stdin.take().ok_or_else(|| {
                std::io::Error::other("could not open stdin for the power command")
            })?;
            if let Some(pw) = password {
                stdin.write_all(pw.as_bytes()).await?;
                stdin.write_all(b"\n").await?;
            }
            stdin.flush().await?;
        }
        child.wait_with_output().await
    };
    match tokio::time::timeout(budget, run).await {
        Ok(r) => r,
        Err(_) => Err(sudo_timed_out_error()),
    }
}

/// The MORE page's valid-key hint.
///
/// A const rather than a literal at the `_` arm, so the test that checks it
/// fits and names every key reads the string the operator is shown.  A test
/// holding its own copy of a screen's text is a guard comparing the source
/// with a copy of itself -- `session.rs`'s `main_menu_key_hint` exists for the
/// same reason.
///
/// Budgeted against `PETSCII_WIDTH - 2`: `show_error` prints a two-space
/// indent in front of whatever it is given.
///
/// **The flag is a parameter for the reason `more_menu_rows` takes one.**  It
/// comes from the kernel, so a version reading `available()` itself could only
/// ever be exercised in whichever state the machine running the suite happens
/// to be in -- and the text for the other state, which is the one a packaged
/// installation shows, would be checked nowhere.  The first version of this
/// did exactly that, under a test comment claiming both were covered.
pub(in crate::telnet) fn more_menu_hint(powered: bool) -> &'static str {
    if powered {
        "Press R, S, H, or Q."
    } else {
        "Press H or Q."
    }
}

/// `  Password for <user>: `, fitted to the narrowest screen.
///
/// **`$USER` is an environment variable, so it can be anything.**  A service
/// account name is easily longer than a C64's whole row, and this was the one
/// string on these screens with no width at all while the hostname ten lines
/// up is carefully cut to 26.  16 leaves room for `  Password for ` and the
/// `: `.
///
/// A free function taking the name, rather than the inline `format!` it was,
/// for the reason `computer_row_for` is one: a guard has to be able to drive
/// the longest name that can ever reach it, and a test that rebuilds the row
/// from the same two literals is a test comparing the source with a copy of
/// itself.
pub(in crate::telnet) fn password_prompt_label(user: Option<&str>) -> String {
    match user.filter(|u| !u.trim().is_empty()) {
        Some(u) => format!(
            "  Password for {}: ",
            crate::webbrowser::truncate_to_width(u, 16)
        ),
        None => "  Password: ".to_string(),
    }
}

/// Reduce a failed command's stderr to the one line worth putting on a 40-col
/// screen.
///
/// The **last** non-empty line, because sudo's useful sentence comes after its
/// chatter ("Sorry, try again." three times, then the count).  The `sudo: `
/// prefix is dropped — the screen already says what was being attempted, and
/// on PETSCII those six characters are a sixth of the row.
pub(in crate::telnet) fn sudo_error_line(stderr: &str, width: usize) -> String {
    let line = stderr
        .lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .unwrap_or("");
    let line = line.strip_prefix("sudo: ").unwrap_or(line);
    if line.is_empty() {
        return "The computer refused the command.".to_string();
    }
    crate::webbrowser::truncate_to_width(line, width)
}

impl TelnetSession {
    // ─── MORE (main menu, second page) ──────────────────────

    /// `Computer: <name>`, fitted to the screen — or `None` where the machine
    /// will not say what it is called.
    ///
    /// Both screens that name the computer render it through here.  They are
    /// two sentences about one fact, and a width rule written twice is a width
    /// rule that holds once.
    fn computer_row(&self) -> Option<String> {
        self.computer_row_for(&crate::relay::hostname_label())
    }

    /// `computer_row` with the name supplied, so a test can drive the longest
    /// one that can ever reach it.
    ///
    /// **The split is the point.**  The first version of the guard rebuilt
    /// this row inline from the same two literals -- a test comparing the
    /// source with a copy of itself.  Proved by mutation: widening the cap
    /// from 26 to 39 left all 576 telnet tests green while the row printed at
    /// 51 columns on a C64, wrapping and eating a second row of a 22-row page,
    /// which is the exact overflow the guard's own comment claimed to prevent.
    pub(in crate::telnet) fn computer_row_for(&self, name: &str) -> Option<String> {
        let name = name.trim();
        if name.is_empty() {
            return None;
        }
        // 40 - "  Computer: " - a column of margin, and 60 on a wide screen.
        let max = if self.terminal_type == TerminalType::Petscii { 26 } else { 60 };
        Some(format!(
            "  Computer: {}",
            self.cyan(&crate::webbrowser::truncate_to_width(name, max))
        ))
    }

    /// Who is asking, for the log: the address, or what kind of session it is
    /// when there is no address.
    ///
    /// One helper, because three log lines named the requester and two of them
    /// did it only for an addressed session.
    fn power_requester(&self) -> String {
        match self.peer_addr {
            Some(ip) => ip.to_string(),
            None => self.client_type_label().to_string(),
        }
    }

    /// What the probe last said about **this action**, if it has been asked.
    ///
    /// Keyed by the action because `probe_elevation` is: sudoers rules are
    /// per-argument, so `shutdown -h now` and `shutdown -r now` can honestly
    /// differ, and answering one out of the other's slot would skip a
    /// password the machine does want -- after the farewell had been sent.
    pub(in crate::telnet) fn remembered_elevation(&self, action: PowerAction) -> Option<Elevate> {
        self.power_elevation
            .iter()
            .find(|(a, _)| *a == action)
            .map(|(_, e)| *e)
    }

    /// Whether this session has spent its `sudo` attempts.
    ///
    /// **Two counters, because one of them cannot see every session.**  An
    /// addressed session is held by a per-IP `LockoutMap`, which is the
    /// counter an attacker cannot reset by reconnecting.  A session with
    /// no address -- a caller on the modem, a CP/M guest that dialled
    /// `ATDT ethernetgateway` -- is not in that map at any key, so it was
    /// bounded by nothing once the per-session field was removed: every wrong
    /// answer here is a real PAM attempt against the operator's *host*
    /// account, and the page simply kept asking.  `MAX_AUTH_ATTEMPTS` is the
    /// same three either way.
    pub(in crate::telnet) fn power_attempts_exhausted(&self) -> bool {
        match self.peer_addr {
            Some(ip) => crate::telnet::is_locked_out(&self.power_lockouts, ip),
            None => self.power_password_failures >= crate::telnet::MAX_AUTH_ATTEMPTS,
        }
    }

    /// Count one refused `sudo`, wherever this session is counted, and return
    /// the running total for the log.
    pub(in crate::telnet) fn record_power_failure(&mut self) -> u32 {
        match self.peer_addr {
            Some(ip) => crate::telnet::record_auth_failure(&self.power_lockouts, ip),
            None => {
                self.power_password_failures = self.power_password_failures.saturating_add(1);
                self.power_password_failures
            }
        }
    }

    /// The main menu's second page.
    ///
    /// Returns `false` when the session must end — the same convention as
    /// [`TelnetSession::handle_main_command`], and the only way out of here
    /// that isn't `Q`: once the machine has accepted a restart or a shutdown
    /// there is nothing left to return to.
    pub(in crate::telnet) async fn more_menu(&mut self) -> Result<bool, std::io::Error> {
        loop {
            self.render_more_menu().await?;
            // The breadcrumb prompt every nested screen uses; `Menu::path()`
            // covers the three top-level menus only, so this page spells its
            // own the way `configuration()` does.
            // Named for the page, like every other prompt in the product
            // (`ethernet/config`, `ethernet/serial`): "more" was the old
            // item's word and stopped naming anything when it became
            // "Second Menu".  Found by reading the live screen, not the code.
            let prompt = format!("{}> ", self.cyan("ethernet/second"));
            self.send(&prompt).await?;
            self.flush().await?;

            // ESC and a dropped session both arrive as `None`, and both mean
            // the same thing here: go back.  `get_menu_input` has already
            // lowercased whatever was typed.
            let input = match self.get_menu_input(false).await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(true),
            };

            match input.as_str() {
                "q" => return Ok(true),
                "h" => {
                    // ASCII only: `to_latin1_bytes` turns anything above
                    // U+00FF into `?`, and a PETSCII screen has no dash of
                    // its own to spare.
                    self.show_help_page("SECOND MENU HELP", Self::more_help_lines())
                        .await?;
                }
                // Gated with the rows and the hint, so a key that is not on
                // the screen is not accepted either -- the same three layers
                // the Windows build uses.  `power_action` keeps its own
                // `Elevate::Blocked` arm behind these: a UI that hides an
                // action and a routine that refuses to perform one are
                // different jobs, and the safety one should not depend on the
                // cosmetic one being right.
                "r" if available() => {
                    if !self.power_action(PowerAction::Restart).await? {
                        return Ok(false);
                    }
                }
                "s" if available() => {
                    if !self.power_action(PowerAction::Shutdown).await? {
                        return Ok(false);
                    }
                }
                _ => {
                    self.show_error(more_menu_hint(available())).await?;
                }
            }
        }
    }

    /// Every row the MORE page draws, in order.
    ///
    /// Split out of the renderer so `test_more_menu_rows_fit_the_screen` can
    /// count and measure the **real** page.  The row-count guard it replaced
    /// was arithmetic over literals -- `3 + 1 + 2 + 2 + 1 + 1 <= 22` -- which
    /// reads nothing from this function and so could never have noticed a row
    /// being added.  This file is full of screens whose budget was found the
    /// hard way; a guard that cannot go red is worse than no guard.
    pub(in crate::telnet) fn more_menu_rows(&self, powered: bool) -> Vec<String> {
        let sep = self.separator();
        let mut rows = vec![
            sep.clone(),
            // Named for the menu item that reaches it: an operator who chose
            // "2  Second Menu" must not land on a page calling itself
            // something else.
            format!("  {}", self.yellow("SECOND MENU")),
            sep,
            String::new(),
        ];
        // Which computer these two keys act on.  A telnet user is by
        // definition somewhere else, and the name is the only thing on this
        // page that says where "the computer" is.
        if let Some(row) = self.computer_row() {
            rows.push(row);
            rows.push(String::new());
        }
        // Hidden, not shown-and-refused, exactly as they are on Windows: an
        // item an operator cannot use teaches them to distrust the menu.  The
        // `2` entry that reaches this page is gated on the same answer, so an
        // empty page is not reachable from the menu either.
        if powered {
            rows.push(format!("  {}  Restart the computer", self.cyan("R")));
            rows.push(format!("  {}  Shut down the computer", self.cyan("S")));
            rows.push(String::new());
        }
        rows.push(format!(
            "  {}  {}",
            self.action_prompt("Q", "Back"),
            self.action_prompt("H", "Help")
        ));
        rows
    }

    async fn render_more_menu(&mut self) -> Result<(), std::io::Error> {
        self.clear_screen().await?;
        for row in self.more_menu_rows(available()) {
            self.send_line(&row).await?;
        }
        Ok(())
    }

    /// Confirm, authenticate, say goodbye, then take the machine down — in
    /// that order.
    ///
    /// Returns `false` once the command has been accepted, so the caller ends
    /// the session.
    ///
    /// **The order is the design.**  Each step is only reached when the one
    /// before it is true, so nothing on the screen is ever a promise the next
    /// step can break: the confirmation comes before a password is asked for
    /// (nobody types a root password to find out what it was for), the
    /// password is *verified on its own* before the goodbye, and the goodbye
    /// is fully clocked out before `shutdown` is run.  Running the command
    /// first and then trying to say goodbye loses the race — systemd starts
    /// stopping units immediately and the socket dies mid-verse, which is the
    /// "looks like a crash" a retro terminal cannot tell from a fault.
    async fn power_action(&mut self, action: PowerAction) -> Result<bool, std::io::Error> {
        if !self.power_confirm(action).await? {
            self.send_line("").await?;
            self.send_line(&format!("  {}", self.dim(action.cancelled())))
                .await?;
            self.send_line("").await?;
            self.send("  Press any key to continue.").await?;
            self.flush().await?;
            self.wait_for_key().await?;
            return Ok(true);
        }

        // **Per IP, not per session, and checked before anything is spent.**
        // The cap used to live on the `TelnetSession`, so hanging up reset it:
        // with `security_enabled` off -- the default -- anyone who reaches the
        // telnet port walks to this prompt with no credential, and three
        // guesses per connection times `conn_rate_max` (20 a minute) is sixty
        // PAM attempts a minute against the operator's *system* account, from
        // as many addresses as the peer likes.  `conn_rate_max` is a bound on
        // that, just not at the scale that matters, since `pam_faillock`
        // denies at three.
        //
        // Counted here rather than in `authenticate`, because that runs only
        // when `security_enabled` is on and this prompt is reachable when it
        // is off -- which is the whole exposure.  `MAX_AUTH_ATTEMPTS` is 3,
        // as the per-session cap it replaced was, so one session behaves
        // exactly as before and only the reconnect changes.
        //
        // **Before the probe, because the probe is itself the cost.**  This
        // used to sit further down, inside the branch that asks for a
        // password, so a caller could hold `y` and spawn one real
        // `sudo -k -n -l` per keypress: on a box whose service account is not
        // in sudoers that is one authentication-failure line in the *host's*
        // auth log for each one, from an unauthenticated LAN user whenever
        // `security_enabled` is off -- which is the default.  Refusing here
        // costs a map lookup instead.
        //
        // Safe to ask this early only because the counter is now
        // `power_lockouts` and nothing but a refused `sudo` on this page
        // raises it.  Against the shared auth map it would have been wrong:
        // a machine needing no password at all (a root gateway, `Direct`)
        // would have been refused for telnet login failures it never made.
        if self.power_attempts_exhausted() {
            glog!(
                "Power: {} has used its attempts; not asking for a password",
                self.power_requester(),
            );
            // **The two counters expire differently, so they must not
            // promise the same thing.**  An address is banned for
            // `LOCKOUT_DURATION` and waiting really does clear it; a
            // session floor has no clock at all and only a fresh
            // connection resets it, so "try again later" would be a
            // screen the next step cannot keep -- the rule the whole
            // order of steps on this page exists to serve.
            self.show_error(match self.peer_addr {
                Some(_) => "Too many tries. Try again later.",
                None => "Too many tries for this session.",
            })
            .await?;
            return Ok(true);
        }

        // **Probed once per session, because the probe is a process spawn.**
        // The cap above counts refused *passwords*, and confirming and then
        // cancelling submits none -- so without this, `R`, `Y`, Enter looped
        // and spawned a real `sudo` every pass.  See `power_elevation` for
        // why one answer is good for the whole session.
        //
        // Remembered **per action**, because sudoers rules are per-argument:
        // see `power_elevation`.  Reusing Shutdown's answer for Restart would
        // skip the password and then fail after the farewell.
        //
        // Only a successful probe is remembered: an error can be the 30 s
        // timeout, which is a statement about this moment rather than about
        // the machine, and re-asking costs at most one spawn per attempt on a
        // path that is already slow enough to bound itself.
        let elev = match self.remembered_elevation(action) {
            Some(e) => e,
            None => match probe_elevation(action.argv()).await {
                Ok(e) => {
                    self.power_elevation.push((action, e));
                    e
                }
                Err(msg) => {
                    self.show_error(&msg).await?;
                    return Ok(true);
                }
            },
        };

        // **Say it cannot be done rather than asking for a password.**  Under
        // `no_new_privs` no password can work, and the prompt would be a
        // promise the next step cannot keep -- the rule the whole order of
        // steps here exists to serve.  Reported before the password, and it
        // names the setting so an operator can find it: measured on the Pi,
        // where the shipped unit's `NoNewPrivileges=yes` made sudo refuse and
        // the operator was shown sudo's *second* line, a hint about container
        // configuration, and would reasonably have concluded they mistyped.
        if elev == Elevate::Blocked {
            self.show_error_lines(&blocked_lines()).await?;
            glog!(
                "Power: {} refused -- no_new_privs is set on this process \
                 (systemd NoNewPrivileges=yes); no password can elevate",
                action.argv().join(" "),
            );
            return Ok(true);
        }

        // **Nothing would be proved on this path, so require a login.**
        // `Direct` and `SudoQuiet` ask for no password -- rightly, there
        // being none to ask -- and `security_enabled` ships off, so without
        // this an unauthenticated peer could restart the machine having
        // presented no credential at all.  Reported after `Blocked` and
        // before the password, in the same place and for the same reason: a
        // screen must not promise a step that cannot happen.
        if elev != Elevate::SudoPassword && !self.authenticated {
            self.show_error_lines(&unverified_lines()).await?;
            // **Says what was observed, not what the setting must be.**  The
            // first version of both this line and the screen asserted
            // "security_enabled is off", which the code never read.  A
            // session can be unauthenticated with it *on*: it may have
            // started before the operator switched it on, which is the very
            // reason this flag is recorded at the door instead of derived.
            // Naming a cause that is false sends the operator to a setting
            // that is already set.
            glog!(
                "Power: {} refused -- {} needs no password and this session \
                 did not authenticate",
                action.argv().join(" "),
                match elev {
                    Elevate::Direct => "this gateway is root, so the command",
                    _ => "a NOPASSWD sudoers rule means sudo",
                },
            );
            return Ok(true);
        }

        // Held only as long as the two `sudo` calls need it: the check, and
        // the command itself.  It is re-fed rather than relying on sudo's
        // timestamp, because a machine with `timestamp_timeout=0` caches
        // nothing and the second call would then fail after the goodbye.
        let password = if elev == Elevate::SudoPassword {
            match self.power_prompt_password(action).await? {
                Some(pw) => Some(pw),
                None => return Ok(true),
            }
        } else {
            None
        };

        // Authenticate on its own first.  `sudo -v` runs no command, so a
        // wrong password costs the operator a message and nothing else.
        //
        // It is **not** a no-op, and the earlier wording here ("changes
        // nothing") was wrong: `-v` refreshes sudo's credential timestamp,
        // which on a default sudoers leaves this account able to run any sudo
        // command without a password for another fifteen minutes.  Nothing in
        // the product offers a shell, so there is nothing here to spend that
        // on -- but the file said both that the timestamp exists (the re-feed
        // below) and that it does not, ten lines apart.
        if elev == Elevate::SudoPassword {
            let out = run_elevated(elev, password.as_deref(), &["-v"]).await;
            match out {
                Ok(o) if !o.status.success() => {
                    // Counted against the address, so leaving and coming back
                    // does not hand out three more.
                    //
                    // **It counts refusals, not wrong passwords**, and the
                    // screens say so.  Every non-zero exit from `sudo -v`
                    // lands here -- a right password from an account that is
                    // not in sudoers at all, a `requiretty` rule, a
                    // `shutdown` that is not on `secure_path`.  Telling those
                    // apart means reading sudo's English, which is
                    // locale-dependent; saying "too many wrong passwords" to
                    // an operator whose password was right and whose sudoers
                    // line is missing sends them looking in the wrong place.
                    // The refusal itself is shown each time and names the
                    // real cause.
                    // Logged for every session, counted for every session.
                    // Both used to sit inside `if let Some(ip)`, so a caller
                    // with no address -- the modem, a CP/M guest -- left no
                    // trace of a refused system password at all.
                    let n = self.record_power_failure();
                    glog!(
                        "Power: sudo refused for {} (failure {})",
                        self.power_requester(),
                        n,
                    );
                    let stderr = String::from_utf8_lossy(&o.stderr);
                    let msg = sudo_error_line(&stderr, self.confirmation_content_width());
                    // **sudo's own line, and then what it cannot say.**  Its
                    // last line is right for the common case and misleading
                    // for two others: an account that is not in sudoers, and
                    // one with no password at all -- which is exactly what
                    // the shipped service user is (`useradd` with no `-p`
                    // leaves a locked `!` in `/etc/shadow`), so an operator
                    // who turns `NoNewPrivileges` off and keeps that account
                    // is told "1 incorrect password attempt" about a password
                    // that cannot exist.  That is the same shape as the
                    // container hint this page showed before it learned to
                    // read the kernel flag: accurate, and pointing at the
                    // wrong thing.
                    //
                    // **Conditional, because we cannot tell which it is.**
                    // Distinguishing them means reading sudo's English, which
                    // is locale-dependent and which `probe_elevation` already
                    // refuses to do.  "If the password is right" costs a
                    // fat-fingered operator nothing -- the condition simply
                    // does not apply to them -- and names the real cause for
                    // the one who cannot win by retyping.
                    //
                    // Written inline rather than built into a `Vec` first so
                    // `test_show_error_literals_fit_petscii` still reads these
                    // strings: that scan looks inside the call's parentheses.
                    self.show_error_lines(&[
                        &msg,
                        "",
                        "If the password is right, this",
                        "account may not be permitted to use",
                        "sudo, or may have no password of",
                        "its own.",
                    ])
                    .await?;
                    return Ok(true);
                }
                Err(e) => {
                    let msg = crate::webbrowser::truncate_to_width(
                        &format!("Could not run sudo: {}", e),
                        self.confirmation_content_width(),
                    );
                    self.show_error(&msg).await?;
                    return Ok(true);
                }
                Ok(_) => {}
            }
        }

        // Who asked, and from where.  A machine that went down without warning
        // is a question the log has to be able to answer -- and this is the
        // one action here that leaves no screen behind to look at.  Written
        // *before* the goodbye, so the record exists even if the terminal is
        // gone by the time we try to say it.
        glog!(
            "Power: {} requested from the session menu by {}",
            action.argv().join(" "),
            self.power_requester(),
        );

        // Past here the machine is going down, so say so and let every byte
        // clock out before the command is issued.
        //
        // **The notice goes on the farewell page, not before it.**  It was
        // printed here first, and `send_farewell` opens with `clear_screen` --
        // so on PETSCII (`0x93`) and on ANSI (`ESC[2J`) the operator's last
        // sight of why their session ended was wiped a few milliseconds after
        // it appeared, and only an ASCII terminal, whose clear is three blank
        // lines, ever showed it.  The comment here claimed it was said; the
        // screen disagreed.
        //
        // **Every write from here is best-effort.**  The operator has
        // confirmed and authenticated, so the decision is made; a `?` on these
        // would let a terminal that hung up mid-verse return a broken pipe
        // *before* the command was ever run, leaving the machine up, the
        // operator's password typed for nothing, and no way to tell that from
        // a refusal.  A hangup is the ordinary end of a session here
        // (`is_normal_disconnect`), not a reason to change what the session
        // asked for.
        let _ = self.send_farewell_with_notice(action.going_down()).await;

        match run_elevated(elev, password.as_deref(), action.argv()).await {
            Ok(o) if o.status.success() => {}
            Ok(o) => {
                // The session has already been said goodbye to, so the log is
                // the only place left to say this.  Reaching here means sudo
                // accepted the password a moment ago and then refused the
                // command, which is a sudoers rule that permits `-v` and not
                // `shutdown`.
                glog!(
                    "ERROR: Power: {} was refused: {}",
                    action.argv().join(" "),
                    sudo_error_line(&String::from_utf8_lossy(&o.stderr), 200)
                );
            }
            Err(e) => {
                glog!("ERROR: Power: could not run {}: {}", action.argv()[0], e);
            }
        }
        Ok(false)
    }

    /// Full-screen warning and a Y/N answer.  Modelled on
    /// `kermit_toggle_atdt_kermit`, which is this project's shape for "get the
    /// operator's intent on the record before doing something they cannot take
    /// back".
    async fn power_confirm(&mut self, action: PowerAction) -> Result<bool, std::io::Error> {
        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;
        self.send_line(&format!("  {}", self.yellow(action.title())))
            .await?;
        self.send_line(&sep).await?;
        self.send_line("").await?;
        for line in confirm_body(action) {
            if line.is_empty() {
                self.send_line("").await?;
            } else {
                self.send_line(&format!("  {}", line)).await?;
            }
        }
        self.send_line("").await?;
        if let Some(row) = self.computer_row() {
            self.send_line(&row).await?;
            self.send_line("").await?;
        }
        self.send(&format!(
            "  {} ({}/{}): ",
            action.question(),
            self.cyan("Y"),
            self.cyan("N")
        ))
        .await?;
        self.flush().await?;

        // Only `y` confirms.  `get_menu_input` lowercases, so there is no
        // upper-case arm to write -- and everything else, ESC and a dropped
        // session included, is a No.  A confirmation that any stray byte can
        // answer is not one.
        let answer = self.get_menu_input(false).await?;
        Ok(answer.as_deref() == Some("y"))
    }

    /// Ask for the password `sudo` wants, masked.
    ///
    /// `None` means the operator backed out (an empty line, ESC, or a dropped
    /// session) — an empty password is never sent, because `sudo -S` would
    /// read it as one wrong attempt and the operator would see a failure
    /// message for a key they pressed to cancel.
    async fn power_prompt_password(
        &mut self,
        action: PowerAction,
    ) -> Result<Option<String>, std::io::Error> {
        self.send_line("").await?;
        self.send_line("").await?;
        self.send_line(&format!(
            "  {}",
            self.dim(match action {
                PowerAction::Restart => "A password is needed to restart.",
                PowerAction::Shutdown => "A password is needed to shut down.",
            })
        ))
        .await?;
        self.send_line("").await?;
        let (_, user) = config::current_owner_identity();
        self.send(&password_prompt_label(user.as_deref())).await?;
        self.flush().await?;
        let pw = self.get_password_input().await?;
        Ok(match pw {
            Some(s) if !s.is_empty() => Some(s),
            _ => None,
        })
    }
}
