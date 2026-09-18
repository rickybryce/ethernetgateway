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
pub(crate) use colors::{swap_case_for_petscii, petscii_to_ascii_byte, ascii_to_petscii_byte,
    to_latin1_bytes};
mod gateway;
pub(in crate::telnet) use gateway::{gw_debug_enabled, gateway_terminal_name, gateway_window,
    gateway_window_source};
// Gateway plumbing types/fns referenced only from tests.
#[cfg(test)]
pub(in crate::telnet) use gateway::{GatewayTelnetIac, GatewayIacState, OptState,
    GatewayInboundEvent, REMOTE_PORT_DISPLAY_CAP, read_gateway_event,
    filter_gateway_output, GatewayOutState, GW_FILTER_FLUSH, OSC_TITLE_MAX,
    EscHold, GW_ESC_PAIR,
    normalize_gateway_input, gateway_default_window,
    gateway_input_for_remote, is_gateway_leave_key};
mod io;
pub(crate) use io::{read_byte_iac_filtered, write_telnet_data};

/// Is this the ordinary end of a session rather than a fault?
///
/// A hangup closes our end of the transport, so the session's next write
/// fails.  That is how a call *normally* ends — reported by Ricky as
/// `Serial modem: session error: broken pipe` after every EGT80 hangup, on a
/// call that had ended perfectly well.
///
/// Logging it as an error is worse than noise: it trains the reader to skip
/// the very lines a real fault would appear in.
///
/// One function rather than a `matches!` at each call site.  The rule already
/// existed in `cpm_modem.rs` and was needed at four, which is the shape of
/// defect this project has been bitten by before — a rule written in two
/// places holds in one.
///
/// `ConnectionAborted` is deliberately **not** here: `transfer.rs` uses it as
/// a control signal meaning "tear this session down", so it is a decision the
/// gateway made rather than a peer that went away, and swallowing it would
/// hide a path worth seeing.
pub(crate) fn is_normal_disconnect(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::UnexpectedEof
            | std::io::ErrorKind::ConnectionReset
    )
}
/// Placing the bundled CP/M terminals is reached from `main.rs` at start-up as
/// well as from a session entering the emulator, so it is re-exported rather
/// than staying `pub(in crate::telnet)` like the rest of the CP/M helpers.
pub(crate) use cpm_emu::{place_bundled_terminals, DriveA};
mod session;
pub(crate) use session::is_backspace_key;
// The main menu's optional items, as a named pair -- see its own comment for
// why it is not two `bool` parameters.  Named here rather than only in
// `session` because `main_help_lines` below takes it too: the help page and
// the menu it explains are told the same thing, or they can disagree about
// which items exist.
pub(in crate::telnet) use session::MenuItems;
/// Shared with the web and desktop editors so all three surfaces describe a
/// slave's missing credential in the same words.
pub(crate) use session::master_password_screen_lines;
// Test-only re-exports: `match_terminal_name` is now reached through
// `TelnetSession::note_announced_terminal` (shared by telnet TTYPE and the SSH
// pty request), so production code no longer names it directly.
#[cfg(test)]
pub(crate) use session::{
    can_be_erase_char, match_terminal_name, DEFAULT_ERASE_CHAR, DETECT_PROMPT, DETECT_REPROMPT,
};
/// The main menu's valid-key hint, built from a list because its variants
/// multiply with the CP/M item and the platform.  Test-only re-export.
#[cfg(test)]
pub(in crate::telnet) use session::main_menu_key_hint;
mod transfer;
// The main menu's second page: restarting and shutting down the computer the
// gateway runs on.  Unix only -- see the module comment; the `M` entry, its
// key handler and its half of the valid-key hint are gated to match.
#[cfg(unix)]
mod power;
// Pure seams, referenced only from tests: the confirmation body, the sudo
// error reducer and the three-way elevation answer.
#[cfg(all(unix, test))]
pub(in crate::telnet) use power::{confirm_body, sudo_error_line, Elevate, PowerAction};
mod config_ui;
// Width-aware confirmation formatter; used by config_ui itself and by the
// test that checks every numeric call site's worst case fits the screen.
#[cfg(test)]
pub(in crate::telnet) use config_ui::{cpm_runs_row, numeric_confirmation_lines};
mod serial_ui;
/// The column a two-column row on the port settings screen puts its second
/// key at.  Test-only re-export -- the screen's own rows are built through
/// `serial_menu_row`, which is what the alignment test drives.
#[cfg(test)]
pub(in crate::telnet) use serial_ui::SERIAL_MENU_SECOND_COL;
mod web;
mod aichat_ui;
mod weather;
mod kernel;
mod cpm_emu;
mod cpm_boot_ui;
mod cpm_mount_ui;
mod cpm_modem;
mod cpm_term;
// The emulator's readiness probe and idle-pacing rule, referenced only from tests.
#[cfg(test)]
pub(in crate::telnet) use cpm_emu::{idle_nap, poll_once, IDLE_POLLS_BEFORE_NAP, IDLE_POLLS_LONG};
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
/// RFC 856 TRANSMIT-BINARY.
///
/// **We already behave as though this is on**, and used to refuse to say so.
/// `tnio.rs` deliberately applies no NVT CR-NUL stuffing to a transfer -- see
/// its module comment -- because a file's bytes are 8-bit data, not text.  But
/// the negotiation catch-all refused every option it did not name, so a peer
/// offering BINARY was told `DONT`/`WONT` and then, being NVT-conformant,
/// applied text rules to our binary blocks.
///
/// Measured 2026-09-06 with a real NovaTerm: over a serial link a Punter
/// download of 1775 bytes arrives byte for byte, and through tcpser -- which
/// logs `Disabling telnet binary xmit/recv` after our refusal -- block 0 is
/// rejected for ever.  Same payload, same protocol, same peer.
const OPT_BINARY: u8 = 0x00;
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
/// Content rows a single help page holds (see `show_help_page`).  Chrome is 6
/// rows: sep(1) + title(1) + sep(1) + blank(1) + blank(1) + footer(1).  PETSCII
/// renders 22 usable rows on a 25-line Commodore 64, so 22 - 6 = 16 content
/// rows; we use 15 to leave breathing room for terminals that occasionally push
/// an extra line at the bottom.  Named rather than inlined so the per-screen
/// help tests can assert against the real limit instead of a copy of it.
const HELP_MAX_CONTENT_LINES: usize = 15;
/// How many failed credentials one address may submit before it is banned for
/// [`LOCKOUT_DURATION`].
///
/// **One name, `pub(crate)`.**  There were two, differing only in the order of
/// two words: this one, private, and a `pub(crate) const MAX_AUTH_ATTEMPTS =
/// MAX_AUTH_ATTEMPTS` alias further down the file, added so `ssh.rs` and
/// `webserver.rs` could name the ceiling in their own messages.  They could
/// not disagree about the *value*, being one definition, but a reader could
/// not tell that without finding both -- and the two spellings had already
/// spread across four files and ended up a dozen lines apart inside one test
/// module.  A security bound is the last thing that should have a synonym.
pub(crate) const MAX_AUTH_ATTEMPTS: u32 = 3;
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

// ─── Gateway output filtering ──────────────────────────────
/// How much of a gateway peer's escape traffic reaches the client.
///
/// One value per terminal type rather than a pair of bools: they map 1:1, and
/// two bools beside each other is how a call site silently says the opposite
/// of what it means.
#[derive(Clone, Copy, PartialEq, Eq)]
enum GatewayFilter {
    /// Commodore, reached through a far end that speaks ASCII: translate the
    /// escape sequences that have a PETSCII equivalent (see `crate::petscii`),
    /// drop the rest, and case-swap the text.
    Petscii,
    /// Commodore, reached through a far end that **understands Commodores**:
    /// pass every byte through untouched, in both directions.
    ///
    /// A board doing its own terminal detection -- telnetbible.com, or another
    /// Ethernet Gateway -- recognises the C64 from its erase byte and serves
    /// native PETSCII in its own 40-column layout.  Its text is already
    /// case-swapped for a Commodore and its colour is already PETSCII, so
    /// translating on top of it would swap the case twice and find no escape
    /// sequences to convert.  Selected by `gateway_petscii_translate = false`;
    /// the same judgement as `AT+PETSCII=0` on a dialled connection, and the
    /// only way to express it for a board reachable solely over SSH.
    Raw,
    /// A terminal with no escape handling: strip every sequence, text as-is.
    Ascii,
    /// A terminal that understands escapes.  CSI -- colour, cursor addressing
    /// -- is the whole point of this mode and passes through untouched, and so
    /// does everything else **except** a window-title OSC.
    ///
    /// `ESC ] 0 ; … BEL` sets a title no client of this gateway has, and a
    /// terminal that does not implement OSC swallows the two-byte `ESC ]` and
    /// prints the rest: bash's `\e]0;\u@\h: \w\a` arrives on screen as
    /// `0;ricky@TelnetBible: ~` in front of every prompt.  Measured on a real
    /// SC126 through the SSH gateway, under **both** EGT80 and QTERM -- which
    /// is what rules out a fault in either terminal and puts the fix here.
    ///
    /// Only the title form is taken, and only when it completes, because this
    /// stream also carries file transfers: see `filter_gateway_output`.
    Ansi,
}

// ─── Input mode ────────────────────────────────────────────
#[derive(Clone, Copy)]
enum InputMode {
    /// Normal line input: echo typed characters, trim result.
    Normal,
    /// Password input: echo `*` for each character, no trim.
    Password,
}

// ─── How a line of input ended ─────────────────────────────
/// `get_line_input` answers `Option<String>`, which folds two different
/// facts into one `None`: the user pressed ESC, and the session went away.
/// Most prompts want the same thing for both -- leave -- so they keep using
/// it.  The CP/M emulator's `A>` prompt does not: a single ESC there used to
/// read as a disconnect and drop the user back to the gateway menu, while
/// every other CP/M surface (stopping a transient, leaving a booted disk)
/// needs ESC twice.  Callers that must tell the two apart use
/// `get_line_input_end` and match on this instead.
#[derive(Debug)]
enum LineEnd {
    /// The user pressed Enter; carries the line (trimmed in `Normal` mode).
    Line(String),
    /// The user pressed ESC.  The session is still up.  The burst says what
    /// arrived with it, which a caller pairing ESCs cannot do without: the
    /// drain that clears an escape sequence would otherwise swallow a fast
    /// second ESC, and would make an arrow key indistinguishable from one.
    Escaped(EscBurst),
    /// The session ended -- there is nobody left to prompt.
    Disconnected,
    /// The line ran past `MAX_INPUT_LENGTH` and was refused (the error is
    /// already on screen).  Not an ESC and not a disconnect: a caller that
    /// can re-prompt should, which is what the `A>` prompt does.
    TooLong,
}

/// What arrived in the same burst as an ESC, gathered by the drain that
/// follows one.  The drain has to happen -- an arrow key's `[A` would
/// otherwise be typed into the next prompt as text -- so anything a caller
/// needs to know about the burst has to be collected on the way past.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct EscBurst {
    /// A second ESC was in the burst.  A human double-tapping ESC is far
    /// slower than the 50 ms drain window and so does *not* land here -- this
    /// is the terminal-sent pair (EGT80, a script, a paste).
    another_esc: bool,
    /// Bytes other than ESC were discarded, so this ESC was the head of a
    /// sequence rather than a keypress in its own right: an arrow key sends
    /// `ESC [ A`.  Two arrow presses must not read as ESC ESC.
    sequence: bool,
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

/// Everything a session dialled from inside another session inherits.
///
/// **One object, because carrying these by hand went wrong three times.**
/// `ATDT ethernetgateway` builds a second `TelnetSession` from within the
/// first, and each thing the power page reads had to travel with it: the
/// credential, then the address, then the two counters.  Each was fixed on
/// its own, each looked complete, and each time the test written alongside
/// asked only about the value just added -- so the next one was found by the
/// next review pass instead.  Grouping them means a new input is added here
/// and every dial-out site carries it without being edited.
#[derive(Clone)]
pub(crate) struct Inherited {
    /// Whether the dialling session proved who it is (never decided afresh).
    pub authenticated: bool,
    /// Whether the caller reached this gateway over a relay.
    ///
    /// **Here because the power page reads it, and it did not travel.**  The
    /// refusal on a password-free machine gives a relayed caller different
    /// advice, because a relay can never become authenticated -- but
    /// `new_cpm_menu` clears `is_relay` on the session it builds, so a relay
    /// caller who pressed `K` and typed `ATDT ethernetgateway` was handed the
    /// advice meant for a telnet client: set `security_enabled` and
    /// reconnect, which that session can never satisfy either.  The same
    /// defect surviving one hop, which is the whole reason these inputs are
    /// one object.
    pub is_relay: bool,
    /// The dialler's address; the sudo attempt cap keys on it.
    pub peer_addr: Option<IpAddr>,
    /// The refused-password floor, **shared** so a re-dial cannot reset it,
    /// and stamped so it expires like every other lockout in the product.
    pub power_failures: Arc<Mutex<(u32, std::time::Instant)>>,
    /// The elevation memo, shared for the same reason: it is what stops a
    /// re-dial spawning another real `sudo`.
    #[cfg(unix)]
    pub power_elevation: Arc<Mutex<Vec<(power::PowerAction, power::Elevate)>>>,
}

impl Inherited {
    /// A context for a session nothing dialled: a fresh, unspent allowance.
    ///
    /// The one way to build one from nothing, so a caller cannot half-fill it
    /// -- which is the defect this struct was introduced to end, and which
    /// reappeared twice more after it was.
    pub(crate) fn fresh(authenticated: bool, peer_addr: Option<IpAddr>) -> Self {
        Self {
            authenticated,
            peer_addr,
            is_relay: false,
            power_failures: Arc::new(Mutex::new((0, std::time::Instant::now()))),
            #[cfg(unix)]
            power_elevation: Default::default(),
        }
    }
}

/// Whether a *telnet* session that got past the door has proved who it is.
///
/// **Only the telnet door computes this; every other entry point states its
/// own answer at construction.**  The first version derived it as
/// `is_serial || security_enabled`, and `is_serial` cannot carry a trust
/// decision: it is set by `new_serial`, which really is a physical port, and
/// *also* by `new_relay` -- which `new_cpm_menu` delegates to.  So a CP/M
/// guest dialling `ATDT ethernetgateway` got a session claiming a login it
/// never made, and on a root or NOPASSWD machine with `security_enabled` off
/// an unauthenticated peer could reach the power page through `K` and restart
/// the computer.  That is the hole this flag exists to close, reopened by
/// another route in the same commit that closed it.
///
/// The lesson is about the flag's *name*: `is_serial` reads as "arrived over
/// a serial port", and what it actually means is "does not speak telnet".
/// Reason from what sets a flag, not from what it is called.
///
/// Telnet is the only path that runs `authenticate`, and it runs it exactly
/// when `security_enabled` is on -- so for telnet that setting *is* the
/// answer.
pub(crate) fn telnet_session_is_credentialed(security_enabled: bool) -> bool {
    security_enabled
}

/// Refused `sudo` attempts, per address, for the whole process.
///
/// Lives here rather than in `power.rs` because that module is Unix-only
/// while `TelnetSession` -- which carries the field this backs -- is not.
///
/// **A gateway login is not permission to guess the machine's password, so
/// this cannot be the map a login clears.**  These counters were the shared
/// `LockoutMap` -- the one telnet, SSH and the web UI authenticate against --
/// and `session.rs` clears an address from that map on every *successful*
/// gateway login.  So with `security_enabled` on, the bound was three guesses
/// per login rather than three per address: log in, spend them on the
/// operator's *host* account, hang up, log in again.  At `conn_rate_max`
/// that is the sixty PAM attempts a minute that moving off the per-session
/// field was supposed to have ended, and it failed in the *hardened*
/// configuration -- the default (`security_enabled` off) held only because
/// there is no login to clear anything.
///
/// The two credentials are different things and now have different counters.
/// Nothing clears this one: it expires on `LOCKOUT_DURATION` like any other
/// entry, and that is the only way out.
///
/// **One map for the process, reached through a session field**, because it
/// guards one thing -- this computer's own account -- and every listener that
/// can reach the page must count into the same total.  Sessions default their
/// `power_lockouts` from here, so no constructor can forget it; taking it
/// from a field rather than calling this directly is what lets a test hand a
/// session a map of its own instead of sharing one with every other test in
/// the process.
pub(crate) fn shared_power_lockouts() -> &'static LockoutMap {
    static POWER_LOCKOUTS: std::sync::OnceLock<LockoutMap> =
        std::sync::OnceLock::new();
    POWER_LOCKOUTS.get_or_init(Default::default)
}

/// Whether a failure recorded `age` ago is still inside the lockout window.
///
/// **A named rule taking an age, because `Instant` cannot be faked.**  The two
/// callers below both wrote `when.elapsed() < LOCKOUT_DURATION` inline, and
/// the only way a test could reach that was to backdate a live entry with
/// `Instant::checked_sub` -- which returns `None` when the result would
/// pre-date the platform's monotonic epoch, boot time on Linux.  So on a host
/// up for less than `LOCKOUT_DURATION` (a fresh CI container) both of those
/// tests took an early `return` and counted as passes, which is this project's
/// own definition of a test that cannot go red.  Driven by a supplied
/// `Duration` the rule is exercised on every machine, including its boundary:
/// exactly at the window the entry has expired.
pub(crate) fn within_lockout_window(age: std::time::Duration) -> bool {
    age < LOCKOUT_DURATION
}

/// Whether `count` failures, the last of them `age` ago, mean this address is
/// locked out right now.
pub(crate) fn lockout_is_active(count: u32, age: std::time::Duration) -> bool {
    count >= MAX_AUTH_ATTEMPTS && within_lockout_window(age)
}

pub(crate) fn is_locked_out(lockouts: &LockoutMap, ip: IpAddr) -> bool {
    let map = lockouts.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((count, when)) = map.get(&ip) {
        lockout_is_active(*count, when.elapsed())
    } else {
        false
    }
}

// ─── Connection rate limit ──────────────────────────────────
//
// Shared between telnet and SSH for the same reason `LockoutMap` is: an
// attacker must not reset the counter by bouncing between protocols.
//
// This is a SEPARATE defence from the auth lockout above, not a second
// spelling of it.  The lockout counts *failed credentials*, and the measured
// attack never submits one: a telnet credential scanner opens a connection
// per guess, sends `USER<CRLF>PASS<CRLF>` blind, and hangs up at the first
// prompt it does not recognise -- so `record_auth_failure` is never reached
// and the lockout never arms.  Measured on an internet-exposed gateway
// 2026-09-13: 62 credential attempts from three IPs (71, 48 and 43
// connections each) and *zero* telnet lockouts.  Counting connections
// catches what counting failures structurally cannot.
//
// Deliberately NOT applied to the web listener.  `webserver::write_response`
// sends `Connection: close`, so every HTTP request is its own TCP
// connection, and the booted-disk screen polls `/vdm/frame` every
// `VDM_POLL_MS` (150 ms) on top of `/logs` every 2 s -- a legitimate
// operator watching a CP/M guest would trip any sane per-IP limit within the
// first second.  That listener is gated by the private-IP allowlist
// (`webserver::web_ip_rejection`) instead.
/// One address's recent connections, plus whether we have already said in the
/// log that it is being refused.
///
/// **The flag is here because the log line was the amplifier.**  The refusal
/// itself is careful -- counted, never stored -- but it used to `glog!`
/// unconditionally, and `glog!` is a blocking `write_all` to the log file
/// inline in the accept loop, which is the serialization the comment beside
/// it warns against.  Worse, the log is a rolling 1 MB x 6: a flood loud
/// enough to matter would push its own evidence out of the file.  One line
/// per address per flood keeps the diagnostic and drops the amplifier.
///
/// It rides the entry that already exists rather than taking a map of its
/// own, so a refusal still allocates nothing, and it is cleared whenever the
/// address is accepted again -- a later flood is a new episode and says so.
#[derive(Default)]
pub(crate) struct ConnRate {
    seen: Vec<std::time::Instant>,
    refusal_logged: bool,
}

impl ConnRate {
    /// How many connections are being held for this address.  Test-facing:
    /// the cap on this is what stops the limiter becoming its own
    /// memory-exhaustion vector.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.seen.len()
    }
}

pub(crate) type ConnRateMap = Arc<Mutex<HashMap<IpAddr, ConnRate>>>;

/// Record a connection from `ip` and report how many fall inside `window`,
/// counting this one.  The caller refuses the connection when the answer
/// exceeds its configured maximum.
///
/// A connection already over `max` is counted but **not stored**: the vector
/// is capped at `max` entries per IP, so a flood cannot grow this map into
/// its own memory-exhaustion vector -- a rate limiter that allocates per
/// refused connection is an amplifier, not a defence.  The practical effect
/// is that refusals do not extend the window, so an IP recovers `window`
/// after its last *accepted* connection rather than after its last attempt.
pub(crate) fn note_connection(
    rates: &ConnRateMap,
    ip: IpAddr,
    max: u32,
    window: std::time::Duration,
) -> (u32, bool) {
    let mut map = rates.lock().unwrap_or_else(|e| e.into_inner());
    let now = std::time::Instant::now();
    // Drop IPs with nothing left inside the window, so the map does not grow
    // one entry per distinct scanner for ever on a public instance -- the
    // same sweep `record_auth_failure` does, and for the same reason.
    map.retain(|_, rate| {
        rate.seen
            .iter()
            .any(|t| now.duration_since(*t) < window)
    });
    let rate = map.entry(ip).or_default();
    rate.seen.retain(|t| now.duration_since(*t) < window);
    if rate.seen.len() as u32 >= max {
        // Over the limit: report it without storing, per the cap above.
        // `first` is true exactly once per flood, so the caller's log line
        // cannot outpace the flood it is describing.
        let first = !rate.refusal_logged;
        rate.refusal_logged = true;
        return (rate.seen.len() as u32 + 1, first);
    }
    // Back under the limit: a later refusal is a new episode and is allowed
    // to say so again.
    rate.refusal_logged = false;
    rate.seen.push(now);
    (rate.seen.len() as u32, false)
}

pub(crate) fn record_auth_failure(lockouts: &LockoutMap, ip: IpAddr) -> u32 {
    let mut map = lockouts.lock().unwrap_or_else(|e| e.into_inner());
    // Drop entries past the lockout window so the map doesn't grow one
    // entry per distinct attacker IP forever on a long-running public
    // instance.  After this sweep, every surviving entry is within the
    // active window, so a fresh `or_insert` below either reuses a
    // still-counting entry or starts a new one.
    map.retain(|_, (_, when)| within_lockout_window(when.elapsed()));
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

/// Check an IPv4 address against private/loopback/link-local ranges and the
/// router restriction.  Returns the rejection reason, or None if allowed.
///
/// `routers` is what the OS says this machine's default route(s) point at (see
/// [`crate::router`]); empty means we could not find out, and the rule falls
/// back to the historical `.1` assumption.
fn reject_insecure_ipv4(
    octets: [u8; 4],
    block_gateway: bool,
    routers: &[IpAddr],
) -> Option<String> {
    let is_private = octets[0] == 10
        || (octets[0] == 172 && (16..=31).contains(&octets[1]))
        || (octets[0] == 192 && octets[1] == 168)
        || octets[0] == 127
        || (octets[0] == 169 && octets[1] == 254); // link-local
    if !is_private {
        return Some(
            "Connection refused: security is disabled, only private IP addresses are allowed."
                .to_string(),
        );
    }
    // Traffic that appears to come from this subnet's router *may* have been
    // forwarded in from outside, which is why refusing it is offered at all —
    // but it is just as often an administrator's own machine, or hairpinned
    // traffic from inside the LAN, so it is allowed unless the operator asks
    // for the strict behaviour.  Loopback keeps its exemption: 127.0.0.1 is
    // this machine, not a router.
    if block_gateway && octets[0] != 127 {
        let addr = IpAddr::V4(std::net::Ipv4Addr::from(octets));
        if crate::router::is_router(addr, routers) {
            // The address the OS actually routes through — no guessing.
            return Some(format!(
                "Connection refused: this network's router ({}) is not allowed by this gateway's settings.",
                addr
            ));
        }
        // Fallback for a host where the router could not be determined: the
        // old convention, so the setting is never silently weaker than before.
        //
        // Keyed on whether we know *this family's* router, not on the list
        // being empty: a host where only an IPv6 default route was found still
        // has an unknown IPv4 router, and dropping the fallback there would
        // quietly stop enforcing what the operator asked for.
        let know_v4_router = routers.iter().any(|r| r.is_ipv4());
        if !know_v4_router && octets[3] == 1 {
            return Some(
                "Connection refused: gateway addresses (*.*.*.1) are not allowed by this gateway's settings."
                    .to_string(),
            );
        }
    }
    None
}

/// When security is disabled, only allow connections from private/loopback IPs.
///
/// `block_gateway` (the `disable_gateway_connections` setting, **off** by
/// default) additionally refuses any address ending in `.1` — typically this
/// subnet's router — except loopback, which is always allowed.  Returns the
/// rejection reason, or `None` if the address is allowed.
pub(crate) fn reject_insecure_ip(ip: IpAddr, block_gateway: bool) -> Option<String> {
    reject_insecure_ip_with(ip, block_gateway, &crate::router::cached_addrs())
}

/// The rule itself, with the router list passed in so it can be tested without
/// depending on the host's own routing table.
pub(crate) fn reject_insecure_ip_with(
    ip: IpAddr,
    block_gateway: bool,
    routers: &[IpAddr],
) -> Option<String> {
    match ip {
        IpAddr::V4(v4) => reject_insecure_ipv4(v4.octets(), block_gateway, routers),
        IpAddr::V6(v6) => {
            // IPv4-mapped IPv6 (::ffff:x.x.x.x) — apply IPv4 rules
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return reject_insecure_ipv4(mapped.octets(), block_gateway, routers);
            }
            if v6.is_loopback() {
                return None;
            }
            // An IPv6 router is refused by the same rule.  There is no `.1`
            // convention to fall back on here, so this only ever fires on a
            // host where detection worked.
            if block_gateway && crate::router::is_router(ip, routers) {
                return Some(format!(
                    "Connection refused: this network's router ({}) is not allowed by this gateway's settings.",
                    ip
                ));
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
            Some(
                "Connection refused: security is disabled, only private IP addresses are allowed."
                    .to_string(),
            )
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

use crate::webbrowser::{truncate_path_to_width, truncate_to_width};

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

// Inside the data directory with everything else the gateway creates (see
// `config::DATA_DIR`).  Missed in the first pass of that move: it is runtime
// state -- accumulated host fingerprints -- and leaving it at the top level
// would have gone on littering the launch directory with the one file the
// change existed to stop scattering.
const GATEWAY_HOSTS_FILE: &str = "ethernetgateway-data/gateway_hosts";

/// Result of checking a host key against the known-hosts file.
pub(crate) enum HostKeyStatus {
    /// Key matches a stored entry.
    Known,
    /// No entry for this host:port.
    Unknown,
    /// Stored key does not match the presented key.
    Changed,
    /// The known-hosts file exists but could not be read (permissions, I/O
    /// error).  Deliberately distinct from `Unknown`: a file we cannot read
    /// may well hold a pin for this host, so treating it as "never seen
    /// before" would silently downgrade to trust-on-first-use — and on the
    /// relay path, which auto-pins without prompting, that means re-pinning
    /// whatever key is presented right before sending the master's
    /// credentials.  Callers must fail rather than pin.
    Unreadable(std::io::Error),
}

/// The host key a server presented, **if it is one this gateway can pin**.
///
/// russh 0.63 widened `client::Handler::check_server_key` from a `PublicKey` to
/// a `PublicKeyOrCertificate`, so the one question both our SSH clients ask --
/// "which key do I check against `gateway_hosts`?" -- now has an answer that is
/// sometimes *none*.  Both handlers go through here, because they are one rule
/// in two places (`relay.rs`'s slave->master link and the SSH Gateway) and a
/// rule written twice is a rule that disagrees with itself eventually.
///
/// **A certificate is refused, not unwrapped**, and that is the whole point of
/// this function.  The obvious migration -- take the `PublicKey` inside the
/// certificate and pin that -- looks like it preserves behaviour and does not:
/// russh's own source says it plainly, that "the key inside a certificate is
/// not something the client was ever told to trust".  Pinning it would mean
/// trust-on-first-use over a key no CA statement was ever checked for, on the
/// two paths where a MITM is most expensive -- the SSH Gateway carries the
/// user's typed remote password, and the relay is about to send the master's
/// unified credentials.  `None` instead lands on each caller's existing
/// "presented no host key" arm, which disconnects.  Fail closed.
///
/// **It cannot fire today**, and it is here so that it already exists when it
/// can: `russh::client::Config::default()` leaves `preferred.host_key_
/// certificates` empty, so neither client offers a `*-cert-v01@openssh.com`
/// host-key algorithm and no server can negotiate one.  The day somebody adds
/// certificate support to that list, the refusal is already written -- rather
/// than the inner key being pinned silently by a `match` arm nobody revisited.
pub(crate) fn pinnable_host_key(
    presented: &russh::keys::PublicKeyOrCertificate,
) -> Option<russh::keys::PublicKey> {
    match presented {
        russh::keys::PublicKeyOrCertificate::PublicKey { key, .. } => Some(key.clone()),
        russh::keys::PublicKeyOrCertificate::Certificate(cert) => {
            // Logged here rather than at the two call sites, so the reason is
            // stated once.  Without it the operator sees only each caller's
            // generic "could not verify server host key", which is true and
            // says nothing about why.
            glog!(
                "SSH: host presented a certificate ({}), not a plain host key -- \
                 refusing: this gateway pins keys in {} and has no CA to check a \
                 certificate against",
                cert.algorithm(),
                GATEWAY_HOSTS_FILE
            );
            None
        }
    }
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
    classify_known_host(
        std::fs::read_to_string(GATEWAY_HOSTS_FILE),
        &format!("{}:{}", host, port),
        &format_host_key(key),
    )
}

/// Forget the pinned key for one host, so the next connection pins afresh.
///
/// **The remedy for a changed host key, and it is deliberately an action an
/// operator takes rather than something the gateway does for itself.** A key
/// that changed because a master was reinstalled and a key that changed because
/// somebody is sitting in the middle are indistinguishable from here -- that is
/// what pinning is *for* -- so the gateway reports the problem and offers the
/// fix, and a human decides. See [`crate::resolve`].
///
/// Returns whether anything was removed, so a caller can tell "fixed" from
/// "there was nothing pinned" without reading the file itself.
pub(crate) fn forget_known_host(host: &str, port: u16) -> std::io::Result<bool> {
    let lookup = format!("{}:{}", host, port);
    let content = match std::fs::read_to_string(GATEWAY_HOSTS_FILE) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    let (kept, removed) = without_known_host(&content, &lookup);
    if removed == 0 {
        return Ok(false);
    }
    crate::config::ensure_parent_dir(GATEWAY_HOSTS_FILE);
    std::fs::write(GATEWAY_HOSTS_FILE, kept)?;
    Ok(true)
}

/// Drop every entry for `lookup`, returning the new text and how many went.
///
/// Pure, because the interesting cases are textual: a comment that happens to
/// begin with the host, a host that is a *prefix* of another (`10.0.0.1:22`
/// must not match `10.0.0.1:2222`), and a file whose last line has no newline.
/// The prefix case is why this matches on the whole first field rather than
/// with `starts_with`.
pub(in crate::telnet) fn without_known_host(content: &str, lookup: &str) -> (String, usize) {
    let mut removed = 0;
    let kept: Vec<&str> = content
        .lines()
        .filter(|line| {
            let t = line.trim();
            if t.is_empty() || t.starts_with('#') {
                return true;
            }
            // The first whitespace-separated field is the host:port.
            if t.split_whitespace().next() == Some(lookup) {
                removed += 1;
                return false;
            }
            true
        })
        .collect();
    let mut out = kept.join("\n");
    // A file that had a trailing newline keeps one; an empty result is empty
    // rather than a lone newline.
    if !out.is_empty() && content.ends_with('\n') {
        out.push('\n');
    }
    (out, removed)
}

/// The decision half of [`check_known_host`], split out so the
/// file-read outcome can be supplied directly.
///
/// The read is passed in as its `Result` because the interesting cases are
/// the *failures*, and `GATEWAY_HOSTS_FILE` is a process-relative path shared
/// by every session — a test that chmod'ed the real file would race every
/// other test in the binary.
pub(in crate::telnet) fn classify_known_host(
    read: Result<String, std::io::Error>,
    lookup: &str,
    key_str: &str,
) -> HostKeyStatus {
    let content = match read {
        Ok(c) => c,
        // No file yet is the ordinary first-run case: pin on first contact.
        // Anything else means a file that may hold a pin we just can't see.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return HostKeyStatus::Unknown;
        }
        Err(e) => return HostKeyStatus::Unreadable(e),
    };

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix(lookup)
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

    // An unreadable-but-present file must NOT be treated as empty: this
    // function rewrites the whole file, so `unwrap_or_default()` here would
    // replace every other pinned host with this single entry.  Only a missing
    // file legitimately starts from empty.
    let mut content = match std::fs::read_to_string(GATEWAY_HOSTS_FILE) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            glog!(
                "Warning: not saving gateway host key — {} exists but could \
                 not be read ({}); rewriting it would discard the host keys \
                 already pinned there.",
                GATEWAY_HOSTS_FILE,
                e
            );
            return;
        }
    };
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
    // The directory must exist before anything in it can be written.
    // `main` creates it at startup, but relying on that alone was wrong twice
    // over: a unit test reaches this writer without going through `main` (which
    // is how the missing directory was found), and an operator can remove the
    // folder while the gateway is running.  The parent of the path we are about
    // to write, never a constant -- see `config::ensure_parent_dir`.
    crate::config::ensure_parent_dir(path);
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

/// The outcome of the last file transfer, shown once on the next menu draw.
///
/// **A vintage terminal takes the screen for a transfer and restores it
/// afterwards**, so anything printed at the instant a transfer ends is thrown
/// away: measured on a C64 under NovaTerm 9.6c, a byte-perfect XMODEM-1K
/// download left the user looking at the restored "Start XMODEM-1K receive
/// now" text with no sign it had worked.  No amount of waiting fixes that --
/// the pause before printing was already a guess, and lengthening it only
/// moves the race.
///
/// So the result is carried to the next screen the terminal actually draws.
/// Whatever it did during the transfer, the menu it returns to says what
/// happened.  Named fields rather than a `(bool, String)` so the pair cannot
/// be swapped at a call site.
pub(in crate::telnet) struct TransferNote {
    /// Whether the transfer succeeded, which picks the colour.
    pub(in crate::telnet) ok: bool,
    /// One line, already phrased for a 40-column screen.
    pub(in crate::telnet) text: String,
}

pub(crate) struct TelnetSession {
    reader: Box<dyn tokio::io::AsyncRead + Unpin + Send>,
    writer: SharedWriter,
    shutdown: Arc<AtomicBool>,
    restart: Arc<AtomicBool>,
    current_menu: Menu,
    terminal_type: TerminalType,
    /// Whether colorized output is emitted.  Kept separate from
    /// `terminal_type` so declining color does not discard the terminal's
    /// encoding/layout: a PETSCII (C64) caller who answers "no color" stays
    /// PETSCII — 40 columns, case-swapped, ANSI-stripped on gateways — and
    /// merely gets plain text instead of PETSCII color codes.  Set from the
    /// color prompt in `detect_terminal_type`; the color helpers
    /// (`green`/`red`/…) return the text unchanged when this is false.
    color_enabled: bool,
    erase_char: u8,
    lockouts: LockoutMap,
    peer_addr: Option<IpAddr>,
    /// Refused `sudo` passwords on the second page, for a session that has no
    /// address.
    ///
    /// **The floor under the per-IP cap, for the sessions that cap cannot
    /// see.**  `power.rs` counts a refusal against the shared `LockoutMap`,
    /// which is keyed by `IpAddr` -- so a session with none (a caller on the
    /// modem, a CP/M guest dialling `ATDT ethernetgateway`) was counted
    /// nowhere and could guess the operator's *system* password without
    /// bound.  A per-session field is exactly what the per-IP counter
    /// replaced, and it is the wrong shape for an address -- reconnecting
    /// resets it -- but a caller with no address has no cheaper way to
    /// reconnect than the one they are already on, and three per connection
    /// is what every session had before.
    ///
    /// Read only under `#[cfg(unix)]`, like `MenuItems::second_page` and for
    /// the same reason: the page and everything on it are Unix-only, so off
    /// Unix this field has no reader and `-D warnings` calls it dead.  The
    /// attribute sits on the field rather than `cfg`-ing it away, because the
    /// last field here that carried a `cfg` orphaned it onto its neighbour
    /// when it was removed and failed the Windows build.
    #[cfg_attr(not(unix), allow(dead_code))]
    power_password_failures: Arc<Mutex<(u32, std::time::Instant)>>,
    /// Refused `sudo` attempts per address, for the power page only.
    ///
    /// **Separate from `lockouts` because a gateway login is not permission
    /// to guess the machine's password.**  `session.rs` clears an address
    /// from `lockouts` on every *successful* gateway login, so while these
    /// two shared one map the bound was three guesses per login rather than
    /// three per address -- log in, spend them on the operator's *host*
    /// account, hang up, log in again.  It failed in the hardened
    /// configuration and held in the default one only because
    /// `security_enabled` off means there is no login to clear anything.
    ///
    /// Defaulted from one process-wide map (`power::shared_power_lockouts`)
    /// rather than threaded from `main`, so every listener counts into the
    /// same total and no constructor can forget it -- while a test can still
    /// hand a session a map of its own, which a bare global would not allow.
    ///
    /// Read only under `#[cfg(unix)]`, and carrying the attribute rather than
    /// a `cfg` for the same reason as the field above it.
    #[cfg_attr(not(unix), allow(dead_code))]
    power_lockouts: LockoutMap,
    /// Whether this session actually proved a credential to get here.
    ///
    /// **Recorded, not derived, because a setting is not an outcome.**  The
    /// obvious test is `cfg.security_enabled && !is_serial`, but
    /// `security_enabled` is read fresh and can be changed while a session is
    /// open: an operator turning it off would make a session that *did*
    /// authenticate look as though it had not, and turning it on would let
    /// one that never authenticated claim it had.  What matters is what
    /// happened at the door, so the door writes it down.
    ///
    /// Used by the power page, where `Elevate::Direct` and
    /// `Elevate::SudoQuiet` ask for no password at all -- so without this an
    /// unauthenticated peer could restart the computer with no credential of
    /// any kind.
    #[cfg_attr(not(unix), allow(dead_code))]
    authenticated: bool,
    /// How the human reached this gateway, for the power page's refusal only.
    ///
    /// Distinct from `is_relay`, which `new_cpm_menu` clears so the caller is
    /// not *labelled* a slave: this one answers "can this person act on the
    /// advice we are about to give them?", and a relayed caller cannot become
    /// authenticated however they reconnect.  Two questions that were sharing
    /// one field, so a relay caller who dialled the menu from inside the CP/M
    /// emulator got advice that could never work.
    #[cfg_attr(not(unix), allow(dead_code))]
    power_arrived_by_relay: bool,
    /// What the elevation probe answered for this session, once it has.
    ///
    /// **The probe is a process spawn, and nothing else bounded how often it
    /// ran.**  The attempt cap counts *refused passwords*, and a caller who
    /// confirms and then cancels -- ESC, or a bare Enter -- submits nothing,
    /// so it records nothing: `R`, `Y`, Enter looped indefinitely, spawning a
    /// real `sudo -k -n -l` per pass.  On a machine whose service account is
    /// not in sudoers each one writes an authentication failure into the
    /// *host's* auth log, and with `security_enabled` off -- the default --
    /// the peer need not have authenticated to do it.  Moving the cap above
    /// the probe was necessary and not sufficient; this is the other half.
    ///
    /// Caching is sound because the probe asks about sudoers, root and
    /// `no_new_privs`, none of which a session can change: within one session
    /// the honest answer is the same every time.  An operator who edits
    /// sudoers mid-session reconnects, which is what `available()` already
    /// expects of them for the cheaper question it caches at startup.
    ///
    /// **Keyed by the action, because the probe is.**  `probe_elevation` asks
    /// `sudo -l -- shutdown -r now` or `-h now`, and sudoers rules are
    /// per-argument: `NOPASSWD: /sbin/shutdown -h now` alone is a perfectly
    /// ordinary rule.  A memo keyed on the session alone would answer Restart
    /// with what Shutdown was told -- skipping the password, clocking out the
    /// farewell, and only then having `sudo` refuse, leaving the refusal in
    /// the log and nowhere else.  That is precisely what this module's
    /// order-of-steps rule exists to prevent, so the two actions get two
    /// slots.
    ///
    /// A real `cfg` rather than the `allow(dead_code)` its neighbours carry,
    /// and necessarily so: `Elevate` is declared in `power.rs`, which does
    /// not exist off Unix, so there is no type here to leave unread.  Keep
    /// the attribute and this comment together if it is ever removed -- that
    /// is what orphaned a doc onto a neighbour and failed the Windows build
    /// before.
    #[cfg(unix)]
    power_elevation: Arc<Mutex<Vec<(power::PowerAction, power::Elevate)>>>,
    transfer_subdir: String,
    xmodem_iac: bool,
    /// Outcome of the last transfer, drawn once by `render_file_transfer`
    /// and cleared -- see `TransferNote`.
    pub(in crate::telnet) last_transfer_note: Option<TransferNote>,
    /// Ignore input for a moment on the next menu prompt.
    ///
    /// **A protocol's teardown keeps arriving after the transfer prompt has
    /// been answered, and the screen behind that prompt is a MENU.**  Punter's
    /// C1 handshake codes are literal ASCII words, so their letters are menu
    /// keys: `GOO` put a `G` on the File Transfer menu (Gateway Shell), and
    /// once that was drained the `D` of `BAD` selected Download a file, which
    /// walked off the very screen carrying the result the operator was meant
    /// to read.  Draining before and after the keypress cannot fix this,
    /// because the bytes are still trickling out when the menu is drawn.
    ///
    /// So the first menu prompt after a transfer is armed the same way the
    /// keypress prompt is -- on the same reasoning, that nobody reads a fresh
    /// screen and chooses from it inside the window.  Set by
    /// `press_any_key_after_transfer` and taken by the menu loop, so it can
    /// only ever suppress one prompt.
    pub(in crate::telnet) arm_next_prompt: bool,
    web_lines: Vec<String>,
    web_scroll: usize,
    web_links: Vec<String>,
    web_history: Vec<(String, usize)>,
    web_url: Option<String>,
    /// Whether the configured homepage has been auto-loaded this visit.
    ///
    /// A *failed* fetch leaves `web_lines` and `web_url` both empty, which is
    /// exactly the "first visit" condition `render_web_browser` tests -- so
    /// with an unreachable `browser_homepage` every redraw started another
    /// blocking fetch and made the user wait out the timeout again.  Cleared
    /// by `web_reset`, so leaving and re-entering the browser does retry.
    web_home_tried: bool,
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
    // Cancel-safety for `session_read_byte`: set true after an IAC byte is
    // consumed but before its command byte is read, so a cancelled read (e.g.
    // the CP/M out-of-band drain's zero-timeout) resumes at the command byte
    // instead of losing the IAC and desyncing telnet parsing.
    mid_iac_cmd: bool,
    // RFC 854 NVT: a bare CR is sent as `CR NUL`, so the NUL that follows a CR
    // is an encoding artefact and not a keystroke.  Set when a CR is handed to a
    // caller, cleared by the next byte.  Telnet only -- a serial or SSH client
    // sends CR alone, and a NUL from those is the peer's own byte.
    //
    // Why it matters: a booted CP/M guest is handed raw keys, and its CCP echoes
    // control characters, so the stray NUL printed as `^@` at the next prompt --
    // but only after a command that did no console I/O to swallow it first,
    // which is why `DIR` looked clean and re-selecting the current drive did not.
    last_was_cr: bool,
    // Telnet option negotiation state. Each per-option flag records a
    // reply we've already sent so we never loop on repeated requests.
    neg_sent_will: Box<[bool; 256]>,
    neg_sent_do: Box<[bool; 256]>,
    neg_sent_wont: Box<[bool; 256]>,
    neg_sent_dont: Box<[bool; 256]>,
    // **"The terminal question is settled"**, whichever way it was settled:
    // by an `SB TTYPE IS` we recognised, or by the BACKSPACE keypress.  It
    // prevents re-requesting the name, it is what `detect_terminal_type` falls
    // back to when nobody presses a key, and -- once detection has concluded --
    // it is what stops a LATE announcement overwriting the answer the machine
    // itself gave.  On telnet it no longer skips the prompt: that is the whole
    // point of asking a Commodore whose modem calls itself a VT100.
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
    /// Is the byte trace armed for this session?
    ///
    /// **Read once here, not per byte.**  It follows `gateway_debug`, whose
    /// accessor takes a global mutex -- fine at the rate the gateway proxy and
    /// the modem consult it, and badly wrong on `session_read_byte`, which the
    /// CP/M out-of-band drain polls once per CPU batch.  A batch ends at every
    /// BDOS/BIOS trap, and this project has measured 6.4M of those a second,
    /// so reading the flag per byte put millions of global mutex acquisitions
    /// a second on the emulator's hottest path -- the same shape as the 1.1 ms
    /// timer that once capped output at 840 char/s.
    ///
    /// Caching it per session is also what the key already promises: its
    /// documentation says "read fresh by each session", so a per-byte read was
    /// over-delivering on a contract nobody asked for.
    trace_bytes: bool,
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
        from: Inherited,
    ) -> Self {
        Self {
            reader,
            writer,
            shutdown,
            restart,
            current_menu: Menu::Main,
            terminal_type: TerminalType::Ascii,
            color_enabled: true,
            erase_char: session::DEFAULT_ERASE_CHAR,
            lockouts,
            // **Stated, not inherited, and that is the one field here that
            // is.**  A physical port has no peer, so this is a fact about the
            // constructor rather than something the context could tell us --
            // `Inherited::fresh(.., None)` is what its only caller passes, and
            // reading `from.peer_addr` here would invite a later caller to
            // hand a serial session an address that does not exist.  Written
            // down because the field *is* carried everywhere else, and an
            // unexplained `None` beside five inherited values reads as the
            // omission that has now been found twice.
            peer_addr: None,
            power_password_failures: from.power_failures,
            power_lockouts: shared_power_lockouts().clone(),
            // A physical serial port is its own trust boundary -- the same
            // judgement `run` makes where it skips authentication for one --
            // but the *caller* supplies it, along with the power counters.
            //
            // **Because `ATDT ethernetgateway` on the physical modem builds
            // one of these per dial.**  While this constructor made its own,
            // every re-dial handed out a fresh allowance of guesses at the
            // host password and a fresh elevation memo, which is the hole
            // closed for the CP/M virtual modem by the sibling path.  The
            // modem holds one `Inherited` for the life of the port and hands
            // it over each time.
            authenticated: from.authenticated,
            power_arrived_by_relay: from.is_relay,
            #[cfg(unix)]
            power_elevation: from.power_elevation,
            transfer_subdir: String::new(),
            xmodem_iac: false,
            last_transfer_note: None,
            arm_next_prompt: false,
            web_lines: Vec::new(),
            web_scroll: 0,
            web_links: Vec::new(),
            web_history: Vec::new(),
            web_url: None,
            web_home_tried: false,
            web_title: None,
            web_forms: Vec::new(),
            weather_location: config::get_config().weather_location,
            is_serial: true,
            is_relay: false,
            serial_port_id: Some(port_id),
            is_ssh: false,
            idle_timeout: std::time::Duration::from_secs(config::get_config().idle_timeout_secs),
            pushback: None,
            mid_iac_cmd: false,
            last_was_cr: false,
            neg_sent_will: Box::new([false; 256]),
            neg_sent_do: Box::new([false; 256]),
            neg_sent_wont: Box::new([false; 256]),
            neg_sent_dont: Box::new([false; 256]),
            ttype_matched: false,
            ttype_raw: None,
            telnet_negotiated: false,
            window_width: None,
            window_height: None,
            trace_bytes: cpm_emu::keytrace_on(),
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
            color_enabled: true,
            erase_char: session::DEFAULT_ERASE_CHAR,
            lockouts,
            peer_addr,
            power_password_failures: Arc::new(Mutex::new((0, std::time::Instant::now()))),
            power_lockouts: shared_power_lockouts().clone(),
            // **Sound, but not for the reason first written here.**  That
            // said `auth_password` is the only method offered; it is not --
            // `auth_publickey` accepts an enrolled relay key.  What makes
            // this `true` correct is that `shell_request` *refuses* a
            // key-authenticated connection ("a relay key is not a login"), so
            // the only connection that reaches a session at all is one that
            // passed the password, whatever `security_enabled` says.
            //
            // That invariant lives in `ssh.rs` and is tied to this line by
            // nothing but the two comments, so
            // `test_ssh_trust_rests_on_the_shell_refusing_a_relay_key` holds
            // them together: relax that refusal to let a slave open a menu
            // and an enrolled key would silently acquire power-page trust.
            //
            // Set here because `run`'s door is the telnet one and skips SSH.
            authenticated: true,
            power_arrived_by_relay: false,
            #[cfg(unix)]
            power_elevation: Default::default(),
            transfer_subdir: String::new(),
            xmodem_iac: false,
            last_transfer_note: None,
            arm_next_prompt: false,
            web_lines: Vec::new(),
            web_scroll: 0,
            web_links: Vec::new(),
            web_history: Vec::new(),
            web_url: None,
            web_home_tried: false,
            web_title: None,
            web_forms: Vec::new(),
            weather_location: config::get_config().weather_location,
            is_serial: false,
            is_relay: false,
            serial_port_id: None,
            is_ssh: true,
            idle_timeout: std::time::Duration::from_secs(config::get_config().idle_timeout_secs),
            pushback: None,
            mid_iac_cmd: false,
            last_was_cr: false,
            neg_sent_will: Box::new([false; 256]),
            neg_sent_do: Box::new([false; 256]),
            neg_sent_wont: Box::new([false; 256]),
            neg_sent_dont: Box::new([false; 256]),
            ttype_matched: false,
            ttype_raw: None,
            telnet_negotiated: false,
            window_width: None,
            window_height: None,
            trace_bytes: cpm_emu::keytrace_on(),
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
            color_enabled: true,
            erase_char: session::DEFAULT_ERASE_CHAR,
            lockouts,
            peer_addr,
            power_password_failures: Arc::new(Mutex::new((0, std::time::Instant::now()))),
            power_lockouts: shared_power_lockouts().clone(),
            // **A relay key is not a login, and this is where that rule was
            // not being kept.**  `shell_request` refuses a key-authenticated
            // connection precisely because "the interactive menu stays behind
            // the password" -- but `exec_request` has no such gate, and
            // `serial-relay <port>` defaults to the `menu` target, which
            // builds a session right here.  So a holder of an enrolled relay
            // key could reach the power page on a root or NOPASSWD master
            // with no password anywhere in the story.
            //
            // The `menu` target is a designed feature, not the bug -- it is
            // how a caller on a slave's serial port reaches the master's
            // menu -- so the fix is not to refuse it.  It is that a relayed
            // caller has not authenticated *to this gateway*: the slave has.
            // That distinction costs nothing on an ordinary master, where
            // `sudo` asks for a password exactly as before, and closes the
            // no-password case, where nothing else would have asked.
            //
            // `new_cpm_menu` overrides this, because a locally dialled menu
            // session is only as credentialed as the session that dialled it.
            authenticated: false,
            power_arrived_by_relay: true,
            #[cfg(unix)]
            power_elevation: Default::default(),
            transfer_subdir: String::new(),
            xmodem_iac: false,
            last_transfer_note: None,
            arm_next_prompt: false,
            web_lines: Vec::new(),
            web_scroll: 0,
            web_links: Vec::new(),
            web_history: Vec::new(),
            web_url: None,
            web_home_tried: false,
            web_title: None,
            web_forms: Vec::new(),
            weather_location: config::get_config().weather_location,
            is_serial: true,
            is_relay: true,
            serial_port_id: None,
            is_ssh: false,
            idle_timeout: std::time::Duration::from_secs(config::get_config().idle_timeout_secs),
            pushback: None,
            mid_iac_cmd: false,
            last_was_cr: false,
            neg_sent_will: Box::new([false; 256]),
            neg_sent_do: Box::new([false; 256]),
            neg_sent_wont: Box::new([false; 256]),
            neg_sent_dont: Box::new([false; 256]),
            ttype_matched: false,
            ttype_raw: None,
            telnet_negotiated: false,
            window_width: None,
            window_height: None,
            trace_bytes: cpm_emu::keytrace_on(),
        }
    }


    /// What a session dialled from inside this one inherits.
    ///
    /// The single place these are gathered, so a dial-out site cannot carry
    /// some of them and leave the rest -- which is how the same hole was
    /// reopened twice, once per value.
    pub(crate) fn inheritable(&self) -> Inherited {
        Inherited {
            authenticated: self.authenticated,
            peer_addr: self.peer_addr,
            is_relay: self.is_relay,
            power_failures: self.power_password_failures.clone(),
            #[cfg(unix)]
            power_elevation: self.power_elevation.clone(),
        }
    }

    /// Create a session for a call the **CP/M emulator's virtual modem** placed
    /// to this gateway's own menu (`ATDT ethernetgateway` from inside a CP/M
    /// terminal such as EGT8080).
    ///
    /// The bytes cross an in-memory duplex to a guest that speaks to a UART, so
    /// they carry raw serial semantics end to end — `new_relay`'s I/O behavior
    /// exactly (`is_serial = true`, no IAC, no local serial port), which is why
    /// this defers to it.  It is not a slave relay though: `is_relay` is cleared
    /// so the Troubleshooting screen doesn't label the caller "Relay (slave)".
    ///
    /// **It inherits its dialler's credential state and address; it decides
    /// neither for itself.**  An earlier version of this comment argued that
    /// an `is_serial` session needs no credential because "whoever dialed it
    /// is already inside a session that passed the gate" -- and with
    /// `security_enabled` off, the shipped default, no gate was passed.  That
    /// reasoning is what let an unauthenticated peer reach the power page
    /// through `K` on a root or NOPASSWD machine.
    ///
    /// The address travels for the same reason the credential does: the sudo
    /// attempt cap keys on `peer_addr`, and a session without one falls back
    /// to a per-session floor that a re-dial resets.  Passing `None` here
    /// made `ATDT ethernetgateway` a way to buy three more guesses at the
    /// operator's host account, as often as the guest cared to hang up and
    /// dial again -- and put no originating address in the log.
    ///
    /// For the same reason it is neither counted against the telnet session cap
    /// nor registered in [`SessionWriters`], exactly as the physical modem's
    /// `dial_ethernet_gateway` is not: this is not an inbound connection but a
    /// second screen for a caller already counted once.  A shutdown still ends
    /// it — the shared `shutdown` flag is passed in, and the caller's own
    /// session is registered, so the goodbye reaches the human either way.
    pub(in crate::telnet) fn new_cpm_menu(
        reader: Box<dyn tokio::io::AsyncRead + Unpin + Send>,
        writer: SharedWriter,
        shutdown: Arc<AtomicBool>,
        restart: Arc<AtomicBool>,
        lockouts: LockoutMap,
        from: Inherited,
    ) -> Self {
        let mut s =
            Self::new_relay(reader, writer, shutdown, restart, from.peer_addr, lockouts);
        s.is_relay = false;
        // **Only as credentialed as whoever dialled it.**  This session is
        // built by `cpm_modem`'s `ATDT ethernetgateway`, from inside another
        // session, and it inherits `new_relay`'s raw-serial semantics -- so
        // it also inherited `is_serial: true`, which the first version of the
        // credential rule read as "trusted".  It is not: with
        // `security_enabled` off, an unauthenticated peer could reach the
        // power page through `K` and restart a root or NOPASSWD machine
        // having proved nothing.  `is_serial` says "does not speak telnet",
        // never "is trusted".
        s.authenticated = from.authenticated;
        // `is_relay` is cleared above so the Troubleshooting and screen-list
        // labels do not call this caller a slave -- but the power page needs
        // to know how the human actually reached the gateway, because a relay
        // cannot act on the advice a telnet client is given.  Two different
        // questions that were sharing one field.
        s.power_arrived_by_relay = from.is_relay;
        // **Shared, not copied, or a re-dial buys a fresh allowance.**  Two
        // more per-session values feed the power page, and both were reset by
        // building this session: the refused-password floor that bounds a
        // caller with no address, and the elevation memo that bounds probe
        // spawns.  So `ATDT ethernetgateway` -> `2` -> `R` handed out three
        // more real PAM attempts and one more real `sudo` per dial, and
        // `ATDT` dials are deliberately not counted against `conn_rate_max`.
        //
        // Copying would not do: the guest's guesses have to count against the
        // caller who dialled, so returning from the menu cannot restore them.
        // They share one allowance.
        s.power_password_failures = from.power_failures;
        #[cfg(unix)]
        {
            s.power_elevation = from.power_elevation;
        }
        s
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
                "",
                "  Press ENTER after a command.",
                "  Type more than one character",
                "  and it is taken as a URL or",
                "  a search instead.",
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
                "",
                "  Commands take effect on ENTER.  Anything",
                "  longer than a single character is treated",
                "  as a URL or a search, so you can type an",
                "  address at the prompt without pressing G.",
            ]
        }
    }

    /// Browser landing-menu key bindings (shown when no page is loaded).
    fn browser_menu_help_lines() -> &'static [&'static str] {
        &[
            "  G  Go to a URL or search query",
            "  K  Open saved bookmarks",
            "  Q  Exit browser to main menu",
            "",
            "  Commands take effect on ENTER.",
            "  Type more than one character and",
            "  it is taken as a URL or a search,",
            "  so an address can be typed here",
            "  without pressing G first.",
        ]
    }

    /// Main-menu help (single width — fits 40 cols so it serves PETSCII too).
    ///
    /// **It draws the menu that is on the screen, not the menu this build
    /// could draw.**  Two of the items are optional -- `K` when the CP/M
    /// emulator is switched off, `2` where the second page has nothing on it
    /// (and everywhere off Unix) -- and each is hidden from the menu, refused
    /// by its key arm and left out of the error hint.  The help was the fourth
    /// surface and it was not gated, so on a packaged installation, where
    /// `NoNewPrivileges=yes` hides the second page entirely, `H` documented a
    /// key that is not on the menu and does nothing when pressed.  A help
    /// screen explaining an absent key is the same defect as an item an
    /// operator cannot use.
    ///
    /// Built into a `OnceLock` per combination rather than written as a
    /// literal: the entries sit inside the list rather than at the end, so a
    /// `cfg`/`if` pair of whole literals would be twenty duplicated lines that
    /// can drift apart silently -- the exact shape this project has been
    /// bitten by -- and every caller here (the tests and `show_help_page`)
    /// wants a `&'static [&'static str]`, which a `OnceLock` gives and a `Vec`
    /// return would not.  Four cells and an explicit match, because the CP/M
    /// flag comes from config and can change while the process runs: a single
    /// cell would freeze whichever menu was drawn first.
    fn main_help_lines(items: MenuItems) -> &'static [&'static str] {
        type Cell = std::sync::OnceLock<Vec<&'static str>>;
        static BOTH: Cell = Cell::new();
        static CPM_ONLY: Cell = Cell::new();
        static SECOND_ONLY: Cell = Cell::new();
        static NEITHER: Cell = Cell::new();
        let cell = match (items.cpm, items.second_page) {
            (true, true) => &BOTH,
            (true, false) => &CPM_ONLY,
            (false, true) => &SECOND_ONLY,
            (false, false) => &NEITHER,
        };
        cell.get_or_init(|| {
            let mut v: Vec<&'static str> = vec![
                "  A  AI Chat: ask questions to an AI",
                "  B  Browser: browse the web",
                "  C  Configuration: server settings",
                "     and other options",
                "  F  File Transfer: upload/download",
                "     with XMODEM, YMODEM, ZMODEM,",
                "     Kermit or Punter",
                "  G  Serial Gateway: pick Port A or B",
                "     and bridge to its wire (when",
                "     that port is in console mode)",
            ];
            // Gated with the row and the error hint: `cpm_emu_enabled = false`
            // takes `K` off the menu, so it comes off this page too.
            if items.cpm {
                v.extend([
                    "  K  CP/M System: run real CP/M .COM",
                    "     software on an emulated Z80",
                ]);
            }
            v.extend([
                "  R  Troubleshooting: diagnose",
                "     terminal input issues",
                "  S  SSH Gateway: connect to a",
                "     remote server via SSH",
                "  T  Telnet Gateway: connect to a",
                "     remote server via telnet",
                "  W  Weather: by city or postal code",
                "  X  Exit: disconnect from server",
            ]);
            // Listed last because the menu draws it last -- a help screen in a
            // different order from the screen it explains is a help screen the
            // reader has to search.  `cfg` *and* the flag, exactly as
            // `main_menu_rows` pushes the row, so no Windows build can grow it
            // however the flag is set.
            #[cfg(unix)]
            if items.second_page {
                v.extend([
                    "  2  Second Menu: restart or shut",
                    "     down the whole computer",
                ]);
            }
            v
        })
    }

    /// MORE page help.  Unix only, for the same reason the page is -- see
    /// `telnet/power.rs`.
    #[cfg(unix)]
    fn more_help_lines() -> &'static [&'static str] {
        &[
            "  The second menu, reached with 2",
            "  from the main menu.",
            "",
            "  R  Restart the computer the",
            "     gateway runs on -- not just",
            "     the gateway itself.",
            "",
            "  S  Shut down that computer. It",
            "     will need switching on by hand.",
            "",
            "  Q  Back to the main menu.",
            "",
            // Three lines, not five: adding the `Q` entry took this table to
            // 17 and `HELP_MAX_CONTENT_LINES` is 15, so the screen split into
            // two pages and the reader had to press a key to finish a short
            // explanation.  Measured on the live gateway, which said
            // "Page 1/2".
            "  Both ask to confirm, then for a",
            "  password. To restart the GATEWAY",
            "  only: Configuration > Server > R.",
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
                "  C  CP/M: emulator, disk images,",
                "     booting, and the printer",
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
                "  C  CP/M: the emulator, disk images,",
                "     booting a disk, and the printer",
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
            "  S  Gateway Shell (drive A:)",
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
    conn_rates: ConnRateMap,
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

    crate::bindwatch::expect("telnet", port);
    tokio::spawn(async move {
        let listener = match TcpListener::bind(format!("0.0.0.0:{}", port)).await {
            Ok(l) => l,
            Err(e) => {
                glog!("Telnet server: failed to bind port {}: {}", port, e);
                crate::bindwatch::failed("telnet", &e);
                return;
            }
        };
        crate::bindwatch::bound("telnet");
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
                            // Per-IP connection rate limit, ahead of the
                            // session claim and the IP allowlist so a refused
                            // connection costs one map lookup and nothing
                            // else -- the same "cheapest gate first" ordering
                            // the web server uses for its lockout check.
                            //
                            // Read fresh each accept, like the security flags
                            // below, so a change applies without a restart.
                            let (rate_max, rate_window) = config::get_conn_rate();
                            let (rate_count, rate_say_so) = if rate_max > 0 {
                                note_connection(&conn_rates, addr.ip(), rate_max, rate_window)
                            } else {
                                (0, false)
                            };
                            if rate_max > 0 && rate_count > rate_max {
                                // Once per flood, not once per connection --
                                // see `ConnRate`.  The write is blocking and
                                // the log rolls, so an unconditional line
                                // here is the amplifier the refusal itself
                                // was written to avoid being.
                                if rate_say_so {
                                    glog!(
                                        "Telnet: rejected {} (more than {} connections in {}s); \
                                         further refusals from this address are not logged \
                                         until it is under the limit again",
                                        addr, rate_max, rate_window.as_secs()
                                    );
                                }
                                // Dropped without a message: a scanner opening
                                // a connection per guess is not reading our
                                // replies, and spawning a bounded write per
                                // refusal is the accept-loop serialization
                                // this file already warns about below.
                                drop(stream);
                                continue;
                            }
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
                            // `get_security_flags` reads only the three
                            // booleans without cloning the full Config,
                            // keeping accept-flood cost down to a
                            // single Mutex acquisition with no String
                            // allocations.
                            let (live_security, live_disable_safety, live_block_gw) =
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
                                && let Some(reason) = reject_insecure_ip(addr.ip(), live_block_gw)
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
                                    color_enabled: true,
                                    erase_char: session::DEFAULT_ERASE_CHAR,
                                    lockouts: lo,
                                    peer_addr: Some(addr.ip()),
                                    power_password_failures: Arc::new(Mutex::new((0, std::time::Instant::now()))),
                                    power_lockouts: shared_power_lockouts().clone(),
                                    // The telnet door computes this in `run`, once it has run.
                                    authenticated: false,
                                    power_arrived_by_relay: false,
                                    #[cfg(unix)]
                                    power_elevation: Default::default(),
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
                                    last_transfer_note: None,
            arm_next_prompt: false,
                                    web_lines: Vec::new(),
                                    web_scroll: 0,
                                    web_links: Vec::new(),
                                    web_history: Vec::new(),
                                    web_url: None,
                                    web_home_tried: false,
                                    web_title: None,
                                    web_forms: Vec::new(),
                                    weather_location: config::get_config().weather_location,
                                    is_serial: false,
                                    is_relay: false,
                                    serial_port_id: None,
                                    is_ssh: false,
                                    idle_timeout: std::time::Duration::from_secs(cfg.idle_timeout_secs),
                                    pushback: None,
                                    mid_iac_cmd: false,
            last_was_cr: false,
                                    neg_sent_will: Box::new([false; 256]),
                                    neg_sent_do: Box::new([false; 256]),
                                    neg_sent_wont: Box::new([false; 256]),
                                    neg_sent_dont: Box::new([false; 256]),
                                    ttype_matched: false,
                                    ttype_raw: None,
                                    telnet_negotiated: false,
                                    window_width: None,
                                    window_height: None,
                                    trace_bytes: cpm_emu::keytrace_on(),
                                };
                                if let Err(e) = session.run().await {
                                    if !is_normal_disconnect(&e) {
                                        glog!("Telnet: session error from {}: {}", addr, e);
                                    }
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

    crate::bindwatch::expect("Kermit", port);
    tokio::spawn(async move {
        let listener = match TcpListener::bind(format!("0.0.0.0:{}", port)).await {
            Ok(l) => l,
            Err(e) => {
                glog!("Kermit server: failed to bind port {}: {}", port, e);
                crate::bindwatch::failed("Kermit", &e);
                return;
            }
        };
        crate::bindwatch::bound("Kermit");
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
