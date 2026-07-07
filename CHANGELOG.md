# Changelog

All notable changes to **ethernet-gateway** are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.6.4] - Unreleased

### Added
- **Configurable desktop GUI display scale (`gui_zoom`).** The console window
  now honors a `gui_zoom` setting: `auto` (default) follows the monitor's own
  scale factor as before, while a number (e.g. `1.0`, `1.25`, `0.8`) pins the
  window's pixels-per-point absolutely so a display that reports an inflated
  DPI no longer renders the GUI oversized. Selectable as "Display scale" from
  the GUI's Server → More panel and the web config's Server → More page
  (Auto / 75% / 100% / 125% / 150% / 200%), and clamped to 0.5–3.0.
- **Show the file being downloaded on the SELECT PROTOCOL screen.** The
  download protocol picker now displays the file name (truncated to the
  terminal width) and byte size above the protocol list, so the user can
  confirm the right file before choosing a protocol.
- **Make directories from the telnet file-transfer menu.** A new **M** option
  creates a subdirectory inside the current transfer working directory (the
  name is validated like a filename — a single component, no `..` or `/`), then
  asks whether to make it the working directory.
- **Weather falls back to MET Norway when Open-Meteo is unreachable.** If the
  Open-Meteo forecast host can't be reached, the Weather menu now automatically
  retries the forecast against MET Norway (`api.met.no` Locationforecast 2.0 —
  free, no API key, independent infrastructure), reusing the coordinates
  already geocoded via Open-Meteo. MET's Celsius/m-per-s data is converted to
  °F/mph and its symbol codes mapped to descriptions; you only see an error if
  both providers fail.
- **Wait for the receiver before starting a Kermit download
  (`kermit_wait_for_receiver`, default on).** A Kermit transfer is
  receiver-driven at the start — the receiving side sends a `NAK` to solicit the
  sender's Send-Init (Frank da Cruz, *Kermit Protocol Manual* §4). The gateway
  now holds its Send-Init until that poke arrives and then sends exactly one, on
  both interactive downloads and Kermit server GET responses. A client that
  never pokes (e.g. C-Kermit) falls through a short bounded wait and gets an
  unprompted Send-Init as before. Wired into the telnet Kermit-settings menu
  (**G**), the web config page, and the GUI.

### Changed
- **Weather fetch fails fast with a clearer message.** The Open-Meteo request
  now uses a 5 s connect timeout (was a single 15 s global) and retries once on
  a transient transport failure, so an unreachable/blocked forecast host no
  longer hangs the Weather menu for 15 s. Errors are distinguished:
  "Zip code not found." (bad zip) vs "Weather service unreachable. Try again
  later." (network/host down) vs "Weather service returned bad data." (parse).

### Fixed
- **Kermit server GET is now case-insensitive.** A client requesting a file in
  a different case than it is stored on disk — CP/M clients such as kercpm3
  uppercase filenames — no longer fails "File not found" and burns a retry
  re-requesting under another case. The server prefers an exact match, then
  falls back to a case-insensitive match among the transfer directory's direct
  entries, so the path-traversal protection is unchanged.
- **Kermit server downloads no longer provoke spurious retransmissions on
  vintage receivers.** The server was sending its Send-Init unsolicited; a
  receiver-driven client (e.g. kercpm3 on CP/M) pokes with a `NAK` to solicit
  it, that poke crossed our Send-Init on the wire, and we resent it — delivering
  a duplicate the client tallied as a retry. The server now waits for the poke
  and answers with a single Send-Init. Combined with the case-insensitive fix
  above, this cuts the retry count such clients report on a clean download from
  2–3 down to the single, unavoidable initiating `NAK` that the Kermit
  receiver-driven start requires (uploads read 0 — there the client is the
  sender and never pokes). Documented in `usermanual.html` and `kermit.html`.
- **Web browser surfaces the real error when the AI chat API rejects a
  request.** The Groq client treated every non-2xx response as an opaque
  transport error (`http status: 401`) and discarded the JSON body, so its
  code to extract Groq's descriptive `error.message` (e.g. "Invalid API Key",
  rate-limit text) never ran. It now reads the body on error responses and
  reports Groq's own message.
- **ZMODEM downloads are no longer throttled to ~5 KB/s on fast links.** When
  reading a hex header the receiver drained up to three trailing bytes (CR, LF,
  and an optional XON), but Forsberg's `zsendhdr` omits the XON for `ZACK` and
  `ZFIN` frames — so on those the drain blocked the full 200 ms per frame
  waiting for an XON that never comes. Because our sender ACK-gates every
  1 KB subpacket (`ZCRCQ` → read `ZACK`), that phantom wait capped throughput
  near one subpacket per 200 ms regardless of link speed. The receiver now
  waits for the third trailing byte only for frame types that actually carry
  it. No wire bytes change; slow retro links are unaffected (a subpacket's own
  transmission already dwarfs 200 ms there).
- **Plain XMODEM sends no longer report a false failure at `xmodem_max_retries
  = 1`.** A Forsberg-compliant receiver NAKs the *first* `EOT` to verify
  end-of-file and ACKs only the resent one (our own receiver does this), so
  completing the handshake requires at least two `EOT` attempts. The send-side
  `EOT` loop was bounded by `xmodem_max_retries`, so at the minimum setting of
  1 it sent a single `EOT`, took the expected verification `NAK` as failure,
  and reported an error on a transfer that had actually succeeded. The `EOT`
  budget now floors at 2; a receiver that ACKs the first `EOT` still returns on
  the first pass, so the common case is unchanged.
- **Serial dial-out stays responsive to shutdown and config restarts.** When
  an `ATDT` target resolved to several unreachable addresses, the modem tried
  each in turn and could block the serial thread for (address count × the S7
  timeout) — during which a server shutdown or a per-port config restart was
  stalled. The dial loop now checks the shutdown/restart flags between address
  attempts and bails with `NO CARRIER`. The peer-dial answer-wait is likewise
  clamped to the same 60 s ceiling `ATDT` uses, so a large S7 can't pin the
  caller's port for up to 255 s while a local peer rings.

### Security
- **SSH server refuses to overwrite an unreadable host key.** If the host-key
  file existed but failed to parse (e.g. truncated by a full disk), the server
  silently generated a new key and wrote it over the old one — changing the
  server's SSH identity and tripping every client's "REMOTE HOST
  IDENTIFICATION HAS CHANGED" warning (and potentially clobbering a merely
  truncated, recoverable key). It now refuses to start the SSH server in that
  case, leaving the file untouched for the operator to restore or remove, the
  way `sshd` treats a bad host key. A *missing* key file is still generated
  normally on first run.
- **Punter receive can no longer be hung by a flood of empty blocks.** A peer
  that streamed valid-checksum, non-final, zero-payload blocks would spin the
  receive loop forever: an empty block never grows the output (so the file-size
  cap never trips) and passes the checksum (so the bad-block cap never trips).
  A conformant C1 sender emits exactly one header-only block per phase (block 0,
  which only announces block 1's size), so the receiver now bounds the number
  of accepted empty non-final blocks and gives up on a peer that exceeds it.
- **Text-mode web browser can no longer be crashed by a deeply-nested page.**
  A page whose HTML nested tags tens of thousands deep (e.g. unclosed `<div>`s,
  well under the 1 MB body cap) parsed into a DOM so deep that any recursive
  walk over it — the title/form extractors, html2text's render pass, or even
  the tree's own recursive `Drop` — overflowed the worker-thread stack and
  aborted the **entire gateway process** (all telnet/SSH sessions), a
  remotely-content-triggered denial of service. The browser now rejects a
  document nested deeper than 256 element levels ("Page is too deeply nested to
  render.") and tears the parsed tree down iteratively so even discarding it
  cannot overflow the stack.
- **Refreshed dependencies to clear RustSec advisories.** `cargo update`
  moved `aes` (yanked) → 0.9.1, `memmap2` (RUSTSEC-2026-0186 unsound) → 0.9.11,
  dropped `anyhow` (RUSTSEC-2026-0190 unsound), and bumped the egui/eframe stack
  to 0.34.3 and `russh` to 0.60.3. The two `quick-xml` DoS advisories
  (RUSTSEC-2026-0194/0195) are waived in `.cargo/audit.toml`: `quick-xml` is a
  build-time proc-macro dependency (`wayland-scanner`) that parses trusted
  Wayland protocol XML at compile time — it is not in the shipped binary and the
  gateway does no runtime XML parsing, so neither DoS path is reachable.

## [0.6.3] - 2026-07-03

### Added
- **The desktop GUI remembers its window position and size.** The
  configuration window now reopens where you last left it — its outer position
  and inner size are saved (debounced) to `gui_window_geometry` in
  `egateway.conf` and restored on the next launch. It is auto-managed: there is
  no config-UI field for it, and an empty value means "use the default size and
  let the window manager place it." Works on X11/Windows/macOS; Wayland
  compositors don't expose a window's position, so it isn't remembered there.
- **Peer-dial: call another serial port directly.** With the new
  `allow_peer_dial` opt-in (default off; wired into telnet **Configuration > M >
  P**, web, and GUI), a modem-mode port can dial another port by address —
  `ATD <Port>@<IP>` (e.g. `ATD B@192.168.1.50`) — or select that port in the
  Serial Gateway menu, and bridge straight through to the device on it (the
  gateway equivalent of calling a friend's modem). A **modem-mode** target
  *rings* and answers per its own AT rules (`S0` auto-answer / manual `ATA`); a
  **console-mode** target connects directly. The connection is a transparent
  byte pipe, so a file-transfer protocol runs end to end between the two
  devices. Result codes follow ATX (`CONNECT`/`BUSY`/`NO ANSWER`/`NO CARRIER`).
  Works on the same gateway and, **over the master/slave relay, from a slave
  device to a port on its master** (`ATD <Port>@<master-ip>`): the slave relays
  the call and the master resolves the address to one of its own ports and
  rings/connects it (gated by the master's `master_accept_relays` +
  `allow_peer_dial`). Cross-gateway is symmetric: the master routes a peer
  address to **any** port a slave has registered — a slave's **console** port
  and its **modem** port (a slave modem port announces itself to the master and,
  when dialed, *rings* the attached device) — so `<Port>@<slave-ip>` reaches a
  slave's port from the master or, via the master as a crossbar, from another
  slave (device ↔ slave-A ↔ master ↔ slave-B ↔ device). Addressing is by IP, so
  gateways need distinct addresses (normal for separate machines). See README
  "Peer-Dial" and user manual §9.2.3.
- **Live relay status in the telnet Master/Slave screen.** A master now lists
  the remote console ports slaves have registered (so you can see connected
  slaves at a glance); a slave shows each console port's link state to its
  master (`down`/`connecting`/`registered`/`bridging`) — relay connectivity is
  now visible without reading the logs.
- **Relay channel handshake / protocol version.** The master now writes a small
  hello (`EGR` magic + a protocol-version byte) as the first bytes on every
  accepted master/slave relay or console-registration channel; the slave
  validates it before using the channel. A master/slave version skew now fails
  cleanly with an "upgrade the older gateway" message instead of desyncing, and
  a slave pointed at a master that is declining relays (`standalone`,
  `master_accept_relays=false`, or at capacity) now detects the refusal — the
  absence of the hello — and backs off with a clear message, instead of
  mistaking the refused-but-open channel for a live registration and idling.
- **Optional hardware carrier (DCD) signalling.** New per-port opt-in
  `serial_a_drive_carrier` / `serial_b_drive_carrier` (default `false`; also a
  checkbox in the GUI/web config and the **C** key in the telnet per-port modem
  menu). When enabled, the modem emulator drives **DTR** as a carrier proxy
  (a PC/USB-serial adapter is a DTE and can't drive a DCD *output*, so you cross
  DTR→DCD in a null-modem cable, as tcpser does), following `AT&C`: `&C0` forces
  it always asserted while the port is open, `&C1` (default) asserts on
  `CONNECT` and drops on `NO CARRIER` / `ATH` / hangup / relay-link-loss (so a
  slave-attached machine sees loss-of-carrier in hardware too). **When off, the
  gateway makes zero modem-control-line calls**, so ports without DCD wiring are
  byte-for-byte unaffected. Modem mode only.
- **Master/Slave serial extender (optional).** A gateway set to
  `gateway_role = slave` extends its serial ports to a `master` gateway over
  the master's existing SSH port; the serial device reaches the master's menu,
  file transfer, and dial-out as if attached to the master, and **files always
  land on the master**. Default `gateway_role = standalone` leaves the feature
  entirely inert. Modem-mode ports relay on connect (the slave resolves its
  *local* dial map; the master dials onward — "resolve local, dial central");
  console-mode ports register with the master and appear in the master's Serial
  Gateway picker (local ports + registered remote ports). New config keys
  (telnet/web/GUI): `gateway_role`, `master_accept_relays`, `slave_master_host`,
  `slave_master_port`, `slave_master_username`, `slave_master_password`,
  `relay_transport` (only `ssh` implemented). The slave authenticates with the
  master's unified username/password and pins the master's SSH host key (TOFU,
  in `gateway_hosts`); relay connections are gated by `master_accept_relays` and
  count against the session cap. The slave's main menu shows a SLAVE-mode notice
  with the master address, and reconnects automatically if the link drops.
- **Serial sessions can now receive administrative broadcasts.** A process-global
  broadcast channel (`serial::broadcast_to_serial`) fans a message out to every
  open serial port, delivered at the **command prompt only** — an in-call
  (online) serial session, which may be carrying a binary file transfer, drains
  its queued messages when it next returns to command mode (`+++`, hangup, or
  call end) so a notice can never corrupt a transfer. This is the serial-side
  counterpart to the telnet/SSH/relay `broadcast_to_sessions` list, completing
  broadcast coverage across all connection types. The shutdown "Goodbye" keeps
  its own reliable shutdown-flag write (which fires even mid-online) and is not
  routed through this channel. Modem mode only. (Extension point: no production
  broadcast is routed to it yet — the first admin-notice feature plugs in here.)

### Fixed
- **Serial `AT&C` now updates the hardware carrier (DCD/DTR) line immediately.**
  With `serial_X_drive_carrier` enabled, changing `AT&C` at the command prompt
  used to take effect only at the next connect/hangup; it now re-applies the
  DCD line right away — `&C0` asserts DTR (carrier forced on regardless of call
  state) and `&C1` restores follow-the-carrier — matching the documented
  contract and the existing `ATZ`/`AT&F` behavior. Found during on-hardware DCD
  validation (DTR→DCD crossover).
- **GUI console started as a boot service now waits for the window manager.**
  When launched as a boot-time systemd service, the console window could come
  up undecorated (no title bar / minimize / close) or with its title bar tucked
  under the desktop panel, because it opened as soon as the X server accepted a
  connection — before the window manager had taken over decoration and
  placement. The display-wait now also waits (bounded, X11-only) for an EWMH
  window manager (`_NET_SUPPORTING_WM_CHECK` on the root window) before opening
  the window. Degrades safely: no `xprop`, a bare X server, or a non-EWMH WM
  falls through after a short cap and opens anyway, and the server is never
  delayed (only the window waits). Non-X11 targets (Windows, macOS, headless,
  pure-Wayland) are unaffected — the wait returns immediately without `DISPLAY`.
- **Serial Gateway menu shows peer-dial addresses without spaces around `@`.**
  Remote (slave) port entries are now displayed as `<Port>@<ip>` — exactly the
  string you type to dial them (`ATDT <Port>@<ip>`). The previous spaced form
  (`<Port> @ <ip>`) invited mistyped dial strings with embedded spaces. The
  remote-bridge screen title and the master's registered-ports status list were
  unspaced to match.
- **Master/Slave configuration now guides the operator by role.** Across the
  telnet menu, web, and desktop GUI, fields that don't apply to the selected
  role are greyed out / disabled: *accept relays* is editable only for a
  **Master** (and now defaults **on** when you switch to Master, since a master
  with it off can't accept slaves), while the master host / port / user / pass
  are editable only for a **Slave**. Switching to Master while the SSH server is
  off now surfaces a warning (a popup in web/GUI, a dedicated screen in telnet)
  explaining that slaves connect over SSH — it points you at the setting but
  never toggles SSH for you.
- **Peer-dial now reminds you about local echo.** A peer-dial connection is a
  transparent link with no host echoing keystrokes back, so the Serial Gateway
  picker shows a "enable local echo to see typing" tip, and the README /
  user-manual peer-dial sections explain that each terminal needs local echo
  (half-duplex) — and that `ATE` does not affect the online data path.
- **Shutdown "Goodbye" now reaches every session, not just when telnet is
  enabled.** The shutdown broadcast used to live inside the telnet accept loop,
  so an SSH-only deployment (`telnet_enabled = false`) tore SSH and relay
  sessions down with no notice. It is now a transport-neutral broadcast invoked
  centrally at shutdown, so telnet, SSH, and master/slave relay sessions all
  receive it for any combination of enabled servers (serial ports already emit
  their own notice). The mechanism is reusable for future all-session messages.
- **File transfers over telnet no longer apply NVT CR-NUL stuffing**, which
  corrupted binary transfers through telnet↔serial bridges (e.g. tcpser) and
  telnet-aware WiFi modems that don't symmetrically un-stuff. The shared
  transfer I/O layer (`tnio`, used by XMODEM/YMODEM/ZMODEM/Kermit/Punter) now
  escapes only IAC (`0xFF` → `IAC IAC`) and passes every other byte —
  including CR (`0x0D`) — through literally, matching RFC 856 binary-transmission
  semantics that 8-bit file transfer requires. CR-NUL stuffing (RFC 854 §2.2)
  is a text-mode rule and was inserting/deleting `0x00` bytes around `0x0D`,
  which manifested as endless mid-transfer checksum failures and a hung peer
  (a Commodore Punter sender, whose `S/B` wait loops are unbounded, would
  strand). Validated against the genuine CCGMS Punter reference
  (`ccgmsterm/test/punter.c`) in both directions, including through a
  telnet-bridge emulation. IAC escaping (the **I** toggle) is unchanged.
- **GUI: external changes to the Kermit idle-timeout are no longer reverted on
  save.** `kermit_idle_timeout` was rendered and saved in the desktop config
  editor but missing from its refresh-from-global and dirty-detection paths, so
  a value changed via the web/telnet UI while the GUI was open could be silently
  overwritten by the GUI's stale field on the next Save.
- **Serial modem mode now auto-reconnects when the device behind the port
  disappears** (e.g. a `socat`/USB-serial bridge that exits when its attached
  terminal closes). Command-mode previously hit a hard I/O error and re-looped,
  spamming the error ~twice/second forever with no recovery; it now logs the
  outage once, backs off 1 s, and reopens the port automatically when the device
  returns — matching console mode.
- **`ATDT` to a hostname now tries every resolved address.** Dialing resolved
  via `to_socket_addrs()` but only attempted the first address, so a host whose
  DNS returns an unreachable IPv6 record first could fail with a silent
  `NO CARRIER` even when a working IPv4 address followed. It now attempts each
  resolved address until one connects, and logs the failure reason instead of
  failing silently.
- **Config save failures are now surfaced.** `write_config_file`/`save_config`
  return a `Result`; the explicit-save paths (desktop GUI Save buttons, telnet
  reset-to-defaults) report a failure instead of always logging success.
- **Hand-edited `serial_*_parity` / `serial_*_flowcontrol` values are honored.**
  Both are now normalized (trim + lowercase) on read and apply, consistent with
  `mode`, so e.g. `serial_a_parity = Even` no longer silently reverts.
- **Config values round-trip without whitespace drift.** `sanitize_value` now
  trims surrounding whitespace (the reader already trimmed), and the dialup
  number/host are sanitized on save so an embedded newline can't corrupt
  `dialup.conf` framing.
- **GUI waits for the X display before opening the console window**, fixing the
  headless drop when the gateway is started as a boot-time service before the
  desktop session's X auth cookie is ready. The wait is adaptive (no delay on a
  normal manual launch) and degrades safely when there is no display.
- **Kermit's async server/receive paths no longer stall a runtime worker** —
  blocking `std::fs` calls moved to `tokio::fs` and the directory listing
  offloaded via `spawn_blocking`.

### Security
- **SSH: warn when a pre-existing host/client private key is group- or
  world-readable.** New keys are written `0600`; a key restored from a backup or
  created by an older build could be more permissive. The gateway now logs a
  `chmod 600` recommendation on load (warn-only — it does not refuse the key,
  matching the trusted-LAN threat model).
- **ZMODEM: bound consecutive empty data subpackets** (`MAX_EMPTY_SUBPACKETS`)
  so a peer can't tar-pit the receive loop with CRC-valid zero-progress
  subpackets.
- **Telnet: bound in-subnegotiation reads** (`SB_DRAIN_TIMEOUT`) so a peer that
  opens an `IAC SB` and then stalls can't pin the reader (slowloris); the outer
  idle wait is unchanged.

## [0.6.2] - 2026-06-19

### Added
- **Session cap and idle timeout are now editable from the telnet Server
  Configuration menu** (the `C` and `D` keys), matching the desktop GUI and the
  web configuration page that already exposed `max_sessions` /
  `idle_timeout_secs` — completing three-UI parity for both settings. Idle
  timeout accepts `0` to disable the idle disconnect. The screen's detected-IP
  hint list is now capped so the new row keeps the PETSCII menu within its
  22-row budget even on a multi-homed host (it previously overflowed at three or
  more private addresses).

### Security
- **Fixed an SSRF-guard bypass for IPv6-literal URLs in the text-mode web browser.** `guard_public_url` classified IP literals with `IpAddr::parse`, but `url::Url::host_str()` returns IPv6 literals *bracketed* (e.g. `[::1]`), which fails that parse and fell through to the resolver path — allowing `http://[::1]/`, `http://[::ffff:127.0.0.1]/`, and the like to reach loopback / link-local / internal IPv6 services (initial request and every redirect hop). The guard now strips the brackets before classifying, blocking the entire internal IPv6 space. Regression test added. IPv4 literals and DNS names were already handled correctly.
- **SSH: an unauthenticated connection no longer consumes a session slot.** `new_client` incremented the session counter for every inbound TCP connection, before authentication, so a peer that opened many transport handshakes and stalled could exhaust `max_sessions` and lock out real users. The slot is now claimed only on a successful login (atomic `fetch_add` + rollback, mirroring the telnet accept loop) and released only if it was claimed — and the cap is now exactly `max_sessions` (was off-by-one, `max_sessions + 1`).
- **Web config: `POST /save` now enforces a same-origin check (CSRF defense-in-depth).** A request whose `Origin`/`Referer` doesn't match our `Host` is rejected with 403, blocking a malicious page from riding the operator's cached Basic-auth credentials to rewrite config (including disabling auth). Requests with neither header (non-browser clients such as `curl`, which can't be a CSRF vector) are still allowed; Basic auth continues to gate them. Lenient-on-absent by design for the trusted-LAN threat model.
- **Kermit server: defense-in-depth subdir re-validation on save.** Both the in-session receiver and the standalone (auth-bypassing) Kermit listener now re-check `rx.subdir` with `is_safe_relative_subdir` before joining it to the transfer dir. No live traversal existed (subdir is only set after that same check inside the Kermit module), but re-validating at the save site closes the door on any future producer-side bypass — the same belt-and-suspenders rationale as the existing filename re-check.

### Fixed
- **Serial console bridge: a stalled telnet peer can no longer wedge server shutdown / port restart.** The dedicated serial-reader thread used an unbounded `blocking_send` onto a bounded channel; when a bridged peer stopped reading and the channel filled, the thread parked past its shutdown/restart checks. It now polls with `try_send` + a short sleep, bailing on shutdown/restart or when the async pump drops its receiver.
- **Serial modem online mode (TCP): a remote host that stops reading no longer blocks shutdown.** `online_mode_tcp` set only a read timeout, so a full remote receive window parked `write_all` indefinitely with the loop's shutdown/restart checks unreachable. A 5 s write timeout is now set (matching the duplex path); an expiry drops carrier (NO CARRIER).
- **XMODEM/YMODEM: YMODEM block 0 is now always validated as CRC-16.** If block 0 took enough retries to cross the negotiation's CRC→checksum fallback point, the block-0 body (and then the data phase) could be misread as a 1-byte checksum, NAK-looping a CRC-only YMODEM sender to exhaustion. The block-0 read and the post-block-0 data phase are now pinned to CRC-16.
- **Logging survives a poisoned lock.** `logger` now recovers a poisoned mutex (`into_inner`) instead of silently dropping the line — matching `config.rs` / `gui.rs`, and most valuable exactly when a thread has just panicked.
- **Kermit streaming: a sequence-aliased NAK now aborts cleanly instead of silently corrupting the file.** In streaming mode the whole file sits in the sender's outstanding-packet set with wrapping (mod-64) sequence numbers, so a file larger than ~64 chunks aliases each seq across many packets. On a genuine mid-stream NAK/loss the sender matched the NAKed seq to the *first* (oldest) outstanding packet sharing it and retransmitted that stale packet; the receiver appends D-packets by sequence with no position field, so it landed the wrong data at the wrong offset. This was benign on lossless TCP/SSH (streaming's intended transport, where NAKs don't occur) and only reachable on an unreliable link such as a serial bridge. An unresolvable NAK now aborts with an actionable error ("disable `kermit_streaming` for this peer"); the timeout-driven retransmit path skips aliased seqs for the same reason. The reliable-transport happy path is unchanged.
- **ZMODEM: `ZFERR` (0x0C) is now handled instead of ignored.** A sender's file read/write-error frame aborts the receive cleanly with an informative error rather than falling through to the ignore arm and waiting out a frame timeout. Every Forsberg 1988 frame is now handled.
- **Text-mode web browser: fixed a remote-triggerable panic on Back.** Returning to a previous page whose re-fetched content is shorter than the saved scroll position could index past the page and panic the session task. The scroll position is now clamped on restore and again defensively at render time.

### Documentation
- **Documented ZMODEM `ZCOMMAND` (frame 0x12) as the one optional spec frame deliberately not implemented** — it is recognized but always refused (non-zero `ZCOMPL`), since arbitrary `/bin/sh -c` execution on a shared, long-lived host is an unacceptable default; use SSH for shell access. Noted in the user manual and the ZMODEM web reference.
- Documented previously-undocumented config keys: `web_enabled`, `web_port`, `gateway_debug`, and `ssh_gateway_auth` in the README config reference, and `punter_max_bad_rounds` / `punter_hangup_on_failure` in the user manual. Added the now-handled `ZFERR` frame to the ZMODEM web reference, and corrected the SSH reference's `auth_password` lifecycle description to match the new claim-slot-on-successful-login behavior.
- README config-reference completeness pass: the "All options" `egateway.conf` sample now lists `disable_ip_safety` and the per-port `serial_a_petscii_translate` / `serial_b_petscii_translate` keys (all three are written by the config saver), the telnet Server-Configuration menu walkthrough documents the new session-cap / idle-timeout keys, and the Other Settings list now includes the gateway debug-trace toggle.

## [0.6.1] - 2026-06-06

### Added
- **Raspberry Pi 4+ (aarch64 Linux) build** — releases now ship an
  `Ethernet_Gateway-aarch64.AppImage` alongside the existing
  x86_64 Linux / Windows / macOS artifacts, built on a native arm64
  runner. Two ARM-only desktop-GUI fixes make it run on the Pi's
  VideoCore/V3D GPU: the wgpu device now requests exactly the limits
  the adapter advertises (so startup no longer aborts with
  "Limit 'max_color_attachments' value 8 is better than allowed 4" or
  the equivalent for other limits), and the GUI prefers the OpenGL ES
  backend instead of the Pi's incomplete Vulkan driver (which panicked
  with "Requested feature is not available on this device").
  `WGPU_BACKEND` still overrides. Other platforms are unaffected.
- **Punter (C1) file-transfer protocol** — the protocol CCGMS /
  Novaterm / StrikeTerm speak natively on Commodore BBSes, added
  alongside XMODEM/YMODEM/ZMODEM/Kermit. Single-file C1 with the full
  two-phase (file-type then data) handshake, both block checksums
  (16-bit additive + cyclic), the "size of next block" framing, and
  the three-`S/B` end-off real C1 endpoints expect. Selectable in the
  telnet upload/download protocol pickers; the outbound PRG/SEQ file
  type is auto-detected from the filename. New `punter_*` tunables
  (block size, timeouts, retries) are editable from the telnet File
  Transfer settings menu, the web configuration page, and the desktop
  GUI, and persist to `egateway.conf`. The send/receive entry points
  take an open stream so a future Multi-Punter (MPP) batch wrapper can
  layer on without touching the wire code.
- **Serial modem `AT+PETSCII=n` command** — toggles PETSCII⇄ASCII
  translation on direct-TCP dials (`AT+PETSCII=1` on, `AT+PETSCII=0`
  off) so a Commodore 64/PET dialing `ATDT host:port` sees readable
  text instead of raw ASCII. Set-only, in the ITU-T V.250 `+`
  extension namespace (`&P` is the pulse-dial make/break ratio on real
  Hayes modems, so it is intentionally left alone). `AT+PETSCII=1`
  persists the setting immediately; `AT&V` reports it as `+PETSCII:n`.
- **PETSCII translation is now editable from every configuration
  surface** — the per-port modem screen in the telnet/serial-console
  menu, the web configuration page, and the desktop GUI — in addition
  to the AT command. It is a per-serial-port setting saved to
  `egateway.conf`.
- Serial: inbound PETSCII punctuation normalizer, and the C64 PETSCII
  DEL key (0x14, INST/DEL) is accepted as a command-line backspace
  when PETSCII translation is active. `+++` escape sequences are
  traced when the gateway debug trace is on.
- **Persisted `gateway_debug` byte-trace flag**, toggleable from the
  GUI/web General frame and the telnet Other Settings / Serial
  Configuration menus. Read fresh per gateway session (no restart
  needed); `EGATEWAY_GATEWAY_DEBUG` still forces it on. The trace
  timestamps each input byte, emits a one-shot `[gw-diag]` terminal
  diagnostic per session (detected type and how it was decided, the
  announced TERMINAL-TYPE, the color decision, advertised telnet
  options, NAWS window size, and — for serial callers — the port's baud
  and PETSCII-translate state, the most common cause of missing ANSI
  color on a serial line), and logs every AT command the modem emulator
  runs alongside a plain-English description of its effect.
- **Web protocol reference pages** served by the configuration web
  server — per-protocol references (XMODEM, YMODEM, ZMODEM, Kermit, the
  Hayes AT command set, and telnet), each documenting that protocol's
  retry/recovery behavior, plus character-set and ANSI escape-sequence
  references, reachable from a new References nav entry.
- **Kermit resume and locking-shift settings are now editable** from
  the telnet Kermit settings menu, the web configuration page, and the
  desktop GUI (previously `egateway.conf`-only).
- **`punter_hangup_on_failure`** — optional drop-carrier-on-give-up for
  Punter, editable from the telnet / web / GUI Punter settings. Because
  C1 has no in-band abort, a give-up otherwise leaves the C64 hung;
  enabling this drops carrier so it sees loss-of-carrier instead.
- **Cooperative TTYPE/NAWS negotiation is now toggleable from the telnet
  session's Gateway Configuration menu** (the `C` key), matching the web
  configuration page and desktop GUI that already exposed
  `telnet_gateway_negotiate`. The menu now shows its on/off state next to
  the telnet-mode and SSH-auth rows.

### Fixed
- AI chat: a follow-up question that merely starts with a menu command
  letter (e.g. "Quantum…") is no longer swallowed by the answer-screen
  navigation. A lone command letter still navigates; any longer line
  is sent to the model.
- **Transfer retry/recovery brought to strict spec.** XMODEM/YMODEM now
  NAK on a data-phase inter-block timeout (re-prompting the sender) and
  cancel with CAN×3 on a non-duplicate block-sequence error instead of
  NAK-looping; ZMODEM routes every data-phase error through one bounded
  counter that re-sends ZRPOS and resets on progress (no infinite ZRPOS
  loop on a permanently-corrupt stream); Kermit emits an Error packet
  when it gives up so the peer is told rather than left waiting.
- **Punter no longer strands a peer on a failed transfer.** A cancel /
  restart from the C64 side is tolerated (longer pre-transfer input
  drain), and corrupt-block recovery is bounded by its own larger round
  cap rather than quitting early and leaving the peer hung.
- **Plain XMODEM now verifies EOT (Forsberg NAK-first-EOT).** The
  receiver NAKs the first EOT and accepts end-of-file only on a resent,
  confirming EOT, so a stray `0x04` from UART line noise in the
  inter-block gap can no longer be mistaken for end-of-file and silently
  truncate an upload to a C64 / CP/M / RC2014 peer. The duplicate-block
  re-arm logic also keeps a non-standard "resend last block on NAK"
  sender from looping. YMODEM keeps immediate-ACK on EOT — its block-0
  size field and end-of-batch handshake already detect a short file.
- **Serial AT parsing hardened.** A command-mode byte ≥ `0x80` (PETSCII
  line noise, or a C64 in lower/upper-case mode sending shifted letters)
  no longer panics the tokenizer and kills that port's modem thread:
  `parse_at_command` returns `ERROR` on non-ASCII input and high bytes
  are filtered at the command-buffer inputs. CR+LF / LF+CR pairs collapse
  to a single terminator so a CRLF terminal no longer runs a spurious
  empty command, and the ring-wait loop honors a per-port restart.
- **Web configuration server lockout / POST hardening.** Credential-less
  requests — the first half of an HTTP Basic challenge plus the
  subresource probes that repeat it — no longer count toward the shared
  per-IP brute-force lockout (only a present-but-wrong credential does),
  so ordinary page loads can't lock out a first-time user. A malformed
  `POST /save` body (non-UTF-8 or zero-length) is now refused instead of
  writing an all-`false` field set that silently disabled
  telnet / SSH / web / security in one shot.

### Changed
- Removed the duplicate Port A/B status banner from the main
  configuration menu — per-port mode is already shown under Serial
  Configuration.
- **Punter bad-block cap decoupled** — `punter_max_bad_rounds` (default
  30) bounds consecutive corrupt-block resend rounds separately from
  `punter_max_retries`, since a real C64 peer never caps resends and a
  low shared cap made the gateway give up first and strand it.

### Security
- **Updated `russh` 0.60.2 → 0.60.3** to clear two high-severity
  (CVSS 7.5) allocation-DoS advisories in the SSH stack:
  RUSTSEC-2026-0154 (`russh` unbounded 32-bit allocation) and
  RUSTSEC-2026-0153 (`russh-cryptovec` unchecked `CryptoVec`
  allocation/growth). A malicious SSH client could otherwise drive
  unbounded memory allocation on the SSH listener.
- **Closed a web-browser POST-redirect SSRF.** The text browser's
  form-submit path used the HTTP client's automatic redirect, so a
  public form action that 30x-redirected to an internal address
  (loopback, link-local metadata, or LAN) was dialed before the SSRF
  guard ran — the final-URL check blocked only rendering, not the
  connection. POST redirects now follow through the same fully-guarded
  fetch path as GET, so the connection itself is refused.

## [0.6.0] - 2026-05-24

### Added

#### Configuration web server
- **Optional HTTP listener** that renders the same settings page the
  desktop GUI does, in a browser.  Off by default; toggle in the GUI
  Server frame (new "Web Server" row between Telnet and Kermit) or
  the telnet `Configuration > Server Configuration` menu's
  `W` / `B` keys.  Port defaults to 8080.
- **Hand-rolled HTTP/1.1 on tokio** (no new dependencies) implementing
  `GET /` (settings page), `GET /logo.png` (the same logo the GUI
  uses), `GET /logs` (2-second polled log tail), `GET /serial-ports`
  (live device enumeration for the dropdown refresh), and
  `POST /save` (config persist + optional restart).
- **Per-frame Save buttons** matching the GUI's three behaviors:
  Server's *Save and Restart* (full server restart cycles through
  `main.rs` exactly the way the GUI does), Serial's *Save* (just
  reloads serial managers via `serial::restart_all_serial`), and the
  plain *Save* on every other frame (persist only).  Unknown action
  values fall back to plain Save so a hand-crafted POST with a typo
  can't accidentally restart the server.
- **POST → 303 See Other → GET** pattern: the save handler redirects
  to `/?notice=Configuration%20saved.` so a browser reload after
  submit doesn't resubmit the form.  Client-side
  `history.replaceState` strips the `?notice=` query right after
  render so the banner appears once per save instead of persisting
  across refreshes.
- **Serial-port dropdown + refresh button** populated server-side
  from `serialport::available_ports()` (the same source the GUI
  ComboBox uses); a small ↻ button next to each port re-scans via
  `GET /serial-ports` and rewrites both selects' options in-place
  without a full page reload.  Operator's selection is preserved
  across refreshes, and a saved port that isn't currently detected
  stays visible with a `(saved)` suffix.
- **CSS Grid Server-frame layout** so the two `Port:` colons in each
  column line up across rows; per-port inputs sized to 6 chars (any
  valid TCP port fits) so the More button fits on row 1 alongside
  Telnet + Web Server.
- **JS modal popups for the More views**, plus inline confirmation
  dialogs that warn before disabling the web server or changing the
  web port — both actions break the operator's current connection.
- **Connection-breaking notice** included in the post-save banner
  when the operator's just-confirmed change will sever the browser
  session (e.g. "Web server port changed to 9090. Reconnect at the
  new port.").

#### Web auth and lockout
- **HTTP Basic Auth** gated on the same `security_enabled` flag that
  guards telnet.  Uses the project's existing length-leak-resistant
  `constant_time_eq` from `telnet.rs`.
- **Shared brute-force lockout map** with telnet and SSH.  Three
  failures across any of the three protocols trip a 5-minute IP ban
  (the same `LockoutMap` the telnet listener uses); failed web
  attempts respond with `429 Too Many Requests` + `Retry-After: 300`
  once the threshold is crossed.  The 429 fires *before* the auth
  check on every subsequent request, so a banned IP can't keep us
  busy parsing malformed POSTs either.
- **Same IP-safety allowlist as telnet**: when login is not required
  and `disable_ip_safety` is off, only private / loopback /
  link-local source IPs are accepted (and `*.*.*.1` gateway
  addresses are rejected).

#### Web defense-in-depth
- 30-second read timeout on `read_request` to stop slow-loris clients
  from parking a tokio task indefinitely.
- `MAX_INFLIGHT = 16` concurrent connections with a `Drop`-guarded
  slot release; excess connections get a `503 Service Unavailable` +
  `Retry-After: 5` rather than being parked behind the read timeout.
- 16 KB cap on request headers, 64 KB cap on POST body — bounded so
  a hostile peer can't drive the per-connection buffer to OOM.
- UTF-8 round-trip safe: `url_decode` accumulates percent-decoded
  bytes into a `Vec<u8>` then runs `from_utf8_lossy`, so values like
  `weather_zip = 日本語` survive the form → config-file → form
  cycle without corruption.

### Changed

#### Unified telnet / SSH / web credentials
- **One username / password pair** now covers the telnet menu, the
  SSH server, and the web configuration UI.  The old per-protocol
  `ssh_username` / `ssh_password` config keys are gone.  Defaults
  unchanged at `admin` / `changeme`.
- **One-time migration**: if the operator's `egateway.conf` still has
  non-default `ssh_username` / `ssh_password` values *and* the
  unified `username` / `password` are still at the factory defaults,
  the legacy SSH values are adopted into the unified pair on load
  (with a `Note: migrating legacy ssh_username=…` log line).  Once
  the next save runs, the legacy keys disappear from the written
  file.  If both pairs were already customized, the unified pair
  wins (the legacy SSH values are silently dropped).
- **GUI Security frame** collapses from two rows (separate Telnet /
  SSH credential rows) to one `Login User / Pass` row + a spacer
  that keeps the frame the same height as the adjacent Server frame.
- **Telnet Security menu** drops the `S` (Set SSH username) /
  `W` (Set SSH password) items; the remaining `U` / `P` items now
  read `Set username` / `Set password` (no more "telnet"
  qualifier).  Status shows a single `Username:` / `Password:`
  pair instead of two.
- **Help screens** under `Configuration > Security` and
  `Configuration > Server Configuration` updated: the security
  help notes "One username/password covers telnet, SSH, and the
  web UI" and the server help describes the new `W` (Toggle Web) /
  `B` (Set Web port) keys.

#### GUI Server frame
- Fixed-width listener column slots so the two `Port:` colons line
  up between rows — the same colon-alignment the web frame gets
  from CSS Grid.  The earlier hand-tuned `add_space(16.0)` left the
  colons at different X positions because "Telnet" / "SSH" and
  "Web Server" / "Kermit Server" have different intrinsic widths.
- **More button moved up to row 1** (with Telnet + Web Server),
  mirroring the web layout.

#### GUI Serial Ports frame (web-side parity adjustments)
- Web Serial frame's header now carries both ports' Enabled
  checkboxes alongside per-port titles ("Serial Port A" / "Serial
  Port B"), matching the GUI's layout exactly.  Per-port rows are
  now `Port X: [select ▼] [↻] Baud: [...] [More...]` with the More
  button kept on the same line via a no-wrap row class.

#### Logger
- Added a parallel non-draining `snapshot(max)` API alongside the
  existing `drain()`.  The GUI keeps using `drain()` for its
  per-frame console accumulator; the web `/logs` endpoint polls
  `snapshot()` so the two views don't compete for log lines.

## [0.5.5] - 2026-05-10

### Added

#### Dual serial-port support
- **Two physically independent serial ports** — `Port A` and `Port B` —
  each with its own enabled flag, mode (modem emulator or telnet-serial
  console), device path, baud, AT/S-register state, and stored
  phone-number slots. The two ports run in separate manager threads,
  persist AT&W state separately, and host independent console-bridge
  slots, so the operator can run a Hayes modem on one wire and a
  telnet-serial bridge on the other (or any other mix) without
  cross-talk.
- **A/B picker submenus** — the `Configuration > M` entry is now
  *Serial Configuration* and opens a picker listing both ports' status;
  selecting a port drops into that port's settings. The main-menu
  *Serial Gateway* (G) likewise opens an A/B picker before bridging,
  showing both ports' status (ineligible ports are dimmed) so the user
  can see *why* a port isn't available.
- **Per-port mode toggle** moved from the Configuration menu to the
  per-port settings menu (T item).  Hidden from sessions that arrived
  over a serial port itself, since flipping that port to console mode
  would tear down the caller's own connection before they could
  acknowledge.
- **GUI Serial Port frame** redesigned: header row carries both ports'
  *Enabled* checkboxes and a shared *Save* button; one row per port
  beneath with a device-path dropdown, baud field, and per-port
  *More…* button into an advanced popup (mode, framing, flow, full
  Hayes AT state). Both popups are independent so settings can be
  compared side-by-side.

### Changed

- **Config schema split** into per-port keys: every former `serial_*`
  key is now `serial_a_*` or `serial_b_*`. Legacy single-port configs
  auto-migrate into Port A on first read; the next save rewrites the
  file in dual-port form. Existing single-port deployments upgrade
  transparently with Port B disabled by default.
- **Serial Gateway main-menu visibility** — now requires at least one
  port to be in console mode (so the menu can't dead-end at an empty
  picker).
- **Dialup mapping** stays a single shared `dialup.conf` consulted by
  both ports' modems — phone-number lookups are global, not per-port.
- **Documentation refreshed** end-to-end (`README.md`,
  `usermanual.html`, `index.html`) for the dual-port architecture,
  including config-key tables, GUI screenshots/descriptions, and the
  Console Mode walkthrough.
- **`ATI0` / `ATI3` identification strings** now advertise the modem as
  Hayes-compatible, matching the behavior callers (BBS dialers, vintage
  terminal software) expect from a Hayes ID query.

### Fixed

- **PETSCII width compliance** in the new pickers and per-port menu
  titles: replaced em-dashes with ASCII hyphens and switched the
  picker layout to two lines per port (role label + device/baud) so
  worst-case lines fit the 40-col PETSCII budget.
- **Stale help text** in `console_show_help` that told users to
  "Press T at the Configuration menu" — T moved into the per-port
  settings menu.

### Security

- **AI-chat output sanitization** — replies from the Groq API are now
  normalized (`\r\n`/`\r` → `\n`) and passed through a
  `sanitize_for_terminal` filter before display, stripping ANSI escape
  sequences, control bytes, lone CRs, and telnet IAC so a prompt-injected
  reply can't smuggle terminal-control payloads through the chat surface.
- **Auth-lockout map bounded** — `record_auth_failure` now sweeps entries
  past the lockout window on every call, so a long-running public-facing
  instance can no longer accumulate one entry per distinct attacker IP
  indefinitely.

## [0.5.4] - 2026-05-06

### Added

#### Serial Console Mode
- **Telnet-serial bridge** as a second role for the serial port,
  alongside the existing Hayes AT modem emulator. Selectable via the
  new `serial_mode` config key (`modem` / `console`). The existing
  `G  Serial Gateway` main-menu item now bridges the telnet/SSH session
  straight to the wire so an operator can drive a microcontroller,
  RS-232 device, or other serial console remotely.
- **Hot mode switch** — flipping `serial_mode` (from the GUI dropdown,
  the new `T  Toggle Modem/Console mode` entry on the Configuration
  menu, or `egateway.conf` directly) reconfigures the running serial
  thread within one manager-poll interval. No restart required. The
  menu toggle is refused for callers connected over the modem itself,
  since switching to console mode would tear down their own session
  before they could acknowledge — flip the mode from a telnet, SSH, or
  system-console session instead.

### Changed

- **Configuration menu** reorganized to surface the new mode toggle and
  to relabel `M  Modem Emulator` ↔ `M  Serial Console` based on
  current `serial_mode`. The new menu walkthrough is documented in
  user-manual §5.6.
- **Documentation pass**: §3.2 of the user manual gained 22 previously
  undocumented config keys (the full `kermit_*` family,
  `ssh_gateway_auth`, `disable_ip_safety`, `allow_atdt_kermit`,
  `kermit_server_enabled` / `_port`); `index.html` grew a Kermit
  subsection in the file-transfer config tables and added cross-links
  to `kermit.html` from each protocol-prompt step; the chapter-8 intro
  now correctly describes five protocols (the old "three related
  protocols" framing predated the ZMODEM and Kermit chapters).

### Fixed

#### Console bridge hardening
- **`run_console_bridge` could wedge** indefinitely when the telnet
  peer's TCP write buffer was full: the spawned async task's
  `duplex_write.write_all().await` would park with no wake-up source,
  stranding the manager thread until process restart. Bounded with a
  200 ms timeout then `abort()`.
- **Orphaned bridge requests** on serial-mode flip: a request that
  arrived in the slot just before `SERIAL_RESTART` fired could be
  silently abandoned because `console_manager_tick` returned without
  polling the slot, leaving the requester's `rx.await` blocked forever.
  Slot is now drained with `Err("Serial mode changed")` on every exit
  path.
- **TOCTOU between request-eligibility check and slot insert**:
  `request_console_bridge` now re-checks
  `check_console_bridge_eligible` under the slot lock so an operator
  flipping `serial_mode` (or disabling serial, or clearing the port
  path) and calling `restart_serial()` in the narrow window between
  the fast-path check and the slot insert can no longer leave a
  request stuck until shutdown.
- **Unbounded `session_to_port` channel** replaced with a bounded
  `tokio::sync::mpsc::channel(64)`; a flow-controlled wire (CTS-low,
  slow peer) plus a fast typist or paste can no longer balloon
  in-memory queue depth. The async-side `.send().await` now
  backpressures `duplex_read`, which backpressures the telnet peer.
- **Slot-cleanup duplication** removed from the `Err(_)` arm of
  `rx.await`; let `ConsoleSlotGuard`'s drop own slot teardown.

#### Serial mode switch responsiveness
- **Modem online loops** (`online_mode_tcp`, `online_mode_duplex`) now
  honor `SERIAL_RESTART` on every iteration; previously a mode flip
  could lag by one block-read interval before the loop noticed.

#### Menu UX & doc-vs-code drift
- **`G  Serial Gateway`** and **`T  Toggle Modem/Console mode`** are
  now hidden from sessions that arrived over the serial port itself.
  The handler-side rejections remain as defense in depth (a serial-side
  caller can still type the letter blind), but the menu no longer
  advertises items that always error.
- **Manual cross-references** to "chapter 9.10" corrected to "9.13"
  (Console Mode lives at 9.13; 9.10 is Chained Command Lines).
- **`AT&K1`** redescribed as Auto-detect (stored, no wire effect)
  instead of "Reserved"; the parser at `src/serial.rs:1140` accepts
  `&K1` and emits `FlowSet(1)`. Missing `&K1` row added to Appendix
  B.4.
- **`AT&F`** entry now notes that it drops the active connection,
  matching the `AtResult::Reset` return.
- **Bare `kermit` alias** for `ATDT KERMIT` documented alongside the
  existing `kermit-server` / `kermit server` aliases.

## [0.5.3] - 2026-05-03

### Added

#### Kermit server expansion
- **Standalone TCP listener** for Kermit server mode on its own port
  (default `2424`, configurable via `kermit_server_port` and
  `kermit_server_enabled`). Lets a peer connect directly to a server-mode
  endpoint without going through the telnet menu — the way real
  `kermit -j host` expects to talk to a remote server.
- **`ATDT KERMIT` dial shortcut** (and aliases `ATDT kermit-server` /
  `ATDT kermit server`) drops a serial-modem caller straight into Kermit
  server mode, indistinguishable on the wire from a real `kermit -j host`
  left in `server` mode. Off by default; enabled via the new
  `allow_atdt_kermit` config flag — it bypasses the telnet menu's auth
  gate, so the toggle is gated behind a security-warning modal in both
  the GUI and the telnet menu.
- **Direct Kermit-server entry** over telnet/SSH — connecting to the
  gateway's Kermit listener drops straight into server-mode dispatch
  with no menu.
- **Additional Kermit server commands**: `remote space`,
  `remote kermit version`, plus full `remote cwd` semantics (subdir-aware
  uploads, `cdup` via bare `..`, refusal of non-existent targets), and
  `remote dir` listing fixes.
- **`AT` command chaining** in the Hayes modem emulator (e.g. `ATE0V1Q0`
  parsed as a single line).

#### Network safety toggles
- **`disable_ip_safety` config flag** — when `security_enabled` is false,
  telnet normally rejects non-private and `*.*.*.1` source IPs. This
  flag opts out of the allowlist. Toggleable from the GUI Security frame
  and the telnet Server Configuration menu, both gated behind a
  security-warning confirmation. Read per connection so changes take
  effect immediately without a restart.
- **`kermit_idle_timeout` config key** (default 300 s, `0` disables).
  Split out from `kermit_negotiation_timeout` so a long-running C-Kermit
  session that idles for hours can suppress the default disconnect.
  Surfaced in the GUI Kermit panel and the telnet Kermit settings menu.

### Changed

- **Kermit settings menu split** into Status and Settings pages,
  navigable via `M`/`V`, so each fits the 22-row × 40-col PETSCII
  budget.
- **Server Configuration menu** combined `I` and `R` into one row to
  keep the PETSCII budget at N=3 detected IPs.
- **GUI logo** swapped from the 1024×512 source (downscaled at runtime)
  to a pre-sized 366×183 asset for a 1:1 blit at standard DPI;
  eliminates the faint mauve cast on dark-blue gradients we previously
  worked around with `mipmap_mode: None`.
- **`russh` updated** 0.60.0 → 0.60.2; RustCrypto transitive deps
  realigned to the versions russh 0.60.2 tests against.
- **Private-file writes** (SSH host key, outgoing client key,
  `egateway.conf`, `dialup.conf`) now use `OpenOptions::create_new` +
  `mode(0o600)` from inception rather than create-then-chmod, closing
  the brief 0o644 window between the two calls. Per-process atomic
  counter applied uniformly so two threads can't clobber each other's
  tmp file.

### Fixed

#### Kermit vintage-receiver interop (AnzioWin canary)
- **Vintage-receiver fallback**: `kermit_send` now retries with classic
  80-byte / CHKT=1 / window=1 capabilities if the extended Send-Init
  exhausts all retries with no response. Vintage Kermits (AnzioWin,
  original CP/M Kermit, MS-DOS Kermit pre-CAPAS, embedded targets)
  always handle classic; modern peers respond on attempt 1 and pay no
  cost.
- **Send-Init ACK** is now built from the negotiated session
  intersection rather than our proposal, so quirky vintage receivers no
  longer see CAPAS bytes / extension fields they didn't propose.
- **Stale ACKs** (peer ACKing an older seq than we asked for) are now
  discarded instead of aborting the transfer. AnzioWin re-emits ACKs
  from prior packets after we've moved on.
- **YMODEM end-of-batch** handshake is now bounded to ~6 s worst case
  (3 s × 2 attempts) instead of the prior 200 s default. Fixes AnzioWin
  (and any receiver that sends post-EOT `'C'` then drops to terminal
  mode) showing the IAC-doubled `0xFF` complement byte rendered as `ÿ`
  on every retry.

#### Kermit server correctness
- **Files save inline** per S-dispatch instead of buffering until
  session end — closes the data-loss window where a peer disconnect or
  idle timeout would strand received files in memory.
- **F-packet** now refuses sender filenames that won't survive
  `validate_filename` ([A-Za-z0-9._-]) before consuming any D-packet
  body. Was silently dropping the whole upload at save time, so a
  literal-mode `put My File.txt` looked successful on the wire but
  vanished from disk.
- **`kermit_resume_partial`** now actually writes back to disk; the
  saver atomic-replaces via tmp+rename when a partial was pre-loaded.
  Previously the create-new save hit `AlreadyExists`, dropped the
  merged data, and left the partial untouched.
- **GET filename round-trip with `#` (default QCTL)**: the server's
  R-handler and `kermit_client_get` now control-quote per spec §6.4.
  Real C-Kermit's GET sender encodes via `encstr` (ckcfn2.c:2474), so a
  filename containing `#` arrived doubled — our server then looked up
  `temp##1.bin` on disk while the file actually saved as `temp#1.bin`.
- **`remote cwd <path>` (G-C)** field-decodes the argument per spec
  §6.7 (a `tochar(N)` length byte + N path bytes); short paths whose
  length byte lands on `tochar(3)='#'` are now control-quoted on the
  wire.
- **Uploads honor `remote cd`**: telnet save callback joins
  `target_dir/<subdir>/<filename>` instead of dropping the per-session
  subdir on the floor.
- **`remote cd ..` (cdup)** is now special-cased — pops one component
  from the per-session subdir, no-op at root, never escapes the
  sandbox. Other `..` forms (`foo/..`, `../etc`) still hit
  `is_safe_relative_subdir` and refuse.
- **`remote cd <typo>`** is now refused with E-packet
  "Directory not found" instead of being silently ACKed and dropping
  subsequent uploads into a phantom path.
- **Idle-timeout disconnect** now ends the telnet session cleanly.
  Pre-fix the gateway sent an "idle timeout" E-packet then returned to
  the file-transfer menu with the TCP socket still open; the next
  `remote ...` from C-Kermit landed on a non-protocol menu and surfaced
  as "too many retries" in the peer's UI. Server now flushes the writer
  after the E-packet, returns `io::ErrorKind::TimedOut`, and the menu
  handler ends the session.

#### Stability
- **GUI Ctrl-C hang when window is minimized**: signal-watcher now
  sends `ViewportCommand::Close` directly instead of relying on
  `request_repaint()` — some WMs throttled repaint delivery for
  minimized windows so `update()` never ran. Plus
  `runtime.shutdown_timeout(2 s)` after `block_on` returns as a
  defensive cap on tokio runtime drop.
- **Connection-rejection greetings** (max sessions, insecure-IP policy)
  now actually reach the client. Replaced non-blocking `try_write` with
  a bounded `write_all` + `flush` + `shutdown` capped at 2 seconds,
  spawned as an independent task so the accept loop doesn't serialize
  at ~0.5 conn/sec under flood.
- **Telnet `session_count`** uses `fetch_add → check → fetch_sub`
  instead of `load → fetch_add`, mirroring the SSH pattern; closes the
  cap-bust TOCTOU.

#### XMODEM / YMODEM / ZMODEM polish
- **YMODEM block-0 CRC error** now NAK-and-retries within negotiation
  instead of falling out and NAK-looping the retransmit as a
  block-number mismatch.
- **YMODEM empty-file** goes straight to EOT instead of emitting a
  SUB-padded data block.
- **XMODEM/YMODEM duplicate-block detection** now ACKs both expected-1
  AND expected-2 per Forsberg's "any already-seen block" recommendation.
- **XMODEM first-block mode auto-detect**: a trailer-format mismatch on
  the very first block falls back to the alternate mode (CRC↔checksum)
  and locks the session. Closes the negotiation timing race against
  vintage Christensen 1977 / CP/M MODEM7 / C64 BBS senders that ignore
  `'C'` until NAK'd, AND the modern slow-startup race where the
  receiver flips to checksum mid-flight against a CRC-capable sender.
- **ZMODEM inter-file header CRC mismatches** now ZNAK-and-retry
  (bounded by `max_retries`) instead of silently truncating the rest of
  a long batch on a single bit-flip.
- **ZMODEM phase-1 negotiation** no longer counts stale ZRQINIT /
  unexpected frames against the retry budget — chatty receivers were
  burning retries on bytes that proved the link was alive.
- **ZMODEM `0x98`** added to the ZDLE escape table (8-bit dual of
  ZDLE/0x18 per Forsberg §10 Table 4).
- **ZMODEM ZSINIT TESCCTL/TESC8** parsing per Forsberg §11.3; receiver
  now ACKs ZSINIT instead of silently ignoring the flag.

#### Web browser
- **HTTPS→HTTP downgrade** is now signalled to the user with a
  `[!] HTTPS failed — fetched over plain HTTP` banner instead of being
  silent. Both `fetch_and_render` and the form-submit POST path were
  transparently retrying over plain HTTP on TLS error.
- **Gopher selector** filters CR/LF/NUL on user-supplied selectors to
  prevent protocol-line injection in search queries (TAB preserved as
  the legitimate item-type-7 separator).

### Tests

- **997 lib + 1 binary e2e tests** pass, 0 failed; clippy clean on
  Linux + `x86_64-pc-windows-gnu`.

## [0.5.2] - 2026-04-29

### Fixed

#### ZMODEM autostart actually works
- The menu-input state machine detected the `** ZDLE [ABC]` prefix and
  called `handle_zmodem_autostart`, which previously sent the spec'd
  abort sequence and printed "ZMODEM is not yet supported" — even
  though `zmodem.rs` has shipped full ZMODEM support. The handler now
  drains the residual ZRQINIT bytes, validates the transfer dir, and
  calls `zmodem_receive`, with a save flow + summary screen matching
  the menu-initiated upload path.

#### ZMODEM receive metadata
- `parse_zfile_info` now returns a `ZfileInfo` struct (Forsberg §11 —
  length is decimal, mtime + mode are octal). `ZmodemReceive` carries
  the matching `modtime` + `mode` fields so the saved file gets the
  correct mtime / permissions instead of the prior `None` / default.
- `modtime=0` / `mode=0` are filtered to `None` in the parser. Our own
  `zmodem_send` and most other senders (including `lrzsz`) write
  `"<len> 0 0 0 0 <len>"` when they don't have those values;
  propagating `Some(0)` would have driven `apply_ymodem_meta` to set
  the saved file's mtime to epoch and mode to 0 (no permissions for
  anyone) — worse than ignoring the field altogether.

#### Atomic batch-receive saves
- The ZMODEM-autostart, ZMODEM/Kermit-batch-upload, and Kermit-server
  save loops all used a non-atomic `exists()` + `std::fs::write`
  pattern with a TOCTOU window. New async `save_received_file` helper
  opens with `create_new(true)` for atomic create-only semantics and
  uses `tokio::fs` for non-blocking I/O. Returns
  `SaveError::AlreadyExists` / `SaveError::WriteFailed` so each caller
  maps to its own per-file skip wording. All four batch-receive save
  sites now share one code path.
- Sync `std::fs::write` of up to 8 MB was blocking the tokio executor
  for tens of milliseconds on long telnet sessions — replaced with the
  async helper above.

#### Cross-platform CI
- **Windows `compute_resume_offset` tests**: `set_modified` on Windows
  requires the file handle to have write permission
  (`FILE_WRITE_ATTRIBUTES`); `File::open` opens read-only so the call
  was failing with permission denied. Replaced the three affected
  mtime-mutation helpers with `OpenOptions::new().write(true).open(...)`.
- **Windows symlink-resume test** unused-variable lint — moved
  `link_path` declaration inside the `#[cfg(unix)]` block alongside the
  symlink call.
- **Rust 1.95 clippy `collapsible_match`** on the seven A-packet
  single-byte sub-attribute arms in `parse_attributes` — converted to
  match guards. Behavior unchanged.

### Changed

- **`MAX_FILE_SIZE` consolidated** to `crate::tnio::MAX_FILE_SIZE`
  (single `u64` constant); xmodem / zmodem / kermit / telnet now
  import it.
- **IAC-escape control surface unified**: removed the vestigial
  `kermit_iac_escape` config field everywhere (struct, parser, writer,
  default, GUI checkbox, telnet menu toggle, settings screen,
  `egateway.conf` docstring). The three Kermit call sites now read
  `self.xmodem_iac` like XMODEM and ZMODEM already do — the menu
  toggle is the single operator-visible source of truth.
- **Kermit error strings** normalized from `"Kermit recv: ..."` to
  `"Kermit: ..."` at six sites.
- **Module docstrings** rewritten for the Ethernet Gateway scope;
  stale "no batch mode" / "full server-mode is not implemented"
  comments and self-referential commit/Gap markers cleaned out.

### Tests

- **935 lib + 1 binary e2e tests** pass; clippy clean on Linux +
  `x86_64-pc-windows-gnu`.

## [0.5.1] - 2026-04-28

### Added

#### Kermit protocol support
- **Full Kermit send and receive** implemented in `src/kermit.rs` per
  Frank da Cruz, "Kermit Protocol Manual" (1987) + C-Kermit extensions.
  S/F/A/D/Z/B/E/C packet dispatch, CHKT 1/2/3 (single-byte / two-byte /
  three-byte CRC), Send-Init capabilities negotiation, long packets,
  eighth-bit prefix, repeat-count compression, and locking-shifts.
- **Sliding window** (selective-repeat ARQ): D-packets ride a windowed
  sender with per-seq retransmit timer and selective NAK retransmit;
  receiver buffers out-of-order packets and NAKs the missing seq.
  Window size 1–31 (spec max 31 < 32 = half of mod-64 seq space, so
  forward/back disambiguation is unambiguous). Control packets
  (S/F/A/Z/B) stay stop-and-wait.
- **Streaming Kermit** (CAPAS byte 3 bit 2): D-packets pushed
  back-to-back with no per-D ACK; receiver suppresses D-ACKs. Z-ACK
  confirms the whole stream. Mid-stream NAKs trigger selective
  retransmit, then resume.
- **Peer TIME field** honored as our retransmit timeout (spec §3.2).
  `TIME=0` falls back to `kermit_packet_timeout` config, floored at
  1 second.
- **Server mode** (S/R/G/I/B/E/C dispatch) — `remote dir`,
  `remote cwd`, `remote help`, `get`, `send`, `bye`, `finish`.
- **Five extended A-packet sub-attributes** per spec §5.1: `&`
  long-form file length (decimal u64), `1` character set, `*` encoding,
  `,` record format, `-` record length. Parsed and surfaced in verbose
  logs; receiver uses `length.or(long_length)` for `declared_size`.
  Encoder emits the existing six tags (`!` length, `#` date, `+` mode,
  `.` system_id, `"` file_type, `@` disposition) plus the new four.
- **Detected Kermit flavor** (C-Kermit, G-Kermit, Kermit-95, …)
  surfaced in the upload-complete summary line.
- **Telnet File Transfer menu** entry for Kermit alongside XMODEM /
  XMODEM-1K / YMODEM / ZMODEM. The first-line hint is now generic
  "(More for others)" since the popup covers every protocol.

### Changed

- **Shared raw I/O extracted** to `src/tnio.rs`: `ReadState`,
  `is_can_abort`, `raw_read_byte`, `nvt_read_byte`,
  `consume_telnet_command`, `raw_write_bytes` plus IAC/SB/SE/WILL/WONT/
  DO/DONT/CAN constants. The byte-stream layer that handles telnet IAC
  unescaping, NVT CR-NUL stripping, Forsberg's CAN×2 abort rule, and
  the matching write-side escaping was duplicated near-verbatim across
  `xmodem.rs` / `zmodem.rs` / `kermit.rs` (~140 lines per module). Net
  delta: 583 lines removed, 289 added.

### Fixed

- **Send-Init `WINDO`/`MAXLX` fields** are now conditional per spec
  §4.4: `WINDO` emitted iff `window > 1`, `MAXLX1`/`MAXLX2` emitted iff
  `long_packets`. Parser reads `WINDO` iff the sliding bit is set in
  CAPAS byte 1, reads `MAXLX` iff the long bit is set. Self-tests
  passed because both sides used the same buggy layout, but a session
  with `long_packets=true, sliding=false` would have advertised an
  extra `WINDO=1` byte that a strict-spec G-Kermit / E-Kermit peer
  would have misread as `MAXLX1=1`, collapsing our advertised MAXL
  from ~4096 to ~138.
- **C0/C1/DEL control range** in `is_kermit_control` was missing
  `0x80..=0x9F` and `0xFF`. Per spec §6.4, these high-bit equivalents
  must also be QCTL-prefixed. The encoder was emitting them raw; the
  decoder now also unctls bodies in the high-bit ctl range when no
  QBIN is active.
- **Long-packet `extended_len`** was being encoded as
  "5 + DATA + CHECK" (including the 5 header bytes after LEN); per
  spec it's "the length of everything in the packet that follows the
  HCHECK" — i.e., DATA + CHECK only. This is what real C-Kermit
  emits, and the mismatch caused every long-packet CRC verification
  to fail in interop.
- **`peer_id` parser**: real C-Kermit's Send-Init buries vendor-specific
  CAPAS extension bytes (CHECKPOINT, WHATAMI, …) in the trailing slot;
  our parser accepted the binary bytes as a string and produced
  garbage like `0___^"U1A`, defeating downstream flavor detection.
  Tightened the heuristic to require a 4-character ASCII letter run
  before treating the trailing bytes as an identifier; otherwise leave
  `peer_id` as `None` and let `detect_flavor` classify by capability
  bits.
- **`record_lrzsz_fixtures`** is now gated behind
  `ZMODEM_RECORD_FIXTURES=1`. The fixture-recorder was `#[ignore]`d
  but `cargo test -- --ignored` was inadvertently running it and
  silently rewriting the committed binary fixtures with timestamp-
  bearing equivalents.

### Documentation

- README and user manual extended with Kermit coverage alongside the
  existing XMODEM / YMODEM / ZMODEM sections.

### Tests

- **+218 tests** for Kermit (CRC + checksum vectors, packet
  round-trips, Send-Init negotiation, sliding-window happy path +
  lossy NAK recovery, streaming round-trips including 64 KB /
  all-bytes / multi-file / lossy, A-packet sub-attribute round-trips,
  server-mode dispatch). Three `#[ignore]` C-Kermit subprocess
  interop tests (stop-and-wait, sliding-window, streaming) drive the
  real `kermit` binary over TCP. Total: **930** unit + proptest
  tests, all green.

## [0.4.0] - 2026-04-25

### Changed

#### Project rename: XMODEM Gateway → Ethernet Gateway
- The product is now **Ethernet Gateway**. The original name no longer
  reflected the scope (SSH, web browser, AI chat, weather, modem
  emulator, gateway proxies — only one of which is XMODEM).
  Functionality is unchanged; this is purely a naming refresh.
- Cargo package renamed `xmodem-gateway` → `ethernet-gateway`.
- GitHub repository moved to
  [`rickybryce/ethernet-gateway`](https://github.com/rickybryce/ethernet-gateway).
- Configuration file renamed `xmodem.conf` → `egateway.conf`.
- SSH host key file renamed `xmodem_ssh_host_key` → `ethernet_ssh_host_key`.
- Outbound SSH gateway client key renamed `xmodem_gateway_ssh_key` →
  `ethernet_gateway_ssh_key`.
- AppImage renamed `XMODEM_Gateway-x86_64.AppImage` →
  `Ethernet_Gateway-x86_64.AppImage`.
- systemd unit renamed `xmodem-gateway.service` → `ethernet-gateway.service`.
- Telnet menu prompt path renamed `xmodem> ` → `ethernet> ` (and all
  sub-paths: `ethernet/xfer`, `ethernet/web`, `ethernet/config/...`).
- Hayes dial shortcut: `ATDT xmodem-gateway` → `ATDT ethernet-gateway`
  (the `1001000` shortcut number is unchanged).
- HTTP browser User-Agent: `XmodemGateway/1.0` → `EthernetGateway/1.0`.

**Migration**: existing deployments that want to preserve identity should
rename `xmodem.conf` → `egateway.conf` and `xmodem_ssh_host_key` →
`ethernet_ssh_host_key` (and the gateway client key) before first start.
Otherwise the gateway will create fresh files and SSH clients will see a
"host key changed" warning.

#### GUI refresh
- New logo (`ethernetgatewaylogo.png`, 1774×887, 2:1 aspect ratio) displayed at
  366×183 with trilinear (mipmap) texture filtering for clean
  downscaling.
- Window/panel background darkened from `#050E1A` to `#000510` to match
  the new logo's deep-navy backdrop.

### Added

#### ZMODEM polish (continuation of 0.3.5 work)
- **`ZRINIT` drain**: receiver now consumes the trailing ZRINIT/handshake
  bytes some senders (notably lrzsz `sz`) emit before they go quiet.
  Eliminates a 5-second stall at the end of a successful ZMODEM receive.
- **`ZSINIT` handler** on receive — sender-supplied attention/escape
  configuration is parsed and ack'd per Forsberg §11.5, so senders that
  block waiting for the ACK now proceed.
- **lrzsz interop suite**: 13 captured-wire replay fixtures (tiny / exact-
  1 KB / all-bytes / 2-file batch / ZSKIP / aborted-mid-batch) plus two
  `#[ignore]` subprocess tests that drive real `sz` / `rz` end-to-end.

#### XMODEM/YMODEM/ZMODEM compliance pass
- **CAN×2 abort handling** per Forsberg's recommendation: a single CAN is
  no longer treated as an abort; two consecutive CANs (with no
  intervening data) are required. Routes through a shared
  `is_can_abort` helper so all three protocols agree.
- **Spec-citation tests** (63 new tests across the four files) that
  reference exact section numbers in the Forsberg specs and validate
  edge-case behavior (block-zero NAK retry, zero-length payloads,
  trailing-`SUB` preservation, etc.).
- **YMODEM maximal compliance** — full Forsberg §6.1 block-0 metadata
  (filename, size, mtime, mode, serial number) is parsed on receive and
  applied (mtime + mode) on save. Send path emits the same set.

#### End-to-end test infrastructure
- **Binary-level e2e test** (`tests/binary_e2e.rs`): launches the actual
  release binary as a subprocess, drives the telnet UI through the web
  browser flow against a hermetic localhost HTTP server, and asserts on
  the rendered output. Catches integration regressions that unit tests
  alone miss.
- **Hermetic e2e tests** for the HTTP and Gopher browsers: spin up
  loopback servers, run the parser/renderer end-to-end, assert on
  PETSCII/ANSI/ASCII rendering invariants.

### Fixed

- Logo rendering aspect ratio is now correct after the asset swap.
  Previously the new 2:1 logo was being squashed into a 1.6:1 box.

### Tests

- Total: **719** unit + proptest tests (718 lib + 1 binary e2e), 0
  failed, 15 ignored. All green on Linux / macOS / Windows.

## [0.3.5] - 2026-04-23

### Added

#### ZMODEM protocol support
- **Full ZMODEM send and receive** implemented per the Forsberg 1988
  specification in `src/zmodem.rs` — ZDLE escape layer, hex / binary16 /
  binary32 headers, CRC-16 and CRC-32, batch transfer per §4, receiver
  `ZSKIP` to decline individual files per §7, and `rz\r` auto-start
  trigger so Qodem, ZOC, and other auto-detecting terminals begin the
  transfer without a separate `rz` command.
- **File transfer menu entry** for ZMODEM alongside XMODEM / XMODEM-1K /
  YMODEM. Stop-and-wait flow control (ZCRCQ mid-frame + ZCRCE
  end-of-frame); our `ZRINIT` advertises `CANFDX|CANOVIO|CANFC32` without
  requiring streaming.
- **Additional file-transfer configuration options** surfaced in the
  Gateway Configuration menu.

### Fixed

- **Windows CI**: ZMODEM fixture binaries are now marked as binary in
  `.gitattributes` so the CRLF auto-conversion on Windows runners does
  not corrupt them. Fixes the sporadic Windows CI failure on
  `test_lrzsz_rz_zskip_interop` and the captured-wire replay tests.
- **CI runner configuration**: resolved transient runner errors that
  were preventing reliable green builds.
- **GUI**: copy/paste now works as expected in the configuration editor
  text fields.

### Documentation

- README updated with NULL-modem adapter guidance and a clarified telnet
  command example.
- User manual extended with ZMODEM coverage alongside the existing
  XMODEM / YMODEM sections.

### Tests

- **+46 tests** added for the ZMODEM implementation (CRC vectors, ZDLE
  round-trips, header round-trips, subpacket round-trips, ZFILE parser,
  full send↔receive round-trips, batch / skip handling, ZABORT, non-zero
  `ZRPOS` resume, proptest fuzzers on adversarial bytes) plus two
  `#[ignore]` lrzsz subprocess interop tests. Total: **617** unit +
  proptest tests, all green.

## [0.3.4] - 2026-04-18

### Fixed

#### XMODEM / YMODEM over telnet — full RFC 854 NVT compliance
- **CR-NUL stuffing on both send and receive.** Bare `0x0D` (CR) in file data
  is now emitted on the wire as `CR NUL` per RFC 854 §2.2, and the receive
  path strips trailing `NUL` after `CR`. Without this, any block containing
  a `0x0D` data byte (common in binary files — EXE, PDF, compressed
  archives) desynced the stream by one byte per CR. Visible symptom was
  "Transfer stalls at 3–4 blocks, client repeatedly sends `'C'`".
- **IAC escape/unescape on both directions** matches the existing telnet
  NVT rule already applied to `IAC` itself; the two transforms are now
  always active together when `xmodem_iac` is on.
- **YMODEM end-of-batch handshake on receive.** After ACKing the final
  `EOT`, the server now sends `'C'` and consumes the "null block 0"
  (filename starts with `NUL`) that strict senders emit per Forsberg §7.4.
  Fixes "YMODEM upload completes all data but client hangs" on ExtraPuTTY,
  Tera Term, and lrzsz's `sb`.
- **YMODEM size-based truncation.** After a YMODEM transfer the receiver
  now truncates to the exact `size` field from block 0 instead of stripping
  trailing `SUB` (0x1A) padding. Fixes files that legitimately end in
  `0x1A` bytes (EXEs, some archives) being silently truncated.

### Added

#### Session-side configuration
- **Gateway Configuration menu** at `Configuration → G` in the telnet
  session: toggles the outbound Telnet mode (Telnet / Raw TCP) and the
  outbound SSH auth mode (Key / Password) at runtime, persists to
  `egateway.conf`, and takes effect on the next gateway connection with no
  server restart. Replaces the per-connection interactive prompts that
  used to live inside the Telnet Gateway and SSH Gateway flows.
- **Config key `ssh_gateway_auth`** (`"key"` or `"password"`, default
  `"password"`) drives the SSH Gateway auth choice. No silent fallback —
  failures now clearly point the user at Server → More or Config → G.
- **Pre-transfer overwrite prompt.** On upload, if the target filename is
  already present the server asks `Overwrite? (Y/N)` *before* the transfer
  starts. Avoids running a multi-MB transfer only to fail at the final
  write step.

#### GUI console
- **"More..." popups** on the Server and Serial Modem frames expose the
  full set of persistent settings that didn't fit on the main panel —
  telnet gateway mode + negotiation, SSH gateway auth (with the gateway's
  public key shown read-only when Key mode is selected), the extended
  Hayes AT profile (E/V/Q, X-level, &C/&D/&K), all 27 S-registers, and
  the four stored phone-number slots. Each popup has its own **Save**
  button that persists without restarting the server.
- **Popup styling** distinct from main panels — deep forest-green panel
  background, brighter-green text-entry fields — so the user immediately
  sees which surface they're editing.

### Changed

#### XMODEM transforms auto-default
- **Default now picked from detected terminal type.** After terminal
  detection, `xmodem_iac` is auto-set to **on** for ANSI sessions
  (PuTTY / ExtraPuTTY, Tera Term, C-Kermit, SecureCRT — all escape per
  RFC) and **off** for PETSCII and ASCII sessions (retro clients like
  IMP8, CCGMS, StrikeTerm, AltairDuino firmware that speak raw bytes
  despite the port-23 connection). User can still flip per-session with
  the `I` key in the File Transfer menu.

#### UX polish
- **Post-transfer settle window.** Error messages after a failed upload
  (transfer failure, save I/O error, duplicate filename) now honour the
  same 1-second pause the success path already used, so ExtraPuTTY's
  transfer dialog has time to close before our message prints. Also
  drains stray bytes from the client's post-transfer chatter so
  `wait_for_key` actually waits for a human keypress.
- **Select Protocol menu** on download now clears the screen instead of
  appending after the download list.
- **Default `ssh_gateway_auth` flipped from `key` to `password`** — works
  out of the box with any SSH server that allows password auth; Key mode
  requires a one-time `authorized_keys` setup.

### Removed

- The interactive `T`-toggle prompt inside the Telnet Gateway flow and
  the `K`-show-pubkey prompt inside the SSH Gateway flow. Both options
  now live in config (editable via GUI Server → More or Config → G).

### Documentation

- User manual §8.3, §8.6 rewritten to reflect NVT symmetry, the auto-IAC
  default, and the overwrite prompt. `index.html` brought in line.
- Modem Emulator help in-session now lists `AT&Zn=s` / `ATDSn` /
  `ATIn` / `ATXn` / `AT&C/&D/&K` / `A/` alongside the pre-existing
  quick reference.

### Tests

- +1 regression test: `test_ymodem_round_trip_preserves_trailing_sub_bytes`
  verifies YMODEM size-truncation preserves a payload that legitimately
  ends in `0x1A` bytes. Total: **571** unit + proptest tests, all green.

## [0.3.3] - 2026-04-18

### Added

#### Telnet server — additional RFC compliance
- **RFC 854 EC / EL**: `IAC EC` now surfaces to line-editors as `DEL` (0x7F)
  and `IAC EL` as `NAK` (0x15), with the `read_input_loop` handling NAK as
  "erase the current line."
- **RFC 859 STATUS** (option 5): `DO STATUS` is answered with `WILL STATUS`;
  `SB STATUS SEND` returns an `SB STATUS IS <state>` dump of every option
  advertised and not yet denied. Works with the Unix `telnet` client's
  `status` / `send status` subcommands.
- **RFC 860 TIMING-MARK** (option 6): `DO TIMING-MARK` is answered with
  `WILL TIMING-MARK` after flushing pending output, providing clients a
  processing-synchronization point.

#### Outgoing Telnet Gateway
- **IAC escape/unescape** in both directions; literal 0xFF data bytes now
  survive the wire without being mistaken for IAC.
- **Full RFC 1143 six-state Q-method** (`No`, `Yes`, `WantYes`,
  `WantYesOpposite`, `WantNo`, `WantNoOpposite`) for option negotiation.
- **Cooperative mode** (`telnet_gateway_negotiate = true`): proactively
  offers `WILL TTYPE`, `WILL NAWS`, and `DO ECHO` at connect; responds to
  `SB TTYPE SEND` with the local user's terminal type; responds to
  `DO NAWS` with the local user's current window size; forwards NAWS
  updates mid-session when the local user resizes.
- **Raw-TCP escape hatch** (`telnet_gateway_raw = true`): bypasses the
  telnet IAC layer entirely for destinations that aren't really telnet.
  Toggleable live from the Telnet Gateway menu with the **T** key; choice
  persists to `egateway.conf`.
- **8 KiB subnegotiation body cap**: malicious remotes cannot exhaust
  memory by sending huge `SB` bodies without a terminating `IAC SE`.
- **Property-based fuzz test** (`qmethod_proptest`) covers the full Q-method
  state machine with randomized sequences. Regression corpus checked into
  `proptest-regressions/telnet.txt`.

#### Outgoing SSH Gateway
- **Public-key authentication** with auto-generated Ed25519 client keypair
  (`ethernet_gateway_ssh_key`, 0o600 on Unix). Tried before password; on
  acceptance, the password prompt is skipped entirely.
- **"Show gateway public key" menu**: press **K** at the SSH Gateway
  menu to display the one-line OpenSSH-format public key for pasting
  into a remote's `~/.ssh/authorized_keys`.
- **Audit log for host-key trust decisions**: TOFU-accept, key-update,
  and key-reject events are written to `glog!` with host, port,
  algorithm, and SHA-256 fingerprint.

#### Hayes modem emulator
- **`A/` repeat-last-command** (no `AT` prefix, no CR required).
- **`ATI0`–`ATI7`** identification variants (product code, ROM checksum,
  ROM test, firmware, OEM, country, diagnostics, product info).
- **Stored phone-number slots**: `AT&Zn=s` stores a number in slot
  `n ∈ {0,1,2,3}`; `ATDS` / `ATDS<n>` dials it. Persisted by `AT&W`,
  restored by `ATZ`. Preserves hostname case so `AT&Z1=Pine.Example.com`
  works.
- **S-registers expanded to S0–S26**: S13–S24 are reserved-zero
  placeholders for legacy init strings; S25 (DTR detect time) and
  S26 (RTS/CTS delay) match Hayes defaults.
- **Dial-string modifiers**: `,` (pause by S8), `W` (wait-for-dialtone by
  S6), `;` (stay in command mode), `*`/`#` (preserved DTMF digits),
  `P`/`T`/`@`/`!` (accepted, ignored). Hostname heuristic prevents
  stripping `P`/`T`/`W` from names like `pine.example.com`.
- **ATX0–ATX4** result-code verbosity per RFC.
- **`AT&C` / `AT&D` / `AT&K`**: parsed, stored, persisted, displayed in
  `AT&V`. Actual hardware pins are not driven; see README limitations.
- **Silent-OK fallback** for unknown commands (`ATB`, `ATC`, `ATL`,
  `ATM`, `AT&B`, `AT&G`, `AT&J`, `AT&S`, `AT&T`, `AT&Y`, …) so legacy
  init strings don't halt mid-setup.

### Security

- **Shared per-IP brute-force lockout** across telnet and SSH servers.
  After 3 failed authentication attempts in 5 minutes, the source IP is
  blocked for 5 minutes across both protocols — an attacker can't bounce
  between them to reset the counter.
- **0o600 file permissions on Unix** for all sensitive files:
  `egateway.conf`, `dialup.conf`, `gateway_hosts`, `ethernet_ssh_host_key`,
  `ethernet_gateway_ssh_key`.
- **Per-PID temporary filenames** for atomic config writes; closes a
  TOCTOU window on shared working directories.
- **`save_config` now acquires the `CONFIG` mutex before disk write**,
  so a concurrent session-side `update_config_values` can't clobber the
  GUI-initiated write.
- **SSH Gateway** now calls `session.disconnect` on every early-return
  path after authentication, preventing orphaned authenticated sessions
  on the remote.

### Fixed

- Q-method refusal flags (`sent_dont` / `sent_wont`) are now cleared on
  every contradicting-verb emission and set on every refusal emission
  (including the `WantYesOpposite → WantNo` transitions). Prevents
  duplicate refusal replies to a misbehaving peer. Caught by the
  proptest fuzzer.
- `gateway_telnet` local → remote direction now IAC-escapes outbound 0xFF
  data bytes correctly.
- `gateway_telnet` remote → local direction now parses inbound IAC rather
  than leaking protocol bytes to the user's terminal.

### Changed

- `gateway_ssh` prompt order: host/port/username first, then try pubkey
  auth, prompt for password only if pubkey is rejected. Matches how
  OpenSSH from the command line behaves.
- Hayes S7 default is now `15` seconds (capped internally at 60); the
  Hayes `50` second default was too slow for gateway users.

## [0.3.2] - earlier

- RFC compliance features for Telnet (RFC 854 / 855 / 857 / 858 /
  1073 / 1091 / 1143).
- Drain before "Press any key" to avoid CRLF stickiness.
- Security fixes and minor bug fixes.

## [0.3.1] - earlier

- Added web browser for user manual.
- Minor UI polish.

## [0.3.0] - earlier

- Added configuration options for telnet/SSH/serial servers.
- GUI for configuration editing (eframe/egui).
- Ring emulator and dialup directory.
- Windows build fix for `GetDiskFreeSpaceExW`.
- S-register persistence via `AT&W`.

[0.6.4]: https://github.com/rickybryce/ethernet-gateway/compare/v0.6.3...HEAD
[0.6.3]: https://github.com/rickybryce/ethernet-gateway/releases/tag/v0.6.3
[0.6.2]: https://github.com/rickybryce/ethernet-gateway/releases/tag/v0.6.2
[0.6.1]: https://github.com/rickybryce/ethernet-gateway/releases/tag/v0.6.1
[0.5.4]: https://github.com/rickybryce/ethernet-gateway/releases/tag/v0.5.4
[0.5.3]: https://github.com/rickybryce/ethernet-gateway/releases/tag/v0.5.3
[0.5.2]: https://github.com/rickybryce/ethernet-gateway/releases/tag/v0.5.2
[0.5.1]: https://github.com/rickybryce/ethernet-gateway/releases/tag/v0.5.1
[0.4.0]: https://github.com/rickybryce/ethernet-gateway/releases/tag/v0.4.0
[0.3.5]: https://github.com/rickybryce/ethernet-gateway/releases/tag/v0.3.5
[0.3.4]: https://github.com/rickybryce/ethernet-gateway/releases/tag/v0.3.4
[0.3.3]: https://github.com/rickybryce/ethernet-gateway/releases/tag/v0.3.3
[0.3.2]: https://github.com/rickybryce/ethernet-gateway/releases/tag/v0.3.2
[0.3.1]: https://github.com/rickybryce/ethernet-gateway/releases/tag/v0.3.1
[0.3.0]: https://github.com/rickybryce/ethernet-gateway/releases/tag/v0.3.0
