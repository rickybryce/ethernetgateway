# Peer-dial live smoke test

End-to-end validation of the **peer-dial** feature (`ATD <Port>@<IP>` /
Serial Gateway pick, commit `363619d`) using real serial device semantics —
two `socat` PTY pairs wired to a headless gateway's Port A and Port B, driven
by `pyserial`. This exercises the cross-thread ring/answer/bridge code that
unit tests can't reach (two live port threads talking to each other).

## Requirements
- `socat`, `python3` + `pyserial` (`pip install pyserial`).
- A built gateway binary. Default: `/home/ricky/xmodem/target/debug/ethernetgateway`
  (override with `GATEWAY_BIN=/path/to/ethernetgateway`). Build with `cargo build`.

## Run

```sh
./run.sh            # modem <-> modem
./run.sh console    # modem  -> console (telnet-serial) target
```

`run.sh` creates two PTY pairs under `work/` (`ttyA_gw`/`ttyA_dev`,
`ttyB_gw`/`ttyB_dev`), writes a headless `work/egateway.conf`
(`enable_console=false`, servers off, `allow_peer_dial=true`, Port A modem,
Port B modem or console), launches the gateway, and runs `driver.py` against
the two device ends. Exit code 0 = all checks passed. The gateway log is at
`work/gateway.log`.

## What it checks

**`modem`** (Port A modem, Port B modem):
- Self-dial (`ATD A@127.0.0.1`) is refused → `NO CARRIER`.
- `ATD B@127.0.0.1`: Port B **rings** (device B sees `RING`), device B answers
  with `ATA`, both ends get `CONNECT`.
- Transparent data both ways (`A->B`, `B->A`).
- `+++` escape → `OK`; `ATH` → `OK`, and the far end (B) sees `NO CARRIER`
  (the bridge tears down cleanly — no leak).
- Unanswered call (B `ATS0=0`, short caller `ATS7=4`) → `NO ANSWER` (ATX4).

**`console`** (Port A modem, Port B telnet-serial):
- `ATD B@127.0.0.1` connects **directly** (no ring).
- Transparent data both ways between A's modem device and B's console device.

## Last run (2026-07-01)

`modem`: **10/10 PASS**. `console`: **3/3 PASS**. Peer-dial Phase 1 (local)
validated live end to end.

## Notes / scope
- `127.0.0.1` resolves as *local* (`host_is_local`), so both scenarios exercise
  the same-gateway bridge. The gateway's own LAN IP works identically.
- Only the **`ATD` (serial) entry point** is exercised here. The **Serial
  Gateway menu** entry (picking a modem port, which rings it the same way) needs
  a telnet client and is a manual check — enable `telnet_enabled` and point a
  telnet session at the menu.
- Cross-gateway peer-dial (`<Port>@<other-ip>` over the relay) is Phase 2, not
  built — a remote address returns `NO CARRIER`.
- PTYs don't carry modem-control lines, so this does not exercise DCD/`drive_carrier`
  (see `tools/dcd-validate/` for that).
