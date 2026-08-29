# Peer-to-peer file transfer smoke test

The sibling harness, `tools/peer-dial-smoke/`, proves two modem ports can be
bridged and that bytes cross intact. This one asks the harder question that
the gateway's own notes raise: **a bridge is a pipe, and a pipe carrying a
file transfer is where any translation becomes corruption.**

So it dials Port A to Port B, answers the call, and then hands the two PTY
device ends to a *real* sender and a *real* receiver — the gateway is nothing
but the wire between them — and compares the file byte for byte.

## Requirements

- `socat`, `python3` + `pyserial`
- `lrzsz` (`sx`/`rx`, `sb`/`rb`, `sz`/`rz`) and C-Kermit (`kermit`)
- A built gateway binary (`cargo build`); `GATEWAY_BIN` overrides the path.

## Run

```sh
./run.sh              # every protocol
./run.sh zmodem       # just one
BAUD=115200 SIZE=65536 ./run.sh
```

Exit 0 = every protocol round-tripped byte for byte. The gateway log is at
`work/gateway.log`.

## The payload

Not a text file: every one of the 256 byte values, plus long runs of the bytes
that break an escaping layer — `CAN` (0x18), `XON`/`XOFF` (0x11/0x13), `SUB`
(0x1A), CR, LF, `0xFF`, `DEL` and `BS`. A pipe that is subtly rewriting one
byte value has nowhere to hide.

## What it checks

| Protocol | Sender | Receiver |
|---|---|---|
| XMODEM | `sx` | `rx` |
| XMODEM-1K | `sx -k` | `rx` |
| YMODEM | `sb` | `rb` |
| ZMODEM | `sz` | `rz` |
| Kermit | `kermit -l … -s` | `kermit -l … -r` |

## Last run (2026-08-29, v0.9.6)

**5 passed, 0 failed** at 9600 baud with an 8 KB payload.

## Two results that are the tools', not the gateway's

Both were established by running the identical pair over a **bare socat PTY
pair with no gateway in the circuit at all** — the control that separates
"our pipe corrupts" from "these two programs behave like this together".

- **`sb` does not exit.** It reports `Retry 0: Timeout on sector ACK` after the
  file, because `rb` leaves as soon as the file is complete and nothing
  acknowledges the end-of-batch null header. Measured with no gateway: `sb`
  rc=124, same message, bytes still identical. The file comparison is the
  verdict, so the driver marks YMODEM's sender as one that may linger.
- **`rz` does not exit** either: it is reading a PTY the gateway holds open, so
  it never sees EOF. Harness business, not protocol business.

## Punter is not covered here, and why

There is no Linux Punter client. The CCGMS reference in
`~/claude/punter-ccgms-interop/` stands in for one everywhere else in this
project, but it **segfaults when run against itself** (measured 2026-08-29:
both ends rc=-11 over a bare PTY pair, no gateway present) — those binaries
were only ever built to face our Rust implementation, one side at a time.

That is a gap in the oracle rather than a result about the bridge, and the
bridge does not know one protocol from another: the five above cross it byte
for byte carrying a payload with all 256 values in it.

## Scope

`127.0.0.1` resolves as *local*, so this is the same-gateway bridge, as with
peer-dial-smoke. The **console-mode** erase-key fold (`serial_*_backspace`)
lives in `run_console_bridge`, which is the telnet-session-to-wire path and is
**not** what peer-dial builds — so nothing here exercises it.
