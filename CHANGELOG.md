# Changelog

All notable changes to **ethernet-gateway** are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

_No unreleased changes._

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

[Unreleased]: https://github.com/rickybryce/ethernet-gateway/compare/v0.5.4...HEAD
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
