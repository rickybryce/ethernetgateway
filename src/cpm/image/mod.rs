//! Mounting a CP/M disk image (`.dsk`) as an emulated drive.
//!
//! When a drive has an image mounted, the emulator reads and writes the CP/M
//! filesystem *inside that image* instead of the drive's host folder under
//! `transfer_dir/CPM/`.  The folder's files are untouched and come back the
//! moment the image is unmounted.
//!
//! The work splits five ways:
//!
//! * [`format`] — geometry.  Where the 128-byte CP/M records sit inside the
//!   file, and the CP/M parameters (block size, directory size, sector skew)
//!   that describe the filesystem laid over them.
//!
//! * [`media`] — the byte store, and the one place every access is
//!   bounds-checked against the real length of the file.
//!
//! * [`fs`] — the filesystem itself: directory entries, extents and allocation
//!   blocks, read and written.
//!
//! * [`identify`] — which format a file is, and whether that answer is trusted
//!   enough to write to it.
//!
//! * [`registry`] — the process-wide table of what is mounted where, and which
//!   drives sessions are using.
//!
//! and this file ties them together: [`mount_image`] opens a `.dsk` and
//! publishes it, [`unmount_drive`] takes it away again, and
//! [`apply_config_mounts`] brings up whatever `cpm_mounts` asks for at startup.
//! The BDOS layer never sees any of it — [`super::fs::CpmFs`] dispatches each
//! operation to the mounted image or to the drive folder, so the emulator's
//! file calls are written once.

pub mod format;
pub mod fs;
pub mod identify;
pub mod media;
pub mod registry;

use std::path::{Path, PathBuf};

/// Folder under the `CPM/` container where operators put their `.dsk` files.
pub const IMAGES_DIR: &str = "images";

/// Is `name` a plausible image filename — a bare name, no path?
///
/// The same rule the file-transfer subsystem applies: no separators, no `..`,
/// nothing hidden.  A mount name arrives from a config file or a web form, so
/// it is the one input here that an attacker could shape, and joining an
/// unvalidated one onto the images folder is a path traversal.
pub fn is_safe_image_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && !name.starts_with('.')
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains("..")
        && !name.contains('\0')
}

/// The images folder inside a `CPM/` container.
pub fn images_dir(cpm_base: &Path) -> PathBuf {
    cpm_base.join(IMAGES_DIR)
}

/// Formats that a blank image can be created in, as `(token, label)`.
///
/// Not simply every entry in `FORMATS` — a format is offered only where we know
/// what a blank one contains, and the two kinds of knowing are worth keeping
/// apart:
///
/// * An unframed format has no per-sector headers to author, so a blank really
///   is nothing but `0xE5` and the whole question is arithmetic.  `ibm3740` and
///   `altairhd` are here on that reasoning; neither blank has been read back by
///   real hardware.
/// * `altair8` has headers, sector IDs and two checksums, and none of that
///   could be reasoned out.  Its blank is pinned by hash against what MITS's
///   own `FORMAT.COM` produced inside a booted Altair.
///
/// What is *not* offered is a format where neither applies, because the
/// alternative is handing someone a file that mounts, looks empty, and is
/// rejected by the first machine that reads it.
pub fn creatable_formats() -> Vec<(&'static str, &'static str)> {
    format::FORMATS
        .iter()
        .filter(|f| f.can_make_blank())
        .map(|f| (f.token, f.label))
        .collect()
}

/// Create a new blank, formatted image in the images folder.
///
/// The filename is built rather than taken: `<token>_<name>.dsk`, so a created
/// image always says outright what format it is.
///
/// That mattered more when a name without a prefix meant a read-only mount; a
/// blank we made ourselves would have come back unwritable, which looks like a
/// bug. It now mounts read-write either way — a freshly formatted directory is
/// consistent, which is exactly what
/// [`identify::directory_is_consistent`] checks — so the prefix is no longer
/// load-bearing here. It is kept because a disk whose name states its format is
/// a kindness to whoever finds it later, and because building the name is what
/// stops a caller inventing one that collides.
///
/// Refuses to overwrite. Creating a disk is the one operation here that has an
/// obvious destructive spelling — "make me a fresh disk called BACKUP" — and
/// there is no undo for the disk that used to have that name.
pub fn create_blank_image(
    cpm_base: &Path,
    token: &str,
    name: &str,
) -> Result<String, String> {
    let fmt = format::by_token(token).ok_or_else(|| format!("no format called '{token}'"))?;
    let blank = fmt
        .blank_image()
        .ok_or_else(|| format!("{}: no measured blank layout for this format", fmt.token))?;

    // The operator names the disk, not the file: strip anything that would make
    // the assembled name unsafe or unparseable rather than rejecting it, since
    // the token before the first underscore is what selects the format and a
    // second underscore would not change that.
    let stem: String = name
        .trim()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' })
        .collect();
    let stem = stem.trim_matches('_').to_string();
    if stem.is_empty() {
        return Err("give the disk a name".into());
    }
    let filename = format!("{}_{}.dsk", fmt.token, stem);
    if !is_safe_image_name(&filename) {
        return Err(format!("'{filename}' is not a valid image name"));
    }

    let dir = images_dir(cpm_base);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = dir.join(&filename);
    if path.exists() {
        return Err(format!("{filename} already exists — delete it first"));
    }
    // Two sessions racing must not both believe they made it, and a write that
    // stops partway must not leave anything behind that looks like a disk.
    // Both come from one file: the staging name is what is `create_new`'d, so
    // it is the lock *and* the buffer.  An earlier version claimed the final
    // name with an empty file first, which reintroduced exactly the failure
    // this is here to avoid — a kill during a 4.9 MB write left a 0-byte
    // `.dsk` that the pickers offered, that would not mount, and that could
    // never be recreated because the name was taken.
    //
    // `.creating` is deliberately not one of `IMAGE_EXTENSIONS`, so a leftover
    // is never offered as a disk.
    let mut tmp = path.clone().into_os_string();
    tmp.push(".creating");
    let tmp = PathBuf::from(tmp);
    // A `.creating` file that is not being written any more is debris from a
    // kill or a crash, and it must not block the name for good: it is
    // deliberately not an image extension, so it shows up in none of the three
    // pickers and an operator has no way to see or clear it.  Anything older
    // than this is reclaimed.  The window is generous next to the write it
    // guards — 4.99 MB, milliseconds — and narrow next to a human retrying.
    const STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(60);
    if let Ok(md) = std::fs::metadata(&tmp) {
        let stale = md
            .modified()
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > STALE_AFTER);
        if stale {
            crate::glog!("CP/M: clearing an abandoned {}", tmp.display());
            let _ = std::fs::remove_file(&tmp);
        }
    }
    let staged = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .map_err(|e| format!("{filename}: another session may be creating this ({e})"))?;

    let write = (|| -> std::io::Result<()> {
        use std::io::Write;
        let mut f = staged;
        f.write_all(&blank)?;
        f.flush()?;
        drop(f);
        // Re-check under the staging lock: `exists` above was before the long
        // write, and renaming over a disk somebody made meanwhile would destroy
        // it with no undo.
        if path.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "a disk with that name appeared while this one was being made",
            ));
        }
        std::fs::rename(&tmp, &path)
    })();
    if let Err(e) = write {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("{filename}: {e}"));
    }

    Ok(format!(
        "created {filename} — {}, empty and ready to mount",
        fmt.label
    ))
}

/// What a drive is doing that stops it being changed, for the mount screens.
///
/// A lent drive reads as *empty* in the mount table, so without this the three
/// screens show a booted session's drives as free and enabled, and the operator
/// gets an unexplained refusal when they press Save.  One function, because
/// three screens phrasing the same state three ways is how they drift.
pub fn drive_held_note(drive0: u8) -> Option<String> {
    registry::boot_loans()
        .into_iter()
        .find(|(d, _)| *d == drive0)
        .map(|(_, name)| format!("{name} — held by a booted disk"))
}

/// Open an image and publish it on a drive.
///
/// Ties the pieces together: validate the name, identify the format, open the
/// file, mount the filesystem, and put it in the registry where every session
/// will find it.  Returns a human-readable note about the mount — chiefly
/// whether it came up read-only and why — for the UIs to show.
///
/// A drive somebody is using is refused; see [`registry::check_can_change`].
pub fn mount_image(cpm_base: &Path, drive0: u8, filename: &str) -> Result<String, String> {
    // The name is validated *first*, before anything looks at a drive or a
    // file.  A traversing name is not a "sorry, that drive is busy" situation,
    // and answering it with one both hides the real problem and makes the
    // refusal depend on unrelated state.  Moving the check inside the shared
    // helper had quietly reordered this.
    if !is_safe_image_name(filename) {
        return Err(format!("'{filename}' is not a valid image name"));
    }
    registry::check_can_change(drive0)?;
    // An image somebody is running cannot also be mounted.  A booted session
    // holds the whole disk in memory and writes it back over the file when it
    // leaves, so a mount made meanwhile would have its work silently replaced —
    // and would afterwards be caching a directory for bytes that are gone.
    // The "one session per image" rule used to be enforced only between boots.
    // The same image on two drives would be two `ImageFs` objects over one
    // file, each caching its own directory and allocation bitmap — so a write
    // through one leaves the other describing bytes that are gone, and its next
    // write allocates blocks the first already used.  The registry's own
    // header states that an image is opened once and shared; this is what makes
    // that true.  The boot path has enforced its half of the rule since the
    // claim moved into the registry; this is the mount-against-mount half.
    // ...on *another* drive.  Re-mounting an image onto the drive it is
    // already on is a re-read, which is what the telnet screen does when an
    // operator picks the disk that drive already shows — and answering that
    // with "unmount it from B: first" while they are mounting onto B: is
    // nonsense.  Only a second, different drive is the two-views hazard.
    if let Some(other) = registry::drive_holding(filename).filter(|d| *d != drive0) {
        return Err(format!(
            "{filename} is already mounted on drive {}: — unmount it there first",
            (b'A' + other) as char
        ));
    }
    if registry::is_image_booted(&images_dir(cpm_base).join(filename)) {
        return Err(format!(
            "{filename} is being run by a booted session — it cannot be mounted at the same time"
        ));
    }
    mount_image_unchecked(cpm_base, drive0, filename)
}

/// Put back a mount a booted session borrowed.
///
/// The same work as [`mount_image`] without the in-use check, because this is
/// not a change anybody may veto: the drive was already the operator's, and it
/// is being restored to what it was.  A lent drive reads as *empty*, so another
/// session can park on it while the boot runs — and if that were allowed to
/// refuse the restore, the drive would end up neither mounted nor lent and
/// would vanish from `cpm_mounts` on the next save from any screen.
pub fn restore_mount(cpm_base: &Path, drive0: u8, filename: &str) -> Result<String, String> {
    mount_image_unchecked(cpm_base, drive0, filename)
}

fn mount_image_unchecked(cpm_base: &Path, drive0: u8, filename: &str) -> Result<String, String> {
    if !is_safe_image_name(filename) {
        return Err(format!("'{filename}' is not a valid image name"));
    }

    let path = images_dir(cpm_base).join(filename);
    let size = std::fs::metadata(&path)
        .map_err(|e| format!("{filename}: {e}"))?
        .len();

    // Identification needs to read the first directory record *as each
    // candidate format would address it*, so the file is opened first and the
    // closure seeks per candidate.
    let mut probe = media::FileMedia::open(&path, true).map_err(|e| format!("{filename}: {e}"))?;
    // The *whole* directory, as each candidate format would address it — not
    // just its first record. Identification needs both: one record says "there
    // is a directory here", and the whole of it says "and it is consistent
    // enough to write to", which is what lets an unlabelled disk be mounted
    // read-write instead of sending its owner off to rename the file.
    let ident = identify::identify(filename, size, |fmt| {
        let mut dir = Vec::with_capacity(fmt.maxdir as usize * 32);
        for rec in 0..fmt.dir_records() {
            let off = fmt.data_record_offset(rec)?;
            let mut buf = [0u8; 128];
            media::Media::read_at(&mut probe, off, &mut buf).ok()?;
            dir.extend_from_slice(&buf);
        }
        (!dir.is_empty()).then_some(dir)
    })
    .map_err(|e| format!("{filename}: {e}"))?;
    drop(probe);

    // Three separate things can force read-only, and the operator is told
    // which: a guessed format, a file the host will not let us write, or a
    // directory that arrived damaged (decided inside `ImageFs::mount`).
    let host_ro = std::fs::metadata(&path)
        .map(|m| m.permissions().readonly())
        .unwrap_or(false);
    let want_ro = ident.force_read_only() || host_ro;

    let medium = media::FileMedia::open(&path, want_ro).map_err(|e| format!("{filename}: {e}"))?;
    let image = fs::ImageFs::mount(Box::new(medium), ident.format, want_ro)
        .map_err(|e| format!("{filename}: {e}"))?;
    let read_only = image.is_read_only();

    let reason = if !read_only {
        String::new()
    } else if ident.force_read_only() {
        // Say what the filesystem check objected to, not just that it did.
        // "Rename it" was the only advice this could give while identification
        // by inspection was never trusted; now that a sound filesystem mounts
        // read-write, a refusal means something specific was wrong, and the
        // operator can act on the specific thing.
        match ident.why {
            Some(why) => format!(
                "this image was identified by inspection and its CP/M directory \
                 has {why} — so writing to it is not safe. If you know the format, \
                 rename it with the prefix to override."
            ),
            None => "the filename does not say which format this is, so it was \
                     identified by inspection — rename it with a format prefix to \
                     allow writing"
                .to_string(),
        }
    } else if host_ro {
        "the image file is read-only on the host".to_string()
    } else {
        "the CP/M directory in this image is damaged".to_string()
    };

    let mount = registry::Mount {
        path: path.clone(),
        filename: filename.to_string(),
        format: ident.format.token,
        read_only,
        read_only_reason: reason.clone(),
        host_read_only: host_ro,
        fs: std::sync::Arc::new(std::sync::Mutex::new(image)),
    };
    registry::mount(drive0, mount)?;

    let drive = (b'A' + drive0) as char;
    crate::glog!(
        "CP/M: mounted {} on drive {}: as {}{}",
        filename,
        drive,
        ident.format.token,
        if read_only { " (read-only)" } else { "" }
    );
    Ok(if read_only {
        format!("{drive}: {filename} — read-only: {reason}")
    } else {
        format!("{drive}: {filename} ({})", ident.format.label)
    })
}

/// Take the image off a drive, so its host folder is visible again.
// Called by the mount UIs, which land in the next step; the mount side is
// already reached from `apply_config_mounts`.
pub fn unmount_drive(drive0: u8) -> Result<String, String> {
    registry::check_can_change(drive0)?;
    let drive = (b'A' + drive0) as char;
    match registry::unmount(drive0) {
        Some(m) => {
            crate::glog!("CP/M: unmounted {} from drive {}:", m.filename, drive);
            Ok(format!(
                "{drive}: {} unmounted — the drive folder's files are visible again",
                m.filename
            ))
        }
        None => Err(format!("drive {drive}: has no image mounted")),
    }
}

/// Parse the `cpm_mounts` config value into (drive index, filename) pairs.
///
/// Format is `A=name.dsk,C=other.dsk`.  Anything unparseable is dropped rather
/// than failing the whole line: a config file is hand-edited, and one bad entry
/// should cost that one mount, not every mount.
pub fn parse_mounts(value: &str) -> Vec<(u8, String)> {
    let mut out: Vec<(u8, String)> = Vec::new();
    for item in value.split(',') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let Some((drive, name)) = item.split_once('=') else {
            continue;
        };
        let drive = drive.trim();
        let name = name.trim();
        let mut chars = drive.chars();
        let (Some(letter), None) = (chars.next(), chars.next()) else {
            continue;
        };
        if !letter.is_ascii_alphabetic() {
            continue;
        }
        let drive0 = letter.to_ascii_uppercase() as u8 - b'A';
        if drive0 >= crate::cpm::fs::NUM_DRIVES || !is_safe_image_name(name) {
            continue;
        }
        // Last entry wins for a repeated drive, so a hand-edited file cannot
        // produce two mounts on one drive.
        out.retain(|(d, _)| *d != drive0);
        out.push((drive0, name.to_string()));
    }
    out.sort_by_key(|(d, _)| *d);
    out
}

/// Render mounts back into the `cpm_mounts` config form.
// Written by the mount UIs when they save; see `unmount_drive`.
/// Render a mount list as the `cpm_mounts` config value.
///
/// **Private on purpose.** Building this value is the one job that has to
/// happen in exactly one place, and it has now been got wrong three times by
/// three different screens each assembling it from `registry::all()` — which
/// omits a drive lent to a booted session, so saving from that screen dropped
/// somebody's drives out of their configuration. The compiler enforces the rule
/// that comments and reviews did not: every caller goes through
/// [`current_mounts_value`], which is where the loans are merged back in.
fn format_mounts(mounts: &[(u8, String)]) -> String {
    mounts
        .iter()
        .map(|(d, n)| format!("{}={}", (b'A' + d) as char, n))
        .collect::<Vec<_>>()
        .join(",")
}

/// Mount everything `cpm_mounts` asks for.
///
/// Run when the emulator starts up.  A mount that fails is logged and skipped —
/// an image that has been deleted or renamed must not stop the other drives, or
/// the emulator, from coming up.
pub fn apply_config_mounts(cpm_base: &Path, value: &str) {
    for (drive0, name) in parse_mounts(value) {
        // Already mounted from a previous session's startup: leave it be, so
        // re-entering the emulator does not reopen a disk somebody is using.
        if registry::get(drive0).is_some_and(|m| m.filename == name) {
            continue;
        }
        if let Err(e) = mount_image(cpm_base, drive0, &name) {
            crate::glog!("CP/M: could not mount {} on {}: {}", name, (b'A' + drive0) as char, e);
        }
    }
}

/// Make the live mount table match `desired`, and report what happened.
///
/// The mount screens hand over the whole set of sixteen drives rather than
/// individual changes, so this works out the difference: a drive whose image
/// did not change is left strictly alone (remounting would drop and reopen a
/// disk somebody may be using), one that lost its image is unmounted, and one
/// that gained or changed image is mounted.
///
/// Shared by the web and desktop screens on purpose.  They present the same set
/// of controls, and letting each work out its own diff is exactly how two
/// surfaces come to disagree about what "no change" means.
///
/// Returns (notes, errors) — both human-readable, both possibly non-empty at
/// once, because one drive being busy must not stop the other fifteen.
pub fn apply_mount_selection(
    cpm_base: &Path,
    desired: &[(u8, String)],
) -> (Vec<String>, Vec<String>) {
    let mut notes = Vec::new();
    let mut errors = Vec::new();
    let current = registry::all();
    for drive0 in 0..crate::cpm::NUM_DRIVES {
        let want = desired
            .iter()
            .find(|(d, _)| *d == drive0)
            .map(|(_, n)| n.as_str());
        let have = current
            .get(drive0 as usize)
            .and_then(|m| m.as_ref())
            .map(|m| m.filename.as_str());
        match (have, want) {
            // Unchanged, including both-absent: touch nothing.
            (a, b) if a == b => {}
            (Some(_), None) => match unmount_drive(drive0) {
                Ok(n) => notes.push(n),
                Err(e) => errors.push(e),
            },
            (_, Some(name)) => {
                // Replacing one image with another: take the old one off first
                // so the drive is not briefly holding two.
                if have.is_some() {
                    if let Err(e) = unmount_drive(drive0) {
                        errors.push(e);
                        continue;
                    }
                }
                match mount_image(cpm_base, drive0, name) {
                    Ok(n) => notes.push(n),
                    Err(e) => errors.push(e),
                }
            }
            (None, None) => {}
        }
    }
    (notes, errors)
}

/// The live mount table in `cpm_mounts` form, for writing back to the config.
pub fn current_mounts_value() -> String {
    // With the emulator disabled the mount table is deliberately cleared and
    // nothing brings it back, so the live table is not authoritative — and
    // persisting it would rewrite `cpm_mounts` as empty and lose the operator's
    // drives the moment they turned CP/M off and saved anything. Report what
    // the configuration says instead. One place, because all three screens
    // persist through here.
    let cfg = crate::config::get_config();
    if !cfg.cpm_emu_enabled {
        return cfg.cpm_mounts.clone();
    }
    let mut mounts: Vec<(u8, String)> = registry::all()
        .iter()
        .enumerate()
        .filter_map(|(i, m)| m.as_ref().map(|m| (i as u8, m.filename.clone())))
        .collect();
    // Drives a booted session is holding are still the operator's mounts — they
    // are simply out of service until it ends.  Leaving them out here would let
    // any save made during a boot rewrite `cpm_mounts` without them, and the
    // configuration would come back short after a restart.
    mounts.extend(registry::boot_loans());
    mounts.sort_by_key(|(d, _)| *d);
    // A drive is briefly in both tables while a booted session hands it back —
    // the restore publishes the mount before the loan ends, deliberately, so
    // the drive is never in neither. Emit it once.
    mounts.dedup_by_key(|(d, _)| *d);
    format_mounts(&mounts)
}

/// Extensions the mount pickers offer.
///
/// Filtering by extension rather than excluding `readme.txt` by name: the
/// folder is a place people keep notes, and every one of those files would
/// otherwise be offered as a disk and then refused for its size.
const IMAGE_EXTENSIONS: &[&str] = &["dsk", "img", "ima", "image", "cpm"];

/// Does this filename look like a disk image rather than a note?
pub fn looks_like_an_image_name(name: &str) -> bool {
    match name.rsplit_once('.') {
        Some((_, ext)) => IMAGE_EXTENSIONS
            .iter()
            .any(|known| ext.eq_ignore_ascii_case(known)),
        None => false,
    }
}

/// Every `.dsk` sitting in the images folder, sorted, for the mount pickers.
// Read by the mount UIs; see `unmount_drive`.
pub fn available_images(cpm_base: &Path) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(images_dir(cpm_base)) {
        for e in rd.flatten() {
            if !e.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let name = e.file_name().to_string_lossy().to_string();
            if is_safe_image_name(&name) && looks_like_an_image_name(&name) {
                out.push(name);
            }
        }
    }
    out.sort_by_key(|a| a.to_lowercase());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of the backend switch: a `CpmFs` aimed at a drive with
    /// an image mounted must read and write the image, and the drive's host
    /// folder must be untouched and still there when the image is unmounted.
    #[test]
    fn test_cpmfs_reads_and_writes_through_a_mounted_image() {
        use crate::cpm::fcb::Fcb;
        use crate::cpm::fs::CpmFs;

        let _g = registry::tests_lock();
        registry::tests_reset();

        // A CPM/ container with an images folder and a blank named image.
        let base = std::env::temp_dir().join("egw_backend_switch");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(images_dir(&base)).unwrap();
        std::fs::create_dir_all(base.join("B")).unwrap();
        let fmt = format::by_token("ibm3740").unwrap();
        std::fs::write(
            images_dir(&base).join("ibm3740_test.dsk"),
            vec![0xE5u8; fmt.min_bytes() as usize],
        )
        .unwrap();
        // A file in the *folder* for drive B:, which must be hidden while the
        // image is mounted and must come back afterwards.
        std::fs::write(base.join("B").join("FOLDER.TXT"), b"from the folder").unwrap();

        let note = mount_image(&base, 1, "ibm3740_test.dsk").expect("mount");
        assert!(note.contains("ibm3740"), "{note}");
        assert!(!note.contains("read-only"), "a named image is writable: {note}");

        let fs = CpmFs::new(base.clone());
        let fcb = |n: &str, x: &str| {
            let mut raw = [0u8; 36];
            raw[0] = 2; // B:
            for (s, c) in raw[1..9].iter_mut().zip(n.bytes().chain(std::iter::repeat(b' '))) {
                *s = c;
            }
            for (s, c) in raw[9..12].iter_mut().zip(x.bytes().chain(std::iter::repeat(b' '))) {
                *s = c;
            }
            Fcb::from_bytes(&raw)
        };

        // The folder's file must NOT be visible: the image replaced the drive.
        assert!(
            !fs.open_existing(&fcb("FOLDER", "TXT")),
            "the drive folder must be hidden while an image is mounted"
        );

        // Create, write and read back through the ordinary CpmFs API.
        let f = fcb("HELLO", "TXT");
        assert!(fs.make(&f), "make on an image-backed drive");
        let mut rec = [0x1Au8; 128];
        rec[..5].copy_from_slice(b"image");
        fs.write_record(&f, 0, &rec).unwrap();
        assert!(fs.open_existing(&f));
        assert_eq!(fs.file_size_records(&f), Some(1));
        assert_eq!(fs.read_record(&f, 0).unwrap().unwrap(), rec);
        assert_eq!(fs.list_matching(&fcb("?????", "???")), vec!["HELLO.TXT"]);

        // It really landed in the image file, not in the folder.
        drop(fs);
        assert!(
            !base.join("B").join("HELLO.TXT").exists(),
            "the write must have gone into the image, not the drive folder"
        );

        // Unmount: the folder's own file is back, and the image's is gone.
        unmount_drive(1).expect("unmount");
        let fs = CpmFs::new(base.clone());
        assert!(
            fs.open_existing(&fcb("FOLDER", "TXT")),
            "the folder's files must return when the image is unmounted"
        );
        assert!(!fs.open_existing(&fcb("HELLO", "TXT")));

        drop(fs);
        registry::tests_reset();
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Two sessions must not interleave records into one file inside a mounted
    /// image, exactly as they cannot on a folder-backed drive.
    ///
    /// The image's mutex makes each *record* atomic but says nothing about a
    /// whole file, so without the write claim two uploads into one name would
    /// silently produce a file made of both.
    #[test]
    fn test_two_sessions_cannot_write_one_file_in_an_image() {
        use crate::cpm::fcb::Fcb;
        use crate::cpm::fs::CpmFs;

        let _g = registry::tests_lock();
        registry::tests_reset();
        let base = std::env::temp_dir().join("egw_image_claim");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(images_dir(&base)).unwrap();
        let fmt = format::by_token("ibm3740").unwrap();
        std::fs::write(
            images_dir(&base).join("ibm3740_claim.dsk"),
            vec![0xE5u8; fmt.min_bytes() as usize],
        )
        .unwrap();
        mount_image(&base, 15, "ibm3740_claim.dsk").expect("mount");

        let mut raw = [0u8; 36];
        raw[0] = 16; // P:
        raw[1..9].copy_from_slice(b"SHARED  ");
        raw[9..12].copy_from_slice(b"DAT");
        let fcb = Fcb::from_bytes(&raw);

        let one = CpmFs::new(base.clone());
        let two = CpmFs::new(base.clone());
        assert!(one.make(&fcb), "first session creates the file");
        assert!(one.write_record(&fcb, 0, &[1u8; 128]).is_ok());
        assert!(
            two.write_record(&fcb, 0, &[2u8; 128]).is_err(),
            "a second session must be refused while the first holds the file"
        );

        // ...and once the first lets go, the second may have it.
        one.release_file(&fcb);
        assert!(two.write_record(&fcb, 0, &[2u8; 128]).is_ok());

        drop(one);
        drop(two);
        registry::tests_reset();
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A mounted image's free space must survive the round trip into the
    /// virtual disk's units and back.
    ///
    /// The first version of this reported a floppy with 32 KB free as
    /// completely full, because it returned an absolute used-block count while
    /// the caller adds the directory reserve on top — pushing the total into
    /// its clamp.  The number STAT prints is the free one, so an off-by-the-
    /// directory error there is the difference between a usable disk and one
    /// that looks like it has no room at all.
    #[test]
    fn test_mounted_image_free_space_survives_the_unit_conversion() {
        use crate::cpm::fs::CpmFs;

        let _g = registry::tests_lock();
        registry::tests_reset();
        let base = std::env::temp_dir().join("egw_freespace");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(images_dir(&base)).unwrap();
        let fmt = format::by_token("ibm3740").unwrap();
        std::fs::write(
            images_dir(&base).join("ibm3740_free.dsk"),
            vec![0xE5u8; fmt.min_bytes() as usize],
        )
        .unwrap();
        // P:, not A: — every default `CpmFs` sits on A:, so mounting there
        // races any other test module that has one alive.
        mount_image(&base, 15, "ibm3740_free.dsk").expect("mount");

        // The same numbers the BDOS allocation-vector call uses.
        const VD_BLS: u64 = 4096;
        const TOTAL: u64 = 2048;
        const DIR: u64 = 8;

        let mut fs = CpmFs::new(base.clone());
        fs.select(15);
        let used = (DIR + fs.current_drive_used_blocks(VD_BLS, TOTAL, DIR)).min(TOTAL);
        let free_bytes = (TOTAL - used) * VD_BLS;

        // A blank 8" SSSD holds about 241K of files.
        assert!(
            free_bytes > 200_000 && free_bytes < 260_000,
            "a blank floppy should report roughly its capacity free, got {free_bytes}"
        );

        drop(fs);
        registry::tests_reset();
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A disk with no room must report no room.
    ///
    /// Reported free space is what STAT prints and what a program checks before
    /// writing, so an optimistic figure invites a write that then fails.  The
    /// bitmap was already right — this pins the conversion into the virtual
    /// disk's units, which is where the error was.
    #[test]
    fn test_a_full_image_reports_no_free_space() {
        use crate::cpm::fs::CpmFs;

        let _g = registry::tests_lock();
        registry::tests_reset();
        let base = std::env::temp_dir().join("egw_fullspace");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(images_dir(&base)).unwrap();
        let fmt = format::by_token("ibm3740").unwrap();
        std::fs::write(
            images_dir(&base).join("ibm3740_full.dsk"),
            vec![0xE5u8; fmt.min_bytes() as usize],
        )
        .unwrap();
        mount_image(&base, 15, "ibm3740_full.dsk").expect("mount");

        // Fill it.
        {
            let m = registry::get(15).unwrap();
            let mut img = m.fs.lock().unwrap();
            let mut n = [b' '; 8];
            n[..4].copy_from_slice(b"HOG1");
            let e = *b"DAT";
            img.create(0, &n, &e).unwrap();
            let mut rec = 0u32;
            while img.write_record(0, &n, &e, rec, &[0xEE; 128]).is_ok() {
                rec += 1;
                assert!(rec < 100_000, "never filled");
            }
            assert_eq!(img.free_blocks(), 0, "the disk really is full");
        }

        const VD_BLS: u64 = 4096;
        const TOTAL: u64 = 2048;
        const DIR: u64 = 8;
        let mut fs = CpmFs::new(base.clone());
        fs.select(15);
        let used = (DIR + fs.current_drive_used_blocks(VD_BLS, TOTAL, DIR)).min(TOTAL);
        let free_bytes = (TOTAL - used) * VD_BLS;
        assert_eq!(free_bytes, 0, "a full disk must report zero free");

        drop(fs);
        registry::tests_reset();
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A drive somebody is sitting on must not have its disk changed underneath
    /// them.
    #[test]
    fn test_mount_change_is_refused_on_a_busy_drive() {
        let _g = registry::tests_lock();
        registry::tests_reset();
        registry::session_start(4242);
        registry::session_select(4242, 3);

        let err = mount_image(Path::new("/tmp"), 3, "whatever.dsk").unwrap_err();
        assert!(err.contains("in use"), "{err}");
        let err = unmount_drive(3).unwrap_err();
        assert!(err.contains("in use"), "{err}");

        registry::session_end(4242);
        registry::tests_reset();
    }

    #[test]
    fn test_parse_mounts() {
        assert_eq!(
            parse_mounts("A=one.dsk,C=two.dsk"),
            vec![(0, "one.dsk".to_string()), (2, "two.dsk".to_string())]
        );
        assert_eq!(parse_mounts(""), vec![]);
        assert_eq!(
            parse_mounts(" a = one.dsk "),
            vec![(0, "one.dsk".to_string())],
            "whitespace and case are forgiven"
        );
    }

    /// One malformed entry must cost that entry, not the whole line — a config
    /// file is hand-edited and losing every mount to one typo is not a fair
    /// trade.
    #[test]
    fn test_parse_mounts_drops_only_the_bad_entries() {
        let got = parse_mounts("A=good.dsk,nonsense,ZZ=x.dsk,Q=../escape,B=also.dsk");
        assert_eq!(
            got,
            vec![(0, "good.dsk".to_string()), (1, "also.dsk".to_string())]
        );
    }

    #[test]
    fn test_parse_mounts_refuses_a_drive_past_p() {
        assert_eq!(parse_mounts("Q=x.dsk"), vec![], "P: is the last drive");
        assert_eq!(parse_mounts("P=x.dsk").len(), 1);
    }

    /// Two entries for one drive would otherwise produce two mounts on it.
    #[test]
    fn test_parse_mounts_last_wins_for_a_repeated_drive() {
        assert_eq!(
            parse_mounts("A=first.dsk,A=second.dsk"),
            vec![(0, "second.dsk".to_string())]
        );
    }

    #[test]
    fn test_mounts_round_trip_through_the_config_form() {
        let mounts = vec![(0, "one.dsk".to_string()), (5, "two.dsk".to_string())];
        let text = format_mounts(&mounts);
        assert_eq!(text, "A=one.dsk,F=two.dsk");
        assert_eq!(parse_mounts(&text), mounts);
    }

    /// The images folder is also where people keep notes; those must not be
    /// offered as disks and then refused for their size.
    #[test]
    fn test_only_image_like_names_are_offered() {
        assert!(looks_like_an_image_name("ibm3740_cpm22.dsk"));
        assert!(looks_like_an_image_name("DISK01.DSK"));
        assert!(looks_like_an_image_name("hd.img"));
        assert!(!looks_like_an_image_name("readme.txt"));
        assert!(!looks_like_an_image_name("images-catalogue.txt"));
        assert!(!looks_like_an_image_name("notes"));
    }

    #[test]
    fn test_safe_image_names() {
        assert!(is_safe_image_name("altair8_games.dsk"));
        assert!(is_safe_image_name("DISK01.DSK"));
        assert!(!is_safe_image_name(""));
        assert!(!is_safe_image_name(".hidden"));
        assert!(!is_safe_image_name("../../etc/passwd"));
        assert!(!is_safe_image_name("sub/dir.dsk"));
        assert!(!is_safe_image_name("back\\slash.dsk"));
        assert!(!is_safe_image_name("nul\0byte.dsk"));
    }

    /// A created disk must land in the images folder under a name that mounts
    /// read-write — which means carrying the format prefix.  A blank disk you
    /// cannot write to would be a puzzle, not a feature.
    #[test]
    fn test_create_blank_names_the_file_so_it_mounts_read_write() {
        let base = std::env::temp_dir().join("egw_create_blank_name");
        let _ = std::fs::remove_dir_all(&base);
        let note = create_blank_image(&base, "altair8", "scratch").expect("creates");
        assert!(note.contains("altair8_scratch.dsk"), "{note}");
        let made = images_dir(&base).join("altair8_scratch.dsk");
        assert_eq!(
            std::fs::metadata(&made).unwrap().len(),
            337_568,
            "a whole floppy, not a sparse file"
        );
        // The name is what makes it writable: identification by prefix rather
        // than by sniffing.
        assert_eq!(format::token_of("altair8_scratch.dsk"), Some("altair8"));
        assert!(available_images(&base).contains(&"altair8_scratch.dsk".to_string()));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Creating over an existing disk would destroy it with no undo, so it is
    /// refused rather than confirmed — the operator can delete and retry.
    #[test]
    fn test_create_blank_never_overwrites() {
        let base = std::env::temp_dir().join("egw_create_blank_clobber");
        let _ = std::fs::remove_dir_all(&base);
        create_blank_image(&base, "altair8", "keepme").expect("first one works");
        let path = images_dir(&base).join("altair8_keepme.dsk");
        std::fs::write(&path, b"precious").unwrap();
        let err = create_blank_image(&base, "altair8", "keepme").unwrap_err();
        assert!(err.contains("already exists"), "{err}");
        assert_eq!(std::fs::read(&path).unwrap(), b"precious", "the old disk survived");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The operator names a disk, not a file.  Anything that would make the
    /// assembled filename unsafe is folded away rather than rejected, but the
    /// result still has to be a safe bare name and still has to start with the
    /// format token — a traversal typed into the name box must not become one.
    #[test]
    fn test_create_blank_sanitises_the_name() {
        let base = std::env::temp_dir().join("egw_create_blank_sanitise");
        let _ = std::fs::remove_dir_all(&base);
        for (typed, want) in [
            ("my disk", "altair8_my_disk.dsk"),
            ("../../etc/passwd", "altair8_etc_passwd.dsk"),
            ("  spaced  ", "altair8_spaced.dsk"),
            ("keep-dashes", "altair8_keep-dashes.dsk"),
        ] {
            let note = create_blank_image(&base, "altair8", typed)
                .unwrap_or_else(|e| panic!("{typed:?}: {e}"));
            assert!(note.contains(want), "{typed:?} became {note}");
            assert!(images_dir(&base).join(want).exists(), "{want} is not there");
        }
        // A name that sanitises away entirely is a refusal, not a file called
        // "altair8_.dsk".
        assert!(create_blank_image(&base, "altair8", "///").is_err());
        assert!(create_blank_image(&base, "altair8", "   ").is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Only formats whose blank layout has actually been measured are offered.
    /// Anything else would hand out a file that mounts, looks empty, and is
    /// refused by the first real machine that reads it.
    #[test]
    fn test_only_measured_formats_can_be_created() {
        let offered = creatable_formats();
        assert!(!offered.is_empty(), "something must be creatable");
        // Asserting `blank_image().is_some()` here would have been circular —
        // `creatable_formats` filters on `can_make_blank` and `blank_image`
        // opens with the same check, so the loop could not fail for any input.
        // What is worth pinning is that every offered format really does
        // produce a whole, correctly-sized disk.
        for (token, _) in &offered {
            let fmt = format::by_token(token).unwrap();
            let blank = fmt.blank_image().unwrap_or_else(|| panic!("{token} makes no blank"));
            assert_eq!(
                blank.len() as u64,
                fmt.min_bytes(),
                "{token}: the blank is not the size its own geometry needs"
            );
            if let Some(size) = fmt.exact_size {
                assert_eq!(blank.len() as u64, size, "{token}");
            }
        }
        assert!(create_blank_image(Path::new("/tmp"), "nosuchformat", "x")
            .unwrap_err()
            .contains("no format called"));
    }

    /// A created disk must be mountable, empty, and writable — the three things
    /// the operator is going to try in the next thirty seconds.
    #[test]
    fn test_a_created_disk_mounts_empty_and_writable() {
        let _g = registry::tests_lock();
        registry::tests_reset();
        let base = std::env::temp_dir().join("egw_create_blank_mount");
        let _ = std::fs::remove_dir_all(&base);
        create_blank_image(&base, "altair8", "fresh").expect("creates");

        let note = mount_image(&base, 1, "altair8_fresh.dsk").expect("mounts");
        assert!(!note.contains("read-only"), "a disk we just made must be writable: {note}");
        let m = registry::get(1).expect("registered");
        assert!(!m.read_only);
        let mut guard = m.fs.lock().unwrap();
        assert!(guard.entries().is_empty(), "a new disk has no files on it");
        let (n, e) = (*b"HELLO   ", *b"TXT");
        guard.create(0, &n, &e).expect("creates a file");
        guard.write_record(0, &n, &e, 0, &[b'x'; 128]).expect("writes");
        assert_eq!(guard.read_record(0, &n, &e, 0).unwrap().unwrap(), [b'x'; 128]);
        drop(guard);

        let _ = unmount_drive(1);
        registry::tests_reset();
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A drive lent to a booted session must still count as the operator's
    /// mount when the configuration is written.
    ///
    /// Otherwise any save made while somebody is booted — from any of the three
    /// UIs, including one that only changed a different drive — rewrites
    /// `cpm_mounts` without the lent drives, and the operator's configuration
    /// comes back short after a restart.  The drive is out of service, not
    /// forgotten.
    #[test]
    fn test_a_lent_drive_survives_a_config_save() {
        let _g = registry::tests_lock();
        registry::tests_reset();
        let base = std::env::temp_dir().join("egw_lent_drive_config");
        let _ = std::fs::remove_dir_all(&base);
        let images = images_dir(&base);
        std::fs::create_dir_all(&images).unwrap();
        let blank = format::by_token("altair8").unwrap().blank_image().unwrap();
        std::fs::write(images.join("altair8_one.dsk"), &blank).unwrap();
        std::fs::write(images.join("altair8_two.dsk"), &blank).unwrap();
        mount_image(&base, 1, "altair8_one.dsk").unwrap();
        mount_image(&base, 2, "altair8_two.dsk").unwrap();
        let full = current_mounts_value();
        assert_eq!(full, "B=altair8_one.dsk,C=altair8_two.dsk");

        // A booted session takes B:.
        let lent = registry::lend_for_boot(1).expect("lent");
        assert_eq!(lent.filename, "altair8_one.dsk");
        assert!(registry::get(1).is_none(), "the live mount is out of service");
        assert_eq!(
            current_mounts_value(),
            full,
            "a lent drive must still be in the configuration"
        );
        // And nobody may mount over it while it is lent.
        let err = mount_image(&base, 1, "altair8_two.dsk").unwrap_err();
        assert!(err.contains("held by a booted disk"), "{err}");

        registry::end_boot_loan(1);
        mount_image(&base, 1, "altair8_one.dsk").unwrap();
        assert_eq!(current_mounts_value(), full);
        registry::tests_reset();
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Putting a borrowed mount back is a restore, not a change, and nobody
    /// may veto it.
    ///
    /// A lent drive reads as *empty*, so while a boot runs another session can
    /// simply park on that drive.  If the restore went through the ordinary
    /// in-use check it would then be refused, and the drive would end up
    /// neither mounted nor lent — which drops it from `cpm_mounts` on the next
    /// save from any screen, losing the operator's configuration for good.
    #[test]
    fn test_restoring_a_borrowed_mount_cannot_be_refused() {
        let _g = registry::tests_lock();
        registry::tests_reset();
        let base = std::env::temp_dir().join("egw_restore_not_vetoable");
        let _ = std::fs::remove_dir_all(&base);
        let images = images_dir(&base);
        std::fs::create_dir_all(&images).unwrap();
        std::fs::write(
            images.join("altair8_borrowed.dsk"),
            format::by_token("altair8").unwrap().blank_image().unwrap(),
        )
        .unwrap();
        mount_image(&base, 1, "altair8_borrowed.dsk").unwrap();

        // A booted session borrows B:.
        registry::lend_for_boot(1).expect("lent");
        registry::end_boot_loan(1);

        // Meanwhile somebody parks on the now-empty-looking drive.
        let squatter = registry::next_session_id();
        registry::session_start(squatter);
        registry::session_select(squatter, 1);
        assert!(
            mount_image(&base, 1, "altair8_borrowed.dsk").is_err(),
            "an ordinary mount is still refused while a session sits there"
        );

        // The restore goes through anyway — it is giving back what was taken.
        restore_mount(&base, 1, "altair8_borrowed.dsk").expect("a restore is not vetoable");
        assert_eq!(
            registry::get(1).map(|m| m.filename),
            Some("altair8_borrowed.dsk".to_string())
        );
        assert_eq!(current_mounts_value(), "B=altair8_borrowed.dsk");

        registry::session_end(squatter);
        registry::tests_reset();
        let _ = std::fs::remove_dir_all(&base);
    }

    /// An image a booted session is running must not also be mountable.
    ///
    /// A booted guest holds the whole disk in memory and writes it back over
    /// the file when it leaves, so a mount made meanwhile has its work silently
    /// replaced — and is afterwards caching a directory for bytes that are
    /// gone.  This case has no loan and no busy mark to catch it: the image
    /// need not be mounted anywhere for a session to boot it.
    #[test]
    fn test_a_booted_image_cannot_be_mounted() {
        let _g = registry::tests_lock();
        registry::tests_reset();
        let base = std::env::temp_dir().join("egw_booted_not_mountable");
        let _ = std::fs::remove_dir_all(&base);
        let images = images_dir(&base);
        std::fs::create_dir_all(&images).unwrap();
        let path = images.join("altair8_running.dsk");
        std::fs::write(&path, format::by_token("altair8").unwrap().blank_image().unwrap()).unwrap();

        let key = registry::claim_booted_image(&path).expect("a free image claims");
        assert!(registry::claim_booted_image(&path).is_none(), "and only once");
        let err = mount_image(&base, 1, "altair8_running.dsk").unwrap_err();
        assert!(err.contains("booted session"), "{err}");

        registry::release_booted_image(&key);
        mount_image(&base, 1, "altair8_running.dsk").expect("mounts once the boot ends");
        registry::tests_reset();
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Disabling the emulator must not steal a loan from a running boot.
    ///
    /// `clear_all` drops the mount table, and it used to drop the loans with
    /// it — so for a session still inside a booted disk the lent drive silently
    /// became folder-backed again, which is how a file ends up half in the
    /// image and half in `CPM/B/`.  Loans belong to the session holding them.
    #[test]
    fn test_clearing_the_mounts_does_not_steal_a_loan() {
        let _g = registry::tests_lock();
        registry::tests_reset();
        let base = std::env::temp_dir().join("egw_clear_keeps_loans");
        let _ = std::fs::remove_dir_all(&base);
        let images = images_dir(&base);
        std::fs::create_dir_all(&images).unwrap();
        std::fs::write(
            images.join("altair8_lent.dsk"),
            format::by_token("altair8").unwrap().blank_image().unwrap(),
        )
        .unwrap();
        mount_image(&base, 1, "altair8_lent.dsk").unwrap();
        registry::lend_for_boot(1).expect("lent to a booted session");
        assert!(registry::is_lent(1));

        registry::clear_all();
        assert!(registry::is_lent(1), "the loan outlives the mount table");

        registry::end_boot_loan(1);
        assert!(!registry::is_lent(1));
        registry::tests_reset();
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A lent drive must be visible as held, in every screen, or the operator
    /// sees a free drive and gets an unexplained refusal on Save.
    #[test]
    fn test_a_lent_drive_is_reported_as_held() {
        let _g = registry::tests_lock();
        registry::tests_reset();
        let base = std::env::temp_dir().join("egw_held_note");
        let _ = std::fs::remove_dir_all(&base);
        let images = images_dir(&base);
        std::fs::create_dir_all(&images).unwrap();
        std::fs::write(
            images.join("altair8_held.dsk"),
            format::by_token("altair8").unwrap().blank_image().unwrap(),
        )
        .unwrap();
        mount_image(&base, 1, "altair8_held.dsk").unwrap();
        assert_eq!(drive_held_note(1), None, "an ordinary mount is not held");

        registry::lend_for_boot(1).expect("lent");
        let note = drive_held_note(1).expect("a lent drive is held");
        assert!(note.contains("altair8_held.dsk"), "{note}");
        assert!(note.contains("booted"), "{note}");
        assert_eq!(drive_held_note(2), None, "and only that drive");

        registry::end_boot_loan(1);
        assert_eq!(drive_held_note(1), None);
        registry::tests_reset();
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Creating a blank must not leave a truncated image behind if the write
    /// cannot finish — it would mount, look plausible, and refuse a retry
    /// because the name is taken.
    #[test]
    fn test_a_created_blank_is_all_there_or_not_there() {
        let base = std::env::temp_dir().join("egw_create_atomic");
        let _ = std::fs::remove_dir_all(&base);
        create_blank_image(&base, "altair8", "whole").expect("creates");
        let made = images_dir(&base).join("altair8_whole.dsk");
        assert_eq!(std::fs::metadata(&made).unwrap().len(), 337_568, "a whole disk");
        // No staging file survives a success.
        assert!(!images_dir(&base).join("altair8_whole.dsk.creating").exists());
        // And what landed really is a formatted disk, checked against the
        // layout rather than against the generator that produced it — comparing
        // to `blank_image()` again would only prove the file was written.
        let img = std::fs::read(&made).unwrap();
        assert_eq!(&img[..3], &[0x80, 0x00, 0x01], "track 0 sector header");
        assert_eq!(img[131], 0xFF, "boot-track stop byte");
        assert_eq!(img[132], 0x80, "boot-track checksum of 128 x 0xE5");
        let data = 6 * 32 * 137; // first sector of track 6
        assert_eq!(img[data + 1], 0, "first data-track sector states its own id");
        assert_eq!(img[data + 4], 0x30, "data-track checksum");
        assert_eq!(img[data + 135], 0xFF, "data-track stop byte");
        assert!(img[data + 7..data + 135].iter().all(|&b| b == 0xE5), "empty");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// `can_make_blank` is what the screens ask, `blank_image` is what actually
    /// builds one, and they must never disagree about which formats work — the
    /// cheap answer existing at all is only safe while it is the same answer.
    #[test]
    fn test_can_make_blank_agrees_with_blank_image() {
        for f in format::FORMATS {
            assert_eq!(
                f.can_make_blank(),
                f.blank_image().is_some(),
                "{}: the cheap check and the real one disagree",
                f.token
            );
        }
    }

    /// One image must not be mounted on two drives.
    ///
    /// That would be two `ImageFs` objects over one file, each caching its own
    /// directory and allocation bitmap — so a write through one leaves the
    /// other describing bytes that are gone, and its next write allocates
    /// blocks the first already used.  The boot path has enforced its half of
    /// "one session per image" since the claim moved into the registry; this is
    /// the mount-against-mount half, and it was the remaining asymmetry.
    #[test]
    fn test_one_image_cannot_be_mounted_on_two_drives() {
        let _g = registry::tests_lock();
        registry::tests_reset();
        let base = std::env::temp_dir().join("egw_no_double_mount");
        let _ = std::fs::remove_dir_all(&base);
        let images = images_dir(&base);
        std::fs::create_dir_all(&images).unwrap();
        let blank = format::by_token("altair8").unwrap().blank_image().unwrap();
        std::fs::write(images.join("altair8_one.dsk"), &blank).unwrap();
        std::fs::write(images.join("altair8_two.dsk"), &blank).unwrap();

        mount_image(&base, 1, "altair8_one.dsk").expect("B: mounts");
        let err = mount_image(&base, 2, "altair8_one.dsk").unwrap_err();
        assert!(err.contains("already mounted on drive B:"), "{err}");
        // A different image is fine...
        mount_image(&base, 2, "altair8_two.dsk").expect("C: takes another image");
        // ...and so is the same image on the drive it is already on.  That is a
        // re-read, not a second view, and the telnet screen does exactly it
        // when an operator picks the disk their drive already shows.  The
        // comment used to claim this and the assertion below it tested
        // something else entirely.
        mount_image(&base, 1, "altair8_one.dsk").expect("B: re-reads its own image");
        assert_eq!(registry::get(1).map(|m| m.filename), Some("altair8_one.dsk".to_string()));
        let _ = unmount_drive(1);
        mount_image(&base, 3, "altair8_one.dsk").expect("free once unmounted");

        // A drive lent to a booted session still counts as holding it.
        registry::lend_for_boot(3).expect("lent");
        let err = mount_image(&base, 4, "altair8_one.dsk").unwrap_err();
        assert!(err.contains("already mounted on drive D:"), "{err}");

        registry::tests_reset();
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Debris from a killed creation must not block that disk name for good.
    ///
    /// The staging file is the lock, and it is deliberately not an image
    /// extension so a half-written disk is never offered by a picker — which
    /// also means an operator cannot see it or clear it without shell access.
    /// A stale one is reclaimed instead.
    #[test]
    fn test_an_abandoned_creation_does_not_block_the_name_forever() {
        let base = std::env::temp_dir().join("egw_stale_creating");
        let _ = std::fs::remove_dir_all(&base);
        let images = images_dir(&base);
        std::fs::create_dir_all(&images).unwrap();
        let debris = images.join("altair8_ghost.dsk.creating");
        std::fs::write(&debris, b"half a disk").unwrap();

        // Fresh debris is treated as a live creation and refused, so two
        // sessions racing still cannot both write the same file.
        let err = create_blank_image(&base, "altair8", "ghost").unwrap_err();
        assert!(err.contains("another session may be creating"), "{err}");

        // Aged past the limit, it is reclaimed and the name works again.
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        filetime_set(&debris, old);
        create_blank_image(&base, "altair8", "ghost").expect("the name is usable again");
        assert_eq!(
            std::fs::metadata(images.join("altair8_ghost.dsk")).unwrap().len(),
            337_568
        );
        assert!(!debris.exists(), "and the debris is gone");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Backdate a file's mtime without pulling in a dependency.
    ///
    /// `File::set_modified` rather than the `utimensat` shim this used to be.
    /// That shim was `#[cfg(unix)]` while its *caller* was not, so the test
    /// above did not compile on Windows at all — a whole platform's build
    /// broken by a test helper, which Linux cannot see and which sat in the
    /// tree until CI said so. The standard library has had this since 1.75 and
    /// it works everywhere, so there is nothing here to gate.
    fn filetime_set(path: &Path, when: std::time::SystemTime) {
        std::fs::File::options()
            .write(true)
            .open(path)
            .and_then(|f| f.set_modified(when))
            .expect("backdate the debris");
    }

    /// A traversal attempt must be refused before anything touches the disk.
    ///
    /// Holds the registry lock even though the name is refused before the table
    /// is reached: "this call cannot reach the global table" is a claim about
    /// today's code path, and the two calls below it in this file are the kind
    /// that can.
    #[test]
    fn test_mount_refuses_a_traversing_name() {
        let _g = registry::tests_lock();
        let err = mount_image(Path::new("/tmp"), 0, "../../etc/passwd").unwrap_err();
        assert!(err.contains("not a valid image name"), "{err}");
    }

    /// Under the lock: this unmounts a real drive in the process-global table,
    /// and "nobody else uses drive 9" is exactly the assumption that made the
    /// mount-form test next door a source of intermittent failures elsewhere.
    #[test]
    fn test_unmount_an_empty_drive_says_so() {
        let _g = registry::tests_lock();
        let err = unmount_drive(9).unwrap_err();
        assert!(err.contains("no image mounted"), "{err}");
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;

    /// Dump the first bytes of one file from one image, using our own reader.
    /// Ignored: set `CPM_DUMP_IMAGE` and `CPM_DUMP_FILE`.
    #[test]
    #[ignore]
    fn dump_one_file_from_an_image() {
        let (Ok(img), Ok(want)) = (
            std::env::var("CPM_DUMP_IMAGE"),
            std::env::var("CPM_DUMP_FILE"),
        ) else {
            eprintln!("set CPM_DUMP_IMAGE and CPM_DUMP_FILE");
            return;
        };
        let path = std::path::PathBuf::from(&img);
        let base = path.parent().unwrap().parent().unwrap().to_path_buf();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let _g = registry::tests_lock();
        registry::tests_reset();
        mount_image(&base, 0, &name).expect("mount");
        let m = registry::get(0).unwrap();
        let mut guard = m.fs.lock().unwrap();
        let (n, e) = {
            let (b, x) = want.split_once('.').unwrap_or((want.as_str(), ""));
            let mut nn = [b' '; 8];
            let mut ee = [b' '; 3];
            for (s, c) in nn.iter_mut().zip(b.bytes()) { *s = c; }
            for (s, c) in ee.iter_mut().zip(x.bytes()) { *s = c; }
            (nn, ee)
        };
        let recs = guard.file_records(0, &n, &e).expect("file exists");
        let r0 = guard.read_record(0, &n, &e, 0).unwrap().unwrap();
        println!("{want}: {recs} records ({} bytes)", recs * 128);
        println!("first 16 bytes: {:02x?}", &r0[..16]);
        drop(guard);
        registry::tests_reset();
    }

    /// Mount every image sitting in a real `CPM/images` folder and report what
    /// happened — the end-to-end check that the naming convention, the
    /// read-only rule and the refusals all behave on real files.
    ///
    /// Ignored: needs a populated images folder.  Set `CPM_LIVE_BASE` to a
    /// `CPM/` container.
    #[test]
    #[ignore]
    fn test_mount_every_image_in_a_real_folder() {
        let Ok(base) = std::env::var("CPM_LIVE_BASE") else {
            eprintln!("set CPM_LIVE_BASE to run this test");
            return;
        };
        let base = std::path::PathBuf::from(base);
        let _g = registry::tests_lock();
        registry::tests_reset();

        let images = available_images(&base);
        assert!(!images.is_empty(), "no images in {}", images_dir(&base).display());
        for (i, name) in images.iter().enumerate() {
            let drive = (i % 16) as u8;
            match mount_image(&base, drive, name) {
                Ok(note) => {
                    println!("  MOUNTED  {note}");
                    if let Some(m) = registry::get(drive) {
                        let guard = m.fs.lock().unwrap_or_else(|e| e.into_inner());
                        let mut names: Vec<String> = guard
                            .entries()
                            .iter()
                            .map(|e| {
                                crate::cpm::fcb::format_8_3(&e.name, &e.ext)
                            })
                            .collect();
                        names.sort();
                        names.dedup();
                        println!("  FILES    {}|{}", name, names.join(" "));
                    }
                }
                Err(e) => println!("  refused  {e}"),
            }
            let _ = unmount_drive(drive);
        }
        registry::tests_reset();
    }

    /// **Every file big enough to need more than one extent, across a folder of
    /// images.**
    ///
    /// The population at risk from an extent bug, and the survey behind the one
    /// that was found: CP/M's first extent is 16 KB, so a file larger than that
    /// can only be read whole by software that positions itself — and until
    /// `Open` learned to honour the extent it was given, every one of those
    /// reads came back from the wrong place without erroring.
    ///
    /// Ignored — set `CPM_LARGE_DIR` to a folder of `.dsk` files.
    #[test]
    #[ignore]
    fn test_survey_files_larger_than_one_extent() {
        let Ok(dir) = std::env::var("CPM_LARGE_DIR") else {
            eprintln!("set CPM_LARGE_DIR to run this");
            return;
        };
        let _g = registry::tests_lock();
        registry::tests_reset();
        let base = std::env::temp_dir().join(format!("egw_large_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(images_dir(&base)).unwrap();

        let mut names: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("dsk")))
            .collect();
        names.sort();

        let mut total = 0usize;
        for src in &names {
            let file = src.file_name().unwrap().to_string_lossy().to_string();
            let dst = images_dir(&base).join(&file);
            if std::fs::copy(src, &dst).is_err() {
                continue;
            }
            if mount_image(&base, 0, &file).is_ok() {
                if let Some(m) = registry::get(0) {
                    let guard = m.fs.lock().unwrap_or_else(|e| e.into_inner());
                    // A file's size is the highest record it reaches, and the
                    // directory holds one entry per extent, so the last extent
                    // is what says how big it is.
                    let mut sizes: std::collections::BTreeMap<String, u32> =
                        std::collections::BTreeMap::new();
                    for e in guard.entries() {
                        let n = crate::cpm::fcb::format_8_3(&e.name, &e.ext);
                        let recs = e.extent * 128 + e.rc as u32;
                        let slot = sizes.entry(n).or_insert(0);
                        *slot = (*slot).max(recs);
                    }
                    let mut big: Vec<(String, u32)> =
                        sizes.into_iter().filter(|(_, r)| *r > 128).collect();
                    big.sort_by_key(|(_, r)| std::cmp::Reverse(*r));
                    if !big.is_empty() {
                        println!("  {file}:");
                        for (n, r) in &big {
                            let extents = r.div_ceil(128);
                            println!("      {n:14} {:7} bytes  {extents} extents", r * 128);
                            total += 1;
                        }
                    }
                }
            }
            let _ = unmount_drive(0);
            let _ = std::fs::remove_file(&dst);
        }
        println!("\n  {total} files need more than one extent");
        let _ = std::fs::remove_dir_all(&base);
        registry::tests_reset();
    }

}
