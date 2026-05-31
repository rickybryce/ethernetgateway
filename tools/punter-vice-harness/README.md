# Punter interop harness — gateway ↔ real NovaTerm (in VICE) via tcpser

A real-peer test for the gateway's Punter (C1) implementation. No Linux
program speaks Punter, so the peer is **NovaTerm** running on an emulated C64
(VICE), bridged to the gateway through **tcpser** (a virtual Hayes modem).

```
  VICE x64sc                tcpser                     ethernet-gateway
  NovaTerm + SwiftLink  ──►  ip232 :25232  ──telnet──►  telnet :2323
  ACIA @ $DE00               (Hayes modem,              (headless, verbose,
                              dial 1 = gateway)          transfer_dir = run/transfer)
```

tcpser auto-negotiates telnet + RFC 856 binary, and the gateway turns on IAC
escaping once telnet is negotiated (`xmodem_iac = telnet_negotiated`), so the
8-bit Punter blocks (full of `0xFF` and `0x0D`) pass through intact.

## What's committed vs. what you supply

Committed (ours): the `*.sh` scripts, `egateway.harness.conf` (config
template), `payloads/` (sample files), this README.

You supply (not in the repo):
- **The gateway binary** — `cargo build --release` in the repo root.
- **tcpser** — third-party, GPL. Build once:
  `git clone https://github.com/go4retro/tcpser && (cd tcpser && make)`,
  then set `TCPSER=/path/to/tcpser/tcpser` (or drop the build in `./tcpser/`).
- **A NovaTerm disk** — set `NOVATERM_DISK=/path/to/novaterm_9.6c.d64`
  (the scripts default to `~/Documents/NovaTerm/novaterm_9.6c.d64`).

VICE 3.7+ (`x64sc`) must be installed.

## Run it

```sh
./run-harness.sh
```

Starts the gateway and tcpser in the background and launches VICE in the
foreground (close VICE to stop everything). A runtime dir `run/` is created
(gitignored) holding the live config and `run/transfer/`. Watch the live trace:

```sh
tail -f gateway.log      # gateway side (verbose per-block Punter log)
tail -f tcpser.log       # modem/serial/IP trace (this is the wire)
```

You can also run the pieces separately (in order): `./start-gateway.sh`,
`./start-tcpser.sh`, `./start-vice.sh`.

## Drive the transfer in NovaTerm

1. **Interface:** in NovaTerm's setup, select **SwiftLink, $DE00, 2400 baud**.
   (For a `*-df00` NovaTerm disk: `ACIA_BASE=0xDF00 ./start-vice.sh` and pick
   `$DF00`.)
2. **Dial:** type `ATDT1` (a tcpser phonebook alias for the gateway). You
   should see `CONNECT` then the gateway's PETSCII welcome / main menu.
3. **Navigate** to **File Transfer → Punter**.
4. **Download (gateway → C64):** choose download, enter `HELLO.PRG` (a runnable
   `10 PRINT "PUNTER OK"`) or `TESTDATA.SEQ`, start NovaTerm's Punter receive.
   Then on the C64: `LOAD"HELLO.PRG",8` / `RUN` → prints `PUNTER OK`.
5. **Upload (C64 → gateway):** choose upload, enter a filename, send from
   NovaTerm via Punter. It lands in `run/transfer/`; verify with
   `xxd run/transfer/<name>`.

## Sample payloads (`payloads/`)

- `HELLO.PRG` — 22-byte C64 BASIC program; `RUN` prints `PUNTER OK`. Exercises
  the PRG file-type path (extension → PRG on download).
- `TESTDATA.SEQ` — short deterministic text; exercises the SEQ path and is easy
  to eyeball after a round trip.

## Troubleshooting

- **Garbled at the first `0xFF`:** IAC escaping didn't engage — toggle it with
  `X` in the gateway's transfer menu and retry.
- **Can't dial:** confirm tcpser is up (`ss -ltn | grep 25232`) and the VICE
  ACIA base matches the NovaTerm disk variant.
- **Speed:** start at 2400 baud; raise NovaTerm's SwiftLink baud and
  `-rsdev1baud` together once it works.

## When a discrepancy shows up

The `tcpser.log` trace is the wire. Any quirk it reveals (framing, timing,
file-type byte, end-off) can be reproduced as a regression test in
`src/punter.rs` `mod reference_interop` — the independent codec that already
validates the gateway against itself over TCP.

## Cleanup

`run-harness.sh` cleans up on exit. Otherwise: `./stop-harness.sh`.
