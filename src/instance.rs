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

/// Where the lock lives.
///
/// Public because a failure to *open* it is the one startup error that needs to
/// name the file rather than the directory: a root-owned lock file is what a
/// single `sudo` run leaves behind, and the diagnosis has to stat the thing it
/// was refused (see `config::data_dir_ownership_lines`).
pub fn lock_path() -> PathBuf {
    PathBuf::from(config::DATA_DIR).join(LOCK_FILE)
}

fn handover_path() -> PathBuf {
    PathBuf::from(config::DATA_DIR).join(HANDOVER_FILE)
}

/// Whoever creates this decides a handover request; see [`claim_request`].
const CLAIM_FILE: &str = "handover.claimed";

fn claim_path() -> PathBuf {
    PathBuf::from(config::DATA_DIR).join(CLAIM_FILE)
}

/// Held for as long as this process is the gateway for this directory.
///
/// Dropping it releases the lock, so it must be kept alive in `main` — a
/// `let _ = acquire()` would unlock immediately and let a second copy in.
/// Nothing in the type stops that (`let _ =` silences `#[must_use]`); the
/// named binding in `main`, `_instance_lock`, is what keeps it.
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
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        write_pid(&mut file);
        return Ok(Instance::Acquired(InstanceLock { _file: file }));
    }
    // Only "would block" is another copy -- the rule the Windows arm learned.
    // A volume that cannot lock at all (NFS without lockd: ENOLCK; some FUSE
    // and SMB mounts: ENOTSUP) used to read as a holder too, naming whatever
    // stale PID the file still held and offering a takeover from nobody.
    let err = std::io::Error::last_os_error();
    if is_lock_held(err.raw_os_error()) {
        Ok(Instance::Busy { pid: read_pid(&path) })
    } else {
        Err(err)
    }
}

/// `EWOULDBLOCK` (`EAGAIN` on every Unix we ship): the lock is held by someone
/// else.  Pure, like [`is_sharing_violation`], so the rule is tested by itself.
#[cfg(unix)]
fn is_lock_held(raw: Option<i32>) -> bool {
    raw == Some(libc::EWOULDBLOCK) || raw == Some(libc::EAGAIN)
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
    let mut tries = 0;
    loop {
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
                return Ok(Instance::Acquired(InstanceLock { _file: file }));
            }
            // Only a sharing violation means somebody else has it -- and a
            // virus scanner or backup tool holding the file for a moment
            // causes one too, so it is asked again briefly before being
            // believed.
            Err(e) if is_sharing_violation(e.raw_os_error()) => {
                tries += 1;
                if tries >= SHARING_RETRIES {
                    return Ok(Instance::Busy { pid: None });
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            // Anything else -- permission denied, a read-only volume, a
            // missing directory -- is *not* another copy, and reporting it as
            // one offered to take over from a gateway that did not exist and
            // then waited fifteen seconds for it to stand down.  Returned as an
            // error so `main`'s ownership diagnosis can name the real cause.
            Err(e) => return Err(e),
        }
    }
}

/// How many times a sharing violation is retried before it is believed.
const SHARING_RETRIES: u32 = 5;

/// `ERROR_SHARING_VIOLATION` (32) or `ERROR_LOCK_VIOLATION` (33): the file is
/// open elsewhere.  A pure function of the raw code so it is tested on every
/// platform, not only where the lock it decides is compiled.
#[cfg(any(windows, test))]
fn is_sharing_violation(raw: Option<i32>) -> bool {
    matches!(raw, Some(32) | Some(33))
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

/// How a handover ended, for [`request_handover`]'s caller.
pub enum Handover {
    /// The running copy stood down and the lock is ours.
    Took(InstanceLock),
    /// It never answered, and the request was taken back unseen.
    NotAnswered,
    /// It agreed, and did not let go within [`HANDOVER_AGREED_WAIT`].
    AgreedButStuck,
    /// We were asked to stop (Ctrl-C, SIGTERM) while waiting.  `holder_agreed`
    /// says the running copy had already taken the request -- it is standing
    /// down, and nothing will replace it.
    Stopped { holder_agreed: bool },
}

/// Claim the decision on a handover request, as either side, and say whether
/// **this** call won it.  The holder stands down only if it claimed, and the
/// newcomer gives up only if *it* did.  Each used to look at the request and
/// act regardless, and at the deadline both could -- the newcomer saw the
/// request unanswered and left, while the holder took it and stood down,
/// leaving no copy serving.
///
/// **An exclusive create, not a delete.**  Two racing deletes of one file both
/// reported success on macOS and on Windows in CI (Linux let only one win),
/// so a delete decided nothing there.  Creating a file that must not exist
/// (`O_EXCL`, `CREATE_NEW`) lets exactly one of two racers through on all
/// three.  The file is `path`, [`claim_path`] in use; the newcomer clears a
/// leftover one before it asks, and clears it again when it is done.
fn claim_request(path: &std::path::Path) -> bool {
    std::fs::OpenOptions::new().write(true).create_new(true).open(path).is_ok()
}

/// The newcomer is done with a request, whatever the outcome: neither file may
/// be left to be read as an instruction, or as a claim, by the next launch.
fn clear_request_files() {
    let _ = std::fs::remove_file(handover_path());
    let _ = std::fs::remove_file(claim_path());
}

/// Ask the running copy to stand down, then wait for its lock.
///
/// On a timeout the request is taken back -- left behind it would tell the
/// *next* launch's holder to stand down for a handover nobody is waiting for --
/// but only if it is still there to take: gone, the holder took it at the
/// last moment and is standing down, and is waited for like any other that
/// agreed.
///
/// **`stop` is honoured while waiting.**  The signal handlers are registered
/// before this runs, so a Ctrl-C or SIGTERM sets `stop` rather than ending the
/// process, and this loop used to ignore it for up to two and a quarter
/// minutes.
///
/// **A holder that agreed is waited for.**  Its watcher consumes the request
/// file the moment it agrees, and from then it *will* stop -- so giving up at
/// the deadline while it was still flushing a booted disk to an SD card left
/// nobody serving: this copy exited, and a moment later so did the old one.
/// The deadline is for a holder that never answered; one that answered gets
/// [`HANDOVER_AGREED_WAIT`] more.
pub fn request_handover(stop: &AtomicBool) -> std::io::Result<Handover> {
    let path = handover_path();
    let claim = claim_path();
    // A claim left by a copy that died mid-handover would win every race below.
    clear_leftover_claim(&claim)?;
    std::fs::write(&path, b"stand down\n")?;
    let start = std::time::Instant::now();
    let mut said_still_stopping = false;
    loop {
        std::thread::sleep(POLL);
        // An error is not "still held": it is a fault to report now, not to
        // retry for fifteen seconds and then call a timeout.
        if let Instance::Acquired(lock) = acquire()? {
            // Ours now.  Clear the request so our own watcher does not read it
            // as an instruction to stand down the moment it starts.
            clear_request_files();
            return Ok(Handover::Took(lock));
        }
        let agreed = holder_agreed(path.exists(), claim.exists());
        match next_step(start.elapsed(), agreed, stop.load(Ordering::SeqCst)) {
            Step::Wait => {}
            Step::WaitForAgreed => {
                if !std::mem::replace(&mut said_still_stopping, true) {
                    glog!("The running copy agreed and is still stopping; waiting for it...");
                }
            }
            Step::Stop => {
                let holder_agreed = !claim_request(&claim);
                clear_request_files();
                return Ok(Handover::Stopped { holder_agreed });
            }
            Step::GiveUp { agreed: true } => {
                clear_request_files();
                return Ok(Handover::AgreedButStuck);
            }
            // Taken back only if the claim is ours; otherwise the holder
            // claimed it just now, and is waited for as one that agreed.
            Step::GiveUp { agreed: false } => {
                if claim_request(&claim) {
                    clear_request_files();
                    return Ok(Handover::NotAnswered);
                }
            }
        }
    }
}

/// Clear a claim left by a copy that died mid-handover, before asking.
///
/// **A claim that will not go is an error, not a shrug.**  It would win the
/// holder's claim on every pass, so the holder could never agree -- and
/// [`holder_agreed`] reads a claim as agreement, so the newcomer would wait out
/// the agreed wait for a copy that never answered and then report it stuck.
///
/// **A sharing violation is asked about a few times first**, as [`acquire`]
/// asks: a virus scanner holding the file for a moment makes a delete fail
/// too.  Nothing else is retried -- a directory in the way, or a read-only
/// folder, says the same thing every time.
fn clear_leftover_claim(claim: &std::path::Path) -> std::io::Result<()> {
    let mut tries = 0;
    loop {
        match std::fs::remove_file(claim) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                tries += 1;
                if claim_delete_may_clear(e.raw_os_error()) && tries < SHARING_RETRIES {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    continue;
                }
                // The path goes with it: it is the file the operator has to remove.
                return Err(std::io::Error::new(
                    e.kind(),
                    format!("{} could not be removed: {e}", claim.display()),
                ));
            }
            _ => return Ok(()),
        }
    }
}

/// Is a failed delete of the claim worth asking again?  Only a Windows sharing
/// violation -- someone holding the file open for a moment.
#[cfg(windows)]
fn claim_delete_may_clear(raw: Option<i32>) -> bool {
    is_sharing_violation(raw)
}
#[cfg(not(windows))]
fn claim_delete_may_clear(_raw: Option<i32>) -> bool {
    false
}

/// Has the running copy agreed to stand down?
///
/// **Either sign will do: the request gone, or a claim made.**  The holder
/// claims and then deletes the request, and the delete can fail -- a Windows
/// virus scanner holding the file open is enough.  Read from the request alone,
/// that holder had not agreed, so at the deadline the newcomer tried to take
/// the request back, lost the claim to it, and tried again on every pass with
/// no deadline left at all.  The newcomer clears a leftover claim before it
/// asks, so a claim present now was made by the holder, for this request.
fn holder_agreed(request_present: bool, claim_present: bool) -> bool {
    !request_present || claim_present
}

/// What [`request_handover`] does after a poll that did not get the lock.
#[derive(Debug, PartialEq)]
enum Step {
    Wait,
    WaitForAgreed,
    Stop,
    GiveUp { agreed: bool },
}

/// The newcomer's decision, pulled out so its order is testable: a stop is
/// honoured before any deadline, an unanswered request is waited for until
/// [`HANDOVER_TIMEOUT`], and an agreed one [`HANDOVER_AGREED_WAIT`] longer.
fn next_step(waited: std::time::Duration, agreed: bool, stop: bool) -> Step {
    if stop {
        Step::Stop
    } else if waited < HANDOVER_TIMEOUT {
        Step::Wait
    } else if agreed && waited < HANDOVER_TIMEOUT + HANDOVER_AGREED_WAIT {
        Step::WaitForAgreed
    } else {
        Step::GiveUp { agreed }
    }
}

/// How much longer a holder that has agreed to stand down is waited for.
const HANDOVER_AGREED_WAIT: std::time::Duration = std::time::Duration::from_secs(120);

/// Clear a request file left behind by a copy that died mid-handover.
///
/// Called by a launch that acquired the lock cleanly: whatever that file was
/// asking for, the process it was asking is gone.
///
/// **Only a request older than our own lock.**  A newcomer launched a moment
/// after us can write its request between our `acquire` and this call, and it
/// is addressed to *us*; deleted as stale, it timed out against a gateway that
/// never saw it.  `since` is taken before `acquire`, so anything written after
/// it is a live request.
pub fn clear_stale_handover_request(since: std::time::SystemTime) {
    for path in [handover_path(), claim_path()] {
        let written = std::fs::metadata(&path).and_then(|m| m.modified());
        if let Ok(written) = written
            && is_stale_request(written, since)
        {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// A request is stale when it was written before we began to acquire --
/// clearly before, by more than a filesystem's timestamp can be off.  File
/// times come from a coarse clock (ext4's is a tick; FAT's is two seconds), so
/// a request written just after `since` can carry an earlier time, and a
/// strict comparison deleted the very request this rule protects.  A dead
/// copy's request is never that young: its newcomer would have taken the lock
/// itself and removed the file.
fn is_stale_request(written: std::time::SystemTime, since: std::time::SystemTime) -> bool {
    const CLOCK_SLACK: std::time::Duration = std::time::Duration::from_secs(2);
    written + CLOCK_SLACK < since
}

/// Watch for a newcomer asking us to stand down, and shut down when one does.
///
/// Trips `shutdown` **without** `restart`, which is exactly what the Quit
/// button does — the process unwinds its server cycle, closes its window and
/// exits, releasing the ports and the lock for the copy that asked.
///
/// **And `stop`**, the flag SIGTERM sets: a stop outranks a reload.  With only
/// `shutdown` set, a Save and Restart already in flight still had `restart`
/// armed, and the holder ran one more full server cycle -- rebinding every
/// port and taking sessions -- before standing down for the copy that asked.
pub fn spawn_handover_watcher(shutdown: Arc<AtomicBool>, stop: Arc<AtomicBool>) {
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
            // Claimed, not merely seen: the newcomer may be taking it back at
            // its deadline, and only one of us can (see `claim_request`).
            // The request is removed *before* the flag: the newcomer watches
            // for our lock, and for the request going as the sign we agreed.
            let asked = handover_path().exists() && claim_request(&claim_path());
            if asked {
                let _ = std::fs::remove_file(handover_path());
                glog!("Another copy of the gateway asked to take over — standing down.");
            }
            let assert_shutdown;
            (standing_down, assert_shutdown) = watcher_pass(asked, standing_down);
            if assert_shutdown {
                // `stop` first: the main loop reads it after `shutdown` wakes
                // it, and must find it already set.
                stop.store(true, Ordering::SeqCst);
                shutdown.store(true, Ordering::SeqCst);
            }
            std::thread::sleep(POLL);
        }
    });
}

/// One pass of the watcher's decision: given whether a request is on disk and
/// whether we have already been asked, should we be asserting `shutdown`?
///
/// **Pulled out so the fix has a test.** The defect it encodes was found live,
/// not by reading, and had no cover: the watcher used to return as soon as
/// `shutdown` was set, which every Save and Restart sets, so it died on the
/// first restart and the gateway silently stopped answering handover requests
/// for the rest of its life.
///
/// Two properties, and both are the fix. It takes **no shutdown flag** -- a
/// pass cannot decide to stop watching, because that is what went wrong. And
/// once asked it keeps asserting, `asked` being false thereafter: the request
/// file is consumed on the first pass that sees it, so a version that only set
/// the flag once would lose the request to a restart cycle clearing it in
/// between.
fn watcher_pass(asked: bool, standing_down: bool) -> (bool, bool) {
    let standing_down = standing_down || asked;
    (standing_down, standing_down)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only a sharing violation is "another copy holds it"; permission
    /// denied, a missing directory or a read-only volume must reach `main`'s
    /// ownership diagnosis instead of an offer to take over from nobody.
    #[test]
    fn test_only_a_sharing_violation_reads_as_another_copy() {
        assert!(is_sharing_violation(Some(32)), "ERROR_SHARING_VIOLATION");
        assert!(is_sharing_violation(Some(33)), "ERROR_LOCK_VIOLATION");
        for other in [Some(5), Some(2), Some(3), Some(19), None] {
            assert!(!is_sharing_violation(other), "{other:?} is not another copy");
        }
        const { assert!(SHARING_RETRIES >= 2, "a scanner's brief hold must be asked about again") };
    }

    /// The Unix half of the same rule: only a held lock is another copy.  A
    /// filesystem that cannot lock is an error for `main` to explain, not a
    /// stale PID to blame.
    #[cfg(unix)]
    #[test]
    fn test_only_a_held_flock_reads_as_another_copy() {
        assert!(is_lock_held(Some(libc::EWOULDBLOCK)));
        assert!(is_lock_held(Some(libc::EAGAIN)));
        for other in [libc::ENOLCK, libc::ENOTSUP, libc::EBADF, libc::EINVAL] {
            assert!(!is_lock_held(Some(other)), "errno {other} is not another copy");
        }
        assert!(!is_lock_held(None));
    }

    /// A request written after we began to acquire is addressed to us and
    /// survives the stale-file sweep; one from before is a dead copy's.
    #[test]
    fn test_only_a_request_from_before_our_lock_is_stale() {
        let t = std::time::SystemTime::now();
        let s = std::time::Duration::from_secs(1);
        assert!(is_stale_request(t - 3 * s, t), "a dead copy's request stays on disk");
        assert!(!is_stale_request(t + s, t), "a live request is deleted as stale");
        // A request written just after `since`, stamped by a coarse clock as
        // just before it, is still live.
        assert!(!is_stale_request(t - std::time::Duration::from_millis(900), t));
        assert!(!is_stale_request(t, t));
        // And the agreed holder is waited for well past the plain deadline.
        const { assert!(HANDOVER_AGREED_WAIT.as_secs() >= 4 * HANDOVER_TIMEOUT.as_secs()) };
    }

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

    /// **A watcher pass can never decide to stop watching, and never forgets.**
    ///
    /// The live defect this replaces: the watcher returned as soon as
    /// `shutdown` was set -- set by every Save and Restart -- so it died on the
    /// first restart and the gateway answered no handover request afterwards.
    #[test]
    fn test_the_watcher_keeps_asking_once_asked_and_never_stands_down_unasked() {
        // Nobody has asked: do not shut a working gateway down.
        assert_eq!(watcher_pass(false, false), (false, false));
        // Asked: start standing down.
        assert_eq!(watcher_pass(true, false), (true, true));
        // **The fix.** The request file is consumed by the pass that saw it, so
        // every later pass has `asked == false` -- and must still assert, or a
        // restart cycle clearing `shutdown` in between would swallow the
        // request and the newcomer would wait out its timeout.
        assert_eq!(watcher_pass(false, true), (true, true));
        // Asked twice is not different from asked once.
        assert_eq!(watcher_pass(true, true), (true, true));
    }

    /// **At the deadline, exactly one side acts on the request.**  The newcomer
    /// giving up and the holder agreeing both claim it, and only one claim
    /// succeeds -- so the holder stands down only if it claimed, and the
    /// newcomer leaves only if it did.  Before, each looked and acted
    /// regardless, and both could: the newcomer left as the holder stood down.
    /// The first version of this fix claimed by deleting the request, and CI
    /// showed two racing deletes both succeed on macOS and Windows; the race
    /// below is what caught it, and now runs against an exclusive create.  Also,
    /// a stop wins over every deadline, and the deadlines keep their order.
    #[test]
    fn test_only_one_side_takes_the_request_and_a_stop_wins() {
        let dir = std::env::temp_dir().join(format!("egw_handover_claim_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(CLAIM_FILE);
        for _ in 0..200 {
            let _ = std::fs::remove_file(&path);
            let p1 = path.clone();
            let p2 = path.clone();
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let (b1, b2) = (barrier.clone(), barrier);
            let holder = std::thread::spawn(move || {
                b1.wait();
                claim_request(&p1)
            });
            let newcomer = std::thread::spawn(move || {
                b2.wait();
                claim_request(&p2)
            });
            let (h, n) = (holder.join().unwrap(), newcomer.join().unwrap());
            assert!(h ^ n, "both or neither claimed the request (holder {h}, newcomer {n})");
        }
        assert!(!claim_request(&path), "a claim already made was made again");
        let _ = std::fs::remove_dir_all(&dir);

        use std::time::Duration;
        let t = HANDOVER_TIMEOUT;
        assert_eq!(next_step(Duration::ZERO, false, false), Step::Wait);
        assert_eq!(next_step(Duration::ZERO, false, true), Step::Stop, "a stop waits for no deadline");
        assert_eq!(next_step(t + HANDOVER_AGREED_WAIT, true, true), Step::Stop);
        assert_eq!(next_step(t, true, false), Step::WaitForAgreed);
        assert_eq!(next_step(t, false, false), Step::GiveUp { agreed: false });
        assert_eq!(next_step(t + HANDOVER_AGREED_WAIT, true, false), Step::GiveUp { agreed: true });

        // And both loops act on the claim's answer, not on a look: two real
        // processes cannot race in a unit test, so this part reads the two
        // functions, each bounded to its own body.
        let src = include_str!("instance.rs").replace("\r\n", "\n");
        let body = |name: &str| {
            let from = src.find(name).unwrap_or_else(|| panic!("{name}"));
            let rest = &src[from..];
            rest[..rest.find("\n}\n").expect("end")].to_string()
        };
        let watcher = body("pub fn spawn_handover_watcher(");
        assert!(
            watcher.contains("let asked = handover_path().exists() && claim_request(&claim_path());"),
            "the holder stands down without winning the claim"
        );
        let newcomer = body("pub fn request_handover(");
        assert!(
            newcomer.contains("Step::GiveUp { agreed: false } => {\n                if claim_request(&claim) {\n                    clear_request_files();\n                    return Ok(Handover::NotAnswered);"),
            "the newcomer gives up without taking the request back"
        );
        assert!(
            newcomer.contains("let holder_agreed = !claim_request(&claim);"),
            "a stop does not take the request back, or misreports who has it"
        );
        assert!(
            newcomer.contains(
                "let agreed = holder_agreed(path.exists(), claim.exists());\n        \
                 match next_step(start.elapsed(), agreed, stop.load(Ordering::SeqCst)) {"
            ),
            "the wait no longer reads the stop flag, or reads agreement from the request alone"
        );
        // That reading is only sound because a leftover claim was cleared
        // before asking -- or the ask refused: one that cannot be removed (a
        // directory stands in for a file held open) is an error.
        assert!(
            newcomer.contains("    clear_leftover_claim(&claim)?;\n    std::fs::write(&path,"),
            "the newcomer asks without clearing a leftover claim, or ignores a claim that will not go"
        );
        let dir = std::env::temp_dir().join(format!("egw_leftover_claim_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let claim = dir.join(CLAIM_FILE);
        assert!(clear_leftover_claim(&claim).is_ok(), "no leftover claim is not an error");
        std::fs::write(&claim, b"").unwrap();
        assert!(clear_leftover_claim(&claim).is_ok() && !claim.exists(), "a leftover claim is cleared");
        std::fs::create_dir(&claim).unwrap();
        assert!(clear_leftover_claim(&claim).is_err(), "a claim that will not go was ignored");
        // Said at once: only a sharing violation is waited on, and that is
        // Windows' alone.
        #[cfg(not(windows))]
        {
            let started = std::time::Instant::now();
            assert!(clear_leftover_claim(&claim).is_err());
            assert!(
                started.elapsed() < std::time::Duration::from_millis(100),
                "a claim that cannot clear was asked about again"
            );
        }
        std::fs::remove_dir(&claim).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        // A holder that claimed but could not delete the request has agreed:
        // read otherwise, the newcomer's give-up loses the claim on every pass.
        assert!(holder_agreed(true, true), "a claim made with the request still there is agreement");
        assert!(holder_agreed(false, false), "the request taken is agreement");
        assert!(!holder_agreed(true, false), "an untouched request is not agreement");
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
