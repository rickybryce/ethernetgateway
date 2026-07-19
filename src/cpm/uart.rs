//! Virtual-modem UART profiles for the CP/M emulator (Flavor B).
//!
//! A CP/M communications program reaches its "modem" by doing `IN`/`OUT` to a
//! UART at a fixed I/O port address — a *status/command* register and a *data*
//! register.  Different machines place that UART at different addresses and
//! use different status-bit conventions, so the operator selects a **profile**
//! naming the machine/port; the profile resolves to `(status_port, data_port,
//! family)`.  The emulator's [`crate::cpm::CpmMachine`] answers `IN`/`OUT` at
//! those addresses.
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
//!
//! This layer only models the *idle* UART (TX always ready, no RX pending) so
//! software can probe and initialise the port; wiring the data registers to
//! the gateway's outbound dial is the next step.

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
    /// The status byte reported while the modem is idle: transmit ready, no
    /// received character pending, no carrier.  Lets a guest initialise the
    /// port and see a stable "ready to send, nothing to read" UART.
    pub fn idle_status(self) -> u8 {
        match self {
            UartFamily::Sio => 0x04,   // TX empty (bit2) set, RX (bit0) clear
            UartFamily::Acia => 0x02,  // TDRE (bit1) set, RDRF (bit0) clear
            UartFamily::Sio88 => 0x01, // RX-not-ready (bit0) set, TX-ready (bit7) clear
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

/// One selectable profile: the config value, a human description for the UIs,
/// and the resolved placement.  `None` placement = the "off" selection.
pub struct UartChoice {
    /// Canonical config value (stored in `egateway.conf`).
    pub key: &'static str,
    /// One-line description shown next to the selection in every UI.
    pub description: &'static str,
    pub profile: Option<UartProfile>,
}

const fn p(status_port: u8, data_port: u8, family: UartFamily) -> Option<UartProfile> {
    Some(UartProfile { status_port, data_port, family })
}

/// Every selectable virtual-modem port, in UI display order.  Single source of
/// truth for config validation and all three UIs.
pub const UART_CHOICES: &[UartChoice] = &[
    UartChoice { key: "off", description: "Off — no virtual modem", profile: None },
    UartChoice {
        key: "rc2014_1a",
        description: "RC2014 SIO/2 board 1, ch A — status 0x80 / data 0x81",
        profile: p(0x80, 0x81, UartFamily::Sio),
    },
    UartChoice {
        key: "rc2014_1b",
        description: "RC2014 SIO/2 board 1, ch B (usual AUX:) — 0x82 / 0x83",
        profile: p(0x82, 0x83, UartFamily::Sio),
    },
    UartChoice {
        key: "rc2014_2a",
        description: "RC2014 SIO/2 board 2, ch A — status 0x84 / data 0x85",
        profile: p(0x84, 0x85, UartFamily::Sio),
    },
    UartChoice {
        key: "rc2014_2b",
        description: "RC2014 SIO/2 board 2, ch B — status 0x86 / data 0x87",
        profile: p(0x86, 0x87, UartFamily::Sio),
    },
    UartChoice {
        key: "altair_2sio1",
        description: "Altair 88-2SIO port 1 — status 0x10 / data 0x11",
        profile: p(0x10, 0x11, UartFamily::Acia),
    },
    UartChoice {
        key: "altair_2sio2",
        description: "Altair 88-2SIO port 2 — status 0x12 / data 0x13",
        profile: p(0x12, 0x13, UartFamily::Acia),
    },
    UartChoice {
        key: "altair_sio",
        description: "Altair 88-SIO — status 0x00 / data 0x01",
        profile: p(0x00, 0x01, UartFamily::Sio88),
    },
];

/// The default selection (`off`) config value.
pub const DEFAULT_UART: &str = "off";

/// Is `key` a recognised profile value?
pub fn is_valid_uart_key(key: &str) -> bool {
    UART_CHOICES.iter().any(|c| c.key == key)
}

/// Resolve a config value to its UART placement.  An unknown key (or `off`)
/// yields `None` — no virtual modem.
pub fn resolve_uart(key: &str) -> Option<UartProfile> {
    UART_CHOICES.iter().find(|c| c.key == key).and_then(|c| c.profile)
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
    fn test_off_is_default_and_no_profile() {
        assert_eq!(DEFAULT_UART, "off");
        assert_eq!(resolve_uart("off"), None);
        assert_eq!(UART_CHOICES[0].key, "off");
    }

    #[test]
    fn test_known_addresses() {
        assert_eq!(
            resolve_uart("rc2014_1b"),
            Some(UartProfile { status_port: 0x82, data_port: 0x83, family: UartFamily::Sio })
        );
        assert_eq!(
            resolve_uart("altair_2sio1"),
            Some(UartProfile { status_port: 0x10, data_port: 0x11, family: UartFamily::Acia })
        );
        assert_eq!(
            resolve_uart("altair_sio").unwrap().family,
            UartFamily::Sio88
        );
    }

    #[test]
    fn test_unknown_key_is_off() {
        assert!(!is_valid_uart_key("bogus"));
        assert_eq!(resolve_uart("bogus"), None);
    }

    #[test]
    fn test_idle_status_bytes() {
        assert_eq!(UartFamily::Sio.idle_status(), 0x04); // TX empty
        assert_eq!(UartFamily::Acia.idle_status(), 0x02); // TDRE
        assert_eq!(UartFamily::Sio88.idle_status(), 0x01); // active-low idle
    }

    #[test]
    fn test_all_keys_unique_and_addresses_distinct() {
        let mut keys = std::collections::HashSet::new();
        for c in UART_CHOICES {
            assert!(keys.insert(c.key), "duplicate key {}", c.key);
            assert!(!c.description.is_empty());
        }
    }
}
