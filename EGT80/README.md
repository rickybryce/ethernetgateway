# EGT80 — "Ethernet Gateway Terminal"

A CP/M terminal program for the Ethernet Gateway, written in Z80 assembly for
**real hardware** as much as for the gateway's CP/M emulator.

Every period terminal we tested was built for one machine's serial port. QTERM
ships a separate binary per port (`qterm82` for an RC2014 SIO/2 at 0x82,
`qterm84` for 0x84, `qtermh1`/`qtermh2` for RomWBW HBIOS units); IMP8 is an
Altair 2SIO build; KERCPM22's generic overlay has no serial driver at all. Get
the pairing wrong and the program is simply silent.

EGT80 asks instead. One `EGT80.COM` presents a menu, you pick the port, and it
remembers. It runs on CP/M 2.2 and CP/M 3.

**Status: phase 1** — working terminal. Title screen, main menu, settings, help,
terminal mode with an escape-key menu, the ANSI/ASCII filter, and the Z80 SIO/2
driver. Verified in the gateway's CP/M emulator: `AT` → `OK` and
`ATDT host:port` → `CONNECT` through the SIO driver at 0x82/0x83, and the ASCII
filter reduces a colour ANSI menu to clean readable text (18 escape sequences in,
0 out, all text intact).

The remaining four drivers (6850 ACIA, RomWBW HBIOS, Z180 ASCI, BDOS AUX), the
saved settings patch area, and XMODEM follow in phases 2–3.

## Building

```sh
make            # assemble EGT80.COM with SLR Z80ASM
make check      # prove the source also assembles with M80 and ZMAC
```

The build runs the **real period assembler**, SLR `Z80ASM.COM`, under
[`zxcc`](http://www.seasip.info/Unix/Zxcc/) — the CP/M-on-Unix runner RomWBW
builds itself with. That is the point: a modern cross-assembler would cheerfully
accept syntax SLR80 rejects, and this program is meant to be rebuildable on the
target machine. If `make` passes here, the source assembles on real CP/M.

Both the assemblers and `zxcc` come from a RomWBW distribution, which is expected
at `~/RomWBW`; override with `make CPMBIN=... ZXCCSRC=...` if yours lives
elsewhere. `zxcc` is compiled on first build into `tools/` (gitignored, third-
party). `EGT80.COM` **is** committed, so you can copy it to a CP/M drive without
owning any of this.

`make check` is a real gate, not decoration: each assembler must both report a
clean assembly *and* produce output. Both conditions are needed — M80 reads an
LF-only file as one enormous line, assembles almost nothing, and exits
successfully, which an early version of this Makefile happily reported as "OK".
Every recipe therefore converts the source to CRLF when staging it. (`ZSM.COM`,
also in `bin80`, is not used as a gate: no command-line form tried under zxcc is
accepted, and guessing at it would prove nothing about our source.)

## Editing rules

The source must keep assembling with SLR Z80ASM, M80 and ZMAC. That constrains
style, so please keep to it:

- No macros, no conditional assembly, no local/temporary labels.
- Upper-case Zilog mnemonics, one statement per line, ≤ 80 columns.
- Labels unique within their first **six** characters.
- Only `EQU`, `ORG`, `DB`, `DW`, `DS`, `END`.
- Expressions limited to `+ - * / AND OR SHL SHR` and `$`.
- Plain 7-bit ASCII; no tabs inside quoted strings.

`make check` after any edit. The gates exist because these limits are easy to
break by accident and impossible to notice in a modern assembler.

## Design

**Ports are chosen at run time.** Four vectors in RAM — RX-ready, get-byte,
TX-ready, send-byte — are rewritten when you pick a port, the same shape QTERM's
build-time overlays use. Cost is one `CALL` per byte with no dispatch logic, and
each driver body is around 40 bytes, which is what makes a menu of ports
affordable on a Z80.

Planned drivers:

| Driver | Ports | Testable in the emulator? |
|--------|-------|---------------------------|
| Z80 SIO/2 **(done)** | 0x80–0x87, channel selectable | yes — `rc2014_1a`…`rc2014_2b` |
| 6850 ACIA | 0x10/0x12 (88-2SIO) and user base | yes — `altair_2sio1`/`2sio2` |
| RomWBW HBIOS | `RST 8`, units 0–3 | yes — `hbios_1`/`hbios_2` |
| Z180 ASCI | SC126 channels 0/1, `IN0`/`OUT0` | **no** — real hardware only |
| CP/M BDOS AUX | funcs 3/4 | yes — `aux` |

The ASCI driver is written from the Z180 register spec and can only be verified
on real iron: the emulator's Z80 core does not implement `IN0`/`OUT0`, which is
exactly why the QTERM `h` builds needed HBIOS. The BDOS AUX driver carries its
own caveat — CP/M 2.2 has no AUX status call, so a read can block on real 2.2
hardware; it is the portable fallback, not the first choice.

**Console I/O is chosen once at startup.** BDOS 12 reports the version: on
CP/M 2.2 the BIOS console vectors are called directly (far cheaper per byte than
a BDOS call, and the terminal loop is the hot path); on CP/M 3, BDOS 6. One test
at init, vectors thereafter — no per-byte version checks.

**Settings live in the `.COM`.** A patch area with a signature holds the port
choice, base address, ANSI-vs-ASCII, echo, CR/LF and XMODEM options; "save
settings" rewrites the image. No second file to carry around, and a corrupt or
missing signature falls back to defaults so a copied `.COM` never boots into
garbage.

**ANSI or ASCII** selects what happens to the byte stream: ANSI passes escape
sequences through to your terminal, ASCII strips CSI sequences and
non-printables so a dumb console isn't littered.

Buffers are deliberately fixed (256-byte receive ring, one 128-byte XMODEM
sector, one 128-byte disk record) rather than "whatever memory is free". A
terminal does not need to claim the TPA, and a known size is a known worst case
on a machine that may only have 32 KB of it.

## Two things learned the hard way

Both are worth knowing before touching this code.

**A CP/M console call may destroy any register.** `CONOUT` is not required to
preserve HL, and our own emulator's BIOS return even mirrors its result into `L`.
An early `PSTR` printed its first character forever, because the console call had
left the string pointer pointing at itself. `CST`/`CIN`/`COUT` therefore preserve
BC, DE and HL. The terminal loop is kept cheap by holding no state across calls
at all — not by shaving pushes off the routine every screen depends on.

**A send wait must be bounded.** If the selected port is wrong, the transmitter
is never ready — an absent chip answers identically forever — so an unbounded
wait hangs at the first keystroke with the menu key dead, which is precisely when
the user needs the menu. `PSEND` gives up after four passes of a 65536-poll loop
(several seconds on a 4 MHz Z80, generous enough that hardware flow control
holding a *working* port off cannot trigger it), and terminal mode then names the
port and says what it means. That turns the classic silent hang into a diagnosis.

## Testing

In the gateway's CP/M emulator, put `EGT80.COM` on a drive under `transfer_dir`
(e.g. `transfer/CPM/A/`), set `cpm_emu_uart` to the profile matching the driver
under test, then drive a telnet session with
`~/claude/gateway-telnet-driver/drive.py`. On real hardware, copy `EGT80.COM`
across with XMODEM — from QTERM use `xk`, never Kermit, which is text-only there
and truncates binaries at the first `^Z`.
