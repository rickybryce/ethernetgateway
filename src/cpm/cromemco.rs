//! The Cromemco 4FDC / 16FDC floppy disk controller.
//!
//! The fourth and last board on the disk-controller plan, and the second user of
//! the FD1771/1793 in [`super::wd1771`] — which is the reason that chip was made
//! a module of its own rather than folded into the Tarbell.
//!
//! Measured from the three `CDISK*` images' own code: the boot sector each disk
//! carries, and the driver inside the operating system it loads. That is the same
//! class of evidence that settled the 88-HDSK write bit and the Tarbell's
//! drive-select polarity, and it is used here the same way — what a working
//! driver does to a register is that register's definition as far as anything we
//! have to satisfy is concerned. Published Cromemco documentation is a
//! cross-check, never the transcription source; same clean-room posture as
//! everything else here except the deliberately-derived z80pack device.
//!
//! # The registers
//!
//! ```text
//!     30h   command (out) / status (in)   \
//!     31h   track                          |  the WD chip's four registers
//!     32h   sector                         |
//!     33h   data                          /
//!     34h   disk control (out) / disk status (in)
//!     04h   auxiliary drive latch (out) / drive status (in)
//! ```
//!
//! # Where the boot sector goes, and how that was settled
//!
//! **0080h**, entered at 0080h — not 0000h, which is where every other board
//! here loads. It is worth recording how that was established, because it was not
//! a guess and it is not something the sector announces.
//!
//! Each of the three loaders reads two absolute bytes, `LD A,(00FCh)` and
//! `LD A,(00FAh)`, and compares each with `44h` — ASCII `D`. Load the sector at
//! `0080h` and those two addresses land on its own bytes at sector offsets `7Ch`
//! and `7Ah`, which on the two 8" disks read `LGSSDD` and `LGDSDD`: LarGe,
//! Single/Double Sided, Double Density. The flags then match all three disks'
//! documented formats exactly — `CDISK02` single-sided double-density,
//! `CDISK03` double-sided double-density, `CDISK01` neither. Load the sector
//! anywhere else and both reads land on memory the loader never wrote.
//!
//! Two further things fall into place at that address and nowhere else: the
//! loader's own restart target resolves to the front of the sector, and
//! `CDISK01`'s exit branch resolves to `0100h`, the address it has just loaded
//! its operating system at. `0080h` is also the CP/M default DMA address, which
//! is presumably why the ROM used it.
//!
//! # Two densities on one disk
//!
//! Track 0 of a Cromemco double-density floppy is recorded **single**-density, so
//! that a single-density boot ROM can read it at all; everything after it is
//! double-density. The loaders say so directly — each reads one track, and only
//! then does `SET 6,D`, setting the density bit in the value it writes to the
//! control port for every track afterwards.
//!
//! The arithmetic agrees exactly, which is what makes this measured rather than
//! reasoned: 3,328 + 76 × 8,192 is 625,920, the length of `CDISK02`, and
//! 3,328 + 153 × 8,192 is 1,256,704, the length of `CDISK03`. Both directories
//! then begin at 11,520 — the third track — which is where two reserved tracks
//! put them.
//!
//! # What a sector is
//!
//! 512 bytes, 16 to a track, on the double-density tracks. `CDISK03`'s BIOS says
//! so in its SETSEC: it stores the CP/M record number and then `SRL A / SRL A`,
//! dividing by four to reach a physical sector, and four 128-byte records to a
//! sector is a 512-byte sector. 16 × 512 is the 8,192 the size arithmetic above
//! already required.
//!
//! # The density bit is latched but not obeyed
//!
//! The control port's bit 6 selects the data separator, and on real hardware
//! setting it wrong means the ID fields cannot be read at all. Here the **medium**
//! is authoritative: the geometry table knows which tracks of which disk are
//! recorded which way, so a driver that asks for the wrong one is answered
//! correctly rather than with an error. That is more forgiving than the board and
//! never less, the same direction the Tarbell's WAIT port and the 88-HDSK's status
//! flags chose. The bit is still latched, because it is real state a driver may
//! read back, and because the next measurement that needs it should find it here.
//!
//! # What is deliberately not here
//!
//! **The RDOS monitor ROM at `C000h`.** The plan for this board assumed one would
//! be needed, and the disks say otherwise. Every one of the three boot sectors
//! opens with `LD A,01h / OUT (40h),A` before it does anything else, and
//! `CDISK03` then loads its operating system across `B380h`–`CCFFh` — straight
//! through `C000h`. A ROM mapped there could not survive that, so the write to
//! `40h` is what removes it, and by the time any loaded code runs there is no ROM
//! in the address space to call. We never map one in, so that write has nothing
//! to undo; port `40h` is left to the machine's unclaimed-port handling rather
//! than modelled as a register this board does not own.

use super::controller::{ColdStart, Controller, HostRequest, Medium};
use super::wd1771::{Need, Wd1771};

/// The four chip registers, and the board's own two.
const PORT_CHIP_BASE: u8 = 0x30;
const PORT_CHIP_TOP: u8 = 0x33;
/// Disk control on write, disk status on read.
const PORT_CONTROL: u8 = 0x34;
/// The auxiliary drive latch — side select out, a not-ready line in.
const PORT_AUX: u8 = 0x04;

/// Drives the board can address.
const DRIVES: usize = 4;

// ---- the control register, `34h` on write ------------------------------

/// Drive select, one bit per drive.
///
/// One-hot, and that is a deduction rather than a convention taken on trust: the
/// value every boot sector writes is `31h`, whose low nibble is `0001`, and each
/// of them is at that moment reading the disk it was booted from — drive 0. Read
/// the field as a binary drive number and `31h` selects drive **1**, which is
/// empty, and nothing would boot at all.
const CTL_DRIVE: u8 = 0x0F;
/// Double density. Set by each loader only after it has read track 0.
const CTL_DOUBLE_DENSITY: u8 = 0x40;
/// Enable the auto-wait: reading the data register stalls the CPU until the chip
/// has a byte.
///
/// Each loader sets it with `OR 80h` in the instruction before it issues a read,
/// and clears it again for the seek. **We cannot stall an instruction and do not
/// need to** — the sector is already in memory by the time the guest asks — so
/// this is latched and not acted on, exactly as the Tarbell's WAIT port reasons.
const CTL_AUTO_WAIT: u8 = 0x80;

// ---- the disk status register, `34h` on read ---------------------------

/// The chip has finished. Bit 0, and the one bit here that is load-bearing:
/// every loader's inner loop is `IN A,(34h) / RRA` and branches on the carry.
const ST_INTRQ: u8 = 0x01;
/// A byte is waiting. Bit 1, from `CDISK01`'s driver, which checks it with
/// `BIT 1,A` at the point its own byte count has run out.
const ST_DRQ: u8 = 0x02;
/// Bit 2 must read **clear**.
///
/// Not a guess about what it means — a measurement of what happens if it is set.
/// `CDISK01`'s seek wait is `IN A,(34h) / BIT 2,A / JR NZ,<give up> / RRA /
/// JR NC,<keep waiting>`, so a set bit abandons every seek the driver makes. It
/// is named for the only thing established about it.
const ST_MUST_BE_CLEAR: u8 = 0x04;
/// The motor is up to speed. Bit 5.
///
/// Inferred, and safe either way: the one place it is read, `CDISK01` skips a
/// delay loop when it is set and runs the delay when it is not. Reported set
/// because nothing here spins up.
const ST_MOTOR_ON: u8 = 0x20;

// ---- the auxiliary latch, `04h` ----------------------------------------

/// Side select, in the value written to `04h`. **Set selects side 0.**
///
/// The loaders establish both the bit and its sense. Each starts with `7Fh` in
/// `E`, writes it to `04h`, and uses `BIT 1,E` to decide whether to seek: on a
/// double-sided disk it does `XOR 02h` after every track, so the sides alternate
/// and the head only steps on the half of the cycle where the bit is set. The
/// first track read — cylinder 0, side 0 — is the one with the bit set.
const AUX_SIDE_0: u8 = 0x02;

/// Bit 6 of `04h` on read is a not-ready line, and must read clear.
///
/// Established the same way as `ST_MUST_BE_CLEAR`: `CDISK01`'s driver spins
/// `IN A,(04h) / AND 40h / JR NZ,<again>` with no way out, so a set bit hangs it.
/// This is also what proves the port is **not** a read-back of the latch — the
/// driver writes values with bit 6 set and then waits for bit 6 to clear, which
/// could never happen if a read returned what was written.
const AUX_NOT_READY: u8 = 0x40;

/// What `04h` reads.
///
/// Zero, and that is the whole answer: bit 6 clear is the only thing established
/// about this register, and inventing bits a driver might test is how a board
/// starts lying. `test_the_auxiliary_port_does_not_read_back_what_was_written`
/// holds it to the one property that was measured.
const AUX_IDLE: u8 = 0x00;

// ---- geometry ----------------------------------------------------------

/// The single-density format: the IBM 3740 one, and what track 0 always is.
const SD_SECTOR_LEN: usize = 128;
const SD_SECTORS: usize = 26;
/// 26 × 128.
const SD_TRACK_BYTES: u64 = SD_SECTOR_LEN as u64 * SD_SECTORS as u64;

/// The double-density format. See the module comment for where 512 comes from.
const DD_SECTOR_LEN: usize = 512;
const DD_SECTORS: usize = 16;
/// 16 × 512.
const DD_TRACK_BYTES: u64 = DD_SECTOR_LEN as u64 * DD_SECTORS as u64;

/// Cylinders on every Cromemco disk here.
const CYLINDERS: u8 = 77;

/// One medium this board takes.
///
/// The three are distinguished by length alone, and no two are the same length —
/// so a file names its geometry the way the mount path's formats do. `sides` and
/// `double_density` are then everything needed to place a track, because the
/// layout rule is the same for all three: cylinder by cylinder, both sides of a
/// cylinder together, track 0 single-density and the rest at the disk's own
/// density.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Format {
    bytes: u64,
    label: &'static str,
    sides: u8,
    double_density: bool,
}

impl Format {
    /// Bytes in one track after track 0.
    fn track_bytes(self) -> u64 {
        if self.double_density {
            DD_TRACK_BYTES
        } else {
            SD_TRACK_BYTES
        }
    }

    /// How the track at `index` is recorded: sector length and sector count.
    ///
    /// Index 0 — cylinder 0, side 0 — is single-density on every Cromemco disk,
    /// including the double-density ones. That is not a special case bolted on;
    /// it is the reason a single-density boot ROM can read a double-density disk
    /// at all.
    fn track_format(self, index: usize) -> (usize, usize) {
        if index > 0 && self.double_density {
            (DD_SECTOR_LEN, DD_SECTORS)
        } else {
            (SD_SECTOR_LEN, SD_SECTORS)
        }
    }

    /// Where the track at `index` begins in the file.
    fn track_offset(self, index: usize) -> u64 {
        if index == 0 {
            0
        } else {
            SD_TRACK_BYTES + (index as u64 - 1) * self.track_bytes()
        }
    }

    /// Tracks on the whole disk.
    fn tracks(self) -> usize {
        CYLINDERS as usize * self.sides as usize
    }

    /// How the size is made up, for the generated readme.
    ///
    /// Kept inside the readme's 80-column budget, which a test enforces — and
    /// the mixed-density note is only worth its width on the disks that have
    /// one. Saying "track 0 is 26x128, the rest 26x128" of a single-density disk
    /// is both longer and less clear.
    fn shape(self) -> String {
        let sides = if self.sides == 2 { "2 sides" } else { "1 side" };
        if self.double_density {
            let (len, n) = self.track_format(1);
            format!(
                "{CYLINDERS} cyl x {sides}, track 0 {SD_SECTORS}x{SD_SECTOR_LEN} then {n}x{len}"
            )
        } else {
            format!("{CYLINDERS} cyl x {sides} x {SD_SECTORS} sectors x {SD_SECTOR_LEN}")
        }
    }
}

/// Every medium this board takes, measured from the three sample disks.
///
/// `CDISK01`'s 256,256 bytes is 77 × 26 × 128 — which is also a Tarbell disk and
/// also a z80pack disk, and is why a machine has to say which boards it carries.
/// Udo Munk's index calls that disk a 5.25" one; the file is laid out as 77
/// tracks of 26 sectors, and its directory begins at 6,656 — exactly two such
/// tracks in — so that is what is modelled. The label says what the layout is
/// rather than what drive it may once have spun in, which is also why that one
/// carries no drive size while the other two do: 77 tracks of 16 × 512 is an 8"
/// format and a 5.25" one of the period is 40 tracks, so those two are placed by
/// their own geometry and the first is not placed at all.
///
/// **The labels are abbreviated because a screen says so** — see
/// `bootable_size_lines`, which owns that budget and the test that enforces it.
/// Spelling these out (`Cromemco 8" double-sided double-density floppy`) overran
/// it, so they use the period abbreviations the neighbouring
/// `z80pack 8" SSSD, 241K` already does. `web/diskreference.html` spells each
/// one out in full.
const FORMATS: &[Format] = &[
    Format {
        bytes: 256_256,
        label: "Cromemco SSSD floppy",
        sides: 1,
        double_density: false,
    },
    Format {
        bytes: 625_920,
        label: "Cromemco 8\" SSDD floppy",
        sides: 1,
        double_density: true,
    },
    Format {
        bytes: 1_256_704,
        label: "Cromemco 8\" DSDD floppy",
        sides: 2,
        double_density: true,
    },
];

/// One drive.
#[derive(Debug, Clone, Copy)]
struct Drive {
    present: bool,
    read_only: bool,
    /// What is in it. `None` when the drive is empty.
    format: Option<Format>,
}

/// The board.
pub struct Cromemco {
    chip: Wd1771,
    drives: [Drive; DRIVES],
    selected: u8,
    /// The control register, as last written.
    control: u8,
    /// The auxiliary latch, as last written. Powers up with side 0 selected,
    /// which is what the loaders write before they do anything.
    aux: u8,
    /// Status reads that found nothing to report.
    idle_polls: u32,
}

impl Default for Cromemco {
    fn default() -> Cromemco {
        Cromemco::new()
    }
}

impl Cromemco {
    pub fn new() -> Cromemco {
        Cromemco {
            chip: Wd1771::new(),
            drives: [Drive { present: false, read_only: true, format: None }; DRIVES],
            selected: 0,
            control: 0,
            aux: AUX_SIDE_0,
            idle_polls: 0,
        }
    }

    /// Which format is in the selected drive.
    fn format(&self) -> Option<Format> {
        self.drives[self.selected as usize % DRIVES].format
    }

    /// Which side the auxiliary latch has selected.
    ///
    /// Always 0 on a single-sided disk, whatever the latch says: there is no other
    /// surface for a driver to reach, and letting the bit address one would place
    /// every track at the wrong offset rather than failing.
    fn side(&self) -> u8 {
        match self.format() {
            Some(f) if f.sides > 1 && self.aux & AUX_SIDE_0 == 0 => 1,
            _ => 0,
        }
    }

    /// The index of the track under the head, in the order the file stores them.
    ///
    /// Cylinder by cylinder with both sides of a cylinder adjacent — which is the
    /// order the loaders read in, alternating the side bit between steps.
    fn track_index(&self, cylinder: u8) -> Option<usize> {
        let f = self.format()?;
        if cylinder >= CYLINDERS {
            return None;
        }
        let index = cylinder as usize * f.sides as usize + self.side() as usize;
        (index < f.tracks()).then_some(index)
    }

    /// Byte offset of a sector on the track under the head.
    fn offset(&self, cylinder: u8, sector: u8) -> Option<(u64, usize)> {
        let f = self.format()?;
        let index = self.track_index(cylinder)?;
        let (len, count) = f.track_format(index);
        if sector == 0 || sector as usize > count {
            return None;
        }
        let at = f.track_offset(index) + (sector as u64 - 1) * len as u64;
        Some((at, len))
    }

    /// Tell the chip what the drive and the track under the head are like.
    ///
    /// Both halves have to happen before a command is judged, and the format half
    /// is the one that is easy to forget: the chip bounds the sector register
    /// against the track's sector count, and a 16-sector double-density track
    /// judged as a 26-sector single-density one accepts sectors that are not
    /// there.
    fn refresh(&mut self) {
        let d = self.drives[self.selected as usize % DRIVES];
        self.chip.set_drive(d.present, d.read_only, CYLINDERS);
        self.chip.set_side(self.side());
        if let Some(f) = d.format {
            let index = self.track_index(self.chip.track()).unwrap_or(0);
            let (len, count) = f.track_format(index);
            self.chip.set_format(len, count);
        }
    }

    /// Turn what the chip wants into what the machine can do.
    ///
    /// `ahead` says whether the access that produced this has already answered
    /// the guest. It is true for a fetch triggered by *reading* the data
    /// register, which is how a multiple-record transfer collects its next
    /// sector — the byte that read returned is the last one of the sector that
    /// just finished, and the machine must not replace it. See
    /// [`HostRequest::ReadAhead`].
    fn serve(&mut self, need: Need, ahead: bool) -> HostRequest {
        let (track, sector, writing) = match need {
            Need::None => return HostRequest::None,
            Need::Read { track, sector } => (track, sector, false),
            Need::Write { track, sector } => (track, sector, true),
        };
        let drive = self.selected;
        match self.offset(track, sector) {
            Some((offset, len)) if writing => HostRequest::Write { drive, offset, len },
            Some((offset, len)) if ahead => HostRequest::ReadAhead { drive, offset, len },
            Some((offset, len)) => HostRequest::Read { drive, offset, len },
            // Nothing on the medium answers to that address. The chip bounds the
            // sector against the track's own count and the seek against the
            // cylinder count, so this is not reachable from a guest today; it
            // exists so that a future geometry whose two halves disagree fails by
            // *not moving data* rather than by reading the wrong bytes.
            //
            // Deliberately not dressed up as an error: nothing here sets a status
            // bit, so a guest that did reach it would poll a busy chip until the
            // stuck-poll detector noticed. That is the honest description, and a
            // worse outcome than a seek error — which is exactly why the two
            // bounds above are the things to keep correct.
            None => HostRequest::None,
        }
    }

    /// The disk status register: what the board reports about the chip.
    fn disk_status(&mut self) -> u8 {
        let mut s = ST_MOTOR_ON;
        if self.chip.intrq() {
            s |= ST_INTRQ;
        }
        if self.chip.drq() {
            s |= ST_DRQ;
        }
        if s & (ST_INTRQ | ST_DRQ) == 0 {
            // Nothing to report. On a driver's inner loop that is a wait, and a
            // wait that never ends is indistinguishable from a crashed CPU unless
            // somebody counts it.
            self.idle_polls = self.idle_polls.saturating_add(1);
        } else {
            self.idle_polls = 0;
        }
        debug_assert_eq!(s & ST_MUST_BE_CLEAR, 0, "bit 2 set abandons every seek a driver makes");
        s
    }
}

impl Controller for Cromemco {
    fn name(&self) -> &'static str {
        "Cromemco 4FDC/16FDC floppy"
    }

    fn owns_port(&self, port: u8) -> bool {
        (PORT_CHIP_BASE..=PORT_CHIP_TOP).contains(&port) || port == PORT_CONTROL || port == PORT_AUX
    }

    fn port_in(&mut self, port: u8) -> (u8, HostRequest) {
        match port {
            PORT_CONTROL => (self.disk_status(), HostRequest::None),
            PORT_AUX => {
                debug_assert_eq!(
                    AUX_IDLE & AUX_NOT_READY,
                    0,
                    "a set bit 6 hangs CDISK01's driver in a wait with no way out"
                );
                (AUX_IDLE, HostRequest::None)
            }
            _ => {
                let (value, need) = self.chip.read(port & 0x03);
                // `true`: this read has already answered with a real byte.
                let req = self.serve(need, true);
                (value, req)
            }
        }
    }

    fn port_out(&mut self, port: u8, value: u8) -> HostRequest {
        match port {
            PORT_CONTROL => {
                self.control = value;
                // One-hot: see `CTL_DRIVE`. A value naming no drive leaves the
                // selection alone rather than picking one, because "deselect
                // everything" is a real thing for a driver to write and guessing
                // a drive for it would act on a disk nobody asked about.
                if let Some(bit) = (0..DRIVES as u8).find(|i| value & CTL_DRIVE & (1 << i) != 0) {
                    self.selected = bit;
                }
                self.refresh();
                HostRequest::None
            }
            PORT_AUX => {
                self.aux = value;
                // The side moves the head to a different surface, so what the
                // chip believes about the track under it has to be re-derived.
                self.refresh();
                HostRequest::None
            }
            _ => {
                if port & 0x03 == super::wd1771::reg::COMMAND {
                    // A command is judged against the drive and the track that
                    // are actually selected.
                    self.refresh();
                    if std::env::var_os("CPM_CROMEMCO_TRACE").is_some() {
                        eprintln!(
                            "cromemco cmd {value:02x} drive {} cyl {} side {} sector {} \
                             ctl {:02x} ({}{})",
                            self.selected,
                            self.chip.track(),
                            self.side(),
                            self.chip.sector(),
                            self.control,
                            // The guest's own view of the density, which the
                            // medium overrides — so a disagreement is exactly
                            // what a bring-up trace needs to show.
                            if self.control & CTL_DOUBLE_DENSITY != 0 { "DD" } else { "SD" },
                            if self.control & CTL_AUTO_WAIT != 0 { " autowait" } else { "" },
                        );
                    }
                }
                let need = self.chip.write(port & 0x03, value);
                self.serve(need, false)
            }
        }
    }

    fn media(&self) -> Vec<Medium> {
        FORMATS
            .iter()
            .map(|f| Medium {
                bytes: f.bytes,
                label: f.label,
                // Less than the smallest sector on the disk, so a trailer can
                // never be mistaken for one more sector of data.
                trailer: SD_SECTOR_LEN as u64 - 1,
                shape: f.shape(),
            })
            .collect()
    }

    fn insert(&mut self, drive: u8, image_len: u64, read_only: bool) -> Result<(), String> {
        let Some(format) = FORMATS.iter().copied().find(|f| {
            image_len >= f.bytes && image_len - f.bytes < SD_SECTOR_LEN as u64
        }) else {
            return Err(format!("{image_len} bytes is not a Cromemco image"));
        };
        let d = self
            .drives
            .get_mut(drive as usize)
            .ok_or_else(|| format!("the Cromemco addresses drives 0-3, not {drive}"))?;
        *d = Drive { present: true, read_only, format: Some(format) };
        if drive == self.selected {
            self.refresh();
        }
        Ok(())
    }

    fn buffer_loaded(&mut self, _drive: u8, bytes: &[u8]) {
        self.chip.sector_loaded(bytes);
    }

    fn buffer(&self, _drive: u8) -> Option<&[u8]> {
        Some(self.chip.sector_out())
    }

    fn cold_start(&self, image: &[u8]) -> ColdStart {
        // One sector, at 0080h, entered at 0080h. See the module comment for how
        // that address was established — it is the one place the loader's own two
        // absolute reads land on bytes it can have meant.
        let Some(sector) = image.get(..SD_SECTOR_LEN) else {
            return ColdStart::NoProgram;
        };
        if !super::boot::looks_bootable(sector) {
            return ColdStart::NoProgram;
        }
        ColdStart::Program { offset: 0, len: SD_SECTOR_LEN, load: 0x0080, entry: 0x0080 }
    }

    fn cold_started(&mut self, drive: u8) {
        // The ROM read track 0 sector 1, single-density, with a real transfer
        // command — so that is the state the loader is entitled to find. The
        // typing matters for the same reason it did on the Tarbell: in the
        // power-on Type I state bit 2 is Track 00 rather than Lost Data.
        self.selected = drive.min(DRIVES as u8 - 1);
        self.aux = AUX_SIDE_0;
        self.chip.set_format(SD_SECTOR_LEN, SD_SECTORS);
        self.refresh();
        self.chip.assume_read_completed(0, 1);
    }

    fn stuck_polls(&self) -> u32 {
        self.idle_polls.max(self.chip.stuck_polls())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SD: u64 = 256_256;
    const DD: u64 = 625_920;
    const DSDD: u64 = 1_256_704;

    fn board(len: u64) -> Cromemco {
        let mut c = Cromemco::new();
        c.insert(0, len, false).unwrap();
        // Drive 0, single density, side 0 — what a loader writes first.
        c.port_out(PORT_CONTROL, 0x31);
        c.port_out(PORT_AUX, 0x7F);
        c
    }

    /// The three lengths are the three sample images, and the arithmetic that
    /// produces them is the whole geometry. Asserted against the file sizes
    /// rather than against itself, so a changed constant fails here.
    #[test]
    fn test_the_geometries_reproduce_the_sample_image_sizes() {
        assert_eq!(SD_TRACK_BYTES, 3_328);
        assert_eq!(DD_TRACK_BYTES, 8_192);
        assert_eq!(CYLINDERS as u64 * SD_TRACK_BYTES, SD, "CDISK01");
        assert_eq!(SD_TRACK_BYTES + 76 * DD_TRACK_BYTES, DD, "CDISK02");
        assert_eq!(SD_TRACK_BYTES + 153 * DD_TRACK_BYTES, DSDD, "CDISK03");
        for f in FORMATS {
            let last = f.tracks() - 1;
            assert_eq!(
                f.track_offset(last) + f.track_bytes(),
                f.bytes,
                "{}: the last track must end at the end of the file",
                f.label
            );
        }
    }

    /// Track 0 is single-density even on a double-density disk, and the *second*
    /// track is where the density changes. Both directories begin at 11,520,
    /// which is two tracks in only if this is right.
    #[test]
    fn test_track_zero_is_single_density_on_a_double_density_disk() {
        let f = FORMATS[1];
        assert_eq!(f.track_format(0), (SD_SECTOR_LEN, SD_SECTORS), "track 0 is always SD");
        assert_eq!(f.track_format(1), (DD_SECTOR_LEN, DD_SECTORS));
        assert_eq!(f.track_offset(1), SD_TRACK_BYTES);
        assert_eq!(f.track_offset(2), 11_520, "where CDISK02's directory really begins");
        assert_eq!(FORMATS[2].track_offset(2), 11_520, "and CDISK03's");
    }

    /// Sectors are numbered from 1, and a double-density track has 16 of them.
    #[test]
    fn test_sector_addressing_on_both_densities() {
        let c = board(DD);
        assert_eq!(c.offset(0, 1), Some((0, 128)), "track 0 sector 1 is the front of the file");
        assert_eq!(c.offset(0, 26), Some((25 * 128, 128)));
        assert_eq!(c.offset(0, 27), None, "and there is no 27th");
        assert_eq!(c.offset(1, 1), Some((3_328, 512)), "track 1 is double density");
        assert_eq!(c.offset(1, 16), Some((3_328 + 15 * 512, 512)));
        assert_eq!(c.offset(1, 17), None);
        assert_eq!(c.offset(0, 0), None, "there is no sector 0");
        assert_eq!(c.offset(77, 1), None, "and no cylinder 77");
    }

    /// The side bit moves the head to the other surface of the *same* cylinder,
    /// which is the adjacent track in the file — not the other half of the disk.
    /// Getting that wrong reads a plausible-looking wrong track.
    #[test]
    fn test_the_side_bit_selects_the_other_surface_of_the_same_cylinder() {
        let mut c = board(DSDD);
        assert_eq!(c.side(), 0);
        assert_eq!(c.offset(1, 1), Some((3_328 + 8_192, 512)), "cylinder 1 side 0 is track 2");

        c.port_out(PORT_AUX, 0x7F & !AUX_SIDE_0);
        assert_eq!(c.side(), 1);
        assert_eq!(c.offset(0, 1), Some((3_328, 512)), "cylinder 0 side 1 is track 1");
        assert_eq!(c.offset(1, 1), Some((3_328 + 2 * 8_192, 512)), "cylinder 1 side 1 is track 3");
    }

    /// A single-sided disk has one surface however the latch is set. The
    /// alternative is every track landing at double its real offset.
    #[test]
    fn test_a_single_sided_disk_ignores_the_side_bit() {
        let mut c = board(DD);
        c.port_out(PORT_AUX, 0x7F & !AUX_SIDE_0);
        assert_eq!(c.side(), 0);
        assert_eq!(c.offset(1, 1), Some((3_328, 512)));
    }

    /// The drive select is one-hot, and `31h` — the value every boot sector
    /// writes while reading the disk it booted from — must mean drive 0.
    #[test]
    fn test_the_control_register_selects_drive_zero_with_31h() {
        let mut c = Cromemco::new();
        c.insert(0, SD, false).unwrap();
        c.insert(1, SD, true).unwrap();
        c.port_out(PORT_CONTROL, 0x31);
        assert_eq!(c.selected, 0, "read as a binary number this would be drive 1");
        c.port_out(PORT_CONTROL, 0x32);
        assert_eq!(c.selected, 1);
        // Deselecting everything leaves the selection where it was, rather than
        // silently acting on drive 0.
        c.port_out(PORT_CONTROL, 0x30);
        assert_eq!(c.selected, 1);
    }

    /// The whole of a double-density sector reaches the guest, and the status
    /// port reports data until it is empty and then completion.
    #[test]
    fn test_a_double_density_sector_moves_512_bytes() {
        let mut c = board(DD);
        // Seek to track 1, which is where double density starts.
        c.port_out(0x33, 1);
        c.port_out(0x30, 0x1B);
        c.port_out(0x32, 3);
        let req = c.port_out(0x30, 0x8C);
        assert_eq!(
            req,
            HostRequest::Read { drive: 0, offset: 3_328 + 2 * 512, len: 512 },
            "512 bytes from the third sector of track 1"
        );

        let sector: Vec<u8> = (0..512).map(|i| (i % 253) as u8).collect();
        c.buffer_loaded(0, &sector);
        let mut got = Vec::new();
        for i in 0..512 {
            let st = c.port_in(PORT_CONTROL).0;
            assert_ne!(st & ST_DRQ, 0, "byte {i}: a byte should be waiting");
            assert_eq!(st & ST_INTRQ, 0, "byte {i}: and the command is not over");
            got.push(c.port_in(0x33).0);
        }
        assert_eq!(got, sector);
        assert_ne!(c.port_in(PORT_CONTROL).0 & ST_INTRQ, 0, "and then it is");
    }

    /// A read of a whole track with one command, which is what every loader does.
    /// It must fetch each sector in turn and end with Record Not Found — the
    /// condition the loaders test for and restart the boot without.
    #[test]
    fn test_one_command_reads_a_whole_track_and_ends_with_record_not_found() {
        let mut c = board(SD);
        c.port_out(0x32, 2); // start at sector 2, as CDISK01 does
        let mut req = c.port_out(0x30, 0x9C);
        let mut offsets = Vec::new();
        for _ in 0..SD_SECTORS + 2 {
            match req {
                // The first sector is asked for by the command write, which owes
                // the guest nothing; every one after it by the read that empties
                // its predecessor, which has already answered. Both shapes are
                // expected here, and that they differ is the point.
                HostRequest::Read { offset, len, .. }
                | HostRequest::ReadAhead { offset, len, .. } => {
                    offsets.push(offset);
                    c.buffer_loaded(0, &vec![0x5A; len]);
                    req = HostRequest::None;
                    for _ in 0..len {
                        req = c.port_in(0x33).1;
                    }
                }
                HostRequest::None => break,
                other => panic!("unexpected {other:?}"),
            }
        }
        let want: Vec<u64> = (2..=26).map(|s| (s - 1) * 128).collect();
        assert_eq!(offsets, want, "sectors 2 through 26, and no more");
        assert_ne!(c.port_in(PORT_CONTROL).0 & ST_INTRQ, 0, "the command has ended");
        assert_ne!(
            c.port_in(0x30).0 & 0x10,
            0,
            "and with Record Not Found, or the loader restarts the boot"
        );
    }

    /// The cold start puts one sector at 0080h and enters there — not at 0000h,
    /// which is where every other board here loads, and not at the front of a
    /// sector loaded somewhere else.
    #[test]
    fn test_the_cold_start_loads_the_boot_sector_at_0080() {
        let c = Cromemco::new();
        let mut image = vec![0u8; SD as usize];
        // The real first four bytes of all three sample disks.
        image[..4].copy_from_slice(&[0x3E, 0x01, 0xD3, 0x40]);
        image[4..8].copy_from_slice(&[0x11, 0x7F, 0x31, 0x21]);
        assert_eq!(
            c.cold_start(&image),
            ColdStart::Program { offset: 0, len: 128, load: 0x0080, entry: 0x0080 },
        );
    }

    /// A blank disk is refused as the data it is.
    #[test]
    fn test_a_blank_disk_is_not_bootable() {
        let c = Cromemco::new();
        assert_eq!(c.cold_start(&vec![0xE5u8; SD as usize]), ColdStart::NoProgram);
        assert_eq!(c.cold_start(&[]), ColdStart::NoProgram);
    }

    /// Only these media, and only these ports.
    #[test]
    fn test_what_it_claims() {
        let c = Cromemco::new();
        for len in [SD, DD, DSDD] {
            assert!(c.accepts(len).is_some(), "{len}");
            assert!(c.accepts(len + 96).is_some(), "a short trailer is still one: {len}");
            assert!(c.accepts(len + 128).is_none(), "a whole extra sector is not: {len}");
        }
        assert!(c.accepts(337_568).is_none(), "an Altair floppy is not ours");
        assert!(c.accepts(4_988_928).is_none(), "nor a hard disk");
        for p in [0x04u8, 0x30, 0x31, 0x32, 0x33, 0x34] {
            assert!(c.owns_port(p), "{p:#04x}");
        }
        for p in [0x00u8, 0x01, 0x05, 0x2F, 0x35, 0x40, 0xF8] {
            assert!(!c.owns_port(p), "{p:#04x} is not this board's");
        }
    }

    /// The console lives at `00h`/`01h` and the board must not answer there, or
    /// the machine goes silent. Stated here as well as in the machine list
    /// because this board is the only one whose ports come anywhere near it.
    #[test]
    fn test_the_board_leaves_the_console_ports_alone() {
        let c = Cromemco::new();
        assert!(!c.owns_port(0x00), "TU-ART status");
        assert!(!c.owns_port(0x01), "TU-ART data");
    }

    /// The auxiliary port is a status input, not a read-back of the latch. A
    /// driver writes bit 6 set and then waits for it to clear; a read-back would
    /// hang there for ever.
    #[test]
    fn test_the_auxiliary_port_does_not_read_back_what_was_written() {
        let mut c = board(SD);
        c.port_out(PORT_AUX, 0xFF);
        assert_eq!(c.port_in(PORT_AUX).0 & AUX_NOT_READY, 0);
    }

    /// A driver polling a chip with nothing to say must be visible, because on
    /// the real board that is a hang.
    #[test]
    fn test_waiting_on_an_idle_chip_is_counted() {
        let mut c = board(SD);
        for _ in 0..40 {
            c.port_in(PORT_CONTROL);
        }
        assert!(c.stuck_polls() >= 40, "a driver stuck here must not look like a crash");
    }

    /// A write to a protected disk is refused, and a writable one commits exactly
    /// the sector's worth of bytes — 512 on a double-density track, so the buffer
    /// handed back must be that long and not the chip's whole array.
    #[test]
    fn test_a_double_density_write_commits_512_bytes() {
        let mut c = board(DD);
        c.port_out(0x33, 1);
        c.port_out(0x30, 0x1B); // seek track 1
        c.port_out(0x32, 2);
        assert_eq!(c.port_out(0x30, 0xAC), HostRequest::None, "nothing until it has the bytes");
        let mut req = HostRequest::None;
        for i in 0..512 {
            req = c.port_out(0x33, (i % 251) as u8);
        }
        assert_eq!(req, HostRequest::Write { drive: 0, offset: 3_328 + 512, len: 512 });
        assert_eq!(c.buffer(0).unwrap().len(), 512);
    }
}
