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
//! * **No instruction ceiling.** `cpm_emu_max_minstr` bounds one transient
//!   program in the emulator and hands the user back their `A>`. A booted
//!   operating system *is* the session, and running indefinitely at its own
//!   prompt is what it is supposed to do — at the default ceiling every booted
//!   disk would have stopped after about forty seconds. What needs bounding is
//!   a user who has walked away, so the session idle timeout does that instead.
//! * **The modem comes along, if it can.** A profile that is a pair of ports
//!   is wired up — `altair_2sio2` is where a real Altair put one. `AUX:` and
//!   HBIOS cannot: they are our BDOS and RomWBW's firmware, and this guest has
//!   its own of both.

use super::*;
use crate::cpm::boot_machine::{BootMachine, ModemAttach};
use crate::telnet::cpm_emu::{cpm_peer_register, idle_nap, poll_once};
use crate::telnet::cpm_modem::CpmModem;
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

        // The virtual modem comes along, when the operator's profile is one a
        // booted machine can have.  A real Altair put its modem on the second
        // port of the 88-2SIO, which is exactly the `altair_2sio2` profile — so
        // comms software running under a booted Altair CP/M finds a UART where
        // it expects one and dials out through us.  `AUX:` and HBIOS cannot
        // come: they are our own BDOS device and RomWBW's firmware, and this
        // guest brings its own of both.
        let access = crate::cpm::resolve_access(&config::get_config().cpm_emu_uart);
        let attach = machine.attach_modem(access);
        let mut modem = CpmModem::new(matches!(attach, ModemAttach::Ports(_, _)));
        modem.set_menu_context(self.shutdown.clone(), self.restart.clone(), self.lockouts.clone());
        // Joins the inbound `CPM@<ip>` pool for as long as the boot lasts, so a
        // booted guest is dialable exactly as an emulator session is.
        let _peer_reg = cpm_peer_register(modem.enabled());

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
        match &attach {
            ModemAttach::Ports(status, data) => {
                self.send_line(&format!(
                    "  {}",
                    self.dim(&format!("Modem on ports {status:#04x}/{data:#04x}."))
                ))
                .await?;
            }
            // Said rather than logged: an operator who set a modem up and finds
            // it missing needs the reason at the moment they notice.
            ModemAttach::Unavailable(why) => {
                self.send_line(&format!("  {}", self.amber("No modem here:"))).await?;
                self.send_line(&format!("  {}", self.amber(why))).await?;
            }
            ModemAttach::Off => {}
        }
        self.send_line(&format!("  {}", self.dim("Press ESC twice to stop."))).await?;
        self.send_line("").await?;
        self.flush().await?;

        let result = self.cpm_boot_run(&mut cpu, &mut machine, &mut modem).await;

        // Save whatever the guest changed, whatever ended the session — a user
        // who pressed ESC still wants their work.
        //
        // Only drive 0, explicitly.  One image is inserted and it goes in drive
        // 0, so today every dirty entry is that one; writing them all to `image`
        // regardless of which drive they came from would quietly become a
        // corrupting bug the moment a second drive is added, and that is exactly
        // the kind of change nobody would think to re-check this loop for.
        if writable {
            for (drive, bytes) in machine.take_dirty() {
                if drive != 0 {
                    glog!("CP/M boot: drive {} was written but has no file — not saved", drive);
                    continue;
                }
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
        modem: &mut CpmModem,
    ) -> Result<(), std::io::Error> {
        let mut executed: u64 = 0;
        let mut esc_run = 0u8;
        let is_petscii = self.terminal_type == TerminalType::Petscii;
        // Consecutive key-poll seams at which the guest did nothing we could
        // see, and the activity marks used to decide that.
        let mut idle_seams: u32 = 0;
        let mut disk_before = machine.disk_accesses();
        // Set when the guest printed since the last key-poll seam.  Checked
        // there rather than where it is set, because output arrives one byte at
        // a time and pacing is decided once per seam.
        let mut printed = false;
        // When the user was last heard from.  A booted operating system has no
        // natural end — it sits at its prompt for ever, which is correct — so
        // the bound on an abandoned session is the operator's idle timeout,
        // exactly as it is for a program parked on a blocking modem read.
        let mut last_key = tokio::time::Instant::now();

        loop {
            cpu.execute_instruction(machine);
            executed += 1;

            if executed.is_multiple_of(KEY_POLL_INTERVAL) {
                // Everything the guest printed since the last seam, in one
                // write.  Draining per instruction instead would be a syscall
                // per character — a guest printing a directory listing would
                // make two thousand of them — and a seam is a fifth of a
                // millisecond, so nothing a person could perceive is lost.
                // It comes first in the seam so that output is always on its
                // way out before the idle nap below.
                let out = machine.take_output();
                if !out.is_empty() {
                    // The guest is driving a bare serial console, so its
                    // control codes go out as they are — a booted OS brings
                    // whatever terminal handling it has of its own, and
                    // second-guessing it would break the software that gets it
                    // right.
                    //
                    // A Commodore is the exception, and not a cosmetic one:
                    // PETSCII swaps the two cases, so an untranslated banner
                    // arrives as graphics characters. Folding the letters is
                    // the least we can do and leaves everything else untouched.
                    if is_petscii {
                        let folded: Vec<u8> =
                            out.iter().map(|&b| ascii_to_petscii_byte(b)).collect();
                        self.send_raw(&folded).await?;
                    } else {
                        self.send_raw(&out).await?;
                    }
                    self.flush().await?;
                    printed = true;
                }

                let mut keys = 0usize;
                // Drain everything waiting rather than one byte per seam, so a
                // pasted command or a file being sent into the guest's console
                // moves at the wire's pace instead of one byte per 20,000
                // instructions.  Bounded so a flood cannot hold the loop here.
                while keys < 256 {
                    let Some(read) = poll_once(self.session_read_byte()) else {
                        break; // nothing waiting right now
                    };
                    let Some(b) = read? else {
                        return Ok(()); // disconnected
                    };
                    keys += 1;
                    // Two ESCs in a row leave, the same gesture the other
                    // emulator uses. A single ESC is passed through, because
                    // plenty of guest software wants it.  `is_esc_key` rather
                    // than a bare 0x1B, so a Commodore's own escape gets a user
                    // out too.
                    if is_esc_key(b, is_petscii) {
                        esc_run += 1;
                        if esc_run >= 2 {
                            return Ok(());
                        }
                    } else {
                        esc_run = 0;
                    }
                    // The guest is an ASCII machine, so a Commodore's keys are
                    // folded on the way in as its output is folded on the way
                    // out.
                    machine.send_key(if is_petscii { petscii_to_ascii_byte(b) } else { b });
                }
                if keys > 0 {
                    last_key = tokio::time::Instant::now();
                }

                // Service the modem at the same seam: this is where the guest's
                // synchronous UART rings cross into async I/O.
                let mut modem_moved = false;
                if modem.enabled() {
                    // Pick up an inbound `CPM@<ip>` call when idle, so the guest
                    // can answer one exactly as an emulator session can.
                    if modem.can_answer() {
                        if let Some(call) = crate::serial::take_cpm_call_request() {
                            modem.accept_incoming(call);
                        }
                    }
                    let tx = machine.modem().drain_tx();
                    let guest_has_rx = machine.modem().rx_len() > 0;
                    let free = machine.modem().rx_free();
                    modem_moved = !tx.is_empty();
                    let rx = modem.service(tx, free, guest_has_rx).await;
                    if !rx.is_empty() {
                        machine.modem().queue_rx(&rx);
                        modem_moved = true;
                    }
                    // Reflect carrier (DCD) into the status the guest polls.
                    machine.modem().set_carrier(modem.carrier_asserted());
                }

                // Pacing.  A guest sitting at its prompt polls the console
                // status register as fast as we will let it, and without this
                // an idle booted session costs a large fraction of a core —
                // the same trap the emulator fell into at 161% CPU.  Only
                // demonstrably idle seams are paced: a keystroke, a printed
                // byte, a modem byte or any disk access resets the count, so
                // nothing that is actually working is ever slowed down.
                let disk_now = machine.disk_accesses();
                if keys > 0 || printed || modem_moved || disk_now != disk_before {
                    idle_seams = 0;
                    // A guest that is printing, loading or moving modem bytes
                    // is not an abandoned session, so the idle clock is held
                    // off.  This matches the emulator, where the timeout is
                    // enforced at a *console read* — a program in the middle of
                    // its work is never cut off, only one waiting for a person
                    // who is not there.
                    last_key = tokio::time::Instant::now();
                } else {
                    idle_seams = idle_seams.saturating_add(1);
                    if let Some(nap) = idle_nap(idle_seams) {
                        tokio::time::sleep(nap).await;
                    }
                }
                disk_before = disk_now;
                printed = false;

                // An abandoned session.  There is no instruction ceiling here
                // on purpose: in the emulator it bounds one transient program
                // and hands the user back their `A>`, but a booted operating
                // system *is* the session and running indefinitely is what it
                // is supposed to do.  At 2000 M-instructions the ceiling would
                // have stopped every booted disk after about forty seconds of
                // sitting at its own prompt.  What actually needs bounding is a
                // user who has gone away, and that is what the idle timeout is.
                if !self.idle_timeout.is_zero() && last_key.elapsed() >= self.idle_timeout {
                    glog!("CP/M boot: session idle timeout with a disk booted");
                    return Ok(());
                }
            }

            if executed.is_multiple_of(YIELD_INTERVAL) {
                tokio::task::yield_now().await;
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
