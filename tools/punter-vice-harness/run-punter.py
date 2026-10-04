#!/usr/bin/env python3
"""One automated Punter download, gateway -> NovaTerm, end to end.

Assumes the gateway, the link (ip232 or a PTY pair) and VICE are already up,
with NovaTerm sitting at its main menu.
"""
import sys, time, novaterm

def main():
    nt = novaterm.NovaTerm()

    # Warp is NOT toggled here.  It must be off -- the serial link runs in real
    # time and VICE at 241% drops bytes on it (NovaTerm's modem init arrives
    # with every letter missing under warp) -- but a blind toggle turns it back
    # ON for a second run, which is worse than leaving it alone.  The launcher
    # turns it off once, after the disk has loaded.
    nt.ensure_terminal_mode()

    # **Set the protocol explicitly.**  It comes up as Zmodem -- the BASIC
    # loader's default -- and a Punter run against a Zmodem receiver looks
    # exactly like a protocol bug.
    nt.keys.focus(); nt.keys.combo('Tab', 'p'); time.sleep(8.0)
    for _ in range(6):
        nt.press('Down', 0.35)
    nt.press('Return', 3.0)
    time.sleep(10.0)                      # NovaTerm loads prt.Punter from disk

    # The dial string depends on which link is under test: over the serial
    # PTY the gateway's own modem answers `ethernetgateway`, while over ip232
    # it is tcpser's phonebook that answers, where `1` is mapped to the
    # gateway.  Dialling the wrong one simply never connects.
    number = sys.argv[1] if len(sys.argv) > 1 else 'ethernetgateway'
    nt.dial(number)                       # hangs up first, and checks it worked
    nt.inst_del(3.5)                      # PETSCII detection
    nt.type('n', 3.0)                     # no colour
    novaterm.past_welcome(nt)             # a new gateway's welcome page
    nt.type('f', 3.0)                     # File Transfer
    nt.type('d', 4.0)                     # Download
    screen = nt.text()
    if not any('elect' in l for l in screen):
        print("did not reach the file list:", [l for l in screen if l.strip()][-6:])
        return 1
    nt.type('4\n', 3.0)                   # PUNTEST.SEQ
    nt.type('p', 3.0)                     # Punter
    time.sleep(1.0)
    nt.keys.focus(); nt.keys.combo('Tab', 'd')   # C= D -- receive
    time.sleep(4.0)
    nt.type('puntest\n', 2.0)
    for _ in range(12):
        time.sleep(10)
        s = nt.text()
        if any('omplete' in l or 'rror' in l or 'bort' in l for l in s):
            break
    for l in nt.text():
        if l.strip():
            print('|%s|' % l)
    return 0

if __name__ == '__main__':
    sys.exit(main())
