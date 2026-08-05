//! z80pack `cpmsim`'s simulated disk I/O.
//!
//! # This one is different from every other board here, in two ways
//!
//! **It is not hardware, and it never was.** The 88-DCDD, the 88-HDSK, the
//! Tarbell 1011 and the Cromemco 16FDC were cards you could hold, with
//! manufacturers' manuals describing bits that no disk in our sample set
//! exercises. This is a *simulator's* device interface, invented by Udo Munk for
//! z80pack's `cpmsim`, so there is no independent authority to measure against —
//! the simulator's source **is** the specification. Do not go looking for a
//! datasheet; there isn't one. That is also why this module is deliberately not
//! named after a board.
//!
//! **So it is derived work, not clean-room.** Everywhere else here — Punter,
//! HBIOS, EGT80, `image::format` — another implementation was a cross-check
//! only, never a source. That posture protects against two harms: copyright
//! entanglement, and transcribing somebody's *reading* of a spec as though it
//! were a measurement. Neither applies when the other implementation defines the
//! truth, so this one is written from z80pack's `cpmsim/srcsim/simio.c`
//! (MIT, Copyright Udo Munk) with its notice carried in `about.hbs` and
//! `THIRD-PARTY-NOTICES.md`. Cross-checked against TDISK03's own CBIOS, which is
//! the client of the interface and agrees with it.
//!
//! # The interface
//!
//! Seven ports, numbered in *decimal* in the original — which matters, because
//! `17` is `0x11` and that is the Altair console's data register:
//!
//! | Port | Meaning |
//! |------|---------|
//! | `0x0A` | drive select |
//! | `0x0B` | track |
//! | `0x0C` | sector, low byte — **1-based** |
//! | `0x0D` | command: `0` read, `1` write. Writing it *performs* the transfer |
//! | `0x0E` | status of the last transfer, `0` = success |
//! | `0x0F` | DMA target address, low byte |
//! | `0x10` | DMA target address, high byte |
//! | `0x11` | sector, high byte (only the large medium needs it) |
//!
//! `offset = (track * sectors_per_track + sector - 1) * 128`
//!
//! # It is a DMA controller, and that is the only structural cost
//!
//! There is no data port. Writing the command register moves 128 bytes straight
//! between the image and *guest memory* at the address the guest latched, which
//! is why [`HostRequest::Dma`] had to exist: every other board here fills its own
//! buffer and lets the guest clock bytes out through a register.
//!
//! # Why it cannot share a machine with the Altair boards
//!
//! Its ports collide with theirs — `0x0A` is the 88-DCDD's data register, and
//! `0x10`/`0x11` are the 88-2SIO console. A machine carries either these boards
//! or those, which is what the board list on
//! [`super::console::MachineChoice`] is for. Before that existed, the two would
//! have been in one machine and the controller dispatch, which runs before the
//! console, would have silently shadowed the console of every Altair disk.

use super::controller::{ColdStart, Controller, HostRequest, Medium};

/// Bytes in a sector. The same 128 as everything else in CP/M's world.
const SECTOR_LEN: usize = 128;

/// The 8" single-density medium: 77 tracks of 26 sectors.
///
/// The same 256,256 bytes as an IBM 3740 — and the same *layout*, which is worth
/// stating because it was briefly got wrong: the disks' own BIOS translates
/// sectors through the standard 6-way IBM table (its `SECTRAN` has two branches,
/// and the interesting one goes via the drive's translate table). None of that
/// is this board's business, though: the guest's BIOS does the translation and
/// asks for a physical sector, exactly as on every other board here.
const FLOPPY_TRACKS: u32 = 77;
const FLOPPY_SECTORS: u32 = 26;

/// The large medium `cpmsim` calls a hard disk: 255 tracks of 128 sectors.
const HARD_TRACKS: u32 = 255;
const HARD_SECTORS: u32 = 128;

/// What went wrong with the last transfer, in the original's own numbering.
///
/// Kept as its numbers rather than an enum of our own because a guest reads them
/// and TDISK03's BIOS tests for zero.
///
/// Our image is in memory, so there is no host `lseek`/`read`/`write` to fail —
/// but 4, 5 and 6 are still reachable, and skipping them was a bug. The
/// original's bounds checks are inclusive of the last track and sector (`>` not
/// `>=`) and let sector 0 through, and it discovers both at the file operation:
/// a negative seek reports 4, and a short read or write reports 5 or 6. Those are
/// the answers a guest is entitled to, so `command` produces them from arithmetic
/// instead of from a failed syscall.
mod status {
    pub const OK: u8 = 0;
    pub const NO_DISK: u8 = 1;
    pub const BAD_TRACK: u8 = 2;
    pub const BAD_SECTOR: u8 = 3;
    pub const SEEK_FAILED: u8 = 4;
    pub const READ_FAILED: u8 = 5;
    pub const WRITE_FAILED: u8 = 6;
    pub const BAD_COMMAND: u8 = 7;
}

/// One drive's medium.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Geometry {
    tracks: u32,
    sectors: u32,
}

impl Geometry {
    const FLOPPY: Geometry = Geometry { tracks: FLOPPY_TRACKS, sectors: FLOPPY_SECTORS };
    const HARD: Geometry = Geometry { tracks: HARD_TRACKS, sectors: HARD_SECTORS };

    fn image_len(self) -> u64 {
        self.tracks as u64 * self.sectors as u64 * SECTOR_LEN as u64
    }

    /// Where a sector lives. Sectors are numbered from 1.
    fn offset(self, track: u8, sector: u16) -> u64 {
        (track as u64 * self.sectors as u64 + sector as u64 - 1) * SECTOR_LEN as u64
    }
}

/// Which medium an image of this size is.
fn geometry_for(image_len: u64) -> Option<Geometry> {
    [Geometry::FLOPPY, Geometry::HARD].into_iter().find(|g| g.image_len() == image_len)
}

/// z80pack `cpmsim`'s disk device.
pub struct Z80pack {
    /// Latched registers. Plain values because that is all they are — this
    /// device has no state machine, no rotation and no command typing.
    drive: u8,
    track: u8,
    sector: u16,
    dma: u16,
    status: u8,
    /// What is in each drive, by drive number.
    disks: Vec<Option<Geometry>>,
    /// A write's bytes, handed up by the machine and handed back to it.
    ///
    /// Present only because [`Controller`] routes a write through
    /// [`Controller::buffer`]; a DMA write is otherwise memory-to-image and
    /// never passes through here.
    buf: Vec<u8>,
}

/// Drives the device addresses. `cpmsim` offers sixteen; a booted machine here
/// hands out as many as it has mounted images for.
const DRIVES: usize = 16;

impl Z80pack {
    pub fn new() -> Z80pack {
        Z80pack {
            drive: 0,
            track: 0,
            sector: 1,
            dma: 0,
            // Power-on status is success, matching the original's zero-initialised
            // static. A guest that reads status before commanding anything sees
            // "fine", which is what it would see there.
            status: status::OK,
            disks: (0..DRIVES).map(|_| None).collect(),
            buf: vec![0; SECTOR_LEN],
        }
    }

    /// The geometry of the disk in the selected drive, if any.
    fn selected(&self) -> Option<Geometry> {
        self.disks.get(self.drive as usize).copied().flatten()
    }

    /// Work out what the command register's write means, setting `status`.
    ///
    /// The checks and their order are the original's, and the order is load
    /// bearing: a guest that gets "bad track" rather than "no disk" would report
    /// the wrong fault to its user.
    fn command(&mut self, cmd: u8) -> HostRequest {
        let Some(geom) = self.selected() else {
            self.status = status::NO_DISK;
            return HostRequest::None;
        };
        // `>` and not `>=`, deliberately: the original compares against the
        // track and sector *counts* this way, so track 77 of a 77-track disk is
        // accepted. Tightening it would refuse a request a real cpmsim serves,
        // and a guest is entitled to whatever that one does.
        if self.track as u32 > geom.tracks {
            self.status = status::BAD_TRACK;
            return HostRequest::None;
        }
        if self.sector as u32 > geom.sectors {
            self.status = status::BAD_SECTOR;
            return HostRequest::None;
        }
        // Sector **zero**, which the original's own checks let through: sectors
        // are 1-based, so its `sector - 1` goes negative, its `lseek` fails, and
        // it reports "seek failed". Ours would underflow a `u64` instead —
        // a panic in a debug build, a wild offset in a release one — and a guest
        // reaches it with nothing more exotic than `OUT (0Ch),0` before a
        // command. Report what the original reports.
        if self.sector == 0 {
            self.status = status::SEEK_FAILED;
            return HostRequest::None;
        }
        let offset = geom.offset(self.track, self.sector);
        // The last track and the last sector are *inclusive* in the checks above,
        // because the original compares with `>` rather than `>=` — which means
        // track 77 of a 77-track disk (tracks are numbered from 0) is accepted
        // and addresses one sector past the medium. The original discovers that
        // at its `read`/`write` and reports an I/O error; without this we would
        // hand the guest an erased sector and call it success, or accept a write
        // that goes nowhere. Faithful *and* bounded.
        if offset + SECTOR_LEN as u64 > geom.image_len() {
            self.status = if cmd == 1 { status::WRITE_FAILED } else { status::READ_FAILED };
            return HostRequest::None;
        }
        match cmd {
            0 => {
                self.status = status::OK;
                HostRequest::Dma {
                    drive: self.drive,
                    offset,
                    len: SECTOR_LEN,
                    addr: self.dma,
                    to_memory: true,
                }
            }
            1 => {
                self.status = status::OK;
                HostRequest::Dma {
                    drive: self.drive,
                    offset,
                    len: SECTOR_LEN,
                    addr: self.dma,
                    to_memory: false,
                }
            }
            _ => {
                self.status = status::BAD_COMMAND;
                HostRequest::None
            }
        }
    }
}

impl Controller for Z80pack {
    fn name(&self) -> &'static str {
        "z80pack cpmsim disk"
    }

    fn owns_port(&self, port: u8) -> bool {
        (0x0A..=0x11).contains(&port)
    }

    fn port_in(&mut self, port: u8) -> (u8, HostRequest) {
        // Every register reads back, as in the original. Nothing here has a
        // side effect: the transfer happens on the command *write*.
        let v = match port {
            0x0A => self.drive,
            0x0B => self.track,
            0x0C => self.sector as u8,
            0x0D => 0,
            0x0E => self.status,
            0x0F => self.dma as u8,
            0x10 => (self.dma >> 8) as u8,
            0x11 => (self.sector >> 8) as u8,
            _ => 0,
        };
        (v, HostRequest::None)
    }

    fn port_out(&mut self, port: u8, value: u8) -> HostRequest {
        match port {
            0x0A => self.drive = value,
            0x0B => self.track = value,
            0x0C => self.sector = (self.sector & 0xFF00) | value as u16,
            0x0D => return self.command(value),
            0x0F => self.dma = (self.dma & 0xFF00) | value as u16,
            0x10 => self.dma = (self.dma & 0x00FF) | ((value as u16) << 8),
            0x11 => self.sector = (self.sector & 0x00FF) | ((value as u16) << 8),
            // Port 0x0E is the status register, and writing it does nothing
            // here. The original has a handler for it that belongs to its
            // virtual-hardware control, not to the disk.
            _ => {}
        }
        HostRequest::None
    }

    fn media(&self) -> Vec<Medium> {
        vec![
            Medium {
                bytes: Geometry::FLOPPY.image_len(),
                label: "z80pack 8\" SSSD, 241K",
                // Exact. Unlike the Altair images, these are produced by the
                // simulator itself and are never padded — and a trailer
                // allowance here would start claiming Tarbell disks that some
                // tool had padded.
                trailer: 0,
                shape: format!("{FLOPPY_TRACKS} tracks x {FLOPPY_SECTORS} sectors x {SECTOR_LEN}"),
            },
            Medium {
                bytes: Geometry::HARD.image_len(),
                label: "z80pack large disk, 4M",
                trailer: 0,
                shape: format!("{HARD_TRACKS} tracks x {HARD_SECTORS} sectors x {SECTOR_LEN}"),
            },
        ]
    }

    fn insert(&mut self, drive: u8, image_len: u64, _read_only: bool) -> Result<(), String> {
        let geom = geometry_for(image_len)
            .ok_or_else(|| format!("{image_len} bytes is not a z80pack disk"))?;
        let slot = self
            .disks
            .get_mut(drive as usize)
            .ok_or_else(|| format!("this device has drives 0-{}", DRIVES - 1))?;
        *slot = Some(geom);
        Ok(())
    }

    fn buffer_loaded(&mut self, _drive: u8, bytes: &[u8]) {
        self.buf = bytes.to_vec();
    }

    fn buffer(&self, _drive: u8) -> Option<&[u8]> {
        Some(&self.buf)
    }

    /// What `cpmsim`'s bootstrap does: load track 0 sector 1 at `0000` and
    /// enter there.
    ///
    /// Confirmed against the disk rather than taken on trust — TDISK03's first
    /// sector begins `C3 19 00`, a jump over its own "BOOT: error booting"
    /// message to the loader at `0019`, which is only sensible if the sector is
    /// entered at its first byte.
    fn cold_start(&self, image: &[u8]) -> ColdStart {
        if image.len() < SECTOR_LEN {
            return ColdStart::NoProgram;
        }
        ColdStart::Program { offset: 0, len: SECTOR_LEN, load: 0x0000, entry: 0x0000 }
    }

    /// Nothing to leave behind.
    ///
    /// Asked anyway, because the Tarbell taught that a synthesised cold start
    /// must leave the board as the real one would. Here the real one is a
    /// simulator whose registers are zero-initialised and whose guest latches
    /// every one of them before its first command — and `status` is only read
    /// after a command. So there is genuinely nothing, and that is a finding
    /// rather than an omission.
    fn cold_started(&mut self, _drive: u8) {}

    fn stuck_polls(&self) -> u32 {
        // A guest cannot wait on this device: writing the command register
        // completes the transfer, so there is no ready flag to spin on and no
        // rotation to miss. The one wait this machine has is its console.
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both media are recognised by size, and nothing else is.
    #[test]
    fn test_media_are_recognised_by_size() {
        assert_eq!(geometry_for(256_256), Some(Geometry::FLOPPY));
        assert_eq!(geometry_for(4_177_920), Some(Geometry::HARD));
        assert_eq!(geometry_for(256_255), None);
        assert_eq!(geometry_for(0), None);
        // The trailer allowance the Altair boards need must NOT be here: a
        // padded Tarbell image is not a z80pack disk.
        assert_eq!(geometry_for(256_256 + 96), None);
    }

    /// Sectors are numbered from 1, and the arithmetic is the original's.
    #[test]
    fn test_sector_offsets_are_one_based() {
        let g = Geometry::FLOPPY;
        assert_eq!(g.offset(0, 1), 0, "track 0 sector 1 is the first byte");
        assert_eq!(g.offset(0, 2), 128);
        assert_eq!(g.offset(1, 1), 26 * 128, "track 1 starts a track in");
        assert_eq!(g.offset(2, 1), 52 * 128, "the CP/M directory's track");
        assert_eq!(g.offset(76, 26), 256_256 - 128, "the last sector");
    }

    fn with_floppy() -> Z80pack {
        let mut c = Z80pack::new();
        c.insert(0, 256_256, false).unwrap();
        c
    }

    /// A read latches four registers and then the command register performs it.
    #[test]
    fn test_a_read_asks_for_a_dma_into_guest_memory() {
        let mut c = with_floppy();
        c.port_out(0x0A, 0); // drive 0
        c.port_out(0x0B, 2); // track 2
        c.port_out(0x0C, 1); // sector 1
        c.port_out(0x0F, 0x80); // DMA low
        c.port_out(0x10, 0x00); // DMA high
        let req = c.port_out(0x0D, 0); // read
        assert_eq!(
            req,
            HostRequest::Dma {
                drive: 0,
                offset: 52 * 128,
                len: 128,
                addr: 0x0080,
                to_memory: true
            }
        );
        assert_eq!(c.port_in(0x0E).0, status::OK);
    }

    /// A write is the same request the other way round.
    #[test]
    fn test_a_write_asks_for_a_dma_out_of_guest_memory() {
        let mut c = with_floppy();
        c.port_out(0x0B, 0);
        c.port_out(0x0C, 3);
        c.port_out(0x0F, 0x00);
        c.port_out(0x10, 0x30);
        let req = c.port_out(0x0D, 1);
        assert_eq!(
            req,
            HostRequest::Dma {
                drive: 0,
                offset: 2 * 128,
                len: 128,
                addr: 0x3000,
                to_memory: false
            }
        );
        assert_eq!(c.port_in(0x0E).0, status::OK);
    }

    /// The sector register is sixteen bits, and the high byte is a separate
    /// port. Only the large medium needs it, and getting the two halves the
    /// wrong way round would read plausible-but-wrong sectors.
    #[test]
    fn test_the_sector_register_is_sixteen_bits() {
        let mut c = Z80pack::new();
        c.insert(0, 4_177_920, false).unwrap();
        c.port_out(0x0C, 0x02); // low
        c.port_out(0x11, 0x00); // high
        assert_eq!(c.sector, 2);
        c.port_out(0x0C, 0x80);
        assert_eq!(c.sector, 0x0080, "the high byte must be preserved");
        // 128 sectors per track, so sector 128 is the last legal one.
        c.port_out(0x0B, 1);
        let req = c.port_out(0x0D, 0);
        assert!(matches!(req, HostRequest::Dma { offset, .. } if offset == (128 + 127) * 128));
        assert_eq!(c.port_in(0x0E).0, status::OK);
        // And reading the halves back gives what was written.
        assert_eq!(c.port_in(0x0C).0, 0x80);
        assert_eq!(c.port_in(0x11).0, 0x00);
    }

    /// The DMA address is likewise two ports, low then high, and each must
    /// leave the other alone.
    #[test]
    fn test_the_dma_address_halves_are_independent() {
        let mut c = with_floppy();
        c.port_out(0x0F, 0x34);
        c.port_out(0x10, 0x12);
        assert_eq!(c.dma, 0x1234);
        c.port_out(0x0F, 0xCD);
        assert_eq!(c.dma, 0x12CD, "writing low must not clear high");
        c.port_out(0x10, 0xAB);
        assert_eq!(c.dma, 0xABCD, "writing high must not clear low");
        assert_eq!(c.port_in(0x0F).0, 0xCD);
        assert_eq!(c.port_in(0x10).0, 0xAB);
    }

    /// Every refusal the device can report, with the original's numbers — a
    /// guest reads these and reports them to a person.
    #[test]
    fn test_the_status_numbers_are_the_originals() {
        let mut c = with_floppy();
        // An empty drive.
        c.port_out(0x0A, 5);
        assert_eq!(c.port_out(0x0D, 0), HostRequest::None);
        assert_eq!(c.port_in(0x0E).0, status::NO_DISK);

        // Past the last track, in two stages that must stay distinguishable.
        // The original's track check compares with `>`, so track 77 of a
        // 77-track disk passes it — and is then caught addressing one sector
        // past the medium, which the original reports as an I/O error rather
        // than a bad track. Track 78 is the first the track check itself
        // refuses.
        c.port_out(0x0A, 0);
        c.port_out(0x0B, 77);
        c.port_out(0x0C, 1);
        assert_eq!(c.port_out(0x0D, 0), HostRequest::None);
        assert_eq!(
            c.port_in(0x0E).0,
            status::READ_FAILED,
            "track 77 passes the track check and fails on the medium's end"
        );
        c.port_out(0x0B, 78);
        assert_eq!(c.port_out(0x0D, 0), HostRequest::None);
        assert_eq!(c.port_in(0x0E).0, status::BAD_TRACK, "and 78 is a bad track");

        // Past the last sector, 26 being allowed for the same reason.
        c.port_out(0x0B, 0);
        c.port_out(0x0C, 26);
        assert!(matches!(c.port_out(0x0D, 0), HostRequest::Dma { .. }), "sector 26 is allowed");
        c.port_out(0x0C, 27);
        assert_eq!(c.port_out(0x0D, 0), HostRequest::None);
        assert_eq!(c.port_in(0x0E).0, status::BAD_SECTOR);

        // A command that is neither read nor write.
        c.port_out(0x0C, 1);
        assert_eq!(c.port_out(0x0D, 2), HostRequest::None);
        assert_eq!(c.port_in(0x0E).0, status::BAD_COMMAND);
    }

    /// **Sector zero must not be able to reach the arithmetic.**
    ///
    /// Sectors are 1-based, so `sector - 1` on zero underflows — a panic in a
    /// debug build, a wild offset in a release one. A guest reaches it with
    /// `OUT (0Ch),0` before a command, which is nothing exotic. The original
    /// lets its own checks pass and finds out at its `lseek`, reporting 4, so
    /// that is what we report.
    #[test]
    fn test_sector_zero_is_refused_rather_than_underflowing() {
        let mut c = with_floppy();
        c.port_out(0x0B, 0);
        c.port_out(0x0C, 0);
        c.port_out(0x11, 0);
        assert_eq!(c.port_out(0x0D, 0), HostRequest::None, "must not produce a transfer");
        assert_eq!(c.port_in(0x0E).0, status::SEEK_FAILED);
        // And on a write, which takes the same path.
        assert_eq!(c.port_out(0x0D, 1), HostRequest::None);
        assert_eq!(c.port_in(0x0E).0, status::SEEK_FAILED);
        // Sector 1 at the same track is fine, so nothing broader was broken.
        c.port_out(0x0C, 1);
        assert!(matches!(c.port_out(0x0D, 0), HostRequest::Dma { offset: 0, .. }));
    }

    /// The last track is *inclusive* in the original's checks, so it addresses
    /// one sector past the medium — and that must be an I/O error rather than a
    /// silent success handing back an erased sector.
    #[test]
    fn test_a_transfer_past_the_end_of_the_medium_is_an_error() {
        let mut c = with_floppy();
        // Track 77 of a 77-track disk: tracks run 0..=76, so this is past it,
        // and the track check above deliberately allows it.
        c.port_out(0x0B, 77);
        c.port_out(0x0C, 1);
        assert_eq!(c.port_out(0x0D, 0), HostRequest::None);
        assert_eq!(c.port_in(0x0E).0, status::READ_FAILED, "a read past the end");
        assert_eq!(c.port_out(0x0D, 1), HostRequest::None);
        assert_eq!(c.port_in(0x0E).0, status::WRITE_FAILED, "a write past the end");

        // The genuinely last sector still works.
        c.port_out(0x0B, 76);
        c.port_out(0x0C, 26);
        assert!(
            matches!(c.port_out(0x0D, 0), HostRequest::Dma { offset, .. } if offset == 256_256 - 128)
        );
        assert_eq!(c.port_in(0x0E).0, status::OK);
    }

    /// The same, on the large medium: 255 tracks numbered 0..=254.
    #[test]
    fn test_the_large_medium_is_bounded_too() {
        let mut c = Z80pack::new();
        c.insert(0, 4_177_920, false).unwrap();
        c.port_out(0x0B, 255);
        c.port_out(0x0C, 1);
        c.port_out(0x11, 0);
        assert_eq!(c.port_out(0x0D, 0), HostRequest::None);
        assert_eq!(c.port_in(0x0E).0, status::READ_FAILED);
        // Its real last sector: track 254, sector 128.
        c.port_out(0x0B, 254);
        c.port_out(0x0C, 128);
        assert!(matches!(
            c.port_out(0x0D, 0),
            HostRequest::Dma { offset, .. } if offset == 4_177_920 - 128
        ));
    }

    /// The ports it claims, and — the point of the board list — the ones it
    /// must not be sharing a machine with.
    #[test]
    fn test_the_ports_it_claims() {
        let c = Z80pack::new();
        for p in 0x0A..=0x11 {
            assert!(c.owns_port(p), "{p:#04x} is one of its registers");
        }
        assert!(!c.owns_port(0x09), "below its range");
        assert!(!c.owns_port(0x12), "above its range");
        // Documenting the collisions that make this a machine of its own: the
        // 88-DCDD's data register and the 88-2SIO console both fall inside.
        assert!(c.owns_port(0x0A), "the 88-DCDD's data port");
        assert!(c.owns_port(0x10) && c.owns_port(0x11), "the 88-2SIO console");
    }

    /// The bootstrap loads one sector at zero and enters it there.
    #[test]
    fn test_cold_start_loads_the_first_sector_at_zero() {
        let c = Z80pack::new();
        let img = vec![0u8; 256_256];
        assert_eq!(
            c.cold_start(&img),
            ColdStart::Program { offset: 0, len: 128, load: 0, entry: 0 }
        );
        // Too small to hold a boot sector at all.
        assert_eq!(c.cold_start(&[0u8; 8]), ColdStart::NoProgram);
    }

    /// A drive number past the device's own count is refused rather than
    /// silently dropped.
    #[test]
    fn test_an_impossible_drive_is_refused() {
        let mut c = Z80pack::new();
        assert!(c.insert(16, 256_256, false).is_err());
        assert!(c.insert(15, 256_256, false).is_ok());
    }
}
