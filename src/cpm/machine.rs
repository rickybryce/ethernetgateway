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
    /// Consecutive `IN`s from a port no device answers, since the last one
    /// that was answered.
    ///
    /// Counted because `0xFF` — the honest reading of an unloaded bus, and the
    /// one ZEXALL folds into its CRC — is also, on a 6850 or an SIO status
    /// register, "a character is waiting".  So a guest whose port is not there
    /// reads `0xFF`, believes a byte arrived, reads the data port, gets `0xFF`
    /// again, prints it, and does that as fast as the host will let it.
    ///
    /// Measured with `cpm_emu_uart = off` and EGT8080 in terminal mode, which is
    /// not a contrived setup — that setting is documented as the way to shut
    /// the emulator's network door, and the terminal's shipped default port is
    /// the gateway's own: **52-65% of a core**, indefinitely, with *nothing*
    /// reaching the client, because the terminal's ASCII filter drops the
    /// bytes.  The only symptom was host CPU, which is why it went unnoticed.
    ///
    /// The existing idle pacing cannot see it: that counts BDOS/BIOS status
    /// calls answering "nothing available", and this guest ends every pass
    /// with a console *write*, which is real work by any measure.  What is
    /// unreal is where the byte came from.
    ///
    /// A claimed read clears it, so software talking to a port that exists is
    /// never paced; a burst of probing (`survey.mac` walking the I/O space)
    /// crosses the threshold a few times and pays a millisecond or two, once.
    unclaimed_reads: u32,
}

impl CpmMachine {
    /// A zeroed 64 KB address space with no virtual modem.
    pub fn new() -> CpmMachine {
        CpmMachine { mem: vec![0u8; 65536], modem: ModemPort::new(), unclaimed_reads: 0 }
    }

    /// Consecutive reads of a port no device answers — see the field.
    ///
    /// Read by the emulator's run loop between CPU batches, which is the only
    /// place that can pause: `port_in` is called from inside the batch and has
    /// nowhere to await.
    pub fn unclaimed_reads(&self) -> u32 {
        self.unclaimed_reads
    }

    /// Forget the count, having paced the guest once for it.
    ///
    /// Without this the count is cleared only by a read that *is* answered —
    /// so a program which sweeps the I/O space and then settles down to work
    /// would carry a large count into a phase with no port reads at all, and
    /// be paced through every batch of it for nothing. Clearing after each nap
    /// makes the rule "a burst pays once, a loop pays every time round", which
    /// is the distinction actually wanted.
    pub fn clear_unclaimed_reads(&mut self) {
        self.unclaimed_reads = 0;
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
        // **A port nothing answers at reads `0xFF`, not zero.**
        //
        // This used to read zero, justified on the grounds that the guest is
        // "software we chose". It is not: the whole point of the emulator is
        // running arbitrary `.COM` files, and software that probes for hardware
        // is exactly the software that reads a port nobody drives. Zero is a
        // *plausible* answer — an idle status register, a device present and
        // ready — so a probe finds a board that is not there. `0xFF` is the
        // answer an unloaded bus gives, because it floats high.
        //
        // Every other machine agrees, and it is worth listing because they were
        // measured rather than assumed: our own `BootMachine` answers `0xFF`;
        // every one of z80pack's eight machines defines
        // `IO_DATA_UNUSED 0xff`, and `cpmsim`'s changelog carries the reason —
        // "unused I/O ports need to return FF, see survey.mac", `survey.mac`
        // being a real CP/M program that inventories hardware. The lone
        // exception there is `intelmdssim`, a different bus entirely.
        //
        // It is also load-bearing for conformance: `INI`/`IND` copy the byte a
        // port gives into memory and set `N` from its top bit, so the value
        // lands in ZEXALL's CRC for the `<ini,outi,ind,outd><,r>` group.
        match self.modem.port_in(address as u8) {
            Some(v) => {
                self.unclaimed_reads = 0;
                v
            }
            None => {
                self.unclaimed_reads = self.unclaimed_reads.saturating_add(1);
                0xFF
            }
        }
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

    /// With no modem selected, the UART ports read as an *empty bus* — `0xFF`,
    /// the same as any other port nothing answers at.
    ///
    /// Zero would be worse than useless here: it is what a real, idle status
    /// register looks like, so a guest probing for a UART would find one and
    /// then wait forever for a character. See `CpmMachine::port_in`.
    #[test]
    fn test_ports_inert_without_modem() {
        let mut m = CpmMachine::new();
        assert_eq!(m.port_in(0x82), 0xFF);
        assert_eq!(m.port_in(0x83), 0xFF);
        // And a port no profile ever uses reads the same way.
        assert_eq!(m.port_in(0x40), 0xFF);
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
        // No port answers in AUX mode; the driver uses the ring accessors. An
        // unanswered port reads as an empty bus, `0xFF`, not zero — see
        // `CpmMachine::port_in`.
        assert_eq!(m.port_in(0x82), 0xFF);
        m.port_out(0x83, b'Z');
        assert!(m.modem_drain_tx().is_empty()); // OUT ignored in AUX mode
        m.modem_tx_push(b'Z'); // driver's AUX-out path
        assert_eq!(m.modem_drain_tx(), b"Z");
        m.modem_queue_rx(b"ab");
        assert_eq!(m.modem_rx_pop(), Some(b'a'));
        assert_eq!(m.modem_rx_pop(), Some(b'b'));
        assert_eq!(m.modem_rx_pop(), None);
    }

    /// **A guest reading hardware that is not there is counted, so the run
    /// loop can pace it.**
    ///
    /// `0xFF` is the right answer for an unclaimed port and stays. The problem
    /// it creates is that on a 6850 or an SIO status register `0xFF` also
    /// means "a character is waiting", so a terminal pointed at a port that
    /// does not exist reads a byte, prints it, and does that forever. Measured
    /// at 52-65% of a core before this counter existed.
    #[test]
    fn test_reads_of_a_port_nothing_answers_are_counted() {
        let mut m = CpmMachine::new();
        m.set_access(ModemAccess::Off);
        assert_eq!(m.unclaimed_reads(), 0, "nothing has been read yet");
        for want in 1..=5 {
            assert_eq!(m.port_in(0x82), 0xFF, "an empty bus floats high");
            assert_eq!(m.unclaimed_reads(), want);
        }
        // Paced once, the count starts again: a burst of probing pays once,
        // a loop pays every time round.
        m.clear_unclaimed_reads();
        assert_eq!(m.unclaimed_reads(), 0);
    }

    /// **A port that IS answered clears the count**, so software talking to
    /// hardware that exists is never paced — which is the whole population of
    /// programs this must not slow down.
    #[test]
    fn test_an_answered_read_clears_the_unclaimed_count() {
        let mut m = CpmMachine::new();
        m.set_access(crate::cpm::resolve_access("rc2014_1b"));
        for _ in 0..10 {
            m.port_in(0x00); // nothing there
        }
        assert!(m.unclaimed_reads() >= 10);
        m.port_in(0x82); // the SIO status port, which this profile does claim
        assert_eq!(
            m.unclaimed_reads(),
            0,
            "a read the modem answered means the guest is talking to real \
             hardware, and must not be paced"
        );
    }
}
