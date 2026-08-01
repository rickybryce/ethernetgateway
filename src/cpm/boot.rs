//! Cold-starting a disk image: the 88-DCDD bootstrap.
//!
//! A real Altair boots a disk from a small PROM — the disk bootstrap loader —
//! which selects drive 0, loads the head, waits for sector 0 to come round,
//! copies its data into low memory and jumps there. From that point the disk's
//! own code is in charge, which is the entire point of this path: the layout
//! knowledge stays on the disk, in software that already works.
//!
//! We do not have the PROM, so the sequence is written here instead. That is a
//! deliberate substitution and worth being clear about — it means this code
//! must land the payload at exactly the address the PROM would, or the boot
//! sector's own jumps go to the wrong place.
//!
//! **The address is not a guess.** The boot sector of a real Altair CP/M disk
//! begins `31 00 DF` (`LXI SP,0DF00h`), `F3` (`DI`), then talks to the
//! controller with `D3 08` / `DB 08`. Its absolute jumps target `0007h`,
//! `0015h`, `0020h`, `0030h`, `0048h` — and each of those matches the offset of
//! the corresponding instruction *within the payload itself*. The jump to
//! `0007h`, for instance, lands on the `DB 08` at payload offset 7. That only
//! works if the payload sits at `0000h`, so it is loaded there and entered
//! there.
//!
//! The 128 data bytes sit at offset 3 of the 137-byte sector. That offset is
//! confirmed for the boot region by the same evidence — the code decodes and
//! its jumps line up — and independently by the fact that this is where the
//! CP/M directory is found on these disks.

use super::dcdd::{Dcdd, Request, SECTOR_LEN};

/// Where the bootstrap puts the boot sector, and enters it.
pub const BOOT_LOAD_ADDR: u16 = 0x0000;

/// Offset of the 128 data bytes inside a boot-region sector.
pub const BOOT_DATA_OFFSET: usize = 3;

/// Bytes the bootstrap transfers per sector.
pub const BOOT_DATA_LEN: usize = 128;

/// Sectors the bootstrap loads, and the step between them.
///
/// One sector is not enough: the loader in sector 0 calls a read routine at
/// `0092h`, which is past the 128 bytes that sector holds. The step is **2**
/// because that is the interleave the disk's own loader uses — its inner loop
/// does `ADI 02` on the sector number and wraps at 33 — so consecutive 128-byte
/// chunks live in sectors 0, 2, 4, … and sector 1 is genuinely empty.
pub const BOOT_SECTORS: u8 = 4;
/// Physical sectors between consecutive boot chunks.
pub const BOOT_INTERLEAVE: u8 = 2;

/// Why a boot did not happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootError {
    /// No disk in the drive the bootstrap was pointed at.
    NoDisk(u8),
    /// The image could not supply the boot sector.
    Unreadable(String),
    /// Sector 0 never came round.
    ///
    /// On real hardware this cannot happen — the disk turns. Here it means the
    /// controller is not advancing, which is the failure the rotation model
    /// exists to prevent, so it is reported as itself rather than left to look
    /// like a hung guest.
    NeverPositioned,
    /// The sector holds nothing that could be code.
    NotBootable,
}

impl std::fmt::Display for BootError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BootError::NoDisk(d) => {
                write!(f, "drive {d} is empty — put a disk image in it first")
            }
            BootError::Unreadable(e) => write!(f, "could not read the boot sector: {e}"),
            BootError::NeverPositioned => {
                write!(f, "the disk never presented sector 0 — the controller is not turning")
            }
            BootError::NotBootable => write!(
                f,
                "this image has no boot sector — it is data, not a system disk"
            ),
        }
    }
}

// WHERE THIS STANDS, for whoever picks it up next.
//
// A real Altair CP/M disk boots, seeks to track 0, and runs its loader.  It
// does *not* reach a sign-on.  The evidence, gathered by tracing the program
// counter (set `CPM_BOOT_TRACE` on the boot test) and counting port accesses:
//
//   * Control flow is right up to a point: 0000 -> 0020 (the track-0 test,
//     which passes) -> 0027 -> 002d -> a CALL to 0048, which loops a few times
//     around 0048..0062, then goes 004d -> 0050 -> 0072 -> 0074 -> **0092**.
//     0092 is past the 128-byte payload, in memory that was never loaded, and
//     from there it wanders through zeros until it stops making sense.
//   * In a whole run the guest touches **only** port 08h (742 reads) and 09h
//     (371 writes).  It never reads the sector-position register and never
//     reads the data port — so it never actually transfers a sector, which is
//     why nothing is loaded for it to jump to.
//   * The loader sets `HL = 0DD80h` and `BC = 0100h` before that CALL, so it
//     intends to load CP/M high; the routine it calls is not doing so.
//
// So the fault is in the read path the loader uses, not in seeking or in the
// status bits (both verified: status reads 0xA1 — at track 0, head may move).
// The next step is to disassemble 0048..0074 from the boot sector and work out
// which register or status bit the read routine is waiting on that we are not
// providing.  Sector 1 of the disk is all zeros while sector 2 holds code,
// which hints the loader expects a sector interleave rather than physically
// sequential reads.

/// How many position-register reads to allow before giving up.
///
/// Two per sector, so a full revolution is `2 * sectors`. A generous multiple
/// of that means a working controller always succeeds and a broken one fails
/// quickly instead of spinning.
const MAX_POLLS: usize = 512;

/// Run the bootstrap: leave the boot sector in memory and return the entry
/// point.
///
/// `fetch` supplies a physical sector; the controller asks and the caller
/// reads, so file access stays where its bounds checks are. `store` receives
/// the payload and the address to put it at.
pub fn cold_boot<F, S>(
    dcdd: &mut Dcdd,
    drive: u8,
    mut fetch: F,
    mut store: S,
) -> Result<u16, BootError>
where
    F: FnMut(u8, u8, u8) -> Result<Vec<u8>, String>,
    S: FnMut(u16, &[u8]),
{
    if !dcdd.has_disk(drive) {
        return Err(BootError::NoDisk(drive));
    }

    // What the PROM does: select the drive, put the head down, wait for
    // sector 0, read it.
    dcdd.port_out(0x08, drive & 0x0F);
    dcdd.port_out(0x09, 0x04); // head load

    let mut positioned = false;
    for _ in 0..MAX_POLLS {
        let (v, _) = dcdd.port_in(0x09);
        if v == 0xFF {
            continue;
        }
        let sector = (v >> 1) & 0x1F;
        let sector_true = v & 0x01 == 0;
        if sector == 0 && sector_true {
            positioned = true;
            break;
        }
    }
    if !positioned {
        return Err(BootError::NeverPositioned);
    }

    // Reading the data port asks for the sector; satisfy that, then take the
    // bytes straight from the buffer rather than clocking 137 port reads.
    let (_, req) = dcdd.port_in(0x0A);
    let (track, sector) = match req {
        Request::Read { track, sector, .. } => (track, sector),
        _ => (0, 0),
    };
    let raw = fetch(drive, track, sector).map_err(BootError::Unreadable)?;
    if raw.len() < BOOT_DATA_OFFSET + BOOT_DATA_LEN {
        return Err(BootError::Unreadable(format!(
            "boot sector is {} bytes, expected {SECTOR_LEN}",
            raw.len()
        )));
    }
    dcdd.sector_loaded(drive, &raw);

    let payload = &raw[BOOT_DATA_OFFSET..BOOT_DATA_OFFSET + BOOT_DATA_LEN];
    if !looks_bootable(payload) {
        return Err(BootError::NotBootable);
    }
    // The rest of the loader, from the interleaved sectors that follow.  A
    // sector that cannot be read stops the copy rather than failing the boot:
    // a shorter loader is legitimate, and the guest will notice long before we
    // could.
    for i in 1..BOOT_SECTORS {
        let sec = sector + i * BOOT_INTERLEAVE;
        let Ok(more) = fetch(drive, track, sec) else { break };
        if more.len() < BOOT_DATA_OFFSET + BOOT_DATA_LEN {
            break;
        }
        store(
            BOOT_LOAD_ADDR + i as u16 * BOOT_DATA_LEN as u16,
            &more[BOOT_DATA_OFFSET..BOOT_DATA_OFFSET + BOOT_DATA_LEN],
        );
    }
    // Close the transfer the bootstrap opened.  Leaving it open holds the
    // "safe to move the head" status bit low, and the first thing a boot
    // sector does is seek to track 0 — so the guest would spin at its very
    // first instruction that touches the drive.
    dcdd.end_transfer(drive);
    store(BOOT_LOAD_ADDR, payload);
    Ok(BOOT_LOAD_ADDR)
}

/// Does this payload look like a boot sector rather than data?
///
/// A cheap sanity check, not a verifier. It exists so that booting a data disk
/// says so instead of running whatever bytes happened to be there — an 8080
/// turned loose on text will do something, and what it does is never useful.
///
/// The test is deliberately weak in one direction: it rejects only the clearly
/// impossible. A blank or text-filled sector is refused; anything with the
/// shape of code is allowed through, because deciding what is *really* a
/// program is not a job a heuristic can do.
pub fn looks_bootable(payload: &[u8]) -> bool {
    if payload.len() < 8 {
        return false;
    }
    // All-identical bytes is an erased or unformatted sector.
    if payload.iter().all(|&b| b == payload[0]) {
        return false;
    }
    // Entirely printable text is a data sector, not code.
    let printable = payload
        .iter()
        .filter(|&&b| (0x20..0x7F).contains(&b) || b == b'\r' || b == b'\n')
        .count();
    if printable == payload.len() {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::super::dcdd::{Disk, Geometry};
    use super::*;

    /// The first bytes of a real Altair CP/M boot sector: LXI SP,0DF00h / DI /
    /// XRA A / OUT 08h / IN 08h / ANI 08h / JNZ 0007h.
    const REAL_BOOT_START: &[u8] = &[
        0x31, 0x00, 0xDF, 0xF3, 0xAF, 0xD3, 0x08, 0xDB, 0x08, 0xE6, 0x08, 0xC2, 0x07, 0x00,
    ];

    fn boot_sector() -> Vec<u8> {
        let mut s = vec![0u8; SECTOR_LEN];
        s[0] = 0x80; // track 0, high bit set as the controller writes it
        s[BOOT_DATA_OFFSET..BOOT_DATA_OFFSET + REAL_BOOT_START.len()]
            .copy_from_slice(REAL_BOOT_START);
        s
    }

    fn with_disk() -> Dcdd {
        let mut c = Dcdd::new();
        c.insert(0, Disk { geometry: Geometry::EIGHT_INCH, read_only: false });
        c
    }

    #[test]
    fn test_cold_boot_loads_the_sector_and_enters_at_zero() {
        let mut c = with_disk();
        let mut stored: Option<(u16, Vec<u8>)> = None;
        let entry = cold_boot(
            &mut c,
            0,
            |_, _, _| Ok(boot_sector()),
            |addr, bytes| stored = Some((addr, bytes.to_vec())),
        )
        .expect("boots");
        assert_eq!(entry, 0x0000, "the boot sector's own jumps assume 0000h");
        let (addr, bytes) = stored.expect("something was stored");
        assert_eq!(addr, 0x0000);
        assert_eq!(bytes.len(), BOOT_DATA_LEN);
        assert_eq!(&bytes[..REAL_BOOT_START.len()], REAL_BOOT_START);
    }

    /// The payload must be taken from offset 3, not from the start of the
    /// sector — the first three bytes are the controller's own header, and
    /// loading them would put `80 00 01` where the entry point belongs.
    #[test]
    fn test_the_sector_header_is_not_part_of_the_payload() {
        let mut c = with_disk();
        let mut stored = Vec::new();
        cold_boot(&mut c, 0, |_, _, _| Ok(boot_sector()), |_, b| stored = b.to_vec()).unwrap();
        assert_eq!(stored[0], 0x31, "must start at the LXI SP, not the header");
        assert_ne!(stored[0], 0x80);
    }

    #[test]
    fn test_booting_an_empty_drive_says_so() {
        let mut c = Dcdd::new();
        let err = cold_boot(&mut c, 0, |_, _, _| Ok(boot_sector()), |_, _| {}).unwrap_err();
        assert_eq!(err, BootError::NoDisk(0));
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn test_an_unreadable_sector_is_reported_not_run() {
        let mut c = with_disk();
        let err = cold_boot(&mut c, 0, |_, _, _| Err("disk on fire".into()), |_, _| {})
            .unwrap_err();
        assert!(matches!(err, BootError::Unreadable(_)));
        assert!(err.to_string().contains("disk on fire"));
    }

    #[test]
    fn test_a_short_sector_is_refused() {
        let mut c = with_disk();
        let err = cold_boot(&mut c, 0, |_, _, _| Ok(vec![0u8; 40]), |_, _| {}).unwrap_err();
        assert!(matches!(err, BootError::Unreadable(_)));
    }

    /// A data disk must be refused rather than entered.  An 8080 turned loose
    /// on text does something, and it is never useful.
    #[test]
    fn test_a_data_disk_is_not_booted() {
        let mut c = with_disk();
        let mut text = vec![0u8; SECTOR_LEN];
        for (i, b) in text[BOOT_DATA_OFFSET..].iter_mut().enumerate() {
            *b = b" ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789"[i % 37];
        }
        let err = cold_boot(&mut c, 0, |_, _, _| Ok(text.clone()), |_, _| {}).unwrap_err();
        assert_eq!(err, BootError::NotBootable);
        assert!(err.to_string().contains("data, not a system disk"));
    }

    #[test]
    fn test_a_blank_sector_is_not_booted() {
        let mut c = with_disk();
        let err = cold_boot(&mut c, 0, |_, _, _| Ok(vec![0xE5; SECTOR_LEN]), |_, _| {})
            .unwrap_err();
        assert_eq!(err, BootError::NotBootable);
    }

    #[test]
    fn test_bootable_heuristic_accepts_code_and_rejects_the_impossible() {
        assert!(looks_bootable(REAL_BOOT_START));
        assert!(!looks_bootable(&[0; 128]), "erased");
        assert!(!looks_bootable(&[0xE5; 128]), "unformatted");
        assert!(!looks_bootable(b"PLAIN TEXT ON A DATA DISK, NOTHING ELSE HERE AT ALL"));
        assert!(!looks_bootable(&[0x31, 0x00]), "too short to judge");
    }

    /// Boot every real image in a folder and report what happened.
    ///
    /// The end-to-end check for this stage: a genuine Altair disk must load and
    /// return an entry point, and a data disk must be refused.  Ignored — set
    /// `CPM_BOOT_DIR` to a folder of `.dsk` files.
    #[test]
    #[ignore]
    fn test_boot_real_images() {
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
        let mut booted = 0;
        for name in &names {
            let path = std::path::Path::new(&dir).join(name);
            let bytes = std::fs::read(&path).unwrap();
            let geom = if bytes.len() as u64 == Geometry::MINIDISK.image_len() {
                Geometry::MINIDISK
            } else {
                Geometry::EIGHT_INCH
            };
            if bytes.len() as u64 != geom.image_len() {
                println!("  skipped  {name} ({} bytes — not an 88-DCDD image)", bytes.len());
                continue;
            }
            let mut c = Dcdd::new();
            c.insert(0, Disk { geometry: geom, read_only: true });
            let mut first = Vec::new();
            match cold_boot(
                &mut c,
                0,
                |_, t, s| {
                    let off = geom.offset(t, s) as usize;
                    Ok(bytes[off..off + SECTOR_LEN].to_vec())
                },
                |_, b| first = b.to_vec(),
            ) {
                Ok(entry) => {
                    booted += 1;
                    println!(
                        "  BOOTS    {name} -> {entry:#06x}, first bytes {:02x?}",
                        &first[..6]
                    );
                }
                Err(e) => println!("  refused  {name}: {e}"),
            }
        }
        assert!(booted > 0, "no image in {dir} produced a boot sector");
    }

    /// The bootstrap waits for sector 0 specifically, not merely for any
    /// sector — a boot from the wrong sector runs the wrong bytes.
    #[test]
    fn test_the_bootstrap_waits_for_sector_zero() {
        let mut c = with_disk();
        let mut asked = Vec::new();
        cold_boot(
            &mut c,
            0,
            |d, t, s| {
                asked.push((d, t, s));
                Ok(boot_sector())
            },
            |_, _| {},
        )
        .unwrap();
        // Sector 0 first, then the interleaved sectors that hold the rest of
        // the loader.
        assert_eq!(asked[0], (0, 0, 0), "the boot sector comes first");
        assert_eq!(
            asked,
            (0..BOOT_SECTORS)
                .map(|i| (0u8, 0u8, i * BOOT_INTERLEAVE))
                .collect::<Vec<_>>(),
            "the loader is read with the disk's own 2:1 interleave"
        );
    }
}
