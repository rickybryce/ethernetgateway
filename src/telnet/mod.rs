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
mod gateway;
pub(in crate::telnet) use gateway::{gw_debug_enabled, gateway_terminal_name};
// Gateway plumbing types/fns referenced only from tests.
#[cfg(test)]
pub(in crate::telnet) use gateway::{GatewayTelnetIac, GatewayIacState, OptState,
    GatewayInboundEvent, REMOTE_PORT_DISPLAY_CAP, read_gateway_event,
    filter_gateway_output, normalize_gateway_input};
mod io;
pub(crate) use io::{read_byte_iac_filtered, write_telnet_data};
mod session;
pub(crate) use session::{match_terminal_name, is_backspace_key};
mod transfer;
mod config_ui;
mod serial_ui;
mod web;
mod aichat_ui;
mod weather;
mod kernel;
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
pub(crate) enum TerminalType {
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


// ─── Input helpers (standalone) ─────────────────────────────

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
            "  S  CP/M file shell (drive A:)",
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
/// RAII backstop that releases a telnet session's `max_sessions` slot and
/// removes its writer from the broadcast list even if `session.run()`
/// panics (F3).  The normal path does the graceful async cleanup and then
/// calls `defuse()`; only a panic-unwind leaves the guard armed, in which
/// case Drop reclaims the slot (sync) and best-effort removes the writer.
/// Without this, a future reachable panic in a session would silently leak a
/// session slot and grow `session_writers` unbounded.
struct SessionSlotGuard {
    count: Arc<AtomicUsize>,
    writers: SessionWriters,
    writer: SharedWriter,
    armed: bool,
}

impl SessionSlotGuard {
    fn defuse(&mut self) {
        self.armed = false;
    }
}

impl Drop for SessionSlotGuard {
    fn drop(&mut self) {
        if !self.armed {
            return; // normal path already released under the async lock
        }
        self.count.fetch_sub(1, Ordering::SeqCst);
        // Best-effort writer removal — `try_lock` avoids awaiting/blocking in
        // Drop.  If contended (rare, and only on a panic unwind), the dead
        // writer is left for the broadcast path to skip (writes to a closed
        // half just error out).
        if let Ok(mut ws) = self.writers.try_lock() {
            ws.retain(|w| !Arc::ptr_eq(w, &self.writer));
        }
    }
}

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
                                // Arm the panic-unwind backstop (F3) now that
                                // the slot is claimed and the writer is
                                // registered; `defuse()` below disables it once
                                // the normal cleanup has run.
                                let mut slot_guard = SessionSlotGuard {
                                    count: sc.clone(),
                                    writers: sw.clone(),
                                    writer: writer_arc.clone(),
                                    armed: true,
                                };
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
                                slot_guard.defuse(); // normal cleanup done
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
                                        let meta = crate::xmodem::YmodemReceiveMeta {
                                            size: rx.declared_size,
                                            modtime: rx.modtime,
                                            mode: rx.mode,
                                        };
                                        // Collision-safe: a name clash is renamed
                                        // DOS/Kermit-style, not dropped.
                                        match TelnetSession::save_received_file_collision_safe(
                                            &dir,
                                            &rx.filename,
                                            &rx.data,
                                            Some(&meta),
                                            rx.resumed,
                                        ) {
                                            Ok(saved_name) => saved.push((saved_name, rx.data.len())),
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
