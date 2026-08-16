//! CP/M emulator — a real CP/M 2.2 environment running on an emulated Z80
//! (or 8080 — `cpm_cpu`, the one CP/M key that serves a booted disk too),
//! reachable as its own main-menu item over telnet/SSH.
//!
//! This is a **completely separate** feature from the Gateway Shell
//! (`kernel.rs`), which is a pure-Rust CP/M-*styled* file manager with no CPU
//! emulation.  The emulator runs actual user-supplied `.COM` software in an
//! emulated CP/M 2.2 machine, sandboxed to a `CPM/` directory under
//! `transfer_dir` (one folder per drive A:–P:).  The design lives in these
//! module docs and in `src/cpm/mod.rs`; the CHANGELOG records how it was
//! built up in stages.
//!
//! ## Naming
//! The Gateway Shell owns the `cpm_` identifier prefix; the emulator uses
//! `cpmemu_` / the `cpm_emu_*` names (and the config key `cpm_emu_enabled`) to
//! keep the two unambiguous.
//!
//! ## Security (finalized B5)
//! The feature runs arbitrary Z80 code, so it stays gated behind
//! `cpm_emu_enabled` — now **on by default**, since the bounds below hold and
//! it ships with its own terminal (EGT8080) on drive A:.  When disabled the menu
//! item is hidden and `K` is rejected.  The guest's route off the machine is the
//! virtual modem, which now also defaults on (to the port EGT8080 expects), so a
//! fresh install can dial out from guest code; `cpm_emu_uart = off` closes that
//! without disabling the emulator.  The trusted-LAN posture is bounded on three axes:
//! - **Jail.** Every BDOS file call resolves through `CpmFs` under the
//!   `CPM/` container in `transfer_dir`: 8.3-name validation (no separators
//!   or `..`), a lexical `starts_with` check, and a canonical-path +
//!   symlink check (a symlink planted in a drive folder can't point out).
//!   Drive indices are clamped to A:–P:.
//! - **CPU.** A runaway is bounded by the configurable instruction ceiling
//!   (`cpm_emu_max_minstr`); the run loop yields every batch, and a
//!   double-`ESC` breaks out at any time — at a console prompt (in-band) and,
//!   via the between-batch out-of-band drain, even from a compute-bound
//!   program that never reads the console.
//! - **Memory/disk.** Each session's machine is a fixed 64 KB (bounded by
//!   `max_sessions`); a single emulated file is capped at 8 MB
//!   (`MAX_CPM_FILE_BYTES`) so a high random-record write can't spray a
//!   multi-gigabyte sparse file.  All BDOS read helpers are length-bounded.
//!
//! The emulator services only BDOS — it has no path to execute host
//! commands.  Outbound/inbound networking goes only through the gated virtual
//! modem (`cpm_modem`), which reuses the existing peer-dial/relay plumbing and
//! is bound by `allow_peer_dial`.  There is no per-drive file-*count* cap (a
//! guest can create many small files); bounded by host disk and acceptable
//! under the trusted-LAN model.
//!
//! ## Status
//! Entering the shell drops into our Rust CCP-lite `A>` prompt.  The full
//! console BDOS group (1/2/6/9/10/11/12) plus the disk/FCB group are wired,
//! so a verb that isn't a built-in is looked up as `<verb>.COM` on the
//! drive, loaded into the TPA with page zero set up (command tail + default
//! FCBs), and run — actual CP/M software (PIP, STAT, ASM, …) runs over
//! telnet/SSH.  The resident CP/M commands (DIR/ERA/REN/TYPE/SAVE/USER + the
//! `d:` drive change) are built in.  Guest output is translated from the
//! ADM-3A terminal to the connected client (ANSI/PETSCII/ASCII) and client
//! cursor keys back to ADM-3A codes (see `cpm_term`).  A gated virtual modem
//! (`cpm_modem`) lets the guest dial out (`ATD A`/`B` local, `ATD A@<ip>`
//! remote via the relay, `ATDT host:port` TCP) and be dialed as `CPM@<ip>`.  A
//! double-`ESC` always returns to `A>` — including from a runaway program —
//! via the out-of-band drain.
use super::*;
use super::cpm_modem::CpmModem;
use super::cpm_term::{self, Adm3a};
use crate::cpm::{parse_afn, parse_command_fcb, parse_dir_operand, split_8_3, Cpm, CpmFs, Fcb, Stop, FCB_SIZE, TPA_BASE, TPA_BYTES, TPA_TOP};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Instructions per [`Cpm::run`] batch before the driver regains control to
/// yield to the async runtime.
const CPM_RUN_BATCH: u64 = 200_000;

/// Poll `fut` exactly once: `Some(output)` if it is ready *right now*,
/// `None` if it would have to wait.  The future is dropped either way, so only
/// pass a cancel-safe one.
///
/// This exists because there is no cheap timer-free way to ask tokio "is this
/// ready?".  `timeout(Duration::ZERO, …)` is the obvious spelling and is what
/// [`TelnetSession::cpmemu_oob_drain`] used to use, but a zero deadline still
/// rounds up to the next timer tick — ~1.1 ms a call, on a path that runs once
/// per emulated console character.  See that function for the full measurement.
///
/// The no-op waker means a `Pending` future's wakeup is discarded, which is
/// correct here: the caller re-polls a fresh future on its next pass rather
/// than waiting to be woken.
/// Idle status polls tolerated at full speed before pacing starts.  Small, but
/// not 1: a program may legitimately poll a couple of times around doing real
/// work, and those passes should stay free.
pub(in crate::telnet) const IDLE_POLLS_BEFORE_NAP: u32 = 8;
/// First-tier nap: brief, because the program may be about to do something and
/// this is still the "just went quiet" case.
const IDLE_NAP: std::time::Duration = std::time::Duration::from_millis(1);
/// Consecutive idle polls after which the session is clearly parked waiting for
/// a human — roughly half a second at the first-tier rate.
pub(in crate::telnet) const IDLE_POLLS_LONG: u32 = 500;
/// Second-tier nap for a session idle a while.  Still far below the threshold
/// of noticing a keypress, and it matters on the small ARM boards this runs on,
/// where the first tier's ~1000 passes/sec is a real share of one slow core
/// rather than a rounding error.
const IDLE_NAP_LONG: std::time::Duration = std::time::Duration::from_millis(8);

/// How long to pause after `idle_polls` consecutive passes that were nothing but
/// a status call answering "nothing available"; `None` to keep running at full
/// speed.
///
/// A comms program's idle loop is exactly such a poll — `LD C,11 / CALL 5 / JR
/// Z` around a keyboard check — and because a status call ends the CPU batch,
/// each turn costs a full driver pass.  Once those passes became cheap (the
/// point of removing the timers from them) nothing was left to slow the loop
/// down, and an idle EGT8080 terminal spun the host at **161% CPU**; with this it
/// measures 1.4%.  Only the demonstrably idle case is paced, so throughput is
/// untouched: any pass doing real work resets the count to zero.
/// Consecutive reads of a port nothing answers before the loop is paced.
///
/// Generous, because a program that *inventories* the I/O space is exactly the
/// software the `0xFF` answer exists for — `survey.mac`, the CP/M program
/// z80pack's changelog names as its reason. A sweep of 256 ports crosses this
/// four times and pays a few milliseconds, once. A guest stuck on a port that
/// is not there crosses it forever.
pub(in crate::telnet) const UNCLAIMED_READS_BEFORE_NAP: u32 = 64;

/// How long to pause when the guest is reading hardware that is not there.
///
/// The same treatment [`idle_nap`] gives an idle console poll, for the same
/// reason and by a different route: there is no work being done, and the host
/// should not spend a core discovering that. See
/// [`crate::cpm::CpmMachine::unclaimed_reads`] for the measurement — 52-65% of
/// a core with `cpm_emu_uart = off` and the bundled terminal at its default
/// port, which is a *documented* configuration and not a contrived one.
///
/// Deliberately not a bigger hammer: this paces the loop, it does not stop it.
/// The guest still sees `0xFF`, still behaves exactly as it would on a real
/// machine with no board at that address, and is still bounded by
/// `cpm_emu_max_minstr` and by a double-`ESC`. What changes is only how much of
/// the host it costs while it does so.
pub(in crate::telnet) fn unclaimed_nap(reads: u32) -> Option<std::time::Duration> {
    (reads > UNCLAIMED_READS_BEFORE_NAP).then_some(IDLE_NAP)
}

pub(in crate::telnet) fn idle_nap(idle_polls: u32) -> Option<std::time::Duration> {
    if idle_polls > IDLE_POLLS_LONG {
        Some(IDLE_NAP_LONG)
    } else if idle_polls > IDLE_POLLS_BEFORE_NAP {
        Some(IDLE_NAP)
    } else {
        None
    }
}

/// Poll `fut` exactly once and drop it, returning `Some` only if it was already
/// ready.  The "is there anything waiting right now?" primitive of the emulator
/// driver loop.
///
/// This replaced `tokio::time::timeout(Duration::ZERO, …)`, which looks
/// equivalent and is not: tokio rounds a deadline up to the next timer tick, so
/// a zero-duration timeout cost ~1.1 ms against ~6 ns for a single poll — paid
/// per emulated character, because the driver regains control at every
/// BDOS/BIOS/HBIOS trap.
///
/// The waker is a no-op, which is sound *only* because nothing waits for a
/// wake-up: the driver loop polls a fresh future on its next pass, paced by
/// [`idle_nap`].  It follows that **every future passed here must be
/// cancel-safe**, since a `Pending` one is dropped mid-flight. The two callers
/// are: `AsyncReadExt::read` (documented cancel-safe — no data is consumed
/// unless it completes) and [`TelnetSession::session_read_byte`], which carries
/// a `mid_iac_cmd` resume point precisely so a cancel between an IAC and its
/// command byte cannot desynchronise telnet parsing.
pub(in crate::telnet) fn poll_once<F: std::future::Future>(fut: F) -> Option<F::Output> {
    let mut fut = std::pin::pin!(fut);
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    match fut.as_mut().poll(&mut cx) {
        std::task::Poll::Ready(v) => Some(v),
        std::task::Poll::Pending => None,
    }
}

/// Highest emulated drive letter (A:–P:, the 16 drives CP/M 2.2 allows).
const CPM_LAST_DRIVE: u8 = b'P';

/// The emulator's sign-on line.
pub(in crate::telnet) const CPM_BANNER: &str = "CP/M 2.2 (iz80).  Type HELP.";

/// An extra line, drawn only when `cpm_cpu` selects the 8080.
///
/// A line of its own rather than a re-worded banner: 40 PETSCII columns cannot
/// hold the version, the processor *and* this, and the first attempt at it
/// dropped "Type HELP." to make room — trading away the only pointer to the
/// command list, on the screen where a new operator meets the emulator. This
/// screen is about 15 rows against a budget of 22, so the unusual case can
/// simply have a row (the boot screen, which is full, could not).
///
/// **It has been a warning, a chooser, a signpost, and is a chooser again.**
/// It began as "EGT80 needs Z80" when the only bundled terminal was Z80 code
/// that would crash the machine the operator had just selected; it then named
/// which of two files to type; for 0.9.2 it was a bare signpost, because one
/// terminal shipped and it ran on either processor. Two ship again — EGT80
/// carries the Z180 ASCI ports that cannot exist in an 8080 binary — so this
/// row is once more the line that says which of the two on drive A: is safe to
/// type under the setting in force. Under the 8080, EGT80 stops at its first
/// Z80-only opcode, and so does the copy an operator who upgraded from before
/// 0.9.2 already had there.
pub(in crate::telnet) const CPM_NOTE_8080: &str = "8080 selected.  Run EGT8080.";

/// EGT8080, the gateway's own CP/M terminal, carried inside the binary and
/// placed on drive A: and in the transfer directory when the drive folders are
/// created (see [`TelnetSession::cpmemu_place_egt80`]).  `include_bytes!` means
/// a release ships one file and the terminal is simply *there* when someone
/// first opens the emulator, rather than being something to find and upload.
///
/// **The build that runs anywhere.** 8080 opcodes are a strict subset of the
/// Z80's, so this one runs under *either* `cpm_cpu` setting and on any machine
/// the emulator imitates.  It is first in [`BUNDLED_TERMINALS`] for that
/// reason: a reader who takes the first name they see has to be right on both
/// settings.
///
/// It is not the only one, and 0.9.2 briefly made it so.  See [`EGT80_COM`] for
/// the machine that cannot be served from here.
const EGT8080_COM: &[u8] = include_bytes!("../../EGT8080/EGT8080.COM");

/// EGT80, the Z80 build of the same program, placed beside EGT8080.
///
/// **It exists for one family of ports, and no care in the 8080 build could
/// provide them.** A Z180 board — an SC126, or any RomWBW machine of that
/// shape — drives its console from the ASCI channels *inside* the processor.
/// Reaching them needs `IN0`/`OUT0`, and knowing a Z180 is there at all needs
/// `MLT BC`; all three are ED-prefixed instructions, and on a true 8080 an ED
/// byte is an undocumented CALL, so such a probe does not fail — it jumps into
/// the weeds.  The bytes therefore cannot be in a binary that must also run on
/// an 8080, which is why EGT8080 offers four port families and this one offers
/// five.
///
/// 0.9.2 retired this build on the reasoning that one binary running everywhere
/// beats two that need choosing between.  That was right about every machine
/// except the Z180, and the Z180 is the one an SC126 owner has: EGT8080 reaches
/// their board only through RomWBW's HBIOS, and a machine whose console is an
/// ASCI port and whose firmware is not RomWBW had nothing left at all.
///
/// **The cost is real and is why EGT8080 leads.** This is Z80 code: run it with
/// `cpm_cpu = 8080` and the machine stops at the first Z80-only opcode.  Hence
/// [`CPM_NOTE_8080`], printed on the sign-on when that setting is in force,
/// names EGT8080 and only EGT8080.
///
/// The two must never share a filename or a settings slot — each saves its
/// configuration inside its own `.COM`, and an operator who upgraded from
/// before 0.9.2 already has an `EGT80.COM` of their own with their ports in it.
/// Placement never overwrites, so theirs wins, which is the right answer: it is
/// the same program and it holds their settings.
const EGT80_COM: &[u8] = include_bytes!("../../EGT8080/EGT80.COM");


/// How many sessions are inside the CP/M emulator right now.
///
/// Every session runs its own Z80 and its own 64 KB, but they all share one set
/// of drive folders under `transfer_dir/CPM` — so "who else is in here" is a
/// fact an arriving user needs.  Simultaneous writes to one file are refused
/// outright (see `CPM_WRITERS` in `cpm/fs.rs`); this only supplies the notice,
/// because a refusal that arrives with no explanation reads as a bug.
static CPM_SESSIONS: AtomicUsize = AtomicUsize::new(0);

/// Counts a session in for as long as it is in the emulator.
///
/// RAII rather than a decrement at the end of the REPL: the session can leave
/// through `EXIT`, a dropped connection, or an error return, and only `Drop`
/// covers all three.
struct CpmSessionCount;

impl CpmSessionCount {
    /// Join, reporting how many sessions were already inside.
    fn enter() -> (CpmSessionCount, usize) {
        let before = CPM_SESSIONS.fetch_add(1, Ordering::SeqCst);
        (CpmSessionCount, before)
    }
}

impl Drop for CpmSessionCount {
    fn drop(&mut self) {
        CPM_SESSIONS.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Filename EGT8080 is placed under.  It is also the name it looks for when
/// saving its settings (CP/M never tells a program its own name, so the name
/// is compiled into it) — renaming the file costs the user that feature.
const EGT8080_NAME: &str = "EGT8080.COM";

/// Filename EGT80 is placed under, on the same terms.
///
/// Eight characters is all an FCB holds, so these two names are as close as
/// two builds can be while remaining distinct owners of their own settings —
/// and distinct they must be, because `EGT8080` *contains* `EGT80` and a
/// substring check on either would pass on the other.
const EGT80_NAME: &str = "EGT80.COM";

/// The terminals shipped inside the binary, placed on drive A: and in the
/// transfer directory.
///
/// A table rather than a call, so the rules that matter — never overwrite,
/// write-and-rename, a failure is logged and not fatal — are stated once. A
/// build is one row, which is how the 8080 one arrived, how the Z80 one left
/// in 0.9.2, and how it came back when an SC126's Z180 console proved that one
/// binary could not serve every machine after all.
///
/// **EGT8080 is first and that is load-bearing** — see
/// [`test_the_terminal_that_runs_on_both_comes_first`].
const BUNDLED_TERMINALS: &[(&str, &[u8])] =
    &[(EGT8080_NAME, EGT8080_COM), (EGT80_NAME, EGT80_COM)];

/// Is the console-input byte trace armed?
///
/// **A diagnostic, not a setting.** It exists to answer one question on real
/// hardware — what does the gateway actually receive when a key is pressed on
/// an SC126 running EGT80, and what does the escape state machine do with it —
/// and questions like that are answered once and then stop being interesting.
/// A config key would cost three screens and a manual entry for ever; an
/// environment variable costs a line, and follows the `EGATEWAY_GATEWAY_DEBUG`
/// precedent already in `config.rs`.
///
/// Read once: an operator cannot change their mind mid-session, and this is
/// consulted on every keystroke.
pub(in crate::telnet) fn keytrace_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("EGATEWAY_CPM_KEYTRACE")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    })
}

/// A run of bytes, compactly, for the outbound half of the key trace.
///
/// Printable bytes stay as themselves so a result code reads as `OK` rather
/// than as six hex pairs; control bytes become their names, which is the
/// whole point when the question is "did a CR LF come back".  Truncated at
/// `max` because a screen paint is one write of several hundred bytes and the
/// interesting part is always the front.
pub(in crate::telnet) fn render_bytes(bytes: &[u8], max: usize) -> String {
    let mut out = String::new();
    for &b in bytes.iter().take(max) {
        match b {
            0x20..=0x7E => out.push(b as char),
            _ => {
                out.push('<');
                out.push_str(&keyname(b));
                out.push('>');
            }
        }
    }
    if bytes.len() > max {
        out.push_str(&format!("… (+{} more)", bytes.len() - max));
    }
    out
}

/// A byte as a human reads it on a key-trace line.
///
/// Control codes are the whole point here, so they get names rather than the
/// caret notation a CP/M console would echo — `ESC` and `^Y` are what the
/// operator pressed, and matching the words to the keycap is what makes a
/// trace readable at a glance.
pub(in crate::telnet) fn keyname(b: u8) -> String {
    match b {
        0x1B => "ESC".to_string(),
        0x0D => "CR".to_string(),
        0x0A => "LF".to_string(),
        0x08 => "BS".to_string(),
        0x09 => "TAB".to_string(),
        0x7F => "DEL".to_string(),
        b if b < 0x20 => format!("^{}", (b + 0x40) as char),
        b if b.is_ascii_graphic() || b == b' ' => format!("'{}'", b as char),
        _ => "high".to_string(),
    }
}

/// Put both bundled terminals in both places they belong, if they are missing.
///
/// **Four files**: each of the two builds goes on CP/M drive A:, where the
/// emulator runs it, *and* loose in the transfer directory, where the
/// file-transfer menus can see it.  Drive A: lives inside `CPM/`, which those
/// menus do not list, so without the second copy the only way to get a terminal
/// onto real hardware is to start the emulator and send it from inside — which
/// is backwards, since the reason to want the file is usually that you have no
/// terminal on the far end yet.  Copies rather than a move: CP/M has to find it
/// on A:, the transfer-dir copy stays the pristine shipped build holding no
/// settings, so an operator who has configured theirs on A: still has exactly
/// one file that remembers and knows which.
///
/// **Only when absent, never overwriting.** Each terminal saves its settings —
/// the selected serial port, the ANSI/ASCII choice, the menu key — into a patch
/// area inside its own `.COM` file, so refreshing the copy on every launch would
/// silently throw away the user's configuration.  It also means a user may
/// deliberately keep an older build, or their own with different defaults.
/// Deleting a file restores the shipped copy on the next launch, which is the
/// documented way back to a known state.
///
/// A failure is logged and ignored rather than propagated: not having the
/// bundled terminal is a missing convenience, and it must not stop someone
/// reaching a CP/M prompt to run their own software.
///
/// **A free function, and synchronous, because two callers need it and one of
/// them is start-up.** It used to be an `async` method on `TelnetSession`
/// reachable only from `cpmemu_ensure_drives`, i.e. only once somebody entered
/// the emulator — so erasing the transfer directory and restarting recreated
/// the drive folders with no terminal in any of them, and the loose copy whose
/// whole purpose is "reach it without starting the emulator" required starting
/// the emulator.  It never touched `self`.
/// `enabled` is `place_bundled_terminals` from the config.  It is a parameter
/// rather than a `get_config()` read in here so the behaviour stays testable
/// without a global, and so both callers are visibly passing the same key --
/// the alternative was a gate at each call site, which is one rule written in
/// two places.
pub(crate) fn place_bundled_terminals(transfer_dir: &str, enabled: bool) {
    if !enabled {
        return;
    }
    for (name, bytes) in BUNDLED_TERMINALS {
        let mut drive_a = PathBuf::from(transfer_dir);
        drive_a.push("CPM");
        drive_a.push("A");
        place_one_terminal(&drive_a, "drive A:", name, bytes);
        place_one_terminal(
            std::path::Path::new(transfer_dir),
            "the transfer directory",
            name,
            bytes,
        );
    }
}

/// Put one bundled terminal in `dir` if it is not already there.
///
/// Split out when the second build arrived: copies of "never overwrite,
/// write-and-rename, log a failure" would have been several places for the
/// settings-preserving rule to hold, and it only has to fail in one of them to
/// throw away a user's configuration.  It now serves two destinations for the
/// same reason.
fn place_one_terminal(dir: &std::path::Path, where_: &str, name: &str, bytes: &[u8]) {
    let path = dir.join(name);
    if path.exists() {
        return; // already there — leave it, settings and all
    }
    // Written to a temporary name and renamed into place, the way `config.rs`
    // writes the config file.  Start-up and a session entering the emulator can
    // both find the file absent and both write it; a plain write would let a
    // CP/M program load a half-written image.  A rename is atomic, so every
    // reader sees either no file or the whole one.  The temporary name carries
    // the process id so two gateways sharing a transfer directory cannot
    // collide either.
    let tmp = path.with_extension(format!("t{}", std::process::id()));
    let placed = match std::fs::write(&tmp, bytes) {
        Ok(()) => std::fs::rename(&tmp, &path),
        Err(e) => Err(e),
    };
    match placed {
        Ok(()) => glog!(
            "CP/M: placed the bundled {} ({} bytes) in {}",
            name,
            bytes.len(),
            where_
        ),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp); // don't leave litter
            glog!("CP/M: could not place {} in {}: {}", name, where_, e);
        }
    }
}

/// Outcome of a single console-input read while a program runs.
enum ConIn {
    /// A translated data byte to hand to the guest.
    Byte(u8),
    /// The user pressed `ESC` twice — abort the program back to `A>`.
    BreakOut,
    /// The session closed (or idled out) — leave the emulator entirely.
    Disconnect,
}

/// Outcome of a BDOS-10 read-console-buffer line read.
enum LineRead {
    /// A completed input line (CR-terminated), edited bytes only.
    Line(Vec<u8>),
    /// The user pressed `ESC` twice mid-line — abort the program back to `A>`.
    BreakOut,
    /// The session closed (or idled out) — leave the emulator entirely.
    Disconnect,
}

/// RAII registration of the CP/M emulator as the dialable `CPM@<ip>` peer
/// endpoint: registered while a modem-enabled shell is active, unregistered
/// (dropping any unclaimed call) on every exit path.  On a slave gateway it
/// also owns the crossbar announcer task (registering `CPM` with the master),
/// which it stops + aborts on drop.
pub(in crate::telnet) struct CpmPeerReg {
    /// `Some` while this session owns the single crossbar announcer.
    announce: Option<(std::sync::Arc<AtomicBool>, tokio::task::JoinHandle<()>)>,
}
impl Drop for CpmPeerReg {
    fn drop(&mut self) {
        crate::serial::cpm_peer_listen_exit();
        if let Some((stop, jh)) = self.announce.take() {
            stop.store(true, std::sync::atomic::Ordering::SeqCst);
            jh.abort();
            crate::serial::cpm_announce_release();
        }
    }
}

/// Join the inbound `CPM@<ip>` call pool for as long as the guard lives.
///
/// `CPM@<ip>` is a single dialable address, but every modem-enabled CP/M
/// session joins the pool and any idle member answers the next inbound call (a
/// hunt group), so two concurrent CP/M users can both receive calls.  On a
/// slave with peer-dial + a master, exactly one pool member (the announce-owner)
/// announces `CPM` to the master so a remote `CPM@<this-slave-ip>` dial reaches
/// the gateway via the crossbar; if that member exits while others remain,
/// remote reachability pauses until a new session re-announces (local answering
/// is unaffected).
///
/// Shared by the emulator and the booted-disk driver: a guest with a virtual
/// modem is dialable whichever of the two it is running under, and having one
/// copy of this is what keeps that true.
pub(in crate::telnet) fn cpm_peer_register(modem_enabled: bool) -> Option<CpmPeerReg> {
    if !modem_enabled {
        return None;
    }
    crate::serial::cpm_peer_listen_enter();
    let cfg = config::get_config();
    // Announcing to our own master is not gated on `allow_peer_dial` (see
    // serial::cpm_slave_announce): that setting governs dialing arbitrary
    // peers, and without the announcement the master cannot reach this slave's
    // CP/M endpoint at all.
    let announce = if cfg.gateway_role == "slave"
        && !cfg.slave_master_host.is_empty()
        && crate::serial::cpm_announce_claim()
    {
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let jh = tokio::spawn(crate::serial::cpm_slave_announce(stop.clone()));
        Some((stop, jh))
    } else {
        None
    };
    Some(CpmPeerReg { announce })
}

/// Outcome of the between-batch out-of-band input drain.
enum OobDrain {
    /// Nothing actionable — buffered bytes (if any) were queued for the guest.
    Continue,
    /// A double-`ESC` was seen out-of-band — break the running program to `A>`.
    BreakOut,
    /// The session closed.
    Disconnect,
}

/// Reassemble a buffered ANSI CSI arrow (`ESC [ A..D`) from the pending-input
/// queue, given the leading `ESC` was already popped.  Consumes the `[` and the
/// final byte and returns the ADM-3A key code on a plain arrow; otherwise
/// leaves the queue untouched (the `ESC` is delivered raw).
pub(in crate::telnet) fn pending_csi_arrow(pending: &mut VecDeque<u8>) -> Option<u8> {
    if pending.front() != Some(&b'[') {
        return None;
    }
    let code = cpm_term::csi_arrow_to_adm3a(*pending.get(1)?)?;
    pending.pop_front(); // '['
    pending.pop_front(); // final (A..D)
    Some(code)
}

/// Result of peeking after an `ESC` for an ANSI CSI arrow sequence.
enum ArrowPeek {
    /// A recognised arrow → this ADM-3A key code.
    Arrow(u8),
    /// A full `ESC [ x` was consumed but isn't an arrow — swallow it.
    UnknownCsi,
    /// Not a CSI (lone `ESC`, or a non-`[` follower that was pushed back).
    NotCsi,
}

impl TelnetSession {
    /// Emulator entry point, invoked from the gated `K` main-menu handler.
    ///
    /// B2: ensure the `CPM/` drive folders exist, print the boot banner,
    /// then run the Rust CCP-lite `A>` REPL until the user types
    /// `EXIT`/`BYE`/`QUIT` (or disconnects).
    pub(in crate::telnet) async fn cpmemu_shell(&mut self) -> Result<(), std::io::Error> {
        self.cpmemu_ensure_drives().await?;

        // The operator may have pointed CP/M at a disk instead of at us.  Done
        // before the emulator's banner rather than inside it, because the two
        // are different machines and pretending otherwise is what makes booting
        // confusing: nothing below this line — the drives, EGT8080, the `A>`
        // prompt — exists inside a booted disk.
        if let Some(path) = self.cpmemu_boot_target().await {
            // Both answers come from the config now, because this is the only
            // way to boot.  The per-visit picker on the disks screen asked them
            // as questions and was removed in 0.9.2 -- two ways to boot one
            // disk, differing in what they asked and what they remembered, was
            // the confusion it caused.
            //
            // `cpm_boot_writable` is a standing setting rather than the
            // per-visit question the picker asked, so it answers for *every*
            // visitor at once — which is why the key says so in
            // `egateway.conf` and on all three screens rather than reading as
            // a convenience.  It defaults on: a booted OS that cannot save is
            // the more surprising machine, and the disks are replaceable.
            let cfg = config::get_config();
            let erase = crate::cpm::boot::backspace_erases(&cfg.cpm_boot_backspace);
            let writable = cfg.cpm_boot_writable;
            drop(cfg);
            return self.cpm_boot_session(&path, writable, erase).await;
        }

        // The processor this visit runs on, read ONCE.  The banner below, the
        // machine `cpmemu_repl` builds and what `VER` reports all come from
        // this one value: another session can save the config while this one is
        // running, and a screen that says one processor while the machine runs
        // another is the failure every other CP/M setting here is careful to
        // avoid.
        let cpu_setting = config::get_config().cpm_cpu.clone();

        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;
        self.send_line(&format!("  {}", self.yellow("CP/M SYSTEM")))
            .await?;
        self.send_line(&sep).await?;
        self.send_line("").await?;
        // Split across two lines because the whole sentence is 66 columns and a
        // PETSCII screen is 40.  Both halves fit, so one wording serves every
        // terminal rather than branching on width.
        self.send_line(&format!(
            "  {}",
            self.amber("WARNING: Be sure you trust the CP/M")
        ))
        .await?;
        self.send_line(&format!(
            "  {}",
            self.amber("files you run in the emulator.")
        ))
        .await?;
        self.send_line(&format!("  {}", self.dim(CPM_BANNER))).await?;
        // One extra line when the processor is not the default: it names what
        // was selected and what that costs, on the screen where the operator is
        // about to type a program's name.  Both lines are measured against the
        // 40-column PETSCII width by `test_cpm_banner_lines_fit_petscii`.
        if crate::cpm::cpu::is_8080(&cpu_setting) {
            self.send_line(&format!("  {}", self.amber(CPM_NOTE_8080))).await?;
        }
        // Boot-banner memory report, as a real CP/M system prints on cold start.
        self.send_line(&format!("  {}", self.dim(&Self::cpmemu_tpa_line())))
            .await?;
        // The two things a user needs before running a program: how to
        // leave the emulator, and how to stop a running program.
        self.send_line(&format!(
            "  {}",
            self.amber("Type EXIT to return to the gateway.")
        ))
        .await?;
        self.send_line(&format!(
            "  {}",
            self.amber("Press ESC twice to stop a program.")
        ))
        .await?;
        // Counted in for the whole visit; the guard releases on every exit
        // path.  Held until `cpmemu_repl` returns, so it must outlive it.
        let (_session_count, already_inside) = CpmSessionCount::enter();
        if already_inside > 0 {
            self.send_line(&format!(
                "  {}",
                self.amber(&format!(
                    "{} other session(s) are in CP/M: the drives are shared.",
                    already_inside
                ))
            ))
            .await?;
            self.send_line(&format!(
                "  {}",
                self.dim("A file being written by one session is refused to the others.")
            ))
            .await?;
        }
        self.send_line("").await?;

        // The filesystem state (current drive, DMA) persists across the
        // whole session at the `CPM/` container.  Canonicalize so the jail
        // prefix check compares absolute paths.
        let cfg = config::get_config();
        let mut base = PathBuf::from(&cfg.transfer_dir);
        base.push("CPM");
        let base = std::fs::canonicalize(&base).unwrap_or(base);
        let mut fs = CpmFs::new(base);

        self.cpmemu_repl(&mut fs, &cpu_setting).await
    }

    /// One byte from the guest's `LST:` device — BDOS 5 or the BIOS `LIST`
    /// vector, which are the same device reached two ways.
    ///
    /// With `cpm_printer` off this is the terminal, which is where printer
    /// output has always gone here; nothing an existing operator relies on
    /// changes when they upgrade. With it on, the byte joins the spool and
    /// **nothing is echoed**: a document being written to disk and simultaneously
    /// sprayed over the screen would make a WordStar print look like a crash.
    ///
    /// The size bound closes the job and starts another rather than dropping the
    /// byte, so a runaway prints many documents instead of one enormous one and
    /// nothing is silently lost.
    ///
    /// `format` and `transfer_dir` are **passed in, not read here**: this is
    /// called once per printed character, and `config::get_config()` clones the
    /// whole `Config` — some twenty owned `String`s — under a mutex. A 60 KB
    /// document would have paid that sixty thousand times. The setting is
    /// sampled once when the program starts instead, which is still finer
    /// grained than the boot path, where it is sampled once per boot.
    async fn cpmemu_print(
        &mut self,
        spool: &mut Option<crate::cpm::printer::SpoolJob>,
        term: &mut Adm3a,
        byte: u8,
        format: Option<crate::cpm::printer::Format>,
        auto_lf: bool,
        transfer_dir: &str,
    ) -> Result<(), std::io::Error> {
        let Some(format) = format else {
            return self.cpmemu_emit(term, &[byte]).await;
        };
        let job =
            spool.get_or_insert_with(|| crate::cpm::printer::SpoolJob::with_auto_lf(auto_lf));
        job.push(byte);
        if job.is_full() {
            self.cpmemu_spool_close(spool, format, transfer_dir).await?;
        }
        Ok(())
    }

    /// Write a finished job out and say where it went.
    ///
    /// The notice matters: a print that leaves no mark on the screen is
    /// indistinguishable from a print that did not happen, and the operator has
    /// no other way to learn the file name. Failure is reported to the session
    /// *and* the log, because this is the one part of printing the user cannot
    /// see for themselves.
    async fn cpmemu_spool_close(
        &mut self,
        spool: &mut Option<crate::cpm::printer::SpoolJob>,
        format: crate::cpm::printer::Format,
        transfer_dir: &str,
    ) -> Result<(), std::io::Error> {
        let Some(job) = spool.take() else { return Ok(()) };
        if job.is_empty() {
            return Ok(());
        }
        let bytes = job.len();
        match job.write(transfer_dir, format) {
            Ok(name) => {
                self.send_line("").await?;
                self.send_line(&format!(
                    "  {}",
                    self.green(&format!("[printed {bytes} bytes to {name}]"))
                ))
                .await?;
            }
            Err(e) => {
                glog!("CP/M printer: could not write the spool file: {e}");
                self.send_line("").await?;
                self.send_line(&format!("  {}", self.red(&format!("[printer: {e}]"))))
                    .await?;
            }
        }
        Ok(())
    }

    /// The disk `cpm_boot_image` names, if it names one that is really there.
    ///
    /// A missing or malformed name falls back to the emulator and says so in
    /// the log rather than refusing to open CP/M at all: the setting is a
    /// preference about which machine to run, and an operator who deletes an
    /// image should lose the boot, not the whole feature.
    ///
    /// **The fallback is [`crate::cpm::boot::boot_target`]'s and not ours**, so
    /// that the three disks screens — which describe the machine this decides —
    /// cannot answer differently from the code that starts it. What is left
    /// here is the announcing: this is the one caller with a session behind it,
    /// and it runs once per visit rather than once per frame.
    ///
    /// The `stat` is synchronous. It is a single one at session entry, on the
    /// same local folder this path is about to read a whole disk image out of,
    /// and every other CP/M screen here already stats from an async fn.
    async fn cpmemu_boot_target(&mut self) -> Option<PathBuf> {
        let cfg = config::get_config();
        let target = crate::cpm::boot::boot_target(&cfg.transfer_dir, &cfg.cpm_boot_image);
        match &target {
            crate::cpm::boot::BootTarget::UnsafeName(name) => {
                glog!("CP/M: cpm_boot_image '{}' is not a valid image name — running the emulator", name);
            }
            crate::cpm::boot::BootTarget::Missing(name) => {
                glog!("CP/M: cpm_boot_image '{}' is not in CPM/images — running the emulator", name);
            }
            // The third fallback, and the one that would otherwise be silent
            // in the worst way: the disk IS there, so nothing looks wrong.
            // Before the boot lists began cold-starting images, a disk like
            // this reached the boot session and printed its `BootError` in red
            // on the operator's terminal; now it never gets that far, so
            // without this line the only trace is a `(not bootable)` mark on a
            // configuration screen they have no reason to open.
            crate::cpm::boot::BootTarget::NotBootable(name) => {
                glog!(
                    "CP/M: cpm_boot_image '{}' carries no boot program — running the emulator. \
                     A disk of programs for another disk is one to MOUNT, not to boot.",
                    name
                );
            }
            _ => {}
        }
        target.into_image()
    }

    /// Ensure `CPM/` and each drive folder `CPM/A`..`CPM/P` exist under
    /// `transfer_dir`, creating any that are missing.  Idempotent and run
    /// on every launch, so a program can select any of the 16 drives without
    /// hitting a "drive does not exist" error.  An empty folder *is* a
    /// formatted, ready-to-use drive here — the CP/M directory is synthesized
    /// from the folder's real files, so there is nothing to `CLRDIR`/format.
    /// Jailed by construction —
    /// the paths are built under the configured `transfer_dir`.
    pub(in crate::telnet) async fn cpmemu_ensure_drives(&mut self) -> Result<(), std::io::Error> {
        let cfg = config::get_config();
        // The same layout the enable-time hook builds, so a folder deleted
        // since then is recreated and the two paths cannot disagree about what
        // the container holds.
        let transfer_dir = cfg.transfer_dir.clone();
        tokio::task::spawn_blocking(move || crate::cpm::layout::ensure_cpm_tree(&transfer_dir))
            .await
            .map_err(std::io::Error::other)??;
        // Also done at start-up (see `main.rs`).  Repeated here because a
        // folder deleted while the gateway is running should be repaired by the
        // next session rather than waiting for a restart -- and it is cheap:
        // four `exists()` checks when everything is already in place.
        let td = cfg.transfer_dir.clone();
        let on = cfg.place_bundled_terminals;
        tokio::task::spawn_blocking(move || place_bundled_terminals(&td, on)).await.ok();
        // Bring up whatever `cpm_mounts` asks for.  Idempotent: a drive already
        // holding the requested image is left alone, so a second session
        // entering the emulator does not reopen a disk the first one is using.
        let mut base = PathBuf::from(&cfg.transfer_dir);
        base.push("CPM");
        let base = std::fs::canonicalize(&base).unwrap_or(base);
        let mounts = cfg.cpm_mounts.clone();
        tokio::task::spawn_blocking(move || {
            crate::cpm::image::apply_config_mounts(&base, &mounts);
        })
        .await
        .ok();
        Ok(())
    }


    /// The Rust CCP-lite command loop.  Prints the `A>` prompt, reads a
    /// line, and dispatches: host-exit words leave; built-ins run; anything
    /// else is looked up as `<verb>.COM` on the drive and run as a real
    /// transient program, falling back to CP/M's bad-verb error (`VERB?`)
    /// when no such file exists.
    async fn cpmemu_repl(
        &mut self,
        fs: &mut CpmFs,
        cpu_setting: &str,
    ) -> Result<(), std::io::Error> {
        // One machine persists for the whole session: the TPA (and the low
        // vectors, reinstalled each load) survive across program runs, so a
        // warm-boot back to `A>` leaves the last program's memory image in
        // place — which is what makes SAVE authentic (dump the TPA a prior
        // program, e.g. DDT, left behind).
        // On the processor the operator picked -- the same key a booted disk
        // reads, because a machine that ran one CPU here and another there
        // would be two answers to one question.
        let mut cpm = Cpm::new_for(cpu_setting);
        // Wire the virtual-modem access (if the operator selected one) so a
        // CP/M comms program finds its modem at the configured machine ports
        // or on the BDOS AUX: device.  The modem "brain" (AT layer + outbound
        // dial) persists for the whole session alongside the machine.
        let access = crate::cpm::resolve_access(&config::get_config().cpm_emu_uart);
        cpm.set_modem_access(access);
        let mut modem = CpmModem::new(access != crate::cpm::ModemAccess::Off);
        // Let the guest dial the gateway it is running inside
        // (`ATDT ethernetgateway`); that spawns a menu session of its own.
        modem.set_menu_context(
            self.shutdown.clone(),
            self.restart.clone(),
            self.lockouts.clone(),
        );
        // Join the inbound `CPM@<ip>` call pool (RAII-released on any exit).
        // `CPM@<ip>` is a single dialable address, but every modem-enabled
        // CP/M session joins the pool and any idle member answers the next
        // inbound call (a hunt group), so two concurrent CP/M users can both
        // receive calls.  On a slave with peer-dial + a master, exactly one
        // pool member (the announce-owner) announces `CPM` to the master so a
        // remote `CPM@<this-slave-ip>` dial reaches the gateway via the
        // crossbar; if that member exits while others remain, remote
        // reachability pauses until a new session re-announces (local
        // answering is unaffected).
        let _peer_reg = cpm_peer_register(modem.enabled());
        // The CCP-lite's own default drive (real CP/M's `CDISK`).  A transient
        // may SELECT another drive (BDOS 14) while it runs — STAT does exactly
        // this for `STAT B:`, to read B:'s free space — but the real CCP
        // re-selects its own default at the top of every command cycle, so
        // that change never sticks to the prompt.  Only a bare `d:` command
        // (below) moves the CCP default.  Without this, `A>STAT B:` left the
        // user stranded at `B>`.
        let mut ccp_drive = fs.current_drive();
        loop {
            // Re-establish the CCP default each cycle, undoing any drive a
            // just-finished transient selected for its own use.
            fs.select(ccp_drive);
            let prompt = self.cyan(&format!("{}>", fs.current_drive_letter()));
            self.send(&prompt).await?;
            self.flush().await?;

            // A running batch supplies the command instead of the keyboard,
            // and the CCP echoes it so the operator can see what ran (CCP22's
            // `PMSG` after the read).  `submitted` is remembered because an
            // unrecognised command has to abort the batch, as it does on real
            // CP/M ("if an error is encountered, the $$$.SUB file is erased").
            let submitted_line = Self::cpmemu_next_submit_line(fs);
            let submitted = submitted_line.is_some();
            let line = match submitted_line {
                Some(s) => {
                    self.send_line(&s).await?;
                    s
                }
                None => match self.get_line_input().await? {
                    Some(s) => s,
                    None => return Ok(()), // disconnected
                },
            };
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let verb = trimmed
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_ascii_uppercase();

            // Drive change: "A:".."H:" selects that drive (CCP convention).
            if verb.len() == 2 && verb.ends_with(':') {
                let d = verb.as_bytes()[0];
                if (b'A'..=CPM_LAST_DRIVE).contains(&d) {
                    fs.select(d - b'A');
                    ccp_drive = d - b'A'; // a bare `d:` moves the CCP default
                } else {
                    self.send_line(&format!("  {}?", self.red(&verb))).await?;
                    if submitted {
                        Self::cpmemu_abort_submit(fs);
                    }
                }
                continue;
            }

            match verb.as_str() {
                "EXIT" | "BYE" | "QUIT" => {
                    // Leaving is a break: don't strand a half-consumed batch
                    // for the next session to resume out of nowhere.
                    Self::cpmemu_abort_submit(fs);
                    return Ok(());
                }
                "HELP" | "?" => self.cpmemu_help().await?,
                "VER" | "VERSION" => {
                    // Names the processor actually running rather than the one
                    // that used to be the only choice: `VER` is where a guest
                    // program's author looks when something decodes oddly, and
                    // a machine that reports a Z80 while running an 8080 sends
                    // them to the wrong place.
                    self.send_line(&format!(
                        "  {}",
                        self.green(&format!(
                            "CP/M 2.2 emulator (iz80 {} core)",
                            if crate::cpm::cpu::is_8080(cpu_setting) { "8080" } else { "Z80" }
                        ))
                    ))
                    .await?;
                    self.send_line(&format!("  {}", self.dim(&Self::cpmemu_tpa_line())))
                        .await?;
                }
                "DIR" => self.cpmemu_dir(fs, trimmed).await?,
                "ERA" | "DEL" => self.cpmemu_era(fs, trimmed).await?,
                "REN" | "RENAME" => self.cpmemu_ren(fs, trimmed).await?,
                "TYPE" => self.cpmemu_type(fs, trimmed).await?,
                "SAVE" => self.cpmemu_save(&mut cpm, fs, trimmed).await?,
                "USER" => self.cpmemu_user(trimmed, fs).await?,
                "HELLO" => {
                    // Non-interactive BDOS print-string demo.
                    if !self.cpmemu_run_program(&mut cpm, &mut modem, &Self::cpmemu_demo_hello(), "", fs).await? {
                        return Ok(());
                    }
                }
                "ECHO" => {
                    // Interactive demo: echoes typed keys (exercises CONIN);
                    // press '.' to end, or double-ESC to break out.
                    self.send_line(&format!(
                        "  {}",
                        self.dim("Echoing keys; '.' ends, ESC ESC aborts.")
                    ))
                    .await?;
                    if !self.cpmemu_run_program(&mut cpm, &mut modem, &Self::cpmemu_demo_echo(), "", fs).await? {
                        return Ok(());
                    }
                }
                other => {
                    // Not a built-in: try to load and run `<verb>.COM` from
                    // the drive.  `None` = no such file (CP/M prints VERB?).
                    match self.cpmemu_try_run_com(&mut cpm, &mut modem, fs, &verb, trimmed).await? {
                        Some(true) => {}                    // ran; back to A>
                        Some(false) => return Ok(()),       // session gone
                        None => {
                            self.send_line(&format!("  {}?", self.red(other))).await?;
                            // "if an error is encountered, the $$$.SUB file is
                            // erased and control reverts to the keyboard"
                            // (CCP22.ASM).  An unknown command is that error.
                            if submitted {
                                Self::cpmemu_abort_submit(fs);
                            }
                        }
                    }
                }
            }
        }
    }

    /// Built-in `DIR`: list the files on the current drive, four per row
    /// (CP/M's `DIR` is a CCP built-in, not a `.COM`).  Prints `No file`
    /// when the drive is empty, as CP/M does.
    /// The FCB naming `$$$.SUB` on **drive A:** — the batch file the CCP
    /// consumes one command at a time.
    ///
    /// Drive A: explicitly (FCB drive byte 1), not the current drive, because
    /// that is what CP/M 2.2's CCP does: `CCP22.ASM` selects disk 0 before
    /// opening the file and re-selects the caller's drive afterwards. The
    /// comment beside that code says "on current disk" and is misleading — the
    /// `MVI A,00H` / `CNZ SELDSK` pair immediately above it switches to A:.
    ///
    /// This is also why the historical rule "SUBMIT only works from drive A:"
    /// exists: `SUBMIT.COM` writes `$$$.SUB` to the *current* drive (verified
    /// by running the real DRI binary in this emulator from B:, which produced
    /// `B:$$$.SUB`), while the CCP only ever reads A:. Submitting from B:
    /// therefore does nothing, on real CP/M and here alike.
    fn cpmemu_sub_fcb() -> Fcb {
        let mut raw = [0u8; FCB_SIZE];
        raw[0] = 1; // 1 = A: (0 would mean "current drive")
        raw[1..9].copy_from_slice(b"$$$     ");
        raw[9..12].copy_from_slice(b"SUB");
        Fcb::from_bytes(&raw)
    }

    /// Take the next command line from `A:$$$.SUB`, or `None` when no batch is
    /// running.
    ///
    /// The format was established by running DRI's real `SUBMIT.COM` inside
    /// this emulator and dumping the file it wrote: 128-byte records, byte 0 a
    /// character count, the text following it — and **the records are in
    /// reverse order**, which `CCP22.ASM` states outright ("Yes $$$.SUB files
    /// are backwards") and implements by reading record `RC-1`. So the *last*
    /// record is the *next* command.
    ///
    /// Everything after the counted text is uninitialised buffer content — the
    /// real dump showed leftovers like `$ *.COM\r\n` after the NUL — so the
    /// length byte is the only trustworthy delimiter and nothing here scans for
    /// a terminator.
    ///
    /// The record is consumed **before** it is returned (the file is truncated,
    /// and deleted once empty), matching the CCP's decrement-and-close. That
    /// ordering is what stops a command which crashes or aborts from being
    /// re-read forever.
    fn cpmemu_next_submit_line(fs: &mut CpmFs) -> Option<String> {
        let fcb = Self::cpmemu_sub_fcb();
        let records = fs.file_size_records(&fcb)?;
        if records == 0 {
            // An empty batch file is a finished one.
            fs.delete(&fcb);
            return None;
        }
        let last = records - 1;
        let rec = fs.read_record(&fcb, last).ok().flatten()?;
        // Consume first, then interpret.
        if last == 0 {
            fs.delete(&fcb);
        } else {
            fs.truncate_to_records(&fcb, last);
        }
        // A count of 0 is a blank line; anything past the record is junk, so a
        // count that cannot fit is treated as a corrupt record rather than
        // trusted (it would otherwise read uninitialised bytes as a command).
        let len = rec[0] as usize;
        if len > 127 {
            return None;
        }
        let text: String = rec[1..1 + len]
            .iter()
            .map(|&b| if (0x20..0x7F).contains(&b) { b as char } else { ' ' })
            .collect();
        Some(text.trim().to_string())
    }

    /// Abandon any running batch: erase `A:$$$.SUB`.  The CCP's `EXITSB` does
    /// exactly this, and calls it on a keyboard break, on a command error, and
    /// when the last line has run.
    fn cpmemu_abort_submit(fs: &mut CpmFs) {
        fs.delete(&Self::cpmemu_sub_fcb());
    }

    async fn cpmemu_dir(&mut self, fs: &CpmFs, line: &str) -> Result<(), std::io::Error> {
        // CP/M's DIR takes an optional filespec: `DIR`, `DIR afn`, `DIR d:`,
        // `DIR d:afn`.  This used to ignore the operand entirely, so
        // `DIR *.COM` listed every file on the drive and looked like it had
        // filtered — silently wrong, on the most-used command there is.
        let operand = line.split_whitespace().nth(1).unwrap_or("");
        let (drive, name, ext) = match parse_dir_operand(operand) {
            Some(triple) => triple,
            None => {
                // Malformed filespec.  Reported, not treated as "everything" —
                // that conflation was the original bug.
                self.send_line(&format!("  {}?", self.red(&operand.to_ascii_uppercase())))
                    .await?;
                return Ok(());
            }
        };
        let mut raw = [0u8; FCB_SIZE];
        raw[0] = drive;
        raw[1..9].copy_from_slice(&name);
        raw[9..12].copy_from_slice(&ext);
        let names = fs.list_matching(&Fcb::from_bytes(&raw));
        if names.is_empty() {
            self.send_line("  No file").await?;
            return Ok(());
        }
        // Three 8.3 columns fit a 40-col PETSCII screen (3×12 + 2 gaps +
        // 2 indent = 40); four fit an 80-col ANSI/ASCII terminal.
        let cols = if self.terminal_type == TerminalType::Petscii {
            3
        } else {
            4
        };
        for chunk in names.chunks(cols) {
            let row: Vec<String> = chunk.iter().map(|n| format!("{:<12}", n)).collect();
            self.send_line(&format!("  {}", row.join(" ").trim_end()))
                .await?;
        }
        Ok(())
    }

    /// Built-in `ERA`: erase file(s) on the current drive matching a
    /// (possibly wildcarded) operand.  An all-wildcard erase (`ERA *.*`)
    /// asks for confirmation first, as CP/M does.  Silent on success;
    /// prints `No file` when nothing matched.
    async fn cpmemu_era(&mut self, fs: &mut CpmFs, line: &str) -> Result<(), std::io::Error> {
        let arg = match line.split_whitespace().nth(1) {
            Some(a) => a,
            None => {
                self.send_line("  ERA what?").await?;
                return Ok(());
            }
        };
        let (name, ext) = match parse_afn(arg) {
            Some(pair) => pair,
            None => {
                self.send_line(&format!("  {}?", self.red(&arg.to_ascii_uppercase())))
                    .await?;
                return Ok(());
            }
        };
        // Confirm a wholesale erase (name and ext all '?').
        if name == [b'?'; 8] && ext == [b'?'; 3] {
            self.send(&format!("  {}", self.amber("ALL FILES (Y/N)? ")))
                .await?;
            self.flush().await?;
            let yes = match self.get_line_input().await? {
                Some(s) => s.trim().eq_ignore_ascii_case("y"),
                None => return Ok(()),
            };
            if !yes {
                return Ok(());
            }
        }
        let mut raw = [0u8; FCB_SIZE];
        raw[1..9].copy_from_slice(&name);
        raw[9..12].copy_from_slice(&ext);
        let fcb = Fcb::from_bytes(&raw);
        if fs.fcb_drive_is_ro(&fcb) {
            self.send_line(&format!(
                "  Bdos Err On {}: R/O",
                fs.current_drive_letter()
            ))
            .await?;
            return Ok(());
        }
        let deleted = fs.delete(&fcb);
        // `delete` skips read-only files, so anything still matching afterwards
        // was protected.  Reporting that matters: "No file" about a file the
        // user can see in `DIR` reads as a bug in the emulator.
        if fs.count_ro_matches(&fcb) > 0 {
            self.send_line("  File R/O").await?;
        } else if deleted == 0 {
            self.send_line("  No file").await?;
        }
        Ok(())
    }

    /// Build a default-drive FCB (drive byte 0 = current drive) from a
    /// concrete 8.3 name/ext, for the resident file commands.
    fn cpmemu_fcb(name: &[u8; 8], ext: &[u8; 3]) -> Fcb {
        let mut raw = [0u8; FCB_SIZE];
        raw[1..9].copy_from_slice(name);
        raw[9..12].copy_from_slice(ext);
        Fcb::from_bytes(&raw)
    }

    /// Built-in `REN` (CP/M resident): rename a file on the current drive.
    /// Accepts the authentic `REN new=old` and, for convenience, `REN new
    /// old`.  Silent on success (as CP/M is); reports if the source is
    /// missing or the destination already exists (no silent clobber).
    async fn cpmemu_ren(&mut self, fs: &mut CpmFs, line: &str) -> Result<(), std::io::Error> {
        // Everything after the verb, with the '=' form normalized to a space
        // so both `new=old` and `new old` split the same way.
        let operand = line
            .split_once(char::is_whitespace)
            .map(|x| x.1.trim())
            .unwrap_or("");
        if operand.is_empty() {
            self.send_line("  REN new=old").await?;
            return Ok(());
        }
        let operand = operand.replace('=', " ");
        let mut parts = operand.split_whitespace();
        let new_spec = parts.next().unwrap_or("");
        let old_spec = parts.next().unwrap_or("");
        let (Some((nn, ne)), Some((on, oe))) = (split_8_3(new_spec), split_8_3(old_spec)) else {
            self.send_line("  REN new=old").await?;
            return Ok(());
        };
        let old = Self::cpmemu_fcb(&on, &oe);
        if fs.rename(&old, &nn, &ne) {
            return Ok(()); // success is silent, as in CP/M
        }
        // Distinguish the refusal cases for a helpful message.
        if fs.fcb_drive_is_ro(&old) {
            self.send_line(&format!(
                "  Bdos Err On {}: R/O",
                fs.current_drive_letter()
            ))
            .await?;
        } else if fs.file_is_ro(&old) {
            self.send_line("  File R/O").await?;
        } else if fs.open_existing(&Self::cpmemu_fcb(&nn, &ne)) {
            self.send_line("  File exists").await?;
        } else {
            self.send_line("  No file").await?;
        }
        Ok(())
    }

    /// Built-in `TYPE` (CP/M resident): stream a text file on the current
    /// drive to the console, stopping at the CP/M end-of-file marker
    /// (`^Z`, 0x1A) as CP/M does.  A binary file is refused (our safety
    /// addition) so it can't spray terminal-hostile bytes at a vintage
    /// screen, and the streamed portion is capped so a huge file can't tie
    /// up the link indefinitely (there is no break-out during a built-in).
    async fn cpmemu_type(&mut self, fs: &mut CpmFs, line: &str) -> Result<(), std::io::Error> {
        let arg = match line.split_whitespace().nth(1) {
            Some(a) => a,
            None => {
                self.send_line("  TYPE what?").await?;
                return Ok(());
            }
        };
        let (name, ext) = match split_8_3(arg) {
            Some(pair) => pair,
            None => {
                self.send_line(&format!("  {}?", self.red(&arg.to_ascii_uppercase())))
                    .await?;
                return Ok(());
            }
        };
        let bytes = match fs.read_whole_file(&Self::cpmemu_fcb(&name, &ext)) {
            Ok(Some(b)) => b,
            Ok(None) => {
                self.send_line("  No file").await?;
                return Ok(());
            }
            Err(_) => {
                self.send_line(&format!("  {}", self.red("[read error]"))).await?;
                return Ok(());
            }
        };
        // Text ends at the first ^Z (CP/M EOF filler), if any.
        let text = match bytes.iter().position(|&b| b == 0x1A) {
            Some(i) => &bytes[..i],
            None => &bytes[..],
        };
        // Binary guard: any NUL, or a heavy run of control bytes (excluding
        // the usual TAB/LF/FF/CR), means "don't stream this".
        const TYPE_MAX: usize = 256 * 1024;
        let controls = text
            .iter()
            .filter(|&&b| b < 0x20 && !matches!(b, 0x09 | 0x0A | 0x0C | 0x0D))
            .count();
        if text.contains(&0) || (text.len() >= 16 && controls * 100 / text.len() > 30) {
            self.send_line("  Cannot TYPE a binary file.").await?;
            return Ok(());
        }
        let (shown, truncated) = if text.len() > TYPE_MAX {
            (&text[..TYPE_MAX], true)
        } else {
            (text, false)
        };
        // Break the text into terminal-width lines and page it with the shared
        // `--More-- (SPACE, RET, Q)` viewer — the same one the Gateway Shell's
        // TYPE uses.  Real CP/M's TYPE just streams (you freeze it with ^S/^Q),
        // but a long file would otherwise blow straight past the screen; the
        // paginated view stops each screenful and waits.  Tabs expand to four
        // spaces and long lines wrap, matching the Gateway Shell exactly.
        let width = if self.terminal_type == TerminalType::Petscii {
            PETSCII_WIDTH - 2
        } else {
            78
        };
        let text_str = String::from_utf8_lossy(shown);
        let mut lines: Vec<String> = Vec::new();
        for raw in text_str.split('\n') {
            let raw = raw.strip_suffix('\r').unwrap_or(raw);
            let expanded = raw.replace('\t', "    ");
            if expanded.is_empty() {
                lines.push(String::new());
                continue;
            }
            let chars: Vec<char> = expanded.chars().collect();
            for chunk in chars.chunks(width) {
                lines.push(chunk.iter().collect());
            }
        }
        if truncated {
            lines.push(format!("  {}", self.dim("[truncated]")));
        }
        self.cpm_page_lines(&lines).await
    }

    /// Built-in `SAVE` (CP/M resident): write `n` 256-byte pages of the TPA
    /// (from 0x0100) to a file on the current drive, exactly as CP/M's
    /// `SAVE n file`.  Because the machine persists across commands, this
    /// captures the memory image a prior program (e.g. `DDT`) left behind.
    async fn cpmemu_save(
        &mut self,
        cpm: &mut Cpm,
        fs: &mut CpmFs,
        line: &str,
    ) -> Result<(), std::io::Error> {
        let mut args = line.split_whitespace().skip(1);
        let pages = match args.next().and_then(|s| s.parse::<u16>().ok()) {
            Some(n) if n <= 255 => n,
            _ => {
                self.send_line("  SAVE n file  (n = 0..255 pages)").await?;
                return Ok(());
            }
        };
        let (name, ext) = match args.next().and_then(split_8_3) {
            Some(pair) => pair,
            None => {
                self.send_line("  SAVE n file  (n = 0..255 pages)").await?;
                return Ok(());
            }
        };
        let fcb = Self::cpmemu_fcb(&name, &ext);
        if !fs.make(&fcb) {
            self.send_line(&format!("  {}", self.red("[cannot create file]"))).await?;
            return Ok(());
        }
        // n pages = n*256 bytes = n*2 records of 128 bytes, read from the TPA.
        let data = cpm.read_block(0x0100, pages as usize * 256);
        for (i, chunk) in data.chunks(128).enumerate() {
            let mut rec = [0u8; 128];
            rec[..chunk.len()].copy_from_slice(chunk);
            if fs.write_record(&fcb, i as u32, &rec).is_err() {
                self.send_line(&format!("  {}", self.red("[write error]"))).await?;
                return Ok(());
            }
        }
        Ok(())
    }

    /// Built-in `USER` (CP/M resident): select a user area 0–15.  The number
    /// is tracked (and shared with BDOS 32, so a program's get/set agrees), but
    /// the emulator keeps a single flat file area — files are not segregated by
    /// user, a documented simplification — so a non-zero area is accepted with
    /// a one-line note rather than silently hiding files.  Recognized (not
    /// passed through to a `.COM`).
    async fn cpmemu_user(&mut self, line: &str, fs: &mut CpmFs) -> Result<(), std::io::Error> {
        match line.split_whitespace().nth(1).and_then(|s| s.parse::<u8>().ok()) {
            Some(n) if n <= 15 => {
                fs.set_user(n);
                if n != 0 {
                    self.send_line("  (files share one flat area)").await?;
                }
            }
            _ => {
                self.send_line("  USER 0..15").await?;
            }
        }
        Ok(())
    }

    /// The free-TPA report a real CP/M system prints in its boot banner
    /// ("62K CP/M VERS 2.2"): the size of the region a `.COM` is loaded into,
    /// in whole K, plus its hex bounds.  Nothing else lives in the TPA at the
    /// `A>` prompt — a program is loaded on demand and its space reclaimed on
    /// return — so the whole TPA is free whenever this is shown.  Derived from
    /// the [`TPA_BASE`]/[`TPA_TOP`] constants so it can never drift from the
    /// memory the emulator actually gives a program.
    pub(in crate::telnet) fn cpmemu_tpa_line() -> String {
        format!(
            "{}K TPA free ({:04X}-{:04X})",
            TPA_BYTES / 1024,
            TPA_BASE,
            TPA_TOP - 1
        )
    }

    /// One-screen help for the CCP-lite built-ins.
    /// The emulator's `HELP`, paginated like every other help screen.
    ///
    /// Paginated rather than simply printed, and the lines kept inside the
    /// PETSCII width, because this is the one help in the gateway that was
    /// neither: it grew past the 22-row screen and 40 columns the moment the
    /// file-loading section was added, so on a C64 the top of it scrolled away
    /// while the bottom wrapped mid-word.
    async fn cpmemu_help(&mut self) -> Result<(), std::io::Error> {
        let lines = Self::cpmemu_help_lines(self.terminal_type == TerminalType::Petscii);
        self.show_help_page("CP/M HELP", lines).await
    }

    /// The help text itself, extracted so a test can assert the REAL lines fit
    /// (the convention every other help screen here follows — a hand-copied
    /// duplicate in a test drifts from what is printed).
    pub(in crate::telnet) fn cpmemu_help_lines(petscii: bool) -> &'static [&'static str] {
        if petscii {
            &[
                "  Built-in commands:",
                "  DIR [d:][afn]  list files",
                "  ERA name   erase (wildcards)",
                "  REN new=old  rename",
                "  TYPE file  show a text file",
                "  SAVE n file  save n TPA pages",
                "  USER n     user area (0-15)",
                "  A: .. P:   change drive",
                "  VER        emulator version",
                "  name       run name.COM",
                "  HELP / ?   this help",
                "  EXIT       leave CP/M",
                "",
                "  Loading your own files:",
                "  The drives are folders under the",
                "  transfer directory: CPM/A..CPM/P.",
                "  In File Transfer, change to CPM/A",
                "  and upload - it is on drive A: at",
                "  once.  EGT8080 can also fetch",
                "  one over the virtual modem.",
                "",
                "  The drives are SHARED with other",
                "  sessions.  A file one session is",
                "  writing is refused to the rest",
                "  until it is closed.",
                "",
                "  SUBMIT runs a .SUB batch job.",
                "  Under an hbios port profile, the",
                "  HBIOS clock (RTCGETTIM) reports",
                "  the host's date and time.",
            ]
        } else {
            &[
                "  Built-in commands:",
                "  DIR [d:][afn]  list files (DIR *.COM)",
                "  ERA name   erase file(s) (wildcards)",
                "  REN new=old  rename a file",
                "  TYPE file  show a text file",
                "  SAVE n file  save n TPA pages",
                "  USER n     select user area (0-15)",
                "  A: .. P:   change drive",
                "  VER        emulator version",
                "  HELLO      BDOS print-string demo",
                "  ECHO       interactive console demo",
                "  name       run name.COM from the drive",
                "  HELP / ?   this help",
                "  EXIT/BYE/QUIT  leave CP/M",
                "",
                "  Loading your own files:",
                "  The drives are folders under the transfer",
                "  directory, CPM/A .. CPM/P.  In the gateway's",
                "  File Transfer menu, change directory to",
                "  CPM/A and upload there - the file is on",
                "  drive A: as soon as it lands.  EGT8080 can",
                "  also fetch one over the virtual modem.",
                "",
                "  The drives are SHARED with any other session",
                "  in the emulator.  A file one session is",
                "  writing is refused to the others until it",
                "  closes the file or leaves; reads are free.",
                "",
                "  SUBMIT runs a .SUB batch job, as CP/M 2.2's",
                "  own CCP does.  Under an hbios_* port profile",
                "  the HBIOS clock (RTCGETTIM) reports the",
                "  host's date and time - CP/M 2.2 has none of",
                "  its own.",
            ]
        }
    }

    /// Run one transient, and close its print job however it ends.
    ///
    /// A wrapper purely so the spool outlives the many `return`s inside — the
    /// program can end by finishing, by exhausting its budget, by `ESC ESC` or
    /// by the user hanging up, and a document that only appeared on the tidiest
    /// of those paths would be the worst kind of unreliable.
    ///
    /// **Returning to `A>` is CP/M's only exact end-of-job**, which is why this
    /// exists at all: the five-second idle close still runs inside the loop for
    /// a program that prints and then keeps working, but a program that prints
    /// and exits should not make its user wait out a timeout.
    ///
    /// The close cannot change what the program returned. Its file is already
    /// written by the time anything could fail, so a failed notice — the session
    /// hung up mid-print, most likely — costs the message and not the document.
    async fn cpmemu_run_program(
        &mut self,
        cpm: &mut Cpm,
        modem: &mut CpmModem,
        program: &[u8],
        tail: &str,
        fs: &mut CpmFs,
    ) -> Result<bool, std::io::Error> {
        // Sampled once, here, and threaded down: the alternative is a full
        // `Config` clone per printed character.  See `cpmemu_print`.
        let cfg = config::get_config();
        let print_format = crate::cpm::printer::format_for(&cfg.cpm_printer);
        // `LST:` is an operating-system service here, not a board, and what
        // reaches it is CP/M's own CR LF -- measured.  So the default is off,
        // which is also what makes WordStar's overstrike mean anything.
        let print_auto_lf = crate::cpm::printer::auto_lf_for(&cfg.cpm_printer_autolf, false);
        let transfer_dir = cfg.transfer_dir.clone();
        drop(cfg);
        let mut spool: Option<crate::cpm::printer::SpoolJob> = None;
        let result = self
            .cpmemu_run_program_inner(
                cpm,
                modem,
                program,
                tail,
                fs,
                &mut spool,
                print_format,
                print_auto_lf,
                &transfer_dir,
            )
            .await;
        if let Some(format) = print_format {
            let _ = self.cpmemu_spool_close(&mut spool, format, &transfer_dir).await;
        }
        result
    }

    /// Run a loaded program on the emulated CPU, servicing the console BDOS
    /// group against the live session, until it warm-boots, the user breaks
    /// out, or the instruction ceiling is hit.  Returns `Ok(false)` if the
    /// session disconnected (the caller should leave the emulator), else
    /// `Ok(true)` (return to the `A>` prompt).
    #[allow(clippy::too_many_arguments)]
    async fn cpmemu_run_program_inner(
        &mut self,
        cpm: &mut Cpm,
        modem: &mut CpmModem,
        program: &[u8],
        tail: &str,
        fs: &mut CpmFs,
        spool: &mut Option<crate::cpm::printer::SpoolJob>,
        print_format: Option<crate::cpm::printer::Format>,
        print_auto_lf: bool,
        transfer_dir: &str,
    ) -> Result<bool, std::io::Error> {
        cpm.load_com(program);
        // Lay down page zero (command tail + default FCBs) so a real `.COM`
        // finds its arguments where CP/M puts them.  Built-in demos pass an
        // empty tail.
        cpm.setup_command_line(tail);
        // Seed the page-zero CDISK byte (0x0004) with the drive/user the
        // program is launched under.  A transient that reads 0x0004 directly
        // to find its login drive (e.g. Infocom's interpreter locating its
        // story file) must see the real drive — otherwise a game run from B:
        // looks for its data on A: and hangs.  `load_com` reinstalls the low
        // vectors, so this has to come after it.
        cpm.set_current_disk(fs.current_drive(), fs.current_user());
        // Reset the DMA to the default buffer, exactly where CP/M's own CCP
        // does it: `CCP22.ASM`'s `TRANS7` calls `SETDMA` (function 26 with
        // 0080H) on the line before `CALL TPA`.
        //
        // Without this, the DMA a program left behind was inherited by the next
        // one. Found by running the real DRI transients in sequence: `PIP`
        // moves the DMA to its own buffer, so a following `DUMP TEST.TXT`
        // printed the stale contents of 0080H — the command tail — instead of
        // the file, silently and with no error. Every program that reads a
        // record without setting its own DMA was exposed, which is most of
        // them; `DUMP` on its own in a fresh session was always correct, which
        // is why this survived.
        fs.set_dma(crate::cpm::DEFAULT_DMA);
        // Runaway ceiling for this run, from config (millions of Z80
        // instructions) — the last-resort backstop.  Interactively, a
        // double-`ESC` breaks out: at a console prompt via `cpmemu_conin`, and
        // between batches via the out-of-band drain below (so even a program
        // that never reads the console is escapable at once).
        let max_instructions =
            config::get_config().cpm_emu_max_minstr as u64 * 1_000_000;
        let abort = AtomicBool::new(false);
        // A SINGLE double-`ESC` tracker shared by `cpmemu_conin` (which reads
        // the wire while the program blocks on a console read) and the
        // between-batch out-of-band drain (which reads while the program
        // computes).  Sharing it is essential: an `ESC ESC` split across the
        // two — e.g. the CSI-arrow peek pushes the 2nd ESC back and the drain
        // then reads it — must still pair and break out.
        let mut last_esc = false;
        // Bytes the out-of-band drain reads while the program is computing
        // (not blocked in a console read) are buffered here for the next
        // CONIN; the drain also breaks out on a double-`ESC` so a program that
        // never reads the console (a compute-bound runaway) is still escapable
        // at once, without waiting out the instruction ceiling.
        let mut pending_input: VecDeque<u8> = VecDeque::new();
        // ADM-3A output decoder: the guest is told it's driving an ADM-3A,
        // and its control codes are translated to the connected terminal.
        // State persists across BDOS calls (a cursor-address sequence can
        // straddle them).
        let mut term = Adm3a::default();
        // Set when an HBIOS blocking call (`IN` with nothing waiting, `OUT`
        // with a full ring) parked the guest on the trap this batch.  The wait
        // is then paced below, so a program sitting in a blocking read costs
        // the host a poll every few milliseconds instead of a spin.  The
        // instruction budget deliberately doesn't advance while parked — a
        // program waiting for its peer isn't a runaway — and the `ESC ESC`
        // break-out still works, because the drain runs at every seam.
        let mut hbios_waiting = false;
        // How long the guest has sat parked on a blocking HBIOS call with
        // nothing arriving.  The session's idle timeout applies here exactly as
        // it does to a console read: the parked path polls the modem instead of
        // blocking on the wire, so it would otherwise never reach the timeout
        // check in `read_byte_filtered` and an abandoned session could sit in a
        // 2 ms poll loop for ever.  Reset by any progress or any keystroke, so a
        // program legitimately waiting for an inbound call is only closed when
        // the *user* has gone away — which is what the operator's timeout means.
        //
        // Measured against the clock rather than by adding up the naps: the rest
        // of the loop body (servicing the modem, draining the wire) also takes
        // time, so summing 2 ms per pass would under-count the wait and let the
        // timeout fire long after the operator's configured limit.
        let mut hbios_parked_since: Option<tokio::time::Instant> = None;

        // Consecutive passes that were nothing but a status call answering
        // "nothing available" — a comms program's idle loop.  Paces the loop
        // without touching throughput; see [`idle_nap`] for why and how much.
        let mut idle_polls: u32 = 0;
        // Ring depth before this pass serviced the modem, so the pacing below can
        // tell "bytes arrived" from "bytes are sitting unread".
        let mut rx_before_service: usize = 0;

        loop {
            // Set by the status-poll arms below when they answer "nothing".
            // Defaults to "this pass did real work", so an unrecognised call is
            // never throttled by mistake — only calls proven idle are.
            let mut idle_poll = false;
            // A job that has gone quiet is finished.  Checked here as well as on
            // the way out because a program can print a report and then carry on
            // running for an hour — a spreadsheet, a BBS — and its operator
            // should not have to quit the program to be given the document.
            if let Some(format) = print_format
                && spool.as_ref().is_some_and(|j| j.idle_expired())
            {
                self.cpmemu_spool_close(spool, format, transfer_dir).await?;
            }
            // Runaway guard, checked every batch regardless of why run()
            // returned.  A BDOS-frequent loop (e.g. polling console status,
            // `LD C,11 / CALL 5 / JR Z`) returns Stop::Bdos each batch and
            // never reaches Stop::BudgetExhausted, so the ceiling must be
            // enforced here, not only in that arm.
            if cpm.instructions() >= max_instructions {
                self.send_line("").await?;
                self.send_line(&format!(
                    "  {}",
                    self.red("[aborted: instruction budget]")
                ))
                .await?;
                return Ok(true);
            }
            match cpm.run(CPM_RUN_BATCH, &abort) {
                Stop::Bdos(func) => {
                    match func {
                        1 => {
                            // Console input WITH echo.
                            match self.cpmemu_conin(&mut pending_input, &mut last_esc).await? {
                                ConIn::Byte(b) => {
                                    self.cpmemu_emit(&mut term, &[b]).await?;
                                    cpm.bdos_return(b);
                                }
                                ConIn::BreakOut => {
                                    self.cpmemu_break_notice().await?;
                                    return Ok(true);
                                }
                                ConIn::Disconnect => return Ok(false),
                            }
                        }
                        2 => {
                            // Console output: char in E.
                            self.cpmemu_emit(&mut term, &[cpm.arg_e()]).await?;
                            cpm.bdos_return(0);
                        }
                        5 => {
                            // List (printer / LST:) output, char in E — the
                            // path WordStar, MBASIC's `LPRINT` and
                            // `PIP LST:=X.TXT` take.
                            //
                            // This used to route to the console unconditionally,
                            // on the grounds that there was no physical printer:
                            // better a visible byte than a dropped one. There is
                            // a printer now, and `cpmemu_print` still routes to
                            // the console when `cpm_printer` is off — so that
                            // reasoning is preserved as the default rather than
                            // overruled.
                            let e = cpm.arg_e();
                            self.cpmemu_print(spool, &mut term, e, print_format, print_auto_lf, transfer_dir)
                                .await?;
                            cpm.bdos_return(0);
                        }
                        6 => {
                            // Direct console I/O: E=0xFF non-blocking read (no
                            // echo), E=0xFE status, else output E.
                            let e = cpm.arg_e();
                            match e {
                                // Direct console input.  A single `E=0xFF` call
                                // reads a key, blocking until one arrives — the
                                // common CP/M idiom for a keypress / Y-N prompt
                                // (a program that wants to poll uses the E=0xFE
                                // status call or BDOS 11 CONST, both non-blocking
                                // below).  Break-out + disconnect handled as for
                                // BDOS 1.
                                0xFF => match self.cpmemu_conin(&mut pending_input, &mut last_esc).await? {
                                    ConIn::Byte(b) => cpm.bdos_return(b),
                                    ConIn::BreakOut => {
                                        self.cpmemu_break_notice().await?;
                                        return Ok(true);
                                    }
                                    ConIn::Disconnect => return Ok(false),
                                },
                                // Status: 0xFF if a key is buffered, else 0.
                                0xFE => {
                                    idle_poll = pending_input.is_empty();
                                    cpm.bdos_return(if pending_input.is_empty() { 0x00 } else { 0xFF });
                                }
                                _ => {
                                    self.cpmemu_emit(&mut term, &[e]).await?;
                                    cpm.bdos_return(0);
                                }
                            }
                        }
                        9 => {
                            // Print $-terminated string at DE.
                            let de = cpm.arg_de();
                            let s = cpm.read_dollar_string(de, 8192);
                            self.cpmemu_emit(&mut term, &s).await?;
                            cpm.bdos_return(0);
                        }
                        10 => {
                            // Read console buffer (line) into memory at DE,
                            // via the break-out-aware console reader.
                            let de = cpm.arg_de();
                            let max = cpm.read_buffer_max(de);
                            match self
                                .cpmemu_read_line(&mut term, &mut pending_input, &mut last_esc, max)
                                .await?
                            {
                                LineRead::Line(bytes) => {
                                    cpm.bdos_read_buffer(de, &bytes);
                                    cpm.bdos_return(0);
                                }
                                LineRead::BreakOut => {
                                    self.cpmemu_break_notice().await?;
                                    return Ok(true);
                                }
                                LineRead::Disconnect => return Ok(false),
                            }
                        }
                        3 => {
                            // AUX (reader) input: hand the guest the next byte
                            // from the virtual modem, or ^Z (0x1A) if none.
                            // (CP/M 2.2 BDOS 3 has no status call; software
                            // that needs one uses the BIOS — best-effort here.)
                            let b = cpm.modem_rx_pop().unwrap_or_else(|| {
                                // CP/M 2.2 has no AUX status call, so ^Z is how
                                // this device says "nothing" — and an AUX-profile
                                // guest polls exactly here.
                                idle_poll = true;
                                0x1A
                            });
                            cpm.bdos_return(b);
                        }
                        4 => {
                            // AUX (punch) output: send E to the virtual modem.
                            let e = cpm.arg_e();
                            cpm.modem_tx_push(e);
                            cpm.bdos_return(0);
                        }
                        11 => {
                            // Console status: 0xFF if a key is ready (buffered
                            // by the out-of-band drain), else 0 — so the classic
                            // `LD C,11 / CALL 5 / OR A / JR Z` poll idiom sees a
                            // keypress instead of spinning to the budget ceiling.
                            idle_poll = pending_input.is_empty();
                            cpm.bdos_return(if pending_input.is_empty() { 0x00 } else { 0xFF });
                        }
                        12 => cpm.bdos_return(0x22), // version: CP/M 2.2
                        _ => {
                            // Disk-system / FCB file BDOS calls (drive
                            // select, DMA, open/read/write/search/delete/
                            // rename/size) need no session I/O, so the core
                            // services them directly.  The disk-info "Get
                            // Addr(...)" calls (DPB / alloc vector, used by
                            // STAT for free space) return an address in HL;
                            // everything else returns a byte code (unknown
                            // funcs → 0).
                            if let Some(hl) = crate::cpm::disk_info_bdos(cpm, fs, func) {
                                cpm.bdos_return_hl(hl);
                            } else {
                                let code = crate::cpm::service_disk_bdos(cpm, fs, func)
                                    .unwrap_or(0);
                                cpm.bdos_return(code);
                            }
                        }
                    }
                }
                Stop::Bios(vector) => {
                    // A direct BIOS jump-table call (software that bypasses
                    // BDOS for console I/O: MBASIC, WordStar, Infocom, …).
                    // We service the console group against the live session
                    // exactly like their BDOS equivalents; the low-level
                    // disk vectors (SELDSK/READ/WRITE/…) are stubbed to 0
                    // since file I/O flows through the BDOS FCB path, not
                    // raw sectors.
                    match vector {
                        // WBOOT: the guest asked to reboot to the CCP.  Emit
                        // the same fresh line as the `Stop::WarmBoot` path
                        // (below) so a program that exits via the BIOS WBOOT
                        // vector without a trailing newline doesn't jam the
                        // next `A>` prompt onto its last output line.
                        1 => {
                            self.send_line("").await?;
                            return Ok(true);
                        }
                        // CONST: 0xFF if a key is buffered (out-of-band
                        // drain), else 0 — the non-blocking status poll.
                        2 => {
                            idle_poll = pending_input.is_empty();
                            cpm.bios_return(if pending_input.is_empty() { 0x00 } else { 0xFF })
                        }
                        // CONIN: blocking keyboard read (no echo).
                        3 => match self.cpmemu_conin(&mut pending_input, &mut last_esc).await? {
                            ConIn::Byte(b) => cpm.bios_return(b),
                            ConIn::BreakOut => {
                                self.cpmemu_break_notice().await?;
                                return Ok(true);
                            }
                            ConIn::Disconnect => return Ok(false),
                        },
                        // CONOUT: character in C to the console.
                        4 => {
                            let c = cpm.arg_c();
                            self.cpmemu_emit(&mut term, &[c]).await?;
                            cpm.bios_return(0);
                        }
                        // LIST: character in C to the printer.  Shared this arm
                        // with CONOUT until the printer existed, which is why
                        // printer output has always appeared on the terminal —
                        // and still does when `cpm_printer` is off, so nothing
                        // an existing operator relies on changes.
                        5 => {
                            let c = cpm.arg_c();
                            self.cpmemu_print(spool, &mut term, c, print_format, print_auto_lf, transfer_dir)
                                .await?;
                            cpm.bios_return(0);
                        }
                        // PUNCH (AUX out): character in C to the virtual modem.
                        6 => {
                            let c = cpm.arg_c();
                            cpm.modem_tx_push(c);
                            cpm.bios_return(0);
                        }
                        // READER (AUX in): next modem byte, or ^Z if none.
                        7 => {
                            let b = cpm.modem_rx_pop().unwrap_or_else(|| {
                                idle_poll = true; // as BDOS 3: ^Z means "nothing"
                                0x1A
                            });
                            cpm.bios_return(b);
                        }
                        // LISTST: list device always ready.
                        15 => cpm.bios_return(0xFF),
                        // BOOT/HOME/SELDSK/SETTRK/SETSEC/SETDMA/READ/WRITE/
                        // SECTRAN: no raw-sector disk emulation — stub to 0.
                        _ => cpm.bios_return(0),
                    }
                }
                Stop::Hbios(func) => {
                    // A RomWBW HBIOS call (`RST 8`).  Serviced synchronously
                    // against the virtual modem; a blocking call whose device
                    // isn't ready is left parked so the seam below (modem
                    // service + break-out drain) runs and the call is
                    // re-reported next batch.
                    if crate::cpm::hbios::service(cpm, func)
                        == crate::cpm::hbios::HbiosOutcome::Waiting
                    {
                        hbios_waiting = true;
                    } else if crate::cpm::hbios::is_idle_status_poll(cpm, func) {
                        // A RomWBW program's wait loop polls input status rather
                        // than blocking, so it never parks and the `hbios_waiting`
                        // pacing above never sees it.  Count it as idle instead —
                        // otherwise this loop spins the host at over a core, and
                        // it alternates with the console-status poll below, so
                        // marking only one of the two would never accumulate.
                        idle_poll = true;
                    }
                }
                Stop::WarmBoot => {
                    // Real CP/M's CCP emits CR/LF before redrawing the
                    // prompt.  Many transients (STAT, and utilities in
                    // general) exit without a trailing newline, relying on
                    // that; without it the `A>` prompt lands jammed onto the
                    // program's last output line.  One `send_line("")`
                    // guarantees the prompt starts on a fresh line.
                    self.send_line("").await?;
                    return Ok(true);
                }
                Stop::Aborted => return Ok(true),
                Stop::BudgetExhausted => {}
            }
            // Service the virtual modem at the batch seam: hand it whatever the
            // guest wrote toward the peer, and queue back any result codes /
            // received bytes for the guest to read.  This is where the
            // synchronous UART/AUX rings cross into async I/O (dial + pump).
            if modem.enabled() {
                // Pick up an inbound `CPM@<ip>` call when idle, so the guest
                // sees RING and can answer (ATA / auto-answer).  Any idle pool
                // member may claim the shared slot; the take is atomic, so only
                // one session gets each call — no double-answer.
                if modem.can_answer() {
                    if let Some(call) = crate::serial::take_cpm_call_request() {
                        modem.accept_incoming(call);
                    }
                }
                let tx = cpm.modem_drain_tx();
                // Unread bytes already in the ring mean the guest is mid-burst,
                // which makes the peer poll non-blocking (see poll_connection).
                let guest_has_rx = cpm.modem_rx_len() > 0;
                rx_before_service = cpm.modem_rx_len();
                let rx = modem.service(tx, cpm.modem_rx_free(), guest_has_rx).await;
                if !rx.is_empty() {
                    cpm.modem_queue_rx(&rx);
                }
                // Reflect carrier (DCD) into the UART status the guest polls.
                cpm.set_carrier(modem.carrier_asserted());
            }
            // Out-of-band break-out reader: drain any wire bytes waiting right
            // now (non-blocking) so a double-`ESC` aborts even a program that
            // never reads the console; other bytes are buffered for CONIN.
            let pending_before = pending_input.len();
            match self.cpmemu_oob_drain(&mut pending_input, &mut last_esc).await {
                Ok(OobDrain::Continue) => {}
                Ok(OobDrain::BreakOut) => {
                    self.cpmemu_break_notice().await?;
                    return Ok(true);
                }
                Ok(OobDrain::Disconnect) => return Ok(false),
                Err(e) => return Err(e),
            }
            // Cooperative yield every batch so a BDOS-frequent loop whose
            // handlers never .await (console status/version/set-DMA/etc.)
            // can't pin the tokio worker.  Interactive handlers already
            // await; this makes the non-awaiting ones cooperative too.
            tokio::task::yield_now().await;
            // Pace an established idle poll loop (see `idle_polls`).  A byte
            // arriving is real work, so it clears the count as well as the
            // status arms do — the very next poll then runs at full speed and
            // sees the keystroke.
            //
            // "Arriving" is deliberately a *change* in the ring depth, not a
            // non-empty ring.  A guest can sit polling for a keypress while
            // peer bytes go unread — a "press any key" prompt during an inbound
            // burst — and treating a merely non-empty ring as progress would
            // reset this counter on every pass and spin the host exactly as
            // before.  Bytes being *consumed* needs no special case: the trap
            // that reads them is not a status poll, so it clears the count by
            // leaving `idle_poll` false.
            let rx_arrived = cpm.modem_rx_len() > rx_before_service;
            if pending_input.len() > pending_before || rx_arrived {
                idle_polls = 0;
            } else if idle_poll {
                idle_polls = idle_polls.saturating_add(1);
                if let Some(nap) = idle_nap(idle_polls) {
                    tokio::time::sleep(nap).await;
                }
            } else {
                idle_polls = 0;
            }
            // And pace a guest reading a port nothing answers.  A separate
            // count from `idle_polls` because it is a different kind of idle
            // and the existing one cannot see it: this guest ends every pass
            // with a console *write*, which is real work by any measure — what
            // is unreal is where the byte it wrote came from.  A read that IS
            // answered clears the count inside the machine, so software
            // talking to a port that exists is never paced.
            if let Some(nap) = unclaimed_nap(cpm.unclaimed_reads()) {
                tokio::time::sleep(nap).await;
                // Cleared after the pause, not left to a claimed read: a burst
                // of probing then pays once, where a loop pays every time
                // round.  See `clear_unclaimed_reads`.
                cpm.clear_unclaimed_reads();
            }
            if pending_input.len() > pending_before {
                hbios_parked_since = None; // the user is here: not idle
            }
            // Pace a parked HBIOS blocking call.  Without this the guest's PC
            // sits on the trap and the loop would spin as fast as the executor
            // allows; a few milliseconds between polls is imperceptible at the
            // byte rates a CP/M comms program works at.
            if hbios_waiting {
                hbios_waiting = false;
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
                let since = *hbios_parked_since.get_or_insert_with(tokio::time::Instant::now);
                if !self.idle_timeout.is_zero() && since.elapsed() >= self.idle_timeout {
                    glog!("CP/M: session idle timeout while a program waited on the modem");
                    return Ok(false);
                }
            } else {
                hbios_parked_since = None; // the guest progressed
            }
        }
    }

    /// Try to load and run `<verb>.COM` from a drive as a real transient
    /// program.  The verb may carry a drive prefix (`B:PIP`); its extension
    /// is always forced to `COM` (the CCP ignores any typed extension for the
    /// command name).  The command tail (everything after the verb) is laid
    /// into page zero for the program.
    ///
    /// Returns `Ok(None)` when no such `.COM` exists (so the caller can print
    /// CP/M's `VERB?`), `Ok(Some(true))` when the program ran and control
    /// should return to the `A>` prompt, and `Ok(Some(false))` when the
    /// session disconnected mid-run (leave the emulator).
    async fn cpmemu_try_run_com(
        &mut self,
        cpm: &mut Cpm,
        modem: &mut CpmModem,
        fs: &mut CpmFs,
        verb: &str,
        line: &str,
    ) -> Result<Option<bool>, std::io::Error> {
        // Parse the verb's drive prefix + name; force the extension to COM.
        let (drive, name, _ext) = parse_command_fcb(verb);
        let fcb = Fcb {
            drive,
            name,
            ext: *b"COM",
            ex: 0,
            s2: 0,
            cr: 0,
            rc: 0,
            r: [0; 3],
        };
        let bytes = match fs.read_whole_file(&fcb) {
            Ok(Some(b)) => b,
            Ok(None) => return Ok(None), // no such .COM
            Err(_) => {
                self.send_line(&format!("  {}", self.red("[load error]")))
                    .await?;
                return Ok(Some(true));
            }
        };
        // The command tail is everything after the first token (the verb).
        let tail = line
            .split_once(char::is_whitespace)
            .map(|x| x.1)
            .unwrap_or("");
        let cont = self.cpmemu_run_program(cpm, modem, &bytes, tail, fs).await?;
        Ok(Some(cont))
    }

    /// Read a console line for BDOS 10 (read-console-buffer) using
    /// `cpmemu_conin`, so it shares the program-console break-out semantics:
    /// CR terminates (echoing CR/LF), backspace / DEL edits, a double-`ESC`
    /// aborts to `A>` (NOT a session drop — the bug when this used the shell's
    /// line editor, where a lone `ESC` looked like a disconnect), and input is
    /// capped at the caller's `max`.
    async fn cpmemu_read_line(
        &mut self,
        term: &mut Adm3a,
        pending: &mut VecDeque<u8>,
        last_esc: &mut bool,
        max: usize,
    ) -> Result<LineRead, std::io::Error> {
        let mut buf: Vec<u8> = Vec::new();
        loop {
            match self.cpmemu_conin(pending, last_esc).await? {
                ConIn::BreakOut => return Ok(LineRead::BreakOut),
                ConIn::Disconnect => return Ok(LineRead::Disconnect),
                ConIn::Byte(b) => match b {
                    b'\r' => {
                        self.cpmemu_emit(term, b"\r\n").await?;
                        return Ok(LineRead::Line(buf));
                    }
                    b'\n' => {} // swallow a LF that trails a CR
                    0x08 | 0x7F => {
                        if buf.pop().is_some() {
                            self.cpmemu_emit(term, b"\x08 \x08").await?; // erase
                        }
                    }
                    _ => {
                        if buf.len() < max {
                            buf.push(b);
                            self.cpmemu_emit(term, &[b]).await?; // echo
                        }
                    }
                },
            }
        }
    }

    /// Pop and translate the next byte the out-of-band drain already buffered,
    /// returning the ADM-3A / ASCII key code, or `None` if nothing is buffered.
    /// Non-blocking — shared by `cpmemu_conin`'s fast path and by non-blocking
    /// direct console input (BDOS 6, E=0xFF).  Does not touch `last_esc` (the
    /// drain already escape-tracked these bytes and never buffers the 2nd `ESC`
    /// of a break-out pair).
    fn cpmemu_pending_key(pending: &mut VecDeque<u8>, is_petscii: bool) -> Option<u8> {
        let b = pending.pop_front()?;
        if is_petscii {
            if let Some(code) = cpm_term::petscii_key_to_adm3a(b) {
                return Some(code);
            }
            return Some(petscii_to_ascii_byte(b));
        }
        // ANSI: reassemble a buffered CSI arrow (`ESC [ A..D`, entirely in the
        // buffer, so no wire lookahead is needed) to its ADM-3A code.
        if b == 0x1B {
            if let Some(code) = pending_csi_arrow(pending) {
                return Some(code);
            }
        }
        Some(b)
    }

    /// Read one console byte for a running program, translating the client's
    /// keys into the ADM-3A codes the guest expects and detecting the
    /// double-`ESC` break-out.
    ///
    /// - A C64 cursor key (a single PETSCII byte) maps straight to its ADM-3A
    ///   code; other PETSCII bytes are folded to ASCII.
    /// - On an ANSI terminal an arrow key arrives as a fast `ESC [ A..D`
    ///   sequence; a short peek after `ESC` recognises it and returns the
    ///   ADM-3A code.  A lone `ESC` (an editor command) has no fast follower,
    ///   so the peek times out and the `ESC` is delivered to the guest; a
    ///   second `ESC` on the next read is the break-out (unchanged behavior).
    async fn cpmemu_conin(
        &mut self,
        pending: &mut VecDeque<u8>,
        last_esc: &mut bool,
    ) -> Result<ConIn, std::io::Error> {
        let is_petscii = self.terminal_type == TerminalType::Petscii;
        // Bytes the out-of-band drain already read (while the program was
        // computing) are delivered first.  The drain already escape-tracked
        // them via the shared `last_esc` and never buffers the 2nd ESC of a
        // break-out pair, so don't retrack here (leave `last_esc` to the drain
        // / wire path) — just translate.
        if let Some(code) = Self::cpmemu_pending_key(pending, is_petscii) {
            if keytrace_on() {
                glog!(
                    "cpmkey CONIN from-buffer {} (0x{:02X}) last_esc={} pending_left={}",
                    keyname(code),
                    code,
                    *last_esc,
                    pending.len()
                );
            }
            return Ok(ConIn::Byte(code));
        }
        loop {
            let b = match self.read_byte_filtered().await {
                Ok(Some(b)) => b,
                Ok(None) => return Ok(ConIn::Disconnect),
                // An idle timeout ends the program (and the session).
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                    return Ok(ConIn::Disconnect)
                }
                Err(e) => return Err(e),
            };

            if keytrace_on() {
                glog!(
                    "cpmkey CONIN wire {} (0x{:02X}) last_esc={} petscii={}",
                    keyname(b),
                    b,
                    *last_esc,
                    is_petscii
                );
            }

            // A pending first ESC + another ESC = break-out (slow, human).
            if is_esc_key(b, is_petscii) {
                if *last_esc {
                    *last_esc = false;
                    if keytrace_on() {
                        glog!("cpmkey CONIN second ESC -> BREAKOUT");
                    }
                    return Ok(ConIn::BreakOut);
                }
                // Peek for a fast CSI arrow (ANSI terminals only).
                if !is_petscii {
                    let peek = self.cpmemu_peek_arrow().await?;
                    if keytrace_on() {
                        glog!(
                            "cpmkey CONIN ESC peek -> {}",
                            match peek {
                                ArrowPeek::Arrow(c) => format!("Arrow(0x{c:02X})"),
                                ArrowPeek::UnknownCsi => "UnknownCsi (swallowed)".to_string(),
                                ArrowPeek::NotCsi => "NotCsi (deliver ESC)".to_string(),
                            }
                        );
                    }
                    match peek {
                        ArrowPeek::Arrow(code) => return Ok(ConIn::Byte(code)),
                        // A non-arrow CSI was consumed whole; read the next key.
                        ArrowPeek::UnknownCsi => continue,
                        ArrowPeek::NotCsi => {} // fall through: deliver the ESC
                    }
                }
                // Lone ESC: deliver it; a following ESC becomes the break-out.
                *last_esc = true;
                if keytrace_on() {
                    glog!("cpmkey CONIN first ESC delivered to guest, last_esc:=true");
                }
                return Ok(ConIn::Byte(0x1B));
            }
            if keytrace_on() && *last_esc {
                glog!("cpmkey CONIN non-ESC after ESC -> last_esc cleared (pair broken)");
            }
            *last_esc = false;

            if is_petscii {
                // A C64 cursor key becomes its ADM-3A code; else fold to ASCII.
                if let Some(code) = cpm_term::petscii_key_to_adm3a(b) {
                    return Ok(ConIn::Byte(code));
                }
                return Ok(ConIn::Byte(petscii_to_ascii_byte(b)));
            }
            return Ok(ConIn::Byte(b));
        }
    }

    /// After an `ESC`, briefly peek for a CSI arrow sequence (`[ A..D`).
    /// Consumes a *complete* CSI so a longer sequence (a function key like
    /// `ESC [ 1 5 ~`, or a modified arrow `ESC [ 1 ; 5 A`) is swallowed whole
    /// rather than leaking its tail to the guest as stray keystrokes.
    async fn cpmemu_peek_arrow(&mut self) -> Result<ArrowPeek, std::io::Error> {
        // Byte 1: the '[' introducer, if it arrives promptly.
        match self.cpmemu_peek_byte().await? {
            Some(b'[') => {}
            Some(other) => {
                self.pushback = Some(other); // not a CSI; give the byte back
                return Ok(ArrowPeek::NotCsi);
            }
            None => return Ok(ArrowPeek::NotCsi), // lone ESC
        }
        // CSI body: parameter / intermediate bytes (0x20..=0x3F) then a final
        // byte (0x40..=0x7E).  A bare final letter with no parameters may be a
        // plain arrow; anything with parameters is swallowed as UnknownCsi.
        // Bounded so a malformed stream can't loop.
        let mut had_params = false;
        for _ in 0..16 {
            match self.cpmemu_peek_byte().await? {
                Some(b) if (0x20..=0x3F).contains(&b) => had_params = true,
                Some(b) if (0x40..=0x7E).contains(&b) => {
                    if !had_params {
                        if let Some(code) = cpm_term::csi_arrow_to_adm3a(b) {
                            return Ok(ArrowPeek::Arrow(code));
                        }
                    }
                    return Ok(ArrowPeek::UnknownCsi);
                }
                _ => return Ok(ArrowPeek::UnknownCsi), // truncated / malformed
            }
        }
        Ok(ArrowPeek::UnknownCsi)
    }

    /// Out-of-band input drain, run between CPU batches.  Reads every wire
    /// byte that is *immediately* available (a single poll, so it never blocks
    /// the CPU), detecting a double-`ESC` break-out even while the guest
    /// is computing and buffering the rest for the next `CONIN`.  This is what
    /// makes a compute-bound program (one that never reads the console)
    /// escapable at once instead of only at the instruction ceiling.  It runs
    /// only *between* batches, and `cpmemu_conin` runs only *during* a console
    /// read, so the two escape trackers never overlap.
    ///
    /// The readiness probe is a *single poll*, not the
    /// `tokio::time::timeout(Duration::ZERO, …)` this used to be.  A zero
    /// duration reads like "don't wait", but tokio rounds every deadline up to
    /// the next timer tick, so each call actually cost ~1.1 ms.  This drain runs
    /// once per CPU batch, and a batch ends at every BDOS/BIOS trap — so a guest
    /// paid that 1.1 ms *per console character*, capping output at ~840 char/s
    /// however fast the CPU core ran (it manages 6.4 M CONOUT traps/s).  A
    /// screen-painting program like EGT8080 issues many writes per update, so it
    /// crawled at what looked like 150 baud.  One poll answers the same
    /// question — is a byte ready right now? — in nanoseconds.
    ///
    /// Cancel-safety note: an unready read is dropped exactly as the zero
    /// timeout dropped it, so the semantics are unchanged.  It is resumable
    /// across the `IAC`→command split (via `session_read_byte`'s
    /// `mid_iac_cmd`), which is the common case.  It is *not* resumable deeper
    /// inside a telnet negotiation (a subnegotiation payload, or between a
    /// `WILL/WONT/DO/DONT` command and its option byte).  A negotiation split
    /// across TCP segments that lands exactly on a between-batch drain could
    /// therefore desync the telnet parser — rare (LAN, mid-run, segment-split)
    /// and non-fatal (no panic/security impact); the resume point is
    /// intentionally shallow so this hot path stays simple.
    async fn cpmemu_oob_drain(
        &mut self,
        pending: &mut VecDeque<u8>,
        last_esc: &mut bool,
    ) -> Result<OobDrain, std::io::Error> {
        let is_petscii = self.terminal_type == TerminalType::Petscii;
        loop {
            // `None` ⇒ still pending ⇒ nothing waiting on the wire right now.
            let Some(read) = poll_once(self.session_read_byte()) else {
                return Ok(OobDrain::Continue);
            };
            match read {
                Ok(Some(b)) => {
                    // Escape tracking uses the SAME `last_esc` as cpmemu_conin,
                    // so a double-`ESC` split across the two still pairs.
                    if keytrace_on() {
                        glog!(
                            "cpmkey DRAIN wire {} (0x{:02X}) last_esc={} pending={}",
                            keyname(b),
                            b,
                            *last_esc,
                            pending.len()
                        );
                    }
                    if is_esc_key(b, is_petscii) {
                        if *last_esc {
                            *last_esc = false;
                            if keytrace_on() {
                                glog!("cpmkey DRAIN second ESC -> BREAKOUT");
                            }
                            return Ok(OobDrain::BreakOut);
                        }
                        *last_esc = true;
                    } else {
                        if keytrace_on() && *last_esc {
                            glog!("cpmkey DRAIN non-ESC after ESC -> last_esc cleared (pair broken)");
                        }
                        *last_esc = false;
                    }
                    // Buffer for CONIN, bounded so a flood can't grow it.
                    if pending.len() < 4096 {
                        pending.push_back(b);
                    }
                }
                Ok(None) => return Ok(OobDrain::Disconnect),
                Err(e) => return Err(e),
            }
        }
    }

    /// Read one byte with a short timeout, for CSI-arrow lookahead — fast
    /// terminal-generated sequences arrive back-to-back, while a human's lone
    /// `ESC` has no follower and times out.
    async fn cpmemu_peek_byte(&mut self) -> Result<Option<u8>, std::io::Error> {
        match tokio::time::timeout(
            std::time::Duration::from_millis(50),
            self.session_read_byte(),
        )
        .await
        {
            Ok(Ok(b)) => Ok(b),
            Ok(Err(e)) => Err(e),
            Err(_) => Ok(None), // timed out — no fast follower
        }
    }

    /// Write guest output to the session, translating the ADM-3A control
    /// stream to the connected terminal (ANSI CSI, PETSCII cursor codes, or
    /// best-effort ASCII) through the persistent [`Adm3a`] decoder.
    async fn cpmemu_emit(&mut self, term: &mut Adm3a, bytes: &[u8]) -> Result<(), std::io::Error> {
        let mut out = Vec::with_capacity(bytes.len());
        for &b in bytes {
            for op in term.feed(b) {
                cpm_term::render_op(op, self.terminal_type, &mut out);
            }
        }
        if !out.is_empty() {
            self.send_raw(&out).await?;
        }
        self.flush().await
    }

    /// Notice shown after a double-`ESC` break-out returns to the prompt.
    async fn cpmemu_break_notice(&mut self) -> Result<(), std::io::Error> {
        self.send_line("").await?;
        self.send_line(&format!("  {}", self.dim("[broke out to A>]")))
            .await
    }

    /// Built-in demo: print a banner via BDOS 9, then warm-boot.
    fn cpmemu_demo_hello() -> Vec<u8> {
        // 0100: 11 09 01   LD DE,0x0109
        // 0103: 0E 09      LD C,9
        // 0105: CD 05 00   CALL 5
        // 0108: C9         RET       ; -> warm boot
        // 0109: msg "$"
        let msg = b"iz80 Z80 CPU online.\r\nCP/M 2.2 BDOS console OK.\r\n$";
        let mut prog: Vec<u8> = vec![
            0x11, 0x09, 0x01, // LD DE,0x0109
            0x0E, 0x09, // LD C,9
            0xCD, 0x05, 0x00, // CALL 5
            0xC9, // RET
        ];
        prog.extend_from_slice(msg);
        prog
    }

    /// Built-in demo: read a key via BDOS 1 (which echoes), loop until '.'.
    fn cpmemu_demo_echo() -> Vec<u8> {
        // 0100: 0E 01      LD C,1
        // 0102: CD 05 00   CALL 5      ; A = char (echoed by BDOS 1)
        // 0105: FE 2E      CP '.'
        // 0107: CA 0D 01   JP Z,done(0x010D)
        // 010A: C3 00 01   JP loop(0x0100)
        // 010D: 0E 00      LD C,0
        // 010F: CD 05 00   CALL 5      ; warm boot
        vec![
            0x0E, 0x01, // LD C,1
            0xCD, 0x05, 0x00, // CALL 5
            0xFE, 0x2E, // CP '.'
            0xCA, 0x0D, 0x01, // JP Z,0x010D
            0xC3, 0x00, 0x01, // JP 0x0100
            0x0E, 0x00, // LD C,0
            0xCD, 0x05, 0x00, // CALL 5
        ]
    }
}

/// Tests for the driver's *own* logic — the CCP-lite built-ins and the CSI
/// arrow reassembly — as opposed to the bundled-artifact checks in
/// `egt80_tests` below.
///
/// The built-ins are the cheap half of this module to cover: they are plain
/// `async fn`s over a `CpmFs` and the session writer, so a scratch drive
/// directory plus a duplex pipe exercises them with no Z80 in the loop.  What
/// is still uncovered here is `cpmemu_run_program` (the BDOS/BIOS/HBIOS
/// service loop) and `cpmemu_oob_drain`, both of which need a running guest.
#[cfg(test)]
mod repl_tests {
    use super::*;
    use crate::telnet::tests::make_test_session_with_peer;
    use crate::telnet::TerminalType;
    use tokio::io::AsyncReadExt;

    /// A scratch `CPM/` container with drive A: created, plus a `CpmFs` on it.
    fn scratch_fs(tag: &str) -> (std::path::PathBuf, CpmFs) {
        let base = std::env::temp_dir()
            .join(format!("cpmemu_repl_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("A")).unwrap();
        let fs = CpmFs::new(base.clone());
        (base, fs)
    }

    /// Collect everything the session has written to the peer.
    ///
    /// The handlers under test return once their output is in the pipe, so a
    /// short real-time quiet period is enough to know the write side is done.
    /// Outputs here are deliberately kept well under the 512-byte duplex
    /// buffer so a handler can never block waiting for this drain.
    async fn drain(peer: &mut tokio::io::DuplexStream) -> String {
        let mut out = Vec::new();
        let mut buf = [0u8; 512];
        while let Ok(Ok(n)) = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            peer.read(&mut buf),
        )
        .await
        {
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        String::from_utf8_lossy(&out).to_string()
    }

    /// Write a `$$$.SUB` the way DRI's `SUBMIT.COM` does: 128-byte records,
    /// byte 0 a character count, **records in reverse order**, and everything
    /// past the counted text left as uninitialised junk.
    ///
    /// The junk is not incidental — the real file dumped out of this emulator
    /// had `$ *.COM\r\n` leftovers sitting after the NUL, so a reader that
    /// scanned for a terminator instead of trusting the count byte would
    /// execute garbage. This fixture reproduces that hazard deliberately.
    fn write_sub_file(dir: &std::path::Path, lines: &[&str]) {
        let mut data = Vec::new();
        for line in lines.iter().rev() {
            let mut rec = [0x00u8; 128];
            rec[0] = line.len() as u8;
            rec[1..1 + line.len()].copy_from_slice(line.as_bytes());
            // Uninitialised-buffer residue after the text, as the real one has.
            rec[1 + line.len()..].fill(b'$');
            data.extend_from_slice(&rec);
        }
        std::fs::write(dir.join("$$$.SUB"), data).unwrap();
    }

    /// The CCP consumes `A:$$$.SUB` last-record-first, so commands come out in
    /// the order the `.SUB` file listed them, and the file shrinks by a record
    /// each time until it is deleted.
    ///
    /// Format and ordering were both established empirically — DRI's real
    /// `SUBMIT.COM` was run inside this emulator and the file it produced was
    /// dumped — and confirmed against CP/M 2.2's own `CCP22.ASM`, which reads
    /// record `RC-1` and says "Yes $$$.SUB files are backwards".
    #[test]
    fn test_submit_lines_come_out_in_file_order_then_the_file_is_erased() {
        // A CP/M filesystem registers a session in the process-global image
        // registry and moves it between drives, which makes a drive look busy
        // to every other test.  Serialise with the registry tests.
        let _g = crate::cpm::image::registry::tests_lock();
        let (base, mut fs) = scratch_fs("sub");
        write_sub_file(&base.join("A"), &["VER", "DIR *.COM", "HELLO"]);
        let path = base.join("A").join("$$$.SUB");
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 3 * 128);

        assert_eq!(
            TelnetSession::cpmemu_next_submit_line(&mut fs).as_deref(),
            Some("VER"),
            "the first line of the .SUB must run first, i.e. the LAST record"
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            2 * 128,
            "the record must be consumed before it is returned"
        );
        assert_eq!(
            TelnetSession::cpmemu_next_submit_line(&mut fs).as_deref(),
            Some("DIR *.COM")
        );
        assert_eq!(
            TelnetSession::cpmemu_next_submit_line(&mut fs).as_deref(),
            Some("HELLO")
        );
        assert!(
            !path.exists(),
            "an exhausted batch must be erased, as the CCP's EXITSB does"
        );
        assert_eq!(
            TelnetSession::cpmemu_next_submit_line(&mut fs),
            None,
            "no batch file means keyboard input"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// `$$$.SUB` is read from **A: only**, whatever drive is current. That is
    /// what CP/M 2.2's CCP does, and it is the reason for the historical rule
    /// that SUBMIT only works from A: — `SUBMIT.COM` writes the file to the
    /// *current* drive, so a submit started on B: leaves a file nothing reads.
    /// Verified against the real binary, which wrote `B:$$$.SUB` when run from
    /// B: in this emulator.
    #[test]
    fn test_submit_is_read_from_drive_a_only() {
        // A CP/M filesystem registers a session in the process-global image
        // registry and moves it between drives, which makes a drive look busy
        // to every other test.  Serialise with the registry tests.
        let _g = crate::cpm::image::registry::tests_lock();
        let (base, mut fs) = scratch_fs("subdrive");
        std::fs::create_dir_all(base.join("B")).unwrap();
        write_sub_file(&base.join("B"), &["VER"]);

        assert_eq!(
            TelnetSession::cpmemu_next_submit_line(&mut fs),
            None,
            "a batch on B: must be ignored while A: has none"
        );
        fs.select(1); // B: current — still must not be consumed
        assert_eq!(
            TelnetSession::cpmemu_next_submit_line(&mut fs),
            None,
            "selecting B: must not make B:$$$.SUB run; the CCP reads A:"
        );
        assert!(base.join("B").join("$$$.SUB").exists(), "and must not erase it");

        // The same file on A: does run, even with B: selected.
        write_sub_file(&base.join("A"), &["DIR"]);
        assert_eq!(
            TelnetSession::cpmemu_next_submit_line(&mut fs).as_deref(),
            Some("DIR"),
            "A:$$$.SUB runs regardless of the current drive"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A corrupt count byte must not be trusted. Everything past the counted
    /// text in a real `$$$.SUB` is uninitialised buffer content, so a length
    /// that cannot fit the record would otherwise turn stale bytes into a
    /// command line.
    #[test]
    fn test_submit_rejects_a_corrupt_length_byte() {
        // A CP/M filesystem registers a session in the process-global image
        // registry and moves it between drives, which makes a drive look busy
        // to every other test.  Serialise with the registry tests.
        let _g = crate::cpm::image::registry::tests_lock();
        let (base, mut fs) = scratch_fs("subbad");
        let mut rec = [b'X'; 128];
        rec[0] = 200; // impossible: a record holds at most 127 text bytes
        std::fs::write(base.join("A").join("$$$.SUB"), rec).unwrap();
        assert_eq!(
            TelnetSession::cpmemu_next_submit_line(&mut fs),
            None,
            "an impossible count must not be executed as a command"
        );
        // A zero count is a blank line, which is legal and simply does nothing.
        let mut blank = [b'$'; 128];
        blank[0] = 0;
        std::fs::write(base.join("A").join("$$$.SUB"), blank).unwrap();
        assert_eq!(
            TelnetSession::cpmemu_next_submit_line(&mut fs).as_deref(),
            Some(""),
            "a zero-length record is an empty command line, not corruption"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Every program starts with the DMA at 0080H, whatever the last one left
    /// it at.
    ///
    /// CP/M's own CCP does this — `CCP22.ASM`'s `TRANS7` calls `SETDMA` on the
    /// line before `CALL TPA` — and we did not, so the DMA leaked from one
    /// program into the next. It was found by running the real DRI transients
    /// in sequence rather than one at a time: `PIP` moves the DMA to its own
    /// buffer, and a following `DUMP TEST.TXT` then printed the stale contents
    /// of 0080H (the command tail) instead of the file, with no error. Any
    /// program that reads a record without setting its own DMA first was
    /// exposed, which is most of them.
    // The registry lock is a test-serialisation mutex, and these tests run
    // on a current-thread runtime that spawns nothing else wanting it, so
    // holding it across an await cannot deadlock here.  The shape is still
    // worth flagging in general, hence the narrow allow rather than a
    // blanket one.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn test_each_program_starts_with_the_default_dma() {
        // Constructing a CP/M machine registers a session in the
        // process-global image registry, which makes drives look busy to
        // every other test.  Serialise with the registry tests.
        let _g = crate::cpm::image::registry::tests_lock();
        let (base, mut fs) = scratch_fs("dma");
        let (mut sess, _peer) = make_test_session_with_peer(TerminalType::Ascii);
        let mut cpm = Cpm::new();
        let mut modem = CpmModem::new(false);

        // MVI C,26 / LXI D,1234h / CALL 5 / JMP 0  — set DMA, then warm-boot
        // out, exactly as PIP leaves it moved.
        let set_dma: [u8; 9] = [0x0E, 26, 0x11, 0x34, 0x12, 0xCD, 0x05, 0x00, 0xC7];
        sess.cpmemu_run_program(&mut cpm, &mut modem, &set_dma, "", &mut fs)
            .await
            .unwrap();
        assert_eq!(
            fs.dma(),
            0x1234,
            "the test program did not actually move the DMA — it cannot prove anything"
        );

        // JMP 0: does nothing but leave.  Its DMA must be the default again.
        let noop: [u8; 1] = [0xC7];
        sess.cpmemu_run_program(&mut cpm, &mut modem, &noop, "", &mut fs)
            .await
            .unwrap();
        assert_eq!(
            fs.dma(),
            crate::cpm::DEFAULT_DMA,
            "a program inherited the previous program's DMA; reads land in its buffer, \
             not at 0080H"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    // The registry lock is a test-serialisation mutex, and these tests run
    // on a current-thread runtime that spawns nothing else wanting it, so
    // holding it across an await cannot deadlock here.  The shape is still
    // worth flagging in general, hence the narrow allow rather than a
    // blanket one.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn test_cpmemu_dir_reports_empty_drive_and_lists_files() {
        // A CP/M filesystem registers a session in the process-global image
        // registry and moves it between drives, which makes a drive look busy
        // to every other test.  Serialise with the registry tests.
        let _g = crate::cpm::image::registry::tests_lock();
        let (base, fs) = scratch_fs("dir");
        let (mut sess, mut peer) = make_test_session_with_peer(TerminalType::Ascii);

        // Empty drive → CP/M's "No file", not a blank screen.
        sess.cpmemu_dir(&fs, "DIR").await.unwrap();
        let empty = drain(&mut peer).await;
        assert!(
            empty.contains("No file"),
            "an empty drive must say 'No file'; got {:?}",
            empty,
        );

        std::fs::write(base.join("A").join("ONE.COM"), b"x").unwrap();
        std::fs::write(base.join("A").join("TWO.TXT"), b"y").unwrap();
        sess.cpmemu_dir(&fs, "DIR").await.unwrap();
        let listing = drain(&mut peer).await;

        let _ = std::fs::remove_dir_all(&base);
        assert!(
            listing.contains("ONE.COM") && listing.contains("TWO.TXT"),
            "DIR must list both files; got {:?}",
            listing,
        );
        // Names are padded into fixed 12-column cells so the listing tabulates.
        assert!(
            listing.contains("ONE.COM     "),
            "DIR must pad each name to a 12-column cell; got {:?}",
            listing,
        );
    }

    // The registry lock is a test-serialisation mutex, and these tests run
    // on a current-thread runtime that spawns nothing else wanting it, so
    // holding it across an await cannot deadlock here.  The shape is still
    // worth flagging in general, hence the narrow allow rather than a
    // blanket one.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn test_cpmemu_era_deletes_and_reports_no_match() {
        // A CP/M filesystem registers a session in the process-global image
        // registry and moves it between drives, which makes a drive look busy
        // to every other test.  Serialise with the registry tests.
        let _g = crate::cpm::image::registry::tests_lock();
        let (base, mut fs) = scratch_fs("era");
        let victim = base.join("A").join("GONE.TXT");
        std::fs::write(&victim, b"bye").unwrap();
        let (mut sess, mut peer) = make_test_session_with_peer(TerminalType::Ascii);

        // No operand at all.
        sess.cpmemu_era(&mut fs, "ERA").await.unwrap();
        assert!(drain(&mut peer).await.contains("ERA what?"));

        // Real erase is silent, as CP/M is.
        sess.cpmemu_era(&mut fs, "ERA GONE.TXT").await.unwrap();
        let quiet = drain(&mut peer).await;
        assert!(!victim.exists(), "ERA must delete the file");
        assert!(
            quiet.trim().is_empty(),
            "a successful ERA is silent; got {:?}",
            quiet,
        );

        // Nothing matching → "No file".
        sess.cpmemu_era(&mut fs, "ERA GONE.TXT").await.unwrap();
        let missing = drain(&mut peer).await;

        let _ = std::fs::remove_dir_all(&base);
        assert!(
            missing.contains("No file"),
            "erasing nothing must say 'No file'; got {:?}",
            missing,
        );
    }

    /// A read-only file must survive `ERA` and be *reported* as protected.
    /// Saying "No file" about a file the user can plainly see in `DIR` reads
    /// as an emulator bug, so the two refusals have to be distinguishable.
    // The registry lock is a test-serialisation mutex, and these tests run
    // on a current-thread runtime that spawns nothing else wanting it, so
    // holding it across an await cannot deadlock here.  The shape is still
    // worth flagging in general, hence the narrow allow rather than a
    // blanket one.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn test_cpmemu_era_refuses_readonly_file() {
        // A CP/M filesystem registers a session in the process-global image
        // registry and moves it between drives, which makes a drive look busy
        // to every other test.  Serialise with the registry tests.
        let _g = crate::cpm::image::registry::tests_lock();
        let (base, mut fs) = scratch_fs("era_ro");
        let keep = base.join("A").join("KEEP.TXT");
        std::fs::write(&keep, b"precious").unwrap();
        CpmFs::set_host_ro(&keep, true).unwrap();

        let (mut sess, mut peer) = make_test_session_with_peer(TerminalType::Ascii);
        sess.cpmemu_era(&mut fs, "ERA KEEP.TXT").await.unwrap();
        let out = drain(&mut peer).await;

        let survived = keep.is_file();
        // Restore write permission before cleanup so the temp dir can go.
        let _ = CpmFs::set_host_ro(&keep, false);
        let _ = std::fs::remove_dir_all(&base);

        assert!(survived, "ERA must not delete a read-only file");
        assert!(
            out.contains("File R/O"),
            "ERA of an R/O file must say 'File R/O', not 'No file'; got {:?}",
            out,
        );
    }

    /// `ERA` on a drive write-protected by BDOS 28 reports CP/M's R/O error
    /// rather than pretending nothing matched.
    // The registry lock is a test-serialisation mutex, and these tests run
    // on a current-thread runtime that spawns nothing else wanting it, so
    // holding it across an await cannot deadlock here.  The shape is still
    // worth flagging in general, hence the narrow allow rather than a
    // blanket one.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn test_cpmemu_era_reports_write_protected_drive() {
        // A CP/M filesystem registers a session in the process-global image
        // registry and moves it between drives, which makes a drive look busy
        // to every other test.  Serialise with the registry tests.
        let _g = crate::cpm::image::registry::tests_lock();
        let (base, mut fs) = scratch_fs("era_wp");
        let f = base.join("A").join("DATA.TXT");
        std::fs::write(&f, b"kept").unwrap();
        fs.set_drive_ro();

        let (mut sess, mut peer) = make_test_session_with_peer(TerminalType::Ascii);
        sess.cpmemu_era(&mut fs, "ERA DATA.TXT").await.unwrap();
        let out = drain(&mut peer).await;

        let survived = f.is_file();
        let _ = std::fs::remove_dir_all(&base);

        assert!(survived, "a write-protected drive must not lose files");
        assert!(
            out.contains("Bdos Err On A: R/O"),
            "expected CP/M's R/O error; got {:?}",
            out,
        );
    }

    /// `REN new=old` is the authentic CP/M form and `REN new old` the
    /// convenience one; both must land, and neither may clobber silently.
    // The registry lock is a test-serialisation mutex, and these tests run
    // on a current-thread runtime that spawns nothing else wanting it, so
    // holding it across an await cannot deadlock here.  The shape is still
    // worth flagging in general, hence the narrow allow rather than a
    // blanket one.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn test_cpmemu_ren_accepts_both_forms_and_refuses_to_clobber() {
        // A CP/M filesystem registers a session in the process-global image
        // registry and moves it between drives, which makes a drive look busy
        // to every other test.  Serialise with the registry tests.
        let _g = crate::cpm::image::registry::tests_lock();
        let (base, mut fs) = scratch_fs("ren");
        let a = base.join("A");
        std::fs::write(a.join("OLD.TXT"), b"payload").unwrap();
        let (mut sess, mut peer) = make_test_session_with_peer(TerminalType::Ascii);

        sess.cpmemu_ren(&mut fs, "REN NEW.TXT=OLD.TXT").await.unwrap();
        assert!(drain(&mut peer).await.trim().is_empty(), "success is silent");
        assert!(!a.join("OLD.TXT").exists() && a.join("NEW.TXT").exists());

        // Space form.
        sess.cpmemu_ren(&mut fs, "REN THIRD.TXT NEW.TXT").await.unwrap();
        assert!(drain(&mut peer).await.trim().is_empty());
        assert!(a.join("THIRD.TXT").exists(), "the space form must work too");

        // Destination exists → refuse, don't clobber.
        std::fs::write(a.join("TAKEN.TXT"), b"keep me").unwrap();
        sess.cpmemu_ren(&mut fs, "REN TAKEN.TXT=THIRD.TXT").await.unwrap();
        let refused = drain(&mut peer).await;
        let kept = std::fs::read(a.join("TAKEN.TXT")).unwrap();

        // Missing source → "No file".
        sess.cpmemu_ren(&mut fs, "REN X.TXT=NOTHERE.TXT").await.unwrap();
        let absent = drain(&mut peer).await;

        // No operand → usage.
        sess.cpmemu_ren(&mut fs, "REN").await.unwrap();
        let usage = drain(&mut peer).await;

        let _ = std::fs::remove_dir_all(&base);
        assert!(refused.contains("File exists"), "got {:?}", refused);
        assert_eq!(kept, b"keep me", "a refused REN must not overwrite");
        assert!(absent.contains("No file"), "got {:?}", absent);
        assert!(usage.contains("REN new=old"), "got {:?}", usage);
    }

    // The registry lock is a test-serialisation mutex, and these tests run
    // on a current-thread runtime that spawns nothing else wanting it, so
    // holding it across an await cannot deadlock here.  The shape is still
    // worth flagging in general, hence the narrow allow rather than a
    // blanket one.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn test_cpmemu_type_streams_text_stops_at_ctrl_z_and_refuses_binary() {
        // A CP/M filesystem registers a session in the process-global image
        // registry and moves it between drives, which makes a drive look busy
        // to every other test.  Serialise with the registry tests.
        let _g = crate::cpm::image::registry::tests_lock();
        let (base, mut fs) = scratch_fs("type");
        let a = base.join("A");
        // ^Z is CP/M's EOF filler: everything after it is padding, not text.
        std::fs::write(a.join("NOTE.TXT"), b"hello there\r\nsecond line\r\n\x1Agarbage")
            .unwrap();
        std::fs::write(a.join("PROG.COM"), [0u8; 64]).unwrap();
        let (mut sess, mut peer) = make_test_session_with_peer(TerminalType::Ascii);

        sess.cpmemu_type(&mut fs, "TYPE NOTE.TXT").await.unwrap();
        let text = drain(&mut peer).await;

        sess.cpmemu_type(&mut fs, "TYPE PROG.COM").await.unwrap();
        let binary = drain(&mut peer).await;

        sess.cpmemu_type(&mut fs, "TYPE").await.unwrap();
        let usage = drain(&mut peer).await;

        sess.cpmemu_type(&mut fs, "TYPE NOPE.TXT").await.unwrap();
        let missing = drain(&mut peer).await;

        let _ = std::fs::remove_dir_all(&base);
        assert!(text.contains("hello there") && text.contains("second line"),
            "TYPE must stream the text; got {:?}", text);
        assert!(
            !text.contains("garbage"),
            "TYPE must stop at ^Z; got {:?}",
            text,
        );
        assert!(binary.contains("Cannot TYPE a binary file"), "got {:?}", binary);
        assert!(usage.contains("TYPE what?"), "got {:?}", usage);
        assert!(missing.contains("No file"), "got {:?}", missing);
    }

    /// `pending_csi_arrow` runs on buffered input, so it has to cope with a
    /// sequence that is only partly present — the ESC has already been popped
    /// by the caller and the rest may not have arrived yet.
    #[test]
    fn test_pending_csi_arrow_reassembles_only_complete_arrows() {
        use crate::telnet::cpm_term;

        for (final_byte, expected) in [
            (b'A', cpm_term::csi_arrow_to_adm3a(b'A').unwrap()),
            (b'B', cpm_term::csi_arrow_to_adm3a(b'B').unwrap()),
            (b'C', cpm_term::csi_arrow_to_adm3a(b'C').unwrap()),
            (b'D', cpm_term::csi_arrow_to_adm3a(b'D').unwrap()),
        ] {
            let mut q: VecDeque<u8> = [b'[', final_byte, b'z'].into_iter().collect();
            assert_eq!(pending_csi_arrow(&mut q), Some(expected));
            assert_eq!(
                q.pop_front(),
                Some(b'z'),
                "only the '[' and the final byte may be consumed",
            );
        }

        // Not a CSI at all: leave the queue completely alone.
        let mut q: VecDeque<u8> = (*b"xy").into_iter().collect();
        assert_eq!(pending_csi_arrow(&mut q), None);
        assert_eq!(q.len(), 2, "a non-CSI must not be consumed");

        // Split sequence — '[' arrived, the final byte has not.  Must report
        // "no arrow" *without* eating the '[', or the byte that follows would
        // be misread once it turns up.
        let mut q: VecDeque<u8> = (*b"[").into_iter().collect();
        assert_eq!(pending_csi_arrow(&mut q), None);
        assert_eq!(q.len(), 1, "a truncated CSI must be left intact");

        // A CSI that isn't an arrow (e.g. ESC [ H) is not our business here.
        let mut q: VecDeque<u8> = (*b"[H").into_iter().collect();
        assert_eq!(pending_csi_arrow(&mut q), None);
        assert_eq!(q.len(), 2, "a non-arrow CSI must be left for the caller");
    }
}

#[cfg(test)]
mod egt80_tests {
    use super::{BUNDLED_TERMINALS, EGT80_COM, EGT80_NAME, EGT8080_COM, EGT8080_NAME};

    /// The committed `.COM`s are build artifacts of `EGT8080/*.Z80`, and CI
    /// cannot rebuild them: that needs SLR's `Z80ASM.COM` and `zxcc`, neither
    /// of which is in this repository (the assembler is third-party, and is
    /// deliberately not vendored).  So the risk is drift — a source edit whose
    /// binary was never rebuilt.
    ///
    /// These checks close most of that gap without any tooling: they compare
    /// the *shape* of each binary against what the source says it must be.
    /// What they cannot catch is a code change made without touching the
    /// version — the local `make` (which assembles with three period
    /// assemblers) remains the real gate, and `make check` should be run
    /// before a release cut.
    ///
    /// Asked through the same table the placement uses, so a second build
    /// cannot arrive with no cover at all.
    #[test]
    fn test_bundled_terminals_look_like_com_files() {
        assert_eq!(
            BUNDLED_TERMINALS.len(),
            2,
            "two builds: the 8080 one that runs everywhere, and the Z80 one that \
             carries the Z180 ASCI ports an 8080 binary cannot hold"
        );
        for (name, bytes) in BUNDLED_TERMINALS {
            assert!(!bytes.is_empty(), "{name} is empty — was it built?");
            assert_eq!(
                bytes.len() % 128,
                0,
                "a CP/M .COM is a whole number of 128-byte records; {name} is {}",
                bytes.len()
            );
            // The first instruction is `JP BEGIN`, jumping over the settings
            // patch area that follows it.
            assert_eq!(bytes[0], 0xC3, "{name} should start with a JP instruction");
        }
        assert_eq!(EGT8080_NAME, "EGT8080.COM");
        assert_eq!(EGT80_NAME, "EGT80.COM");
        // Both are 8.3 names CP/M can open, which is not automatic: EGT8080
        // is exactly the eight characters the FCB has room for.
        for (name, _) in BUNDLED_TERMINALS {
            let (stem, ext) = name.split_once('.').expect("an 8.3 name");
            assert!(stem.len() <= 8, "{name}: {stem} will not fit an FCB");
            assert_eq!(ext, "COM");
        }
    }

    /// **The 8080 sign-on names only the build that runs on an 8080.**
    ///
    /// The banner note and the help text are static literals — nothing builds
    /// them from [`BUNDLED_TERMINALS`], so nothing but this stops them naming
    /// `EGT80` on the one screen where that name is actively harmful. EGT80 is
    /// Z80 code: under `cpm_cpu = 8080` it stops at its first Z80-only opcode,
    /// and this is the screen somebody reads *because* they do not know what to
    /// type. It ships again for the Z180 ASCI ports, and drive A: holds it —
    /// but the 8080 note must point at the other one.
    ///
    /// A whole-word check, because `EGT8080` contains `EGT80`: a substring test
    /// would pass on either name and prove nothing. That is not hypothetical —
    /// it is why this test exists in this form.
    #[test]
    fn test_the_8080_note_never_names_the_z80_build() {
        use super::{TelnetSession, CPM_NOTE_8080};

        let mut lines: Vec<&str> = vec![CPM_NOTE_8080];
        for petscii in [true, false] {
            lines.extend(TelnetSession::cpmemu_help_lines(petscii));
        }
        for line in lines {
            for word in line.split(|c: char| !c.is_ascii_alphanumeric()) {
                assert_ne!(
                    word, "EGT80",
                    "this line names the Z80 build, which stops at its first \
                     Z80-only opcode under cpm_cpu = 8080: {line:?}"
                );
            }
        }
    }

    /// **The one that runs on both processors is offered first.**
    ///
    /// Not cosmetic: the placement log and the emulator's help lead with
    /// whatever heads this table, and under `cpm_cpu = 8080` the Z80 build is
    /// not merely slower, it crashes the machine. A reader who takes the first
    /// name they see has to be right on either setting.
    #[test]
    fn test_the_terminal_that_runs_on_both_comes_first() {
        assert_eq!(BUNDLED_TERMINALS[0].0, EGT8080_NAME);
    }

    /// Every bundled terminal lands in **both** places, and a second run
    /// changes nothing.
    ///
    /// The regression this pins is a real one, reported after an operator
    /// erased their transfer directory: the placement was an `async` method on
    /// `TelnetSession` called only from `cpmemu_ensure_drives`, so start-up
    /// recreated the sixteen drive folders with no terminal in any of them, and
    /// the loose transfer-directory copies — whose entire purpose is to reach a
    /// terminal *without* starting the emulator — appeared only once you had
    /// started the emulator.
    ///
    /// Both halves matter and they pull opposite ways: place all four, and
    /// overwrite none of them.  A terminal saves its settings inside its own
    /// `.COM`, so a run that "helpfully" refreshed the copies would silently
    /// discard the operator's configuration.
    #[test]
    fn test_bundled_terminals_are_placed_in_both_places_and_never_overwritten() {
        let base = std::env::temp_dir().join(format!("egw_place_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let drive_a = base.join("CPM").join("A");
        std::fs::create_dir_all(&drive_a).expect("temp drive A:");

        super::place_bundled_terminals(base.to_str().expect("utf-8 temp path"), true);

        // Four files: two builds x two destinations.
        for (name, bytes) in BUNDLED_TERMINALS {
            for dir in [base.as_path(), drive_a.as_path()] {
                let p = dir.join(name);
                let got = std::fs::read(&p).unwrap_or_else(|e| panic!("{p:?} missing: {e}"));
                assert_eq!(&got, bytes, "{p:?} is not the shipped build");
            }
        }

        // Now stand in for an operator who has configured their copy, and for
        // one they deleted.  A second run must restore only the missing file.
        let configured = drive_a.join(EGT80_NAME);
        std::fs::write(&configured, b"not the shipped build").expect("write");
        std::fs::remove_file(base.join(EGT8080_NAME)).expect("remove");

        super::place_bundled_terminals(base.to_str().expect("utf-8 temp path"), true);

        assert_eq!(
            std::fs::read(&configured).expect("still there"),
            b"not the shipped build",
            "a configured terminal was overwritten — that is the operator's settings gone",
        );
        assert_eq!(
            std::fs::read(base.join(EGT8080_NAME)).expect("restored"),
            EGT8080_COM,
            "a deleted terminal should come back, which is the documented reset",
        );
        // And no half-written temporaries left behind.
        let litter: Vec<_> = std::fs::read_dir(&base)
            .expect("readable")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".t"))
            .collect();
        assert!(litter.is_empty(), "left temporaries behind: {litter:?}");

        // Off: a missing file stays missing.  The switch decides whether a
        // *missing* terminal is written; it never removes one, which is why the
        // configured copy above is still expected to be sitting there.
        std::fs::remove_file(base.join(EGT8080_NAME)).expect("remove");
        super::place_bundled_terminals(base.to_str().expect("utf-8 temp path"), false);
        assert!(
            !base.join(EGT8080_NAME).exists(),
            "place_bundled_terminals = false still wrote a missing terminal",
        );
        assert!(
            drive_a.join(EGT80_NAME).exists(),
            "turning the switch off must not remove anything",
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_bundled_terminals_have_their_settings_block_where_the_save_expects_it() {
        // Each rewrites exactly one record of its own file to save settings,
        // and that record number is compiled into it: the block sits at 0180H,
        // which is file offset 0x80 — record 1.  If the layout ever moves, the
        // save would rewrite the wrong part of the program, so pin it here.
        //
        // The signature is the same string in both builds on purpose: each
        // reads only its own file, and one signature keeps this check, and the
        // documented way of reading defaults out of a committed binary, from
        // needing to know which build it is looking at.
        const OFFSET: usize = 0x80;
        for (name, bytes) in BUNDLED_TERMINALS {
            assert!(bytes.len() > OFFSET + 8, "{name} is too short to hold a settings block");
            assert_eq!(
                &bytes[OFFSET..OFFSET + 8],
                b"EGT80CFG",
                "{name}: settings signature must sit at file offset 0x80 (record 1)"
            );
        }
    }

    /// **Each build carries its own name, and not the other's.**
    ///
    /// That string is the FCB the program opens to save its settings, so a
    /// wrong one writes the operator's configuration into a file that is not
    /// this program — and with two builds on one drive, the file it would land
    /// in is the *other terminal*.
    ///
    /// **Both literals are FCB fields, not display names**: 8 characters of
    /// name padded with spaces, then 3 of extension, 11 bytes with no dot.
    /// Spelling them out matters — the 0.9.2 rename rewrote `EGT80   COM` into
    /// `EGT8080   COM`, which is thirteen bytes and therefore cannot occur in
    /// any FCB, so this guard passed while proving nothing. A literal that is a
    /// *layout* is not a name to be search-and-replaced.
    #[test]
    fn test_each_shipped_terminal_carries_its_own_name() {
        let fcb_name = |bytes: &[u8], want: &[u8]| bytes.windows(want.len()).any(|w| w == want);
        assert!(
            fcb_name(EGT8080_COM, b"EGT8080 COM"),
            "EGT8080.COM does not carry its own name — it would save its \
             settings somewhere else"
        );
        assert!(
            fcb_name(EGT80_COM, b"EGT80   COM"),
            "EGT80.COM does not carry its own name — it would save its \
             settings somewhere else"
        );
        // Neither may carry the other's, which is the failure that would put
        // one terminal's settings inside the other's file.  `EGT8080 COM` is
        // its own 11 bytes and cannot be mistaken for `EGT80   COM`, so this
        // is a genuine pair of checks rather than one restated.
        assert!(
            !fcb_name(EGT8080_COM, b"EGT80   COM"),
            "EGT8080.COM carries the Z80 build's filename"
        );
        assert!(
            !fcb_name(EGT80_COM, b"EGT8080 COM"),
            "EGT80.COM carries the 8080 build's filename"
        );
    }

    /// **Only the Z80 build carries the Z180 probe, and that is the byte that
    /// matters.**
    ///
    /// `MLT BC` — `ED 4C` — is what asks the processor whether it is a Z180,
    /// and it is the one ED-prefixed pair in this program that would *execute*
    /// on a machine that might be a true 8080, where an `ED` byte is an
    /// undocumented `CALL`: it would not fail, it would jump into the weeds.
    /// The porter replaces that probe with `SCF` in the 8080 build, so the
    /// question is answered "no" without being asked and the ASCI driver below
    /// it is unreachable.
    ///
    /// **Measured, not reasoned, because reasoning got it wrong once.** The
    /// 8080 image still contains eleven `ED 38`/`ED 39` bytes — the driver's
    /// `IN0`/`OUT0`, laid down as `DB`, which `check8080.py` skips because they
    /// are directives rather than instructions. A comment claiming "no ED byte
    /// anywhere" survived in the source for a while on the strength of that
    /// checker passing. Unreachable is the true and sufficient property; the
    /// absence of `ED 4C` is what enforces it, so that is what this pins.
    #[test]
    fn test_only_the_z80_build_carries_the_z180_probe() {
        const MLT_BC: &[u8] = &[0xED, 0x4C];
        let has = |b: &[u8]| b.windows(2).any(|w| w == MLT_BC);
        assert!(
            has(EGT80_COM),
            "EGT80.COM has lost its Z180 probe — it is the build that exists to \
             find one, and without MLT BC the ASCI family can never be selected"
        );
        assert!(
            !has(EGT8080_COM),
            "EGT8080.COM carries MLT BC. On a true 8080 that ED byte is an \
             undocumented CALL into the middle of the program"
        );
        // And the driver bytes really are still there in both, so the claim
        // above is about reachability rather than about their absence.  A test
        // that quietly stopped being true because someone excised the driver
        // should say so rather than pass on a changed premise.
        let in0 = |b: &[u8]| b.windows(2).filter(|w| w == &[0xED, 0x38]).count();
        assert!(in0(EGT80_COM) > 0 && in0(EGT8080_COM) > 0, "the ASCI driver moved");
    }

    /// The one check that closes the "code change without a version bump" gap
    /// the sibling tests above cannot: an explicit hash of the committed
    /// binary.
    ///
    /// It is compiled into every release with `include_bytes!` but no CI
    /// runner can rebuild it — that needs a period Z80 assembler under zxcc
    /// — so the checked-in artifact, not the `.Z80` source, is what users
    /// actually run.  Pinning it here means the bytes cannot change without
    /// someone updating this constant in the same commit, which puts the
    /// change in front of a reviewer.  It does *not* prove a binary matches
    /// its source; only `make` in `EGT8080/` does that.
    ///
    /// **When you legitimately rebuild**, run `make` in `EGT8080/` (which
    /// gates on three independent assemblers and on the 8080 instruction-set
    /// check), then update this from:
    ///     sha256sum EGT8080/EGT8080.COM
    #[test]
    fn test_bundled_terminals_match_pinned_hashes() {
        use sha2::{Digest, Sha256};

        // Same order as `BUNDLED_TERMINALS`, which the zip below checks
        // rather than assumes — it caught the order being wrong here when
        // there were two, which is exactly the mistake that would otherwise
        // pin each binary against the other one's hash and pass.
        const PINNED: &[(&str, &str)] = &[
            ("EGT8080.COM", "61563d7fc5c55b6c65a14cf29e61b6acef2f42aee4f898fb59f3fbdda8417cbd"),
            ("EGT80.COM", "2e4031519467b5b761bca9eddedd732b046a49f694c2acf00916e6926e8ace5b"),
        ];

        for ((name, bytes), (pin_name, pinned)) in BUNDLED_TERMINALS.iter().zip(PINNED) {
            // Zipped, so a terminal added to one list and not the other is a
            // length mismatch rather than a silently unchecked binary.
            assert_eq!(name, pin_name, "the two lists have drifted out of order");
            let actual = format!("{:x}", Sha256::digest(*bytes));
            assert_eq!(
                &actual, pinned,
                "\n{name} has changed but its pinned hash has not.\n\
                 If you rebuilt it on purpose, run `make` in EGT8080/ and set\n\
                 its entry in PINNED to:\n    {}\n",
                actual
            );
        }
        assert_eq!(BUNDLED_TERMINALS.len(), PINNED.len(), "every terminal needs a hash");
    }

    /// Every EGT8080 screen has to fit the terminal it is printed on: 24 rows by
    /// 80 columns, the CP/M-era console (ADM-3A, VT100, and what the gateway
    /// renders to).  A screen two lines too tall loses its *heading* off the
    /// top, which is the part naming what you are looking at.
    ///
    /// This is a source-level check on purpose: it parses the `DB` strings out
    /// of the sources, so it needs no assembler and runs in CI, unlike the
    /// binaries themselves.  It caught help page 3 having been over the limit
    /// since it was written, and page 2 going over when line settings were
    /// described in it.  Two rows are reserved for the "Press any key" prompt
    /// that follows a full-screen page.
    ///
    /// This was written against a source whose program name was `EGT80`, and
    /// the fourteen strings carrying that name are two columns wider now that
    /// it is `EGT8080` — a block already near the limit would have wrapped
    /// with nothing failing.
    ///
    /// Nothing else checks this.  The retired porter refused to *emit* a source
    /// line over 80 columns, and when it went that rule went with it — `make
    /// check` is the M80 and ZMAC portability gates, and `check8080.py` decodes
    /// opcodes.  Neither has ever had a line-length rule, and the source limit
    /// was never the same limit as this one anyway: one bounds the assembly
    /// file, this bounds the screen the program draws.
    #[test]
    fn test_egt80_screens_fit_a_24_by_80_terminal() {
        check_screens_fit("EGT8080.Z80", include_str!("../../EGT8080/EGT8080.Z80"));
        // Both sources, not just the derived one.  EGT80.Z80 is where a screen
        // is actually edited, and the porter rewrites instructions rather than
        // text -- so a line that overruns is introduced here and inherited
        // there, and checking only the generated file would report the fault
        // against a file nobody typed it into.  It is also not the same set of
        // screens: the Z180 ASCI menus exist only in this one.
        check_screens_fit("EGT80.Z80", include_str!("../../EGT8080/EGT80.Z80"));
    }

    /// One source's screens, for [`test_egt80_screens_fit_a_24_by_80_terminal`].
    #[cfg(test)]
    fn check_screens_fit(src_name: &str, src: &str) {
        const ROWS: usize = 24;
        const COLS: usize = 80;
        const PROMPT_ROWS: usize = 2; // blank line + "Press any key."

        let mut label = String::new();
        let mut text = String::new();
        // Only *printable* blocks are screens.  Every string EGT8080 prints ends
        // with a zero terminator, because that is what its string-output
        // routine stops on; a `DB` block without one is a lookup table indexed
        // a few bytes at a time (the HBIOS device-type names, say) and its
        // total length means nothing on a screen.  Checking for the terminator
        // is what separates the two — a name-based exception list would rot.
        let check = |label: &str, text: &str, terminated: bool| {
            if label.is_empty() || !terminated {
                return;
            }
            let rows = text.matches('\n').count();
            let widest = text
                .split('\n')
                .map(|l| l.trim_end_matches('\r').len())
                .max()
                .unwrap_or(0);
            assert!(
                rows + PROMPT_ROWS <= ROWS,
                "{src_name}: screen {label} is {rows} rows; with the key prompt \
                 that scrolls its heading off a {ROWS}-row terminal"
            );
            assert!(
                widest <= COLS - 2,
                "{src_name}: screen {label} has a {widest}-column line; it would \
                 wrap on an {COLS}-column terminal"
            );
        };

        let mut terminated = false;
        for line in src.lines() {
            let body = line.split(';').next().unwrap_or("");
            let ends_with_zero = |operands: &str| {
                let t = operands.trim().trim_end_matches(',');
                t == "0" || t.ends_with(",0")
            };
            // A new label starts a new message; an indented DB continues one.
            if let Some((name, rest)) = body.split_once(":") {
                if !name.starts_with(char::is_whitespace)
                    && !name.is_empty()
                    && rest.trim_start().starts_with("DB")
                {
                    check(&label, &text, terminated);
                    label = name.to_string();
                    let operands = rest.trim_start().trim_start_matches("DB");
                    text = render_db(operands);
                    terminated = ends_with_zero(operands);
                    continue;
                }
            }
            if !label.is_empty() && body.starts_with(char::is_whitespace) {
                let t = body.trim_start();
                if let Some(rest) = t.strip_prefix("DB") {
                    text.push_str(&render_db(rest));
                    terminated = ends_with_zero(rest);
                    continue;
                }
            }
            check(&label, &text, terminated);
            label.clear();
            text.clear();
            terminated = false;
        }
        check(&label, &text, terminated);
    }

    /// Render one `DB` operand list the way the console would see it: quoted
    /// runs verbatim, and the `CR`/`LF` symbols as the bytes they equate to.
    /// Anything else (a numeric byte, a `0` terminator) contributes no width.
    fn render_db(operands: &str) -> String {
        let mut out = String::new();
        let mut rest = operands;
        while !rest.is_empty() {
            if let Some(open) = rest.find('\'') {
                let (before, tail) = rest.split_at(open);
                out.push_str(&symbols(before));
                let tail = &tail[1..];
                match tail.find('\'') {
                    Some(close) => {
                        out.push_str(&tail[..close]);
                        rest = &tail[close + 1..];
                    }
                    None => return out, // unterminated: nothing sane to add
                }
            } else {
                out.push_str(&symbols(rest));
                return out;
            }
        }
        out
    }

    fn symbols(part: &str) -> String {
        part.split(|c: char| !c.is_ascii_alphanumeric())
            .filter_map(|tok| match tok {
                "CR" => Some('\r'),
                "LF" => Some('\n'),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn test_egt80_default_port_matches_the_gateway_default() {
        // "They work together out of the box" rests on two constants in two
        // different languages agreeing: the gateway's DEFAULT_UART and the
        // PBASE/PKIND defaults compiled into EGT8080.  Nothing in either build
        // would notice them drifting apart — the symptom would be a fresh
        // install where the bundled terminal cannot reach the modem, which is
        // exactly the confusion this pairing exists to prevent.
        //
        // Both sources.  The defaults are DB values, and the porter rewrites
        // instructions rather than data, so the two blocks can only agree by
        // having been edited together -- and the pairing this test defends is
        // "the bundled terminal reaches the modem on a fresh install", which
        // is a promise about whichever of the two the operator typed.
        for (src_name, src) in [
            ("EGT8080.Z80", include_str!("../../EGT8080/EGT8080.Z80")),
            ("EGT80.Z80", include_str!("../../EGT8080/EGT80.Z80")),
        ] {
        let field = |name: &str| -> String {
            src.lines()
                .find(|l| l.trim_start().starts_with(&format!("{name}:")))
                .unwrap_or_else(|| panic!("{src_name} should declare {name}"))
                .split_whitespace()
                .nth(2)
                .unwrap_or_else(|| panic!("{name} should have a DB value"))
                .to_string()
        };

        // EGT8080's port kind default must be the SIO family (PKSIO = 1).
        assert_eq!(field("PKIND"), "PKSIO", "{src_name} should default to the SIO family");

        // ...and its base address must be the status port DEFAULT_UART resolves
        // to.  EGT8080 writes it as a Z80 hex literal, e.g. 82H.
        let base = field("PBASE");
        let base = u8::from_str_radix(base.trim_end_matches('H'), 16)
            .unwrap_or_else(|_| panic!("PBASE should be a hex literal, got {base}"));
        match crate::cpm::resolve_access(crate::cpm::uart::DEFAULT_UART) {
            crate::cpm::ModemAccess::Ports(p) => assert_eq!(
                p.status_port, base,
                "{src_name} defaults to port {base:#04x} but the gateway's default \
                 profile ({}) answers at {:#04x}",
                crate::cpm::uart::DEFAULT_UART, p.status_port
            ),
            other => panic!("the default UART profile should be a port profile, got {other:?}"),
        }
        }
    }

    /// The shipped settings block and the damaged-block fallback must not
    /// drift apart.
    ///
    /// EGT8080 carries its defaults twice: once as the settings block inside the
    /// `.COM` (what a fresh copy starts with) and once as `DEFBLK`, the table
    /// `CFGBAD` copies over that block when a saved one fails validation. They
    /// have to agree in every field but `PKIND` — a block we just rejected is
    /// no reason to believe any particular port is present.
    ///
    /// This is checked because they *did* drift, twice in one evening: the
    /// fallback used to be a run of loads that shared one accumulator between
    /// fields, so changing the shipped display mode to ASCII and the shipped
    /// clear to the ADM-3A `^Z` each left the fallback quietly restoring the
    /// old value. The table replaced the register dance; this keeps the table
    /// honest. Source-level, so it runs in CI, which cannot rebuild EGT8080.
    ///
    /// `CFGBAD` copies this table over the settings block with a copy loop —
    /// the 8080 build's replacement for the Z80 `LDIR`, and precisely the code
    /// this checks the table against.
    #[test]
    fn test_egt80_fallback_defaults_match_the_shipped_block() {
        for src in [
            include_str!("../../EGT8080/EGT8080.Z80"),
            // Both sources: the porter rewrites instructions, not data, so a
            // default edited in one block and not the other survives the
            // derivation intact and ships in both binaries.
            include_str!("../../EGT8080/EGT80.Z80"),
        ] {
            check_fallback_defaults(&src.replace("\r\n", "\n"));
        }
    }

    /// One source's two default blocks, compared field by field.
    #[cfg(test)]
    fn check_fallback_defaults(src: &str) {
        // The caller normalises to LF: a Windows checkout has CRLF endings and
        // every line here would otherwise carry a trailing \r into the value.

        /// `LABEL: DB value ; comment` -> (label, value), resolving the two
        /// port-kind equates the table uses by name.
        fn field(line: &str) -> Option<(String, u8)> {
            let code = line.split(';').next()?.trim();
            let (label, rest) = match code.split_once(':') {
                Some((l, r)) => (l.trim().to_string(), r),
                None => (String::new(), code),
            };
            let val = rest.trim().strip_prefix("DB")?.trim();
            let n = match val {
                "PKNONE" => 0,
                "PKSIO" => 1,
                v if v.ends_with('H') => u8::from_str_radix(v.trim_end_matches('H'), 16).ok()?,
                v => v.parse().ok()?,
            };
            Some((label, n))
        }

        // The shipped block: every DB between the signature and CFGADR.
        let block_start = src.find("CFGSIG: DB").expect("settings block signature not found");
        let block_end = src[block_start..].find("CFGADR").expect("end of settings block not found");
        let shipped: Vec<(String, u8)> = src[block_start..block_start + block_end]
            .lines()
            .filter_map(field)
            // The 8-byte signature is a quoted string, so `field` already
            // rejects it; naming it here as well means a future signature
            // written as numeric bytes cannot slip in as a setting.
            .filter(|(name, _)| name != "CFGSIG")
            .collect();

        // The fallback table: every DB between DEFBLK and DEFLEN.
        let tbl_start = src.find("DEFBLK: DB").expect("DEFBLK not found");
        let tbl_end = src[tbl_start..].find("DEFLEN").expect("DEFLEN not found");
        let fallback: Vec<(String, u8)> = src[tbl_start..tbl_start + tbl_end]
            .lines()
            .filter_map(field)
            .collect();

        assert!(
            shipped.len() >= 10,
            "only parsed {} shipped fields — this scan has stopped matching",
            shipped.len()
        );
        assert_eq!(
            shipped.len(),
            fallback.len(),
            "the fallback table has {} entries for {} settings — a field was added \
             to the block without adding it here (or the reverse)",
            fallback.len(),
            shipped.len()
        );

        for (i, ((name, want), (_, got))) in shipped.iter().zip(fallback.iter()).enumerate() {
            if name == "PKIND" {
                assert_eq!(
                    *got, 0,
                    "the fallback must select NO port: a rejected block is no reason to \
                     believe one is there"
                );
                continue;
            }
            assert_eq!(
                got, want,
                "field {i} ({name}): the shipped default is {want} but a damaged block \
                 would be restored to {got}"
            );
        }
    }

    #[test]
    fn test_bundled_terminals_match_the_versions_in_their_sources() {
        // Catches the realistic mistake: bumping the version in a .Z80 and
        // committing without rebuilding the .COM.
        //
        // `include_str!` needs a literal path, so the pairs are written out
        // rather than looped over `BUNDLED_TERMINALS`.
        let mver = |name: &str, src: &str| -> String {
            let line = src
                .lines()
                .find(|l| l.trim_start().starts_with("MVER:"))
                .unwrap_or_else(|| panic!("{name}'s source should declare MVER"));
            line.split('\'').nth(1).expect("MVER should contain a quoted version string").to_string()
        };

        let pairs: [(&str, &str, &[u8]); 2] = [
            (EGT8080_NAME, include_str!("../../EGT8080/EGT8080.Z80"), EGT8080_COM),
            (EGT80_NAME, include_str!("../../EGT8080/EGT80.Z80"), EGT80_COM),
        ];
        let mut versions = Vec::new();
        for (name, src, bytes) in pairs {
            let version = mver(name, src);
            assert!(
                bytes.windows(version.len()).any(|w| w == version.as_bytes()),
                "the built {name} does not contain its source's version string \
                 ({version:?}) — rebuild it with `make -C EGT8080`"
            );
            versions.push(version);
        }
        // The two version lines must differ, and they do by naming their
        // processor.  Without this the check could be satisfied by pinning
        // both binaries against one source's string — which is exactly what a
        // copy-paste of the block above would do, and it would pass while
        // proving nothing about the second build.
        assert_ne!(
            versions[0], versions[1],
            "each build's version line should name its own processor"
        );
    }
}
