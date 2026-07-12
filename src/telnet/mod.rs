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
mod transfer;
mod config_ui;
mod serial_ui;
mod web;
mod aichat_ui;
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
