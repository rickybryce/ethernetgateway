//! ANSI escape sequences translated into PETSCII, for a Commodore terminal
//! reached through one of the gateways.
//!
//! **This replaces a stripper, and the difference is the whole point.** Both
//! PETSCII output paths -- `telnet/gateway.rs`'s `filter_gateway_output` and
//! `serial.rs`'s `AnsiStripState` (the modem's `AT+PETSCII=1` dial-out) --
//! used to *delete* every escape sequence a remote sent. A C64 therefore got
//! no colour, no cleared screen and no cursor control from any host, which is
//! exactly what a user reported on 2026-09-05: "no colors, ESC does not work,
//! clear screen is not working" through the Telnet Gateway to telnetbible.com.
//!
//! Deleting was never the right answer, because a C64 has an exact equivalent
//! for almost everything a text BBS actually sends. Measured on the wire that
//! day, telnetbible.com's whole escape vocabulary is `ESC[2J`, `ESC[H` and
//! SGR colour -- no cursor addressing at all. Every one of those maps to a
//! single PETSCII control byte.
//!
//! **The colour table is measured, not invented.** The same board was captured
//! twice, rendering the same screen: once as ANSI (announced as a 7-bit
//! terminal) and once as native PETSCII (announced as a Commodore). That is
//! one program's own opinion of which PETSCII colour each of its ANSI colours
//! means, and the table below agrees with it on every colour it covers:
//!
//! ```text
//!   ESC[34m   -> 1F  blue          ESC[92m  -> 99  light green
//!   ESC[1;96m -> 9F  cyan          ESC[32m  -> 1E  green
//!   ESC[93m   -> 9E  yellow        ESC[0m   -> 9B  light grey
//! ```
//!
//! This gateway's *own* palette is deliberately **not** used as a second
//! oracle, and the reason is worth recording. `green()` renders `ESC[1;32m`
//! for an ANSI client and `1E` for a Commodore -- but `ESC[1;32m` and
//! `ESC[92m` are the same colour in ANSI, and the board above renders `92`
//! as `99` (light green). Both are defensible; ours is a house style for our
//! own menus, the board's is one program's reading of ANSI. Since this
//! translator carries *someone else's* content, the board's reading wins, and
//! a test asserting our palette round-trips through here would be pinning a
//! preference as if it were a fact.
//!
//! **Anything not translatable is still dropped, exactly as before.** Cursor
//! addressing has no PETSCII equivalent, and neither does a per-character
//! background colour; those disappear the way every sequence used to. So this
//! module can only ever add output a C64 previously never saw -- it cannot
//! introduce a sequence that reaches the terminal raw.

/// PETSCII colour control codes.  Named rather than spelled inline because
/// four of the sixteen differ by one bit from a cursor-movement code and a
/// transposition would be invisible in a table of hex.
// PETSCII black (0x90) is deliberately absent: nothing here ever emits it.
// See `FG_NORMAL` for why.
const WHITE: u8 = 0x05;
const RED: u8 = 0x1C;
const CYAN: u8 = 0x9F;
const PURPLE: u8 = 0x9C;
const GREEN: u8 = 0x1E;
const BLUE: u8 = 0x1F;
const YELLOW: u8 = 0x9E;
const DARK_GREY: u8 = 0x97;
const MEDIUM_GREY: u8 = 0x98;
const LIGHT_RED: u8 = 0x96;
const LIGHT_GREEN: u8 = 0x99;
const LIGHT_BLUE: u8 = 0x9A;
const LIGHT_GREY: u8 = 0x9B;

/// What `ESC[0m` and `ESC[39m` restore.  The same value the gateway's own
/// `dim()` helper uses, so a reset leaves the screen in the colour our menus
/// draw themselves in rather than in whatever the last host chose.
const DEFAULT_COLOUR: u8 = LIGHT_GREY;

/// Reverse video on/off — `ESC[7m` and `ESC[27m`.
const RVS_ON: u8 = 0x12;
const RVS_OFF: u8 = 0x92;

/// Clear the screen and home the cursor: a C64's single `CLR` does both, so
/// `ESC[2J` needs no companion for `ESC[H`.
const CLR: u8 = 0x93;
/// Home the cursor without clearing — `ESC[H` on its own.
const HOME: u8 = 0x13;

/// Cursor movement, in the order `ESC[A`, `B`, `C`, `D` name it.
const CRSR_UP: u8 = 0x91;
const CRSR_DOWN: u8 = 0x11;
const CRSR_RIGHT: u8 = 0x1D;
const CRSR_LEFT: u8 = 0x9D;

/// The back-arrow, top-left of a Commodore keyboard.  This gateway has always
/// treated it as the ESC key (`is_esc_key`), and it is the key the screens
/// tell a C64 user to press.
const BACK_ARROW: u8 = 0x5F;

/// The eight ANSI foreground colours at normal intensity (`ESC[30m`..`37m`).
///
/// Black maps to **dark grey**, and PETSCII black (0x90) is never emitted at
/// all.  A C64's default screen is dark blue, and ANSI boards send `ESC[30m`
/// on the assumption of a black background, so a faithful `90` would render
/// text very nearly invisible -- and a screen the user cannot read is a worse
/// answer than a colour that is one shade off.  Nothing measured covers this
/// case, because the board captured above never sends black; it is the one
/// entry in the table chosen rather than observed, and it is the first place
/// to look if a board's dark accents come out wrong.
const FG_NORMAL: [u8; 8] = [
    DARK_GREY, // 30 black
    RED,       // 31 red
    GREEN,     // 32 green      (measured: ESC[32m -> 1E)
    YELLOW,    // 33 yellow
    BLUE,      // 34 blue       (measured: ESC[34m -> 1F)
    PURPLE,    // 35 magenta
    CYAN,      // 36 cyan
    LIGHT_GREY, // 37 white
];

/// The same eight at high intensity — `ESC[90m`..`97m`, and `ESC[1m` combined
/// with a normal colour, which is how nearly every board actually writes them.
const FG_BRIGHT: [u8; 8] = [
    MEDIUM_GREY, // 90 bright black
    LIGHT_RED,   // 91
    LIGHT_GREEN, // 92            (measured: ESC[92m -> 99)
    YELLOW,      // 93            (measured: ESC[93m -> 9E)
    LIGHT_BLUE,  // 94
    PURPLE,      // 95 — PETSCII has no lighter purple
    CYAN,        // 96            (measured: ESC[1;96m -> 9F)
    WHITE,       // 97
];

/// How many bytes of CSI parameters are kept before the sequence is abandoned.
///
/// ECMA-48 does not bound the parameter run, but no real terminal sends more
/// than a handful.  A host that drops a sequence's final byte would otherwise
/// swallow every byte that followed it for ever, so the parser gives up and
/// resumes ordinary output — the same reasoning, and the same number, as the
/// `ANSI_STRIP_CSI_LEN_CAP` this module replaces.
const CSI_PARAM_CAP: usize = 64;

/// Translates a remote's ANSI output into PETSCII, one byte at a time.
///
/// The state is carried across calls because a sequence can straddle any two
/// reads: a 4 KB read boundary falls at an arbitrary byte, so an `ESC` at the
/// end of one buffer and `[` at the start of the next is ordinary rather than
/// exotic.
#[derive(Default)]
pub(crate) struct AnsiToPetscii {
    /// 0 = ordinary text, 1 = ESC seen, 2 = inside CSI, 3 = inside a string
    /// sequence (OSC and friends), 4 = ESC inside a string sequence.
    state: u8,
    /// Parameter and intermediate bytes of the CSI being read.
    params: Vec<u8>,
    /// Whether `ESC[1m` is in force, which selects the bright half of the
    /// table for a colour that arrives in a later sequence.  Boards write
    /// both `ESC[1;32m` and `ESC[1m` … `ESC[32m`, so this cannot be handled
    /// inside a single sequence.
    bold: bool,
}

impl AnsiToPetscii {
    /// A fresh translator for one connection.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Feed one byte from the remote.
    ///
    /// `text` is called for every byte that is *not* part of an escape
    /// sequence, so the caller keeps ownership of its own character
    /// translation — the PETSCII case swap, the `BS` mapping, and whatever
    /// else that path does.  It is a closure rather than a `fn` because
    /// `serial.rs`'s punctuation folder carries state of its own, and it may
    /// emit nothing at all (the gateway drops `~`, which has no PETSCII
    /// equivalent).
    ///
    /// Control bytes this module produces are pushed to `out` directly and
    /// never passed to `text`: they are already PETSCII, and a caller's case
    /// swap must not be given a second opinion about them.
    pub(crate) fn feed<F>(&mut self, byte: u8, text: &mut F, out: &mut Vec<u8>)
    where
        F: FnMut(u8, &mut Vec<u8>),
    {
        match self.state {
            0 => {
                if byte == 0x1B {
                    self.state = 1;
                } else {
                    text(byte, out);
                }
            }
            1 => match byte {
                b'[' => {
                    self.params.clear();
                    self.state = 2;
                }
                // OSC, DCS, SOS, PM, APC — a string sequence with a payload we
                // have no use for.  Swallowed to its terminator, as before.
                b']' | b'P' | b'X' | b'^' | b'_' => self.state = 3,
                // Another ESC: the first stood alone and means nothing here.
                0x1B => self.state = 1,
                // Every other two-character sequence (charset selection, RIS,
                // save/restore cursor).  None has a PETSCII equivalent.
                _ => self.state = 0,
            },
            2 => {
                if (0x40..=0x7E).contains(&byte) {
                    self.emit_csi(byte, out);
                    self.params.clear();
                    self.state = 0;
                } else if byte == 0x1B {
                    // A truncated sequence followed by a new one.
                    self.params.clear();
                    self.state = 1;
                } else if byte < 0x20 || byte == 0x7F || self.params.len() >= CSI_PARAM_CAP {
                    // A control byte inside a CSI means the sequence was never
                    // finished; recover rather than eat the rest of the stream.
                    self.params.clear();
                    self.state = 0;
                } else {
                    self.params.push(byte);
                }
            }
            3 => {
                if byte == 0x07 {
                    self.state = 0;
                } else if byte == 0x1B {
                    self.state = 4;
                }
            }
            _ => {
                // ESC inside a string sequence: `ESC \` is ST and ends it,
                // anything else was part of the payload.
                self.state = if byte == b'\\' { 0 } else { 3 };
            }
        }
    }

    /// Whether a sequence is still being read, so a caller can tell "nothing
    /// arrived" from "something arrived and is not finished yet".
    #[cfg(test)]
    pub(crate) fn mid_sequence(&self) -> bool {
        self.state != 0
    }

    /// One complete CSI, now that its final byte has arrived.
    ///
    /// Everything without a PETSCII equivalent -- cursor addressing, erase in
    /// line, scroll regions, device queries -- falls through and is dropped,
    /// which is what the whole sequence used to do.
    fn emit_csi(&mut self, final_byte: u8, out: &mut Vec<u8>) {
        // A private-use CSI (`ESC[?25l` and friends) addresses a capability no
        // Commodore has.  Dropped whole rather than parsed for parameters.
        if self.params.first().is_some_and(|&b| (0x3C..=0x3F).contains(&b)) {
            return;
        }
        match final_byte {
            b'm' => {
                let params = parse_params(&self.params);
                self.emit_sgr(&params, out);
            }
            b'J' => {
                // Erase in display.  Only `2J` (and `3J`, which additionally
                // drops scrollback a C64 has not got) clears the whole screen;
                // erase-to-end and erase-to-start have no equivalent and would
                // do more harm than nothing if approximated by a full clear.
                if matches!(first_param(&self.params, 0), 2 | 3) {
                    out.push(CLR);
                }
            }
            b'H' | b'f' => {
                // Cursor position.  A C64 cannot be addressed, but the
                // overwhelmingly common use is `ESC[H` with no parameters —
                // go home — which it can do exactly.  Anything aiming at a
                // real row and column is dropped rather than guessed at.
                let params = parse_params(&self.params);
                if params.is_empty() || params.iter().all(|&p| p <= 1) {
                    out.push(HOME);
                }
            }
            b'A' | b'B' | b'C' | b'D' => {
                // Cursor movement.  The parameter is a repeat count that
                // defaults to 1, and PETSCII expresses a repeat by repeating
                // the byte.  Capped so a host asking for 9999 rows of movement
                // cannot flood the wire on a 300-baud C64.
                let n = first_param(&self.params, 1).clamp(1, 80);
                let code = match final_byte {
                    b'A' => CRSR_UP,
                    b'B' => CRSR_DOWN,
                    b'C' => CRSR_RIGHT,
                    _ => CRSR_LEFT,
                };
                for _ in 0..n {
                    out.push(code);
                }
            }
            _ => {}
        }
    }

    /// Select Graphic Rendition — the colour half, and the only part of ANSI
    /// a text BBS leans on.
    ///
    /// Parameters are applied left to right because `ESC[1;32m` sets bold
    /// before it sets the colour, and the two together choose one byte.
    fn emit_sgr(&mut self, params: &[u16], out: &mut Vec<u8>) {
        // A bare `ESC[m` is `ESC[0m`.
        if params.is_empty() {
            self.bold = false;
            out.push(RVS_OFF);
            out.push(DEFAULT_COLOUR);
            return;
        }
        for &p in params {
            match p {
                0 => {
                    self.bold = false;
                    out.push(RVS_OFF);
                    out.push(DEFAULT_COLOUR);
                }
                1 => self.bold = true,
                // Faint and normal-intensity both leave the bright table.
                2 | 22 => self.bold = false,
                7 => out.push(RVS_ON),
                27 => out.push(RVS_OFF),
                30..=37 => {
                    let i = (p - 30) as usize;
                    out.push(if self.bold { FG_BRIGHT[i] } else { FG_NORMAL[i] });
                }
                39 => out.push(DEFAULT_COLOUR),
                90..=97 => out.push(FG_BRIGHT[(p - 90) as usize]),
                // Background colours (40-49, 100-107) and every text
                // attribute a C64 cannot express are dropped.  The screen has
                // one background and it belongs to the user, not to a BBS.
                _ => {}
            }
        }
    }
}

/// CSI parameters as numbers.  Empty parameters count as zero, which is what
/// ECMA-48 says and what `ESC[;5m` relies on.
fn parse_params(raw: &[u8]) -> Vec<u16> {
    if raw.is_empty() {
        return Vec::new();
    }
    raw.split(|&b| b == b';')
        .map(|field| {
            field
                .iter()
                .filter(|b| b.is_ascii_digit())
                .fold(0u16, |acc, &b| {
                    acc.saturating_mul(10).saturating_add((b - b'0') as u16)
                })
        })
        .collect()
}

/// The first CSI parameter, or `default` when the sequence carried none.
///
/// An explicitly zero parameter also means the default for the sequences that
/// use this (a repeat count of nought is a repeat count of one), which is why
/// it does not simply return the parsed value.
fn first_param(raw: &[u8], default: u16) -> u16 {
    match parse_params(raw).first().copied() {
        Some(0) | None => default,
        Some(n) => n,
    }
}

/// A key from a Commodore keyboard, on its way out to a host that speaks
/// ASCII.  The mirror image of everything above, and the half that was
/// missing entirely.
///
/// **Cursor keys.**  A C64's are single PETSCII control bytes and were
/// forwarded raw.  Measured from Ricky's C64 through the gateway on
/// 2026-09-05: CRSR down arrived at the far end as `11`, CRSR left as `9D`.
/// No ASCII host has ever understood either, and `11` is worse than
/// meaningless -- it is XON, so a host with software flow control reads a
/// cursor key as permission to resume sending.
///
/// **The back-arrow becomes ESC.**  A C64 has no key marked ESC, and this
/// gateway has always treated the back-arrow as one: `is_esc_key` accepts it
/// at every prompt, and the bridges tell the user to press it twice to
/// disconnect.  It nevertheless reached the remote as `5F`, an underscore, so
/// the one key a Commodore user believes is ESC did nothing at the far end.
/// Reported 2026-09-05 against telnetbible.com, whose own `is_esc_key`
/// accepts `5F` only from a terminal it has decided is a Commodore -- and it
/// decides that from the erase byte, which the gateway folds to `7F` for the
/// benefit of unix hosts.  Rather than unpick that (folding is right for an
/// ASCII board and wrong for a PETSCII-aware one, so it is a per-board choice
/// and not a global one), the key is sent as what the user means by it, which
/// works on *every* host rather than only on those that can recognise a C64.
/// **The cost is the underscore**: a Commodore can no longer type one at a
/// remote -- nothing at a BBS, and it does bite at a unix shell.  `CTRL+:`
/// remains a second ESC (measured on CCGMS).
///
/// The gateway's leave pair is unaffected: its call site weighs the key
/// *before* this runs, so two quick presses still disconnect and a single one
/// is forwarded.  The modem's online mode has no such pair -- it leaves with
/// `+++` -- so there the back-arrow is only ever an ESC.
///
/// Returns the sequence to send, or `None` for every other byte, which the
/// caller forwards under its own rules.
pub(crate) fn key_to_ansi_host(byte: u8) -> Option<&'static [u8]> {
    match byte {
        CRSR_UP => Some(b"\x1b[A"),
        CRSR_DOWN => Some(b"\x1b[B"),
        CRSR_RIGHT => Some(b"\x1b[C"),
        CRSR_LEFT => Some(b"\x1b[D"),
        BACK_ARROW => Some(b"\x1b"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run a whole stream through, with a text handler that passes bytes
    /// through unchanged — the escape translation is what is under test here,
    /// not any caller's character mapping.
    fn xlate(input: &[u8]) -> Vec<u8> {
        let mut st = AnsiToPetscii::new();
        let mut out = Vec::new();
        let mut text = |b: u8, out: &mut Vec<u8>| out.push(b);
        for &b in input {
            st.feed(b, &mut text, &mut out);
        }
        out
    }

    /// The measured table: telnetbible.com rendering one screen twice, once
    /// as ANSI and once as native PETSCII.  Its own opinion of what each of
    /// its colours means on a Commodore, captured 2026-09-05.
    #[test]
    fn test_the_measured_board_agrees_with_the_colour_table() {
        for (ansi, petscii) in [
            (&b"\x1b[34m"[..], BLUE),
            (&b"\x1b[1;96m"[..], CYAN),
            (&b"\x1b[93m"[..], YELLOW),
            (&b"\x1b[1;33m"[..], YELLOW),
            (&b"\x1b[92m"[..], LIGHT_GREEN),
            (&b"\x1b[32m"[..], GREEN),
        ] {
            assert_eq!(
                xlate(ansi),
                vec![petscii],
                "{:?} should translate to {:02X}",
                String::from_utf8_lossy(&ansi[1..]),
                petscii
            );
        }
        // A reset also leaves reverse video, which is why it is two bytes.
        assert_eq!(xlate(b"\x1b[0m"), vec![RVS_OFF, DEFAULT_COLOUR]);
    }

    #[test]
    fn test_clear_screen_and_home() {
        // The exact opener telnetbible.com sends.  A C64's CLR homes as well,
        // so the pair is a clear followed by a redundant-but-harmless home.
        assert_eq!(xlate(b"\x1b[2J\x1b[H"), vec![CLR, HOME]);
        assert_eq!(xlate(b"\x1b[J"), Vec::<u8>::new(), "erase-to-end has no equivalent");
        assert_eq!(xlate(b"\x1b[0J"), Vec::<u8>::new());
        assert_eq!(xlate(b"\x1b[1J"), Vec::<u8>::new());
        assert_eq!(xlate(b"\x1b[3J"), vec![CLR]);
        // Addressing a real row and column is dropped, not approximated.
        assert_eq!(xlate(b"\x1b[10;5H"), Vec::<u8>::new());
        assert_eq!(xlate(b"\x1b[1;1H"), vec![HOME]);
    }

    #[test]
    fn test_cursor_movement_repeats_the_petscii_byte() {
        assert_eq!(xlate(b"\x1b[A"), vec![CRSR_UP]);
        assert_eq!(xlate(b"\x1b[3B"), vec![CRSR_DOWN; 3]);
        assert_eq!(xlate(b"\x1b[0C"), vec![CRSR_RIGHT], "a zero count means one");
        assert_eq!(xlate(b"\x1b[2D"), vec![CRSR_LEFT; 2]);
        // A host asking for thousands of moves cannot flood a 300-baud wire.
        assert_eq!(xlate(b"\x1b[9999B").len(), 80);
    }

    #[test]
    fn test_bold_selects_the_bright_colour_across_sequences() {
        // Both spellings must give the same byte: boards write each.
        assert_eq!(xlate(b"\x1b[1;31m"), vec![LIGHT_RED]);
        assert_eq!(xlate(b"\x1b[1m\x1b[31m"), vec![LIGHT_RED]);
        assert_eq!(xlate(b"\x1b[31m"), vec![RED]);
        // …and it has to be cleared again by a reset or by 22.
        assert_eq!(
            xlate(b"\x1b[1m\x1b[0m\x1b[31m"),
            vec![RVS_OFF, DEFAULT_COLOUR, RED]
        );
        assert_eq!(xlate(b"\x1b[1m\x1b[22m\x1b[31m"), vec![RED]);
    }

    #[test]
    fn test_reverse_video() {
        assert_eq!(xlate(b"\x1b[7m"), vec![RVS_ON]);
        assert_eq!(xlate(b"\x1b[27m"), vec![RVS_OFF]);
        assert_eq!(xlate(b"\x1b[7;32m"), vec![RVS_ON, GREEN]);
    }

    #[test]
    fn test_background_and_unknown_attributes_are_dropped() {
        // A C64's background belongs to the user; a BBS does not get a vote.
        assert_eq!(xlate(b"\x1b[44m"), Vec::<u8>::new());
        assert_eq!(xlate(b"\x1b[4m"), Vec::<u8>::new(), "underline");
        assert_eq!(xlate(b"\x1b[38;5;208m"), Vec::<u8>::new(), "256-colour");
        assert_eq!(xlate(b"\x1b[?25l"), Vec::<u8>::new(), "private-use");
        assert_eq!(xlate(b"\x1b[6n"), Vec::<u8>::new(), "device query");
    }

    /// The property that makes this safe to put on a live byte stream: a
    /// sequence split at any point must translate to the same thing as one
    /// that arrived whole, because a 4 KB read boundary falls wherever it
    /// likes.
    #[test]
    fn test_a_sequence_split_at_any_byte_gives_the_same_answer() {
        let wire = b"a\x1b[1;32mb\x1b[2Jc\x1b[3Dd\x1b[0me";
        let whole = xlate(wire);
        for cut in 0..wire.len() {
            let mut st = AnsiToPetscii::new();
            let mut out = Vec::new();
            let mut text = |b: u8, out: &mut Vec<u8>| out.push(b);
            for &b in &wire[..cut] {
                st.feed(b, &mut text, &mut out);
            }
            for &b in &wire[cut..] {
                st.feed(b, &mut text, &mut out);
            }
            assert_eq!(out, whole, "split at {} changed the output", cut);
        }
    }

    #[test]
    fn test_ordinary_text_reaches_the_caller_untouched() {
        // Every byte that is not part of a sequence, including the high half
        // a PETSCII stream is full of.
        let mut st = AnsiToPetscii::new();
        let mut out = Vec::new();
        let mut text = |b: u8, out: &mut Vec<u8>| out.push(b);
        for b in 0u8..=255 {
            if b == 0x1B {
                continue;
            }
            st.feed(b, &mut text, &mut out);
        }
        assert_eq!(out.len(), 255);
        assert!(!st.mid_sequence());
    }

    #[test]
    fn test_an_unterminated_sequence_cannot_eat_the_stream() {
        // A host that drops a final byte must not silence everything after it.
        let long = [b'1'; CSI_PARAM_CAP + 10];
        let mut wire = vec![0x1B, b'['];
        wire.extend_from_slice(&long);
        wire.extend_from_slice(b"visible");
        let out = xlate(&wire);
        assert!(
            out.ends_with(b"visible"),
            "recovered output was {:?}",
            String::from_utf8_lossy(&out)
        );
    }

    #[test]
    fn test_two_character_sequences_are_dropped_whole() {
        // `ESC 7`, `ESC 8`, `ESC =`, charset selectors.  None has a PETSCII
        // equivalent, and dropping the ESC alone would leave the following
        // byte on screen as a stray glyph.  (Carried over from the stripper
        // this module replaced, which had this case and little else.)
        assert_eq!(xlate(b"A\x1b7B"), b"AB".to_vec());
        assert_eq!(xlate(b"A\x1b=B"), b"AB".to_vec());
    }

    #[test]
    fn test_string_sequences_are_swallowed_to_their_terminator() {
        // A window title reaches no C64, under either terminator.
        assert_eq!(xlate(b"\x1b]0;title\x07ok"), b"ok".to_vec());
        assert_eq!(xlate(b"\x1b]0;title\x1b\\ok"), b"ok".to_vec());
    }

    #[test]
    fn test_commodore_keys_out_to_an_ascii_host() {
        assert_eq!(key_to_ansi_host(CRSR_UP), Some(&b"\x1b[A"[..]));
        assert_eq!(key_to_ansi_host(CRSR_DOWN), Some(&b"\x1b[B"[..]));
        assert_eq!(key_to_ansi_host(CRSR_RIGHT), Some(&b"\x1b[C"[..]));
        assert_eq!(key_to_ansi_host(CRSR_LEFT), Some(&b"\x1b[D"[..]));
        // The key a C64 user calls ESC, which used to arrive as an underscore.
        assert_eq!(key_to_ansi_host(BACK_ARROW), Some(&b"\x1b"[..]));
        // Everything else is the caller's business, including the keys that
        // already work: RUN/STOP is 03 and must stay a literal Ctrl-C, and a
        // real ESC needs no help.
        for b in [0x03u8, 0x0D, 0x14, 0x1B, b'A', b'z', 0x92, 0x9E] {
            assert_eq!(key_to_ansi_host(b), None, "byte {:02X}", b);
        }
    }
}
