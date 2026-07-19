//! Virtual-modem UART profiles for the CP/M emulator (Flavor B).
//!
//! A CP/M communications program reaches its "modem" by doing `IN`/`OUT` to a
//! UART at a fixed I/O port address — a *status/command* register and a *data*
//! register.  Different machines place that UART at different addresses and
//! use different status-bit conventions, so the operator selects a **profile**
//! naming the machine/port; the profile resolves to a [`ModemAccess`] telling
//! the emulator how the guest reaches the modem:
//! - `Ports(profile)` — direct `IN`/`OUT` at the profile's status + data ports
//!   ([`crate::cpm::CpmMachine`] answers there).
//! - `Aux` — the modem is on the CP/M BDOS `AUX:` device (functions 3/4), the
//!   hardware-independent path RomWBW/SC126 comms software uses (a Z180 ASCI
//!   *port* profile can't work: our Z80 core doesn't decode the Z180
//!   `IN0`/`OUT0` instructions the ASCI needs).
//! - `Off` — no virtual modem.
//!
//! Addresses are sourced from real firmware/drivers:
//! - **RC2014 / RomWBW Z80 SIO/2**: RomWBW `HDIAG/sio.asm` — command/status at
//!   the even base, data at base+1; RR0 status bit0 = RX char available, bit2 =
//!   TX buffer empty.  The four channels of two SIO/2 boards sit at
//!   0x80/0x82/0x84/0x86 (qterm `QT-RC82`/`QT-RC84` patches; the 0x82 channel
//!   is the usual RomWBW `AUX:`).
//! - **Altair 88-2SIO** (6850 ACIA): David Hansel's Altair simulator
//!   `serial.cpp` — control/status at 0x10/0x12, data at 0x11/0x13; status
//!   bit0 = RDRF (RX), bit1 = TDRE (TX ready).
//! - **Altair 88-SIO**: same source — 0x00/0x01, active-low status (bit0 set =
//!   RX *not* ready, bit7 clear = TX ready).

/// The status-register convention a UART family uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UartFamily {
    /// Zilog Z80 SIO (RR0): bit0 = RX available, bit2 = TX empty (active-high).
    Sio,
    /// Motorola 6850 ACIA: bit0 = RDRF (RX), bit1 = TDRE (TX ready).
    Acia,
    /// Altair 88-SIO: active-low (bit0 set = RX not ready, bit7 clear = TX ready).
    Sio88,
}

impl UartFamily {
    /// The status byte the guest reads, given whether a received byte is
    /// waiting and whether TX can accept a byte (transmit-ready / buffer empty)
    /// and whether carrier (DCD) is present.
    ///
    /// DCD is surfaced **active-high** (bit set = carrier up) so that the
    /// no-carrier idle byte is identical to what this UART presented before a
    /// carrier bit existed — polled software that only masks the RX/TX bits is
    /// unaffected; only software that specifically tests DCD sees the new bit.
    /// On the Z80 SIO that matches real RR0 (bit3 = DCD asserted); on the 6850
    /// the real polarity is carrier-*loss*-active, but active-high keeps the
    /// idle byte stable and is what the few CP/M ACIA programs that read DCD
    /// expect from our modem.
    pub fn status(self, rx_ready: bool, tx_ready: bool, carrier: bool) -> u8 {
        match self {
            // SIO RR0: bit0 RX available, bit2 TX buffer empty, bit3 DCD.
            UartFamily::Sio => {
                (if rx_ready { 0x01 } else { 0 })
                    | (if tx_ready { 0x04 } else { 0 })
                    | (if carrier { 0x08 } else { 0 })
            }
            // 6850: bit0 RDRF (RX), bit1 TDRE (TX ready), bit2 DCD.
            UartFamily::Acia => {
                (if rx_ready { 0x01 } else { 0 })
                    | (if tx_ready { 0x02 } else { 0 })
                    | (if carrier { 0x04 } else { 0 })
            }
            // Active-low: bit0 SET means "no RX"; bit7 SET means "TX not
            // ready" (bit7 clear = TX ready).  No DCD bit for this layout.
            // TX-ready is honoured here too so the flow-control invariant
            // (transmit-not-ready when the ring is full) holds for 88-SIO.
            UartFamily::Sio88 => {
                (if rx_ready { 0x00 } else { 0x01 }) | (if tx_ready { 0x00 } else { 0x80 })
            }
        }
    }
}

/// A resolved UART placement: where the two registers live and how status is
/// encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UartProfile {
    pub status_port: u8,
    pub data_port: u8,
    pub family: UartFamily,
}

/// How the guest reaches the virtual modem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModemAccess {
    /// No virtual modem.
    Off,
    /// Direct UART port I/O at the given placement.
    Ports(UartProfile),
    /// The CP/M BDOS `AUX:` device (functions 3/4).
    Aux,
}

/// One selectable choice: the config value, a human description for the UIs,
/// and how the guest reaches the modem.
pub struct UartChoice {
    /// Canonical config value (stored in `egateway.conf`).
    pub key: &'static str,
    /// One-line description shown next to the selection in every UI.
    pub description: &'static str,
    pub access: ModemAccess,
}

const fn ports(status_port: u8, data_port: u8, family: UartFamily) -> ModemAccess {
    ModemAccess::Ports(UartProfile { status_port, data_port, family })
}

/// Every selectable virtual-modem port, in UI display order.  Single source of
/// truth for config validation and all three UIs.
pub const UART_CHOICES: &[UartChoice] = &[
    UartChoice { key: "off", description: "Off — no virtual modem", access: ModemAccess::Off },
    UartChoice {
        key: "rc2014_1a",
        description: "RC2014 SIO/2 board 1, ch A — status 0x80 / data 0x81",
        access: ports(0x80, 0x81, UartFamily::Sio),
    },
    UartChoice {
        key: "rc2014_1b",
        description: "RC2014 SIO/2 board 1, ch B (usual AUX:) — 0x82 / 0x83",
        access: ports(0x82, 0x83, UartFamily::Sio),
    },
    UartChoice {
        key: "rc2014_2a",
        description: "RC2014 SIO/2 board 2, ch A — status 0x84 / data 0x85",
        access: ports(0x84, 0x85, UartFamily::Sio),
    },
    UartChoice {
        key: "rc2014_2b",
        description: "RC2014 SIO/2 board 2, ch B — status 0x86 / data 0x87",
        access: ports(0x86, 0x87, UartFamily::Sio),
    },
    UartChoice {
        key: "altair_2sio1",
        description: "Altair 88-2SIO port 1 — status 0x10 / data 0x11",
        access: ports(0x10, 0x11, UartFamily::Acia),
    },
    UartChoice {
        key: "altair_2sio2",
        description: "Altair 88-2SIO port 2 — status 0x12 / data 0x13",
        access: ports(0x12, 0x13, UartFamily::Acia),
    },
    UartChoice {
        key: "altair_sio",
        description: "Altair 88-SIO — status 0x00 / data 0x01",
        access: ports(0x00, 0x01, UartFamily::Sio88),
    },
    UartChoice {
        key: "aux",
        description: "BDOS AUX: device (SC126 / RomWBW, hardware-independent)",
        access: ModemAccess::Aux,
    },
];

/// The default selection (`off`) config value.
pub const DEFAULT_UART: &str = "off";

/// Is `key` a recognised profile value?
pub fn is_valid_uart_key(key: &str) -> bool {
    UART_CHOICES.iter().any(|c| c.key == key)
}

/// Resolve a config value to how the guest reaches the modem.  An unknown key
/// (or `off`) yields [`ModemAccess::Off`].
pub fn resolve_access(key: &str) -> ModemAccess {
    UART_CHOICES
        .iter()
        .find(|c| c.key == key)
        .map(|c| c.access)
        .unwrap_or(ModemAccess::Off)
}

/// The description for a config value (for a UI to show the current setting),
/// or the `off` description if unknown.
pub fn uart_description(key: &str) -> &'static str {
    UART_CHOICES
        .iter()
        .find(|c| c.key == key)
        .map(|c| c.description)
        .unwrap_or(UART_CHOICES[0].description)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_off_is_default() {
        assert_eq!(DEFAULT_UART, "off");
        assert_eq!(resolve_access("off"), ModemAccess::Off);
        assert_eq!(UART_CHOICES[0].key, "off");
    }

    #[test]
    fn test_known_addresses() {
        assert_eq!(
            resolve_access("rc2014_1b"),
            ModemAccess::Ports(UartProfile { status_port: 0x82, data_port: 0x83, family: UartFamily::Sio })
        );
        assert_eq!(
            resolve_access("altair_2sio1"),
            ModemAccess::Ports(UartProfile { status_port: 0x10, data_port: 0x11, family: UartFamily::Acia })
        );
    }

    #[test]
    fn test_aux_choice() {
        assert_eq!(resolve_access("aux"), ModemAccess::Aux);
        assert!(is_valid_uart_key("aux"));
    }

    #[test]
    fn test_unknown_key_is_off() {
        assert!(!is_valid_uart_key("bogus"));
        assert_eq!(resolve_access("bogus"), ModemAccess::Off);
    }

    #[test]
    fn test_status_bytes() {
        // Idle (no RX, TX ready, no carrier): unchanged from the pre-DCD bytes.
        assert_eq!(UartFamily::Sio.status(false, true, false), 0x04);
        assert_eq!(UartFamily::Acia.status(false, true, false), 0x02);
        assert_eq!(UartFamily::Sio88.status(false, true, false), 0x01);
        // A received byte waiting sets the RX-available bit.
        assert_eq!(UartFamily::Sio.status(true, true, false), 0x05); // TX empty + RX avail
        assert_eq!(UartFamily::Acia.status(true, true, false), 0x03); // TDRE + RDRF
        assert_eq!(UartFamily::Sio88.status(true, true, false), 0x00); // active-low: RX ready
    }

    #[test]
    fn test_carrier_bit() {
        // DCD (carrier) is active-high and additive to the idle byte.
        assert_eq!(UartFamily::Sio.status(false, true, true), 0x0C); // TX + DCD(bit3)
        assert_eq!(UartFamily::Acia.status(false, true, true), 0x06); // TDRE + DCD(bit2)
        // Sio88 surfaces no DCD bit.
        assert_eq!(UartFamily::Sio88.status(false, true, true), 0x01);
    }

    #[test]
    fn test_tx_not_ready_clears_tx_bit() {
        // When the TX buffer can't accept a byte, the TX-ready bit is clear.
        assert_eq!(UartFamily::Sio.status(false, false, false), 0x00);
        assert_eq!(UartFamily::Acia.status(false, false, false), 0x00);
        // 88-SIO is active-low: TX-not-ready SETS bit7 (idle 0x01 → 0x81).
        assert_eq!(UartFamily::Sio88.status(false, false, false), 0x81);
        assert_eq!(UartFamily::Sio88.status(true, false, false), 0x80); // RX ready + TX busy
    }

    #[test]
    fn test_all_keys_unique() {
        let mut keys = std::collections::HashSet::new();
        for c in UART_CHOICES {
            assert!(keys.insert(c.key), "duplicate key {}", c.key);
            assert!(!c.description.is_empty());
        }
    }
}
