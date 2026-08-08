//! z80pack `cpmsim`'s bank-switching MMU — what banked CP/M 3 runs on.
//!
//! **Derived, not clean-room**, for the same reason [`super::z80pack`] is: this
//! is a simulator's device invented by Udo Munk, so there is no datasheet and no
//! independent authority — the simulator's source *is* the specification. Written
//! from `cpmsim/srcsim/simio.c` and `simmem.h` (MIT, Copyright Udo Munk), whose
//! notice is carried in `about.hbs`.
//!
//! # What it is
//!
//! Bank 0 is the whole 64 KB and always exists. Banks 1.. are `segsize` bytes
//! each and replace only the *bottom* of the address space; everything at or
//! above `segsize` is **common** — it stays in bank 0 whichever bank is
//! selected, which is how a banked operating system keeps its BIOS and its stack
//! reachable while it swaps the memory underneath them.
//!
//! | port | read | write |
//! |------|------|-------|
//! | `14h` | banks allocated | allocate that many banks (once) |
//! | `15h` | current bank | select bank |
//! | `16h` | segment size in 256-byte pages | set it, before any bank exists |
//! | `17h` | common write-protect | set common write-protect |
//!
//! # Why it is here
//!
//! Without it, z80pack's CP/M 3 disks load, print their sign-on and then stop
//! dead. They are not broken and neither was the disk driver: the banked BIOS
//! selects a bank, the write goes nowhere, and it carries on executing whatever
//! was already there. Measured — `cpm3-1.dsk` writes port `14h` once and port
//! `15h` **284 times** before it goes quiet.

/// Banks the hardware allows, bank 0 included.
const MAX_BANKS: usize = 16;

/// Default segment size: 48 KB, so `C000`–`FFFF` is common.
const DEFAULT_SEGSIZE: usize = 49_152;

/// The bank-switching state of one machine.
#[derive(Debug, Clone)]
pub struct Mmu {
    /// Banks 1.., each `segsize` bytes. Bank 0 is the machine's own memory and
    /// is not stored here.
    banks: Vec<Vec<u8>>,
    selected: usize,
    segsize: usize,
    /// Write-protect for the common area. Bit 7 is set by the hardware when a
    /// write is refused, which is how a guest finds out it happened.
    wp_common: u8,
}

impl Default for Mmu {
    fn default() -> Self {
        Mmu { banks: Vec::new(), selected: 0, segsize: DEFAULT_SEGSIZE, wp_common: 0 }
    }
}

impl Mmu {
    /// Has any bank been allocated?  When not, every access goes straight to
    /// bank 0 and the machine behaves exactly as it did before this existed —
    /// which is what keeps the cost off every other disk's hot path.
    #[inline]
    pub fn is_idle(&self) -> bool {
        self.banks.is_empty()
    }

    /// Does this address currently live in a bank other than 0?
    ///
    /// The whole mapping rule, and it is worth stating plainly: an address at or
    /// above the segment size is **common** and stays in bank 0 no matter what
    /// is selected.
    #[inline]
    fn banked(&self, addr: u16) -> Option<usize> {
        if self.selected == 0 || addr as usize >= self.segsize {
            return None;
        }
        Some(self.selected)
    }

    /// Read through the mapping, given bank 0.
    #[inline]
    pub fn read(&self, bank0: &[u8], addr: u16) -> u8 {
        match self.banked(addr) {
            Some(b) => self.banks[b - 1][addr as usize],
            None => bank0[addr as usize],
        }
    }

    /// Write through the mapping. A write to write-protected common memory is
    /// **refused and remembered** — bit 7 of the protect register is how the
    /// guest is told, and dropping the write silently would hide a fault the
    /// hardware reports.
    #[inline]
    pub fn write(&mut self, bank0: &mut [u8], addr: u16, value: u8) {
        if addr as usize >= self.segsize && self.wp_common != 0 {
            self.wp_common |= 0x80;
            return;
        }
        match self.banked(addr) {
            Some(b) => self.banks[b - 1][addr as usize] = value,
            None => bank0[addr as usize] = value,
        }
    }

    /// Does this port belong to the MMU?
    pub fn owns_port(port: u8) -> bool {
        (0x14..=0x17).contains(&port)
    }

    /// Read one of its registers.
    pub fn port_in(&self, port: u8) -> u8 {
        match port {
            0x14 => (self.banks.len() + 1) as u8,
            0x15 => self.selected as u8,
            0x16 => (self.segsize >> 8) as u8,
            0x17 => self.wp_common,
            _ => 0xFF,
        }
    }

    /// Write one of its registers.
    ///
    /// Out-of-range requests are **ignored rather than fatal**. z80pack stops
    /// the CPU on them; here a guest asking for a bank it never allocated gets
    /// nothing rather than taking the session down, which is the same direction
    /// every other board in this emulator chose.
    pub fn port_out(&mut self, port: u8, value: u8) {
        match port {
            // Allocate. The count includes bank 0, which already exists, and it
            // is a once-only operation — z80pack refuses a second call outright.
            0x14 => {
                if self.banks.is_empty() && value as usize <= MAX_BANKS {
                    let extra = (value as usize).saturating_sub(1);
                    self.banks = vec![vec![0u8; self.segsize]; extra];
                }
            }
            0x15 => {
                if (value as usize) <= self.banks.len() {
                    self.selected = value as usize;
                }
            }
            // Segment size, in 256-byte pages, and only before any bank exists:
            // resizing an allocated bank is not a thing the hardware can do.
            0x16 => {
                if self.banks.is_empty() {
                    self.segsize = (value as usize) << 8;
                }
            }
            0x17 => self.wp_common = value,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_idle_until_a_bank_is_allocated() {
        let mut m = Mmu::default();
        assert!(m.is_idle(), "no bank means no cost to anyone else");
        m.port_out(0x15, 3); // selecting a bank that does not exist
        assert_eq!(m.selected, 0, "an unallocated bank must not become current");
        m.port_out(0x14, 4);
        assert!(!m.is_idle());
        assert_eq!(m.port_in(0x14), 4, "reads back the bank count, bank 0 included");
    }

    /// The mapping rule: below the segment size is banked, at or above it is
    /// common and stays in bank 0 whatever is selected.
    #[test]
    fn test_common_memory_is_shared_and_banked_memory_is_not() {
        let mut bank0 = vec![0u8; 0x10000];
        let mut m = Mmu::default();
        m.port_out(0x14, 3); // banks 0, 1, 2

        m.write(&mut bank0, 0x0100, 0xAA); // bank 0, low
        m.write(&mut bank0, 0xF000, 0xC0); // bank 0, common

        m.port_out(0x15, 1);
        assert_eq!(m.read(&bank0, 0x0100), 0x00, "bank 1 starts empty, not bank 0's data");
        assert_eq!(m.read(&bank0, 0xF000), 0xC0, "common memory follows you into a bank");
        m.write(&mut bank0, 0x0100, 0xBB);
        m.write(&mut bank0, 0xF000, 0xC1);

        m.port_out(0x15, 0);
        assert_eq!(m.read(&bank0, 0x0100), 0xAA, "bank 0's low memory was not disturbed");
        assert_eq!(m.read(&bank0, 0xF000), 0xC1, "but the common write was, because it is shared");

        m.port_out(0x15, 2);
        assert_eq!(m.read(&bank0, 0x0100), 0x00, "bank 2 is its own memory again");
    }

    #[test]
    fn test_segment_size_is_settable_only_before_allocation() {
        let mut m = Mmu::default();
        assert_eq!(m.port_in(0x16), (DEFAULT_SEGSIZE >> 8) as u8);
        m.port_out(0x16, 0x80); // 32 KB
        assert_eq!(m.segsize, 0x8000);
        m.port_out(0x14, 2);
        m.port_out(0x16, 0xC0);
        assert_eq!(m.segsize, 0x8000, "an allocated bank cannot be resized");
    }

    /// A refused write to protected common memory is remembered, because the
    /// hardware tells the guest by setting bit 7 and swallowing it silently
    /// would hide the fault.
    #[test]
    fn test_write_protected_common_memory_refuses_and_records() {
        let mut bank0 = vec![0u8; 0x10000];
        let mut m = Mmu::default();
        m.port_out(0x14, 2);
        m.port_out(0x17, 1);
        m.write(&mut bank0, 0xF000, 0x42);
        assert_eq!(bank0[0xF000], 0x00, "the write must not land");
        assert_eq!(m.port_in(0x17) & 0x80, 0x80, "and the guest must be able to see that");
        // Banked memory is unaffected by the common protect.
        m.port_out(0x15, 1);
        m.write(&mut bank0, 0x0100, 0x42);
        assert_eq!(m.read(&bank0, 0x0100), 0x42);
    }

    #[test]
    fn test_ports_claimed() {
        for p in 0x14..=0x17u8 {
            assert!(Mmu::owns_port(p));
        }
        for p in [0x00u8, 0x0E, 0x10, 0x13, 0x18, 0x82] {
            assert!(!Mmu::owns_port(p), "{p:#04x} belongs to something else");
        }
    }
}
