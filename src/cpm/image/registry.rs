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
use std::path::PathBuf;
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

/// The image mounted on a drive, if any.
pub fn get(drive0: u8) -> Option<Mount> {
    let t = table().read().unwrap_or_else(|e| e.into_inner());
    t.get(drive0 as usize).and_then(|m| m.clone())
}

/// Every mount, for the UIs.  Index 0 is A:.
pub fn all() -> Vec<Option<Mount>> {
    table().read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// True if any drive has an image mounted.
pub fn any_mounted() -> bool {
    all().iter().any(|m| m.is_some())
}

/// Put a mount on a drive, replacing whatever was there.
///
/// The caller has already opened and identified the image; this only publishes
/// it.  Refuses a drive letter past P:.
pub fn set(drive0: u8, mount: Mount) -> Result<(), String> {
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
pub fn clear(drive0: u8) -> Option<Mount> {
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
    /// True when anything at all is using the drive.
    pub fn busy(&self) -> bool {
        self.sitting > 0 || self.writing > 0
    }

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

// ---- session bookkeeping ------------------------------------------------

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

    /// The registry is global, so tests that touch it must not run beside each
    /// other.  Same pattern as the config tests.
    fn registry_lock() -> std::sync::MutexGuard<'static, ()> {
        static L: OnceLock<Mutex<()>> = OnceLock::new();
        L.get_or_init(|| Mutex::new(())).lock().unwrap_or_else(|e| e.into_inner())
    }

    fn reset() {
        clear_all();
        lock!(sessions()).clear();
    }

    #[test]
    fn test_usage_is_empty_with_no_sessions() {
        let _g = registry_lock();
        reset();
        assert!(usage().iter().all(|u| !u.busy()));
        assert!(check_can_change(0).is_ok());
    }

    #[test]
    fn test_a_session_marks_its_current_drive_busy() {
        let _g = registry_lock();
        reset();
        session_start(1);
        session_select(1, 2); // C:
        assert!(usage_of(2).busy());
        assert!(!usage_of(0).busy(), "other drives stay free");
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
        assert!(usage_of(3).busy());
        session_end(1);
        assert!(!usage_of(3).busy());
        reset();
    }

    #[test]
    fn test_set_and_clear_a_drive() {
        let _g = registry_lock();
        reset();
        assert!(!any_mounted());
        assert!(get(1).is_none());
        assert!(
            set(NUM_DRIVES, dummy_mount()).is_err(),
            "P: is the last drive"
        );
        set(1, dummy_mount()).unwrap();
        assert!(any_mounted());
        assert_eq!(get(1).unwrap().filename, "test.dsk");
        let gone = clear(1).expect("clear returns what was there");
        assert_eq!(gone.filename, "test.dsk");
        assert!(get(1).is_none());
        assert!(!any_mounted());
        reset();
    }

    #[test]
    fn test_clear_all_empties_every_drive() {
        let _g = registry_lock();
        reset();
        set(0, dummy_mount()).unwrap();
        set(5, dummy_mount()).unwrap();
        clear_all();
        assert!(!any_mounted());
        reset();
    }

    /// Mounting on top of a drive replaces the old mount rather than stacking.
    #[test]
    fn test_set_replaces() {
        let _g = registry_lock();
        reset();
        set(0, dummy_mount()).unwrap();
        let mut second = dummy_mount();
        second.filename = "other.dsk".into();
        set(0, second).unwrap();
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
