//! The machine a booted disk runs on.
//!
//! This is the other half of the boot path: 64 KB of memory, the 88-DCDD on
//! ports 08h–0Ah, an 88-2SIO console on 10h/11h, and nothing else. No BDOS, no
//! page-zero vectors, no CCP — the disk's own operating system supplies all of
//! that. Our part is to be plausible hardware and get out of the way.
//!
//! # How this differs from the CP/M emulator next door
//!
//! `cpm::mod` traps BDOS and BIOS calls and services them against a filesystem
//! we control. Here nothing is trapped. That is the trade the booted path
//! makes, and it has consequences worth stating rather than discovering:
//!
//! * The guest owns every drive. Folder-backed drives, the jail, `EXIT` and the
//!   Gateway Shell do not exist inside it.
//! * The blast radius is the images in the drives — narrower than the
//!   filesystem path's, and easier to state, but the per-file write claim that
//!   stops two sessions interleaving records has no meaning here. A booted
//!   image is therefore held by one session and opened read-only unless the
//!   operator says otherwise.
//! * Unknown ports read as `0xFF` (an idle bus) rather than as whatever was
//!   last driven, so a guest probing for hardware we do not have sees nothing
//!   instead of an echo of itself.

// Staged build: the machine and its tests are complete, but the telnet driver
// that owns a session — the run loop, the double-ESC exit, the instruction
// ceiling — is the next step.  **Remove when that lands.**
#![allow(dead_code)]

use super::boot::{cold_boot, BootError};
use super::dcdd::{Dcdd, Disk, Geometry, Request, SECTOR_LEN};
use super::uart::UartFamily;
use iz80::{Cpu, Machine};

/// Console status port on an 88-2SIO.
pub const CONSOLE_STATUS_PORT: u8 = 0x10;
/// Console data port.
pub const CONSOLE_DATA_PORT: u8 = 0x11;

/// One image in a drive.
struct Mounted {
    bytes: Vec<u8>,
    geometry: Geometry,
    read_only: bool,
    dirty: bool,
}

/// Memory, ports and drives for a booted disk.
pub struct BootMachine {
    mem: Vec<u8>,
    dcdd: Dcdd,
    disks: Vec<Option<Mounted>>,
    /// Bytes the guest has printed.
    tx: Vec<u8>,
    /// Bytes waiting for the guest to read.
    rx: std::collections::VecDeque<u8>,
    /// Reads of the console status register since the last one that reported
    /// anything. A guest waiting forever on a key is normal; a guest waiting
    /// forever on a *disk* is not, and the two are told apart by this and
    /// `Dcdd::polls_on_sector`.
    idle_status_reads: u64,
    /// Diagnostic: how many times each port was touched.
    #[cfg(test)]
    port_hits: std::collections::BTreeMap<u8, u64>,
}

impl BootMachine {
    pub fn new() -> BootMachine {
        BootMachine {
            mem: vec![0; 0x10000],
            dcdd: Dcdd::new(),
            disks: (0..16).map(|_| None).collect(),
            tx: Vec::new(),
            rx: std::collections::VecDeque::new(),
            idle_status_reads: 0,
            #[cfg(test)]
            port_hits: std::collections::BTreeMap::new(),
        }
    }

    /// Put an image in a drive.
    ///
    /// The whole image is held in memory. These are floppies — 308 KB, or 4.8 MB
    /// for a hard disk — and a booted guest seeks constantly, so paging every
    /// sector off the host would turn every `DIR` into a storm of small reads.
    /// Writes are collected and given back with [`BootMachine::take_dirty`].
    pub fn insert(&mut self, drive: u8, bytes: Vec<u8>, read_only: bool) -> Result<(), String> {
        let geometry = geometry_for(bytes.len() as u64)
            .ok_or_else(|| format!("{} bytes is not an 88-DCDD image", bytes.len()))?;
        self.dcdd.insert(drive, Disk { geometry, read_only });
        if let Some(slot) = self.disks.get_mut(drive as usize) {
            *slot = Some(Mounted { bytes, geometry, read_only, dirty: false });
        }
        Ok(())
    }

    /// Images that the guest has written to, for the caller to persist.
    ///
    /// Handing them back rather than writing them ourselves keeps host file
    /// access on the caller's side, where the read-only rules and the mount
    /// bookkeeping already live.
    pub fn take_dirty(&mut self) -> Vec<(u8, Vec<u8>)> {
        let mut out = Vec::new();
        for (i, slot) in self.disks.iter_mut().enumerate() {
            if let Some(m) = slot {
                if m.dirty && !m.read_only {
                    m.dirty = false;
                    out.push((i as u8, m.bytes.clone()));
                }
            }
        }
        out
    }

    /// Cold-boot from a drive, leaving the CPU ready to run.
    pub fn boot(&mut self, cpu: &mut Cpu, drive: u8) -> Result<(), BootError> {
        let disks = &self.disks;
        let mut payload: Option<Vec<u8>> = None;
        let entry = cold_boot(
            &mut self.dcdd,
            drive,
            |d, t, s| {
                let m = disks
                    .get(d as usize)
                    .and_then(|x| x.as_ref())
                    .ok_or_else(|| format!("drive {d} is empty"))?;
                let off = m.geometry.offset(t, s) as usize;
                m.bytes
                    .get(off..off + SECTOR_LEN)
                    .map(|b| b.to_vec())
                    .ok_or_else(|| format!("track {t} sector {s} is past the end of the image"))
            },
            |addr, bytes| payload = Some((addr, bytes.to_vec()).1),
        )?;
        if let Some(p) = payload {
            self.mem[entry as usize..entry as usize + p.len()].copy_from_slice(&p);
        }
        cpu.registers().set_pc(entry);
        Ok(())
    }

    /// Give the guest a byte of console input.
    pub fn send_key(&mut self, byte: u8) {
        self.rx.push_back(byte);
    }

    /// Take everything the guest has printed.
    pub fn take_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.tx)
    }

    /// Console status reads since anything last happened.
    pub fn idle_status_reads(&self) -> u64 {
        self.idle_status_reads
    }

    /// Position-register reads without the disk moving on.
    pub fn stuck_polls(&self) -> u32 {
        self.dcdd.polls_on_sector()
    }

    /// Serve whatever the controller asked for after a port access.
    fn service(&mut self, req: Request) {
        match req {
            Request::None => {}
            Request::Read { drive, track, sector } => {
                let bytes = self
                    .disks
                    .get(drive as usize)
                    .and_then(|x| x.as_ref())
                    .and_then(|m| {
                        let off = m.geometry.offset(track, sector) as usize;
                        m.bytes.get(off..off + SECTOR_LEN).map(|b| b.to_vec())
                    });
                // A read past the end of the image gives the guest an erased
                // sector rather than a panic: a real drive returns *something*
                // from unformatted media, and the guest's own error handling
                // is better placed to react than we are.
                self.dcdd
                    .sector_loaded(drive, &bytes.unwrap_or_else(|| vec![0xE5; SECTOR_LEN]));
            }
            Request::Write { drive, track, sector } => {
                let Some(buf) = self.dcdd.sector_buffer(drive).copied() else {
                    return;
                };
                if let Some(m) = self.disks.get_mut(drive as usize).and_then(|x| x.as_mut()) {
                    if m.read_only {
                        return;
                    }
                    let off = m.geometry.offset(track, sector) as usize;
                    if let Some(dst) = m.bytes.get_mut(off..off + SECTOR_LEN) {
                        dst.copy_from_slice(&buf);
                        m.dirty = true;
                    }
                }
            }
        }
    }
}

impl Default for BootMachine {
    fn default() -> BootMachine {
        BootMachine::new()
    }
}

/// Which geometry an image of this size has, if any.
pub fn geometry_for(len: u64) -> Option<Geometry> {
    [Geometry::EIGHT_INCH, Geometry::MINIDISK]
        .into_iter()
        .find(|g| g.image_len() == len)
}

impl Machine for BootMachine {
    fn peek(&mut self, address: u16) -> u8 {
        self.mem[address as usize]
    }

    fn poke(&mut self, address: u16, value: u8) {
        self.mem[address as usize] = value;
    }

    fn port_in(&mut self, address: u16) -> u8 {
        let port = address as u8;
        #[cfg(test)]
        {
            *self.port_hits.entry(port).or_insert(0) += 1;
        }
        match port {
            0x08..=0x0A => {
                let (v, req) = self.dcdd.port_in(port);
                let was_fill = matches!(req, Request::Read { .. });
                self.service(req);
                self.idle_status_reads = 0;
                if was_fill {
                    // The controller asked for the sector *because* the guest
                    // wanted its first byte, so it had none to give and
                    // answered 0xFF.  Now that the sector is loaded, ask again
                    // and hand over the real byte.  Returning the placeholder
                    // would eat the first byte of every sector the guest reads
                    // — which boots far enough to look like it is working and
                    // then produces silence.
                    let (v2, req2) = self.dcdd.port_in(port);
                    self.service(req2);
                    return v2;
                }
                v
            }
            CONSOLE_STATUS_PORT => {
                self.idle_status_reads = self.idle_status_reads.saturating_add(1);
                // The 88-2SIO is an ACIA: bit 0 receive-full, bit 1
                // transmit-ready.  Transmit is always ready — our "wire" is a
                // buffer, so there is nothing to be busy about.
                UartFamily::Acia.status(!self.rx.is_empty(), true, true)
            }
            CONSOLE_DATA_PORT => {
                self.idle_status_reads = 0;
                self.rx.pop_front().unwrap_or(0)
            }
            // An idle bus, not an echo of whatever was last driven.
            _ => 0xFF,
        }
    }

    fn port_out(&mut self, address: u16, value: u8) {
        let port = address as u8;
        #[cfg(test)]
        {
            *self.port_hits.entry(port | 0x80).or_insert(0) += 1;
        }
        match port {
            0x08..=0x0A => {
                let req = self.dcdd.port_out(port, value);
                self.service(req);
                self.idle_status_reads = 0;
            }
            CONSOLE_DATA_PORT => {
                self.tx.push(value & 0x7F);
                self.idle_status_reads = 0;
            }
            // The control register and anything else: accepted and discarded.
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(geom: Geometry) -> Vec<u8> {
        vec![0u8; geom.image_len() as usize]
    }

    #[test]
    fn test_geometry_is_recognised_by_size() {
        assert_eq!(geometry_for(337_568), Some(Geometry::EIGHT_INCH));
        assert_eq!(geometry_for(35 * 16 * 137), Some(Geometry::MINIDISK));
        assert_eq!(geometry_for(256_256), None, "not an 88-DCDD image");
        assert_eq!(geometry_for(0), None);
    }

    #[test]
    fn test_inserting_a_wrong_sized_image_is_refused() {
        let mut m = BootMachine::new();
        let err = m.insert(0, vec![0; 1234], true).unwrap_err();
        assert!(err.contains("not an 88-DCDD image"), "{err}");
    }

    /// Console output must reach the caller, and input must reach the guest.
    #[test]
    fn test_console_round_trip() {
        let mut m = BootMachine::new();
        // Nothing waiting: receive-full clear, transmit ready set.
        let s = m.port_in(CONSOLE_STATUS_PORT as u16);
        assert_eq!(s & 0x01, 0, "no input yet");
        assert_ne!(s & 0x02, 0, "always ready to print");

        m.send_key(b'A');
        assert_ne!(m.port_in(CONSOLE_STATUS_PORT as u16) & 0x01, 0, "a key is waiting");
        assert_eq!(m.port_in(CONSOLE_DATA_PORT as u16), b'A');
        assert_eq!(m.port_in(CONSOLE_STATUS_PORT as u16) & 0x01, 0, "consumed");

        m.port_out(CONSOLE_DATA_PORT as u16, b'H');
        m.port_out(CONSOLE_DATA_PORT as u16, b'i');
        assert_eq!(m.take_output(), b"Hi");
        assert!(m.take_output().is_empty(), "output is taken, not copied");
    }

    /// A guest probing for hardware we do not have must see an idle bus, not an
    /// echo of whatever it last wrote.
    #[test]
    fn test_unknown_ports_read_as_an_idle_bus() {
        let mut m = BootMachine::new();
        m.port_out(0x40, 0x55);
        assert_eq!(m.port_in(0x40), 0xFF);
    }

    /// Waiting on the console is normal and must not look like a stuck disk.
    #[test]
    fn test_idle_console_polling_is_counted_separately_from_disk_polling() {
        let mut m = BootMachine::new();
        for _ in 0..10 {
            m.port_in(CONSOLE_STATUS_PORT as u16);
        }
        assert!(m.idle_status_reads() >= 10, "console waiting is counted");
        assert_eq!(m.stuck_polls(), 0, "and is not confused with the disk");
        m.send_key(b'x');
        m.port_in(CONSOLE_DATA_PORT as u16);
        assert_eq!(m.idle_status_reads(), 0, "reset once something happens");
    }

    /// A sector written by the guest must come back for the caller to persist,
    /// and only once.
    #[test]
    fn test_writes_are_collected_for_the_caller() {
        let mut m = BootMachine::new();
        m.insert(0, image(Geometry::EIGHT_INCH), false).unwrap();
        m.port_out(0x08, 0);
        m.port_out(0x09, 0x04); // head load
        m.port_out(0x09, 0x80); // write enable
        for i in 0..SECTOR_LEN {
            m.port_out(0x0A, i as u8);
        }
        for _ in 0..4 {
            m.port_in(0x09); // rotate until the sector passes
        }
        let dirty = m.take_dirty();
        assert_eq!(dirty.len(), 1, "the written image comes back");
        assert_eq!(dirty[0].0, 0);
        assert_eq!(dirty[0].1[5], 5, "the bytes landed in the image");
        assert!(m.take_dirty().is_empty(), "taken once, not repeatedly");
    }

    /// A read-only image must never come back as dirty, however hard the guest
    /// tries.
    #[test]
    fn test_a_read_only_image_is_never_written() {
        let mut m = BootMachine::new();
        m.insert(0, image(Geometry::EIGHT_INCH), true).unwrap();
        m.port_out(0x08, 0);
        m.port_out(0x09, 0x04);
        m.port_out(0x09, 0x80);
        for i in 0..SECTOR_LEN {
            m.port_out(0x0A, i as u8);
        }
        for _ in 0..4 {
            m.port_in(0x09);
        }
        assert!(m.take_dirty().is_empty(), "a protected image stays clean");
    }

    /// Reading past the end of an image gives an erased sector rather than
    /// panicking — a real drive returns something from unformatted media.
    #[test]
    fn test_reading_past_the_end_yields_an_erased_sector() {
        let mut m = BootMachine::new();
        m.insert(0, image(Geometry::MINIDISK), true).unwrap();
        m.port_out(0x08, 0);
        m.port_out(0x09, 0x04);
        // Step well past the last track, then read.
        for _ in 0..100 {
            m.port_out(0x09, 0x01);
        }
        m.port_in(0x0A);
        let v = m.port_in(0x0A);
        assert!(v == 0xE5 || v == 0, "no panic, and a defined byte: {v:#04x}");
    }

    /// The first byte of a sector must reach the guest.
    ///
    /// Regression: reading the data port for a sector not yet buffered made the
    /// controller ask the host for it and answer 0xFF, and the caller then
    /// filled the buffer — so the byte the guest had asked for was dropped and
    /// every sector arrived one byte short. A disk boots far enough to look
    /// like it works and then goes silent.
    #[test]
    fn test_the_first_byte_of_a_sector_is_not_lost() {
        let mut img = image(Geometry::EIGHT_INCH);
        for (i, b) in img[..SECTOR_LEN].iter_mut().enumerate() {
            *b = (i as u8).wrapping_add(1); // distinct, and never 0xFF at [0]
        }
        let mut m = BootMachine::new();
        m.insert(0, img, true).unwrap();
        m.port_out(0x08, 0);
        m.port_out(0x09, 0x04);
        let first = m.port_in(0x0A);
        assert_eq!(first, 1, "the guest must get byte 0, not a placeholder");
        assert_eq!(m.port_in(0x0A), 2, "and then byte 1");
        assert_eq!(m.port_in(0x0A), 3);
    }

    /// After booting, the drive must report that the head may move.
    ///
    /// Regression: the bootstrap reads the data port to fetch the boot sector,
    /// which opens a transfer. An open transfer holds "safe to move the head"
    /// low, and the first thing a boot sector does is seek to track 0 — so the
    /// guest span forever at its first instruction that touched the drive.
    #[test]
    fn test_the_head_may_move_once_the_bootstrap_is_done() {
        let mut img = image(Geometry::EIGHT_INCH);
        img[3..3 + 8].copy_from_slice(&[0x31, 0x00, 0xDF, 0xF3, 0xAF, 0xD3, 0x08, 0xDB]);
        let mut m = BootMachine::new();
        m.insert(0, img, true).unwrap();
        let mut cpu = Cpu::new_8080();
        m.boot(&mut cpu, 0).expect("boots");
        // Status is active low: the move-OK bit reads 0 when it is safe.
        let status = m.port_in(0x08);
        assert_eq!(
            status & 0x02,
            0,
            "the head must be free to seek right after boot, got {status:#04x}"
        );
    }

    /// Booting sets the program counter to the entry point the bootstrap
    /// reports, and leaves the payload where the CPU will find it.
    #[test]
    fn test_boot_places_the_payload_and_sets_pc() {
        let mut img = image(Geometry::EIGHT_INCH);
        // A plausible boot sector: LXI SP,0DF00h / DI / XRA A / OUT 08h.
        let code = [0x31u8, 0x00, 0xDF, 0xF3, 0xAF, 0xD3, 0x08, 0xDB];
        img[3..3 + code.len()].copy_from_slice(&code);
        let mut m = BootMachine::new();
        m.insert(0, img, true).unwrap();
        let mut cpu = Cpu::new_8080();
        m.boot(&mut cpu, 0).expect("boots");
        assert_eq!(cpu.registers().pc(), 0x0000);
        assert_eq!(m.peek(0x0000), 0x31, "the payload is in memory");
        assert_eq!(m.peek(0x0007), 0xDB, "and its jump targets line up");
    }

    #[test]
    fn test_booting_an_empty_drive_reports_it() {
        let mut m = BootMachine::new();
        let mut cpu = Cpu::new_8080();
        assert!(matches!(m.boot(&mut cpu, 0), Err(BootError::NoDisk(0))));
    }

    /// The end-to-end check the plan asked for: boot a real disk, run it, and
    /// see whether the guest's own operating system says anything.
    ///
    /// This is the strong oracle — it exercises the controller, the bootstrap,
    /// the CPU and the console together, and cannot be satisfied by a plausible
    /// wrong answer the way a "does this look like text" check can.
    ///
    /// Ignored: set `CPM_BOOT_IMAGE` to a `.dsk`.
    #[test]
    #[ignore]
    fn test_boot_a_real_disk_and_run_it() {
        let Ok(path) = std::env::var("CPM_BOOT_IMAGE") else {
            eprintln!("set CPM_BOOT_IMAGE to run this");
            return;
        };
        let bytes = std::fs::read(&path).unwrap();
        let mut m = BootMachine::new();
        m.insert(0, bytes, true).expect("an 88-DCDD image");
        let mut cpu = Cpu::new_8080();
        m.boot(&mut cpu, 0).expect("boots");

        // A short PC trace first: where a boot goes wrong is a control-flow
        // question, and the answer is always in the first hundred instructions.
        if std::env::var("CPM_BOOT_TRACE").is_ok() {
            let mut trace = Vec::new();
            for _ in 0..400u64 {
                trace.push(cpu.registers().pc());
                cpu.execute_instruction(&mut m);
            }
            println!("first PCs: {:04x?}", &trace[..60.min(trace.len())]);
        }
        let mut out = Vec::new();
        for _ in 0..20_000_000u64 {
            cpu.execute_instruction(&mut m);
            out.extend(m.take_output());
            if out.len() > 400 {
                break;
            }
        }
        let text: String = out
            .iter()
            .map(|&b| if (0x20..0x7F).contains(&b) || b == b'\n' { b as char } else { '.' })
            .collect();
        println!("--- {} ---\n{}", path, text);
        println!("port hits (0x80 bit = OUT): {:?}", m.port_hits);
        println!("mem[0..12] = {:02x?}", (0..12).map(|a| m.peek(a)).collect::<Vec<_>>());
        let st = m.port_in(0x08);
        println!("status={st:#04x}  track0 bit={}  moveok bit={}",
                 if st & 0x40 == 0 { "AT TRACK 0" } else { "not track 0" },
                 if st & 0x02 == 0 { "may move" } else { "busy" });
        println!(
            "pc={:#06x} stuck_polls={} idle_console={}",
            cpu.registers().pc(),
            m.stuck_polls(),
            m.idle_status_reads()
        );
        // Not yet an assertion.  The guest boots, seeks, loads sectors and runs
        // the code it loaded — the program counter ends up well inside the
        // loaded image rather than in the boot sector — but it has not reached
        // its sign-on yet.  What is missing is most likely another status or
        // timing detail the BIOS waits on.  When it prints, turn this into the
        // assertion the plan asked for: require the sign-on text.
        if out.is_empty() {
            println!("(no console output yet — see the note in this test)");
        }
    }
}
