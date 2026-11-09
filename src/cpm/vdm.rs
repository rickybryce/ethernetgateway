//! The Processor Technology VDM-1 — the display of a booted disk.
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
//! register** on port `C8h`, which says which of the sixteen lines is shown
//! first and on which screen row (see [`frame`]).  Without it a guest that has scrolled is displayed rotated — the
//! text is all present and all in the wrong order, which is the failure most
//! likely to be mistaken for a wrong memory address.
//!
//! **CLEAN-ROOM.**  The VDM-1 has a published manual, so by the discriminator
//! settled for Punter, HBIOS and EGT8080 — does an independent authority exist? —
//! this is written from the documented behaviour of the card: the Processor
//! Technology *VDM-1 Video Display Module* manual, April 1976 (copy kept in
//! `.claude/specs/vdm1/`).  z80pack's `iodevices/proctec-vdm.c` is not a source,
//! unlike `z80pack.rs` which is correctly labelled derived.  (The scroll rule
//! below was once taken from it, and the manual showed it wrong.)

/// Where the screen window lives in the guest's address space.
pub const BASE: u16 = 0xCC00;
/// Characters across.
pub const COLS: usize = 64;
/// Lines down.
pub const ROWS: usize = 16;
/// The whole window, in bytes.
pub const WINDOW: usize = COLS * ROWS;
/// The scroll register: which line is displayed first, and on which row.
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
/// last wrote to port `C8h`.  The manual's Figure 2-11 names its two halves:
///
/// * bits 3-0, **"Beginning Display Line Absolute Address"**: the line of
///   memory shown first, with the sixteen lines a ring from there;
/// * bits 7-4, **"First Displayed Screen Position"**: the screen row that
///   first line is shown on.  The rows above it are black.
///
/// **The text moves down; it is not cut off.**  Section 2.7.4's test sets bits
/// 4-7 and expects "only the bottom line on the screen" to show, the display
/// moving "up and down the screen with a black area above it", while "the first
/// line on display, regardless of its vertical position on the screen, should
/// remain the same".  Section 3.1.7 is the circuit: the line counter is loaded
/// with the low nibble when the black band (the "window shade") ends, and
/// counts on from there.  This once followed z80pack instead, which blanks the
/// rows above and moves nothing -- every line shown was the wrong one.
pub fn frame(window: &[u8; WINDOW], scroll: u8) -> Frame {
    let top = (scroll & 0x0F) as usize;
    let first = (scroll >> 4) as usize;
    let mut out = [[Cell::BLANK; COLS]; ROWS];
    for (r, row) in out.iter_mut().enumerate().skip(first) {
        // Screen row `first` shows line `top`, and the rest follow it round;
        // what would fall below the last row is simply not shown.
        let src = (top + r - first) % ROWS;
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

    /// **The upper nibble moves the display down under a black band.**  The
    /// manual's own scroll test (section 2.7.4, Figure 2-11): the screen row
    /// the first line appears on comes from bits 7-4, the rows above it are
    /// black, and "the first line on display ... should remain the same".  With
    /// all four set, "only the bottom line on the screen should be displayed".
    #[test]
    fn test_the_upper_nibble_moves_the_display_down_under_a_black_band() {
        let mut lines = Vec::new();
        for r in 0..ROWS {
            lines.push(format!("LINE{r}"));
        }
        let refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
        let w = window_of(&refs);
        let plain = frame_text(&frame(&w, 0x02));
        let moved = frame_text(&frame(&w, 0x32));
        for (r, row) in moved.iter().enumerate().take(3) {
            assert!(row.trim().is_empty(), "row {r} still shows {row:?}");
        }
        assert!(moved[3].starts_with("LINE2"), "the first line moved down with the band: {:?}", moved[3]);
        assert_eq!(&moved[3..], &plain[..ROWS - 3], "the display moved down as a whole");

        let bottom = frame_text(&frame(&w, 0xF0));
        assert!(bottom[..ROWS - 1].iter().all(|r| r.trim().is_empty()), "only the bottom row shows");
        assert!(bottom[ROWS - 1].starts_with("LINE0"), "and it is the first line: {:?}", bottom[ROWS - 1]);
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
}
