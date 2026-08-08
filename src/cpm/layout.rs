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
    let current = images_readme();
    match std::fs::read_to_string(&path) {
        // Ours, and out of date: refresh it.
        //
        // "Never overwrite" was the original rule and it was wrong in the one
        // way that matters — this file is *instructions*, and an operator who
        // has ever launched the gateway keeps whichever version they first ran,
        // for ever. The copy in this repo's own working tree was written on
        // 1 August and still told the reader that an image without a format
        // prefix mounts READ-ONLY and must be renamed to be writable. That
        // stopped being true when identification learned to verify a
        // filesystem, and the stale advice is why this project's own images
        // folder was full of hand-renamed disks. It was also missing the entire
        // "MOUNTING IS NOT BOOTING" section, which is most of what a reader
        // needs.
        //
        // A file that no longer starts with our header is the operator's, not
        // ours, and is never touched — which is the case
        // [`test_never_overwrites_existing_content`] is really about.
        Ok(existing) if existing != current && existing.starts_with(README_HEADER) => {
            if let Err(e) = std::fs::write(&path, &current) {
                crate::glog!("CP/M: could not refresh {}: {}", path.display(), e);
            } else {
                crate::glog!("CP/M: refreshed {} for this version", path.display());
            }
        }
        // Ours and already current, or somebody else's: leave it alone.
        Ok(_) => {}
        Err(_) => {
            if let Err(e) = std::fs::write(&path, &current) {
                crate::glog!("CP/M: could not write {}: {}", path.display(), e);
            }
        }
    }
}

/// The first line of every readme this project has generated.
///
/// The marker for "this file is ours to update". Deliberately the *existing*
/// header rather than a new version stamp: a stamp would only identify readmes
/// written after the stamp was added, and the readmes that need refreshing are
/// precisely the ones written before it.
const README_HEADER: &str = "CP/M DISK IMAGES\n================";

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

An Altair 88-DCDD floppy or an 88-HDSK hard disk can also be BOOTED, which
is a different thing.  Mounting gives you one drive of sixteen with the
gateway's CP/M underneath.  Booting hands the disk the whole machine, and
its OWN operating system runs — so booting reaches the disks that are not
CP/M at all: Altair DOS, Altair Disk Extended BASIC, Time Sharing BASIC,
Hard Disk BASIC, and CP/M 3.0.

Inside a booted disk there is no jail, no A> from us and no EGT80.  Press
ESC twice to get back to the gateway.  A booted image is held by one session
at a time.  Disks are opened READ-ONLY unless the boot picker is told
otherwise, and that one answer covers the mounted disks as well as the booted
one - the guest may write any of them, exactly as a machine with the
write-protect tabs off would.  An image the host will not let us write stays
read-only whatever the picker was told.

Your mounted images do come along: each rides the controller slot its
drive letter names (B: is slot 1, C: is slot 2), and the disk being booted
is always slot 0.  What a slot IS belongs to the board — on the floppy
controllers it is a drive, but on the 88-HDSK it is a PLATTER, four to a
drive, so slots 0-3 are the first Datakeeper's four platters.  What the
guest CALLS them, and how many it can reach at all, belongs to its own
BIOS - stock Altair CP/M knows four drives, the 88-HDSK CP/M uses the
fixed platter (slot 1) as its B:, and Hard Disk BASIC's MOUNT numbers its
disks by platter.  An empty slot between two full ones answers nothing,
exactly as the real board does, so selecting one looks like a hang;
ESC ESC still works.

Set what the CP/M menu item runs with cpm_boot_image (the 'CP/M runs'
setting in every UI), or boot one for a single visit from the telnet boot
picker.  What can be booted is decided by size alone - see FORMATS YOU CAN
BOOT below.


NAMING: PUT THE FORMAT IN THE FILENAME
--------------------------------------

Name an image  <format>_<anything>.dsk  — the format token comes first,
then an underscore, then whatever you like:

",
    );
    // One example per mountable format, from the same table the section below
    // lists.  Written out by hand, this block offered two of the three tokens
    // for as long as `altair8` had been mountable — and the test that guards
    // this named the missing one, so the readme and its test each knew half.
    for f in FORMATS {
        s.push_str(&format!("    {}_<what-it-holds>.dsk\n", f.token));
    }
    s.push_str(
        "
An image named this way is trusted and mounts READ-WRITE.

YOU DO NOT HAVE TO RENAME ANYTHING.  An image without a prefix is
identified by inspection, and if its CP/M filesystem checks out it mounts
READ-WRITE just the same.  No two formats here are the same size, so the
size names the format outright; what inspection actually decides is whether
the file really holds that filesystem.  The whole directory is read and
checked -- every allocation block inside the disk, no block claimed by two
files, every record count matching the blocks it claims.  Random bytes under
the wrong geometry fail that immediately.

An image that does NOT check out still mounts, but READ-ONLY, and says what
was wrong with it.  That is the honest answer for a file which is the right
size and is not this filesystem: plenty of disks are 256,256 bytes without
being CP/M at all -- a UCSD p-System disk is, and so is a Cromemco CDOS one.
Reading such an image is safe and often what you want; writing to it is not.

Renaming with a prefix is therefore an override, not a requirement: it says
\"I know what this is\" and skips the inspection.


FORMATS YOU CAN MOUNT
---------------------

These hold a CP/M filesystem the gateway reads and writes itself.  A prefix
is optional, as above: a sound filesystem is writable either way.

",
    );
    for f in FORMATS {
        s.push_str(&format!("    {:<10} {}\n", f.token, f.label));
        if let Some(size) = f.exact_size {
            // "or a little more", not "exactly": several images in circulation
            // carry a few bytes past their last record, and refusing those was
            // a real defect on both paths.  The mount side tolerates anything
            // short of one whole record, past which it is a different geometry.
            s.push_str(&format!(
                "    {:<10} {} bytes (a short trailer is OK)\n",
                "", size
            ));
        }
        s.push('\n');
    }
    s.push_str(
        "\
FORMATS YOU CAN BOOT
--------------------

The gateway does not read the filesystem of these at all — it runs the
disk, and the disk's own operating system does that work.  There is no
naming convention here and nothing to rename: an image is bootable if it is
the right size.  A few bytes of trailer past the last sector are allowed,
because several images in circulation have one.

",
    );
    // Asked of the machine rather than listed here.  A controller that can boot
    // a medium says so itself, so adding a board documents it — this section
    // said "these are MITS 88-DCDD floppies" for as long as the hard disk had
    // been booting them.
    for m in super::boot_machine::BootMachine::bootable_media() {
        // The label sits in the mount table's token column: there is no token
        // to put there, and leaving the gap makes the entry look truncated.
        s.push_str(&format!("    {}\n", m.label));
        s.push_str(&format!("    {:<10} {} bytes = {}\n", "", m.bytes, m.shape));
        s.push_str(&format!(
            "    {:<10} plus up to {} bytes of trailer\n\n",
            "", m.trailer,
        ));
    }
    s.push_str(
        "\
This is how the disks that are NOT CP/M run: Altair DOS, Altair Disk
Extended BASIC, Time Sharing BASIC and Hard Disk BASIC all boot, and so
does CP/M 3.0.  Mounting any of them shows nothing, which is correct — they
are not CP/M filesystems.  A programs disk (data, with no boot sector) is
refused.

Not all of these are the same disk, and a size that no board claims is
refused — so a truncated or badly padded file may be refused even though it
looks fine.  Three boards claim 256,256 bytes: it is a Tarbell 1011 floppy,
a z80pack 8\" disk and a Cromemco single-density floppy, all raw 26-sector
tracks that look alike from outside.  What settles it is not the file but
the boot loader inside it, which has to drive its own controller's
registers — see the machine setting below.

THE MACHINE MATTERS, AND IT IS A SEPARATE SETTING.  A booted disk brings its
own operating system, and that system was written for a particular machine —
so it expects its console, and its disk controller, at particular ports.
cpm_boot_machine says which machine to be, and it DEFAULTS TO auto: the disk
is asked.  A boot loader has to drive its own controller's registers, so the
image states which machine it is for, and that is read rather than guessed.
When a disk does not say plainly the Altair default stands, and the boot
screen tells you which of the two happened.  Setting a machine explicitly
always overrides it.

The symptom of getting it wrong is distinctive and worth knowing, because it
looks like a broken disk: the disk LOADS, reads its sectors, and then goes
completely quiet.  It is printing to hardware that is not in the machine and
polling a keyboard that will never answer.  Nothing is wrong with the image.


WHERE TO GET IMAGES
-------------------

The gateway ships no disk images.  They are not ours to distribute; the
collections below are maintained by other people, under their own terms,
and are worth getting from the source.

  z80pack - Udo Munk
    https://github.com/udo-munk/z80pack
    The IMSAI 8080 disk library is in  imsaisim/disks/library  and is the
    best starting point: about twenty 8\" single-density disks holding
    CP/M 1.3 through 3.0, MP/M, IMDOS, BASICs, comms tools and demos.
    All are 256,256 bytes; the CP/M ones are the ibm3740 layout this
    gateway reads, and the rest (UCSD p-System) boot but do not mount.

  Altair 8800 Simulator - David Hansel
    https://github.com/dhansel/Altair8800
    The  disks  folder holds the MITS Altair, Tarbell and Cromemco
    collections.  All of them boot.  For mounting, the TDISKnn disks
    are ibm3740; the CDISKnn ones are Cromemco and boot only.

  Altair-Duino-Disks - J.P. McNeely
    https://github.com/jpmcneely/AltairDuino-Disks
    Hard-disk images for the Altair-Duino, including BASIC, COBOL and
    dBase II.  These are the altairhd format.

Two more places worth knowing for CP/M software in general, though you
will usually be downloading individual programs there rather than whole
disks:

  http://www.retroarchive.org/cpm/
  http://cpmarchives.classiccmp.org/

DROP THEM IN AS THEY COME.  Every collection above is named after the
emulated controller a disk belonged to, which says nothing about its
layout — and none of that matters any more.  Copy the files into the
images folder under whatever names they arrived with, and the gateway
works out what each one is.

  BOOTING needs no renaming, and never did.
  MOUNTING inspects the filesystem, and a sound one is writable.

Renaming is only worth doing for two reasons.  One is that you want a
name you can read, in which case put the prefix on so the gateway does
not have to guess: ibm3740_wordstar.dsk .  The other is that a disk you
are sure about was refused, and you want to overrule that.

  BE CAREFUL WITH THE SECOND ONE.  A prefix SKIPS the inspection, so it
  turns a read-only refusal into a writable mount — including when the
  refusal was right.  CDISK01.DSK is exactly that trap: it is 256,256
  bytes, so it looks like an ibm3740 disk, but it holds CROMEMCO CDOS
  and not a CP/M filesystem at all.  Naming it ibm3740_ would let the
  gateway write CP/M structures over a filesystem it cannot read.  It
  boots perfectly as it stands.

Disks that are not CP/M can only be booted, and that is correct rather
than a fault — mounting one shows no files because there is no CP/M
directory in it to read.  In the collections above that means Altair
DOS, Altair Disk Extended BASIC, Time Sharing BASIC, Hard Disk BASIC,
the minidisks, the UCSD p-System disks and Cromemco CDOS.

The mount screen always says which way a disk was taken, and when it is
read-only it says what was wrong.


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

Mounting an image on drive A: hides BOTH bundled terminals, because
EGT8080.COM and EGT80.COM live in the A: folder.  Since transferring
files in and out of a mounted image is done with XMODEM from inside the
terminal, you usually want to mount images on B: or later and leave A:
as it is.  (EGT8080 is the one to run: it is built to the 8080's
instruction set, so it works whichever processor cpm_cpu selects.)
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

    /// **A readme we wrote is refreshed; one the operator wrote is not.**
    ///
    /// The file is instructions, and the original "never overwrite" rule meant
    /// an operator kept whichever version they first ran for ever. This repo's
    /// own working copy was three months stale: it still said an unprefixed
    /// image mounts read-only and must be renamed — untrue since identification
    /// learned to verify a filesystem, and the reason this project's images
    /// folder was full of hand-renamed disks.
    #[test]
    fn test_a_stale_generated_readme_is_refreshed() {
        let dir = temp("refresh");
        let t = dir.to_string_lossy().to_string();
        ensure_cpm_tree(&t).unwrap();
        let readme = cpm_dir(&t).join(IMAGES_DIR).join("readme.txt");

        // An older generated readme: our header, but text that has moved on.
        let stale = format!("{README_HEADER}\n\nsomething we used to say.\n");
        std::fs::write(&readme, &stale).unwrap();
        ensure_cpm_tree(&t).unwrap();
        assert_eq!(
            std::fs::read_to_string(&readme).unwrap(),
            images_readme(),
            "a generated readme that has fallen behind must be brought up to date"
        );

        // The operator's own file keeps its content, even in this folder.
        std::fs::write(&readme, b"MY NOTES\n========\n\nhands off.\n").unwrap();
        ensure_cpm_tree(&t).unwrap();
        assert_eq!(
            std::fs::read_to_string(&readme).unwrap(),
            "MY NOTES\n========\n\nhands off.\n",
            "a file that is not one of ours must never be rewritten"
        );

        // And a current one is not rewritten needlessly.
        std::fs::write(&readme, images_readme()).unwrap();
        let before = std::fs::metadata(&readme).unwrap().modified().unwrap();
        ensure_cpm_tree(&t).unwrap();
        assert_eq!(std::fs::metadata(&readme).unwrap().modified().unwrap(), before);

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

    /// Every medium that BOOTS must appear too.
    ///
    /// The readme used to list only the mountable formats, which meant someone
    /// holding an Altair floppy — the commonest disk there is for this hardware
    /// — read it and concluded the gateway did not support their image, when in
    /// fact it boots. Then it listed the floppy geometries *only*, and said so in
    /// prose ("these are MITS 88-DCDD floppies"), for as long as the hard disk
    /// had been booting. Both faults were the same one: a second list. It is now
    /// rendered from the machine's own controllers, so a board that can boot a
    /// medium documents it by existing.
    #[test]
    fn test_readme_documents_every_bootable_medium() {
        let text = images_readme();
        assert!(text.contains("FORMATS YOU CAN BOOT"), "the section must exist");
        let media = crate::cpm::boot_machine::BootMachine::bootable_media();
        assert!(media.len() >= 3, "the floppy, the minidisk and the hard disk at least");
        for m in media {
            assert!(text.contains(m.label), "readme omits the bootable {}", m.label);
            assert!(
                text.contains(&m.bytes.to_string()),
                "readme omits the size of {} ({} bytes)",
                m.label,
                m.bytes
            );
        }
        assert!(
            !text.contains("These are MITS 88-DCDD floppies"),
            "the prose must not narrow what the list says"
        );
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

    /// A claim that stopped being true is worse than no claim.  Altair floppies
    /// boot *and* mount now, and this file is where someone holding one looks
    /// first.
    ///
    /// This test was itself stale twice over, which is the lesson in it. It
    /// pinned two exact spellings — `"NOT supported\nyet"` and `"are NOT
    /// supported"` — and so sat green while the readme said "Mounting one is
    /// still not supported" two lines away in a third wording. And it *required*
    /// the phrase "BOOT rather than mount", which was true when only booting
    /// worked and became the stale claim itself once `altair8` mounted. A guard
    /// on the claim, not on the sentence: search case-insensitively, and require
    /// that the format token is offered for renaming like every other mountable
    /// format.
    #[test]
    fn test_readme_does_not_call_altair_floppies_unmountable() {
        let text = images_readme();
        let flat = text.to_ascii_lowercase().replace('\n', " ");
        for claim in ["not supported", "unsupported", "cannot be mounted", "refused rather than"] {
            assert!(
                !flat.contains(claim),
                "the readme still disclaims a format that works: {claim:?}"
            );
        }
        // The positive half: an operator has to be told the token to name a
        // disk with, for *every* mountable format.
        //
        // This used to name `altair8` alone, and so went stale a third time —
        // in the other direction. The readme's example block was hand-written
        // and offered two of the three tokens; the test knew the third. Between
        // them they covered everything and neither was the source. Both now come
        // off `FORMATS`, so a new mountable format is documented or this fails.
        assert!(!FORMATS.is_empty(), "this test is vacuous with no formats");
        for f in FORMATS {
            assert!(
                text.contains(&format!("{}_", f.token)),
                "the readme must offer the {} rename, since these mount",
                f.token
            );
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
