//! Global log buffer shared between the server and GUI console.
//!
//! All server output that previously went to `eprintln!` is routed through
//! [`log()`] which writes to stderr and two parallel in-memory ring buffers:
//! a drain-style buffer used by the GUI's per-frame accumulator and a
//! snapshot-style buffer used by the web server (which can be polled
//! without disturbing the GUI's view).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

const MAX_LINES: usize = 2000;

static LOG_BUFFER: OnceLock<Mutex<VecDeque<String>>> = OnceLock::new();
static HISTORY_BUFFER: OnceLock<Mutex<VecDeque<String>>> = OnceLock::new();

// ─── On-disk log: size-bounded, with old generations deleted ──────────

/// What the on-disk log is allowed to do.  Held separately from the open
/// file so the policy can be compared cheaply when the config is re-read.
#[derive(Clone, Debug, PartialEq)]
pub struct FilePolicy {
    /// Active log file.
    pub path: PathBuf,
    /// Rotate once the active file would exceed this many bytes.
    pub max_bytes: u64,
    /// How many rotated generations to keep (`.1` … `.max_files`).  Anything
    /// older is **deleted**, which is what bounds disk use.  Zero keeps no
    /// generations at all: the active file is simply truncated.
    pub max_files: u32,
}

/// The open log plus the byte count that decides when to rotate.
struct FileSink {
    policy: FilePolicy,
    file: std::fs::File,
    written: u64,
}

static FILE_SINK: OnceLock<Mutex<Option<FileSink>>> = OnceLock::new();

fn file_sink() -> &'static Mutex<Option<FileSink>> {
    FILE_SINK.get_or_init(|| Mutex::new(None))
}

/// The hard ceiling on how much disk the log can ever occupy, in KB: the active
/// file plus every kept generation.  Stated in KB because that is the unit the
/// config uses, and exposed so the UIs can show the real total instead of
/// leaving the operator to multiply it out.
pub fn max_disk_kb(max_size_kb: u64, max_files: u32) -> u64 {
    max_size_kb.saturating_mul(max_files as u64 + 1)
}

/// Build the policy a [`crate::config::Config`] describes, or `None` when file
/// logging is switched off.
pub fn file_policy_from(cfg: &crate::config::Config) -> Option<FilePolicy> {
    if !cfg.log_to_file || cfg.log_file.trim().is_empty() {
        return None;
    }
    Some(FilePolicy {
        path: PathBuf::from(cfg.log_file.trim()),
        max_bytes: cfg.log_max_size_kb.saturating_mul(1024),
        max_files: cfg.log_max_files,
    })
}

/// Whether `cfg` actually results in a log file being written.
///
/// The single answer to that question, because `log_to_file` alone is **not**
/// the whole rule: a blank `log_file` is an off-switch too (see
/// [`file_policy_from`]).  Every surface that reports the state — the startup
/// banner, the web hint, the GUI hint — asks this rather than re-deriving it, so
/// none of them can claim a file is being written when none is.
pub fn file_logging_enabled(cfg: &crate::config::Config) -> bool {
    file_policy_from(cfg).is_some()
}

/// Path of rotated generation `n` (1 = most recent).  `foo.log` → `foo.log.1`.
fn rotated_path(base: &Path, n: u32) -> PathBuf {
    let mut s = base.as_os_str().to_os_string();
    s.push(format!(".{}", n));
    PathBuf::from(s)
}

/// Would writing `next_len` more bytes push the active file past its limit?
/// A zero limit means "never rotate on size".  The first line of a fresh file
/// is always written even if it alone exceeds the limit, since rotating an
/// empty file would loop forever.
fn should_rotate(written: u64, next_len: u64, max_bytes: u64) -> bool {
    max_bytes > 0 && written > 0 && written.saturating_add(next_len) > max_bytes
}

/// Shift the generations down and delete the oldest, then reopen `path` empty.
///
/// Errors go to stderr directly, **never** through [`log()`] — this runs while
/// the sink mutex is held, so logging here would deadlock.
fn rotate(policy: &FilePolicy) -> std::io::Result<std::fs::File> {
    if policy.max_files == 0 {
        // Keep no history: truncate in place.
        return open_log(&policy.path, true);
    }
    // The oldest generation is dropped entirely — this is what stops the log
    // growing without bound.
    let oldest = rotated_path(&policy.path, policy.max_files);
    if oldest.exists() {
        if let Err(e) = std::fs::remove_file(&oldest) {
            eprintln!("Log rotation: could not delete {}: {}", oldest.display(), e);
        }
    }
    for n in (1..policy.max_files).rev() {
        let from = rotated_path(&policy.path, n);
        if from.exists() {
            let to = rotated_path(&policy.path, n + 1);
            if let Err(e) = std::fs::rename(&from, &to) {
                eprintln!("Log rotation: could not rename {}: {}", from.display(), e);
            }
        }
    }
    if policy.path.exists() {
        let to = rotated_path(&policy.path, 1);
        if let Err(e) = std::fs::rename(&policy.path, &to) {
            eprintln!(
                "Log rotation: could not rename {}: {}",
                policy.path.display(),
                e
            );
        }
    }
    open_log(&policy.path, true)
}

/// Open (or create) the log.  Owner-only on Unix: log lines name hosts, ports
/// and usernames, the same privacy reasoning as the config and dialup files.
fn open_log(path: &Path, truncate: bool) -> std::io::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true);
    if truncate {
        opts.truncate(true);
    } else {
        opts.append(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}

/// Point file logging at `policy`, or turn it off with `None`.
///
/// Idempotent and safe to call on every config load — the server re-reads its
/// config on each restart cycle.  An unchanged policy keeps the file open
/// (reopening on every restart would be pointless churn); a changed path or
/// limit reopens.
pub fn configure_file_logging(policy: Option<FilePolicy>) {
    let mut slot = file_sink().lock().unwrap_or_else(|e| e.into_inner());
    match policy {
        None => {
            *slot = None;
        }
        Some(p) => {
            if let Some(existing) = slot.as_ref() {
                if existing.policy == p {
                    return; // already logging exactly this way
                }
            }
            // Append rather than truncate: a restart should extend the log, not
            // discard what the previous run recorded.
            match open_log(&p.path, false) {
                Ok(file) => {
                    let written = file.metadata().map(|m| m.len()).unwrap_or(0);
                    *slot = Some(FileSink { policy: p, file, written });
                }
                Err(e) => {
                    // stderr, not log(): we hold the sink mutex.
                    eprintln!(
                        "Warning: could not open log file {}: {} — file logging disabled.",
                        p.path.display(),
                        e
                    );
                    *slot = None;
                }
            }
        }
    }
}

/// Append one line to `sink`, rotating first if it would overflow.
///
/// Takes the sink by reference rather than reaching for the global so the
/// rotation behaviour can be tested against a locally-owned sink.  That is not
/// cosmetic: the global is shared with every other test in the binary, and any
/// one of them calling [`log()`] lands a line in whichever file is armed — which
/// with a small size cap rotates the test's own newest line out from under it.
/// An owned sink has no such coupling.
///
/// Returns `Err` if the line could not be written, in which case the caller is
/// expected to disarm — a sink that cannot be written to is not usable.
fn write_line_to(sink: &mut FileSink, line: &str) -> std::io::Result<()> {
    let bytes = line.len() as u64 + 1; // + newline
    if should_rotate(sink.written, bytes, sink.policy.max_bytes) {
        // Errors go to stderr, never through log(): the caller holds the sink
        // mutex, so logging here would deadlock.
        let f = rotate(&sink.policy).inspect_err(|e| {
            eprintln!("Log rotation failed: {} — file logging disabled.", e);
        })?;
        sink.file = f;
        sink.written = 0;
    }
    // No fsync: one write syscall per line is cheap, but flushing to the SD
    // card on a Pi for every verbose protocol line would not be.  The
    // in-memory rings and stderr/journald cover a hard crash.
    use std::io::Write;
    sink.file
        .write_all(line.as_bytes())
        .and_then(|()| sink.file.write_all(b"\n"))
        .inspect_err(|e| {
            eprintln!("Log write failed: {} — file logging disabled.", e);
        })?;
    sink.written = sink.written.saturating_add(bytes);
    Ok(())
}

/// Append one line to the process-wide on-disk log, if one is armed.
fn write_to_file(line: &str) {
    let mut slot = file_sink().lock().unwrap_or_else(|e| e.into_inner());
    let Some(sink) = slot.as_mut() else { return };
    if write_line_to(sink, line).is_err() {
        *slot = None;
    }
}

// Deliberately NO rate limiting.  It was considered and rejected: verbose mode
// logs per protocol block, which can legitimately exceed any sane
// lines-per-second ceiling during a fast transfer, so a limiter would discard
// exactly the lines an operator turned verbose on to see.  Growth is bounded by
// rotation and generation deletion instead, which loses old data rather than
// current data.

/// Initialise the global log buffers.  Safe to call more than once.
pub fn init() {
    LOG_BUFFER.get_or_init(|| Mutex::new(VecDeque::with_capacity(MAX_LINES)));
    HISTORY_BUFFER.get_or_init(|| Mutex::new(VecDeque::with_capacity(MAX_LINES)));
}

/// Log a message to stderr and append it to both shared buffers.  The
/// drain buffer feeds the GUI's per-frame console accumulator; the
/// history buffer is a non-draining ring that lets the web-config
/// console poll for recent lines without competing with the GUI.
pub fn log(msg: String) {
    eprintln!("{}", msg);
    // Before the rings, so a line is on disk even if a ring lock is contended.
    write_to_file(&msg);
    // Recover from a poisoned lock rather than dropping the line: if a
    // thread panicked while holding the log mutex, logging is exactly when
    // we most want it to keep working.  Matches config.rs / gui.rs.
    if let Some(buf) = LOG_BUFFER.get() {
        let mut buf = buf.lock().unwrap_or_else(|e| e.into_inner());
        buf.push_back(msg.clone());
        while buf.len() > MAX_LINES {
            buf.pop_front();
        }
    }
    if let Some(buf) = HISTORY_BUFFER.get() {
        let mut buf = buf.lock().unwrap_or_else(|e| e.into_inner());
        buf.push_back(msg);
        while buf.len() > MAX_LINES {
            buf.pop_front();
        }
    }
}

/// Drain all buffered log lines (used by the GUI console each frame).
pub fn drain() -> Vec<String> {
    if let Some(buf) = LOG_BUFFER.get() {
        let mut buf = buf.lock().unwrap_or_else(|e| e.into_inner());
        return buf.drain(..).collect();
    }
    Vec::new()
}

/// Return a snapshot of the most recent log lines without removing
/// them from the history buffer.  Used by the web-config console
/// poller (the GUI's accumulator continues to drain its own buffer).
pub fn snapshot(max: usize) -> Vec<String> {
    if let Some(buf) = HISTORY_BUFFER.get() {
        let buf = buf.lock().unwrap_or_else(|e| e.into_inner());
        let len = buf.len();
        let skip = len.saturating_sub(max);
        return buf.iter().skip(skip).cloned().collect();
    }
    Vec::new()
}

/// Convenience macro that replaces `eprintln!`.
macro_rules! glog {
    () => { $crate::logger::log(String::new()) };
    ($($arg:tt)*) => { $crate::logger::log(format!($($arg)*)) };
}
pub(crate) use glog;

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialises the tests that arm the process-global [`FILE_SINK`], and
    /// disarms it from `Drop`.
    ///
    /// Required by any test that calls `configure_file_logging`, because that
    /// replaces one process-wide sink: two such tests each pointing it at their
    /// own temp file corrupt each other, one test's writes landing in the
    /// other's file while its rotation sequence stops advancing. That was
    /// observed, not theorised — `left: ["t.log", "t.log.2"]`, the `.1`
    /// generation missing, within a minute of stressing `logger::tests` with 4
    /// threads.
    ///
    /// **`Drop` is the important half.** A failed assertion returns early, so a
    /// trailing `configure_file_logging(None)` is skipped exactly when it
    /// matters most: the sink would stay armed at a temp directory the test then
    /// deletes, and every later `log()` in the suite would hit a write error.
    /// Same reasoning as `ConfigTestGuard` — cleanup belongs inside the critical
    /// section, not after it.
    ///
    /// Note the lock alone is **not** sufficient protection for a test that
    /// asserts on file *contents* under a size cap: nothing serialises the
    /// ~1650 tests that merely call `log()`, and each of those writes into
    /// whichever sink is armed. Such a test should own its sink and drive
    /// [`write_line_to`] directly, as
    /// `test_rotation_bounds_disk_and_deletes_oldest` does.
    struct FileLogTestGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl FileLogTestGuard {
        fn new() -> Self {
            static FILE_LOG_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
            let lock = FILE_LOG_TEST_LOCK
                .get_or_init(|| Mutex::new(()))
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            // Start from a known state: a previous panic cannot leak its sink
            // into this test even though Drop normally clears it.
            configure_file_logging(None);
            Self { _lock: lock }
        }
    }

    impl Drop for FileLogTestGuard {
        fn drop(&mut self) {
            configure_file_logging(None);
        }
    }

    /// Log a couple of unique sentinel strings, then verify
    /// `snapshot()` finds them.  The buffer is a global singleton
    /// shared across the test binary, so we look for our sentinels
    /// rather than asserting an exact count — other tests may have
    /// logged between the calls below.
    #[test]
    fn test_snapshot_contains_recent_log_lines() {
        init();
        let sentinel = format!("snapshot_sentinel_{}_{}", std::process::id(), 7919);
        log(sentinel.clone());
        let snap = snapshot(MAX_LINES);
        assert!(
            snap.iter().any(|l| l == &sentinel),
            "snapshot did not include the just-logged sentinel"
        );
    }

    /// `snapshot()` must not drain the history buffer — two
    /// back-to-back calls must both see the sentinel.  The drain
    /// vs. history separation is the whole reason `snapshot()`
    /// exists; this guards against accidentally swapping it back
    /// to a draining read.
    #[test]
    fn test_snapshot_is_non_draining() {
        init();
        let sentinel = format!("nondrain_sentinel_{}_{}", std::process::id(), 8161);
        log(sentinel.clone());
        let first = snapshot(MAX_LINES);
        let second = snapshot(MAX_LINES);
        assert!(first.iter().any(|l| l == &sentinel));
        assert!(second.iter().any(|l| l == &sentinel));
    }

    /// `snapshot(max)` returns at most `max` lines — verifies the
    /// tail-trimming logic so the web `/logs` endpoint can bound its
    /// response size regardless of how full the buffer is.
    #[test]
    fn test_snapshot_respects_max_cap() {
        init();
        for i in 0..50 {
            log(format!("snapshot_cap_{}_{}", std::process::id(), i));
        }
        let snap = snapshot(8);
        assert!(snap.len() <= 8, "snapshot returned {} > cap of 8", snap.len());
    }

    // ─── On-disk log: size bound + generation deletion ───────────────

    #[test]
    fn test_rotated_path_appends_generation() {
        assert_eq!(
            rotated_path(Path::new("ethernetgateway.log"), 1),
            PathBuf::from("ethernetgateway.log.1")
        );
        assert_eq!(
            rotated_path(Path::new("/var/log/eg.log"), 5),
            PathBuf::from("/var/log/eg.log.5")
        );
    }

    /// The disk bound Ricky asked for, stated once: active file + generations.
    #[test]
    fn test_max_disk_kb_counts_the_active_file_too() {
        assert_eq!(max_disk_kb(1024, 5), 6144); // the shipped default: 6 MB
        assert_eq!(max_disk_kb(1024, 0), 1024); // no generations kept
        // Saturating, so a nonsense config can't wrap to a small number and
        // silently promise a bound it doesn't enforce.
        assert_eq!(max_disk_kb(u64::MAX, 4), u64::MAX);
    }

    #[test]
    fn test_should_rotate_boundaries() {
        // Under the limit: keep writing.
        assert!(!should_rotate(100, 10, 1000));
        // Exactly at the limit is still fine; past it rotates.
        assert!(!should_rotate(990, 10, 1000));
        assert!(should_rotate(991, 10, 1000));
        // A zero limit disables size-based rotation.
        assert!(!should_rotate(u64::MAX / 2, 10, 0));
        // An empty file never rotates, even for an oversized line — otherwise a
        // line bigger than the limit would rotate forever and never be written.
        assert!(!should_rotate(0, 99_999, 10));
    }

    /// End to end: the active file rotates, generations shift, and the oldest
    /// is **deleted** so the log cannot grow without bound.
    ///
    /// Drives a **locally-owned** sink rather than arming the global one. That
    /// is deliberate and was arrived at the hard way: with the global sink, every
    /// other test in the binary that calls `log()` writes into this test's file,
    /// and against a 200-byte cap those foreign lines rotate `line 059` out of
    /// the active file. Serialising the sink-arming tests was not enough —
    /// nothing serialises the ~1650 tests that merely log. An owned sink removes
    /// the shared state instead of racing it.
    #[test]
    fn test_rotation_bounds_disk_and_deletes_oldest() {
        let dir = std::env::temp_dir().join(format!("eg_log_rot_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let base = dir.join("t.log");

        // 200-byte cap, keep 2 generations => at most 3 files on disk.
        let policy = FilePolicy { path: base.clone(), max_bytes: 200, max_files: 2 };
        let mut sink = FileSink {
            file: open_log(&policy.path, false).unwrap(),
            written: 0,
            policy,
        };

        // Each line is ~40 bytes, so this rotates several times over.
        for i in 0..60 {
            write_line_to(&mut sink, &format!("line {i:03} ------------------------------")).unwrap();
        }

        let mut present: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        present.sort();
        // Active + .1 + .2 and nothing older: .3 must have been deleted.
        assert_eq!(
            present,
            vec!["t.log".to_string(), "t.log.1".into(), "t.log.2".into()],
            "expected exactly the active file plus 2 generations"
        );

        let total: u64 = present
            .iter()
            .map(|n| std::fs::metadata(dir.join(n)).unwrap().len())
            .sum();
        // The bound is per-file, so the total is under (max_files + 1) * cap
        // plus one final line's slack on the active file.
        assert!(
            total <= 200 * 3 + 64,
            "total on-disk log grew to {total} bytes, past the bound"
        );

        // The newest data must be in the ACTIVE file — rotation must not leave
        // the current line stranded in a generation.
        let active = std::fs::read_to_string(&base).unwrap();
        assert!(active.contains("line 059"), "active log missing newest line");

        drop(sink);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Disabling file logging must actually stop writing, and re-enabling must
    /// *append* rather than discard what a previous run recorded.
    #[test]
    fn test_configure_off_then_on_appends() {
        let _guard = FileLogTestGuard::new();
        let dir = std::env::temp_dir().join(format!("eg_log_cfg_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let base = dir.join("a.log");
        let policy = || FilePolicy { path: base.clone(), max_bytes: 0, max_files: 3 };

        configure_file_logging(Some(policy()));
        write_to_file("first");
        configure_file_logging(None);
        write_to_file("must not appear");
        configure_file_logging(Some(policy()));
        write_to_file("second");
        configure_file_logging(None);

        let body = std::fs::read_to_string(&base).unwrap();
        assert!(body.contains("first") && body.contains("second"), "got {body:?}");
        assert!(
            !body.contains("must not appear"),
            "wrote while file logging was off: {body:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `file_policy_from` is the one place config turns into policy, so the
    /// off-switch and the KB->bytes conversion are pinned here.
    #[test]
    fn test_file_policy_from_config() {
        let mut cfg = crate::config::Config::default();
        // Shipped default is ON, so an untouched config logs to a file.
        assert!(cfg.log_to_file, "file logging should ship enabled");
        let p = file_policy_from(&cfg).expect("default config must yield a policy");
        assert_eq!(p.max_bytes, cfg.log_max_size_kb * 1024, "KB must become bytes");
        assert_eq!(p.max_files, cfg.log_max_files);

        cfg.log_to_file = false;
        assert!(file_policy_from(&cfg).is_none(), "disabled must yield no policy");

        // A blank path is treated as off rather than creating a file named "".
        cfg.log_to_file = true;
        cfg.log_file = "   ".into();
        assert!(file_policy_from(&cfg).is_none(), "blank path must yield no policy");
    }

    /// `file_logging_enabled` must agree with `file_policy_from` on every state.
    /// Four surfaces report the on/off state from it — the startup banner and the
    /// telnet / web / GUI screens — and they each used to re-derive the rule,
    /// which is how the banner came to print "Logging to " for a blank path while
    /// nothing was being written.
    #[test]
    fn test_file_logging_enabled_matches_the_policy() {
        let base = crate::config::Config::default();
        for (to_file, path, want) in [
            (true, "eg.log", true),
            (false, "eg.log", false),
            (true, "", false),   // blank path is an off-switch of its own
            (true, "   ", false), // ...including whitespace-only
            (false, "", false),
        ] {
            let cfg = crate::config::Config {
                log_to_file: to_file,
                log_file: path.into(),
                ..base.clone()
            };
            assert_eq!(
                file_logging_enabled(&cfg),
                want,
                "log_to_file={to_file}, log_file={path:?}"
            );
            assert_eq!(
                file_logging_enabled(&cfg),
                file_policy_from(&cfg).is_some(),
                "the predicate and the policy must never disagree"
            );
        }
    }

    /// `drain()` and `snapshot()` are independent — the GUI's
    /// per-frame drain must not remove lines from the web's
    /// history view.  Log a sentinel, drain (which clears the
    /// GUI buffer), then assert the sentinel is still in the
    /// snapshot.
    #[test]
    fn test_drain_does_not_affect_snapshot() {
        init();
        let sentinel = format!("drain_isolation_{}_{}", std::process::id(), 5051);
        log(sentinel.clone());
        let _ = drain();
        let snap = snapshot(MAX_LINES);
        assert!(
            snap.iter().any(|l| l == &sentinel),
            "drain() removed a line from snapshot's history buffer"
        );
    }
}
