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

**Status: complete (v0.7)** — a working terminal with all five port families,
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

**And then on real hardware**, which is where it counts: an SC126 running RomWBW,
EGT80 on HBIOS unit 1, over the gateway's serial modem at 9600 8N1. A 8,704-byte
`.COM` and a 104,960-byte `.DAT` — 820 XMODEM blocks — round-tripped
**byte-identical in both directions**. That pairing is also what found the
register-discipline bug documented below: before it, those same files arrived the
correct length and entirely zero, with no error reported at either end. Emulator
tests could not see it, because our HBIOS preserved a register that real firmware
does not.

## Getting it

Nothing to install: EGT80 is compiled into the gateway binary and placed on CP/M
drive A: when the emulator first creates its drive folders. `DIR` shows it, and
typing `EGT80` runs it.

That copy is **never overwritten** afterwards. EGT80 stores its settings inside
its own `.COM`, so replacing the file on each launch would silently discard the
port you chose — and you may be deliberately running an older or locally-modified
build. Delete it if you want the shipped copy back on the next launch.

Each release archive also carries `EGT80.COM` as a loose file: that is the copy to
send to real CP/M hardware over XMODEM (from QTERM use `xk`, never Kermit — it is
text-only there and truncates binaries at the first `^Z`).

The quickest proof it works needs nothing outside the gateway: run `EGT80`, press
`T` for terminal mode, and dial the gateway itself with
`ATDT ethernetgateway` — the menu answers over the virtual modem, so a
successful `CONNECT` tests the port, the modem and the terminal in one go.

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

**CI cannot rebuild this.** Assembling needs SLR's `Z80ASM.COM` and `zxcc`, and
neither is in the repository — the assembler is third-party software we do not
vendor. So `EGT80.COM` is a committed artifact, and the risk is drift: a source
edit whose binary was never rebuilt. Four unit tests in `src/telnet/cpm_emu.rs`
close that gap with no tooling at all — they check the bundled binary is
a whole number of 128-byte records, starts with the `JP` over the patch area,
carries the `EGT80CFG` signature at file offset `0x80` (where the save routine
rewrites record 1), and contains the version string that `EGT80.Z80` declares.
The version check catches the realistic mistake of bumping the version without
rebuilding.

The fourth, `test_bundled_egt80_matches_pinned_hash`, covers what the other
three cannot: a code change made *without* touching the version. It asserts an
explicit `sha256` of the committed binary, so the bytes users run cannot change
unless someone edits the hash in the same commit — which puts it in front of a
reviewer. **So after any legitimate rebuild:**

```sh
make && make check          # the real gate — three assemblers
sha256sum EGT80.COM         # paste into PINNED in that test
```

The hash pins the artifact, not its correspondence to the source; only `make`
proves that, so run it before a release cut.

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
| Altair 88-SIO | 0x00/0x01, or any address | yes — `altair_sio` |
| Z180 ASCI | channel 0/1, internal I/O base (asked of the firmware) | **not here** — needs real iron |
| CP/M BDOS AUX | no parameters (funcs 3/4) | yes — `aux` |

**The menu names machines, not chips.** Choosing a port used to mean choosing a
chip family, which is a question only someone who knows their board can answer.
The top level now reads: the gateway's own emulated port (the default, so EGT80
and the gateway work together untouched), RomWBW firmware (any such machine), the
Altair 88-2SIO, the Altair 88-SIO, "other hardware" — which is the old chip list,
one level down, with the free-form address prompts intact — and the CP/M `AUX:`
device. A line above the list says which key follows from what the program can
actually determine about the machine: RomWBW detected, a Z180 without it, or
neither.

The **88-SIO** is a separate item rather than an address on the 6850 screen
because it is a different board: it reports ready by pulling a bit *low*, so an
88-2SIO driver pointed at it reads every test inverted and the port looks
permanently busy and permanently empty at once. The addresses were never the only
difference.

Two of these carry caveats the menus and help state plainly:

**Z180 ASCI** can only be verified on real hardware — the emulator's Z80 core
does not implement `IN0`/`OUT0`, which is exactly why the QTERM `h` builds needed
HBIOS. Those instructions can't be encoded by a Z80 assembler either, so the
routines lay them down as `DB ED,38,port` / `DB ED,39,port` and patch the operand
byte when the port is selected — the same self-patching the vectors use, one level
down. Since no test here can reach that code, it was hardened by reading rather
than by running:

- **The firmware is asked, rather than guessed at.** On a RomWBW machine
  `CIODEVICE` reports each character unit's device type and base I/O address, and
  `CIOCNT` how many there are. The HBIOS unit prompt therefore *lists what is
  really there* — `1  ASCI    base C0` — instead of offering a bare `0-3`, and
  the ASCI menu's `R` takes the base from that answer instead of assuming one.
  This matters because `C0` is only *mostly* right: RomWBW puts the Z180 block at
  `C0` on the Small Computer, RC2014-Z180, SZ180, GMZ180, DYNO and EPITX
  platforms but at `40` on the N8, MK4, N8PC and RPH ones. The reported base
  belongs to a physical channel, and channel 1 sits one address above channel 0
  (`ASCI1_BASE = Z180_BASE + 1`), so the channel number is subtracted to recover
  the block base — the number this program adds the channel back to. Off by one
  there would put every register one byte out.
- **The internal I/O base is offered by name, not just as hex.** The Z180's
  serial registers live inside the CPU and the whole internal register block is
  relocatable — the ICR decides where. RomWBW moves it to `C0` on Small Computer
  Central boards (`cfg_SCZ180.asm`), so EGT80's default of `00` addresses nothing
  there and the symptom is this family's usual silence. The ASCI menu therefore
  offers `C0` as a labelled choice: knowing that number is knowledge about
  someone else's firmware, not something a user should have to look up to type
  into a hex prompt. Reaching the same port through HBIOS avoids the question
  altogether, which is why that is the recommended path on a RomWBW machine.
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

## Line settings, and the honest answer about baud rates

Settings → `B` sets speed and framing. What that can *do* depends entirely on
the family, and the screen says which case you are in rather than offering a knob
that goes nowhere:

| Family | What EGT80 can set |
|--------|--------------------|
| RomWBW HBIOS | speed **and** framing, in one `CIOINIT` firmware call |
| Z180 ASCI | speed and framing — the rate genuinely lives in CNTLA/CNTLB |
| 6850 ACIA | framing, and the rate only as the board clock ÷1, ÷16 or ÷64 |
| Z80 SIO/2 | **nothing** — see below |
| CP/M AUX: | **nothing** — the OS owns the port |

A **Z80 SIO/2 has no baud rate generator at all**: the bit rate arrives on a
clock pin from the board or its CTC. Only framing is inside the chip, and its
registers are *write-only*, so EGT80 cannot even read what the ROM chose — it
would be reprogramming from a guess, with a silent port as the prize. So it
declines and explains, which is more useful than a rate field that lies.

And against the gateway's own virtual modem **none of it matters in either
direction**: a TCP connection has no bit rate, so the emulated UART accepts
line-configuration writes and ignores them. Nothing you set here can break the
gateway link.

Hence the default: **EGT80 programs nothing at all** unless you press `A`. The
port keeps whatever the ROM, the firmware or the OS set up — the arrangement that
already works on the machine — and `R` returns to that state. `A` also makes the
setting stick: it is re-applied whenever the port is selected, including at
startup, so it is a setting rather than a one-off command.

Two encodings are worth recording because they are easy to get wrong:

- **HBIOS** takes a 16-bit line-characteristics word whose baud field is not a
  rate but an exponent pair — the published rule is `V = 75 × 2^X × 3^Y` with the
  bits laid out `YXXXX`, which is why the table jumps from 9 (38400) to 24
  (57600). `RATEHB` holds the codes so that arithmetic is done once. RTS and DTR
  are asserted deliberately: clearing DTR on a real modem drops the call.
  Verified end to end — after applying 19200 7E2, a `CIOQUERY` probe run in the
  emulator read back `289E`, which decodes as exactly that plus RTS+DTR.
- **The 6850 has only eight framing combinations**, so 7 data bits without parity,
  and 8 data bits with parity and two stop bits, do not exist. Those are refused
  by name rather than rounded to the nearest — a framing mismatch shows up as
  garbage characters, which is the symptom the user came to this screen to fix.

The Z180 rate table assumes the 18.432 MHz crystal Z180 boards use precisely
because it divides to the standard rates (an SC126 has one); the screen prints
that assumption next to the rate. Both ASCI registers are read-modify-write, since
CNTLA carries the receiver and transmitter enables and clearing either would
silence the port being configured. One caveat is in the silicon: CNTLB bit 5 reads
back as the CTS input but writes the prescaler select, so it can never be
preserved by a read and is always taken from the table. **Reasoned, not run** —
like the rest of the ASCI driver.

## Getting unstuck

Two things exist because the alternative was confusing rather than because the
code needed them:

**`D` on the port screen** selects the default — Z80 SIO/2 board 1 channel B,
82/83 — which is also the gateway's default virtual-modem port. It resets the
whole port group, so no leftover from another family can be left pointing
somewhere odd. The screen also names the port currently in force, since "which
port am I on?" is why most people open that menu. A Rust test fails the build if
this default and the gateway's ever drift apart.

**The screen is cleared and the banner redrawn** before the main menu and on
entering terminal mode, so it is always clear which program you are talking to
once a remote system has filled the screen. There is no portable clear on CP/M —
the terminal is not the computer's and the BIOS offers nothing — so Settings → `C`
picks the dialect: the ANSI `ESC [ 2 J` (**the default** — what the terminals
people actually sit at understand, whether a terminal emulator over USB serial or
the gateway's own ANSI clients), the ADM-3A's `^Z` for a period terminal or a
PETSCII C64 through the gateway (whose translation re-renders it for the client),
or off for a printing terminal, where clearing the screen means feeding paper.
The default was `^Z` at first, on the reasoning that its failure mode is "nothing
happens" rather than `[2J` printed as litter; it moved to ANSI because "nothing
happens" is what a modern terminal on real hardware actually got. Clearing the screen also
means a message can be wiped before it is read, so the places where a message is
the only feedback — a damaged settings block, a save, a refused port family —
now pause for a keypress.

**The port entry points save registers; the drivers are not all alike.** Three of
the five drivers touch only `A` and `BC`, so it appeared safe to hold a buffer
pointer in `HL` across a port call — and for those three it was. HBIOS is an
`RST 8` into RomWBW's firmware and `AUX:` is a BDOS call, and neither preserves
anything beyond its documented returns. Both XMODEM loops walk the buffer with
`HL` across those calls, which corrupted every transfer on those two families
*without reporting an error*: the byte sent and the byte folded into the CRC came
from the same wandering pointer, so they agreed and the far end accepted a file of
the right length full of the wrong bytes. `PST`/`PIN`/`POST`/`POUT` now save
`BC`/`DE`/`HL`, as `CST`/`CIN` always did. It was invisible in the emulator
because our HBIOS preserved `HL`; it no longer does, for exactly this reason.

**ASCII is the shipped default; ANSI is opt-in.** The two failure modes are not
symmetric. An ANSI terminal shows plain ASCII perfectly, so defaulting to ASCII
costs that user only colour, which Settings → `A` turns on. A plain or PETSCII
terminal shows ANSI as litter — escape sequences printed as text, over every
screen — so defaulting to ANSI costs *that* user a program they cannot read well
enough to find the setting that would fix it. The default therefore goes to the
reading that is merely plainer rather than the one that is broken. (Note the
clear-screen dialect is a separate setting and still defaults to the ANSI
`ESC [ 2 J` — see below.)

**The terminal-mode menu deliberately does not offer Settings.** The menu key
gets you `E)xit`, `H)elp`, `U)pload` and `D)ownload`, and that is all. Settings
used to be on it and did not work: the settings screen drew, but keystrokes were
still going to the remote, so nothing could be selected and the way out was not
obvious. Transfers belong on this menu because the whole point is to dial, ask
the far end for a file, and fetch it without dropping the line. Settings do not:
they are a sit-down task, and `E` returns to the main menu where `S` works
properly. Removing the option is better than a settings screen that ignores you.

**The menus are coloured, and switch themselves off.** Headings are cyan,
labels cyan with amber values, and the key letter in every menu line is
highlighted. Colour is governed by the ANSI/ASCII setting that already exists (which now
defaults to ASCII, so colour is off until asked for),
because that setting means precisely the thing colour depends on — whether this
terminal understands escape sequences. In ASCII mode not one escape byte is
emitted, so a printing terminal or a PETSCII console sees exactly the plain text
it saw before colour existed: no extra setting to find, and nothing to go wrong.
(The clear-screen dialect stays a separate setting, because "how do I clear" and
"can you show colour" are genuinely different questions — an ADM-3A answers `^Z`
and *no* to the second.)

Rather than chop every menu into coloured fragments, the key letters are coloured
by a printer that recognises the shape every menu line already has — two spaces,
the key, two spaces, the text — so the menus stay single readable blocks in the
source, and a continuation line (five leading spaces) is left alone because its
third character is a space. The escape sequences live in their own strings, never
inside menu text, so the screen-fit test still measures real printable width.

**The menu key is any control key you press.** Settings → `K` asks for it
directly instead of cycling three fixed choices, because which key is free
depends on the remote system rather than on us (`^Y` is WordStar's delete-line,
`^]` is telnet's escape, `^\` is Kermit's, and something wants each of them).
Five are refused and the refusal says why: `^C` backs out of every screen here,
CR/LF/TAB are ordinary typing, and `ESC` begins the arrow-key sequences, so a
cursor key would open the menu. A saved key is validated on startup too — an
invalid one would trap that key for ever.

**`^C`** gets you out — of any menu (the port list, every per-family prompt,
Settings) as well as the notice screens. Settings and port changes take effect immediately, but
they are only written to the file on `V`, and until then the menu says
`(changed — press V to keep it for next time)` — without that, "my settings
aren't saving" is the obvious conclusion. And where a message appears *while you
are typing* — the wrong-port notice — everything else you type is swallowed, so
a half-typed line cannot run menu commands by accident; only `^C` leaves. `^C`
also aborts a transfer in progress and cancels at the filename prompt.

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

## A further pass found four more

**CP/M 3 could swallow a data byte and eat a keystroke.** Direct console I/O
(BDOS 6) reserves three values of `E` — `FFh`, `FEh`, `FDh` mean read, status and
read-waiting — so writing a *data* byte of `FDh` or above through it performs an
input instead: the byte never prints, and a keystroke the user had typed is
consumed. A terminal passing 8-bit data hits this. Those three values now go out
through BDOS 2, which has no reserved values. (Reasoned from the CP/M 3 API, not
run: this emulator is CP/M 2.2, so the affected path can only be exercised on a
CP/M 3 machine.)

**A transfer can now be stopped.** Without it, a transfer whose peer has walked
away could only be waited out through every retry — the one moment a person most
wants out. `ESC` or the menu key stops it, checked once per block so the cost is
nothing; any other key is swallowed, because a stray keystroke shouldn't end a
transfer.

**A stale dead-port flag could misreport a failure.** `PDEAD` was only cleared
when terminal mode started, so a transfer run from the menu after a dead-port
session would blame the port for a failure it had nothing to do with. Each
transfer now starts with a fresh diagnosis — and when the port *is* the problem,
the failure names it instead of saying "the other end stopped answering".

**With local echo on, `CR` was echoed without its `LF`**, so the local screen
kept overwriting one line while the remote saw correct line ends.

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
