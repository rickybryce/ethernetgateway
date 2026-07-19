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

mod fcb;
mod fs;
mod machine;

pub use fcb::{parse_afn, parse_command_fcb, split_8_3, Fcb, FCB_SIZE};
pub use fs::CpmFs;
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
        self.mem.poke(0x0000, 0xC3); // JP WBOOT handler
        self.mem.poke16(0x0001, STACK_TOP);
        self.mem.poke(0x0005, 0xC3); // JP BDOS
        self.mem.poke16(0x0006, STACK_TOP);
    }

    /// Load a `.COM` image into the TPA and prepare to run it: the stack is
    /// placed just below the reserved system area with the warm-boot
    /// address pushed, so a program that ends in `RET` reboots cleanly, and
    /// the PC is set to the TPA base.  Bytes past the usable TPA are
    /// silently dropped (a `.COM` never legitimately exceeds it).
    pub fn load_com(&mut self, program: &[u8]) {
        self.install_low_memory();
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
        13 => {
            // Reset disk system: default drive A:, DMA 0x0080.
            fs.select(0);
            fs.set_dma(fs::DEFAULT_DMA);
            Some(0)
        }
        14 => {
            // Select disk: E = drive (0 = A:).
            let e = cpm.reg8(Reg8::E);
            fs.select(e);
            Some(0)
        }
        15 => Some(with_fcb(cpm, |_cpm, fcb| {
            if fs.open_existing(fcb).is_some() {
                fcb.ex = 0;
                fcb.s2 = 0;
                fcb.cr = 0;
                fcb.rc = 0;
                0x00
            } else {
                0xFF
            }
        })),
        16 => Some(0), // close: write-through, nothing to flush
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
                Err(_) => 0xFF,
            }
        })),
        22 => Some(with_fcb(cpm, |_cpm, fcb| {
            if fs.make(fcb).is_some() {
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
        34 => Some(with_fcb(cpm, |cpm, fcb| {
            let rr = fcb.random_record();
            let dma = cpm.read_block(fs.dma(), 128);
            let mut data = [0u8; 128];
            data.copy_from_slice(&dma);
            match fs.write_record(fcb, rr, &data) {
                Ok(()) => {
                    fcb.set_seq_record(rr);
                    0x00
                }
                Err(_) => 0xFF,
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

    /// End-to-end: a Z80 program MAKEs a file, WRITEs a record from the
    /// DMA buffer, CLOSEs, re-OPENs, READs the record back into a different
    /// DMA buffer, and prints it — driven through the real BDOS file calls
    /// against a temp `CPM/` drive.  Exercises the FCB↔memory↔host-file glue
    /// (read_block/write_block/store_position/seq_record) the driver relies
    /// on.
    #[test]
    fn test_program_file_io_roundtrip() {
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
    fn test_setup_command_line_page_zero() {
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
        assert!(fs.make(&fcb).is_some());
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
        // Same tight loop, but the abort flag is already set: no progress.
        let prog = [0xC3, 0x00, 0x01];
        let mut cpm = Cpm::new();
        cpm.load_com(&prog);
        let abort = AtomicBool::new(true);
        assert_eq!(cpm.run(1_000_000, &abort), Stop::Aborted);
        assert_eq!(cpm.instructions(), 0);
    }
}
