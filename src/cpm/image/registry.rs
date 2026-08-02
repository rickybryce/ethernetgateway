//! The process-wide table of which image is mounted on which drive, and which
//! drives CP/M sessions are currently sitting on.
//!
//! # Why the table is global
//!
//! Every CP/M session shares one set of drive folders, and now one set of
//! mounted images.  Two sessions writing the same image through *separate*
//! handles would each keep their own idea of the directory and each write it
//! back, and the loser's files would vanish — a corruption the per-file
//! `CPM_WRITERS` claim in `cpm/fs.rs` cannot prevent, because it guards files
//! within a filesystem and this is damage to the filesystem itself.  So an
//! image is opened once and shared: one [`ImageFs`], behind one mutex, however
//! many sessions reach it.
//!
//! # Why mounting is live
//!
//! A mount or unmount takes effect immediately, in every session, including
//! ones already inside the emulator.  That is safe for a reason worth stating,
//! because it is not obvious: a session that is *using* a drive is holding an
//! `Arc` to its `ImageFs`, and replacing the table entry does not disturb that.
//! The old filesystem stays alive and consistent until the last user of it lets
//! go.  There is no moment at which a half-finished operation finds its disk
//! swapped underneath it.
//!
//! So the in-use check below is **not** what makes this correct — the `Arc` and
//! the mutex are.  What the check prevents is *confusion*: someone sitting at
//! `B>` who suddenly finds a different disk there, or who unmounts what they
//! are working on and cannot understand why their next `DIR` disagrees.  That
//! is a real thing to protect people from, but it is a usability guard, and it
//! is worth being clear about which kind of guard it is.

use super::fs::ImageFs;
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use crate::cpm::fs::NUM_DRIVES;

/// One mounted image.
#[derive(Clone)]
pub struct Mount {
    /// Absolute path of the image file.
    pub path: PathBuf,
    /// Bare filename, as the UIs show it.
    pub filename: String,
    /// Format token it was mounted as.
    pub format: &'static str,
    /// True when this mount refuses writes — either the image was identified by
    /// sniffing rather than by name, the file itself is read-only, or the
    /// directory arrived damaged.
    pub read_only: bool,
    /// Why it is read-only, for the UIs to explain.  Empty when it is not.
    pub read_only_reason: String,
    /// The shared filesystem.  Cloning a `Mount` clones this handle, which is
    /// the point: everyone gets the same one.
    pub fs: Arc<Mutex<ImageFs>>,
}

impl std::fmt::Debug for Mount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mount")
            .field("filename", &self.filename)
            .field("format", &self.format)
            .field("read_only", &self.read_only)
            .finish()
    }
}

/// Drive A:–P:, each either empty or holding a mounted image.
type Table = Vec<Option<Mount>>;

fn table() -> &'static RwLock<Table> {
    static TABLE: OnceLock<RwLock<Table>> = OnceLock::new();
    TABLE.get_or_init(|| RwLock::new(vec![None; NUM_DRIVES as usize]))
}

/// What one live CP/M session is doing with the drives.
#[derive(Debug, Clone, Default)]
struct SessionUse {
    /// The drive its prompt is sitting on.
    current: u8,
    /// Drives it has an unfinished write on.
    writing: BTreeSet<u8>,
}

fn sessions() -> &'static Mutex<HashMap<u64, SessionUse>> {
    static SESSIONS: OnceLock<Mutex<HashMap<u64, SessionUse>>> = OnceLock::new();
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Poisoning is not a reason to stop serving CP/M sessions: a panic in one
/// session's BDOS call must not take the mount table out for everybody.
macro_rules! lock {
    ($m:expr) => {
        $m.lock().unwrap_or_else(|e| e.into_inner())
    };
}

// ---- mount table --------------------------------------------------------
//
// `all` and `any_mounted`, and the `path` field, are read by the mount
// screens and by code that reports on a mount.

/// The image mounted on a drive, if any.
pub fn get(drive0: u8) -> Option<Mount> {
    let t = table().read().unwrap_or_else(|e| e.into_inner());
    t.get(drive0 as usize).and_then(|m| m.clone())
}

/// Every mount, for the UIs.  Index 0 is A:.
pub fn all() -> Vec<Option<Mount>> {
    table().read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Put a mount on a drive, replacing whatever was there.
///
/// The caller has already opened and identified the image; this only publishes
/// it.  Refuses a drive letter past P:.
pub fn mount(drive0: u8, mount: Mount) -> Result<(), String> {
    if drive0 >= NUM_DRIVES {
        return Err(format!("no such drive ({drive0})"));
    }
    let mut t = table().write().unwrap_or_else(|e| e.into_inner());
    t[drive0 as usize] = Some(mount);
    Ok(())
}

/// Take the mount off a drive, returning what was there.
///
/// The drive's host folder becomes visible again — its files were never
/// touched while the image was mounted.  Any session still using the image
/// keeps a working handle to it until it lets go; see the module comment.
pub fn unmount(drive0: u8) -> Option<Mount> {
    let mut t = table().write().unwrap_or_else(|e| e.into_inner());
    t.get_mut(drive0 as usize).and_then(|slot| slot.take())
}

/// Drop every mount.  Used when the emulator is disabled.
pub fn clear_all() {
    let mut t = table().write().unwrap_or_else(|e| e.into_inner());
    for slot in t.iter_mut() {
        *slot = None;
    }
}

// ---- who is using what --------------------------------------------------

/// How a drive is being used right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    /// Sessions whose prompt is on this drive.
    pub sitting: usize,
    /// Sessions part-way through a write to this drive.  A mount change here
    /// is the one that would actually confuse a running program.
    pub writing: usize,
}

impl Usage {
    /// A short phrase for the UIs, or `None` when the drive is idle.
    pub fn describe(&self) -> Option<String> {
        if self.writing > 0 {
            Some(format!(
                "{} session{} writing",
                self.writing,
                if self.writing == 1 { "" } else { "s" }
            ))
        } else if self.sitting > 0 {
            Some(format!(
                "in use by {} session{}",
                self.sitting,
                if self.sitting == 1 { "" } else { "s" }
            ))
        } else {
            None
        }
    }
}

/// Current usage of every drive.  Index 0 is A:.
pub fn usage() -> Vec<Usage> {
    let mut out = vec![Usage::default(); NUM_DRIVES as usize];
    for use_ in lock!(sessions()).values() {
        if let Some(u) = out.get_mut(use_.current as usize) {
            u.sitting += 1;
        }
        for d in &use_.writing {
            if let Some(u) = out.get_mut(*d as usize) {
                u.writing += 1;
            }
        }
    }
    out
}

/// Usage of one drive.
pub fn usage_of(drive0: u8) -> Usage {
    usage().get(drive0 as usize).copied().unwrap_or_default()
}

/// Refuse a mount change while somebody is on the drive.
///
/// Not a correctness guard — see the module comment — but the difference
/// between an operator changing a disk and a user watching their disk change
/// under them.
pub fn check_can_change(drive0: u8) -> Result<(), String> {
    if lock!(borrowed()).contains_key(&drive0) {
        return Err(format!(
            "drive {}: is held by a booted disk — it comes back when that session ends",
            (b'A' + drive0) as char
        ));
    }
    let u = usage_of(drive0);
    match u.describe() {
        Some(what) => Err(format!(
            "drive {}: is {} — try again once that session leaves CP/M",
            (b'A' + drive0) as char,
            what
        )),
        None => Ok(()),
    }
}

/// Drop every mount *and* every session record.
///
/// Tests need both: `CpmFs::new` registers a session sitting on drive A:, and a
/// session left registered by another test makes A: look busy, which makes
/// `check_can_change` refuse a mount there.  Clearing only the mounts left that
/// half in place and produced a failure that appeared only in a full run.
#[cfg(test)]
pub fn tests_reset() {
    clear_all();
    lock!(sessions()).clear();
    // Not part of `clear_all`: a loan belongs to a live booted session, which
    // will end it itself.  Disabling the emulator mid-boot must not make a lent
    // drive look folder-backed again to a session still inside it.
    lock!(borrowed()).clear();
    lock!(booted_images()).clear();
}

/// The registry is process-global, so tests that touch it must not run beside
/// each other.  Same pattern as the config tests.
#[cfg(test)]
pub fn tests_lock() -> std::sync::MutexGuard<'static, ()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

// ---- session bookkeeping ------------------------------------------------

/// Images a booted session is running, by canonical path.
///
/// This lives here rather than in the boot module because [`mount_image`] has
/// to be able to ask.  A booted image that is *not* mounted anywhere has no
/// loan and no busy mark, so nothing stopped a second session mounting the same
/// file on a drive and writing to it — and the boot's write-back then replaced
/// the whole file, discarding that work and leaving the new mount's cached
/// directory describing bytes that no longer exist.  The rule was "one session
/// per image", enforced boot-against-boot only.
fn booted_images() -> &'static Mutex<std::collections::HashSet<PathBuf>> {
    static B: OnceLock<Mutex<std::collections::HashSet<PathBuf>>> = OnceLock::new();
    B.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

/// Claim an image for a booted session, returning the key it was filed under,
/// or `None` if somebody already has it.
///
/// The key is canonicalised: boot targets and mount paths are built from one
/// config value by two routes and only one of them canonicalises, so comparing
/// them raw would let the same file be claimed twice under two names.
///
/// **The caller must keep the key and release *that*.** Canonicalising again
/// later is not the same operation: it needs the file to still be there, and
/// falls back to the raw path when it is not. With the shipped relative
/// `transfer_dir`, an image deleted while it was booted would therefore be
/// released under a different key than it was claimed under — leaking the
/// claim, so that image could never be booted or mounted again without a
/// restart.
pub fn claim_booted_image(path: &Path) -> Option<PathBuf> {
    let key = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    lock!(booted_images()).insert(key.clone()).then_some(key)
}

/// Release a booted image, by the key [`claim_booted_image`] handed back.
pub fn release_booted_image(key: &Path) {
    lock!(booted_images()).remove(key);
}

/// Is this image being run by a booted session right now?
pub fn is_image_booted(path: &Path) -> bool {
    let key = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    lock!(booted_images()).contains(&key)
}

/// Drives lent to a booted session, as `drive -> filename`.
///
/// A booted guest is given the *bytes* of a mounted image and rewrites the file
/// wholesale when it leaves, so the live [`Mount`] — which caches a directory
/// and an allocation bitmap — has to go out of service while that happens.
/// Simply removing it is not enough, though: a drive that reads as empty is a
/// drive the configuration screens will happily persist as empty, and an
/// operator would find `cpm_mounts` quietly shortened after somebody booted a
/// disk.  So a lent drive is *recorded* rather than forgotten.
fn borrowed() -> &'static Mutex<std::collections::HashMap<u8, String>> {
    static B: OnceLock<Mutex<std::collections::HashMap<u8, String>>> = OnceLock::new();
    B.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// Take a mount out of service for a booted session, remembering it.
///
/// Atomic with respect to [`check_can_change`]: the drive is recorded as lent
/// before the lock on the mount table is released, so there is no instant in
/// which it looks like a free drive somebody else may mount over.  That window
/// is not theoretical — another session entering the emulator re-applies
/// `cpm_mounts`, and would open a second, live `ImageFs` on the very file the
/// booted guest is about to rewrite.
pub fn lend_for_boot(drive0: u8) -> Option<Mount> {
    // The loan is recorded while the mount table is still locked, so no reader
    // can see the drive as free-and-unlent.
    let mut lent = lock!(borrowed());
    let mut t = table().write().unwrap_or_else(|e| e.into_inner());
    let mount = t.get_mut(drive0 as usize).and_then(|slot| slot.take())?;
    lent.insert(drive0, mount.filename.clone());
    Some(mount)
}

/// The drive an image is already mounted on, if any.
///
/// Mounting one file twice gives two independent `ImageFs` objects over it,
/// each with its own cached directory and allocation bitmap, and a write
/// through either leaves the other stale.  A lent drive counts: it is still the
/// operator's mount, just out of service.
pub fn drive_holding(filename: &str) -> Option<u8> {
    let t = table().read().unwrap_or_else(|e| e.into_inner());
    if let Some(i) = t.iter().position(|m| m.as_ref().is_some_and(|m| m.filename == filename)) {
        return Some(i as u8);
    }
    drop(t);
    boot_loans().into_iter().find(|(_, n)| n == filename).map(|(d, _)| d)
}

/// Is this drive lent to a booted session?
pub fn is_lent(drive0: u8) -> bool {
    lock!(borrowed()).contains_key(&drive0)
}

/// Record a loan without a mount behind it, for tests that need a drive to
/// read as lent.
#[cfg(test)]
pub fn note_loan_for_tests(drive0: u8, filename: &str) {
    lock!(borrowed()).insert(drive0, filename.to_string());
}

/// Stop recording a drive as lent.  The caller mounts it again itself.
pub fn end_boot_loan(drive0: u8) -> Option<String> {
    lock!(borrowed()).remove(&drive0)
}

/// Drives currently lent to booted sessions, as `(drive, filename)`.
pub fn boot_loans() -> Vec<(u8, String)> {
    let mut v: Vec<(u8, String)> = lock!(borrowed()).iter().map(|(d, n)| (*d, n.clone())).collect();
    v.sort_by_key(|(d, _)| *d);
    v
}

/// Hand out a session id.
///
/// One allocator, shared by the emulator's `CpmFs` and the boot path, because
/// the table below is keyed by this number: two counters would eventually issue
/// the same id and have a booted disk and an emulator session clear each
/// other's drive bookkeeping.
pub fn next_session_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
}

/// Register a CP/M session as present, sitting on drive A:.
pub fn session_start(id: u64) {
    lock!(sessions()).insert(id, SessionUse::default());
}

/// Forget a session.  Called when its `CpmFs` drops, which is what guarantees
/// a drive cannot stay marked busy after the user has gone.
pub fn session_end(id: u64) {
    lock!(sessions()).remove(&id);
}

/// Note that a session's prompt has moved to another drive.
pub fn session_select(id: u64, drive0: u8) {
    lock!(sessions()).entry(id).or_default().current = drive0;
}

/// Note that a session has started writing on a drive.
pub fn session_writing(id: u64, drive0: u8) {
    lock!(sessions()).entry(id).or_default().writing.insert(drive0);
}

/// Note that a session has finished writing on a drive.
pub fn session_done_writing(id: u64, drive0: u8) {
    if let Some(u) = lock!(sessions()).get_mut(&id) {
        u.writing.remove(&drive0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry_lock() -> std::sync::MutexGuard<'static, ()> {
        super::tests_lock()
    }

    fn reset() {
        clear_all();
        lock!(sessions()).clear();
        // The two tables `clear_all` deliberately leaves alone, because they
        // belong to live boot sessions.  A test starting with a loan or a claim
        // left over from another would see `check_can_change` refuse a free
        // drive.
        lock!(borrowed()).clear();
        lock!(booted_images()).clear();
    }

    /// Hammer every entry point that touches the mount table and the loan
    /// table from several threads at once, and require the lot to finish.
    ///
    /// `lend_for_boot` is the only place that holds one of those locks while
    /// taking the other, so a lock-order inversion added later is the defect
    /// this shape of code invites — and a deadlock is not something reading the
    /// diff reliably catches.  The deadline is the assertion: if the ordering
    /// is ever inverted this stops finishing rather than failing an assert, so
    /// the test reports it instead of hanging the suite.
    #[test]
    fn test_the_mount_and_loan_locks_do_not_deadlock() {
        let _g = registry_lock();
        reset();

        let (tx, rx) = std::sync::mpsc::channel();
        let mut handles = Vec::new();
        for t in 0..6u8 {
            let tx = tx.clone();
            handles.push(std::thread::spawn(move || {
                for i in 0..3_000u32 {
                    let d = ((t as u32 + i) % NUM_DRIVES as u32) as u8;
                    match i % 6 {
                        0 => {
                            let _ = lend_for_boot(d);
                        }
                        1 => {
                            let _ = end_boot_loan(d);
                        }
                        2 => {
                            let _ = boot_loans();
                        }
                        3 => {
                            let _ = check_can_change(d);
                        }
                        4 => {
                            let _ = all();
                        }
                        _ => {
                            let _ = unmount(d);
                        }
                    }
                }
                let _ = tx.send(());
            }));
        }
        drop(tx);
        for n in 0..6 {
            rx.recv_timeout(std::time::Duration::from_secs(30)).unwrap_or_else(|_| {
                panic!("thread {n} never finished — the mount and loan locks deadlocked")
            });
        }
        for h in handles {
            h.join().unwrap();
        }
        reset();
    }

    /// The loan table has to behave at its edges: a drive that is not mounted
    /// cannot be lent, a drive already lent cannot be lent twice, and a drive
    /// number past the end is refused rather than panicking.
    #[test]
    fn test_loan_edges() {
        let _g = registry_lock();
        reset();
        assert!(lend_for_boot(0).is_none(), "nothing mounted, nothing to lend");
        assert!(lend_for_boot(200).is_none(), "a wild drive number must not panic");
        assert!(end_boot_loan(200).is_none());
        assert!(boot_loans().is_empty());
        assert!(check_can_change(0).is_ok());
        reset();
    }

    #[test]
    fn test_usage_is_empty_with_no_sessions() {
        let _g = registry_lock();
        reset();
        assert!(usage().iter().all(|u| u.describe().is_none()));
        assert!(check_can_change(0).is_ok());
    }

    #[test]
    fn test_a_session_marks_its_current_drive_busy() {
        let _g = registry_lock();
        reset();
        session_start(1);
        session_select(1, 2); // C:
        assert!(usage_of(2).describe().is_some());
        assert!(usage_of(0).describe().is_none(), "other drives stay free");
        let err = check_can_change(2).unwrap_err();
        assert!(err.contains("drive C:"), "{err}");
        reset();
    }

    #[test]
    fn test_leaving_releases_the_drive() {
        let _g = registry_lock();
        reset();
        session_start(1);
        session_select(1, 1);
        assert!(check_can_change(1).is_err());
        session_end(1);
        assert!(
            check_can_change(1).is_ok(),
            "a drive must not stay busy after the session goes"
        );
        reset();
    }

    #[test]
    fn test_writing_is_reported_ahead_of_sitting() {
        let _g = registry_lock();
        reset();
        session_start(1);
        session_select(1, 0);
        session_writing(1, 0);
        let u = usage_of(0);
        assert_eq!(u.writing, 1);
        assert_eq!(u.describe().unwrap(), "1 session writing");
        session_done_writing(1, 0);
        assert_eq!(usage_of(0).writing, 0);
        assert_eq!(usage_of(0).describe().unwrap(), "in use by 1 session");
        reset();
    }

    #[test]
    fn test_several_sessions_are_counted() {
        let _g = registry_lock();
        reset();
        for id in 1..=3 {
            session_start(id);
            session_select(id, 4);
        }
        assert_eq!(usage_of(4).sitting, 3);
        assert_eq!(usage_of(4).describe().unwrap(), "in use by 3 sessions");
        reset();
    }

    /// A session that ends without tidying up must not leave a drive pinned.
    #[test]
    fn test_session_end_clears_write_marks_too() {
        let _g = registry_lock();
        reset();
        session_start(1);
        session_writing(1, 3);
        assert!(usage_of(3).describe().is_some());
        session_end(1);
        assert!(usage_of(3).describe().is_none());
        reset();
    }

    #[test]
    fn test_mount_and_unmount_a_drive() {
        let _g = registry_lock();
        reset();
        assert!(all().iter().all(|m| m.is_none()));
        assert!(get(1).is_none());
        assert!(
            mount(NUM_DRIVES, dummy_mount()).is_err(),
            "P: is the last drive"
        );
        mount(1, dummy_mount()).unwrap();
        assert!(all().iter().any(|m| m.is_some()));
        assert_eq!(get(1).unwrap().filename, "test.dsk");
        let gone = unmount(1).expect("clear returns what was there");
        assert_eq!(gone.filename, "test.dsk");
        assert!(get(1).is_none());
        assert!(all().iter().all(|m| m.is_none()));
        reset();
    }

    #[test]
    fn test_clear_all_empties_every_drive() {
        let _g = registry_lock();
        reset();
        mount(0, dummy_mount()).unwrap();
        mount(5, dummy_mount()).unwrap();
        clear_all();
        assert!(all().iter().all(|m| m.is_none()));
        reset();
    }

    /// Mounting on top of a drive replaces the old mount rather than stacking.
    #[test]
    fn test_mounting_replaces_rather_than_stacking() {
        let _g = registry_lock();
        reset();
        mount(0, dummy_mount()).unwrap();
        let mut second = dummy_mount();
        second.filename = "other.dsk".into();
        mount(0, second).unwrap();
        assert_eq!(get(0).unwrap().filename, "other.dsk");
        assert_eq!(all().iter().filter(|m| m.is_some()).count(), 1);
        reset();
    }

    fn dummy_mount() -> Mount {
        use super::super::format::by_token;
        use super::super::media::MemMedia;
        let fmt = by_token("ibm3740").unwrap();
        let img = vec![0xE5u8; fmt.min_bytes() as usize];
        let fs = ImageFs::mount(Box::new(MemMedia::new(img)), fmt, true).unwrap();
        Mount {
            path: PathBuf::from("/tmp/test.dsk"),
            filename: "test.dsk".into(),
            format: fmt.token,
            read_only: true,
            read_only_reason: String::new(),
            fs: Arc::new(Mutex::new(fs)),
        }
    }
}
