//! The main menu's second page — restarting and shutting down the computer
//! the gateway runs on.
//!
//! **Unix only.** The whole module is `#[cfg(unix)]`, along with the `M` entry
//! on the main menu, its key handler and its half of the valid-key hint. There
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
    /// `sudo` answers without a password (NOPASSWD, or a live timestamp).
    SudoQuiet,
    /// `sudo` wants the operator's password.
    SudoPassword,
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
/// `Err` carries a line to show the operator; the only case is a machine with
/// no `sudo` at all, which is a fact about the installation and not something
/// a retry will fix.
pub(in crate::telnet) async fn probe_elevation(argv: &[&str]) -> Result<Elevate, String> {
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
    match tokio::process::Command::new("sudo")
        .args(["-n", "-l", "--"])
        .args(argv)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
    {
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
        Elevate::SudoQuiet => {
            let mut c = tokio::process::Command::new("sudo");
            c.arg("-n");
            c.args(argv);
            c
        }
        Elevate::SudoPassword => {
            let mut c = tokio::process::Command::new("sudo");
            c.args(["-S", "-p", ""]);
            c.args(argv);
            c
        }
    };
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

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
pub(in crate::telnet) const MORE_MENU_HINT: &str = "Press R, S, H, or Q.";

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
                "r" => {
                    if !self.power_action(PowerAction::Restart).await? {
                        return Ok(false);
                    }
                }
                "s" => {
                    if !self.power_action(PowerAction::Shutdown).await? {
                        return Ok(false);
                    }
                }
                _ => {
                    self.show_error(MORE_MENU_HINT).await?;
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
    pub(in crate::telnet) fn more_menu_rows(&self) -> Vec<String> {
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
        rows.push(format!("  {}  Restart the computer", self.cyan("R")));
        rows.push(format!("  {}  Shut down the computer", self.cyan("S")));
        rows.push(String::new());
        rows.push(format!(
            "  {}  {}",
            self.action_prompt("Q", "Back"),
            self.action_prompt("H", "Help")
        ));
        rows
    }

    async fn render_more_menu(&mut self) -> Result<(), std::io::Error> {
        self.clear_screen().await?;
        for row in self.more_menu_rows() {
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

        let elev = match probe_elevation(action.argv()).await {
            Ok(e) => e,
            Err(msg) => {
                self.show_error(&msg).await?;
                return Ok(true);
            }
        };

        // Held only as long as the two `sudo` calls need it: the check, and
        // the command itself.  It is re-fed rather than relying on sudo's
        // timestamp, because a machine with `timestamp_timeout=0` caches
        // nothing and the second call would then fail after the goodbye.
        let password = if elev == Elevate::SudoPassword {
            // **Per IP, not per session.**  The cap used to live on the
            // `TelnetSession`, so hanging up reset it: with `security_enabled`
            // off -- the default -- anyone who reaches the telnet port walks to
            // this prompt with no credential, and three guesses per connection
            // times `conn_rate_max` (20 a minute) is sixty PAM attempts a
            // minute against the operator's *system* account, from as many
            // addresses as the peer likes.  The cap that used to
            // live here offered `conn_rate_max` as the bound on exactly that,
            // and it is a bound -- just not at the scale that matters, since
            // `pam_faillock` denies at three.
            //
            // So it goes in the shared `LockoutMap`, the same counter the
            // telnet, SSH and web credentials use, for the reason that map is
            // already shared between them: a counter an attacker can reset by
            // reconnecting is not a counter.  `MAX_AUTH_ATTEMPTS` is 3, as
            // the per-session cap it replaced was, so one session behaves
            // exactly as before and only the reconnect changes.
            //
            // Checked here rather than in `authenticate`, because that runs
            // only when `security_enabled` is on and this prompt is reachable
            // when it is off -- which is the whole exposure.
            if let Some(ip) = self.peer_addr
                && crate::telnet::is_locked_out(&self.lockouts, ip)
            {
                glog!("Power: {} is locked out; not asking for a password", ip);
                self.show_error("Too many tries. Try again later.")
                    .await?;
                return Ok(true);
            }

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
                    if let Some(ip) = self.peer_addr {
                        let n = crate::telnet::record_auth_failure(&self.lockouts, ip);
                        glog!("Power: sudo refused for {} (failure {})", ip, n);
                    }
                    let stderr = String::from_utf8_lossy(&o.stderr);
                    let msg = sudo_error_line(&stderr, self.confirmation_content_width());
                    self.show_error(&msg).await?;
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
        let who = match self.peer_addr {
            Some(ip) => ip.to_string(),
            None => self.client_type_label().to_string(),
        };
        glog!(
            "Power: {} requested from the session menu by {}",
            action.argv().join(" "),
            who,
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
        // `$USER` is an environment variable, so it can be anything -- a
        // service account name is easily longer than a C64's whole row, and
        // this was the one string on these screens with no width at all while
        // the hostname ten lines up is carefully cut to 26.  16 leaves room
        // for `  Password for ` and the `: `.
        let (_, user) = config::current_owner_identity();
        let label = match user.as_deref().filter(|u| !u.trim().is_empty()) {
            Some(u) => format!(
                "  Password for {}: ",
                crate::webbrowser::truncate_to_width(u, 16)
            ),
            None => "  Password: ".to_string(),
        };
        self.send(&label).await?;
        self.flush().await?;
        let pw = self.get_password_input().await?;
        Ok(match pw {
            Some(s) if !s.is_empty() => Some(s),
            _ => None,
        })
    }
}
