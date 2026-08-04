//! The Tarbell 1011 single-density floppy disk interface.
//!
//! Written from *The Tarbell Floppy Disk Interface Manual* (bitsavers, with a
//! text layer), which unlike the Altair floppy's manual contains a full Theory of
//! Operation, a jumper-by-jumper description of the board, a walkthrough of its
//! bootstrap PROM, and the FD1771 data sheet reproduced as §7-2. Cross-checked
//! against the driver source carried on the `TDISK*` images themselves. Same
//! clean-room posture as everything else here.
//!
//! # What this board is
//!
//! Very little on top of the chip, which is the point: five ports, a latch, and
//! one genuinely unusual trick.
//!
//! ```text
//!     F8h   command (out) / status (in)   \
//!     F9h   track                          |  the FD1771's four registers,
//!     FAh   sector                         |  selected by the low two address
//!     FBh   data                          /   bits — see cpm::wd1771
//!     FCh   extended command (out) / WAIT (in)
//! ```
//!
//! The base address is DIP-switch selectable on A3–A7 and the manual's examples
//! all use `F8h`; one of the drivers on the sample disks says `DISK EQU 0E8H
//! ;DIFFERENT DISK PORTS`, which is the same board strapped elsewhere. `F8h` is
//! what the bootstrap PROM is programmed for — "the upper five bits of I/O
//! instructions are always high, to match the standard setting of the dip switch"
//! — so it is what a booted disk expects.
//!
//! # The trick: `IN FCh` stalls the processor
//!
//! Reading the WAIT port does not return a value the CPU then tests. It **forces
//! a hardware wait** — the board holds the bus until the 1771 raises DRQ or
//! INTRQ, and only then completes the instruction. A driver's inner loop is
//! therefore `IN WAIT` with no polling at all, and the manual explains how it
//! knows which event woke it:
//!
//! > "If the most significant bit is 0, the interface is indicating that it was
//! > the INTRQ that caused the end of the wait. If 1, it was the DRQ, indicating
//! > some data is ready to process."
//!
//! **We cannot stall an instruction, and do not need to.** Nothing here takes
//! time: a sector is fetched from a `Vec` the moment it is asked for, so by the
//! time the guest reads the WAIT port the thing it would have waited for has
//! already happened. So the read answers immediately with whichever of the two is
//! true. This is the same reasoning the 88-HDSK's status flags use, and it fails
//! in the same direction if it is ever wrong: a guest is told to proceed sooner
//! than a real board would, never later, so nothing waits forever.
//!
//! The one case that needs care is neither flag being true — a driver reading the
//! WAIT port when the chip is idle and has nothing to say. On the real board that
//! is a hang, cured only by a reset; here it would be a lie whichever answer we
//! gave. It is reported as INTRQ (bit 7 clear, "the operation ended") and counted,
//! so a guest doing it repeatedly shows up in `stuck_polls` rather than looking
//! like a crashed CPU.

use super::controller::{ColdStart, Controller, HostRequest, Medium};
use super::wd1771::{Need, Wd1771, SECTORS_PER_TRACK, SECTOR_LEN};

/// Tracks on the 8" single-density disks this board carried.
pub const TRACKS: u8 = 77;

/// Bytes in a full image: 77 × 26 × 128, and the `TDISK*` files are exactly this.
pub const IMAGE_LEN: u64 = TRACKS as u64 * SECTORS_PER_TRACK as u64 * SECTOR_LEN as u64;

/// Drives the board can address.
const DRIVES: usize = 4;

/// The five ports, at the standard DIP setting.
const PORT_BASE: u8 = 0xF8;
const PORT_TOP: u8 = 0xFC;
/// Extended command out, WAIT in.
const PORT_WAIT: u8 = 0xFC;

/// The low three data bits of an extended command select what it does.
///
/// "This actually decodes the bottom three combinations of D0, D1 and D2, since
/// these lines are active low" — of the eight, three are wired: `000` can pulse
/// the drive reset line, `001` drives the S0 line, and `010` is the one that
/// matters here, which "strobes data bits 4,5,6,7 into latch U40".
const FN_LATCH: u8 = 0b010;

/// Where the drive select sits in that latch, and which way round it is.
///
/// "E31 — This is connected to the least significant bit (Bit 4) of the latch,
/// and may be used to select drive 0 or 1 under software control."
///
/// **The bit is active low**, which cost a bring-up to establish and is worth
/// stating rather than leaving in the code. Two things say so. The manual
/// describes this latch's sibling pads as inverted — "E33 — This line is
/// connected to the latch bit 3 **inverted**", "E42 — Connected to latch bit 1
/// **inverted**" — so what the board brings out are the latch's `Q*` outputs, and
/// a written 1 pulls its line low. And E29, which E31 drives, selects the
/// zero-suffixed drive lines "when E29 is **low**".
///
/// The confirmation is a real disk. TDISK02's CP/M writes `F2` to this port —
/// every latch bit high — at the moment it is unmistakably working with drive 0:
/// it has just booted from it and is loading its system off it. Read the bit as
/// active high and the board switches to drive 1, which is empty, and CP/M signs
/// on and then says `Bdos Err On A: Bad Sector` for ever.
///
/// Two drives is all the documented software select reaches; a four-drive machine
/// wires the spare latch outputs to the radial head-load lines, and which one a
/// given installation used is a jumper rather than a fact about the board. So the
/// rest of the latch is remembered without being interpreted.
const LATCH_DRIVE: u8 = 0x10;

/// One drive.
#[derive(Debug, Clone, Copy)]
struct Drive {
    present: bool,
    read_only: bool,
}

/// The board.
pub struct Tarbell {
    chip: Wd1771,
    drives: [Drive; DRIVES],
    selected: u8,
    /// The extended-command latch, as last strobed.
    latch: u8,
    /// Which drive a pending host request is for, since the chip does not know.
    pending: u8,
    /// WAIT-port reads that found nothing to report.
    idle_waits: u32,
}

impl Default for Tarbell {
    fn default() -> Tarbell {
        Tarbell::new()
    }
}

impl Tarbell {
    pub fn new() -> Tarbell {
        Tarbell {
            chip: Wd1771::new(),
            drives: [Drive { present: false, read_only: true }; DRIVES],
            selected: 0,
            latch: 0,
            pending: 0,
            idle_waits: 0,
        }
    }

    /// Byte offset of a sector, in the order these images store them.
    ///
    /// Track then sector, sectors numbered **from 1** as IBM 3740 formats them —
    /// which is why the boot PROM sets the sector register to 1 rather than 0.
    /// No skew and no framing: 77 × 26 × 128 is exactly the file length, so every
    /// byte in the file is sector data and the arithmetic is the whole mapping.
    /// Whatever interleave the disk uses lives in its own BIOS, which is the
    /// reason booting reaches disks whose layout we never had to work out.
    fn offset(&self, track: u8, sector: u8) -> Option<u64> {
        if track >= TRACKS || sector == 0 || sector as usize > SECTORS_PER_TRACK {
            return None;
        }
        let index = track as u64 * SECTORS_PER_TRACK as u64 + u64::from(sector - 1);
        Some(index * SECTOR_LEN as u64)
    }

    /// Tell the chip what the selected drive is like, before it acts on it.
    fn refresh_drive(&mut self) {
        let d = self.drives[self.selected as usize % DRIVES];
        self.chip.set_drive(d.present, d.read_only, TRACKS);
    }

    /// Turn what the chip wants into what the machine can do.
    fn serve(&mut self, need: Need) -> HostRequest {
        match need {
            Need::None => HostRequest::None,
            Need::Read { track, sector } => match self.offset(track, sector) {
                Some(offset) => {
                    self.pending = self.selected;
                    HostRequest::Read { drive: self.selected, offset, len: SECTOR_LEN }
                }
                None => HostRequest::None,
            },
            Need::Write { track, sector } => match self.offset(track, sector) {
                Some(offset) => {
                    self.pending = self.selected;
                    HostRequest::Write { drive: self.selected, offset, len: SECTOR_LEN }
                }
                None => HostRequest::None,
            },
        }
    }

    /// The WAIT port: which of the two events the processor would have waited for.
    fn wait_port(&mut self) -> u8 {
        if self.chip.drq() {
            self.idle_waits = 0;
            // Bit 7 set: data is ready. The PROM does `ORA A` and tests the sign.
            0x80
        } else if self.chip.intrq() {
            self.idle_waits = 0;
            0x00
        } else {
            // Neither. See the module comment: on the real board this hangs, and
            // there is no honest answer, so it is counted and reported as "the
            // operation ended" — the answer that lets a driver look at the status
            // register and find out for itself.
            self.idle_waits = self.idle_waits.saturating_add(1);
            0x00
        }
    }
}

impl Controller for Tarbell {
    fn name(&self) -> &'static str {
        "Tarbell 1011 floppy"
    }

    fn owns_port(&self, port: u8) -> bool {
        (PORT_BASE..=PORT_TOP).contains(&port)
    }

    fn port_in(&mut self, port: u8) -> (u8, HostRequest) {
        if port == PORT_WAIT {
            return (self.wait_port(), HostRequest::None);
        }
        (self.chip.read(port & 0x03), HostRequest::None)
    }

    fn port_out(&mut self, port: u8, value: u8) -> HostRequest {
        if port == PORT_WAIT {
            if std::env::var_os("CPM_TARBELL_TRACE").is_some() {
                eprintln!("tarbell ext {value:02x} (fn {:03b})", value & 0b111);
            }
            if value & 0b111 == FN_LATCH {
                self.latch = value & 0xF0;
                // Active low: see `LATCH_DRIVE`.
                self.selected = u8::from(value & LATCH_DRIVE == 0);
                self.refresh_drive();
            }
            // The other two decoded functions pulse the drive's reset line and
            // the S0 line. Neither has anything to act on here, and both are
            // jumper-dependent even on the real board.
            return HostRequest::None;
        }
        // A command has to be judged against the drive that is actually selected.
        if port & 0x03 == 0 {
            self.refresh_drive();
            if std::env::var_os("CPM_TARBELL_TRACE").is_some() {
                eprintln!(
                    "tarbell cmd {:02x} drive {} track {} sector {}",
                    value,
                    self.selected,
                    self.chip.track(),
                    self.chip.sector(),
                );
            }
        }
        let need = self.chip.write(port & 0x03, value);
        self.serve(need)
    }

    fn media(&self) -> Vec<Medium> {
        vec![Medium {
            bytes: IMAGE_LEN,
            label: "Tarbell 1011 8\" floppy",
            trailer: SECTOR_LEN as u64 - 1,
            shape: format!("{TRACKS} tracks x {SECTORS_PER_TRACK} sectors x {SECTOR_LEN}"),
        }]
    }

    fn insert(&mut self, drive: u8, image_len: u64, read_only: bool) -> Result<(), String> {
        if self.accepts(image_len).is_none() {
            return Err(format!("{image_len} bytes is not a Tarbell image"));
        }
        let d = self
            .drives
            .get_mut(drive as usize)
            .ok_or_else(|| format!("the Tarbell addresses drives 0-3, not {drive}"))?;
        *d = Drive { present: true, read_only };
        if drive == self.selected {
            self.refresh_drive();
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
        // The 32-byte PROM at U23, from the manual's own walkthrough: wait for
        // INTRQ (the reset-driven restore has the head at track 0), set the sector
        // register to 1, issue `8Ch` — "bits 7,6,5 are 100 ... bit 4 is 0 because
        // we want a single record, bit 3 is 1 for IBM format, bit 2 is 1 to make
        // the head load at the beginning" — read 128 bytes to 0000h, then:
        //
        // ```text
        //     0019  DB F8     RDONE: IN  DSTAT   ; read disk status
        //     001B  B7               ORA A       ; set flags
        //     001C  CA 7D 00         JZ  07DH    ; if zero, go to SBOOT
        //     001F  76        WAIT:  HLT
        //     ```
        //
        // So it enters at **007Dh**, three bytes from the end of the sector it
        // just loaded, and only if the status came back clean. Four of the six
        // sample images carry `C3 00 00` there — a jump back to the front of the
        // sector, which is where their loader really starts. That is why
        // `ColdStart` has a separate entry point.
        let Some(sector) = image.get(..SECTOR_LEN) else {
            return ColdStart::NoProgram;
        };
        if !super::boot::looks_bootable(sector) {
            return ColdStart::NoProgram;
        }
        ColdStart::Program { offset: 0, len: SECTOR_LEN, load: 0x0000, entry: 0x007D }
    }

    fn cold_started(&mut self, drive: u8) {
        // The PROM read track 0 sector 1 with a real `8Ch`, so that is the state
        // the loader is entitled to find — including the status register being
        // typed as a transfer rather than as a seek. See
        // `Wd1771::assume_read_completed`.
        self.selected = drive.min(DRIVES as u8 - 1);
        self.refresh_drive();
        self.chip.assume_read_completed(0, 1);
    }

    fn stuck_polls(&self) -> u32 {
        self.idle_waits.max(self.chip.stuck_polls())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn board() -> Tarbell {
        let mut t = Tarbell::new();
        t.insert(0, IMAGE_LEN, false).unwrap();
        // Select drive 0 through the latch, as a driver would — and the select bit
        // is active low, so drive 0 is the bit *set*. This is the value TDISK02's
        // CP/M really writes.
        t.port_out(PORT_WAIT, FN_LATCH | LATCH_DRIVE);
        t
    }

    /// The geometry, checked against the real files rather than against itself.
    #[test]
    fn test_the_geometry_matches_the_sample_images() {
        assert_eq!(IMAGE_LEN, 256_256);
        assert_eq!(TRACKS as usize * SECTORS_PER_TRACK, 2002, "sectors on a disk");
    }

    /// Sectors are numbered from 1, which is why the boot PROM writes 1 and not 0.
    /// Getting this wrong shifts every read by one sector and still "works" until
    /// a file's contents come back scrambled.
    #[test]
    fn test_sectors_are_numbered_from_one() {
        let t = board();
        assert_eq!(t.offset(0, 1), Some(0), "track 0 sector 1 is the front of the file");
        assert_eq!(t.offset(0, 2), Some(128));
        assert_eq!(t.offset(0, 26), Some(25 * 128));
        assert_eq!(t.offset(1, 1), Some(26 * 128), "and track 1 follows track 0");
        assert_eq!(t.offset(76, 26), Some(IMAGE_LEN - 128), "the last sector is the last one");
        assert_eq!(t.offset(0, 0), None, "there is no sector 0");
        assert_eq!(t.offset(0, 27), None);
        assert_eq!(t.offset(77, 1), None, "and no track 77");
    }

    /// A read: seek, set the sector, issue `8Ch`, and the board asks the machine
    /// for exactly the bytes that sector occupies.
    #[test]
    fn test_a_read_asks_for_the_right_bytes() {
        let mut t = board();
        // Seek track 3 — the 1771 takes the destination from its data register.
        t.port_out(0xFB, 3);
        t.port_out(0xF8, 0x1B);
        t.port_out(0xFA, 5); // sector 5
        let req = t.port_out(0xF8, 0x8C);
        let want = (3 * SECTORS_PER_TRACK as u64 + 4) * SECTOR_LEN as u64;
        assert_eq!(req, HostRequest::Read { drive: 0, offset: want, len: SECTOR_LEN });
    }

    /// The whole sector reaches the guest through the data port, and the WAIT port
    /// says "data" until the last byte and then "finished".
    #[test]
    fn test_the_wait_port_reports_data_then_completion() {
        let mut t = board();
        t.port_out(0xFA, 1);
        assert!(matches!(t.port_out(0xF8, 0x8C), HostRequest::Read { .. }));

        let sector: Vec<u8> = (0..SECTOR_LEN).map(|i| (i ^ 0x5A) as u8).collect();
        t.buffer_loaded(0, &sector);

        let mut got = Vec::new();
        for i in 0..SECTOR_LEN {
            assert_eq!(
                t.port_in(PORT_WAIT).0 & 0x80,
                0x80,
                "byte {i}: the wait must end on DRQ, with bit 7 set"
            );
            got.push(t.port_in(0xFB).0);
        }
        assert_eq!(got, sector);
        assert_eq!(
            t.port_in(PORT_WAIT).0 & 0x80,
            0,
            "and then on INTRQ, with bit 7 clear"
        );
    }

    /// A write collects the sector and hands it back once, at the end.
    #[test]
    fn test_a_write_commits_the_sector_once() {
        let mut t = board();
        t.port_out(0xFB, 2);
        t.port_out(0xF8, 0x1B); // seek track 2
        t.port_out(0xFA, 9);
        assert_eq!(t.port_out(0xF8, 0xAC), HostRequest::None, "nothing yet");

        let mut req = HostRequest::None;
        for i in 0..SECTOR_LEN {
            assert_eq!(t.port_in(PORT_WAIT).0 & 0x80, 0x80, "it wants byte {i}");
            req = t.port_out(0xFB, i as u8);
        }
        let want = (2 * SECTORS_PER_TRACK as u64 + 8) * SECTOR_LEN as u64;
        assert_eq!(req, HostRequest::Write { drive: 0, offset: want, len: SECTOR_LEN });
        assert_eq!(t.buffer(0).unwrap()[7], 7);
    }

    /// The latch selects the drive, and a command is judged against *that* drive —
    /// so a write to a protected disk in drive 1 is refused even though drive 0 is
    /// writable.
    #[test]
    fn test_the_latch_selects_the_drive_a_command_acts_on() {
        let mut t = Tarbell::new();
        t.insert(0, IMAGE_LEN, false).unwrap();
        t.insert(1, IMAGE_LEN, true).unwrap();

        // The select bit is active low, so a *set* bit means drive 0.
        t.port_out(PORT_WAIT, FN_LATCH | LATCH_DRIVE); // drive 0
        t.port_out(0xFA, 1);
        assert!(matches!(t.port_out(0xF8, 0xAC), HostRequest::None));
        assert_eq!(t.port_in(0xF8).0 & 0x40, 0, "drive 0 is writable");

        t.port_out(PORT_WAIT, FN_LATCH); // drive 1
        t.port_out(0xFA, 1);
        t.port_out(0xF8, 0xAC);
        assert_ne!(t.port_in(0xF8).0 & 0x40, 0, "drive 1 is protected and must say so");
    }

    /// An extended command that is not the latch strobe changes no selection.
    #[test]
    fn test_only_the_latch_function_moves_the_selection() {
        let mut t = board();
        t.port_out(PORT_WAIT, FN_LATCH); // drive 1, the bit clear
        assert_eq!(t.selected, 1);
        // Function 000 pulses the drive reset line; it must not reselect.
        t.port_out(PORT_WAIT, 0b000);
        assert_eq!(t.selected, 1, "the selection is unchanged");
    }

    /// The cold start loads one sector at zero and enters at 7Dh, which is what
    /// the PROM does and is not the same address.
    #[test]
    fn test_the_cold_start_enters_at_7d() {
        let t = Tarbell::new();
        // A sector shaped like the real ones: code at the front, a jump at 7Dh.
        let mut image = vec![0u8; IMAGE_LEN as usize];
        image[..3].copy_from_slice(&[0x31, 0x00, 0x01]); // LXI SP,0100h
        image[0x7D..0x80].copy_from_slice(&[0xC3, 0x00, 0x00]); // JMP 0000h
        assert_eq!(
            t.cold_start(&image),
            ColdStart::Program { offset: 0, len: SECTOR_LEN, load: 0x0000, entry: 0x007D },
        );
    }

    /// A blank disk is refused as the data it is — `TDISK06` in the sample set is
    /// exactly this, 256,256 bytes of `E5`.
    #[test]
    fn test_a_blank_disk_is_not_bootable() {
        let t = Tarbell::new();
        assert_eq!(t.cold_start(&vec![0xE5u8; IMAGE_LEN as usize]), ColdStart::NoProgram);
        assert_eq!(t.cold_start(&[]), ColdStart::NoProgram);
    }

    /// Only this medium, and only these ports.
    #[test]
    fn test_what_it_claims() {
        let t = Tarbell::new();
        assert!(t.accepts(IMAGE_LEN).is_some());
        assert!(t.accepts(IMAGE_LEN + 96).is_some(), "a short trailer is still a Tarbell disk");
        assert!(t.accepts(IMAGE_LEN + 128).is_none(), "a whole extra sector is not");
        assert!(t.accepts(337_568).is_none(), "an Altair floppy is not ours");
        assert!(t.accepts(4_988_928).is_none(), "nor a hard disk");
        for p in [0xF8u8, 0xF9, 0xFA, 0xFB, 0xFC] {
            assert!(t.owns_port(p), "{p:#04x}");
        }
        assert!(!t.owns_port(0xF7));
        assert!(!t.owns_port(0xFD));
        assert!(!t.owns_port(0x08), "the Altair floppy's ports stay its own");
    }

    /// A driver waiting on a chip with nothing to say must be visible, because on
    /// the real board that is a hang.
    #[test]
    fn test_waiting_on_an_idle_chip_is_counted() {
        let mut t = board();
        for _ in 0..40 {
            t.port_in(PORT_WAIT);
        }
        assert!(t.stuck_polls() >= 40, "a driver stuck here must not look like a crash");
    }
}
