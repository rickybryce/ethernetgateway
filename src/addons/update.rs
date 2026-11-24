//! Checking for new and updated add-ons, and installing them.
//!
//! **A signed list, not a pinned one.**  The setup wizard's download
//! ([`super::fetch`]) carries its catalogue inside the build, so a gateway
//! only ever gets what was released with it.  An update has to learn about
//! add-ons newer than the build, so the list comes from the add-ons
//! repository -- and since these are programs the gateway will run, it is
//! signed: `catalogue.txt` with an ed25519 signature in `catalogue.sig`,
//! made with a private key kept off every repository, checked here against
//! [`PUBLIC_KEY_HEX`].  A list that does not verify is refused whole; there
//! is no unsigned fallback.  Every file it names is then checked against
//! its size and SHA-256 before anything is installed.  Decided with Ricky
//! 2026-11-23.
//!
//! **The list (tab separated, `#` comments):**
//!
//! ```text
//! egw-addons 1
//! addon  <id>  <version>  <min_gateway|->  <max_gateway|->
//! file   <id>/<name>  <bytes>  <sha256>  <1 = executable|0>
//! ```
//!
//! **Only what fits this gateway is offered** ([`super::range_fits`]), and
//! an update replaces the whole folder in one rename -- except files the
//! list does not name, which are the add-on's own (a high-score table) and
//! are carried over.

use super::fetch::File;
use std::path::Path;

/// The branch the list is read from.  Its signature, not the branch, is what
/// makes it trusted.
pub(crate) const BRANCH: &str = "main";

/// The ed25519 public key the list's signature is checked with.  The
/// private half is Ricky's (`~/.config/ethernetgateway/addons-signing-key.pem`
/// on his desktop); a new key means a gateway release carrying the new
/// public half.
const PUBLIC_KEY_HEX: &str = "f4616144f7f535091f63536fe8f3b257bf194504dfdcebcfc646e4d262aab67e";

/// The largest list or signature accepted: far beyond any real one.
const LIST_LIMIT: u64 = 256 * 1024;

/// One add-on in the list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    pub id: String,
    pub version: String,
    pub min_gateway: Option<Vec<u32>>,
    pub max_gateway: Option<Vec<u32>>,
    pub files: Vec<File>,
}

/// Why an entry is offered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum State {
    /// Not installed.
    New,
    /// Installed, at the version given (empty when it names none).
    Update { installed: String },
}

/// An entry this gateway can install, and why it is offered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Offer {
    pub entry: Entry,
    pub state: State,
}

impl Offer {
    /// "guess 1.2 (new)" / "hello 1.1 (you have 1.0)": one line for any screen.
    pub(crate) fn label(&self) -> String {
        let v = if self.entry.version.is_empty() { String::new() } else { format!(" {}", self.entry.version) };
        match &self.state {
            State::New => format!("{}{v} (new)", self.entry.id),
            State::Update { installed } if installed.is_empty() => format!("{}{v} (update)", self.entry.id),
            State::Update { installed } => format!("{}{v} (you have {installed})", self.entry.id),
        }
    }
}

fn hex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

/// Whether `sig_hex` is `key`'s ed25519 signature of exactly `text`.
pub(crate) fn verify(text: &[u8], sig_hex: &str, key: &[u8]) -> Result<(), String> {
    let sig = hex(sig_hex).filter(|s| s.len() == 64).ok_or("the signature is not one")?;
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, key)
        .verify(text, &sig)
        .map_err(|_| "the list's signature does not match: refused".to_string())
}

/// Read a signed list.  **Strict**: it is signed, so a line that does not
/// parse is a mistake in making it, and guessing past it could install half
/// an add-on.  Every file must belong to an `addon` line, and every add-on
/// must have an `addon.conf`.
pub(crate) fn parse(text: &str) -> Result<Vec<Entry>, String> {
    let mut lines = text.lines().map(str::trim_end).filter(|l| !l.trim().is_empty() && !l.starts_with('#'));
    if lines.next() != Some("egw-addons 1") {
        return Err("the list is not one this gateway reads (no \"egw-addons 1\" line)".into());
    }
    let range = |v: &str, key: &str| -> Result<Option<Vec<u32>>, String> {
        if v == "-" { Ok(None) } else { super::parse_version(key, v).map(Some) }
    };
    let mut out: Vec<Entry> = Vec::new();
    for line in lines {
        let f: Vec<&str> = line.split('\t').collect();
        match f.as_slice() {
            ["addon", id, version, min, max] => {
                if !super::fetch::plain_name(id) || out.iter().any(|e| e.id == *id) {
                    return Err(format!("bad or repeated add-on \"{id}\" in the list"));
                }
                out.push(Entry {
                    id: id.to_string(),
                    version: if *version == "-" { String::new() } else { version.to_string() },
                    min_gateway: range(min, "min_gateway")?,
                    max_gateway: range(max, "max_gateway")?,
                    files: Vec::new(),
                });
            }
            ["file", path, bytes, sha, exec] => {
                let file = super::fetch::parse_file_fields(path, bytes, sha, exec)
                    .ok_or_else(|| format!("bad file line for \"{path}\" in the list"))?;
                let entry = out
                    .iter_mut()
                    .find(|e| e.id == file.folder())
                    .ok_or_else(|| format!("{path} is listed before its add-on"))?;
                if entry.files.iter().any(|g| g.path == file.path) {
                    return Err(format!("{path} is listed twice"));
                }
                entry.files.push(file);
            }
            _ => return Err(format!("\"{line}\" is not a line this gateway reads")),
        }
    }
    for e in &out {
        let conf = format!("{}/{}", e.id, super::MANIFEST);
        if !e.files.iter().any(|f| f.path == conf) {
            return Err(format!("{} has no {} in the list", e.id, super::MANIFEST));
        }
    }
    Ok(out)
}

/// Whether version `a` is newer than `b`: compared part by part on the dots,
/// numerically where both parts are numbers (so 1.10 is after 1.9).  An
/// installed add-on with no version is older than any.
pub(crate) fn newer(a: &str, b: &str) -> bool {
    if b.is_empty() {
        return !a.is_empty();
    }
    let (pa, pb): (Vec<&str>, Vec<&str>) = (a.split('.').collect(), b.split('.').collect());
    for i in 0..pa.len().max(pb.len()) {
        let (x, y) = (pa.get(i).copied().unwrap_or("0"), pb.get(i).copied().unwrap_or("0"));
        let ord = match (x.parse::<u64>(), y.parse::<u64>()) {
            (Ok(x), Ok(y)) => x.cmp(&y),
            _ => x.cmp(y),
        };
        if ord != std::cmp::Ordering::Equal {
            return ord == std::cmp::Ordering::Greater;
        }
    }
    false
}

/// The version an installed folder's `addon.conf` gives, read leniently: a
/// folder this gateway cannot load (an older add-on outside its range) can
/// still be updated.
fn installed_version(dir: &Path) -> String {
    let Ok(text) = std::fs::read_to_string(dir.join(super::MANIFEST)) else { return String::new() };
    text.lines()
        .filter_map(|l| l.split_once('='))
        .filter(|(k, _)| k.trim().eq_ignore_ascii_case("version"))
        .map(|(_, v)| v.trim().to_string())
        .next_back()
        .unwrap_or_default()
}

/// What the list offers a gateway at version `ours` with add-ons in `root`:
/// entries in its range that are not installed, or are newer than the copy
/// that is.  In the list's order.
pub(crate) fn offers(entries: &[Entry], root: &Path, ours: &[u32]) -> Vec<Offer> {
    entries
        .iter()
        .filter(|e| super::range_fits(e.min_gateway.as_deref(), e.max_gateway.as_deref(), ours).is_ok())
        .filter_map(|e| {
            let dir = root.join(&e.id);
            let state = if !dir.exists() {
                State::New
            } else if !dir.join(super::fetch::MARKER).exists() {
                // A folder of that name the gateway did not download: the
                // operator's own, never replaced (see `fetch::MARKER`).
                return None;
            } else {
                let installed = installed_version(&dir);
                if !newer(&e.version, &installed) {
                    return None;
                }
                State::Update { installed }
            };
            Some(Offer { entry: e.clone(), state })
        })
        .collect()
}

fn url(path: &str) -> String {
    format!("https://raw.githubusercontent.com/{}/{BRANCH}/{path}", super::fetch::REPO)
}

const AGENT: &str = "EthernetGateway (add-on update)";

/// Fetch and verify the list: the network step behind every Check.
pub(crate) fn fetch_list() -> Result<Vec<Entry>, String> {
    let key = hex(PUBLIC_KEY_HEX).ok_or("the built-in key is malformed")?;
    let text = crate::cpm::fetch::get(&url("catalogue.txt"), AGENT, LIST_LIMIT, 4096)?;
    let sig = crate::cpm::fetch::get(&url("catalogue.sig"), AGENT, 1024, 256)?;
    verify(&text, &String::from_utf8_lossy(&sig), &key)?;
    parse(std::str::from_utf8(&text).map_err(|_| "the list is not text")?)
}

/// Check: what this gateway could install now.
pub(crate) fn check(root: &Path) -> Result<Vec<Offer>, String> {
    Ok(offers(&fetch_list()?, root, &super::this_gateway()))
}

/// Install `offers`, one add-on at a time, and say what happened.  Shares
/// the wizard download's one-at-a-time rule: both write in `AddOns/`.
pub(crate) fn install(root: &Path, offers: &[Offer]) -> super::fetch::Report {
    let Some(_running) = super::fetch::one_at_a_time() else {
        let mut busy = super::fetch::Report::default();
        busy.failed.push(("add-ons".into(), "another download is already running".into()));
        return busy;
    };
    install_with(root, offers, |f| crate::cpm::fetch::get(&url(&f.path), AGENT, 1 << 20, f.bytes))
}

/// [`install`] with the network as a parameter, for the tests.
pub(crate) fn install_with(
    root: &Path,
    offers: &[Offer],
    get: impl Fn(&File) -> Result<Vec<u8>, String>,
) -> super::fetch::Report {
    let mut report = super::fetch::Report::default();
    for o in offers {
        match install_one(root, &o.entry, &get) {
            Ok(()) => {
                crate::logger::log(format!("Add-on \"{}\" installed: {}", o.entry.id, o.label()));
                report.fetched.push(o.label());
            }
            Err(e) => {
                crate::logger::log(format!("Add-on \"{}\" not installed: {e}", o.entry.id));
                report.failed.push((o.entry.id.clone(), e));
            }
        }
    }
    report
}

/// Copy `from` into `to`, folders and all.
fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    if from.is_dir() {
        std::fs::create_dir_all(to)?;
        for e in std::fs::read_dir(from)? {
            let e = e?;
            copy_tree(&e.path(), &to.join(e.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(from, to).map(|_| ())
    }
}

/// One add-on: assembled and checked in a hidden folder, the old copy's own
/// files carried over, then swapped in.  Nothing visible changes unless
/// every step before the swap succeeded, and a failed swap puts the old
/// folder back.
fn install_one(root: &Path, e: &Entry, get: &impl Fn(&File) -> Result<Vec<u8>, String>) -> Result<(), String> {
    std::fs::create_dir_all(root).map_err(|err| err.to_string())?;
    let target = root.join(&e.id);
    // Checked again here, at the step that relies on them.
    if target.exists() && !target.join(super::fetch::MARKER).exists() {
        return Err("a folder of that name is the operator's own; left alone".into());
    }
    // A running copy works in its folder: on Linux the swap would succeed and
    // what it saved afterwards would go with the old folder.
    if super::in_use(&e.id) || super::gui::is_open(&e.id) {
        return Err("someone is using it; try again when nobody is".into());
    }
    // Hidden, so discovery passes over both while this runs.
    let staging = root.join(format!(".{}.new", e.id));
    let old = root.join(format!(".{}.old", e.id));
    let _ = std::fs::remove_dir_all(&staging);
    let _ = std::fs::remove_dir_all(&old);
    let built = (|| -> Result<(), String> {
        std::fs::create_dir_all(&staging).map_err(|err| err.to_string())?;
        for f in &e.files {
            super::fetch::write_checked(&staging, f, get(f)?)?;
        }
        // The add-on's own files: whatever the old folder has that the list
        // does not name.
        if target.is_dir() {
            for item in std::fs::read_dir(&target).map_err(|err| err.to_string())?.flatten() {
                let name = item.file_name().to_string_lossy().into_owned();
                let listed = e.files.iter().any(|f| f.path.rsplit('/').next() == Some(name.as_str()));
                if !listed {
                    copy_tree(&item.path(), &staging.join(&name)).map_err(|err| format!("keeping {name}: {err}"))?;
                }
            }
        }
        super::fetch::mark(&staging)
    })();
    if let Err(err) = built {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(err);
    }
    let had_old = target.exists();
    if had_old {
        if let Err(err) = std::fs::rename(&target, &old) {
            let _ = std::fs::remove_dir_all(&staging);
            // Windows: a running add-on holds its folder.
            return Err(format!("its folder is in use ({err}); try again when nobody is running it"));
        }
    }
    if let Err(err) = std::fs::rename(&staging, &target) {
        if had_old {
            let _ = std::fs::rename(&old, &target);
        }
        let _ = std::fs::remove_dir_all(&staging);
        return Err(err.to_string());
    }
    let _ = std::fs::remove_dir_all(&old);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::KeyPair;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("egw-update-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn to_hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    /// A list line for a file holding `body`.
    fn file_line(path: &str, body: &[u8], exec: bool) -> String {
        format!("file\t{path}\t{}\t{}\t{}\n", body.len(), crate::cpm::fetch::sha256(body), u8::from(exec))
    }

    #[test]
    fn test_the_built_in_key_is_a_key() {
        assert_eq!(hex(PUBLIC_KEY_HEX).map(|k| k.len()), Some(32));
    }

    /// **A list signed by the add-ons repository's script verifies here**:
    /// `tools/make-catalogue.sh` (openssl, hex) and this module (ring) agree
    /// on the signature, and on the built-in key.  The pair in `testdata` is
    /// a real one; regenerate it with the script if the key changes.
    #[test]
    fn test_a_list_from_the_signing_script_verifies() {
        let text = include_bytes!("testdata/catalogue.txt");
        let sig = include_str!("testdata/catalogue.sig");
        let key = hex(PUBLIC_KEY_HEX).unwrap();
        verify(text, sig, &key).expect("the script's signature");
        let entries = parse(std::str::from_utf8(text).unwrap()).unwrap();
        assert!(entries.iter().any(|e| e.id == "hello"), "{entries:?}");
        // Windows checks out text with CRLF: the bytes signed must be the
        // bytes read, so the test data is binary (see .gitattributes).
        assert!(!text.contains(&b'\r'));
    }

    /// **Only the key's own signature of exactly these bytes passes**: one
    /// changed byte, another key, or a mangled signature are all refused.
    #[test]
    fn test_only_a_true_signature_passes() {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let pair = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let text = b"egw-addons 1\naddon\thello\t1.0\t-\t-\n";
        let sig = to_hex(pair.sign(text).as_ref());
        let key = pair.public_key().as_ref();
        assert!(verify(text, &sig, key).is_ok());
        assert!(verify(b"other bytes", &sig, key).is_err(), "other bytes");
        let mut changed = text.to_vec();
        changed[20] ^= 1;
        assert!(verify(&changed, &sig, key).is_err(), "a changed byte");
        let other = ring::signature::Ed25519KeyPair::from_pkcs8(
            ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap().as_ref(),
        )
        .unwrap();
        assert!(verify(text, &sig, other.public_key().as_ref()).is_err(), "another key");
        assert!(verify(text, &sig[2..], key).is_err(), "a short signature");
        assert!(verify(text, "zz", key).is_err());
        assert!(verify(text, &format!("  {sig}\n"), key).is_ok(), "a trailing newline is not a difference");
    }

    #[test]
    fn test_the_list_parses_and_refuses_what_is_off() {
        let good = format!(
            "# made by make-catalogue\negw-addons 1\naddon\thello\t1.1\t1.1\t-\n{}{}addon\tguess\t-\t-\t1.4\n{}",
            file_line("hello/addon.conf", b"command = h.sh\n", false),
            file_line("hello/h.sh", b"#!/bin/sh\n", true),
            file_line("guess/addon.conf", b"command = g.sh\n", false),
        );
        let e = parse(&good).unwrap();
        assert_eq!(e.len(), 2);
        assert_eq!((e[0].id.as_str(), e[0].version.as_str(), e[0].files.len()), ("hello", "1.1", 2));
        assert_eq!((e[0].min_gateway.clone(), e[0].max_gateway.clone()), (Some(vec![1, 1]), None));
        assert_eq!((e[1].version.as_str(), e[1].max_gateway.clone()), ("", Some(vec![1, 4])));
        assert!(e[0].files[1].executable);
        let conf = file_line("x/addon.conf", b"c", false);
        for (bad, why) in [
            (good.replacen("egw-addons 1", "egw-addons 2", 1), "egw-addons 1"),
            (format!("egw-addons 1\n{conf}"), "before its add-on"),
            ("egw-addons 1\naddon\tx\t1\t-\t-\n".to_string(), "no addon.conf"),
            (format!("egw-addons 1\naddon\tx\t1\t-\t-\naddon\tx\t1\t-\t-\n{conf}"), "repeated"),
            ("egw-addons 1\naddon\t..\t1\t-\t-\n".to_string(), "bad"),
            (format!("egw-addons 1\naddon\tx\t1\tsoon\t-\n{conf}"), "min_gateway"),
            (format!("egw-addons 1\naddon\tx\t1\t-\t-\n{conf}{conf}"), "twice"),
            (format!("egw-addons 1\naddon\tx\t1\t-\t-\nfile\tx/../y\t1\t{}\t0\n", "a".repeat(64)), "bad file"),
            ("egw-addons 1\naddon\tx\t1\t-\t-\nmystery line\n".to_string(), "not a line"),
        ] {
            let got = parse(&bad).unwrap_err();
            assert!(got.contains(why), "{why}: {got}");
        }
    }

    #[test]
    fn test_versions_compare_by_number() {
        assert!(newer("1.10", "1.9"));
        assert!(newer("2", "1.9.9"));
        assert!(newer("1.0.1", "1.0"));
        assert!(!newer("1.0", "1.0.0"), "the same version is not newer");
        assert!(!newer("1.9", "1.10"));
        assert!(newer("1.0", ""), "no installed version: anything is newer");
        assert!(!newer("", ""));
        assert!(newer("1.0b", "1.0a"), "words compare as text");
    }

    /// **Offered: new, or newer than installed, and in this gateway's range.**
    #[test]
    fn test_offers_are_new_or_newer_and_fit_this_gateway() {
        let root = tmp("offers");
        std::fs::create_dir_all(root.join("hello")).unwrap();
        std::fs::write(root.join("hello/addon.conf"), "version = 1.0\ncommand = h.sh\n").unwrap();
        super::super::fetch::mark(&root.join("hello")).unwrap();
        std::fs::create_dir_all(root.join("same")).unwrap();
        std::fs::write(root.join("same/addon.conf"), "version = 2.0\n").unwrap();
        super::super::fetch::mark(&root.join("same")).unwrap();
        // The operator's own folder of a listed name, older and unmarked.
        std::fs::create_dir_all(root.join("mine")).unwrap();
        std::fs::write(root.join("mine/addon.conf"), "command = m.sh\n").unwrap();
        let entry = |id: &str, v: &str, min: Option<Vec<u32>>, max: Option<Vec<u32>>| Entry {
            id: id.into(),
            version: v.into(),
            min_gateway: min,
            max_gateway: max,
            files: Vec::new(),
        };
        let list = vec![
            entry("hello", "1.1", None, None),
            entry("same", "2.0", None, None),
            entry("guess", "1.0", None, None),
            entry("future", "1.0", Some(vec![9]), None),
            entry("past", "1.0", None, Some(vec![1, 0])),
            entry("mine", "9.0", None, None),
        ];
        let got = offers(&list, &root, &[1, 1, 0]);
        let labels: Vec<String> = got.iter().map(Offer::label).collect();
        assert_eq!(labels, ["hello 1.1 (you have 1.0)", "guess 1.0 (new)"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **An update replaces the listed files, keeps the add-on's own, and a
    /// bad file changes nothing at all.**
    #[test]
    fn test_an_update_is_whole_or_not_at_all() {
        let root = tmp("install");
        let dir = root.join("hello");
        std::fs::create_dir_all(dir.join("data")).unwrap();
        std::fs::write(dir.join("addon.conf"), "version = 1.0\n").unwrap();
        std::fs::write(dir.join("old-only.txt"), "mine").unwrap();
        std::fs::write(dir.join("data/scores.txt"), "999").unwrap();
        super::super::fetch::mark(&dir).unwrap();
        let conf = b"version = 1.1\ncommand = h.sh\n".to_vec();
        let prog = b"#!/bin/sh\necho new\n".to_vec();
        let list = format!(
            "egw-addons 1\naddon\thello\t1.1\t-\t-\n{}{}",
            file_line("hello/addon.conf", &conf, false),
            file_line("hello/h.sh", &prog, true)
        );
        let entries = parse(&list).unwrap();
        let o = offers(&entries, &root, &[1, 1, 0]);
        assert_eq!(o.len(), 1);
        let (c2, p2) = (conf.clone(), prog.clone());
        let serve = move |f: &File| Ok(if f.path.ends_with("addon.conf") { c2.clone() } else { p2.clone() });

        // A file that arrives wrong -- the same length, so only the SHA-256
        // can tell: nothing changes.
        let mut tampered = prog.clone();
        tampered[12] ^= 0x20;
        let bad = install_with(&root, &o, |f: &File| if f.path.ends_with("h.sh") { Ok(tampered.clone()) } else { serve(f) });
        assert_eq!(bad.failed.len(), 1, "{bad:?}");
        assert_eq!(std::fs::read_to_string(dir.join("addon.conf")).unwrap(), "version = 1.0\n");
        assert!(!root.join(".hello.new").exists() && !root.join(".hello.old").exists());

        // In use: refused, untouched.
        {
            let _running = super::super::InUse::enter("hello");
            let busy = install_with(&root, &o, &serve);
            assert!(busy.failed.first().is_some_and(|(_, why)| why.contains("using it")), "{busy:?}");
            assert_eq!(std::fs::read_to_string(dir.join("addon.conf")).unwrap(), "version = 1.0\n");
        }
        // Not ours: refused even when asked directly.
        let other = root.join("other");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("addon.conf"), "mine").unwrap();
        let mut theirs = o[0].clone();
        theirs.entry.id = "other".into();
        theirs.entry.files.iter_mut().for_each(|f| f.path = f.path.replacen("hello/", "other/", 1));
        let refused = install_with(&root, &[theirs], &serve);
        assert!(refused.failed.first().is_some_and(|(_, why)| why.contains("operator's own")), "{refused:?}");
        assert_eq!(std::fs::read_to_string(other.join("addon.conf")).unwrap(), "mine");

        let good = install_with(&root, &o, &serve);
        assert_eq!(good.fetched, ["hello 1.1 (you have 1.0)"], "{good:?}");
        assert_eq!(std::fs::read(dir.join("addon.conf")).unwrap(), conf);
        assert_eq!(std::fs::read(dir.join("h.sh")).unwrap(), prog);
        assert_eq!(std::fs::read_to_string(dir.join("data/scores.txt")).unwrap(), "999", "its own data kept");
        assert_eq!(std::fs::read_to_string(dir.join("old-only.txt")).unwrap(), "mine");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_ne!(std::fs::metadata(dir.join("h.sh")).unwrap().permissions().mode() & 0o111, 0);
        }
        assert!(!root.join(".hello.new").exists() && !root.join(".hello.old").exists());
        assert!(dir.join(super::super::fetch::MARKER).exists(), "still marked as ours");
        assert!(offers(&entries, &root, &[1, 1, 0]).is_empty(), "up to date now");
        let _ = std::fs::remove_dir_all(&root);
    }
}
