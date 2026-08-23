//! Working out which machine a disk wants, from the disk.
//!
//! # Why this is allowed, when "do not autodetect" is written all over this path
//!
//! Because it is not the same thing. The sector step was once autodetected by
//! running four candidates and keeping whichever printed something — *scoring* a
//! hypothesis — and it went badly in the way scoring always does: a wrong layout
//! that scribbles a few bytes at a console beats a right one still loading, so
//! the wrong answer won for five disks.
//!
//! This reads a **declaration**. A boot loader has to talk to its own disk
//! controller's registers, and a BIOS has to read its own console's status
//! register; those `IN` and `OUT` operands are in the image, put there by whoever
//! built the disk. Reading them is the same class of evidence as the 88-HDSK
//! volume label that says where the boot program is, or the disk's own DPB and
//! translate table — all of which this project already trusts. What a driver does
//! to a register is that register's definition as far as that driver is concerned.
//!
//! Three rules keep it honest:
//!
//! * **Only distinctive ports count.** `0Ah` belongs to both the 88-DCDD's data
//!   register and z80pack's drive select, so it is evidence of nothing. `08h`/`09h`
//!   are the floppy's alone and `0Bh`–`0Eh` are z80pack's alone.
//! * **Both halves must agree.** A console at `00h`/`01h` fits an Altair 88-SIO
//!   *and* z80pack; only the controller separates them. A machine is chosen when
//!   its controller and its console are both attested.
//! * **Not decisive means not chosen.** No scoring, no "best match". If the
//!   evidence does not name one machine, the operator's setting stands and the
//!   boot says what happened.
//!
//! And it is *proved*, not argued: `test_detect_every_real_image` requires that
//! every disk which reaches a sign-on under an explicit machine reaches the same
//! one under detection.
//!
//! # What it cannot do
//!
//! Tell you a disk is broken. A data disk has no loader and a disk for hardware
//! we do not emulate names ports nothing here claims; both come back
//! [`Detected::Unclear`], which is honest. And it says nothing about whether the
//! disk *works* — TDISK04 is detected correctly and still writes nothing to any
//! console port, because its screen is a Processor Technology VDM-1: a card
//! with no data port, whose picture lives in the guest's own memory and is
//! shown in the browser rather than in the session. [`image_drives_vdm`] says
//! that in advance; it is a separate question from which machine, and does not
//! change the answer to this one.

use super::console::{Board, MachineChoice, MACHINE_CHOICES};

/// How far to look for the **boot loader's** controller registers.
///
/// One sector, and this bound is load-bearing rather than an optimisation. It was
/// first written as two whole tracks, on the reasoning that more evidence cannot
/// hurt — and the measurement refuted that immediately: over two tracks almost
/// every disk appeared to "drive more than one controller", because the *BIOS*
/// is in there driving its own board and because file bytes that happen to read
/// `D3 nn` are indistinguishable from an `OUT` in a scan that does not
/// disassemble. The boot sector is where a loader is, it is small, and it is
/// almost entirely code.
///
/// 137 rather than 128 so a framed Altair sector's three header bytes and its
/// checksum tail are inside the window; the extra bytes are harmless.
const BOOT_SCAN_BYTES: usize = 137;

/// How far to look for the **BIOS's** console registers.
///
/// Two 8-inch tracks: the system tracks, where a booted disk's BIOS lives. The
/// console question tolerates this much noise where the controller question does
/// not, because a console is only accepted when *both* of its registers are read,
/// and stray bytes do not conspire to produce a specific pair.
const SYSTEM_SCAN_BYTES: usize = 2 * 32 * 137;

/// Ports that belong to exactly one board, so touching one is proof.
///
/// Deliberately *not* each board's whole range. The 88-DCDD answers `08h`–`0Ah`
/// and z80pack `0Ah`–`11h`, and the byte they share is the one a naive check
/// would trip over — as would `10h`/`11h`, which are z80pack registers and also
/// the Altair's console, so a plain Altair CP/M disk names them innocently.
fn distinctive_ports(board: Board) -> &'static [u8] {
    match board {
        Board::Dcdd => &[0x08, 0x09],
        Board::Hdsk => &[0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7],
        Board::Tarbell => &[0xF8, 0xF9, 0xFA, 0xFB, 0xFC],
        Board::Z80pack => &[0x0B, 0x0C, 0x0D, 0x0E],
        // The 16FDC's four chip registers and its control port. Its auxiliary
        // latch at `04h` is deliberately left out: `04h`/`05h` is also the
        // console of the VDM-1 Tarbell machines, so it fails rule one.
        Board::Cromemco => &[0x30, 0x31, 0x32, 0x33, 0x34],
    }
}

/// What the image said about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Detected {
    /// One machine is attested by both its controller and its console.
    Machine(&'static str),
    /// The evidence does not name one machine. The operator's setting stands.
    Unclear(&'static str),
}

/// Every port an `IN` reads and every port an `OUT` writes, in the first stretch
/// of an image.
///
/// A *superset*: a data byte pair can look like `D3 nn`, and this does not
/// disassemble. That is why only distinctive ports are trusted and why both
/// halves have to agree — noise adds ports, so a check that requires several
/// specific ones survives it, while a check that picks the "best" match would not.
fn ports_touched(
    image: &[u8],
    limit: usize,
) -> (std::collections::HashSet<u8>, std::collections::HashSet<u8>) {
    let mut ins = std::collections::HashSet::new();
    let mut outs = std::collections::HashSet::new();
    let end = image.len().min(limit);
    let mut i = 0;
    while i + 1 < end {
        match image[i] {
            0xDB => {
                ins.insert(image[i + 1]);
                i += 2;
            }
            0xD3 => {
                outs.insert(image[i + 1]);
                i += 2;
            }
            _ => i += 1,
        }
    }
    (ins, outs)
}

/// The machine to actually use for an image, given a configured value.
///
/// `auto` asks the disk; anything else is taken as said. When the disk does not
/// say plainly, the default stands — **unless the default's boards cannot even
/// carry an image this size**, in which case a machine whose boards can is
/// preferred. That last step is not a guess: whether a board accepts a length is
/// a hard constraint, and the alternative is refusing to boot a disk while a
/// machine that could read it sits in the list.
pub fn machine_for(configured: &str, image: &[u8]) -> (String, Option<String>) {
    if configured != super::console::AUTO_MACHINE {
        return (configured.to_string(), None);
    }
    match detect_machine(image) {
        Detected::Machine(m) => (m.to_string(), Some(format!("Detected machine: {m}"))),
        Detected::Unclear(why) => {
            let default = super::console::DEFAULT_MACHINE;
            if super::boot_machine::BootMachine::machine_accepts(default, image.len() as u64) {
                return (default.to_string(), Some(format!("Machine not stated ({why})")));
            }
            match MACHINE_CHOICES.iter().find(|m| {
                super::boot_machine::BootMachine::machine_accepts(m.key, image.len() as u64)
            }) {
                Some(m) => (
                    m.key.to_string(),
                    Some(format!("Machine not stated ({why}); only {} takes this size", m.key)),
                ),
                None => (default.to_string(), Some(format!("Machine not stated ({why})"))),
            }
        }
    }
}

/// Does this disk's own system software drive a VDM-1?
///
/// The same class of evidence as everything else here, and unusually strong for
/// it: the Processor Technology VDM-1 has exactly one register — the scroll
/// port at `C8h` — and a driver has to write it. Nothing else this gateway
/// emulates answers there.
///
/// **Measured across all 75 images in the four collections.** `OUT C8h` in the
/// system tracks fires on exactly two, TDISK04 and `cpm14-vdm`, and both really
/// are VDM-1 disks: **zero false positives**. The conjunction first proposed —
/// the port *and* an address in the `CC00`–`CFFF` window — turned out to be
/// unnecessary, because the address half is worthless on its own: 60 of the 75
/// address that page for reasons that have nothing to do with a video card. The
/// port alone is the declaration.
///
/// What it misses is the reason the screen itself does not depend on it:
/// **DISK11**'s VDM driver lives in the CUTER monitor ROM at `C000h` and is
/// therefore not on the disk at all. No scan of an image can find code that is
/// not in it. So this is used for the *advance warning* — telling the operator
/// before the guest goes quiet where its screen has gone — while the screen is
/// offered to every booted session regardless, and reports whether the guest
/// has really driven the card once it is running.
pub fn image_drives_vdm(image: &[u8]) -> bool {
    let (_, outs) = ports_touched(image, SYSTEM_SCAN_BYTES);
    outs.contains(&super::vdm::SCROLL_PORT)
}

/// Does this disk require a monitor ROM it does not carry?
///
/// The signal is the disk **testing for one**, which is a declaration rather
/// than an inference: `LDA C000` — `3A 00 C0` — in the system tracks.  A disk
/// that reads the first byte of the monitor window is checking whether the
/// monitor is there, and the two that do it here both go on to print that they
/// need CUTER and stop.  See [`super::rom::ROM_CHOICES`] for the disassembly.
///
/// **Measured across all 74 images reachable in the two Altair collections**:
/// this fires on `DISK11.DSK` and on the Altair-Duino collection's
/// `DISK16.DSK`, which is Hansel's `DISK11` byte for byte — one disk under two
/// numbers, the same pattern already documented for `DISK17`/`DISK12`.  Zero
/// other hits.
///
/// **A call into the window would not do.**  Scanning for `CALL C0xx`–`C7xx`
/// instead fires on **45** of the 74 — every disk with a BIOS jump table up
/// there — and it cannot tell the case that needs a *file* from the case the
/// six-byte synthesised entry already serves: `TDISK05` calls `C019` and works
/// today with no ROM at all.  The signature test separates them exactly,
/// because only a disk that will refuse to run bothers to look.
///
/// Used to *describe* a disk, never to gate one: nothing is loaded or refused on
/// the strength of it, for the same reason [`image_drives_vdm`] only warns —
/// what a disk needs is the disk's business, and a scan of an image cannot find
/// what is not in it.
pub fn image_needs_monitor_rom(image: &[u8]) -> bool {
    let end = image.len().min(SYSTEM_SCAN_BYTES);
    image
        .get(..end)
        .map(|w| w.windows(3).any(|t| t == [0x3A, 0x00, 0xC0]))
        .unwrap_or(false)
}

/// Which machine this image is for, if it says so plainly.
pub fn detect_machine(image: &[u8]) -> Detected {
    // The controller from the boot sector alone, the console from the system
    // tracks. Two windows because they answer two questions with different noise
    // tolerances — see the constants.
    let (boot_ins, boot_outs) = ports_touched(image, BOOT_SCAN_BYTES);
    let (ins, _) = ports_touched(image, SYSTEM_SCAN_BYTES);
    let touched = |p: u8| boot_ins.contains(&p) || boot_outs.contains(&p);

    // Which board's registers does the loader drive? More than one answer means
    // the image is not telling us anything we can act on.
    let boards: Vec<Board> =
        [Board::Dcdd, Board::Hdsk, Board::Tarbell, Board::Z80pack, Board::Cromemco]
        .into_iter()
        .filter(|b| distinctive_ports(*b).iter().any(|p| touched(*p)))
        .collect();
    let board = match boards.as_slice() {
        [one] => *one,
        [] => return Detected::Unclear("no disk-controller registers in its boot code"),
        _ => return Detected::Unclear("its boot code drives more than one controller"),
    };

    // When exactly one machine carries that board, the board *is* the answer and
    // the console adds nothing — there is no second machine to choose between.
    // This is what reaches a CP/M 3 loader whose console arrangement we do not
    // recognise: its controller is unmistakable, and no other machine has it.
    let carriers: Vec<&'static MachineChoice> =
        MACHINE_CHOICES.iter().filter(|m| m.boards.contains(&board)).collect();
    if let [only] = carriers.as_slice() {
        return Detected::Machine(only.key);
    }

    // Otherwise the console would have to separate them — and for the Altair
    // boards it *cannot*, so we do not try.
    //
    // MITS system software chooses its console from the front-panel sense
    // switches at run time (port `FFh`), which is why its BIOS carries drivers
    // for the 88-SIO, the 88-2SIO, the 4PIO and the ACR all at once. Scanning
    // such an image finds console registers it will never use, and the scan
    // cannot tell which. This was measured, not feared: `DISK0E`, an Altair
    // minidisk that boots perfectly on the default machine, was detected as
    // `altair_sio` and went silent. Naming a wrong machine breaks a disk that
    // worked, which is the one thing this must never do.
    //
    // So console evidence is used only for the Tarbell board, where it is both
    // needed and sound — those disks are assembled for one console, with the
    // choice compiled in (`VDM EQU TRUE`, `CSTAT EQU 04H`), and no front panel
    // involved. Everything else keeps the operator's setting.
    if board != Board::Tarbell {
        return Detected::Unclear(
            "its board is shared by several machines and MITS software picks its \
             console from the front panel",
        );
    }

    // A console is attested only when *both* its registers are read — one port
    // could be anything.
    let mut fits: Vec<&'static MachineChoice> = MACHINE_CHOICES
        .iter()
        .filter(|m| m.boards.contains(&board))
        .filter(|m| ins.contains(&m.console.status_port) && ins.contains(&m.console.data_port))
        .collect();

    // A monitor-ROM console is the same port pair as its plain sibling plus a
    // call into the ROM, so the two are separated by that call and nothing else.
    // Without this the pair would always be ambiguous and both would be dropped.
    let calls_cuter = image
        .windows(3)
        .take(SYSTEM_SCAN_BYTES)
        .any(|w| w == [0xCD, 0x19, 0xC0]);
    fits.retain(|m| (m.console.rom != super::console::MonitorRom::None) == calls_cuter);

    match fits.as_slice() {
        [one] => Detected::Machine(one.key),
        [] => Detected::Unclear("its console is not one this gateway has"),
        _ => Detected::Unclear("more than one machine fits its console"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an image whose first bytes touch the given ports, the way a loader
    /// and a BIOS would.
    fn image_touching(ins: &[u8], outs: &[u8], cuter: bool) -> Vec<u8> {
        let mut v = Vec::new();
        for p in outs {
            v.extend_from_slice(&[0xD3, *p]);
        }
        for p in ins {
            v.extend_from_slice(&[0xDB, *p]);
        }
        if cuter {
            v.extend_from_slice(&[0xCD, 0x19, 0xC0]);
        }
        v.resize(4096, 0);
        v
    }

    /// **An Altair disk is deliberately left to the operator's setting.**
    ///
    /// Not a gap — a decision, and one that a real disk forced. The four Altair
    /// machines carry identical boards and differ only in console, and MITS
    /// software picks its console from the front panel at run time, so its image
    /// contains drivers for consoles it will never use. `DISK0E` was detected as
    /// `altair_sio` on that evidence and went silent, having booted perfectly on
    /// the default. Unclear keeps the default, which is what every Altair disk
    /// here needs.
    #[test]
    fn test_an_altair_disk_is_left_to_the_setting() {
        let img = image_touching(&[0x08, 0x10, 0x11], &[0x08, 0x09, 0x0A], false);
        assert!(
            matches!(detect_machine(&img), Detected::Unclear(why) if why.contains("front panel")),
            "an Altair board must not be resolved by console scanning: {:?}",
            detect_machine(&img)
        );
        // And `machine_for` therefore hands back the default, which works.
        let (key, _) = machine_for(super::super::console::AUTO_MACHINE, &img);
        assert_eq!(key, super::super::console::DEFAULT_MACHINE);
    }

    /// The Tarbell VDM machines, separated from each other by one `CALL`.
    #[test]
    fn test_the_two_tarbell_console_machines_are_separated_by_the_rom_call() {
        let plain = image_touching(&[0xF8, 0xFC, 0x04, 0x05], &[0xF8, 0xFA], false);
        assert_eq!(detect_machine(&plain), Detected::Machine("console_04"));

        let rom = image_touching(&[0xF8, 0xFC, 0x04, 0x05], &[0xF8, 0xFA], true);
        assert_eq!(detect_machine(&rom), Detected::Machine("console_04_cuter"));
    }

    /// **The case that needs the controller.** A console at `00h`/`01h` fits an
    /// Altair 88-SIO and z80pack equally, so the console cannot separate them —
    /// but only one machine carries z80pack's board, which settles it outright.
    #[test]
    fn test_the_controller_separates_two_machines_sharing_a_console() {
        let zp = image_touching(&[0x00, 0x01, 0x0E], &[0x0A, 0x0B, 0x0C, 0x0D], false);
        assert_eq!(detect_machine(&zp), Detected::Machine("z80pack"));

        // The Altair side is NOT resolved, on purpose — see the test above.
        let altair = image_touching(&[0x00, 0x01, 0x08], &[0x08, 0x09, 0x0A], false);
        assert!(matches!(detect_machine(&altair), Detected::Unclear(_)));
    }

    /// The shared port is evidence of nothing, and must not be treated as any
    /// board's signature.
    #[test]
    fn test_the_port_two_boards_share_proves_nothing() {
        let img = image_touching(&[0x0A], &[0x0A], false);
        assert!(matches!(detect_machine(&img), Detected::Unclear(_)));
    }

    /// A data disk says nothing, and must not be guessed at.
    #[test]
    fn test_a_disk_with_no_driver_code_is_unclear() {
        assert_eq!(
            detect_machine(&[0xE5u8; 4096]),
            Detected::Unclear("no disk-controller registers in its boot code")
        );
        assert!(matches!(detect_machine(&[]), Detected::Unclear(_)));
    }

    /// The VDM-1 declares itself with the one register it has, and the scan for
    /// it is deliberately the *narrow* one — the system tracks, where a BIOS's
    /// display driver is. Over a whole image a stray `D3 C8` in a data file is
    /// indistinguishable from an `OUT`, which is the same reasoning that bounds
    /// every other window in this module.
    #[test]
    fn test_a_disk_that_drives_the_vdm_scroll_register_says_so() {
        assert!(image_drives_vdm(&image_touching(&[], &[super::super::vdm::SCROLL_PORT], false)));
        // Reading the port is not driving it: the card answers nothing on an
        // `IN`, so an `IN C8h` is somebody else's port, not a VDM-1 driver.
        assert!(!image_drives_vdm(&image_touching(&[super::super::vdm::SCROLL_PORT], &[], false)));
        assert!(!image_drives_vdm(&image_touching(&[0x04], &[0x08], false)));
        assert!(!image_drives_vdm(&[]));
    }

    /// Addressing the screen window is *not* evidence — 60 of the 75 images in
    /// the four collections address `CC00`–`CFFF` for reasons that have nothing
    /// to do with a video card, which is why the conjunction first proposed was
    /// dropped in favour of the port alone.
    #[test]
    fn test_addressing_the_screen_window_is_not_evidence() {
        // `LD HL,CC00h` — a perfectly ordinary thing for a program with 48K of
        // memory to do.
        let mut img = vec![0x21, 0x00, 0xCC];
        img.resize(4096, 0);
        assert!(!image_drives_vdm(&img));
    }

    /// The advance warning is bounded by the system tracks, and beyond them it
    /// says nothing rather than guessing.
    #[test]
    fn test_a_disk_that_tests_for_a_monitor_rom_is_recognised() {
        // `LDA C000` in the system tracks, which is the disk asking whether the
        // monitor is there.
        let mut img = vec![0u8; SYSTEM_SCAN_BYTES];
        img[500..503].copy_from_slice(&[0x3A, 0x00, 0xC0]);
        assert!(image_needs_monitor_rom(&img));

        // Not a read of some other address, and not a *write* there -- `STA
        // C000` is a guest using the memory, not asking about it.
        for other in [[0x3A, 0x01, 0xC0], [0x3A, 0x00, 0xC1], [0x32, 0x00, 0xC0]] {
            let mut img = vec![0u8; SYSTEM_SCAN_BYTES];
            img[500..503].copy_from_slice(&other);
            assert!(!image_needs_monitor_rom(&img), "{other:02X?} is not the signature test");
        }
        assert!(!image_needs_monitor_rom(&[]), "an empty file asks for nothing");
        assert!(!image_needs_monitor_rom(&[0x3A, 0x00]), "a truncated file cannot match");
    }

    /// The same bound as the VDM scan, and for the same reason: three bytes that
    /// spell an instruction turn up in file data constantly, so the window is
    /// the system tracks and nothing beyond them.
    #[test]
    fn test_the_monitor_rom_scan_stops_where_the_system_tracks_do() {
        let mut img = vec![0u8; SYSTEM_SCAN_BYTES + 4096];
        let at = SYSTEM_SCAN_BYTES + 100;
        img[at..at + 3].copy_from_slice(&[0x3A, 0x00, 0xC0]);
        assert!(!image_needs_monitor_rom(&img), "a byte triple in a data file is not a request");
    }

    #[test]
    fn test_the_vdm_scan_stops_where_the_system_tracks_do() {
        let mut img = vec![0u8; SYSTEM_SCAN_BYTES + 64];
        img[SYSTEM_SCAN_BYTES + 8] = 0xD3;
        img[SYSTEM_SCAN_BYTES + 9] = super::super::vdm::SCROLL_PORT;
        assert!(!image_drives_vdm(&img), "a byte pair in a data file is not a driver");
    }

    /// A disk for a controller we have, with a console we do not, is refused
    /// rather than given the nearest console. TDISK04 is *not* an instance of
    /// this and never was: it detects as `console_04`, which is right, because
    /// the console is the keyboard and the keyboard is at `04h`/`05h`. Its
    /// VDM-1 is a display with no port to detect and nothing for a console
    /// choice to say — see [`image_drives_vdm`].
    #[test]
    fn test_a_console_we_do_not_have_is_unclear() {
        // Tarbell registers, and a console at ports nothing here uses.
        let img = image_touching(&[0xF8, 0xFC, 0x71, 0x72], &[0xF8], false);
        assert_eq!(
            detect_machine(&img),
            Detected::Unclear("its console is not one this gateway has")
        );
    }

    /// One console register is not enough — a lone port could be anything, and
    /// noise in a byte scan produces plenty of lone ports.
    #[test]
    fn test_one_console_register_is_not_evidence() {
        let img = image_touching(&[0xF8, 0x04], &[0xF8], false);
        assert!(matches!(detect_machine(&img), Detected::Unclear(_)));
    }

    /// **Every real image: detection must never name a machine that does not
    /// work.**
    ///
    /// The proof the module comment promises, and note carefully what it does and
    /// does not require. Detection is allowed to say `Unclear` — the operator's
    /// setting then stands, and for every Altair-family disk that setting is the
    /// default they already needed. What it may never do is name the *wrong*
    /// machine, because that would take a disk which worked and break it.
    ///
    /// So the criterion is: the answer is either the machine known to work, or
    /// `Unclear` when the default is the machine known to work. An earlier
    /// version of this test demanded a positive answer for every disk and failed
    /// on the 88-HDSK — which has no boot sector driving ports at all, because it
    /// finds its boot program through the volume label instead. That was the test
    /// being wrong about what matters, not the detector.
    ///
    /// Ignored: `CPM_DETECT_DIR` a folder of images, and the expectations are
    /// checked against a table of the disks in the sample sets.
    #[test]
    #[ignore]
    fn test_detect_every_real_image() {
        let Ok(dir) = std::env::var("CPM_DETECT_DIR") else {
            eprintln!("set CPM_DETECT_DIR to run this");
            return;
        };
        // What each disk is known to need, from having been booted on it —
        // keyed by the **CRC-32 of its contents**, with the name kept only so a
        // failure is readable.
        //
        // Keyed on content because a filename is not an identity. Three
        // basenames collide across the sample sets — `cpm13.dsk`, `cpm14.dsk`
        // and `cpm22.dsk` each exist in two z80pack libraries as *different
        // disks* — and keying on the name failed this test on a folder it was
        // never written for: z80pack altairsim's `cpm13.dsk` is
        // "TARBELL 62K CPM V1.3", boots correctly, and was reported as a
        // detection bug because cpmsim's unrelated `cpm13.dsk` sits in this
        // table. The disks this project reads are renamed by their owners as a
        // matter of course, which is the whole reason the product identifies an
        // image by inspection; the test that guards it should not do worse.
        let expect: &[(u32, &str, &str)] = &[
            (0x2739_6D2F, "TDISK01.DSK", "altair_2sio"),
            (0x9E67_A3D1, "TDISK02.DSK", "altair_2sio"),
            (0xF779_6AF5, "TDISK03.DSK", "z80pack"),
            (0xAFDA_1589, "TDISK04.DSK", "console_04"),
            (0xE9DE_0744, "TDISK05.DSK", "console_04_cuter"),
            (0xB88F_A5FB, "DISK01.DSK", "altair_2sio"),
            (0x7A6D_D0E7, "HDSK03.DSK", "altair_2sio"),
            (0xAFA7_7247, "cpm22-1.dsk", "z80pack"),
            (0xF27B_9631, "mpm-1.dsk", "z80pack"),
            (0xD4A4_132F, "ucsd-iv-1.dsk", "z80pack"),
            (0xDB32_6D36, "cpm13.dsk (cpmsim)", "z80pack"),
            // All three Cromemco disks reach a prompt and take a `DIR` on this
            // machine. CDISK01 is the one that matters most here: it is 256,256
            // bytes, so a size can never choose it — only its boot loader's
            // registers can.
            (0x76D8_3B8A, "CDISK01.DSK", "cromemco"),
            (0x77D0_8B46, "CDISK02.DSK", "cromemco"),
            (0x8C92_11D3, "CDISK03.DSK", "cromemco"),
        ];
        let mut names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.to_ascii_lowercase().ends_with(".dsk"))
            .collect();
        names.sort();
        let mut wrong = Vec::new();
        let mut checked = 0usize;
        for name in &names {
            let bytes = std::fs::read(std::path::Path::new(&dir).join(name)).unwrap();
            let got = detect_machine(&bytes);
            let crc = crate::zmodem::crc32(&bytes);
            let want = expect.iter().find(|(c, _, _)| *c == crc).map(|(_, l, m)| (*l, *m));
            if want.is_some() {
                checked += 1;
            }
            let want = want.map(|(_, m)| m);
            let shown = match &got {
                Detected::Machine(m) => (*m).to_string(),
                Detected::Unclear(why) => format!("unclear ({why})"),
            };
            println!("  {name:22} -> {shown}");
            if let Some(want) = want {
                let acceptable = got == Detected::Machine(want)
                    || (matches!(got, Detected::Unclear(_))
                        && want == super::super::console::DEFAULT_MACHINE);
                if !acceptable {
                    wrong.push(format!("{name}: needs {want}, detected {shown}"));
                }
            } else if let Detected::Machine(m) = got {
                // A disk with no known-good machine must not be given one
                // confidently -- that is how a working disk gets broken.
                println!("      (no expectation recorded; detected {m})");
            }
        }
        assert!(wrong.is_empty(), "detection disagreed with what works:\n  {}", wrong.join("\n  "));
        // A folder none of the known disks is in proves nothing, and used to
        // *look* like a pass.  Say so instead.
        println!("  ({checked} of {} images had a recorded expectation)", names.len());
    }
}
