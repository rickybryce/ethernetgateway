#!/usr/bin/env python3
"""Drive real file transfers *through* a gateway peer-dial bridge.

The peer-dial smoke test proves two modem ports can be bridged and that
bytes cross intact.  This one asks the harder question the gateway's own
docs raise: the bridge is a **pipe**, and a pipe carrying a file transfer
is the case where any translation the gateway does becomes corruption.

So: dial A -> B, answer, then hand the two PTY device ends to a real
sender and a real receiver and compare the bytes that come out.

Exit 0 = every protocol round-tripped byte for byte.
"""

import argparse
import filecmp
import os
import shutil
import subprocess
import sys
import time

try:
    import serial
except ImportError:
    sys.exit("pyserial not found: pip install pyserial")

PASS = FAIL = 0


def ok(msg):
    global PASS
    PASS += 1
    print(f"  PASS  {msg}", flush=True)


def bad(msg):
    global FAIL
    FAIL += 1
    print(f"  FAIL  {msg}", flush=True)


def drain(ser, secs=0.4):
    end = time.monotonic() + secs
    while time.monotonic() < end:
        if ser.in_waiting:
            ser.read(ser.in_waiting)
        time.sleep(0.02)


def send(ser, line):
    ser.write((line + "\r").encode())
    ser.flush()


def expect(ser, token, timeout):
    tb = token.encode()
    buf = bytearray()
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        n = ser.in_waiting
        if n:
            buf += ser.read(n)
            if tb in buf:
                return True
        else:
            time.sleep(0.03)
    got = bytes(buf).decode("ascii", "replace").replace("\r", "\\r").replace("\n", "\\n")
    print(f"        (wanted '{token}', got '{got}')", flush=True)
    return False


def dial(dev_a, dev_b):
    """Bring up the bridge.  Returns True when both ends report CONNECT."""
    a = serial.Serial(dev_a, 9600, timeout=0)
    b = serial.Serial(dev_b, 9600, timeout=0)
    try:
        drain(a); drain(b)
        send(b, "ATS0=0")       # answer manually, so we see the RING
        drain(b, 0.3)
        send(a, "ATD B@127.0.0.1")
        if not expect(b, "RING", 8):
            return False
        send(b, "ATA")
        if not expect(b, "CONNECT", 8):
            return False
        if not expect(a, "CONNECT", 8):
            return False
        # The modem prints CONNECT then switches to online mode; give both
        # port threads a moment to enter the bridge before a protocol starts
        # talking, or the first block lands in the AT parser.
        time.sleep(0.7)
        return True
    finally:
        a.close(); b.close()


def hangup(dev_a, dev_b):
    """Drop the call so the next protocol starts from a known state."""
    try:
        a = serial.Serial(dev_a, 9600, timeout=0)
        try:
            time.sleep(1.2)          # +++ needs a guard time of silence
            a.write(b"+++"); a.flush()
            time.sleep(1.2)
            send(a, "ATH")
            expect(a, "OK", 4)
        finally:
            a.close()
    except Exception as e:
        print(f"        (hangup: {e})", flush=True)
    time.sleep(0.5)
    for d in (dev_a, dev_b):
        try:
            s = serial.Serial(d, 9600, timeout=0)
            drain(s, 0.3)
            s.close()
        except Exception:
            pass


def raw(dev):
    subprocess.run(["stty", "-F", dev, "raw", "-echo", "9600"], check=False)


def run_pair(name, recv_cmd, send_cmd, dev_a, dev_b, workdir, src, timeout,
             line_mode=False, sender_may_linger=False):
    """Receiver on B, sender on A, each speaking to its own PTY device end.

    `line_mode` hands the device path to the tool (C-Kermit's `-l`) instead of
    binding it to stdin/stdout; `sender_may_linger` marks a tool whose sender
    does not exit on this rig even with no gateway present -- see the YMODEM
    note where the protocol table is built."""
    outdir = os.path.join(workdir, name)
    shutil.rmtree(outdir, ignore_errors=True)
    os.makedirs(outdir)
    raw(dev_a); raw(dev_b)

    if line_mode:
        fa = fb = None
        rp = subprocess.Popen(recv_cmd + [dev_b], cwd=outdir,
                              stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        time.sleep(1.5)
        sp = subprocess.Popen(send_cmd + [dev_a], cwd=workdir,
                              stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    else:
        fa = os.open(dev_a, os.O_RDWR | os.O_NOCTTY)
        fb = os.open(dev_b, os.O_RDWR | os.O_NOCTTY)
        rp = subprocess.Popen(recv_cmd, cwd=outdir, stdin=fb, stdout=fb,
                              stderr=subprocess.PIPE)
        time.sleep(0.6)          # let the receiver post its opening NAK/'C'
        sp = subprocess.Popen(send_cmd, cwd=workdir, stdin=fa, stdout=fa,
                              stderr=subprocess.PIPE)
    try:
        src_rc = rcv_rc = None
        try:
            src_rc = sp.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            sp.kill(); sp.wait(timeout=5)
            if not sender_may_linger:
                bad(f"{name}: sender timed out after {timeout}s")
        try:
            rcv_rc = rp.wait(timeout=25)
        except subprocess.TimeoutExpired:
            # A receiver reading a PTY the gateway still holds open never sees
            # EOF, so some of them sit after the last file rather than exiting.
            # That is the harness's business, not the protocol's -- the verdict
            # below is the file itself.
            rp.kill()
            rp.wait(timeout=5)
        def chat(p):
            h = p.stderr if p.stderr is not None else p.stdout
            return (h.read() or b"").decode("utf8", "replace").strip() if h else ""
        se, re_ = chat(sp), chat(rp)
        print(f"        sender rc={src_rc} receiver rc={rcv_rc}", flush=True)
        for tag, txt in (("send", se), ("recv", re_)):
            for line in txt.splitlines()[-4:]:
                print(f"        [{tag}] {line}", flush=True)
    finally:
        for fd in (fa, fb):
            if fd is not None:
                os.close(fd)

    got = os.path.join(outdir, os.path.basename(src))
    if not os.path.exists(got):
        # XMODEM has no filename on the wire; the receiver was told one.
        cand = [f for f in os.listdir(outdir)]
        if len(cand) == 1:
            got = os.path.join(outdir, cand[0])
        else:
            bad(f"{name}: no received file (dir has {cand})")
            return False
    if filecmp.cmp(src, got, shallow=False):
        ok(f"{name}: {os.path.getsize(src)} bytes round-tripped byte for byte")
        return True
    bad(f"{name}: received file differs "
        f"(sent {os.path.getsize(src)}, got {os.path.getsize(got)})")
    return False


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--dev-a", required=True)
    ap.add_argument("--dev-b", required=True)
    ap.add_argument("--work", required=True)
    ap.add_argument("--size", type=int, default=8192)
    ap.add_argument("--only", default=None, help="run just this protocol")
    args = ap.parse_args()

    # A payload with every byte value, protocol control bytes, and long runs
    # -- the shapes that break an escaping layer.
    blob = bytearray()
    while len(blob) < args.size:
        blob += bytes(range(256))
        blob += b"\x03\x18\x18\x18\x11\x13\x1a\x0d\x0a\xff\xff\x7f\x08" * 4
    blob = bytes(blob[:args.size])
    src = os.path.join(args.work, "payload.bin")
    with open(src, "wb") as f:
        f.write(blob)

    ccgms = os.path.expanduser("~/claude/punter-ccgms-interop")

    def punter_pair(dev_a, dev_b):
        """CCGMS' own Punter reference at both ends of the bridge.

        No file comparison: the harness generates and verifies its own payload
        and exits non-zero on a mismatch, so a clean pair of exit statuses is
        the whole result."""
        snd = os.path.join(ccgms, "ccgms-send")
        rcv = os.path.join(ccgms, "ccgms-recv")
        if not (os.path.isfile(snd) and os.path.isfile(rcv)):
            print("  SKIP  punter: CCGMS harness not built here", flush=True)
            return
        # MEASURED 2026-08-29: the CCGMS reference **segfaults against itself**
        # over a bare socat PTY pair with no gateway in the circuit (both ends
        # rc=-11, the receiver printing `punter_recv_string: sending "(null)"`).
        # Those binaries were only ever built to face our Rust implementation,
        # one side at a time; reference-against-reference is a use they do not
        # support, and no other Linux Punter client exists.  So this is a gap in
        # the oracle, not a result about the bridge -- and the bridge does not
        # know one protocol from another: the five above cross it byte for byte
        # carrying a payload with all 256 values in it.
        print("  SKIP  punter: the CCGMS reference cannot drive both ends "
              "(segfaults against itself with no gateway -- measured)", flush=True)
        return
        raw(dev_a); raw(dev_b)
        fa = os.open(dev_a, os.O_RDWR | os.O_NOCTTY)
        fb = os.open(dev_b, os.O_RDWR | os.O_NOCTTY)
        try:
            rp = subprocess.Popen([rcv], stdin=fb, stdout=fb, stderr=subprocess.PIPE)
            time.sleep(0.6)
            sp = subprocess.Popen([snd], stdin=fa, stdout=fa, stderr=subprocess.PIPE)
            rcs = rss = None
            try:
                rss = sp.wait(timeout=90)
            except subprocess.TimeoutExpired:
                sp.kill(); sp.wait(timeout=5)
            try:
                rcs = rp.wait(timeout=30)
            except subprocess.TimeoutExpired:
                rp.kill(); rp.wait(timeout=5)
            print(f"        sender rc={rss} receiver rc={rcs}", flush=True)
            for line in (rp.stderr.read() or b"").decode("utf8", "replace").splitlines()[-3:]:
                print(f"        [recv] {line}", flush=True)
            if rss == 0 and rcs == 0:
                ok("punter: CCGMS reference payload crossed the bridge intact")
            else:
                bad(f"punter: sender rc={rss}, receiver rc={rcs}")
        finally:
            os.close(fa); os.close(fb)
    protocols = [
        ("xmodem",    ["rx", "recv.bin"],      ["sx", src]),
        ("xmodem-1k", ["rx", "recv.bin"],      ["sx", "-k", src]),
        ("ymodem",    ["rb"],                  ["sb", src]),
        ("zmodem",    ["rz"],                  ["sz", src]),
        # C-Kermit drives the line itself; the device path is appended by
        # run_pair after `-l`.
        ("kermit",    ["kermit", "-b", "9600", "-i", "-r", "-l"],
                      ["kermit", "-b", "9600", "-i", "-s", src, "-l"]),
    ]
    # `sb`'s end-of-batch null header goes unacknowledged on a PTY rig because
    # `rb` exits as soon as the file is complete.  MEASURED with no gateway in
    # the circuit at all (bare socat pair: sb rc=124, same "Timeout on sector
    # ACK", bytes still identical), so it is lrzsz's behaviour on this rig and
    # not something the bridge does.  The file comparison is the verdict.
    LINGER = {"ymodem"}
    LINE_MODE = {"kermit"}
    if args.only:
        protocols = [p for p in protocols if p[0] == args.only]

    for name, recv_cmd, send_cmd in protocols:
        if shutil.which(recv_cmd[0]) is None or shutil.which(send_cmd[0]) is None:
            print(f"  SKIP  {name}: {recv_cmd[0]}/{send_cmd[0]} not installed", flush=True)
            continue
        print(f"[{name}]", flush=True)
        if not dial(args.dev_a, args.dev_b):
            bad(f"{name}: could not bring up the bridge")
            continue
        run_pair(name, recv_cmd, send_cmd, args.dev_a, args.dev_b,
                 args.work, src, timeout=120,
                 line_mode=name in LINE_MODE,
                 sender_may_linger=name in LINGER)
        hangup(args.dev_a, args.dev_b)

    if not args.only or args.only == "punter":
        print("[punter]", flush=True)
        if dial(args.dev_a, args.dev_b):
            punter_pair(args.dev_a, args.dev_b)
            hangup(args.dev_a, args.dev_b)
        else:
            bad("punter: could not bring up the bridge")

    print(f"\n{PASS} passed, {FAIL} failed.", flush=True)
    return 1 if FAIL else 0


if __name__ == "__main__":
    sys.exit(main())
