//! The live screens a booted session offers, and the wiring that gets them to
//! a viewer.
//!
//! Nothing here is a *device*.  The devices are [`super::vdm`] (a character
//! grid at `CC00`) and [`super::dazzler`] (a colour picture anywhere in the
//! guest's memory); this module is the plumbing between a guest running on a
//! session task and a browser that arrives on the HTTP listener, which is the
//! only reason two things in one process cannot otherwise reach each other.
//!
//! **One entry per session, carrying whichever cards that guest has.**  Not one
//! entry per card, and the reason is a real machine rather than tidiness:
//! TDISK04 has a VDM-1 as its console *and* runs `KSCOPE`, which drives a
//! Dazzler.  Listing that session twice would offer the operator two names for
//! one seat at one computer.
//!
//! The whole "don't do work nobody is watching" design lives in one flag.  A
//! session checks it at every key-poll seam — thousands of times a second, so
//! it has to be cheap: one uncontended atomic swap, beside an uncontended lock
//! of the browser key queue at the same seam — and only samples memory when a
//! viewer has actually asked.  It is also self-pacing: one publish per request means a
//! browser polling every 150 ms costs seven snapshots a second and no timer
//! exists anywhere.

use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// The VDM-1 half of a snapshot: the card's window, and the one piece of state
/// that is not in memory.
#[derive(Clone)]
pub struct VdmPart {
    pub window: Box<[u8; super::vdm::WINDOW]>,
    pub scroll: u8,
    /// Has the guest ever written the scroll register?  A driver's own
    /// declaration, reported and never used to gate — see `vdm`.
    pub active: bool,
}

/// The Dazzler half: the picture's bytes, and the two registers that say how
/// to read them.
///
/// The bytes are carried rather than the rendered pixels because the format
/// register is *animated* — GDEMO rewrites it twenty-nine times while running —
/// so the picture and the way to interpret it have to travel together or a
/// viewer paints one frame's bytes under the next frame's mode.
#[derive(Clone)]
pub struct DazzlerPart {
    pub bytes: Vec<u8>,
    /// Port `0Eh` as the guest last wrote it: on-bit plus A15..A9.
    pub address: u8,
    /// Port `0Fh` as the guest last wrote it: the format.
    pub format: u8,
}

/// What a viewer sees in the list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listing {
    /// Stable for the life of the session; how a viewer names the screen.
    pub id: u64,
    /// The image, and where the person typing at it came from.
    pub label: String,
    /// This guest has driven the VDM-1's scroll register.
    pub vdm_active: bool,
    /// This guest has switched a Dazzler on.
    pub dazzler_on: bool,
    /// Has any frame been published yet?
    pub has_frame: bool,
}

/// A published screen: everything needed to draw what the guest can see.
#[derive(Clone)]
pub struct Snapshot {
    pub label: String,
    /// Bumped on every publish, so a viewer can tell a still screen from a
    /// stopped one without diffing a kilobyte.
    pub generation: u64,
    pub vdm: VdmPart,
    /// `None` until the guest addresses a Dazzler.  Absent rather than blank,
    /// because "this machine has no colour card" and "the card is showing
    /// black" are different facts and only one of them is worth a canvas.
    pub dazzler: Option<DazzlerPart>,
    /// Has this guest actually *read* the joystick board?
    ///
    /// Reported, never used to gate anything — the VDM-1's `C8h` lesson. It
    /// answers the one question a player cannot get from the picture: whether
    /// the program running is one that wants a joystick at all. A board is
    /// offered on every booted session because we cannot know in advance which
    /// disks use one, and this is how the page can stop looking broken when the
    /// answer is no.
    pub joystick_seen: bool,
}

struct Live {
    label: String,
    /// Set by a viewer asking for a frame, cleared by the session publishing
    /// one.
    wanted: AtomicBool,
    vdm_active: AtomicBool,
    dazzler_on: AtomicBool,
    frame: Mutex<Option<Snapshot>>,
    /// Keystrokes a viewer has typed, waiting for the session to collect them.
    ///
    /// The way *in*, mirroring the frame's way out, and it is only a queue
    /// because the two ends run on different tasks — the session drains it at
    /// the same seam it publishes, and hands each byte to the machine through
    /// the same call its own terminal's bytes go through.
    ///
    /// Bounded, and dropped rather than blocked when full: a viewer holding a
    /// key down while the guest is busy reading a track must not be able to
    /// grow this without limit, and a real keyboard's buffer overflows too.
    keys: Mutex<std::collections::VecDeque<u8>>,
    /// Everything any viewer is holding, OR-ed: the fast path's summary, so a
    /// seam with nothing held costs one atomic load and no lock.  Kept equal
    /// to the OR of `viewers`' masks by whoever changes `viewers`, under its
    /// lock.
    ///
    /// **A level, not a queue, and that is the whole distinction.** A keystroke
    /// is an event and is delivered once; a joystick is a position that persists
    /// until the hand moves. So this is stored rather than drained.
    joystick: AtomicU16,
    /// What each watching page is holding, and when it last said so (in
    /// [`crate::cpm::speed::now_ms`] milliseconds).
    ///
    /// **Per page, because two browsers are two players.** One shared mask,
    /// replaced by every report, meant the last page to post won: player 1's
    /// stick centred whenever player 2's page reported, and the reverse, so
    /// neither held steady.  Each page names itself (`v` on `/vdm/joy`) and the
    /// guest sees every live page's mask together.
    ///
    /// A page that has stopped talking -- closed, navigated away, or its
    /// network gone -- must not leave a stick held: its entry goes after
    /// [`JOYSTICK_IDLE_MS`].  Without that a lost key-up is permanent, and "the
    /// ship spins for ever" is the failure a level-based input has that a queue
    /// does not.  Bounded by [`MAX_JOYSTICK_VIEWERS`].
    viewers: Mutex<Vec<JoyViewer>>,
    /// Whether this screen's machine has the joystick board at all.  The key
    /// can be on while a machine has none -- z80pack never gets one -- and a
    /// page told so hands its ten letters back to the keyboard.
    board: AtomicBool,
}

/// How many typed bytes may wait for a busy guest.
///
/// The same order as the machine's own key queue: enough for a pasted command
/// line, not enough to matter if somebody leans on the keyboard.
const KEY_QUEUE_CAP: usize = 64;

/// Every live screen, in registration order.
///
/// A `Vec` rather than a map because the list is short — one entry per booted
/// session — and the order is the one the viewer's picker should show.
type ScreenList = Vec<(u64, Arc<Live>)>;

fn screens() -> &'static Mutex<ScreenList> {
    static S: OnceLock<Mutex<ScreenList>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(Vec::new()))
}

fn next_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// A session's claim on a slot in the live list.
///
/// Removes itself on drop, which is not a tidiness detail: a session ends by
/// `ESC ESC`, by the user hanging up, or by an error path, and a screen left
/// behind by any of those would sit in the list for ever showing a frame that
/// stopped changing.
pub struct Screen {
    id: u64,
    live: Arc<Live>,
}

impl Drop for Screen {
    fn drop(&mut self) {
        if let Ok(mut list) = screens().lock() {
            list.retain(|(id, _)| *id != self.id);
        }
    }
}

/// Register a live screen for a session, named for the viewer's list.
pub fn register(label: impl Into<String>) -> Screen {
    let id = next_id();
    let live = Arc::new(Live {
        label: label.into(),
        wanted: AtomicBool::new(false),
        vdm_active: AtomicBool::new(false),
        dazzler_on: AtomicBool::new(false),
        frame: Mutex::new(None),
        keys: Mutex::new(std::collections::VecDeque::new()),
        joystick: AtomicU16::new(0),
        viewers: Mutex::new(Vec::new()),
        board: AtomicBool::new(false),
    });
    if let Ok(mut list) = screens().lock() {
        list.push((id, live.clone()));
    }
    Screen { id, live }
}

impl Screen {
    /// Say whether this machine has the joystick board.
    pub fn set_board(&self, fitted: bool) {
        self.live.board.store(fitted, Ordering::Relaxed);
    }

    /// This screen's id, as the web UI names it.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Has a viewer asked for a frame since the last one was published?
    ///
    /// Takes the request as it reports it, so one poll produces one snapshot.
    pub fn wanted(&self) -> bool {
        self.live.wanted.swap(false, Ordering::Relaxed)
    }

    /// Collect what a viewer has typed since the last look.
    ///
    /// Returns the bytes and leaves the queue empty, so a keystroke is
    /// delivered exactly once however many sessions or seams go by.
    pub fn take_keys(&self) -> Vec<u8> {
        match self.live.keys.lock() {
            Ok(mut q) => q.drain(..).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// What the viewer is holding on the joystick.
    ///
    /// Read, never drained: a held direction has to still be held next time the
    /// session looks. Everything centres if the page has gone quiet for
    /// [`JOYSTICK_IDLE_MS`], so a closed tab releases the stick rather than
    /// leaving a guest with the helm hard over.
    pub fn joystick(&self) -> crate::cpm::d7a::Held {
        // **Nothing held is one atomic load and no clock.** The session pump
        // calls this at every seam, and reading a wall clock to find out that a
        // stick nobody is touching is still centred is a cost with no purchase.
        // Only when something is held does it read the clock and take the
        // pages' lock (see `joystick_at`).
        if self.live.joystick.load(Ordering::Acquire) == 0 {
            return crate::cpm::d7a::Held::none();
        }
        self.joystick_at(crate::cpm::speed::now_ms())
    }

    /// [`Screen::joystick`] with the clock supplied.
    ///
    /// **The idle release cannot be tested by backdating the stored stamp**, and
    /// finding that out is why this exists: `now_ms` counts from the first call
    /// in the process, so a few milliseconds in there is no "earlier" to move a
    /// report to — the subtraction saturates and the stick stays put. Injecting
    /// the clock tests the rule without a one-second sleep, the same choice the
    /// printer's idle close made.
    fn joystick_at(&self, now: u64) -> crate::cpm::d7a::Held {
        // Under the viewers' lock, which is also what a report takes: a page
        // that goes quiet is dropped here, and the summary is restated from
        // what remains, so the fast path's next read is already right.  A press
        // arriving meanwhile waits for the lock rather than being lost.
        let mut viewers = self.live.viewers.lock().unwrap_or_else(|e| e.into_inner());
        viewers.retain(|v| now.saturating_sub(v.at) <= JOYSTICK_IDLE_MS);
        let mask = viewers.iter().fold(0, |m, v| m | v.mask);
        self.live.joystick.store(mask, Ordering::Release);
        crate::cpm::d7a::Held::from_mask(mask)
    }

    /// Publish what the guest can see.
    ///
    /// The caller does the sampling because only it has the machine — and it
    /// must sample through the machine's own `peek`, so a banked guest is read
    /// through its MMU rather than out of the array behind it.
    pub fn publish(&self, vdm: VdmPart, dazzler: Option<DazzlerPart>, joystick_seen: bool) {
        self.live.vdm_active.store(vdm.active, Ordering::Relaxed);
        self.live.dazzler_on.store(dazzler.is_some(), Ordering::Relaxed);
        if let Ok(mut slot) = self.live.frame.lock() {
            let generation = slot.as_ref().map(|s| s.generation + 1).unwrap_or(1);
            *slot = Some(Snapshot {
                label: self.live.label.clone(),
                generation,
                vdm,
                dazzler,
                joystick_seen,
            });
        }
    }
}

/// Type at one screen's guest.
///
/// Separate from [`look`] because it is a different act with a different
/// consequence, and the caller gating it is the web listener — this module has
/// no opinion about who may type, only about how the bytes get there.
///
/// Returns false when the screen has gone, so a page left open across the end
/// of a session is told rather than typing into nothing.  Bytes beyond the
/// queue's cap are dropped, exactly as a real keyboard buffer overflows.
pub fn push_keys(id: u64, bytes: &[u8]) -> bool {
    let live = {
        let Ok(list) = screens().lock() else { return false };
        match list.iter().find(|(sid, _)| *sid == id) {
            Some((_, l)) => l.clone(),
            None => return false,
        }
    };
    if let Ok(mut q) = live.keys.lock() {
        for b in bytes {
            if q.len() >= KEY_QUEUE_CAP {
                break;
            }
            q.push_back(*b);
        }
    }
    true
}

/// One watching page's joystick: who, what it holds, when it last said so.
#[derive(Debug, Clone, Copy)]
struct JoyViewer {
    viewer: u64,
    mask: u16,
    at: u64,
}

/// How many pages may hold a stick on one screen at once.  Two players is
/// the board; a few more is somebody with tabs open.  A report from one more
/// replaces the page heard from longest ago, so the list cannot grow.
pub const MAX_JOYSTICK_VIEWERS: usize = 8;

/// How long a joystick report stands before everything centres.
///
/// Comfortably longer than the page's own report interval, so an ordinary late
/// request never drops a held direction, and short enough that a closed tab
/// lets go while the player is still looking at the screen.
pub const JOYSTICK_IDLE_MS: u64 = 1_000;

/// Does this screen's machine have the joystick board?  `None` when the
/// screen has gone.
pub fn has_board(id: u64) -> Option<bool> {
    let list = screens().lock().ok()?;
    list.iter().find(|(sid, _)| *sid == id).map(|(_, l)| l.board.load(Ordering::Relaxed))
}

/// Hold or release directions at one screen's guest.
///
/// Takes the **whole** set every time rather than a change, which is what makes
/// a dropped request harmless: the next one restates the truth. Bits outside
/// [`crate::cpm::d7a::bit::ALL`] are discarded rather than trusted.
///
/// Returns false when the screen has gone, like [`push_keys`], so a page left
/// open across the end of a session is told.
///
/// One page's report -- [`set_joystick_from`] with page `0`.  For the tests;
/// the route names its page.
#[cfg(test)]
pub fn set_joystick(id: u64, mask: u16) -> bool {
    set_joystick_from(id, 0, mask)
}

/// What page `viewer` is holding on screen `id`.  Each page's mask stands for
/// itself: the guest sees every live page's together, so two browsers are two
/// players (see `Live::viewers`).
pub fn set_joystick_from(id: u64, viewer: u64, mask: u16) -> bool {
    set_joystick_from_at(id, viewer, mask, crate::cpm::speed::now_ms())
}

/// [`set_joystick_from`] with the clock supplied, for the same reason the
/// reader takes one (see [`Screen::joystick_at`]).
fn set_joystick_from_at(id: u64, viewer: u64, mask: u16, now: u64) -> bool {
    let live = {
        let Ok(list) = screens().lock() else { return false };
        match list.iter().find(|(sid, _)| *sid == id) {
            Some((_, l)) => l.clone(),
            None => return false,
        }
    };
    let mask = mask & crate::cpm::d7a::bit::ALL;
    let mut viewers = live.viewers.lock().unwrap_or_else(|e| e.into_inner());
    match viewers.iter().position(|v| v.viewer == viewer) {
        // A release removes the page: nothing held needs no entry, and it
        // keeps the list to the pages actually holding something.
        Some(i) if mask == 0 => {
            viewers.swap_remove(i);
        }
        Some(i) => viewers[i] = JoyViewer { viewer, mask, at: now },
        None if mask == 0 => {}
        None => {
            if viewers.len() >= MAX_JOYSTICK_VIEWERS
                && let Some(oldest) = (0..viewers.len()).min_by_key(|&i| viewers[i].at)
            {
                viewers.swap_remove(oldest);
            }
            viewers.push(JoyViewer { viewer, mask, at: now });
        }
    }
    let all = viewers.iter().fold(0, |m, v| m | v.mask);
    live.joystick.store(all, Ordering::Release);
    true
}

/// Every live screen, oldest first.
pub fn list() -> Vec<Listing> {
    let Ok(list) = screens().lock() else { return Vec::new() };
    list.iter()
        .map(|(id, live)| Listing {
            id: *id,
            label: live.label.clone(),
            vdm_active: live.vdm_active.load(Ordering::Relaxed),
            dazzler_on: live.dazzler_on.load(Ordering::Relaxed),
            has_frame: live.frame.lock().map(|f| f.is_some()).unwrap_or(false),
        })
        .collect()
}

/// What asking after a screen can find.
///
/// Three outcomes rather than an `Option`, because "this session has ended" and
/// "this session has not been round its loop yet" want different words on the
/// screen and would otherwise both read as a blank display — the one state a
/// viewer cannot tell apart by looking.
pub enum Look {
    /// No such screen: the session ended, or never existed.
    Gone,
    /// Registered, but no frame published yet.  The next seam will publish one.
    Waiting { label: String },
    /// The latest published frame.
    Frame(Box<Snapshot>),
}

/// Ask one screen for its latest frame.
///
/// Asking is also how a session learns that anybody is watching, so a viewer
/// that stops polling costs a parked guest, per seam, one uncontended atomic
/// swap for the screen (beside the key-queue check every seam makes anyway) and
/// no copying.  The frame returned is the one published *before* this request; at a
/// 150 ms poll the display is at most one poll behind, which is the price of
/// not running a timer for a screen nobody has open.
pub fn look(id: u64) -> Look {
    let live = {
        let Ok(list) = screens().lock() else { return Look::Gone };
        match list.iter().find(|(sid, _)| *sid == id) {
            Some((_, l)) => l.clone(),
            None => return Look::Gone,
        }
    };
    live.wanted.store(true, Ordering::Relaxed);
    match live.frame.lock().ok().and_then(|f| f.clone()) {
        Some(s) => Look::Frame(Box::new(s)),
        None => Look::Waiting { label: live.label.clone() },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blank_vdm() -> VdmPart {
        VdmPart { window: Box::new([b' '; super::super::vdm::WINDOW]), scroll: 0, active: false }
    }

    #[test]
    fn test_a_registered_screen_is_listed_and_removed_on_drop() {
        let screen = register("TEST.DSK from 10.0.0.9");
        let id = screen.id();
        let listed = list();
        let mine = listed.iter().find(|l| l.id == id).expect("just registered");
        assert_eq!(mine.label, "TEST.DSK from 10.0.0.9");
        assert!(!mine.has_frame, "nothing published yet");
        assert!(!mine.vdm_active, "no C8h write seen yet");
        assert!(!mine.dazzler_on, "no Dazzler addressed yet");

        drop(screen);
        assert!(list().iter().all(|l| l.id != id), "a finished session leaves no screen");
    }

    /// The publish/poll handshake: a session that nobody is watching does no
    /// work, and one request produces exactly one snapshot.
    #[test]
    fn test_a_screen_is_only_sampled_when_somebody_asks() {
        let screen = register("TEST.DSK");
        assert!(!screen.wanted(), "nobody has asked yet");

        assert!(matches!(look(screen.id()), Look::Waiting { .. }));
        assert!(screen.wanted(), "the request was heard");
        assert!(!screen.wanted(), "and taken exactly once");

        screen.publish(blank_vdm(), None, false);
        let Look::Frame(snap) = look(screen.id()) else { panic!("published") };
        assert_eq!(snap.generation, 1);
        assert!(snap.dazzler.is_none());
    }

    /// "The session ended" and "the session has not painted yet" are different
    /// facts and both look like a blank screen, so they are different answers.
    #[test]
    fn test_a_screen_that_has_ended_is_not_a_screen_that_is_waiting() {
        let screen = register("TEST.DSK");
        let id = screen.id();
        assert!(matches!(look(id), Look::Waiting { .. }));
        drop(screen);
        assert!(matches!(look(id), Look::Gone));
    }

    #[test]
    fn test_the_generation_counter_separates_a_still_screen_from_a_stopped_one() {
        let screen = register("TEST.DSK");
        let generation_of = |id| match look(id) {
            Look::Frame(s) => s.generation,
            _ => panic!("published"),
        };
        screen.publish(blank_vdm(), None, false);
        let first = generation_of(screen.id());
        screen.publish(blank_vdm(), None, false);
        assert_eq!(
            generation_of(screen.id()),
            first + 1,
            "an identical frame is still a new frame"
        );
    }

    /// One session, both cards — the arrangement TDISK04 actually needs, since
    /// its console is a VDM-1 and `KSCOPE` on it drives a Dazzler.
    #[test]
    fn test_one_session_can_carry_both_cards() {
        let screen = register("TDISK04.DSK");
        let mut vdm = blank_vdm();
        vdm.active = true;
        screen.publish(
            vdm,
            Some(DazzlerPart { bytes: vec![0x0F; 512], address: 0x81, format: 0x30 }),
            false,
        );
        let listed = list();
        let mine = listed.iter().find(|l| l.id == screen.id()).expect("registered");
        assert!(mine.vdm_active && mine.dazzler_on, "both cards are reported");

        let Look::Frame(snap) = look(screen.id()) else { panic!("published") };
        assert!(snap.vdm.active);
        assert_eq!(snap.dazzler.as_ref().map(|d| d.address), Some(0x81));
    }

    /// A card switched off between frames must *disappear*, not linger as a
    /// black picture — a viewer cannot tell those apart by looking.
    #[test]
    fn test_a_dazzler_that_goes_away_stops_being_listed() {
        let screen = register("TEST.DSK");
        screen.publish(
            blank_vdm(),
            Some(DazzlerPart { bytes: vec![0; 512], address: 0x81, format: 0x30 }),
            false,
        );
        screen.publish(blank_vdm(), None, false);
        let listed = list();
        let mine = listed.iter().find(|l| l.id == screen.id()).expect("registered");
        assert!(!mine.dazzler_on);
        let Look::Frame(snap) = look(screen.id()) else { panic!("published") };
        assert!(snap.dazzler.is_none());
    }

    /// The way in, mirroring the way out: a viewer types, the session collects
    /// it once, and the queue is empty afterwards.
    #[test]
    fn test_a_viewer_can_type_and_the_session_collects_it_once() {
        let screen = register("TEST.DSK");
        assert!(screen.take_keys().is_empty(), "nobody has typed");

        assert!(push_keys(screen.id(), b"DIR\r"));
        assert_eq!(screen.take_keys(), b"DIR\r".to_vec());
        assert!(screen.take_keys().is_empty(), "and delivered exactly once");
    }

    /// Both keyboards feed one queue, in the order the bytes arrived — which is
    /// what two keyboards on one port do. Two people typing at once interleave,
    /// and that is a shared terminal rather than a defect.
    #[test]
    fn test_two_keyboards_share_one_queue() {
        let screen = register("TEST.DSK");
        push_keys(screen.id(), b"AB");
        push_keys(screen.id(), b"CD");
        assert_eq!(screen.take_keys(), b"ABCD".to_vec());
    }

    /// A viewer leaning on a key while the guest is busy reading a track must
    /// not be able to grow this without bound. A real keyboard buffer overflows
    /// too; what matters is that it stops rather than that nothing is lost.
    #[test]
    fn test_the_key_queue_is_bounded() {
        let screen = register("TEST.DSK");
        let flood = vec![b'x'; KEY_QUEUE_CAP * 4];
        assert!(push_keys(screen.id(), &flood));
        assert_eq!(screen.take_keys().len(), KEY_QUEUE_CAP);
    }

    /// Typing at a session that has ended is refused rather than silently
    /// dropped, so a page left open across the end of a session can say so.
    #[test]
    fn test_typing_at_a_finished_session_is_refused() {
        let screen = register("TEST.DSK");
        let id = screen.id();
        assert!(push_keys(id, b"X"));
        drop(screen);
        assert!(!push_keys(id, b"X"), "the screen has gone");
    }

    #[test]
    fn test_an_unknown_screen_id_is_not_an_error() {
        assert!(matches!(look(u64::MAX), Look::Gone), "a closed session's link just stops working");
    }

    /// **A joystick is a level, so it is read and not drained.** The whole
    /// difference from the key queue: a keystroke is delivered once, a held
    /// direction has to still be held next time the session looks.
    #[test]
    fn test_a_held_direction_survives_being_read() {
        use crate::cpm::d7a::bit;
        let screen = register("TEST.DSK");
        assert!(set_joystick(screen.id(), bit::P1_LEFT | bit::P1_FIRE));
        for read in 0..3 {
            let held = screen.joystick();
            assert!(held.left[0], "still pushed left on read {read}");
            assert!(held.fire[0], "and still firing on read {read}");
        }
        // Releasing is a report of its own, and zero is a real value: dropping
        // it as "nothing to say" would leave the stick over.
        assert!(set_joystick(screen.id(), 0));
        assert_eq!(screen.joystick(), crate::cpm::d7a::Held::none(), "an empty mask centres all");
    }

    /// Bits we do not define must not become directions.
    #[test]
    fn test_a_stray_bit_is_discarded() {
        use crate::cpm::d7a::bit;
        let screen = register("TEST.DSK");
        assert!(set_joystick(screen.id(), 0xFFFF));
        let held = screen.joystick();
        assert_eq!(
            crate::cpm::d7a::Held::from_mask(bit::ALL),
            held,
            "everything defined is held, and nothing else was invented",
        );
    }

    /// **A page that stops talking must let go of the stick.** This is the one
    /// failure a level has and a queue does not: a key-up that never arrives —
    /// a closed tab, a lost network — would otherwise hold the helm over for
    /// the rest of the session.
    #[test]
    fn test_a_silent_page_releases_the_stick() {
        use crate::cpm::d7a::bit;
        let screen = register("TEST.DSK");
        assert!(set_joystick(screen.id(), bit::P1_RIGHT));
        assert_ne!(screen.joystick(), crate::cpm::d7a::Held::none(), "held while talking");
        // Read the stick at a supplied time past the idle window rather than
        // sleeping for it -- backdating the stored stamp cannot work, see
        // `joystick_at`.
        let live = {
            let list = screens().lock().unwrap();
            list.iter().find(|(id, _)| *id == screen.id()).map(|(_, l)| l.clone()).unwrap()
        };
        let later = crate::cpm::speed::now_ms() + JOYSTICK_IDLE_MS + 1;
        assert_eq!(
            screen.joystick_at(later),
            crate::cpm::d7a::Held::none(),
            "silence centres it",
        );
        // And it is latched off, so the next read costs no clock.
        assert_eq!(live.joystick.load(Ordering::Relaxed), 0);
    }

    /// **A press after the idle release is honoured.**
    ///
    /// The release is not sticky: a report after it works.  (A press landing
    /// while the reader prunes waits for the pages' lock rather than racing
    /// it, so there is no window in which it can be thrown away.)
    #[test]
    fn test_a_press_after_the_idle_release_is_honoured() {
        use crate::cpm::d7a::bit;
        let screen = register("TEST.DSK");
        assert!(set_joystick(screen.id(), bit::P1_LEFT));
        let later = crate::cpm::speed::now_ms() + JOYSTICK_IDLE_MS + 1;
        assert_eq!(screen.joystick_at(later), crate::cpm::d7a::Held::none(), "released");

        // A new press after the latch is honoured, not swallowed by it.
        assert!(set_joystick(screen.id(), bit::P1_RIGHT));
        let held = screen.joystick();
        assert!(held.right[0], "the press after a release must reach the guest");
        assert!(!held.left[0], "and it is the new direction, not the old one");
    }

    /// **Two browsers are two players.**  One shared mask, replaced by every
    /// report, meant the last page to post won: each page's heartbeat centred
    /// the other player's stick, so neither held steady, and one page letting
    /// go released the other's.  Each page's mask now stands for itself.
    #[test]
    fn test_two_pages_are_two_players() {
        use crate::cpm::d7a::bit;
        let screen = register("TEST.DSK");
        assert!(set_joystick_from(screen.id(), 11, bit::P1_UP));
        assert!(set_joystick_from(screen.id(), 22, bit::P2_LEFT));
        let both = screen.joystick();
        assert!(both.up[0] && both.left[1], "one page's report replaced the other's: {both:?}");
        // Player 1's heartbeat restates their own stick, not the board.
        assert!(set_joystick_from(screen.id(), 11, bit::P1_UP));
        assert!(screen.joystick().left[1], "a heartbeat centred the other player");
        // Player 2 lets go; player 1 is still holding.
        assert!(set_joystick_from(screen.id(), 22, 0));
        let after = screen.joystick();
        assert!(after.up[0] && !after.left[1], "a release reached the wrong player: {after:?}");
    }

    /// A page that goes quiet lets go of *its* stick, and only its own.
    #[test]
    fn test_a_silent_page_lets_go_of_its_own_stick_only() {
        use crate::cpm::d7a::bit;
        let screen = register("TEST.DSK");
        let t0 = crate::cpm::speed::now_ms();
        assert!(set_joystick_from_at(screen.id(), 1, bit::P1_RIGHT, t0));
        assert!(set_joystick_from_at(screen.id(), 2, bit::P2_DOWN, t0 + 800));
        let held = screen.joystick_at(t0 + JOYSTICK_IDLE_MS + 200);
        assert!(!held.right[0], "the silent page still holds its stick");
        assert!(held.down[1], "the page still talking was released with it");
    }

    /// The pages are bounded: one more than [`MAX_JOYSTICK_VIEWERS`] replaces
    /// the page heard from longest ago, and the newcomer is held.
    #[test]
    fn test_joystick_pages_are_bounded() {
        use crate::cpm::d7a::bit;
        let screen = register("TEST.DSK");
        let t0 = crate::cpm::speed::now_ms();
        for v in 0..MAX_JOYSTICK_VIEWERS as u64 {
            assert!(set_joystick_from_at(screen.id(), v, bit::P1_FIRE, t0 + v));
        }
        assert!(set_joystick_from_at(screen.id(), 99, bit::P2_FIRE, t0 + 50));
        let live = {
            let list = screens().lock().unwrap();
            list.iter().find(|(id, _)| *id == screen.id()).map(|(_, l)| l.clone()).unwrap()
        };
        let viewers = live.viewers.lock().unwrap();
        assert_eq!(viewers.len(), MAX_JOYSTICK_VIEWERS);
        assert!(!viewers.iter().any(|v| v.viewer == 0), "the oldest page was kept");
        assert!(viewers.iter().any(|v| v.viewer == 99), "the newcomer was refused");
    }

    /// A page left open across the end of a session is told, like typing is.
    #[test]
    fn test_holding_at_a_screen_that_has_gone() {
        let id = {
            let screen = register("TEST.DSK");
            screen.id()
        };
        assert!(!set_joystick(id, 1), "the screen went with its session");
    }
}
