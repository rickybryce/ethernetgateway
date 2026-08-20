//! One gateway per directory, and a way to hand over to a new copy.
//!
//! **A second copy binds nothing, and that is worse than it sounds.** Launch
//! the gateway twice from the same place and the second process comes up
//! fully, prints its banner, opens a window, offers every configuration
//! screen -- and holds not one listener, because the first copy still has the
//! ports. Measured on 2026-08-19: five copies stacked up, the oldest still
//! serving telnet while a newer one served the web UI, so a Save in the
//! visible window never reached the process that was answering connections.
//! `bindwatch` says so in the log, but by then the operator is editing the
//! wrong process's settings.
//!
//! So a launch now finds out first, and asks. The GUI offers to **take over**:
//! the running copy stands down and this one takes the ports.
//!
//! **The handover is cooperative, not a kill.** The obvious mechanism is
//! `SIGTERM`, and it is wrong twice over: there is no `SIGTERM` on Windows, and
//! a signal skips the gateway's own shutdown path -- the broadcast that tells
//! connected sessions the server is going down, the bounded join of the serial
//! threads, the staged write of a booted disk image. Instead the newcomer
//! leaves a request file and the holder, which polls for it, trips its
//! ordinary `shutdown` flag. Identical to a Quit from its own window, on every
//! platform, with no new signal plumbing.
//!
//! **The lock is an OS lock, never a PID file.** A PID in a file is a liar: it
//! goes stale on a crash, and PIDs are reused, so a stale one can name a live
//! and entirely unrelated process. An advisory lock is released by the kernel
//! when the holder dies however it dies, so a crashed gateway leaves nothing
//! to clean up and the next launch simply succeeds. The PID *is* written into
//! the file, but only so a message can name it -- nothing is ever decided from
//! it.

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::config;
use crate::logger::glog;

/// Lock file name, inside the data directory.
const LOCK_FILE: &str = "ethernetgateway.lock";

/// The newcomer's request that the holder stand down.
const HANDOVER_FILE: &str = "handover.request";

/// How long a newcomer waits for the holder to release the ports.
///
/// The holder's own shutdown waits up to 3 s for serial threads plus 2 s for
/// the tokio runtime, so this has to exceed that with room to spare or a
/// perfectly healthy handover would be reported as a failure.
const HANDOVER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// How often the holder checks whether it has been asked to stand down, and
/// how often a newcomer retries the lock.  Fast enough to feel immediate, slow
/// enough to be free.
const POLL: std::time::Duration = std::time::Duration::from_millis(250);

fn lock_path() -> PathBuf {
    PathBuf::from(config::DATA_DIR).join(LOCK_FILE)
}

fn handover_path() -> PathBuf {
    PathBuf::from(config::DATA_DIR).join(HANDOVER_FILE)
}

/// Held for as long as this process is the gateway for this directory.
///
/// Dropping it releases the lock, so it must be kept alive in `main` — a
/// `let _ = acquire()` would unlock immediately and let a second copy in.
/// `#[must_use]` on the enum below is what stops that being silent.
pub struct InstanceLock {
    _file: File,
}

/// What a launch found.
#[must_use]
pub enum Instance {
    /// We are the gateway for this directory; hold this until we exit.
    Acquired(InstanceLock),
    /// Another copy holds it.  `pid` is for the message only.
    Busy { pid: Option<u32> },
}

/// Try to become the gateway for this directory.
///
/// The data directory must already exist (`config::ensure_data_dir`).
///
/// One implementation per platform rather than one function with two `cfg`
/// blocks inside it: the two do not merely lock differently, they *learn*
/// different things. A Unix newcomer can still read the holder's PID out of
/// the file, because `flock` is advisory and does not prevent an open; a
/// Windows one cannot, since the share mode that does the locking is exactly
/// what refuses it the open. Written as a single body that returned early for
/// Windows, the difference showed up as a dead `read_pid` and a needless
/// `return` -- both of them clippy errors on that target only, and invisible
/// from Linux.
#[cfg(unix)]
pub fn acquire() -> std::io::Result<Instance> {
    use std::os::unix::io::AsRawFd;
    let path = lock_path();
    // Read/write and NOT truncated: another copy may be holding it, and
    // truncating would erase the PID we are about to read out of it.
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;
    // SAFETY: `flock` takes a live descriptor we own for the duration of the
    // call and touches no memory of ours.  LOCK_NB makes it return rather than
    // block, which is the whole point -- a launch must not hang behind another
    // copy.
    let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    if locked {
        write_pid(&mut file);
        Ok(Instance::Acquired(InstanceLock { _file: file }))
    } else {
        Ok(Instance::Busy { pid: read_pid(&path) })
    }
}

/// Windows counterpart: the open *is* the lock.
///
/// There is no advisory lock in `std` for Windows, but there is something
/// better suited here -- a share mode of 0 means no other process may open the
/// file at all, so acquiring is an open and releasing is the handle closing,
/// including on a crash. The cost is that a second copy cannot read the PID
/// either, which is why `Busy::pid` is an `Option` rather than a number.
#[cfg(windows)]
pub fn acquire() -> std::io::Result<Instance> {
    use std::os::windows::fs::OpenOptionsExt;
    let path = lock_path();
    match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(&path)
    {
        Ok(mut file) => {
            write_pid(&mut file);
            Ok(Instance::Acquired(InstanceLock { _file: file }))
        }
        // A sharing violation means somebody else has it.  Any other error
        // would also leave us unable to serve, and reporting "already running"
        // is the honest reading of "cannot claim the directory".
        Err(_) => Ok(Instance::Busy { pid: None }),
    }
}

/// Record our PID in the lock file, for a later copy's message.
///
/// Failure is ignored on purpose: the lock is already held at this point, and
/// losing a diagnostic must not cost us the gateway.
fn write_pid(file: &mut File) {
    use std::io::Seek;
    let _ = file.set_len(0);
    let _ = file.seek(std::io::SeekFrom::Start(0));
    let _ = write!(file, "{}", std::process::id());
    let _ = file.flush();
}

/// The PID in the lock file, if it holds a plausible one.
///
/// Unix only: on Windows the share mode that locks the file also refuses us
/// the open, so there is nothing to read.
#[cfg(unix)]
fn read_pid(path: &std::path::Path) -> Option<u32> {
    use std::io::Read;
    let mut s = String::new();
    File::open(path).ok()?.read_to_string(&mut s).ok()?;
    s.trim().parse().ok()
}

/// Ask the running copy to stand down, then wait for its lock.
///
/// Returns the lock on success. On timeout the request file is removed again —
/// leaving it behind would tell the *next* launch's holder to stand down for a
/// handover nobody is waiting for.
pub fn request_handover() -> std::io::Result<Option<InstanceLock>> {
    std::fs::write(handover_path(), b"stand down\n")?;
    let deadline = std::time::Instant::now() + HANDOVER_TIMEOUT;
    while std::time::Instant::now() < deadline {
        std::thread::sleep(POLL);
        if let Ok(Instance::Acquired(lock)) = acquire() {
            // Ours now.  Clear the request so our own watcher does not read it
            // as an instruction to stand down the moment it starts.
            let _ = std::fs::remove_file(handover_path());
            return Ok(Some(lock));
        }
    }
    let _ = std::fs::remove_file(handover_path());
    Ok(None)
}

/// Clear a request file left behind by a copy that died mid-handover.
///
/// Called by a launch that acquired the lock cleanly: whatever that file was
/// asking for, the process it was asking is gone.
pub fn clear_stale_handover_request() {
    if handover_path().exists() {
        let _ = std::fs::remove_file(handover_path());
    }
}

/// Watch for a newcomer asking us to stand down, and shut down when one does.
///
/// Trips `shutdown` **without** `restart`, which is exactly what the Quit
/// button does — the process unwinds its server cycle, closes its window and
/// exits, releasing the ports and the lock for the copy that asked.
pub fn spawn_handover_watcher(shutdown: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        // **Must outlive a restart, and must keep asserting once asked.**
        //
        // The first version returned as soon as `shutdown` was set, which is
        // set by every Save and Restart -- so the watcher died on the first
        // restart and the gateway silently stopped answering handover requests
        // for the rest of its life. `main`'s signal watcher already carries
        // this exact lesson in its comment ("Loops to survive server restarts
        // (flag resets to false between cycles)"); this had to learn it twice.
        //
        // And once a handover has been asked for, the flag is re-asserted on
        // every pass rather than set once. A restart cycle clears `shutdown`
        // between server cycles, so a request that arrived in that window
        // would otherwise be wiped and the newcomer would wait out its timeout
        // against a gateway that had agreed to stand down.
        let mut standing_down = false;
        loop {
            if handover_path().exists() {
                // Removed *before* the flag: the newcomer watches for our
                // lock, not for this file, and a request left on disk would be
                // read as an instruction by whoever holds the directory next.
                let _ = std::fs::remove_file(handover_path());
                glog!("Another copy of the gateway asked to take over — standing down.");
                standing_down = true;
            }
            if standing_down {
                shutdown.store(true, Ordering::SeqCst);
            }
            std::thread::sleep(POLL);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The paths must sit inside the data directory, not beside the binary —
    /// the whole point of the move.
    #[test]
    fn test_the_lock_and_request_live_in_the_data_directory() {
        assert!(lock_path().starts_with(config::DATA_DIR));
        assert!(handover_path().starts_with(config::DATA_DIR));
        // And they are distinct files; one path used for both would make a
        // request indistinguishable from the lock itself.
        assert_ne!(lock_path(), handover_path());
    }

    /// **The wait has to outlast a healthy shutdown, or a working handover
    /// reports failure.** The holder's own teardown allows 3 s for the serial
    /// threads and 2 s for the runtime drop, so a timeout at or below that
    /// would give up on a copy that was standing down correctly.
    #[test]
    fn test_the_handover_wait_outlasts_a_healthy_shutdown() {
        assert!(
            HANDOVER_TIMEOUT >= std::time::Duration::from_secs(10),
            "3s serial join + 2s runtime drop needs real headroom"
        );
        assert!(POLL < std::time::Duration::from_secs(1), "a handover must feel immediate");
    }
}
