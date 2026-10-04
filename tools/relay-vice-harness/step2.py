#!/usr/bin/env python3
"""Re-run the failing xmodem download by hand, with a screen after every step.

The sweep keeps only the FINAL screen, which cannot say which step the run
stopped at.  The first attempt at this crashed before it began: it assumed the
line was idle while the C64 was still in a session, so `hangup` never found
'ok' and its keystrokes walked into the master's help pages.  The line is made
clean the way relay-one.sh makes it -- by restarting the slave -- and the
caller does that before this runs.

Every step mirrors run-transfer.py's download path exactly; the only additions
are the dumps and the long tail of polls.  A different sequence here would
measure a different thing.
"""
import sys, time, novaterm

def dump(nt, tag):
    print("\n===== %s" % tag, flush=True)
    for l in nt.text():
        if l.strip():
            print("|%s|" % l, flush=True)

nt = novaterm.NovaTerm()
nt.ensure_terminal_mode(); dump(nt, "terminal mode")

# index 3 = Xmodem-CRC, picked exactly as run-transfer.py picks it
nt.keys.focus(); nt.keys.combo('Tab', 'p'); time.sleep(8.0)
for _ in range(3):
    nt.press('Down', 0.35)
nt.press('Return', 3.0); time.sleep(10.0)
dump(nt, "protocol selected (Xmodem-CRC)")

nt.dial('ethernetgateway'); dump(nt, "dialled")
nt.inst_del(3.5);   dump(nt, "INST/DEL (terminal detect)")
nt.type('n', 3.0);  dump(nt, "n (no colour)")
novaterm.past_welcome(nt); dump(nt, "past the welcome page, if any")
nt.type('f', 3.0);  dump(nt, "f (File Transfer)")
nt.type('d', 4.0);  dump(nt, "d (Download)")
nt.type('4\n', 3.0); dump(nt, "4 (PUNTEST.SEQ)")
nt.type('x', 4.0); time.sleep(1.0); dump(nt, "x (gateway sends XMODEM)")

started = any('ytes recv' in l or 'ytes rec' in l for l in nt.text())
print("\nreceiver already running? %s" % started, flush=True)
if not started:
    nt.keys.focus(); nt.keys.combo('Tab', 'd'); time.sleep(4.0)
    dump(nt, "C= D (start NovaTerm's receiver)")

if any('.ile:' in l or 'ile:' in l for l in nt.text()):
    nt.type('xmodem\n', 2.0); dump(nt, "save-as name")
else:
    print("\n(no 'file:' prompt on screen)", flush=True)

for _ in range(3):
    scr = nt.text()
    if any('prg' in l for l in scr):
        nt.type('s\n', 1.2); dump(nt, "file type -> s")
    elif any('eplace' in l for l in scr):
        nt.type('y', 1.2); dump(nt, "replace -> y")
    else:
        break

for i in range(12):
    time.sleep(5)
    dump(nt, "waiting +%ds" % ((i + 1) * 5))
