//! Configuration file management.
//!
//! Reads/writes a simple key=value config file (`egateway.conf`). If the file
//! does not exist at startup it is created with sensible defaults. Unknown
//! keys are silently ignored; missing keys are filled with defaults and the
//! file is rewritten.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use crate::logger::glog;

/// Name of the configuration file (lives next to the binary).
pub const CONFIG_FILE: &str = "egateway.conf";

/// Path to the configuration file actually read/written at runtime.
///
/// Production always uses [`CONFIG_FILE`] in the working directory.  Under
/// `cfg(test)` it redirects to a **process-unique** file in the temp
/// directory, so the parallel test binary never reads or writes the
/// developer's real `egateway.conf`.  That shared working-dir file (plus the
/// global `CONFIG` singleton) was the one piece of cross-test mutable state
/// that let a config-mutating test (e.g. the relay onward-dial tests) leak
/// into another test's `get_config()` read, and let one `cargo test`
/// invocation contaminate the next through the file left behind.  The
/// redirect is compile-time gated, so the release binary is byte-for-byte
/// unchanged; only test builds see the temp path.
#[cfg(not(test))]
fn config_file_path() -> String {
    CONFIG_FILE.to_string()
}

#[cfg(test)]
fn config_file_path() -> String {
    use std::sync::OnceLock;
    static TEST_CONFIG_PATH: OnceLock<String> = OnceLock::new();
    TEST_CONFIG_PATH
        .get_or_init(|| {
            std::env::temp_dir()
                .join(format!("egateway_test_{}.conf", std::process::id()))
                .to_string_lossy()
                .into_owned()
        })
        .clone()
}

/// Test-only lock serialising every test that depends on the process-wide
/// `Config`, whichever module the test lives in.
///
/// It lives here rather than in one module's test submodule because the state
/// it guards is global: `kermit`'s tests repoint `transfer_dir`, and any test
/// elsewhere that *reads* it — directly or through a function that calls
/// `get_config()` internally — races them and fails intermittently. That is
/// exactly the mechanism behind the `test_server_g_dir_returns_listing` flake,
/// so a second copy of the lock per module would just reintroduce it across
/// module boundaries.
///
/// `tokio::sync::Mutex` so the guard can cross await points on a
/// multi-threaded runtime, which `std::sync::Mutex` cannot.
#[cfg(test)]
pub(crate) static CONFIG_TEST_LOCK: tokio::sync::Mutex<()> =
    tokio::sync::Mutex::const_new(());

// ─── Defaults ──────────────────────────────────────────────
const DEFAULT_TELNET_ENABLED: bool = true;
const DEFAULT_TELNET_PORT: u16 = 2323;
/// Default for the outgoing Telnet Gateway's cooperative negotiation.
/// Off by default so dialing raw-TCP-on-port-23 services (legacy MUDs,
/// hand-rolled BBS software) still works — those services don't speak
/// telnet and would see our IAC offers as garbage.  Enable when the
/// destinations you dial are genuine telnet servers.
const DEFAULT_TELNET_GATEWAY_NEGOTIATE: bool = false;
/// Default for the outgoing Telnet Gateway's protocol-layer override.
/// Off by default (smart mode) so the gateway parses telnet IAC in both
/// directions.  When true, the gateway treats the remote as a raw TCP
/// byte stream — no IAC escape on outbound, no IAC parse on inbound —
/// which is the last-resort escape hatch for destinations that clearly
/// aren't telnet at all.
const DEFAULT_TELNET_GATEWAY_RAW: bool = false;
/// Byte-level trace of the SSH/Telnet gateway proxy loops, for diagnosing
/// input corruption and terminal-translation issues.  Off by default — it
/// is verbose, per-byte output meant for troubleshooting.  Read fresh by
/// each gateway session, so toggling it takes effect on the next session
/// without restarting the program.
const DEFAULT_GATEWAY_DEBUG: bool = false;
/// Operator override for the terminal geometry a gateway session reports to
/// the remote host (SSH PTY request / Telnet NAWS).  `0` means "auto": use
/// the size the local client negotiated via NAWS, and fall back to the
/// per-terminal-type default when it negotiated none.
///
/// This exists because terminal *type* does not determine terminal *width*.
/// A C64 running CCGMS in ASCII mode reports its backspace as `0x08`, so we
/// detect ANSI and would otherwise claim 80 columns for a physically
/// 40-column screen; CCGMS's soft 80-column mode is the mirror case in
/// PETSCII.  Retro clients reach us through WiFi modems and tcpser, which
/// never send NAWS on the C64's behalf, so nothing but the operator can
/// tell us the truth.  Getting it wrong misplaces every readline redraw and
/// backspace past the real margin.  `0` is load-bearing — it is the only way
/// to ask for auto, so neither value may be floored to 1.
const DEFAULT_GATEWAY_TERM_WIDTH: u16 = 0;
/// Rows counterpart of `DEFAULT_GATEWAY_TERM_WIDTH`.  `0` = auto.
const DEFAULT_GATEWAY_TERM_HEIGHT: u16 = 0;
const DEFAULT_ENABLE_CONSOLE: bool = true;
/// Whether the desktop GUI's first-run setup wizard has already been shown.
///
/// **False in `Config::default()`** — the default config is exactly what a
/// genuinely fresh install gets (no config file on disk), and that install is
/// the one that should see the wizard.
///
/// An *existing* config file that lacks the key resolves to **true** instead
/// (see the reader), because a file on disk means the gateway has been
/// configured before: an operator upgrading into this version must not be
/// dropped into a wizard that could rewrite settings they already chose.  The
/// two directions are deliberately different, and
/// `test_missing_wizard_key_in_existing_file_reads_completed` pins it.
const DEFAULT_SETUP_WIZARD_COMPLETED: bool = false;
const DEFAULT_SECURITY_ENABLED: bool = false;
/// When `security_enabled` is false, the telnet listener restricts
/// inbound connections to RFC 1918 / loopback / link-local / ULA
/// addresses and rejects gateway-style `*.*.*.1` addresses.  This
/// flag, when true, disables that allowlist entirely and accepts
/// every connection regardless of source.  Off by default because
/// the allowlist is the only thing standing between a public IP
/// and an unauthenticated telnet session.
const DEFAULT_DISABLE_IP_SAFETY: bool = false;
const DEFAULT_USERNAME: &str = "admin";
/// The default password, exposed crate-wide so callers (e.g. main.rs's
/// insecure-default warning) test against this constant rather than a
/// duplicated string literal that could silently drift from the real default.
pub(crate) const DEFAULT_PASSWORD: &str = "changeme";
const DEFAULT_TRANSFER_DIR: &str = "transfer";
/// Place `EGT8080.COM` and `EGT80.COM` when they are missing.
///
/// **On, because the file being there is the point.** They are our own CP/M
/// terminal in period assembly, compiled into the binary, and they go two
/// places: CP/M drive A:, where the emulator runs one, and loose in the
/// transfer directory, where the file-transfer menus can send one to real
/// hardware without starting the emulator at all.  On a fresh install nothing
/// else puts them there.
///
/// **This is a placement switch, not a delete switch.** Turning it off stops
/// the gateway *writing* a missing file; it never removes one already there,
/// and it cannot: the copy on A: holds the operator's own settings inside its
/// `.COM`.  It exists for the operator who keeps their own build, or their own
/// `EGT80.COM` from before 0.9.2, and would rather a deleted file stayed
/// deleted than quietly reappear on the next restart.
///
/// Leaving it on costs four `exists()` checks per start-up and nothing else --
/// a file already in place is never rewritten, whatever this says.
const DEFAULT_PLACE_BUNDLED_TERMINALS: bool = true;
/// GUI display scale.  `"auto"` (or empty) lets egui use the monitor's own
/// scale factor; a number (e.g. `1.0`, `1.25`) pins the pixels-per-point
/// absolutely so the console renders the same size on any display.
const DEFAULT_GUI_ZOOM: &str = "auto";
/// Clamp range for a numeric `gui_zoom`, matching egui's practical limits.
const GUI_ZOOM_MIN: f32 = 0.5;
const GUI_ZOOM_MAX: f32 = 3.0;
const DEFAULT_MAX_SESSIONS: usize = 50;
/// Write the log to a file as well as stderr.  On by default: a gateway is
/// usually left running unattended, and the in-memory rings only hold the last
/// 2000 lines.
const DEFAULT_LOG_TO_FILE: bool = true;
/// Active log file, in the binary's working directory alongside `egateway.conf`.
const DEFAULT_LOG_FILE: &str = "ethernetgateway.log";
/// Rotate once the active log reaches this size, in KB.
const DEFAULT_LOG_MAX_SIZE_KB: u64 = 1024;
/// Rotated generations to keep; older ones are deleted.  With the defaults the
/// log can never occupy more than 1024 KB x (5 + 1) = 6 MB total.
const DEFAULT_LOG_MAX_FILES: u32 = 5;
const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 900; // 15 minutes
const DEFAULT_GROQ_API_KEY: &str = "";
const DEFAULT_BROWSER_HOMEPAGE: &str = "http://telnetbible.com";
const DEFAULT_WEATHER_LOCATION: &str = "";
// Weather units: "auto" infers from the geocoded country (US -> Fahrenheit/mph,
// everywhere else -> Celsius/km/h); "us" and "metric" force it.
const DEFAULT_WEATHER_UNITS: &str = "auto";
const DEFAULT_VERBOSE: bool = false;
const DEFAULT_SERIAL_ENABLED: bool = false;
/// Default serial console mode.  `"modem"` runs the Hayes AT emulator on the
/// configured port; `"console"` keeps the port idle until a telnet/SSH user
/// chooses Serial Gateway, which bridges their session to the port; `"kermit"`
/// runs a persistent Kermit server directly on the wire (as if the modem had
/// been dialed with `ATDT KERMIT`, but always-on and with no AT layer).
const DEFAULT_SERIAL_MODE: &str = "modem";
const DEFAULT_SERIAL_PORT: &str = "";
const DEFAULT_SERIAL_BAUD: u32 = 9600;
const DEFAULT_SERIAL_DATABITS: u8 = 8;
const DEFAULT_SERIAL_PARITY: &str = "none";
const DEFAULT_SERIAL_STOPBITS: u8 = 1;
const DEFAULT_SERIAL_FLOWCONTROL: &str = "none";
const DEFAULT_XMODEM_NEGOTIATION_TIMEOUT: u64 = 45;
const DEFAULT_XMODEM_BLOCK_TIMEOUT: u64 = 20;
const DEFAULT_XMODEM_MAX_RETRIES: usize = 10;
/// How long the XMODEM/YMODEM receiver waits between successive
/// `C` / NAK pokes during the initial handshake.  Christensen's original
/// XMODEM.DOC and Forsberg's reference implementations use ~10 s; 7 s is
/// a compromise that starts quickly when the sender misses a poke but
/// avoids stockpiling extras in a slow-starting sender's input buffer.
const DEFAULT_XMODEM_NEGOTIATION_RETRY_INTERVAL: u64 = 7;
/// ZMODEM negotiation timeout: how long the sender/receiver keeps
/// retrying ZRQINIT / ZRINIT before giving up.  Analogous to the
/// XMODEM negotiation timeout but for ZMODEM's handshake frames.
const DEFAULT_ZMODEM_NEGOTIATION_TIMEOUT: u64 = 45;
/// ZMODEM per-frame read timeout in seconds.  Applied once a transfer
/// has started — bounds how long we wait for the next header after
/// sending a response frame.
const DEFAULT_ZMODEM_FRAME_TIMEOUT: u64 = 30;
/// ZMODEM max retries for ZRQINIT, ZRPOS, and ZDATA frames.
const DEFAULT_ZMODEM_MAX_RETRIES: u32 = 10;
/// Seconds between successive ZRINIT / ZRQINIT re-sends during the
/// ZMODEM negotiation handshake.  Analogous to the XMODEM family's
/// C-retry interval: long enough that a slow-starting peer doesn't
/// stockpile extras, short enough that a dropped poke doesn't stall
/// the session for long.  The per-session budget is still bounded by
/// `zmodem_negotiation_timeout`.
const DEFAULT_ZMODEM_NEGOTIATION_RETRY_INTERVAL: u64 = 5;
/// Kermit negotiation timeout: how long the sender/receiver keeps
/// retrying the Send-Init handshake before giving up.
const DEFAULT_KERMIT_NEGOTIATION_TIMEOUT: u64 = 300;
/// Kermit per-packet read timeout in seconds — bounds how long we
/// wait for the next response after sending a packet.
const DEFAULT_KERMIT_PACKET_TIMEOUT: u64 = 10;
/// Kermit server-mode idle timeout: how long `kermit_server` waits
/// between commands from the peer before declaring the session
/// stalled, sending an "idle timeout" E-packet, and disconnecting.
/// Defaults to the negotiation-timeout default (300 s) since the two
/// were historically the same value before they were split.  A value
/// of `0` disables the idle deadline — the server will wait
/// indefinitely for the peer's next command.
const DEFAULT_KERMIT_IDLE_TIMEOUT: u64 = DEFAULT_KERMIT_NEGOTIATION_TIMEOUT;
/// Kermit max retries per packet (NAK / timeout retransmits).
const DEFAULT_KERMIT_MAX_RETRIES: u32 = 5;
/// Kermit advertised max packet length (10..=9024).  4096 strikes a
/// balance between throughput and re-transmit cost on a flaky line.
const DEFAULT_KERMIT_MAX_PACKET_LENGTH: u16 = 4096;
/// Kermit sliding-window size (1..=31).  1 is stop-and-wait; 4 is a
/// conservative streaming-friendly default.
const DEFAULT_KERMIT_WINDOW_SIZE: u8 = 4;
/// Kermit block-check type advertised: 1 = 6-bit checksum, 2 = 12-bit
/// checksum, 3 = CRC-16/KERMIT.  Default 3 (strongest).
const DEFAULT_KERMIT_BLOCK_CHECK_TYPE: u8 = 3;
const DEFAULT_KERMIT_LONG_PACKETS: bool = true;
const DEFAULT_KERMIT_SLIDING_WINDOWS: bool = true;
/// Streaming Kermit: peer skips ACKing each packet on reliable links
/// (TCP/SSH).  Default true; turn off only if your remote side bridges
/// into an unreliable serial line.
const DEFAULT_KERMIT_STREAMING: bool = true;
const DEFAULT_KERMIT_ATTRIBUTE_PACKETS: bool = true;
const DEFAULT_KERMIT_REPEAT_COMPRESSION: bool = true;
/// Kermit 8th-bit quoting policy: "auto" (only when peer asks),
/// "on" (always), "off" (never).
const DEFAULT_KERMIT_8BIT_QUOTE: &str = "auto";
/// Resume partial uploads (Frank da Cruz spec §5.1, disposition='R').
/// When true, the receiver tells the sender to skip bytes already on
/// disk under `transfer_dir/<filename>`.  Default false: opt-in,
/// because a peer that ignores disposition='R' would re-send from
/// byte 0 against our pre-loaded partial and corrupt the result.
const DEFAULT_KERMIT_RESUME_PARTIAL: bool = false;
/// Maximum age (hours) of a partial file we'll resume.  Older
/// partials are treated as stale rot rather than legitimate
/// resumable transfers.  168 = one week.
const DEFAULT_KERMIT_RESUME_MAX_AGE_HOURS: u32 = 168;
/// Locking-shift mode (Frank da Cruz spec §3.4.5): when both peers
/// advertise CAPAS_LOCKING_SHIFT, the encoder uses SO/SI region
/// markers instead of QBIN's per-byte 8th-bit prefix.  Off by
/// default — no modern Kermit peer (C-Kermit, G-Kermit, Kermit-95,
/// E-Kermit) negotiates it; flip it on if you're talking to a
/// strict-spec implementation that does.
const DEFAULT_KERMIT_LOCKING_SHIFTS: bool = false;
/// Wait for the receiver's initiating NAK before sending our Send-Init on
/// a Kermit *download* (gateway = sender).  A real Kermit receiver pokes
/// the sender with periodic NAKs once it enters receive mode; waiting for
/// that first NAK keeps our Send-Init from landing on the terminal as
/// on-screen garbage before the user's client is ready.  On by default;
/// only the interactive telnet download consults it (server / test paths
/// never wait).  Falls through and sends anyway if no NAK arrives within
/// the negotiation timeout, so peers that don't NAK still work.
const DEFAULT_KERMIT_WAIT_FOR_RECEIVER: bool = true;
/// Allow `ATDT KERMIT` (or `ATDT kermit-server`) from the serial modem
/// emulator to drop directly into Kermit server mode without going
/// through the telnet menu's auth gate.  Off by default because it
/// bypasses any `security_enabled` username/password the operator has
/// configured.  Enable only when the serial line itself is trusted
/// (private cable, isolated lab); for any auth-required deployment
/// keep this off and have callers go through the regular telnet
/// menu (F → K) instead.
const DEFAULT_ALLOW_ATDT_KERMIT: bool = false;
/// Peer-dial: let a modem-mode serial port dial another port directly
/// (`ATD <Port>@<IP>`, or select a modem port in the Serial Gateway
/// menu) and bridge to the device on it, instead of always landing on
/// the gateway menu.  A dialed modem port rings and answers per its own
/// AT rules (S0 auto-answer / manual `ATA`); a console port connects
/// directly.  Off by default: it lets any modem caller ring/connect to
/// any addressable port, so it is opt-in even under the trusted-LAN
/// threat model.  See `GatewayPeerDialPlan.md`.
///
/// It gates *dialing*, not *being registered*.  A slave announces its ports
/// (and its CP/M endpoint) to its own master regardless of this flag — that
/// pairing is already explicit, mutual and authenticated, and a slave its
/// master could never reach would defeat the purpose of slave mode.  What stays
/// gated is this gateway dialing an arbitrary peer, and a master accepting a
/// *third party's* crossbar dial into a slave (`relay::run_master_relay_peer`).
const DEFAULT_ALLOW_PEER_DIAL: bool = false;
/// Let a **slave's** Kermit-server-mode port be served by *this* master's
/// Kermit server over the relay, so the device on the slave's wire lists,
/// uploads to and downloads from the master's transfer directory.
///
/// Off by default.  Kermit's server mode has no authentication of its own —
/// the same reason `allow_atdt_kermit` and the standalone listener are opt-in —
/// so this hands a remote wire unauthenticated read and write access to the
/// transfer directory.  It is the best-placed of those three paths (the peer is
/// a slave that authenticated to this master over SSH, and
/// `master_accept_relays` is already required), but it is still the operator's
/// decision.  Master-side only; a slave with it set is unaffected.
const DEFAULT_ALLOW_RELAY_KERMIT: bool = false;
/// Standalone Kermit-server TCP listener.  When `true`, the gateway
/// binds `kermit_server_port` and drops every connection straight into
/// Kermit server mode — no telnet menu, no auth gate, no private-IP
/// allowlist.  Off by default because, like `allow_atdt_kermit`, it
/// bypasses every security check the gateway has; the operator opts in
/// after seeing the GUI / telnet-menu confirmation popup that explains
/// the risk.  Default port 2424 is unassigned by IANA and high enough
/// to avoid the `<1024 needs root` trap, while still being mnemonic
/// (24 ≈ "kermit").
const DEFAULT_KERMIT_SERVER_ENABLED: bool = false;
const DEFAULT_KERMIT_SERVER_PORT: u16 = 2424;
/// Punter (C1) total block size in bytes (8..=255).  255 is the native C1
/// maximum and the right choice for virtually every connection; lowering it
/// (40 is the recommended floor) cuts the resend cost on a noisy line at the
/// expense of per-block handshake overhead.  The 7-byte header is included in
/// this figure, so 255 yields a 248-byte payload.
const DEFAULT_PUNTER_BLOCK_SIZE: u16 = 255;
/// Punter negotiation timeout: how long the receiver/sender waits for the
/// peer's first handshake code before giving up — long enough to start the
/// transfer on the C64 terminal.
const DEFAULT_PUNTER_NEGOTIATION_TIMEOUT: u64 = 45;
/// Punter per-block read timeout in seconds — bounds how long we wait for a
/// handshake code or block body once a transfer is under way.
const DEFAULT_PUNTER_BLOCK_TIMEOUT: u64 = 20;
/// Punter max retries for a handshake code / block (resend GOO·BAD·ACK·S/B).
const DEFAULT_PUNTER_MAX_RETRIES: u32 = 10;
/// Punter consecutive bad/corrupt-block resend rounds tolerated before the
/// gateway gives up on a transfer.  Kept separate from (and higher than)
/// `max_retries` because a real C64 peer (CCGMS/Novaterm) places no outer cap
/// on resend rounds — it keeps re-requesting a block until the data arrives
/// clean or the user aborts on the keyboard.  C1 has no in-band abort, so if
/// the gateway quits first it leaves the C64 spinning; a generous cap lets a
/// noisy-but-working line recover instead of stranding the peer, while still
/// bounding a hopeless systematic mismatch.  30 ≈ the C64's per-stage codecyc.
const DEFAULT_PUNTER_MAX_BAD_ROUNDS: u32 = 30;
/// Drop the connection (carrier) when a Punter transfer gives up, rather than
/// returning to the menu.  C1 has no in-band abort, so a give-up otherwise
/// leaves the C64 spinning in its own retry loop until its (long) internal
/// timeout; closing the connection makes the modem bridge signal loss-of-
/// carrier so the C64 exits its transfer at once.  Off by default — it tears
/// down the whole session, a heavy hammer best reserved for callers who
/// actually hit the strand.
const DEFAULT_PUNTER_HANGUP_ON_FAILURE: bool = false;
/// Seconds between successive handshake-code re-sends during the Punter
/// negotiation phase.  Analogous to the XMODEM/ZMODEM retry intervals.
const DEFAULT_PUNTER_NEGOTIATION_RETRY_INTERVAL: u64 = 5;
/// Configuration web server.  Off by default.  Port 8080 is the
/// canonical "alternate HTTP" port — high enough to avoid the
/// `<1024 needs root` trap and unlikely to collide with system
/// services.  Uses the same `security_enabled` credentials for HTTP Basic
/// auth as the telnet listener, and the same `disable_ip_safety` escape
/// hatch — but, unlike telnet, applies the private-IP allowlist regardless of
/// whether login is required (M-9; the page renders secrets).
const DEFAULT_WEB_ENABLED: bool = false;
const DEFAULT_WEB_PORT: u16 = 8080;
/// CP/M emulator — a real CP/M 2.2 Z80 environment reachable from
/// the main menu.  **On by default.**
///
/// It was default-off while it was being built out, on the same cautious
/// footing as every other feature.  It is on now because the balance changed:
/// the emulator is bounded on three axes (every file call jailed under
/// `transfer_dir/CPM`, a runaway stopped by `cpm_emu_max_minstr`, a double-`ESC`
/// always returning to `A>`), it services BDOS/BIOS only and has no path to a
/// host command, and it now ships with a terminal of its own (EGT8080) that lands
/// on drive A: by itself — so the feature is useful the moment someone opens it
/// rather than something they have to discover and enable.
///
/// What it does still run is arbitrary *guest* code, which is why the gate
/// remains: an operator who does not want that sets `cpm_emu_enabled = false`.
/// The guest's way off the machine is the virtual modem, and that is now on by
/// default too (see [`crate::cpm::uart::DEFAULT_UART`]) — so a fresh install can
/// dial out from guest code.  `cpm_emu_uart = off` closes that door while
/// leaving the emulator itself usable.
const DEFAULT_CPM_EMU_ENABLED: bool = true;
/// Can the web UI's disk-screen page type at a booted guest?
///
/// **On**, and the reasoning is Ricky's (2026-08-09): the web UI already edits
/// every setting and shows the password and the API key, so anyone who can
/// reach it can do far more than type at a CP/M prompt, and an operator who
/// exposes it to a public network has already been told what that means.  The
/// key exists because typing is a *different* thing from watching and deserves
/// its own switch — a gateway that is deliberately read-only for onlookers is a
/// reasonable thing to want, and without a key the only way to get it would be
/// to turn the web server off.
///
/// What it does not open: this is the booted-disk path only, it goes through
/// the same key translation the terminal's own bytes do, and the `ESC ESC`
/// exit gesture is deliberately not honoured from a browser — ending a session
/// somebody else is sitting at is not a keystroke.
const DEFAULT_CPM_SCREEN_INPUT: bool = true;

/// Booting writes to its images unless the operator says otherwise.
///
/// **On, because a disk that cannot be written is not the machine the software
/// expects.**  A vintage operating system saves files, formats disks and
/// updates its own directory; boot one read-only and every `SAVE` appears to
/// work and is gone at the next boot.  That is a worse failure than losing a
/// disk, because it is silent — the guest is told the write succeeded, since
/// the alternative is to fail writes at a guest that has no idea what to do
/// about it.
///
/// This was `false` up to 0.9.2, on the argument that a booted guest has no
/// guard left that understands its requests.  That argument is still true and
/// is why this is a key at all; what changed is the judgement of which failure
/// an operator would rather have.  The disks most people run here come from
/// public collections and can be had again, so a scrambled one costs a
/// download rather than the work on it.
///
/// **Recovery is not automatic, and every surface that offers this says so.**
/// [`crate::cpm::fetch::missing`] never overwrites a file already in the
/// images folder — that file is the operator's, possibly their own disk under
/// a catalogue name — so re-fetching a disk the guest scrambled means deleting
/// it first.
const DEFAULT_CPM_BOOT_WRITABLE: bool = true;
/// Refuse connections whose source address ends in `.1` — typically the router
/// on the local subnet — while the IP allowlist is in force.
///
/// **Off by default**, so such a connection is allowed.  It used to be refused
/// unconditionally, on the reasoning that traffic appearing to come *from* the
/// router may have been forwarded from outside the network.  That is a real
/// case, but it is not the common one: on plenty of networks the router's
/// address is simply where an administrator sits, or where hairpinned traffic
/// from inside the LAN appears to originate, and refusing it left an operator
/// with only the blunt `disable_ip_safety` — which drops the allowlist
/// entirely — as a way in.  Now the narrow behaviour is the opt-in and the rest
/// of the allowlist stays intact either way.
const DEFAULT_DISABLE_GATEWAY_CONNECTIONS: bool = false;
/// Runaway ceiling for the CP/M emulator, in millions of instructions
/// per program run (2000 = 2 billion).  Generous enough for real utilities
/// (an assembler pass, a BASIC run) yet finite so a compute-bound `.COM`
/// that never reads the console still terminates.  Interactive programs are
/// additionally escapable with double-`ESC` at any input prompt.
const DEFAULT_CPM_EMU_MAX_MINSTR: u32 = 2000;

/// The most `cpm_emu_max_minstr` may be: a million million instructions.
///
/// A sanity bound, not a protocol one, and it is **clamped rather than
/// rejected** — which is the opposite of what every other bounded key here
/// does, so it is worth saying why. A Kermit packet longer than 9024 bytes is
/// *nonsense*, so falling back to the default is the right answer for it. An
/// operator who writes `4000000000` here is not talking nonsense: they mean "do
/// not stop my program", and at roughly a hundred million emulated instructions
/// a second even this cap is over three months of continuous running. Rejecting
/// their number would drop them to the 2000 default — a hundred thousand times
/// *shorter* than they asked for, written back over their setting the next time
/// anything saves the config, and with no message to say so. Clamping keeps the
/// intent; the two values behave identically for any program that exists.
///
/// The cap also keeps the number six digits, which is what lets the telnet
/// screen put the emulator and its ceiling on one 40-column row.
pub const MAX_CPM_EMU_MAX_MINSTR: u32 = 1_000_000;
const DEFAULT_SERIAL_ECHO: bool = true;
const DEFAULT_SERIAL_VERBOSE: bool = true;
const DEFAULT_SERIAL_QUIET: bool = false;
/// Default S-register values (S0–S26), comma-separated for config storage.
/// S7 is 15 (not the Hayes 50) — gateway-friendly carrier wait.  S13–S24
/// are reserved-zero placeholders; S25 (DTR detect 50 ms) and S26 (RTS/CTS
/// delay 10 ms) match Hayes.  Older config files with only 13 values are
/// still accepted: missing indices fall back to defaults.
const DEFAULT_SERIAL_S_REGS: &str =
    "5,0,43,13,10,8,2,15,2,6,14,95,50,0,0,0,0,0,0,0,0,0,0,0,0,5,1";
/// ATX4 — emit the full extended result-code set (Hayes default).
const DEFAULT_SERIAL_X_CODE: u8 = 4;
/// AT&D0 — ignore DTR (gateway-friendly; Hayes default is &D2).
const DEFAULT_SERIAL_DTR_MODE: u8 = 0;
/// AT&K0 — no modem-level flow control (gateway-friendly; Hayes default is &K3).
/// Physical port flow control is still controlled by `serial_flowcontrol`.
const DEFAULT_SERIAL_FLOW_MODE: u8 = 0;
/// AT&C1 — DCD reflects carrier state (Hayes default).
const DEFAULT_SERIAL_DCD_MODE: u8 = 1;
/// AT+PETSCII=0 — PETSCII translation off (ASCII passthrough on direct-TCP
/// dials).  Vendor extension; C64 callers flip this on with `AT+PETSCII=1`
/// (which persists immediately) so subsequent `ATDT host:port` sessions
/// render PETSCII correctly.  Also editable from the telnet, web, and GUI
/// config surfaces.
const DEFAULT_SERIAL_PETSCII_TRANSLATE: bool = false;
/// Drive a hardware carrier line off (default).  When true, the modem
/// emulator drives DTR as a carrier proxy (asserted on CONNECT, dropped
/// on NO CARRIER, tied to AT&C) so a vintage terminal wired DTR→DCD via a
/// null-modem cable sees carrier detect.  A PC/USB-serial adapter is a DTE
/// and cannot drive a DCD pin directly, so DTR is the standard proxy.
/// Default off means the gateway makes **zero** modem-line calls, so ports
/// without DCD wiring are byte-for-byte unaffected.  Modem-mode only
/// (console mode has no AT&C carrier concept).
const DEFAULT_SERIAL_DRIVE_CARRIER: bool = false;
const DEFAULT_SSH_ENABLED: bool = false;
const DEFAULT_SSH_PORT: u16 = 2222;
/// Default SSH-gateway authentication mode: "key" uses the gateway's
/// auto-generated Ed25519 client key; "password" prompts the operator
/// for a remote password on each connect.  Password is the default
/// because most remote SSH accounts accept passwords out of the box —
/// key mode requires the operator to first install the gateway's
/// public key on the remote's `~/.ssh/authorized_keys`.
const DEFAULT_SSH_GATEWAY_AUTH: &str = "password";

// ── Master/Slave serial-extender (relay) defaults ──────────
/// Gateway role.  `standalone` (default) is today's behavior — the
/// relay feature is entirely inert.  `master` accepts relay connections
/// from slaves (gated by `master_accept_relays`); `slave` extends its
/// serial ports to a master.  Roles are mutually exclusive (§9 #18).
const DEFAULT_GATEWAY_ROLE: &str = "standalone";
/// Master gate: even with the SSH server up for normal logins, a master
/// only accepts relay channels when this is on.  Off by default so
/// accepting relays is never *implied* by enabling SSH (§4.7).
const DEFAULT_MASTER_ACCEPT_RELAYS: bool = false;
/// Slave → the master's relay port.  Defaults to the SSH port, since the
/// recommended transport rides the master's existing SSH server.
const DEFAULT_SLAVE_MASTER_PORT: u16 = 2222;
/// Relay transport: `ssh` (recommended — reuses the master's SSH server,
/// auth + encryption for free) or `raw` (a dedicated plaintext port, the
/// §4.3 alternative).
const DEFAULT_RELAY_TRANSPORT: &str = "ssh";

/// Identifier for one of the two configurable serial ports.
///
/// Two physically independent ports — Port A (the legacy single port) and
/// Port B (added when the gateway grew dual-port support) — share an
/// identical settings shape but persist under distinct `serial_a_*` /
/// `serial_b_*` config keys, run separate modem-emulator state machines,
/// and own separate console-bridge slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SerialPortId {
    A,
    B,
}

impl SerialPortId {
    /// Single-character display label ("A" or "B") used in menus, log
    /// lines, and prompts.  Centralized so a future rename touches one
    /// place instead of every menu render site.
    pub fn label(self) -> &'static str {
        match self {
            SerialPortId::A => "A",
            SerialPortId::B => "B",
        }
    }

    /// 0/1 array index, for static arrays keyed by port (e.g. the
    /// per-port restart flags and console-bridge slots in `serial.rs`).
    pub fn index(self) -> usize {
        match self {
            SerialPortId::A => 0,
            SerialPortId::B => 1,
        }
    }
}

/// Iteration helper: `[SerialPortId::A, SerialPortId::B]`.  Lets callers
/// loop over both ports without re-listing the enum variants.
pub const SERIAL_PORT_IDS: [SerialPortId; 2] = [SerialPortId::A, SerialPortId::B];

/// The CP/M virtual modem's saved AT profile (`AT&W`).
///
/// Deliberately the fields the physical modem persists, minus the ones with no
/// meaning without a wire: `&D` DTR handling and `&K` flow control are accepted
/// by the AT layer but not modelled, and there are no stored dial slots (`&Z`)
/// yet.  `s_regs` uses the same comma-separated form the serial ports use, so
/// someone reading `egateway.conf` meets one format rather than two.
#[derive(Debug, Clone, PartialEq)]
pub struct CpmModemProfile {
    pub echo: bool,
    pub verbose: bool,
    pub quiet: bool,
    pub x_code: u8,
    pub dcd_mode: u8,
    /// S0..S27, comma-separated; empty means the power-on values.
    pub s_regs: String,
}

impl Default for CpmModemProfile {
    fn default() -> Self {
        CpmModemProfile {
            echo: true,
            verbose: true,
            quiet: false,
            x_code: 4,
            dcd_mode: 1,
            s_regs: String::new(),
        }
    }
}

/// Per-port serial settings.  Each `Config` owns two of these — one for
/// Port A, one for Port B — and the persisted file keys those fields
/// under `serial_a_*` and `serial_b_*` respectively.
#[derive(Debug, Clone, PartialEq)]
pub struct SerialPortConfig {
    /// Master enable for this port.  When false, the port's manager
    /// thread idles and no menu surface activates.
    pub enabled: bool,
    /// `"modem"` (Hayes AT emulator), `"console"` (telnet-serial bridge), or
    /// `"kermit"` (always-on Kermit server on the wire).
    pub mode: String,
    /// Device path (e.g. `/dev/ttyUSB0`, `COM3`).  Empty = unconfigured.
    pub port: String,
    pub baud: u32,
    pub databits: u8,
    pub parity: String,
    pub stopbits: u8,
    pub flowcontrol: String,
    /// Saved modem echo setting (AT&W persists, ATZ restores).
    pub echo: bool,
    /// Saved modem verbose/numeric mode (AT&W persists, ATZ restores).
    pub verbose: bool,
    /// Saved modem quiet mode (AT&W persists, ATZ restores).
    pub quiet: bool,
    /// Saved S-register values as comma-separated decimal.
    pub s_regs: String,
    /// Saved ATX result-code level (0-4).
    pub x_code: u8,
    /// Saved AT&D DTR-handling mode (0-3).
    pub dtr_mode: u8,
    /// Saved AT&K flow-control mode (0-4).
    pub flow_mode: u8,
    /// Saved AT&C DCD mode (0-1).
    pub dcd_mode: u8,
    /// Stored phone-number slots (AT&Zn=s sets, ATDSn dials).  Four slots,
    /// persisted by AT&W and restored by ATZ.  Empty string = unset.
    pub stored_numbers: [String; 4],
    /// Saved AT+PETSCII PETSCII-translation toggle.  When true, the modem
    /// emulator translates the byte stream on direct-TCP dials so a
    /// PETSCII terminal (C64/PET) sees readable text from an ASCII
    /// host.  Vendor-extension AT command; `AT+PETSCII=1` persists it
    /// immediately, and it is also editable from the telnet, web, and
    /// GUI config surfaces.
    pub petscii_translate: bool,
    /// Drive DTR as a hardware carrier proxy (default false).  When true,
    /// the modem emulator asserts/drops DTR with the connection (tied to
    /// AT&C) so a terminal wired DTR→DCD sees carrier detect.  When false
    /// the gateway never touches the modem-control lines, so a port without
    /// DCD wiring behaves exactly as before.  Editable in all three config
    /// UIs.  Modem-mode only.
    pub drive_carrier: bool,
}

impl Default for SerialPortConfig {
    fn default() -> Self {
        Self {
            enabled: DEFAULT_SERIAL_ENABLED,
            mode: DEFAULT_SERIAL_MODE.into(),
            port: DEFAULT_SERIAL_PORT.into(),
            baud: DEFAULT_SERIAL_BAUD,
            databits: DEFAULT_SERIAL_DATABITS,
            parity: DEFAULT_SERIAL_PARITY.into(),
            stopbits: DEFAULT_SERIAL_STOPBITS,
            flowcontrol: DEFAULT_SERIAL_FLOWCONTROL.into(),
            echo: DEFAULT_SERIAL_ECHO,
            verbose: DEFAULT_SERIAL_VERBOSE,
            quiet: DEFAULT_SERIAL_QUIET,
            s_regs: DEFAULT_SERIAL_S_REGS.into(),
            x_code: DEFAULT_SERIAL_X_CODE,
            dtr_mode: DEFAULT_SERIAL_DTR_MODE,
            flow_mode: DEFAULT_SERIAL_FLOW_MODE,
            dcd_mode: DEFAULT_SERIAL_DCD_MODE,
            stored_numbers: [
                String::new(),
                String::new(),
                String::new(),
                String::new(),
            ],
            petscii_translate: DEFAULT_SERIAL_PETSCII_TRANSLATE,
            drive_carrier: DEFAULT_SERIAL_DRIVE_CARRIER,
        }
    }
}

/// Runtime configuration loaded from `egateway.conf`.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// Enable the telnet server. Set to false for SSH-only access.
    pub telnet_enabled: bool,
    pub telnet_port: u16,
    /// When true, the outgoing Telnet Gateway proactively offers
    /// `WILL TTYPE` / `WILL NAWS` at connect time and accepts
    /// `DO TTYPE` / `DO NAWS` requests from the remote.  ECHO cooperation
    /// is independent and always on.  Default false to preserve
    /// compatibility with raw-TCP services on port 23.
    pub telnet_gateway_negotiate: bool,
    /// When true, the Telnet Gateway disables its telnet-IAC layer
    /// entirely and treats the remote as raw TCP.  Intended for
    /// destinations that clearly aren't telnet (some legacy MUDs, custom
    /// BBS software).  Supersedes `telnet_gateway_negotiate` — when raw
    /// is on, there is no negotiation to do.
    pub telnet_gateway_raw: bool,
    /// Byte-level trace, for diagnosing input corruption and
    /// terminal-translation issues.  Read fresh by each session, so toggling
    /// it takes effect on the next one without a restart.  The
    /// `EGATEWAY_GATEWAY_DEBUG` environment variable forces it on regardless
    /// of this flag.
    ///
    /// **This key's scope has grown twice, so it is spelled out here.**  It
    /// began as the SSH/Telnet gateway proxy loops alone, and the doc said
    /// so long after `serial.rs` had joined it -- a key whose description
    /// understates what it does is how the next reader fails to find the
    /// instrument they need.  It now covers four things:
    ///
    /// * the **SSH/Telnet gateway proxy loops** (`telnet/gateway.rs`);
    /// * the **Hayes escape** state machine, `[esc]` -- which escape
    ///   character was accepted, how much silence preceded it, and the byte
    ///   that broke a sequence, that last one being how a failed `+++` is
    ///   distinguished from a slow one;
    /// * the **modem's result codes**, `[modem]` -- `OK`, `NO CARRIER` and
    ///   the rest.  These are traced apart from session output because they
    ///   really are separate: a result code comes from the modem, not from
    ///   the host you dialled;
    /// * every **session byte in and out**, `cpmkey` -- which reader or
    ///   writer saw it, and what the CP/M console's escape state machine
    ///   decided about it.
    ///
    /// Anything added here should be listed above rather than left for
    /// someone to find by reading the source.
    pub gateway_debug: bool,
    /// Columns to report to the remote for SSH/Telnet gateway sessions, or
    /// `0` for auto (client NAWS, else the per-terminal-type default).
    /// Terminal type does not imply terminal width — see
    /// `DEFAULT_GATEWAY_TERM_WIDTH` for why an operator override is the only
    /// thing that can get a C64's real width to the remote.
    pub gateway_term_width: u16,
    /// Rows counterpart of `gateway_term_width`.  `0` = auto.
    pub gateway_term_height: u16,
    /// Show the GUI configuration/console window on startup.
    pub enable_console: bool,
    /// Set once the desktop GUI's first-run setup wizard has been completed
    /// (or skipped).  Purely a GUI concern — no server behaviour reads it, and
    /// it is the only thing standing between a fresh install and the wizard.
    /// See [`DEFAULT_SETUP_WIZARD_COMPLETED`] for why a missing key in an
    /// existing file reads as `true` while the struct default is `false`.
    pub setup_wizard_completed: bool,
    pub security_enabled: bool,
    /// When true, the telnet listener accepts connections from every
    /// source IP, including public addresses and `*.*.*.1` gateway
    /// addresses, even when `security_enabled` is false.  Off by
    /// default; opt-in only via the GUI / telnet-menu confirmation
    /// popup that explains the risk.  Has no effect when
    /// `security_enabled` is true (auth runs regardless).
    pub disable_ip_safety: bool,
    pub username: String,
    pub password: String,
    pub transfer_dir: String,
    /// Write the bundled CP/M terminals when they are missing?
    ///
    /// On by default.  Never overwrites a file that is already there, so this
    /// only decides whether a *missing* one is restored.  See
    /// [`DEFAULT_PLACE_BUNDLED_TERMINALS`].
    pub place_bundled_terminals: bool,
    /// Last GUI window geometry as `x,y,width,height` (outer position + inner
    /// size, in logical points).  Auto-managed by the desktop GUI to reopen the
    /// window where the operator left it; empty = unset (default size + window-
    /// manager placement).  Deliberately NOT exposed in any config UI — it is
    /// written automatically by the GUI, never hand-edited.
    pub gui_window_geometry: String,
    /// Desktop GUI display scale.  `"auto"` = follow the monitor's scale
    /// factor (egui default); a number pins the absolute pixels-per-point so
    /// the console isn't magnified on a high-DPI display.  See
    /// [`Config::gui_zoom_factor`].
    pub gui_zoom: String,
    pub max_sessions: usize,
    pub idle_timeout_secs: u64,
    /// Groq API key. If empty, AI chat is disabled.
    pub groq_api_key: String,
    /// Browser homepage URL. If empty, browser opens to a blank prompt.
    pub browser_homepage: String,
    /// Last-used weather location — a city name or postal code, worldwide
    /// (e.g. "62051", "London, GB", "Zürich").  If empty, the user is prompted
    /// without a default.  Migrated from the legacy `weather_zip` key.
    pub weather_location: String,
    /// Weather display units: "auto" (infer from country), "us"
    /// (Fahrenheit/mph), or "metric" (Celsius/km/h).
    pub weather_units: String,
    /// Mirror the log to [`Config::log_file`] as well as stderr.
    pub log_to_file: bool,
    /// Active log file path.
    pub log_file: String,
    /// Rotate the active log once it reaches this many KB.  Zero disables
    /// size-based rotation (the file then grows unbounded).
    pub log_max_size_kb: u64,
    /// How many rotated generations to keep (`.1` ...).  Older ones are
    /// deleted, so worst-case disk use is `log_max_size_kb * (log_max_files
    /// + 1)`.  Zero keeps none: the active file is truncated on rotation.
    pub log_max_files: u32,
    /// Enable verbose XMODEM protocol logging to stderr.
    pub verbose: bool,
    /// XMODEM negotiation timeout in seconds.  Shared with XMODEM-1K
    /// and YMODEM — they use the same protocol code path.
    pub xmodem_negotiation_timeout: u64,
    /// XMODEM per-block timeout in seconds.  Shared with XMODEM-1K
    /// and YMODEM.
    pub xmodem_block_timeout: u64,
    /// XMODEM maximum retries per block.  Shared with XMODEM-1K and YMODEM.
    pub xmodem_max_retries: usize,
    /// Seconds between successive `C` / NAK pokes during the initial
    /// XMODEM/YMODEM negotiation handshake.  Shared with XMODEM-1K and
    /// YMODEM.  Kept short enough to recover quickly on lost pokes,
    /// long enough that a slow-starting sender doesn't stockpile extras.
    pub xmodem_negotiation_retry_interval: u64,
    /// ZMODEM negotiation timeout in seconds.
    pub zmodem_negotiation_timeout: u64,
    /// ZMODEM per-frame read timeout in seconds.
    pub zmodem_frame_timeout: u64,
    /// ZMODEM max retries for ZRQINIT / ZRPOS / ZDATA frames.
    pub zmodem_max_retries: u32,
    /// Seconds between ZRINIT / ZRQINIT re-sends during the ZMODEM
    /// negotiation handshake.  Analogous to
    /// `xmodem_negotiation_retry_interval` for the XMODEM family.
    pub zmodem_negotiation_retry_interval: u64,
    /// Kermit negotiation timeout (Send-Init handshake) in seconds.
    pub kermit_negotiation_timeout: u64,
    /// Kermit per-packet read timeout in seconds.
    pub kermit_packet_timeout: u64,
    /// Kermit server-mode idle timeout in seconds.  How long the
    /// gateway's Kermit server waits between commands from the peer
    /// before sending "Server idle timeout" and disconnecting.  A
    /// value of `0` disables the deadline — the server idles
    /// indefinitely.
    pub kermit_idle_timeout: u64,
    /// Kermit max retries per packet.
    pub kermit_max_retries: u32,
    /// Advertised max packet length in our Send-Init (10..=9024).
    pub kermit_max_packet_length: u16,
    /// Sliding window size advertised (1..=31).  1 = stop-and-wait.
    pub kermit_window_size: u8,
    /// Block-check type advertised (1=6-bit, 2=12-bit, 3=CRC-16/KERMIT).
    pub kermit_block_check_type: u8,
    /// Advertise long-packets capability.
    pub kermit_long_packets: bool,
    /// Advertise sliding-window capability.
    pub kermit_sliding_windows: bool,
    /// Advertise streaming capability.  Auto-degrades to sliding/stop-
    /// and-wait if the peer doesn't advertise it.
    pub kermit_streaming: bool,
    /// Advertise attribute-packet (A) support.
    pub kermit_attribute_packets: bool,
    /// Use repeat-count compression.
    pub kermit_repeat_compression: bool,
    /// 8th-bit quoting policy: "auto" / "on" / "off".
    pub kermit_8bit_quote: String,
    /// Resume partial uploads via the spec's disposition='R' tag in the
    /// receiver's A-packet ACK.  Off by default; flip on once both sides
    /// are known to honor it (we ship sender support in a follow-up).
    pub kermit_resume_partial: bool,
    /// Max age in hours for a partial file to qualify for resume.
    /// Older partials are ignored (treated as stale rot).
    pub kermit_resume_max_age_hours: u32,
    /// Advertise locking-shift (SO/SI) capability for 8-bit transit
    /// over 7-bit-only links.  Off by default; modern peers use QBIN.
    pub kermit_locking_shifts: bool,
    /// On a Kermit download, wait for the receiver's initiating NAK before
    /// sending our Send-Init (avoids on-screen garbage before the client is
    /// in receive mode).  On by default; only the interactive telnet
    /// download honors it.
    pub kermit_wait_for_receiver: bool,
    /// Allow `ATDT KERMIT` to drop callers directly into Kermit server
    /// mode from the serial modem emulator, bypassing any telnet-menu
    /// auth gate.  Off by default.
    pub allow_atdt_kermit: bool,
    /// Allow peer-dial: a modem-mode serial port may dial another port
    /// directly (`ATD <Port>@<IP>`) or select a modem port in the Serial
    /// Gateway menu, bridging to the device on it.  Off by default.
    pub allow_peer_dial: bool,
    /// Run a standalone Kermit-server TCP listener on
    /// `kermit_server_port`.  Bypasses authentication AND the
    /// private-IP allowlist — every accepted connection drops straight
    /// into Kermit server mode.  Off by default; opt-in via the GUI
    /// Server frame or the telnet menu's Server Configuration screen,
    /// each of which gates the off→on transition behind the same
    /// security warning popup as `allow_atdt_kermit`.
    pub kermit_server_enabled: bool,
    /// Port for the standalone Kermit server listener.  Only consulted
    /// when `kermit_server_enabled` is true.
    pub kermit_server_port: u16,
    /// Punter (C1) total block size in bytes (8..=255, header included).
    pub punter_block_size: u16,
    /// Punter negotiation timeout (seconds): wait for the peer's first
    /// handshake code before giving up.
    pub punter_negotiation_timeout: u64,
    /// Punter per-block read timeout (seconds) once a transfer is under way.
    pub punter_block_timeout: u64,
    /// Punter max retries for a handshake code / block.
    pub punter_max_retries: u32,
    /// Punter consecutive bad-block resend rounds before giving up on the
    /// transfer.  Separate from (and larger than) `punter_max_retries`.
    pub punter_max_bad_rounds: u32,
    /// Seconds between handshake-code re-sends during Punter negotiation.
    pub punter_negotiation_retry_interval: u64,
    /// Drop the connection when a Punter transfer gives up, so the C64 — which
    /// C1 gives no in-band way to abort — sees loss-of-carrier instead of
    /// hanging.  Off by default (it tears down the whole session).
    pub punter_hangup_on_failure: bool,
    /// Run the HTTP configuration web server.  Mirrors the GUI's
    /// settings page in a browser.  Accepts only private/loopback source
    /// IPs unless `disable_ip_safety` is set — applied regardless of whether
    /// login is required (unlike the telnet listener, which drops the
    /// allowlist once `security_enabled` is on; the web page renders the
    /// password + API key, so it doesn't — M-9).  HTTP Basic auth is gated by
    /// the same `security_enabled` flag.
    pub web_enabled: bool,
    /// Port for the configuration web server.  Only consulted when
    /// `web_enabled` is true.
    pub web_port: u16,
    /// Enable the CP/M emulator main-menu item.  Default-off: it
    /// runs arbitrary user-supplied Z80 `.COM` software in an emulated CP/M
    /// 2.2 environment, sandboxed to a `CPM/` directory under `transfer_dir`.
    /// When false the main-menu item is hidden and the `K` key is rejected.
    pub cpm_emu_enabled: bool,
    /// Let the web UI's disk-screen page send keystrokes to a booted guest.
    ///
    /// The screen is always readable; this decides whether it is also a
    /// keyboard.  See [`DEFAULT_CPM_SCREEN_INPUT`].
    pub cpm_screen_input: bool,
    /// One-shot: open the VDM / Dazzler screen once the gateway is back up.
    ///
    /// **Not a setting, and deliberately not on any configuration screen** —
    /// the same posture as [`Config::setup_wizard_completed`]. It is a marker
    /// the desktop UI leaves for itself: turning the web server on from the
    /// screen button restarts the gateway, which destroys the window that was
    /// asked, so the intent has to outlive it. In the file rather than in
    /// memory because the config is re-read on every restart cycle and would
    /// survive the process actually exiting, which a memory flag would not.
    ///
    /// Cleared when it is read, *before* the browser is opened: a marker that
    /// outlived one failed attempt would open a window at every launch, which
    /// is a much worse fault than not opening one.
    pub open_screen_after_restart: bool,
    /// May a booted disk write to the images it is running?
    ///
    /// **On, because a guest whose writes are discarded loses work silently.**
    /// A booted disk has no guard left that understands its requests — mounting
    /// goes through our own filesystem and can refuse a bad one, while a booted
    /// guest owns the whole image and rewrites the file when it leaves — so the
    /// only protections that remain are blunt: this key, and one session per
    /// image.  See [`DEFAULT_CPM_BOOT_WRITABLE`] for why the blunt one defaults
    /// open, and what recovery does and does not do for you.
    pub cpm_boot_writable: bool,
    /// Refuse connections from `*.*.*.1` (the local router, typically) while
    /// the IP allowlist applies.  Off by default; see
    /// [`DEFAULT_DISABLE_GATEWAY_CONNECTIONS`].  Loopback is never affected.
    pub disable_gateway_connections: bool,
    /// Runaway ceiling for a single CP/M-emulator program run, in millions
    /// of instructions (2000 = 2 billion) — instructions of whichever
    /// processor `cpm_cpu` names, a Z80 unless it says otherwise.  A
    /// compute-bound `.COM` that never performs console I/O is aborted once it
    /// reaches this count, so the user always regains the `A>` prompt.
    pub cpm_emu_max_minstr: u32,
    /// Virtual-modem access profile for the CP/M emulator — which machine/port
    /// (`rc2014_1b`, `altair_2sio1`, …) the emulated modem answers at, the BDOS
    /// `AUX:` device (`aux`), a RomWBW HBIOS serial unit (`hbios_1` /
    /// `hbios_2`), or `off`.  Validated against
    /// `crate::cpm::uart::UART_CHOICES`.
    pub cpm_emu_uart: String,
    /// Disk images mounted on CP/M drives, as `A=name.dsk,C=other.dsk`.
    ///
    /// One key rather than sixteen, because the mount UIs render the whole set
    /// as a unit anyway and sixteen keys would be sixteen chances for the three
    /// config surfaces to drift apart.  Values are bare filenames inside
    /// `CPM/images` — never paths; see `cpm::image::is_safe_image_name`.
    pub cpm_mounts: String,
    /// What the CP/M menu item runs: our emulator, or a disk image booted on
    /// emulated Altair hardware.
    ///
    /// Empty — the default — means the CP/M emulator, so a config file written
    /// before this key existed keeps behaving exactly as it did.  Otherwise a
    /// bare filename inside `CPM/images`, which is cold-booted on an emulated
    /// 88-DCDD controller and given the whole machine.
    ///
    /// **Booting is not mounting.**  A mounted image is one drive among sixteen
    /// with our BDOS underneath; a booted one runs its own operating system and
    /// owns the hardware (mounted images ride along at the unit their drive
    /// letter names), so the jail, the CCP-lite prompt, EGT8080 and
    /// `cpm_emu_uart` do not apply inside it.  That is why this is a separate
    /// key from `cpm_mounts` and not another entry in it.
    pub cpm_boot_image: String,
    /// Which machine a booted disk believes it is running on — specifically,
    /// where it finds its console.
    ///
    /// Only meaningful alongside `cpm_boot_image`, because it describes the
    /// hardware around a booted guest and the emulator has no console to place
    /// (it services BDOS calls instead).  Defaults to the Altair 88-2SIO at
    /// `10h`/`11h`, which is what this path has always been, so an upgrade
    /// cannot silence a disk that boots today.
    ///
    /// It exists because a disk that loads perfectly and then sits polling a
    /// keyboard we do not have looks identical to a disk we cannot read, and the
    /// difference is not something to guess at — see
    /// [`crate::cpm::console`].
    pub cpm_boot_machine: String,
    /// What a booted disk is handed when the operator presses Backspace:
    /// `backspace` (BS, 0x08) or `rubout` (the key as the terminal sent it).
    ///
    /// Only meaningful alongside `cpm_boot_image` — the emulator reads its own
    /// console line and already accepts both spellings.  It exists because
    /// there is no answer that is right for every disk, and that was **measured
    /// across two whole disk folders** rather than reasoned:
    ///
    /// * MITS CP/M 2.2, Altair Disk Extended BASIC and Altair Hard Disk BASIC —
    ///   24 of the 29 Altair-folder disks that reach a prompt — erase on BS and
    ///   read a terminal's DEL as a Teletype **rubout**, deleting the character
    ///   and then printing the character they deleted.  On a screen that is the
    ///   `TESTINGGNIT` this key exists to stop.
    /// * CP/M 1.3, 1.4 and the 1975 build are the **opposite**: the rubout is
    ///   their editing key, and BS prints a literal `^H`.  Translating for them
    ///   breaks something that works.
    /// * Digital Research's own CP/M 2.2 accepts either, so neither setting can
    ///   hurt it.
    ///
    /// `backspace` is the default because it is right for the large majority.
    /// It is also the whole answer now: the boot picker that used to ask again
    /// per disk, seeding from this key, went with the second boot path in 0.9.2,
    /// so a CP/M 1.x disk wants `rubout` set before it is booted.
    pub cpm_boot_backspace: String,
    /// What to do with CP/M printer output: `off`, `odt` or `text`.
    ///
    /// Reaches the emulator *and* a booted disk, like `cpm_cpu` — the two get
    /// there by completely different routes (our BDOS/BIOS `LIST` service, and
    /// a printer port a booted guest drives itself) but they produce one
    /// document either way. See [`crate::cpm::printer`].
    pub cpm_printer: String,
    /// Whether a bare CR advances the paper: `auto`, `on` or `off`.
    ///
    /// The auto-line-feed switch a real printer interface carried, and for the
    /// reason it carried one — period software uses a bare CR for both "end of
    /// line" and "return and overprint", on the same board. `auto` keeps the
    /// answer measured for each printer. See [`crate::cpm::printer::auto_lf_for`].
    pub cpm_printer_autolf: String,
    /// Which printer board a BOOTED disk finds: `off`, or a key from
    /// [`crate::cpm::printer::PORT_CHOICES`].
    ///
    /// Only meaningful alongside `cpm_printer`, and only for a booted disk —
    /// the emulator's printer is a BDOS service and has no port at all.
    pub cpm_printer_port: String,
    /// Which processor both CP/M machines run: `z80` or `8080`.
    ///
    /// The one CP/M setting that reaches the emulator *and* a booted disk —
    /// where the console, the backspace key and the boot image describe a
    /// booted disk, and the modem port describes the emulator, this is
    /// underneath both.
    ///
    /// `z80` is the default: it is a superset that runs the 8080 software these
    /// disks are made of.  The 8080 is offered because it is the more literal
    /// Altair and because period 8080 diagnostics — which detect the CPU from
    /// `DCR A` setting parity rather than overflow — are correct to fail on a
    /// Z80.  It no longer costs the terminal: the one `EGT8080.COM` placed on
    /// drive A: runs on either processor.  See
    /// [`crate::cpm::cpu`].
    pub cpm_cpu: String,
    /// The CP/M virtual modem's saved AT profile, written by `AT&W` from
    /// inside the emulator and reloaded when the modem powers up or the guest
    /// issues `ATZ` — the same arrangement the physical ports have under their
    /// `serial_*` keys.  Without it every visit to the emulator started at
    /// factory defaults, so a comms program's init string had to be retyped.
    pub cpm_emu_modem: CpmModemProfile,
    /// Settings for Serial Port A (the legacy single port).  Persisted
    /// under `serial_a_*` keys; legacy `serial_*` keys auto-migrate here
    /// on first read.
    pub serial_a: SerialPortConfig,
    /// Settings for Serial Port B (added when the gateway grew dual-port
    /// support).  Persisted under `serial_b_*` keys.  Defaults to
    /// `enabled = false` so existing single-port deployments keep their
    /// observable behavior unchanged until the operator opts in.
    pub serial_b: SerialPortConfig,
    /// Enable SSH server interface.
    pub ssh_enabled: bool,
    /// SSH server port.
    pub ssh_port: u16,
    /// Authentication mode used when the operator connects to a remote
    /// SSH server through the outbound SSH Gateway.  Accepted values:
    /// "key" (uses the gateway's auto-generated Ed25519 client key) or
    /// "password" (prompts for the remote password each time).
    pub ssh_gateway_auth: String,

    // ── Master/Slave serial-extender (relay) ───────────────
    /// Gateway role: "standalone" (default), "master", or "slave".
    /// Mutually exclusive (§9 #18).  Validated on read/apply.
    pub gateway_role: String,
    /// Master gate — accept inbound relay channels from slaves.  Has no
    /// effect unless `gateway_role == "master"`.
    pub master_accept_relays: bool,
    /// Master gate — serve this master's Kermit server to a slave's
    /// Kermit-server-mode port (see [`DEFAULT_ALLOW_RELAY_KERMIT`]).  Off by
    /// default; Kermit server mode is unauthenticated.
    pub allow_relay_kermit: bool,
    /// Slave → the master's hostname/IP to connect to.  Empty until the
    /// operator configures slave mode.
    pub slave_master_host: String,
    /// Slave → the master's relay port (defaults to the SSH port).
    pub slave_master_port: u16,
    /// Slave → username it authenticates to the master with.  Must match
    /// the master's unified `username` (§9 #6).
    pub slave_master_username: String,
    /// Slave → password it authenticates to the master with.  Must match
    /// the master's unified `password`.  Persisted plaintext like the
    /// other credentials (file written 0600).
    pub slave_master_password: String,
    /// Relay transport: "ssh" (default) or "raw".
    pub relay_transport: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            telnet_enabled: DEFAULT_TELNET_ENABLED,
            telnet_port: DEFAULT_TELNET_PORT,
            telnet_gateway_negotiate: DEFAULT_TELNET_GATEWAY_NEGOTIATE,
            telnet_gateway_raw: DEFAULT_TELNET_GATEWAY_RAW,
            gateway_debug: DEFAULT_GATEWAY_DEBUG,
            gateway_term_width: DEFAULT_GATEWAY_TERM_WIDTH,
            gateway_term_height: DEFAULT_GATEWAY_TERM_HEIGHT,
            enable_console: DEFAULT_ENABLE_CONSOLE,
            setup_wizard_completed: DEFAULT_SETUP_WIZARD_COMPLETED,
            security_enabled: DEFAULT_SECURITY_ENABLED,
            disable_ip_safety: DEFAULT_DISABLE_IP_SAFETY,
            username: DEFAULT_USERNAME.into(),
            password: DEFAULT_PASSWORD.into(),
            transfer_dir: DEFAULT_TRANSFER_DIR.into(),
            place_bundled_terminals: DEFAULT_PLACE_BUNDLED_TERMINALS,
            gui_window_geometry: String::new(),
            gui_zoom: DEFAULT_GUI_ZOOM.into(),
            max_sessions: DEFAULT_MAX_SESSIONS,
            idle_timeout_secs: DEFAULT_IDLE_TIMEOUT_SECS,
            groq_api_key: DEFAULT_GROQ_API_KEY.into(),
            browser_homepage: DEFAULT_BROWSER_HOMEPAGE.into(),
            weather_location: DEFAULT_WEATHER_LOCATION.into(),
            weather_units: DEFAULT_WEATHER_UNITS.into(),
            log_to_file: DEFAULT_LOG_TO_FILE,
            log_file: DEFAULT_LOG_FILE.into(),
            log_max_size_kb: DEFAULT_LOG_MAX_SIZE_KB,
            log_max_files: DEFAULT_LOG_MAX_FILES,
            verbose: DEFAULT_VERBOSE,
            xmodem_negotiation_timeout: DEFAULT_XMODEM_NEGOTIATION_TIMEOUT,
            xmodem_block_timeout: DEFAULT_XMODEM_BLOCK_TIMEOUT,
            xmodem_max_retries: DEFAULT_XMODEM_MAX_RETRIES,
            xmodem_negotiation_retry_interval: DEFAULT_XMODEM_NEGOTIATION_RETRY_INTERVAL,
            zmodem_negotiation_timeout: DEFAULT_ZMODEM_NEGOTIATION_TIMEOUT,
            zmodem_frame_timeout: DEFAULT_ZMODEM_FRAME_TIMEOUT,
            zmodem_max_retries: DEFAULT_ZMODEM_MAX_RETRIES,
            zmodem_negotiation_retry_interval: DEFAULT_ZMODEM_NEGOTIATION_RETRY_INTERVAL,
            kermit_negotiation_timeout: DEFAULT_KERMIT_NEGOTIATION_TIMEOUT,
            kermit_packet_timeout: DEFAULT_KERMIT_PACKET_TIMEOUT,
            kermit_idle_timeout: DEFAULT_KERMIT_IDLE_TIMEOUT,
            kermit_max_retries: DEFAULT_KERMIT_MAX_RETRIES,
            kermit_max_packet_length: DEFAULT_KERMIT_MAX_PACKET_LENGTH,
            kermit_window_size: DEFAULT_KERMIT_WINDOW_SIZE,
            kermit_block_check_type: DEFAULT_KERMIT_BLOCK_CHECK_TYPE,
            kermit_long_packets: DEFAULT_KERMIT_LONG_PACKETS,
            kermit_sliding_windows: DEFAULT_KERMIT_SLIDING_WINDOWS,
            kermit_streaming: DEFAULT_KERMIT_STREAMING,
            kermit_attribute_packets: DEFAULT_KERMIT_ATTRIBUTE_PACKETS,
            kermit_repeat_compression: DEFAULT_KERMIT_REPEAT_COMPRESSION,
            kermit_8bit_quote: DEFAULT_KERMIT_8BIT_QUOTE.into(),
            kermit_resume_partial: DEFAULT_KERMIT_RESUME_PARTIAL,
            kermit_resume_max_age_hours: DEFAULT_KERMIT_RESUME_MAX_AGE_HOURS,
            kermit_locking_shifts: DEFAULT_KERMIT_LOCKING_SHIFTS,
            kermit_wait_for_receiver: DEFAULT_KERMIT_WAIT_FOR_RECEIVER,
            allow_atdt_kermit: DEFAULT_ALLOW_ATDT_KERMIT,
            allow_peer_dial: DEFAULT_ALLOW_PEER_DIAL,
            kermit_server_enabled: DEFAULT_KERMIT_SERVER_ENABLED,
            kermit_server_port: DEFAULT_KERMIT_SERVER_PORT,
            punter_block_size: DEFAULT_PUNTER_BLOCK_SIZE,
            punter_negotiation_timeout: DEFAULT_PUNTER_NEGOTIATION_TIMEOUT,
            punter_block_timeout: DEFAULT_PUNTER_BLOCK_TIMEOUT,
            punter_max_retries: DEFAULT_PUNTER_MAX_RETRIES,
            punter_max_bad_rounds: DEFAULT_PUNTER_MAX_BAD_ROUNDS,
            punter_negotiation_retry_interval: DEFAULT_PUNTER_NEGOTIATION_RETRY_INTERVAL,
            punter_hangup_on_failure: DEFAULT_PUNTER_HANGUP_ON_FAILURE,
            web_enabled: DEFAULT_WEB_ENABLED,
            web_port: DEFAULT_WEB_PORT,
            cpm_emu_enabled: DEFAULT_CPM_EMU_ENABLED,
            cpm_screen_input: DEFAULT_CPM_SCREEN_INPUT,
            cpm_boot_writable: DEFAULT_CPM_BOOT_WRITABLE,
            open_screen_after_restart: false,
            disable_gateway_connections: DEFAULT_DISABLE_GATEWAY_CONNECTIONS,
            cpm_emu_max_minstr: DEFAULT_CPM_EMU_MAX_MINSTR,
            cpm_emu_uart: crate::cpm::uart::DEFAULT_UART.to_string(),
            cpm_mounts: String::new(),
            cpm_boot_image: String::new(),
            cpm_boot_machine: crate::cpm::console::AUTO_MACHINE.to_string(),
            cpm_boot_backspace: crate::cpm::boot::DEFAULT_BACKSPACE.to_string(),
            cpm_printer: crate::cpm::printer::DEFAULT_PRINTER.to_string(),
            cpm_printer_port: crate::cpm::printer::DEFAULT_PRINTER_PORT.to_string(),
            cpm_printer_autolf: crate::cpm::printer::DEFAULT_PRINTER_AUTOLF.to_string(),
            cpm_cpu: crate::cpm::cpu::DEFAULT_CPU.to_string(),
            cpm_emu_modem: CpmModemProfile::default(),
            serial_a: SerialPortConfig::default(),
            serial_b: SerialPortConfig::default(),
            ssh_enabled: DEFAULT_SSH_ENABLED,
            ssh_port: DEFAULT_SSH_PORT,
            ssh_gateway_auth: DEFAULT_SSH_GATEWAY_AUTH.into(),
            gateway_role: DEFAULT_GATEWAY_ROLE.into(),
            master_accept_relays: DEFAULT_MASTER_ACCEPT_RELAYS,
            allow_relay_kermit: DEFAULT_ALLOW_RELAY_KERMIT,
            slave_master_host: String::new(),
            slave_master_port: DEFAULT_SLAVE_MASTER_PORT,
            slave_master_username: String::new(),
            slave_master_password: String::new(),
            relay_transport: DEFAULT_RELAY_TRANSPORT.into(),
        }
    }
}

impl Config {
    /// Borrow the per-port settings for `id`.
    pub fn port(&self, id: SerialPortId) -> &SerialPortConfig {
        match id {
            SerialPortId::A => &self.serial_a,
            SerialPortId::B => &self.serial_b,
        }
    }

    /// Mutably borrow the per-port settings for `id`.
    pub fn port_mut(&mut self, id: SerialPortId) -> &mut SerialPortConfig {
        match id {
            SerialPortId::A => &mut self.serial_a,
            SerialPortId::B => &mut self.serial_b,
        }
    }

    /// Is this a master that wants relays but cannot possibly accept one?
    ///
    /// The SSH relay listens on the SSH server's port, so `master_accept_relays`
    /// is inert while `ssh_enabled` is false — a slave has nothing to connect to.
    /// The `relay_transport` check matters because the (unimplemented) raw
    /// transport would not ride SSH, so warning about SSH would then be wrong.
    ///
    /// One method rather than the condition open-coded per surface: the startup
    /// warning and the telnet / web / GUI screens all ask this, so they cannot
    /// disagree about when to complain.  (Learned from the log-file keys, where
    /// three copies of an "is it on?" rule did drift apart.)
    pub fn relays_blocked_by_ssh_off(&self) -> bool {
        self.gateway_role == "master"
            && self.master_accept_relays
            && self.relay_transport == "ssh"
            && !self.ssh_enabled
    }

    /// One-line explanation of the current gateway terminal-geometry setting,
    /// for the web and GUI panels that both show it under Server → More.
    ///
    /// Written once rather than per surface, for the reason
    /// `relays_blocked_by_ssh_off` above exists: the two copies of the
    /// log-file hint had already drifted (one said "the console above only",
    /// which was wrong inside a popup) before they were collapsed into
    /// `logger::log_state_hint`.  The telnet UI is deliberately not a caller —
    /// it has a full paginated help page, not a one-liner.
    ///
    /// Both dimensions are arguments rather than read from `self` so the GUI's
    /// hint tracks a half-typed field instead of the last saved value — the
    /// same reason `log_state_hint` takes its numbers.
    pub fn gateway_term_hint(width: u16, height: u16) -> String {
        match (width, height) {
            (0, 0) => "Auto: the size your client reports via NAWS, else 40x25 \
                       for PETSCII and 80x24 for ANSI/ASCII. Set these when a \
                       client cannot report its real size — a C64 running CCGMS \
                       in ASCII mode is detected as ANSI and would be told it \
                       has 80 columns for a 40-column screen."
                .to_string(),
            (w, 0) => format!(
                "Reporting {w} columns to the remote; rows stay automatic. \
                 A wrong width misplaces line wrap, backspace and tab \
                 completion past the real margin."
            ),
            (0, h) => format!(
                "Reporting {h} rows to the remote; width stays automatic \
                 (client NAWS, else the terminal-type default)."
            ),
            (w, h) => format!(
                "Reporting {w}x{h} to the remote, overriding whatever the \
                 client negotiated. Set both to 0 for automatic."
            ),
        }
    }

    /// Resolve `gui_zoom` to an absolute pixels-per-point override for the
    /// desktop GUI.  `None` means "auto" (empty or the literal `auto`) — let
    /// egui follow the monitor's own scale factor.  A parsed number is clamped
    /// to [`GUI_ZOOM_MIN`]..=[`GUI_ZOOM_MAX`] so a stray value can't render the
    /// console unusably tiny or huge.
    pub fn gui_zoom_factor(&self) -> Option<f32> {
        let v = self.gui_zoom.trim();
        if v.is_empty() || v.eq_ignore_ascii_case("auto") {
            return None;
        }
        v.parse::<f32>()
            .ok()
            .filter(|z| z.is_finite())
            .map(|z| z.clamp(GUI_ZOOM_MIN, GUI_ZOOM_MAX))
    }
}

/// Global config singleton. Protected by a Mutex so it can be updated at
/// runtime (e.g. when `update_config_value` persists a changed setting).
static CONFIG: Mutex<Option<Config>> = Mutex::new(None);

/// Get a clone of the current configuration.
pub fn get_config() -> Config {
    let guard = CONFIG.lock().unwrap_or_else(|e| e.into_inner());
    guard.clone().unwrap_or_default()
}

/// Read the two boolean flags the telnet accept loop consults on every
/// inbound connection without cloning the full Config (which allocates
/// ~20 owned Strings per call).  Returned as a `(security_enabled,
/// disable_ip_safety)` tuple so the gating expression in the listener
/// stays a single live read under one Mutex acquisition.
pub fn get_security_flags() -> (bool, bool, bool) {
    let guard = CONFIG.lock().unwrap_or_else(|e| e.into_inner());
    match guard.as_ref() {
        Some(cfg) => (
            cfg.security_enabled,
            cfg.disable_ip_safety,
            cfg.disable_gateway_connections,
        ),
        None => (
            DEFAULT_SECURITY_ENABLED,
            DEFAULT_DISABLE_IP_SAFETY,
            DEFAULT_DISABLE_GATEWAY_CONNECTIONS,
        ),
    }
}

/// Read the gateway-debug trace flag without cloning the full Config.
/// Consulted by the serial `+++` escape diagnostics on every read, so it
/// avoids the ~20-String allocation `get_config()` would cost.  Mirrors
/// the "Gateway Debug Trace" toggle in the menu/GUI/web config.
pub fn get_gateway_debug() -> bool {
    let guard = CONFIG.lock().unwrap_or_else(|e| e.into_inner());
    match guard.as_ref() {
        Some(cfg) => cfg.gateway_debug,
        None => DEFAULT_GATEWAY_DEBUG,
    }
}

/// Load (or create) the configuration file and store it in the global singleton.
pub fn load_or_create_config() -> Config {
    let path = config_file_path();
    let cfg = if Path::new(&path).exists() {
        match read_config_file_checked(&path) {
            Ok(cfg) => {
                // Rewrite to ensure all keys are present.
                if let Err(e) = write_config_file(&path, &cfg) {
                    glog!("Warning: {}", e);
                }
                cfg
            }
            Err(e) => {
                // An existing config we cannot read must NOT be silently
                // overwritten with defaults — that would downgrade a secured
                // gateway to security-off / password "changeme" on a corrupt/
                // non-UTF-8/empty file or a transient permission blip.
                //
                // On a RELOAD (SIGHUP re-runs this) we already hold a good
                // in-memory config, so keep running on it — mirroring
                // `update_config_values`' "keeping current settings" — rather
                // than dying (which, under systemd `Restart=on-failure`, could
                // restart-storm). Only on FIRST startup, with nothing to fall
                // back to, do we fail loud and leave the file for the operator.
                let existing = CONFIG.lock().unwrap_or_else(|p| p.into_inner()).clone();
                match resolve_unreadable_existing_config(existing) {
                    Ok(kept) => {
                        glog!(
                            "Warning: {} could not be re-read ({}); keeping current settings.",
                            CONFIG_FILE, e
                        );
                        return kept;
                    }
                    Err(()) => {
                        glog!("FATAL: {} exists but could not be read: {}", CONFIG_FILE, e);
                        glog!("       Refusing to start rather than overwrite it with");
                        glog!("       insecure defaults. Fix or remove the file, then restart.");
                        std::process::exit(1);
                    }
                }
            }
        }
    } else {
        let cfg = Config::default();
        match write_config_file(&path, &cfg) {
            Ok(()) => glog!("Created default configuration: {}", CONFIG_FILE),
            Err(e) => glog!("Warning: {}", e),
        }
        cfg
    };

    let mut guard = CONFIG.lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some(cfg.clone());
    cfg
}

/// Put `cfg` in the global singleton and hand back what was there.
///
/// Test-only, and **in memory only** — it does not touch `egateway.conf`, which
/// is the whole point: a test that needs `get_config()` to answer differently
/// should not rewrite the operator's file to ask the question.
/// `update_config_value` would.
///
/// The caller must hold [`CONFIG_TEST_LOCK`] and put the old value back, for
/// the reason that lock exists: this state is process-wide and any test that
/// reads the config races one that changes it.
#[cfg(test)]
pub(crate) fn swap_config_for_test(cfg: Option<Config>) -> Option<Config> {
    let mut guard = CONFIG.lock().unwrap_or_else(|e| e.into_inner());
    std::mem::replace(&mut *guard, cfg)
}

/// Decide what `load_or_create_config` does when an existing config file can't
/// be read: keep the already-loaded config if we have one (a reload — don't
/// take the service down over a transient/corrupt-file blip), or signal a
/// fail-loud exit when there's nothing to fall back to (first startup — never
/// silently run on insecure defaults). Pure so it can be unit-tested without
/// touching the global singleton or `process::exit`.
fn resolve_unreadable_existing_config(existing: Option<Config>) -> Result<Config, ()> {
    match existing {
        Some(cfg) => Ok(cfg),
        None => Err(()),
    }
}

/// Parse a config file into a `Config`, tolerating an unreadable file by
/// returning defaults (with a warning).  A best-effort read that is only
/// used by tests now — every runtime caller (startup and mid-run updates)
/// uses `read_config_file_checked` so an unreadable *existing* file is never
/// silently replaced with insecure defaults (see `load_or_create_config`).
#[cfg(test)]
fn read_config_file(path: &str) -> Config {
    match read_config_file_checked(path) {
        Ok(cfg) => cfg,
        Err(e) => {
            glog!("Warning: could not read {}: {}", path, e);
            Config::default()
        }
    }
}

/// Like `read_config_file`, but surfaces a read failure — missing file,
/// non-UTF-8 bytes, a permission/I/O error, or an existing file with no
/// recognizable `key = value` lines — as an `Err` instead of falling back to
/// defaults.  Per-key parsing stays lenient: missing or malformed individual
/// keys still resolve to their documented defaults.
fn read_config_file_checked(path: &str) -> std::io::Result<Config> {
    let content = std::fs::read_to_string(path)?;

    let mut map = HashMap::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = trimmed.split_once('=') {
            map.insert(key.trim().to_string(), value.trim().to_string());
        }
    }

    // An existing file that yields NO key=value pairs (empty, whitespace-only,
    // or comments-only) is treated as unreadable rather than parsed as "all
    // defaults" (M-12 residual).  Otherwise an external truncation to zero
    // bytes — or a file emptied before the fsync-before-rename fix landed —
    // would silently downgrade a secured gateway to security-off / password
    // "changeme" and get rewritten with those defaults.  A real config always
    // has at least one recognized key; a genuinely first-run install has no
    // file at all and takes the create-with-defaults path in the caller.
    if map.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "config file has no recognizable settings (empty or corrupt)",
        ));
    }

    let mut cfg = Config {
        telnet_enabled: map
            .get("telnet_enabled")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(DEFAULT_TELNET_ENABLED),
        telnet_port: map
            .get("telnet_port")
            .and_then(|v| v.parse().ok())
            .filter(|&v: &u16| v >= 1)
            .unwrap_or(DEFAULT_TELNET_PORT),
        telnet_gateway_negotiate: map
            .get("telnet_gateway_negotiate")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(DEFAULT_TELNET_GATEWAY_NEGOTIATE),
        telnet_gateway_raw: map
            .get("telnet_gateway_raw")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(DEFAULT_TELNET_GATEWAY_RAW),
        gateway_debug: map
            .get("gateway_debug")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(DEFAULT_GATEWAY_DEBUG),
        // No `>= 1` filter on either: 0 means "auto" and is the only way to
        // ask for it, so flooring these would make auto unreachable.
        gateway_term_width: map
            .get("gateway_term_width")
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_GATEWAY_TERM_WIDTH),
        gateway_term_height: map
            .get("gateway_term_height")
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_GATEWAY_TERM_HEIGHT),
        enable_console: map
            .get("enable_console")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(DEFAULT_ENABLE_CONSOLE),
        // Missing key on an existing file => already-configured install: do NOT
        // run the wizard (see DEFAULT_SETUP_WIZARD_COMPLETED).  Only a config
        // file we create ourselves carries `false`.
        setup_wizard_completed: map
            .get("setup_wizard_completed")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(true),
        security_enabled: map
            .get("security_enabled")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(DEFAULT_SECURITY_ENABLED),
        disable_ip_safety: map
            .get("disable_ip_safety")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(DEFAULT_DISABLE_IP_SAFETY),
        username: map
            .get("username")
            .filter(|v| !v.is_empty())
            .cloned()
            .unwrap_or_else(|| DEFAULT_USERNAME.into()),
        password: map
            .get("password")
            .filter(|v| !v.is_empty())
            .cloned()
            .unwrap_or_else(|| DEFAULT_PASSWORD.into()),
        transfer_dir: map
            .get("transfer_dir")
            .filter(|v| !v.is_empty())
            .cloned()
            .unwrap_or_else(|| DEFAULT_TRANSFER_DIR.into()),
        place_bundled_terminals: map
            .get("place_bundled_terminals")
            .map(|v| v == "true")
            .unwrap_or(DEFAULT_PLACE_BUNDLED_TERMINALS),
        gui_window_geometry: map
            .get("gui_window_geometry")
            .map(|v| v.trim().to_string())
            .unwrap_or_default(),
        gui_zoom: map
            .get("gui_zoom")
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| DEFAULT_GUI_ZOOM.into()),
        max_sessions: map
            .get("max_sessions")
            .and_then(|v| v.parse().ok())
            .filter(|&v: &usize| v >= 1)
            .unwrap_or(DEFAULT_MAX_SESSIONS),
        idle_timeout_secs: map
            .get("idle_timeout_secs")
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_IDLE_TIMEOUT_SECS),
        groq_api_key: map
            .get("groq_api_key")
            .cloned()
            .unwrap_or_else(|| DEFAULT_GROQ_API_KEY.into()),
        browser_homepage: map
            .get("browser_homepage")
            .cloned()
            .unwrap_or_else(|| DEFAULT_BROWSER_HOMEPAGE.into()),
        // Prefer the current key; fall back to the legacy `weather_zip` so an
        // upgrading config keeps its saved location until the next save rewrites
        // it under `weather_location`.
        weather_location: map
            .get("weather_location")
            .or_else(|| map.get("weather_zip"))
            .cloned()
            .unwrap_or_else(|| DEFAULT_WEATHER_LOCATION.into()),
        weather_units: map
            .get("weather_units")
            .map(|v| v.trim().to_ascii_lowercase())
            .filter(|v| matches!(v.as_str(), "auto" | "us" | "metric"))
            .unwrap_or_else(|| DEFAULT_WEATHER_UNITS.into()),
        log_to_file: map
            .get("log_to_file")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(DEFAULT_LOG_TO_FILE),
        log_file: map
            .get("log_file")
            .cloned()
            .unwrap_or_else(|| DEFAULT_LOG_FILE.into()),
        log_max_size_kb: map
            .get("log_max_size_kb")
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_LOG_MAX_SIZE_KB),
        log_max_files: map
            .get("log_max_files")
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_LOG_MAX_FILES),
        verbose: map
            .get("verbose")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(DEFAULT_VERBOSE),
        xmodem_negotiation_timeout: map
            .get("xmodem_negotiation_timeout")
            .and_then(|v| v.parse().ok())
            .filter(|&v: &u64| v >= 1)
            .unwrap_or(DEFAULT_XMODEM_NEGOTIATION_TIMEOUT),
        xmodem_block_timeout: map
            .get("xmodem_block_timeout")
            .and_then(|v| v.parse().ok())
            .filter(|&v: &u64| v >= 1)
            .unwrap_or(DEFAULT_XMODEM_BLOCK_TIMEOUT),
        xmodem_max_retries: map
            .get("xmodem_max_retries")
            .and_then(|v| v.parse().ok())
            .filter(|&v: &usize| v >= 1)
            .unwrap_or(DEFAULT_XMODEM_MAX_RETRIES),
        xmodem_negotiation_retry_interval: map
            .get("xmodem_negotiation_retry_interval")
            .and_then(|v| v.parse().ok())
            .filter(|&v: &u64| v >= 1)
            .unwrap_or(DEFAULT_XMODEM_NEGOTIATION_RETRY_INTERVAL),
        zmodem_negotiation_timeout: map
            .get("zmodem_negotiation_timeout")
            .and_then(|v| v.parse().ok())
            .filter(|&v: &u64| v >= 1)
            .unwrap_or(DEFAULT_ZMODEM_NEGOTIATION_TIMEOUT),
        zmodem_frame_timeout: map
            .get("zmodem_frame_timeout")
            .and_then(|v| v.parse().ok())
            .filter(|&v: &u64| v >= 1)
            .unwrap_or(DEFAULT_ZMODEM_FRAME_TIMEOUT),
        zmodem_max_retries: map
            .get("zmodem_max_retries")
            .and_then(|v| v.parse().ok())
            .filter(|&v: &u32| v >= 1)
            .unwrap_or(DEFAULT_ZMODEM_MAX_RETRIES),
        zmodem_negotiation_retry_interval: map
            .get("zmodem_negotiation_retry_interval")
            .and_then(|v| v.parse().ok())
            .filter(|&v: &u64| v >= 1)
            .unwrap_or(DEFAULT_ZMODEM_NEGOTIATION_RETRY_INTERVAL),
        kermit_negotiation_timeout: map
            .get("kermit_negotiation_timeout")
            .and_then(|v| v.parse().ok())
            .filter(|&v: &u64| v >= 1)
            .unwrap_or(DEFAULT_KERMIT_NEGOTIATION_TIMEOUT),
        kermit_packet_timeout: map
            .get("kermit_packet_timeout")
            .and_then(|v| v.parse().ok())
            .filter(|&v: &u64| v >= 1)
            .unwrap_or(DEFAULT_KERMIT_PACKET_TIMEOUT),
        // No `>= 1` filter on the idle timeout — `0` is the explicit
        // "disable idle deadline" sentinel.  The server-mode dispatch
        // loop in `kermit_server_with_outcome` checks for 0 and skips
        // the read deadline entirely when set, so the peer can hold a
        // Kermit-server session open indefinitely without typing for
        // hours (useful when driving the gateway from a long-running
        // C-Kermit session that idles between commands).
        kermit_idle_timeout: map
            .get("kermit_idle_timeout")
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_KERMIT_IDLE_TIMEOUT),
        kermit_max_retries: map
            .get("kermit_max_retries")
            .and_then(|v| v.parse().ok())
            .filter(|&v: &u32| v >= 1)
            .unwrap_or(DEFAULT_KERMIT_MAX_RETRIES),
        kermit_max_packet_length: map
            .get("kermit_max_packet_length")
            .and_then(|v| v.parse().ok())
            .filter(|&v: &u16| (10..=9024).contains(&v))
            .unwrap_or(DEFAULT_KERMIT_MAX_PACKET_LENGTH),
        kermit_window_size: map
            .get("kermit_window_size")
            .and_then(|v| v.parse().ok())
            .filter(|&v: &u8| (1..=31).contains(&v))
            .unwrap_or(DEFAULT_KERMIT_WINDOW_SIZE),
        kermit_block_check_type: map
            .get("kermit_block_check_type")
            .and_then(|v| v.parse().ok())
            .filter(|&v: &u8| matches!(v, 1..=3))
            .unwrap_or(DEFAULT_KERMIT_BLOCK_CHECK_TYPE),
        kermit_long_packets: map
            .get("kermit_long_packets")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(DEFAULT_KERMIT_LONG_PACKETS),
        kermit_sliding_windows: map
            .get("kermit_sliding_windows")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(DEFAULT_KERMIT_SLIDING_WINDOWS),
        kermit_streaming: map
            .get("kermit_streaming")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(DEFAULT_KERMIT_STREAMING),
        kermit_attribute_packets: map
            .get("kermit_attribute_packets")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(DEFAULT_KERMIT_ATTRIBUTE_PACKETS),
        kermit_repeat_compression: map
            .get("kermit_repeat_compression")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(DEFAULT_KERMIT_REPEAT_COMPRESSION),
        kermit_8bit_quote: map
            .get("kermit_8bit_quote")
            .map(|v| v.trim().to_ascii_lowercase())
            .filter(|v| matches!(v.as_str(), "auto" | "on" | "off"))
            .unwrap_or_else(|| DEFAULT_KERMIT_8BIT_QUOTE.into()),
        kermit_resume_partial: map
            .get("kermit_resume_partial")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(DEFAULT_KERMIT_RESUME_PARTIAL),
        kermit_resume_max_age_hours: map
            .get("kermit_resume_max_age_hours")
            .and_then(|v| v.parse().ok())
            .filter(|&v: &u32| v >= 1)
            .unwrap_or(DEFAULT_KERMIT_RESUME_MAX_AGE_HOURS),
        kermit_locking_shifts: map
            .get("kermit_locking_shifts")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(DEFAULT_KERMIT_LOCKING_SHIFTS),
        kermit_wait_for_receiver: map
            .get("kermit_wait_for_receiver")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(DEFAULT_KERMIT_WAIT_FOR_RECEIVER),
        allow_atdt_kermit: map
            .get("allow_atdt_kermit")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(DEFAULT_ALLOW_ATDT_KERMIT),
        allow_peer_dial: map
            .get("allow_peer_dial")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(DEFAULT_ALLOW_PEER_DIAL),
        kermit_server_enabled: map
            .get("kermit_server_enabled")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(DEFAULT_KERMIT_SERVER_ENABLED),
        kermit_server_port: map
            .get("kermit_server_port")
            .and_then(|v| v.parse().ok())
            .filter(|&v: &u16| v >= 1)
            .unwrap_or(DEFAULT_KERMIT_SERVER_PORT),
        punter_block_size: map
            .get("punter_block_size")
            .and_then(|v| v.parse().ok())
            .filter(|&v: &u16| (8..=255).contains(&v))
            .unwrap_or(DEFAULT_PUNTER_BLOCK_SIZE),
        punter_negotiation_timeout: map
            .get("punter_negotiation_timeout")
            .and_then(|v| v.parse().ok())
            .filter(|&v: &u64| v >= 1)
            .unwrap_or(DEFAULT_PUNTER_NEGOTIATION_TIMEOUT),
        punter_block_timeout: map
            .get("punter_block_timeout")
            .and_then(|v| v.parse().ok())
            .filter(|&v: &u64| v >= 1)
            .unwrap_or(DEFAULT_PUNTER_BLOCK_TIMEOUT),
        punter_max_retries: map
            .get("punter_max_retries")
            .and_then(|v| v.parse().ok())
            .filter(|&v: &u32| v >= 1)
            .unwrap_or(DEFAULT_PUNTER_MAX_RETRIES),
        punter_max_bad_rounds: map
            .get("punter_max_bad_rounds")
            .and_then(|v| v.parse().ok())
            .filter(|&v: &u32| v >= 1)
            .unwrap_or(DEFAULT_PUNTER_MAX_BAD_ROUNDS),
        punter_negotiation_retry_interval: map
            .get("punter_negotiation_retry_interval")
            .and_then(|v| v.parse().ok())
            .filter(|&v: &u64| v >= 1)
            .unwrap_or(DEFAULT_PUNTER_NEGOTIATION_RETRY_INTERVAL),
        punter_hangup_on_failure: map
            .get("punter_hangup_on_failure")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(DEFAULT_PUNTER_HANGUP_ON_FAILURE),
        web_enabled: map
            .get("web_enabled")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(DEFAULT_WEB_ENABLED),
        web_port: map
            .get("web_port")
            .and_then(|v| v.parse().ok())
            .filter(|&v: &u16| v >= 1)
            .unwrap_or(DEFAULT_WEB_PORT),
        cpm_emu_enabled: map
            .get("cpm_emu_enabled")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(DEFAULT_CPM_EMU_ENABLED),
        cpm_screen_input: map
            .get("cpm_screen_input")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(DEFAULT_CPM_SCREEN_INPUT),
        cpm_boot_writable: map
            .get("cpm_boot_writable")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(DEFAULT_CPM_BOOT_WRITABLE),
        open_screen_after_restart: map
            .get("open_screen_after_restart")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(false),
        disable_gateway_connections: map
            .get("disable_gateway_connections")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(DEFAULT_DISABLE_GATEWAY_CONNECTIONS),
        // Clamped at the top, rejected at the bottom.  `0` has always meant
        // "unreadable, use the default" here and still does; a number above the
        // cap is a coherent wish and is granted as far as it goes.  See
        // [`MAX_CPM_EMU_MAX_MINSTR`].
        cpm_emu_max_minstr: map
            .get("cpm_emu_max_minstr")
            .and_then(|v| v.parse().ok())
            .filter(|&v: &u32| v >= 1)
            .map(|v: u32| v.min(MAX_CPM_EMU_MAX_MINSTR))
            .unwrap_or(DEFAULT_CPM_EMU_MAX_MINSTR),
        cpm_emu_uart: map
            .get("cpm_emu_uart")
            .filter(|v| crate::cpm::uart::is_valid_uart_key(v))
            .cloned()
            .unwrap_or_else(|| crate::cpm::uart::DEFAULT_UART.to_string()),
        cpm_mounts: map.get("cpm_mounts").cloned().unwrap_or_default(),
        cpm_boot_image: map.get("cpm_boot_image").cloned().unwrap_or_default(),
        // Missing means `auto`: a config written before this key existed gets
        // detection, which for every disk that booted then resolves to the
        // machine it already used -- proved in `test_detect_every_real_image`.
        cpm_boot_machine: map
            .get("cpm_boot_machine")
            .cloned()
            .unwrap_or_else(|| crate::cpm::console::AUTO_MACHINE.to_string()),
        // Missing means the modern behaviour, which is a change for a config
        // written before this key existed -- deliberately, because that config
        // predates the fix and its owner is the person who reported the
        // reprinted characters.  The boot picker asks again either way.
        cpm_boot_backspace: map
            .get("cpm_boot_backspace")
            .cloned()
            .unwrap_or_else(|| crate::cpm::boot::DEFAULT_BACKSPACE.to_string()),
        // **Asymmetric, on purpose, exactly like `setup_wizard_completed`.**
        // A *new* config gets `DEFAULT_PRINTER` (text since 0.9.2) because a
        // printout that goes nowhere is a printout lost.  A config file that
        // EXISTS and does not mention the key is an upgrade -- `cpm_printer`
        // first shipped in 0.9.1, so every 0.9.0 install is one -- and an
        // upgrade must not start writing files into somebody's transfer folder
        // because they installed a new version.  They never asked for a
        // printer; changing a default is not consent.
        //
        // The two cases really are distinguishable: a fresh install has the key
        // written out with everything else, so only an older file can be
        // missing it.
        cpm_printer: map
            .get("cpm_printer")
            .cloned()
            .unwrap_or_else(|| crate::cpm::printer::PRINTER_OFF.to_string()),
        cpm_printer_port: map
            .get("cpm_printer_port")
            .cloned()
            .unwrap_or_else(|| crate::cpm::printer::DEFAULT_PRINTER_PORT.to_string()),
        cpm_printer_autolf: map
            .get("cpm_printer_autolf")
            .cloned()
            .unwrap_or_else(|| crate::cpm::printer::DEFAULT_PRINTER_AUTOLF.to_string()),
        // Missing means the Z80, which is what every config written before this
        // key existed was already running -- so an upgrade changes nothing.
        cpm_cpu: map
            .get("cpm_cpu")
            .cloned()
            .unwrap_or_else(|| crate::cpm::cpu::DEFAULT_CPU.to_string()),
        cpm_emu_modem: CpmModemProfile {
            echo: map
                .get("cpm_emu_echo")
                .map(|v| v.eq_ignore_ascii_case("true"))
                .unwrap_or(true),
            verbose: map
                .get("cpm_emu_verbose")
                .map(|v| v.eq_ignore_ascii_case("true"))
                .unwrap_or(true),
            quiet: map
                .get("cpm_emu_quiet")
                .map(|v| v.eq_ignore_ascii_case("true"))
                .unwrap_or(false),
            x_code: map
                .get("cpm_emu_x_code")
                .and_then(|v| v.parse().ok())
                .filter(|&v: &u8| v <= 4)
                .unwrap_or(4),
            dcd_mode: map
                .get("cpm_emu_dcd_mode")
                .and_then(|v| v.parse().ok())
                .filter(|&v: &u8| v <= 1)
                .unwrap_or(1),
            s_regs: map.get("cpm_emu_s_regs").cloned().unwrap_or_default(),
        },
        serial_a: read_serial_port_config(&map, "serial_a", true),
        serial_b: read_serial_port_config(&map, "serial_b", false),
        ssh_enabled: map
            .get("ssh_enabled")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(DEFAULT_SSH_ENABLED),
        ssh_port: map
            .get("ssh_port")
            .and_then(|v| v.parse().ok())
            .filter(|&v: &u16| v >= 1)
            .unwrap_or(DEFAULT_SSH_PORT),
        ssh_gateway_auth: map
            .get("ssh_gateway_auth")
            .map(|v| v.trim().to_ascii_lowercase())
            .filter(|v| matches!(v.as_str(), "key" | "password"))
            .unwrap_or_else(|| DEFAULT_SSH_GATEWAY_AUTH.into()),
        gateway_role: map
            .get("gateway_role")
            .map(|v| v.trim().to_ascii_lowercase())
            .filter(|v| matches!(v.as_str(), "standalone" | "master" | "slave"))
            .unwrap_or_else(|| DEFAULT_GATEWAY_ROLE.into()),
        master_accept_relays: map
            .get("master_accept_relays")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(DEFAULT_MASTER_ACCEPT_RELAYS),
        allow_relay_kermit: map
            .get("allow_relay_kermit")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(DEFAULT_ALLOW_RELAY_KERMIT),
        slave_master_host: map
            .get("slave_master_host")
            .map(|v| v.trim().to_string())
            .unwrap_or_default(),
        slave_master_port: map
            .get("slave_master_port")
            .and_then(|v| v.parse().ok())
            .filter(|&v: &u16| v >= 1)
            .unwrap_or(DEFAULT_SLAVE_MASTER_PORT),
        slave_master_username: map
            .get("slave_master_username")
            .cloned()
            .unwrap_or_default(),
        slave_master_password: map
            .get("slave_master_password")
            .cloned()
            .unwrap_or_default(),
        relay_transport: map
            .get("relay_transport")
            .map(|v| v.trim().to_ascii_lowercase())
            .filter(|v| matches!(v.as_str(), "ssh" | "raw"))
            .unwrap_or_else(|| DEFAULT_RELAY_TRANSPORT.into()),
    };

    // ── Legacy ssh_username / ssh_password migration ───────────
    // Older configs kept independent credentials for telnet and SSH
    // under `ssh_username` / `ssh_password`.  Those are now merged
    // into the unified `username` / `password` pair shared across
    // telnet, SSH, and the web UI.  If an upgrading config still has
    // the legacy keys with a non-default value AND the unified pair
    // is still at the factory default, adopt the legacy SSH value so
    // the operator's working SSH login keeps working until they
    // explicitly change it.  This keeps a one-time silent path open
    // while letting the unified pair fully replace the old keys on
    // the next save.
    if cfg.username == DEFAULT_USERNAME
        && let Some(legacy) = map.get("ssh_username").filter(|v| !v.is_empty() && v.as_str() != DEFAULT_USERNAME)
    {
        // Don't log the username *value* — it lands in the console log the
        // web `/logs` snapshot exposes, and the parallel password migration
        // below deliberately logs only that it happened, not the secret.
        glog!(
            "Note: migrating legacy ssh_username to unified username (telnet+SSH+web share creds now)."
        );
        cfg.username = legacy.clone();
    }
    if cfg.password == DEFAULT_PASSWORD
        && let Some(legacy) = map.get("ssh_password").filter(|v| !v.is_empty() && v.as_str() != DEFAULT_PASSWORD)
    {
        glog!(
            "Note: migrating legacy ssh_password to unified password (telnet+SSH+web share creds now)."
        );
        cfg.password = legacy.clone();
    }
    Ok(cfg)
}

/// Read one port's settings from `map` under `prefix` (e.g. `"serial_a"`).
///
/// When `legacy_fallback` is true, missing `serial_a_*` keys fall back to
/// the legacy un-prefixed `serial_*` keys.  This is the dual-port
/// migration path: an existing single-port `egateway.conf` continues to
/// load into Port A on startup, and the next `save_config` rewrites it
/// under the new key names.  Port B never falls back — its only valid
/// source is `serial_b_*` keys.
fn read_serial_port_config(
    map: &HashMap<String, String>,
    prefix: &str,
    legacy_fallback: bool,
) -> SerialPortConfig {
    // Look up `<prefix>_<key>` first; if absent and legacy_fallback is on,
    // try the legacy `serial_<key>` form (or the explicit override for
    // the four stored-number slots, which use `serial_stored_<n>`).
    let lookup = |suffix: &str, legacy: &str| -> Option<String> {
        let primary = format!("{}_{}", prefix, suffix);
        if let Some(v) = map.get(&primary) {
            return Some(v.clone());
        }
        if legacy_fallback {
            if let Some(v) = map.get(legacy) {
                return Some(v.clone());
            }
        }
        None
    };

    SerialPortConfig {
        enabled: lookup("enabled", "serial_enabled")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(DEFAULT_SERIAL_ENABLED),
        mode: lookup("mode", "serial_mode")
            .map(|v| v.trim().to_ascii_lowercase())
            .filter(|v| matches!(v.as_str(), "modem" | "console" | "kermit"))
            .unwrap_or_else(|| DEFAULT_SERIAL_MODE.into()),
        port: lookup("port", "serial_port").unwrap_or_else(|| DEFAULT_SERIAL_PORT.into()),
        baud: lookup("baud", "serial_baud")
            .and_then(|v| v.parse().ok())
            .filter(|&v: &u32| v >= 300)
            .unwrap_or(DEFAULT_SERIAL_BAUD),
        databits: lookup("databits", "serial_databits")
            .and_then(|v| v.parse().ok())
            .filter(|&v: &u8| matches!(v, 5..=8))
            .unwrap_or(DEFAULT_SERIAL_DATABITS),
        parity: lookup("parity", "serial_parity")
            .map(|v| v.trim().to_ascii_lowercase())
            .filter(|v| matches!(v.as_str(), "none" | "odd" | "even"))
            .unwrap_or_else(|| DEFAULT_SERIAL_PARITY.into()),
        stopbits: lookup("stopbits", "serial_stopbits")
            .and_then(|v| v.parse().ok())
            .filter(|&v: &u8| v == 1 || v == 2)
            .unwrap_or(DEFAULT_SERIAL_STOPBITS),
        flowcontrol: lookup("flowcontrol", "serial_flowcontrol")
            .map(|v| v.trim().to_ascii_lowercase())
            .filter(|v| matches!(v.as_str(), "none" | "hardware" | "software"))
            .unwrap_or_else(|| DEFAULT_SERIAL_FLOWCONTROL.into()),
        echo: lookup("echo", "serial_echo")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(DEFAULT_SERIAL_ECHO),
        verbose: lookup("verbose", "serial_verbose")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(DEFAULT_SERIAL_VERBOSE),
        quiet: lookup("quiet", "serial_quiet")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(DEFAULT_SERIAL_QUIET),
        s_regs: lookup("s_regs", "serial_s_regs")
            .unwrap_or_else(|| DEFAULT_SERIAL_S_REGS.into()),
        x_code: lookup("x_code", "serial_x_code")
            .and_then(|v| v.parse::<u8>().ok())
            .filter(|&v| v <= 4)
            .unwrap_or(DEFAULT_SERIAL_X_CODE),
        dtr_mode: lookup("dtr_mode", "serial_dtr_mode")
            .and_then(|v| v.parse::<u8>().ok())
            .filter(|&v| v <= 3)
            .unwrap_or(DEFAULT_SERIAL_DTR_MODE),
        flow_mode: lookup("flow_mode", "serial_flow_mode")
            .and_then(|v| v.parse::<u8>().ok())
            .filter(|&v| v <= 4)
            .unwrap_or(DEFAULT_SERIAL_FLOW_MODE),
        dcd_mode: lookup("dcd_mode", "serial_dcd_mode")
            .and_then(|v| v.parse::<u8>().ok())
            .filter(|&v| v <= 1)
            .unwrap_or(DEFAULT_SERIAL_DCD_MODE),
        stored_numbers: [
            lookup("stored_0", "serial_stored_0").unwrap_or_default(),
            lookup("stored_1", "serial_stored_1").unwrap_or_default(),
            lookup("stored_2", "serial_stored_2").unwrap_or_default(),
            lookup("stored_3", "serial_stored_3").unwrap_or_default(),
        ],
        petscii_translate: lookup("petscii_translate", "serial_petscii_translate")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(DEFAULT_SERIAL_PETSCII_TRANSLATE),
        drive_carrier: lookup("drive_carrier", "serial_drive_carrier")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(DEFAULT_SERIAL_DRIVE_CARRIER),
    }
}

/// Sanitize a config value for writing: strip newlines/carriage returns
/// (which would corrupt the line-based file framing) and trim surrounding
/// whitespace.  The reader trims values too (see `read_config_file`), so
/// trimming on write keeps a load→save→load round-trip stable instead of
/// silently mutating a value's surrounding whitespace across one save.
fn sanitize_value(s: &str) -> String {
    s.chars()
        .filter(|&c| c != '\n' && c != '\r')
        .collect::<String>()
        .trim()
        .to_string()
}

/// Save a full `Config` to disk and update the global singleton.
/// Used by the GUI save button.
///
/// Holds the `CONFIG` mutex across the write so that a concurrent
/// `update_config_values` (from a session-side toggle, e.g. the Telnet
/// Gateway's raw-mode toggle) can't race and clobber our write with its
/// own re-read-then-write.
/// Persist `cfg` to disk and refresh the in-memory singleton.
///
/// Returns `Err` with a human-readable reason if the file write failed, so
/// an explicit "Save" action can tell the user the change was not persisted
/// instead of silently reporting success.  The in-memory cache is updated
/// regardless so the running process reflects the requested settings even
/// when persistence fails.
/// Lay out the CP/M container for a config that has just enabled the emulator.
///
/// Kept here rather than at each call site so telnet, the web server and the
/// desktop GUI cannot drift about whether they do it — the three-surface rule
/// this config file lives by.
fn ensure_cpm_layout(cfg: &Config) {
    if let Err(e) = crate::cpm::layout::ensure_cpm_tree(&cfg.transfer_dir) {
        glog!("Warning: could not create the CP/M folders: {}", e);
    }
}

pub fn save_config(cfg: &Config) -> Result<(), String> {
    let mut guard = CONFIG.lock().unwrap_or_else(|e| e.into_inner());
    let was_cpm_enabled = guard.as_ref().map(|c| c.cpm_emu_enabled).unwrap_or(false);
    let result = write_config_file(&config_file_path(), cfg);
    if let Err(ref e) = result {
        glog!("Warning: {}", e);
    }
    if cfg.cpm_emu_enabled && !was_cpm_enabled {
        ensure_cpm_layout(cfg);
    }
    if was_cpm_enabled && !cfg.cpm_emu_enabled {
        crate::cpm::image::registry::clear_all();
    }
    *guard = Some(cfg.clone());
    result
}

/// Write `key = value` to `out`, applying `Display` formatting to the
/// value.  Used for booleans (`Display` yields `"true"` / `"false"`)
/// and integer fields.
fn write_kv(out: &mut String, key: &str, value: impl std::fmt::Display) {
    use std::fmt::Write;
    let _ = writeln!(out, "{} = {}", key, value);
}

/// Write `key = value` to `out` for string fields, sanitizing the
/// value to strip embedded newlines/CRs that would otherwise corrupt
/// the file framing.
fn write_kv_str(out: &mut String, key: &str, value: &str) {
    use std::fmt::Write;
    let _ = writeln!(out, "{} = {}", key, sanitize_value(value));
}

/// Emit one port's full settings section (header comment, all key/value
/// lines, trailing blank).  Centralizes the dual-port symmetry so adding
/// a new per-port field touches one place instead of two.
fn write_serial_port_section(
    out: &mut String,
    title: &str,
    prefix: &str,
    port: &SerialPortConfig,
) {
    use std::fmt::Write;
    let _ = writeln!(out, "# {}", title);
    write_kv(out, &format!("{}_enabled", prefix), port.enabled);
    write_kv_str(out, &format!("{}_mode", prefix), &port.mode);
    write_kv_str(out, &format!("{}_port", prefix), &port.port);
    write_kv(out, &format!("{}_baud", prefix), port.baud);
    write_kv(out, &format!("{}_databits", prefix), port.databits);
    write_kv_str(out, &format!("{}_parity", prefix), &port.parity);
    write_kv(out, &format!("{}_stopbits", prefix), port.stopbits);
    write_kv_str(out, &format!("{}_flowcontrol", prefix), &port.flowcontrol);
    write_kv(out, &format!("{}_echo", prefix), port.echo);
    write_kv(out, &format!("{}_verbose", prefix), port.verbose);
    write_kv(out, &format!("{}_quiet", prefix), port.quiet);
    write_kv_str(out, &format!("{}_s_regs", prefix), &port.s_regs);
    write_kv(out, &format!("{}_x_code", prefix), port.x_code);
    write_kv(out, &format!("{}_dtr_mode", prefix), port.dtr_mode);
    write_kv(out, &format!("{}_flow_mode", prefix), port.flow_mode);
    write_kv(out, &format!("{}_dcd_mode", prefix), port.dcd_mode);
    for (i, slot) in port.stored_numbers.iter().enumerate() {
        write_kv_str(out, &format!("{}_stored_{}", prefix, i), slot);
    }
    write_kv(out, &format!("{}_petscii_translate", prefix), port.petscii_translate);
    write_kv(out, &format!("{}_drive_carrier", prefix), port.drive_carrier);
    out.push('\n');
}

/// Write the config file with comments.
///
/// Section-by-section build pattern: each comment block + `write_kv`
/// call pairs the human-visible key name with its `cfg.field` value at
/// the call site.  Adding a new field is one section addition.
/// Replacing the original 60-positional-arg `format!()` template
/// closes the misalignment footgun where missing one slot mid-template
/// would silently shift every subsequent field onto the wrong line.
fn write_config_file(path: &str, cfg: &Config) -> Result<(), String> {
    let mut content = String::with_capacity(8192);

    content.push_str("\
# Ethernet Gateway Configuration
#
# This file is auto-generated if it does not exist.
# Edit values below to customise the server.

");

    content.push_str("# Telnet server: set to false to disable (SSH-only mode)\n");
    write_kv(&mut content, "telnet_enabled", cfg.telnet_enabled);
    content.push('\n');

    content.push_str("# Telnet server port\n");
    write_kv(&mut content, "telnet_port", cfg.telnet_port);
    content.push('\n');

    content.push_str("\
# Outgoing Telnet Gateway cooperative negotiation.
# When true, the gateway offers WILL TTYPE / WILL NAWS at connect and
# accepts DO TTYPE / DO NAWS requests from the remote server.  Leave this
# false if you dial raw-TCP services (legacy MUDs, hand-rolled BBSes that
# don't implement the telnet protocol) — those would see the IAC offers
# as garbage bytes.  ECHO cooperation is always on regardless of this
# setting (raw-TCP services never send WILL ECHO).
");
    write_kv(&mut content, "telnet_gateway_negotiate", cfg.telnet_gateway_negotiate);
    content.push('\n');

    content.push_str("\
# Outgoing Telnet Gateway raw-TCP escape hatch.
# When true, the gateway bypasses its entire telnet-IAC layer and treats
# the remote as a raw TCP byte stream.  Last-resort override for
# destinations that clearly aren't telnet at all.  Supersedes
# telnet_gateway_negotiate (there's nothing to negotiate in raw mode).
# Toggleable from the Telnet Gateway menu.
");
    write_kv(&mut content, "telnet_gateway_raw", cfg.telnet_gateway_raw);
    content.push('\n');

    content.push_str("\
# Byte-trace (debug).  Verbose, per-byte output for diagnosing input
# corruption and terminal-translation issues.  Read fresh by each session,
# so toggling takes effect on the next one without a restart.  Toggleable
# from the GUI/web General settings and the Serial Configuration menu.  The
# EGATEWAY_GATEWAY_DEBUG environment variable forces it on too.
#   It covers FOUR things, all under this one key:
#     - the SSH/Telnet gateway proxy loops;
#     - [esc]   the Hayes escape: which escape character was accepted, how
#               much silence preceded it, and the byte that broke a
#               sequence - that last one tells a failed +++ from a slow one;
#     - [modem] the modem's result codes (OK, NO CARRIER, ...).  Traced
#               apart from session output because they ARE separate: a
#               result code comes from the modem, not from the host you
#               dialled;
#     - cpmkey  every session byte in and out, and what the CP/M console's
#               escape state machine decided about it.
#   Leave it off in normal use: it logs a line per byte or per write.
");
    write_kv(&mut content, "gateway_debug", cfg.gateway_debug);
    content.push('\n');

    content.push_str("\
# Terminal geometry reported to the remote host by the SSH and Telnet
# gateways (SSH PTY request / telnet NAWS).  Leave both 0 for automatic:
# the size your client negotiated via NAWS, or the terminal-type default
# (40x25 for PETSCII, 80x24 for ANSI/ASCII) when it negotiated none.
#
# Set these when the automatic answer is wrong, which it is for most retro
# clients: terminal *type* does not imply terminal *width*.  A C64 running
# CCGMS in ASCII mode sends 0x08 for backspace, so it is detected as ANSI
# and told it has 80 columns for a physically 40-column screen; CCGMS's
# soft 80-column mode is the mirror case in PETSCII.  WiFi modems and
# tcpser don't send NAWS for the C64, so only you can say.  When the remote
# has the wrong width, everything past the real margin -- line wrap,
# backspace, history recall, tab completion -- is drawn in the wrong place.
");
    write_kv(&mut content, "gateway_term_width", cfg.gateway_term_width);
    write_kv(&mut content, "gateway_term_height", cfg.gateway_term_height);
    content.push('\n');

    content.push_str("\
# Show the GUI configuration/console window on startup.
# Set to false when running as a headless service.
");
    write_kv(&mut content, "enable_console", cfg.enable_console);
    content.push('\n');

    content.push_str("\
# Set true once the desktop GUI's first-run setup wizard has been completed or
# skipped; the wizard only appears while this is false.  Set it back to false
# (or use \"Run setup wizard...\" in the GUI's Server \"More\" window) to walk
# through the initial configuration again.  A pre-existing config file with no
# such key is treated as already-configured, so upgrades never see the wizard.
");
    write_kv(&mut content, "setup_wizard_completed", cfg.setup_wizard_completed);
    content.push('\n');

    content.push_str("# Security: set to true to require username/password login\n");
    write_kv(&mut content, "security_enabled", cfg.security_enabled);
    content.push('\n');

    content.push_str("\
# Disable IP-safety allowlist.  When security_enabled is false, the telnet
# listener normally rejects every non-private source IP and every
# gateway-style *.*.*.1 address — that allowlist is the only thing
# standing between a public IP and an unauthenticated session.  Set this
# to true to accept connections from every source regardless.  For the
# TELNET listener it has no effect when security_enabled is true (auth
# runs in either case).  The WEB server is different: it keeps the
# allowlist even with login on (the config page shows the password / API
# key), so `disable_ip_safety = true` is the ONLY way to reach the web UI
# from a non-private IP.  Toggleable from the GUI Security frame and the
# telnet Server Configuration menu — both gate the off→on transition
# behind a security-warning confirmation.
# disable_gateway_connections: refuse connections whose source address ends
#   in .1 -- usually the router on this subnet -- while the allowlist applies.
#   Off by default, so those connections are allowed; loopback (127.0.0.1) is
#   never affected either way.  This is the narrow alternative to
#   disable_ip_safety, which drops the allowlist altogether.
");
    write_kv(&mut content, "disable_ip_safety", cfg.disable_ip_safety);
    write_kv(&mut content, "disable_gateway_connections", cfg.disable_gateway_connections);
    content.push('\n');

    content.push_str("# Credentials (only used when security_enabled = true)\n");
    write_kv_str(&mut content, "username", &cfg.username);
    write_kv_str(&mut content, "password", &cfg.password);
    content.push('\n');

    content.push_str("# Directory for file transfers (relative to working directory)\n");
    write_kv_str(&mut content, "transfer_dir", &cfg.transfer_dir);
    content.push_str("\
# place_bundled_terminals: write EGT8080.COM and EGT80.COM when they are
#   missing?  ON by default.  They are this gateway's own CP/M terminal in
#   period assembly, compiled into the binary, and each is placed in TWO
#   places: CP/M drive A:, where the emulator runs it, and loose in the
#   transfer directory, where the file-transfer menus can send it to real
#   hardware without starting the emulator at all.  Nothing else puts them
#   there on a fresh install.
#   A file already present is NEVER overwritten whatever this says -- each
#   terminal saves its settings inside its own .COM, so refreshing a copy
#   would discard your configuration.  So this only decides whether a
#   MISSING one is written: turn it off if you keep your own build, or your
#   own EGT80.COM from before 0.9.2, and would rather a file you deleted
#   stayed deleted instead of reappearing on the next restart.
");
    write_kv(&mut content, "place_bundled_terminals", cfg.place_bundled_terminals);
    write_kv_str(&mut content, "gui_window_geometry", &cfg.gui_window_geometry);
    content.push('\n');

    content.push_str(
        "# Desktop GUI display scale: \"auto\" follows the monitor's scale factor,\n\
         # or set a number (e.g. 1.0, 1.25, 0.8) to pin the console size on a\n\
         # display whose reported DPI makes the window too large or small.\n",
    );
    write_kv_str(&mut content, "gui_zoom", &cfg.gui_zoom);
    content.push('\n');

    content.push_str("# Maximum concurrent telnet sessions\n");
    write_kv(&mut content, "max_sessions", cfg.max_sessions);
    content.push('\n');

    content.push_str("# Idle session timeout in seconds (0 = no timeout)\n");
    write_kv(&mut content, "idle_timeout_secs", cfg.idle_timeout_secs);
    content.push('\n');

    content.push_str("\
# Groq API key for AI Chat (get one at https://console.groq.com/keys)
# Leave empty to disable AI Chat.
");
    write_kv_str(&mut content, "groq_api_key", &cfg.groq_api_key);
    content.push('\n');

    content.push_str("\
# Browser homepage URL (loaded automatically when entering the browser)
# Leave empty to start with a blank prompt.
");
    write_kv_str(&mut content, "browser_homepage", &cfg.browser_homepage);
    content.push('\n');

    content.push_str("# Last-used weather location: city or postal code, worldwide\n");
    content.push_str("# (e.g. 62051, \"London, GB\", Zurich) -- updated automatically when you check weather\n");
    write_kv_str(&mut content, "weather_location", &cfg.weather_location);
    content.push('\n');

    content.push_str("# Weather units: auto (infer from country), us (F/mph), or metric (C/km/h)\n");
    write_kv_str(&mut content, "weather_units", &cfg.weather_units);
    content.push('\n');

    content.push_str("# Log file: mirror the console log to disk as well as stderr.\n");
    content.push_str("# The log is size-bounded and old generations are DELETED, so it cannot\n");
    content.push_str("# grow without limit: the active file is rotated to .1 once it reaches\n");
    content.push_str("# log_max_size_kb, .1 becomes .2 and so on, and anything past\n");
    content.push_str("# log_max_files is removed.  Worst-case disk use is therefore\n");
    content.push_str("# log_max_size_kb x (log_max_files + 1) -- 6 MB with the defaults below.\n");
    content.push_str("# Set log_max_size_kb = 0 to disable size-based rotation (unbounded growth),\n");
    content.push_str("# or log_max_files = 0 to keep no history at all.\n");
    write_kv(&mut content, "log_to_file", cfg.log_to_file);
    write_kv_str(&mut content, "log_file", &cfg.log_file);
    write_kv(&mut content, "log_max_size_kb", cfg.log_max_size_kb);
    write_kv(&mut content, "log_max_files", cfg.log_max_files);
    content.push('\n');

    content.push_str("# Verbose logging: set to true for detailed XMODEM protocol diagnostics\n");
    write_kv(&mut content, "verbose", cfg.verbose);
    content.push('\n');

    content.push_str("\
# XMODEM-family protocol timeouts (apply to XMODEM, XMODEM-1K, and YMODEM).
# xmodem_negotiation_timeout:      seconds to wait for the peer to start sending.
# xmodem_block_timeout:            seconds to wait for each data block.
# xmodem_max_retries:              retry limit per block.
# xmodem_negotiation_retry_interval: seconds between C/NAK pokes during
#                                    the initial handshake (spec suggests 10 s,
#                                    default 7 s).
");
    write_kv(&mut content, "xmodem_negotiation_timeout", cfg.xmodem_negotiation_timeout);
    write_kv(&mut content, "xmodem_block_timeout", cfg.xmodem_block_timeout);
    write_kv(&mut content, "xmodem_max_retries", cfg.xmodem_max_retries);
    write_kv(&mut content, "xmodem_negotiation_retry_interval", cfg.xmodem_negotiation_retry_interval);
    content.push('\n');

    content.push_str("\
# ZMODEM protocol tunables.
# zmodem_negotiation_timeout:       seconds to wait for ZRQINIT / ZRINIT handshake.
# zmodem_frame_timeout:             seconds to wait for each header / subpacket.
# zmodem_max_retries:               retry limit for ZRQINIT / ZRPOS / ZDATA frames.
# zmodem_negotiation_retry_interval: seconds between ZRINIT / ZRQINIT re-sends
#                                    during the handshake (default 5 s).
");
    write_kv(&mut content, "zmodem_negotiation_timeout", cfg.zmodem_negotiation_timeout);
    write_kv(&mut content, "zmodem_frame_timeout", cfg.zmodem_frame_timeout);
    write_kv(&mut content, "zmodem_max_retries", cfg.zmodem_max_retries);
    write_kv(&mut content, "zmodem_negotiation_retry_interval", cfg.zmodem_negotiation_retry_interval);
    content.push('\n');

    content.push_str("\
# Kermit protocol tunables.
# kermit_negotiation_timeout:  seconds to wait for the Send-Init handshake.
# kermit_packet_timeout:       seconds to wait for each packet response.
# kermit_idle_timeout:         seconds the gateway's Kermit *server* waits
#                              between commands from the peer before sending
#                              an idle-timeout error and disconnecting.  Set
#                              to 0 to disable the deadline entirely (server
#                              waits indefinitely for the peer's next command).
#                              Distinct from kermit_negotiation_timeout, which
#                              bounds the handshake itself.
# kermit_max_retries:          retry limit per packet on NAK / timeout.
# kermit_max_packet_length:    advertised MAXL (10..=9024).  Long packets are
#                              negotiated separately; values >94 require the
#                              peer to also support extended-length packets.
# kermit_window_size:          sliding-window depth (1..=31).  1 = stop-and-wait.
# kermit_block_check_type:     1 = 6-bit checksum, 2 = 12-bit, 3 = CRC-16/KERMIT.
# kermit_long_packets:         advertise long-packet capability.
# kermit_sliding_windows:      advertise sliding-window capability.
# kermit_streaming:            advertise streaming-Kermit (no per-packet ACKs).
#                              Big speed win on TCP/SSH; turn this off only if
#                              your remote side bridges into an unreliable
#                              serial line (some WiFi modems do this).
# kermit_attribute_packets:    advertise A-packet (file metadata) support.
# kermit_repeat_compression:   use repeat-count compression (RLE).
# kermit_8bit_quote:           auto (only when peer asks), on, or off.
# kermit_resume_partial:       resume partial uploads (spec disposition='R').
#                              Off by default; turn on only when the peer is
#                              known to honor disposition='R' in the A-packet
#                              ACK, otherwise the transfer can corrupt the
#                              file.
# kermit_resume_max_age_hours: ignore on-disk partials older than this when
#                              deciding whether to resume.  168 = one week.
# kermit_locking_shifts:       advertise SO/SI region-shift capability for
#                              8-bit transit on 7-bit links (Frank da Cruz
#                              §3.4.5).  Off by default — no modern Kermit
#                              peer (C-Kermit, G-Kermit, Kermit-95, E-Kermit)
#                              negotiates it; flip on only if you're talking
#                              to a strict-spec implementation that does.
# kermit_wait_for_receiver:    on a download, wait for the receiver's first
#                              NAK before sending our Send-Init, so it doesn't
#                              land as on-screen garbage before the client is
#                              in receive mode.  On by default; sends anyway if
#                              no NAK arrives within kermit_negotiation_timeout.
# allow_atdt_kermit:           let `ATDT KERMIT` from the serial modem
#                              emulator drop directly into Kermit server mode
#                              without going through the telnet menu.  Off
#                              by default because it bypasses any
#                              security_enabled username/password gate.
#                              Enable only on trusted serial lines; for any
#                              auth-required deployment leave this off and
#                              have callers go via the telnet F/K path.
# allow_peer_dial:             let a modem-mode serial port dial another port
#                              directly (ATD <Port>@<IP>, or pick a modem port
#                              in the Serial Gateway menu) and bridge to the
#                              device on it, instead of always landing on the
#                              gateway menu.  A dialed modem port rings and
#                              answers per its own AT rules (S0 auto-answer /
#                              manual ATA); a console port connects directly.
#                              On a MASTER this flag ALSO gates a slave's relay
#                              onward-dial (Model B): the master refuses to open
#                              the outbound connection to a slave's requested
#                              host:port unless this is on.
#                              Off by default (opt-in even on a trusted LAN).
");
    write_kv(&mut content, "kermit_negotiation_timeout", cfg.kermit_negotiation_timeout);
    write_kv(&mut content, "kermit_packet_timeout", cfg.kermit_packet_timeout);
    write_kv(&mut content, "kermit_idle_timeout", cfg.kermit_idle_timeout);
    write_kv(&mut content, "kermit_max_retries", cfg.kermit_max_retries);
    write_kv(&mut content, "kermit_max_packet_length", cfg.kermit_max_packet_length);
    write_kv(&mut content, "kermit_window_size", cfg.kermit_window_size);
    write_kv(&mut content, "kermit_block_check_type", cfg.kermit_block_check_type);
    write_kv(&mut content, "kermit_long_packets", cfg.kermit_long_packets);
    write_kv(&mut content, "kermit_sliding_windows", cfg.kermit_sliding_windows);
    write_kv(&mut content, "kermit_streaming", cfg.kermit_streaming);
    write_kv(&mut content, "kermit_attribute_packets", cfg.kermit_attribute_packets);
    write_kv(&mut content, "kermit_repeat_compression", cfg.kermit_repeat_compression);
    write_kv_str(&mut content, "kermit_8bit_quote", &cfg.kermit_8bit_quote);
    write_kv(&mut content, "kermit_resume_partial", cfg.kermit_resume_partial);
    write_kv(&mut content, "kermit_resume_max_age_hours", cfg.kermit_resume_max_age_hours);
    write_kv(&mut content, "kermit_locking_shifts", cfg.kermit_locking_shifts);
    write_kv(&mut content, "kermit_wait_for_receiver", cfg.kermit_wait_for_receiver);
    write_kv(&mut content, "allow_atdt_kermit", cfg.allow_atdt_kermit);
    write_kv(&mut content, "allow_peer_dial", cfg.allow_peer_dial);
    content.push('\n');

    content.push_str("\
# Standalone Kermit server listener.
# kermit_server_enabled:  bind a dedicated TCP port that drops every accepted
#                         connection straight into Kermit server mode — no
#                         telnet menu, no auth gate, no private-IP allowlist.
#                         Off by default; enabling it bypasses every security
#                         check the gateway has, so opt in only when the
#                         network path itself is trusted.
# kermit_server_port:     TCP port for the listener (default 2424).
");
    write_kv(&mut content, "kermit_server_enabled", cfg.kermit_server_enabled);
    write_kv(&mut content, "kermit_server_port", cfg.kermit_server_port);
    content.push('\n');

    content.push_str("\
# Punter (C1) protocol tunables.  C1 is the file-transfer protocol CCGMS /
# Novaterm / StrikeTerm speak natively on Commodore BBSes.
# punter_block_size:                 total block size in bytes (8..=255, the
#                                    7-byte header included).  255 = native max
#                                    (248-byte payload); lower it toward 40 for
#                                    noisy lines at the cost of handshake overhead.
# punter_negotiation_timeout:        seconds to wait for the peer's first code.
# punter_block_timeout:              per-block read timeout once under way.
# punter_max_retries:                handshake-code / block retry limit.
# punter_max_bad_rounds:             consecutive corrupt-block resend rounds
#                                    tolerated before giving up (kept higher
#                                    than max_retries; a real C64 peer never
#                                    caps these, so a low value strands it).
# punter_negotiation_retry_interval: seconds between code re-sends.
# punter_hangup_on_failure:          drop the connection (carrier) when a
#                                    transfer gives up so the C64 — which C1
#                                    can't be told to abort — exits instead of
#                                    hanging.  Ends the whole session; off by
#                                    default.
");
    write_kv(&mut content, "punter_block_size", cfg.punter_block_size);
    write_kv(&mut content, "punter_negotiation_timeout", cfg.punter_negotiation_timeout);
    write_kv(&mut content, "punter_block_timeout", cfg.punter_block_timeout);
    write_kv(&mut content, "punter_max_retries", cfg.punter_max_retries);
    write_kv(&mut content, "punter_max_bad_rounds", cfg.punter_max_bad_rounds);
    write_kv(
        &mut content,
        "punter_negotiation_retry_interval",
        cfg.punter_negotiation_retry_interval,
    );
    write_kv(&mut content, "punter_hangup_on_failure", cfg.punter_hangup_on_failure);
    content.push('\n');

    content.push_str("\
# Configuration web server.  Renders the same settings page the GUI
# does, in a browser.  Accepts only private/loopback source IPs unless
# `disable_ip_safety` is set; unlike the telnet listener this allowlist
# applies whether or not login is required (the page shows the password /
# API key).  HTTP Basic auth uses the same `security_enabled` flag and the
# `username` / `password` credentials.
# web_enabled: bind a TCP listener on `web_port` and serve the
#              configuration page.
# web_port:    TCP port for the web listener (default 8080).
");
    write_kv(&mut content, "web_enabled", cfg.web_enabled);
    write_kv(&mut content, "web_port", cfg.web_port);
    content.push('\n');

    content.push_str("\
# CP/M emulator.  When enabled, the main menu offers a 'CP/M
# System' item that runs a real CP/M 2.2 environment on an emulated Z80 - or
# an 8080, see cpm_cpu below - running .COM software the user launches,
# sandboxed to a CPM/ directory under transfer_dir.  On by default; set it off and the menu
# item is hidden and the key is rejected.
# cpm_emu_enabled: the CP/M emulator main-menu item (on by default).  It runs
#   the Z80 software a user launches -- be sure you trust the CP/M files you
#   run -- jailed to the CPM/ directory under transfer_dir and bounded by
#   cpm_emu_max_minstr; set false to hide the menu item and reject the key.
#   The guest reaches the network only through cpm_emu_uart, which now defaults
#   to rc2014_1b (the port the bundled EGT8080 terminal expects); set it to off
#   to leave the emulator with no modem.
# cpm_emu_max_minstr: runaway ceiling per program run, in millions of
#   instructions (2000 = 2 billion).  A compute-bound .COM that never reads
#   the console is aborted at this count so the A> prompt always returns.
#   Minimum 1; anything above 1000000 is CAPPED at 1000000 rather than
#   refused, so asking for `no limit' gets you as close as this goes -- which
#   at emulated speed is over three months of continuous running.  Note that
#   this bounds one transient program in the EMULATOR only: a booted disk is
#   the session and is meant to sit at its prompt, so it has no such ceiling.
# cpm_emu_uart: how the emulated CP/M reaches the virtual modem.  off
#   (default) = no modem; a machine/port profile, e.g. rc2014_1b (RC2014 SIO/2
#   0x82/0x83), altair_2sio1 (Altair 88-2SIO 0x10/0x11); aux (BDOS AUX:
#   device); or hbios_1 / hbios_2 (RomWBW HBIOS serial unit 1 / 2, reached by
#   RST 8 — for software built for RomWBW rather than for a bare UART, such as
#   the QTERM 'h' builds).
# cpm_screen_input: may the web UI's disk-screen page TYPE at a booted disk?
#   The screen itself is always readable when the web server is on; this
#   decides whether it is also a keyboard.  On by default.  The two keyboards
#   share one queue, exactly as two keyboards on one port would, so the person
#   at the terminal and the person in the browser can both type -- and their
#   characters interleave if they do it at once, which is what a shared
#   terminal is.  The ESC ESC exit gesture is NOT honoured from a browser:
#   ending a session somebody else is sitting at is not a keystroke.
# cpm_boot_writable: may a booted disk WRITE to the images it is running?
#   ON by default, because a vintage OS saves files, formats disks and updates
#   its own directory -- boot one read-only and every SAVE appears to work and
#   is gone at the next boot.  A mounted image is read through our own
#   filesystem, which can refuse a request it does not like; a booted disk owns
#   the whole image and rewrites the file when it leaves, so there is no guard
#   left that understands what the guest is asking for.  What remains is blunt:
#   this key, and one session per image.  It applies to the boot disk AND to
#   every image that comes along mounted, because they are all in the same
#   machine.  Turn it off and the guest's writes are accepted and discarded,
#   which keeps every disk exactly as it is.  Note that re-downloading a disk
#   the guest scrambled means deleting your copy first: the download never
#   overwrites a file already in the images folder.
# open_screen_after_restart: NOT a setting -- a one-shot marker the desktop UI
#   leaves for itself.  Turning the web server on from the VDM / Dazzler button
#   restarts the gateway, and this is how the window that comes back knows to
#   finish opening the screen you asked for.  It is cleared as soon as it is
#   read.  Setting it by hand just opens a browser once on the next start.
");
    write_kv(&mut content, "cpm_emu_enabled", cfg.cpm_emu_enabled);
    write_kv(&mut content, "cpm_screen_input", cfg.cpm_screen_input);
    write_kv(&mut content, "cpm_boot_writable", cfg.cpm_boot_writable);
    write_kv(&mut content, "open_screen_after_restart", cfg.open_screen_after_restart);
    write_kv(&mut content, "cpm_emu_max_minstr", cfg.cpm_emu_max_minstr);
    write_kv(&mut content, "cpm_emu_uart", &cfg.cpm_emu_uart);
    content.push_str("\
# cpm_mounts: disk images mounted on CP/M drives, as A=name.dsk,C=other.dsk.
#   A mounted drive reads and writes the CP/M filesystem inside the image
#   instead of its folder under CPM/; the folder's files are untouched and come
#   back when it is unmounted.  Names are bare filenames in CPM/images.  An
#   image needs no format prefix: no two formats here are the same size, so the
#   size names the format and the whole CP/M directory is then checked for
#   consistency.  If it checks out the image mounts READ-WRITE; if it does not
#   it mounts read-only and says why, because a file of the right size that is
#   not this filesystem (a UCSD p-System or Cromemco CDOS disk is also 256,256
#   bytes) is the one mistake no later check could catch.  A prefix (see
#   CPM/images/readme.txt) overrides the inspection.
");
    write_kv(&mut content, "cpm_mounts", &cfg.cpm_mounts);
    content.push_str("\
# cpm_boot_image: what the CP/M menu item runs.  Empty (default) = the CP/M
#   emulator: our BDOS, drives A:-P: under CPM/, EGT8080 and the virtual modem.
#   A bare filename in CPM/images instead COLD-BOOTS that disk on whichever
#   emulated MITS controller its size names - an 88-DCDD floppy board or an
#   88-HDSK hard disk - and the disk's own operating system takes the whole
#   machine.  Altair CP/M 2.2 and 3.0, Altair DOS, Disk Extended BASIC, Time
#   Sharing BASIC and Hard Disk BASIC all boot this way.  Booting is not mounting: inside a booted
#   disk there is no jail, no A> from us and no EGT8080, because the guest is
#   talking to hardware rather than to our BDOS.  Your MOUNTED images do come
#   along, each on the controller SLOT its drive letter names (B: is slot 1,
#   C: slot 2), with the booted disk always slot 0 - but what a slot IS belongs
#   to the board: a drive on the floppy controllers, a PLATTER on the 88-HDSK,
#   which carries four to a drive.  The guest names them itself and reaches
#   only as many as its own BIOS knows - four drives for stock Altair CP/M, and
#   for the 88-HDSK CP/M the fixed platter as its B:.  Disks are opened
#   WRITABLE unless cpm_boot_writable is turned off, and that answer covers
#   the mounted disks as well as the booted one.
#   A name that is no longer in CPM/images runs the EMULATOR instead and says
#   so in the log: this is a preference about which machine to run, so deleting
#   an image costs you the boot and not the whole feature.  The disk screens
#   follow what will really start, naming drives A:-P: again rather than the
#   slots of a board nobody is going to get.
");
    write_kv(&mut content, "cpm_boot_image", &cfg.cpm_boot_image);
    content.push_str("\
# cpm_boot_machine: which machine a BOOTED disk (above) thinks it is running on
#   - specifically, where it finds its console.  Ignored by the CP/M emulator,
#   which has no console to place because it services BDOS calls instead.
#     auto              DEFAULT - work it out from the disk.  A boot loader has
#                       to drive its own controller's registers, so the image
#                       says which; when it does not say plainly the Altair
#                       default stands, and the boot screen tells you which
#                       happened.  Never names a machine a disk does not work on.
#     altair_2sio       Altair 88-2SIO at 0x10/0x11 (what `auto` falls back to;
#                       every Altair disk boots because its console is here)
#     altair_sio        Altair 88-SIO at 0x00/0x01, active-low status
#     console_04        console at 0x04/0x05, ready when the bit is CLEAR
#     console_04_cuter  as above, but the guest prints by CALLing a Processor
#                       Technology CUTER ROM, which we synthesise at 0xC019
#   A disk that loads its operating system and then goes quiet is usually
#   looking at a console that is not there, not misreading the disk - it will
#   sit polling a keyboard port for ever.  `auto` reads a DECLARATION rather
#   than guessing: the ports the disk's own boot code drives.  It deliberately
#   does not try to pick a console for MITS disks, because those choose theirs
#   from the front-panel sense switches at run time and their BIOS carries
#   drivers for consoles they never use.
");
    write_kv(&mut content, "cpm_boot_machine", &cfg.cpm_boot_machine);
    content.push_str("\
# cpm_boot_backspace: what a BOOTED disk (above) is handed when you press
#   Backspace.  Ignored by the CP/M emulator, which reads its own console line
#   and already accepts either.  This is the ruling, not a default: the boot
#   from whatever is set here, so this is the default rather than the ruling.
#     backspace  DEFAULT - send BS (0x08), which the disk erases on
#     rubout     send the key as your terminal did (DEL, 0x7F)
#   There is no answer that is right for every disk, and this was measured
#   across two whole disk folders rather than reasoned.  MITS CP/M 2.2, Altair
#   Disk Extended BASIC and Altair Hard Disk BASIC - 24 of the 29 Altair-folder
#   disks that reach a prompt - erase on BS and read DEL as a Teletype RUBOUT,
#   which deletes the character and then PRINTS the character it deleted: type
#   TESTING, backspace over it, and the screen reads TESTINGGNIT.  CP/M 1.3,
#   1.4 and the 1975 build are the opposite - the rubout is their editing key
#   and BS prints a literal ^H - so they are the reason `rubout` exists.
#   Digital Research's own CP/M 2.2, MP/M and UCSD p-System accept either.
");
    write_kv(&mut content, "cpm_boot_backspace", &cfg.cpm_boot_backspace);
    content.push_str("\
# cpm_printer: where CP/M printer output goes.  Reaches BOTH CP/M machines,
#   like cpm_cpu, but by two different routes: in the emulator the printer is
#   an OS service (BDOS function 5 and the BIOS LIST vector), and a booted disk
#   drives a printer PORT itself (see cpm_printer_port).  Either way one
#   document is written into the transfer folder for you to print.
#     off    printer output appears on your terminal, as it always has.
#            Nothing is written to disk -- and nothing can be recovered
#            afterwards either, which is why this is no longer the default.
#     odt    an OpenDocument text file (.odt), monospaced, one page per form
#            feed - opens in LibreOffice, Word or Google Docs.  Overstrike
#            becomes real bold and underline.
#     text   DEFAULT - plain text (.txt), form feeds kept.  Nothing is written
#            until a guest actually prints.
#   The file is named PRINT-YYYYMMDD-HHMMSS from this machine's clock and lands
#   in a `printer' folder inside the transfer directory - its own folder so a
#   printer left on does not scatter documents through your files, and NOT on a
#   CP/M drive: it is for you, not for the guest, and the file-transfer menu
#   reaches it by changing directory into `printer'.
#   A job is finished after 5 SECONDS with nothing printed - CP/M has no
#   end-of-print signal, so silence is the only one there is - and in the
#   emulator also when the program returns to the A> prompt, which is exact.
#   Bold and underline survive into an .odt.  Period software does not ask for
#   them, it OVERSTRIKES - WordStar prints the line, sends a bare CR and
#   reprints just the emphasised run at the same columns - and that is turned
#   into real styling.  Measured against WordStar 3.0.  Which is also why
#   cpm_printer_autolf matters: with the switch on, an overstrike pass lands on
#   a line of its own instead of on top of the text.
");
    write_kv(&mut content, "cpm_printer", &cfg.cpm_printer);
    content.push_str("\
# cpm_printer_port: which printer board a BOOTED disk finds.  Ignored by the
#   emulator, whose printer is a BDOS service with no port at all, and ignored
#   entirely when cpm_printer = off.
#     altair_c  DEFAULT - Altair line printer, data register 03h
#     off       a booted disk has no printer
#   Measured, not reasoned: Altair Hard Disk BASIC answering LINEPRINTER? C
#   initialises with OUT 03h<-11h / OUT 02h<-00h and then sends one 7-bit ASCII
#   character per byte to 03h, ending each line with a bare CR.  The status
#   register is not emulated because it does not need to be - an unclaimed port
#   reads 0xFF here, and every period convention reads a high bit as ready.
");
    write_kv(&mut content, "cpm_printer_port", &cfg.cpm_printer_port);
    content.push_str("\
# cpm_printer_autolf: does a bare carriage return advance the paper?
#   The DIP switch a real printer interface carried, and it carried one because
#   the byte stream cannot say.  Both meanings are in use by period software on
#   the SAME Altair line printer, and both were measured here:
#     - Altair Hard Disk BASIC's LPRINT sends ALPHA<CR>BETA<CR> and no line feed
#       at all, so a bare CR is its line ending.  With the switch off the whole
#       report prints on one line.
#     - WordStar 3.0 (installed for a `Teletype-like printer') emphasises by
#       OVERSTRIKING: it prints the line, sends a bare CR, and reprints just the
#       bold run at the same columns.  With the switch on, every emphasised
#       fragment lands on a line of its own instead of on top of the text.
#     auto   DEFAULT - whatever was measured for the printer in question: on for
#            the booted disk's Altair line printer, off for the emulator's LST:
#            service (CP/M sends CR LF, so overstrike is meaningful there)
#     on     a bare CR ends the line          (Altair BASIC, Disk BASIC)
#     off    a bare CR returns and overprints (WordStar, and anything that
#            emphasises the way a daisy-wheel printer did)
");
    write_kv(&mut content, "cpm_printer_autolf", &cfg.cpm_printer_autolf);
    content.push_str("\
# cpm_cpu: which processor BOTH CP/M machines run - the emulator's transient
#   programs and a booted disk's whole operating system.  The only CP/M setting
#   that reaches both.
#     z80   DEFAULT - a Zilog Z80.  A strict superset of the 8080, so it runs
#           every 8080 disk here, and Altairs were very commonly fitted with a
#           Z80 upgrade board.
#     8080  an Intel 8080 - the processor the Altair actually shipped with, and
#           the more literal machine for these disks.
#   THE TERMINAL: this gateway places EGT8080.COM on CP/M drive A:.  It is
#   built to the 8080's instruction set, so it runs on EITHER setting - which
#   is why it is the only one shipped.  Choose the 8080 when you are
#   running period 8080 software - notably diagnostics that identify the CPU
#   from DCR A setting parity rather than overflow, and are therefore RIGHT to
#   fail on a Z80.
");
    write_kv(&mut content, "cpm_cpu", &cfg.cpm_cpu);
    content.push_str("\
# The CP/M virtual modem's saved AT profile, written by AT&W from inside the
# emulator and reloaded on power-up and on ATZ - exactly as the physical ports
# save theirs.  Hand-editing is fine; AT&F ignores all of it and returns the
# modem to factory defaults.  cpm_emu_s_regs is S0..S27 comma-separated, and
# empty means the power-on values.
");
    write_kv(&mut content, "cpm_emu_echo", cfg.cpm_emu_modem.echo);
    write_kv(&mut content, "cpm_emu_verbose", cfg.cpm_emu_modem.verbose);
    write_kv(&mut content, "cpm_emu_quiet", cfg.cpm_emu_modem.quiet);
    write_kv(&mut content, "cpm_emu_x_code", cfg.cpm_emu_modem.x_code);
    write_kv(&mut content, "cpm_emu_dcd_mode", cfg.cpm_emu_modem.dcd_mode);
    write_kv(&mut content, "cpm_emu_s_regs", &cfg.cpm_emu_modem.s_regs);
    content.push('\n');

    content.push_str("\
# Serial ports.  The gateway exposes two physically independent ports —
# Port A and Port B — each with its own enabled flag, role (modem
# emulator, telnet-serial console, or Kermit server), serial parameters,
# and persisted AT/S-register state.
#
# <port>_enabled = true activates that port.  <port>_mode selects its role:
#   modem    — run the Hayes AT command emulator
#   console  — expose the port via the telnet menu's Serial Gateway,
#              bridging the telnet client directly to the wire.
#   kermit   — run an always-on Kermit server directly on the wire
#              (no AT layer; bypasses auth — trusted lines only).
#
# Legacy single-port configs (using bare `serial_*` keys) auto-migrate
# into Port A on first read; this writer always emits the dual-port form.
");
    write_serial_port_section(&mut content, "Serial Port A", "serial_a", &cfg.serial_a);
    write_serial_port_section(&mut content, "Serial Port B", "serial_b", &cfg.serial_b);

    content.push_str("\
# SSH server interface (encrypted access to the gateway).  Set
# ssh_enabled = true to activate.  Authenticates against the unified
# `username` / `password` above — telnet, SSH, and the web UI share
# one credential pair.
");
    write_kv(&mut content, "ssh_enabled", cfg.ssh_enabled);
    content.push('\n');

    content.push_str("# SSH server port\n");
    write_kv(&mut content, "ssh_port", cfg.ssh_port);
    content.push('\n');

    content.push_str("\
# Authentication mode for the OUTBOUND SSH Gateway (the menu item that
# proxies to a remote SSH server).  Values:
#   key      — use the gateway's built-in Ed25519 client key.  Copy the
#              public half (shown in the GUI Server > More popup, or
#              extract with `ssh-keygen -y -f ethernet_gateway_ssh_key`)
#              into the remote's ~/.ssh/authorized_keys first.
#   password — prompt the operator for the remote account's password on
#              each connect.  No key is offered.
");
    write_kv_str(&mut content, "ssh_gateway_auth", &cfg.ssh_gateway_auth);
    content.push('\n');

    content.push_str("\
# ── Master/Slave serial extender (relay) ───────────────────────
# gateway_role: standalone (default) | master | slave.  Standalone is
# today's behavior — the relay feature is entirely inert.  Roles are
# mutually exclusive.
");
    write_kv_str(&mut content, "gateway_role", &cfg.gateway_role);
    content.push('\n');

    content.push_str("\
# master_accept_relays: a MASTER only accepts relay channels from slaves
# when this is on.  Off by default so accepting relays is never implied
# by enabling SSH for normal logins.
");
    write_kv(&mut content, "master_accept_relays", cfg.master_accept_relays);
    content.push_str("\
# allow_relay_kermit: a MASTER serves its own Kermit server to a slave port
#   that is in Kermit-server mode, so the device on the slave's wire lists,
#   uploads to and downloads from THIS machine's transfer directory.  Off by
#   default: Kermit server mode has no authentication of its own (same reason
#   allow_atdt_kermit and kermit_server_enabled are opt-in).
");
    write_kv(&mut content, "allow_relay_kermit", cfg.allow_relay_kermit);
    content.push('\n');

    content.push_str("\
# SLAVE settings — where to reach the master and the credentials to log
# in with (must match the master's unified username/password).  Ignored
# unless gateway_role = slave.
");
    write_kv_str(&mut content, "slave_master_host", &cfg.slave_master_host);
    write_kv(&mut content, "slave_master_port", cfg.slave_master_port);
    write_kv_str(&mut content, "slave_master_username", &cfg.slave_master_username);
    write_kv_str(&mut content, "slave_master_password", &cfg.slave_master_password);
    content.push('\n');

    // The generated config is a documentation surface, and this line offered a
    // choice that does nothing: `raw` parses and stores, but no raw transport
    // exists, so the relay still rides SSH.  Setting it warned only in the
    // startup log — which an operator editing this file by hand has no reason
    // to be reading.  No UI exposes the key at all (deliberately: see the
    // web-form key list), so this comment is the only place a hand-editor
    // learns.
    content.push_str(
        "# relay_transport: ssh (default, recommended) | raw\n\
         #   raw is NOT yet implemented -- setting it changes nothing and the\n\
         #   relay still uses SSH.  No configuration screen offers this key;\n\
         #   it is settable only here.\n",
    );
    write_kv_str(&mut content, "relay_transport", &cfg.relay_transport);

    // Write to a per-PID + per-thread tmp file with owner-only mode
    // from the moment of creation, then rename into place.  On Unix
    // we open with `O_CREAT|O_EXCL` and mode 0o600 in one syscall so
    // the file is never visible at default-umask permissions — the
    // config holds plaintext credentials (telnet password, SSH
    // password, Groq API key) and a post-write `chmod` would leave
    // a brief 0o644 window on a shared host.  Windows users on
    // multi-user systems should place the binary in a per-user
    // folder to get equivalent NTFS ACL protection.
    //
    // The PID suffix prevents two instances in the same working
    // directory from clobbering each other's tmp; the per-process
    // atomic counter prevents two threads in the same process from
    // doing the same (e.g. a SIGHUP-driven reload firing concurrently
    // with a GUI-driven save).
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::SeqCst);
    let tmp = format!("{}.{}.{}.tmp", path, std::process::id(), seq);

    // fsync the tmp file's contents to disk *before* the rename.  A rename
    // is only atomic with respect to the directory entry, not the data
    // blocks: on a crash or power loss between write and rename the entry
    // can point at a zero-length or partially-written file.  That truncated
    // config then fails to parse and — with the M-12 guard — halts startup,
    // or (pre-guard) reset the gateway to insecure defaults.  fsync closes
    // that window so a rename never publishes unflushed bytes.
    #[cfg(unix)]
    let write_result = {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .and_then(|mut f| f.write_all(content.as_bytes()).and_then(|()| f.sync_all()))
            .and_then(|()| std::fs::rename(&tmp, path))
    };
    #[cfg(not(unix))]
    let write_result = std::fs::File::create(&tmp)
        .and_then(|mut f| {
            use std::io::Write;
            f.write_all(content.as_bytes()).and_then(|()| f.sync_all())
        })
        .and_then(|()| std::fs::rename(&tmp, path));

    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("could not write {}: {}", path, e));
    }
    Ok(())
}

/// Update a single key in the config file and the in-memory singleton.
/// Reads the current file, updates the key, writes it back, and refreshes
/// the global config so that subsequent `get_config()` calls see the change.
pub fn update_config_value(key: &str, value: &str) {
    update_config_values(&[(key, value)]);
}

/// Update multiple keys in a single read-modify-write cycle.
/// Holds the global CONFIG lock for the entire operation to prevent
/// concurrent callers from overwriting each other's changes.
pub fn update_config_values(pairs: &[(&str, &str)]) {
    let mut guard = CONFIG.lock().unwrap_or_else(|e| e.into_inner());
    let path = config_file_path();
    let mut cfg = if Path::new(&path).exists() {
        match read_config_file_checked(&path) {
            Ok(c) => c,
            // Unreadable mid-run: keep the current in-memory config rather
            // than clobbering the on-disk file with defaults in the write
            // below (same insecure-downgrade hazard as at startup).
            Err(e) => {
                glog!(
                    "Warning: could not read {} ({}); keeping current settings.",
                    CONFIG_FILE, e
                );
                guard.as_ref().cloned().unwrap_or_default()
            }
        }
    } else {
        Config::default()
    };
    let was_cpm_enabled = cfg.cpm_emu_enabled;
    for &(key, value) in pairs {
        apply_config_key(&mut cfg, key, value);
    }
    // Turning the emulator on lays out its folders straight away, so an
    // operator can put software in CPM/A and a disk image in CPM/images
    // without first having to start a session.  Only on the transition: doing
    // it on every save would be a filesystem sweep per settings change.
    if cfg.cpm_emu_enabled && !was_cpm_enabled {
        ensure_cpm_layout(&cfg);
    }
    // Turning the emulator off releases every mounted image, so the files are
    // not held open by a feature nobody can reach any more.  `cpm_mounts` is
    // left alone: turning it back on should restore what was mounted.
    if was_cpm_enabled && !cfg.cpm_emu_enabled {
        crate::cpm::image::registry::clear_all();
    }
    // High-frequency runtime persistence (setting toggles, AT&W, etc.):
    // best-effort with a logged warning rather than propagating to every
    // call site.  The explicit save_config path returns the error instead.
    if let Err(e) = write_config_file(&path, &cfg) {
        glog!("Warning: {}", e);
    }
    *guard = Some(cfg);
}

/// Apply one per-port key/value pair to a `SerialPortConfig`.  `suffix`
/// is the part of the key after the `serial_a_` / `serial_b_` prefix
/// (e.g. `"baud"`, `"stored_2"`).  Validation rules mirror
/// `read_serial_port_config` so both code paths accept exactly the same
/// set of values.
fn apply_serial_port_key(port: &mut SerialPortConfig, suffix: &str, value: &str) {
    match suffix {
        "enabled" => port.enabled = value.eq_ignore_ascii_case("true"),
        "mode" => {
            let lower = value.trim().to_ascii_lowercase();
            if matches!(lower.as_str(), "modem" | "console" | "kermit") {
                port.mode = lower;
            }
        }
        "port" => port.port = value.to_string(),
        "baud" => {
            if let Ok(v) = value.parse::<u32>() && v >= 300 {
                port.baud = v;
            }
        }
        "databits" => {
            if let Ok(v) = value.parse::<u8>() && matches!(v, 5..=8) {
                port.databits = v;
            }
        }
        "parity" => {
            let lower = value.trim().to_ascii_lowercase();
            if matches!(lower.as_str(), "none" | "odd" | "even") {
                port.parity = lower;
            }
        }
        "stopbits" => {
            if let Ok(v) = value.parse::<u8>() && (v == 1 || v == 2) {
                port.stopbits = v;
            }
        }
        "flowcontrol" => {
            let lower = value.trim().to_ascii_lowercase();
            if matches!(lower.as_str(), "none" | "hardware" | "software") {
                port.flowcontrol = lower;
            }
        }
        "echo" => port.echo = value.eq_ignore_ascii_case("true"),
        "verbose" => port.verbose = value.eq_ignore_ascii_case("true"),
        "quiet" => port.quiet = value.eq_ignore_ascii_case("true"),
        "s_regs" => port.s_regs = value.to_string(),
        "x_code" => {
            if let Ok(v) = value.parse::<u8>() && v <= 4 {
                port.x_code = v;
            }
        }
        "dtr_mode" => {
            if let Ok(v) = value.parse::<u8>() && v <= 3 {
                port.dtr_mode = v;
            }
        }
        "flow_mode" => {
            if let Ok(v) = value.parse::<u8>() && v <= 4 {
                port.flow_mode = v;
            }
        }
        "dcd_mode" => {
            if let Ok(v) = value.parse::<u8>() && v <= 1 {
                port.dcd_mode = v;
            }
        }
        "stored_0" => port.stored_numbers[0] = value.to_string(),
        "stored_1" => port.stored_numbers[1] = value.to_string(),
        "stored_2" => port.stored_numbers[2] = value.to_string(),
        "stored_3" => port.stored_numbers[3] = value.to_string(),
        "petscii_translate" => port.petscii_translate = value.eq_ignore_ascii_case("true"),
        "drive_carrier" => port.drive_carrier = value.eq_ignore_ascii_case("true"),
        _ => {}
    }
}

/// Construct the per-port config key for `id` and `suffix`, e.g.
/// `serial_key(SerialPortId::A, "baud")` → `"serial_a_baud"`.  Used by
/// runtime persistence paths (modem AT&W, telnet revert) that need to
/// target a specific port's keys.
pub fn serial_key(id: SerialPortId, suffix: &str) -> String {
    format!(
        "serial_{}_{}",
        match id {
            SerialPortId::A => "a",
            SerialPortId::B => "b",
        },
        suffix
    )
}

/// Apply a single key-value pair to a Config struct.
fn apply_config_key(cfg: &mut Config, key: &str, value: &str) {
    match key {
        "telnet_enabled" => cfg.telnet_enabled = value.eq_ignore_ascii_case("true"),
        "telnet_port" => {
            if let Ok(v) = value.parse::<u16>() && v >= 1 {
                cfg.telnet_port = v;
            }
        }
        "telnet_gateway_negotiate" => {
            cfg.telnet_gateway_negotiate = value.eq_ignore_ascii_case("true");
        }
        "telnet_gateway_raw" => {
            cfg.telnet_gateway_raw = value.eq_ignore_ascii_case("true");
        }
        "gateway_debug" => cfg.gateway_debug = value.eq_ignore_ascii_case("true"),
        // Both accept 0 deliberately — 0 is "auto", not an invalid width, so
        // these must not carry the `v >= 1` guard the port keys use.
        "gateway_term_width" => {
            if let Ok(v) = value.parse::<u16>() {
                cfg.gateway_term_width = v;
            }
        }
        "gateway_term_height" => {
            if let Ok(v) = value.parse::<u16>() {
                cfg.gateway_term_height = v;
            }
        }
        "enable_console" => cfg.enable_console = value.eq_ignore_ascii_case("true"),
        "setup_wizard_completed" => {
            cfg.setup_wizard_completed = value.eq_ignore_ascii_case("true")
        }
        "security_enabled" => cfg.security_enabled = value.eq_ignore_ascii_case("true"),
        "disable_ip_safety" => cfg.disable_ip_safety = value.eq_ignore_ascii_case("true"),
        "username" => cfg.username = value.to_string(),
        "password" => cfg.password = value.to_string(),
        "transfer_dir" => cfg.transfer_dir = value.to_string(),
        "place_bundled_terminals" => {
            cfg.place_bundled_terminals = value.eq_ignore_ascii_case("true")
        }
        "gui_window_geometry" => cfg.gui_window_geometry = value.trim().to_string(),
        "gui_zoom" => cfg.gui_zoom = value.trim().to_string(),
        "max_sessions" => {
            if let Ok(v) = value.parse::<usize>() && v >= 1 {
                cfg.max_sessions = v;
            }
        }
        "idle_timeout_secs" => {
            if let Ok(v) = value.parse() {
                cfg.idle_timeout_secs = v;
            }
        }
        "groq_api_key" => cfg.groq_api_key = value.to_string(),
        "browser_homepage" => cfg.browser_homepage = value.to_string(),
        // Accept the legacy key name too so a live update via the old name
        // still lands (mirrors the reader's fallback).
        "weather_location" | "weather_zip" => cfg.weather_location = value.to_string(),
        "weather_units" => {
            let v = value.trim().to_ascii_lowercase();
            if matches!(v.as_str(), "auto" | "us" | "metric") {
                cfg.weather_units = v;
            }
        }
        "verbose" => cfg.verbose = value.eq_ignore_ascii_case("true"),
        "log_to_file" => cfg.log_to_file = value.eq_ignore_ascii_case("true"),
        // Trimmed to match `logger::file_policy_from`, which trims before using
        // the path; an empty value is a valid "switch file logging off".
        "log_file" => cfg.log_file = value.trim().to_string(),
        // No `>= 1` floor on either limit — unlike most numeric keys here, `0`
        // is meaningful for both: no size rotation, and keep no rotated
        // generations.  See `logger::should_rotate` / `logger::rotate`.
        "log_max_size_kb" => {
            if let Ok(v) = value.parse::<u64>() {
                cfg.log_max_size_kb = v;
            }
        }
        "log_max_files" => {
            if let Ok(v) = value.parse::<u32>() {
                cfg.log_max_files = v;
            }
        }
        "xmodem_negotiation_timeout" => {
            if let Ok(v) = value.parse::<u64>() && v >= 1 {
                cfg.xmodem_negotiation_timeout = v;
            }
        }
        "xmodem_block_timeout" => {
            if let Ok(v) = value.parse::<u64>() && v >= 1 {
                cfg.xmodem_block_timeout = v;
            }
        }
        "xmodem_max_retries" => {
            if let Ok(v) = value.parse::<usize>() && v >= 1 {
                cfg.xmodem_max_retries = v;
            }
        }
        "xmodem_negotiation_retry_interval" => {
            if let Ok(v) = value.parse::<u64>() && v >= 1 {
                cfg.xmodem_negotiation_retry_interval = v;
            }
        }
        "zmodem_negotiation_timeout" => {
            if let Ok(v) = value.parse::<u64>() && v >= 1 {
                cfg.zmodem_negotiation_timeout = v;
            }
        }
        "zmodem_frame_timeout" => {
            if let Ok(v) = value.parse::<u64>() && v >= 1 {
                cfg.zmodem_frame_timeout = v;
            }
        }
        "zmodem_max_retries" => {
            if let Ok(v) = value.parse::<u32>() && v >= 1 {
                cfg.zmodem_max_retries = v;
            }
        }
        "zmodem_negotiation_retry_interval" => {
            if let Ok(v) = value.parse::<u64>() && v >= 1 {
                cfg.zmodem_negotiation_retry_interval = v;
            }
        }
        "kermit_negotiation_timeout" => {
            if let Ok(v) = value.parse::<u64>() && v >= 1 {
                cfg.kermit_negotiation_timeout = v;
            }
        }
        "kermit_packet_timeout" => {
            if let Ok(v) = value.parse::<u64>() && v >= 1 {
                cfg.kermit_packet_timeout = v;
            }
        }
        "kermit_idle_timeout" => {
            // No `>= 1` floor — `0` is the explicit "disable" sentinel
            // matching the loader's filter at `read_config_file`.
            if let Ok(v) = value.parse::<u64>() {
                cfg.kermit_idle_timeout = v;
            }
        }
        "kermit_max_retries" => {
            if let Ok(v) = value.parse::<u32>() && v >= 1 {
                cfg.kermit_max_retries = v;
            }
        }
        "kermit_max_packet_length" => {
            if let Ok(v) = value.parse::<u16>() && (10..=9024).contains(&v) {
                cfg.kermit_max_packet_length = v;
            }
        }
        "kermit_window_size" => {
            if let Ok(v) = value.parse::<u8>() && (1..=31).contains(&v) {
                cfg.kermit_window_size = v;
            }
        }
        "kermit_block_check_type" => {
            if let Ok(v) = value.parse::<u8>() && matches!(v, 1..=3) {
                cfg.kermit_block_check_type = v;
            }
        }
        "kermit_long_packets" => {
            cfg.kermit_long_packets = value.eq_ignore_ascii_case("true");
        }
        "kermit_sliding_windows" => {
            cfg.kermit_sliding_windows = value.eq_ignore_ascii_case("true");
        }
        "kermit_streaming" => {
            cfg.kermit_streaming = value.eq_ignore_ascii_case("true");
        }
        "kermit_attribute_packets" => {
            cfg.kermit_attribute_packets = value.eq_ignore_ascii_case("true");
        }
        "kermit_repeat_compression" => {
            cfg.kermit_repeat_compression = value.eq_ignore_ascii_case("true");
        }
        "kermit_8bit_quote" => {
            let lower = value.trim().to_ascii_lowercase();
            if matches!(lower.as_str(), "auto" | "on" | "off") {
                cfg.kermit_8bit_quote = lower;
            }
        }
        "kermit_resume_partial" => {
            cfg.kermit_resume_partial = value.eq_ignore_ascii_case("true");
        }
        "kermit_resume_max_age_hours" => {
            if let Ok(v) = value.parse::<u32>() && v >= 1 {
                cfg.kermit_resume_max_age_hours = v;
            }
        }
        "kermit_locking_shifts" => {
            cfg.kermit_locking_shifts = value.eq_ignore_ascii_case("true");
        }
        "kermit_wait_for_receiver" => {
            cfg.kermit_wait_for_receiver = value.eq_ignore_ascii_case("true");
        }
        "allow_peer_dial" => {
            cfg.allow_peer_dial = value.eq_ignore_ascii_case("true");
        }
        "allow_atdt_kermit" => {
            cfg.allow_atdt_kermit = value.eq_ignore_ascii_case("true");
        }
        "kermit_server_enabled" => {
            cfg.kermit_server_enabled = value.eq_ignore_ascii_case("true");
        }
        "kermit_server_port" => {
            if let Ok(v) = value.parse::<u16>() && v >= 1 {
                cfg.kermit_server_port = v;
            }
        }
        "punter_block_size" => {
            if let Ok(v) = value.parse::<u16>() && (8..=255).contains(&v) {
                cfg.punter_block_size = v;
            }
        }
        "punter_negotiation_timeout" => {
            if let Ok(v) = value.parse::<u64>() && v >= 1 {
                cfg.punter_negotiation_timeout = v;
            }
        }
        "punter_block_timeout" => {
            if let Ok(v) = value.parse::<u64>() && v >= 1 {
                cfg.punter_block_timeout = v;
            }
        }
        "punter_max_retries" => {
            if let Ok(v) = value.parse::<u32>() && v >= 1 {
                cfg.punter_max_retries = v;
            }
        }
        "punter_max_bad_rounds" => {
            if let Ok(v) = value.parse::<u32>() && v >= 1 {
                cfg.punter_max_bad_rounds = v;
            }
        }
        "punter_negotiation_retry_interval" => {
            if let Ok(v) = value.parse::<u64>() && v >= 1 {
                cfg.punter_negotiation_retry_interval = v;
            }
        }
        "punter_hangup_on_failure" => {
            cfg.punter_hangup_on_failure = value.eq_ignore_ascii_case("true");
        }
        "web_enabled" => cfg.web_enabled = value.eq_ignore_ascii_case("true"),
        "cpm_screen_input" => cfg.cpm_screen_input = value.eq_ignore_ascii_case("true"),
        "cpm_boot_writable" => cfg.cpm_boot_writable = value.eq_ignore_ascii_case("true"),
        "open_screen_after_restart" => {
            cfg.open_screen_after_restart = value.eq_ignore_ascii_case("true")
        }
        "cpm_emu_enabled" => cfg.cpm_emu_enabled = value.eq_ignore_ascii_case("true"),
        "cpm_mounts" => cfg.cpm_mounts = value.to_string(),
        "cpm_boot_image" => {
            // Validated here as well as at the point of use.  The value can
            // arrive from a web form, so it is shaped by whoever posted it, and
            // a name that could never be opened has no business being written
            // into the config file at all.  Empty is always allowed: that is
            // "run the emulator".
            if value.is_empty() || crate::cpm::image::is_safe_image_name(value) {
                cfg.cpm_boot_image = value.to_string();
            }
        }
        "cpm_boot_machine" => {
            // Only a machine we actually are.  An unrecognised value would
            // resolve to the default console at run time anyway, but refusing it
            // here keeps the config file honest about what the gateway is doing
            // — the alternative is a file that says one machine and a gateway
            // that is another.
            if crate::cpm::console::is_valid_machine_key(value) {
                cfg.cpm_boot_machine = value.to_string();
            }
        }
        "cpm_boot_backspace" => {
            // Only one of the two offered values, for the same reason as the
            // machine above: a web form or a hand edit could put anything here,
            // and `backspace_erases` treats everything it does not recognise as
            // the default — so an unchecked write would leave the file claiming
            // a setting the gateway is not honouring.
            if crate::cpm::boot::BACKSPACE_CHOICES.iter().any(|(v, _)| *v == value) {
                cfg.cpm_boot_backspace = value.to_string();
            }
        }
        "cpm_printer" => {
            // Only one of the three offered values.  Same reason as the keys
            // above, with one extra: `format_for` reads anything it does not
            // recognise as OFF, so an unchecked write could leave the file
            // promising documents that were never going to be written.
            if crate::cpm::printer::PRINTER_CHOICES.iter().any(|(v, _)| *v == value) {
                cfg.cpm_printer = value.to_string();
            }
        }
        "cpm_printer_autolf" => {
            // One of the three, for the same reason as the two below:
            // `auto_lf_for` reads anything it does not recognise as `auto`, so
            // an unchecked write could leave the file promising a switch
            // position the printer was never going to be in.
            if crate::cpm::printer::AUTOLF_CHOICES.iter().any(|(v, _)| *v == value) {
                cfg.cpm_printer_autolf = value.to_string();
            }
        }
        "cpm_printer_port" => {
            // `off` plus the board keys.  A wrong port here would capture
            // bytes meant for another device, so an unrecognised value is
            // refused rather than defaulted.
            if value == crate::cpm::printer::PORT_OFF
                || crate::cpm::printer::PORT_CHOICES.iter().any(|p| p.key == value)
            {
                cfg.cpm_printer_port = value.to_string();
            }
        }
        "cpm_cpu" => {
            // Only one of the two processors we have, for the same reason as
            // the two above: `is_8080` reads anything it does not recognise as
            // the Z80, so an unchecked write would leave the file claiming an
            // 8080 while both machines ran a Z80.
            if crate::cpm::cpu::CPU_CHOICES.iter().any(|(v, _)| *v == value) {
                cfg.cpm_cpu = value.to_string();
            }
        }
        "disable_gateway_connections" => {
            cfg.disable_gateway_connections = value.eq_ignore_ascii_case("true")
        }
        "cpm_emu_max_minstr" => {
            // Clamped, not rejected — the same rule the file loader follows, so
            // typing a huge number into any of the three UIs lands on the cap
            // rather than on the default.  See [`MAX_CPM_EMU_MAX_MINSTR`].
            if let Ok(v) = value.parse::<u32>() && v >= 1 {
                cfg.cpm_emu_max_minstr = v.min(MAX_CPM_EMU_MAX_MINSTR);
            }
        }
        "cpm_emu_uart" => {
            if crate::cpm::uart::is_valid_uart_key(value) {
                cfg.cpm_emu_uart = value.to_string();
            }
        }
        "cpm_emu_echo" => cfg.cpm_emu_modem.echo = value.eq_ignore_ascii_case("true"),
        "cpm_emu_verbose" => cfg.cpm_emu_modem.verbose = value.eq_ignore_ascii_case("true"),
        "cpm_emu_quiet" => cfg.cpm_emu_modem.quiet = value.eq_ignore_ascii_case("true"),
        "cpm_emu_x_code" => {
            if let Ok(v) = value.parse::<u8>() && v <= 4 {
                cfg.cpm_emu_modem.x_code = v;
            }
        }
        "cpm_emu_dcd_mode" => {
            if let Ok(v) = value.parse::<u8>() && v <= 1 {
                cfg.cpm_emu_modem.dcd_mode = v;
            }
        }
        "cpm_emu_s_regs" => cfg.cpm_emu_modem.s_regs = value.to_string(),
        "web_port" => {
            if let Ok(v) = value.parse::<u16>() && v >= 1 {
                cfg.web_port = v;
            }
        }
        // Per-port keys.  Both `serial_a_*` and `serial_b_*` prefixes
        // are recognized; the helper below dispatches on the prefix and
        // applies the shared validation rules to whichever port-config
        // slice is selected.  Anything else falls through to the
        // unrecognized-key arm.
        k if k.starts_with("serial_a_") => {
            apply_serial_port_key(&mut cfg.serial_a, &k["serial_a_".len()..], value);
        }
        k if k.starts_with("serial_b_") => {
            apply_serial_port_key(&mut cfg.serial_b, &k["serial_b_".len()..], value);
        }
        "ssh_enabled" => cfg.ssh_enabled = value.eq_ignore_ascii_case("true"),
        "ssh_port" => {
            if let Ok(v) = value.parse::<u16>() && v >= 1 {
                cfg.ssh_port = v;
            }
        }
        "ssh_gateway_auth" => {
            let lower = value.trim().to_ascii_lowercase();
            if matches!(lower.as_str(), "key" | "password") {
                cfg.ssh_gateway_auth = lower;
            }
        }
        "gateway_role" => {
            let lower = value.trim().to_ascii_lowercase();
            if matches!(lower.as_str(), "standalone" | "master" | "slave") {
                cfg.gateway_role = lower;
            }
        }
        "allow_relay_kermit" => cfg.allow_relay_kermit = value.eq_ignore_ascii_case("true"),
        "master_accept_relays" => {
            cfg.master_accept_relays = value.eq_ignore_ascii_case("true")
        }
        "slave_master_host" => cfg.slave_master_host = value.trim().to_string(),
        "slave_master_port" => {
            if let Ok(v) = value.parse::<u16>() && v >= 1 {
                cfg.slave_master_port = v;
            }
        }
        "slave_master_username" => cfg.slave_master_username = value.to_string(),
        "slave_master_password" => cfg.slave_master_password = value.to_string(),
        "relay_transport" => {
            let lower = value.trim().to_ascii_lowercase();
            if matches!(lower.as_str(), "ssh" | "raw") {
                cfg.relay_transport = lower;
            }
        }
        _ => {}
    }
}

// ─── Dialup mapping (dialup.conf) ─────────────────────────

/// Name of the dialup mapping file (lives next to the binary).
pub const DIALUP_FILE: &str = "dialup.conf";

/// A single dialup mapping: phone number → host:port.
#[derive(Debug, Clone, PartialEq)]
pub struct DialupEntry {
    pub number: String,
    pub host: String,
    pub port: u16,
}

/// Load all dialup mappings from `dialup.conf`.
/// If the file does not exist, creates it with a default starter entry.
pub fn load_dialup_mappings() -> Vec<DialupEntry> {
    try_load_dialup_mappings().unwrap_or_default()
}

/// Load the dialup mappings, surfacing a read failure instead of hiding it as
/// an empty map.
///
/// The distinction matters to any caller that *writes the file back*.
/// [`save_dialup_mappings`] rewrites it wholesale, so a read-modify-write
/// built on a silently-empty list replaces every existing mapping with
/// whatever the caller just added — the operator adds one number and loses the
/// rest, with no error anywhere. Read-only callers (lookup, the listing
/// screen) are content to degrade to "no mappings", and keep using
/// [`load_dialup_mappings`]; mutating callers must use this and refuse.
///
/// A *missing* file is not a failure: that is first run, and it seeds the
/// default mapping exactly as before.
pub fn try_load_dialup_mappings() -> Result<Vec<DialupEntry>, std::io::Error> {
    if !Path::new(DIALUP_FILE).exists() {
        let defaults = vec![DialupEntry {
            number: "1234567".into(),
            host: "telnetbible.com".into(),
            port: 6400,
        }];
        save_dialup_mappings(&defaults);
        glog!("Created default dialup mapping: {}", DIALUP_FILE);
        return Ok(defaults);
    }
    dialup_entries_from_read(std::fs::read_to_string(DIALUP_FILE))
}

/// Decision half of [`try_load_dialup_mappings`], split out so the failure
/// branches are testable — `DIALUP_FILE` is a fixed process-relative path, so
/// a test that made the real file unreadable would race every other test.
fn dialup_entries_from_read(
    read: Result<String, std::io::Error>,
) -> Result<Vec<DialupEntry>, std::io::Error> {
    match read {
        Ok(content) => Ok(parse_dialup_mappings(&content)),
        // Deleted between the exists() check and the open: genuinely no
        // mappings, not a file we failed to read.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

/// Parse dialup mappings from file content.
fn parse_dialup_mappings(content: &str) -> Vec<DialupEntry> {
    let mut entries = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some((number, target)) = trimmed.split_once('=') {
            let number = number.trim().to_string();
            let target = target.trim();
            if number.is_empty() || target.is_empty() {
                continue;
            }
            let (host, port) = if let Some((h, p)) = target.rsplit_once(':') {
                match p.parse::<u16>() {
                    Ok(port) if port > 0 => (h.to_string(), port),
                    _ => (target.to_string(), 23),
                }
            } else {
                (target.to_string(), 23)
            };
            entries.push(DialupEntry { number, host, port });
        }
    }
    entries
}

/// Save all dialup mappings to `dialup.conf`.
pub fn save_dialup_mappings(entries: &[DialupEntry]) {
    let mut content = String::from(
        "# Dialup Mapping\n\
         #\n\
         # Map phone numbers to host:port targets.\n\
         # Format: number = host:port\n\
         #\n\
         # Example:\n\
         # 5551234 = bbs.example.com:23\n\
         \n",
    );
    for entry in entries {
        // Sanitize number/host so an embedded newline can't corrupt the
        // line-based framing (matches write_config_file's write_kv_str).
        content.push_str(&format!(
            "{} = {}:{}\n",
            sanitize_value(&entry.number),
            sanitize_value(&entry.host),
            entry.port
        ));
    }
    // Same atomic + owner-only-from-creation pattern as
    // `write_config_file` above.  Setting mode 0o600 *before* rename
    // means the final path is never visible at default-umask
    // permissions; the dialup mapping file reveals host/port pairs
    // the operator has configured, which is a privacy signal other
    // local users shouldn't have.
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::SeqCst);
    let tmp = format!("{}.{}.{}.tmp", DIALUP_FILE, std::process::id(), seq);

    // fsync before rename (see `write_config_file`): a rename only makes the
    // directory entry atomic, not the data blocks, so a crash between write
    // and rename could publish a truncated mapping file.
    #[cfg(unix)]
    let write_result = {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .and_then(|mut f| f.write_all(content.as_bytes()).and_then(|()| f.sync_all()))
            .and_then(|()| std::fs::rename(&tmp, DIALUP_FILE))
    };
    #[cfg(not(unix))]
    let write_result = std::fs::File::create(&tmp)
        .and_then(|mut f| {
            use std::io::Write;
            f.write_all(content.as_bytes()).and_then(|()| f.sync_all())
        })
        .and_then(|()| std::fs::rename(&tmp, DIALUP_FILE));

    if let Err(e) = write_result {
        glog!("Warning: could not write {}: {}", DIALUP_FILE, e);
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Normalize a phone number to digits only for comparison.
/// e.g. "(555) 123-4567" → "5551234567"
pub fn normalize_phone_number(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_digit()).collect()
}

/// Look up a phone number in the dialup mappings.
/// Returns the host:port string if found, or None.
pub fn lookup_dialup_number(number: &str) -> Option<String> {
    let normalized = normalize_phone_number(number);
    if normalized.is_empty() {
        return None;
    }
    let entries = load_dialup_mappings();
    for entry in &entries {
        if normalize_phone_number(&entry.number) == normalized {
            return Some(format!("{}:{}", entry.host, entry.port));
        }
    }
    None
}

// ─── Tests ─────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// **A default may change; somebody else's installation may not.**
    ///
    /// `cpm_printer` became `text` in 0.9.2, and it first shipped in 0.9.1 — so
    /// every config file written by 0.9.0 or earlier lacks the key entirely. If
    /// a missing key resolved to the new default, upgrading would silently start
    /// writing `PRINT-*.txt` into somebody's transfer folder because they
    /// installed a new version. They never asked for a printer, and changing a
    /// default is not consent.
    ///
    /// Same asymmetry, and the same reasoning, as `setup_wizard_completed`.
    #[test]
    fn test_an_upgrade_does_not_switch_the_printer_on() {
        let dir = std::env::temp_dir().join("egw_printer_upgrade_test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("old.conf");

        // A config file from before the key existed.
        std::fs::write(&path, "telnet_port = 2323\ncpm_emu_enabled = true\n").unwrap();
        let upgraded = read_config_file(&path.to_string_lossy());
        assert_eq!(
            upgraded.cpm_printer,
            crate::cpm::printer::PRINTER_OFF,
            "an upgrade must not start writing files unasked"
        );

        // A fresh install, which writes every key, gets the new default.
        assert_eq!(
            Config::default().cpm_printer,
            crate::cpm::printer::DEFAULT_PRINTER,
            "a new install captures printouts rather than losing them"
        );
        assert_ne!(
            Config::default().cpm_printer,
            upgraded.cpm_printer,
            "the whole point is that these two differ"
        );

        // And an explicit `off` is still honoured, which is the other way an
        // operator says no.
        std::fs::write(&path, "cpm_printer = off\n").unwrap();
        assert_eq!(
            read_config_file(&path.to_string_lossy()).cpm_printer,
            crate::cpm::printer::PRINTER_OFF
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_default_config() {
        let cfg = Config::default();
        assert!(cfg.telnet_enabled);
        assert_eq!(cfg.telnet_port, 2323);
        assert!(!cfg.telnet_gateway_negotiate);
        assert!(!cfg.telnet_gateway_raw);
        assert!(cfg.enable_console);
        assert!(!cfg.security_enabled);
        assert_eq!(cfg.username, "admin");
        assert_eq!(cfg.password, "changeme");
        assert_eq!(cfg.transfer_dir, "transfer");
        assert_eq!(cfg.max_sessions, 50);
        assert_eq!(cfg.idle_timeout_secs, 900);
        assert_eq!(cfg.groq_api_key, "");
        assert_eq!(cfg.browser_homepage, "http://telnetbible.com");
        assert_eq!(cfg.weather_location, "");
        assert_eq!(cfg.weather_units, "auto");
        assert!(!cfg.verbose);
        assert_eq!(cfg.xmodem_negotiation_timeout, 45);
        assert_eq!(cfg.xmodem_block_timeout, 20);
        assert_eq!(cfg.xmodem_max_retries, 10);
        assert_eq!(cfg.xmodem_negotiation_retry_interval, 7);
        assert_eq!(cfg.zmodem_negotiation_timeout, 45);
        assert_eq!(cfg.zmodem_frame_timeout, 30);
        assert_eq!(cfg.zmodem_max_retries, 10);
        assert_eq!(cfg.zmodem_negotiation_retry_interval, 5);
        assert_eq!(cfg.kermit_negotiation_timeout, 300);
        assert_eq!(cfg.kermit_packet_timeout, 10);
        assert_eq!(cfg.kermit_max_retries, 5);
        assert_eq!(cfg.kermit_max_packet_length, 4096);
        assert_eq!(cfg.kermit_window_size, 4);
        assert_eq!(cfg.kermit_block_check_type, 3);
        assert!(cfg.kermit_long_packets);
        assert!(cfg.kermit_sliding_windows);
        assert!(cfg.kermit_streaming);
        assert!(cfg.kermit_attribute_packets);
        assert!(cfg.kermit_repeat_compression);
        assert_eq!(cfg.kermit_8bit_quote, "auto");
        assert!(!cfg.kermit_resume_partial);
        assert_eq!(cfg.kermit_resume_max_age_hours, 168);
        assert!(!cfg.kermit_locking_shifts);
        assert!(cfg.kermit_wait_for_receiver);
        assert!(!cfg.allow_atdt_kermit);
        assert!(!cfg.allow_peer_dial);
        assert!(!cfg.punter_hangup_on_failure);
        for port in [&cfg.serial_a, &cfg.serial_b] {
            assert!(!port.enabled);
            assert_eq!(port.mode, "modem");
            assert_eq!(port.port, "");
            assert_eq!(port.baud, 9600);
            assert_eq!(port.databits, 8);
            assert_eq!(port.parity, "none");
            assert_eq!(port.stopbits, 1);
            assert_eq!(port.flowcontrol, "none");
            assert!(port.echo);
            assert!(port.verbose);
            assert!(!port.quiet);
            assert_eq!(
                port.s_regs,
                "5,0,43,13,10,8,2,15,2,6,14,95,50,0,0,0,0,0,0,0,0,0,0,0,0,5,1"
            );
            assert_eq!(port.x_code, 4);
            assert_eq!(port.dtr_mode, 0);
            assert_eq!(port.flow_mode, 0);
            assert_eq!(port.dcd_mode, 1);
            assert_eq!(
                port.stored_numbers,
                [
                    String::new(),
                    String::new(),
                    String::new(),
                    String::new(),
                ]
            );
        }
        assert!(!cfg.ssh_enabled);
        assert_eq!(cfg.ssh_port, 2222);
        // Unified credentials: telnet, SSH, and the web UI all
        // authenticate against the same username/password now.
        assert_eq!(cfg.username, "admin");
        assert_eq!(cfg.password, "changeme");
        // Web config server defaults: opt-in, port 8080 (the canonical
        // "alternate HTTP" port — high enough to avoid `<1024 needs
        // root` and unlikely to collide with system services).
        assert!(!cfg.web_enabled);
        assert_eq!(cfg.web_port, 8080);
        // Master/Slave relay: inert by default (standalone, no relays).
        assert_eq!(cfg.gateway_role, "standalone");
        assert!(!cfg.master_accept_relays);
        assert_eq!(cfg.slave_master_host, "");
        assert_eq!(cfg.slave_master_port, 2222);
        assert_eq!(cfg.slave_master_username, "");
        assert_eq!(cfg.slave_master_password, "");
        assert_eq!(cfg.relay_transport, "ssh");
    }

    /// `relays_blocked_by_ssh_off` is the one predicate behind the startup
    /// warning and the telnet / web / GUI notices, so pin every arm of it.
    #[test]
    fn test_relays_blocked_by_ssh_off() {
        let master_ready = Config {
            gateway_role: "master".into(),
            master_accept_relays: true,
            relay_transport: "ssh".into(),
            ssh_enabled: false,
            ..Config::default()
        };
        assert!(
            master_ready.relays_blocked_by_ssh_off(),
            "a master accepting SSH relays with SSH off is exactly the blocked case"
        );

        // Each condition on its own must clear the warning.
        let cases = [
            ("ssh on", Config { ssh_enabled: true, ..master_ready.clone() }),
            ("not accepting relays", Config { master_accept_relays: false, ..master_ready.clone() }),
            ("standalone", Config { gateway_role: "standalone".into(), ..master_ready.clone() }),
            ("slave", Config { gateway_role: "slave".into(), ..master_ready.clone() }),
            // The raw transport would not ride SSH, so complaining about SSH
            // being off would be wrong — this is why the predicate checks it.
            ("raw transport", Config { relay_transport: "raw".into(), ..master_ready.clone() }),
        ];
        for (why, cfg) in cases {
            assert!(
                !cfg.relays_blocked_by_ssh_off(),
                "spurious blocked-relay warning for: {why}"
            );
        }

        // A default config must never warn — a fresh install is standalone.
        assert!(!Config::default().relays_blocked_by_ssh_off());
    }

    /// Invalid enum-valued relay keys fall back to their defaults rather
    /// than storing garbage (mirrors the ssh_gateway_auth / parity guards).
    #[test]
    fn test_relay_keys_validate_and_fall_back() {
        let mut cfg = Config::default();

        // Valid values are accepted (lower-cased / trimmed).
        apply_config_key(&mut cfg, "gateway_role", "  Master ");
        assert_eq!(cfg.gateway_role, "master");
        apply_config_key(&mut cfg, "relay_transport", "RAW");
        assert_eq!(cfg.relay_transport, "raw");

        // Invalid values leave the prior value untouched.
        apply_config_key(&mut cfg, "gateway_role", "bogus");
        assert_eq!(cfg.gateway_role, "master");
        apply_config_key(&mut cfg, "relay_transport", "carrier-pigeon");
        assert_eq!(cfg.relay_transport, "raw");

        // Port rejects 0 / non-numeric, keeps default.
        apply_config_key(&mut cfg, "slave_master_port", "0");
        assert_eq!(cfg.slave_master_port, 2222);
        apply_config_key(&mut cfg, "slave_master_port", "2200");
        assert_eq!(cfg.slave_master_port, 2200);

        // Free-text fields pass through.
        apply_config_key(&mut cfg, "slave_master_host", " 10.0.0.5 ");
        assert_eq!(cfg.slave_master_host, "10.0.0.5");
        apply_config_key(&mut cfg, "master_accept_relays", "true");
        assert!(cfg.master_accept_relays);
        assert!(!cfg.allow_peer_dial);
        apply_config_key(&mut cfg, "allow_peer_dial", "true");
        assert!(cfg.allow_peer_dial);
        apply_config_key(&mut cfg, "allow_peer_dial", "false");
        assert!(!cfg.allow_peer_dial);
    }

    /// The four on-disk-log keys must be settable through `apply_config_key`,
    /// the path BOTH the telnet and web UIs write by (`update_config_value` /
    /// `update_config_values`).  The GUI is different — it persists the whole
    /// `Config` struct — which is exactly why this needed its own test: a key
    /// with a parser, a writer and a struct field but no `apply_config_key` arm
    /// looks completely wired, works in the GUI, and is silently dropped by the
    /// other two UIs.  That is how these four shipped in `3c9ff89`.
    #[test]
    fn test_log_keys_apply() {
        let mut cfg = Config::default();

        apply_config_key(&mut cfg, "log_to_file", "false");
        assert!(!cfg.log_to_file);
        apply_config_key(&mut cfg, "log_to_file", "TRUE");
        assert!(cfg.log_to_file, "the bool arm is case-insensitive like its peers");

        apply_config_key(&mut cfg, "log_file", "  /var/log/eg.log  ");
        assert_eq!(cfg.log_file, "/var/log/eg.log", "trimmed, as file_policy_from expects");
        apply_config_key(&mut cfg, "log_file", "");
        assert_eq!(cfg.log_file, "", "empty is a valid off-switch, not a rejection");

        apply_config_key(&mut cfg, "log_max_size_kb", "2048");
        assert_eq!(cfg.log_max_size_kb, 2048);
        apply_config_key(&mut cfg, "log_max_files", "3");
        assert_eq!(cfg.log_max_files, 3);

        // Zero must survive on both: it is the documented "no size rotation" /
        // "keep no history" sentinel, so neither may be floored to 1.
        apply_config_key(&mut cfg, "log_max_size_kb", "0");
        assert_eq!(cfg.log_max_size_kb, 0);
        apply_config_key(&mut cfg, "log_max_files", "0");
        assert_eq!(cfg.log_max_files, 0);

        // Junk leaves the previous value alone rather than resetting it.
        apply_config_key(&mut cfg, "log_max_size_kb", "lots");
        assert_eq!(cfg.log_max_size_kb, 0);
        apply_config_key(&mut cfg, "log_max_files", "-1");
        assert_eq!(cfg.log_max_files, 0);
    }

    /// The gateway terminal-geometry override must survive `apply_config_key`
    /// (telnet + web write through it), and `0` must survive as the "auto"
    /// sentinel.  Flooring these to 1 — the guard every *port* key carries —
    /// would make automatic geometry unreachable from every UI, which is the
    /// same trap `log_max_size_kb` / `log_max_files` sit in.
    #[test]
    fn test_gateway_term_geometry_keys_apply() {
        let mut cfg = Config {
            gateway_term_width: 132,
            gateway_term_height: 50,
            ..Config::default()
        };

        apply_config_key(&mut cfg, "gateway_term_width", "40");
        assert_eq!(cfg.gateway_term_width, 40);
        apply_config_key(&mut cfg, "gateway_term_height", "25");
        assert_eq!(cfg.gateway_term_height, 25);

        // Zero is "auto", not an invalid width.
        apply_config_key(&mut cfg, "gateway_term_width", "0");
        assert_eq!(cfg.gateway_term_width, 0, "0 must be accepted as auto");
        apply_config_key(&mut cfg, "gateway_term_height", "0");
        assert_eq!(cfg.gateway_term_height, 0, "0 must be accepted as auto");

        // Junk and out-of-range leave the previous value alone rather than
        // resetting it to the default.
        apply_config_key(&mut cfg, "gateway_term_width", "80");
        apply_config_key(&mut cfg, "gateway_term_width", "wide");
        assert_eq!(cfg.gateway_term_width, 80, "junk must not clobber");
        apply_config_key(&mut cfg, "gateway_term_width", "70000");
        assert_eq!(cfg.gateway_term_width, 80, "past u16 must not clobber");
        apply_config_key(&mut cfg, "gateway_term_width", "-1");
        assert_eq!(cfg.gateway_term_width, 80, "negative must not clobber");
    }

    /// Drift-proof version of the test above, for every key rather than four:
    /// each key the config *writer* emits must also have an `apply_config_key`
    /// arm, or the telnet and web UIs cannot set it.  Scans this file's own
    /// source (the technique that found the third over-wide `show_error`
    /// literal) because a `match` cannot be introspected at runtime and the
    /// `_ => {}` fallthrough makes an unhandled key indistinguishable from a
    /// handled one.
    ///
    /// Per-port `serial_a_*` / `serial_b_*` keys are emitted by
    /// `write_serial_port_section` and applied by `apply_port_key`, so they
    /// appear in neither literal set and are covered by their own tests.
    #[test]
    fn test_every_written_key_can_be_applied() {
        // Scan only the production half.  The marker strings below appear
        // verbatim in this test's own source, so scanning the whole file makes
        // the scanner match itself and report a phantom key called "key".
        let whole = include_str!("config.rs");
        let src = {
            // Anchor on the test MODULE, not the first `#[cfg(test)]` item:
            // CONFIG_TEST_LOCK and friends are test-only items near the top of
            // the file, so cutting at the first attribute discarded the writer
            // and the scan silently found nothing.
            //
            // Must not span a line break: a Windows checkout has CRLF endings,
            // so an anchor containing a literal `\n` matches nothing there and
            // this test failed on windows-latest only.
            let cut = whole
                .find("\nmod tests {")
                .expect("test module marker moved — this scan needs updating");
            &whole[..cut]
        };

        // Keys the writer persists: write_kv(&mut content, "key", ...) and the
        // _str variant.
        let mut written: Vec<&str> = Vec::new();
        for marker in ["write_kv(&mut content, \"", "write_kv_str(&mut content, \""] {
            let mut rest = src;
            while let Some(i) = rest.find(marker) {
                rest = &rest[i + marker.len()..];
                if let Some(end) = rest.find('"') {
                    written.push(&rest[..end]);
                }
            }
        }
        assert!(
            written.len() > 80,
            "expected >80 written keys, found {} — the writer scan has stopped \
             matching (did write_kv's call shape change?)",
            written.len()
        );

        // Keys apply_config_key handles: string literals in match-arm position
        // inside its body, including `"a" | "b" =>` alternates.
        let body = {
            let start = src
                .find("fn apply_config_key")
                .expect("apply_config_key not found — this scan needs renaming");
            let after = &src[start..];
            // Ends at the next top-level `fn ` definition.
            let end = after[1..].find("\nfn ").map(|e| e + 1).unwrap_or(after.len());
            &after[..end]
        };
        let mut applied: Vec<&str> = Vec::new();
        for line in body.lines() {
            let t = line.trim();
            // Match-arm lines look like: "key" => ...  or  "a" | "b" => ...
            let Some(arrow) = t.find("=>") else { continue };
            let head = &t[..arrow];
            if !head.trim_start().starts_with('"') {
                continue;
            }
            let mut rest = head;
            while let Some(i) = rest.find('"') {
                rest = &rest[i + 1..];
                if let Some(end) = rest.find('"') {
                    applied.push(&rest[..end]);
                    rest = &rest[end + 1..];
                } else {
                    break;
                }
            }
        }
        assert!(
            applied.len() > 80,
            "expected >80 applied keys, found {} — the arm scan has stopped \
             matching apply_config_key's real arms",
            applied.len()
        );

        let missing: Vec<&&str> = written
            .iter()
            .filter(|k| !applied.contains(k))
            .collect();
        assert!(
            missing.is_empty(),
            "these keys are written to egateway.conf but have no \
             apply_config_key arm, so the telnet and web UIs silently drop \
             them (the GUI would still work, which is what hides it): {:?}",
            missing
        );
    }

    #[test]
    fn test_read_config_file() {
        let dir = std::env::temp_dir().join("xmodem_test_read_cfg");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test.conf");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "# comment line").unwrap();
        writeln!(f, "telnet_port = 9999").unwrap();
        writeln!(f, "security_enabled = true").unwrap();
        writeln!(f, "username = myuser").unwrap();
        writeln!(f, "password = mypass").unwrap();
        writeln!(f, "transfer_dir = files").unwrap();
        writeln!(f, "max_sessions = 10").unwrap();
        writeln!(f, "idle_timeout_secs = 300").unwrap();
        writeln!(f, "unknown_key = ignored").unwrap();
        drop(f);

        let cfg = read_config_file(path.to_str().unwrap());
        assert_eq!(cfg.telnet_port, 9999);
        assert!(cfg.security_enabled);
        assert_eq!(cfg.username, "myuser");
        assert_eq!(cfg.password, "mypass");
        assert_eq!(cfg.transfer_dir, "files");
        assert_eq!(cfg.max_sessions, 10);
        assert_eq!(cfg.idle_timeout_secs, 300);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // M-12: an existing-but-unreadable config must surface an Err from the
    // checked reader so startup fails loud rather than silently overwriting
    // the file with insecure defaults (security off, password "changeme").
    #[test]
    fn test_read_config_file_checked_rejects_non_utf8() {
        let dir = std::env::temp_dir().join("xmodem_test_checked_nonutf8");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("corrupt.conf");
        // Invalid UTF-8 byte sequence (0xFF is never valid in UTF-8) — the
        // kind of garbage a power-loss truncation or disk corruption leaves.
        std::fs::write(&path, [0xFF, 0xFE, 0x00, 0x80]).unwrap();

        let result = read_config_file_checked(path.to_str().unwrap());
        assert!(
            result.is_err(),
            "non-UTF-8 config must be reported as an error, not defaults"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_read_config_file_checked_rejects_missing_file() {
        let result =
            read_config_file_checked("/nonexistent/path/that/does/not/exist_checked.conf");
        assert!(result.is_err(), "missing config must be reported as an error");
    }

    // M-12 residual: an existing file that parses to no recognized settings
    // (empty / whitespace / comments-only — e.g. a zero-byte truncation) must
    // be reported as unreadable, not accepted as "all defaults" (which would
    // silently downgrade security_enabled/password on the rewrite).
    #[test]
    fn test_read_config_file_checked_rejects_empty_and_commentonly() {
        let dir = std::env::temp_dir().join("xmodem_test_empty_cfg");
        let _ = std::fs::create_dir_all(&dir);
        for (name, body) in [
            ("empty.conf", ""),
            ("blank.conf", "   \n\t\n  \n"),
            ("comments.conf", "# just a comment\n# another\n"),
        ] {
            let path = dir.join(name);
            std::fs::write(&path, body).unwrap();
            assert!(
                read_config_file_checked(path.to_str().unwrap()).is_err(),
                "{name}: a config with no recognized keys must be an error"
            );
        }
        // Sanity: one real key makes it parse cleanly.
        let ok = dir.join("ok.conf");
        std::fs::write(&ok, "telnet_port = 2323\n").unwrap();
        assert!(read_config_file_checked(ok.to_str().unwrap()).is_ok());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_read_config_missing_keys_use_defaults() {
        let dir = std::env::temp_dir().join("xmodem_test_missing_keys");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("partial.conf");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "telnet_port = 4444").unwrap();
        drop(f);

        let cfg = read_config_file(path.to_str().unwrap());
        assert_eq!(cfg.telnet_port, 4444);
        assert!(!cfg.security_enabled);
        assert_eq!(cfg.username, "admin");
        assert_eq!(cfg.transfer_dir, "transfer");
        // SSH fields should also get defaults when missing from file
        assert!(!cfg.ssh_enabled);
        assert_eq!(cfg.ssh_port, 2222);
        // SSH no longer has its own credential pair — the unified
        // `username` / `password` covers it (asserted above).

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_read_config_invalid_port_uses_default() {
        let dir = std::env::temp_dir().join("xmodem_test_bad_port");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("bad.conf");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "telnet_port = notanumber").unwrap();
        drop(f);

        let cfg = read_config_file(path.to_str().unwrap());
        assert_eq!(cfg.telnet_port, DEFAULT_TELNET_PORT);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Test 7: a config file with malformed content — lines without
    /// `=`, garbage tokens, junk values — must not panic.  Every key
    /// should fall back to its default.
    #[test]
    fn test_read_config_malformed_file_falls_back_to_defaults() {
        let dir = std::env::temp_dir().join("xmodem_test_malformed");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("garbage.conf");
        let mut f = std::fs::File::create(&path).unwrap();
        // A mix of malformed constructs a hostile or buggy editor
        // might leave behind.
        writeln!(f, "this line has no equals sign").unwrap();
        writeln!(f, "= value_with_no_key").unwrap();
        writeln!(f, "telnet_port = ").unwrap();           // empty value
        writeln!(f, "telnet_port = -99999999999").unwrap(); // overflow
        writeln!(f, "max_sessions = banana").unwrap();
        writeln!(f, "security_enabled = maybe").unwrap();
        writeln!(f, "serial_baud = 0").unwrap();            // below min
        writeln!(f, "serial_databits = 42").unwrap();       // out of valid range
        writeln!(f, "serial_parity = quantum").unwrap();    // invalid enum
        writeln!(f, "serial_mode = telegraph").unwrap();    // invalid enum
        writeln!(f, "###").unwrap();                        // comment-ish
        writeln!(f, "\x00\x01\x02binary junk").unwrap();
        writeln!(f).unwrap();                                // blank
        writeln!(f, "     ").unwrap();
        drop(f);

        let cfg = read_config_file(path.to_str().unwrap());
        // Every field must hold its default — nothing the malformed
        // file offered was acceptable.
        let defaults = Config::default();
        assert_eq!(cfg.telnet_port, defaults.telnet_port);
        assert_eq!(cfg.max_sessions, defaults.max_sessions);
        assert_eq!(cfg.security_enabled, defaults.security_enabled);
        // Malformed legacy `serial_*` keys must still fall back to defaults
        // even though the migration path picks them up for Port A.
        assert_eq!(cfg.serial_a.baud, defaults.serial_a.baud);
        assert_eq!(cfg.serial_a.databits, defaults.serial_a.databits);
        assert_eq!(cfg.serial_a.parity, defaults.serial_a.parity);
        assert_eq!(cfg.serial_a.mode, defaults.serial_a.mode);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Test 7b: reading a config file that doesn't exist returns the
    /// full default Config without panicking.
    #[test]
    fn test_read_config_missing_file_returns_defaults() {
        let cfg = read_config_file("/nonexistent/path/that/does/not/exist.conf");
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn test_write_and_reread_config() {
        let dir = std::env::temp_dir().join("xmodem_test_roundtrip");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("roundtrip.conf");

        let original = Config {
            // Not the default, so the roundtrip proves the key is written and
            // read back rather than defaulting twice at both ends.
            place_bundled_terminals: false,
            cpm_mounts: "A=DISK01.DSK,C=CDISK02.DSK".to_string(),
            cpm_boot_image: "HDSK04.DSK".to_string(),
            // Deliberately not the default, so the roundtrip proves the key is
            // written and read back rather than merely defaulting twice.  It
            // was `true` while the default was `false`; both have to move
            // together or this stops testing anything.
            cpm_boot_writable: false,
            // A one-shot marker, not a setting -- but it round-trips like any
            // other key and a roundtrip test that skipped it would not notice
            // it being dropped from the writer.
            open_screen_after_restart: true,
            cpm_boot_machine: "console_04_cuter".to_string(),
            // Likewise not the default: `rubout` is what a CP/M 1.x operator
            // sets, and it has to survive a write/read cycle to be worth having.
            cpm_boot_backspace: crate::cpm::boot::BACKSPACE_RUBOUT.to_string(),
            cpm_printer: crate::cpm::printer::PRINTER_ODT.to_string(),
            cpm_printer_port: crate::cpm::printer::PORT_OFF.to_string(),
            // Not the default: `off` is what an operator running WordStar sets,
            // and it has to survive a write/read cycle to be worth having.
            cpm_printer_autolf: crate::cpm::printer::AUTOLF_OFF.to_string(),
            // Likewise not the default: the 8080 is the setting an operator
            // running period diagnostics picks, and it has to survive the cycle.
            cpm_cpu: crate::cpm::cpu::CPU_8080.to_string(),
            telnet_enabled: false,
            telnet_port: 1234,
            telnet_gateway_negotiate: true,
            telnet_gateway_raw: true,
            gateway_debug: true,
            gateway_term_width: 40,
            gateway_term_height: 25,
            enable_console: true,
            setup_wizard_completed: true,
            security_enabled: true,
            disable_ip_safety: true,
            username: "bob".into(),
            password: "secret".into(),
            transfer_dir: "myfiles".into(),
            gui_window_geometry: "100,120,1280,900".into(),
            gui_zoom: "auto".into(),
            max_sessions: 5,
            idle_timeout_secs: 60,
            groq_api_key: "gsk_test123".into(),
            browser_homepage: "https://example.com".into(),
            weather_location: "90210".into(),
            weather_units: "metric".into(),
            log_to_file: false,
            log_file: "custom.log".into(),
            log_max_size_kb: 77,
            log_max_files: 9,
            verbose: true,
            xmodem_negotiation_timeout: 120,
            xmodem_block_timeout: 30,
            xmodem_max_retries: 15,
            xmodem_negotiation_retry_interval: 9,
            zmodem_negotiation_timeout: 90,
            zmodem_frame_timeout: 45,
            zmodem_max_retries: 7,
            zmodem_negotiation_retry_interval: 8,
            kermit_negotiation_timeout: 60,
            kermit_packet_timeout: 12,
            kermit_idle_timeout: 120,
            kermit_max_retries: 8,
            kermit_max_packet_length: 2048,
            kermit_window_size: 8,
            kermit_block_check_type: 2,
            kermit_long_packets: false,
            kermit_sliding_windows: false,
            kermit_streaming: false,
            kermit_attribute_packets: false,
            kermit_repeat_compression: false,
            kermit_8bit_quote: "on".into(),
            kermit_resume_partial: true,
            kermit_resume_max_age_hours: 72,
            kermit_locking_shifts: true,
            kermit_wait_for_receiver: false,
            allow_atdt_kermit: true,
            allow_peer_dial: true,
            kermit_server_enabled: true,
            kermit_server_port: 2525,
            punter_block_size: 200,
            punter_negotiation_timeout: 50,
            punter_block_timeout: 25,
            punter_max_retries: 12,
            punter_max_bad_rounds: 18,
            punter_negotiation_retry_interval: 6,
            punter_hangup_on_failure: true,
            web_enabled: true,
            web_port: 9090,
            cpm_emu_enabled: true,
            cpm_screen_input: false,
            disable_gateway_connections: true,
            cpm_emu_max_minstr: 500,
            cpm_emu_uart: "rc2014_1b".to_string(),
            cpm_emu_modem: CpmModemProfile {
                echo: false,
                verbose: false,
                quiet: true,
                x_code: 2,
                dcd_mode: 0,
                s_regs: "3,0,43,13,10,8,2,20".to_string(),
            },
            serial_a: SerialPortConfig {
                enabled: true,
                mode: "console".into(),
                port: "/dev/ttyUSB0".into(),
                baud: 115200,
                databits: 7,
                parity: "even".into(),
                stopbits: 2,
                flowcontrol: "hardware".into(),
                echo: false,
                verbose: false,
                quiet: true,
                s_regs: "1,0,43,13,10,8,2,50,2,6,14,95,50".into(),
                x_code: 3,
                dtr_mode: 2,
                flow_mode: 3,
                dcd_mode: 0,
                stored_numbers: [
                    "5551234".into(),
                    "example.com:23".into(),
                    String::new(),
                    "9W,5551212".into(),
                ],
                petscii_translate: true,
                drive_carrier: true,
            },
            serial_b: SerialPortConfig {
                enabled: true,
                mode: "modem".into(),
                port: "/dev/ttyUSB1".into(),
                baud: 19200,
                databits: 8,
                parity: "odd".into(),
                stopbits: 1,
                flowcontrol: "software".into(),
                echo: true,
                verbose: true,
                quiet: false,
                s_regs: "5,1,43,13,10,8,2,15,2,6,14,95,50".into(),
                x_code: 2,
                dtr_mode: 1,
                flow_mode: 2,
                dcd_mode: 1,
                stored_numbers: [
                    "B1".into(),
                    "B2".into(),
                    "B3".into(),
                    "B4".into(),
                ],
                petscii_translate: false,
                drive_carrier: false,
            },
            ssh_enabled: true,
            ssh_port: 2222,
            ssh_gateway_auth: "password".into(),
            gateway_role: "slave".into(),
            master_accept_relays: true,
            allow_relay_kermit: true,
            slave_master_host: "192.168.1.10".into(),
            slave_master_port: 2200,
            slave_master_username: "relay-user".into(),
            slave_master_password: "relay-pass".into(),
            relay_transport: "ssh".into(),
        };
        write_config_file(path.to_str().unwrap(), &original).unwrap();
        let loaded = read_config_file(path.to_str().unwrap());

        assert_eq!(loaded.telnet_enabled, original.telnet_enabled);
        assert_eq!(loaded.telnet_port, original.telnet_port);
        assert_eq!(
            loaded.telnet_gateway_negotiate,
            original.telnet_gateway_negotiate
        );
        assert_eq!(loaded.telnet_gateway_raw, original.telnet_gateway_raw);
        assert_eq!(loaded.gateway_debug, original.gateway_debug);
        assert_eq!(loaded.gateway_term_width, original.gateway_term_width);
        assert_eq!(loaded.gateway_term_height, original.gateway_term_height);
        assert_eq!(loaded.enable_console, original.enable_console);
        assert_eq!(
            loaded.setup_wizard_completed,
            original.setup_wizard_completed
        );
        assert_eq!(loaded.security_enabled, original.security_enabled);
        assert_eq!(loaded.username, original.username);
        assert_eq!(loaded.password, original.password);
        assert_eq!(loaded.transfer_dir, original.transfer_dir);
        assert_eq!(loaded.max_sessions, original.max_sessions);
        assert_eq!(loaded.idle_timeout_secs, original.idle_timeout_secs);
        assert_eq!(loaded.groq_api_key, original.groq_api_key);
        assert_eq!(loaded.browser_homepage, original.browser_homepage);
        assert_eq!(loaded.weather_location, original.weather_location);
        assert_eq!(loaded.weather_units, original.weather_units);
        assert_eq!(loaded.verbose, original.verbose);
        assert_eq!(loaded.xmodem_negotiation_timeout, original.xmodem_negotiation_timeout);
        assert_eq!(loaded.xmodem_block_timeout, original.xmodem_block_timeout);
        assert_eq!(loaded.xmodem_max_retries, original.xmodem_max_retries);
        assert_eq!(
            loaded.xmodem_negotiation_retry_interval,
            original.xmodem_negotiation_retry_interval
        );
        assert_eq!(loaded.zmodem_negotiation_timeout, original.zmodem_negotiation_timeout);
        assert_eq!(loaded.zmodem_frame_timeout, original.zmodem_frame_timeout);
        assert_eq!(loaded.zmodem_max_retries, original.zmodem_max_retries);
        assert_eq!(
            loaded.zmodem_negotiation_retry_interval,
            original.zmodem_negotiation_retry_interval
        );
        assert_eq!(
            loaded.kermit_negotiation_timeout,
            original.kermit_negotiation_timeout
        );
        assert_eq!(loaded.kermit_packet_timeout, original.kermit_packet_timeout);
        assert_eq!(loaded.kermit_max_retries, original.kermit_max_retries);
        assert_eq!(
            loaded.kermit_max_packet_length,
            original.kermit_max_packet_length
        );
        assert_eq!(loaded.kermit_window_size, original.kermit_window_size);
        assert_eq!(
            loaded.kermit_block_check_type,
            original.kermit_block_check_type
        );
        assert_eq!(loaded.kermit_long_packets, original.kermit_long_packets);
        assert_eq!(
            loaded.kermit_sliding_windows,
            original.kermit_sliding_windows
        );
        assert_eq!(loaded.kermit_streaming, original.kermit_streaming);
        assert_eq!(
            loaded.kermit_attribute_packets,
            original.kermit_attribute_packets
        );
        assert_eq!(
            loaded.kermit_repeat_compression,
            original.kermit_repeat_compression
        );
        assert_eq!(loaded.kermit_8bit_quote, original.kermit_8bit_quote);
        assert_eq!(
            loaded.kermit_resume_partial,
            original.kermit_resume_partial
        );
        assert_eq!(
            loaded.kermit_resume_max_age_hours,
            original.kermit_resume_max_age_hours
        );
        assert_eq!(
            loaded.kermit_locking_shifts,
            original.kermit_locking_shifts
        );
        assert_eq!(
            loaded.kermit_wait_for_receiver,
            original.kermit_wait_for_receiver
        );
        assert_eq!(loaded.allow_atdt_kermit, original.allow_atdt_kermit);
        assert_eq!(loaded.allow_peer_dial, original.allow_peer_dial);
        assert_eq!(loaded.kermit_server_enabled, original.kermit_server_enabled);
        assert_eq!(loaded.kermit_server_port, original.kermit_server_port);
        assert_eq!(loaded.punter_block_size, original.punter_block_size);
        assert_eq!(loaded.punter_negotiation_timeout, original.punter_negotiation_timeout);
        assert_eq!(loaded.punter_block_timeout, original.punter_block_timeout);
        assert_eq!(loaded.punter_max_retries, original.punter_max_retries);
        assert_eq!(loaded.punter_max_bad_rounds, original.punter_max_bad_rounds);
        assert_eq!(
            loaded.punter_negotiation_retry_interval,
            original.punter_negotiation_retry_interval
        );
        assert_eq!(loaded.punter_hangup_on_failure, original.punter_hangup_on_failure);
        assert_eq!(loaded.serial_a, original.serial_a);
        assert_eq!(loaded.serial_b, original.serial_b);
        assert_eq!(loaded.ssh_enabled, original.ssh_enabled);
        assert_eq!(loaded.ssh_port, original.ssh_port);
        assert_eq!(loaded.ssh_gateway_auth, original.ssh_gateway_auth);
        assert_eq!(loaded.gateway_role, original.gateway_role);
        assert_eq!(loaded.master_accept_relays, original.master_accept_relays);
        assert_eq!(loaded.slave_master_host, original.slave_master_host);
        assert_eq!(loaded.slave_master_port, original.slave_master_port);
        assert_eq!(loaded.slave_master_username, original.slave_master_username);
        assert_eq!(loaded.slave_master_password, original.slave_master_password);
        assert_eq!(loaded.relay_transport, original.relay_transport);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_security_enabled_case_insensitive() {
        let dir = std::env::temp_dir().join("xmodem_test_bool_case");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("case.conf");

        for val in &["TRUE", "True", "true"] {
            std::fs::write(&path, format!("security_enabled = {}", val)).unwrap();
            let cfg = read_config_file(path.to_str().unwrap());
            assert!(cfg.security_enabled, "Failed for value: {}", val);
        }

        for val in &["false", "False", "no", "0", ""] {
            std::fs::write(&path, format!("security_enabled = {}", val)).unwrap();
            let cfg = read_config_file(path.to_str().unwrap());
            assert!(!cfg.security_enabled, "Should be false for value: {}", val);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The two directions of the wizard flag are deliberately asymmetric: a
    /// config file that predates the key belongs to an already-configured
    /// gateway and must NOT be dropped into the setup wizard, while the
    /// defaults a first run writes must.  See DEFAULT_SETUP_WIZARD_COMPLETED.
    #[test]
    fn test_missing_wizard_key_in_existing_file_reads_completed() {
        let dir = std::env::temp_dir().join("xmodem_test_wizard_key");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("upgrade.conf");

        // An old config file: real settings, no setup_wizard_completed key.
        std::fs::write(&path, "telnet_enabled = true\ntelnet_port = 2323\n").unwrap();
        let cfg = read_config_file(path.to_str().unwrap());
        assert!(
            cfg.setup_wizard_completed,
            "an upgrade must not be sent through the first-run wizard"
        );

        // An explicit false still means "run it".
        std::fs::write(&path, "setup_wizard_completed = false\n").unwrap();
        let cfg = read_config_file(path.to_str().unwrap());
        assert!(!cfg.setup_wizard_completed);

        // A fresh install has no file at all, and takes its value from the
        // struct default — which is the one that shows the wizard.  Flipping
        // DEFAULT_SETUP_WIZARD_COMPLETED would silence it on every fresh
        // install, so this assertion is the guard on that constant.
        assert!(!Config::default().setup_wizard_completed);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_read_nonexistent_file_returns_defaults() {
        let cfg = read_config_file("/tmp/xmodem_nonexistent_12345.conf");
        assert_eq!(cfg.telnet_port, DEFAULT_TELNET_PORT);
        assert!(!cfg.security_enabled);
    }

    /// Assert that every field the reader recognizes is present in
    /// the writer's output.  Direct regression test for the pre-
    /// refactor positional `format!()` footgun where missing one
    /// `{}` slot would silently shift every subsequent value onto
    /// the wrong line — under the refactor each field is named at
    /// the call site, but this test guards against drift between
    /// the reader's key list and the writer's emit list.
    #[test]
    fn test_write_emits_every_reader_recognized_key() {
        let dir = std::env::temp_dir().join("xmodem_test_field_presence");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("presence.conf");

        write_config_file(path.to_str().unwrap(), &Config::default()).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();

        // Every key the reader's `apply_config_key` matches must
        // appear as a `key = ...` line in the written file.  Lock
        // this list down — adding a new field means updating both
        // the reader and this test.
        let expected_keys: &[&str] = &[
            "telnet_enabled",
            "telnet_port",
            "telnet_gateway_negotiate",
            "telnet_gateway_raw",
            "gateway_debug",
            "enable_console",
            "setup_wizard_completed",
            "security_enabled",
            "disable_ip_safety",
            "username",
            "password",
            "transfer_dir",
            "gui_zoom",
            "max_sessions",
            "idle_timeout_secs",
            "groq_api_key",
            "browser_homepage",
            "weather_location",
            "weather_units",
            "verbose",
            "xmodem_negotiation_timeout",
            "xmodem_block_timeout",
            "xmodem_max_retries",
            "xmodem_negotiation_retry_interval",
            "zmodem_negotiation_timeout",
            "zmodem_frame_timeout",
            "zmodem_max_retries",
            "zmodem_negotiation_retry_interval",
            "kermit_negotiation_timeout",
            "kermit_packet_timeout",
            "kermit_idle_timeout",
            "kermit_max_retries",
            "kermit_max_packet_length",
            "kermit_window_size",
            "kermit_block_check_type",
            "kermit_long_packets",
            "kermit_sliding_windows",
            "kermit_streaming",
            "kermit_attribute_packets",
            "kermit_repeat_compression",
            "kermit_8bit_quote",
            "kermit_resume_partial",
            "kermit_resume_max_age_hours",
            "kermit_locking_shifts",
            "kermit_wait_for_receiver",
            "punter_block_size",
            "punter_negotiation_timeout",
            "punter_block_timeout",
            "punter_max_retries",
            "punter_max_bad_rounds",
            "punter_negotiation_retry_interval",
            "punter_hangup_on_failure",
            "web_enabled",
            "web_port",
            "cpm_emu_enabled",
            "cpm_emu_max_minstr",
            "cpm_emu_uart",
            "serial_a_enabled",
            "serial_a_mode",
            "serial_a_port",
            "serial_a_baud",
            "serial_a_databits",
            "serial_a_parity",
            "serial_a_stopbits",
            "serial_a_flowcontrol",
            "serial_a_echo",
            "serial_a_verbose",
            "serial_a_quiet",
            "serial_a_s_regs",
            "serial_a_x_code",
            "serial_a_dtr_mode",
            "serial_a_flow_mode",
            "serial_a_dcd_mode",
            "serial_a_stored_0",
            "serial_a_stored_1",
            "serial_a_stored_2",
            "serial_a_stored_3",
            "serial_a_petscii_translate",
            "serial_a_drive_carrier",
            "serial_b_enabled",
            "serial_b_mode",
            "serial_b_port",
            "serial_b_baud",
            "serial_b_databits",
            "serial_b_parity",
            "serial_b_stopbits",
            "serial_b_flowcontrol",
            "serial_b_echo",
            "serial_b_verbose",
            "serial_b_quiet",
            "serial_b_s_regs",
            "serial_b_x_code",
            "serial_b_dtr_mode",
            "serial_b_flow_mode",
            "serial_b_dcd_mode",
            "serial_b_stored_0",
            "serial_b_stored_1",
            "serial_b_stored_2",
            "serial_b_stored_3",
            "serial_b_petscii_translate",
            "serial_b_drive_carrier",
            "ssh_enabled",
            "ssh_port",
            "ssh_gateway_auth",
            "gateway_role",
            "master_accept_relays",
            "slave_master_host",
            "slave_master_port",
            "slave_master_username",
            "slave_master_password",
            "relay_transport",
        ];

        for key in expected_keys {
            let needle = format!("{} = ", key);
            assert!(
                written.contains(&needle),
                "key `{}` missing from written config — writer drifted from reader's apply_config_key match list",
                key
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_empty_config_file_returns_defaults() {
        let dir = std::env::temp_dir().join("xmodem_test_empty");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("empty.conf");
        std::fs::write(&path, "").unwrap();

        let cfg = read_config_file(path.to_str().unwrap());
        assert_eq!(cfg.telnet_port, DEFAULT_TELNET_PORT);
        assert_eq!(cfg.transfer_dir, DEFAULT_TRANSFER_DIR);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_apply_config_key_serial_fields() {
        let mut cfg = Config::default();

        apply_config_key(&mut cfg, "serial_a_enabled", "true");
        assert!(cfg.serial_a.enabled);

        apply_config_key(&mut cfg, "serial_a_enabled", "false");
        assert!(!cfg.serial_a.enabled);

        apply_config_key(&mut cfg, "serial_a_port", "/dev/ttyS0");
        assert_eq!(cfg.serial_a.port, "/dev/ttyS0");

        apply_config_key(&mut cfg, "serial_a_baud", "115200");
        assert_eq!(cfg.serial_a.baud, 115200);

        apply_config_key(&mut cfg, "serial_a_databits", "7");
        assert_eq!(cfg.serial_a.databits, 7);

        // Invalid databits should be ignored
        apply_config_key(&mut cfg, "serial_a_databits", "9");
        assert_eq!(cfg.serial_a.databits, 7);

        apply_config_key(&mut cfg, "serial_a_parity", "even");
        assert_eq!(cfg.serial_a.parity, "even");

        // Invalid parity should be ignored
        apply_config_key(&mut cfg, "serial_a_parity", "bogus");
        assert_eq!(cfg.serial_a.parity, "even");

        apply_config_key(&mut cfg, "serial_a_stopbits", "2");
        assert_eq!(cfg.serial_a.stopbits, 2);

        // Invalid stopbits should be ignored
        apply_config_key(&mut cfg, "serial_a_stopbits", "3");
        assert_eq!(cfg.serial_a.stopbits, 2);

        apply_config_key(&mut cfg, "serial_a_flowcontrol", "hardware");
        assert_eq!(cfg.serial_a.flowcontrol, "hardware");

        // Invalid flow should be ignored
        apply_config_key(&mut cfg, "serial_a_flowcontrol", "bogus");
        assert_eq!(cfg.serial_a.flowcontrol, "hardware");

        // mode accepts "modem" / "console" / "kermit" (case-insensitive),
        // anything else is ignored.
        apply_config_key(&mut cfg, "serial_a_mode", "console");
        assert_eq!(cfg.serial_a.mode, "console");
        apply_config_key(&mut cfg, "serial_a_mode", "MODEM");
        assert_eq!(cfg.serial_a.mode, "modem");
        apply_config_key(&mut cfg, "serial_a_mode", "Kermit");
        assert_eq!(cfg.serial_a.mode, "kermit");
        apply_config_key(&mut cfg, "serial_a_mode", "bogus");
        assert_eq!(cfg.serial_a.mode, "kermit");
        // Restore console for the subsequent dual-port assertions below.
        apply_config_key(&mut cfg, "serial_a_mode", "console");
        assert_eq!(cfg.serial_a.mode, "console");
        // Whitespace around the value is trimmed before validation.
        apply_config_key(&mut cfg, "serial_a_mode", "  Console  ");
        assert_eq!(cfg.serial_a.mode, "console");
        // Empty value rejected — keep the existing setting.
        apply_config_key(&mut cfg, "serial_a_mode", "");
        assert_eq!(cfg.serial_a.mode, "console");

        // The same dispatch routes serial_b_* keys to Port B without
        // touching Port A.  This is the entire dual-port plumbing in one
        // assertion: prefix selects the slice.
        apply_config_key(&mut cfg, "serial_b_baud", "57600");
        assert_eq!(cfg.serial_b.baud, 57600);
        // Port A's previously-set values must be untouched by Port B writes.
        assert_eq!(cfg.serial_a.baud, 115200);
        assert_eq!(cfg.serial_a.mode, "console");
    }

    /// Out-of-range numeric serial values on the apply path are rejected (the
    /// prior value is kept), mirroring the file reader's clamping. Covers the
    /// numeric fields the dispatch test above doesn't: baud (<300), x_code
    /// (>4), dtr_mode (>3), flow_mode (>4), dcd_mode (>1), plus non-numeric.
    #[test]
    fn test_apply_serial_numeric_out_of_range_rejected() {
        let mut cfg = Config::default();

        // baud: below 300, or non-numeric, is rejected.
        apply_config_key(&mut cfg, "serial_a_baud", "57600");
        assert_eq!(cfg.serial_a.baud, 57600);
        apply_config_key(&mut cfg, "serial_a_baud", "100");
        assert_eq!(cfg.serial_a.baud, 57600, "baud < 300 must be rejected");
        apply_config_key(&mut cfg, "serial_a_baud", "fast");
        assert_eq!(cfg.serial_a.baud, 57600, "non-numeric baud must be rejected");

        // x_code: valid 0..=4.
        apply_config_key(&mut cfg, "serial_a_x_code", "4");
        assert_eq!(cfg.serial_a.x_code, 4);
        apply_config_key(&mut cfg, "serial_a_x_code", "5");
        assert_eq!(cfg.serial_a.x_code, 4, "x_code > 4 must be rejected");

        // dtr_mode: valid 0..=3.
        apply_config_key(&mut cfg, "serial_a_dtr_mode", "3");
        assert_eq!(cfg.serial_a.dtr_mode, 3);
        apply_config_key(&mut cfg, "serial_a_dtr_mode", "4");
        assert_eq!(cfg.serial_a.dtr_mode, 3, "dtr_mode > 3 must be rejected");

        // flow_mode: valid 0..=4.
        apply_config_key(&mut cfg, "serial_a_flow_mode", "4");
        assert_eq!(cfg.serial_a.flow_mode, 4);
        apply_config_key(&mut cfg, "serial_a_flow_mode", "5");
        assert_eq!(cfg.serial_a.flow_mode, 4, "flow_mode > 4 must be rejected");

        // dcd_mode: valid 0..=1.
        apply_config_key(&mut cfg, "serial_a_dcd_mode", "1");
        assert_eq!(cfg.serial_a.dcd_mode, 1);
        apply_config_key(&mut cfg, "serial_a_dcd_mode", "2");
        assert_eq!(cfg.serial_a.dcd_mode, 1, "dcd_mode > 1 must be rejected");
    }

    /// parity/flowcontrol are normalized (trim + lowercase) on the apply
    /// path, consistent with `mode`, while genuinely invalid values are
    /// still rejected.
    #[test]
    fn test_apply_serial_parity_flowcontrol_normalized() {
        let mut cfg = Config::default();
        apply_config_key(&mut cfg, "serial_a_parity", "  Even ");
        assert_eq!(cfg.serial_a.parity, "even");
        apply_config_key(&mut cfg, "serial_a_flowcontrol", "HARDWARE");
        assert_eq!(cfg.serial_a.flowcontrol, "hardware");
        // Invalid value is still rejected (prior value kept).
        apply_config_key(&mut cfg, "serial_a_parity", "bogus");
        assert_eq!(cfg.serial_a.parity, "even");
    }

    /// The file reader normalizes parity/flowcontrol case the same way,
    /// so a hand-edited `serial_a_parity = Even` is honored rather than
    /// silently reverting to the default.
    #[test]
    fn test_read_serial_parity_flowcontrol_normalized() {
        let dir = std::env::temp_dir().join("egw_test_parity_norm");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test.conf");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "serial_a_parity = Even").unwrap();
        writeln!(f, "serial_a_flowcontrol = Hardware").unwrap();
        drop(f);

        let cfg = read_config_file(path.to_str().unwrap());
        assert_eq!(cfg.serial_a.parity, "even");
        assert_eq!(cfg.serial_a.flowcontrol, "hardware");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A failed file write is reported (not silently swallowed), so an
    /// explicit Save can tell the user persistence did not happen.
    #[test]
    fn test_write_config_file_reports_failure() {
        let cfg = Config::default();
        // Parent directory does not exist → the atomic tmp open/rename fails.
        let bad = "/nonexistent-egw-dir-xyz/egateway.conf";
        assert!(
            write_config_file(bad, &cfg).is_err(),
            "writing under a non-existent directory must return Err"
        );
    }

    /// Reading a config file with `serial_a_mode = console` (case-
    /// insensitive, with surrounding whitespace) loads to the
    /// canonical lowercase value.  Reading without the key falls back
    /// to the modem default.
    #[test]
    fn test_read_config_serial_mode_variants() {
        let dir = std::env::temp_dir().join("xmodem_test_serial_mode");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("mode.conf");

        for (raw, expected) in [
            ("modem", "modem"),
            ("console", "console"),
            ("CONSOLE", "console"),
            ("  Modem  ", "modem"),
            ("kermit", "kermit"),
            ("KERMIT", "kermit"),
        ] {
            std::fs::write(&path, format!("serial_a_mode = {}", raw)).unwrap();
            let cfg = read_config_file(path.to_str().unwrap());
            assert_eq!(
                cfg.serial_a.mode, expected,
                "input {:?} should normalize to {:?}",
                raw, expected
            );
        }

        // Missing key → default.
        std::fs::write(&path, "serial_a_enabled = true").unwrap();
        let cfg = read_config_file(path.to_str().unwrap());
        assert_eq!(cfg.serial_a.mode, "modem");

        // Invalid value → default, doesn't poison other fields.
        std::fs::write(
            &path,
            "serial_a_enabled = true\nserial_a_mode = telegraph\nserial_a_baud = 19200",
        )
        .unwrap();
        let cfg = read_config_file(path.to_str().unwrap());
        assert_eq!(cfg.serial_a.mode, "modem");
        assert!(cfg.serial_a.enabled);
        assert_eq!(cfg.serial_a.baud, 19200);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `serial_<port>_mode` round-trips through write→read correctly for
    /// every value on both ports.  Guards against the writer dropping
    /// the field for one of the enum values, and against a future
    /// edit that accidentally loses the per-port symmetry.
    #[test]
    fn test_serial_mode_round_trip_both_values() {
        let dir = std::env::temp_dir().join("xmodem_test_serial_mode_rt");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("rt.conf");

        for value in ["modem", "console", "kermit"] {
            let cfg = Config {
                serial_a: SerialPortConfig {
                    mode: value.into(),
                    ..SerialPortConfig::default()
                },
                serial_b: SerialPortConfig {
                    mode: value.into(),
                    ..SerialPortConfig::default()
                },
                ..Config::default()
            };
            write_config_file(path.to_str().unwrap(), &cfg).unwrap();
            let loaded = read_config_file(path.to_str().unwrap());
            assert_eq!(loaded.serial_a.mode, value);
            assert_eq!(loaded.serial_b.mode, value);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_apply_config_key_unknown_key_ignored() {
        let mut cfg = Config::default();
        let baud_before = cfg.serial_a.baud;
        apply_config_key(&mut cfg, "nonexistent_key", "value");
        assert_eq!(cfg.serial_a.baud, baud_before);
    }

    #[test]
    fn test_apply_config_key_weather_location() {
        let mut cfg = Config::default();
        apply_config_key(&mut cfg, "weather_location", "London, GB");
        assert_eq!(cfg.weather_location, "London, GB");
        // The legacy key name still lands on the new field.
        let mut cfg2 = Config::default();
        apply_config_key(&mut cfg2, "weather_zip", "90210");
        assert_eq!(cfg2.weather_location, "90210");
    }

    #[test]
    fn test_apply_config_key_weather_units() {
        let mut cfg = Config::default();
        apply_config_key(&mut cfg, "weather_units", "METRIC");
        assert_eq!(cfg.weather_units, "metric"); // normalized
        // Invalid values are rejected, leaving the prior value intact.
        apply_config_key(&mut cfg, "weather_units", "kelvin");
        assert_eq!(cfg.weather_units, "metric");
    }

    #[test]
    fn test_weather_units_invalid_in_file_falls_back_to_auto() {
        // A hand-edited invalid units value loaded from disk must fall back to
        // the "auto" default (reader-side validation), not persist verbatim.
        let dir = std::env::temp_dir().join("xmodem_test_wx_units");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("units.conf");
        std::fs::write(&path, "weather_units = kelvin\n").unwrap();
        assert_eq!(read_config_file(path.to_str().unwrap()).weather_units, "auto");
        // A valid value loads as-is.
        std::fs::write(&path, "weather_units = metric\n").unwrap();
        assert_eq!(read_config_file(path.to_str().unwrap()).weather_units, "metric");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_legacy_weather_zip_migrates_to_location() {
        // An upgrading config that still has only the old `weather_zip` key
        // must load into `weather_location` (so Ricky's Pi keeps 62051).
        let dir = std::env::temp_dir().join("xmodem_test_wx_legacy");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("legacy.conf");
        std::fs::write(&path, "weather_zip = 62051\n").unwrap();
        let cfg = read_config_file(path.to_str().unwrap());
        assert_eq!(cfg.weather_location, "62051");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_weather_location_persists_foreign_values() {
        // City names, postcodes with spaces, commas, and UTF-8 must round-trip
        // through the config file writer + reader unchanged — persistence works
        // for other countries exactly as for a US zip.
        let dir = std::env::temp_dir().join("xmodem_test_wx_foreign");
        let _ = std::fs::create_dir_all(&dir);
        for (i, loc) in ["London, GB", "SW1A 1AA", "Zürich", "São Paulo", "東京", "62051"]
            .iter()
            .enumerate()
        {
            let path = dir.join(format!("wx_{i}.conf"));
            let original = Config {
                weather_location: loc.to_string(),
                weather_units: "metric".to_string(),
                ..Config::default()
            };
            write_config_file(path.to_str().unwrap(), &original).unwrap();
            let loaded = read_config_file(path.to_str().unwrap());
            assert_eq!(loaded.weather_location, *loc, "location {loc:?} must round-trip");
            assert_eq!(loaded.weather_units, "metric");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_apply_config_key_ssh_fields() {
        let mut cfg = Config::default();

        apply_config_key(&mut cfg, "ssh_enabled", "true");
        assert!(cfg.ssh_enabled);

        apply_config_key(&mut cfg, "ssh_enabled", "false");
        assert!(!cfg.ssh_enabled);

        apply_config_key(&mut cfg, "ssh_port", "3333");
        assert_eq!(cfg.ssh_port, 3333);

        // Invalid port should be ignored
        apply_config_key(&mut cfg, "ssh_port", "notanumber");
        assert_eq!(cfg.ssh_port, 3333);

        // Legacy ssh_username / ssh_password keys are no longer
        // recognized by apply_config_key — they only get a one-time
        // migration at file-load time (covered by the dedicated
        // migration tests below).  Confirm that pushing them through
        // apply_config_key is a silent no-op and does NOT alter the
        // unified `username` / `password` field.
        let before_user = cfg.username.clone();
        let before_pass = cfg.password.clone();
        apply_config_key(&mut cfg, "ssh_username", "sshuser");
        apply_config_key(&mut cfg, "ssh_password", "sshpass");
        assert_eq!(cfg.username, before_user);
        assert_eq!(cfg.password, before_pass);
    }

    #[test]
    fn test_legacy_ssh_credential_migration() {
        // Upgrading from a pre-merge config: the file has the legacy
        // ssh_username / ssh_password keys with non-default values
        // and the unified pair is still at the factory default.  On
        // load we should adopt the legacy SSH values into the
        // unified pair so the operator's working SSH login keeps
        // working until they change it deliberately.
        let dir = std::env::temp_dir().join("xmodem_test_ssh_cred_migration");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("legacy.conf");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "ssh_username = oldsshuser").unwrap();
        writeln!(f, "ssh_password = oldsshpass").unwrap();
        drop(f);

        let cfg = read_config_file(path.to_str().unwrap());
        assert_eq!(cfg.username, "oldsshuser");
        assert_eq!(cfg.password, "oldsshpass");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_legacy_ssh_migration_does_not_overwrite_custom_creds() {
        // If the operator already customized the telnet (now unified)
        // username/password, the legacy SSH migration must NOT
        // clobber it — the unified pair wins because the operator
        // explicitly set it.
        let dir = std::env::temp_dir().join("xmodem_test_ssh_no_overwrite");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("both.conf");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "username = customuser").unwrap();
        writeln!(f, "password = custompass").unwrap();
        writeln!(f, "ssh_username = ignoreme").unwrap();
        writeln!(f, "ssh_password = ignoremetoo").unwrap();
        drop(f);

        let cfg = read_config_file(path.to_str().unwrap());
        assert_eq!(cfg.username, "customuser");
        assert_eq!(cfg.password, "custompass");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_save_drops_legacy_ssh_credential_keys() {
        // After a save the on-disk file must NOT contain
        // ssh_username / ssh_password keys — the unified pair is the
        // only credential surface going forward.
        let dir = std::env::temp_dir().join("xmodem_test_ssh_drop_legacy");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("written.conf");
        write_config_file(path.to_str().unwrap(), &Config::default()).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(
            !written.contains("ssh_username"),
            "writer still emits legacy ssh_username"
        );
        assert!(
            !written.contains("ssh_password"),
            "writer still emits legacy ssh_password"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_apply_config_key_telnet_fields() {
        let mut cfg = Config::default();

        apply_config_key(&mut cfg, "telnet_enabled", "false");
        assert!(!cfg.telnet_enabled);

        apply_config_key(&mut cfg, "telnet_enabled", "true");
        assert!(cfg.telnet_enabled);

        apply_config_key(&mut cfg, "telnet_port", "8080");
        assert_eq!(cfg.telnet_port, 8080);

        // Invalid port should be ignored
        apply_config_key(&mut cfg, "telnet_port", "notanumber");
        assert_eq!(cfg.telnet_port, 8080);

        apply_config_key(&mut cfg, "enable_console", "false");
        assert!(!cfg.enable_console);

        apply_config_key(&mut cfg, "enable_console", "true");
        assert!(cfg.enable_console);
    }

    #[test]
    fn test_apply_config_key_kermit_server_fields() {
        let mut cfg = Config::default();
        // Defaults: disabled, port 2424.
        assert!(!cfg.kermit_server_enabled);
        assert_eq!(cfg.kermit_server_port, 2424);

        apply_config_key(&mut cfg, "kermit_server_enabled", "true");
        assert!(cfg.kermit_server_enabled);
        apply_config_key(&mut cfg, "kermit_server_enabled", "false");
        assert!(!cfg.kermit_server_enabled);

        apply_config_key(&mut cfg, "kermit_server_port", "2525");
        assert_eq!(cfg.kermit_server_port, 2525);

        // Invalid (non-numeric) ignored.
        apply_config_key(&mut cfg, "kermit_server_port", "notanumber");
        assert_eq!(cfg.kermit_server_port, 2525);
        // Zero ignored (port must be ≥ 1).
        apply_config_key(&mut cfg, "kermit_server_port", "0");
        assert_eq!(cfg.kermit_server_port, 2525);
    }

    #[test]
    fn test_apply_config_key_web_fields() {
        let mut cfg = Config::default();
        // Defaults: disabled, port 8080.
        assert!(!cfg.web_enabled);
        assert_eq!(cfg.web_port, 8080);

        apply_config_key(&mut cfg, "web_enabled", "true");
        assert!(cfg.web_enabled);
        apply_config_key(&mut cfg, "web_enabled", "false");
        assert!(!cfg.web_enabled);

        apply_config_key(&mut cfg, "web_port", "9090");
        assert_eq!(cfg.web_port, 9090);

        // Invalid (non-numeric) ignored.
        apply_config_key(&mut cfg, "web_port", "notanumber");
        assert_eq!(cfg.web_port, 9090);
        // Zero ignored (port must be ≥ 1).
        apply_config_key(&mut cfg, "web_port", "0");
        assert_eq!(cfg.web_port, 9090);
    }

    #[test]
    fn test_apply_config_key_cpm_emu_enabled() {
        let mut cfg = Config::default();
        // On by default; the key still has to turn it off cleanly, which is
        // what an operator who does not want guest code will use.
        assert!(cfg.cpm_emu_enabled);

        apply_config_key(&mut cfg, "cpm_emu_enabled", "true");
        assert!(cfg.cpm_emu_enabled);
        apply_config_key(&mut cfg, "cpm_emu_enabled", "false");
        assert!(!cfg.cpm_emu_enabled);
    }

    #[test]
    fn test_apply_config_key_cpm_emu_max_minstr() {
        let mut cfg = Config::default();
        assert_eq!(cfg.cpm_emu_max_minstr, DEFAULT_CPM_EMU_MAX_MINSTR);

        apply_config_key(&mut cfg, "cpm_emu_max_minstr", "500");
        assert_eq!(cfg.cpm_emu_max_minstr, 500);
        // Zero and non-numeric are rejected (>= 1 guard), value unchanged.
        apply_config_key(&mut cfg, "cpm_emu_max_minstr", "0");
        assert_eq!(cfg.cpm_emu_max_minstr, 500);
        apply_config_key(&mut cfg, "cpm_emu_max_minstr", "abc");
        assert_eq!(cfg.cpm_emu_max_minstr, 500);

        // Above the cap is **clamped, not refused** — the whole point of the
        // bound. Refusing would leave 500 here and, from a config file, drop an
        // operator asking for "no limit" to the 2000 default and write that
        // over their setting.
        apply_config_key(&mut cfg, "cpm_emu_max_minstr", &u32::MAX.to_string());
        assert_eq!(cfg.cpm_emu_max_minstr, MAX_CPM_EMU_MAX_MINSTR);
        apply_config_key(&mut cfg, "cpm_emu_max_minstr", "1000001");
        assert_eq!(cfg.cpm_emu_max_minstr, MAX_CPM_EMU_MAX_MINSTR);
        // The cap itself is allowed through untouched.
        apply_config_key(&mut cfg, "cpm_emu_max_minstr", &MAX_CPM_EMU_MAX_MINSTR.to_string());
        assert_eq!(cfg.cpm_emu_max_minstr, MAX_CPM_EMU_MAX_MINSTR);
    }

    /// A config file written before the cap existed must not lose its setting.
    ///
    /// This is the case the cap was chosen *for*: `cpm_emu_max_minstr` ships in
    /// released versions, so a file out there can hold any `u32`. Clamping keeps
    /// what the operator meant; the rejecting form every other bounded key uses
    /// would silently substitute the 2000 default and then write it back over
    /// their file the next time anything saved.
    #[test]
    fn test_an_existing_oversized_ceiling_is_capped_not_reset() {
        let dir = std::env::temp_dir().join(format!("egw_ceiling_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("egateway.conf");
        std::fs::write(&path, "cpm_emu_max_minstr = 4000000000\n").expect("write");

        let cfg = read_config_file_checked(path.to_str().unwrap()).expect("read");
        assert_eq!(
            cfg.cpm_emu_max_minstr, MAX_CPM_EMU_MAX_MINSTR,
            "an oversized ceiling must be capped at the maximum"
        );
        assert_ne!(
            cfg.cpm_emu_max_minstr, DEFAULT_CPM_EMU_MAX_MINSTR,
            "it must NOT fall back to the default — that is the data loss this \
             clamp exists to avoid"
        );

        // Zero keeps its old meaning: unreadable, use the default.  Existing
        // behaviour, and the clamp must not have quietly turned it into 1.
        std::fs::write(&path, "cpm_emu_max_minstr = 0\n").expect("write");
        let cfg = read_config_file_checked(path.to_str().unwrap()).expect("read");
        assert_eq!(cfg.cpm_emu_max_minstr, DEFAULT_CPM_EMU_MAX_MINSTR);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The CPU is validated where it is written for the same reason as the two
    /// settings below, and one reason of its own: `is_8080` reads anything it
    /// does not recognise as the Z80, so an unchecked write would leave a
    /// config file claiming an 8080 while both machines ran a Z80.
    #[test]
    fn test_apply_config_key_cpm_cpu() {
        let mut cfg = Config::default();
        assert_eq!(
            cfg.cpm_cpu,
            crate::cpm::cpu::CPU_Z80,
            "a fresh config runs the Z80 — the superset that runs every disk here"
        );

        // Both offered processors are settable, iterated rather than hand-typed
        // so a third could not be added without reaching this key.
        for (value, _) in crate::cpm::cpu::CPU_CHOICES {
            apply_config_key(&mut cfg, "cpm_cpu", value);
            assert_eq!(cfg.cpm_cpu, *value);
        }

        // Anything else is refused rather than stored.
        cfg.cpm_cpu = crate::cpm::cpu::CPU_8080.to_string();
        for bad in ["", "z-80", "8085", "Z80 ", "nonsense"] {
            apply_config_key(&mut cfg, "cpm_cpu", bad);
            assert_eq!(cfg.cpm_cpu, crate::cpm::cpu::CPU_8080, "{bad:?} must be refused");
        }

        // It survives a save/load cycle, and an absent key means the Z80 —
        // which is what every config written before this key existed was
        // already running, so an upgrade changes nothing.
        let dir = std::env::temp_dir().join("egw_test_cpm_cpu_rt");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("cpu.conf");
        let path = file.to_str().unwrap();
        write_config_file(path, &cfg).unwrap();
        assert_eq!(read_config_file(path).cpm_cpu, crate::cpm::cpu::CPU_8080);
        let text = std::fs::read_to_string(path).unwrap();
        let stripped: String = text
            .lines()
            .filter(|l| !l.trim_start().starts_with("cpm_cpu"))
            .map(|l| format!("{l}\n"))
            .collect();
        std::fs::write(path, stripped).unwrap();
        assert_eq!(
            read_config_file(path).cpm_cpu,
            crate::cpm::cpu::CPU_Z80,
            "a config written before this key existed must get the Z80"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The boot setting is validated where it is written, not only where it is
    /// used: it can arrive from a web form, so it is shaped by whoever posted
    /// it, and a name that could never be opened has no business reaching the
    /// config file.
    #[test]
    fn test_apply_config_key_cpm_boot_machine() {
        let mut cfg = Config::default();
        assert_eq!(
            cfg.cpm_boot_machine,
            crate::cpm::console::AUTO_MACHINE,
            "a fresh config detects the machine from the disk"
        );
        // `auto` is a policy rather than a machine, so it is not in the choice
        // list and has to be accepted explicitly.
        apply_config_key(&mut cfg, "cpm_boot_machine", "altair_sio");
        apply_config_key(&mut cfg, "cpm_boot_machine", crate::cpm::console::AUTO_MACHINE);
        assert_eq!(cfg.cpm_boot_machine, crate::cpm::console::AUTO_MACHINE);
        // And it survives being written out and read back, which is what an
        // upgrade does on its first save.  A default that could not round-trip
        // would silently become something else.
        let dir = std::env::temp_dir().join("egw_test_auto_machine_rt");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("auto.conf");
        let path = file.to_str().unwrap();
        write_config_file(path, &cfg).unwrap();
        let back = read_config_file(path);
        assert_eq!(
            back.cpm_boot_machine,
            crate::cpm::console::AUTO_MACHINE,
            "the default must survive a save/load cycle"
        );
        // A config file with the key ABSENT must also mean detection: that is an
        // upgrade from a version before the key existed.
        let text = std::fs::read_to_string(path).unwrap();
        let stripped: String = text
            .lines()
            .filter(|l| !l.trim_start().starts_with("cpm_boot_machine"))
            .map(|l| format!("{l}\n"))
            .collect();
        std::fs::write(path, stripped).unwrap();
        assert_eq!(
            read_config_file(path).cpm_boot_machine,
            crate::cpm::console::AUTO_MACHINE,
            "a config written before this key existed must get detection"
        );
        let _ = std::fs::remove_dir_all(&dir);

        // Every machine in the shared list must be settable — iterated rather
        // than typed out, so a new one cannot be added without being accepted
        // here.
        for c in crate::cpm::console::MACHINE_CHOICES {
            apply_config_key(&mut cfg, "cpm_boot_machine", c.key);
            assert_eq!(cfg.cpm_boot_machine, c.key);
        }

        // Anything else is refused, leaving the value alone. The web form is the
        // reason this matters: the value arrives shaped by whoever posted it, and
        // a config file that names a machine we are not is worse than one that
        // names the default.
        cfg.cpm_boot_machine = "console_04".to_string();
        for bad in ["", "bogus", "ALTAIR_2SIO", "console_04 ", "altair"] {
            apply_config_key(&mut cfg, "cpm_boot_machine", bad);
            assert_eq!(cfg.cpm_boot_machine, "console_04", "{bad:?} must be refused");
        }
    }

    #[test]
    fn test_apply_config_key_cpm_boot_image() {
        let mut cfg = Config::default();
        assert_eq!(cfg.cpm_boot_image, "", "the emulator by default");

        apply_config_key(&mut cfg, "cpm_boot_image", "HDSK04.DSK");
        assert_eq!(cfg.cpm_boot_image, "HDSK04.DSK");

        // Empty is always allowed — it means "run the emulator".
        apply_config_key(&mut cfg, "cpm_boot_image", "");
        assert_eq!(cfg.cpm_boot_image, "");

        // Anything that is not a bare filename is refused, leaving the value
        // alone.  These are the shapes a path traversal takes.
        cfg.cpm_boot_image = "good.dsk".to_string();
        for bad in ["../../etc/passwd", "/etc/passwd", "sub/dir.dsk", "..", ".hidden.dsk"] {
            apply_config_key(&mut cfg, "cpm_boot_image", bad);
            assert_eq!(cfg.cpm_boot_image, "good.dsk", "{bad:?} must be refused");
        }
    }

    #[test]
    fn test_apply_config_key_cpm_emu_uart() {
        let mut cfg = Config::default();
        assert_eq!(cfg.cpm_emu_uart, "rc2014_1b"); // default: EGT8080's port

        apply_config_key(&mut cfg, "cpm_emu_uart", "rc2014_1b");
        assert_eq!(cfg.cpm_emu_uart, "rc2014_1b");
        apply_config_key(&mut cfg, "cpm_emu_uart", "altair_sio");
        assert_eq!(cfg.cpm_emu_uart, "altair_sio");
        // An unknown profile is rejected; the value is unchanged.
        apply_config_key(&mut cfg, "cpm_emu_uart", "bogus");
        assert_eq!(cfg.cpm_emu_uart, "altair_sio");
    }

    #[test]
    fn test_apply_config_key_security_fields() {
        let mut cfg = Config::default();

        apply_config_key(&mut cfg, "security_enabled", "true");
        assert!(cfg.security_enabled);
        apply_config_key(&mut cfg, "security_enabled", "false");
        assert!(!cfg.security_enabled);

        apply_config_key(&mut cfg, "disable_ip_safety", "true");
        assert!(cfg.disable_ip_safety);
        apply_config_key(&mut cfg, "disable_ip_safety", "false");
        assert!(!cfg.disable_ip_safety);
        // Case-insensitive parse, mirroring the read_config_file path.
        apply_config_key(&mut cfg, "disable_ip_safety", "TRUE");
        assert!(cfg.disable_ip_safety);

        apply_config_key(&mut cfg, "username", "myuser");
        assert_eq!(cfg.username, "myuser");

        apply_config_key(&mut cfg, "password", "mypass");
        assert_eq!(cfg.password, "mypass");
    }

    #[test]
    fn test_apply_config_key_xmodem_fields() {
        let mut cfg = Config::default();

        apply_config_key(&mut cfg, "transfer_dir", "/tmp/files");
        assert_eq!(cfg.transfer_dir, "/tmp/files");

        apply_config_key(&mut cfg, "xmodem_negotiation_timeout", "60");
        assert_eq!(cfg.xmodem_negotiation_timeout, 60);

        apply_config_key(&mut cfg, "xmodem_block_timeout", "30");
        assert_eq!(cfg.xmodem_block_timeout, 30);

        apply_config_key(&mut cfg, "xmodem_max_retries", "15");
        assert_eq!(cfg.xmodem_max_retries, 15);

        apply_config_key(&mut cfg, "xmodem_negotiation_retry_interval", "9");
        assert_eq!(cfg.xmodem_negotiation_retry_interval, 9);
        // Zero rejected (min 1)
        apply_config_key(&mut cfg, "xmodem_negotiation_retry_interval", "0");
        assert_eq!(cfg.xmodem_negotiation_retry_interval, 9);

        // Invalid values should be ignored
        apply_config_key(&mut cfg, "xmodem_negotiation_timeout", "notanumber");
        assert_eq!(cfg.xmodem_negotiation_timeout, 60);

        apply_config_key(&mut cfg, "zmodem_negotiation_timeout", "90");
        assert_eq!(cfg.zmodem_negotiation_timeout, 90);

        apply_config_key(&mut cfg, "zmodem_frame_timeout", "45");
        assert_eq!(cfg.zmodem_frame_timeout, 45);

        apply_config_key(&mut cfg, "zmodem_max_retries", "7");
        assert_eq!(cfg.zmodem_max_retries, 7);

        apply_config_key(&mut cfg, "zmodem_negotiation_retry_interval", "8");
        assert_eq!(cfg.zmodem_negotiation_retry_interval, 8);
        apply_config_key(&mut cfg, "zmodem_negotiation_retry_interval", "0");
        assert_eq!(cfg.zmodem_negotiation_retry_interval, 8);

        // Invalid zmodem values ignored; zero also rejected (min >=1)
        apply_config_key(&mut cfg, "zmodem_frame_timeout", "0");
        assert_eq!(cfg.zmodem_frame_timeout, 45);
        apply_config_key(&mut cfg, "zmodem_max_retries", "abc");
        assert_eq!(cfg.zmodem_max_retries, 7);
        apply_config_key(&mut cfg, "zmodem_negotiation_timeout", "0");
        assert_eq!(cfg.zmodem_negotiation_timeout, 90);
        apply_config_key(&mut cfg, "zmodem_negotiation_timeout", "-5");
        assert_eq!(cfg.zmodem_negotiation_timeout, 90);
    }

    /// The zmodem_* defaults must match the values that were hardcoded
    /// as `FRAME_TIMEOUT_SECS` / `Z*_MAX_RETRIES` constants in zmodem.rs
    /// before those became runtime-configurable.  If someone ever tweaks
    /// these defaults they should do so deliberately — the assertion
    /// below forces that decision to be explicit rather than accidental.
    #[test]
    fn test_zmodem_defaults_match_previously_hardcoded_values() {
        let cfg = Config::default();
        assert_eq!(
            cfg.zmodem_negotiation_timeout, 45,
            "default must match xmodem_negotiation_timeout which was the prior source"
        );
        assert_eq!(
            cfg.zmodem_frame_timeout, 30,
            "default must match the previously-hardcoded FRAME_TIMEOUT_SECS"
        );
        assert_eq!(
            cfg.zmodem_max_retries, 10,
            "default must match the previously-hardcoded Z*_MAX_RETRIES"
        );
    }

    /// Reading a config file that includes zmodem keys must round-trip
    /// those keys into the Config struct.  Separate from the full
    /// write/reread test so a failure here localizes to zmodem parsing.
    #[test]
    fn test_read_config_parses_zmodem_keys() {
        let dir = std::env::temp_dir().join("xmodem_test_zmodem_keys");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("z.conf");
        let mut f = std::fs::File::create(&path).unwrap();
        use std::io::Write;
        writeln!(f, "zmodem_negotiation_timeout = 77").unwrap();
        writeln!(f, "zmodem_frame_timeout = 22").unwrap();
        writeln!(f, "zmodem_max_retries = 4").unwrap();
        drop(f);

        let cfg = read_config_file(path.to_str().unwrap());
        assert_eq!(cfg.zmodem_negotiation_timeout, 77);
        assert_eq!(cfg.zmodem_frame_timeout, 22);
        assert_eq!(cfg.zmodem_max_retries, 4);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// When zmodem keys are absent from the file, defaults kick in.
    /// Covers the rollout case where an existing egateway.conf predates
    /// the zmodem_* additions.
    #[test]
    fn test_read_config_missing_zmodem_keys_fall_back_to_defaults() {
        let dir = std::env::temp_dir().join("xmodem_test_zmodem_missing");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("legacy.conf");
        let mut f = std::fs::File::create(&path).unwrap();
        use std::io::Write;
        // Pre-zmodem config: only the xmodem-family keys.
        writeln!(f, "xmodem_negotiation_timeout = 99").unwrap();
        drop(f);

        let cfg = read_config_file(path.to_str().unwrap());
        let defaults = Config::default();
        assert_eq!(cfg.xmodem_negotiation_timeout, 99);
        assert_eq!(cfg.zmodem_negotiation_timeout, defaults.zmodem_negotiation_timeout);
        assert_eq!(cfg.zmodem_frame_timeout, defaults.zmodem_frame_timeout);
        assert_eq!(cfg.zmodem_max_retries, defaults.zmodem_max_retries);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_apply_config_key_other_fields() {
        let mut cfg = Config::default();

        apply_config_key(&mut cfg, "groq_api_key", "gsk_test123");
        assert_eq!(cfg.groq_api_key, "gsk_test123");

        apply_config_key(&mut cfg, "browser_homepage", "http://example.com");
        assert_eq!(cfg.browser_homepage, "http://example.com");

        apply_config_key(&mut cfg, "verbose", "true");
        assert!(cfg.verbose);
        apply_config_key(&mut cfg, "verbose", "false");
        assert!(!cfg.verbose);

        apply_config_key(&mut cfg, "max_sessions", "100");
        assert_eq!(cfg.max_sessions, 100);

        apply_config_key(&mut cfg, "idle_timeout_secs", "1800");
        assert_eq!(cfg.idle_timeout_secs, 1800);
    }

    // ─── sanitize_value ─────────────────────────────────

    #[test]
    fn test_sanitize_value_clean() {
        assert_eq!(sanitize_value("hello"), "hello");
    }

    #[test]
    fn test_sanitize_value_strips_newlines() {
        assert_eq!(sanitize_value("line1\nline2"), "line1line2");
    }

    #[test]
    fn test_sanitize_value_strips_cr() {
        assert_eq!(sanitize_value("line1\rline2"), "line1line2");
    }

    #[test]
    fn test_sanitize_value_strips_crlf() {
        assert_eq!(sanitize_value("a\r\nb"), "ab");
    }

    #[test]
    fn test_sanitize_value_empty() {
        assert_eq!(sanitize_value(""), "");
    }

    #[test]
    fn test_sanitize_value_trims_surrounding_whitespace() {
        assert_eq!(sanitize_value("  pw  "), "pw");
        assert_eq!(sanitize_value("\t spaced \t"), "spaced");
        // Newlines are stripped first, then surrounding whitespace trimmed.
        assert_eq!(sanitize_value("  a\nb  "), "ab");
        // Interior whitespace is preserved.
        assert_eq!(sanitize_value("hello world"), "hello world");
    }

    /// A string value with surrounding whitespace round-trips to its trimmed
    /// form consistently: write trims (sanitize_value) to match the read-side
    /// trim, so load→save→load is stable instead of silently mutating across
    /// one save cycle.
    #[test]
    fn test_string_value_roundtrip_is_stable() {
        let dir = std::env::temp_dir().join("egw_test_ws_roundtrip");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test.conf");
        let p = path.to_str().unwrap();

        let cfg = Config {
            password: "secret ".to_string(),
            username: "  admin".to_string(),
            ..Config::default()
        };
        write_config_file(p, &cfg).unwrap();

        let loaded = read_config_file(p);
        assert_eq!(loaded.password, "secret");
        assert_eq!(loaded.username, "admin");

        // Re-saving the loaded config and reloading yields identical values.
        write_config_file(p, &loaded).unwrap();
        let reloaded = read_config_file(p);
        assert_eq!(reloaded.password, "secret");
        assert_eq!(reloaded.username, "admin");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A dialup host containing a newline must not corrupt the line-based
    /// framing: `save_dialup_mappings` sanitizes number/host the same way
    /// `write_config_file` sanitizes values, keeping each entry on one line
    /// and parsing back cleanly.
    #[test]
    fn test_dialup_host_sanitized_keeps_framing() {
        let nasty = "evil\nhost";
        // Reconstruct the exact line save_dialup_mappings builds.
        let line = format!(
            "{} = {}:{}\n",
            sanitize_value("5551234"),
            sanitize_value(nasty),
            23u16
        );
        assert!(
            !line.trim_end_matches('\n').contains('\n'),
            "sanitized entry must stay on a single line"
        );
        let parsed = parse_dialup_mappings(&line);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].number, "5551234");
        assert_eq!(parsed[0].host, "evilhost");
        assert_eq!(parsed[0].port, 23);
    }

    // ─── Dialup mapping tests ─────────────────────────────

    /// A read failure must not look like "no mappings".
    ///
    /// `save_dialup_mappings` rewrites the file wholesale, so a
    /// read-modify-write starting from a silently-empty list would replace
    /// every existing mapping with just the one being added. The mutating
    /// caller (the telnet "add mapping" screen) refuses on `Err`; only a
    /// genuinely absent file may yield an empty list.
    #[test]
    fn test_dialup_entries_from_read_separates_unreadable_from_absent() {
        // Readable: parsed as usual.
        let ok = dialup_entries_from_read(Ok(
            "5551234 = bbs.example.com:23\n".to_string()
        ))
        .expect("a readable file must not error");
        assert_eq!(ok.len(), 1);
        assert_eq!(ok[0].host, "bbs.example.com");

        // Vanished between the exists() check and the open: really no entries.
        let gone = dialup_entries_from_read(Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "vanished",
        )))
        .expect("an absent file is not a failure");
        assert!(gone.is_empty());

        // Present but unreadable: must be an error, never an empty Vec.
        for kind in [
            std::io::ErrorKind::PermissionDenied,
            std::io::ErrorKind::InvalidData,
        ] {
            let err = dialup_entries_from_read(Err(std::io::Error::new(kind, "boom")));
            assert!(
                err.is_err(),
                "{:?} must surface as an error, or a later save wipes the file",
                kind,
            );
        }
    }

    #[test]
    fn test_parse_dialup_mappings_basic() {
        let content = "5551234 = bbs.example.com:23\n8675309 = retro.host:2323\n";
        let entries = parse_dialup_mappings(content);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].number, "5551234");
        assert_eq!(entries[0].host, "bbs.example.com");
        assert_eq!(entries[0].port, 23);
        assert_eq!(entries[1].number, "8675309");
        assert_eq!(entries[1].host, "retro.host");
        assert_eq!(entries[1].port, 2323);
    }

    #[test]
    fn test_parse_dialup_mappings_default_port() {
        let content = "5551234 = bbs.example.com\n";
        let entries = parse_dialup_mappings(content);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].host, "bbs.example.com");
        assert_eq!(entries[0].port, 23);
    }

    #[test]
    fn test_parse_dialup_mappings_comments_and_blanks() {
        let content = "# A comment\n\n5551234 = host:80\n  # Another comment\n";
        let entries = parse_dialup_mappings(content);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].number, "5551234");
    }

    #[test]
    fn test_parse_dialup_mappings_empty() {
        let entries = parse_dialup_mappings("");
        assert!(entries.is_empty());
    }

    #[test]
    fn test_parse_dialup_mappings_skip_invalid() {
        let content = "= host:80\n5551234 =\nno_equals_sign\n";
        let entries = parse_dialup_mappings(content);
        assert!(entries.is_empty());
    }

    #[test]
    fn test_parse_dialup_mappings_port_zero_defaults() {
        let content = "5551234 = bbs.example.com:0\n";
        let entries = parse_dialup_mappings(content);
        assert_eq!(entries.len(), 1);
        // Port 0 is invalid, so the whole "host:0" is treated as the host
        // and port defaults to 23
        assert_eq!(entries[0].port, 23);
    }

    #[test]
    fn test_parse_dialup_mappings_port_overflow() {
        let content = "5551234 = host:99999\n";
        let entries = parse_dialup_mappings(content);
        assert_eq!(entries.len(), 1);
        // Port overflow fails u16 parse, entire target treated as host
        assert_eq!(entries[0].port, 23);
    }

    #[test]
    fn test_dialup_save_load_roundtrip() {
        let dir = std::env::temp_dir().join("xmodem_test_dialup_rt");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("dialup_rt.conf");
        let path = file.to_str().unwrap();

        let entries = vec![
            DialupEntry { number: "5551234".into(), host: "bbs.example.com".into(), port: 23 },
            DialupEntry { number: "8675309".into(), host: "retro.host".into(), port: 2323 },
        ];

        // Write manually to the temp file
        let mut content = String::new();
        for e in &entries {
            content.push_str(&format!("{} = {}:{}\n", e.number, e.host, e.port));
        }
        std::fs::write(path, &content).unwrap();

        // Parse it back
        let loaded = parse_dialup_mappings(&std::fs::read_to_string(path).unwrap());
        assert_eq!(loaded, entries);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_parse_dialup_mappings_whitespace_tolerance() {
        let content = "  5551234  =  bbs.example.com:23  \n";
        let entries = parse_dialup_mappings(content);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].number, "5551234");
        assert_eq!(entries[0].host, "bbs.example.com");
        assert_eq!(entries[0].port, 23);
    }

    // ─── normalize_phone_number ─────────────────────────

    #[test]
    fn test_gui_zoom_factor_auto_and_empty() {
        let mut cfg = Config::default();
        assert_eq!(cfg.gui_zoom, "auto");
        assert_eq!(cfg.gui_zoom_factor(), None);
        cfg.gui_zoom = "  AUTO  ".into();
        assert_eq!(cfg.gui_zoom_factor(), None);
        cfg.gui_zoom = "".into();
        assert_eq!(cfg.gui_zoom_factor(), None);
    }

    #[test]
    fn test_gui_zoom_factor_numeric_and_clamped() {
        let mut cfg = Config {
            gui_zoom: "1.0".into(),
            ..Default::default()
        };
        assert_eq!(cfg.gui_zoom_factor(), Some(1.0));
        cfg.gui_zoom = " 1.25 ".into();
        assert_eq!(cfg.gui_zoom_factor(), Some(1.25));
        // Out-of-range values clamp into [GUI_ZOOM_MIN, GUI_ZOOM_MAX].
        cfg.gui_zoom = "0.1".into();
        assert_eq!(cfg.gui_zoom_factor(), Some(GUI_ZOOM_MIN));
        cfg.gui_zoom = "99".into();
        assert_eq!(cfg.gui_zoom_factor(), Some(GUI_ZOOM_MAX));
        // Garbage / non-finite falls back to auto (None) rather than panicking.
        cfg.gui_zoom = "big".into();
        assert_eq!(cfg.gui_zoom_factor(), None);
        cfg.gui_zoom = "nan".into();
        assert_eq!(cfg.gui_zoom_factor(), None);
    }

    #[test]
    fn test_gui_zoom_roundtrips_through_conf() {
        let dir = std::env::temp_dir().join("egw_test_gui_zoom_rt");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test.conf");
        let p = path.to_str().unwrap();

        let cfg = Config { gui_zoom: "1.25".into(), ..Config::default() };
        write_config_file(p, &cfg).unwrap();
        let loaded = read_config_file(p);
        assert_eq!(loaded.gui_zoom, "1.25");
        assert_eq!(loaded.gui_zoom_factor(), Some(1.25));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_normalize_phone_number_digits_only() {
        assert_eq!(normalize_phone_number("5551234"), "5551234");
    }

    #[test]
    fn test_normalize_phone_number_strips_formatting() {
        assert_eq!(normalize_phone_number("555-1234"), "5551234");
        assert_eq!(normalize_phone_number("(800) 555-1234"), "8005551234");
        assert_eq!(normalize_phone_number("+1-800-555-1234"), "18005551234");
    }

    #[test]
    fn test_normalize_phone_number_empty() {
        assert_eq!(normalize_phone_number(""), "");
        assert_eq!(normalize_phone_number("---"), "");
    }

    // ─── lookup matching ──────────────────────────────────

    #[test]
    fn test_lookup_dialup_normalized_matching() {
        // "555-5555" should match an entry stored as "5555555"
        let content = "5555555 = bbs.example.com:23\n";
        let entries = parse_dialup_mappings(content);
        assert_eq!(entries.len(), 1);
        assert_eq!(
            normalize_phone_number("555-5555"),
            normalize_phone_number(&entries[0].number)
        );
    }

    #[test]
    fn test_lookup_dialup_formatted_entry_matches_plain() {
        // Entry stored as "555-1234" should match input "5551234"
        let content = "555-1234 = bbs.example.com:23\n";
        let entries = parse_dialup_mappings(content);
        assert_eq!(entries.len(), 1);
        assert_eq!(
            normalize_phone_number("5551234"),
            normalize_phone_number(&entries[0].number)
        );
    }

    #[test]
    fn test_lookup_empty_normalized_returns_none() {
        // A number with no digits should never match anything
        let normalized = normalize_phone_number("---");
        assert!(normalized.is_empty());
    }

    #[test]
    fn test_default_dialup_entry() {
        // The default starter entry should be 1234567 -> telnetbible.com:6400
        let default = DialupEntry {
            number: "1234567".into(),
            host: "telnetbible.com".into(),
            port: 6400,
        };
        assert_eq!(default.number, "1234567");
        assert_eq!(default.host, "telnetbible.com");
        assert_eq!(default.port, 6400);
    }

    // ─── Dual-port migration & round-trip ──────────────────

    /// Legacy single-port `egateway.conf` files use bare `serial_*`
    /// keys with no port prefix.  When the gateway loads such a file
    /// it must auto-migrate every legacy key into Port A while leaving
    /// Port B at defaults — and the next save (covered by the round-
    /// trip tests above) emits the new dual-port form.
    #[test]
    fn test_legacy_serial_keys_migrate_to_port_a() {
        let dir = std::env::temp_dir().join("xmodem_test_serial_legacy_migrate");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("legacy.conf");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "serial_enabled = true").unwrap();
        writeln!(f, "serial_mode = console").unwrap();
        writeln!(f, "serial_port = /dev/ttyUSB0").unwrap();
        writeln!(f, "serial_baud = 38400").unwrap();
        writeln!(f, "serial_databits = 7").unwrap();
        writeln!(f, "serial_parity = even").unwrap();
        writeln!(f, "serial_stopbits = 2").unwrap();
        writeln!(f, "serial_flowcontrol = hardware").unwrap();
        writeln!(f, "serial_echo = false").unwrap();
        writeln!(f, "serial_quiet = true").unwrap();
        writeln!(f, "serial_s_regs = 1,2,3").unwrap();
        writeln!(f, "serial_x_code = 2").unwrap();
        writeln!(f, "serial_dtr_mode = 1").unwrap();
        writeln!(f, "serial_flow_mode = 3").unwrap();
        writeln!(f, "serial_dcd_mode = 0").unwrap();
        writeln!(f, "serial_stored_0 = 5551111").unwrap();
        writeln!(f, "serial_stored_2 = 5553333").unwrap();
        drop(f);

        let cfg = read_config_file(path.to_str().unwrap());

        // Every legacy key landed on Port A.
        assert!(cfg.serial_a.enabled);
        assert_eq!(cfg.serial_a.mode, "console");
        assert_eq!(cfg.serial_a.port, "/dev/ttyUSB0");
        assert_eq!(cfg.serial_a.baud, 38400);
        assert_eq!(cfg.serial_a.databits, 7);
        assert_eq!(cfg.serial_a.parity, "even");
        assert_eq!(cfg.serial_a.stopbits, 2);
        assert_eq!(cfg.serial_a.flowcontrol, "hardware");
        assert!(!cfg.serial_a.echo);
        assert!(cfg.serial_a.quiet);
        assert_eq!(cfg.serial_a.s_regs, "1,2,3");
        assert_eq!(cfg.serial_a.x_code, 2);
        assert_eq!(cfg.serial_a.dtr_mode, 1);
        assert_eq!(cfg.serial_a.flow_mode, 3);
        assert_eq!(cfg.serial_a.dcd_mode, 0);
        assert_eq!(cfg.serial_a.stored_numbers[0], "5551111");
        assert_eq!(cfg.serial_a.stored_numbers[2], "5553333");

        // Port B was untouched — still at defaults.
        assert_eq!(cfg.serial_b, SerialPortConfig::default());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// When both prefixed and legacy keys are present, the prefixed
    /// form wins.  This guards the migration path against silently
    /// reverting after the writer has already emitted the dual-port
    /// form once.
    #[test]
    fn test_serial_a_prefixed_keys_win_over_legacy() {
        let dir = std::env::temp_dir().join("xmodem_test_serial_prefix_wins");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("both.conf");
        let mut f = std::fs::File::create(&path).unwrap();
        // Legacy says baud 9600, prefixed says baud 115200.  Prefixed wins.
        writeln!(f, "serial_baud = 9600").unwrap();
        writeln!(f, "serial_a_baud = 115200").unwrap();
        drop(f);

        let cfg = read_config_file(path.to_str().unwrap());
        assert_eq!(cfg.serial_a.baud, 115200);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A hand-edited config file that mixes legacy and prefixed keys
    /// for Port A — e.g. the user partially migrated the file or
    /// re-introduced a legacy line by accident — must read each field
    /// from whichever form is present, with prefixed winning.  Port A
    /// fields with NO prefixed form fall back to legacy; Port A fields
    /// WITH a prefixed form ignore the legacy duplicate; Port B is
    /// unaffected throughout.
    ///
    /// This is the regression test for an issue that an earlier review
    /// raised in the abstract: the per-field fallback design tolerates
    /// partial migration gracefully but only because every field
    /// independently re-runs the lookup.  A future "optimize the
    /// migration" change that decides "all-or-nothing per port" would
    /// silently regress every hand-edited config that hits this path.
    #[test]
    fn test_serial_partial_prefix_keys_with_legacy_fallback() {
        let dir = std::env::temp_dir().join("xmodem_test_serial_partial_prefix");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("partial.conf");
        let mut f = std::fs::File::create(&path).unwrap();
        // Port A: baud + parity in NEW form, port + enabled in LEGACY
        // form, mode missing entirely (should use default).
        writeln!(f, "serial_a_baud = 9600").unwrap();
        writeln!(f, "serial_a_parity = even").unwrap();
        writeln!(f, "serial_port = /dev/ttyTEST").unwrap(); // legacy form
        writeln!(f, "serial_enabled = true").unwrap();      // legacy form
        // Both prefixed AND legacy parity present — prefixed must win.
        writeln!(f, "serial_parity = odd").unwrap();        // legacy duplicate
        // Port B has nothing — must stay at defaults regardless of
        // legacy keys (Port B never migrates).
        drop(f);

        let cfg = read_config_file(path.to_str().unwrap());

        // Port A: prefixed where present, legacy where absent.
        assert_eq!(cfg.serial_a.baud, 9600, "prefixed baud");
        assert_eq!(cfg.serial_a.parity, "even", "prefixed parity wins over legacy");
        assert_eq!(cfg.serial_a.port, "/dev/ttyTEST", "legacy port falls back");
        assert!(cfg.serial_a.enabled, "legacy enabled falls back");
        // Field with neither prefixed nor legacy → default.
        assert_eq!(cfg.serial_a.mode, DEFAULT_SERIAL_MODE);

        // Port B is fully default — legacy keys must NOT bleed across.
        assert_eq!(cfg.serial_b, SerialPortConfig::default());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Port B never falls back to legacy keys.  The legacy migration
    /// is Port-A-only by design — the legacy single-port file never
    /// described two ports.
    #[test]
    fn test_serial_b_does_not_migrate_legacy_keys() {
        let dir = std::env::temp_dir().join("xmodem_test_serial_b_no_migrate");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("b.conf");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "serial_enabled = true").unwrap();
        writeln!(f, "serial_baud = 38400").unwrap();
        drop(f);

        let cfg = read_config_file(path.to_str().unwrap());
        // Port A picked the legacy keys up.
        assert!(cfg.serial_a.enabled);
        assert_eq!(cfg.serial_a.baud, 38400);
        // Port B did NOT — legacy keys are Port A's domain only.
        assert!(!cfg.serial_b.enabled);
        assert_eq!(cfg.serial_b.baud, DEFAULT_SERIAL_BAUD);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Port-B-only round-trip: write a file with Port B configured,
    /// read it back, and confirm every Port B field survives.  This
    /// is a direct guard against the writer or reader silently
    /// shadowing Port B onto Port A.
    #[test]
    fn test_serial_b_round_trip() {
        let dir = std::env::temp_dir().join("xmodem_test_serial_b_rt");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("b.conf");

        let original = Config {
            serial_b: SerialPortConfig {
                enabled: true,
                mode: "console".into(),
                port: "/dev/ttyUSB1".into(),
                baud: 57600,
                databits: 7,
                parity: "odd".into(),
                stopbits: 2,
                flowcontrol: "software".into(),
                echo: false,
                verbose: false,
                quiet: true,
                s_regs: "9,8,7".into(),
                x_code: 1,
                dtr_mode: 3,
                flow_mode: 4,
                dcd_mode: 0,
                stored_numbers: [
                    "B-zero".into(),
                    String::new(),
                    "B-two".into(),
                    "B-three".into(),
                ],
                petscii_translate: true,
                drive_carrier: true,
            },
            ..Config::default()
        };
        write_config_file(path.to_str().unwrap(), &original).unwrap();
        let loaded = read_config_file(path.to_str().unwrap());

        assert_eq!(loaded.serial_b, original.serial_b);
        // Port A stayed at defaults — writing Port B doesn't bleed.
        assert_eq!(loaded.serial_a, SerialPortConfig::default());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `serial_key` produces the canonical persistence key for any
    /// (port, suffix) pair.  Tested directly because the modem AT&W
    /// path and the telnet revert helper both build keys this way and
    /// a typo in one place (e.g. `serial_a_baud` vs `serial_a-baud`)
    /// would silently fail to persist.
    #[test]
    fn test_serial_key_format() {
        assert_eq!(serial_key(SerialPortId::A, "baud"), "serial_a_baud");
        assert_eq!(serial_key(SerialPortId::B, "stored_3"), "serial_b_stored_3");
    }

    /// Every `apply_serial_port_key` suffix that the AT&W path emits
    /// must round-trip via `serial_key()` for both ports.  This is the
    /// single integration point where a typo in either side (modem
    /// emulator AT&W persistence keys vs. config schema) would cause
    /// silent persistence failure.
    #[test]
    fn test_serial_key_round_trips_through_apply_for_both_ports() {
        // Suffixes the AT&W path persists (matches the call list in
        // serial.rs::process_at_command for AtResult::SaveConfig).
        let suffixes: &[(&str, &str)] = &[
            ("enabled", "true"),
            ("mode", "console"),
            ("port", "/dev/ttyTEST"),
            ("baud", "57600"),
            ("databits", "7"),
            ("parity", "even"),
            ("stopbits", "2"),
            ("flowcontrol", "hardware"),
            ("echo", "false"),
            ("verbose", "false"),
            ("quiet", "true"),
            ("s_regs", "9,8,7,6,5,4,3,2,1"),
            ("x_code", "2"),
            ("dtr_mode", "3"),
            ("flow_mode", "4"),
            ("dcd_mode", "0"),
            ("stored_0", "111"),
            ("stored_1", "222"),
            ("stored_2", "333"),
            ("stored_3", "444"),
            ("petscii_translate", "true"),
            ("drive_carrier", "true"),
        ];

        for &id in &[SerialPortId::A, SerialPortId::B] {
            let mut cfg = Config::default();
            for (suffix, value) in suffixes {
                let key = serial_key(id, suffix);
                apply_config_key(&mut cfg, &key, value);
            }
            // Every field on the targeted port should now reflect the
            // applied value.
            let port = cfg.port(id);
            assert!(port.enabled, "port {} enabled", id.label());
            assert_eq!(port.mode, "console");
            assert_eq!(port.port, "/dev/ttyTEST");
            assert_eq!(port.baud, 57600);
            assert_eq!(port.databits, 7);
            assert_eq!(port.parity, "even");
            assert_eq!(port.stopbits, 2);
            assert_eq!(port.flowcontrol, "hardware");
            assert!(!port.echo);
            assert!(!port.verbose);
            assert!(port.quiet);
            assert_eq!(port.s_regs, "9,8,7,6,5,4,3,2,1");
            assert_eq!(port.x_code, 2);
            assert_eq!(port.dtr_mode, 3);
            assert_eq!(port.flow_mode, 4);
            assert_eq!(port.dcd_mode, 0);
            assert_eq!(port.stored_numbers[0], "111");
            assert_eq!(port.stored_numbers[1], "222");
            assert_eq!(port.stored_numbers[2], "333");
            assert_eq!(port.stored_numbers[3], "444");
            assert!(port.petscii_translate);
            assert!(port.drive_carrier);

            // The OTHER port must still be at defaults — proves no
            // cross-contamination.
            let other = match id {
                SerialPortId::A => &cfg.serial_b,
                SerialPortId::B => &cfg.serial_a,
            };
            assert_eq!(*other, SerialPortConfig::default(), "port {} bled into other", id.label());
        }
    }

    /// A full AT&W → file-write → file-read cycle preserves Port A's
    /// values without ever touching Port B's keys, and vice versa.
    /// This is the load-bearing invariant for the entire dual-port
    /// design — if it ever broke, both ports' AT&W would
    /// non-deterministically corrupt each other's persisted state.
    #[test]
    fn test_atw_persistence_cycle_is_isolated() {
        let dir = std::env::temp_dir().join("xmodem_test_atw_isolation");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("isolated.conf");

        // Start with both ports at non-default values so the test
        // detects accidental defaulting as well as cross-pollution.
        let original = Config {
            serial_a: SerialPortConfig {
                enabled: true,
                mode: "modem".into(),
                port: "/dev/ttyA".into(),
                baud: 4800,
                s_regs: "1,1,1".into(),
                x_code: 1,
                ..SerialPortConfig::default()
            },
            serial_b: SerialPortConfig {
                enabled: true,
                mode: "modem".into(),
                port: "/dev/ttyB".into(),
                baud: 19200,
                s_regs: "2,2,2".into(),
                x_code: 2,
                ..SerialPortConfig::default()
            },
            ..Config::default()
        };
        write_config_file(path.to_str().unwrap(), &original).unwrap();

        // Simulate Port A's AT&W changing only Port A's saved fields.
        // We bypass the global singleton by mutating a fresh in-memory
        // Config — we want this test to be hermetic, not depend on the
        // global mutex state.  (The full real path through
        // `update_config_values` is exercised by the round-trip tests
        // above; this test pins the *isolation* property.)
        let mut after_a_atw = read_config_file(path.to_str().unwrap());
        for (suffix, value) in &[
            ("echo", "false"),
            ("verbose", "false"),
            ("quiet", "true"),
            ("s_regs", "55,55,55"),
            ("x_code", "4"),
        ] {
            apply_config_key(&mut after_a_atw, &serial_key(SerialPortId::A, suffix), value);
        }
        write_config_file(path.to_str().unwrap(), &after_a_atw).unwrap();

        let reloaded = read_config_file(path.to_str().unwrap());

        // Port A's AT&W fields changed.
        assert_eq!(reloaded.serial_a.s_regs, "55,55,55");
        assert_eq!(reloaded.serial_a.x_code, 4);
        assert!(!reloaded.serial_a.echo);
        assert!(!reloaded.serial_a.verbose);
        assert!(reloaded.serial_a.quiet);
        // Port A's other fields untouched.
        assert_eq!(reloaded.serial_a.port, "/dev/ttyA");
        assert_eq!(reloaded.serial_a.baud, 4800);
        // Port B is byte-identical to its pre-AT&W state.
        assert_eq!(reloaded.serial_b, original.serial_b);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The `Config::port` / `port_mut` accessors return the right
    /// slice for each port id.  Trivial but worth pinning so a future
    /// rename can't silently swap A↔B.
    #[test]
    fn test_config_port_accessor_dispatch() {
        let mut cfg = Config::default();
        cfg.serial_a.baud = 1200;
        cfg.serial_b.baud = 2400;
        assert_eq!(cfg.port(SerialPortId::A).baud, 1200);
        assert_eq!(cfg.port(SerialPortId::B).baud, 2400);
        cfg.port_mut(SerialPortId::A).baud = 4800;
        cfg.port_mut(SerialPortId::B).baud = 9600;
        assert_eq!(cfg.serial_a.baud, 4800);
        assert_eq!(cfg.serial_b.baud, 9600);
    }

    #[test]
    fn test_resolve_unreadable_existing_config() {
        // Reload path: we already hold a good in-memory config, so keep it
        // rather than taking the service down over a transient/corrupt-file
        // blip (which would restart-storm under systemd Restart=on-failure).
        let existing = Config {
            security_enabled: true,
            password: "secret".to_string(),
            ..Config::default()
        };
        let kept = resolve_unreadable_existing_config(Some(existing.clone()))
            .expect("should keep the already-loaded config on reload");
        assert!(kept.security_enabled);
        assert_eq!(kept.password, "secret");

        // First-startup path: nothing to fall back to → signal fail-loud
        // exit rather than silently running on insecure defaults.
        assert!(resolve_unreadable_existing_config(None).is_err());
    }
}
