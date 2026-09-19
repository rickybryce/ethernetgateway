"""The one byte comparison both verifiers use.

Download and upload are checked by two scripts because the file is fetched
from two different places -- a D64 directory and the gateway's transfer
directory -- but *what counts as identical* must not be decided twice.  A
second copy of this rule would drift, and the difference would then read as a
protocol defect on whichever side was checked by the stale copy.
"""


def compare(body, want):
    """Is `body` the payload?  Returns (ok, description).

    Trailing SUB inside one block is correct behaviour, not corruption -- but
    it is reported out loud with the byte count rather than trimmed quietly,
    because a silent trim would also hide a genuinely truncated transfer.

    **There are two reasons it happens and this function cannot tell them
    apart**, which is why the note names both instead of asserting one.
    XMODEM and XMODEM-1K carry no length field at all, so the last block is
    padded to a 128-byte boundary and the receiver has nothing to trim to.  A
    YMODEM sender *does* declare a length, and may declare one longer than the
    payload -- in which case a correct receiver keeps the padding, because the
    sender said it was file content.

    Measured on the relay rig 2026-09-18: NovaTerm's YMODEM block 0 declared
    `size=1778` for a 1775-byte payload on both links, and the gateway logged
    "truncated to YMODEM size 1778 bytes" -- it read the length field and
    honoured it exactly.  The old note here said "XMODEM has no length field"
    about that run, which is false for YMODEM and sent a reviewer looking for
    a receiver defect that did not exist.  A grader that explains a result is
    making a claim, and this one was wrong on the protocol it named.
    """
    if body == want:
        return True, "%d bytes identical" % len(body)
    trimmed = body
    while trimmed and trimmed[-1] == 0x1A:
        trimmed = trimmed[:-1]
    pad = len(body) - len(trimmed)
    if trimmed == want and pad < 128:
        return True, ("%d bytes identical + %d bytes of SUB padding "
                      "(sender's own trailing bytes: XMODEM declares no "
                      "length, and a YMODEM sender may declare more than it "
                      "sends)" % (len(want), pad))
    return False, "%d bytes, expected %d — DIFFERS" % (len(body), len(want))
