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
//! it has to be exactly that cheap — and only samples memory when a viewer has
//! actually asked.  It is also self-pacing: one publish per request means a
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
    /// What the viewer is holding on the joystick, as a mask of
    /// [`crate::cpm::d7a::bit`] flags.
    ///
    /// **A level, not a queue, and that is the whole distinction.** A keystroke
    /// is an event and is delivered once; a joystick is a position that persists
    /// until the hand moves. So this is stored rather than drained, and it is an
    /// atomic rather than a mutex because the session reads it at every seam
    /// while a viewer writes it only when a key goes down or up.
    joystick: AtomicU16,
    /// When the page last said anything about the joystick, in
    /// [`crate::cpm::speed::now_ms`] milliseconds.
    ///
    /// A page that has stopped talking — closed, navigated away, or its network
    /// gone — must not leave a stick held. Without this a lost key-up is
    /// permanent, and "the ship spins for ever" is the failure a level-based
    /// input has that a queue does not.
    joystick_at: AtomicU64,
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
        joystick_at: AtomicU64::new(0),
    });
    if let Ok(mut list) = screens().lock() {
        list.push((id, live.clone()));
    }
    Screen { id, live }
}

impl Screen {
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
        // The mask is checked again inside, which is cheaper than the
        // `Instant::elapsed` this avoids.
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
        // `Acquire`, paired with the writer's `Release` on this same field: it
        // is what makes the timestamp read below certain to be the one that
        // belongs to this mask. See `set_joystick` for the race that needs it.
        let mask = self.live.joystick.load(Ordering::Acquire);
        if mask == 0 {
            return crate::cpm::d7a::Held::none();
        }
        let at = self.live.joystick_at.load(Ordering::Acquire);
        if now.saturating_sub(at) > JOYSTICK_IDLE_MS {
            // Latch it off, so this costs one comparison rather than a clock
            // read on every seam thereafter.
            //
            // **Conditionally**, not a plain store: between the decision above
            // and this line a viewer can press a key, and a blind `store(0)`
            // would throw that press away. Zero it only if it is still the mask
            // we judged.
            let _ = self.live.joystick.compare_exchange(
                mask,
                0,
                Ordering::AcqRel,
                Ordering::Relaxed,
            );
            return crate::cpm::d7a::Held::none();
        }
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

/// How long a joystick report stands before everything centres.
///
/// Comfortably longer than the page's own report interval, so an ordinary late
/// request never drops a held direction, and short enough that a closed tab
/// lets go while the player is still looking at the screen.
pub const JOYSTICK_IDLE_MS: u64 = 1_000;

/// Hold or release directions at one screen's guest.
///
/// Takes the **whole** set every time rather than a change, which is what makes
/// a dropped request harmless: the next one restates the truth. Bits outside
/// [`crate::cpm::d7a::bit::ALL`] are discarded rather than trusted.
///
/// Returns false when the screen has gone, like [`push_keys`], so a page left
/// open across the end of a session is told.
pub fn set_joystick(id: u64, mask: u16) -> bool {
    let live = {
        let Ok(list) = screens().lock() else { return false };
        match list.iter().find(|(sid, _)| *sid == id) {
            Some((_, l)) => l.clone(),
            None => return false,
        }
    };
    // **The timestamp goes first, and the orderings are not decoration.**
    // These are two atomics holding one fact, and the reader checks the mask
    // before the time. Written the other way round -- mask first -- a reader
    // could see a *fresh* mask beside a *stale* timestamp, decide the page had
    // gone quiet, and throw the keypress away; with `Relaxed` on both, the
    // compiler and the processor are free to produce exactly that even from
    // source in this order. Storing the time first and releasing the mask means
    // a reader that has acquired this mask has necessarily also seen its time.
    //
    // It matters most at the moment it is least forgivable: a page that has been
    // open and idle carries an old timestamp, so the *first* key of a game is
    // the one at risk, and the symptom would be a press that does nothing until
    // the next heartbeat.
    live.joystick_at.store(crate::cpm::speed::now_ms(), Ordering::Release);
    live.joystick.store(mask & crate::cpm::d7a::bit::ALL, Ordering::Release);
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
/// that stops polling costs a parked guest one atomic load per seam and nothing
/// else.  The frame returned is the one published *before* this request; at a
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
        // Backdate the report past the idle window rather than sleeping for it.
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
    /// Note what this does *not* cover, because the first version of this
    /// comment claimed it did: the race is a press landing *between* the
    /// judgement and the latch, and no single-threaded test can produce that
    /// interleaving. Replacing the conditional latch with a blind
    /// `store(0)` leaves this test passing — measured, not assumed — so the
    /// conditional store is pinned from the source in
    /// `test_the_joystick_timestamp_is_published_before_the_mask` instead. What
    /// this test does hold is the part a caller can see: the latch is not
    /// sticky, and a report after it works.
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

    /// The two atomics hold one fact, so the writer must publish the time
    /// *before* the mask — a reader checks the mask first, and a fresh mask
    /// beside a stale time reads as silence. Pinned from the source because the
    /// consequence is a dropped keypress that no single-threaded test can see.
    #[test]
    fn test_the_joystick_timestamp_is_published_before_the_mask() {
        let src = include_str!("screen.rs");
        let start = src.find("pub fn set_joystick(id: u64, mask: u16)").expect("the fn");
        let body = &src[start..];
        let end = body.find("\n}\n").expect("the end") + start;
        let code: String = src[start..end]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let at = code.find("joystick_at.store").expect("the timestamp store");
        let mask = code.find("joystick.store").expect("the mask store");
        assert!(
            at < mask,
            "the mask is published before its timestamp, so a reader can see a fresh press \
             beside a stale time and discard it",
        );
        assert!(code.contains("Ordering::Release"), "and both stores must release");

        // The other half of the same race, and pinned here for the same reason:
        // a blind `store(0)` in the idle latch discards a press that arrives
        // while the reader is deciding, and every behavioural test still passes
        // with one -- which was checked by putting one back.
        let start = src.find("fn joystick_at(&self, now: u64)").expect("the reader");
        let body = &src[start..];
        let end = body.find("\n    }\n").expect("the end") + start;
        let reader: String = src[start..end]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            reader.contains("compare_exchange"),
            "the idle latch must zero the mask only if it is still the one it judged; an \
             unconditional store throws away a press that landed in between",
        );
        assert!(
            !reader.contains("joystick.store("),
            "an unconditional store to the mask is exactly what must not be here",
        );
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
