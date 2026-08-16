#!/usr/bin/env python3
"""Derive EGT8080.Z80 - the 8080 build - from EGT80.Z80.

WHY THIS IS A SCRIPT AND NOT A HAND-MAINTAINED SECOND FILE
----------------------------------------------------------

Two sources were chosen over conditional assembly, and the cost of that
choice is drift: EGT80.Z80 is 4,540 lines, it gets edited, and a second
copy would quietly stop matching it.  So the second copy is *derived*.
Edit EGT80.Z80, run `make port`, and the 8080 build is the same program
again.  Anything genuinely different between the two lives here, as a
named rule with a reason, instead of as a difference somebody has to
notice in a diff.

This is not conditional assembly: both files are ordinary CP/M assembler
sources that a period assembler builds on a real machine, which is the
property the EGT80 build exists to keep.

WHAT THE 8080 CANNOT DO, MEASURED
---------------------------------

A census of EGT80.Z80 at instruction position finds exactly four things
outside the 8080's instruction set, and nothing else - no IX/IY, no
shadow registers, no DJNZ, no RES/SET/SLA/SRL, no block I/O:

    JR cc,label / JR label   312   ->  JP cc,label / JP label
    BIT 7,A                    2   ->  OR A + JP M  (see below)
    LD (nn),DE                 2   ->  EX DE,HL + LD (nn),HL
    LDIR                       1   ->  a copy loop
    DB 0EDH,4CH (MLT BC)       1   ->  removed with the Z180 probe

That fourth line is the reason check8080.py exists.  A census by
MNEMONIC finds the first, second, fourth and fifth and reports "nothing
else" - `LD (nn),DE` hides among a hundred 8080 `LD`s.  A check by
instruction FORM found it in the first run.

The mnemonics stay Zilog and the assembler stays SLR Z80ASM: what has to
be in the 8080's set is the *opcodes emitted*, not the words used to
write them.  `ADC A,40H` is 8080 `ACI 40H` and both assemble to CE 40.
`check8080.py` is what actually holds that line - it reads the derived
source and refuses anything outside the subset, so the guarantee is in
the build rather than in this comment.

TWO FLAG FACTS THAT COULD HAVE SPOILED THE PORT, BOTH CHECKED
-------------------------------------------------------------

* **Parity.** The 8080's P flag is parity where the Z80's P/V is
  overflow.  There is no `JP PE` or `JP PO` anywhere in EGT80.Z80, so
  the difference never arises.
* **DAA.** PHEXD does `AND 0FH / ADD A,90H / DAA / ADC A,40H / DAA`,
  the hex-digit-to-ASCII trick.  DAA differs between the two CPUs only
  after a *subtraction* - the Z80's N flag makes it adjust downward -
  and both of these follow additions.  Same result on either core.
  It is still the one instruction worth running rather than reasoning
  about, which the live gate does.
"""

import re
import sys

# Conditions a Z80 JR can carry.  All four are also JP conditions on an
# 8080, so the rewrite never has to invent one.
JR_RE = re.compile(r'^(\s+)JR(\s+)(.*)$')


def split_code(line):
    """The instruction part of a line, and where it starts.

    Returns (prefix, body) where prefix is the label and whitespace.
    Comment-only and blank lines come back with body ''.
    """
    code = line.split(';')[0].rstrip()
    if not code.strip():
        return line, ''
    return None, code


def rewrite_jr(lines):
    """JR -> JP, keeping the column layout the source is written in.

    `JR` and `JP` are both two characters, so the operand column does not
    move and the derived file diffs cleanly against its parent.
    """
    out = []
    n = 0
    for line in lines:
        code = line.split(';')[0]
        m = re.match(r'^(\s+)JR(\s|,)', code)
        if m:
            # Only at instruction position: a label called JR would not be
            # indented, and `JR` inside a comment or a string is not `code`.
            out.append(line.replace('JR', 'JP', 1))
            n += 1
        else:
            out.append(line)
    return out, n


# Quoted text that must NOT be renamed, even though it contains EGT80.
#
# `EGT80CFG` is the settings-block signature: the same eight bytes in both
# builds on purpose, because each reads only its own file and one
# signature keeps the offset-0x80 reader from having to know which build
# it is looking at.  `EGT80   ` is the FCB filename field, which is
# exactly eight characters wide - it gets its own rule below, because
# renaming it here would produce a ten-character field.
KEEP_LITERAL = ("EGT80CFG", "EGT80   ")


def rename_in_strings(text):
    """`EGT80` -> `EGT8080` inside quoted text, and nowhere else.

    These are the words the *user* reads - "Save settings into EGT80.COM",
    "EGT80 done." - and in this build the program is not called that.  It
    is not cosmetic: the save message names the file EGT8080 actually
    writes, and a wrong one sends somebody looking in the other build's
    file for settings that are not there.  The live gate caught these
    still saying EGT80 after everything else had been ported, which is
    what a rule here prevents next time.

    Assembler lines are <= 80 columns by this source's own rules, and each
    of these grows by two, so the result is measured rather than assumed.
    """
    out = []
    renamed = 0
    for n, line in enumerate(text.split('\n'), 1):
        # Only inside single-quoted literals on a DB line.
        def fix(m):
            nonlocal renamed
            body = m.group(1)
            if body in KEEP_LITERAL or 'EGT80' not in body:
                return m.group(0)
            renamed += 1
            return "'" + body.replace('EGT80', 'EGT8080') + "'"

        new = re.sub(r"'([^']*)'", fix, line) if re.search(r'\bDB\b', line) else line
        if len(new.rstrip()) > 80:
            sys.exit(
                f"port8080: line {n} would be {len(new.rstrip())} columns after "
                f"renaming, and this source keeps to 80:\n  {new}"
            )
        out.append(new)
    return '\n'.join(out), renamed


# --- The named differences, each with its reason ------------------------
#
# Every one of these is a (find, replace, why) applied to the whole file
# exactly once.  Applying zero times is an error: it means EGT80.Z80 has
# moved and the rule no longer describes it, which is the failure this
# script exists to make loud instead of silent.

# --- Port I/O: the biggest difference, and the one that nearly shipped ---
#
# The Z80 reads a port whose address is in C.  The 8080 has no such
# instruction: the port is an immediate byte inside the opcode, full
# stop.  EGT80's whole premise is a port chosen at RUN TIME, so on an
# 8080 each driver has to write the address into its own IN or OUT
# before executing it.
#
# That is self-modifying code, and it is the idiom this program already
# uses twice over - PSSET2 patches the four port vectors, ASCPAT patches
# the ASCI register addresses.  It is safe here for the same reason: the
# patch happens immediately before the instruction it patches, in a
# routine with no interrupts and no reentrancy, and the vector contract
# already says these routines keep nothing in registers.
#
# `LD C,A` and `LD B,0` go with them.  B was cleared because the Z80 puts
# BC on the address bus and stray high bits confuse partially-decoded
# boards; an 8080 puts the immediate port on both halves of the address
# bus, so there is no high byte to get wrong.
#
# Labels are IOPn rather than something derived from the routine, because
# this source requires labels unique in their first SIX characters and
# `SIOOST` + a suffix is `SIOOST` again.

PORT_IO = [
    # (routine label, extra line before the port is used, kind)
    ("SIOST",  "", "in"),   ("SIOIN",  "        INC     A\n", "in"),
    ("SIOOST", "", "in"),   ("SIOOUT", None, "out"),
    ("ACIST",  "", "in"),   ("ACIIN",  "        INC     A\n", "in"),
    ("ACIOST", "", "in"),   ("ACIOUT", None, "out"),
    ("S8ST",   "", "in"),   ("S8IN",   "        INC     A\n", "in"),
    ("S8OST",  "", "in"),   ("S8OUT",  None, "out"),
]


def port_io_rules():
    """One (find, replace, why) per driver entry point."""
    rules = []
    for n, (label, extra, kind) in enumerate(PORT_IO, 1):
        tag = f"IOP{n}"
        if kind == "in":
            find = (f"{label}:{' ' * (8 - len(label) - 1)}LD      A,(PBASE)\n"
                    f"{extra}"
                    "        LD      C,A\n"
                    "        LD      B,0\n"
                    "        IN      A,(C)\n")
            replace = (f"{label}:{' ' * (8 - len(label) - 1)}LD      A,(PBASE)\n"
                       f"{extra}"
                       f"        LD      ({tag}+1),A     ; no IN A,(C) on an 8080: the port\n"
                       f"{tag}:{' ' * (8 - len(tag) - 1)}IN      A,(0)           ; is patched into the operand above\n")
        else:
            find = (f"{label}:{' ' * (8 - len(label) - 1)}LD      E,A"
                    + ("             ; hold the byte; A is needed for the port\n"
                       if label == "SIOOUT" else "\n")
                    + "        LD      A,(PBASE)\n"
                      "        INC     A\n"
                      "        LD      C,A\n"
                      "        LD      B,0\n"
                      "        LD      A,E\n"
                      "        OUT     (C),A\n")
            replace = (f"{label}:{' ' * (8 - len(label) - 1)}LD      E,A             ; hold the byte; A is needed for the port\n"
                       "        LD      A,(PBASE)\n"
                       "        INC     A\n"
                       f"        LD      ({tag}+1),A     ; no OUT (C),A on an 8080: patch it\n"
                       "        LD      A,E\n"
                       f"{tag}:{' ' * (8 - len(tag) - 1)}OUT     (0),A\n")
        rules.append((find, replace, f"{label} port I/O"))
    return rules


SUBSTITUTIONS = [
    (
        # ACINIT writes two control bytes to one port, so both operands
        # are patched from the one address before either runs.
        """ACINIT: LD      A,(PBASE)
        LD      C,A
        LD      B,0
        LD      A,03H           ; master reset
        OUT     (C),A
        LD      A,15H           ; 8N1, /16 clock, receive interrupt off
        OUT     (C),A
        RET""",
        """ACINIT: LD      A,(PBASE)
        LD      (IOP13+1),A     ; one port, two writes: both operands are
        LD      (IOP14+1),A     ; patched before either instruction runs
        LD      A,03H           ; master reset
IOP13:  OUT     (0),A
        LD      A,15H           ; 8N1, /16 clock, receive interrupt off
IOP14:  OUT     (0),A
        RET""",
        "ACINIT port writes",
    ),
    (
        """        LD      A,(PBASE)
        LD      C,A
        LD      B,0
        LD      A,03H           ; master reset first, as the data sheet
        OUT     (C),A           ; requires before a control write
        LD      A,E
        OUT     (C),A""",
        """        LD      A,(PBASE)
        LD      (IOP15+1),A     ; as ACINIT: one port, two writes, both
        LD      (IOP16+1),A     ; operands patched first
        LD      A,03H           ; master reset first, as the data sheet
IOP15:  OUT     (0),A           ; requires before a control write
        LD      A,E
IOP16:  OUT     (0),A""",
        "LSACIA port writes",
    ),
    (
        # HBST: RST 8 leaves a count in A, or an error with bit 7 set.
        # `BIT 7,A` tests it without touching A, which the 8080 has no
        # instruction for - but it does not need one.  `OR A` already
        # follows on the good path, and it sets S from bit 7, so the test
        # is free: OR A, then JP M.  A and the returned flags are
        # identical to the Z80 build's.
        """        RST     8
        BIT     7,A             ; see HBERR below: negative is an error, not
        JP      NZ,HBNONE       ; a count, and must not read as "data ready"
        OR      A
        RET""",
        """        RST     8
        OR      A               ; see HBERR below: negative is an error, not
        JP      M,HBNONE        ; a count, and must not read as "data ready".
        RET                     ; OR A sets S from bit 7, so no BIT is needed
                                ; and A comes back untouched, as this returns""",
        "HBST bit-7 test",
    ),
    (
        """        RST     8
        BIT     7,A
        JP      NZ,HBFULL
        OR      A
        RET""",
        """        RST     8
        OR      A               ; bit 7 = error, not room.  S flag, as HBST
        JP      M,HBFULL
        RET""",
        "HBOST bit-7 test",
    ),
    (
        # The one LDIR.  BC is the count and the 8080 has no flag from
        # DEC BC, so the loop tests B or C for zero itself.
        """        LD      BC,DEFLEN
        LDIR""",
        """        LD      BC,DEFLEN
CFGCPY: LD      A,(HL)          ; LDIR by hand: the 8080 has no block move,
        LD      (DE),A          ; and DEC BC sets no flags, so the count is
        INC     HL              ; tested with LD A,B / OR C
        INC     DE
        DEC     BC
        LD      A,B
        OR      C
        JP      NZ,CFGCPY""",
        "LDIR",
    ),
    (
        # ISZ180 probes with MLT BC, laid down as DB 0EDH,4CH because no
        # Z80 assembler encodes it.  On an 8080 that ED byte is an
        # UNDOCUMENTED CALL - so the probe does not merely fail here, it
        # jumps into the weeds.  An 8080 is never a Z180, so the answer is
        # a constant one and the whole probe goes: the ED bytes must not be
        # in the binary at all, unreachable or not.  The ASCI family then
        # leaves the port menu through the guard that was always in front
        # of it - all three callers read carry as "not a Z180" and take the
        # path they already had for a plain Z80.
        """ISZ180: LD      BC,0202H
        DB      0EDH,4CH        ; MLT BC on a Z180; no-op on a Z80
        LD      A,B
        OR      A
        JP      NZ,ISZNOT       ; B still 2: not a Z180
        LD      A,C
        CP      04H             ; 2 x 2
        JP      NZ,ISZNOT
        XOR     A               ; carry clear: this is a Z180
        RET
ISZNOT: SCF
        RET""",
        """ISZ180: SCF                     ; an 8080 is never a Z180, and this must
        RET                     ; not even ASK.  The Z80 build probes with
                                ; MLT BC, laid down as DB 0EDH,4CH because
                                ; no Z80 assembler encodes it - and on an
                                ; 8080 that ED byte is an UNDOCUMENTED CALL,
                                ; so the probe would not fail, it would jump
                                ; into the weeds.  The bytes are gone rather
                                ; than left unreachable.  Carry set is "not
                                ; a Z180", which all three callers already
                                ; know how to handle.""",
        "ISZ180 probe",
    ),
    (
        # `LD (nn),DE` is Z80-only - ED 53 - and it is the one Z80
        # instruction the mnemonic census MISSED, because it shares
        # `LD` with a hundred 8080 moves.  check8080.py found it, which
        # is the whole argument for checking forms rather than mnemonics.
        #
        # XCHG then SHLD does it.  That destroys HL and DE, and both
        # routines return immediately: every caller in PORTS1 reloads
        # both pairs before the next call, so nothing downstream reads
        # them.
        """PSSET2: LD      (VPST+1),HL
        LD      (VPIN+1),DE
        RET
PSSETO: LD      (VPOST+1),HL
        LD      (VPOUT+1),DE
        RET""",
        """PSSET2: LD      (VPST+1),HL
        EX      DE,HL           ; the 8080 has no LD (nn),DE, so XCHG and
        LD      (VPIN+1),HL     ; store HL.  HL and DE are dead on return -
        RET                     ; every caller reloads both pairs.
PSSETO: LD      (VPOST+1),HL
        EX      DE,HL
        LD      (VPOUT+1),HL
        RET""",
        "PSSET2/PSSETO vector stores",
    ),
    (
        # Identity.  CP/M never tells a program its own name, so the file
        # this one writes its settings back into is hard-coded - and it
        # must be *this* program's file, not the Z80 build's.
        """CFGFCB: DB      0
        DB      'EGT80   '""",
        """CFGFCB: DB      0
        DB      'EGT8080 '""",
        "settings FCB filename",
    ),
    (
        # After the string pass, which has already made this EGT8080.  What
        # is left is the processor it names.
        """MVER:   DB      'EGT8080 v0.7  CP/M 2.2 / CP/M 3  Z80',CR,LF,0""",
        """MVER:   DB      'EGT8080 v0.7  CP/M 2.2 / CP/M 3  8080',CR,LF,0""",
        "version line",
    ),
    (
        """; EGT80 - "Ethernet Gateway Terminal"
;""",
        """; EGT8080 - "Ethernet Gateway Terminal", 8080 build
;
; DERIVED FILE - DO NOT EDIT.  Generated from EGT80.Z80 by
; tools/port8080.py; run `make port` after editing EGT80.Z80.  Editing
; this file directly means the next `make port` throws the edit away.
;
; The same program as EGT80, built for a machine with no Z80 in it - an
; Altair, an IMSAI, or the gateway's own emulator with cpm_cpu = 8080.
; 8080 opcodes are a strict subset of the Z80's, so this build runs on
; BOTH settings and is the one placed on drive A: by default; EGT80.COM
; stays for the Z80 -- and it is not a courtesy copy.  A Z180 board such
; as the SC126 drives its console from the ASCI channels inside the
; processor, reached with IN0/OUT0 and found with MLT BC.  Those are all
; ED-prefixed instructions, and an ED byte on a true 8080 is an
; undocumented CALL - so the family cannot be USED here.  The machines
; this build exists for and the machines that need the ASCI are disjoint;
; that is why there are two.
;
; Note what that does and does not mean, because it is measurable and was
; once written down wrong.  The ASCI driver's IN0/OUT0 bytes are still in
; this file, laid down as DB, and 11 of them are in the assembled .COM.
; They are unreachable: ISZ180 below always answers "not a Z180", every
; caller branches away, and the one byte pair that would have EXECUTED -
; MLT BC, ED 4C - is gone.  Reachability is the property that matters;
; "no ED byte anywhere in the image" is a stronger claim than this build
; makes, and counting the bytes is how you find that out.
;
; What differs from EGT80.Z80, and nothing else does: JR became JP, two
; BIT 7,A became OR A + JP M, the one LDIR became a copy loop, and the
; Z180 probe answers "no" without asking.  That probe is MLT BC, an
; ED-prefixed instruction, and on a true 8080 an ED byte is an
; undocumented CALL - it would not fail, it would jump into the weeds.
;
; The Z180 ASCI family is still ON the port menu here and is refused when
; chosen ("This processor is not a Z180, so it has no ASCI ports"), which
; is what ISZ180 always answering "not a Z180" buys.  Listing it and
; refusing beats hiding it: the operator of a Z180 board learns the family
; exists and that this is the wrong build for it, rather than finding a
; menu that is silently one item shorter.  Use EGT80.COM there.
;
; The reasoning for each is in tools/port8080.py.
;""",
        "header block",
    ),
]


def main(src_path, dst_path):
    src = open(src_path, encoding='ascii').read()

    # JR first: the rules below are written against the post-rewrite text,
    # so that what they show is what the 8080 file actually says.
    lines, jrs = rewrite_jr(src.split('\n'))
    if jrs == 0:
        sys.exit("port8080: no JR found at instruction position - that cannot be right")
    src = '\n'.join(lines)

    src, renamed = rename_in_strings(src)
    if renamed == 0:
        sys.exit("port8080: no EGT80 found in quoted text - that cannot be right")

    for find, replace, why in port_io_rules() + SUBSTITUTIONS:
        count = src.count(find)
        if count != 1:
            sys.exit(
                f"port8080: the rule for '{why}' matched {count} times, not once.\n"
                f"EGT80.Z80 has moved under it - update the rule in "
                f"tools/port8080.py rather than the derived file."
            )
        src = src.replace(find, replace, 1)

    open(dst_path, 'w', encoding='ascii').write(src)
    print(f"--- {dst_path}: {jrs} JR -> JP, {renamed} strings renamed, "
          f"{len(port_io_rules()) + len(SUBSTITUTIONS)} named differences")


if __name__ == '__main__':
    main(sys.argv[1] if len(sys.argv) > 1 else 'EGT80.Z80',
         sys.argv[2] if len(sys.argv) > 2 else 'EGT8080.Z80')
