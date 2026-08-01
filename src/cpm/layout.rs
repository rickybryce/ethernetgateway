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

use super::image::{format::FORMATS, IMAGES_DIR};
use std::path::{Path, PathBuf};

/// Name of the container directory under `transfer_dir`.
pub const CPM_DIR: &str = "CPM";

/// Last drive letter — CP/M's FCB drive field is four bits, so P: is the end.
const LAST_DRIVE: u8 = b'A' + super::fs::NUM_DRIVES - 1;

/// The `CPM/` container inside a transfer directory.
pub fn cpm_dir(transfer_dir: &str) -> PathBuf {
    Path::new(transfer_dir).join(CPM_DIR)
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


FORMATS
-------

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
WHERE TO GET IMAGES, AND WHAT TO RENAME THEM TO
-----------------------------------------------

The gateway ships no disk images.  Two well-known collections work well;
download them yourself and rename copies as below.

ALTAIR-DUINO / ALTAIR 8800 SIMULATOR disks come named after the emulated
controller they belonged to, which says nothing about their layout.  Map
them by their SIZE:

  CDISK01.DSK   256,256 bytes  ->  ibm3740_<what-it-holds>.dsk
  HDSKnn.DSK  4,988,928 bytes  ->  altairhd_<what-it-holds>.dsk
  TDISKnn.DSK   256,256 bytes  ->  ibm3740_<what-it-holds>.dsk

  So DISK07.DSK holding WordStar becomes  altair8_wordstar.dsk .

  The 337,568-byte Altair 88-DCDD floppies (DISKnn.DSK) are NOT supported
  yet.  Their directory reads but file content past the first allocation
  block does not, so they are refused rather than mounted with bad data.

  Not every one of these is a CP/M disk either.  The Altair DOS, Altair
  Disk BASIC, Time Sharing BASIC and mini-disk images use their own
  filesystems; they will mount but show no files, because there is no
  CP/M directory in them to read.  That is expected.

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
