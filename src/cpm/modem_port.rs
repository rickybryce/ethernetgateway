//! The virtual modem's side of a UART, independent of which machine it is in.
//!
//! Two emulated machines need this and they are not related: [`CpmMachine`]
//! runs a `.COM` under our own BDOS, and [`BootMachine`] runs a disk image's
//! own operating system on emulated Altair hardware. Both present the guest
//! with a UART at a configured pair of ports, and both move bytes through the
//! same two rings — TX (guest → peer), filled by `OUT` to the data port and
//! drained by the async driver; RX (peer → guest), filled by the driver and
//! drained by `IN`.
//!
//! It lives here rather than in either machine because the ring semantics are
//! the part that is easy to get subtly wrong — backpressure, the SIO's read-
//! register pointer, what "transmit ready" means when the driver has not run
//! yet — and having one copy means a booted disk and the emulator cannot
//! disagree about it.
//!
//! [`CpmMachine`]: super::machine::CpmMachine
//! [`BootMachine`]: super::boot_machine::BootMachine

use super::uart::{ModemAccess, UartFamily};
use std::collections::VecDeque;

/// Cap on each ring so a guest (or peer) that never drains can't grow the
/// buffer without bound.  64 KB matches the gateway's duplex buffers.
pub const MODEM_RING_CAP: usize = 65536;

/// A virtual modem hanging off a machine's I/O ports.
pub struct ModemPort {
    access: ModemAccess,
    /// Guest → peer bytes (filled by `OUT`/AUX-out, drained by the driver).
    tx: VecDeque<u8>,
    /// Peer → guest bytes (filled by the driver, drained by `IN`/AUX-in).
    rx: VecDeque<u8>,
    /// Whether the modem currently has a carrier (surfaced as DCD in status).
    /// Set by the driver each pump cycle from the modem's online state.
    carrier: bool,
    /// Z80 SIO read-register pointer (0..7), set by a WR0 write and cleared
    /// after the next status read.  0 (the default) selects RR0, so software
    /// that never touches the pointer reads live status.
    sio_ptr: u8,
}

impl ModemPort {
    /// A modem nobody can reach.
    pub fn new() -> ModemPort {
        ModemPort {
            access: ModemAccess::Off,
            tx: VecDeque::new(),
            rx: VecDeque::new(),
            carrier: false,
            sio_ptr: 0,
        }
    }

    /// Select how the guest reaches the modem.
    pub fn set_access(&mut self, access: ModemAccess) {
        self.access = access;
    }

    /// Set the carrier (DCD) state the status register reflects.
    pub fn set_carrier(&mut self, carrier: bool) {
        self.carrier = carrier;
    }

    /// Drain everything the guest wrote toward the peer.
    pub fn drain_tx(&mut self) -> Vec<u8> {
        self.tx.drain(..).collect()
    }

    /// Free space remaining in the RX ring — how many peer bytes the guest can
    /// still accept.  The driver uses this to cap how much it reads from the
    /// peer, so a slow guest applies backpressure (unread bytes stay in the
    /// socket) instead of losing data.
    pub fn rx_free(&self) -> usize {
        MODEM_RING_CAP.saturating_sub(self.rx.len())
    }

    /// Queue peer bytes for the guest to read (bounded).
    pub fn queue_rx(&mut self, data: &[u8]) {
        for &b in data {
            if self.rx.len() >= MODEM_RING_CAP {
                break;
            }
            self.rx.push_back(b);
        }
    }

    /// Pop one received byte (BDOS AUX input).
    pub fn rx_pop(&mut self) -> Option<u8> {
        self.rx.pop_front()
    }

    /// Push one byte toward the peer (BDOS AUX output, bounded).
    pub fn tx_push(&mut self, b: u8) {
        if self.tx.len() < MODEM_RING_CAP {
            self.tx.push_back(b);
        }
    }

    /// Bytes waiting for the guest to read (HBIOS input-status count).
    pub fn rx_len(&self) -> usize {
        self.rx.len()
    }

    /// Room left in the TX ring (HBIOS output-status count).  Zero means the
    /// guest must wait — the same backpressure the port-I/O status bit reports
    /// as transmit-not-ready.
    pub fn tx_free(&self) -> usize {
        MODEM_RING_CAP.saturating_sub(self.tx.len())
    }

    /// The HBIOS serial unit the modem answers as, if an HBIOS mode is set.
    pub fn hbios_unit(&self) -> Option<u8> {
        match self.access {
            ModemAccess::Hbios { unit } => Some(unit),
            _ => None,
        }
    }

    /// Answer an `IN`, or `None` if this port is not the modem's.
    ///
    /// `None` rather than a default byte because the two machines disagree
    /// about what an unclaimed port reads: our own returns 0, a booted Altair
    /// returns an idle bus of `0xFF`. That is the caller's business, not this
    /// module's.
    pub fn port_in(&mut self, port: u8) -> Option<u8> {
        let ModemAccess::Ports(u) = self.access else {
            return None;
        };
        if port == u.status_port {
            // Live status: RX-available if a byte waits; TX-ready only while
            // the TX ring has room (so a polled sender that outruns the driver
            // waits instead of overflowing and losing bytes); DCD from carrier.
            let tx_ready = self.tx.len() < MODEM_RING_CAP;
            let rr0 = u.family.status(!self.rx.is_empty(), tx_ready, self.carrier);
            if u.family == UartFamily::Sio {
                // Return the register the WR0 pointer selected, then the
                // pointer auto-resets to 0 (RR0) as the real SIO does.
                let ptr = self.sio_ptr;
                self.sio_ptr = 0;
                return Some(match ptr {
                    0 => rr0,
                    1 => 0x01, // RR1: All Sent, no Rx errors (our ideal wire)
                    _ => 0x00, // RR2 (vector) and unused registers: 0
                });
            }
            return Some(rr0);
        }
        if port == u.data_port {
            return Some(self.rx.pop_front().unwrap_or(0));
        }
        None
    }

    /// Take an `OUT`; returns whether the port belonged to the modem.
    pub fn port_out(&mut self, port: u8, value: u8) -> bool {
        let ModemAccess::Ports(u) = self.access else {
            return false;
        };
        if port == u.data_port {
            self.tx_push(value);
            return true;
        }
        if port == u.status_port {
            if u.family == UartFamily::Sio {
                // SIO command register (WR0): the low 3 bits select the read
                // register for the next status IN.  A write while a non-zero
                // pointer is set targets that WRn (config we don't model) and
                // returns the pointer to 0, matching the SIO's behaviour.
                if self.sio_ptr == 0 {
                    self.sio_ptr = value & 0x07;
                } else {
                    self.sio_ptr = 0;
                }
            }
            // Other status/command writes (ACIA control, 88-SIO) are accepted
            // and ignored — we present a fixed idle UART.
            return true;
        }
        false
    }
}

impl Default for ModemPort {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cpm::uart::resolve_access;

    #[test]
    fn test_an_unclaimed_port_is_left_to_the_machine() {
        let mut m = ModemPort::new();
        assert_eq!(m.port_in(0x82), None, "no modem selected");
        assert!(!m.port_out(0x82, 0x55));

        m.set_access(resolve_access("rc2014_1b")); // 0x82/0x83
        assert!(m.port_in(0x82).is_some(), "status is ours");
        assert!(m.port_in(0x83).is_some(), "data is ours");
        assert_eq!(m.port_in(0x40), None, "and nothing else is");
    }

    #[test]
    fn test_rings_carry_bytes_both_ways_with_backpressure() {
        let mut m = ModemPort::new();
        m.set_access(resolve_access("altair_2sio2"));
        m.queue_rx(b"Hi");
        assert_eq!(m.rx_len(), 2);
        assert_eq!(m.port_in(0x13), Some(b'H'));
        assert_eq!(m.port_in(0x13), Some(b'i'));
        assert_eq!(m.port_in(0x13), Some(0), "an empty ring reads zero");

        m.port_out(0x13, b'X');
        m.port_out(0x13, b'Y');
        assert_eq!(m.drain_tx(), b"XY");
        assert!(m.drain_tx().is_empty(), "drained, not copied");

        assert_eq!(m.rx_free(), MODEM_RING_CAP);
        m.queue_rx(&vec![0u8; MODEM_RING_CAP + 100]);
        assert_eq!(m.rx_len(), MODEM_RING_CAP, "the ring is bounded");
        assert_eq!(m.rx_free(), 0);
    }
}
