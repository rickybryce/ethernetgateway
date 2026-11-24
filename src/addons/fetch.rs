//! Downloading the Ethernet Gateway add-ons from their own repository.
//!
//! **Pinned, like the CP/M sample disks** (`crate::cpm::fetch`): this build
//! carries the commit and every file's size and SHA-256
//! (`src/addons_catalogue.txt`), and a file that arrives different is
//! refused.  These are programs the gateway will run, so a change in the
//! repository reaches a gateway only through a gateway release -- never
//! because somebody pushed to it.  Decided with Ricky 2026-11-22.
//!
//! **The unit is the add-on, not the file.**  An add-on whose folder is
//! already in `AddOns/` is left exactly as it is (it may be the operator's
//! own, or edited); a new one is assembled in a hidden staging folder and
//! only renamed into place once every file has arrived and checked out, so
//! an interrupted download never leaves half an add-on in the list.

use std::path::Path;

/// The repository the add-ons come from.
pub(crate) const REPO: &str = "rickybryce/ethernetgateway-addons";
/// The commit this build's catalogue was generated from.  A commit, never a
/// branch: a branch would let the repository change what "checked" means.
pub(crate) const COMMIT: &str = "e353abb2c9fd49422a753751c9da4beb6051d4ad";

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

    fn url(&self) -> String {
        format!("https://raw.githubusercontent.com/{REPO}/{COMMIT}/{}", self.path)
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

/// A file from its four catalogue fields, or `None` when any is off.  The
/// pinned catalogue and the updater's signed list read files the same way.
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

/// Every file on offer.  A malformed line is dropped rather than guessed at;
/// `test_the_catalogue_parses_completely` is what stops that happening
/// silently.
pub(crate) fn catalogue() -> Vec<File> {
    parse_catalogue(include_str!("../addons_catalogue.txt"))
}

fn parse_catalogue(text: &str) -> Vec<File> {
    text.lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .filter_map(|line| {
            let mut f = line.split('\t');
            parse_file_fields(f.next()?, f.next()?, f.next()?, f.next()?)
        })
        .collect()
}

/// The add-ons the catalogue offers, in its order, once each.
pub(crate) fn folders(files: &[File]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for f in files {
        if !out.iter().any(|o| o == f.folder()) {
            out.push(f.folder().to_string());
        }
    }
    out
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

/// Fetch every add-on that is not already in `root`.
///
/// **One run at a time, process-wide.**  Two runs (the wizard finished twice
/// in quick succession) shared each add-on's staging folder: one's
/// `remove_dir_all` could empty the folder the other was filling, and a file
/// being written could be renamed into place unverified.  A second run is
/// refused and says so; the first will have fetched whatever it lacked.
pub(crate) fn download_missing(root: &Path, progress: impl FnMut(&str, usize, usize)) -> Report {
    let Some(_running) = one_at_a_time() else {
        let mut busy = Report::default();
        busy.failed.push(("add-ons".into(), "another download is already running".into()));
        return busy;
    };
    download_with(root, &catalogue(), progress, |f| {
        crate::cpm::fetch::get(&f.url(), "EthernetGateway (add-ons)", 1 << 20, f.bytes)
    })
}

/// `download_missing` with the catalogue and the network as parameters, so a
/// test can hand it bytes.
fn download_with(
    root: &Path,
    files: &[File],
    mut progress: impl FnMut(&str, usize, usize),
    get: impl Fn(&File) -> Result<Vec<u8>, String>,
) -> Report {
    let mut report = Report::default();
    if let Err(e) = std::fs::create_dir_all(root) {
        report.failed.push(("add-ons folder".into(), e.to_string()));
        return report;
    }
    let all = folders(files);
    for (i, folder) in all.iter().enumerate() {
        let target = root.join(folder);
        if target.exists() {
            report.skipped.push(folder.clone());
            continue;
        }
        progress(folder, i + 1, all.len());
        // Hidden, so discovery passes it over while it is being filled.
        let staging = root.join(format!(".{folder}.part"));
        let _ = std::fs::remove_dir_all(&staging);
        let filled = std::fs::create_dir_all(&staging).map_err(|e| e.to_string()).and_then(|()| {
            for f in files.iter().filter(|f| f.folder() == folder) {
                write_checked(&staging, f, get(f)?)?;
            }
            mark(&staging)
        });
        match filled {
            // Claimed only now, whole.  A folder that appeared meanwhile is
            // the operator's and wins.
            Ok(()) if !target.exists() => match std::fs::rename(&staging, &target) {
                Ok(()) => report.fetched.push(folder.clone()),
                Err(e) => {
                    let _ = std::fs::remove_dir_all(&staging);
                    report.failed.push((folder.clone(), e.to_string()));
                }
            },
            Ok(()) => {
                let _ = std::fs::remove_dir_all(&staging);
                report.skipped.push(folder.clone());
            }
            Err(e) => {
                let _ = std::fs::remove_dir_all(&staging);
                report.failed.push((folder.clone(), e));
            }
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("egw-addon-fetch-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn file(path: &str, body: &[u8], executable: bool) -> File {
        File {
            path: path.into(),
            bytes: body.len() as u64,
            sha256: crate::cpm::fetch::sha256(body),
            executable,
        }
    }

    /// Every line of the shipped catalogue is read, every add-on in it has
    /// its manifest, and only scripts are made executable.
    #[test]
    fn test_the_catalogue_parses_completely() {
        let text = include_str!("../addons_catalogue.txt");
        let lines = text.lines().filter(|l| !l.starts_with('#') && !l.trim().is_empty()).count();
        let all = catalogue();
        assert_eq!(all.len(), lines, "a catalogue line was dropped");
        assert!(text.contains(COMMIT), "the catalogue was generated from another commit");
        for folder in folders(&all) {
            assert!(
                all.iter().any(|f| f.path == format!("{folder}/{}", crate::addons::MANIFEST)),
                "{folder} has no manifest in the catalogue"
            );
        }
        for f in &all {
            assert_eq!(f.executable, f.path.ends_with(".sh"), "{}", f.path);
        }
    }

    /// A path that could leave the add-ons folder is never accepted.
    #[test]
    fn test_a_catalogue_path_must_be_folder_and_name() {
        let ok = "a\t1\t".to_string() + &"0".repeat(64) + "\t0";
        for bad in ["../x", "a/../b", "/abs/x", "a/b/c", ".hidden/x", "a/.x", "a\\b/x", "x"] {
            let line = format!("{bad}\t1\t{}\t0", "0".repeat(64));
            assert!(parse_catalogue(&line).is_empty(), "{bad} accepted");
        }
        assert!(parse_catalogue(&ok.replacen('a', "a/b", 1)).len() == 1);
    }

    /// Good bytes arrive as a whole folder, scripts executable; an add-on
    /// already there is left alone; a bad file leaves nothing behind.
    #[test]
    fn test_an_add_on_arrives_whole_or_not_at_all() {
        let root = tmp("whole");
        let files = vec![
            file("good/addon.conf", b"command = run.sh\n", false),
            file("good/run.sh", b"#!/bin/sh\n", true),
            file("bad/addon.conf", b"command = run.sh\n", false),
            file("bad/run.sh", b"#!/bin/sh\n", true),
            file("mine/addon.conf", b"theirs\n", false),
        ];
        std::fs::create_dir_all(root.join("mine")).unwrap();
        std::fs::write(root.join("mine/addon.conf"), "my own\n").unwrap();
        let report = download_with(&root, &files, |_, _, _| {}, |f| {
            if f.path == "bad/run.sh" {
                Ok(b"tampered!!".to_vec())
            } else {
                Ok(match f.path.as_str() {
                    "good/run.sh" | "bad/run.sh" => b"#!/bin/sh\n".to_vec(),
                    "mine/addon.conf" => b"theirs\n".to_vec(),
                    _ => b"command = run.sh\n".to_vec(),
                })
            }
        });
        assert_eq!(report.fetched, vec!["good"]);
        assert_eq!(report.skipped, vec!["mine"]);
        assert_eq!(report.failed.len(), 1, "{report:?}");
        assert!(!root.join("bad").exists(), "half an add-on was left in the list");
        assert!(!root.join(".bad.part").exists(), "the staging folder was left behind");
        assert_eq!(std::fs::read_to_string(root.join("mine/addon.conf")).unwrap(), "my own\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(root.join("good/run.sh")).unwrap().permissions().mode();
            assert!(mode & 0o111 != 0, "the program was not made executable");
        }
        assert!(crate::addons::load(&root.join("good")).is_ok(), "the downloaded add-on does not load");
        assert!(root.join("good").join(MARKER).exists(), "a downloaded add-on is not marked as ours");
        assert!(!root.join("mine").join(MARKER).exists(), "the operator's own folder was marked");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The pinned files are still what the repository serves at the pinned
    /// commit.  Live: needs the network, so run by hand before a release.
    #[test]
    #[ignore]
    fn test_live_the_pinned_add_ons_download_and_verify() {
        let root = tmp("live");
        let report = download_missing(&root, |_, _, _| {});
        assert!(report.failed.is_empty(), "{}", report.summary());
        assert_eq!(report.fetched, folders(&catalogue()));
        for f in folders(&catalogue()) {
            assert!(crate::addons::load(&root.join(&f)).is_ok(), "{f} does not load");
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}
