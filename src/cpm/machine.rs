//! The emulated machine's memory + I/O ports for the CP/M 2.2 environment.
//!
//! A flat 64 KB address space is all a non-banked CP/M 2.2 system needs.
//! I/O ports are stubbed for now (reads return 0, writes are dropped); a
//! later phase (the virtual modem, kernelplan.md §"Virtual modem") wires
//! specific ports to the gateway's outbound dial.

use iz80::Machine;

/// 64 KB RAM machine backing the Z80 CPU.  Ports are inert until the
/// virtual-UART layer lands.
pub struct CpmMachine {
    mem: Vec<u8>,
}

impl CpmMachine {
    /// A zeroed 64 KB address space.
    pub fn new() -> CpmMachine {
        CpmMachine {
            mem: vec![0u8; 65536],
        }
    }
}

impl Default for CpmMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl Machine for CpmMachine {
    fn peek(&mut self, address: u16) -> u8 {
        self.mem[address as usize]
    }

    fn poke(&mut self, address: u16, value: u8) {
        self.mem[address as usize] = value;
    }

    // Ports are inert for now — no hardware is wired to the emulated Z80.
    fn port_in(&mut self, _address: u16) -> u8 {
        0
    }

    fn port_out(&mut self, _address: u16, _value: u8) {}
}
