//! How fast a booted guest is allowed to run.
//!
//! Nothing paced the CPU before this. The session pump naps when a guest is
//! *idle* — polling its console for a key it has not been given — but a guest
//! that is working runs at whatever the host can manage.
//!
//! **Two figures, and they are not the same claim.** Stepped in a tight loop with
//! nothing else to do, this machine manages 21 million instructions a second at
//! 6.73 cycles each — an effective **141 MHz, seventy-one times an Altair
//! 8800's 2 MHz** (`boot_machine::tests::measure_how_fast_a_booted_guest_runs`).
//! That is the emulation's ceiling. A *live session* has telnet I/O, screen
//! publishing and task yields around it, and measured while actually playing
//! SPACEWAR it reached **16.65 MHz — about eight times** period speed. Eight
//! times is what an operator meets, and it is still far too fast to play; the
//! ceiling is what the governor has to be able to hold down to.
//!
//! For a compile either figure is a gift. For anything that keeps time by
//! counting — a game, a delay loop, a music routine — it is the difference
//! between usable and not.
//!
//! Held to `2`, the same SPACEWAR session measured **1.95 MHz**, inside 3% of
//! target.
//!
//! # Cycles, not instructions
//!
//! `iz80` counts cycles per instruction from **separate tables for the 8080 and
//! the Z80**, so pacing on `cycle_count()` is as accurate as the emulated
//! instruction mix — no average has to be assumed on our side. This is a
//! deliberate contrast with [`super::boot_machine::BootMachine::dazzler_phase`],
//! which had no such counter available and says so.
//!
//! # Where the governor lives, and why it is not in `step`
//!
//! In the **session pump**, never in `BootMachine::step`. Two reasons, and the
//! second is the one that would hurt:
//!
//! * A machine is a machine whoever is driving it. The thing being limited is a
//!   *session* — a person watching a screen in real time — not the emulation.
//! * Every live gate in this project drives `step` directly, in a loop, hundreds
//!   of millions of times. A governor inside `step` would slow the whole test
//!   suite by the same seventy-one times it slows a guest, which is a way of
//!   making the disk survey and the boot gates unrunnable.
//!
//! # The clock is virtual time against real time
//!
//! Not a sleep per instruction, which no operating system's timer granularity
//! could serve. The guest accrues cycles; those cycles *are* a duration at the
//! chosen clock; and the pump sleeps whenever the guest has got ahead of the
//! wall by more than [`SLACK`]. A guest that goes idle and is napped by the
//! existing idle path simply falls behind and the governor has nothing to do,
//! which is the right answer rather than a special case.

use std::time::Duration;

/// What `cpm_boot_speed` may say, for the three config UIs.
///
/// A single list so telnet, the web UI and the desktop enumerate the same
/// choices, exactly as `uart.rs` and `console.rs` do for theirs. `(value,
/// label)`; the label is what a 40-column PETSCII screen shows, so it is short.
pub const SPEED_CHOICES: &[(&str, &str)] = &[
    ("auto", "Period speed for the CPU"),
    ("unlimited", "As fast as this host can"),
    ("2", "2 MHz (Altair 8800)"),
    ("4", "4 MHz (Z80 systems)"),
    ("6", "6 MHz"),
    ("8", "8 MHz"),
];

/// The default: the speed the chosen processor actually ran at.
pub const DEFAULT: &str = "auto";

/// An 8080's clock on the machines this gateway emulates.
///
/// The MITS Altair 8800 ran its 8080 at 2 MHz, and every 8080 board here is of
/// that generation.
pub const MHZ_8080: f64 = 2.0;

/// A Z80's clock, for the same purpose.
///
/// 4 MHz is the Z80A used by the Cromemco and RC2014-era machines whose disks
/// boot here. Stated rather than measured — nothing in this program can measure
/// a 1970s crystal — and it is the figure `auto` uses when `cpm_cpu` says `z80`.
pub const MHZ_Z80: f64 = 4.0;

/// How far ahead of the wall clock a guest may get before the pump sleeps.
///
/// **A latency budget, not a tuning knob.** A sleep per instruction is
/// impossible — no timer has that granularity — so the guest necessarily runs in
/// bursts, and this is how long a burst is. It bounds how stale the joystick can
/// be when the guest next reads it, and equally how coarse the guest's own sense
/// of time is. 5 ms is a third of a 60 Hz frame: fine enough that a game's
/// motion is smooth and a keypress is not noticeably late, coarse enough that
/// the pump sleeps a couple of hundred times a second rather than thousands.
pub const SLACK: Duration = Duration::from_millis(5);

/// What a `cpm_boot_speed` setting means, before any CPU is known.
///
/// **The one reading of a setting.** `mhz_for` and `label_for` each had their
/// own, and they drifted: `off`, `none` and `0` ran the guest unpaced while the
/// desktop combo labelled them "Period speed for the CPU". Both now ask this, so
/// the number a guest runs at and the words a screen shows cannot part again.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Speed {
    /// The chosen processor's own clock.
    Auto,
    /// No pacing at all.
    Unlimited,
    /// A clock in MHz, already floored at [`MIN_MHZ`].
    Mhz(f64),
}

/// Read a setting.
///
/// An unrecognised setting reads as `Auto` rather than as unlimited: a typo in
/// a config file should not silently remove the governor.
///
/// **Any spelling of zero is unlimited**, not only the literal `0`: `0.0` and
/// `00` are the same number, and an operator who typed one meant what `0`
/// means.  It used to read as `auto` because it fell past the word match into
/// the numeric arm, whose `> 0` refusal then sent it to the default -- a
/// distinction by spelling that nobody could have intended.  A negative clock
/// is still nonsense and still reads as `Auto`.
pub fn parse(setting: &str) -> Speed {
    match setting.trim().to_ascii_lowercase().as_str() {
        "unlimited" | "off" | "none" => Speed::Unlimited,
        "auto" | "" => Speed::Auto,
        // Floored at `MIN_MHZ`: `1e-20` made the first nap overflow a
        // `Duration` and panic the session, and `0.0001` slept for hours inside
        // the pump where ESC-ESC cannot reach.  `inf` is not a clock either.
        other => match other.parse::<f64>() {
            Ok(0.0) => Speed::Unlimited,
            Ok(m) if m.is_finite() && m > 0.0 => Speed::Mhz(m.max(MIN_MHZ)),
            _ => Speed::Auto,
        },
    }
}

/// The clock `setting` asks for, given the CPU in use.
///
/// `None` means unlimited — no pacing at all, which is what every version before
/// this did.  See [`parse`] for how a setting is read.
pub fn mhz_for(setting: &str, cpu: &str) -> Option<f64> {
    let period = if cpu.trim().eq_ignore_ascii_case("8080") { MHZ_8080 } else { MHZ_Z80 };
    match parse(setting) {
        Speed::Unlimited => None,
        Speed::Auto => Some(period),
        Speed::Mhz(m) => Some(m),
    }
}

/// The slowest clock a setting may ask for.  Slower than any real machine of
/// the period, and fast enough that no nap the governor asks for is long.
pub const MIN_MHZ: f64 = 0.1;

/// The longest single nap.  The pump re-checks after it, so a long debt is
/// still paid -- in pieces short enough that the session stays responsive.
pub const MAX_NAP: Duration = Duration::from_secs(1);

/// How far behind the wall a guest may fall before the governor stops
/// counting the difference -- see [`Governor::behind`].
pub const MAX_ARREARS: Duration = Duration::from_millis(50);

/// The row of [`SPEED_CHOICES`] a setting *means*, if the list carries it.
///
/// Every surface that marks or cycles the current choice goes through this, so
/// `off`, `0` and `2.0` sit on the row that runs the guest the same way rather
/// than on no row at all -- matching by spelling sent the telnet key back to
/// `auto` from `off` and left the desktop combo highlighting nothing.
pub fn choice_index(setting: &str) -> Option<usize> {
    let speed = parse(setting);
    SPEED_CHOICES.iter().position(|(value, _)| parse(value) == speed)
}

/// The label for a setting, for a screen that shows the current value.
///
/// From [`parse`], like [`mhz_for`], so the label always describes the clock
/// the guest is actually held to.  A listed choice is found by what it
/// *means*, so `2.0` shows the `2` row's label.
pub fn label_for(setting: &str) -> String {
    if let Some(i) = choice_index(setting) {
        return SPEED_CHOICES[i].1.to_string();
    }
    match parse(setting) {
        // A number the list does not carry is still a valid setting.
        Speed::Mhz(m) => format!("{m} MHz"),
        // Both are listed, so this is unreachable while the list holds them;
        // answering rather than panicking keeps a trimmed list survivable.
        Speed::Auto | Speed::Unlimited => SPEED_CHOICES[0].1.to_string(),
    }
}

/// The setting in the fewest characters that still say it, for a status row.
///
/// The **resolved** clock rather than the setting's name: `auto` on its own tells
/// an operator nothing about what their machine is doing, and the number is both
/// shorter and the actual answer. A 40-column PETSCII screen shows this beside a
/// processor name, so it has to be brief.
pub fn short_label(setting: &str, cpu: &str) -> String {
    match mhz_for(setting, cpu) {
        None => "unlimited".to_string(),
        Some(mhz) if mhz.fract() == 0.0 => format!("{mhz:.0} MHz"),
        Some(mhz) => format!("{mhz} MHz"),
    }
}

/// A choice's label with `auto` resolved for the CPU in use.
///
/// `auto` on its own does not tell an operator what their machine is doing, and
/// the telnet screen already shows the number because it shows the resolved
/// value. The web select and the desktop combo showed the phrase alone, so the
/// same setting read differently on three surfaces; this is what makes them
/// agree.
pub fn choice_label(value: &str, label: &str, cpu: &str) -> String {
    if value.eq_ignore_ascii_case("auto") {
        match mhz_for(value, cpu) {
            Some(mhz) if mhz.fract() == 0.0 => return format!("{label} ({mhz:.0} MHz)"),
            Some(mhz) => return format!("{label} ({mhz} MHz)"),
            None => {}
        }
    }
    label.to_string()
}

/// Milliseconds since this process started.
///
/// One shared monotonic base, so everything that reasons about real time here —
/// the joystick's ramp, its idle release, and this module's governor — measures
/// from the same zero and can be compared. It lives here rather than beside the
/// first feature that wanted it, because a clock is not a joystick's property.
///
/// **Deliberately wall time**, on a machine whose only other clock is its
/// instruction count: how long a finger has been on a key, and how long a guest
/// has been running, are facts about the room rather than about the emulation.
pub fn now_ms() -> u64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// Holds a guest to a clock by comparing its cycles against real time.
///
/// Deliberately owns no clock: every entry point is given the time, so the whole
/// thing is tested against supplied values rather than by sleeping — the same
/// choice the printer's idle close and the joystick's ramp made.
#[derive(Debug, Clone)]
pub struct Governor {
    cycles_per_sec: f64,
    /// Cycles already accounted for, so a re-baseline does not double-count.
    base_cycles: u64,
    /// Real time when the run began, in milliseconds from the caller's clock.
    base_ms: u64,
}

impl Governor {
    /// A governor for `mhz`, starting now at `cycles`.
    pub fn new(mhz: f64, cycles: u64, now_ms: u64) -> Governor {
        Governor { cycles_per_sec: mhz * 1e6, base_cycles: cycles, base_ms: now_ms }
    }

    /// How long to sleep so the guest is no longer ahead of the wall, if it is.
    ///
    /// `None` means it is not ahead — either it is keeping up or it has fallen
    /// behind, and **falling behind is not made up for**. A guest that was
    /// napped while idle, or descheduled, or reading a disk, must not then be
    /// given a burst at seventy-one times speed to "catch up": that is exactly
    /// the symptom being fixed, arriving in bursts instead of continuously.
    ///
    /// **Forgetting is done here, not by the caller.**  Only the idle nap used
    /// to re-baseline, so any *other* stall -- a console write on a slow link,
    /// a printer job being rendered, the host descheduling the task -- left
    /// the guest behind its base, and it then ran unpaced until it had caught
    /// up: the burst this doc says cannot happen.  Past [`MAX_ARREARS`] the
    /// base moves to now, so the debt is dropped the moment it exists.
    pub fn behind(&mut self, cycles: u64, now_ms: u64) -> Option<Duration> {
        let virtual_secs = cycles.saturating_sub(self.base_cycles) as f64 / self.cycles_per_sec;
        let real_secs = now_ms.saturating_sub(self.base_ms) as f64 / 1000.0;
        let ahead = virtual_secs - real_secs;
        if ahead < -MAX_ARREARS.as_secs_f64() {
            self.rebase(cycles, now_ms);
            return None;
        }
        if ahead <= SLACK.as_secs_f64() {
            return None;
        }
        Some(Duration::try_from_secs_f64(ahead).unwrap_or(MAX_NAP).min(MAX_NAP))
    }

    /// Start counting again from here.
    ///
    /// For the moment a guest stops being paced and starts again — after a long
    /// idle nap, say — so the arrears it built up while nobody was playing do
    /// not become a free run at full speed.
    pub fn rebase(&mut self, cycles: u64, now_ms: u64) {
        self.base_cycles = cycles;
        self.base_ms = now_ms;
    }

    /// The clock this governor is holding, in MHz — for a status line.
    pub fn mhz(&self) -> f64 {
        self.cycles_per_sec / 1e6
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_auto_is_the_speed_of_the_chosen_processor() {
        assert_eq!(mhz_for("auto", "8080"), Some(MHZ_8080));
        assert_eq!(mhz_for("auto", "z80"), Some(MHZ_Z80));
        // Case and padding are how a config file really looks.
        assert_eq!(mhz_for("  AUTO ", " 8080 "), Some(MHZ_8080));
    }

    #[test]
    fn test_unlimited_is_the_only_way_to_turn_it_off() {
        for off in ["unlimited", "off", "none", "0", "UNLIMITED"] {
            assert_eq!(mhz_for(off, "z80"), None, "{off}");
        }
    }

    /// **A typo must not silently remove the governor.** Reading an unknown
    /// setting as "unlimited" would turn a misspelling into a 71x machine, which
    /// is the failure this whole module exists to prevent.
    #[test]
    fn test_an_unknown_setting_falls_back_to_the_period_speed() {
        assert_eq!(mhz_for("nonsense", "8080"), Some(MHZ_8080));
        assert_eq!(mhz_for("-3", "z80"), Some(MHZ_Z80));
        assert_eq!(mhz_for("", "z80"), Some(MHZ_Z80));
    }

    /// What the desktop combo shows must be the clock the guest runs at.
    ///
    /// `label_for` had its own reading and labelled `off`/`none`/`0` as period
    /// speed while `mhz_for` ran them unpaced; `0.0` read as auto while `0` read
    /// as unlimited.  Classified three ways for both functions, every input.
    #[test]
    fn test_the_label_and_the_clock_agree_about_every_setting() {
        let unlimited = SPEED_CHOICES.iter().find(|(v, _)| *v == "unlimited").unwrap().1;
        let auto = SPEED_CHOICES[0].1;
        for s in [
            "off", "none", "0", "0.0", "00", "unlimited", "UNLIMITED", "auto", "", "2", "2.0",
            "4.5", "garbage", "-3", "inf", "nan", "0.0001",
        ] {
            let clock = match mhz_for(s, "z80") {
                None => "unlimited",
                Some(m) if m == MHZ_Z80 && parse(s) == Speed::Auto => "auto",
                Some(_) => "paced",
            };
            let label = label_for(s);
            let shown = if label == unlimited {
                "unlimited"
            } else if label == auto {
                "auto"
            } else {
                "paced"
            };
            assert_eq!(shown, clock, "{s:?}: labelled {label:?}");
        }
        assert_eq!(mhz_for("0.0", "z80"), None, "every spelling of zero is unlimited");
        assert_eq!(label_for("2.0"), label_for("2"), "a listed clock by any spelling");
        assert_eq!(label_for("4.5"), "4.5 MHz");
    }

    #[test]
    fn test_a_number_is_taken_as_megahertz() {
        assert_eq!(mhz_for("2", "z80"), Some(2.0));
        assert_eq!(mhz_for("3.5", "8080"), Some(3.5));
    }

    #[test]
    fn test_every_choice_resolves_and_is_labelled() {
        for (value, label) in SPEED_CHOICES {
            assert!(!label.is_empty(), "{value} needs a label");
            assert_eq!(&label_for(value), label, "{value} must label itself");
            // Every listed choice must be meaningful for both processors.
            for cpu in ["8080", "z80"] {
                let got = mhz_for(value, cpu);
                if *value == "unlimited" {
                    assert!(got.is_none());
                } else {
                    assert!(got.is_some_and(|m| m > 0.0), "{value} on {cpu}");
                }
            }
        }
        // Labels are read on a 40-column PETSCII screen beside a key and a
        // prefix, so they cannot be long.
        for (_, label) in SPEED_CHOICES {
            assert!(label.chars().count() <= 26, "{label:?} is too long for 40 columns");
        }
    }

    /// **The same setting must not read differently on three screens.** The
    /// telnet row shows the resolved clock because it shows a resolved value;
    /// the web select and the desktop combo showed the phrase alone, so `auto`
    /// told an operator nothing about what their machine was doing.
    #[test]
    fn test_auto_says_what_it_resolves_to() {
        let (v, l) = SPEED_CHOICES[0];
        assert_eq!(v, "auto", "the first choice is the one that needs resolving");
        assert_eq!(choice_label(v, l, "8080"), format!("{l} (2 MHz)"));
        assert_eq!(choice_label(v, l, "z80"), format!("{l} (4 MHz)"));
        // Every other choice already says what it is, so it is left alone.
        for (value, label) in &SPEED_CHOICES[1..] {
            assert_eq!(&choice_label(value, label, "z80"), label, "{value}");
        }
    }

    /// **A setting finds its row by meaning.**  The telnet key cycled from
    /// the row whose *spelling* matched, so `off` went back to `auto` instead
    /// of on from `unlimited`, and the desktop combo highlighted no row for an
    /// empty setting.  A number the list does not carry has no row.
    #[test]
    fn test_a_setting_finds_its_row_by_meaning() {
        let row = |v: &str| SPEED_CHOICES.iter().position(|(c, _)| *c == v);
        assert_eq!(choice_index("off"), row("unlimited"));
        assert_eq!(choice_index("0"), row("unlimited"));
        assert_eq!(choice_index("2.0"), row("2"));
        assert_eq!(choice_index(""), row("auto"));
        assert_eq!(choice_index(" AUTO "), row("auto"));
        assert_eq!(choice_index("3"), None);
        for (i, (value, _)) in SPEED_CHOICES.iter().enumerate() {
            assert_eq!(choice_index(value), Some(i), "{value}");
        }
    }

    #[test]
    fn test_the_default_is_a_choice_that_exists() {
        assert!(SPEED_CHOICES.iter().any(|(v, _)| *v == DEFAULT));
    }

    /// A guest running at exactly the chosen rate is never slept.
    #[test]
    fn test_a_guest_keeping_time_is_not_paced() {
        let mut g = Governor::new(2.0, 0, 0);
        // 2 MHz: 2,000 cycles is a millisecond.
        for ms in [1u64, 10, 100, 1_000] {
            assert_eq!(g.behind(2_000 * ms, ms), None, "on time at {ms} ms");
        }
    }

    /// Getting ahead is what the governor is for.
    #[test]
    fn test_a_guest_that_races_ahead_is_slept_by_the_excess() {
        let mut g = Governor::new(2.0, 0, 0);
        // A full second of cycles in no time at all.
        let nap = g.behind(2_000_000, 0).expect("way ahead");
        assert!(
            (nap.as_secs_f64() - 1.0).abs() < 0.01,
            "a second of virtual time owes about a second, got {nap:?}",
        );
        // Just inside the slack is left alone; just outside is not.
        let inside = 2_000 * (SLACK.as_millis() as u64);
        assert_eq!(g.behind(inside, 0), None, "within the slack, no sleep");
        assert!(g.behind(inside * 3, 0).is_some(), "well past the slack, slept");
    }

    /// **Arrears are not repaid.** A guest that fell behind — napped while idle,
    /// descheduled, waiting on a disk — must not be handed a burst at full speed
    /// to catch up, because arriving in bursts is the symptom being fixed.
    #[test]
    fn test_falling_behind_does_not_earn_a_fast_burst() {
        let mut g = Governor::new(2.0, 0, 0);
        // Ten seconds of wall clock, one second of cycles: far behind.
        assert_eq!(g.behind(2_000_000, 10_000), None, "behind, but not owed anything");
        // **The burst itself.**  A tenth of a second of cycles in no wall time
        // must be paced now -- not run free until nine seconds are made up.
        assert!(
            g.behind(2_000_000 + 200_000, 10_000).is_some(),
            "a guest that fell behind was handed a free run to catch up"
        );
    }

    /// A clock no machine had is floored, so no nap can be hours long or
    /// overflow a `Duration`; and no single nap is longer than [`MAX_NAP`].
    #[test]
    fn test_a_tiny_clock_cannot_hang_or_panic_the_pump() {
        for setting in ["1e-20", "0.0001", "0.00000001"] {
            assert_eq!(mhz_for(setting, "z80"), Some(MIN_MHZ), "{setting}");
        }
        assert_eq!(mhz_for("inf", "z80"), Some(MHZ_Z80), "infinity is not a clock");
        assert_eq!(label_for("0.0001"), format!("{MIN_MHZ} MHz"), "the label shows the floor");
        assert_eq!(label_for("inf"), SPEED_CHOICES[0].1, "and agrees about infinity");
        let mut g = Governor::new(MIN_MHZ, 0, 0);
        let nap = g.behind(u64::MAX, 0).expect("far ahead");
        assert!(nap <= MAX_NAP, "{nap:?}");
    }

    #[test]
    fn test_rebasing_forgets_the_arrears() {
        let mut g = Governor::new(2.0, 0, 0);
        // A long idle: 10 s of wall, no cycles.
        assert_eq!(g.behind(0, 10_000), None);
        g.rebase(0, 10_000);
        // From here, a second of cycles owes a second again.
        let nap = g.behind(2_000_000, 10_000).expect("ahead of the new base");
        assert!((nap.as_secs_f64() - 1.0).abs() < 0.01, "{nap:?}");
    }

    #[test]
    fn test_the_clock_is_reported_as_it_was_asked_for() {
        assert_eq!(Governor::new(2.0, 0, 0).mhz(), 2.0);
        assert_eq!(Governor::new(4.0, 0, 0).mhz(), 4.0);
    }

    /// A cycle counter that wraps or is read out of order must not produce a
    /// gigantic sleep.
    #[test]
    fn test_a_cycle_count_behind_the_base_is_not_a_huge_nap() {
        let mut g = Governor::new(2.0, 1_000_000, 1_000);
        assert_eq!(g.behind(0, 2_000), None, "saturating, not wrapping");
    }
}
