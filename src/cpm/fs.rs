//! Directory-backed CP/M filesystem state: the current drive, the DMA
//! transfer address, and resolution of an FCB to a **jailed** host path
//! under `CPM/<drive>/`.
//!
//! Each emulated drive A:–P: is a folder inside the `CPM/` container under
//! `transfer_dir` (created on launch — see `telnet/cpm_emu.rs`).  Because a
//! resolved filename is always a validated 8.3 name (no separators, no
//! `..`) joined onto a fixed single-letter drive directory beneath the
//! container base, a guest can never escape to the host filesystem — the
//! same jail guarantee the transfer subsystem relies on.

use super::fcb::{format_8_3, split_8_3, Fcb, FCB_SIZE};
use std::path::{Path, PathBuf};

/// Number of emulated drives.  CP/M 2.2's FCB drive field is 4 bits, so the
/// architectural maximum is 16 (A: through P:); we expose all of them, each a
/// folder auto-created under `CPM/`.
pub const NUM_DRIVES: u8 = 16;

/// Default DMA (disk transfer) address in the guest — the CP/M default
/// buffer at 0x0080 (the second half of the zero page).
pub const DEFAULT_DMA: u16 = 0x0080;

/// Maximum size of a single emulated file (8 MB, matching the gateway's
/// file-transfer cap).  Bounds a guest that writes a huge/high random
/// record number (up to the 24-bit ~2 GB range) so a `.COM` can't spray a
/// multi-gigabyte sparse file to exhaust the host disk.  This is also the
/// real CP/M 2.2 per-file ceiling, so it doesn't constrain legitimate use.
pub const MAX_CPM_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// A synthetic 32-byte CP/M directory entry (one extent of one file).
pub type DirEntry = [u8; 32];

/// Directory-backed CP/M filesystem: which drive is current, where the
/// DMA buffer is, and the `CPM/` container base on the host.
pub struct CpmFs {
    /// Absolute path to the `CPM/` container (holds `A`..`H`).
    base: PathBuf,
    /// Current drive, 0-based (0 = A:, 7 = H:).
    drive: u8,
    /// Current DMA transfer address in guest memory.
    dma: u16,
    /// Directory entries produced by the last `search_first`, walked one at
    /// a time by `search_next` (a point-in-time snapshot of the drive).
    search: Vec<DirEntry>,
    /// Cursor into `search` (index of the last entry returned).
    search_pos: usize,
    /// Current CP/M user number (0–15).  Tracked so BDOS 32 get/set and the
    /// `USER` command are self-consistent; the directory is *not* segregated
    /// by user (all files share one flat namespace), a documented
    /// simplification of this host-directory-backed filesystem.
    user: u8,
    /// Software write-protect bitmap, one bit per drive (bit 0 = A:).  Set by
    /// BDOS 28 (Write Protect Disk) for the current drive, reported by BDOS 29
    /// (Get R/O Vector), cleared by BDOS 13 (Reset Disk System) and BDOS 37
    /// (Reset Drive).
    ///
    /// Deliberately in-memory and volatile: on real CP/M this is a BDOS flag
    /// that lives until the next disk reset, *not* a property of the media, so
    /// persisting it to the host would be wrong (and would need the sidecar
    /// metadata we specifically avoid — see `set_file_ro`).
    ro_drives: u16,
}

impl CpmFs {
    /// A filesystem rooted at `base` (the `CPM/` container), current drive
    /// A:, DMA at the default 0x0080.
    pub fn new(base: PathBuf) -> CpmFs {
        CpmFs {
            base,
            drive: 0,
            dma: DEFAULT_DMA,
            search: Vec::new(),
            search_pos: 0,
            user: 0,
            ro_drives: 0,
        }
    }

    /// BDOS 28: software write-protect the current drive until the next disk
    /// reset.
    pub fn set_drive_ro(&mut self) {
        self.ro_drives |= 1u16 << self.drive;
    }

    /// BDOS 29: the R/O bitmap (bit 0 = A:) for all sixteen drives.
    pub fn ro_vector(&self) -> u16 {
        self.ro_drives
    }

    /// BDOS 13: clear every software write-protect (Reset Disk System).
    pub fn clear_all_drive_ro(&mut self) {
        self.ro_drives = 0;
    }

    /// BDOS 37: clear the write-protect on just the drives set in `mask`
    /// (bit 0 = A:).
    pub fn clear_drive_ro(&mut self, mask: u16) {
        self.ro_drives &= !mask;
    }

    /// Whether the drive an FCB addresses is software write-protected.  The
    /// FCB drive byte is 0 for "current drive", so this resolves it the same
    /// way every other path does.
    pub fn fcb_drive_is_ro(&self, fcb: &Fcb) -> bool {
        match self.drive_index_for(fcb.drive) {
            Some(d) => self.ro_drives & (1u16 << d) != 0,
            None => false,
        }
    }

    /// Whether a host file is read-only, which is how this filesystem stores
    /// CP/M's per-file R/O (t1') attribute — no sidecar metadata, so the
    /// attribute means the same thing to the host user as to the guest.
    ///
    /// `Permissions::readonly` is the portable spelling: on Unix it is "no
    /// write bit set for anyone", on Windows the read-only attribute itself.
    pub fn host_is_ro(path: &Path) -> bool {
        std::fs::metadata(path)
            .map(|m| m.permissions().readonly())
            .unwrap_or(false)
    }

    /// BDOS 30 (Set File Attributes), R/O half: mark the file the FCB names
    /// read-only (or writable) on the host.  `None` if the file doesn't
    /// resolve or the permission change failed.
    ///
    /// Only t1' (R/O) is honoured.  t2' (System) and t3' (Archive) have
    /// nowhere to live in a plain host directory and are accepted-and-ignored
    /// rather than faked — storing them would mean dropping sidecar files into
    /// the very folders users drop their own files into.
    pub fn set_file_ro(&self, fcb: &Fcb, ro: bool) -> Option<()> {
        let path = self.resolve(fcb)?;
        if !path.is_file() {
            return None;
        }
        Self::set_host_ro(&path, ro).ok()
    }

    /// Set or clear a host file's read-only state.
    ///
    /// Deliberately **not** `Permissions::set_readonly(false)`: on Unix that
    /// sets *every* write bit, so clearing CP/M's R/O attribute would leave the
    /// file world-writable (0o666) — a guest turning an attribute off must not
    /// widen host permissions.  Clearing grants owner-write only and leaves the
    /// group/other bits as they were; setting clears all three, which is what
    /// "read-only" has to mean for [`CpmFs::host_is_ro`] to agree.
    pub fn set_host_ro(path: &Path, ro: bool) -> std::io::Result<()> {
        let mut perms = std::fs::metadata(path)?.permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = perms.mode();
            perms.set_mode(if ro { mode & !0o222 } else { mode | 0o200 });
        }
        #[cfg(not(unix))]
        {
            perms.set_readonly(ro);
        }
        std::fs::set_permissions(path, perms)
    }

    /// Current CP/M user number (0–15).
    pub fn current_user(&self) -> u8 {
        self.user
    }

    /// Set the current CP/M user number, clamped to 0–15.
    pub fn set_user(&mut self, user: u8) {
        self.user = user & 0x0F;
    }

    /// Current drive, 0-based (0 = A:).
    pub fn current_drive(&self) -> u8 {
        self.drive
    }

    /// Current drive as an uppercase letter (`A`..`H`).
    pub fn current_drive_letter(&self) -> char {
        (b'A' + self.drive) as char
    }

    /// Select a drive by 0-based index (BDOS 14 convention: E = 0 → A:).
    /// Returns false (and changes nothing) for an out-of-range drive.
    pub fn select(&mut self, drive0: u8) -> bool {
        if drive0 < NUM_DRIVES {
            self.drive = drive0;
            true
        } else {
            false
        }
    }

    /// Current DMA transfer address.
    pub fn dma(&self) -> u16 {
        self.dma
    }

    /// Set the DMA transfer address (BDOS 26).
    pub fn set_dma(&mut self, addr: u16) {
        self.dma = addr;
    }

    /// Map an FCB's drive byte (0 = current, 1 = A:, …) to a 0-based drive
    /// index, or `None` if it names a drive beyond P:.
    pub fn drive_index_for(&self, fcb_drive: u8) -> Option<u8> {
        match fcb_drive {
            0 => Some(self.drive),
            d if d <= NUM_DRIVES => Some(d - 1),
            _ => None,
        }
    }

    /// The host directory for a 0-based drive index.
    pub fn drive_dir(&self, drive0: u8) -> PathBuf {
        self.base.join(((b'A' + drive0) as char).to_string())
    }

    /// Resolve an FCB to a concrete, jailed host path.  Returns `None` if
    /// the drive is out of range or the FCB does not name a legal, concrete
    /// (non-wildcard) 8.3 file — which, together with the fixed drive
    /// directory, guarantees the path stays inside the `CPM/` container.
    pub fn resolve(&self, fcb: &Fcb) -> Option<PathBuf> {
        let drive0 = self.drive_index_for(fcb.drive)?;
        self.resolve_name(drive0, &fcb.name, &fcb.ext)
    }

    /// Resolve a concrete 8.3 name on a 0-based drive to a jailed host
    /// path.  Re-validates as a concrete name (rejecting wildcards and
    /// separators) so the join cannot traverse out of the drive directory.
    fn resolve_name(&self, drive0: u8, name: &[u8; 8], ext: &[u8; 3]) -> Option<PathBuf> {
        let filename = format_8_3(name, ext);
        // Primary defense: a concrete 8.3 name carries no separators or
        // "..", so joining it onto a fixed single-letter drive directory
        // cannot traverse out of the container.
        split_8_3(&filename)?;
        let dir = self.drive_dir(drive0);
        // CP/M names are uppercase 8.3; host files may be any case.  Prefer an
        // existing file that matches case-insensitively (so a lowercase
        // `foo.txt` placed by the operator is openable, not just listed) and
        // fall back to the canonical uppercased name for a to-be-created file.
        // Matches the Gateway Shell's case-insensitive resolution [[project_session_bookmark_2026-07-15]].
        let path = Self::existing_ci(&dir, &filename).unwrap_or_else(|| dir.join(&filename));
        if !Self::is_within(&self.base, &path) {
            return None;
        }
        // Belt-and-suspenders symlink defense (mirrors transfer.rs
        // `verify_transfer_path`): the resolved real path must live under the
        // real base, so a symlink can't point a file operation out of the
        // jail.  A file being created doesn't exist yet, so when the target
        // itself can't be canonicalized we fall back to canonicalizing its
        // parent (the drive directory) — that closes the gap where a *drive
        // directory* symlink could redirect a `make`/create outside the jail.
        //
        // Residual TOCTOU (accepted under the trusted-LAN threat model): the
        // caller opens the returned path in a separate step, so a symlink
        // swapped into `CPM/<drive>` between this check and the open could
        // redirect the op.  The guest can't create symlinks through this FS
        // (`make` = `File::create`), so this needs a *separate* local writer
        // to the container — out of scope for the trusted operator model.
        if let Ok(canon_base) = std::fs::canonicalize(&self.base) {
            match std::fs::canonicalize(&path) {
                Ok(canon_target) => {
                    if !canon_target.starts_with(&canon_base) {
                        return None;
                    }
                }
                Err(_) => {
                    // Target not created yet: verify the drive directory it
                    // would be created in isn't itself a symlink escaping.
                    if let Some(parent) = path.parent() {
                        if let Ok(canon_parent) = std::fs::canonicalize(parent) {
                            if !canon_parent.starts_with(&canon_base) {
                                return None;
                            }
                        }
                        // If the parent can't canonicalize either, the lexical
                        // `is_within` guarantee above still holds.
                    }
                }
            }
        }
        Some(path)
    }

    /// True if `path` is lexically within `base` (neither may contain a
    /// `..` that climbs out — our names never do, but check anyway).
    fn is_within(base: &Path, path: &Path) -> bool {
        path.starts_with(base) && !path.components().any(|c| c.as_os_str() == "..")
    }

    /// Find an existing regular file in `dir` whose name equals `filename`
    /// case-insensitively (the exact-case name first, then a scan).  Skips
    /// symlinks (via `DirEntry::file_type`, which does not follow them), so a
    /// planted link is never resolved — matching the enumeration paths.
    /// Returns `None` for a to-be-created file so the caller uses the
    /// canonical uppercased name.
    fn existing_ci(dir: &Path, filename: &str) -> Option<PathBuf> {
        let exact = dir.join(filename);
        if exact.is_file() {
            return Some(exact);
        }
        for entry in std::fs::read_dir(dir).ok()?.flatten() {
            if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                if let Some(nm) = entry.file_name().to_str() {
                    if nm.eq_ignore_ascii_case(filename) {
                        return Some(entry.path());
                    }
                }
            }
        }
        None
    }

    /// BDOS "open file" (15): does the FCB name an existing file on its
    /// drive?  Returns the resolved path when it exists, else `None`.
    pub fn open_existing(&self, fcb: &Fcb) -> Option<PathBuf> {
        let path = self.resolve(fcb)?;
        if path.is_file() {
            Some(path)
        } else {
            None
        }
    }

    /// Read an entire file's bytes for loading a transient program (a
    /// `.COM`) into the TPA.  Jailed via `open_existing` (canonical-prefix +
    /// symlink checks); returns `Ok(None)` when the file doesn't exist, and
    /// refuses a file larger than the CP/M per-file cap so a giant host file
    /// can't be slurped whole into memory.  (`load_com` further truncates to
    /// the usable TPA, but bounding the read keeps the `Vec` small.)
    pub fn read_whole_file(&self, fcb: &Fcb) -> std::io::Result<Option<Vec<u8>>> {
        let path = match self.open_existing(fcb) {
            Some(p) => p,
            None => return Ok(None),
        };
        if std::fs::metadata(&path)?.len() > MAX_CPM_FILE_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "file exceeds max CP/M file size",
            ));
        }
        Ok(Some(std::fs::read(&path)?))
    }

    /// BDOS "make file" (22): create (truncating any existing file) the
    /// file the FCB names, so subsequent writes land in it.  Returns the
    /// path on success.
    pub fn make(&self, fcb: &Fcb) -> Option<PathBuf> {
        if self.fcb_drive_is_ro(fcb) {
            return None;
        }
        let path = self.resolve(fcb)?;
        // `File::create` truncates, so an existing R/O file must be refused
        // here.  (The host would refuse it too — unlike unlink, opening for
        // write does check the file's own permission — but failing on our own
        // check keeps the reason explicit and the behaviour identical on
        // Windows.)
        if path.is_file() && Self::host_is_ro(&path) {
            return None;
        }
        match std::fs::File::create(&path) {
            Ok(_) => Some(path),
            Err(_) => None,
        }
    }

    /// BDOS "rename file" (23): rename the file `old` names to the new 8.3
    /// name on the same drive.  Refuses if the source is missing or the
    /// destination already exists (no silent clobber).  Returns success.
    /// Refuses on a write-protected drive, and refuses to rename a read-only
    /// file — like `unlink`, a Unix `rename` is governed by the directory's
    /// write bit, so the host permission does not enforce this for us.
    pub fn rename(&self, old: &Fcb, new_name: &[u8; 8], new_ext: &[u8; 3]) -> bool {
        if self.fcb_drive_is_ro(old) {
            return false;
        }
        let drive0 = match self.drive_index_for(old.drive) {
            Some(d) => d,
            None => return false,
        };
        let old_path = match self.resolve_name(drive0, &old.name, &old.ext) {
            Some(p) => p,
            None => return false,
        };
        let new_path = match self.resolve_name(drive0, new_name, new_ext) {
            Some(p) => p,
            None => return false,
        };
        if !old_path.is_file() || new_path.exists() || Self::host_is_ro(&old_path) {
            return false;
        }
        std::fs::rename(old_path, new_path).is_ok()
    }

    /// BDOS "compute file size" (35): the number of 128-byte records in the
    /// file the FCB names (its virtual CP/M size), or `None` if unresolved.
    pub fn file_size_records(&self, fcb: &Fcb) -> Option<u32> {
        let path = self.resolve(fcb)?;
        let size = std::fs::metadata(&path).ok()?.len();
        Some(size.div_ceil(128) as u32)
    }

    /// Read one 128-byte record at `record` from the file the FCB names.
    /// Returns `Ok(None)` at end-of-file (nothing there to read); a short
    /// final record is padded with the CP/M EOF filler (0x1A).
    pub fn read_record(&self, fcb: &Fcb, record: u32) -> std::io::Result<Option<[u8; 128]>> {
        use std::io::{Read, Seek, SeekFrom};
        let path = match self.resolve(fcb) {
            Some(p) => p,
            None => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "unresolved FCB",
                ))
            }
        };
        let mut f = std::fs::File::open(&path)?;
        let offset = record as u64 * 128;
        if offset >= f.metadata()?.len() {
            return Ok(None); // reading at/after EOF
        }
        f.seek(SeekFrom::Start(offset))?;
        let mut buf = [0x1Au8; 128]; // pad a short final record with ^Z
        let mut filled = 0;
        while filled < 128 {
            let n = f.read(&mut buf[filled..])?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        if filled == 0 {
            Ok(None)
        } else {
            Ok(Some(buf))
        }
    }

    /// BDOS "search first" (17): find every host file on the FCB's drive
    /// whose 8.3 name matches the (possibly `?`-wildcarded) FCB, build a
    /// directory entry per extent, and return the first.  `search_next`
    /// walks the rest.  Returns `None` when nothing matches.
    pub fn search_first(&mut self, fcb: &Fcb) -> Option<DirEntry> {
        self.search = self.build_dir_entries(fcb);
        self.search_pos = 0;
        self.search.first().copied()
    }

    /// BDOS "search next" (18): the next entry from the last `search_first`,
    /// or `None` when the directory listing is exhausted.
    pub fn search_next(&mut self) -> Option<DirEntry> {
        if self.search_pos + 1 < self.search.len() {
            self.search_pos += 1;
            Some(self.search[self.search_pos])
        } else {
            None
        }
    }

    /// Does the current drive hold a temporary `$`-prefixed file?
    ///
    /// This is what BDOS 13 (Reset Disk System) reports in `A`, and the CCP is
    /// its consumer: `CCP22.ASM` does `CALL RESET` (function 13) then
    /// `STA SUBFL`, so this flag is how a fresh CCP discovers that a `SUBMIT`
    /// batch is already in progress.
    ///
    /// The test is deliberately **any name beginning with `$`**, not `$$$.SUB`
    /// specifically. CP/M 2.2's `BDOS22.ASM` drive-login scan compares only the
    /// first filename byte — `SUI '$'` — and its own comment calls it "some
    /// sort of TEMPORARY FILE OF THE $$$.EXT VARIETY", so `$FOO.BAR` sets the
    /// flag too. Narrowing it to `$$$.SUB` would be a plausible-looking
    /// deviation from the real thing.
    ///
    /// Real CP/M also requires the directory entry's user byte to match the
    /// current user. This filesystem keeps one flat namespace per drive (a
    /// documented simplification — see `user`), so there is no per-user
    /// directory to filter and every visible file counts.
    pub fn has_temp_dollar_file(&self) -> bool {
        // `$` then all-wildcard: the same matcher every other directory
        // operation uses, rather than a second ad-hoc scan.
        let mut raw = [0u8; FCB_SIZE];
        raw[1] = b'$';
        raw[2..12].fill(b'?');
        !self.list_matching(&Fcb::from_bytes(&raw)).is_empty()
    }

    /// Shrink the file the FCB names to `records` 128-byte records, returning
    /// the new record count, or `None` if it does not resolve/exist.
    ///
    /// This is how the CCP consumes a `$$$.SUB`: real CP/M decrements the
    /// FCB's record count and closes the file, which the BDOS turns into a
    /// shorter file.  We have no directory to rewrite, so the host file is
    /// truncated directly — the same observable result.
    ///
    /// Refuses on a software-write-protected drive (BDOS 28), like every other
    /// mutating path here.
    pub fn truncate_to_records(&self, fcb: &Fcb, records: u32) -> Option<u32> {
        if self.fcb_drive_is_ro(fcb) {
            return None;
        }
        let path = self.open_existing(fcb)?;
        let len = (records as u64) * 128;
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .and_then(|f| f.set_len(len))
            .ok()?;
        Some(records)
    }

    /// The 8.3 names on the FCB's drive whose name/ext match its (possibly
    /// wildcarded) pattern, sorted and deduplicated — the listing behind the
    /// CCP's `DIR [d:][afn]`.
    ///
    /// Matching goes through `Fcb::matches`, the same predicate BDOS Search
    /// First and the built-in `ERA` use, so `DIR *.COM` cannot disagree with
    /// `ERA *.COM` about which files a wildcard covers.
    ///
    /// Deduplicated because CP/M gives a file over 16 KB one directory entry
    /// per extent, and a listing must still show it once. Host files that are
    /// not legal 8.3 names are omitted, matching what CP/M programs can see.
    pub fn list_matching(&self, fcb: &Fcb) -> Vec<String> {
        let drive0 = match self.drive_index_for(fcb.drive) {
            Some(d) => d,
            None => return Vec::new(),
        };
        let dir = self.drive_dir(drive0);
        let mut names = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                if e.file_type().map(|t| t.is_file()).unwrap_or(false) {
                    let fname = e.file_name().to_string_lossy().to_string();
                    if let Some((n, x)) = split_8_3(&fname)
                        && fcb.matches(&n, &x)
                    {
                        names.push(super::fcb::format_8_3(&n, &x));
                    }
                }
            }
        }
        names.sort();
        names.dedup();
        names
    }

    /// Number of `block_size`-byte allocation blocks the CP/M-visible files on
    /// the current drive occupy — each file's byte length rounded up to a whole
    /// block (as CP/M allocates), summed.  Used to synthesize the allocation
    /// vector for BDOS "get free space" queries (STAT's "bytes remaining").
    /// Only valid 8.3 files count, matching what the directory shows.
    pub fn current_drive_used_blocks(&self, block_size: u64) -> u64 {
        let dir = self.drive_dir(self.drive);
        let mut blocks: u64 = 0;
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                if !e.file_type().map(|t| t.is_file()).unwrap_or(false) {
                    continue;
                }
                let fname = e.file_name().to_string_lossy().to_string();
                if split_8_3(&fname).is_none() {
                    continue; // not a CP/M-visible name
                }
                let len = e.metadata().map(|m| m.len()).unwrap_or(0);
                blocks += len.div_ceil(block_size); // 0-byte file → 0 data blocks
            }
        }
        blocks
    }

    /// Every host file on the FCB's drive whose 8.3 name matches it.
    ///
    /// Shared by [`CpmFs::delete`] and [`CpmFs::count_ro_matches`] on purpose:
    /// they must agree on exactly this set, or the `File R/O` message would
    /// describe different files than the erase actually skipped.
    fn matching_files(&self, fcb: &Fcb) -> Vec<PathBuf> {
        let Some(drive0) = self.drive_index_for(fcb.drive) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if let Ok(rd) = std::fs::read_dir(self.drive_dir(drive0)) {
            for e in rd.flatten() {
                if !e.file_type().map(|t| t.is_file()).unwrap_or(false) {
                    continue;
                }
                let fname = e.file_name().to_string_lossy().to_string();
                if let Some((n, x)) = split_8_3(&fname) {
                    if fcb.matches(&n, &x) {
                        out.push(e.path());
                    }
                }
            }
        }
        out
    }

    /// BDOS "delete file" (19): remove every host file on the FCB's drive
    /// matching the (possibly wildcarded) FCB.  Returns the count deleted.
    /// A read-only file is skipped, not deleted — CP/M refuses to erase an
    /// R/O file, and the host permission alone does **not** stop this: on Unix
    /// `unlink` is governed by the *directory's* write bit, not the file's, so
    /// without this check a `chmod -w` file was erasable from the guest.
    /// A software write-protected drive (BDOS 28) refuses outright.
    pub fn delete(&self, fcb: &Fcb) -> usize {
        if self.fcb_drive_is_ro(fcb) {
            return 0;
        }
        let mut count = 0;
        for path in self.matching_files(fcb) {
            if !Self::host_is_ro(&path) && std::fs::remove_file(&path).is_ok() {
                count += 1;
            }
        }
        count
    }

    /// How many files matching `fcb` were left in place by [`CpmFs::delete`]
    /// because they are read-only.  Lets a caller tell "nothing matched" from
    /// "matched but protected" so it can report the difference.
    pub fn count_ro_matches(&self, fcb: &Fcb) -> usize {
        self.matching_files(fcb)
            .iter()
            .filter(|p| Self::host_is_ro(p))
            .count()
    }

    /// Build the directory-entry list for every file matching `fcb`, sorted
    /// by name, one entry per 16 KB extent (so multi-extent files and file
    /// sizes are represented the way `STAT`/`DIR` expect).
    fn build_dir_entries(&self, fcb: &Fcb) -> Vec<DirEntry> {
        let mut out = Vec::new();
        let drive0 = match self.drive_index_for(fcb.drive) {
            Some(d) => d,
            None => return out,
        };
        let dir = self.drive_dir(drive0);
        /// One matching host file, ready to be turned into directory entries.
        /// `host_name` is kept only to sort the listing by the on-disk name.
        struct Match {
            name: [u8; 8],
            ext: [u8; 3],
            size: u64,
            ro: bool,
            host_name: String,
        }
        let mut files: Vec<Match> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                if !e.file_type().map(|t| t.is_file()).unwrap_or(false) {
                    continue;
                }
                let fname = e.file_name().to_string_lossy().to_string();
                if let Some((n, x)) = split_8_3(&fname) {
                    if fcb.matches(&n, &x) {
                        let md = e.metadata();
                        let size = md.as_ref().map(|m| m.len()).unwrap_or(0);
                        let ro = md
                            .as_ref()
                            .map(|m| m.permissions().readonly())
                            .unwrap_or(false);
                        files.push(Match { name: n, ext: x, size, ro, host_name: fname });
                    }
                }
            }
        }
        files.sort_by(|a, b| a.host_name.cmp(&b.host_name));
        for f in files {
            out.extend(dir_entries_for_file(&f.name, &f.ext, f.size, f.ro));
        }
        out
    }

    /// Write one 128-byte record at `record` into the file the FCB names
    /// (which must already exist via open/make).  Seeking past the current
    /// end zero-fills the gap, matching CP/M's record model.
    pub fn write_record(&self, fcb: &Fcb, record: u32, data: &[u8; 128]) -> std::io::Result<()> {
        use std::io::{Seek, SeekFrom, Write};
        // A software write-protected drive (BDOS 28) refuses every write.
        if self.fcb_drive_is_ro(fcb) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "drive is write-protected (BDOS 28)",
            ));
        }
        let offset = record as u64 * 128;
        // Bound the file size so a guest can't seek to a huge random record
        // (up to ~2 GB) and exhaust the host disk.
        if offset + 128 > MAX_CPM_FILE_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "record beyond max CP/M file size",
            ));
        }
        let path = match self.resolve(fcb) {
            Some(p) => p,
            None => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "unresolved FCB",
                ))
            }
        };
        // A read-only *file* is refused here rather than left to the host.
        // Opening for write does fail for an ordinary user, but **root bypasses
        // file permissions** (`CAP_DAC_OVERRIDE`) — and this gateway commonly
        // runs from systemd — so relying on the OS would let a guest write to a
        // file it is not allowed to erase or rename.  Checking here also keeps
        // all four mutating paths enforcing the attribute the same way.
        // (A file that isn't there reads as not-read-only and falls through to
        // the open below, which reports the missing file.)
        if Self::host_is_ro(&path) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "file is read-only (CP/M t1')",
            ));
        }
        let mut f = std::fs::OpenOptions::new().read(true).write(true).open(&path)?;
        f.seek(SeekFrom::Start(offset))?;
        f.write_all(data)?;
        Ok(())
    }
}

/// Build the CP/M directory entries for a single file: one 32-byte entry
/// per 16 KB extent, carrying user 0, the 8.3 name, the extent number
/// (EX/S2), and the record count (RC).  The allocation map is filled with
/// distinct non-zero block numbers so a directory scanner treats the space
/// as used.  An empty file still gets one entry (RC = 0).
///
/// `ro` sets the t1' attribute — the high bit of the first extension byte —
/// which is how CP/M marks a file read-only, so a host-side `chmod -w` shows
/// up as `R/O` in `STAT` and is refused by erase/rename in the guest.
fn dir_entries_for_file(name: &[u8; 8], ext: &[u8; 3], size: u64, ro: bool) -> Vec<DirEntry> {
    let records = size.div_ceil(128) as u32; // 128-byte records
    let extents = if records == 0 {
        1
    } else {
        records.div_ceil(128) // 128 records per 16 KB extent
    };
    let mut out = Vec::new();
    let mut block: u8 = 1;
    for k in 0..extents {
        let mut e: DirEntry = [0u8; 32];
        e[0] = 0; // user number 0
        e[1..9].copy_from_slice(name);
        e[9..12].copy_from_slice(ext);
        if ro {
            e[9] |= 0x80; // t1' = R/O
        }
        e[12] = (k & 0x1F) as u8; // EX
        e[14] = ((k >> 5) & 0x3F) as u8; // S2
        let recs_this = if records == 0 {
            0
        } else if k == extents - 1 {
            records - k * 128
        } else {
            128
        };
        e[15] = recs_this as u8; // RC (128 fits as 0x80)
        // Allocation map: one 8-bit block per 8 records (1 KB blocks).
        let blocks = (recs_this.div_ceil(8)).min(16) as usize;
        for slot in e.iter_mut().skip(16).take(blocks) {
            *slot = block;
            block = block.wrapping_add(1).max(1);
        }
        out.push(e);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::fcb::Fcb;
    use super::*;

    fn fcb_named(drive: u8, name: &str, ext: &str) -> Fcb {
        let mut b = [b' '; super::super::fcb::FCB_SIZE];
        b[0] = drive;
        for (i, c) in name.bytes().enumerate() {
            b[1 + i] = c;
        }
        for (i, c) in ext.bytes().enumerate() {
            b[9 + i] = c;
        }
        // Position fields must be zero, not space.
        b[12..].fill(0);
        Fcb::from_bytes(&b)
    }

    #[test]
    fn test_select_and_letter() {
        let mut fs = CpmFs::new(PathBuf::from("/tmp/cpm"));
        assert_eq!(fs.current_drive_letter(), 'A');
        assert!(fs.select(1));
        assert_eq!(fs.current_drive_letter(), 'B');
        assert!(fs.select(15)); // P: is the last drive
        assert_eq!(fs.current_drive_letter(), 'P');
        assert!(!fs.select(16)); // Q: is beyond P:
        assert_eq!(fs.current_drive_letter(), 'P'); // unchanged
    }

    #[test]
    fn test_drive_index_for() {
        let fs = CpmFs::new(PathBuf::from("/tmp/cpm"));
        assert_eq!(fs.drive_index_for(0), Some(0)); // default = current (A)
        assert_eq!(fs.drive_index_for(1), Some(0)); // A:
        assert_eq!(fs.drive_index_for(8), Some(7)); // H:
        assert_eq!(fs.drive_index_for(16), Some(15)); // P:
        assert_eq!(fs.drive_index_for(17), None); // beyond P: unsupported
    }

    #[test]
    fn test_resolve_jailed_path() {
        let base = PathBuf::from("/tmp/xmodem_cpm_base");
        let fs = CpmFs::new(base.clone());
        let fcb = fcb_named(1, "PIP", "COM"); // A:PIP.COM
        let p = fs.resolve(&fcb).unwrap();
        assert_eq!(p, base.join("A").join("PIP.COM"));
    }

    #[test]
    fn test_resolve_default_drive_follows_current() {
        let base = PathBuf::from("/tmp/xmodem_cpm_base");
        let mut fs = CpmFs::new(base.clone());
        fs.select(2); // C:
        let fcb = fcb_named(0, "X", "TXT"); // drive 0 = current = C:
        let p = fs.resolve(&fcb).unwrap();
        assert_eq!(p, base.join("C").join("X.TXT"));
    }

    #[test]
    fn test_resolve_rejects_bad_drive_and_wildcards() {
        let fs = CpmFs::new(PathBuf::from("/tmp/cpm"));
        // Drive beyond P:.
        assert!(fs.resolve(&fcb_named(17, "A", "TXT")).is_none());
        // Wildcard name is not a concrete file.
        assert!(fs.resolve(&fcb_named(1, "??", "COM")).is_none());
    }

    /// Create an isolated `CPM/` base with an `A` drive directory.
    fn temp_base(tag: &str) -> PathBuf {
        // PID-scoped like `cpm_emu`'s sibling `scratch_fs`: each of these
        // tests starts by `remove_dir_all`ing its base, so a path shared
        // between two overlapping `cargo test` processes means one run wipes
        // the other's fixture mid-test. Unique tags make it safe *within* a
        // run; the PID makes it safe between them.
        let base = std::env::temp_dir()
            .join(format!("xmodem_cpmfs_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("A")).unwrap();
        base
    }

    /// BDOS 13's return flag: set when the logged-in drive holds a temporary
    /// `$`-prefixed file.  This is how a fresh CCP discovers a `SUBMIT` batch
    /// is already running (`CCP22.ASM`: `CALL RESET` then `STA SUBFL`).
    ///
    /// Pins the rule the real BDOS actually implements — **any** name starting
    /// with `$`, from the single `SUI '$'` comparison on the first filename
    /// byte in `BDOS22.ASM` — rather than the narrower `$$$.SUB` it would be
    /// tempting to check for.
    #[test]
    fn test_has_temp_dollar_file() {
        let base = temp_base("dollar");
        let mut fs = CpmFs::new(base.clone());
        assert!(!fs.has_temp_dollar_file(), "a clean drive sets no flag");

        std::fs::write(base.join("A").join("PIP.COM"), b"x").unwrap();
        assert!(
            !fs.has_temp_dollar_file(),
            "an ordinary file must not set the flag"
        );

        // The submit file itself.
        std::fs::write(base.join("A").join("$$$.SUB"), b"x").unwrap();
        assert!(fs.has_temp_dollar_file(), "$$$.SUB must set the flag");

        // Any `$`-prefixed name counts, which is what the BDOS scan tests.
        std::fs::remove_file(base.join("A").join("$$$.SUB")).unwrap();
        std::fs::write(base.join("A").join("$WORK.TMP"), b"x").unwrap();
        assert!(
            fs.has_temp_dollar_file(),
            "the real BDOS compares only the first byte, so $WORK.TMP counts"
        );

        // Per-drive: a flag on A: must not follow you to B:.
        std::fs::create_dir_all(base.join("B")).unwrap();
        fs.select(1);
        assert!(
            !fs.has_temp_dollar_file(),
            "the flag describes the logged-in drive, not the whole disk set"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_make_write_read_roundtrip() {
        let base = temp_base("rw");
        let fs = CpmFs::new(base.clone());
        let fcb = fcb_named(1, "DATA", "TXT");

        // No file yet: open fails.
        assert!(fs.open_existing(&fcb).is_none());

        // Make, then write two records.
        assert!(fs.make(&fcb).is_some());
        let mut rec0 = [0u8; 128];
        rec0[..5].copy_from_slice(b"HELLO");
        let mut rec1 = [0u8; 128];
        rec1[..5].copy_from_slice(b"WORLD");
        fs.write_record(&fcb, 0, &rec0).unwrap();
        fs.write_record(&fcb, 1, &rec1).unwrap();

        // Now it opens, and reads back what we wrote.
        assert!(fs.open_existing(&fcb).is_some());
        let got0 = fs.read_record(&fcb, 0).unwrap().unwrap();
        assert_eq!(&got0[..5], b"HELLO");
        let got1 = fs.read_record(&fcb, 1).unwrap().unwrap();
        assert_eq!(&got1[..5], b"WORLD");
        // Reading past EOF yields None.
        assert!(fs.read_record(&fcb, 2).unwrap().is_none());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_resolve_is_case_insensitive_for_existing_files() {
        let base = temp_base("ci");
        // A lowercase host file (operator-placed / externally copied).
        std::fs::write(base.join("A").join("readme.txt"), b"hello there").unwrap();
        let fs = CpmFs::new(base.clone());
        let fcb = fcb_named(1, "README", "TXT"); // CP/M sees uppercase 8.3

        // It resolves to the real lowercase path and opens/reads.
        assert!(fs.open_existing(&fcb).is_some(), "lowercase host file must be openable");
        let rec = fs.read_record(&fcb, 0).unwrap().unwrap();
        assert_eq!(&rec[..11], b"hello there");

        // A genuinely-absent file still resolves to the canonical uppercase
        // path (for creation) and does not open.
        let missing = fcb_named(1, "NOPE", "TXT");
        assert!(fs.open_existing(&missing).is_none());
        assert!(fs.resolve(&missing).unwrap().ends_with("A/NOPE.TXT"));

        let _ = std::fs::remove_dir_all(&base);
    }

    /// Decode the 8.3 name out of a 32-byte directory entry.
    fn entry_name(e: &DirEntry) -> String {
        let mut n = [b' '; 8];
        let mut x = [b' '; 3];
        n.copy_from_slice(&e[1..9]);
        x.copy_from_slice(&e[9..12]);
        super::super::fcb::format_8_3(&n, &x)
    }

    #[test]
    fn test_search_enumerates_matching() {
        let base = temp_base("search");
        std::fs::write(base.join("A").join("ALPHA.TXT"), b"a").unwrap();
        std::fs::write(base.join("A").join("BETA.TXT"), b"b").unwrap();
        std::fs::write(base.join("A").join("GAMMA.COM"), b"c").unwrap();
        // A host file that is not a valid 8.3 name is invisible.
        std::fs::write(base.join("A").join("not a cpm name!.zzzz"), b"x").unwrap();
        let mut fs = CpmFs::new(base.clone());

        // "????????.???" matches every valid 8.3 file.
        let all = fcb_named(1, "????????", "???");
        let mut names = Vec::new();
        let mut cur = fs.search_first(&all);
        while let Some(e) = cur {
            names.push(entry_name(&e));
            cur = fs.search_next();
        }
        assert_eq!(names, vec!["ALPHA.TXT", "BETA.TXT", "GAMMA.COM"]);

        // "????????.TXT" matches only the .TXT files.
        let txt = fcb_named(1, "????????", "TXT");
        let mut txts = Vec::new();
        let mut cur = fs.search_first(&txt);
        while let Some(e) = cur {
            txts.push(entry_name(&e));
            cur = fs.search_next();
        }
        assert_eq!(txts, vec!["ALPHA.TXT", "BETA.TXT"]);

        // No match -> None.
        let none = fcb_named(1, "NOPE", "XYZ");
        assert!(fs.search_first(&none).is_none());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_search_multi_extent_file() {
        let base = temp_base("multiext");
        // 20000 bytes > 16 KB -> two extents.
        std::fs::write(base.join("A").join("BIG.DAT"), vec![0u8; 20000]).unwrap();
        let mut fs = CpmFs::new(base.clone());
        let pat = fcb_named(1, "BIG", "DAT");
        let e0 = fs.search_first(&pat).unwrap();
        let e1 = fs.search_next().unwrap();
        assert!(fs.search_next().is_none());
        assert_eq!(e0[12], 0); // EX 0
        assert_eq!(e0[15], 128); // first extent full (128 records)
        assert_eq!(e1[12], 1); // EX 1
        // 20000 bytes = 157 records total; second extent has 157-128 = 29.
        assert_eq!(e1[15], 29);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// `list_matching` backs the CCP's `DIR [d:][afn]`.  Covers what the old
    /// `list_current` did (current drive, sorted, illegal 8.3 names invisible,
    /// per-drive) plus the filtering and explicit-drive forms it could not
    /// express — the gap that let `DIR *.COM` list every file.
    #[test]
    fn test_list_matching() {
        let base = temp_base("list");
        std::fs::write(base.join("A").join("B.TXT"), b"b").unwrap();
        std::fs::write(base.join("A").join("A.COM"), b"a").unwrap();
        std::fs::write(base.join("A").join("bad name.zzzz"), b"x").unwrap(); // invisible
        std::fs::create_dir_all(base.join("B")).unwrap();
        std::fs::write(base.join("B").join("ONLY.B"), b"1").unwrap();
        let mut fs = CpmFs::new(base.clone());

        let pat = |spec: &str| {
            let (drive, name, ext) = super::super::fcb::parse_dir_operand(spec).unwrap();
            let mut raw = [0u8; FCB_SIZE];
            raw[0] = drive;
            raw[1..9].copy_from_slice(&name);
            raw[9..12].copy_from_slice(&ext);
            Fcb::from_bytes(&raw)
        };

        // Bare `DIR`: everything on the current drive, sorted, bad names hidden.
        assert_eq!(fs.list_matching(&pat("")), vec!["A.COM", "B.TXT"]);
        // Filtered — the whole point.
        assert_eq!(fs.list_matching(&pat("*.COM")), vec!["A.COM"]);
        assert_eq!(fs.list_matching(&pat("*.TXT")), vec!["B.TXT"]);
        // A concrete name, and one that matches nothing.
        assert_eq!(fs.list_matching(&pat("A.COM")), vec!["A.COM"]);
        assert!(fs.list_matching(&pat("NOSUCH.*")).is_empty());
        // An explicit drive reaches the other drive without selecting it.
        assert_eq!(fs.list_matching(&pat("B:")), vec!["ONLY.B"]);
        assert_eq!(fs.current_drive(), 0, "DIR B: must not change the current drive");
        // And the current drive still follows `select`.
        fs.select(1);
        assert_eq!(fs.list_matching(&pat("")), vec!["ONLY.B"]);
        assert_eq!(fs.list_matching(&pat("A:")), vec!["A.COM", "B.TXT"]);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_delete_matching() {
        let base = temp_base("delete");
        std::fs::write(base.join("A").join("ONE.TXT"), b"1").unwrap();
        std::fs::write(base.join("A").join("TWO.TXT"), b"2").unwrap();
        std::fs::write(base.join("A").join("KEEP.COM"), b"k").unwrap();
        let fs = CpmFs::new(base.clone());
        let del = fcb_named(1, "????????", "TXT");
        assert_eq!(fs.delete(&del), 2);
        assert!(!base.join("A").join("ONE.TXT").exists());
        assert!(!base.join("A").join("TWO.TXT").exists());
        assert!(base.join("A").join("KEEP.COM").exists()); // untouched
        // Deleting again matches nothing.
        assert_eq!(fs.delete(&del), 0);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn test_resolve_rejects_symlink_escape() {
        let base = temp_base("symlink");
        // A file outside the jail.
        let outside = base
            .parent()
            .unwrap()
            .join("xmodem_cpm_secret_outside.txt");
        std::fs::write(&outside, b"secret").unwrap();
        // Plant a symlink with a valid 8.3 name inside drive A: pointing out.
        let link = base.join("A").join("ESCAPE.TXT");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        let fs = CpmFs::new(base.clone());
        let fcb = fcb_named(1, "ESCAPE", "TXT");
        // The canonicalized target is outside base -> refused.
        assert!(fs.resolve(&fcb).is_none());
        assert!(fs.open_existing(&fcb).is_none());
        assert!(fs.read_record(&fcb, 0).is_err());
        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn test_resolve_rejects_symlinked_drive_dir_on_create() {
        // A drive *directory* that is a symlink pointing outside the jail
        // must not let a create (make) escape, even though the target file
        // doesn't exist yet (so the target itself can't be canonicalized).
        let base = temp_base("symdir");
        let outside = base
            .parent()
            .unwrap()
            .join("xmodem_cpm_outside_dir");
        std::fs::create_dir_all(&outside).unwrap();
        // Drive B: is a symlink to the outside directory.
        let drive_b = base.join("B");
        std::os::unix::fs::symlink(&outside, &drive_b).unwrap();
        let fs = CpmFs::new(base.clone());
        let fcb = fcb_named(2, "PWNED", "TXT"); // drive B:
        // resolve/make must refuse: the drive dir canonicalizes outside base.
        assert!(fs.resolve(&fcb).is_none());
        assert!(fs.make(&fcb).is_none());
        assert!(!outside.join("PWNED.TXT").exists()); // nothing created outside
        let _ = std::fs::remove_file(&drive_b);
        let _ = std::fs::remove_dir_all(&outside);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_rename() {
        let base = temp_base("rename");
        std::fs::write(base.join("A").join("OLD.TXT"), b"data").unwrap();
        let fs = CpmFs::new(base.clone());
        let old = fcb_named(1, "OLD", "TXT");
        let (nn, ne) = super::super::fcb::split_8_3("NEW.TXT").unwrap();
        assert!(fs.rename(&old, &nn, &ne));
        assert!(!base.join("A").join("OLD.TXT").exists());
        assert!(base.join("A").join("NEW.TXT").exists());
        // Renaming a missing source fails.
        assert!(!fs.rename(&old, &nn, &ne));
        // No clobber: renaming onto an existing file fails.
        std::fs::write(base.join("A").join("SRC.TXT"), b"s").unwrap();
        let src = fcb_named(1, "SRC", "TXT");
        assert!(!fs.rename(&src, &nn, &ne)); // NEW.TXT already exists
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_file_size_records() {
        let base = temp_base("size");
        std::fs::write(base.join("A").join("A.DAT"), vec![0u8; 200]).unwrap(); // 2 records
        std::fs::write(base.join("A").join("B.DAT"), vec![0u8; 128]).unwrap(); // 1 record
        std::fs::write(base.join("A").join("C.DAT"), b"").unwrap(); // 0 records
        let fs = CpmFs::new(base.clone());
        assert_eq!(fs.file_size_records(&fcb_named(1, "A", "DAT")), Some(2));
        assert_eq!(fs.file_size_records(&fcb_named(1, "B", "DAT")), Some(1));
        assert_eq!(fs.file_size_records(&fcb_named(1, "C", "DAT")), Some(0));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_read_whole_file() {
        let base = temp_base("wholefile");
        let fs = CpmFs::new(base.clone());
        let fcb = fcb_named(1, "PROG", "COM");
        // Missing file -> Ok(None).
        assert!(fs.read_whole_file(&fcb).unwrap().is_none());
        // Write some bytes, read them all back.
        std::fs::write(base.join("A").join("PROG.COM"), b"\xC3\x00\x01hi").unwrap();
        let got = fs.read_whole_file(&fcb).unwrap().unwrap();
        assert_eq!(got, b"\xC3\x00\x01hi");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_write_record_rejects_beyond_size_cap() {
        let base = temp_base("sizecap");
        let fs = CpmFs::new(base.clone());
        let fcb = fcb_named(1, "BIG", "DAT");
        assert!(fs.make(&fcb).is_some());
        let data = [0u8; 128];
        // A record just under the cap is fine.
        let last_ok = (MAX_CPM_FILE_BYTES / 128 - 1) as u32;
        assert!(fs.write_record(&fcb, last_ok, &data).is_ok());
        // A record past the cap (near the 24-bit random-record range) is
        // rejected before any 2 GB sparse file can be created.
        assert!(fs.write_record(&fcb, 0x00FF_FFFF, &data).is_err());
        // The file never grew past the cap.
        let len = std::fs::metadata(base.join("A").join("BIG.DAT")).unwrap().len();
        assert!(len <= MAX_CPM_FILE_BYTES, "file grew to {len}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_read_short_final_record_padded_with_ctrl_z() {
        let base = temp_base("pad");
        let fs = CpmFs::new(base.clone());
        let fcb = fcb_named(1, "SHORT", "TXT");
        assert!(fs.make(&fcb).is_some());
        // Write a 3-byte file directly (not a full record).
        std::fs::write(base.join("A").join("SHORT.TXT"), b"abc").unwrap();
        let rec = fs.read_record(&fcb, 0).unwrap().unwrap();
        assert_eq!(&rec[..3], b"abc");
        assert_eq!(rec[3], 0x1A); // padded with ^Z
        assert_eq!(rec[127], 0x1A);
        let _ = std::fs::remove_dir_all(&base);
    }

    // ─── R/O attribute + write protect (BDOS 28/29/30/37) ────────────

    /// Mark a host file read-only, the way a host user or BDOS 30 would —
    /// through the same routine production uses, so the tests can't drift from
    /// it (and so clearing doesn't go world-writable).
    fn set_ro(path: &Path, ro: bool) {
        CpmFs::set_host_ro(path, ro).unwrap();
    }

    /// The bug this whole feature exists for: a Unix `unlink` is governed by
    /// the *directory's* write bit, not the file's, so a `chmod -w` file was
    /// happily erasable from the guest.  `delete` must skip it.
    #[test]
    fn test_readonly_file_survives_delete() {
        let base = temp_base("ro_del");
        let fs = CpmFs::new(base.clone());
        let path = base.join("A").join("KEEP.TXT");
        std::fs::write(&path, b"precious").unwrap();
        set_ro(&path, true);

        let fcb = fcb_named(1, "KEEP", "TXT");
        assert_eq!(fs.delete(&fcb), 0, "a read-only file must not be deleted");
        assert!(path.is_file(), "file was erased despite being R/O");
        assert_eq!(fs.count_ro_matches(&fcb), 1, "should report it as protected");

        // Clearing the attribute makes it deletable again.
        set_ro(&path, false);
        assert_eq!(fs.delete(&fcb), 1);
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Same blind spot for rename — also directory-governed on Unix.
    #[test]
    fn test_readonly_file_cannot_be_renamed() {
        let base = temp_base("ro_ren");
        let fs = CpmFs::new(base.clone());
        let path = base.join("A").join("LOCKED.TXT");
        std::fs::write(&path, b"x").unwrap();
        set_ro(&path, true);

        let old = fcb_named(1, "LOCKED", "TXT");
        let mut nn = [b' '; 8];
        nn[..3].copy_from_slice(b"NEW");
        assert!(!fs.rename(&old, &nn, b"TXT"), "R/O file must not be renamed");
        assert!(path.is_file());
        assert!(!base.join("A").join("NEW.TXT").exists());

        set_ro(&path, false);
        assert!(fs.rename(&old, &nn, b"TXT"));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A wildcard erase deletes what it may and leaves the protected file.
    #[test]
    fn test_wildcard_delete_spares_only_readonly() {
        let base = temp_base("ro_wild");
        let fs = CpmFs::new(base.clone());
        for n in ["ONE.TXT", "TWO.TXT", "THREE.TXT"] {
            std::fs::write(base.join("A").join(n), b"d").unwrap();
        }
        set_ro(&base.join("A").join("TWO.TXT"), true);

        let fcb = fcb_named(1, "????????", "TXT");
        assert_eq!(fs.delete(&fcb), 2, "the two writable files go");
        assert!(base.join("A").join("TWO.TXT").is_file(), "R/O one stays");
        assert_eq!(fs.count_ro_matches(&fcb), 1);
        set_ro(&base.join("A").join("TWO.TXT"), false);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A host read-only file must surface as CP/M's t1' attribute so `STAT`
    /// and `DIR` show `R/O` rather than claiming it is writable.
    #[test]
    fn test_readonly_shows_as_t1_prime_in_dir_entry() {
        let base = temp_base("ro_attr");
        let mut fs = CpmFs::new(base.clone());
        let path = base.join("A").join("PROT.TXT");
        std::fs::write(&path, b"z").unwrap();

        let fcb = fcb_named(1, "PROT", "TXT");
        let e = fs.search_first(&fcb).expect("entry");
        assert_eq!(e[9] & 0x80, 0, "writable file must not carry t1'");

        set_ro(&path, true);
        let e = fs.search_first(&fcb).expect("entry");
        assert_eq!(e[9] & 0x80, 0x80, "R/O file must carry t1'");
        // The name itself must be unharmed — the flag rides the high bit only.
        assert_eq!(e[9] & 0x7F, b'T');
        set_ro(&path, false);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// BDOS 30's R/O half, through the filesystem seam.
    #[test]
    fn test_set_file_ro_round_trip() {
        let base = temp_base("ro_set");
        let fs = CpmFs::new(base.clone());
        let path = base.join("A").join("ATTR.TXT");
        std::fs::write(&path, b"q").unwrap();
        let fcb = fcb_named(1, "ATTR", "TXT");

        assert!(fs.set_file_ro(&fcb, true).is_some());
        assert!(CpmFs::host_is_ro(&path));
        assert!(fs.set_file_ro(&fcb, false).is_some());
        assert!(!CpmFs::host_is_ro(&path));

        // A file that isn't there is a failure (BDOS 30 returns 0xFF).
        assert!(fs.set_file_ro(&fcb_named(1, "GONE", "TXT"), true).is_none());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A read-only file must refuse writes from the emulator itself, not merely
    /// from the host: **root bypasses file permissions**, so a gateway running
    /// from systemd as root would otherwise let a guest overwrite a file it is
    /// not allowed to erase or rename.
    #[test]
    fn test_readonly_file_refuses_write_record() {
        let base = temp_base("ro_write");
        let fs = CpmFs::new(base.clone());
        let path = base.join("A").join("LOCK.DAT");
        std::fs::write(&path, vec![b'o'; 128]).unwrap();
        set_ro(&path, true);

        let fcb = fcb_named(1, "LOCK", "DAT");
        let err = fs.write_record(&fcb, 0, &[b'x'; 128]).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            vec![b'o'; 128],
            "the file's contents must be untouched"
        );
        // Reads are unaffected — R/O is a write attribute.
        assert!(fs.read_record(&fcb, 0).unwrap().is_some());

        set_ro(&path, false);
        assert!(fs.write_record(&fcb, 0, &[b'x'; 128]).is_ok());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Clearing the R/O attribute must not widen host permissions.
    /// `Permissions::set_readonly(false)` sets *every* write bit on Unix, so
    /// the obvious spelling would turn a private 0o600 file into a
    /// world-writable 0o666 one just because a guest cleared t1'.
    #[cfg(unix)]
    #[test]
    fn test_clearing_ro_does_not_go_world_writable() {
        use std::os::unix::fs::PermissionsExt;
        let base = temp_base("ro_perms");
        let path = base.join("A").join("PRIV.TXT");
        std::fs::write(&path, b"secret").unwrap();
        // A deliberately private file.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        CpmFs::set_host_ro(&path, true).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o400, "setting R/O clears every write bit");
        assert!(CpmFs::host_is_ro(&path));

        CpmFs::set_host_ro(&path, false).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "clearing R/O must restore owner-write only, not 0o666"
        );
        assert_eq!(mode & 0o022, 0, "group/other must never gain write");
        assert!(!CpmFs::host_is_ro(&path));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// BDOS 28 write-protects the whole drive: all four mutating paths refuse,
    /// reads still work, and BDOS 13/37 release it.
    #[test]
    fn test_drive_write_protect_blocks_mutations() {
        let base = temp_base("ro_drive");
        let mut fs = CpmFs::new(base.clone());
        std::fs::write(base.join("A").join("DATA.TXT"), b"hello").unwrap();
        let fcb = fcb_named(1, "DATA", "TXT");

        assert!(!fs.fcb_drive_is_ro(&fcb));
        assert_eq!(fs.ro_vector(), 0);
        fs.set_drive_ro();
        assert!(fs.fcb_drive_is_ro(&fcb));
        assert_eq!(fs.ro_vector(), 0b1, "A: is bit 0");

        // Every mutating path refuses.
        assert!(fs.write_record(&fcb, 0, &[b'x'; 128]).is_err());
        assert!(fs.make(&fcb_named(1, "NEW", "TXT")).is_none());
        assert_eq!(fs.delete(&fcb), 0);
        let mut nn = [b' '; 8];
        nn[..2].copy_from_slice(b"NX");
        assert!(!fs.rename(&fcb, &nn, b"TXT"));
        // …and nothing on disk changed.
        assert_eq!(
            std::fs::read(base.join("A").join("DATA.TXT")).unwrap(),
            b"hello"
        );
        assert!(!base.join("A").join("NEW.TXT").exists());
        // Reading is unaffected — it's a *write* protect.
        assert!(fs.read_record(&fcb, 0).unwrap().is_some());

        // BDOS 37 with A:'s bit clears just that drive.
        fs.clear_drive_ro(0b1);
        assert!(!fs.fcb_drive_is_ro(&fcb));
        assert_eq!(fs.delete(&fcb), 1);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The write-protect is per drive, and BDOS 13 clears all of them.
    #[test]
    fn test_write_protect_is_per_drive() {
        let base = temp_base("ro_perdrive");
        std::fs::create_dir_all(base.join("C")).unwrap();
        let mut fs = CpmFs::new(base.clone());
        assert!(fs.select(2)); // C:
        fs.set_drive_ro();
        assert_eq!(fs.ro_vector(), 0b100, "C: is bit 2");

        // An FCB naming A: explicitly (drive byte 1) is unaffected.
        assert!(!fs.fcb_drive_is_ro(&fcb_named(1, "X", "TXT")));
        // An FCB naming C: explicitly (drive byte 3) is protected.
        assert!(fs.fcb_drive_is_ro(&fcb_named(3, "X", "TXT")));
        // Bit 37 for a *different* drive leaves C: protected.
        fs.clear_drive_ro(0b10);
        assert_eq!(fs.ro_vector(), 0b100);
        // Reset Disk System clears everything.
        fs.clear_all_drive_ro();
        assert_eq!(fs.ro_vector(), 0);
        let _ = std::fs::remove_dir_all(&base);
    }
}
