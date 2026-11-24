//! What every add-on download shares: the repository, a file as the signed
//! list describes it, writing one only once its SHA-256 matches, the marker
//! that says a folder is ours to update, and one download at a time.
//!
//! **There is one list, and it is signed** (`super::update`).  The setup
//! wizard used to install a list pinned into the build (a commit and each
//! file's SHA-256), which meant an add-on published after a release never
//! reached a new gateway's wizard.  Ricky, 2026-11-24: the wizard, like
//! every Check, reads what the repository offers now.  What keeps a push
//! from changing what a gateway runs is the signature over that list, made
//! with a key that lives only on Ricky's machine, not a pin.

use std::path::Path;

/// The repository the add-ons come from.
pub(crate) const REPO: &str = "rickybryce/ethernetgateway-addons";

/// One file of one add-on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct File {
    /// `folder/name`, as in the repository.
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
    /// Made executable after download: a raw download carries no mode, and
    /// an add-on's program must be runnable (`resolve_program` checks).
    pub executable: bool,
}

impl File {
    /// The add-on folder the file belongs to.
    pub(crate) fn folder(&self) -> &str {
        self.path.split('/').next().unwrap_or("")
    }
}

/// Whether `p` is one plain, visible file or folder name.
pub(crate) fn plain_name(p: &str) -> bool {
    !p.is_empty() && p != "." && p != ".." && !p.starts_with('.') && !p.contains('\\') && !p.contains('/')
}

/// Whether a catalogue path is exactly `folder/name`, both plain names.
fn plain_path(path: &str) -> bool {
    let parts: Vec<&str> = path.split('/').collect();
    parts.len() == 2 && parts.iter().all(|p| plain_name(p))
}

/// A file from its four fields in the signed list, or `None` when any is off.
pub(crate) fn parse_file_fields(path: &str, bytes: &str, sha256: &str, exec: &str) -> Option<File> {
    let path = path.trim();
    let sha256 = sha256.trim();
    let ok = plain_path(path) && sha256.len() == 64 && sha256.bytes().all(|b| b.is_ascii_hexdigit());
    if !ok {
        return None;
    }
    Some(File {
        path: path.to_string(),
        bytes: bytes.trim().parse().ok()?,
        sha256: sha256.to_ascii_lowercase(),
        executable: match exec.trim() {
            "1" => true,
            "0" => false,
            _ => return None,
        },
    })
}

/// Check `body` against `f` and write it into `dir` under its name,
/// executable if the catalogue says so.
pub(crate) fn write_checked(dir: &Path, f: &File, body: Vec<u8>) -> Result<(), String> {
    if body.len() as u64 != f.bytes {
        return Err(format!("{}: expected {} bytes, got {}", f.path, f.bytes, body.len()));
    }
    if crate::cpm::fetch::sha256(&body) != f.sha256 {
        return Err(format!("{}: checksum mismatch", f.path));
    }
    let name = f.path.rsplit('/').next().unwrap_or_default();
    let dest = dir.join(name);
    std::fs::write(&dest, &body).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    if f.executable {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// The file the gateway leaves in every add-on folder it downloaded.
///
/// **Only a folder carrying it is ever updated.**  An operator's own add-on
/// that happens to share a name with one of ours (their own `hello`, with no
/// version) would otherwise read as an older copy and be replaced by "install
/// new and updated".  Hidden, so nothing lists it; deleting it keeps the
/// gateway's hands off that folder.
pub(crate) const MARKER: &str = ".egw-installed";

/// Leave [`MARKER`] in `dir`.
pub(crate) fn mark(dir: &Path) -> Result<(), String> {
    std::fs::write(
        dir.join(MARKER),
        format!(
            "Downloaded by Ethernet Gateway from github.com/{REPO}.\n\
             Check for add-ons may update this folder: it replaces the files the\n\
             add-on list names and keeps the rest.  Delete this file to stop that.\n"
        ),
    )
    .map_err(|e| e.to_string())
}

/// Held while something writes in `AddOns/`; dropped, the next may start.
pub(crate) struct Running;

static RUNNING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

impl Drop for Running {
    fn drop(&mut self) {
        RUNNING.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// **One download or update at a time, process-wide**, or `None` when one
/// is running.  Two runs shared each add-on's staging folder: one's
/// `remove_dir_all` could empty the folder the other was filling.
pub(crate) fn one_at_a_time() -> Option<Running> {
    (!RUNNING.swap(true, std::sync::atomic::Ordering::SeqCst)).then_some(Running)
}

/// What a download did, add-on by add-on.
#[derive(Debug, Default)]
pub(crate) struct Report {
    pub fetched: Vec<String>,
    /// Already there, so left alone.
    pub skipped: Vec<String>,
    pub failed: Vec<(String, String)>,
}

impl Report {
    /// One line for a log or a status label.
    pub(crate) fn summary(&self) -> String {
        let mut parts = Vec::new();
        if !self.fetched.is_empty() {
            parts.push(format!("downloaded {}", self.fetched.join(", ")));
        }
        if !self.skipped.is_empty() {
            parts.push(format!("already there: {}", self.skipped.join(", ")));
        }
        for (name, why) in &self.failed {
            parts.push(format!("{name} failed: {why}"));
        }
        if parts.is_empty() {
            "nothing to download".into()
        } else {
            parts.join("; ")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A path that could leave the add-ons folder is never accepted, and
    /// neither is a hash that is not one or an executable flag that is
    /// neither 0 nor 1.
    #[test]
    fn test_a_list_path_must_be_folder_and_name() {
        let sha = "0".repeat(64);
        for bad in ["../x", "a/../b", "/abs/x", "a/b/c", ".hidden/x", "a/.x", "a\\b/x", "x"] {
            assert!(parse_file_fields(bad, "1", &sha, "0").is_none(), "{bad} accepted");
        }
        assert!(parse_file_fields("a/b", "1", &sha, "0").is_some());
        assert!(parse_file_fields("a/b", "1", &sha[1..], "0").is_none(), "a short hash");
        assert!(parse_file_fields("a/b", "1", &sha, "x").is_none(), "an odd executable flag");
    }
}
