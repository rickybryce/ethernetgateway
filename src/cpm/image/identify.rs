//! Deciding what format an image file is, and whether we trust that decision
//! enough to write to it.
//!
//! Three ways to arrive at a format, and they do not deserve the same
//! confidence:
//!
//! * **Named** — the filename carries a format token, `ibm3740_cpm22.dsk`.  The
//!   operator said what this is.  Mounts read-write.
//!
//! * **Verified** — no token, but the size names exactly one format *and* the
//!   whole directory holds together as that filesystem.  Mounts read-write.
//!
//! * **Sniffed** — the size names a format but the directory does not hold
//!   together, or several formats fit.  Mounts **read-only**, always.
//!
//! # Why the middle one exists
//!
//! The danger this module guards against is real: a *misidentified format* is
//! the one failure no later check can catch, because every offset is computed
//! from the wrong geometry, so every check agrees with every other check and the
//! first write lands in the middle of somebody's files. Everything else that
//! writes to an image is guarded by checks that can actually detect a
//! problem — a block off the end of the disk, a directory entry that did not
//! stick, two files claiming one block.
//!
//! But for a long time the answer to that danger was "rename the file", and that
//! was stricter than the evidence. **No two formats we support are the same
//! size.** So a size does not select *between* formats at all; it names one. The
//! realistic danger is therefore not "the wrong one of two candidates" but "a
//! file of that size which is not a CP/M filesystem at all" — a UCSD p-System
//! disk is 256,256 bytes, and so is a Cromemco CDOS one.
//!
//! And *that* question is answerable, by applying the same class of check the
//! write path already trusts, up front and to the whole directory rather than to
//! four entries: every block inside the disk, no block claimed twice, record
//! counts agreeing with the blocks claimed. Random bytes under a wrong geometry
//! fail it almost immediately. See [`directory_is_consistent`].
//!
//! The result is what somebody who drops a disk in the folder expects: it works,
//! without being told to rename it first. A disk that does *not* check out is
//! still readable, which is what you want when looking at an unknown disk, and
//! it says why it is read-only.

use super::format::{by_token, token_of, Format, FORMATS};

/// How a format was arrived at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    /// The filename named the format outright.
    Named,
    /// No token, but the size names exactly one format *and* the whole
    /// directory holds together as that filesystem.  Mounts read-write.
    ///
    /// This exists because the read-only rule was stricter than the evidence.
    /// The comment above is right that a misidentified format is the failure no
    /// later check can catch — but with one format per size there is no other
    /// format to be mistaken for, and the realistic danger is different: a file
    /// of that size which is not a CP/M filesystem at all. A UCSD p-System disk
    /// is 256,256 bytes. That danger is answerable, by
    /// [`directory_is_consistent`], and answering it is better than telling
    /// somebody who dropped a disk in the folder to rename it.
    Verified,
    /// The size names a format but the directory does not hold together — or
    /// more than one candidate matched.  Read-only, no exceptions.
    Sniffed,
}

/// A successful identification.
#[derive(Debug, Clone)]
pub struct Identified {
    pub format: &'static Format,
    pub confidence: Confidence,
    /// Why the filesystem check refused to trust this, when it did.
    ///
    /// Carried rather than discarded because "read-only" on its own sends people
    /// to the wrong fix — a damaged directory and a disk that is not CP/M at all
    /// want different things done about them.
    pub why: Option<&'static str>,
}

impl Identified {
    /// True when this image must be mounted read-only.
    pub fn force_read_only(&self) -> bool {
        self.confidence == Confidence::Sniffed
    }

    /// How this image was identified, in one phrase.
    ///
    /// Test-only: in production the same information reaches the operator as the
    /// mount's `read_only_reason`, which can be more specific because it also
    /// knows whether the *host* file is writable. This exists so the
    /// real-image survey can print a verdict per disk.
    #[cfg(test)]
    pub fn describe(&self) -> String {
        match (self.confidence, self.why) {
            (Confidence::Named, _) => "named by its filename".into(),
            (Confidence::Verified, _) => {
                "identified by inspection, filesystem checks out".into()
            }
            (Confidence::Sniffed, Some(why)) => format!("read-only: {why}"),
            (Confidence::Sniffed, None) => "identified by size only - read-only".into(),
        }
    }
}

/// Why an image could not be identified.  Each case gets its own message
/// because each has a different fix, and "could not mount" on its own sends
/// people to the wrong one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unknown {
    /// The filename named a format we do not have.
    NoSuchFormat(String),
    /// The named format does not fit a file this size.
    WrongSize {
        token: &'static str,
        expected: u64,
        actual: u64,
    },
    /// No token, and nothing in the table matches the size.
    NoMatchingFormat { size: u64 },
    /// The size matched, but there is no CP/M directory where the format says
    /// one should be — so this is not a CP/M disk, or not this format.
    NoDirectory { candidates: Vec<&'static str> },
    /// More than one format fits, and the content cannot separate them.
    Ambiguous { candidates: Vec<&'static str> },
}

impl std::fmt::Display for Unknown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unknown::NoSuchFormat(t) => write!(
                f,
                "no format called '{t}' — see readme.txt in the images folder"
            ),
            Unknown::WrongSize { token, expected, actual } => write!(
                f,
                "named as {token} but that format is {expected} bytes and this file is {actual}"
            ),
            Unknown::NoMatchingFormat { size } => write!(
                f,
                "no known format is {size} bytes — rename it with a format prefix, \
                 e.g. ibm3740_mydisk.dsk (see readme.txt)"
            ),
            Unknown::NoDirectory { candidates } => write!(
                f,
                "no CP/M directory found — this may not be a CP/M disk (tried {})",
                candidates.join(", ")
            ),
            Unknown::Ambiguous { candidates } => write!(
                f,
                "several formats fit this file ({}) — rename it with the right \
                 prefix to say which",
                candidates.join(", ")
            ),
        }
    }
}

/// Does a plausible CP/M directory sit where `fmt` says one should?
///
/// Only the *first* directory record is examined, and deliberately so.  Sector
/// skew scatters the rest of the directory among the data sectors, so a linear
/// scan of what should be "the directory" walks straight into file content and
/// concludes the disk is corrupt.  Logical sector 0 maps to physical sector 0
/// under every skew table seen, which makes the first record the one place the
/// directory is reliably findable before the format is known.
///
/// Takes the record itself rather than reading it, so the caller keeps
/// ownership of the medium and this stays testable without a file.
///
/// **This is the coarse gate, not the whole check.** The warning above is about a
/// *linear* scan; [`directory_is_consistent`] reads the rest of the directory
/// through the candidate format's own addressing, which de-skews it correctly, so
/// it does not walk into file content. The two are used in order: this one asks
/// "is there a directory here at all", and refusing here means "not a CP/M disk";
/// the other asks "does it hold together well enough to write to".
pub fn looks_like_directory(record: &[u8; 128]) -> bool {
    let mut live = 0;
    for i in 0..4 {
        let e = &record[i * 32..(i + 1) * 32];
        // Free, in either of its two spellings.
        if e[0] == 0xE5 || (e[0] == 0 && e[1..12].iter().all(|&c| c == 0)) {
            continue;
        }
        // A user number outside 0..15 is not a file entry.  (0x20/0x21 are
        // CP/M 3 labels and timestamps, which are legitimate but are not
        // evidence either way, so they are not counted as live.)
        if e[0] > 15 {
            return false;
        }
        // Names are printable 8.3.
        if e[1..12].iter().any(|&c| !(0x20..0x7F).contains(&(c & 0x7F))) {
            return false;
        }
        // A record count above 128 is impossible in one extent.
        if e[15] > 128 {
            return false;
        }
        live += 1;
    }
    live > 0
}

/// Does the whole directory hold together as a CP/M filesystem of this format?
///
/// This is what lets an unlabelled image be *written* to, and it is a different
/// kind of check from [`looks_like_directory`] — which asks whether four
/// entries look plausible, and a scrambled or foreign disk can pass that by
/// luck. This asks whether the directory is *consistent*, which random bytes
/// under the wrong geometry are not:
///
/// * every entry names printable 8.3, a user number of 0–15, and no more than
///   128 records in one extent;
/// * every allocation block is inside the disk (`1..=blocks`, and never 0,
///   which means "unused");
/// * an extent must claim **at least** as many blocks as its record count needs;
/// * **no block is claimed by two entries** — the one that random data fails
///   almost immediately, because collisions are overwhelmingly likely once
///   sixteen block numbers per entry are arbitrary bytes;
/// * an entry's record count agrees with how many blocks it claims.
///
/// An entirely erased directory passes: that is a freshly formatted disk, and
/// refusing to write to a blank would be perverse.
///
/// **There is deliberately no upper bound on blocks claimed**, and that is a
/// finding rather than an oversight. An earlier version required the count to be
/// within one of what the record count needs, which rejected a real Altair
/// 88-HDSK disk carrying 48 files: one directory entry can map more than one
/// logical extent, so `rc` describes only the last of them while the allocation
/// covers all. The rule looked reasonable and was wrong about real disks.
///
/// The same class of check the write path already applies — "a block off the end
/// of the disk, two files claiming one block" — but applied *before* trusting
/// the mount rather than after the first write. It cannot sit at "nearly right":
/// one out-of-range or double-claimed block fails it outright.
pub fn directory_is_consistent(dir: &[u8], format: &Format) -> Result<(), &'static str> {
    let blocks = format.data_records() / (format.blocksize / 128).max(1);
    let per_block = (format.blocksize / 128).max(1);
    let mut claimed: std::collections::HashSet<u16> = std::collections::HashSet::new();
    let mut live = 0usize;

    for e in dir.chunks_exact(32) {
        // Free, in either spelling.
        if e[0] == 0xE5 {
            continue;
        }
        // CP/M 3 disc labels and timestamp records are legitimate and carry no
        // allocation, so they are skipped rather than judged.
        if e[0] == 0x20 || e[0] == 0x21 {
            continue;
        }
        if e[0] > 15 {
            return Err("a user number no CP/M uses");
        }
        if e[1..12].iter().any(|&c| !(0x20..0x7F).contains(&(c & 0x7F))) {
            return Err("a filename that is not printable 8.3");
        }
        if e[15] > 128 {
            return Err("a record count too large for one extent");
        }
        // Sixteen 8-bit block numbers, or eight 16-bit ones. Which it is
        // follows from the disk's size, exactly as CP/M itself decides.
        let wide = blocks > 255;
        let nums: Vec<u16> = if wide {
            e[16..32].chunks_exact(2).map(|p| u16::from_le_bytes([p[0], p[1]])).collect()
        } else {
            e[16..32].iter().map(|&b| b as u16).collect()
        };
        let used: Vec<u16> = nums.into_iter().filter(|&b| b != 0).collect();
        for b in &used {
            if *b as u32 > blocks {
                return Err("an allocation block off the end of the disk");
            }
            if !claimed.insert(*b) {
                return Err("two directory entries claiming one block");
            }
        }
        // An extent's record count cannot need more blocks than it claims, nor
        // leave a claimed block entirely unaccounted for.
        let need = (e[15] as u32).div_ceil(per_block) as usize;
        if need > used.len() {
            return Err("an extent claiming fewer blocks than its records need");
        }
        live += 1;
    }
    // A blank disk is fine; so is a populated one. What is not fine is a
    // directory of nothing but labels, which is not evidence of anything.
    let _ = live;
    Ok(())
}

/// Identify the format of an image.
///
/// `filename` is the bare name in the images folder and `size` its length in
/// bytes.  `first_record` reads logical record 0 of the data area — the first
/// directory record — for a candidate format; the caller supplies it because
/// only the caller has the file open, and because where that record *is*
/// depends on which format is being considered.
pub fn identify<F>(
    filename: &str,
    size: u64,
    mut directory: F,
) -> Result<Identified, Unknown>
where
    F: FnMut(&Format) -> Option<Vec<u8>>,
{
    // --- named ----------------------------------------------------------
    if let Some(token) = token_of(filename) {
        let Some(format) = by_token(token) else {
            return Err(Unknown::NoSuchFormat(token.to_string()));
        };
        let need = format.min_bytes();
        if size < need {
            return Err(Unknown::WrongSize {
                token: format.token,
                expected: need,
                actual: size,
            });
        }
        // A named format is trusted even if the directory looks odd: the
        // operator may be mounting a freshly formatted, entirely blank disk,
        // and refusing that would be obstructive.
        return Ok(Identified { format, confidence: Confidence::Named, why: None });
    }

    // --- sniffed --------------------------------------------------------
    let by_size: Vec<&'static Format> = FORMATS
        .iter()
        .filter(|f| f.exact_size == Some(size))
        .collect();
    if by_size.is_empty() {
        return Err(Unknown::NoMatchingFormat { size });
    }
    let names: Vec<&'static str> = by_size.iter().map(|f| f.token).collect();

    // Content is the tie-breaker: which candidate has a directory where it
    // expects one — and does that directory hold together well enough to write
    // to?  Read once per candidate and judged twice, because the two questions
    // have different answers and different consequences.
    let mut matching: Vec<(&'static Format, Result<(), &'static str>)> = Vec::new();
    for f in by_size.iter().copied() {
        let Some(dir) = directory(f) else { continue };
        if dir.len() < 128 {
            continue;
        }
        let mut first = [0u8; 128];
        first.copy_from_slice(&dir[..128]);
        if !looks_like_directory(&first) {
            continue;
        }
        matching.push((f, directory_is_consistent(&dir, f)));
    }

    match matching.len() {
        0 => Err(Unknown::NoDirectory { candidates: names }),
        1 => {
            let (format, consistent) = matching[0];
            Ok(Identified {
                format,
                why: consistent.err(),
                // The whole point: a filesystem that checks out is writable
                // without being renamed.  One that does not is still readable,
                // which is what somebody looking at an unknown disk wants.
                confidence: if consistent.is_ok() {
                    Confidence::Verified
                } else {
                    Confidence::Sniffed
                },
            })
        }
        _ => Err(Unknown::Ambiguous {
            candidates: matching.iter().map(|(f, _)| f.token).collect(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory record holding one plausible entry, rest free.
    fn dir_record(name: &str) -> [u8; 128] {
        let mut r = [0xE5u8; 128];
        r[0] = 0;
        for (slot, c) in r[1..12].iter_mut().zip(name.bytes()) {
            *slot = c;
        }
        r[12] = 0;
        r[15] = 4;
        r
    }

    #[test]
    fn test_named_image_is_trusted_and_writable() {
        let fmt = by_token("altairhd").unwrap();
        let id = identify("altairhd_games.dsk", fmt.min_bytes(), |_| None).unwrap();
        assert_eq!(id.format.token, "altairhd");
        assert_eq!(id.confidence, Confidence::Named);
        assert!(!id.force_read_only(), "a named image mounts read-write");
    }

    /// A whole directory that holds together, for the read-write path.
    ///
    /// One file, one block, and every other entry erased — the shape a real disk
    /// has. Built rather than hand-typed so the record count and the block count
    /// agree, which is one of the things being checked.
    fn consistent_dir(fmt: &Format) -> Vec<u8> {
        let mut dir = vec![0xE5u8; fmt.maxdir as usize * 32];
        let e = &mut dir[..32];
        e[0] = 0; // user 0
        e[1..12].copy_from_slice(b"HELLO   TXT");
        e[12] = 0; // extent 0
        e[15] = 4; // 4 records, which fits in one 1K block
        e[16..32].fill(0);
        e[16] = 2; // block 2, the first after the directory
        dir
    }

    /// **The behaviour this change is for: drop a disk in, it works.**
    ///
    /// No filename token, and it mounts read-write because the filesystem checks
    /// out. Before this, the only way to write to it was to rename the file.
    #[test]
    fn test_an_unnamed_image_whose_filesystem_checks_out_is_writable() {
        for token in ["ibm3740", "altair8", "altairhd"] {
            let fmt = by_token(token).unwrap();
            let id = identify("whatever.dsk", fmt.min_bytes(), |f| {
                (f.token == token).then(|| consistent_dir(f))
            })
            .unwrap();
            assert_eq!(id.format.token, token);
            assert_eq!(id.confidence, Confidence::Verified, "{token}");
            assert!(!id.force_read_only(), "{token}: a checked filesystem is writable");
            assert!(id.describe().contains("checks out"));
        }
    }

    /// A directory that does *not* hold together stays read-only — the guard has
    /// to still bite, or the change would be a licence to corrupt disks.
    ///
    /// Each case is a separate way random or foreign bytes fail, and each is
    /// mutated from the *same* known-good directory so the only difference is the
    /// fault itself.
    #[test]
    fn test_an_inconsistent_directory_stays_read_only() {
        let fmt = by_token("ibm3740").unwrap();
        let blocks = fmt.data_records() / (fmt.blocksize / 128).max(1);

        type Mutate = Box<dyn Fn(&mut Vec<u8>)>;
        let cases: Vec<(&str, Mutate)> = vec![
            (
                "a block off the end of the disk",
                Box::new(move |d: &mut Vec<u8>| d[16] = (blocks + 8).min(255) as u8),
            ),
            (
                "two entries claiming one block",
                Box::new(|d: &mut Vec<u8>| {
                    let (a, b) = d.split_at_mut(32);
                    b[..32].copy_from_slice(a);
                    b[1..12].copy_from_slice(b"OTHER   TXT");
                }),
            ),
            (
                "fewer blocks claimed than the records need",
                Box::new(|d: &mut Vec<u8>| {
                    // 128 records is a full extent and needs 16 blocks of 1K;
                    // claiming one is impossible.
                    d[15] = 128;
                    d[16..32].fill(0);
                    d[16] = 2;
                }),
            ),
        ];

        for (what, mutate) in cases {
            let id = identify("whatever.dsk", fmt.min_bytes(), |f| {
                if f.token != "ibm3740" {
                    return None;
                }
                let mut d = consistent_dir(f);
                mutate(&mut d);
                Some(d)
            })
            .unwrap_or_else(|e| panic!("{what}: should still identify, got {e:?}"));
            assert_eq!(id.confidence, Confidence::Sniffed, "{what} must not be trusted");
            assert!(id.force_read_only(), "{what} must stay read-only");
        }
    }

    /// Some faults are caught *earlier* and refused outright rather than
    /// downgraded to read-only, which is the stronger answer.
    ///
    /// Worth its own test because the two outcomes are easy to confuse: a first
    /// record that is not a plausible directory at all means "this is not a CP/M
    /// disk", and mounting it read-only would be pretending otherwise.
    #[test]
    fn test_an_impossible_first_record_is_refused_outright() {
        let fmt = by_token("ibm3740").unwrap();
        for (what, at, value) in [
            ("a record count that cannot fit one extent", 15usize, 200u8),
            ("a user number no CP/M uses", 0usize, 0x7F),
        ] {
            let err = identify("whatever.dsk", fmt.min_bytes(), |f| {
                if f.token != "ibm3740" {
                    return None;
                }
                let mut d = consistent_dir(f);
                d[at] = value;
                Some(d)
            })
            .unwrap_err();
            assert!(
                matches!(err, Unknown::NoDirectory { .. }),
                "{what}: expected outright refusal, got {err:?}"
            );
        }
    }

    /// **Identify every real image in a folder and report the verdict.**
    ///
    /// The measurement this change stands or falls on, because the whole point is
    /// what happens to a disk somebody dropped in the folder *without* renaming
    /// it. Two things have to be true at once, and only real disks can show it:
    /// genuine CP/M disks must come out writable, and a file of the right size
    /// that is not a CP/M filesystem must not.
    ///
    /// The sample set contains exactly the awkward case — a UCSD p-System disk is
    /// 256,256 bytes, the same as an IBM 3740, and is not remotely a CP/M disk.
    ///
    /// Ignored: set `CPM_IDENTIFY_DIR` to a folder of images. Filenames are
    /// ignored on purpose — the point is identification without them.
    #[test]
    #[ignore]
    fn test_identify_real_images_without_their_names() {
        let Ok(dir) = std::env::var("CPM_IDENTIFY_DIR") else {
            eprintln!("set CPM_IDENTIFY_DIR to run this");
            return;
        };
        let mut names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.to_ascii_lowercase().ends_with(".dsk"))
            .collect();
        names.sort();
        let mut writable = 0;
        for name in &names {
            let path = std::path::Path::new(&dir).join(name);
            let bytes = std::fs::read(&path).unwrap();
            // Deliberately a name with no format token, whatever the file is
            // really called: this is the "dropped it in the folder" case.
            let r = identify("unnamed.dsk", bytes.len() as u64, |fmt| {
                let mut d = Vec::new();
                for rec in 0..fmt.dir_records() {
                    let off = fmt.data_record_offset(rec)? as usize;
                    d.extend_from_slice(bytes.get(off..off + 128)?);
                }
                Some(d)
            });
            match r {
                Ok(id) => {
                    if !id.force_read_only() {
                        writable += 1;
                    }
                    println!(
                        "  {:22} {:>9}  {:9} {:<10} {}",
                        name,
                        bytes.len(),
                        if id.force_read_only() { "READ-ONLY" } else { "writable" },
                        id.format.token,
                        id.describe()
                    );
                }
                Err(e) => println!("  {name:22} {:>9}  refused    {e}", bytes.len()),
            }
        }
        assert!(writable > 0, "no image in {dir} was identified well enough to write to");
    }

    /// A blank formatted disk must be writable. Refusing to write to an empty
    /// disk because it has no files would be perverse, and it is exactly what a
    /// "must contain a valid file" rule would do.
    #[test]
    fn test_a_blank_directory_is_writable() {
        let fmt = by_token("ibm3740").unwrap();
        let dir = vec![0xE5u8; fmt.maxdir as usize * 32];
        assert_eq!(directory_is_consistent(&dir, fmt), Ok(()), "an erased directory is consistent");
    }

    /// The rule this module exists for.
    #[test]
    fn test_sniffed_image_is_forced_read_only() {
        let fmt = by_token("altairhd").unwrap();
        let id = identify("games.dsk", fmt.min_bytes(), |f| {
            (f.token == "altairhd").then(|| dir_record("ED      COM").to_vec())
        })
        .unwrap();
        assert_eq!(id.format.token, "altairhd");
        assert_eq!(id.confidence, Confidence::Sniffed);
        assert!(
            id.force_read_only(),
            "a guessed format must never be written to"
        );
    }

    #[test]
    fn test_unknown_token_is_named_in_the_error() {
        let err = identify("banana_x.dsk", 256_256, |_| None).unwrap_err();
        assert_eq!(err, Unknown::NoSuchFormat("banana".into()));
        assert!(err.to_string().contains("banana"));
        assert!(err.to_string().contains("readme.txt"), "point at the docs");
    }

    /// A named format that cannot fit the file is refused rather than mounted
    /// and left to fail on every read.
    #[test]
    fn test_named_but_wrong_size_is_refused() {
        let err = identify("altairhd_short.dsk", 1000, |_| None).unwrap_err();
        match err {
            Unknown::WrongSize { token, actual, .. } => {
                assert_eq!(token, "altairhd");
                assert_eq!(actual, 1000);
            }
            other => panic!("expected WrongSize, got {other:?}"),
        }
    }

    #[test]
    fn test_no_format_of_that_size() {
        let err = identify("mystery.dsk", 12_345, |_| None).unwrap_err();
        assert_eq!(err, Unknown::NoMatchingFormat { size: 12_345 });
        assert!(err.to_string().contains("ibm3740_"), "suggest the convention");
    }

    /// A file the right size that is not a CP/M disk at all — the minidisk and
    /// hard-disk images in the sample set are exactly this.
    #[test]
    fn test_right_size_but_no_directory_is_refused() {
        let fmt = by_token("ibm3740").unwrap();
        let err = identify("something.dsk", fmt.min_bytes(), |_| Some(vec![0x00u8; 128]))
            .unwrap_err();
        match &err {
            Unknown::NoDirectory { candidates } => assert!(candidates.contains(&"ibm3740")),
            other => panic!("expected NoDirectory, got {other:?}"),
        }
        assert!(err.to_string().contains("may not be a CP/M disk"));
    }

    #[test]
    fn test_unreadable_candidate_does_not_panic() {
        let fmt = by_token("ibm3740").unwrap();
        let err = identify("x.dsk", fmt.min_bytes(), |_| None).unwrap_err();
        assert!(matches!(err, Unknown::NoDirectory { .. }));
    }

    // ---- the directory heuristic ----------------------------------------

    /// Point this at the real sample images and confirm each one is either
    /// identified correctly or refused for the right reason.
    ///
    /// The refusals matter as much as the matches: not every `.DSK` holds a
    /// CP/M filesystem.  The Altair minidisk and hard-disk images in the sample
    /// set are Disk BASIC and unformatted respectively, and mounting either as
    /// CP/M would show a directory made of file data.
    ///
    /// Ignored: needs `CPM_IMAGE_DIR`.
    #[test]
    #[ignore]
    fn test_identifies_the_real_sample_images() {
        let Ok(dir) = std::env::var("CPM_IMAGE_DIR") else {
            eprintln!("set CPM_IMAGE_DIR to run this test");
            return;
        };
        let dir = std::path::PathBuf::from(dir);

        // (file, expected token or None if it must be refused)
        let cases: [(&str, Option<&str>); 4] = [
            ("TDISK01.DSK", Some("ibm3740")),
            // Altair 88-DCDD.  `None` here until 2026-08-01, when the block
            // mapping was solved and `altair8` went back into FORMATS — the
            // expectation outlived the fact by three days because this gate is
            // `#[ignore]` and nobody re-ran it.
            ("DISK01.DSK", Some("altair8")),
            ("DISK0C.DSK", None),  // Altair minidisk — Disk BASIC, not CP/M
            ("HDSK01.DSK", None),  // hard disk — no CP/M directory anywhere
        ];

        for (file, want) in cases {
            let path = dir.join(file);
            let Ok(bytes) = std::fs::read(&path) else {
                eprintln!("{file} not present — skipping");
                continue;
            };
            let size = bytes.len() as u64;
            let got = identify(file, size, |f| {
                let mut dir = Vec::new();
                for rec in 0..f.dir_records() {
                    let off = f.data_record_offset(rec)?;
                    let end = off.checked_add(128)?;
                    if end > size {
                        return None;
                    }
                    dir.extend_from_slice(&bytes[off as usize..end as usize]);
                }
                (!dir.is_empty()).then_some(dir)
            });
            match (want, got) {
                (Some(token), Ok(id)) => {
                    assert_eq!(id.format.token, token, "{file}");
                    // These filenames carry no token, so they take the
                    // inspection path — and a real CP/M disk must come out of it
                    // WRITABLE. Until 2026-08-05 this asserted `Sniffed` and
                    // read-only, which was the behaviour before an unlabelled
                    // disk could be trusted; the whole point of the change is
                    // that a sound filesystem no longer has to be renamed.
                    assert_eq!(
                        id.confidence,
                        Confidence::Verified,
                        "{file}: a real CP/M disk must verify without its name"
                    );
                    assert!(!id.force_read_only(), "{file}: verified means writable");
                }
                (Some(token), Err(e)) => panic!("{file}: expected {token}, got refusal: {e}"),
                (None, Err(_)) => {}
                (None, Ok(id)) => panic!(
                    "{file} is not a CP/M disk but was identified as {}",
                    id.format.token
                ),
            }
        }
    }

    #[test]
    fn test_directory_heuristic_accepts_a_real_looking_record() {
        assert!(looks_like_directory(&dir_record("STAT    COM")));
    }

    #[test]
    fn test_directory_heuristic_rejects_file_data() {
        // 8080 code: high bytes, unprintable names.
        let mut r = [0u8; 128];
        for (i, b) in r.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(7).wrapping_add(0xC3);
        }
        assert!(!looks_like_directory(&r));
    }

    /// An all-free record is not evidence of a directory — a blank disk and a
    /// zero-filled data block look identical, and treating that as a match
    /// would let any zero-filled file mount as any format.
    #[test]
    fn test_directory_heuristic_rejects_an_empty_record() {
        assert!(!looks_like_directory(&[0xE5u8; 128]));
        assert!(!looks_like_directory(&[0x00u8; 128]));
    }

    #[test]
    fn test_directory_heuristic_rejects_impossible_record_count() {
        let mut r = dir_record("GOOD    COM");
        r[15] = 200; // more than 128 records in one extent
        assert!(!looks_like_directory(&r));
    }

    #[test]
    fn test_directory_heuristic_rejects_bad_user_number() {
        let mut r = dir_record("GOOD    COM");
        r[0] = 99;
        assert!(!looks_like_directory(&r));
    }
}
