//! Which processor the CP/M machines run on — the Z80, or the 8080 it grew
//! from.
//!
//! One list for all three configuration screens, the way
//! [`super::uart::UART_CHOICES`] serves the virtual modem's port and
//! [`super::console::MACHINE_CHOICES`] serves a booted disk's hardware.
//!
//! **This is the one CP/M setting that reaches both machines.** Where the
//! console, the backspace key and the boot image describe a *booted disk*, and
//! the UART profile describes the *emulator*, the CPU is underneath both: the
//! emulator's `A>` runs transient programs on it and a booted disk runs its
//! whole operating system on it. So both [`super::Cpm`] and
//! [`super::boot_machine::BootMachine`] take their processor from here.
//!
//! ## Why the Z80 is the default, and what choosing the 8080 costs
//!
//! The Altair shipped with an 8080 and every MITS disk in the sample set is
//! 8080 code, so the 8080 is the more literal machine. The Z80 is the default
//! anyway because it is a strict superset that runs all of that, and Altairs
//! were very commonly fitted with Z80 upgrade boards.
//!
//! **This setting used to cost the terminal**, and that is worth recording
//! because it was the deciding argument for the default and is no longer
//! true. The bundled terminal was Z80 code (`EGT80.COM`, retired in 0.9.2);
//! placed on drive A: on first launch, it executed a Z80-only opcode as
//! something else on an 8080 and took CP/M down with it. `EGT8080.COM` is the
//! same terminal built to the 8080's instruction set — 8080 opcodes being a
//! strict subset, it runs on *either* setting — and it is the only one
//! shipped now. Choosing the 8080 now
//! costs the `hbios_*` modem profiles' usual clientele (RomWBW software is
//! Z80/Z180 code, though our HBIOS itself answers an 8080 perfectly well) and
//! nothing else we ship.
//!
//! iz80's 8080 mode is a faithful one rather than a relabelled Z80 — real
//! parity instead of overflow, the 8080's subtract half-carry, its own `DAA`,
//! and the unused flag bits forced — which is what makes the setting worth
//! offering at all.
//!
//! One other combination is worth knowing about rather than guarding against:
//! the `hbios_*` [`super::uart`] profiles emulate **RomWBW**, which is Z80/Z180
//! firmware, so the software that looks for it is Z80 code and will not run on
//! an 8080 whatever we answer. The gateway does not refuse the pairing — the
//! guest is free to probe and find nothing, exactly as it would on real 8080
//! iron — but nothing about `hbios_*` becomes useful by selecting the 8080.

use iz80::Cpu;

/// The `cpm_cpu` value for a Zilog Z80.
pub const CPU_Z80: &str = "z80";

/// The `cpm_cpu` value for an Intel 8080.
pub const CPU_8080: &str = "8080";

/// What `cpm_cpu` is when nothing says otherwise.
pub const DEFAULT_CPU: &str = CPU_Z80;

/// The choices for `cpm_cpu`, `(value, label)`.
///
/// Both labels fit 26 characters, because the telnet screen truncates to that
/// on a 40-column PETSCII terminal and a label that arrives cut in half cannot
/// say what it means.
///
/// Neither names a cost any more: `EGT8080.COM` runs on both, so the choice is
/// now only about which processor the software you are running expects.
///
/// **"most", not "all".** The Z80's instruction set contains the 8080's, so
/// almost anything written for an 8080 runs — but the two processors do not
/// agree on everything, and where they differ the 8080 program is entitled to
/// notice. `DCR A` sets parity on an 8080 and overflow on a Z80, so a period
/// diagnostic that identifies its host that way is *right* to fail here; that
/// is the case `cpm_cpu = 8080` exists for. Saying "runs 8080 code too" told an
/// operator the setting could not matter, which is the one thing it must not
/// say. It reads "runs most 8080 code" rather than "runs most 8080 code too"
/// only because 26 characters is all a 40-column PETSCII row gives, and a label
/// that arrives cut in half says less than a shorter one that does not.
pub const CPU_CHOICES: &[(&str, &str)] = &[
    (CPU_Z80, "Z80 (runs most 8080 code)"),
    (CPU_8080, "8080 (what MITS shipped)"),
];

/// Whether `value` selects the 8080.
///
/// Anything unrecognised reads as the Z80 rather than refusing: this is
/// hand-editable in `egateway.conf`, and the Z80 is the setting that runs
/// every disk here, so a typo costs nothing rather than silently narrowing
/// what the machine can execute.
pub fn is_8080(value: &str) -> bool {
    value.trim().eq_ignore_ascii_case(CPU_8080)
}

/// What to show for the current `cpm_cpu` setting.
///
/// Reads through [`is_8080`] rather than matching the string, so a hand-edited
/// typo is *displayed* as the processor the gateway is actually giving it — the
/// failure this exists to prevent is a screen that agrees with the config file
/// and disagrees with the machine.
pub fn cpu_label(value: &str) -> &'static str {
    let want = if is_8080(value) { CPU_8080 } else { CPU_Z80 };
    CPU_CHOICES.iter().find(|(v, _)| *v == want).map(|(_, l)| *l).unwrap_or(want)
}

/// The CPU a `cpm_cpu` value names, ready to run.
///
/// The single place either machine turns the setting into a processor, so the
/// emulator and a booted disk cannot end up disagreeing about what the operator
/// asked for.
pub fn new_cpu(value: &str) -> Cpu {
    if is_8080(value) {
        Cpu::new_8080()
    } else {
        Cpu::new_z80()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iz80::Machine;

    #[test]
    fn test_the_default_is_the_z80() {
        assert_eq!(DEFAULT_CPU, CPU_Z80);
        assert!(!is_8080(DEFAULT_CPU));
        assert_eq!(cpu_label(DEFAULT_CPU), CPU_CHOICES[0].1);
    }

    #[test]
    fn test_every_choice_resolves_to_itself() {
        for (value, _) in CPU_CHOICES {
            assert_eq!(is_8080(value), *value == CPU_8080, "{value} resolved to the other CPU");
            assert!(
                cpu_label(value) == CPU_CHOICES.iter().find(|(v, _)| v == value).unwrap().1,
                "{value} is not labelled as itself"
            );
        }
    }

    /// A value neither choice matches must read *and display* as the Z80.
    ///
    /// Both halves matter: a config file is hand-editable, and a screen that
    /// showed "8080" while the machine ran a Z80 would send somebody looking
    /// for the fault in their software.
    #[test]
    fn test_an_unrecognised_setting_reads_as_the_z80() {
        for bad in ["", "  ", "Z-80", "i8080a", "8085", "nonsense"] {
            assert!(!is_8080(bad), "{bad:?} must not select the 8080");
            assert_eq!(cpu_label(bad), CPU_CHOICES[0].1, "{bad:?} must display as the Z80");
        }
        // Spelling and spacing are the operator's, not ours.
        for good in [" 8080", "8080 ", "8080"] {
            assert!(is_8080(good), "{good:?} must select the 8080");
        }
    }

    /// **The setting really changes the processor, proved by an instruction the
    /// two decode differently.**
    ///
    /// `0x08` is `EX AF,AF'` on a Z80 and an unused no-op on an 8080, so one
    /// core swaps the accumulator with its shadow and the other leaves it
    /// alone. Asserting on the *behaviour* rather than on which constructor was
    /// called is the whole point: a selector that returned the right label and
    /// the same CPU would pass every other test here.
    #[test]
    fn test_the_two_settings_decode_differently() {
        struct Mem([u8; 65536]);
        impl Machine for Mem {
            fn peek(&mut self, a: u16) -> u8 {
                self.0[a as usize]
            }
            fn poke(&mut self, a: u16, v: u8) {
                self.0[a as usize] = v;
            }
            fn port_in(&mut self, _: u16) -> u8 {
                0xFF // an unclaimed port, as the real machines answer
            }
            fn port_out(&mut self, _: u16, _: u8) {}
        }
        // A: = 0x11, shadow AF cleared, then EX AF,AF' (or nothing at all).
        let run = |setting: &str| {
            let mut cpu = new_cpu(setting);
            let mut mem = Mem([0; 65536]);
            mem.0[0] = 0x3E; // LD A,0x11
            mem.0[1] = 0x11;
            mem.0[2] = 0x08; // EX AF,AF'  /  8080: unused, no effect
            cpu.registers().set_pc(0);
            cpu.execute_instruction(&mut mem);
            cpu.execute_instruction(&mut mem);
            cpu.registers().a()
        };
        assert_eq!(run(CPU_8080), 0x11, "the 8080 must not know EX AF,AF'");
        assert_ne!(run(CPU_Z80), 0x11, "the Z80 must swap the accumulator away");
    }
}
