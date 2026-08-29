#!/usr/bin/env python3
"""Transfers to and from the gateway, driven through its own telnet menu.

The protocol gates in `src/` run our codec against a real lrzsz or C-Kermit
peer, which proves the *protocol* code.  They do not go through the product:
the menus, the file picker, the protocol chooser and the session's own byte
path.  This drives all of that -- log in over telnet exactly as a user does,
pick Download or Upload, choose the protocol, and then hand the live socket to
a real `sx`/`rx`/`sb`/`rb`/`sz`/`rz` and compare the bytes.

Exit 0 = every direction of every protocol matched byte for byte.
"""

import argparse
import filecmp
import os
import re
import shutil
import socket
import subprocess
import sys
import time

IAC, DONT, DO, WONT, WILL, SB, SE = 255, 254, 253, 252, 251, 250, 240
PASS = FAIL = 0


def ok(m):
    global PASS
    PASS += 1
    print(f"  PASS  {m}", flush=True)


def bad(m):
    global FAIL
    FAIL += 1
    print(f"  FAIL  {m}", flush=True)


class Session:
    """Telnet session that refuses every option, so the wire stays 8-bit clean."""

    def __init__(self, host, port):
        self.s = socket.create_connection((host, port), timeout=10)
        self.s.settimeout(0.4)
        self.buf = b""
        self.log = b""

    def _pump(self):
        try:
            data = self.s.recv(4096)
        except socket.timeout:
            return
        if not data:
            raise EOFError("peer closed")
        out, i = bytearray(), 0
        while i < len(data):
            b = data[i]
            if b != IAC:
                out.append(b); i += 1; continue
            if i + 1 >= len(data):
                break
            cmd = data[i + 1]
            if cmd in (DO, DONT, WILL, WONT):
                if i + 2 >= len(data):
                    break
                reply = WONT if cmd == DO else (DONT if cmd == WILL else None)
                if reply is not None:
                    self.s.sendall(bytes([IAC, reply, data[i + 2]]))
                i += 3
            elif cmd == SB:
                j = data.find(bytes([IAC, SE]), i)
                i = (j + 2) if j != -1 else len(data)
            elif cmd == IAC:
                out.append(IAC); i += 2
            else:
                i += 2
        self.buf += bytes(out)
        self.log += bytes(out)

    def wait(self, pattern, timeout=20):
        end = time.time() + timeout
        while time.time() < end:
            if pattern in self.buf:
                return True
            try:
                self._pump()
            except EOFError:
                return pattern in self.buf
        return False

    def drain(self, secs=1.5):
        end = time.time() + secs
        while time.time() < end:
            try:
                self._pump()
            except EOFError:
                return

    def send(self, d):
        self.s.sendall(d.encode() if isinstance(d, str) else d)

    def clear(self):
        self.buf = b""

    def text(self):
        return re.sub(r"\x1b\[[0-9;?]*[A-Za-z]", "", self.log.decode("latin-1"))

    def close(self):
        try:
            self.s.close()
        except Exception:
            pass


def login(host, port):
    s = Session(host, port)
    if not s.wait(b"BACKSPACE", timeout=20):
        raise SystemExit(f"no terminal-detection prompt; got:\n{s.text()[-800:]}")
    s.send(b"\x08")                 # 0x08 -> ANSI
    s.wait(b"color", timeout=10)
    s.send(b"N")                    # plain text, so the screens are easy to match
    s.drain(2.0)
    return s


def to_menu(s):
    """Main menu -> File Transfer, with IAC escaping turned off.

    The gateway sets IAC escaping from whether the client negotiated telnet
    options -- a real client (PuTTY, C-Kermit) gets 0xFF doubled, a raw TCP
    client gets a transparent stream.  This driver answers negotiation, so it
    is treated as a real client and the escaping comes on; `lrzsz` does not
    speak telnet and would see the doubled bytes as corruption.  Pressing `I`
    is what a user in the same position does, and it exercises the toggle."""
    s.clear(); s.send("F\r"); s.drain(2.0)
    if b"Upload" not in s.buf and b"UPLOAD" not in s.buf.upper():
        raise RuntimeError(f"no file-transfer menu:\n{s.text()[-600:]}")
    if b"IAC escaping [ON]" in s.buf:
        s.clear(); s.send("I\r"); s.drain(2.0)
        if b"IAC escaping [OFF]" not in s.buf:
            raise RuntimeError(f"could not turn IAC escaping off:\n{s.text()[-600:]}")


def pick_file_number(s, name):
    """The download picker lists files by number; find the one we want."""
    txt = re.sub(r"\x1b\[[0-9;?]*[A-Za-z]", "", s.buf.decode("latin-1"))
    for line in txt.splitlines():
        m = re.match(r"\s*(\d+)[).\s]\s*(\S+)", line)
        if m and m.group(2).upper() == name.upper():
            return m.group(1)
    return None


TRACE = os.environ.get("TRACE_HANDOFF")


def handoff_traced(s, cmd, cwd, timeout):
    """Same as handoff, but relays through Python so the wire can be logged."""
    import selectors
    s.s.settimeout(None); s.s.setblocking(True)
    p = subprocess.Popen(cmd, cwd=cwd, stdin=subprocess.PIPE,
                         stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    sel = selectors.DefaultSelector()
    sel.register(s.s, selectors.EVENT_READ, "net")
    sel.register(p.stdout, selectors.EVENT_READ, "prog")
    end = time.time() + timeout
    log = []
    while time.time() < end and p.poll() is None:
        for key, _ in sel.select(timeout=0.5):
            if key.data == "net":
                d = s.s.recv(4096)
                if d:
                    log.append(("gw->prog", d[:24]))
                    try:
                        p.stdin.write(d); p.stdin.flush()
                    except Exception:
                        pass
            else:
                d = p.stdout.read1(4096)
                if d:
                    log.append(("prog->gw", d[:24]))
                    s.s.sendall(d)
    try:
        rc = p.wait(timeout=5)
    except subprocess.TimeoutExpired:
        p.kill(); rc = None
    print("        --- wire trace (last 12 events, first 24 bytes each) ---", flush=True)
    for tag, d in log[-12:]:
        print(f"        {tag}: {d.hex(' ')}", flush=True)
    return rc, (p.stderr.read() or b"").decode("utf8", "replace").strip()


def handoff(s, cmd, cwd, timeout):
    if TRACE:
        return handoff_traced(s, cmd, cwd, timeout)
    """Hand the live socket to a real transfer program and wait for it."""
    s.s.settimeout(None)
    s.s.setblocking(True)
    fd = s.s.fileno()
    p = subprocess.Popen(cmd, cwd=cwd, stdin=fd, stdout=fd, stderr=subprocess.PIPE)
    try:
        rc = p.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        p.kill(); p.wait(timeout=5); rc = None
    err = (p.stderr.read() or b"").decode("utf8", "replace").strip()
    return rc, err


def settle(s, quiet=1.0):
    """Consume the gateway's pre-transfer text before the sender starts.

    The gateway prints "Start transfer within N seconds" and an ESC-to-cancel
    line, then begins polling.  A real terminal emulator has already drawn and
    consumed that text by the time the user starts their transfer; this driver
    has not, so without draining it the bytes sit in the socket and the sender
    reads them as protocol input.

    That is not cosmetic.  MEASURED on the XMODEM upload: `sx` took a stray
    text byte for an early ACK and ran one ACK ahead for the whole transfer,
    so at the end it read block 32's real ACK as the acknowledgement of its
    EOT, reported "Transfer complete" and exited -- before the gateway's
    Forsberg EOT-confirmation NAK arrived.  The gateway was right, the sender
    was satisfied, and the file was silently never written.  Draining any poll
    bytes along with the text costs nothing: the gateway re-polls."""
    s.wait(b"Start transfer", timeout=10)
    s.drain(quiet)


DOWNLOAD_KEY = {"xmodem": "X", "xmodem-1k": "1", "ymodem": "Y", "zmodem": "Z"}
DOWNLOAD_RECV = {
    "xmodem":    ["rx", "got.bin"],
    "xmodem-1k": ["rx", "got.bin"],
    "ymodem":    ["rb"],
    "zmodem":    ["rz"],
}
UPLOAD_KEY = {"xmodem": "X", "ymodem": "X", "zmodem": "Z"}
UPLOAD_SEND = {
    "xmodem": ["sx"],
    "ymodem": ["sb"],
    "zmodem": ["sz"],
}


def download(host, port, proto, payload, outroot):
    """The gateway SENDS: menu -> D -> file -> protocol, then we receive."""
    name = os.path.basename(payload)
    out = os.path.join(outroot, "dl-" + proto)
    shutil.rmtree(out, ignore_errors=True); os.makedirs(out)
    s = login(host, port)
    try:
        to_menu(s)
        s.clear(); s.send("D\r"); s.drain(2.0)
        num = pick_file_number(s, name)
        if num is None:
            bad(f"download/{proto}: {name} is not in the picker:\n{s.text()[-500:]}")
            return
        s.clear(); s.send(num + "\r"); s.drain(2.0)
        if not (b"rotocol" in s.buf or b"ROTOCOL" in s.buf):
            bad(f"download/{proto}: no protocol prompt:\n{s.text()[-500:]}")
            return
        s.send(DOWNLOAD_KEY[proto] + "\r")
        settle(s)
        rc, err = handoff(s, DOWNLOAD_RECV[proto], out, 120)
        got = os.path.join(out, name if proto in ("ymodem", "zmodem") else "got.bin")
        if not os.path.exists(got):
            files = os.listdir(out)
            if len(files) == 1:
                got = os.path.join(out, files[0])
            else:
                bad(f"download/{proto}: nothing received (rc={rc}) {files} {err[-200:]}")
                return
        if filecmp.cmp(payload, got, shallow=False):
            ok(f"download/{proto}: gateway sent {os.path.getsize(payload)} bytes intact")
        else:
            bad(f"download/{proto}: bytes differ (got {os.path.getsize(got)})")
    finally:
        s.close()


def upload(host, port, proto, payload, xferdir):
    """The gateway RECEIVES: menu -> U -> protocol, then we send."""
    name = f"up-{proto}.bin"
    staged = os.path.join(os.path.dirname(payload), name)
    shutil.copyfile(payload, staged)
    landed = os.path.join(xferdir, name)
    if os.path.exists(landed):
        os.remove(landed)
    s = login(host, port)
    try:
        to_menu(s)
        s.clear(); s.send("U\r"); s.drain(2.0)
        if b"Filename" not in s.buf:
            bad(f"upload/{proto}: no filename prompt:\n{s.text()[-500:]}")
            return
        # Filename first, then the protocol screen -- the same order as
        # download (file, then protocol), which is deliberate in the product.
        s.clear(); s.send(name + "\r"); s.drain(2.5)
        if not (b"rotocol" in s.buf or b"ROTOCOL" in s.buf):
            bad(f"upload/{proto}: no protocol prompt after the filename:\n{s.text()[-500:]}")
            return
        s.send(UPLOAD_KEY[proto] + "\r")
        settle(s)
        rc, err = handoff(s, UPLOAD_SEND[proto] + [staged],
                          os.path.dirname(payload), 120)
        # The gateway writes the file after the transfer closes; give it a moment.
        for _ in range(40):
            if os.path.exists(landed) and os.path.getsize(landed) >= os.path.getsize(payload):
                break
            time.sleep(0.25)
        if not os.path.exists(landed):
            s.drain(2.0)
            bad(f"upload/{proto}: nothing landed in the transfer dir (rc={rc})")
            print(f"        sender said: {err[-160:]!r}", flush=True)
            print(f"        session tail: {s.text()[-700:]!r}", flush=True)
            print(f"        dir now: {sorted(os.listdir(xferdir))}", flush=True)
            return
        if filecmp.cmp(payload, landed, shallow=False):
            ok(f"upload/{proto}: gateway received {os.path.getsize(payload)} bytes intact")
        else:
            bad(f"upload/{proto}: bytes differ (landed {os.path.getsize(landed)})")
    finally:
        s.close()


def punter(host, port, direction, xferdir, work):
    """Punter through the menu, with the CCGMS reference as the peer.

    There is no Linux Punter client, so `ccgms-recv` / `ccgms-send` stand in --
    the same reference the `src/punter.rs` gates use.  They carry a fixed
    300-byte payload of their own (`i * 7 + 1`) and check it themselves, so the
    file on the gateway side has to be exactly that and the verdict is the
    harness's exit status plus, on upload, the bytes that landed."""
    ccgms = os.path.expanduser("~/claude/punter-ccgms-interop")
    expect = bytes(((i * 7 + 1) & 0xFF) for i in range(300))
    name = "PUNTER.BIN" if direction == "download" else "up-punter.bin"
    binary = os.path.join(ccgms, "ccgms-recv" if direction == "download" else "ccgms-send")
    if not os.path.isfile(binary):
        print(f"  SKIP  {direction}/punter: CCGMS harness not built here", flush=True)
        return

    if direction == "download":
        with open(os.path.join(xferdir, name), "wb") as f:
            f.write(expect)
    landed = os.path.join(xferdir, name)
    if direction == "upload" and os.path.exists(landed):
        os.remove(landed)

    s = login(host, port)
    try:
        to_menu(s)
        if direction == "download":
            s.clear(); s.send("D\r"); s.drain(2.0)
            num = pick_file_number(s, name)
            if num is None:
                bad(f"download/punter: {name} not in the picker")
                return
            s.clear(); s.send(num + "\r"); s.drain(2.0)
            s.send("P\r")
        else:
            s.clear(); s.send("U\r"); s.drain(2.0)
            s.clear(); s.send(name + "\r"); s.drain(2.5)
            s.send("P\r")
        # No extra drain here.  Punter's sender opens the handshake straight
        # after the preamble, and a one-second drain ate that opening code --
        # the reference then hit its handshake timeout, and it *segfaults* on a
        # timeout (measured: it does so on pipes and sockets alike with nothing
        # sent at all), which reads like a transfer failure rather than a
        # harness one.  Waiting for the last preamble line is enough.
        settle(s, quiet=0.0)
        rc, err = handoff(s, [binary], work, 120)
        if direction == "download":
            if rc == 0:
                ok("download/punter: the CCGMS reference accepted 300 bytes from the gateway")
            else:
                bad(f"download/punter: reference rc={rc} {err[-200:]}")
        else:
            for _ in range(40):
                if os.path.exists(landed) and os.path.getsize(landed) >= len(expect):
                    break
                time.sleep(0.25)
            if not os.path.exists(landed):
                bad(f"upload/punter: nothing landed (rc={rc}) {err[-200:]}")
            elif open(landed, "rb").read() == expect:
                ok("upload/punter: gateway received the reference's 300 bytes intact")
            else:
                bad(f"upload/punter: bytes differ ({os.path.getsize(landed)} landed)")
    finally:
        s.close()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, required=True)
    ap.add_argument("--xferdir", required=True)
    ap.add_argument("--work", required=True)
    ap.add_argument("--size", type=int, default=4096)
    ap.add_argument("--only", default=None)
    args = ap.parse_args()

    blob = bytearray()
    while len(blob) < args.size:
        blob += bytes(range(256))
        blob += b"\x03\x18\x18\x18\x11\x13\x1a\x0d\x0a\xff\xff\x7f\x08" * 4
    blob = bytes(blob[: args.size])
    payload = os.path.join(args.xferdir, "PAYLOAD.BIN")
    with open(payload, "wb") as f:
        f.write(blob)
    local = os.path.join(args.work, "PAYLOAD.BIN")
    with open(local, "wb") as f:
        f.write(blob)

    for proto in ["xmodem", "xmodem-1k", "ymodem", "zmodem"]:
        if args.only and args.only != proto:
            continue
        print(f"[download/{proto}]", flush=True)
        try:
            download(args.host, args.port, proto, payload, args.work)
        except Exception as e:
            bad(f"download/{proto}: {e}")

    if not args.only or args.only == "punter":
        print("[download/punter]", flush=True)
        try:
            punter(args.host, args.port, "download", args.xferdir, args.work)
        except Exception as e:
            bad(f"download/punter: {e}")

    for proto in ["xmodem", "ymodem", "zmodem"]:
        if args.only and args.only != proto:
            continue
        print(f"[upload/{proto}]", flush=True)
        try:
            upload(args.host, args.port, proto, local, args.xferdir)
        except Exception as e:
            bad(f"upload/{proto}: {e}")

    if not args.only or args.only == "punter":
        print("[upload/punter]", flush=True)
        try:
            punter(args.host, args.port, "upload", args.xferdir, args.work)
        except Exception as e:
            bad(f"upload/punter: {e}")

    print(f"\n{PASS} passed, {FAIL} failed.", flush=True)
    return 1 if FAIL else 0


if __name__ == "__main__":
    sys.exit(main())
