//! The emulated machine's memory + I/O ports for the CP/M 2.2 environment.
//!
//! A flat 64 KB address space is all a non-banked CP/M 2.2 system needs.
//! I/O ports are inert unless a **virtual-modem** access mode is selected
//! (see [`crate::cpm::uart`]).  With `ModemAccess::Ports`, the machine answers
//! `IN`/`OUT` at the profile's status + data ports, moving bytes through two
//! rings — a TX ring (guest → peer) filled by `OUT` to the data port, and an
//! RX ring (peer → guest) drained by `IN` from the data port.  The async
//! driver services the rings between CPU batches (it forwards TX to the peer
//! connection and queues received bytes into RX), so the synchronous port I/O
//! never has to `.await`.  With `ModemAccess::Aux` the ports stay inert and the
//! guest reaches the same rings through the BDOS `AUX:` device (funcs 3/4,
//! handled in the driver).

use crate::cpm::modem_port::ModemPort;
use crate::cpm::uart::ModemAccess;
use iz80::Machine;

/// 64 KB RAM machine backing the Z80 CPU, plus the virtual-modem channel.
pub struct CpmMachine {
    mem: Vec<u8>,
    /// The guest's UART and its rings.  Shared with the booted-disk machine so
    /// the two cannot disagree about backpressure or status bits; see
    /// [`ModemPort`].
    modem: ModemPort,
}

impl CpmMachine {
    /// A zeroed 64 KB address space with no virtual modem.
    pub fn new() -> CpmMachine {
        CpmMachine { mem: vec![0u8; 65536], modem: ModemPort::new() }
    }

    /// Select how the guest reaches the virtual modem.
    pub fn set_access(&mut self, access: ModemAccess) {
        self.modem.set_access(access);
    }

    /// Set the carrier (DCD) state the status register reflects.  Called by
    /// the driver each pump cycle from the modem's online state.
    pub fn set_carrier(&mut self, carrier: bool) {
        self.modem.set_carrier(carrier);
    }

    /// Drain everything the guest wrote toward the peer.
    pub fn modem_drain_tx(&mut self) -> Vec<u8> {
        self.modem.drain_tx()
    }

    /// Free space remaining in the RX ring — how many peer bytes the guest
    /// can still accept before the ring is full.  The driver uses this to cap
    /// how much it reads from the peer, so a slow guest applies backpressure
    /// (unread bytes stay in the socket / duplex) instead of losing data.
    pub fn modem_rx_free(&self) -> usize {
        self.modem.rx_free()
    }

    /// Queue peer bytes for the guest to read (bounded).
    pub fn modem_queue_rx(&mut self, data: &[u8]) {
        self.modem.queue_rx(data);
    }

    /// Pop one received byte (BDOS AUX input).
    pub fn modem_rx_pop(&mut self) -> Option<u8> {
        self.modem.rx_pop()
    }

    /// Push one byte toward the peer (BDOS AUX output, bounded).
    pub fn modem_tx_push(&mut self, b: u8) {
        self.modem.tx_push(b);
    }

    /// Bytes waiting for the guest to read (HBIOS input-status count).
    pub fn modem_rx_len(&self) -> usize {
        self.modem.rx_len()
    }

    /// Room left in the TX ring (HBIOS output-status count).  Zero means the
    /// guest must wait — the same backpressure the port-I/O status bit reports
    /// as transmit-not-ready.
    pub fn modem_tx_free(&self) -> usize {
        self.modem.tx_free()
    }

    /// The HBIOS serial unit the virtual modem answers as, if an HBIOS access
    /// mode is selected.
    pub fn hbios_unit(&self) -> Option<u8> {
        self.modem.hbios_unit()
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
        // A port the modem does not claim reads zero here.  A booted Altair
        // answers 0xFF for the same case — see `BootMachine` — because an
        // unloaded S-100 bus floats high; under our own BDOS the guest is
        // running software we chose, and zero is the long-standing behaviour.
        self.modem.port_in(address as u8).unwrap_or(0)
    }

    fn port_out(&mut self, address: u16, value: u8) {
        self.modem.port_out(address as u8, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cpm::modem_port::MODEM_RING_CAP;
    use crate::cpm::uart::resolve_access;

    #[test]
    fn test_ports_inert_without_modem() {
        let mut m = CpmMachine::new();
        assert_eq!(m.port_in(0x82), 0);
        assert_eq!(m.port_in(0x83), 0);
        m.port_out(0x82, 0x55); // must not panic
        assert!(m.modem_drain_tx().is_empty());
    }

    #[test]
    fn test_uart_status_and_data_rings() {
        let mut m = CpmMachine::new();
        m.set_access(resolve_access("rc2014_1b")); // Z80 SIO 0x82 status / 0x83 data
        // Idle: TX empty, no RX.
        assert_eq!(m.port_in(0x82), 0x04);
        assert_eq!(m.port_in(0x83), 0x00);
        // Peer sends two bytes -> RX-available bit sets, guest reads them.
        m.modem_queue_rx(b"Hi");
        assert_eq!(m.port_in(0x82), 0x05); // TX empty + RX avail
        assert_eq!(m.port_in(0x83), b'H');
        assert_eq!(m.port_in(0x83), b'i');
        assert_eq!(m.port_in(0x82), 0x04); // drained
        assert_eq!(m.port_in(0x83), 0x00);
        // Guest writes go to the TX ring for the driver to forward.
        m.port_out(0x83, b'X');
        m.port_out(0x83, b'Y');
        assert_eq!(m.modem_drain_tx(), b"XY");
        assert!(m.modem_drain_tx().is_empty());
    }

    #[test]
    fn test_rx_free_tracks_ring() {
        let mut m = CpmMachine::new();
        assert_eq!(m.modem_rx_free(), MODEM_RING_CAP);
        m.modem_queue_rx(b"hello");
        assert_eq!(m.modem_rx_free(), MODEM_RING_CAP - 5);
    }

    #[test]
    fn test_tx_ready_clears_when_ring_full() {
        let mut m = CpmMachine::new();
        m.set_access(resolve_access("rc2014_1b")); // Z80 SIO, TX empty = bit2
        assert_eq!(m.port_in(0x82) & 0x04, 0x04); // TX ready when empty
        // Fill the TX ring to capacity via the data port.
        for _ in 0..MODEM_RING_CAP {
            m.port_out(0x83, b'x');
        }
        assert_eq!(m.port_in(0x82) & 0x04, 0x00); // TX no longer ready
        // Draining restores TX-ready.
        let _ = m.modem_drain_tx();
        assert_eq!(m.port_in(0x82) & 0x04, 0x04);
    }

    #[test]
    fn test_sio_register_pointer() {
        let mut m = CpmMachine::new();
        m.set_access(resolve_access("rc2014_1b")); // Z80 SIO
        // Default pointer (0): status reads return RR0 as before.
        assert_eq!(m.port_in(0x82), 0x04);
        // Select RR1 via WR0 (low 3 bits = 1); next status read is RR1.
        m.port_out(0x82, 0x01);
        assert_eq!(m.port_in(0x82), 0x01); // RR1: All Sent, no errors
        // Pointer auto-reset: the following read is RR0 again.
        assert_eq!(m.port_in(0x82), 0x04);
        // A command byte with pointer bits 0 (e.g. a reset command 0x18)
        // leaves the pointer at 0, so status stays RR0.
        m.port_out(0x82, 0x18);
        assert_eq!(m.port_in(0x82), 0x04);
    }

    #[test]
    fn test_carrier_surfaced_in_status() {
        let mut m = CpmMachine::new();
        m.set_access(resolve_access("rc2014_1b")); // Z80 SIO, DCD = bit3
        assert_eq!(m.port_in(0x82), 0x04); // no carrier: TX empty only
        m.set_carrier(true);
        assert_eq!(m.port_in(0x82), 0x0C); // TX empty + DCD
        m.set_carrier(false);
        assert_eq!(m.port_in(0x82), 0x04); // carrier dropped
    }

    #[test]
    fn test_aux_leaves_ports_inert() {
        let mut m = CpmMachine::new();
        m.set_access(ModemAccess::Aux);
        // No port answers in AUX mode; the driver uses the ring accessors.
        assert_eq!(m.port_in(0x82), 0);
        m.port_out(0x83, b'Z');
        assert!(m.modem_drain_tx().is_empty()); // OUT ignored in AUX mode
        m.modem_tx_push(b'Z'); // driver's AUX-out path
        assert_eq!(m.modem_drain_tx(), b"Z");
        m.modem_queue_rx(b"ab");
        assert_eq!(m.modem_rx_pop(), Some(b'a'));
        assert_eq!(m.modem_rx_pop(), Some(b'b'));
        assert_eq!(m.modem_rx_pop(), None);
    }
}
