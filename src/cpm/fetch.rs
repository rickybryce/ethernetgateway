//! Fetching the sample disks, so an operator does not have to go and find them.
//!
//! The disks are **not ours** and are never shipped: they are David Hansel's
//! Altair 8800 simulator collection and Jim McNeely's Altair-Duino disks, and
//! the vintage software on them belongs to MITS, Microsoft, Digital Research and
//! Infocom. What this does is fetch them *on the operator's behalf* from the
//! original repositories, which is a different act from redistributing them and
//! is why nothing here is mirrored or bundled.
//!
//! Three rules make the offer honest rather than a lucky dip.
//!
//! **Only disks that are known to run.** Thirty-four of them, and the list is
//! not typed out: the generator cold-starts every candidate *from the bytes its
//! pinned URL really serves* and drops the ones that do not start. Four are
//! dropped — `DISK0B`, `DISK0D`, `DISK0F` and `TDISK06`, each a companion disk
//! of programs for a system disk that does boot, carrying no boot program of its
//! own. `TDISK04` is kept although it prints nothing: it boots and paints a
//! VDM-1 screen rather than writing to a console port, which is a disk that
//! works and needs the VDM / Dazzler page to be seen.
//!
//! That the list is *derived* matters more than the four names. It was an
//! exclusion list transcribed from a survey run elsewhere until 2026-08-15, and
//! it had drifted: it recorded `TDISK06` as "a blank" when the disk is McNeely's
//! `VDM-1 programs` and its directory is full. The names happened to still be
//! right; the reason had rotted.
//!
//! **Pinned, so what arrives is what was tested.** The URL names a commit, not
//! a branch. Neither upstream folder has changed since 2021, so this
//! is stable — but a pin is what makes "known to run" keep meaning something
//! rather than quietly rotting the way a stale readme does.
//!
//! **Verified, so a changed file is caught rather than accepted.** Every disk
//! carries the SHA-256 of the bytes the pinned URL really served when the
//! manifest was generated, checked again on arrival. A mismatch is refused and
//! reported; it is not written to disk.
//!
//! And nothing already in the images folder is ever overwritten. An operator
//! who has edited a disk, or put their own there under the same name, keeps it.

use std::path::Path;

/// One place disks are fetched from: a repository, a commit, and a folder in it.
///
/// **Two repositories, because neither contains the other.** This began as one,
/// and the second was added on 2026-08-15 after measuring what each holds.
/// `dhansel/Altair8800` documents `DISK13`–`DISK16` as CP/M 3.0 disk 1 and 2,
/// the Felix animation system and CP/M 2.2 MITS+Tarbell. `jpmcneely/
/// AltairDuino-Disks` has four hard disks the other does not — the Infocom
/// adventures, BASIC, COBOL and dBase II — but *also* carries four files called
/// `DISK13`–`DISK16` which are different disks entirely, are undocumented in its
/// own catalogue (that stops at `DISK12`), and one of which does not boot.
///
/// A fifth name looked unique and was not: its `DISK17` is Hansel's `DISK12`
/// byte for byte, the IMP modem executive filed under a second number, so
/// offering it would have downloaded one disk twice.
///
/// So the contested four come from Hansel and the unique four from McNeely, and
/// a disk names its source rather than the fetcher assuming one. **A filename is
/// not an identity** — the same lesson the disk survey learned when three
/// basenames collided across the z80pack libraries.
pub struct Source {
    /// How a manifest line names this source.
    pub key: &'static str,
    /// `owner/repo` on GitHub.
    pub repo: &'static str,
    /// A commit, never a branch: the manifest records bytes that were booted,
    /// and a branch name would let upstream change what "verified" refers to.
    pub commit: &'static str,
    /// The folder inside the repository holding the images.
    pub folder: &'static str,
}

/// Every repository the catalogue draws on.
pub const SOURCES: &[Source] = &[
    Source {
        key: "hansel",
        repo: "dhansel/Altair8800",
        commit: "3a42f6646c193567f1c9859c3fa1d06126088490",
        folder: "disks",
    },
    Source {
        key: "duino",
        repo: "jpmcneely/AltairDuino-Disks",
        commit: "95a2324461f39562be9f46762b7b22cf1afec445",
        folder: "original",
    },
    Source {
        key: "duino-extra",
        repo: "jpmcneely/AltairDuino-Disks",
        commit: "95a2324461f39562be9f46762b7b22cf1afec445",
        folder: "extra",
    },
];

/// The source a manifest line names, if it names one this build knows.
pub fn source_for(key: &str) -> Option<&'static Source> {
    SOURCES.iter().find(|s| s.key == key)
}

/// The repositories, once each, for the operator to see before agreeing.
///
/// Deduplicated by repository rather than by source, because two folders of one
/// repository are one place to an operator deciding whether to trust it — and
/// listing `AltairDuino-Disks` twice would read like two different projects.
pub fn source_repos() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for s in SOURCES {
        let shown = format!("github.com/{}", s.repo);
        if !out.contains(&shown) {
            out.push(shown);
        }
    }
    out
}

/// One disk in the catalogue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Disk {
    /// The filename, which is also the name it takes in the images folder.
    pub name: String,
    /// Size in bytes, as served.
    pub bytes: u64,
    /// SHA-256 of the bytes the pinned URL served when this was generated.
    pub sha256: String,
    /// Which [`Source`] serves it, by [`Source::key`].
    pub source: String,
    /// What it is, in a few words, for the operator choosing whether to bother.
    pub note: String,
}

impl Disk {
    /// Where this disk is fetched from.
    ///
    /// Infallible because [`catalogue`] drops any line naming a source this
    /// build does not have, so a `Disk` that exists has a source that resolves.
    /// `test_every_disk_names_a_source_that_exists` is what keeps that true.
    pub fn url(&self) -> String {
        let s = source_for(&self.source).expect("catalogue() rejects unknown sources");
        format!(
            "https://raw.githubusercontent.com/{}/{}/{}/{}",
            s.repo, s.commit, s.folder, self.name
        )
    }
}

/// The manifest, generated by `record_altairduino_manifest` from the disks this
/// project has actually booted.
fn manifest_text() -> &'static str {
    include_str!("altairduino.txt")
}

/// Every disk on offer.
///
/// Parsed rather than compiled in as a literal so the generated file stays
/// readable and reviewable in a diff — the same choice as `repodisks.txt`. A
/// malformed line is skipped rather than fatal: a catalogue that refuses to
/// load at all would cost the operator the whole feature over one bad row, and
/// [`test_the_manifest_parses_completely`] is what stops that happening
/// silently.
pub fn catalogue() -> Vec<Disk> {
    manifest_text()
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .filter_map(|line| {
            let mut f = line.split('\t');
            let name = f.next()?.trim().to_string();
            let bytes = f.next()?.trim().parse().ok()?;
            let sha256 = f.next()?.trim().to_string();
            let source = f.next()?.trim().to_string();
            let note = f.next().unwrap_or("").trim().to_string();
            // An unknown source is dropped rather than defaulted: guessing a
            // repository for a disk would fetch *something* under the right
            // name, which is the one failure this file's hash pinning exists to
            // make impossible.
            (!name.is_empty() && sha256.len() == 64 && source_for(&source).is_some())
                .then_some(Disk { name, bytes, sha256, source, note })
        })
        .collect()
}

/// The disks not already in the images folder.
///
/// **Never overwrites.** A file already there is the operator's — they may have
/// edited it, or put their own disk under that name — so it is left exactly as
/// it is and reported as already present rather than replaced.
pub fn missing(images: &Path, all: &[Disk]) -> Vec<Disk> {
    all.iter().filter(|d| !images.join(&d.name).exists()).cloned().collect()
}

/// How a download ended.
#[derive(Default, Debug, PartialEq, Eq)]
pub struct Report {
    /// Disks written to the images folder.
    pub fetched: Vec<String>,
    /// Disks already present, left untouched.
    pub skipped: Vec<String>,
    /// Disks that could not be had, and why.
    pub failed: Vec<(String, String)>,
}

impl Report {
    /// One line an operator can read on any of the three interfaces.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if !self.fetched.is_empty() {
            parts.push(format!("{} downloaded", self.fetched.len()));
        }
        if !self.skipped.is_empty() {
            parts.push(format!("{} already there", self.skipped.len()));
        }
        if !self.failed.is_empty() {
            parts.push(format!("{} failed", self.failed.len()));
        }
        if parts.is_empty() {
            return "Nothing to do.".to_string();
        }
        parts.join(", ")
    }

    /// The failures, with identical reasons collapsed into one line.
    ///
    /// **Because the common failure is the same failure thirty-four times.**
    /// With no internet every disk fails with the same name-resolution error,
    /// and the callers used to print the first three verbatim — the same
    /// sentence three times over, ~200 characters saying one thing, on a
    /// screen that may be 40 columns wide.  Grouping says it once and counts
    /// the rest, which is both shorter and more informative: `34 disks: <err>`
    /// tells you the network is down, where three named disks suggested three
    /// unlucky files.
    ///
    /// Order is first-seen, so a single odd failure among many identical ones
    /// keeps its own line rather than being buried.  `max` bounds the number of
    /// *groups*, not the number of disks.
    pub fn failure_lines(&self, max: usize) -> Vec<String> {
        let mut groups: Vec<(String, Vec<&str>)> = Vec::new();
        for (name, why) in &self.failed {
            match groups.iter_mut().find(|(reason, _)| reason == why) {
                Some((_, names)) => names.push(name),
                None => groups.push((why.clone(), vec![name])),
            }
        }
        groups
            .into_iter()
            .take(max)
            .map(|(reason, names)| match names.as_slice() {
                [one] => format!("{one}: {reason}"),
                many => format!("{} disks: {reason}", many.len()),
            })
            .collect()
    }
}

/// SHA-256, for verifying what arrived is what was tested.
///
/// Written out rather than pulled in: this is the only hash this crate needs,
/// a dependency for it would be a supply-chain decision taken for one function,
/// and the algorithm is fixed for ever by its own specification.
pub(in crate::cpm) fn sha256(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut msg = data.to_vec();
    let bits = (data.len() as u64) * 8;
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bits.to_be_bytes());

    for chunk in msg.chunks(64) {
        let mut w = [0u32; 64];
        for (i, word) in w.iter_mut().enumerate().take(16) {
            let b = &chunk[i * 4..i * 4 + 4];
            *word = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, v) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *slot = slot.wrapping_add(v);
        }
    }
    h.iter().map(|w| format!("{w:08x}")).collect()
}

/// Fetch one disk and verify it, returning its bytes.
///
/// Verification is not a formality: the whole claim of this feature is that
/// these exact bytes were booted, so a file that arrives different is refused
/// rather than written. A truncated download and a changed upstream both land
/// here, and both should stop.
/// GET a pinned URL and return its body.
///
/// One copy, because the sample disks and the monitor ROMs
/// ([`super::rom`]) fetch from the same place on the same terms and a second
/// agent would be a second set of timeouts to keep in step. The caller owns
/// verification: this says only what arrived.
pub(in crate::cpm) fn get(url: &str, user_agent: &str, limit: u64) -> Result<Vec<u8>, String> {
    let agent = ureq::Agent::new_with_config(
        ureq::config::Config::builder()
            .timeout_global(Some(std::time::Duration::from_secs(120)))
            .build(),
    );
    let mut resp =
        agent.get(url).header("User-Agent", user_agent).call().map_err(|e| format!("{e}"))?;
    if resp.status().as_u16() != 200 {
        return Err(format!("HTTP {}", resp.status().as_u16()));
    }
    resp.body_mut().with_config().limit(limit).read_to_vec().map_err(|e| format!("{e}"))
}

fn fetch_one(disk: &Disk) -> Result<Vec<u8>, String> {
    let body = get(&disk.url(), "EthernetGateway (CP/M sample disks)", 64 << 20)?;
    if body.len() as u64 != disk.bytes {
        return Err(format!("expected {} bytes, got {}", disk.bytes, body.len()));
    }
    let got = sha256(&body);
    if got != disk.sha256 {
        return Err(format!("checksum mismatch (got {}…)", &got[..12]));
    }
    Ok(body)
}

#[cfg(test)]
mod report_tests {
    use super::*;

    /// The offline case: every disk fails with the same message.
    ///
    /// Measured, not imagined — running the gateway in a network namespace
    /// with no route produced exactly this, 34 times over: "io: failed to
    /// lookup address information: Temporary failure in name resolution".
    #[test]
    fn test_identical_failures_are_reported_once_with_a_count() {
        let why = "io: failed to lookup address information";
        let report = Report {
            failed: (0..34).map(|i| (format!("DISK{i:02}.DSK"), why.to_string())).collect(),
            ..Default::default()
        };
        let lines = report.failure_lines(3);
        assert_eq!(lines.len(), 1, "one reason should be one line, got {lines:?}");
        assert_eq!(lines[0], format!("34 disks: {why}"));
    }

    /// A lone odd failure keeps its own name rather than being buried in the
    /// crowd — first-seen order, so it survives the `max` cut.
    #[test]
    fn test_a_single_distinct_failure_keeps_its_name() {
        let report = Report {
            failed: vec![
                ("ODD.DSK".into(), "checksum mismatch".into()),
                ("A.DSK".into(), "HTTP 404".into()),
                ("B.DSK".into(), "HTTP 404".into()),
            ],
            ..Default::default()
        };
        let lines = report.failure_lines(4);
        assert_eq!(lines, vec!["ODD.DSK: checksum mismatch", "2 disks: HTTP 404"]);
    }

    /// `max` bounds groups, and an empty report says nothing at all.
    #[test]
    fn test_failure_lines_bounds_groups_and_handles_empty() {
        assert!(Report::default().failure_lines(3).is_empty());
        let report = Report {
            failed: (0..5).map(|i| (format!("D{i}.DSK"), format!("reason {i}"))).collect(),
            ..Default::default()
        };
        assert_eq!(report.failure_lines(2).len(), 2, "max bounds the group count");
    }
}

/// Fetch every disk that is not already there.
///
/// `progress` is called before each download with the name and the position in
/// the run, so a caller can say something on a screen that is about to be quiet
/// for a minute. Failures do not stop the run: one unavailable disk should not
/// cost the operator the other twenty-nine.
pub fn download_missing(
    images: &Path,
    mut progress: impl FnMut(&str, usize, usize),
) -> Result<Report, String> {
    std::fs::create_dir_all(images).map_err(|e| format!("{}: {e}", images.display()))?;
    let all = catalogue();
    let mut report = Report::default();
    let wanted = missing(images, &all);
    for d in &all {
        if !wanted.iter().any(|w| w.name == d.name) {
            report.skipped.push(d.name.clone());
        }
    }
    let total = wanted.len();
    for (i, disk) in wanted.iter().enumerate() {
        progress(&disk.name, i + 1, total);
        match fetch_one(disk) {
            Ok(bytes) => {
                // Written to a temporary name and renamed, so an interrupted
                // download cannot leave a half a disk image behind under a name
                // the rest of the gateway will then try to mount.
                let tmp = images.join(format!("{}.part", disk.name));
                let done = images.join(&disk.name);
                match std::fs::write(&tmp, &bytes).and_then(|_| std::fs::rename(&tmp, &done)) {
                    Ok(()) => report.fetched.push(disk.name.clone()),
                    Err(e) => {
                        let _ = std::fs::remove_file(&tmp);
                        report.failed.push((disk.name.clone(), format!("{e}")));
                    }
                }
            }
            Err(e) => report.failed.push((disk.name.clone(), e)),
        }
    }
    Ok(report)
}

#[cfg(test)]
mod generate {
    //! Building `altairduino.txt` from the disks this project has booted.
    //!
    //! Generated, never typed: the whole claim is that these exact bytes were
    //! surveyed, and a hand-kept list drifts from the survey the first time a
    //! disk changes category.

    use super::*;

    /// Regenerate the manifest.
    ///
    /// Ignored: it needs the local collections *and* the network. It downloads
    /// every candidate from the pinned URL and requires the bytes to match the
    /// local copy this project actually booted — so the manifest records what
    /// the URL really serves, verified against what was tested, rather than a
    /// hash of a local file nobody checked was the same.
    ///
    /// **Nothing is taken on trust from a folder listing.** A candidate is
    /// cold-started before it is written to the manifest, and one that does not
    /// boot is left out and reported. The manifest is what a user downloads by
    /// pressing one button, so every line in it has to be a disk that runs; the
    /// previous exclusion list was four names typed in from a survey run
    /// somewhere else, which is exactly the kind of claim that rots.
    ///
    /// Set `ALTAIR_DISKS` to Hansel's collection (default
    /// `$HOME/AltairRepos/Altair8800/disks`) and `DUINO_DISKS` to a checkout of
    /// `jpmcneely/AltairDuino-Disks` (default `$HOME/AltairRepos/AltairDuino-Disks`).
    #[test]
    #[ignore]
    fn record_altairduino_manifest() {
        let home = std::env::var("HOME").unwrap();
        let hansel = std::env::var("ALTAIR_DISKS")
            .unwrap_or_else(|_| format!("{home}/AltairRepos/Altair8800/disks"));
        let duino = std::env::var("DUINO_DISKS")
            .unwrap_or_else(|_| format!("{home}/AltairRepos/AltairDuino-Disks"));

        // **Both collections or nothing.**  `tools/cpm-live-gates` runs every
        // `#[ignore]` test, so this one runs on any machine that has Hansel's
        // disks — and writing a manifest from whichever collections happened to
        // be present would quietly drop four disks from the shipped catalogue
        // and look like a successful regeneration.  Skipping says so instead.
        for (what, dir) in [("ALTAIR_DISKS", &hansel), ("DUINO_DISKS", &duino)] {
            if !std::path::Path::new(dir).is_dir() {
                eprintln!("skipping: {what} is not a directory ({dir}) — both collections are \
                           needed to regenerate the manifest, so nothing was written");
                return;
            }
        }

        // Which source serves which disk.  Hansel's collection is taken whole;
        // McNeely's contributes only what Hansel does not have, because the four
        // names they share are *different disks* and Hansel's are the documented
        // ones.  Listing the five explicitly rather than diffing the folders:
        // a diff would silently pick up whatever a future checkout added, and
        // this file's whole promise is that a human decided each line.
        // NOT `DISK17.DSK`, and the reason is the whole point of this list being
        // hand-decided.  It is a name Hansel's collection does not have, so a
        // by-name comparison called it unique and it was very nearly shipped —
        // but its bytes are `DISK12.DSK`'s exactly, the IMP modem executive
        // under a second number.  Offering it would have downloaded one disk
        // twice.  A filename is not an identity, and a *missing* filename is not
        // missing content: the check that matters is the hash, which is why
        // `test_no_disk_is_offered_twice` now holds it.
        let from_duino: &[(&str, &str)] = &[
            ("HDSK04.DSK", "duino"),
            ("HDSK05.DSK", "duino-extra"),
            ("HDSK06.DSK", "duino-extra"),
            ("HDSK07.DSK", "duino-extra"),
        ];

        let mut candidates: Vec<(String, &str, std::path::PathBuf)> = Vec::new();
        let mut names: Vec<String> = std::fs::read_dir(&hansel)
            .expect("Hansel's collection")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.to_ascii_uppercase().ends_with(".DSK"))
            .collect();
        names.sort();
        for n in &names {
            candidates.push((n.clone(), "hansel", std::path::Path::new(&hansel).join(n)));
        }
        for (n, key) in from_duino {
            let folder = source_for(key).expect("a known source").folder;
            candidates.push((
                n.to_string(),
                key,
                std::path::Path::new(&duino).join(folder).join(n),
            ));
        }
        candidates.sort_by(|a, b| a.0.cmp(&b.0));

        let mut rows = String::new();
        let mut written = 0usize;
        let mut withheld: Vec<String> = Vec::new();
        let cfg = crate::config::get_config();
        let (mach, cpu) = (cfg.cpm_boot_machine.clone(), cfg.cpm_cpu.clone());
        drop(cfg);

        for (name, key, path) in &candidates {
            let local = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            let disk = Disk {
                name: name.clone(),
                bytes: local.len() as u64,
                sha256: sha256(&local),
                source: key.to_string(),
                note: String::new(),
            };
            let served = fetch_one(&disk).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(
                sha256(&served),
                disk.sha256,
                "{name}: the pinned URL serves different bytes from the copy that was booted"
            );
            // The bytes the URL serves are the ones cold-started, not the local
            // copy: what a user downloads is what has to boot.
            // Only a disk with no boot program is dropped.  A board mismatch
            // would mean the machine this ran on is configured for other
            // hardware, which says nothing about the disk — and silently
            // shrinking the shipped catalogue because of a local setting is
            // exactly the failure this must not have.
            match crate::cpm::boot_machine::BootMachine::bootability(served, &mach, &cpu) {
                crate::cpm::boot::Bootability::Boots => {}
                crate::cpm::boot::Bootability::NoBootProgram(e) => {
                    withheld.push(format!("{name} ({e})"));
                    eprintln!("  WITHHELD {name}: {e}");
                    continue;
                }
                crate::cpm::boot::Bootability::NoBoardForIt(e) => panic!(
                    "{name}: this machine has no board for it ({e}).  The manifest must be \
                     generated with cpm_boot_machine = auto, or it would drop good disks."
                ),
            }
            rows.push_str(&format!(
                "{}\t{}\t{}\t{}\t{}\n",
                disk.name,
                disk.bytes,
                disk.sha256,
                disk.source,
                describe(&disk.name)
            ));
            written += 1;
            eprintln!("  verified {} ({} bytes, {})", disk.name, disk.bytes, key);
        }

        let mut out = String::new();
        // Built line by line: a `\`-continued literal keeps the source's own
        // indentation in the output, which put six spaces in front of every
        // comment line the first time.
        let mut header = vec![
            "# Altair sample disks that this gateway is known to run.".to_string(),
            "#".to_string(),
            "# Generated by `record_altairduino_manifest` -- do not edit by hand.  Each line".to_string(),
            "# is NAME<TAB>BYTES<TAB>SHA256<TAB>SOURCE<TAB>NOTE.  The hash is of the bytes the".to_string(),
            "# pinned URL really served, checked against the local copy this project booted,".to_string(),
            "# and those same served bytes were then cold-started -- so every disk here is".to_string(),
            "# one that boots, not one that was believed to.".to_string(),
            "#".to_string(),
            "# SOURCE names a repository and folder in `SOURCES` (src/cpm/fetch.rs):".to_string(),
        ];
        for s in SOURCES {
            header.push(format!("#   {:<12} github.com/{}  ({}/)", s.key, s.repo, s.folder));
            header.push(format!("#   {:<12} pinned at {}", "", s.commit));
        }
        header.extend([
            "#".to_string(),
            "# Two repositories because neither contains the other.  Hansel's DISK13-DISK16".to_string(),
            "# are CP/M 3.0 disk 1 and 2, Felix and CP/M 2.2 MITS+Tarbell, and are documented".to_string(),
            "# as such; McNeely's four files of those names are DIFFERENT disks, undocumented".to_string(),
            "# in its own catalogue, one of which does not boot.  So the contested names come".to_string(),
            "# from Hansel and only the five McNeely uniquely has come from McNeely.".to_string(),
            "#".to_string(),
            "# The disks are not ours and are not shipped -- this fetches them from the".to_string(),
            "# original repositories on the operator's behalf.  The vintage software on them".to_string(),
            "# belongs to MITS, Microsoft, Digital Research and Infocom.".to_string(),
            "#".to_string(),
        ]);
        if !withheld.is_empty() {
            header.push("# Offered by neither screen because they did not cold-start when this".to_string());
            header.push("# was generated -- data companions that carry no boot program:".to_string());
            for w in &withheld {
                header.push(format!("#   {w}"));
            }
            header.push("#".to_string());
        }
        for line in header {
            out.push_str(&line);
            out.push('\n');
        }
        out.push_str(&rows);
        std::fs::write("src/cpm/altairduino.txt", &out).expect("write");
        eprintln!("wrote src/cpm/altairduino.txt: {written} disks, {} withheld", withheld.len());
    }

    /// A few words per disk, so the operator choosing whether to download knows
    /// what they are getting. Families rather than one line each: the survey
    /// knows what boots, not what it is, and inventing a description per disk
    /// would be exactly the hand-written drift this generator avoids.
    fn describe(name: &str) -> &'static str {
        match name {
            "TDISK04.DSK" => "CP/M 1.4 for the VDM-1 - paints the VDM screen, not the terminal",
            // The five from McNeely are named one by one, because unlike the
            // families below we know exactly what each is: its own catalogue
            // says so, and they are the reason that repository was added.
            "HDSK04.DSK" => "Altair 88-HDSK hard disk - Infocom adventures under CP/M",
            "HDSK05.DSK" => "Altair 88-HDSK hard disk - BASIC (Microsoft BASCOM)",
            "HDSK06.DSK" => "Altair 88-HDSK hard disk - COBOL",
            "HDSK07.DSK" => "Altair 88-HDSK hard disk - dBase II",
            "DISK17.DSK" => "MITS Altair 88-DCDD floppy - IMP modem executive",
            n if n.starts_with("HDSK") => "Altair 88-HDSK hard disk, 4.9 MB",
            n if n.starts_with("CDISK") => "Cromemco 4FDC/16FDC floppy - CDOS or CP/M 2.2",
            n if n.starts_with("TDISK") => "Tarbell 1011 floppy - CP/M",
            _ => "MITS Altair 88-DCDD floppy - CP/M, DOS or BASIC",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The catalogue is generated, so what matters is that every row survives
    /// the parse — a row silently dropped is a disk quietly missing from the
    /// offer, and nothing else would notice.
    #[test]
    fn test_the_manifest_parses_completely() {
        let rows = manifest_text()
            .lines()
            .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
            .count();
        let disks = catalogue();
        assert_eq!(disks.len(), rows, "a manifest row failed to parse");
        assert!(disks.len() >= 30, "the whole verified collection: got {}", disks.len());
    }

    /// Every entry has to be usable: a name, a real size, and a full hash.
    #[test]
    fn test_every_disk_is_completely_described() {
        for d in catalogue() {
            assert!(d.name.to_ascii_uppercase().ends_with(".DSK"), "{d:?}");
            assert!(d.bytes > 0, "{d:?}");
            assert_eq!(d.sha256.len(), 64, "{d:?}");
            assert!(d.sha256.chars().all(|c| c.is_ascii_hexdigit()), "{d:?}");
            assert!(!d.note.is_empty(), "{} has nothing to tell the operator", d.name);
        }
    }

    /// **The disks that do not work are not on offer.** Three data companions
    /// with no boot program and one blank: downloading those and finding they
    /// do nothing is the disappointment this feature exists to avoid.
    #[test]
    fn test_the_disks_that_do_not_run_are_not_offered() {
        let names: Vec<String> = catalogue().into_iter().map(|d| d.name).collect();
        for refused in ["DISK0B.DSK", "DISK0D.DSK", "DISK0F.DSK", "TDISK06.DSK"] {
            assert!(!names.contains(&refused.to_string()), "{refused} does not boot");
        }
        // And the one that boots to a screen rather than a port *is* offered —
        // it works, it just needs the VDM / Dazzler page to be seen.
        assert!(names.contains(&"TDISK04.DSK".to_string()));
    }

    /// Pinned to a commit, never a branch: "known to run" refers to particular
    /// bytes, and a branch would let upstream change what that means.
    ///
    /// Every disk, not the first one: with more than one source, checking
    /// `catalogue()[0]` would prove it of whichever repository sorts first and
    /// say nothing at all about the other.
    #[test]
    fn test_downloads_are_pinned_to_a_commit() {
        for d in catalogue() {
            let url = d.url();
            let s = source_for(&d.source).expect("a known source");
            assert!(url.contains(s.commit), "{url} is not pinned to {}", s.commit);
            assert_eq!(s.commit.len(), 40, "{} is not a full commit id", s.key);
            assert!(s.commit.chars().all(|c| c.is_ascii_hexdigit()), "{}", s.key);
            for branch in ["/master/", "/main/", "/HEAD/"] {
                assert!(!url.contains(branch), "a branch is not a pin: {url}");
            }
            assert!(url.starts_with("https://"), "{url}");
        }
    }

    /// **Every line resolves to a repository this build knows.**
    ///
    /// [`Disk::url`] unwraps its source, which is sound only because
    /// [`catalogue`] drops a line naming an unknown one — and that silent drop
    /// is exactly how a typo in the manifest would become a disk that quietly
    /// stopped being offered. This is what makes the drop loud.
    #[test]
    fn test_every_disk_names_a_source_that_exists() {
        let all = catalogue();
        let lines = manifest_text()
            .lines()
            .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
            .count();
        assert_eq!(all.len(), lines, "a manifest line was dropped by the parser");
        for d in &all {
            assert!(source_for(&d.source).is_some(), "{}: unknown source {}", d.name, d.source);
            let _ = d.url();
        }
    }

    /// **No disk is offered twice, under any name.**
    ///
    /// With two collections a disk can be unique by *filename* and identical by
    /// content, and that is not hypothetical: `DISK17.DSK` exists only in
    /// McNeely's collection, and is `DISK12.DSK` byte for byte. It passed a
    /// by-name uniqueness check and would have had operators download the IMP
    /// modem executive twice under two numbers. The hash is what identifies a
    /// disk; the name is what it happens to be filed under.
    #[test]
    fn test_no_disk_is_offered_twice() {
        let all = catalogue();
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(
                    a.sha256, b.sha256,
                    "{} and {} are the same disk under two names",
                    a.name, b.name
                );
                assert_ne!(a.name, b.name, "{} is listed twice", a.name);
            }
        }
    }

    /// Two folders of one repository are one place to trust, and are shown once.
    #[test]
    fn test_the_repositories_are_listed_once_each() {
        let repos = source_repos();
        let mut sorted = repos.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), repos.len(), "a repository is listed twice: {repos:?}");
        assert!(repos.iter().all(|r| r.starts_with("github.com/")), "{repos:?}");
        // The telnet screen prints one per line at two-space indent on a
        // 40-column PETSCII terminal.
        for r in &repos {
            assert!(r.len() + 2 <= 40, "{r} does not fit a C64 screen");
        }
    }

    /// **A file already in the images folder is never a candidate.** The
    /// operator may have edited it or put their own there under that name.
    #[test]
    fn test_an_existing_disk_is_never_overwritten() {
        let dir = std::env::temp_dir().join(format!("egfetch{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let all = catalogue();
        assert_eq!(missing(&dir, &all).len(), all.len(), "an empty folder wants them all");

        let mine = dir.join(&all[0].name);
        std::fs::write(&mine, b"my own disk").unwrap();
        let after = missing(&dir, &all);
        assert_eq!(after.len(), all.len() - 1);
        assert!(!after.iter().any(|d| d.name == all[0].name), "it is already there");
        assert_eq!(std::fs::read(&mine).unwrap(), b"my own disk", "and untouched");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The hash is the whole verification, so it is pinned against the
    /// specification's own vectors rather than trusted.
    #[test]
    fn test_sha256_against_the_published_vectors() {
        assert_eq!(
            sha256(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        // A block-boundary case, where the padding rule is easiest to get wrong.
        assert_eq!(
            sha256(&[b'a'; 64]),
            "ffe054fe7ae0cb6dc65c3af9b61d5209f439851db43d0ba5997337df154668eb"
        );
    }

    /// **The real thing, end to end**: fetch every disk from the pinned URLs
    /// into an empty folder, verify each against its recorded hash, then run it
    /// again and watch it touch nothing.
    ///
    /// Ignored: it needs the network and moves 17 MB.
    #[test]
    #[ignore]
    fn test_fetch_the_collection_for_real() {
        let dir = std::env::temp_dir().join(format!("egfetchlive{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let first = download_missing(&dir, |name, i, n| eprintln!("  {i}/{n} {name}"))
            .expect("the download runs");
        assert!(first.failed.is_empty(), "{:?}", first.failed);
        assert_eq!(first.fetched.len(), catalogue().len(), "all of them");
        assert!(first.skipped.is_empty(), "the folder was empty");

        // Every file is the size the manifest promised, on disk.
        for d in catalogue() {
            let got = std::fs::metadata(dir.join(&d.name)).expect(&d.name).len();
            assert_eq!(got, d.bytes, "{}", d.name);
        }

        // Run again: nothing is re-fetched and nothing is rewritten.
        let before = std::fs::metadata(dir.join(&catalogue()[0].name)).unwrap().modified().unwrap();
        let second = download_missing(&dir, |_, _, _| {}).expect("the second run");
        assert!(second.fetched.is_empty(), "nothing left to fetch");
        assert_eq!(second.skipped.len(), catalogue().len());
        let after = std::fs::metadata(dir.join(&catalogue()[0].name)).unwrap().modified().unwrap();
        assert_eq!(before, after, "an existing disk was rewritten");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **Every disk we offer must actually boot, from the bytes the source
    /// serves.**
    ///
    /// The whole promise of this feature is "known to run", and until now that
    /// rested on a survey of a *local* checkout plus the belief that the URL
    /// serves the same thing. This downloads them and boots each one, so the
    /// claim is tested end to end against the source an operator will use.
    ///
    /// `TDISK04` is the one exception and it is in the manifest deliberately:
    /// it boots and paints a VDM-1 screen instead of writing to a console port,
    /// so it produces no console output by design. Its note says so.
    ///
    /// Ignored: needs the network, and boots thirty machines.
    #[test]
    #[ignore]
    fn test_every_offered_disk_boots_when_downloaded() {
        use crate::cpm::boot_machine::BootMachine;
        let dir = std::env::temp_dir().join(format!("egboots{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let report = download_missing(&dir, |_, _, _| {}).expect("the download runs");
        assert!(report.failed.is_empty(), "{:?}", report.failed);

        let mut silent = Vec::new();
        for disk in catalogue() {
            let bytes = std::fs::read(dir.join(&disk.name)).expect(&disk.name);
            let (machine, _) =
                crate::cpm::detect::machine_for(crate::cpm::console::AUTO_MACHINE, &bytes);
            let mut m = BootMachine::new();
            m.set_machine(&machine);
            m.insert(0, bytes, true).unwrap_or_else(|e| panic!("{}: {e}", disk.name));
            let mut cpu = BootMachine::new_cpu();
            m.boot(&mut cpu, 0).unwrap_or_else(|e| panic!("{}: {e}", disk.name));
            let mut out = Vec::new();
            for _ in 0..20_000_000u64 {
                m.step(&mut cpu);
                out.extend(m.take_output());
                if out.len() > 40 {
                    break;
                }
            }
            if out.is_empty() {
                silent.push(disk.name.clone());
            }
            eprintln!("  {:<14} {}", disk.name, if out.is_empty() { "(silent)" } else { "spoke" });
        }
        assert_eq!(
            silent,
            vec!["TDISK04.DSK".to_string()],
            "a disk we recommend did not boot — the manifest is promising something untrue"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_the_report_says_what_happened() {
        let mut r = Report::default();
        assert_eq!(r.summary(), "Nothing to do.");
        r.fetched.push("A".into());
        r.skipped.push("B".into());
        r.failed.push(("C".into(), "boom".into()));
        assert_eq!(r.summary(), "1 downloaded, 1 already there, 1 failed");
    }
}
