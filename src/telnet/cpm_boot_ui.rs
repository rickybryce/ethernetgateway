//! Running a booted disk image for a telnet or SSH session.
//!
//! The other emulator path — `cpm_emu` — traps BDOS calls and services them
//! against a filesystem we control. This one traps nothing. It boots the disk
//! and gets out of the way, and the disk's own operating system does the rest.
//! That is what makes it able to run Altair DOS, Disk BASIC and Time Sharing
//! BASIC, none of which are CP/M and none of which the filesystem path can
//! reach.
//!
//! The bounds are different in kind, so they are stated rather than inherited:
//!
//! * **One session per image.** A booted guest owns whole drives and writes
//!   raw sectors, so the per-file claim that keeps two CP/M sessions from
//!   interleaving records has nothing to grip. A second session is refused.
//! * **Read-only unless asked.** The guest can write anywhere inside an image,
//!   and nothing above it interprets the format well enough to notice a
//!   mistake. Protection is the default; writing is a decision.
//! * **The instruction ceiling still applies.** A guest that never yields is
//!   the same hazard here as there, and `cpm_emu_max_minstr` is the same
//!   answer.

use super::*;
use crate::cpm::boot_machine::BootMachine;
use iz80::Cpu;
use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

/// Images currently booted, by path, so one is never run twice at once.
fn booted() -> &'static Mutex<HashSet<std::path::PathBuf>> {
    static B: OnceLock<Mutex<HashSet<std::path::PathBuf>>> = OnceLock::new();
    B.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Claims an image for one session and releases it however the session ends.
///
/// RAII rather than a matched pair of calls: a boot can leave through an error,
/// a dropped connection or the shutdown broadcast, and an image left claimed
/// after any of those could never be booted again without a restart.
struct BootClaim(std::path::PathBuf);

impl Drop for BootClaim {
    fn drop(&mut self) {
        booted().lock().unwrap_or_else(|e| e.into_inner()).remove(&self.0);
    }
}

impl BootClaim {
    fn take(path: &std::path::Path) -> Option<BootClaim> {
        let mut held = booted().lock().unwrap_or_else(|e| e.into_inner());
        if held.contains(path) {
            return None;
        }
        held.insert(path.to_path_buf());
        Some(BootClaim(path.to_path_buf()))
    }
}

/// How often to look for a keystroke, in instructions.
///
/// The guest polls its console far more often than a person types, so checking
/// every instruction would spend the whole budget on the input path. This is
/// frequent enough that typing feels immediate and rare enough to be free.
const KEY_POLL_INTERVAL: u64 = 20_000;

/// Instructions between yields to the runtime.
///
/// Without this the emulator loop starves every other task on the thread —
/// the same lesson the CP/M emulator learned when an idle guest spun the host
/// at 161% CPU.
const YIELD_INTERVAL: u64 = 200_000;

impl TelnetSession {
    /// Boot an image and run it until the guest stops or the user leaves.
    ///
    /// `image` is the host path; `writable` decides whether changes are kept.
    pub(in crate::telnet) async fn cpm_boot_session(
        &mut self,
        image: &std::path::Path,
        writable: bool,
    ) -> Result<(), std::io::Error> {
        let Some(_claim) = BootClaim::take(image) else {
            self.send_line(&format!(
                "  {}",
                self.red("That image is already running in another session.")
            ))
            .await?;
            self.send_line(&format!(
                "  {}",
                self.dim("A booted disk owns its drives, so only one session")
            ))
            .await?;
            self.send_line(&format!("  {}", self.dim("can have it at a time."))).await?;
            self.send_line("").await?;
            return Ok(());
        };

        let bytes = match tokio::fs::read(image).await {
            Ok(b) => b,
            Err(e) => {
                self.send_line(&format!("  {}", self.red(&format!("Cannot read image: {e}"))))
                    .await?;
                return Ok(());
            }
        };

        let mut machine = BootMachine::new();
        if let Err(e) = machine.insert(0, bytes, !writable) {
            self.send_line(&format!("  {}", self.red(&e))).await?;
            return Ok(());
        }
        let mut cpu = Cpu::new_8080();
        if let Err(e) = machine.boot(&mut cpu, 0) {
            self.send_line(&format!("  {}", self.red(&e.to_string()))).await?;
            self.send_line("").await?;
            return Ok(());
        }

        let name = image.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        self.send_line("").await?;
        self.send_line(&format!("  {} {}", self.green("Booted"), self.amber(&name)))
            .await?;
        self.send_line(&format!(
            "  {}",
            self.dim(if writable { "Changes are saved." } else { "Read-only." })
        ))
        .await?;
        self.send_line(&format!("  {}", self.dim("Press ESC twice to stop."))).await?;
        self.send_line("").await?;
        self.flush().await?;

        let result = self.cpm_boot_run(&mut cpu, &mut machine).await;

        // Save whatever the guest changed, whatever ended the session — a user
        // who pressed ESC still wants their work.
        if writable {
            for (_, bytes) in machine.take_dirty() {
                if let Err(e) = tokio::fs::write(image, &bytes).await {
                    glog!("CP/M boot: could not save {}: {}", name, e);
                }
            }
        }
        self.send_line("").await?;
        self.send_line(&format!("  {}", self.dim("Returned to the gateway."))).await?;
        self.send_line("").await?;
        result
    }

    /// The run loop: step the CPU, move console bytes both ways.
    async fn cpm_boot_run(
        &mut self,
        cpu: &mut Cpu,
        machine: &mut BootMachine,
    ) -> Result<(), std::io::Error> {
        let ceiling = config::get_config().cpm_emu_max_minstr as u64 * 1_000_000;
        let mut executed: u64 = 0;
        let mut esc_run = 0u8;

        loop {
            cpu.execute_instruction(machine);
            executed += 1;

            let out = machine.take_output();
            if !out.is_empty() {
                // The guest is driving a bare serial console, so its bytes go
                // out as they are — no ADM-3A translation, because a booted OS
                // brings whatever terminal handling it has of its own.
                self.send_raw(&out).await?;
                self.flush().await?;
            }

            if executed.is_multiple_of(KEY_POLL_INTERVAL) {
                if let Some(b) = self.cpm_boot_poll_key().await? {
                    // Two ESCs in a row leave, the same gesture the other
                    // emulator uses. A single ESC is passed through, because
                    // plenty of guest software wants it.
                    if b == 0x1B {
                        esc_run += 1;
                        if esc_run >= 2 {
                            return Ok(());
                        }
                    } else {
                        esc_run = 0;
                    }
                    machine.send_key(b);
                }
            }

            if executed.is_multiple_of(YIELD_INTERVAL) {
                tokio::task::yield_now().await;
                if executed >= ceiling {
                    self.send_line("").await?;
                    self.send_line(&format!(
                        "  {}",
                        self.red("Stopped: the guest ran past the instruction ceiling.")
                    ))
                    .await?;
                    self.send_line(&format!(
                        "  {}",
                        self.dim("Raise cpm_emu_max_minstr if it needed longer.")
                    ))
                    .await?;
                    return Ok(());
                }
                // A guest waiting on a disk that is not turning is a bug in
                // our controller, not in the disk — say so rather than let it
                // look like a runaway program, which is a mistake this project
                // has already made once.
                if machine.stuck_polls() > 1_000_000 {
                    self.send_line("").await?;
                    self.send_line(&format!(
                        "  {}",
                        self.red("Stopped: the guest is waiting for a sector that never arrives.")
                    ))
                    .await?;
                    glog!("CP/M boot: controller stalled — the disk is not advancing");
                    return Ok(());
                }
            }
        }
    }

    /// A keystroke if one is waiting, without blocking the guest.
    async fn cpm_boot_poll_key(&mut self) -> Result<Option<u8>, std::io::Error> {
        match tokio::time::timeout(
            std::time::Duration::from_millis(1),
            self.session_read_byte(),
        )
        .await
        {
            Ok(Ok(b)) => Ok(b),
            Ok(Err(e)) => Err(e),
            Err(_) => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One session per image, and the claim comes back however the session
    /// ends — a claim leaked by an error path could never be booted again
    /// without restarting the gateway.
    #[test]
    fn test_an_image_can_only_be_booted_once_at_a_time() {
        let p = std::path::Path::new("/tmp/egw_boot_claim_test.dsk");
        let first = BootClaim::take(p).expect("first claim");
        assert!(BootClaim::take(p).is_none(), "a second session must be refused");
        drop(first);
        assert!(BootClaim::take(p).is_some(), "the claim returns when the session ends");
    }

    /// Different images do not block each other.
    #[test]
    fn test_two_different_images_can_run_together() {
        let a = BootClaim::take(std::path::Path::new("/tmp/egw_boot_a.dsk")).unwrap();
        let b = BootClaim::take(std::path::Path::new("/tmp/egw_boot_b.dsk"));
        assert!(b.is_some(), "separate images are independent");
        drop(a);
    }

    /// The poll interval must divide the yield interval, or the key check and
    /// the yield drift apart and one of them effectively stops happening.
    #[test]
    fn test_the_loop_intervals_line_up() {
        assert!(
            YIELD_INTERVAL.is_multiple_of(KEY_POLL_INTERVAL),
            "the yield must fall on a key-poll boundary, or the two drift apart \
             and one of them effectively stops happening"
        );
    }
}
