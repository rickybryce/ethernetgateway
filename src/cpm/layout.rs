//! Creating the CP/M container: the sixteen drive folders and the images
//! folder, with the readme that explains how to name a disk image.
//!
//! This runs when the emulator is **enabled**, not when someone first launches
//! it.  The difference matters to an operator: the folders are where you put
//! the software you want to run, and the images folder is where you put a
//! `.dsk` — both of which you want to do *before* your first session, not
//! after.  Turning the emulator on and finding nowhere to put anything is a
//! small thing that wastes real time.
//!
//! Nothing here ever overwrites.  A folder that exists is left exactly as it
//! is, and the readme is written only when absent — an operator who has
//! annotated it keeps their notes.

use super::boot_machine::{BOOT_GEOMETRIES, MAX_IMAGE_TRAILER};
use super::image::{format::FORMATS, IMAGES_DIR};
use std::path::{Path, PathBuf};

/// Name of the container directory under `transfer_dir`.
pub const CPM_DIR: &str = "CPM";

/// Last drive letter — CP/M's FCB drive field is four bits, so P: is the end.
const LAST_DRIVE: u8 = b'A' + super::fs::NUM_DRIVES - 1;

/// The `CPM/` container inside a transfer directory.
///
/// **Canonicalised when it can be**, and that is not cosmetic:
/// `CpmFs::image_file_key` uses a mount's path verbatim as the key for the
/// cross-session per-file write claim, so a drive mounted through a surface
/// that resolved the path and one mounted through a surface that did not would
/// not collide there — and two sessions could interleave records into one file.
/// The shipped `transfer_dir` is relative, so the two spellings really do
/// differ.  One function, because this had already been applied to one surface
/// of three.
///
/// Falls back to the unresolved path before the tree exists, which is the only
/// time it can fail and is exactly when nothing is mounted yet.
pub fn cpm_dir(transfer_dir: &str) -> PathBuf {
    let base = Path::new(transfer_dir).join(CPM_DIR);
    std::fs::canonicalize(&base).unwrap_or(base)
}

/// Create the container, the sixteen drive folders and the images folder,
/// leaving anything that already exists untouched.
///
/// Idempotent, and safe to call from several places at once — two sessions
/// entering the emulator at the same moment on a fresh install both run this,
/// and `create_dir_all` is happy about that.
pub fn ensure_cpm_tree(transfer_dir: &str) -> std::io::Result<()> {
    let base = cpm_dir(transfer_dir);
    for drive in b'A'..=LAST_DRIVE {
        std::fs::create_dir_all(base.join((drive as char).to_string()))?;
    }
    let images = base.join(IMAGES_DIR);
    std::fs::create_dir_all(&images)?;
    write_images_readme(&images);
    Ok(())
}

/// Put `readme.txt` in the images folder if it is not already there.
///
/// Failure is logged and ignored: not having the readme is a missing
/// convenience, and it must not stop the drives from being usable.
fn write_images_readme(images: &Path) {
    let path = images.join("readme.txt");
    if path.exists() {
        return;
    }
    if let Err(e) = std::fs::write(&path, images_readme()) {
        crate::glog!("CP/M: could not write {}: {}", path.display(), e);
    }
}

/// The text of the images-folder readme.
///
/// The format table is rendered from [`FORMATS`] rather than typed out, so a
/// format added to the code cannot go missing from the documentation an
/// operator actually reads.
pub fn images_readme() -> String {
    let mut s = String::new();
    s.push_str(
        "\
CP/M DISK IMAGES
================

Put .dsk disk images in this folder, then mount one on a drive from the
gateway's CP/M settings (telnet), the CP/M mount screen (web), or the
Mount CP/M Drives window (desktop).

A mounted drive reads and writes the CP/M filesystem INSIDE the image
instead of its folder under CPM/.  The folder's own files are not touched
while an image is mounted, and they come straight back when you unmount it.


MOUNTING IS NOT BOOTING
-----------------------

An Altair 88-DCDD floppy can also be BOOTED, which is a different thing.
Mounting gives you one drive of sixteen with the gateway's CP/M underneath.
Booting hands the disk the whole machine, and its OWN operating system runs
— so booting reaches the disks that are not CP/M at all: Altair DOS, Altair
Disk Extended BASIC, Time Sharing BASIC, and CP/M 3.0.

Inside a booted disk there is no jail, no A> from us and no EGT80.  Press
ESC twice to get back to the gateway.  A booted image is opened READ-ONLY
and held by one session at a time.

Your mounted images do come along: each rides the controller unit its
drive letter names (B: is unit 1, C: is unit 2), and the disk being booted
is always unit 0.  What the guest CALLS them, and how many it can reach at
all, belongs to its own BIOS - stock Altair CP/M knows four.  An empty
unit between two full ones answers nothing, exactly as the real board
does, so selecting one looks like a hang; ESC ESC still works.

Set what the CP/M menu item runs with cpm_boot_image (the 'CP/M runs'
setting in every UI), or boot one for a single visit from the telnet boot
picker.  Only 88-DCDD floppies boot: 337,568 bytes (8-inch) or 76,720
(minidisk), plus any short trailer.


NAMING: PUT THE FORMAT IN THE FILENAME
--------------------------------------

Name an image  <format>_<anything>.dsk  — the format token comes first,
then an underscore, then whatever you like:

    ibm3740_cpm22.dsk
    altairhd_cobol.dsk

An image named this way is trusted and mounts READ-WRITE.

An image WITHOUT a format prefix still works, but it mounts READ-ONLY.
The gateway has to guess the format from the file's size and contents, and
a wrong guess cannot be detected afterwards: every offset would be computed
from the wrong geometry, so everything would look consistent right up until
a write landed in the middle of another file.  Reading a guessed image is
safe, so that is allowed; writing to one is not.

To make a read-only image writable, rename it with the right prefix.


FORMATS YOU CAN MOUNT
---------------------

These hold a CP/M filesystem the gateway reads and writes itself.  Name one
with its token to make it writable, as above.

",
    );
    for f in FORMATS {
        s.push_str(&format!("    {:<10} {}\n", f.token, f.label));
        if let Some(size) = f.exact_size {
            s.push_str(&format!("    {:<10} exactly {} bytes\n", "", size));
        }
        s.push('\n');
    }
    s.push_str(
        "\
FORMATS YOU CAN BOOT
--------------------

These are MITS 88-DCDD floppies.  The gateway does not read their
filesystem at all — it runs the disk, and the disk's own operating system
does that work.  There is no naming convention here and nothing to rename:
an image is bootable if it is the right size.  A few bytes of trailer past
the last sector are allowed, because several images in circulation have
one.

",
    );
    for (g, label) in BOOT_GEOMETRIES {
        // The label sits in the mount table's token column: there is no token
        // to put there, and leaving the gap makes the entry look truncated.
        s.push_str(&format!("    {label}\n"));
        s.push_str(&format!(
            "    {:<10} {} bytes = {} tracks x {} sectors x 137\n",
            "",
            g.image_len(),
            g.tracks,
            g.sectors,
        ));
        s.push_str(&format!(
            "    {:<10} plus up to {} bytes of trailer\n\n",
            "", MAX_IMAGE_TRAILER,
        ));
    }
    s.push_str(
        "\
This is how the disks that are NOT CP/M run: Altair DOS, Altair Disk
Extended BASIC and Time Sharing BASIC all boot, and so does CP/M 3.0.
Mounting any of them shows nothing, which is correct — they are not CP/M
filesystems.  A programs disk (data, with no boot sector) is refused.

Not all of these are the same disk.  A 337,568-byte image is an 8-inch
floppy; 76,720 is a minidisk.  The gateway tells them apart by size, so a
truncated or padded file may be refused even though it looks fine.


WHERE TO GET IMAGES, AND WHAT TO RENAME THEM TO
-----------------------------------------------

The gateway ships no disk images.  They are not ours to distribute; the
collections below are maintained by other people, under their own terms,
and are worth getting from the source.

  z80pack - Udo Munk
    https://github.com/udo-munk/z80pack
    The IMSAI 8080 disk library is in  imsaisim/disks/library  and is the
    best starting point: about twenty 8\" single-density disks holding
    CP/M 2.2, CP/M 3, IMDOS, BASICs, comms tools and graphics demos.
    Every one of them is the ibm3740 layout this gateway reads.

  Altair 8800 Simulator - David Hansel
    https://github.com/dhansel/Altair8800
    The  disks  folder holds the MITS Altair, Tarbell and Cromemco
    collections.  The 256,256-byte disks (TDISKnn, CDISK01) are ibm3740.

  Altair-Duino-Disks - J.P. McNeely
    https://github.com/jpmcneely/AltairDuino-Disks
    Hard-disk images for the Altair-Duino, including BASIC, COBOL and
    dBase II.  These are the altairhd format.

Two more places worth knowing for CP/M software in general, though you
will usually be downloading individual programs there rather than whole
disks:

  http://www.retroarchive.org/cpm/
  http://cpmarchives.classiccmp.org/

Rename copies as below.  Sizes are the reliable guide.

ALTAIR-DUINO / ALTAIR 8800 SIMULATOR disks come named after the emulated
controller they belonged to, which says nothing about their layout.  Map
them by their SIZE:

  CDISK01.DSK   256,256 bytes  ->  ibm3740_<what-it-holds>.dsk
  HDSKnn.DSK  4,988,928 bytes  ->  altairhd_<what-it-holds>.dsk
  TDISKnn.DSK   256,256 bytes  ->  ibm3740_<what-it-holds>.dsk

  So a 256,256-byte disk holding WordStar becomes
  ibm3740_wordstar.dsk .

  The 337,568-byte Altair 88-DCDD floppies are a different case: they
  BOOT rather than mount, and need no renaming at all.  Mounting one is
  still not supported — its directory reads but file content past the
  first allocation block does not, so it is refused rather than mounted
  with bad data.  Booting sidesteps that entirely, because the disk's own
  operating system does the reading.

  The same goes for the disks that are not CP/M at all: Altair DOS,
  Altair Disk BASIC, Time Sharing BASIC and the minidisks.  Mounting
  shows no files, because there is no CP/M directory in them to read.
  Boot them instead.

IMSAI 8080esp / z80pack images are 256,256-byte 8\" single-density disks
and are all the IBM 3740 layout:

  anything.dsk  256,256 bytes  ->  ibm3740_<what-it-holds>.dsk

  The UCSD p-System disks in that collection are not CP/M either, and
  behave the same way as the Altair ones above.

Sizes are the reliable guide, and the mount screen tells you if a name
does not match the file.


NOTES
-----

Not every .dsk file holds a CP/M filesystem.  Altair minidisk images are
usually Disk BASIC, and hard-disk images are often unformatted; the gateway
refuses to mount those rather than show you a directory made of file data.

The MITS naming you may already have — DISK01.DSK, TDISK01.DSK, CDISK01.DSK,
HDSK01.DSK — is NOT what the gateway keys off.  Those names say which
emulated controller the disk belonged to, not how the data is laid out.
Rename a copy with a format prefix from the list above to mount it
read-write.

A drive someone is currently using cannot have its disk changed.  The mount
screens show which drives are in use.

Mounting an image on drive A: hides EGT80, the bundled terminal, because
EGT80 lives in the A: folder.  Since transferring files in and out of a
mounted image is done with XMODEM from inside EGT80, you usually want to
mount images on B: or later and leave A: as it is.
",
    );
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("egw_layout_{tag}"));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// Lay the tree out in a real transfer directory, for setting up a machine
    /// by hand.  Ignored: set `CPM_LAYOUT_DIR` to a transfer directory.
    #[test]
    #[ignore]
    fn write_cpm_tree_into() {
        let Ok(dir) = std::env::var("CPM_LAYOUT_DIR") else {
            eprintln!("set CPM_LAYOUT_DIR to run this");
            return;
        };
        ensure_cpm_tree(&dir).unwrap();
        println!("laid out {dir}/CPM");
    }

    #[test]
    fn test_creates_every_drive_and_the_images_folder() {
        let dir = temp("create");
        let t = dir.to_string_lossy().to_string();
        ensure_cpm_tree(&t).unwrap();
        for drive in b'A'..=LAST_DRIVE {
            let d = cpm_dir(&t).join((drive as char).to_string());
            assert!(d.is_dir(), "{}: missing", drive as char);
        }
        assert!(cpm_dir(&t).join(IMAGES_DIR).is_dir());
        assert!(cpm_dir(&t).join(IMAGES_DIR).join("readme.txt").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The whole promise of this module: it never destroys anything.
    #[test]
    fn test_never_overwrites_existing_content() {
        let dir = temp("preserve");
        let t = dir.to_string_lossy().to_string();
        ensure_cpm_tree(&t).unwrap();

        let a_file = cpm_dir(&t).join("A").join("MINE.COM");
        std::fs::write(&a_file, b"my program").unwrap();
        let readme = cpm_dir(&t).join(IMAGES_DIR).join("readme.txt");
        std::fs::write(&readme, b"my own notes").unwrap();

        ensure_cpm_tree(&t).unwrap();
        assert_eq!(std::fs::read(&a_file).unwrap(), b"my program");
        assert_eq!(
            std::fs::read(&readme).unwrap(),
            b"my own notes",
            "an annotated readme must survive"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_is_idempotent() {
        let dir = temp("idem");
        let t = dir.to_string_lossy().to_string();
        for _ in 0..3 {
            ensure_cpm_tree(&t).unwrap();
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every format in the table must appear in the readme, because that file
    /// is the only place an operator learns what to call their image.
    #[test]
    fn test_readme_documents_every_format() {
        let text = images_readme();
        for f in FORMATS {
            assert!(text.contains(f.token), "readme omits format {}", f.token);
            assert!(text.contains(f.label), "readme omits the label for {}", f.token);
        }
    }

    /// Every geometry that BOOTS must appear too.
    ///
    /// The readme used to list only the mountable formats, which meant someone
    /// holding an Altair floppy — the commonest disk there is for this hardware
    /// — read it and concluded the gateway did not support their image, when in
    /// fact it boots. Rendered from `BOOT_GEOMETRIES` so a geometry added to
    /// the code cannot go missing from the file operators actually read.
    #[test]
    fn test_readme_documents_every_bootable_geometry() {
        let text = images_readme();
        assert!(text.contains("FORMATS YOU CAN BOOT"), "the section must exist");
        for (g, label) in BOOT_GEOMETRIES {
            assert!(text.contains(label), "readme omits the bootable {label}");
            assert!(
                text.contains(&g.image_len().to_string()),
                "readme omits the size of {label} ({} bytes)",
                g.image_len()
            );
        }
    }

    /// Mounting and booting are different things and the readme has to say so —
    /// it is the one place an operator meets both without any other context.
    #[test]
    fn test_readme_distinguishes_mounting_from_booting() {
        let text = images_readme();
        assert!(text.contains("MOUNTING IS NOT BOOTING"));
        assert!(text.contains("FORMATS YOU CAN MOUNT"));
        assert!(text.contains("ESC twice"), "must say how to get back out");
        assert!(text.contains("READ-ONLY"), "and that a booted image is protected");
    }

    /// The readme ships to operators.  Naming the upstream *convention*
    /// (`DISKnn.DSK`) is useful — it is what a downloaded file is actually
    /// called — but the index of images and what each one holds is a working
    /// note of ours, not something we distribute or should appear to.
    #[test]
    fn test_readme_carries_no_index_of_sample_images() {
        let text = images_readme().to_ascii_uppercase();
        assert!(!text.contains("DISKDIR"), "the sample-image manifest is not ours to ship");
        // Naming a specific disk and saying what is on it is the shape of that
        // index, and is what must not leak in.
        for claim in ["DISK07.DSK HOLDING", "DISK01.DSK:", "DISK08.DSK"] {
            assert!(!text.contains(claim), "readme describes a specific sample image: {claim}");
        }
    }

    /// A claim that stopped being true is worse than no claim.  These floppies
    /// boot now, and this file is where someone holding one looks first.
    #[test]
    fn test_readme_does_not_still_call_altair_floppies_unsupported() {
        let text = images_readme();
        assert!(
            !text.contains("NOT supported\nyet") && !text.contains("are NOT supported"),
            "the readme still says Altair floppies are unsupported; they boot"
        );
        assert!(text.contains("BOOT rather than mount"), "and it must say what to do instead");
    }

    /// The readme must state the read-only rule, since that is the single
    /// behaviour most likely to surprise someone.
    #[test]
    fn test_readme_explains_the_read_only_rule() {
        let text = images_readme();
        assert!(text.contains("READ-ONLY"));
        assert!(text.contains("rename"), "must say how to fix it");
    }

    /// Lines must fit a narrow terminal — this file is read over telnet on
    /// hardware that may be 80 columns, and it is a plain text file besides.
    /// Print the readme exactly as an operator receives it.  Ignored: a
    /// reading aid, not a check — the file is prose and prose is reviewed by
    /// reading it, which is how the stale "not supported" claim was found.
    #[test]
    #[ignore]
    fn test_show_the_readme() {
        println!("{}", images_readme());
    }

    #[test]
    fn test_readme_lines_fit_eighty_columns() {
        for (n, line) in images_readme().lines().enumerate() {
            assert!(
                line.chars().count() <= 78,
                "readme line {} is {} columns: {line:?}",
                n + 1,
                line.chars().count()
            );
        }
    }
}
