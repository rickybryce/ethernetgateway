//! CP/M emulator (Flavor B) — a real CP/M 2.2 environment running in an
//! emulated Z80, reachable as its own main-menu item over telnet/SSH.
//!
//! This is a **completely separate** feature from the Gateway Shell
//! (`kernel.rs`, "Flavor A", a pure-Rust CP/M-*flavored* file manager with
//! no CPU emulation).  Flavor B runs actual user-supplied `.COM` software
//! in an emulated CP/M 2.2 machine, sandboxed to a `CPM/` directory under
//! `transfer_dir` (one folder per drive A:–H:).  See `kernelplan.md` §13
//! for the full design and the phased plan.
//!
//! ## Naming
//! Flavor A owns the `cpm_` identifier prefix; Flavor B uses `cpmemu_` /
//! the `cpm_emu_*` names (and the config key `cpm_emu_enabled`) to keep the
//! two unambiguous.
//!
//! ## Security (finalized B5)
//! The feature runs arbitrary Z80 code, so it is gated behind
//! `cpm_emu_enabled` (default-off): when disabled the menu item is hidden
//! and `K` is rejected.  The trusted-LAN posture is bounded on three axes:
//! - **Jail.** Every BDOS file call resolves through `CpmFs` under the
//!   `CPM/` container in `transfer_dir`: 8.3-name validation (no separators
//!   or `..`), a lexical `starts_with` check, and a canonical-path +
//!   symlink check (a symlink planted in a drive folder can't point out).
//!   Drive indices are clamped to A:–H:.
//! - **CPU.** A runaway is bounded by the configurable instruction ceiling
//!   (`cpm_emu_max_minstr`); the run loop yields every batch, and
//!   interactive programs are additionally escapable with a double-`ESC` at
//!   any console-input prompt.
//! - **Memory/disk.** Each session's machine is a fixed 64 KB (bounded by
//!   `max_sessions`); a single emulated file is capped at 8 MB
//!   (`MAX_CPM_FILE_BYTES`) so a high random-record write can't spray a
//!   multi-gigabyte sparse file.  All BDOS read helpers are length-bounded.
//!
//! The emulator services only BDOS — it has no path to execute host
//! commands, and outbound I/O (a virtual modem) is deliberately absent
//! (deferred B6+).  There is no per-drive file-*count* cap (a guest can
//! create many small files); bounded by host disk and acceptable under the
//! trusted-LAN model.
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
//! cursor keys back to ADM-3A codes (see `cpm_term`).  Still to come (B6+):
//! a concurrent out-of-band break-out reader and the virtual modem.
use super::*;
use super::cpm_term::{self, Adm3a};
use crate::cpm::{parse_afn, parse_command_fcb, split_8_3, Cpm, CpmFs, Fcb, Stop, FCB_SIZE};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

/// Instructions per [`Cpm::run`] batch before the driver regains control to
/// yield to the async runtime.
const CPM_RUN_BATCH: u64 = 200_000;

/// Highest emulated drive letter (A:–H:, 8 drives).
const CPM_LAST_DRIVE: u8 = b'H';

/// Outcome of a single console-input read while a program runs.
enum ConIn {
    /// A translated data byte to hand to the guest.
    Byte(u8),
    /// The user pressed `ESC` twice — abort the program back to `A>`.
    BreakOut,
    /// The session closed (or idled out) — leave the emulator entirely.
    Disconnect,
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
    /// Flavor-B entry point, invoked from the gated `K` main-menu handler.
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
        self.send_line(&format!(
            "  {}",
            self.amber("WARNING: runs arbitrary Z80 code.")
        ))
        .await?;
        self.send_line(&format!(
            "  {}",
            self.dim("CP/M 2.2 (iz80). Type HELP; EXIT to")
        ))
        .await?;
        self.send_line(&format!("  {}", self.dim("leave."))).await?;
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

    /// Ensure `CPM/` and each drive folder `CPM/A`..`CPM/H` exist under
    /// `transfer_dir`, creating any that are missing.  Idempotent and run
    /// on every launch, so a program can select any of the 8 drives without
    /// hitting a "drive does not exist" error.  Jailed by construction —
    /// the paths are built under the configured `transfer_dir`.
    async fn cpmemu_ensure_drives(&mut self) -> Result<(), std::io::Error> {
        let cfg = config::get_config();
        for drive in b'A'..=CPM_LAST_DRIVE {
            let mut p = PathBuf::from(&cfg.transfer_dir);
            p.push("CPM");
            p.push((drive as char).to_string());
            tokio::fs::create_dir_all(&p).await?;
        }
        Ok(())
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
        // Wire the virtual-modem UART (if the operator selected one) so a CP/M
        // comms program finds its modem at the configured machine ports.
        cpm.set_uart(crate::cpm::resolve_uart(&config::get_config().cpm_emu_uart));
        loop {
            let prompt = self.cyan(&format!("{}>", fs.current_drive_letter()));
            self.send(&prompt).await?;
            self.flush().await?;

            let line = match self.get_line_input().await? {
                Some(s) => s,
                None => return Ok(()), // disconnected
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
                if (b'A'..=b'H').contains(&d) {
                    fs.select(d - b'A');
                } else {
                    self.send_line(&format!("  {}?", self.red(&verb))).await?;
                }
                continue;
            }

            match verb.as_str() {
                "EXIT" | "BYE" | "QUIT" => return Ok(()),
                "HELP" | "?" => self.cpmemu_help().await?,
                "VER" | "VERSION" => {
                    self.send_line(&format!(
                        "  {}",
                        self.green("CP/M 2.2 emulator (iz80 Z80 core)")
                    ))
                    .await?;
                }
                "DIR" => self.cpmemu_dir(fs).await?,
                "ERA" | "DEL" => self.cpmemu_era(fs, trimmed).await?,
                "REN" | "RENAME" => self.cpmemu_ren(fs, trimmed).await?,
                "TYPE" => self.cpmemu_type(fs, trimmed).await?,
                "SAVE" => self.cpmemu_save(&mut cpm, fs, trimmed).await?,
                "USER" => self.cpmemu_user(trimmed).await?,
                "HELLO" => {
                    // Non-interactive BDOS print-string demo.
                    if !self.cpmemu_run_program(&mut cpm, &Self::cpmemu_demo_hello(), "", fs).await? {
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
                    if !self.cpmemu_run_program(&mut cpm, &Self::cpmemu_demo_echo(), "", fs).await? {
                        return Ok(());
                    }
                }
                other => {
                    // Not a built-in: try to load and run `<verb>.COM` from
                    // the drive.  `None` = no such file (CP/M prints VERB?).
                    match self.cpmemu_try_run_com(&mut cpm, fs, &verb, trimmed).await? {
                        Some(true) => {}                    // ran; back to A>
                        Some(false) => return Ok(()),       // session gone
                        None => {
                            self.send_line(&format!("  {}?", self.red(other))).await?;
                        }
                    }
                }
            }
        }
    }

    /// Built-in `DIR`: list the files on the current drive, four per row
    /// (CP/M's `DIR` is a CCP built-in, not a `.COM`).  Prints `No file`
    /// when the drive is empty, as CP/M does.
    async fn cpmemu_dir(&mut self, fs: &CpmFs) -> Result<(), std::io::Error> {
        let names = fs.list_current();
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
        if fs.delete(&fcb) == 0 {
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
        // Distinguish the two refusal cases for a helpful message.
        if fs.open_existing(&Self::cpmemu_fcb(&nn, &ne)).is_some() {
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
        self.cpmemu_write_text(shown).await?;
        self.send_line("").await?;
        if truncated {
            self.send_line(&format!("  {}", self.dim("[truncated]"))).await?;
        }
        Ok(())
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

    /// Built-in `USER` (CP/M resident): select a user area 0–15.  The
    /// emulator models each drive as a single flat area, so only area 0
    /// exists; a valid `USER 0` is accepted silently and any other valid
    /// number reports the single-area limitation rather than silently
    /// hiding files.  Recognized (not passed through to a `.COM`).
    async fn cpmemu_user(&mut self, line: &str) -> Result<(), std::io::Error> {
        match line.split_whitespace().nth(1).and_then(|s| s.parse::<u8>().ok()) {
            Some(0) => {}
            Some(n) if n <= 15 => {
                self.send_line("  Only user area 0 (single flat area).").await?;
            }
            _ => {
                self.send_line("  USER 0..15").await?;
            }
        }
        Ok(())
    }

    /// One-screen help for the CCP-lite built-ins.
    async fn cpmemu_help(&mut self) -> Result<(), std::io::Error> {
        for line in [
            "  Built-in commands:",
            "  DIR        list files on this drive",
            "  ERA name   erase file(s) (wildcards)",
            "  REN new=old  rename a file",
            "  TYPE file  show a text file",
            "  SAVE n file  save n TPA pages",
            "  USER n     select user area (0)",
            "  A: .. H:   change drive",
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
        program: &[u8],
        tail: &str,
        fs: &mut CpmFs,
    ) -> Result<bool, std::io::Error> {
        cpm.load_com(program);
        // Lay down page zero (command tail + default FCBs) so a real `.COM`
        // finds its arguments where CP/M puts them.  Built-in demos pass an
        // empty tail.
        cpm.setup_command_line(tail);
        // Runaway ceiling for this run, from config (millions of Z80
        // instructions).  No concurrent wire-reader yet (deferred B6+); the
        // abort flag stays clear and this ceiling is the compute-bound
        // runaway guard, while double-ESC at an input prompt handles
        // interactive break-out.
        let max_instructions =
            config::get_config().cpm_emu_max_minstr as u64 * 1_000_000;
        let abort = AtomicBool::new(false);
        let mut last_esc = false;
        // ADM-3A output decoder: the guest is told it's driving an ADM-3A,
        // and its control codes are translated to the connected terminal.
        // State persists across BDOS calls (a cursor-address sequence can
        // straddle them).
        let mut term = Adm3a::default();

        loop {
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
                            match self.cpmemu_conin(&mut last_esc).await? {
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
                        6 => {
                            // Direct console I/O: E=0xFF read (no echo),
                            // E=0xFE status, else output E.
                            let e = cpm.arg_e();
                            match e {
                                0xFF => match self.cpmemu_conin(&mut last_esc).await? {
                                    ConIn::Byte(b) => cpm.bdos_return(b),
                                    ConIn::BreakOut => {
                                        self.cpmemu_break_notice().await?;
                                        return Ok(true);
                                    }
                                    ConIn::Disconnect => return Ok(false),
                                },
                                0xFE => cpm.bdos_return(0), // no buffered char
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
                            // Read console buffer (line) into memory at DE.
                            match self.get_line_input().await? {
                                Some(line) => {
                                    let bytes: Vec<u8> = line.bytes().collect();
                                    let de = cpm.arg_de();
                                    cpm.bdos_read_buffer(de, &bytes);
                                    cpm.bdos_return(0);
                                }
                                None => return Ok(false),
                            }
                        }
                        11 => cpm.bdos_return(0), // console status: none ready
                        12 => cpm.bdos_return(0x22), // version: CP/M 2.2
                        _ => {
                            // Disk-system / FCB file BDOS calls (drive
                            // select, DMA, open/read/write/search/delete/
                            // rename/size) need no session I/O, so the core
                            // services them directly.  Truly-unknown funcs
                            // return 0.
                            let code = crate::cpm::service_disk_bdos(cpm, fs, func)
                                .unwrap_or(0);
                            cpm.bdos_return(code);
                        }
                    }
                }
                Stop::WarmBoot | Stop::Aborted => return Ok(true),
                Stop::BudgetExhausted => {}
            }
            // Cooperative yield every batch so a BDOS-frequent loop whose
            // handlers never .await (console status/version/set-DMA/etc.)
            // can't pin the tokio worker.  Interactive handlers already
            // await; this makes the non-awaiting ones cooperative too.
            tokio::task::yield_now().await;
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
        let cont = self.cpmemu_run_program(cpm, &bytes, tail, fs).await?;
        Ok(Some(cont))
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
    async fn cpmemu_conin(&mut self, last_esc: &mut bool) -> Result<ConIn, std::io::Error> {
        let is_petscii = self.terminal_type == TerminalType::Petscii;
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

    /// Write literal text (a `TYPE`d file, not program output) to the
    /// session: on a C64 the ASCII text is case-swapped + Latin-1 encoded via
    /// the normal `send` path so it renders correctly; elsewhere the bytes go
    /// out raw.  Unlike [`Self::cpmemu_emit`], this does NOT run the ADM-3A
    /// decoder — the bytes are file content, not a program's control stream.
    async fn cpmemu_write_text(&mut self, bytes: &[u8]) -> Result<(), std::io::Error> {
        if self.terminal_type == TerminalType::Petscii {
            let s = String::from_utf8_lossy(bytes);
            self.send(&s).await?;
        } else {
            self.send_raw(bytes).await?;
        }
        self.flush().await
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
