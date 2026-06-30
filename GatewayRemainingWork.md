# Gateway Master/Slave — Remaining Work Plan

**Status as of 2026-06-30 (origin/dev `2a884ed`):** the master/slave serial
extender is feature-complete against `GatewaySlavePlan.md` §6 (P1+P2) and
**every §9 "Required (not optional)" item is implemented** (incl. #14 reconnect
backoff classification and #15 keepalive). Three code-review passes found no
remaining defects. This document tracks the work we deliberately deferred plus
one new feature request (drive DCD). **None of this is done yet** — it's the
"later" pile. Tackle in roughly the order below.

Companion docs: `GatewaySlavePlan.md` (the design), `README.md` (user docs +
"Relay limitations" + "Outbound Connections" sections).

---

## 1. Manual two-instance SSH smoke test  *(highest value)*

The one thing no unit test covers. A CI test can't: the config is a
process-global singleton, the SSH host key is written to CWD, and we need two
live processes talking over a real socket. So this is a documented manual
procedure (same rationale as the CCGMS/VICE harnesses).

### Setup
- **Two working directories**, each with its own `egateway.conf` and its own
  host key (run each instance from its own dir). E.g. `~/relay-test/master/`
  and `~/relay-test/slave/`.
- **Serial ports for the slave:** use a `socat` PTY pair so no real hardware is
  needed — `socat -d -d pty,raw,echo=0,link=/tmp/ttyGW pty,raw,echo=0,link=/tmp/ttyDEV`
  then point `serial_a_port = /tmp/ttyGW` and drive the "device" end
  (`/tmp/ttyDEV`) with a terminal (`minicom`/`cu`) or a script. (Reuse the
  `~/claude/punter-vice/serial/` harness pattern.)
- **Master `egateway.conf`:** `ssh_enabled = true`, `gateway_role = master`,
  `master_accept_relays = true`, set `username`/`password`, pick an SSH port.
- **Slave `egateway.conf`:** `gateway_role = slave`, `slave_master_host`/
  `slave_master_port`/`slave_master_username`/`slave_master_password` pointed at
  the master, one serial port in `modem` mode (scenarios 3–5, 7) and/or
  `console` mode (scenario 6).

### Scenarios (each: steps → expected)
1. **Modem dial → master menu.** Device does `ATDT ethernet-gateway` (or the
   gateway number). → CONNECT; the **master's** menu renders over the link;
   terminal detection runs; menu navigation works.
2. **Onward dial (Model B).** Device does `ATDT <number-in-slave-phonebook>` or
   `ATDT host:port`. → slave resolves locally, master dials on its network,
   transparent bytes both ways (`device ↔ slave ↔ master ↔ BBS`).
3. **File transfer over relay (the core promise).** From the master menu,
   upload and download a **binary** file (include bytes `0xFF`, `0x00`, `0x1B`,
   `0x0D`). → bytes are byte-identical end to end; uploaded files land in the
   **master's** `transfer_dir`. Try XMODEM/YMODEM/ZMODEM at least.
4. **Console-mode register → pick → bridge.** Slave port in `console` mode;
   confirm it appears in the **master's** Serial Gateway picker as a remote
   port (`A @ <slave-ip>`); pick it → master user is bridged transparently to
   the slave's console device; the slave's *own* picker shows that port as
   `-> master` (ineligible).
5. **`+++` / ATO across a relay menu call.** Mid-session `+++` → OK; `ATO`
   resumes the same call. Then verify the **idle-timeout caveat**: park with
   `+++`, wait past the master's `idle_timeout_secs`, `ATO` → `NO CARRIER`
   (documented behavior). Onward-dial relay has no such timeout.
6. **Reconnect policy (#14).**
   - *Network:* kill the master mid-idle-registration → slave logs the outage
     **once**, retries with capped backoff (1→30 s); restart master → slave
     logs "reconnected" and re-registers.
   - *Auth:* set a wrong `slave_master_password` → slave logs auth-rejected
     **once**, backs off ~6 min, and the **slave's own IP is NOT locked out**
     of the master's telnet/SSH (verify a normal login from the slave host
     still works — proves the 6-min backoff > 5-min lockout window).
   - *Refused:* point the slave at a `standalone` master (or one with
     `master_accept_relays=false`) → slave logs "not accepting relays", backs
     off 60 s, does not hammer.
7. **Keepalive / dead-link (#15).** Silently sever the link (e.g.
   `sudo iptables -A INPUT -p tcp --dport <sshport> -j DROP`, or `kill -STOP`
   the master) while a console registration is idle. → within ~2 min the slave
   detects the dead link and reconnects, **and** the master reaps the dead
   connection: its session-cap slot is released and the stale remote port
   disappears from the picker (`SshHandler::drop`). Remove the rule / `CONT` to
   confirm recovery.
8. **Host-key TOFU.** First slave connect → master key pinned to the slave's
   `gateway_hosts` (log: "pinned master … first contact"). Then tamper that
   entry (or regenerate the master's host key) → next connect is **refused**
   ("host key CHANGED"), classified as Auth (hard backoff), not a hammer loop.
9. **Standalone regression.** Run a plain `gateway_role = standalone` instance
   and confirm transfers, dialing, server toggles, and SSH logins behave
   exactly as before (the relay code is inert).

### CI-able complement (see #11 below)
An in-process **fake-slave / fake-master** harness (two `TelnetSession`/relay
halves over `tokio::io::duplex` or a loopback `TcpListener`, driving a scripted
binary transfer through the relay) would cover most of scenarios 3/4 without two
processes. Worth building so transfers-over-relay stop being manual-only.

---

## 2. Drive DCD / hardware carrier  *(new request — must not affect users without a DCD pin)*

**Goal:** when a connection is established, signal "carrier present" on a
hardware line so a vintage terminal configured for carrier detect sees it; drop
it on `NO CARRIER` / `ATH` / relay-link-loss (#3). Closes the gap the README
"Limitations" section documents (AT&C is parsed/stored but no pin is driven).

### Hard constraint (the cabling reality — investigated 2026-06-30)
A PC / USB-serial adapter is wired as **DTE**. `serialport` 4.9 only exposes
**`write_data_terminal_ready` (DTR)** and **`write_request_to_send` (RTS)** as
drivable outputs; **DCD/DSR/CTS/RI are read-only inputs** — we **cannot drive a
DCD pin directly**. The standard modem-emulator approach is therefore: **drive
DTR (carrier proxy)** and let the user's **null-modem / DCE cable cross
DTR→DCD** into the vintage machine's DCD input (this is how tcpser et al. do
it). Document the wiring; optionally allow RTS instead of DTR via config.

### "Does not affect users without a DCD pin" → default-off per-port opt-in
- New per-port config key, e.g. **`serial_a_drive_carrier` / `serial_b_drive_carrier`
  (default `false`)**. When **off, the gateway never touches DTR/RTS** — behavior
  is byte-for-byte identical to today, so anyone without DCD wiring is wholly
  unaffected. This is what satisfies the constraint.
- Optional second key for **which line** to drive (`dtr` default | `rts`) if we
  want flexibility; can be deferred (start with DTR only).
- Wire the new key(s) into **all three UIs** (telnet config sub-screen, web
  form, GUI) per the project rule, with defaults from `DEFAULT_*` consts and
  README/conf-writer parity.

### Semantics (tie to existing AT&C, already parsed)
- `AT&C0` (default): DCD forced **always on** → assert DTR for the lifetime of
  the open port (when the opt-in is on).
- `AT&C1`: DCD **follows carrier** → assert DTR on CONNECT, drop on
  `NO CARRIER`/`ATH`/disconnect/relay-loss, re-assert on the next CONNECT.
- (AT&D — *DTR from the terminal* — is the read direction and is separate; out
  of scope here, still a documented limitation.)

### Hook points (serial.rs)
- `open_serial_port` / port open: set the initial line state per opt-in + AT&C.
- `dial_tcp` / `dial_master_relay` / online-entry: assert on CONNECT.
- online-exit / `ATH` / `AT&F`/`ATZ` / relay disconnect (#3): drop on carrier
  loss. Make sure the **slave** drops DTR on relay-link-loss so the attached
  machine gets `NO CARRIER` via hardware too, not just the in-band result code.
- Guard every call behind the opt-in so the off path makes **zero** serialport
  modem-line calls.

### Testing
- Mostly **manual / hardware**: a USB-serial adapter with a loopback or a second
  adapter reading the line; or a scope/LED. `socat` PTYs do **not** faithfully
  carry modem-control lines, so they can't validate this.
- Unit-testable parts: the **decision logic** (given opt-in + AT&C state +
  connection state → desired DTR level) factored into a pure function; the
  config key parse/validate/roundtrip + 3-UI wiring; that the off path issues no
  line calls (via a trait seam / mock port if we add one).
- Update README "Limitations" (DCD now drivable via opt-in + wiring note) and
  the AT&C help text.

---

## 3. Other deferred items (lower priority)

- **#9 — Channel-open handshake / protocol version.** Advertise a small
  identity + protocol-version byte on relay channel open so a master/slave
  version mismatch fails cleanly with a clear message instead of a confusing
  desync. Natural home: the first bytes of the relay exec, or an SSH
  channel-open extension. Low risk, nice-to-have.
- **#10 — Observability.** Operator-visible relay status: a master view of
  "connected slaves / registered remote ports", clearer connect/lose log lines
  (some exist already), maybe a telnet status page. The logs from #14/#15 cover
  the basics now.
- **#11 — Through-relay interop tests.** Run the existing CCGMS / lrzsz interop
  *through* the relay hop to prove transfers survive it, plus the in-process
  fake-slave harness from §1. This is the CI-able piece that would retire the
  "transfers over relay are manual-only" gap (the data-transparency review flagged
  it).
- **raw transport (`relay_transport = raw`).** Skipped by decision — SSH is the
  adopted transport. The key is retained, hidden from UIs, and startup-warned if
  hand-set. Only build if a non-SSH path is ever wanted (would need its own
  port + auth/lockout).
- **Head-of-line blocking in `ssh.rs` `data()`.** Documented, **not reachable
  today** (one channel per connection: modem connect-per-call, console one
  connection per port). Only needs the per-channel mpsc pump fix *if* a future
  single-connection multi-channel design lands.

---

## 4. Suggested order & rough effort

1. **Manual two-instance smoke test** (§1) — *no code*, ~half a day to run
   through; highest confidence per hour. Do this first; it may surface real
   issues the unit tests can't.
2. **In-process relay transfer harness** (§1 complement / #11) — *small-medium*;
   makes transfers-over-relay CI-covered.
3. **Drive DCD** (§2) — *medium*; config key (×3 UIs) + line-drive hooks +
   manual hardware validation. Default-off keeps it safe to ship incrementally.
4. **#9 handshake / #10 observability** — *small each*; polish.
5. **#11 through-relay CCGMS/lrzsz** — *medium*; depends on the harness.
6. raw transport / head-of-line — only if a concrete need appears.

(0.6.3 is still held — none of this gates a release. When 0.6.3 does ship, run
the `versionchange.txt` checklist.)
