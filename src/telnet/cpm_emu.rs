//! CP/M emulator (Flavor B) — a real CP/M 2.2 environment running in an
//! emulated Z80, reachable as its own main-menu item over telnet/SSH.
//!
//! This is a **completely separate** feature from the Gateway Shell
//! (`kernel.rs`, "Flavor A", which is a pure-Rust CP/M-*flavored* file
//! manager with no CPU emulation).  Flavor B runs actual user-supplied
//! `.COM` software in an emulated CP/M 2.2 machine, sandboxed to a `CPM/`
//! directory under `transfer_dir` (one folder per drive A:–P:).  See
//! `kernelplan.md` §13 for the full design and the phased delivery plan
//! (B0 scaffold → B1 CPU/console → B2 CCP-lite → B3 filesystem →
//! B4 run `.COM` → B5 harden).
//!
//! ## Naming
//! Flavor A already owns the `cpm_` identifier prefix; Flavor B uses the
//! `cpmemu_` prefix (and the config key `cpm_emu_enabled`) to keep the two
//! unambiguous.
//!
//! ## Security
//! Gated behind `cpm_emu_enabled` (default-off): when disabled the menu item
//! is hidden and the `K` key is rejected.  Once execution lands (B4) every
//! BDOS file call is jailed under `CPM/` via the existing `transfer_dir`
//! path primitives, and a runaway `.COM` is escapable via an out-of-band
//! `ESC ESC` break-out plus a cycle budget (the ZCOMMAND lesson: never give
//! a peer host-side execution).
//!
//! ## Status: B1 (CPU + console spike)
//! The `iz80` Z80 core is wired to a 64 KB [`Cpm`] machine with our own
//! BDOS console calls (see `src/cpm/`).  Entering the shell runs a small
//! built-in Z80 self-test `.COM` that prints through the emulated BDOS to
//! this telnet/SSH session — proving the CPU + console path end-to-end.
//! Interactive input (BDOS CONIN), an out-of-band `ESC ESC` break-out
//! while a program runs, and loading real user `.COM`s from `CPM/` come in
//! later phases (B2+); a runaway is currently bounded by the instruction
//! budget (see [`Cpm::run`]).
use super::*;
use crate::cpm::{Cpm, Stop};
use std::sync::atomic::AtomicBool;

/// Instructions per [`Cpm::run`] batch before the driver regains control
/// to check the abort flag and yield to the async runtime.
const CPM_RUN_BATCH: u64 = 200_000;

/// Absolute instruction ceiling for the B1 self-test — a final backstop so
/// a wedged demo can never spin forever even with no interactive reader
/// wired yet.  Real long-running programs get a proper (much larger /
/// abort-driven) bound once interactive break-out lands.
const CPM_DEMO_MAX_INSTRUCTIONS: u64 = 5_000_000;

impl TelnetSession {
    /// Flavor-B entry point, invoked from the gated `K` main-menu handler.
    ///
    /// B1: announce the feature, then run a built-in Z80 self-test `.COM`
    /// through the emulated CP/M BDOS, streaming its console output to the
    /// session.  Returns to the main menu when the program warm-boots.
    pub(in crate::telnet) async fn cpm_emulator(&mut self) -> Result<(), std::io::Error> {
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
            self.dim("Flavor B (CP/M 2.2) - self-test:")
        ))
        .await?;
        self.send_line("").await?;
        self.flush().await?;

        self.cpm_run_selftest().await?;

        self.send_line("").await?;
        self.send("  Press any key to continue.").await?;
        self.flush().await?;
        self.wait_for_key().await?;
        Ok(())
    }

    /// Run the built-in self-test program on the emulated Z80 and stream
    /// its BDOS console output to the session.  Kept small and self-
    /// contained so it exercises the full CPU→BDOS→session path without
    /// needing a user-supplied `.COM` or the (later) `CPM/` filesystem.
    async fn cpm_run_selftest(&mut self) -> Result<(), std::io::Error> {
        // Hand-assembled CP/M .COM (loads at 0x0100):
        //   0100: 11 09 01     LD DE,msg (0x0109)
        //   0103: 0E 09        LD C,9        ; BDOS print-string
        //   0105: CD 05 00     CALL 5
        //   0108: C9           RET           ; -> warm-boot vector (0x0000)
        //   0109: msg, "$"-terminated
        let msg = b"iz80 Z80 CPU online.\r\nCP/M 2.2 BDOS console OK.\r\n$";
        let mut prog: Vec<u8> = vec![
            0x11, 0x09, 0x01, // LD DE,0x0109
            0x0E, 0x09, // LD C,9
            0xCD, 0x05, 0x00, // CALL 5
            0xC9, // RET
        ];
        prog.extend_from_slice(msg);

        let mut cpm = Cpm::new();
        cpm.load_com(&prog);
        // No interactive reader is wired yet (B2), so the abort flag stays
        // clear here; the instruction budget + ceiling bound a runaway.
        let abort = AtomicBool::new(false);
        let mut output: Vec<u8> = Vec::new();

        loop {
            match cpm.run(CPM_RUN_BATCH, &abort) {
                Stop::Bdos(func) => match func {
                    2 => {
                        // Console output: single char in E.
                        output.push(cpm.arg_e());
                        cpm.bdos_return(0);
                    }
                    9 => {
                        // Print $-terminated string at DE.
                        let de = cpm.arg_de();
                        output.extend(cpm.read_dollar_string(de, 8192));
                        cpm.bdos_return(0);
                    }
                    _ => {
                        // Unimplemented BDOS call: return 0 and continue
                        // (full BDOS coverage arrives in B2/B3).
                        cpm.bdos_return(0);
                    }
                },
                Stop::WarmBoot | Stop::Aborted => break,
                Stop::BudgetExhausted => {
                    if cpm.instructions() >= CPM_DEMO_MAX_INSTRUCTIONS {
                        self.send_line(&format!(
                            "  {}",
                            self.red("[self-test exceeded budget]")
                        ))
                        .await?;
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            }
        }

        // Stream the collected console output.  CP/M emits raw bytes with
        // CRLF line endings; render as text for now (proper ADM-3A/VT100↔
        // PETSCII terminal translation is a later, B4, task).
        let text = String::from_utf8_lossy(&output);
        for line in text.split_terminator("\r\n") {
            self.send_line(&format!("  {}", self.green(line))).await?;
        }
        self.flush().await?;
        Ok(())
    }
}
