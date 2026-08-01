//! The machine a booted disk runs on.
//!
//! This is the other half of the boot path: 64 KB of memory, the 88-DCDD on
//! ports 08h–0Ah, an 88-2SIO console on 10h/11h, the front-panel sense switches
//! on FFh, and — when the operator has selected a port profile that fits — the
//! virtual modem. No BDOS, no page-zero vectors, no CCP: the disk's own
//! operating system supplies all of that. Our part is to be plausible hardware
//! and get out of the way.
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

use super::boot::{cold_boot, BootError};
use super::dcdd::{Dcdd, Disk, Geometry, Request, SECTOR_LEN};
use super::modem_port::ModemPort;
use super::uart::{ModemAccess, UartFamily};
use iz80::{Cpu, Machine};

/// Console status port on an 88-2SIO.
pub const CONSOLE_STATUS_PORT: u8 = 0x10;
/// Console data port.
pub const CONSOLE_DATA_PORT: u8 = 0x11;

/// The front-panel sense switches, read on port FFh.
pub const SENSE_SWITCH_PORT: u8 = 0xFF;

/// What the sense switches say when nobody has set them.
///
/// This is not a neutral choice and it is not zero-by-accident. MITS system
/// software — Altair DOS, Disk BASIC, Time Sharing BASIC — asks the *front
/// panel* which console board the machine has, because in 1976 that genuinely
/// varied from machine to machine. It reads port FFh and picks a terminal
/// driver from the low four bits: 88-SIO on ports 00h/01h, 88-ACR on 06h/07h,
/// 4PIO on 20h-23h, or the 88-2SIO on 10h/11h.
///
/// The 88-2SIO is the one we emulate, and zero is the setting that selects it.
/// Leaving the port floating at FFh instead selected the 88-SIO — so Altair DOS
/// booted perfectly, wrote its sign-on to port 01h, and we dropped every byte
/// on the floor. The disk was never the problem; the front panel was.
///
/// It is a constant rather than a setting because there is nothing yet for an
/// operator to set it from. When the boot path gets its config key and its
/// screens, the switches belong there too: they are the one control that
/// decides what hardware MITS software believes it is talking to.
pub const DEFAULT_SENSE_SWITCHES: u8 = 0x00;

/// Console bytes a guest may have waiting before further ones are dropped.
///
/// Matches the emulator's pending-input bound. Generous next to anything a
/// person types, and small enough that a client streaming into a guest which
/// never reads its console cannot grow the host's memory.
const KEY_QUEUE_CAP: usize = 4096;

/// Ports this machine's own hardware answers, which a virtual modem may not
/// take over: the 88-DCDD controller, the console, and the front panel.
const RESERVED_PORTS: &[u8] = &[
    0x08,
    0x09,
    0x0A,
    CONSOLE_STATUS_PORT,
    CONSOLE_DATA_PORT,
    SENSE_SWITCH_PORT,
];

/// What happened when the configured virtual modem was offered to a booted
/// machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModemAttach {
    /// The operator has no virtual modem selected.
    Off,
    /// Wired up at these status and data ports.
    Ports(u8, u8),
    /// Selected, but it cannot exist in a booted machine — with the reason,
    /// which is shown to whoever is booting rather than logged and forgotten.
    Unavailable(String),
}

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
    /// What the front panel reports on port FFh.
    sense_switches: u8,
    /// The virtual modem, if the operator selected a port profile that can
    /// exist here.  Shared with the CP/M emulator's machine, so the rings and
    /// the status bits behave identically in both.
    modem: ModemPort,
    /// Accesses to the disk controller's ports, ever.
    ///
    /// The driver's "is this guest doing anything?" signal for the one piece of
    /// work it cannot otherwise see. Console bytes and modem bytes pass through
    /// its own hands; a disk load does not, and pacing a guest that is busy
    /// reading a track would make every `DIR` crawl.
    disk_accesses: u64,
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
            sense_switches: DEFAULT_SENSE_SWITCHES,
            modem: ModemPort::new(),
            disk_accesses: 0,
            #[cfg(test)]
            port_hits: std::collections::BTreeMap::new(),
        }
    }

    /// The CPU a booted disk runs on.
    ///
    /// A **Z80**, and one place decides so the driver and the tests cannot
    /// disagree. The Altair shipped with an 8080 and every MITS disk here is
    /// 8080 code, so an 8080 core is the more literal machine — but the Z80 is
    /// a superset that runs all of it, Altairs were very commonly fitted with
    /// Z80 upgrade boards, and the CP/M emulator next door is already a Z80.
    ///
    /// The deciding case is our own: EGT80 is Z80 code and declares itself so.
    /// On an 8080 core it loads, executes a Z80-only opcode as something else,
    /// and takes CP/M down with it — the sign-on comes back corrupted on the
    /// warm boot. A machine that cannot run the terminal we ship with it is the
    /// wrong machine.
    pub fn new_cpu() -> Cpu {
        Cpu::new_z80()
    }

    /// Offer the configured virtual modem to this machine.
    ///
    /// A booted disk *can* have our modem, and on a real Altair it is obvious
    /// where: the 88-2SIO is a two-port board, the console is port A, and the
    /// modem goes on port B — which is exactly the `altair_2sio2` profile at
    /// 12h/13h. Software running under a booted Altair CP/M then finds a UART
    /// where it expects one and dials out through us.
    ///
    /// Two kinds of profile cannot come along, and saying so plainly beats a
    /// modem that silently is not there:
    ///
    /// * `AUX:` and HBIOS are not hardware. They are our own BDOS device and
    ///   RomWBW's firmware call, and a booted disk brings its own of both — so
    ///   there is nothing for us to answer.
    /// * A profile whose ports land on the disk controller, the console or the
    ///   front panel would fight this machine's own hardware. `altair_2sio1` is
    ///   the one that catches people out: it is 10h/11h, which *is* the
    ///   console, because on a real Altair the console is 2SIO port A.
    pub fn attach_modem(&mut self, access: ModemAccess) -> ModemAttach {
        match access {
            ModemAccess::Off => ModemAttach::Off,
            ModemAccess::Aux => ModemAttach::Unavailable(
                "the AUX: device belongs to our BDOS, and a booted disk brings its own".into(),
            ),
            ModemAccess::Hbios { .. } => ModemAttach::Unavailable(
                "HBIOS is RomWBW firmware, which a booted Altair disk does not have".into(),
            ),
            ModemAccess::Ports(u) => {
                if let Some(&clash) = RESERVED_PORTS
                    .iter()
                    .find(|&&p| p == u.status_port || p == u.data_port)
                {
                    let what = match clash {
                        CONSOLE_STATUS_PORT | CONSOLE_DATA_PORT => "the console",
                        SENSE_SWITCH_PORT => "the front panel",
                        _ => "the disk controller",
                    };
                    return ModemAttach::Unavailable(format!(
                        "port {clash:#04x} is {what} on this machine — try altair_2sio2"
                    ));
                }
                self.modem.set_access(access);
                ModemAttach::Ports(u.status_port, u.data_port)
            }
        }
    }

    /// The virtual modem's rings, for the driver to pump between CPU batches.
    pub fn modem(&mut self) -> &mut ModemPort {
        &mut self.modem
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

    /// Cold-boot with a specific sector step, for diagnosing a disk whose
    /// loader is not laid out the way the bootstrap assumes.
    ///
    /// Every Altair disk we have — CP/M and MITS alike — uses the 2:1 step
    /// [`super::boot::BOOT_INTERLEAVE`], so nothing in the product needs this.
    /// It exists because trying another step is the first thing you would want
    /// to do with a disk that loads and then runs off into nothing.
    #[cfg(test)]
    fn boot_with_step(&mut self, cpu: &mut Cpu, drive: u8, step: u8) -> Result<(), BootError> {
        let disks = &self.disks;
        let mut chunks: Vec<(u16, Vec<u8>)> = Vec::new();
        let entry = super::boot::cold_boot_with_step(
            &mut self.dcdd,
            drive,
            step,
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
            |addr, bytes| chunks.push((addr, bytes.to_vec())),
        )?;
        for (addr, bytes) in chunks {
            let at = addr as usize;
            let end = (at + bytes.len()).min(self.mem.len());
            self.mem[at..end].copy_from_slice(&bytes[..end - at]);
        }
        cpu.registers().set_pc(entry);
        Ok(())
    }

    /// Cold-boot from a drive, leaving the CPU ready to run.
    pub fn boot(&mut self, cpu: &mut Cpu, drive: u8) -> Result<(), BootError> {
        let disks = &self.disks;
        // Every chunk with the address it belongs at.  The bootstrap stores
        // more than one, and keeping only the last — or ignoring the address —
        // silently loads a partial loader that runs off its own end.
        let mut chunks: Vec<(u16, Vec<u8>)> = Vec::new();
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
            |addr, bytes| chunks.push((addr, bytes.to_vec())),
        )?;
        for (addr, bytes) in chunks {
            let at = addr as usize;
            let end = (at + bytes.len()).min(self.mem.len());
            self.mem[at..end].copy_from_slice(&bytes[..end - at]);
        }
        cpu.registers().set_pc(entry);
        Ok(())
    }

    /// Give the guest a byte of console input.
    ///
    /// Bounded, and it has to be: a real console has no queue at all, and a
    /// guest that is not reading its own console — sitting in a compute loop, or
    /// simply wedged — would otherwise let a client stream bytes into this
    /// buffer for as long as it liked. Dropping the excess is what a real UART
    /// does when nobody reads the receive register, and it is the same bound the
    /// emulator puts on its own pending-input queue.
    pub fn send_key(&mut self, byte: u8) {
        if self.rx.len() < KEY_QUEUE_CAP {
            self.rx.push_back(byte);
        }
    }

    /// Take everything the guest has printed.
    pub fn take_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.tx)
    }

    /// Console status reads since anything last happened.
    ///
    /// A guest waiting forever on a key is normal; a guest waiting forever on a
    /// *disk* is a fault, and this tells the two apart with
    /// [`BootMachine::stuck_polls`] reporting the other side.  The driver paces
    /// on observed activity instead (see `cpm_boot_ui`), so this is the test's
    /// way of proving the two are not confused.
    #[cfg(test)]
    pub fn idle_status_reads(&self) -> u64 {
        self.idle_status_reads
    }

    /// Accesses to the disk controller's ports since the machine was made.
    pub fn disk_accesses(&self) -> u64 {
        self.disk_accesses
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

/// The disk geometries a booted 88-DCDD can carry, with what to call them.
///
/// A list rather than two constants because the documentation an operator reads
/// is rendered from it — the same discipline `image::format::FORMATS` follows,
/// so a geometry added here cannot go missing from the readme that tells people
/// which disks work.
pub const BOOT_GEOMETRIES: &[(Geometry, &str)] = &[
    (Geometry::EIGHT_INCH, "Altair 88-DCDD 8\" floppy"),
    (Geometry::MINIDISK, "Altair 88-MDS 5.25\" minidisk"),
];

/// Which geometry an image of this size has, if any.
///
/// A short trailer is allowed. Several of the images in circulation carry a few
/// bytes past the last sector — 96 bytes on the CP/M 3 and MITS+Tarbell disks,
/// 80 bytes of `1A` on the minidisks, which is a CP/M end-of-file pad from
/// whatever copied them. Rejecting those on an exact size match cost us seven
/// perfectly good disks, including both CP/M 3 images.
///
/// The tolerance is deliberately less than one sector: past that, the size no
/// longer identifies the geometry and accepting it would mean reading a disk we
/// have not actually recognised.
pub fn geometry_for(len: u64) -> Option<Geometry> {
    BOOT_GEOMETRIES.iter().map(|(g, _)| *g).find(|g| {
        let want = g.image_len();
        len >= want && len - want < SECTOR_LEN as u64
    })
}

/// The largest trailer a bootable image may carry past its last sector.
pub const MAX_IMAGE_TRAILER: u64 = SECTOR_LEN as u64 - 1;

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
                self.disk_accesses = self.disk_accesses.saturating_add(1);
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
            // The front panel. Input only on real hardware, and the reason
            // every MITS operating system can find its console.
            SENSE_SWITCH_PORT => self.sense_switches,
            // The virtual modem, if the operator gave this machine one. Asked
            // after the fixed hardware above, though `attach_modem` has already
            // refused any profile that could overlap it.
            //
            // Deliberately no idle bookkeeping here. A comms program polling
            // its UART is not console activity, and whether it is *idle* is a
            // question only the driver can answer — it is the one that knows
            // whether the pump actually moved a byte this batch.
            other => self.modem.port_in(other).unwrap_or(0xFF),
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
                self.disk_accesses = self.disk_accesses.saturating_add(1);
                let req = self.dcdd.port_out(port, value);
                self.service(req);
                self.idle_status_reads = 0;
            }
            CONSOLE_DATA_PORT => {
                self.tx.push(value & 0x7F);
                self.idle_status_reads = 0;
            }
            // The control register and anything else: offered to the modem,
            // then accepted and discarded.
            other => {
                self.modem.port_out(other, value);
            }
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

    /// Real images in circulation carry a few bytes past the last sector, and
    /// refusing them on an exact size match locked out both CP/M 3 disks and
    /// every minidisk.
    #[test]
    fn test_a_short_trailer_does_not_hide_the_geometry() {
        assert_eq!(geometry_for(337_664), Some(Geometry::EIGHT_INCH), "96-byte trailer");
        assert_eq!(geometry_for(76_800), Some(Geometry::MINIDISK), "80 bytes of 1A");
        assert_eq!(
            geometry_for(337_568 + SECTOR_LEN as u64),
            None,
            "a whole extra sector is a different disk, not a trailer"
        );
        assert_eq!(geometry_for(337_567), None, "and short is never rounded up");
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

    /// The operator's modem must reach a booted guest at the ports they chose.
    ///
    /// This is the whole point of `attach_modem`: a booted Altair CP/M running
    /// comms software has to find a UART where such software looks for one.
    #[test]
    fn test_the_configured_modem_ports_reach_a_booted_guest() {
        use crate::cpm::resolve_access;

        let mut m = BootMachine::new();
        assert_eq!(
            m.attach_modem(resolve_access("altair_2sio2")),
            ModemAttach::Ports(0x12, 0x13),
            "the second 2SIO port is where an Altair's modem lives"
        );
        // What the peer sent reaches the guest...
        m.modem().queue_rx(b"OK");
        assert_eq!(m.port_in(0x13), b'O');
        assert_eq!(m.port_in(0x13), b'K');
        // ...and what the guest writes comes back for the driver to forward.
        m.port_out(0x13, b'A');
        m.port_out(0x13, b'T');
        assert_eq!(m.modem().drain_tx(), b"AT");
    }

    /// The profile that catches people out: on a real Altair the console *is*
    /// 2SIO port A, so pointing the modem at it would have the two fighting
    /// over `0x10`/`0x11`.
    #[test]
    fn test_a_modem_profile_that_lands_on_our_hardware_is_refused() {
        use crate::cpm::resolve_access;

        let mut m = BootMachine::new();
        match m.attach_modem(resolve_access("altair_2sio1")) {
            ModemAttach::Unavailable(why) => {
                assert!(why.contains("console"), "must name the clash: {why}");
                assert!(why.contains("altair_2sio2"), "and what to use instead: {why}");
            }
            other => panic!("0x10/0x11 is the console, got {other:?}"),
        }
        // And the console still works, rather than answering as a UART.
        assert_ne!(m.port_in(CONSOLE_STATUS_PORT as u16), 0);
    }

    /// `AUX:` and HBIOS are our BDOS device and RomWBW's firmware.  A booted
    /// disk brings its own of both, so there is nothing for us to answer — and
    /// saying why beats a modem that is silently absent.
    #[test]
    fn test_the_non_hardware_modem_modes_explain_themselves() {
        use crate::cpm::resolve_access;

        for key in ["aux", "hbios_1", "hbios_2"] {
            let mut m = BootMachine::new();
            match m.attach_modem(resolve_access(key)) {
                ModemAttach::Unavailable(why) => assert!(!why.is_empty(), "{key} must say why"),
                other => panic!("{key} cannot exist in a booted machine, got {other:?}"),
            }
        }
        let mut m = BootMachine::new();
        assert_eq!(m.attach_modem(resolve_access("off")), ModemAttach::Off);
    }

    /// A client streaming at a guest that never reads its console must not be
    /// able to grow the host's memory.  A real UART simply drops what nobody
    /// collects, and so do we.
    #[test]
    fn test_console_input_is_bounded() {
        let mut m = BootMachine::new();
        for i in 0..(KEY_QUEUE_CAP * 2) {
            m.send_key((i & 0xFF) as u8);
        }
        assert_eq!(m.rx.len(), KEY_QUEUE_CAP, "the queue stops growing");
        // And the bytes kept are the *earliest*, so a guest that starts reading
        // sees the beginning of what was typed rather than an arbitrary window.
        assert_eq!(m.port_in(CONSOLE_DATA_PORT as u16), 0);
        assert_eq!(m.port_in(CONSOLE_DATA_PORT as u16), 1);
    }

    /// With no modem attached, its ports must read as an idle bus like any
    /// other hardware we do not have — not as zero, and not as an echo.
    #[test]
    fn test_modem_ports_are_inert_until_one_is_attached() {
        let mut m = BootMachine::new();
        m.port_out(0x13, 0x55);
        assert_eq!(m.port_in(0x12), 0xFF);
        assert_eq!(m.port_in(0x13), 0xFF);
    }

    /// The front panel must answer, and must not answer with the idle bus.
    ///
    /// Regression, and an expensive one: port FFh fell through to the
    /// unknown-port `0xFF`, which is a valid sense-switch reading meaning
    /// "console on the 88-SIO". Altair DOS, Disk BASIC and Time Sharing BASIC
    /// all booted correctly and then printed to ports 00h/01h, which we do not
    /// emulate — so a fully working guest looked like a disk that would not
    /// boot. The switches must select the 88-2SIO console we actually provide.
    #[test]
    fn test_the_sense_switches_select_the_console_we_emulate() {
        let mut m = BootMachine::new();
        let sw = m.port_in(SENSE_SWITCH_PORT as u16);
        assert_eq!(sw, DEFAULT_SENSE_SWITCHES);
        assert_ne!(
            sw, 0xFF,
            "a floating FFh sends MITS software to a console we do not have"
        );
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
        let mut cpu = BootMachine::new_cpu();
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
        let mut cpu = BootMachine::new_cpu();
        m.boot(&mut cpu, 0).expect("boots");
        assert_eq!(cpu.registers().pc(), 0x0000);
        assert_eq!(m.peek(0x0000), 0x31, "the payload is in memory");
        assert_eq!(m.peek(0x0007), 0xDB, "and its jump targets line up");
    }

    #[test]
    fn test_booting_an_empty_drive_reports_it() {
        let mut m = BootMachine::new();
        let mut cpu = BootMachine::new_cpu();
        assert!(matches!(m.boot(&mut cpu, 0), Err(BootError::NoDisk(0))));
    }

    /// Run a booted guest until it stops printing, and return what it said.
    ///
    /// "Stops printing" rather than a fixed instruction count: a CP/M command
    /// takes wildly different times depending on whether it touches the disk,
    /// and a fixed budget either truncates `DIR` or wastes seconds on a prompt.
    #[cfg(test)]
    fn run_until_quiet(m: &mut BootMachine, cpu: &mut Cpu, budget: u64) -> Vec<u8> {
        let mut out = Vec::new();
        let mut quiet: u64 = 0;
        for _ in 0..budget {
            cpu.execute_instruction(m);
            let o = m.take_output();
            if o.is_empty() {
                quiet += 1;
                // Long enough to cover a seek and a track read.
                if quiet > 3_000_000 && !out.is_empty() {
                    break;
                }
            } else {
                quiet = 0;
                out.extend(o);
            }
        }
        out
    }

    #[cfg(test)]
    fn printable(bytes: &[u8]) -> String {
        bytes
            .iter()
            .map(|&b| if (0x20..0x7F).contains(&b) || b == b'\n' { b as char } else { '.' })
            .collect()
    }

    /// The end-to-end test that exercises everything at once: boot a real disk,
    /// use its own operating system to list its own directory, run **our**
    /// terminal on it, and check that our virtual modem's ports reach it.
    ///
    /// EGT80 is the sharpest probe available for this. It is a real CP/M
    /// program, built by a period assembler, that drives a UART directly — so
    /// if it comes up and talks to `0x12`/`0x13` inside a booted Altair, then
    /// the controller, the bootstrap, the CPU, the console, the guest's own
    /// BDOS and our modem ports are all working together.
    ///
    /// **Blocked on getting EGT80 onto an Altair floppy in the first place**,
    /// which is the same unsolved mapping that keeps `altair8` out of
    /// `image::format::FORMATS`. Writing the file in with `cpmtools` — using
    /// the measured geometry and the skew recovered from the disk's own boot
    /// tracks — produces a directory entry for **extent 1 only**, with no
    /// extent 0 anywhere on the disk. CP/M's `DIR` lists extent-0 entries, so
    /// the guest correctly does not show it, and the file would be truncated
    /// even if it did. That is a fact about the Altair block mapping, not about
    /// booting: the same disk boots, runs its own `DIR` and lists its own
    /// forty-one files perfectly from the bytes we never touched.
    ///
    /// The way in that does not need us to understand the layout at all is the
    /// guest's own: these disks carry `PCGET.COM`, Mike Douglas's XMODEM
    /// receiver for the 88-2SIO, so the guest can pull EGT80 in over our
    /// virtual modem port and write it with its own BDOS. That is the next
    /// thing to try, and it would test more of the path than this does.
    ///
    /// Ignored: set `CPM_BOOT_IMAGE` to an Altair CP/M image with `EGT80.COM`
    /// really on it. Framing an image back up needs two sector checksums, both
    /// measured from the disks themselves and holding for every sector of
    /// DISK01 (192/192 and 2272/2272): tracks 0-5 keep a plain sum of the 128
    /// data bytes at byte 132; tracks 6-76 keep the sum of the data plus header
    /// bytes 2, 3, 5 and 6, at byte 4.
    #[test]
    #[ignore]
    fn test_run_egt80_inside_a_booted_disk() {
        let Ok(path) = std::env::var("CPM_BOOT_IMAGE") else {
            eprintln!("set CPM_BOOT_IMAGE to an Altair CP/M image carrying EGT80.COM");
            return;
        };
        let bytes = std::fs::read(&path).unwrap();
        let mut m = BootMachine::new();
        m.insert(0, bytes, true).expect("an 88-DCDD image");

        // The modem where a real Altair would have put it: 2SIO port B.
        assert_eq!(
            m.attach_modem(crate::cpm::resolve_access("altair_2sio2")),
            ModemAttach::Ports(0x12, 0x13),
        );

        let mut cpu = BootMachine::new_cpu();
        m.boot(&mut cpu, 0).expect("boots");

        let banner = printable(&run_until_quiet(&mut m, &mut cpu, 60_000_000));
        println!("--- sign-on ---\n{banner}");
        assert!(banner.contains("CP/M"), "no sign-on: {banner:?}");

        // The guest's own DIR, which is its filesystem answering, not ours.
        for &b in b"DIR\r" {
            m.send_key(b);
        }
        let dir = printable(&run_until_quiet(&mut m, &mut cpu, 200_000_000));
        println!("--- DIR ---\n{dir}");
        assert!(
            dir.to_ascii_uppercase().contains("EGT80"),
            "the guest's own DIR does not list EGT80: {dir:?}"
        );

        // Now run it.
        for &b in b"EGT80\r" {
            m.send_key(b);
        }
        let screen = printable(&run_until_quiet(&mut m, &mut cpu, 200_000_000));
        println!("--- EGT80 ---\n{screen}");
        assert!(
            screen.to_ascii_uppercase().contains("EGT80"),
            "EGT80 did not start: {screen:?}"
        );
        println!(
            "modem: guest wrote {:?}, rx free {}",
            printable(&m.modem().drain_tx()),
            m.modem().rx_free()
        );
    }

    /// Send a file to a booted guest with XMODEM, over the virtual modem's
    /// ports, while the guest runs.
    ///
    /// A sender rather than a receiver because that is the direction that puts
    /// a file *onto* a disk we cannot write ourselves. Deliberately written out
    /// here in the classic form rather than driven through `crate::xmodem`:
    /// that one speaks to an async stream, and what is on the other side of
    /// this is a synchronous pair of rings being clocked by a CPU. Keeping the
    /// two apart also means this test would notice if our own implementation
    /// drifted, instead of agreeing with it.
    ///
    /// Returns what the guest printed while the transfer ran.
    #[cfg(test)]
    fn xmodem_send_to_guest(
        m: &mut BootMachine,
        cpu: &mut Cpu,
        file: &[u8],
        budget: u64,
    ) -> (bool, Vec<u8>) {
        const SOH: u8 = 0x01;
        const EOT: u8 = 0x04;
        const ACK: u8 = 0x06;
        const NAK: u8 = 0x15;
        const CRC: u8 = b'C';

        let blocks: Vec<Vec<u8>> = file
            .chunks(128)
            .map(|c| {
                let mut b = c.to_vec();
                // The last block is padded, as XMODEM has no length field.
                b.resize(128, 0x1A);
                b
            })
            .collect();

        let mut console = Vec::new();
        let mut sent = 0usize; // blocks fully acknowledged
        let mut crc_mode = None; // decided by the receiver's first prompt
        let mut in_flight = false;
        let mut done = false;
        let mut eot_sent = false;

        for _ in 0..budget {
            cpu.execute_instruction(m);
            console.extend(m.take_output());

            // Whatever the guest wrote at its UART is the receiver talking.
            for b in m.modem().drain_tx() {
                match b {
                    NAK | CRC if crc_mode.is_none() => crc_mode = Some(b == CRC),
                    ACK if in_flight => {
                        in_flight = false;
                        sent += 1;
                    }
                    ACK if eot_sent => done = true,
                    NAK if in_flight => in_flight = false, // resend the same block
                    _ => {}
                }
            }
            if done {
                break;
            }
            let Some(use_crc) = crc_mode else {
                continue; // the receiver has not asked yet
            };
            if in_flight || eot_sent {
                continue;
            }
            if sent == blocks.len() {
                m.modem().queue_rx(&[EOT]);
                eot_sent = true;
                continue;
            }

            let n = sent;
            let mut pkt = vec![SOH, (n as u8).wrapping_add(1), !(n as u8).wrapping_add(1)];
            pkt.extend_from_slice(&blocks[n]);
            if use_crc {
                // CRC-16/XMODEM, written out here rather than borrowed from
                // `crate::xmodem`: an independent implementation is the whole
                // value of a cross-check, and one that calls the code under
                // test would agree with it however wrong it was.
                let mut c: u16 = 0;
                for &byte in &blocks[n] {
                    c ^= (byte as u16) << 8;
                    for _ in 0..8 {
                        c = if c & 0x8000 != 0 { (c << 1) ^ 0x1021 } else { c << 1 };
                    }
                }
                pkt.push((c >> 8) as u8);
                pkt.push(c as u8);
            } else {
                pkt.push(blocks[n].iter().fold(0u8, |a, &b| a.wrapping_add(b)));
            }
            m.modem().queue_rx(&pkt);
            in_flight = true;
        }
        (done, console)
    }

    /// Receive a file from a booted guest with XMODEM, over the modem's ports.
    ///
    /// The other direction, and the only way to check that what the guest wrote
    /// to its disk is what we sent: read it back with the guest's own reader.
    #[cfg(test)]
    fn xmodem_receive_from_guest(
        m: &mut BootMachine,
        cpu: &mut Cpu,
        budget: u64,
    ) -> (Option<Vec<u8>>, Vec<u8>) {
        const SOH: u8 = 0x01;
        const EOT: u8 = 0x04;
        const ACK: u8 = 0x06;
        const NAK: u8 = 0x15;

        let mut console = Vec::new();
        let mut file = Vec::new();
        let mut wire: Vec<u8> = Vec::new();
        let mut done = false;
        // Checksum mode: the plainest thing every sender supports.
        let mut prompted = 0u64;

        for i in 0..budget {
            cpu.execute_instruction(m);
            console.extend(m.take_output());
            wire.extend(m.modem().drain_tx());

            // Prod the sender until it starts, the way a real receiver does.
            if wire.is_empty() && file.is_empty() && i.wrapping_sub(prompted) > 20_000_000 {
                prompted = i;
                m.modem().queue_rx(&[NAK]);
            }

            loop {
                match wire.first() {
                    Some(&EOT) => {
                        wire.remove(0);
                        m.modem().queue_rx(&[ACK]);
                        done = true;
                    }
                    Some(&SOH) if wire.len() >= 132 => {
                        let pkt: Vec<u8> = wire.drain(..132).collect();
                        let sum = pkt[3..131].iter().fold(0u8, |a, &b| a.wrapping_add(b));
                        if pkt[1] == !pkt[2] && sum == pkt[131] {
                            file.extend_from_slice(&pkt[3..131]);
                            m.modem().queue_rx(&[ACK]);
                        } else {
                            m.modem().queue_rx(&[NAK]);
                        }
                    }
                    // Anything that is not the start of a packet is noise
                    // between frames; drop it rather than desynchronise.
                    Some(&b) if b != SOH => {
                        wire.remove(0);
                        continue;
                    }
                    _ => break,
                }
            }
            if done {
                break;
            }
        }
        (if done { Some(file) } else { None }, console)
    }

    /// The whole path, end to end, using nothing but the guest's own software:
    /// boot an Altair CP/M disk, run **its** XMODEM receiver, push our terminal
    /// into it through **our** virtual modem's ports, and see the file appear
    /// in the disk's own directory.
    ///
    /// This is the answer to a problem rather than a demonstration. We cannot
    /// write an Altair floppy from the host — `cpmtools` derives `EXM 1` where
    /// the disk's BIOS says `EXM 0`, so its directory entry is one the guest
    /// will not look at — and the block mapping past the first allocation block
    /// is still unsolved. None of that matters here, because the guest does its
    /// own filesystem work. It also tests more than the mount path ever could:
    /// the controller, the bootstrap, the CPU, the console, the guest's BDOS,
    /// its disk *writes*, and our modem ports, all at once.
    ///
    /// `PCGET.COM` is Mike Douglas's XMODEM receiver, on the Altair CP/M disks.
    /// Given `B` it uses 2SIO port B at 12h/13h — which is exactly the
    /// `altair_2sio2` profile, and why that is the one to pick for a booted
    /// Altair.
    ///
    /// Ignored: set `CPM_BOOT_IMAGE` to an Altair CP/M image carrying PCGET.COM.
    #[test]
    #[ignore]
    fn test_pcget_pulls_egt80_in_over_the_virtual_modem() {
        /// Our own terminal, the thing worth putting on the disk.
        const EGT80_COM: &[u8] = include_bytes!("../../EGT80/EGT80.COM");

        let Ok(path) = std::env::var("CPM_BOOT_IMAGE") else {
            eprintln!("set CPM_BOOT_IMAGE to an Altair CP/M image carrying PCGET.COM");
            return;
        };
        let bytes = std::fs::read(&path).unwrap();
        let mut m = BootMachine::new();
        // Writable: the point is that the guest writes to it.
        m.insert(0, bytes, false).expect("an 88-DCDD image");
        assert_eq!(
            m.attach_modem(crate::cpm::resolve_access("altair_2sio2")),
            ModemAttach::Ports(0x12, 0x13),
            "PCGET's port B is our altair_2sio2"
        );
        // A modem that is off-hook, since the guest is entitled to look.
        m.modem().set_carrier(true);

        let mut cpu = BootMachine::new_cpu();
        m.boot(&mut cpu, 0).expect("boots");
        let signon = printable(&run_until_quiet(&mut m, &mut cpu, 60_000_000));
        assert!(signon.contains("CP/M"), "no sign-on: {signon:?}");

        // Ask the guest to receive, on the port our modem is wired to.
        for &b in b"PCGET EGT80.COM B\r" {
            m.send_key(b);
        }
        let prompt = printable(&run_until_quiet(&mut m, &mut cpu, 200_000_000));
        println!("--- PCGET ---\n{prompt}");
        assert!(
            prompt.contains("XMODEM"),
            "PCGET did not ask for the file: {prompt:?}"
        );

        let (done, during) = xmodem_send_to_guest(&mut m, &mut cpu, EGT80_COM, 4_000_000_000);
        println!("--- transfer ---\n{}", printable(&during));
        // Whatever the guest managed to write, for inspection when this fails:
        // a sector the guest wrote and cannot read back is a fault in our
        // controller, and the only way to tell is to look at the bytes.
        if let Ok(dump) = std::env::var("CPM_BOOT_DUMP") {
            if let Some((_, img)) = m.take_dirty().first() {
                std::fs::write(&dump, img).unwrap();
                println!("(wrote the guest's image to {dump})");
            } else {
                println!("(the guest wrote nothing at all)");
            }
        }
        assert!(done, "the transfer never completed");

        let after = printable(&run_until_quiet(&mut m, &mut cpu, 400_000_000));
        println!("--- after ---\n{after}");
        assert!(
            after.contains("Transfer Complete"),
            "PCGET did not report success: {after:?}"
        );

        // The disk's own directory is the only witness that counts.
        for &b in b"DIR EGT80.COM\r" {
            m.send_key(b);
        }
        let dir = printable(&run_until_quiet(&mut m, &mut cpu, 400_000_000));
        println!("--- DIR ---\n{dir}");
        assert!(
            dir.to_ascii_uppercase().contains("EGT80"),
            "the guest wrote the file but does not list it: {dir:?}"
        );

        // Now read it back with the guest's own sender, which is the only
        // check that the bytes on the disk are the bytes we sent: it uses the
        // guest's filesystem, its block mapping and its BIOS, none of which we
        // understand well enough to verify ourselves.
        for &b in b"PCPUT EGT80.COM B\r" {
            m.send_key(b);
        }
        let ready = printable(&run_until_quiet(&mut m, &mut cpu, 400_000_000));
        println!("--- PCPUT ---\n{ready}");
        let (got, _) = xmodem_receive_from_guest(&mut m, &mut cpu, 4_000_000_000);
        let got = got.expect("the guest never sent the file back");
        assert!(
            got.len() >= EGT80_COM.len(),
            "got {} bytes back, sent {}",
            got.len(),
            EGT80_COM.len()
        );
        // XMODEM pads the last block, so compare only what we sent.
        assert_eq!(
            &got[..EGT80_COM.len()],
            EGT80_COM,
            "the file came back different from the one we sent"
        );
        println!("round trip: {} bytes, identical", EGT80_COM.len());

        // And it is in the image the caller would persist.
        let dirty = m.take_dirty();
        assert_eq!(dirty.len(), 1, "the written image comes back for saving");
        assert!(
            dirty[0].1.windows(5).any(|w| w == b"EGT80"),
            "the directory entry is in the image we would write out"
        );
    }

    /// Type at a booted guest and let it settle.
    #[cfg(test)]
    fn type_at(m: &mut BootMachine, cpu: &mut Cpu, keys: &[u8], budget: u64) -> String {
        for &b in keys {
            m.send_key(b);
        }
        printable(&run_until_quiet(m, cpu, budget))
    }

    /// Boot a disk, pull EGT80 onto it with the guest's own `PCGET`, and hand
    /// back the resulting image.
    ///
    /// Done once and reused, because it is the slow part and every case below
    /// wants the same disk.
    #[cfg(test)]
    fn image_with_egt80(path: &str, egt80: &[u8]) -> Vec<u8> {
        let mut m = BootMachine::new();
        m.insert(0, std::fs::read(path).unwrap(), false).expect("an 88-DCDD image");
        assert_eq!(
            m.attach_modem(crate::cpm::resolve_access("altair_2sio2")),
            ModemAttach::Ports(0x12, 0x13)
        );
        m.modem().set_carrier(true);
        let mut cpu = BootMachine::new_cpu();
        m.boot(&mut cpu, 0).expect("boots");
        run_until_quiet(&mut m, &mut cpu, 60_000_000);
        type_at(&mut m, &mut cpu, b"PCGET EGT80.COM B\r", 200_000_000);
        let (done, _) = xmodem_send_to_guest(&mut m, &mut cpu, egt80, 4_000_000_000);
        assert!(done, "could not put EGT80 on the disk");
        run_until_quiet(&mut m, &mut cpu, 400_000_000);
        m.take_dirty().pop().expect("the guest wrote the image").1
    }

    /// **Every comms port EGT80 offers, driven from inside a booted disk.**
    ///
    /// The question this answers is not "does the modem work" — `PCGET` already
    /// showed that — but "does the port the *operator* picks in EGT80 line up
    /// with the port they picked in the gateway". Those are two independent
    /// settings that have to name the same hardware, and nothing until now
    /// checked that they do.
    ///
    /// Each case boots the disk fresh, runs EGT80, walks its menus to select a
    /// port, and then moves bytes both ways over that port. The mismatch case
    /// at the end is the control: if it passed, the others would prove nothing,
    /// because a modem answering at every address would satisfy them all.
    ///
    /// Ignored: set `CPM_BOOT_IMAGE` to an Altair CP/M image carrying PCGET.COM.
    #[test]
    #[ignore]
    fn test_egt80_comms_ports_inside_a_booted_disk() {
        const EGT80_COM: &[u8] = include_bytes!("../../EGT80/EGT80.COM");
        let Ok(path) = std::env::var("CPM_BOOT_IMAGE") else {
            eprintln!("set CPM_BOOT_IMAGE to an Altair CP/M image carrying PCGET.COM");
            return;
        };
        let disk = image_with_egt80(&path, EGT80_COM);
        println!("EGT80 is on the disk; now testing its ports.\n");

        // (gateway profile, EGT80 menu keys, what its Port: line should say,
        //  whether the two should reach each other)
        // The expected text is EGT80's own "Port:" wording — the chip family
        // *and* the address — because a bare address matches all sorts of
        // unrelated things on that screen and would let a case pass without the
        // port having been selected at all.
        let cases: &[(&str, &[u8], &str, bool)] = &[
            // The pairing a booted Altair wants: 2SIO port B at both ends.
            ("altair_2sio2", b"SP32", "6850 ACIA at 12", true),
            // The gateway's own default port, which EGT80 offers by our name.
            ("rc2014_1b", b"SP1", "Z80 SIO/2 at 82", true),
            // The original MITS board.
            // `4` then `1`: EGT80 asks which address, as it does for the 2SIO.
            ("altair_sio", b"SP41", "Altair 88-SIO at 00", true),
            // The control: both ends working, aimed at different addresses.
            ("altair_2sio2", b"SP1", "Z80 SIO/2 at 82", false),
        ];

        for (uart, keys, want_port, should_reach) in cases {
            let mut m = BootMachine::new();
            m.insert(0, disk.clone(), true).unwrap();
            let attach = m.attach_modem(crate::cpm::resolve_access(uart));
            assert!(
                matches!(attach, ModemAttach::Ports(_, _)),
                "{uart} should be usable in a booted machine, got {attach:?}"
            );
            m.modem().set_carrier(true);

            let mut cpu = BootMachine::new_cpu();
            m.boot(&mut cpu, 0).expect("boots");
            run_until_quiet(&mut m, &mut cpu, 60_000_000);

            let start = type_at(&mut m, &mut cpu, b"EGT80\r", 400_000_000);
            assert!(
                start.contains("Ethernet Gateway Terminal"),
                "EGT80 did not start under {uart}: {start:?}"
            );

            // Settings -> Serial port -> the choice for this case.
            let picked = type_at(&mut m, &mut cpu, keys, 200_000_000);
            if !picked.contains(want_port) {
                // The screen is the evidence: a menu that asked something we
                // did not answer looks identical to a port that did not take.
                println!("--- {uart}: EGT80 after {} ---\n{picked}", String::from_utf8_lossy(keys));
                panic!("EGT80 does not report {want_port:?} under {uart}");
            }

            // Back out to the main menu and into terminal mode.
            type_at(&mut m, &mut cpu, b"Q", 100_000_000);
            type_at(&mut m, &mut cpu, b"T", 100_000_000);

            // Peer -> guest: what we queue should appear on EGT80's screen.
            m.modem().queue_rx(b"PING-FROM-GATEWAY");
            let seen = printable(&run_until_quiet(&mut m, &mut cpu, 200_000_000));

            // Guest -> peer: what we type should leave through the modem.
            let _ = m.modem().drain_tx();
            type_at(&mut m, &mut cpu, b"xyz", 100_000_000);
            let sent = String::from_utf8_lossy(&m.modem().drain_tx()).to_string();

            let reached = seen.contains("PING-FROM-GATEWAY");
            let replied = sent.contains("xyz");
            println!(
                "  {uart:<14} EGT80 {:<6} port {want_port}: in={} out={}",
                String::from_utf8_lossy(keys),
                if reached { "yes" } else { "no " },
                if replied { "yes" } else { "no" },
            );
            if *should_reach {
                assert!(reached, "{uart}: EGT80 never showed what we sent — {seen:?}");
                assert!(replied, "{uart}: typing never reached the modem — {sent:?}");
            } else {
                assert!(
                    !reached && !replied,
                    "{uart} answered a port EGT80 was not pointed at — \
                     the matching cases prove nothing if this one passes"
                );
            }
        }
    }

    /// The deep test of the port a booted Altair actually uses: move a real
    /// file through **EGT80's own XMODEM**, at volume, and read it back.
    ///
    /// The port matrix proves each port is wired to the right addresses, but it
    /// does so with a burst of a few bytes. That is not the same as a working
    /// link: a UART that drops a byte under load, or gets its transmit-ready
    /// bit wrong, passes a short burst and fails a file. This sends 4 KB — 32
    /// XMODEM blocks, each acknowledged — through the terminal we ship, has the
    /// guest write it to its own disk, and then reads it back with `PCPUT` and
    /// compares. Every byte has to survive EGT80's receiver, our modem rings,
    /// the guest's filesystem and the 88-DCDD write path.
    ///
    /// Ignored: set `CPM_BOOT_IMAGE` to an Altair CP/M image carrying PCGET.COM.
    #[test]
    #[ignore]
    fn test_egt80_transfers_a_file_over_2sio2() {
        const EGT80_COM: &[u8] = include_bytes!("../../EGT80/EGT80.COM");
        let Ok(path) = std::env::var("CPM_BOOT_IMAGE") else {
            eprintln!("set CPM_BOOT_IMAGE to an Altair CP/M image carrying PCGET.COM");
            return;
        };
        // Deliberately not EGT80's own bytes: a file that happened to be left
        // on the disk would otherwise let this pass without transferring
        // anything. A pattern that is not 8.3-ish text also shows up plainly if
        // it lands in the wrong place.
        let payload: Vec<u8> = (0..4096u32).map(|i| (i as u8) ^ 0x5A).collect();

        let disk = image_with_egt80(&path, EGT80_COM);
        let mut m = BootMachine::new();
        m.insert(0, disk, false).unwrap();
        assert_eq!(
            m.attach_modem(crate::cpm::resolve_access("altair_2sio2")),
            ModemAttach::Ports(0x12, 0x13)
        );
        m.modem().set_carrier(true);
        let mut cpu = BootMachine::new_cpu();
        m.boot(&mut cpu, 0).expect("boots");
        run_until_quiet(&mut m, &mut cpu, 60_000_000);

        // EGT80, on 88-2SIO port B.
        let start = type_at(&mut m, &mut cpu, b"EGT80\r", 400_000_000);
        assert!(start.contains("Ethernet Gateway Terminal"), "{start:?}");
        let picked = type_at(&mut m, &mut cpu, b"SP32", 200_000_000);
        assert!(picked.contains("6850 ACIA at 12"), "{picked:?}");
        type_at(&mut m, &mut cpu, b"Q", 100_000_000);

        // Its own XMODEM receive, into a file on the guest's disk.
        let ask = type_at(&mut m, &mut cpu, b"D", 200_000_000);
        assert!(
            ask.contains("Receive as which file?"),
            "EGT80 did not ask for a name: {ask:?}"
        );
        type_at(&mut m, &mut cpu, b"XFER.DAT\r", 200_000_000);

        let (done, seen) = xmodem_send_to_guest(&mut m, &mut cpu, &payload, 4_000_000_000);
        assert!(done, "EGT80 never finished receiving: {}", printable(&seen));
        let after = printable(&run_until_quiet(&mut m, &mut cpu, 400_000_000));
        println!("--- EGT80 receive ---\n{}{after}", printable(&seen));
        assert!(after.contains("Received."), "EGT80 did not report success: {after:?}");

        // Verified from the image itself rather than by driving EGT80's exit
        // path.  The first attempt typed `X` then `DIR XFER.DAT`, and when the
        // exit did not land where expected those keystrokes went into EGT80's
        // own menu — where the *echo* of what was typed contained "XFER" and
        // made the check pass while proving nothing.  The bytes the guest
        // committed to the disk cannot be faked by an echo.
        let img = m.take_dirty().pop().expect("the guest wrote the disk").1;
        let entry = img
            .windows(11)
            .position(|w| w == b"XFER    DAT")
            .expect("no directory entry for XFER.DAT on the disk");
        println!("directory entry for XFER.DAT at byte {entry}");
        assert!(
            img.windows(64).any(|w| w == &payload[..64]),
            "the file's own bytes are not on the disk"
        );
        println!("{} bytes through EGT80's own XMODEM, onto the guest's disk", payload.len());

        // NOT YET COVERED: reading it back out through EGT80 (its `U` upload)
        // or through `PCPUT`, which would also prove the guest can *read* what
        // it wrote.  `test_pcget_pulls_egt80_in_over_the_virtual_modem` already
        // does a full write-then-read round trip over this same port, so what
        // is missing here is EGT80's send path, not the port's.
    }

    /// Boot every image in a folder and print what each one says.
    ///
    /// The survey behind the single-disk test: one run tells you which
    /// operating systems reach a sign-on and which go quiet, which is the only
    /// way to see a bring-up move forwards or backwards as a whole. Ignored —
    /// set `CPM_BOOT_DIR` to a folder of `.dsk` files.
    #[test]
    #[ignore]
    fn test_boot_every_image_and_report() {
        let Ok(dir) = std::env::var("CPM_BOOT_DIR") else {
            eprintln!("set CPM_BOOT_DIR to run this");
            return;
        };
        let mut names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.to_ascii_lowercase().ends_with(".dsk"))
            .collect();
        names.sort();
        let mut spoke = 0;
        for name in &names {
            let bytes = std::fs::read(std::path::Path::new(&dir).join(name)).unwrap();
            if geometry_for(bytes.len() as u64).is_none() {
                println!("  skipped  {name}  ({} bytes — not an 88-DCDD image)", bytes.len());
                continue;
            }
            let mut m = BootMachine::new();
            m.insert(0, bytes, true).unwrap();
            let mut cpu = BootMachine::new_cpu();
            if let Err(e) = m.boot(&mut cpu, 0) {
                println!("  refused  {name}: {e}");
                continue;
            }
            let mut out = m.take_output();
            for _ in 0..20_000_000u64 {
                cpu.execute_instruction(&mut m);
                out.extend(m.take_output());
                if out.len() > 120 {
                    break;
                }
            }
            let text: String = out
                .iter()
                .map(|&b| if (0x20..0x7F).contains(&b) { b as char } else { '.' })
                .collect();
            if out.is_empty() {
                println!(
                    "  SILENT   {name}  pc={:#06x} stuck_polls={}",
                    cpu.registers().pc(),
                    m.stuck_polls()
                );
            } else {
                spoke += 1;
                println!("  spoke    {name}: {text}");
            }
        }
        assert!(spoke > 0, "no image in {dir} said anything");
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
        let mut cpu = BootMachine::new_cpu();
        match std::env::var("CPM_BOOT_STEP").ok().and_then(|s| s.parse::<u8>().ok()) {
            Some(step) => {
                m.boot_with_step(&mut cpu, 0, step).expect("boots");
                println!("(sector step {step}, forced)");
            }
            None => m.boot(&mut cpu, 0).expect("boots"),
        }

        // What the loader actually is, before watching it run.  Set
        // `CPM_BOOT_DISASM=addr:count` — the loaded boot code is the only
        // authority on what hardware it expects, and reading it is how the
        // three faults in the CP/M path were found.
        if let Ok(spec) = std::env::var("CPM_BOOT_DISASM") {
            let (addr, count) = spec.split_once(':').unwrap_or((spec.as_str(), "40"));
            let addr = u16::from_str_radix(addr.trim_start_matches("0x"), 16).unwrap();
            let count: usize = count.parse().unwrap();
            let saved = cpu.registers().pc();
            cpu.registers().set_pc(addr);
            for _ in 0..count {
                let at = cpu.registers().pc();
                println!("  {at:04x}  {}", cpu.disasm_instruction(&mut m));
            }
            cpu.registers().set_pc(saved);
        }

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
        // The oracle the plan asked for: the guest's own operating system must
        // say who it is.  This cannot be satisfied by a plausible wrong answer
        // the way a "does this look like text" check can — the controller, the
        // bootstrap, the CPU and the console all have to work for a sign-on to
        // appear at all.
        let want = std::env::var("CPM_BOOT_EXPECT").unwrap_or_else(|_| "CP/M".into());
        assert!(
            text.contains(&want),
            "the guest never said {want:?}; it printed: {text:?}"
        );
    }
}
