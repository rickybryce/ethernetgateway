//! CP/M 2.2 emulator core — a real Z80 CPU (the BSD-licensed `iz80` crate)
//! driven by our own CP/M 2.2 BDOS/BIOS, sandboxed to a `CPM/` directory
//! under `transfer_dir`.
//!
//! This is a **completely separate** feature from the Gateway Shell
//! (`telnet/kernel.rs`), which is a pure-Rust CP/M-*styled* file manager
//! with no CPU emulation.  The design is documented here and in
//! `telnet/cpm_emu.rs`; the emulator was built in stages (scaffold → CPU and
//! console → CCP-lite → filesystem → running `.COM` → hardening), and the
//! CHANGELOG records what each one delivered.
//!
//! ## Design of the run loop (B1)
//! CP/M software reaches the operating system with `CALL 5` (the BDOS
//! entry) and reboots with `JP 0` / a `RET` to the warm-boot vector.  We
//! intercept both by watching the program counter: the pure, synchronous
//! [`Cpm::run`] steps the CPU until it either reaches the BDOS entry
//! (returning [`Stop::Bdos`] with the function number so the *host*
//! services the call — file I/O jailed, console I/O over the session), or
//! warm-boots, or exhausts its instruction budget, or sees the external
//! abort flag.  Keeping the CPU stepping synchronous and returning to an
//! async driver for I/O cleanly separates the two worlds and makes the
//! whole core unit-testable with no live session.
//!
//! ## Runaway `.COM` escape
//! Two independent guarantees, per the plan's hard requirement:
//! - the **abort flag** (an `AtomicBool` the async driver can set from an
//!   out-of-band `ESC ESC` wire-reader) is checked every instruction, and
//! - the **instruction budget** bounds each [`Cpm::run`] batch, so the
//!   driver regains control to check the flag / yield even if the guest
//!   never performs console I/O (an infinite `JP $` loop).

mod fcb;
mod fs;
pub mod hbios;
pub mod hdsk;
pub mod boot;
pub mod boot_machine;
pub mod console;
pub mod controller;
pub mod cpu;
pub mod dcdd;
pub mod detect;
pub mod image;
pub mod layout;
mod machine;
pub mod modem_port;
pub mod printer;
pub mod tarbell;
pub mod uart;
pub mod wd1771;
pub mod cromemco;
pub mod z80pack;

pub use fcb::{parse_afn, parse_command_fcb, parse_dir_operand, split_8_3, Fcb, FCB_SIZE};
pub use fs::{CpmFs, DEFAULT_DMA, NUM_DRIVES};
pub use machine::CpmMachine;
pub use uart::{resolve_access, ModemAccess};

use iz80::{Cpu, Machine, Reg8, Reg16};
use std::sync::atomic::{AtomicBool, Ordering};

/// BDOS entry point — programs `CALL 5`.
pub const BDOS_ENTRY: u16 = 0x0005;
/// Warm-boot vector — programs `JP 0` (or `RET` to it) to reboot.
pub const WBOOT: u16 = 0x0000;
/// IOBYTE — the CP/M logical-device assignment byte, at its architectural
/// page-zero home.  BDOS 7/8 (get/set I/O byte) read and write it here.
const IOBYTE_ADDR: u16 = 0x0003;
/// CDISK — the current-drive / current-user byte at its architectural
/// page-zero home: low nibble = current drive (0 = A:), high nibble =
/// current user (0–15).  Real CP/M's CCP/BDOS maintains this so a
/// transient can read the drive it was loaded from directly from page
/// zero (Infocom's CP/M interpreter does exactly this to locate its
/// story file — without it, a game run from B: looks on A:, fails to
/// open its data, and hangs).
const CDISK_ADDR: u16 = 0x0004;
/// Transient Program Area base — where a `.COM` is loaded and starts.
/// The host clock as RomWBW's six-byte date/time buffer: year, month, day,
/// hour, minute, second — **each BCD encoded**, per the published HBIOS
/// interface (`RomWBW/Source/Doc/SystemGuide.md`, "Each byte is BCD encoded").
///
/// CP/M 2.2 has no clock of its own, so this is the only time an emulated
/// program can get, and it is read-only: [`hbios`] refuses RTCSETTIM rather
/// than pretend a guest can set the host's clock.
///
/// **Local time where the platform can tell us** (Unix, via `localtime_r`),
/// because an RTC in a CP/M machine shows wall-clock time and a user comparing
/// it against the clock on the wall is the whole point. Elsewhere — Windows,
/// which needs an API we do not otherwise link — it falls back to UTC rather
/// than adding a dependency for one call; the difference is documented, and
/// the gateway's own deployments are Unix.
pub fn host_clock_bcd() -> [u8; 6] {
    let (y, m, d, h, mi, s) = host_clock_parts();
    clock_bcd_from_parts(y, m, d, h, mi, s)
}

/// The host wall clock as plain numbers: `(year, month, day, hour, min, sec)`.
///
/// Extracted from [`host_clock_bcd`] rather than written twice, because the
/// printer names its spool files from the same clock and "local time where the
/// platform can tell us, UTC elsewhere" is a rule this project would otherwise
/// be stating in two places — the shape of defect it has produced more than
/// once. The year is the full four digits here; only the BCD packing truncates
/// it to two.
pub fn host_clock_parts() -> (i64, i64, i64, i64, i64, i64) {
    #[cfg(unix)]
    {
        // SAFETY: `time` with a null argument returns the value rather than
        // storing it, and `localtime_r` writes into a `tm` we own — the
        // reentrant form precisely so no shared buffer is involved.
        let secs = unsafe { libc::time(std::ptr::null_mut()) };
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        if !unsafe { libc::localtime_r(&secs, &mut tm) }.is_null() {
            return (
                tm.tm_year as i64 + 1900,
                tm.tm_mon as i64 + 1,
                tm.tm_mday as i64,
                tm.tm_hour as i64,
                tm.tm_min as i64,
                tm.tm_sec as i64,
            );
        }
    }
    // UTC fallback: the epoch second split into a civil date and a time.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    (y, m, d, rem / 3600, (rem % 3600) / 60, rem % 60)
}

/// Pack a date and time into the six BCD bytes the buffer wants.
///
/// Every field is clamped to what its BCD byte can hold: a leap second arrives
/// as `tm_sec == 60`, and the year is the last two digits, which is all the
/// format has room for (a 2100 machine reads as `00`, exactly as period
/// hardware did).
fn clock_bcd_from_parts(year: i64, mon: i64, day: i64, hour: i64, min: i64, sec: i64) -> [u8; 6] {
    [
        to_bcd(year.rem_euclid(100) as u8),
        to_bcd(mon.clamp(1, 12) as u8),
        to_bcd(day.clamp(1, 31) as u8),
        to_bcd(hour.clamp(0, 23) as u8),
        to_bcd(min.clamp(0, 59) as u8),
        to_bcd(sec.clamp(0, 59) as u8),
    ]
}

/// One byte of BCD: 42 -> 0x42.  Values above 99 cannot be represented, and
/// every caller clamps before reaching here.
fn to_bcd(n: u8) -> u8 {
    ((n / 10) << 4) | (n % 10)
}

/// Civil date from a day count since 1970-01-01, by Howard Hinnant's
/// `civil_from_days`.  Used only on the non-Unix fallback path, where there is
/// no `localtime_r` to do it for us.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub const TPA_BASE: u16 = 0x0100;
/// Top of the usable TPA in our layout; the stack starts here and grows
/// down, leaving the region above for the (pretend, for now) BDOS/BIOS.
const STACK_TOP: u16 = 0xFE00;
/// Top of the usable TPA, exposed so the shell can report the same figure a
/// real CP/M system prints (`STACK_TOP` itself stays private — callers have
/// no business knowing where our stack lives, only how big the TPA is).
pub const TPA_TOP: u16 = STACK_TOP;
/// Size of the usable TPA in bytes — the largest `.COM` [`Cpm::load_com`]
/// will accept without truncation.
pub const TPA_BYTES: u16 = TPA_TOP - TPA_BASE;

/// BIOS jump-table base (the `BOOT` entry).  Real CP/M software that does
/// direct console I/O — MBASIC, WordStar, Turbo Pascal, Infocom games —
/// finds the console routines by reading the warm-boot pointer at 0x0001
/// (which points at this table's `WBOOT` entry) and walking the `JP`
/// vectors.  We lay a real 17-entry table here whose `JP` operands are
/// unique trap addresses [`BIOS_TRAP`]`+i`; `run` recognises a PC in that
/// range as a BIOS call and returns [`Stop::Bios`] so the host can service
/// it, exactly as it does for the BDOS entry.  The table sits above the
/// TPA top (0x0001/0x0006 report `STACK_TOP`) so a guest never overwrites
/// it.  Kept clear of the DPB/alloc scratch at 0xFE80/0xFE90.
const BIOS_BASE: u16 = 0xFF00;
/// Per-vector trap addresses the BIOS jump table's `JP`s point at.  A guest
/// either jumps through the table (`CALL BIOS_BASE+3*i` → `JP BIOS_TRAP+i`)
/// or, like MBASIC, extracts the operand and calls `BIOS_TRAP+i` directly;
/// both land on the trap PC.
const BIOS_TRAP: u16 = 0xFF40;
/// CP/M 2.2 BIOS jump-table length: BOOT, WBOOT, CONST, CONIN, CONOUT,
/// LIST, PUNCH, READER, HOME, SELDSK, SETTRK, SETSEC, SETDMA, READ, WRITE,
/// LISTST, SECTRAN.
const BIOS_VECTORS: u16 = 17;

/// Page-zero `RST 8` vector — where a RomWBW system's HBIOS entry lives.  On
/// real hardware HBIOS is banked out of the CPU's reach, so RomWBW keeps a
/// small proxy in the top pages of RAM and points this vector at it; software
/// makes an HBIOS call with `RST 8` (function in `B`, unit in `C`).  We install
/// the same vector shape — `JP` to a trap address the host services — and only
/// when an HBIOS access mode is selected, so a plain CP/M guest sees the
/// untouched page zero it would on a non-RomWBW machine.  See [`hbios`].
const HBIOS_VECTOR: u16 = 0x0008;
/// Trap address the `RST 8` vector jumps to, recognised by [`Cpm::run`].  It
/// sits at the same place a real system's proxy entry does, above every
/// structure the emulator keeps in high memory (alloc vector, BIOS table, DPB)
/// and above the guest's stack top.
const HBIOS_TRAP: u16 = 0xFFF0;

/// Why a [`Cpm::run`] batch returned control to the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// The guest reached the BDOS entry with this function number in `C`.
    /// The host services it (reading further arguments from the registers
    /// / memory) and then calls [`Cpm::bdos_return`].
    Bdos(u8),
    /// The guest called a BIOS jump-table vector directly (this is the
    /// vector index: 1=WBOOT, 2=CONST, 3=CONIN, 4=CONOUT, 5=LIST, …).  The
    /// host services the console group against the live session and calls
    /// [`Cpm::bios_return`]; used by software that bypasses BDOS for
    /// console I/O (MBASIC, WordStar, Infocom, …).
    Bios(u8),
    /// The guest made a RomWBW HBIOS call (`RST 8`); this is the function
    /// number from `B`, with the unit in `C`.  Serviced by [`hbios`] against
    /// the virtual modem when an `hbios_*` access mode is selected.  The host
    /// may leave the call *unanswered* (simply run again without calling
    /// [`Cpm::hbios_return`]) to park the guest on a blocking call until the
    /// device is ready — the PC stays on the trap, so the next batch re-reports
    /// it.
    Hbios(u8),
    /// System reset / warm boot (BDOS function 0, `JP 0`, or `RET` to the
    /// warm-boot vector).  The run is over.
    WarmBoot,
    /// The instruction budget for this batch was reached without hitting
    /// the BDOS entry or a warm boot — the driver should check the abort
    /// flag, yield, and (for a legitimate long-running program) run again.
    BudgetExhausted,
    /// The external abort flag was set (ESC ESC break-out).
    Aborted,
}

/// The emulated CP/M machine: a CPU — a Z80, or the 8080 `cpm_cpu` can name
/// instead — plus its 64 KB address space.
pub struct Cpm {
    cpu: Cpu,
    mem: CpmMachine,
    /// Total instructions executed since the last load — used both for the
    /// warm-boot gate (ignore the initial `PC == 0`) and diagnostics.
    instructions: u64,
    /// Line characteristics the guest last set through the HBIOS `INITDEV`
    /// call, reported back verbatim by `QUERY`.  There is no real UART to
    /// program, so this is remembered rather than acted on.
    hbios_line: u16,
}

impl Default for Cpm {
    fn default() -> Self {
        Self::new()
    }
}

impl Cpm {
    /// A fresh machine on the default processor, with the CP/M low-memory
    /// vectors installed.
    ///
    /// The default rather than the configured CPU, so the tests below — and
    /// anything else with no operator to ask — get one known machine.  The
    /// session driver uses [`Cpm::new_for`].
    pub fn new() -> Cpm {
        Self::new_for(cpu::DEFAULT_CPU)
    }

    /// A fresh machine on the processor `cpm_cpu` names.
    ///
    /// See [`cpu`] for what the choice costs: the 8080 is the more literal
    /// Altair, and it cannot run EGT80.
    pub fn new_for(cpu_setting: &str) -> Cpm {
        let mut cpm = Cpm {
            cpu: cpu::new_cpu(cpu_setting),
            mem: CpmMachine::new(),
            instructions: 0,
            hbios_line: hbios::default_line(),
        };
        cpm.install_low_memory();
        cpm
    }

    /// Install the CP/M low-memory vectors: the warm-boot vector at 0x0000
    /// and the BDOS entry at 0x0005 as real CP/M lays them out (`JP <addr>`),
    /// so a guest that inspects address 6 to find the top of the TPA sees a
    /// sane value.  We intercept both by program counter, so the jump targets
    /// themselves are never run.  Re-run on every `load_com` so a program
    /// that trashed page zero can't corrupt the next program's vectors —
    /// mirrors real CP/M reloading the system on a warm boot.
    fn install_low_memory(&mut self) {
        // Warm-boot vector at 0x0000 points at the BIOS `WBOOT` entry (the
        // 2nd table slot), as real CP/M lays it out, so a guest that reads
        // 0x0001 to find the BIOS jump table walks a real table.
        self.mem.poke(0x0000, 0xC3); // JP WBOOT (BIOS entry+3)
        self.mem.poke16(0x0001, BIOS_BASE + 3);
        self.mem.poke(0x0005, 0xC3); // JP BDOS
        self.mem.poke16(0x0006, STACK_TOP);
        self.install_bios_table();
        self.install_hbios_vector();
    }

    /// Install (or clear) the `RST 8` HBIOS vector to match the selected
    /// access mode.  Present only for an `hbios_*` mode: RomWBW software finds
    /// its API where RomWBW puts it, and everything else finds the page zero of
    /// a plain CP/M 2.2 machine — so a program probing for RomWBW on a
    /// port-I/O profile isn't told a system it can't have is present.
    fn install_hbios_vector(&mut self) {
        if self.mem.hbios_unit().is_some() {
            self.mem.poke(HBIOS_VECTOR, 0xC3); // JP <proxy entry>
            self.mem.poke16(HBIOS_VECTOR + 1, HBIOS_TRAP);
        } else {
            self.mem.poke(HBIOS_VECTOR, 0x00);
            self.mem.poke16(HBIOS_VECTOR + 1, 0);
        }
    }

    /// Lay a real 17-entry CP/M 2.2 BIOS jump table at [`BIOS_BASE`].  Each
    /// slot is a `JP BIOS_TRAP+i`; `run` traps a PC in the `BIOS_TRAP`
    /// range and returns [`Stop::Bios`]`(i)`.  A guest reaches a vector
    /// either by `CALL`ing the table slot (the `JP` lands on the trap) or,
    /// like MBASIC, by extracting the `JP` operand and calling the trap
    /// address directly — both work because the operand *is* the trap PC.
    fn install_bios_table(&mut self) {
        for i in 0..BIOS_VECTORS {
            let slot = BIOS_BASE + 3 * i;
            self.mem.poke(slot, 0xC3); // JP
            self.mem.poke16(slot + 1, BIOS_TRAP + i);
        }
    }

    /// Load a `.COM` image into the TPA and prepare to run it: the stack is
    /// placed just below the reserved system area with the warm-boot
    /// address pushed, so a program that ends in `RET` reboots cleanly, and
    /// the PC is set to the TPA base.  Bytes past the usable TPA are
    /// silently dropped (a `.COM` never legitimately exceeds it).
    pub fn load_com(&mut self, program: &[u8]) {
        self.install_low_memory();
        let max = TPA_BYTES as usize;
        for (i, b) in program.iter().take(max).enumerate() {
            self.mem.poke(TPA_BASE + i as u16, *b);
        }
        let sp = STACK_TOP.wrapping_sub(2);
        self.mem.poke16(sp, WBOOT); // RET here => warm boot
        self.cpu.registers().set16(Reg16::SP, sp);
        self.cpu.registers().set_pc(TPA_BASE);
        self.instructions = 0;
    }

    /// Step the CPU until a BDOS call, warm boot, the `budget` instruction
    /// count, or the `abort` flag — whichever comes first.  Pure and
    /// synchronous; see the module docs for how the async driver uses it.
    pub fn run(&mut self, budget: u64, abort: &AtomicBool) -> Stop {
        let mut executed = 0u64;
        while executed < budget {
            if abort.load(Ordering::Relaxed) {
                return Stop::Aborted;
            }
            let pc = self.cpu.registers().pc();
            // Trap a BDOS call whether the guest `CALL`s the 0x0005 entry
            // (the JP there points at STACK_TOP) or reads the entry address
            // from the 0x0006 word and calls STACK_TOP directly — both are a
            // BDOS call; only 0x0005 was trapped before, so a 0x0006-pointer
            // call ran off into uninitialised RAM.
            if pc == BDOS_ENTRY || pc == STACK_TOP {
                let func = self.cpu.registers().get8(Reg8::C);
                if func == 0 {
                    return Stop::WarmBoot; // BDOS 0 = system reset
                }
                return Stop::Bdos(func);
            }
            if pc == WBOOT && self.instructions > 0 {
                return Stop::WarmBoot;
            }
            // A direct BIOS jump-table call: PC landed on one of the trap
            // addresses the table's `JP`s point at.  Vector index = offset.
            if (BIOS_TRAP..BIOS_TRAP + BIOS_VECTORS).contains(&pc) {
                return Stop::Bios((pc - BIOS_TRAP) as u8);
            }
            // An HBIOS call: `RST 8` pushed the return address and the page-zero
            // vector jumped here.  The function is in `B` (the unit in `C`).
            // Nothing is popped yet — a host that parks a blocking call just
            // runs again and lands right back here.
            if pc == HBIOS_TRAP {
                return Stop::Hbios(self.cpu.registers().get8(Reg8::B));
            }
            self.cpu.execute_instruction(&mut self.mem);
            self.instructions += 1;
            executed += 1;
        }
        Stop::BudgetExhausted
    }

    /// Return from a serviced BDOS call: CP/M passes a byte result in `A`
    /// (mirrored in `L`, with `B`/`H` cleared, the lrzsz/CP/M convention),
    /// then the call `RET`s to the address the guest's `CALL 5` pushed.
    pub fn bdos_return(&mut self, value: u8) {
        self.cpu.registers().set8(Reg8::A, value);
        self.cpu.registers().set8(Reg8::L, value);
        self.cpu.registers().set8(Reg8::B, 0);
        self.cpu.registers().set8(Reg8::H, 0);
        let sp = self.cpu.registers().get16(Reg16::SP);
        let ret = self.mem.peek16(sp);
        self.cpu.registers().set16(Reg16::SP, sp.wrapping_add(2));
        self.cpu.registers().set_pc(ret);
    }

    /// Return from a BDOS call that yields an address in `HL` (the "Get
    /// Addr(...)" group — e.g. Get DPB / Get Alloc).  Sets `HL` to the address
    /// (with `A`/`B` mirroring `L`/`H`, the CP/M register convention) and
    /// `RET`s, unlike [`Cpm::bdos_return`] which forces `H=0` for a byte code.
    pub fn bdos_return_hl(&mut self, hl: u16) {
        let lo = (hl & 0xFF) as u8;
        let hi = (hl >> 8) as u8;
        self.cpu.registers().set8(Reg8::A, lo);
        self.cpu.registers().set8(Reg8::L, lo);
        self.cpu.registers().set8(Reg8::B, hi);
        self.cpu.registers().set8(Reg8::H, hi);
        let sp = self.cpu.registers().get16(Reg16::SP);
        let ret = self.mem.peek16(sp);
        self.cpu.registers().set16(Reg16::SP, sp.wrapping_add(2));
        self.cpu.registers().set_pc(ret);
    }

    /// Return from a serviced BIOS console call: CP/M BIOS routines pass
    /// their byte result in `A` (CONST status, CONIN/READER character); we
    /// mirror it in `L` for the occasional caller that reads the low word,
    /// then `RET` to the address the guest's `CALL` pushed.  Output vectors
    /// (CONOUT/LIST/PUNCH) that return nothing just pass 0.
    pub fn bios_return(&mut self, value: u8) {
        self.cpu.registers().set8(Reg8::A, value);
        self.cpu.registers().set8(Reg8::L, value);
        let sp = self.cpu.registers().get16(Reg16::SP);
        let ret = self.mem.peek16(sp);
        self.cpu.registers().set16(Reg16::SP, sp.wrapping_add(2));
        self.cpu.registers().set_pc(ret);
    }

    /// Finish an HBIOS call: the result code in `A` (0 = success, non-zero =
    /// error, per the API's convention) and `RET` to where `RST 8` came from.
    /// Scramble `HL`, which an HBIOS call does not promise to preserve.
    ///
    /// Real RomWBW returns values in `HL` for several functions and uses it
    /// freely inside the others: software must not hold a pointer there across
    /// an `RST 8`.  Ours used to leave it untouched, which is *permissive* — and
    /// permissiveness here hid a total data-corruption bug in EGT80's own
    /// transfers that only appeared on real hardware (the XMODEM loops walked
    /// the buffer with `HL` across the port driver, so on a real machine every
    /// block "passed" its CRC while the file filled with whatever `HL` had
    /// wandered onto).  An emulator that is looser than the hardware turns a
    /// reproducible bug into a field report, so this one is not.
    ///
    /// Only the functions whose documented returns do *not* include `H` or `L`
    /// call this — `CIOQUERY` and `CIODEVICE` return in `L`, so they must not.
    pub fn hbios_scramble_hl(&mut self) {
        self.cpu.registers().set16(Reg16::HL, 0xFFFF);
    }

    pub fn hbios_return(&mut self, result: u8) {
        self.cpu.registers().set8(Reg8::A, result);
        // Flags must reflect the result, not whatever the guest had before the
        // `RST 8`.  A real HBIOS call returns through code that ends on a
        // flag-setting instruction, and callers rely on it: QTERM's RomWBW
        // overlay returns the status straight to a `JR Z` in QTERM's core, so
        // stale flags read as "device not ready" and the program waits for a
        // transmitter that is already free.  These are the flags `OR A` leaves
        // (S/Z/P from the value, H/N/C cleared, plus the two undocumented bits
        // a real Z80 copies from the value).
        let f = (result & 0x80)                                  // S
            | (if result == 0 { 0x40 } else { 0 })               // Z
            | (result & 0x28)                                    // undocumented 5/3
            | (if result.count_ones().is_multiple_of(2) { 0x04 } else { 0 }); // P
        self.cpu.registers().set8(Reg8::F, f);
        self.hbios_ret();
    }

    /// Finish an HBIOS call that yields a byte in `E` as well as the result in
    /// `A` — the input / status group (character read, pending count, free
    /// count).
    pub fn hbios_return_e(&mut self, result: u8, e: u8) {
        self.cpu.registers().set8(Reg8::E, e);
        self.hbios_return(result);
    }

    /// Finish an HBIOS call that reports line characteristics in `DE` and a
    /// terminal type in `L` (the device-configuration query).
    pub fn hbios_return_de_l(&mut self, result: u8, de: u16, l: u8) {
        self.cpu.registers().set16(Reg16::DE, de);
        self.cpu.registers().set8(Reg8::L, l);
        self.hbios_return(result);
    }

    /// Finish an HBIOS device-description call: device type in `D`, device
    /// number in `E`, attributes in `C`, mode in `H`, base I/O address in `L`.
    pub fn hbios_return_device(&mut self, result: u8, d: u8, e: u8, c: u8, h: u8, l: u8) {
        self.cpu.registers().set8(Reg8::D, d);
        self.cpu.registers().set8(Reg8::E, e);
        self.cpu.registers().set8(Reg8::C, c);
        self.cpu.registers().set8(Reg8::H, h);
        self.cpu.registers().set8(Reg8::L, l);
        self.hbios_return(result);
    }

    /// Pop the `RST 8` return address and resume the guest there.
    fn hbios_ret(&mut self) {
        let sp = self.cpu.registers().get16(Reg16::SP);
        let ret = self.mem.peek16(sp);
        self.cpu.registers().set16(Reg16::SP, sp.wrapping_add(2));
        self.cpu.registers().set_pc(ret);
    }

    /// HBIOS sub-function argument: the byte in `C` (the unit for a serial
    /// call, the sub-function for the `GET`/`SET` groups).
    pub fn arg_hbios_unit(&mut self) -> u8 {
        self.reg8(Reg8::C)
    }

    /// Bytes waiting for the guest on the virtual modem.
    pub fn modem_rx_len(&self) -> usize {
        self.mem.modem_rx_len()
    }

    /// Consecutive reads of a port no device answers.
    ///
    /// See [`CpmMachine::unclaimed_reads`] for what this is protecting against
    /// — a guest polling hardware that is not there, at the speed of the host.
    pub fn unclaimed_reads(&self) -> u32 {
        self.mem.unclaimed_reads()
    }

    /// Forget the unclaimed-read count, having paced the guest for it.
    pub fn clear_unclaimed_reads(&mut self) {
        self.mem.clear_unclaimed_reads();
    }

    /// Room left in the guest's outbound modem ring.
    pub fn modem_tx_free(&self) -> usize {
        self.mem.modem_tx_free()
    }

    /// The HBIOS serial unit the virtual modem answers as, if any.
    pub fn hbios_unit(&self) -> Option<u8> {
        self.mem.hbios_unit()
    }

    /// Line characteristics the guest last set through HBIOS `INITDEV`.
    pub fn hbios_line(&self) -> u16 {
        self.hbios_line
    }

    /// Remember the line characteristics an HBIOS `INITDEV` requested, so
    /// `QUERY` reports back what was set.
    pub fn set_hbios_line(&mut self, line: u16) {
        self.hbios_line = line;
    }

    /// BIOS "console/list/punch output" argument: the character in `C`.
    pub fn arg_c(&mut self) -> u8 {
        self.reg8(Reg8::C)
    }

    /// Read an 8-bit register (for the host to fetch BDOS arguments).
    pub fn reg8(&mut self, r: Reg8) -> u8 {
        self.cpu.registers().get8(r)
    }

    /// Set an 8-bit register (test-only: stage BDOS arguments for a direct
    /// `service_disk_bdos` call without assembling a program).
    #[cfg(test)]
    pub fn set_reg8(&mut self, r: Reg8, v: u8) {
        self.cpu.registers().set8(r, v);
    }

    /// Set a 16-bit register (test-only, as [`Cpm::set_reg8`]).
    #[cfg(test)]
    pub fn set_reg16(&mut self, rr: Reg16, v: u16) {
        self.cpu.registers().set16(rr, v);
    }

    /// Read a 16-bit register (e.g. `DE` for BDOS 9's string pointer).
    pub fn reg16(&mut self, rr: Reg16) -> u16 {
        self.cpu.registers().get16(rr)
    }

    /// Where the guest is about to execute.
    ///
    /// Test-only, for diagnosing a program that stops without saying why. A CPU
    /// conformance suite is the case that needed it: its failure action is a
    /// silent `JMP 0000`, so the only thing distinguishing "passed quietly" from
    /// "failed at test 14" is the address it jumped from.
    #[cfg(test)]
    pub fn pc(&mut self) -> u16 {
        self.cpu.registers().pc()
    }

    /// BDOS "console output" (function 2) argument: the character in `E`.
    /// A convenience wrapper so callers needn't import `iz80` register
    /// enums just to service the common console calls.
    pub fn arg_e(&mut self) -> u8 {
        self.reg8(Reg8::E)
    }

    /// BDOS "print string" (function 9) argument: the string pointer in
    /// `DE`.
    pub fn arg_de(&mut self) -> u16 {
        self.reg16(Reg16::DE)
    }

    /// Read `len` bytes of guest memory starting at `address` (wrapping the
    /// 16-bit address space), e.g. a 36-byte FCB or a 128-byte DMA record.
    pub fn read_block(&mut self, address: u16, len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        let mut a = address;
        for _ in 0..len {
            out.push(self.mem.peek(a));
            a = a.wrapping_add(1);
        }
        out
    }

    /// Write a block of bytes to guest memory starting at `address`
    /// (wrapping the 16-bit address space).
    pub fn write_block(&mut self, address: u16, data: &[u8]) {
        let mut a = address;
        for &b in data {
            self.mem.poke(a, b);
            a = a.wrapping_add(1);
        }
    }

    /// Collect a `$`-terminated BDOS "print string" (function 9) starting
    /// at `addr`, bounded by `limit` bytes so a missing terminator can't
    /// run away across the whole address space.  The `$` is not included.
    pub fn read_dollar_string(&mut self, addr: u16, limit: usize) -> Vec<u8> {
        let mut out = Vec::new();
        let mut p = addr;
        for _ in 0..limit {
            let b = self.mem.peek(p);
            if b == b'$' {
                break;
            }
            out.push(b);
            p = p.wrapping_add(1);
        }
        out
    }

    /// Service BDOS "read console buffer" (function 10): write an input
    /// line into the buffer at `de` using CP/M's layout — byte 0 is the
    /// caller-set maximum, byte 1 the count we filled in, and the
    /// characters follow.  The line is truncated to the maximum so a long
    /// paste can never overrun the guest's buffer.
    /// The caller-set maximum length (byte 0) of a BDOS-10 read-console
    /// buffer at `de` — so the driver can cap interactive input as it reads.
    pub fn read_buffer_max(&mut self, de: u16) -> usize {
        self.mem.peek(de) as usize
    }

    pub fn bdos_read_buffer(&mut self, de: u16, line: &[u8]) {
        let max = self.mem.peek(de) as usize;
        let n = line.len().min(max);
        self.mem.poke(de.wrapping_add(1), n as u8);
        for (i, b) in line.iter().take(n).enumerate() {
            self.mem.poke(de.wrapping_add(2).wrapping_add(i as u16), *b);
        }
    }

    /// Build page zero for a transient-program launch exactly as the CP/M
    /// CCP does before it jumps to the TPA: the command tail (the arguments
    /// after the program name) is uppercased and stored at 0x0080 as a
    /// length-prefixed, NUL-terminated string, and the first two tail tokens
    /// are parsed into the two default FCBs at 0x005C and 0x006C.
    ///
    /// Notes matching real CP/M behavior:
    /// - The tail carries its leading delimiter space and the length counts
    ///   it (`PIP A:=B:X` ⇒ tail ` A:=B:X`, length 8).
    /// - The 0x0080 region *is* the default 128-byte DMA buffer, so the
    ///   first disk read a program performs overwrites the tail — programs
    ///   that need their arguments copy them out first (as they always did).
    /// - The two default FCBs overlap (0x006C lies inside the 0x005C FCB);
    ///   FCB1 gets its extent/record fields zeroed, FCB2 is only the 12-byte
    ///   drive+name+ext the CCP lays down there.
    ///
    /// Call after [`Cpm::load_com`], before [`Cpm::run`].
    pub fn setup_command_line(&mut self, tail: &str) {
        let up = tail.trim().to_ascii_uppercase();

        // Command tail at 0x0080: a leading space when non-empty, capped so
        // the length byte + text + NUL terminator all fit the 128-byte page.
        let body = if up.is_empty() {
            String::new()
        } else {
            format!(" {up}")
        };
        let bytes = body.as_bytes();
        let n = bytes.len().min(126);
        self.mem.poke(0x0080, n as u8);
        for (i, &b) in bytes.iter().take(n).enumerate() {
            self.mem.poke(0x0081 + i as u16, b);
        }
        self.mem.poke(0x0081 + n as u16, 0x00);

        // Default FCBs parsed from the first two whitespace tokens.
        let mut toks = up.split_whitespace();
        let (d1, n1, e1) = parse_command_fcb(toks.next().unwrap_or(""));
        let (d2, n2, e2) = parse_command_fcb(toks.next().unwrap_or(""));
        self.write_default_fcb(0x005C, d1, &n1, &e1, true);
        self.write_default_fcb(0x006C, d2, &n2, &e2, false);
    }

    /// Lay a parsed default FCB (drive byte + 8.3 name/ext) into guest memory
    /// at `at`.  For the primary FCB (`zero_fields`) the extent/record fields
    /// (ex,s1,s2,rc and cr,r0..r2) are cleared so the program starts at
    /// record 0; the secondary FCB is just the 12-byte name the CCP writes.
    fn write_default_fcb(&mut self, at: u16, drive: u8, name: &[u8; 8], ext: &[u8; 3], zero_fields: bool) {
        self.mem.poke(at, drive);
        for (i, &b) in name.iter().enumerate() {
            self.mem.poke(at + 1 + i as u16, b);
        }
        for (i, &b) in ext.iter().enumerate() {
            self.mem.poke(at + 9 + i as u16, b);
        }
        if zero_fields {
            for off in 12u16..16 {
                self.mem.poke(at + off, 0); // ex, s1, s2, rc
            }
            for off in 32u16..36 {
                self.mem.poke(at + off, 0); // cr, r0, r1, r2
            }
        }
    }

    /// Write the CP/M CDISK byte at page-zero 0x0004 from a 0-based drive
    /// (0 = A:) and user number (0–15): low nibble = drive, high nibble =
    /// user.  Real CP/M maintains this so a transient can read its login
    /// drive directly from page zero.
    pub fn set_current_disk(&mut self, drive: u8, user: u8) {
        self.mem.poke(CDISK_ADDR, ((user & 0x0F) << 4) | (drive & 0x0F));
    }

    /// Select how the guest reaches the virtual modem (direct UART ports, the
    /// BDOS `AUX:` device, or off).  For `Ports`, a comms program's `IN`/`OUT`
    /// at the profile's addresses reach the modem's byte rings.
    pub fn set_modem_access(&mut self, access: ModemAccess) {
        self.mem.set_access(access);
        // The `RST 8` vector's presence follows the access mode, so selecting
        // (or clearing) an HBIOS mode after construction is honoured.
        self.install_hbios_vector();
    }

    /// Drain the bytes the guest has written toward the peer (the modem TX
    /// ring), for the async driver to forward to the connection.
    pub fn modem_drain_tx(&mut self) -> Vec<u8> {
        self.mem.modem_drain_tx()
    }

    /// Queue bytes received from the peer for the guest to read (the modem RX
    /// ring), serviced by the driver's pump.
    pub fn modem_queue_rx(&mut self, data: &[u8]) {
        self.mem.modem_queue_rx(data);
    }

    /// Reflect the modem's carrier (DCD) state into the UART status register.
    pub fn set_carrier(&mut self, carrier: bool) {
        self.mem.set_carrier(carrier);
    }

    /// Free space in the modem RX ring, so the driver can cap how much it
    /// reads from the peer (backpressure for a slow guest).
    pub fn modem_rx_free(&self) -> usize {
        self.mem.modem_rx_free()
    }

    /// Pop one received byte for the BDOS `AUX:`-input path (function 3).
    pub fn modem_rx_pop(&mut self) -> Option<u8> {
        self.mem.modem_rx_pop()
    }

    /// Push one byte from the BDOS `AUX:`-output path (function 4) toward the
    /// peer.
    pub fn modem_tx_push(&mut self, b: u8) {
        self.mem.modem_tx_push(b);
    }

    /// Total instructions executed since the last `load_com` (diagnostics).
    pub fn instructions(&self) -> u64 {
        self.instructions
    }
}

/// Place a 32-byte directory entry into the guest's 128-byte DMA record for
/// a search result; the rest is filled with the CP/M "empty entry" marker
/// (0xE5) so a scanner sees only slot 0 (directory code 0) as valid.
fn write_dir_record(cpm: &mut Cpm, dma: u16, entry: &[u8; 32]) {
    let mut record = [0xE5u8; 128];
    record[..32].copy_from_slice(entry);
    cpm.write_block(dma, &record);
}

/// Service the "disk system" BDOS calls that need only guest memory + the
/// filesystem (drive select, DMA, and the FCB file operations) — i.e. every
/// BDOS call that performs **no** console/session I/O.  Returns
// Synthesized virtual-disk geometry for the BDOS disk-info queries STAT uses
// to report "bytes remaining".  An 8 MB drive with 4 KB allocation blocks:
// 2048 blocks, 1024 directory entries.  The geometry is fixed for every drive
// (all drives are host folders of effectively the same capacity); only the
// allocation vector varies, reflecting each drive's actual usage.
const VD_BLS: u64 = 4096; // allocation block size
const VD_BSH: u8 = 5; // block shift (128 << 5 = 4096)
const VD_BLM: u8 = 31; // block mask (4096/128 - 1)
const VD_EXM: u8 = 1; // extent mask (BLS=4096, DSM>255)
const VD_DSM: u16 = 2047; // highest block number (2048 blocks × 4 KB = 8 MB)
const VD_DRM: u16 = 1023; // highest directory entry (1024 entries)
const VD_AL0: u8 = 0xFF; // 8 directory blocks reserved (32 KB / 4 KB)
const VD_AL1: u8 = 0x00;
const VD_DIR_BLOCKS: u64 = 8; // blocks the directory occupies (per AL0/AL1)
const VD_CKS: u16 = 0; // fixed disk: no directory-checksum vector
const VD_OFF: u16 = 0; // no reserved (system) tracks
const VD_SPT: u16 = 128; // 128-byte sectors per track (cosmetic for STAT)

// Reserved-RAM scratch (in the 0xFE00..0xFFFF system area, above the TPA and
// the downward-growing stack, which a well-behaved program never touches)
// where the synthesized DPB + allocation vector are materialized for the guest
// to read via the address the query returns.
//
// The allocation vector is 256 bytes (2048 blocks / 8), so it fills the entire
// 0xFE00..0xFF00 window below the BIOS jump table at [`BIOS_BASE`] (0xFF00);
// the 15-byte DPB therefore lives ABOVE the table's trap range, at 0xFF60.
// Keeping DPB and alloc at DISTINCT addresses also matters for STAT: it reads
// the DPB, then the alloc vector, so a shared buffer would let Get-Alloc
// clobber DPB fields STAT still needs.  `test_get_alloc_preserves_bios_table`
// and `test_scratch_layout_clear_of_bios_table` pin this invariant — an
// earlier ALLOC_ADDR of 0xFE90 overran straight through the BIOS table.
const DPB_ADDR: u16 = 0xFF60;
const ALLOC_ADDR: u16 = 0xFE00;

/// The fixed 15-byte CP/M 2.2 Disk Parameter Block for the virtual drive.
fn build_dpb() -> [u8; 15] {
    let mut d = [0u8; 15];
    d[0..2].copy_from_slice(&VD_SPT.to_le_bytes());
    d[2] = VD_BSH;
    d[3] = VD_BLM;
    d[4] = VD_EXM;
    d[5..7].copy_from_slice(&VD_DSM.to_le_bytes());
    d[7..9].copy_from_slice(&VD_DRM.to_le_bytes());
    d[9] = VD_AL0;
    d[10] = VD_AL1;
    d[11..13].copy_from_slice(&VD_CKS.to_le_bytes());
    d[13..15].copy_from_slice(&VD_OFF.to_le_bytes());
    d
}

/// Handle the disk-info "Get Addr(...)" BDOS calls STAT uses for free space:
/// 31 = Get Addr(DPB), 27 = Get Addr(Alloc).  Materializes a synthesized DPB /
/// allocation vector in reserved guest RAM and returns its address (to be put
/// in `HL`).  `None` for any other function.
pub fn disk_info_bdos(cpm: &mut Cpm, fs: &CpmFs, func: u8) -> Option<u16> {
    match func {
        // Return login vector: HL = bitmap of active drives (bit 0 = A:).
        // Every drive A:–P: exists (its folder is auto-created), so all
        // sixteen bits are set.  Without this the call returned 0 (no
        // drives), which confused drive-enumeration utilities.
        24 => Some(0xFFFF),
        // Get R/O Vector: HL = bitmap of software write-protected drives
        // (bit 0 = A:), as set by BDOS 28 and cleared by BDOS 13 / 37.  This
        // returned a hardcoded 0 for as long as nothing could *become* R/O;
        // now that BDOS 28 works, reporting the real bitmap is what keeps
        // 28 and 29 consistent with each other.
        29 => Some(fs.ro_vector()),
        31 => {
            cpm.write_block(DPB_ADDR, &build_dpb());
            Some(DPB_ADDR)
        }
        27 => {
            // Allocation vector: a set bit marks a used block — the reserved
            // directory blocks plus the blocks this drive's files occupy — so
            // STAT's free count (zero bits × block size) reflects real usage.
            let total = VD_DSM as u64 + 1; // 2048 blocks
            let used =
                (VD_DIR_BLOCKS + fs.current_drive_used_blocks(VD_BLS, total, VD_DIR_BLOCKS)).min(total);
            let nbytes = (VD_DSM as usize / 8) + 1; // 256 bytes = exactly 2048 bits
            let mut vec = vec![0u8; nbytes];
            for b in 0..used as usize {
                vec[b / 8] |= 0x80 >> (b % 8); // MSB-first, matching AL0/AL1
            }
            cpm.write_block(ALLOC_ADDR, &vec);
            Some(ALLOC_ADDR)
        }
        _ => None,
    }
}

/// `Some(return_code)` when `func` is one of these, or `None` for a
/// console-group call the async driver must handle itself.
///
/// Keeping this glue in the core (rather than inline in the telnet driver)
/// gives it a single implementation that is unit-testable without a live
/// session, and lets both the driver and the end-to-end roundtrip test
/// exercise the *same* code.
pub fn service_disk_bdos(cpm: &mut Cpm, fs: &mut CpmFs, func: u8) -> Option<u8> {
    // Read the FCB at DE, run `op` on it, and (if `op` returns a code)
    // persist the possibly-updated position fields back to guest memory.
    fn with_fcb(
        cpm: &mut Cpm,
        op: impl FnOnce(&mut Cpm, &mut Fcb) -> u8,
    ) -> u8 {
        let de = cpm.reg16(Reg16::DE);
        let mut raw = cpm.read_block(de, FCB_SIZE);
        let mut fcb = Fcb::from_bytes(&raw);
        let code = op(cpm, &mut fcb);
        fcb.store_position(&mut raw);
        cpm.write_block(de, &raw);
        code
    }

    match func {
        // 7 = Get I/O Byte, 8 = Set I/O Byte.  The IOBYTE lives at its
        // architectural home, 0x0003 in page zero; backing get/set with that
        // byte makes a program's set-then-get round-trip self-consistent.
        // Logical-device redirection has no observable effect in the
        // single-console model, but the value is now honestly stored and
        // returned instead of being dropped (get → 0, set → no-op).
        7 => Some(cpm.read_block(IOBYTE_ADDR, 1)[0]),
        8 => {
            let iobyte = cpm.reg8(Reg8::E);
            cpm.write_block(IOBYTE_ADDR, &[iobyte]);
            Some(0)
        }
        13 => {
            // Reset disk system: default drive A:, DMA 0x0080, and every
            // software write-protect released (BDOS 28's flag lives only until
            // the next disk reset).
            fs.select(0);
            fs.set_dma(fs::DEFAULT_DMA);
            fs.clear_all_drive_ro();
            cpm.set_current_disk(fs.current_drive(), fs.current_user());
            // A = 0FFH when the drive just logged in holds a temporary
            // `$`-prefixed file, else 0.  Not decoration: this is how a fresh
            // CCP learns a SUBMIT batch is already running — `CCP22.ASM` does
            // `CALL RESET` (this function) and stores A straight into its
            // submit flag.  Real CP/M sets it from the login directory scan in
            // `BDOS22.ASM` (`SUI '$'` on the first filename byte); we ask the
            // filesystem the same question.  Returning a flat 0 here was only
            // *accidentally* harmless because our own CCP-lite checks for
            // `A:$$$.SUB` directly rather than trusting this.
            Some(if fs.has_temp_dollar_file() { 0xFF } else { 0 })
        }
        14 => {
            // Select disk: E = drive (0 = A:).  Keep the page-zero CDISK
            // byte in step so a program that reads 0x0004 after selecting
            // sees the drive it just chose.
            let e = cpm.reg8(Reg8::E);
            fs.select(e);
            cpm.set_current_disk(fs.current_drive(), fs.current_user());
            Some(0)
        }
        15 => Some(with_fcb(cpm, |_cpm, fcb| {
            if fs.open_existing(fcb) {
                fcb.ex = 0;
                fcb.s2 = 0;
                fcb.cr = 0;
                fcb.rc = 0;
                0x00
            } else {
                0xFF
            }
        })),
        16 => Some(with_fcb(cpm, |_cpm, fcb| {
            // Close: writes here are write-through and there is no directory to
            // rewrite, so there is nothing to flush — but the RETURN CODE still
            // has to be right.  Real CP/M answers 0FFH when the file cannot be
            // closed, and `BDOS22.ASM` spells that exit out:
            //
            //     ; ERROR EXIT: RETURN PARAMETER SET TO 0FFH
            //     ;             MEANING THAT FILE CANNOT BE CLOSED
            //     CLOSE7: LXI H,RETPAR / DCR M / RET
            //
            // matching the documented contract (255 when the name is not in the
            // directory).  This used to return a flat 0, so closing an FCB
            // naming a file that is not there reported success.
            //
            // Two cases return SUCCESS without looking for the file at all,
            // taken from the same listing rather than assumed — `CLOSEF` does
            // `CALL GETRO / RNZ` and `CALL FCB14 / ANI 80H / RNZ`, both with the
            // return parameter still 0:
            //   * a software write-protected drive (BDOS 28) — a R/O drive is
            //     not a close *error*, there is simply nothing to write back;
            //   * FCB byte 14 (S2) with its high bit set, CP/M's "this extent
            //     needs no directory update" marker.
            // Byte 14 is read unmasked by `Fcb::from_bytes`, so the flag
            // survives to be tested here.
            // Named rather than inlined into one condition so the two kinds
            // of success stay distinguishable to a reader: these two answer
            // without consulting the directory at all, and `||` keeps that
            // literal — `open_existing` is not called when either holds.
            let no_directory_work = fs.fcb_drive_is_ro(fcb) || fcb.s2 & 0x80 != 0;
            // Whatever the return code, the guest is finished with this file:
            // let another session write it without waiting for this one to
            // leave the emulator (see CPM_WRITERS in fs.rs).
            fs.release_file(fcb);
            if no_directory_work || fs.open_existing(fcb) {
                0x00
            } else {
                0xFF
            }
        })),
        17 => {
            let de = cpm.reg16(Reg16::DE);
            let raw = cpm.read_block(de, FCB_SIZE);
            let fcb = Fcb::from_bytes(&raw);
            match fs.search_first(&fcb) {
                Some(entry) => {
                    write_dir_record(cpm, fs.dma(), &entry);
                    Some(0)
                }
                None => Some(0xFF),
            }
        }
        18 => match fs.search_next() {
            Some(entry) => {
                write_dir_record(cpm, fs.dma(), &entry);
                Some(0)
            }
            None => Some(0xFF),
        },
        19 => {
            let de = cpm.reg16(Reg16::DE);
            let raw = cpm.read_block(de, FCB_SIZE);
            let fcb = Fcb::from_bytes(&raw);
            Some(if fs.delete(&fcb) > 0 { 0x00 } else { 0xFF })
        }
        20 => Some(with_fcb(cpm, |cpm, fcb| {
            let rec = fcb.seq_record();
            match fs.read_record(fcb, rec) {
                Ok(Some(buf)) => {
                    cpm.write_block(fs.dma(), &buf);
                    fcb.advance_record();
                    0x00
                }
                Ok(None) | Err(_) => 0x01, // EOF / error
            }
        })),
        21 => Some(with_fcb(cpm, |cpm, fcb| {
            let rec = fcb.seq_record();
            let dma = cpm.read_block(fs.dma(), 128);
            let mut data = [0u8; 128];
            data.copy_from_slice(&dma);
            match fs.write_record(fcb, rec, &data) {
                Ok(()) => {
                    fcb.advance_record();
                    0x00
                }
                // Write sequential: 0 = OK, nonzero = error (0xFF is CP/M's
                // "file not found", not a write code — a full/failed write is
                // a generic 0x01 that every caller reads as failure).
                Err(_) => 0x01,
            }
        })),
        22 => Some(with_fcb(cpm, |_cpm, fcb| {
            if fs.make(fcb) {
                fcb.ex = 0;
                fcb.s2 = 0;
                fcb.cr = 0;
                fcb.rc = 0;
                0x00
            } else {
                0xFF
            }
        })),
        23 => {
            let de = cpm.reg16(Reg16::DE);
            let raw = cpm.read_block(de, FCB_SIZE);
            let old = Fcb::from_bytes(&raw);
            // New name in the FCB's second half: byte 16 drive, 17..25 name,
            // 25..28 ext.
            let mut new_name = [b' '; 8];
            let mut new_ext = [b' '; 3];
            for (slot, &src) in new_name.iter_mut().zip(&raw[17..25]) {
                *slot = src & 0x7F;
            }
            for (slot, &src) in new_ext.iter_mut().zip(&raw[25..28]) {
                *slot = src & 0x7F;
            }
            Some(if fs.rename(&old, &new_name, &new_ext) {
                0x00
            } else {
                0xFF
            })
        }
        25 => Some(fs.current_drive()), // current disk
        26 => {
            let de = cpm.reg16(Reg16::DE);
            fs.set_dma(de);
            Some(0)
        }
        28 => {
            // Write Protect Disk: software write-protect the current drive
            // until the next disk reset.  Enforced in the filesystem's four
            // mutating paths (write / make / delete / rename) and reported by
            // BDOS 29.
            fs.set_drive_ro();
            Some(0)
        }
        30 => {
            // Set File Attributes.  The attribute bits ride the *high* bits of
            // the FCB's name and extension bytes, which `Fcb::from_bytes`
            // strips, so read them from the raw FCB the way BDOS 23 reads its
            // second-half name.
            //
            // Only t1' (R/O, high bit of the first extension byte) is honoured;
            // it maps onto the host file's read-only permission.  t2' (System)
            // and t3' (Archive) are accepted and ignored — a plain host
            // directory has nowhere to keep them, and inventing a sidecar file
            // would litter the folders users drop their own files into.
            // Returns 0xFF when the file doesn't exist, as CP/M does.
            let de = cpm.reg16(Reg16::DE);
            let raw = cpm.read_block(de, FCB_SIZE);
            let ro = raw[9] & 0x80 != 0;
            let fcb = Fcb::from_bytes(&raw);
            if fs.fcb_drive_is_ro(&fcb) {
                return Some(0xFF);
            }
            Some(match fs.set_file_ro(&fcb, ro) {
                Some(()) => 0x00,
                None => 0xFF,
            })
        }
        37 => {
            // Reset Drive: release the software write-protect on the drives
            // selected by the DE bitmap (bit 0 = A:).
            //
            // In real CP/M this also marks those drives "not logged in" so the
            // next access re-reads the directory.  Here that half is a genuine
            // no-op rather than a stub: drives are host folders that are always
            // present, and the CP/M directory is synthesized live from them on
            // every search, so there is no cached state to invalidate and no
            // media change to detect.
            let de = cpm.reg16(Reg16::DE);
            fs.clear_drive_ro(de);
            Some(0)
        }
        32 => {
            // Get/Set user number: E=0xFF gets (returns current user in A),
            // else sets it (0–15).  Returns the current user in A either way.
            let e = cpm.reg8(Reg8::E);
            if e != 0xFF {
                fs.set_user(e);
                cpm.set_current_disk(fs.current_drive(), fs.current_user());
            }
            Some(fs.current_user())
        }
        33 => Some(with_fcb(cpm, |cpm, fcb| {
            let rr = fcb.random_record();
            match fs.read_record(fcb, rr) {
                Ok(Some(buf)) => {
                    cpm.write_block(fs.dma(), &buf);
                    fcb.set_seq_record(rr);
                    0x00
                }
                Ok(None) | Err(_) => 0x01,
            }
        })),
        // 34 = write random; 40 = write random with zero-fill.  For our
        // byte-exact host files the two are identical (a write past EOF is
        // zero-filled by the OS either way), so 40 aliases 34 — without this
        // it fell through to the "unknown" arm and returned 0 (fake success)
        // while silently dropping the write.
        34 | 40 => Some(with_fcb(cpm, |cpm, fcb| {
            let rr = fcb.random_record();
            let dma = cpm.read_block(fs.dma(), 128);
            let mut data = [0u8; 128];
            data.copy_from_slice(&dma);
            match fs.write_record(fcb, rr, &data) {
                Ok(()) => {
                    fcb.set_seq_record(rr);
                    0x00
                }
                // Write random uses CP/M's documented error codes: 0x06 =
                // "R/W past the physical end of disk" (our per-file size cap
                // rejects the record as InvalidInput), else 0x05 = write /
                // directory-overflow error.  (0xFF was never a write code.)
                Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => 0x06,
                Err(_) => 0x05,
            }
        })),
        35 => {
            // Compute file size -> set R0..R2 to the record count.
            let de = cpm.reg16(Reg16::DE);
            let mut raw = cpm.read_block(de, FCB_SIZE);
            let fcb = Fcb::from_bytes(&raw);
            let recs = fs.file_size_records(&fcb).unwrap_or(0);
            raw[33] = recs as u8;
            raw[34] = (recs >> 8) as u8;
            raw[35] = (recs >> 16) as u8;
            cpm.write_block(de, &raw);
            Some(0)
        }
        36 => {
            // Set random record from the current sequential position.
            let de = cpm.reg16(Reg16::DE);
            let mut raw = cpm.read_block(de, FCB_SIZE);
            let fcb = Fcb::from_bytes(&raw);
            let rr = fcb.seq_record();
            raw[33] = rr as u8;
            raw[34] = (rr >> 8) as u8;
            raw[35] = (rr >> 16) as u8;
            cpm.write_block(de, &raw);
            Some(0)
        }
        _ => None, // console-group / unknown: handled by the caller
    }
}

#[cfg(test)]
mod tests {
    /// The RTC buffer is BCD, per the published HBIOS interface — a plain
    /// binary byte would read as a different (and often impossible) number on
    /// the guest side: 0x1F is 31 in binary but not a valid BCD date at all.
    #[test]
    fn test_clock_bcd_packing() {
        assert_eq!(to_bcd(0), 0x00);
        assert_eq!(to_bcd(9), 0x09);
        assert_eq!(to_bcd(10), 0x10);
        assert_eq!(to_bcd(42), 0x42);
        assert_eq!(to_bcd(99), 0x99);

        // 2026-07-31 22:05:09 -> the six bytes RomWBW's buffer expects.
        assert_eq!(
            clock_bcd_from_parts(2026, 7, 31, 22, 5, 9),
            [0x26, 0x07, 0x31, 0x22, 0x05, 0x09]
        );
        // A leap second (tm_sec == 60) has no BCD-legal home; clamp it.
        assert_eq!(clock_bcd_from_parts(2016, 12, 31, 23, 59, 60)[5], 0x59);
        // Only the last two digits of the year fit, as on period hardware.
        assert_eq!(clock_bcd_from_parts(2100, 1, 1, 0, 0, 0)[0], 0x00);
    }

    /// The civil-date conversion behind the non-Unix fallback, on the dates
    /// that break naive implementations: the epoch, both kinds of century
    /// boundary, and a leap day.
    #[test]
    fn test_civil_from_days_landmarks() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        // Day numbers cross-checked against a reference calendar rather than
        // worked out by hand — the first draft of this test had them a day out
        // and would have "confirmed" a broken conversion.
        assert_eq!(civil_from_days(11016), (2000, 2, 29)); // 2000 IS a leap year
        assert_eq!(civil_from_days(11017), (2000, 3, 1));
        assert_eq!(civil_from_days(19723), (2024, 1, 1));
        assert_eq!(civil_from_days(20513), (2026, 3, 1));
        // ...and 2100 is NOT a leap year, the case the /100 and /400 terms
        // exist for: the day after 28 February is 1 March.
        assert_eq!(civil_from_days(47540), (2100, 2, 28));
        assert_eq!(civil_from_days(47541), (2100, 3, 1));
    }

    /// The live clock must produce a buffer a guest can actually read: every
    /// byte legal BCD, and the fields inside their calendar ranges.
    #[test]
    fn test_host_clock_is_legal_bcd() {
        let t = host_clock_bcd();
        for (i, b) in t.iter().enumerate() {
            assert!(
                (b >> 4) <= 9 && (b & 0x0F) <= 9,
                "byte {i} = {b:#04x} is not valid BCD"
            );
        }
        let dec = |b: u8| (b >> 4) * 10 + (b & 0x0F);
        assert!((1..=12).contains(&dec(t[1])), "month {}", dec(t[1]));
        assert!((1..=31).contains(&dec(t[2])), "day {}", dec(t[2]));
        assert!(dec(t[3]) <= 23, "hour {}", dec(t[3]));
        assert!(dec(t[4]) <= 59, "minute {}", dec(t[4]));
        assert!(dec(t[5]) <= 59, "second {}", dec(t[5]));
    }

    use super::*;


    /// Drive a program to completion the way the async session will, but
    /// synchronously: service BDOS console-output calls into a byte buffer
    /// and stop on warm boot.  Returns (console_output, stop_reason).
    fn drive(program: &[u8]) -> (Vec<u8>, Stop) {
        let mut cpm = Cpm::new();
        cpm.load_com(program);
        let abort = AtomicBool::new(false);
        let mut out = Vec::new();
        loop {
            match cpm.run(100_000, &abort) {
                Stop::Bdos(func) => {
                    match func {
                        2 => {
                            // Console output: char in E.
                            out.push(cpm.reg8(Reg8::E));
                            cpm.bdos_return(0);
                        }
                        9 => {
                            // Print $-terminated string at DE.
                            let de = cpm.reg16(Reg16::DE);
                            out.extend(cpm.read_dollar_string(de, 4096));
                            cpm.bdos_return(0);
                        }
                        _ => cpm.bdos_return(0),
                    }
                }
                other => return (out, other),
            }
        }
    }

    // NOTE: `PRELIM.COM` cannot pass here, and the reason is settled — do not
    // spend another session on it. It is **not** our pushes and never was.
    //
    // This note used to say the pushed block "lands one byte high" with an
    // SP-logged instruction trace as the next step. That diagnosis was anchored
    // on the wrong address: `regs2` is at `0559h`, not `0558h` — the `hlval`
    // bytes `A5 3C` that follow it sit at `056Dh` and pin it — so the block was
    // always in the right place, with only its top byte wrong.
    //
    // The fault is upstream of the register test. PRELIM opens by rotating all
    // 65,536 bytes down one with `LDIR` and back up with `LDDR` from
    // `HL = 0080h`, which passes over its own `LDIR` at `01A4h`; the write
    // pointer trails the read pointer by one, so at `HL = 01A5h` it replaces
    // the `ED` with `B0`. A repeating block instruction re-fetches its opcode
    // every iteration (`PC ← PC - 2`), so the copy stops there while the
    // following `LDDR` — whose writes move *away* from its own bytes — runs to
    // completion. Memory is left shifted up one byte and every later test reads
    // the wrong byte. See `test_a_block_move_that_overwrites_its_own_opcode_stops_there`.
    //
    // That is the hardware's behaviour, not a defect: z80pack ships both models
    // and calls the other one `FAST_BLOCK`, "much faster but not accurate Z80
    // block instr." Its `cpmsim` — which carries these very disks — defines it;
    // its accurate machines do not. PRELIM's rotation survives only there.
    //
    // The core itself is covered by the suites that *can* judge it: EXZ80DOC
    // passes all 79 groups, and Diagnostics II reports `CPU TESTS OK`.

    /// The shadow-register round trip `PRELIM.COM` rejects, in isolation.
    ///
    /// Found by running the real exerciser (see the suite test below): PRELIM
    /// pops known values into both register sets, pushes them back to a known
    /// address, and compares memory. It fails at its check of the pushed `AF'`
    /// and its failure action is a silent `JMP 0000`, so this reproduces the
    /// same round trip in twelve bytes to say *which* instruction is wrong
    /// rather than "a CPU suite failed".
    ///
    /// Not `#[ignore]`d: it needs no external file, and if it ever starts
    /// passing that is news.
    #[test]
    fn test_shadow_register_round_trip() {
        let _g = crate::cpm::image::registry::tests_lock();
        // AF' = 0x0402 (A'=04, F'=02), AF = 0x0806 (A=08, F=06) — the same
        // shape of values PRELIM uses, chosen so a swapped or masked byte is
        // obvious in the failure message.
        const DATA: u16 = 0x0200;
        const OUT: u16 = 0x0300;
        let prog = [
            0x31, 0x00, 0x02, // LXI SP,DATA
            0xF1, //             POP AF        ; AF = 0402 (destined for shadow)
            0x08, //             EX AF,AF'
            0xF1, //             POP AF        ; AF = 0806
            0x31, 0x04, 0x03, // LXI SP,OUT+4
            0xF5, //             PUSH AF       ; main AF -> OUT+2
            0x08, //             EX AF,AF'
            0xF5, //             PUSH AF       ; shadow AF -> OUT
            0x76, //             HLT
        ];
        let mut cpm = Cpm::new();
        cpm.load_com(&prog);
        cpm.write_block(DATA, &[0x02, 0x04, 0x06, 0x08]);
        let abort = AtomicBool::new(false);
        // A HLT stops the machine; a budget bounds it either way.
        let _ = cpm.run(200, &abort);
        let got = cpm.read_block(OUT, 4);
        assert_eq!(
            got,
            vec![0x02, 0x04, 0x06, 0x08],
            "shadow/main AF did not round-trip through EX AF,AF' + PUSH/POP.\n\
             expected F'=02 A'=04 F=06 A=08, got F'={:02X} A'={:02X} F={:02X} A={:02X}",
            got[0], got[1], got[2], got[3]
        );
    }

    /// The whole register-file push `PRELIM.COM` builds, in isolation.
    ///
    /// PRELIM sets `SP` to a known address, pushes `IY IX HL DE BC AF`, swaps to
    /// the shadow set and pushes `HL' DE' BC' AF'`, then checks the twenty bytes
    /// that lands. It was written to answer whether the fault PRELIM reports is
    /// in the pushes themselves or in something it does before them, and the
    /// answer is **before them** — the pushes are right, and this passing is
    /// what says so. See the note above `test_the_shadow_af_round_trip`.
    #[test]
    fn test_the_register_file_pushes_where_prelim_expects() {
        let _g = crate::cpm::image::registry::tests_lock();
        const TOP: u16 = 0x0400;
        // Load every register with a distinct value, then push exactly as
        // PRELIM does. Values ascend so a misplaced byte names itself.
        let prog = [
            0x31, 0x00, 0x03, //       LXI SP,0300      ; load area
            0xF1, //                   POP AF           ; AF' <- 0402
            0xC1, //                   POP BC           ; BC' <- 0806
            0xD1, //                   POP DE           ; DE' <- 0C0A
            0xE1, //                   POP HL           ; HL' <- 100E
            0x08, 0xD9, //             EX AF,AF' / EXX  ; park them as shadow
            0xF1, //                   POP AF           ; AF  <- 1412
            0xC1, //                   POP BC
            0xD1, //                   POP DE
            0xE1, //                   POP HL
            0xDD, 0xE1, //             POP IX
            0xFD, 0xE1, //             POP IY
            0x31, 0x00, 0x04, //       LXI SP,TOP
            0xFD, 0xE5, //             PUSH IY
            0xDD, 0xE5, //             PUSH IX
            0xE5, 0xD5, 0xC5, 0xF5, // PUSH HL/DE/BC/AF
            0x08, 0xD9, //             EX AF,AF' / EXX
            0xE5, 0xD5, 0xC5, 0xF5, // PUSH HL'/DE'/BC'/AF'
            0x76, //                   HLT
        ];
        let mut cpm = Cpm::new();
        cpm.load_com(&prog);
        // 11 register pairs' worth of ascending bytes for the POPs to consume.
        let load: Vec<u8> = (0..22).map(|i| (i as u8 + 1) * 2).collect();
        cpm.write_block(0x0300, &load);
        let abort = AtomicBool::new(false);
        let _ = cpm.run(400, &abort);

        // Ten pushes of two bytes each land at TOP-20 .. TOP-1.
        let got = cpm.read_block(TOP - 20, 20);
        let below = cpm.read_block(TOP - 21, 1);
        assert_eq!(
            below[0], 0,
            "a push wrote below TOP-20 — the block would be shifted, which is \
             what PRELIM was once thought to be reporting"
        );
        assert_eq!(got.len(), 20);
        // The last push (AF') must be the lowest pair: F' then A'.
        assert_eq!(
            (got[0], got[1]),
            (0x02, 0x04),
            "AF' did not land at the bottom of the block; got {got:02X?}"
        );
    }

    /// **`LDIR`/`LDDR` with `BC = 0` must move 65,536 bytes, not none.**
    ///
    /// The counter is decremented *before* it is tested, so zero wraps to
    /// `FFFFh` and the block runs the whole address space. This is not a corner
    /// nobody reaches: it is the first thing `PRELIM.COM` does, and it is how
    /// its author checks the repeat loop terminates on the counter rather than
    /// on a sign or a zero test done in the wrong order.
    ///
    /// Kept small and one-directional so a failure names which instruction is
    /// wrong instead of reporting "memory looks odd".
    #[test]
    fn test_a_block_move_with_bc_zero_moves_the_whole_address_space() {
        let _g = crate::cpm::image::registry::tests_lock();
        // LDIR: copy 0200h.. down one byte, wrapping the whole way round.
        let prog = [
            0x21, 0x00, 0x02, // LD HL,0200h
            0x11, 0xFF, 0x01, // LD DE,01FFh
            0x01, 0x00, 0x00, // LD BC,0000h
            0xED, 0xB0, //       LDIR
            0x76, //             HLT
        ];
        let mut cpm = Cpm::new();
        cpm.load_com(&prog);
        cpm.write_block(0x0200, &[0xAA, 0xBB, 0xCC]);
        let abort = AtomicBool::new(false);
        // Generous: a full sweep is 65,536 transfers.
        let _ = cpm.run(400_000, &abort);
        assert_eq!(
            cpm.read_block(0x01FF, 3),
            vec![0xAA, 0xBB, 0xCC],
            "LDIR with BC=0 must copy 65,536 bytes — the source moved down one \
             byte. Copying none leaves the destination untouched, which is what \
             shifts every later address in PRELIM by one and makes its register \
             test read the wrong byte."
        );

        // LDDR, the other direction, for the same reason.
        let prog = [
            0x21, 0x00, 0x02, // LD HL,0200h
            0x11, 0x01, 0x02, // LD DE,0201h
            0x01, 0x00, 0x00, // LD BC,0000h
            0xED, 0xB8, //       LDDR
            0x76, //             HLT
        ];
        let mut cpm = Cpm::new();
        cpm.load_com(&prog);
        cpm.write_block(0x01FE, &[0xAA, 0xBB, 0xCC]);
        let _ = cpm.run(400_000, &abort);
        assert_eq!(
            cpm.read_block(0x01FF, 3),
            vec![0xAA, 0xBB, 0xCC],
            "LDDR with BC=0 must copy 65,536 bytes too"
        );
    }

    /// **A repeating block instruction re-fetches its own opcode, so it stops
    /// if the copy overwrites it.**
    ///
    /// This is the whole of why `PRELIM.COM` cannot pass here, and it is a
    /// property worth pinning rather than a curiosity. The Z80 implements
    /// `LDIR` as *one* transfer followed by `PC ← PC - 2`, so the instruction is
    /// fetched again from memory on every iteration. A copy that runs over its
    /// own two opcode bytes therefore changes what executes next — the loop
    /// ends there, mid-block.
    ///
    /// `PRELIM` opens by rotating all 65,536 bytes down one byte with `LDIR`
    /// and back up with `LDDR`, from `HL = 0080h` — which passes straight over
    /// its own `LDIR` at `01A4h`. On this model the `LDIR` stops after ~294
    /// bytes while the following `LDDR` (whose writes move *away* from its
    /// opcode) runs to completion, so memory is left shifted up by one and
    /// every later test reads the wrong byte. That is not a fault in the core:
    /// it is what the hardware does.
    ///
    /// z80pack, whose disks carry these exercisers, ships both models and says
    /// so in `sim.h` — `FAST_BLOCK`, "much faster but not accurate Z80 block
    /// instr.", loops internally and cannot see the overwrite. Its `cpmsim`
    /// defines it; its accurate machines (`cromemcosim`, `mosteksim`, `picosim`)
    /// leave it commented out. `PRELIM` passes only on the inaccurate one.
    #[test]
    fn test_a_block_move_that_overwrites_its_own_opcode_stops_there() {
        let _g = crate::cpm::image::registry::tests_lock();
        // The copy starts *below* this program and moves up, writing one byte
        // behind itself — so the destination pointer walks over the `LDIR`'s
        // own two bytes at 0109h, exactly as PRELIM's does at 01A4h.
        let prog = [
            0x01, 0x40, 0x00, // 0100: LD BC,0040h   (64 — more than it will get)
            0x21, 0xF0, 0x00, // 0103: LD HL,00F0h   (source, below the program)
            0x11, 0xEF, 0x00, // 0106: LD DE,00EFh   (dest = source - 1)
            0xED, 0xB0, //       0109: LDIR
            0x76, //             010B: HLT
        ];
        let mut cpm = Cpm::new();
        cpm.load_com(&prog);
        cpm.write_block(0x00F0, &[0xAA]); // proves the copy ran at all
        cpm.write_block(0x0120, &[0xEE]); // beyond where it can reach if it stops
        let abort = AtomicBool::new(false);
        let _ = cpm.run(100_000, &abort);

        assert_eq!(cpm.read_block(0x00EF, 1), vec![0xAA], "the copy never started");
        // The destination reaches 0109h when the source reaches 010Ah — 27
        // transfers in, well short of the 64 the counter allows. Everything
        // past that must be untouched.
        assert_eq!(
            cpm.read_block(0x011F, 1),
            vec![0x00],
            "the copy ran past its own opcode. A repeating block instruction \
             re-fetches, so overwriting its opcode has to stop it; looping \
             internally is z80pack's FAST_BLOCK, which its own header calls \
             'not accurate'."
        );
    }

    /// **Run a real CPU conformance suite against our Z80 core.**
    ///
    /// Everything else here tests the *gateway*: our BDOS, our controllers, our
    /// console. Nothing has ever tested the CPU itself, and every emulation
    /// fault found so far was found indirectly — a disk misbehaving, a sign-on
    /// coming back corrupted. This tests it directly, with the instruction
    /// exercisers the 8080/Z80 world settled on:
    ///
    /// * `PRELIM.COM` — Frank Cringle's preliminary tests. **Expected to fail
    ///   here, for a reason that is settled and is not a fault in the core** —
    ///   see the note above `test_the_shadow_af_round_trip`. It opens by
    ///   rotating all of memory with `LDIR`/`LDDR` across the instruction's own
    ///   opcode, which only survives on an emulator that loops block
    ///   instructions internally instead of re-fetching. Its failure action is
    ///   a silent `JMP 0000`, so it simply prints nothing.
    /// * `8080PRE.COM` — the 8080 equivalent.
    /// * `CPUTEST.COM` — Diagnostics II by Supersoft. Broad, and it names the
    ///   area that failed.
    /// * `EXZ80DOC.COM` — the ZEXALL family, *documented* flags only. It
    ///   compares a CRC per instruction group against a known-good value, so it
    ///   cannot be satisfied by output that merely looks plausible — the
    ///   exact-oracle property this project keeps choosing. **All 79 groups
    ///   pass**, ending `All tests successful.` The last one to fall was
    ///   `<ini,outi,ind,outd><,r>`, and it was not an instruction bug at all:
    ///   those instructions copy a byte *from a port* into memory and set `N`
    ///   from its top bit, so an unclaimed port's value lands in the CRC. It
    ///   read `0` here and `0xFF` everywhere else — see `CpmMachine::port_in`.
    ///
    /// * `EXZ80ALL.COM` — **the same exerciser with the undocumented flag bits
    ///   pinned as well, and it also passes all 79 groups.** Its banner says
    ///   `Undefined status bits taken into account`, against the doc build's
    ///   `NOT taken into account`, which is how you tell the two runs apart.
    ///
    ///   This bullet used to say that `EXZ80ALL` was unfair because iz80 "does
    ///   not claim to reproduce" those bits. That was simply wrong, and it left
    ///   a gap in the record that looked like a compatibility risk: iz80
    ///   implements bits 3 and 5 throughout, from Sean Young's *The
    ///   Undocumented Z80 Documented* — including the two places they do not
    ///   simply copy the result, the block instructions (TUZD-4.2) and 16-bit
    ///   add (TUZD-8.6). Measuring it took less time than the claim had been
    ///   sitting there.
    ///
    ///   **It is not on any disk — build it.** `ex.mac` is the source for the
    ///   whole family and `exkind` picks the variant (`0` = 8080, `1` = doc,
    ///   `2` = all), so with z80pack's own assembler:
    ///   ```text
    ///   z80asm -l -T -sn -p0 -dexkind=2 -fb -oexz80all.com ex.mac
    ///   ```
    ///   Validate the toolchain before trusting its output: build `-dexkind=1`
    ///   and check it still reports all 79 groups OK, which is what says the
    ///   assembler and the source agree with the `EXZ80DOC.COM` on the disk.
    ///   (Byte-identical it will not be — the disk copy is padded to a CP/M
    ///   record and fills uninitialised space differently.)
    ///
    /// The suites live on z80pack's `z80tests.dsk` and `i8080tests.dsk`, and
    /// `cpmls -f ibm-3740` / `cpmcp -f ibm-3740` read them — the skew *is* the
    /// IBM 3740 one. The disks also carry the sources, `prelim.mac` and
    /// `ex.mac`, which are worth more than the binaries when a group fails:
    /// they say what each test does and what it expects.
    ///
    /// This paragraph used to say the opposite — that `cpmsim`'s `SECTRAN` is
    /// `HL = BC + 1` with no translation, so an `ibm3740` reader would scramble
    /// the file, and to extract with a no-skew reader. That was read from the
    /// wrong branch of `SECTRAN`; the other one goes through the DPH's
    /// translation table, which is our `IBM3740_SKEW` plus one. It is a
    /// dangerous thing to get wrong in this direction: a scrambled directory
    /// still lists plausible filenames, so the mistake shows up as a CPU that
    /// appears to fail its own conformance suite.
    ///
    /// Ignored, and wants a release build — `EXZ80DOC` is billions of cycles:
    /// ```text
    /// CPM_CPUTEST_COM=/path/EXZ80DOC.COM cargo test --release \
    ///     --bin ethernetgateway test_cpu_conformance_suite -- --ignored --nocapture
    /// ```
    ///
    /// `CPM_CPUTEST_CPU=8080` runs the same harness on the 8080 core that
    /// `cpm_cpu = 8080` selects. That is how the 8080 setting is checked
    /// against software that can tell: `TEST8080.COM` identifies the processor
    /// from `DCR A` setting parity rather than overflow, so it fails on our Z80
    /// — correctly — and must pass here.
    #[test]
    #[ignore]
    fn test_cpu_conformance_suite() {
        use std::io::Write;
        let Ok(path) = std::env::var("CPM_CPUTEST_COM") else {
            eprintln!("set CPM_CPUTEST_COM to a .COM exerciser to run this");
            return;
        };
        let _g = crate::cpm::image::registry::tests_lock();
        let program = std::fs::read(&path).expect("the exerciser");
        // On whichever processor `cpm_cpu` names, defaulting to the Z80 as a
        // session does. `CPM_CPUTEST_CPU=8080` is how the 8080 setting gets
        // checked against real 8080 software: TEST8080 *fails* on our Z80 and
        // is right to — it detects the CPU from `DCR A` setting parity rather
        // than overflow — so it passing under this setting, and only under this
        // setting, is the strongest evidence available that the choice reaches
        // the machine rather than only the config file.
        let setting =
            std::env::var("CPM_CPUTEST_CPU").unwrap_or_else(|_| cpu::DEFAULT_CPU.to_string());
        println!("[cpu: {}]", cpu::cpu_label(&setting));
        let mut cpm = Cpm::new_for(&setting);
        cpm.load_com(&program);
        let abort = AtomicBool::new(false);

        // Streamed rather than collected, because a full ZEXALL run takes
        // minutes and a silent test that might have hung is not something you
        // can act on. The text is also the result: these suites report per-group
        // pass/fail as they go.
        let mut out = Vec::new();
        let emit = |bytes: &[u8], out: &mut Vec<u8>| {
            out.extend_from_slice(bytes);
            std::io::stdout().write_all(bytes).ok();
            std::io::stdout().flush().ok();
        };
        // A ring of recent PCs, for a suite whose failure action is a silent
        // `JMP 0000`. Off unless asked for, because single-stepping a suite like
        // ZEXALL is billions of iterations — but a *failing* suite stops early,
        // so the trace is affordable exactly when it is needed.
        // Set `CPM_CPUTEST_TRACE=1`.
        let trace = std::env::var("CPM_CPUTEST_TRACE").is_ok();
        let mut recent: std::collections::VecDeque<u16> = std::collections::VecDeque::new();

        loop {
            let step = if trace { 1 } else { 50_000_000 };
            if trace {
                let pc = cpm.pc();
                if recent.len() == 24 {
                    recent.pop_front();
                }
                recent.push_back(pc);
            }
            match cpm.run(step, &abort) {
                Stop::Bdos(func) => match func {
                    2 => {
                        let c = cpm.reg8(Reg8::E);
                        emit(&[c], &mut out);
                        cpm.bdos_return(0);
                    }
                    9 => {
                        let de = cpm.reg16(Reg16::DE);
                        let s = cpm.read_dollar_string(de, 8192);
                        emit(&s, &mut out);
                        cpm.bdos_return(0);
                    }
                    // Console status / input. An exerciser should not ask, but
                    // if one does, saying "no key waiting" beats hanging.
                    11 => cpm.bdos_return(0),
                    1 => cpm.bdos_return(b'\r'),
                    _ => cpm.bdos_return(0),
                },
                Stop::BudgetExhausted => continue,
                other => {
                    println!("\n[stopped: {other:?}]");
                    if trace {
                        println!(
                            "last PCs: {}",
                            recent.iter().map(|p| format!("{p:04X}")).collect::<Vec<_>>().join(" ")
                        );
                    }
                    // Post-mortem memory, for a suite that compares against a
                    // block it built rather than printing what it found.
                    // `CPM_CPUTEST_DUMP=0558:20` (hex addr, decimal len).
                    if let Ok(spec) = std::env::var("CPM_CPUTEST_DUMP") {
                        let (a, n) = spec.split_once(':').unwrap_or((spec.as_str(), "16"));
                        let a = u16::from_str_radix(a.trim_start_matches("0x"), 16).unwrap_or(0);
                        let n: usize = n.parse().unwrap_or(16);
                        println!("mem {a:04X}: {:02X?}", cpm.read_block(a, n));
                    }
                    break;
                }
            }
        }

        let text = String::from_utf8_lossy(&out).replace('\r', "");
        // What each suite says when a group fails. Checked as a set rather than
        // by looking for a success banner, because the suites differ in how they
        // announce success and agree on how they announce failure.
        for bad in ["ERROR", "error", "FAILED", "failed", "CPU HAS FAILED"] {
            assert!(
                !text.contains(bad),
                "the CPU suite reported {bad:?} — our core is wrong somewhere.\n{text}"
            );
        }
        assert!(!text.trim().is_empty(), "the exerciser printed nothing at all");
    }

    #[test]
    fn test_bios_jump_table_installed() {
        // Constructing a CP/M machine registers a session in the
        // process-global image registry, which makes drives look busy to
        // every other test.  Serialise with the registry tests.
        let _g = crate::cpm::image::registry::tests_lock();
        // load_com lays a real 17-entry BIOS jump table; the warm-boot
        // pointer at 0x0001 points at its WBOOT entry (the 2nd slot) so a
        // guest that reads 0x0001 walks a real table, and each slot is a
        // `JP <trap>` whose operand is the per-vector trap address.
        let mut cpm = Cpm::new();
        cpm.load_com(&[0xC9]); // trivial RET program
        assert_eq!(cpm.mem.peek16(0x0001), BIOS_BASE + 3);
        assert_eq!(cpm.mem.peek(0x0000), 0xC3); // JP at 0x0000
        for i in 0..BIOS_VECTORS {
            let slot = BIOS_BASE + 3 * i;
            assert_eq!(cpm.mem.peek(slot), 0xC3, "vector {i} is a JP");
            assert_eq!(cpm.mem.peek16(slot + 1), BIOS_TRAP + i, "vector {i} operand");
        }
    }

    #[test]
    fn test_bios_conout_through_table_and_direct() {
        // Constructing a CP/M machine registers a session in the
        // process-global image registry, which makes drives look busy to
        // every other test.  Serialise with the registry tests.
        let _g = crate::cpm::image::registry::tests_lock();
        // Both ways a guest reaches CONOUT (vector 4) must trap: CALLing the
        // jump-table slot (`JP` lands on the trap) and — like MBASIC —
        // CALLing the extracted operand address directly.
        let abort = AtomicBool::new(false);
        // CALL through the table slot: CALL 0xFF0C (BIOS_BASE + 3*4).
        let mut cpm = Cpm::new();
        cpm.load_com(&[0xCD, 0x0C, 0xFF, 0x76 /*HALT-ish filler*/]);
        assert_eq!(cpm.run(100, &abort), Stop::Bios(4));
        // CALL the trap operand directly: CALL 0xFF44 (BIOS_TRAP + 4).
        let mut cpm = Cpm::new();
        cpm.load_com(&[0xCD, 0x44, 0xFF, 0x76]);
        assert_eq!(cpm.run(100, &abort), Stop::Bios(4));
    }

    #[test]
    fn test_bios_conin_returns_value_and_rets() {
        // Constructing a CP/M machine registers a session in the
        // process-global image registry, which makes drives look busy to
        // every other test.  Serialise with the registry tests.
        let _g = crate::cpm::image::registry::tests_lock();
        // CALL CONIN (vector 3), host supplies a byte via bios_return, and
        // the guest stores A to a buffer then warm-boots — proving the
        // BIOS call returns the value in A and RETs to the caller.
        // 0100: CD 09 FF   CALL 0xFF09 (CONIN table slot)
        // 0103: 32 20 01   LD (0x0120),A
        // 0106: C3 00 00   JP 0 (warm boot)
        let prog = [0xCD, 0x09, 0xFF, 0x32, 0x20, 0x01, 0xC3, 0x00, 0x00];
        let mut cpm = Cpm::new();
        cpm.load_com(&prog);
        let abort = AtomicBool::new(false);
        assert_eq!(cpm.run(100, &abort), Stop::Bios(3));
        cpm.bios_return(b'X');
        assert_eq!(cpm.run(100, &abort), Stop::WarmBoot);
        assert_eq!(cpm.mem.peek(0x0120), b'X');
    }

    #[test]
    fn test_bios_conout_arg_in_c() {
        // Constructing a CP/M machine registers a session in the
        // process-global image registry, which makes drives look busy to
        // every other test.  Serialise with the registry tests.
        let _g = crate::cpm::image::registry::tests_lock();
        // CONOUT takes its character in C (arg_c), unlike BDOS 2's E.
        // 0100: 0E 41      LD C,'A'
        // 0102: CD 0C FF   CALL CONOUT
        // 0105: C3 00 00   JP 0
        let prog = [0x0E, b'A', 0xCD, 0x0C, 0xFF, 0xC3, 0x00, 0x00];
        let mut cpm = Cpm::new();
        cpm.load_com(&prog);
        let abort = AtomicBool::new(false);
        assert_eq!(cpm.run(100, &abort), Stop::Bios(4));
        assert_eq!(cpm.arg_c(), b'A');
    }

    #[test]
    fn test_bdos_print_string_then_warm_boot() {
        // LD DE,msg / LD C,9 / CALL 5 / LD C,0 / CALL 5 / msg: "HI!$"
        // Layout from 0x0100:
        //   0100: 11 0D 01     LD DE,0x010D
        //   0103: 0E 09        LD C,9
        //   0105: CD 05 00     CALL 5
        //   0108: 0E 00        LD C,0
        //   010A: CD 05 00     CALL 5
        //   010D: "HI!$"
        let prog = [
            0x11, 0x0D, 0x01, // LD DE,0x010D
            0x0E, 0x09, // LD C,9
            0xCD, 0x05, 0x00, // CALL 5
            0x0E, 0x00, // LD C,0
            0xCD, 0x05, 0x00, // CALL 5
            b'H', b'I', b'!', b'$',
        ];
        let (out, stop) = drive(&prog);
        assert_eq!(out, b"HI!");
        assert_eq!(stop, Stop::WarmBoot);
    }

    #[test]
    fn test_bdos_conout_then_ret_warm_boots() {
        // LD E,'A' / LD C,2 / CALL 5 / RET   (RET -> warm-boot vector 0)
        //   0100: 1E 41        LD E,'A'
        //   0102: 0E 02        LD C,2
        //   0104: CD 05 00     CALL 5
        //   0107: C9           RET
        let prog = [
            0x1E, b'A', // LD E,'A'
            0x0E, 0x02, // LD C,2
            0xCD, 0x05, 0x00, // CALL 5
            0xC9, // RET -> 0x0000 warm boot
        ];
        let (out, stop) = drive(&prog);
        assert_eq!(out, b"A");
        assert_eq!(stop, Stop::WarmBoot);
    }

    #[test]
    fn test_runaway_hits_instruction_budget() {
        // Constructing a CP/M machine registers a session in the
        // process-global image registry, which makes drives look busy to
        // every other test.  Serialise with the registry tests.
        let _g = crate::cpm::image::registry::tests_lock();
        // JP $ (tight infinite loop): 0100: C3 00 01
        let prog = [0xC3, 0x00, 0x01];
        let mut cpm = Cpm::new();
        cpm.load_com(&prog);
        let abort = AtomicBool::new(false);
        assert_eq!(cpm.run(1000, &abort), Stop::BudgetExhausted);
        assert!(cpm.instructions() >= 1000);
    }

    #[test]
    fn test_bdos_read_buffer_writes_cpm_layout() {
        // Constructing a CP/M machine registers a session in the
        // process-global image registry, which makes drives look busy to
        // every other test.  Serialise with the registry tests.
        let _g = crate::cpm::image::registry::tests_lock();
        let mut cpm = Cpm::new();
        let de = 0x0200u16;
        // Caller sets the maximum length in byte 0.
        cpm.mem.poke(de, 8);
        cpm.bdos_read_buffer(de, b"HELLO");
        assert_eq!(cpm.mem.peek(de), 8); // max preserved
        assert_eq!(cpm.mem.peek(de + 1), 5); // count filled in
        let mut got = Vec::new();
        for i in 0..5 {
            got.push(cpm.mem.peek(de + 2 + i));
        }
        assert_eq!(got, b"HELLO");
    }

    #[test]
    fn test_bdos_read_buffer_truncates_to_max() {
        // Constructing a CP/M machine registers a session in the
        // process-global image registry, which makes drives look busy to
        // every other test.  Serialise with the registry tests.
        let _g = crate::cpm::image::registry::tests_lock();
        let mut cpm = Cpm::new();
        let de = 0x0300u16;
        cpm.mem.poke(de, 3); // max 3
        cpm.bdos_read_buffer(de, b"OVERLONG");
        assert_eq!(cpm.mem.peek(de + 1), 3); // truncated count
        let mut got = Vec::new();
        for i in 0..3 {
            got.push(cpm.mem.peek(de + 2 + i));
        }
        assert_eq!(got, b"OVE");
    }

    /// End-to-end: a Z80 program MAKEs a file, WRITEs a record from the
    /// DMA buffer, CLOSEs, re-OPENs, READs the record back into a different
    /// DMA buffer, and prints it — driven through the real BDOS file calls
    /// against a temp `CPM/` drive.  Exercises the FCB↔memory↔host-file glue
    /// (read_block/write_block/store_position/seq_record) the driver relies
    /// on.
    #[test]
    fn test_program_file_io_roundtrip() {
        // Constructing a CP/M machine registers a session in the
        // process-global image registry, which makes drives look busy to
        // every other test.  Serialise with the registry tests.
        let _g = crate::cpm::image::registry::tests_lock();
        let base = std::env::temp_dir().join("xmodem_cpm_prog_io");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("A")).unwrap();
        let mut fs = CpmFs::new(base.clone());
        let mut cpm = Cpm::new();

        // FCB for A:IO.TXT at 0x005C (the CP/M default FCB address).
        let mut fcb = [0u8; FCB_SIZE];
        fcb[1..9].copy_from_slice(b"IO      ");
        fcb[9..12].copy_from_slice(b"TXT");
        cpm.write_block(0x005C, &fcb);
        // Data to write lives at the default DMA (0x0080), '$'-terminated.
        cpm.write_block(0x0080, b"DISK OK!$");

        // Assemble the program.
        let mut p: Vec<u8> = Vec::new();
        let op = |p: &mut Vec<u8>, de: u16, c: u8| {
            p.extend_from_slice(&[0x11, de as u8, (de >> 8) as u8]); // LD DE,de
            p.extend_from_slice(&[0x0E, c]); // LD C,c
            p.extend_from_slice(&[0xCD, 0x05, 0x00]); // CALL 5
        };
        op(&mut p, 0x005C, 22); // make
        op(&mut p, 0x005C, 21); // write (DMA=0x0080)
        op(&mut p, 0x005C, 16); // close
        op(&mut p, 0x005C, 15); // open (resets position)
        op(&mut p, 0x0200, 26); // set DMA = 0x0200
        op(&mut p, 0x005C, 20); // read into 0x0200
        op(&mut p, 0x0200, 9); // print string at 0x0200
        p.extend_from_slice(&[0x0E, 0x00, 0xCD, 0x05, 0x00]); // LD C,0 / CALL 5
        cpm.load_com(&p);
        // load_com zeroed nothing above the program, but our FCB/DMA writes
        // were done after load_com would overwrite 0x0080? No — TPA starts at
        // 0x0100, so 0x005C/0x0080 are untouched by load_com.  Re-assert:
        assert_eq!(cpm.read_block(0x0080, 4), b"DISK");

        let abort = AtomicBool::new(false);
        let mut out = Vec::new();
        while let Stop::Bdos(func) = cpm.run(100_000, &abort) {
            if func == 9 {
                // Console print-string (BDOS 9) is a console-group call the
                // driver would handle; service it inline here to capture it.
                let de = cpm.reg16(Reg16::DE);
                out.extend(cpm.read_dollar_string(de, 4096));
                cpm.bdos_return(0);
            } else if let Some(code) = service_disk_bdos(&mut cpm, &mut fs, func) {
                // Exercise the REAL shared disk-BDOS dispatch (make/write/
                // close/open/read + set-DMA), the same code the driver runs.
                cpm.bdos_return(code);
            } else {
                cpm.bdos_return(0);
            }
        }

        assert_eq!(out, b"DISK OK!");
        // The file really exists on disk with our bytes.
        let disk = std::fs::read(base.join("A").join("IO.TXT")).unwrap();
        assert_eq!(&disk[..8], b"DISK OK!");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_bdos_login_vector_lists_all_drives() {
        // Constructing a CP/M machine registers a session in the
        // process-global image registry, which makes drives look busy to
        // every other test.  Serialise with the registry tests.
        let _g = crate::cpm::image::registry::tests_lock();
        let base = std::env::temp_dir().join("xmodem_cpm_login");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("A")).unwrap();
        let fs = CpmFs::new(base.clone());
        let mut cpm = Cpm::new();
        // BDOS 24: HL bitmap with all sixteen drives A:–P: active.
        assert_eq!(disk_info_bdos(&mut cpm, &fs, 24), Some(0xFFFF));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// BDOS 13 (Reset Disk System) through the dispatch a guest actually
    /// reaches: it resets drive/DMA/write-protect **and** returns 0FFH when the
    /// drive it logs in holds a temporary `$`-prefixed file, else 0.
    ///
    /// That return value is load-bearing, which is why it gets a test rather
    /// than being left at the flat `0` it used to be: `CCP22.ASM` calls this
    /// function and stores A straight into its submit flag, so a real CCP
    /// running here would never notice a `SUBMIT` batch already in progress.
    #[test]
    fn test_bdos_reset_reports_temp_file_and_resets_state() {
        // Constructing a CP/M machine registers a session in the
        // process-global image registry, which makes drives look busy to
        // every other test.  Serialise with the registry tests.
        let _g = crate::cpm::image::registry::tests_lock();
        let base = std::env::temp_dir().join("xmodem_cpm_reset13");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("A")).unwrap();
        std::fs::create_dir_all(base.join("B")).unwrap();
        let mut fs = CpmFs::new(base.clone());
        let mut cpm = Cpm::new();

        // Nothing temporary on A: → 0.
        std::fs::write(base.join("A").join("PIP.COM"), b"x").unwrap();
        assert_eq!(service_disk_bdos(&mut cpm, &mut fs, 13), Some(0));

        // The reset half still happens: drive back to A:, default DMA, and any
        // BDOS 28 write-protect released.
        fs.select(1);
        fs.set_drive_ro();
        fs.set_dma(0x1234);
        assert_eq!(service_disk_bdos(&mut cpm, &mut fs, 13), Some(0));
        assert_eq!(fs.current_drive(), 0, "reset selects A:");
        assert_eq!(fs.dma(), fs::DEFAULT_DMA, "reset restores the default DMA");
        assert_eq!(fs.ro_vector(), 0, "reset releases every write-protect");

        // A submit file on A: → 0FFH, which is how a CCP detects a batch.
        std::fs::write(base.join("A").join("$$$.SUB"), b"x").unwrap();
        assert_eq!(
            service_disk_bdos(&mut cpm, &mut fs, 13),
            Some(0xFF),
            "a $-prefixed file on the logged-in drive must set the flag"
        );

        // Reset logs in A:, so a temp file sitting on B: must NOT set it —
        // even if B: was current when the call was made.
        std::fs::remove_file(base.join("A").join("$$$.SUB")).unwrap();
        std::fs::write(base.join("B").join("$$$.SUB"), b"x").unwrap();
        fs.select(1);
        assert_eq!(
            service_disk_bdos(&mut cpm, &mut fs, 13),
            Some(0),
            "the flag describes A:, the drive reset logs in — not B:"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// BDOS 16 (Close File): 0 when the file is there, **0FFH when it is not**,
    /// and 0 without searching in the two cases the real `CLOSEF` short-circuits.
    ///
    /// Writes here are write-through and there is no directory to rewrite, so
    /// close has no work to do — but it used to return a flat 0, which reported
    /// success for a file that does not exist. `BDOS22.ASM` has an explicit
    /// error exit for that ("MEANING THAT FILE CANNOT BE CLOSED"), and the
    /// documented contract is 255 when the name is not in the directory.
    #[test]
    fn test_bdos_close_reports_a_missing_file() {
        // Constructing a CP/M machine registers a session in the
        // process-global image registry, which makes drives look busy to
        // every other test.  Serialise with the registry tests.
        let _g = crate::cpm::image::registry::tests_lock();
        let base = std::env::temp_dir()
            .join(format!("xmodem_cpm_close_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("A")).unwrap();
        std::fs::write(base.join("A").join("NOTE.TXT"), b"body").unwrap();

        let mut fs = CpmFs::new(base.clone());
        let mut cpm = Cpm::new();
        cpm.load_com(&[0xC9]);

        // FCB naming an existing file, on A:.
        let fcb_at = 0x0100u16;
        let mut raw = [b' '; FCB_SIZE];
        raw[0] = 1; // A:
        raw[1..5].copy_from_slice(b"NOTE");
        raw[9..12].copy_from_slice(b"TXT");
        raw[12..].fill(0);
        cpm.write_block(fcb_at, &raw);
        cpm.set_reg16(Reg16::DE, fcb_at);
        assert_eq!(
            service_disk_bdos(&mut cpm, &mut fs, 16),
            Some(0x00),
            "closing a file that exists succeeds"
        );

        // Same FCB after the file is gone → the error the flat 0 used to hide.
        std::fs::remove_file(base.join("A").join("NOTE.TXT")).unwrap();
        cpm.write_block(fcb_at, &raw);
        cpm.set_reg16(Reg16::DE, fcb_at);
        assert_eq!(
            service_disk_bdos(&mut cpm, &mut fs, 16),
            Some(0xFF),
            "a name not in the directory must report 0FFH, not success"
        );

        // FCB byte 14 (S2) high bit = "no directory update needed": success
        // without looking, so it stays 0 even with the file still missing.
        let mut marked = raw;
        marked[14] = 0x80;
        cpm.write_block(fcb_at, &marked);
        cpm.set_reg16(Reg16::DE, fcb_at);
        assert_eq!(
            service_disk_bdos(&mut cpm, &mut fs, 16),
            Some(0x00),
            "S2 bit 7 short-circuits to success, as CLOSEF's `ANI 80H / RNZ` does"
        );

        // A software write-protected drive is not a close *error* either —
        // `CALL GETRO / RNZ` returns with the parameter still 0.
        //
        // The file is left MISSING on purpose: with it present both the
        // short-circuit and the existence check return 0, so the assertion
        // could not tell them apart and the R/O rule was untested. (Mutation
        // testing caught exactly that.) Missing, the two disagree — 0 only if
        // the write-protect check short-circuits.
        fs.select(0);
        fs.set_drive_ro();
        cpm.write_block(fcb_at, &raw);
        cpm.set_reg16(Reg16::DE, fcb_at);
        assert_eq!(
            service_disk_bdos(&mut cpm, &mut fs, 16),
            Some(0x00),
            "closing on a write-protected drive succeeds; there is just nothing to write back"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_bdos_user_number_get_set() {
        // Constructing a CP/M machine registers a session in the
        // process-global image registry, which makes drives look busy to
        // every other test.  Serialise with the registry tests.
        let _g = crate::cpm::image::registry::tests_lock();
        let base = std::env::temp_dir().join("xmodem_cpm_user");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("A")).unwrap();
        let mut fs = CpmFs::new(base.clone());
        let mut cpm = Cpm::new();
        // Set user 3 (E=3), then get (E=0xFF) returns 3.
        cpm.set_reg8(Reg8::E, 3);
        assert_eq!(service_disk_bdos(&mut cpm, &mut fs, 32), Some(3));
        assert_eq!(fs.current_user(), 3);
        cpm.set_reg8(Reg8::E, 0xFF);
        assert_eq!(service_disk_bdos(&mut cpm, &mut fs, 32), Some(3));
        // Values clamp to 0–15.
        cpm.set_reg8(Reg8::E, 20);
        assert_eq!(service_disk_bdos(&mut cpm, &mut fs, 32), Some(4)); // 20 & 0x0F
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_bdos_iobyte_get_set_roundtrip() {
        // Constructing a CP/M machine registers a session in the
        // process-global image registry, which makes drives look busy to
        // every other test.  Serialise with the registry tests.
        let _g = crate::cpm::image::registry::tests_lock();
        // BDOS 8 (Set I/O Byte) stores E at the IOBYTE address; BDOS 7 (Get
        // I/O Byte) reads it back — a set-then-get round-trip is now
        // self-consistent instead of get→0 / set→dropped.
        let base = std::env::temp_dir().join("xmodem_cpm_iobyte");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("A")).unwrap();
        let mut fs = CpmFs::new(base.clone());
        let mut cpm = Cpm::new();
        cpm.load_com(&[0xC9]); // lays down page zero
        // Default IOBYTE reads as 0.
        assert_eq!(service_disk_bdos(&mut cpm, &mut fs, 7), Some(0x00));
        // Set IOBYTE = 0x95 (a typical CON:/RDR:/PUN:/LST: assignment) via E.
        cpm.set_reg8(Reg8::E, 0x95);
        assert_eq!(service_disk_bdos(&mut cpm, &mut fs, 8), Some(0));
        // Get reads the same value back, and it physically lives at 0x0003.
        assert_eq!(service_disk_bdos(&mut cpm, &mut fs, 7), Some(0x95));
        assert_eq!(cpm.read_block(IOBYTE_ADDR, 1)[0], 0x95);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// BDOS 28 (Write Protect Disk) ↔ 29 (Get R/O Vector) ↔ 13/37 (release).
    /// 28 and 29 must agree: 29 reported a hardcoded 0 for as long as nothing
    /// could become R/O, so wiring 28 without 29 would have left a program
    /// unable to see the protection it had just asked for.
    #[test]
    fn test_bdos_write_protect_and_ro_vector() {
        // Constructing a CP/M machine registers a session in the
        // process-global image registry, which makes drives look busy to
        // every other test.  Serialise with the registry tests.
        let _g = crate::cpm::image::registry::tests_lock();
        let base = std::env::temp_dir().join("xmodem_cpm_wp");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("A")).unwrap();
        std::fs::create_dir_all(base.join("B")).unwrap();
        let mut fs = CpmFs::new(base.clone());
        let mut cpm = Cpm::new();
        cpm.load_com(&[0xC9]);

        // Nothing protected initially.
        assert_eq!(disk_info_bdos(&mut cpm, &fs, 29), Some(0x0000));

        // Protect A: (the current drive).
        assert_eq!(service_disk_bdos(&mut cpm, &mut fs, 28), Some(0));
        assert_eq!(disk_info_bdos(&mut cpm, &fs, 29), Some(0x0001));

        // Select B: and protect that too — the vector carries both bits.
        cpm.set_reg8(Reg8::E, 1);
        assert_eq!(service_disk_bdos(&mut cpm, &mut fs, 14), Some(0));
        assert_eq!(service_disk_bdos(&mut cpm, &mut fs, 28), Some(0));
        assert_eq!(disk_info_bdos(&mut cpm, &fs, 29), Some(0x0003));

        // BDOS 37 (Reset Drive) with only A:'s bit releases A: alone.
        cpm.set_reg16(Reg16::DE, 0x0001);
        assert_eq!(service_disk_bdos(&mut cpm, &mut fs, 37), Some(0));
        assert_eq!(disk_info_bdos(&mut cpm, &fs, 29), Some(0x0002));

        // BDOS 13 (Reset Disk System) releases everything.
        assert_eq!(service_disk_bdos(&mut cpm, &mut fs, 13), Some(0));
        assert_eq!(disk_info_bdos(&mut cpm, &fs, 29), Some(0x0000));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// BDOS 30 (Set File Attributes): the R/O bit rides the *high* bit of the
    /// first extension byte, which `Fcb::from_bytes` strips — so reading it
    /// from the raw FCB (as BDOS 23 does for its second-half name) is the whole
    /// trick.  System/Archive are accepted and ignored, not faked.
    #[test]
    fn test_bdos_set_file_attributes_ro_bit() {
        // Constructing a CP/M machine registers a session in the
        // process-global image registry, which makes drives look busy to
        // every other test.  Serialise with the registry tests.
        let _g = crate::cpm::image::registry::tests_lock();
        let base = std::env::temp_dir().join("xmodem_cpm_attr");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("A")).unwrap();
        let path = base.join("A").join("NOTE.TXT");
        std::fs::write(&path, b"body").unwrap();

        let mut fs = CpmFs::new(base.clone());
        let mut cpm = Cpm::new();
        cpm.load_com(&[0xC9]);

        // Build an FCB at 0x0100 naming NOTE.TXT with t1' set on the ext.
        let fcb_at = 0x0100u16;
        let mut raw = [b' '; FCB_SIZE];
        raw[0] = 1; // A:
        raw[1..5].copy_from_slice(b"NOTE");
        raw[9..12].copy_from_slice(b"TXT");
        raw[12..].fill(0);
        raw[9] |= 0x80; // t1' = R/O
        cpm.write_block(fcb_at, &raw);
        cpm.set_reg16(Reg16::DE, fcb_at);

        assert_eq!(service_disk_bdos(&mut cpm, &mut fs, 30), Some(0x00));
        assert!(CpmFs::host_is_ro(&path), "BDOS 30 must set the host R/O bit");

        // …and it is genuinely enforced, not merely recorded.
        let fcb = Fcb::from_bytes(&raw);
        assert_eq!(fs.delete(&fcb), 0, "the now-R/O file must resist erase");
        assert!(path.is_file());

        // Clearing t1' makes it writable again.
        raw[9] &= 0x7F;
        cpm.write_block(fcb_at, &raw);
        assert_eq!(service_disk_bdos(&mut cpm, &mut fs, 30), Some(0x00));
        assert!(!CpmFs::host_is_ro(&path));

        // A missing file is 0xFF, as CP/M reports.
        let mut gone = raw;
        gone[1..5].copy_from_slice(b"GONE");
        cpm.write_block(fcb_at, &gone);
        assert_eq!(service_disk_bdos(&mut cpm, &mut fs, 30), Some(0xFF));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A write-protected drive must refuse the write path with an error code
    /// the guest can act on, not the fake success an unhandled function gets.
    #[test]
    fn test_bdos_write_to_protected_drive_reports_error() {
        // Constructing a CP/M machine registers a session in the
        // process-global image registry, which makes drives look busy to
        // every other test.  Serialise with the registry tests.
        let _g = crate::cpm::image::registry::tests_lock();
        let base = std::env::temp_dir().join("xmodem_cpm_wpwrite");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("A")).unwrap();
        std::fs::write(base.join("A").join("OUT.DAT"), vec![0u8; 128]).unwrap();

        let mut fs = CpmFs::new(base.clone());
        let mut cpm = Cpm::new();
        cpm.load_com(&[0xC9]);

        let fcb_at = 0x0100u16;
        let mut raw = [b' '; FCB_SIZE];
        raw[0] = 1;
        raw[1..4].copy_from_slice(b"OUT");
        raw[9..12].copy_from_slice(b"DAT");
        raw[12..].fill(0);
        cpm.write_block(fcb_at, &raw);
        cpm.set_reg16(Reg16::DE, fcb_at);

        assert_eq!(service_disk_bdos(&mut cpm, &mut fs, 28), Some(0));
        // 21 = write sequential → 0x01 (generic write error).
        assert_eq!(service_disk_bdos(&mut cpm, &mut fs, 21), Some(0x01));
        // 34 = write random → 0x05 (write / directory-overflow error).
        cpm.write_block(fcb_at, &raw);
        assert_eq!(service_disk_bdos(&mut cpm, &mut fs, 34), Some(0x05));
        // 22 = make → 0xFF; the drive is protected, so no new file appears.
        let mut newf = raw;
        newf[1..4].copy_from_slice(b"NEW");
        cpm.write_block(fcb_at, &newf);
        assert_eq!(service_disk_bdos(&mut cpm, &mut fs, 22), Some(0xFF));
        assert!(!base.join("A").join("NEW.DAT").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_cdisk_byte_tracks_drive_and_user() {
        // Constructing a CP/M machine registers a session in the
        // process-global image registry, which makes drives look busy to
        // every other test.  Serialise with the registry tests.
        let _g = crate::cpm::image::registry::tests_lock();
        // Page-zero CDISK (0x0004): low nibble = drive, high nibble = user.
        // A program that reads it directly (Infocom's interpreter, to find
        // its story file) must see the real login drive — the bug that hung
        // witness.com on B:.
        let base = std::env::temp_dir().join("xmodem_cpm_cdisk");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("A")).unwrap();
        std::fs::create_dir_all(base.join("B")).unwrap();
        let mut fs = CpmFs::new(base.clone());
        let mut cpm = Cpm::new();
        cpm.load_com(&[0xC9]); // lays down page zero

        // Launched on B: (drive 1), user 0 → 0x0004 == 0x01.
        fs.select(1);
        cpm.set_current_disk(fs.current_drive(), fs.current_user());
        assert_eq!(cpm.read_block(CDISK_ADDR, 1)[0], 0x01);

        // A guest BDOS 14 select of A: pulls CDISK back to 0x00.
        cpm.set_reg8(Reg8::E, 0);
        assert_eq!(service_disk_bdos(&mut cpm, &mut fs, 14), Some(0));
        assert_eq!(cpm.read_block(CDISK_ADDR, 1)[0], 0x00);

        // Select C: (drive 2) with user 5 set → high nibble 5, low nibble 2.
        cpm.set_reg8(Reg8::E, 5);
        service_disk_bdos(&mut cpm, &mut fs, 32); // set user 5
        cpm.set_reg8(Reg8::E, 2);
        service_disk_bdos(&mut cpm, &mut fs, 14); // select C:
        assert_eq!(cpm.read_block(CDISK_ADDR, 1)[0], 0x52);

        // BDOS 13 (reset disk system) returns to A:, preserving the user.
        service_disk_bdos(&mut cpm, &mut fs, 13);
        assert_eq!(cpm.read_block(CDISK_ADDR, 1)[0], 0x50);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_bdos_write_random_zero_fill_persists() {
        // Constructing a CP/M machine registers a session in the
        // process-global image registry, which makes drives look busy to
        // every other test.  Serialise with the registry tests.
        let _g = crate::cpm::image::registry::tests_lock();
        let base = std::env::temp_dir().join("xmodem_cpm_wr40");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("A")).unwrap();
        let mut fs = CpmFs::new(base.clone());
        let mut cpm = Cpm::new();

        // Make A:REC.DAT, FCB at 0x005C.
        let mut fcb = [0u8; FCB_SIZE];
        fcb[1..9].copy_from_slice(b"REC     ");
        fcb[9..12].copy_from_slice(b"DAT");
        cpm.write_block(0x005C, &fcb);
        cpm.set_reg16(Reg16::DE, 0x005C);
        assert_eq!(service_disk_bdos(&mut cpm, &mut fs, 22), Some(0)); // make

        // Put a payload at the DMA (0x0080) and write it as random record 0
        // via function 40 (write random, zero fill) — must persist, not drop.
        cpm.write_block(0x0080, b"FORTY-OK");
        let mut fcb = cpm.read_block(0x005C, FCB_SIZE);
        fcb[33] = 0; // r0
        fcb[34] = 0; // r1
        fcb[35] = 0; // r2
        cpm.write_block(0x005C, &fcb);
        cpm.set_reg16(Reg16::DE, 0x005C);
        assert_eq!(service_disk_bdos(&mut cpm, &mut fs, 40), Some(0));

        // The bytes reached the host file (func 40 was NOT a silent no-op).
        let disk = std::fs::read(base.join("A").join("REC.DAT")).unwrap();
        assert_eq!(&disk[..8], b"FORTY-OK");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_disk_info_bdos_dpb_and_free_space() {
        // Constructing a CP/M machine registers a session in the
        // process-global image registry, which makes drives look busy to
        // every other test.  Serialise with the registry tests.
        let _g = crate::cpm::image::registry::tests_lock();
        let base = std::env::temp_dir().join("xmodem_cpm_diskinfo");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("A")).unwrap();
        let fs = CpmFs::new(base.clone());
        let mut cpm = Cpm::new();

        // BDOS 31 (Get DPB): address returned, DPB fields match the geometry.
        let dpb_addr = disk_info_bdos(&mut cpm, &fs, 31).unwrap();
        let d = cpm.read_block(dpb_addr, 15);
        assert_eq!(d[2], VD_BSH);
        assert_eq!(d[3], VD_BLM);
        assert_eq!(u16::from_le_bytes([d[5], d[6]]), VD_DSM);
        assert_eq!(u16::from_le_bytes([d[7], d[8]]), VD_DRM);
        assert_eq!(d[9], VD_AL0);

        // Count free (zero) bits in the allocation vector.
        let free_blocks = |cpm: &mut Cpm| -> u64 {
            let addr = disk_info_bdos(cpm, &fs, 27).unwrap();
            let nbytes = (VD_DSM as usize / 8) + 1;
            let v = cpm.read_block(addr, nbytes);
            v.iter().map(|b| b.count_zeros() as u64).sum()
        };

        // Empty drive: only the directory's 8 reserved blocks are used.
        let total = VD_DSM as u64 + 1;
        assert_eq!(free_blocks(&mut cpm), total - VD_DIR_BLOCKS);

        // A ~5000-byte file occupies 2 of the 4096-byte blocks; free drops by 2.
        std::fs::write(base.join("A").join("BIG.DAT"), vec![0u8; 5000]).unwrap();
        assert_eq!(free_blocks(&mut cpm), total - VD_DIR_BLOCKS - 2);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_scratch_layout_clear_of_bios_table() {
        // Static invariant: the DPB and the 256-byte allocation vector must
        // not overlap the BIOS jump table or its trap range, and must fit in
        // memory.  An earlier ALLOC_ADDR (0xFE90) overran into the table at
        // 0xFF00; this guards against a silent regression if the geometry
        // (VD_DSM) or the table location ever changes.
        let alloc = ALLOC_ADDR as usize;
        let alloc_len = (VD_DSM as usize / 8) + 1;
        let dpb = DPB_ADDR as usize;
        let dpb_len = 15;
        let table = BIOS_BASE as usize;
        let table_end = BIOS_TRAP as usize + BIOS_VECTORS as usize; // through the trap range
        // Everything stays inside the 64 KB space.
        assert!(alloc + alloc_len <= 0x1_0000);
        assert!(dpb + dpb_len <= 0x1_0000);
        assert!(table_end <= 0x1_0000);
        // The alloc vector clears the BIOS table+traps entirely.
        assert!(
            alloc + alloc_len <= table || alloc >= table_end,
            "alloc vector [{alloc:#06x},{:#06x}) overlaps BIOS region [{table:#06x},{table_end:#06x})",
            alloc + alloc_len
        );
        // The DPB clears the BIOS table+traps, the alloc vector, and the stack.
        assert!(
            dpb + dpb_len <= table || dpb >= table_end,
            "DPB overlaps BIOS region"
        );
        assert!(
            dpb + dpb_len <= alloc || dpb >= alloc + alloc_len,
            "DPB overlaps alloc vector"
        );
    }

    #[test]
    fn test_get_alloc_preserves_bios_table() {
        // Constructing a CP/M machine registers a session in the
        // process-global image registry, which makes drives look busy to
        // every other test.  Serialise with the registry tests.
        let _g = crate::cpm::image::registry::tests_lock();
        // Regression: servicing BDOS 27 (Get-Alloc) must not corrupt the BIOS
        // jump table.  Before the scratch relocation, the 256-byte alloc
        // vector written at 0xFE90 ran through the table at 0xFF00, zeroing
        // the `JP <trap>` vectors that direct-console software (MBASIC,
        // WordStar, Infocom) walks — so a program that called Get-Alloc and
        // then reached console I/O via the table jumped into garbage.
        let base = std::env::temp_dir().join("xmodem_cpm_allocguard");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("A")).unwrap();
        let fs = CpmFs::new(base.clone());
        let mut cpm = Cpm::new();
        cpm.load_com(&[0xC9]); // installs the low memory + BIOS table
        // Snapshot the freshly-installed table, service Get-Alloc, compare.
        for i in 0..BIOS_VECTORS {
            let slot = BIOS_BASE + 3 * i;
            assert_eq!(cpm.mem.peek(slot), 0xC3);
            assert_eq!(cpm.mem.peek16(slot + 1), BIOS_TRAP + i);
        }
        let _ = disk_info_bdos(&mut cpm, &fs, 27).unwrap();
        for i in 0..BIOS_VECTORS {
            let slot = BIOS_BASE + 3 * i;
            assert_eq!(cpm.mem.peek(slot), 0xC3, "vector {i} JP clobbered by Get-Alloc");
            assert_eq!(cpm.mem.peek16(slot + 1), BIOS_TRAP + i, "vector {i} operand clobbered");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_setup_command_line_page_zero() {
        // Constructing a CP/M machine registers a session in the
        // process-global image registry, which makes drives look busy to
        // every other test.  Serialise with the registry tests.
        let _g = crate::cpm::image::registry::tests_lock();
        let mut cpm = Cpm::new();
        cpm.load_com(&[0xC9]); // RET
        cpm.setup_command_line("B:FOO.TXT BAR.DAT");

        // Command tail at 0x0080: length byte + the (leading-space) tail.
        let n = cpm.read_block(0x0080, 1)[0] as usize;
        assert_eq!(&cpm.read_block(0x0081, n), b" B:FOO.TXT BAR.DAT");
        assert_eq!(cpm.read_block(0x0081 + n as u16, 1)[0], 0); // NUL terminator

        // Default FCB1 at 0x005C: drive B: (2), FOO.TXT, fields zeroed.
        let f1 = cpm.read_block(0x005C, 16);
        assert_eq!(f1[0], 2);
        assert_eq!(&f1[1..9], b"FOO     ");
        assert_eq!(&f1[9..12], b"TXT");
        assert_eq!(&f1[12..16], &[0, 0, 0, 0]); // ex,s1,s2,rc

        // Default FCB2 at 0x006C: default drive (0), BAR.DAT.
        let f2 = cpm.read_block(0x006C, 12);
        assert_eq!(f2[0], 0);
        assert_eq!(&f2[1..9], b"BAR     ");
        assert_eq!(&f2[9..12], b"DAT");
    }

    #[test]
    fn test_setup_command_line_empty_tail() {
        // Constructing a CP/M machine registers a session in the
        // process-global image registry, which makes drives look busy to
        // every other test.  Serialise with the registry tests.
        let _g = crate::cpm::image::registry::tests_lock();
        let mut cpm = Cpm::new();
        cpm.load_com(&[0xC9]);
        cpm.setup_command_line("");
        assert_eq!(cpm.read_block(0x0080, 1)[0], 0); // zero-length tail
        // FCB1 carries a blank name on the default drive.
        let f1 = cpm.read_block(0x005C, 12);
        assert_eq!(f1[0], 0);
        assert_eq!(&f1[1..9], b"        ");
        assert_eq!(&f1[9..12], b"   ");
    }

    /// End-to-end B4a: write a `.COM` onto a temp drive through the real FS
    /// API, read it back with `read_whole_file`, load it into the TPA with
    /// `setup_command_line`, and run it — proving a real program image is
    /// loaded from a drive and executed.  The program prints a banner via
    /// BDOS 9, the same path the CCP-lite driver uses.
    #[test]
    fn test_run_com_loaded_from_drive() {
        // Constructing a CP/M machine registers a session in the
        // process-global image registry, which makes drives look busy to
        // every other test.  Serialise with the registry tests.
        let _g = crate::cpm::image::registry::tests_lock();
        let base = std::env::temp_dir().join("xmodem_cpm_run_from_drive");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("A")).unwrap();
        let fs = CpmFs::new(base.clone());

        // A tiny HELLO.COM: LD DE,msg / LD C,9 / CALL 5 / RET ; msg "OK!$".
        let prog: [u8; 13] = [
            0x11, 0x09, 0x01, // LD DE,0x0109
            0x0E, 0x09, // LD C,9
            0xCD, 0x05, 0x00, // CALL 5
            0xC9, // RET -> warm boot
            b'O', b'K', b'!', b'$',
        ];

        // Write it to A:HELLO.COM via the real make + write_record path.
        let mut raw = [0u8; FCB_SIZE];
        raw[1..9].copy_from_slice(b"HELLO   ");
        raw[9..12].copy_from_slice(b"COM");
        let fcb = Fcb::from_bytes(&raw);
        assert!(fs.make(&fcb));
        let mut rec = [0u8; 128];
        rec[..prog.len()].copy_from_slice(&prog);
        fs.write_record(&fcb, 0, &rec).unwrap();

        // Load it back the way the driver does and run it.
        let bytes = fs.read_whole_file(&fcb).unwrap().expect("HELLO.COM exists");
        assert_eq!(&bytes[..prog.len()], &prog);
        let mut cpm = Cpm::new();
        cpm.load_com(&bytes);
        cpm.setup_command_line("");
        let abort = AtomicBool::new(false);
        let mut out = Vec::new();
        loop {
            match cpm.run(100_000, &abort) {
                Stop::Bdos(9) => {
                    let de = cpm.reg16(Reg16::DE);
                    out.extend(cpm.read_dollar_string(de, 4096));
                    cpm.bdos_return(0);
                }
                Stop::Bdos(_) => cpm.bdos_return(0),
                Stop::WarmBoot => break,
                other => panic!("unexpected stop {other:?}"),
            }
        }
        assert_eq!(out, b"OK!");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_load_com_reinstalls_low_vectors() {
        // Constructing a CP/M machine registers a session in the
        // process-global image registry, which makes drives look busy to
        // every other test.  Serialise with the registry tests.
        let _g = crate::cpm::image::registry::tests_lock();
        // A program can trash page zero; the next load_com must restore the
        // warm-boot (0x0000) and BDOS (0x0005) JP vectors so the following
        // program's CALL 5 / warm boot still behave — mirrors CP/M reloading
        // the system on a warm boot.
        let mut cpm = Cpm::new();
        cpm.write_block(0x0000, &[0xFF; 8]); // clobber both vectors
        cpm.load_com(&[0xC9]); // RET
        assert_eq!(cpm.read_block(0x0000, 1)[0], 0xC3); // JP restored
        assert_eq!(cpm.read_block(0x0005, 1)[0], 0xC3);
    }

    #[test]
    fn test_tpa_persists_across_loads() {
        // Constructing a CP/M machine registers a session in the
        // process-global image registry, which makes drives look busy to
        // every other test.  Serialise with the registry tests.
        let _g = crate::cpm::image::registry::tests_lock();
        // The machine persists across program runs: memory a prior program
        // left in the TPA (above the next program) survives the next
        // load_com. This is what lets SAVE dump a previous program's image.
        let mut cpm = Cpm::new();
        cpm.write_block(0x0100, &[0xC9]); // a tiny "program"
        cpm.write_block(0x4000, &[0x42, 0x43, 0x44]); // marker left in the TPA
        cpm.load_com(&[0xC9]); // load a new program at 0x0100
        // The marker well above the loaded region is untouched.
        assert_eq!(cpm.read_block(0x4000, 3), &[0x42, 0x43, 0x44]);
    }

    #[test]
    fn test_abort_flag_stops_the_loop() {
        // Constructing a CP/M machine registers a session in the
        // process-global image registry, which makes drives look busy to
        // every other test.  Serialise with the registry tests.
        let _g = crate::cpm::image::registry::tests_lock();
        // Same tight loop, but the abort flag is already set: no progress.
        let prog = [0xC3, 0x00, 0x01];
        let mut cpm = Cpm::new();
        cpm.load_com(&prog);
        let abort = AtomicBool::new(true);
        assert_eq!(cpm.run(1_000_000, &abort), Stop::Aborted);
        assert_eq!(cpm.instructions(), 0);
    }
}
