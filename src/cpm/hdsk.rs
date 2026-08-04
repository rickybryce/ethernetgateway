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

use super::controller::{ColdStart, Controller, HostRequest, Medium};

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

/// Bytes of the volume label in sector 0 that we read.
const LABEL_LEN: usize = 0x2C;

/// Offset in the label of the boot program's first sector, little-endian.
const LABEL_BOOT_SECTOR: usize = 0x28;

/// Offset in the label of how many sectors that program occupies.
const LABEL_BOOT_COUNT: usize = 0x2A;

/// Data buffers in the controller's own memory, 256 bytes each.
const BUFFERS: usize = 4;

/// The three IV bytes whose meanings are pinned by *two* agreeing sources —
/// errata 88-HDSK-ME02 and the equates in ADEXER's own source on these disks.
///
/// Everything else in the IV space stays a plain store, for the reasons on
/// [`Hdsk::iv`]. These three are different because they are the ones a real
/// diagnostic *reads to find out where the heads are*, and leaving them inert
/// made ADEXER report cylinder 511 after a seek to 3 — a stored zero, inverted
/// by the hardware rule, is all-ones.
mod iv {
    /// `IVH`, "Disk Control A": start/stop, extension, platter and side select,
    /// and the four unit-select lines.
    pub const DISK_CONTROL_A: usize = 17;
    /// `IVI`, "Disk Control B": the active-low positioner lines, and bit 0 is
    /// `IVIC8N`, "Cyl address bit 8 (inverted)".
    pub const DISK_CONTROL_B: usize = 18;
    /// `IVJ`: "Cyl address bits 7:0 (inverted)".
    pub const CYL_LOW: usize = 19;

    /// `IVIC8N` — cylinder bit 8, inverted, in [`DISK_CONTROL_B`].
    pub const CYL_BIT8: u8 = 0x01;
    /// `IVICRN` — "Cyl Restore (active low)". Pulled low, the positioner walks
    /// the heads back to cylinder 0.
    pub const CYL_RESTORE: u8 = 0x08;
    /// `IVHSS`, start/stop, which a Read Status leaves alone.
    pub const START_STOP: u8 = 0x80;
    /// What Read Status forces into the platter/side/extension field:
    /// "Extension Select low, Platter and Head select high".
    pub const PLATTER_AND_SIDE: u8 = 0x30;
}

/// IV bytes the controller addresses, per the diagnostic's `MAXIV equ 0FFh`.
const IV_BYTES: usize = 256;

/// What a formatted but unwritten sector holds.
///
/// **Measured, not assumed.** The whole second half of HDSK03 is 9,744 sectors
/// of nothing but `E5`, uniform, and HDSK04's free space is the same byte. That
/// is the CP/M erased-directory value as well, which is the point: a formatted
/// surface reads as empty to the filesystem above it.
///
/// What this deliberately does *not* reproduce is the sector headers a real
/// format writes — these images hold sector *data* only, with no header bytes
/// anywhere, so at the level this controller operates a format is exactly "the
/// data comes back erased".
const FORMAT_FILL: u8 = 0xE5;

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
///
/// # These are not deduced from the bit conditions — the disks state them
///
/// Four of the hard-disk images carry the assembler source of the 88-HDSK
/// software itself, and it defines the command set outright:
///
/// ```text
///     CSEEK   equ 00h     ;Seek                 Bits 15:12 = 0000b
///     CWRSEC  equ 020h    ;Write Sector         same bit fields as CRDSECT
///     CRDSEC  equ 030h    ;Read Sector          Bits 15:12 = 0011b
///     CWRBUF  equ 040h    ;Write Buffer
///     CRDBUF  equ 050h    ;Read Buffer          Bits 15:12 = 0101b
///     CRSTAT  equ 060h    ;Read Status (IV Byte)
///     CSETIV  equ 080h    ;Set IV Byte
///     CRUSEC  equ 0A0h    ;Read Unformatted Sector
///     CFORMT  equ 0C0h    ;Format
///     CINIT   equ 0E0h    ;Initialize
/// ```
///
/// That is **nine** commands, not the seven the manual's table lists, and it
/// is why this decodes the whole four-bit nibble rather than testing bit 15
/// first. Testing bit 15 first — which is what the manual's Set Byte example
/// invites, since `80h` is the only value it shows with that bit set — folded
/// Format (`C0h`) and Initialize (`E0h`) into Set Byte. Both then "succeeded"
/// while doing nothing, which is the same silent-success failure the Write
/// Sector nibble already caused once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    /// Move the heads. Cylinder in bits 0–8, unit in 10–11.
    Seek,
    /// Platter → buffer.
    ReadSector,
    /// Platter → buffer, without checking the sector header.
    ///
    /// Not in the manual at all — found by tracing what ADEXER sends and then
    /// finding `CRUSEC equ 0A0h ;Read Unformatted Sector` in its source. It is
    /// how the diagnostic reads a sector whose header is damaged, and, more
    /// importantly, how it *probes write protection*: `DUMYRD` issues one with an
    /// error mask of `80h` and its own comment says the result is "0 if not write
    /// protected, a=80h if write protected".
    ReadUnformatted,
    /// Buffer → platter.
    WriteSector,
    /// Buffer → Altair.
    ReadBuffer,
    /// Altair → buffer.
    WriteBuffer,
    /// Read one of the controller's internal IV bytes back to the Altair.
    ReadStatus,
    /// Write one of the controller's internal IV bytes.
    SetByte,
    /// Format one whole side of one platter.
    Format,
    /// Reset the controller. The one command, with Set Byte, that does not
    /// need a ready drive.
    ///
    /// Also what every command word the other seven groups do not claim decodes
    /// as, because the firmware's index is three bits wide and its table has
    /// eight entries: group 7 is the reset, and nothing falls off the end.
    Initialize,
}

impl Command {
    fn of(word: u16) -> Command {
        // **This is the firmware's own decode.** The 8X300 disassembly (deramp,
        // `88-HDSK 8X300.pdf`, with the original engineer's annotations) shows the
        // command loop take *three* bits and index a jump table:
        //
        // ```text
        //     0019: MOVE LB.IV RR 5 L 3 --> AUX   ; command bits 15:13
        //     0024: XEC  AUX + 0025               ; execute one of eight JMPs
        //     0025: JMP 002D   seek
        //     0026: JMP 0042   write sector / read sector      <- one entry
        //     0027: JMP 004D   write buffer / read buffer      <- one entry
        //     0028: JMP 0072   read status
        //     0029: JMP 007E   set byte
        //     002A: JMP 0088   (read unformatted sector)
        //     002B: JMP 0091   (format)
        //     002C: JMP 0000   reset controller (used for HOME)
        // ```
        //
        // Three consequences, and each of them corrects something here:
        //
        // 1. **The manual is wrong about the write bit, and this says why.** §3-4
        //    has Write Sector differing from Read Sector in *bit 13* — but bit 13
        //    is part of the three-bit group index, and it is 1 for both sector
        //    commands, so it cannot be the direction. The direction is **bit 12**,
        //    which the firmware tests explicitly in the buffer routine at 0053,
        //    annotated by hand "Read/WRITE?": `AND AUX AND R01 RR 4 --> R01`,
        //    where R01 holds command bits 12:8, so bit 4 of it is bit 12. Four
        //    witnesses now agree — the MITS BIOS, the disks' `CWRSEC equ 020h`,
        //    ADEXER's live `2003`, and the controller's own code — and this last
        //    one is the mechanism rather than another observation.
        // 2. **There is no unrecognised command.** Three bits index eight
        //    entries, exhaustively, so every 16-bit word is *some* command. This
        //    used to decode on four bits against a list of ten exact values, which
        //    was stricter than the board: `70xx` is a Read Status to the real
        //    controller and was nothing here.
        // 3. Bit 12 is a **don't care** outside the two transfer groups, exactly
        //    as it is spare in the manual's own tables for those commands.
        match (word >> 13) & 0x07 {
            0 => Command::Seek,
            1 => {
                if word & 0x1000 != 0 {
                    Command::ReadSector
                } else {
                    Command::WriteSector
                }
            }
            2 => {
                if word & 0x1000 != 0 {
                    Command::ReadBuffer
                } else {
                    Command::WriteBuffer
                }
            }
            3 => Command::ReadStatus,
            4 => Command::SetByte,
            5 => Command::ReadUnformatted,
            6 => Command::Format,
            _ => Command::Initialize,
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
    /// No command has run yet, so the power-on error byte is still what a read
    /// would find.
    ///
    /// Without this the all-ones answer was given to whichever status read
    /// happened to come first, *including one after a command that worked* —
    /// telling a driver its successful seek had every fault the board can
    /// report. It only means "power-on" until the board has done something.
    fresh: bool,

    /// The controller's own IV bytes, written by Set Byte and read back by Read
    /// Status.
    ///
    /// 256 of them, which is the range the diagnostic on these disks allows
    /// (`MAXIV equ 0FFh`), addressed by the low byte of either command. Table 3-B
    /// says which of those the board really has: "0-255 addr used 1-7, 17-22,
    /// 34-37". Holding all 256 is harmless and keeps the store a plain array —
    /// a guest writing an unused address gets it back, where the board would
    /// give it nothing, and no software here does that.
    ///
    /// **What this models and what it does not.** A write is remembered and a
    /// read returns it, which is exactly what the IV byte *test* in that
    /// diagnostic asks for — write a pattern, read it back, compare. It is not
    /// a model of the 8X300's own registers: on the real board some of these
    /// are inputs reflecting drive state, and its own source says so — "the IV
    /// Byte on the Disk Data Card is an input, so this test..." — while others
    /// drive the head positioner directly. Errata 88-HDSK-ME02 is specific: IV
    /// bytes 17, 18 and 19 "all invert the data", so selecting cylinder 0 means
    /// writing 255. Nothing here knows which address is which kind, and guessing
    /// at hardware semantics is how the sector layout went wrong for four
    /// hypotheses. So: the addressable store is real, the meanings are not, and
    /// no guest OS here needs them — seeking goes through Seek.
    ///
    /// **The reversed bit order in that errata must not be applied here.** It
    /// says the user data bits are reversed "from the way they appear to the
    /// Altair via the Set Byte and Read Status commands", user bit 0 being Altair
    /// bit 7. That transformation sits between the pins and the Altair, so it
    /// applies going in and again coming out: a write followed by a read is
    /// reversed twice and the Altair sees the byte it wrote. Adding a reversal to
    /// either command would break the round trip, which is the only thing this
    /// store is for.
    iv: [u8; IV_BYTES],
    /// A Set Byte waiting for its data byte, with the address it will go to.
    ///
    /// The command carries the *address* in its low byte and the *data* arrives
    /// afterwards at port 167, which is what the disk's own `WRITIV` does:
    /// `mov a,c` / `out ADATA` for the address, then the data once the
    /// write-ready flag comes up. Without this phase that data byte fell
    /// through to `pending_low` and was silently taken as half of the next
    /// command.
    iv_pending: Option<u8>,
    /// A byte Read Status has put on the read channel for the guest to take.
    ///
    /// Its own field rather than a buffer transfer: it comes out of port 165
    /// like buffer data, but it is one byte from a different place, and giving
    /// it a fake buffer would mean a status read could clobber a sector.
    status_byte: Option<u8>,

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
            fresh: true,
            iv: [0; IV_BYTES],
            iv_pending: None,
            status_byte: None,
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

    /// What a Read Status of `addr` answers.
    ///
    /// Two of these are not storage at all but the head position, read back
    /// **inverted**, which is how `GETCYL` in ADEXER recovers the current
    /// cylinder: read IV 18, complement, keep bit 0 for cylinder bit 8; read
    /// IV 19, complement, and that is the low byte. Both halves are stated twice
    /// over — by the errata ("IV Bytes H, I, and J ... all invert the data ... to
    /// select Cylinder Address 0 ... you would write 255") and by ADEXER's own
    /// equates and its `cma ;cyl address bits are inverted`.
    ///
    /// The other bits of IV 18 are output latches for the positioner's
    /// active-low lines, so they read back as last written.
    fn iv_read(&self, addr: usize, unit: u8) -> u8 {
        let cylinder = self.units.get(unit as usize).map(|u| u.cylinder).unwrap_or(0);
        let stored = self.iv[addr % IV_BYTES];
        match addr {
            iv::DISK_CONTROL_B => {
                let bit8 = ((cylinder >> 8) as u8) & iv::CYL_BIT8;
                (stored & !iv::CYL_BIT8) | (!bit8 & iv::CYL_BIT8)
            }
            iv::CYL_LOW => !(cylinder as u8),
            _ => stored,
        }
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
        // The board has now run a command, so the power-on error byte is no
        // longer what a status read should find.
        //
        // This briefly had an exception for "unrecognised" commands, to keep the
        // errata's all-ones answer alive through the 4PIO initialisation in
        // Table 3-C — which includes `OUT 163,255`. The firmware says there is no
        // such thing as unrecognised: `FF00` is group 7, the controller reset. So
        // that sequence really does reset the board, the reset really does write
        // the error byte, and the exception was protecting a behaviour the
        // hardware does not have.
        self.fresh = false;
        let unit = ((word >> 10) & 0x03) as u8;
        self.selected = unit;
        if std::env::var_os("CPM_HDSK_TRACE").is_some() {
            eprintln!("hdsk cmd {:04x} -> {:?} unit {unit}", word, Command::of(word));
        }

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
            Command::ReadSector | Command::WriteSector | Command::ReadUnformatted => {
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
                // An unformatted read is also the write-protect probe, so it
                // reports the line while still doing the read. Deliberately only
                // this command and a write: reporting it on *every* read would be
                // the more literal model of a hardware write-protect line, but
                // these images are read-only by default and a guest that checks
                // the whole error byte rather than masking it would then see every
                // read fail. ADEXER masks with 7Fh on an ordinary read and 80h on
                // this one, which is exactly the distinction being drawn here.
                if matches!(Command::of(word), Command::ReadUnformatted)
                    && self.units[unit as usize].read_only
                {
                    self.status |= error::WRITE_PROTECT;
                }
                self.pending_buffer = buffer;
                if writing {
                    // A write completes here. The machine performs it the
                    // instant this returns, before the guest can execute
                    // another instruction, so there is nothing for it to wait
                    // on — and *leaving* Ready low was a real bug: nothing
                    // raised it again, because only a read is completed by
                    // `buffer_loaded`. The guest asked for a sector to be
                    // written, never saw the command finish, and the file it
                    // was saving silently did not appear.
                    self.ready = true;
                    HostRequest::Write { drive: unit, offset, len: SECTOR_LEN }
                } else {
                    // A read is not finished until the machine has supplied the
                    // bytes, which it does through `buffer_loaded`.
                    self.ready = false;
                    HostRequest::Read { drive: unit, offset, len: SECTOR_LEN }
                }
            }
            Command::ReadBuffer | Command::WriteBuffer => {
                let raw = (word & 0xFF) as usize;
                // The one place the manual contradicts itself, and both
                // halves are quoted here so the next reader does not have to go
                // and find them. §3-4 on Read Buffer: "the transfer length 0
                // through 255 (transfer length = n-1; n=# of databytes)", which
                // makes a stored 0 mean *one* byte. Then, two paragraphs later
                // on Write Buffer: "Note that a value of 0 in the transfer
                // length implies a transfer of 256 bytes."
                //
                // The second reading is the one the hardware must have: it is the
                // ordinary behaviour of a counter loaded with 0 and counted to
                // wrap, and it is what the CP/M BIOS on these disks relies on —
                // it asks for a whole sector with a length of 0, and the files it
                // reads come back byte-exact against the guest's own PCPUT. A
                // partial transfer is unexercised by any software here, so if the
                // `n-1` reading is right for non-zero lengths, nothing we have
                // would notice.
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
                // The address is this command's low byte; the data byte follows
                // at port 167 once the write-ready flag comes up. So the
                // command is *not* finished here — `ready` stays low until the
                // byte arrives, which is exactly the sequence the errata's own
                // example waits through.
                self.iv_pending = Some((word & 0xFF) as u8);
                self.write_ready = true;
                self.ready = false;
                HostRequest::None
            }
            Command::ReadStatus => {
                // Two witnesses agree that an unready unit gets no status. The
                // diagnostic on these disks: "the Datakeeper Read Status command
                // insists that the selected Unit be ready before it will return
                // status". And errata 88-HDSK-ME02: "The Read Status command
                // will not complete if the selected Unit is not Ready."
                //
                // **We deviate on purpose, in the forgiving direction.** The
                // hardware does not *complete* — Ready never rises and the
                // caller falls out through its own timeout, which is why
                // `READIV` on the disk carries a one-second one. We complete and
                // set NOT_READY instead. The guest reaches the same conclusion
                // from the error byte, without a wait that this emulator would
                // report as a stalled controller.
                //
                // Not modelled, and specified too loosely to guess at: the same
                // errata says Read Status "rewrites 7 bits in IV Byte H (Address
                // 17)" — user bits 4-7 from the command's unit bits, user bits
                // 1-3 always 011b, user bit 0 left alone. How two unit bits
                // become four is not stated, and inventing a mapping is how the
                // sector layout went wrong four times. A guest that writes IV 17,
                // reads status, and reads IV 17 back will see what it wrote here
                // and something else on real hardware.
                let addr = (word & 0xFF) as usize;
                if !self.units.get(unit as usize).is_some_and(|u| u.present) {
                    self.status |= error::NOT_READY;
                    self.ready = true;
                    return HostRequest::None;
                }
                // Errata ME02: "the Read Status command rewrites 7 bits in IV
                // Byte H (Address 17). User Bits 4-7 get set according to the
                // Unit bits in the command byte, User Bits 1:3 always get set to
                // 011b (Extension Select low, Platter and Head select high).
                // User Bit 0 (Start/Stop all drives) is unchanged."
                //
                // The errata does not say how two unit bits become four; ADEXER's
                // equates do — `IVHUS1..IVHUS4` are one line per drive, so it is
                // a one-hot decode. Its constants are in the Altair's own bit
                // order, which is what this store holds, so they are used as
                // written.
                self.iv[iv::DISK_CONTROL_A] = (self.iv[iv::DISK_CONTROL_A]
                    & iv::START_STOP)
                    | iv::PLATTER_AND_SIDE
                    | (1u8 << unit.min(3));
                self.status_byte = Some(self.iv_read(addr, unit));
                self.read_ready = true;
                // **Ready and the byte come up together**, which is the one way
                // this differs from a Read Buffer of one — and it is not a
                // guess. `READIV` on the disk waits for Ready *first* and then
                // takes the byte, with the comment "HDCMD returns when CRDY -
                // so CDA should already be set". Modelling it like a buffer
                // transfer, finished only once the guest has read the byte,
                // leaves that routine spinning on Ready forever: a hang, and in
                // the one piece of software that uses this command at all.
                //
                // Set Byte is the opposite order — the errata's own example
                // waits on the write flag, sends the parameter, and only then
                // waits on Ready — so the two are not symmetric and cannot be
                // written from one pattern.
                self.ready = true;
                HostRequest::None
            }
            Command::Format => {
                // One whole side of one platter, which is what the utility on
                // these disks announces it is doing ("Formatting platter N,
                // side M") and what its 60-second timeout is sized for. The
                // operands are in the low byte — the source's own comment on
                // `CFORMT` reads "ADATA Bits 7:6 = Platter #, Bit 5 = Side #",
                // and its `GETHED` folds those into the same head field the
                // sector commands use, because head 0-7 *is* platter × 2 + side.
                let head = ((word >> 5) & 0x07) as u8;
                let Some(u) = self.units.get(unit as usize).copied().filter(|u| u.present) else {
                    self.status |= error::NOT_READY;
                    self.ready = true;
                    return HostRequest::None;
                };
                if u.read_only {
                    self.status |= error::WRITE_PROTECT;
                    self.ready = true;
                    return HostRequest::None;
                }
                if head >= HEADS {
                    self.status |= error::ILLEGAL_SECTOR;
                    self.ready = true;
                    return HostRequest::None;
                }
                // A surface is not contiguous in the image: this head's 24
                // sectors sit once per cylinder, a whole cylinder apart. Hence
                // the strided fill — the controller does its own address
                // arithmetic, as every other command here does, and the machine
                // only copies bytes.
                //
                // The heads finish at cylinder 0. The firmware's format entry
                // opens with the pair of writes its annotator marked "seek track
                // 0" and "cyl restore" before it formats anything, which makes
                // sense of a command that then works its way outward across the
                // whole surface.
                if let Some(u) = self.units.get_mut(unit as usize) {
                    u.cylinder = 0;
                }
                self.ready = true;
                HostRequest::Fill {
                    drive: unit,
                    offset: head as u64 * SECTORS as u64 * SECTOR_LEN as u64,
                    chunk: SECTORS as usize * SECTOR_LEN,
                    stride: HEADS as u64 * SECTORS as u64 * SECTOR_LEN as u64,
                    count: CYLINDERS as usize,
                    byte: FORMAT_FILL,
                }
            }
            Command::Initialize => {
                // A controller reset. The one command besides Set Byte that the
                // errata's error table exempts from needing a ready drive, so
                // it must not report one missing: an empty machine being
                // initialised is not a fault.
                //
                // **It also brings the heads home.** The firmware's group-7 entry
                // jumps to 0000, and the first thing the code there does is the
                // sequence its annotator labelled "CYL. RESTORE" — which is why
                // the same annotation calls this entry "reset controller (used for
                // HOME)". A driver that resets the board and then reads without
                // seeking is entitled to be at cylinder 0.
                for u in self.units.iter_mut() {
                    if u.present {
                        u.cylinder = 0;
                    }
                }
                self.transfer = None;
                self.status_byte = None;
                self.iv_pending = None;
                self.read_ready = false;
                self.write_ready = false;
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
                // Errata ME03's all-ones answer, but only while it is still
                // true: once a command has run, the error byte is that
                // command's result and handing back 0xFF instead would report
                // every fault the board can name on a operation that worked.
                if self.first_status_read && self.fresh {
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
                // A Read Status byte comes out of this port too, and it is
                // checked first: it is one byte from the IV store, not part of
                // any buffer. `ready` is deliberately not touched — that command
                // completed when it was issued, and raising the flag again here
                // would report a board ready for work it has not been given.
                if let Some(b) = self.status_byte.take() {
                    return (b, HostRequest::None);
                }
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
                // A Set Byte's data byte, if one is owed. Checked before both
                // the buffer transfer and the command-assembly fallthrough:
                // taking it as `pending_low` is precisely the bug this replaced,
                // and it would have made the byte disappear into the next
                // command word.
                if let Some(addr) = self.iv_pending.take() {
                    self.iv[addr as usize] = value;
                    // One IV write is not storage: `IVICRN`, "Cyl Restore
                    // (active low)", walks the heads back to cylinder 0. ADEXER's
                    // `RE` asserts it — its own comment is "Set the strobe line
                    // high the restore line low" — and with this inert it printed
                    // "Restoring." while the heads stayed where they were.
                    //
                    // A level, not an edge, which is why this one is modelled and
                    // the cylinder *strobe* beside it is not: a strobe is a low
                    // pulse and its timing would have to be inferred, while every
                    // guest operating system here seeks with the Seek command
                    // instead. Restore needs no such guess.
                    if addr as usize == iv::DISK_CONTROL_B && value & iv::CYL_RESTORE == 0 {
                        let selected = self.selected;
                        if let Some(u) = self.units.get_mut(selected as usize) {
                            if u.present {
                                u.cylinder = 0;
                            }
                        }
                    }
                    self.ready = true;
                    return HostRequest::None;
                }
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

    fn media(&self) -> Vec<Medium> {
        // One medium, and the trailer allowance comes with it: demanding an
        // exact length was the same mistake that locked out both CP/M 3 disks
        // and every minidisk on the floppy side. Images in circulation carry a
        // few bytes past the last sector, and a hard disk is no less likely to
        // have been copied by something that padded it.
        vec![Medium {
            bytes: IMAGE_LEN,
            label: "Altair 88-HDSK hard disk",
            trailer: SECTOR_LEN as u64 - 1,
            shape: format!(
                "{CYLINDERS} cylinders x {HEADS} heads x {SECTORS} sectors x {SECTOR_LEN}"
            ),
        }]
    }

    fn insert(&mut self, drive: u8, image_len: u64, read_only: bool) -> Result<(), String> {
        // Asked of `accepts` rather than re-tested, so the two cannot come to
        // disagree about what fits — which they did, in the direction that
        // matters least visibly: `accepts` claiming a disk that `insert` then
        // refused.
        if self.accepts(image_len).is_none() {
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

    fn cold_start(&self, image: &[u8]) -> ColdStart {
        // Sector 0 of every one of these disks is a volume label, and it says
        // where the boot program is rather than the location being fixed.  That
        // was found by comparing the four hard-disk images: the CP/M pair name
        // sector 7, which is where the loader really is — its `31 00 D7` is the
        // `LXI SP,0D700h` that the loader source on the same disk computes for a
        // 63 K system — and the two Disk BASIC images name sector 24, where
        // their own `F3 C3` (`DI`, then a jump) sits and where the CP/M disks
        // have nothing but zeros.  A fixed sector 7 would boot half of them.
        let Some(label) = image.get(..LABEL_LEN) else {
            return ColdStart::NoProgram;
        };
        let sector = u16::from_le_bytes([label[LABEL_BOOT_SECTOR], label[LABEL_BOOT_SECTOR + 1]]);
        let count = u16::from_le_bytes([label[LABEL_BOOT_COUNT], label[LABEL_BOOT_COUNT + 1]]);
        // A count of zero would mean loading nothing and jumping into it, and a
        // sector of zero would mean the label is its own boot program. Either
        // way this disk does not name one — which is a fact about the disk, not
        // about the controller, and is reported as such.
        if sector == 0 || count == 0 {
            return ColdStart::NoProgram;
        }
        let offset = sector as u64 * SECTOR_LEN as u64;
        let len = count as usize * SECTOR_LEN;
        // A program that does not fit on the disk is not a program, and this is
        // the case a *blank* hard disk produces: an erased platter has an erased
        // label, so both fields read `E5E5` and name 58,853 sectors starting
        // 15 MB into a 4.9 MB disk. Bounded here, where the medium's size is
        // known, because the alternative was a caller reporting "the boot
        // program runs past the end" — which reads as a fault in the gateway
        // when the honest answer is that the disk is empty.
        if offset.saturating_add(len as u64) > image.len() as u64 {
            return ColdStart::NoProgram;
        }
        // Both stages of every disk here are loaded at zero, which is what the
        // CP/M loader's own source says of itself: "the hard disk bootloader ROM
        // (HDBL) loads this program into memory at address zero".
        ColdStart::Program { offset, len, load: 0x0000 }
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

    /// Read one IV byte the way `READIV` does: Read Status, then take the byte.
    fn read_iv(h: &mut Hdsk, addr: u8) -> u8 {
        read_iv_on_unit(h, addr, 0)
    }

    fn read_iv_on_unit(h: &mut Hdsk, addr: u8, unit: u8) -> u8 {
        command(h, 0x6000 | (u16::from(unit) << 10) | u16::from(addr));
        h.port_in(port::DATA_IN).0
    }

    /// Write one IV byte the way `WRITIV` does: Set Byte with the address, then
    /// the data at the data port.
    fn write_iv(h: &mut Hdsk, addr: u8, value: u8) {
        command(h, 0x8000 | u16::from(addr));
        h.port_out(port::DATA_OUT, value);
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
        let req = command(&mut h, 0x2000 | 2);
        assert_eq!(
            req,
            HostRequest::Write { drive: 0, offset: 2 * SECTOR_LEN as u64, len: SECTOR_LEN }
        );
        assert_eq!(&h.buffer(0).unwrap()[..4], &[1, 2, 3, 4]);
        // And the command must *complete*. Leaving Ready low here meant the
        // guest waited for a write it had already been given, and a file it
        // saved silently never appeared on the disk.
        assert!(ready(&mut h), "a write sector has to report finished");
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
        let req = command(&mut h, 0x2000);
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

    /// Every command the disks' own source defines, decoded as itself.
    ///
    /// The values are transcribed from the 88-HDSK source carried *on* four of
    /// these hard disks — the equate names are theirs. This is the test that
    /// would have caught the bug it was written for: testing bit 15 first made
    /// `CFORMT` and `CINIT` both decode as Set Byte, so a format reported
    /// success and erased nothing.
    #[test]
    fn test_the_eight_commands_decode_as_the_disks_define_them() {
        for (high, want, name) in [
            (0x00u8, Command::Seek, "CSEEK"),
            (0x20, Command::WriteSector, "CWRSEC"),
            (0x30, Command::ReadSector, "CRDSEC"),
            (0x40, Command::WriteBuffer, "CWRBUF"),
            (0x50, Command::ReadBuffer, "CRDBUF"),
            (0x60, Command::ReadStatus, "CRSTAT"),
            (0x80, Command::SetByte, "CSETIV"),
            (0xC0, Command::Format, "CFORMT"),
            (0xE0, Command::Initialize, "CINIT"),
        ] {
            // With the operand bits full of ones as well as empty: the decode
            // must depend on the command nibble alone.
            for low in [0x00u16, 0x00FF] {
                let word = u16::from(high) << 8 | low;
                assert_eq!(Command::of(word), want, "{name} ({word:#06x})");
            }
        }
        // And the two that used to be swallowed are distinct from Set Byte.
        assert_ne!(Command::of(0xC000), Command::of(0x8000), "format is not set-byte");
        assert_ne!(Command::of(0xE000), Command::of(0x8000), "initialize is not set-byte");
    }

    /// Set Byte then Read Status, through the handshake the errata documents.
    ///
    /// The address rides in each command's low byte and the data follows at
    /// port 167 — which is what the `WRITIV` routine on these disks does.
    #[test]
    fn test_an_iv_byte_written_by_set_byte_reads_back_through_read_status() {
        let mut h = board();
        let _ = h.port_in(port::STATUS);

        // Set IV byte 0xA8 — the address the disks' own diagnostic uses to test
        // the processor card's data bus.
        command(&mut h, 0x80A8);
        assert!(!ready(&mut h), "not finished until the data byte arrives");
        assert!(h.port_in(port::WRITE_READY).0 & 0x80 != 0, "it wants the byte");
        h.port_out(port::DATA_OUT, 0x5A);
        assert!(ready(&mut h), "and now the command is done");

        // Read Status for the same address, in the order the disk's own READIV
        // uses: wait for Ready, read the error byte, *then* take the data. Its
        // comment is explicit — "HDCMD returns when CRDY - so CDA should already
        // be set" — so both flags must be up together. Modelling this like a
        // one-byte Read Buffer, finished only once the guest has read it, leaves
        // that routine spinning on Ready forever.
        command(&mut h, 0x60A8);
        assert!(ready(&mut h), "ready comes up with the byte, not after it");
        assert!(h.port_in(port::READ_READY).0 & 0x80 != 0, "and the byte is waiting");
        assert_eq!(h.port_in(port::STATUS).0, 0, "no error");
        assert_eq!(h.port_in(port::DATA_IN).0, 0x5A, "then the byte itself");
        assert_eq!(
            h.port_in(port::READ_READY).0 & 0x80,
            0,
            "with nothing further offered"
        );
    }

    /// A Set Byte's data byte must not become half of the next command.
    ///
    /// It did: with no parameter phase the byte fell through to `pending_low`,
    /// so the *next* command was assembled from a stale low half. Proved with a
    /// command whose low byte decides which sector is read.
    #[test]
    fn test_a_set_byte_data_byte_is_not_taken_as_a_command_half() {
        let mut h = board();
        let _ = h.port_in(port::STATUS);
        command(&mut h, 0x8010);
        h.port_out(port::DATA_OUT, 0x07); // would have been a sector number

        // Now a read of sector 0, sent low-then-high as always.
        let req = command(&mut h, 0x3000);
        assert_eq!(
            req,
            HostRequest::Read { drive: 0, offset: 0, len: SECTOR_LEN },
            "the sector must come from this command, not the last data byte"
        );
    }

    /// "the Datakeeper Read Status command insists that the selected Unit be
    /// ready before it will return status" — the diagnostic's own comment, and
    /// the only software here that issues the command.
    #[test]
    fn test_read_status_refuses_an_absent_unit() {
        let mut h = board();
        let _ = h.port_in(port::STATUS);
        // Unit 1 has no disk.
        command(&mut h, 0x6000 | (1 << 10));
        assert_eq!(h.port_in(port::READ_READY).0 & 0x80, 0, "no byte is offered");
        assert_eq!(h.port_in(port::STATUS).0 & error::NOT_READY, error::NOT_READY);
    }

    /// A format erases one whole recording surface — every cylinder's worth of
    /// that head, a cylinder apart, which is why the request is strided.
    #[test]
    fn test_a_format_erases_one_whole_side() {
        let mut h = board();
        let _ = h.port_in(port::STATUS);
        // Format head 1 (platter 0, side 1): the operands are in the low byte.
        let req = command(&mut h, 0xC000 | (1 << 5));
        assert_eq!(
            req,
            HostRequest::Fill {
                drive: 0,
                offset: SECTORS as u64 * SECTOR_LEN as u64,
                chunk: SECTORS as usize * SECTOR_LEN,
                stride: HEADS as u64 * SECTORS as u64 * SECTOR_LEN as u64,
                count: CYLINDERS as usize,
                byte: 0xE5,
            }
        );
        assert!(ready(&mut h));
        // The whole surface and no more: 406 cylinders x 24 sectors x 256.
        assert_eq!(CYLINDERS as u64 * SECTORS as u64 * SECTOR_LEN as u64, IMAGE_LEN / 2);
    }

    /// The read-only default is the blunt guard a booted disk has instead of
    /// every guard that understood the guest's request. A format must meet it.
    #[test]
    fn test_a_read_only_disk_refuses_a_format() {
        let mut h = Hdsk::new();
        h.insert(0, IMAGE_LEN, true).unwrap();
        let _ = h.port_in(port::STATUS);
        assert_eq!(command(&mut h, 0xC000), HostRequest::None, "nothing is erased");
        assert_eq!(h.port_in(port::STATUS).0 & error::WRITE_PROTECT, error::WRITE_PROTECT);
    }

    /// Initialize is one of the two commands the errata's error table exempts
    /// from needing a ready drive, so an empty machine being initialised must
    /// not be reported as a fault.
    #[test]
    fn test_initialize_needs_no_disk() {
        let mut h = Hdsk::new();
        let _ = h.port_in(port::STATUS);
        assert_eq!(command(&mut h, 0xE000), HostRequest::None);
        assert!(ready(&mut h), "it completes");
        assert_eq!(h.port_in(port::STATUS).0, 0, "and reports nothing wrong");
    }

    /// It also clears whatever the board was in the middle of, which is what a
    /// driver reaches for it *for*.
    #[test]
    fn test_initialize_abandons_a_transfer_in_flight() {
        let mut h = board();
        let _ = h.port_in(port::STATUS);
        command(&mut h, 0x4000); // write buffer, 256 bytes
        assert!(h.port_in(port::WRITE_READY).0 & 0x80 != 0);
        command(&mut h, 0xE000);
        assert_eq!(h.port_in(port::WRITE_READY).0 & 0x80, 0, "the transfer is gone");
        assert!(ready(&mut h));
    }

    /// The power-on all-ones error byte must not be handed to a status read that
    /// follows a command — that reports every fault the board can name on an
    /// operation which worked.
    #[test]
    fn test_the_power_on_error_byte_does_not_mask_a_real_result() {
        let mut h = board();
        // No dummy read at init, which is the case that broke: straight to a
        // seek, then ask how it went.
        command(&mut h, 5);
        assert_eq!(h.port_in(port::STATUS).0, 0x00, "the seek worked; say so");

        // And on a board nobody has commanded, the errata's answer still holds.
        let mut fresh = board();
        assert_eq!(fresh.port_in(port::STATUS).0, 0xFF);
    }

    /// Read Unformatted Sector reads like Read Sector, and doubles as the
    /// write-protect probe.
    ///
    /// Found by tracing ADEXER: it sends `A3xx`, which decoded as unrecognised
    /// here, so its `SB` command returned nothing and — worse — its `DUMYRD`
    /// write-protect check saw a clear error byte and concluded a read-only disk
    /// was writable. Its own comment on that routine is the specification: "0 if
    /// not write protected, a=80h if write protected". The firmware confirms the
    /// shape: group 5 sets a mode register and jumps straight into the sector
    /// routine at 0043.
    #[test]
    fn test_read_unformatted_sector_reads_and_probes_write_protection() {
        let mut h = board();
        let _ = h.port_in(port::STATUS);
        let req = command(&mut h, 0xA000 | 5);
        assert_eq!(
            req,
            HostRequest::Read { drive: 0, offset: 5 * SECTOR_LEN as u64, len: SECTOR_LEN },
            "an unformatted read is still a read"
        );
        assert_eq!(h.port_in(port::STATUS).0 & error::WRITE_PROTECT, 0);

        let mut h = Hdsk::new();
        h.insert(0, IMAGE_LEN, true).unwrap();
        let _ = h.port_in(port::STATUS);
        let req = command(&mut h, 0xA000 | 5);
        assert!(matches!(req, HostRequest::Read { .. }), "the read is not refused");
        assert_eq!(
            h.port_in(port::STATUS).0 & error::WRITE_PROTECT,
            error::WRITE_PROTECT,
            "and the probe must see the protection"
        );

        // An ordinary read must NOT report it, or a guest that checks the whole
        // error byte would see every read of a read-only disk fail.
        let mut h = Hdsk::new();
        h.insert(0, IMAGE_LEN, true).unwrap();
        let _ = h.port_in(port::STATUS);
        command(&mut h, 0x3000 | 5);
        assert_eq!(h.port_in(port::STATUS).0 & error::WRITE_PROTECT, 0, "not on a plain read");
    }

    /// Read Status of IV 18 and 19 reports where the heads actually are.
    ///
    /// Reconstructed here exactly as `GETCYL` does it: read IV 18, complement,
    /// keep bit 0 as cylinder bit 8; read IV 19, complement, low byte. With these
    /// inert, ADEXER reported cylinder 511 after a seek to 3 — a stored zero,
    /// inverted, is all ones. The firmware inverts the cylinder on its way out to
    /// the positioner too, at 002F/0031, annotated "INVERT CYL. ADDRESS".
    #[test]
    fn test_read_status_reports_the_head_position_the_way_getcyl_reads_it() {
        let mut h = board();
        let _ = h.port_in(port::STATUS);

        for want in [0u16, 3, 100, 300, 405] {
            command(&mut h, want); // seek unit 0
            let msb = read_iv(&mut h, iv::DISK_CONTROL_B as u8);
            let low = read_iv(&mut h, iv::CYL_LOW as u8);
            let got = (u16::from(!msb & iv::CYL_BIT8) << 8) | u16::from(!low);
            assert_eq!(got, want, "GETCYL must recover cylinder {want}");
        }
    }

    /// Pulling `IVICRN` low walks the heads back to cylinder 0 — ADEXER's `RE`.
    ///
    /// With IV writes as pure storage it printed "Restoring." and the heads stayed
    /// where they were.
    #[test]
    fn test_an_iv_restore_returns_the_heads_to_cylinder_zero() {
        let mut h = board();
        let _ = h.port_in(port::STATUS);
        command(&mut h, 100);
        assert_eq!(h.units[0].cylinder, 100);

        // Restore is active low, so it is *clearing* that bit that asks for it.
        write_iv(&mut h, iv::DISK_CONTROL_B as u8, !iv::CYL_RESTORE);
        assert_eq!(h.units[0].cylinder, 0, "the heads must come home");

        // And an idle write, with restore high, moves nothing.
        command(&mut h, 42);
        write_iv(&mut h, iv::DISK_CONTROL_B as u8, 0xFF);
        assert_eq!(h.units[0].cylinder, 42, "restore not asserted, so nothing happens");
    }

    /// Errata ME02: Read Status rewrites seven bits of IV byte 17 — the unit
    /// select lines one-hot, platter and side high, extension low, start/stop
    /// left alone.
    #[test]
    fn test_read_status_rewrites_the_unit_select_byte() {
        let mut h = Hdsk::new();
        h.insert(0, IMAGE_LEN, false).unwrap();
        h.insert(2, IMAGE_LEN, false).unwrap();
        let _ = h.port_in(port::STATUS);

        // Set the start/stop bit, which the rewrite must preserve.
        write_iv(&mut h, iv::DISK_CONTROL_A as u8, 0xFF);

        let _ = read_iv_on_unit(&mut h, iv::DISK_CONTROL_A as u8, 2);
        let v = read_iv_on_unit(&mut h, iv::DISK_CONTROL_A as u8, 2);
        assert_eq!(v & iv::START_STOP, iv::START_STOP, "start/stop is unchanged");
        assert_eq!(v & 0x70, iv::PLATTER_AND_SIDE, "extension low, platter and side high");
        assert_eq!(v & 0x0F, 1 << 2, "unit 2 selects the third drive line, one-hot");
    }

    /// The command index is **three bits**, and its table is exhaustive.
    ///
    /// Taken from the firmware, which does `MOVE LB.IV RR 5 L 3 --> AUX` and then
    /// `XEC AUX + 0025` into eight `JMP`s. Two things follow that a four-bit
    /// decode got wrong: bit 12 is a **don't care** outside the two transfer
    /// groups, and **nothing is unrecognised** — every word is some command.
    #[test]
    fn test_the_command_index_is_three_bits_and_bit_twelve_is_spare() {
        for (high, want, why) in [
            (0x00u8, Command::Seek, "CSEEK"),
            (0x10, Command::Seek, "group 0 with bit 12 spare"),
            (0x60, Command::ReadStatus, "CRSTAT"),
            (0x70, Command::ReadStatus, "group 3 with bit 12 spare"),
            (0x80, Command::SetByte, "CSETIV"),
            (0x90, Command::SetByte, "group 4 with bit 12 spare"),
            (0xA0, Command::ReadUnformatted, "CRUSEC"),
            (0xB0, Command::ReadUnformatted, "group 5 with bit 12 spare"),
            (0xC0, Command::Format, "CFORMT"),
            (0xD0, Command::Format, "group 6 with bit 12 spare"),
            (0xE0, Command::Initialize, "CINIT"),
            (0xF0, Command::Initialize, "group 7 with bit 12 spare"),
        ] {
            let word = u16::from(high) << 8;
            assert_eq!(Command::of(word), want, "{why} ({word:#06x})");
        }
    }

    /// The direction is **bit 12**, and bit 13 cannot be it.
    ///
    /// The manual says Write Sector differs from Read Sector in bit 13. The
    /// firmware's three-bit group index *contains* bit 13, and it is 1 for both
    /// sector commands — so the claim is impossible on its own terms. The
    /// firmware tests bit 12, in the buffer routine its annotator marked
    /// "Read/WRITE?".
    #[test]
    fn test_the_direction_bit_is_twelve_and_bit_thirteen_selects_the_group() {
        // Both sector commands sit in group 1, so bit 13 is set in both.
        assert_eq!((0x2000u16 >> 13) & 7, 1, "write sector is group 1");
        assert_eq!((0x3000u16 >> 13) & 7, 1, "read sector is group 1");
        assert_ne!(0x2000u16 & 0x2000, 0, "and bit 13 is set in the write");
        assert_ne!(0x3000u16 & 0x2000, 0, "and in the read");

        // Only bit 12 tells them apart — for sectors and for buffers alike.
        for (write, read) in [(0x2000u16, 0x3000u16), (0x4000, 0x5000)] {
            assert_eq!(write ^ read, 0x1000, "the pair differs in bit 12 alone");
            assert!(matches!(
                (Command::of(write), Command::of(read)),
                (Command::WriteSector, Command::ReadSector)
                    | (Command::WriteBuffer, Command::ReadBuffer)
            ));
        }
    }

    /// The manual's own 4PIO initialisation issues a controller reset.
    ///
    /// Table 3-C tells a driver to set the port directions with, among others,
    /// `OUT 163,255` — and `FF00` is group 7, which the firmware's table sends to
    /// the reset entry at 0000. So the documented init really does reset the
    /// board. This test exists because the opposite was briefly implemented here:
    /// `FF00` was treated as unrecognised in order to keep errata ME03's
    /// power-on all-ones alive through the init, which was protecting a behaviour
    /// the hardware does not have.
    #[test]
    fn test_the_documented_4pio_init_resets_the_controller() {
        let mut h = board();
        // Put the heads somewhere and start a transfer, so the reset is visible.
        command(&mut h, 100);
        command(&mut h, 0x4000);
        assert!(h.port_in(port::WRITE_READY).0 & 0x80 != 0, "a transfer is open");

        // The init sequence's command-port write.
        h.port_out(port::DATA_OUT, 255);
        h.port_out(port::COMMAND, 255);

        assert_eq!(h.units[0].cylinder, 0, "a reset brings the heads home");
        assert_eq!(h.port_in(port::WRITE_READY).0 & 0x80, 0, "and abandons the transfer");
        assert!(ready(&mut h));
    }

    /// Initialize brings the heads home — the firmware's group-7 entry jumps to
    /// 0000, whose opening sequence its annotator labelled "CYL. RESTORE", and
    /// the same note calls the entry "reset controller (used for HOME)".
    #[test]
    fn test_initialize_brings_the_heads_home() {
        let mut h = Hdsk::new();
        h.insert(0, IMAGE_LEN, false).unwrap();
        h.insert(1, IMAGE_LEN, false).unwrap();
        let _ = h.port_in(port::STATUS);
        command(&mut h, 100);
        command(&mut h, (1 << 10) | 42);
        assert_eq!((h.units[0].cylinder, h.units[1].cylinder), (100, 42));

        command(&mut h, 0xE000);
        assert_eq!(
            (h.units[0].cylinder, h.units[1].cylinder),
            (0, 0),
            "a controller reset restores every drive it has"
        );
    }

    /// And a format leaves them there too: the firmware's format entry opens with
    /// the writes marked "seek track 0" and "cyl restore" before it formats.
    #[test]
    fn test_a_format_leaves_the_heads_at_cylinder_zero() {
        let mut h = board();
        let _ = h.port_in(port::STATUS);
        command(&mut h, 200);
        assert_eq!(h.units[0].cylinder, 200);
        command(&mut h, 0xC000);
        assert_eq!(h.units[0].cylinder, 0, "a format starts by homing the heads");
    }

    /// Images in circulation carry a few bytes past the last sector. Demanding
    /// an exact length locked out both CP/M 3 disks and every minidisk on the
    /// floppy side; the trait asks every controller not to repeat it.
    #[test]
    fn test_a_short_trailer_is_still_a_hard_disk() {
        let h = Hdsk::new();
        assert!(h.accepts(IMAGE_LEN).is_some(), "exact");
        assert!(h.accepts(IMAGE_LEN + 96).is_some(), "96-byte trailer");
        assert!(h.accepts(IMAGE_LEN + SECTOR_LEN as u64 - 1).is_some(), "the largest trailer");
        assert!(
            h.accepts(IMAGE_LEN + SECTOR_LEN as u64).is_none(),
            "a whole extra sector is a different disk, not a trailer"
        );
        assert!(h.accepts(IMAGE_LEN - 1).is_none(), "and short is never rounded up");
        // `insert` must agree, because it asks the same question.
        let mut h = Hdsk::new();
        assert!(h.insert(0, IMAGE_LEN + 96, false).is_ok());
        assert!(h.insert(0, IMAGE_LEN - 1, false).is_err());
    }

    /// Each unit keeps its own head position. A controller that kept one
    /// cylinder for the board would read the right sector of the wrong track as
    /// soon as a guest used two hard disks.
    #[test]
    fn test_each_unit_remembers_its_own_cylinder() {
        let mut h = Hdsk::new();
        h.insert(0, IMAGE_LEN, false).unwrap();
        h.insert(1, IMAGE_LEN, false).unwrap();
        let _ = h.port_in(port::STATUS);

        command(&mut h, 5); // unit 0 to cylinder 5
        command(&mut h, (1 << 10) | 9); // unit 1 to cylinder 9

        let req = command(&mut h, 0x3000 | (1 << 10) | 3); // read unit 1, sector 3
        let want = ((9u64 * HEADS as u64) * SECTORS as u64 + 3) * SECTOR_LEN as u64;
        assert_eq!(req, HostRequest::Read { drive: 1, offset: want, len: SECTOR_LEN });

        let req = command(&mut h, 0x3000 | 3); // read unit 0, sector 3
        let want = ((5u64 * HEADS as u64) * SECTORS as u64 + 3) * SECTOR_LEN as u64;
        assert_eq!(req, HostRequest::Read { drive: 0, offset: want, len: SECTOR_LEN });
    }

    /// A disk whose label names no boot program is a data disk — a fact about
    /// the disk. Reporting it as "no controller can cold-start this" sends the
    /// reader after missing code of ours.
    #[test]
    fn test_a_disk_that_names_no_boot_program_says_which_it_is() {
        let h = Hdsk::new();
        // Big enough to hold the program the label will name, since a program
        // that does not fit is itself a reason to say NoProgram.
        let mut image = vec![0u8; 8 * SECTOR_LEN];
        assert_eq!(h.cold_start(&image), ColdStart::NoProgram, "an empty label");

        image[LABEL_BOOT_SECTOR] = 7;
        assert_eq!(h.cold_start(&image), ColdStart::NoProgram, "a count of zero");

        image[LABEL_BOOT_COUNT] = 1;
        assert_eq!(
            h.cold_start(&image),
            ColdStart::Program { offset: 7 * SECTOR_LEN as u64, len: SECTOR_LEN, load: 0 },
            "sector 7, one sector, at zero — the CP/M pair's own label"
        );
        assert_eq!(h.cold_start(&[]), ColdStart::NoProgram, "and no label at all");

        // The blank-disk case, which is the one that reaches a user: an erased
        // platter has an erased label, so both fields read E5E5 and name a
        // program 15 MB into a 4.9 MB disk.
        let blank = vec![0xE5u8; IMAGE_LEN as usize];
        assert_eq!(h.cold_start(&blank), ColdStart::NoProgram, "a formatted, empty disk");
        // And a label naming one sector too many is refused on the same ground.
        let mut over = vec![0u8; 8 * SECTOR_LEN];
        over[LABEL_BOOT_SECTOR] = 7;
        over[LABEL_BOOT_COUNT] = 2;
        assert_eq!(h.cold_start(&over), ColdStart::NoProgram, "one sector past the end");
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
