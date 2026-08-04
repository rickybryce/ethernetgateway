//! The Western Digital FD1771 floppy disk formatter/controller.
//!
//! Written from the FD1771 data sheet, which the Tarbell manual reproduces whole
//! as its §7-2 — command summary, flag summary, and the status-register matrix of
//! Table 6. Cross-checked against the command bytes in that manual's own test
//! programs (`43h` step in, `63h` step out, `8Ch` read) and against the driver
//! source carried on the disks themselves, whose `HLAB EQU 8 ;8 FOR HD LD AT BEG
//! OF SEEK` confirms bit 3 is the head-load flag. Same clean-room posture as
//! everything else here: published documentation, cross-checked against what
//! software does, never transcribed from another emulator.
//!
//! # Why this is its own module
//!
//! Two boards in the plan use this chip — the Tarbell 1011 and the Cromemco
//! 4FDC/16FDC — and they differ only in the wrapper: which ports the registers
//! appear at, how the drive is selected, and what the board does about waiting.
//! Build the chip once and wrap it twice. The alternative is discovering, on the
//! second board, that half of it was entangled with the first board's ports.
//!
//! # The one thing worth knowing before reading further
//!
//! **The status register means different things depending on the command that is
//! running.** Bit 6 is Write Protect after a Type I command and the top bit of the
//! record type after a Read Sector; bit 5 is Head Engaged in one and Write Fault
//! in another; bit 2 is Track 00 in one and Lost Data in the other. Table 6 of the
//! data sheet is a matrix for exactly this reason, and a controller that assembles
//! one fixed byte will satisfy a driver that only looks at Busy and then mislead
//! everything else. So the command in progress is remembered, and the status is
//! built from it.
//!
//! # What is not modelled, and why
//!
//! * **Write Track** — formatting. These images hold sector *data* only: 77 × 26 ×
//!   128 is exactly 256,256 bytes with no room for ID fields, gaps or address
//!   marks. There is nothing in the file for a format to write, which is the same
//!   reason a blank file is not a blank Altair floppy. It is refused visibly
//!   rather than pretended.
//! * **Read Track** — the same problem from the other side: it would have to
//!   invent the framing a real track has and this file does not.
//! * **Rotational and stepping time.** A real seek takes milliseconds and a real
//!   sector comes round once per revolution; here both are instant. That is the
//!   same decision the 88-HDSK made, and it is safe in the same way — software
//!   waits for flags, and a flag that is already set is never waited on wrongly.
//!   The one hazard is a driver that waits for something to go *away*; see
//!   `index` below, which is why that bit alternates.

/// Bytes in an IBM 3740 sector, which is what these disks hold.
pub const SECTOR_LEN: usize = 128;

/// What the chip needs the board to do with the medium.
///
/// The chip knows track, sector and what direction; it does not know where that
/// lands in a file, because that is the board's geometry and not the chip's. Same
/// division as everywhere else here: the part that knows the arithmetic does it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Need {
    /// Nothing.
    None,
    /// Fill the sector buffer from this track and sector.
    Read { track: u8, sector: u8 },
    /// Write the sector buffer back to this track and sector.
    Write { track: u8, sector: u8 },
}

/// The four registers, as the low two bits of the port address select them.
pub mod reg {
    /// Command on write, status on read.
    pub const COMMAND: u8 = 0;
    /// The track register — what the chip believes the head is on.
    pub const TRACK: u8 = 1;
    /// The sector register.
    pub const SECTOR: u8 = 2;
    /// The data register, and the transfer path.
    pub const DATA: u8 = 3;
}

/// Status bits, named for what they mean **after a Type I command**.
///
/// Table 6 again: four of these eight change meaning under the transfer commands,
/// so they are named twice and assembled separately.
mod type1 {
    pub const NOT_READY: u8 = 0x80;
    pub const PROTECTED: u8 = 0x40;
    pub const HEAD_LOADED: u8 = 0x20;
    pub const SEEK_ERROR: u8 = 0x10;
    pub const CRC_ERROR: u8 = 0x08;
    pub const TRACK_0: u8 = 0x04;
    pub const INDEX: u8 = 0x02;
    pub const BUSY: u8 = 0x01;
}

/// Status bits as they mean under Type II and Type III commands.
mod type2 {
    pub const NOT_READY: u8 = 0x80;
    /// Record type high bit on a read; Write Protect on a write.
    pub const WRITE_PROTECT: u8 = 0x40;
    /// Record type low bit on a read; Write Fault on a write.
    pub const WRITE_FAULT: u8 = 0x20;
    pub const RECORD_NOT_FOUND: u8 = 0x10;
    pub const CRC_ERROR: u8 = 0x08;
    pub const LOST_DATA: u8 = 0x04;
    pub const DRQ: u8 = 0x02;
    pub const BUSY: u8 = 0x01;
}

/// Which of the four command families is in progress.
///
/// Kept because the status register cannot be assembled without it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Restore, Seek, Step, Step In, Step Out.
    One,
    /// Read Sector, Write Sector.
    Two,
    /// Read Address, Read Track, Write Track.
    Three,
    /// Force Interrupt.
    Four,
}

impl Kind {
    /// Which family a command byte belongs to.
    ///
    /// From the data sheet's command summary. Bit 7 clear is always Type I; above
    /// that the top three bits choose, and `110` is the odd one out because Force
    /// Interrupt sits in the middle of the Type III range.
    fn of(cmd: u8) -> Kind {
        match cmd >> 4 {
            0x0..=0x7 => Kind::One,
            0x8..=0xB => Kind::Two,
            0xD => Kind::Four,
            _ => Kind::Three,
        }
    }
}

/// A transfer between the data register and the sector buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Move {
    None,
    Reading,
    Writing,
}

/// The chip.
pub struct Wd1771 {
    // ---- the four registers --------------------------------------------
    track: u8,
    sector: u8,
    data: u8,

    /// The command being executed, for typing the status register.
    cmd: u8,

    // ---- what the board tells us about the selected drive ---------------
    ready: bool,
    read_only: bool,
    tracks: u8,

    // ---- flags ----------------------------------------------------------
    busy: bool,
    drq: bool,
    intrq: bool,
    /// Set by whatever went wrong, in the bit position for the running command.
    error: u8,
    /// Head loaded, from the `h` flag of Type I commands. Reported in Type I
    /// status bit 5, and some drivers will not read until they see it.
    head_loaded: bool,
    /// Alternates on every Type I status read; see the struct's doc comment.
    index: bool,

    // ---- the sector in flight -------------------------------------------
    buffer: [u8; SECTOR_LEN],
    pos: usize,
    moving: Move,

    /// Status reads that found nothing new, for the stuck-guest detector.
    idle_polls: u32,
    /// What the last status read returned, so "nothing new" can be recognised.
    prev_status: u8,
    /// Which way the last step went. `Step` repeats it, which is the one piece of
    /// state a driver can lose track of if the controller guesses.
    last_step_in: bool,
}

impl Default for Wd1771 {
    fn default() -> Wd1771 {
        Wd1771::new()
    }
}

impl Wd1771 {
    pub fn new() -> Wd1771 {
        Wd1771 {
            track: 0,
            sector: 1,
            data: 0,
            // Idle, and typed as the family whose status reports Track 00 and
            // Index — which is what a driver polls before it does anything else.
            cmd: 0x00,
            ready: false,
            read_only: true,
            tracks: 77,
            busy: false,
            drq: false,
            intrq: false,
            error: 0,
            head_loaded: false,
            index: false,
            buffer: [0; SECTOR_LEN],
            pos: 0,
            moving: Move::None,
            idle_polls: 0,
            prev_status: 0xFF,
            last_step_in: true,
        }
    }

    /// What the board knows and the chip does not: whether a disk is there, and
    /// whether it may be written.
    pub fn set_drive(&mut self, ready: bool, read_only: bool, tracks: u8) {
        self.ready = ready;
        self.read_only = read_only;
        self.tracks = tracks.max(1);
    }

    /// The track register, for a board's diagnostics.
    pub fn track(&self) -> u8 {
        self.track
    }

    /// The sector register, likewise.
    pub fn sector(&self) -> u8 {
        self.sector
    }

    /// Data Request — a byte is waiting, or wanted.
    pub fn drq(&self) -> bool {
        self.drq
    }

    /// Command complete.
    pub fn intrq(&self) -> bool {
        self.intrq
    }

    /// Status reads that found nothing, since the last one that did.
    pub fn stuck_polls(&self) -> u32 {
        self.idle_polls
    }

    /// The sector buffer, for a board answering a [`Need::Write`].
    pub fn sector_out(&self) -> &[u8] {
        &self.buffer
    }

    /// Hand the chip the bytes a [`Need::Read`] asked for.
    ///
    /// The transfer opens here rather than when the command was issued, because
    /// until the bytes exist there is nothing to request.
    pub fn sector_loaded(&mut self, bytes: &[u8]) {
        let n = bytes.len().min(SECTOR_LEN);
        self.buffer[..n].copy_from_slice(&bytes[..n]);
        self.buffer[n..].fill(0);
        self.pos = 0;
        self.moving = Move::Reading;
        self.drq = true;
        self.idle_polls = 0;
    }

    /// Leave the chip as a completed, error-free Read Sector would have.
    ///
    /// For a board whose bootstrap PROM loads its boot sector with a real Type II
    /// read, and whose emulated bootstrap does not. The **typing** is the part
    /// that matters: a loader checking for read errors expects bit 2 to be Lost
    /// Data, and in the power-on Type I state it is Track 00 — which is set,
    /// because the head is on track 0. See `Controller::cold_started`.
    pub fn assume_read_completed(&mut self, track: u8, sector: u8) {
        // `8Ch` is what the Tarbell PROM issues: read, single record, IBM format,
        // head loaded at the beginning.
        self.cmd = 0x8C;
        self.track = track;
        self.sector = sector;
        self.head_loaded = true;
        self.error = 0;
        self.busy = false;
        self.drq = false;
        // The PROM's last act is to read the status register, which acknowledges
        // the interrupt. A loader that finds one pending would think its own first
        // command had already finished.
        self.intrq = false;
        self.moving = Move::None;
        self.prev_status = 0xFF;
    }

    /// A read or write of a register, by its low-two-bits address.
    pub fn read(&mut self, r: u8) -> u8 {
        match r & 0x03 {
            reg::COMMAND => {
                // Reading the status clears the interrupt, which is the standard
                // way a driver acknowledges completion.
                let s = self.status();
                self.intrq = false;
                if Kind::of(self.cmd) == Kind::One {
                    // See `index`: a driver that waits for the index pulse to
                    // pass needs it to pass.
                    self.index = !self.index;
                }
                s
            }
            reg::TRACK => self.track,
            reg::SECTOR => self.sector,
            reg::DATA => {
                if self.moving == Move::Reading {
                    let b = self.buffer[self.pos.min(SECTOR_LEN - 1)];
                    self.pos += 1;
                    self.idle_polls = 0;
                    if self.pos >= SECTOR_LEN {
                        // Done: the command completes and asks for attention.
                        self.moving = Move::None;
                        self.drq = false;
                        self.busy = false;
                        self.intrq = true;
                    } else {
                        self.drq = true;
                    }
                    self.data = b;
                }
                self.data
            }
            _ => 0xFF,
        }
    }

    /// Write a register. Returns what the board must do about it, if anything.
    pub fn write(&mut self, r: u8, value: u8) -> Need {
        match r & 0x03 {
            reg::COMMAND => self.command(value),
            reg::TRACK => {
                self.track = value;
                Need::None
            }
            reg::SECTOR => {
                self.sector = value;
                Need::None
            }
            reg::DATA => {
                self.data = value;
                if self.moving == Move::Writing {
                    self.buffer[self.pos.min(SECTOR_LEN - 1)] = value;
                    self.pos += 1;
                    self.idle_polls = 0;
                    if self.pos >= SECTOR_LEN {
                        self.moving = Move::None;
                        self.drq = false;
                        self.busy = false;
                        self.intrq = true;
                        return Need::Write { track: self.track, sector: self.sector };
                    }
                    self.drq = true;
                }
                Need::None
            }
            _ => Need::None,
        }
    }

    /// Execute a command byte.
    fn command(&mut self, cmd: u8) -> Need {
        let kind = Kind::of(cmd);

        // Force Interrupt is the one command accepted while busy, and its whole
        // job is to stop whatever is running.
        if kind == Kind::Four {
            self.cmd = cmd;
            self.busy = false;
            self.drq = false;
            self.moving = Move::None;
            self.error = 0;
            // "i3 = 1, Immediate interrupt" — and with no condition bits set the
            // data sheet has it terminate without one.
            self.intrq = cmd & 0x08 != 0;
            return Need::None;
        }

        self.cmd = cmd;
        self.error = 0;
        self.intrq = false;
        self.idle_polls = 0;

        match kind {
            Kind::One => {
                // The head-load flag, bit 3, which the disks' own driver calls
                // HLAB. It stays loaded until something unloads it.
                if cmd & 0x08 != 0 {
                    self.head_loaded = true;
                }
                let rate_and_verify_ok = self.ready;
                match cmd >> 4 {
                    0x0 => {
                        // Restore: seek track 0. "TR00 input does not go active
                        // low after 255 stepping pulses" is the failure, which
                        // cannot happen to a model that simply arrives.
                        self.track = 0;
                    }
                    0x1 => {
                        // Seek: the data register holds the desired track, and
                        // the track register is updated to match.
                        let want = self.data;
                        if want < self.tracks {
                            self.track = want;
                        } else {
                            self.error |= type1::SEEK_ERROR;
                        }
                    }
                    // Step, Step In, Step Out. Two details that are easy to get
                    // wrong and both matter to a driver that trusts the track
                    // register: plain `Step` repeats **the last direction**, and
                    // the update flag (bit 4) decides whether the register
                    // follows the head at all. A controller that always stepped
                    // inward, or always updated, would leave a driver's idea of
                    // the head somewhere the head is not.
                    0x2 | 0x3 => {
                        let inward = self.last_step_in;
                        self.step(cmd, inward);
                    }
                    0x4 | 0x5 => {
                        self.last_step_in = true;
                        self.step(cmd, true);
                    }
                    _ => {
                        self.last_step_in = false;
                        self.step(cmd, false);
                    }
                }
                // A verify with no disk in the drive cannot verify.
                if cmd & 0x04 != 0 && !rate_and_verify_ok {
                    self.error |= type1::SEEK_ERROR;
                }
                self.busy = false;
                self.intrq = true;
                Need::None
            }
            Kind::Two => {
                // "The TYPE II and III Commands will not execute unless the drive
                // is ready."
                if !self.ready {
                    self.busy = false;
                    self.intrq = true;
                    return Need::None;
                }
                let writing = cmd & 0x20 != 0;
                if writing && self.read_only {
                    // "any Write command is immediately terminated, an interrupt
                    // is generated and the Write Protect status bit is set".
                    self.error |= type2::WRITE_PROTECT;
                    self.busy = false;
                    self.intrq = true;
                    return Need::None;
                }
                if self.sector == 0 || self.sector as usize > SECTORS_PER_TRACK {
                    // Nothing on the track answers to that number.
                    self.error |= type2::RECORD_NOT_FOUND;
                    self.busy = false;
                    self.intrq = true;
                    return Need::None;
                }
                self.busy = true;
                self.pos = 0;
                if writing {
                    self.moving = Move::Writing;
                    self.drq = true;
                    Need::None
                } else {
                    // The board fetches; `sector_loaded` opens the transfer.
                    self.moving = Move::None;
                    self.drq = false;
                    Need::Read { track: self.track, sector: self.sector }
                }
            }
            Kind::Three => {
                if !self.ready {
                    self.busy = false;
                    self.intrq = true;
                    return Need::None;
                }
                if cmd >> 4 == 0xC {
                    // Read Address: six bytes of the ID field. Synthesised,
                    // because an unframed image has no ID fields to read — but
                    // synthesised from the truth, so a driver asking "where am I"
                    // gets the right answer.
                    let len_code = 0x00; // 128 bytes, IBM 3740
                    let id = [self.track, 0, self.sector, len_code, 0, 0];
                    self.buffer[..6].copy_from_slice(&id);
                    self.buffer[6..].fill(0);
                    self.pos = 0;
                    self.moving = Move::Reading;
                    self.drq = true;
                    self.busy = true;
                    Need::None
                } else {
                    // Read Track and Write Track. Both need the framing this
                    // image does not contain; saying so beats a silent success
                    // that leaves a guest believing it formatted a disk.
                    self.error |= type2::LOST_DATA;
                    self.busy = false;
                    self.intrq = true;
                    Need::None
                }
            }
            Kind::Four => unreachable!("handled above"),
        }
    }

    /// One step of the head, honouring the update flag.
    fn step(&mut self, cmd: u8, inward: bool) {
        if cmd & 0x10 == 0 {
            return; // the head moves; the register does not follow
        }
        self.track = if inward {
            self.track.saturating_add(1).min(self.tracks.saturating_sub(1))
        } else {
            self.track.saturating_sub(1)
        };
    }

    /// The status register, assembled for whichever command last ran.
    fn status(&mut self) -> u8 {
        let mut s = 0u8;
        match Kind::of(self.cmd) {
            Kind::One | Kind::Four => {
                if !self.ready {
                    s |= type1::NOT_READY;
                }
                if self.read_only {
                    s |= type1::PROTECTED;
                }
                if self.head_loaded {
                    s |= type1::HEAD_LOADED;
                }
                if self.track == 0 {
                    s |= type1::TRACK_0;
                }
                if self.index {
                    s |= type1::INDEX;
                }
                s |= self.error & (type1::SEEK_ERROR | type1::CRC_ERROR);
                if self.busy {
                    s |= type1::BUSY;
                }
            }
            Kind::Two | Kind::Three => {
                if !self.ready {
                    s |= type2::NOT_READY;
                }
                // Write Protect is reported on a write; on a read this bit is the
                // record type, and a normal data mark reads as zero here.
                if self.cmd & 0x20 != 0 && self.read_only {
                    s |= type2::WRITE_PROTECT;
                }
                s |= self.error
                    & (type2::WRITE_PROTECT
                        | type2::WRITE_FAULT
                        | type2::RECORD_NOT_FOUND
                        | type2::CRC_ERROR
                        | type2::LOST_DATA);
                if self.drq {
                    s |= type2::DRQ;
                }
                if self.busy {
                    s |= type2::BUSY;
                }
            }
        }
        // The index bit is deliberately excluded from "did anything change":
        // it alternates on every Type I status read by design, so counting it as
        // progress would mean a guest spinning on this register never looked
        // stuck, which is the whole thing the counter exists to notice.
        let meaningful = s & !type1::INDEX;
        if meaningful == self.prev_status {
            self.idle_polls = self.idle_polls.saturating_add(1);
        } else {
            self.idle_polls = 0;
        }
        self.prev_status = meaningful;
        s
    }
}

/// Sectors on an IBM 3740 track, numbered 1 upward.
pub const SECTORS_PER_TRACK: usize = 26;

#[cfg(test)]
mod tests {
    use super::*;

    fn chip() -> Wd1771 {
        let mut c = Wd1771::new();
        c.set_drive(true, false, 77);
        c
    }

    /// The command families, from the data sheet's summary table, checked at the
    /// values the manual's own test programs use.
    #[test]
    fn test_command_families_match_the_data_sheet() {
        assert_eq!(Kind::of(0x03), Kind::One, "restore");
        assert_eq!(Kind::of(0x1B), Kind::One, "seek");
        assert_eq!(Kind::of(0x43), Kind::One, "step in, from the manual's test program");
        assert_eq!(Kind::of(0x63), Kind::One, "step out, likewise");
        assert_eq!(Kind::of(0x8C), Kind::Two, "read sector, as the boot PROM issues it");
        assert_eq!(Kind::of(0xA8), Kind::Two, "write sector");
        assert_eq!(Kind::of(0xC4), Kind::Three, "read address");
        assert_eq!(Kind::of(0xE4), Kind::Three, "read track");
        assert_eq!(Kind::of(0xF4), Kind::Three, "write track");
        assert_eq!(Kind::of(0xD0), Kind::Four, "force interrupt");
    }

    /// A restore puts the head on track 0 and says so — in the Type I bit
    /// position, which is bit 2 and is Lost Data under a transfer command.
    #[test]
    fn test_restore_reaches_track_zero_and_reports_it() {
        let mut c = chip();
        c.write(reg::TRACK, 40);
        assert_eq!(c.write(reg::COMMAND, 0x0B), Need::None);
        assert_eq!(c.read(reg::TRACK), 0);
        let s = c.read(reg::COMMAND);
        assert_ne!(s & type1::TRACK_0, 0, "track 0 must be reported: {s:#04x}");
        assert_eq!(s & type1::BUSY, 0, "and the command is finished");
    }

    /// Seek takes its destination from the data register, which is the one place
    /// this chip is genuinely surprising.
    #[test]
    fn test_seek_takes_its_track_from_the_data_register() {
        let mut c = chip();
        c.write(reg::DATA, 33);
        c.write(reg::COMMAND, 0x1B);
        assert_eq!(c.read(reg::TRACK), 33);
        assert_eq!(c.read(reg::COMMAND) & type1::SEEK_ERROR, 0);

        // Past the end of the disk is a seek error, not a wild head.
        c.write(reg::DATA, 200);
        c.write(reg::COMMAND, 0x1B);
        assert_eq!(c.read(reg::TRACK), 33, "the head does not move");
        assert_ne!(c.read(reg::COMMAND) & type1::SEEK_ERROR, 0);
    }

    /// The step commands honour the update flag: with it clear the head moves and
    /// the track register does not follow.
    #[test]
    fn test_the_update_flag_decides_whether_the_track_register_follows() {
        let mut c = chip();
        c.write(reg::COMMAND, 0x5B); // step in, u = 1
        assert_eq!(c.read(reg::TRACK), 1);
        c.write(reg::COMMAND, 0x4B); // step in, u = 0
        assert_eq!(c.read(reg::TRACK), 1, "the register must not follow");
    }

    /// A read moves 128 bytes through the data register, one Data Request each,
    /// and finishes with an interrupt.
    #[test]
    fn test_a_sector_read_moves_128_bytes_and_then_interrupts() {
        let mut c = chip();
        c.write(reg::SECTOR, 5);
        assert_eq!(c.write(reg::COMMAND, 0x8C), Need::Read { track: 0, sector: 5 });
        assert!(!c.drq(), "nothing to offer until the board supplies the sector");

        let sector: Vec<u8> = (0..SECTOR_LEN).map(|i| i as u8).collect();
        c.sector_loaded(&sector);

        let mut got = Vec::new();
        for _ in 0..SECTOR_LEN {
            assert!(c.drq(), "a byte should be waiting");
            got.push(c.read(reg::DATA));
        }
        assert_eq!(got, sector);
        assert!(!c.drq(), "and none after the last");
        assert!(c.intrq(), "the command completed");
        assert_eq!(c.read(reg::COMMAND) & type2::BUSY, 0);
        assert!(!c.intrq(), "reading the status acknowledges it");
    }

    /// A write collects 128 bytes and only then asks the board to commit them.
    #[test]
    fn test_a_sector_write_asks_the_board_once_it_has_every_byte() {
        let mut c = chip();
        c.write(reg::TRACK, 3);
        c.write(reg::SECTOR, 7);
        assert_eq!(c.write(reg::COMMAND, 0xAC), Need::None);
        assert!(c.drq(), "it wants the first byte");

        for i in 0..SECTOR_LEN - 1 {
            assert_eq!(c.write(reg::DATA, i as u8), Need::None, "byte {i} is not the last");
        }
        assert_eq!(
            c.write(reg::DATA, 0xFF),
            Need::Write { track: 3, sector: 7 },
            "the last byte commits the sector"
        );
        assert_eq!(c.sector_out()[SECTOR_LEN - 1], 0xFF);
        assert!(c.intrq());
    }

    /// A write to a protected disk is refused before it starts, and says why in
    /// the bit the data sheet names.
    #[test]
    fn test_a_write_to_a_protected_disk_is_refused() {
        let mut c = Wd1771::new();
        c.set_drive(true, true, 77);
        c.write(reg::SECTOR, 1);
        assert_eq!(c.write(reg::COMMAND, 0xAC), Need::None);
        assert!(!c.drq(), "it must not ask for data it will not write");
        assert_ne!(c.read(reg::COMMAND) & type2::WRITE_PROTECT, 0);
    }

    /// The status register is typed by the running command. This is the whole
    /// reason the command byte is remembered, so it is asserted directly: the same
    /// physical bit reads as Track 00 after a restore and as Lost Data after a
    /// failed Write Track.
    #[test]
    fn test_the_status_register_is_typed_by_the_command() {
        let mut c = chip();
        c.write(reg::COMMAND, 0x0B); // restore -> Type I
        assert_ne!(c.read(reg::COMMAND) & 0x04, 0, "bit 2 is Track 00 here");

        c.write(reg::COMMAND, 0xF4); // write track -> Type III, refused
        let s = c.read(reg::COMMAND);
        assert_ne!(s & type2::LOST_DATA, 0, "and the same bit is Lost Data here");
        assert_eq!(s & type1::HEAD_LOADED, 0, "with no head-engaged bit in this family");
    }

    /// A sector number that is not on the track is Record Not Found, rather than
    /// a read of whatever happened to be at that offset.
    #[test]
    fn test_a_sector_number_off_the_track_is_record_not_found() {
        let mut c = chip();
        for bad in [0u8, 27, 200] {
            c.write(reg::SECTOR, bad);
            assert_eq!(c.write(reg::COMMAND, 0x8C), Need::None, "sector {bad} must not be read");
            assert_ne!(c.read(reg::COMMAND) & type2::RECORD_NOT_FOUND, 0, "sector {bad}");
        }
    }

    /// Type II commands do not run without a disk, and Type I commands do — which
    /// is what lets a driver restore a drive it has not yet loaded.
    #[test]
    fn test_a_transfer_needs_a_disk_and_a_seek_does_not() {
        let mut c = Wd1771::new(); // no disk
        c.write(reg::SECTOR, 1);
        assert_eq!(c.write(reg::COMMAND, 0x8C), Need::None);
        assert_ne!(c.read(reg::COMMAND) & type2::NOT_READY, 0);

        c.write(reg::DATA, 5);
        c.write(reg::COMMAND, 0x1B);
        assert_eq!(c.read(reg::TRACK), 5, "a seek happens regardless of Ready");
    }

    /// Force Interrupt stops a transfer in flight, and is the one command allowed
    /// while the chip is busy.
    #[test]
    fn test_force_interrupt_stops_a_transfer() {
        let mut c = chip();
        c.write(reg::SECTOR, 1);
        c.write(reg::COMMAND, 0x8C);
        c.sector_loaded(&[0xAA; SECTOR_LEN]);
        assert!(c.drq());

        c.write(reg::COMMAND, 0xD0);
        assert!(!c.drq(), "the transfer is abandoned");
        assert_eq!(c.read(reg::COMMAND) & type1::BUSY, 0);
    }

    /// Read Address answers with where the head actually is.
    #[test]
    fn test_read_address_reports_the_real_position() {
        let mut c = chip();
        c.write(reg::DATA, 12);
        c.write(reg::COMMAND, 0x1B); // seek 12
        c.write(reg::SECTOR, 9);
        c.write(reg::COMMAND, 0xC4);
        let id: Vec<u8> = (0..6).map(|_| c.read(reg::DATA)).collect();
        assert_eq!(id[0], 12, "track");
        assert_eq!(id[2], 9, "sector");
        assert_eq!(id[3], 0, "128 bytes, the IBM 3740 length code");
    }
}
