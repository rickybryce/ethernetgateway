//! Telnet server — session menu, file transfer (XMODEM/YMODEM/ZMODEM/
//! Kermit), SSH gateway, AI chat, web browser, weather, modem emulator.
//!
//! Listens on a configurable port and supports three terminal types: ANSI
//! (modern terminals), ASCII (no color), and PETSCII (Commodore 64). Terminal
//! type is auto-detected by asking the client to press backspace and examining
//! the byte sent (0x14 = PETSCII, 0x08/0x7F = ANSI, other = ASCII).
//!
//! The server operates in character-at-a-time mode (server-side echo) for
//! compatibility with vintage hardware. All visible text fits within 40 columns
//! for PETSCII terminals; ANSI/ASCII separators use 56 columns.

// The telnet option handler has several `ARM if opt == FOO =>` arms whose
// bodies are plain `if body-check { … }` blocks without an else branch.
// Clippy (Rust 1.95+) suggests collapsing the inner `if` into an additional
// guard on the outer match arm.  We deliberately don't, because for the
// option-specific arms (STATUS / TIMING-MARK handling) a false guard would
// fall through to the generic `DO =>` / `DONT =>` / `WILL =>` arm and emit
// the opposite telnet response.  The current style is preserved for
// behavioural clarity.
#![allow(clippy::collapsible_match)]

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::io::Read;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::config;
use crate::logger::glog;

// ─── Submodules (split out of the original monolithic telnet.rs) ───
mod colors;
pub(crate) use colors::{swap_case_for_petscii, petscii_to_ascii_byte, to_latin1_bytes};
mod weather;
// Weather free helpers/types are referenced only from tests; re-export under
// cfg(test) so the non-test build doesn't see an unused re-export.
#[cfg(test)]
pub(crate) use weather::{GeoResult, WeatherUnits, resolve_weather_units, format_temp,
    format_wind, validate_weather_location, split_location_query, pick_geo_result,
    parse_geo_results};

// ─── Telnet protocol (RFC 854/855) ──────────────────────────
const IAC: u8 = 0xFF;
const SE: u8 = 0xF0;
const BRK: u8 = 0xF3;
const IP: u8 = 0xF4;
const AYT: u8 = 0xF6;
/// Erase Character (RFC 854): delete the last received character.
const EC: u8 = 0xF7;
/// Erase Line (RFC 854): delete the current input line.
const EL: u8 = 0xF8;
const SB: u8 = 0xFA;
const WILL: u8 = 0xFB;
const WONT: u8 = 0xFC;
const DO: u8 = 0xFD;
const DONT: u8 = 0xFE;

/// Synthetic byte returned by the IAC parser when it receives IAC EL.
/// Upstream line-editors treat it as "erase the current line."  0x15 is
/// ASCII NAK (Ctrl-U), the conventional line-kill key on Unix.
const LINE_ERASE_BYTE: u8 = 0x15;

/// Maximum subnegotiation body size.  A remote peer could in theory send
/// an arbitrarily large `IAC SB <opt> ... IAC SE` payload and drive our
/// memory use unbounded before the terminating `IAC SE` arrived.  Real
/// telnet subnegotiations (TTYPE, NAWS, NEW-ENVIRON) are at most a few
/// hundred bytes; 8 KiB is a comfortable overestimate.  Bytes beyond
/// this cap are dropped but the state machine keeps scanning for
/// `IAC SE` so it doesn't desync.
const MAX_SB_BODY_BYTES: usize = 8192;

/// Maximum time to wait for the next byte *once a subnegotiation has begun*.
/// A peer that sends `IAC SB` and then dribbles bytes slowly (or never sends
/// the terminating `IAC SE`) would otherwise pin the reader task
/// indefinitely — a slowloris-style stall.  Real subnegotiations arrive in a
/// single burst, so 15s is a generous ceiling.  This bounds only the in-SB
/// reads; the outer wait for the next command/data byte stays unbounded so a
/// legitimately idle interactive session is never disconnected here.
const SB_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

// Telnet options
const OPT_ECHO: u8 = 0x01;
const OPT_SGA: u8 = 0x03;
/// RFC 859 — Status.
const OPT_STATUS: u8 = 0x05;
/// RFC 860 — Timing Mark.
const OPT_TIMING_MARK: u8 = 0x06;
const OPT_TTYPE: u8 = 0x18;
const OPT_NAWS: u8 = 0x1F;

/// STATUS subnegotiation keywords (RFC 859).
const STATUS_IS: u8 = 0x00;
const STATUS_SEND: u8 = 0x01;

// TTYPE subnegotiation (RFC 1091)
const TTYPE_IS: u8 = 0x00;
const TTYPE_SEND: u8 = 0x01;

// ─── ANSI escape codes ──────────────────────────────────────
const ANSI_GREEN: &str = "\x1b[1;32m";
const ANSI_RED: &str = "\x1b[1;31m";
const ANSI_CYAN: &str = "\x1b[1;36m";
const ANSI_YELLOW: &str = "\x1b[1;33m";
const ANSI_AMBER: &str = "\x1b[33m";
const ANSI_BLUE: &str = "\x1b[1;34m";
const ANSI_WHITE: &str = "\x1b[1;37m";
const ANSI_DIM: &str = "\x1b[37m";
const ANSI_RESET: &str = "\x1b[0m";
const ANSI_CLEAR: &str = "\x1b[2J\x1b[H";

// ─── PETSCII color codes ────────────────────────────────────
const PETSCII_GREEN: u8 = 0x1E;
const PETSCII_RED: u8 = 0x96;
const PETSCII_CYAN: u8 = 0x9F;
const PETSCII_YELLOW: u8 = 0x9E;
const PETSCII_LIGHT_BLUE: u8 = 0x9A;
const PETSCII_WHITE: u8 = 0x05;
const PETSCII_LIGHT_GRAY: u8 = 0x9B;
const PETSCII_CLEAR: u8 = 0x93;
const PETSCII_DEFAULT: u8 = PETSCII_LIGHT_GRAY;

const PETSCII_WIDTH: usize = 40;
const MAX_INPUT_LENGTH: usize = 1024;
/// Max server addresses listed on the Server Configuration screen.  The
/// detected-IP list is otherwise unbounded, which on a multi-homed host
/// pushed the PETSCII menu past the 22-row C64 budget; capping it keeps
/// the screen bounded (see `test_server_config_menu_row_count`).
const SERVER_ADDR_DISPLAY_CAP: usize = 3;
const MAX_AUTH_ATTEMPTS: u32 = 3;
/// Per-IP ban window after `MAX_AUTH_ATTEMPTS` failures.  `pub(crate)` so the
/// slave reconnect loop's auth-backoff (serial.rs §9 #14) can be tested to
/// exceed it — a shorter backoff would let a wrong-credential slave lock its
/// own IP out.
pub(crate) const LOCKOUT_DURATION: std::time::Duration =
    std::time::Duration::from_secs(5 * 60);

// ─── Terminal Type ──────────────────────────────────────────
#[derive(Debug, Clone, Copy, PartialEq)]
enum TerminalType {
    Ascii,
    Ansi,
    Petscii,
}

/// Transfer protocol selected at upload time.  The XMODEM/YMODEM
/// branch hands off to `xmodem_receive`, which auto-detects block
/// size (SOH vs STX), CRC vs checksum, and the YMODEM block-0
/// filename header.  The ZMODEM branch hands off to
/// `zmodem_receive`, which emits ZRINIT and waits for ZFILE.
#[derive(Debug, Clone, Copy, PartialEq)]
enum UploadProtocol {
    /// XMODEM / YMODEM — receiver auto-detects variant.
    XmodemYmodem,
    /// ZMODEM — receiver initiates the session with ZRINIT.
    Zmodem,
    /// Kermit — receiver waits for the peer's Send-Init; flavor
    /// (C-Kermit, G-Kermit, etc.) is auto-detected from the peer's
    /// CAPAS bits and surfaced in the post-transfer summary.
    Kermit,
    /// Punter (C1) — the protocol CCGMS / Novaterm speak natively on
    /// Commodore BBSes.  Receiver drives the GOO/ACK/S-B handshake and
    /// records the sender's declared PRG/SEQ file type.
    Punter,
}

/// Transfer protocol selected at download time by the user.  Picked
/// per-transfer via the `SELECT PROTOCOL` prompt; no persistent config.
#[derive(Debug, Clone, Copy, PartialEq)]
enum DownloadProtocol {
    /// Classic XMODEM — 128-byte SOH blocks, CRC-16 with checksum fallback.
    Xmodem,
    /// XMODEM-1K — 1024-byte STX blocks (with SOH fallback for the
    /// final partial block).  Opportunistically falls back to plain
    /// XMODEM if the receiver NAKs the first STX.
    Xmodem1k,
    /// YMODEM — block 0 with filename + size, then 1K-style data
    /// blocks.
    Ymodem,
    /// ZMODEM — Forsberg ZMODEM with ZDLE escaping, hex + binary
    /// headers, stop-and-wait 1K subpackets.
    Zmodem,
    /// Kermit — full-spec Kermit with negotiated long packets,
    /// sliding window, streaming, and attribute packets per the
    /// peer's CAPAS bits.
    Kermit,
    /// Punter (C1) — Commodore BBS protocol; sender drives the
    /// ACK/block handshake.  File type (PRG/SEQ) is auto-detected from
    /// the file and overridable by the user.
    Punter,
}

// ─── Input mode ────────────────────────────────────────────
#[derive(Clone, Copy)]
enum InputMode {
    /// Normal line input: echo typed characters, trim result.
    Normal,
    /// Password input: echo `*` for each character, no trim.
    Password,
}

/// Outcome of `TelnetSession::save_received_file`.  Used by every
/// batch-upload save loop (ZMODEM autostart, Kermit server, ZMODEM /
/// Kermit batch upload) so each site can map the result to its own
/// "skipped: already exists" / "skipped: write failed" wording.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SaveError {
    /// A file with the target name already exists.  Caller decides
    /// whether that's a hard error (interactive single-file upload)
    /// or a per-file skip (batch / autostart / server).
    AlreadyExists,
    /// I/O error other than `AlreadyExists` — disk full, permission
    /// denied, mid-write failure.  Best treated as "skip this file."
    WriteFailed,
}

// ─── Menu ───────────────────────────────────────────────────
#[derive(Clone, Debug, PartialEq)]
enum Menu {
    Main,
    FileTransfer,
    Browser,
}

/// Result of one Kermit-settings page render.  `Switch` means the user
/// pressed the cross-page nav key (M from Status → menu, V from Menu →
/// status); `Back` returns to the calling File Transfer menu.  Used by
/// `kermit_settings` to drive the two-page split that keeps each screen
/// within the 22-row × 40-col PETSCII budget.
enum KermitPageNav {
    Switch,
    Back,
}

impl Menu {
    fn path(&self) -> &'static str {
        match self {
            Menu::Main => "ethernet",
            Menu::FileTransfer => "ethernet/xfer",
            Menu::Browser => "ethernet/web",
        }
    }
}

// ─── Auth lockout ───────────────────────────────────────────
//
// The same `LockoutMap` is shared between the telnet server and the SSH
// server so that a brute-force attacker cannot simply bounce between
// protocols to reset their counter.  A single successful auth on either
// protocol clears the lockout for that IP.
pub(crate) type LockoutMap = Arc<Mutex<HashMap<IpAddr, (u32, std::time::Instant)>>>;

pub(crate) fn is_locked_out(lockouts: &LockoutMap, ip: IpAddr) -> bool {
    let map = lockouts.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((count, when)) = map.get(&ip) {
        *count >= MAX_AUTH_ATTEMPTS && when.elapsed() < LOCKOUT_DURATION
    } else {
        false
    }
}

pub(crate) fn record_auth_failure(lockouts: &LockoutMap, ip: IpAddr) -> u32 {
    let mut map = lockouts.lock().unwrap_or_else(|e| e.into_inner());
    // Drop entries past the lockout window so the map doesn't grow one
    // entry per distinct attacker IP forever on a long-running public
    // instance.  After this sweep, every surviving entry is within the
    // active window, so a fresh `or_insert` below either reuses a
    // still-counting entry or starts a new one.
    map.retain(|_, (_, when)| when.elapsed() < LOCKOUT_DURATION);
    let entry = map
        .entry(ip)
        .or_insert((0, std::time::Instant::now()));
    entry.0 += 1;
    entry.1 = std::time::Instant::now();
    entry.0
}

/// Constant-time byte slice comparison to prevent timing attacks on credentials.
/// Iterates over both slices fully regardless of length difference so that
/// neither the length relationship nor the content is leaked via timing.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = (a.len() != b.len()) as u8;
    let max_len = a.len().max(b.len());
    for i in 0..max_len {
        let x = if i < a.len() { a[i] } else { 0 };
        let y = if i < b.len() { b[i] } else { 0 };
        diff |= x ^ y;
    }
    diff == 0
}

pub(crate) fn clear_lockout(lockouts: &LockoutMap, ip: IpAddr) {
    let mut map = lockouts.lock().unwrap_or_else(|e| e.into_inner());
    map.remove(&ip);
}

/// Constant used by callers that need to reference the lockout attempt
/// ceiling when constructing their own user-visible messages.
pub(crate) const AUTH_MAX_ATTEMPTS: u32 = MAX_AUTH_ATTEMPTS;

/// Check an IPv4 address against private/loopback/link-local ranges and the
/// gateway (.1) restriction. Returns the rejection reason, or None if allowed.
fn reject_insecure_ipv4(octets: [u8; 4]) -> Option<&'static str> {
    let is_private = octets[0] == 10
        || (octets[0] == 172 && (16..=31).contains(&octets[1]))
        || (octets[0] == 192 && octets[1] == 168)
        || octets[0] == 127
        || (octets[0] == 169 && octets[1] == 254); // link-local
    if !is_private {
        return Some("Connection refused: security is disabled, only private IP addresses are allowed.");
    }
    if octets[3] == 1 && octets[0] != 127 {
        return Some("Connection refused: gateway addresses (*.*.*.1) are not allowed when security is disabled.");
    }
    None
}

/// When security is disabled, only allow connections from private/loopback IPs,
/// and reject any address ending in .1 (typically a gateway), except for
/// loopback addresses (127.x.x.x). Returns the rejection reason, or None
/// if the address is allowed.
pub(crate) fn reject_insecure_ip(ip: IpAddr) -> Option<&'static str> {
    match ip {
        IpAddr::V4(v4) => reject_insecure_ipv4(v4.octets()),
        IpAddr::V6(v6) => {
            // IPv4-mapped IPv6 (::ffff:x.x.x.x) — apply IPv4 rules
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return reject_insecure_ipv4(mapped.octets());
            }
            if v6.is_loopback() {
                return None;
            }
            let segments = v6.segments();
            // Link-local (fe80::/10)
            if segments[0] & 0xffc0 == 0xfe80 {
                return None;
            }
            // Unique local (fd00::/8)
            if segments[0] & 0xff00 == 0xfd00 {
                return None;
            }
            Some("Connection refused: security is disabled, only private IP addresses are allowed.")
        }
    }
}

/// Returns true if the IP is a private/link-local address (not loopback, not public).
fn is_private_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 10
                || (o[0] == 172 && (16..=31).contains(&o[1]))
                || (o[0] == 192 && o[1] == 168)
                || (o[0] == 169 && o[1] == 254)
        }
        IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                let o = mapped.octets();
                return o[0] == 10
                    || (o[0] == 172 && (16..=31).contains(&o[1]))
                    || (o[0] == 192 && o[1] == 168)
                    || (o[0] == 169 && o[1] == 254);
            }
            let seg = v6.segments();
            // Link-local (fe80::/10)
            (seg[0] & 0xffc0 == 0xfe80)
            // Unique local (fd00::/8)
            || (seg[0] & 0xff00 == 0xfd00)
        }
    }
}


/// Map a TTYPE name reported by the client (via `IAC SB TTYPE IS ...`)
/// to one of our TerminalType variants. Returns None for names we don't
/// recognize so the caller falls back to the BACKSPACE-press detection.
/// Names arrive uppercase per RFC 1091, but we match case-insensitively
/// to be tolerant of non-compliant clients.
fn match_terminal_name(name: &str) -> Option<TerminalType> {
    let upper = name.trim().to_ascii_uppercase();
    if upper.is_empty() {
        return None;
    }
    // PETSCII clients: C64, C128, and explicit PETSCII names.
    if upper == "C64"
        || upper == "C128"
        || upper == "COMMODORE"
        || upper.starts_with("PETSCII")
        || upper.starts_with("C64")
        || upper.starts_with("C128")
    {
        return Some(TerminalType::Petscii);
    }
    // ANSI-capable: xterm family, vt100+, ansi*, linux console, screen/tmux.
    if upper.starts_with("XTERM")
        || upper.starts_with("VT")
        || upper.starts_with("ANSI")
        || upper.starts_with("LINUX")
        || upper.starts_with("SCREEN")
        || upper.starts_with("TMUX")
        || upper.starts_with("RXVT")
        || upper.starts_with("KONSOLE")
        || upper.starts_with("ALACRITTY")
        || upper.starts_with("WEZTERM")
        || upper == "CYGWIN"
        || upper == "PUTTY"
    {
        return Some(TerminalType::Ansi);
    }
    // Dumb/unknown terminals: fall back to plain ASCII (no color).
    if upper == "DUMB" || upper == "UNKNOWN" || upper == "NETWORK" {
        return Some(TerminalType::Ascii);
    }
    None
}

// ─── Input helpers (standalone) ─────────────────────────────

fn is_backspace_key(byte: u8, erase_char: u8) -> bool {
    byte == erase_char || byte == 0x08 || byte == 0x7F || byte == 0x14
}

/// Returns true for ANSI ESC (0x1B), plus C64 back-arrow (0x5F) when petscii is true.
pub(crate) fn is_esc_key(byte: u8, petscii: bool) -> bool {
    byte == 0x1B || (petscii && byte == 0x5F)
}

use crate::webbrowser::truncate_to_width;

/// Return the private (RFC 1918 / link-local / ULA) IPv4 and IPv6
/// addresses of this machine, excluding loopback.
fn get_server_addresses() -> Vec<String> {
    let mut addrs = Vec::new();
    if let Ok(ifaces) = if_addrs::get_if_addrs() {
        for iface in &ifaces {
            if iface.is_loopback() {
                continue;
            }
            let ip = iface.ip();
            if !is_private_ip(ip) {
                continue;
            }
            let s = ip.to_string();
            if !addrs.contains(&s) {
                addrs.push(s);
            }
        }
    }
    addrs
}

/// Read a single byte, filtering out telnet IAC protocol sequences.
async fn read_byte_iac_filtered(
    reader: &mut (dyn tokio::io::AsyncRead + Unpin + Send),
    filter_iac: bool,
) -> Result<Option<u8>, std::io::Error> {
    let mut buf = [0u8; 1];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => return Ok(None),
            Ok(_) => {
                let byte = buf[0];
                if filter_iac && byte == 0xFF {
                    match reader.read(&mut buf).await {
                        Ok(0) => return Ok(None),
                        Ok(_) => {
                            let cmd = buf[0];
                            if cmd == 0xFF {
                                return Ok(Some(0xFF));
                            }
                            if cmd == 0xFA {
                                // Subnegotiation — consume until IAC SE.  Bound
                                // each in-SB read so a peer can't pin us by
                                // dribbling an SB that never terminates; a
                                // stalled SB is treated as a closed connection.
                                let mut in_iac = false;
                                loop {
                                    match tokio::time::timeout(
                                        SB_DRAIN_TIMEOUT,
                                        reader.read(&mut buf),
                                    )
                                    .await
                                    {
                                        Err(_) => return Ok(None),
                                        Ok(Ok(0)) => return Ok(None),
                                        Ok(Ok(_)) => {
                                            if in_iac {
                                                if buf[0] == 0xF0 {
                                                    break;
                                                }
                                                in_iac = false;
                                            } else if buf[0] == 0xFF {
                                                in_iac = true;
                                            }
                                        }
                                        Ok(Err(e)) => return Err(e),
                                    }
                                }
                                continue;
                            }
                            // WILL/WONT/DO/DONT — consume the option byte
                            if (0xFB..=0xFE).contains(&cmd) {
                                match reader.read(&mut buf).await {
                                    Ok(0) => return Ok(None),
                                    Err(e) => return Err(e),
                                    _ => {}
                                }
                            }
                            continue;
                        }
                        Err(e) => return Err(e),
                    }
                }
                return Ok(Some(byte));
            }
            Err(e) => return Err(e),
        }
    }
}

/// Events surfaced by the outgoing Telnet Gateway's local-side reader.
///
/// Unlike [`read_byte_iac_filtered`] (which drops every IAC sequence
/// silently), this reader surfaces `SB NAWS <w><h> IAC SE` as a structured
/// resize event so the gateway can forward it to the remote server while
/// a session is already live.  All other IAC framing — 2-byte commands,
/// option negotiations, non-NAWS subnegotiations — is still consumed.
#[derive(Debug, PartialEq, Eq)]
enum GatewayInboundEvent {
    /// A plain data byte from the local user.  `IAC IAC` is unescaped.
    Data(u8),
    /// The local client sent `IAC SB NAWS <cols16><rows16> IAC SE`.
    NawsResize(u16, u16),
    /// Connection closed.
    Eof,
}

/// Read one event from the local user's side of a Telnet Gateway session.
async fn read_gateway_event(
    reader: &mut (dyn tokio::io::AsyncRead + Unpin + Send),
) -> std::io::Result<GatewayInboundEvent> {
    let mut buf = [0u8; 1];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => return Ok(GatewayInboundEvent::Eof),
            Ok(_) => {}
            Err(e) => return Err(e),
        }
        let byte = buf[0];
        if byte != IAC {
            return Ok(GatewayInboundEvent::Data(byte));
        }
        // Read the command byte.
        match reader.read(&mut buf).await {
            Ok(0) => return Ok(GatewayInboundEvent::Eof),
            Ok(_) => {}
            Err(e) => return Err(e),
        }
        let cmd = buf[0];
        match cmd {
            IAC => return Ok(GatewayInboundEvent::Data(IAC)),
            SB => {
                // Read the option code.
                match reader.read(&mut buf).await {
                    Ok(0) => return Ok(GatewayInboundEvent::Eof),
                    Ok(_) => {}
                    Err(e) => return Err(e),
                }
                let opt = buf[0];
                // Read body until IAC SE, unescaping IAC IAC → single
                // IAC.  Cap accumulated size so a malicious peer cannot
                // drive memory unbounded by sending a giant SB without
                // a terminating IAC SE; bytes past the cap are dropped
                // but the loop still scans for IAC SE to stay in sync.
                let mut body: Vec<u8> = Vec::new();
                let mut in_iac = false;
                loop {
                    // Bound in-SB reads (slowloris guard); a stalled
                    // subnegotiation is treated as a closed connection.
                    match tokio::time::timeout(SB_DRAIN_TIMEOUT, reader.read(&mut buf)).await {
                        Err(_) => return Ok(GatewayInboundEvent::Eof),
                        Ok(Ok(0)) => return Ok(GatewayInboundEvent::Eof),
                        Ok(Ok(_)) => {}
                        Ok(Err(e)) => return Err(e),
                    }
                    let b = buf[0];
                    if in_iac {
                        if b == SE {
                            break;
                        } else if b == IAC {
                            if body.len() < MAX_SB_BODY_BYTES {
                                body.push(IAC);
                            }
                            in_iac = false;
                        } else {
                            in_iac = false;
                        }
                    } else if b == IAC {
                        in_iac = true;
                    } else if body.len() < MAX_SB_BODY_BYTES {
                        body.push(b);
                    }
                }
                if opt == OPT_NAWS && body.len() == 4 {
                    let w = u16::from_be_bytes([body[0], body[1]]);
                    let h = u16::from_be_bytes([body[2], body[3]]);
                    return Ok(GatewayInboundEvent::NawsResize(w, h));
                }
                // Non-NAWS subnegotiation: drop and keep reading.
            }
            WILL | WONT | DO | DONT => {
                // Consume the option byte; drop the negotiation.
                match reader.read(&mut buf).await {
                    Ok(0) => return Ok(GatewayInboundEvent::Eof),
                    Ok(_) => {}
                    Err(e) => return Err(e),
                }
            }
            _ => {
                // 2-byte command (NOP, DM, BRK, IP, AO, AYT, EC, EL, GA)
                // — already fully consumed.
            }
        }
    }
}

// ─── SSH Gateway helpers ────────────────────────────────────

/// True when gateway byte-tracing is enabled — either by the
/// `gateway_debug` config flag (toggleable from the GUI, web console, and
/// the in-session Serial Configuration menu) or forced on by the
/// `EGATEWAY_GATEWAY_DEBUG` environment variable (any non-empty value).
/// Gates the chatty per-byte diagnostics in the SSH and Telnet gateway
/// proxy loops so they cost nothing when off.  `cfg_flag` is the caller's
/// already-read `cfg.gateway_debug`, avoiding a second config lock.
fn gw_debug_enabled(cfg_flag: bool) -> bool {
    cfg_flag || std::env::var_os("EGATEWAY_GATEWAY_DEBUG").is_some_and(|v| !v.is_empty())
}

/// Maximum bytes the gateway-debug `dbg_in` line buffer will accumulate
/// before being force-flushed.  Prevents a no-newline stream (a TUI editor,
/// a binary paste, a remote program doing its own line editing) from growing
/// the trace buffer without bound while gateway_debug is enabled.
const GW_DBG_IN_CAP: usize = 4096;

/// Format a byte slice as a compact hex + printable-ASCII dump for the
/// gateway diagnostics log, e.g. `73 75 64 6f | "sudo"`.  Non-printable
/// bytes render as `.` in the ASCII column.
fn gw_hexdump(bytes: &[u8]) -> String {
    let hex: Vec<String> = bytes.iter().map(|b| format!("{:02x}", b)).collect();
    let ascii: String = bytes
        .iter()
        .map(|&b| if (0x20..=0x7E).contains(&b) { b as char } else { '.' })
        .collect();
    format!("{} | \"{}\"", hex.join(" "), ascii)
}

/// Filter SSH gateway output for non-ANSI terminals.
///
/// Strips all ANSI escape sequences (CSI, OSC, DCS, PM, APC, SOS) from the
/// byte stream.  For PETSCII terminals, plain-text bytes are also case-swapped.
/// `state` is the ANSI parser state carried across calls (start at 0):
///   0=normal, 1=ESC seen, 2=CSI sequence, 3=string sequence, 4=ESC in string
fn filter_gateway_output(input: &[u8], state: &mut u8, is_petscii: bool, out: &mut Vec<u8>) {
    for &b in input {
        match *state {
            0 => {
                if b == 0x1B {
                    *state = 1;
                } else if is_petscii {
                    match b {
                        b'~' => {}  // tilde has no PETSCII equivalent
                        0x08 | 0x7F => out.push(0x14),  // backspace/DEL → PETSCII DEL
                        b'A'..=b'Z' => out.push(b + 32),
                        b'a'..=b'z' => out.push(b - 32),
                        _ => out.push(b),
                    }
                } else {
                    out.push(b);
                }
            }
            1 => {
                *state = match b {
                    b'[' => 2,                                   // CSI
                    b']' | b'P' | b'^' | b'_' | b'X' => 3,      // OSC/DCS/PM/APC/SOS
                    0x1B => 1,                                   // Another ESC
                    _ => 0,                                      // 2-char sequence done
                };
            }
            2 => {
                // CSI: parameter/intermediate bytes stay in state 2.
                // Final byte (0x40-0x7E) ends the sequence.
                if (0x40..=0x7E).contains(&b) {
                    *state = 0;
                } else if b == 0x1B {
                    *state = 1;
                } else if b < 0x20 || b == 0x7F {
                    *state = 0;
                }
            }
            3 => {
                // String sequence: consume until BEL or ESC
                if b == 0x07 {
                    *state = 0;
                } else if b == 0x1B {
                    *state = 4;
                }
            }
            _ => {
                // ESC inside string: '\' = ST (end), else resume string
                *state = if b == b'\\' { 0 } else { 3 };
            }
        }
    }
}

/// Per-option Q-method state — full RFC 1143 six-state variant.
///
/// Each option tracks two independent state machines: one for our side
/// (what we've declared via WILL/WONT) and one for the peer's side (what
/// they've declared via WILL/WONT).
///
/// The "Opposite" variants handle the race where we change our mind
/// about an option while a prior request is still in flight.  Example:
/// we send `WILL TTYPE` (entering WantYes), then before the peer's reply
/// arrives we decide we no longer want TTYPE, so we send `WONT TTYPE`
/// — we cannot simply go to WantNo because our WILL is still on the wire
/// and the peer will eventually respond to it.  Instead we enter
/// `WantYesOpposite`, meaning "we're still waiting for the WILL reply,
/// but our current intent is Off."  When the peer finally replies, the
/// state machine resolves cleanly.
///
/// See RFC 1143 §7 for the full transition table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OptState {
    /// Option is off and no negotiation is in flight.
    No,
    /// Option is on.
    Yes,
    /// We have asked to enable the option; awaiting peer's reply.
    WantYes,
    /// Same as WantYes, but since sending the request we've changed our
    /// mind and now want the option off.  On the peer's reply we will
    /// send the opposite verb.
    WantYesOpposite,
    /// We have asked to disable the option; awaiting peer's reply.
    WantNo,
    /// Same as WantNo, but since sending the request we've changed our
    /// mind and now want the option on.  On the peer's reply we will
    /// send the opposite verb.
    WantNoOpposite,
}

/// Telnet-client IAC parser + Q-method state machine for the outgoing
/// gateway.  Handles the remote→local direction: parses IAC, unescapes
/// `IAC IAC` to a single data byte, consumes 2-byte commands, and
/// performs option negotiation.
///
/// Negotiation policy:
///
/// - **ECHO** (RFC 857) — always cooperative: peer's `WILL ECHO` is
///   accepted with `DO ECHO`.  Raw-TCP services never send WILL ECHO so
///   this is always safe.
/// - **TTYPE** (RFC 1091) and **NAWS** (RFC 1073) — cooperative only
///   when `cooperate == true`.  Gated because cooperation implies
///   proactive `WILL TTYPE` / `WILL NAWS` at connect, which raw-TCP
///   services would see as garbage.
/// - **Everything else** — refused: `WILL → DONT`, `DO → WONT`.
///
/// The parser never initiates a TTYPE/NAWS request from the peer side;
/// we don't care about the server's own terminal type or window size.
struct GatewayTelnetIac {
    state: GatewayIacState,
    /// Cooperate on TTYPE / NAWS (from the config toggle).
    cooperate: bool,
    /// Terminal name reported in `SB TTYPE IS`.  Chosen to match the
    /// local user's detected terminal type.
    terminal_name: String,
    /// Width to report in `SB NAWS`.
    window_cols: u16,
    /// Height to report in `SB NAWS`.
    window_rows: u16,
    /// Per-option state: what we've said about our own side.
    us_state: Box<[OptState; 256]>,
    /// Per-option state: what the peer has said about their side.
    him_state: Box<[OptState; 256]>,
    /// Whether we've already sent a `DONT <opt>` refusal for this option.
    /// Cleared when the peer finally sends `WONT <opt>` to ack the refusal.
    /// Prevents a chattery peer from getting repeated DONTs for the same
    /// unwanted WILL.
    sent_dont: Box<[bool; 256]>,
    /// Whether we've already sent a `WONT <opt>` refusal.  Cleared when the
    /// peer sends `DONT <opt>` to ack.
    sent_wont: Box<[bool; 256]>,
    /// Subnegotiation buffer.  `sb_option` is set when we enter the SB
    /// body (just after `IAC SB <opt>`); `sb_body` accumulates bytes
    /// with `IAC IAC` already unescaped to single 0xFF.
    sb_option: u8,
    sb_body: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
enum GatewayIacState {
    /// Either a plain data byte or the start of a new IAC sequence.
    Normal,
    /// Previous byte was IAC; waiting for the command byte.
    SawIac,
    /// Previous bytes were IAC + WILL/WONT/DO/DONT; waiting for the option.
    SawVerb(u8),
    /// Just saw `IAC SB`; the next byte is the option code.
    SawSbOption,
    /// Inside an SB subnegotiation body; scanning for IAC SE.
    InSb,
    /// Inside an SB body, just saw an IAC; next byte decides whether it was
    /// IAC SE (end of SB) or IAC IAC (escaped data byte, stay in SB).
    InSbIac,
}

impl GatewayTelnetIac {
    /// Build a fresh parser.  Returns `(parser, initial_offers)` — any
    /// bytes that must be written to the remote before we start reading,
    /// to advertise our cooperative options.  Empty when `cooperate` is
    /// off (reactive-only mode).
    fn new(
        cooperate: bool,
        terminal_name: String,
        window_cols: u16,
        window_rows: u16,
    ) -> (Self, Vec<u8>) {
        let mut parser = Self {
            state: GatewayIacState::Normal,
            cooperate,
            terminal_name,
            window_cols,
            window_rows,
            us_state: Box::new([OptState::No; 256]),
            him_state: Box::new([OptState::No; 256]),
            sent_dont: Box::new([false; 256]),
            sent_wont: Box::new([false; 256]),
            sb_option: 0,
            sb_body: Vec::new(),
        };
        let mut initial = Vec::new();
        if cooperate {
            // Proactively offer WILL TTYPE and WILL NAWS; proactively
            // request DO ECHO so we don't need to wait for the peer to
            // offer echo (some BBSes wait for the client to ask first).
            // Set the matching WantYes states so peer acks are recognised.
            parser.us_state[OPT_TTYPE as usize] = OptState::WantYes;
            parser.us_state[OPT_NAWS as usize] = OptState::WantYes;
            parser.him_state[OPT_ECHO as usize] = OptState::WantYes;
            initial.extend_from_slice(&[IAC, WILL, OPT_TTYPE]);
            initial.extend_from_slice(&[IAC, WILL, OPT_NAWS]);
            initial.extend_from_slice(&[IAC, DO, OPT_ECHO]);
        }
        (parser, initial)
    }

    /// True if we should answer the peer's `WILL <opt>` with `DO <opt>`.
    fn cooperate_with_his_will(&self, opt: u8) -> bool {
        // ECHO from the server is always welcome — it means "I'll echo
        // your input," which for a retro user is what makes typing
        // visible.  Everything else (WILL TTYPE / WILL NAWS from the
        // server is unusual) we decline.
        opt == OPT_ECHO
    }

    /// True if we should answer the peer's `DO <opt>` with `WILL <opt>`.
    fn cooperate_with_his_do(&self, opt: u8) -> bool {
        self.cooperate && (opt == OPT_TTYPE || opt == OPT_NAWS)
    }

    fn feed(&mut self, byte: u8, data: &mut Vec<u8>, replies: &mut Vec<u8>) {
        match self.state {
            GatewayIacState::Normal => {
                if byte == IAC {
                    self.state = GatewayIacState::SawIac;
                } else {
                    data.push(byte);
                }
            }
            GatewayIacState::SawIac => {
                match byte {
                    IAC => {
                        data.push(IAC);
                        self.state = GatewayIacState::Normal;
                    }
                    SB => {
                        self.state = GatewayIacState::SawSbOption;
                    }
                    WILL | WONT | DO | DONT => {
                        self.state = GatewayIacState::SawVerb(byte);
                    }
                    _ => {
                        // 2-byte command (NOP, DM, BRK, IP, AO, AYT, EC,
                        // EL, GA, SE-out-of-context) — consumed.
                        self.state = GatewayIacState::Normal;
                    }
                }
            }
            GatewayIacState::SawVerb(verb) => {
                let opt = byte;
                match verb {
                    WILL => self.handle_recv_will(opt, replies),
                    WONT => self.handle_recv_wont(opt, replies),
                    DO => self.handle_recv_do(opt, replies),
                    DONT => self.handle_recv_dont(opt, replies),
                    _ => {}
                }
                self.state = GatewayIacState::Normal;
            }
            GatewayIacState::SawSbOption => {
                self.sb_option = byte;
                self.sb_body.clear();
                self.state = GatewayIacState::InSb;
            }
            GatewayIacState::InSb => {
                if byte == IAC {
                    self.state = GatewayIacState::InSbIac;
                } else if self.sb_body.len() < MAX_SB_BODY_BYTES {
                    self.sb_body.push(byte);
                }
                // Bytes beyond MAX_SB_BODY_BYTES are dropped; we stay in
                // InSb so an eventual IAC SE still terminates the SB.
            }
            GatewayIacState::InSbIac => {
                match byte {
                    SE => {
                        self.process_subneg(replies);
                        self.state = GatewayIacState::Normal;
                    }
                    IAC => {
                        // Escaped IAC inside SB — keep as single 0xFF
                        // (subject to the body-size cap).
                        if self.sb_body.len() < MAX_SB_BODY_BYTES {
                            self.sb_body.push(IAC);
                        }
                        self.state = GatewayIacState::InSb;
                    }
                    _ => {
                        // Malformed; resume scanning for IAC SE.
                        self.state = GatewayIacState::InSb;
                    }
                }
            }
        }
    }

    // ─── Q-method handlers (his side) ─────────────────────

    fn handle_recv_will(&mut self, opt: u8, replies: &mut Vec<u8>) {
        let idx = opt as usize;
        match self.him_state[idx] {
            OptState::No => {
                if self.cooperate_with_his_will(opt) {
                    self.him_state[idx] = OptState::Yes;
                    self.sent_dont[idx] = false; // contradicts any prior refusal
                    replies.extend_from_slice(&[IAC, DO, opt]);
                } else if !self.sent_dont[idx] {
                    // Refuse, but only once per cycle.  Q-method keeps
                    // him at No because we do not want it on.
                    self.sent_dont[idx] = true;
                    replies.extend_from_slice(&[IAC, DONT, opt]);
                }
            }
            OptState::Yes => {
                // Already on — spec says ignore.
            }
            OptState::WantYes => {
                // Peer acks our DO.
                self.him_state[idx] = OptState::Yes;
            }
            OptState::WantYesOpposite => {
                // Peer acked our original DO, but we've since changed to
                // wanting No; send DONT and enter WantNo.  Mark the
                // refusal so a misbehaving peer that re-sends WILL from
                // the subsequent WantNo state doesn't get a duplicate.
                self.him_state[idx] = OptState::WantNo;
                self.sent_dont[idx] = true;
                replies.extend_from_slice(&[IAC, DONT, opt]);
            }
            OptState::WantNo => {
                // Error: peer sent WILL in response to our DONT.  Log
                // by dropping back to No and, if we haven't already,
                // refuse again.
                self.him_state[idx] = OptState::No;
                if !self.sent_dont[idx] {
                    self.sent_dont[idx] = true;
                    replies.extend_from_slice(&[IAC, DONT, opt]);
                }
            }
            OptState::WantNoOpposite => {
                // Error but harmless: we wanted Yes again anyway.  The
                // stale DONT we sent on the way in is now contradicted
                // by our accepting Yes — clear the refusal flag.
                self.him_state[idx] = OptState::Yes;
                self.sent_dont[idx] = false;
            }
        }
    }

    fn handle_recv_wont(&mut self, opt: u8, replies: &mut Vec<u8>) {
        let idx = opt as usize;
        // Peer is acking our refusal or withdrawing — reset refusal-sent
        // so a future fresh cycle can issue a DONT again.
        self.sent_dont[idx] = false;
        match self.him_state[idx] {
            OptState::No => {
                // Already off — ignore.
            }
            OptState::Yes => {
                self.him_state[idx] = OptState::No;
                replies.extend_from_slice(&[IAC, DONT, opt]);
            }
            OptState::WantNo => {
                self.him_state[idx] = OptState::No;
            }
            OptState::WantNoOpposite => {
                // Peer confirmed our DONT, but we changed to WantYes;
                // send a fresh DO.
                self.him_state[idx] = OptState::WantYes;
                self.sent_dont[idx] = false;
                replies.extend_from_slice(&[IAC, DO, opt]);
            }
            OptState::WantYes => {
                // Peer refused our DO.
                self.him_state[idx] = OptState::No;
            }
            OptState::WantYesOpposite => {
                // Peer refused our DO, but we already swung back to No,
                // so we're exactly where we wanted.
                self.him_state[idx] = OptState::No;
            }
        }
    }

    fn handle_recv_do(&mut self, opt: u8, replies: &mut Vec<u8>) {
        let idx = opt as usize;
        match self.us_state[idx] {
            OptState::No => {
                if self.cooperate_with_his_do(opt) {
                    self.us_state[idx] = OptState::Yes;
                    self.sent_wont[idx] = false; // contradicts any prior refusal
                    replies.extend_from_slice(&[IAC, WILL, opt]);
                    if opt == OPT_NAWS {
                        self.emit_naws_sb(replies);
                    }
                } else if !self.sent_wont[idx] {
                    self.sent_wont[idx] = true;
                    replies.extend_from_slice(&[IAC, WONT, opt]);
                }
            }
            OptState::Yes => {
                // Already on — ignore.
            }
            OptState::WantYes => {
                self.us_state[idx] = OptState::Yes;
                if opt == OPT_NAWS {
                    self.emit_naws_sb(replies);
                }
            }
            OptState::WantYesOpposite => {
                // Peer acked our WILL but we want No; send WONT.  Mark
                // the refusal so a misbehaving peer that re-sends DO
                // from the subsequent WantNo state doesn't get a dup.
                self.us_state[idx] = OptState::WantNo;
                self.sent_wont[idx] = true;
                replies.extend_from_slice(&[IAC, WONT, opt]);
            }
            OptState::WantNo => {
                // Error: peer DO after our WONT.  Bounce to No.
                self.us_state[idx] = OptState::No;
                if !self.sent_wont[idx] {
                    self.sent_wont[idx] = true;
                    replies.extend_from_slice(&[IAC, WONT, opt]);
                }
            }
            OptState::WantNoOpposite => {
                // Error but harmless — we wanted Yes.  The stale WONT
                // we sent on the way in is contradicted by accepting
                // Yes; clear the refusal flag.
                self.us_state[idx] = OptState::Yes;
                self.sent_wont[idx] = false;
                if opt == OPT_NAWS {
                    self.emit_naws_sb(replies);
                }
            }
        }
    }

    fn handle_recv_dont(&mut self, opt: u8, replies: &mut Vec<u8>) {
        let idx = opt as usize;
        self.sent_wont[idx] = false;
        match self.us_state[idx] {
            OptState::No => {
                // Already off.
            }
            OptState::Yes => {
                self.us_state[idx] = OptState::No;
                replies.extend_from_slice(&[IAC, WONT, opt]);
            }
            OptState::WantNo => {
                self.us_state[idx] = OptState::No;
            }
            OptState::WantNoOpposite => {
                // Peer confirmed DONT, but we changed to WantYes — send WILL.
                self.us_state[idx] = OptState::WantYes;
                self.sent_wont[idx] = false;
                replies.extend_from_slice(&[IAC, WILL, opt]);
            }
            OptState::WantYes => {
                // Peer refused our WILL.
                self.us_state[idx] = OptState::No;
            }
            OptState::WantYesOpposite => {
                // Peer refused our WILL, and we already swung back to No —
                // exactly where we wanted.
                self.us_state[idx] = OptState::No;
            }
        }
    }

    // ─── Active-change helpers (for mind-changes mid-flight) ──

    /// Ask for our side of `opt` to be enabled (send `WILL`).  Advances
    /// the Q-method state for `us_state[opt]` per RFC 1143 §7.
    ///
    /// Currently unused by `gateway_telnet` — we only enter `WantYes` via
    /// the proactive offers in `new()` — but kept for symmetry and so
    /// future active-change flows compile cleanly.
    #[allow(dead_code)]
    fn request_local_enable(&mut self, opt: u8, replies: &mut Vec<u8>) {
        let idx = opt as usize;
        match self.us_state[idx] {
            OptState::No => {
                self.us_state[idx] = OptState::WantYes;
                self.sent_wont[idx] = false; // contradicts any prior refusal
                replies.extend_from_slice(&[IAC, WILL, opt]);
            }
            OptState::Yes => {} // already on
            OptState::WantNo => {
                // Changed mind mid-flight.
                self.us_state[idx] = OptState::WantNoOpposite;
            }
            OptState::WantNoOpposite => {} // already queued to enable
            OptState::WantYes => {}
            OptState::WantYesOpposite => {
                // Reverting to original intent.
                self.us_state[idx] = OptState::WantYes;
            }
        }
    }

    /// Ask for our side of `opt` to be disabled (send `WONT`).
    #[allow(dead_code)]
    fn request_local_disable(&mut self, opt: u8, replies: &mut Vec<u8>) {
        let idx = opt as usize;
        match self.us_state[idx] {
            OptState::Yes => {
                self.us_state[idx] = OptState::WantNo;
                replies.extend_from_slice(&[IAC, WONT, opt]);
            }
            OptState::No => {} // already off
            OptState::WantYes => {
                self.us_state[idx] = OptState::WantYesOpposite;
            }
            OptState::WantYesOpposite => {}
            OptState::WantNo => {}
            OptState::WantNoOpposite => {
                self.us_state[idx] = OptState::WantNo;
            }
        }
    }

    // ─── Subnegotiation ───────────────────────────────────

    fn process_subneg(&mut self, replies: &mut Vec<u8>) {
        if self.sb_option == OPT_TTYPE
            && self.us_state[OPT_TTYPE as usize] == OptState::Yes
            && self.sb_body.first().copied() == Some(TTYPE_SEND)
        {
            // Respond with our terminal name.  Any 0xFF in the name
            // (shouldn't happen for our controlled values) would need
            // IAC-doubling; we check explicitly.
            let mut body = vec![IAC, SB, OPT_TTYPE, TTYPE_IS];
            for &b in self.terminal_name.as_bytes() {
                if b == IAC {
                    body.push(IAC);
                }
                body.push(b);
            }
            body.extend_from_slice(&[IAC, SE]);
            replies.extend_from_slice(&body);
        }
        // All other SB bodies are informational only — we silently drop.
    }

    /// Record an updated window size from the local user and, if NAWS is
    /// currently enabled on our side, emit an `IAC SB NAWS <w><h> IAC SE`
    /// update to the remote.  Called from the gateway loop when the user
    /// resizes their terminal mid-session.
    fn send_naws_update(&mut self, cols: u16, rows: u16, replies: &mut Vec<u8>) {
        self.window_cols = cols;
        self.window_rows = rows;
        if self.us_state[OPT_NAWS as usize] == OptState::Yes {
            self.emit_naws_sb(replies);
        }
    }

    fn emit_naws_sb(&self, replies: &mut Vec<u8>) {
        // `IAC SB NAWS <w16_BE> <h16_BE> IAC SE`, with any byte equal to
        // IAC doubled per RFC 854.
        let w = self.window_cols.to_be_bytes();
        let h = self.window_rows.to_be_bytes();
        let size_bytes = [w[0], w[1], h[0], h[1]];
        let mut body = vec![IAC, SB, OPT_NAWS];
        for &b in &size_bytes {
            if b == IAC {
                body.push(IAC);
            }
            body.push(b);
        }
        body.extend_from_slice(&[IAC, SE]);
        replies.extend_from_slice(&body);
    }
}

/// Default terminal name reported via `SB TTYPE IS`.  Chosen to be
/// informative to modern BBSes and still truthful.
fn gateway_terminal_name(tt: TerminalType) -> &'static str {
    match tt {
        TerminalType::Petscii => "PETSCII",
        TerminalType::Ansi => "ANSI",
        TerminalType::Ascii => "DUMB",
    }
}

/// Default window dimensions to report via `SB NAWS` when the local
/// client hasn't supplied any via its own NAWS.
fn gateway_default_window(tt: TerminalType) -> (u16, u16) {
    match tt {
        TerminalType::Petscii => (PETSCII_WIDTH as u16, 25),
        TerminalType::Ansi | TerminalType::Ascii => (80, 24),
    }
}

/// Write `bytes` to `w`, doubling any 0xFF as IAC IAC per RFC 854.  Used
/// by the outgoing telnet gateway in both directions so that literal 0xFF
/// data bytes survive the wire without being mistaken for IAC.
async fn write_telnet_data<W>(w: &mut W, bytes: &[u8]) -> std::io::Result<()>
where
    W: AsyncWriteExt + Unpin + ?Sized,
{
    let mut last = 0;
    for (i, &b) in bytes.iter().enumerate() {
        if b == IAC {
            if last < i {
                w.write_all(&bytes[last..i]).await?;
            }
            w.write_all(&[IAC, IAC]).await?;
            last = i + 1;
        }
    }
    if last < bytes.len() {
        w.write_all(&bytes[last..]).await?;
    }
    Ok(())
}

/// Normalize a client input byte for SSH gateway forwarding.
///
/// Telnet clients send CR+LF or CR+NUL for Enter; SSH expects bare CR.
/// Returns `Some(byte)` if the byte should be forwarded, `None` to suppress.
fn normalize_gateway_input(b: u8, last_cr: &mut bool) -> Option<u8> {
    if (b == b'\n' || b == 0x00) && *last_cr {
        *last_cr = false;
        return None;
    }
    *last_cr = b == b'\r';
    Some(b)
}

/// SSH client handler for the gateway feature. Captures the server's host key
/// so it can be verified against the known-hosts file after connection.
struct GatewayHandler {
    server_key: Arc<std::sync::Mutex<Option<russh::keys::PublicKey>>>,
}

impl russh::client::Handler for GatewayHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &russh::keys::PublicKey,
    ) -> Result<bool, Self::Error> {
        if let Ok(mut key) = self.server_key.lock() {
            *key = Some(server_public_key.clone());
        }
        Ok(true)
    }
}

// ─── Known-hosts management ────────────────────────────────

const GATEWAY_HOSTS_FILE: &str = "gateway_hosts";

/// Result of checking a host key against the known-hosts file.
pub(crate) enum HostKeyStatus {
    /// Key matches a stored entry.
    Known,
    /// No entry for this host:port.
    Unknown,
    /// Stored key does not match the presented key.
    Changed,
}

/// Format the key as "algorithm base64" for storage.
fn format_host_key(key: &russh::keys::PublicKey) -> String {
    // key.to_string() produces "algorithm base64 comment" in OpenSSH format;
    // we only want "algorithm base64".
    let s = key.to_string();
    let parts: Vec<&str> = s.splitn(3, ' ').collect();
    if parts.len() >= 2 {
        format!("{} {}", parts[0], parts[1])
    } else {
        s
    }
}

/// Look up a host:port in the known-hosts file and compare the key.
pub(crate) fn check_known_host(
    host: &str,
    port: u16,
    key: &russh::keys::PublicKey,
) -> HostKeyStatus {
    let lookup = format!("{}:{}", host, port);
    let key_str = format_host_key(key);

    let content = match std::fs::read_to_string(GATEWAY_HOSTS_FILE) {
        Ok(c) => c,
        Err(_) => return HostKeyStatus::Unknown,
    };

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix(&lookup)
            && let Some(stored_key) = rest.strip_prefix(' ')
        {
            if stored_key == key_str {
                return HostKeyStatus::Known;
            }
            return HostKeyStatus::Changed;
        }
    }
    HostKeyStatus::Unknown
}

/// Save a host key to the known-hosts file.
///
/// Uses a static mutex to serialise read-modify-write across concurrent
/// sessions, and write-to-temp-then-rename for crash safety.
pub(crate) fn save_known_host(host: &str, port: u16, key: &russh::keys::PublicKey) {
    static HOSTS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = HOSTS_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let entry = format!("{}:{} {}\n", host, port, format_host_key(key));

    let mut content = std::fs::read_to_string(GATEWAY_HOSTS_FILE).unwrap_or_default();
    // Remove any existing entry for this host:port
    let lookup = format!("{}:{} ", host, port);
    let filtered: Vec<&str> = content
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            trimmed.is_empty()
                || trimmed.starts_with('#')
                || !trimmed.starts_with(&lookup)
        })
        .collect();
    content = filtered.join("\n");
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(&entry);
    if let Err(e) = atomic_write(GATEWAY_HOSTS_FILE, &content) {
        glog!("Warning: could not save gateway host key: {}", e);
    } else {
        // Restrict mode to owner-only.  The stored host public keys
        // are themselves public, but the file also exposes the dial
        // history (which hosts the operator has connected to) — a
        // meaningful privacy signal that other local users shouldn't
        // have.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(
                GATEWAY_HOSTS_FILE,
                std::fs::Permissions::from_mode(0o600),
            );
        }
    }
}

/// Write `content` to `path` atomically by writing to a uniquely-named
/// temporary file and then renaming it into place. This prevents partial
/// writes and avoids races between concurrent callers.
///
/// Callers that perform read-modify-write on the same file must still
/// serialise externally (e.g. via a mutex) to avoid lost updates.
fn atomic_write(path: &str, content: &str) -> Result<(), std::io::Error> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = format!("{}.{}.{}.tmp", path, std::process::id(), seq);
    std::fs::write(&tmp, content)?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

// ─── SharedWriter ───────────────────────────────────────────
pub(crate) type SharedWriter = Arc<tokio::sync::Mutex<Box<dyn tokio::io::AsyncWrite + Unpin + Send>>>;
pub(crate) type SessionWriters = Arc<tokio::sync::Mutex<Vec<SharedWriter>>>;

/// The notice sent to every live session when the server shuts down.  Kept
/// as one constant so the central async broadcast (telnet/SSH/relay) and
/// the serial thread's own notice stay in sync (and the test pins it).
pub(crate) const SHUTDOWN_GOODBYE: &str = "Server shutting down. Goodbye.";

/// Broadcast `msg` to every registered async session writer, then optionally
/// close each writer.
///
/// Telnet, SSH interactive shells, and master/slave relay sessions all
/// register their `SharedWriter` into the shared [`SessionWriters`] list, so
/// this reaches every async connection **regardless of which servers are
/// enabled**.  It is the single transport-agnostic broadcast primitive:
/// invoked from the central shutdown path in `main.rs` (previously the
/// shutdown notice lived in the telnet accept loop and was skipped entirely
/// on an SSH-only deployment), and the hook for any future all-session
/// broadcast message.
///
/// `close = true` flushes and shuts each writer down after the write (the
/// shutdown-goodbye path); pass `false` for an in-band message that leaves
/// the session running.  A per-writer `try_lock` skips a session that is
/// mid-write (holding its own writer lock) rather than blocking the whole
/// broadcast on it — at shutdown such a session is being torn down anyway.
///
/// Serial sessions are **not** in this list — they run on blocking threads
/// with a synchronous port and emit their own notice from
/// `serial::serial_thread` on the shutdown flag.
pub async fn broadcast_to_sessions(writers: &SessionWriters, msg: &[u8], close: bool) {
    let writers = writers.lock().await;
    for w in writers.iter() {
        if let Ok(mut writer) = w.try_lock() {
            let _ = writer.write_all(msg).await;
            let _ = writer.flush().await;
            if close {
                let _ = writer.shutdown().await;
            }
        }
    }
}

/// A Serial Gateway pick: either a local port (A/B) or a registered
/// remote console port on a slave (§9 #12), keyed by the slave's IP and
/// port label.
enum GatewayPick {
    Local(crate::config::SerialPortId),
    Remote { ip: IpAddr, label: String },
}

/// Max remote console ports shown in the Serial Gateway picker.  §9 #12
/// allows "paging OR a cap"; a cap (like `SERVER_ADDR_DISPLAY_CAP`) keeps
/// the picker inside the 22-row PETSCII budget without paging state.
const REMOTE_PORT_DISPLAY_CAP: usize = 6;

// ─── TelnetSession ──────────────────────────────────────────

pub(crate) struct TelnetSession {
    reader: Box<dyn tokio::io::AsyncRead + Unpin + Send>,
    writer: SharedWriter,
    shutdown: Arc<AtomicBool>,
    restart: Arc<AtomicBool>,
    current_menu: Menu,
    terminal_type: TerminalType,
    erase_char: u8,
    lockouts: LockoutMap,
    peer_addr: Option<IpAddr>,
    transfer_subdir: String,
    xmodem_iac: bool,
    web_lines: Vec<String>,
    web_scroll: usize,
    web_links: Vec<String>,
    web_history: Vec<(String, usize)>,
    web_url: Option<String>,
    web_title: Option<String>,
    web_forms: Vec<crate::webbrowser::WebForm>,
    weather_location: String,
    is_serial: bool,
    /// True for a master/slave **relay** session (a remote device bridged
    /// in from a slave).  Such a session behaves like a serial caller
    /// (`is_serial = true`, raw 8-bit, owns no local port) but is labelled
    /// distinctly and, for the console-mode picker, identified by its
    /// peer (the slave's IP).  False for every local/telnet/SSH session.
    is_relay: bool,
    /// When `is_serial = true`, this records WHICH physical port the
    /// caller dialed in on (Port A or Port B).  Used by
    /// `modem_apply_settings` to scope the 60-s warn-+-revert flow to
    /// the caller's own port: editing the OTHER port's settings from
    /// inside a serial session can't tear down this session, so the
    /// warn flow there would just be noise.  `None` for non-serial
    /// sessions.
    serial_port_id: Option<crate::config::SerialPortId>,
    is_ssh: bool,
    idle_timeout: std::time::Duration,
    // One-byte pushback used by drain_trailing_eol to safely return any
    // non-CR/LF byte it reads back to the next real input call.
    pushback: Option<u8>,
    // Telnet option negotiation state. Each per-option flag records a
    // reply we've already sent so we never loop on repeated requests.
    neg_sent_will: Box<[bool; 256]>,
    neg_sent_do: Box<[bool; 256]>,
    neg_sent_wont: Box<[bool; 256]>,
    neg_sent_dont: Box<[bool; 256]>,
    // TTYPE result — set once via SB TTYPE IS. Prevents re-requesting
    // and lets detect_terminal_type skip the BACKSPACE prompt.
    ttype_matched: bool,
    // Raw TERMINAL-TYPE name the client announced via SB TTYPE IS,
    // recorded even when it isn't one we recognize, so the gateway-debug
    // terminal diagnostic can show exactly what the client sent (e.g.
    // minicom's "ansi" or "xterm").  None until the first TTYPE IS, and
    // always None for serial callers (they skip telnet negotiation).
    ttype_raw: Option<String>,
    // Set the first time session_read_byte sees an IAC SB or
    // WILL/WONT/DO/DONT from the peer.  Distinguishes a true telnet
    // client (which participates in option negotiation, RFC 854/856)
    // from a raw TCP client (netcat, retro firmware) that just pipes
    // bytes.  Used to auto-enable IAC escaping only for real telnet.
    telnet_negotiated: bool,
    // NAWS (window size) from SB NAWS — fed into terminal-size queries
    // (e.g. browser layout, menu pagination).  None if the peer didn't
    // negotiate; callers fall back to TerminalType-driven defaults.
    window_width: Option<u16>,
    window_height: Option<u16>,
}

impl TelnetSession {
    const TRANSFER_PAGE_SIZE: usize = 10;
    /// File-size cap for upload/download UI; sourced from tnio so all
    /// four protocols agree on a single value.  Cast to `usize` once
    /// because telnet UI math is `usize`-shaped.
    const MAX_FILE_SIZE: usize = crate::tnio::MAX_FILE_SIZE as usize;
    const MAX_FILENAME_LEN: usize = 64;

    /// Create a session for a serial modem connection.  Starts in
    /// ASCII as a safe default, then runs the BACKSPACE-key terminal
    /// probe in `detect_terminal_type` so a C64 dialing in via the
    /// modem emulator can land in PETSCII instead of ASCII.  IAC
    /// option negotiation is skipped (the wire isn't telnet — IAC
    /// bytes would render as garbage on the caller's terminal).
    /// Authentication is also skipped: arrival via ATDT on a physical
    /// port is its own trust boundary.
    ///
    /// Serial sessions don't have a peer IP and don't run
    /// `authenticate()`, so the lockout map is genuinely empty —
    /// there's nothing to count against.  Kept as a constructor
    /// parameter for API symmetry with `new_ssh` so future code can't
    /// accidentally diverge.
    pub(crate) fn new_serial(
        port_id: crate::config::SerialPortId,
        reader: Box<dyn tokio::io::AsyncRead + Unpin + Send>,
        writer: SharedWriter,
        shutdown: Arc<AtomicBool>,
        restart: Arc<AtomicBool>,
        lockouts: LockoutMap,
    ) -> Self {
        Self {
            reader,
            writer,
            shutdown,
            restart,
            current_menu: Menu::Main,
            terminal_type: TerminalType::Ascii,
            erase_char: 0x7F,
            lockouts,
            peer_addr: None,
            transfer_subdir: String::new(),
            xmodem_iac: false,
            web_lines: Vec::new(),
            web_scroll: 0,
            web_links: Vec::new(),
            web_history: Vec::new(),
            web_url: None,
            web_title: None,
            web_forms: Vec::new(),
            weather_location: config::get_config().weather_location,
            is_serial: true,
            is_relay: false,
            serial_port_id: Some(port_id),
            is_ssh: false,
            idle_timeout: std::time::Duration::from_secs(config::get_config().idle_timeout_secs),
            pushback: None,
            neg_sent_will: Box::new([false; 256]),
            neg_sent_do: Box::new([false; 256]),
            neg_sent_wont: Box::new([false; 256]),
            neg_sent_dont: Box::new([false; 256]),
            ttype_matched: false,
            ttype_raw: None,
            telnet_negotiated: false,
            window_width: None,
            window_height: None,
        }
    }

    /// Create a session for an SSH connection.  Uses ANSI terminal
    /// (color, no IAC), skips terminal detection and authentication
    /// (already handled by the SSH layer).
    ///
    /// `lockouts` is the SAME map the telnet listener uses, so any
    /// future code that wires `TelnetSession::authenticate()` into
    /// the SSH path inherits cross-IP attempt counting that already
    /// applies to the SSH `auth_password` handler.  Without this
    /// sharing, an SSH-side TelnetSession::authenticate() call would
    /// silently bypass the lockout enforcement done in `ssh.rs`.
    pub(crate) fn new_ssh(
        reader: Box<dyn tokio::io::AsyncRead + Unpin + Send>,
        writer: SharedWriter,
        shutdown: Arc<AtomicBool>,
        restart: Arc<AtomicBool>,
        peer_addr: Option<IpAddr>,
        lockouts: LockoutMap,
    ) -> Self {
        Self {
            reader,
            writer,
            shutdown,
            restart,
            current_menu: Menu::Main,
            terminal_type: TerminalType::Ansi,
            erase_char: 0x7F,
            lockouts,
            peer_addr,
            transfer_subdir: String::new(),
            xmodem_iac: false,
            web_lines: Vec::new(),
            web_scroll: 0,
            web_links: Vec::new(),
            web_history: Vec::new(),
            web_url: None,
            web_title: None,
            web_forms: Vec::new(),
            weather_location: config::get_config().weather_location,
            is_serial: false,
            is_relay: false,
            serial_port_id: None,
            is_ssh: true,
            idle_timeout: std::time::Duration::from_secs(config::get_config().idle_timeout_secs),
            pushback: None,
            neg_sent_will: Box::new([false; 256]),
            neg_sent_do: Box::new([false; 256]),
            neg_sent_wont: Box::new([false; 256]),
            neg_sent_dont: Box::new([false; 256]),
            ttype_matched: false,
            ttype_raw: None,
            telnet_negotiated: false,
            window_width: None,
            window_height: None,
        }
    }

    /// Create a session for an inbound **master/slave relay** connection.
    ///
    /// On the master, a slave bridges a remote serial device's data phase
    /// to us over a relay channel (an SSH channel in P2, an in-process
    /// socket in the loopback test).  The bytes carry **raw serial
    /// semantics** end to end — no telnet IAC, no CR-NUL stuffing — so the
    /// session behaves like a directly-attached serial caller: terminal
    /// detection runs, output is raw 8-bit.  We therefore set
    /// `is_serial = true` to inherit that I/O behavior.
    ///
    /// Unlike `new_serial`, the master owns **no local serial port** for a
    /// relay caller, so `serial_port_id` is `None`.  Every "own-port" check
    /// (`self.is_serial && self.serial_port_id == Some(id)`) consequently
    /// evaluates false, which is correct: a relayed device is not attached
    /// to any of *this* gateway's ports and may freely bridge to a local
    /// port via the Serial Gateway menu.
    ///
    /// `peer_addr` is the slave's IP (the relay endpoint), so per-IP
    /// lockout accounting and logging attribute to the right host.
    /// `lockouts` is the shared map (as with `new_ssh`) so any future
    /// `authenticate()` on the relay path inherits cross-IP counting;
    /// today the relay is gated by the transport (SSH auth in P2), so a
    /// relay session — like a serial session — does not itself auth.
    ///
    /// Called by `crate::relay::run_master_relay_session`, which the
    /// master SSH relay-channel handler (`ssh.rs` `exec_request`) drives.
    pub(crate) fn new_relay(
        reader: Box<dyn tokio::io::AsyncRead + Unpin + Send>,
        writer: SharedWriter,
        shutdown: Arc<AtomicBool>,
        restart: Arc<AtomicBool>,
        peer_addr: Option<IpAddr>,
        lockouts: LockoutMap,
    ) -> Self {
        Self {
            reader,
            writer,
            shutdown,
            restart,
            current_menu: Menu::Main,
            terminal_type: TerminalType::Ascii,
            erase_char: 0x7F,
            lockouts,
            peer_addr,
            transfer_subdir: String::new(),
            xmodem_iac: false,
            web_lines: Vec::new(),
            web_scroll: 0,
            web_links: Vec::new(),
            web_history: Vec::new(),
            web_url: None,
            web_title: None,
            web_forms: Vec::new(),
            weather_location: config::get_config().weather_location,
            is_serial: true,
            is_relay: true,
            serial_port_id: None,
            is_ssh: false,
            idle_timeout: std::time::Duration::from_secs(config::get_config().idle_timeout_secs),
            pushback: None,
            neg_sent_will: Box::new([false; 256]),
            neg_sent_do: Box::new([false; 256]),
            neg_sent_wont: Box::new([false; 256]),
            neg_sent_dont: Box::new([false; 256]),
            ttype_matched: false,
            ttype_raw: None,
            telnet_negotiated: false,
            window_width: None,
            window_height: None,
        }
    }


    // ─── I/O helpers ───────────────────────────────────────

    async fn send(&mut self, text: &str) -> Result<(), std::io::Error> {
        match self.terminal_type {
            TerminalType::Petscii => {
                let swapped = swap_case_for_petscii(text);
                let bytes = to_latin1_bytes(&swapped);
                self.send_raw(&bytes).await
            }
            _ => self.send_raw(text.as_bytes()).await,
        }
    }

    async fn send_line(&mut self, text: &str) -> Result<(), std::io::Error> {
        let line = format!("{}\r\n", text);
        self.send(&line).await
    }

    /// Write user-data bytes to the session. In telnet mode, any 0xFF
    /// data byte is escaped as IAC IAC (0xFF 0xFF) per RFC 854 so the
    /// peer doesn't misinterpret it as the start of a protocol command.
    /// Serial and SSH sessions don't speak the IAC protocol, so bytes
    /// pass through unchanged there.
    async fn send_raw(&mut self, bytes: &[u8]) -> Result<(), std::io::Error> {
        let needs_escape = !self.is_serial && !self.is_ssh;
        if !needs_escape || !bytes.contains(&IAC) {
            return self.writer.lock().await.write_all(bytes).await;
        }
        let mut escaped = Vec::with_capacity(bytes.len() + 1);
        for &b in bytes {
            escaped.push(b);
            if b == IAC {
                escaped.push(IAC);
            }
        }
        self.writer.lock().await.write_all(&escaped).await
    }

    /// Write raw telnet-protocol bytes (IAC sequences) without any data
    /// escaping. Use only for sending IAC commands and option
    /// negotiation where 0xFF bytes are intentional.
    async fn send_telnet_protocol(&mut self, bytes: &[u8]) -> Result<(), std::io::Error> {
        self.writer.lock().await.write_all(bytes).await
    }

    async fn flush(&mut self) -> Result<(), std::io::Error> {
        self.writer.lock().await.flush().await
    }

    async fn clear_screen(&mut self) -> Result<(), std::io::Error> {
        match self.terminal_type {
            TerminalType::Petscii => self.send_raw(&[PETSCII_CLEAR]).await,
            TerminalType::Ansi => self.send_raw(ANSI_CLEAR.as_bytes()).await,
            TerminalType::Ascii => self.send_raw(b"\r\n\r\n\r\n").await,
        }
    }

    async fn read_byte_filtered(&mut self) -> Result<Option<u8>, std::io::Error> {
        if self.idle_timeout.is_zero() {
            self.session_read_byte().await
        } else {
            match tokio::time::timeout(self.idle_timeout, self.session_read_byte()).await {
                Ok(result) => result,
                Err(_) => Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "idle timeout",
                )),
            }
        }
    }

    /// Read a single data byte from the session. In telnet mode, IAC
    /// sequences are consumed transparently. DO/WILL option requests
    /// get WONT/DONT replies (RFC 855) except for options we support
    /// (ECHO, SGA, TTYPE, NAWS). AYT (Are You There) gets a visible
    /// reply. IP (Interrupt Process) and BRK (Break) surface as the
    /// terminal's ESC byte so callers treat them like a Ctrl+C / ESC.
    async fn session_read_byte(&mut self) -> Result<Option<u8>, std::io::Error> {
        if let Some(b) = self.pushback.take() {
            return Ok(Some(b));
        }
        let filter_iac = !self.is_serial && !self.is_ssh;
        let mut buf = [0u8; 1];
        loop {
            if self.reader.read(&mut buf).await? == 0 {
                return Ok(None);
            }
            let byte = buf[0];
            if !filter_iac || byte != IAC {
                return Ok(Some(byte));
            }
            if self.reader.read(&mut buf).await? == 0 {
                return Ok(None);
            }
            let cmd = buf[0];
            match cmd {
                IAC => return Ok(Some(IAC)), // escaped data 0xFF
                SB => {
                    self.telnet_negotiated = true;
                    let Some(payload) = self.read_subneg_payload().await? else {
                        return Ok(None);
                    };
                    if let Some((opt, body)) = payload.split_first() {
                        self.handle_subnegotiation(*opt, body).await?;
                    }
                }
                WILL | WONT | DO | DONT => {
                    self.telnet_negotiated = true;
                    if self.reader.read(&mut buf).await? == 0 {
                        return Ok(None);
                    }
                    let opt = buf[0];
                    self.handle_telnet_option(cmd, opt).await?;
                }
                AYT => {
                    // Through send_line so PETSCII case-swap applies if the
                    // terminal type is known.
                    self.send_line("[Yes]").await?;
                    self.flush().await?;
                }
                IP | BRK => {
                    let esc = if self.terminal_type == TerminalType::Petscii {
                        0x5F
                    } else {
                        0x1B
                    };
                    return Ok(Some(esc));
                }
                EC => {
                    // RFC 854: delete the last received character.  Our
                    // architecture has no low-level input buffer, so
                    // translate to DEL (0x7F); the line-input layer
                    // already handles this as backspace.
                    return Ok(Some(0x7F));
                }
                EL => {
                    // RFC 854: delete everything on the current line.
                    // Translate to NAK (0x15); the line-input loop
                    // treats this as "erase-line."
                    return Ok(Some(LINE_ERASE_BYTE));
                }
                _ => {
                    // NOP (241), DM (242), AO (245), GA (249) — consumed.
                    //
                    // DM is the SYNCH marker (RFC 854 §3).  Proper SYNCH
                    // requires reading TCP urgent-mode data; we do not
                    // implement that, so DM is informational only.
                }
            }
        }
    }

    /// Consume a subnegotiation payload up to (and including) the
    /// terminating IAC SE. Returns the payload bytes with any escaped
    /// `IAC IAC` unescaped. First byte is the option code. Returns
    /// Ok(None) if the connection closes mid-sequence.
    ///
    /// Each read is bounded by `SB_DRAIN_TIMEOUT` (slowloris guard): a peer
    /// that sends `IAC SB <opt>` and then stalls without `IAC SE` must not
    /// pin the session task and its `max_sessions` slot indefinitely.  This
    /// guard is independent of `idle_timeout_secs` (which can be 0 = off),
    /// matching the two gateway-path SB readers, which bound the identical
    /// loop the same way regardless of idle config.  A stalled drain is
    /// treated as a closed connection (Ok(None)).
    async fn read_subneg_payload(&mut self) -> Result<Option<Vec<u8>>, std::io::Error> {
        let mut payload = Vec::with_capacity(32);
        let mut buf = [0u8; 1];
        loop {
            match tokio::time::timeout(SB_DRAIN_TIMEOUT, self.reader.read(&mut buf)).await {
                Err(_) => return Ok(None),
                Ok(Ok(0)) => return Ok(None),
                Ok(Ok(_)) => {}
                Ok(Err(e)) => return Err(e),
            }
            if buf[0] != IAC {
                if payload.len() < 512 {
                    payload.push(buf[0]);
                }
                continue;
            }
            match tokio::time::timeout(SB_DRAIN_TIMEOUT, self.reader.read(&mut buf)).await {
                Err(_) => return Ok(None),
                Ok(Ok(0)) => return Ok(None),
                Ok(Ok(_)) => {}
                Ok(Err(e)) => return Err(e),
            }
            match buf[0] {
                SE => return Ok(Some(payload)),
                IAC => {
                    if payload.len() < 512 {
                        payload.push(IAC);
                    }
                }
                _ => {
                    // Malformed — skip and keep scanning for IAC SE.
                }
            }
        }
    }

    /// Reply to peer WILL/WONT/DO/DONT per RFC 855. Options we want
    /// enabled (ECHO, SGA on our side; SGA, TTYPE, NAWS on peer's side)
    /// treat the matching ack as a no-op. Everything else is refused
    /// once. DONT/WONT get a matching ack only if we had actually
    /// advertised the corresponding WILL/DO.
    async fn handle_telnet_option(
        &mut self,
        cmd: u8,
        opt: u8,
    ) -> Result<(), std::io::Error> {
        match cmd {
            DO if opt == OPT_TIMING_MARK => {
                // RFC 860: DO TIMING-MARK is a one-shot synchronization
                // request — reply with WILL TIMING-MARK *after* we have
                // flushed whatever output was queued when the DO arrived.
                // The WILL response is itself the mark; no persistent
                // state (so we don't set neg_sent_will).
                self.flush().await?;
                self.send_telnet_protocol(&[IAC, WILL, OPT_TIMING_MARK]).await?;
                self.flush().await?;
            }
            DONT if opt == OPT_TIMING_MARK => {
                // RFC 860: DONT TIMING-MARK has no action to ack since
                // we never maintain the option as enabled.
            }
            DO if opt == OPT_STATUS => {
                // RFC 859: agree to act as the status sender.  Mark
                // neg_sent_will so the peer's future DOs are treated as
                // acks and we don't loop.  A later SB STATUS SEND will
                // trigger the actual state dump.
                if !self.neg_sent_will[OPT_STATUS as usize] {
                    self.neg_sent_will[OPT_STATUS as usize] = true;
                    self.send_telnet_protocol(&[IAC, WILL, OPT_STATUS]).await?;
                    self.flush().await?;
                }
            }
            DONT if opt == OPT_STATUS => {
                // Peer withdraws the status-sender role.  Ack with WONT
                // only if we had asserted WILL.
                if self.neg_sent_will[OPT_STATUS as usize] {
                    self.neg_sent_will[OPT_STATUS as usize] = false;
                    self.send_telnet_protocol(&[IAC, WONT, OPT_STATUS]).await?;
                    self.flush().await?;
                }
            }
            WILL if opt == OPT_STATUS => {
                // We don't request status from clients — refuse.
                if !self.neg_sent_dont[OPT_STATUS as usize] {
                    self.neg_sent_dont[OPT_STATUS as usize] = true;
                    self.send_telnet_protocol(&[IAC, DONT, OPT_STATUS]).await?;
                    self.flush().await?;
                }
            }
            DO => {
                // If we already advertised WILL for opt, peer's DO is an
                // acknowledgement — no reply needed.
                if self.neg_sent_will[opt as usize] {
                    return Ok(());
                }
                if self.neg_sent_wont[opt as usize] {
                    return Ok(());
                }
                self.neg_sent_wont[opt as usize] = true;
                self.send_telnet_protocol(&[IAC, WONT, opt]).await?;
                self.flush().await?;
            }
            WILL => {
                // If we already advertised DO for opt, peer's WILL is an
                // acknowledgement — no reply needed.
                if self.neg_sent_do[opt as usize] && opt != OPT_TTYPE {
                    // TTYPE still needs SB SEND on first WILL so we can
                    // request the name; handled below.
                    return Ok(());
                }
                if opt == OPT_TTYPE {
                    if !self.neg_sent_do[opt as usize] {
                        self.neg_sent_do[opt as usize] = true;
                        self.send_telnet_protocol(&[IAC, DO, OPT_TTYPE]).await?;
                    }
                    if !self.ttype_matched {
                        self.send_telnet_protocol(&[
                            IAC, SB, OPT_TTYPE, TTYPE_SEND, IAC, SE,
                        ])
                        .await?;
                    }
                    self.flush().await?;
                    return Ok(());
                }
                if opt == OPT_NAWS {
                    if !self.neg_sent_do[opt as usize] {
                        self.neg_sent_do[opt as usize] = true;
                        self.send_telnet_protocol(&[IAC, DO, OPT_NAWS]).await?;
                        self.flush().await?;
                    }
                    return Ok(());
                }
                if self.neg_sent_dont[opt as usize] {
                    return Ok(());
                }
                self.neg_sent_dont[opt as usize] = true;
                self.send_telnet_protocol(&[IAC, DONT, opt]).await?;
                self.flush().await?;
            }
            DONT => {
                // Acknowledge with WONT only if we had previously
                // advertised WILL for opt.
                if self.neg_sent_will[opt as usize]
                    && !self.neg_sent_wont[opt as usize]
                {
                    self.neg_sent_wont[opt as usize] = true;
                    self.send_telnet_protocol(&[IAC, WONT, opt]).await?;
                    self.flush().await?;
                }
            }
            WONT => {
                // Acknowledge with DONT only if we had previously
                // advertised DO for opt.
                if self.neg_sent_do[opt as usize]
                    && !self.neg_sent_dont[opt as usize]
                {
                    self.neg_sent_dont[opt as usize] = true;
                    self.send_telnet_protocol(&[IAC, DONT, opt]).await?;
                    self.flush().await?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Process a completed subnegotiation. `body` is the payload after
    /// the option code. TTYPE IS sets terminal_type if the reported
    /// name is recognized; NAWS stores the reported window dimensions.
    async fn handle_subnegotiation(
        &mut self,
        opt: u8,
        body: &[u8],
    ) -> Result<(), std::io::Error> {
        match opt {
            OPT_TTYPE => {
                if body.first().copied() == Some(TTYPE_IS) && !self.ttype_matched {
                    let name_bytes = &body[1..];
                    let name: String = name_bytes
                        .iter()
                        .map(|&b| b as char)
                        .filter(|c| !c.is_control())
                        .collect();
                    // Record what the client announced even when we don't
                    // recognize it, so the gateway-debug terminal diagnostic
                    // can show the exact name that failed to match.
                    self.ttype_raw = Some(name.clone());
                    if let Some(tt) = match_terminal_name(&name) {
                        self.terminal_type = tt;
                        self.ttype_matched = true;
                    }
                }
            }
            OPT_STATUS => {
                // RFC 859: only the SEND request needs a response.  The
                // IS variant (a peer dumping its state to us) is ignored
                // — we don't maintain a model of peer options.
                if body.first().copied() == Some(STATUS_SEND)
                    && self.neg_sent_will[OPT_STATUS as usize]
                {
                    self.send_status_is().await?;
                }
            }
            OPT_NAWS => {
                if body.len() >= 4 {
                    let w = u16::from_be_bytes([body[0], body[1]]);
                    let h = u16::from_be_bytes([body[2], body[3]]);
                    if w > 0 {
                        self.window_width = Some(w);
                    }
                    if h > 0 {
                        self.window_height = Some(h);
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Emit `IAC SB STATUS IS <state> IAC SE` in response to a peer's
    /// `IAC SB STATUS SEND IAC SE` (RFC 859).
    ///
    /// The state body is a concatenation of `WILL opt` and `DO opt`
    /// triplets for every option we have advertised and not had denied.
    /// Any 0xFF byte inside the body (none of our opts are 0xFF, but the
    /// RFC requires it) is doubled per IAC escaping rules.
    async fn send_status_is(&mut self) -> Result<(), std::io::Error> {
        let mut body = vec![IAC, SB, OPT_STATUS, STATUS_IS];
        for opt in 0u8..=255u8 {
            let idx = opt as usize;
            if self.neg_sent_will[idx] && !self.neg_sent_wont[idx] {
                body.push(WILL);
                if opt == IAC {
                    body.push(IAC);
                }
                body.push(opt);
            }
            if self.neg_sent_do[idx] && !self.neg_sent_dont[idx] {
                body.push(DO);
                if opt == IAC {
                    body.push(IAC);
                }
                body.push(opt);
            }
            if opt == 255 {
                break;
            }
        }
        body.push(IAC);
        body.push(SE);
        self.send_telnet_protocol(&body).await?;
        self.flush().await
    }

    /// Consume up to `max` immediately-queued CR/LF bytes left behind by a
    /// linemode telnet client (e.g. the `\n` of a CRLF pair after a menu
    /// selection or line submit). Uses a short read timeout so nothing is
    /// eaten in char-at-a-time mode. Any non-CR/LF byte seen is pushed back
    /// for the next real input call, so no keystrokes are lost.
    async fn drain_trailing_eol(&mut self, max: usize) {
        if self.pushback.is_some() {
            return;
        }
        for _ in 0..max {
            let res = tokio::time::timeout(
                std::time::Duration::from_millis(20),
                self.session_read_byte(),
            )
            .await;
            match res {
                Ok(Ok(Some(b))) if b == b'\r' || b == b'\n' => continue,
                Ok(Ok(Some(b))) => {
                    self.pushback = Some(b);
                    return;
                }
                _ => return,
            }
        }
    }

    async fn echo_backspace(&mut self) -> Result<(), std::io::Error> {
        match self.terminal_type {
            TerminalType::Petscii => self.send_raw(&[0x9D, 0x20, 0x9D]).await,
            _ => self.send_raw(&[0x08, 0x20, 0x08]).await,
        }
    }

    async fn get_line_input(&mut self) -> Result<Option<String>, std::io::Error> {
        self.read_input_loop(&mut Vec::new(), InputMode::Normal).await
    }

    async fn get_password_input(&mut self) -> Result<Option<String>, std::io::Error> {
        self.read_input_loop(&mut Vec::new(), InputMode::Password).await
    }

    /// Core input loop shared by `get_line_input` and `get_password_input`.
    /// In `Normal` mode, typed characters are echoed
    /// and the result is trimmed. In `Password` mode, `*` is echoed instead and
    /// the result is returned untrimmed.
    async fn read_input_loop(
        &mut self,
        buf: &mut Vec<u8>,
        mode: InputMode,
    ) -> Result<Option<String>, std::io::Error> {
        let is_password = matches!(mode, InputMode::Password);
        loop {
            let byte = match self.read_byte_filtered().await? {
                Some(b) => b,
                None => return Ok(None),
            };

            if byte == b'\r' || byte == b'\n' {
                self.send_raw(b"\r\n").await?;
                self.flush().await?;
                // Drain the paired byte of a CRLF (or LFCR) so the next
                // prompt isn't silently satisfied by a leftover newline.
                self.drain_trailing_eol(1).await;
                let result: String = if self.terminal_type == TerminalType::Petscii {
                    buf.iter()
                        .map(|&b| petscii_to_ascii_byte(b) as char)
                        .collect()
                } else {
                    buf.iter().map(|&b| b as char).collect()
                };
                return Ok(Some(if is_password {
                    result
                } else {
                    result.trim().to_string()
                }));
            }

            if is_esc_key(byte, self.terminal_type == TerminalType::Petscii) {
                self.drain_input().await;
                return Ok(None);
            }

            if is_backspace_key(byte, self.erase_char) {
                if !buf.is_empty() {
                    buf.pop();
                    self.echo_backspace().await?;
                    self.flush().await?;
                }
                continue;
            }

            if byte == LINE_ERASE_BYTE {
                // RFC 854 EL (delivered by session_read_byte as 0x15).
                // Erase the current line both in the buffer and on the
                // user's terminal.
                while !buf.is_empty() {
                    buf.pop();
                    self.echo_backspace().await?;
                }
                self.flush().await?;
                continue;
            }

            if byte < 0x20 {
                continue;
            }

            if buf.len() >= MAX_INPUT_LENGTH {
                self.send_raw(b"\r\n").await?;
                self.show_error("Input too long.").await?;
                return Ok(None);
            }

            if is_password {
                self.send_raw(b"*").await?;
            } else {
                self.send_raw(&[byte]).await?;
            }
            self.flush().await?;
            buf.push(byte);
        }
    }

    async fn get_menu_input(
        &mut self,
        instant_digits: bool,
    ) -> Result<Option<String>, std::io::Error> {
        // ZMODEM autostart detection state.  A compliant ZMODEM sender
        // opens a transfer with `** ZDLE <header-type>` where
        // `<header-type>` is one of `A` (binary/CRC-16), `B` (hex),
        // or `C` (binary/CRC-32).  Reading the full four-byte prefix
        // off the menu input loop is an unambiguous "the user's
        // terminal just tried to auto-start a ZMODEM transfer" signal
        // — bridge directly into the ZMODEM receive flow so the upload
        // succeeds without the user having to navigate the menu first.
        let mut zmodem_state: u8 = 0;
        loop {
            let byte = match self.read_byte_filtered().await? {
                Some(b) => b,
                None => return Ok(None),
            };

            // ZMODEM autostart: **\x18[ABC].
            match (zmodem_state, byte) {
                (0, b'*') => {
                    zmodem_state = 1;
                    continue;
                }
                (1, b'*') => {
                    zmodem_state = 2;
                    continue;
                }
                (2, 0x18) => {
                    zmodem_state = 3;
                    continue;
                }
                (3, b'A') | (3, b'B') | (3, b'C') => {
                    self.handle_zmodem_autostart().await?;
                    // Bounce back to the caller so the menu redraws.
                    return Ok(None);
                }
                _ => {
                    zmodem_state = 0;
                    // Fall through and process `byte` normally.
                }
            }

            if is_esc_key(byte, self.terminal_type == TerminalType::Petscii) {
                self.drain_input().await;
                return Ok(None);
            }

            if byte == b'\r' || byte == b'\n' {
                continue;
            }
            if is_backspace_key(byte, self.erase_char) {
                continue;
            }
            if byte < 0x20 {
                continue;
            }

            let ch = if self.terminal_type == TerminalType::Petscii {
                (petscii_to_ascii_byte(byte) as char).to_ascii_lowercase()
            } else {
                (byte as char).to_ascii_lowercase()
            };

            if ch.is_ascii_alphabetic() {
                self.send_raw(&[byte]).await?;
                self.send_raw(b"\r\n").await?;
                self.flush().await?;
                // Linemode clients send `letter\r\n`; drop the trailing
                // CRLF so a follow-up prompt isn't auto-submitted.
                self.drain_trailing_eol(2).await;
                return Ok(Some(ch.to_string()));
            }

            if ch.is_ascii_digit() {
                if instant_digits {
                    self.send_raw(&[byte]).await?;
                    self.send_raw(b"\r\n").await?;
                    self.flush().await?;
                    self.drain_trailing_eol(2).await;
                    return Ok(Some(ch.to_string()));
                }

                self.send_raw(&[byte]).await?;
                self.flush().await?;
                let mut input = String::new();
                input.push(ch);

                loop {
                    let b2 = match self.read_byte_filtered().await? {
                        Some(b) => b,
                        None => return Ok(None),
                    };

                    if b2 == b'\r' || b2 == b'\n' {
                        self.send_raw(b"\r\n").await?;
                        self.flush().await?;
                        self.drain_trailing_eol(1).await;
                        return Ok(Some(input));
                    }

                    if is_esc_key(b2, self.terminal_type == TerminalType::Petscii) {
                        self.drain_input().await;
                        return Ok(None);
                    }

                    if is_backspace_key(b2, self.erase_char) {
                        if !input.is_empty() {
                            input.pop();
                            self.echo_backspace().await?;
                            self.flush().await?;
                        }
                        continue;
                    }

                    if b2 < 0x20 {
                        continue;
                    }

                    let ch2 = if self.terminal_type == TerminalType::Petscii {
                        petscii_to_ascii_byte(b2) as char
                    } else {
                        b2 as char
                    };

                    if ch2.is_ascii_digit() && input.len() < MAX_INPUT_LENGTH {
                        self.send_raw(&[b2]).await?;
                        self.flush().await?;
                        input.push(ch2);
                    }
                }
            }

            self.send_raw(&[byte]).await?;
            self.send_raw(b"\r\n").await?;
            self.flush().await?;
            self.drain_trailing_eol(2).await;
            return Ok(Some(ch.to_string()));
        }
    }

    async fn wait_for_key(&mut self) -> Result<(), std::io::Error> {
        loop {
            match self.read_byte_filtered().await? {
                Some(b)
                    if b >= 0x20
                        || b == b'\r'
                        || b == b'\n'
                        || is_esc_key(b, self.terminal_type == TerminalType::Petscii) =>
                {
                    return Ok(());
                }
                None => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::ConnectionAborted,
                        "disconnected",
                    ));
                }
                _ => continue,
            }
        }
    }

    async fn drain_input(&mut self) {
        self.drain_input_until_quiet(50, None).await;
    }

    /// Read and discard pending input until the line is quiet for `quiet_ms`,
    /// optionally capped at `max_ms` total wall-clock so a peer that is still
    /// actively streaming can't stall us forever.
    ///
    /// The default `drain_input` uses a short 50ms gap, fine for clearing the
    /// dribble left by a menu keystroke.  Before a *file transfer* we drain
    /// with a longer gap (see the transfer-start call sites): at 1200 baud a
    /// 50ms gap is only ~6 char-times, so a peer flushing its serial buffer in
    /// a late burst — e.g. CCGMS after a silent Punter cancel, which sends no
    /// wire byte to signal the abort — can dribble stale bytes past a 50ms
    /// drain and have them mistaken for the next transfer's opening handshake.
    /// This drain runs after we print "start within N seconds" but before the
    /// human has started their sender, so a longer gap never eats a legitimate
    /// opening code.
    async fn drain_input_until_quiet(&mut self, quiet_ms: u64, max_ms: Option<u64>) {
        let deadline = max_ms
            .map(|m| std::time::Instant::now() + std::time::Duration::from_millis(m));
        while let Ok(Ok(Some(_))) = tokio::time::timeout(
            std::time::Duration::from_millis(quiet_ms),
            self.session_read_byte(),
        )
        .await
        {
            if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
                break;
            }
        }
    }

    /// Handle a detected ZMODEM autostart prefix (`**\x18[ABC]`) on the
    /// menu input stream.  The four leading bytes have already been
    /// consumed by the menu state machine; the rest of the partial
    /// ZRQINIT header (and any retransmits) sit on the wire.  We drain
    /// them, set up the transfer directory, and hand off to the regular
    /// `zmodem_receive` flow — once we emit our `rz\r` + ZRINIT the
    /// sender's protocol retry will resync regardless of what we just
    /// drained.  Files are saved using the sender's filename (after
    /// path validation), with `apply_ymodem_meta` applied for mtime /
    /// mode so the upload behaves identically to a menu-initiated
    /// `Z` upload.
    async fn handle_zmodem_autostart(&mut self) -> Result<(), std::io::Error> {
        glog!("File transfer: ZMODEM autostart detected; switching to receive");
        // Drain residual ZRQINIT bytes the sender already pushed before
        // we got a chance to start the receiver — the sender will
        // retransmit once it sees our ZRINIT below.
        self.drain_input().await;

        self.ensure_transfer_dir().await?;
        if Self::is_disk_full() {
            self.show_error("Disk space is low. Uploads disabled.")
                .await?;
            return Ok(());
        }

        self.send_line("").await?;
        self.send_line(&format!(
            "  {}",
            self.green("ZMODEM upload detected — receiving...")
        ))
        .await?;
        self.flush().await?;

        let verbose = config::get_config().verbose;
        let target_dir = self.transfer_path();
        let target_dir_for_decide = target_dir.clone();
        // Auto-accept anything with a valid filename that doesn't
        // already exist.  Same sanitation as the interactive batch
        // upload's "subsequent files" path.
        let decide = move |_idx: usize, sender_name: &str, _size: Option<u64>| -> bool {
            if Self::validate_filename(sender_name).is_err() {
                return false;
            }
            !target_dir_for_decide.join(sender_name).exists()
        };

        let start = std::time::Instant::now();
        let result = {
            let mut writer_guard = self.writer.lock().await;
            crate::zmodem::zmodem_receive(
                &mut self.reader,
                &mut *writer_guard,
                self.xmodem_iac,
                verbose,
                decide,
            )
            .await
        };
        let elapsed = start.elapsed();

        let received = match result {
            Ok(rxs) => rxs,
            Err(e) => {
                self.post_transfer_settle().await;
                self.show_error(&format!("ZMODEM receive failed: {}", e))
                    .await?;
                return Ok(());
            }
        };

        // Save each accepted file with the sender's name + metadata.
        // Files the decide closure rejected aren't in `received` at all
        // — the sender saw a ZSKIP and moved on — so any skips here
        // are post-receive failures (write error, race on existence).
        let mut saved: Vec<(String, usize)> = Vec::new();
        let mut skipped: Vec<(String, &'static str)> = Vec::new();
        for rx in &received {
            if Self::validate_filename(&rx.filename).is_err() {
                // Sanitize the sender-supplied name before it can reach the
                // terminal in the skipped summary (it may carry ANSI escapes).
                skipped.push((crate::aichat::sanitize_for_terminal(&rx.filename), "invalid filename"));
                continue;
            }
            let filepath = target_dir.join(&rx.filename);
            // Atomic create-only open — closes the TOCTOU window
            // between an `exists()` check and the write that
            // `std::fs::write` would leave open, and async lets the
            // 8 MB cap not block the tokio executor.
            let meta = (rx.modtime.is_some() || rx.mode.is_some())
                .then_some(crate::xmodem::YmodemReceiveMeta {
                    size: None,
                    modtime: rx.modtime,
                    mode: rx.mode,
                });
            match Self::save_received_file(&filepath, &rx.data, meta.as_ref()).await {
                Ok(()) => saved.push((rx.filename.clone(), rx.data.len())),
                Err(SaveError::AlreadyExists) => {
                    skipped.push((rx.filename.clone(), "already exists"));
                }
                Err(SaveError::WriteFailed) => {
                    skipped.push((rx.filename.clone(), "write failed"));
                }
            }
        }

        self.post_transfer_settle().await;
        self.send_line("").await?;
        self.send_line(&format!(
            "  ZMODEM upload completed in {:.1}s.",
            elapsed.as_secs_f64()
        ))
        .await?;
        self.send_line(&format!(
            "  Received: {} file(s), saved: {}, skipped: {}.",
            received.len(),
            saved.len(),
            skipped.len()
        ))
        .await?;
        for (name, size) in &saved {
            self.send_line(&format!(
                "    {} {} ({} bytes)",
                self.green("✓"),
                self.amber(name),
                size
            ))
            .await?;
        }
        for (name, reason) in &skipped {
            self.send_line(&format!(
                "    {} {} ({})",
                self.red("✗"),
                self.amber(name),
                reason
            ))
            .await?;
        }
        self.send_line("").await?;
        self.send("  Press any key to continue.").await?;
        self.flush().await?;
        let _ = self.wait_for_key().await;
        Ok(())
    }

    async fn show_error(&mut self, msg: &str) -> Result<(), std::io::Error> {
        self.send_line(&format!("  {}", self.red(msg))).await?;
        self.send_line("").await?;
        self.send("  Press any key to continue.").await?;
        self.flush().await?;
        self.wait_for_key().await?;
        Ok(())
    }

    /// Option 4 (`punter_hangup_on_failure`): C1 has no in-band abort, so when
    /// a Punter transfer gives up the C64 is left spinning in its own retry
    /// loop until its (long) internal timeout.  Dropping the connection makes
    /// the modem bridge signal loss-of-carrier so the C64 exits its transfer
    /// at once.  Sends a short notice — no `wait_for_key`, since the peer is
    /// mid-protocol and won't press a key — and returns `ConnectionAborted`.
    /// `handle_file_transfer_command` propagates that kind up through `run()`,
    /// whose caller unconditionally shuts down the writer (telnet TCP socket /
    /// SSH channel), which is the carrier drop.
    async fn punter_hangup(&mut self) -> Result<(), std::io::Error> {
        self.send_line("").await?;
        self.send_line(&format!(
            "  {}",
            self.amber("Dropping carrier to release the C64.")
        ))
        .await?;
        self.flush().await?;
        Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionAborted,
            "Punter transfer failed; hanging up to release the peer",
        ))
    }

    /// Pause after an XMODEM/YMODEM transfer so the client's own
    /// transfer dialog finishes closing and the underlying terminal is
    /// visible again before we print status.  Drains trailing bytes
    /// from the client's post-transfer chatter (NAWS updates, stray
    /// CR/LF from a dialog-dismiss keypress, etc.) so the subsequent
    /// `wait_for_key` actually waits for a human keypress instead of
    /// being satisfied by leftover noise.
    async fn post_transfer_settle(&mut self) {
        self.drain_input().await;
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        self.drain_input().await;
    }

    /// Show a multi-line informational message and wait for a keypress.
    async fn show_error_lines(&mut self, lines: &[&str]) -> Result<(), std::io::Error> {
        self.send_line("").await?;
        for line in lines {
            self.send_line(&format!("  {}", line)).await?;
        }
        self.send_line("").await?;
        self.send("  Press any key to continue.").await?;
        self.flush().await?;
        self.wait_for_key().await?;
        Ok(())
    }

    /// Show a full-screen help page with a header and wait for a keypress.
    /// Split help content into pages that each fit within `max_per_page`
    /// lines.  Prefers breaking at **blank lines** so a logical group —
    /// a section header plus its continuation lines, a letter-command
    /// plus its description — stays together on a single page.  Falls
    /// back to a hard split at `max_per_page` only if no blank exists
    /// within the range; authors avoid that path by separating groups
    /// with a blank line in the help content.
    ///
    /// The returned pages have trailing blanks stripped and leading
    /// blanks skipped so each page renders cleanly without drifting
    /// chrome.
    fn paginate_help<'a>(
        lines: &'a [&'a str],
        max_per_page: usize,
    ) -> Vec<Vec<&'a str>> {
        assert!(max_per_page >= 1, "max_per_page must be ≥ 1");
        fn is_blank(s: &str) -> bool {
            s.trim().is_empty()
        }
        let mut pages: Vec<Vec<&'a str>> = Vec::new();
        let mut remaining: &[&str] = lines;
        while !remaining.is_empty() {
            let take = remaining.len().min(max_per_page);
            // Prefer splitting at the last blank line within `take`.
            // Falling back to `take` only when no blank exists in range
            // — authors should avoid this by separating groups with
            // blanks, but we don't want to loop forever on malformed
            // input.
            let mut split = take;
            for i in (1..=take).rev() {
                if is_blank(remaining[i - 1]) {
                    split = i;
                    break;
                }
            }
            // Emit the page with trailing blanks trimmed.
            let mut page: Vec<&str> = remaining[..split].to_vec();
            while page.last().is_some_and(|s| is_blank(s)) {
                page.pop();
            }
            if !page.is_empty() {
                pages.push(page);
            }
            // Skip leading blanks on the next page so the header isn't
            // followed by an awkward empty line.
            remaining = &remaining[split..];
            while !remaining.is_empty() && is_blank(remaining[0]) {
                remaining = &remaining[1..];
            }
        }
        pages
    }

    async fn show_help_page(
        &mut self,
        title: &str,
        lines: &[&str],
    ) -> Result<(), std::io::Error> {
        // Chrome is 6 rows: sep(1) + title(1) + sep(1) + blank(1) +
        // blank(1) + footer(1).  PETSCII renders 22 usable rows on a
        // 25-line Commodore 64, so 22 - 6 = 16 content rows.  We use 15
        // to leave a little breathing room for terminals that occasionally
        // push an extra line at the bottom.
        const MAX_CONTENT_LINES: usize = 15;

        let pages = Self::paginate_help(lines, MAX_CONTENT_LINES);
        // Empty content is rare but possible; treat it as one blank page
        // so the caller still gets the usual "Press any key" affordance.
        let pages: Vec<Vec<&str>> = if pages.is_empty() {
            vec![Vec::new()]
        } else {
            pages
        };
        let total = pages.len();

        for (idx, page_lines) in pages.iter().enumerate() {
            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            self.send_line(&format!("  {}", self.yellow(title))).await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;
            for line in page_lines {
                self.send_line(line).await?;
            }
            self.send_line("").await?;

            let is_last = idx + 1 == total;
            let prompt = if total == 1 {
                "  Press any key to continue.".to_string()
            } else if is_last {
                format!("  Page {}/{} - Press any key.", idx + 1, total)
            } else {
                format!("  Page {}/{} - next key, Q to quit", idx + 1, total)
            };
            self.send(&prompt).await?;
            self.flush().await?;

            let key = self.wait_for_key_returning().await?;
            // Early-exit on Q between pages.  ESC also bails out so the
            // existing "escape twice means leave this screen" reflex
            // works on help screens too.
            if !is_last
                && (matches!(key, b'q' | b'Q')
                    || is_esc_key(key, self.terminal_type == TerminalType::Petscii))
            {
                break;
            }
        }
        Ok(())
    }

    /// Variant of `wait_for_key` that returns the byte that unblocked
    /// it.  Used by paginated help screens so they can react to `Q`
    /// (quit) or ESC during multi-page navigation.
    async fn wait_for_key_returning(&mut self) -> Result<u8, std::io::Error> {
        loop {
            match self.read_byte_filtered().await? {
                Some(b)
                    if b >= 0x20
                        || b == b'\r'
                        || b == b'\n'
                        || is_esc_key(b, self.terminal_type == TerminalType::Petscii) =>
                {
                    return Ok(b);
                }
                None => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::ConnectionAborted,
                        "disconnected",
                    ));
                }
                _ => continue,
            }
        }
    }

    // ─── Terminal detection ─────────────────────────────────

    async fn detect_terminal_type(&mut self) -> Result<(), std::io::Error> {
        // Serial callers don't speak the telnet protocol — dialing
        // ATDT ETHERNET-GATEWAY puts a raw byte stream on the wire, so
        // IAC bytes (0xFF) would render as garbage characters on the
        // C64/CP/M terminal.  Skip option negotiation and go straight
        // to the BACKSPACE prompt for serial.
        if !self.is_serial {
            // Advertise server-side echo + char-at-a-time mode, and request
            // terminal type + window size from the client. Mark the DOs as
            // sent so a client-initiated WILL TTYPE / WILL NAWS is treated
            // as an acknowledgement instead of triggering a duplicate DO.
            self.send_telnet_protocol(&[
                IAC, WILL, OPT_ECHO,
                IAC, WILL, OPT_SGA,
                IAC, DO, OPT_SGA,
                IAC, DO, OPT_TTYPE,
                IAC, DO, OPT_NAWS,
            ])
            .await?;
            self.neg_sent_will[OPT_ECHO as usize] = true;
            self.neg_sent_will[OPT_SGA as usize] = true;
            self.neg_sent_do[OPT_SGA as usize] = true;
            self.neg_sent_do[OPT_TTYPE as usize] = true;
            self.neg_sent_do[OPT_NAWS as usize] = true;
            self.flush().await?;

            // Give the client a moment to respond, then process negotiation
            // replies (including any TTYPE IS / NAWS subnegotiations).
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            self.drain_input().await;
        }

        // If TTYPE already identified the client, skip the manual prompt.
        // `detect_method` records how the terminal type was decided, for
        // the gateway-debug terminal diagnostic emitted below.
        let detect_method;
        if self.ttype_matched {
            self.erase_char = match self.terminal_type {
                TerminalType::Petscii => 0x14,
                _ => 0x7F,
            };
            detect_method = format!(
                "telnet TTYPE \"{}\"",
                self.ttype_raw.as_deref().unwrap_or("?")
            );
        } else {
            self.send_raw(b"\r\nPress BACKSPACE to detect terminal: ")
                .await?;
            self.flush().await?;

            let byte = match tokio::time::timeout(
                std::time::Duration::from_secs(60),
                self.read_byte_filtered(),
            )
            .await
            {
                Ok(result) => match result? {
                    Some(b) => b,
                    None => return Ok(()),
                },
                Err(_) => {
                    self.send_raw(b"\r\n\r\n  Disconnected: idle timeout.\r\n\r\n")
                        .await?;
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "idle timeout during terminal detection",
                    ));
                }
            };

            self.erase_char = byte;
            self.terminal_type = match byte {
                0x14 => TerminalType::Petscii,
                0x08 | 0x7F => TerminalType::Ansi,
                _ => TerminalType::Ascii,
            };
            detect_method = format!("BACKSPACE key 0x{:02x}", byte);
        }

        let type_name = match self.terminal_type {
            TerminalType::Petscii => "PETSCII (Commodore 64)",
            TerminalType::Ansi => "ANSI",
            TerminalType::Ascii => "ASCII",
        };
        self.send(&format!("\r\nTerminal detected: {}\r\n", type_name))
            .await?;
        self.flush().await?;
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        self.drain_input().await;

        // Color preference — user must explicitly choose Y or N
        let color_label = match self.terminal_type {
            TerminalType::Petscii => "PETSCII",
            _ => "ANSI",
        };
        self.send(&format!(
            "Use {} color? (Y/N): ",
            color_label
        ))
        .await?;
        self.flush().await?;

        let accepted = loop {
            let color_byte = match tokio::time::timeout(
                std::time::Duration::from_secs(60),
                self.read_byte_filtered(),
            )
            .await
            {
                Ok(result) => match result? {
                    Some(b) => b,
                    None => return Ok(()),
                },
                Err(_) => {
                    self.send_raw(b"\r\n\r\n  Disconnected: idle timeout.\r\n\r\n")
                        .await?;
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "idle timeout during color selection",
                    ));
                }
            };

            let choice = if self.terminal_type == TerminalType::Petscii {
                petscii_to_ascii_byte(color_byte)
            } else {
                color_byte
            };

            match choice {
                b'y' | b'Y' => {
                    self.send_raw(&[color_byte]).await?;
                    self.send_raw(b"\r\n").await?;
                    self.flush().await?;
                    break true;
                }
                b'n' | b'N' => {
                    self.send_raw(&[color_byte]).await?;
                    self.send_raw(b"\r\n").await?;
                    self.flush().await?;
                    break false;
                }
                _ => continue, // ignore other keys
            }
        };

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        self.drain_input().await;

        if accepted {
            if self.terminal_type == TerminalType::Ascii {
                self.terminal_type = TerminalType::Ansi;
                self.send_raw(b"ANSI color enabled.\r\n").await?;
            }
        } else if self.terminal_type != TerminalType::Ascii {
            self.terminal_type = TerminalType::Ascii;
            self.send_raw(b"Color disabled.\r\n").await?;
        }

        self.send_raw(b"\r\n").await?;
        self.flush().await?;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        self.drain_input().await;

        self.log_terminal_diagnostic(&detect_method, accepted);

        Ok(())
    }

    /// Emit a one-shot, human-readable terminal diagnostic to the gateway
    /// log — but only when gateway-debug tracing is on (the `gateway_debug`
    /// config flag, toggleable from the GUI / web console / Serial Config
    /// menu, or the `EGATEWAY_GATEWAY_DEBUG` env var).  This is the single
    /// place that explains *why a caller did or didn't get color*: the
    /// detected terminal type and how it was decided, the raw TTYPE the
    /// client announced (matched or not), the color choice, the NAWS window
    /// size, the telnet options we advertised, what we'll advertise onward
    /// to a remote host, and — for serial callers — the dialed port's baud
    /// and PETSCII-translate state.  PETSCII translate strips ANSI color
    /// sequences before they reach the caller, which is the most common
    /// reason ANSI color goes missing on a serial line, so it's called out
    /// explicitly.  Costs nothing when the flag and env var are both unset.
    fn log_terminal_diagnostic(&self, detect_method: &str, color_answer: bool) {
        if !gw_debug_enabled(config::get_gateway_debug()) {
            return;
        }

        // Only telnet/serial callers reach here — `detect_terminal_type`
        // (the sole caller) is gated behind `!self.is_ssh`.
        let session = if self.is_serial {
            match self.serial_port_id {
                Some(id) => format!("serial port {}", id.label()),
                None => "serial".to_string(),
            }
        } else {
            "telnet (TCP)".to_string()
        };

        let tt = match self.terminal_type {
            TerminalType::Petscii => "PETSCII",
            TerminalType::Ansi => "ANSI",
            TerminalType::Ascii => "ASCII",
        };

        let color = match (self.terminal_type, color_answer) {
            (TerminalType::Ascii, _) => "DISABLED — plain text",
            (_, true) => "ENABLED — caller answered Y",
            (_, false) => "DISABLED — caller answered N",
        };

        let ttype_line = match &self.ttype_raw {
            Some(name) => {
                let matched = match match_terminal_name(name) {
                    Some(TerminalType::Petscii) => "recognized as PETSCII",
                    Some(TerminalType::Ansi) => "recognized as ANSI",
                    Some(TerminalType::Ascii) => "recognized as ASCII",
                    None => "UNRECOGNIZED -> fell back to BACKSPACE probe",
                };
                format!("\"{}\" ({})", name, matched)
            }
            None if self.is_serial => {
                "<none — serial connections skip telnet negotiation>".to_string()
            }
            None => "<none — client sent no TERMINAL-TYPE>".to_string(),
        };

        // What we advertised for each key option, as a will/do summary.
        // For serial these are all "-" (no telnet negotiation happens).
        let opt_state = |opt: u8| -> &'static str {
            let i = opt as usize;
            let willed = self.neg_sent_will[i] && !self.neg_sent_wont[i];
            let doed = self.neg_sent_do[i] && !self.neg_sent_dont[i];
            match (willed, doed) {
                (true, true) => "will+do",
                (true, false) => "will",
                (false, true) => "do",
                (false, false) => "-",
            }
        };

        let window = match (self.window_width, self.window_height) {
            (Some(w), Some(h)) => format!("{}x{}", w, h),
            (Some(w), None) => format!("{}x?", w),
            (None, Some(h)) => format!("?x{}", h),
            (None, None) => "<not negotiated>".to_string(),
        };

        let ssh_term = match self.terminal_type {
            TerminalType::Petscii => "dumb (40x25)",
            TerminalType::Ascii => "dumb (80x24)",
            TerminalType::Ansi => "xterm (80x24)",
        };

        let cfg = config::get_config();

        glog!("[gw-diag] ----- terminal diagnostic ----------------------------");
        glog!("[gw-diag] session:         {}", session);
        glog!("[gw-diag] terminal type:   {}  (via {})", tt, detect_method);
        glog!("[gw-diag] color:           {}", color);
        glog!("[gw-diag] erase char:      0x{:02x}", self.erase_char);
        glog!("[gw-diag] TTYPE reported:  {}", ttype_line);
        glog!(
            "[gw-diag] telnet opts:     ECHO={} SGA={} TTYPE={} NAWS={}  (peer spoke telnet: {})",
            opt_state(OPT_ECHO),
            opt_state(OPT_SGA),
            opt_state(OPT_TTYPE),
            opt_state(OPT_NAWS),
            if self.telnet_negotiated { "yes" } else { "no" },
        );
        glog!("[gw-diag] window (NAWS):   {}", window);
        glog!(
            "[gw-diag] onward advertise: telnet TTYPE=\"{}\"  |  ssh TERM={}",
            gateway_terminal_name(self.terminal_type),
            ssh_term,
        );
        glog!(
            "[gw-diag] config:          telnet_gateway_negotiate={}",
            cfg.telnet_gateway_negotiate,
        );

        if self.is_serial
            && let Some(id) = self.serial_port_id
        {
            let p = cfg.port(id);
            glog!(
                "[gw-diag] serial port {}:   baud={} petscii_translate={}",
                id.label(),
                p.baud,
                if p.petscii_translate { "ON" } else { "off" },
            );
            if p.petscii_translate && self.terminal_type == TerminalType::Ansi {
                glog!(
                    "[gw-diag] *** PETSCII translate is ON: ANSI color sequences are STRIPPED"
                );
                glog!(
                    "[gw-diag] *** before reaching the caller — that produces black & white output."
                );
                glog!(
                    "[gw-diag] *** For ANSI color on this port, turn PETSCII translate OFF",
                );
                glog!(
                    "[gw-diag] *** (AT+PETSCII=0, or the Serial Configuration menu)."
                );
            }
        }

        glog!("[gw-diag] ------------------------------------------------------");
    }

    // ─── Authentication ─────────────────────────────────────

    async fn authenticate(&mut self) -> Result<bool, std::io::Error> {
        if let Some(ip) = self.peer_addr
            && is_locked_out(&self.lockouts, ip)
        {
            glog!("Telnet: auth rejected for {} (locked out)", ip);
            self.send_line("").await?;
            self.send_line(&format!(
                "  {}",
                self.red("Too many attempts. Try later.")
            ))
            .await?;
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            return Ok(false);
        }

        let cfg = config::get_config();
        let idle_timeout = std::time::Duration::from_secs(cfg.idle_timeout_secs);
        let sep = self.separator();
        self.clear_screen().await?;
        self.send_line(&sep).await?;
        self.send_line(&format!("  {}", self.yellow("ETHERNET GATEWAY")))
            .await?;
        self.send_line(&sep).await?;
        self.send_line("").await?;

        for attempt in 1..=MAX_AUTH_ATTEMPTS {
            self.send(&format!("  {} ", self.cyan("Username:")))
                .await?;
            self.flush().await?;
            let username = if idle_timeout.is_zero() {
                match self.get_line_input().await {
                    Ok(Some(s)) => s,
                    Ok(None) => return Ok(false),
                    Err(e) => return Err(e),
                }
            } else {
                match tokio::time::timeout(idle_timeout, self.get_line_input()).await {
                    Ok(Ok(Some(s))) => s,
                    Ok(Ok(None)) => return Ok(false),
                    Ok(Err(e)) => return Err(e),
                    Err(_) => {
                        let _ = self
                            .send_line("\r\nDisconnected: idle timeout.")
                            .await;
                        return Ok(false);
                    }
                }
            };

            self.send(&format!("  {} ", self.cyan("Password:")))
                .await?;
            self.flush().await?;
            let password = if idle_timeout.is_zero() {
                match self.get_password_input().await {
                    Ok(Some(s)) => s,
                    Ok(None) => return Ok(false),
                    Err(e) => return Err(e),
                }
            } else {
                match tokio::time::timeout(idle_timeout, self.get_password_input()).await {
                    Ok(Ok(Some(s))) => s,
                    Ok(Ok(None)) => return Ok(false),
                    Ok(Err(e)) => return Err(e),
                    Err(_) => {
                        let _ = self
                            .send_line("\r\nDisconnected: idle timeout.")
                            .await;
                        return Ok(false);
                    }
                }
            };

            // Evaluate BOTH comparisons before combining (no `&&`
            // short-circuit): short-circuiting skips the password compare when
            // the username is wrong, so the response time would leak whether
            // the username was valid.  Mirrors `ssh::auth_password`.
            let user_ok = constant_time_eq(username.as_bytes(), cfg.username.as_bytes());
            let pass_ok = constant_time_eq(password.as_bytes(), cfg.password.as_bytes());
            if user_ok && pass_ok {
                if let Some(ip) = self.peer_addr {
                    clear_lockout(&self.lockouts, ip);
                }
                return Ok(true);
            }

            if let Some(ip) = self.peer_addr {
                let count = record_auth_failure(&self.lockouts, ip);
                if count >= MAX_AUTH_ATTEMPTS {
                    glog!("Telnet: {} locked out after {} failures", ip, count);
                    self.send_line(&format!(
                        "  {}",
                        self.red("Too many failed attempts.")
                    ))
                    .await?;
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    return Ok(false);
                }
            }

            let remaining = MAX_AUTH_ATTEMPTS - attempt;
            if remaining > 0 {
                self.send_line(&format!(
                    "  {} ({} {} remaining)",
                    self.red("Login incorrect."),
                    remaining,
                    if remaining == 1 {
                        "attempt"
                    } else {
                        "attempts"
                    },
                ))
                .await?;
                self.send_line("").await?;
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            } else {
                self.send_line(&format!(
                    "  {}",
                    self.red("Too many failed attempts.")
                ))
                .await?;
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }
        Ok(false)
    }

    // ─── Main session loop ──────────────────────────────────

    pub(crate) async fn run(&mut self) -> Result<(), std::io::Error> {
        let cfg = config::get_config();

        if !self.is_ssh {
            self.detect_terminal_type().await?;

            // Auto-set the IAC-escaping default based on
            // whether the client actually speaks the telnet protocol
            // (RFC 854/856).  detect_terminal_type() has already sent
            // our opening WILL/DO batch and drained the reply window,
            // so session_read_byte has flipped telnet_negotiated on
            // iff the peer answered with any option-negotiation or
            // subnegotiation bytes.  Real telnet clients (PuTTY, Tera
            // Term, C-Kermit, SecureCRT) always negotiate and need
            // 0xFF escaped; raw TCP clients (netcat, IMP8, CCGMS,
            // StrikeTerm, AltairDuino firmware) stay silent and get a
            // transparent byte stream.  Serial sessions skip the
            // negotiation entirely (no IAC), so telnet_negotiated
            // stays false and xmodem_iac is left off — matching the
            // raw byte stream a serial modem caller expects.  The I
            // key on the File Transfer menu still lets the user
            // override per-session.
            self.xmodem_iac = self.telnet_negotiated;

            // Serial sessions don't authenticate — they arrived via
            // ATDT on a physical port, which is its own trust boundary.
            if !self.is_serial
                && cfg.security_enabled
                && !self.authenticate().await?
            {
                return Ok(());
            }
        }

        // The main menu render does its own clear + banner; emitting a
        // separate welcome banner here would just flash on screen before
        // being wiped, which is especially painful at 1200 baud on a C64.
        match self.run_menu_loop().await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                let _ = self
                    .send_line("\r\n\r\nDisconnected: idle timeout.")
                    .await;
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Inner menu loop, separated so that idle timeout errors from any
    /// sub-menu propagate up and are handled uniformly in `run()`.
    async fn run_menu_loop(&mut self) -> Result<(), std::io::Error> {
        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                self.send_line("\r\nServer shutting down. Goodbye.")
                    .await?;
                break;
            }

            match self.current_menu {
                Menu::Main => self.render_main_menu().await?,
                Menu::FileTransfer => self.render_file_transfer().await?,
                Menu::Browser => self.render_web_browser().await?,
            }

            let prompt = self.prompt_str();
            self.send(&prompt).await?;
            self.flush().await?;

            let input = self.get_menu_input(true).await?;

            let input = match input {
                Some(s) if !s.is_empty() => s,
                _ => {
                    // ESC pressed — go to main menu or stay
                    if self.current_menu == Menu::Browser {
                        self.web_reset();
                    }
                    self.current_menu = Menu::Main;
                    continue;
                }
            };

            match self.current_menu.clone() {
                Menu::Main => {
                    if !self.handle_main_command(&input).await? {
                        break;
                    }
                }
                Menu::FileTransfer => {
                    self.handle_file_transfer_command(&input).await?;
                }
                Menu::Browser => {
                    self.handle_web_browser_command(&input).await?;
                }
            }
        }

        Ok(())
    }

    // ─── Main menu ──────────────────────────────────────────

    async fn render_main_menu(&mut self) -> Result<(), std::io::Error> {
        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;
        self.send_line(&format!("  {}", self.yellow("ETHERNET GATEWAY")))
            .await?;
        self.send_line(&sep).await?;
        self.send_line("").await?;

        // Slave-mode notice (§9 #13).  Shown only on a slave's own inbound
        // menu (never on the master or on a relay session, whose config is
        // the master's).  The slave still serves its own menu, but its
        // serial ports relay to the master, so point the operator there.
        // Costs 3 rows in slave mode only; the main menu is 16/22 rows so
        // a slave lands at ~19, still inside the PETSCII budget.
        {
            let cfg = config::get_config();
            if cfg.gateway_role == "slave" {
                self.send_line(&format!(
                    "  {}",
                    self.amber("SLAVE mode: ports relay to master.")
                ))
                .await?;
                let max_host = if self.terminal_type == TerminalType::Petscii {
                    28 // 40 - "  Master: " - margin
                } else {
                    66
                };
                let host = if cfg.slave_master_host.is_empty() {
                    "(not configured)".to_string()
                } else {
                    truncate_to_width(&cfg.slave_master_host, max_host)
                };
                self.send_line(&format!("  Master: {}", self.amber(&host)))
                    .await?;
                self.send_line("").await?;
            }
        }

        self.send_line(&format!(
            "  {}  AI Chat",
            self.cyan("A")
        ))
        .await?;
        self.send_line(&format!(
            "  {}  Simple Browser",
            self.cyan("B")
        ))
        .await?;
        self.send_line(&format!(
            "  {}  Configuration",
            self.cyan("C")
        ))
        .await?;
        self.send_line(&format!(
            "  {}  File Transfer",
            self.cyan("F")
        ))
        .await?;
        // Always shown.  Eligibility — and the own-port loopback reject
        // for a serial-arrived session — is enforced by the picker, the
        // single source of truth, which explains *why* a port is
        // unavailable rather than silently hiding the entry.  A
        // serial-arrived user can still legitimately bridge to a
        // *different* port (e.g. Port A's device to Port B's), so the
        // item must not be hidden for them.  Keeping it always-present
        // also avoids a menu that flickers as console targets come and
        // go (relevant once remote ports register at runtime).
        self.send_line(&format!(
            "  {}  Serial Gateway",
            self.cyan("G")
        ))
        .await?;
        self.send_line(&format!(
            "  {}  Troubleshooting",
            self.cyan("R")
        ))
        .await?;
        self.send_line(&format!(
            "  {}  SSH Gateway",
            self.cyan("S")
        ))
        .await?;
        self.send_line(&format!(
            "  {}  Telnet Gateway",
            self.cyan("T")
        ))
        .await?;
        self.send_line(&format!(
            "  {}  Weather",
            self.cyan("W")
        ))
        .await?;
        self.send_line(&format!("  {}  Exit", self.cyan("X")))
            .await?;
        self.send_line("").await?;
        self.send_line(&format!("  {}", self.action_prompt("H", "Help")))
            .await?;
        Ok(())
    }

    async fn handle_main_command(&mut self, input: &str) -> Result<bool, std::io::Error> {
        match input {
            "h" => {
                self.show_help_page("HELP", Self::main_help_lines()).await?;
            }
            "r" => {
                self.troubleshooting().await?;
            }
            "w" => {
                self.weather().await?;
            }
            "a" => {
                let cfg = config::get_config();
                if cfg.groq_api_key.is_empty() {
                    self.show_error_lines(&[
                        "No API key configured.",
                        "",
                        "To enable AI Chat:",
                        "1. Visit https://console.groq.com",
                        "2. Create a free account",
                        "3. Generate an API key",
                        "4. Configuration > Other Settings",
                        "   and set the AI API key",
                    ]).await?;
                } else {
                    self.ai_chat(&cfg.groq_api_key).await?;
                }
            }
            "b" => {
                self.current_menu = Menu::Browser;
            }
            "c" => {
                self.configuration().await?;
            }
            "f" => {
                self.current_menu = Menu::FileTransfer;
            }
            "g" => {
                self.gateway_serial().await?;
            }
            "s" => {
                self.gateway_ssh().await?;
            }
            "t" => {
                self.gateway_telnet().await?;
            }
            "x" => {
                self.send_farewell().await?;
                return Ok(false);
            }
            _ => {
                self.show_error("Press A-C, F, G, R, S, T, W, X, or H.").await?;
            }
        }
        Ok(true)
    }

    /// Print John 3:16 (KJV) on a fresh page when the user quits from
    /// the main menu, then block long enough for every byte to clock
    /// out on even a 1200 baud link before the caller drops the
    /// connection.  A 1200 baud 8N1 link carries 120 bytes/sec; we
    /// tally the bytes we emit and sleep `bytes / 120 s + 1 s` so the
    /// closing `TCP FIN` / SSH EOF doesn't truncate the final line on
    /// slow retro terminals.
    async fn send_farewell(&mut self) -> Result<(), std::io::Error> {
        self.clear_screen().await?;

        // Wrap width leaves a two-char indent on both layouts.  36/76
        // rather than 38/78 keeps room for color-code padding without
        // risking an overflow wrap on narrow PETSCII screens.
        let wrap_width = if self.terminal_type == TerminalType::Petscii {
            36
        } else {
            76
        };
        let verse = "For God so loved the world, that he gave his only \
                     begotten Son, that whosoever believeth in him \
                     should not perish, but have everlasting life.";

        // `byte_count` is a running tally of everything we send after
        // the clear-screen, so the transmit-delay calculation reflects
        // what actually went down the wire.  The clear-screen prefix
        // itself is a handful of bytes (ANSI ESC[2J ESC[H, PETSCII 0x93,
        // or blank for ASCII); 16 is a safe ceiling.
        let mut byte_count: usize = 16;

        self.send_line("").await?;
        byte_count += 2;

        let header = format!("  {}", self.yellow("John 3:16 (KJV)"));
        byte_count += header.len() + 2;
        self.send_line(&header).await?;

        self.send_line("").await?;
        byte_count += 2;

        for line in crate::aichat::wrap_line(verse, wrap_width) {
            let out = format!("  {}", line);
            byte_count += out.len() + 2;
            self.send_line(&out).await?;
        }

        self.send_line("").await?;
        byte_count += 2;
        self.flush().await?;

        // transmit_ms = bytes / 120 s, rounded up.  Adding 1 s of
        // quiet before disconnect lets the final stop-bit settle
        // before we close the socket.
        let transmit_ms = (byte_count as u64).saturating_mul(1000).div_ceil(120);
        tokio::time::sleep(std::time::Duration::from_millis(
            transmit_ms.saturating_add(1000),
        ))
        .await;
        Ok(())
    }

    // ─── File Transfer menu ──────────────────────────────────

    async fn render_file_transfer(&mut self) -> Result<(), std::io::Error> {
        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;
        self.send_line(&format!("  {}", self.yellow("FILE TRANSFER")))
            .await?;
        self.send_line(&sep).await?;
        self.send_line("").await?;
        let max_dir = if self.terminal_type == TerminalType::Petscii {
            30
        } else {
            60
        };
        let dir_str = truncate_to_width(&self.transfer_dir_display(), max_dir);
        self.send_line(&format!("  Dir: {}", self.amber(&dir_str)))
            .await?;
        self.send_line("").await?;
        self.send_line(&format!(
            "  {}  Upload a file",
            self.cyan("U")
        ))
        .await?;
        self.send_line(&format!(
            "  {}  Download a file",
            self.cyan("D")
        ))
        .await?;
        self.send_line(&format!(
            "  {}  Delete a file",
            self.cyan("X")
        ))
        .await?;
        self.send_line(&format!(
            "  {}  Change directory",
            self.cyan("C")
        ))
        .await?;
        self.send_line(&format!(
            "  {}  Make directory",
            self.cyan("M")
        ))
        .await?;
        self.send_line(&format!(
            "  {}  Kermit server mode",
            self.cyan("K")
        ))
        .await?;
        let iac_status = if self.xmodem_iac {
            self.green("ON")
        } else {
            self.red("OFF")
        };
        self.send_line(&format!(
            "  {}  IAC escaping [{}]",
            self.cyan("I"),
            iac_status
        ))
        .await?;
        self.send_line("").await?;
        let footer = self.nav_footer();
        self.send_line(&footer).await?;
        Ok(())
    }

    async fn handle_file_transfer_command(
        &mut self,
        input: &str,
    ) -> Result<bool, std::io::Error> {
        match input {
            "u" => {
                if let Err(e) = self.file_transfer_upload().await {
                    // ConnectionAborted means the session should end: either a
                    // deliberate Punter hangup-on-failure (`punter_hangup`) or
                    // the client already dropped (`wait_for_key`/reads surface
                    // EOF as ConnectionAborted).  Propagate so `run()` tears
                    // down and the writer is shut (carrier drop) instead of
                    // writing a doomed "Press any key" to a dead socket.
                    if e.kind() == std::io::ErrorKind::ConnectionAborted {
                        return Err(e);
                    }
                    self.show_error(&format!("Transfer error: {}", e))
                        .await?;
                }
            }
            "d" => {
                if let Err(e) = self.file_transfer_download().await {
                    if e.kind() == std::io::ErrorKind::ConnectionAborted {
                        return Err(e);
                    }
                    self.show_error(&format!("Transfer error: {}", e))
                        .await?;
                }
            }
            "x" => {
                if let Err(e) = self.file_transfer_delete().await {
                    self.show_error(&format!("Error: {}", e)).await?;
                }
            }
            "c" => {
                self.file_transfer_chdir().await?;
            }
            "m" => {
                self.file_transfer_mkdir().await?;
            }
            "k" => {
                match self.file_transfer_kermit_server().await {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                        // Kermit idle-timeout: propagate up so the
                        // session ends and the peer's TCP socket gets
                        // an immediate EOF on top of the E-packet we
                        // just sent.  Without this the peer's next
                        // `remote ...` lands on the file-transfer
                        // menu and surfaces as "too many retries".
                        return Err(e);
                    }
                    Err(e) => {
                        self.show_error(&format!("Server error: {}", e)).await?;
                    }
                }
            }
            "i" => {
                self.xmodem_iac = !self.xmodem_iac;
            }
            "q" => {
                self.current_menu = Menu::Main;
            }
            "h" => {
                self.show_help_page("FILE TRANSFER HELP", Self::file_transfer_menu_help_lines())
                    .await?;
            }
            "r" => {} // Refresh — just re-render
            _ => {
                self.show_error("Press U, D, X, C, M, K, I, R, Q, or H.")
                    .await?;
            }
        }
        Ok(true)
    }

    fn transfer_dir_display(&self) -> String {
        let cfg = config::get_config();
        if self.transfer_subdir.is_empty() {
            format!("{}/", cfg.transfer_dir)
        } else {
            format!("{}/{}/", cfg.transfer_dir, self.transfer_subdir)
        }
    }

    fn transfer_path(&self) -> std::path::PathBuf {
        let cfg = config::get_config();
        let mut p = std::path::PathBuf::from(&cfg.transfer_dir);
        if !self.transfer_subdir.is_empty() {
            p.push(&self.transfer_subdir);
        }
        p
    }

    /// Verify that the current transfer_subdir resolves to a path inside the
    /// transfer base directory. Resets to root if it escapes (e.g. via symlink).
    fn verify_transfer_path(&mut self) -> bool {
        let cfg = config::get_config();
        let base = match std::fs::canonicalize(&cfg.transfer_dir) {
            Ok(p) => p,
            Err(_) => return false,
        };
        let full = match std::fs::canonicalize(self.transfer_path()) {
            Ok(p) => p,
            Err(_) => {
                self.transfer_subdir.clear();
                return false;
            }
        };
        if full.starts_with(&base) {
            true
        } else {
            self.transfer_subdir.clear();
            false
        }
    }

    async fn ensure_transfer_dir(&mut self) -> Result<(), std::io::Error> {
        tokio::fs::create_dir_all(self.transfer_path()).await
    }

    /// Atomic write of a freshly received file to the transfer dir,
    /// shared by every batch-receive save site (ZMODEM autostart,
    /// ZMODEM/Kermit menu-initiated upload's per-batch-file path,
    /// Kermit server-mode dispatch).  `create_new` closes the TOCTOU
    /// window between an `exists()` check and the actual write that a
    /// plain `std::fs::write` would leave open; `tokio::fs` keeps the
    /// 8 MB cap from blocking the executor.
    ///
    /// Returns `SaveError::AlreadyExists` if a file with the same name
    /// is already present (caller decides whether that's "skip" or a
    /// fatal upload error), or `SaveError::WriteFailed` for any other
    /// I/O failure.
    async fn save_received_file(
        path: &std::path::Path,
        data: &[u8],
        meta: Option<&crate::xmodem::YmodemReceiveMeta>,
    ) -> Result<(), SaveError> {
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .await
        {
            Ok(mut file) => {
                use tokio::io::AsyncWriteExt;
                if file.write_all(data).await.is_err() {
                    return Err(SaveError::WriteFailed);
                }
                if file.flush().await.is_err() {
                    return Err(SaveError::WriteFailed);
                }
                drop(file);
                Self::apply_ymodem_meta(path, meta);
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(SaveError::AlreadyExists)
            }
            Err(_) => Err(SaveError::WriteFailed),
        }
    }

    /// Sync sibling of `save_received_file` for callers that can't
    /// `.await` (e.g. the Kermit server's on-file callback, which runs
    /// inside a non-async closure).  Same `SaveError` discrimination
    /// as the async sibling — only the I/O backend differs.  At the
    /// file sizes we deal with (≤8 MB) the blocking write is sub-
    /// millisecond on SSD and a few ms on spinning disk; briefly
    /// stalling the runtime is preferable to plumbing async closures
    /// through `kermit_server`'s generic boundary.
    ///
    /// `replace_existing=true` is the resume case: the caller has
    /// already merged the on-disk partial bytes into `data`, so we
    /// must atomically replace whatever's at `path` with the merged
    /// full file.  Done via tmp-file + rename so a process death
    /// mid-write leaves the original partial intact rather than
    /// corrupting both versions.  `false` keeps the create-new
    /// "refuse to clobber" semantics that every other save site
    /// uses.
    pub(crate) fn save_received_file_sync(
        path: &std::path::Path,
        data: &[u8],
        meta: Option<&crate::xmodem::YmodemReceiveMeta>,
        replace_existing: bool,
    ) -> Result<(), SaveError> {
        use std::io::Write;
        if replace_existing {
            // Resume: write to <name>.kermit-resume.tmp, fsync,
            // rename over the partial.  POSIX rename is atomic
            // within a filesystem; on failure we leave .tmp behind
            // but the original partial is untouched.
            let mut tmp_path = path.to_path_buf();
            let mut tmp_name = tmp_path
                .file_name()
                .map(|n| n.to_os_string())
                .unwrap_or_default();
            tmp_name.push(".kermit-resume.tmp");
            tmp_path.set_file_name(tmp_name);
            let mut file = match std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp_path)
            {
                Ok(f) => f,
                Err(_) => return Err(SaveError::WriteFailed),
            };
            if file.write_all(data).is_err() || file.flush().is_err() {
                drop(file);
                let _ = std::fs::remove_file(&tmp_path);
                return Err(SaveError::WriteFailed);
            }
            drop(file);
            if std::fs::rename(&tmp_path, path).is_err() {
                let _ = std::fs::remove_file(&tmp_path);
                return Err(SaveError::WriteFailed);
            }
            Self::apply_ymodem_meta(path, meta);
            return Ok(());
        }
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(mut file) => {
                if file.write_all(data).is_err() {
                    return Err(SaveError::WriteFailed);
                }
                if file.flush().is_err() {
                    return Err(SaveError::WriteFailed);
                }
                drop(file);
                Self::apply_ymodem_meta(path, meta);
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(SaveError::AlreadyExists)
            }
            Err(_) => Err(SaveError::WriteFailed),
        }
    }

    /// Apply YMODEM block-0 metadata to a freshly saved file.  Both
    /// modtime and mode are best-effort — failures are ignored because
    /// they don't affect data integrity.  Mode is masked to `0o777` so
    /// a misbehaving sender can't set setuid/setgid/sticky bits on our
    /// saved files; mode application is a no-op on non-Unix platforms.
    /// Sync std::fs calls are deliberate — these are microsecond-level
    /// operations that run once per saved file, so the cost of routing
    /// through `spawn_blocking` would exceed the operations themselves.
    fn apply_ymodem_meta(
        path: &std::path::Path,
        meta: Option<&crate::xmodem::YmodemReceiveMeta>,
    ) {
        let Some(m) = meta else { return };
        if let Some(secs) = m.modtime
            && let Ok(file) = std::fs::OpenOptions::new().write(true).open(path)
        {
            let when = std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs);
            let _ = file.set_modified(when);
        }
        #[cfg(unix)]
        if let Some(mode) = m.mode {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(mode & 0o777);
            let _ = std::fs::set_permissions(path, perms);
        }
    }

    pub(crate) fn validate_filename(name: &str) -> Result<(), &'static str> {
        if name.is_empty() {
            return Err("Filename cannot be empty");
        }
        if name.len() > Self::MAX_FILENAME_LEN {
            return Err("Filename too long (max 64 chars)");
        }
        if name.starts_with('.') {
            return Err("Filename cannot start with a dot");
        }
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
        {
            return Err("Only letters, numbers, dots, hyphens, underscores");
        }
        if !name.chars().any(|c| c.is_ascii_alphanumeric()) {
            return Err("Filename must contain a letter or number");
        }
        if name.contains("..") {
            return Err("Invalid filename");
        }
        Ok(())
    }

    async fn list_transfer_entries_in(
        path: &std::path::Path,
    ) -> Result<Vec<(String, u64, bool)>, std::io::Error> {
        let mut dir = match tokio::fs::read_dir(&path).await {
            Ok(d) => d,
            Err(_) => return Ok(Vec::new()),
        };
        let mut entries: Vec<(String, u64, bool)> = Vec::new();
        while let Ok(Some(entry)) = dir.next_entry().await {
            let metadata = match entry.metadata().await {
                Ok(m) => m,
                Err(_) => continue,
            };
            if let Some(name) = entry.file_name().to_str() {
                if metadata.is_dir() {
                    entries.push((name.to_string(), 0, true));
                } else if metadata.is_file() {
                    entries.push((name.to_string(), metadata.len(), false));
                }
            }
        }
        entries.sort_by(|a, b| {
            b.2.cmp(&a.2)
                .then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase()))
        });
        Ok(entries)
    }

    fn format_file_size(size: u64) -> String {
        if size < 1024 {
            format!("{} B", size)
        } else if size < 1024 * 1024 {
            format!("{:.1} KB", size as f64 / 1024.0)
        } else {
            format!("{:.1} MB", size as f64 / (1024.0 * 1024.0))
        }
    }

    /// Returns true if disk usage exceeds 90%.
    fn is_disk_full() -> bool {
        #[cfg(unix)]
        {
            use std::ffi::CString;
            use std::mem::MaybeUninit;
            let cfg = config::get_config();
            let dir = if std::path::Path::new(&cfg.transfer_dir).exists() {
                cfg.transfer_dir.clone()
            } else {
                ".".to_string()
            };
            // "." never contains a nul byte, so the fallback CString is
            // always constructable.
            let path = CString::new(dir.as_str())
                .unwrap_or_else(|_| c".".to_owned());
            let mut stat = MaybeUninit::<libc::statvfs>::uninit();
            let rc = unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) };
            if rc != 0 {
                return true;
            }
            let stat = unsafe { stat.assume_init() };
            // `f_frsize` / `f_blocks` / `f_bavail` are u64 on Linux but
            // u32 on macOS/BSD — cast all three to u64 explicitly so
            // the multiplication is portable across the Unix targets
            // our release workflow builds (Linux x86_64 + macOS aarch64).
            // The casts are no-ops on Linux; clippy flags them because
            // it only sees the host target.
            #[allow(clippy::unnecessary_cast)]
            let frsize = stat.f_frsize as u64;
            #[allow(clippy::unnecessary_cast)]
            let total = stat.f_blocks as u64 * frsize;
            #[allow(clippy::unnecessary_cast)]
            let avail = stat.f_bavail as u64 * frsize;
            if total == 0 || avail >= total {
                return total == 0;
            }
            let used_pct = 100 - (avail * 100 / total);
            used_pct > 90
        }
        #[cfg(windows)]
        {
            use std::ffi::OsStr;
            use std::os::windows::ffi::OsStrExt;

            #[repr(C)]
            #[allow(non_snake_case)]
            struct ULARGE_INTEGER {
                QuadPart: u64,
            }

            unsafe extern "system" {
                fn GetDiskFreeSpaceExW(
                    lpDirectoryName: *const u16,
                    lpFreeBytesAvailableToCaller: *mut ULARGE_INTEGER,
                    lpTotalNumberOfBytes: *mut ULARGE_INTEGER,
                    lpTotalNumberOfFreeBytes: *mut ULARGE_INTEGER,
                ) -> i32;
            }

            let cfg = config::get_config();
            let dir = if std::path::Path::new(&cfg.transfer_dir).exists() {
                cfg.transfer_dir.clone()
            } else {
                ".".to_string()
            };
            let wide: Vec<u16> = OsStr::new(&dir).encode_wide().chain(std::iter::once(0)).collect();
            let mut avail = ULARGE_INTEGER { QuadPart: 0 };
            let mut total = ULARGE_INTEGER { QuadPart: 0 };
            let mut _free = ULARGE_INTEGER { QuadPart: 0 };
            let rc = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut avail, &mut total, &mut _free) };
            if rc == 0 || total.QuadPart == 0 {
                return total.QuadPart == 0;
            }
            let used_pct = 100 - (avail.QuadPart * 100 / total.QuadPart);
            used_pct > 90
        }
        #[cfg(not(any(unix, windows)))]
        {
            false
        }
    }

    // ─── UPLOAD ─────────────────────────────────────────────

    /// Prompt the user to pick the upload protocol on its own screen.
    /// Returns `None` if the user pressed ESC / PETSCII `<-` to cancel
    /// back to the file-transfer menu.  Parallel to
    /// [`Self::prompt_download_protocol`] — same screen layout,
    /// navigation keys, and petscii/ANSI handling.
    async fn prompt_upload_protocol(
        &mut self,
    ) -> Result<Option<UploadProtocol>, std::io::Error> {
        let is_petscii = self.terminal_type == TerminalType::Petscii;
        let esc_label = if is_petscii { "<-" } else { "ESC" };

        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;
        self.send_line(&format!(
            "  {}",
            self.yellow("SELECT UPLOAD PROTOCOL")
        ))
        .await?;
        self.send_line(&sep).await?;
        self.send_line("").await?;
        // Keep each line <= 39 columns so it doesn't wrap on a 40-column
        // PETSCII (C64) screen.
        self.send_line(&format!(
            "  {}  XMODEM/YMODEM  128/1K, auto",
            self.cyan("X")
        ))
        .await?;
        self.send_line(&format!(
            "  {}  ZMODEM         1K, autostart",
            self.cyan("Z")
        ))
        .await?;
        self.send_line(&format!(
            "  {}  KERMIT         any flavor, auto",
            self.cyan("K")
        ))
        .await?;
        self.send_line(&format!(
            "  {}  PUNTER         C1 CCGMS/Novaterm",
            self.cyan("P")
        ))
        .await?;
        self.send_line("").await?;
        self.send(&format!(
            "  Pick one, or {} to cancel: ",
            self.cyan(esc_label)
        ))
        .await?;
        self.flush().await?;

        loop {
            let b = match self.read_byte_filtered().await? {
                Some(b) => b,
                None => return Ok(None),
            };
            if is_esc_key(b, is_petscii) {
                self.send_line("").await?;
                return Ok(None);
            }
            let ch = if is_petscii {
                (petscii_to_ascii_byte(b) as char).to_ascii_lowercase()
            } else {
                (b as char).to_ascii_lowercase()
            };
            // Accept 'Y' as a synonym for 'X' so a user thinking
            // "YMODEM" doesn't have to hunt for the right key — the
            // XMODEM/YMODEM receive path handles both.
            let chosen = match ch {
                'x' | 'y' => Some(UploadProtocol::XmodemYmodem),
                'z' => Some(UploadProtocol::Zmodem),
                'k' => Some(UploadProtocol::Kermit),
                'p' => Some(UploadProtocol::Punter),
                _ => None,
            };
            if let Some(p) = chosen {
                self.send_raw(&[b]).await?;
                self.send_line("").await?;
                self.flush().await?;
                return Ok(Some(p));
            }
            // Invalid key — stay at the prompt.
        }
    }

    async fn file_transfer_upload(&mut self) -> Result<(), std::io::Error> {
        self.ensure_transfer_dir().await?;

        if Self::is_disk_full() {
            self.show_error("Disk space is low. Uploads disabled.")
                .await?;
            return Ok(());
        }

        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;
        self.send_line(&format!("  {}", self.yellow("UPLOAD FILE")))
            .await?;
        self.send_line(&sep).await?;
        self.send_line("").await?;

        let p = format!("  {} ", self.cyan("Filename:"));
        self.send(&p).await?;
        self.flush().await?;

        let filename = match self.get_line_input().await? {
            Some(s) if !s.is_empty() => s,
            _ => return Ok(()),
        };

        if let Err(msg) = Self::validate_filename(&filename) {
            self.show_error(msg).await?;
            return Ok(());
        }

        let filepath = self.transfer_path().join(&filename);

        // Detect duplicates up-front so the user doesn't sit through a
        // whole transfer only to have the save-step fail.  Prompt to
        // overwrite; if declined, cancel cleanly.
        let overwrite = if tokio::fs::try_exists(&filepath).await.unwrap_or(false) {
            self.send_line("").await?;
            self.send_line(&format!(
                "  {}",
                self.yellow(&format!("File '{}' already exists.", filename))
            ))
            .await?;
            self.send(&format!(
                "  {} ",
                self.cyan("Overwrite? (Y/N):")
            ))
            .await?;
            self.flush().await?;
            self.drain_input().await;
            let answer = match self.read_byte_filtered().await? {
                Some(b) => {
                    if self.terminal_type == TerminalType::Petscii {
                        petscii_to_ascii_byte(b)
                    } else {
                        b
                    }
                }
                None => return Ok(()),
            };
            self.send_line("").await?;
            if answer != b'y' && answer != b'Y' {
                return Ok(());
            }
            true
        } else {
            false
        };

        // Ask the user which protocol their sender will use.  Putting
        // this on its own screen after the filename + overwrite prompts
        // mirrors the download flow (file → protocol → transfer) and
        // gives the user as long as they need to browse menus on their
        // terminal before committing to the transfer window.  ESC /
        // PETSCII `<-` at the protocol prompt cancels cleanly.
        let protocol = match self.prompt_upload_protocol().await? {
            Some(p) => p,
            None => return Ok(()),
        };

        self.send_line("").await?;
        self.send_line(&format!(
            "  Ready to receive: {}",
            self.amber(&filename)
        ))
        .await?;
        self.send_line(&format!(
            "  Max file size: {} MB",
            Self::MAX_FILE_SIZE / (1024 * 1024)
        ))
        .await?;
        self.send_line("").await?;
        self.send_line(&format!(
            "  {}",
            self.green(match protocol {
                UploadProtocol::XmodemYmodem =>
                    "Start XMODEM/YMODEM send from your terminal now.",
                UploadProtocol::Zmodem =>
                    "Start ZMODEM send from your terminal now.",
                UploadProtocol::Kermit =>
                    "Start KERMIT send from your terminal now.",
                UploadProtocol::Punter =>
                    "Start PUNTER send from your terminal now.",
            })
        ))
        .await?;
        // Make it explicit that the action happens on the user's side.
        // For ExtraPutty it's File Transfer → Zmodem → Send; other
        // terminals have similar menu items.  Users who know the drill
        // can ignore this — it's here for the first-timer path.
        if matches!(protocol, UploadProtocol::Zmodem) {
            self.send_line(
                "  (ExtraPutty: File Transfer > Zmodem > Send. Other clients vary.)",
            )
            .await?;
        }
        let neg_timeout = {
            let cfg = config::get_config();
            match protocol {
                UploadProtocol::Zmodem => cfg.zmodem_negotiation_timeout,
                UploadProtocol::Kermit => cfg.kermit_negotiation_timeout,
                UploadProtocol::Punter => cfg.punter_negotiation_timeout,
                UploadProtocol::XmodemYmodem => cfg.xmodem_negotiation_timeout,
            }
        };
        self.send_line(&format!("  Start transfer within {} seconds.", neg_timeout))
            .await?;
        let esc_label = match self.terminal_type {
            TerminalType::Petscii => "<-",
            _ => "ESC",
        };
        self.send_line(&format!("  {} to cancel", self.cyan(esc_label)))
            .await?;
        self.send_line("").await?;
        self.flush().await?;

        if config::get_config().verbose {
            glog!("Upload: IAC escaping={} protocol={:?}", self.xmodem_iac, protocol);
        }
        // See the download path: Punter's silent cancel can leave stale bytes
        // in the pipe, so drain with a longer quiet gap before receiving.
        if matches!(protocol, UploadProtocol::Punter) {
            self.drain_input_until_quiet(250, Some(2000)).await;
        } else {
            self.drain_input().await;
        }

        let verbose = config::get_config().verbose;
        let start = std::time::Instant::now();
        let mut writer_guard = self.writer.lock().await;
        // Normalize both receive paths to a Vec of (sender-proposed
        // filename, data).  XMODEM/YMODEM never carries a filename in
        // the protocol, so we mark it as None and the user-entered
        // name wins.  ZMODEM carries a filename per file; we keep it
        // so batches can save files 2..N under their sender names.
        // The third tuple slot carries optional YMODEM metadata
        // (modtime/mode/sno) parsed from block 0; ZMODEM doesn't surface
        // file attributes through this path so its entries are always
        // `None`.  The save-side applies modtime + mode after writing.
        type Received = Vec<(Option<String>, Vec<u8>, Option<crate::xmodem::YmodemReceiveMeta>)>;
        // Decide callback for the ZMODEM receiver.  The first file
        // (idx 0) is always accepted — the user typed a destination
        // filename in the upload prompt, so they want this one saved
        // regardless of what the sender called it.  Later files in a
        // batch use the sender's name, which we sanitize through the
        // same `validate_filename` rules as user input and reject with
        // ZSKIP if they fail or collide with an existing file.  The
        // path-existence check is a sync std::fs call — fast, no
        // runtime-blocking concern.
        let transfer_path = self.transfer_path();
        let decide = |idx: usize,
                      sender_name: &str,
                      _size: Option<u64>|
         -> bool {
            if idx == 0 {
                return true;
            }
            if Self::validate_filename(sender_name).is_err() {
                return false;
            }
            !transfer_path.join(sender_name).exists()
        };
        let is_petscii = self.terminal_type == TerminalType::Petscii;
        // Captured by the Kermit branch's mapping closure when the
        // peer's flavor is detected.  Surfaced in the post-transfer
        // summary so the user sees who they talked to.
        let mut kermit_flavor: Option<String> = None;
        let result: Result<Received, String> = match protocol {
            UploadProtocol::Zmodem => crate::zmodem::zmodem_receive(
                &mut self.reader,
                &mut *writer_guard,
                self.xmodem_iac,
                verbose,
                decide,
            )
            .await
            .map(|rxs| {
                rxs.into_iter()
                    .map(|rx| {
                        // ZFILE info per Forsberg §11 carries length / mtime
                        // / mode — feed them into apply_ymodem_meta so the
                        // saved file gets the sender's mtime + permissions
                        // (matching YMODEM and Kermit behavior).
                        let meta = (rx.modtime.is_some() || rx.mode.is_some())
                            .then_some(crate::xmodem::YmodemReceiveMeta {
                                size: None,
                                modtime: rx.modtime,
                                mode: rx.mode,
                            });
                        (Some(rx.filename), rx.data, meta)
                    })
                    .collect()
            }),
            UploadProtocol::XmodemYmodem => crate::xmodem::xmodem_receive_batch(
                &mut self.reader,
                &mut *writer_guard,
                self.xmodem_iac,
                self.terminal_type == TerminalType::Petscii,
                verbose,
            )
            .await
            // A YMODEM batch yields multiple files.  The first keeps the
            // user-entered name (matching plain XMODEM / ZMODEM / Kermit); files
            // 2..N take the sender's block-0 filename (the save path sanitizes
            // it against path traversal, as it does for ZMODEM/Kermit names).
            .map(|files| {
                files
                    .into_iter()
                    .enumerate()
                    .map(|(i, f)| {
                        let name = if i == 0 { None } else { f.filename };
                        (name, f.data, f.meta)
                    })
                    .collect()
            }),
            UploadProtocol::Kermit => crate::kermit::kermit_receive(
                &mut self.reader,
                &mut *writer_guard,
                self.xmodem_iac,
                is_petscii,
                verbose,
            )
            .await
            .map(|rxs| {
                // Capture flavor (per-session, identical across files
                // in a batch).
                kermit_flavor = rxs.first().map(|r| r.flavor.display());
                // Map KermitReceive list to (Option<filename>, data, None).
                // First file gets None for filename so user-entered name
                // wins (matches XMODEM/YMODEM behavior); subsequent files
                // in the batch use the sender's name like ZMODEM does.
                rxs.into_iter()
                    .enumerate()
                    .map(|(i, rx)| {
                        let name = if i == 0 { None } else { Some(rx.filename) };
                        let meta = crate::xmodem::YmodemReceiveMeta {
                            size: rx.declared_size,
                            modtime: rx.modtime,
                            mode: rx.mode,
                        };
                        (name, rx.data, Some(meta))
                    })
                    .collect()
            }),
            UploadProtocol::Punter => crate::punter::punter_receive(
                &mut self.reader,
                &mut *writer_guard,
                self.xmodem_iac,
                is_petscii,
                verbose,
            )
            .await
            // C1 carries no filename, so the user-entered name normally
            // wins (matching XMODEM/YMODEM).  Novaterm preserves the
            // declared PRG/SEQ/USR type via the CBM directory entry; on
            // Linux we don't have that, so we append the matching
            // extension when the user's filename has none — the same
            // suffix `PunterFileType::autodetect` will read on the way
            // back out.  Anything the user typed with an explicit
            // extension is honored verbatim, and `Unknown` skips the
            // suffix entirely.
            .map(|(data, file_type)| {
                let has_extension = filename
                    .find('.')
                    .map(|i| i > 0)
                    .unwrap_or(false);
                let chosen_name = match file_type.extension() {
                    Some(ext) if !has_extension => {
                        Some(format!("{}.{}", filename, ext))
                    }
                    _ => None,
                };
                vec![(chosen_name, data, None)]
            }),
        };
        drop(writer_guard);
        let elapsed = start.elapsed();

        let uploads = match result {
            Ok(v) => v,
            Err(e) => {
                self.post_transfer_settle().await;
                // Option 4: with no in-band abort, a Punter give-up otherwise
                // strands the C64.  Drop carrier instead of waiting on a
                // keypress the hung peer will never send — but only on a
                // genuine give-up, NOT a user-initiated cancel (ESC →
                // "Transfer cancelled"), which must return to the menu.
                if matches!(protocol, UploadProtocol::Punter)
                    && config::get_config().punter_hangup_on_failure
                    && !e.contains("cancelled")
                {
                    self.send_line(&format!(
                        "  {}",
                        self.red(&format!("Transfer failed: {}", e))
                    ))
                    .await?;
                    return self.punter_hangup().await;
                }
                self.show_error(&format!("Transfer failed: {}", e))
                    .await?;
                return Ok(());
            }
        };

        // Save each file.  The first file goes to the user-entered
        // path with the user-chosen overwrite behavior.  Any additional
        // files (ZMODEM batch mode per Forsberg §4) go to the sender's
        // own filename after the same `validate_filename` sanitation
        // we apply to user input — and if the name collides with an
        // existing file we skip rather than clobber.  Batch files
        // share the transfer-complete window with the first file; we
        // don't prompt per-file.
        let mut saved: Vec<(String, usize)> = Vec::new();
        let mut skipped: Vec<(String, &'static str)> = Vec::new();

        for (idx, (sender_name, data, ymeta)) in uploads.iter().enumerate() {
            if idx == 0 {
                // First file: user-entered filename, honor overwrite.
                // A codec may refine the name — Punter appends the
                // .prg/.seq extension matching the declared CBM type when
                // the user's filename had none (the same suffix
                // `PunterFileType::autodetect` reads on the way back out).
                // The user's overwrite choice for the base name carries to
                // the suffixed name; a late collision still surfaces via
                // create_new below.
                let (save_name, save_path) = match sender_name {
                    Some(n) if Self::validate_filename(n).is_ok() => {
                        (n.clone(), self.transfer_path().join(n))
                    }
                    _ => (filename.clone(), filepath.clone()),
                };
                let mut opts = tokio::fs::OpenOptions::new();
                opts.write(true);
                if overwrite {
                    opts.create(true).truncate(true);
                } else {
                    opts.create_new(true);
                }
                match opts.open(&save_path).await {
                    Ok(mut file) => {
                        if let Err(e) = file.write_all(data).await {
                            self.post_transfer_settle().await;
                            self.show_error(&format!("Failed to save: {}", e))
                                .await?;
                            return Ok(());
                        }
                        let _ = file.flush().await;
                        drop(file);
                        Self::apply_ymodem_meta(&save_path, ymeta.as_ref());
                        saved.push((save_name, data.len()));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                        self.post_transfer_settle().await;
                        self.show_error("File already exists.").await?;
                        return Ok(());
                    }
                    Err(e) => {
                        self.post_transfer_settle().await;
                        self.show_error(&format!("Failed to save: {}", e))
                            .await?;
                        return Ok(());
                    }
                }
            } else {
                // Batch file 2..N: save under sender's name.  ZMODEM, Kermit,
                // and a YMODEM batch (`sb file1 file2 …`) all produce these, so
                // `sender_name` is Some here.  Routes through the same atomic
                // save_received_file helper as the autostart and Kermit-server
                // batch paths so the create_new + tokio::fs guarantees stay
                // symmetric.
                let name = match sender_name {
                    Some(n) => n.clone(),
                    // A YMODEM batch file whose block-0 name wasn't valid UTF-8
                    // arrives nameless — save it under a generated name rather
                    // than silently dropping it (ZMODEM/Kermit always name theirs).
                    None => format!("ymodem_file_{}", idx + 1),
                };
                if Self::validate_filename(&name).is_err() {
                    // Sanitize the sender-supplied name before it reaches the
                    // terminal (a rejected name can carry ANSI escapes).
                    let safe = crate::aichat::sanitize_for_terminal(&name);
                    skipped.push((safe, "invalid filename"));
                    continue;
                }
                let batch_path = self.transfer_path().join(&name);
                match Self::save_received_file(&batch_path, data, ymeta.as_ref()).await {
                    Ok(()) => saved.push((name, data.len())),
                    Err(SaveError::AlreadyExists) => {
                        skipped.push((name, "already exists"));
                    }
                    Err(SaveError::WriteFailed) => {
                        skipped.push((name, "write failed"));
                    }
                }
            }
        }

        self.post_transfer_settle().await;

        // Transfer-complete summary.  Preserve the classic single-file
        // "N bytes, M blocks, T seconds" format when exactly one file
        // was transferred (by far the common case); expand to a
        // per-file list only when we actually saw a batch.
        self.send_line("").await?;
        if uploads.len() == 1 {
            let bytes = saved.first().map(|(_, n)| *n).unwrap_or(0);
            let blocks = bytes.div_ceil(crate::xmodem::XMODEM_BLOCK_SIZE);
            self.send_line(&format!(
                "  {}",
                self.green("Upload complete!")
            ))
            .await?;
            self.send_line(&format!(
                "  {} bytes, {} blocks, {:.1}s",
                bytes,
                blocks,
                elapsed.as_secs_f64()
            ))
            .await?;
        } else {
            self.send_line(&format!(
                "  {}",
                self.green(&format!(
                    "Upload complete: {} saved, {} skipped, {:.1}s",
                    saved.len(),
                    skipped.len(),
                    elapsed.as_secs_f64()
                ))
            ))
            .await?;
            for (name, bytes) in &saved {
                self.send_line(&format!(
                    "  {} {} ({} bytes)",
                    self.green("*"),
                    name,
                    bytes
                ))
                .await?;
            }
            for (name, reason) in &skipped {
                self.send_line(&format!(
                    "  {} {} ({})",
                    self.yellow("-"),
                    name,
                    reason
                ))
                .await?;
            }
        }
        // Surface detected Kermit flavor (auto-classified from the
        // peer's Send-Init / peer_id) so users see whom they talked to.
        if let Some(flavor) = &kermit_flavor {
            self.send_line(&format!("  {} {}", self.dim("Peer:"), flavor))
                .await?;
        }
        self.send_line("").await?;
        self.send("  Press any key to continue.").await?;
        self.flush().await?;
        self.wait_for_key().await?;
        Ok(())
    }

    // ─── DOWNLOAD ───────────────────────────────────────────

    async fn file_transfer_download(&mut self) -> Result<(), std::io::Error> {
        self.ensure_transfer_dir().await?;
        let mut page: usize = 0;

        loop {
            let files = Self::list_transfer_entries_in(&self.transfer_path())
                .await?
                .into_iter()
                .filter(|(_, _, is_dir)| !is_dir)
                .map(|(name, size, _)| (name, size))
                .collect::<Vec<_>>();

            if files.is_empty() {
                self.show_error("No files available.").await?;
                return Ok(());
            }

            let total_pages = files.len().div_ceil(Self::TRANSFER_PAGE_SIZE);
            if page >= total_pages {
                page = total_pages - 1;
            }
            let offset = page * Self::TRANSFER_PAGE_SIZE;
            let end = (offset + Self::TRANSFER_PAGE_SIZE).min(files.len());
            let page_files = &files[offset..end];

            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            self.send_line(&format!(
                "  {}",
                self.yellow("DOWNLOAD FILE")
            ))
            .await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;
            self.send_line(&format!(
                "   {} {:<22} {}",
                self.cyan("#."),
                "Filename",
                "Size"
            ))
            .await?;
            self.send_line(&format!(
                "  {}",
                self.yellow(&"-".repeat(36))
            ))
            .await?;

            for (i, (name, size)) in page_files.iter().enumerate() {
                let num = i + 1;
                let display_name = if name.chars().count() > 22 {
                    let truncated: String = name.chars().take(19).collect();
                    format!("{}...", truncated)
                } else {
                    name.clone()
                };
                let size_display = Self::format_file_size(*size);
                self.send_line(&format!(
                    "  {:>2}. {:<22} {}",
                    num, display_name, size_display
                ))
                .await?;
            }

            self.send_line("").await?;
            self.send_line(&format!(
                "  Page {} of {}",
                page + 1,
                total_pages
            ))
            .await?;
            self.send_line("").await?;

            let mut nav = Vec::new();
            if page > 0 {
                nav.push(self.action_prompt("P", "Prev"));
            }
            if page + 1 < total_pages {
                nav.push(self.action_prompt("N", "Next"));
            }
            nav.push(self.action_prompt("Q", "Back"));
            nav.push(self.action_prompt("H", "Help"));
            let esc_label = match self.terminal_type {
                TerminalType::Petscii => "<-",
                _ => "ESC",
            };
            nav.push(self.action_prompt(esc_label, "Main"));
            self.send_line(&format!("  {}", nav.join(" | ")))
                .await?;
            self.send_line("").await?;
            self.send(&format!("  {} ", self.cyan("Select #:")))
                .await?;
            self.flush().await?;

            let input = match self.get_line_input().await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(()),
            };

            match input.as_str() {
                "p" => {
                    page = page.saturating_sub(1);
                }
                "n" => {
                    if page + 1 < total_pages {
                        page += 1;
                    }
                }
                "q" => return Ok(()),
                "h" => {
                    self.show_help_page("DOWNLOAD HELP", Self::download_help_lines())
                        .await?;
                }
                other => {
                    if let Ok(num) = other.parse::<usize>() {
                        if num >= 1 && num <= page_files.len() {
                            let (ref filename, file_size) = page_files[num - 1];
                            self.initiate_download(filename, file_size).await?;
                        } else {
                            self.show_error("Invalid selection.").await?;
                        }
                    } else {
                        self.show_error("Enter a number, P, N, Q, or H.")
                            .await?;
                    }
                }
            }
        }
    }

    /// Prompt the user for which XMODEM-family protocol to use for this
    /// download.  Shows the file being downloaded (name + size) so the user
    /// can confirm they picked the right one before starting.  Returns `None`
    /// if the user presses ESC to cancel.
    async fn prompt_download_protocol(
        &mut self,
        filename: &str,
        file_size: u64,
    ) -> Result<Option<DownloadProtocol>, std::io::Error> {
        let is_petscii = self.terminal_type == TerminalType::Petscii;
        let esc_label = if is_petscii { "<-" } else { "ESC" };

        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;
        self.send_line(&format!("  {}", self.yellow("SELECT PROTOCOL")))
            .await?;
        self.send_line(&sep).await?;
        self.send_line("").await?;
        // Show what's being downloaded so the user can verify they picked the
        // right file before choosing a protocol.
        let max_name = if is_petscii { 31 } else { 60 };
        self.send_line(&format!(
            "  File: {}",
            self.amber(&truncate_to_width(filename, max_name))
        ))
        .await?;
        self.send_line(&format!("  Size: {} bytes", file_size))
            .await?;
        self.send_line("").await?;
        // Keep each line <= 39 columns so it doesn't wrap on a 40-column
        // PETSCII (C64) screen.
        self.send_line(&format!(
            "  {}  XMODEM     128-byte blocks",
            self.cyan("X")
        ))
        .await?;
        self.send_line(&format!(
            "  {}  XMODEM-1K  1024-byte blocks",
            self.cyan("1")
        ))
        .await?;
        self.send_line(&format!(
            "  {}  YMODEM     name+size hdr, 1K",
            self.cyan("Y")
        ))
        .await?;
        self.send_line(&format!(
            "  {}  ZMODEM     autostart, 1K",
            self.cyan("Z")
        ))
        .await?;
        self.send_line(&format!(
            "  {}  KERMIT     any flavor, auto",
            self.cyan("K")
        ))
        .await?;
        self.send_line(&format!(
            "  {}  PUNTER     C1 CCGMS/Novaterm",
            self.cyan("P")
        ))
        .await?;
        self.send_line("").await?;
        self.send(&format!(
            "  Pick one, or {} to cancel: ",
            self.cyan(esc_label)
        ))
        .await?;
        self.flush().await?;

        loop {
            let b = match self.read_byte_filtered().await? {
                Some(b) => b,
                None => return Ok(None),
            };
            if is_esc_key(b, is_petscii) {
                self.send_line("").await?;
                return Ok(None);
            }
            let ch = if is_petscii {
                (petscii_to_ascii_byte(b) as char).to_ascii_lowercase()
            } else {
                (b as char).to_ascii_lowercase()
            };
            let chosen = match ch {
                'x' => Some(DownloadProtocol::Xmodem),
                '1' => Some(DownloadProtocol::Xmodem1k),
                'y' => Some(DownloadProtocol::Ymodem),
                'z' => Some(DownloadProtocol::Zmodem),
                'k' => Some(DownloadProtocol::Kermit),
                'p' => Some(DownloadProtocol::Punter),
                _ => None,
            };
            if let Some(p) = chosen {
                self.send_raw(&[b]).await?;
                self.send_line("").await?;
                self.flush().await?;
                return Ok(Some(p));
            }
            // Invalid key — stay at the prompt.
        }
    }

    async fn initiate_download(
        &mut self,
        filename: &str,
        file_size: u64,
    ) -> Result<(), std::io::Error> {
        let blocks = (file_size as usize).div_ceil(crate::xmodem::XMODEM_BLOCK_SIZE);

        self.send_line("").await?;
        self.send_line(&format!(
            "  Sending: {}",
            self.amber(filename)
        ))
        .await?;
        self.send_line(&format!(
            "  {} bytes, {} blocks",
            file_size, blocks
        ))
        .await?;

        if file_size as usize > Self::MAX_FILE_SIZE {
            self.show_error("File too large.").await?;
            return Ok(());
        }

        let filepath = self.transfer_path().join(filename);
        let data = match tokio::fs::read(&filepath).await {
            Ok(d) => d,
            Err(e) => {
                self.show_error(&format!("Failed to read: {}", e))
                    .await?;
                return Ok(());
            }
        };
        // Best-effort fs metadata for the YMODEM block-0 modtime/mode
        // fields (Forsberg §6.1).  Both are informational — if metadata
        // lookup fails or the platform doesn't expose UNIX mode bits we
        // pass `None` and the sender emits octal `0` in that slot.
        let (file_modtime, file_mode) = match tokio::fs::metadata(&filepath).await {
            Ok(m) => {
                let modtime = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs());
                #[cfg(unix)]
                let mode = {
                    use std::os::unix::fs::MetadataExt;
                    Some(m.mode())
                };
                #[cfg(not(unix))]
                let mode: Option<u32> = None;
                (modtime, mode)
            }
            Err(_) => (None, None),
        };

        // Prompt the user to pick the transfer protocol for this download.
        // ESC at the prompt cancels the transfer.
        let protocol = match self.prompt_download_protocol(filename, file_size).await? {
            Some(p) => p,
            None => return Ok(()),
        };

        self.send_line("").await?;
        self.send_line(&format!(
            "  {}",
            self.green(match protocol {
                DownloadProtocol::Xmodem => "Start XMODEM receive now.",
                DownloadProtocol::Xmodem1k => "Start XMODEM-1K receive now.",
                DownloadProtocol::Ymodem => "Start YMODEM receive now.",
                DownloadProtocol::Zmodem => "Start ZMODEM receive now.",
                DownloadProtocol::Kermit => "Start KERMIT receive now.",
                DownloadProtocol::Punter => "Start PUNTER receive now.",
            })
        ))
        .await?;
        let neg_timeout = {
            let cfg = config::get_config();
            match protocol {
                DownloadProtocol::Zmodem => cfg.zmodem_negotiation_timeout,
                DownloadProtocol::Kermit => cfg.kermit_negotiation_timeout,
                DownloadProtocol::Punter => cfg.punter_negotiation_timeout,
                DownloadProtocol::Xmodem
                | DownloadProtocol::Xmodem1k
                | DownloadProtocol::Ymodem => cfg.xmodem_negotiation_timeout,
            }
        };
        self.send_line(&format!("  Start transfer within {} seconds.", neg_timeout))
            .await?;
        let esc_label = match self.terminal_type {
            TerminalType::Petscii => "<-",
            _ => "ESC",
        };
        self.send_line(&format!("  {} to cancel", self.cyan(esc_label)))
            .await?;
        self.send_line("").await?;
        self.flush().await?;

        if config::get_config().verbose {
            glog!("Download: IAC escaping={} protocol={:?}", self.xmodem_iac, protocol);
        }
        // Punter has no in-band cancel, so a restart after a C64-side abort can
        // strand stale bytes in the pipe; drain with a longer quiet gap to
        // clear them before this transfer's handshake (capped so a peer still
        // streaming can't stall the start).  Other protocols keep the short gap.
        if matches!(protocol, DownloadProtocol::Punter) {
            self.drain_input_until_quiet(250, Some(2000)).await;
        } else {
            self.drain_input().await;
        }

        let start = std::time::Instant::now();
        let cfg = config::get_config();
        let verbose = cfg.verbose;
        let mut writer_guard = self.writer.lock().await;
        let result = if matches!(protocol, DownloadProtocol::Zmodem) {
            // zmodem_send is batch-capable; download always sends
            // exactly one file, so we pass a single-element slice.
            let batch: [(&str, &[u8]); 1] = [(filename, &data)];
            crate::zmodem::zmodem_send(
                &mut self.reader,
                &mut *writer_guard,
                &batch,
                self.xmodem_iac,
                verbose,
            )
            .await
        } else if matches!(protocol, DownloadProtocol::Kermit) {
            let is_petscii = self.terminal_type == TerminalType::Petscii;
            let files = vec![crate::kermit::KermitSendFile {
                name: filename,
                data: &data,
                modtime: file_modtime,
                mode: file_mode,
            }];
            // Interactive download: hold the Send-Init until the receiver's
            // initiating NAK arrives (gated by `kermit_wait_for_receiver`) so
            // the S packet doesn't paint as garbage on a vintage client (e.g.
            // QTerm) that isn't yet in receive mode when the menu selection
            // is made.  Server mode never takes this path.
            crate::kermit::kermit_send_with_starting_seq(
                &mut self.reader,
                &mut *writer_guard,
                &files,
                self.xmodem_iac,
                is_petscii,
                verbose,
                0,
                false,
                cfg.kermit_wait_for_receiver,
            )
            .await
        } else if matches!(protocol, DownloadProtocol::Punter) {
            // C1 declares a PRG/SEQ type in its Phase-A block; auto-detect it
            // from the filename (text extensions → SEQ, else PRG).
            let file_type = crate::punter::PunterFileType::autodetect(filename);
            crate::punter::punter_send(
                &mut self.reader,
                &mut *writer_guard,
                &data,
                file_type,
                self.xmodem_iac,
                self.terminal_type == TerminalType::Petscii,
                verbose,
            )
            .await
        } else {
            // YMODEM always uses 1K data blocks; XMODEM-1K uses 1K
            // blocks without the filename header; classic XMODEM uses
            // 128-byte blocks only.
            let use_1k = matches!(
                protocol,
                DownloadProtocol::Xmodem1k | DownloadProtocol::Ymodem,
            );
            let ymodem = if matches!(protocol, DownloadProtocol::Ymodem) {
                Some(crate::xmodem::YmodemHeader {
                    filename: filename.to_string(),
                    size: file_size,
                    modtime: file_modtime,
                    mode: file_mode,
                })
            } else {
                None
            };
            crate::xmodem::xmodem_send(
                &mut self.reader,
                &mut *writer_guard,
                &data,
                self.xmodem_iac,
                self.terminal_type == TerminalType::Petscii,
                verbose,
                use_1k,
                ymodem,
            )
            .await
        };
        drop(writer_guard);
        let elapsed = start.elapsed();

        match result {
            Ok(()) => {
                // Brief pause so the remote terminal can switch back from
                // XMODEM mode to text display.
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                self.send_line("").await?;
                self.send_line(&format!(
                    "  {}",
                    self.green("Download complete!")
                ))
                .await?;
                self.send_line(&format!(
                    "  {} bytes, {} blocks, {:.1}s",
                    data.len(),
                    blocks,
                    elapsed.as_secs_f64()
                ))
                .await?;
            }
            Err(e) => {
                self.send_line("").await?;
                self.send_line(&format!(
                    "  {}",
                    self.red(&format!("Transfer failed: {}", e))
                ))
                .await?;
                // Option 4: with no in-band abort, a Punter give-up otherwise
                // strands the C64.  Drop carrier so it sees loss-of-carrier —
                // but only on a genuine give-up, NOT a user-initiated cancel
                // (ESC → "Transfer cancelled"), which must return to the menu
                // like every other protocol rather than drop the whole session.
                if matches!(protocol, DownloadProtocol::Punter)
                    && config::get_config().punter_hangup_on_failure
                    && !e.contains("cancelled")
                {
                    return self.punter_hangup().await;
                }
            }
        }

        self.send_line("").await?;
        self.send("  Press any key to continue.").await?;
        self.flush().await?;
        self.wait_for_key().await?;
        Ok(())
    }

    // ─── KERMIT SERVER MODE ─────────────────────────────────

    /// Idle as a Kermit server: peer drives the session by sending
    /// Kermit commands (`send`, `get`, `dir`, `finish`, `bye`, etc.).
    /// On exit, any files received during the session are written to
    /// the current transfer subdir using the same `validate_filename`
    /// rules as the interactive upload path.  Files whose sender-
    /// supplied names fail validation or collide with an existing
    /// path are skipped rather than clobbered, mirroring ZMODEM batch
    /// behavior.
    async fn file_transfer_kermit_server(&mut self) -> Result<(), std::io::Error> {
        self.ensure_transfer_dir().await?;

        if Self::is_disk_full() {
            self.show_error("Disk space is low. Server mode disabled.")
                .await?;
            return Ok(());
        }

        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;
        self.send_line(&format!("  {}", self.yellow("KERMIT SERVER MODE"))).await?;
        self.send_line(&sep).await?;
        self.send_line("").await?;
        self.send_line(&format!(
            "  {}",
            self.green("Listening for Kermit packets.")
        ))
        .await?;
        self.send_line(&format!(
            "  {}",
            self.dim("Your screen will be quiet — that's normal.")
        ))
        .await?;
        self.send_line("").await?;
        self.send_line(&format!(
            "  {}",
            self.amber("Needs a Kermit-aware client at your end.")
        ))
        .await?;
        self.send_line("  A plain telnet client cannot drive this.").await?;
        self.send_line("").await?;
        self.send_line("  Compatible clients:").await?;
        self.send_line(&format!(
            "    {} use the built-in Kermit menu",
            self.cyan("Tera Term / Kermit-95 —")
        ))
        .await?;
        self.send_line(&format!(
            "    {} run from a separate shell:",
            self.cyan("C-Kermit / G-Kermit —")
        ))
        .await?;
        self.send_line(&format!(
            "      {}",
            self.amber("kermit -j host:port -g file")
        ))
        .await?;
        self.send_line("").await?;
        self.send_line("  Remote commands once your client is talking:").await?;
        self.send_line(&format!(
            "    {}  upload to us",
            self.cyan("send <file>")
        ))
        .await?;
        self.send_line(&format!(
            "    {}  download from us",
            self.cyan("get <file>")
        ))
        .await?;
        self.send_line(&format!(
            "    {}  list / change dir / show help",
            self.cyan("remote dir / cwd / help")
        ))
        .await?;
        self.send_line(&format!(
            "    {}  end the session",
            self.cyan("finish | bye")
        ))
        .await?;
        self.send_line("").await?;
        let esc_label = match self.terminal_type {
            TerminalType::Petscii => "<-",
            _ => "ESC",
        };
        let idle_secs = config::get_config().kermit_idle_timeout;
        self.send_line(&format!(
            "  {} returns to the File Transfer menu.",
            self.cyan(esc_label)
        ))
        .await?;
        if idle_secs == 0 {
            self.send_line(
                "  Idle timeout disabled — server holds the session",
            )
            .await?;
            self.send_line("  open until the peer sends finish/bye.").await?;
        } else {
            let idle_display = if idle_secs >= 60 && idle_secs.is_multiple_of(60) {
                format!("{} min", idle_secs / 60)
            } else {
                format!("{}s", idle_secs)
            };
            self.send_line(&format!(
                "  After {} idle, we send the client an error packet",
                self.amber(&idle_display)
            ))
            .await?;
            self.send_line("  and disconnect.").await?;
        }
        self.send_line("  See kermit.html for full client setup.").await?;
        self.send_line("").await?;
        self.flush().await?;

        let verbose = config::get_config().verbose;
        let is_petscii = self.terminal_type == TerminalType::Petscii;

        // Saved/skipped lists are populated by the on-file callback as
        // each S-dispatch completes — see the `kermit_server` doc
        // comment.  Hoisting them out here keeps the summary render
        // below independent of what kermit_server returns.
        let mut saved: Vec<(String, usize)> = Vec::new();
        let mut skipped: Vec<(String, &'static str)> = Vec::new();
        let target_dir = self.transfer_path();

        let start = std::time::Instant::now();
        let result = {
            let mut writer_guard = self.writer.lock().await;
            crate::kermit::kermit_server_with_outcome(
                &mut self.reader,
                &mut *writer_guard,
                self.xmodem_iac,
                is_petscii,
                verbose,
                |rx| {
                    // Filename strictness now enforced at F-packet
                    // receipt (see kermit.rs F-packet handler), so any
                    // KermitReceive that reaches this callback already
                    // has a saver-acceptable name.  The defensive check
                    // stays because validate_filename is cheap and
                    // closes the door on any future kermit-side bypass.
                    if Self::validate_filename(&rx.filename).is_err() {
                        // Sanitize before the name can reach the terminal summary.
                        skipped.push((crate::aichat::sanitize_for_terminal(&rx.filename), "invalid filename"));
                        return;
                    }
                    // Defense-in-depth: re-validate the subdir before joining
                    // it.  rx.subdir is only ever set after kermit's own
                    // is_safe_relative_subdir today, but re-checking here
                    // closes the door on any future kermit-side bypass — the
                    // same belt-and-suspenders rationale as the filename
                    // re-check above.
                    if !crate::kermit::is_safe_relative_subdir(&rx.subdir) {
                        skipped.push((rx.filename.clone(), "unsafe subdir"));
                        return;
                    }
                    // Honor any `remote cwd <subdir>` the peer set —
                    // server-mode stamps `rx.subdir` with its current
                    // working subdir at the moment of receipt.  Without
                    // this, `remote cd assembly` followed by `put hello.txt`
                    // silently landed hello.txt in the base transfer_dir
                    // instead of transfer_dir/assembly, and a follow-up
                    // `remote dir` would show an empty assembly directory.
                    let dir = if rx.subdir.is_empty() {
                        target_dir.clone()
                    } else {
                        target_dir.join(&rx.subdir)
                    };
                    let filepath = dir.join(&rx.filename);
                    let meta = crate::xmodem::YmodemReceiveMeta {
                        size: rx.declared_size,
                        modtime: rx.modtime,
                        mode: rx.mode,
                    };
                    match Self::save_received_file_sync(
                        &filepath,
                        &rx.data,
                        Some(&meta),
                        rx.resumed,
                    ) {
                        Ok(()) => saved.push((rx.filename.clone(), rx.data.len())),
                        Err(SaveError::AlreadyExists) => {
                            skipped.push((rx.filename.clone(), "already exists"));
                        }
                        Err(SaveError::WriteFailed) => {
                            skipped.push((rx.filename.clone(), "write failed"));
                        }
                    }
                },
            )
            .await
        };
        let elapsed = start.elapsed();

        // On Err the closure may have already committed files to
        // disk before the failure — fall through to the summary so
        // the user sees which ones landed, with the error shown
        // alongside.  Early-returning here would silently drop
        // saved/skipped, which is the bug the audit caught.
        let (error_msg, idle_timeout) = match &result {
            Ok(outcome) => (None, outcome.idle_timeout),
            Err(e) => (Some(format!("Server session failed: {}", e)), false),
        };
        let total = saved.len() + skipped.len();

        // On idle-timeout the gateway has just written an E-packet
        // ("Server idle timeout") to the socket and we MUST return
        // before sending any more bytes.  The peer's protocol parser
        // is queued to read that E-packet on its next request — if we
        // mix in summary text first, the peer reads the text as
        // garbage, doesn't surface the E-packet message, and the
        // operator sees "too many retries" instead of a clean
        // "connection closed" with the timeout reason.
        // Returning ErrorKind::TimedOut here propagates up through
        // `?` in the menu loop and ends the telnet session, which
        // is what gives the peer a clean EOF on its socket.
        if idle_timeout {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "Kermit server idle timeout — disconnecting",
            ));
        }

        // Summary screen.
        self.post_transfer_settle().await;
        self.send_line("").await?;
        self.send_line(&format!(
            "  Server session ended in {:.1}s.",
            elapsed.as_secs_f64()
        ))
        .await?;
        if let Some(msg) = &error_msg {
            self.send_line(&format!("  {}", self.red(msg))).await?;
        }
        self.send_line(&format!(
            "  Received: {} file(s), saved: {}, skipped: {}.",
            total,
            saved.len(),
            skipped.len()
        ))
        .await?;
        for (name, size) in &saved {
            self.send_line(&format!(
                "    {} {} ({} bytes)",
                self.green("✓"),
                self.amber(name),
                size
            ))
            .await?;
        }
        for (name, reason) in &skipped {
            self.send_line(&format!(
                "    {} {} ({})",
                self.red("✗"),
                self.amber(name),
                reason
            ))
            .await?;
        }
        self.send_line("").await?;
        self.wait_for_key().await?;
        Ok(())
    }

    // ─── DELETE ─────────────────────────────────────────────

    async fn file_transfer_delete(&mut self) -> Result<(), std::io::Error> {
        self.ensure_transfer_dir().await?;
        let mut page: usize = 0;

        loop {
            let files = Self::list_transfer_entries_in(&self.transfer_path())
                .await?
                .into_iter()
                .filter(|(_, _, is_dir)| !is_dir)
                .map(|(name, size, _)| (name, size))
                .collect::<Vec<_>>();

            if files.is_empty() {
                self.show_error("No files to delete.").await?;
                return Ok(());
            }

            let total_pages = files.len().div_ceil(Self::TRANSFER_PAGE_SIZE);
            if page >= total_pages {
                page = total_pages - 1;
            }
            let offset = page * Self::TRANSFER_PAGE_SIZE;
            let end = (offset + Self::TRANSFER_PAGE_SIZE).min(files.len());
            let page_files = &files[offset..end];

            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            self.send_line(&format!(
                "  {}",
                self.yellow("DELETE FILE")
            ))
            .await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;
            self.send_line(&format!(
                "   {} {:<22} {}",
                self.cyan("#."),
                "Filename",
                "Size"
            ))
            .await?;
            self.send_line(&format!(
                "  {}",
                self.yellow(&"-".repeat(36))
            ))
            .await?;

            for (i, (name, size)) in page_files.iter().enumerate() {
                let num = i + 1;
                let display_name = if name.chars().count() > 22 {
                    let truncated: String = name.chars().take(19).collect();
                    format!("{}...", truncated)
                } else {
                    name.clone()
                };
                let size_display = Self::format_file_size(*size);
                self.send_line(&format!(
                    "  {:>2}. {:<22} {}",
                    num, display_name, size_display
                ))
                .await?;
            }

            self.send_line("").await?;
            self.send_line(&format!(
                "  Page {} of {}",
                page + 1,
                total_pages
            ))
            .await?;
            self.send_line("").await?;

            let mut nav = Vec::new();
            if page > 0 {
                nav.push(self.action_prompt("P", "Prev"));
            }
            if page + 1 < total_pages {
                nav.push(self.action_prompt("N", "Next"));
            }
            nav.push(self.action_prompt("Q", "Back"));
            nav.push(self.action_prompt("H", "Help"));
            let esc_label = match self.terminal_type {
                TerminalType::Petscii => "<-",
                _ => "ESC",
            };
            nav.push(self.action_prompt(esc_label, "Main"));
            self.send_line(&format!("  {}", nav.join(" | ")))
                .await?;
            self.send_line("").await?;
            self.send(&format!("  {} ", self.cyan("Select #:")))
                .await?;
            self.flush().await?;

            let input = match self.get_line_input().await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(()),
            };

            match input.as_str() {
                "p" => {
                    page = page.saturating_sub(1);
                }
                "n" => {
                    if page + 1 < total_pages {
                        page += 1;
                    }
                }
                "q" => return Ok(()),
                "h" => {
                    self.show_help_page("DELETE HELP", Self::delete_help_lines())
                        .await?;
                }
                other => {
                    if let Ok(num) = other.parse::<usize>() {
                        if num >= 1 && num <= page_files.len() {
                            let (ref filename, _) = page_files[num - 1];
                            self.send_line("").await?;
                            let p = format!(
                                "  Delete {}? ({}/{}) ",
                                self.amber(filename),
                                self.green("Y"),
                                self.red("N"),
                            );
                            self.send(&p).await?;
                            self.flush().await?;

                            match self.read_byte_filtered().await? {
                                Some(b)
                                    if {
                                        let ch =
                                            if self.terminal_type == TerminalType::Petscii {
                                                petscii_to_ascii_byte(b)
                                            } else {
                                                b
                                            };
                                        ch == b'y' || ch == b'Y'
                                    } =>
                                {
                                    self.send_line("").await?;
                                    let path = self.transfer_path().join(filename);
                                    match tokio::fs::remove_file(&path).await {
                                        Ok(()) => {
                                            self.send_line(&format!(
                                                "  {}",
                                                self.green("File deleted.")
                                            ))
                                            .await?;
                                            self.send_line("").await?;
                                            self.send(
                                                "  Press any key to continue.",
                                            )
                                            .await?;
                                            self.flush().await?;
                                            self.wait_for_key().await?;
                                        }
                                        Err(e) => {
                                            self.show_error(&format!(
                                                "Delete failed: {}",
                                                e
                                            ))
                                            .await?;
                                        }
                                    }
                                }
                                _ => {
                                    self.send_line("").await?;
                                    self.send_line("  Cancelled.").await?;
                                    self.send_line("").await?;
                                    self.send("  Press any key to continue.")
                                        .await?;
                                    self.flush().await?;
                                    self.wait_for_key().await?;
                                }
                            }
                        } else {
                            self.show_error("Invalid selection.").await?;
                        }
                    } else {
                        self.show_error("Enter a number, P, N, Q, or H.")
                            .await?;
                    }
                }
            }
        }
    }

    // ─── CHANGE DIRECTORY ───────────────────────────────────

    async fn file_transfer_chdir(&mut self) -> Result<(), std::io::Error> {
        self.ensure_transfer_dir().await?;

        let entries =
            Self::list_transfer_entries_in(&self.transfer_path()).await?;
        let dirs: Vec<&str> = entries
            .iter()
            .filter(|(_, _, is_dir)| *is_dir)
            .map(|(name, _, _)| name.as_str())
            .collect();

        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;
        self.send_line(&format!(
            "  {}",
            self.yellow("CHANGE DIRECTORY")
        ))
        .await?;
        self.send_line(&sep).await?;
        self.send_line("").await?;

        let max_dir = if self.terminal_type == TerminalType::Petscii {
            26
        } else {
            56
        };
        let dir_str =
            truncate_to_width(&self.transfer_dir_display(), max_dir);
        self.send_line(&format!(
            "  Current: {}",
            self.amber(&dir_str)
        ))
        .await?;
        self.send_line("").await?;

        let mut num = 0usize;
        if !self.transfer_subdir.is_empty() {
            num += 1;
            self.send_line(&format!(
                "  {:>2}. {}",
                num,
                self.cyan("..")
            ))
            .await?;
        }

        for name in &dirs {
            num += 1;
            let display = if name.chars().count() > 30 {
                let t: String = name.chars().take(27).collect();
                format!("{}...", t)
            } else {
                name.to_string()
            };
            self.send_line(&format!(
                "  {:>2}. {}/",
                num,
                self.cyan(&display)
            ))
            .await?;
        }

        if num == 0 {
            self.show_error("No subdirectories.").await?;
            return Ok(());
        }

        self.send_line("").await?;
        self.send(&format!("  {} ", self.cyan("Select #:")))
            .await?;
        self.flush().await?;

        let input = match self.get_line_input().await? {
            Some(s) if !s.is_empty() => s,
            _ => return Ok(()),
        };

        if input == "q" {
            return Ok(());
        }

        if let Ok(n) = input.parse::<usize>() {
            if n == 0 {
                self.show_error("Invalid selection.").await?;
                return Ok(());
            }
            let has_parent = !self.transfer_subdir.is_empty();
            if has_parent && n == 1 {
                if let Some(pos) = self.transfer_subdir.rfind('/') {
                    self.transfer_subdir.truncate(pos);
                } else {
                    self.transfer_subdir.clear();
                }
            } else {
                let dir_idx = if has_parent { n - 2 } else { n - 1 };
                if dir_idx < dirs.len() {
                    let name = dirs[dir_idx];
                    let prev = self.transfer_subdir.clone();
                    if self.transfer_subdir.is_empty() {
                        self.transfer_subdir = name.to_string();
                    } else {
                        self.transfer_subdir =
                            format!("{}/{}", self.transfer_subdir, name);
                    }
                    if !self.verify_transfer_path() {
                        self.transfer_subdir = prev;
                        self.show_error("Access denied.").await?;
                    }
                } else {
                    self.show_error("Invalid selection.").await?;
                }
            }
        } else {
            self.show_error("Enter a number or Q.").await?;
        }
        Ok(())
    }

    /// Create a new subdirectory inside the current transfer working directory,
    /// then offer to make it the working directory.  The name goes through
    /// `validate_filename` (a single component — no `..`, `/`, or leading dot),
    /// so the new path can't escape the transfer base; the optional switch is
    /// still re-checked with `verify_transfer_path` for defense in depth.
    async fn file_transfer_mkdir(&mut self) -> Result<(), std::io::Error> {
        self.ensure_transfer_dir().await?;

        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;
        self.send_line(&format!("  {}", self.yellow("MAKE DIRECTORY")))
            .await?;
        self.send_line(&sep).await?;
        self.send_line("").await?;

        let max_dir = if self.terminal_type == TerminalType::Petscii {
            26
        } else {
            56
        };
        let dir_str = truncate_to_width(&self.transfer_dir_display(), max_dir);
        self.send_line(&format!("  In: {}", self.amber(&dir_str)))
            .await?;
        self.send_line("").await?;
        self.send(&format!("  {} ", self.cyan("New directory name:")))
            .await?;
        self.flush().await?;

        let name = match self.get_line_input().await? {
            Some(s) if !s.trim().is_empty() => s.trim().to_string(),
            _ => return Ok(()), // empty / cancel
        };

        if let Err(msg) = Self::validate_filename(&name) {
            self.show_error(msg).await?;
            return Ok(());
        }

        let target = self.transfer_path().join(&name);
        match tokio::fs::create_dir(&target).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                self.show_error("That name already exists.").await?;
                return Ok(());
            }
            Err(e) => {
                self.show_error(&format!("Could not create: {}", e)).await?;
                return Ok(());
            }
        }

        let created = truncate_to_width(&format!("Created {}/", name), max_dir);
        self.send_line(&format!("  {}", self.green(&created))).await?;
        self.send_line("").await?;

        // Offer to switch into the new directory.
        self.send(&format!(
            "  {} ",
            self.cyan("Make this the working dir? (Y/N):")
        ))
        .await?;
        self.flush().await?;
        self.drain_input().await;
        let answer = match self.read_byte_filtered().await? {
            Some(b) => {
                if self.terminal_type == TerminalType::Petscii {
                    petscii_to_ascii_byte(b)
                } else {
                    b
                }
            }
            None => return Ok(()),
        };
        self.send_line("").await?;

        if answer == b'y' || answer == b'Y' {
            let prev = self.transfer_subdir.clone();
            if self.transfer_subdir.is_empty() {
                self.transfer_subdir = name.clone();
            } else {
                self.transfer_subdir = format!("{}/{}", self.transfer_subdir, name);
            }
            if self.verify_transfer_path() {
                let disp = truncate_to_width(&self.transfer_dir_display(), max_dir);
                self.send_line(&format!("  {} {}", self.dim("Now in:"), self.amber(&disp)))
                    .await?;
            } else {
                // Should not happen (validate_filename bars escape), but revert
                // and report rather than leave a bad subdir set.
                self.transfer_subdir = prev;
                self.show_error("Access denied.").await?;
                return Ok(());
            }
        }

        self.send_line("").await?;
        self.send("  Press any key to continue.").await?;
        self.flush().await?;
        self.wait_for_key().await?;
        Ok(())
    }

    // ─── SSH GATEWAY ────────────────────────────────────────

    /// Gateway timeout for SSH connection attempts.
    const GATEWAY_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

    /// Prompt for the remote SSH host, port, and username.  Password is
    /// collected separately (`gateway_password_prompt`) so we can skip
    /// it entirely when public-key authentication succeeds.
    async fn gateway_host_prompts(
        &mut self,
    ) -> Result<Option<(String, u16, String)>, std::io::Error> {
        self.send(&format!("  {} ", self.cyan("Host:")))
            .await?;
        self.flush().await?;
        let host = match self.get_line_input().await? {
            Some(s) if !s.is_empty() => s,
            _ => return Ok(None),
        };

        self.send(&format!("  {} ", self.cyan("Port (22):")))
            .await?;
        self.flush().await?;
        let port: u16 = match self.get_line_input().await? {
            Some(s) if s.is_empty() => 22,
            Some(s) => match s.parse::<u16>() {
                Ok(p) if p > 0 => p,
                _ => {
                    self.show_error("Invalid port number.").await?;
                    return Ok(None);
                }
            },
            None => return Ok(None),
        };

        self.send(&format!("  {} ", self.cyan("Username:")))
            .await?;
        self.flush().await?;
        let username = match self.get_line_input().await? {
            Some(s) if !s.is_empty() => s,
            _ => return Ok(None),
        };

        Ok(Some((host, port, username)))
    }

    /// Prompt for the remote SSH password.  Called only after public-key
    /// authentication is rejected by the remote so users who have set up
    /// the gateway's key in the remote's `authorized_keys` never see
    /// this prompt at all.
    async fn gateway_password_prompt(
        &mut self,
    ) -> Result<Option<String>, std::io::Error> {
        self.send(&format!("  {} ", self.cyan("Password:")))
            .await?;
        self.flush().await?;
        match self.get_password_input().await? {
            Some(s) => Ok(Some(s)),
            None => Ok(None),
        }
    }

    /// SSH gateway: connect to a remote server and proxy the session.
    async fn gateway_ssh(&mut self) -> Result<(), std::io::Error> {
        let cfg = config::get_config();
        let idle_timeout = std::time::Duration::from_secs(cfg.idle_timeout_secs);

        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;
        self.send_line(&format!("  {}", self.yellow("SSH GATEWAY")))
            .await?;
        self.send_line(&sep).await?;
        self.send_line("").await?;
        self.send_line("  Connect to a remote SSH server.")
            .await?;
        let esc_label = match self.terminal_type {
            TerminalType::Petscii => "<-",
            _ => "ESC",
        };
        self.send_line(&format!(
            "  Press {} at any prompt to cancel.",
            self.cyan(esc_label)
        ))
        .await?;
        let auth_label = if cfg.ssh_gateway_auth == "password" {
            self.yellow("password")
        } else {
            self.green("gateway key")
        };
        self.send_line(&format!("  Auth: {}", auth_label)).await?;
        self.send_line("").await?;

        let (host, port, username) = if idle_timeout.is_zero() {
            match self.gateway_host_prompts().await {
                Ok(Some(v)) => v,
                Ok(None) => return Ok(()),
                Err(e) => return Err(e),
            }
        } else {
            match tokio::time::timeout(
                idle_timeout,
                self.gateway_host_prompts(),
            )
            .await
            {
                Ok(Ok(Some(v))) => v,
                Ok(Ok(None)) => return Ok(()),
                Ok(Err(e)) => return Err(e),
                Err(_) => {
                    let _ = self
                        .send_line("\r\nDisconnected: idle timeout.")
                        .await;
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "idle timeout in gateway prompts",
                    ));
                }
            }
        };

        self.send_line("").await?;
        self.send_line(&format!(
            "  Connecting to {}:{}...",
            self.amber(&host),
            port
        ))
        .await?;
        self.flush().await?;

        // Connect to remote SSH server
        let ssh_config = std::sync::Arc::new(russh::client::Config {
            inactivity_timeout: Some(std::time::Duration::from_secs(600)),
            ..Default::default()
        });
        let server_key_slot: Arc<std::sync::Mutex<Option<russh::keys::PublicKey>>> =
            Arc::new(std::sync::Mutex::new(None));
        let handler = GatewayHandler {
            server_key: server_key_slot.clone(),
        };

        let mut session = match tokio::time::timeout(
            Self::GATEWAY_CONNECT_TIMEOUT,
            russh::client::connect(ssh_config, (host.as_str(), port), handler),
        )
        .await
        {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                self.show_error(&format!("Connection failed: {}", e))
                    .await?;
                return Ok(());
            }
            Err(_) => {
                self.show_error("Connection timed out.").await?;
                return Ok(());
            }
        };

        // Verify server host key against known-hosts file
        let server_key = server_key_slot
            .lock()
            .ok()
            .and_then(|mut slot| slot.take());
        let Some(ref key) = server_key else {
            self.show_error("Could not verify server host key.").await?;
            let _ = session
                .disconnect(russh::Disconnect::ByApplication, "no host key", "")
                .await;
            return Ok(());
        };
        {
            match check_known_host(&host, port, key) {
                HostKeyStatus::Known => {}
                HostKeyStatus::Unknown => {
                    let fingerprint = key.fingerprint(russh::keys::HashAlg::Sha256);
                    let algo = key.algorithm();
                    self.send_line("").await?;
                    self.send_line(&format!(
                        "  {}",
                        self.yellow("Host key not recognized.")
                    ))
                    .await?;
                    let algo_str = algo.to_string();
                    let fp_str = fingerprint.to_string();
                    self.send_line(&format!("  Type: {}", self.cyan(&algo_str)))
                        .await?;
                    self.send_line(&format!(
                        "  Fingerprint: {}",
                        self.cyan(&fp_str)
                    ))
                    .await?;
                    self.send_line("").await?;
                    self.send(&format!(
                        "  {} ",
                        self.cyan("Trust this host? (Y/N):")
                    ))
                    .await?;
                    self.flush().await?;
                    self.drain_input().await;
                    let answer = match self.read_byte_filtered().await? {
                        Some(b) => {
                            if self.terminal_type == TerminalType::Petscii {
                                petscii_to_ascii_byte(b)
                            } else {
                                b
                            }
                        }
                        None => return Ok(()),
                    };
                    self.send_line("").await?;
                    if answer != b'y' && answer != b'Y' {
                        let _ = session
                            .disconnect(russh::Disconnect::ByApplication, "host key rejected", "")
                            .await;
                        self.show_error("Connection aborted.").await?;
                        return Ok(());
                    }
                    save_known_host(&host, port, key);
                    glog!(
                        "SSH gateway: TOFU-accepted host key for {}:{} ({} {})",
                        host,
                        port,
                        key.algorithm(),
                        key.fingerprint(russh::keys::HashAlg::Sha256),
                    );
                    self.send_line(&format!(
                        "  {}",
                        self.green("Host key saved.")
                    ))
                    .await?;
                }
                HostKeyStatus::Changed => {
                    let fingerprint = key.fingerprint(russh::keys::HashAlg::Sha256);
                    let algo_str = key.algorithm().to_string();
                    let fp_str = fingerprint.to_string();
                    self.send_line("").await?;
                    self.send_line(&format!(
                        "  {}",
                        self.red("WARNING: HOST KEY HAS CHANGED!")
                    ))
                    .await?;
                    self.send_line(&format!(
                        "  {}",
                        self.red("This could indicate a security threat.")
                    ))
                    .await?;
                    self.send_line(&format!("  New type: {}", self.cyan(&algo_str)))
                        .await?;
                    self.send_line(&format!(
                        "  New fingerprint: {}",
                        self.cyan(&fp_str)
                    ))
                    .await?;
                    self.send_line("").await?;
                    self.send(&format!(
                        "  {} ",
                        self.cyan("Update key? (Y/N):")
                    ))
                    .await?;
                    self.flush().await?;
                    self.drain_input().await;
                    let answer = match self.read_byte_filtered().await? {
                        Some(b) => {
                            if self.terminal_type == TerminalType::Petscii {
                                petscii_to_ascii_byte(b)
                            } else {
                                b
                            }
                        }
                        None => return Ok(()),
                    };
                    self.send_line("").await?;
                    if answer == b'y' || answer == b'Y' {
                        save_known_host(&host, port, key);
                        glog!(
                            "SSH gateway: operator UPDATED changed host key for {}:{} (new {} {})",
                            host,
                            port,
                            key.algorithm(),
                            key.fingerprint(russh::keys::HashAlg::Sha256),
                        );
                        self.send_line(&format!(
                            "  {}",
                            self.green("Host key updated.")
                        ))
                        .await?;
                    } else {
                        glog!(
                            "SSH gateway: operator REJECTED changed host key for {}:{} (presented {} {})",
                            host,
                            port,
                            key.algorithm(),
                            key.fingerprint(russh::keys::HashAlg::Sha256),
                        );
                        let _ = session
                            .disconnect(russh::Disconnect::ByApplication, "host key rejected", "")
                            .await;
                        self.show_error("Connection aborted.").await?;
                        return Ok(());
                    }
                }
            }
        }

        // Authenticate using the configured mode.  The server-config
        // `ssh_gateway_auth` key dictates the method: "key" uses the
        // gateway's own auto-generated Ed25519 client key (copy the
        // public half printed by `cat gateway_client_key.pub` into the
        // remote's `~/.ssh/authorized_keys` first); "password" prompts
        // the operator each time.  No silent fallback — the remote sees
        // exactly one auth method, so failures are unambiguous.
        let mut authed = false;
        if cfg.ssh_gateway_auth == "password" {
            let password = if idle_timeout.is_zero() {
                match self.gateway_password_prompt().await {
                    Ok(Some(p)) => p,
                    Ok(None) => {
                        let _ = session
                            .disconnect(russh::Disconnect::ByApplication, "cancelled", "")
                            .await;
                        return Ok(());
                    }
                    Err(e) => {
                        let _ = session
                            .disconnect(russh::Disconnect::ByApplication, "cancelled", "")
                            .await;
                        return Err(e);
                    }
                }
            } else {
                match tokio::time::timeout(
                    idle_timeout,
                    self.gateway_password_prompt(),
                )
                .await
                {
                    Ok(Ok(Some(p))) => p,
                    Ok(Ok(None)) => {
                        let _ = session
                            .disconnect(russh::Disconnect::ByApplication, "cancelled", "")
                            .await;
                        return Ok(());
                    }
                    Ok(Err(e)) => {
                        let _ = session
                            .disconnect(russh::Disconnect::ByApplication, "cancelled", "")
                            .await;
                        return Err(e);
                    }
                    Err(_) => {
                        let _ = session
                            .disconnect(russh::Disconnect::ByApplication, "idle timeout", "")
                            .await;
                        let _ = self
                            .send_line("\r\nDisconnected: idle timeout.")
                            .await;
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "idle timeout at password prompt",
                        ));
                    }
                }
            };
            match session.authenticate_password(&username, &password).await {
                Ok(russh::client::AuthResult::Success) => {
                    authed = true;
                    glog!(
                        "SSH gateway: authenticated to {}:{} as {} via password",
                        host, port, username,
                    );
                }
                Ok(russh::client::AuthResult::Failure { .. }) => {}
                Err(e) => {
                    let _ = session
                        .disconnect(russh::Disconnect::ByApplication, "auth error", "")
                        .await;
                    self.show_error(&format!("Auth error: {}", e)).await?;
                    return Ok(());
                }
            }
        } else {
            // "key" mode — gateway's Ed25519 client key, no password fallback.
            match crate::ssh::load_or_generate_client_key() {
                Ok(key) => {
                    // best_supported_rsa_hash returns Result<Option<Option<HashAlg>>>:
                    //   outer Option = "server doesn't specify a preference",
                    //   inner Option = "preference is 'no hash' (i.e., not RSA)".
                    // Two flattens collapse both to Option<HashAlg>.
                    let hash_alg = session
                        .best_supported_rsa_hash()
                        .await
                        .ok()
                        .flatten()
                        .flatten();
                    match session
                        .authenticate_publickey(
                            &username,
                            russh::keys::PrivateKeyWithHashAlg::new(
                                std::sync::Arc::new(key),
                                hash_alg,
                            ),
                        )
                        .await
                    {
                        Ok(russh::client::AuthResult::Success) => {
                            authed = true;
                            glog!(
                                "SSH gateway: authenticated to {}:{} as {} via pubkey",
                                host, port, username,
                            );
                            self.send_line(&format!(
                                "  {}",
                                self.green("Authenticated (gateway key).")
                            ))
                            .await?;
                        }
                        Ok(russh::client::AuthResult::Failure { .. }) => {}
                        Err(e) => {
                            glog!("SSH gateway: pubkey auth error: {}", e);
                        }
                    }
                }
                Err(e) => {
                    glog!("SSH gateway: client key unavailable: {}", e);
                }
            }
        }
        if !authed {
            let _ = session
                .disconnect(russh::Disconnect::ByApplication, "auth failed", "")
                .await;
            if cfg.ssh_gateway_auth == "password" {
                self.show_error("Authentication failed.").await?;
            } else {
                self.show_error(
                    "Key authentication failed. Copy the gateway's public \
                     key (shown in the GUI Server > More popup) into the \
                     remote's ~/.ssh/authorized_keys, or switch to Password \
                     mode from Configuration > Gateway Configuration.",
                )
                .await?;
            }
            return Ok(());
        }

        // Open channel and request PTY + shell.  Every error path from
        // here forward must call `session.disconnect` before returning
        // — otherwise the remote sees an orphaned, still-authenticated
        // session and its connection slot stays occupied until a TCP
        // timeout eventually reaps it.
        let channel = match session.channel_open_session().await {
            Ok(ch) => ch,
            Err(e) => {
                let _ = session
                    .disconnect(russh::Disconnect::ByApplication, "channel open failed", "")
                    .await;
                self.show_error(&format!("Channel error: {}", e))
                    .await?;
                return Ok(());
            }
        };

        let (cols, rows, term) = match self.terminal_type {
            TerminalType::Petscii => (40, 25, "dumb"),
            TerminalType::Ascii => (80, 24, "dumb"),
            TerminalType::Ansi => (80, 24, "xterm"),
        };

        if let Err(e) = channel
            .request_pty(false, term, cols, rows, 0, 0, &[])
            .await
        {
            let _ = session
                .disconnect(russh::Disconnect::ByApplication, "pty request failed", "")
                .await;
            self.show_error(&format!("PTY error: {}", e)).await?;
            return Ok(());
        }
        if let Err(e) = channel.request_shell(false).await {
            let _ = session
                .disconnect(russh::Disconnect::ByApplication, "shell request failed", "")
                .await;
            self.show_error(&format!("Shell error: {}", e)).await?;
            return Ok(());
        }

        let esc_label = match self.terminal_type {
            TerminalType::Petscii => "<-",
            _ => "ESC",
        };
        self.send_line(&format!(
            "  {}",
            self.green("Connected.")
        ))
        .await?;
        self.send_line(&format!(
            "  Press {} twice to disconnect.",
            self.cyan(esc_label)
        ))
        .await?;
        self.send_line("").await?;
        self.flush().await?;

        // Proxy I/O between telnet client and SSH channel
        let stream = channel.into_stream();
        let (mut ssh_reader, mut ssh_writer) = tokio::io::split(stream);

        let reader = &mut self.reader;
        let writer = &self.writer;
        let erase_char = self.erase_char;
        let is_petscii = self.terminal_type == TerminalType::Petscii;
        let is_ascii = self.terminal_type == TerminalType::Ascii;
        // Idle bound for the live bridge: if neither side sends a byte
        // within this window, tear the session down so a half-open client
        // (laptop asleep, NAT drop) can't pin it — and its max_sessions
        // slot — forever.  Copied out before the reader borrow below; zero
        // disables it, matching the rest of the session's idle policy.
        let idle_timeout = self.idle_timeout;

        let mut ssh_buf = [0u8; 4096];
        let mut filter_buf: Vec<u8> = Vec::new();
        let mut ansi_state: u8 = 0;
        let mut last_cr = false;
        let mut last_was_esc = false;
        let esc_byte: u8 = if is_petscii { 0x5F } else { 0x1B };

        // Gateway byte-tracing (EGATEWAY_GATEWAY_DEBUG).  `dbg_in` accumulates
        // every byte we forward to the remote shell and is flushed to the log
        // on each CR/LF — so the log shows exactly the line bash receives on
        // RETURN, which is the crux of the c64sshwrap long-line truncation
        // investigation.  A no-newline stream (binary paste, TUI input editor)
        // is capped at GW_DBG_IN_CAP bytes so a long-running debug session
        // doesn't grow the buffer without bound.
        let gw_debug = gw_debug_enabled(cfg.gateway_debug);
        let mut dbg_in: Vec<u8> = Vec::new();
        // Per-byte timing: `+Δms` is the gap since the previous input byte and
        // `t=…` is elapsed since trace start.  Large gaps = bytes typed live
        // (character-mode terminal); a near-zero burst = a line dumped at once
        // (screen-memory walk).  This is what tells the two mechanisms apart.
        let gw_start = std::time::Instant::now();
        let mut gw_last = gw_start;
        if gw_debug {
            glog!(
                "[gw] SSH gateway trace ON — term={:?} pty=({}x{},{})",
                self.terminal_type, cols, rows, term
            );
        }

        loop {
            tokio::select! {
                _ = tokio::time::sleep(idle_timeout), if !idle_timeout.is_zero() => {
                    // Idle in both directions past the timeout window —
                    // disconnect so a half-open client can't pin the
                    // session and leak its max_sessions slot.
                    break;
                }
                byte = read_byte_iac_filtered(reader, true) => {
                    match byte {
                        Ok(Some(b)) if is_esc_key(b, is_petscii) => {
                            if last_was_esc {
                                break; // Two consecutive ESC presses — disconnect
                            }
                            last_was_esc = true;
                        }
                        Ok(Some(b)) => {
                            // Forward the previously held ESC before this byte
                            if last_was_esc {
                                last_was_esc = false;
                                let e = if is_petscii { petscii_to_ascii_byte(esc_byte) } else { esc_byte };
                                if let Some(e) = normalize_gateway_input(e, &mut last_cr)
                                    && ssh_writer.write_all(&[e]).await.is_err() { break; }
                            }
                            let raw = b;
                            let b = if is_petscii { petscii_to_ascii_byte(b) } else { b };
                            let b = if b == erase_char && erase_char != 0x7F { 0x7F } else { b };
                            if let Some(b) = normalize_gateway_input(b, &mut last_cr) {
                                if gw_debug {
                                    let now = std::time::Instant::now();
                                    let dt = now.duration_since(gw_last).as_millis();
                                    let t = now.duration_since(gw_start).as_millis();
                                    gw_last = now;
                                    let swap = if raw != b { format!(" (petscii 0x{:02x})", raw) } else { String::new() };
                                    glog!("[gw-in] +{:>5}ms t={:>6}ms  byte=0x{:02x} '{}'{}",
                                        dt, t, b,
                                        if (0x20..=0x7E).contains(&b) { b as char } else { '.' },
                                        swap);
                                    if b == b'\r' || b == b'\n' {
                                        glog!("[gw-in] line ({} bytes) -> {}", dbg_in.len(), gw_hexdump(&dbg_in));
                                        dbg_in.clear();
                                    } else {
                                        dbg_in.push(b);
                                        if dbg_in.len() >= GW_DBG_IN_CAP {
                                            glog!("[gw-in] line (no CR/LF, {} bytes cap) -> {}",
                                                dbg_in.len(), gw_hexdump(&dbg_in));
                                            dbg_in.clear();
                                        }
                                    }
                                }
                                if ssh_writer.write_all(&[b]).await.is_err() { break; }
                                if ssh_writer.flush().await.is_err() { break; }
                            }
                        }
                        _ => break,
                    }
                }
                n = ssh_reader.read(&mut ssh_buf) => {
                    match n {
                        Ok(0) => break,
                        Ok(n) => {
                            let data = if is_petscii || is_ascii {
                                filter_buf.clear();
                                filter_gateway_output(&ssh_buf[..n], &mut ansi_state, is_petscii, &mut filter_buf);
                                &filter_buf[..]
                            } else {
                                &ssh_buf[..n]
                            };
                            if gw_debug {
                                glog!("[gw-out] raw {} bytes -> {}", n, gw_hexdump(&ssh_buf[..n]));
                                if is_petscii || is_ascii {
                                    glog!("[gw-out] filtered {} bytes -> {}", data.len(), gw_hexdump(data));
                                }
                            }
                            if !data.is_empty() {
                                let mut w = writer.lock().await;
                                if w.write_all(data).await.is_err() { break; }
                                if w.flush().await.is_err() { break; }
                            }
                        }
                        Err(_) => break,
                    }
                }
            }
        }

        // Clean up SSH channel and session
        let _ = ssh_writer.shutdown().await;
        drop(ssh_writer);
        drop(ssh_reader);
        let _ = session
            .disconnect(russh::Disconnect::ByApplication, "bye", "")
            .await;

        self.send_line("").await?;
        self.send_line(&format!(
            "  {}",
            self.yellow("Connection closed.")
        ))
        .await?;
        self.send_line("").await?;
        self.send("  Press any key to continue.").await?;
        self.flush().await?;
        if idle_timeout.is_zero() {
            self.wait_for_key().await?;
        } else {
            match tokio::time::timeout(idle_timeout, self.wait_for_key()).await {
                Ok(result) => result?,
                Err(_) => {
                    let _ = self
                        .send_line("\r\nDisconnected: idle timeout.")
                        .await;
                }
            }
        }
        Ok(())
    }

    // ─── TELNET GATEWAY ──────────────────────────────────────

    /// Telnet gateway: connect to a remote telnet server and proxy the session.
    async fn gateway_telnet(&mut self) -> Result<(), std::io::Error> {
        let cfg = config::get_config();
        let idle_timeout = std::time::Duration::from_secs(cfg.idle_timeout_secs);
        let esc_label = match self.terminal_type {
            TerminalType::Petscii => "<-",
            _ => "ESC",
        };

        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;
        self.send_line(&format!("  {}", self.yellow("TELNET GATEWAY")))
            .await?;
        self.send_line(&sep).await?;
        self.send_line("").await?;
        self.send_line("  Connect to a remote telnet server.")
            .await?;
        self.send_line(&format!(
            "  Press {} at any prompt to cancel.",
            self.cyan(esc_label)
        ))
        .await?;
        let mode_label = if cfg.telnet_gateway_raw {
            self.red("Raw TCP (no IAC parsing)")
        } else {
            self.green("Telnet protocol")
        };
        self.send_line(&format!("  Mode: {}", mode_label)).await?;
        self.send_line("").await?;

        // Gather host and port
        let get_host_port = async {
            self.send(&format!("  {} ", self.cyan("Host:")))
                .await?;
            self.flush().await?;
            let host = match self.get_line_input().await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(None),
            };

            self.send(&format!("  {} ", self.cyan("Port (23):")))
                .await?;
            self.flush().await?;
            let port: u16 = match self.get_line_input().await? {
                Some(s) if s.is_empty() => 23,
                Some(s) => match s.parse::<u16>() {
                    Ok(p) if p > 0 => p,
                    _ => {
                        self.show_error("Invalid port number.").await?;
                        return Ok(None);
                    }
                },
                None => return Ok(None),
            };

            Ok::<Option<(String, u16)>, std::io::Error>(Some((host, port)))
        };

        let (host, port) = if idle_timeout.is_zero() {
            match get_host_port.await {
                Ok(Some(hp)) => hp,
                Ok(None) => return Ok(()),
                Err(e) => return Err(e),
            }
        } else {
            match tokio::time::timeout(idle_timeout, get_host_port).await {
                Ok(Ok(Some(hp))) => hp,
                Ok(Ok(None)) => return Ok(()),
                Ok(Err(e)) => return Err(e),
                Err(_) => {
                    let _ = self
                        .send_line("\r\nDisconnected: idle timeout.")
                        .await;
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "idle timeout in telnet gateway prompts",
                    ));
                }
            }
        };

        self.send_line("").await?;
        self.send_line(&format!(
            "  Connecting to {}:{}...",
            self.amber(&host),
            port
        ))
        .await?;
        self.flush().await?;

        // Connect to remote telnet server
        let addr = format!("{}:{}", host, port);
        let remote = match tokio::time::timeout(
            Self::GATEWAY_CONNECT_TIMEOUT,
            tokio::net::TcpStream::connect(&addr),
        )
        .await
        {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                self.show_error(&format!("Connection failed: {}", e))
                    .await?;
                return Ok(());
            }
            Err(_) => {
                self.show_error("Connection timed out.").await?;
                return Ok(());
            }
        };
        let _ = remote.set_nodelay(true);

        self.send_line(&format!(
            "  {}",
            self.green("Connected.")
        ))
        .await?;
        self.send_line(&format!(
            "  Press {} twice to disconnect.",
            self.cyan(esc_label)
        ))
        .await?;
        self.send_line("").await?;
        self.flush().await?;

        // Proxy I/O between local telnet client and remote telnet server
        let (mut remote_reader, mut remote_writer) = remote.into_split();

        let reader = &mut self.reader;
        let writer = &self.writer;
        let is_petscii = self.terminal_type == TerminalType::Petscii;
        let is_ascii = self.terminal_type == TerminalType::Ascii;

        let erase_char = self.erase_char;
        let mut remote_buf = [0u8; 4096];
        // Idle bound for the live bridge (see gateway_ssh): disconnect a
        // half-open client so it can't pin the session's max_sessions slot.
        // Zero disables it, matching the session's idle policy.
        let idle_timeout = self.idle_timeout;
        let mut filter_buf: Vec<u8> = Vec::new();
        let mut ansi_state: u8 = 0;
        let mut last_was_esc = false;
        let esc_byte: u8 = if is_petscii { 0x5F } else { 0x1B };

        // Telnet-client IAC state machine + option negotiator.  Whether
        // we offer TTYPE / NAWS proactively at connect is gated by the
        // `telnet_gateway_negotiate` config flag.  ECHO cooperation is
        // always on.  In raw mode (`telnet_gateway_raw = true`) the
        // parser is still constructed but its initial offers and
        // negotiation paths are bypassed — see the `raw` checks below.
        let raw = cfg.telnet_gateway_raw;
        let terminal_name = gateway_terminal_name(self.terminal_type).to_string();
        let (cols_default, rows_default) = gateway_default_window(self.terminal_type);
        let cols = self.window_width.unwrap_or(cols_default);
        let rows = self.window_height.unwrap_or(rows_default);
        let (mut iac, initial_offers) = GatewayTelnetIac::new(
            !raw && cfg.telnet_gateway_negotiate,
            terminal_name,
            cols,
            rows,
        );
        if !raw && !initial_offers.is_empty() {
            if remote_writer.write_all(&initial_offers).await.is_err() {
                let _ = remote_writer.shutdown().await;
                return Ok(());
            }
            let _ = remote_writer.flush().await;
        }
        let mut data_from_remote: Vec<u8> = Vec::with_capacity(4096);
        let mut replies_to_remote: Vec<u8> = Vec::new();

        // Gateway byte-tracing (EGATEWAY_GATEWAY_DEBUG) — mirrors the SSH
        // gateway path so the Telnet Gateway can be checked for the same
        // c64sshwrap long-line truncation.
        let gw_debug = gw_debug_enabled(cfg.gateway_debug);
        let mut dbg_in: Vec<u8> = Vec::new();
        let gw_start = std::time::Instant::now();
        let mut gw_last = gw_start;
        if gw_debug {
            glog!(
                "[gw] Telnet gateway trace ON — term={:?} raw={} window=({}x{})",
                self.terminal_type, raw, cols, rows
            );
        }

        loop {
            tokio::select! {
                _ = tokio::time::sleep(idle_timeout), if !idle_timeout.is_zero() => {
                    // Idle in both directions past the timeout window —
                    // disconnect so a half-open client can't pin the
                    // session and leak its max_sessions slot.
                    break;
                }
                event = read_gateway_event(reader) => {
                    match event {
                        Ok(GatewayInboundEvent::Data(b)) if is_esc_key(b, is_petscii) => {
                            if last_was_esc {
                                break; // Two consecutive ESC presses — disconnect
                            }
                            last_was_esc = true;
                        }
                        Ok(GatewayInboundEvent::Data(b)) => {
                            // Forward the previously held ESC before this byte
                            if last_was_esc {
                                last_was_esc = false;
                                let e = if is_petscii { petscii_to_ascii_byte(esc_byte) } else { esc_byte };
                                let write_ok = if raw {
                                    remote_writer.write_all(&[e]).await.is_ok()
                                } else {
                                    write_telnet_data(&mut remote_writer, &[e]).await.is_ok()
                                };
                                if !write_ok { break; }
                            }
                            let raw_in = b;
                            let b = if is_petscii { petscii_to_ascii_byte(b) } else { b };
                            let b = if b == erase_char && erase_char != 0x7F { 0x7F } else { b };
                            if gw_debug {
                                let now = std::time::Instant::now();
                                let dt = now.duration_since(gw_last).as_millis();
                                let t = now.duration_since(gw_start).as_millis();
                                gw_last = now;
                                let swap = if raw_in != b { format!(" (petscii 0x{:02x})", raw_in) } else { String::new() };
                                glog!("[gw-in] +{:>5}ms t={:>6}ms  byte=0x{:02x} '{}'{}",
                                    dt, t, b,
                                    if (0x20..=0x7E).contains(&b) { b as char } else { '.' },
                                    swap);
                                if b == b'\r' || b == b'\n' {
                                    glog!("[gw-in] line ({} bytes) -> {}", dbg_in.len(), gw_hexdump(&dbg_in));
                                    dbg_in.clear();
                                } else {
                                    dbg_in.push(b);
                                    if dbg_in.len() >= GW_DBG_IN_CAP {
                                        glog!("[gw-in] line (no CR/LF, {} bytes cap) -> {}",
                                            dbg_in.len(), gw_hexdump(&dbg_in));
                                        dbg_in.clear();
                                    }
                                }
                            }
                            let write_ok = if raw {
                                remote_writer.write_all(&[b]).await.is_ok()
                            } else {
                                write_telnet_data(&mut remote_writer, &[b]).await.is_ok()
                            };
                            if !write_ok { break; }
                            if remote_writer.flush().await.is_err() { break; }
                        }
                        Ok(GatewayInboundEvent::NawsResize(cols, rows)) => {
                            if !raw {
                                let mut naws_update = Vec::new();
                                iac.send_naws_update(cols, rows, &mut naws_update);
                                if !naws_update.is_empty() {
                                    if remote_writer.write_all(&naws_update).await.is_err() { break; }
                                    if remote_writer.flush().await.is_err() { break; }
                                }
                            }
                            // In raw mode we swallow the resize — the
                            // destination isn't speaking telnet so there's
                            // nowhere to forward it to.
                        }
                        Ok(GatewayInboundEvent::Eof) => break,
                        Err(_) => break,
                    }
                }
                n = remote_reader.read(&mut remote_buf) => {
                    match n {
                        Ok(0) => break,
                        Ok(n) => {
                            let raw_slice: &[u8];
                            if raw {
                                // No IAC parsing — bytes are user data straight through.
                                raw_slice = &remote_buf[..n];
                            } else {
                                data_from_remote.clear();
                                replies_to_remote.clear();
                                for &b in &remote_buf[..n] {
                                    iac.feed(b, &mut data_from_remote, &mut replies_to_remote);
                                }
                                if !replies_to_remote.is_empty() {
                                    if remote_writer.write_all(&replies_to_remote).await.is_err() { break; }
                                    if remote_writer.flush().await.is_err() { break; }
                                }
                                raw_slice = &data_from_remote[..];
                            }
                            let data: &[u8] = if is_petscii || is_ascii {
                                filter_buf.clear();
                                filter_gateway_output(raw_slice, &mut ansi_state, is_petscii, &mut filter_buf);
                                &filter_buf[..]
                            } else {
                                raw_slice
                            };
                            if gw_debug {
                                glog!("[gw-out] raw {} bytes -> {}", raw_slice.len(), gw_hexdump(raw_slice));
                                if is_petscii || is_ascii {
                                    glog!("[gw-out] filtered {} bytes -> {}", data.len(), gw_hexdump(data));
                                }
                            }
                            if !data.is_empty() {
                                let mut w = writer.lock().await;
                                // Always IAC-escape when writing to the
                                // local user — their client is a real
                                // telnet peer and a literal 0xFF would
                                // be misinterpreted as IAC.
                                if write_telnet_data(&mut **w, data).await.is_err() { break; }
                                if w.flush().await.is_err() { break; }
                            }
                        }
                        Err(_) => break,
                    }
                }
            }
        }

        // Clean up
        let _ = remote_writer.shutdown().await;

        self.send_line("").await?;
        self.send_line(&format!(
            "  {}",
            self.yellow("Connection closed.")
        ))
        .await?;
        self.send_line("").await?;
        self.send("  Press any key to continue.").await?;
        self.flush().await?;
        if idle_timeout.is_zero() {
            self.wait_for_key().await?;
        } else {
            match tokio::time::timeout(idle_timeout, self.wait_for_key()).await {
                Ok(result) => result?,
                Err(_) => {
                    let _ = self
                        .send_line("\r\nDisconnected: idle timeout.")
                        .await;
                }
            }
        }
        Ok(())
    }

    // ─── SERIAL GATEWAY ─────────────────────────────────────

    /// True when `id` is the very port this session arrived on.
    /// Bridging that port back into itself would loop the user's
    /// terminal, so the picker marks it ineligible and `gateway_serial`
    /// rejects a stale pick of it.  A non-serial session (telnet/SSH)
    /// never owns a serial port, so this is always false for them — they
    /// may bridge to any eligible port.
    fn is_own_arrival_port(&self, id: crate::config::SerialPortId) -> bool {
        self.is_serial && self.serial_port_id == Some(id)
    }

    /// Render the Serial Gateway port picker.  Returns the user's pick
    /// (a local port or a registered remote console port, §9 #12), or
    /// `Ok(None)` if they backed out.  Always shows both local ports'
    /// status — even when only one is eligible — so the menu structure
    /// stays consistent and the user can see *why* a port is unavailable.
    async fn gateway_serial_picker(
        &mut self,
    ) -> Result<Option<GatewayPick>, std::io::Error> {
        use crate::config::{SerialPortId, SERIAL_PORT_IDS};

        loop {
            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            self.send_line(&format!("  {}", self.yellow("SERIAL GATEWAY")))
                .await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;

            let cfg = config::get_config();
            // When peer-dial is on, show this gateway's address so a caller
            // knows the phone-book form to dial from a modem port:
            // `ATD <Port>@<ip>`.  One line, and only when the feature is on,
            // so the default layout is unchanged.
            if cfg.allow_peer_dial {
                let ip = crate::serial::primary_local_ip();
                // Keep <=40 cols: "Dial: <Port>@" (13) + IPv4 (<=15) = 28.
                self.send_line(&format!(
                    "  {}",
                    self.dim(&format!("Dial: <Port>@{}", ip))
                ))
                .await?;
                self.send_line("").await?;
            }
            let mut any_eligible = false;
            for id in SERIAL_PORT_IDS {
                let port = cfg.port(id);
                // A serial-arrived session must not bridge its own
                // arrival port back into itself, so exclude only that
                // port — every other port stays selectable.
                let own_port = self.is_own_arrival_port(id);
                // On a slave, a console port is dedicated to the master
                // (it runs the registration loop, not the local console
                // bridge), so it isn't selectable here — picking it would
                // hang waiting for a local bridge nothing services (§9 #13).
                let relayed_to_master = cfg.gateway_role == "slave"
                    && port.enabled
                    && port.mode == "console"
                    && !port.port.is_empty();
                let console_ok = !own_port
                    && !relayed_to_master
                    && crate::serial::check_console_bridge_eligible(&cfg, id).is_ok();
                // A modem-mode port is selectable when peer-dial is enabled:
                // picking it rings the port (the device answers per its own
                // AT rules), just like `ATD <Port>@<IP>`.
                let peer_ok = cfg.allow_peer_dial
                    && !own_port
                    && !relayed_to_master
                    && port.enabled
                    && port.mode != "console"
                    && !port.port.is_empty();
                let ok = console_ok || peer_ok;
                any_eligible |= ok;
                // Two-line per-port entry so the device path + baud
                // never overflow the 40-col PETSCII budget.  Line 1 is
                // the role label; line 2 (when there is a device set)
                // shows the path/baud indented to align under the
                // role label.  ASCII-only — no em-dash so .len() and
                // display width agree.
                let label = format!("[{}] Port {}", id.label(), id.label());
                let role = if own_port {
                    "Your port"
                } else if relayed_to_master {
                    "-> master"
                } else if !port.enabled {
                    "Disabled"
                } else if port.mode != "console" {
                    // Modem port: selectable (rings) only when peer-dial is on.
                    if peer_ok { "Modem (rings)" } else { "Modem mode" }
                } else if port.port.is_empty() {
                    "No device"
                } else {
                    "Console mode"
                };
                let role_colored = if own_port {
                    self.amber(role)
                } else if relayed_to_master {
                    self.dim(role)
                } else if !port.enabled {
                    self.red(role)
                } else if port.mode != "console" {
                    if peer_ok { self.green(role) } else { self.amber(role) }
                } else if port.port.is_empty() {
                    self.red(role)
                } else {
                    self.green(role)
                };
                self.send_line(&format!(
                    "  {} - {}",
                    if ok { self.cyan(&label) } else { self.dim(&label) },
                    role_colored
                ))
                .await?;
                if !port.port.is_empty() {
                    // Indent under "[A] " on line 1 (6 spaces).  Path
                    // truncated so the worst-case line stays under
                    // 40 cols: 6 indent + path(<=23) + " " + baud(<=6) = 36.
                    self.send_line(&format!(
                        "      {} {}",
                        self.amber(&truncate_to_width(&port.port, 23)),
                        port.baud
                    ))
                    .await?;
                }
            }
            self.send_line("").await?;

            // Registered remote console ports (§9 #12), capped.  Each is a
            // single line keyed by a digit; the captured Vec maps the digit
            // back to its (slave IP, label) on selection.
            let remotes = crate::relay::list_remote_ports();
            let shown: Vec<(std::net::IpAddr, String)> =
                remotes.iter().take(REMOTE_PORT_DISPLAY_CAP).cloned().collect();
            any_eligible |= !shown.is_empty();
            if !remotes.is_empty() {
                self.send_line(&format!("  {}", self.dim("Remote (slave) ports:")))
                    .await?;
                for (i, (ip, label)) in shown.iter().enumerate() {
                    // No spaces around '@' — the entry is exactly the string the
                    // user types to dial it (`ATDT <Port>@<ip>`).
                    let entry = truncate_to_width(&format!("{}@{}", label, ip), 30);
                    self.send_line(&format!(
                        "  {} {}",
                        self.cyan(&format!("[{}]", i + 1)),
                        self.green(&entry)
                    ))
                    .await?;
                }
                if remotes.len() > shown.len() {
                    self.send_line(&format!(
                        "  {}",
                        self.dim(&format!("+{} more not shown", remotes.len() - shown.len()))
                    ))
                    .await?;
                }
                self.send_line("").await?;
            }

            if !any_eligible {
                self.send_line(&format!(
                    "  {}",
                    self.red("No port is available to bridge.")
                ))
                .await?;
                self.send_line(&format!(
                    "  {}",
                    self.dim("Enable console mode via Config > M.")
                ))
                .await?;
                self.send_line("").await?;
            }
            if any_eligible {
                // A picked port is a transparent, direct link (no host echoing
                // keystrokes back), so the caller needs their terminal's local
                // echo to see what they type. 38 cols — fits the PETSCII width.
                self.send_line(&format!(
                    "  {}",
                    self.dim("Tip: enable local echo to see typing")
                ))
                .await?;
            }
            self.send_line(&format!("  {}", self.action_prompt("Q", "Back")))
                .await?;
            let prompt = format!("{}> ", self.cyan("ethernet/gateway"));
            self.send(&prompt).await?;
            self.flush().await?;

            let input = match self.get_menu_input(false).await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(None),
            };
            match input.as_str() {
                "a" => return Ok(Some(GatewayPick::Local(SerialPortId::A))),
                "b" => return Ok(Some(GatewayPick::Local(SerialPortId::B))),
                "q" => return Ok(None),
                s => {
                    // A digit selects a remote port from the shown list.
                    if let Ok(n) = s.parse::<usize>()
                        && n >= 1
                        && n <= shown.len()
                    {
                        let (ip, label) = shown[n - 1].clone();
                        return Ok(Some(GatewayPick::Remote { ip, label }));
                    }
                    self.show_error("Press A, B, a number, or Q.").await?;
                    continue;
                }
            }
            // Final eligibility for a LOCAL pick is re-checked by the
            // caller, so a dim port still returns (the user gets a
            // specific reason rather than a generic rejection).
        }
    }

    /// Bridge the telnet session directly to one of the configured
    /// serial ports.  Always presents an A/B picker first; the chosen
    /// port must be `enabled = true` with `mode = "console"`.
    ///
    /// The escape sequence is two consecutive ESC presses (PETSCII `<-`
    /// on Commodore terminals).  A single ESC is forwarded to the wire
    /// after one read cycle, so editors that need ESC (vi, ed) keep
    /// working as long as the user types a normal key after each ESC.
    async fn gateway_serial(&mut self) -> Result<(), std::io::Error> {
        // Always render a picker — even if only one port is eligible
        // — so the user can see both ports' status side-by-side and
        // the menu structure stays consistent regardless of config.
        match self.gateway_serial_picker().await? {
            None => Ok(()),
            Some(GatewayPick::Local(id)) => self.gateway_serial_local(id).await,
            Some(GatewayPick::Remote { ip, label }) => {
                self.gateway_serial_remote(ip, label).await
            }
        }
    }

    /// Bridge to a local serial port (the original Serial Gateway path).
    async fn gateway_serial_local(
        &mut self,
        id: crate::config::SerialPortId,
    ) -> Result<(), std::io::Error> {
        // Bridging the modem-emulator's own port back into the session
        // that arrived over that very port would loop the user's
        // terminal to itself — a footgun.  Reject *only* that case: a
        // serial-arrived user may still bridge to a different port.
        // (The picker already marks the arrival port ineligible, so this
        // is the belt-and-braces guard against a stale pick.)
        if self.is_own_arrival_port(id) {
            self.show_error_lines(&[
                "Cannot bridge a serial port to",
                "itself.  Pick a different port.",
            ])
            .await?;
            return Ok(());
        }

        let cfg = config::get_config();
        let port_cfg = cfg.port(id).clone();
        // A console-mode target connects directly; a modem-mode target is
        // rung (peer-dial) and answers per its own AT rules.  Re-validate
        // under the picked id — mode/eligibility might have changed since
        // the picker rendered (operator could have toggled it elsewhere).
        let is_console = port_cfg.mode == "console";
        if is_console {
            if let Err(e) = crate::serial::check_console_bridge_eligible(&cfg, id) {
                self.show_error_lines(&["Could not acquire serial port:", "", e.as_str()])
                    .await?;
                return Ok(());
            }
        } else if !cfg.allow_peer_dial || !port_cfg.enabled || port_cfg.port.is_empty() {
            // Modem-mode target requires the peer-dial opt-in and a live port.
            self.show_error_lines(&[
                "That port can't be dialed.",
                "",
                "Enable peer-dial (Serial Config > P)",
                "and give the modem port a device.",
            ])
            .await?;
            return Ok(());
        }

        let esc_label = match self.terminal_type {
            TerminalType::Petscii => "<-",
            _ => "ESC",
        };

        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;
        self.send_line(&format!(
            "  {}",
            self.yellow(&format!("SERIAL GATEWAY (PORT {})", id.label()))
        ))
        .await?;
        self.send_line(&sep).await?;
        self.send_line("").await?;
        // Stack the port info so a long device path
        // (e.g. /dev/ttyUSB10) can never overflow the 40-col
        // PETSCII width.
        self.send_line(&format!(
            "  Port: {}",
            self.amber(&port_cfg.port)
        ))
        .await?;
        self.send_line(&format!(
            "  Baud: {}",
            self.amber(&port_cfg.baud.to_string())
        ))
        .await?;
        self.send_line(&format!(
            "  Data: {}{}{} flow={}",
            port_cfg.databits,
            port_cfg.parity.chars().next().unwrap_or('N').to_uppercase(),
            port_cfg.stopbits,
            port_cfg.flowcontrol,
        ))
        .await?;
        self.send_line("").await?;
        self.send_line(&format!(
            "  Press {} {} to disconnect.",
            self.cyan(esc_label),
            self.cyan(esc_label)
        ))
        .await?;
        self.send_line("  Single ESC passes through on the").await?;
        self.send_line("  next keystroke.").await?;
        self.send_line("").await?;
        self.send(&format!(
            "  {} ",
            self.cyan("Connect now? (Y/N):")
        ))
        .await?;
        self.flush().await?;

        let confirm = match self.read_byte_filtered().await? {
            Some(b) => b,
            None => return Ok(()),
        };
        // Terminate the prompt line.  The user's terminal supplies
        // its own echo of `Y` (or its absence) — the gateway only
        // emits a CRLF here so subsequent output starts cleanly,
        // matching the convention used by `modem_apply_settings`.
        self.send_line("").await?;
        if confirm != b'Y' && confirm != b'y' {
            return Ok(());
        }

        // Acquire the bridge BEFORE printing "Connected." so the
        // user doesn't see a confusing "Connected." followed
        // immediately by an acquisition error.  The request returns
        // quickly when the serial-manager loop is healthy (it polls
        // the slot every 150 ms).
        self.send_line(&format!(
            "  {}",
            self.dim(if is_console { "Acquiring serial port..." } else { "Ringing port..." })
        ))
        .await?;
        self.flush().await?;
        let bridge = if is_console {
            match crate::serial::request_console_bridge(id).await {
                Ok(b) => b,
                Err(e) => {
                    self.show_error_lines(&["Could not acquire serial port:", "", e.as_str()])
                        .await?;
                    return Ok(());
                }
            }
        } else {
            // Ring the modem-mode target; it answers per its own AT rules
            // (S0 auto-answer / manual ATA).  ~30 s covers the default
            // S0=5 at the 6 s ring cadence, plus a manual answer.
            use crate::serial::PeerCallOutcome;
            match crate::serial::request_peer_call(id, std::time::Duration::from_secs(30)).await {
                Ok(b) => b,
                Err(outcome) => {
                    let why = match outcome {
                        PeerCallOutcome::Busy => "That port is busy (in a call).",
                        PeerCallOutcome::NoAnswer => "No answer.",
                        _ => "The call could not be completed.",
                    };
                    self.show_error_lines(&["Could not connect:", "", why]).await?;
                    return Ok(());
                }
            }
        };

        self.send_line(&format!(
            "  {}",
            self.green("Connected.")
        ))
        .await?;
        self.send_line("").await?;
        self.flush().await?;

        let result = self.run_serial_console_loop(bridge).await;

        self.send_line("").await?;
        self.send_line(&format!(
            "  {}",
            self.yellow("Serial bridge closed.")
        ))
        .await?;
        self.send_line("").await?;
        self.send("  Press any key to continue.").await?;
        self.flush().await?;
        let idle_timeout = std::time::Duration::from_secs(
            config::get_config().idle_timeout_secs,
        );
        if idle_timeout.is_zero() {
            let _ = self.wait_for_key().await;
        } else {
            let _ = tokio::time::timeout(idle_timeout, self.wait_for_key()).await;
        }
        result
    }

    /// Bridge to a registered **remote** console port on a slave (§9 #12).
    /// The master reaches inward: claim the slave's idle registration
    /// channel, send the one-byte activate signal so the slave starts
    /// bridging its UART, then run the same console pump against the
    /// channel.  Dropping the stream at the end closes the channel, which
    /// the slave sees as end-of-bridge (it re-registers).
    async fn gateway_serial_remote(
        &mut self,
        ip: IpAddr,
        label: String,
    ) -> Result<(), std::io::Error> {
        use tokio::io::AsyncWriteExt;

        let esc_label = match self.terminal_type {
            TerminalType::Petscii => "<-",
            _ => "ESC",
        };
        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;
        let title = truncate_to_width(&format!("REMOTE: {}@{}", label, ip), 36);
        self.send_line(&format!("  {}", self.yellow(&title))).await?;
        self.send_line(&sep).await?;
        self.send_line("").await?;
        self.send_line(&format!(
            "  Press {} {} to disconnect.",
            self.cyan(esc_label),
            self.cyan(esc_label)
        ))
        .await?;
        self.send_line("").await?;
        self.send(&format!("  {} ", self.cyan("Connect now? (Y/N):")))
            .await?;
        self.flush().await?;
        let confirm = match self.read_byte_filtered().await? {
            Some(b) => b,
            None => return Ok(()),
        };
        self.send_line("").await?;
        if confirm != b'Y' && confirm != b'y' {
            return Ok(());
        }

        // Claim the registration channel (removes it from the registry so
        // no other master user can grab the same port).
        let Some(mut stream) = crate::relay::remove_remote_port(ip, &label) else {
            self.show_error_lines(&[
                "That remote port is no longer",
                "available (slave disconnected).",
            ])
            .await?;
            return Ok(());
        };
        // Signal the slave that a user attached so it starts bridging its
        // UART (the byte is consumed by the slave, never reaches the user).
        if stream
            .write_all(&[crate::relay::RELAY_ACTIVATE_BYTE])
            .await
            .is_err()
            || stream.flush().await.is_err()
        {
            self.show_error_lines(&[
                "Remote port went away before",
                "the bridge could start.",
            ])
            .await?;
            return Ok(());
        }

        self.send_line(&format!("  {}", self.green("Connected."))).await?;
        self.send_line("").await?;
        self.flush().await?;

        let result = self.run_serial_console_loop(stream).await;

        self.send_line("").await?;
        self.send_line(&format!(
            "  {}",
            self.yellow("Remote serial bridge closed.")
        ))
        .await?;
        self.send_line("").await?;
        self.send("  Press any key to continue.").await?;
        self.flush().await?;
        let idle_timeout =
            std::time::Duration::from_secs(config::get_config().idle_timeout_secs);
        if idle_timeout.is_zero() {
            let _ = self.wait_for_key().await;
        } else {
            let _ = tokio::time::timeout(idle_timeout, self.wait_for_key()).await;
        }
        result
    }

    /// Inner pump loop for the Serial Gateway.  Reads bytes from the
    /// telnet session and writes them to the serial bridge; reads
    /// bytes from the bridge and writes them back to the session.
    /// Exits cleanly on double-ESC or when either side closes.
    async fn run_serial_console_loop(
        &mut self,
        bridge: tokio::io::DuplexStream,
    ) -> Result<(), std::io::Error> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (mut bridge_read, mut bridge_write) = tokio::io::split(bridge);

        let reader = &mut self.reader;
        let writer = &self.writer;
        let is_petscii = self.terminal_type == TerminalType::Petscii;
        let erase_char = self.erase_char;
        // Idle bound for the bridge (see gateway_ssh): disconnect a
        // half-open client so it can't pin the session's max_sessions
        // slot.  Zero disables it.
        let idle_timeout = self.idle_timeout;
        let mut last_was_esc = false;
        let esc_byte: u8 = if is_petscii { 0x5F } else { 0x1B };

        let mut bridge_buf = [0u8; 4096];

        loop {
            tokio::select! {
                _ = tokio::time::sleep(idle_timeout), if !idle_timeout.is_zero() => {
                    // Idle in both directions past the timeout window —
                    // disconnect so a half-open client can't pin the
                    // session and leak its max_sessions slot.
                    break;
                }
                event = read_gateway_event(reader) => {
                    match event {
                        Ok(GatewayInboundEvent::Data(b)) if is_esc_key(b, is_petscii) => {
                            if last_was_esc {
                                break; // double-ESC — exit bridge
                            }
                            last_was_esc = true;
                        }
                        Ok(GatewayInboundEvent::Data(b)) => {
                            if last_was_esc {
                                last_was_esc = false;
                                let e = if is_petscii {
                                    petscii_to_ascii_byte(esc_byte)
                                } else {
                                    esc_byte
                                };
                                if bridge_write.write_all(&[e]).await.is_err() {
                                    break;
                                }
                            }
                            let b = if is_petscii { petscii_to_ascii_byte(b) } else { b };
                            // Map an unusual erase byte (e.g. PETSCII
                            // 0x14) back to ASCII DEL so editors that
                            // expect 0x7F see what they expect.
                            let b = if b == erase_char && erase_char != 0x7F { 0x7F } else { b };
                            if bridge_write.write_all(&[b]).await.is_err() {
                                break;
                            }
                            if bridge_write.flush().await.is_err() {
                                break;
                            }
                        }
                        Ok(GatewayInboundEvent::NawsResize(_, _)) => {
                            // No way to tell the wire about a window
                            // resize; ignore.
                        }
                        Ok(GatewayInboundEvent::Eof) => break,
                        Err(_) => break,
                    }
                }
                n = bridge_read.read(&mut bridge_buf) => {
                    match n {
                        Ok(0) => break,
                        Ok(n) => {
                            let data = &bridge_buf[..n];
                            let mut w = writer.lock().await;
                            // Always IAC-escape on the wire to the
                            // local user — they're a real telnet peer
                            // and a literal 0xFF would be misread as
                            // IAC.
                            if write_telnet_data(&mut **w, data).await.is_err() {
                                break;
                            }
                            if w.flush().await.is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            }
        }

        let _ = bridge_write.shutdown().await;
        Ok(())
    }

    // ─── AI CHAT ────────────────────────────────────────────

    /// Lines of answer content per page (screen minus header/footer).
    const PAGE_CONTENT_LINES: usize = 14;

    async fn ai_chat(&mut self, api_key: &str) -> Result<(), std::io::Error> {
        let content_width = if self.terminal_type == TerminalType::Petscii {
            PETSCII_WIDTH - 2
        } else {
            78
        };

        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;
        self.send_line(&format!("  {}", self.yellow("AI CHAT")))
            .await?;
        self.send_line(&sep).await?;
        self.send_line("").await?;
        self.send_line(&format!(
            "  {}",
            self.dim("Type a question, or Q to exit.")
        ))
        .await?;
        self.send_line("").await?;
        self.send(&format!("  {}: ", self.cyan("Q")))
            .await?;
        self.flush().await?;

        let mut question = match self.get_line_input().await? {
            Some(s) if !s.is_empty() && !s.eq_ignore_ascii_case("q") => s,
            _ => return Ok(()),
        };

        loop {
            // Inline "Thinking..." on the current screen rather than
            // doing a full clear + banner redraw — at 1200 baud the
            // extra wipe is a visible flicker before the answer page
            // (which does its own clear) replaces it anyway.
            self.send_line(&format!("  {}...", self.dim("Thinking")))
                .await?;
            self.flush().await?;

            let key = api_key.to_string();
            let q = question.clone();
            let result = tokio::task::spawn_blocking(move || {
                crate::aichat::ask(&key, &q)
            })
            .await
            .map_err(|e| {
                std::io::Error::other(e.to_string())
            })?;

            match result {
                Ok(answer) => {
                    // Normalize CR / CRLF to LF first — `.lines()` splits on
                    // \n and \r\n but leaves a bare \r mid-string, where a
                    // prompt-injected reply could use it to overwrite the
                    // prompt on ANSI terminals.  Then strip control bytes,
                    // ESC, and IAC per line so the LLM can't smuggle cursor
                    // moves, screen wipes, or telnet commands through the
                    // chat surface.
                    let normalized = answer.replace("\r\n", "\n").replace('\r', "\n");
                    let lines: Vec<String> = normalized
                        .lines()
                        .map(crate::aichat::sanitize_for_terminal)
                        .flat_map(|line| crate::aichat::wrap_line(&line, content_width))
                        .collect();

                    match self.ai_show_answer(&question, &lines).await? {
                        Some(next_q) => question = next_q,
                        None => return Ok(()),
                    }
                }
                Err(e) => {
                    let max_w = if self.terminal_type == TerminalType::Petscii {
                        30
                    } else {
                        50
                    };
                    self.show_error(&truncate_to_width(&e, max_w)).await?;
                    return Ok(());
                }
            }
        }
    }

    /// Display a paginated AI answer. Returns `Some(question)` if the user
    /// typed a new question, or `None` to exit.
    async fn ai_show_answer(
        &mut self,
        question: &str,
        lines: &[String],
    ) -> Result<Option<String>, std::io::Error> {
        let page_h = Self::PAGE_CONTENT_LINES;
        let content_max = if self.terminal_type == TerminalType::Petscii {
            PETSCII_WIDTH - 2
        } else {
            78
        };
        let mut scroll = 0usize;

        loop {
            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;

            let max_q = if self.terminal_type == TerminalType::Petscii {
                34
            } else {
                52
            };
            let q_display = truncate_to_width(question, max_q);
            self.send_line(&format!(
                "  {}",
                self.yellow(&format!("Q: {}", q_display))
            ))
            .await?;
            self.send_line(&sep).await?;

            let total = lines.len();
            let end = (scroll + page_h).min(total);
            let page_lines = &lines[scroll..end];
            for line in page_lines {
                let safe = truncate_to_width(line, content_max);
                self.send_line(&format!("  {}", safe)).await?;
            }
            for _ in (end - scroll)..page_h {
                self.send_line("").await?;
            }

            let has_prev = scroll > 0;
            let has_next = end < total;
            self.send_line(&format!(
                "  {}",
                self.dim(&format!("({}-{} of {})", scroll + 1, end, total))
            ))
            .await?;
            let mut parts = Vec::new();
            if has_prev {
                parts.push(self.action_prompt("P", "Pv"));
            }
            if has_next {
                parts.push(self.action_prompt("N", "Nx"));
            }
            parts.push(self.action_prompt("Q", "Done"));
            parts.push(self.action_prompt("H", "Help"));
            self.send_line(&format!("  {}", parts.join(" ")))
                .await?;
            self.send(&format!("  {}: ", self.cyan(">")))
                .await?;
            self.flush().await?;

            // Read a full line before acting.  A lone command letter
            // (Q/N/P/H by itself, then Enter) navigates; anything longer
            // is sent to the AI as a new question — so a follow-up that
            // merely starts with a command letter (e.g. "Quantum...") is
            // no longer swallowed by the menu.  ESC / disconnect → None.
            let input = match self.get_line_input().await? {
                Some(s) => s,
                None => return Ok(None),
            };
            if input.is_empty() {
                continue;
            }
            // Only a one-character line CAN be a command, and only when
            // it would actually do something — `n` on the last page or
            // `p` on the first page falls through to the question path
            // instead of silently no-op'ing.  Q and H always act.
            let cmd = if input.chars().count() == 1 {
                let c = input.chars().next().unwrap().to_ascii_lowercase();
                match c {
                    'q' | 'h' => c,
                    'n' if has_next => c,
                    'p' if has_prev => c,
                    _ => '\0',
                }
            } else {
                '\0'
            };

            match cmd {
                'n' => { if has_next { scroll += page_h; } }
                'p' => { if has_prev { scroll = scroll.saturating_sub(page_h); } }
                'q' => { return Ok(None); }
                'h' => {
                    self.show_help_page("AI CHAT HELP", Self::ai_chat_help_lines())
                        .await?;
                }
                _ => {
                    // Not a navigation command — send the whole line to
                    // the AI as a new question.
                    return Ok(Some(input));
                }
            }
        }
    }


    // ─── MODEM EMULATOR ──────────────────────────────────────

    // ─── Dialup Mapping ────────────────────────────────────

    async fn dialup_mapping(&mut self) -> Result<(), std::io::Error> {
        loop {
            let entries = config::load_dialup_mappings();

            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            self.send_line(&format!("  {}", self.yellow("DIALUP MAPPING")))
                .await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;

            // Built-in gateway entry (not deletable)
            self.send_line(&format!(
                "     {} = {}",
                self.cyan("1001000"),
                self.amber("ethernet-gateway")
            ))
            .await?;

            if entries.is_empty() {
                self.send_line("").await?;
                self.send_line("  No other mappings defined.").await?;
            } else {
                // Show up to 9 user entries to fit the screen
                let max_show = 9;
                for (i, entry) in entries.iter().take(max_show).enumerate() {
                    let num_col = self.cyan(&entry.number);
                    let target = format!("{}:{}", entry.host, entry.port);
                    let line = format!(
                        "  {}. {} = {}",
                        i + 1,
                        num_col,
                        self.amber(&target)
                    );
                    self.send_line(&line).await?;
                }
                if entries.len() > max_show {
                    self.send_line(&format!(
                        "  ... and {} more",
                        entries.len() - max_show
                    ))
                    .await?;
                }
            }
            self.send_line("").await?;

            self.send_line(&format!(
                "  {}  Add mapping",
                self.cyan("A")
            ))
            .await?;
            if !entries.is_empty() {
                self.send_line(&format!(
                    "  {}  Delete mapping",
                    self.cyan("D")
                ))
                .await?;
            }
            self.send_line("").await?;
            self.send_line(&format!(
                "  {}  {}",
                self.action_prompt("Q", "Back"),
                self.action_prompt("H", "Help")
            ))
            .await?;

            let prompt = format!("{}> ", self.cyan("ethernet/dialup"));
            self.send(&prompt).await?;
            self.flush().await?;

            let input = match self.get_menu_input(false).await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(()),
            };

            match input.as_str() {
                "a" => {
                    self.dialup_add_entry().await?;
                }
                "d" if !entries.is_empty() => {
                    self.dialup_delete_entry(&entries).await?;
                }
                "h" => {
                    self.show_help_page("DIALUP MAPPING HELP", Self::dialup_help_lines())
                        .await?;
                }
                "q" => return Ok(()),
                _ => {
                    if entries.is_empty() {
                        self.show_error("Press A, H, or Q.").await?;
                    } else {
                        self.show_error("Press A, D, H, or Q.").await?;
                    }
                }
            }
        }
    }

    async fn dialup_add_entry(&mut self) -> Result<(), std::io::Error> {
        self.send_line("").await?;

        self.send(&format!("  {} ", self.cyan("Phone number:")))
            .await?;
        self.flush().await?;
        let number = match self.get_line_input().await? {
            Some(s) if !s.is_empty() => s,
            _ => return Ok(()),
        };

        // Validate: must contain at least one digit
        if !number.chars().any(|c| c.is_ascii_digit()) {
            self.show_error("Number must contain digits.").await?;
            return Ok(());
        }

        self.send(&format!("  {} ", self.cyan("Host:")))
            .await?;
        self.flush().await?;
        let host = match self.get_line_input().await? {
            Some(s) if !s.is_empty() => s,
            _ => return Ok(()),
        };

        self.send(&format!("  {} ", self.cyan("Port (23):")))
            .await?;
        self.flush().await?;
        let port: u16 = match self.get_line_input().await? {
            Some(s) if s.is_empty() => 23,
            Some(s) => match s.parse::<u16>() {
                Ok(p) if p > 0 => p,
                _ => {
                    self.show_error("Invalid port number.").await?;
                    return Ok(());
                }
            },
            None => return Ok(()),
        };

        let mut entries = config::load_dialup_mappings();

        // Remove any existing entry with the same normalized number
        let new_norm = config::normalize_phone_number(&number);
        entries.retain(|e| config::normalize_phone_number(&e.number) != new_norm);

        entries.push(config::DialupEntry {
            number,
            host,
            port,
        });
        config::save_dialup_mappings(&entries);

        self.send_line("").await?;
        self.send_line("  Mapping saved.").await?;
        self.send_line("").await?;
        self.send("  Press any key to continue.").await?;
        self.flush().await?;
        self.wait_for_key().await?;
        Ok(())
    }

    async fn dialup_delete_entry(
        &mut self,
        entries: &[config::DialupEntry],
    ) -> Result<(), std::io::Error> {
        self.send_line("").await?;
        self.send(&format!(
            "  {} ",
            self.cyan("Entry # to delete:")
        ))
        .await?;
        self.flush().await?;

        let input = match self.get_line_input().await? {
            Some(s) if !s.is_empty() => s,
            _ => return Ok(()),
        };

        let idx: usize = match input.parse::<usize>() {
            Ok(n) if n >= 1 && n <= entries.len() => n - 1,
            _ => {
                self.show_error("Invalid entry number.").await?;
                return Ok(());
            }
        };

        let mut entries = entries.to_vec();
        let removed = entries.remove(idx);
        config::save_dialup_mappings(&entries);
        self.send_line(&format!(
            "  Removed: {} = {}:{}",
            removed.number, removed.host, removed.port
        ))
        .await?;
        self.send_line("").await?;
        self.send("  Press any key to continue.").await?;
        self.flush().await?;
        self.wait_for_key().await?;
        Ok(())
    }

    // ─── Modem settings ───────────────────────────────────

    /// Render the Serial Configuration submenu (the new entry point
    /// from Configuration → M).  Lists both ports with their status
    /// and lets the user pick one to drop into `modem_settings`.
    async fn serial_configuration_menu(&mut self) -> Result<(), std::io::Error> {
        use crate::config::{SerialPortId, SERIAL_PORT_IDS};

        loop {
            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            self.send_line(&format!("  {}", self.yellow("SERIAL CONFIGURATION")))
                .await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;

            let cfg = config::get_config();
            for id in SERIAL_PORT_IDS {
                let port = cfg.port(id);
                // Two-line per-port entry so the role + device path +
                // baud never overflow the 40-col PETSCII budget.  Line
                // 1: role label; line 2 (when configured): path + baud.
                let label = format!("[{}] Port {}", id.label(), id.label());
                let role_colored = if !port.enabled {
                    self.red("Disabled")
                } else if port.mode == "console" {
                    self.green("Console mode")
                } else {
                    self.amber("Modem mode")
                };
                self.send_line(&format!(
                    "  {} - {}",
                    self.cyan(&label),
                    role_colored
                ))
                .await?;
                if !port.port.is_empty() {
                    self.send_line(&format!(
                        "      {} {}",
                        self.amber(&truncate_to_width(&port.port, 23)),
                        port.baud
                    ))
                    .await?;
                }
            }
            self.send_line("").await?;
            let dbg_state = if cfg.gateway_debug {
                self.green("ON")
            } else {
                self.red("OFF")
            };
            self.send_line(&format!(
                "  {} - Gateway debug trace: {}",
                self.cyan("[D]"),
                dbg_state
            ))
            .await?;
            let peer_state = if cfg.allow_peer_dial {
                self.green("ON")
            } else {
                self.red("OFF")
            };
            self.send_line(&format!(
                "  {} - Peer-dial (Port@IP): {}",
                self.cyan("[P]"),
                peer_state
            ))
            .await?;
            self.send_line("").await?;
            self.send_line(&format!(
                "  {}  {}  {}  {}",
                self.action_prompt("D", "Debug"),
                self.action_prompt("P", "Peer-dial"),
                self.action_prompt("Q", "Back"),
                self.action_prompt("H", "Help")
            ))
            .await?;
            let prompt = format!("{}> ", self.cyan("ethernet/serial"));
            self.send(&prompt).await?;
            self.flush().await?;

            let input = match self.get_menu_input(false).await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(()),
            };
            match input.as_str() {
                "a" => self.modem_settings(SerialPortId::A).await?,
                "b" => self.modem_settings(SerialPortId::B).await?,
                "d" => {
                    let v = (!cfg.gateway_debug).to_string();
                    tokio::task::spawn_blocking(move || {
                        config::update_config_value("gateway_debug", &v);
                    })
                    .await
                    .ok();
                }
                "p" => {
                    let v = (!cfg.allow_peer_dial).to_string();
                    tokio::task::spawn_blocking(move || {
                        config::update_config_value("allow_peer_dial", &v);
                    })
                    .await
                    .ok();
                }
                "h" => self.serial_configuration_help().await?,
                "q" => return Ok(()),
                _ => {
                    self.show_error("Press A, B, D, P, H, or Q.").await?;
                }
            }
        }
    }

    async fn serial_configuration_help(&mut self) -> Result<(), std::io::Error> {
        self.show_help_page("SERIAL CONFIGURATION HELP", Self::serial_config_help_lines())
            .await
    }

    /// Serial-configuration submenu help (single width — fits 40 so it serves
    /// PETSCII too).  Associated fn so a unit test asserts it fits 40 cols.
    fn serial_config_help_lines() -> &'static [&'static str] {
        &[
            "  Each serial port has its own enabled",
            "  flag, role (Modem Emulator or Serial",
            "  Console), device path, baud rate, and",
            "  AT/S-register state.",
            "",
            "  Pick A or B to configure that port.",
            "  Inside, press T to toggle between",
            "  Modem and Console mode for the port",
            "  you're editing.",
            "",
            "  Press D to toggle the gateway debug",
            "  trace (byte-level logging of SSH/",
            "  Telnet gateway sessions). Takes effect",
            "  on the next gateway session.",
            "",
            "  Press P to toggle peer-dial: a modem",
            "  port may dial another port directly",
            "  (ATD Port@IP) or ring a modem port",
            "  picked from the Serial Gateway menu,",
            "  instead of the gateway menu.",
        ]
    }

    async fn modem_settings(
        &mut self,
        id: crate::config::SerialPortId,
    ) -> Result<(), std::io::Error> {
        // Snapshot current config so we can detect changes and revert if needed.
        let original_cfg = config::get_config();

        loop {
            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;

            let cfg = config::get_config();
            let port = cfg.port(id).clone();
            let console_mode = port.mode == "console";
            let title = if console_mode {
                format!("PORT {} - SERIAL CONSOLE", id.label())
            } else {
                format!("PORT {} - MODEM EMULATOR", id.label())
            };
            self.send_line(&format!("  {}", self.yellow(&title)))
                .await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;

            let status = if port.enabled {
                self.green("ENABLED")
            } else {
                self.red("Disabled")
            };
            let mode_label = if console_mode {
                self.green("Console")
            } else {
                self.amber("Modem")
            };
            // Status + Mode share one line to keep the menu under
            // the 22-row PETSCII budget when ATD + Dialup + Ring
            // are all visible.
            self.send_line(&format!(
                "  Status: {}  Mode: {}",
                status, mode_label
            ))
            .await?;
            let port_display = if port.port.is_empty() {
                "(not set)".to_string()
            } else {
                port.port.clone()
            };
            self.send_line(&format!(
                "  Port:   {}",
                self.amber(&port_display)
            ))
            .await?;
            self.send_line(&format!(
                "  Baud:   {}",
                self.amber(&port.baud.to_string())
            ))
            .await?;
            let data_str = format!(
                "{}-{}-{}",
                port.databits,
                port.parity.chars().next().unwrap_or('N').to_uppercase(),
                port.stopbits
            );
            // Drive-carrier (DCD proxy) is a modem-emulator feature, so —
            // like PETSCII — it shares an existing row rather than spending
            // one of the 22-row PETSCII budget.  The Data value is stable-
            // width (X-Y-Z), so appending the carrier state here always fits
            // 40 columns.
            if console_mode {
                self.send_line(&format!("  Data:   {}", self.amber(&data_str)))
                    .await?;
            } else {
                let carrier_state = if port.drive_carrier { "on" } else { "off" };
                self.send_line(&format!(
                    "  Data:   {}   Carrier: {}",
                    self.amber(&data_str),
                    self.amber(carrier_state)
                ))
                .await?;
            }
            // PETSCII xlate is a modem-emulator feature (direct-TCP dials
            // only), so it rides on the Flow line in modem mode rather
            // than spending a row of the 22-row PETSCII budget.
            if console_mode {
                self.send_line(&format!(
                    "  Flow:   {}",
                    self.amber(&port.flowcontrol)
                ))
                .await?;
            } else {
                let petscii_state = if port.petscii_translate { "on" } else { "off" };
                self.send_line(&format!(
                    "  Flow:   {}   PETSCII: {}",
                    self.amber(&port.flowcontrol),
                    self.amber(petscii_state)
                ))
                .await?;
            }
            if port.enabled && !console_mode {
                self.send_line(&format!(
                    "  {}",
                    self.amber("ATD ETHERNET-GATEWAY")
                ))
                .await?;
            }
            self.send_line("").await?;
            self.send_line(&format!(
                "  {}  Toggle enabled/disabled",
                self.cyan("E")
            ))
            .await?;
            // T moved here from the Configuration menu so each port's
            // mode toggle lives next to the rest of its settings.
            // Hidden only when the caller is dialed in on THIS port —
            // flipping their own port to console mid-session would
            // tear down their connection before they could confirm.
            // Hiding T for the OTHER port would be over-conservative:
            // restarting Port B from a Port A serial session is safe.
            let toggling_own_port = self.is_serial && self.serial_port_id == Some(id);
            if !toggling_own_port {
                self.send_line(&format!(
                    "  {}  Toggle Modem/Console mode",
                    self.cyan("T")
                ))
                .await?;
            }
            self.send_line(&format!(
                "  {}  Select serial port",
                self.cyan("S")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Set baud rate",
                self.cyan("B")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Set data/parity/stop",
                self.cyan("P")
            ))
            .await?;
            // X (PETSCII xlate) shares this row in modem mode to keep the
            // menu within the 22-row PETSCII budget.
            if console_mode {
                self.send_line(&format!(
                    "  {}  Set flow control",
                    self.cyan("F")
                ))
                .await?;
            } else {
                self.send_line(&format!(
                    "  {}  Set flow control   {}  PETSCII",
                    self.cyan("F"),
                    self.cyan("X")
                ))
                .await?;
            }
            // Dialup mapping and ring emulator are modem-emulator
            // features only — they don't apply to a raw console bridge.
            if !console_mode {
                self.send_line(&format!(
                    "  {}  Dialup Mapping   {}  Carrier",
                    self.cyan("D"),
                    self.cyan("C")
                ))
                .await?;
                // Hide Ring on the port the caller is dialed in on
                // (ringing yourself isn't useful) but allow it on the
                // OTHER port — a Port-A serial session can ring Port B's
                // wire if there's separate hardware listening over there.
                let ringing_own_port = self.is_serial && self.serial_port_id == Some(id);
                if !ringing_own_port {
                    self.send_line(&format!(
                        "  {}  Ring emulator",
                        self.cyan("I")
                    ))
                    .await?;
                }
            }
            self.send_line("").await?;
            self.send_line(&format!(
                "  {}  {}",
                self.action_prompt("Q", "Back"),
                self.action_prompt("H", "Help")
            ))
            .await?;

            let prompt_label = if console_mode {
                format!("ethernet/console-{}", id.label().to_ascii_lowercase())
            } else {
                format!("ethernet/modem-{}", id.label().to_ascii_lowercase())
            };
            let prompt = format!("{}> ", self.cyan(&prompt_label));
            self.send(&prompt).await?;
            self.flush().await?;

            let input = match self.get_menu_input(false).await? {
                Some(s) if !s.is_empty() => s,
                _ => {
                    self.modem_apply_settings(id, &original_cfg).await?;
                    return Ok(());
                }
            };

            match input.as_str() {
                "e" => {
                    let new_val = if port.enabled { "false" } else { "true" };
                    let v = new_val.to_string();
                    let key = config::serial_key(id, "enabled");
                    tokio::task::spawn_blocking(move || {
                        config::update_config_value(&key, &v);
                    })
                    .await
                    .ok();
                }
                "t" if !(self.is_serial && self.serial_port_id == Some(id)) => {
                    self.toggle_serial_mode(id).await?;
                }
                "s" => {
                    self.modem_select_port(id).await?;
                }
                "b" => {
                    self.modem_set_baud(id).await?;
                }
                "p" => {
                    self.modem_set_data_params(id).await?;
                }
                "f" => {
                    self.modem_set_flow(id).await?;
                }
                "x" if !console_mode => {
                    // Toggle PETSCII translation and persist immediately —
                    // it's a sticky per-port preference, the same field the
                    // AT+PETSCII command and the web/GUI surfaces write.
                    let new_val = if port.petscii_translate { "false" } else { "true" };
                    let v = new_val.to_string();
                    let key = config::serial_key(id, "petscii_translate");
                    tokio::task::spawn_blocking(move || {
                        config::update_config_value(&key, &v);
                    })
                    .await
                    .ok();
                }
                "c" if !console_mode => {
                    // Toggle the drive-carrier (DCD proxy) opt-in and
                    // persist immediately — same per-port field the web and
                    // GUI surfaces write.  Takes effect on the next port
                    // restart (modem_apply_settings triggers one via the
                    // diff below).
                    let new_val = if port.drive_carrier { "false" } else { "true" };
                    let v = new_val.to_string();
                    let key = config::serial_key(id, "drive_carrier");
                    tokio::task::spawn_blocking(move || {
                        config::update_config_value(&key, &v);
                    })
                    .await
                    .ok();
                }
                "d" if !console_mode => {
                    self.dialup_mapping().await?;
                }
                "i" if !(console_mode
                    || self.is_serial && self.serial_port_id == Some(id)) =>
                {
                    self.modem_ring_emulator(id).await?;
                }
                "h" => {
                    self.modem_show_help(id).await?;
                }
                "q" => {
                    self.modem_apply_settings(id, &original_cfg).await?;
                    return Ok(());
                }
                _ => {
                    // T and I are hidden only when the caller is
                    // dialed in on THIS port (toggling/ringing your
                    // own port isn't useful).  Any other combination
                    // shows the full menu.
                    let on_own_port = self.is_serial && self.serial_port_id == Some(id);
                    let msg = match (console_mode, on_own_port) {
                        (true, true) => "Press E, S, B, P, F, H, or Q.",
                        (true, false) => "Press E, T, S, B, P, F, H, or Q.",
                        (false, true) => "Press E, S, B, P, C, D, F, X, H, or Q.",
                        (false, false) => "Press E, T, S, B, P, C, D, F, X, I, H, or Q.",
                    };
                    self.show_error(msg).await?;
                }
            }
        }
    }

    /// Apply modem settings changes for a specific port.  For serial
    /// users, ask for acknowledgement and revert if no response within
    /// 60 seconds.  Diff is per-port — saving Port A's changes leaves
    /// any in-flight Port B activity alone.
    async fn modem_apply_settings(
        &mut self,
        id: crate::config::SerialPortId,
        original_cfg: &config::Config,
    ) -> Result<(), std::io::Error> {
        let new_cfg = config::get_config();
        let new_port = new_cfg.port(id);
        let old_port = original_cfg.port(id);
        let changed = new_port.enabled != old_port.enabled
            || new_port.mode != old_port.mode
            || new_port.port != old_port.port
            || new_port.baud != old_port.baud
            || new_port.databits != old_port.databits
            || new_port.parity != old_port.parity
            || new_port.stopbits != old_port.stopbits
            || new_port.flowcontrol != old_port.flowcontrol
            || new_port.petscii_translate != old_port.petscii_translate
            || new_port.drive_carrier != old_port.drive_carrier;

        if !changed {
            return Ok(());
        }

        // The warn-+-revert flow is only meaningful when the caller's
        // own modem session is the one being reconfigured: changing
        // baud / framing / port-device underneath them would tear
        // down their connection mid-edit, so we ask for explicit
        // Y+Enter confirmation against a 60-s deadline.  When a
        // serial-side caller is editing the OTHER port (e.g. dialed
        // in on Port A, editing Port B), the restart only affects the
        // other manager and the caller's connection is unaffected —
        // skip the warn-+-revert and just apply.
        let editing_own_port = self.is_serial && self.serial_port_id == Some(id);
        if !editing_own_port {
            crate::serial::restart_serial(id);
            return Ok(());
        }

        // Serial user editing their own port: warn before applying new
        // settings, then require Y+Enter acknowledgement.  Random
        // bytes from a baud mismatch must not count as confirmation.
        // I/O errors during the prompt are non-fatal — we still need
        // to reach the revert logic.
        let _ = self.send_line("").await;
        let _ = self.send_line(&format!(
            "  {}",
            self.yellow("New settings will be applied.")
        )).await;
        let _ = self.send_line(&format!(
            "  {}",
            self.yellow("You have 60 seconds to adjust")
        )).await;
        let _ = self.send_line(&format!(
            "  {}",
            self.yellow("your terminal and type Y then")
        )).await;
        let _ = self.send_line(&format!(
            "  {}",
            self.yellow("Enter, or settings will revert.")
        )).await;
        let _ = self.send_line("").await;
        let _ = self.flush().await;

        // Apply the new serial settings now.
        crate::serial::restart_serial(id);

        let deadline = tokio::time::Instant::now()
            + tokio::time::Duration::from_secs(60);
        let mut next_remind = tokio::time::Instant::now()
            + tokio::time::Duration::from_secs(5);
        let mut got_y = false;

        loop {
            let wait_until = std::cmp::min(next_remind, deadline);
            let remaining = wait_until.saturating_duration_since(tokio::time::Instant::now());

            match tokio::time::timeout(remaining, self.read_byte_filtered()).await {
                Ok(Ok(Some(byte))) => {
                    if got_y {
                        if byte == b'\r' || byte == b'\n' {
                            // Y + Enter — confirmed
                            let _ = self.send_line("").await;
                            let _ = self.send_line(&format!(
                                "  {}",
                                self.green("Settings confirmed.")
                            )).await;
                            let _ = self.send_line("").await;
                            let _ = self.send("  Press any key to continue.").await;
                            let _ = self.flush().await;
                            let _ = self.wait_for_key().await;
                            return Ok(());
                        }
                        // Y followed by non-Enter — noise, reset
                        got_y = false;
                    } else if byte == b'Y' || byte == b'y' {
                        got_y = true;
                    }
                    // Ignore other bytes (likely noise from baud mismatch)
                }
                Ok(Ok(None)) | Ok(Err(_)) => {
                    // Connection lost — revert
                    break;
                }
                Err(_) => {
                    // Timeout interval
                    if tokio::time::Instant::now() >= deadline {
                        break;
                    }
                    let secs_left = deadline
                        .saturating_duration_since(tokio::time::Instant::now())
                        .as_secs();
                    let _ = self.send_line(&format!(
                        "  Type Y+Enter to confirm. ({}s left)",
                        secs_left
                    )).await;
                    let _ = self.flush().await;
                    next_remind += tokio::time::Duration::from_secs(5);
                }
            }
        }

        // No acknowledgement — revert
        let _ = self.send_line("").await;
        let _ = self.send_line(&format!(
            "  {}",
            self.red("No response. Reverting settings.")
        )).await;
        let _ = self.flush().await;

        Self::revert_serial_config(id, original_cfg).await;
        crate::serial::restart_serial(id);
        Ok(())
    }

    /// Toggle one port's mode between "modem" and "console".  Refuses
    /// the toggle when the caller is dialed in over THIS PORT'S modem
    /// — switching that port to console mode would tear down their
    /// own connection before they could acknowledge, and the 60 s
    /// Y+Enter recovery in `modem_apply_settings` cannot be reached
    /// once the modem session is gone.  A serial-side caller toggling
    /// the OTHER port's mode is fine — that restart doesn't affect
    /// their connection.  Console-mode sessions are raw passthroughs
    /// that don't run TelnetSession, so they never reach this code.
    async fn toggle_serial_mode(
        &mut self,
        id: crate::config::SerialPortId,
    ) -> Result<(), std::io::Error> {
        if self.is_serial && self.serial_port_id == Some(id) {
            self.show_error_lines(&[
                "Cannot toggle THIS port's mode",
                "from a modem-side session on it.",
                "Switching to Console would drop",
                "this connection before it could",
                "confirm.",
                "",
                "Connect via telnet, SSH, or the",
                "system console and press T from",
                "the per-port settings menu there.",
            ])
            .await?;
            return Ok(());
        }

        let original_cfg = config::get_config();
        let new_mode = if original_cfg.port(id).mode == "console" {
            "modem"
        } else {
            "console"
        };
        let v = new_mode.to_string();
        let key = config::serial_key(id, "mode");
        tokio::task::spawn_blocking(move || {
            config::update_config_value(&key, &v);
        })
        .await
        .ok();
        self.modem_apply_settings(id, &original_cfg).await
    }

    /// Revert one port's config to a previous snapshot using a single
    /// batch write.  Port-scoped — never touches the other port.
    async fn revert_serial_config(
        id: crate::config::SerialPortId,
        cfg: &config::Config,
    ) {
        let port = cfg.port(id).clone();
        let _ = tokio::task::spawn_blocking(move || {
            let enabled_key = config::serial_key(id, "enabled");
            let mode_key = config::serial_key(id, "mode");
            let port_key = config::serial_key(id, "port");
            let baud_key = config::serial_key(id, "baud");
            let databits_key = config::serial_key(id, "databits");
            let parity_key = config::serial_key(id, "parity");
            let stopbits_key = config::serial_key(id, "stopbits");
            let flow_key = config::serial_key(id, "flowcontrol");
            let baud_str = port.baud.to_string();
            let databits_str = port.databits.to_string();
            let stopbits_str = port.stopbits.to_string();
            config::update_config_values(&[
                (enabled_key.as_str(), if port.enabled { "true" } else { "false" }),
                (mode_key.as_str(), port.mode.as_str()),
                (port_key.as_str(), port.port.as_str()),
                (baud_key.as_str(), baud_str.as_str()),
                (databits_key.as_str(), databits_str.as_str()),
                (parity_key.as_str(), port.parity.as_str()),
                (stopbits_key.as_str(), stopbits_str.as_str()),
                (flow_key.as_str(), port.flowcontrol.as_str()),
            ]);
        })
        .await;
    }

    async fn modem_select_port(
        &mut self,
        id: crate::config::SerialPortId,
    ) -> Result<(), std::io::Error> {
        let title = format!("PORT {} - DEVICE", id.label());
        loop {
            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            self.send_line(&format!("  {}", self.yellow(&title))).await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;
            self.send_line(&format!("  {}...", self.dim("Detecting ports"))).await?;
            self.flush().await?;

            let ports = tokio::task::spawn_blocking(crate::serial::list_serial_ports)
                .await
                .unwrap_or_default();

            if ports.is_empty() {
                self.clear_screen().await?;
                self.send_line(&sep).await?;
                self.send_line(&format!("  {}", self.yellow(&title))).await?;
                self.send_line(&sep).await?;
                self.send_line("").await?;
                self.send_line(&format!("  {}", self.red("No serial ports detected.")))
                    .await?;
                self.send_line("").await?;
                self.send_line(&format!(
                    "  {}  Refresh port list",
                    self.cyan("R")
                ))
                .await?;
                self.send_line(&format!(
                    "  {}  None (clear port)",
                    self.cyan("N")
                ))
                .await?;
                self.send_line("").await?;
                self.send_line(&format!("  {}", self.action_prompt("Q", "Back")))
                    .await?;
                self.send(&format!("  {} ", self.cyan("Port:"))).await?;
                self.flush().await?;

                let input = match self.get_line_input().await? {
                    Some(s) if !s.is_empty() => s,
                    _ => return Ok(()),
                };
                let port_key = config::serial_key(id, "port");
                match input.as_str() {
                    "r" => continue,
                    "n" => {
                        let k = port_key.clone();
                        tokio::task::spawn_blocking(move || {
                            config::update_config_value(&k, "");
                        })
                        .await
                        .ok();
                        return Ok(());
                    }
                    "q" | "" => return Ok(()),
                    _ => {
                        // Allow typing a port path directly even with no ports detected
                        let port_name = input;
                        let k = port_key.clone();
                        tokio::task::spawn_blocking(move || {
                            config::update_config_value(&k, &port_name);
                        })
                        .await
                        .ok();
                        return Ok(());
                    }
                }
            }

            // Redraw with port list
            self.clear_screen().await?;
            self.send_line(&sep).await?;
            self.send_line(&format!("  {}", self.yellow(&title))).await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;
            let max_w = if self.terminal_type == TerminalType::Petscii {
                30
            } else {
                50
            };
            for (i, port) in ports.iter().enumerate() {
                self.send_line(&format!(
                    "  {:>2}. {}",
                    i + 1,
                    truncate_to_width(port, max_w)
                ))
                .await?;
            }
            self.send_line("").await?;
            self.send_line(&format!(
                "  {}  Refresh port list",
                self.cyan("R")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  None (clear port)",
                self.cyan("N")
            ))
            .await?;
            self.send_line("").await?;
            self.send_line(&format!(
                "  {}",
                self.dim("Enter #, R, N, or type a path.")
            )).await?;
            self.send_line(&format!("  {}", self.action_prompt("Q", "Back"))).await?;
            self.send(&format!("  {} ", self.cyan("Port:"))).await?;
            self.flush().await?;

            let input = match self.get_line_input().await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(()),
            };

            let port_key = config::serial_key(id, "port");
            match input.as_str() {
                "r" => continue,
                "n" => {
                    let k = port_key.clone();
                    tokio::task::spawn_blocking(move || {
                        config::update_config_value(&k, "");
                    })
                    .await
                    .ok();
                    return Ok(());
                }
                "q" => return Ok(()),
                _ => {}
            }

            if let Ok(idx) = input.parse::<usize>() {
                if idx >= 1 && idx <= ports.len() {
                    let port_name = ports[idx - 1].clone();
                    let k = port_key.clone();
                    tokio::task::spawn_blocking(move || {
                        config::update_config_value(&k, &port_name);
                    })
                    .await
                    .ok();
                } else {
                    self.show_error("Invalid selection.").await?;
                    continue;
                }
            } else {
                // Allow typing a port path directly
                let port_name = input;
                let k = port_key.clone();
                tokio::task::spawn_blocking(move || {
                    config::update_config_value(&k, &port_name);
                })
                .await
                .ok();
            }
            return Ok(());
        }
    }

    async fn modem_set_baud(
        &mut self,
        id: crate::config::SerialPortId,
    ) -> Result<(), std::io::Error> {
        let bauds = [
            "300", "1200", "2400", "4800", "9600", "19200", "38400",
            "57600", "115200",
        ];
        let title = format!("PORT {} - BAUD RATE", id.label());
        loop {
            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            self.send_line(&format!("  {}", self.yellow(&title))).await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;
            for (i, b) in bauds.iter().enumerate() {
                self.send_line(&format!(
                    "  {}  {}",
                    self.cyan(&(i + 1).to_string()),
                    b
                ))
                .await?;
            }
            self.send_line("").await?;
            self.send_line(&format!("  {}", self.action_prompt("Q", "Back"))).await?;
            let prompt = format!("{}> ", self.cyan("baud"));
            self.send(&prompt).await?;
            self.flush().await?;

            let input = match self.get_menu_input(true).await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(()),
            };

            match input.as_str() {
                "q" => return Ok(()),
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" => {
                    // Safe: the match arm only accepts single ASCII digits 1-9.
                    let idx_v = (input.as_bytes()[0] - b'1') as usize;
                    let baud_str = bauds[idx_v].to_string();
                    let key = config::serial_key(id, "baud");
                    tokio::task::spawn_blocking(move || {
                        config::update_config_value(&key, &baud_str);
                    })
                    .await
                    .ok();
                    return Ok(());
                }
                _ => {
                    self.show_error("Press 1-9 or Q.").await?;
                }
            }
        }
    }

    async fn modem_set_data_params(
        &mut self,
        id: crate::config::SerialPortId,
    ) -> Result<(), std::io::Error> {
        let title = format!("PORT {} - DATA BITS", id.label());
        // Data bits
        loop {
            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            self.send_line(&format!("  {}", self.yellow(&title))).await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;
            self.send_line(&format!("  {}  5 bits", self.cyan("5"))).await?;
            self.send_line(&format!("  {}  6 bits", self.cyan("6"))).await?;
            self.send_line(&format!("  {}  7 bits", self.cyan("7"))).await?;
            self.send_line(&format!("  {}  8 bits", self.cyan("8"))).await?;
            self.send_line("").await?;
            self.send_line(&format!("  {}", self.action_prompt("Q", "Back"))).await?;
            let prompt = format!("{}> ", self.cyan("data"));
            self.send(&prompt).await?;
            self.flush().await?;

            let input = match self.get_menu_input(true).await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(()),
            };

            match input.as_str() {
                "q" => return Ok(()),
                "5" | "6" | "7" | "8" => {
                    let v = input.clone();
                    let key = config::serial_key(id, "databits");
                    tokio::task::spawn_blocking(move || {
                        config::update_config_value(&key, &v);
                    })
                    .await
                    .ok();
                    break;
                }
                _ => {
                    self.show_error("Press 5-8 or Q.").await?;
                }
            }
        }

        // Parity
        let parity_title = format!("PORT {} - PARITY", id.label());
        loop {
            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            self.send_line(&format!("  {}", self.yellow(&parity_title))).await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;
            self.send_line(&format!("  {}  None", self.cyan("1"))).await?;
            self.send_line(&format!("  {}  Odd", self.cyan("2"))).await?;
            self.send_line(&format!("  {}  Even", self.cyan("3"))).await?;
            self.send_line("").await?;
            self.send_line(&format!("  {}", self.action_prompt("Q", "Back"))).await?;
            let prompt = format!("{}> ", self.cyan("parity"));
            self.send(&prompt).await?;
            self.flush().await?;

            let input = match self.get_menu_input(true).await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(()),
            };

            let parity = match input.as_str() {
                "1" => "none",
                "2" => "odd",
                "3" => "even",
                "q" => return Ok(()),
                _ => {
                    self.show_error("Press 1-3 or Q.").await?;
                    continue;
                }
            };
            let p = parity.to_string();
            let key = config::serial_key(id, "parity");
            tokio::task::spawn_blocking(move || {
                config::update_config_value(&key, &p);
            })
            .await
            .ok();
            break;
        }

        // Stop bits
        let stop_title = format!("PORT {} - STOP BITS", id.label());
        loop {
            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            self.send_line(&format!("  {}", self.yellow(&stop_title))).await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;
            self.send_line(&format!("  {}  1 stop bit", self.cyan("1"))).await?;
            self.send_line(&format!("  {}  2 stop bits", self.cyan("2"))).await?;
            self.send_line("").await?;
            self.send_line(&format!("  {}", self.action_prompt("Q", "Back"))).await?;
            let prompt = format!("{}> ", self.cyan("stop"));
            self.send(&prompt).await?;
            self.flush().await?;

            let input = match self.get_menu_input(true).await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(()),
            };

            match input.as_str() {
                "q" => return Ok(()),
                "1" | "2" => {
                    let v = input.clone();
                    let key = config::serial_key(id, "stopbits");
                    tokio::task::spawn_blocking(move || {
                        config::update_config_value(&key, &v);
                    })
                    .await
                    .ok();
                    return Ok(());
                }
                _ => {
                    self.show_error("Press 1-2 or Q.").await?;
                }
            }
        }
    }

    async fn modem_set_flow(
        &mut self,
        id: crate::config::SerialPortId,
    ) -> Result<(), std::io::Error> {
        let title = format!("PORT {} - FLOW CONTROL", id.label());
        loop {
            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            self.send_line(&format!("  {}", self.yellow(&title))).await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;
            self.send_line(&format!("  {}  None", self.cyan("1"))).await?;
            self.send_line(&format!("  {}  Hardware (RTS/CTS)", self.cyan("2"))).await?;
            self.send_line(&format!("  {}  Software (XON/XOFF)", self.cyan("3"))).await?;
            self.send_line("").await?;
            self.send_line(&format!("  {}", self.action_prompt("Q", "Back"))).await?;
            let prompt = format!("{}> ", self.cyan("flow"));
            self.send(&prompt).await?;
            self.flush().await?;

            let input = match self.get_menu_input(true).await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(()),
            };

            let flow = match input.as_str() {
                "1" => "none",
                "2" => "hardware",
                "3" => "software",
                "q" => return Ok(()),
                _ => {
                    self.show_error("Press 1-3 or Q.").await?;
                    continue;
                }
            };
            let f = flow.to_string();
            let key = config::serial_key(id, "flowcontrol");
            tokio::task::spawn_blocking(move || {
                config::update_config_value(&key, &f);
            })
            .await
            .ok();
            return Ok(());
        }
    }

    async fn modem_ring_emulator(
        &mut self,
        id: crate::config::SerialPortId,
    ) -> Result<(), std::io::Error> {
        let cfg = config::get_config();
        let port = cfg.port(id).clone();

        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;
        self.send_line(&format!(
            "  {}",
            self.yellow(&format!("PORT {} - RING EMULATOR", id.label()))
        ))
        .await?;
        self.send_line(&sep).await?;
        self.send_line("").await?;

        // Check if serial port is enabled
        if !port.enabled || port.port.is_empty() {
            self.send_line(&format!(
                "  {}",
                self.red("Serial port is not enabled.")
            ))
            .await?;
            self.send_line("").await?;
            self.send("  Press any key to continue.").await?;
            self.flush().await?;
            self.wait_for_key().await?;
            return Ok(());
        }

        // Create progress channel
        let (tx, mut rx) = tokio::sync::mpsc::channel::<u8>(16);

        if !crate::serial::request_ring(id, tx) {
            self.send_line(&format!(
                "  {}",
                self.red("A ring is already in progress.")
            ))
            .await?;
            self.send_line("").await?;
            self.send("  Press any key to continue.").await?;
            self.flush().await?;
            self.wait_for_key().await?;
            return Ok(());
        }

        self.send_line(&format!(
            "  Calling {}...",
            self.amber(&port.port)
        ))
        .await?;
        self.send_line("").await?;
        self.send_line(&format!("  {}", self.action_prompt("Q", "Cancel")))
            .await?;
        self.flush().await?;

        // Show rings as they happen.  Q or ESC cancels (drops rx
        // which signals the serial thread to abort).  Timeout if the
        // serial thread never picks up the request.
        let reader = &mut self.reader;
        let writer = &self.writer;
        let is_petscii = self.terminal_type == TerminalType::Petscii;
        let mut answered = false;
        let mut serial_error = false;
        let timeout = tokio::time::sleep(std::time::Duration::from_secs(15));
        tokio::pin!(timeout);

        loop {
            tokio::select! {
                event = rx.recv() => {
                    match event {
                        Some(0) => {
                            // RING — reset timeout on each ring
                            timeout.as_mut().reset(tokio::time::Instant::now()
                                + std::time::Duration::from_secs(15));
                            let mut w = writer.lock().await;
                            let _ = w.write_all(b"  RING...\r\n").await;
                            let _ = w.flush().await;
                        }
                        Some(1) => {
                            // Answered
                            answered = true;
                            break;
                        }
                        Some(2) => {
                            // Serial port error
                            serial_error = true;
                            break;
                        }
                        _ => break, // channel closed
                    }
                }
                byte = read_byte_iac_filtered(reader, true) => {
                    match byte {
                        Ok(Some(b)) if is_esc_key(b, is_petscii)
                            || b == b'q' || b == b'Q' =>
                        {
                            break;
                        }
                        Ok(None) | Err(_) => break,
                        _ => {} // ignore other keys
                    }
                }
                _ = &mut timeout => {
                    serial_error = true;
                    break;
                }
            }
        }

        // Drop the receiver to signal cancellation if we broke out early,
        // and clear the slot in case the serial thread never picked it up.
        drop(rx);
        crate::serial::cancel_ring_request(id);

        self.send_line("").await?;
        if answered {
            self.send_line(&format!(
                "  {}",
                self.green("Remote machine connected.")
            ))
            .await?;
        } else if serial_error {
            self.send_line(&format!(
                "  {}",
                self.red("Serial connection failed.")
            ))
            .await?;
        } else {
            self.send_line(&format!(
                "  {}",
                self.yellow("Ring cancelled.")
            ))
            .await?;
        }
        self.send_line("").await?;
        self.send("  Press any key to continue.").await?;
        self.flush().await?;
        self.wait_for_key().await?;
        Ok(())
    }

    async fn modem_show_help(
        &mut self,
        id: crate::config::SerialPortId,
    ) -> Result<(), std::io::Error> {
        let console_mode = config::get_config().port(id).mode == "console";
        if console_mode {
            return self.console_show_help().await;
        }
        let lines = Self::modem_help_lines(self.terminal_type == TerminalType::Petscii);
        self.show_help_page("MODEM EMULATOR HELP", lines).await
    }

    /// Hayes modem-emulator help, split by terminal width.  Associated fn so a
    /// unit test asserts the REAL lines fit 40 cols (see `punter_help_lines`).
    fn modem_help_lines(petscii: bool) -> &'static [&'static str] {
        if petscii {
            &[
                "  This server emulates a Hayes-",
                "  compatible modem on this serial",
                "  port. Connect retro hardware",
                "  and use AT commands.  The other",
                "  port is configured separately.",
                "",
                "  Dialing:",
                "  ATDT ethernet-gateway",
                "    Connect to this gateway",
                "  ATDT host:port",
                "    Dial a remote telnet host",
                "  ATDL     Redial last number",
                "",
                "  Stored numbers:",
                "  AT&Zn=s  Store number in slot",
                "  ATDSn    Dial stored slot 0-3",
                "",
                "  Control:",
                "  ATH      Hang up",
                "  +++      Return to cmd mode",
                "  ATO      Return online",
                "  A/       Repeat last command",
                "",
                "  Information:",
                "  ATIn     Info 0-7 (model, ROM)",
                "  AT&V     Show settings",
                "  ATSn?    Query S-register n",
                "",
                "  Configuration:",
                "  ATXn     Result-code level 0-4",
                "  AT&Cn    DCD mode (0-1)",
                "    (DTR->DCD if opt-in on)",
                "  AT&Dn    DTR handling (0-3)",
                "  AT&Kn    Flow control (0-4)",
                "  AT+PETSCII=n  PETSCII xlate 0/1",
                "  AT&W     Save settings",
                "  ATZ      Reload saved settings",
                "  AT&F     Reset to gateway",
                "           defaults",
                "",
                "  Gateway-friendly defaults:",
                "  S7=15  (50 s Hayes; faster",
                "         failed-dial recovery)",
                "  &D0    (ignore DTR; retro",
                "         clients often don't",
                "         wire it correctly)",
                "  &K0    (no modem flow control;",
                "         port-level serial flow",
                "         is still honored)",
                "",
                "  Override any of these with the",
                "  matching AT command and AT&W.",
            ]
        } else {
            &[
                "  This server emulates a Hayes-compatible",
                "  modem on this serial port.  Connect",
                "  retro hardware (Commodore 64, CP/M,",
                "  Altair, RC2014, etc.) and drive it",
                "  with standard AT commands.",
                "",
                "  Dialing:",
                "  ATDT ethernet-gateway",
                "    Connect to this gateway's menus",
                "  ATDT host:port",
                "    Dial a remote telnet host",
                "  ATDL       Redial the last number",
                "  ATDP ...   Same as ATDT (no pulse/tone",
                "             distinction on TCP)",
                "",
                "  Stored numbers (4 slots, persistent):",
                "  AT&Zn=str  Store number/host in slot n",
                "  ATDSn      Dial stored slot 0-3",
                "  AT&V       Shows the active table",
                "",
                "  Control:",
                "  ATH        Hang up the active connection",
                "  +++        Return to command mode with",
                "             S2/S12 Hayes guard-time timing",
                "  ATO        Return to online mode",
                "  A/         Repeat the last AT command",
                "             (no CR needed)",
                "",
                "  Information queries:",
                "  ATIn       0-7: model, config, ROM sum,",
                "             ROM test, firmware, OEM, etc.",
                "  AT&V       Show every current setting",
                "  ATSn?      Query S-register n",
                "",
                "  Configuration:",
                "  ATEn       Echo off/on (E0 / E1)",
                "  ATVn       Numeric/verbose result codes",
                "  ATQn       Quiet (Q1 suppresses results)",
                "  ATXn       Result-code level 0-4 (see",
                "             README for the table)",
                "  AT&Cn      DCD: 0=always on, 1=carrier",
                "             (drives DTR->DCD when the port's drive-carrier opt-in is enabled)",
                "  AT&Dn      DTR handling 0-3",
                "  AT&Kn      Flow control 0-4",
                "  AT+PETSCII=n  PETSCII translation on direct-",
                "             TCP dials (0=off, 1=on; persists)",
                "  ATSn=v     Set S-register n to v",
                "  AT&W       Save settings to egateway.conf",
                "  ATZ        Reload saved settings",
                "  AT&F       Reset to gateway defaults",
                "",
                "  Gateway-friendly default deviations:",
                "  S7=15      Wait-for-carrier (Hayes: 50 s).",
                "             Keeps failed TCP dials snappy.",
                "  &D0        Ignore DTR (Hayes: &D2 hangs up",
                "             on DTR drop).  Retro clients",
                "             often don't drive DTR correctly,",
                "             which would cause spurious",
                "             disconnects.",
                "  &K0        No modem-level flow control",
                "             (Hayes: &K3 RTS/CTS).  Port-level",
                "             flow is still honored via this",
                "             port's serial_<x>_flowcontrol key",
                "             in egateway.conf.",
                "",
                "  Override any of these with the matching AT",
                "  command and AT&W to persist.",
                "",
                "  Commands the emulator can't meaningfully",
                "  implement on TCP (ATB, ATC, ATL, ATM,",
                "  AT&B/&G/&J/&S/&T/&Y) return OK so legacy",
                "  init strings run to completion.",
            ]
        }
    }

    async fn console_show_help(&mut self) -> Result<(), std::io::Error> {
        let lines = Self::console_help_lines(self.terminal_type == TerminalType::Petscii);
        self.show_help_page("SERIAL CONSOLE HELP", lines).await
    }

    /// Serial-console (telnet-serial bridge) help, split by terminal width.
    /// Associated fn so a unit test asserts the REAL lines fit 40 cols.
    fn console_help_lines(petscii: bool) -> &'static [&'static str] {
        if petscii {
            &[
                "  This menu configures the serial",
                "  port as a raw telnet-serial",
                "  bridge.  No AT commands, no",
                "  dialing - just byte passthrough",
                "  between the telnet session and",
                "  the connected hardware.",
                "",
                "  Settings on this menu:",
                "  E  Open or close the device",
                "  S  Pick the serial device path",
                "  B  Match the baud rate of the",
                "     attached hardware",
                "  P  Data bits, parity, stop bits",
                "  F  Flow control: none, software",
                "     (XON/XOFF), or hardware",
                "     (RTS/CTS)",
                "",
                "  Using the bridge:",
                "  Pick Serial Gateway from the",
                "  main menu to enter the bridge.",
                "  Press <- <- (PETSCII) or",
                "  ESC ESC (ANSI/ASCII) to leave.",
                "  A single ESC is forwarded so",
                "  editors like vi keep working.",
                "",
                "  Switching modes:",
                "  Press T in this menu to return",
                "  to Modem Emulator mode.  Each",
                "  port toggles independently.",
            ]
        } else {
            &[
                "  This menu configures this serial port as a",
                "  raw telnet-serial bridge.  No AT commands,",
                "  no dialing - just byte passthrough between",
                "  the telnet session and the connected",
                "  hardware.",
                "",
                "  Settings on this menu:",
                "  E  Open or close the device file",
                "  S  Pick the serial device (/dev/ttyUSB0,",
                "     COM3, etc.)",
                "  B  Match the baud rate of the attached",
                "     hardware",
                "  P  Data bits, parity, stop bits",
                "  F  Flow control: none, software (XON/XOFF),",
                "     or hardware (RTS/CTS)",
                "",
                "  Using the bridge:",
                "  Pick \"Serial Gateway\" from the main menu",
                "  to enter the bridge.  Press ESC ESC to",
                "  disconnect (a single ESC is forwarded to",
                "  the wire so editors like vi keep working).",
                "",
                "  Switching modes:",
                "  Press T in this menu to return to Modem",
                "  Emulator mode.  Each port (A, B) toggles",
                "  independently.",
            ]
        }
    }

    // ─── CONFIGURATION ──────────────────────────────────────

    /// Render the "Server addresses:" banner — the gateway's reachable
    /// IPs (capped at `SERVER_ADDR_DISPLAY_CAP`) plus a sample
    /// `ATD <ip>:<port>` dial string.  Shown at the top of the
    /// CONFIGURATION menu as a "how to reach this gateway" banner.
    /// (Relocated here off the Server Configuration screen in the
    /// master/slave work to free a row for the `M Master/Slave` entry —
    /// §4.7 of the design note.)  No-op when no addresses are detected.
    async fn render_server_address_block(&mut self) -> Result<(), std::io::Error> {
        let cfg = config::get_config();
        let addrs = get_server_addresses();
        if addrs.is_empty() {
            return Ok(());
        }
        self.send_line(&format!("  {}", self.dim("Server addresses:")))
            .await?;
        let max_w = if self.terminal_type == TerminalType::Petscii {
            36 // 40 - 4 chars indent
        } else {
            52 // 56 - 4 chars indent
        };
        for addr in addrs.iter().take(SERVER_ADDR_DISPLAY_CAP) {
            let display = truncate_to_width(addr, max_w);
            self.send_line(&format!("    {}", display)).await?;
        }
        if cfg.telnet_enabled {
            let example = format!("ATD {}:{}", addrs[0], cfg.telnet_port);
            let max_example = if self.terminal_type == TerminalType::Petscii {
                38 // 40 - 2 chars indent
            } else {
                54 // 56 - 2 chars indent
            };
            let example = truncate_to_width(&example, max_example);
            self.send_line(&format!("  {}", self.amber(&example))).await?;
        }
        Ok(())
    }

    async fn configuration(&mut self) -> Result<(), std::io::Error> {
        loop {
            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            self.send_line(&format!("  {}", self.yellow("CONFIGURATION")))
                .await?;
            self.send_line(&sep).await?;

            // "How to reach this gateway" banner — relocated here from the
            // Server Configuration screen (§4.7) so that screen has room
            // for the M Master/Slave entry.
            self.render_server_address_block().await?;
            self.send_line("").await?;

            // Per-port mode/status is shown under Serial Configuration (M),
            // so the top-level menu no longer duplicates it here.
            self.send_line(&format!(
                "  {}  Security",
                self.cyan("E")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Gateway Configuration",
                self.cyan("G")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Serial Configuration",
                self.cyan("M")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Server Configuration",
                self.cyan("S")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  File Transfer",
                self.cyan("F")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Other Settings",
                self.cyan("O")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Reset Defaults",
                self.cyan("R")
            ))
            .await?;
            self.send_line("").await?;
            self.send_line(&format!(
                "  {}  {}",
                self.action_prompt("Q", "Back"),
                self.action_prompt("H", "Help")
            ))
            .await?;

            let prompt = format!("{}> ", self.cyan("ethernet/config"));
            self.send(&prompt).await?;
            self.flush().await?;

            let input = match self.get_menu_input(false).await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(()),
            };

            match input.as_str() {
                "e" => {
                    self.security_settings().await?;
                }
                "g" => {
                    self.gateway_configuration().await?;
                }
                "m" => {
                    self.serial_configuration_menu().await?;
                }
                "o" => {
                    self.other_settings().await?;
                }
                "s" => {
                    self.server_configuration().await?;
                }
                "f" => {
                    self.file_transfer_settings().await?;
                }
                "r" => {
                    self.config_reset_defaults().await?;
                }
                "h" => {
                    let lines = Self::config_submenu_help_lines(
                        self.terminal_type == TerminalType::Petscii,
                    );
                    self.show_help_page("CONFIGURATION HELP", lines).await?;
                }
                "q" => return Ok(()),
                _ => {
                    self.show_error("Press E, F, G, M, O, R, S, H, or Q.").await?;
                }
            }
        }
    }

    // ─── OTHER SETTINGS ──────────────────────────────────────

    async fn other_settings(&mut self) -> Result<(), std::io::Error> {
        loop {
            let cfg = config::get_config();

            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            self.send_line(&format!("  {}", self.yellow("OTHER SETTINGS")))
                .await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;

            let key_display = if cfg.groq_api_key.is_empty() {
                self.red("(not set)")
            } else {
                self.green("(set)")
            };
            self.send_line(&format!("  AI API key:  {}", key_display))
                .await?;
            self.send_line(&format!(
                "  Homepage:    {}",
                self.amber(&cfg.browser_homepage)
            ))
            .await?;
            // Truncate the location to what fits on one line — a saved value
            // can be up to 60 chars, which on a 40-col PETSCII screen would
            // wrap and push this exactly-22-row menu past the budget (the
            // prompt would scroll off a C64).  Width leaves room for the
            // "  Weather:     " prefix and the " [units]" suffix.
            let loc_display = if cfg.weather_location.is_empty() {
                self.dim("(not set)")
            } else {
                let max_loc = if self.terminal_type == TerminalType::Petscii { 16 } else { 48 };
                self.amber(&truncate_to_width(&cfg.weather_location, max_loc))
            };
            // Show the units alongside the location so this menu mirrors the
            // web/GUI (which place the units control next to the location).
            self.send_line(&format!(
                "  Weather:     {} [{}]",
                loc_display,
                self.dim(&cfg.weather_units)
            ))
            .await?;

            let verbose_status = if cfg.verbose {
                self.green("ON")
            } else {
                self.dim("off")
            };
            self.send_line(&format!("  Verbose log: {}", verbose_status))
                .await?;

            let gui_status = if cfg.enable_console {
                self.green("ON")
            } else {
                self.dim("off")
            };
            self.send_line(&format!("  GUI startup: {}", gui_status))
                .await?;

            let gw_dbg_status = if cfg.gateway_debug {
                self.green("ON")
            } else {
                self.dim("off")
            };
            self.send_line(&format!("  Gateway dbg: {}", gw_dbg_status))
                .await?;
            self.send_line("").await?;

            self.send_line(&format!(
                "  {}  Set AI API key (Groq)",
                self.cyan("A")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Set browser homepage",
                self.cyan("B")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Set weather location",
                self.cyan("W")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Cycle weather units",
                self.cyan("U")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Toggle verbose transfer logging",
                self.cyan("V")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Toggle GUI on startup",
                self.cyan("G")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Toggle gateway debug trace",
                self.cyan("D")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Restart server",
                self.cyan("R")
            ))
            .await?;
            self.send_line("").await?;
            self.send_line(&format!(
                "  {}  {}",
                self.action_prompt("Q", "Back"),
                self.action_prompt("H", "Help")
            ))
            .await?;

            let prompt = format!("{}> ", self.cyan("ethernet/config/other"));
            self.send(&prompt).await?;
            self.flush().await?;

            let input = match self.get_menu_input(false).await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(()),
            };

            match input.as_str() {
                "a" => {
                    self.other_set_field(
                        "AI API key",
                        "groq_api_key",
                        if cfg.groq_api_key.is_empty() { "(not set)" } else { "(hidden)" },
                        true,
                    )
                    .await?;
                }
                "b" => {
                    self.other_set_field(
                        "Browser homepage",
                        "browser_homepage",
                        &cfg.browser_homepage,
                        false,
                    )
                    .await?;
                }
                "w" => {
                    self.other_set_field(
                        "Weather location",
                        "weather_location",
                        &cfg.weather_location,
                        false,
                    )
                    .await?;
                }
                "u" => {
                    // Cycle auto -> us -> metric -> auto (mirrors the weather
                    // screen's own toggle and the web/GUI picker).
                    let next = match cfg.weather_units.as_str() {
                        "auto" => "us",
                        "us" => "metric",
                        _ => "auto",
                    }
                    .to_string();
                    tokio::task::spawn_blocking(move || {
                        config::update_config_value("weather_units", &next);
                    })
                    .await
                    .ok();
                }
                "v" => {
                    let new_val = if cfg.verbose { "false" } else { "true" };
                    let v = new_val.to_string();
                    tokio::task::spawn_blocking(move || {
                        config::update_config_value("verbose", &v);
                    })
                    .await
                    .ok();
                }
                "g" => {
                    let new_val = if cfg.enable_console { "false" } else { "true" };
                    let v = new_val.to_string();
                    tokio::task::spawn_blocking(move || {
                        config::update_config_value("enable_console", &v);
                    })
                    .await
                    .ok();
                    self.config_restart_notice().await?;
                }
                "d" => {
                    let v = (!cfg.gateway_debug).to_string();
                    tokio::task::spawn_blocking(move || {
                        config::update_config_value("gateway_debug", &v);
                    })
                    .await
                    .ok();
                }
                "r" => {
                    self.config_restart_server().await?;
                }
                "h" => {
                    self.other_show_help().await?;
                }
                "q" => return Ok(()),
                _ => {
                    self.show_error("Press A, B, W, U, V, G, D, R, H, or Q.").await?;
                }
            }
        }
    }

    /// Prompt for a free-form (or secret) config string and persist it.
    /// Returns `true` if the value was changed/saved, `false` if the user
    /// cancelled with empty input — so a caller whose setting needs a
    /// server restart can show the restart notice only on an actual change.
    async fn other_set_field(
        &mut self,
        label: &str,
        key: &str,
        current_display: &str,
        is_secret: bool,
    ) -> Result<bool, std::io::Error> {
        self.send_line("").await?;
        self.send_line(&format!(
            "  Current {}: {}",
            label.to_lowercase(),
            if is_secret {
                self.dim(current_display)
            } else {
                self.amber(current_display)
            }
        ))
        .await?;
        self.send(&format!("  New {}: ", label.to_lowercase())).await?;
        self.flush().await?;

        let input = if is_secret {
            self.get_password_input().await?
        } else {
            self.get_line_input().await?
        };

        let input = match input {
            Some(s) if !s.is_empty() => s,
            _ => return Ok(false),
        };

        let k = key.to_string();
        let v = input;
        let saved_label = label.to_string();
        tokio::task::spawn_blocking(move || {
            config::update_config_value(&k, &v);
        })
        .await
        .ok();
        self.send_line("").await?;
        self.send_line(&format!(
            "  {}",
            self.green(&format!("{} updated.", saved_label))
        ))
        .await?;
        self.send_line("").await?;
        self.send("  Press any key to continue.").await?;
        self.flush().await?;
        self.wait_for_key().await?;
        Ok(true)
    }

    async fn other_show_help(&mut self) -> Result<(), std::io::Error> {
        let lines = Self::other_help_lines(self.terminal_type == TerminalType::Petscii);
        self.show_help_page("OTHER SETTINGS HELP", lines).await
    }

    /// Other-settings help, split by terminal width.  Associated fn so a unit
    /// test asserts the REAL lines fit 40 cols (see `punter_help_lines`).
    fn other_help_lines(petscii: bool) -> &'static [&'static str] {
        if petscii {
            &[
                "  A  Groq API key for AI Chat",
                "     (get one free at groq.com)",
                "  B  Default homepage URL for",
                "     the built-in web browser",
                "  W  Weather location (city or",
                "     postal code, worldwide)",
                "  U  Cycle weather units",
                "     (auto / us / metric)",
                "  V  Toggle verbose transfer log",
                "  G  Toggle GUI on startup",
                "     (requires restart)",
                "  D  Toggle gateway debug trace",
                "  R  Restart the server",
            ]
        } else {
            &[
                "  A  Groq API key for AI Chat (get one",
                "     free at console.groq.com)",
                "  B  Default homepage URL for the",
                "     built-in web browser",
                "  W  Weather location (city or postal code)",
                "  U  Cycle weather units (auto / us / metric)",
                "  V  Toggle verbose transfer logging",
                "  G  Toggle GUI on startup (requires",
                "     a server restart)",
                "  D  Toggle gateway debug trace",
                "  R  Restart the server",
            ]
        }
    }

    // ─── SECURITY SETTINGS ───────────────────────────────────

    async fn security_settings(&mut self) -> Result<(), std::io::Error> {
        loop {
            let cfg = config::get_config();

            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            self.send_line(&format!("  {}", self.yellow("SECURITY")))
                .await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;

            let login_status = if cfg.security_enabled {
                self.green("ENABLED")
            } else {
                self.red("Disabled")
            };
            self.send_line(&format!("  Require login: {}", login_status))
                .await?;
            self.send_line("").await?;

            // One credential pair now covers telnet, SSH, and the web
            // UI; the earlier per-protocol user/pass lines collapsed
            // into a single Username / Password display.
            self.send_line(&format!(
                "  Username: {}",
                self.amber(&cfg.username)
            ))
            .await?;
            self.send_line(&format!(
                "  Password: {}",
                self.dim("(hidden)")
            ))
            .await?;
            self.send_line("").await?;

            self.send_line(&format!(
                "  {}  Toggle require login",
                self.cyan("L")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Set username",
                self.cyan("U")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Set password",
                self.cyan("P")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Restart server",
                self.cyan("R")
            ))
            .await?;
            self.send_line("").await?;
            self.send_line(&format!(
                "  {}  {}",
                self.action_prompt("Q", "Back"),
                self.action_prompt("H", "Help")
            ))
            .await?;

            let prompt = format!("{}> ", self.cyan("ethernet/config/security"));
            self.send(&prompt).await?;
            self.flush().await?;

            let input = match self.get_menu_input(false).await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(()),
            };

            match input.as_str() {
                "l" => {
                    let new_val = if cfg.security_enabled { "false" } else { "true" };
                    let v = new_val.to_string();
                    tokio::task::spawn_blocking(move || {
                        config::update_config_value("security_enabled", &v);
                    })
                    .await
                    .ok();
                    self.config_restart_notice().await?;
                }
                "u" => {
                    self.security_set_field("Username", "username", &cfg.username, false).await?;
                }
                "p" => {
                    self.security_set_field("Password", "password", &cfg.password, true).await?;
                }
                "r" => {
                    self.config_restart_server().await?;
                }
                "h" => {
                    self.security_show_help().await?;
                }
                "q" => return Ok(()),
                _ => {
                    self.show_error("Press L, U, P, R, H, or Q.").await?;
                }
            }
        }
    }

    async fn security_set_field(
        &mut self,
        label: &str,
        key: &str,
        current: &str,
        is_password: bool,
    ) -> Result<(), std::io::Error> {
        self.send_line("").await?;
        if is_password {
            self.send_line(&format!(
                "  Current {}: {}",
                label.to_lowercase(),
                self.dim("(hidden)")
            ))
            .await?;
        } else {
            self.send_line(&format!(
                "  Current {}: {}",
                label.to_lowercase(),
                self.amber(current)
            ))
            .await?;
        }
        self.send(&format!("  New {}: ", label.to_lowercase())).await?;
        self.flush().await?;

        let input = if is_password {
            self.get_password_input().await?
        } else {
            self.get_line_input().await?
        };

        let input = match input {
            Some(s) if !s.is_empty() => s,
            _ => return Ok(()),
        };

        let k = key.to_string();
        let v = input;
        tokio::task::spawn_blocking(move || {
            config::update_config_value(&k, &v);
        })
        .await
        .ok();
        self.config_restart_notice().await?;
        Ok(())
    }

    async fn security_show_help(&mut self) -> Result<(), std::io::Error> {
        let lines = Self::security_help_lines(self.terminal_type == TerminalType::Petscii);
        self.show_help_page("SECURITY HELP", lines).await
    }

    /// Login-security settings help, split by terminal width.  Associated fn so a
    /// unit test asserts the REAL lines fit 40 cols (see `punter_help_lines`).
    fn security_help_lines(petscii: bool) -> &'static [&'static str] {
        if petscii {
            &[
                "  Configure login security.",
                "",
                "  Menu items:",
                "  L  Toggle login requirement",
                "  U  Set the login username",
                "  P  Set the login password",
                "  R  Restart the server",
                "",
                "  Credentials:",
                "  One username/password covers",
                "  telnet, SSH, and the web UI.",
                "  Stored in plaintext in",
                "  egateway.conf - don't reuse",
                "  sensitive passwords here.",
                "",
                "  When security is OFF:",
                "  Only private-range IPs can",
                "  connect (RFC 1918, loopback,",
                "  link-local, IPv6 unique-local).",
                "  Public IPs are refused, and",
                "  gateway addresses (*.*.*.1)",
                "  are rejected defensively.",
                "",
                "  When security is ON:",
                "  Any IP may connect, but must",
                "  authenticate. 3 failed logins",
                "  from the same IP triggers a",
                "  5-minute lockout for that IP.",
                "",
                "  Telnet transmits credentials",
                "  in cleartext. Use SSH for any",
                "  non-local access.",
                "",
                "  Changes are saved immediately",
                "  but require a server restart.",
            ]
        } else {
            &[
                "  Configure login security.",
                "",
                "  Menu items:",
                "  L  Toggle whether a login is required",
                "  U  Set the login username",
                "  P  Set the login password",
                "  R  Restart the server",
                "",
                "  Credentials:",
                "  One username/password pair covers telnet,",
                "  SSH, and the web configuration UI.  Stored",
                "  in plaintext in egateway.conf - don't reuse",
                "  sensitive passwords on this server.",
                "",
                "  When security is OFF (default):",
                "  Only private-range IPs are allowed to",
                "  connect (RFC 1918 10/172.16/192.168,",
                "  loopback 127.0.0.0/8, link-local",
                "  169.254.0.0/16, IPv6 ::1, fe80::/10,",
                "  and fd00::/8). Public IPs get a refusal",
                "  message, and gateway addresses (those",
                "  ending in .1) are rejected to guard",
                "  against accidental router exposure.",
                "",
                "  When security is ON:",
                "  Any IP may connect but must authenticate.",
                "  After 3 failed login attempts from the",
                "  same IP, that address is locked out for",
                "  5 minutes. Credentials are compared in",
                "  constant time to resist timing attacks.",
                "",
                "  Telnet transmits every byte (including",
                "  the password) in cleartext. For any",
                "  non-local access, use the SSH interface",
                "  instead (Configuration > Server > S).",
                "",
                "  Changes are saved immediately but",
                "  require a server restart to take effect.",
            ]
        }
    }

    // ─── SERVER CONFIGURATION ───────────────────────────────

    async fn server_configuration(&mut self) -> Result<(), std::io::Error> {
        loop {
            let cfg = config::get_config();

            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            self.send_line(&format!("  {}", self.yellow("SERVER CONFIGURATION")))
                .await?;
            self.send_line(&sep).await?;

            let telnet_status = if cfg.telnet_enabled {
                self.green("ENABLED")
            } else {
                self.red("Disabled")
            };
            self.send_line(&format!(
                "  Telnet: {} (port {})",
                telnet_status, cfg.telnet_port
            ))
            .await?;
            let ssh_status = if cfg.ssh_enabled {
                self.green("ENABLED")
            } else {
                self.red("Disabled")
            };
            self.send_line(&format!(
                "  SSH:    {} (port {})",
                ssh_status, cfg.ssh_port
            ))
            .await?;
            let kermit_status = if cfg.kermit_server_enabled {
                self.green("ENABLED")
            } else {
                self.red("Disabled")
            };
            self.send_line(&format!(
                "  Kermit: {} (port {})",
                kermit_status, cfg.kermit_server_port
            ))
            .await?;
            let web_status = if cfg.web_enabled {
                self.green("ENABLED")
            } else {
                self.red("Disabled")
            };
            self.send_line(&format!(
                "  Web:    {} (port {})",
                web_status, cfg.web_port
            ))
            .await?;
            let ip_safety_status = if cfg.disable_ip_safety {
                self.red("DISABLED")
            } else {
                self.green("Enabled")
            };
            self.send_line(&format!(
                "  IP safety: {}",
                ip_safety_status
            ))
            .await?;
            self.send_line("").await?;

            // (The "Server addresses:" banner now lives at the top of the
            // CONFIGURATION menu — see render_server_address_block / §4.7.)

            self.send_line(&format!(
                "  {}  Toggle telnet    {}  Set telnet port",
                self.cyan("T"),
                self.cyan("P")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Toggle SSH       {}  Set SSH port",
                self.cyan("S"),
                self.cyan("O")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Toggle Kermit    {}  Set Kermit port",
                self.cyan("K"),
                self.cyan("J")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Toggle Web       {}  Set Web port",
                self.cyan("W"),
                self.cyan("B")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  IP safety        {}  Restart server",
                self.cyan("I"),
                self.cyan("R")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Session cap      {}  Idle timeout",
                self.cyan("C"),
                self.cyan("D")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Master/Slave",
                self.cyan("M")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  {}",
                self.action_prompt("Q", "Back"),
                self.action_prompt("H", "Help")
            ))
            .await?;

            let prompt = format!("{}> ", self.cyan("ethernet/config/server"));
            self.send(&prompt).await?;
            self.flush().await?;

            let input = match self.get_menu_input(false).await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(()),
            };

            match input.as_str() {
                "t" => {
                    let new_val = if cfg.telnet_enabled { "false" } else { "true" };
                    let v = new_val.to_string();
                    tokio::task::spawn_blocking(move || {
                        config::update_config_value("telnet_enabled", &v);
                    })
                    .await
                    .ok();
                    self.config_restart_notice().await?;
                }
                "p" => {
                    self.config_set_port("Telnet", "telnet_port", cfg.telnet_port).await?;
                }
                "s" => {
                    let new_val = if cfg.ssh_enabled { "false" } else { "true" };
                    let v = new_val.to_string();
                    tokio::task::spawn_blocking(move || {
                        config::update_config_value("ssh_enabled", &v);
                    })
                    .await
                    .ok();
                    self.config_restart_notice().await?;
                }
                "o" => {
                    self.config_set_port("SSH", "ssh_port", cfg.ssh_port).await?;
                }
                "k" => {
                    self.kermit_server_toggle(cfg.kermit_server_enabled).await?;
                }
                "j" => {
                    self.config_set_port(
                        "Kermit server",
                        "kermit_server_port",
                        cfg.kermit_server_port,
                    )
                    .await?;
                }
                "w" => {
                    let new_val = if cfg.web_enabled { "false" } else { "true" };
                    let v = new_val.to_string();
                    tokio::task::spawn_blocking(move || {
                        config::update_config_value("web_enabled", &v);
                    })
                    .await
                    .ok();
                    self.config_restart_notice().await?;
                }
                "b" => {
                    self.config_set_port("Web", "web_port", cfg.web_port).await?;
                }
                "i" => {
                    self.disable_ip_safety_toggle(cfg.disable_ip_safety).await?;
                }
                "c" => {
                    self.config_set_count(
                        "session cap",
                        "max_sessions",
                        cfg.max_sessions as u64,
                        1,
                        "New session cap (1 or more)",
                    )
                    .await?;
                }
                "d" => {
                    self.config_set_count(
                        "idle timeout",
                        "idle_timeout_secs",
                        cfg.idle_timeout_secs,
                        0,
                        "New idle timeout in seconds (0 = off)",
                    )
                    .await?;
                }
                "m" => {
                    self.master_slave_config().await?;
                }
                "r" => {
                    self.config_restart_server().await?;
                }
                "h" => {
                    self.config_show_help().await?;
                }
                "q" => return Ok(()),
                _ => {
                    // Keep this short — show_error indents by 2 chars
                    // and PETSCII tops out at 40 cols.  The expanded
                    // "Press T, P, S, O, ..." form blew the limit once
                    // W and B were added, so we now point to the menu.
                    self.show_error("Press a letter from the menu.").await?;
                }
            }
        }
    }

    // ─── MASTER / SLAVE (relay) sub-screen ───────────────────

    /// Master/Slave serial-extender settings (§4.7).  Its own fresh
    /// 22-row budget.  Shows the role and the relevant master/slave
    /// fields, and lets the operator change them.  Role / relay changes
    /// take effect on the next server restart (the relay listener and the
    /// slave client are started at boot from `gateway_role`), so changes
    /// here surface a restart notice.
    async fn master_slave_config(&mut self) -> Result<(), std::io::Error> {
        loop {
            let cfg = config::get_config();

            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            self.send_line(&format!("  {}", self.yellow("MASTER / SLAVE")))
                .await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;

            let role_disp = match cfg.gateway_role.as_str() {
                "master" => self.green("MASTER"),
                "slave" => self.cyan("SLAVE"),
                _ => self.dim("STANDALONE"),
            };
            self.send_line(&format!("  Role: {}", role_disp)).await?;

            let is_master = cfg.gateway_role == "master";
            let is_slave = cfg.gateway_role == "slave";

            // Accept-relays applies to a MASTER only; grey it out in the other
            // roles so the operator isn't led to toggle a field that is inert.
            if is_master {
                let accept_disp = if cfg.master_accept_relays {
                    self.green("ENABLED")
                } else {
                    self.red("Disabled")
                };
                self.send_line(&format!("  Accept relays: {}", accept_disp))
                    .await?;
            } else {
                self.send_line(&format!(
                    "  {}",
                    self.dim("Accept relays: (master only)")
                ))
                .await?;
            }

            // Master host/user/pass point this gateway at its master, so they
            // apply to a SLAVE only; grey them out in the other roles.
            if is_slave {
                let host_disp = if cfg.slave_master_host.is_empty() {
                    self.dim("(not set)")
                } else {
                    self.amber(&format!(
                        "{}:{}",
                        cfg.slave_master_host, cfg.slave_master_port
                    ))
                };
                self.send_line(&format!("  Master: {}", host_disp)).await?;
                let user_disp = if cfg.slave_master_username.is_empty() {
                    self.dim("(not set)")
                } else {
                    self.amber(&cfg.slave_master_username)
                };
                self.send_line(&format!("  User:   {}", user_disp)).await?;
                let pass_disp = if cfg.slave_master_password.is_empty() {
                    self.dim("(not set)")
                } else {
                    self.green("(set)")
                };
                self.send_line(&format!("  Pass:   {}", pass_disp)).await?;
            } else {
                self.send_line(&format!(
                    "  {}",
                    self.dim("Master/User/Pass: (slave only)")
                ))
                .await?;
            }
            self.send_line("").await?;

            // Live relay status (§9 #10), read-only.  A master lists the
            // remote console ports slaves have registered right now; a slave
            // shows each console port's link state to the master — so an
            // operator can confirm connectivity without grepping logs.  The
            // Serial Gateway picker remains where a master user actually
            // bridges to a remote port; this is a compact summary (capped to
            // keep the screen inside the 22-row PETSCII budget).
            match cfg.gateway_role.as_str() {
                "master" => {
                    let ports = crate::relay::list_remote_ports();
                    self.send_line(&format!(
                        "  {} ({})",
                        self.dim("Registered remote ports:"),
                        ports.len()
                    ))
                    .await?;
                    const RELAY_STATUS_CAP: usize = 3;
                    for (ip, label) in ports.iter().take(RELAY_STATUS_CAP) {
                        self.send_line(&format!("    {}@{}", self.amber(label), ip))
                            .await?;
                    }
                    if ports.len() > RELAY_STATUS_CAP {
                        self.send_line(&format!(
                            "    {}",
                            self.dim(&format!("+{} more", ports.len() - RELAY_STATUS_CAP))
                        ))
                        .await?;
                    }
                    self.send_line("").await?;
                }
                "slave" => {
                    for id in [
                        crate::config::SerialPortId::A,
                        crate::config::SerialPortId::B,
                    ] {
                        let p = cfg.port(id);
                        if p.enabled && p.mode == "console" {
                            let st = crate::relay::slave_link_state(id.index());
                            self.send_line(&format!(
                                "  Link {}: {}",
                                id.label(),
                                self.amber(st.label())
                            ))
                            .await?;
                        }
                    }
                    self.send_line("").await?;
                }
                _ => {}
            }

            self.send_line(&format!(
                "  {}  Cycle role       {}  Accept relays",
                self.cyan("R"),
                self.cyan("A")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Master host      {}  Master port",
                self.cyan("M"),
                self.cyan("P")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Master user      {}  Master pass",
                self.cyan("U"),
                self.cyan("W")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  {}",
                self.action_prompt("Q", "Back"),
                self.action_prompt("H", "Help")
            ))
            .await?;

            let prompt = format!("{}> ", self.cyan("ethernet/config/relay"));
            self.send(&prompt).await?;
            self.flush().await?;

            let input = match self.get_menu_input(false).await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(()),
            };

            match input.as_str() {
                "r" => {
                    let next = match cfg.gateway_role.as_str() {
                        "standalone" => "master",
                        "master" => "slave",
                        _ => "standalone",
                    };
                    let became_master = next == "master";
                    let v = next.to_string();
                    tokio::task::spawn_blocking(move || {
                        config::update_config_value("gateway_role", &v);
                        // A master with relays off can't accept slaves, so
                        // default the accept-relays gate ON when entering
                        // master (the operator can still turn it off with A).
                        if became_master {
                            config::update_config_value("master_accept_relays", "true");
                        }
                    })
                    .await
                    .ok();
                    // The relay listens on the SSH port, so a master needs the
                    // SSH server enabled. Warn if it's off — never toggle it.
                    if became_master && !config::get_config().ssh_enabled {
                        self.relay_ssh_needed_notice().await?;
                    }
                    self.config_restart_notice().await?;
                }
                "a" => {
                    if cfg.gateway_role != "master" {
                        self.relay_field_not_applicable(
                            "Accept relays: Master role only.",
                        )
                        .await?;
                    } else {
                        let v = (!cfg.master_accept_relays).to_string();
                        tokio::task::spawn_blocking(move || {
                            config::update_config_value("master_accept_relays", &v);
                        })
                        .await
                        .ok();
                        self.config_restart_notice().await?;
                    }
                }
                "m" => {
                    if cfg.gateway_role != "slave" {
                        self.relay_field_not_applicable(
                            "Master settings: Slave role only.",
                        )
                        .await?;
                    } else if self
                        // `other_set_field` only persists on a non-empty entry;
                        // show the restart notice only when it actually changed.
                        .other_set_field(
                            "Master host",
                            "slave_master_host",
                            &cfg.slave_master_host,
                            false,
                        )
                        .await?
                    {
                        self.config_restart_notice().await?;
                    }
                }
                "p" => {
                    if cfg.gateway_role != "slave" {
                        self.relay_field_not_applicable(
                            "Master settings: Slave role only.",
                        )
                        .await?;
                    } else {
                        // config_set_port shows its own restart notice on a
                        // successful change, so this branch must not add one.
                        self.config_set_port(
                            "Master",
                            "slave_master_port",
                            cfg.slave_master_port,
                        )
                        .await?;
                    }
                }
                "u" => {
                    if cfg.gateway_role != "slave" {
                        self.relay_field_not_applicable(
                            "Master settings: Slave role only.",
                        )
                        .await?;
                    } else if self
                        .other_set_field(
                            "Master user",
                            "slave_master_username",
                            &cfg.slave_master_username,
                            false,
                        )
                        .await?
                    {
                        self.config_restart_notice().await?;
                    }
                }
                "w" => {
                    if cfg.gateway_role != "slave" {
                        self.relay_field_not_applicable(
                            "Master settings: Slave role only.",
                        )
                        .await?;
                    } else if self
                        .other_set_field(
                            "Master pass",
                            "slave_master_password",
                            if cfg.slave_master_password.is_empty() {
                                "(not set)"
                            } else {
                                "(set)"
                            },
                            true,
                        )
                        .await?
                    {
                        self.config_restart_notice().await?;
                    }
                }
                "h" => {
                    self.show_help_page(
                        "MASTER / SLAVE HELP",
                        Self::master_slave_help_lines(
                            self.terminal_type == TerminalType::Petscii,
                        ),
                    )
                    .await?;
                }
                "q" => return Ok(()),
                _ => {
                    self.show_error("Press a letter from the menu.").await?;
                }
            }
        }
    }

    /// Help lines for the Master/Slave sub-screen.  Kept in a function so
    /// the help-fit tests can iterate them (see CLAUDE.md testing notes).
    /// One table that fits the 40-col PETSCII budget (so it also fits the
    /// 80-col ANSI budget); `petscii` is accepted for signature parity
    /// with the other `*_help_lines` and the `all_help_line_groups` table.
    fn master_slave_help_lines(_petscii: bool) -> &'static [&'static str] {
        &[
            "  Role / relay settings.",
            "",
            "  Standalone: normal gateway.",
            "  Master: accepts slave relays",
            "    (also enable Accept relays).",
            "  Slave: bridges its serial ports",
            "    to the master over SSH.",
            "",
            "  R Cycle role   A Accept relays",
            "  M Host  P Port  U User  W Pass",
            "",
            "  Slave logs in with the master's",
            "  username/password.  Restart to",
            "  apply.",
        ]
    }

    /// Toggle `disable_ip_safety`.  Off→on shows a full-screen security
    /// warning (the listener will accept connections from any source IP,
    /// including public addresses, while `security_enabled` is false)
    /// and prompts Y/N — same posture as `kermit_server_toggle`.  On→off
    /// is one-click safe (re-tightens the allowlist).  Either outcome
    /// falls through and returns to the Server Configuration screen via
    /// the surrounding `loop` in `server_configuration`.  The change is
    /// effective immediately because the accept loop reads the live
    /// config on each connection.
    async fn disable_ip_safety_toggle(
        &mut self,
        currently_disabled: bool,
    ) -> Result<(), std::io::Error> {
        if currently_disabled {
            tokio::task::spawn_blocking(move || {
                config::update_config_value("disable_ip_safety", "false");
            })
            .await
            .ok();
            self.send_line("").await?;
            self.send_line(&format!(
                "  {}",
                self.green("IP-safety allowlist re-enabled.")
            ))
            .await?;
            self.send_line("").await?;
            self.send("  Press any key to continue.").await?;
            self.flush().await?;
            self.wait_for_key().await?;
            return Ok(());
        }

        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;
        self.send_line(&format!(
            "  {}",
            self.yellow("DISABLE IP SAFETY — SECURITY WARNING")
        ))
        .await?;
        self.send_line(&sep).await?;
        self.send_line("").await?;
        self.send_line(&format!(
            "  {}",
            self.amber("This removes the private-IP allowlist.")
        ))
        .await?;
        self.send_line("").await?;
        self.send_line(
            "  When Require Login is off, the telnet listener",
        )
        .await?;
        self.send_line(
            "  normally accepts only private/loopback/link-local",
        )
        .await?;
        self.send_line(
            "  addresses, and rejects gateway-style *.*.*.1",
        )
        .await?;
        self.send_line(
            "  addresses. That allowlist is the only thing",
        )
        .await?;
        self.send_line(
            "  standing between a public IP and an unauthenticated",
        )
        .await?;
        self.send_line("  session.").await?;
        self.send_line("").await?;
        self.send_line(
            "  Disabling it accepts every source IP. Anyone who",
        )
        .await?;
        self.send_line(
            "  can reach your telnet port will be able to connect.",
        )
        .await?;
        self.send_line("").await?;
        self.send_line(
            "  Disable only when a firewall, VPN, or other network",
        )
        .await?;
        self.send_line(
            "  control sits in front of the listener, or when you",
        )
        .await?;
        self.send_line(
            "  are about to enable Require Login. The change takes",
        )
        .await?;
        self.send_line(
            "  effect on the next inbound connection.",
        )
        .await?;
        self.send_line("").await?;
        self.send(&format!(
            "  Disable IP safety? ({}/{}): ",
            self.cyan("Y"),
            self.cyan("N")
        ))
        .await?;
        self.flush().await?;

        let answer = self.get_menu_input(false).await?;
        let confirmed = matches!(
            answer.as_deref().map(|s| s.trim()),
            Some("y") | Some("Y")
        );

        self.send_line("").await?;
        if confirmed {
            tokio::task::spawn_blocking(move || {
                config::update_config_value("disable_ip_safety", "true");
            })
            .await
            .ok();
            self.send_line(&format!(
                "  {}",
                self.green("IP-safety allowlist disabled.")
            ))
            .await?;
        } else {
            self.send_line(&format!(
                "  {}",
                self.dim("IP safety left enabled.")
            ))
            .await?;
        }
        self.send_line("").await?;
        self.send("  Press any key to continue.").await?;
        self.flush().await?;
        self.wait_for_key().await?;
        Ok(())
    }

    /// Toggle `kermit_server_enabled`.  Off→on shows a full-screen
    /// security warning (the listener bypasses authentication AND the
    /// private-IP allowlist) and prompts Y/N — same posture as
    /// `kermit_toggle_atdt_kermit`.  On→off is one-click safe.  Either
    /// outcome falls through and returns to the Server Configuration
    /// screen via the surrounding `loop` in `server_configuration`.
    async fn kermit_server_toggle(
        &mut self,
        currently_enabled: bool,
    ) -> Result<(), std::io::Error> {
        if currently_enabled {
            tokio::task::spawn_blocking(move || {
                config::update_config_value("kermit_server_enabled", "false");
            })
            .await
            .ok();
            self.send_line("").await?;
            self.send_line(&format!(
                "  {}",
                self.green("Kermit server disabled.")
            ))
            .await?;
            self.config_restart_notice().await?;
            return Ok(());
        }

        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;
        self.send_line(&format!(
            "  {}",
            self.yellow("ENABLE KERMIT SERVER — SECURITY WARNING")
        ))
        .await?;
        self.send_line(&sep).await?;
        self.send_line("").await?;
        self.send_line(&format!(
            "  {}",
            self.amber("This bypasses ALL gateway security.")
        ))
        .await?;
        self.send_line("").await?;
        self.send_line(
            "  When enabled, the gateway opens a dedicated TCP",
        )
        .await?;
        self.send_line(
            "  listener that drops every accepted connection",
        )
        .await?;
        self.send_line(
            "  straight into Kermit server mode — no telnet menu,",
        )
        .await?;
        self.send_line(
            "  no username, no password, no private-IP filter.",
        )
        .await?;
        self.send_line("").await?;
        self.send_line(
            "  Anyone who can reach the listener can read and",
        )
        .await?;
        self.send_line(
            "  write files in your transfer directory.",
        )
        .await?;
        self.send_line("").await?;
        self.send_line(
            "  Enable only when the network path is trusted",
        )
        .await?;
        self.send_line(
            "  (LAN you control, isolated lab, single-user setup).",
        )
        .await?;
        self.send_line("").await?;
        self.send(&format!(
            "  Enable Kermit server? ({}/{}): ",
            self.cyan("Y"),
            self.cyan("N")
        ))
        .await?;
        self.flush().await?;

        let answer = self.get_menu_input(false).await?;
        let confirmed = matches!(
            answer.as_deref().map(|s| s.trim()),
            Some("y") | Some("Y")
        );

        self.send_line("").await?;
        if confirmed {
            tokio::task::spawn_blocking(move || {
                config::update_config_value("kermit_server_enabled", "true");
            })
            .await
            .ok();
            self.send_line(&format!(
                "  {}",
                self.green("Kermit server enabled.")
            ))
            .await?;
            self.config_restart_notice().await?;
        } else {
            self.send_line(&format!(
                "  {}",
                self.dim("Kermit server left disabled.")
            ))
            .await?;
            self.send_line("").await?;
            self.send("  Press any key to continue.").await?;
            self.flush().await?;
            self.wait_for_key().await?;
        }
        Ok(())
    }

    // ─── GATEWAY CONFIGURATION ──────────────────────────────
    //
    // Submenu of Server Configuration.  Edits the two persistent
    // outbound-gateway modes so the user doesn't have to touch the GUI
    // or `egateway.conf` for these settings.  Changes take effect on the
    // next gateway connection — no server restart needed.
    async fn gateway_configuration(&mut self) -> Result<(), std::io::Error> {
        loop {
            let cfg = config::get_config();

            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            self.send_line(&format!("  {}", self.yellow("GATEWAY CONFIGURATION")))
                .await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;

            let telnet_mode = if cfg.telnet_gateway_raw {
                self.red("Raw TCP")
            } else {
                self.green("Telnet")
            };
            self.send_line(&format!("  Telnet mode: {}", telnet_mode))
                .await?;
            let coop = if cfg.telnet_gateway_negotiate {
                self.green("On")
            } else {
                self.red("Off")
            };
            self.send_line(&format!("  Cooperative: {}", coop))
                .await?;
            let ssh_auth = if cfg.ssh_gateway_auth == "password" {
                self.yellow("Password")
            } else {
                self.green("Key")
            };
            self.send_line(&format!("  SSH auth:    {}", ssh_auth))
                .await?;
            self.send_line("").await?;

            self.send_line(&format!(
                "  {}  Toggle telnet mode (Telnet/Raw)",
                self.cyan("T")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Toggle cooperative (TTYPE/NAWS)",
                self.cyan("C")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Toggle SSH auth (Key/Password)",
                self.cyan("S")
            ))
            .await?;
            self.send_line("").await?;
            self.send_line(&format!(
                "  {}  {}",
                self.action_prompt("Q", "Back"),
                self.action_prompt("H", "Help")
            ))
            .await?;

            let prompt = format!("{}> ", self.cyan("ethernet/config/server/gateway"));
            self.send(&prompt).await?;
            self.flush().await?;

            let input = match self.get_menu_input(false).await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(()),
            };

            match input.as_str() {
                "t" => {
                    let new_val = if cfg.telnet_gateway_raw { "false" } else { "true" };
                    let v = new_val.to_string();
                    tokio::task::spawn_blocking(move || {
                        config::update_config_value("telnet_gateway_raw", &v);
                    })
                    .await
                    .ok();
                }
                "c" => {
                    let new_val = if cfg.telnet_gateway_negotiate {
                        "false"
                    } else {
                        "true"
                    };
                    let v = new_val.to_string();
                    tokio::task::spawn_blocking(move || {
                        config::update_config_value("telnet_gateway_negotiate", &v);
                    })
                    .await
                    .ok();
                }
                "s" => {
                    let new_val = if cfg.ssh_gateway_auth == "password" {
                        "key"
                    } else {
                        "password"
                    };
                    let v = new_val.to_string();
                    tokio::task::spawn_blocking(move || {
                        config::update_config_value("ssh_gateway_auth", &v);
                    })
                    .await
                    .ok();
                }
                "h" => {
                    self.gateway_config_show_help().await?;
                }
                "q" => return Ok(()),
                _ => {
                    self.show_error("Press T, C, S, H, or Q.").await?;
                }
            }
        }
    }

    async fn gateway_config_show_help(&mut self) -> Result<(), std::io::Error> {
        let lines = Self::gateway_config_help_lines(self.terminal_type == TerminalType::Petscii);
        self.show_help_page("GATEWAY CONFIG HELP", lines).await
    }

    /// Telnet/SSH-gateway configuration help, split by terminal width.
    /// Associated fn so a unit test asserts the REAL lines fit 40 cols.
    fn gateway_config_help_lines(petscii: bool) -> &'static [&'static str] {
        if petscii {
            &[
                "  Configure the outbound Telnet",
                "  and SSH Gateway menus (the S",
                "  and T main-menu items that",
                "  proxy to remote servers).",
                "",
                "  Telnet mode:",
                "    Telnet - parse IAC option",
                "             negotiation; works",
                "             with real telnet",
                "             servers. Default.",
                "    Raw    - raw TCP byte stream,",
                "             no IAC. Use for MUDs",
                "             and hand-rolled BBS",
                "             software that don't",
                "             speak telnet.",
                "",
                "  Telnet mode options:",
                "    Cooperative - proactively offers",
                "      TTYPE, NAWS, DO ECHO so BBSes",
                "      that wait for the client to",
                "      ask first still get full-",
                "      screen behavior. Enable for",
                "      cooperative telnet servers;",
                "      disable for raw-TCP services.",
                "",
                "  SSH auth:",
                "    Key      - offer the gateway's",
                "               Ed25519 client key.",
                "               Paste the public half",
                "               into the remote's",
                "               ~/.ssh/authorized_keys",
                "               first. Passwordless.",
                "    Password - prompt for the remote",
                "               account's password on",
                "               each connect.",
                "",
                "  Both settings are saved to",
                "  egateway.conf and take effect on",
                "  the next gateway connection.",
                "  No server restart is required.",
            ]
        } else {
            &[
                "  Configure the outbound Telnet and SSH",
                "  Gateway menus (the S and T items on the",
                "  main menu that proxy to remote servers).",
                "",
                "  Telnet mode:",
                "    Telnet  - parse IAC option negotiation",
                "              (default; works with every real",
                "              telnet server). IAC bytes in",
                "              data are escaped as IAC IAC.",
                "    Raw     - raw TCP byte stream, no IAC.",
                "              Use for MUDs and hand-rolled",
                "              BBS software that aren't telnet.",
                "              Bytes pass through unmodified.",
                "",
                "  Cooperative mode (Telnet only):",
                "    When on, the gateway sends WILL TTYPE,",
                "    WILL NAWS, and DO ECHO proactively so",
                "    BBSes that wait for the client to ask",
                "    first still get echo cooperation,",
                "    terminal-type adaptation, and full-screen",
                "    window sizing. Off by default so raw-TCP",
                "    services aren't spammed with IAC bytes",
                "    they can't parse.",
                "",
                "  SSH auth:",
                "    Key      - offer the gateway's Ed25519",
                "               client key. Copy the public",
                "               half (shown under Server >",
                "               More in the GUI) into the",
                "               remote's authorized_keys file.",
                "               Passwordless once installed.",
                "    Password - prompt for the remote account's",
                "               password on each connect. No",
                "               key is offered.",
                "",
                "  Host keys:",
                "    On first dial, the gateway displays the",
                "    remote's SHA-256 fingerprint and asks",
                "    whether to trust it (TOFU). Accepted",
                "    fingerprints are saved to gateway_hosts;",
                "    a changed key triggers a prominent",
                "    HOST KEY CHANGED warning.",
                "",
                "  Changes are saved immediately and take",
                "  effect on the next gateway connection.",
                "  No server restart is required.",
            ]
        }
    }

    async fn config_set_port(
        &mut self,
        label: &str,
        key: &str,
        current: u16,
    ) -> Result<(), std::io::Error> {
        self.send_line("").await?;
        self.send_line(&format!(
            "  Current {} port: {}",
            label,
            self.amber(&current.to_string())
        ))
        .await?;
        self.send("  New port (1-65535): ").await?;
        self.flush().await?;

        let input = match self.get_line_input().await? {
            Some(s) if !s.is_empty() => s,
            _ => return Ok(()),
        };

        if let Ok(port) = input.parse::<u16>() {
            if port >= 1 {
                let k = key.to_string();
                let v = port.to_string();
                tokio::task::spawn_blocking(move || {
                    config::update_config_value(&k, &v);
                })
                .await
                .ok();
                self.config_restart_notice().await?;
            } else {
                self.show_error("Invalid port number.").await?;
            }
        } else {
            self.show_error("Invalid port number.").await?;
        }
        Ok(())
    }

    /// Prompt for an integer server setting (session cap / idle timeout)
    /// and persist it.  Shows the current value, reads a line, and accepts
    /// values `>= min` — `min = 1` floors the session cap, `min = 0` lets
    /// the idle timeout be disabled (and renders the current `0` as
    /// "0 (disabled)").  Non-numeric or out-of-range input is rejected.
    /// Like `config_set_port`, the change needs a server restart, so it
    /// ends on the shared restart notice.
    async fn config_set_count(
        &mut self,
        label: &str,
        key: &str,
        current: u64,
        min: u64,
        prompt: &str,
    ) -> Result<(), std::io::Error> {
        self.send_line("").await?;
        let shown = if current == 0 && min == 0 {
            "0 (disabled)".to_string()
        } else {
            current.to_string()
        };
        self.send_line(&format!("  Current {}: {}", label, self.amber(&shown)))
            .await?;
        self.send(&format!("  {}: ", prompt)).await?;
        self.flush().await?;

        let input = match self.get_line_input().await? {
            Some(s) if !s.is_empty() => s,
            _ => return Ok(()),
        };

        if let Ok(v) = input.parse::<u64>() {
            if v >= min {
                let k = key.to_string();
                let val = v.to_string();
                tokio::task::spawn_blocking(move || {
                    config::update_config_value(&k, &val);
                })
                .await
                .ok();
                self.config_restart_notice().await?;
            } else {
                self.show_error("Value out of range.").await?;
            }
        } else {
            self.show_error("Enter a whole number.").await?;
        }
        Ok(())
    }

    async fn config_restart_notice(&mut self) -> Result<(), std::io::Error> {
        self.send_line("").await?;
        self.send_line(&format!(
            "  {}",
            self.yellow("Restart the server for changes")
        ))
        .await?;
        self.send_line(&format!(
            "  {}",
            self.yellow("to take effect.")
        ))
        .await?;
        self.send_line("").await?;
        self.send("  Press any key to continue.").await?;
        self.flush().await?;
        self.wait_for_key().await?;
        Ok(())
    }

    /// Brief "this field doesn't apply in the current role" notice for the
    /// Master/Slave menu, so a greyed option explains itself instead of
    /// silently doing nothing when its key is pressed.
    async fn relay_field_not_applicable(
        &mut self,
        msg: &str,
    ) -> Result<(), std::io::Error> {
        self.send_line("").await?;
        self.send_line(&format!("  {}", self.yellow(msg))).await?;
        self.send_line(&format!("  {}", self.dim("Change Role (R) first.")))
            .await?;
        self.send_line("").await?;
        self.send("  Press any key to continue.").await?;
        self.flush().await?;
        self.wait_for_key().await?;
        Ok(())
    }

    /// Warn (only) that switching to Master needs the SSH server, which is
    /// currently off — the relay listens on the SSH port.  Per the operator's
    /// choice this never toggles SSH; it just points the way.
    async fn relay_ssh_needed_notice(&mut self) -> Result<(), std::io::Error> {
        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;
        self.send_line(&format!("  {}", self.yellow("MASTER NEEDS SSH")))
            .await?;
        self.send_line(&sep).await?;
        self.send_line("").await?;
        self.send_line("  Slaves connect to a master over").await?;
        self.send_line("  the SSH server, which is now OFF.").await?;
        self.send_line("").await?;
        self.send_line(&format!(
            "  {}",
            self.dim("Enable SSH in Server settings and")
        ))
        .await?;
        self.send_line(&format!(
            "  {}",
            self.dim("restart, or slaves can't connect.")
        ))
        .await?;
        self.send_line("").await?;
        self.send("  Press any key to continue.").await?;
        self.flush().await?;
        self.wait_for_key().await?;
        Ok(())
    }

    async fn config_restart_server(&mut self) -> Result<(), std::io::Error> {
        self.send_line("").await?;
        self.send_line(&format!(
            "  {}",
            self.red("WARNING: All active sessions")
        ))
        .await?;
        self.send_line(&format!(
            "  {}",
            self.red("will be disconnected.")
        ))
        .await?;
        self.send_line("").await?;
        self.send("  Restart the server? (Y/N) ").await?;
        self.flush().await?;

        let input = match self.get_menu_input(false).await? {
            Some(s) if !s.is_empty() => s,
            _ => return Ok(()),
        };

        if input == "y" {
            self.send_line("").await?;
            self.send_line(&format!(
                "  {}",
                self.yellow("Restarting server...")
            ))
            .await?;
            self.flush().await?;
            self.restart.store(true, Ordering::SeqCst);
            self.shutdown.store(true, Ordering::SeqCst);
        }
        Ok(())
    }

    async fn config_reset_defaults(&mut self) -> Result<(), std::io::Error> {
        self.send_line("").await?;
        self.send_line(&format!(
            "  {}",
            self.red("WARNING: This will reset ALL")
        ))
        .await?;
        self.send_line(&format!(
            "  {}",
            self.red("settings to factory defaults.")
        ))
        .await?;
        self.send_line(&format!(
            "  {}",
            self.red("The API key will be cleared.")
        ))
        .await?;
        self.send_line("").await?;
        self.send("  Reset all settings? (Y/N) ").await?;
        self.flush().await?;

        let input = match self.get_menu_input(false).await? {
            Some(s) if !s.is_empty() => s,
            _ => return Ok(()),
        };

        if input == "y" {
            let defaults = config::Config::default();
            let saved = tokio::task::spawn_blocking(move || config::save_config(&defaults))
                .await
                .unwrap_or_else(|e| Err(format!("save task panicked: {e}")));
            self.send_line("").await?;
            match saved {
                Ok(()) => {
                    self.send_line(&format!(
                        "  {}",
                        self.green("All settings reset to defaults.")
                    ))
                    .await?;
                }
                Err(e) => {
                    self.send_line(&format!(
                        "  {}",
                        self.amber(&format!("Reset applied in memory but NOT saved: {}", e))
                    ))
                    .await?;
                }
            }
            self.config_restart_notice().await?;
        }
        Ok(())
    }

    async fn config_show_help(&mut self) -> Result<(), std::io::Error> {
        let lines = Self::config_help_lines(self.terminal_type == TerminalType::Petscii);
        self.show_help_page("SERVER CONFIGURATION HELP", lines).await
    }

    /// Server-configuration settings help, split by terminal width.  Associated
    /// fn so a unit test asserts the REAL lines fit 40 cols (see `punter_help_lines`).
    fn config_help_lines(petscii: bool) -> &'static [&'static str] {
        if petscii {
            &[
                "  Change settings for THIS server.",
                "",
                "  T  Enable or disable the telnet",
                "     server listener",
                "  P  Change the telnet port",
                "  S  Enable or disable the SSH",
                "     server listener",
                "  O  Change the SSH port",
                "  K  Toggle the standalone Kermit",
                "     server listener (bypasses",
                "     auth and private-IP filter)",
                "  J  Change the Kermit server port",
                "  W  Toggle the configuration web",
                "     server (HTTP/Basic auth on",
                "     the same login as telnet)",
                "  B  Change the web server port",
                "  I  Toggle IP safety. When OFF,",
                "     telnet accepts every source",
                "     IP (no private-IP filter).",
                "     Effective immediately.",
                "  R  Restart the server",
                "  C  Set the max concurrent",
                "     sessions (1 or more)",
                "  D  Set the idle-disconnect",
                "     timeout in seconds; 0 keeps",
                "     sessions open indefinitely",
                "  M  Master/Slave settings (relay",
                "     serial ports to/from another",
                "     gateway over SSH)",
                "",
                "  Most changes are saved at once",
                "  but require a server restart;",
                "  IP safety applies immediately.",
            ]
        } else {
            &[
                "  Change settings for THIS server.",
                "",
                "  T  Enable or disable the telnet server",
                "  P  Change the telnet listening port",
                "  S  Enable or disable the SSH server",
                "  O  Change the SSH listening port",
                "  K  Toggle the standalone Kermit server",
                "     (bypasses auth and the private-IP filter)",
                "  J  Change the Kermit server listening port",
                "  W  Toggle the configuration web server.  Renders",
                "     the same settings page the GUI does in a",
                "     browser; uses the unified login credentials",
                "     under Security when login is required.",
                "  B  Change the web server listening port",
                "  I  Toggle IP safety. When ON (default), and",
                "     login is not required, the telnet listener",
                "     only accepts private/loopback addresses",
                "     and rejects *.*.*.1 gateways. When OFF, every",
                "     source IP is accepted. Takes effect on the",
                "     next inbound connection (no restart needed).",
                "  R  Restart the server now",
                "  C  Set the maximum number of concurrent sessions",
                "     (1 or more)",
                "  D  Set the idle-disconnect timeout in seconds; 0",
                "     keeps idle sessions connected indefinitely",
                "  M  Master/Slave settings (relay serial ports",
                "     to/from another gateway over SSH)",
                "",
                "  Most changes are saved to the config file",
                "  immediately but require a server restart to",
                "  take effect; IP safety is the exception and",
                "  applies on the next connection.",
            ]
        }
    }

    // ─── FILE TRANSFER SETTINGS ─────────────────────────────
    //
    // Top-level submenu under Configuration > File Transfer.  Holds
    // the shared transfer-directory setting plus a per-protocol
    // selector that drills into XMODEM / YMODEM / ZMODEM settings
    // pages.  Each protocol page edits only the keys that apply to
    // that protocol; XMODEM and YMODEM share the `xmodem_*` keys
    // because they share a single protocol code path in `xmodem.rs`.

    async fn file_transfer_settings(&mut self) -> Result<(), std::io::Error> {
        loop {
            let cfg = config::get_config();

            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            self.send_line(&format!("  {}", self.yellow("FILE TRANSFER")))
                .await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;

            self.send_line(&format!(
                "  Transfer dir:  {}",
                self.amber(&cfg.transfer_dir)
            ))
            .await?;
            self.send_line("").await?;

            self.send_line(&format!(
                "  {}  Change transfer directory",
                self.cyan("D")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  XMODEM settings",
                self.cyan("X")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  YMODEM settings",
                self.cyan("Y")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  ZMODEM settings",
                self.cyan("Z")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  KERMIT settings",
                self.cyan("K")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  PUNTER settings",
                self.cyan("P")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Restart server",
                self.cyan("R")
            ))
            .await?;
            self.send_line("").await?;
            self.send_line(&format!(
                "  {}  {}",
                self.action_prompt("Q", "Back"),
                self.action_prompt("H", "Help")
            ))
            .await?;

            let prompt = format!("{}> ", self.cyan("ethernet/config/xfer"));
            self.send(&prompt).await?;
            self.flush().await?;

            let input = match self.get_menu_input(false).await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(()),
            };

            match input.as_str() {
                "d" => {
                    self.xmodem_set_dir(&cfg.transfer_dir).await?;
                }
                "x" => {
                    self.xmodem_settings().await?;
                }
                "y" => {
                    self.ymodem_settings().await?;
                }
                "z" => {
                    self.zmodem_settings().await?;
                }
                "k" => {
                    self.kermit_settings().await?;
                }
                "p" => {
                    self.punter_settings().await?;
                }
                "r" => {
                    self.config_restart_server().await?;
                }
                "h" => {
                    self.file_transfer_show_help().await?;
                }
                "q" => return Ok(()),
                _ => {
                    self.show_error("Press D, X, Y, Z, K, P, R, H, or Q.").await?;
                }
            }
        }
    }

    async fn file_transfer_show_help(&mut self) -> Result<(), std::io::Error> {
        let lines = Self::file_transfer_help_lines(self.terminal_type == TerminalType::Petscii);
        self.show_help_page("FILE TRANSFER HELP", lines).await
    }

    /// File-transfer settings help, split by terminal width.  Associated fn so a
    /// unit test asserts the REAL lines fit 40 cols (see `punter_help_lines`).
    fn file_transfer_help_lines(petscii: bool) -> &'static [&'static str] {
        if petscii {
            &[
                "  Configure file-transfer options.",
                "",
                "  D  Transfer directory: where",
                "     uploads land and downloads",
                "     are served from",
                "  X  XMODEM settings",
                "  Y  YMODEM settings",
                "  Z  ZMODEM settings",
                "  K  KERMIT settings",
                "  P  PUNTER settings",
                "  R  Restart the server",
                "",
                "  XMODEM, XMODEM-1K, and YMODEM",
                "  share the same timeouts.",
                "  ZMODEM, Kermit, and Punter",
                "  each have their own.",
            ]
        } else {
            &[
                "  Configure file-transfer options.",
                "",
                "  D  Transfer directory: where uploads",
                "     land and downloads are served from",
                "  X  XMODEM settings (XMODEM + XMODEM-1K)",
                "  Y  YMODEM settings (shared with XMODEM)",
                "  Z  ZMODEM settings",
                "  K  KERMIT settings",
                "  P  PUNTER settings",
                "  R  Restart the server",
                "",
                "  XMODEM, XMODEM-1K, and YMODEM share",
                "  the same timeouts because they share",
                "  the same protocol code path. ZMODEM,",
                "  Kermit, and Punter each have their own",
                "  independent tunables.",
            ]
        }
    }

    // ─── XMODEM SETTINGS ────────────────────────────────────
    //
    // These settings also govern XMODEM-1K and YMODEM because all
    // three protocols share the same `xmodem_*` config keys and the
    // same send/receive code path in `xmodem.rs`.

    async fn xmodem_settings(&mut self) -> Result<(), std::io::Error> {
        self.xmodem_family_settings(
            "XMODEM SETTINGS",
            "ethernet/config/xfer/xmodem",
            "XMODEM family",
        )
        .await
    }

    async fn ymodem_settings(&mut self) -> Result<(), std::io::Error> {
        self.xmodem_family_settings(
            "YMODEM SETTINGS",
            "ethernet/config/xfer/ymodem",
            "XMODEM family (shared)",
        )
        .await
    }

    /// Shared renderer for the XMODEM / YMODEM settings pages.  Both
    /// protocols edit the same `xmodem_*` config keys, so the page
    /// differs only in its heading and breadcrumb.  A note under the
    /// status block calls out the shared-family behavior so operators
    /// aren't surprised when editing either page changes the other.
    async fn xmodem_family_settings(
        &mut self,
        header: &str,
        breadcrumb: &str,
        applies_to: &str,
    ) -> Result<(), std::io::Error> {
        loop {
            let cfg = config::get_config();

            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            self.send_line(&format!("  {}", self.yellow(header))).await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;

            self.send_line(&format!(
                "  Negotiate:      {} s",
                self.amber(&cfg.xmodem_negotiation_timeout.to_string())
            ))
            .await?;
            self.send_line(&format!(
                "  Retry interval: {} s",
                self.amber(&cfg.xmodem_negotiation_retry_interval.to_string())
            ))
            .await?;
            self.send_line(&format!(
                "  Block timeout:  {} s",
                self.amber(&cfg.xmodem_block_timeout.to_string())
            ))
            .await?;
            self.send_line(&format!(
                "  Max retries:    {}",
                self.amber(&cfg.xmodem_max_retries.to_string())
            ))
            .await?;
            self.send_line(&format!("  Applies to:     {}", self.dim(applies_to)))
                .await?;
            self.send_line("").await?;

            self.send_line(&format!(
                "  {}  Set negotiation timeout",
                self.cyan("N")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Set retry interval",
                self.cyan("I")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Set block timeout",
                self.cyan("B")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Set max retries",
                self.cyan("M")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Restart server",
                self.cyan("R")
            ))
            .await?;
            self.send_line("").await?;
            self.send_line(&format!(
                "  {}  {}",
                self.action_prompt("Q", "Back"),
                self.action_prompt("H", "Help")
            ))
            .await?;

            let prompt = format!("{}> ", self.cyan(breadcrumb));
            self.send(&prompt).await?;
            self.flush().await?;

            let input = match self.get_menu_input(false).await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(()),
            };

            match input.as_str() {
                "n" => {
                    self.xmodem_set_numeric(
                        "Negotiation timeout",
                        "xmodem_negotiation_timeout",
                        cfg.xmodem_negotiation_timeout,
                        1,
                        300,
                        "seconds",
                    )
                    .await?;
                }
                "i" => {
                    self.xmodem_set_numeric(
                        "Retry interval",
                        "xmodem_negotiation_retry_interval",
                        cfg.xmodem_negotiation_retry_interval,
                        1,
                        60,
                        "seconds",
                    )
                    .await?;
                }
                "b" => {
                    self.xmodem_set_numeric(
                        "Block timeout",
                        "xmodem_block_timeout",
                        cfg.xmodem_block_timeout,
                        1,
                        120,
                        "seconds",
                    )
                    .await?;
                }
                "m" => {
                    self.xmodem_set_numeric(
                        "Max retries",
                        "xmodem_max_retries",
                        cfg.xmodem_max_retries as u64,
                        1,
                        100,
                        "retries",
                    )
                    .await?;
                }
                "r" => {
                    self.config_restart_server().await?;
                }
                "h" => {
                    self.xmodem_show_help().await?;
                }
                "q" => return Ok(()),
                _ => {
                    self.show_error("Press N, I, B, M, R, H, or Q.").await?;
                }
            }
        }
    }

    // ─── ZMODEM SETTINGS ────────────────────────────────────

    async fn zmodem_settings(&mut self) -> Result<(), std::io::Error> {
        loop {
            let cfg = config::get_config();

            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            self.send_line(&format!("  {}", self.yellow("ZMODEM SETTINGS")))
                .await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;

            self.send_line(&format!(
                "  Negotiate:      {} s",
                self.amber(&cfg.zmodem_negotiation_timeout.to_string())
            ))
            .await?;
            self.send_line(&format!(
                "  Retry interval: {} s",
                self.amber(&cfg.zmodem_negotiation_retry_interval.to_string())
            ))
            .await?;
            self.send_line(&format!(
                "  Frame timeout:  {} s",
                self.amber(&cfg.zmodem_frame_timeout.to_string())
            ))
            .await?;
            self.send_line(&format!(
                "  Max retries:    {}",
                self.amber(&cfg.zmodem_max_retries.to_string())
            ))
            .await?;
            self.send_line("").await?;

            self.send_line(&format!(
                "  {}  Set negotiation timeout",
                self.cyan("N")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Set retry interval",
                self.cyan("I")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Set frame timeout",
                self.cyan("F")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Set max retries",
                self.cyan("M")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Restart server",
                self.cyan("R")
            ))
            .await?;
            self.send_line("").await?;
            self.send_line(&format!(
                "  {}  {}",
                self.action_prompt("Q", "Back"),
                self.action_prompt("H", "Help")
            ))
            .await?;

            let prompt = format!("{}> ", self.cyan("ethernet/config/xfer/zmodem"));
            self.send(&prompt).await?;
            self.flush().await?;

            let input = match self.get_menu_input(false).await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(()),
            };

            match input.as_str() {
                "n" => {
                    self.xmodem_set_numeric(
                        "Negotiation timeout",
                        "zmodem_negotiation_timeout",
                        cfg.zmodem_negotiation_timeout,
                        1,
                        300,
                        "seconds",
                    )
                    .await?;
                }
                "i" => {
                    self.xmodem_set_numeric(
                        "Retry interval",
                        "zmodem_negotiation_retry_interval",
                        cfg.zmodem_negotiation_retry_interval,
                        1,
                        60,
                        "seconds",
                    )
                    .await?;
                }
                "f" => {
                    self.xmodem_set_numeric(
                        "Frame timeout",
                        "zmodem_frame_timeout",
                        cfg.zmodem_frame_timeout,
                        1,
                        120,
                        "seconds",
                    )
                    .await?;
                }
                "m" => {
                    self.xmodem_set_numeric(
                        "Max retries",
                        "zmodem_max_retries",
                        cfg.zmodem_max_retries as u64,
                        1,
                        100,
                        "retries",
                    )
                    .await?;
                }
                "r" => {
                    self.config_restart_server().await?;
                }
                "h" => {
                    self.zmodem_show_help().await?;
                }
                "q" => return Ok(()),
                _ => {
                    self.show_error("Press N, I, F, M, R, H, or Q.").await?;
                }
            }
        }
    }

    async fn zmodem_show_help(&mut self) -> Result<(), std::io::Error> {
        let lines = Self::zmodem_help_lines(self.terminal_type == TerminalType::Petscii);
        self.show_help_page("ZMODEM SETTINGS HELP", lines).await
    }

    /// ZMODEM settings help, split by terminal width.  Associated fn so a unit
    /// test asserts the REAL lines fit 40 columns (see `punter_help_lines`).
    fn zmodem_help_lines(petscii: bool) -> &'static [&'static str] {
        if petscii {
            &[
                "  Configure ZMODEM file transfer",
                "  settings.",
                "",
                "  N  Negotiation timeout: how",
                "     long to wait for ZRQINIT /",
                "     ZRINIT handshake",
                "  I  Retry interval: ZRINIT/",
                "     ZRQINIT re-send gap (def 5)",
                "  F  Frame timeout: per-frame",
                "     read timeout in transfer",
                "  M  Max retries for ZRQINIT /",
                "     ZRPOS / ZDATA frames",
                "  R  Restart the server",
                "",
                "  Takes effect on next transfer.",
            ]
        } else {
            &[
                "  Configure ZMODEM file transfer",
                "  settings.",
                "",
                "  N  Negotiation timeout: how long to",
                "     wait for the ZRQINIT / ZRINIT",
                "     handshake",
                "  I  Retry interval: seconds between",
                "     ZRINIT / ZRQINIT re-sends (def 5)",
                "  F  Frame timeout: per-frame read",
                "     timeout once a transfer is live",
                "  M  Max retries: retry cap for ZRQINIT,",
                "     ZRPOS, and ZDATA frames",
                "  R  Restart the server",
                "",
                "  Takes effect on next transfer.",
            ]
        }
    }

    // ─── PUNTER SETTINGS ────────────────────────────────────

    async fn punter_settings(&mut self) -> Result<(), std::io::Error> {
        loop {
            let cfg = config::get_config();

            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            self.send_line(&format!("  {}", self.yellow("PUNTER SETTINGS")))
                .await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;

            self.send_line(&format!(
                "  Block size:     {} bytes",
                self.amber(&cfg.punter_block_size.to_string())
            ))
            .await?;
            self.send_line(&format!(
                "  Negotiate:      {} s",
                self.amber(&cfg.punter_negotiation_timeout.to_string())
            ))
            .await?;
            self.send_line(&format!(
                "  Retry interval: {} s",
                self.amber(&cfg.punter_negotiation_retry_interval.to_string())
            ))
            .await?;
            self.send_line(&format!(
                "  Block timeout:  {} s",
                self.amber(&cfg.punter_block_timeout.to_string())
            ))
            .await?;
            self.send_line(&format!(
                "  Max retries:    {}",
                self.amber(&cfg.punter_max_retries.to_string())
            ))
            .await?;
            self.send_line(&format!(
                "  Bad-blk limit:  {} rounds",
                self.amber(&cfg.punter_max_bad_rounds.to_string())
            ))
            .await?;
            self.send_line("").await?;

            self.send_line(&format!(
                "  {}  Set block size",
                self.cyan("B")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Set negotiation timeout",
                self.cyan("N")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Set retry interval",
                self.cyan("I")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Set block timeout",
                self.cyan("F")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Set max retries",
                self.cyan("M")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Set bad-block limit",
                self.cyan("G")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Hangup on fail: {}",
                self.cyan("D"),
                self.amber(if cfg.punter_hangup_on_failure { "on" } else { "off" })
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Restart server",
                self.cyan("R")
            ))
            .await?;
            self.send_line("").await?;
            self.send_line(&format!(
                "  {}  {}",
                self.action_prompt("Q", "Back"),
                self.action_prompt("H", "Help")
            ))
            .await?;

            let prompt = format!("{}> ", self.cyan("ethernet/config/xfer/punter"));
            self.send(&prompt).await?;
            self.flush().await?;

            let input = match self.get_menu_input(false).await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(()),
            };

            match input.as_str() {
                "b" => {
                    self.xmodem_set_numeric(
                        "Block size",
                        "punter_block_size",
                        cfg.punter_block_size as u64,
                        8,
                        255,
                        "bytes",
                    )
                    .await?;
                }
                "n" => {
                    self.xmodem_set_numeric(
                        "Negotiation timeout",
                        "punter_negotiation_timeout",
                        cfg.punter_negotiation_timeout,
                        1,
                        300,
                        "seconds",
                    )
                    .await?;
                }
                "i" => {
                    self.xmodem_set_numeric(
                        "Retry interval",
                        "punter_negotiation_retry_interval",
                        cfg.punter_negotiation_retry_interval,
                        1,
                        60,
                        "seconds",
                    )
                    .await?;
                }
                "f" => {
                    self.xmodem_set_numeric(
                        "Block timeout",
                        "punter_block_timeout",
                        cfg.punter_block_timeout,
                        1,
                        120,
                        "seconds",
                    )
                    .await?;
                }
                "m" => {
                    self.xmodem_set_numeric(
                        "Max retries",
                        "punter_max_retries",
                        cfg.punter_max_retries as u64,
                        1,
                        100,
                        "retries",
                    )
                    .await?;
                }
                "g" => {
                    self.xmodem_set_numeric(
                        "Bad-block limit",
                        "punter_max_bad_rounds",
                        cfg.punter_max_bad_rounds as u64,
                        1,
                        1000,
                        "rounds",
                    )
                    .await?;
                }
                "d" => {
                    // Shared generic bool-toggle helper (despite the name).
                    self.kermit_toggle_bool(
                        "Hangup on failure",
                        "punter_hangup_on_failure",
                        cfg.punter_hangup_on_failure,
                    )
                    .await?;
                }
                "r" => {
                    self.config_restart_server().await?;
                }
                "h" => {
                    self.punter_show_help().await?;
                }
                "q" => return Ok(()),
                _ => {
                    self.show_error("Press B, N, I, F, M, G, D, R, H, or Q.").await?;
                }
            }
        }
    }

    async fn punter_show_help(&mut self) -> Result<(), std::io::Error> {
        let lines = Self::punter_help_lines(self.terminal_type == TerminalType::Petscii);
        self.show_help_page("PUNTER SETTINGS HELP", lines).await
    }

    /// Punter settings help text, split by terminal width.  An associated fn
    /// (no `self`) so a unit test can assert the PETSCII variant fits 40
    /// columns against the real lines — no duplicated copy to drift from.
    fn punter_help_lines(petscii: bool) -> &'static [&'static str] {
        if petscii {
            &[
                "  Configure PUNTER (C1) file",
                "  transfer settings.  C1 is the",
                "  protocol CCGMS and Novaterm",
                "  speak on Commodore BBSes.",
                "",
                "  B  Block size in bytes (8-255).",
                "     255 = native max; lower for",
                "     noisy lines (40 floor)",
                "  N  Negotiation timeout: wait for",
                "     the peer's first code",
                "  I  Retry interval: code re-send",
                "     gap during negotiation",
                "  F  Block timeout: per-block read",
                "     timeout in transfer",
                "  M  Max retries per code / block",
                "  G  Bad-block limit: how many",
                "     corrupt-block resends before",
                "     giving up (vs M, per-code)",
                "  D  Hang up on failure: drop",
                "     carrier so a stranded C64",
                "     exits (C1 has no abort)",
                "  R  Restart the server",
                "",
                "  Takes effect on next transfer.",
            ]
        } else {
            &[
                "  Configure PUNTER (C1) file transfer",
                "  settings.  C1 is the protocol CCGMS /",
                "  Novaterm speak on Commodore BBSes.",
                "",
                "  B  Block size in bytes (8-255). 255 is",
                "     the native max; lower it toward 40",
                "     for noisy lines",
                "  N  Negotiation timeout: how long to",
                "     wait for the peer's first code",
                "  I  Retry interval: seconds between",
                "     handshake-code re-sends",
                "  F  Block timeout: per-block read",
                "     timeout once a transfer is live",
                "  M  Max retries: retry cap per code / block",
                "  G  Bad-block limit: consecutive corrupt-block",
                "     resends tolerated before giving up (kept higher",
                "     than M; a real C64 peer never caps these, so a",
                "     low value makes the gateway quit and strand it)",
                "  D  Hang up on failure: drop carrier when a transfer",
                "     gives up so a stranded C64 exits (C1 has no",
                "     in-band abort). Ends the whole session.",
                "  R  Restart the server",
                "",
                "  Takes effect on next transfer.",
            ]
        }
    }

    // ─── KERMIT SETTINGS ────────────────────────────────────
    //
    // Kermit has the largest configuration surface of any of the
    // file-transfer protocols.  We split it across three pages of
    // status (timeouts/retries, packet/window/check, capability bits)
    // since not all of it fits in PETSCII's 22 rows.

    /// Top-level Kermit settings entry point.  The screen is split into
    /// two pages so each fits within the PETSCII 22-row × 40-col budget:
    /// a read-only Status page and an editable Settings menu.  `M` on
    /// the Status page jumps to Settings; `V` on the Settings menu jumps
    /// back to Status; `Q` on either exits to File Transfer.
    async fn kermit_settings(&mut self) -> Result<(), std::io::Error> {
        let mut on_status = true;
        loop {
            let nav = if on_status {
                self.kermit_status_page().await?
            } else {
                self.kermit_settings_menu_page().await?
            };
            match nav {
                KermitPageNav::Switch => on_status = !on_status,
                KermitPageNav::Back => return Ok(()),
            }
        }
    }

    /// Render the read-only Kermit status page.  Returns `Switch` when
    /// the operator presses `M` (jump to the editable Settings menu),
    /// `Back` on `Q`.  `H` shows help and re-renders.  Designed to fit
    /// PETSCII 22×40 with all values at their realistic max widths
    /// (5-digit timeouts, 4-digit max-packet, 2-digit window, etc.).
    async fn kermit_status_page(&mut self) -> Result<KermitPageNav, std::io::Error> {
        loop {
            let cfg = config::get_config();
            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            self.send_line(&format!("  {}", self.yellow("KERMIT STATUS")))
                .await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;

            let idle_display = if cfg.kermit_idle_timeout == 0 {
                "off".to_string()
            } else {
                format!("{} s", cfg.kermit_idle_timeout)
            };
            self.send_line(&format!(
                "  Negotiate: {} s",
                self.amber(&cfg.kermit_negotiation_timeout.to_string())
            ))
            .await?;
            self.send_line(&format!(
                "  Packet: {} s    Retries: {}",
                self.amber(&cfg.kermit_packet_timeout.to_string()),
                self.amber(&cfg.kermit_max_retries.to_string()),
            ))
            .await?;
            self.send_line(&format!(
                "  Idle: {}",
                self.amber(&idle_display)
            ))
            .await?;
            self.send_line(&format!(
                "  Max packet: {}   Window: {}",
                self.amber(&cfg.kermit_max_packet_length.to_string()),
                self.amber(&cfg.kermit_window_size.to_string()),
            ))
            .await?;
            self.send_line(&format!(
                "  Block check: {}    Long: {}",
                self.amber(&cfg.kermit_block_check_type.to_string()),
                self.amber(if cfg.kermit_long_packets { "on" } else { "off" }),
            ))
            .await?;
            self.send_line(&format!(
                "  Sliding: {}    Streaming: {}",
                self.amber(if cfg.kermit_sliding_windows { "on" } else { "off" }),
                self.amber(if cfg.kermit_streaming { "on" } else { "off" }),
            ))
            .await?;
            self.send_line(&format!(
                "  Attributes: {}    Repeat: {}",
                self.amber(if cfg.kermit_attribute_packets { "on" } else { "off" }),
                self.amber(if cfg.kermit_repeat_compression { "on" } else { "off" }),
            ))
            .await?;
            self.send_line(&format!(
                "  8-bit quote: {}",
                self.amber(&cfg.kermit_8bit_quote)
            ))
            .await?;
            self.send_line(&format!(
                "  Locking: {}    Resume: {}",
                self.amber(if cfg.kermit_locking_shifts { "on" } else { "off" }),
                self.amber(if cfg.kermit_resume_partial { "on" } else { "off" }),
            ))
            .await?;
            self.send_line(&format!(
                "  Resume age: {} h",
                self.amber(&cfg.kermit_resume_max_age_hours.to_string())
            ))
            .await?;
            self.send_line(&format!(
                "  Wait for rx: {}",
                self.amber(if cfg.kermit_wait_for_receiver { "on" } else { "off" }),
            ))
            .await?;
            self.send_line(&format!(
                "  ATDT KERMIT: {}",
                self.amber(if cfg.allow_atdt_kermit { "enabled" } else { "disabled" })
            ))
            .await?;
            self.send_line("").await?;
            self.send_line(&format!(
                "  {}  Settings   {}  {}",
                self.cyan("M"),
                self.action_prompt("Q", "Back"),
                self.action_prompt("H", "Help")
            ))
            .await?;

            let prompt = format!("{}> ", self.cyan("ethernet/config/xfer/kermit"));
            self.send(&prompt).await?;
            self.flush().await?;

            let input = match self.get_menu_input(false).await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(KermitPageNav::Back),
            };

            match input.as_str() {
                "m" => return Ok(KermitPageNav::Switch),
                "q" => return Ok(KermitPageNav::Back),
                "h" => self.kermit_show_help().await?,
                _ => self.show_error("Press M, Q, or H.").await?,
            }
        }
    }

    /// Render the editable Kermit settings menu.  Returns `Switch` when
    /// the operator presses `V` (jump back to the Status page), `Back`
    /// on `Q`.  Action keys (N/P/X/M/W/C/L/S/T/A/E/I/8/R/K) dispatch to
    /// the same setters the original combined screen used.  Labels are
    /// abbreviated to fit PETSCII 40-col with the standard column-22
    /// two-keys-per-row alignment.
    async fn kermit_settings_menu_page(
        &mut self,
    ) -> Result<KermitPageNav, std::io::Error> {
        loop {
            let cfg = config::get_config();
            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            self.send_line(&format!("  {}", self.yellow("KERMIT SETTINGS")))
                .await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;

            self.send_line(&format!(
                "  {}  Negotiate        {}  Packet timeout",
                self.cyan("N"),
                self.cyan("P")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Max retries      {}  Max length",
                self.cyan("X"),
                self.cyan("M")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Window size      {}  Block check",
                self.cyan("W"),
                self.cyan("C")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Long packets     {}  Sliding wins",
                self.cyan("L"),
                self.cyan("S")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Streaming        {}  Attributes",
                self.cyan("T"),
                self.cyan("A")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Repeat compr     {}  Idle timeout",
                self.cyan("E"),
                self.cyan("I"),
            ))
            .await?;
            self.send_line(&format!(
                "  {}  8-bit quote      {}  Restart server",
                self.cyan("8"),
                self.cyan("R")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Locking shifts   {}  Resume uploads",
                self.cyan("F"),
                self.cyan("U")
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Resume max age   {}  Toggle ATDT KERMIT",
                self.cyan("D"),
                self.cyan("K"),
            ))
            .await?;
            self.send_line(&format!(
                "  {}  Wait for rx",
                self.cyan("G"),
            ))
            .await?;
            self.send_line("").await?;
            self.send_line(&format!(
                "  {}  Status   {}  {}",
                self.cyan("V"),
                self.action_prompt("Q", "Back"),
                self.action_prompt("H", "Help")
            ))
            .await?;

            let prompt = format!("{}> ", self.cyan("ethernet/config/xfer/kermit"));
            self.send(&prompt).await?;
            self.flush().await?;

            let input = match self.get_menu_input(false).await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(KermitPageNav::Back),
            };

            match input.as_str() {
                "v" => return Ok(KermitPageNav::Switch),
                "q" => return Ok(KermitPageNav::Back),
                "n" => {
                    self.xmodem_set_numeric(
                        "Negotiation timeout",
                        "kermit_negotiation_timeout",
                        cfg.kermit_negotiation_timeout,
                        1,
                        300,
                        "seconds",
                    )
                    .await?;
                }
                "p" => {
                    self.xmodem_set_numeric(
                        "Packet timeout",
                        "kermit_packet_timeout",
                        cfg.kermit_packet_timeout,
                        1,
                        120,
                        "seconds",
                    )
                    .await?;
                }
                "x" => {
                    self.xmodem_set_numeric(
                        "Max retries",
                        "kermit_max_retries",
                        cfg.kermit_max_retries as u64,
                        1,
                        20,
                        "retries",
                    )
                    .await?;
                }
                "m" => {
                    self.xmodem_set_numeric(
                        "Max packet length",
                        "kermit_max_packet_length",
                        cfg.kermit_max_packet_length as u64,
                        10,
                        9024,
                        "bytes",
                    )
                    .await?;
                }
                "w" => {
                    self.xmodem_set_numeric(
                        "Window size",
                        "kermit_window_size",
                        cfg.kermit_window_size as u64,
                        1,
                        31,
                        "packets",
                    )
                    .await?;
                }
                "c" => {
                    self.xmodem_set_numeric(
                        "Block check type",
                        "kermit_block_check_type",
                        cfg.kermit_block_check_type as u64,
                        1,
                        3,
                        "(1/2/3)",
                    )
                    .await?;
                }
                "l" => {
                    self.kermit_toggle_bool(
                        "Long packets",
                        "kermit_long_packets",
                        cfg.kermit_long_packets,
                    )
                    .await?;
                }
                "s" => {
                    self.kermit_toggle_bool(
                        "Sliding windows",
                        "kermit_sliding_windows",
                        cfg.kermit_sliding_windows,
                    )
                    .await?;
                }
                "t" => {
                    self.kermit_toggle_bool(
                        "Streaming",
                        "kermit_streaming",
                        cfg.kermit_streaming,
                    )
                    .await?;
                }
                "a" => {
                    self.kermit_toggle_bool(
                        "Attribute packets",
                        "kermit_attribute_packets",
                        cfg.kermit_attribute_packets,
                    )
                    .await?;
                }
                "e" => {
                    self.kermit_toggle_bool(
                        "Repeat compression",
                        "kermit_repeat_compression",
                        cfg.kermit_repeat_compression,
                    )
                    .await?;
                }
                "i" => {
                    // 0 disables; 86400 (1 day) is a generous upper
                    // bound that still bounds memory growth from any
                    // peer-supplied state we might accumulate per
                    // session.
                    self.xmodem_set_numeric(
                        "Idle timeout",
                        "kermit_idle_timeout",
                        cfg.kermit_idle_timeout,
                        0,
                        86400,
                        "seconds (0 = disabled)",
                    )
                    .await?;
                }
                "8" => {
                    let next = match cfg.kermit_8bit_quote.as_str() {
                        "auto" => "on",
                        "on" => "off",
                        _ => "auto",
                    };
                    let key = "kermit_8bit_quote".to_string();
                    let v = next.to_string();
                    tokio::task::spawn_blocking(move || {
                        config::update_config_value(&key, &v);
                    })
                    .await
                    .ok();
                    self.send_line("").await?;
                    self.send_line(&format!(
                        "  {}",
                        self.green(&format!("8-bit quote set to {}.", next))
                    ))
                    .await?;
                    self.send_line("").await?;
                    self.send("  Press any key to continue.").await?;
                    self.flush().await?;
                    self.wait_for_key().await?;
                }
                "r" => {
                    self.config_restart_server().await?;
                }
                "k" => {
                    self.kermit_toggle_atdt_kermit(cfg.allow_atdt_kermit).await?;
                }
                "f" => {
                    self.kermit_toggle_bool(
                        "Locking shifts",
                        "kermit_locking_shifts",
                        cfg.kermit_locking_shifts,
                    )
                    .await?;
                }
                "u" => {
                    self.kermit_toggle_bool(
                        "Resume partial uploads",
                        "kermit_resume_partial",
                        cfg.kermit_resume_partial,
                    )
                    .await?;
                }
                "g" => {
                    self.kermit_toggle_bool(
                        "Wait for receiver NAK on download",
                        "kermit_wait_for_receiver",
                        cfg.kermit_wait_for_receiver,
                    )
                    .await?;
                }
                "d" => {
                    self.xmodem_set_numeric(
                        "Resume max age",
                        "kermit_resume_max_age_hours",
                        cfg.kermit_resume_max_age_hours as u64,
                        1,
                        8760,
                        "hours",
                    )
                    .await?;
                }
                "h" => {
                    self.kermit_show_help().await?;
                }
                _ => {
                    self.show_error("Press a listed key, V, R, K, H, or Q.")
                        .await?;
                }
            }
        }
    }

    /// Toggle `allow_atdt_kermit`.  When enabling, show a full-screen
    /// security warning and prompt for explicit Y/N confirmation —
    /// flipping this on lets serial callers reach Kermit server mode
    /// without going through the telnet auth gate, so we want the
    /// operator's intent on the record.  Disabling is one-click safe
    /// (no popup): tightening security never needs a confirmation.
    /// On confirmation (or unconditional disable), persist immediately
    /// via `update_config_value` so the change takes effect for the
    /// next ATDT without a server restart.
    async fn kermit_toggle_atdt_kermit(
        &mut self,
        currently_enabled: bool,
    ) -> Result<(), std::io::Error> {
        if currently_enabled {
            // Disable path — no confirmation needed.
            tokio::task::spawn_blocking(move || {
                config::update_config_value("allow_atdt_kermit", "false");
            })
            .await
            .ok();
            self.send_line("").await?;
            self.send_line(&format!(
                "  {}",
                self.green("ATDT KERMIT disabled.")
            ))
            .await?;
            self.send_line("").await?;
            self.send("  Press any key to continue.").await?;
            self.flush().await?;
            self.wait_for_key().await?;
            return Ok(());
        }

        // Enable path — full-screen warning, Y/N prompt.
        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;
        self.send_line(&format!(
            "  {}",
            self.yellow("ENABLE ATDT KERMIT — SECURITY WARNING")
        ))
        .await?;
        self.send_line(&sep).await?;
        self.send_line("").await?;
        self.send_line(&format!(
            "  {}",
            self.amber("This bypasses telnet authentication.")
        ))
        .await?;
        self.send_line("").await?;
        self.send_line(
            "  When enabled, anyone who can reach a serial port",
        )
        .await?;
        self.send_line(
            "  can dial ATDT KERMIT and land directly in Kermit",
        )
        .await?;
        self.send_line(
            "  server mode — no username, no password, no menu.",
        )
        .await?;
        self.send_line("").await?;
        self.send_line(
            "  If your gateway has security_enabled = true and you",
        )
        .await?;
        self.send_line(
            "  need every caller to authenticate, leave this OFF",
        )
        .await?;
        self.send_line(&format!(
            "  and have callers go via the {} menu's {} entry",
            self.cyan("File Transfer"),
            self.cyan("K")
        ))
        .await?;
        self.send_line(&format!(
            "  (main menu {} then {}) — that path runs the auth",
            self.cyan("F"),
            self.cyan("K")
        ))
        .await?;
        self.send_line("  prompt before handing off to Kermit.").await?;
        self.send_line("").await?;
        self.send_line(
            "  Enable only when the serial line itself is trusted",
        )
        .await?;
        self.send_line(
            "  (private cable, isolated lab, single-user setup).",
        )
        .await?;
        self.send_line("").await?;
        self.send(&format!(
            "  Enable ATDT KERMIT? ({}/{}): ",
            self.cyan("Y"),
            self.cyan("N")
        ))
        .await?;
        self.flush().await?;

        let answer = self.get_menu_input(false).await?;
        let confirmed = matches!(
            answer.as_deref().map(|s| s.trim()),
            Some("y") | Some("Y")
        );

        self.send_line("").await?;
        if confirmed {
            tokio::task::spawn_blocking(move || {
                config::update_config_value("allow_atdt_kermit", "true");
            })
            .await
            .ok();
            self.send_line(&format!(
                "  {}",
                self.green("ATDT KERMIT enabled.")
            ))
            .await?;
        } else {
            self.send_line(&format!(
                "  {}",
                self.dim("ATDT KERMIT left disabled.")
            ))
            .await?;
        }
        self.send_line("").await?;
        self.send("  Press any key to continue.").await?;
        self.flush().await?;
        self.wait_for_key().await?;
        Ok(())
    }

    /// Helper: flip a Kermit boolean config key, persist, and confirm.
    async fn kermit_toggle_bool(
        &mut self,
        label: &str,
        key: &str,
        current: bool,
    ) -> Result<(), std::io::Error> {
        let next = !current;
        let k = key.to_string();
        let v = next.to_string();
        tokio::task::spawn_blocking(move || {
            config::update_config_value(&k, &v);
        })
        .await
        .ok();
        self.send_line("").await?;
        self.send_line(&format!(
            "  {}",
            self.green(&format!(
                "{} {}.",
                label,
                if next { "enabled" } else { "disabled" }
            ))
        ))
        .await?;
        self.send_line("").await?;
        self.send("  Press any key to continue.").await?;
        self.flush().await?;
        self.wait_for_key().await?;
        Ok(())
    }

    async fn kermit_show_help(&mut self) -> Result<(), std::io::Error> {
        let lines = Self::kermit_help_lines(self.terminal_type == TerminalType::Petscii);
        self.show_help_page("KERMIT SETTINGS HELP", lines).await
    }

    /// Kermit settings help, split by terminal width.  Associated fn so a unit
    /// test asserts the REAL lines fit 40 columns (see `punter_help_lines`).
    fn kermit_help_lines(petscii: bool) -> &'static [&'static str] {
        if petscii {
            &[
                "  Configure Kermit transfer",
                "  parameters.  Negotiated with",
                "  the peer at session start.",
                "",
                "  N  Negotiate timeout (45 s)",
                "  P  Per-packet timeout",
                "  X  Max retries per packet",
                "  M  Max packet length",
                "  W  Sliding window size",
                "  C  Block check type 1/2/3",
                "  L/S/T/A/E/I  toggles",
                "  8  cycle 8-bit quote mode",
                "  F  locking-shift toggle",
                "  U  resume partial uploads",
                "  D  resume max age (hours)",
                "  K  ATDT KERMIT toggle",
                "     (bypasses security)",
                "",
                "  Streaming auto-degrades to",
                "  sliding/stop-and-wait when",
                "  the peer can't do it.",
            ]
        } else {
            &[
                "  Configure Kermit transfer parameters.",
                "  These are advertised in our Send-Init;",
                "  the peer's response narrows the session",
                "  to the intersection of capabilities.",
                "",
                "  N  Negotiate timeout (Send-Init handshake)",
                "  P  Per-packet read timeout",
                "  X  Max retries per packet (NAK / timeout)",
                "  M  Max packet length we'll advertise",
                "  W  Sliding-window size (1=stop-and-wait)",
                "  C  Block check type: 1=6-bit, 2=12-bit, 3=CRC-16",
                "  L  Long-packet capability",
                "  S  Sliding-window capability",
                "  T  Streaming capability",
                "  A  Attribute-packet capability",
                "  E  Repeat-count compression",
                "  I  Telnet IAC escape during transfer",
                "  8  8-bit quote: auto / on / off",
                "  F  Locking-shift (SO/SI) capability for",
                "     8-bit data over 7-bit links",
                "  U  Resume partial uploads (disposition R):",
                "     append to a matching on-disk partial",
                "  D  Resume max age in hours: ignore on-disk",
                "     partials older than this when resuming",
                "  K  Allow ATDT KERMIT from either serial",
                "     port's modem (bypasses security_enabled",
                "     auth gate; prompts for explicit Y/N",
                "     before enabling)",
                "",
                "  Streaming requires a reliable transport.",
                "  Disable when bridging to flaky serial.",
            ]
        }
    }

    async fn xmodem_set_dir(&mut self, current: &str) -> Result<(), std::io::Error> {
        self.send_line("").await?;
        self.send_line(&format!(
            "  Current directory: {}",
            self.amber(current)
        ))
        .await?;
        self.send("  New directory: ").await?;
        self.flush().await?;

        let input = match self.get_line_input().await? {
            Some(s) if !s.is_empty() => s,
            _ => return Ok(()),
        };

        let v = input.clone();
        tokio::task::spawn_blocking(move || {
            config::update_config_value("transfer_dir", &v);
        })
        .await
        .ok();
        self.send_line("").await?;
        self.send_line(&format!(
            "  {}",
            self.green(&format!("Transfer dir set to: {}", input))
        ))
        .await?;
        self.send_line("").await?;
        self.send("  Press any key to continue.").await?;
        self.flush().await?;
        self.wait_for_key().await?;
        Ok(())
    }

    async fn xmodem_set_numeric(
        &mut self,
        label: &str,
        key: &str,
        current: u64,
        min: u64,
        max: u64,
        unit: &str,
    ) -> Result<(), std::io::Error> {
        self.send_line("").await?;
        self.send_line(&format!(
            "  Current {}: {}",
            label.to_lowercase(),
            self.amber(&current.to_string())
        ))
        .await?;
        self.send(&format!("  New value ({}-{}): ", min, max)).await?;
        self.flush().await?;

        let input = match self.get_line_input().await? {
            Some(s) if !s.is_empty() => s,
            _ => return Ok(()),
        };

        if let Ok(val) = input.parse::<u64>() {
            if val >= min && val <= max {
                let k = key.to_string();
                let v = val.to_string();
                tokio::task::spawn_blocking(move || {
                    config::update_config_value(&k, &v);
                })
                .await
                .ok();
                self.send_line("").await?;
                self.send_line(&format!(
                    "  {}",
                    self.green(&format!("{} set to {} {}.", label, val, unit))
                ))
                .await?;
                self.send_line("").await?;
                self.send("  Press any key to continue.").await?;
                self.flush().await?;
                self.wait_for_key().await?;
            } else {
                self.show_error(&format!("Value must be {}-{}.", min, max)).await?;
            }
        } else {
            self.show_error("Invalid number.").await?;
        }
        Ok(())
    }

    async fn xmodem_show_help(&mut self) -> Result<(), std::io::Error> {
        let lines = Self::xmodem_help_lines(self.terminal_type == TerminalType::Petscii);
        self.show_help_page("XMODEM SETTINGS HELP", lines).await
    }

    /// XMODEM-family settings help, split by terminal width.  An associated fn
    /// (no `self`) so a unit test asserts the REAL lines fit 40 columns —
    /// matching `punter_help_lines`, with no duplicated copy to drift.
    fn xmodem_help_lines(petscii: bool) -> &'static [&'static str] {
        if petscii {
            &[
                "  Configure XMODEM family transfer",
                "  settings. Shared with XMODEM-1K",
                "  and YMODEM.",
                "",
                "  N  Negotiation timeout: how",
                "     long to wait for transfer",
                "     to begin",
                "  I  Retry interval: C/NAK poke",
                "     gap (spec ~10 s, def 7 s)",
                "  B  Block timeout: how long to",
                "     wait for each block",
                "  M  Max retries per block",
                "  R  Restart the server",
                "",
                "  Takes effect on next transfer.",
            ]
        } else {
            &[
                "  Configure XMODEM family transfer",
                "  settings. Shared with XMODEM-1K and",
                "  YMODEM (same protocol code path).",
                "",
                "  N  Negotiation timeout: how long to",
                "     wait for a transfer to begin",
                "  I  Retry interval: seconds between",
                "     C/NAK pokes during the handshake",
                "     (spec suggests ~10, default 7)",
                "  B  Block timeout: how long to wait",
                "     for each data block",
                "  M  Max retries: retry limit per block",
                "  R  Restart the server",
                "",
                "  Takes effect on next transfer.",
            ]
        }
    }

    // ─── TROUBLESHOOTING ────────────────────────────────────

    fn client_type_label(&self) -> &'static str {
        if self.is_relay {
            "Relay (slave)"
        } else if self.is_ssh {
            "SSH"
        } else if self.is_serial {
            "Serial modem"
        } else if self.telnet_negotiated {
            "Telnet"
        } else {
            "Raw TCP"
        }
    }

    fn terminal_type_label(&self) -> &'static str {
        match self.terminal_type {
            TerminalType::Petscii => "PETSCII",
            TerminalType::Ansi => "ANSI",
            TerminalType::Ascii => "ASCII",
        }
    }

    async fn troubleshooting(&mut self) -> Result<(), std::io::Error> {
        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;
        self.send_line(&format!(
            "  {}",
            self.yellow("CHARACTER TROUBLESHOOTING")
        ))
        .await?;
        self.send_line(&sep).await?;
        self.send_line("").await?;
        self.send_line(&format!(
            "  Client:   {}",
            self.cyan(self.client_type_label())
        ))
        .await?;
        self.send_line(&format!(
            "  Terminal: {}",
            self.cyan(self.terminal_type_label())
        ))
        .await?;
        self.send_line(&format!(
            "  IAC esc:  {}",
            self.cyan(if self.xmodem_iac { "On" } else { "Off" })
        ))
        .await?;
        self.send_line("").await?;
        self.send_line("  Press any key to see its hex value.")
            .await?;
        let esc_label = match self.terminal_type {
            TerminalType::Petscii => "<-",
            _ => "ESC",
        };
        self.send_line(&format!(
            "  Press {} twice to return to menu.",
            self.cyan(esc_label)
        ))
        .await?;
        self.send_line("").await?;
        // PETSCII width minus 1 — same auto-wrap reason as `separator()`.
        self.send_line(&self.yellow(&"-".repeat(
            if self.terminal_type == TerminalType::Petscii { PETSCII_WIDTH - 1 } else { 56 }
        )))
        .await?;
        self.send_line("").await?;
        self.flush().await?;

        let mut last_was_esc = false;

        loop {
            let byte = match self.read_byte_filtered().await? {
                Some(b) => b,
                None => return Ok(()),
            };

            let name = match byte {
                0x00 => "NUL",
                0x01 => "SOH",
                0x02 => "STX",
                0x03 => "ETX",
                0x04 => "EOT",
                0x05 => "ENQ",
                0x06 => "ACK",
                0x07 => "BEL",
                0x08 => "BS",
                0x09 => "TAB",
                0x0A => "LF",
                0x0B => "VT",
                0x0C => "FF",
                0x0D => "CR",
                0x0E => "SO",
                0x0F => "SI",
                0x10 => "DLE",
                0x11 => "DC1",
                0x12 => "DC2",
                0x13 => "DC3",
                0x14 => "DC4/C64-DEL",
                0x15 => "NAK",
                0x16 => "SYN",
                0x17 => "ETB",
                0x18 => "CAN",
                0x19 => "EM",
                0x1A => "SUB",
                0x1B => "ESC",
                0x1C => "FS",
                0x1D => "GS/C64-RIGHT",
                0x1E => "RS",
                0x1F => "US",
                0x7F => "DEL",
                0x91 => "C64-UP",
                0x93 => "C64-CLR",
                0x9D => "C64-LEFT",
                _ => "",
            };

            let display = if !name.is_empty() {
                format!("  Key: {} ({:3}) = {}",
                    self.cyan(&format!("0x{:02X}", byte)), byte, name)
            } else if (0x20..=0x7E).contains(&byte) {
                format!("  Key: {} ({:3}) = '{}'",
                    self.cyan(&format!("0x{:02X}", byte)), byte, byte as char)
            } else {
                format!("  Key: {} ({:3})",
                    self.cyan(&format!("0x{:02X}", byte)), byte)
            };
            self.send_line(&display).await?;
            self.flush().await?;

            if is_esc_key(byte, self.terminal_type == TerminalType::Petscii) {
                if last_was_esc {
                    self.send_line("").await?;
                    self.send_line("  Returning to main menu...").await?;
                    self.flush().await?;
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    return Ok(());
                }
                last_was_esc = true;
            } else {
                last_was_esc = false;
            }
        }
    }

    // ─── WEB BROWSER ────────────────────────────────────────

    const WEB_MAX_HISTORY: usize = 50;

    /// Number of content lines per page.
    /// Total screen budget is 22 rows: header (sep + title + sep = 3) +
    /// content + blank (1) + footer (position + url + nav1 + nav2 = 4) + prompt (1) = 9 overhead.
    /// 22 - 9 = 13 content lines.
    const WEB_PAGE_HEIGHT: usize = 13;

    /// Content width for HTML rendering.
    /// Slightly narrower than the display to leave room for link number suffixes
    /// like `[12]` that are appended after html2text wraps.
    fn web_content_width(&self) -> usize {
        if self.terminal_type == TerminalType::Petscii {
            33 // 40 - 2 indent - 5 for "[NNN]"
        } else {
            73 // 80 - 2 indent - 5 for "[NNN]"
        }
    }

    async fn render_web_browser(&mut self) -> Result<(), std::io::Error> {
        // Auto-load homepage on first visit if configured
        if self.web_lines.is_empty() && self.web_url.is_none() {
            let cfg = config::get_config();
            if !cfg.browser_homepage.is_empty() {
                let url = crate::webbrowser::normalize_url(&cfg.browser_homepage);
                self.web_fetch_page(&url, false).await?;
            }
        }

        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;

        if self.web_lines.is_empty() {
            // Home screen — no page loaded
            self.send_line(&format!("  {}", self.yellow("WEB BROWSER"))).await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;
            self.send_line(&format!("  {}", self.dim("Try:"))).await?;
            self.send_line(&format!("  {}",
                self.dim("  http://telnetbible.com")
            )).await?;
            self.send_line(&format!("  {}",
                self.dim("  gopher://gopher.floodgap.com")
            )).await?;
            self.send_line("").await?;
            self.send_line(&format!("  {} {} {} {}",
                self.action_prompt("G", "Go/Search"),
                self.action_prompt("K", "Bookmarks"),
                self.action_prompt("Q", "Back"),
                self.action_prompt("H", "Help"),
            )).await?;
        } else {
            // Page view — show title + paginated content
            let title_display = match &self.web_title {
                Some(t) => {
                    let max_w = if self.terminal_type == TerminalType::Petscii { 34 } else { 52 };
                    crate::webbrowser::truncate_to_width(t, max_w)
                }
                None => "Web Browser".to_string(),
            };
            self.send_line(&format!("  {}", self.yellow(&title_display))).await?;
            self.send_line(&sep).await?;

            let page_h = Self::WEB_PAGE_HEIGHT;
            let total = self.web_lines.len();
            // Defensive clamp: never let a scroll position index past the
            // current page — guarantees the page_lines slice below can't
            // panic regardless of how web_scroll was set.
            let start = self.web_scroll.min(total.saturating_sub(1));
            let end = (start + page_h).min(total);

            let content_max = if self.terminal_type == TerminalType::Petscii {
                PETSCII_WIDTH - 2
            } else {
                78
            };
            let page_lines: Vec<String> = self.web_lines[start..end].to_vec();
            for line in &page_lines {
                let safe = crate::webbrowser::truncate_to_width(line, content_max);
                let colored = self.colorize_link_markers(&safe);
                self.send_line(&format!("  {}", colored)).await?;
            }
            self.send_line("").await?;

            // Status line
            let has_prev = start > 0;
            let has_next = end < total;
            let url_display = match &self.web_url {
                Some(u) => {
                    let max_w = if self.terminal_type == TerminalType::Petscii { 36 } else { 54 };
                    crate::webbrowser::truncate_to_width(u, max_w)
                }
                None => String::new(),
            };
            self.send_line(&format!("  {}", self.dim(&format!("({}-{} of {})", start + 1, end, total)))).await?;
            if !self.web_forms.is_empty() {
                let form_count = self.web_forms.len();
                let form_hint = if form_count == 1 {
                    "1 form on this page (F to edit)".to_string()
                } else {
                    format!("{} forms on this page (F to edit)", form_count)
                };
                self.send_line(&format!("  {}", self.amber(&form_hint))).await?;
            } else {
                self.send_line(&format!("  {}", self.dim(&url_display))).await?;
            }

            // Navigation footer — two rows to fit all commands
            let is_petscii = self.terminal_type == TerminalType::Petscii;
            let has_forms = !self.web_forms.is_empty();
            // Row 1: navigation
            let mut nav = Vec::new();
            if has_prev { nav.push(self.action_prompt("P", "Pv")); }
            if has_next { nav.push(self.action_prompt("N", "Nx")); }
            nav.push(self.action_prompt("T", "Top"));
            nav.push(self.action_prompt("E", "End"));
            nav.push(self.action_prompt("S", "Find"));
            if !is_petscii {
                nav.push(self.action_prompt("G", "Go"));
            }
            self.send_line(&format!("  {}", nav.join(" "))).await?;
            // Row 2: actions
            let mut act = Vec::new();
            if is_petscii {
                act.push(self.action_prompt("G", "Go"));
            }
            if !self.web_links.is_empty() {
                act.push(self.action_prompt("L", "Lk"));
            }
            if has_forms {
                act.push(self.action_prompt("F", "Fm"));
            }
            act.push(self.action_prompt("K", "Bm"));
            act.push(self.action_prompt("H", "?"));
            if !self.web_history.is_empty() {
                act.push(self.action_prompt("B", "Bk"));
            }
            act.push(self.action_prompt("Q", "X"));
            self.send_line(&format!("  {}", act.join(" "))).await?;
        }
        Ok(())
    }

    async fn handle_web_browser_command(&mut self, input: &str) -> Result<bool, std::io::Error> {
        if self.web_lines.is_empty() {
            // Home screen commands
            match input {
                "g" => {
                    self.web_prompt_url().await?;
                }
                "k" => {
                    self.web_show_bookmarks().await?;
                }
                "h" => {
                    self.web_show_help(false).await?;
                }
                "q" => {
                    self.web_reset();
                    self.current_menu = Menu::Main;
                }
                "r" => {} // just redraw
                _ => {
                    self.show_error("Press G, K, H, or Q.").await?;
                }
            }
        } else {
            // Page view commands
            match input {
                "q" => {
                    // Close page, return to browser home
                    self.web_lines.clear();
                    self.web_scroll = 0;
                }
                "r" => {
                    if let Some(url) = self.web_url.clone() {
                        self.web_fetch_page(&url, false).await?;
                    }
                }
                "n" => {
                    let page_h = Self::WEB_PAGE_HEIGHT;
                    let total = self.web_lines.len();
                    if self.web_scroll + page_h < total {
                        self.web_scroll += page_h;
                    } else {
                        self.show_error("End of page.").await?;
                    }
                }
                "p" => {
                    if self.web_scroll > 0 {
                        let page_h = Self::WEB_PAGE_HEIGHT;
                        self.web_scroll = self.web_scroll.saturating_sub(page_h);
                    } else {
                        self.show_error("Top of page.").await?;
                    }
                }
                "t" => {
                    self.web_scroll = 0;
                }
                "e" => {
                    let page_h = Self::WEB_PAGE_HEIGHT;
                    let total = self.web_lines.len();
                    if total > page_h {
                        self.web_scroll = total - page_h;
                    } else {
                        self.web_scroll = 0;
                    }
                }
                "g" => {
                    self.web_prompt_url().await?;
                }
                "l" => {
                    self.web_prompt_link().await?;
                }
                "s" => {
                    self.web_search_in_page().await?;
                }
                "k" => {
                    self.web_save_bookmark().await?;
                }
                "f" => {
                    self.web_show_forms().await?;
                }
                "h" => {
                    self.web_show_help(true).await?;
                }
                "b" => {
                    if let Some((prev_url, prev_scroll)) = self.web_history.last().cloned() {
                        if self.web_fetch_page(&prev_url, false).await? {
                            // Clamp: the re-fetched page may be shorter than
                            // it was when we saved prev_scroll (dynamic pages),
                            // and an out-of-range scroll panics the render slice.
                            self.web_scroll =
                                prev_scroll.min(self.web_lines.len().saturating_sub(1));
                            self.web_history.pop();
                        }
                    } else {
                        self.show_error("No history.").await?;
                    }
                }
                _ => {
                    self.show_error("Unknown command.").await?;
                }
            }
        }
        Ok(true)
    }

    async fn web_prompt_url(&mut self) -> Result<(), std::io::Error> {
        self.send_line("").await?;
        self.send(&format!("  {}: ", self.cyan("URL/Search"))).await?;
        self.flush().await?;

        let url_input = match self.get_line_input().await? {
            Some(s) if !s.is_empty() => s,
            _ => return Ok(()),
        };

        let url = crate::webbrowser::normalize_url(&url_input);
        self.web_fetch_page(&url, true).await?;
        Ok(())
    }

    async fn web_prompt_link(&mut self) -> Result<(), std::io::Error> {
        if self.web_links.is_empty() {
            self.show_error("No links on this page.").await?;
            return Ok(());
        }

        self.send_line("").await?;
        self.send(&format!("  {} (1-{}): ", self.cyan("Link #"), self.web_links.len())).await?;
        self.flush().await?;

        let input = match self.get_line_input().await? {
            Some(s) if !s.is_empty() => s,
            _ => return Ok(()),
        };

        // Drain any stray bytes (e.g. NUL from telnet CR+NUL) before following
        self.drain_input().await;

        if let Ok(num) = input.parse::<usize>() {
            self.web_follow_link(num).await?;
        } else {
            self.show_error("Enter a number.").await?;
        }
        Ok(())
    }

    async fn web_follow_link(&mut self, num: usize) -> Result<(), std::io::Error> {
        if num >= 1 && num <= self.web_links.len() {
            let link = self.web_links[num - 1].clone();
            let resolved = match &self.web_url {
                Some(base) => crate::webbrowser::resolve_url(base, &link),
                None => crate::webbrowser::normalize_url(&link),
            };
            self.web_fetch_page(&resolved, true).await?;
        } else {
            self.show_error(&format!("Link {} not found.", num)).await?;
        }
        Ok(())
    }

    async fn web_fetch_page(&mut self, url: &str, push_history: bool) -> Result<bool, std::io::Error> {
        // Gopher search URLs need a query term before fetching
        let url = if crate::webbrowser::is_gopher_search(url) {
            self.send_line("").await?;
            self.send(&format!("  {}: ", self.cyan("Search"))).await?;
            self.flush().await?;
            let query = match self.get_line_input().await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(false),
            };
            crate::webbrowser::build_gopher_search_url(url, &query)
        } else {
            url.to_string()
        };

        self.send_line("").await?;
        self.send_line(&format!("  {}...", self.dim("Loading"))).await?;
        self.flush().await?;

        let width = self.web_content_width();
        let url_owned = url.clone();
        let is_gopher = url.starts_with("gopher://");

        let result = tokio::task::spawn_blocking(move || {
            if is_gopher {
                crate::webbrowser::fetch_gopher(&url_owned, width)
            } else {
                crate::webbrowser::fetch_and_render(&url_owned, width)
            }
        })
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?;

        self.web_apply_result(result, push_history).await
    }

    async fn web_show_help(&mut self, page_view: bool) -> Result<(), std::io::Error> {
        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;
        self.send_line(&format!("  {}", self.yellow("BROWSER HELP"))).await?;
        self.send_line(&sep).await?;

        if page_view {
            let is_petscii = self.terminal_type == TerminalType::Petscii;
            // Intro (dim): link-number explanation, width-specific wording.
            if is_petscii {
                self.send_line(&format!("  {}",
                    self.dim("[1] [2] etc. next to text")
                )).await?;
                self.send_line(&format!("  {}",
                    self.dim("are links to other pages.")
                )).await?;
            } else {
                self.send_line(&format!("  {}",
                    self.dim("[1], [2], etc. next to text are links")
                )).await?;
                self.send_line(&format!("  {}",
                    self.dim("to other pages.")
                )).await?;
            }
            self.send_line("").await?;
            for line in Self::browser_page_help_lines(is_petscii) {
                self.send_line(line).await?;
            }
        } else {
            for line in Self::browser_menu_help_lines() {
                self.send_line(line).await?;
            }
            self.send_line("").await?;
            self.send_line(&format!("  {}",
                self.dim("Examples:")
            )).await?;
            self.send_line(&format!("  {}",
                self.dim("  http://telnetbible.com")
            )).await?;
            self.send_line(&format!("  {}",
                self.dim("  gopher://gopher.floodgap.com")
            )).await?;
            self.send_line(&format!("  {}",
                self.dim("  rust programming (search)")
            )).await?;
        }

        self.send_line("").await?;
        self.send("  Press any key to continue.").await?;
        self.flush().await?;
        self.wait_for_key().await?;
        Ok(())
    }

    /// In-page browser key bindings, split by terminal width.  Plain
    /// (uncolored) lines the display iterates and a unit test asserts fit 40
    /// cols on PETSCII (see `punter_help_lines`).
    fn browser_page_help_lines(petscii: bool) -> &'static [&'static str] {
        if petscii {
            &[
                "  N/P  Next/Previous page",
                "  T/E  Jump to Top/End",
                "  S    Search text in page",
                "  G    Go to URL or search",
                "  L    Follow link (any #)",
                "  F    Fill out forms",
                "  K    Save bookmark",
                "  B    Back to previous page",
                "  R    Reload current page",
                "  Q    Close page",
                "  ESC  Exit browser",
            ]
        } else {
            &[
                "  N / P  Next page / Previous page",
                "  T / E  Jump to Top / End of page",
                "  S      Search for text in page",
                "  G      Go to a URL or search query",
                "  L      Follow a link (any number)",
                "  F      Fill out and submit forms",
                "  K      Save page as bookmark",
                "  B      Back to previous page",
                "  R      Reload current page",
                "  Q      Close page (browser home)",
                "  ESC    Exit browser to main menu",
            ]
        }
    }

    /// Browser landing-menu key bindings (shown when no page is loaded).
    fn browser_menu_help_lines() -> &'static [&'static str] {
        &[
            "  G  Go to a URL or search query",
            "  K  Open saved bookmarks",
            "  Q  Exit browser to main menu",
        ]
    }

    /// Main-menu help (single width — fits 40 cols so it serves PETSCII too).
    fn main_help_lines() -> &'static [&'static str] {
        &[
            "  A  AI Chat: ask questions to an AI",
            "  B  Browser: browse the web",
            "  C  Configuration: server settings",
            "     and other options",
            "  F  File Transfer: upload/download",
            "     files using the XMODEM protocol",
            "  G  Serial Gateway: pick Port A or B",
            "     and bridge to its wire (when",
            "     that port is in console mode)",
            "  R  Troubleshooting: diagnose",
            "     terminal input issues",
            "  S  SSH Gateway: connect to a",
            "     remote server via SSH",
            "  T  Telnet Gateway: connect to a",
            "     remote server via telnet",
            "  W  Weather: by city or postal code",
            "  X  Exit: disconnect from server",
        ]
    }

    /// Configuration submenu help, split by terminal width.  Associated fn so a
    /// unit test asserts the REAL lines fit 40 cols.
    fn config_submenu_help_lines(petscii: bool) -> &'static [&'static str] {
        if petscii {
            &[
                "  Configuration submenus:",
                "",
                "  E  Security: require login,",
                "     set usernames and passwords",
                "",
                "  G  Gateway: configure outbound",
                "     Telnet and SSH Gateway menus",
                "",
                "  M  Serial Configuration: pick",
                "     Port A or B and set its",
                "     mode (Modem / Console),",
                "     device, baud, AT settings.",
                "",
                "  S  Server: enable/disable",
                "     services, set ports, and",
                "     restart the server",
                "",
                "  F  File Transfer: per-protocol",
                "     XMODEM, YMODEM, ZMODEM setup",
                "     plus the transfer directory",
                "",
                "  O  Other: AI key, logging,",
                "     and general settings",
                "",
                "  R  Reset all settings to",
                "     default values (asks first)",
                "",
                "  What needs a restart:",
                "    S (ports, enable/disable)",
                "    E (credentials, login",
                "       requirement)",
                "    O > G (GUI on startup)",
                "",
                "  Everything else applies at",
                "  the next session / transfer.",
            ]
        } else {
            &[
                "  Configuration submenus:",
                "",
                "  E  Security: require login, set",
                "     usernames and passwords",
                "",
                "  G  Gateway: configure the outbound",
                "     Telnet and SSH Gateway menus",
                "     (proxy to remote servers)",
                "",
                "  M  Serial Configuration: pick Port A",
                "     or Port B and set its mode (Modem",
                "     Emulator or Serial Console),",
                "     device, baud, AT/S-register state,",
                "     and dialup mapping.  Each port has",
                "     independent settings.",
                "",
                "  S  Server: enable/disable services,",
                "     set ports, and restart the server",
                "",
                "  F  File Transfer: per-protocol",
                "     XMODEM/YMODEM/ZMODEM tuning",
                "     plus the shared transfer directory",
                "",
                "  O  Other: AI key, logging, and",
                "     general settings",
                "",
                "  R  Reset all settings to their",
                "     factory defaults (confirms first)",
                "",
                "  Which changes need a restart:",
                "    S changes (ports, enable/disable)",
                "    E changes (credentials, login toggle)",
                "    O > G toggle (GUI on startup)",
                "",
                "  Everything else (file-transfer",
                "  timings, gateway mode, modem AT",
                "  settings, AI key, homepage, weather",
                "  location) applies at the next session",
                "  or transfer without a restart.",
            ]
        }
    }

    /// File-transfer *menu* help (the F-menu's H screen — distinct from the
    /// per-protocol file-transfer *settings* help in `file_transfer_help_lines`).
    fn file_transfer_menu_help_lines() -> &'static [&'static str] {
        &[
            "  Menu items:",
            "  U  Upload a file to the server",
            "  D  Download a file from server",
            "  X  Delete a file on the server",
            "  C  Change to a subdirectory",
            "  M  Make a new subdirectory",
            "  K  Kermit server mode (idle for",
            "     remote get/send/dir/finish)",
            "  I  Toggle IAC escaping on/off",
            "  R  Refresh the screen",
            "  Q  Back to the main menu",
            "",
            "  Picking a protocol on upload:",
            "    X  XMODEM or YMODEM - variant",
            "       auto-detected from block 0.",
            "    Z  ZMODEM - full Forsberg",
            "       batch with ZSKIP handling.",
            "    P  Punter - Commodore C1",
            "       (CCGMS / Novaterm).",
            "    Kermit is not a picker option",
            "    - use K (server mode) above.",
            "",
            "  Picking a protocol on download:",
            "    X  Classic XMODEM (128 B)",
            "    1  XMODEM-1K (1024 B blocks,",
            "       SOH fallback if peer NAKs)",
            "    Y  YMODEM (filename + size",
            "       header, then 1K data)",
            "    Z  ZMODEM (auto-starts in",
            "       most modern terminals)",
            "    P  Punter (Commodore C1)",
            "    Kermit is not a picker option",
            "    - use K (server mode) above.",
            "",
            "  IAC escaping (I toggle):",
            "    Telnet reserves byte 0xFF as",
            "    the IAC marker. When trans-",
            "    ferring binary files that may",
            "    contain 0xFF, enable IAC",
            "    escaping so the stream",
            "    survives the wire intact.",
            "    Both sides must agree on the",
            "    setting. Default is ON for",
            "    telnet clients, OFF for SSH",
            "    (which has no IAC layer).",
            "",
            "  Limits:",
            "    Maximum file size: 8 MB.",
            "    Filenames: 64 chars max,",
            "    letters/digits/._- only, may",
            "    not start with a dot or",
            "    contain '..' (path traversal",
            "    protection).",
            "",
            "  Timeouts and retry intervals",
            "  are tunable in Configuration >",
            "  File Transfer > X / Y / Z.",
        ]
    }

    /// Download file-picker help.
    fn download_help_lines() -> &'static [&'static str] {
        &[
            "  #    Enter file number to download",
            "  P    Previous page of files",
            "  N    Next page of files",
            "  Q    Back to file transfer menu",
            "  ESC  Return to main menu",
        ]
    }

    /// Delete file-picker help.
    fn delete_help_lines() -> &'static [&'static str] {
        &[
            "  #    Enter file number to delete",
            "  P    Previous page of files",
            "  N    Next page of files",
            "  Q    Back to file transfer menu",
            "  ESC  Return to main menu",
        ]
    }

    /// AI-chat help.
    fn ai_chat_help_lines() -> &'static [&'static str] {
        &[
            "  Navigation:",
            "  P    Previous page of answer",
            "  N    Next page of answer",
            "  Q    Done, return to main menu",
            "",
            "  Or type a new question and",
            "  press Enter to ask again.",
            "  The model keeps conversational",
            "  context within a single AI Chat",
            "  session.",
            "",
            "  About the service:",
            "  Powered by Groq (groq.com), a",
            "  free LLM inference API. The",
            "  model is Llama 3.3 70B",
            "  Versatile, a capable general-",
            "  purpose assistant.",
            "",
            "  Getting a key:",
            "  1. Visit console.groq.com and",
            "     create a free account.",
            "  2. Generate an API key (starts",
            "     with gsk_...).",
            "  3. Set it in Configuration >",
            "     Other Settings > A, or paste",
            "     into egateway.conf as",
            "     groq_api_key = gsk_...",
            "  4. Restart the server.",
            "",
            "  Rate limits:",
            "  Free-tier limits are generous",
            "  for interactive use but rate-",
            "  throttle on sustained high",
            "  traffic. See groq.com for the",
            "  current limits.",
            "",
            "  Privacy:",
            "  Questions and answers are sent",
            "  to Groq's API and subject to",
            "  their terms of service. Don't",
            "  paste sensitive information.",
        ]
    }

    /// Dialup-mapping help.
    fn dialup_help_lines() -> &'static [&'static str] {
        &[
            "  Map phone numbers to host:port",
            "  targets.  This table is shared",
            "  across both ports' modems - one",
            "  dialup.conf consulted by Port A",
            "  and Port B alike.",
            "",
            "  Dial a number with ATDT, ATDP,",
            "  or ATD (all work the same) and",
            "  the server connects to the",
            "  mapped host:port for you.",
            "",
            "  You can still dial host:port",
            "  directly - mappings are optional.",
            "",
            "  Mappings are saved in dialup.conf.",
        ]
    }

    /// Bookmarks-list help.
    fn bookmarks_help_lines() -> &'static [&'static str] {
        &[
            "  #    Enter bookmark number to open",
            "  D    Delete a bookmark by number",
            "  ESC  Cancel and go back",
        ]
    }

    /// Web-form help.
    fn form_help_lines() -> &'static [&'static str] {
        &[
            "  #    Enter a field number to",
            "       edit its value",
            "  S    Submit the form",
            "  Q    Cancel and go back",
        ]
    }

    async fn web_save_bookmark(&mut self) -> Result<(), std::io::Error> {
        if let Some(url) = &self.web_url {
            let title = self.web_title.as_deref().unwrap_or("Untitled");
            if crate::webbrowser::add_bookmark(url, title) {
                self.send_line("").await?;
                self.send_line(&format!("  {}", self.green("Bookmark saved."))).await?;
                self.send_line("").await?;
                self.send("  Press any key to continue.").await?;
                self.flush().await?;
                self.wait_for_key().await?;
            } else {
                self.show_error("Already bookmarked (or full).").await?;
            }
        } else {
            self.show_error("No page to bookmark.").await?;
        }
        Ok(())
    }

    async fn web_show_bookmarks(&mut self) -> Result<(), std::io::Error> {
        let bookmarks = crate::webbrowser::load_bookmarks();
        if bookmarks.is_empty() {
            self.show_error("No bookmarks saved.").await?;
            return Ok(());
        }

        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;
        self.send_line(&format!("  {}", self.yellow("BOOKMARKS"))).await?;
        self.send_line(&sep).await?;
        self.send_line("").await?;

        let max_title = if self.terminal_type == TerminalType::Petscii { 30 } else { 60 };
        let display_max = bookmarks.len().min(Self::WEB_PAGE_HEIGHT);
        for (i, bm) in bookmarks.iter().take(display_max).enumerate() {
            let title = crate::webbrowser::truncate_to_width(&bm.title, max_title);
            self.send_line(&format!("  {:>2}. {}", i + 1, title)).await?;
        }
        if bookmarks.len() > display_max {
            self.send_line(&format!("  {} more...", bookmarks.len() - display_max)).await?;
        }

        self.send_line("").await?;
        self.send_line(&format!("  {} {} {}",
            self.dim("#=Open"),
            self.action_prompt("D", "Delete"),
            self.action_prompt("H", "Help"),
        )).await?;
        self.send(&format!("  {}: ", self.cyan("#/D"))).await?;
        self.flush().await?;

        let input = match self.get_line_input().await? {
            Some(s) if !s.is_empty() => s,
            _ => return Ok(()),
        };

        if input == "h" {
            self.show_help_page("BOOKMARKS HELP", Self::bookmarks_help_lines())
                .await?;
        } else if input == "d" {
            // Delete mode
            self.send(&format!("  {} (1-{}): ", self.cyan("Delete #"), display_max)).await?;
            self.flush().await?;
            let del_input = match self.get_line_input().await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(()),
            };
            if let Ok(num) = del_input.parse::<usize>() {
                if num >= 1 && num <= display_max {
                    crate::webbrowser::remove_bookmark(num - 1);
                    self.send_line(&format!("  {}", self.green("Deleted."))).await?;
                    self.send_line("").await?;
                    self.send("  Press any key to continue.").await?;
                    self.flush().await?;
                    self.wait_for_key().await?;
                } else {
                    self.show_error("Invalid number.").await?;
                }
            }
        } else if let Ok(num) = input.parse::<usize>() {
            if num >= 1 && num <= display_max {
                let url = bookmarks[num - 1].url.clone();
                self.web_fetch_page(&url, true).await?;
            } else {
                self.show_error("Invalid number.").await?;
            }
        }
        Ok(())
    }

    async fn web_search_in_page(&mut self) -> Result<(), std::io::Error> {
        self.send_line("").await?;
        self.send(&format!("  {}: ", self.cyan("Find"))).await?;
        self.flush().await?;

        let query = match self.get_line_input().await? {
            Some(s) if !s.is_empty() => s.to_ascii_lowercase(),
            _ => return Ok(()),
        };

        // Search from line after current scroll position, then wrap around
        let total = self.web_lines.len();
        let start_line = self.web_scroll + 1;
        for offset in 0..total {
            let idx = (start_line + offset) % total;
            if self.web_lines[idx].to_ascii_lowercase().contains(&query) {
                // Scroll to put the match at the top of the page
                self.web_scroll = idx;
                return Ok(());
            }
        }

        self.show_error("Not found.").await?;
        Ok(())
    }

    async fn web_show_forms(&mut self) -> Result<(), std::io::Error> {
        if self.web_forms.is_empty() {
            self.show_error("No forms on this page.").await?;
            return Ok(());
        }

        if self.web_forms.len() == 1 {
            return self.web_edit_form(0).await;
        }

        self.send_line("").await?;
        self.send_line(&format!("  {}", self.yellow("FORMS"))).await?;
        let forms_snapshot: Vec<String> = self.web_forms.iter().enumerate().map(|(i, form)| {
            let label = crate::webbrowser::truncate_to_width(&form.label, 30);
            format!("  {}. {}", i + 1, label)
        }).collect();
        for line in &forms_snapshot {
            self.send_line(line).await?;
        }
        self.send_line("").await?;
        self.send(&format!("  {} (1-{}): ", self.cyan("Form #"), self.web_forms.len())).await?;
        self.flush().await?;

        let input = match self.get_line_input().await? {
            Some(s) if !s.is_empty() => s,
            _ => return Ok(()),
        };

        if let Ok(num) = input.parse::<usize>() {
            if num >= 1 && num <= self.web_forms.len() {
                self.web_edit_form(num - 1).await?;
            } else {
                self.show_error("Invalid form number.").await?;
            }
        } else {
            self.show_error("Enter a number.").await?;
        }
        Ok(())
    }

    async fn web_edit_form(&mut self, form_idx: usize) -> Result<(), std::io::Error> {
        let mut form = self.web_forms[form_idx].clone();

        // If the form has no visible fields (only hidden), submit immediately
        let has_visible = form.fields.iter().any(|f| !matches!(f, crate::webbrowser::FormField::Hidden { .. }));
        if !has_visible {
            self.web_forms[form_idx] = form;
            return self.web_submit_form(form_idx).await;
        }

        loop {
            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            let title = crate::webbrowser::truncate_to_width(&form.label, 34);
            self.send_line(&format!("  {}", self.yellow(&title))).await?;
            self.send_line(&sep).await?;

            let mut field_num = 0usize;
            let is_petscii = self.terminal_type == TerminalType::Petscii;
            let max_label = if is_petscii { 12 } else { 20 };
            let max_val = if is_petscii { 18 } else { 40 };

            let display_lines: Vec<String> = form.fields.iter().filter_map(|field| {
                match field {
                    crate::webbrowser::FormField::Hidden { .. } => None,
                    crate::webbrowser::FormField::Text { label, value, .. }
                    | crate::webbrowser::FormField::TextArea { label, value, .. } => {
                        field_num += 1;
                        // Sanitize the value for display only — the stored
                        // `value` is submitted verbatim, so we must not strip
                        // control bytes from it (M-8; labels/option-text are
                        // already sanitized in WebPage::sanitize).
                        let display_val = if value.is_empty() {
                            "(empty)".to_string()
                        } else {
                            crate::aichat::sanitize_for_terminal(value)
                        };
                        Some(format!("  {}.{}: {}",
                            field_num,
                            crate::webbrowser::truncate_to_width(label, max_label),
                            crate::webbrowser::truncate_to_width(&display_val, max_val),
                        ))
                    }
                    crate::webbrowser::FormField::Select { label, options, selected, .. } => {
                        field_num += 1;
                        let chosen = options.get(*selected).map(|(_, t)| t.as_str()).unwrap_or("?");
                        Some(format!("  {}.{}: {}",
                            field_num,
                            crate::webbrowser::truncate_to_width(label, max_label),
                            crate::webbrowser::truncate_to_width(chosen, max_val),
                        ))
                    }
                    crate::webbrowser::FormField::Checkbox { label, checked, .. } => {
                        field_num += 1;
                        let mark = if *checked { "[X]" } else { "[ ]" };
                        Some(format!("  {}.{}: {}",
                            field_num,
                            crate::webbrowser::truncate_to_width(label, max_label),
                            mark,
                        ))
                    }
                    crate::webbrowser::FormField::Radio { label, checked, .. } => {
                        field_num += 1;
                        let mark = if *checked { "(X)" } else { "( )" };
                        Some(format!("  {}.{}: {}",
                            field_num,
                            crate::webbrowser::truncate_to_width(label, max_label),
                            mark,
                        ))
                    }
                }
            }).collect();

            for line in &display_lines {
                self.send_line(line).await?;
            }

            self.send_line("").await?;
            self.send_line(&format!("  {} {} {} {}",
                self.action_prompt("S", "Submit"),
                self.dim("#=Edit"),
                self.action_prompt("Q", "Cancel"),
                self.action_prompt("H", "Help"),
            )).await?;
            self.send(&format!("  {}: ", self.cyan("#/S/Q"))).await?;
            self.flush().await?;

            let input = match self.get_line_input().await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(()),
            };

            match input.as_str() {
                "s" => {
                    self.web_forms[form_idx] = form;
                    return self.web_submit_form(form_idx).await;
                }
                "q" => return Ok(()),
                "h" => {
                    self.show_help_page("FORM HELP", Self::form_help_lines())
                        .await?;
                }
                other => {
                    if let Ok(num) = other.parse::<usize>() {
                        if let Some(real_idx) = crate::webbrowser::visible_field_index(&form.fields, num) {
                            self.web_edit_field(&mut form, real_idx).await?;
                        } else {
                            self.show_error("Invalid field number.").await?;
                        }
                    } else {
                        self.show_error("Enter S, Q, H, or a field #.").await?;
                    }
                }
            }
        }
    }

    async fn web_edit_field(&mut self, form: &mut crate::webbrowser::WebForm, idx: usize) -> Result<(), std::io::Error> {
        use crate::webbrowser::FormField;

        let (is_text, is_password, is_select, is_checkbox, is_radio, label_str, opt_count) = {
            let field = &form.fields[idx];
            match field {
                FormField::Text { label, input_type, .. } => {
                    (true, input_type == "password", false, false, false, label.clone(), 0)
                }
                FormField::TextArea { label, .. } => {
                    (true, false, false, false, false, label.clone(), 0)
                }
                FormField::Select { options, .. } => {
                    (false, false, true, false, false, String::new(), options.len())
                }
                FormField::Checkbox { .. } => {
                    (false, false, false, true, false, String::new(), 0)
                }
                FormField::Radio { name, .. } => {
                    (false, false, false, false, true, name.clone(), 0)
                }
                FormField::Hidden { .. } => {
                    return Ok(());
                }
            }
        };

        if is_text {
            self.send_line("").await?;
            self.send(&format!("  {}: ", self.cyan(&label_str))).await?;
            self.flush().await?;
            let input = if is_password {
                self.get_password_input().await?
            } else {
                self.get_line_input().await?
            };
            if let Some(new_val) = input {
                match &mut form.fields[idx] {
                    FormField::Text { value, .. } | FormField::TextArea { value, .. } => {
                        *value = new_val;
                    }
                    _ => {}
                }
            }
        } else if is_select {
            self.send_line("").await?;
            let opts_snapshot: Vec<(String, bool)> = if let FormField::Select { options, selected, .. } = &form.fields[idx] {
                options.iter().enumerate().map(|(i, (_, display))| {
                    (display.clone(), i == *selected)
                }).collect()
            } else {
                Vec::new()
            };
            for (i, (display, is_sel)) in opts_snapshot.iter().enumerate() {
                let marker = if *is_sel { ">" } else { " " };
                self.send_line(&format!("  {}{}.{}",
                    marker, i + 1,
                    crate::webbrowser::truncate_to_width(display, 30),
                )).await?;
            }
            self.send(&format!("  {} (1-{}): ", self.cyan("Pick"), opt_count)).await?;
            self.flush().await?;
            if let Some(input) = self.get_line_input().await?
                && let Ok(n) = input.parse::<usize>()
                    && n >= 1 && n <= opt_count
                        && let FormField::Select { selected, .. } = &mut form.fields[idx] {
                            *selected = n - 1;
                        }
        } else if is_checkbox {
            if let FormField::Checkbox { checked, .. } = &mut form.fields[idx] {
                *checked = !*checked;
            }
        } else if is_radio {
            let radio_name = label_str;
            for f in form.fields.iter_mut() {
                if let FormField::Radio { name, checked, .. } = f
                    && *name == radio_name {
                        *checked = false;
                    }
            }
            if let FormField::Radio { checked, .. } = &mut form.fields[idx] {
                *checked = true;
            }
        }
        Ok(())
    }

    async fn web_submit_form(&mut self, form_idx: usize) -> Result<(), std::io::Error> {
        self.send_line("").await?;
        self.send_line(&format!("  {}...", self.dim("Submitting"))).await?;
        self.flush().await?;

        let form = self.web_forms[form_idx].clone();
        let base = self.web_url.clone().unwrap_or_default();
        let width = self.web_content_width();

        let result = tokio::task::spawn_blocking(move || {
            crate::webbrowser::submit_form(&base, &form, width)
        })
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?;

        self.web_apply_result(result, true).await?;
        Ok(())
    }

    async fn web_apply_result(
        &mut self,
        result: Result<crate::webbrowser::WebPage, String>,
        push_history: bool,
    ) -> Result<bool, std::io::Error> {
        match result {
            Ok(page) => {
                if push_history
                    && let Some(old_url) = self.web_url.as_ref() {
                        self.web_history.push((old_url.clone(), self.web_scroll));
                        if self.web_history.len() > Self::WEB_MAX_HISTORY {
                            self.web_history.remove(0);
                        }
                    }
                self.web_url = Some(page.url);
                self.web_title = page.title;
                self.web_lines = page.lines;
                self.web_links = page.links;
                self.web_forms = page.forms;
                self.web_scroll = 0;
                Ok(true)
            }
            Err(e) => {
                // Sanitize before display: a fetch error can echo remote-derived
                // bytes (e.g. "Bad URL: <href>" from a page link, or a network
                // error carrying a remote host string), which would otherwise
                // reach the terminal raw — the same escape-injection risk M-8
                // closes on the page-render path.
                let max_w = if self.terminal_type == TerminalType::Petscii { 30 } else { 50 };
                let safe = crate::aichat::sanitize_for_terminal(&e);
                self.show_error(&crate::webbrowser::truncate_to_width(&safe, max_w)).await?;
                Ok(false)
            }
        }
    }

    fn web_reset(&mut self) {
        self.web_lines.clear();
        self.web_scroll = 0;
        self.web_links.clear();
        self.web_forms.clear();
        self.web_history.clear();
        self.web_url = None;
        self.web_title = None;
    }
}

// ─── Server startup ─────────────────────────────────────────

/// Send a connection-rejection message and close the stream cleanly.
///
/// Designed to be `tokio::spawn`'d from the accept loop — must not
/// block the loop itself, since rejections can arrive in floods (max-
/// sessions reached, or a host scanning from a non-RFC1918 IP under
/// security_enabled=false).  The owned `Vec<u8>` lets the caller
/// `tokio::spawn(send_rejection_message(stream, msg))` without
/// fighting borrow checker.
///
/// We use a bounded write_all + flush + shutdown rather than the
/// non-blocking `try_write` so the message actually reaches a vintage
/// terminal that's slow to drain its receive buffer (Commodore 64 over
/// EtherLink, AltairDuino on a 9600 bps line, etc.).  `try_write`
/// silently drops the bytes when the kernel send buffer can't take
/// them immediately, leaving the user staring at "connection closed"
/// with no explanation — particularly painful on retro hardware that
/// can't easily reconnect.  The 2-second cap keeps a misbehaving peer
/// from holding a tokio task open indefinitely.
async fn send_rejection_message(
    mut stream: tokio::net::TcpStream,
    msg: Vec<u8>,
) {
    use tokio::io::AsyncWriteExt;
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        async {
            let _ = stream.write_all(&msg).await;
            let _ = stream.flush().await;
            let _ = stream.shutdown().await;
        },
    )
    .await;
    // stream drops here regardless of timeout outcome.
}

/// Start the telnet server accept loop.
pub fn start_server(
    shutdown: Arc<AtomicBool>,
    restart: Arc<AtomicBool>,
    shutdown_notify: Arc<tokio::sync::Notify>,
    session_writers: SessionWriters,
    lockouts: LockoutMap,
) {
    let cfg = config::get_config();
    if !cfg.telnet_enabled {
        return;
    }
    let port = cfg.telnet_port;
    let max_sessions = cfg.max_sessions;
    // Note: `security_enabled` and `disable_ip_safety` are NOT captured
    // here.  Both are read fresh on each accept so the GUI / telnet-menu
    // toggles take effect immediately on the next inbound connection
    // without requiring a server restart.

    tokio::spawn(async move {
        let listener = match TcpListener::bind(format!("0.0.0.0:{}", port)).await {
            Ok(l) => l,
            Err(e) => {
                glog!("Telnet server: failed to bind port {}: {}", port, e);
                return;
            }
        };
        glog!("Telnet server listening on port {}", port);

        let session_count = Arc::new(AtomicUsize::new(0));

        loop {
            if shutdown.load(Ordering::SeqCst) {
                // The shutdown goodbye is broadcast centrally from main.rs
                // (see `broadcast_to_sessions`) so it reaches SSH/relay
                // sessions too, not just when the telnet server is enabled.
                break;
            }
            tokio::select! {
                result = listener.accept() => {
                    match result {
                        Ok((stream, addr)) => {
                            // Atomic claim: fetch_add returns the value
                            // BEFORE the increment, so concurrent
                            // accepts each see a unique slot.  This
                            // closes the load-then-fetch_add TOCTOU
                            // window where two threads could both
                            // observe `current < max_sessions` and bust
                            // the cap.  If we end up over the limit, roll
                            // back the increment before rejecting.  (The SSH
                            // server enforces the same cap independently, with
                            // its OWN counter and this same fetch_add +
                            // rollback pattern — claimed on a successful login
                            // in `auth_password`, released on disconnect.  Only
                            // the per-IP lockout map is shared between the two;
                            // the session counters are separate, so each
                            // protocol allows up to `max_sessions`.)
                            let prev = session_count.fetch_add(1, Ordering::SeqCst);
                            if prev >= max_sessions {
                                session_count.fetch_sub(1, Ordering::SeqCst);
                                glog!("Telnet: rejected {} (max {} sessions)", addr, max_sessions);
                                // Spawn the rejection write so the
                                // 2-second bounded send doesn't block
                                // the accept loop.  Without spawning,
                                // a flood of rejections (max-sessions
                                // reached, or a host scanning from a
                                // non-RFC1918 IP) would serialize the
                                // accept loop at ~0.5 conn/sec — a
                                // self-inflicted DoS for legitimate
                                // clients.
                                tokio::spawn(send_rejection_message(
                                    stream,
                                    b"Too many connections. Try again later.\r\n".to_vec(),
                                ));
                                continue;
                            }
                            // Re-read each accept so toggles in the
                            // GUI / telnet menu apply immediately.
                            // `get_security_flags` reads only the two
                            // booleans without cloning the full Config,
                            // keeping accept-flood cost down to a
                            // single Mutex acquisition with no String
                            // allocations.
                            let (live_security, live_disable_safety) =
                                config::get_security_flags();
                            // NOTE: telnet deliberately still couples the IP
                            // allowlist to `security_enabled` — enabling auth
                            // opens telnet to any source IP.  This is the
                            // OPPOSITE of the web server (M-9,
                            // `webserver.rs::handle_connection`), which now
                            // applies the allowlist regardless of login.  The
                            // asymmetry is intentional: the web page echoes the
                            // password + API key into `value="…"` attributes,
                            // so widening its IP exposure on auth is dangerous;
                            // telnet echoes no secrets and is the retro-hardware
                            // path where "turn on auth to expose it" is a
                            // legitimate deployment.  `disable_ip_safety`
                            // remains the escape hatch for both.
                            if !live_security
                                && !live_disable_safety
                                && let Some(reason) = reject_insecure_ip(addr.ip())
                            {
                                session_count.fetch_sub(1, Ordering::SeqCst);
                                glog!("Telnet: rejected {} ({})", addr, reason);
                                let msg = format!("{}\r\n", reason).into_bytes();
                                tokio::spawn(send_rejection_message(stream, msg));
                                continue;
                            }
                            glog!("Telnet: connection from {} ({}/{})", addr, prev + 1, max_sessions);
                            let sd = shutdown.clone();
                            let rs = restart.clone();
                            let sc = session_count.clone();
                            let sw = session_writers.clone();
                            let lo = lockouts.clone();
                            tokio::spawn(async move {
                                let _ = stream.set_nodelay(true);
                                let (read_half, write_half) = stream.into_split();
                                let writer_box: Box<dyn tokio::io::AsyncWrite + Unpin + Send> = Box::new(write_half);
                                let writer_arc: SharedWriter = Arc::new(tokio::sync::Mutex::new(writer_box));
                                sw.lock().await.push(writer_arc.clone());
                                let mut session = TelnetSession {
                                    reader: Box::new(read_half),
                                    writer: writer_arc.clone(),
                                    shutdown: sd,
                                    restart: rs,
                                    current_menu: Menu::Main,
                                    terminal_type: TerminalType::Ansi,
                                    erase_char: 0x7F,
                                    lockouts: lo,
                                    peer_addr: Some(addr.ip()),
                                    transfer_subdir: String::new(),
                                    // Start with IAC escaping off; session_read_byte
                                    // flips telnet_negotiated on as soon as the client
                                    // sends any telnet option negotiation, and run()
                                    // sets xmodem_iac from that flag after terminal
                                    // detection.  Real telnet clients (PuTTY, Tera Term,
                                    // C-Kermit, SecureCRT) always negotiate and get
                                    // IAC escaping; raw TCP clients (netcat, retro
                                    // firmware) don't and get a transparent byte
                                    // stream.  The I toggle in the File Transfer menu
                                    // still lets the user override per-session.
                                    xmodem_iac: false,
                                    web_lines: Vec::new(),
                                    web_scroll: 0,
                                    web_links: Vec::new(),
                                    web_history: Vec::new(),
                                    web_url: None,
                                    web_title: None,
                                    web_forms: Vec::new(),
                                    weather_location: config::get_config().weather_location,
                                    is_serial: false,
                                    is_relay: false,
                                    serial_port_id: None,
                                    is_ssh: false,
                                    idle_timeout: std::time::Duration::from_secs(cfg.idle_timeout_secs),
                                    pushback: None,
                                    neg_sent_will: Box::new([false; 256]),
                                    neg_sent_do: Box::new([false; 256]),
                                    neg_sent_wont: Box::new([false; 256]),
                                    neg_sent_dont: Box::new([false; 256]),
                                    ttype_matched: false,
                                    ttype_raw: None,
                                    telnet_negotiated: false,
                                    window_width: None,
                                    window_height: None,
                                };
                                if let Err(e) = session.run().await {
                                    glog!("Telnet: session error from {}: {}", addr, e);
                                }
                                {
                                    let mut w = writer_arc.lock().await;
                                    let _ = w.shutdown().await;
                                }
                                sw.lock().await.retain(|w| !Arc::ptr_eq(w, &writer_arc));
                                sc.fetch_sub(1, Ordering::SeqCst);
                                glog!("Telnet: {} disconnected", addr);
                            });
                        }
                        Err(e) => {
                            glog!("Telnet: accept error: {}", e);
                        }
                    }
                }
                _ = shutdown_notify.notified() => {}
                _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {}
            }
        }
    });
}

/// Standalone Kermit-server TCP listener.  When the operator enables
/// `kermit_server_enabled` (GUI Server frame or telnet Server
/// Configuration menu), this binds `kermit_server_port` and drops every
/// accepted connection straight into Kermit server mode — no telnet
/// menu, no terminal detection, no auth gate, no private-IP filter.
/// The bypass is deliberate and gated by the GUI / menu confirmation
/// popup; same posture as `allow_atdt_kermit`.
///
/// Spec compliance posture: every accepted socket is handed to
/// `kermit::kermit_server_with_outcome`, the same entry point the
/// in-band telnet path (`F → K`) uses.  All Kermit-protocol behavior
/// — Send-Init handshake, capability negotiation (long packets,
/// sliding window, streaming, attribute packets, repeat compression,
/// 8-bit quoting, locking shifts), CHK1/CHK2/CRC-16, R/S/G command
/// dispatch, ZCRCQ/ZCRCE flow control, NAK retries, idle-timeout
/// E-packet, batch transfers — is identical to the in-band path.
/// Differences are confined to transport flags:
///
/// - `is_tcp = false` so the protocol layer doesn't apply telnet
///   IAC escaping (raw TCP is 8-bit clean, which is what real
///   Kermit clients connecting to `kermit -j host:port` expect).
/// - `is_petscii = false` because there's no terminal on the other
///   end — peers are Kermit clients, not interactive terminals.
///
/// Files received are saved into `cfg.transfer_dir` using the same
/// validation + AlreadyExists/WriteFailed handling as the in-band
/// kermit-server path; unsafe filenames and collisions are skipped,
/// not clobbered.
pub fn start_kermit_server(
    shutdown: Arc<AtomicBool>,
    shutdown_notify: Arc<tokio::sync::Notify>,
) {
    let cfg = config::get_config();
    if !cfg.kermit_server_enabled {
        return;
    }
    let port = cfg.kermit_server_port;

    tokio::spawn(async move {
        let listener = match TcpListener::bind(format!("0.0.0.0:{}", port)).await {
            Ok(l) => l,
            Err(e) => {
                glog!("Kermit server: failed to bind port {}: {}", port, e);
                return;
            }
        };
        glog!(
            "Kermit server listening on port {} (auth + IP filter bypassed)",
            port
        );

        loop {
            if shutdown.load(Ordering::SeqCst) {
                break;
            }
            tokio::select! {
                result = listener.accept() => {
                    match result {
                        Ok((stream, addr)) => {
                            glog!("Kermit server: connection from {}", addr);
                            tokio::spawn(async move {
                                let _ = stream.set_nodelay(true);
                                let (mut read_half, mut write_half) = stream.into_split();
                                // Single snapshot of the config for this
                                // session — verbose flag and transfer_dir
                                // are stable for the duration of one
                                // connection, and folding the two
                                // independent get_config() calls into one
                                // avoids re-locking the global mutex
                                // mid-session-setup.
                                let session_cfg = config::get_config();
                                let verbose = session_cfg.verbose;
                                let target_dir =
                                    std::path::PathBuf::from(&session_cfg.transfer_dir);
                                if let Err(e) = tokio::fs::create_dir_all(&target_dir).await {
                                    glog!(
                                        "Kermit server: cannot create transfer dir {:?}: {}",
                                        target_dir,
                                        e
                                    );
                                    return;
                                }
                                let mut saved: Vec<(String, usize)> = Vec::new();
                                let mut skipped: Vec<(String, &'static str)> = Vec::new();
                                let result = crate::kermit::kermit_server_with_outcome(
                                    &mut read_half,
                                    &mut write_half,
                                    false, // not telnet — no IAC escaping on the wire
                                    false, // not PETSCII
                                    verbose,
                                    |rx| {
                                        if TelnetSession::validate_filename(&rx.filename).is_err() {
                                            // Sanitize before the name can reach the terminal summary.
                                            skipped.push((crate::aichat::sanitize_for_terminal(&rx.filename), "invalid filename"));
                                            return;
                                        }
                                        // Defense-in-depth: re-validate the
                                        // subdir before joining.  This
                                        // standalone listener bypasses auth and
                                        // the IP allowlist by design, so
                                        // re-checking matters even though
                                        // rx.subdir is only set after kermit's
                                        // own is_safe_relative_subdir today.
                                        if !crate::kermit::is_safe_relative_subdir(&rx.subdir) {
                                            skipped.push((rx.filename.clone(), "unsafe subdir"));
                                            return;
                                        }
                                        let dir = if rx.subdir.is_empty() {
                                            target_dir.clone()
                                        } else {
                                            target_dir.join(&rx.subdir)
                                        };
                                        if let Err(e) = std::fs::create_dir_all(&dir) {
                                            glog!(
                                                "Kermit server: cannot create subdir {:?}: {}",
                                                dir,
                                                e
                                            );
                                            skipped.push((rx.filename.clone(), "subdir create failed"));
                                            return;
                                        }
                                        let filepath = dir.join(&rx.filename);
                                        let meta = crate::xmodem::YmodemReceiveMeta {
                                            size: rx.declared_size,
                                            modtime: rx.modtime,
                                            mode: rx.mode,
                                        };
                                        match TelnetSession::save_received_file_sync(
                                            &filepath,
                                            &rx.data,
                                            Some(&meta),
                                            rx.resumed,
                                        ) {
                                            Ok(()) => saved.push((rx.filename.clone(), rx.data.len())),
                                            Err(SaveError::AlreadyExists) => {
                                                skipped.push((rx.filename.clone(), "already exists"));
                                            }
                                            Err(SaveError::WriteFailed) => {
                                                skipped.push((rx.filename.clone(), "write failed"));
                                            }
                                        }
                                    },
                                )
                                .await;
                                let _ = write_half.shutdown().await;
                                match result {
                                    Ok(_) => glog!(
                                        "Kermit server: {} closed — saved {}, skipped {}",
                                        addr,
                                        saved.len(),
                                        skipped.len()
                                    ),
                                    Err(e) => glog!("Kermit server: {} session error: {}", addr, e),
                                }
                            });
                        }
                        Err(e) => {
                            glog!("Kermit server: accept error: {}", e);
                        }
                    }
                }
                _ = shutdown_notify.notified() => {}
                _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {}
            }
        }
    });
}

// ─── Tests ──────────────────────────────────────────────────

#[cfg(test)]
mod tests;
