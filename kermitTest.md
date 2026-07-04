# Kermit ↔ QTerm (SC126) Interop — Troubleshooting Journal

**Status: UNRESOLVED.** Kermit transfers between the Ethernet Gateway and
**QTerm 4.3e** (binary `qtermh1.com`) on an **SC126 (Z80 / CP/M)** fail in
**both directions** with QTerm reporting **"Invalid packet type."**
**XMODEM works fine** to the same machine at the same baud (9600 8N1).

This file records what was tried, what was captured on the wire, and — most
importantly — what has been **ruled out**, so we don't re-run dead ends next
time. All code changes made during this investigation were **reverted**
(see "Code reverted" below); this is a notes-only artifact (untracked).

Author's framing (Ricky): the goal is reliable Kermit for **vintage clients
generally**, not moving one file. XMODEM-works is a workaround, not the goal.

---

## Test rig

- **Master gateway:** Raspberry Pi 5 at `192.168.1.178`, repo checkout at
  `~/ethernet-gateway`, release build in `target/release/`, web `/logs` on
  `:8080` (verbose per-packet Kermit trace), config
  `target/release/egateway.conf`.
- **Link:** SC126 wired by **direct serial cable** to the Pi's FTDI
  (`/dev/ttyUSB1` = `serial_b`, **modem mode**, **9600 8N1**, flow control
  none). The gateway's "modem mode" just emulates Hayes AT so QTerm can
  "dial"; there is no real modem in the path.
- **QTerm client:** `qtermh1.com`, source repo cloned at
  `/home/ricky/AltairRepos/qterm` (also `git.imzadi.de/acn/qterm`). Key
  source: `source/RECVK.Z` (receive state machine), `source/SENDK.Z` (send
  state machine), `source/KUTIL.Z` (`spack`/`rpack`/checksums), `KERMIT.I`
  (constants).
- **Wire capture:** `socat -x -v PTY,link=~/ttyKTAP … /dev/ttyUSB1 …`
  interposed between the gateway and the FTDI, logging both directions with
  timestamps to `~/serialtap.log`. `>` = gateway→SC126, `<` = SC126→gateway.
  (This tap machinery was torn down on revert; recreate it the same way.)

---

## Symptom

- **Download** (gateway = Kermit **sender**): QTerm shows "Invalid packet
  type" and aborts. Traces to `RECVK.Z:89` → `dbadp` (`KUTIL.Z:983`, string
  at `:985`) in the `rinit` (Send-Init-wait) state.
- **Upload** (QTerm = Kermit **sender**): also "Invalid packet type",
  immediately. Traces to `SENDK.Z:85` (`sireta`) → `dbadp`, i.e. QTerm's
  send-init loop rejects **the gateway's Send-Init ACK**.
- `dbadp` fires when `rpack` returns a **well-formed packet of an
  unexpected TYPE** (not the byte-0 timeout/checksum path, which NAKs). So in
  both directions QTerm is *parsing a valid-looking packet from us and
  finding the wrong type byte in it.*

---

## Wire captures (ground truth)

### Our Send-Init (download), byte-perfect
```
01 2d 20 53 | 7e 27 20 40 2d 23 59 31 7e 20 | 3d | 0d
SOH LEN SEQ 'S'   MAXL TIME NPAD PADC EOL QCTL QBIN CHKT REPT CAPAS  CHK  CR
```
- LEN `2d` = tochar(13) = SEQ+TYPE+DATA(10)+CHECK. SEQ `20`=0. TYPE `53`='S'.
- MAXL 94, TIME 7, NPAD 0, PADC NUL, EOL CR, QCTL '#', QBIN **'Y'**,
  CHKT **'1'**, REPT '~', CAPAS 0.
- **Checksum hand-verified correct** for type-1: sum(LEN..last-data)=797,
  `(797 + ((797&0xC0)>>6)) & 0x3F` = 29 → tochar = `0x3d` = '='. Matches.

### QTerm's own Send-Init (captured during upload) — the reference
```
01 30 20 53 | 7a 2c 20 40 2d 23 26 33 7e 20 20 20 20 | 31 | 0d 11
SOH LEN SEQ 'S'   MAXL TIME NPAD PADC EOL QCTL QBIN CHKT REPT  CAPAS…  CHK  CR XON
```
- LEN `30` = tochar(16) → **13 data bytes**.
- MAXL **90**, TIME **12**, NPAD 0, PADC NUL, EOL CR, QCTL '#',
  QBIN **'&'** (0x26 = its `MYHIBIT`), CHKT **'3'** (CRC-16), REPT '~',
  then **CAPAS + more (4 trailing bytes)**.
- Gateway parsed it cleanly: `flavor=G-Kermit`.
- **Every QTerm packet ends `<EOL=CR> <0x11=Ctrl-Q/XON>`** — its fixed
  packet trailer (`SENDK`/`spack` `KUTIL.Z:108-111`). The trailing `0x11`
  is normal, not an error signal.

### Gateway's Send-Init ACK (upload) that QTerm rejected
```
01 2d 20 59 | 7a 2c 20 40 2d 23 26 31 7e 20 | 54 | 0d
SOH LEN SEQ 'Y'   MAXL TIME NPAD PADC EOL QCTL QBIN CHKT REPT CAPAS  CHK  CR
```
- Valid `'Y'` ACK, seq 0, echoing negotiated params. QTerm's `SENDK` reads
  it and `dbadp`s.

### Differences between our S-Init and QTerm's (candidate leads for next time)
| Field | Ours | QTerm |
|------|------|-------|
| data-byte count | **10** (through CAPAS[0]) | **13** (full field set + trailing capas) |
| QBIN | 'Y' (willing) | '&' (will 8-bit-prefix with '&') |
| CHKT | 1 (config) → tried 3 | 3 |
| MAXL / TIME | 94 / 7 | 90 / 12 |

---

## What was RULED OUT (do not re-chase)

1. **Not a gateway framing/checksum bug.** Our S-Init and ACK are
   byte-perfect and pass a hand-computed type-1 check. Verified `KUTIL.Z`
   `rpack`/`spack` are self-consistent and use the *same* LEN/checksum
   conventions we do; per the source our packets *should* parse as 'S'/'Y'.
2. **Not premature-send / on-screen garbage.** Originally the gateway blasted
   the S-Init the instant the protocol was picked, before QTerm was in
   receive mode → garbage on screen. A "wait for the receiver's initiating
   NAK, then send" change **fixed the garbage** (confirmed on the wire:
   gateway now waits, sees QTerm's NAK `01 23 20 4e 33 0d`, then sends). But
   QTerm **still** `dbadp`s the correctly-timed S-Init. So timing-of-send was
   a *real but separate* bug; not the cause of "invalid packet type."
3. **Not block-check type.** Set gateway `kermit_block_check_type = 3` to
   match QTerm's proposal (3). Still fails.
4. **NOT UART overrun / line pacing.** Added a config-gated inter-byte send
   delay and tested at **10 ms/byte** (≈11 ms/byte on a 9600 line where the
   natural byte time is ~1 ms). **Still "invalid packet type."** A slow-UART
   burst-overrun theory would have been cured by this. It was not.
5. **Not baud rate (strongly implied).** **XMODEM works at this exact baud
   (9600 8N1).** So the byte path gateway→SC126 is clean and the SC126 keeps
   up at 9600. XMODEM survives any hypothetical drop via block retransmit;
   Kermit's Send-Init has no recovery — but pacing (#4) already removed the
   drop hypothesis anyway.
6. **Not line corruption in general.** QTerm's replies (NAK, its own S-Init)
   parse perfectly on our side, so the return path is clean; and XMODEM
   proves the forward path is clean.

**Net:** QTerm receives our correct bytes and its parser still reports the
wrong packet type — a genuine parse/behavior mismatch we could not explain
from the repo source, which says it should work.

---

## Leading hypotheses for next time (unproven)

- **`qtermh1.com` binary differs from the repo source** (patched/older/variant
  build). The source analysis says our packets parse; the binary disagrees.
  → Disassemble/inspect `qtermh1.com`'s actual `rpack`/`spack`/checksum, or
  confirm the exact QTerm version, rather than trusting `KUTIL.Z`.
- **Make our packets byte-structurally identical to QTerm's own.** QTerm can
  obviously parse its own S-Init format. Try emitting an S-Init with the
  **same field set QTerm uses**: 13 data bytes (full CAPAS/WINDO/MAXLX
  tail), **QBIN='&'**, CHKT=3, and matching MAXL/TIME — then see if QTerm
  accepts it. If yes, bisect which field it was.
- **A QTerm/8-bit or handshake setting.** QTerm proposes QBIN='&' (8-bit
  prefixing) and terminates packets with XON (0x11). Check whether QTerm is
  in an XON/XOFF-handshake or 8-bit mode that expects specific behavior from
  the sender.
- **Ricky's plan:** try a **lower baud rate with various QTerm settings** on
  the next hardware session. (Kept as a fallback even though #4/#5 argue
  against timing — cheap to try, and lets us vary QTerm settings at the same
  time.)

---

## Diagnostics that worked well (recreate these next time)

- **Verbose Kermit `/logs`** (`verbose = true`, web on `:8080`). During this
  session we *temporarily* added outgoing/incoming `wire=<hex>` dumps to
  `kermit.rs` (reverted). Re-add them if needed — they made the S-Init/ACK/NAK
  bytes visible without a tap.
- **socat serial tap** (see "Test rig"). Safe here because it's a direct
  cable into a polled UART — DCD doesn't gate data. Gives both directions +
  timing, and reveals bytes sent *before* the transfer (menu/prompt).

---

## Code reverted on 2026-07-04 (so we start clean next time)

All of the following were **backed out** (`git restore`) — the repo is back
at `dev` HEAD `310d335`:
- `kermit.rs`: verbose outgoing/incoming `wire=` hex dumps; `wait_for_receiver`
  param + `wait_for_initiating_nak` + 250 ms settle; `PacedWriter` inter-byte
  pacing adapter.
- `config.rs`: `kermit_wait_for_receiver` and `kermit_send_byte_delay_ms`
  keys (+ defaults/parse/write/setter/tests).
- `telnet.rs`: passing `kermit_wait_for_receiver` into the download.
- `relay/tests.rs`: matching call-site arg.

Two of these were genuinely good and worth re-doing when we return to this:
- **wait-for-receiver-NAK before Send-Init** (fixes the on-screen garbage;
  spec-friendly since real receivers NAK too). Was config-gated
  (`kermit_wait_for_receiver`, default on) and only wired into the
  interactive telnet download; all other `kermit_send` callers passed
  `false`. Reviewed clean (one deadline-accounting fix applied: give the
  Send-Init exchange a fresh negotiation deadline after the wait).
- **inter-byte send pacing** (`kermit_send_byte_delay_ms`, default 0, via a
  `PacedWriter` AsyncWrite adapter wrapping the writer once in
  `kermit_send_impl`/`kermit_receive_with_init`). Didn't fix QTerm, but is a
  legitimate knob for genuinely slow peers.

`.178` config restored to pre-session state (serial_b → `/dev/ttyUSB1`,
`block_check_type = 1`, no new keys); socat tap torn down.
