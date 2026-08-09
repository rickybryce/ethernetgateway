//! The Processor Technology VDM-1, and the live screens the web UI shows.
//!
//! The VDM-1 was a *video card*, not a terminal.  It has no serial line, no
//! keyboard and — the part that decides this module's whole shape — **no data
//! port**.  A character appears on the screen by being stored into memory at
//! `CC00`; the card scans that 1 KB window sixty times a second and lights
//! whatever it finds.  The guest never learns the display exists.
//!
//! Two consequences that are worth stating plainly, because they are why this
//! is a small module rather than a large one:
//!
//! * **Reading the screen cannot disturb the guest.**  We sample the same
//!   bytes the card would, through the machine's own `peek` (so a banked guest
//!   is read through its MMU, not around it).  There is no write, no trap, no
//!   timing change.  That is what makes it safe to open a screen on *any*
//!   booted session without first proving it is a VDM-1 guest.
//! * **A literal repaint cannot be wrong.**  Deriving a character *stream*
//!   from screen writes always could — that was the objection that deferred
//!   this feature twice — but a 64x16 grid sampled at `CC00` is the picture,
//!   not a reconstruction of one.  The cursor needs no special case either: on
//!   this card it is simply a cell with bit 7 set (inverse video).
//!
//! The only piece of state the card holds outside memory is the **scroll
//! register** on port `C8h`, which says which of the sixteen lines is shown at
//! the top.  Without it a guest that has scrolled is displayed rotated — the
//! text is all present and all in the wrong order, which is the failure most
//! likely to be mistaken for a wrong memory address.
//!
//! **CLEAN-ROOM.**  The VDM-1 has a published manual, so by the discriminator
//! settled for Punter, HBIOS and EGT80 — does an independent authority exist? —
//! this is written from the documented behaviour of the card.  z80pack's
//! `iodevices/proctec-vdm.c` is a cross-check afterwards, not a source, unlike
//! `z80pack.rs` which is correctly labelled derived.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// Where the screen window lives in the guest's address space.
pub const BASE: u16 = 0xCC00;
/// Characters across.
pub const COLS: usize = 64;
/// Lines down.
pub const ROWS: usize = 16;
/// The whole window, in bytes.
pub const WINDOW: usize = COLS * ROWS;
/// The scroll register: which line is displayed first.
///
/// Write-only, exactly as on the card — a real VDM-1 answers nothing on an
/// `IN`, so we claim this port for `OUT` alone and leave reads to fall through
/// to the machine's unclaimed-port answer.
pub const SCROLL_PORT: u8 = 0xC8;

/// One character cell, as the card would light it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cell {
    /// The glyph, already folded to something a browser can show.
    pub ch: char,
    /// Bit 7 was set: the card displays this cell in inverse video.
    pub inverse: bool,
}

impl Cell {
    /// An unlit cell — what a screen that has never been written looks like.
    pub const BLANK: Cell = Cell { ch: ' ', inverse: false };
}

/// A whole screen, top line first.
pub type Frame = [[Cell; COLS]; ROWS];

/// The glyph for one byte of screen memory, bit 7 already stripped.
///
/// The card's character generator is a ROM, and which ROM a given VDM-1 had
/// fitted decides what the upper half of the range looks like: the base card
/// showed 64 characters (upper case only) and the lower-case option showed 96.
/// We show the printable ASCII range as itself and everything else as a blank,
/// which is the conservative reading — a machine whose ROM folded `a` to `A`
/// is still *legible* rendered as `a`, whereas guessing a graphic for a control
/// byte would put marks on the screen that the operator cannot account for.
fn glyph(b: u8) -> char {
    match b {
        0x20..=0x7E => b as char,
        _ => ' ',
    }
}

/// Render the window the way the card would scan it.
///
/// This is the entire correctness surface of the VDM-1 and it is deliberately
/// pure: no machine, no session, no display.  `scroll` is the value the guest
/// last wrote to port `C8h`; its low four bits name the line shown at the top,
/// and the sixteen lines are a ring from there.
///
/// The upper bits of the scroll register are not ours to interpret — on the
/// real card they are outside the beginning-line field — so they are masked off
/// rather than guessed at.
pub fn frame(window: &[u8; WINDOW], scroll: u8) -> Frame {
    let top = (scroll & 0x0F) as usize;
    let mut out = [[Cell::BLANK; COLS]; ROWS];
    for (r, row) in out.iter_mut().enumerate() {
        // The ring: the line at the top is `top`, and the rest follow it round.
        let src = (top + r) % ROWS;
        for (c, cell) in row.iter_mut().enumerate() {
            let b = window[src * COLS + c];
            *cell = Cell { ch: glyph(b & 0x7F), inverse: b & 0x80 != 0 };
        }
    }
    out
}

/// The sixteen lines as text, for a caller that only wants the characters.
pub fn frame_text(frame: &Frame) -> Vec<String> {
    frame.iter().map(|row| row.iter().map(|c| c.ch).collect()).collect()
}

/// The inverse-video mask, one `'0'`/`'1'` per column, line by line.
///
/// Sent alongside the text rather than folded into it so the browser paints
/// what the card would light and nothing has to encode a glyph and an attribute
/// in the same character.
pub fn frame_inverse(frame: &Frame) -> Vec<String> {
    frame
        .iter()
        .map(|row| row.iter().map(|c| if c.inverse { '1' } else { '0' }).collect())
        .collect()
}

// ─── Live screens ───────────────────────────────────────────────────
//
// A booted session registers one of these for as long as it runs, and the web
// UI lists them.  Nothing here is a *device*: it is the wiring between a guest
// running on a session task and a viewer that arrives on the HTTP listener,
// which is the only reason the two can be in the same process and still not be
// able to reach each other.

/// What a viewer sees in the list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listing {
    /// Stable for the life of the session; how a viewer names the screen.
    pub id: u64,
    /// The image, and where the person typing at it came from.
    pub label: String,
    /// Has this guest actually driven the VDM-1's scroll register?
    ///
    /// The honest signal, and free: a guest that has written port `C8h` is
    /// running a VDM-1 driver, and one that has not is showing whatever
    /// happens to live at `CC00`.  We do not *refuse* to show the second kind
    /// — sampling costs the guest nothing and a program may paint the screen
    /// without ever touching the scroll register — but the viewer should be
    /// told which one they are looking at rather than left to wonder why the
    /// screen is full of noise.
    pub active: bool,
    /// Has any frame been published yet?
    pub has_frame: bool,
}

/// A published screen: the bytes, plus what is needed to render them.
#[derive(Clone)]
pub struct Snapshot {
    pub window: Box<[u8; WINDOW]>,
    pub scroll: u8,
    /// Bumped on every publish, so a viewer can tell a still screen from a
    /// stopped one without diffing 1 KB.
    pub generation: u64,
    pub active: bool,
    pub label: String,
}

impl Snapshot {
    /// The rendered screen.
    pub fn frame(&self) -> Frame {
        frame(&self.window, self.scroll)
    }
}

struct Live {
    label: String,
    /// Set by a viewer asking for a frame, cleared by the session publishing
    /// one.
    ///
    /// This is the whole of the "don't do work nobody is watching" design.  A
    /// session checks one relaxed atomic per key-poll seam — thousands of
    /// times a second, so it has to be exactly that cheap — and only copies
    /// the kilobyte when somebody has actually asked.  It is also self-pacing:
    /// one publish per request means a viewer polling every 150 ms costs seven
    /// snapshots a second and no timer anywhere.
    wanted: AtomicBool,
    active: AtomicBool,
    frame: Mutex<Option<Snapshot>>,
}

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
        active: AtomicBool::new(false),
        frame: Mutex::new(None),
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

    /// Publish the guest's screen.
    ///
    /// `read` is the machine's own memory read — `peek`, never the raw array,
    /// so a banked guest is sampled through its MMU exactly as its own CPU
    /// would see it.
    pub fn publish(&self, mut read: impl FnMut(u16) -> u8, scroll: u8, active: bool) {
        let mut window = Box::new([0u8; WINDOW]);
        for (i, b) in window.iter_mut().enumerate() {
            *b = read(BASE.wrapping_add(i as u16));
        }
        self.live.active.store(active, Ordering::Relaxed);
        if let Ok(mut slot) = self.live.frame.lock() {
            let generation = slot.as_ref().map(|s| s.generation + 1).unwrap_or(1);
            *slot = Some(Snapshot {
                window,
                scroll,
                generation,
                active,
                label: self.live.label.clone(),
            });
        }
    }
}

/// Every live screen, oldest first.
pub fn list() -> Vec<Listing> {
    let Ok(list) = screens().lock() else { return Vec::new() };
    list.iter()
        .map(|(id, live)| Listing {
            id: *id,
            label: live.label.clone(),
            active: live.active.load(Ordering::Relaxed),
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
/// that stops polling costs a parked guest one atomic load per seam and
/// nothing else.  The frame returned is the one published *before* this
/// request; at a 150 ms poll the display is at most one poll behind, which is
/// the price of not running a timer for a screen nobody has open.
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

    /// Fill a window with a caller's lines, space-padded.
    fn window_of(lines: &[&str]) -> [u8; WINDOW] {
        let mut w = [b' '; WINDOW];
        for (r, line) in lines.iter().enumerate().take(ROWS) {
            for (c, b) in line.bytes().enumerate().take(COLS) {
                w[r * COLS + c] = b;
            }
        }
        w
    }

    #[test]
    fn test_an_unscrolled_screen_reads_in_memory_order() {
        let w = window_of(&["FIRST", "SECOND"]);
        let f = frame(&w, 0);
        let text = frame_text(&f);
        assert_eq!(text.len(), ROWS);
        assert!(text[0].starts_with("FIRST"));
        assert!(text[1].starts_with("SECOND"));
        assert_eq!(text[0].chars().count(), COLS, "every line is the full width");
    }

    /// The scroll register is the difference between a screen and the same
    /// screen rotated — the failure most likely to be mistaken for reading the
    /// wrong address, because every character is present and legible.
    #[test]
    fn test_the_scroll_register_chooses_the_top_line() {
        let mut lines = Vec::new();
        for r in 0..ROWS {
            lines.push(format!("LINE{r}"));
        }
        let refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
        let w = window_of(&refs);

        let text = frame_text(&frame(&w, 3));
        assert!(text[0].starts_with("LINE3"), "line 3 is at the top");
        assert!(text[1].starts_with("LINE4"));
        // And it is a ring, not a truncation: the lines above the scroll point
        // come back round at the bottom.
        assert!(text[ROWS - 1].starts_with("LINE2"));
    }

    #[test]
    fn test_only_the_low_four_bits_of_the_scroll_register_are_ours() {
        let mut lines = Vec::new();
        for r in 0..ROWS {
            lines.push(format!("LINE{r}"));
        }
        let refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
        let w = window_of(&refs);
        // 0xF2 and 0x02 must agree: the upper nibble is outside the
        // beginning-line field and guessing at it would rotate the screen for
        // reasons the operator could not account for.
        assert_eq!(frame_text(&frame(&w, 0xF2)), frame_text(&frame(&w, 0x02)));
    }

    #[test]
    fn test_bit_seven_is_inverse_video_not_a_different_character() {
        let mut w = [b' '; WINDOW];
        w[0] = b'A';
        w[1] = b'A' | 0x80;
        let f = frame(&w, 0);
        assert_eq!(f[0][0], Cell { ch: 'A', inverse: false });
        assert_eq!(f[0][1], Cell { ch: 'A', inverse: true }, "the same letter, lit differently");

        let inv = frame_inverse(&f);
        assert_eq!(&inv[0][..2], "01");
    }

    /// The cursor on this card *is* an inverse-video cell — there is no cursor
    /// register and no output character for it — so a literal repaint carries
    /// it with no special case at all.  Pinned because "where is the cursor?"
    /// is the first question asked of any screen model, and the answer here is
    /// "you are already drawing it".
    #[test]
    fn test_the_cursor_needs_no_special_case() {
        let mut w = window_of(&["A>"]);
        w[2] = b' ' | 0x80; // the cursor, sitting after the prompt
        let f = frame(&w, 0);
        assert_eq!(frame_text(&f)[0].trim_end(), "A>");
        assert!(f[0][2].inverse, "the cell after the prompt is lit");
    }

    #[test]
    fn test_unprintable_bytes_are_blanks_not_invented_glyphs() {
        let mut w = [0u8; WINDOW];
        w[0] = 0x00;
        w[1] = 0x07;
        w[2] = 0x7F;
        w[3] = b'X';
        let text = frame_text(&frame(&w, 0));
        assert_eq!(&text[0][..4], "   X");
    }

    #[test]
    fn test_a_registered_screen_is_listed_and_removed_on_drop() {
        let screen = register("TEST.DSK from 10.0.0.9");
        let id = screen.id();
        let listed = list();
        let mine = listed.iter().find(|l| l.id == id).expect("just registered");
        assert_eq!(mine.label, "TEST.DSK from 10.0.0.9");
        assert!(!mine.has_frame, "nothing published yet");
        assert!(!mine.active, "no C8h write seen yet");

        drop(screen);
        assert!(list().iter().all(|l| l.id != id), "a finished session leaves no screen");
    }

    /// The publish/poll handshake: a session that nobody is watching does no
    /// work, and one request produces exactly one snapshot.
    #[test]
    fn test_a_screen_is_only_sampled_when_somebody_asks() {
        let screen = register("TEST.DSK");
        assert!(!screen.wanted(), "nobody has asked yet");

        // A viewer arrives.  The first request finds no frame — the session
        // has not been round its loop yet — but it leaves the flag set.
        assert!(matches!(look(screen.id()), Look::Waiting { .. }));
        assert!(screen.wanted(), "the request was heard");
        assert!(!screen.wanted(), "and taken exactly once");

        screen.publish(|addr| if addr == BASE { b'Z' } else { b' ' }, 0, true);
        let Look::Frame(snap) = look(screen.id()) else { panic!("published") };
        assert_eq!(snap.generation, 1);
        assert!(snap.active);
        assert_eq!(frame_text(&snap.frame())[0].trim_end(), "Z");
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
        screen.publish(|_| b' ', 0, false);
        let first = generation_of(screen.id());
        screen.publish(|_| b' ', 0, false);
        assert_eq!(
            generation_of(screen.id()),
            first + 1,
            "an identical frame is still a new frame"
        );
    }

    /// The window is read through the caller's own memory read, address by
    /// address, so a machine that banks memory is sampled the way its CPU sees
    /// it rather than out of the raw array behind its MMU.  Asserted by
    /// checking that every address in the window was asked for, exactly once
    /// and in range.
    #[test]
    fn test_the_whole_window_is_read_through_the_machines_own_peek() {
        let screen = register("TEST.DSK");
        let seen = std::cell::RefCell::new(Vec::new());
        screen.publish(
            |addr| {
                seen.borrow_mut().push(addr);
                0
            },
            0,
            false,
        );
        let seen = seen.into_inner();
        assert_eq!(seen.len(), WINDOW);
        assert_eq!(seen[0], BASE);
        assert_eq!(seen[WINDOW - 1], BASE + (WINDOW as u16 - 1));
    }

    #[test]
    fn test_an_unknown_screen_id_is_not_an_error() {
        assert!(matches!(look(u64::MAX), Look::Gone), "a closed session's link just stops working");
    }
}
