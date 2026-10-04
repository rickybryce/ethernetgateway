"""Drive NovaTerm 9.6c inside VICE, for the Punter interop gate.

Two channels, because neither alone is enough:

* **Reading** is VICE's remote monitor.  NovaTerm relocates its screen -- it is
  at `$8C00`, not `$0400` -- so `vicemon.Mon.screen_base` follows `$D018` and
  CIA2 `$DD00` rather than assuming the default and reporting stale memory.
* **Typing** is X11 XTEST into the VICE window.  NovaTerm scans the CIA1
  keyboard matrix itself and never reads the KERNAL buffer, so VICE's own
  `keybuf` monitor command reaches it not at all -- measured, by injecting `t`
  at the main menu and watching nothing happen.

The menu **remembers where it was left**, so nothing here counts keypresses
from an assumed top: every move reads the current selection first.  Assuming it
started at the top is how an early run walked into "Exit terminal / Are you
sure?".
"""
import time
from vicemon import Mon
from c64keys import Keys

# Main-menu items, in screen order, with the row each occupies.
MAIN_ROWS = range(9, 17)
MAIN_ITEMS = [
    "terminal mode", "dial a number", "configuration", "disk operations",
    "buffer menu", "device settings", "utility modules", "exit terminal",
]

class NovaTerm:
    def __init__(self, monport=29876):
        self.keys = Keys()
        self.keys.focus()
        self.monport = monport

    # ── reading ──────────────────────────────────────────────
    def _mon(self):
        return Mon(self.monport)

    def cells(self):
        """The 1000 raw screen codes, from wherever the VIC is really looking."""
        m = self._mon()
        v = m.peek(m.screen_base(), 1000)
        m.resume(); m.k.close()
        return v

    def text(self, v=None):
        """The screen as 25 lines of plain text."""
        v = v if v is not None else self.cells()
        def ch(c):
            c &= 0x7F
            if 1 <= c <= 26: return chr(ord('a') + c - 1)
            if c == 0x20 or c == 0x60: return ' '
            if 0x30 <= c <= 0x3F: return chr(c)
            if 0x21 <= c <= 0x2F: return chr(c)
            return '.'
        return ["".join(ch(v[r*40+c]) for c in range(40)).rstrip() for r in range(25)]

    def selected(self, rows=MAIN_ROWS):
        """Which menu row is highlighted.

        The selection is a wide reverse-video bar; an unselected row carries a
        single reversed cell, its hotkey letter.  Counting reversed cells tells
        them apart, where testing bit 7 alone does not -- the whole menu box is
        drawn reversed.
        """
        v = self.cells()
        best, best_n = None, 1
        for i, r in enumerate(rows):
            n = sum(1 for c in range(40) if v[r*40+c] & 0x80)
            if n > best_n:
                best, best_n = i, n
        return best

    # ── typing ───────────────────────────────────────────────
    def press(self, key, settle=0.35):
        self.keys.focus(); self.keys.key(key); time.sleep(settle)

    def type(self, s, settle=0.8):
        self.keys.focus(); self.keys.type(s); time.sleep(settle)

    def inst_del(self, settle=3.0):
        """Press the C64's INST/DEL key, which sends PETSCII 0x14.

        **Not `type(chr(0x14))`.**  That looks like it sends the byte and does
        nothing at all: `XK.string_to_keysym` has no symbol for a raw control
        character, so the lookup yields keycode 0 and no key is pressed.  The
        gateway then detects the terminal from whatever byte arrives next and
        calls it ASCII -- which is a wrong answer that looks like a wrong
        setting.  X has a name for this key; use it.
        """
        self.press('BackSpace', settle)

    def hangup(self, tries=4):
        """Drop the call and **prove** we are in command mode.

        Reading scrollback for "no carrier" was guesswork twice over: the
        message only appears when there was a call to drop, and NovaTerm's own
        "Hanging up..." is a statement of intent, not of outcome -- on a socat
        PTY it drops DTR, which a PTY does not carry, so nothing happens.

        So this asks the modem instead: `AT` must be answered with `OK`.  A
        positive control, not the absence of a symptom.
        """
        def responds():
            self.type("at\n", 2.5)
            lines = [l.strip() for l in self.text() if l.strip()]
            return bool(lines) and lines[-1] == "ok"

        for attempt in range(tries):
            if attempt == 0:
                self.keys.focus()
                self.keys.combo("Tab", "h")     # NovaTerm's own hangup
                time.sleep(3.0)
            else:
                self.type("+++", 2.5)           # Hayes in-band escape, needs
                self.type("ath\n", 3.0)        # no DTR
            if responds():
                return True
        return False

    def dial(self, number="ethernetgateway", settle=9.0):
        """Hang up, then dial, from a state we have checked rather than assumed."""
        if not self.hangup():
            raise RuntimeError("could not get to 'no carrier'; screen: %r"
                               % [l for l in self.text() if l.strip()][-4:])
        self.type("atdt %s\n" % number, settle)

    def at_menu(self):
        """Whether NovaTerm is really showing its MAIN menu.

        A highlight alone is not enough: `selected()` looks for a wide
        reverse-video run in the menu's rows, and any other screen with one
        there -- a transfer dialog left up by an aborted run, say -- reports a
        bogus selection, after which the navigation moves the wrong number of
        rows on the wrong screen.  So check the menu's own words as well.
        """
        text = self.text()
        looks_right = any("erminal mode" in l for l in text) and \
                      any("onfiguration" in l for l in text)
        return looks_right and self.selected() is not None

    def ensure_terminal_mode(self):
        """Get to terminal mode from wherever we are, without assuming.

        `C= Z` returns to the main menu from anywhere in terminal mode, so a
        screen left behind by an aborted transfer is recoverable without
        restarting NovaTerm.
        """
        if self.at_menu():
            self.choose("terminal mode")
            time.sleep(2.0)
            return
        # Not terminal mode and not the menu: shake off whatever dialog is up.
        for _ in range(3):
            self.press("Return", 1.0)
        time.sleep(1.5)
        if self.at_menu():
            self.choose("terminal mode")
            time.sleep(2.0)

    def choose(self, name, rows=MAIN_ROWS, items=MAIN_ITEMS):
        """Move the highlight onto `name` and press RETURN."""
        want = items.index(name)
        here = self.selected(rows)
        if here is None:
            raise RuntimeError("no menu selection visible; is NovaTerm at a menu?")
        step = "Down" if want > here else "Up"
        for _ in range(abs(want - here)):
            self.press(step, 0.3)
        got = self.selected(rows)
        if got != want:
            raise RuntimeError("wanted %r (row %d), highlight is on row %d" % (name, want, got))
        self.press("Return", 1.5)


def past_welcome(nt, timeout=15.0):
    """Press SPACE past the gateway's welcome page, if it shows.

    A gateway shows it for its first seven days (since e035aba, 2026-09-19),
    between the colour question and the main menu, and it waits for a key.
    Unexpected, it took the next keystroke -- the Telnet Gateway's `t`, or the
    `f` for File Transfer -- and everything after went to the wrong screen.

    Polled until either the page or the main menu shows: one look a fixed time
    after the colour answer could come before a page that takes ~2.5 s to draw
    at 2400 baud.  Matched on lowercase words, since NovaTerm's screen text
    turns capitals into dots ("upload/download" is the main menu's File
    Transfer line).  Lives here, once, because every script that reaches the
    menu needs it, and two copies of a screen-scraper drift.
    """
    end = time.time() + timeout
    while time.time() < end:
        scr = nt.text()
        if any('or the main menu' in l or 'his page stops appearing' in l for l in scr):
            print('  welcome page -> SPACE', flush=True)
            nt.type(' ', 3.0)
            return
        if any('upload/download' in l for l in scr):
            return
        time.sleep(1.0)
