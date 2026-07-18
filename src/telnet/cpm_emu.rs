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
//! ## Status: B0 (scaffold)
//! This file is currently a placeholder session that only announces the
//! feature and returns.  The emulator core (iz80 CPU dependency + our own
//! CP/M 2.2 BDOS/BIOS) arrives in later phases.
use super::*;

impl TelnetSession {
    /// Flavor-B entry point, invoked from the gated `K` main-menu handler.
    ///
    /// B0 scaffold: display the feature banner + "runs arbitrary Z80 code"
    /// caution + a "not yet implemented" notice, then wait for a key and
    /// return to the main menu.  Later phases replace the body with the
    /// real emulator REPL (`A>` prompt → run `.COM` → warm-boot).
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
            self.dim("Real CP/M 2.2 in an emulated Z80.")
        ))
        .await?;
        self.send_line(&format!(
            "  {}",
            self.dim("Runs your own .COM software,")
        ))
        .await?;
        self.send_line(&format!(
            "  {}",
            self.dim("sandboxed to the CPM/ folder.")
        ))
        .await?;
        self.send_line("").await?;

        self.send_line(&format!(
            "  {}",
            self.amber("WARNING: runs arbitrary Z80 code.")
        ))
        .await?;
        self.send_line("").await?;

        self.send_line(&format!(
            "  {}",
            self.red("Not yet implemented.")
        ))
        .await?;
        self.send_line(&format!(
            "  {}",
            self.dim("Flavor B is under construction.")
        ))
        .await?;
        self.send_line("").await?;

        self.send("  Press any key to continue.").await?;
        self.flush().await?;
        self.wait_for_key().await?;
        Ok(())
    }
}
