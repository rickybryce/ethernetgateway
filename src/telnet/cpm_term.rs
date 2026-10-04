//! ADM-3A terminal translation for the CP/M emulator.
//!
//! CP/M full-screen software (WordStar, Turbo Pascal, dBASE, …) is installed
//! for one specific terminal and emits that terminal's control codes.  The
//! emulator presents a single virtual terminal — the **Lear Siegler
//! ADM-3A**, the universal lowest-common-denominator CP/M terminal — and
//! translates its output into whatever the *connected* client speaks (ANSI
//! for a modern terminal, PETSCII for a C64, best-effort for a dumb ASCII
//! TTY).  Client cursor keys are translated the other way, into the ADM-3A
//! codes the program expects.  Users install their `.COM` software "for an
//! ADM-3A."
//!
//! The decoder is a small state machine (the ADM-3A cursor-address sequence
//! `ESC = <row+0x20> <col+0x20>` spans several bytes that a program may emit
//! across separate BDOS calls, so the partial state must persist).  Both the
//! decoder and the per-terminal renderers are pure and unit-tested here with
//! no live session.
use super::TerminalType;
use super::colors::ascii_to_petscii_byte;

/// PETSCII control bytes used by the C64 renderer.
const PET_CLEAR: u8 = 0x93;
const PET_HOME: u8 = 0x13;
const PET_DOWN: u8 = 0x11;
const PET_UP: u8 = 0x91;
const PET_RIGHT: u8 = 0x1D;
const PET_LEFT: u8 = 0x9D;

/// ADM-3A key codes the guest program reads for cursor motion (an ADM-3A
/// keyboard sends these control characters).
pub(in crate::telnet) const ADM_UP: u8 = 0x0B; // ^K
pub(in crate::telnet) const ADM_DOWN: u8 = 0x0A; // ^J
pub(in crate::telnet) const ADM_LEFT: u8 = 0x08; // ^H
pub(in crate::telnet) const ADM_RIGHT: u8 = 0x0C; // ^L

/// A decoded ADM-3A screen operation, independent of the client terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::telnet) enum TermOp {
    /// A printable / passthrough byte (includes CR, LF, TAB, BELL).
    Print(u8),
    /// Move the cursor to a 0-based (row, col).
    CursorTo(u8, u8),
    /// Clear the screen and home the cursor.
    ClearHome,
    /// Home the cursor (no clear).
    Home,
    Up,
    Left,
    Right,
}

/// Stateful ADM-3A output decoder.  Feed guest output bytes one at a time;
/// each returns zero or more [`TermOp`]s.
#[derive(Default)]
pub(in crate::telnet) struct Adm3a {
    stage: Stage,
    /// A CR went to a Commodore and nothing has followed it yet: how many
    /// rows to climb if what follows is not an LF -- see [`Adm3a::emit`].
    petscii_cr: Option<u8>,
    /// Cells printed since the C64's cursor was last at column 0 of a row
    /// we know -- past 39 the line has wrapped onto the next row.
    petscii_col: u16,
    /// The widest `petscii_col` this line has reached.  A bare CR returns to
    /// column 0 but the C64 keeps the rows it wrapped onto linked, so its
    /// next `0x0D` still leaves from the end of them.
    petscii_wide: u16,
}

#[derive(Default, Clone, Copy)]
enum Stage {
    #[default]
    Ground,
    /// Saw `ESC`.
    Esc,
    /// Saw `ESC =`, expecting the row byte.
    Row,
    /// Saw `ESC = <row>`, expecting the col byte.
    Col(u8),
}

impl Adm3a {
    /// Feed one guest output byte, returning any completed operations.
    pub(in crate::telnet) fn feed(&mut self, b: u8) -> Vec<TermOp> {
        match self.stage {
            Stage::Ground => match b {
                0x1B => {
                    self.stage = Stage::Esc;
                    vec![]
                }
                0x1A => vec![TermOp::ClearHome], // ^Z
                0x1E => vec![TermOp::Home],       // ^^
                0x0B => vec![TermOp::Up],         // ^K
                0x0C => vec![TermOp::Right],      // ^L
                0x08 => vec![TermOp::Left],       // ^H
                // Everything else (printables, CR, LF, TAB, BELL) passes
                // through; CR/LF match the client's own newline handling.
                _ => vec![TermOp::Print(b)],
            },
            Stage::Esc => {
                if b == b'=' {
                    self.stage = Stage::Row;
                    vec![]
                } else {
                    // Not the ADM-3A cursor-address sequence; the ADM-3A has
                    // no other ESC codes, so pass both bytes through.
                    self.stage = Stage::Ground;
                    vec![TermOp::Print(0x1B), TermOp::Print(b)]
                }
            }
            Stage::Row => {
                self.stage = Stage::Col(b);
                vec![]
            }
            Stage::Col(row) => {
                self.stage = Stage::Ground;
                // ADM-3A biases row/col by 0x20 (space).
                vec![TermOp::CursorTo(row.wrapping_sub(0x20), b.wrapping_sub(0x20))]
            }
        }
    }
}

impl Adm3a {
    /// Feed one guest byte and render what it completes for `term`.
    ///
    /// The one place a renderer needs memory, and only for a Commodore. CP/M
    /// says CR (column 0, same row) and LF (down one, same column) as two
    /// motions; a C64 has neither -- its `0x0D` is *both* at once and its
    /// screen editor ignores `0x0A` entirely. Rendered byte for byte, a bare LF
    /// (how ADM-3A software moves down a row) did nothing, and a bare CR (how
    /// it rewrites a line) dropped a row. So a CR is sent at once -- most are
    /// half of a CR LF, and a prompt must not wait for its successor -- and
    /// whatever comes next decides: an LF has already happened, anything else
    /// steps back up first. Plain CR LF text still costs one byte per line.
    ///
    /// "Back up" is one row per 40 columns the line used: CP/M prints for 80
    /// columns, a C64 wraps a longer line onto the next row, and its `0x0D`
    /// leaves from the row the cursor is on -- so a 60-character status line
    /// rewritten with a bare CR climbs two rows, not one.
    ///
    /// The known limit: the C64 keeps rows linked until the screen is cleared,
    /// and this tracks only the line being printed.  A bare CR after cursor
    /// addressing onto a row an *earlier* long line left linked lands a row
    /// low.  Modelling the whole screen's link table would fix it; full-screen
    /// software addresses the cursor rather than rewriting with CR, so it is
    /// left until something is seen to need it.
    pub(in crate::telnet) fn emit(&mut self, b: u8, term: TerminalType, out: &mut Vec<u8>) {
        for op in self.feed(b) {
            if term != TerminalType::Petscii {
                render_op(op, term, out);
                continue;
            }
            let pending = self.petscii_cr.take();
            match op {
                TermOp::Print(b'\r') => {
                    if let Some(ups) = pending {
                        out.extend(std::iter::repeat_n(PET_UP, ups.into()));
                    }
                    out.push(b'\r');
                    let wide = self.petscii_wide.max(self.petscii_col);
                    // The C64 joins at most two rows into one logical line,
                    // and its `0x0D` leaves from the end of the one the cursor
                    // is in: the row pair holding it, plus the pair's second
                    // row only if the line ever reached into it.  The climb
                    // goes back to the pair's first row -- which is where the
                    // ADM-3A's CR goes too, since it wraps at 80 columns and a
                    // pair is exactly one of its lines.
                    let row = u32::from(self.petscii_col) / 40;
                    let pair = row - row % 2;
                    let end = if u32::from(wide) >= (pair + 1) * 40 { pair + 1 } else { pair };
                    self.petscii_cr = Some((end - pair + 1) as u8);
                    self.petscii_col = (pair * 40) as u16;
                    self.petscii_wide = wide;
                    continue;
                }
                TermOp::Print(b'\n') => {
                    if pending.is_none() {
                        out.push(PET_DOWN);
                        self.petscii_col %= 40;
                    }
                    // A new row either way: CR LF begins a fresh line.
                    self.petscii_wide = self.petscii_col;
                    continue;
                }
                _ => {}
            }
            if let Some(ups) = pending {
                out.extend(std::iter::repeat_n(PET_UP, ups.into()));
            }
            // Saturating: a guest may print for ever without a line end.
            self.petscii_col = match op {
                TermOp::CursorTo(_, c) => u16::from(c.min(39)),
                TermOp::ClearHome | TermOp::Home => 0,
                TermOp::Left => self.petscii_col.saturating_sub(1),
                TermOp::Right => self.petscii_col.saturating_add(1),
                TermOp::Print(c) if c >= 0x20 && c != 0x7F => self.petscii_col.saturating_add(1),
                _ => self.petscii_col,
            };
            self.petscii_wide = match op {
                TermOp::CursorTo(..) | TermOp::ClearHome | TermOp::Home => self.petscii_col,
                _ => self.petscii_wide.max(self.petscii_col),
            };
            render_op(op, term, out);
        }
    }
}

/// Render one operation for the connected terminal, appending to `out`.
pub(in crate::telnet) fn render_op(op: TermOp, term: TerminalType, out: &mut Vec<u8>) {
    match term {
        TerminalType::Ansi => render_ansi(op, out),
        TerminalType::Ascii => render_ascii(op, out),
        TerminalType::Petscii => render_petscii(op, out),
    }
}

fn render_ansi(op: TermOp, out: &mut Vec<u8>) {
    match op {
        TermOp::Print(b) => out.push(b),
        TermOp::CursorTo(r, c) => {
            // ANSI CSI is 1-based.
            out.extend_from_slice(
                format!("\x1b[{};{}H", r as u16 + 1, c as u16 + 1).as_bytes(),
            );
        }
        TermOp::ClearHome => out.extend_from_slice(b"\x1b[2J\x1b[H"),
        TermOp::Home => out.extend_from_slice(b"\x1b[H"),
        TermOp::Up => out.extend_from_slice(b"\x1b[A"),
        TermOp::Left => out.push(0x08),
        TermOp::Right => out.extend_from_slice(b"\x1b[C"),
    }
}

fn render_ascii(op: TermOp, out: &mut Vec<u8>) {
    // A dumb TTY can't position; keep the text stream linear so line-oriented
    // programs still read correctly.  Full-screen apps won't work here anyway.
    match op {
        TermOp::Print(b) => out.push(b),
        TermOp::Left => out.push(0x08),
        TermOp::ClearHome => out.extend_from_slice(b"\r\n"),
        TermOp::CursorTo(..) | TermOp::Home | TermOp::Up | TermOp::Right => {}
    }
}

fn render_petscii(op: TermOp, out: &mut Vec<u8>) {
    match op {
        // An ADM-3A draws nothing for DEL (padding, to a terminal), and on a
        // C64 `0x7F` is a graphic -- so it is dropped here, not moved left as
        // the shared rule does for an *echoed* rubout.
        TermOp::Print(0x7F) => {}
        // Otherwise the gateway's one ASCII-to-PETSCII byte rule: case swapped.
        TermOp::Print(b) => out.push(ascii_to_petscii_byte(b)),
        TermOp::CursorTo(r, c) => {
            // No absolute addressing on a C64: home, then step down/right.
            // Clamp to the C64's real 40x25 screen (rows 0-24, cols 0-39) so
            // an 80-column program's far-right positions land on the last
            // column instead of wrapping onto the next physical line.
            out.push(PET_HOME);
            for _ in 0..r.min(24) {
                out.push(PET_DOWN);
            }
            for _ in 0..c.min(39) {
                out.push(PET_RIGHT);
            }
        }
        TermOp::ClearHome => out.push(PET_CLEAR),
        TermOp::Home => out.push(PET_HOME),
        TermOp::Up => out.push(PET_UP),
        TermOp::Left => out.push(PET_LEFT),
        TermOp::Right => out.push(PET_RIGHT),
    }
}

/// Map the final byte of an ANSI CSI arrow sequence (`ESC [ <b>`) to the
/// ADM-3A key code, or `None` for a sequence we don't translate.
pub(in crate::telnet) fn csi_arrow_to_adm3a(final_byte: u8) -> Option<u8> {
    match final_byte {
        b'A' => Some(ADM_UP),
        b'B' => Some(ADM_DOWN),
        b'C' => Some(ADM_RIGHT),
        b'D' => Some(ADM_LEFT),
        _ => None,
    }
}

/// Map a PETSCII cursor-key byte the C64 sends to the ADM-3A key code, or
/// `None` if it isn't a cursor key.
pub(in crate::telnet) fn petscii_key_to_adm3a(byte: u8) -> Option<u8> {
    match byte {
        PET_UP => Some(ADM_UP),
        PET_DOWN => Some(ADM_DOWN),
        PET_LEFT => Some(ADM_LEFT),
        PET_RIGHT => Some(ADM_RIGHT),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_all(bytes: &[u8]) -> Vec<TermOp> {
        let mut a = Adm3a::default();
        bytes.iter().flat_map(|&b| a.feed(b)).collect()
    }

    #[test]
    fn test_printable_passthrough() {
        assert_eq!(feed_all(b"Hi!\r\n"), vec![
            TermOp::Print(b'H'),
            TermOp::Print(b'i'),
            TermOp::Print(b'!'),
            TermOp::Print(b'\r'),
            TermOp::Print(b'\n'),
        ]);
    }

    #[test]
    fn test_control_codes_decode() {
        assert_eq!(feed_all(&[0x1A]), vec![TermOp::ClearHome]);
        assert_eq!(feed_all(&[0x1E]), vec![TermOp::Home]);
        assert_eq!(feed_all(&[0x0B]), vec![TermOp::Up]);
        assert_eq!(feed_all(&[0x0C]), vec![TermOp::Right]);
        assert_eq!(feed_all(&[0x08]), vec![TermOp::Left]);
    }

    #[test]
    fn test_cursor_address_across_bytes() {
        // ESC = (row 2 -> 0x22) (col 5 -> 0x25) => CursorTo(2, 5).
        let mut a = Adm3a::default();
        assert!(a.feed(0x1B).is_empty());
        assert!(a.feed(b'=').is_empty());
        assert!(a.feed(0x22).is_empty());
        assert_eq!(a.feed(0x25), vec![TermOp::CursorTo(2, 5)]);
    }

    #[test]
    fn test_unknown_escape_passes_through() {
        // ESC followed by a non-'=' byte: both bytes pass through.
        assert_eq!(feed_all(&[0x1B, b'X']), vec![
            TermOp::Print(0x1B),
            TermOp::Print(b'X'),
        ]);
    }

    fn emit_all(bytes: &[u8], term: TerminalType) -> Vec<u8> {
        let mut a = Adm3a::default();
        let mut out = Vec::new();
        for &b in bytes {
            a.emit(b, term, &mut out);
        }
        out
    }

    /// A C64's `0x0D` is CR and LF at once and its `0x0A` does nothing, so
    /// the two CP/M motions are rebuilt from what follows a CR.
    #[test]
    fn test_a_commodore_gets_cr_and_lf_as_two_motions() {
        let p = TerminalType::Petscii;
        // Ordinary text: one byte per line, exactly as before.
        assert_eq!(emit_all(b"a\r\nb", p), vec![b'A', 0x0D, b'B']);
        // A bare LF is ADM-3A's cursor down.
        assert_eq!(emit_all(b"a\nb", p), vec![b'A', PET_DOWN, b'B']);
        // A bare CR rewrites the same line: down-and-back, then back up.
        assert_eq!(emit_all(b"ab\rc", p), vec![b'A', b'B', 0x0D, PET_UP, b'C']);
        assert_eq!(emit_all(b"\r\r\n", p), vec![0x0D, PET_UP, 0x0D]);
        // A line past 40 columns took two rows, so its bare CR climbs two.
        let mut long = vec![b'x'; 60];
        long.extend_from_slice(b"\ry");
        let mut want = vec![b'X'; 60];
        want.extend_from_slice(&[0x0D, PET_UP, PET_UP, b'Y']);
        assert_eq!(emit_all(&long, p), want);
        // A second bare CR on that line still climbs both rows: the C64 keeps
        // them linked after the first CR rewrote only its start.
        let mut twice = vec![b'x'; 60];
        twice.extend_from_slice(b"\rok\rz");
        let mut want = vec![b'X'; 60];
        want.extend_from_slice(&[0x0D, PET_UP, PET_UP, b'O', b'K', 0x0D, PET_UP, PET_UP, b'Z']);
        assert_eq!(emit_all(&twice, p), want);
        // At 80 columns an ADM-3A has wrapped to its next line, so the CR
        // returns to the start of THAT line -- the C64's third row -- and a
        // second CR there climbs from it, not from the first row.
        let mut far = vec![b'x'; 80];
        far.extend_from_slice(b"\rok\rz");
        let mut want = vec![b'X'; 80];
        want.extend_from_slice(&[0x0D, PET_UP, b'O', b'K', 0x0D, PET_UP, b'Z']);
        assert_eq!(emit_all(&far, p), want);
        // 100 columns: the second line's pair reached one row, so one climb.
        let mut mid = vec![b'x'; 100];
        mid.extend_from_slice(b"\rz");
        let mut want = vec![b'X'; 100];
        want.extend_from_slice(&[0x0D, PET_UP, b'Z']);
        assert_eq!(emit_all(&mid, p), want);
        // A guest that never ends a line does not overflow the count.
        let mut a = Adm3a::default();
        let mut sink = Vec::new();
        for _ in 0..70_000 {
            a.emit(b'x', p, &mut sink);
            sink.clear();
        }
        a.emit(b'\r', p, &mut sink); // the CR's arithmetic at the saturated column
        a.emit(b'z', p, &mut sink);
        // ...and its CR LF is still the one byte.
        assert_eq!(emit_all(&[&[b'x'; 60][..], b"\r\ny"].concat(), p).len(), 62);
        // DEL draws nothing, as on an ADM-3A -- never a PETSCII graphic.
        assert_eq!(emit_all(b"a\x7fb", p), vec![b'A', b'B']);
        // Nothing changes for a terminal that has both motions.
        assert_eq!(emit_all(b"a\rb\nc\r\n", TerminalType::Ansi), b"a\rb\nc\r\n");
        assert_eq!(emit_all(b"a\rb\nc\r\n", TerminalType::Ascii), b"a\rb\nc\r\n");
    }

    fn render_all(ops: &[TermOp], term: TerminalType) -> Vec<u8> {
        let mut out = Vec::new();
        for &op in ops {
            render_op(op, term, &mut out);
        }
        out
    }

    #[test]
    fn test_render_ansi() {
        assert_eq!(render_all(&[TermOp::CursorTo(2, 5)], TerminalType::Ansi), b"\x1b[3;6H");
        assert_eq!(render_all(&[TermOp::ClearHome], TerminalType::Ansi), b"\x1b[2J\x1b[H");
        assert_eq!(render_all(&[TermOp::Up], TerminalType::Ansi), b"\x1b[A");
        assert_eq!(render_all(&[TermOp::Right], TerminalType::Ansi), b"\x1b[C");
        assert_eq!(render_all(&[TermOp::Print(b'Z')], TerminalType::Ansi), b"Z");
    }

    #[test]
    fn test_render_petscii() {
        // Absolute address: HOME, one DOWN, two RIGHT.
        assert_eq!(
            render_all(&[TermOp::CursorTo(1, 2)], TerminalType::Petscii),
            vec![PET_HOME, PET_DOWN, PET_RIGHT, PET_RIGHT]
        );
        // Positions past the C64's 40x25 screen clamp (no wrap): row->24, col->39.
        let clamped = render_all(&[TermOp::CursorTo(30, 60)], TerminalType::Petscii);
        assert_eq!(clamped[0], PET_HOME);
        assert_eq!(clamped.iter().filter(|&&b| b == PET_DOWN).count(), 24);
        assert_eq!(clamped.iter().filter(|&&b| b == PET_RIGHT).count(), 39);
        assert_eq!(render_all(&[TermOp::ClearHome], TerminalType::Petscii), vec![PET_CLEAR]);
        assert_eq!(render_all(&[TermOp::Up], TerminalType::Petscii), vec![PET_UP]);
        // Case is swapped for the C64.
        assert_eq!(render_all(&[TermOp::Print(b'A')], TerminalType::Petscii), vec![b'a']);
        assert_eq!(render_all(&[TermOp::Print(b'a')], TerminalType::Petscii), vec![b'A']);
    }

    #[test]
    fn test_render_ascii_ignores_positioning() {
        assert_eq!(render_all(&[TermOp::CursorTo(3, 3)], TerminalType::Ascii), b"");
        assert_eq!(render_all(&[TermOp::Print(b'X')], TerminalType::Ascii), b"X");
        assert_eq!(render_all(&[TermOp::ClearHome], TerminalType::Ascii), b"\r\n");
    }

    #[test]
    fn test_input_key_maps() {
        assert_eq!(csi_arrow_to_adm3a(b'A'), Some(ADM_UP));
        assert_eq!(csi_arrow_to_adm3a(b'D'), Some(ADM_LEFT));
        assert_eq!(csi_arrow_to_adm3a(b'Z'), None);
        assert_eq!(petscii_key_to_adm3a(PET_UP), Some(ADM_UP));
        assert_eq!(petscii_key_to_adm3a(PET_RIGHT), Some(ADM_RIGHT));
        assert_eq!(petscii_key_to_adm3a(b'x'), None);
    }
}
