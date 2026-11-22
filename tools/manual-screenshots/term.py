#!/usr/bin/env python3
"""term.py <ready-file> <host> <port> [steps...]  -- run inside an xterm.

A minimal telnet client for screenshots: refuses every option (as a plain
client would), answers terminal detection with Backspace (ANSI), says Y to
colour, passes the welcome page, then plays the steps:
  w:SECONDS   wait
  s:TEXT      send TEXT (\\r is Enter)
Everything the gateway sends is written straight to this terminal, so what
the screenshot shows is the xterm's own rendering of the gateway's output.
When the steps are done it touches <ready-file> and waits to be killed."""
import socket, sys, threading, time, os

IAC, DONT, DO, WONT, WILL, SB, SE = 255, 254, 253, 252, 251, 250, 240
ready, host, port, steps = sys.argv[1], sys.argv[2], int(sys.argv[3]), sys.argv[4:]
sock = socket.create_connection((host, port))
out = sys.stdout.buffer

def reader():
    st = 0; buf = bytearray()
    while True:
        data = sock.recv(4096)
        if not data:
            return
        for b in data:
            if st == 0:
                if b == IAC: st = 1
                else: buf.append(b)
            elif st == 1:
                if b in (DO, DONT, WILL, WONT): st = b
                elif b == SB: st = SB
                elif b == IAC: buf.append(IAC); st = 0
                else: st = 0
            elif st in (DO, DONT, WILL, WONT):
                if st == DO: sock.sendall(bytes([IAC, WONT, b]))
                elif st == WILL: sock.sendall(bytes([IAC, DONT, b]))
                st = 0
            elif st == SB:
                if b == IAC: st = -1
            elif st == -1:
                st = 0 if b == SE else SB
        out.write(bytes(buf)); out.flush(); buf.clear()

threading.Thread(target=reader, daemon=True).start()
def send(b): sock.sendall(b)
time.sleep(1.5); send(b'\x08')      # terminal detection: Backspace = ANSI
time.sleep(1.0); send(b'Y')         # colour
time.sleep(1.0); send(b' ')         # past the welcome page
time.sleep(2.0)
for st in steps:
    k, _, v = st.partition(':')
    if k == 'w': time.sleep(float(v))
    elif k == 's': send(v.replace('\\r', '\r').encode())
open(ready, 'w').close()
while True:
    time.sleep(60)
