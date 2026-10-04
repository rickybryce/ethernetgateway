#!/usr/bin/env python3
"""One automated transfer against NovaTerm, in either direction.

    run-transfer.py <protocol> <download|upload> [dial]

`protocol` names both ends at once: the entry to pick in NovaTerm's own
protocol menu, and the key the gateway's transfer menu wants.  Keeping the two
in one table is the point -- running a Punter test against a Zmodem receiver
looks exactly like a protocol defect, and NovaTerm resets to Zmodem on load.
"""
import re, sys, time, novaterm

# NovaTerm's status line carries a running session clock, hh:mm:ss.
CLOCK = re.compile(r'\d\d:\d\d:\d\d')

# name -> (index in NovaTerm's Select-protocol list, download key, upload key)
# NovaTerm's list, in order: Zmodem, Ymodem batch, Ymodem-g, Xmodem-CRC,
# Xmodem-1k, Xmodem-1k-g, Punter, Multi-Punter, Kermit, WXmodem recv, Quit.
#
# **The gateway's two menus do not offer the same keys, so a protocol needs a
# key per direction.**  Download is the gateway choosing how to send and
# offers `X 1 Y Z K P`; upload is the gateway receiving, where 1K is the
# *sender's* choice detected per block from STX, so it offers only
# `X Y Z K P`.  XMODEM-1K is the one asymmetry, and a single-key table sent
# `1` at the upload prompt, which is not a choice there: the gateway stayed at
# its prompt, never armed a receiver, and NovaTerm sat on its transfer screen
# with nothing listening.  Nothing in the run said so -- the log had no
# `Upload:` line at all, and the failure read as XMODEM-1K being broken.
PROTOCOLS = {
    'zmodem':    (0, 'z', 'z'),
    'ymodem':    (1, 'y', 'y'),
    'xmodem':    (3, 'x', 'x'),
    'xmodem1k':  (4, '1', 'x'),
    'punter':    (6, 'p', 'p'),
    'kermit':    (8, 'k', 'k'),
}

def dismiss_welcome(nt):
    """Press SPACE past the gateway's welcome page, if it is showing.

    A gateway shows it for its first seven days (since e035aba, 2026-09-19),
    between the colour question and the main menu, and it waits for a key.
    Unexpected, it took the next keystroke -- the Telnet Gateway's `t` -- and
    the host and port then landed on the main menu, where the port's `2`
    opened the Second Menu.  Matched on lowercase words, since NovaTerm's
    screen text turns capitals into dots.
    """
    if any('or the main menu' in l or 'his page stops appearing' in l for l in nt.text()):
        print('  welcome page -> SPACE', flush=True)
        nt.type(' ', 3.0)


def started(nt):
    """Is NovaTerm already receiving?

    ZMODEM auto-download means the transfer can be under way before the
    harness would have started it, and the key that starts it by hand (C=) is
    also the key that aborts one in flight.  So ask the screen.  The transfer
    display is the same for every protocol -- a "bytes recv:" counter -- and
    the reversed capital in "Zmodem download" decodes to '.', so match on the
    counter rather than on the protocol's name.
    """
    scr = nt.text()
    return any('ytes recv' in l or 'ytes rec' in l for l in scr)


def answer_upload_dialogs(nt, name):
    """Answer whatever NovaTerm puts up on the way into an upload.

    There are up to three dialogs and which of them appear depends on the
    protocol, so this reads the screen and responds rather than replaying a
    fixed key sequence.  A fixed sequence is exactly what failed: the batch
    protocols open a *settings* dialog (Device / Translation / Pattern match)
    BEFORE the file selector, so a blind 'puntest\n' was swallowed as the
    RETURN that dismisses it, the selector came up behind, the rest of the
    name went to the terminal, and nothing was ever selected.  The gateway sat
    resending ZRINIT for the whole window and the run read as a ZMODEM
    failure, with the transfer never started at all.

    Each state is announced, because the screen is the only record of what was
    asked and a run that answers the wrong dialog looks identical to one the
    gateway failed.
    """
    typed_name = False
    for _ in range(6):
        scr = [l for l in nt.text() if l.strip()]
        blob = ' '.join(scr)
        if 'irectory' in blob and 'elected' in blob:
            # The two-column file selector the batch protocols use: they carry
            # the name in band, so NovaTerm picks the file rather than asking
            # for one.  f3 adds the highlighted entry to "selected", f7 starts.
            print('  dialog: file selector -> f3, f7', flush=True)
            nt.press('F3', 1.5)
            nt.press('F7', 3.0)
            return 'selector'
        if 'attern match' in blob:
            print('  dialog: upload settings -> RETURN', flush=True)
            nt.type('\n', 2.5)
            continue
        if 'prg' in blob:                        # "Type (prg,seq,usr):"
            print('  dialog: file type -> s', flush=True)
            nt.type('s\n', 1.5)
            continue
        if 'eplace' in blob:                     # "Replace?"
            print('  dialog: replace -> y', flush=True)
            nt.type('y', 1.5)
            continue
        if not typed_name:
            print('  dialog: name prompt -> %s' % name, flush=True)
            for l in scr[-6:]:
                print('    |%s|' % l, flush=True)
            nt.type(name + '\n', 2.5)
            typed_name = True
            continue
        return 'done'
    return 'gave up'


def set_protocol(nt, index):
    nt.keys.focus(); nt.keys.combo('Tab', 'p'); time.sleep(8.0)
    here = 0                     # the list always opens on Zmodem
    for _ in range(index - here):
        nt.press('Down', 0.35)
    nt.press('Return', 3.0)
    time.sleep(10.0)             # NovaTerm loads the protocol module from disk

def main():
    if len(sys.argv) < 3 or sys.argv[1] not in PROTOCOLS:
        print(__doc__); print("protocols:", ", ".join(sorted(PROTOCOLS)))
        return 2
    name, direction = sys.argv[1], sys.argv[2]
    dial = sys.argv[3] if len(sys.argv) > 3 else '1'
    index, dl_key, ul_key = PROTOCOLS[name]
    key = dl_key if direction == 'download' else ul_key

    nt = novaterm.NovaTerm()
    nt.ensure_terminal_mode()
    set_protocol(nt, index)
    nt.dial(dial)
    nt.inst_del(3.5)
    nt.type('n', 3.0)
    dismiss_welcome(nt)
    nt.type('f', 3.0)

    if direction == 'download':
        nt.type('d', 4.0)
        if not any('elect' in l for l in nt.text()):
            print("no file list:", [l for l in nt.text() if l.strip()][-5:]); return 1
        nt.type('4\n', 3.0)                 # PUNTEST.SEQ
        nt.type(key, 4.0)
        time.sleep(1.0)
        # **Do not press C= if the receiver is already running.**  C= is
        # NovaTerm's abort key -- the download screen says so -- and it is
        # polled continuously by `readline`/`rzdatexi` in the protocol
        # modules.  ZMODEM auto-downloads: `rzauthdr` catches the gateway's
        # ZRQINIT and NovaTerm answers ZRINIT on its own, about a second after
        # the gateway is told to send.  Pressing C= D four seconds later
        # therefore did not start the transfer, it CANCELLED one already in
        # its data phase -- measured at tcpser: ZRINIT, ZRPOS, our first data
        # subpacket, then eight CAN and eight backspaces exactly as the combo
        # was struck.  Two runs were read as a ZMODEM framing defect on that
        # evidence, and the sender was fine.
        if not started(nt):
            nt.keys.focus(); nt.keys.combo('Tab', 'd')
            time.sleep(4.0)
        # **Ask the dialog what it wants.**  It differs per protocol: Punter
        # and the XMODEM family prompt for a save-as name, while YMODEM and
        # ZMODEM carry the name in band and prompt for nothing.  Typing a name
        # blindly at those two sends it to the terminal instead, the receiver
        # never starts, and the gateway reports "Timeout waiting for receiver
        # to start" -- which looks like the protocol failing to negotiate.
        if any('.ile:' in l or 'ile:' in l for l in nt.text()):
            nt.type('%s\n' % name[:8], 2.0)
        # NovaTerm's download dialog asks different follow-up questions per
        # protocol, and any pause here is spent against the gateway's
        # 45-second window: answer them immediately or the transfer aborts
        # before the receiver has sent its first byte, which reads as a
        # protocol failure and is a lost race.
        for _ in range(3):
            scr = nt.text()
            if any('prg' in l for l in scr):          # "Type (prg,seq,usr):"
                nt.type('s\n', 1.2)
            elif any('eplace' in l for l in scr):     # "Replace?"
                nt.type('y', 1.2)
            else:
                break
    else:
        # The protocol first, then a name only if the gateway asks: the
        # Upload menu now asks which protocol before anything else, and
        # YMODEM, ZMODEM and Kermit ask no filename.  See relay-vice-harness.
        nt.type('u', 4.0)
        nt.type(key, 4.0)
        if key in ('x', 'p'):
            nt.type('%sup.seq\n' % name[:6], 3.5)
        time.sleep(1.0)
        nt.keys.focus(); nt.keys.combo('Tab', 'u')
        time.sleep(5.0)
        print("  answering the upload dialogs:", flush=True)
        print("  ->", answer_upload_dialogs(nt, 'puntest'), flush=True)

    # **A standing instruction is not an outcome.**  This used to break on
    # 'bort', which matches "hold C= to abort" -- the instruction NovaTerm
    # prints across the whole transfer -- so the run returned about ten
    # seconds in and the sweep graded the transfer directory while the
    # transfer was still running.  An XMODEM upload that completed
    # byte-perfectly was reported as "the upload never reached the gateway".
    #
    # So: a real outcome word, or a screen that has stopped changing.  The
    # settled check is what covers the protocols whose screen says nothing at
    # the end -- our XMODEM receiver waits out a 20-second silence before
    # accepting an EOT NAK the sender never answers, and the C64 is quiet for
    # all of it while the file has yet to be written.
    # The settled check must ignore NovaTerm's session clock, which ticks
    # once a second: comparing the whole screen, it never compares equal and
    # the branch is dead -- every run without an outcome word burned the full
    # 180 s instead of ~30.  Drop the line the clock is ON, found by matching
    # the clock rather than by its position, since the only evidence for the
    # position is dumps that print non-blank lines and so cannot say what
    # index anything really has.  Normalising more widely (stripping digits,
    # say) would blind this to the byte counter, which is the one number whose
    # movement means the transfer is still running.
    last, still = None, 0
    for _ in range(18):
        time.sleep(10)
        s = nt.text()
        if any('omplete' in l or 'rror' in l or 'ailed' in l for l in s):
            break
        body = [l for l in s if not CLOCK.search(l)]
        if body == last:
            still += 1
            if still >= 3:
                break
        else:
            last, still = body, 0
    for l in nt.text():
        if l.strip():
            print('|%s|' % l)
    return 0

if __name__ == '__main__':
    sys.exit(main())
