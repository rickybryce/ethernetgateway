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

**Status: complete (v0.4)** — a working terminal with all five port families,
settings that survive a restart, and XMODEM file transfer in both directions.

Verified in the gateway's CP/M emulator, each family against the matching
`cpm_emu_uart` profile: `AT` → `OK` and `ATI4` identifying the modem through
Z80 SIO/2 (`rc2014_1b`), 6850 ACIA (`altair_2sio1`), RomWBW HBIOS
(`hbios_1`) and CP/M AUX (`aux`); settings saved, EGT80 exited and re-run,
picking the saved port and filter mode back up; a deliberately corrupted
settings block falling back to defaults with a message; HBIOS refused with an
explanation on a machine with no `RST 8` vector; and the ASCII filter reducing a
colour ANSI menu to clean text (18 escape sequences in, 0 out, all text intact).

**Transfers** were tested end to end against the gateway's own File Transfer
menu, dialled from inside EGT80: a file uploaded from CP/M arrived byte-identical
(once the trailing `^Z` record padding is trimmed), and a file downloaded to CP/M
matched the original for its whole length with `^Z` padding to the block
boundary — which is what CP/M's record granularity means for XMODEM.

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

| Driver | Selection | Verified |
|--------|-----------|----------|
| Z80 SIO/2 | four channels (0x80/0x82/0x84/0x86) or any address | yes — `rc2014_1b` |
| 6850 ACIA | 88-2SIO 0x10 / 0x12, or any address | yes — `altair_2sio1` |
| RomWBW HBIOS | unit 0–3, via `RST 8` | yes — `hbios_1` |
| Z180 ASCI | channel 0/1, internal I/O base settable | **not here** — needs real iron |
| CP/M BDOS AUX | no parameters (funcs 3/4) | yes — `aux` |

Two of these carry caveats the menus and help state plainly:

**Z180 ASCI** can only be verified on real hardware — the emulator's Z80 core
does not implement `IN0`/`OUT0`, which is exactly why the QTERM `h` builds needed
HBIOS. Those instructions can't be encoded by a Z80 assembler either, so the
routines lay them down as `DB ED,38,port` / `DB ED,39,port` and patch the operand
byte when the port is selected — the same self-patching the vectors use, one level
down. Since no test here can reach that code, it was hardened by reading rather
than by running:

- **The CPU is checked before the family is accepted.** The ASCI is inside the
  Z180, so on a plain Z80 those opcodes are undefined and the driver would read
  whatever they leave behind. `MLT` tells the two apart — on a Z180 `ED 4C`
  multiplies `B` by `C`, on a Z80 it is a two-byte no-op — so `2 × 2 = 4`
  identifies the processor with no side effects either way. This half *is*
  testable here: the emulator has no `MLT`, and EGT80 duly refuses ASCI and
  keeps the previous port.
- **Latched receive errors are cleared.** `STAT` carries OVRN/PE/FE, and on the
  Z180 an overrun stops the receiver until the error is reset via CNTLA's `EFR`
  bit — so one burst of line noise would otherwise leave the port deaf for the
  rest of the session with nothing to show why. The status routine now resets the
  flags (read-modify-write, so baud and framing are untouched) and discards the
  byte that arrived with the error: a protocol is better served by a missing byte
  it will ask for again than by a corrupt one it must detect.
- **The I/O base has its own setting.** It used to share `PBASE` with the port
  address of the SIO/ACIA families, so selecting ASCI inherited whatever the last
  family used and addressed registers that don't exist. It is now a separate byte,
  range-checked so `base+9` can't run off the end of the I/O space.

Register offsets and status bits were taken from the Z180 register documentation
in RomWBW's own ASCI driver (facts, not code): `CNTLA` at base+0, `STAT` +4,
`TDR` +6, `RDR` +8, channel 1 the same +1; `RDRF` bit 7, `TDRE` bit 1.

**CP/M AUX** has no status call in CP/M 2.2 — the operating system simply offers
none. The driver therefore reads one byte ahead and holds it, treating the `^Z`
the gateway returns for "nothing waiting" as not-ready. Two consequences, both
documented in the program's own help: this family cannot receive a literal `^Z`,
and on real CP/M 2.2 hardware (where the read blocks) the status call waits
rather than returning. It works well against the gateway; on real hardware prefer
the family that matches the serial chip.

**RomWBW HBIOS** is checked before it is accepted: a RomWBW system puts a `JP` at
the `RST 8` vector, so the absence of one is a reliable "not a RomWBW machine".
Better to say so than to hang on the first keystroke — on a bare CP/M machine
that instruction lands in unused memory.

**Console I/O is chosen once at startup.** BDOS 12 reports the version: on
CP/M 2.2 the BIOS console vectors are called directly (far cheaper per byte than
a BDOS call, and the terminal loop is the hot path); on CP/M 3, BDOS 6. One test
at init, vectors thereafter — no per-byte version checks.

**Settings live in the `.COM`.** The block sits at a fixed, 128-byte-aligned
address (`0180H`, file record 1) that the first instruction jumps over, so
saving rewrites one record holding settings and nothing else — no risk of writing
a half-open FCB or a live variable back into the image. `V` at the menu writes
that record with a random-record write, pointing the DMA address straight at the
image so nothing is copied. CP/M never tells a program its own name, so
`EGT80.COM` is hard-coded in one FCB: a renamed or off-drive copy makes the save
fail with a message instead of writing to the wrong file. At startup the block is
validated — signature plus a range check on every field — and anything wrong
falls back to defaults with a message, so a damaged copy still starts usable.

**ANSI or ASCII** selects what happens to the byte stream: ANSI passes escape
sequences through to your terminal, ASCII strips CSI sequences and
non-printables so a dumb console isn't littered.

Buffers are deliberately fixed (256-byte receive ring, one 128-byte XMODEM
sector, one 128-byte disk record) rather than "whatever memory is free". A
terminal does not need to claim the TPA, and a known size is a known worst case
on a machine that may only have 32 KB of it.

## Transfers

Plain XMODEM: 128-byte blocks, CRC-16 with the checksum fallback an older peer
may insist on, and one buffer that serves as both the protocol block and the
CP/M record — they are the same 128 bytes, which is why the two fit together so
neatly. The CRC is computed a bit at a time rather than from a 512-byte table: at
9600 baud a byte takes a millisecond and its CRC takes microseconds, so the table
would cost more memory than the whole transfer routine and buy nothing.

`U` uploads, `D` downloads, from the main menu **or** from the terminal-mode menu
key — leaving terminal mode doesn't hang up, so the usual sequence is to dial,
tell the far end what you want, then press the menu key and `U`/`D`. A dot marks
each block and a `?` each retry.

## Reviewing the assembly found three more

A quality/stability pass over the source after phase 3 turned up one real bug
and two things that were bounded wrongly. All three are the sort that a clean
line never shows you:

**A resent block was rejected instead of acknowledged.** The duplicate-block
check compared the block number, then did `INC (HL)` before branching — and
`INC (HL)` sets the flags itself, so the branch tested the increment rather than
the comparison. Every lost ACK (the exact case XMODEM's duplicate handling
exists for) therefore turned into a NAK, and a noisy line would eventually fail
the transfer rather than recovering. Proved with a deliberately lossy sender that
repeats block 1: the old binary answers `NAK`, the fixed one `ACK`, and the file
is two blocks, not three.

**Noise was charged against the retry budget.** The far end usually prints
something like "start your transfer now", and every character of it arrived where
a protocol byte was expected. Spending a retry per character failed the transfer
before it began. Noise now has its own generous budget, separate from the retries,
refreshed on progress; the sender also purges once the mode is agreed, and treats
an unexpected byte as noise to wait through rather than as a reason to resend.

**The stack could overflow into the transfer buffer.** 32 levels was ample for
this program's own depth, but a real CP/M BIOS may use the *caller's* stack, and
the reserve sat directly above `XBUF` — so an overflow would quietly corrupt the
block being transferred instead of failing where it happened. Now 64 levels.

## Three things learned the hard way

Both are worth knowing before touching this code.

**A CP/M console call may destroy any register.** `CONOUT` is not required to
preserve HL, and our own emulator's BIOS return even mirrors its result into `L`.
An early `PSTR` printed its first character forever, because the console call had
left the string pointer pointing at itself. `CST`/`CIN`/`COUT` therefore preserve
BC, DE and HL. The terminal loop is kept cheap by holding no state across calls
at all — not by shaving pushes off the routine every screen depends on.

**Register discipline is the whole game in a counted loop.** The XMODEM code
counts 128 bytes in `B` while walking the buffer with `HL`, and calls routines
that need registers of their own. Three separate bugs came from exactly that:
`PSEND`, `XGETB` and `XACCUM` each used `BC` as a private counter, and `XACCUM`
also used `HL`. Worse, `PSEND` restored `BC` *before* its final jump to the
output vector — and the port driver sets `B` to address the port, so the restore
was undone on the way out. Every one of them produced the same spectacular
symptom: the send loop walked off the end of the buffer and shovelled a megabyte
of RAM at the wire. `PSEND`/`XGETB` now preserve `BC`, `XACCUM` preserves `BC`
and `HL`, and each restores *after* its last vector call. The port and console
vectors themselves preserve nothing — that is stated at the top of the source.

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
