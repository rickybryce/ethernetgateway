//! The Cromemco Dazzler — the first colour graphics card for microcomputers,
//! and the VDM-1's problem one card along.
//!
//! Like the VDM-1 it is a *display* rather than a console: it has no data port
//! and the picture is main memory, which it reads continuously by DMA (the
//! manual's own words: "the DAZZLER can display a picture while at the same
//! time the computer is executing either a related or unrelated program").  So
//! sampling it cannot disturb the guest, and the same publish-on-poll spine
//! serves it.
//!
//! It differs from the VDM-1 in three ways that shape this module, and all
//! three were **measured on real software before they were read in the manual**
//! (`test_measure_what_a_dazzler_program_drives`):
//!
//! * **The picture can be anywhere.** Port `0Eh` carries the on/off bit and the
//!   top seven address bits, so the buffer moves with the program: KSCOPE puts
//!   it at `0200`, DMATION at `1000`, SPACEWAR at `1800`, GDEMO at `D600`.
//! * **The format is animated, not configured.** GDEMO rewrites port `0Fh`
//!   twenty-nine times while running, so the bytes and the way to read them
//!   must be sampled together or a viewer paints one frame under another
//!   frame's mode.
//! * **There is a readable status, and software leans on it hard.** GDEMO polls
//!   `IN 0Eh` **58.8 million times** waiting for the end-of-frame bit.  A
//!   display that handled only the writes would leave it spinning on whatever
//!   the unclaimed-port rule happened to return — the same trap as the floating
//!   sense switches.
//!
//! **CLEAN-ROOM.** Written from the *Cromemco Dazzler Manual* (1979), a
//! published document — the same discriminator settled for Punter, HBIOS, EGT80
//! and the VDM-1: does an independent authority exist?  The register tables and
//! the memory map below are that manual's, and the four measured programs above
//! decode correctly under them, which is the cross-check.  Another emulator's
//! source is a cross-check afterwards, never a source.

/// The on/off bit and the picture's address.  Write-only.
pub const ADDRESS_PORT: u8 = 0x0E;
/// The format register.  Write-only.
pub const FORMAT_PORT: u8 = 0x0F;

/// A 512-byte picture, and a 2 KB one.
///
/// The manual: "the picture may require 512 bytes of memory or 2K bytes of
/// memory depending on the mode".
pub const SMALL: usize = 512;
pub const LARGE: usize = 2048;

/// Bytes across one quadrant's row, and rows down one quadrant.
///
/// From the manual's memory map: bytes 0–15 are the first row of the first
/// quadrant, and a quadrant is one 512-byte page.
const ROW_BYTES: usize = 16;
const QUADRANT_ROWS: usize = SMALL / ROW_BYTES;

/// Is the card switched on?  Bit 7 of the address register.
pub fn is_on(address: u8) -> bool {
    address & 0x80 != 0
}

/// Where the picture starts.
///
/// The low seven bits of the address register are **A15..A9** — the manual is
/// explicit that the lowest bit is A9, not A8, so the picture sits on a
/// 512-byte boundary.  Getting this wrong by one bit would put every picture at
/// half its real address, which is the kind of mistake that renders *something*
/// and so survives a casual look.
pub fn base(address: u8) -> u16 {
    ((address & 0x7F) as u16) << 9
}

/// The format register, unpacked.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Format {
    /// D6: resolution x4 — one *bit* per element, colour from this register.
    pub x4: bool,
    /// D5: the picture occupies 2 KB rather than 512 bytes.
    pub large: bool,
    /// D4: a colour picture rather than black-and-white.
    pub colour: bool,
    /// D3–D0: in x4 mode, the colour (or grey level) of the whole picture.
    /// Unused in normal resolution, where every element carries its own.
    pub ink: u8,
}

impl Format {
    pub fn from_byte(b: u8) -> Format {
        Format {
            x4: b & 0x40 != 0,
            large: b & 0x20 != 0,
            colour: b & 0x10 != 0,
            ink: b & 0x0F,
        }
    }

    /// How many bytes of guest memory this picture occupies.
    pub fn bytes(&self) -> usize {
        if self.large { LARGE } else { SMALL }
    }

    /// The picture's size in elements.
    ///
    /// The manual: normal resolution is "32 x 32 element picture for 512 bytes
    /// or 64 x 64 element picture for 2K bytes", and resolution x4 is "64 x 64
    /// element picture for 512 bytes or 128 x 128 element picture for 2K bytes".
    pub fn size(&self) -> (usize, usize) {
        let n = match (self.x4, self.large) {
            (false, false) => 32,
            (false, true) => 64,
            (true, false) => 64,
            (true, true) => 128,
        };
        (n, n)
    }
}

/// A rendered picture: one 4-bit value per element.
///
/// Four bits either way, and what they *mean* is the format's business — in
/// colour they are red/green/blue/intensity from D0 up, in black-and-white they
/// are "one of 16 levels of grey".  Carried as the raw nibble with the flag
/// beside it rather than resolved to RGB here, because the palette belongs to
/// whoever is painting and this module has no display.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Picture {
    pub width: usize,
    pub height: usize,
    /// True: each cell is red|green|blue|intensity.  False: a grey level.
    pub colour: bool,
    /// `width * height` cells, top row first, left to right.
    pub cells: Vec<u8>,
}

/// Render what the card would scan.
///
/// This is the whole correctness surface of the Dazzler and it is deliberately
/// pure: no machine, no session, no display.  `bytes` is the guest's picture as
/// sampled from [`base`]; short input is tolerated and shows as unlit, because
/// a picture whose buffer runs off the top of memory is the guest's business
/// and not a reason to refuse to draw.
///
/// The layout, from the manual, in the order it bites:
///
/// * **Quadrants.** "The 2K byte DAZZLER picture is stored in memory as four
///   quadrants. Each quadrant of the picture occupies one 512-byte page of
///   memory. Only one page of memory is displayed for a 512-byte picture."  The
///   map shows them top-left, top-right, bottom-left, bottom-right — so the
///   second 512 bytes are the *right* half of the top, not the next rows down.
///   This is the one that a scrolling-text mental model gets wrong.
/// * **Two elements per byte** in normal resolution, "one byte of memory is
///   used to represent two adjacent elements", low nibble first.
/// * **Eight elements per byte** in x4 mode, in a 4x2 block laid out
///   `D0 D1 D4 D5` over `D2 D3 D6 D7` — not a raster run, and not column-major
///   either.
pub fn frame(bytes: &[u8], format: Format) -> Picture {
    let (width, height) = format.size();
    let mut cells = vec![0u8; width * height];
    // Half the picture across and down: one quadrant, in elements.
    let (qw, qh) = (width / 2, height / 2);
    let quadrants = if format.large { 4 } else { 1 };

    for q in 0..quadrants {
        // Quadrant order is reading order: top-left, top-right, bottom-left,
        // bottom-right.  With one quadrant it is the top-left, and the picture
        // *is* that quadrant, so the offsets are zero.
        let (ox, oy) = if format.large {
            ((q % 2) * qw, (q / 2) * qh)
        } else {
            (0, 0)
        };
        for row in 0..QUADRANT_ROWS {
            for col in 0..ROW_BYTES {
                let b = bytes.get(q * SMALL + row * ROW_BYTES + col).copied().unwrap_or(0);
                if format.x4 {
                    // Eight elements: a 4-wide, 2-high block per byte.
                    for (bit, (dx, dy)) in
                        [(0, 0), (1, 0), (0, 1), (1, 1), (2, 0), (3, 0), (2, 1), (3, 1)]
                            .into_iter()
                            .enumerate()
                    {
                        let x = ox + col * 4 + dx;
                        let y = oy + row * 2 + dy;
                        let lit = b & (1 << bit) != 0;
                        cells[y * width + x] = if lit { format.ink } else { 0 };
                    }
                } else {
                    // Two elements: the low nibble is the left one.
                    let x = ox + col * 2;
                    let y = oy + row;
                    cells[y * width + x] = b & 0x0F;
                    cells[y * width + x + 1] = b >> 4;
                }
            }
        }
    }

    Picture { width, height, colour: format.colour, cells }
}

/// The card's readable status, on port `0Eh`.
///
/// The manual: "Only two bits of input port 0EH are used. Bit D7 is low during
/// odd lines and high during even lines. Bit D6 goes low for 4 ms between
/// frames to indicate end of frame."
///
/// `phase` is where we are in a frame, `0.0..1.0`.  The unused bits are
/// returned **high**, matching the rest of this machine's answer for a line
/// nothing drives — a card that answered zero in the six bits it does not
/// implement would be inventing a reading.
///
/// The 4 ms is of a 60 Hz frame, so the blanking interval is very nearly a
/// quarter of it; that ratio is the manual's and is what the test pins.
pub fn status(phase: f32, line_is_even: bool) -> u8 {
    const FRAME_MS: f32 = 1000.0 / 60.0;
    const BLANK_MS: f32 = 4.0;
    let mut b = 0xFF;
    if phase >= 1.0 - BLANK_MS / FRAME_MS {
        b &= !0x40; // D6 low: end of frame.
    }
    if !line_is_even {
        b &= !0x80; // D7 low during odd lines.
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The four values four real programs wrote, decoded.  This is the
    /// cross-check that the manual's encoding and the measured software agree —
    /// each on its own could be misread, and together they cannot.
    #[test]
    fn test_the_measured_programs_decode_the_way_the_manual_says() {
        // KSCOPE: on, buffer at 0200, 64x64 colour in 2K.
        assert!(is_on(0x81));
        assert_eq!(base(0x81), 0x0200);
        let k = Format::from_byte(0x30);
        assert_eq!((k.x4, k.large, k.colour), (false, true, true));
        assert_eq!(k.size(), (64, 64));
        assert_eq!(k.bytes(), LARGE);

        // DMATION at 1000, SPACEWAR at 1800, GDEMO at D600.
        assert_eq!(base(0x88), 0x1000);
        assert_eq!(base(0x8C), 0x1800);
        assert_eq!(base(0xEB), 0xD600);

        // SPACEWAR: 128x128 black-and-white at grey level 12.
        let s = Format::from_byte(0x6C);
        assert_eq!((s.x4, s.large, s.colour), (true, true, false));
        assert_eq!(s.size(), (128, 128));
        assert_eq!(s.ink, 12);

        // And off is off, whatever else the register says.
        assert!(!is_on(0x0B));
    }

    /// The lowest address bit is **A9**, not A8.  Off by one here halves every
    /// picture's address, and the result still renders *something*, which is
    /// exactly how a wrong layout survives a casual look.
    #[test]
    fn test_the_address_register_starts_at_a9() {
        assert_eq!(base(0x01), 0x0200, "the lowest bit is A9");
        assert_eq!(base(0x02), 0x0400);
        assert_eq!(base(0x7F), 0xFE00, "and the top bit of the seven is A15");
        assert_eq!(base(0xFF), 0xFE00, "the on-bit is not an address bit");
    }

    #[test]
    fn test_normal_resolution_puts_the_low_nibble_on_the_left() {
        let mut bytes = vec![0u8; SMALL];
        bytes[0] = 0x21; // left element 1, right element 2
        let p = frame(&bytes, Format::from_byte(0x10)); // 32x32 colour, 512 bytes
        assert_eq!(p.width, 32);
        assert_eq!(p.cells[0], 1);
        assert_eq!(p.cells[1], 2);
        assert!(p.colour);
    }

    /// The four quadrants are the map's, and this is the one a text-shaped
    /// mental model gets wrong: the second 512 bytes are the **right half of
    /// the top**, not the next rows down.
    #[test]
    fn test_the_second_page_is_the_right_half_not_the_next_rows() {
        let mut bytes = vec![0u8; LARGE];
        bytes[0] = 0x01; // quadrant 1, first element
        bytes[SMALL] = 0x02; // quadrant 2
        bytes[2 * SMALL] = 0x03; // quadrant 3
        bytes[3 * SMALL] = 0x04; // quadrant 4
        let p = frame(&bytes, Format::from_byte(0x30)); // 64x64 colour, 2K
        assert_eq!(p.width, 64);
        assert_eq!(p.cells[0], 1, "top-left");
        assert_eq!(p.cells[32], 2, "top-right — halfway along the FIRST row");
        assert_eq!(p.cells[32 * 64], 3, "bottom-left");
        assert_eq!(p.cells[32 * 64 + 32], 4, "bottom-right");
    }

    /// One byte is eight elements in a 4x2 block, `D0 D1 D4 D5` over
    /// `D2 D3 D6 D7` — neither a raster run nor a column.
    #[test]
    fn test_resolution_x4_lays_a_byte_out_as_four_by_two() {
        let want = [(0x01, (0, 0)), (0x02, (1, 0)), (0x04, (0, 1)), (0x08, (1, 1)),
                    (0x10, (2, 0)), (0x20, (3, 0)), (0x40, (2, 1)), (0x80, (3, 1))];
        for (bit, (x, y)) in want {
            let mut bytes = vec![0u8; SMALL];
            bytes[0] = bit;
            // x4, 512 bytes, black-and-white, ink 15.
            let p = frame(&bytes, Format::from_byte(0x4F));
            assert_eq!(p.width, 64);
            assert!(!p.colour);
            let lit: Vec<usize> = p.cells.iter().enumerate().filter(|(_, c)| **c != 0)
                .map(|(i, _)| i).collect();
            assert_eq!(lit, vec![y * 64 + x], "bit {bit:#04x} belongs at ({x},{y})");
        }
    }

    /// In x4 mode the colour is the *register's*, not the memory's — every lit
    /// element is the same ink, which is why full colour there needs
    /// interleaved frames.
    #[test]
    fn test_resolution_x4_takes_its_colour_from_the_format_register() {
        let mut bytes = vec![0u8; SMALL];
        bytes[0] = 0xFF;
        let p = frame(&bytes, Format::from_byte(0x5A)); // x4, colour, ink 0x0A
        assert!(p.cells.iter().filter(|&&c| c != 0).all(|&c| c == 0x0A));
        assert_eq!(p.cells.iter().filter(|&&c| c != 0).count(), 8);
    }

    /// A buffer that runs off the top of memory draws blank rather than
    /// panicking — where a guest points its picture is the guest's business.
    #[test]
    fn test_a_short_buffer_is_unlit_not_a_panic() {
        let p = frame(&[0xFF, 0xFF], Format::from_byte(0x30));
        assert_eq!(p.cells.len(), 64 * 64);
        assert_eq!(p.cells[0], 0x0F);
        assert!(p.cells[100..].iter().all(|&c| c == 0));
    }

    /// The status bits GDEMO polls 58.8 million times.  The unused six read
    /// high, like every other line nothing drives on this machine.
    #[test]
    fn test_the_end_of_frame_bit_goes_low_only_between_frames() {
        assert_eq!(status(0.0, true) & 0x40, 0x40, "mid-frame: not end of frame");
        assert_eq!(status(0.5, true) & 0x40, 0x40);
        assert_eq!(status(0.99, true) & 0x40, 0, "the last 4 ms of 16.7: end of frame");
        // D7 is the line parity, and it is independent of the frame bit.
        assert_eq!(status(0.5, true) & 0x80, 0x80, "even line");
        assert_eq!(status(0.5, false) & 0x80, 0, "odd line");
        // Everything else reads high.
        assert_eq!(status(0.5, true) & 0x3F, 0x3F);
    }

    /// The blanking interval is 4 ms of a 60 Hz frame — a quarter of it, near
    /// enough — so a guest polling for end-of-frame finds it soon and not
    /// almost never.  A ratio wrong by an order of magnitude would look like a
    /// hang rather than a wrong number.
    #[test]
    fn test_the_blanking_interval_is_the_manuals_share_of_a_frame() {
        let low = (0..1000).filter(|i| status(*i as f32 / 1000.0, true) & 0x40 == 0).count();
        assert!((230..=250).contains(&low), "got {low} per 1000, want ~240 (4 ms of 16.67)");
    }
}
