//! CP/M emulator — a real CP/M 2.2 environment running in an emulated Z80,
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
//! it ships with its own terminal (EGT80) on drive A:.  When disabled the menu
//! item is hidden and `K` is rejected.  The guest's route off the machine is the
//! virtual modem, which now also defaults on (to the port EGT80 expects), so a
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
use std::sync::atomic::AtomicBool;

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
/// down, and an idle EGT80 terminal spun the host at **161% CPU**; with this it
/// measures 1.4%.  Only the demonstrably idle case is paced, so throughput is
/// untouched: any pass doing real work resets the count to zero.
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

/// EGT80, the gateway's own CP/M terminal, carried inside the binary and
/// placed on drive A: when the drive folders are created (see
/// [`TelnetSession::cpmemu_place_egt80`]).  It is built from `EGT80/EGT80.Z80`
/// by that directory's Makefile; `include_bytes!` means a release ships one
/// file and the terminal is simply *there* when someone first opens the
/// emulator, rather than being something they have to find and upload.
const EGT80_COM: &[u8] = include_bytes!("../../EGT80/EGT80.COM");

/// Filename EGT80 is placed under.  It is also the name EGT80 looks for when
/// saving its settings (CP/M never tells a program its own name, so the name
/// is compiled into it) — renaming the file costs the user that feature.
const EGT80_NAME: &str = "EGT80.COM";

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
struct CpmPeerReg {
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
        self.send_line(&format!(
            "  {}",
            self.dim("CP/M 2.2 (iz80).  Type HELP.")
        ))
        .await?;
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
        self.send_line("").await?;

        // The filesystem state (current drive, DMA) persists across the
        // whole session at the `CPM/` container.  Canonicalize so the jail
        // prefix check compares absolute paths.
        let cfg = config::get_config();
        let mut base = PathBuf::from(&cfg.transfer_dir);
        base.push("CPM");
        let base = std::fs::canonicalize(&base).unwrap_or(base);
        let mut fs = CpmFs::new(base);

        self.cpmemu_repl(&mut fs).await
    }

    /// Ensure `CPM/` and each drive folder `CPM/A`..`CPM/P` exist under
    /// `transfer_dir`, creating any that are missing.  Idempotent and run
    /// on every launch, so a program can select any of the 16 drives without
    /// hitting a "drive does not exist" error.  An empty folder *is* a
    /// formatted, ready-to-use drive here — the CP/M directory is synthesized
    /// from the folder's real files, so there is nothing to `CLRDIR`/format.
    /// Jailed by construction —
    /// the paths are built under the configured `transfer_dir`.
    async fn cpmemu_ensure_drives(&mut self) -> Result<(), std::io::Error> {
        let cfg = config::get_config();
        for drive in b'A'..=CPM_LAST_DRIVE {
            let mut p = PathBuf::from(&cfg.transfer_dir);
            p.push("CPM");
            p.push((drive as char).to_string());
            tokio::fs::create_dir_all(&p).await?;
        }
        self.cpmemu_place_egt80(&cfg.transfer_dir).await;
        Ok(())
    }

    /// Put EGT80 on drive A: if it is not already there.
    ///
    /// **Only when absent, never overwriting.** EGT80 saves its settings — the
    /// selected serial port, the ANSI/ASCII choice, the menu key — into a patch
    /// area inside its own `.COM` file, so refreshing the copy on every launch
    /// would silently throw away the user's configuration. It also means a user
    /// may deliberately keep an older build, or their own build with different
    /// defaults. Deleting the file restores the shipped copy on the next launch,
    /// which is the documented way to get back to a known state.
    ///
    /// A failure here is logged and ignored rather than propagated: not having
    /// the bundled terminal is a missing convenience, and it must not stop
    /// someone from reaching a CP/M prompt to run their own software.
    async fn cpmemu_place_egt80(&mut self, transfer_dir: &str) {
        let mut path = PathBuf::from(transfer_dir);
        path.push("CPM");
        path.push("A");
        path.push(EGT80_NAME);
        if tokio::fs::metadata(&path).await.is_ok() {
            return; // already there — leave it, settings and all
        }
        // Written to a temporary name and renamed into place, the way
        // `config.rs` writes the config file.  Two sessions can enter the
        // emulator at the same moment on a first-ever launch, both find the
        // file absent, and both write it; a plain write would let a CP/M
        // program load a half-written image.  A rename is atomic, so every
        // reader sees either no file or the whole one.  The temporary name
        // carries the process id so two gateways sharing a transfer directory
        // cannot collide either.
        let tmp = path.with_extension(format!("t{}", std::process::id()));
        let placed = match tokio::fs::write(&tmp, EGT80_COM).await {
            Ok(()) => tokio::fs::rename(&tmp, &path).await,
            Err(e) => Err(e),
        };
        match placed {
            Ok(()) => glog!(
                "CP/M: placed the bundled {} ({} bytes) on drive A:",
                EGT80_NAME,
                EGT80_COM.len()
            ),
            Err(e) => {
                let _ = tokio::fs::remove_file(&tmp).await; // don't leave litter
                glog!("CP/M: could not place {} on drive A: {}", EGT80_NAME, e);
            }
        }
    }

    /// The Rust CCP-lite command loop.  Prints the `A>` prompt, reads a
    /// line, and dispatches: host-exit words leave; built-ins run; anything
    /// else is looked up as `<verb>.COM` on the drive and run as a real
    /// transient program, falling back to CP/M's bad-verb error (`VERB?`)
    /// when no such file exists.
    async fn cpmemu_repl(&mut self, fs: &mut CpmFs) -> Result<(), std::io::Error> {
        // One machine persists for the whole session: the TPA (and the low
        // vectors, reinstalled each load) survive across program runs, so a
        // warm-boot back to `A>` leaves the last program's memory image in
        // place — which is what makes SAVE authentic (dump the TPA a prior
        // program, e.g. DDT, left behind).
        let mut cpm = Cpm::new();
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
        let _peer_reg = if modem.enabled() {
            crate::serial::cpm_peer_listen_enter();
            let cfg = config::get_config();
            // Announcing to our own master is not gated on `allow_peer_dial`
            // (see serial::cpm_slave_announce): that setting governs dialing
            // arbitrary peers, and without the announcement the master cannot
            // reach this slave's CP/M endpoint at all.
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
        } else {
            None
        };
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
                    self.send_line(&format!(
                        "  {}",
                        self.green("CP/M 2.2 emulator (iz80 Z80 core)")
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
        } else if fs
            .resolve(&old)
            .map(|p| CpmFs::host_is_ro(&p))
            .unwrap_or(false)
        {
            self.send_line("  File R/O").await?;
        } else if fs.open_existing(&Self::cpmemu_fcb(&nn, &ne)).is_some() {
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
        if fs.make(&fcb).is_none() {
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
    async fn cpmemu_help(&mut self) -> Result<(), std::io::Error> {
        for line in [
            "  Built-in commands:",
            "  DIR [d:][afn]  list files (DIR *.COM)",
            "  ERA name   erase file(s) (wildcards)",
            "  REN new=old  rename a file",
            "  TYPE file  show a text file",
            "  SAVE n file  save n TPA pages",
            "  USER n     select user area (0)",
            "  A: .. P:   change drive",
            "  VER        emulator version",
            "  HELLO      BDOS print-string demo",
            "  ECHO       interactive console demo",
            "  name       run name.COM from the drive",
            "  HELP / ?   this help",
            "  EXIT/BYE/QUIT  leave CP/M",
        ] {
            self.send_line(line).await?;
        }
        Ok(())
    }

    /// Run a loaded program on the emulated Z80, servicing the console BDOS
    /// group against the live session, until it warm-boots, the user breaks
    /// out, or the instruction ceiling is hit.  Returns `Ok(false)` if the
    /// session disconnected (the caller should leave the emulator), else
    /// `Ok(true)` (return to the `A>` prompt).
    async fn cpmemu_run_program(
        &mut self,
        cpm: &mut Cpm,
        modem: &mut CpmModem,
        program: &[u8],
        tail: &str,
        fs: &mut CpmFs,
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
                            // List (printer / LST:) output, char in E.  There
                            // is no physical printer, so route it to the console
                            // — a program's printer output stays visible instead
                            // of vanishing (previously the call returned 0 and
                            // dropped the byte).
                            self.cpmemu_emit(&mut term, &[cpm.arg_e()]).await?;
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
                        // CONOUT / LIST: character in C to the console.
                        4 | 5 => {
                            let c = cpm.arg_c();
                            self.cpmemu_emit(&mut term, &[c]).await?;
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

            // A pending first ESC + another ESC = break-out (slow, human).
            if is_esc_key(b, is_petscii) {
                if *last_esc {
                    *last_esc = false;
                    return Ok(ConIn::BreakOut);
                }
                // Peek for a fast CSI arrow (ANSI terminals only).
                if !is_petscii {
                    match self.cpmemu_peek_arrow().await? {
                        ArrowPeek::Arrow(code) => return Ok(ConIn::Byte(code)),
                        // A non-arrow CSI was consumed whole; read the next key.
                        ArrowPeek::UnknownCsi => continue,
                        ArrowPeek::NotCsi => {} // fall through: deliver the ESC
                    }
                }
                // Lone ESC: deliver it; a following ESC becomes the break-out.
                *last_esc = true;
                return Ok(ConIn::Byte(0x1B));
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
    /// screen-painting program like EGT80 issues many writes per update, so it
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
                    if is_esc_key(b, is_petscii) {
                        if *last_esc {
                            *last_esc = false;
                            return Ok(OobDrain::BreakOut);
                        }
                        *last_esc = true;
                    } else {
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
    #[tokio::test]
    async fn test_each_program_starts_with_the_default_dma() {
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

    #[tokio::test]
    async fn test_cpmemu_dir_reports_empty_drive_and_lists_files() {
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

    #[tokio::test]
    async fn test_cpmemu_era_deletes_and_reports_no_match() {
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
    #[tokio::test]
    async fn test_cpmemu_era_refuses_readonly_file() {
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
    #[tokio::test]
    async fn test_cpmemu_era_reports_write_protected_drive() {
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
    #[tokio::test]
    async fn test_cpmemu_ren_accepts_both_forms_and_refuses_to_clobber() {
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

    #[tokio::test]
    async fn test_cpmemu_type_streams_text_stops_at_ctrl_z_and_refuses_binary() {
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
    use super::{EGT80_COM, EGT80_NAME};

    /// The committed `EGT80.COM` is a build artifact of `EGT80/EGT80.Z80`, and
    /// CI cannot rebuild it: that needs SLR's `Z80ASM.COM` and `zxcc`, neither
    /// of which is in this repository (the assembler is third-party, and is
    /// deliberately not vendored).  So the risk is drift — a source edit whose
    /// binary was never rebuilt.
    ///
    /// These checks close most of that gap without any tooling: they compare
    /// the *shape* of the binary against what the source says it must be, and
    /// the version string against the one the source prints.  What they cannot
    /// catch is a code change made without touching the version — the local
    /// `make` (which assembles with three period assemblers) remains the real
    /// gate, and `make check` should be run before a release cut.
    #[test]
    fn test_bundled_egt80_looks_like_a_com_file() {
        assert!(!EGT80_COM.is_empty(), "EGT80.COM is empty — was it built?");
        assert_eq!(
            EGT80_COM.len() % 128,
            0,
            "a CP/M .COM is a whole number of 128-byte records; got {}",
            EGT80_COM.len()
        );
        // The first instruction is `JP BEGIN`, jumping over the settings
        // patch area that follows it.
        assert_eq!(EGT80_COM[0], 0xC3, "should start with a JP instruction");
        assert_eq!(EGT80_NAME, "EGT80.COM");
    }

    #[test]
    fn test_bundled_egt80_has_its_settings_block_where_the_save_expects_it() {
        // EGT80 rewrites exactly one record of its own file to save settings,
        // and that record number is compiled into it: the block sits at 0180H,
        // which is file offset 0x80 — record 1.  If the layout ever moves, the
        // save would rewrite the wrong part of the program, so pin it here.
        const OFFSET: usize = 0x80;
        assert!(EGT80_COM.len() > OFFSET + 8);
        assert_eq!(
            &EGT80_COM[OFFSET..OFFSET + 8],
            b"EGT80CFG",
            "settings signature must sit at file offset 0x80 (record 1)"
        );
    }

    /// The one check that closes the "code change without a version bump" gap
    /// the sibling tests above cannot: an explicit hash of the committed
    /// binary.
    ///
    /// `EGT80.COM` is compiled into every release with `include_bytes!` but no
    /// CI runner can rebuild it — that needs a period Z80 assembler under zxcc
    /// — so the checked-in artifact, not `EGT80.Z80`, is what users actually
    /// run.  Pinning it here means the bytes cannot change without someone
    /// updating this constant in the same commit, which puts the change in
    /// front of a reviewer.  It does *not* prove the binary matches the
    /// source; only `make` in `EGT80/` does that.
    ///
    /// **When you legitimately rebuild EGT80**, run `make` (which gates on
    /// three independent assemblers), then update this hash from:
    ///     sha256sum EGT80/EGT80.COM
    #[test]
    fn test_bundled_egt80_matches_pinned_hash() {
        use sha2::{Digest, Sha256};

        const PINNED: &str =
            "b576eb3ee06eaa94833df35508724611c23800e8f6c8b6291809e83094efe367";

        let actual = format!("{:x}", Sha256::digest(EGT80_COM));
        assert_eq!(
            actual, PINNED,
            "\nEGT80.COM has changed but its pinned hash has not.\n\
             If you rebuilt it on purpose, run `make` in EGT80/ and set\n\
             PINNED in this test to:\n    {}\n",
            actual
        );
    }

    /// Every EGT80 screen has to fit the terminal it is printed on: 24 rows by
    /// 80 columns, the CP/M-era console (ADM-3A, VT100, and what the gateway
    /// renders to).  A screen two lines too tall loses its *heading* off the
    /// top, which is the part naming what you are looking at.
    ///
    /// This is a source-level check on purpose: it parses the `DB` strings out
    /// of `EGT80.Z80`, so it needs no assembler and runs in CI, unlike the
    /// binary itself.  It caught help page 3 having been over the limit since
    /// it was written, and page 2 going over when line settings were described
    /// in it.  Two rows are reserved for the "Press any key" prompt that
    /// follows a full-screen page.
    #[test]
    fn test_egt80_screens_fit_a_24_by_80_terminal() {
        const ROWS: usize = 24;
        const COLS: usize = 80;
        const PROMPT_ROWS: usize = 2; // blank line + "Press any key."

        let src = include_str!("../../EGT80/EGT80.Z80");
        let mut label = String::new();
        let mut text = String::new();
        // Only *printable* blocks are screens.  Every string EGT80 prints ends
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
                "EGT80 screen {label} is {rows} rows; with the key prompt that \
                 scrolls its heading off a {ROWS}-row terminal"
            );
            assert!(
                widest <= COLS - 2,
                "EGT80 screen {label} has a {widest}-column line; it would wrap \
                 on an {COLS}-column terminal"
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
        // PBASE/PKIND defaults compiled into EGT80.  Nothing in either build
        // would notice them drifting apart — the symptom would be a fresh
        // install where the bundled terminal cannot reach the modem, which is
        // exactly the confusion this pairing exists to prevent.
        let src = include_str!("../../EGT80/EGT80.Z80");
        let field = |name: &str| -> String {
            src.lines()
                .find(|l| l.trim_start().starts_with(&format!("{name}:")))
                .unwrap_or_else(|| panic!("EGT80.Z80 should declare {name}"))
                .split_whitespace()
                .nth(2)
                .unwrap_or_else(|| panic!("{name} should have a DB value"))
                .to_string()
        };

        // EGT80's port kind default must be the SIO family (PKSIO = 1).
        assert_eq!(field("PKIND"), "PKSIO", "EGT80 should default to the SIO family");

        // ...and its base address must be the status port DEFAULT_UART resolves
        // to.  EGT80 writes it as a Z80 hex literal, e.g. 82H.
        let base = field("PBASE");
        let base = u8::from_str_radix(base.trim_end_matches('H'), 16)
            .unwrap_or_else(|_| panic!("PBASE should be a hex literal, got {base}"));
        match crate::cpm::resolve_access(crate::cpm::uart::DEFAULT_UART) {
            crate::cpm::ModemAccess::Ports(p) => assert_eq!(
                p.status_port, base,
                "EGT80 defaults to port {base:#04x} but the gateway's default \
                 profile ({}) answers at {:#04x}",
                crate::cpm::uart::DEFAULT_UART, p.status_port
            ),
            other => panic!("the default UART profile should be a port profile, got {other:?}"),
        }
    }

    #[test]
    fn test_bundled_egt80_matches_the_version_in_its_source() {
        // Catches the realistic mistake: bumping the version in EGT80.Z80 and
        // committing without rebuilding EGT80.COM.
        let src = include_str!("../../EGT80/EGT80.Z80");
        let line = src
            .lines()
            .find(|l| l.trim_start().starts_with("MVER:"))
            .expect("EGT80.Z80 should declare its version in MVER");
        let quoted: Vec<&str> = line.split('\'').collect();
        let version = quoted
            .get(1)
            .expect("MVER should contain a quoted version string");
        assert!(
            EGT80_COM
                .windows(version.len())
                .any(|w| w == version.as_bytes()),
            "the built EGT80.COM does not contain the source's version string \
             ({version:?}) — rebuild it with `make -C EGT80`"
        );
    }
}
