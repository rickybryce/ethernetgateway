//! MITS 88-DCDD floppy controller.
//!
//! Enough of the controller for a disk image to boot its own operating system,
//! so the guest's BIOS does the sector translation and the filesystem work
//! instead of us. That is the whole point: the layout knowledge is already on
//! the disk, in code that works, and reverse-engineering it has a poor record
//! here — the Altair 88-DCDD *filesystem* is still unsupported after six ruled
//! out hypotheses (see `image::format`). A controller sidesteps all of it, and
//! reaches the disks that hold Altair DOS, Disk BASIC or Time Sharing BASIC
//! rather than CP/M at all.
//!
//! # The register interface
//!
//! Three ports. The behaviour below is the *hardware's*, taken from the
//! published description of the 88-DCDD and cross-checked against the Altair
//! 8800 simulator's account of the same registers — which is a fair source for
//! this precisely because it describes hardware and not a filesystem. The code
//! here is our own.
//!
//! ```text
//!  OUT 08h  drive select    bit 7 = deselect/clear, bits 0-3 = drive
//!  OUT 09h  drive control   bit 0 step in     bit 1 step out
//!                           bit 2 head load   bit 3 head unload
//!                           bit 4 int enable  bit 5 int disable
//!                           bit 6 lower head current (ignored)
//!                           bit 7 begin write
//!  OUT 0Ah  write data
//!
//!  IN  08h  status          active LOW — the byte is returned inverted
//!  IN  09h  sector position bits 1-5 = sector number x 2, bit 0 = "sector
//!                           true" (0 when the sector is under the head)
//!  IN  0Ah  read data
//! ```
//!
//! # Rotation, and why it is the first thing built here
//!
//! A real disk turns. Software waits for the sector it wants to come round by
//! polling the position register, so **the position must advance on its own or
//! every guest hangs in a poll loop** — and a hung guest looks exactly like a
//! runaway program, which is a failure this project has already misdiagnosed
//! once. So rotation is modelled first, and tested first.
//!
//! The model: each read of the position register flips a "sector true" flag,
//! and the sector advances on the flip *to false*. That gives every sector two
//! reads at the head — one where it is positioned and one where it is not —
//! which matters because some software waits for the flag to go false rather
//! than true. MBASIC saving a file under CP/M is the known example, and a
//! controller that only ever reports "positioned" leaves it spinning forever.

// Staged build: the controller and its tests are complete, but the boot driver
// that drives it — cold start, the CPU handover, the console wiring — is the
// next step, so nothing outside this module calls it yet. CI treats warnings as
// errors, hence the allow. **Remove it when the driver lands**; a blanket allow
// left behind goes on hiding real dead code, which is exactly what happened to
// the image module before its UIs arrived.
#![allow(dead_code)]

/// Bytes in one physical sector on the wire, header and trailer included.
pub const SECTOR_LEN: usize = 137;

/// Sectors per track on an 8" disk.
pub const SECTORS_PER_TRACK: u8 = 32;

/// Tracks on an 8" disk.
pub const TRACKS: u8 = 77;

/// Sectors per track on a 5.25" minidisk.
pub const MINI_SECTORS_PER_TRACK: u8 = 16;

/// Tracks on a 5.25" minidisk.
pub const MINI_TRACKS: u8 = 35;

/// Drives the controller can address. The select register carries four bits,
/// so sixteen is the architectural limit; real machines had far fewer.
pub const MAX_DRIVES: usize = 16;

/// Which physical geometry a mounted disk has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    pub tracks: u8,
    pub sectors: u8,
}

impl Geometry {
    /// The 8" disk an Altair 88-DCDD normally carries.
    pub const EIGHT_INCH: Geometry = Geometry { tracks: TRACKS, sectors: SECTORS_PER_TRACK };
    /// The 5.25" minidisk.
    pub const MINIDISK: Geometry =
        Geometry { tracks: MINI_TRACKS, sectors: MINI_SECTORS_PER_TRACK };

    /// Byte offset of a sector in the image file.
    ///
    /// Sectors are stored in physical rotation order with nothing between them,
    /// so this is the whole of the addressing — the controller hands the guest
    /// all 137 bytes and lets its BIOS decide which of them are data.
    pub fn offset(&self, track: u8, sector: u8) -> u64 {
        track as u64 * self.sectors as u64 * SECTOR_LEN as u64 + sector as u64 * SECTOR_LEN as u64
    }

    /// Bytes an image of this geometry occupies.
    pub fn image_len(&self) -> u64 {
        self.tracks as u64 * self.sectors as u64 * SECTOR_LEN as u64
    }
}

/// Status bits, before the register is inverted for the negative-logic bus.
///
/// Named rather than written as literals at the point of use: the register is
/// returned inverted, and a bare hex constant next to a `!` is unreadable.
mod status {
    /// A new byte is available to read.
    pub const NRDA: u8 = 0x80;
    /// The head is on track 0.
    pub const TRACK0: u8 = 0x40;
    /// Interrupts are enabled.
    pub const INT_ENABLED: u8 = 0x20;
    /// The head is loaded — set means loaded.
    pub const HEAD_LOADED: u8 = 0x04;
    /// It is safe to move the head.
    pub const MOVE_OK: u8 = 0x02;
    /// The controller wants the next byte of write data.
    pub const ENWD: u8 = 0x01;
    /// Bits a ready drive reports as unused.
    pub const READY_UNUSED: u8 = 0x18;
}

/// Control-register bits, written to port 09h.
mod control {
    pub const STEP_IN: u8 = 0x01;
    pub const STEP_OUT: u8 = 0x02;
    pub const HEAD_LOAD: u8 = 0x04;
    pub const HEAD_UNLOAD: u8 = 0x08;
    pub const INT_ENABLE: u8 = 0x10;
    pub const INT_DISABLE: u8 = 0x20;
    pub const WRITE_ENABLE: u8 = 0x80;
}

/// One drive's mechanical state.
#[derive(Debug, Clone)]
struct Drive {
    /// Present only when a disk is loaded.
    disk: Option<Disk>,
    track: u8,
    sector: u8,
    head_loaded: bool,
    interrupts: bool,
    /// Set between "begin write" and the end of the sector.
    writing: bool,
    /// Cursor into `buffer`; `None` when no transfer is in progress.
    byte: Option<usize>,
    buffer: [u8; SECTOR_LEN],
    /// True when `buffer` holds changes not yet given back.
    dirty: bool,
}

/// A disk in a drive.
#[derive(Debug, Clone)]
pub struct Disk {
    pub geometry: Geometry,
    pub read_only: bool,
}

impl Default for Drive {
    fn default() -> Drive {
        Drive {
            disk: None,
            track: 0,
            sector: 0,
            head_loaded: false,
            interrupts: false,
            writing: false,
            byte: None,
            buffer: [0; SECTOR_LEN],
            dirty: false,
        }
    }
}

/// What the controller wants the host to do after a port access.
///
/// The controller does not own the images: it says which sector it needs and
/// the caller moves the bytes. That keeps every file access on the caller's
/// side, where the bounds checking and the read-only rules already live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    /// Nothing to do.
    None,
    /// Fill the sector buffer from this drive/track/sector.
    Read { drive: u8, track: u8, sector: u8 },
    /// Write the sector buffer back.
    Write { drive: u8, track: u8, sector: u8 },
}

/// The controller.
pub struct Dcdd {
    drives: [Drive; MAX_DRIVES],
    /// `None` when no drive is selected.
    selected: Option<u8>,
    /// The rotation flag — see the module comment.
    sector_true: bool,
    /// Counts position-register reads since the sector last changed, so a
    /// guest stuck waiting on a sector that never arrives can be spotted.
    polls_on_sector: u32,
}

impl Default for Dcdd {
    fn default() -> Dcdd {
        Dcdd::new()
    }
}

impl Dcdd {
    pub fn new() -> Dcdd {
        Dcdd {
            drives: std::array::from_fn(|_| Drive::default()),
            selected: None,
            sector_true: false,
            polls_on_sector: 0,
        }
    }

    /// Put a disk in a drive.
    pub fn insert(&mut self, drive: u8, disk: Disk) {
        if let Some(d) = self.drives.get_mut(drive as usize) {
            *d = Drive { disk: Some(disk), ..Drive::default() };
        }
    }

    /// Take a disk out.
    pub fn eject(&mut self, drive: u8) {
        if let Some(d) = self.drives.get_mut(drive as usize) {
            *d = Drive::default();
        }
    }

    /// Is a disk loaded in this drive?
    pub fn has_disk(&self, drive: u8) -> bool {
        self.drives.get(drive as usize).is_some_and(|d| d.disk.is_some())
    }

    /// The currently selected drive, if any.
    pub fn selected(&self) -> Option<u8> {
        self.selected
    }

    /// How many times the position register has been read without the sector
    /// moving on.
    ///
    /// A guest polling for a sector it will never see is the classic way this
    /// hardware hangs, and it presents as a spinning CPU rather than an error.
    /// The driver watches this so it can say what is actually happening.
    pub fn polls_on_sector(&self) -> u32 {
        self.polls_on_sector
    }

    fn cur(&self) -> Option<&Drive> {
        self.selected.and_then(|s| self.drives.get(s as usize))
    }

    fn cur_mut(&mut self) -> Option<&mut Drive> {
        let s = self.selected?;
        self.drives.get_mut(s as usize)
    }

    /// Read a port. `0x08`, `0x09` or `0x0A`.
    pub fn port_in(&mut self, port: u8) -> (u8, Request) {
        match port & 0x0F {
            0x08 => (self.read_status(), Request::None),
            0x09 => self.read_position(),
            0x0A => self.read_data(),
            _ => (0xFF, Request::None),
        }
    }

    /// Write a port.
    pub fn port_out(&mut self, port: u8, value: u8) -> Request {
        match port & 0x0F {
            0x08 => self.select(value),
            0x09 => self.control(value),
            0x0A => self.write_data(value),
            _ => Request::None,
        }
    }

    /// Status register, port 08h — **returned inverted**, because the bus is
    /// active low. Every bit below is built in positive logic and flipped once
    /// at the end, which is the only arrangement that stays readable.
    fn read_status(&self) -> u8 {
        let mut s = 0u8;
        if let Some(d) = self.cur() {
            if d.disk.is_some() {
                s |= status::READY_UNUSED;
                if d.track == 0 {
                    s |= status::TRACK0;
                }
                if d.head_loaded {
                    s |= status::HEAD_LOADED;
                    // With the head down a byte is available whenever a
                    // transfer is under way.
                    if d.byte.is_some() && !d.writing {
                        s |= status::NRDA;
                    }
                    if d.writing {
                        s |= status::ENWD;
                    }
                }
                if d.interrupts {
                    s |= status::INT_ENABLED;
                }
                // The head may move whenever it is not mid-transfer.
                if d.byte.is_none() {
                    s |= status::MOVE_OK;
                }
            }
        }
        !s
    }

    /// Sector position register, port 09h.
    ///
    /// This is where rotation happens. Reading it flips `sector_true`; the
    /// sector advances on the flip to false, so every sector is reported once
    /// as positioned and once as not. Software that waits for the flag to fall
    /// — MBASIC saving under CP/M is the known case — would hang otherwise.
    fn read_position(&mut self) -> (u8, Request) {
        let Some(sel) = self.selected else {
            return (0xFF, Request::None);
        };
        let Some(d) = self.drives.get(sel as usize) else {
            return (0xFF, Request::None);
        };
        if d.disk.is_none() || !d.head_loaded {
            return (0xFF, Request::None);
        }

        let mut request = Request::None;
        self.polls_on_sector = self.polls_on_sector.saturating_add(1);
        if self.sector_true {
            self.sector_true = false;
        } else {
            // Leaving a sector: hand back anything written to it, then step on.
            let (sectors, was_writing, track, sector) = {
                let d = &self.drives[sel as usize];
                let g = d.disk.as_ref().map(|k| k.geometry).unwrap_or(Geometry::EIGHT_INCH);
                (g.sectors, d.writing && d.dirty, d.track, d.sector)
            };
            if was_writing {
                request = Request::Write { drive: sel, track, sector };
            }
            let d = &mut self.drives[sel as usize];
            d.writing = false;
            d.dirty = false;
            d.sector = (d.sector + 1) % sectors.max(1);
            d.byte = None;
            self.sector_true = true;
            self.polls_on_sector = 0;
        }

        let d = &self.drives[sel as usize];
        let mut v = 0xC0 | (d.sector << 1);
        if !self.sector_true {
            v |= 0x01;
        }
        (v, request)
    }

    /// Read one byte of the current sector, port 0Ah.
    fn read_data(&mut self) -> (u8, Request) {
        let Some(sel) = self.selected else {
            return (0xFF, Request::None);
        };
        let (track, sector, need_fill) = {
            let Some(d) = self.drives.get(sel as usize) else {
                return (0xFF, Request::None);
            };
            if d.disk.is_none() || d.writing {
                return (0xFF, Request::None);
            }
            (d.track, d.sector, d.byte.is_none())
        };
        if need_fill {
            // The caller fills `buffer` and calls `sector_loaded`.
            self.drives[sel as usize].byte = Some(0);
            return (0xFF, Request::Read { drive: sel, track, sector });
        }
        let d = &mut self.drives[sel as usize];
        let i = d.byte.unwrap_or(0);
        let v = d.buffer.get(i).copied().unwrap_or(0);
        d.byte = Some((i + 1).min(SECTOR_LEN));
        (v, Request::None)
    }

    /// Give the controller the bytes it asked for.
    pub fn sector_loaded(&mut self, drive: u8, bytes: &[u8]) {
        if let Some(d) = self.drives.get_mut(drive as usize) {
            let n = bytes.len().min(SECTOR_LEN);
            d.buffer[..n].copy_from_slice(&bytes[..n]);
            d.buffer[n..].fill(0);
            d.byte = Some(0);
            d.dirty = false;
        }
    }

    /// End any transfer in progress on a drive.
    ///
    /// The mechanism reports "safe to move the head" only when it is not
    /// mid-transfer, so a transfer left open pins that bit low and a guest
    /// waiting to seek spins forever.  Real hardware finishes the sector; the
    /// bootstrap has to say so explicitly because it stops reading part way
    /// through, having taken what it needs straight from the buffer.
    pub fn end_transfer(&mut self, drive: u8) {
        if let Some(d) = self.drives.get_mut(drive as usize) {
            d.byte = None;
            d.writing = false;
        }
    }

    /// The sector buffer, for a write the controller has asked to commit.
    pub fn sector_buffer(&self, drive: u8) -> Option<&[u8; SECTOR_LEN]> {
        self.drives.get(drive as usize).map(|d| &d.buffer)
    }

    /// Drive select, port 08h.
    fn select(&mut self, value: u8) -> Request {
        let drive = value & 0x0F;
        if drive as usize >= MAX_DRIVES {
            return Request::None;
        }
        if value & 0x80 != 0 {
            // Deselect: the disk stays in, the mechanism resets.
            if let Some(d) = self.drives.get_mut(drive as usize) {
                d.head_loaded = false;
                d.writing = false;
                d.byte = None;
            }
            self.selected = None;
        } else {
            self.selected = Some(drive);
        }
        self.sector_true = false;
        self.polls_on_sector = 0;
        Request::None
    }

    /// Drive control, port 09h.
    fn control(&mut self, value: u8) -> Request {
        let Some(sel) = self.selected else {
            return Request::None;
        };
        let tracks = self.drives[sel as usize]
            .disk
            .as_ref()
            .map(|d| d.geometry.tracks)
            .unwrap_or(TRACKS);
        let d = &mut self.drives[sel as usize];
        if value & control::STEP_IN != 0 && d.track + 1 < tracks {
            d.track += 1;
            d.byte = None;
        }
        if value & control::STEP_OUT != 0 && d.track > 0 {
            d.track -= 1;
            d.byte = None;
        }
        if value & control::HEAD_LOAD != 0 {
            d.head_loaded = true;
        }
        if value & control::HEAD_UNLOAD != 0 {
            d.head_loaded = false;
            d.byte = None;
        }
        if value & control::INT_ENABLE != 0 {
            d.interrupts = true;
        }
        if value & control::INT_DISABLE != 0 {
            d.interrupts = false;
        }
        if value & control::WRITE_ENABLE != 0 {
            let read_only = d.disk.as_ref().is_some_and(|k| k.read_only);
            if !read_only {
                d.writing = true;
                d.byte = Some(0);
            }
        }
        Request::None
    }

    /// Write one byte into the current sector, port 0Ah.
    fn write_data(&mut self, value: u8) -> Request {
        let Some(sel) = self.selected else {
            return Request::None;
        };
        let d = &mut self.drives[sel as usize];
        if !d.writing {
            return Request::None;
        }
        let i = d.byte.unwrap_or(0);
        if i < SECTOR_LEN {
            d.buffer[i] = value;
            d.dirty = true;
            d.byte = Some(i + 1);
        }
        Request::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready() -> Dcdd {
        let mut c = Dcdd::new();
        c.insert(0, Disk { geometry: Geometry::EIGHT_INCH, read_only: false });
        c.port_out(0x08, 0); // select drive 0
        c.port_out(0x09, control::HEAD_LOAD);
        c
    }

    /// Sector numbers must be readable back out of the position register.
    fn sector_of(v: u8) -> u8 {
        (v >> 1) & 0x1F
    }

    // ---- rotation: built and tested first, per the module comment ----------

    /// The disk must turn on its own. A position register that never advances
    /// leaves every guest polling forever, and it presents as a spinning CPU
    /// rather than an error.
    #[test]
    fn test_the_disk_rotates_when_polled() {
        let mut c = ready();
        let mut seen = Vec::new();
        for _ in 0..64 {
            let (v, _) = c.port_in(0x09);
            seen.push(sector_of(v));
        }
        let distinct: std::collections::BTreeSet<u8> = seen.iter().copied().collect();
        assert_eq!(
            distinct.len(),
            SECTORS_PER_TRACK as usize,
            "every sector must come round; saw {distinct:?}"
        );
    }

    /// Each sector is reported once positioned and once not.
    ///
    /// Software that waits for "sector true" to *fall* — MBASIC saving under
    /// CP/M is the known case — hangs against a controller that only ever says
    /// "positioned".
    #[test]
    fn test_sector_true_falls_as_well_as_rises() {
        let mut c = ready();
        let mut trues = 0;
        let mut falses = 0;
        for _ in 0..64 {
            let (v, _) = c.port_in(0x09);
            if v & 0x01 == 0 {
                trues += 1;
            } else {
                falses += 1;
            }
        }
        assert!(trues > 0 && falses > 0, "the flag must go both ways: {trues}/{falses}");
    }

    #[test]
    fn test_sectors_advance_in_order_and_wrap() {
        let mut c = ready();
        let mut order = Vec::new();
        for _ in 0..(SECTORS_PER_TRACK as usize * 2 + 2) {
            let (v, _) = c.port_in(0x09);
            let s = sector_of(v);
            if order.last() != Some(&s) {
                order.push(s);
            }
        }
        for pair in order.windows(2) {
            let expect = (pair[0] + 1) % SECTORS_PER_TRACK;
            assert_eq!(pair[1], expect, "sector order broke at {pair:?}");
        }
    }

    /// The stuck-poll counter is what lets the driver tell "waiting for the
    /// disk" apart from "runaway program". It must reset when the disk moves.
    #[test]
    fn test_poll_counter_tracks_a_stuck_guest_and_resets_on_movement() {
        let mut c = ready();
        let mut last = None;
        let mut saw_nonzero = false;
        for _ in 0..8 {
            let (v, _) = c.port_in(0x09);
            let s = sector_of(v);
            let moved = last.is_some_and(|l| l != s);
            if moved {
                assert_eq!(c.polls_on_sector(), 0, "the counter resets when the disk moves");
            } else if last.is_some() {
                saw_nonzero |= c.polls_on_sector() > 0;
            }
            last = Some(s);
        }
        assert!(saw_nonzero, "the counter must climb while the disk sits still");
    }

    /// A drive with no disk, or with the head up, reports nothing rather than
    /// a plausible sector — otherwise a guest reads a position that cannot be.
    #[test]
    fn test_no_disk_or_head_up_reports_nothing() {
        let mut c = Dcdd::new();
        c.port_out(0x08, 0);
        assert_eq!(c.port_in(0x09).0, 0xFF, "no disk");
        c.insert(0, Disk { geometry: Geometry::EIGHT_INCH, read_only: false });
        c.port_out(0x08, 0);
        assert_eq!(c.port_in(0x09).0, 0xFF, "head not loaded");
        c.port_out(0x09, control::HEAD_LOAD);
        assert_ne!(c.port_in(0x09).0, 0xFF, "head down: a real position");
    }

    // ---- head movement -----------------------------------------------------

    #[test]
    fn test_head_steps_and_stops_at_both_ends() {
        let mut c = ready();
        for _ in 0..(TRACKS as usize + 10) {
            c.port_out(0x09, control::STEP_IN);
        }
        assert_eq!(c.drives[0].track, TRACKS - 1, "must not step past the last track");
        for _ in 0..(TRACKS as usize + 10) {
            c.port_out(0x09, control::STEP_OUT);
        }
        assert_eq!(c.drives[0].track, 0, "must not step before track 0");
    }

    /// Track 0 is how a guest finds home before anything else works.
    #[test]
    fn test_status_reports_track_zero() {
        let mut c = ready();
        let track0 = |c: &mut Dcdd| !c.port_in(0x08).0 & status::TRACK0 != 0;
        assert!(track0(&mut c), "starts at track 0");
        c.port_out(0x09, control::STEP_IN);
        assert!(!track0(&mut c), "no longer at track 0");
        c.port_out(0x09, control::STEP_OUT);
        assert!(track0(&mut c), "back home");
    }

    #[test]
    fn test_head_load_and_unload() {
        let mut c = ready();
        let loaded = |c: &mut Dcdd| !c.port_in(0x08).0 & status::HEAD_LOADED != 0;
        assert!(loaded(&mut c));
        c.port_out(0x09, control::HEAD_UNLOAD);
        assert!(!loaded(&mut c));
    }

    // ---- data transfer -----------------------------------------------------

    /// Reading asks the host for the sector, then hands it back byte by byte.
    #[test]
    fn test_reading_a_sector_asks_for_it_then_serves_it() {
        let mut c = ready();
        let (_, req) = c.port_in(0x0A);
        assert_eq!(req, Request::Read { drive: 0, track: 0, sector: 0 });
        let mut sector = [0u8; SECTOR_LEN];
        for (i, b) in sector.iter_mut().enumerate() {
            *b = i as u8;
        }
        c.sector_loaded(0, &sector);
        for i in 0..SECTOR_LEN {
            let (v, _) = c.port_in(0x0A);
            assert_eq!(v, i as u8, "byte {i}");
        }
    }

    /// All 137 bytes are served, header and trailer included — where the data
    /// sits inside them is the guest BIOS's business, not ours.
    #[test]
    fn test_the_whole_physical_sector_is_served() {
        let mut c = ready();
        c.port_in(0x0A);
        c.sector_loaded(0, &[0xAB; SECTOR_LEN]);
        let served = (0..SECTOR_LEN).filter(|_| c.port_in(0x0A).0 == 0xAB).count();
        assert_eq!(served, SECTOR_LEN, "a sector is {SECTOR_LEN} bytes on the wire");
    }

    /// A write is committed when the sector leaves the head, not before.
    #[test]
    fn test_a_write_is_committed_when_the_sector_passes() {
        let mut c = ready();
        c.port_out(0x09, control::WRITE_ENABLE);
        for i in 0..SECTOR_LEN {
            c.port_out(0x0A, i as u8);
        }
        // Rotate until the sector leaves.
        let mut committed = None;
        for _ in 0..4 {
            let (_, req) = c.port_in(0x09);
            if let Request::Write { drive, track, sector } = req {
                committed = Some((drive, track, sector));
                break;
            }
        }
        assert_eq!(committed, Some((0, 0, 0)), "the written sector must be handed back");
        assert_eq!(c.sector_buffer(0).unwrap()[5], 5);
    }

    /// A read-only disk must refuse the write sequence outright, so a guest
    /// cannot get as far as filling a buffer that will never be committed.
    #[test]
    fn test_a_read_only_disk_refuses_writes() {
        let mut c = Dcdd::new();
        c.insert(0, Disk { geometry: Geometry::EIGHT_INCH, read_only: true });
        c.port_out(0x08, 0);
        c.port_out(0x09, control::HEAD_LOAD);
        c.port_out(0x09, control::WRITE_ENABLE);
        c.port_out(0x0A, 0x55);
        for _ in 0..4 {
            if let (_, Request::Write { .. }) = c.port_in(0x09) {
                panic!("a read-only disk must never ask for a write");
            }
        }
    }

    // ---- geometry ----------------------------------------------------------

    #[test]
    fn test_geometry_addressing_matches_the_image_layout() {
        let g = Geometry::EIGHT_INCH;
        assert_eq!(g.offset(0, 0), 0);
        assert_eq!(g.offset(0, 1), 137);
        assert_eq!(g.offset(1, 0), 32 * 137);
        assert_eq!(g.image_len(), 337_568, "the size of a real Altair 8\" image");
        assert_eq!(Geometry::MINIDISK.image_len(), 35 * 16 * 137);
    }

    #[test]
    fn test_minidisk_wraps_at_its_own_sector_count() {
        let mut c = Dcdd::new();
        c.insert(0, Disk { geometry: Geometry::MINIDISK, read_only: false });
        c.port_out(0x08, 0);
        c.port_out(0x09, control::HEAD_LOAD);
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..64 {
            let (v, _) = c.port_in(0x09);
            seen.insert(sector_of(v));
        }
        assert_eq!(seen.len(), MINI_SECTORS_PER_TRACK as usize);
    }

    /// Selecting a drive that does not exist must not panic or leave the
    /// controller pointing at one that does.
    #[test]
    fn test_selecting_a_missing_drive_is_inert() {
        let mut c = Dcdd::new();
        c.port_out(0x08, 5);
        assert_eq!(c.port_in(0x09).0, 0xFF);
        assert_eq!(c.port_in(0x08).0, !0u8, "no drive: every status bit clear");
    }

    #[test]
    fn test_unknown_ports_are_inert() {
        let mut c = ready();
        assert_eq!(c.port_in(0x0B).0, 0xFF);
        assert_eq!(c.port_out(0x0B, 0x55), Request::None);
    }
}
