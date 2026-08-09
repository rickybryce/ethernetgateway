//! The Cromemco bank select on port `40h` — what Cromix runs on.
//!
//! Cromemco's memory boards (the 16KZ, the 4KZ and their successors) carry a
//! bank-select feature, and the whole of it is one write-only port. From the
//! *Cromemco 16KZ RAM Card Technical Manual*, Rev C, §2.2:
//!
//! > On power-up the active memory bank is Bank 0. Only memory boards mapped
//! > to this bank are immediately active after power-up. At this point, any
//! > bank or banks may be enabled under software control, by addressing I/O
//! > port 40H dedicated to this function. The 8 bits output from port 40H
//! > enable or disable the corresponding bank(s) in memory. A set bit "1" in
//! > the corresponding bit position will enable the memory bank. A reset bit
//! > "0" will disable it.
//!
//! So it is a **bitmap, not a bank number** — eight banks of 64 KB, one bit
//! each, bit 0 being bank 0. That is why Cromix's very first instruction is
//! `LD A,1 / OUT (40h),A`: it is enabling bank 0, the one already active, which
//! only makes sense as a bitmap.
//!
//! **CLEAN-ROOM**, from that manual — the same discriminator settled for
//! Punter, HBIOS, EGT80, the VDM-1 and the Dazzler. Deliberately *not* read out
//! of z80pack's `cromemcosim`, which is what made `z80pack.rs` the one
//! derived device in this codebase; its `IO-PORTS` file was used only to learn
//! that `40h` is the bank select at all, which is a fact about a port number
//! and not a design.
//!
//! # What a bitmap means for an emulator
//!
//! Hardware lets several banks be enabled at once, because a *card* is assigned
//! to a bank by DIP switch and a machine may have cards at different addresses
//! in different banks — the manual's Direct Memory Access Override exists
//! precisely for blocks "residing in different memory banks, with identical or
//! overlapping addresses". We have one 64 KB card's worth of memory per bank,
//! so two bits set would be two cards answering one address: bus contention,
//! which no manual defines. We take the lowest set bit and say so, rather than
//! inventing a merge.

/// The port. Write-only: the manual describes eight bits *output*, and says
/// nothing about reading, so an `IN` falls through to the machine's usual
/// answer for a port nobody drives.
pub const PORT: u8 = 0x40;

/// Banks the hardware allows — eight levels of 64 KB.
pub const BANKS: usize = 8;

/// The bank-select state of one machine.
#[derive(Default)]
pub struct BankSelect {
    /// The bank the machine is reading and writing.
    ///
    /// Zero until a guest selects otherwise, which is also the power-up state
    /// the manual describes.
    current: usize,
    /// Banks 1.. , each 64 KB, allocated on first use.
    ///
    /// Bank 0 is the machine's own memory and is not here: a guest that never
    /// banks must not pay 512 KB for the possibility, and — more importantly —
    /// every existing machine must keep bank 0 as the array it always was.
    upper: Vec<Option<Vec<u8>>>,
}

impl BankSelect {
    /// True while bank 0 is selected, which is the whole life of every guest
    /// that never touches the port.
    ///
    /// The caller's fast path: one comparison, then the flat array index it
    /// always was.
    pub fn is_idle(&self) -> bool {
        self.current == 0
    }

    /// Which bank is selected.
    ///
    /// Test-only: the machine asks `is_idle` and then hands memory over, so
    /// nothing in the product needs the number — but the tests below are about
    /// *which* bank a bitmap names, which is the part that would be wrong.
    #[cfg(test)]
    pub fn current(&self) -> usize {
        self.current
    }

    /// Handle a write to the port.
    ///
    /// The lowest set bit wins, for the reason in the module comment. A write
    /// of zero enables *nothing*, which on real hardware means no card answers
    /// and the machine cannot execute — so it is treated as bank 0 rather than
    /// modelled: a guest that has stopped its own memory is not a state we can
    /// usefully reproduce, and the alternative is a machine that reads `FF`
    /// for ever and looks like a crashed emulator rather than a crashed guest.
    pub fn port_out(&mut self, value: u8) {
        self.current = match value {
            0 => 0,
            v => v.trailing_zeros() as usize,
        };
    }

    /// Read a byte from the selected bank.
    ///
    /// `bank0` is the machine's own memory, passed in rather than owned,
    /// because it is bank 0 and every unbanked path must keep using it
    /// directly.
    pub fn read(&self, bank0: &[u8], addr: u16) -> u8 {
        if self.current == 0 {
            return bank0[addr as usize];
        }
        match self.upper.get(self.current - 1).and_then(|b| b.as_ref()) {
            Some(bank) => bank[addr as usize],
            // A bank the guest selected but has never written. Empty RAM, not
            // an error: a card that is present and blank reads as whatever it
            // powered up as, and zero is what our bank 0 starts as too.
            None => 0,
        }
    }

    /// Write a byte to the selected bank, allocating it on first use.
    pub fn write(&mut self, bank0: &mut [u8], addr: u16, value: u8) {
        if self.current == 0 {
            bank0[addr as usize] = value;
            return;
        }
        if self.upper.len() < BANKS - 1 {
            self.upper.resize_with(BANKS - 1, || None);
        }
        let bank = self.upper[self.current - 1].get_or_insert_with(|| vec![0; 0x1_0000]);
        bank[addr as usize] = value;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The manual's own example, and the reason we know it is a bitmap: bit 0
    /// is bank 0, which is already active at power-up, so `OUT (40h),1` is a
    /// no-op — exactly what Cromix opens with.
    #[test]
    fn test_bit_zero_is_bank_zero() {
        let mut b = BankSelect::default();
        assert!(b.is_idle(), "power-up is bank 0");
        b.port_out(0x01);
        assert_eq!(b.current(), 0);
        assert!(b.is_idle(), "selecting the bank you are already in changes nothing");
    }

    /// One bit per bank, up the register — *not* a bank number. Reading `0x02`
    /// as "bank 2" instead of "bank 1" is the mistake this pins, and it would
    /// put a guest's memory one bank along from where it left it.
    #[test]
    fn test_each_bit_selects_its_own_bank() {
        let mut b = BankSelect::default();
        for (value, bank) in [(0x01u8, 0usize), (0x02, 1), (0x04, 2), (0x08, 3), (0x80, 7)] {
            b.port_out(value);
            assert_eq!(b.current(), bank, "{value:#04x} is bank {bank}");
        }
    }

    /// Banks are separate 64 KB spaces: the same address in two banks holds two
    /// different bytes, which is the entire point of the feature.
    #[test]
    fn test_a_bank_is_its_own_memory() {
        let mut zero = vec![0u8; 0x1_0000];
        let mut b = BankSelect::default();
        b.write(&mut zero, 0x1234, 0xAA);

        b.port_out(0x02); // bank 1
        assert_eq!(b.read(&zero, 0x1234), 0, "a fresh bank does not see bank 0");
        b.write(&mut zero, 0x1234, 0xBB);
        assert_eq!(b.read(&zero, 0x1234), 0xBB);

        b.port_out(0x01); // back to bank 0
        assert_eq!(b.read(&zero, 0x1234), 0xAA, "bank 0 kept its own byte");
        assert_eq!(zero[0x1234], 0xAA, "and it really is the machine's own array");
    }

    /// Two bits set is two cards answering one address — bus contention, which
    /// no manual defines. The lowest wins, deliberately and visibly, rather
    /// than some merge we would have invented.
    #[test]
    fn test_several_banks_enabled_takes_the_lowest() {
        let mut b = BankSelect::default();
        b.port_out(0b0000_1100); // banks 2 and 3
        assert_eq!(b.current(), 2);
    }

    /// A guest that enables nothing has switched its own memory off, which is
    /// not a machine that can run. Bank 0 rather than a machine that reads
    /// nothing at all: the failure should look like the guest's, not ours.
    #[test]
    fn test_enabling_nothing_falls_back_to_bank_zero() {
        let mut b = BankSelect::default();
        b.port_out(0x04);
        b.port_out(0x00);
        assert_eq!(b.current(), 0);
        assert!(b.is_idle());
    }

    /// Upper banks cost nothing until they are used — a guest that never banks
    /// must not pay half a megabyte for the possibility.
    #[test]
    fn test_upper_banks_are_allocated_only_when_written() {
        let mut zero = vec![0u8; 0x1_0000];
        let mut b = BankSelect::default();
        b.port_out(0x80); // bank 7, selected but never written
        assert!(b.upper.is_empty(), "selecting a bank allocates nothing");
        assert_eq!(b.read(&zero, 0), 0, "and it reads as blank RAM");
        b.write(&mut zero, 0, 1);
        assert_eq!(b.upper.iter().filter(|s| s.is_some()).count(), 1, "only the one");
    }
}
