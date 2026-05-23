//! Global log buffer shared between the server and GUI console.
//!
//! All server output that previously went to `eprintln!` is routed through
//! [`log()`] which writes to stderr and two parallel in-memory ring buffers:
//! a drain-style buffer used by the GUI's per-frame accumulator and a
//! snapshot-style buffer used by the web server (which can be polled
//! without disturbing the GUI's view).

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

const MAX_LINES: usize = 2000;

static LOG_BUFFER: OnceLock<Mutex<VecDeque<String>>> = OnceLock::new();
static HISTORY_BUFFER: OnceLock<Mutex<VecDeque<String>>> = OnceLock::new();

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
    if let Some(buf) = LOG_BUFFER.get()
        && let Ok(mut buf) = buf.lock()
    {
        buf.push_back(msg.clone());
        while buf.len() > MAX_LINES {
            buf.pop_front();
        }
    }
    if let Some(buf) = HISTORY_BUFFER.get()
        && let Ok(mut buf) = buf.lock()
    {
        buf.push_back(msg);
        while buf.len() > MAX_LINES {
            buf.pop_front();
        }
    }
}

/// Drain all buffered log lines (used by the GUI console each frame).
pub fn drain() -> Vec<String> {
    if let Some(buf) = LOG_BUFFER.get()
        && let Ok(mut buf) = buf.lock()
    {
        return buf.drain(..).collect();
    }
    Vec::new()
}

/// Return a snapshot of the most recent log lines without removing
/// them from the history buffer.  Used by the web-config console
/// poller (the GUI's accumulator continues to drain its own buffer).
pub fn snapshot(max: usize) -> Vec<String> {
    if let Some(buf) = HISTORY_BUFFER.get()
        && let Ok(buf) = buf.lock()
    {
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
