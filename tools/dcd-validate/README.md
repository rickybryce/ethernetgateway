# DCD / drive-carrier hardware validation (Option B)

Validates the gateway's `serial_X_drive_carrier` feature (commit `3fd25f0`):
when a call connects, the modem emulator asserts **DTR** as a carrier proxy;
it drops DTR on `NO CARRIER` / `ATH` / hangup / relay-link-loss, per `AT&C`.
A PC/USB adapter is a DTE and can't drive a DCD *output*, so the real cabling
crosses **DTR → the vintage machine's DCD input** (same trick tcpser uses).

This harness proves that DTR actually moves the way `AT&C` says it should,
using a second adapter as an observer. `socat` PTYs **cannot** carry
modem-control lines, so this must be done on real hardware.

> The gateway drives **DTR only** and never *reads* a status line. So a
> self-loopback jumper on the gateway's own adapter would be invisible — you
> need a *second* device to read the line. That's Option B.

---

## 1. Bill of materials

- **2 × USB-serial adapters** of the *same* signaling level (see the level
  warning below). Adapter **A** = the gateway's port; adapter **B** = observer.
- **2 jumper wires** (or a small breadboard): one for the carrier line, one for
  a common ground.
- A host running the gateway + this script (they can be the same PC; the two
  adapters are just two `/dev/ttyUSB*` nodes).

### Which adapters?
- **Best: RS-232 DB9 USB adapters.** They expose all nine pins including a real
  **DCD input (pin 1)**, so the observer watches the true carrier line and the
  wiring mirrors the real deployment exactly.
- **OK: TTL FTDI breakouts** — but many (e.g. the Sparkfun "FTDI Basic") only
  break out **DTR, RTS, CTS** and *no* DCD/DSR input. On those, **CTS is your
  only readable input**, so you wire A.DTR → B.CTS and watch CTS instead of CD.
  The script auto-reports which lines your adapter actually exposes.

### ⚠️ Level warning
Do **not** mix a TTL adapter with an RS-232 adapter directly — RS-232 swings
±5–12 V and will exceed a 3.3/5 V TTL pin's rating. Use **two TTL** adapters
*or* **two RS-232** adapters, wired directly. (For the eventual real vintage
machine, use whatever level converter that machine needs, e.g. an EZ232 for a
C64.)

---

## 2. Wiring

Pick the observer's input line based on what adapter B exposes (the script
tells you). Cross the gateway's DTR into it, and tie grounds together.

| From (Adapter A = gateway) | To (Adapter B = observer) | DB9 pins (A→B) |
|----------------------------|---------------------------|----------------|
| **DTR** (carrier proxy)    | **DCD** (preferred)       | 4 → 1          |
| — or DTR                   | **DSR**                   | 4 → 6          |
| — or DTR                   | **CTS** (TTL fallback)    | 4 → 8          |
| **GND**                    | **GND**                   | 5 → 5          |

Only **one** carrier wire is needed — DTR to whichever single input you chose —
plus the common ground. Nothing else needs to be connected for this test
(TX/RX are not exercised; we're only watching a control line).

---

## 3. Software setup

```sh
# pyserial (already present on this box as 3.5; here for a clean machine)
pip install pyserial          # or: sudo apt install python3-serial

# serial port permissions, if you get "Permission denied" opening the device:
sudo usermod -aG dialout $USER   # then log out and back in
```

Identify the two device nodes (unplug/replug one to tell them apart):

```sh
ls -l /dev/serial/by-id/ 2>/dev/null   # stable names, if present
dmesg | grep -i ttyUSB | tail          # e.g. ttyUSB0 (gateway), ttyUSB1 (observer)
```

---

## 4. Gateway config for the test

In the gateway's `egateway.conf` (the working dir you launch it from), set
**Port A** to modem mode on adapter A, with the carrier proxy on:

```ini
serial_a_enabled       = true
serial_a_mode          = modem
serial_a_port          = /dev/ttyUSB0     # adapter A
serial_a_baud          = 9600
serial_a_drive_carrier = true             # the feature under test
```

You can also toggle `serial_a_drive_carrier` live from the telnet per-port
modem menu (the **C** key) or the web/GUI checkbox — handy for the off/on
comparison in step 5.4 without restarting.

> Modem mode only: console-mode ports never build the modem state machine, so
> they don't drive carrier. Keep the port in `modem` mode for this test.

---

## 5. Run it

**Terminal 1 — observer (adapter B):**
```sh
cd tools/dcd-validate          # from the repo root
python3 dcd_observer.py /dev/ttyUSB1          # watches all lines
# or, if only CTS is wired/exposed:
python3 dcd_observer.py /dev/ttyUSB1 --watch cts
```
It prints the initial state, flags any lines your adapter can't read, then logs
every transition with a timestamp.

**Terminal 2 — the gateway** (from its config dir), then drive calls on
adapter A. Easiest driver is a terminal on adapter A talking to the modem
emulator (`cu -l /dev/ttyUSB0 -s 9600`, `minicom`, or `screen /dev/ttyUSB0
9600`). Type the AT commands below.

### 5.1  `AT&C1` (default) — carrier follows the call
1. At idle (no call): observer shows the carrier line **low**.
2. Place a call that connects — e.g. `ATDT <something-that-answers>` or
   `ATDT host:port` to a listening service → gateway prints `CONNECT`.
   → **observer logs `low -> HIGH`** at CONNECT.
3. Hang up: `+++` then `ATH` (or drop the remote end) → gateway prints
   `NO CARRIER`. → **observer logs `HIGH -> low`**.
4. Repeat connect → the line **re-asserts** on the next CONNECT.

### 5.2  `AT&C0` — carrier forced on for the port's lifetime
1. `AT&C0` → the carrier line should go/stay **HIGH** while the port is open,
   *regardless* of call state (it stays HIGH even at the idle AT prompt).
2. Connect and hang up: the line **stays HIGH** throughout (does not drop on
   `NO CARRIER`). This is the "&C0 = DCD always asserted" contract.
3. `AT&C1` to return to follow-the-call behavior.

### 5.3  Relay-link-loss (only if testing a slave)
If adapter A is on a **slave** gateway relaying to a master: with a call up
over the relay, kill the master (or sever the link). → the slave should drop
DTR so the attached machine sees hardware `NO CARRIER`, not just the in-band
result code. → **observer logs `HIGH -> low`** on link loss.

### 5.4  The safety guarantee — `drive_carrier = false` moves nothing
1. Set `serial_a_drive_carrier = false` (C key in the telnet menu, or edit +
   restart).
2. Repeat 5.1: connect, hang up, `AT&C0`/`AT&C1`. → the observer must log
   **zero transitions**. The off-path issues *no* modem-line calls, so a port
   without DCD wiring is byte-for-byte unaffected. This is the key
   "doesn't-break-anyone" claim.

---

## 6. Expected observer output (shape)

```
# Observing /dev/ttyUSB1 @ 9600  watching: cd, dsr, cts, ri
# initial: cd=low   dsr=low   cts=low   ri=low
# ---- watching for transitions (Ctrl-C to stop) ----
14:02:11 (+  6.44s)  CD   low  -> HIGH   [cd=HIGH  dsr=low   cts=low   ri=low ]   <- CONNECT (&C1)
14:02:29 (+ 24.10s)  CD   HIGH -> low    [cd=low   dsr=low   cts=low   ri=low ]   <- NO CARRIER
# stopped. 2 transition(s) observed over 31.2s.
```

(The `<- ...` annotations are added here for clarity; the script prints the
bracketed live snapshot.)

---

## 7. Troubleshooting

- **All lines read `n/a`** → the adapter doesn't expose input status lines over
  its driver. Use a DB9 RS-232 adapter, or fall back to metering/scoping the
  DTR pin on adapter A directly (DB9 pin 4 ↔ pin 5 GND: asserted ≈ +V,
  dropped ≈ −V).
- **Only CTS is readable** → normal for cheap TTL FTDI breakouts. Wire A.DTR →
  B.CTS and use `--watch cts`.
- **No transition on CONNECT** → confirm `serial_a_drive_carrier = true` is
  actually loaded (check the gateway log / `AT&V`), confirm the call really
  reached `CONNECT`, and confirm the jumper is on the right pins.
- **Line is inverted** (HIGH at idle, low on connect) → you likely wired to an
  active-low pin or crossed lines; watch the *transitions* rather than absolute
  polarity, and double-check the pin map. RS-232 asserted = positive voltage =
  pyserial `True`.
- **Permission denied opening the port** → `dialout` group (see §3).
- **Both adapters enumerate as the same name** → use `/dev/serial/by-id/…`
  stable paths.

---

## 8. Notes for this weekend

- Built for **Option B** (two adapters, crossover). If you only have one
  adapter handy, the one-adapter fallback is: meter/scope/LED on adapter A's
  DTR pin and run steps 5.1–5.4 watching the pin instead of this script.
- We can adjust the watched line / wiring once you see what your specific
  adapters expose — run the observer first; its startup lines tell you which
  inputs are usable before you commit to a pin.
- Deferred (not covered here): driving **RTS** instead of DTR (the optional
  second config key was not built — DTR only for now), and the `AT&D`
  read-direction (terminal→gateway DTR), which is out of scope for this
  feature.
