#!/usr/bin/env python3
"""Refuse any instruction EGT8080.Z80 emits that an 8080 cannot execute.

WHY THIS EXISTS
---------------

EGT8080.Z80 is written in Zilog mnemonics and assembled by a Z80
assembler, because what has to be inside the 8080's instruction set is
the *opcodes emitted*, not the words used to write them - `ADC A,40H` is
8080 `ACI 40H`, and both are CE 40.  The price of that choice is that
the assembler will happily accept `JR` or `LDIR` and say nothing.

So the rule is held here instead.  Every instruction in the file is
matched against the 8080's set, written out below as forms rather than
as a list of mnemonics, because half the Z80-only instructions share a
mnemonic with an 8080 one: `LD A,B` is 8080, `LD A,(IX+0)` is not; `ADD
A,B` is 8080, `ADD HL,BC` is 8080 but `ADC HL,BC` is not.  Anything this
file cannot positively account for is an error, not a pass - a checker
that only knows what to reject grows a hole every time somebody writes
something it has not seen.

The failure this prevents is worth stating.  A stray Z80 instruction on
a rarely-taken path assembles, links, ships, and then crashes an Altair
in whatever menu nobody exercised.  The live gate runs the terminal but
it cannot run every path.  This can.

Run by `make EGT8080.COM`; it needs no CP/M tooling, so it is also the
one part of the EGT8080 build a machine without zxcc can run.
"""

import re
import sys

# --- the 8080's registers and operand shapes ----------------------------

R8 = r'(?:A|B|C|D|E|H|L|\(HL\))'          # 8-bit operands MOV can name
RP = r'(?:BC|DE|HL|SP)'                    # 16-bit pairs
RP_STACK = r'(?:BC|DE|HL|AF)'              # PUSH/POP name AF, not SP
CC = r'(?:NZ|Z|NC|C|PO|PE|P|M)'            # all eight 8080 conditions
# An expression: a label, a number, a character, or arithmetic on them.
# Deliberately loose - this checker is about instruction *forms*, and the
# assembler is the authority on whether an expression is valid.
X = r'[^,]+'

# Instructions rejected before the whitelist is consulted.
#
# These name registers the 8080 does not have, and they would otherwise
# slip through a *form* that is right about its shape: `LD A,(IX+0)` is
# an `LD r,something`, `LD (IX+0),A` is an `LD (expr),A`, `ADD A,(IY+2)`
# is an `ADD A,something`. Rejecting the register outright is exact -
# there is no 8080 instruction that mentions IX, IY, I or R at all - and
# it cannot be over-broad, which a shape-by-shape exclusion could.
REJECT = [
    (r'\b(IX|IY|IXH|IXL|IYH|IYL)\b', 'IX/IY do not exist on an 8080'),
    (r"^LD\s+A\s*,\s*[IR]$", 'LD A,I and LD A,R are ED-prefixed'),
    (r"^LD\s+[IR]\s*,\s*A$", 'LD I,A and LD R,A are ED-prefixed'),
    (r"^EX\s+AF", "the shadow registers do not exist on an 8080"),
]

FORMS = [
    # 8-bit moves.  The immediate forms exclude a parenthesised operand:
    # `LD BC,(1234H)` is ED 4B, `LD DE,(nn)` is ED 5B and `LD SP,(nn)` is
    # ED 7B - all Z80-only, and all shaped exactly like the LXI they are
    # not.  `LD HL,(nn)` IS 8080 (LHLD) and has its own form below, so
    # excluding the paren here loses nothing.
    #
    # This was the second hole of the same kind, found the same way: a
    # probe that injects the instruction and checks the checker rejects
    # it.  Do that for every form added here.
    rf'LD\s+{R8},{R8}',
    rf'LD\s+{R8},(?!\(){X}',              # LD r,n  (MVI)
    rf'LD\s+A,\((?:BC|DE)\)',
    rf'LD\s+\((?:BC|DE)\),A',
    rf'LD\s+A,\({X}\)',                    # LDA nn
    rf'LD\s+\({X}\),A',                    # STA nn
    # 16-bit moves
    rf'LD\s+{RP},(?!\(){X}',              # LXI / also LD SP,HL below
    r'LD\s+SP,HL',
    rf'LD\s+HL,\({X}\)',                   # LHLD
    rf'LD\s+\({X}\),HL',                   # SHLD
    # arithmetic and logic, 8-bit
    rf'(?:ADD|ADC|SUB|SBC)\s+A,{R8}',
    rf'(?:ADD|ADC|SUB|SBC)\s+A,{X}',
    rf'(?:AND|OR|XOR|CP)\s+{R8}',
    rf'(?:AND|OR|XOR|CP)\s+{X}',
    rf'SUB\s+{R8}',                        # SUB r, the one-operand spelling
    rf'SUB\s+{X}',                         # SUB n  (SUI) - the source writes
                                           # both spellings, and `SUB 'A'-1`
                                           # is an expression, not a register
    rf'INC\s+{R8}',
    rf'DEC\s+{R8}',
    # 16-bit arithmetic.  ADD HL,rp is 8080; ADC/SBC HL,rp is NOT.
    rf'ADD\s+HL,{RP}',
    rf'INC\s+{RP}',
    rf'DEC\s+{RP}',
    # rotates - the four accumulator ones only.  RLC/RRC/RL/RR on a
    # register are CB-prefixed and Z80-only.
    r'(?:RLCA|RRCA|RLA|RRA)',
    # flags and the accumulator
    r'(?:DAA|CPL|SCF|CCF|NOP|HALT|DI|EI)',
    # control flow
    rf'JP\s+{CC},{X}',
    rf'JP\s+{X}',
    r'JP\s+\(HL\)',
    rf'CALL\s+{CC},{X}',
    rf'CALL\s+{X}',
    rf'RET\s+{CC}',
    r'RET',
    rf'RST\s+{X}',
    # stack and exchange
    rf'PUSH\s+{RP_STACK}',
    rf'POP\s+{RP_STACK}',
    r'EX\s+\(SP\),HL',
    r'EX\s+DE,HL',
    # I/O.  IN A,(n) / OUT (n),A only.  The port must be an *expression*,
    # never the register C: `IN A,(C)` and `OUT (C),A` are ED 78 / ED 79,
    # Z80-only, and the 8080 has no register-indirect I/O at all.
    #
    # This is the hole that let the first EGT8080 build ship a wild jump.
    # `(?:[^,]+)` matched `(C)` perfectly happily, sixteen times, and the
    # mnemonic census missed them too because IN and OUT are shared - so
    # the program ran on a Z80, passed every shape check, and on an 8080
    # executed ED as an undocumented CALL straight into a string constant.
    # An operand pattern that accepts anything is not a check.
    rf'IN\s+A,\((?!\s*C\s*\)){X}\)',
    rf'OUT\s+\((?!\s*C\s*\)){X}\),A',
]

# Assembler directives, which emit no opcode at all.  DB is data by
# definition: if somebody lays down an ED byte with DB, this checker
# cannot know and is not trying to - that is what the ISZ180 rule in
# port8080.py is for.
DIRECTIVES = r'(?:EQU|ORG|DB|DW|DS|DEFB|DEFW|DEFS|END|\.Z80|\.8080)'

FORM_RE = re.compile(r'^(?:' + '|'.join(f'(?:{f})' for f in FORMS) + r')\s*$', re.I)
REJECT_RE = [(re.compile(p, re.I), why) for p, why in REJECT]
DIRECTIVE_RE = re.compile(rf'^{DIRECTIVES}\b', re.I)
LABEL_RE = re.compile(r'^([A-Za-z$?@][A-Za-z0-9$?@_]*):?\s*')


def instruction(line):
    """The instruction on a source line, or None.

    Strips the comment and any label.  A label without a colon in column
    one is how this source writes EQU lines, so a leading symbol is
    dropped whenever what follows still looks like a statement.
    """
    code = line.split(';')[0].rstrip()
    if not code.strip():
        return None
    if code[0] not in ' \t':
        # Something in column 1: a label, with or without a colon.
        m = LABEL_RE.match(code)
        if not m:
            return None
        code = code[m.end():]
    return code.strip() or None


def main(path):
    bad = []
    checked = 0
    for n, line in enumerate(open(path, encoding='ascii'), 1):
        ins = instruction(line.rstrip('\n').rstrip('\r'))
        if ins is None:
            continue
        if DIRECTIVE_RE.match(ins):
            continue
        checked += 1
        rejected = next((why for r, why in REJECT_RE if r.search(ins)), None)
        if rejected is not None:
            bad.append((n, f"{ins}   <- {rejected}"))
        elif not FORM_RE.match(ins):
            bad.append((n, ins))

    if bad:
        print(f"*** {path}: {len(bad)} instruction(s) an 8080 cannot execute:",
              file=sys.stderr)
        for n, ins in bad[:40]:
            print(f"    line {n}: {ins}", file=sys.stderr)
        if len(bad) > 40:
            print(f"    ... and {len(bad) - 40} more", file=sys.stderr)
        print("\n    If one of these IS an 8080 instruction this checker does not",
              file=sys.stderr)
        print("    know, add its FORM to tools/check8080.py - never widen it to",
              file=sys.stderr)
        print("    a bare mnemonic, because half of them are shared.", file=sys.stderr)
        return 1

    print(f"--- {path}: {checked} instructions, all in the 8080 set")
    return 0


# --- self-test ---------------------------------------------------------
#
# This checker has now had TWO holes of the same kind, and both were found
# the same way: write the instruction down and see whether it is rejected.
# So that is a mode, run by `make`, rather than something someone thinks
# to do.
#
# MUST_ACCEPT is half the value: a rule that rejects `LD BC,(nn)` by also
# rejecting `LD HL,(nn)` has not helped, and only the second list notices.

MUST_REJECT = [
    'JR      NZ,FOO', 'JR      FOO', 'DJNZ    FOO',
    'BIT     7,A', 'RES     0,B', 'SET     3,(HL)',
    'SLA     A', 'SRL     B', 'RL      C', 'RRC     D',
    'LDIR', 'LDDR', 'CPIR', 'INIR', 'OTIR',
    'IN      A,(C)', 'IN      B,(C)', 'OUT     (C),A', 'OUT     (C),B',
    'LD      BC,(1234H)', 'LD      DE,(1234H)', 'LD      SP,(1234H)',
    'LD      A,(IX+0)', 'LD      (IY+3),B', 'ADD     A,(IX+2)',
    'INC     (IX+0)', 'PUSH    IX', 'POP     IY', 'JP      (IX)',
    'LD      A,I', 'LD      R,A', 'EXX', "EX      AF,AF'",
    'ADC     HL,BC', 'SBC     HL,DE', 'NEG', 'IM      1', 'RETI', 'RETN',
    'RLD', 'RRD', 'LD      SP,IX',
]

MUST_ACCEPT = [
    'LD      A,B', 'LD      A,(HL)', 'LD      (HL),A', 'LD      A,(BC)',
    'LD      (DE),A', 'LD      A,(1234H)', 'LD      (1234H),A',
    'LD      HL,(1234H)', 'LD      (1234H),HL', 'LD      SP,HL',
    'LD      BC,1234H', 'LD      A,0FFH',
    'ADD     A,B', 'ADD     A,10H', 'ADC     A,40H', 'SUB     B', 'SUB     5',
    'SBC     A,C', 'AND     0FH', 'OR      A', 'XOR     A', 'CP      3',
    'ADD     HL,DE', 'INC     A', 'DEC     BC', 'INC     (HL)',
    'RLCA', 'RRCA', 'RLA', 'RRA', 'DAA', 'CPL', 'SCF', 'CCF',
    'NOP', 'HALT', 'DI', 'EI',
    'JP      NZ,FOO', 'JP      FOO', 'JP      (HL)', 'JP      M,FOO',
    'CALL    Z,FOO', 'CALL    FOO', 'RET     NC', 'RET', 'RST     8',
    'PUSH    AF', 'POP     HL', 'EX      (SP),HL', 'EX      DE,HL',
    'IN      A,(82H)', 'OUT     (83H),A', 'IN      A,(0)', 'OUT     (0),A',
]


def self_test():
    """Prove the checker rejects what it must and accepts what it must."""
    wrong = []
    for ins in MUST_REJECT:
        if not any(r.search(ins) for r, _ in REJECT_RE) and FORM_RE.match(ins):
            wrong.append(f"ACCEPTED a Z80-only instruction: {ins!r}")
    for ins in MUST_ACCEPT:
        if any(r.search(ins) for r, _ in REJECT_RE) or not FORM_RE.match(ins):
            wrong.append(f"REJECTED an 8080 instruction: {ins!r}")
    if wrong:
        print("*** check8080 self-test failed:", file=sys.stderr)
        for w in wrong:
            print(f"    {w}", file=sys.stderr)
        return 1
    print(f"--- self-test: {len(MUST_REJECT)} rejected, {len(MUST_ACCEPT)} accepted")
    return 0


if __name__ == '__main__':
    if len(sys.argv) > 1 and sys.argv[1] == '--self-test':
        sys.exit(self_test())
    sys.exit(main(sys.argv[1] if len(sys.argv) > 1 else 'EGT8080.Z80'))
