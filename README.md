# Ethernet Gateway

A telnet-based XMODEM/YMODEM/ZMODEM/Kermit/Punter file transfer server, SSH
gateway, Hayes-compatible modem emulator on **two physically
independent serial ports** (each with optional telnet-serial console
bridge) for serial-attached retro hardware, text-mode web browser, and
AI chat client written in Rust. Supports PETSCII (Commodore 64), ANSI,
and ASCII terminals. Designed for local network use with retro and
modern terminal clients.

**[User Manual](http://ethernetgateway.com/index.html)**
&nbsp;&middot;&nbsp;
**[Kermit Reference](http://ethernetgateway.com/kermit.html)**

Once you run the server on your PC, you can telnet to that server from
anywhere on your network (allow firewall port 2323).

Example: `telnet 192.168.1.160:2323`

This program also serves as a modem emulator. For an Altairduino PRO,
connect directly to the altairduino, and set your modem port to be 2SIO2.
(A6/A7 on mine). Remember, you can configure the serial ports by pressing
stop and aux1 up.

Run IMP8, then hit T for terminal mode on the Altairduino.

Example: `ATDT :2323` — for gateway options: `ATDT ethernet-gateway`

Note: For the Altairduino, I simply connected my USB to RS232 adapter to
the 9 pin RS232 connector.

For other machines, you may need to use a NULL modem adapter (Cross RX
and TX).

This should also work with the RC2014 / SC126, etc as well.

Author: Ricky Bryce

## Warning

**The telnet interface is intended for local/private network use only.** Telnet
transmits all data (including credentials) in cleartext. Do not expose the
telnet port to the public internet. The SSH interface provides encrypted access
but is still intended for trusted environments.

### Network Security Behavior

When **security is disabled** (the default), the server only accepts telnet
connections from private IP addresses:

- `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16` (RFC 1918 private ranges)
- `127.0.0.0/8` (loopback)
- `169.254.0.0/16` (link-local)
- IPv6 loopback (`::1`), link-local (`fe80::/10`), and unique local (`fd00::/8`)

Connections from public IP addresses are refused with an error message.
Additionally, gateway addresses (those ending in `.1`, such as `192.168.1.1`)
are rejected to prevent accidental exposure through router interfaces.

To accept connections from **any IP address**, you must enable security
(`security_enabled = true` in `egateway.conf`) and set a strong username and
password. Even with security enabled, running this software on a public network
is **not recommended** — telnet credentials are transmitted in cleartext and can
be intercepted. Use the SSH interface for any non-local access.

## Standards Compliance

### Telnet RFCs

The embedded telnet server and the client half of the Telnet Gateway implement
the core parts of the telnet protocol suite that matter for interactive
terminal and BBS use:

| RFC | Title | Implementation notes |
|-----|-------|----------------------|
| **RFC 854** | Telnet Protocol Specification | IAC framing, IAC IAC data escaping, two-byte command handling. AYT replies with `[Yes]`; IP / BRK surface as ESC to the line-editor; EC translates to DEL (backspace) and EL to NAK (erase-line) so line-input honors them; NOP / DM / AO / GA are consumed. Full TCP urgent-mode SYNCH is not implemented (DM is informational) — per RFC 6093 the urgent mechanism is deprecated because middleboxes routinely strip or mangle the urgent pointer. Outbound 0xFF bytes are escaped as IAC IAC; inbound IAC sequences are consumed transparently. |
| **RFC 855** | Telnet Option Specifications | DO / DONT / WILL / WONT negotiation with per-option state. Options we don't support receive WONT / DONT so the peer doesn't wait. |
| **RFC 857** | Telnet Echo Option | The server advertises WILL ECHO to become the echoing side and honors peer requests for ECHO. |
| **RFC 858** | Suppress Go Ahead Option | WILL SGA / DO SGA to operate in full-duplex character-at-a-time mode (rather than half-duplex GA mode). |
| **RFC 859** | Status Option | `DO STATUS` → `WILL STATUS`; `IAC SB STATUS SEND IAC SE` returns an `IAC SB STATUS IS <state> IAC SE` dump listing every option the server has advertised and not had denied. Usable via the Unix `telnet` client's `status` / `send status` subcommands. |
| **RFC 860** | Timing Mark Option | `DO TIMING-MARK` is answered with `WILL TIMING-MARK` after flushing pending output, providing clients a processing-synchronization point. The response is one-shot — no persistent option state. |
| **RFC 1073** | Window Size Option (NAWS) | Client-reported window dimensions are captured via `IAC SB NAWS <w16><h16> IAC SE` and exposed to the session for layout decisions. |
| **RFC 1091** | Terminal-Type Option (TTYPE) | On client WILL TTYPE the server replies DO, then issues `IAC SB TTYPE SEND IAC SE` and records the first `IS` response. Used as a hint for PETSCII / ANSI / ASCII detection. |
| **RFC 1143** | Q-Method of Option Negotiation | Per-option tracking of advertised DO / WILL / DONT prevents the classic negotiation loop. |

Options not negotiated (BINARY, LINEMODE, ENVIRON, NEW-ENVIRON, TSPEED,
COM-PORT, CHARSET) are explicitly refused with WONT / DONT so the peer
doesn't stall waiting for an answer.

#### Outgoing Telnet Gateway

The Telnet Gateway menu (and internally the RFC 854/855 side of `ATDT
host:port` when used for file transfer) dials out to remote telnet servers.
Compliance operates in two modes controlled by the `telnet_gateway_negotiate`
config flag:

**Reactive mode (default, `telnet_gateway_negotiate = false`)**

The gateway does not send any proactive negotiation offers, so raw-TCP
services on port 23 (legacy MUDs, hand-rolled BBS software, etc.) are not
poked with IAC bytes they don't understand.  It still does:

- Escape outbound 0xFF data bytes as `IAC IAC` so literal 0xFF survives
  the wire without being mistaken for the start of an IAC sequence.
- Parse inbound IAC from the remote and silently consume 2-byte commands
  (NOP, DM, BRK, IP, AO, AYT, EC, EL, GA) and subnegotiation bodies
  instead of leaking them into the user's terminal.
- Accept peer's `WILL ECHO` with `DO ECHO` (always on — raw-TCP services
  never send `WILL ECHO`, so this is safe in both modes).  This fixes the
  silent-typing failure on BBSes that expect the server to echo.
- Refuse every other peer-initiated option: `WILL <opt>` → `DONT <opt>`,
  `DO <opt>` → `WONT <opt>`.  Refusals are one-shot per cycle (RFC 1143
  spirit) so a persistent remote can't drive us into a loop.

**Raw-TCP escape hatch (`telnet_gateway_raw = true`)**

When set, the gateway bypasses its entire telnet-IAC layer: no IAC
escaping on outbound, no IAC parsing on inbound, no negotiation.
Intended for destinations that clearly aren't telnet at all (legacy
MUDs, hand-rolled BBS software).  Supersedes `telnet_gateway_negotiate`.
The Telnet Gateway menu shows the current mode and lets you toggle it
with a single keystroke; the change is saved to `egateway.conf` so future
sessions start in the selected mode.  Bytes written to the local user
are still IAC-escaped so their telnet client doesn't misinterpret a
stray 0xFF as a protocol byte.

**Cooperative mode (`telnet_gateway_negotiate = true`)**

In addition to everything reactive mode does, the gateway:

- Sends `IAC WILL TTYPE`, `IAC WILL NAWS`, and `IAC DO ECHO` as proactive
  offers at connect time, so BBSes that wait for the client to ask first
  still get echo, terminal-type adaptation, and window-size awareness.
- Responds to `SB TTYPE SEND` with `SB TTYPE IS PETSCII` / `ANSI` / `DUMB`
  depending on the local user's terminal type, so remotes can serve
  appropriate content.
- Responds to `DO NAWS` with `WILL NAWS` plus an immediate `SB NAWS`
  carrying the local user's actual window dimensions (from their own
  NAWS, or terminal-type defaults: 40×25 for PETSCII, 80×24 for ANSI /
  ASCII).  Any 0xFF byte in the width/height is properly IAC-doubled.
- **Forwards NAWS updates mid-session**: if the local user resizes their
  terminal during a gateway session, the new dimensions are captured
  from their `IAC SB NAWS` subnegotiation and relayed to the remote
  server as an updated `SB NAWS`.
- Tracks each option through a **full RFC 1143 six-state Q-method**
  (`No` / `Yes` / `WantYes` / `WantYesOpposite` / `WantNo` /
  `WantNoOpposite`), so mind-changes while a prior WILL or DO is in
  flight resolve cleanly instead of racing into inconsistent state.

The gateway never waits for a reply to any message it sends, so silent
or partially-compliant remote servers do not cause it to stall.  Enable
cooperative mode when dialing real telnet servers; leave it off for
compatibility with raw-TCP destinations.

### Hayes AT Command Set

See [Hayes Compliance Summary](#hayes-compliance-summary) in the Modem
Emulator section for a full command inventory and the three gateway-friendly
default deviations (`AT&D0`, `AT&K0`, `S7=15`).

## Prerequisites

### Debian 13 / Ubuntu

Install build dependencies and the Rust toolchain:

```sh
sudo apt update
sudo apt install -y build-essential pkg-config cmake curl libudev-dev
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Follow the prompts (press 1 for the default installation). Then load the
environment into your current shell:

```sh
source "$HOME/.cargo/env"
```

### Fedora / RHEL / AlmaLinux

```sh
sudo dnf install -y gcc gcc-c++ make cmake pkg-config curl
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

### Arch Linux

```sh
sudo pacman -S --needed base-devel cmake curl
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

### Windows

1. Download and run the rustup installer from https://rustup.rs
2. When prompted, install the Visual Studio C++ Build Tools (required)
3. Open a new terminal after installation completes

`cmake` is also required. Install it from https://cmake.org/download/ or via
winget:

```
winget install Kitware.CMake
```

### macOS

```sh
xcode-select --install
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
brew install cmake
```

### Verify Installation

```sh
rustc --version    # should show 1.85.0 or later
cargo --version
cmake --version
```

## Building

```sh
cargo build --release
```

The binary will be at `target/release/ethernet-gateway`.

## Verifying Releases

Pre-built binaries are published to the [GitHub Releases][releases] page
for Linux (x86_64), macOS (aarch64), and Windows (x86_64). Every release
ships with:

- The binary archive (`ethernet-gateway-vX.Y.Z-<target>.tar.gz` or `.zip`).
- A SHA-256 checksum (`<archive>.sha256`).
- Optionally a detached GPG signature (`<archive>.asc`) — produced if the
  release signer has a GPG key configured.
- A [Sigstore][sigstore] keyless signature (`<archive>.sig` +
  `<archive>.pem`) bound to the publisher's GitHub identity. Produced on
  every release automatically; no key management required.

[releases]: https://github.com/rickybryce/ethernet-gateway/releases
[sigstore]: https://www.sigstore.dev/

### Verifying the checksum

```sh
sha256sum -c ethernet-gateway-v0.6.1-x86_64-unknown-linux-gnu.tar.gz.sha256
```

### Verifying the GPG signature (if present)

```sh
gpg --keyserver keys.openpgp.org --recv-keys <KEY_FINGERPRINT>
gpg --verify \
    ethernet-gateway-v0.6.1-x86_64-unknown-linux-gnu.tar.gz.asc \
    ethernet-gateway-v0.6.1-x86_64-unknown-linux-gnu.tar.gz
```

### Verifying the Sigstore signature

[`cosign`](https://github.com/sigstore/cosign) is required (one-time install,
free):

```sh
cosign verify-blob \
    --certificate ethernet-gateway-v0.6.1-x86_64-unknown-linux-gnu.tar.gz.pem \
    --signature   ethernet-gateway-v0.6.1-x86_64-unknown-linux-gnu.tar.gz.sig \
    --certificate-identity-regexp "https://github.com/rickybryce/ethernet-gateway/.*" \
    --certificate-oidc-issuer "https://token.actions.githubusercontent.com" \
    ethernet-gateway-v0.6.1-x86_64-unknown-linux-gnu.tar.gz
```

This ties the binary to a specific GitHub Actions workflow run on
this repository.

### OS-level trust prompts

Neither Windows `.exe` nor macOS `.app` bundles ship with commercial
code-signing certificates (those cost $100–400/year and aren't in scope
for a hobby project). As a result:

- **Windows**: SmartScreen shows "Windows protected your PC"; click
  *More info* → *Run anyway*. Verify the SHA-256 and GPG/Sigstore
  signature first.
- **macOS**: Gatekeeper shows "cannot be opened because the developer
  cannot be verified"; right-click → *Open* → *Open*, or remove the
  quarantine attribute with `xattr -d com.apple.quarantine <path>`.
- **Linux**: no equivalent prompt; just verify and run.

If this causes friction in your environment, build from source
(`cargo build --release`) — the result is identical modulo build
reproducibility.

## Running

```sh
./ethernet-gateway
```

On first run, a default configuration file `egateway.conf` is created in the
working directory. The telnet server listens on port 2323 by default.

Connect with any telnet client:

```sh
telnet <server-ip> 2323
```

Or, if the SSH interface is enabled, connect with any SSH client:

```sh
ssh <ssh-user>@<server-ip> -p 2222
```

### Running as a systemd Service (Linux)

A hardened systemd unit file is provided at
[`contrib/systemd/ethernet-gateway.service`](contrib/systemd/ethernet-gateway.service).
To install:

```sh
# Create a dedicated unprivileged user
sudo useradd --system --home-dir /var/lib/ethernet-gateway \
             --shell /usr/sbin/nologin ethernet-gateway
sudo install -d -m 0750 -o ethernet-gateway -g ethernet-gateway \
             /var/lib/ethernet-gateway

# Install the binary
sudo install -m 0755 target/release/ethernet-gateway /usr/local/bin/

# Install and start the service
sudo install -m 0644 contrib/systemd/ethernet-gateway.service \
             /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now ethernet-gateway.service

# Watch the log
journalctl -u ethernet-gateway -f
```

The unit ships with defensive hardening enabled by default:
`NoNewPrivileges`, `PrivateTmp`, `ProtectSystem=strict`,
`ProtectHome`, namespace restrictions, `SystemCallFilter=@system-service`,
capability bounding, and a 512 MiB memory cap.  Edit the file to
loosen anything that breaks your deployment.

Set the telnet server port below 1024 (e.g. 23) by uncommenting the
`CapabilityBoundingSet=CAP_NET_BIND_SERVICE` line and matching
`AmbientCapabilities`.

## GUI Configuration Editor

When `enable_console = true` (the default), a graphical configuration window
opens on startup. The GUI provides:

- **Live console output** -- server log messages stream in the bottom panel
- **Configuration editing** -- all `egateway.conf` settings can be changed and
  saved without editing the file by hand
- **Two independent serial ports** -- the Serial Port frame has a header row
  with **Enabled** checkboxes for both **Port A** and **Port B**, plus a
  shared **Save** button.  Each port has its own row beneath the header
  with a device-path dropdown (auto-detected; refresh button to re-scan)
  and a baud field.  Each row's **More...** button opens that port's
  advanced popup, where you select the **Mode** (*Modem (AT Command)
  Mode* or *Telnet-Serial Mode* — see Console Mode below), framing,
  flow control, and the full Hayes AT-state surface.  The two popups
  are independent so you can compare settings side-by-side.
- **"More..." popups** -- the Server, File Transfer, and per-port Serial
  Port frames each have a **More...** button that opens an advanced-
  options window. The File Transfer popup exposes the XMODEM-family
  timeouts plus the independent ZMODEM tunables (handshake, frame
  timeout, retry cap) side by side.
- **User Manual button** -- opens the PDF user manual on GitHub in your browser
- **Save and Restart Server** -- writes changes to `egateway.conf` and restarts
  the server so all changes (including security, ports, and credentials) take
  effect immediately

The GUI window closes automatically when the server receives a shutdown signal
(Ctrl+C, SIGTERM, SIGHUP) or when the Save and Restart Server button is
clicked (the GUI reopens after the restart completes). Closing the GUI window
does **not** stop the server -- it continues running headless until a shutdown
signal is received.

To disable the GUI, set `enable_console = false` in `egateway.conf` or uncheck
"Show GUI on Startup" in the Other Settings section and save.

## Main Menu

After connecting and completing terminal detection (and login, if security is
enabled), the main menu offers:

```
  A  AI Chat
  B  Simple Browser
  C  Configuration
  F  File Transfer
  G  Serial Gateway
  R  Troubleshooting
  S  SSH Gateway
  T  Telnet Gateway
  W  Weather
  X  Exit
```

The **Serial Gateway** option appears whenever **at least one** of the
two serial ports (Port A or Port B) is configured in **console mode**
(see Console Mode below).  Picking it opens an A/B picker showing both
ports' status; the bridge is then established to whichever port you
choose, and your telnet/SSH session becomes a transparent pipe to the
wire in both directions.  Press **ESC** twice to leave the bridge and
return to the menu.

## Configuration

Most settings can be changed from within a telnet or SSH session using the
**C** (Configuration) menu, which provides submenus for:

- **E** Security -- toggle login requirement, set the unified username /
  password used by telnet, SSH, and the configuration web UI
- **G** Gateway Configuration -- outbound Telnet and SSH Gateway options
- **M** Serial Configuration -- opens an A/B picker submenu listing both
  ports with their current status (Disabled / Modem mode / Console
  mode).  Pick a port to enter that port's settings menu, where you
  can toggle its mode (the per-port **T** item flips between Modem
  Emulator and Serial Console), set its device, baud, framing, flow
  control, AT-state, dialup mapping, and ring emulator.  Each port's
  settings are fully independent and persist under separate
  `serial_a_*` / `serial_b_*` keys in `egateway.conf`.
- **S** Server Configuration -- enable/disable telnet and SSH, set ports,
  session cap, idle timeout, and the network-safety opt-out
  (`disable_ip_safety`)
- **F** File Transfer -- submenu with shared transfer directory and
  per-protocol settings pages:
  - **X** XMODEM settings -- negotiation timeout, retry interval
    (C/NAK poke cadence), block timeout, and retry limit (shared with
    XMODEM-1K and YMODEM, which use the same protocol code)
  - **Y** YMODEM settings -- same keys as XMODEM; page calls out the
    shared-family behavior
  - **Z** ZMODEM settings -- independent handshake timeout, retry
    interval (ZRINIT/ZRQINIT re-send cadence), per-frame read timeout,
    and ZRQINIT/ZRPOS/ZDATA retry cap
  - **K** Kermit settings -- standalone-listener toggle and port,
    `allow_atdt_kermit`, plus the protocol-tuning surface (negotiation
    timeout, packet timeout, idle timeout, max retries, max packet
    length, window size, block-check type, capability flags, resume,
    locking shifts, 8-bit quoting). See the
    [Kermit Reference](http://ethernetgateway.com/kermit.html) for
    the full discussion of each tunable.
  - **P** Punter settings -- negotiation timeout and retry interval,
    per-block timeout, retry limit, max bad-block rounds, block size,
    and the hang-up-on-failure toggle (drops carrier on give-up, since
    C1 has no in-band abort to free a stranded C64)
- **O** Other Settings -- AI API key, browser homepage, weather zip, verbose
  logging, GUI on startup
- **R** Reset Defaults -- restore all settings to factory defaults

All settings are persisted to `egateway.conf` automatically. You can also edit
`egateway.conf` by hand. All options:

```ini
# Telnet server: set to false to disable (SSH-only mode)
telnet_enabled = true

# Telnet server port
telnet_port = 2323

# Outgoing Telnet Gateway cooperative negotiation (see Telnet RFCs section).
# Off by default so raw-TCP services on port 23 keep working.
telnet_gateway_negotiate = false

# Outgoing Telnet Gateway raw-TCP escape hatch.
# When true, the gateway disables its telnet-IAC layer entirely and
# treats the remote as raw TCP.  Toggleable live from the Telnet Gateway
# menu (press 'T' at the mode prompt) — changes are persisted here.
telnet_gateway_raw = false

# Show the GUI configuration/console window on startup.
# Set to false when running as a headless service.
enable_console = true

# Security: set to true to require username/password login
security_enabled = false

# Credentials (only used when security_enabled = true)
username = admin
password = changeme

# Directory for file transfers (relative to working directory)
transfer_dir = transfer

# Maximum concurrent telnet sessions
max_sessions = 50

# Idle session timeout in seconds (0 = no timeout)
idle_timeout_secs = 900

# Groq API key for AI Chat (get one at https://console.groq.com/keys)
# Leave empty to disable AI Chat.
groq_api_key =

# Browser homepage URL (loaded automatically when entering the browser)
# Leave empty to start with a blank prompt.
browser_homepage = http://telnetbible.com

# Last-used weather zip code (updated automatically when you check weather)
weather_zip =

# Verbose logging: set to true for detailed per-block / per-subpacket
# protocol diagnostics across XMODEM, YMODEM, ZMODEM, Kermit, and Punter.
verbose = false

# XMODEM-family protocol timeouts (apply to XMODEM, XMODEM-1K, and YMODEM —
# they share the same protocol code path).
# xmodem_negotiation_timeout:        seconds to wait for the peer to start sending.
# xmodem_block_timeout:              seconds to wait for each data block.
# xmodem_max_retries:                retry limit per block.
# xmodem_negotiation_retry_interval: seconds between C/NAK pokes during the
#                                    initial handshake (spec ~10 s, default 7).
xmodem_negotiation_timeout = 45
xmodem_block_timeout = 20
xmodem_max_retries = 10
xmodem_negotiation_retry_interval = 7

# ZMODEM protocol tunables (independent of the XMODEM family).
# zmodem_negotiation_timeout:        seconds to wait for ZRQINIT / ZRINIT handshake.
# zmodem_frame_timeout:              seconds to wait for each header / subpacket.
# zmodem_max_retries:                retry limit for ZRQINIT / ZRPOS / ZDATA frames.
# zmodem_negotiation_retry_interval: seconds between ZRINIT / ZRQINIT re-sends
#                                    during the handshake (default 5).
zmodem_negotiation_timeout = 45
zmodem_frame_timeout = 30
zmodem_max_retries = 10
zmodem_negotiation_retry_interval = 5

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
# allow_atdt_kermit:           let `ATDT KERMIT` from the serial modem
#                              emulator drop directly into Kermit server mode
#                              without going through the telnet menu.  Off
#                              by default because it bypasses any
#                              security_enabled username/password gate.
#                              Enable only on trusted serial lines; for any
#                              auth-required deployment leave this off and
#                              have callers go via the telnet F/K path.
kermit_negotiation_timeout = 300
kermit_packet_timeout = 10
kermit_idle_timeout = 300
kermit_max_retries = 5
kermit_max_packet_length = 4096
kermit_window_size = 4
kermit_block_check_type = 3
kermit_long_packets = true
kermit_sliding_windows = true
kermit_streaming = true
kermit_attribute_packets = true
kermit_repeat_compression = true
kermit_8bit_quote = auto
kermit_resume_partial = false
kermit_resume_max_age_hours = 168
kermit_locking_shifts = false
allow_atdt_kermit = false

# Standalone Kermit server listener.
# kermit_server_enabled:  bind a dedicated TCP port that drops every accepted
#                         connection straight into Kermit server mode — no
#                         telnet menu, no auth gate, no private-IP allowlist.
#                         Off by default; enabling it bypasses every security
#                         check the gateway has, so opt in only when the
#                         network path itself is trusted.
# kermit_server_port:     TCP port for the listener (default 2424).
kermit_server_enabled = false
kermit_server_port = 2424

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
punter_block_size = 255
punter_negotiation_timeout = 45
punter_block_timeout = 20
punter_max_retries = 10
punter_max_bad_rounds = 30
punter_negotiation_retry_interval = 5
punter_hangup_on_failure = false

# Serial ports.  The gateway exposes two physically independent ports —
# Port A and Port B — each with its own enabled flag, role (modem
# emulator or telnet-serial console), serial parameters, and persisted
# AT/S-register state.
#
# <port>_enabled = true activates that port.  <port>_mode selects its role:
#   modem    — run the Hayes AT command emulator
#   console  — expose the port via the telnet menu's Serial Gateway,
#              bridging the telnet client directly to the wire.
#
# Legacy single-port configs (using bare `serial_*` keys) auto-migrate
# into Port A on first read; the writer always emits the dual-port form.
# Port B defaults to enabled = false so existing single-port deployments
# behave identically until you opt in.

# Serial Port A
serial_a_enabled = false
serial_a_mode = modem
serial_a_port =
serial_a_baud = 9600
serial_a_databits = 8
serial_a_parity = none
serial_a_stopbits = 1
serial_a_flowcontrol = none
serial_a_echo = true
serial_a_verbose = true
serial_a_quiet = false
serial_a_s_regs = 5,0,43,13,10,8,2,15,2,6,14,95,50,0,0,0,0,0,0,0,0,0,0,0,0,5,1
serial_a_x_code = 4
serial_a_dtr_mode = 0
serial_a_flow_mode = 0
serial_a_dcd_mode = 1
serial_a_stored_0 =
serial_a_stored_1 =
serial_a_stored_2 =
serial_a_stored_3 =

# Serial Port B
serial_b_enabled = false
serial_b_mode = modem
serial_b_port =
serial_b_baud = 9600
serial_b_databits = 8
serial_b_parity = none
serial_b_stopbits = 1
serial_b_flowcontrol = none
serial_b_echo = true
serial_b_verbose = true
serial_b_quiet = false
serial_b_s_regs = 5,0,43,13,10,8,2,15,2,6,14,95,50,0,0,0,0,0,0,0,0,0,0,0,0,5,1
serial_b_x_code = 4
serial_b_dtr_mode = 0
serial_b_flow_mode = 0
serial_b_dcd_mode = 1
serial_b_stored_0 =
serial_b_stored_1 =
serial_b_stored_2 =
serial_b_stored_3 =

# SSH server interface (encrypted access to the gateway)
# Set ssh_enabled = true to activate. Uses its own credentials.
ssh_enabled = false

# SSH server port
ssh_port = 2222

# SSH, telnet, and the web UI share the unified `username` / `password`
# above — there are no separate SSH credentials anymore.  An upgrading
# config with non-default legacy `ssh_username` / `ssh_password` keys
# is migrated into the unified pair on first load (only when the
# unified pair is still at the factory defaults).

# SSH gateway (outbound proxy) authentication method offered when bridging
# a telnet session out to a remote SSH host (the SSH Gateway menu / ATDT).
#   password — authenticate to the remote host with a password (default)
#   key      — authenticate with the gateway's own SSH key
ssh_gateway_auth = password

# Web configuration server (browser-based config editor).
# web_enabled: bind an HTTP server for editing the configuration from a
#              browser.  Off by default.  Shares the unified username /
#              password and the per-IP lockout with telnet and SSH.
# web_port:    TCP port for the web configuration server (default 8080).
web_enabled = false
web_port = 8080

# Gateway debug trace: extra per-connection diagnostics (AT commands and
# their effect, gateway negotiation steps).  Noisier than `verbose` and
# aimed at chasing connection-level issues; off by default.
gateway_debug = false
```

### Setting Up Authentication

To require a username and password, either use the in-app Configuration menu
(**C** > **E** Security) or edit `egateway.conf` by hand:

1. Open `egateway.conf` in a text editor
2. Set `security_enabled = true`
3. Change `username` and `password` to your desired credentials
4. Restart the server

When enabled, users must authenticate after terminal detection. Failed login
attempts are tracked per IP address -- after 3 failures, the IP is locked out
for 5 minutes.

**Note:** Credentials are stored in plaintext in `egateway.conf`. This is
consistent with the telnet protocol itself, which transmits all data
(including passwords) in cleartext. Do not reuse sensitive passwords here.
This authentication is intended as a lightweight access control for private
networks, not as a security boundary.

### Setting Up AI Chat

The AI Chat feature uses the [Groq API](https://groq.com), which provides free
access to fast LLM inference. To enable it:

1. Go to https://console.groq.com and create a free account
2. Navigate to **API Keys** and generate a new key (starts with `gsk_`)
3. Set the key via Configuration > Other Settings > **A** (Set AI API key), or
   open `egateway.conf` and set: `groq_api_key = gsk_your_key_here`
4. Restart the server

If no API key is configured, selecting AI Chat from the menu will display
instructions on how to obtain one.

### Setting Up the Browser Homepage

The browser loads `http://telnetbible.com` by default. To change it, use
Configuration > Other Settings > **B** (Set browser homepage), or edit
`egateway.conf`:

1. Open `egateway.conf`
2. Set `browser_homepage` to a URL, e.g.: `browser_homepage = example.com`
3. Restart the server

## Terminal Support

On connect, the server asks the user to press **Backspace** to detect the
terminal type:

| Byte received | Terminal type | Description |
|---------------|---------------|-------------|
| 0x14          | PETSCII       | Commodore 64 (40-column, single-byte color codes) |
| 0x08 or 0x7F  | ANSI          | Modern terminal with escape sequence color |
| Other         | ASCII         | Plain text, no color |

After detection, the server asks whether to enable color. The user must press
Y or N to continue; no default is applied.

## Transferring Files

### Supported Protocols

The gateway implements six file-transfer protocols, selectable per-transfer
from menus on the gateway side:

| Protocol | Block size | CRC | Direction | Notes |
|----------|------------|-----|-----------|-------|
| **XMODEM** | 128 B (SOH) | CRC-16 or checksum | up/down | Auto-detects CRC vs. checksum on receive; classic single-file. |
| **XMODEM-1K** | 1024 B (STX) | CRC-16 | up/down | Download option; on upload the XMODEM/YMODEM branch accepts STX blocks transparently. Opportunistically falls back to SOH if the peer NAKs the first STX. |
| **YMODEM** | 1024 B + block-0 header | CRC-16 | up/down | Block 0 carries filename + size; the receive path auto-detects it. |
| **ZMODEM** | variable subpackets (1 K default) | CRC-16 out, CRC-16 or CRC-32 in | up/down | Full Forsberg spec: ZRQINIT handshake, ZDLE escaping, ZSKIP, batch sends and receives. On upload the first file is saved under the name you entered; subsequent files in a batch use the sender's filename (validated, collisions skipped via ZSKIP). The one optional frame deliberately not implemented is `ZCOMMAND` (remote command execution) — it is always refused for security; use SSH for shell access. |
| **Kermit** | configurable long packets (4096 default) | 6-bit / 12-bit checksum or CRC-16/KERMIT | up/down + server | Columbia spec — sliding windows, attribute packets, RESEND, locking shifts, 8-bit quoting. Both **client** (push/pull from the menu) and **server** (idle in the file-transfer menu's `K` slot, on the standalone TCP listener, or via `ATDT KERMIT`) modes. See the [Kermit Reference](http://ethernetgateway.com/kermit.html) for the full surface. |
| **Punter** | 255 B blocks, 248 B payload (configurable down to 40) | dual checksum — 16-bit additive + cyclic rotate-left | up/down | C1 "New Punter" — the protocol CCGMS / Novaterm / StrikeTerm speak natively on Commodore BBSes. Two-phase transfer (type block then data blocks) with a 3-byte ASCII handshake. C1 has no in-band abort, so a stalled give-up can optionally drop carrier (`punter_hangup_on_failure`) to free a stranded C64. |

On upload, the gateway offers **XMODEM / YMODEM** (variant auto-detected),
**ZMODEM**, **Kermit**, or **Punter**. On download, you pick the specific
variant you want, including Kermit and Punter. Kermit also has a dedicated
server mode (press **K**
on the File Transfer menu) and a standalone TCP listener (set
`kermit_server_enabled = true` in `egateway.conf`).

### Uploading a File to the Server

1. Connect via telnet and navigate to **F** (File Transfer)
2. Press **U** (Upload)
3. Enter a filename (letters, numbers, dots, hyphens, underscores only; max 64
   characters; cannot start with a dot, cannot contain `..`, must include at
   least one letter or digit)
4. On the **SELECT UPLOAD PROTOCOL** screen, press **X** (XMODEM / YMODEM —
   block size, CRC mode, and batch header are auto-detected), **Z** (ZMODEM),
   **K** (Kermit, any flavor — see the
   [Kermit Reference](http://ethernetgateway.com/kermit.html)), or **P**
   (Punter — Commodore C1)
5. The server displays "Start XMODEM/YMODEM send now", "Start ZMODEM send
   now", "Start Kermit send now", or "Start PUNTER send … now" and waits for
   the negotiation handshake
6. In your terminal client, start the matching upload
   - Most terminal programs have a "Send File" or "Upload" option under a
     Transfer or File menu
   - ExtraPutty: **File Transfer → Zmodem → Send**; SyncTerm: **Ctrl-PgUp**;
     Kermit: `kermit -s file` or the equivalent client UI
7. On completion, the server reports bytes, blocks, and elapsed time. For
   ZMODEM and Kermit batches, every file the sender transmits is listed
   (saved or skipped)

### Downloading a File from the Server

1. Navigate to **F** (File Transfer), then press **D** (Download)
2. The server lists files in the current transfer directory (paginated, 10 per
   page)
3. Enter the number of the file to download
4. On the **SELECT PROTOCOL** screen, choose **X** (XMODEM), **1** (XMODEM-1K),
   **Y** (YMODEM), **Z** (ZMODEM), **K** (Kermit), or **P** (Punter)
5. The server prompts "Start *protocol* receive now" and waits for the peer
   to begin
6. In your terminal client, start the matching receive and choose where to
   save the file locally (ZMODEM auto-starts in most modern terminals; for
   Kermit, run `kermit -r` or the equivalent client UI)
7. On completion, the server reports the transfer result

### Other File Operations

- **X** -- Delete a file (with confirmation)
- **C** -- Change to a subdirectory within the transfer directory
- **K** -- Kermit server mode: idle and wait for a Kermit client's commands
  (`get`, `send`, `dir`, `cwd`, `finish`, etc.). See the
  [Kermit Reference](http://ethernetgateway.com/kermit.html) for the full
  G-subcommand table.
- **I** -- Toggle IAC escaping on/off (needed when transferring binary files
  over telnet that contain 0xFF bytes)

### IAC Escaping

Telnet reserves byte 0xFF as the IAC (Interpret As Command) marker. When
transferring binary files that may contain 0xFF, enable IAC escaping with the
**I** toggle in the File Transfer menu. Both the server and your terminal client
must agree on whether IAC escaping is active. For text files or when your client
handles this automatically, leave it off (the default).

## SSH Server

The SSH server provides encrypted access to the same gateway menus and features
available over telnet. This is useful when connecting from modern clients where
encryption is preferred over plaintext telnet.

### Enabling the SSH Server

Use Configuration > Server Configuration to toggle SSH and set the port, and
Configuration > Security to set the login credentials. Or edit `egateway.conf`
by hand:

1. Set `ssh_enabled = true`
2. Change `username` and `password` to your desired credentials (the same pair
   is used by telnet, SSH, and the web configuration UI)
3. Optionally change `ssh_port` (default 2222)
4. Restart the server

On first start with SSH enabled, the server generates an Ed25519 host key and
saves it to `ethernet_ssh_host_key` in the working directory. This key is reused
on subsequent starts so that clients can verify the server's identity.

### Connecting

```sh
ssh <username>@<server-ip> -p 2222
```

After authenticating, you are presented with the same Ethernet Gateway menu
system as a telnet connection, using ANSI terminal mode. All features (file
transfer, SSH/telnet gateway, browser, AI chat, modem emulator, weather) are
available.

### SSH, Telnet, and Web UI Credentials

A single `username` / `password` pair in `egateway.conf` is used by **all
three** authenticated interfaces — the telnet menu, the SSH server, and the
configuration web UI.  The factory defaults are `admin` / `changeme`; change
them via Configuration > Security in the telnet menu, the Security frame in the
GUI / web UI, or by editing `egateway.conf` directly.

If you're upgrading from a release that still had the separate
`ssh_username` / `ssh_password` keys, the first time the new server reads your
config those legacy values are migrated into the unified pair *only* when the
unified pair is still at the factory defaults — so a setup that customised the
SSH login keeps working without intervention.  Once the next save runs, the
legacy keys are dropped from the file.

Three failed logins from the same source IP — across any of telnet, SSH, or
the web UI — trip a 5-minute lockout that affects all three protocols.

**Note:** credentials in `egateway.conf` are stored in plaintext. While SSH
and HTTPS-fronted access to the web UI would be encrypted on the wire, the
config file is not. Protect it with appropriate file permissions.

## SSH Gateway

The SSH Gateway allows you to connect through the server to a remote SSH host.
This is useful for accessing SSH servers from terminals that only support telnet
(such as a Commodore 64).

1. From the main menu, press **S** (SSH Gateway)
2. Optionally press **K** at the first prompt to display the gateway's public
   key (see *Public-Key Authentication* below)
3. Enter the remote host, port (default 22), and username
4. The gateway attempts public-key authentication using its own keypair first
5. If the remote doesn't trust the gateway key, you are prompted for a password
6. Once connected, you have a full interactive shell on the remote server
7. Press **ESC** twice to disconnect from the SSH session

The server acts as a proxy between your telnet client and the remote SSH server.
All input is forwarded to the SSH session, and all output is sent back to your
terminal. Telnet line-ending conventions (CR+LF, CR+NUL) are automatically
normalized to bare CR for SSH compatibility.

For PETSCII and ASCII terminals, ANSI escape sequences from the remote host are
automatically stripped, and text is converted to the appropriate encoding. ANSI
terminals receive the raw output unmodified. The PTY size is set to 40x25 for
PETSCII and 80x24 for ANSI/ASCII terminals.

### Public-Key Authentication

On the first outbound SSH dial, the gateway generates an Ed25519 client keypair
and stores it as `ethernet_gateway_ssh_key` (0o600 on Unix). Every subsequent
dial tries public-key authentication with this key *before* falling back to a
password prompt. If the remote accepts the key, you skip the password prompt
entirely — identical to how OpenSSH from the command line behaves.

To set up passwordless login to a particular remote:

1. Open the SSH Gateway menu and press **K** — the gateway's public key is
   displayed in the standard `ssh-ed25519 AAAA…` OpenSSH format.
2. Copy that line into the remote server's `~/.ssh/authorized_keys` file.
3. Future dials to that host skip the password prompt.

If the remote doesn't have the gateway's key in its `authorized_keys`, you see
a one-line notice (`Pubkey not accepted — password required.`) and the
password prompt appears as before.

### Host-Key Verification

The first time you dial a new SSH server, the gateway shows the server's
SHA-256 fingerprint and asks whether to trust it (TOFU — trust-on-first-use).
If accepted, the fingerprint is saved to `gateway_hosts` (0o600 on Unix) and
checked on every subsequent dial. A changed key produces a prominent
`WARNING: HOST KEY HAS CHANGED!` with the option to update or abort.

All host-key trust decisions (first-time accept, update, and reject) are
written to the server log so there is a forensic trail if a key change turns
out to be a man-in-the-middle attempt.

### SSH Gateway vs SSH Server

`gateway_hosts` holds the *remote* servers' public keys (similar to an OpenSSH
client's `~/.ssh/known_hosts`). `ethernet_ssh_host_key` is the Ethernet Gateway's
*own* SSH server host key. `ethernet_gateway_ssh_key` is the gateway's outgoing-
client keypair used for public-key authentication to remote servers. All three
are independent.

## Telnet Gateway

The Telnet Gateway connects through the server to a remote telnet host. This is
useful for accessing BBS systems or other telnet services from retro terminals.

1. From the main menu, press **T** (Telnet Gateway)
2. At the mode prompt, press **T** to toggle between `Telnet protocol` and
   `Raw TCP` mode if needed (see below), or any other key to continue
3. Enter the remote host and port (default 23)
4. Once connected, all input and output is proxied between your terminal and the
   remote server
5. Press **ESC** twice to disconnect

For PETSCII and ASCII terminals, ANSI escape sequences from the remote host are
automatically filtered.

### Protocol Modes

The gateway has three modes of operation, all documented in the [Telnet RFCs](#telnet-rfcs)
section above. In short:

- **Telnet protocol (default)** — the gateway parses IAC framing in both
  directions, accepts cooperative ECHO from the remote, refuses other options.
  Works with any real telnet server.
- **Cooperative mode** (`telnet_gateway_negotiate = true` in `egateway.conf`) —
  adds proactive TTYPE, NAWS, and DO ECHO offers so modern BBSes can adapt
  content and render full-screen layouts at your actual window size.
- **Raw TCP** (toggled with **T** at the gateway menu, saved persistently) —
  the IAC layer is disabled entirely. Use this when dialing destinations that
  don't speak telnet at all (legacy MUDs, hand-rolled BBS software, some
  services on port 23). The toggle persists to `egateway.conf` so you only need
  to set it once per destination type.

## Modem Emulator

The modem emulator provides Hayes AT command emulation on a physical serial
port. This allows retro hardware (Commodore 64, CP/M machines, etc.) to connect
to the gateway and to remote telnet hosts using a serial connection and standard
modem commands.

The gateway exposes **two physically independent ports**, **Port A** and
**Port B**.  Each port is fully independent — its own enabled flag,
mode, device, baud, AT/S-register state, and stored phone-number
slots — so you can run a Hayes modem on one port and a telnet-serial
bridge on the other (or two modems, or two bridges) at the same time.

Each port can run in one of two modes:

- **Modem (AT Command) Mode** (default) — runs the Hayes emulator described
  below.
- **Telnet-Serial Mode** — keeps the port idle until a telnet/SSH user
  selects **G  Serial Gateway** from the main menu and picks this
  port, at which point the session is bridged directly to the wire.
  See **Console Mode** below.

The mode is per-port: the **Mode** dropdown inside each port's GUI
"More..." popup, and the per-port **T** (Toggle Modem/Console mode)
item in the telnet **Configuration > M (Serial Configuration) >
Port A or B** submenu, both switch a single port between the two
modes.  The setting persists under `serial_a_mode` / `serial_b_mode`
in `egateway.conf`.

### Setting Up

1. From the main menu, press **C** (Configuration)
2. Press **M** (Serial Configuration) to open the A/B picker
3. Press **A** or **B** to enter that port's settings menu
4. Press **T** if needed to switch between **Modem** mode (default)
   and **Console** mode for the port you're editing
5. Press **E** to enable the port
6. Press **S** to select a serial device (auto-detected)
7. Configure baud rate, data bits, parity, stop bits, and flow control as needed
8. Press **Q** to apply -- settings take effect immediately (no restart needed)

Or edit `egateway.conf` directly under the `serial_a_*` / `serial_b_*`
keys and restart the server.

### Supported AT Commands

| Command | Action |
|---------|--------|
| `AT`    | OK (attention) |
| `AT?`   | Show AT command help |
| `ATZ`   | Reset modem to stored settings (saved by AT&W) |
| `AT&F`  | Reset modem to factory defaults (gateway-friendly, see below) |
| `AT&W`  | Save current modem settings to `egateway.conf` |
| `AT&V`  | Display current modem configuration |
| `ATE0` / `ATE1` | Echo off / on |
| `ATV0` / `ATV1` | Numeric / verbose result codes |
| `ATQ0` / `ATQ1` | Result codes on / quiet mode (suppress results) |
| `ATI` / `ATI0`–`ATI7` | Identification variants (product ID, ROM checksum, ROM test, firmware, OEM, country, diag, product info) |
| `ATH`   | Hang up (close any active connection) |
| `ATA`   | Answer incoming ring |
| `ATO`   | Return to online mode (resume after `+++` escape) |
| `ATX0`–`ATX4` | Result code verbosity (see table below) |
| `AT&C0` / `AT&C1` | DCD always on / DCD reflects carrier (default) |
| `AT&D0`–`AT&D3` | DTR handling (0 = ignore, default; 1 = cmd mode on drop; 2 = hang up; 3 = hang up + reset) |
| `AT&K0`–`AT&K4` | Modem-layer flow control (0 = none, default; 1 = auto-detect, stored only — no wire effect; 3 = RTS/CTS; 4 = XON/XOFF) |
| `ATS?`  | Show S-register help |
| `ATS`*n*`?` | Query S-register *n* (returns 3-digit value) |
| `ATS`*n*`=`*v* | Set S-register *n* to value *v* (0–255). Range S0–S26 |
| `ATDL`  | Redial last number |
| `ATDS` / `ATDS`*n* | Dial stored number from slot *n* (0–3; default 0) |
| `AT&Z`*n*`=`*s* | Store phone number or host *s* in slot *n* (0–3) |
| `ATDT ethernet-gateway` | Connect to this gateway's menus |
| `ATDT KERMIT` | Drop straight into Kermit server mode (aliases: `ATDT kermit`, `ATDT kermit-server`, `ATDT kermit server`). Requires `allow_atdt_kermit = true`; off by default because it bypasses the telnet auth gate. See the [Kermit Reference](http://ethernetgateway.com/kermit.html). |
| `ATDT host:port` | Dial a remote telnet host |
| `ATDP host:port` | Pulse dial (same as ATDT — no distinction for TCP) |
| `A/`    | Repeat the last command (no `AT` prefix, no CR required) |
| `+++`   | Return to command mode (with guard time from S12) |

Unrecognized commands (`ATB`, `ATC`, `ATL`, `ATM`, `AT&B`, `AT&G`, `AT&J`,
`AT&S`, `AT&T`, `AT&Y`, etc.) are accepted and return `OK` so that legacy
init strings don't halt with `ERROR` on commands the emulator has no
hardware to implement.

**Dial modifiers** inside phone-number dial strings:

| Modifier | Action |
|----------|--------|
| `,` | Pause for S8 seconds (default 2s) before continuing |
| `W` | Wait for dial tone (adds S6 seconds, virtual) |
| `;` | After dial, return to command mode instead of going online |
| `*`, `#` | DTMF digits, preserved for phone-number lookup |
| `P`, `T`, `@`, `!` | Pulse/tone/quiet/hookflash selectors, ignored |

Modifiers are only honored when the dial string looks like a phone number.
Hostnames like `pine.example.com` or `www.example.com` are not stripped.

**Result codes and ATX levels:** In verbose mode (default) results are text
(`OK`, `CONNECT`, `NO CARRIER`, `ERROR`). In numeric mode (`ATV0`) results are
digits. Quiet mode (`ATQ1`) suppresses all result codes. The ATX level
controls which codes the modem may emit and whether `CONNECT` includes the
line speed:

| Level | Extended codes | CONNECT format |
|-------|----------------|----------------|
| X0 | Basic only; BUSY / NO DIALTONE / NO ANSWER collapse to NO CARRIER | `CONNECT` (code 1) |
| X1 | Basic + baud in CONNECT | `CONNECT <baud>` (code per baud) |
| X2 | Adds NO DIALTONE detection | `CONNECT <baud>` |
| X3 | Adds BUSY detection | `CONNECT <baud>` |
| X4 | Full extended set (gateway default) | `CONNECT <baud>` |

Numeric `CONNECT` codes follow Hayes conventions: 1 = 300, 5 = 1200,
10 = 2400, 12 = 9600, 16 = 19200, 28 = 38400, 87 = 115200. Non-standard
baud rates fall back to code 1.

**S-registers:** Query with `ATSn?`, set with `ATSn=v`, or type `ATS?` for help.
`AT&W` saves all registers to `egateway.conf`; `ATZ` restores saved values;
`AT&F` resets to gateway-friendly factory defaults.

| Register | Default | Description |
|----------|---------|-------------|
| S0  | 5   | Auto-answer ring count (0 = disabled) |
| S1  | 0   | Ring counter (current) |
| S2  | 43  | Escape character (43 = `+`) |
| S3  | 13  | Carriage return character |
| S4  | 10  | Line feed character |
| S5  | 8   | Backspace character |
| S6  | 2   | Wait for dial tone (seconds) |
| S7  | **15** | Wait for carrier (seconds) — Hayes default is 50; reduced here so failed dials return quickly. Capped internally at 60 s. |
| S8  | 2   | Comma pause time (seconds) |
| S9  | 6   | Carrier detect response time (1/10s) |
| S10 | 14  | Carrier loss disconnect time (1/10s) |
| S11 | 95  | DTMF tone duration (milliseconds) |
| S12 | 50  | Escape guard time (1/50s; 50 = 1 second) |
| S13–S24 | 0 | Reserved. Stored and persisted so legacy init strings that probe these registers don't halt with `ERROR`, but they have no effect on the emulator. |
| S25 | 5   | DTR detect time (1/100s). Reserved — no DTR pin. |
| S26 | 1   | RTS-to-CTS delay (1/100s). Reserved — no RTS/CTS pins. |

Keep `S3`, `S4`, and `S5` at distinct values. Command-mode line editing
dispatches on the raw byte: the CR branch is checked before BS, so setting
`S3 = 8` would cause backspace to terminate the line. Leaving S3/S4/S5 at
their Hayes defaults (13/10/8) avoids this.

### Hayes Compliance Summary

The emulator implements the Hayes Smartmodem AT command set: AT, ATZ, AT&F,
AT&W, AT&V, ATE, ATV, ATQ, ATI (I0–I7), ATH, ATA, ATO, ATX, AT&C, AT&D,
AT&K, AT&Z (stored numbers), ATD (with T/P/L/S variants), ATSn, S-registers
S0–S26, the `A/` repeat-last-command shortcut, and the `+++` escape with
S2/S12 guard-time semantics. `AT&W` persists every Hayes setting — echo,
verbose, quiet, X, &C, &D, &K, all 27 S-registers, and four stored-number
slots — to `egateway.conf`; `ATZ` restores them. Numeric and verbose result
codes honor the ATX level.

Commands the emulator can't meaningfully implement over TCP (`ATB`, `ATC`,
`ATL`, `ATM`, `AT&B`, `AT&G`, `AT&J`, `AT&S`, `AT&T`, `AT&Y`) are accepted
and return `OK` so that legacy init strings run to completion.

**Gateway-friendly default deviations:**

| Setting | Gateway default | Hayes default | Why we differ |
|---------|-----------------|---------------|---------------|
| `AT&D` | `&D0` (ignore DTR) | `&D2` (hang up on DTR drop) | Many retro clients don't drive DTR correctly. `&D2` would cause spurious disconnects. |
| `AT&K` | `&K0` (no modem-level flow control) | `&K3` (RTS/CTS) | C64, CP/M, and similar clients rarely implement hardware flow control. The physical port flow control is still set per-port by `serial_a_flowcontrol` / `serial_b_flowcontrol` in `egateway.conf`. |
| `S7` | 15 seconds | 50 seconds | Keeps failed TCP dials responsive. Raising S7 is allowed up to an internal cap of 60 s. |

All three deviations can be overridden interactively (e.g. `AT&D2`,
`AT&K3`, `ATS7=50`) and persisted with `AT&W`.

**Implementation notes:**

- `AT&D`, `AT&K`, and `AT&C` are parsed, stored, displayed in `AT&V`, and
  persisted — but their effects on RS-232 hardware signalling (DTR monitoring,
  RTS/CTS handshake, DCD line) are not enforced by the emulator. See the
  **Limitations** section below for why.
- `ATX1`–`ATX4` all affect result codes and `CONNECT` formatting.
- `ATS6` (wait-for-dial-tone) and `ATS8` (comma pause) sleep for the
  configured number of seconds before the TCP connect, summed per modifier
  and capped at 60 seconds total.
- The `+++` escape follows the Hayes timing spec (one guard time of silence
  before the `+` triple, then another guard time after). Setting `ATS12=0`
  or `ATS2>127` disables escape detection.

### Escaping and Resuming

The `+++` escape sequence returns to command mode while keeping the connection
alive. Type `ATO` to resume the connection, or `ATH` to hang up. This follows
standard Hayes modem behavior: one second of silence, then `+++`, then another
second of silence.

### Ring Emulator

Telnet and SSH users can simulate an incoming phone call to a serial device
from the per-port settings menu (**I** — Ring emulator).  The Ring item is
per-port: pick **Configuration > M > A** (or **B**) first to choose
which port should ring, then press **I**.  The modem sends `RING`
to that port at standard US phone cadence (every 6 seconds).  After
S0 rings (default 5), the modem auto-answers and the serial device
receives the Ethernet Gateway main menu, just as if it had dialed in
with `ATDT ethernet-gateway`.  The serial device can also answer
manually with `ATA` during ringing.  The two ports' ring slots are
independent — Port A and Port B can be ringing simultaneously.

### Serial Safety

When changing a port's parameters from a serial session, the server asks
for confirmation. If there is no response within 60 seconds (e.g., because
the terminal settings no longer match), the settings are automatically
reverted. This prevents lockout when accidentally misconfiguring the
serial port the operator is connected through.

### Dialup Mapping

The Dialup Mapping feature (per-port menu **D**, reachable from
**Configuration > M > A or B**) lets you map phone numbers to
`host:port` targets.  The mapping table is **shared** across both
ports — `dialup.conf` is one file consulted by both modems — so a
number you've added is dialable from either Port A's or Port B's
modem with `ATDT`, `ATDP`, or `ATD`.  The server looks up the number
and connects to the mapped host instead.

A built-in entry maps **1001000** to the local Ethernet Gateway menu (equivalent
to `ATDT ethernet-gateway`). This entry cannot be deleted.

Mappings are stored in `dialup.conf` (created automatically on first access
with a default starter entry). Phone numbers are matched by digits only --
formatting characters like dashes, spaces, and parentheses are ignored, so
`555-1234` and `5551234` are treated as the same number.

If a dialed number has no mapping, the modem returns `NO CARRIER`. You can
still dial hostnames and `host:port` targets directly -- mappings only apply
when the dial string looks like a phone number (digits and formatting only, no
letters or dots).

### Limitations

This is a software modem emulator, not a real modem. The Hayes command set
(including `AT&C`, `AT&D`, `AT&K`) is fully parsed, stored, persisted via
`AT&W`, and displayed in `AT&V`, but the emulator does not drive the RS-232
hardware signal pins that those commands nominally control:

- **DCD (Data Carrier Detect, pin 1)** -- A real modem asserts DCD when a
  carrier is established with the remote modem. `AT&C1` is accepted and
  persisted, but this emulator does not drive DCD, so the serial device has
  no hardware indication that a connection is active.
- **RI (Ring Indicator, pin 9)** -- A real modem asserts RI when an incoming
  call is ringing. The ring emulator sends `RING` result codes over the
  serial data line, but the RI pin is never driven.
- **DSR (Data Set Ready, pin 6)** -- A real modem asserts DSR when powered
  on and ready. This emulator does not control DSR.
- **DTR (Data Terminal Ready, pin 4)** -- A real modem monitors DTR from the
  terminal to detect hangup requests. `AT&D2`/`AT&D3` is accepted and
  persisted, but the emulator does not read DTR (semantics vary by
  USB-to-serial adapter and platform). Use `ATH` or `+++` to hang up.
- **CTS/RTS (Clear to Send / Request to Send, pins 8/7)** -- `AT&K3`/`AT&K4`
  is accepted and persisted. Actual hardware or software flow control on the
  wire is controlled per-port by `serial_a_flowcontrol` / `serial_b_flowcontrol`
  in `egateway.conf` (not by `AT&K`), so retro clients that can't do RTS/CTS
  keep working at the per-port default of `none`.

Most retro terminal software works fine without these signals, especially
when configured to ignore DCD (sometimes labeled "Force DTR" or "Ignore
Carrier" in the terminal program settings). If your software requires DCD to
be asserted before it will communicate, check its configuration for an
option to disable carrier detection.

### Console Mode (Telnet-Serial Bridge)

When a port is set to **Telnet-Serial Mode**, its Hayes emulator is
disabled and the port becomes selectable in the **G  Serial Gateway**
A/B picker.  Each port toggles independently, so Port A can run the
Hayes modem while Port B serves as a console bridge (or vice versa,
or both as bridges).  This is useful for talking to a microcontroller
or embedded console connected to one of the wires — your telnet/SSH
session becomes a transparent pipe to that port in both directions.

**Switching modes (per port):**

- **GUI:** open the chosen port's **More...** popup from the Serial
  Port frame and set its **Mode** dropdown to **Telnet-Serial Mode**.
  Save reconfigures only the changed port; the other port keeps
  running.
- **Telnet/SSH:** **Configuration > M (Serial Configuration) >
  A or B > T (Toggle Modem/Console mode)**.  The per-port menu's
  banner flips between `MODEM EMULATOR` and `SERIAL CONSOLE`, and
  the Dialup Mapping / Ring Emulator items hide in console mode.
  **T** is hidden from a session that arrived over the modem itself
  (flipping its own port to console would tear down its connection
  before it could acknowledge).
- **`egateway.conf`:** set `serial_a_mode = console` (or
  `serial_b_mode = console`).  The change is hot-applied within one
  manager-poll interval — no restart required.

**Using the bridge:**

1. From the main menu, press **G** (Serial Gateway).  An A/B picker
   appears showing both ports' status; ineligible ports (disabled,
   modem mode, no device) are dimmed.
2. Press **A** or **B** to pick a port.  The gateway prints that
   port's active framing (port, baud, data/parity/stop, flow) and
   asks `Connect now? (Y/N):` — type **Y** to enter the bridge.
3. Bytes flow in both directions until you press **ESC twice in a row**
   (PETSCII `<-` twice on Commodore terminals). A single ESC is
   forwarded to the wire on the next keystroke, so editors that need
   ESC (vi, emacs) keep working.

The bridge is single-user **per port**: only one telnet/SSH session at
a time can hold each port, but Port A and Port B can each host their
own concurrent bridge.  A second request to the same port gets
**Another session is already using Port X** until the first session
disconnects.

The Serial Gateway option is hidden from sessions that came in over a
serial port (you can't bridge a port back into its own bridge), and
also hidden when neither port is in console mode (so the menu doesn't
dead-end at an empty picker).

## Web Browser

The built-in text-mode web browser renders HTML pages as plain text with
numbered link references. It works on all terminal types, including 40-column
PETSCII screens.

### Browsing a Page

1. From the main menu, press **B** (Simple Browser)
2. Enter a URL (e.g. `example.com`) or a search query (e.g. `rust programming`)
   - URLs without a scheme automatically get `https://` prepended
   - Text without dots is treated as a search query and sent to DuckDuckGo
3. The page is fetched, converted to plain text, and displayed with pagination

### Understanding Links

When a page is displayed, clickable links are marked with numbered tags like
**[1]**, **[2]**, **[3]** next to the linked text. To follow a link, press
**L** and enter the link number.

### Page Navigation Commands

| Key   | Action                              |
|-------|-------------------------------------|
| N / P | Next page / Previous page           |
| T / E | Jump to Top / End of page           |
| L     | Follow a link by number             |
| G     | Go to a new URL or search query     |
| S     | Search for text within the page     |
| F     | Fill out and submit forms           |
| K     | Save current page as a bookmark     |
| B     | Go back to the previous page        |
| R     | Reload the current page             |
| H     | Show help                           |
| Q     | Close page (return to browser home) |
| ESC   | Exit browser to main menu           |

### Bookmarks

- Press **K** while viewing a page to save it as a bookmark
- Press **K** on the browser home screen to open your saved bookmarks
- Select a bookmark by number to navigate to it
- Press **D** in the bookmarks list, then enter a number to delete one
- Up to 100 bookmarks are stored in `bookmarks.txt` next to the binary

### Forms

Many web pages contain forms (search boxes, login fields, etc.). When forms are
detected, the status line shows the form count. Press **F** to interact:

1. If multiple forms exist, select one by number
2. Edit fields by entering the field number
   - Text fields: type a new value
   - Select dropdowns: choose an option by number
   - Checkboxes and radio buttons: toggle or select
3. Press **S** to submit the form, or **Q** to cancel

### Browser Limits

- Maximum page size: 1 MB
- Maximum rendered lines: 5,000
- HTTP request timeout: 15 seconds
- Page history depth: 50 pages
- HTTPS connections that fail due to TLS errors automatically retry over HTTP

### Gopher Protocol

The browser supports the Gopher protocol alongside HTTP/HTTPS. Gopher is a
text-native protocol that predates the web and renders beautifully on retro
terminals, including 40-column PETSCII screens.

To browse a Gopher server, press **G** and enter a `gopher://` URL:

```
gopher://gopher.floodgap.com
gopher://gopher.quux.org
```

Gopher directory listings are displayed with numbered links, just like web
pages. Text files are displayed as plain text. Gopher search items (type 7)
automatically prompt for a search query before fetching results. All browser
features (pagination, history, back, bookmarks) work with Gopher URLs.

## AI Chat

AI Chat provides an interactive question-and-answer interface powered by the
Groq API. Requires a Groq API key (see [Setting Up AI Chat](#setting-up-ai-chat)
above).

1. From the main menu, press **A** (AI Chat)
2. Type a question and press Enter
3. The server shows "Thinking..." while waiting for the response
4. The answer is displayed with pagination (**N** next, **P** previous, **Q** done)
5. From the answer screen, type a new question to continue the conversation,
   or press **Q** to return to the main menu

Responses are word-wrapped to fit the terminal width (38 columns for PETSCII,
78 for ANSI/ASCII).

## Weather

The Weather feature displays current conditions and a 3-day forecast for any
US zip code, powered by [Open-Meteo](https://open-meteo.com) (free, no API
key required).

1. From the main menu, press **W** (Weather)
2. Enter a 5-digit US zip code, or press Enter to use the last one
3. Current temperature, humidity, wind, and a 3-day forecast are displayed

The last-used zip code is saved to `egateway.conf` so it becomes the default
for all future sessions.

## Signals

The server handles POSIX signals for graceful shutdown:

- **SIGINT** (Ctrl+C) -- Shut down, notify all connected sessions
- **SIGTERM** -- Shut down (e.g., from `kill` or systemd)
- **SIGHUP** -- Shut down

## Disclaimer

This software is provided on an "as is" basis, without warranties of any kind,
express or implied. Use at your own risk. The author is not responsible for any
data loss, security breaches, or damages resulting from the use of this
software. The user is solely responsible for securing their own network,
credentials, and data. Telnet is an inherently insecure protocol -- do not use
this software on untrusted networks.

Portions of this project were developed with the assistance of AI tools 
including Claude Code.

## License

This project is licensed under the [GNU General Public License v3.0 or later](https://www.gnu.org/licenses/gpl-3.0.html) (GPL-3.0-or-later).
