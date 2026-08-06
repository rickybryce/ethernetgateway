//! RomWBW **HBIOS** character-I/O calls for the emulated CP/M machine.
//!
//! Some CP/M comms software is built for RomWBW rather than for a bare
//! machine: instead of poking a UART at a fixed I/O port, it asks the firmware
//! to move the byte.  The QTERM `h` builds are the example that motivated this
//! — they reach the serial device with `RST 8`, a function number in `B` and a
//! unit number in `C`, and never touch a port at all.  On a port-I/O profile
//! (or `aux`) such a program hangs before it prints anything, because its very
//! first call goes nowhere.
//!
//! This module answers the **serial (character device) group** of that API for
//! one unit — the virtual modem — plus the two management calls a program uses
//! to check what it is talking to.  It is written from the published HBIOS
//! interface description (function numbers, which register carries what, and
//! the result convention); no RomWBW code is used, and the implementation is
//! entirely our own dispatch over the modem rings in
//! [`crate::cpm::machine`].
//!
//! It also answers the **RTC group's** read call, because CP/M 2.2 has no clock
//! of its own and RomWBW software asks the firmware for one; the time comes
//! from the host, and setting it is refused (see `FN_RTC_SETTIM`).
//!
//! Deliberately **not** implemented: bank switching / memory management, disk,
//! video, sound, and the DSKY.  Those are RomWBW *hardware* services with
//! no counterpart here, and pretending otherwise would strand a program deeper
//! in its run than an honest refusal does.  Every function outside the
//! supported set returns [`ERR`], which the API defines as a failure (any
//! non-zero / negative result), so a caller finds out at the call rather than
//! by corrupted behaviour later.  This is an emulation of one API group, not a
//! RomWBW system.
//!
//! ## Blocking calls
//!
//! `IN` waits for a character and `OUT` waits for room to send one.  Rather
//! than spin the emulator inside a handler, an unready call is left
//! *unanswered*: the driver simply runs the CPU again, and because the guest's
//! PC is still parked on the `RST 8` trap the call is re-reported next batch —
//! after the driver's normal seam work (service the modem, drain the wire,
//! check for the `ESC ESC` break-out) has had a turn.  The guest sees one
//! blocking call; the host stays responsive and interruptible.

use super::Cpm;

/// Serial: wait for a character, return it in `E`.
const FN_IN: u8 = 0x00;
/// Serial: wait until the device can accept the character in `E`, then send it.
const FN_OUT: u8 = 0x01;
/// Serial: input status — how many characters are waiting.
const FN_IST: u8 = 0x02;
/// Serial: output status — how much output buffer space is free.
const FN_OST: u8 = 0x03;
/// Serial: initialise the unit to the line characteristics in `DE`.
const FN_INITDEV: u8 = 0x04;
/// Serial: report the unit's current line characteristics.
const FN_QUERY: u8 = 0x05;
/// Serial: describe the unit's hardware.
const FN_DEVICE: u8 = 0x06;
/// Management: report the firmware version.
const FN_VER: u8 = 0xF1;
/// Management: the `GET` group, whose sub-function is in `C`.
const FN_GET: u8 = 0xF8;
/// `GET` sub-function: count of serial units.
const GET_CIOCNT: u8 = 0x00;
/// `GET` sub-function: count of real-time-clock units.
const GET_RTCCNT: u8 = 0x20;
/// RTC: read the clock into the six-byte buffer at `HL`.
const FN_RTC_GETTIM: u8 = 0x20;
/// RTC: set the clock from the six-byte buffer at `HL`.
const FN_RTC_SETTIM: u8 = 0x21;

/// Our result code for anything we do not implement.  The API treats a
/// non-zero (negative) result as a failure; `0xFF` is the plainest such value
/// and is ours, not a borrowed constant.
const ERR: u8 = 0xFF;
/// Success.
const OK: u8 = 0x00;

/// "Reset the unit using its current settings" — the value RomWBW software
/// passes in `DE` when it wants initialisation without changing the line.
const LINE_KEEP: u16 = 0xFFFF;

/// Line characteristics we report before anything sets them.  The encoding
/// packs the baud rate and handshake in `D` and the framing in `E`; the value
/// here reads as 8 data bits, 1 stop bit, no parity, no flow control, which is
/// what the virtual modem behaves like.  A guest that sets its own value gets
/// that value back from `QUERY` — we keep it verbatim rather than
/// re-interpreting a rate no real UART is clocking here.
const LINE_DEFAULT: u16 = 0x0000;

/// Device type we report for the virtual modem.  Every value in the published
/// list names a specific chip driver, none of which is running here, so we
/// report one that is *not* in that list.
///
/// It used to report 0 on the reasoning that zero meant "no driver" — but 0 is
/// `CIODEV_UART`, the 16C550 driver, so the answer read as a definite claim to
/// be a chip we are not.  Nothing caught it until EGT80 started *displaying*
/// what this call returns, at which point the emulator's own modem listed
/// itself as "UART base 00".  An out-of-range type is the honest answer: a
/// program that decodes it learns "not one of the known drivers", which is
/// exactly the situation.  The RS-232 attribute a comms program actually cares
/// about is still set, and the base I/O address stays 0 — there is no port.
const DEVICE_TYPE_NONE: u8 = 0xFF;
/// Serial device attributes: bit 7 clear = RS-232 (rather than a terminal).
const DEVICE_ATTR_RS232: u8 = 0x00;
/// Device mode: no chip variant to report.
const DEVICE_MODE_NONE: u8 = 0x00;
/// Base I/O address: none — this unit is not at a port.
const DEVICE_BASE_NONE: u8 = 0x00;

/// The API generation this implements, reported by `VER` as Maj/Min/Upd/Pat in
/// `DE`.  Platform id (`L`) is 0: we are not any of the real RomWBW platforms,
/// and a program that switches on platform id is about to do something
/// hardware-specific that would not work here anyway.
const VER_DE: u16 = 0x0306;
const VER_PLATFORM: u8 = 0x00;
/// Terminal type reported by `QUERY` in `L`.  A different field from the
/// platform id above, even though both happen to be zero: this one says "no
/// particular terminal", which is the truth for a virtual modem whose far end
/// is whatever dialled in.
const TERM_TYPE_NONE: u8 = 0x00;

/// What the driver should do after [`service`] looked at an HBIOS call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HbiosOutcome {
    /// The call was answered (the guest has been resumed past it).
    Answered,
    /// A blocking call whose device is not ready.  The call was left
    /// unanswered; run the CPU again after the seam and it will be re-reported.
    Waiting,
}

/// Was this call an input-status poll that answered "nothing waiting"?
///
/// Used by the async driver to recognise a guest's idle loop and pace it.  A
/// RomWBW comms program's wait-for-activity loop polls this function, and
/// because every HBIOS call ends the CPU batch, each turn costs a full driver
/// pass; with nothing slowing those passes down, an idle session spins the host
/// at over a core (measured 164% before this was recognised).
///
/// Deliberately does *not* check that the unit is ours: a guest spinning on a
/// call that gets refused is just as idle as one spinning on an empty ring, and
/// pacing it is just as correct.
///
/// Kept as a predicate rather than a new [`HbiosOutcome`] variant so that
/// "answered" keeps its single meaning at every existing call site — this asks
/// a different question (was there anything to report?) from the one the
/// outcome answers (did the guest get resumed?).
pub fn is_idle_status_poll(cpm: &Cpm, func: u8) -> bool {
    func == FN_IST && cpm.modem_rx_len() == 0
}

/// Service one [`Stop::Hbios`] call against the virtual modem.
///
/// Synchronous by design — every supported function is a ring operation or a
/// constant, so the async driver needs no special handling beyond honouring
/// [`HbiosOutcome::Waiting`].
pub fn service(cpm: &mut Cpm, func: u8) -> HbiosOutcome {
    // Which unit this build of the guest software was patched to use.  A call
    // for any other unit is refused, exactly as a program built for the wrong
    // port address finds nothing at that address: the failure points at the
    // profile, which is what makes it diagnosable.
    let our_unit = cpm.hbios_unit();
    let unit = cpm.arg_hbios_unit();

    // No HBIOS access mode selected: refuse everything, including the
    // management group.  The `RST 8` vector is only installed for an
    // `hbios_*` profile, but the trap address itself is always live — a guest
    // that reaches it another way (a `CALL` straight at it, or a stray jump)
    // must not be told a RomWBW system is present, because on a port profile
    // one is not.  Answering `VER` here would make the emulator lie to any
    // program that probes before deciding how to reach its modem.
    if our_unit.is_none() {
        cpm.hbios_return(ERR);
        return HbiosOutcome::Answered;
    }

    // The management group is unit-independent.
    match func {
        FN_VER => {
            // VER returns L, so HL is left alone here.
            cpm.hbios_return_de_l(OK, VER_DE, VER_PLATFORM);
            return HbiosOutcome::Answered;
        }
        FN_GET => {
            // Sub-function in C.  Only the serial-unit count is meaningful
            // here; the disk / RTC / video counts describe hardware we do not
            // have, so they are refused rather than answered with zero (which
            // a caller would read as a successful "none").
            if unit == GET_CIOCNT {
                // Units 0..=our unit exist as far as the guest can tell; only
                // ours answers.
                let count = our_unit.map(|u| u.saturating_add(1)).unwrap_or(0);
                cpm.hbios_return_e(OK, count);
            } else if unit == GET_RTCCNT {
                // One clock, so software that probes before asking the time
                // finds it.  Answered rather than refused precisely because we
                // do implement RTCGETTIM below.
                cpm.hbios_return_e(OK, 1);
            } else {
                cpm.hbios_return(ERR);
            }
            cpm.hbios_scramble_hl();
            return HbiosOutcome::Answered;
        }
        _ => {}
    }

    // The RTC group is not a character device: RTCGETTIM takes a buffer
    // address in HL and no unit in C, so it is answered here, above the
    // character-unit check that would otherwise reject it.
    match func {
        FN_RTC_GETTIM => {
            let buf = cpm.reg16(iz80::Reg16::HL);
            let now = crate::cpm::host_clock_bcd();
            cpm.write_block(buf, &now);
            cpm.hbios_return(OK);
            // HL is an *entry* parameter here, not a documented return, so it
            // is scrambled like every other such call: an emulator looser than
            // the hardware turns a guest's stale-pointer bug into a field
            // report (see `hbios_scramble_hl`).
            cpm.hbios_scramble_hl();
            return HbiosOutcome::Answered;
        }
        FN_RTC_SETTIM => {
            // Refused, not silently accepted.  The clock here is the host's,
            // and a guest cannot be allowed to set that — nor should it be told
            // it succeeded and then read back a time it did not set.
            cpm.hbios_return(ERR);
            cpm.hbios_scramble_hl();
            return HbiosOutcome::Answered;
        }
        _ => {}
    }

    if our_unit != Some(unit) {
        cpm.hbios_return(ERR);
        return HbiosOutcome::Answered;
    }

    match func {
        FN_IN => match cpm.modem_rx_pop() {
            Some(b) => {
                cpm.hbios_return_e(OK, b);
                cpm.hbios_scramble_hl();
                HbiosOutcome::Answered
            }
            // Nothing yet: park the guest on the call.
            None => HbiosOutcome::Waiting,
        },
        FN_OUT => {
            if cpm.modem_tx_free() == 0 {
                // Ring full — the peer is behind.  Park rather than drop the
                // byte, which is the whole point of a blocking send.
                return HbiosOutcome::Waiting;
            }
            let b = cpm.arg_e();
            cpm.modem_tx_push(b);
            cpm.hbios_return(OK);
            cpm.hbios_scramble_hl();
            HbiosOutcome::Answered
        }
        // IST and OST report a COUNT in A as well as in E, and the flags follow
        // it, so Z means "nothing waiting" / "no room".  The API labels A a
        // result code, but RomWBW's own drivers return the count there (the SIO
        // driver's IST returns 1 when a character is waiting), and callers rely
        // on it: QTERM's overlay hands A straight to a JR Z.  Do not "correct"
        // this to always-zero-on-success.
        FN_IST => {
            // Capped at 0x7F, not 0xFF: the API reserves bit 7 for an error
            // result ("negative values indicate a standard HBIOS result
            // code"), so a count that large would read as a failure to any
            // guest that checks — as one now does, having hung on exactly
            // that ambiguity.  No real driver reports 128 pending bytes
            // either; RomWBW's character buffers are far smaller.
            let pending = cpm.modem_rx_len().min(0x7F) as u8;
            cpm.hbios_return_e(pending, pending);
            cpm.hbios_scramble_hl();
            HbiosOutcome::Answered
        }
        FN_OST => {
            let free = cpm.modem_tx_free().min(0x7F) as u8; // as above: bit 7
            // is an error flag, never a count
            cpm.hbios_return_e(free, free);
            cpm.hbios_scramble_hl();
            HbiosOutcome::Answered
        }
        FN_INITDEV => {
            // There is no UART to program: a TCP connection has no baud rate.
            // Accept the request (a failure here stops software before it
            // starts — it is the first call QTERM's overlay makes) and
            // remember the characteristics so QUERY reports back what was set.
            let de = cpm.arg_de();
            if de != LINE_KEEP {
                cpm.set_hbios_line(de);
            }
            cpm.hbios_return(OK);
            cpm.hbios_scramble_hl();
            HbiosOutcome::Answered
        }
        FN_QUERY => {
            let line = cpm.hbios_line();
            cpm.hbios_return_de_l(OK, line, TERM_TYPE_NONE);
            HbiosOutcome::Answered
        }
        FN_DEVICE => {
            cpm.hbios_return_device(
                OK,
                DEVICE_TYPE_NONE,
                unit,
                DEVICE_ATTR_RS232,
                DEVICE_MODE_NONE,
                DEVICE_BASE_NONE,
            );
            HbiosOutcome::Answered
        }
        // Serial functions beyond the group above, and every other HBIOS
        // group (disk, RTC, video, sound, DSKY, bank management): refused.
        _ => {
            cpm.hbios_return(ERR);
            HbiosOutcome::Answered
        }
    }
}

/// The default line characteristics a freshly created machine reports.
pub fn default_line() -> u16 {
    LINE_DEFAULT
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cpm::uart::resolve_access;
    use crate::cpm::{Cpm, Stop};
    use std::sync::atomic::AtomicBool;

    /// Assemble the smallest program that makes one HBIOS call: set the
    /// function/unit, `RST 8`, then halt on a warm boot.
    fn machine_with_call(func: u8, unit: u8, profile: &str) -> (Cpm, AtomicBool) {
        let mut cpm = Cpm::new();
        cpm.set_modem_access(resolve_access(profile));
        // LD B,func / LD C,unit / RST 8 / JP 0
        cpm.load_com(&[0x06, func, 0x0E, unit, 0xCF, 0xC3, 0x00, 0x00]);
        (cpm, AtomicBool::new(false))
    }

    /// The clock: RTCGETTIM fills the six-byte buffer HL points at, with the
    /// BCD the published interface specifies, and reports success.
    ///
    /// CP/M 2.2 has no clock, so this is the only way an emulated program can
    /// learn the date — and it is what makes `hbios_*` more than a serial port.
    #[test]
    fn test_rtc_get_time_fills_the_buffer_in_bcd() {
        // Constructing a CP/M machine registers a session in the
        // process-global image registry, which makes drives look busy to
        // every other test.  Serialise with the registry tests.
        let _g = crate::cpm::image::registry::tests_lock();
        let mut cpm = Cpm::new();
        cpm.set_modem_access(resolve_access("hbios_1"));
        // LD B,0x20 / LD HL,0x2000 / RST 8 / JP 0
        cpm.load_com(&[0x06, FN_RTC_GETTIM, 0x21, 0x00, 0x20, 0xCF, 0xC3, 0x00, 0x00]);
        let abort = AtomicBool::new(false);
        assert_eq!(cpm.run(100, &abort), Stop::Hbios(FN_RTC_GETTIM));
        assert_eq!(service(&mut cpm, FN_RTC_GETTIM), HbiosOutcome::Answered);
        assert_eq!(cpm.reg8(iz80::Reg8::A), OK, "RTCGETTIM must report success");

        let buf = cpm.read_block(0x2000, 6);
        for (i, b) in buf.iter().enumerate() {
            assert!(
                (b >> 4) <= 9 && (b & 0x0F) <= 9,
                "buffer byte {i} = {b:#04x} is not BCD; the interface says every \
                 byte is BCD encoded"
            );
        }
        let dec = |b: u8| (b >> 4) * 10 + (b & 0x0F);
        assert!((1..=12).contains(&dec(buf[1])), "month byte {:#04x}", buf[1]);
        assert!((1..=31).contains(&dec(buf[2])), "day byte {:#04x}", buf[2]);
        // Nothing was written past the six bytes the buffer is defined to hold.
        assert_eq!(cpm.read_block(0x2006, 1)[0], 0x00, "wrote past the buffer");
        // HL is an entry parameter, not a documented return, so it must come
        // back scrambled like every other such call — being looser than the
        // hardware is what let EGT80's HL bug reach a real machine.
        assert_eq!(
            cpm.reg16(iz80::Reg16::HL),
            0xFFFF,
            "RTCGETTIM must not promise HL survives; see hbios_scramble_hl"
        );
    }

    /// Setting the clock is refused, not silently accepted.  The time here is
    /// the host's; answering OK and then reading back a different time is worse
    /// than saying no.
    #[test]
    fn test_rtc_set_time_is_refused() {
        let (mut cpm, abort) = machine_with_call(FN_RTC_SETTIM, 0, "hbios_1");
        assert_eq!(cpm.run(100, &abort), Stop::Hbios(FN_RTC_SETTIM));
        assert_eq!(service(&mut cpm, FN_RTC_SETTIM), HbiosOutcome::Answered);
        assert_eq!(cpm.reg8(iz80::Reg8::A), ERR, "RTCSETTIM must fail, not lie");
    }

    /// A program that probes for a clock before asking the time must find one —
    /// and must still not find a disk or a video unit.
    #[test]
    fn test_sysget_reports_one_rtc_but_no_disk() {
        let (mut cpm, abort) = machine_with_call(FN_GET, GET_RTCCNT, "hbios_1");
        assert_eq!(cpm.run(100, &abort), Stop::Hbios(FN_GET));
        assert_eq!(service(&mut cpm, FN_GET), HbiosOutcome::Answered);
        assert_eq!(cpm.reg8(iz80::Reg8::A), OK);
        assert_eq!(cpm.reg8(iz80::Reg8::E), 1, "one RTC");

        // 0x10 is the disk-unit count: still refused, because there is no disk.
        let (mut cpm, abort) = machine_with_call(FN_GET, 0x10, "hbios_1");
        assert_eq!(cpm.run(100, &abort), Stop::Hbios(FN_GET));
        assert_eq!(service(&mut cpm, FN_GET), HbiosOutcome::Answered);
        assert_eq!(cpm.reg8(iz80::Reg8::A), ERR, "we have no disk units to report");
    }

    /// With no HBIOS profile selected the machine is a plain CP/M 2.2 one, and
    /// that has no clock either — the RTC group must not become a back door
    /// past the profile gate.
    ///
    /// Reached by CALLing the trap directly, the way
    /// `test_no_hbios_profile_refuses_even_the_management_group` does: on a
    /// port profile the page-zero `RST 8` vector is not installed at all, so an
    /// `RST 8` never arrives here — it wanders off into unused memory until the
    /// instruction ceiling stops it, which is what a bare CP/M machine does.
    #[test]
    fn test_rtc_refused_without_an_hbios_profile() {
        // Constructing a CP/M machine registers a session in the
        // process-global image registry, which makes drives look busy to
        // every other test.  Serialise with the registry tests.
        let _g = crate::cpm::image::registry::tests_lock();
        let mut cpm = Cpm::new();
        cpm.set_modem_access(resolve_access("rc2014_1b")); // a PORT profile
        // LD B,RTCGETTIM / LD HL,2000h / CALL <trap> / JP 0
        cpm.load_com(&[
            0x06, FN_RTC_GETTIM, 0x21, 0x00, 0x20, 0xCD, 0xF0, 0xFF, 0xC3, 0x00, 0x00,
        ]);
        let abort = AtomicBool::new(false);
        assert_eq!(cpm.run(100, &abort), Stop::Hbios(FN_RTC_GETTIM), "trap fires");
        assert_eq!(service(&mut cpm, FN_RTC_GETTIM), HbiosOutcome::Answered);
        assert_eq!(cpm.reg8(iz80::Reg8::A), ERR, "no RomWBW here, so no clock");
        assert_eq!(cpm.read_block(0x2000, 6), vec![0u8; 6], "and wrote nothing");
    }

    #[test]
    fn test_rst8_vector_installed_only_for_hbios_profiles() {
        // Constructing a CP/M machine registers a session in the
        // process-global image registry, which makes drives look busy to
        // every other test.  Serialise with the registry tests.
        let _g = crate::cpm::image::registry::tests_lock();
        let mut cpm = Cpm::new();
        cpm.set_modem_access(resolve_access("rc2014_1b"));
        cpm.load_com(&[0xC9]);
        assert_eq!(cpm.read_block(0x0008, 1)[0], 0x00, "port profile: no vector");
        cpm.set_modem_access(resolve_access("hbios_1"));
        cpm.load_com(&[0xC9]);
        assert_eq!(cpm.read_block(0x0008, 1)[0], 0xC3, "hbios: JP installed");
    }

    #[test]
    fn test_rst8_reaches_the_trap_with_function_in_b() {
        let (mut cpm, abort) = machine_with_call(FN_IST, 1, "hbios_1");
        assert_eq!(cpm.run(100, &abort), Stop::Hbios(FN_IST));
    }

    #[test]
    fn test_ist_reports_pending_count_and_ost_reports_free_space() {
        let (mut cpm, abort) = machine_with_call(FN_IST, 1, "hbios_1");
        cpm.modem_queue_rx(b"abc");
        assert_eq!(cpm.run(100, &abort), Stop::Hbios(FN_IST));
        assert_eq!(service(&mut cpm, FN_IST), HbiosOutcome::Answered);
        assert_eq!(cpm.reg8(iz80::Reg8::A), 3, "3 bytes waiting");
        assert_eq!(cpm.reg8(iz80::Reg8::E), 3, "count mirrored in E");

        let (mut cpm, abort) = machine_with_call(FN_OST, 1, "hbios_1");
        assert_eq!(cpm.run(100, &abort), Stop::Hbios(FN_OST));
        assert_eq!(service(&mut cpm, FN_OST), HbiosOutcome::Answered);
        // Capped at 0x7F, not 0xFF: bit 7 is the API's error flag, so a count
        // that large would read as a failed call.  A guest that checks bit 7
        // (as it must, to tell a wrong unit from a ready one) would otherwise
        // see "error" on a perfectly healthy port.
        assert_eq!(cpm.reg8(iz80::Reg8::A), 0x7F, "free space, capped below the error flag");
        assert_eq!(
            cpm.reg8(iz80::Reg8::A) & 0x80,
            0,
            "a count must never have bit 7 set"
        );
    }

    #[test]
    fn test_in_returns_byte_in_e_and_parks_when_empty() {
        let (mut cpm, abort) = machine_with_call(FN_IN, 1, "hbios_1");
        assert_eq!(cpm.run(100, &abort), Stop::Hbios(FN_IN));
        // Nothing queued: the call is left unanswered and re-reported.
        assert_eq!(service(&mut cpm, FN_IN), HbiosOutcome::Waiting);
        assert_eq!(cpm.run(100, &abort), Stop::Hbios(FN_IN), "PC still parked");
        cpm.modem_queue_rx(b"Q");
        assert_eq!(service(&mut cpm, FN_IN), HbiosOutcome::Answered);
        assert_eq!(cpm.reg8(iz80::Reg8::A), OK);
        assert_eq!(cpm.reg8(iz80::Reg8::E), b'Q');
    }

    #[test]
    fn test_out_sends_e_toward_the_peer() {
        // Constructing a CP/M machine registers a session in the
        // process-global image registry, which makes drives look busy to
        // every other test.  Serialise with the registry tests.
        let _g = crate::cpm::image::registry::tests_lock();
        let mut cpm = Cpm::new();
        cpm.set_modem_access(resolve_access("hbios_2"));
        // LD B,OUT / LD C,2 / LD E,'Z' / RST 8 / JP 0
        cpm.load_com(&[0x06, FN_OUT, 0x0E, 0x02, 0x1E, b'Z', 0xCF, 0xC3, 0x00, 0x00]);
        let abort = AtomicBool::new(false);
        assert_eq!(cpm.run(100, &abort), Stop::Hbios(FN_OUT));
        assert_eq!(service(&mut cpm, FN_OUT), HbiosOutcome::Answered);
        assert_eq!(cpm.reg8(iz80::Reg8::A), OK);
        assert_eq!(cpm.modem_drain_tx(), b"Z");
    }

    #[test]
    fn test_ports_stay_inert_under_an_hbios_profile() {
        // The two access modes must not overlap: with HBIOS selected, the I/O
        // ports belong to whatever hardware the guest thinks is there, and
        // reading them must not drain the modem's rings.
        let mut m = crate::cpm::CpmMachine::new();
        m.set_access(resolve_access("hbios_1"));
        m.modem_queue_rx(b"xy");
        use iz80::Machine;
        // `0xFF` is what "inert" means on a bus: nothing drives those lines, so
        // they float high. Zero would be a *plausible* SIO status — an idle,
        // present board — which is the opposite of what this asserts. See
        // `CpmMachine::port_in`.
        assert_eq!(m.port_in(0x82), 0xFF, "SIO status stays inert");
        assert_eq!(m.port_in(0x83), 0xFF, "SIO data must not drain the ring");
        m.port_out(0x83, b'Z');
        assert!(m.modem_drain_tx().is_empty(), "a port write reaches nothing");
        assert_eq!(m.modem_rx_len(), 2, "ring untouched");
    }

    #[test]
    fn test_wrong_unit_is_refused() {
        // A unit-1 build (qtermh1) under the unit-2 profile finds nothing —
        // the HBIOS equivalent of the wrong port address.
        let (mut cpm, abort) = machine_with_call(FN_IST, 1, "hbios_2");
        assert_eq!(cpm.run(100, &abort), Stop::Hbios(FN_IST));
        assert_eq!(service(&mut cpm, FN_IST), HbiosOutcome::Answered);
        assert_eq!(cpm.reg8(iz80::Reg8::A), ERR);
    }

    #[test]
    fn test_initdev_accepts_and_query_reports_back() {
        // Constructing a CP/M machine registers a session in the
        // process-global image registry, which makes drives look busy to
        // every other test.  Serialise with the registry tests.
        let _g = crate::cpm::image::registry::tests_lock();
        let mut cpm = Cpm::new();
        cpm.set_modem_access(resolve_access("hbios_1"));
        // LD B,INITDEV / LD C,1 / LD DE,-1 / RST 8 / JP 0  (the "keep current
        // settings" call QTERM's RomWBW overlay makes at startup)
        cpm.load_com(&[0x06, FN_INITDEV, 0x0E, 0x01, 0x11, 0xFF, 0xFF, 0xCF, 0xC3, 0x00, 0x00]);
        let abort = AtomicBool::new(false);
        assert_eq!(cpm.run(100, &abort), Stop::Hbios(FN_INITDEV));
        assert_eq!(service(&mut cpm, FN_INITDEV), HbiosOutcome::Answered);
        assert_eq!(cpm.reg8(iz80::Reg8::A), OK, "startup init must succeed");
        assert_eq!(cpm.hbios_line(), default_line(), "-1 keeps current line");

        // An explicit setting round-trips through QUERY.
        cpm.set_hbios_line(0x1234);
        assert_eq!(service(&mut cpm, FN_QUERY), HbiosOutcome::Answered);
        assert_eq!(cpm.reg16(iz80::Reg16::DE), 0x1234);
    }

    #[test]
    fn test_device_describes_a_virtual_unit() {
        let (mut cpm, abort) = machine_with_call(FN_DEVICE, 1, "hbios_1");
        assert_eq!(cpm.run(100, &abort), Stop::Hbios(FN_DEVICE));
        assert_eq!(service(&mut cpm, FN_DEVICE), HbiosOutcome::Answered);
        assert_eq!(cpm.reg8(iz80::Reg8::A), OK);
        assert_eq!(cpm.reg8(iz80::Reg8::D), DEVICE_TYPE_NONE, "no real driver");
        assert_eq!(cpm.reg8(iz80::Reg8::E), 1, "our unit number");
        assert_eq!(cpm.reg8(iz80::Reg8::L), DEVICE_BASE_NONE, "not at a port");
    }

    #[test]
    fn test_ver_and_unit_count_are_answered() {
        let (mut cpm, abort) = machine_with_call(FN_VER, 0, "hbios_1");
        assert_eq!(cpm.run(100, &abort), Stop::Hbios(FN_VER));
        assert_eq!(service(&mut cpm, FN_VER), HbiosOutcome::Answered);
        assert_eq!(cpm.reg8(iz80::Reg8::A), OK);
        assert_eq!(cpm.reg16(iz80::Reg16::DE), VER_DE);

        let (mut cpm, abort) = machine_with_call(FN_GET, GET_CIOCNT, "hbios_2");
        assert_eq!(cpm.run(100, &abort), Stop::Hbios(FN_GET));
        assert_eq!(service(&mut cpm, FN_GET), HbiosOutcome::Answered);
        assert_eq!(cpm.reg8(iz80::Reg8::A), OK);
        assert_eq!(cpm.reg8(iz80::Reg8::E), 3, "units 0..2 as far as it can see");
    }

    #[test]
    fn test_no_hbios_profile_refuses_even_the_management_group() {
        // Constructing a CP/M machine registers a session in the
        // process-global image registry, which makes drives look busy to
        // every other test.  Serialise with the registry tests.
        let _g = crate::cpm::image::registry::tests_lock();
        // The trap address is always live, so a guest can reach it without the
        // page-zero vector (a CALL straight at it, or a stray jump).  On a port
        // profile there is no RomWBW system, and answering VER would tell a
        // program that probes before choosing how to reach its modem exactly
        // the wrong thing.
        let mut cpm = Cpm::new();
        cpm.set_modem_access(resolve_access("rc2014_1b")); // a PORT profile
        // LD B,VER / CALL <trap> / JP 0
        cpm.load_com(&[0x06, FN_VER, 0xCD, 0xF0, 0xFF, 0xC3, 0x00, 0x00]);
        let abort = AtomicBool::new(false);
        assert_eq!(cpm.run(100, &abort), Stop::Hbios(FN_VER), "trap still fires");
        assert_eq!(service(&mut cpm, FN_VER), HbiosOutcome::Answered);
        assert_eq!(cpm.reg8(iz80::Reg8::A), ERR, "must not claim RomWBW");
        // The serial group too, not just management.
        for func in [FN_IST, FN_IN, FN_OUT, FN_DEVICE] {
            let mut cpm = Cpm::new();
            cpm.set_modem_access(resolve_access("altair_2sio1"));
            cpm.load_com(&[0x06, func, 0x0E, 0x01, 0xCD, 0xF0, 0xFF, 0xC3, 0x00, 0x00]);
            let abort = AtomicBool::new(false);
            assert_eq!(cpm.run(100, &abort), Stop::Hbios(func));
            assert_eq!(service(&mut cpm, func), HbiosOutcome::Answered);
            assert_eq!(cpm.reg8(iz80::Reg8::A), ERR, "func {func:#04x}");
        }
    }

    #[test]
    fn test_unsupported_groups_are_refused_not_faked() {
        // Disk status (0x10), a bank switch (0xF2) and an unknown GET
        // sub-function must all fail rather than return a plausible success.
        for (func, unit) in [(0x10u8, 1u8), (0xF2, 1), (FN_GET, 0x10)] {
            let (mut cpm, abort) = machine_with_call(func, unit, "hbios_1");
            assert_eq!(cpm.run(100, &abort), Stop::Hbios(func));
            assert_eq!(service(&mut cpm, func), HbiosOutcome::Answered);
            assert_eq!(cpm.reg8(iz80::Reg8::A), ERR, "func {func:#04x} must error");
        }
    }

    #[test]
    fn test_answered_call_resumes_after_the_rst() {
        // The `RST 8` return address must be popped exactly once: the guest
        // continues with the instruction after it (here `JP 0` → warm boot).
        let (mut cpm, abort) = machine_with_call(FN_IST, 1, "hbios_1");
        assert_eq!(cpm.run(100, &abort), Stop::Hbios(FN_IST));
        service(&mut cpm, FN_IST);
        assert_eq!(cpm.run(100, &abort), Stop::WarmBoot);
    }
}

