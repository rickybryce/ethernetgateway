//! The emulated machine's memory + I/O ports for the CP/M 2.2 environment.
//!
//! A flat 64 KB address space is all a non-banked CP/M 2.2 system needs.
//! I/O ports are inert unless a **virtual-modem UART profile** is selected
//! (see [`crate::cpm::uart`]): when one is, the machine answers `IN`/`OUT` at
//! the profile's status + data ports so a CP/M comms program finds its modem.
//! This layer models only the *idle* UART (transmit ready, no receive
//! pending); wiring the data registers to the gateway's outbound dial is the
//! next step.

use crate::cpm::uart::UartProfile;
use iz80::Machine;

/// 64 KB RAM machine backing the Z80 CPU, plus an optional virtual-modem UART.
pub struct CpmMachine {
    mem: Vec<u8>,
    uart: Option<UartProfile>,
}

impl CpmMachine {
    /// A zeroed 64 KB address space with no virtual modem.
    pub fn new() -> CpmMachine {
        CpmMachine {
            mem: vec![0u8; 65536],
            uart: None,
        }
    }

    /// Select (or clear, with `None`) the virtual-modem UART placement.
    pub fn set_uart(&mut self, uart: Option<UartProfile>) {
        self.uart = uart;
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

    fn port_in(&mut self, address: u16) -> u8 {
        let port = address as u8;
        if let Some(u) = self.uart {
            if port == u.status_port {
                // Idle UART: transmit ready, nothing received.
                return u.family.idle_status();
            }
            if port == u.data_port {
                // No received byte yet (the dial-out bridge lands next).
                return 0;
            }
        }
        0
    }

    fn port_out(&mut self, _address: u16, _value: u8) {
        // Writes to the UART's data/command ports are accepted and dropped for
        // now (no outbound bridge yet); other ports remain inert.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cpm::uart::resolve_uart;

    #[test]
    fn test_ports_inert_without_uart() {
        let mut m = CpmMachine::new();
        assert_eq!(m.port_in(0x82), 0);
        assert_eq!(m.port_in(0x83), 0);
        m.port_out(0x82, 0x55); // must not panic
    }

    #[test]
    fn test_uart_answers_at_selected_ports() {
        let mut m = CpmMachine::new();
        m.set_uart(resolve_uart("rc2014_1b")); // Z80 SIO at 0x82 (status) / 0x83 (data)
        // Status port reports the idle Z80-SIO status (TX empty, no RX).
        assert_eq!(m.port_in(0x82), 0x04);
        // Data port has nothing received yet.
        assert_eq!(m.port_in(0x83), 0x00);
        // A different port is still inert.
        assert_eq!(m.port_in(0x10), 0x00);
        // Clearing the profile makes the ports inert again.
        m.set_uart(None);
        assert_eq!(m.port_in(0x82), 0x00);
    }

    #[test]
    fn test_acia_profile_idle_status() {
        let mut m = CpmMachine::new();
        m.set_uart(resolve_uart("altair_2sio1")); // 6850 ACIA at 0x10 / 0x11
        assert_eq!(m.port_in(0x10), 0x02); // TDRE set
    }
}
