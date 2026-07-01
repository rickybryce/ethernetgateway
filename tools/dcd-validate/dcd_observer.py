#!/usr/bin/env python3
"""DCD/carrier observer for validating the gateway's drive-carrier feature.

Option B setup (two USB-serial adapters):

    Adapter A  = the GATEWAY's serial port (drives DTR as a carrier proxy).
    Adapter B  = this OBSERVER (reads the line the gateway toggles).

    Wire:  A.DTR  ->  B.DCD   (or B.DSR, or B.CTS — whichever B exposes)
           A.GND  ->  B.GND

This script opens adapter B read-only for modem-status purposes, polls the
CD / DSR / CTS / RI input lines, and prints a timestamped line every time any
of them changes.  You then drive calls on the gateway (CONNECT / NO CARRIER /
ATH / &C0 / &C1 / drive_carrier off) and watch which line follows.

The gateway drives *DTR only*; it never reads status lines.  So this observer
is the only thing that can "see" the carrier proxy move — a self-loopback
jumper on the gateway's own adapter would be invisible.

Usage:
    python3 dcd_observer.py /dev/ttyUSB1
    python3 dcd_observer.py /dev/ttyUSB1 --watch cts      # only care about CTS
    python3 dcd_observer.py /dev/ttyUSB1 --interval 20    # faster polling (ms)

Requires: pyserial  (pip install pyserial  /  apt install python3-serial)
Exit with Ctrl-C; a transition summary is printed on the way out.
"""

import argparse
import sys
import time

try:
    import serial  # pyserial
except ImportError:
    sys.exit(
        "pyserial not found. Install it with:\n"
        "    pip install pyserial\n"
        "  or\n"
        "    sudo apt install python3-serial"
    )

# The input (readable) modem-status lines pyserial exposes, and the pyserial
# property that reads each. DCD is the "real" carrier line; DSR/CTS are the
# common fallbacks when a cheap TTL breakout doesn't bring DCD out.
LINES = {
    "cd": "cd",    # Data Carrier Detect  (DB9 pin 1) — the true carrier line
    "dsr": "dsr",  # Data Set Ready       (DB9 pin 6)
    "cts": "cts",  # Clear To Send        (DB9 pin 8) — often the only TTL input
    "ri": "ri",    # Ring Indicator       (DB9 pin 9)
}


def read_line(ser, attr):
    """Read one status line; return True/False, or None if unsupported."""
    try:
        return bool(getattr(ser, attr))
    except (OSError, serial.SerialException, AttributeError):
        return None


def snapshot(ser, watch):
    return {name: read_line(ser, attr) for name, attr in watch.items()}


def fmt_state(v):
    if v is None:
        return "n/a"
    return "HIGH" if v else "low "


def main():
    ap = argparse.ArgumentParser(
        description="Observe DCD/DSR/CTS transitions driven by the gateway's DTR carrier proxy."
    )
    ap.add_argument("port", help="observer serial device, e.g. /dev/ttyUSB1")
    ap.add_argument("--baud", type=int, default=9600,
                    help="baud to open at (irrelevant to status lines; default 9600)")
    ap.add_argument("--interval", type=int, default=50,
                    help="poll interval in milliseconds (default 50)")
    ap.add_argument("--watch", choices=list(LINES) + ["all"], default="all",
                    help="which input line to watch (default: all)")
    args = ap.parse_args()

    watch = LINES if args.watch == "all" else {args.watch: LINES[args.watch]}

    try:
        ser = serial.Serial(args.port, args.baud, timeout=0)
    except (OSError, serial.SerialException) as e:
        sys.exit(f"Could not open {args.port}: {e}\n"
                 f"(In the 'dialout' group? try: sudo usermod -aG dialout $USER, then re-login.)")

    # Don't back-drive the observer's own control outputs — its DTR/RTS go
    # nowhere in this setup, but keep them de-asserted so nothing is ambiguous.
    try:
        ser.dtr = False
        ser.rts = False
    except (OSError, serial.SerialException):
        pass

    interval = max(args.interval, 1) / 1000.0
    start = time.monotonic()

    def stamp():
        return f"{time.strftime('%H:%M:%S')} (+{time.monotonic() - start:7.2f}s)"

    prev = snapshot(ser, watch)
    print(f"# Observing {args.port} @ {args.baud}  watching: {', '.join(watch)}")
    unsupported = [n for n, v in prev.items() if v is None]
    if unsupported:
        print(f"# NOTE: these lines read n/a (adapter likely doesn't expose them): "
              f"{', '.join(unsupported)}")
        supported = [n for n, v in prev.items() if v is not None]
        if supported:
            print(f"#       usable input line(s) on this adapter: {', '.join(supported)} "
                  f"— wire the gateway's DTR to one of those.")
        else:
            print("#       WARNING: NO readable status lines on this adapter. "
                  "Use a DB9 RS-232 USB adapter, or meter/scope the DTR pin instead.")
    print(f"# initial: " + "  ".join(f"{n}={fmt_state(v)}" for n, v in prev.items()))
    print("# ---- watching for transitions (Ctrl-C to stop) ----")
    sys.stdout.flush()

    transitions = 0
    try:
        while True:
            cur = snapshot(ser, watch)
            for name in watch:
                if cur[name] is None or prev[name] is None:
                    continue
                if cur[name] != prev[name]:
                    transitions += 1
                    arrow = "low  -> HIGH" if cur[name] else "HIGH -> low "
                    print(f"{stamp()}  {name.upper():>3}  {arrow}   "
                          f"[{'  '.join(f'{n}={fmt_state(v)}' for n, v in cur.items())}]")
                    sys.stdout.flush()
            prev = cur
            time.sleep(interval)
    except KeyboardInterrupt:
        print(f"\n# stopped. {transitions} transition(s) observed over "
              f"{time.monotonic() - start:.1f}s.")
    finally:
        ser.close()


if __name__ == "__main__":
    main()
