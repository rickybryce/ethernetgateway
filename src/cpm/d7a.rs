//! The Cromemco D+7A — the board the Dazzler games read their joysticks from.
//!
//! Same premise as [`super::dazzler`] and [`super::vdm`]: a 1970s card with no
//! console of its own, so a program using it goes quiet on the session and the
//! operator has nothing to press. The difference is direction — those two are
//! *displays* and this is an *input*, which is why the browser that watches a
//! booted guest can now also play it.
//!
//! # What the hardware is, measured
//!
//! Not remembered from a manual. `DISK10.DSK` carries `SPACEWAR.COM` and
//! Cromemco's own `ADCTEST.COM`, and
//! `boot_machine::tests::test_measure_what_a_dazzler_program_drives` was run on
//! both:
//!
//! | program  | `18h`                 | `19h`–`1Ch`        |
//! |----------|-----------------------|--------------------|
//! | SPACEWAR | IN ×33,283            | IN ×8,321 **each** |
//! | ADCTEST  | IN ×3,330, **OUT ×1** | IN ×1,111 each     |
//!
//! Four channels read in equal numbers, and one port read three or four times
//! per pass around the loop and written once at start-up. ADCTEST then
//! *documents the board on its own screen*, which is where the rest comes from:
//!
//! * "**1X**", "**1Y**", "**2X**", "**2Y**" — four readout rows, so two
//!   joysticks with two axes each, on `19h`–`1Ch`.
//! * "*The bottom row bits should become zeros each time the associated push
//!   button is pressed*" — the switches are their own byte and are **active
//!   low**. That byte is `18h`.
//! * "*Exit by pushing switches 1 & 4 on either joystick*" — up to four
//!   switches per stick, not one fire button.
//! * "*adjust RV1 so the top row 1X is all zeros*" — a **centred stick reads
//!   zero**. Not mid-scale: this is a bipolar converter whose zero is trimmed,
//!   so centre is `00` and deflection runs either side of it.
//!
//! What no measurement here could establish is the *sign* of each direction and
//! which port carries which row. Those are pinned by
//! `test_adctest_reads_back_what_the_board_was_told`, which drives the real
//! `ADCTEST.COM` and reads its own readout — the period program's opinion of our
//! board, in its own words, which beats any reasoning about polarity.
//!
//! # The ramp
//!
//! A key has no magnitude and this is an analogue converter, so a held key
//! *grows*: centre at the moment of the press, full deflection after
//! [`RAMP_MS`]. Tapping nudges, holding swings — which is what proportional
//! controls in these games want (SPACEWAR's rotation rate is its stick's
//! deflection, so a fixed full-scale would only ever spin flat out).
//!
//! Release returns to centre at once rather than decaying, because that is what
//! a sprung stick does when you let go of it.
//!
//! **The ramp is integrated here, not in the browser.** A page reports which
//! keys are *down*; the elapsed time is this board's business. Two reasons, both
//! about the same thing: the guest polls these ports tens of thousands of times
//! a second while the page can only report on its own poll (150 ms), so a
//! browser-computed level would arrive in visible steps; and a dropped or late
//! request then changes only *when* a press is noticed, never the shape of the
//! swing that follows.

/// The switch byte — all four buttons of both sticks, active low.
pub const SWITCH_PORT: u8 = 0x18;

/// The four analogue channels, in the order ADCTEST lists its rows:
/// stick 1 X, stick 1 Y, stick 2 X, stick 2 Y.
pub const AXIS_PORTS: [u8; 4] = [0x19, 0x1A, 0x1B, 0x1C];

/// Every port this board answers.
///
/// Derived from the two above rather than written out again: a port list in two
/// places is a rule that holds in one, and the compiler can carry this one.
pub const PORTS: [u8; 5] = [
    SWITCH_PORT,
    AXIS_PORTS[0],
    AXIS_PORTS[1],
    AXIS_PORTS[2],
    AXIS_PORTS[3],
];

/// How long a held key takes to reach full deflection.
///
/// Measured against the games rather than chosen: at 200 ms SPACEWAR's ship is
/// hard to aim because a tap already swings it a long way, and at 1 s it feels
/// unresponsive. Half a second is a stick you can both nudge and swing.
pub const RAMP_MS: u64 = 500;

/// Full deflection, either side of a centred zero.
///
/// 127 and not 128, so the two directions are symmetric: a stick that reads
/// -128 one way and +127 the other turns faster to one side in every game that
/// scales by it.
pub const FULL: i8 = 127;

/// Which stick, and which of its axes.
///
/// Indexes [`AXIS_PORTS`], so the order is ADCTEST's row order and nothing else
/// gets to decide it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    /// Stick 1, horizontal — ADCTEST's `1X`.
    P1X = 0,
    /// Stick 1, vertical — `1Y`.
    P1Y = 1,
    /// Stick 2, horizontal — `2X`.
    P2X = 2,
    /// Stick 2, vertical — `2Y`.
    P2Y = 3,
}

impl Axis {
    /// All four, in port order.
    pub const ALL: [Axis; 4] = [Axis::P1X, Axis::P1Y, Axis::P2X, Axis::P2Y];

    /// The port this axis is read from.
    pub fn port(self) -> u8 {
        AXIS_PORTS[self as usize]
    }
}

/// What the viewer is holding down, as a set rather than a stream of presses.
///
/// A joystick is a *level*, so this is deliberately not the key queue: two
/// producers pushing bytes cannot express "still held", which is the whole
/// quantity a stick has. One flag per direction rather than a signed axis
/// because both opposites can genuinely be down at once, and the board — not
/// the page — decides what that means.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Held {
    /// Per stick, in [`Axis`] order of sticks: `[stick 1, stick 2]`.
    pub up: [bool; 2],
    pub down: [bool; 2],
    pub left: [bool; 2],
    pub right: [bool; 2],
    pub fire: [bool; 2],
}

impl Held {
    /// Nothing held — a centred stick with no button pressed.
    pub fn none() -> Self {
        Self::default()
    }

    /// Which way this axis is being pushed: `-1`, `0` or `+1`.
    ///
    /// **Both opposites held is centre, not "last one wins".** A real stick
    /// cannot be pushed two ways at once, and centre is the reading that
    /// corresponds to the hand actually on it. It also makes a lost key-up
    /// harmless in the common case: hold left, then press right, and the stick
    /// centres rather than staying jammed left.
    pub fn direction(&self, axis: Axis) -> i8 {
        let (positive, negative) = match axis {
            Axis::P1X => (self.right[0], self.left[0]),
            Axis::P1Y => (self.up[0], self.down[0]),
            Axis::P2X => (self.right[1], self.left[1]),
            Axis::P2Y => (self.up[1], self.down[1]),
        };
        i8::from(positive) - i8::from(negative)
    }
}

/// Bit positions in the wire mask, which is how a page reports what is held.
///
/// A mask and not ten form fields, because the page reports **all** of them on
/// every change: a set is the whole state, so one late or dropped request
/// cannot leave a key stuck down the way a stream of press/release events can.
pub mod bit {
    pub const P1_UP: u16 = 1 << 0;
    pub const P1_DOWN: u16 = 1 << 1;
    pub const P1_LEFT: u16 = 1 << 2;
    pub const P1_RIGHT: u16 = 1 << 3;
    pub const P1_FIRE: u16 = 1 << 4;
    pub const P2_UP: u16 = 1 << 5;
    pub const P2_DOWN: u16 = 1 << 6;
    pub const P2_LEFT: u16 = 1 << 7;
    pub const P2_RIGHT: u16 = 1 << 8;
    pub const P2_FIRE: u16 = 1 << 9;
    /// Every bit that means anything, so a stray one can be rejected rather
    /// than silently setting a direction nobody asked for.
    pub const ALL: u16 = 0x03FF;
}

impl Held {
    /// Read a wire mask. Bits outside [`bit::ALL`] are ignored.
    pub fn from_mask(mask: u16) -> Self {
        let on = |b: u16| mask & b != 0;
        Self {
            up: [on(bit::P1_UP), on(bit::P2_UP)],
            down: [on(bit::P1_DOWN), on(bit::P2_DOWN)],
            left: [on(bit::P1_LEFT), on(bit::P2_LEFT)],
            right: [on(bit::P1_RIGHT), on(bit::P2_RIGHT)],
            fire: [on(bit::P1_FIRE), on(bit::P2_FIRE)],
        }
    }

}

/// One axis's swing: which way, and since when.
#[derive(Debug, Clone, Copy, Default)]
struct Swing {
    dir: i8,
    since_ms: u64,
}

/// The board, as a booted machine holds it.
///
/// Owns no clock of its own — every entry point takes the time — so the ramp is
/// tested against injected values rather than by sleeping, the same as the
/// printer's idle close.
#[derive(Debug, Clone, Default)]
pub struct D7a {
    held: Held,
    swings: [Swing; 4],
    /// Whether a guest has ever addressed this board.
    ///
    /// Reported, never used to gate a read: the same honesty as the VDM-1's
    /// `C8h` latch. A viewer is offered a joystick on every booted session
    /// because we cannot know in advance which disks want one, and this is how
    /// a screen can say whether anything is actually listening.
    addressed: bool,
    /// Test-only override of the switch byte; see [`D7a::force_switches`].
    #[cfg(test)]
    forced_switches: Option<u8>,
    /// Test-only override of the axes; see [`D7a::force_axes`].
    #[cfg(test)]
    forced_axes: Option<[u8; 4]>,
}

impl D7a {
    pub fn new() -> Self {
        Self::default()
    }

    /// Take a new set of held keys, at time `now_ms`.
    ///
    /// An axis whose direction is unchanged keeps its swing — so a page
    /// re-reporting the same keys every 150 ms does not restart the ramp, which
    /// would pin every axis near centre for ever and is the defect a
    /// browser-side timer would have had.
    pub fn set_held(&mut self, held: Held, now_ms: u64) {
        self.held = held;
        for axis in Axis::ALL {
            let want = held.direction(axis);
            let swing = &mut self.swings[axis as usize];
            if swing.dir != want {
                *swing = Swing { dir: want, since_ms: now_ms };
            }
        }
    }

    /// This axis's deflection at `now_ms`, as the guest's converter reads it.
    ///
    /// Centre is zero and the two directions are the two signs, which is what
    /// ADCTEST's zero adjustment says the hardware does.
    pub fn axis(&self, axis: Axis, now_ms: u64) -> u8 {
        #[cfg(test)]
        if let Some(forced) = self.forced_axes {
            return forced[axis as usize];
        }
        let swing = self.swings[axis as usize];
        if swing.dir == 0 {
            return 0;
        }
        let held_ms = now_ms.saturating_sub(swing.since_ms).min(RAMP_MS);
        // Integer, and rounded down, so full scale is reached exactly at
        // RAMP_MS rather than one tick early or late.
        let magnitude = (i64::from(FULL) * held_ms as i64 / RAMP_MS as i64) as i8;
        if swing.dir > 0 { magnitude as u8 } else { (-magnitude) as u8 }
    }

    /// The switch byte: **active low**, so no button pressed is `0xFF`.
    ///
    /// One bit per stick for now, and the bits are the ones ADCTEST watches:
    /// stick 1's first switch and stick 2's first switch. The other three
    /// switches per stick are real on the hardware and have no key, so they
    /// stay high — a stick with three buttons nobody is pressing, which is
    /// exactly what an unwired switch reads.
    pub fn switches(&self) -> u8 {
        #[cfg(test)]
        if let Some(forced) = self.forced_switches {
            return forced;
        }
        let mut byte = 0xFFu8;
        if self.held.fire[0] {
            byte &= !P1_FIRE_BIT;
        }
        if self.held.fire[1] {
            byte &= !P2_FIRE_BIT;
        }
        byte
    }

    /// Force the four axis bytes, for a *deterministic* probe.
    ///
    /// Test-only, and the reason it exists is worth stating: the ramp is
    /// wall-clock, which makes a run unrepeatable, and an experiment that
    /// compares two runs needs them to differ in exactly one thing. Forcing the
    /// value takes the clock out.
    #[cfg(test)]
    pub fn force_axes(&mut self, values: Option<[u8; 4]>) {
        self.forced_axes = values;
    }

    /// Force the switch byte, for probing which switch a program uses.
    ///
    /// Test-only. `Held` carries one button per stick because that is what the
    /// keyboard offers; the board has four per stick, and which of them a game
    /// reads is a question only the game can answer.
    #[cfg(test)]
    pub fn force_switches(&mut self, byte: Option<u8>) {
        self.forced_switches = byte;
    }

    /// Answer a port read, or `None` if this board does not own the port.
    pub fn port_in(&mut self, port: u8, now_ms: u64) -> Option<u8> {
        if !PORTS.contains(&port) {
            return None;
        }
        self.addressed = true;
        if port == SWITCH_PORT {
            return Some(self.switches());
        }
        let axis = Axis::ALL.iter().copied().find(|a| a.port() == port)?;
        Some(self.axis(axis, now_ms))
    }

    /// A write to one of our ports.
    ///
    /// ADCTEST does exactly one (`OUT 18h,00`) and SPACEWAR none, so nothing
    /// here depends on it — but it is still this board being addressed, which
    /// is worth recording for the same reason the reads are.
    pub fn port_out(&mut self, port: u8, _value: u8) -> bool {
        if PORTS.contains(&port) {
            self.addressed = true;
            return true;
        }
        false
    }

    /// Has a guest touched this board? Reported, never used to gate a read.
    pub fn addressed(&self) -> bool {
        self.addressed
    }

}

/// Stick 1's fire button, in the switch byte.
const P1_FIRE_BIT: u8 = 0x01;
/// Stick 2's fire button.
const P2_FIRE_BIT: u8 = 0x10;

#[cfg(test)]
mod tests {
    use super::*;

    fn held_right(stick: usize) -> Held {
        let mut h = Held::none();
        h.right[stick] = true;
        h
    }

    #[test]
    fn test_a_centred_stick_reads_zero() {
        // ADCTEST's zero adjustment: centre is 00, not mid-scale.
        let b = D7a::new();
        for axis in Axis::ALL {
            assert_eq!(b.axis(axis, 10_000), 0, "{axis:?}");
        }
    }

    #[test]
    fn test_no_button_pressed_is_active_high() {
        assert_eq!(D7a::new().switches(), 0xFF, "active low: nothing pressed is all ones");
    }

    #[test]
    fn test_a_press_clears_only_its_own_bit() {
        let mut b = D7a::new();
        let mut h = Held::none();
        h.fire[0] = true;
        b.set_held(h, 0);
        assert_eq!(b.switches(), !P1_FIRE_BIT, "0xFE: every switch high but stick 1's");
        h.fire[1] = true;
        b.set_held(h, 0);
        assert_eq!(b.switches(), !(P1_FIRE_BIT | P2_FIRE_BIT), "0xEE, as ADCTEST read back");
        // And the three switches per stick that have no key stay high.
        assert_eq!(b.switches() & 0x0E, 0x0E, "stick 1's unwired switches");
        assert_eq!(b.switches() & 0xE0, 0xE0, "stick 2's unwired switches");
    }

    /// **The ramp is the feature**: a key has no magnitude, so time supplies
    /// one. Centre at the press, full at [`RAMP_MS`], and monotonic between.
    #[test]
    fn test_a_held_key_swings_progressively() {
        let mut b = D7a::new();
        b.set_held(held_right(0), 1_000);
        assert_eq!(b.axis(Axis::P1X, 1_000), 0, "centre at the moment of the press");
        let quarter = b.axis(Axis::P1X, 1_000 + RAMP_MS / 4) as i8;
        let half = b.axis(Axis::P1X, 1_000 + RAMP_MS / 2) as i8;
        let full = b.axis(Axis::P1X, 1_000 + RAMP_MS) as i8;
        assert!(0 < quarter && quarter < half && half < full, "{quarter} {half} {full}");
        assert_eq!(full, FULL, "full deflection exactly at RAMP_MS");
        assert_eq!(b.axis(Axis::P1X, 1_000 + RAMP_MS * 10) as i8, FULL, "and it stays there");
    }

    #[test]
    fn test_the_two_directions_are_the_two_signs() {
        let mut b = D7a::new();
        b.set_held(held_right(0), 0);
        let right = b.axis(Axis::P1X, RAMP_MS) as i8;
        let mut h = Held::none();
        h.left[0] = true;
        b.set_held(h, 0);
        let left = b.axis(Axis::P1X, RAMP_MS) as i8;
        assert_eq!(right, FULL);
        assert_eq!(left, -FULL);
        assert_eq!(right, -left, "symmetric, so neither side turns faster");
    }

    /// A page re-reporting the same keys must not restart the swing, or every
    /// axis sits near centre for ever — the defect a browser-side timer has.
    #[test]
    fn test_repeating_the_same_keys_does_not_restart_the_ramp() {
        let mut b = D7a::new();
        b.set_held(held_right(0), 0);
        for t in (0..=RAMP_MS).step_by(150) {
            b.set_held(held_right(0), t); // the page polls; nothing changed
        }
        assert_eq!(b.axis(Axis::P1X, RAMP_MS) as i8, FULL, "the swing survived the polling");
    }

    #[test]
    fn test_releasing_returns_to_centre_at_once() {
        let mut b = D7a::new();
        b.set_held(held_right(0), 0);
        assert_eq!(b.axis(Axis::P1X, RAMP_MS) as i8, FULL);
        b.set_held(Held::none(), RAMP_MS);
        assert_eq!(b.axis(Axis::P1X, RAMP_MS), 0, "a sprung stick centres when let go");
    }

    /// Both opposites down is centre — a real stick cannot be pushed two ways,
    /// and it makes a lost key-up recoverable by pressing the other direction.
    #[test]
    fn test_opposite_keys_centre_the_axis() {
        let mut b = D7a::new();
        let mut h = Held::none();
        h.left[0] = true;
        h.right[0] = true;
        b.set_held(h, 0);
        assert_eq!(b.axis(Axis::P1X, RAMP_MS), 0);
        // And the rule lives in `direction`, so it is checked there too rather
        // than only through the board that consults it.
        assert_eq!(h.direction(Axis::P1X), 0);
    }

    #[test]
    fn test_the_two_sticks_are_independent() {
        let mut b = D7a::new();
        let mut h = Held::none();
        h.right[0] = true;
        h.up[1] = true;
        b.set_held(h, 0);
        assert_eq!(b.axis(Axis::P1X, RAMP_MS) as i8, FULL, "1X");
        assert_eq!(b.axis(Axis::P1Y, RAMP_MS), 0, "1Y untouched");
        assert_eq!(b.axis(Axis::P2X, RAMP_MS), 0, "2X untouched");
        assert_eq!(b.axis(Axis::P2Y, RAMP_MS) as i8, FULL, "2Y");
    }

    #[test]
    fn test_the_board_answers_its_own_ports_and_no_others() {
        let mut b = D7a::new();
        for port in PORTS {
            assert!(b.port_in(port, 0).is_some(), "{port:#04x} is ours");
        }
        for port in [0x00u8, 0x0E, 0x0F, 0x10, 0x17, 0x1D, 0x40, 0xFF] {
            assert!(b.port_in(port, 0).is_none(), "{port:#04x} is not ours");
        }
    }

    #[test]
    fn test_the_axis_ports_are_read_in_adctests_row_order() {
        assert_eq!(Axis::P1X.port(), 0x19);
        assert_eq!(Axis::P1Y.port(), 0x1A);
        assert_eq!(Axis::P2X.port(), 0x1B);
        assert_eq!(Axis::P2Y.port(), 0x1C);
        assert_eq!(SWITCH_PORT, 0x18, "the port ADCTEST also writes once");
    }

    /// Reported, never used to gate a read — the VDM-1's `C8h` lesson.
    #[test]
    fn test_addressing_is_recorded_but_gates_nothing() {
        let mut b = D7a::new();
        assert!(!b.addressed());
        assert_eq!(b.port_in(AXIS_PORTS[0], 0), Some(0), "answered before anyone asked");
        assert!(b.addressed(), "and the asking is now on record");
    }

    /// A swing must not wrap when the clock is behind the press — a page's
    /// report and the guest's read do not arrive in a guaranteed order.
    #[test]
    fn test_a_clock_behind_the_press_does_not_wrap() {
        let mut b = D7a::new();
        b.set_held(held_right(0), 10_000);
        assert_eq!(b.axis(Axis::P1X, 9_000), 0, "saturating, not a huge deflection");
    }
}
