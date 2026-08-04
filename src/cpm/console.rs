//! Which machine a booted disk believes it is running on.
//!
//! The controllers in [`super::controller`] answer "what disk is this?". This
//! module answers the other half — "what does the guest print to, and type
//! from?" — and the two are genuinely separate questions. Every disk here
//! reached its own operating system through our 88-DCDD, 88-HDSK or Tarbell
//! board and then went looking for a console, and three of the six Tarbell
//! disks went looking somewhere we were not.
//!
//! # Why this is a setting and not a detection
//!
//! Because guessing is the mistake this path has already made twice. The sector
//! step was briefly autodetected by running four candidates and keeping
//! whichever printed something — and a *wrong* layout that scribbles at a
//! console beats a right one still loading, so the wrong answer won for five
//! disks. A console is worse, not better: what a guest polls is exactly as
//! ambiguous. TDISK04 sits on `IN 04h` forever, but so would any program
//! waiting on a keyboard that happens to be at `04h` on some *other* machine,
//! and there is no reply we could send that distinguishes them. So the operator
//! says which machine, once, and we are simply that machine.
//!
//! # Where the addresses come from
//!
//! Each entry is measured from a disk that demands it, not transcribed from a
//! table. The evidence is the guest's own driver, disassembled out of the
//! system tracks — the same class of evidence that settled the 88-HDSK write
//! bit (the disks carry the 8X300 firmware) and the Tarbell drive-select
//! polarity (a working guest writes `F2`). What a driver does to a register is
//! the register's definition as far as that driver is concerned, and a driver
//! is the only thing we have to satisfy.
//!
//! # The ready bit is not a detail
//!
//! Two of these boards report "a key is waiting" with a bit **clear**, and
//! getting that backwards does not produce a quiet machine — it produces a
//! machine that claims a keypress on every poll and reads a stream of garbage,
//! which looks like a corrupt disk. TDISK04 was measured parked on the `JNZ` at
//! `BED3`, spinning because our idle bus reads `0xFF` and bit 0 set means *not*
//! ready to it. Take the polarity from the guest, never from the board's name.

use super::uart::UartFamily;

/// A monitor ROM the machine carries, as the bytes it would have left behind.
///
/// Some consoles are not reached through a port at all: the guest `CALL`s an
/// address in a monitor ROM and lets the ROM do the talking. TDISK05 is one —
/// its BIOS assembles with `VIDEO EQU TRUE` and prints with a single
/// `CALL 0C019h`, the Processor Technology CUTER entry, having put the character
/// in `B` and cleared `A` to select output device 0.
///
/// We do not have CUTER, so the entry is **synthesised** — and that is the same
/// substitution [`super::boot`] already makes for a boot PROM we do not have,
/// documented the same way rather than quietly. What makes it honest is that
/// the guest cannot tell: these are real Z80 instructions at the real address,
/// doing what the real routine's caller is entitled to expect. It is not a host
/// trap, and nothing in the port dispatch knows it exists.
///
/// Deliberately a list of placements rather than one blob. Cromemco's 16FDC
/// cold-starts from a 4 KB ROM at `C000` with a dozen entry points, so the next
/// user of this is much larger than a six-byte stub, and the shape should not
/// have to change to admit it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RomImage {
    /// Where each run of bytes belongs.
    pub chunks: Vec<(u16, Vec<u8>)>,
}

/// What kind of monitor ROM a machine carries, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorRom {
    /// None: the guest reaches its console entirely through ports.
    None,
    /// A Processor Technology CUTER output entry at `C019`.
    ///
    /// One entry point, and that is measured rather than assumed: TDISK05's
    /// system tracks contain exactly one `CALL` into `C0xx` and no `JMP` at all,
    /// so nothing else in the ROM is ever reached. A guest that needed CUTER's
    /// keyboard or its clear-screen would show up here as more call sites.
    Cuter,
}

impl MonitorRom {
    /// The bytes this ROM would have left in the address space.
    ///
    /// Takes the console's data port because the stub has to *go* somewhere, and
    /// threading it through here keeps one machine's port in one place rather
    /// than repeating `05h` in a byte array that nobody would think to update.
    pub fn image(self, data_port: u8) -> Option<RomImage> {
        match self {
            MonitorRom::None => None,
            MonitorRom::Cuter => Some(RomImage {
                // PUSH AF / MOV A,B / OUT (data),A / POP AF / RET
                //
                // The push and pop are not caution, they are the contract. This
                // disk's own BIOS source states it: "ALL REGISTERS MUST BE SAVED
                // AND RESTORED BY YOUR VIDEO DRIVER IN ORDER TO BE COMPATIABLE
                // WITH CPM." A stub that clobbered `A` would work for a sign-on
                // and then corrupt whatever the CCP was holding.
                chunks: vec![(CUTER_CHAR_OUT, vec![0xF5, 0x78, 0xD3, data_port, 0xF1, 0xC9])],
            }),
        }
    }
}

/// Where CUTER's character-output entry lives.
///
/// `0C019h`, from TDISK05's own BIOS source: `OUTADDR EQU 0C019H` with the
/// comment "PUT OUTPUT ADDRESS HERE", and confirmed against the assembled
/// system tracks, which hold `CD 19 C0` and nothing else pointing into the ROM.
pub const CUTER_CHAR_OUT: u16 = 0xC019;

/// A console board: where its two registers live and how status is encoded.
///
/// The family comes from [`UartFamily`] rather than being restated here, so the
/// status-bit conventions have one owner and one set of tests. Note that a
/// family names a *convention*, not a part number — the board at `04h`/`05h` on
/// the VDM-1 Tarbell machines is not an Altair 88-SIO, but it reports readiness
/// the same active-low way, and pretending otherwise would mean a second copy of
/// the same four lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConsoleBoard {
    pub status_port: u8,
    pub data_port: u8,
    pub family: UartFamily,
    /// A monitor ROM the guest prints through instead of the data port.
    pub rom: MonitorRom,
}

/// One selectable machine: the config value, a description for the UIs, and the
/// console the guest will find.
///
/// Named for the *machine* and not for the console alone, even though today
/// every entry differs only in its console. That is deliberate and it is not
/// speculative generality: 256,256 bytes is already claimed by the Tarbell and
/// will be claimed by Cromemco and by z80pack's simulated controller, and
/// `Controller::accepts` hands an image to the first board that recognises the
/// length. Resolving that needs an operator's choice, it needs to live on this
/// same setting, and renaming a config key across telnet, web and desktop later
/// is precisely the duplicated-rule churn that has produced defects here three
/// times. One key now, with room in it.
pub struct MachineChoice {
    /// Canonical config value (stored in `egateway.conf`).
    pub key: &'static str,
    /// One-line description shown next to the selection in every UI.
    pub description: &'static str,
    pub console: ConsoleBoard,
}

const fn board(status_port: u8, data_port: u8, family: UartFamily) -> ConsoleBoard {
    ConsoleBoard { status_port, data_port, family, rom: MonitorRom::None }
}

/// Every selectable machine, in UI display order. Single source of truth for
/// config validation and all three configuration screens, the way
/// [`super::uart::UART_CHOICES`] serves the virtual-modem port and
/// [`super::boot::boot_choices`] serves the boot image.
pub const MACHINE_CHOICES: &[MachineChoice] = &[
    MachineChoice {
        key: "altair_2sio",
        description: "Altair 88-2SIO console - 0x10 / 0x11",
        console: board(0x10, 0x11, UartFamily::Acia),
    },
    MachineChoice {
        key: "altair_sio",
        description: "Altair 88-SIO console - 0x00 / 0x01",
        console: board(0x00, 0x01, UartFamily::Sio88),
    },
    MachineChoice {
        key: "console_04",
        // 38 columns, so it fits a 40-column PETSCII screen with the two-space
        // indent every menu line here carries.  Checked by the fit test below
        // rather than by eye, because a 42-column label got through last time.
        description: "Console at 0x04 / 0x05, ready-low",
        console: board(0x04, 0x05, UartFamily::Sio88),
    },
    MachineChoice {
        key: "console_04_cuter",
        description: "As 0x04 / 0x05, printing via CUTER ROM",
        console: ConsoleBoard { rom: MonitorRom::Cuter, ..board(0x04, 0x05, UartFamily::Sio88) },
    },
];

/// The default machine.
///
/// The 88-2SIO at `10h`/`11h` — which is not a preference, it is the machine
/// this path has always been. Every disk that boots today boots because its
/// console is there, so any other default would silence a working gateway on
/// upgrade. A config file with no such key must mean exactly what it meant
/// before the key existed.
pub const DEFAULT_MACHINE: &str = "altair_2sio";

/// Is `key` a recognised machine value?
pub fn is_valid_machine_key(key: &str) -> bool {
    MACHINE_CHOICES.iter().any(|c| c.key == key)
}

/// Resolve a config value to the console a guest will find. An unknown key
/// yields the default machine's console rather than no console at all: a
/// mistyped setting should leave the gateway working, not mute.
pub fn resolve_console(key: &str) -> ConsoleBoard {
    MACHINE_CHOICES
        .iter()
        .find(|c| c.key == key)
        .or_else(|| MACHINE_CHOICES.iter().find(|c| c.key == DEFAULT_MACHINE))
        .map(|c| c.console)
        .expect("the default machine is in the list")
}

/// The description for a config value, for a UI to show the current setting.
pub fn machine_description(key: &str) -> &'static str {
    MACHINE_CHOICES
        .iter()
        .find(|c| c.key == key)
        .map(|c| c.description)
        .unwrap_or(MACHINE_CHOICES[0].description)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default must be bit-for-bit the machine this path was before the
    /// setting existed, or an upgrade silences every disk that works today.
    #[test]
    fn test_the_default_is_todays_machine() {
        assert_eq!(DEFAULT_MACHINE, "altair_2sio");
        let c = resolve_console(DEFAULT_MACHINE);
        assert_eq!(c.status_port, 0x10);
        assert_eq!(c.data_port, 0x11);
        assert_eq!(c.family, UartFamily::Acia);
        assert_eq!(c.rom, MonitorRom::None, "the Altair console needs no ROM");
        assert_eq!(MACHINE_CHOICES[0].key, DEFAULT_MACHINE, "and it is offered first");
    }

    /// An unknown key must leave the gateway working rather than mute. A typo in
    /// a hand-edited config file is the likeliest way to reach this.
    #[test]
    fn test_an_unknown_machine_falls_back_to_the_default() {
        assert!(!is_valid_machine_key("bogus"));
        assert_eq!(resolve_console("bogus"), resolve_console(DEFAULT_MACHINE));
        assert_eq!(machine_description("bogus"), MACHINE_CHOICES[0].description);
    }

    /// The polarity that matters. TDISK04 was measured parked on `JNZ CONIN`
    /// because bit 0 set means *not* ready to it; an active-high reading would
    /// have claimed a keypress on every poll and fed it garbage.
    #[test]
    fn test_the_04_05_board_reports_ready_with_the_bit_clear() {
        let c = resolve_console("console_04");
        assert_eq!(c.status_port, 0x04);
        assert_eq!(c.data_port, 0x05);
        // A key waiting: bit 0 clear.  Nothing waiting: bit 0 set.
        assert_eq!(c.family.status(true, true, false) & 0x01, 0x00, "a key is waiting");
        assert_ne!(c.family.status(false, true, false) & 0x01, 0x00, "nothing waiting");
    }

    /// The CUTER stub must be real Z80 code at the real address, preserving
    /// every register, and it must print through *this* machine's data port
    /// rather than a hardcoded one.
    #[test]
    fn test_the_cuter_stub_is_code_that_preserves_registers() {
        let c = resolve_console("console_04_cuter");
        assert_eq!(c.rom, MonitorRom::Cuter);
        let image = c.rom.image(c.data_port).expect("CUTER carries a ROM");
        assert_eq!(image.chunks.len(), 1, "one entry point, as the disk demands");
        let (at, bytes) = &image.chunks[0];
        assert_eq!(*at, 0xC019, "OUTADDR, from the disk's own BIOS source");
        assert_eq!(bytes, &vec![0xF5, 0x78, 0xD3, 0x05, 0xF1, 0xC9]);
        assert_eq!(bytes[0], 0xF5, "PUSH AF - its own source requires this");
        assert_eq!(bytes[4], 0xF1, "POP AF");
        assert_eq!(bytes[3], c.data_port, "prints through this machine's port");
        assert_eq!(*bytes.last().unwrap(), 0xC9, "and returns to the caller");
    }

    /// A machine with no ROM must place nothing at all — an empty image and a
    /// six-byte one are very different things to a guest whose memory reaches
    /// that high.
    #[test]
    fn test_a_port_console_places_no_rom() {
        for key in ["altair_2sio", "altair_sio", "console_04"] {
            let c = resolve_console(key);
            assert_eq!(c.rom.image(c.data_port), None, "{key} must place no bytes");
        }
    }

    /// Keys are unique, descriptions exist, and every description fits a
    /// 40-column PETSCII screen once a menu's two-space indent is allowed for.
    /// Measured rather than eyeballed: a 42-column label got past review once.
    #[test]
    fn test_choices_are_unique_and_fit_forty_columns() {
        let mut keys = std::collections::HashSet::new();
        for c in MACHINE_CHOICES {
            assert!(keys.insert(c.key), "duplicate key {}", c.key);
            assert!(!c.description.is_empty());
            assert!(
                c.description.len() <= 38,
                "{:?} is {} columns; 38 is the budget at 40 minus a two-space indent",
                c.description,
                c.description.len()
            );
            assert!(c.key.len() <= 20, "{:?} is a long config value", c.key);
        }
    }

    /// No machine may put its console on a port the disk controllers own, or the
    /// guest's console and its disk would answer each other's reads. Checked
    /// against the real boards rather than a written-down range.
    #[test]
    fn test_no_console_lands_on_a_controller_port() {
        let m = super::super::boot_machine::BootMachine::new();
        for c in MACHINE_CHOICES {
            for port in [c.console.status_port, c.console.data_port] {
                assert!(
                    !m.owns_disk_port(port),
                    "{}: port {port:#04x} belongs to a disk controller",
                    c.key
                );
            }
        }
    }
}
