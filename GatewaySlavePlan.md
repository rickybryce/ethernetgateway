# Gateway Master/Slave (Remote Serial Extender) — Design Note

**Status:** Feasibility + design sketch only. NOT implemented. Written 2026-06-28.
**Author context:** Ethernet Gateway (`/home/ricky/xmodem`, cargo pkg `ethernet-gateway`),
on `dev`/`master` at `f39d178`, version 0.6.3 (unreleased).

---

## 1. The idea

Keep all current functionality, add one new capability: a **master/slave** topology.

- **Master gateway** runs everything as today (menus, file transfer, SSH proxy, AI chat,
  weather, modem emulator, etc.).
- **Slave gateway** has physical serial ports with retro hardware attached. It runs the local
  **modem-command dialog** itself (see §3), but once a device **connects** it **bridges the session
  over IP to the master** — so the master provides the menu, file transfer, and dial-out, as if the
  device were attached to the master.
- Operation: put a gateway into **slave mode**, give it the **master's IP** + credentials (and
  per-port mapping). Done.

This is, in networking terms, a **serial-over-IP / remote serial extender** (same family as
RFC 2217, ser2net, a network serial concentrator) — with the refinement (decided 2026-06-28) that
the **AT/modem layer runs on the slave** and only the *connected session* is relayed to the master.

**Verdict: feasible, strong architectural fit, additive — existing *functional* behavior is
unchanged for standalone/master gateways; the one visible footprint is a `Master/Slave` nav item in
the telnet config menus (§4.7 / §9 #11).**

---

## 2. Why the codebase is well-positioned

The gateway already bridges serial↔TCP in every direction this feature needs — we'd be
generalizing an existing pattern across the network, not inventing one:

| Existing primitive | Where | Relevance |
|---|---|---|
| Serial → **in-process** gateway session | `ATDT ethernet-gateway` duplex bridge (serial.rs, `Handle`/duplex path) | The slave/master feature is *this exact bridge*, but the session lives on a **remote** master instead of in-process. |
| Serial → **outbound TCP** | `dial_tcp` (serial.rs:2396) | Proves the serial-thread can pump a UART out to a TCP socket. |
| **Inbound TCP** → serial / session | serial-console mode + telnet session | The master-side intake is a variant of this. |
| Generic-stream protocols | xmodem/ymodem/zmodem/kermit/punter take `AsyncRead`/`AsyncWrite` + `is_tcp` | Transfers already run over an abstract stream. |
| **8-bit transparent transport** | tnio (CR-NUL stuffing removed 2026-06-28, commit `92ea6e8`) | Exactly what a raw serial relay needs — no NVT mangling. |
| **Auto-reconnect** | serial reconnect (commit `4cfad87`) + dial-all-addresses (`5b0611e`) | Directly reusable for the slave's persistent retry-to-master. |
| Auth + per-IP lockout (shared telnet/SSH/web) | telnet.rs / webserver.rs | The SSH transport (§4.3) authenticates relays via the existing `auth_password` + lockout — no separate gate to build. |

---

## 3. Roles / who runs what  *(revised 2026-06-28: AT layer on the slave)*

The model evolved from a "dumb byte relay" (master runs the modem emulator) to **the modem/command
layer running on the slave**, because the config-heavy AT layer naturally belongs where the device's
config lives — this removes the need to ship config to the master (old #1) and the `AT&W`
persistence problem (old #17). See §9 for the superseded items.

- **Slave = local modem + relay-on-connect.** For each **modem-mode** port the slave runs its **own
  modem emulator** locally: AT commands, S-registers, `+++`/`ATH`, `AT&W` (persisted to the slave's
  own `egateway.conf` exactly as today). It handles the entire **command-mode dialog** with the
  attached device with no master involvement. When the device **connects** (the emulator reaches
  CONNECT — e.g. `ATDT`), the slave switches to **data mode and bridges the byte stream to the
  master**, conveying at connect only the dynamic **call target** (the master's own services, or a
  `host:port` it resolved from its *local* dial-mappings). A **console-mode** port has no AT layer —
  it's a transparent relay from the moment the master bridges to it (#12).
- **Master = the far end of every connected call + all services.** The master serves the menu, file
  transfer, AI chat, weather, SSH proxy, and performs **dial-out on its own network**. It is on the
  other end of *every* connected relay — whether the device asked for the master's menu or asked to
  dial onward to an external BBS (the master dials it).
- **Invariant — files land on the master.** Because the master is the far end of every connected
  call and runs the services, the gateway's File Transfer reads/writes the **master's**
  `transfer_dir`. (A transfer with an *external* BBS is end-to-end between the device and that BBS,
  relayed transparently through the master — neither gateway stores it, which is normal.)

Net split: the **static, config-heavy AT/modem layer stays entirely on the slave** (nothing to ship;
`AT&W` is a local write); the **master owns the dynamic session** (services, dial-out, file
storage). The earlier "modem emulator runs on the master" framing is superseded.

### Dialing — **Model B (chosen 2026-06-28): resolve local, dial central**
When the device dials, the slave's modem resolves the number against its **local** dial-mappings
(part of its local AT config), then at connect hands the master either "**your menu/services**" or a
resolved "**host:port**"; the master serves its menu or **dials onward**. Dial-out always executes on
the **master's** network, so the data path for an external call is `device ↔ slave ↔ master ↔ BBS`
(two hops; negligible at retro baud). Rationale for B over the alternatives:
- **Model A (slave dials directly)** — simpler/fewer hops and the master isn't a SPOF for BBS calls,
  but BBS dial-out leaves each slave's own network (no central egress/log). *Rejected: we want the
  master in every path.*
- **Model C (central phonebook on the master)** — one shared BBS list, but the phonebook would be
  the lone piece of AT config *not* kept local. *Rejected: keep all AT config on the slave.*

So: per-slave phonebook (local), **master performs every dial** ("master controls everything"), and
the files-land-on-master invariant is unaffected (it depends only on the master serving its own File
Transfer, which is always reached by bridging to the master).

---

## 4. The real work

### 4.1 Byte-source bridging — much smaller under the AT-on-slave model
Originally (master runs the modem emulator) this was the biggest item: the emulator is built on a
**blocking** `Box<dyn SerialPort>` (`serialport` crate, dedicated thread — `serial_manager` 542,
`serial_thread` 906, `command_mode_tick` 1023, `open_serial_port` 711), so running it over an
**async** tokio relay stream meant a real sync/async impedance refactor.

The §3 decision (AT layer on the slave) **largely dissolves that**:
- **Slave side:** the modem emulator keeps running on its **native blocking UART** — no change. On
  CONNECT it bridges the blocking UART to the async relay (SSH/TCP) — exactly the existing
  **serial-console bridge** pattern (blocking port ↔ async duplex/session, serial.rs console-bridge
  slots), just pointed outward at the master instead of an in-process telnet session.
- **Master side:** the master feeds the incoming relay stream into its **existing async session
  machinery** — `TelnetSession`/menu, the file-transfer protocols, dial-out — all of which already
  speak `AsyncRead`/`AsyncWrite`. No emulator-over-async rewrite.

So the remaining work is *plumbing* (bridge the slave's console-style pump outward; accept a relay
stream on the master and hand it to a session), not a sync/async rewrite of the modem emulator.
**Required regardless:** the relay carries **raw serial semantics** (`is_tcp = false`, no IAC/CR-NUL)
end to end — see §9 #2.

### 4.2 Carrier / line-control — now mostly slave-local under the AT-on-slave model
The modem emulator runs on the slave against its **local** UART, so DTR/DSR/RTS/CTS/DCD, S7
carrier-wait, `&C`/`&D`, and DTR-drop=hangup are all handled **locally by the slave's emulator** —
the master never touches the device's serial lines. That collapses most of the old line-control
problem: there's no remote UART for the master to control, so the **RFC 2217 control-channel tier is
largely moot**.

What actually crosses the relay is just **connect / disconnect**:
- **Master → slave "call ended"** (the dialed BBS hung up, or the user left the master's menu) → the
  slave's emulator drops **carrier (DCD)** to the device → `NO CARRIER`. `+++`/`ATH` and in-band
  behavior work normally because the slave runs the emulator.
- **Slave → master "device hung up / relay-link lost"** → the master tears the session down
  (§9 #16). If the relay link itself drops mid-call, the slave's *local* emulator drops carrier to
  the device immediately (§9 #3) — natural now that the emulator is local, not a special case.

Baud/parity/flow are configured locally on the slave (it owns the UART). **No control sub-channel is
needed for v1** — the only cross-machine line semantics are the connect/disconnect events above.

### 4.3 Transport + auth — **SSH is the recommended path (reuses the existing SSH port)**

Two viable transports. **The SSH option is recommended**: it reuses the existing SSH port
(2222) while normal SSH logins from other PCs keep working, and it gives auth + encryption for
free.

**Recommended — relay as an SSH channel on the existing SSH server (port 2222):**
- The **slave is an SSH client** that connects to the master's SSH server and opens a
  **dedicated channel requesting a relay subsystem/exec** (e.g. `exec "serial-relay A"`), rather
  than a shell.
- The master's russh handler **routes by channel-request type**:
  - `shell` / `pty` request → the menu/`TelnetSession` (today's path — **normal SSH from other
    PCs, unchanged and concurrent**).
  - relay `exec`/`subsystem` request → the serial-intake machinery.
- **No new port.** SSH already serves many simultaneous connections, so interactive PC logins
  and slave relays coexist on 2222.
- **Auth + encryption come for free** from SSH — this *supersedes* the dedicated-raw-port +
  custom-credential/lockout approach below and removes the raw serial-injection attack surface,
  so the link is safe even over an untrusted network.
- Cost (grounded in current code):
  - **Server:** `src/ssh.rs` currently implements only `channel_open_session`, `pty_request`,
    `shell_request`, `data` — there is **no `exec_request`/`subsystem_request` handler yet**, so
    one must be added to do the routing.
  - **Client:** a russh **client already exists** —
    `impl russh::client::Handler for GatewayHandler` (`src/telnet.rs:1301`, the SSH-gateway
    proxy) — so the slave-side SSH-client relay reuses a proven pattern, not new infrastructure.
- **Do NOT** byte-sniff a raw protocol onto 2222 alongside SSH — fragile; use a real SSH channel.

**Alternative — dedicated raw TCP relay port (simpler to build, but "another port"):**
- A separate raw port for the relay — **NOT** the telnet menu port (telnet IAC/menu would
  corrupt binary transfers — the CR-NUL lesson). Keep it 8-bit clean (IAC-only or none).
- You must then **add auth yourself** (reuse the credential + per-IP lockout system) — a raw
  serial-injection port is real attack surface — and it's plaintext on the wire unless separately
  tunneled. Acceptable on a trusted LAN; weaker than the SSH option.

### 4.4 Multiplexing — both serial ports without confusion
A single **raw** byte pipe canNOT carry both ports — Port A and B bytes would interleave
indistinguishably. Keep them logically separate, three ways (best first):
1. **SSH channel multiplexing** (pairs with §4.3 recommended): **one** SSH connection from the
   slave carrying **two channels** (A and B). SSH does the framing/demux — one auth'd, encrypted
   connection, two independent streams, **zero custom framing code**. Answers "share the port"
   and "both ports, no confusion" together.
2. **One connection per port** (simplest for the raw-port alternative): Port A and Port B each
   get their own dedicated pipe. No confusion; reuses everything.
3. **Custom framing** (`[port_id][len][payload]`) over one raw socket — works but adds a
   framing/buffering layer and loses transparency. Only worth it to conserve connections, which
   isn't a real constraint here. Least attractive.

### 4.5 Config + 3-UI wiring + tests (project rule: telnet + web + gui)
New keys (defaults preserve today's behavior):
- `gateway_role = standalone | master | slave`  (default `standalone`)
- `slave_master_host`, `slave_master_port`       (slave → where to connect; port defaults to the
  SSH port, e.g. 2222, under the recommended SSH transport)
- `slave_master_username`, `slave_master_password` (slave → credentials it logs into the master's
  SSH server with; **must match the master's configured `username`/`password`**). Persisted in
  `egateway.conf` like the existing unified credentials (plaintext, file written `0600`). See §9 #6.
- `master_accept_relays = true|false`            (master gate; see §4.6 — with SSH transport the
  listener already exists, this just permits relay channels)
- `relay_transport = ssh | raw`                  (default `ssh`); `master_relay_port` only needed
  for the `raw` alternative (§4.3)
- per-port identity the slave advertises (e.g. `serial-relay A`) so the master knows which
  logical port a channel is
Wire into all three UIs; add config round-trip tests + a loopback bridge test (slave relay ↔
master intake over a local socket, run a transfer through it).

### 4.6 Operational model — is the master "always listening"? (yes, essentially)
With the **recommended SSH transport**, the master's SSH server is **already listening** whenever
SSH is enabled — there's no new listener to start. So the operator experience is exactly what you'd
hope:

- **Master:** enable SSH (as many already do) + set `master_accept_relays = true`. No per-slave
  configuration: the master **spins up a serial session on demand** for each incoming relay
  channel, keyed by the port identity the slave advertises. It just sits and accepts.
- **Slave:** enable slave mode, point at the **master's IP** (and SSH port), and enter the
  master's **username + password** (`slave_master_username`/`slave_master_password`, which must
  match the master's configured credentials). That's it — no key files to copy. All persisted in
  `egateway.conf`.

Two honest caveats so "just point at the IP" is the *whole* story:
1. The slave **authenticates with username + password** matching the master's configured
   credentials (the master's existing SSH `auth_password` handler validates them; the slave then
   requests the relay channel rather than a shell). So "configure the slave" includes those
   credentials, not only the IP. (Decided over a per-slave key — see §9 #6 for the tradeoff.)
2. `master_accept_relays` exists so an operator can keep SSH on for normal logins but explicitly
   opt **in** to accepting relays (and decide which logical port slots are allowed). If you'd
   rather it be zero-config, the gate could default on whenever `gateway_role = master` — a policy
   choice for implementation time.

For the **raw-port alternative**, "always listening" requires the master to *also* open
`master_relay_port` (another port) and run its own auth — which is the main reason the SSH
transport is preferred.

### 4.7 UI placement (telnet / web / GUI) — decided

The master/slave **settings live on a dedicated sub-screen**, reached by a single navigation
entry; the main Server Configuration screen is not cluttered with them.

> **Standing UI footprint (all roles).** The address-block move + the `M Master/Slave` nav line are
> present on **every** gateway regardless of role — you need the nav item to *set* the role in the
> first place. So this telnet menu layout shift applies to standalone/master/slave alike; it changes
> *presentation*, not behavior (see §9 #11). The functional/wire behavior of a standalone or
> no-slaves master gateway is unchanged.

**Telnet — the row-budget plan (PETSCII = 22 rows × 40 cols):**
- The Server Configuration screen is currently at **exactly 22 rows** worst-case (it's why the
  detected-IP list is capped at 3, `SERVER_ADDR_DISPLAY_CAP`). To make room *without* a footer
  hack and *without* removing any setting:
  - **Move the server-addresses block** — the `Server addresses:` label + capped IP list + the
    `ATD <ip>:<port>` example (≤5 rows worst-case) — **off** the Server Configuration screen and
    **onto the top of the CONFIGURATION (main config) menu**, right under the header (reads as a
    "here's your gateway / how to dial in" banner). This block is currently rendered *only* on the
    Server Config screen (telnet.rs:9767–9795), so relocating it creates no duplication.
  - Rows after the move: **CONFIGURATION** 14 → ~19–20 (≤22, headroom); **Server Configuration**
    22 → 17 static.
- **Add a dedicated menu line `M  Master/Slave`** on the Server Configuration screen (key `M` is
  free there; used keys are T P S O K J W B I R C D Q H) → opens the master/slave **sub-screen**.
  Server Config becomes 17 + 1 = **18 rows** (≤22, ~4 spare). No footer cramming; nothing else
  removed.
- The **sub-screen** (its own fresh 22-row budget) holds: `gateway_role`
  (standalone/master/slave); master **host / port / username / password** (slave — password entry
  masked, reusing the existing credential-entry pattern); per-port identity; and the **"Accept
  slave connections" toggle** (master).
- Test impact: `test_config_menu_row_count` covers both screens — bump the config-submenu figure
  by the address block and drop it from the server-config figure (which then adds the nav line);
  both stay `≤ 22`.

**GUI:** put the master/slave fields (role, master host/port/username/password, per-port identity,
"Accept slaves" toggle) under the **existing Server "More…" popup** (`gui.rs:368`) — password as a
masked field; no layout pressure.

**Web:** add a **"Master/Slave" section card** to the server config page with the same fields
(password as `<input type="password">`). The web config is a scrolling form of section cards (no
row limit, no "More" button) — no special handling needed.

**The "Accept slaves" enable toggle** lives on the sub-screen / More popup / web card (NOT on the
main Server Config screen). It is an explicit opt-in so accepting relays is never *implied* by SSH
being enabled for normal logins; alternatively it could default on whenever `gateway_role =
master` (policy choice at implementation time — see §4.6).

---

## 5. Effort & risk

- **Conceptual fit:** excellent. Additive, default-off (`standalone`), preserves all current
  functionality.
- **Effort:** moderate, and **lighter than first thought** now the AT layer is on the slave (§3):
  the big sync/async byte-source refactor (§4.1) mostly dissolves — the modem emulator stays on the
  slave's native blocking UART, and the master reuses its existing async session machinery. The real
  work is *plumbing*: the slave's outbound relay-on-connect (≈ the existing console-bridge, pointed
  at the master), the `exec`/`subsystem` routing on the russh server, the Model-B dial-relay
  (slave resolves → master dials), and the config/UI wiring. RFC-2217 line control (§4.2) is now
  largely **moot**, not just deferred.
- **Main risks:**
  - getting **connect/disconnect + teardown** right across the relay (§9 #3/#15/#16) — a dropped
    link must make the slave's *local* emulator drop carrier (`NO CARRIER`) and the master release
    the session/slot, rather than hang. Same teardown discipline as the serial-reconnect and Punter
    end-off work.
  - **two-hop dial path** (Model B: device ↔ slave ↔ master ↔ BBS) — keep raw 8-bit framing
    (#2) and backpressure (#7) intact end to end through both hops.

---

## 6. Suggested phased plan

- **P1 — Relay plumbing (§4.1): DONE 2026-06-30 (uncommitted on `dev`).** Transport-agnostic
  master-intake + slave-bridge primitive, loopback-tested. Shipped pieces:
  - **Master intake — `src/relay.rs` `run_master_relay_session(reader, write_half, peer_addr,
    shutdown, restart, lockouts)`:** wraps an accepted relay stream in a relay `TelnetSession` and
    runs the full session machinery (menu / transfer / dial-out). Transport-agnostic
    (`AsyncRead`/`AsyncWrite` only) — the P2 SSH `exec`/`subsystem` handler and the loopback test
    share this one path. `#[allow(dead_code)]` until P2 wires the production (SSH) caller.
  - **`TelnetSession::new_relay(...)` (telnet.rs):** master-side session for a relayed device.
    `is_serial = true` (terminal detection runs, raw 8-bit, **no telnet IAC / no CR-NUL** — §9 #2
    raw serial semantics), `serial_port_id = None` (owns no local port → every "own-port" check is
    correctly false; a relayed device may bridge to a local port), `peer_addr = slave IP`. Skips
    auth like a serial caller (the transport authenticates in P2).
  - **Slave-side bridge primitive:** `serial::online_mode_duplex` generalized from concrete
    `DuplexStream` halves to generic `R: AsyncRead + Unpin` / `W: AsyncWrite + Unpin`, so the same
    UART⇄async pump serves both the in-process dial bridges and the P2 outward relay. No standalone
    `bridge_uart_to_relay` wrapper yet — its caller (Model-B dial / SSH client) arrives in P2;
    adding an uncalled `pub fn` now would be dead code (clippy).
  - **Loopback test** (`src/relay/tests.rs`): a `tokio::io::duplex` pair drives a complete session
    over the relay (detect → main menu → quit/farewell → clean EOF), plus a raw-transparency probe
    (a 0xFF/IAC byte at the color prompt is passed through, not consumed as a telnet command).
  - Suite **1243 lib + 1 e2e green, clippy `--all-targets` clean.** Standalone behavior unchanged
    (relay code is additive; no production caller until P2).
- **P2 — SSH transport + config/UI + Model-B dialing: DONE 2026-06-30 (uncommitted on `dev`).**
  Shipped:
  - **P2a — Config + roles (config.rs):** `gateway_role` (standalone|master|slave, default
    standalone), `master_accept_relays` (default OFF — explicit opt-in), `slave_master_host/port/
    username/password`, `relay_transport` (ssh|raw, default ssh). Full lifecycle (const/struct/
    default/parse-validate/write/`apply_config_key`) + round-trip/defaults/validation tests.
  - **P2b — 3-UI wiring:** telnet `M Master/Slave` sub-screen (`master_slave_config`) with the
    §4.7 address-block relocation to the CONFIGURATION menu (`render_server_address_block`); row
    tests updated + `master_slave_help_lines` registered in `all_help_line_groups`. Web
    `frame_master_slave` card. GUI `draw_server_relay` in the Server "More…" popup
    (`slave_master_port_buf` wired through all 5 buffer sites). Per-port identity is *derived*
    from the port letter (not a config key).
  - **P2c — Master SSH relay intake (ssh.rs):** `exec_request` parses `serial-relay <port>
    menu|dial <host:port>` (shared `relay::parse_relay_command`), gates on `gateway_role==master
    && master_accept_relays`, and routes the channel → `run_master_relay_session` (menu) or
    `run_master_relay_dial` (onward dial). Per-channel `relay_writers` map so one slave connection
    carries Ports A+B as separate channels; `data()`/`channel_eof()` route by channel. `shell`/
    `pty` stay the interactive path. **Review findings 1 & 2 resolved:** auth is enforced by the
    existing `auth_password` before any channel request, and `run_master_relay_session` now
    registers/deregisters its writer in `session_writers`.
  - **P2d — Slave SSH-client relay + Model-B dialing (relay.rs + serial.rs):** `SlaveRelayHandler`
    + `connect_master_relay` (russh client, password auth, host key accept-on-first-use per the
    trusted-LAN threat model). `handle_dial` intercepts slave mode → `slave_resolve_relay_target`
    (local phonebook resolution → Menu or Dial) → `dial_master_relay` bridges the UART to the
    relay via the generalized `online_mode_duplex`. **Connect-per-call** for modem mode (no
    persistent connection needed there).
  - **P2e — tests:** command-contract round-trip (`RelayTarget::exec_command` ↔
    `parse_relay_command`), onward-dial transparent piping (`run_master_relay_dial` ↔ a fake TCP
    echo), plus P1's full-session loopback (the exact thing the SSH channel carries). All CI-able.
  - Suite green (1273 lib + 1 e2e at last full run), clippy `--all-targets` clean.
  - **Deferrals — DONE 2026-06-30 (this session):**
    - **Console-mode remote ports (§9 #12): DONE.** `serial-register <port>` exec → master holds
      the channel idle in a global `REMOTE_PORTS` registry; the Serial Gateway picker lists local
      A/B + registered remote ports (digit-keyed, capped at `REMOTE_PORT_DISPLAY_CAP=6` per the
      "cap not paging" allowance); on pick the master claims the channel, sends the one-byte
      `RELAY_ACTIVATE_BYTE`, and bridges via the existing console pump. Slave runs
      `console_slave_register_tick` (open port → register → await activate → bridge → reconnect,
      bounded/shutdown-aware); its own picker marks the relayed port "-> master" (ineligible).
      Registrations count against `max_sessions`; `channel_close` drops the registry entry.
    - **Review finding 3 (relay identity): DONE** — `is_relay` flag; `client_type_label()` →
      "Relay (slave)".
    - **`+++`/ATO resume across a relay call: DONE** — `ActiveConnection::Relay` preserves the SSH
      connection across `+++`; ATO resumes; clean EOF on disconnect/hangup.
    - **#13 slave-mode warning: DONE** — the slave's main telnet menu shows "SLAVE mode: ports
      relay to master" + the master address.
  - **Remaining deferrals (genuinely optional / out of band):**
    - **Real two-instance SSH smoke test** — manual ground truth (a CI test would race the
      process-global config singleton + write a host key to CWD; same reason CCGMS/VICE is manual).
    - **`relay_transport = raw` (P3): SKIPPED by decision (2026-06-30)** — SSH transport adopted;
      raw is the design's explicitly-skippable alternative. Config key retained, hidden from UIs,
      startup-warned if hand-set.
    - **Head-of-line blocking:** documented in `ssh.rs` `data()`. Not reachable today — the slave
      opens one channel per connection (modem connect-per-call; console one connection per port),
      so no two channels share a connection. The per-channel-pump fix belongs with any future
      single-connection multi-channel design.
  - **Two adversarial review passes (2026-06-30): all findings fixed** except the deferrals above.
    P2-review fixes shipped: relay connect timeout (15s), `channel_close` cleanup (no writer leak),
    onward-dial `copy_bidirectional` (no truncation), slave host-key TOFU pin via `gateway_hosts`,
    relay channels counted against `max_sessions`, shared `spawn_channel_reader`, clean `+++` EOF,
    and the master-ssh-disabled / raw-transport startup warnings. 1248 lib + 1 e2e green.
- **P3 — (alternative) raw-port transport:** only if a non-SSH path is wanted — dedicated raw
  relay port + its own auth/lockout. Skippable if SSH transport is adopted.
- **P4 — (optional, likely unneeded) Full line control:** an RFC-2217-style control sub-channel —
  now largely **moot** since the modem emulator (and all line control) lives on the slave (§4.2).

Each phase is independently testable and leaves `standalone` (today's) behavior untouched.

---

## 7. Key file references (current code, for whoever implements)

- `src/serial.rs`: `serial_manager` (542), `open_serial_port` (711), `serial_thread` (906),
  `command_mode_tick` (1023), `dial_tcp` (2396); console-bridge request slots / per-port active
  flags (~122–150); DCD/carrier (`&C`/S7) logic (97, 220, 1590).
- `src/tnio.rs`: 8-bit transparent raw I/O (IAC escaping; no CR-NUL).
- `src/ssh.rs`: russh **server**. Currently handles `auth_password` (361),
  `channel_open_session` (435), `pty_request` (444), `shell_request` (459), `data` (544) —
  **no `exec_request`/`subsystem_request` yet**; add one for relay-channel routing (§4.3 / P2).
- `src/telnet.rs`: session machinery, auth, per-IP lockout (shared with SSH/web). Also holds the
  russh **client** `impl russh::client::Handler for GatewayHandler` (1301, the SSH-gateway proxy)
  — the slave SSH-client relay reuses this pattern.
- `src/config.rs`: key parse/validate/write + `DEFAULT_*` consts (new keys go here first).
- **Telnet menus (§4.7):** `server_configuration` (telnet.rs:9704) — drop the address block
  (9767–9795), add the `M Master/Slave` line; CONFIGURATION parent menu (~9088, items E/G/M/S/F/O/R)
  — add the address block at the top; `SERVER_ADDR_DISPLAY_CAP` (telnet.rs:120);
  `test_config_menu_row_count` (telnet.rs:15334) — update both screens' row math (≤22).
- 3-UI parity: `telnet.rs` (menus/setters), `webserver.rs` (form fields — add a Master/Slave
  section card), `gui.rs` (fields — Server "More…" popup, gui.rs:368).
- `versionchange.txt`: doc files to bump on release (already lists the web reference pages).

---

## 8. One-line summary

A master/slave serial extender is a natural generalization of the gateway's existing
`ATDT ethernet-gateway` in-process bridge across the network. **Model (decided 2026-06-28): the
AT/modem layer runs on the slave**; once a device connects, the slave bridges the *session* to the
master, which provides the menu, file transfer, and dial-out (**Model B**: the slave resolves its
local phonebook, the master dials). **Transport: an SSH channel on the master's existing SSH port**
— normal SSH logins keep working, both ports ride as two SSH channels on one auth'd+encrypted
connection (no custom framing), the master is "always listening," and the slave just needs
slave-mode + the master's IP + username/password. Keeping the AT layer on the slave means **all
modem config (incl. `AT&W`) stays local** (no config to ship) and the old sync/async byte-source
refactor mostly dissolves — the emulator stays on the slave's blocking UART and the master reuses
its existing async session machinery; the main work is relay plumbing + the `exec`/`subsystem`
routing + Model-B dialing. **Files always land on the master** (it's the far end of every connected
call). Fully additive; default `standalone` behavior is unchanged.

---

## 9. Open details to nail down (pre-implementation)

Surfaced in the 2026-06-28 design review. Some now decided; the rest flagged.

> **Numbering:** the `#n` are **discovery-order IDs** (stable handles for cross-references like
> "§9 #2"), *not* sequential — items are grouped below by **status** (Decided / Required / Resolved /
> Minor), so the numbers jump around within each group. That's intentional; don't renumber.

### Decided (2026-06-28)
- **#1 No modem config crosses to the master — the AT layer runs on the slave (revised; supersedes
  "slave advertises its config").** Under the §3 AT-on-slave model the slave's modem config (mode,
  S-registers, **dial-mappings/phonebook**, `&C`/`&D`, idle timeout, `AT&W` persistence) stays
  **entirely local** — nothing is advertised or shipped. The only thing that crosses at connect is
  the **dynamic call target**: for a modem-mode port, "the master's menu/services" or a `host:port`
  the slave resolved from its local dial-mappings (Model B, §3 Dialing); for a console-mode port,
  just "bridge me" when the master picks it (#12). This is the change that motivated the whole model
  revision — it removes the config-advertisement protocol and the old `AT&W` problem (#17).
- **#4 Identity = slave IP + port letter.** A remote-port slot is keyed by `(slave_ip, A|B)`.
  Stable across reconnects from the same slave (same slot — the call/session resets and carrier
  drops, but the identity persists). Disambiguates multiple slaves cleanly.
- **#8 Relay channels count toward the SSH session cap.** No separate cap. A master serving many
  slaves **plus** human SSH logins should have its SSH session cap sized accordingly so the two
  don't starve each other.
- **#12 Remote ports appear in the master's Serial Gateway picker (console-mode).** A telnet/SSH
  user on the master can bridge to **local A/B *and* registered remote (slave) console ports**,
  gated by the slave's mode: a **console**-mode slave port is offered — the slave registers it with
  the master as "available," and the **master** initiates the bridge when a master user picks it
  (master reaches **inward** to the slave's device). A **modem**-mode slave port is **not** in the
  picker — the **slave** runs its own emulator and bridges to the master only when its device dials
  out (§3). So the two directions are: modem = slave-initiated on CONNECT; console = master-initiated
  on pick. This is the inbound counterpart that makes console-mode remote ports reachable at all.
  Work:
  - generalize `gateway_serial_picker` (telnet.rs:6644) + `any_port_console_eligible` /
    `check_console_bridge_eligible` from the fixed `SERIAL_PORT_IDS = [A, B]` enum to "local A/B +
    connected remote ports" keyed by `(slave IP, port)` (#4) — the picker is a consumer of #1's
    data model;
  - **Never hide the Serial Gateway item — make the picker the single eligibility authority
    (decided 2026-06-29).** Today the menu item is gated by **two** conditions at telnet.rs:3418-3420
    — `!self.is_serial` AND `any_port_console_eligible(&config)` — plus a **third** blanket reject
    inside `gateway_serial()` (telnet.rs:6760: `if self.is_serial { …not available… }`). **Drop all
    three blanket gates** and always render `G Serial Gateway`:
    - The `!self.is_serial` gate over-hides: it blocks a serial-arrived user (e.g. a local device
      that did `ATDT ethernet-gateway`) from reaching a *different* local port or a *remote* slave
      port — validation scenario **V2** is exactly this path. Replace the function-level blanket
      reject with a **per-port own-port reject** (refuse only when the picked id equals the arrival
      port); the picker **excludes only the arrival port** so a serial user still can't loop to
      itself.
    - The `any_port_console_eligible` gate was there to avoid dead-ending at the picker. But the
      picker already reports unavailability gracefully ("No port is in console mode — set one via
      Config > M", telnet.rs:6708), so an always-present item never truly dead-ends. **Dropping this
      gate is the bigger win under master/slave:** eligible targets become live runtime state
      (slaves connect/disconnect), so keeping the gate would make the menu item **flicker in and out**
      as slaves come and go — surprising UX. A **stable** item that sometimes says "nothing available
      right now" is clearer, and it collapses the eligibility logic into one place (the picker)
      instead of splitting it between a menu gate and the picker.
    - Net effect: **less conditional logic than the old "relax to per-port but keep the eligibility
      gate" plan**, a stable menu, and the picker as the single source of truth. The picker still
      reads the **live remote-port registry** (runtime state, not just `&config`) to list registered
      remote console ports alongside local A/B.
    - **Standalone-shippable precursor — DONE 2026-06-29 (uncommitted on `dev`).** Dropping the
      `is_serial` gates is independent of master/slave: it lets a serial-arrived user bridge to the
      *other local* port today (e.g. Port A's C64 ↔ Port B's device). Implemented as a small,
      separately-testable change ahead of the relay feature:
      - **telnet.rs ~3414** — Serial Gateway menu item is now **always rendered** (both the
        `!self.is_serial` and `any_port_console_eligible` conditions removed).
      - **telnet.rs `gateway_serial()`** — blanket `if self.is_serial { reject }` replaced by a
        **per-port own-port reject** keyed off the new `is_own_arrival_port(id)` helper (rejects only
        when the picked port == the arrival port).
      - **telnet.rs `gateway_serial_picker()`** — marks **only the arrival port** ineligible
        ("Your port", dim) for a serial session; reworded the no-eligible-target fallback to
        "No port is available to bridge. / Enable console mode via Config > M." (no longer falsely
        claims no console port when the user's own port is the only console one).
      - **serial.rs** — removed the now-unused `any_port_console_eligible` helper + its test (would
        be dead code → clippy failure in this binary crate).
      - Tests: added `test_non_serial_session_owns_no_port` + an `is_own_arrival_port` block in
        `test_telnet_session_new_serial_stores_port_id`; updated the picker fallback-string fit test.
        Full suite **1242 lib + 1 e2e green, clippy clean**. The remaining master/slave work
        (remote-port registry in the picker, paging/cap, live-registry visibility) is unchanged and
        still pending.
  - the picker uses **two lines per entry**, so against the 22-row PETSCII budget many slaves
    (× 2 ports) overflow → add **paging or a cap** (like the detected-IP cap);
  - the existing one-user-per-port active-bridge flag extends to remote ports.
  - **Persistent registration channel.** Unlike a modem-mode port (slave-initiated *on connect*), a
    console-mode slave port has no device-initiated trigger — so the slave must open a **persistent
    relay channel to the master at startup and keep it idle**, registering the port as "available"
    for the master to bridge on demand. Implication: a registered-but-idle console port **consumes a
    session-cap slot (#8) the whole time it's registered**, not only during an active bridge — size
    the master's SSH session cap to include idle console registrations.
- **#13 Slave inbound telnet/SSH stays independent (Option 1) + a slave-mode warning.** In slave
  mode the slave keeps serving its **own** telnet/SSH normally — slave mode only adds the outbound
  serial relay (purely additive). Consequences:
  - The slave's relayed serial ports show **busy/unavailable** in the slave's *own* Serial Gateway
    picker (reusing the one-user-per-port active-bridge flag — no special case).
  - An operator wanting a **headless** slave just turns off the inbound servers with the existing
    `telnet_enabled` / `ssh_enabled` / `web_enabled` toggles — no new mechanism.
  - **No auto-forward to the master in v1.** Proxying inbound telnet/SSH into the master's menu is a
    separate feature with its own auth / loop (slave→master→bridge-to-slave's-own-port) / attack-
    surface implications; defer (the SSH-gateway proxy machinery could seed it later).
  - **Warning on the main telnet menu:** when `gateway_role = slave`, show a notice — e.g.
    *"This gateway is in SLAVE mode — connect to the master at `<master IP>` instead"*
    (red/amber, near the top). The master IP is always known (it's `slave_master_host`) and is
    **confirmed reachable once a relay is connected**, so display it directly; optionally annotate
    the live connection state (e.g. "master 192.168.1.10 — connected" vs "… — not connected") since
    the slave already tracks its relay link for reconnect. Room is fine: the main menu is **16/22
    rows** (row-count test in telnet.rs), so a 1–2 line warning lands at 17–18. Shown **only** in
    slave mode; standalone / master menus are unchanged. (Mirror the notice in the web/GUI server
    pages if cheap.)
- **#18 Roles are mutually exclusive — no cascading in v1.** `gateway_role` is exactly one of
  standalone/master/slave. A node cannot be both a slave (relaying upstream) and a master (accepting
  downstream) — no multi-hop chains. Prevents relay loops and bounds complexity; revisit only if a
  concrete need appears.
- **#6 Slave auth = username + password.** The slave authenticates to the master's SSH server with
  `slave_master_username`/`slave_master_password`, which must match the master's configured
  unified credentials; the master's existing `auth_password` (ssh.rs:361) validates them and the
  channel-request type (relay vs shell) does the routing. Chosen over a per-slave key + allowlist
  because **no one wants to copy a key/config over** — username+password is the natural,
  zero-file-shuffling setup. Persisted in `egateway.conf` (plaintext, `0600`, like the existing
  credentials). Master side needs **no** new auth code — only the channel routing + the
  `master_accept_relays` gate. Known tradeoffs (acceptable under the trusted-LAN threat model):
  (a) the slave reuses the master's *human-login* credentials, so a compromised slave config grants
  master login — keep the slave's `egateway.conf` protected (`0600`); (b) since the creds are full
  master credentials, the relay gate (`master_accept_relays`) + channel-type routing are what keep
  a relay connection from also opening a shell; (c) **rotating the master's password breaks every
  slave at once** — each slave's stored `slave_master_password` goes stale, so they fail auth and
  (per #14) risk tripping the per-IP lockout. Operationally: when you change the master's password,
  update every slave's stored credential too.

### Required behaviors (correctness — not optional)
- **#2 Relay streams are RAW serial (`is_tcp = false`).** The far end is a real UART device that
  doesn't speak telnet, so the master's session over the relay must NOT apply IAC escaping or
  CR-NUL stuffing (the bug we just removed). The §4.1 byte-source must carry "serial semantics"
  regardless of the TCP/SSH transport underneath. *(Refines §4.1.)*
- **#3 Carrier drop on relay-link loss.** If the slave↔master link dies mid-call, no in-band byte
  can reach the attached machine — so the **slave must locally drop DCD/DTR** to give it
  `NO CARRIER`. v1 therefore needs *local* serial line-drop on link loss, even though data is
  otherwise in-band-only. *(Refines §4.2 — the "in-band only, no line control" tier is not
  sufficient for this case.)*
- **#5 The slave owns its full port config AND runs the modem-command layer (revised).** Under §3
  the slave opens its UART (baud/parity/databits/flow) *and* runs the modem emulator locally
  (AT/S-registers/`AT&W`/dial-mappings). It chooses which ports to relay and, per port, modem vs
  console mode. (This supersedes the earlier "slave runs no modem logic" framing.)
- **#7 End-to-end UART backpressure.** The slave must pace TCP→UART at line rate, reading the
  socket only as fast as it can write the wire, so the fast master/relay can't overrun the slow
  UART (critical for file transfers). TCP backpressure handles this *if* the slave doesn't
  pre-drain the socket.
- **#14 Reconnect policy — keep trying, but distinguish network vs auth vs relay-refused failure.**
  **DONE 2026-06-30 (post-review).** `connect_master_*` now return `relay::RelayConnectError`
  {`Network`|`Auth`|`Refused`}; `console_slave_register_tick` classifies the failure and backs off
  per class — `Network` capped-exponential 1→30 s, `Auth` 6 min (> the 5-min lockout window so a
  wrong-credential slave never self-bans), `Refused` 60 s — and logs the outage **once**
  (`should_log_outage`) instead of every retry. Modem-mode dial is connect-per-call (device redials),
  so it just surfaces the reason + `NO CARRIER`. Tests: `test_next_network_backoff_*`,
  `test_relay_reconnect_delay_*`, `test_should_log_outage_*`, `test_relay_connect_error_*`.
  Original requirement below:
  Yes, the slave keeps trying. Reuse the proven serial-reconnect pattern (commit `4cfad87`): **log
  the outage once** (no ~2/sec spam), **honor the shutdown/role-change flag** (no spin; exits
  cleanly), and reconnect automatically when the master returns. Applies to both the **initial**
  connect (master not up yet) and a **mid-session** drop. Three refinements:
  - **On a mid-session drop, signal the local device first** (§9 #3): drop DCD/DTR so the attached
    machine sees `NO CARRIER`, *then* retry — don't silently stall.
  - **Network/transport failure vs auth rejection are different.** A transport failure (master
    down, link dropped) → retry briskly with a **capped backoff**. An **auth rejection** (wrong
    `slave_master_username`/`password`) must **NOT** be retried tightly: the master's lockout is
    **3 failures → 5-minute per-IP ban**, shared across telnet/SSH/web (telnet.rs:121–122), so
    hammering bad creds **locks the slave's own IP out** (and burns the relay's share of #8's cap).
    On auth rejection, back off hard (minutes) or pause and surface the reason, reflected as the
    "not connected" state in the main-menu warning (#13: e.g. "master 192.168.1.10 — auth
    rejected").
  - **Authenticated-but-relay-refused is a *third* mode, distinct from both.** If login succeeds
    but the master refuses the relay channel — it's in `standalone`, `master_accept_relays` is off,
    or it's an older build with no relay handler — the slave must **not** treat that like a
    reconnectable outage and hammer it. Back off hard (like auth rejection) and surface a config-
    level message ("master is not accepting relays"), since the target is reachable and authenticating
    fine; only the relay request is being declined.
  - Each relayed port/channel retries independently.
- **#15 Dead-link / half-open detection (keepalive).** **DONE 2026-06-30 (post-review).** SSH
  keepalive enabled on both ends of relay links — the slave relay client (`relay.rs`
  `keepalive_interval=30s, keepalive_max=3`) and the master SSH server (`ssh.rs`, same), with no
  `inactivity_timeout` so an idle-but-alive console registration stays up. A dead link is now
  detected in ~2 min: the slave's reconnect loop (#14) re-establishes, and on the master the dead
  connection's `SshHandler::drop` releases the session slot + remote-port registry entry (so #3/#16
  actually fire). Original requirement below:
  There is **no keepalive anywhere** in the
  codebase today. A *silently* dropped relay link (master powered off, cable pulled, NAT
  idle-timeout) isn't noticed until the next write fails — so #3 (carrier drop) and #14 (reconnect)
  won't fire promptly, leaving a **stale master-side session** and a slave that wrongly believes it
  is connected. Enable **TCP keepalive and/or SSH-level keepalive** on relay links (both ends), with
  a bounded interval, so a dead link is detected and #3/#14 actually trigger.
- **#16 Master-side teardown + slot release on relay drop.** When a relay link drops (or keepalive
  fails, #15), the master must tear down whatever that connected call was driving on its side — the
  bridged menu session, an in-flight file transfer, or a dial-out connection it opened on the
  device's behalf (Model B) — and, if a master-side telnet/SSH user was console-bridged to
  that remote port (#12), **end their bridge** ("Serial bridge closed") and **release the
  `(slave-ip, port)` slot** so the slave's reconnect re-establishes cleanly rather than colliding
  with a stuck/duplicate slot. Also define the **duplicate-connect policy**: if a new relay arrives
  for a `(slave-ip, port)` slot still marked active (e.g. the old link half-died, #15 hasn't fired
  yet), the new one should **take over** (displace + tear down the stale slot) rather than be
  rejected — otherwise a reconnecting slave is locked out by its own ghost until keepalive expires.
- **#21 Master-as-dial-proxy egress surface (Model B).** Because the master dials on a slave's
  behalf (§3 Dialing), an **authenticated** slave can make the **master** open a TCP connection to
  *any* `host:port` it asks for — `dial_tcp` has no egress guard (dialing arbitrary hosts is a
  modem's whole job), so Model B effectively turns the master into an authenticated outbound proxy.
  Acceptable under the trusted-LAN / authenticated-slave threat model, but it is a **new attack
  surface** worth stating: a compromised or hostile slave could probe/reach internal services on the
  *master's* network. **Deferrable knob** (implementation time): default **accept any target** (matches
  today's modem behavior); add an optional **egress allowlist** only if a master sits on a more-trusted
  network than its slaves. Note the master's web browser already has an SSRF guard — the dial path
  deliberately does not, so this is a conscious choice, not an oversight.

### Resolved by the §3 model
- **#17 `AT&W` from a modem-mode device — RESOLVED.** Because the modem emulator runs on the
  **slave** (§3), `AT&W` is a normal **local write** to the slave's own `egateway.conf`, exactly as
  today — no cross-machine persistence question. The earlier options (a/b/c) and the whole problem
  are moot. (This, with old #1, is *why* the model moved the AT layer to the slave.)

### Minor / nice-to-have
- **#9 Channel-open handshake:** advertise identity + a protocol-version byte so mismatched
  master/slave versions fail cleanly — same handshake that carries #1's advertised config.
- **#10 Observability:** operator-visible status/log — "slave <ip> port A connected", "relay link
  lost — carrier dropped".
- **#11 Through-relay interop tests:** run the existing CCGMS / lrzsz interop *through* the relay
  hop to prove transfers survive it; plus an in-process fake-slave harness. Also pin a regression
  test that `gateway_role = standalone` (and `master` with no slaves connected) preserves today's
  **functional/wire behavior** — transfers, dialing, server toggles, SSH logins. **Caveat — not
  byte-for-byte UI:** §4.7 intentionally relocates the server-address block to the main CONFIG menu
  and adds the `M Master/Slave` nav line for *all* gateways (you need it to set the role), so the
  telnet **menu layout** changes regardless of role. The regression covers behavior, not menu
  layout. Likewise, master mode is functionally inert until a slave connects, but once slaves
  connect their relay/registration channels consume SSH session-cap slots (#8) — size the cap.
- **#19 Role change restarts the subsystem; keys are role-gated.** Changing `gateway_role` (or the
  master host/creds) restarts the relay/serial subsystem, consistent with the existing
  server-config "restart notice". Irrelevant keys are ignored per role (a slave ignores
  `master_accept_relays`; a master ignores `slave_master_*`).
- **#20 File location + picker labels.** Files for relayed transfers land in the **master's**
  `transfer_dir` (centralized storage — a feature, but state it in docs). The Serial Gateway picker
  (#12) must **label remote ports by slave** (e.g. "Slave 192.168.1.50 Port A") so multiple slaves
  are distinguishable.

### Documentation & help to update (required on ship, follows the project's doc-sweep discipline)
- **#22 Doc/help surfaces to update when the feature lands** (alongside the implementation, not as an
  afterthought):
  - **Telnet in-app help** — add a **Master/Slave help screen**: keep its lines in a
    `*_help_lines()` fn and add it to the **help-fit aggregate test's groups array** (the established
    pattern — fit tests iterate the fns, never hand-copied), respecting the **22-row × 40-col PETSCII
    budget**. Update the Server Config / main CONFIG help text to mention the new `M Master/Slave`
    nav item + the relocated address block (§4.7), and document the slave-mode warning (#13).
  - **usermanual.html** — add a **Master/Slave setup** section (role; master host/port/username/
    password; "accept slaves" toggle; files-land-on-master behavior; slave warning; reconnect/auth
    notes). Then **regenerate `usermanual.pdf` with WeasyPrint** (per `versionchange.txt` — Producer
    must stay "WeasyPrint").
  - **web/index.html** — add a **Master/Slave** section mirroring the new web config card and the
    usermanual content.
  - **README.md (only if needed)** — document the new config keys (`gateway_role`,
    `slave_master_host`/`_port`/`_username`/`_password`, `master_accept_relays`, `relay_transport`),
    sourcing defaults from the `DEFAULT_*` consts in `config.rs` and cross-checking the web form +
    conf-writer (the "defaults come from config.rs" rule).
  - **CHANGELOG.md** — an entry under the shipping version.
  - **versionchange.txt** — only if a *new* web page is added for the feature (a dedicated reference
    page), add it to the version-bump checklist; otherwise no change.

---

## 10. Validation scenarios (worked examples the design must satisfy)

Sanity checks traced end-to-end through the plan. Each must hold for the design to be coherent.

### V1 — Files from devices on *either* gateway land on the master
- **Master-local device** (modem mode): `ATDT ethernet-gateway` → master's menu → **File
  Transfer** → master's `transfer_dir`. ✓
- **Slave device** (modem mode): on CONNECT the slave bridges to the **master's** menu → **File
  Transfer** → master's `transfer_dir` (§3 invariant — the master is the far end of every connected
  call). ✓
- Scope note: this is the gateway's **File Transfer feature**. A device dialing an *external BBS*
  transfers end-to-end with that BBS (stored on neither gateway) — normal, not a "gateway file."

### V2 — From the master, reach a slave's telnet-serial (console) device
Flow: master-local device does `ATDT ethernet-gateway` → master's menu → **Serial Gateway** → picks
the **slave's console-mode port** → bridged. Path: `terminal ↔ master ↔ relay ↔ slave ↔ device`.
Relies on:
- **#12** — the master's Serial Gateway picker lists local A/B **+ registered remote console ports**
  (live remote registry, not just `&config`); the **main-menu item is always shown** (never hidden),
  so it's reachable whether or not a local console port exists;
- **#12 never-hide + per-port loopback reject (critical for this path)** — V2 arrives via
  `ATDT ethernet-gateway` (a local serial device → `is_serial == true`), so the *current* blanket
  `!self.is_serial` menu gate AND the `gateway_serial()` blanket reject would block it entirely. V2
  requires both blanket gates dropped — the menu item always shows, and the picker **excludes only
  the arrival port** (own-port reject), so the slave's remote console port is still reachable;
- **#12 persistent registration** — the slave keeps an idle relay channel open so the console port
  is "available" for the master to bridge on pick (consumes a session-cap slot, #8);
- **#13** — that same console port shows **busy** in the *slave's own* picker (it's owned by the
  master's bridge; one-user-per-port).
- Nuance: the Serial Gateway is a **transparent terminal pipe**, not the File Transfer feature — V2
  gives interactive terminal access to the slave's device; it is *not* the path by which "files land
  on the master" (that's V1). The two are different features and both hold independently.

### V3 (implied) — A slave device dials an external BBS
`ATDT <number>` on the slave → slave resolves its **local** phonebook → hands the master the target
→ **master dials** (Model B) → `device ↔ slave ↔ master ↔ BBS`. File transfers here are end-to-end
with the BBS (V1 scope note). Confirms Model-B dialing + raw framing (#2) + backpressure (#7) across
two hops.

## Review follow-ups (dev TODOs, deferred — not user-facing)

Surfaced by the 2026-06-30 quality/stability review passes (after the feature
shipped to `dev` as `b18fc79..f11589e`; fixes landed as `75058ea` + `d7e7797`).
These two were **intentionally deferred** — neither is a hang or data loss, both
are pre-existing architecture or cosmetic, and a fix costs more than it's worth
under the trusted-LAN threat model. Revisit only if the cost/benefit changes.

- **(P2) Shutdown "Goodbye" broadcast is telnet-coupled. RESOLVED 2026-07-01.**
  The `session_writers` shutdown broadcast loop lived inside the *telnet* accept
  task, so on an SSH-only deployment (`telnet_enabled = false`) it never ran — no
  goodbye for SSH shells *or* relay sessions. Fixed by hoisting it into a
  transport-neutral primitive, `telnet::broadcast_to_sessions(writers, msg, close)`,
  invoked from the central shutdown step in `main.rs` so it runs for **any**
  combination of enabled servers. The telnet accept loop now just breaks on
  shutdown. Serial sessions (blocking threads, no async writer in the list) emit
  the same notice from `serial::serial_thread` on the shutdown flag; the message
  is unified in one constant (`telnet::SHUTDOWN_GOODBYE`). The helper is also the
  hook for future all-session broadcast messages. Verified live: a paramiko SSH
  client on a `telnet_enabled=false` gateway received the goodbye + EOF on SIGTERM.
  (Related: the broadcast's `shutdown()` on a relay's gateway-side write half
  doesn't directly EOF the parked read either — see the doc comment in
  `relay.rs::run_master_relay_session`.)
- **(P3, cosmetic) Slave connect `block_on` can race runtime drop on shutdown.**
  The slave serial thread is a detached `std::thread`; its master-connect
  `block_on` is bounded by `RELAY_CONNECT_TIMEOUT` (15 s) but doesn't re-check
  `shutdown` mid-handshake. If shutdown fires while connecting to a master that
  accepted TCP but stalls, `main.rs` force-drops the runtime at ~2.5 s and that
  thread's `block_on` can print a "runtime is shutting down" panic line on exit.
  Process still exits cleanly. Fix options: a shutdown-poll inside the connect
  `block_on`, or a shorter connect timeout.
