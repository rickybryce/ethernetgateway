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
use std::time::{Duration, Instant};

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

/// What the on-disk log is doing right now.
///
/// The `Paused` state is the whole point of this enum.  A write error used to
/// throw the sink away, which meant one full disk, one unplugged USB stick or one
/// momentary NFS hiccup silently stopped file logging **for the life of the
/// process** — the operator only found out later, from a log that stops
/// mid-sentence.  Keeping the policy lets logging re-arm itself once the
/// underlying problem clears.
enum Sink {
    /// No log file wanted (or none could ever be opened).
    Off,
    /// Writing normally.
    Armed(FileSink),
    /// A write (or the initial open) failed.  The file is closed, but the policy
    /// is remembered so it can be reopened at `retry_at`.  `dropped` counts the
    /// lines that did not reach disk, and is reported in the file itself when it
    /// comes back, so the gap is visible to whoever reads the log later.
    Paused {
        policy: FilePolicy,
        retry_at: Instant,
        backoff: Duration,
        dropped: u64,
    },
}

impl Sink {
    /// The policy currently in force, whether armed or paused.  Used by
    /// [`configure_file_logging`] so a re-configure with unchanged settings does
    /// not reset a pause's backoff.
    fn policy(&self) -> Option<&FilePolicy> {
        match self {
            Sink::Off => None,
            Sink::Armed(s) => Some(&s.policy),
            Sink::Paused { policy, .. } => Some(policy),
        }
    }
}

/// How long to wait before the first attempt to reopen a failed log, and the
/// ceiling the delay doubles up to.
///
/// Deliberately not a per-line retry: on a full disk that would mean a failed
/// `write` syscall (and a stderr line) for every message the gateway emits, which
/// is the reason retrying was rejected the first time round.  A doubling backoff
/// costs at most one reopen attempt every 30 s at first and every 5 min once a
/// failure looks permanent, while still recovering within half a minute from the
/// transient case that actually happens.
const RETRY_BACKOFF_START: Duration = Duration::from_secs(30);
const RETRY_BACKOFF_MAX: Duration = Duration::from_secs(300);

/// The next delay in the backoff sequence: double, capped.  Pure, so the
/// sequence is testable without waiting on a clock.
fn next_backoff(current: Duration) -> Duration {
    current.saturating_mul(2).min(RETRY_BACKOFF_MAX)
}

static FILE_SINK: OnceLock<Mutex<Sink>> = OnceLock::new();

fn file_sink() -> &'static Mutex<Sink> {
    FILE_SINK.get_or_init(|| Mutex::new(Sink::Off))
}

/// Lines logged before the config was read, held so they can still reach the
/// file once one is armed.
///
/// The log path lives in the config, so nothing can be written to disk until the
/// config has been loaded — but the version banner and every
/// `load_or_create_config` diagnostic (including the FATAL "exists but could not
/// be read" refusal) happen *before* that. Without this the file always began
/// mid-story, missing exactly the startup diagnostics an operator reading a log
/// after the fact wants most.
///
/// Collection stops at the first [`configure_file_logging`] call — that is what
/// "pre-arm" means, and it is also when we learn whether a file was wanted at
/// all. If one was, the backlog is written into it in order; either way the
/// buffer is then dropped, so an operator who runs with file logging off does not
/// accumulate lines nothing will ever read.
static PREARM_BACKLOG: OnceLock<Mutex<Option<VecDeque<String>>>> = OnceLock::new();

/// Cap on the pre-arm backlog.  Startup is a few dozen lines; this is generous
/// enough to cover a pathological config-migration run while still bounding the
/// memory a never-armed process holds.  Oldest lines are dropped first — the
/// newest are the ones nearest the failure being diagnosed.
const MAX_PREARM_LINES: usize = 500;

fn prearm_backlog() -> &'static Mutex<Option<VecDeque<String>>> {
    PREARM_BACKLOG.get_or_init(|| Mutex::new(Some(VecDeque::new())))
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

/// Whether the log file is wanted but **not currently open** -- the sink is
/// paused, retrying after a failed open or write.
///
/// **A policy being on is not a file being open, and the startup banner needs
/// the second question.** [`file_logging_enabled`] answers the first, which is
/// what the config surfaces want ("should there be a log file"). The banner
/// asked it too and announced the path unconditionally, so a launch in a
/// directory it could not write printed two contradictory lines in a row --
/// `Warning: could not open log file ... Permission denied` and then `Logging
/// to ...` (measured 2026-08-20). The warning is the true one.
pub fn file_logging_is_paused() -> bool {
    matches!(&*file_sink().lock().unwrap_or_else(|e| e.into_inner()), Sink::Paused { .. })
}

/// One-line description of what the current log settings will do on disk.
///
/// Shown under the log controls in the web and GUI config panels.  Shared rather
/// than written once per surface: the two copies this replaced had already
/// drifted ("the console above only" vs "the console only"), and "above" was
/// wrong in a popup anyway — the console pane is behind it, not above it.
///
/// `size_kb`/`files` are passed in rather than read from `cfg` because the GUI
/// edits them as text: its fields sync on the following frame, so reading `cfg`
/// there would show a figure one keystroke stale.  The on/off decision still
/// comes from [`file_logging_enabled`], so it cannot disagree with the rest.
pub fn log_state_hint(cfg: &crate::config::Config, size_kb: u64, files: u32) -> String {
    if !file_logging_enabled(cfg) {
        "Logging to stderr and the console only.".to_string()
    } else if size_kb == 0 {
        "No size limit \u{2014} this file can grow without bound.".to_string()
    } else {
        format!(
            "At most {} KB on disk ({} plus {} rotated; the oldest is deleted).",
            max_disk_kb(size_kb, files),
            cfg.log_file.trim(),
            files,
        )
    }
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
/// **A failed rename aborts the rotation** rather than carrying on to truncate.
/// Truncating after a rename failure destroyed the very content the rename was
/// supposed to preserve — a read-only directory, a cross-device `.1`, or a
/// generation held open by another process turned "rotate the log" into "delete
/// the log".  Returning the error instead pauses file logging (see [`Sink`]),
/// which loses new lines for a while but never loses lines already on disk.
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
                // Abort: the next rename in the sequence would overwrite the
                // generation this one failed to move out of the way.
                eprintln!(
                    "Log rotation: could not rename {}: {} — rotation abandoned, the \
                     current log is left intact.",
                    from.display(),
                    e
                );
                return Err(e);
            }
        }
    }
    if policy.path.exists() {
        let to = rotated_path(&policy.path, 1);
        if let Err(e) = std::fs::rename(&policy.path, &to) {
            eprintln!(
                "Log rotation: could not rename {}: {} — keeping the current log rather \
                 than truncating it.",
                policy.path.display(),
                e
            );
            return Err(e);
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
            *slot = Sink::Off;
        }
        Some(p) => {
            if slot.policy() == Some(&p) {
                // Already logging exactly this way (or already retrying exactly
                // this way — a re-configure must not reset a pause's backoff, or
                // a config-saving operator would drive a full disk's retries
                // back down to every 30 s).  Retire the backlog anyway: this is
                // still a configure call, so the pre-arm window is over and
                // anything held has already been written by the call that opened
                // this sink.
                retire_prearm_backlog(&mut slot);
                return;
            }
            // Append rather than truncate: a restart should extend the log, not
            // discard what the previous run recorded.
            match open_log(&p.path, false) {
                Ok(file) => {
                    let written = file.metadata().map(|m| m.len()).unwrap_or(0);
                    *slot = Sink::Armed(FileSink { policy: p, file, written });
                }
                Err(e) => {
                    // stderr, not log(): we hold the sink mutex.  Paused rather
                    // than off — a log directory that does not exist yet, or a
                    // volume that has not finished mounting at boot, is exactly
                    // the case that fixes itself a minute later.
                    eprintln!(
                        "Warning: could not open log file {}: {} — retrying every {}s.",
                        p.path.display(),
                        e,
                        RETRY_BACKOFF_START.as_secs()
                    );
                    *slot = Sink::Paused {
                        policy: p,
                        retry_at: Instant::now() + RETRY_BACKOFF_START,
                        backoff: RETRY_BACKOFF_START,
                        dropped: 0,
                    };
                }
            }
        }
    }
    // Whatever was decided, the pre-arm window closes here: flush the backlog
    // into the file if we have one, then drop it.
    retire_prearm_backlog(&mut slot);
}

/// Write any pre-arm backlog into `sink` (if there is one) and stop collecting.
///
/// Called with the sink lock held, so the lock order is FILE_SINK → BACKLOG.
/// [`log()`] never holds both at once — it finishes with the sink before touching
/// the backlog — so there is no cycle and no deadlock.
fn retire_prearm_backlog(slot: &mut Sink) {
    let mut held = prearm_backlog().lock().unwrap_or_else(|e| e.into_inner());
    // `None` means the window already closed; a second configure call is a no-op.
    let Some(lines) = held.take() else { return };
    // A paused sink has no open file, so the backlog is dropped rather than
    // queued: those lines are already on stderr, and holding them for a retry
    // that may never come is the unbounded buffer this cap exists to avoid.
    if let Sink::Armed(sink) = slot {
        drain_backlog_into(lines, sink);
    }
}

/// Write `lines` into `sink`, oldest first.
///
/// Split out from [`retire_prearm_backlog`] so it can be tested against an owned
/// sink and an owned deque — no process-global state, so no ordering dependence
/// on the rest of the suite (the same reasoning as [`write_line_to`]).
fn drain_backlog_into(lines: VecDeque<String>, sink: &mut FileSink) {
    for line in lines {
        // Reported once, then stop — repeating it per backlogged line would
        // bury it.  The next ordinary log() call pauses the sink and tells the
        // operator; this only explains the startup lines that did not make it.
        if let Err(e) = write_line_to(sink, &line) {
            eprintln!("Log write failed while writing the startup backlog: {}", e);
            break;
        }
    }
}

/// Push `line` onto `lines`, dropping the oldest beyond `cap`.
///
/// The newest lines are kept because they are the ones nearest whatever failure
/// is being diagnosed.  Free of globals so the bound is directly testable.
fn push_bounded(lines: &mut VecDeque<String>, line: &str, cap: usize) {
    lines.push_back(line.to_string());
    while lines.len() > cap {
        lines.pop_front();
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
/// expected to pause the sink — a file that cannot be written to is not usable
/// right now, though it may well be again shortly (see [`Sink::Paused`]).
fn write_line_to(sink: &mut FileSink, line: &str) -> std::io::Result<()> {
    let bytes = line.len() as u64 + 1; // + newline
    if should_rotate(sink.written, bytes, sink.policy.max_bytes) {
        // Errors go to stderr, never through log(): the caller holds the sink
        // mutex, so logging here would deadlock.  `rotate` has already said which
        // step failed and that nothing was truncated.
        let f = rotate(&sink.policy)?;
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
        // No message here: each caller reports it with the context that makes
        // it useful — write_to_file_at turns it into the operator-facing pause
        // notice (which reaches the console panes, where a bare stderr line
        // would not), and the backlog drain says which stage failed.
        ?;
    sink.written = sink.written.saturating_add(bytes);
    Ok(())
}

/// Append one line to the process-wide on-disk log, if one is armed.
///
/// Reports whether the line reached a file, so [`log()`] can hold it in the
/// pre-arm backlog instead, plus any state-change notice for [`log()`] to emit
/// once the lock is gone.  The sink lock is released before returning,
/// which is what keeps `log()` from ever holding the sink and backlog locks at
/// the same time (see [`retire_prearm_backlog`]).
fn write_to_file(line: &str) -> FileWrite {
    let mut slot = file_sink().lock().unwrap_or_else(|e| e.into_inner());
    write_to_file_at(&mut slot, line, Instant::now())
}

/// What one attempt to write a line to the on-disk log did.
struct FileWrite {
    /// Did the line reach a file?
    wrote: bool,
    /// A state change the operator should hear about — file logging pausing or
    /// coming back.
    ///
    /// Returned rather than printed because this is produced while the sink
    /// mutex is held, and the console rings must not be fed from under it.
    /// [`log()`] emits it once the lock is gone, which is what puts it in the
    /// GUI and web consoles too: before this it went only to stderr, so the one
    /// message saying "your logs stopped" was invisible on the two surfaces an
    /// operator actually watches.
    notice: Option<String>,
}

/// The whole write-and-recover state machine, over a caller-owned [`Sink`] and an
/// explicit `now`.
///
/// Split out from [`write_to_file`] for the same two reasons as
/// [`write_line_to`]: the global sink is shared with every other test in the
/// binary, and the clock is not something a test should be made to wait on.
/// `now` is passed in so the backoff can be driven forward instantly.
fn write_to_file_at(slot: &mut Sink, line: &str, now: Instant) -> FileWrite {
    let quiet = |wrote| FileWrite { wrote, notice: None };
    match std::mem::replace(slot, Sink::Off) {
        Sink::Off => quiet(false),
        Sink::Armed(mut sink) => {
            let err = match write_line_to(&mut sink, line) {
                Ok(()) => {
                    *slot = Sink::Armed(sink);
                    return quiet(true);
                }
                Err(e) => e,
            };
            *slot = Sink::Paused {
                policy: sink.policy,
                retry_at: now + RETRY_BACKOFF_START,
                backoff: RETRY_BACKOFF_START,
                dropped: 1,
            };
            // Said once, on the transition — not once per line.  A failing disk
            // that repeated this for every message would bury the reason it
            // started.  Self-contained: it names the error rather than pointing
            // at a previous line, because on the GUI and web consoles there is
            // no previous line to point at.
            FileWrite {
                wrote: false,
                notice: Some(format!(
                    "File logging paused: {} — retrying in {}s.",
                    err,
                    RETRY_BACKOFF_START.as_secs()
                )),
            }
        }
        Sink::Paused { policy, retry_at, backoff, dropped } => {
            if now < retry_at {
                // Not due yet: count the line and stay quiet.  This is the branch
                // that runs for all but a handful of lines during an outage, so
                // it must do no I/O at all.
                *slot = Sink::Paused { policy, retry_at, backoff, dropped: dropped.saturating_add(1) };
                return quiet(false);
            }
            match resume(&policy, dropped, line) {
                Some(sink) => {
                    *slot = Sink::Armed(sink);
                    FileWrite {
                        wrote: true,
                        notice: Some(format!(
                            "File logging resumed; {} line(s) were lost while it was paused.",
                            dropped
                        )),
                    }
                }
                None => {
                    // Still broken: wait longer before the next attempt, so a
                    // permanent failure settles into one reopen every 5 min.
                    let backoff = next_backoff(backoff);
                    *slot = Sink::Paused {
                        policy,
                        backoff,
                        retry_at: now + backoff,
                        dropped: dropped.saturating_add(1),
                    };
                    quiet(false)
                }
            }
        }
    }
}

/// Try to bring a paused log back: reopen it, record the gap, then write `line`.
///
/// The gap notice goes in the **file**, not just on stderr, because the file is
/// what an operator reads afterwards — and a log that resumes with no mention of
/// the outage reads as a quiet period rather than as missing data.
fn resume(policy: &FilePolicy, dropped: u64, line: &str) -> Option<FileSink> {
    let file = open_log(&policy.path, false).ok()?;
    let written = file.metadata().map(|m| m.len()).unwrap_or(0);
    let mut sink = FileSink { policy: policy.clone(), file, written };
    let notice = format!(
        "--- file logging resumed after an error; {} line(s) were not written ---",
        dropped
    );
    write_line_to(&mut sink, &notice).ok()?;
    write_line_to(&mut sink, line).ok()?;
    Some(sink)
}

/// Hold a line that could not be written yet, while the pre-arm window is open.
///
/// A no-op once [`configure_file_logging`] has run: after that, a line that
/// didn't reach a file is one the operator chose not to keep (or one that failed
/// to write and reported itself), not one waiting for a file to exist.
fn buffer_prearm_line(line: &str) {
    let mut held = prearm_backlog().lock().unwrap_or_else(|e| e.into_inner());
    let Some(lines) = held.as_mut() else { return };
    push_bounded(lines, line, MAX_PREARM_LINES);
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
    // A line that predates the config (the version banner, the config-load
    // diagnostics) has no file to go to yet, so it waits in the backlog and is
    // written when one is armed.
    let outcome = write_to_file(&msg);
    if !outcome.wrote {
        buffer_prearm_line(&msg);
    }
    push_to_rings(msg);
    // The sink lock is released by now, so a pause/resume notice can go through
    // the same rings as everything else — the GUI console and the web console
    // are where an operator would notice that logging stopped, and neither of
    // them sees stderr.  Pushed directly rather than via log(), which would
    // re-enter the sink and, on the pause transition, count its own notice as a
    // dropped line.
    if let Some(notice) = outcome.notice {
        eprintln!("{}", notice);
        push_to_rings(notice);
    }
}

/// Append `line` to both in-memory console buffers.
///
/// Recovers from a poisoned lock rather than dropping the line: if a thread
/// panicked while holding one of these, logging is exactly when we most want it
/// to keep working.  Matches config.rs / gui.rs.
fn push_to_rings(line: String) {
    if let Some(buf) = LOG_BUFFER.get() {
        let mut buf = buf.lock().unwrap_or_else(|e| e.into_inner());
        buf.push_back(line.clone());
        while buf.len() > MAX_LINES {
            buf.pop_front();
        }
    }
    if let Some(buf) = HISTORY_BUFFER.get() {
        let mut buf = buf.lock().unwrap_or_else(|e| e.into_inner());
        buf.push_back(line);
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

    /// A rotation that cannot rename must leave the active log **alone**.
    ///
    /// This was a real data-loss path: `rotate` warned about the failed rename
    /// and then reopened the active file with `truncate(true)` anyway, so a log
    /// that could not be moved aside was emptied instead of rotated — the one
    /// outcome rotation exists to prevent.
    ///
    /// The rename is made to fail portably by putting a **non-empty directory**
    /// where generation `.1` belongs: `remove_file` will not delete it and
    /// `rename` will not replace it, on Unix or Windows, without needing
    /// permission games that behave differently on each.
    #[test]
    fn test_rotation_failure_leaves_the_log_intact() {
        let dir = std::env::temp_dir().join(format!("eg_log_rotfail_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let base = dir.join("t.log");

        // `t.log.1` is a non-empty directory: nothing can rename onto it.
        let blocker = rotated_path(&base, 1);
        std::fs::create_dir_all(&blocker).unwrap();
        std::fs::write(blocker.join("keep"), b"x").unwrap();

        // 50-byte cap against ~40-byte lines: the first line fits, the second
        // must rotate.
        let policy = FilePolicy { path: base.clone(), max_bytes: 50, max_files: 1 };
        let mut sink = FileSink {
            file: open_log(&policy.path, false).unwrap(),
            written: 0,
            policy,
        };

        // Under the cap: written normally.
        write_line_to(&mut sink, "keep me ------------------------------").unwrap();
        // Over the cap: rotation is attempted, fails, and must report failure
        // rather than silently truncating.
        let err = write_line_to(&mut sink, "this one triggers the rotation --------");
        assert!(err.is_err(), "a failed rotation must be reported to the caller");

        let body = std::fs::read_to_string(&base).unwrap();
        assert!(
            body.contains("keep me"),
            "the active log was truncated after a failed rename: {body:?}"
        );

        drop(sink);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The backoff doubles and stops at the ceiling — it must never grow without
    /// bound (a week-long delay is indistinguishable from the old "disabled until
    /// restart" behaviour this replaced).
    #[test]
    fn test_backoff_doubles_and_caps() {
        assert_eq!(next_backoff(RETRY_BACKOFF_START), RETRY_BACKOFF_START * 2);
        assert_eq!(next_backoff(RETRY_BACKOFF_MAX), RETRY_BACKOFF_MAX);
        assert_eq!(next_backoff(RETRY_BACKOFF_MAX / 2 + Duration::from_secs(1)), RETRY_BACKOFF_MAX);
        // Iterating always lands exactly on the cap and stays there.
        let mut d = RETRY_BACKOFF_START;
        for _ in 0..20 {
            d = next_backoff(d);
        }
        assert_eq!(d, RETRY_BACKOFF_MAX);
        assert!(RETRY_BACKOFF_START < RETRY_BACKOFF_MAX, "the sequence must actually back off");
    }

    /// A write error must **pause** file logging, not end it for the life of the
    /// process — and the recovery must say how much was lost, in the file.
    ///
    /// The old behaviour dropped the sink on the first error, so one full disk or
    /// one momentary write failure stopped logging until the gateway was
    /// restarted, with nothing in the log to say so.
    ///
    /// Drives an owned [`Sink`] and an explicit clock: no globals (every other
    /// test in the binary logs into whatever sink is armed) and no waiting.
    #[test]
    fn test_write_error_pauses_then_re_arms_and_reports_the_gap() {
        let dir = std::env::temp_dir().join(format!("eg_log_pause_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let base = dir.join("p.log");

        let policy = FilePolicy { path: base.clone(), max_bytes: 0, max_files: 0 };
        // A read-only handle to a writable path: writes through it fail, but
        // reopening it (which is what recovery does) succeeds.  That is exactly
        // the shape of a transient failure.
        std::fs::write(&base, b"").unwrap();
        let mut slot = Sink::Armed(FileSink {
            file: std::fs::File::open(&base).unwrap(),
            written: 0,
            policy,
        });

        let t0 = Instant::now();
        let first = write_to_file_at(&mut slot, "lost one", t0);
        assert!(!first.wrote, "a failed write must report failure");
        assert!(
            first.notice.as_deref().is_some_and(|n| n.contains("paused") && n.contains("retrying")),
            "the pause must be announced through log()'s rings, not only stderr: {:?}",
            first.notice,
        );
        match &slot {
            Sink::Paused { dropped, backoff, .. } => {
                assert_eq!(*dropped, 1);
                assert_eq!(*backoff, RETRY_BACKOFF_START, "first retry uses the starting delay");
            }
            _ => panic!("a write error must leave the sink paused, not off or armed"),
        }

        // Before the retry is due: lines are counted, and nothing is reopened.
        let during = write_to_file_at(&mut slot, "lost two", t0 + Duration::from_secs(1));
        assert!(!during.wrote);
        assert!(
            during.notice.is_none(),
            "only the transition is announced; a line during the pause must be silent"
        );
        assert!(matches!(slot, Sink::Paused { dropped: 2, .. }), "lines during the pause must be counted");

        // Once it is due, the file reopens and logging carries on by itself.
        let due = t0 + RETRY_BACKOFF_START + Duration::from_secs(1);
        let back = write_to_file_at(&mut slot, "after recovery", due);
        assert!(back.wrote, "the sink must re-arm itself");
        assert!(
            back.notice.as_deref().is_some_and(|n| n.contains("resumed") && n.contains("2 line")),
            "the recovery notice must reach the consoles and state the loss: {:?}",
            back.notice,
        );
        assert!(matches!(slot, Sink::Armed(_)), "a successful retry must leave the sink armed");
        assert!(write_to_file_at(&mut slot, "still logging", due).wrote, "and stay armed afterwards");

        let body = std::fs::read_to_string(&base).unwrap();
        assert!(
            body.contains("2 line(s) were not written"),
            "the resumed log must record the size of the gap: {body:?}"
        );
        assert!(body.contains("after recovery") && body.contains("still logging"), "got {body:?}");
        assert!(
            !body.contains("lost one") && !body.contains("lost two"),
            "lines written while paused cannot appear — they were never held: {body:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A retry that fails backs off further instead of hammering the disk, and
    /// keeps counting what was lost.
    #[test]
    fn test_failed_retry_backs_off_further() {
        // A path inside a directory that does not exist: reopening always fails.
        let missing = std::env::temp_dir()
            .join(format!("eg_log_absent_{}", std::process::id()))
            .join("nope")
            .join("x.log");
        let policy = FilePolicy { path: missing, max_bytes: 0, max_files: 0 };
        let t0 = Instant::now();
        let mut slot = Sink::Paused {
            policy,
            retry_at: t0,
            backoff: RETRY_BACKOFF_START,
            dropped: 3,
        };

        let retry = write_to_file_at(&mut slot, "still nowhere to go", t0);
        assert!(!retry.wrote);
        assert!(retry.notice.is_none(), "a failed retry must not re-announce the pause");
        match &slot {
            Sink::Paused { dropped, backoff, .. } => {
                assert_eq!(*dropped, 4, "the line must still be counted as lost");
                assert_eq!(*backoff, next_backoff(RETRY_BACKOFF_START), "the delay must grow");
            }
            _ => panic!("a failed retry must stay paused"),
        }
    }

    /// An unchanged policy must not reset a pause.  The server re-reads its
    /// config on every restart cycle, and an operator saving settings during a
    /// disk-full outage would otherwise drive the retries back down to every 30 s
    /// — turning a bounded backoff into a busy loop.
    #[test]
    fn test_reconfiguring_the_same_policy_keeps_the_backoff() {
        let _guard = FileLogTestGuard::new();
        let policy = FilePolicy {
            path: std::env::temp_dir().join(format!("eg_log_same_{}.log", std::process::id())),
            max_bytes: 0,
            max_files: 2,
        };
        let far_off = Instant::now() + RETRY_BACKOFF_MAX;
        {
            let mut slot = file_sink().lock().unwrap_or_else(|e| e.into_inner());
            *slot = Sink::Paused {
                policy: policy.clone(),
                retry_at: far_off,
                backoff: RETRY_BACKOFF_MAX,
                dropped: 9,
            };
        }

        configure_file_logging(Some(policy.clone()));
        {
            let slot = file_sink().lock().unwrap_or_else(|e| e.into_inner());
            match &*slot {
                Sink::Paused { backoff, dropped, retry_at, .. } => {
                    assert_eq!(*backoff, RETRY_BACKOFF_MAX, "an unchanged policy must not reset the backoff");
                    // `>=`, not `==`: nothing serialises the ~1650 tests that
                    // merely call `log()`, and each of those lands on the armed
                    // sink — here, bumping the paused count.  What matters is
                    // that the count was carried over, not reset.
                    assert!(*dropped >= 9, "nor the count of what was lost (got {dropped})");
                    assert_eq!(*retry_at, far_off, "nor bring the retry forward");
                }
                _ => panic!("an unchanged policy must leave a paused sink paused"),
            }
        }

        // A *changed* policy is a different instruction, and does reopen.
        let changed = FilePolicy { max_files: 3, ..policy };
        configure_file_logging(Some(changed));
        {
            let slot = file_sink().lock().unwrap_or_else(|e| e.into_inner());
            assert!(matches!(&*slot, Sink::Armed(_)), "a changed policy must be applied");
        }
        let _ = std::fs::remove_file(
            std::env::temp_dir().join(format!("eg_log_same_{}.log", std::process::id())),
        );
    }

    /// The pre-arm backlog reaches the file, in order, once one is armed.
    ///
    /// This is what puts the version banner and the `load_or_create_config`
    /// diagnostics into the log: the log path comes from the config, so those
    /// lines are all emitted before any file can be open. Uses an owned sink and
    /// an owned deque — the real backlog is a process global whose window closes
    /// on the first `configure_file_logging` call anywhere in the binary, so a
    /// test driving it would depend on suite ordering.
    #[test]
    fn test_prearm_backlog_reaches_the_file_in_order() {
        let dir = std::env::temp_dir().join(format!("eg_log_pre_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let base = dir.join("p.log");

        let policy = FilePolicy { path: base.clone(), max_bytes: 0, max_files: 0 };
        let mut sink = FileSink {
            file: open_log(&policy.path, false).unwrap(),
            written: 0,
            policy,
        };

        let backlog: VecDeque<String> = ["Ethernet Gateway v9.9.9", "Author: Ricky Bryce", "Created default configuration: egateway.conf"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        drain_backlog_into(backlog, &mut sink);
        // A line logged after arming must follow the backlog, not precede it.
        write_line_to(&mut sink, "Logging to p.log").unwrap();

        let body = std::fs::read_to_string(&base).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(
            lines,
            vec![
                "Ethernet Gateway v9.9.9",
                "Author: Ricky Bryce",
                "Created default configuration: egateway.conf",
                "Logging to p.log",
            ],
            "backlog must land in order, ahead of post-arm lines"
        );

        drop(sink);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The backlog is bounded, and keeps the NEWEST lines — those are the ones
    /// nearest whatever failure is being diagnosed.  Unbounded would mean a
    /// process that never arms a log file grows a buffer nothing will ever read.
    #[test]
    fn test_prearm_backlog_is_bounded_keeping_newest() {
        let mut lines = VecDeque::new();
        for i in 0..10 {
            push_bounded(&mut lines, &format!("line {i}"), 4);
        }
        assert_eq!(lines.len(), 4, "cap not enforced");
        assert_eq!(
            lines.iter().map(String::as_str).collect::<Vec<_>>(),
            vec!["line 6", "line 7", "line 8", "line 9"],
            "the oldest lines must be the ones dropped"
        );
        // The shipped cap is generous enough for a startup, and bounded.
        assert!(
            (100..=2000).contains(&MAX_PREARM_LINES),
            "MAX_PREARM_LINES = {MAX_PREARM_LINES} is outside the sane range"
        );
    }

    /// Arming a file flushes the backlog into it — and arming a *second* file
    /// must NOT replay those lines, or every restart cycle duplicates the whole
    /// startup block into the log.
    ///
    /// This drives the real globals (that is the property under test), so it
    /// takes the guard.  An earlier version of this test asserted only that two
    /// `Option::take`s in a row yield `None`, which passes no matter what the
    /// production code does — by the time it ran, another test had usually
    /// already closed the window, so both takes returned `None` and the
    /// assertion was trivially true.  Mutation-tested: replacing retire's
    /// `take()` with `clone()` fails this version and passed the old one.
    #[test]
    fn test_prearm_backlog_is_flushed_once_then_never_replayed() {
        let _guard = FileLogTestGuard::new();
        let dir = std::env::temp_dir().join(format!("eg_log_once_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let first = dir.join("one.log");
        let second = dir.join("two.log");

        // The guard's own configure(None) closed the window, so re-open it and
        // seed a line the way a pre-config log() call would have.
        {
            let mut held = prearm_backlog().lock().unwrap_or_else(|e| e.into_inner());
            let mut q = VecDeque::new();
            q.push_back("PREARM_SENTINEL banner line".to_string());
            *held = Some(q);
        }

        let policy = |p: &std::path::Path| FilePolicy {
            path: p.to_path_buf(),
            max_bytes: 0,
            max_files: 0,
        };

        configure_file_logging(Some(policy(&first)));
        let body = std::fs::read_to_string(&first).unwrap();
        assert!(
            body.contains("PREARM_SENTINEL"),
            "arming a file must flush the pre-arm backlog into it; got {body:?}"
        );

        // A later arm (a restart cycle, or a changed log path) must start clean.
        configure_file_logging(Some(policy(&second)));
        let body2 = std::fs::read_to_string(&second).unwrap();
        assert!(
            !body2.contains("PREARM_SENTINEL"),
            "the backlog was replayed into a second file — every restart would \
             duplicate the startup block; got {body2:?}"
        );

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

    /// The hint shown under the log controls in the web and GUI panels.  Its
    /// three branches are the ones an operator can reach — off, no size limit,
    /// bounded — and the bounded figure must come from `max_disk_kb` rather than
    /// being multiplied out again.  Lives here because both surfaces share it;
    /// two copies had already drifted in wording before this was consolidated.
    #[test]
    fn test_log_state_hint_covers_each_state() {
        let cfg = crate::config::Config::default();

        // Off, by the flag.
        let off = crate::config::Config { log_to_file: false, ..cfg.clone() };
        assert!(
            log_state_hint(&off, 1024, 5).contains("stderr"),
            "off state: {}",
            log_state_hint(&off, 1024, 5)
        );

        // Off, by a blank (or whitespace-only) path — an off-switch of its own.
        for blank in ["", "   "] {
            let blanked = crate::config::Config {
                log_to_file: true,
                log_file: blank.into(),
                ..cfg.clone()
            };
            let h = log_state_hint(&blanked, 1024, 5);
            assert!(h.contains("stderr"), "a blank log_file must read as off: {h}");
            assert!(
                file_policy_from(&blanked).is_none(),
                "hint and policy must agree that a blank path means off"
            );
        }

        let on = crate::config::Config {
            log_to_file: true,
            log_file: "eg.log".into(),
            ..cfg.clone()
        };

        // No size limit.
        let h = log_state_hint(&on, 0, 5);
        assert!(h.contains("without bound"), "unbounded state: {h}");

        // Bounded — states the real total and names the file.
        let h = log_state_hint(&on, 1024, 5);
        let expected = max_disk_kb(1024, 5);
        assert!(
            h.contains(&format!("{expected} KB")),
            "bounded hint should state the {expected} KB bound: {h}"
        );
        assert!(h.contains("eg.log"), "bounded hint should name the file: {h}");

        // The numbers come from the ARGUMENTS, not from cfg — that is what lets
        // the GUI show a figure that tracks a half-typed field instead of
        // lagging a keystroke behind.
        let h = log_state_hint(&on, 512, 3);
        assert!(
            h.contains(&format!("{} KB", max_disk_kb(512, 3))),
            "hint must use the passed-in numbers, not cfg's: {h}"
        );
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
