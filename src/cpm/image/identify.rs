//! Deciding what format an image file is, and whether we trust that decision
//! enough to write to it.
//!
//! There are two ways to arrive at a format, and they do not deserve the same
//! confidence:
//!
//! * **Named** — the filename carries a format token, `ibm3740_cpm22.dsk`.  The
//!   operator said what this is.  Mounts read-write.
//!
//! * **Sniffed** — no token, so the format is inferred from the file's size and
//!   the shape of what sits where a directory should be.  Mounts **read-only**,
//!   always.
//!
//! The asymmetry is deliberate and it is the last line of defence in this
//! module.  Everything else that writes to an image is guarded by checks that
//! can actually detect the problem — a block off the end of the disk, a
//! directory entry that did not stick, two files claiming one block.  A
//! *misidentified format* is the one failure none of those can catch: every
//! offset is computed from the wrong geometry, so every check agrees with every
//! other check, and the first write lands in the middle of somebody's files.
//!
//! Sniffing is therefore for *reading* an image someone dropped in the folder
//! unlabelled.  Renaming it with a token is how you say "I know what this is",
//! and that is what unlocks writing.

use super::format::{by_token, token_of, Format, FORMATS};

/// How a format was arrived at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    /// The filename named the format outright.
    Named,
    /// The format was inferred.  Read-only, no exceptions.
    Sniffed,
}

/// A successful identification.
#[derive(Debug, Clone)]
pub struct Identified {
    pub format: &'static Format,
    pub confidence: Confidence,
}

impl Identified {
    /// True when this image must be mounted read-only.
    pub fn force_read_only(&self) -> bool {
        self.confidence == Confidence::Sniffed
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
    mut first_record: F,
) -> Result<Identified, Unknown>
where
    F: FnMut(&Format) -> Option<[u8; 128]>,
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
        return Ok(Identified { format, confidence: Confidence::Named });
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
    // expects one?
    let matching: Vec<&'static Format> = by_size
        .iter()
        .copied()
        .filter(|f| first_record(f).is_some_and(|r| looks_like_directory(&r)))
        .collect();

    match matching.len() {
        0 => Err(Unknown::NoDirectory { candidates: names }),
        1 => Ok(Identified {
            format: matching[0],
            confidence: Confidence::Sniffed,
        }),
        _ => Err(Unknown::Ambiguous {
            candidates: matching.iter().map(|f| f.token).collect(),
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

    /// The rule this module exists for.
    #[test]
    fn test_sniffed_image_is_forced_read_only() {
        let fmt = by_token("altairhd").unwrap();
        let id = identify("games.dsk", fmt.min_bytes(), |f| {
            (f.token == "altairhd").then(|| dir_record("ED      COM"))
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
        let err = identify("something.dsk", fmt.min_bytes(), |_| Some([0x00u8; 128]))
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
                let off = f.data_record_offset(0)?;
                let end = off.checked_add(128)?;
                if end > size {
                    return None;
                }
                bytes[off as usize..end as usize].try_into().ok()
            });
            match (want, got) {
                (Some(token), Ok(id)) => {
                    assert_eq!(id.format.token, token, "{file}");
                    assert_eq!(
                        id.confidence,
                        Confidence::Sniffed,
                        "{file}: these filenames carry no token"
                    );
                    assert!(id.force_read_only(), "{file}: sniffed means read-only");
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
