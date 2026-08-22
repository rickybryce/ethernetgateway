//! Running a booted disk image for a telnet or SSH session.
//!
//! The other emulator path — `cpm_emu` — traps BDOS calls and services them
//! against a filesystem we control. This one traps nothing. It boots the disk
//! and gets out of the way, and the disk's own operating system does the rest.
//! That is what makes it able to run Altair DOS, Disk BASIC and Time Sharing
//! BASIC, none of which are CP/M and none of which the filesystem path can
//! reach.
//!
//! The bounds are different in kind, so they are stated rather than inherited:
//!
//! * **One session per image.** A booted guest owns whole drives and writes
//!   raw sectors, so the per-file claim that keeps two CP/M sessions from
//!   interleaving records has nothing to grip. A second session is refused.
//! * **Read-only unless asked, and then everywhere.** The guest can write
//!   anywhere inside an image, and nothing above it interprets the format well
//!   enough to notice a mistake. Protection is the default; writing is a
//!   decision — one decision, covering every disk in the machine, because that
//!   is what a machine with the write-protect tabs off is. The mounted disks
//!   were once excluded from it on the strength of `Mount::read_only`, which
//!   is our BDOS's opinion of a format a booted guest never asks us about; see
//!   `plan_boot_disks`. What still refuses a write is the host refusing the
//!   file.
//! * **No instruction ceiling.** `cpm_emu_max_minstr` bounds one transient
//!   program in the emulator and hands the user back their `A>`. A booted
//!   operating system *is* the session, and running indefinitely at its own
//!   prompt is what it is supposed to do — at the default ceiling every booted
//!   disk would have stopped after about forty seconds. What needs bounding is
//!   a user who has walked away, so the session idle timeout does that instead.
//! * **The modem comes along, if it can.** A profile that is a pair of ports
//!   is wired up — `altair_2sio2` is where a real Altair put one. `AUX:` and
//!   HBIOS cannot: they are our BDOS and RomWBW's firmware, and this guest has
//!   its own of both.

use super::*;
use crate::cpm::boot_machine::{BootMachine, ModemAttach};
use crate::telnet::cpm_emu::{cpm_peer_register, idle_nap, poll_once};
use crate::telnet::cpm_modem::CpmModem;
use iz80::Cpu;

/// How a booted session names itself in the web UI's screen list.
///
/// Pure, so the naming can be tested without a session, a disk or a listener.
/// The image comes first because it is what the operator picked and — since one
/// image can only be booted once — it is the name that is actually unique;
/// where the typist is sitting follows, because the person choosing a screen in
/// the browser is not necessarily the person at the keyboard.
fn screen_label(
    image: &str,
    peer: Option<std::net::IpAddr>,
    is_serial: bool,
    is_ssh: bool,
    port: Option<crate::config::SerialPortId>,
) -> String {
    let from = if is_serial {
        match port {
            Some(crate::config::SerialPortId::A) => "serial port A".to_string(),
            Some(crate::config::SerialPortId::B) => "serial port B".to_string(),
            // A relay session is serial in behaviour and arrives over IP, so it
            // has a peer and no local port.  Naming the slave's address is the
            // most useful thing available.
            None => match peer {
                Some(ip) => format!("relay {ip}"),
                None => "serial".to_string(),
            },
        }
    } else {
        match (peer, is_ssh) {
            (Some(ip), true) => format!("SSH {ip}"),
            (Some(ip), false) => format!("telnet {ip}"),
            (None, true) => "SSH".to_string(),
            (None, false) => "telnet".to_string(),
        }
    };
    format!("{image} — {from}")
}

/// What to tell a session whose disk drives a VDM-1, before it goes quiet.
///
/// The advance warning half of the design: a guest painting a memory-mapped
/// screen writes to no console port at all, so without this line a VDM-1 disk
/// boots, takes keystrokes perfectly and looks broken. Only printed for a disk
/// that *declared* the card (`detect::image_drives_vdm`) — every booted session
/// offers its screen, but saying so on a disk that has nothing to show would
/// make the line noise and teach the operator to skip it.
///
/// Pure and returning lines rather than printing, so the widths can be checked:
/// the PETSCII terminals this gateway serves are 40 columns, and these are the
/// only lines here carrying a URL.
fn vdm_notice(web_enabled: bool, ip: &str, port: u16) -> Vec<String> {
    let mut lines = vec!["Screen: this disk paints a VDM-1".to_string()];
    if web_enabled {
        lines.push("(no port to print to). Watch it at".to_string());
        // On its own line because an address plus a sentence does not fit 40
        // columns, and a wrapped URL is one a person cannot type back in.
        lines.push(format!("http://{ip}:{port}/vdm"));
    } else {
        lines.push("(no port to print to). The web UI".to_string());
        lines.push("would show it, but it is off.".to_string());
    }
    lines
}

/// Claims an image for one session and releases it however the session ends.
///
/// RAII rather than a matched pair of calls: a boot can leave through an error,
/// a dropped connection or the shutdown broadcast, and an image left claimed
/// after any of those could never be booted again without a restart.
struct BootClaim(std::path::PathBuf);

impl Drop for BootClaim {
    fn drop(&mut self) {
        crate::cpm::image::registry::release_booted_image(&self.0);
    }
}

impl BootClaim {
    /// Claim an image, by identity rather than by spelling.
    ///
    /// The key is canonicalised for the same reason [`same_file`] exists: boot
    /// targets and mount paths are built from the same config value by
    /// different code, and only one of those routes canonicalises — so the same
    /// file arrives under a relative and an absolute name. Comparing the two
    /// raw would let one disk be claimed twice, put it in two machines at once,
    /// and have both write it back at teardown with the last one out silently
    /// discarding the other's work. That is precisely the "one session per
    /// image" rule this type exists to keep.
    fn take(path: &std::path::Path) -> Option<BootClaim> {
        // The key comes back from the claim and is kept verbatim.  Deriving it
        // again at release time would need the file to still exist, and an
        // image deleted mid-session would be released under a different name
        // than it was claimed under — leaking the claim for the life of the
        // process.
        crate::cpm::image::registry::claim_booted_image(path).map(BootClaim)
    }
}

/// One disk in the booted machine: where its bytes came from and whether the
/// guest may change them.
///
/// The path is carried per unit rather than assumed, because saving is the
/// dangerous end of this: the guest hands back "unit 3 is dirty" and nothing
/// else, so a machine that did not remember which file unit 3 came from would
/// have to guess — and the only cheap guess, "write it to the image we booted",
/// silently overwrites one disk with another.
struct BootDisk {
    path: std::path::PathBuf,
    writable: bool,
    /// Held for the life of the session; dropping it releases the image.
    _claim: BootClaim,
    /// The drive this came off, when it was a mount we took out of the
    /// registry — so it can be put back when the session ends.
    remount: Option<u8>,
}

/// How often to look for a keystroke, in instructions.
///
/// The guest polls its console far more often than a person types, so checking
/// every instruction would spend the whole budget on the input path. This is
/// frequent enough that typing feels immediate and rare enough to be free.
const KEY_POLL_INTERVAL: u64 = 20_000;

/// Instructions between yields to the runtime.
///
/// Without this the emulator loop starves every other task on the thread —
/// the same lesson the CP/M emulator learned when an idle guest spun the host
/// at 161% CPU.
const YIELD_INTERVAL: u64 = 200_000;

/// How often the speed governor is consulted, in instructions.
///
/// Much finer than [`KEY_POLL_INTERVAL`], and it has to be: at 2 MHz and the
/// measured 6.73 cycles an instruction, twenty thousand instructions is 67 ms of
/// virtual time, so checking there would let the guest run in seventh-of-a-second
/// bursts and the pacing would be visible as stutter. Two thousand is about
/// 6.7 ms at 2 MHz, which is the same order as `speed::SLACK`. It divides both
/// the key and yield intervals, so the seams still line up.
const SPEED_CHECK_INTERVAL: u64 = 2_000;

/// A speed check coarser than the key poll would pace in visible bursts.
///
/// At compile time rather than in a test, because both sides are constants: a
/// build that got this wrong should not produce a binary at all.
const _: () = assert!(SPEED_CHECK_INTERVAL < KEY_POLL_INTERVAL);

/// Are these two paths the same file?
///
/// Textual equality is not enough on its own.  The boot image's path and a
/// mount's path are built by different code from the same config value, and the
/// guard that stops the booted disk being inserted a second time depends on
/// them matching — if they ever did not, one file would sit in two units as two
/// independent copies with separate write-backs, and whichever saved last would
/// silently win.  Canonicalising also folds away symlinks and `.` components.
/// Falls back to the plain comparison when a path cannot be resolved, which is
/// the honest answer for a file that is not there.
fn same_file(a: &std::path::Path, b: &std::path::Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => a == b,
    }
}

/// One disk the boot path intends to insert.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BootPlanStep {
    unit: u8,
    path: std::path::PathBuf,
    writable: bool,
    /// Take the mount out of service but do **not** put the disk in a unit.
    /// Only the booted disk itself: it is already unit 0, and its mount exists
    /// solely to be got out of the way of the write-back.
    lend_only: bool,
}

/// Decide which mounted images a booted machine should carry, and where.
///
/// Pure, so the rules can be tested without a session, a registry or a disk:
///
/// * slot 0 is the disk being booted and is not in the plan — the bootstrap can
///   load a system from any slot (measured), but the system it loads comes up
///   as its own A: and reads slot 0 from then on, so a boot disk parked
///   anywhere else runs against whatever is in slot 0 and looks like a hang;
/// * every other mount rides the slot its drive letter names, because that is
///   the only mapping under which the letters an operator chose mean anything
///   to the guest. What a *slot* is belongs to the board — a drive on the
///   floppy controllers, a platter on the 88-HDSK, which carries four to a
///   drive — and this function does not need to know which: it hands the
///   machine a number, and the machine's controllers decide what it addresses;
/// * a mount is writable when the boot session is writable and the *host* will
///   let the file be written. Deliberately **not** gated on `Mount::read_only`:
///   that is our BDOS's answer, and two of its three causes — "identified by
///   inspection" and "the directory does not add up" — are statements about
///   *our* writer, which a booted guest never calls. It owns the format and
///   writes whole sectors, so a disk our record-placer would not touch is one
///   its own operating system reads and writes perfectly. Gating on that made
///   a booted machine less capable than the hardware it emulates: the disks
///   were in the drives and every one of them was write-protected by an
///   opinion the guest had not asked for. What survives is the write-protect
///   tab — `Mount::host_read_only`, a fact about the file — plus the
///   operator's own answer for the session;
/// * a drive another session is working in is left out, and so is the boot
///   image appearing twice, which would be two views of one file with separate
///   write-backs and the last one saved winning.
///
/// Returns the plan and the lines to show for whatever was left out, or `Err`
/// with a reason the boot must not go ahead at all.
fn plan_boot_disks(
    mounts: &[Option<crate::cpm::image::registry::Mount>],
    usage: &[crate::cpm::image::registry::Usage],
    boot_image: &std::path::Path,
    writable: bool,
    units: usize,
) -> Result<(Vec<BootPlanStep>, Vec<String>), String> {
    let mut plan = Vec::new();
    let mut notes = Vec::new();
    for (drive0, slot) in mounts.iter().enumerate() {
        let Some(mount) = slot else { continue };
        if drive0 >= units {
            continue;
        }
        let letter = (b'A' + drive0 as u8) as char;
        // The disk being booted, if it also happens to be mounted somewhere.
        // It must not go into a second unit — one file in two units is two
        // views with separate write-backs — but the mount still has to be taken
        // out of service, and *that* is the case the first version of this
        // missed.  The boot session rewrites this file wholesale, so a live
        // `ImageFs` left over it ends up holding an open descriptor on an
        // unlinked inode: every later read shows pre-boot content and every
        // write disappears into a deleted file, silently.
        if same_file(&mount.path, boot_image) {
            // The boot disk's mount *must* go out of service — the session
            // rewrites that file — so a drive somebody is working in cannot be
            // taken quietly the way another disk's can simply be left out.
            // There is no version of this that is safe, so the boot is refused
            // instead.  Every other drive is skipped by the check below; doing
            // that here would take the drive anyway, which is the asymmetry
            // that made this a defect.
            if usage.get(drive0).and_then(|u| u.describe()).is_some() {
                return Err(format!(
                    "{} is in use on drive {letter}: by another session, and booting it \
                     would take that drive away mid-file",
                    mount.filename
                ));
            }
            plan.push(BootPlanStep {
                unit: drive0 as u8,
                path: mount.path.clone(),
                writable: false,
                lend_only: true,
            });
            notes.push(if drive0 == 0 {
                format!("A: {} is the boot disk.", mount.filename)
            } else {
                format!("{letter}: is the boot disk; held, not repeated.")
            });
            continue;
        }
        if drive0 == 0 {
            // Unit 0 belongs to the disk we booted, so anything else mounted on
            // A: is behind it.  Saying so beats letting someone wonder where it
            // went — but it is not touched, and stays usable elsewhere.
            notes.push(format!("A: {} is behind the boot disk.", mount.filename));
            continue;
        }
        if usage.get(drive0).and_then(|u| u.describe()).is_some() {
            notes.push(format!("{letter}: {} is in use elsewhere.", mount.filename));
            continue;
        }
        plan.push(BootPlanStep {
            unit: drive0 as u8,
            path: mount.path.clone(),
            writable: writable && !mount.host_read_only,
            lend_only: false,
        });
    }
    Ok((plan, notes))
}

/// Warn about empty units between the occupied ones.
///
/// An empty unit on a real 88-DCDD answers nothing at all, and a guest that
/// selects one waits for a head that never loads — so `STAT D:` on a machine
/// with A:, B:, C: and F: mounted appears to lock up.  That is the hardware's
/// behaviour and is left alone; what we can do is warn, and point at the way
/// out, which does still work.
fn gap_warning<T>(disks: &[Option<T>]) -> Option<String> {
    let highest = disks.iter().rposition(|d| d.is_some())?;
    let gaps: Vec<String> = (1..highest)
        .filter(|u| disks[*u].is_none())
        .map(|u| format!("{}:", (b'A' + u as u8) as char))
        .collect();
    if gaps.is_empty() {
        return None;
    }
    // Two short lines rather than one long one: this is read on a 40-column
    // PETSCII screen as often as an 80-column one, and the neighbouring static
    // text was hand-split to fit.  Generated text has to be built to the same
    // width or it is the only thing on the screen that wraps.
    Some(format!("{} empty - a guest that picks", gaps.join(" ")))
}

/// One key on its way into a booted guest's console.
///
/// The guest brings its own terminal handling and we do not second-guess it —
/// with one exception, and `erase` is the operator's say over whether that
/// exception applies to the disk in front of them.
///
/// A modern telnet client's Backspace key sends **DEL (0x7F)** and a Commodore's
/// sends **PETSCII DEL (0x14)**.  Most of these operating systems read 0x7F as a
/// Teletype *rubout* — they delete the character and then **echo the character
/// they deleted**, so backspacing over `TESTING` leaves `TESTINGGNIT` on screen —
/// and 0x14 they do not recognise at all.  What those want is plain **BS
/// (0x08)**, which they answer with the universal `BS SPACE BS`.
///
/// But not all of them, which is why this is a setting and not a rule.  Measured
/// across two whole disk folders (see
/// `boot_machine.rs`'s `test_survey_backspace_across_every_bootable_image`), of
/// the 38 images that reach a prompt:
///
/// | guest                                      | 0x08       | 0x7F       |
/// |--------------------------------------------|------------|------------|
/// | MITS CP/M 2.2, Altair Disk / Hard Disk BASIC | `08 20 08` | `G`, `\G`  |
/// | DRI CP/M 2.2, MP/M, UCSD p-System          | `08 20 08` | `08 20 08` |
/// | **CP/M 1.3 / 1.4 / 1975**                  | **`^H`**   | `G`        |
///
/// 29 erase on BS and 7 on DEL — but the third row is the one that matters
/// here: for CP/M 1.x the rubout *is* the editing key and BS prints a literal
/// `^H`, so translating breaks something that already worked.  Hence
/// `cpm_boot_backspace`, and hence the boot picker asking again per disk.
fn boot_key_for_guest(byte: u8, is_petscii: bool, erase: bool) -> u8 {
    // Whichever byte this session's guest edits with.
    let del = if erase { 0x08 } else { 0x7F };
    match byte {
        // Both spellings of the key, folded to the one the guest acts on.  A
        // Commodore's is folded under `rubout` too, not just under `backspace`:
        // 0x14 means nothing to any of these guests, so leaving it alone would
        // give a C64 no editing key at all rather than the disk's own one.
        0x7F => del,
        0x14 if is_petscii => del,
        // The guest is an ASCII machine, so a Commodore's letters are folded on
        // the way in as its output is folded on the way out.
        b if is_petscii => petscii_to_ascii_byte(b),
        b => b,
    }
}

/// Marks the drives a booted session is holding as in use, and releases them
/// however the session ends.
///
/// A booted guest owns whole platters for as long as it runs, so the drives it
/// took must read as busy everywhere else: the mount screens then refuse to
/// change a disk out from under it, and a second boot skips those drives
/// instead of taking a second copy of the same file.
///
/// RAII for the same reason [`BootClaim`] is — a boot can end through an error,
/// a dropped connection or the shutdown broadcast, and drives left marked busy
/// after any of those would need a restart to clear.
struct BootDrivesBusy(Option<u64>);

impl BootDrivesBusy {
    fn hold(units: &[u8]) -> BootDrivesBusy {
        use crate::cpm::image::registry;
        // No drives taken, no session.  Registering one anyway would not be
        // free: a session in the table sits on a drive, and the default is A:.
        if units.is_empty() {
            return BootDrivesBusy(None);
        }
        let id = registry::next_session_id();
        registry::session_start(id);
        // Move off A: before marking anything.  A new session is recorded as
        // sitting on drive A: — sensible for the emulator, wrong here, and it
        // would refuse a mount change on A: while naming a drive this session
        // is not touching.  Park it on a drive it really holds.
        registry::session_select(id, units[0]);
        for unit in units {
            registry::session_writing(id, *unit);
        }
        BootDrivesBusy(Some(id))
    }
}

impl Drop for BootDrivesBusy {
    fn drop(&mut self) {
        if let Some(id) = self.0 {
            crate::cpm::image::registry::session_end(id);
        }
    }
}

/// Puts back the mounts a booted session borrowed, however the session ends.
///
/// A mount is taken out of the registry while the guest holds it, and that has
/// to be undone on *every* path out — not just the tidy one. Between the take
/// and the run loop there is a boot that can fail and a dozen writes to a
/// socket that can drop, and an early return through any of them used to leave
/// an operator's configured drives silently empty until they restarted.
///
/// So it is RAII, for the same reason [`BootClaim`] is. The remount is
/// synchronous because `Drop` cannot await: it opens a file and reads a
/// directory per borrowed drive, so up to sixteen of each, once, at session
/// teardown. That is a real cost on a runtime worker and it is accepted
/// deliberately — the alternative is a guarantee that stops holding exactly
/// when it is needed, which is after the connection has already gone.
struct RemountOnDrop {
    base: std::path::PathBuf,
    /// `(drive, bare filename)` for each mount taken.
    taken: Vec<(u8, String)>,
}

impl Drop for RemountOnDrop {
    fn drop(&mut self) {
        for (drive, name) in std::mem::take(&mut self.taken) {
            // Restore *first*, end the loan second.
            //
            // Restoring is not a mount *change*, so it is not something another
            // session may veto — `restore_mount` skips the in-use check, and
            // therefore does not need the loan gone to succeed.  Ending the
            // loan first left the drive in *neither* table for the length of a
            // file open, a format identify and a directory read, and in that
            // window a save from any screen writes `cpm_mounts` without the
            // drive while `drive_holding` reports the image as free for a
            // second session to mount elsewhere.  Being briefly in *both*
            // tables is harmless by comparison: reads consult the image before
            // the folder, and `current_mounts_value` de-duplicates by drive.
            let restored = crate::cpm::image::restore_mount(&self.base, drive, &name);
            crate::cpm::image::registry::end_boot_loan(drive);
            if let Err(e) = restored {
                glog!("CP/M boot: could not restore {} on drive {}: {}", name, drive, e);
            }
        }
    }
}

/// Write an image out so a failure cannot leave a damaged one behind.
///
/// Into a temporary beside the target, then renamed over it.  `rename` within a
/// directory is atomic on every platform this runs on, so a reader either sees
/// the old image or the new one — never a truncated file with no directory.
/// The plain write this replaced was tolerable when one disk was at stake; it
/// is not when a booted session can be holding sixteen of an operator's.
/// Where an image is staged while it is being written.
///
/// The suffix is *appended*, never substituted.  `with_extension` replaces the
/// extension, and `games.img` and `games.dsk` are both mountable — so that
/// spelling gave the two of them one temporary between them, and two concurrent
/// saves could land one disk's bytes in the other.  A separate function so the
/// rule can be asserted on its own; the collision it prevents needs two boot
/// sessions racing and cannot be shown by saving one file after another.
fn saving_path(path: &std::path::Path) -> std::path::PathBuf {
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".saving");
    std::path::PathBuf::from(tmp)
}

async fn save_image_atomically(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    // `rename` needs write permission on the *directory*, not on the file — so
    // unlike the plain write this replaced, it would happily replace an image
    // the operator had chmod'd read-only, and leave the new file writable into
    // the bargain.  Check first, and carry the old file's mode across.
    let perms = match tokio::fs::metadata(path).await {
        Ok(m) if m.permissions().readonly() => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "the image file is read-only on the host",
            ));
        }
        Ok(m) => Some(m.permissions()),
        Err(_) => None,
    };
    let tmp = saving_path(path);
    if let Err(e) = tokio::fs::write(&tmp, bytes).await {
        // Otherwise a teardown that runs out of space leaves up to sixteen
        // part-written `.saving` files, invisible to every picker and with no
        // reclaim — the same debris the `.creating` path is careful about.
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e);
    }
    if let Some(p) = perms {
        let _ = tokio::fs::set_permissions(&tmp, p).await;
    }
    match tokio::fs::rename(&tmp, path).await {
        Ok(()) => Ok(()),
        Err(e) => {
            // Leaving the temporary behind would look like a second disk in
            // the images folder, so clear it up before reporting.
            let _ = tokio::fs::remove_file(&tmp).await;
            Err(e)
        }
    }
}

/// Units the 88-DCDD can address.  Sixteen, because the drive-select register
/// carries four bits — not because any guest here uses that many.
const MAX_BOOT_UNITS: usize = crate::cpm::dcdd::MAX_DRIVES;

impl TelnetSession {
    /// Put every mounted image into the booted machine, following the plan
    /// [`plan_boot_disks`] made, and report what happened for each.
    ///
    /// The decision is separated from the doing because the decision is the
    /// part with rules in it — which unit, whose read-only flag wins, what to
    /// skip — and none of that should need a live session to test.
    fn cpm_boot_attach_mounts(
        &self,
        machine: &mut BootMachine,
        disks: &mut [Option<BootDisk>],
        writable: bool,
        remounts: &mut RemountOnDrop,
        boot_image: &std::path::Path,
        machine_key: &str,
    ) -> Result<Vec<String>, String> {
        use crate::cpm::image::registry;
        let (plan, mut notes) = plan_boot_disks(
            &registry::all(),
            &registry::usage(),
            boot_image,
            writable,
            disks.len(),
        )?;
        // The board the *booted* disk is on, so a mount that lands on a
        // different one can be called out.  From the file's size, the same way
        // `insert` decides it — asking the machine would be circular, since
        // nothing has gone into it yet.
        // **On this machine**, not on every board the gateway has.  Sizes are
        // not unique across boards — 256,256 bytes is an IBM 3740 to the
        // Tarbell and an 8" SSSD to z80pack, which is the whole reason
        // `MachineChoice::boards` exists — so `board_for(None, ..)` answers
        // with whichever board `MACHINE_CHOICES` lists first, and that is not
        // a fact about the machine being booted.  Asking with `None` on both
        // sides of the comparison below made it report "is on the Tarbell
        // 1011, not the booted disk's board" for a Cromemco single-density
        // disk the guest reads perfectly.
        let boot_board = std::fs::metadata(boot_image).ok().and_then(|md| {
            crate::cpm::boot_machine::BootMachine::board_for(Some(machine_key), md.len())
        });
        for step in plan {
            let letter = (b'A' + step.unit) as char;
            // The booted disk's own mount: take it out of service and stop.
            // It is already in unit 0, and claiming or inserting it again would
            // be the double-view this exists to prevent.
            if step.lend_only {
                if let Some(m) = registry::lend_for_boot(step.unit) {
                    remounts.taken.push((step.unit, m.filename.clone()));
                }
                continue;
            }
            let name = step
                .path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            // Claimed here rather than in the plan: a claim is a side effect on
            // a process-wide table, and a planning function that took one would
            // be untestable and would leak claims on every dry run.
            let Some(claim) = BootClaim::take(&step.path) else {
                // Somebody else is running this file — most often *this*
                // session, when one image is mounted on two drives.  Either
                // way it is about to be rewritten under this mount, so the
                // mount goes out of service even though no disk goes in.
                if let Some(m) = registry::lend_for_boot(step.unit) {
                    remounts.taken.push((step.unit, m.filename.clone()));
                }
                notes.push(format!("{letter}: {name} is booted elsewhere; held."));
                continue;
            };
            let bytes = match std::fs::read(&step.path) {
                Ok(b) => b,
                Err(e) => {
                    notes.push(format!("{letter}: cannot read {name} ({e})."));
                    continue;
                }
            };
            let len = bytes.len();
            // Only a disk this controller can actually turn.  A hard-disk or
            // Tarbell image mounts perfectly well for the emulator and has no
            // business on an 88-DCDD; refusing it by name beats presenting a
            // drive the guest cannot read.
            if let Err(e) = machine.insert(step.unit, bytes, !step.writable) {
                notes.push(format!("{letter}: {name} - {e}"));
                continue;
            }
            // Take the mount out of the registry while the guest has it.
            //
            // This is the difference between handing over a disk and copying
            // one.  A mounted image is a *live* object: `mount_image` keeps an
            // open `ImageFs` with the directory and allocation bitmap cached in
            // memory, and that object outlives the session that caused it.  If
            // it stayed while a booted guest rewrote the same file, the cache
            // would describe a disk that no longer exists — the next emulator
            // write would allocate blocks the guest had already used and lay a
            // directory entry over live data.  There is no reload that fixes
            // this after the fact, so the mount goes away for the duration and
            // is opened again, fresh, at the end.
            let remount = match registry::lend_for_boot(step.unit) {
                Some(m) => {
                    remounts.taken.push((step.unit, m.filename.clone()));
                    Some(step.unit)
                }
                None => None,
            };
            disks[step.unit as usize] = Some(BootDisk {
                path: step.path.clone(),
                writable: step.writable,
                _claim: claim,
                remount,
            });
            // Named by the board that took it, not by the drive letter it was
            // mounted under, and warned about when that board is not the one the
            // booted disk is driving.
            //
            // This is the case that reads as a broken mount and is not one: the
            // board is chosen by the image's *size*, so mounting a floppy while
            // booting a hard disk lands it on the 88-DCDD perfectly well, while
            // the guest is talking to the 88-HDSK and never looks there.
            // Everything worked; nothing is reachable; and until this line
            // nothing said so.
            //
            // Through `slot_name` rather than assembled here: the 88-HDSK's
            // slots are a drive and a platter, not a flat row, so a second copy
            // of "word plus number" would quietly disagree with every other
            // screen the moment a board's slots stopped being flat — which is
            // exactly what happened to the board itself.
            let board =
                crate::cpm::boot_machine::BootMachine::board_for(Some(machine_key), len as u64);
            let slot = crate::cpm::boot::slot_name(
                &crate::cpm::boot::SlotNaming::Boards,
                step.unit,
                Some(len as u64),
            );
            notes.push(format!(
                "{slot}: {name}{}",
                if step.writable { "" } else { " (R/O)" }
            ));
            // Only when both boards are known.  An unknown one is not evidence
            // of a mismatch, and warning on it would fire this on every mount
            // the moment the boot image became unreadable — the loudest possible
            // response to the least informative state.
            if let (Some(got), Some(want)) = (board, boot_board) {
                if got != want {
                    notes.push(format!("{name} is on the {got},"));
                    notes.push("not the booted disk's board -".to_string());
                    notes.push("the guest may not reach it.".to_string());
                }
            }
        }
        notes.extend(gap_warning(disks));
        Ok(notes)
    }

    /// Boot an image and run it until the guest stops or the user leaves.
    ///
    /// `image` is the host path; `writable` decides whether changes are kept;
    /// `erase` is what the Backspace key is handed to the guest as, which no
    /// single answer suits every disk — see [`boot_key_for_guest`].
    pub(in crate::telnet) async fn cpm_boot_session(
        &mut self,
        image: &std::path::Path,
        writable: bool,
        erase: bool,
    ) -> Result<(), std::io::Error> {
        let Some(claim) = BootClaim::take(image) else {
            self.send_line(&format!(
                "  {}",
                self.red("That image is already running in")
            ))
            .await?;
            self.send_line(&format!("  {}", self.red("another session."))).await?;
            self.send_line(&format!(
                "  {}",
                self.dim("A booted disk owns its drives, so")
            ))
            .await?;
            self.send_line(&format!("  {}", self.dim("only one session can have it at"))).await?;
            self.send_line(&format!("  {}", self.dim("a time."))).await?;
            self.send_line("").await?;
            return Ok(());
        };

        let bytes = match tokio::fs::read(image).await {
            Ok(b) => b,
            Err(e) => {
                self.send_line(&format!("  {}", self.red(&format!("Cannot read image: {e}"))))
                    .await?;
                return Ok(());
            }
        };

        let mut machine = BootMachine::new();
        // Be the machine the operator chose, before anything else touches this
        // one.  `attach_modem` below vets a modem profile against *this*
        // machine's console ports, and `boot` lays down its monitor ROM, so both
        // would be working from an Altair's layout if this came later.
        //
        // `auto` asks the disk.  A boot loader has to drive its own controller's
        // registers and a BIOS has to read its own console's, so the image says
        // which machine it is for; when it does not say plainly we stay the
        // default rather than guess.  See `cpm::detect`.
        let configured = config::get_config().cpm_boot_machine.clone();
        let (machine_key, detected_note) =
            crate::cpm::detect::machine_for(&configured, &bytes);
        // Asked here because `bytes` is about to be handed to the machine, and
        // answered on the banner below — a disk that never boots needs no
        // advice about where to watch it.
        let drives_vdm = crate::cpm::detect::image_drives_vdm(&bytes);
        machine.set_machine(&machine_key);
        // Unit 0 is the disk being booted, and it has to be: the bootstrap can
        // load a system from any unit — that was measured — but the operating
        // system it loads comes up as its own A: and reads unit 0 from then on.
        // Booting a disk parked anywhere else therefore loads fine and then
        // runs against whatever happens to be in unit 0, which looks like a
        // hang rather than a mistake.
        //
        // Declaration order is load-bearing, because drop order is its reverse.
        // `disks` first so it drops *last*: it owns every `BootClaim`, and a
        // claim released before the mounts go back would let another session
        // start booting the image while `restore_mount` — which skips every
        // guard by design — publishes a live `ImageFs` over it.  The order out
        // is `busy`, then `remounts` (restore, then end the loan), then the
        // claims: the image is claimed or mounted at every instant, and the
        // drive is in a table at every instant.
        let mut disks: Vec<Option<BootDisk>> = (0..MAX_BOOT_UNITS).map(|_| None).collect();
        let mut remounts = RemountOnDrop { base: self.cpmmount_base(), taken: Vec::new() };
        if let Err(e) = machine.insert(0, bytes, !writable) {
            self.send_line(&format!("  {}", self.red(&e))).await?;
            return Ok(());
        }
        disks[0] = Some(BootDisk {
            path: image.to_path_buf(),
            writable,
            _claim: claim,
            remount: None,
        });

        // Everything else mounted comes along, at the slot its drive letter
        // names: B: is slot 1, C: is slot 2, and so on.  What that slot *is* is
        // the board's business — a drive on a floppy controller, a platter on
        // the 88-HDSK — and what the guest calls it is the guest's: its BIOS
        // names what it knows about and refuses the rest, which for stock
        // Altair CP/M means A: to D:.  We present the hardware and let the disk
        // decide, exactly as with everything else on this path.
        let notes = match self
            .cpm_boot_attach_mounts(
                &mut machine,
                &mut disks,
                writable,
                &mut remounts,
                image,
                &machine_key,
            )
        {
            Ok(n) => n,
            Err(why) => {
                self.send_line(&format!("  {}", self.red(&why))).await?;
                self.send_line("").await?;
                return Ok(());
            }
        };
        // Hold every drive we took for as long as the guest runs.  Without
        // this a mount screen elsewhere would happily swap a disk out from
        // under a running Altair, and the change would be invisible to it —
        // the bytes are already in the machine — until the write-back at the
        // end put the old contents over the new file.
        // Only the drives that were really taken from the mount table.  Unit 0
        // is the booted disk and is not a drive anybody else is using, so
        // marking it would refuse a mount change on A: and name a drive nothing
        // is touching.
        let busy = BootDrivesBusy::hold(
            &disks
                .iter()
                .filter_map(|d| d.as_ref().and_then(|d| d.remount))
                .collect::<Vec<_>>(),
        );

        // The virtual modem comes along, when the operator's profile is one a
        // booted machine can have.  A real Altair put its modem on the second
        // port of the 88-2SIO, which is exactly the `altair_2sio2` profile — so
        // comms software running under a booted Altair CP/M finds a UART where
        // it expects one and dials out through us.  `AUX:` and HBIOS cannot
        // come: they are our own BDOS device and RomWBW's firmware, and this
        // guest brings its own of both.
        let access = crate::cpm::resolve_access(&config::get_config().cpm_emu_uart);
        let attach = machine.attach_modem(access);
        // The printer, after the modem for the same reason the modem comes
        // after `set_machine`: the port it claims must not be one this
        // machine's console or modem already answers, and both of those are
        // settled by now.  Off unless `cpm_printer` is on *and* a board is
        // named — two keys, because a port claimed by nobody's printer would
        // still be a port taken away from the guest.
        let printer_cfg = config::get_config();
        let printer_port = crate::cpm::printer::format_for(&printer_cfg.cpm_printer)
            .and(crate::cpm::printer::port_for(&printer_cfg.cpm_printer_port))
            .map(|p| p.data);
        machine.attach_printer(printer_port);
        // The joystick board, last of the three, and gated on its own key for
        // the same reason the printer is: ports `18h`-`1Ch` read `0xFF`
        // unclaimed, and `0xFF` on an analogue axis is a stick jammed
        // off-centre rather than an absence of one -- so a machine whose
        // operator did not ask for a joystick must be left exactly as it was.
        machine.set_joystick(printer_cfg.cpm_joystick);
        let mut modem = CpmModem::new(matches!(attach, ModemAttach::Ports(_, _)));
        modem.set_menu_context(self.shutdown.clone(), self.restart.clone(), self.lockouts.clone());
        // Joins the inbound `CPM@<ip>` pool for as long as the boot lasts, so a
        // booted guest is dialable exactly as an emulator session is.
        let _peer_reg = cpm_peer_register(modem.enabled());

        // Read once and used twice — to build the CPU and to say which one the
        // guest got.  Two reads could straddle another session's save and put a
        // notice on the screen that the machine disagrees with.
        let cpu_setting = config::get_config().cpm_cpu.clone();
        let mut cpu = BootMachine::new_cpu_for(&cpu_setting);
        if let Err(e) = machine.boot(&mut cpu, 0) {
            self.send_line(&format!("  {}", self.red(&e.to_string()))).await?;
            self.send_line("").await?;
            return Ok(());
        }

        let name = image.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();

        // Offer this guest's screen to the web UI, for as long as it runs.
        //
        // Registered unconditionally, and *after* the boot succeeded so a disk
        // that never started never appears in the list.  Unconditionally
        // because a VDM-1 has no data port and no way to announce itself: the
        // guest paints by storing bytes into its memory, so the only thing we
        // could gate on before the fact is a guess about the disk.  Sampling
        // costs the guest nothing — it is a read of its own RAM through its own
        // MMU — so the honest arrangement is to offer every booted session and
        // tell the viewer which ones have actually driven the card.
        let screen = crate::cpm::screen::register(screen_label(
            &name,
            self.peer_addr,
            self.is_serial,
            self.is_ssh,
            self.serial_port_id,
        ));
        // Logged rather than printed at the session: which screen is which is
        // an operator's question, asked from the browser, and the logs page is
        // where the rest of that kind of answer already lives.  Printing it
        // into the guest's session would put a line about our web UI on the
        // console of a machine that is about to start painting its own.
        glog!("CP/M boot: screen {} is {}", screen.id(), name);

        self.send_line("").await?;
        self.send_line(&format!("  {} {}", self.green("Booted"), self.amber(&name)))
            .await?;
        self.send_line(&format!(
            "  {}",
            self.dim(if writable { "Changes are saved." } else { "Read-only." })
        ))
        .await?;
        // Where this guest's screen went, for the disks that have one.  Said
        // before the guest starts painting, because after that the session is
        // the guest's and anything we print lands in the middle of its display.
        if drives_vdm {
            let webcfg = config::get_config();
            for line in vdm_notice(
                webcfg.web_enabled,
                &crate::serial::primary_local_ip(),
                webcfg.web_port,
            ) {
                self.send_line(&format!("  {}", self.dim(&line))).await?;
            }
        }
        // Which console the guest has been given.  Said rather than left
        // implicit, because a disk that goes quiet at this point is almost
        // always looking at a console that is not there, and the operator's
        // first question will be "which one did it get?".  Only when it is not
        // the default, so an ordinary Altair boot gains no extra line.
        // What detection concluded, when it was asked. Said even if it landed on
        // the default, because "this disk did not say which machine it wants" is
        // the single most useful thing to know about a disk that then goes quiet.
        if let Some(note) = &detected_note {
            let width = if self.terminal_type == TerminalType::Petscii { 38 } else { 76 };
            self.send_line(&format!("  {}", self.dim(&truncate_to_width(note, width)))).await?;
        }
        // The processor, on the same rule as the console below: only when it is
        // not the default.  A booted guest that decodes an instruction the way
        // the other CPU would looks like a corrupt disk, and this is the line
        // that says otherwise.
        if crate::cpm::cpu::is_8080(&cpu_setting) {
            self.send_line(&format!("  {}", self.dim("CPU: 8080."))).await?;
        }
        if machine_key != crate::cpm::console::DEFAULT_MACHINE {
            let c = machine.console();
            self.send_line(&format!(
                "  {}",
                // 32 columns at its widest ("Console 0x04/0x05 + CUTER ROM."),
                // so it fits 40 -- checked by hand because a runtime `format!`
                // cannot be measured by the source scan.
                self.dim(&format!(
                    "Console {:#04x}/{:#04x}{}.",
                    c.status_port,
                    c.data_port,
                    match c.rom {
                        crate::cpm::console::MonitorRom::Cuter => " + CUTER ROM",
                        crate::cpm::console::MonitorRom::None => "",
                    }
                ))
            ))
            .await?;
        }
        // The other drives, and why any of them is missing.  How many the guest
        // can actually reach is its BIOS's decision — stock Altair CP/M knows
        // four — so this says what the hardware offers, not what will appear.
        if !notes.is_empty() {
            self.send_line(&format!("  {}", self.dim("Also in the drives:"))).await?;
            let width = if self.terminal_type == TerminalType::Petscii { 36 } else { 74 };
            for note in &notes {
                self.send_line(&format!("   {}", self.dim(&truncate_to_width(note, width))))
                    .await?;
            }
            if notes.iter().any(|n| n.contains("empty")) {
                self.send_line(&format!("   {}", self.dim("one looks like a hang: ESC ESC out.")))
                    .await?;
            }
            self.send_line(&format!("  {}", self.dim("The disk's own OS decides how"))).await?;
            self.send_line(&format!("  {}", self.dim("many of them it can use."))).await?;
        }
        match &attach {
            ModemAttach::Ports(status, data) => {
                self.send_line(&format!(
                    "  {}",
                    // 27 columns at its widest ("Modem on ports 0x12/0x13."), so
                    // it fits 40 -- checked by hand because a runtime `format!`
                    // cannot be measured by the source scan.
                    self.dim(&format!("Modem on ports {status:#04x}/{data:#04x}."))
                ))
                .await?;
            }
            // Said rather than logged: an operator who set a modem up and finds
            // it missing needs the reason at the moment they notice.
            ModemAttach::Unavailable(why) => {
                self.send_line(&format!("  {}", self.amber("No modem here:"))).await?;
                self.send_line(&format!("  {}", self.amber(why))).await?;
            }
            ModemAttach::Off => {}
        }
        self.send_line(&format!("  {}", self.dim("Press ESC twice to stop."))).await?;
        self.send_line("").await?;
        self.flush().await?;

        let result = self.cpm_boot_run(&mut cpu, &mut machine, &mut modem, erase, &screen).await;

        // Save whatever the guest changed, whatever ended the session — a user
        // who pressed ESC still wants their work.
        //
        // Every unit goes back to *its own* file.  The previous version of this
        // loop handled one disk and said, in as many words, that writing every
        // dirty unit to `image` would become a corrupting bug the moment a
        // second drive was added.  This is that moment: `disks[unit]` is the
        // only thing that knows where unit 3 came from, and a unit with no
        // entry is refused rather than guessed at.
        for (unit, bytes) in machine.take_dirty() {
            let Some(disk) = disks.get(unit as usize).and_then(|d| d.as_ref()) else {
                glog!("CP/M boot: unit {} was written but has no file — not saved", unit);
                continue;
            };
            if !disk.writable {
                // Belt and braces: the machine should never mark a read-only
                // disk dirty, so if this fires something above is wrong and
                // the write is the wrong thing to trust.
                glog!(
                    "CP/M boot: unit {} is read-only but reported changes — not saved ({})",
                    unit,
                    disk.path.display()
                );
                continue;
            }
            // Written beside the image and renamed over it, so a failure
            // partway — a full disk, a network transfer_dir, the gateway being
            // killed — leaves the old image intact instead of a half-new file
            // that parses as neither.  The blast radius here is now every
            // mounted disk, not just the one the operator chose to boot.
            if let Err(e) = save_image_atomically(&disk.path, &bytes).await {
                glog!("CP/M boot: could not save {}: {}", disk.path.display(), e);
            }
        }

        // Release the drives before the mounts go back: `mount_image` refuses a
        // drive that is in use, and this session is the one holding them.  The
        // remount itself happens when `remounts` drops, a few lines later, so
        // that it also covers the paths that never reach here.
        drop(busy);

        self.send_line("").await?;
        self.send_line(&format!("  {}", self.dim("Returned to the gateway."))).await?;
        self.send_line("").await?;
        result
    }

    /// Drive the booted guest, and close its print job however the visit ends.
    ///
    /// The wrapper is here for the same reason the emulator has one: the loop
    /// leaves by `ESC ESC` or by the user hanging up, and a document that only
    /// survived one of those would be worse than no document. There is no
    /// third, tidy exit on this path — a booted operating system never
    /// "finishes" — so both of them have to work.
    async fn cpm_boot_run(
        &mut self,
        cpu: &mut Cpu,
        machine: &mut BootMachine,
        modem: &mut CpmModem,
        erase: bool,
        screen: &crate::cpm::screen::Screen,
    ) -> Result<(), std::io::Error> {
        let cfg = config::get_config();
        let transfer_dir = cfg.transfer_dir.clone();
        let print_format = crate::cpm::printer::format_for(&cfg.cpm_printer);
        // The board as well as the format: a job on a booted disk has to be
        // built for the *interface* it is coming from, because whether a bare
        // CR ends the line is the board's auto-line-feed switch and not
        // anything the bytes can say.  Resolved the same way the port was
        // claimed above, so the two cannot disagree about which board this is.
        let printer_board = print_format.and(crate::cpm::printer::port_for(&cfg.cpm_printer_port));
        // The auto-line-feed switch: the operator's setting, falling back to
        // whatever was measured for this board.  Resolved here with the rest,
        // so one boot cannot change its mind halfway through a document.
        let print_auto_lf = crate::cpm::printer::auto_lf_for(
            &cfg.cpm_printer_autolf,
            printer_board.map(|b| b.auto_lf).unwrap_or(false),
        );
        let mut spool: Option<crate::cpm::printer::SpoolJob> = None;
        // What the guest actually achieved, reported once when the session ends.
        //
        // Here rather than inside the run loop because that loop has half a
        // dozen exits and this has to cover all of them -- and because the
        // achieved figure is the only way an operator can *check* that a speed
        // setting did what it says. A governor that is quietly not working looks
        // exactly like one that is, right up to the moment somebody plays a
        // game.
        let pace_cycles = cpu.cycle_count();
        let pace_started = std::time::Instant::now();
        let result = self
            .cpm_boot_run_inner(
                cpu,
                machine,
                modem,
                erase,
                &mut spool,
                print_format,
                print_auto_lf,
                &transfer_dir,
                screen,
            )
            .await;
        // Whatever the guest left on the platen is still a print.  The file is
        // written before anything here can fail, so a hung-up session loses the
        // notice and keeps the document.
        let _ = self.cpm_boot_spool_close(&mut spool, print_format, &transfer_dir).await;
        let secs = pace_started.elapsed().as_secs_f64();
        let cycles = cpu.cycle_count().saturating_sub(pace_cycles);
        if secs > 0.5 && cycles > 0 {
            glog!(
                "CP/M boot: session ran {:.1}s at an effective {:.2} MHz ({} cycles)",
                secs,
                cycles as f64 / secs / 1e6,
                cycles
            );
        }
        result
    }

    /// Write a finished booted-disk print job out and name it on screen.
    ///
    /// Deliberately not shared with the emulator's `cpmemu_spool_close`: that
    /// one reports through the emulator's own coloured-notice style at a point
    /// where the guest is stopped, while this one interrupts a live serial
    /// console mid-session and has to leave the guest's own display alone. The
    /// spool and document logic *is* shared, in `crate::cpm::printer` — which is
    /// the part that would actually hurt to have twice.
    async fn cpm_boot_spool_close(
        &mut self,
        spool: &mut Option<crate::cpm::printer::SpoolJob>,
        format: Option<crate::cpm::printer::Format>,
        transfer_dir: &str,
    ) -> Result<(), std::io::Error> {
        let Some(job) = spool.take() else { return Ok(()) };
        let Some(format) = format else { return Ok(()) };
        if job.is_empty() {
            return Ok(());
        }
        let bytes = job.len();
        match job.write(transfer_dir, format) {
            Ok(name) => {
                self.send_raw(b"\r\n").await?;
                self.send_line(&format!(
                    "  {}",
                    self.green(&format!("[printed {bytes} bytes to {name}]"))
                ))
                .await?;
                self.flush().await?;
            }
            Err(e) => {
                glog!("CP/M printer: could not write the spool file: {e}");
                self.send_raw(b"\r\n").await?;
                self.send_line(&format!("  {}", self.red(&format!("[printer: {e}]"))))
                    .await?;
                self.flush().await?;
            }
        }
        Ok(())
    }

    /// The run loop: step the CPU, move console bytes both ways — and printer
    /// bytes out to the spool.
    #[allow(clippy::too_many_arguments)]
    async fn cpm_boot_run_inner(
        &mut self,
        cpu: &mut Cpu,
        machine: &mut BootMachine,
        modem: &mut CpmModem,
        erase: bool,
        spool: &mut Option<crate::cpm::printer::SpoolJob>,
        print_format: Option<crate::cpm::printer::Format>,
        print_auto_lf: bool,
        transfer_dir: &str,
        screen: &crate::cpm::screen::Screen,
    ) -> Result<(), std::io::Error> {
        // The speed governor, if the operator asked for one.  Built here rather
        // than inside the machine on purpose: this limits a *session*, and every
        // live gate in this project drives `BootMachine::step` in a loop
        // hundreds of millions of times -- a governor down there would slow the
        // test suite by the same factor it slows a guest.
        // Read once, here, rather than threaded through eight arguments: both
        // keys are read when a boot starts, and this is where the run begins.
        let (speed_setting, cpu_setting) = {
            let cfg = config::get_config();
            (cfg.cpm_boot_speed.clone(), cfg.cpm_cpu.clone())
        };
        let mut governor = crate::cpm::speed::mhz_for(&speed_setting, &cpu_setting)
            .map(|mhz| crate::cpm::speed::Governor::new(mhz, cpu.cycle_count(), crate::cpm::speed::now_ms()));
        if let Some(g) = governor.as_ref() {
            glog!("CP/M boot: holding the guest to {:.2} MHz", g.mhz());
        }
        let mut executed: u64 = 0;
        let mut esc_run = 0u8;
        let is_petscii = self.terminal_type == TerminalType::Petscii;
        // Consecutive key-poll seams at which the guest did nothing we could
        // see, and the activity marks used to decide that.
        let mut idle_seams: u32 = 0;
        let mut disk_before = machine.disk_accesses();
        // Set when the guest printed since the last key-poll seam.  Checked
        // there rather than where it is set, because output arrives one byte at
        // a time and pacing is decided once per seam.
        let mut printed = false;
        // When the user was last heard from.  A booted operating system has no
        // natural end — it sits at its prompt for ever, which is correct — so
        // the bound on an abandoned session is the operator's idle timeout,
        // exactly as it is for a program parked on a blocking modem read.
        let mut last_key = tokio::time::Instant::now();

        loop {
            // `step`, not `execute_instruction`: a blocking console needs the
            // guest to re-run its read rather than be handed a byte that is not
            // there.  See `BootMachine::step`.
            machine.step(cpu);
            executed += 1;

            // Pace the guest to its processor's clock.  Cycles rather than
            // instructions, from `iz80`'s own per-CPU tables, so the rate is as
            // accurate as the instruction mix rather than an average we assumed.
            if let Some(g) = governor.as_ref() {
                if executed.is_multiple_of(SPEED_CHECK_INTERVAL) {
                    if let Some(nap) = g.behind(cpu.cycle_count(), crate::cpm::speed::now_ms()) {
                        tokio::time::sleep(nap).await;
                    }
                }
            }

            if executed.is_multiple_of(KEY_POLL_INTERVAL) {
                // Everything the guest printed since the last seam, in one
                // write.  Draining per instruction instead would be a syscall
                // per character — a guest printing a directory listing would
                // make two thousand of them — and a seam is a fifth of a
                // millisecond, so nothing a person could perceive is lost.
                //
                // **The keys below are read first, deliberately.** This write
                // blocks whenever the caller's link is slower than the guest
                // prints, which for a talkative guest on a serial line is
                // always — and a keystroke waiting behind it is one the guest
                // could have acted on a whole buffer earlier.  Reading the
                // keyboard first costs nothing and means `CTRL-C` reaches a
                // runaway `PRINT` loop at the top of the seam that noticed it,
                // rather than after the seam's output has been handed over.
                let mut keys = 0usize;
                // Drain everything waiting rather than one byte per seam, so a
                // pasted command or a file being sent into the guest's console
                // moves at the wire's pace instead of one byte per 20,000
                // instructions.  Bounded so a flood cannot hold the loop here.
                while keys < 256 {
                    let Some(read) = poll_once(self.session_read_byte()) else {
                        break; // nothing waiting right now
                    };
                    let Some(b) = read? else {
                        return Ok(()); // disconnected
                    };
                    keys += 1;
                    // Two ESCs in a row leave, the same gesture the other
                    // emulator uses. A single ESC is passed through, because
                    // plenty of guest software wants it.  `is_esc_key` rather
                    // than a bare 0x1B, so a Commodore's own escape gets a user
                    // out too.
                    if is_esc_key(b, is_petscii) {
                        esc_run += 1;
                        if esc_run >= 2 {
                            return Ok(());
                        }
                    } else {
                        esc_run = 0;
                    }
                    machine.send_key(boot_key_for_guest(b, is_petscii, erase));
                }
                if keys > 0 {
                    last_key = tokio::time::Instant::now();
                }

                let out = machine.take_output();
                if !out.is_empty() {
                    // The guest is driving a bare serial console, so its
                    // control codes go out as they are — a booted OS brings
                    // whatever terminal handling it has of its own, and
                    // second-guessing it would break the software that gets it
                    // right.
                    //
                    // A Commodore is the exception, and not a cosmetic one:
                    // PETSCII swaps the two cases, so an untranslated banner
                    // arrives as graphics characters. Folding the letters is
                    // the least we can do and leaves everything else untouched.
                    if is_petscii {
                        let folded: Vec<u8> =
                            out.iter().map(|&b| ascii_to_petscii_byte(b)).collect();
                        self.send_raw(&folded).await?;
                    } else {
                        self.send_raw(&out).await?;
                    }
                    self.flush().await?;
                    printed = true;
                }

                // Keystrokes from a browser watching this guest's screen.
                //
                // Through `boot_key_for_guest` exactly like the session's own
                // bytes, so the backspace the operator chose reaches the guest
                // whichever keyboard it came from — two keyboards on one port
                // must not disagree about what DEL means.  PETSCII folding is
                // deliberately *not* applied: a browser sends ASCII whoever the
                // terminal belongs to, and treating it as a Commodore's
                // keyboard because a C64 happens to be watching would mangle it.
                //
                // No `ESC ESC` here, and that is the one thing this path does
                // differently: ending a session somebody else is sitting at is
                // not a keystroke.  A double escape from the browser reaches the
                // guest as two escapes, which is what a guest expects anyway.
                for b in screen.take_keys() {
                    machine.send_key(boot_key_for_guest(b, false, erase));
                }

                // And what the browser is *holding*, which is a different act
                // from typing and so a different call.  Read rather than
                // drained: a direction stays pushed until the hand moves, so
                // handing the board the whole set once per seam is both the
                // cheapest and the only correct shape -- the port read itself
                // must not reach for a lock tens of thousands of times a
                // second.
                //
                // Unconditional because it is one atomic load when nothing is
                // held -- and that is true of both halves, which is worth
                // saying because it was not at first: `Screen::joystick`
                // returns on the mask before reading a clock, and
                // `set_joystick_held` reads one only if the machine has a
                // board. The same bargain as `publish_screen`.
                machine.set_joystick_held(screen.joystick());

                // The screens, at the same seam and to whoever has them
                // open in the browser.  A no-op — one atomic load — unless a
                // viewer polled since the last one, so a guest nobody is
                // watching runs exactly as it did before this existed.
                //
                // It has to be here rather than anywhere the guest "prints",
                // because a memory-mapped card has no such moment: the picture
                // is a property of the guest's RAM at an instant, and the seam
                // is the only instant we own.
                machine.publish_screen(screen);

                // The printer, at the same seam and by the same reasoning — but
                // to a spool rather than to the wire, and with no PETSCII
                // folding: this is going into a document for a person on a
                // modern machine, not onto a Commodore's screen.
                let printed_bytes = machine.take_print();
                if !printed_bytes.is_empty() {
                    let job = spool.get_or_insert_with(|| {
                        crate::cpm::printer::SpoolJob::with_auto_lf(print_auto_lf)
                    });
                    for b in printed_bytes {
                        job.push(b);
                    }
                    if job.is_full() {
                        self.cpm_boot_spool_close(spool, print_format, transfer_dir)
                            .await?;
                    }
                }
                // A booted disk has no "returned to the prompt" for us to see —
                // its operating system *is* the session — so silence is the only
                // end-of-job signal there is on this path.
                if spool.as_ref().is_some_and(|j| j.idle_expired()) {
                    self.cpm_boot_spool_close(spool, print_format, transfer_dir)
                        .await?;
                }


                // Service the modem at the same seam: this is where the guest's
                // synchronous UART rings cross into async I/O.
                let mut modem_moved = false;
                if modem.enabled() {
                    // Pick up an inbound `CPM@<ip>` call when idle, so the guest
                    // can answer one exactly as an emulator session can.
                    if modem.can_answer() {
                        if let Some(call) = crate::serial::take_cpm_call_request() {
                            modem.accept_incoming(call);
                        }
                    }
                    let tx = machine.modem().drain_tx();
                    let guest_has_rx = machine.modem().rx_len() > 0;
                    let free = machine.modem().rx_free();
                    modem_moved = !tx.is_empty();
                    let rx = modem.service(tx, free, guest_has_rx).await;
                    if !rx.is_empty() {
                        machine.modem().queue_rx(&rx);
                        modem_moved = true;
                    }
                    // Reflect carrier (DCD) into the status the guest polls.
                    machine.modem().set_carrier(modem.carrier_asserted());
                }

                // Pacing.  A guest sitting at its prompt polls the console
                // status register as fast as we will let it, and without this
                // an idle booted session costs a large fraction of a core —
                // the same trap the emulator fell into at 161% CPU.  Only
                // demonstrably idle seams are paced: a keystroke, a printed
                // byte, a modem byte or any disk access resets the count, so
                // nothing that is actually working is ever slowed down.
                let disk_now = machine.disk_accesses();
                if keys > 0 || printed || modem_moved || disk_now != disk_before {
                    idle_seams = 0;
                    // A guest that is printing, loading or moving modem bytes
                    // is not an abandoned session, so the idle clock is held
                    // off.  This matches the emulator, where the timeout is
                    // enforced at a *console read* — a program in the middle of
                    // its work is never cut off, only one waiting for a person
                    // who is not there.
                    last_key = tokio::time::Instant::now();
                } else {
                    idle_seams = idle_seams.saturating_add(1);
                    if let Some(nap) = idle_nap(idle_seams) {
                        tokio::time::sleep(nap).await;
                        // **Forget the arrears.** While the guest was napped it
                        // fell behind its clock, and without this an hour spent
                        // at a prompt would buy an hour of virtual time to be
                        // spent at full speed the moment somebody typed -- the
                        // burst this governor exists to prevent, arriving by the
                        // one path that looks like good citizenship.
                        if let Some(g) = governor.as_mut() {
                            g.rebase(cpu.cycle_count(), crate::cpm::speed::now_ms());
                        }
                    }
                }
                disk_before = disk_now;
                printed = false;

                // An abandoned session.  There is no instruction ceiling here
                // on purpose: in the emulator it bounds one transient program
                // and hands the user back their `A>`, but a booted operating
                // system *is* the session and running indefinitely is what it
                // is supposed to do.  At 2000 M-instructions the ceiling would
                // have stopped every booted disk after about forty seconds of
                // sitting at its own prompt.  What actually needs bounding is a
                // user who has gone away, and that is what the idle timeout is.
                if !self.idle_timeout.is_zero() && last_key.elapsed() >= self.idle_timeout {
                    glog!("CP/M boot: session idle timeout with a disk booted");
                    return Ok(());
                }
            }

            if executed.is_multiple_of(YIELD_INTERVAL) {
                tokio::task::yield_now().await;
                // A guest waiting on a disk that is not turning is a bug in
                // our controller, not in the disk — say so rather than let it
                // look like a runaway program, which is a mistake this project
                // has already made once.
                if machine.stuck_polls() > 1_000_000 {
                    self.send_line("").await?;
                    self.send_line(&format!(
                        "  {}",
                        self.red("Stopped: the guest is waiting for a")
                    ))
                    .await?;
                    self.send_line(&format!("  {}", self.red("sector that never arrives."))).await?;
                    glog!("CP/M boot: controller stalled — the disk is not advancing");
                    return Ok(());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A VDM-1 disk is told where its screen went, and told the truth when
    /// there is nowhere for it to go — the same say-why rule the rest of this
    /// boot path follows, and the case that actually matters, because the web
    /// UI is **off by default**.
    #[test]
    fn test_a_vdm_disk_is_told_where_its_screen_is() {
        let on = vdm_notice(true, "192.168.1.178", 8080);
        assert!(on.iter().any(|l| l.contains("http://192.168.1.178:8080/vdm")));

        let off = vdm_notice(false, "192.168.1.178", 8080);
        assert!(!off.iter().any(|l| l.contains("http://")), "no address that shows nothing");
        assert!(off.iter().any(|l| l.contains("off")), "say why: {off:?}");

        // 40 columns with the two-space indent these are printed under, which
        // the URL line is the only real candidate to overrun.
        for line in on.iter().chain(off.iter()) {
            assert!(line.chars().count() <= 38, "{line:?} does not fit a PETSCII screen");
        }
    }

    /// The screen list names the image and where the typist is, because the
    /// person picking a screen in the browser is not necessarily the person at
    /// the keyboard.
    #[test]
    fn test_a_screen_is_named_for_its_image_and_its_caller() {
        use crate::config::SerialPortId;
        let ip: std::net::IpAddr = "10.0.0.9".parse().unwrap();

        assert_eq!(
            screen_label("TDISK04.DSK", Some(ip), false, false, None),
            "TDISK04.DSK — telnet 10.0.0.9"
        );
        assert_eq!(
            screen_label("TDISK04.DSK", Some(ip), false, true, None),
            "TDISK04.DSK — SSH 10.0.0.9"
        );
        assert_eq!(
            screen_label("TDISK04.DSK", None, true, false, Some(SerialPortId::B)),
            "TDISK04.DSK — serial port B"
        );
        // A relay session behaves like a serial caller and arrives over IP, so
        // it has a peer and no local port.  Naming the slave's address is the
        // most useful thing there is.
        assert_eq!(
            screen_label("TDISK04.DSK", Some(ip), true, false, None),
            "TDISK04.DSK — relay 10.0.0.9"
        );
        // Nothing known about the caller still produces a usable name rather
        // than a dangling separator.
        assert_eq!(screen_label("A.DSK", None, false, false, None), "A.DSK — telnet");
    }

    /// One session per image, and the claim comes back however the session
    /// ends — a claim leaked by an error path could never be booted again
    /// without restarting the gateway.
    #[test]
    fn test_an_image_can_only_be_booted_once_at_a_time() {
        // The claim set lives in the process-global registry, and every test
        // that resets the registry wipes it — so these must serialise with
        // them.  Without this they pass alone and fail about one run in ten
        // beside `test_a_booted_image_cannot_be_mounted`, which is the profile
        // of a flake that reaches CI and gets re-run away.
        let _g = crate::cpm::image::registry::tests_lock();
        let p = std::path::Path::new("/tmp/egw_boot_claim_test.dsk");
        let first = BootClaim::take(p).expect("first claim");
        assert!(BootClaim::take(p).is_none(), "a second session must be refused");
        drop(first);
        assert!(BootClaim::take(p).is_some(), "the claim returns when the session ends");
    }

    /// Different images do not block each other.
    #[test]
    fn test_two_different_images_can_run_together() {
        // The claim set lives in the process-global registry, and every test
        // that resets the registry wipes it — so these must serialise with
        // them.  Without this they pass alone and fail about one run in ten
        // beside `test_a_booted_image_cannot_be_mounted`, which is the profile
        // of a flake that reaches CI and gets re-run away.
        let _g = crate::cpm::image::registry::tests_lock();
        let a = BootClaim::take(std::path::Path::new("/tmp/egw_boot_a.dsk")).unwrap();
        let b = BootClaim::take(std::path::Path::new("/tmp/egw_boot_b.dsk"));
        assert!(b.is_some(), "separate images are independent");
        drop(a);
    }

    /// The poll interval must divide the yield interval, or the key check and
    /// the yield drift apart and one of them effectively stops happening.
    #[test]
    fn test_the_loop_intervals_line_up() {
        // The speed check is finer than both, and must divide them or the seams
        // drift apart: a pace check landing between a key poll and a yield
        // would make the pacing depend on where in the loop it happened to fall.
        assert!(
            KEY_POLL_INTERVAL.is_multiple_of(SPEED_CHECK_INTERVAL),
            "the key poll must land on a speed check: {KEY_POLL_INTERVAL} / {SPEED_CHECK_INTERVAL}",
        );
        assert!(
            YIELD_INTERVAL.is_multiple_of(SPEED_CHECK_INTERVAL),
            "and so must the yield",
        );
        assert!(
            YIELD_INTERVAL.is_multiple_of(KEY_POLL_INTERVAL),
            "the yield must fall on a key-poll boundary, or the two drift apart \
             and one of them effectively stops happening"
        );
    }
    /// Under `backspace`, both spellings of the key reach the guest as the one
    /// byte most of these operating systems erase on.
    ///
    /// A client sends 0x7F, a Commodore sends 0x14, and a guest reading 0x7F
    /// literally echoes the character it just deleted instead of rubbing it out
    /// — the `TESTINGGNIT` on screen that this setting exists to stop.
    #[test]
    fn test_backspace_mode_sends_bs_for_either_delete_key() {
        assert_eq!(boot_key_for_guest(0x7F, false, true), 0x08, "a client's DEL key");
        assert_eq!(boot_key_for_guest(0x7F, true, true), 0x08, "DEL over a PETSCII session");
        assert_eq!(boot_key_for_guest(0x14, true, true), 0x08, "a Commodore's DEL key");
        assert_eq!(boot_key_for_guest(0x08, false, true), 0x08, "a client already sending BS");
    }

    /// Under `rubout` the guest gets the key its own operating system edits
    /// with, which for CP/M 1.x is the only one that works — BS prints a
    /// literal `^H` there.
    ///
    /// A Commodore's 0x14 is folded here too, and that is the part worth
    /// pinning: leaving it alone would be faithful to the wire and useless to
    /// the person, because no guest in either survey recognises 0x14 at all.
    #[test]
    fn test_rubout_mode_sends_del_for_either_delete_key() {
        assert_eq!(boot_key_for_guest(0x7F, false, false), 0x7F, "passed through");
        assert_eq!(boot_key_for_guest(0x14, true, false), 0x7F, "a Commodore's DEL key");
        assert_eq!(
            boot_key_for_guest(0x08, false, false),
            0x08,
            "a terminal sending BS is left alone — the setting is about the DEL key"
        );
    }

    /// And nothing else changed: 0x14 is only a delete on a Commodore keyboard,
    /// and the case fold a booted guest depends on still happens, both ways.
    #[test]
    fn test_the_rest_of_the_key_path_is_untouched() {
        for erase in [true, false] {
            assert_eq!(boot_key_for_guest(0x14, false, erase), 0x14, "^T from ASCII");
            assert_eq!(boot_key_for_guest(b'A', true, erase), b'a', "PETSCII upper bank");
            assert_eq!(boot_key_for_guest(0xC1, true, erase), b'A', "PETSCII shifted-upper");
            for b in [b'A', b'z', b'7', 0x1B, 0x0D, 0x03] {
                assert_eq!(boot_key_for_guest(b, false, erase), b, "{b:#04X} on ASCII");
            }
        }
    }

    /// The label a screen shows is the behaviour the machine gives, even when
    /// the config file says something neither setting means.
    #[test]
    fn test_an_unrecognised_backspace_setting_reads_as_the_default() {
        use crate::cpm::boot::{backspace_erases, backspace_label, BACKSPACE_CHOICES};
        assert!(backspace_erases("backspace"));
        assert!(!backspace_erases("rubout"));
        assert!(!backspace_erases("  RUBOUT  "), "trimmed and case-insensitive");
        assert!(backspace_erases("nonsense"), "unknown falls back to the default");
        assert_eq!(backspace_label("nonsense"), BACKSPACE_CHOICES[0].1);
        assert_eq!(backspace_label("rubout"), BACKSPACE_CHOICES[1].1);
    }

    /// The other half of the same repair: what the guest sends *back* to erase.
    ///
    /// Every guest measured answers a backspace with the universal
    /// `BS SPACE BS`, which on a Commodore has to render as left, space, left.
    /// The C64 is the only terminal that needs anything done to it, so this is
    /// checked here rather than left to the ASCII path.
    #[test]
    fn test_a_guests_erase_reaches_a_commodore_as_an_overwrite() {
        let out: Vec<u8> = b"\x08 \x08".iter().map(|&b| ascii_to_petscii_byte(b)).collect();
        assert_eq!(out, b"\x9D \x9D", "left, space, left — not a destructive DEL");
    }

    use crate::cpm::image::registry::{Mount, Usage};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    fn mount_at(name: &str, read_only: bool) -> Mount {
        Mount {
            path: PathBuf::from(format!("/images/{name}")),
            filename: name.to_string(),
            read_only,
            read_only_reason: String::new(),
            // Our BDOS's answer and the host's are separate questions, and the
            // boot path asks only the second — so a helper that conflated them
            // could not have shown the difference.  `mount_at` is the ordinary
            // case: whatever we think of the directory, the file is writable.
            host_read_only: false,
            format: "altair8",
            fs: std::sync::Arc::new(std::sync::Mutex::new(
                crate::cpm::image::fs::ImageFs::mount(
                    Box::new(crate::cpm::image::media::MemMedia::new(
                        crate::cpm::image::format::by_token("altair8")
                            .unwrap()
                            .blank_image()
                            .unwrap(),
                    )),
                    crate::cpm::image::format::by_token("altair8").unwrap(),
                    read_only,
                )
                .unwrap(),
            )),
        }
    }

    /// Sixteen slots with the named drives filled.
    fn mounts(spec: &[(usize, &str, bool)]) -> Vec<Option<Mount>> {
        let mut v: Vec<Option<Mount>> = (0..16).map(|_| None).collect();
        for (d, n, ro) in spec {
            v[*d] = Some(mount_at(n, *ro));
        }
        v
    }

    fn idle() -> Vec<Usage> {
        vec![Usage::default(); 16]
    }

    /// The mapping the whole feature rests on: a mounted image rides the board
    /// slot its drive letter names.  Anything else and the letters an operator
    /// chose stop meaning what they say.
    #[test]
    fn test_mounts_ride_the_slot_their_letter_names() {
        let m = mounts(&[(0, "boot.dsk", false), (1, "b.dsk", false), (5, "f.dsk", false)]);
        let (plan, _) = plan_boot_disks(&m, &idle(), Path::new("/images/boot.dsk"), true, 16).unwrap();
        assert_eq!(
            plan.iter()
                .filter(|s| !s.lend_only)
                .map(|s| (s.unit, s.path.clone()))
                .collect::<Vec<_>>(),
            vec![
                (1u8, PathBuf::from("/images/b.dsk")),
                (5u8, PathBuf::from("/images/f.dsk")),
            ],
            "B: must be unit 1 and F: unit 5, gaps and all"
        );
    }

    /// Unit 0 belongs to the disk being booted.  The bootstrap can load from
    /// any unit, but what it loads comes up as A: and reads unit 0 — so a
    /// second disk there would be running the wrong machine.
    #[test]
    fn test_unit_zero_never_receives_a_disk() {
        // A: holds the boot disk itself.  Nothing is inserted — it is already
        // unit 0 — but the mount must still be taken out of service, because
        // the session is going to rewrite that very file.
        let m = mounts(&[(0, "boot.dsk", false)]);
        let (plan, _) = plan_boot_disks(&m, &idle(), Path::new("/images/boot.dsk"), true, 16).unwrap();
        assert_eq!(plan.len(), 1);
        assert!(plan[0].lend_only, "the boot disk's own mount is lent, not inserted");

        // A: holds something else — it is shadowed, and the operator is told.
        // It is not touched: it stays mounted and usable elsewhere.
        let m = mounts(&[(0, "other.dsk", false)]);
        let (plan, notes) = plan_boot_disks(&m, &idle(), Path::new("/images/boot.dsk"), true, 16).unwrap();
        assert!(plan.is_empty(), "unit 0 is the boot disk's, always");
        assert!(notes.iter().any(|n| n.contains("behind the boot disk")), "{notes:?}");
    }

    /// The same file in two units would be two views of one disk with separate
    /// write-backs, and whichever saved last would win.  It is still lent,
    /// though: the session rewrites that file, and a live `ImageFs` left over
    /// it would afterwards be holding an unlinked inode.
    #[test]
    fn test_the_boot_image_is_lent_but_never_inserted_twice() {
        let m = mounts(&[(3, "boot.dsk", false)]);
        let (plan, notes) = plan_boot_disks(&m, &idle(), Path::new("/images/boot.dsk"), true, 16).unwrap();
        assert_eq!(plan.len(), 1);
        assert!(plan[0].lend_only, "taken out of service, not given a unit");
        assert_eq!(plan[0].unit, 3, "lent from the drive it is actually on");
        assert!(notes.iter().any(|n| n.contains("boot disk")), "{notes:?}");
    }

    /// **Which read-only answer a booted disk is subject to, and which it is
    /// not.**
    ///
    /// The session's own is absolute: a machine the operator did not open for
    /// writing writes nowhere, because this path lays down raw sectors and
    /// nothing above it could notice a mistake.
    ///
    /// The *mount's* is not, and that is the whole point of this test. Two of
    /// its three causes — the format was identified by inspection, or the
    /// directory did not add up — are our record-placer declining a job it is
    /// not sure of. A booted guest never calls it: the disk's own operating
    /// system owns the format. Honouring that answer here write-protected
    /// every companion disk in the machine on the strength of an opinion the
    /// guest had not asked for, which is not what the hardware does.
    ///
    /// What does survive is the host refusing the file — the write-protect
    /// tab, a fact about the file rather than about its contents.
    #[test]
    fn test_the_boot_path_honours_the_host_not_our_own_writer() {
        let mut m = mounts(&[(1, "plain.dsk", false), (2, "we-would-not.dsk", true)]);
        // A third: the host itself refuses the file, whatever we make of it.
        let mut tab = mount_at("write-protected.dsk", false);
        tab.host_read_only = true;
        m[3] = Some(tab);

        let w = |mounts: &[Option<Mount>], writable| {
            plan_boot_disks(mounts, &idle(), Path::new("/images/boot.dsk"), writable, 16)
                .unwrap()
                .0
                .iter()
                .map(|s| (s.unit, s.writable))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            w(&m, true),
            vec![(1, true), (2, true), (3, false)],
            "our own read-only verdict must not reach a booted guest; the host's must"
        );
        assert_eq!(
            w(&m, false),
            vec![(1, false), (2, false), (3, false)],
            "a read-only boot session must not write anywhere"
        );
    }

    /// The boot disk's own mount has to go out of service — the session
    /// rewrites that file — so unlike every other drive it cannot simply be
    /// left out when somebody is working in it.  The boot is refused instead.
    #[test]
    fn test_booting_a_disk_somebody_is_working_in_is_refused() {
        let m = mounts(&[(1, "boot.dsk", false)]);
        let mut u = idle();
        u[1] = Usage { sitting: 1, writing: 0 };
        let err = plan_boot_disks(&m, &u, Path::new("/images/boot.dsk"), true, 16)
            .expect_err("must refuse rather than take the drive anyway");
        assert!(err.contains("boot.dsk") && err.contains("B:"), "{err}");
        // Idle, it is taken as normal.
        assert!(plan_boot_disks(&m, &idle(), Path::new("/images/boot.dsk"), true, 16)
            .unwrap()
            .0[0]
            .lend_only);
    }

    /// A drive another session is working in must not be handed to a guest that
    /// owns whole platters.
    #[test]
    fn test_a_busy_drive_is_left_out() {
        let m = mounts(&[(1, "busy.dsk", false), (2, "free.dsk", false)]);
        let mut u = idle();
        u[1] = Usage { sitting: 1, writing: 0 };
        let (plan, notes) = plan_boot_disks(&m, &u, Path::new("/images/boot.dsk"), true, 16).unwrap();
        assert_eq!(plan.iter().map(|s| s.unit).collect::<Vec<_>>(), vec![2]);
        assert!(notes.iter().any(|n| n.contains("in use elsewhere")), "{notes:?}");
    }

    /// A machine with fewer units than sixteen must not be handed a plan that
    /// runs off the end of it.
    #[test]
    fn test_the_plan_respects_the_unit_count() {
        let m = mounts(&[(1, "b.dsk", false), (9, "j.dsk", false)]);
        let (plan, _) = plan_boot_disks(&m, &idle(), Path::new("/images/boot.dsk"), true, 4).unwrap();
        assert_eq!(plan.iter().map(|s| s.unit).collect::<Vec<_>>(), vec![1]);
    }

    /// The boot disk must be recognised as the boot disk however its path was
    /// spelled.  Two different pieces of code build that path from the same
    /// config value, and if they ever disagreed the same file would sit in two
    /// units as two independent copies — with separate write-backs, and
    /// whichever saved last silently winning.
    #[test]
    fn test_the_boot_disk_is_recognised_through_a_differently_spelled_path() {
        let dir = std::env::temp_dir().join("egw_boot_path_identity");
        let _ = std::fs::create_dir_all(&dir);
        let real = dir.join("boot.dsk");
        std::fs::write(&real, b"disk").unwrap();

        // The same file reached by another route.  A `.` component would not
        // do: Rust's `Path` equality compares components and already folds
        // those away, so it would prove nothing.  `..` is not folded, and is
        // what a differently-built base path actually looks like.
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let spelled = dir.join("sub").join("..").join("boot.dsk");
        assert_ne!(spelled, real, "the test is pointless if these are equal");
        assert!(same_file(&spelled, &real), "canonicalising must fold this away");

        let mut m: Vec<Option<Mount>> = (0..16).map(|_| None).collect();
        m[2] = Some(Mount {
            path: spelled,
            filename: "boot.dsk".into(),
            read_only: false,
            read_only_reason: String::new(),
            host_read_only: false,
            format: "altair8",
            fs: mount_at("boot.dsk", false).fs,
        });
        let (plan, notes) = plan_boot_disks(&m, &idle(), &real, true, 16).unwrap();
        assert_eq!(plan.len(), 1);
        assert!(plan[0].lend_only, "the boot disk must not be inserted twice");
        assert!(notes.iter().any(|n| n.contains("boot disk")), "{notes:?}");

        // Two genuinely different files are still two different files.
        let other = dir.join("other.dsk");
        std::fs::write(&other, b"other").unwrap();
        let mut m2: Vec<Option<Mount>> = (0..16).map(|_| None).collect();
        m2[2] = Some(Mount {
            path: other,
            filename: "other.dsk".into(),
            read_only: false,
            read_only_reason: String::new(),
            host_read_only: false,
            format: "altair8",
            fs: mount_at("other.dsk", false).fs,
        });
        let other_plan = plan_boot_disks(&m2, &idle(), &real, true, 16).unwrap().0;
        assert_eq!(other_plan.len(), 1);
        assert!(!other_plan[0].lend_only, "a different file really is inserted");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A blank Altair image, written where a test can mount it.
    fn blank_image_at(path: &Path) {
        std::fs::write(
            path,
            crate::cpm::image::format::by_token("altair8").unwrap().blank_image().unwrap(),
        )
        .unwrap();
    }

    /// The hazard the remount exists for: a mounted image is a *live* object
    /// with its directory cached in memory, and a booted guest rewrites the
    /// whole file underneath it.  Leaving the old mount in place would leave
    /// that cache describing a disk that no longer exists — and the next write
    /// through it allocating blocks the guest had already used.
    ///
    /// So the drive must be empty while the guest holds it, and mounted again
    /// afterwards from the bytes on disk.  This drives the two registry calls
    /// directly, which is the part that has to be right; the boot loop around
    /// them is exercised live.
    #[test]
    fn test_a_taken_mount_is_removed_and_comes_back_fresh() {
        use crate::cpm::image::{mount_image, registry};
        let _g = registry::tests_lock();
        registry::tests_reset();

        let base = std::env::temp_dir().join("egw_boot_remount");
        let _ = std::fs::remove_dir_all(&base);
        let images = crate::cpm::image::images_dir(&base);
        std::fs::create_dir_all(&images).unwrap();
        let file = images.join("altair8_taken.dsk");
        blank_image_at(&file);

        mount_image(&base, 1, "altair8_taken.dsk").expect("mounts on B:");
        let before = registry::get(1).expect("mounted");
        assert!(!before.read_only);

        // Booting takes it out of the registry, so nothing is holding a stale
        // view of a file the guest is about to rewrite.
        assert!(registry::unmount(1).is_some(), "the mount is taken away");
        assert!(registry::get(1).is_none(), "drive B: must be empty while booted");

        // The guest rewrites the file wholesale, exactly as the write-back does.
        let mut changed = std::fs::read(&file).unwrap();
        changed[0] ^= 0xFF;
        std::fs::write(&file, &changed).unwrap();

        // And it comes back, reading the bytes that are there now.
        mount_image(&base, 1, "altair8_taken.dsk").expect("remounts on B:");
        let after = registry::get(1).expect("mounted again");
        assert_eq!(after.filename, "altair8_taken.dsk");
        assert!(!Arc::ptr_eq(&before.fs, &after.fs), "a fresh ImageFs, not the stale one");

        registry::tests_reset();
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The drives must come back even when the session never reaches its tidy
    /// exit.  Between taking a mount and starting the guest there is a boot
    /// that can fail and a dozen writes to a socket that can drop; an early
    /// return through any of them used to leave an operator's configured drives
    /// silently empty until they restarted the gateway.
    #[test]
    fn test_taken_mounts_come_back_even_on_an_early_exit() {
        use crate::cpm::image::{mount_image, registry};
        let _g = registry::tests_lock();
        registry::tests_reset();

        let base = std::env::temp_dir().join("egw_boot_remount_raii");
        let _ = std::fs::remove_dir_all(&base);
        let images = crate::cpm::image::images_dir(&base);
        std::fs::create_dir_all(&images).unwrap();
        blank_image_at(&images.join("altair8_a.dsk"));
        blank_image_at(&images.join("altair8_b.dsk"));
        mount_image(&base, 1, "altair8_a.dsk").unwrap();
        mount_image(&base, 2, "altair8_b.dsk").unwrap();

        {
            let mut remounts =
                RemountOnDrop { base: base.clone(), taken: Vec::new() };
            for drive in [1u8, 2] {
                // The real path: lent, not merely unmounted, so the remount has
                // to end the loan before `mount_image` will accept the drive.
                let m = registry::lend_for_boot(drive).expect("taken for the guest");
                remounts.taken.push((drive, m.filename.clone()));
            }
            assert!(registry::get(1).is_none() && registry::get(2).is_none());
            assert_eq!(registry::boot_loans().len(), 2, "both drives read as lent");
            // ...and now the session dies here, without reaching any cleanup.
        }

        assert_eq!(
            registry::get(1).map(|m| m.filename),
            Some("altair8_a.dsk".to_string()),
            "B: must come back"
        );
        assert_eq!(
            registry::get(2).map(|m| m.filename),
            Some("altair8_b.dsk".to_string()),
            "C: must come back"
        );
        assert!(registry::boot_loans().is_empty(), "no loan may outlive the session");
        registry::tests_reset();
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Saving must never replace an image the host says is read-only.
    ///
    /// The plain write this replaced failed with EACCES on a `chmod 444` file.
    /// `rename` does not: it needs permission on the *directory*, so the atomic
    /// save would have quietly overwritten a disk somebody had deliberately
    /// protected — and left the replacement writable, so the next mount would
    /// come up read-write too.
    #[tokio::test]
    async fn test_a_read_only_image_is_never_replaced() {
        let dir = std::env::temp_dir().join("egw_save_readonly");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("altair8_precious.dsk");
        std::fs::write(&path, b"original").unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&path, perms).unwrap();

        let err = save_image_atomically(&path, b"replacement").await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied, "{err}");
        assert_eq!(std::fs::read(&path).unwrap(), b"original", "the disk survived");
        assert!(
            std::fs::metadata(&path).unwrap().permissions().readonly(),
            "and is still protected"
        );

        // A writable one is replaced, atomically, keeping its mode.
        let ok = dir.join("altair8_scratch.dsk");
        std::fs::write(&ok, b"old").unwrap();
        save_image_atomically(&ok, b"new").await.unwrap();
        assert_eq!(std::fs::read(&ok).unwrap(), b"new");
        // No temporary left behind for the mount pickers to find.
        assert!(!dir.join("altair8_scratch.dsk.saving").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two images differing only in extension must not share a temp name.
    /// Asserted on the derivation itself, because the collision needs two boot
    /// sessions racing and a sequential save cannot show it.
    #[test]
    fn test_saving_paths_are_distinct_per_image() {
        let p = |s: &str| saving_path(Path::new(s));
        assert_ne!(p("/i/games.dsk"), p("/i/games.img"), "one temp for two disks");
        assert_eq!(p("/i/games.dsk"), Path::new("/i/games.dsk.saving"));
        // Every mountable extension must stay distinct from every other.
        let names: Vec<_> = ["dsk", "img", "ima", "image", "cpm"]
            .iter()
            .map(|e| p(&format!("/i/x.{e}")))
            .collect();
        let mut uniq = names.clone();
        uniq.sort();
        uniq.dedup();
        assert_eq!(uniq.len(), names.len(), "temp names collide: {names:?}");
        // And the temporary must not itself look like a mountable image, or the
        // pickers would offer a half-written disk.
        let tmp = p("/i/x.dsk");
        let tmp_name = tmp.file_name().unwrap().to_string_lossy().to_string();
        assert!(
            !crate::cpm::image::looks_like_an_image_name(&tmp_name),
            "a half-written disk must not be offered by the mount pickers: {tmp_name}"
        );
    }

    /// Two images differing only in extension must not share a temp name.
    ///
    /// `with_extension("dsk.saving")` *replaces* the extension, so `games.img`
    /// and `games.dsk` — both mountable — collided on one temporary, and two
    /// concurrent saves could land one disk's bytes in the other.
    #[tokio::test]
    async fn test_the_save_temp_name_is_appended_not_substituted() {
        let dir = std::env::temp_dir().join("egw_save_tempname");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dsk = dir.join("games.dsk");
        let img = dir.join("games.img");
        std::fs::write(&dsk, b"aaaa").unwrap();
        std::fs::write(&img, b"bbbb").unwrap();
        save_image_atomically(&dsk, b"DSK!").await.unwrap();
        save_image_atomically(&img, b"IMG!").await.unwrap();
        assert_eq!(std::fs::read(&dsk).unwrap(), b"DSK!");
        assert_eq!(std::fs::read(&img).unwrap(), b"IMG!", "one disk landed in the other");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// One image must never be claimed twice because two code paths spell its
    /// path differently.  Boot targets and mount paths come from the same
    /// config value by different routes and only one canonicalises, so raw
    /// equality would put one disk in two machines, both writing it back.
    #[test]
    fn test_a_claim_is_by_identity_not_by_spelling() {
        // The claim set lives in the process-global registry, and every test
        // that resets the registry wipes it — so these must serialise with
        // them.  Without this they pass alone and fail about one run in ten
        // beside `test_a_booted_image_cannot_be_mounted`, which is the profile
        // of a flake that reaches CI and gets re-run away.
        let _g = crate::cpm::image::registry::tests_lock();
        let dir = std::env::temp_dir().join("egw_claim_identity");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let real = dir.join("one.dsk");
        std::fs::write(&real, b"disk").unwrap();
        let other_spelling = dir.join("sub").join("..").join("one.dsk");
        assert_ne!(other_spelling, real, "the test needs two spellings");

        let first = BootClaim::take(&real).expect("first claim");
        assert!(
            BootClaim::take(&other_spelling).is_none(),
            "the same file claimed twice under another name"
        );
        drop(first);
        // Released, so it can be booted again afterwards.
        assert!(BootClaim::take(&other_spelling).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A disk booted without being mounted first is still visible somewhere.**
    ///
    /// This was the gap: `boot_loans` records a mount that a boot *took away*,
    /// so it covers the disk that was already mounted — and covers nothing at
    /// all for the ordinary case, picking a disk out of the boot picker's list.
    /// That image was in none of the registry's tables, so the disks screen
    /// showed no sign of it while `mount_image` refused it by name.
    ///
    /// A screen that refuses an action for a reason it never displayed is the
    /// same class of defect as a lent drive reading as empty, and it is checked
    /// the same way: against the registry, not against the drawing code.
    #[test]
    fn test_a_booted_image_is_listed_even_when_it_was_never_mounted() {
        use crate::cpm::image::registry;
        let _g = registry::tests_lock();
        registry::tests_reset();

        let dir = std::env::temp_dir().join("egw_booted_listing");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("altair8_solo.dsk");
        std::fs::write(&path, b"disk").unwrap();

        assert!(registry::booted_image_names().is_empty(), "nothing booted yet");
        assert!(
            registry::boot_loans().is_empty(),
            "and nothing lent — this disk was never mounted, which is the case that was missed"
        );

        let claim = BootClaim::take(&path).expect("claimed");
        assert_eq!(
            registry::booted_image_names(),
            vec!["altair8_solo.dsk".to_string()],
            "the screen has to be able to name what is running"
        );
        // Still nothing lent: the two lists answer different questions, and
        // conflating them is what would put a drive letter on a booted disk.
        assert!(registry::boot_loans().is_empty());

        drop(claim);
        assert!(
            registry::booted_image_names().is_empty(),
            "the listing outlived the session — the screen would refuse it for ever"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A claim must be released even if the image is deleted while it is
    /// booted.
    ///
    /// The key is canonicalised, and canonicalising needs the file to exist —
    /// so re-deriving it at release time would file the removal under a
    /// different name than the claim, and that image could never be booted or
    /// mounted again without restarting the gateway.  With the shipped
    /// relative `transfer_dir` this is not a corner case.
    #[test]
    fn test_a_claim_is_released_even_if_the_image_is_deleted() {
        // The claim set lives in the process-global registry, and every test
        // that resets the registry wipes it — so these must serialise with
        // them.  Without this they pass alone and fail about one run in ten
        // beside `test_a_booted_image_cannot_be_mounted`, which is the profile
        // of a flake that reaches CI and gets re-run away.
        let _g = crate::cpm::image::registry::tests_lock();
        let dir = std::env::temp_dir().join("egw_claim_deleted");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("vanishing.dsk");
        std::fs::write(&path, b"disk").unwrap();

        let claim = BootClaim::take(&path).expect("claimed");
        assert!(crate::cpm::image::registry::is_image_booted(&path));
        // The operator deletes it from the images folder mid-session.
        std::fs::remove_file(&path).unwrap();
        drop(claim);

        // Put it back and it must be bootable again.
        std::fs::write(&path, b"disk").unwrap();
        assert!(
            !crate::cpm::image::registry::is_image_booted(&path),
            "the claim leaked: this image can never be booted again"
        );
        assert!(BootClaim::take(&path).is_some(), "and can be claimed afresh");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Only drives really taken from the mount table are marked busy.  Marking
    /// unit 0 would refuse a mount change on A: while naming a drive nothing is
    /// using, for as long as the booted session sits at its prompt.
    #[test]
    fn test_only_taken_mounts_are_held_busy() {
        let _g = crate::cpm::image::registry::tests_lock();
        crate::cpm::image::registry::tests_reset();
        {
            let _busy = BootDrivesBusy::hold(&[2, 3]);
            let usage = crate::cpm::image::registry::usage();
            assert!(usage[2].describe().is_some(), "C: is held");
            assert!(usage[3].describe().is_some(), "D: is held");
            assert!(usage[0].describe().is_none(), "A: is nobody's business");
            assert!(usage[1].describe().is_none());
        }
        // And holding nothing must register nothing at all — including not
        // parking a phantom session on A:, which is what registering an empty
        // hold used to do.
        {
            let _none = BootDrivesBusy::hold(&[]);
            assert!(
                crate::cpm::image::registry::usage_of(0).describe().is_none(),
                "a boot that took no mounts must not mark drive A: busy"
            );
        }
        // RAII: a boot that ends any way at all must release them.
        assert!(
            crate::cpm::image::registry::usage().iter().all(|u| u.describe().is_none()),
            "drives stay busy after the session ended"
        );
        crate::cpm::image::registry::tests_reset();
    }

    /// A gap between drives is warned about, because selecting one looks
    /// exactly like a crash — an empty 88-DCDD unit answers nothing and the
    /// guest waits for a head that never loads.
    #[test]
    fn test_gaps_between_drives_are_warned_about() {
        let filled = |units: &[usize]| -> Vec<Option<()>> {
            let mut v: Vec<Option<()>> = (0..16).map(|_| None).collect();
            for u in units {
                v[*u] = Some(());
            }
            v
        };
        let w = gap_warning(&filled(&[0, 1, 2, 5])).expect("D: and E: are gaps");
        assert!(w.contains("D:") && w.contains("E:"), "{w}");
        // Generated text goes on a 40-column PETSCII screen too, and the three
        // spaces it is indented by count against it.
        assert!(w.len() + 3 <= 40, "{} chars will wrap on PETSCII: {w:?}", w.len() + 3);
        // Contiguous drives have no gap, and neither does a lone boot disk.
        assert_eq!(gap_warning(&filled(&[0, 1, 2])), None);
        assert_eq!(gap_warning(&filled(&[0])), None);
        assert_eq!(gap_warning::<()>(&[]), None);
        // A gap *above* the last disk is not a gap — nothing is beyond it.
        assert_eq!(gap_warning(&filled(&[0, 1])), None);
    }
}
