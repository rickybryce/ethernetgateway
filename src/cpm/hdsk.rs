//! MITS 88-HDSK hard disk controller (the "Datakeeper").
//!
//! Written from the published manual — `88-HDSK.pdf`, §3-4 and §3-5 — and its
//! errata sheets, cross-checked against observed behaviour. Not transcribed
//! from another emulator; same clean-room posture as Punter, HBIOS and EGT80.
//! `web/diskreference.html` holds the write-up this was built from.
//!
//! # How it differs from the floppy, and why that shaped the seam
//!
//! The 88-DCDD is a bare board: the guest polls a rotating sector counter and
//! shifts bytes when the sector it wants comes round. This is not that. The
//! Datakeeper is an intelligent controller with its own 8X300 processor and
//! 1 KB of buffer memory, reached through an 88-4PIO board, and the guest
//! *asks* it to do things:
//!
//! ```text
//!     Read Sector  -> moves a sector from the platter into one of four buffers
//!     Read Buffer  -> moves that buffer up to the Altair, a byte at a time
//! ```
//!
//! Two steps, not one, and no rotational timing anywhere. That is why
//! [`crate::cpm::controller`] speaks in byte ranges: neither vocabulary would
//! survive being imposed on the other.
//!
//! # The handshake
//!
//! Four status flags, each **bit 7** of its own port, each cleared by touching
//! the paired data port. The manual's own example (errata `ME05`, because the
//! routine in the body never clears Controller Ready and is described there as
//! nonfunctional) waits on them like this:
//!
//! ```text
//!     in  163         ; clear the command-acknowledge flag
//!     out 167, low    ; low byte of the command
//!     in  167         ; clear the data-port-available flag
//!     out 163, high   ; high byte -- this starts the command
//!     wait on 166 bit 7 -> the controller will take a data byte
//!     wait on 160 bit 7 -> the controller has finished
//!     in  161         ; clears ready, returns the error byte
//! ```
//!
//! **The flags are set immediately.** On real hardware the controller answers
//! within microseconds — the errata measures 4.5 µs, "only 1 or 2 8080
//! instruction times" — and says outright there is no need to spin waiting for
//! them. Software written against that will often not poll at all, so a model
//! that made a flag take time would hang guests the real board never hung.

use super::controller::{Controller, HostRequest};

/// Bytes in one hard-disk sector.
pub const SECTOR_LEN: usize = 256;

/// Sectors per cylinder, per head. Manual §3-4: "the 24 addressable sectors
/// per cylinder", sector address 0 through 23.
pub const SECTORS: u8 = 24;

/// Heads on the drives these images came off.
///
/// The board addresses 0–7 — the fixed platter is 2 and 3, the removable 0 and
/// 1 — but a drive with fewer platters narrows that, and the 4.9 MB images are
/// a single platter: 406 × 2 × 24 × 256 is exactly their size.
pub const HEADS: u8 = 2;

/// Cylinders. The manual gives the seek range as 0–405.
pub const CYLINDERS: u16 = 406;

/// The one image size this controller recognises.
pub const IMAGE_LEN: u64 =
    CYLINDERS as u64 * HEADS as u64 * SECTORS as u64 * SECTOR_LEN as u64;

/// Data buffers in the controller's own memory, 256 bytes each.
const BUFFERS: usize = 4;

/// Drives the board can address.
const UNITS: usize = 4;

/// Ports the 88-4PIO occupies — octal 160–167.
const PORT_BASE: u8 = 0xA0;
const PORT_TOP: u8 = 0xA7;

/// The four command/status channels, by port.
mod port {
    /// IN: controller ready, bit 7. Cleared by reading [`STATUS`].
    pub const READY: u8 = 0xA0;
    /// IN: the error byte. Reading it clears [`READY`].
    pub const STATUS: u8 = 0xA1;
    /// IN: command acknowledged, bit 7. Cleared by writing [`COMMAND`].
    pub const ACK: u8 = 0xA2;
    /// OUT: high byte of a command — writing it starts the command.
    pub const COMMAND: u8 = 0xA3;
    /// IN: read data available, bit 7. Cleared by reading [`DATA_IN`].
    pub const READ_READY: u8 = 0xA4;
    /// IN: read-buffer and status data.
    pub const DATA_IN: u8 = 0xA5;
    /// IN: write data accepted, bit 7. Cleared by writing [`DATA_OUT`].
    pub const WRITE_READY: u8 = 0xA6;
    /// OUT: command parameters and write-buffer data.
    pub const DATA_OUT: u8 = 0xA7;
}

/// Bits of the error byte at port 161.
///
/// From errata `ME03`, which supplies a table the manual body omits.
pub mod error {
    /// The drive is not ready. Every command but INITIALIZE and SET IV BYTE.
    pub const NOT_READY: u8 = 1 << 0;
    /// A sector number outside 0–23.
    pub const ILLEGAL_SECTOR: u8 = 1 << 1;
    /// The sector is write-protected.
    pub const WRITE_PROTECT: u8 = 1 << 7;
}

/// What the top nibble of a command's high byte selects.
///
/// The manual gives these as individual bit conditions — "bits 12 and 13 must
/// be ones and bits 14 and 15 must be zeros" — which is unreadable at the point
/// of use and easy to transcribe wrongly. Collected here once, as the nibble
/// they add up to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    /// Move the heads. Cylinder in bits 0–8, unit in 10–11.
    Seek,
    /// Platter → buffer.
    ReadSector,
    /// Buffer → platter.
    WriteSector,
    /// Buffer → Altair.
    ReadBuffer,
    /// Altair → buffer.
    WriteBuffer,
    /// Write one of the controller's internal IV bytes.
    SetByte,
    /// Anything the board would not recognise.
    Unknown,
}

impl Command {
    fn of(word: u16) -> Command {
        // Bit 15 first: Set Byte is 80h in the high byte, and the manual's own
        // example sends exactly that.
        if word & 0x8000 != 0 {
            return Command::SetByte;
        }
        match (word >> 12) & 0x07 {
            0b000 => Command::Seek,
            0b001 => Command::WriteSector,
            0b011 => Command::ReadSector,
            0b100 => Command::WriteBuffer,
            0b101 => Command::ReadBuffer,
            _ => Command::Unknown,
        }
    }
}

/// One drive attached to the controller.
#[derive(Debug, Clone, Copy)]
struct Unit {
    present: bool,
    read_only: bool,
    /// Where the heads are. A read or write uses this, not a cylinder in the
    /// command — the command carries only head and sector.
    cylinder: u16,
}

/// The controller.
pub struct Hdsk {
    units: [Unit; UNITS],
    buffers: [[u8; SECTOR_LEN]; BUFFERS],

    /// Low byte of the command being assembled, latched from port 167.
    pending_low: u8,
    /// The unit the last command named.
    selected: u8,

    // ---- handshake flags, each bit 7 of its own port --------------------
    ready: bool,
    ack: bool,
    read_ready: bool,
    write_ready: bool,

    /// The error byte at port 161.
    status: u8,
    /// Errata ME03: every bit reads as 1 on the first read after power-on.
    /// A driver that has not seen that yet may take a zero as "no controller".
    first_status_read: bool,

    /// An in-progress Read Buffer or Write Buffer transfer.
    transfer: Option<Transfer>,

    /// Which buffer the sector command in flight is using.
    ///
    /// Carried here because the machine answers a byte-range request without
    /// echoing back which buffer asked for it — the seam deliberately knows
    /// nothing about buffers.
    pending_buffer: usize,

    /// Reads of a ready flag that found nothing, since the last one that did.
    ///
    /// The board has no rotating counter to get stuck on, but it has flags a
    /// guest can wait on forever — and a guest doing that looks exactly like a
    /// crashed CPU, which is the confusion the floppy bring-up had to untangle.
    /// Counted so the driver can say which it is.
    idle_polls: u32,
}

/// A byte-at-a-time move between a buffer and the Altair.
#[derive(Debug, Clone, Copy)]
struct Transfer {
    buffer: usize,
    pos: usize,
    len: usize,
    writing: bool,
}

impl Default for Hdsk {
    fn default() -> Hdsk {
        Hdsk::new()
    }
}

impl Hdsk {
    pub fn new() -> Hdsk {
        Hdsk {
            units: [Unit { present: false, read_only: true, cylinder: 0 }; UNITS],
            buffers: [[0; SECTOR_LEN]; BUFFERS],
            pending_low: 0,
            selected: 0,
            // Idle and ready to be given a command, which is the state a guest
            // finds the board in after the Datakeeper's own reset.
            ready: true,
            ack: false,
            read_ready: false,
            write_ready: false,
            status: 0,
            first_status_read: true,
            transfer: None,
            pending_buffer: 0,
            idle_polls: 0,
        }
    }

    /// Byte offset of a sector, in the order these images store them.
    ///
    /// Cylinder, then head, then sector — which is what the sample set's own
    /// README describes ADEXER's `XD` command producing, and what the file size
    /// confirms: 406 × 2 × 24 × 256 is exactly 4,988,928.
    fn offset(&self, unit: u8, head: u8, sector: u8) -> Option<u64> {
        let u = self.units.get(unit as usize)?;
        if !u.present || head >= HEADS || sector >= SECTORS || u.cylinder >= CYLINDERS {
            return None;
        }
        let track = u.cylinder as u64 * HEADS as u64 + head as u64;
        Some((track * SECTORS as u64 + sector as u64) * SECTOR_LEN as u64)
    }

    /// Bit 7 set when `flag`, and the idle counter kept.
    fn poll(&mut self, flag: bool) -> u8 {
        if flag {
            self.idle_polls = 0;
            0x80
        } else {
            self.idle_polls = self.idle_polls.saturating_add(1);
            0x00
        }
    }

    /// Act on a fully assembled command.
    fn execute(&mut self, word: u16) -> HostRequest {
        // Taking the command is acknowledged immediately, as the real board
        // does — the errata measures microseconds and says not to spin on it.
        self.ack = true;
        self.status = 0;
        let unit = ((word >> 10) & 0x03) as u8;
        self.selected = unit;

        match Command::of(word) {
            Command::Seek => {
                let cylinder = word & 0x01FF;
                self.ready = true;
                match self.units.get_mut(unit as usize) {
                    Some(u) if u.present && cylinder < CYLINDERS => u.cylinder = cylinder,
                    Some(u) if u.present => self.status |= error::ILLEGAL_SECTOR,
                    _ => self.status |= error::NOT_READY,
                }
                HostRequest::None
            }
            Command::ReadSector | Command::WriteSector => {
                let sector = (word & 0x1F) as u8;
                let head = ((word >> 5) & 0x07) as u8;
                let buffer = ((word >> 8) & 0x03) as usize;
                let writing = Command::of(word) == Command::WriteSector;
                let Some(offset) = self.offset(unit, head, sector) else {
                    // Which of the two it is matters to a driver: an unplugged
                    // drive and a sector number off the end of the platter are
                    // different faults with different fixes.
                    self.status |= if self.units.get(unit as usize).is_some_and(|u| u.present) {
                        error::ILLEGAL_SECTOR
                    } else {
                        error::NOT_READY
                    };
                    self.ready = true;
                    return HostRequest::None;
                };
                if writing && self.units[unit as usize].read_only {
                    self.status |= error::WRITE_PROTECT;
                    self.ready = true;
                    return HostRequest::None;
                }
                // The machine moves the bytes; the controller says where.  It
                // is not finished until that has happened, so Ready stays low.
                self.ready = false;
                self.pending_buffer = buffer;
                if writing {
                    HostRequest::Write { drive: unit, offset, len: SECTOR_LEN }
                } else {
                    HostRequest::Read { drive: unit, offset, len: SECTOR_LEN }
                }
            }
            Command::ReadBuffer | Command::WriteBuffer => {
                let raw = (word & 0xFF) as usize;
                // "Note that a value of 0 in the transfer length implies a
                // transfer of 256 bytes" — and a length of one is written as 0
                // elsewhere in the same section, so this is the one place the
                // manual is genuinely ambiguous. 0 means the whole buffer.
                let len = if raw == 0 { SECTOR_LEN } else { raw };
                let buffer = ((word >> 8) & 0x03) as usize;
                let writing = Command::of(word) == Command::WriteBuffer;
                self.transfer = Some(Transfer { buffer, pos: 0, len, writing });
                if writing {
                    self.write_ready = true;
                } else {
                    self.read_ready = true;
                }
                self.ready = false;
                HostRequest::None
            }
            Command::SetByte => {
                // The IV bytes are the controller's own internals — drive
                // select, start/stop, the cylinder latches. Nothing above the
                // board can observe them, so they are accepted and the command
                // completes, which is what the guest is waiting on.
                self.write_ready = true;
                self.ready = true;
                HostRequest::None
            }
            Command::Unknown => {
                self.ready = true;
                HostRequest::None
            }
        }
    }
}

impl Controller for Hdsk {
    fn name(&self) -> &'static str {
        "MITS 88-HDSK hard disk"
    }

    fn owns_port(&self, port: u8) -> bool {
        (PORT_BASE..=PORT_TOP).contains(&port)
    }

    fn port_in(&mut self, port: u8) -> (u8, HostRequest) {
        match port {
            port::READY => {
                let v = self.poll(self.ready);
                (v, HostRequest::None)
            }
            port::STATUS => {
                // Reading the error byte is what clears Ready — that is the
                // documented pairing, and a driver that never reads it will
                // wait forever on the *next* command.
                self.ready = false;
                if self.first_status_read {
                    self.first_status_read = false;
                    return (0xFF, HostRequest::None);
                }
                (self.status, HostRequest::None)
            }
            port::ACK => {
                let v = self.poll(self.ack);
                (v, HostRequest::None)
            }
            // Reading a nominally-output port clears its handshake flag. That
            // is PIA behaviour, and the manual's own example relies on it:
            // `in ACMD` before sending a command, `in ADATA` after.
            port::COMMAND => {
                self.ack = false;
                (0xFF, HostRequest::None)
            }
            port::READ_READY => {
                let v = self.poll(self.read_ready);
                (v, HostRequest::None)
            }
            port::DATA_IN => {
                self.read_ready = false;
                let Some(t) = self.transfer.as_mut().filter(|t| !t.writing) else {
                    return (0xFF, HostRequest::None);
                };
                let byte = self.buffers[t.buffer][t.pos % SECTOR_LEN];
                t.pos += 1;
                if t.pos >= t.len {
                    self.transfer = None;
                    self.ready = true;
                } else {
                    self.read_ready = true;
                }
                (byte, HostRequest::None)
            }
            port::WRITE_READY => {
                let v = self.poll(self.write_ready);
                (v, HostRequest::None)
            }
            port::DATA_OUT => {
                self.write_ready = false;
                (0xFF, HostRequest::None)
            }
            _ => (0xFF, HostRequest::None),
        }
    }

    fn port_out(&mut self, port: u8, value: u8) -> HostRequest {
        match port {
            port::COMMAND => {
                self.ack = false;
                let word = u16::from(value) << 8 | u16::from(self.pending_low);
                self.execute(word)
            }
            port::DATA_OUT => {
                self.write_ready = false;
                match self.transfer.as_mut().filter(|t| t.writing) {
                    Some(t) => {
                        self.buffers[t.buffer][t.pos % SECTOR_LEN] = value;
                        t.pos += 1;
                        if t.pos >= t.len {
                            self.transfer = None;
                            self.ready = true;
                        } else {
                            self.write_ready = true;
                        }
                    }
                    // Not mid-transfer, so this is the low half of a command
                    // waiting for its high half.
                    None => self.pending_low = value,
                }
                HostRequest::None
            }
            _ => HostRequest::None,
        }
    }

    fn accepts(&self, image_len: u64) -> Option<&'static str> {
        (image_len == IMAGE_LEN).then_some("Altair 88-HDSK hard disk")
    }

    fn insert(&mut self, drive: u8, image_len: u64, read_only: bool) -> Result<(), String> {
        if image_len != IMAGE_LEN {
            return Err(format!("{image_len} bytes is not an 88-HDSK image"));
        }
        let u = self
            .units
            .get_mut(drive as usize)
            .ok_or_else(|| format!("the 88-HDSK addresses units 0-3, not {drive}"))?;
        *u = Unit { present: true, read_only, cylinder: 0 };
        Ok(())
    }

    fn buffer_loaded(&mut self, _drive: u8, bytes: &[u8]) {
        let n = bytes.len().min(SECTOR_LEN);
        self.buffers[self.pending_buffer][..n].copy_from_slice(&bytes[..n]);
        // The sector is in the buffer, so the command is done.
        self.ready = true;
    }

    fn buffer(&self, _drive: u8) -> Option<&[u8]> {
        Some(&self.buffers[self.pending_buffer])
    }

    fn stuck_polls(&self) -> u32 {
        self.idle_polls
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn board() -> Hdsk {
        let mut h = Hdsk::new();
        h.insert(0, IMAGE_LEN, false).unwrap();
        h
    }

    /// Send a 16-bit command the way the manual's example does: low byte to
    /// 167, then high byte to 163, which is what starts it.
    fn command(h: &mut Hdsk, word: u16) -> HostRequest {
        h.port_out(port::DATA_OUT, (word & 0xFF) as u8);
        h.port_out(port::COMMAND, (word >> 8) as u8)
    }

    fn ready(h: &mut Hdsk) -> bool {
        h.port_in(port::READY).0 & 0x80 != 0
    }

    /// The geometry the manual states, checked against the file size the real
    /// images have rather than against itself.
    #[test]
    fn test_geometry_matches_the_real_images() {
        assert_eq!(IMAGE_LEN, 4_988_928);
        assert_eq!(CYLINDERS as u64 * HEADS as u64, 812, "812 tracks of 24 sectors");
    }

    /// Errata ME03: every bit of the error byte reads as 1 on the first read
    /// after power-on. A driver that expects that and gets 0 may decide there
    /// is no controller there.
    #[test]
    fn test_the_first_status_read_is_all_ones() {
        let mut h = board();
        assert_eq!(h.port_in(port::STATUS).0, 0xFF, "first read after power-on");
        assert_eq!(h.port_in(port::STATUS).0, 0x00, "and not thereafter");
    }

    /// Reading the error byte is what clears Controller Ready. The manual's
    /// body gets this wrong by never doing it, which is why its sample routine
    /// is described in the errata as nonfunctional.
    #[test]
    fn test_reading_the_error_byte_clears_ready() {
        let mut h = board();
        // Checked *before* reading the status byte, because reading it is
        // precisely what clears the flag — consuming the power-on 0xFF first
        // would clear it too, which is the behaviour under test.
        assert!(ready(&mut h), "an idle board is ready for a command");
        let _ = h.port_in(port::STATUS);
        assert!(!ready(&mut h), "reading 161 clears the ready flag");
        // And a completed command sets it again.
        command(&mut h, 0); // seek unit 0 to cylinder 0
        assert!(ready(&mut h), "a finished command reports ready");
    }

    /// A seek moves the heads, and a read afterwards uses *that* cylinder —
    /// the read command carries only head and sector, so a controller that
    /// forgot the seek would silently read cylinder 0 every time.
    #[test]
    fn test_a_read_uses_the_cylinder_the_seek_set() {
        let mut h = board();
        let _ = h.port_in(port::STATUS);
        // Seek unit 0 to cylinder 5: type 000, cylinder in bits 0-8.
        assert_eq!(command(&mut h, 5), HostRequest::None);
        // Read sector 3, head 1, buffer 0: type 011.
        let req = command(&mut h, 0x3000 | (1 << 5) | 3);
        let want = ((5u64 * HEADS as u64 + 1) * SECTORS as u64 + 3) * SECTOR_LEN as u64;
        assert_eq!(req, HostRequest::Read { drive: 0, offset: want, len: SECTOR_LEN });
    }

    /// The two-step shape the whole board is built around: a sector goes to a
    /// buffer, and only a second command moves it to the Altair.
    #[test]
    fn test_a_sector_reaches_the_guest_through_a_buffer() {
        let mut h = board();
        let _ = h.port_in(port::STATUS);
        let req = command(&mut h, 0x3000); // read sector 0, head 0, buffer 0
        assert!(matches!(req, HostRequest::Read { .. }));
        assert!(!ready(&mut h), "not finished until the machine supplies the bytes");

        let mut sector = [0u8; SECTOR_LEN];
        sector.iter_mut().enumerate().for_each(|(i, b)| *b = i as u8);
        h.buffer_loaded(0, &sector);
        assert!(ready(&mut h), "the sector is in the buffer, so the command is done");

        // Read Buffer: type 101, length 0 meaning all 256.
        command(&mut h, 0x5000);
        let mut got = Vec::new();
        for _ in 0..SECTOR_LEN {
            assert!(h.port_in(port::READ_READY).0 & 0x80 != 0, "a byte should be waiting");
            got.push(h.port_in(port::DATA_IN).0);
        }
        assert_eq!(got, sector.to_vec());
        assert!(ready(&mut h), "and the transfer completes");
        assert_eq!(
            h.port_in(port::READ_READY).0 & 0x80,
            0,
            "with nothing further offered"
        );
    }

    /// "Note that a value of 0 in the transfer length implies a transfer of 256
    /// bytes" — a length of zero must not mean no bytes.
    #[test]
    fn test_a_transfer_length_of_zero_means_a_whole_sector() {
        let mut h = board();
        let _ = h.port_in(port::STATUS);
        command(&mut h, 0x3000);
        h.buffer_loaded(0, &[0xAB; SECTOR_LEN]);
        command(&mut h, 0x5000); // length byte 0
        let mut n = 0;
        while h.port_in(port::READ_READY).0 & 0x80 != 0 {
            h.port_in(port::DATA_IN);
            n += 1;
            assert!(n <= SECTOR_LEN, "ran past a sector");
        }
        assert_eq!(n, SECTOR_LEN, "0 means 256, not none");
    }

    /// The guest's half of a write: bytes go into a buffer, then a Write Sector
    /// puts that buffer on the platter.
    #[test]
    fn test_the_guest_writes_through_a_buffer_too() {
        let mut h = board();
        let _ = h.port_in(port::STATUS);
        command(&mut h, 0x4000 | 4); // write buffer 0, length 4
        for b in [1u8, 2, 3, 4] {
            assert!(h.port_in(port::WRITE_READY).0 & 0x80 != 0);
            h.port_out(port::DATA_OUT, b);
        }
        assert!(ready(&mut h));
        // Write sector 2, head 0, buffer 0: type 001.
        let req = command(&mut h, 0x1000 | 2);
        assert_eq!(
            req,
            HostRequest::Write { drive: 0, offset: 2 * SECTOR_LEN as u64, len: SECTOR_LEN }
        );
        assert_eq!(&h.buffer(0).unwrap()[..4], &[1, 2, 3, 4]);
    }

    /// A drive that is not there and a sector that is not on the platter are
    /// different faults, and a driver acts differently on each.
    #[test]
    fn test_absent_drive_and_illegal_sector_report_differently() {
        let mut h = board();
        let _ = h.port_in(port::STATUS);
        // Unit 1 has no disk: bits 10-11 select it.
        command(&mut h, 0x3000 | (1 << 10));
        assert_eq!(h.port_in(port::STATUS).0 & error::NOT_READY, error::NOT_READY);

        // Sector 30 does not exist on unit 0 — only 0-23 do.
        command(&mut h, 0x3000 | 30);
        let st = h.port_in(port::STATUS).0;
        assert_eq!(st & error::ILLEGAL_SECTOR, error::ILLEGAL_SECTOR, "{st:#04x}");
        assert_eq!(st & error::NOT_READY, 0, "the drive is present, the sector is not");
    }

    /// A write-protected disk refuses the write and says why, rather than
    /// quietly not doing it.
    #[test]
    fn test_a_read_only_disk_refuses_a_write_sector() {
        let mut h = Hdsk::new();
        h.insert(0, IMAGE_LEN, true).unwrap();
        let _ = h.port_in(port::STATUS);
        let req = command(&mut h, 0x1000);
        assert_eq!(req, HostRequest::None, "nothing is written");
        assert_eq!(h.port_in(port::STATUS).0 & error::WRITE_PROTECT, error::WRITE_PROTECT);
    }

    /// Only the 4.9 MB images, and only this controller's ports.
    #[test]
    fn test_what_it_claims() {
        let h = Hdsk::new();
        assert_eq!(h.accepts(IMAGE_LEN), Some("Altair 88-HDSK hard disk"));
        assert_eq!(h.accepts(337_568), None, "a floppy is not ours");
        assert!(h.owns_port(0xA0) && h.owns_port(0xA7));
        assert!(!h.owns_port(0x08), "the floppy's ports stay the floppy's");
        assert!(!h.owns_port(0xA8));
    }

    /// A guest waiting on a flag that never sets looks exactly like a crashed
    /// CPU. The count is what tells the two apart.
    #[test]
    fn test_waiting_on_a_flag_that_never_sets_is_counted() {
        let mut h = board();
        let _ = h.port_in(port::STATUS);
        for _ in 0..50 {
            h.port_in(port::READ_READY);
        }
        assert!(h.stuck_polls() >= 50, "a guest spinning here must be visible");
        // ...and progress resets it.
        command(&mut h, 0x3000);
        h.buffer_loaded(0, &[0; SECTOR_LEN]);
        command(&mut h, 0x5000);
        h.port_in(port::READ_READY);
        assert_eq!(h.stuck_polls(), 0);
    }
}
