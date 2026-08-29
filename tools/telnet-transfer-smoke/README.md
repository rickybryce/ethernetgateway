# Transfers to and from the gateway, through its own telnet menu

The gates in `src/xmodem.rs`, `src/zmodem.rs` and `src/punter.rs` run our codec
against a real lrzsz or CCGMS peer. That proves the **protocol code**. It does
not touch the **product**: the main menu, the file picker, the protocol chooser,
the IAC-escaping toggle and the session's own byte path.

This drives all of that — logs in over telnet exactly as a user does, answers
terminal detection, picks Download or Upload, chooses the protocol, and then
hands the live socket to a real `sx`/`rx`/`sb`/`rb`/`sz`/`rz` (or the CCGMS
Punter reference) and compares the bytes.

## Requirements

- `python3`, `lrzsz`, a built gateway (`cargo build`; `GATEWAY_BIN` overrides)
- Punter additionally needs `~/claude/punter-ccgms-interop/` built — it is
  skipped with a note when absent.

## Run

```sh
./run.sh              # every direction of every protocol
./run.sh zmodem       # just one
PORT=2397 SIZE=16384 ./run.sh
TRACE_HANDOFF=1 ./run.sh xmodem     # log the wire, both directions
```

Exit 0 = every path matched byte for byte. Logs: `work/gateway.log` and
`work/ethernetgateway-data/ethernetgateway.log` (the config sets `verbose`).

## Last run (2026-08-29, v0.9.6)

**9 passed, 0 failed** — download and upload for XMODEM, XMODEM-1K (download),
YMODEM, ZMODEM and Punter, with a 4 KB payload carrying all 256 byte values.

## Three things this rig gets wrong if you let it

All three are the harness's business, not the gateway's, and each one first
presented as a product bug.

- **The preamble must be drained before the sender starts.** The gateway prints
  "Start transfer within N seconds" and an ESC-to-cancel line, then begins
  polling. A real terminal emulator has consumed that text by the time the user
  starts their transfer; this driver had not, so `sx` read a stray text byte as
  an early ACK and ran **one ACK ahead for the whole transfer**. At the end it
  took block 32's real ACK as the acknowledgement of its EOT, printed "Transfer
  complete", exited 0 — and never saw the gateway's Forsberg EOT-confirmation
  NAK. The gateway was correct throughout and the file was simply never written.
  A sender reporting success is not evidence that a transfer happened.

- **Punter is the exception: do not drain at all.** Its sender opens the
  handshake immediately after the preamble, so the same one-second drain ate the
  opening code. `settle(quiet=0.0)` for Punter, a full second for everyone else.

- **The CCGMS reference segfaults on a handshake timeout.** Measured: `ccgms-recv`
  with nothing sent to it exits `-11` on pipes *and* on sockets alike. So a
  drained handshake does not look like a timeout, it looks like a crash — which
  is how the drain above cost an afternoon.

## IAC escaping

The gateway sets IAC escaping from whether the client negotiated telnet options:
a real client (PuTTY, C-Kermit) gets `0xFF` doubled, a raw TCP client (netcat,
retro firmware) gets a transparent stream. This driver answers negotiation, so
it is treated as a real client and escaping comes on — and `lrzsz` does not
speak telnet. The driver presses `I` to turn it off, which is what a user in the
same position does, and exercises the toggle on the way past.

## Not covered here

Kermit, whose menu entry is server mode (`K`) rather than a picker choice — the
six C-Kermit gates in `src/kermit.rs` cover client and server both ways. And the
**peer-to-peer** case, where the gateway is a pipe between two devices rather
than an endpoint: that is `tools/peer-transfer-smoke/`.
