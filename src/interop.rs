//! Finding the live interop harness binaries.  Tests only.
//!
//! **Why this exists: an env-gated test that skips by early return is the
//! quietest way a suite can lie.**  The CCGMS gates asked for
//! `CCGMS_{SEND,RECV,XFER}_BIN`, printed "not set; skipping" to a stderr nobody
//! sees (cargo hides a passing test's output) and returned `Ok`.  Reviewed on
//! 2026-08-24 they had done nothing for an unknown length of time while
//! counting as six passes, with the compiled harness sitting built on the same
//! machine the whole while -- nobody had exported the variables.
//!
//! Three rules follow from that, and they are the whole of this module:
//!
//! * **Look where the harness actually lives.**  Its own README builds it in one
//!   place; probing there means the gates run whenever they *can* run, so
//!   forgetting an export is no longer a silent loss of coverage.
//! * **A stated path that is not there is an error, not a skip.**  If someone
//!   sets the variable, they have declared an intent to run the gate; a typo
//!   must not be answered with a pass.
//! * **Absent is the only skip**, which is the honest CI case -- no harness can
//!   be built there.  Every gate using this is `#[ignore]`d as well, so an
//!   ordinary run reports it in the `ignored` count rather than folding it into
//!   `passed`.

use std::path::PathBuf;

/// Where `~/claude/punter-ccgms-interop/README.md` builds the three binaries.
/// Relative to `$HOME`, so it is the same string on every machine that follows
/// that README.
const CONVENTIONAL_DIR: &str = "claude/punter-ccgms-interop";

/// The harness binary for `env_var`, or `None` if there is genuinely none here.
///
/// `name` is the filename the README builds, used when the variable is unset.
/// Panics when the variable *is* set and names something that is not a file --
/// see the module comment for why that is not a skip.
pub fn harness_bin(env_var: &str, name: &str) -> Option<PathBuf> {
    resolve(env_var, std::env::var(env_var).ok(), name)
}

/// The rule itself, with the environment passed in.
///
/// Split out so the "a stated path that is missing is an error" case can be
/// tested **without mutating the environment**. `set_var` is `unsafe` in Rust
/// 2024 because a process-wide write races every other thread, and this suite
/// runs tests in parallel -- so a test that set a variable to check this would
/// be trading a real soundness hazard for coverage of one branch.
fn resolve(env_var: &str, stated: Option<String>, name: &str) -> Option<PathBuf> {
    if let Some(stated) = stated {
        let path = PathBuf::from(&stated);
        assert!(
            path.is_file(),
            "{env_var} is set to {stated:?}, which is not a file.  Setting it \
             declares an intent to run this gate, so a path that is not there \
             is a broken harness rather than a reason to skip -- answering it \
             with a pass is how a gate reports coverage it never had.",
        );
        return Some(path);
    }
    // Unset: fall back to where the harness is built, so a forgotten export
    // costs nothing.  Absent there too means this machine has no harness.
    let home = std::env::var_os("HOME")?;
    let path = PathBuf::from(home).join(CONVENTIONAL_DIR).join(name);
    path.is_file().then_some(path)
}

/// Announce that a gate is standing down because its harness is not here.
///
/// **One wording, so the guard below can find every one of them.** The first
/// version of that scan matched the bare phrase "; skipping" and hit a YMODEM
/// *protocol* log line in production code -- a marker generic enough to collide
/// with real logging cannot police anything. A named call cannot collide.
///
/// Note what does *not* belong here: the lrzsz gates `panic!` when `sz` is
/// missing, because those are run deliberately and a missing tool means the run
/// did not happen. Skipping is only right where a harness genuinely cannot exist
/// on the machine, which is the CI case.
pub fn skipping(what: &str) {
    eprintln!("interop gate skipped: no {what} on this machine");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stated path that is not there fails rather than skipping.  This is the
    /// rule the module exists for, so it is the one worth a test.
    #[test]
    fn test_a_stated_harness_path_that_is_missing_is_an_error() {
        let stated = Some("/nonexistent/harness/binary".to_string());
        let caught = std::panic::catch_unwind(|| {
            resolve("CCGMS_RECV_BIN", stated.clone(), "ccgms-recv")
        });
        assert!(
            caught.is_err(),
            "a harness path that does not exist must fail the gate, not skip it",
        );
    }

    /// Nothing stated and nothing at the conventional path is the CI case:
    /// `None`, no panic.  The positive control for the rule above -- a version
    /// that panicked here would turn every CI run red, which is a far worse
    /// failure than the one being guarded against.
    #[test]
    fn test_no_harness_anywhere_is_a_skip_not_a_failure() {
        assert!(
            resolve("CCGMS_RECV_BIN", None, "no-such-harness-binary-exists").is_none(),
            "with no harness present this must answer None so CI stays green",
        );
    }

    /// A stated path that *does* exist is used as given, whatever is sitting at
    /// the conventional location -- an explicit choice outranks the fallback.
    #[test]
    fn test_a_stated_harness_path_that_exists_wins() {
        // Any file will do; this one is guaranteed to be here.
        let me = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");
        assert_eq!(
            resolve("CCGMS_RECV_BIN", Some(me.to_string()), "ccgms-recv").as_deref(),
            Some(std::path::Path::new(me)),
        );
    }

    /// **Every gate that can skip is `#[ignore]`d.**
    ///
    /// This is the invariant whose absence caused the whole problem.  A test
    /// that early-returns when its harness is missing looks identical to one
    /// that ran: cargo hides a passing test's output, so the "skipping" line
    /// goes nowhere and the gate is counted in `passed`.  `#[ignore]` is what
    /// makes the difference visible -- an ordinary run reports it under
    /// `ignored`, and `--ignored` is what deliberately selects it, which is
    /// also how `tools/cpm-live-gates` finds every gate.
    ///
    /// Six CCGMS gates were missing it, so `--ignored` never selected them and
    /// an ordinary run counted them as passes.  Scanned rather than remembered,
    /// because the next interop test will be written by someone who never read
    /// this.
    #[test]
    fn test_every_skippable_gate_is_ignored() {
        let sources: &[(&str, &str)] = &[
            ("punter.rs", include_str!("punter.rs")),
            ("xmodem.rs", include_str!("xmodem.rs")),
            ("zmodem.rs", include_str!("zmodem.rs")),
            ("kermit.rs", include_str!("kermit.rs")),
        ];
        let mut checked = 0usize;
        for (name, src) in sources {
            let lines: Vec<&str> = src.lines().collect();
            for (i, line) in lines.iter().enumerate() {
                // Comments stripped, or this scan reads the prose above it --
                // which names the call it is looking for.
                let code = match line.find("//") {
                    Some(c) => &line[..c],
                    None => line,
                };
                if !code.contains("interop::skipping(") {
                    continue;
                }
                // Walk back to the enclosing fn ...
                let mut f = i;
                while f > 0 && !lines[f].trim_start().starts_with("fn ")
                    && !lines[f].contains(" fn ")
                {
                    f -= 1;
                }
                // ... then over the attributes stacked above it.
                let mut a = f;
                while a > 0 {
                    let prev = lines[a - 1].trim_start();
                    if prev.starts_with("#[") || prev.starts_with("///") || prev.is_empty() {
                        a -= 1;
                    } else {
                        break;
                    }
                }
                let attrs = lines[a..f].join("\n");
                assert!(
                    attrs.contains("#[ignore]"),
                    "{name}:{} can skip when its harness is absent but is not \
                     #[ignore]d, so `--ignored` never selects it and an ordinary \
                     run counts it as a pass:\n  {}",
                    f + 1,
                    lines[f].trim(),
                );
                checked += 1;
            }
        }
        // A scan that matches nothing passes vacuously, and the marker it looks
        // for is an ordinary string somebody could reword.
        assert!(
            checked >= 8,
            "only {checked} skippable gates found -- the scan has stopped \
             matching the skip idiom it is meant to police",
        );
    }

}
