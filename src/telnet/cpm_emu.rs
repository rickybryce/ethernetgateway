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
//! ## Security
//! Gated behind `cpm_emu_enabled` (default-off): when disabled the menu
//! item is hidden and `K` is rejected.  A runaway program is bounded by an
//! instruction ceiling, and interactive programs can be broken out of with
//! a double-`ESC` at any console-input prompt.  Every future BDOS file call
//! (B3) resolves under `CPM/` via the existing `transfer_dir` jail.
//!
//! ## Status: B4a (run a real `.COM` from a drive)
//! Entering the shell drops into our Rust CCP-lite `A>` prompt.  The full
//! console BDOS group (1/2/6/9/10/11/12) plus the disk/FCB group (B3) are
//! wired, so a verb that isn't a built-in is looked up as `<verb>.COM` on
//! the drive, loaded into the TPA with page zero set up (command tail +
//! default FCBs), and run — actual CP/M software (PIP, STAT, ASM, …) runs
//! over telnet/SSH.  Still to come (B4b+): richer terminal translation
//! (ADM-3A/VT52/VT100) and a concurrent out-of-band break-out reader.
use super::*;
use crate::cpm::{parse_afn, parse_command_fcb, Cpm, CpmFs, Fcb, Stop, FCB_SIZE};
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
                "HELLO" => {
                    // Non-interactive BDOS print-string demo.
                    if !self.cpmemu_run_program(&Self::cpmemu_demo_hello(), "", fs).await? {
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
                    if !self.cpmemu_run_program(&Self::cpmemu_demo_echo(), "", fs).await? {
                        return Ok(());
                    }
                }
                other => {
                    // Not a built-in: try to load and run `<verb>.COM` from
                    // the drive.  `None` = no such file (CP/M prints VERB?).
                    match self.cpmemu_try_run_com(fs, &verb, trimmed).await? {
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

    /// One-screen help for the CCP-lite built-ins.
    async fn cpmemu_help(&mut self) -> Result<(), std::io::Error> {
        for line in [
            "  Built-in commands:",
            "  HELP / ?   this help",
            "  VER        emulator version",
            "  DIR        list files on this drive",
            "  ERA name   erase file(s) (wildcards)",
            "  A: .. H:   change drive",
            "  HELLO      BDOS print-string demo",
            "  ECHO       interactive console demo",
            "  name       run name.COM from the drive",
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
        program: &[u8],
        tail: &str,
        fs: &mut CpmFs,
    ) -> Result<bool, std::io::Error> {
        let mut cpm = Cpm::new();
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
                                    self.cpmemu_conout(&[b]).await?;
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
                            self.cpmemu_conout(&[cpm.arg_e()]).await?;
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
                                    self.cpmemu_conout(&[e]).await?;
                                    cpm.bdos_return(0);
                                }
                            }
                        }
                        9 => {
                            // Print $-terminated string at DE.
                            let de = cpm.arg_de();
                            let s = cpm.read_dollar_string(de, 8192);
                            self.cpmemu_conout(&s).await?;
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
                            let code = crate::cpm::service_disk_bdos(&mut cpm, fs, func)
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
        let cont = self.cpmemu_run_program(&bytes, tail, fs).await?;
        Ok(Some(cont))
    }

    /// Read one console byte for a running program, translating for the
    /// terminal and detecting the double-`ESC` break-out.  A single `ESC`
    /// is delivered to the guest (CP/M editors use it); a second `ESC`
    /// immediately after aborts.
    async fn cpmemu_conin(&mut self, last_esc: &mut bool) -> Result<ConIn, std::io::Error> {
        match self.read_byte_filtered().await {
            Ok(Some(b)) => {
                let is_petscii = self.terminal_type == TerminalType::Petscii;
                if is_esc_key(b, is_petscii) {
                    if *last_esc {
                        *last_esc = false;
                        return Ok(ConIn::BreakOut);
                    }
                    *last_esc = true;
                    return Ok(ConIn::Byte(0x1B)); // deliver first ESC as ASCII
                }
                *last_esc = false;
                let b = if is_petscii { petscii_to_ascii_byte(b) } else { b };
                Ok(ConIn::Byte(b))
            }
            Ok(None) => Ok(ConIn::Disconnect),
            // An idle timeout ends the program (and the session).
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => Ok(ConIn::Disconnect),
            Err(e) => Err(e),
        }
    }

    /// Write console-output bytes from a guest to the session.  On a
    /// PETSCII (C64) terminal the guest's ASCII output is routed through the
    /// gateway's normal text path (`send`, which case-swaps + Latin-1
    /// encodes) so plain text renders correctly instead of showing lowercase
    /// as graphics; on ANSI/ASCII the exact bytes go out (IAC-escaped for
    /// telnet).  Full ADM-3A/VT52/VT100 terminal-emulation translation is a
    /// later (B4) task; this makes ordinary text-mode output correct for a
    /// C64 today.
    async fn cpmemu_conout(&mut self, bytes: &[u8]) -> Result<(), std::io::Error> {
        if self.terminal_type == TerminalType::Petscii {
            let s = String::from_utf8_lossy(bytes);
            self.send(&s).await?;
        } else {
            self.send_raw(bytes).await?;
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
