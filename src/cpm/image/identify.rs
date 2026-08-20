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
use super::fs::Params;

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
                "no format called '{t}' - see readme.txt in the images folder"
            ),
            Unknown::WrongSize { token, expected, actual } => write!(
                f,
                "named as {token} but that format is {expected} bytes and this file is {actual}"
            ),
            // Deliberately does NOT suggest renaming. A prefix says which
            // layout to use, not how big the file is, so it cannot help a size
            // nothing here mounts — and it used to be actively harmful advice:
            // taking it turned this refusal into a trusted read-write mount of
            // the wrong geometry, which is what `Format::max_bytes` now stops.
            // The genuinely useful remedy is the other one, because a disk we
            // cannot mount is often still bootable — every Cromemco
            // double-density image is exactly that.
            Unknown::NoMatchingFormat { size } => write!(
                f,
                "no known format is {size} bytes - nothing here mounts a disk \
                 that size, and renaming cannot change that. It may still be \
                 bootable: set it as the boot disk (see readme.txt)"
            ),
            // Says what to do about it, like its sibling above.  A disk with a
            // size we know but no CP/M directory is very often a disk that is
            // not CP/M *at all* and boots its own operating system — the Altair
            // hard disks carrying Disk BASIC and the Accounting System are
            // exactly that, and they boot perfectly.  Reporting only "no
            // directory" sent the operator looking for a fault in a disk that
            // works.
            Unknown::NoDirectory { candidates } => write!(
                f,
                "no CP/M directory found - this is probably not a CP/M disk. It \
                 may still boot its own operating system: set it as the boot disk. \
                 (tried {})",
                candidates.join(", ")
            ),
            Unknown::Ambiguous { candidates } => write!(
                f,
                "several formats fit this file ({}) - rename it with the right \
                 prefix to say which",
                candidates.join(", ")
            ),
        }
    }
}

#[cfg(test)]
mod refusal_tests {
    use super::*;

    /// **Every refusal an operator can read must be plain ASCII.**
    ///
    /// These go out over telnet, where a 40-column PETSCII terminal renders a
    /// UTF-8 em dash as three garbage glyphs rather than one character — which
    /// is exactly how Ricky saw `no CP/M directory found â this may not be...`
    /// on a real session. The width tests cannot catch it: a multi-byte char
    /// counts as one `char` and three bytes on the wire.
    #[test]
    fn test_every_refusal_is_ascii() {
        let reasons = [
            Unknown::NoSuchFormat("bogus".to_string()),
            Unknown::WrongSize { token: "ibm3740", expected: 256_256, actual: 1 },
            Unknown::NoMatchingFormat { size: 12_345 },
            Unknown::NoDirectory { candidates: vec!["altairhd"] },
            Unknown::Ambiguous { candidates: vec!["a", "b"] },
        ];
        for r in reasons {
            let text = r.to_string();
            assert!(
                text.is_ascii(),
                "a refusal an operator reads on telnet is not ASCII: {text:?}"
            );
        }
    }

    /// **The same rule, applied to the modules the test above cannot reach.**
    ///
    /// `test_every_refusal_is_ascii` enumerates `Unknown`, which is possible
    /// because its variants are *values*. Every sibling message an operator
    /// reads on the same screen is built by `format!` at the point of use and
    /// cannot be enumerated at all — so for three years the rule held in the one
    /// module that could state it, and eleven messages in the modules next door
    /// carried U+2014 to the same telnet session. Two of them
    /// (`registry.rs`'s "held by a booted disk", "try again once that session
    /// leaves CP/M") are the refusals an operator meets most often, and every
    /// `cpm_emu_uart` description was one too.
    ///
    /// A source scan is the honest test here, the same technique as
    /// `telnet/tests.rs`'s `test_no_petscii_translator_maps_backspace_to_
    /// destructive_del`: what is being asserted is a property of the *text in
    /// the file*, and there is no value to hold.
    ///
    /// What actually goes wrong is worth stating, because it is not only ugly.
    /// PETSCII is the milder case: `to_latin1_bytes` maps anything above U+00FF
    /// to `?`, so the column count survives. `TerminalType::Ascii` takes the
    /// other branch of `send()` and writes the UTF-8 bytes untouched, so an em
    /// dash is three columns where `truncate_to_width` counted one — and the
    /// modem line on the CP/M settings screen is truncated to an exact width.
    ///
    /// Scoped to the modules whose strings reach `send()`/`send_line()`:
    /// mount and boot refusals, and the three config UIs' shared choice lists.
    /// `glog!` output is deliberately *not* in scope — it goes to the console
    /// and the log file, which are not 40 columns of PETSCII.
    #[test]
    fn test_no_operator_facing_string_is_non_ascii() {
        // (label, source).  Every one of these builds text that a telnet
        // session prints verbatim.
        let sources: [(&str, &str); 6] = [
            ("cpm/image/mod.rs", include_str!("mod.rs")),
            ("cpm/image/registry.rs", include_str!("registry.rs")),
            ("cpm/image/identify.rs", include_str!("identify.rs")),
            ("cpm/boot.rs", include_str!("../boot.rs")),
            ("cpm/uart.rs", include_str!("../uart.rs")),
            ("cpm/console.rs", include_str!("../console.rs")),
        ];
        for (label, src) in sources {
            let bad = non_ascii_literals(src);
            assert!(
                bad.is_empty(),
                "{label}: {} string(s) an operator may read on telnet are not ASCII: {bad:#?}\n\
                 Use '-' rather than an em dash; see this test for why.",
                bad.len()
            );
        }
    }

    /// Every non-ASCII double-quoted literal in `src` that is **not** inside a
    /// `#[cfg(test)]` item.
    ///
    /// Comments are skipped rather than stripped, because the prose in this
    /// project uses em dashes freely and correctly — the rule is about text
    /// that reaches a terminal, not about the source. Test modules are skipped
    /// for the same reason: a test's assertion message is read in a terminal
    /// that is not a C64.
    ///
    /// Deliberately simple, and the simplifications are safe here rather than
    /// in general: no file in the list uses raw strings (checked), and a
    /// `\u{2014}` escape would evade this scan — it is written in ASCII, so it
    /// is not what anybody types by accident, which is the failure this catches.
    fn non_ascii_literals(src: &str) -> Vec<String> {
        let c: Vec<char> = src.chars().collect();
        let mut out = Vec::new();
        let mut i = 0usize;
        let mut depth: i32 = 0;
        // Depth at which the innermost `#[cfg(test)]` item opened, if we are in
        // one.  Covers `mod tests` and bare `#[cfg(test)] fn` helpers alike.
        let mut test_at: Option<i32> = None;
        let mut pending_cfg_test = false;
        while i < c.len() {
            // Line comment.
            if c[i] == '/' && c.get(i + 1) == Some(&'/') {
                while i < c.len() && c[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            // Block comment.  Not nested-aware; none of these files nest them.
            if c[i] == '/' && c.get(i + 1) == Some(&'*') {
                i += 2;
                while i + 1 < c.len() && !(c[i] == '*' && c[i + 1] == '/') {
                    i += 1;
                }
                i = (i + 2).min(c.len());
                continue;
            }
            // String literal.
            if c[i] == '"' {
                i += 1;
                let mut lit = String::new();
                while i < c.len() && c[i] != '"' {
                    if c[i] == '\\' {
                        // Keep the escape out of the text; it cannot be the
                        // non-ASCII we are looking for.
                        i += 2;
                        continue;
                    }
                    lit.push(c[i]);
                    i += 1;
                }
                i += 1;
                if test_at.is_none() && !lit.is_ascii() {
                    out.push(lit);
                }
                continue;
            }
            // A quote is a char literal only when it closes like one; anything
            // else is a lifetime, and swallowing `'static` would eat the rest
            // of the file.
            if c[i] == '\'' {
                if c.get(i + 1) == Some(&'\\') {
                    i += 2;
                    while i < c.len() && c[i] != '\'' {
                        i += 1;
                    }
                    i += 1;
                    continue;
                }
                if c.get(i + 2) == Some(&'\'') {
                    i += 3;
                    continue;
                }
                i += 1;
                continue;
            }
            // `#[cfg(test)]` arms the next block we open.
            if c[i] == '#' && c[i..].iter().take(12).collect::<String>() == "#[cfg(test)]" {
                pending_cfg_test = true;
                i += 12;
                continue;
            }
            if c[i] == '{' {
                if pending_cfg_test && test_at.is_none() {
                    test_at = Some(depth);
                    pending_cfg_test = false;
                }
                depth += 1;
                i += 1;
                continue;
            }
            if c[i] == '}' {
                depth -= 1;
                if test_at == Some(depth) {
                    test_at = None;
                }
                i += 1;
                continue;
            }
            i += 1;
        }
        out
    }

    /// The scanner itself is load-bearing, so it is tested: a guard that cannot
    /// fire is worse than no guard, and every simplification above is a way for
    /// it to quietly stop firing.
    #[test]
    fn test_the_non_ascii_scan_can_actually_fire() {
        assert!(non_ascii_literals("let s = \"plain ascii\";").is_empty());
        assert_eq!(
            non_ascii_literals("let s = \"an em dash \u{2014} here\";"),
            vec!["an em dash \u{2014} here".to_string()],
            "the scan must see a literal em dash"
        );
        assert!(
            non_ascii_literals("// a comment \u{2014} with a dash\n").is_empty(),
            "prose in comments is not the rule"
        );
        assert!(
            non_ascii_literals("/* block \u{2014} dash */").is_empty(),
            "nor in block comments"
        );
        assert!(
            non_ascii_literals("#[cfg(test)]\nmod t { fn f() { let s = \"x \u{2014} y\"; } }")
                .is_empty(),
            "a test module's own messages are not read on a C64"
        );
        assert_eq!(
            non_ascii_literals(
                "#[cfg(test)]\nmod t { fn f() { let s = \"x \u{2014} y\"; } }\nfn g() { let s = \"z \u{2014} w\"; }"
            ),
            vec!["z \u{2014} w".to_string()],
            "and production code AFTER a test module is still in scope"
        );
        assert!(
            non_ascii_literals("fn f<'a>(x: &'a str) -> &'static str { \"ok\" }").is_empty(),
            "a lifetime must not be mistaken for a char literal"
        );
        assert_eq!(
            non_ascii_literals("let q = '\"'; let s = \"a \u{2014} b\";"),
            vec!["a \u{2014} b".to_string()],
            "a quote char literal must not desynchronise the scan"
        );
        assert!(
            non_ascii_literals("let s = \"escaped \\\" quote\";").is_empty(),
            "an escaped quote does not end the literal"
        );
    }

    /// **A disk we cannot mount is often one that boots**, and the refusal has
    /// to say so — the Altair hard disks carrying Disk BASIC and the Accounting
    /// System have no CP/M directory and boot perfectly. Saying only "no
    /// directory" sent an operator looking for a fault in a working disk.
    #[test]
    fn test_a_disk_that_is_not_cpm_is_pointed_at_the_boot_picker() {
        let text = Unknown::NoDirectory { candidates: vec!["altairhd"] }.to_string();
        assert!(text.contains("boot"), "{text}");
        // And its sibling, which had this right first and is the reason the
        // gap was visible at all.
        assert!(Unknown::NoMatchingFormat { size: 999 }.to_string().contains("boot"));
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
        // A user number outside 0..15 is not a file entry — CP/M's own
        // directory search compares this byte against the current user number
        // and so can never match one — but it is **not** grounds to reject the
        // disk. It used to be, and that refused the ITC CP/M 2.2 disks outright:
        // their first directory entry is a vendor volume label, first byte
        // `0x81`, name "Userdisk", followed by ordinary files.
        //
        // Not counted as live either, which is what keeps this strict: the
        // `live > 0` at the end still demands a real file entry, with a
        // printable 8.3 name, so a record of nothing but unmatched bytes fails
        // exactly as it did before. (0x20/0x21 are CP/M 3 labels and timestamps
        // and are skipped by the same rule.)
        if e[0] > 15 {
            continue;
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

/// Is every entry in this directory erased — a freshly *formatted* disk?
///
/// **Only `E5` counts here**, and that is the point. Elsewhere an all-zero entry
/// is treated as free too, because populated directories do contain them, but a
/// directory that is zero from end to end is not evidence of a formatted disk —
/// it is evidence of a file nobody has written to. `E5` is what a format writes,
/// deliberately, and it is the only thing that says "this disk was formatted and
/// is empty" rather than "these bytes have never meant anything".
///
/// The distinction earns its keep: it admits our own `blank_image()` output while
/// still refusing a 256,256-byte file of zeros, which could be anything.
///
/// Deliberately demands the *whole* directory. One erased record proves nothing —
/// a file of padding has plenty — but a directory erased from end to end at
/// exactly the offsets this format computes is a blank disk of this format.
pub fn is_erased_directory(dir: &[u8]) -> bool {
    !dir.is_empty() && dir.iter().all(|&b| b == 0xE5)
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
/// * every allocation block is inside the disk (`1..=DSM`, and never 0, which
///   means "unused") and outside the directory's own blocks;
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
///
/// **Every number here comes from [`Params::derive`], not from the format
/// directly**, because "the same check at two moments" is only true if both
/// moments compute it the same way — and they did not. This function used to
/// derive its own from [`Format::data_blocks`], which returns a *count*: it
/// compared `b > blocks` where the last legal block is `blocks - 1`, so an entry
/// naming one block past the end of the disk passed, and it decided the
/// allocation-map width on `blocks > 255` where CP/M's rule is `DSM > 255`,
/// which disagrees with the mount at exactly 256 blocks and would decode eight
/// 16-bit block numbers out of bytes the mount reads as sixteen 8-bit ones.
/// The first was live (no format has 256 blocks, so the second was not), and
/// `ImageFs::mount` caught it afterwards and forced read-only — but with the
/// generic "the directory is damaged" rather than the specific reason
/// [`Identified::why`] exists to carry. `data_blocks` was itself created because
/// this arithmetic lived in four places; this was the fifth.
pub fn directory_is_consistent(dir: &[u8], format: &Format) -> Result<(), &'static str> {
    let params = Params::derive(format);
    let per_block = params.records_per_block.max(1);
    // Blocks the directory itself occupies. An entry claiming one of them is
    // corrupt in the same way as one off the end — the mount path has always
    // rejected it (`ImageFs::inconsistency`) and this side never did.
    let dir_blocks = params.dir_records.div_ceil(per_block);
    let mut claimed: std::collections::HashSet<u16> = std::collections::HashSet::new();
    let mut live = 0usize;
    let mut other = 0usize;

    for e in dir.as_chunks::<32>().0 {
        // Free, in either spelling.
        if e[0] == 0xE5 {
            continue;
        }
        // CP/M 3 disc labels and timestamp records are legitimate and carry no
        // allocation, so they are skipped rather than judged.
        if e[0] == 0x20 || e[0] == 0x21 {
            other += 1;
            continue;
        }
        // Any other user byte outside 0-15 is **skipped, not fatal**.
        //
        // This used to reject the whole disk, and it was wrong about real ones:
        // the ITC CP/M 2.2 disks carry a vendor volume label as their first
        // directory entry, first byte `0x81`, name "Userdisk". Rejecting on it
        // said "this may not be a CP/M disk" about a disk whose every other
        // entry is an ordinary file.
        //
        // Skipping is also what **CP/M itself does**: a directory search
        // compares the entry's user byte with the current user number, which is
        // 0-15, so an entry outside that range is never matched by anything.
        // Ignoring it is the emulation, not a relaxation of one.
        //
        // The strictness that makes size-plus-inspection safe is kept by the
        // `live` rule at the end: skipping cannot turn a directory of noise into
        // a pass, because noise produces no well-formed file entries either.
        if e[0] > 15 {
            other += 1;
            continue;
        }
        if e[1..12].iter().any(|&c| !(0x20..0x7F).contains(&(c & 0x7F))) {
            return Err("a filename that is not printable 8.3");
        }
        if e[15] > 128 {
            return Err("a record count too large for one extent");
        }
        // Sixteen 8-bit block numbers, or eight 16-bit ones. Which it is
        // follows from the disk's size, exactly as CP/M itself decides — and
        // from the same `wide_blocks` the mount will use, not a second reading
        // of the rule.
        let nums: Vec<u16> = if params.wide_blocks {
            e[16..32].as_chunks::<2>().0.iter().map(|p| u16::from_le_bytes(*p)).collect()
        } else {
            e[16..32].iter().map(|&b| b as u16).collect()
        };
        let used: Vec<u16> = nums.into_iter().filter(|&b| b != 0).collect();
        for b in &used {
            if *b > params.max_block {
                return Err("an allocation block off the end of the disk");
            }
            if (*b as u32) < dir_blocks {
                return Err("an allocation block inside the directory");
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
    // A blank disk is fine — every entry free — and so is a populated one. What
    // is not fine is a directory with no CP/M file entries in it at all but
    // something in it nonetheless: that is not evidence of this filesystem, it
    // is evidence of bytes. This is the rule that lets the skips above be safe,
    // because it is what noise cannot satisfy.
    if live == 0 && other > 0 {
        return Err("no CP/M file entries, only records CP/M would never match");
    }
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
    //
    // Only an *understood* token takes this path. An unrecognised one falls
    // through to inspection rather than refusing the disk, and that is a
    // deliberate change: `token_of` calls anything before the first underscore a
    // token, so `TDISK03_comal.dsk` or `my_backup.dsk` — perfectly ordinary
    // names — used to be rejected outright with "no format called 'TDISK03'".
    // Outright rejection is worse than read-only: the disk cannot be used at
    // all, and the user did nothing wrong. If inspection also fails we report
    // the token confusion, because by then it is the likelier explanation.
    let named = token_of(filename).map(|t| (t, by_token(t)));
    if let Some((_, Some(format))) = named {
        let need = format.min_bytes();
        // Both ends. Too small was always refused; too *large* was not, and a
        // name is the one path that mounts read-write without inspecting
        // anything — so the only guard was that the file was big enough. A
        // Cromemco double-density image renamed `ibm3740_*.dsk` is 625,920
        // bytes against the 256,256 that format needs, and was accepted: read
        // as single density, directory landing mid-track, and writable.
        //
        // The gateway used to *recommend* exactly that, too — the refusal for
        // an unknown size said "rename it with a format prefix". See
        // `Unknown::NoMatchingFormat`.
        if size < need || size > format.max_bytes() {
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
    //
    // The same trailer tolerance the named path uses, and for the same reason:
    // an image in circulation may carry a few bytes past its last record, and
    // that does not make it a different disk. This required an *exact* match
    // until it was measured — `DISK13`, `DISK14` and `DISK16` are 337,664
    // bytes, which is an `altair8` disk plus 96, and all three boot perfectly
    // while mounting refused them outright on size before their directory was
    // ever looked at. The identical 96-byte trailer was one of the two root
    // causes that once stopped these disks *booting*; it was fixed there and
    // left here.
    //
    // Widening this costs no safety, because size was never what made a disk
    // writable: whatever it lets through still has its whole directory checked
    // below, and fails to read-only *with the reason* if it does not hold up.
    let by_size: Vec<&'static Format> = FORMATS
        .iter()
        .filter(|f| f.exact_size.is_some_and(|exact| (exact..=f.max_bytes()).contains(&size)))
        .collect();
    // An unrecognised token becomes the explanation only when inspection has
    // nothing better to offer.
    let unknown_token = || match named {
        Some((t, None)) => Some(Unknown::NoSuchFormat(t.to_string())),
        _ => None,
    };
    if by_size.is_empty() {
        return Err(unknown_token().unwrap_or(Unknown::NoMatchingFormat { size }));
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
        // A directory with nothing live in it is *also* a CP/M directory — it is
        // a freshly formatted disk, and refusing one as "not a CP/M disk" is
        // wrong. `looks_like_directory` cannot say so on its own, because a
        // single erased record is equally consistent with a file full of padding;
        // requiring the *whole* directory to be erased is what makes it evidence.
        //
        // Found by testing our own `blank_image()` against our own identifier:
        // a blank we created could not be mounted unless its filename carried a
        // prefix, which `create_blank_image` happens to always add. Rename it and
        // it became unusable.
        if !looks_like_directory(&first) && !is_erased_directory(&dir) {
            continue;
        }
        matching.push((f, directory_is_consistent(&dir, f)));
    }

    match matching.len() {
        0 => Err(unknown_token().unwrap_or(Unknown::NoDirectory { candidates: names })),
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

    /// An image with a short trailer is the same disk, unnamed as well as named.
    ///
    /// **Measured, not supposed.** `DISK13`, `DISK14` and `DISK16` in the widely
    /// circulated Altair set are 337,664 bytes — an `altair8` disk plus exactly
    /// 96 — and all three boot. Mounting used to demand an exact size and so
    /// refused them on the file length before their directory was ever read,
    /// reporting "no known format is 337664 bytes". With the tolerance they
    /// mount read-write and list coherent CP/M 3 and CP/M 2.2 directories.
    ///
    /// The same 96-byte trailer once stopped these disks *booting*, was fixed on
    /// that path, and was left on this one — which is why the number here is
    /// spelled out rather than derived: it is a real quantity from real files.
    #[test]
    fn test_an_unnamed_image_with_a_short_trailer_is_still_the_same_disk() {
        for token in ["ibm3740", "altair8", "altairhd"] {
            let fmt = by_token(token).unwrap();
            let record = fmt.max_bytes() - fmt.min_bytes() + 1;
            for extra in [0, 96.min(record - 1), record - 1] {
                let id = identify("whatever.dsk", fmt.min_bytes() + extra, |f| {
                    (f.token == token).then(|| consistent_dir(f))
                })
                .unwrap_or_else(|e| panic!("{token} +{extra}: {e}"));
                assert_eq!(id.format.token, token);
                assert!(!id.force_read_only(), "{token} +{extra} must stay writable");
            }
            // A whole record more is a different geometry, not a trailer.
            assert!(
                identify("whatever.dsk", fmt.min_bytes() + record, |f| {
                    (f.token == token).then(|| consistent_dir(f))
                })
                .is_err(),
                "{token}: a whole extra record must not be taken as this format"
            );
        }
        // The real file lengths, so the constants above cannot drift away from
        // the disks that motivated them.
        let altair8 = by_token("altair8").unwrap();
        assert_eq!(altair8.min_bytes(), 337_568);
        assert!(
            (altair8.min_bytes()..=altair8.max_bytes()).contains(&337_664),
            "DISK13/14/16 are 337,664 bytes and must be inside the tolerance"
        );
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
        // The *first illegal* block, not a comfortably illegal one. This read
        // `(blocks + 8).min(255)` and cleared the boundary by eight, which is
        // why it went on passing while the check was off by one — see
        // `test_the_last_block_is_legal_and_the_next_one_is_not`.
        let first_bad = Params::derive(fmt).max_block + 1;

        type Mutate = Box<dyn Fn(&mut Vec<u8>)>;
        let cases: Vec<(&str, Mutate)> = vec![
            (
                "a block off the end of the disk",
                Box::new(move |d: &mut Vec<u8>| d[16] = first_bad as u8),
            ),
            (
                "a block inside the directory",
                Box::new(|d: &mut Vec<u8>| d[16] = 1),
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

    /// **The exact boundary, in both directions**, because an off-by-one here is
    /// invisible to every test that clears it by a margin.
    ///
    /// `data_blocks()` is a *count*, so the last legal block is one less than it.
    /// `directory_is_consistent` compared against the count and let an entry
    /// naming block DSM+1 — one past the end of the disk — through; the mount
    /// path caught it afterwards with `params.max_block` and forced read-only,
    /// so the two checks that are supposed to be one check disagreed by exactly
    /// one block. A test that asserts only "some too-large block fails" cannot
    /// see that. This one asserts the pair.
    #[test]
    fn test_the_last_block_is_legal_and_the_next_one_is_not() {
        // A format whose DSM fits in a byte, so the boundary can be written into
        // an 8-bit allocation map at all.
        let fmt = by_token("ibm3740").unwrap();
        let params = Params::derive(fmt);
        assert!(!params.wide_blocks, "this test needs 8-bit block numbers");
        assert!(params.max_block < 255, "the boundary must be representable in one byte");

        let with_block = |b: u16| {
            let mut dir = consistent_dir(fmt);
            dir[16] = b as u8;
            directory_is_consistent(&dir, fmt)
        };

        assert_eq!(with_block(params.max_block), Ok(()), "the last block on the disk is legal");
        assert_eq!(
            with_block(params.max_block + 1),
            Err("an allocation block off the end of the disk"),
            "one past the last block is off the end"
        );
    }

    /// **Identify and the mount must decide the allocation-map width the same
    /// way**, or they read entirely different block numbers out of the same
    /// sixteen bytes.
    ///
    /// CP/M's rule is `DSM > 255`. This function used to apply it to the block
    /// *count* instead, which agrees everywhere except at exactly 256 blocks —
    /// where identify would decode eight 16-bit numbers from bytes the mount
    /// reads as sixteen 8-bit ones, and then pronounce the result consistent
    /// enough to write to. No format in the table has 256 blocks, so the
    /// disagreement was dormant; asserted here against the rule rather than
    /// against the table, so adding such a format cannot wake it up.
    #[test]
    fn test_identify_and_the_mount_agree_on_allocation_width() {
        for fmt in FORMATS {
            let params = Params::derive(fmt);
            assert_eq!(
                params.wide_blocks,
                params.max_block > 255,
                "{}: the width must follow DSM, not the block count",
                fmt.token
            );
            // And the count-based rule that used to be here is the one that
            // differs — at 256 blocks exactly, which is what the pair pins.
            assert_eq!(
                params.max_block as u32,
                fmt.data_blocks().saturating_sub(1),
                "{}: DSM is one less than the block count",
                fmt.token
            );
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

    /// **A disk with more than 255 blocks uses 16-bit allocation entries, and
    /// the halves must not be swapped.**
    ///
    /// `altairhd` is such a disk — 1215 blocks — and getting the endianness
    /// wrong would not merely fail the check: it would pass for small block
    /// numbers (whose high byte is zero) and quietly mis-read large ones, which
    /// is the shape of fault that corrupts a hard disk rather than refusing it.
    /// Asserted deliberately rather than left to the all-formats test above,
    /// where the wide path was only being exercised by accident.
    #[test]
    fn test_wide_allocation_entries_are_little_endian() {
        let fmt = by_token("altairhd").unwrap();
        let blocks = fmt.data_blocks();
        assert!(blocks > 255, "altairhd must be a 16-bit-allocation disk to test this");

        // Block 0x0102 = 258, written low byte first. Read big-endian it would
        // be 0x0201 = 513 — also in range, so only the *value* distinguishes
        // them, which is why this is checked at all.
        let mut dir = vec![0xE5u8; fmt.maxdir as usize * 32];
        let e = &mut dir[..32];
        e[0] = 0;
        e[1..12].copy_from_slice(b"BIG     DAT");
        e[12] = 0;
        e[15] = 1; // one record: needs one block
        e[16..32].fill(0);
        e[16] = 0x02;
        e[17] = 0x01;
        assert_eq!(directory_is_consistent(&dir, fmt), Ok(()), "258 is a legal block");

        // Now a block number that is only out of range when read correctly:
        // 0xFFFF little-endian is 65535, far past 1215. Read as two 8-bit
        // entries it would be 255 and 255 — the second a duplicate, so that
        // reading would also fail, but for the wrong reason. Use a value whose
        // low byte alone is legal to make the distinction sharp.
        let mut dir2 = vec![0xE5u8; fmt.maxdir as usize * 32];
        let e = &mut dir2[..32];
        e[0] = 0;
        e[1..12].copy_from_slice(b"BAD     DAT");
        e[12] = 0;
        e[15] = 1;
        e[16..32].fill(0);
        e[16] = 0x10; // 16 on its own: perfectly legal as an 8-bit block
        e[17] = 0x40; // makes it 0x4010 = 16400, far off the end
        assert_eq!(
            directory_is_consistent(&dir2, fmt),
            Err("an allocation block off the end of the disk"),
            "the high byte must be read, not ignored"
        );
    }

    /// **An ordinary filename with an underscore must not disqualify a disk.**
    ///
    /// Found by this file's own test suite tripping over it: `token_of` calls
    /// anything before the first underscore a format token, so `my_backup.dsk`
    /// and `TDISK03_comal.dsk` were rejected outright — "no format called
    /// 'TDISK03'" — and the disk could not be used at all. That is a worse
    /// outcome than read-only for a user who did nothing wrong, and it is exactly
    /// the case that matters: people drop files in the folder under whatever name
    /// they already have.
    #[test]
    fn test_an_unrecognised_prefix_falls_through_to_inspection() {
        let fmt = by_token("ibm3740").unwrap();
        for name in ["TDISK03_comal.dsk", "my_backup.dsk", "cpm22_2.dsk"] {
            let id = identify(name, fmt.min_bytes(), |f| {
                (f.token == "ibm3740").then(|| consistent_dir(f))
            })
            .unwrap_or_else(|e| panic!("{name} must not be refused for its name: {e}"));
            assert_eq!(id.format.token, "ibm3740", "{name}");
            assert_eq!(id.confidence, Confidence::Verified, "{name}");
        }
    }

    /// But when inspection has nothing to offer either, an unrecognised prefix is
    /// still the most useful thing to say — the user probably did mean a token.
    #[test]
    fn test_an_unrecognised_prefix_is_still_reported_when_inspection_fails() {
        let err = identify("nosuch_disk.dsk", 256_256, |_| None).unwrap_err();
        assert_eq!(err, Unknown::NoSuchFormat("nosuch".to_string()));
        // And a size nothing matches reports the token rather than the size,
        // for the same reason.
        let err = identify("nosuch_disk.dsk", 12_345, |_| None).unwrap_err();
        assert_eq!(err, Unknown::NoSuchFormat("nosuch".to_string()));
    }

    /// **A blank we make ourselves must verify, unnamed.**
    ///
    /// Driven from the real `blank_image()` bytes rather than a hand-built
    /// directory, because the claim being checked is about the two features
    /// agreeing: whatever "format a new disk" writes has to be something
    /// "identify an unnamed disk" then trusts. A hand-built directory would only
    /// prove the checker consistent with my idea of a blank.
    #[test]
    fn test_a_blank_we_created_verifies_without_a_prefix() {
        for f in FORMATS {
            let Some(bytes) = f.blank_image() else { continue };
            let size = bytes.len() as u64;
            let id = identify("blank.dsk", size, |fmt| {
                let mut dir = Vec::new();
                for rec in 0..fmt.dir_records() {
                    let off = fmt.data_record_offset(rec)? as usize;
                    dir.extend_from_slice(bytes.get(off..off + 128)?);
                }
                (!dir.is_empty()).then_some(dir)
            });
            match id {
                Ok(id) => {
                    assert_eq!(id.format.token, f.token, "{}", f.token);
                    assert_eq!(
                        id.confidence,
                        Confidence::Verified,
                        "{}: a blank we formatted must be trusted",
                        f.token
                    );
                    assert!(!id.force_read_only(), "{}: and writable", f.token);
                }
                // An entirely erased directory has no live entry, so the coarse
                // gate can legitimately refuse it — in which case the two
                // features do NOT agree and that is worth knowing explicitly
                // rather than discovering when somebody formats a disk.
                Err(e) => panic!("{}: a blank we created was refused: {e}", f.token),
            }
        }
    }

    /// `E5` means formatted-and-empty; `00` means never written. Only the first
    /// is evidence of a blank CP/M disk, and conflating them would let any file
    /// of zeros the right size be mounted read-write.
    #[test]
    fn test_only_e5_counts_as_a_formatted_blank() {
        assert!(is_erased_directory(&[0xE5u8; 2048]));
        assert!(!is_erased_directory(&[0x00u8; 2048]), "zeros are not a format");
        assert!(!is_erased_directory(&[]), "nothing is not a blank disk");
        // One live entry and the rest erased is not "erased" — it is a disk with
        // a file on it, which takes the ordinary path.
        let mut d = [0xE5u8; 2048];
        d[0] = 0;
        assert!(!is_erased_directory(&d));
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

    /// A file far **larger** than its named format is refused too, and that half
    /// was missing.
    ///
    /// Naming a format skips the directory inspection and mounts read-write, so
    /// the size check is the only thing standing between a mis-named file and a
    /// trusted mount of the wrong geometry. The real numbers: a Cromemco
    /// double-density image is 625,920 bytes and `ibm3740` needs 256,256, so
    /// "big enough" was satisfied more than twice over. It mounted, writable,
    /// reading a double-density disk as a single-density one — and the refusal
    /// message for its size used to *recommend* that very rename.
    #[test]
    fn test_a_file_much_larger_than_its_named_format_is_refused() {
        for (name, size) in [
            ("ibm3740_cdisk02.dsk", 625_920u64),  // Cromemco 8" SSDD
            ("ibm3740_cdisk03.dsk", 1_256_704),   // Cromemco 8" DSDD
            ("altair8_hard.dsk", 4_988_928),      // a hard disk named as a floppy
        ] {
            match identify(name, size, |_| None) {
                Err(Unknown::WrongSize { actual, .. }) => assert_eq!(actual, size),
                other => panic!("{name} must be refused, got {other:?}"),
            }
        }
    }

    /// But a genuine trailer still mounts, which is why the bound is one record
    /// rather than an exact match.
    ///
    /// Images in circulation carry a few bytes past their last record, and on
    /// the boot path a size test that rejected a 96-byte trailer was a real
    /// defect. Anything a whole record or more over is a different geometry.
    #[test]
    fn test_a_short_trailer_is_still_the_same_disk() {
        for f in FORMATS {
            let base = f.min_bytes();
            let record = f.max_bytes() - base + 1;
            for extra in [0, 96.min(record - 1), record - 1] {
                let name = format!("{}_trailered.dsk", f.token);
                assert!(
                    identify(&name, base + extra, |_| None).is_ok(),
                    "{}: a {extra}-byte trailer must still be this disk",
                    f.token
                );
            }
            let name = format!("{}_toolong.dsk", f.token);
            assert!(
                identify(&name, base + record, |_| None).is_err(),
                "{}: a whole extra record is a different geometry",
                f.token
            );
        }
    }

    /// A size nothing mounts is refused, and the message must **not** offer a
    /// rename as the way out.
    ///
    /// This test used to require the opposite — it asserted the message
    /// contained `ibm3740_`, "suggest the convention" — and so it held the
    /// harmful advice in place. A prefix names the layout, not the size, so it
    /// cannot help a file no format is the size of; what it *can* do is skip
    /// the inspection and mount the wrong geometry read-write, which is
    /// precisely what a Cromemco double-density image renamed `ibm3740_*` did.
    /// The honest remedy is the other one: such a disk often still boots.
    #[test]
    fn test_no_format_of_that_size() {
        let err = identify("mystery.dsk", 12_345, |_| None).unwrap_err();
        assert_eq!(err, Unknown::NoMatchingFormat { size: 12_345 });
        let msg = err.to_string();
        assert!(msg.contains("12345"), "say what size it is: {msg:?}");
        assert!(
            !msg.contains("ibm3740_") && !msg.contains("prefix,"),
            "must not recommend a rename that cannot help and can hurt: {msg:?}"
        );
        assert!(msg.contains("boot"), "point at the remedy that exists: {msg:?}");
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
        assert!(err.to_string().contains("probably not a CP/M disk"));
        // And it says what to do instead: these disks very often boot.
        assert!(err.to_string().contains("boot disk"));
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
