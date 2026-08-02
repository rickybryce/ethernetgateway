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
//! * **Read-only unless asked.** The guest can write anywhere inside an image,
//!   and nothing above it interprets the format well enough to notice a
//!   mistake. Protection is the default; writing is a decision.
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
use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

/// Images currently booted, by path, so one is never run twice at once.
fn booted() -> &'static Mutex<HashSet<std::path::PathBuf>> {
    static B: OnceLock<Mutex<HashSet<std::path::PathBuf>>> = OnceLock::new();
    B.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Claims an image for one session and releases it however the session ends.
///
/// RAII rather than a matched pair of calls: a boot can leave through an error,
/// a dropped connection or the shutdown broadcast, and an image left claimed
/// after any of those could never be booted again without a restart.
struct BootClaim(std::path::PathBuf);

impl Drop for BootClaim {
    fn drop(&mut self) {
        booted().lock().unwrap_or_else(|e| e.into_inner()).remove(&self.0);
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
        let key = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let mut held = booted().lock().unwrap_or_else(|e| e.into_inner());
        if held.contains(&key) {
            return None;
        }
        held.insert(key.clone());
        Some(BootClaim(key))
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
/// * unit 0 is the disk being booted and is not in the plan — the bootstrap can
///   load a system from any unit (measured), but the system it loads comes up
///   as its own A: and reads unit 0 from then on, so a boot disk parked
///   anywhere else runs against whatever is in unit 0 and looks like a hang;
/// * every other mount rides the unit its drive letter names, because that is
///   the only mapping under which the letters an operator chose mean anything
///   to the guest;
/// * a mount is writable only when the boot session is writable *and* the
///   mount is — the stricter of the two wins, since this path writes raw
///   sectors and nothing above it understands the format well enough to catch
///   a mistake;
/// * a drive another session is working in is left out, and so is the boot
///   image appearing twice, which would be two views of one file with separate
///   write-backs and the last one saved winning.
///
/// Returns the plan and the lines to show for whatever was left out.
fn plan_boot_disks(
    mounts: &[Option<crate::cpm::image::registry::Mount>],
    usage: &[crate::cpm::image::registry::Usage],
    boot_image: &std::path::Path,
    writable: bool,
    units: usize,
) -> (Vec<BootPlanStep>, Vec<String>) {
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
            writable: writable && !mount.read_only,
            lend_only: false,
        });
    }
    (plan, notes)
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
/// So it is RAII, for the same reason [`BootClaim`] is. The remount is the
/// synchronous `mount_image` because `Drop` cannot await; it opens one file and
/// reads a directory, at session teardown, which is a fair price for a
/// guarantee that holds when the connection has already gone.
struct RemountOnDrop {
    base: std::path::PathBuf,
    /// `(drive, bare filename)` for each mount taken.
    taken: Vec<(u8, String)>,
}

impl Drop for RemountOnDrop {
    fn drop(&mut self) {
        for (drive, name) in std::mem::take(&mut self.taken) {
            // End the loan first: a lent drive refuses a mount change, and this
            // session is the one holding it.
            // Restoring is not a mount *change*, so it is not something another
            // session may veto.  `mount_image` refuses a drive that reads as in
            // use, and a lent drive reads as empty — so anyone who parked on it
            // meanwhile could otherwise block the restore, and the drive would
            // end up neither mounted nor lent, which drops it from
            // `cpm_mounts` on the next save from any screen.
            crate::cpm::image::registry::end_boot_loan(drive);
            if let Err(e) = crate::cpm::image::restore_mount(&self.base, drive, &name) {
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
    tokio::fs::write(&tmp, bytes).await?;
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
    ) -> Vec<String> {
        use crate::cpm::image::registry;
        let (plan, mut notes) = plan_boot_disks(
            &registry::all(),
            &registry::usage(),
            boot_image,
            writable,
            disks.len(),
        );
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
                notes.push(format!("{letter}: {name} is booted elsewhere."));
                continue;
            };
            let bytes = match std::fs::read(&step.path) {
                Ok(b) => b,
                Err(e) => {
                    notes.push(format!("{letter}: cannot read {name} ({e})."));
                    continue;
                }
            };
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
            notes.push(format!(
                "{letter}: {name}{}",
                if step.writable { "" } else { " (R/O)" }
            ));
        }
        notes.extend(gap_warning(disks));
        notes
    }

    /// Boot an image and run it until the guest stops or the user leaves.
    ///
    /// `image` is the host path; `writable` decides whether changes are kept.
    pub(in crate::telnet) async fn cpm_boot_session(
        &mut self,
        image: &std::path::Path,
        writable: bool,
    ) -> Result<(), std::io::Error> {
        let Some(claim) = BootClaim::take(image) else {
            self.send_line(&format!(
                "  {}",
                self.red("That image is already running in another session.")
            ))
            .await?;
            self.send_line(&format!(
                "  {}",
                self.dim("A booted disk owns its drives, so only one session")
            ))
            .await?;
            self.send_line(&format!("  {}", self.dim("can have it at a time."))).await?;
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
        // Unit 0 is the disk being booted, and it has to be: the bootstrap can
        // load a system from any unit — that was measured — but the operating
        // system it loads comes up as its own A: and reads unit 0 from then on.
        // Booting a disk parked anywhere else therefore loads fine and then
        // runs against whatever happens to be in unit 0, which looks like a
        // hang rather than a mistake.
        // Declared before anything can take a mount, and before `busy`, so it
        // drops *after* the drives are released — remounting a drive this
        // session still holds busy is refused.
        let mut remounts = RemountOnDrop { base: self.cpmmount_base(), taken: Vec::new() };
        let mut disks: Vec<Option<BootDisk>> = (0..MAX_BOOT_UNITS).map(|_| None).collect();
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

        // Everything else mounted comes along, at the unit its drive letter
        // names: B: is unit 1, C: is unit 2, and so on.  What the guest calls
        // them is the guest's business — its BIOS names the units it knows
        // about and refuses the rest, which for stock Altair CP/M means A: to
        // D:.  We present the hardware and let the disk decide, exactly as with
        // everything else on this path.
        let notes =
            self.cpm_boot_attach_mounts(&mut machine, &mut disks, writable, &mut remounts, image);
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
        let mut modem = CpmModem::new(matches!(attach, ModemAttach::Ports(_, _)));
        modem.set_menu_context(self.shutdown.clone(), self.restart.clone(), self.lockouts.clone());
        // Joins the inbound `CPM@<ip>` pool for as long as the boot lasts, so a
        // booted guest is dialable exactly as an emulator session is.
        let _peer_reg = cpm_peer_register(modem.enabled());

        let mut cpu = BootMachine::new_cpu();
        if let Err(e) = machine.boot(&mut cpu, 0) {
            self.send_line(&format!("  {}", self.red(&e.to_string()))).await?;
            self.send_line("").await?;
            return Ok(());
        }

        let name = image.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        self.send_line("").await?;
        self.send_line(&format!("  {} {}", self.green("Booted"), self.amber(&name)))
            .await?;
        self.send_line(&format!(
            "  {}",
            self.dim(if writable { "Changes are saved." } else { "Read-only." })
        ))
        .await?;
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

        let result = self.cpm_boot_run(&mut cpu, &mut machine, &mut modem).await;

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

    /// The run loop: step the CPU, move console bytes both ways.
    async fn cpm_boot_run(
        &mut self,
        cpu: &mut Cpu,
        machine: &mut BootMachine,
        modem: &mut CpmModem,
    ) -> Result<(), std::io::Error> {
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
            cpu.execute_instruction(machine);
            executed += 1;

            if executed.is_multiple_of(KEY_POLL_INTERVAL) {
                // Everything the guest printed since the last seam, in one
                // write.  Draining per instruction instead would be a syscall
                // per character — a guest printing a directory listing would
                // make two thousand of them — and a seam is a fifth of a
                // millisecond, so nothing a person could perceive is lost.
                // It comes first in the seam so that output is always on its
                // way out before the idle nap below.
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
                    // The guest is an ASCII machine, so a Commodore's keys are
                    // folded on the way in as its output is folded on the way
                    // out.
                    machine.send_key(if is_petscii { petscii_to_ascii_byte(b) } else { b });
                }
                if keys > 0 {
                    last_key = tokio::time::Instant::now();
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
                        self.red("Stopped: the guest is waiting for a sector that never arrives.")
                    ))
                    .await?;
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

    /// One session per image, and the claim comes back however the session
    /// ends — a claim leaked by an error path could never be booted again
    /// without restarting the gateway.
    #[test]
    fn test_an_image_can_only_be_booted_once_at_a_time() {
        let p = std::path::Path::new("/tmp/egw_boot_claim_test.dsk");
        let first = BootClaim::take(p).expect("first claim");
        assert!(BootClaim::take(p).is_none(), "a second session must be refused");
        drop(first);
        assert!(BootClaim::take(p).is_some(), "the claim returns when the session ends");
    }

    /// Different images do not block each other.
    #[test]
    fn test_two_different_images_can_run_together() {
        let a = BootClaim::take(std::path::Path::new("/tmp/egw_boot_a.dsk")).unwrap();
        let b = BootClaim::take(std::path::Path::new("/tmp/egw_boot_b.dsk"));
        assert!(b.is_some(), "separate images are independent");
        drop(a);
    }

    /// The poll interval must divide the yield interval, or the key check and
    /// the yield drift apart and one of them effectively stops happening.
    #[test]
    fn test_the_loop_intervals_line_up() {
        assert!(
            YIELD_INTERVAL.is_multiple_of(KEY_POLL_INTERVAL),
            "the yield must fall on a key-poll boundary, or the two drift apart \
             and one of them effectively stops happening"
        );
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

    /// The mapping the whole feature rests on: a mounted image rides the unit
    /// its drive letter names.  Anything else and the letters an operator chose
    /// stop meaning what they say.
    #[test]
    fn test_mounts_ride_the_unit_their_letter_names() {
        let m = mounts(&[(0, "boot.dsk", false), (1, "b.dsk", false), (5, "f.dsk", false)]);
        let (plan, _) = plan_boot_disks(&m, &idle(), Path::new("/images/boot.dsk"), true, 16);
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
        let (plan, _) = plan_boot_disks(&m, &idle(), Path::new("/images/boot.dsk"), true, 16);
        assert_eq!(plan.len(), 1);
        assert!(plan[0].lend_only, "the boot disk's own mount is lent, not inserted");

        // A: holds something else — it is shadowed, and the operator is told.
        // It is not touched: it stays mounted and usable elsewhere.
        let m = mounts(&[(0, "other.dsk", false)]);
        let (plan, notes) = plan_boot_disks(&m, &idle(), Path::new("/images/boot.dsk"), true, 16);
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
        let (plan, notes) = plan_boot_disks(&m, &idle(), Path::new("/images/boot.dsk"), true, 16);
        assert_eq!(plan.len(), 1);
        assert!(plan[0].lend_only, "taken out of service, not given a unit");
        assert_eq!(plan[0].unit, 3, "lent from the drive it is actually on");
        assert!(notes.iter().any(|n| n.contains("boot disk")), "{notes:?}");
    }

    /// Two ways to be read-only, and the stricter wins.  This path writes raw
    /// sectors with nothing above it able to notice a mistake, so a session the
    /// operator did not open for writing must not write anywhere.
    #[test]
    fn test_read_only_is_the_stricter_of_mount_and_session() {
        let m = mounts(&[(1, "rw.dsk", false), (2, "ro.dsk", true)]);
        let w = |writable| {
            plan_boot_disks(&m, &idle(), Path::new("/images/boot.dsk"), writable, 16)
                .0
                .iter()
                .map(|s| (s.unit, s.writable))
                .collect::<Vec<_>>()
        };
        assert_eq!(w(true), vec![(1, true), (2, false)], "a R/O mount stays R/O");
        assert_eq!(
            w(false),
            vec![(1, false), (2, false)],
            "a read-only boot session must not write a writable mount"
        );
    }

    /// A drive another session is working in must not be handed to a guest that
    /// owns whole platters.
    #[test]
    fn test_a_busy_drive_is_left_out() {
        let m = mounts(&[(1, "busy.dsk", false), (2, "free.dsk", false)]);
        let mut u = idle();
        u[1] = Usage { sitting: 1, writing: 0 };
        let (plan, notes) = plan_boot_disks(&m, &u, Path::new("/images/boot.dsk"), true, 16);
        assert_eq!(plan.iter().map(|s| s.unit).collect::<Vec<_>>(), vec![2]);
        assert!(notes.iter().any(|n| n.contains("in use elsewhere")), "{notes:?}");
    }

    /// A machine with fewer units than sixteen must not be handed a plan that
    /// runs off the end of it.
    #[test]
    fn test_the_plan_respects_the_unit_count() {
        let m = mounts(&[(1, "b.dsk", false), (9, "j.dsk", false)]);
        let (plan, _) = plan_boot_disks(&m, &idle(), Path::new("/images/boot.dsk"), true, 4);
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
            format: "altair8",
            fs: mount_at("boot.dsk", false).fs,
        });
        let (plan, notes) = plan_boot_disks(&m, &idle(), &real, true, 16);
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
            format: "altair8",
            fs: mount_at("other.dsk", false).fs,
        });
        let other_plan = plan_boot_disks(&m2, &idle(), &real, true, 16).0;
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
        // And holding nothing must register nothing at all.
        {
            let _none = BootDrivesBusy::hold(&[]);
            assert!(
                crate::cpm::image::registry::usage().iter().all(|u| u.describe().is_none()),
                "a boot that took no mounts must not mark a drive busy"
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
