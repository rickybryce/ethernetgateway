//! CP/M 2.2 emulator core (Flavor B) — a real Z80 CPU (the BSD-licensed
//! `iz80` crate) driven by our own CP/M 2.2 BDOS/BIOS, sandboxed to a
//! `CPM/` directory under `transfer_dir`.
//!
//! This is a **completely separate** feature from the Gateway Shell
//! (`telnet/kernel.rs`, "Flavor A", a pure-Rust CP/M-*flavored* file
//! manager with no CPU emulation).  See `kernelplan.md` §13 for the full
//! design and the phased plan (B0 scaffold → B1 CPU/console → B2 CCP-lite
//! → B3 filesystem → B4 run `.COM` → B5 harden).
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

mod machine;

pub use machine::CpmMachine;

use iz80::{Cpu, Machine, Reg8, Reg16};
use std::sync::atomic::{AtomicBool, Ordering};

/// BDOS entry point — programs `CALL 5`.
pub const BDOS_ENTRY: u16 = 0x0005;
/// Warm-boot vector — programs `JP 0` (or `RET` to it) to reboot.
pub const WBOOT: u16 = 0x0000;
/// Transient Program Area base — where a `.COM` is loaded and starts.
pub const TPA_BASE: u16 = 0x0100;
/// Top of the usable TPA in our layout; the stack starts here and grows
/// down, leaving the region above for the (pretend, for now) BDOS/BIOS.
const STACK_TOP: u16 = 0xFE00;

/// Why a [`Cpm::run`] batch returned control to the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// The guest reached the BDOS entry with this function number in `C`.
    /// The host services it (reading further arguments from the registers
    /// / memory) and then calls [`Cpm::bdos_return`].
    Bdos(u8),
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

/// The emulated CP/M machine: a Z80 CPU plus its 64 KB address space.
pub struct Cpm {
    cpu: Cpu,
    mem: CpmMachine,
    /// Total instructions executed since the last load — used both for the
    /// warm-boot gate (ignore the initial `PC == 0`) and diagnostics.
    instructions: u64,
}

impl Default for Cpm {
    fn default() -> Self {
        Self::new()
    }
}

impl Cpm {
    /// A fresh Z80 machine with the CP/M low-memory vectors installed.
    pub fn new() -> Cpm {
        let mut cpm = Cpm {
            cpu: Cpu::new(), // Z80
            mem: CpmMachine::new(),
            instructions: 0,
        };
        // Warm-boot vector at 0x0000 and BDOS entry at 0x0005 as real CP/M
        // lays them out (JP <addr>), so a guest that inspects address 6 to
        // find the top of the TPA sees a sane value.  We intercept both by
        // program counter, so the jump targets themselves are never run.
        cpm.mem.poke(0x0000, 0xC3); // JP WBOOT handler
        cpm.mem.poke16(0x0001, STACK_TOP);
        cpm.mem.poke(0x0005, 0xC3); // JP BDOS
        cpm.mem.poke16(0x0006, STACK_TOP);
        cpm
    }

    /// Load a `.COM` image into the TPA and prepare to run it: the stack is
    /// placed just below the reserved system area with the warm-boot
    /// address pushed, so a program that ends in `RET` reboots cleanly, and
    /// the PC is set to the TPA base.  Bytes past the usable TPA are
    /// silently dropped (a `.COM` never legitimately exceeds it).
    pub fn load_com(&mut self, program: &[u8]) {
        let max = (STACK_TOP - TPA_BASE) as usize;
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
            if pc == BDOS_ENTRY {
                let func = self.cpu.registers().get8(Reg8::C);
                if func == 0 {
                    return Stop::WarmBoot; // BDOS 0 = system reset
                }
                return Stop::Bdos(func);
            }
            if pc == WBOOT && self.instructions > 0 {
                return Stop::WarmBoot;
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

    /// Read an 8-bit register (for the host to fetch BDOS arguments).
    pub fn reg8(&mut self, r: Reg8) -> u8 {
        self.cpu.registers().get8(r)
    }

    /// Read a 16-bit register (e.g. `DE` for BDOS 9's string pointer).
    pub fn reg16(&mut self, rr: Reg16) -> u16 {
        self.cpu.registers().get16(rr)
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
    pub fn bdos_read_buffer(&mut self, de: u16, line: &[u8]) {
        let max = self.mem.peek(de) as usize;
        let n = line.len().min(max);
        self.mem.poke(de.wrapping_add(1), n as u8);
        for (i, b) in line.iter().take(n).enumerate() {
            self.mem.poke(de.wrapping_add(2).wrapping_add(i as u16), *b);
        }
    }

    /// Total instructions executed since the last `load_com` (diagnostics).
    pub fn instructions(&self) -> u64 {
        self.instructions
    }
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn test_abort_flag_stops_the_loop() {
        // Same tight loop, but the abort flag is already set: no progress.
        let prog = [0xC3, 0x00, 0x01];
        let mut cpm = Cpm::new();
        cpm.load_com(&prog);
        let abort = AtomicBool::new(true);
        assert_eq!(cpm.run(1_000_000, &abort), Stop::Aborted);
        assert_eq!(cpm.instructions(), 0);
    }
}
