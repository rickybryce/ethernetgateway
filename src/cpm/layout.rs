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
//! Nothing here overwrites an operator's own work.  A folder that exists is left
//! exactly as it is.  The readme is the one exception and a deliberate one: it
//! is *instructions*, so a copy we wrote that has fallen behind the code is
//! refreshed rather than left to mislead for ever.  A file that is not one of
//! ours — anything not starting with our header — is never touched.

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
    write_repo_disks(&images);
    // The ROMs folder, on the same terms: an empty folder that does not say what
    // it is for is the `(drive folder)` mistake one level down, so it arrives
    // with its readme.
    let roms = base.join(super::rom::ROMS_DIR);
    std::fs::create_dir_all(&roms)?;
    write_roms_readme(&roms);
    Ok(())
}

/// Put `readme.txt` in the ROMs folder, refreshing a stale copy of ours.
///
/// Same rule as the images readme, including why it may be overwritten: it is
/// instructions, and an operator otherwise keeps whichever version they first
/// ran for ever.
fn write_roms_readme(roms: &Path) {
    let path = roms.join("readme.txt");
    let current = roms_readme();
    match std::fs::read_to_string(&path) {
        Ok(existing) if existing != current && existing.starts_with(ROMS_README_HEADER) => {
            if let Err(e) = std::fs::write(&path, &current) {
                crate::glog!("CP/M: could not refresh {}: {}", path.display(), e);
            }
        }
        Ok(_) => {}
        Err(_) => {
            if let Err(e) = std::fs::write(&path, &current) {
                crate::glog!("CP/M: could not write {}: {}", path.display(), e);
            }
        }
    }
}

/// The marker for "this ROMs readme is ours to update".
const ROMS_README_HEADER: &str = "CP/M MONITOR ROMS\n=================";

/// The text of the ROMs-folder readme.
///
/// The table is rendered from [`super::rom::ROM_CHOICES`] rather than typed out,
/// so a ROM added to the code cannot go missing from the file an operator reads
/// — the same rule as the format table above.
pub fn roms_readme() -> String {
    let mut s = String::new();
    s.push_str(
        "\
CP/M MONITOR ROMS
=================

Some disks are not self-contained.  They carry an operating system and a
BIOS, and then print through a routine that was never on the disk, because
on the machine they were built for it was already in memory -- a monitor in
ROM, or loaded from tape before the disk went in.  Such a disk boots into
silence, and neither the disk nor the gateway is at fault.

This folder is where those monitors go.  Put the file here, then choose it
with `cpm_boot_rom` from the gateway's CP/M settings (telnet), the CP/M
settings page (web), or the CP/M window (desktop).  Every one of those
screens will also offer to FETCH the file for you, and the sample-disk
download brings it along -- a file arriving here does not switch anything
on, it only lets you choose it.

The gateway ships no ROMs.  They are not ours to distribute; the file is
fetched from its author's own repository, pinned to one commit and checked
against a SHA-256 we recorded from a copy we tested.  A file already in
this folder is never overwritten -- it may be yours, and a monitor is
exactly the sort of thing a hobbyist patches.  To re-fetch one, delete it
first.

Intel HEX (.hex) and raw binary are both accepted.  A HEX file says where
its own bytes belong; a raw binary is loaded at the start of the window
below.  Either way the gateway refuses a file whose bytes fall outside that
window, because a monitor assembled for a different address would be
written over the guest's own memory -- which looks like a disk that boots
and then behaves impossibly.

WHAT IS ON OFFER
----------------
",
    );
    for c in super::rom::ROM_CHOICES {
        let Some(f) = c.rom.as_ref() else { continue };
        s.push_str(&format!(
            "  {}\n    file    {}\n    window  {:04X}-{:04X}\n    setting cpm_boot_rom = {}\n\n",
            c.description, f.file, f.span.0, f.span.1, c.key
        ));
    }
    s.push_str(
        "\
A ROM is only loaded for a BOOTED disk (`cpm_boot_image`).  The CP/M
emulator has no console to place -- it services BDOS calls instead -- so
the setting is ignored there.

WHY YOU MIGHT WANT ONE
----------------------
DISK11.DSK in the Altair collection is a CP/M built for a Processor
Technology VDM-1 with the CUTER monitor at C000h.  It checks for it, and
without it prints

  This version of CP/M requires CUTER for VDM-1 to be present at C000h.

and then stops.  With the ROM in place it comes up on the VDM-1 screen,
which the gateway serves in a browser -- see the images folder readme for
how to reach it.
",
    );
    s
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

repodisks.txt beside this file lists what is on every disk in the
collections this gateway is known to run, so you can tell which one you
want before you go and find it.  Every disk is listed once by name, and
each line says whether that disk BOOTS or is MOUNT ONLY.  The disks
themselves are not ours to ship - that file says where each comes from.


MOUNTING IS NOT BOOTING
-----------------------

An Altair 88-DCDD floppy or an 88-HDSK hard disk can also be BOOTED, which
is a different thing.  Mounting gives you one drive of sixteen with the
gateway's CP/M underneath.  Booting hands the disk the whole machine, and
its OWN operating system runs — so booting reaches the disks that are not
CP/M at all: Altair DOS, Altair Disk Extended BASIC, Time Sharing BASIC,
Hard Disk BASIC, and CP/M 3.0.

Inside a booted disk there is no jail, no A> from us and no EGT8080.  Press
ESC twice to get back to the gateway.  A booted image is held by one session
at a time.  Disks are opened WRITABLE unless cpm_boot_writable is turned
off, and that one answer covers the mounted disks as well as the booted one
- the guest may write any of them, exactly as a machine with the
write-protect tabs off would.  Turn it off and writes are accepted and
discarded, which keeps every disk exactly as it is.  An image the host will
not let us write stays read-only whatever that setting says.

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
setting in every UI).  That is the only way to boot: a second, per-visit
boot picker lived on the telnet disks screen until 0.9.2 and was removed,
because two boots that asked different questions and remembered different
things was the most confusing thing here.  A disk is offered only if it
really cold-starts - see FORMATS YOU CAN BOOT below.

The mount screens follow it.  With a disk set to boot they offer only the
images on that disk's own board, and name the slots the way that board
does; under the emulator they offer everything and the slots are drives
A: to P:.  That is why a floppy can vanish from the list while a hard disk
is booting: the guest could never have read it.


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
being CP/M at all -- a UCSD p-System disk is one.
Reading such an image is safe and often what you want; writing to it is not.

Renaming with a prefix is therefore an override, not a requirement: it says
\"I know what this is\" and skips the inspection.


HOW TO MOUNT ONE
----------------

Mounting puts the image on one of the sixteen CP/M drives.  You keep the
gateway's own CP/M, its A> prompt and its terminals; the drive you mount on
reads and writes the filesystem inside the image instead of its folder.

  Telnet or SSH   C (Configuration), C (CP/M Settings), then I
                  (Mount/unmount disk images), then M (Mount an image).
                  Pick a drive letter, then a file.  N makes a blank
                  disk instead.
  Web UI          the \"AI, Browser, Weather & CP/M - More...\" panel, then
                  the \"Mount CP/M Drives\" button.
  Desktop         the \"Mount CP/M Drives...\" button, which opens a window
                  with a row per drive.

Then, inside CP/M, change to that drive the way you always would:

    B:
    DIR

Mounts are saved in cpm_mounts and survive a restart, and they take effect
in every session at once.  Unmount from the same screen.

Two things worth knowing before you choose a drive.  Mounting on A: hides
the bundled terminal, because EGT8080.COM lives in the A: folder - so B:
or later is usually what you want.  And a drive somebody is
using cannot have its disk changed; the screens show which are in use.


HOW TO BOOT ONE
---------------

Booting runs the disk's own operating system on the whole machine.  There
is no A> from us, no jail, no EGT8080 and no EXIT - press ESC twice to come
back to the gateway.

To make a disk what the CP/M menu item runs, set cpm_boot_image:

  Telnet or SSH   C (Configuration), C (CP/M Settings), B (Boot settings),
                  then R (Choose what CP/M runs) and pick it from the list.
                  The emulator is the first entry.
  Web UI          the \"CP/M runs:\" list in the CP/M panel.
  Desktop         the \"CP/M runs:\" list.

Leave that setting empty and the CP/M menu item runs the emulator, which
is the default.  Two more settings sit beside it and are worth leaving
alone unless something misbehaves: the machine (auto - the disk is asked)
and the processor.

A booted image is held by ONE session at a time, and one disk cannot be
booted and mounted at the same moment.  It opens WRITABLE unless
cpm_boot_writable is turned off (W on the telnet Boot settings screen, a
checkbox in the web and desktop UIs) - a booted OS expects to be able to
save, and discarding its writes loses the work silently.  Whatever you have
mounted comes along for the ride, each disk at the controller slot its
drive letter names.


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
collections below are maintained by other people, under their own terms.

THE GATEWAY CAN FETCH MOST OF THEM FOR YOU, so the rest of this section is
only needed for the ones it does not cover.  Every disk screen offers to
download the sample disks:

  Telnet or SSH   the mount screen's  D  option
  Web UI          the \"Download sample disks\" button
  Desktop         the \"Download sample disks\" button

",
    );
    // The count, the size and the repositories are rendered from the manifest
    // the downloader actually reads, never typed out here -- the same rule as
    // the format table above, and for the same reason.  This section named no
    // download at all until 2026-08-21: the feature shipped, three screens grew
    // a button, and the file sitting next to the disks went on sending the
    // operator to GitHub by hand.  A sentence a human maintains beside a list
    // the code maintains is the half that rots.
    let all = super::fetch::catalogue();
    let megabytes = all.iter().map(|d| d.bytes).sum::<u64>() as f64 / (1024.0 * 1024.0);
    s.push_str(&format!(
        "\
That brings {} disks and about {:.0} MB into this folder: the ones this
gateway is known to run, fetched on your behalf from
",
        all.len(),
        megabytes
    ));
    for repo in super::fetch::source_repos() {
        s.push_str(&format!("    {repo}\n"));
    }
    s.push_str(
        "\
pinned to a commit and checked against a recorded SHA-256, so what arrives
is what was tested.  Disks carrying no boot program of their own are left
out.  Nothing already in this folder is overwritten - a disk you have
edited, or put here yourself under the same name, is kept.

It also brings the CP/M monitor ROMs, into the roms folder next door: one
of these disks prints through a monitor that was never on it and will not
run without one.  See that folder's readme.  A ROM arriving does not
switch anything on - `cpm_boot_rom` is still off until you choose it.

Those are the collections marked below.  z80pack is not part of the offer
and is still worth fetching by hand:

  z80pack - Udo Munk
    https://github.com/udo-munk/z80pack
    The IMSAI 8080 disk library is in  imsaisim/disks/library  and is the
    best starting point: about twenty 8\" single-density disks holding
    CP/M 1.3 through 3.0, MP/M, IMDOS, BASICs, comms tools and demos.
    All are 256,256 bytes; the CP/M ones are the ibm3740 layout this
    gateway reads, and the rest (UCSD p-System) boot but do not mount.

  Altair 8800 Simulator - David Hansel            (in the download)
    https://github.com/dhansel/Altair8800
    The  disks  folder holds the MITS Altair, Tarbell and Cromemco
    collections.  All of them boot, and all but the BASIC and DOS ones
    mount: the DISKnn floppies are altair8, TDISKnn are ibm3740, and
    the three CDISKnn are Cromemco - single density, and the two
    double-density formats added by measuring the disks themselves.

  Altair-Duino-Disks - J.P. McNeely               (in the download)
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
  refusal was right.  A UCSD p-System disk is exactly that trap: it is
  256,256 bytes, so it looks like an ibm3740 disk, and it holds no CP/M
  filesystem at all.  Naming it ibm3740_ would let the gateway write
  CP/M structures over a filesystem it cannot read.  It boots perfectly
  as it stands.

Disks that are not CP/M can only be booted, and that is correct rather
than a fault — mounting one shows no files because there is no CP/M
directory in it to read.  In the collections above that means Altair
DOS, Altair Disk Extended BASIC, Time Sharing BASIC, Hard Disk BASIC,
the minidisks and the UCSD p-System disks.  Cromemco CDOS is NOT in
that list: CDOS keeps a CP/M-compatible filesystem, so those disks
mount and read like any other.

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
They do not need changing: the file is inspected either way.

A drive someone is currently using cannot have its disk changed.  The mount
screens show which drives are in use.

Mounting an image on drive A: hides the bundled terminal, because
EGT8080.COM lives in the A: folder.  Since transferring files in and out
of a mounted image is done with XMODEM from inside the terminal, you
usually want to mount images on B: or later and leave A: as it is.
(EGT8080 is built to the 8080's instruction set, so it works whichever
processor cpm_cpu selects.)
",
    );
    s
}

/// The first line of every `repodisks.txt` this project has generated.
///
/// Same job as [`README_HEADER`], and for the same reason: this file is a
/// *reference*, so a copy that has fallen behind the disks we support should be
/// refreshed rather than left to mislead. Anything that does not start with
/// this line is the operator's own and is never touched.
const REPODISKS_HEADER: &str = "WHAT IS ON THE DISKS\n====================";

/// The catalogue of the disk collections this gateway is known to run, and what
/// is on each disk.
///
/// Shipped in the binary rather than built at run time, because it is a
/// *reference to disks the operator may not have yet* — the whole point is to
/// be able to read it before going to find them. It is generated from the real
/// collections by the `#[ignore]` `record_repodisks` test, which mounts every
/// image and reads its actual directory, so nothing here is transcribed by
/// hand.
pub fn repo_disks() -> &'static str {
    include_str!("repodisks.txt")
}

/// Put `repodisks.txt` in the images folder, refreshing our own stale copy.
///
/// Failure is logged and ignored, exactly like the readme: a missing reference
/// must not stop the drives working.
fn write_repo_disks(images: &Path) {
    let path = images.join("repodisks.txt");
    let current = repo_disks();
    match std::fs::read_to_string(&path) {
        Ok(existing) if existing != current && existing.starts_with(REPODISKS_HEADER) => {
            if let Err(e) = std::fs::write(&path, current) {
                crate::glog!("CP/M: could not refresh {}: {}", path.display(), e);
            } else {
                crate::glog!("CP/M: refreshed {} for this version", path.display());
            }
        }
        Ok(_) => {}
        Err(_) => {
            if let Err(e) = std::fs::write(&path, current) {
                crate::glog!("CP/M: could not write {}: {}", path.display(), e);
            }
        }
    }
}

#[cfg(test)]
mod generate {
    //! Building `repodisks.txt` from the real collections.
    //!
    //! A generator rather than a hand-written list, for the reason every other
    //! table in this project is generated: a catalogue typed out by a person is
    //! wrong the first time a disk is added, and nobody notices because it
    //! still *looks* right. This mounts each image through the product's own
    //! identify-and-mount path and reads the directory the disk really has.

    use super::*;
    use crate::cpm::image::{fs::ImageFs, identify, media::FileMedia, media::Media};

    /// Where the collections live on the machine that generates this.
    ///
    /// Paths rather than a scan, because a *name* for each collection is the
    /// point — "z80pack cpmsim" tells a reader where to go and a directory
    /// name would not.
    /// Where each collection comes from, printed with it.
    ///
    /// The addresses matter more than the listings do: the disks are not ours
    /// to ship, so a catalogue of software the reader cannot go and get is a
    /// tease.  Read from the checkouts themselves rather than from memory —
    /// `git remote` for the Altair one, the project's own README for z80pack.
    /// The fourth field is which images to list: `&[]` means the whole folder.
    ///
    /// It exists for one collection. `jpmcneely/AltairDuino-Disks` holds 36
    /// images of which 26 are byte-identical to Hansel's, so listing the folder
    /// would repeat a quarter of this file to say nothing new. Named disks
    /// instead — the ones that collection uniquely has, which are exactly the
    /// ones the downloader takes from it.
    const REPOS: &[(&str, &str, &str, &str, &[&str])] = &[
        (
            "hansel",
            "Altair-Duino / Altair8800 simulator (David Hansel)",
            "AltairRepos/Altair8800/disks",
            "https://github.com/dhansel/Altair8800  (the disks/ folder)",
            &[],
        ),
        (
            "duino",
            "Altair-Duino disks (Jim McNeely) -- what only it has",
            "AltairRepos/AltairDuino-Disks/original",
            "https://github.com/jpmcneely/AltairDuino-Disks  (the original/ folder)",
            // Not DISK17.DSK: a name this collection alone has, whose bytes are
            // Hansel's DISK12.DSK exactly.  Listing it would catalogue one disk
            // twice under two numbers.
            &["HDSK04.DSK"],
        ),
        (
            "duino-extra",
            "Altair-Duino disks (Jim McNeely) -- the extra/ folder",
            "AltairRepos/AltairDuino-Disks/extra",
            "https://github.com/jpmcneely/AltairDuino-Disks  (the extra/ folder)",
            &[],
        ),
        ("altairsim", "z80pack — altairsim library", "z80pack/altairsim/disks/library", Z80PACK, &[]),
        ("cpmsim", "z80pack — cpmsim library", "z80pack/cpmsim/disks/library", Z80PACK, &[]),
        ("cromemcosim", "z80pack — cromemcosim library", "z80pack/cromemcosim/disks/library", Z80PACK, &[]),
        ("imsaisim", "z80pack — imsaisim library", "z80pack/imsaisim/disks/library", Z80PACK, &[]),
        // Deliberately NOT z80pack's intelmdssim library. The Intel MDS is not
        // a machine this gateway emulates, and it shows: four of its seven
        // disks are CP/M that our format table cannot read. Listing a
        // collection we cannot open would make this file a catalogue of
        // disappointments rather than of what works.
    ];

    /// One address for all four of its libraries.
    const Z80PACK: &str = "https://github.com/udo-munk/z80pack  (<sim>/disks/library)";

    /// Every file on one image, as the disk's own directory has it.
    ///
    /// `None` when the image has no CP/M filesystem we can read — a disk that
    /// boots its own operating system (Altair DOS, Disk BASIC) keeps its files
    /// in a layout that is that system's business, not CP/M's. Saying so is
    /// more use than an empty list, which reads as "an empty disk".
    fn files_on(path: &std::path::Path) -> Option<Vec<String>> {
        let size = std::fs::metadata(path).ok()?.len();
        let filename = path.file_name()?.to_string_lossy().to_string();
        let mut probe = FileMedia::open(path, true).ok()?;
        let ident = identify::identify(&filename, size, |fmt| {
            let mut dir = Vec::with_capacity(fmt.maxdir as usize * 32);
            for rec in 0..fmt.dir_records() {
                let off = fmt.data_record_offset(rec)?;
                let mut buf = [0u8; 128];
                Media::read_at(&mut probe, off, &mut buf).ok()?;
                dir.extend_from_slice(&buf);
            }
            (!dir.is_empty()).then_some(dir)
        })
        .ok()?;
        drop(probe);
        let media = FileMedia::open(path, true).ok()?;
        let fs = ImageFs::mount(Box::new(media), ident.format, true).ok()?;

        // Every user area, not just 0: a disk that keeps its tools on user 1
        // would otherwise look half empty. The area is named only when it is
        // not the ordinary one, so the common case stays uncluttered.
        let mut names: Vec<String> = Vec::new();
        for user in 0..16u8 {
            // `????????.???` — the same wildcard `DIR` builds, through the
            // same matcher, so this listing cannot disagree with what the
            // operator sees on the drive.
            let mut raw = [0u8; 36];
            raw[1..12].fill(b'?');
            let fcb = crate::cpm::fcb::Fcb::from_bytes(&raw);
            for (name, ext) in fs.matching(user, &fcb) {
                let n = String::from_utf8_lossy(&name).trim_end().to_string();
                let e = String::from_utf8_lossy(&ext).trim_end().to_string();
                let full = if e.is_empty() { n } else { format!("{n}.{e}") };
                names.push(if user == 0 { full } else { format!("{full}  (user {user})") });
            }
        }
        Some(names)
    }

    /// What a disk *is*, worked out from the files that are on it.
    ///
    /// The catalogue used to print a name and a directory listing and nothing
    /// else, so finding the disk you wanted meant reading 98 listings. This is
    /// the one line that saves that -- and it is derived from the disk's own
    /// directory, never from a catalogue or from the filename, so it cannot
    /// claim something the disk does not carry.
    ///
    /// Two rules keep it from stating plausible falsehoods, and both were
    /// written after a first version stated them:
    ///
    /// * **A system is claimed only on the files that ARE the system.**
    ///   `CDOSCPM.COM` is a CDOS-to-CP/M converter that lives on *CP/M* disks;
    ///   matching it labelled four CP/M disks "Cromemco CDOS". The system
    ///   markers are the kernel and its generator -- `CDOS.COM` **and**
    ///   `CDOSGEN.COM`, `CPM3.SYS`, `MPM.SYS` -- not a utility that mentions
    ///   one.
    /// * **A passenger is a proportion, not a count.** `hd-tools.dsk` carries
    ///   345 files including all six Zork files, and calling it "Infocom
    ///   adventures" would describe a tools disk by its smallest corner. A flat
    ///   "needs two matches" does **not** stop that -- six clears two easily,
    ///   and the first version of this rule printed the very label its own
    ///   comment said it prevented. What separates a subject from a passenger
    ///   is the share of the disk it occupies: measured over all 94 disks, the
    ///   three labels this rejects on `hd-tools` are its lowest shares (games
    ///   0.6%, Infocom 1.8%, assembler 2.4%) and every label worth keeping sits
    ///   at 3.8% or above, so a theme must be **two matches and a
    ///   thirty-second of the disk**. Nothing at 2% of 345 files is what a disk
    ///   *is*; that disk gets the honest fallback instead.
    ///
    ///   A *defining* file is exempt from both tests -- one `COMAL.COM` or
    ///   `TURBO.COM` is what that disk is for -- and those are ranked first for
    ///   the same reason.
    ///
    /// Deliberately card-neutral: `KSCOPE.COM` and `MICRO80.COM` are graphics
    /// demos on both an Altair and a Cromemco, but the card they drive is a
    /// VDM-1 on one and a Dazzler on the other, and the disk does not say
    /// which. "graphics demos" is what the directory supports.
    ///
    /// `None` for a disk with no CP/M filesystem: there are no contents to
    /// derive from, and the entry says it boots its own system instead.
    fn describe_contents(files: &[String]) -> Option<String> {
        if files.iter().any(|f| f.starts_with("(no CP/M")) {
            return None;
        }
        // The user-area suffix is ours, not the disk's -- match the real name.
        let names: std::collections::BTreeSet<&str> =
            files.iter().map(|f| f.split("  (user ").next().unwrap_or(f)).collect();
        let n = names.len();
        let has = |m: &[&str]| m.iter().filter(|f| names.contains(**f)).count();

        // (markers, how many must be present, label)
        const SYSTEM: &[(&[&str], usize, &str)] = &[
            (&["CPM3.SYS", "CPMLDR.COM"], 1, "CP/M 3.0"),
            (&["MPM.SYS", "MPMLDR.COM"], 1, "MP/M"),
            (&["SYSTEM.PASCAL"], 1, "UCSD p-System"),
            (&["CDOS.COM", "CDOSGEN.COM"], 2, "Cromemco CDOS"),
            (&["BDOS.SYS", "BIOS.SYS"], 2, "IMDOS (IMSAI CP/M)"),
            (&["CPM62.SYS"], 1, "CP/M 2.2 (62K)"),
            (&["MOVCPM.COM", "SYSGEN.COM", "CPM64.SYS", "CPM.COM"], 1, "CP/M 2.2"),
        ];
        // (markers, defining, label)
        //
        // **The labels are short because they are read on one line.** A summary
        // is `marker -- system, theme, theme, N files` and must fit 80 columns
        // beside a readme that is held to 80 -- and it is the LABEL that pays,
        // because a system name and a file count cannot be shortened without
        // becoming wrong. `terminal and file-transfer tools` (32 characters)
        // alone put four entries over, and the worst line at 96. Two of these
        // got more accurate in the process: `comms tools` covers both the
        // terminals and the transfer programs the markers actually list, and
        // `development tools` covers the debuggers -- `SID`/`ZSID` are not
        // linkers, which the old label implied.
        const THEMES: &[(&[&str], bool, &str)] = &[
            (&["FELIX.COM"], true, "the Felix system"),
            (&["COMAL.COM"], true, "COMAL"),
            (&["TURBO.COM"], true, "Turbo Pascal"),
            (&["DBASE.COM", "DBASE.OVL"], true, "dBASE"),
            (&["WS.COM", "SPELSTAR.OVR", "SPELSTAR.DCT", "MAILMRGE.OVR", "WSOVLY1.OVR"], true, "WordStar"),
            (&["SUPERCLC.COM", "BUDGET.CAL", "BRKEVN.CAL", "ARCS-DEP.CAL"], true, "SuperCalc worksheets"),
            (&["ADEXER.COM"], true, "the Altair hard-disk exerciser"),
            (&["MBASIC.COM", "BASIC.COM", "BASIC5.COM", "BASCOM.COM", "XYCPM95.COM", "VBASIC.COM",
               "BASIC-E.COM", "CBASIC.COM"], true, "BASIC"),
            (&["ZORK1.DAT", "ZORK2.DAT", "ZORK3.DAT", "ZORK1.COM", "ZORK2.COM", "ZORK3.COM"], false,
             "Infocom adventures"),
            (&["KSCOPE.COM", "MICRO80.COM", "SKETCH.COM", "COLOR.COM", "BARPLOT.COM", "DAZCHESS.COM",
               "DAZZLE.COM", "DAZZPLOT.COM", "GDEMO.COM", "GRAPHX.COM", "BOUNCE.COM", "LIFE.COM"], false,
             "graphics demos"),
            (&["8080EXM.COM", "8080PRE.COM", "EX8080.COM", "EXZ80DOC.COM", "PRELIM.COM", "CPUTEST.COM",
               "ZEXALL.COM", "ZEXDOC.COM"], false, "CPU exercisers"),
            (&["KERMIT.COM", "KERMIT3.COM", "QTERM.COM", "QT-IMSAI.COM", "PCGET.COM", "PCPUT.COM",
               "MODEM.COM", "XMODEM.COM"], false, "comms tools"),
            (&["M80.COM", "L80.COM", "MAC.COM", "RMAC.COM", "CREF80.COM", "LINK.COM", "ZSID.COM",
               "SID.COM"], false, "development tools"),
            (&["CHESS.COM", "CHASE.COM", "CATCHUM.COM", "DEFLECT.COM", "ALIENS.COM", "PACMAN.COM",
               "DOGFIGHT.COM", "4DTICTAC.COM", "AMBUSH.COM", "CHECKERS.COM", "MYCHESSN.COM",
               "CRAWL.COM", "LADDER.COM", "ADVENT.COM", "STARTREK.COM", "OTHELLO.COM"], false, "games"),
        ];
        const SOURCE_EXT: &[&str] =
            &["ASM", "MAC", "Z80", "PRN", "REL", "LIB", "FOR", "PAS", "SRC"];
        /// A non-defining theme must be at least one part in this many of the
        /// disk. Measured, not chosen: see the passenger rule above.
        const THEME_SHARE: usize = 32;

        let system = SYSTEM.iter().find(|(m, need, _)| has(m) >= *need).map(|(_, _, l)| *l);

        let mut themes: Vec<(bool, usize, &str)> = THEMES
            .iter()
            .filter_map(|(m, defining, label)| {
                let hits = has(m);
                // A small disk is exempt from the share test as well as the
                // count: two files out of six IS what that disk is, and the
                // proportion says so anyway -- but a dozen-file disk with one
                // match would fail a share test that a reader would not.
                let small = n <= 12;
                let enough = hits >= 2 && hits * THEME_SHARE >= n;
                (hits > 0 && (*defining || small || enough)).then_some((*defining, hits, *label))
            })
            .collect();
        // Defining first, then by weight of evidence.  A stable sort keeps the
        // table's own order for ties, so the list above is the tie-break and
        // the output cannot depend on hash order.
        themes.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
        themes.truncate(2);

        let source = names
            .iter()
            .filter(|f| {
                f.rsplit_once('.').is_some_and(|(_, e)| SOURCE_EXT.contains(&e))
            })
            .count();

        let mut parts: Vec<String> = Vec::new();
        // The system it carries, named and nothing more. It used to read
        // "{sys} system disk" when no theme followed it, and the marker on the
        // line now says whether it boots -- which showed that suffix up:
        // `cpm22-2.dsk` carries CPM64.SYS with the BIOS and BOOT sources beside
        // it and does NOT boot, being a disk for *building* a system. Naming
        // the system is the fact; calling it a system disk was an inference on
        // top of it, and the CDOSCPM.COM category error one step along.
        if let Some(sys) = system {
            parts.push(sys.to_string());
        }
        parts.extend(themes.iter().map(|(_, _, l)| l.to_string()));
        if parts.is_empty() {
            parts.push(
                if source >= 3.max(n / 2) { "source and listings" } else { "assorted CP/M files" }
                    .to_string(),
            );
        } else if source >= 6.max(n * 2 / 3) {
            parts.push("with source".to_string());
        }
        Some(parts.join(", "))
    }

    /// One catalogue row, before it is rendered.
    ///
    /// Hoisted out of `record_repodisks` so the marker logic can be tested
    /// without the disks: the generator needs real images and so can never
    /// run in CI, but what it *claims* takes no disk at all. Exactly the
    /// reasoning that put `describe_contents` under test.
    struct Entry {
        disk: String,
        tag: &'static str,
        order: usize,
        files: Option<Vec<String>>,
        /// Whether a boot list would offer this disk, from the product's
        /// own [`crate::cpm::boot::image_can_boot`] rather than a second
        /// opinion — so the catalogue and the picker cannot disagree.
        ///
        /// That function is false for exactly one reason,
        /// `Bootability::NoBootProgram`, which is the machine-independent
        /// one: "cannot boot on any machine in any configuration". A disk
        /// this machine merely has no *board* for still counts as bootable
        /// here, which is what makes the answer safe to ship in a file.
        boots: bool,
    }


    /// What one entry says about itself, wherever it is printed.
    ///
    /// **The marker comes first because it is the decision.** Booting and
    /// mounting are different things a disk is *for*, and which of them a
    /// disk can do is the fact a reader is choosing on — more so than what
    /// is on it. It leads the line so it can be scanned down in a
    /// plain-text file with no other formatting.
    ///
    /// Three states, from two independent readings of the same file: the
    /// boot marker is sector 0, the directory is the CP/M filesystem, and
    /// neither is derived from the other. `neither` is not a hedge — a disk
    /// with no boot program *and* no CP/M directory is one this gateway can
    /// do nothing with, and saying so is more use than describing it twice
    /// over as an absence.
    ///
    /// Naming these was the point of the exercise. The old text said
    /// "boots its own operating system" for any disk with no CP/M
    /// filesystem, which flatly claimed a boot for the six that cannot:
    /// `DISK0B`, `DISK0D`, `DISK0F` and the three `ucsd-*` second disks.
    /// Two independent facts had been collapsed into one sentence that only
    /// happened to be right for the disks anyone had tried.
    fn summary(e: &Entry) -> String {
        let what = match &e.files {
            None if e.boots => "its own operating system, no CP/M filesystem".to_string(),
            // The legend says what `neither` means; this adds what it IS
            // rather than repeating the two absences back.
            None => "data in another system's format".to_string(),
            Some(f) if f.is_empty() => "an empty CP/M directory".to_string(),
            Some(f) => {
                let what = describe_contents(f).unwrap_or_else(|| "assorted CP/M files".into());
                let n = f.len();
                format!("{what}, {n} file{}", if n == 1 { "" } else { "s" })
            }
        };
        let mark = match (e.boots, e.files.is_some()) {
            (true, _) => "boots",
            (false, true) => "mount only",
            (false, false) => "neither",
        };
        format!("{mark} -- {what}")
    }

    /// Regenerate `src/cpm/repodisks.txt` from the collections above.
    ///
    /// Ignored: it needs the disks. Set `REPODISKS_HOME` to the folder holding
    /// them (default `$HOME`), and `REPODISKS_OUT` to write somewhere else.
    ///
    /// Two-step opt-in, mirroring `record_punter_fixture`: `#[ignore]` keeps it
    /// off the default pass, and `REPODISKS_RECORD=1` keeps it off bulk
    /// `--ignored` runs.  That second step is not belt-and-braces --
    /// `tools/cpm-live-gates` *is* such a run and is handed **one** collection's
    /// folder, so this test rewrote the committed catalogue down to whichever
    /// collection happened to be on disk.  It cost 331 lines (the whole
    /// Altair-Duino listing) on 2026-08-16; `git status` caught it, and a run
    /// followed by an unexamined `git add -A` would not have.
    ///
    ///     REPODISKS_RECORD=1 REPODISKS_HOME=~/disks \
    ///       cargo test --release record_repodisks -- --ignored --nocapture
    #[test]
    #[ignore]
    fn record_repodisks() {
        let home = std::env::var("REPODISKS_HOME")
            .or_else(|_| std::env::var("HOME"))
            .expect("a home folder");
        let out = std::env::var("REPODISKS_OUT")
            .unwrap_or_else(|_| "src/cpm/repodisks.txt".into());
        if std::env::var("REPODISKS_RECORD").ok().as_deref() != Some("1") {
            eprintln!("REPODISKS_RECORD=1 not set; skipping (this test REWRITES {out})");
            return;
        }

        // Everything first, then one sorted pass: the catalogue is ordered by
        // disk name across all the collections, not grouped by collection.
        // Grouping was how it grew and it made the file unusable for its one
        // purpose -- you cannot look a disk up by name unless you already know
        // whose collection it is in, which is the thing you came to find out.
        let mut entries: Vec<Entry> = Vec::new();
        let mut present: Vec<&str> = Vec::new();

        for (order, (tag, name, rel, _from, only)) in REPOS.iter().enumerate() {
            let dir = std::path::Path::new(&home).join(rel);
            if !dir.is_dir() {
                eprintln!("skipping {name}: no {}", dir.display());
                continue;
            }
            let mut images: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
                .expect("readable")
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| {
                    p.extension().map(|e| e.eq_ignore_ascii_case("dsk")).unwrap_or(false)
                })
                .filter(|p| {
                    only.is_empty()
                        || p.file_name()
                            .map(|n| only.contains(&n.to_string_lossy().as_ref()))
                            .unwrap_or(false)
                })
                .collect();
            images.sort();
            if images.is_empty() {
                continue;
            }
            present.push(name);
            for image in images {
                let disk = image.file_name().unwrap().to_string_lossy().to_string();
                let boots = crate::cpm::boot::image_can_boot(&image);
                entries.push(Entry { disk, tag, order, files: files_on(&image), boots });
            }
        }
        let found = entries.len();
        assert!(found > 0, "no disks found under {home} — set REPODISKS_HOME");

        // Case-insensitively, because `CDISK01.DSK` and `cpm13.dsk` are one
        // list to a reader and two to a byte comparison -- ASCII order would
        // put every upper-case name first and hide half the catalogue below
        // the other half.  Ties go to the order the collections are declared
        // in, so the eight names that exist in more than one collection
        // (`cpm22.dsk` is in three) always land the same way round.
        entries.sort_by(|a, b| {
            a.disk.to_ascii_lowercase().cmp(&b.disk.to_ascii_lowercase()).then(a.order.cmp(&b.order))
        });

        let mut s = String::new();
        s.push_str(REPODISKS_HEADER);
        s.push_str(
            "\n\nWhat is on each disk image the gateway is known to run, so you can\n\
             tell which one you want before going to find it.  These are not\n\
             shipped with the gateway -- they are other people's collections, and\n\
             the readme in this folder says how to put one here.\n\n\
             Every disk is listed once, by name, A to Z, whichever collection it\n\
             came from -- the index below, then the same disks in full.  Eight\n\
             names exist in more than one collection and are different disks, so\n\
             each entry names the collection it came from.\n\n\
             Each listing is the disk's OWN directory, read through the same\n\
             mount path the gateway uses, and the line under each name is worked\n\
             out from those files -- not from the filename and not from anyone's\n\
             catalogue.\n\n\
             Every line begins with what the disk can DO, which is the choice\n\
             you are making.  Booting and mounting are different things:\n\n\
             \x20 boots        it carries a boot program -- set it as the boot\n\
             \x20              disk and its own operating system takes the\n\
             \x20              machine.  It can also be mounted.\n\
             \x20 mount only   no boot program, but it has a CP/M filesystem --\n\
             \x20              mount it on a drive letter.  A disk of programs\n\
             \x20              FOR another disk is one to mount, not to boot.\n\
             \x20 neither      no boot program and no CP/M filesystem: data in\n\
             \x20              some other system's format, which this gateway\n\
             \x20              can do nothing with.\n\n\
             Those are two separate readings of the same file -- the boot marker\n\
             is sector 0, the file list is the CP/M directory -- so neither is\n\
             guessed from the other.\n",
        );

        s.push_str("\n\n\nWHERE THEY COME FROM\n--------------------\n");
        for (tag, name, _rel, from, _only) in REPOS {
            if !present.contains(name) {
                continue;
            }
            s.push_str(&format!("\n>>>>> {tag} -- {name}\n"));
            s.push_str(&format!("      {from}\n"));
        }

        s.push_str("\n\n\nTHE DISKS, A TO Z\n-----------------\n");
        for e in &entries {
            s.push_str(&format!("\n  {}  ({})\n      {}\n", e.disk, e.tag, summary(e)));
        }

        s.push_str("\n\n\nEVERY DISK IN FULL\n------------------\n");
        for e in &entries {
            // `({tag})  {summary}` and not `{tag} -- {summary}`: the summary
            // already leads with a `-- `-separated marker, so the old form put
            // three of them on one line -- and it made the summary text differ
            // between the index and here, when it is one `summary` call.
            s.push_str(&format!("\n>> {}\n   ({})  {}\n", e.disk, e.tag, summary(e)));
            match &e.files {
                Some(names) if !names.is_empty() => {
                    for n in names {
                        s.push_str(n);
                        s.push('\n');
                    }
                }
                Some(_) => s.push_str("(the CP/M directory is empty)\n"),
                None => s.push_str("(no CP/M filesystem -- this disk boots its own system)\n"),
            }
            s.push('\n');
        }

        // A collection already in the catalogue must still be in it.  The loop
        // above *skips* a collection whose folder is absent, so regenerating
        // with only some of them on disk silently drops the rest -- which is a
        // truncation, not a regeneration, and the file is compiled in with
        // `include_str!`, so it ships.  `found > 0` above does not catch this:
        // one collection is enough to satisfy it.
        if let Ok(old) = std::fs::read_to_string(&out) {
            let lost: Vec<&str> = REPOS
                .iter()
                .map(|(_tag, name, ..)| *name)
                .filter(|name| {
                    let heading = format!("-- {name}");
                    old.contains(name) && !s.contains(&heading)
                })
                .collect();
            assert!(
                lost.is_empty(),
                "refusing to rewrite {out}: it already lists {lost:?}, and this run \
                 found no disks for them under {home}.  Point REPODISKS_HOME at a \
                 folder holding every collection, or the catalogue loses them.",
            );
        }
        std::fs::write(&out, &s).expect("write");
        eprintln!("wrote {out}: {found} disks, {} bytes", s.len());
    }

    /// `describe_contents` runs only when the catalogue is regenerated, which
    /// needs the disks and so cannot happen in CI -- and it is the one part of
    /// the generator that makes a *claim* rather than copying a directory
    /// listing. So its rules are tested here from file lists alone, where the
    /// real disks are not needed. Both rules below were written after a first
    /// version stated the falsehood each one now prevents.
    #[cfg(test)]
    mod tests {
        use super::describe_contents;

        fn files(names: &[&str]) -> Vec<String> {
            names.iter().map(|n| n.to_string()).collect()
        }

        /// Padding to a size, so a rule about proportion can be tested at one.
        fn filler(n: usize) -> Vec<String> {
            (0..n).map(|i| format!("FILL{i:04}.TXT")).collect()
        }

        fn describe(names: &[&str], pad: usize) -> String {
            let mut f = files(names);
            f.extend(filler(pad));
            describe_contents(&f).expect("a CP/M directory describes itself")
        }

        /// **The marker is the decision, and it must be two readings not one.**
        /// The text this replaced said "boots its own operating system" for any
        /// disk with no CP/M filesystem, which claimed a boot for the eight that
        /// cannot -- `DISK0B`, `DISK0D`, `DISK0F` and the five `ucsd-*` second
        /// disks. Sector 0 and the CP/M directory are independent, so all four
        /// combinations are real and each gets its own answer here.
        #[test]
        fn test_every_entry_says_what_the_disk_can_do() {
            let e = |files: Option<Vec<String>>, boots: bool| super::Entry {
                disk: "x.dsk".into(),
                tag: "t",
                order: 0,
                files,
                boots,
            };
            let some = |names: &[&str]| Some(files(names));

            // Boots, and its filesystem is its own business.
            let s = super::summary(&e(None, true));
            assert!(s.starts_with("boots -- "), "{s}");
            assert!(s.contains("no CP/M filesystem"), "{s}");

            // The case that was wrong: no boot program AND no CP/M directory.
            let s = super::summary(&e(None, false));
            assert!(s.starts_with("neither -- "), "{s}");
            assert!(
                !s.contains("boots its own"),
                "a disk that cannot boot must not be said to boot: {s}",
            );

            // A CP/M directory and no boot program: the mount-only case, which
            // is what most of these disks are for.
            let s = super::summary(&e(some(&["CHESS.COM", "CHASE.COM"]), false));
            assert!(s.starts_with("mount only -- "), "{s}");
            assert!(s.contains("2 files"), "and it still says what is on it: {s}");

            // Boots AND has a directory -- the ordinary system disk.
            let s = super::summary(&e(some(&["MOVCPM.COM"]), true));
            assert!(s.starts_with("boots -- "), "{s}");
            assert!(s.contains("CP/M 2.2"), "{s}");
            assert!(s.contains("1 file") && !s.contains("1 files"), "singular: {s}");

            // An empty directory is not the same as no directory.
            let s = super::summary(&e(Some(Vec::new()), false));
            assert!(s.starts_with("mount only -- "), "{s}");
            assert!(s.contains("empty CP/M directory"), "{s}");
        }

        /// A system disk that does not boot is a real disk, not a contradiction
        /// -- `cpm22-2.dsk` carries `CPM64.SYS` beside the BIOS and BOOT
        /// sources and is for *building* a system. So the summary may name the
        /// system, and must not call the disk a system disk.
        #[test]
        fn test_carrying_a_system_is_not_the_same_as_booting_one() {
            let e = super::Entry {
                disk: "cpm22-2.dsk".into(),
                tag: "cpmsim",
                order: 0,
                files: Some(files(&["CPM64.SYS", "BIOS.Z80", "BOOT.Z80", "SYSGEN.SUB"])),
                boots: false,
            };
            let s = super::summary(&e);
            assert!(s.starts_with("mount only -- "), "{s}");
            assert!(s.contains("CP/M 2.2"), "the system it carries is a fact: {s}");
            assert!(!s.contains("system disk"), "but calling it one is an inference: {s}");
        }

        #[test]
        fn test_a_disk_with_no_filesystem_has_nothing_to_describe() {
            // The marker the generator itself writes, not a real file name.
            let f = files(&["(no CP/M filesystem -- this disk boots its own system)"]);
            assert_eq!(describe_contents(&f), None);
        }

        /// The measured case: `hd-tools.dsk`, 345 files, all six Zork files.
        /// A flat two-match rule prints "Infocom adventures" here, which is the
        /// defect this test exists to hold shut.
        #[test]
        fn test_zork_on_a_tools_disk_is_a_passenger_not_the_subject() {
            let zork = &["ZORK1.COM", "ZORK1.DAT", "ZORK2.COM", "ZORK2.DAT", "ZORK3.COM", "ZORK3.DAT"];
            let big = describe(zork, 339);
            assert!(
                !big.contains("Infocom"),
                "six Zork files out of 345 is 1.8% of the disk and must not name it: {big}",
            );
            // The positive control: the same six files ARE the subject of a
            // disk that is only those files.  Without this, the assertion above
            // passes just as well when the label has been deleted outright.
            let small = describe(zork, 0);
            assert!(small.contains("Infocom adventures"), "a Zork disk is a Zork disk: {small}");
        }

        /// Between those two sizes the rule is the share, so check it bites at
        /// the boundary rather than only at the extremes.
        #[test]
        fn test_a_theme_is_named_on_its_share_of_the_disk() {
            // 6 files, one part in 32 => named up to 192 files, not beyond.
            let zork = &["ZORK1.COM", "ZORK1.DAT", "ZORK2.COM", "ZORK2.DAT", "ZORK3.COM", "ZORK3.DAT"];
            assert!(describe(zork, 192 - 6).contains("Infocom"), "6 of 192 is the last size that qualifies");
            assert!(!describe(zork, 193 - 6).contains("Infocom"), "6 of 193 is a passenger");
        }

        /// A single *defining* file is exempt from both tests -- that is the
        /// whole point of the flag, and a proportional rule would otherwise
        /// bury it on any large disk.
        #[test]
        fn test_one_defining_file_names_a_disk_of_any_size() {
            assert!(describe(&["COMAL.COM"], 0).contains("COMAL"));
            assert!(describe(&["COMAL.COM"], 400).contains("COMAL"));
        }

        /// A theme with a single match on a big disk was never named, and must
        /// still not be: the share test is an addition to the count, not a
        /// replacement for it.
        #[test]
        fn test_one_match_still_does_not_name_a_large_disk() {
            let d = describe(&["CHESS.COM"], 40);
            assert!(!d.contains("games"), "one game among 41 files is not a games disk: {d}");
        }

        /// The measured case behind the other rule: `CDOSCPM.COM` is a
        /// CDOS-to-CP/M *converter* and lives on CP/M disks. Matching it
        /// labelled four CP/M disks "Cromemco CDOS".
        #[test]
        fn test_a_system_is_claimed_only_on_the_files_that_are_the_system() {
            let d = describe(&["CDOSCPM.COM", "MOVCPM.COM", "SYSGEN.COM"], 20);
            assert!(!d.contains("CDOS"), "a converter is not the system it converts: {d}");
            assert!(d.contains("CP/M 2.2"), "and the system that IS there is named: {d}");
            // Both markers together are the system.
            let real = describe(&["CDOS.COM", "CDOSGEN.COM"], 20);
            assert!(real.contains("Cromemco CDOS"), "the kernel and its generator are: {real}");
        }

        /// A disk that matches nothing still gets an honest line rather than an
        /// empty one -- and the source-heavy variant is a different answer.
        #[test]
        fn test_a_disk_matching_nothing_says_so_without_guessing() {
            assert_eq!(describe(&["IMP.COM", "MLOAD.COM"], 10), "assorted CP/M files");
            let src = describe(&["THING.ASM", "THING.PRN", "THING.REL", "OTHER.MAC"], 0);
            assert_eq!(src, "source and listings");
        }
    }
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

    /// The disk catalogue lands beside the readme, and follows the same rule:
    /// ours is refreshed when it falls behind, the operator's is never touched.
    /// It is a *reference*, so a stale copy misleads in exactly the way a stale
    /// readme does.
    #[test]
    fn test_the_disk_catalogue_is_written_and_kept_current() {
        let dir = temp("repodisks");
        let t = dir.to_string_lossy().to_string();
        ensure_cpm_tree(&t).unwrap();
        let path = cpm_dir(&t).join(IMAGES_DIR).join("repodisks.txt");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), repo_disks());

        let stale = format!("{REPODISKS_HEADER}\n\nan older catalogue.\n");
        std::fs::write(&path, &stale).unwrap();
        ensure_cpm_tree(&t).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), repo_disks(), "ours is refreshed");

        std::fs::write(&path, b"MY DISKS\n\nhands off.\n").unwrap();
        ensure_cpm_tree(&t).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "MY DISKS\n\nhands off.\n",
            "a file that is not one of ours must never be rewritten"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The catalogue's shape, because it is read by a person in a plain-text
    /// window and the markers are all the structure it has.
    #[test]
    fn test_the_disk_catalogue_has_the_shape_it_promises() {
        let s = repo_disks();
        assert!(s.starts_with(REPODISKS_HEADER), "the header is what marks it as ours");

        // The three sections the header promises, in the order it promises
        // them.  A `find` on each rather than a `contains`, because "the index
        // below, then the same disks in full" is a claim about order.
        //
        // **These patterns end in a newline, which is only safe here because of
        // another test.** A trailing `\n` does not match a CRLF file -- the
        // heading is followed by `\r` -- and two source-scanning tests failed on
        // Windows for exactly that reason. This one is reading `repodisks.txt`,
        // which `.gitattributes` pins to LF and
        // `test_the_catalogue_ships_with_unix_line_endings` proves is free of
        // `\r` on every platform. That guard is load-bearing for this test, not
        // merely tidy.
        let (where_from, a_to_z, in_full) = (
            s.find("\nWHERE THEY COME FROM\n").expect("a provenance section"),
            s.find("\nTHE DISKS, A TO Z\n").expect("an index"),
            s.find("\nEVERY DISK IN FULL\n").expect("the full listings"),
        );
        assert!(where_from < a_to_z && a_to_z < in_full, "the sections are in reading order");

        // Every collection says who it came from, on the line under its name.
        let repos: Vec<&str> = s.lines().filter(|l| l.starts_with(">>>>> ")).collect();
        assert!(repos.len() >= 4, "every collection we support: {repos:?}");
        for r in &repos {
            assert!(r.contains(" -- "), "{r} names its short tag and then itself");
            let after = s.split_once(&format!("{r}\n")).expect("a heading has a body").1;
            let url = after.lines().next().unwrap_or_default();
            assert!(url.contains("https://"), "{r} must say where to get it, got {url:?}");
        }

        // The index and the full listings are the same disks.  This is the
        // promise that a reader leans on -- looking a name up in the index and
        // finding nothing under it is the one failure that makes the file
        // useless -- and it is exactly what a partial regeneration breaks.
        let index: Vec<&str> = s[a_to_z..in_full]
            .lines()
            .filter(|l| l.starts_with("  ") && !l.starts_with("      ") && !l.trim().is_empty())
            .map(|l| l.trim())
            .collect();
        let full: Vec<&str> = s[in_full..].lines().filter_map(|l| l.strip_prefix(">> ")).collect();
        assert!(index.len() > 80, "one index row per image, got {}", index.len());
        assert_eq!(
            index.len(),
            full.len(),
            "the index and the listings must be the same disks, not {} and {}",
            index.len(),
            full.len(),
        );
        for (row, disk) in index.iter().zip(&full) {
            // The index row is "NAME  (tag)"; the listing heading is the name.
            let (name, tag) = row.split_once("  (").expect("an index row names its collection");
            assert_eq!(name, *disk, "the index and the listings must run in the same order");
            assert!(tag.ends_with(')') && tag.len() > 1, "{row} names a collection");
            assert!(name.to_ascii_lowercase().ends_with(".dsk"), "{name} is a disk image");
        }

        // A to Z, case-insensitively -- the whole reason the file was
        // reordered.  Byte order would put every upper-case name first and
        // hide half the catalogue below the other half.
        let keys: Vec<String> = full.iter().map(|d| d.to_ascii_lowercase()).collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted, "the disks must be in A-to-Z order");

        // Eight names exist in more than one collection, and the header says
        // so in prose -- so the prose is checked against the file rather than
        // left to rot.  `cpm22.dsk` is the one in three.
        let mut counts: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
        for d in &full {
            *counts.entry(*d).or_default() += 1;
        }
        let shared: Vec<(&&str, &usize)> = counts.iter().filter(|(_, n)| **n > 1).collect();
        assert_eq!(shared.len(), 8, "the header promises eight shared names: {shared:?}");
        assert!(
            s.contains("Eight\nnames exist in more than one collection"),
            "and the header says eight, in the words a reader sees",
        );
        assert_eq!(counts.get("cpm22.dsk"), Some(&3), "cpm22.dsk is the one in three");

        // Every disk says *something*: a summary line, then a listing or why
        // there is none.
        for block in s[in_full..].split("\n>> ").skip(1) {
            let (name, rest) = block.split_once('\n').expect("a disk block has a body");
            let summary = rest.lines().next().unwrap_or_default();
            assert!(summary.contains(" -- "), "{name} needs its one-line summary, got {summary:?}");
            assert!(!rest.trim().is_empty(), "{name} has an empty entry");
        }

        // **Every entry declares what the disk can DO, and the words are the
        // legend's.** Booting and mounting are different things, and the marker
        // is the field a reader chooses on -- so a disk that carried none, or
        // one whose marker the legend never explains, is the failure that
        // matters here. Checked in the index and the listings both, because the
        // two are rendered from one `summary` and a reader uses whichever is in
        // front of them.
        const MARKS: [&str; 3] = ["boots", "mount only", "neither"];
        for m in MARKS {
            assert!(s.contains(&format!("\n  {m}  ")) || s.contains(&format!(" {m}   ")),
                    "the legend must explain {m:?}");
        }
        let mut tally = std::collections::BTreeMap::new();
        for (where_, lines) in [("index", &s[a_to_z..in_full]), ("listings", &s[in_full..])] {
            let summaries: Vec<&str> = lines
                .lines()
                .filter(|l| l.contains(" -- ") && l.starts_with("   "))
                .collect();
            assert_eq!(
                summaries.len(),
                full.len(),
                "one summary per disk in the {where_}, got {}",
                summaries.len(),
            );
            for line in summaries {
                // The listings prefix the summary with `(tag)`; the index
                // does not. Strip it so the SAME summary is checked in both.
                let body = line.trim();
                let body = match body.strip_prefix('(') {
                    Some(r) => r.split_once(')').expect("a tag closes").1.trim(),
                    None => body,
                };
                let mark = MARKS.iter().find(|m| {
                    body.strip_prefix(**m).is_some_and(|r| r.starts_with(" -- "))
                });
                let mark = mark.unwrap_or_else(|| {
                    panic!("a {where_} summary must begin with a marker the legend explains: {body:?}")
                });
                *tally.entry((where_, *mark)).or_insert(0usize) += 1;
            }
        }
        // The index and the listings must AGREE about every disk, not merely
        // each be well-formed: they are one `summary` rendered twice, so a
        // disagreement means a disk is described two ways in one file.
        for m in MARKS {
            assert_eq!(
                tally.get(&("index", m)).copied().unwrap_or(0),
                tally.get(&("listings", m)).copied().unwrap_or(0),
                "the index and the listings disagree about how many disks are {m:?}",
            );
        }
        // A marker no disk carries would mean the legend documents a state the
        // generator cannot produce -- and one carried by every disk would mean
        // the distinction is not being drawn at all.
        for m in MARKS {
            let n = tally.get(&("index", m)).copied().unwrap_or(0);
            assert!(n > 0, "no disk is {m:?}, so the legend explains a state that cannot happen");
            assert!(n < full.len(), "every disk is {m:?}, so nothing is being distinguished");
        }

        // The marker is about sector 0 and the file list is about the CP/M
        // directory, so "boots" must not be a restatement of "has files". A
        // disk that boots and has no filesystem, and one that has files and
        // does not boot, both have to exist or the two readings have collapsed
        // into one -- which is exactly the defect this replaced.
        assert!(s.contains("boots -- its own operating system"), "a boot with no CP/M directory");
        assert!(s.contains("mount only -- "), "a CP/M directory with no boot program");

        // The collection we cannot read must not be advertised as one we run.
        assert!(!s.contains("intelmds"), "the Intel MDS is not a machine this gateway emulates");
    }

    /// **The shape is the product, so the line endings are too.** This file is
    /// compiled in with `include_str!`, and a Windows checkout that normalised
    /// its line feeds would turn every separator into something the shape test
    /// does not recognise — on one platform only. Pinned in `.gitattributes`;
    /// this fails loudly rather than letting the shape test fail obscurely.
    #[test]
    fn test_the_catalogue_ships_with_unix_line_endings() {
        assert!(
            !repo_disks().contains('\r'),
            "repodisks.txt has CRLF: the .gitattributes `text eol=lf` pin is missing or lost"
        );
    }

    /// **The catalogue fits 80 columns, like the readme beside it.**
    ///
    /// Both are plain text in one folder, read by one person in one editor, so
    /// holding one to 80 and not the other was an inconsistency rather than a
    /// decision. Adding the boot marker is what surfaced it: six lines went
    /// over, the worst at 96, and the fix was to shorten the two longest theme
    /// labels rather than to wrap a summary -- one line per disk is the whole
    /// point of the index.
    ///
    /// **The headroom is thin (79 of 80), and that is the useful part.** A new
    /// disk carrying a long system name and two long themes can exceed it, and
    /// this test is how that gets noticed at the moment the catalogue is
    /// regenerated rather than by a reader meeting a wrapped line. If it fires,
    /// shorten a label -- do not widen the limit, or the readme's rule and this
    /// one drift apart again.
    #[test]
    fn test_the_catalogue_lines_fit_eighty_columns() {
        let long: Vec<(usize, &str)> = repo_disks()
            .lines()
            .map(|l| (l.chars().count(), l))
            .filter(|(n, _)| *n > 80)
            .collect();
        assert!(long.is_empty(), "these lines do not fit 80 columns: {long:#?}");
    }

    /// A disk with no CP/M filesystem says so rather than showing an empty
    /// listing — "no files" and "a layout that is not CP/M's" look identical on
    /// the page and mean completely different things. A third of these images
    /// are Altair DOS, Disk BASIC, UCSD p-System, Cromix or ISIS.
    #[test]
    fn test_the_catalogue_distinguishes_no_files_from_no_filesystem() {
        let s = repo_disks();
        assert!(s.contains("no CP/M filesystem"), "the note must exist");
        // And every collection says where it came from.  A catalogue of
        // software the reader cannot go and get is a tease, and the images
        // readme promises this file carries the addresses.
        assert!(s.contains("https://github.com/dhansel/Altair8800"));
        assert!(s.contains("https://github.com/udo-munk/z80pack"));
        // And it is used, not merely defined.
        assert!(s.matches("no CP/M filesystem -- this disk boots its own system").count() > 10);
        // A disk that *does* have a filesystem lists real 8.3 names.
        assert!(s.contains("\nPIP.COM\n"), "a listing looks like a CP/M directory");
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

    /// **The readme's telnet route to the CP/M screen must be one route.**
    ///
    /// It gave two. The mounting section said `C (Configuration)`, then
    /// `O (Other Settings)`, then `E (CP/M settings)`; the boot section, forty
    /// lines later, said `C (Configuration), C (CP/M Settings)`. The second is
    /// what the menu does -- measured by driving it -- so the first sent an
    /// operator into Other Settings, where there is no CP/M entry at all and
    /// `E` is not a key. It had presumably been right once and outlived a menu
    /// reshuffle, which is exactly what a second copy of a route does.
    ///
    /// Pinned as *absence of the stale phrasing* plus *both sections naming the
    /// same two keys*, because the failure was disagreement rather than a wrong
    /// value: a test asserting only that the right path appears would have
    /// passed the whole time the wrong one sat above it.
    #[test]
    fn test_the_readme_gives_one_route_to_the_cpm_screen() {
        let text = images_readme();
        assert!(
            !text.contains("O (Other Settings)"),
            "the CP/M screen is reached with C from Configuration; Other Settings has no CP/M entry"
        );
        let route = "C (Configuration), C (CP/M Settings)";
        let hits = text.matches(route).count();
        assert!(
            hits >= 2,
            "both the mounting and the boot section must name the same route, found {hits}"
        );
    }

    /// **The readme must offer the download the gateway actually has.**
    ///
    /// Reported 2026-08-21. `WHERE TO GET IMAGES` sent the operator to GitHub to
    /// copy files in by hand, and had done since before the downloader existed:
    /// the feature shipped, all three disk screens grew a "Download sample
    /// disks" button, `web/cpmreference.html` gained a paragraph about it, and
    /// the one file sitting *in the images folder* — the first thing anybody
    /// looking at an empty folder reads — never mentioned it.
    ///
    /// The same shape as the format table two tests up, and the same fix: the
    /// count, the size and the repository list are rendered from
    /// [`super::super::fetch`], so the offer cannot describe a downloader other
    /// than the one that runs. What is asserted here is the part prose still
    /// owns — that the section names the offer at all, and names the surfaces
    /// it is on.
    #[test]
    fn test_the_roms_readme_lists_every_rom_the_gateway_offers() {
        let text = roms_readme();
        assert!(text.starts_with(ROMS_README_HEADER), "the header is the marker for refreshing it");

        // **Scoped to the section, not the file** — the images-readme test's own
        // lesson. "cpm_boot_rom" and the folder name appear in the prose above,
        // so a whole-file `contains` would pass with the table gutted.
        let table = text
            .split_once("WHAT IS ON OFFER")
            .map(|(_, rest)| rest.split_once("A ROM is only loaded").map_or(rest, |(t, _)| t))
            .expect("the offer must be a findable section");

        // Rendered from the catalogue rather than typed, so a ROM added to the
        // code cannot go missing from the file an operator reads.
        let mut listed = 0;
        for c in super::super::rom::ROM_CHOICES {
            let Some(f) = c.rom.as_ref() else { continue };
            listed += 1;
            assert!(table.contains(f.file), "{} is not in the table: {table:?}", f.file);
            assert!(table.contains(c.key), "{} is not named as a setting: {table:?}", c.key);
            assert!(
                table.contains(&format!("{:04X}-{:04X}", f.span.0, f.span.1)),
                "{}'s window is not stated: {table:?}",
                f.file
            );
        }
        assert!(listed > 0, "a readme offering nothing would make the folder a mystery");

        // The two rules an operator has to know before they put a file here, and
        // the reason the folder exists at all.  Checked against the text with
        // its whitespace flattened, because where a sentence happens to wrap is
        // not one of its claims — the first version of this test failed on
        // "boots into silence" purely because the line broke between the words.
        let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
        for claim in ["never overwritten", "fetched from its author", "boots into silence"] {
            assert!(flat.contains(claim), "the readme must say {claim:?}");
        }
        // Named by the setting, not by a path we might move.
        assert!(text.contains("cpm_boot_rom"), "the readme must name the setting");
    }

    #[test]
    fn test_readme_offers_the_download_the_gateway_has() {
        let text = images_readme();

        // The offer, and the three places it is made.  A reader on any one
        // surface must find their own.
        // **Scoped to the section, not to the file.** Each of these three
        // surface names appears two or three times in this readme already (the
        // mount instructions and the boot instructions both list all three), so
        // a whole-file `contains` passes with the download block gutted -- which
        // it did, when the first version of this test was mutation-checked by
        // replacing the Web UI line with "the mount panel". Cut the block out
        // and ask it directly.
        let block = text
            .split_once("download the sample disks:")
            .map(|(_, rest)| rest.split_once("\nThat brings").map(|(b, _)| b).unwrap_or(rest))
            .expect("the download offer must be a findable section");
        assert!(
            block.contains("Download sample disks"),
            "the offer must name the button by the label the screens really use: {block:?}"
        );
        for surface in ["Telnet or SSH", "Web UI", "Desktop"] {
            assert!(
                block.contains(surface),
                "the download offer must tell a {surface} reader where the button is: {block:?}"
            );
        }

        // Rendered from the manifest, so the numbers cannot drift from it.
        let all = super::super::fetch::catalogue();
        assert!(!all.is_empty(), "a manifest with no disks would make the offer a lie");
        assert!(
            text.contains(&format!("brings {} disks", all.len())),
            "the disk count must come from the catalogue ({})",
            all.len()
        );

        // Every repository the downloader really fetches from is named.  Adding
        // a source without documenting it fails here.
        for repo in super::super::fetch::source_repos() {
            assert!(text.contains(&repo), "the download does not name its source {repo}");
        }

        // z80pack is *not* in the offer, and the readme must not fold it in:
        // an operator told the button covers it would wait for disks that never
        // arrive.
        assert!(
            text.contains("z80pack is not part of the offer"),
            "the one collection the download does not cover must say so"
        );
        assert!(
            !super::super::fetch::source_repos().iter().any(|r| r.contains("z80pack")),
            "if z80pack ever joins the download, the sentence above is wrong"
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
