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
want before you go and find it.  The disks themselves are not ours to
ship - that file says where each collection comes from.


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

    cromemcodd_<what-it-holds>.dsk
    cromemcodsdd_<what-it-holds>.dsk
    z80packhd_<what-it-holds>.dsk
    ibm3740_<what-it-holds>.dsk
    altair8_<what-it-holds>.dsk
    altairhd_<what-it-holds>.dsk

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
"I know what this is" and skips the inspection.


HOW TO MOUNT ONE
----------------

Mounting puts the image on one of the sixteen CP/M drives.  You keep the
gateway's own CP/M, its A> prompt and its terminals; the drive you mount on
reads and writes the filesystem inside the image instead of its folder.

  Telnet or SSH   main menu C (Configuration), then O (Other Settings),
                  then E (CP/M settings), then I (Mount/unmount disk
                  images), then M (Mount an image).  Pick a drive
                  letter, then a file.  N makes a blank disk instead.
  Web UI          the "AI, Browser, Weather & CP/M - More..." panel, then
                  the "Mount CP/M Drives" button.
  Desktop         the "Mount CP/M Drives..." button, which opens a window
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
  Web UI          the "CP/M runs:" list in the CP/M panel.
  Desktop         the "CP/M runs:" list.

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

    cromemcodd Cromemco 8" SSDD, 508K (MICAH CP/M 2.2)
               625920 bytes (a short trailer is OK)

    cromemcodsdd Cromemco 8" DSDD, 1216K (ITC CP/M 2.2)
               1256704 bytes (a short trailer is OK)

    z80packhd  z80pack cpmsim hard disk, 4M
               4177920 bytes (a short trailer is OK)

    ibm3740    IBM 3740 8" SSSD, 241K (Tarbell, Cromemco SD)
               256256 bytes (a short trailer is OK)

    altair8    Altair 88-DCDD 8" SSSD, 300K (MITS)
               337568 bytes (a short trailer is OK)

    altairhd   Altair 88-HDSK hard disk, 4.8M (Altair-Duino)
               4988928 bytes (a short trailer is OK)

FORMATS YOU CAN BOOT
--------------------

The gateway does not read the filesystem of these at all — it runs the
disk, and the disk's own operating system does that work.  There is no
naming convention here and nothing to rename: an image is bootable if it is
the right size.  A few bytes of trailer past the last sector are allowed,
because several images in circulation have one.

    Altair 88-DCDD 8" floppy
               337568 bytes = 77 tracks x 32 sectors x 137
               plus up to 136 bytes of trailer

    Altair 88-MDS 5.25" minidisk
               76720 bytes = 35 tracks x 16 sectors x 137
               plus up to 136 bytes of trailer

    Altair 88-HDSK hard disk
               4988928 bytes = 406 cylinders x 2 heads x 24 sectors x 256
               plus up to 255 bytes of trailer

    Tarbell 1011 8" floppy
               256256 bytes = 77 tracks x 26 sectors x 128
               plus up to 127 bytes of trailer

    z80pack 8" SSSD, 241K
               256256 bytes = 77 tracks x 26 sectors x 128
               plus up to 0 bytes of trailer

    z80pack large disk, 4M
               4177920 bytes = 255 tracks x 128 sectors x 128
               plus up to 0 bytes of trailer

    Cromemco SSSD floppy
               256256 bytes = 77 cyl x 1 side x 26 sectors x 128
               plus up to 127 bytes of trailer

    Cromemco 8" SSDD floppy
               625920 bytes = 77 cyl x 1 side, track 0 26x128 then 16x512
               plus up to 127 bytes of trailer

    Cromemco 8" DSDD floppy
               1256704 bytes = 77 cyl x 2 sides, track 0 26x128 then 16x512
               plus up to 127 bytes of trailer

This is how the disks that are NOT CP/M run: Altair DOS, Altair Disk
Extended BASIC, Time Sharing BASIC and Hard Disk BASIC all boot, and so
does CP/M 3.0.  Mounting any of them shows nothing, which is correct — they
are not CP/M filesystems.  A programs disk (data, with no boot sector) is
refused.

Not all of these are the same disk, and a size that no board claims is
refused — so a truncated or badly padded file may be refused even though it
looks fine.  Three boards claim 256,256 bytes: it is a Tarbell 1011 floppy,
a z80pack 8" disk and a Cromemco single-density floppy, all raw 26-sector
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
    best starting point: about twenty 8" single-density disks holding
    CP/M 1.3 through 3.0, MP/M, IMDOS, BASICs, comms tools and demos.
    All are 256,256 bytes; the CP/M ones are the ibm3740 layout this
    gateway reads, and the rest (UCSD p-System) boot but do not mount.

  Altair 8800 Simulator - David Hansel
    https://github.com/dhansel/Altair8800
    The  disks  folder holds the MITS Altair, Tarbell and Cromemco
    collections.  All of them boot, and all but the BASIC and DOS ones
    mount: the DISKnn floppies are altair8, TDISKnn are ibm3740, and
    the three CDISKnn are Cromemco - single density, and the two
    double-density formats added by measuring the disks themselves.

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
