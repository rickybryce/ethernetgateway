//! The FTP server's file store: every file operation an FTP session performs,
//! against the transfer directory, behind the jail.
//!
//! **One implementation of the safety rules.**  Phase 3 adds a slave's store,
//! which forwards each operation to its master -- and the master answers by
//! running *this* code against its own `transfer_dir`.  So the jail, the name
//! rule, the CP/M in-use guard, the busy-file set and the disk-full gate exist
//! once, and are always evaluated on the machine that holds the files.
//!
//! Paths in are *virtual* (`/`-rooted, normalised by `proto::resolve`, so no
//! `..` survives to here).  The jail's second half is here: every existing
//! target is canonicalised and must still be inside the canonical transfer
//! directory, which is what stops a symlink leading out.

use std::borrow::Cow;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use super::proto::Entry;
use crate::config;
use crate::telnet::TelnetSession;

/// Why an operation was refused.  Each maps to one FTP reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    NotFound,
    NotADirectory,
    NotAFile,
    /// A name being created fails the gateway's name rule.
    BadName(Cow<'static, str>),
    /// The CP/M emulator is using it (see `cpm::outside_change_refusal`).
    Locked(Cow<'static, str>),
    /// Another FTP session is writing it.
    Busy,
    Exists,
    NotEmpty,
    DiskFull,
    /// A restart offset past the end of the file.
    BadOffset,
    /// Refused for a reason given in the text.  Owned or not: on a slave the
    /// text was decided on the master and arrived over the wire.
    Denied(Cow<'static, str>),
    Io(String),
}

impl StoreError {
    /// The reply code and text.  `550` for "can't do that to that file";
    /// `553` for a name we won't create; `450`/`452` for the transient ones a
    /// client may retry; `554` for a bad restart offset (RFC 3659 §5.5).
    pub fn reply(&self) -> (u16, String) {
        match self {
            StoreError::NotFound => (550, "No such file or directory.".into()),
            StoreError::NotADirectory => (550, "Not a directory.".into()),
            StoreError::NotAFile => (550, "Not a plain file.".into()),
            StoreError::BadName(why) => (553, format!("File name not allowed: {why}.")),
            StoreError::Locked(why) => (550, why.to_string()),
            StoreError::Busy => (450, "File busy: another session is writing it.".into()),
            StoreError::Exists => (550, "Already exists.".into()),
            StoreError::NotEmpty => (550, "Directory not empty.".into()),
            StoreError::DiskFull => (452, "Disk space is low.".into()),
            StoreError::BadOffset => (554, "Restart offset is past the end of the file.".into()),
            StoreError::Denied(why) => (550, why.to_string()),
            StoreError::Io(e) => (451, format!("Local error: {e}.")),
        }
    }
}

fn io_err(e: std::io::Error) -> StoreError {
    use std::io::ErrorKind as K;
    match e.kind() {
        K::NotFound => StoreError::NotFound,
        K::AlreadyExists => StoreError::Exists,
        K::DirectoryNotEmpty => StoreError::NotEmpty,
        K::NotADirectory => StoreError::NotADirectory,
        K::IsADirectory => StoreError::NotAFile,
        // Permanent on Unix, so 550 rather than the "try again" of a 451.  Not
        // on Windows, where it is also what a file held open by another
        // program, or briefly by an antivirus scanner, looks like.
        K::PermissionDenied if !cfg!(windows) => StoreError::Denied(Cow::Borrowed("Permission denied on this machine.")),
        K::ReadOnlyFilesystem => StoreError::Denied(Cow::Borrowed("The transfer folder is read-only.")),
        K::StorageFull | K::QuotaExceeded => StoreError::DiskFull,
        _ => StoreError::Io(e.kind().to_string()),
    }
}

type Result<T> = std::result::Result<T, StoreError>;

/// How a write places its bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteMode {
    /// STOR: written beside the target and renamed over it when complete.
    Replace,
    /// APPE: added to the end, in place.
    Append,
    /// STOR after REST: written in place from this offset.
    At(u64),
    /// STOU: a name that must not exist yet.
    New,
}

/// Prefix of an in-progress `Replace` upload.  A leading dot fails the name
/// rule, so no client can address one, and listings hide it.
pub const TEMP_PREFIX: &str = ".egw-ftp-";

/// Paths an FTP session is writing right now.  A second writer, or a delete or
/// rename of one, is told the file is busy: two STORs to one name would
/// otherwise both rename over it, and an APPE beside a STOR interleaves.
fn writing() -> &'static Mutex<HashSet<PathBuf>> {
    static W: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    W.get_or_init(Default::default)
}

fn is_being_written(p: &Path) -> bool {
    let key = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    writing().lock().unwrap_or_else(|e| e.into_inner()).contains(&key)
}

/// A claim in [`writing`], released when dropped -- however the upload ends.
struct WriteClaim(PathBuf);

impl WriteClaim {
    fn take(p: &Path) -> Result<WriteClaim> {
        let mut w = writing().lock().unwrap_or_else(|e| e.into_inner());
        if !w.insert(p.to_path_buf()) {
            return Err(StoreError::Busy);
        }
        Ok(WriteClaim(p.to_path_buf()))
    }
}

impl Drop for WriteClaim {
    fn drop(&mut self) {
        writing().lock().unwrap_or_else(|e| e.into_inner()).remove(&self.0);
    }
}

/// An upload in progress.  [`Upload::finish`] puts it in place; dropping it
/// unfinished (an error, ABOR, a lost connection, a restart) removes a
/// `Replace` upload's temporary file, so a broken transfer never leaves half a
/// file under the real name -- or a stray temp beside it.
pub struct Upload {
    pub file: tokio::fs::File,
    temp: Option<PathBuf>,
    target: PathBuf,
    _claim: WriteClaim,
}

impl Upload {
    /// Flush, sync, and (for `Replace`) rename over the target.  The CP/M
    /// guard is asked again here: a disk can be mounted or booted while the
    /// bytes were arriving.
    pub async fn finish(mut self) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        self.file.flush().await.map_err(io_err)?;
        self.file.sync_all().await.map_err(io_err)?;
        if let Some(temp) = self.temp.take() {
            if let Some(why) = crate::cpm::outside_change_refusal(&self.target) {
                let _ = std::fs::remove_file(&temp);
                return Err(StoreError::Locked(Cow::Borrowed(why)));
            }
            if let Err(e) = tokio::fs::rename(&temp, &self.target).await {
                let _ = std::fs::remove_file(&temp);
                return Err(io_err(e));
            }
        }
        Ok(())
    }
}

impl Drop for Upload {
    fn drop(&mut self) {
        if let Some(temp) = self.temp.take() {
            let _ = std::fs::remove_file(temp);
        }
    }
}

/// Whether two existing paths are the same folder: by device and inode on
/// Unix (whatever the case of the name or the filesystem), by a
/// case-insensitive canonical comparison on Windows, where folders are
/// case-insensitive.
fn same_dir(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (std::fs::metadata(a), std::fs::metadata(b)) {
            (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
            (Ok(x), Ok(y)) => x.to_string_lossy().to_lowercase() == y.to_string_lossy().to_lowercase(),
            _ => false,
        }
    }
}

/// The store over this gateway's own `transfer_dir`.
#[derive(Debug, Clone, Default)]
pub struct LocalStore;

impl LocalStore {
    /// The canonical transfer directory, created if missing.  Read from the
    /// config per operation, so a changed `transfer_dir` applies at once.
    fn base() -> Result<PathBuf> {
        let dir = config::get_config().transfer_dir;
        let _ = std::fs::create_dir_all(&dir);
        std::fs::canonicalize(&dir).map_err(|_| StoreError::Denied(Cow::Borrowed("The transfer folder is unavailable.")))
    }

    /// The CP/M emulator's container under the transfer directory.
    fn cpm_container(base: &Path) -> PathBuf {
        base.join("CPM")
    }

    /// Refuse a path that is not already in clean virtual form: absolute,
    /// `/`-separated, no empty, `.` or `..` component, and nothing a host could
    /// read as more than one name -- a backslash, NUL, or on Windows a `:` (a
    /// drive or stream, which makes `PathBuf::push` replace the whole path).
    ///
    /// **Not redundant with `proto::resolve`.**  A local session's paths have
    /// been through it, but on a master they arrive from a slave over the
    /// wire, and the rename path joins a leaf that is never canonicalised:
    /// `RNFR /..` would have named the transfer directory's parent.
    fn clean(vpath: &str) -> Result<()> {
        if vpath == "/" {
            return Ok(());
        }
        let ok = vpath.starts_with('/')
            && vpath[1..].split('/').all(|c| {
                !c.is_empty()
                    && c != "."
                    && c != ".."
                    && !c.contains(['\\', '\0'])
                    // Only Windows reads ':' as a drive or stream; on Unix it
                    // is an ordinary character a real file name may hold.
                    && !(cfg!(windows) && c.contains(':'))
            });
        if ok { Ok(()) } else { Err(StoreError::Denied(Cow::Borrowed("That path is not allowed."))) }
    }

    /// The host path a virtual path names (not yet canonicalised).
    fn join(base: &Path, vpath: &str) -> PathBuf {
        let mut p = base.to_path_buf();
        for comp in vpath.split('/').filter(|c| !c.is_empty()) {
            p.push(comp);
        }
        p
    }

    /// An existing target, canonical and inside the jail.
    fn existing(base: &Path, vpath: &str) -> Result<PathBuf> {
        Self::clean(vpath)?;
        let canon = std::fs::canonicalize(Self::join(base, vpath)).map_err(io_err)?;
        if !canon.starts_with(base) {
            return Err(StoreError::Denied(Cow::Borrowed("Outside the transfer folder.")));
        }
        Ok(canon)
    }

    /// Where a new name would go: its canonical parent (an existing directory
    /// inside the jail) and the leaf, which must pass the gateway's name rule
    /// -- the same rule every other upload path applies.
    fn new_target(base: &Path, vpath: &str) -> Result<(PathBuf, String)> {
        Self::clean(vpath)?;
        let (dir, leaf) = super::proto::split_leaf(vpath);
        let leaf = leaf.ok_or(StoreError::Denied(Cow::Borrowed("That is the top folder.")))?;
        TelnetSession::validate_new_name(&leaf).map_err(|e| StoreError::BadName(Cow::Borrowed(e)))?;
        let parent = Self::existing(base, &dir)?;
        if !parent.is_dir() {
            return Err(StoreError::NotADirectory);
        }
        Ok((parent, leaf))
    }

    /// The emulator's own folders -- the `CPM` container and anything inside
    /// it that is a directory (its drive folders and its images folder) --
    /// may not be renamed or removed.  Renaming one moves mounted disks out
    /// from under their mounts; the files inside are guarded one by one.
    ///
    /// Asked of the name as given *and* of where it really leads, so neither
    /// spelling nor a link slips past: on Windows and macOS `/cpm` is the
    /// `CPM` folder (compared without case there), and a `CPM` or `CPM/images`
    /// that is a symlink -- a supported layout -- is still the emulator's.
    fn is_structural_dir(base: &Path, p: &Path) -> bool {
        if !p.is_dir() {
            return false;
        }
        let cpm = Self::cpm_container(base);
        if !cpm.is_dir() {
            return false;
        }
        // The name as given, or anything above it up to the transfer folder,
        // *is* the container -- by identity, so neither case nor a link
        // decides: `/cpm` on a case-insensitive disk is `CPM`, and a `CPM`
        // that is a symlink (a supported layout) is still the emulator's.
        let mut at = Some(p);
        while let Some(dir) = at {
            if dir == base {
                break;
            }
            if same_dir(dir, &cpm) {
                return true;
            }
            at = dir.parent();
        }
        // Where it leads: inside the container's real location.
        match (std::fs::canonicalize(p), std::fs::canonicalize(&cpm)) {
            (Ok(real), Ok(cpm_real)) => real.starts_with(&cpm_real),
            _ => false,
        }
    }

    fn entry_for(base: &Path, name: String, meta: &std::fs::Metadata, path: &Path) -> Entry {
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let locked = if meta.is_dir() {
            Self::is_structural_dir(base, path)
        } else {
            crate::cpm::outside_change_refusal(path).is_some()
        };
        Entry { name, is_dir: meta.is_dir(), len: if meta.is_dir() { 0 } else { meta.len() }, mtime, locked }
    }

    /// Run a blocking store operation off the async workers.
    async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
        tokio::task::spawn_blocking(f)
            .await
            .unwrap_or_else(|_| Err(StoreError::Io("task failed".into())))
    }

    /// Facts about one path.
    pub async fn stat(&self, vpath: &str) -> Result<Entry> {
        let vpath = vpath.to_string();
        Self::blocking(move || {
            let base = Self::base()?;
            let p = Self::existing(&base, &vpath)?;
            let meta = std::fs::metadata(&p).map_err(io_err)?;
            let name = super::proto::split_leaf(&vpath).1.unwrap_or_else(|| "/".into());
            Ok(Self::entry_for(&base, name, &meta, &p))
        })
        .await
    }

    /// A directory's entries, or the one entry a file path names (`LIST
    /// file` is legal and lists just it).  Sorted by name; our own temp files
    /// and names a listing can't carry are left out.
    pub async fn list(&self, vpath: &str) -> Result<Vec<Entry>> {
        let vpath = vpath.to_string();
        Self::blocking(move || {
            let base = Self::base()?;
            let p = Self::existing(&base, &vpath)?;
            let meta = std::fs::metadata(&p).map_err(io_err)?;
            if !meta.is_dir() {
                let name = super::proto::split_leaf(&vpath).1.unwrap_or_default();
                return Ok(vec![Self::entry_for(&base, name, &meta, &p)]);
            }
            let mut out = Vec::new();
            for e in std::fs::read_dir(&p).map_err(io_err)?.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with(TEMP_PREFIX) || !super::proto::listable(&name) {
                    continue;
                }
                let path = e.path();
                // Follows a symlink, so a link shows as what it points at;
                // reaching through it is still refused by `existing`.
                let Ok(m) = std::fs::metadata(&path) else { continue };
                out.push(Self::entry_for(&base, name, &m, &path));
            }
            out.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(out)
        })
        .await
    }

    /// Open a file to send, positioned at `offset`.
    pub async fn open_read(&self, vpath: &str, offset: u64) -> Result<tokio::fs::File> {
        let vpath = vpath.to_string();
        let file = Self::blocking(move || {
            use std::io::{Seek, SeekFrom};
            let base = Self::base()?;
            let p = Self::existing(&base, &vpath)?;
            if !p.is_file() {
                return Err(StoreError::NotAFile);
            }
            let mut f = std::fs::File::open(&p).map_err(io_err)?;
            if offset > f.metadata().map_err(io_err)?.len() {
                return Err(StoreError::BadOffset);
            }
            f.seek(SeekFrom::Start(offset)).map_err(io_err)?;
            Ok(f)
        })
        .await?;
        Ok(tokio::fs::File::from_std(file))
    }

    /// Start an upload to `vpath`.
    pub async fn begin_write(&self, vpath: &str, mode: WriteMode) -> Result<Upload> {
        let vpath = vpath.to_string();
        let (file, temp, target, claim) = Self::blocking(move || {
            use std::io::{Seek, SeekFrom};
            if TelnetSession::is_disk_full() {
                return Err(StoreError::DiskFull);
            }
            let base = Self::base()?;
            let (parent, leaf) = Self::new_target(&base, &vpath)?;
            let target = parent.join(&leaf);
            // A restart offset is checked before anything is created: REST
            // past the end (or into a file that isn't there) must not leave
            // an empty file behind.
            if let WriteMode::At(offset) = mode {
                let len = std::fs::metadata(&target).map(|m| m.len()).unwrap_or(0);
                if offset > len {
                    return Err(StoreError::BadOffset);
                }
            }
            if let Ok(meta) = std::fs::symlink_metadata(&target) {
                if mode == WriteMode::New {
                    return Err(StoreError::Exists);
                }
                if meta.file_type().is_symlink() {
                    return Err(StoreError::Denied(Cow::Borrowed("That name is a link; not replaced.")));
                }
                if meta.is_dir() {
                    return Err(StoreError::NotAFile);
                }
                if let Some(why) = crate::cpm::outside_change_refusal(&target) {
                    return Err(StoreError::Locked(Cow::Borrowed(why)));
                }
            }
            // Claimed under the file's real path when it exists, so two
            // sessions spelling one file differently (on a case-insensitive
            // disk) still see each other.
            let claim_key = std::fs::canonicalize(&target).unwrap_or_else(|_| target.clone());
            let claim = WriteClaim::take(&claim_key)?;
            let mut opts = std::fs::OpenOptions::new();
            let (file, temp) = match mode {
                WriteMode::Replace => {
                    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                    opts.write(true).create_new(true);
                    // A temp left by a crash of an earlier run with the same
                    // process ID would make `create_new` fail and the client
                    // hear "Already exists" about a file it can't see: take
                    // the next number instead.
                    let mut placed = None;
                    for _ in 0..100 {
                        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let temp = parent.join(format!("{TEMP_PREFIX}{}-{n}", std::process::id()));
                        match opts.open(&temp) {
                            Ok(f) => {
                                placed = Some((f, Some(temp)));
                                break;
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                            Err(e) => return Err(io_err(e)),
                        }
                    }
                    placed.ok_or(StoreError::Io("no free temporary name".into()))?
                }
                WriteMode::New => (opts.write(true).create_new(true).open(&target).map_err(io_err)?, None),
                WriteMode::Append => (opts.append(true).create(true).open(&target).map_err(io_err)?, None),
                WriteMode::At(offset) => {
                    let mut f = opts.write(true).create(true).truncate(false).open(&target).map_err(io_err)?;
                    if offset > f.metadata().map_err(io_err)?.len() {
                        return Err(StoreError::BadOffset);
                    }
                    f.seek(SeekFrom::Start(offset)).map_err(io_err)?;
                    (f, None)
                }
            };
            Ok((file, temp, target, claim))
        })
        .await?;
        Ok(Upload { file: tokio::fs::File::from_std(file), temp, target, _claim: claim })
    }

    /// DELE: remove a file.
    pub async fn delete(&self, vpath: &str) -> Result<()> {
        let vpath = vpath.to_string();
        Self::blocking(move || {
            let base = Self::base()?;
            let p = Self::existing(&base, &vpath)?;
            if p.is_dir() {
                return Err(StoreError::NotAFile);
            }
            if is_being_written(&p) {
                return Err(StoreError::Busy);
            }
            if let Some(why) = crate::cpm::outside_change_refusal(&p) {
                return Err(StoreError::Locked(Cow::Borrowed(why)));
            }
            std::fs::remove_file(&p).map_err(io_err)
        })
        .await
    }

    /// MKD: create a directory.
    pub async fn mkdir(&self, vpath: &str) -> Result<()> {
        let vpath = vpath.to_string();
        Self::blocking(move || {
            let base = Self::base()?;
            let (parent, leaf) = Self::new_target(&base, &vpath)?;
            std::fs::create_dir(parent.join(leaf)).map_err(io_err)
        })
        .await
    }

    /// RMD: remove an empty directory.
    pub async fn rmdir(&self, vpath: &str) -> Result<()> {
        let vpath = vpath.to_string();
        Self::blocking(move || {
            let base = Self::base()?;
            let p = Self::existing(&base, &vpath)?;
            if !p.is_dir() {
                return Err(StoreError::NotADirectory);
            }
            if p == base {
                return Err(StoreError::Denied(Cow::Borrowed("That is the top folder.")));
            }
            if Self::is_structural_dir(&base, &p) {
                return Err(StoreError::Locked(Cow::Borrowed("Used by the CP/M emulator.")));
            }
            std::fs::remove_dir(&p).map_err(io_err)
        })
        .await
    }

    /// Check that `vpath` may be renamed, before RNFR answers 350 -- so a
    /// client hears the refusal at the first step, not after naming the
    /// destination.  The rename itself checks again.
    pub async fn check_rename_source(&self, vpath: &str) -> Result<()> {
        let vpath = vpath.to_string();
        Self::blocking(move || {
            let base = Self::base()?;
            Self::rename_source(&base, &vpath).map(|_| ())
        })
        .await
    }

    fn rename_source(base: &Path, vpath: &str) -> Result<PathBuf> {
        Self::clean(vpath)?;
        // The source itself, not what it points at: renaming a link renames
        // the link.  Its parent must be in the jail.
        let (dir, leaf) = super::proto::split_leaf(vpath);
        let leaf = leaf.ok_or(StoreError::Denied(Cow::Borrowed("That is the top folder.")))?;
        let src = Self::existing(base, &dir)?.join(&leaf);
        let meta = std::fs::symlink_metadata(&src).map_err(io_err)?;
        // A link to a folder counts as the folder here: a symlinked `CPM` or
        // drive folder is still the emulator's.
        let is_folder = meta.is_dir() || (meta.file_type().is_symlink() && src.is_dir());
        if is_folder && Self::is_structural_dir(base, &src) {
            return Err(StoreError::Locked(Cow::Borrowed("Used by the CP/M emulator.")));
        }
        if !is_folder {
            if is_being_written(&src) {
                return Err(StoreError::Busy);
            }
            if let Some(why) = crate::cpm::outside_change_refusal(&src) {
                return Err(StoreError::Locked(Cow::Borrowed(why)));
            }
        }
        Ok(src)
    }

    /// RNFR/RNTO: rename or move within the jail.  A file may replace a file
    /// (as `rename(2)` does, and as clients expect); nothing replaces a
    /// directory.
    pub async fn rename(&self, from: &str, to: &str) -> Result<()> {
        let (from, to) = (from.to_string(), to.to_string());
        Self::blocking(move || {
            let base = Self::base()?;
            let src = Self::rename_source(&base, &from)?;
            let (parent, leaf) = Self::new_target(&base, &to)?;
            let dst = parent.join(leaf);
            if dst == src {
                return Ok(());
            }
            if let Ok(meta) = std::fs::symlink_metadata(&dst) {
                if meta.is_dir() || src.is_dir() {
                    return Err(StoreError::Exists);
                }
                if is_being_written(&dst) {
                    return Err(StoreError::Busy);
                }
                if let Some(why) = crate::cpm::outside_change_refusal(&dst) {
                    return Err(StoreError::Locked(Cow::Borrowed(why)));
                }
            }
            // A directory may not be moved inside itself.
            if src.is_dir() && dst.starts_with(&src) {
                return Err(StoreError::Denied(Cow::Borrowed("Cannot move a folder inside itself.")));
            }
            std::fs::rename(&src, &dst).map_err(io_err)
        })
        .await
    }

    /// STOU: a free name in `dir`, from the client's suggestion if it gave a
    /// usable one (`name`, then `name.1`, `name.2`, ...), else `ftp-upload`.
    pub async fn unique_name(&self, dir: &str, hint: Option<String>) -> Result<String> {
        let dir = dir.to_string();
        Self::blocking(move || {
            let base = Self::base()?;
            let parent = Self::existing(&base, &dir)?;
            if !parent.is_dir() {
                return Err(StoreError::NotADirectory);
            }
            let stem = hint
                .filter(|h| TelnetSession::validate_new_name(h).is_ok())
                .unwrap_or_else(|| "ftp-upload".to_string());
            for n in 0..10_000u32 {
                let name = if n == 0 { stem.clone() } else { format!("{stem}.{n}") };
                if TelnetSession::validate_new_name(&name).is_ok() && !parent.join(&name).exists() {
                    let sep = if dir == "/" { "" } else { "/" };
                    return Ok(format!("{dir}{sep}{name}"));
                }
            }
            Err(StoreError::Exists)
        })
        .await
    }
}
