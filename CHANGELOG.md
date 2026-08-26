# Changelog

All notable changes to **ethernetgateway** are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.9.6] - Unreleased

### Added

- **A console bridge says so when the port rewrites the erase key, before you
  connect.**  `serial_a_backspace` / `serial_b_backspace` fold Backspace and
  Delete to the byte the device edits with, which is what the setting is for
  and is right for typing.  It is wrong for a file transfer, and a console
  bridge carries those: the CP/M-side `PCGET` / `PCPUT` utilities run XMODEM
  over the console line, so the blocks travel through the same fold.  The
  failure is quiet and does not resolve itself -- only blocks containing 0x08,
  0x7F or 0x14 are altered, they fail their check, the sender resends and the
  fold reproduces the identical corruption, so the transfer stalls rather than
  failing outright, and nothing on screen names an erase key.

  The Serial Gateway now says this on the connect screen, above the Y/N prompt
  so it is read while there is still something to do about it, and **only when
  the port actually folds** -- a notice that appears whatever the setting says
  is one an operator learns to skip.  The gateway does not try to detect the
  transfer itself: it is a pipe for one, the two ends run the protocol between
  them, and nothing declares it.  The setting is the answer, so the operator is
  told the setting is on.

  Shown for a **remote** console port on a slave too.  The slave folds in its
  own process, so a master had no way to know -- and a relayed port picked from
  a master is the case this setting was first reported for.  `serial-register`
  now carries the erase key as an optional third token, the way it grew the mode
  in 0.9.5: a slave older than the addition sends nothing, and the master then
  says nothing rather than guessing, because "we were not told" and "it is set
  to pass through" have to stay different answers.

### Changed

- **The erase-key fold is a console-mode setting again; the modem path no longer
  folds.**  `serial_a_backspace` / `serial_b_backspace` shipped in 0.9.5 applying
  to both a console bridge and the modem path.  The modem half is withdrawn: a
  console bridge is a link the gateway owns end to end, while a modem-mode port
  is usually a *pipe* -- `ATDT <host>` to a BBS, a peer dial, a relayed port --
  where two other parties run whatever protocol crosses it and the gateway is
  only carrying bytes.  Rewriting a byte in a pipe corrupts any file transfer
  containing it, and does so identically on every retry, so no checksum recovers
  it and the transfer stalls rather than failing outright.  The gateway cannot
  tell a keystroke from a payload byte there, and the several attempts to infer
  it were less trustworthy than not rewriting at all.

  What this means in practice: a port in **console** mode behaves exactly as it
  does in 0.9.5 -- the setting is unchanged, it is still on all three surfaces,
  and the Altair erase-key fix it was written for still works.  A port in
  **modem** mode passes 0x08, 0x7F and 0x14 through untouched whatever the
  setting says, which is what the gateway did before 0.9.5.  `ATZ` and `AT&F` no
  longer carry an erase-key state.  Kermit-server mode never folded and still
  does not.

  The PETSCII translator on `ATDT <host>` is unchanged and carries the same
  caveat it always has: `AT+PETSCII=0` before sending a binary over a dialled
  connection.

- **The erase key is greyed out when the port is not in console mode.**  It only
  ever applied to a console bridge -- a modem port passes 0x08, 0x7F and 0x14
  through, and on a Kermit-server wire they are packet data whose rewriting
  would corrupt a transfer -- but the web page and the desktop editor showed a
  live control in all three modes, explaining the restriction in a hint beside
  it.  An operator could set it, watch it save, and get no change on the wire.
  Both now grey it outside console mode, and the web page re-enables it the
  moment the Mode select changes; telnet already omitted the row, which is the
  same rule on a screen with no way to draw "greyed".  **The stored value is
  kept** -- a disabled control is not submitted, and the serial save path skips
  an absent field rather than reading it as a cleared one -- so switching a port
  to modem mode and back does not lose the erase key.

- **The erase key moved next to Mode, out of the Hayes AT block.**  It was under
  "Hayes AT Saved State" on the web page and the desktop editor, and it belonged
  there for exactly one release: 0.9.5 made it modem state, reloaded by `ATZ` and
  cleared by `AT&F`.  Withdrawing the modem-path fold took that away, leaving the
  one row in that block no AT command touches.  **It is not a Hayes setting at
  all**: Hayes answers "which byte is the erase key?" with `S5`, on the AT command
  line, which this gateway implements with the standard default of 8 and which
  lives in the S-register block.  This setting is a property of the console
  *wire*, so it now sits beside the Mode control that decides whether it applies.
  The AT-command reference gained a note distinguishing the two, and Hayes says
  nothing about the case the modem fold covered because a modem in online mode is
  *transparent* -- which is a better argument for having removed it than the one
  the withdrawal entry gives.  Telnet already showed the row next to its Mode
  line; only a stale comment needed moving.

### Fixed

- **Six live interop gates had been passing without running, and one of them
  could not fail even when it did.**  The CCGMS gates -- the closest automated
  stand-in for a real C64, run against the genuine `ccgmsterm` reference codecs
  -- asked for `CCGMS_SEND_BIN` / `CCGMS_RECV_BIN` / `CCGMS_XFER_BIN`, and when
  those were unset they printed a note to a stream cargo hides for passing tests
  and returned success.  They were also the only interop tests in the suite not
  marked `#[ignore]`, so `--ignored` never selected them and an ordinary run
  folded them into its pass count.  The compiled harness had been sitting built
  on the same machine throughout.

  They now find the harness where its own README builds it, so no environment
  variable is needed; a variable that *is* set but names a missing file is an
  error rather than a skip; and all six are `#[ignore]`d like their lrzsz and
  C-Kermit siblings, so an ordinary run reports them under `ignored` and the
  live-gate sweep picks them up (82 gates to 88).

  Separately, the Punter **send** gate asserted nothing at all -- it printed the
  result and returned, so it passed whether the transfer succeeded, failed or
  timed out.  It now requires the send to complete and the reference receiver to
  accept the payload.  A guard test scans for any gate that can skip without
  being `#[ignore]`d, since the next one will be written by someone who never
  read this entry.

- **The web page could not set a port's erase key, and saving from it silently
  cleared one set elsewhere.**  `serial_a_backspace` / `serial_b_backspace` are a
  three-way choice and had been listed among the *boolean* keys since the setting
  shipped, so the save path ran `is_truthy("rubout")` over them and stored
  `false`.  Two consequences, and the second is the worse one: the page could
  never set the erase key at all, and **any** save from the web UI wrote `false`
  over whatever telnet or the desktop had set, because an absent field also
  became `false` and `backspace_target("false")` is `None` -- pass-through.  An
  operator who set `backspace` for an Altair from the terminal, then changed an
  unrelated setting in the browser, quietly got their wire back to pass-through.
  Measured through the real save path rather than by reading it, and the
  regression test asserts **both** halves: a "can it be set?" test passes while
  the clobber survives.

## [0.9.5] - 2026-08-23

### Added

- **A booted disk can be given a monitor ROM, and `DISK11.DSK` runs.**  Some
  disks are not self-contained: they carry an operating system and a BIOS and
  then print through a routine that was never on the disk, because on the
  machine they were built for it was already in memory.  `cpm_boot_rom` (default
  `off`, on all three configuration screens) loads one into a *booted* machine
  before it starts, and leaves it **writable** — these monitors were RAM-resident
  on the real machines too, and their callers patch them.

  `DISK11.DSK` is the case, and it was more forthcoming than expected: it does
  not merely go quiet, it *tests* for the monitor — `LDA C000 / CPI 7Fh / RZ` —
  and prints "This version of CP/M requires CUTER for VDM-1 to be present at
  C000h." before stopping for ever.  Four things were measured before anything
  was built, and the first three each ruled out a cheaper fix:

  * **The signature byte alone is worthless.**  Poking `7F` into `C000` gets past
    the gate, and the guest then goes quiet with no output and a blank screen.
  * It calls **three** of the monitor's entries, not one — `C019` (character
    out), `C0F9` and `C1D7` — so the six-byte synthesised entry behind
    `console_04_cuter` could never have served it.
  * It **patches six bytes inside the monitor's own image** before printing a
    character (`C03F`, `C040`, `C042`, `C045`, `C1EE`, `C215`), every one inside
    the real file.  A stub cannot be patched: the disk depends on the original's
    instruction layout.
  * `C0F9` in the real ROM is `LXI H,CC00 / MVI M,A0h` — the VDM-1 screen clear.
    So the picture lands in the window the gateway already renders, and the disk
    comes up in a browser at `/vdm` with no new display work at all.

  Verified end to end through the shipped publish path: with the monitor loaded
  the disk signs on as `47K CP/M Version 2.2mits (07/28/80)` on the card, has
  driven the scroll register, reaches `A>` and answers `DIR`.  The negative
  control is asserted in the same gate, because a test that only checks the good
  case cannot tell a working ROM from a disk that would have run anyway.

  The ROM files are **not shipped** — they are not ours.  Each catalogue entry is
  pinned to an upstream commit and verified by SHA-256, fetched on the operator's
  behalf from every CP/M settings screen, and a file already in `CPM/roms/` is
  never overwritten.  **The sample-disk download brings them along** in the same
  trip: a disk that arrives and cannot run without a 6 KB file is not much of a
  sample, and the gap is one an operator has no way to know about.  That is safe
  to do unasked for one reason worth stating — it does not turn anything *on*.
  `cpm_boot_rom` stays `off`, so the file being present changes nothing about how
  any machine behaves; it puts it in place so the setting *can* be chosen, exactly
  as fetching a disk puts an image in place so it can be booted.  Every screen
  that offers the download now names **where the ROM comes from** before you
  agree, derived from the pinned URL so it cannot name the wrong repository, and
  a grouped download failure says "N files" rather than "N disks" -- with no
  network every item fails for the same reason and lands in one group, which used
  to report a monitor ROM as a disk.  Intel HEX and raw binary are both accepted, and bytes
  falling outside the entry's declared window are refused: a monitor assembled
  for a different address would be written over the guest's own memory, which
  presents as a disk that boots and then behaves impossibly.

- **Each serial port can now decide which byte its device edits with**, the way a
  booted CP/M disk already could.  `serial_a_backspace` / `serial_b_backspace`
  take `passthrough` (the default, and what the gateway has always done),
  `backspace` (always send 0x08) or `rubout` (always send 0x7F), on the per-port
  <em>More</em> screen of all three surfaces &mdash; deliberately the same two
  words as `cpm_boot_backspace`, so an operator meets one vocabulary rather than
  two.  It exists because the terminal decides which of the two it sends and
  cannot be asked to change it, while a lot of period hardware edits with 0x08
  and a modern client sends 0x7F: neither end is wrong, and the result looks like
  a broken keyboard rather than a mismatch.  Both spellings fold, and a
  Commodore&rsquo;s 0x14 folds too &mdash; passing it through leaves a C64 with no
  editing key at all rather than the wrong one.

  **It applies to the modem path too, and the direction reverses.**  On a console
  bridge the operator is on the network side and the byte travels out to the
  device on the wire; in modem mode the device on the wire is typing and the byte
  travels out to whatever it dialled.  One setting either way &mdash; <em>the
  erase key this port sends onward is this byte</em> &mdash; folded at the one
  seam each path has: `run_console_bridge`&rsquo;s session&rarr;wire drain, and
  `process_online_bytes`, which both online loops share.  <code>ATZ</code>
  reloads it and <code>AT&amp;F</code> clears it, like every other saved modem
  setting.

  **Never in Kermit-server mode, and that is correctness rather than scope.**
  That wire carries 0x08, 0x7F and 0x14 as ordinary bytes inside a packet, and
  rewriting one would corrupt any transfer containing it.  It rewrites keystrokes
  generally, so sending a binary through the same connection wants
  <code>passthrough</code> &mdash; the same trade the PETSCII translator carries,
  and said in the same place.

  Verified on the operator&rsquo;s own hardware, as a controlled experiment
  rather than an observation: an Altair on a slave&rsquo;s console port, picked
  from the master, sent the same keystrokes under both settings.  With
  <code>backspace</code> the DEL arrived as a destructive erase
  (<code>\x08 \x08</code> echoed, the character gone); with
  <code>passthrough</code> the same DEL arrived as a Teletype rubout and the
  Altair <em>reprinted</em> the deleted character &mdash; which is what "the
  backspace key is broken" looked like.

- **A changed master host key can now be fixed from any configuration surface,
  with the operator's consent.**  A slave pins its master's SSH host key on first
  contact and refuses a changed one &mdash; correctly, because a reinstalled
  master and a man-in-the-middle look identical from there.  Until now the only
  remedy was a log line telling somebody to hand-edit `gateway_hosts`, which is
  no remedy at all on a headless slave reached over telnet from a C64.

  `src/resolve.rs` holds a registry of *pending, resolvable* problems: something
  that hits one reports it, the three configuration surfaces show it and offer
  the fix, and taking the fix runs it and withdraws the entry.  The web UI and
  the desktop editor grow a red panel above the settings; telnet grows an
  **`X  Resolve Errors`** row on the Configuration menu that **appears only while
  there is something to resolve**, because a row that is always there is a row
  nobody reads.

  **Nothing is fixed automatically, and the explanation says why.**  Every entry
  needs a human to say yes, and the host-key one states plainly that clearing it
  discards the evidence &mdash; "It is also what a man-in-the-middle looks like,
  and nothing here can tell the two apart".  The entry is also *withdrawn* when
  the problem stops applying (a relay that connects clears it), which is what
  keeps the list worth reading.

  Verified end to end against a real master: a deliberately wrong pinned key
  raised the problem, the web panel and the telnet screen both showed it with the
  warning, confirming the fix removed the stale entry, and the next connection
  pinned the master's real key with no problem left listed and the telnet row
  gone.

- **Fixed: AI Chat and the weather service printed typographic Unicode raw,
  which garbled dashes and broke the wrapping.**  One defect, two symptoms, and
  the second looked like a separate bug: asking about a `PLC-5` came back with
  U+2011 non-breaking hyphens, U+202F narrow spaces and U+2019 apostrophes in it,
  each of which a PETSCII or 7-bit terminal draws as two or three pieces of
  rubbish.  And because the word-wrap counts *characters* while such a terminal
  draws one glyph per *byte*, a 78-character line arrived as **82 columns** on an
  80-column screen and wrapped itself — measured on a real reply: 143 characters,
  151 bytes.  Folded, 78 characters is 78 columns again.

  **The fold already existed and had exactly one caller.**  `fold_terminal_safe`
  was written for the web browser (an HTML table arrives as 918 bytes of
  box-drawing characters) and lived there, so the two other surfaces that print
  fetched text at a terminal sanitized their escapes and then printed the
  typography as-is.  The composed rule now has a name,
  `aichat::display_for_terminal`, and all three surfaces call it.  It is
  deliberately *not* folded into `sanitize_for_terminal`: the browser sanitizes a
  page's URL with that, and folding an en-dash in a URL breaks relative-link
  resolution — which the existing
  `test_sanitize_does_not_fold_the_url_or_form_values` caught when it was tried.

- **AI Chat works again: Groq retired the model it asked for.**
  `llama-3.3-70b-versatile` began answering `404 model_not_found` -- "the model
  does not exist or you do not have access to it" -- and AI Chat was simply dead,
  with every unit test still passing because none of them speak to Groq.  The
  default is now `openai/gpt-oss-120b`, and **`ai_model` makes it a setting**, on
  all three configuration screens: a retired model must not need a new build.

  Two things were measured against a live key rather than assumed.  The candidate
  models differ in ways that matter to a terminal: `qwen/qwen3.6-27b` returns its
  chain-of-thought inside the answer as a literal `<think>` block, and the
  reasoning models can put the whole reply in a `reasoning` field and leave
  `content` **empty** -- which this client handed back verbatim, i.e. a blank
  screen indistinguishable from a hang.  So a leading `<think>` block is stripped
  and `reasoning` is used when `content` is empty.  (The first measurement of all
  this was wrong and had to be redone: it capped `max_tokens` at 80, a limit the
  product never sends, which made three usable models look broken.)

  A live gate now exists (`GROQ_KEY`), asserting a reply arrives, is not empty,
  and carries no chain-of-thought markup.

  **Each model is also asked to keep its working short**, with the setting that
  model accepts.  Measured, not sent blindly: `low` on the `gpt-oss` family took
  the unseen reasoning from 141 characters to 25 *and made the answer longer*
  (132 &rarr; 186) for fewer tokens (78 &rarr; 63); `qwen` accepts only `none` or
  `default`, and `none` took a one-sentence reply from 589 completion tokens and
  2272 characters to **35 and 148** with no `<think>` block left to strip.  The
  other three models on offer **refuse the parameter outright with HTTP 400**, so
  sending it blindly would have broken three working configurations.  A model the
  rule does not recognise that refuses it anyway is retried once without it &mdash;
  verified by forcing the refusal, where the retry answers and its removal
  reproduces the error exactly.

- **Fixed: the "More" popups in the desktop editor slid off the left edge and
  filled the window.**  A wrapping label lays out to the width available to it,
  and inside an auto-sizing window that *is* the window's width, which is decided
  by its content -- a feedback loop that does not oscillate but ratchets.  The
  AI/Browser/Weather/CP/M panel was measured at 650&nbsp;px on its first frame,
  664 on the second, 678 on the third, **+14 every frame** until it reached the
  1120&nbsp;px of the host window and stopped only because egui clamps there.
  What you saw was a panel sliding left with the first character of every label
  cut off, and resizing the window did not help, because the content ended up
  wider than any window.  One bounded wrap width fixes it, and measuring all six
  popups rather than the reported one found two more creeping the same way
  (*General — More* and *Mount CP/M Drives*); the other three were already
  stable, their labels being short enough never to wrap.

  **The bound belongs on the window, not on the content.**  Bounding the content
  with `Ui::set_max_width` stops the ratchet and widens the layout rect
  *leftward* by the frame's inner margin, which puts the content outside the
  window's own clip rect -- so it was moved onto the `Window` itself.

- **Fixed: a label too wide for its column cut the first character off everything
  beside it, including the Save button ("ave").**  `cpm_choice_row` allocates a
  fixed 196&nbsp;px box and lays the label out *right-to-left* inside it, so a
  label that does not fit neither truncates nor wraps -- it grows the container
  **leftward**, dragging every left-aligned widget below it outside the clip
  rect.  `Booted disk's monitor ROM:`, added earlier in this release, was two
  characters too long.  A guard now measures **every** column label with egui's
  own font metrics under the program's real theme; the first version of that
  guard used egui's default 14&nbsp;px body font where this program sets
  16.8&nbsp;px, and passed with the offending label put back.

- **Fixed: a booted disk that needs a monitor ROM was told to "fix or replace"
  a file that had never been downloaded.**  Three states -- none selected, one
  selected whose file is absent, and a file present but unusable -- had been
  collapsed into two, so an operator who selected CUTER on a fresh install was
  sent to repair something that did not exist.  Each now names the action that
  applies, and "not there" gets its own sentence rather than an `io::Error` and
  an absolute path, which a 38-column boot banner delivered truncated.

- **A disk that needs a monitor ROM now says so in three places before it
  disappoints you.**  `repodisks.txt` carries the note on `DISK11.DSK` --
  generated from the disk's own bytes, not a hard-coded filename, so it cannot
  drift -- the boot banner warns when such a disk is starting without one, and
  the note names the **setting** rather than the fact, because the file arriving
  in `CPM/roms/` does nothing until `cpm_boot_rom` selects it.

  The signal is the disk *testing* for the monitor: `LDA C000` in the system
  tracks.  **Measured across 140 images in all four collections (106 distinct
  byte-images): exactly one disk does it** -- `DISK11.DSK`, which also appears in
  the Altair-Duino collection as `DISK16.DSK`, byte for byte, one disk under two
  numbers.  A call into the monitor window would have been the obvious signal and
  is useless: it fires on 45 of 140, every disk with a jump table up there, and it
  cannot separate the disk that needs a *file* from `TDISK05`, which calls `C019`
  and works today on the six-byte synthesised entry.  Only a disk that will refuse
  to run bothers to look.

- **A booted guest now runs at its processor's speed, and `cpm_boot_speed`
  decides.** Nothing paced the CPU before: the pump naps when a guest is *idle*,
  but one that is working ran at whatever the host could manage. Measured, that
  is **16.65 MHz while actually playing SPACEWAR over telnet — about eight times
  an Altair 8800** — and **141 MHz** stepped in a tight loop with nothing else to
  do. A compile enjoys it; anything that keeps time by counting does not, which
  is why SPACEWAR was unplayable.

  `auto` (the default) holds the guest to what the processor you chose actually
  ran at: **2 MHz for the 8080, 4 MHz for the Z80**. `unlimited` restores the old
  behaviour, and a bare number is megahertz. Held to `2`, the same SPACEWAR
  session measured **1.95 MHz**.

  Paced on **cycles, not instructions** — `iz80` counts them from separate tables
  for the 8080 and the Z80, so the rate is as accurate as the instruction mix
  rather than an average assumed on our side. The governor lives in the session
  pump and **never in `BootMachine::step`**: what is being limited is a session
  with a person watching it, and every live gate in this project drives `step` in
  a loop hundreds of millions of times, so a governor down there would slow the
  test suite by the same factor and make the disk survey unrunnable.

  **The pacing is against the wall clock, so it does not depend on this
  computer.** A faster host reaches the cycle budget sooner and sleeps longer; the
  guest still runs at 2 MHz. The governor can only ever slow a guest *down* —
  measured on this machine, targets of 2 and 4 MHz were held to 1.95 and 3.93,
  while a target of 8 reached only 6.92 because this host cannot sustain more in a
  live session. Arrears are deliberately not repaid: a guest that fell behind
  while idle or waiting on a disk is not given a burst at full speed to catch up,
  because arriving in bursts is the symptom being fixed. Every session logs the
  clock it actually achieved, which is the only way to check that a setting did
  what it says.

- **The Cromemco joystick games are playable, from the browser watching the
  screen.** `SPACEWAR`, `GOTCHA`, `DOGFIGHT`, `TANKWAR`, `CHASE`, `AMBUSH` and
  `TRACK` read the **D+7A** analog board — two joysticks, two axes and four
  switches each, on ports `18h`–`1Ch`. `cpm_joystick` (on by default) gives a
  booted machine one, and the keyboard of any browser on the `/vdm` page plays
  it: player 1 `W`/`A`/`S`/`Z` with `X` to fire, player 2 `I`/`J`/`K`/`M` with
  `N`. The page lists all ten, from the same table the handler keys off, so the
  legend cannot drift from what it sends.

  **A held key swings**: centred at the press, full deflection half a second
  later. A key has no magnitude and these are analog controls — SPACEWAR turns
  its ship at a rate set by deflection, so a fixed full-scale would only ever
  spin flat out. The ramp is integrated by the board and not in the browser,
  because the guest reads those ports tens of thousands of times a second while
  a page can only speak on its own 150 ms poll; a level computed in the browser
  would arrive in visible steps, and a late request would change the shape of
  the swing rather than only when it started.

  **The page reports the whole set of held keys every time, not changes.** A
  dropped request is then corrected by the next one instead of leaving a
  direction stuck down — the one failure a level-based control has that a
  keystroke queue does not. For the same reason the gateway centres everything
  if a page has said nothing for a second, so a closed tab lets go of the helm.

  **Every value was measured against Cromemco's own `ADCTEST.COM`**, a
  calibration program on `DISK10.DSK` that displays all four channels and the
  switch byte, and which the board is now tested against:
  `test_adctest_reads_back_what_the_board_was_told` drives the real program and
  decodes its own readout. A centred stick reads `00`, right and up `7F`, left
  and down `81`; the switch byte is active low with player 1's button on bit 0
  and player 2's on bit 4. Nothing here was taken from a remembered manual —
  the ports came from a port trace, the channel order and the polarity from the
  period program's own display. The bit-4 assignment was a guess when it was
  written and is a measurement now.

  This also corrects something that was quietly wrong: an unclaimed port reads
  `FFh`, and on an analog axis `FFh` is not "no joystick" but a stick pushed
  hard over — so these games have always run against a jammed control. That is
  why the key defaults to on: the choice was never whether to add hardware but
  which wrong answer to give, a centred stick nobody is touching or a hard-over
  one. It is still a key, because claiming a port is a real change to a machine
  and an operator is entitled to a booted guest that is byte for byte what it
  was.

  **SPACEWAR itself was then driven, and it responds to every input the
  keyboard offers.** Proving that took three attempts, and the failures are the
  useful part: holding a direction and watching the picture proves nothing, since
  the ships carry inertia and the display animates regardless; counting lit
  pixels was no better, every condition oscillating between roughly 55 and 95;
  and a first "controlled" version counting distinct sprite orientations *failed*,
  because the count conflated which sprites existed with how they were turned.
  What works is an experiment rather than an observation — boot a fresh machine
  per condition, run identical instruction counts, apply exactly one input, and
  compare pictures, the emulator being deterministic once the wall-clock ramp is
  forced out of the way. A **repeat of the do-nothing run** is that method's
  licence: without byte-identical baselines every verdict could be the emulator
  wandering. Measured, all eight axis directions and both sticks' first switch
  change the game (41–140 cells of a 16,384-cell picture), and switches 2, 3 and
  4 of each stick change nothing at all — so the two bits the keyboard drives are
  exactly the two SPACEWAR reads.

### Changed

- **`repodisks.txt` is ordered by disk name, and every disk says what it is.**
  The catalogue was grouped by collection, which made it unusable for its one
  purpose: you cannot look a disk up by name unless you already know whose
  collection it is in, and that is the thing you came to find out. Every disk
  is now listed once, A to Z, whichever collection it came from — an index of
  all 98 first, then the same disks in full — with every collection's address
  gathered at the head of the file and a short tag naming it on each entry.
  Eight names exist in more than one collection and are genuinely different
  disks (`cpm22.dsk` is in three), so that tag is what tells them apart, and
  the sort is case-insensitive because byte order would put every upper-case
  name first and hide half the catalogue below the other half.

  Each entry also carries **one line saying what is on the disk**, so choosing
  one no longer means reading 98 directory listings. It is derived from the
  disk's own directory — never from the filename, never from anyone else's
  catalogue — which is what keeps it from claiming something the disk does not
  carry, and two rules keep it from stating plausible falsehoods instead. A
  system is claimed only on the files that *are* the system: `CDOSCPM.COM` is a
  CDOS-to-CP/M converter that lives on CP/M disks, and matching it labelled
  four CP/M disks "Cromemco CDOS". And **a passenger is a proportion, not a
  count**: `hd-tools.dsk` carries 345 files including all six Zork files, so a
  flat "needs two matches" printed "Infocom adventures" for a tools disk —
  describing it by its smallest corner, which is the very thing the rule was
  written to prevent. Measured across all 98 disks, the labels rejected there
  are its lowest shares (0.6%, 1.8%, 2.4%) and every label worth keeping sits
  at 3.8% or above, so a theme must be two matches *and* a thirty-second of the
  disk. A disk where nothing clears that gets the honest fallback. A single
  *defining* file is exempt either way — one `COMAL.COM` is what that disk is
  for.

- **Every catalogue entry says whether the disk boots or is mount only.** It
  leads the line, because booting and mounting are different things a disk is
  *for* and which one it can do is the fact you are choosing on. Three states —
  `boots`, `mount only` (a CP/M filesystem but no boot program), `neither` (no
  boot program and no CP/M filesystem, so data in another system's format).
  Across the 98 disks that is 73 / 17 / 8.

  The marker comes from the product's own `boot::image_can_boot`, not a second
  opinion, so the catalogue and the boot picker cannot disagree — and that
  function is false for exactly one reason, `Bootability::NoBootProgram`, the
  machine-independent one, which is what makes the answer safe to ship in a
  file. A disk this machine merely has no *board* for still counts as bootable.

  This corrected a claim rather than only adding one. The old text printed
  "boots its own operating system" for **any** disk with no CP/M filesystem,
  which flatly asserted a boot for the eight that cannot: `DISK0B`, `DISK0D`,
  `DISK0F` and the five `ucsd-*` second disks. Two independent readings of one
  file — sector 0, and the CP/M directory — had been collapsed into a single
  sentence that only happened to be right for the disks anyone had tried. For
  the same reason the summary no longer calls anything a "system disk":
  `cpm22-2.dsk` carries `CPM64.SYS` with the BIOS and BOOT sources beside it and
  does **not** boot, being a disk for *building* a system. Naming the system it
  carries is a fact; calling it a system disk was an inference on top of one.

  Two theme labels were shortened so every line fits 80 columns, which the
  `readme.txt` beside the catalogue was already held to and this file was not.
  The marker is what surfaced it — six lines went over, the worst at 96 — and
  shortening a label beat wrapping a summary, because one line per disk is the
  point of the index. Both got *more* accurate on the way: `comms tools` covers
  the transfer programs as well as the terminals, and `development tools` covers
  the debuggers, `SID` and `ZSID` not being linkers as the old label implied.

## [0.9.4] - 2026-08-22

### Added

- **Telnet can edit the CP/M virtual modem's saved Hayes profile.** The six
  values the guest stores with `AT&W` — `ATE` echo, `ATV` verbose, `ATQ` quiet,
  `ATX` result level, `AT&C` DCD and the S-registers — were editable in the web
  UI and on the desktop, and in telnet only *written* when the guest saved them.
  So the one surface a C64 or an SC126 actually reaches was the one that could
  not repair a profile, and a guest that had stored `ATQ1` came up silent with
  no way back but to know that and undo it from inside the emulator, or to edit
  `egateway.conf` by hand.

  The CP/M settings screen sits exactly on the 22-row PETSCII budget, so the
  rule its row-count test states applies: a new question brings its own screen.
  `M` opens **CP/M Modem Profile** (16 rows) and takes `D`, *Default modem
  port*, with it — a rare one-shot, where `U`'s port cycling is the key an
  operator reaches for, so the swap is one for one and the parent screen is
  unchanged in size. `D` still works if pressed on the parent, the same courtesy
  `G` gets. Each value sits in its own action row rather than in a status block
  above them, which is what keeps the new screen to 16.

  Verified on a live 40-column PETSCII session: every row fits, the toggles
  persist to the config file, and `X`/`&C` cycle within the ranges the
  emulator's own AT parser accepts.

- **A second copy offers to take over instead of coming up useless.** Launching
  the gateway twice in the same directory used to produce a process that
  started fully, opened a window, offered every configuration screen, and held
  no listener at all — while the *first* copy went on answering connections.
  Five copies stacked up that way on 2026-08-19, the oldest still serving
  telnet while a newer one served the web UI, so a Save in the visible window
  never reached the process that was answering.

  A launch now finds out first. It claims the directory with an OS-level lock —
  never a PID file, which goes stale on a crash and whose PIDs get reused — and
  a second copy is offered **Take Over** or **Quit** in a window with no server
  behind it, naming the running copy by PID. Nothing is offered to edit there,
  because saving settings from a window whose server was never started is how
  the copies came to disagree in the first place.

  **The handover is cooperative, not a kill.** `SIGTERM` was the obvious
  mechanism and is wrong twice over: it does not exist on Windows, and it skips
  the gateway's own shutdown — the broadcast that tells connected sessions the
  server is going down, the bounded join of the serial threads, the staged
  write of a booted disk image. The newcomer leaves a request file instead and
  the holder, which polls for it, trips the same `shutdown` flag its own Quit
  button uses. Identical on every platform, with no new signal plumbing.

  A **headless** launch refuses instead of asking: with no window there is
  nobody to ask, and a service started twice by accident must not stop a
  working gateway on nobody's authority.

  Two files were missed in the first pass of the folder move and are now inside
  it as well: `gateway_hosts` (the accumulated known-host fingerprints for
  outbound SSH) and `bookmarks.txt` for the text browser — whose own doc comment
  said "stored next to the binary", so grepping for paths found it where
  grepping for the rule would not have. And every writer of a file in that
  folder now ensures the folder exists rather than trusting `main` to have done
  it: a unit test reaches those writers without going through `main`, which is
  how the gap was found, and an operator can delete the folder while the gateway
  is running.

- **Closing the GUI window asks whether you meant to stop the server, and a
  Quit button says so out loud.** Closing the console window leaves the server
  running, which is right from a shell — the terminal is still there and Ctrl-C
  still reaches the process — and a trap from a desktop icon: the AppImage's own
  desktop entry sets `Terminal=false`, so the process inherits the graphical VT
  and no shell exists to press it in.

  With no Quit control anywhere either, the only move the window offered was to
  close it and relaunch, and **a second copy binds nothing**. Measured: five
  copies stacked up, the oldest still serving telnet while a newer one served
  the web UI, so a Save in the visible window never reached the process
  answering connections — `bindwatch` had been saying exactly that in the log
  the whole time.

  The close is now vetoed and asks: *Stop the Server and Quit*, *Leave It
  Running*, or *Cancel*. The **Quit** button sits in the window header rather
  than under a `More...` popup, because somebody who cannot find how to stop a
  program does not go looking under Server. Dismissing the dialog is Cancel,
  never an answer — a modal that stopped a server because it was waved away
  would be worse than the trap it replaced. The wizard's own title-bar X is
  untouched, and a Save and Restart still returns a fresh window.

- **A root or `sudo` session is warned before it writes anything, not after.**
  The same trap the unreadable-config diagnosis explains a day late, caught
  while it can still be avoided: a red banner across the top of the GUI, and
  the same lines in the startup log so a root run with no window says it too.

  `SUDO_USER` is the signal, not root alone. A machine that always runs the
  gateway as root is doing nothing wrong and will never hit this — root writes
  the files and root reads them back. A *temporary* escalation is the dangerous
  shape, because the operator is coming back as themselves afterwards, and
  `SUDO_USER` names exactly who: the warning says which account will be locked
  out, that the serial port it was escalated for does not need root, and the
  `chown` that hands the directory back. **Dismiss** puts it away for that
  window; it is deliberately not a config key, since persisting it would
  silence the warning on the installs that have never seen it. It returns after
  a Save and Restart, which is right rather than sloppy — a Save is precisely
  when a root session has just written `egateway.conf` as root.

  The serial group is a parameter rather than a hardcoded `dialout`: Linux gates
  a serial device that way, macOS does not (a `/dev/cu.*` is not group-gated),
  so naming it on a Mac would send the operator looking for something that does
  not exist. Windows never sees this warning at all, and that is correct rather
  than a gap — the `0600` that causes the lockout is applied only on the unix
  write path, so a file an elevated Windows process creates inherits the
  folder's ACL and stays readable.


### Changed

- **BREAKING: everything the gateway creates now lives in one
  `ethernetgateway-data` folder.** A launch used to write straight into
  whatever directory it was started from, which for a desktop icon is wherever
  that icon lives: `egateway.conf`, a log, an SSH host key and a whole
  `transfer/` tree dropped among the operator's own files. A launch now adds
  exactly **one** entry to that directory, and everything goes inside it.

  **Existing installations do not carry over, deliberately.** There is no
  fallback and no migration: an upgrade will not find the old `egateway.conf`,
  the old SSH host key or the old transfer directory, and will create fresh
  ones. Moving somebody's host key and disk images automatically is the kind of
  thing that goes wrong once and cannot be undone, so the choice is left with
  the operator — move the old files into `ethernetgateway-data` by hand, or
  start fresh. Note the SSH host key: a new one means every client that
  connected before reports the host identity has changed.

  The **working** directory, not the binary's — they are the same thing when
  the binary is run from its own folder, and very different for an AppImage,
  where the executable sits in a read-only temp mount at a fresh path every
  launch and nothing can be written beside it. And not a folder called
  `ethernetgateway`, which cannot work at all: that collides with the binary
  whenever the two share a directory (`create_dir_all` fails with `File
  exists`), and a case variant is no escape on macOS or Windows, where
  filenames are case-insensitive.

  Two settings are **defaults, not rules**: `transfer_dir` and `log_file` now
  default inside the folder, but a value you set is used exactly as written, so
  `transfer_dir = /mnt/bbsfiles` is never relocated. That is why they are
  defaults rather than a resolution step — a rule that rewrote relative paths
  would have to be idempotent or it would prefix its own output on the next
  load.


### Fixed

- **The desktop labelled the same Hayes toggle two different ways in one
  window.** The serial ports' advanced panel says `Echo (E1)` / `Verbose (V1)` /
  `Quiet (Q1)`; the CP/M virtual modem's block, eight hundred lines away, said a
  bare `Echo` / `Verbose` / `Quiet` and put the AT letter in a tooltip — under a
  comment claiming that block exists "for the same reason the ports' is". The AT
  letter is what an operator matches against the manual and against what the
  guest typed, so it belongs in the label. All three surfaces now agree on those
  three, and a test pins it: whole-file matching would have passed on the serial
  block alone, so it checks the CP/M checkboxes themselves. The longer fields
  are deliberately left to differ, since telnet has 40 columns and the web has a
  form row.

- **A doc comment pointed at a function that does not exist.**
  `cpm_emu.rs` sent a reader to `TelnetSession::cpmemu_place_egt80` for how the
  bundled terminals are placed; there is no such method and no `cpmemu_place*`
  anything — the real one is `place_bundled_terminals`. Found by running
  `cargo doc`, which also caught two comments rendering wrongly: a literal
  `<err>` and `<label>`-style placeholders read as HTML tags, and a `Stop::Hbios`
  link that could not resolve from its module.

- **The images readme gave two different telnet routes to the CP/M screen.**
  The mounting section said `C (Configuration)`, then `O (Other Settings)`, then
  `E (CP/M settings)`; the boot section forty lines below said
  `C (Configuration), C (CP/M Settings)`. The second is what the menu does, so
  the first walked an operator into Other Settings, which has no CP/M entry and
  no `E` key. Found by driving the menu rather than reading it. A test now pins
  that both sections name the same two keys and that the stale phrasing is gone
  — the failure was self-disagreement, so asserting only that the correct route
  appears would have passed the entire time the wrong one sat above it.

- **A relayed `ATDT` answered `CONNECT` before the master had dialled
  anything.** The slave turns the relay hello straight into a modem `CONNECT`
  with carrier asserted, and the master sent that hello when it *accepted the
  channel* — before placing the call. So every onward-dial failure reached the
  attached device as `CONNECT` followed immediately by `NO CARRIER`, with DCD
  raised in between. Measured on a live master/slave pair: a dial refused by
  `allow_peer_dial` and a dial to a host with nothing listening both did it, the
  second with the gate switched **on** — so this was never a configuration
  problem, and no reordering of config gates could have fixed it. `CONNECT` to a
  modem means carrier is up; vintage terminal software and BBS scripts act on it.

  The hello is now withheld for the two targets that place a call (`dial` and
  `peer`) until the far end actually answers, and still sent at accept for
  `menu` and `kermit`, where the master *is* the far end and accepting is the
  whole answer. Its absence then means what the slave needs it to mean: no call,
  report `NO CARRIER`. That covers the failures the master itself observes — the
  two `allow_peer_dial` gates, connection refused, the onward-dial answer
  timeout, a local peer port that is unregistered, not dialable, or never
  answers its ring. **One case is deliberately not covered**: a peer-dial across
  the *crossbar* to another slave's port, where the master hands the hello over
  as soon as it claims that slave's registration channel. Claiming is not
  answering — the far slave then rings its own device and, if nobody picks up,
  simply drops the channel without telling the master — so a crossbar call to an
  unanswered device is still `CONNECT` followed by `NO CARRIER`. Closing that
  needs the far slave to report back, which *is* a framing change and would move
  `RELAY_PROTOCOL_VERSION`. The slave waits
  longer for the hello when it asked for a dial, since the master is holding it
  across its own answer wait, and the outer connect budget grows to match so a
  slow dial is not reported as a network fault.

  The bytes are unchanged, so this is not a framing change and
  `RELAY_PROTOCOL_VERSION` does not move. The one skew that matters is an old
  slave against a new master on a dial that succeeds slowly, where the old slave
  gives up at its fixed 5 s; a dial that fails, or one that succeeds promptly,
  behaves the same or better on both.

  Every success path writes the hello through one `answer_and_bridge` helper
  rather than by hand, because the dialing targets have four of them and a fifth
  added later would otherwise hang a slave until its timeout. Eight relay
  transfer tests now consume the hello as the slave does: two (XMODEM, Kermit)
  failed when it moved onto that wire, and six passed only because their
  handshakes rescan for a start byte — the six were the more useful finding.

- **A refused Kermit relay was reported to the slave as a working one, and
  retried once a second for ever.** `RELAY_HELLO` is the master saying
  *accepted* — it exists so a slave can tell an accepted relay from a
  refused-but-open channel, because russh's `exec()` returns `Ok` either way.
  `allow_relay_kermit` (off by default) was read in `run_master_relay_kermit`,
  which runs *after* the channel is acknowledged, after the hello, and after
  the "accepted serial relay" log line — so the one gate still outstanding was
  the one behind the handshake meant to report it.

  Measured on a live master/slave pair: the slave logged `CONNECTED — the
  master's Kermit server is on this wire; files live on the master`, repeated
  that in its link summary, then read EOF and took it for a dropped link.
  Because the *connect* had succeeded it reset its attempt counter and backoff
  every cycle, so instead of one deduped outage line and the 60 s
  `RECONNECT_BACKOFF_REFUSED` it reconnected about once a second indefinitely,
  writing a log line on both machines each round. The gate now sits with the
  other refusals, ahead of the hello, and the slave's existing `Refused`
  handling does the rest. A source-scanning test pins the ordering, since
  `exec_request` needs a live russh session no unit test can build. The
  slave-side message also now lists `allow_relay_kermit` among the causes it
  offers — it could not name the one that was actually in force.

- **A registered slave port was logged as a "console port" whatever mode it was
  in.** `serial-register` carries only the port label, so the master never
  learns the mode; both console and modem ports register through it, as the
  registry documents. Modem ports began registering in 0.9.2 and modem is the
  default, so the common case was the mislabelled one. It now says "serial
  port", which is what the wire actually supports.

- **The images-folder readme never mentioned that the gateway can fetch the
  disks for you.** Reported: `CPM/images/readme.txt` reads as though the only
  way to get disks is to go to GitHub and copy files in by hand. It was written
  before the downloader existed and never followed it — the feature shipped,
  all three disk screens grew a "Download sample disks" button, and
  `web/cpmreference.html` gained a paragraph about it, while the one file
  sitting *in the images folder*, which is the first thing anybody looking at an
  empty folder reads, went on sending them to fetch 34 disks manually.

  `WHERE TO GET IMAGES` now leads with the offer and names where it is on each
  of the three surfaces. The disk count, the total size and the repository list
  are **rendered from the manifest the downloader really reads** rather than
  typed into prose — the same rule this file already applied to the format
  table, and the half that rots is always the one a human maintains beside a
  list the code maintains. z80pack is called out as the collection the offer
  does *not* cover, with a test that fails if it ever joins. Existing readmes
  refresh themselves on next launch (verified live), because this file is
  instructions rather than an operator's own work.

- **The manual's sample configuration disagreed with the real defaults.** The
  0.9.4 data-directory move left `transfer_dir = transfer` and
  `log_file = ethernetgateway.log` in it — both one directory level short, in
  the very section whose prose explains that everything moved — and
  `weather_zip` was still listed, the legacy spelling of `weather_location`
  that the parser still accepts and nobody should now be taught to write. The
  sweep that caught this class at the time looked at the manual's `<tr>` rows
  and missed these, because this block is a `<pre><code>`.

  The manual's first-launch example was stale the same way: it claimed the
  gateway creates `egateway.conf` and `transfer/`, and omitted both the
  `Data directory:` and `Logging to` lines that 0.9.4 added. It now matches what
  the binary actually prints. `test_the_manual_sample_config_matches_the_real_
  defaults` compares every `key = value` line in the manual against a config
  file written from `Config::default()` — so the scan is the file rather than
  the markup, and a default that moves without the manual following it fails.

- **CLAUDE.md still described booting as read-only.** `cpm_boot_writable` has
  defaulted to `true` since 0.9.3; the guidance file said the opposite for two
  releases, which is the costlier place for that claim to be wrong because it is
  what steers work on the feature.

- **The mount screens now name the disk that has slot 0.** Reported: with
  `CP/M runs: Boot HDSK04.DSK`, the first mount row showed `(drive folder)` next
  to a note reading "the booted disk is here" — two statements that contradict
  each other, and neither naming the disk. A boot disk is deliberately *not* a
  mount (one file cannot be both; the guest owns the format and rewrites the file
  when it leaves), so slot 0 is **reserved for** it rather than filled by it —
  but nothing on the screen said by what, so the row read as a drive the operator
  had failed to fill.

  Slot 0 now names its occupant on all three surfaces, from one text
  (`MountContext::boot_slot_note`) and from the **resolved** boot target, so a
  setting naming a disk that is gone runs the emulator and claims nothing. The
  desktop and web pickers show that disk and are not selectable while it is
  empty; with something mounted underneath they stay editable — a mount left
  behind the boot disk has to be removable without first clearing
  `cpm_boot_image` — and gain the warning that the guest cannot reach it, said
  where it can still be changed rather than only in the boot screen's notes
  afterwards. The telnet screen gained a `Booting:` line, and says "No *other*
  images mounted" when a disk has slot 0, because the old wording read as a
  contradiction beside it.

- **The desktop and web mount screens now list booted images at all.** The
  telnet screen has had a `Booted:` section since 0.9.2 — an image booted without
  having been mounted first is in none of the mount tables, so without it a disk
  could be offered, refused on Save as "being run by a booted session", and
  accounted for nowhere. The other two surfaces showed nothing, which left the
  one surface that answered "what is running?" as the one a C64 operator was
  least likely to be using. The filter moved from the telnet module into
  `registry::booted_to_report`, and a test holds that all three surfaces ask it.

- **The CP/M printer screen told a C64 the wrong place to look, and then cut the
  answer off.** The three `cpm_printer` labels ended in `transfer/printer/` --
  which stopped being a real path when the transfer directory moved under
  `ethernetgateway-data`, and which a literal in a *shared* label could never be
  right about anyway, since the folder follows whatever `transfer_dir` the
  operator set. Worse, the telnet screen renders that value through a 26-column
  budget on PETSCII, so the path was exactly the part a 40-column client dropped:
  a C64 read `OpenDocument (.odt) in tra`. Measured on a live session.

  The labels now name the format only (`OpenDocument (.odt) file`), and every
  surface says where the document lands from the live setting -- the telnet note
  rows, the web hint and the desktop's hover text all already described "a
  `printer` folder inside the transfer directory", so the *documentation was
  right and the code had drifted from it*, the reverse of the last three passes.
  The board and bare-CR labels were shortened for the same reason (`Altair line
  printer - 03h` keeps the port that `data 03h (BASIC's 'C')` lost at column 26).
  A new test holds the rule: every label in those three lists must fit the width
  the screen actually renders it at -- read out of the screen's own source, so
  the two cannot drift -- and no two may truncate to the same text, because two
  settings that read identically on a C64 are worse than a lost tail.

- **A path row now shows the end of the path.** `Dir:`, `Current:`, `In:`,
  `Now in:` and the log-file row truncate a path into 26-30 PETSCII columns, and
  they kept the *head* -- so after the data directory move, whose default base is
  30 columns on its own, a C64 operator changing directory saw
  `ethernetgateway-data/tr...` at the root and three levels down, the same text
  either way. The base is a setting they chose and can read on the configuration
  screen; what a path row is for is the part that changes. `truncate_path_to_width`
  drops the front instead (`...tgateway-data/transfer/CPM/`), and the test pins
  the old behaviour as the bug: at that width the head-truncation of the root and
  of a sub-directory are byte-for-byte identical. Same lesson as `cpm_runs_row`'s
  `(missing)` marker -- when what is new sits at the end of the line, a naive
  truncation deletes exactly it.

- **A copy that is serving nothing now says so in its own window.** The instance
  lock closed the same-directory case, but the case it cannot close is a copy
  launched from a *different* directory -- a desktop icon while a systemd unit
  serves from its own `WorkingDirectory`. That copy claims its own lock quite
  legitimately, comes up with a full editor window, and binds nothing: every
  setting saved from it reaches a config the serving process never re-reads,
  which is the original five-stacked-copies defect arriving by the one route the
  lock is per-directory about. The aggregate bind warning is now drawn across the
  top of the GUI as well as logged -- one renderer, so the two cannot disagree --
  and it names the data directory that window is editing, which is what makes
  "this Save will not reach it" concrete. Dismissal is tied to the *text*, not a
  flag, so a Save and Restart that still binds nothing says so again.

- **Ownership advice no longer tells an operator to break a service.** The
  diagnosis assumed the other owner was root-from-`sudo` and ended with
  `chown -R you ethernetgateway-data` whatever the uid was -- but the shipped
  systemd unit runs as `User=ethernetgateway` out of `/var/lib/ethernetgateway`,
  so an operator running the binary by hand in that directory was told to take
  the running service's files away from it. Both diagnoses now split on uid 0:
  root gets the sudo story and the chown, any other owner is named as another
  account's installation and told to run as that account or from a directory of
  its own. Verified live against a directory owned by a service uid.

- **The fatal startup window could not be closed by a signal, and never appeared
  on macOS.** Two defects in the window added earlier in this cycle. It
  registered no egui context, so the signal watcher had nothing to send `Close`
  to -- measured: `SIGTERM`, still alive four seconds later, and it stayed alive.
  And its display check asked for `DISPLAY`/`WAYLAND_DISPLAY` under `cfg(unix)`,
  which macOS *is*, while setting neither: on the one platform in the release
  matrix where double-clicking is the normal way to start it, the window that
  exists for exactly that launch could never open. It now honours `shutdown` like
  every other window here, and only Linux/BSD consult the environment.

- **A launch that could not ask reported success.** With `enable_console = true`
  on a machine with no display, a second copy called `gui::run`, winit refused
  the event loop, and the launch fell through to "Left the running copy alone"
  -- the line a deliberate *Quit* prints -- and exited 0. A service manager or a
  script then read success from a launch that did nothing. `gui::run` now reports
  whether a window ran, and a copy that never got to ask exits non-zero saying
  so. The headless refusal's advice was also corrected: it recommended
  `enable_console = true` flatly, which for a service is a setting that fails a
  different way.

- **A fifth writer had no directory guard.** The pass that gave four writers
  `ensure_parent_dir` missed `save_dialup_mappings`, because it is a second copy
  of `write_config_file`'s atomic-write pattern in the same file rather than a
  call to it -- and no test wrote that file, which is why nothing caught it. It
  is now split into `write_dialup_file(path, entries)` so it can be tested at all
  (no `chdir`, which would move every other test's relative paths), and the test
  covers the guard, the 0600 mode and the absence of a leftover temp file. The
  log's own `open_log` gained the same guard, but **only inside the data
  directory** -- the first attempt applied it to every log path and broke
  `test_failed_retry_backs_off_further`, which was right to break: `Sink::Paused`
  exists for the volume that has not finished mounting at boot, and creating a
  directory on an unmounted mount point puts the log on the underlying
  filesystem where the real volume then shadows it. Waiting is correct for an
  operator's path; recreating is correct for ours, and the two halves of that
  rule now pin each other.

- **`Data directory:` printed Windows' verbatim prefix.** `canonicalize` returns
  `\\?\C:\...`, and this is the one line the manual tells operators to read
  when they want to know which data directory a copy is using -- a path they do
  not recognise cannot answer that. Stripped, except for the `\\?\UNC\` form,
  where stripping would leave a path that names nothing.

- **The AppImage pin was too blunt.** Pinning `$HOME` whenever no stream is a
  terminal also caught `nohup ./Ethernet_Gateway.AppImage >log 2>&1 &` from a
  deliberate directory. It now never moves away from an `ethernetgateway-data`
  that is already there: an existing folder is the operator's answer to the
  question.

- **Documentation for two copies and for a service.** The manual quoted
  `bindwatch`'s *old* wording, from before the lock landed, so its troubleshooting
  section described a cause the program no longer names. It now quotes the current
  text and explains what a same-directory second copy does instead (offer a
  handover, or refuse and exit non-zero when headless), plus what taking over
  from a systemd-managed copy leaves behind: the unit exits 0, so
  `Restart=on-failure` does not restart it, `systemctl status` shows it inactive,
  and a later `systemctl start` meets the headless refusal until the manual copy
  is stopped. The shipped unit file documents the same three outcomes where an
  operator installing it will see them. All of it measured end to end: holder
  stands down, exits 0, newcomer binds and passes its own port check.

- **The sudo-ownership trap could no longer say what it was.** 0.9.4's own
  diagnosis -- the one that names `sudo chown -R <you> ethernetgateway-data`
  instead of "remove the file" -- became unreachable in the case it was written
  for. Everything the gateway creates lives in one directory now, so a single
  root run leaves *all* of it root-owned, and the first thing a later launch
  touches is not the config: it is the instance lock. The launch died on
  `FATAL: could not claim the data directory ... Permission denied`, which names
  no cause and reads like a second copy is holding the ports, sending the
  operator hunting a process that cannot exist. Measured with a root-owned lock
  file.

  All three paths a root run breaks -- the lock, the launch directory itself and
  the transfer tree -- now share one ownership diagnosis
  (`config::data_dir_ownership_lines`), and every message that offers a `chown`
  builds it from the same helper, pinned by a test that asserts all three agree.
  The lock's own wording says outright that this is *not* the two-copies case,
  because a second copy started here is offered a handover and never reaches it.

- **A fatal startup error is now visible from a desktop launch.** Started from a
  directory it cannot write, the gateway printed its FATAL to stderr and exited
  1 -- and the AppImage's own desktop entry sets `Terminal=false`, so from an
  icon that text goes to the session journal and the operator sees a program
  that does nothing when double-clicked. Measured 2026-08-20: exit 1, no window,
  nothing on screen. The three fatal startup paths now also draw the same lines
  in a small window (`gui::show_startup_failure`), and the message names the
  launch directory **absolutely**, since "the directory this was launched from"
  is not somewhere an operator can go and look without being told which one.

  Offered only when *no* standard stream is a terminal and a graphical session
  exists: being wrong about a terminal in the parting note costs a wrong
  sentence, while being wrong here would cost a modal window blocking a process
  nobody is watching, so `gateway | tee log` stays silent.

- **Which data directory you get no longer depends on how you launched.**
  `ethernetgateway-data` is resolved against the *working* directory, and a
  desktop launch does not define one -- the desktop-entry spec leaves it
  undefined when `Path=` is absent, and `Path=` takes a literal so it cannot say
  `$HOME`. The same AppImage double-clicked from a file manager and started from
  an application menu could therefore land on two different trees: two configs,
  two SSH host keys, two transfer directories, and two per-directory locks that
  know nothing about each other -- `bindwatch`'s surviving "a copy launched from
  a DIFFERENT directory" case, reached by accident rather than by choice. The
  AppImage's `AppRun` now pins the working directory to `$HOME`, but **only when
  no standard stream is a terminal**, so a deliberate launch from a shell (the
  repo's own harnesses do exactly that) keeps the directory it was given. And
  because nothing on screen or in the log could tell two trees apart, startup
  now logs the data directory once, as an absolute path.

- **A writer's directory guard named a constant instead of the path it was about
  to write.** Four writers called `ensure_data_dir` and then wrote to a `path`
  argument -- the same directory only by coincidence. Under `cfg(test)` the
  config path is redirected to a temp file, so the guard created a folder it
  never used: the config tests alone left an `ethernetgateway-data` behind in
  whatever directory `cargo test` ran from, and now leave nothing (measured).
  One `config::ensure_parent_dir(path)` replaces all four. The full suite still
  creates one, but for an honest reason -- the SSH key and CP/M-layout tests
  write to the real default paths, which is what those paths are.

- **The startup banner announced a log file that had failed to open.** In a
  directory it could not write, the log read `Warning: could not open log file
  ... Permission denied` and then, on the very next line, `Logging to ...`. The
  banner asked whether a log file was *wanted*; it now asks the sink whether one
  is *open* (`logger::file_logging_is_paused`) and says so when it is not.

- **Documentation that had drifted with the data-directory move**: the manual's
  appendix table still gave `log_file`'s default as `ethernetgateway.log` while
  its other table had the new path (found by diffing every key documented twice
  -- the two tables now agree); the SSH reference still placed the three key
  files "in the gateway's working directory"; `SECURITY.md` listed the sensitive
  files without saying that the folder's own ownership is what locks an operator
  out of them; and the wizard's troubleshooting note still looked for
  `egateway.conf` beside the binary.

  Every path literal that names the data directory is now pinned to `DATA_DIR`
  by a source-scanning test rather than to a second copy of its spelling -- the
  last pass found two files that never moved because the rule lived in a
  sentence and not in a pattern. Mutation-checked both ways: renaming the
  constant fails the test, and a near-miss literal in another module fails it by
  name.

- **CTRL-C reaches a booted CP/M guest, and you can see that it did.** Reported
  from an SC126: a BASIC `PRINT` loop could not be broken, though the same disk
  broke normally on an Altairduino.

  The keystroke was never the problem — the *backlog* was. A dialled-in session
  was bridged with a flat 64 KB buffer, added "to handle slow baud rates without
  data loss": a buffer cannot lose data on that side, and a large one at a slow
  baud rate buys latency instead. 65536 bytes is **68 seconds** of 9600-baud
  wire and **36 minutes** at 300. A booted guest runs at emulated-CPU speed, so
  a `PRINT` loop fills the whole buffer at once and the caller is reading a
  minute-old screen. Press CTRL-C and the guest breaks *immediately*, while the
  screen pours for another minute — so a key that worked perfectly reads as
  dead, and the break sits in the backlog behind everything already committed.

  The bridge is now sized in **wire time** (about a second, floored and capped),
  so the effect of a keystroke is visible about as fast as a person can notice.
  The boot loop also reads the keyboard *before* writing the guest's output,
  since that write blocks whenever the caller's line is slower than the guest
  prints — which for a talkative guest on a serial line is always.

  Two things this shape of bug punishes, both of which were tried first: a lower
  baud rate and smaller writes each *reduce the drain rate*, which multiplies
  the latency rather than relieving it. The slow screen and the dead key were
  one symptom, not two.

  The two directions are sized **separately**, which needed two `simplex` pipes
  rather than one `duplex`: `tokio::io::duplex(n)` caps each direction at `n`,
  and they want opposite things. Output wants to be small — it is backlog the
  caller has not read. Input wants headroom, because the serial thread writes
  inbound bytes under a *five-second* timeout and treats expiry as a dead call
  (carrier dropped, `NO CARRIER`). A session does not read while it paints, so
  type-ahead or a pasted command arriving during a screen paint has to fit —
  and sizing input from wire time would have made that easier the *slower* the
  line, which is the same trap as the backlog itself.

  **File transfers are unaffected, and that is tested rather than argued.** A
  buffer's size cannot change how long bytes take to reach the peer — only
  whether our writer returns before they are on the wire — and no write in any
  transfer path is wrapped in a timeout, so a paced write has nothing to trip.
  An XMODEM-1K block (1029 bytes on the wire) no longer fits in the bridge at
  once, so it is pinned round-tripping through bridges of 960, 1028 and **64**
  bytes; ZMODEM, which streams and so is the shape that could deadlock rather
  than merely slow, is pinned the same way under a bounded timeout so a
  deadlock fails the test instead of hanging. Only the interactive session
  bridge is resized; the `ATDT KERMIT` bridge is untouched.

- **An unreadable `egateway.conf` now says why, and names a fix that keeps your
  settings.** Reported: after using the modem feature the gateway would no
  longer start at all unless run as root, even with the serial ports disabled,
  on Linux Mint with the user in the `dialout` group.

  One run under `sudo` does it — typically to reach a serial device before a
  `dialout` group change has taken effect, since group membership only applies
  to a new login session. That run leaves `egateway.conf` owned by root at mode
  `0600`. The mode is deliberate and right: the file holds the gateway password
  and the Groq API key. But root-owned *plus* `0600` means the operator's own
  account cannot so much as read it, so every later launch takes the FATAL path
  and the gateway looks like a program that requires root — true only in the
  sense that root ignores file permissions. Disabling the serial ports cannot
  help, because that setting lives inside the file that cannot be read.

  Refusing to start is correct and unchanged: overwriting an unreadable config
  with defaults would turn a permission blip into security-off and password
  `changeme`. The **message** was the defect. It named no cause, and of the two
  remedies it offered, "remove the file" discards every setting the operator
  has while `chown` restores access and keeps them. When a permission error is
  on a file somebody else owns, the diagnosis now says so and prints the
  `chown`. It is withheld otherwise — a claim about ownership must not be made
  about a corrupt file or an I/O error. Measured which state file actually does
  this: an unreadable log, an unreadable SSH host key and an unwritable
  `transfer/` all still start (the last logs `Permission denied` warnings and
  binds anyway). Only `egateway.conf` is fatal.

- **The line printed when the console window closes no longer contradicts what
  just happened.** It was gated on the restart flag alone, so it announced
  "Server still running (Ctrl-C to stop)" on the way out of a shutdown — and
  had been doing that after every Ctrl-C, unnoticed because the wording reads
  as plausible immediately after one. It now consults both flags, and what it
  advises depends on the launch: Ctrl-C from a shell, and a `pkill` when the
  process has no terminal to press it in.

## [0.9.3] - 2026-08-16

### Security

- **The byte trace no longer records passwords.** The per-keystroke diagnostic
  added this cycle sits under *every* prompt in the gateway, so with
  `gateway_debug = true` the telnet/SSH login password, the SSH gateway's
  remote password and the Groq API key were each written to the log one byte
  per line (`cpmkey WIRE 'p' (0x70)`). `log_to_file` ships enabled and the same
  buffer is served at `/logs`, so turning on a diagnostic to chase a stuck key
  also put credentials on disk. The trace is now suppressed for the duration of
  any password prompt. The suppression is an argument threaded to the reader
  rather than a flag on the session, so it cannot outlive the prompt it belongs
  to — an early return anywhere in the input loop would leak a flag. Outbound
  tracing was never affected: a password prompt echoes `*`.

### Fixed

- **The modem pump now traces what it reads off the wire** (under
  `gateway_debug`), one line per read. **Control bytes only** — a printable
  byte is counted, never quoted. This read is one buffer *below* the password
  mute in the session's own trace, so nothing there can protect it, and on an
  `ATDT bbs.example:23` call the content would be the user's password on
  someone else's system. Bulk reads are counted rather than described, so a
  file transfer over the same pump cannot flush the console ring and evict the
  keystrokes the line exists to catch. The existing `cpmkey WIRE` trace is
  logged where the *session* reads, which is two buffers further on — the
  pump's read, then the duplex, then the session — so a keystroke that produces
  no WIRE line could be held in any of the three. That ambiguity sent an
  investigation to the wrong layer; this line answers the one question the
  other cannot, which is whether the byte reached the gateway at all.

- **A single ESC now reaches the remote through all three gateways.** Reported
  from an SC126: `ATDT telnetbible.com:6400` straight from the modem passes ESC
  through fine, but the same host reached through the telnet gateway never saw
  the first ESC — and a second one left the gateway instead. The SSH gateway
  and the serial console bridge had it too; they were three copies of one rule.

  The cause was that an ESC was *held* rather than sent, and forwarded only
  when a following byte arrived — which is how an arrow key (`ESC [ A`) reached
  the remote whole, and which meant a **lone** ESC waited for a second byte
  that never came. Pressing ESC once at a remote's prompt did nothing at all,
  so `vi`, WordStar and every menu driven by the key were unusable through a
  gateway session. The serial menu's own help text has claimed "a single ESC is
  forwarded so editors like vi keep working" the whole time; the code never
  did it.

  Nothing is held now. Every ESC goes to the remote with every other byte, and
  leaving is two ESCs **in a row and within half a second**. *In a row* is not
  redundant with the timer: an arrow key is `ESC [ A`, so two cursor presses put
  two ESCs a few milliseconds apart, and a rule that measured only time would
  throw you out for pressing Up twice — the intervening `[` is what tells them
  apart. The rule lives in one type used by all three bridges rather than being
  written out three times.

  **Note the change in how you leave:** the two presses must now be quick. They
  used to count as a pair however far apart, so an ESC to leave `vi`'s insert
  mode and another a minute later ended the session. The on-screen banner and
  the serial help text say "twice quickly" accordingly.

- **A bare ENTER in the text browser redraws the page instead of throwing it
  away.** The browser prompt moved to a line reader this cycle, which reports an
  empty line where the old one silently ignored it — and the empty line fell
  into the ESC branch, so it reset the browser and returned to the main menu.
  A reader part-way down a long page who pressed Enter — to redraw, or out of
  habit — lost the page, the scroll position, the URL and the history. Every
  other menu ignores a stray Enter, and now so does this one.

- **Following a `<meta refresh>` no longer discards the HTTPS-downgrade
  warning.** Both features landed in this cycle and had never worked together:
  the notice was prepended *after* the refresh was followed, so a site that
  offered no working HTTPS, was fetched over cleartext, and then refreshed to
  another page handed the reader that page with no `[!]` notice at all — on
  exactly the HTTP-only sites the downgrade exists for. The reason is now
  carried across the hop. It is shown when the page in front of you is
  cleartext, so a refresh that lands back on working HTTPS is correctly not
  flagged.

- **A form posts the button it says it will**, whatever the case of its
  `type` attribute (`<button type="Submit">` was not recognised as a submit
  control at all, so a later `<input type=submit>` took both the label and the
  posted name). The button *shown* was the first
  submit control with a non-empty `value`, while the button *sent* was the
  first with a non-empty `name` — two separate decisions that agree on ordinary
  forms and diverge the moment the default button is unnamed, so a form with
  `<input type=submit value="Go">` ahead of `<input type=submit name="btnI">`
  displayed "Go" and posted `btnI`. That is the same swap as the Google
  `btnI` / "I'm Feeling Lucky" defect fixed earlier this cycle, arriving by
  another route. Both now come from one first-control-wins decision, which
  `<button type=submit>` takes part in as well; an unnamed default button posts
  no button field, as in a real browser.

- **`place_bundled_terminals` is parsed like every other boolean.** It was the
  one bool in `egateway.conf` compared case-sensitively, while the config UIs
  used a case-insensitive compare for the same key — so a hand-edited
  `place_bundled_terminals = True` meant `false` at start-up and `true`
  everywhere else.

- **A remote's window title no longer prints in front of every prompt on an
  ANSI terminal.** bash sets the title from its prompt with
  `ESC ] 0 ; user@host: ~ BEL`; no client of this gateway has a title bar, and
  a terminal that does not implement OSC swallows the two-byte `ESC ]` and
  prints the rest — so an SC126 through the SSH gateway showed
  `0;ricky@TelnetBible: ~` ahead of each prompt, which reads as the prompt
  appearing twice. Measured under both EGT80 and QTERM, which is what ruled out
  a fault in either terminal. ANSI clients previously bypassed the output
  filter entirely; they now go through it and lose a *completed* window title
  and nothing else — CSI colour and cursor addressing pass through untouched,
  as they must. PETSCII and ASCII terminals are unchanged: they still have
  every sequence stripped.

  Only the title form is dropped, and only once it completes, because a gateway
  session is a plain terminal proxy and the same stream carries file transfers.
  Run `sz` on the far host and its ZMODEM bytes arrive here; `1B 5D` turns up
  about once per 64 KB of binary, so swallowing `ESC ]` to the next BEL would
  eat part of a download — identically on every retry, so the protocol's own
  CRC could never recover it. A candidate is instead held and released byte for
  byte unless it proves to be `ESC ]`, then `0;` / `1;` / `2;`, then printable
  ASCII, then BEL or `ESC \`, all inside 256 bytes.

  A candidate is also released as soon as the remote goes quiet (100 ms).
  Carrying one across a read is necessary — a burst larger than the 4 KB read
  buffer is split at an arbitrary byte, so a title straddling two reads is
  ordinary — but carrying one indefinitely would deadlock a stop-and-wait
  transfer: a block whose last byte the filter is weighing never completes, the
  receiver times out, the sender resends the identical block, and the identical
  hold repeats, which no CRC can recover from. A stripping terminal releases
  nothing, its pending escape belonging to a sequence being discarded.

  Note the trade: terminal detection classifies any client whose backspace is
  `0x08` / `0x7F` as ANSI, so a modern terminal that *does* have a title bar
  (xterm, PuTTY, the inbound SSH path) loses title updates through a gateway
  session as well. Nothing can ask a terminal whether it implements OSC, and
  the terminals that cannot are the ones this gateway is for.

- **The bundled CP/M terminals are placed at start-up, not only when someone
  enters the emulator.** Erasing the transfer directory and restarting
  recreated the sixteen drive folders with no `EGT8080.COM` or `EGT80.COM` in
  any of them, and none in the transfer directory either. The placement was
  reachable only from the CP/M session path, so the documented behaviour —
  placed when the drive folders are created — was not what happened. The loose
  transfer-directory copies made it plainest: they exist so the file-transfer
  menus can send a terminal to real hardware *without* starting the emulator,
  and they appeared only once you had started the emulator. All four files (two
  builds × drive A: and the transfer directory) are now written whenever one is
  missing, and a file already present is still never overwritten.

### Added

- **New `place_bundled_terminals`** — on by default, on all three interfaces
  (telnet *File Transfer* screen key **T**, the web *File Transfer — More*
  popup, and the desktop *File Transfer — More* popup). It decides whether a
  *missing* `EGT8080.COM` / `EGT80.COM` is written back; it never removes one
  and never overwrites one, because each terminal saves its settings inside its
  own `.COM`. Turn it off if you keep your own build, or your own `EGT80.COM`
  from before 0.9.2, and would rather a file you deleted stayed deleted.

### Changed

- **`cpm_boot_writable` now defaults to `true`** — a booted disk may write to
  its images unless you say otherwise. A vintage operating system saves files,
  formats disks and updates its own directory; booting one read-only made every
  `SAVE` appear to succeed and vanish at the next boot, which is a worse failure
  than losing a disk because it is silent. The reason the setting exists is
  unchanged — a booted guest writes raw sectors and no guard above it
  understands the format — but the disks most people run here come from public
  collections and can be fetched again, so a scrambled one costs a download
  rather than the work on it. Turn it off to keep every disk exactly as it is.
  **Existing installations are unaffected**: the key is written explicitly into
  every `egateway.conf`, so an upgrade keeps whatever it already said. Note that
  re-downloading a disk a guest scrambled means deleting your copy first — the
  sample download never overwrites a file already in the images folder.

## [0.9.2] - 2026-08-15

### Added

- **Both CP/M terminals ship, and both are placed for you.** `EGT8080.COM` —
  written to the 8080's instruction set, which the Z80's is a strict superset
  of, so it runs on any machine here and under either `cpm_cpu` — is the one to
  reach for. `EGT80.COM`, the Z80 build, goes beside it because it carries a
  family of ports the other cannot have at all: a **Z180** board such as an
  SC126 drives its console from the ASCI channels *inside* the processor,
  reached with `IN0`/`OUT0` and found with `MLT BC`. All three are ED-prefixed
  instructions, and on a true 8080 an `ED` byte is an undocumented `CALL` — such
  a probe would not fail, it would jump into the weeds — so those bytes cannot
  exist in a binary that must also run on an 8080. No amount of care in EGT8080
  could serve a Z180 console; the answer had to be a second binary.

  This cycle briefly went the other way, on the reasoning that one binary
  running everywhere beats two that need choosing between. That was right about
  every machine except the Z180, which is the one an SC126 owner has, and it was
  put back before the release. One source either way: `EGT80.Z80` is what gets
  edited and `tools/port8080.py` derives the 8080 file from it, so a change is
  made once and both assemblers' gates run on both.

  Both are now placed **in the transfer directory** as well as on CP/M drive A:.
  Drive A: lives inside `CPM/`, which the file-transfer menus do not list, so
  previously the only way to get the terminal onto a real CP/M machine was to
  start the emulator and send it from inside — backwards, since the reason to
  want the file is usually that the far end has no terminal yet. Neither copy is
  ever overwritten: each saves its settings inside its own `.COM`, and an
  operator upgrading keeps their own configured copy.

- **A port test, on all three interfaces.** *Test ports* on the desktop's
  Server *More…* window and on the web page, `F` on the telnet CONFIGURATION
  menu: it connects to every listener that really bound, at this machine's own
  network address, and reports what answered. A port that does not answer is
  something local blocking it — the desktop and web pages turn the word **Port**
  red beside it and redden the *More…* button that leads to the detail, and the
  telnet screens mark it with a `*`.

  **A pass is deliberately never reported as "open".** On Windows the Filtering
  Platform exempts traffic a machine sends to its own address, and macOS's
  firewall is per-application and does not filter self-traffic — so on those two
  a blocked port answers anyway. The failing direction is trustworthy
  everywhere; the passing direction is worth nothing on two platforms out of
  three. Both popups therefore carry the same platform table (one source in
  `portcheck`, also in the manual at §5.6.1) saying exactly that, with the
  running platform's column marked, and the telnet CONFIGURATION menu carries a
  permanent `* open ports on firewall` line so the advice is there even when
  the check has found nothing.

  It runs once at start-up too — an operator should not have to know to ask —
  and only against listeners that actually took their port, because a listener
  that failed to bind is not a firewall problem and reporting it as one would
  send someone to the wrong place. A result that could not be got at all (no
  network address on this host yet, no route) is a third state and says so;
  reporting it as an answer would be an all-clear the check did not earn.

- **The setup wizard offers to download the sample CP/M disks.** Ticked to begin
  with, on the CP/M screen, saying how many disks, how many megabytes and which
  two repositories they come from before you decide — a CP/M emulator with no
  disks is the state nearly every new operator would otherwise have to dig
  themselves out of. It is an action and not a setting: nothing about it reaches
  `egateway.conf`, and the download starts once *Save and Restart Server* has
  settled the transfer directory it needs. Same one implementation as the
  *Download sample disks* button on every disk screen, so the two offers cannot
  come to disagree about what they fetch, and anything already in the images
  folder is left alone.

- **Four more sample disks, from a second collection.** The downloader now also
  draws on Jim McNeely's `AltairDuino-Disks`, which has disks David Hansel's
  Altair 8800 simulator does not: the **Infocom adventures** hard disk
  (`HDSK04`), **BASIC** (`HDSK05`), **COBOL** (`HDSK06`), **dBase II**
  (`HDSK07`). Thirty-four disks now, up from thirty.

  Both collections, because neither contains the other, and the reason is a
  trap worth recording: McNeely's also holds four files named `DISK13`–`DISK16`
  which are **different disks** from Hansel's of those names — his are CP/M 3.0
  disk 1 and 2, the Felix animation system and CP/M 2.2 MITS+Tarbell, and are
  documented as such, while McNeely's are undocumented in its own catalogue
  (which stops at `DISK12`) and one of them does not boot at all. Taking "the
  better repo" wholesale would have silently swapped four working, documented
  disks for four different ones under the same names. So a manifest line now
  names its own source, the contested four come from Hansel, and only the five
  above come from McNeely. A filename is not an identity — which bit twice: its
  `DISK17` is a name Hansel has no disk for and is his `DISK12` byte for byte,
  so it is not offered either.

  `repodisks.txt` catalogues the new disks — every file on each, read through
  the same mount path the gateway uses — and lists only what that collection
  uniquely has, since 26 of its images are byte-identical to Hansel's.

### Changed

- **The desktop's configuration frames line up.** The six frames are three rows
  of two, and each row's two frames ended at different heights — a floor was
  being applied to some of them, and a floor does not align anything: whichever
  frame's content ran past it grew alone. Raising the floor until nothing
  exceeded it did straighten them, and left a band of dead space under every
  frame that pushed the logo down under the console. So the shorter frame is
  padded to match the taller one instead, from the height its content actually
  came to. The Security frame's hand-cut spacer — added long ago to keep it
  level with Server — is gone with it, and that row is 8 px shorter than before
  rather than taller.

- **The setup wizard's text is no longer against the window edges.** It owns the
  whole window while it is open, so unlike the editor's framed rows nothing else
  was holding it off the glass.

- **The desktop window and the web page now use the logo's own background
  colour** (`#00040e`), which removes the visible edge where the artwork met the
  page.

- **SSH sessions now honour the client's `TERM`.** It arrives in the pty
  request and was being discarded, so every SSH session was assumed to be ANSI
  whatever the client said — a `dumb` terminal was sent colour it cannot
  render, and a Commodore-side client was sent ANSI instead of PETSCII. Telnet
  has always taken this from TTYPE; both transports now go through one
  function, so they cannot come to disagree about what a terminal name means.

- **The CP/M printer captures to a text file by default.** It was `off`, on the
  reasoning that a feature which writes files into the operator's transfer
  directory should be asked for. That rule is right in general and was wrong
  here: nothing is written until a guest actually prints, so someone who never
  uses `LST:` never sees a file — what `off` really bought was that the first
  person to print anything had their output scattered across the terminal with
  no way to get it back. Text rather than `odt` because it is the format that
  cannot disappoint; `odt` still carries overstrike through as real bold and
  underline, and `off` still sends everything to the terminal.

- **The CP/M controls line up.** Every row of the desktop's CP/M popup started
  its dropdown wherever its own label happened to end, and the dropdowns were
  ragged on the right too — `ComboBox::width` is a *minimum*, so a longer value
  grew the box and the edges moved as settings changed. Labels are now
  right-aligned in one column so the colons agree, and every control is exactly
  the same width. The Groq key, Home, Weather location and Units rows join the
  same column, so the popup reads as one form. (Two faults only the screenshot
  caught: the column clipped the `B` off *Booted disk's backspace:*, and the one
  row with a button after its dropdown had the button drawn *inside* the control
  box, on top of a value cut off mid-port.)

- **A brighter logo**, `eglogobrightsmall.png`, in both the desktop window and
  the web page — the same 366×183 as the file it replaces, which the GUI blits
  1:1 because minifying a larger source once put a mauve cast in the gradient.

- **The booted-disk screen is reachable from the desktop, not only a browser.**
  A **VDM / Dazzler…** button sits beside *Mount CP/M Drives…* and opens the
  page — at the screen itself (`/vdm`), on the port the running server is
  actually on, which is the *saved* one: a port typed into the box and not saved
  is a port nothing is listening on.

  It stays one screen rather than two. Rendering the picture natively in the
  desktop UI would mean a second implementation of the VDM-1 grid and the
  Dazzler's four modes to keep in step with the first, for a page the gateway
  already serves.

  If the web server is off, the button **offers to turn it on** rather than
  doing nothing. That offer says out loud that enabling it **restarts the
  gateway** — the listener only binds at start-up, and the restart ends every
  telnet and SSH session in progress — and the operator confirms before any of
  it happens. After the restart the screen opens by itself, so they do not have
  to find the button a second time; the intent rides across in a one-shot
  `open_screen_after_restart` marker, which is cleared as it is read so a launch
  that failed to open a browser cannot keep trying at every start.

- **The web "Disk Screen" button is now "VDM / Dazzler".** It reads like a
  screen *about disks*, which is not what it is: it shows what a booted guest
  paints on a Processor Technology VDM-1 or a Cromemco Dazzler. The page, its
  title and the "may type at a booted disk" setting on the web and desktop all
  say the same thing now.

- **The mount screens now follow what is going to run.** They offered every
  image in the folder against sixteen drive letters, whatever was booting, and
  that produced a real and thoroughly confusing failure: the board an image
  lands on is chosen by its **size**, so mounting a floppy while a hard disk was
  set to boot put it on the 88-DCDD while the guest talked only to the 88-HDSK.
  The disk was mounted, correct, and permanently invisible — and the guest's own
  B: was an empty platter, so it answered `Bdos Err on B: Bad Sector`. The only
  hint was a warning printed *after* the boot had already started.

  Now, on all three interfaces: an image is offered only if the machine that
  will run could actually reach it, and the count that was withheld is shown
  with the reason rather than the list quietly shrinking. Slots are named by
  what the boot disk makes them — `A:`–`P:` under the emulator, `unit 0.0` …
  `unit 3.3` beside a booted 88-HDSK — and named after the *booted* disk's board
  rather than each row's own image, which is why one screen could previously
  show `unit 0.0` beside `Drive 1`, two vocabularies in one list. The telnet
  screen retitles itself **CHOOSE A SLOT** when a disk is booting.

  One `MountContext` answers all of it, so the three interfaces cannot drift.

- **There is one way to boot a disk now.** The telnet CP/M Disk Images screen
  carried a `B  Boot an image`, which ran a disk for one visit and remembered
  nothing, while `cpm_boot_image` decided what the CP/M menu item ran and
  remembered everything. Two boots that asked different questions and disagreed
  about what would happen next was the single largest source of confusion in the
  feature — you could set a boot disk on one screen, boot a different one from
  the other, and have no way to tell which you were looking at. `B` is gone;
  that screen is configuration, and booting is what the CP/M menu item does.

- **New `cpm_boot_writable` — "Disk writes" on the CP/M Boot Settings screen,
  a checkbox in the web and desktop UIs.** The removed picker was the *only*
  place that asked "Allow writes?", and every other route booted read-only, so
  deleting it would have quietly deleted the ability to boot a disk writable.
  The question became a setting instead. Off by default, and it is a stronger
  thing than the question it replaces: a standing setting applies to everyone
  who reaches CP/M, not to one person for one visit, and it covers the boot disk
  **and** every image mounted beside it — which is what a machine with the
  write-protect tabs off is.

  With the picker went its per-disk Backspace question too, so
  `cpm_boot_backspace` is now the whole answer: set `rubout` before booting a
  CP/M 1.x disk.

- **"What CP/M runs" is a picker now, not a cycling key.** `R` on the telnet
  CP/M Boot Settings screen opened nothing — it advanced the setting by one and
  redrew a one-line status. That was fine when the answer was "the emulator or
  the one disk you have"; with the sample disks downloaded it meant pressing `R`
  up to thirty-four times, and one press too many meant going round again. It
  now opens **CHOOSE WHAT CP/M RUNS**: the emulator as entry 1, then every disk
  that boots, paged nine to a screen with the same `P`/`N`/`Q` footer as the
  mount wizard's boot picker, and the current setting marked. Choosing returns
  to the boot screen with the answer on the `Runs:` row. `Q` changes nothing, so
  the list can be looked at without committing to it.

- **The CPU choice reads "Z80 (runs most 8080 code)".** It said "runs 8080 code
  too", which told an operator the setting could not matter to them. It can: the
  processors disagree where it counts — `DCR A` sets parity on an 8080 and
  overflow on a Z80 — so a period diagnostic that identifies its host that way
  is *right* to fail on the Z80, and that is the case `cpm_cpu = 8080` exists
  for. ("most 8080 code" rather than "most 8080 code too" only because 26
  characters is all a 40-column PETSCII row gives.)

- **Booting a disk now confirms on a screen of its own.** The two questions a
  boot asks — allow writes, and how the disk wants Backspace — printed
  *underneath* the boot picker, which is itself at its full 22 rows, so on a
  PETSCII terminal the heading and the top of the disk list scrolled away while
  the operator was being asked to authorise writes to their disks. The new
  screen names the disk, its medium and size, and points at `repodisks.txt` in
  the images folder, which lists what each sample disk holds — the thing you
  want a moment before committing a disk to a boot, and something the picker's
  bare filename cannot tell you.

- **CP/M moved up a level on the telnet menus: CONFIGURATION → `C`.** It had
  been under Other Settings since it was a single on/off toggle, and it is now
  an emulator, a disk-image wizard, a boot picker and a printer — a feature
  area like Serial or File Transfer rather than a general setting. It also
  relieves the screen that needed it most: Other Settings sat at *exactly* its
  22-row PETSCII budget, so the one screen with no room to spare was carrying
  the entry that keeps growing. CONFIGURATION had the room (19 → 20 rows in the
  worst case, with three detected addresses).

  The undocumented `I` shortcut on the CONFIGURATION menu — which jumped
  straight to the disk-image wizard while being displayed nowhere and named in
  no error hint — is gone with it. A key that works but is invisible is the
  same class of defect as one that is shown but does nothing.


### Fixed

- **Mount changes made with the CP/M emulator switched off reported success and
  were silently discarded.** The live table is deliberately not authoritative
  while the emulator is off, so the configuration was written back with its old
  value — but nothing stopped the screens acting, so all three confirmed a
  change that vanished at the next restart. They now refuse with the reason,
  from the one function every mount path already passes through.

- **A controller reporting a longer write than its own buffer would panic the
  session.** The only unguarded index on the disk write path; both ends are now
  bounded and a short buffer refuses the write rather than committing a partial
  sector.

- **A stray `^@` appeared at a booted CP/M prompt, and it was a real keystroke.**
  RFC 854 says a telnet client spells a bare CR as `CR NUL`, and the gateway
  forwarded that padding NUL to the guest as if it were typed. A booted CP/M
  echoes control characters, so it printed `^@` — and it was *in the line
  buffer*, so the next command had to be backspaced clear before it would run.

  It only showed after a command that did no console I/O to swallow it first,
  which is why `DIR` and logging in a drive looked clean while re-selecting the
  current drive did not: reported as `A>b:`, `B>dir`, `B>a:`, then `A>^@`. Only
  the one NUL directly after a CR is dropped, and only on telnet — a NUL
  anywhere else is the peer's own byte, and the LF of a `CR LF` is a real
  newline that guest software wants. File transfers are untouched: they read
  through `tnio`, which documents that it deliberately does no CR-NUL
  processing, because a payload byte is a payload byte.

- **The boot menus no longer offer disks that cannot boot.** Both selectors —
  the `cpm_boot_image` list on all three configuration screens, and the boot
  picker on the telnet mount screens — listed every image in the folder. The
  picker filtered by *size*, which sounds like a bootability test and is not:
  a data disk is the same size as the system disk it carries programs for, so
  all four of the collection's data companions were offered and all four failed
  when chosen.

  Both now ask `BootMachine::bootability`, which **replays the cold start the
  boot session itself runs**, so a list cannot promise what a boot refuses. The
  verdict distinguishes the two failures that look alike: a disk with no boot
  program is withheld, while a disk this machine simply has no *board* for is
  still offered, because that one is a `cpm_boot_machine` setting the operator
  can change and a disk vanishing when they change it would be a worse mystery
  than a boot that fails naming the boards. The answer is cached against each
  file's identity, since one of the callers is a panel that redraws four times
  a second.

  Measured across all five collections: withheld are exactly the data disks and
  second volumes — `DISK0B/0D/0F`, `TDISK06`, `cpm3-2`, `ucsd-*-2`, `z80tests`,
  `dazzler_stuff`, `vio-stuff` and their kin. The mount selectors are
  deliberately **not** filtered: mounting a data disk beside the system disk it
  belongs to is exactly what those disks are for.

  The download manifest is now generated the same way — each candidate is
  cold-started from *the bytes the pinned URL serves* before it earns a line,
  rather than from an exclusion list typed in from a survey run elsewhere. It
  independently reproduced the same four exclusions.

- **`Q` did not leave the CP/M Disk Images screen.** It was displayed as
  `Q=Back` from the day the screen shipped and never handled: it fell into the
  "anything else, ignore it" arm, which redraws the menu, so the one documented
  way out did nothing and only ESC or a bare Enter left — neither of which the
  screen mentions. Handled now, and a test holds every key the screen displays
  against the keys it handles, the same guard the boot screen already had.

- **The telnet boot picker listed ten disk images and stopped.** No page
  indicator, no Next, and nothing saying more existed — so an eleventh bootable
  image could not be reached from the telnet screen, and silently: the disk
  appeared not to be bootable at all rather than not shown. It now pages like
  the mount picker beside it, with `Page x of y` and Prev/Next.

  Nine per page rather than the ten every other listing uses, because this
  screen also spends four lines explaining that a booted disk runs its own
  operating system, and ten entries would overrun the 22-row PETSCII screen by
  one. The page numbers are resolved against the *page*, not the whole list, so
  "1" on page 2 boots the tenth image and not the first — verified live by
  giving only that image a plausible boot sector and watching which one got past
  the check.

- **A space at the terminal-detection prompt turned every space into a delete
  key.** `Press BACKSPACE to detect terminal:` took whatever byte answered it
  as the session's erase character, with no check. Answer it with a space and
  `0x20` became the erase key: from then on every space in a weather location,
  a filename or a password erased a character instead of typing one — and the
  SSH/telnet/serial gateways translate the erase character to `0x7F` on the way
  out, so every space typed at a *remote host* arrived as a destructive DEL.

  Easy to hit because `ATDT ethernetgateway` from inside the CP/M emulator
  opens a **second** detection prompt: the outer session is normally identified
  automatically from the client's announced terminal type and never asks, so
  the one inside EGT80/EGT8080 is the first time many operators see the
  question. Neither terminal was at fault — the same answer broke a plain
  telnet session too.

  Space is now refused as an erase character and the prompt asks again, up to
  twice, naming space specifically. **Only space is refused.** The first fix
  banned every printable byte and was wrong about real hardware: an Apple I
  clone's editing key is the back arrow `0x5F` (`←` in ASCII-1963, before that
  code point became underscore) and the early Unix ttys erased with `#` — both
  printable, both genuine. The retries are bounded and fall through to the old
  behaviour so a relay or a test harness cannot be trapped in a prompt it
  cannot satisfy.

- **A directory entry one block past the end of a disk passed the image
  identification check.** `directory_is_consistent` compared against
  `Format::data_blocks()`, which is a *count*, where the last legal block is
  one less — so the check that decides whether an unlabelled disk may be
  written to disagreed with the mount by exactly one block, and the operator
  got the generic "the CP/M directory in this image is damaged" instead of the
  specific reason. It now derives every number from `Params::derive`, the same
  source the mount uses, which also settles a dormant disagreement about
  allocation-map width at exactly 256 blocks, and it rejects an entry claiming
  a block inside the directory's own area as the mount already did.

- **The declared minimum Rust version could not build the crate.**
  `rust-version` said 1.87 while the code uses let chains, stabilised in 1.88,
  so anyone following the README got a parse error rather than cargo's "this
  crate requires rustc …". The real floor is **1.92**: egui/eframe 0.34 require
  it, so it is the dependency graph and not our own source that decides. A
  gating CI job now builds at the declared minimum — it caught the second wrong
  answer (1.88) on its first run, and `versionchange.txt` now carries the
  command that measures the floor rather than an instruction to reason about it.


### Security

- **`webbrowser` bumped to 1.2.2** for RUSTSEC-2026-0257 (argument injection
  through the Unix `BROWSER` template). It reaches us only through
  `egui-winit`, for opening a link from the desktop GUI — the gateway's own
  text-mode browser is unrelated code — so exposure was limited, but the fix is
  a lockfile bump.

## [0.9.1] - 2026-08-10

### Added

- **The Processor Technology VDM-1 — a booted disk's screen, in the web UI.**
  Some CP/M disks print to no port at all. The VDM-1 was a 1976 S-100 *video
  card*: no serial line, no keyboard, and no data port — a character appears by
  being stored into memory at `CC00`, and the card scans that 1 KB window. A
  disk written for one boots here, takes keystrokes perfectly, and leaves the
  session it was started from blank for ever, because the guest never produces
  a character stream to show. The new `/vdm` page on the configuration web
  server draws the 64×16 grid instead, with the scroll register on port `C8h`
  choosing which line is displayed first.

  **The browser is what makes this work at all.** Repainting a grid into a
  session needs cursor addressing: fine on ANSI, absent on ASCII, and hopeless
  on a 40-column PETSCII C64 that cannot show 64 columns. The deferral notes
  weighed those three terminals against deriving a character *stream* from the
  guest's writes — which needs three heuristics and can be wrong about what the
  guest meant. A page dissolves the question: the repaint is literal, no
  terminal is involved, and whoever is watching need not be whoever is typing.

  **Sampling cannot disturb the guest**, so every booted session offers a
  screen rather than only the disks known to use the card — it is a read of the
  guest's own memory, through its own MMU, with no write, no trap and no change
  in timing. The list marks the guests that have really driven the card, since
  writing `C8h` is a VDM-1 driver's own declaration and inferring nothing is
  better than guessing. Nothing is copied while no browser is watching: one
  relaxed flag per loop, one snapshot per request, no timer anywhere.

  It reaches `TDISK04` (CP/M 1.4, *VDM VERSION*) and `altairsim`'s
  `cpm14-vdm`, which was the last image in the four collections that was real
  work rather than a correct refusal. `DISK11` stays dark: its VDM driver lives
  in a CUTER monitor ROM rather than on the disk, and no scan of an image can
  find code that is not in it.

- **The Cromemco Dazzler — colour graphics from 1976, on the same page.** The
  first colour graphics card for microcomputers, and the VDM-1's problem one
  card along: it reads its picture out of main memory by DMA and gives the
  program no data port at all, so software written for one runs here and shows
  nothing. `/vdm` now paints it too, and a session can have both cards at once
  — which TDISK04 actually needs, since its console is a VDM-1 and `KSCOPE` on
  it drives a Dazzler.

  All four modes: 32×32 and 64×64 in sixteen colours with four bits per element
  in memory, and 64×64 and 128×128 in resolution ×4, where one *bit* is an
  element and the whole picture takes its colour from the format register. The
  picture is four 512-byte quadrants — top-left, top-right, bottom-left,
  bottom-right — which is the part a text-shaped mental model gets wrong, since
  the second page is the right half of the top rather than the next rows down.

  **Measured first, then read.** Before the manual was found, a harness recorded
  what four real programs drive: KSCOPE `OUT 0Eh,81 / 0Fh,30`, DMATION `88/30`,
  SPACEWAR `8C/6C`, GDEMO `EB` and twenty-nine writes of `0Fh`. Every one of
  those decodes correctly under the manual's tables, which is the cross-check
  neither source could give alone. GDEMO also reads `IN 0Eh` **58.8 million
  times** — the card has a readable end-of-frame bit, and a display that
  handled only the writes would leave it polling a value that never changes.
  That port is claimed only once a guest has addressed a card, so a machine that
  has never seen a Dazzler answers exactly as it did before.

  `0Eh` is also a z80pack disk register. The disk controller is matched first
  and keeps it: a colour card the guest never asked for must not cost it a
  drive.

  **The joystick games are not supported and that was a decision.** SPACEWAR,
  GOTCHA, DOGFIGHT, TANKWAR, CHASE, AMBUSH and TRACK read the D+7A analog board
  on ports `18h`–`1Ch` — measured, 66,000 reads in one run. They draw perfectly
  and cannot be played, because nothing a terminal or a browser produces is an
  X/Y voltage. `DISK10.DSK` is Cromemco's whole library and twelve more images
  carry Dazzler software.

- **The screen is a keyboard too** (`cpm_screen_input`, on by default). Click
  the screen page and type: the bytes reach the guest through the *same*
  translation the terminal's own bytes use, so the backspace key chosen with
  `cpm_boot_backspace` behaves identically from either keyboard. **Both work at
  once** — there is one key queue, exactly as two keyboards wired to one port
  would share one — so the person at the terminal and the person in the browser
  can both type at the same guest; simultaneous typing interleaves, which is
  what a shared terminal is rather than a fault. Proved live by typing `ST` at
  the telnet session and `AT⏎` in the browser and watching `STAT` run.

  `Ctrl-C` and `Ctrl-S` reach the guest — `GDEMO` asks for the latter by name.
  `ESC ESC` deliberately does not: ending a session somebody else is sitting at
  is not a keystroke, so a double escape from the browser arrives as two
  escapes. The setting is read per keystroke rather than at start-up, so
  turning it off needs no restart and a page left open stops offering a
  keyboard the first time it is refused. The screen stays readable either way —
  watching and typing are different acts and get different answers.

  This is what makes the Dazzler disks usable rather than only watchable:
  `LIFE` asks `ENTER DATA` and `GDEMO` wants `Ctrl-S`, and neither needs a
  joystick.

- **The gateway can fetch the sample disks for you.** Every disk screen —
  the telnet mount wizard, the web *Mount CP/M Drives* page and the desktop
  window, plus the CP/M settings screen on all three — offers to download them
  into `CPM/images` before you mount anything. About 23 MB, and it is offered
  on the settings screen too because an operator can pick a *boot* disk without
  ever opening the mount screen: an offer nobody sees is not an offer.

  **Only the disks that are known to run** — thirty of the Altair-Duino
  collection's thirty-four, chosen by this project's own boot survey rather
  than by hand. The other four are left out deliberately: three are data
  companions with no boot program and one is a blank, and downloading a disk
  that does nothing is not a favour.

  Pinned to a commit and SHA-256 verified, so what arrives is what was tested,
  and a changed file is refused rather than accepted. **Nothing already in the
  folder is ever overwritten**, and downloads land on a `.part` name and are
  renamed so an interrupted fetch cannot leave half a disk behind. Nothing is
  mirrored: the disks are David Hansel's collection and the software on them
  belongs to MITS, Microsoft and Digital Research, so this fetches them from
  the original repository on the operator's behalf and names the source before
  asking. `test_every_offered_disk_boots_when_downloaded` downloads all thirty
  and boots every one, so "known to run" is a tested claim.

- **`repodisks.txt`, a catalogue of what is on every disk we support.** Written
  into the images folder beside the readme, listing each disk in the four
  collections and the files on it — read through the same mount path the
  gateway uses, so it is each disk's own directory rather than somebody's
  notes, with the address each collection came from at the head of its section.
  A disk with no CP/M filesystem says so instead of showing an empty listing,
  because "no files" and "a layout that is not CP/M's" look identical and mean
  completely different things.

- **Cromemco's bank select on port `40h`**, clean-room from the 64KZ-II
  Instruction Manual: a bitmap of eight 64 KB banks, one bit each, with the
  upper 32 KB common to every bank because the card is two 32 KB blocks each
  placeable in any combination of banks. Cromix — Cromemco's Unix-like
  operating system — needs it, and now boots, banks and configures its console
  instead of sliding through 64 KB of `NOP`. It still stops at `Unable to open
  console`: its TU-ART is armed for interrupts, and this emulator has never
  delivered one to any guest. `iz80` implements interrupt mode 1 only, and the
  TU-ART is a mode-0 device, so that is where it rests for now.

- **A boot that will paint a VDM-1 says so.** A disk whose own system tracks
  write port `C8h` is announced on the boot banner with the address to open —
  and told plainly when the web server is off, which it is by default. Measured
  across all 75 images in the four collections: `OUT C8h` in the system tracks
  fires on exactly the two VDM-1 disks and nothing else. The conjunction first
  proposed (the port *and* an address in the screen window) turned out to be
  unnecessary — 60 of the 75 address that page for reasons that have nothing to
  do with a video card, so the port alone is the declaration.

### Fixed

- **A disk that will not mount now says it might boot.** `HDSK01` and `HDSK02`
  are Altair Hard Disk BASIC and the Accounting System — not CP/M filesystems,
  so refusing to mount them is right, but "no CP/M directory found" sent
  operators looking for a fault in disks that work perfectly. The sibling
  refusal had pointed at the boot picker for months; this one now does too.

- **Refusals are plain ASCII again.** They carried a UTF-8 em dash, which a
  40-column PETSCII terminal renders as three garbage glyphs. The width tests
  could not catch it: a multi-byte character counts as one `char` and three
  bytes on the wire.

- **A value prompt gets its own screen.** The shared "type a new value" prompt
  behind eight telnet menu entries printed underneath a menu already at the
  22-row budget, so it scrolled on a Commodore.

- **The Groq API key says whose it is, and that it is optional** — and it moved
  off the first row of its frame on the web page and the desktop GUI. An
  optional key at the top of a frame reads as something the gateway needs
  before it will work; the weather location took its place.

- **The mount screens' drive letters share one column.** The page and window
  fonts are proportional, so `I:` and `M:` are different widths and every
  control started at a slightly different place down a list of sixteen.

## [0.9.0] - 2026-08-08

### Added

- **z80pack's CP/M 3 works.** It loaded, printed its sign-on and then stopped
  dead, which looked like a broken disk and was not. Banked CP/M 3 needs the
  MMU that z80pack's `cpmsim` provides on ports `14h`-`17h`: bank 0 is the whole
  64 KB, banks 1.. replace only the bottom `segsize` bytes, and everything above
  that is *common* memory shared by every bank — which is how a banked operating
  system keeps its BIOS and its stack reachable while swapping the memory
  underneath them. Measured: `cpm3-1.dsk` writes the bank-select port 284 times
  before it goes quiet.

  **Implementing the MMU was not enough on its own**, and the second half is the
  more interesting one: the disk's DMA still wrote straight into bank 0, so every
  sector landed where a banked guest was not looking. CP/M 3 read its own
  directory as empty and retried the same sector for ever — 1,677 status polls
  and not one console read. z80pack keeps a separate `dma_write` that honours the
  mapping, and now so do we. The disk boots to `A>`, `DIR` lists it and `SHOW`
  reports it.

- **The z80pack hard disk mounts** (`z80packhd`, 4,177,920 bytes) — the last size
  in any of the collections that was a real CP/M filesystem we refused. 2 KB
  blocks, 1,024 directory entries, and **no reserved tracks at all**: the
  directory starts at byte zero and the whole 4 MB is data, which is a
  simulator's disk rather than a machine's. Its parameters came from the BIOS of
  the system disk that uses it, since the volume carries no operating system of
  its own, and were then checked by reading a 21 KB file back through that
  guest's own CP/M and comparing it character for character.

- **CP/M printer: bold and underline are real, a bare CR is now a switch, and
  the text file ends its lines the way CP/M does.** All three came out of one
  measurement — running WordStar 3.0 on a booted disk and looking at what
  reached the printer.

  **The auto-line-feed switch is now yours** (`cpm_printer_autolf`: `auto`,
  `on`, `off`, on all three configuration surfaces). Whether a bare carriage
  return advances the paper cannot be read off the byte stream, and *both*
  answers are in use by period software on the same Altair line printer: Altair
  Hard Disk BASIC's `LPRINT` sends `ALPHA<CR>BETA<CR>` with no line feed at all,
  so a bare CR is its line ending; WordStar emphasises by **overstriking** —
  print the line, bare CR, reprint just the bold run at the same columns. With
  the switch fixed on, as it was, every emphasised fragment landed on a line of
  its own instead of on top of the text. Real interfaces put this on a DIP
  switch for exactly this reason. `auto` keeps whatever was measured for each
  printer, so nothing changes unless you change it.

  **Overstrike becomes real styling.** Bold, underline and both together now
  survive into the `.odt` as proper ODF spans, recorded per column at the moment
  the second pass lands — the plain-text rendition keeps exactly the same
  characters, and a document nobody emphasised gains no markup at all. Verified
  by printing from WordStar on a booted disk and opening the result in
  LibreOffice, which shows `<b>BOLD</b>` and `<u>UNDER</u>`. This is what the
  OpenDocument format was there for; until now it carried none of it.

  **The `.txt` ends its lines CRLF**, not with the host's convention. Measured
  both ends: CP/M's own text is CR LF (`PIP LST:=DEMO.ASM` sent 65 of each), and
  this file is written to be collected onto a C64, a CP/M box or an RC2014 by
  transfer protocols that are binary-transparent — so what is written is exactly
  what arrives, and a bare-LF file `TYPE`d on CP/M staircases down the screen.

- **Cromemco double-density disks mount now — the last CP/M disks in the
  collections that would not.** Two new formats, `cromemcodd` (625,920 bytes,
  single sided) and `cromemcodsdd` (1,256,704 bytes, double sided), which
  between them cover MICAH 64k CP/M 2.2 and Intelligent Terminals Corp 56k
  CP/M 2.2. They booted before; now they mount as a drive as well.

  Both record **track 0 in single density** so that a single-density boot ROM
  can read the disk at all, and everything after it in double density — which
  is why neither size factors into a tidy geometry, and why their reserved area
  is 11,520 bytes rather than a whole number of data tracks. The filesystem
  parameters are the disks' own, read out of a booted guest by calling its
  BIOS's `SELDSK` and following the disk parameter header: a declaration, the
  same class of evidence the machine detector reads out of a boot loader. Two
  independent disks per format agree on every field.

  **The double-sided one says it does not translate sectors and translates
  anyway.** Its `XLT` pointer is zero, which is CP/M's way of saying there is no
  skew, and believing it produced a format that mounted, listed its directory
  perfectly, and returned scrambled file content — the exact failure the Altair
  mapping took four ruled-out hypotheses to escape. That BIOS interleaves inside
  its own `SETSEC` rather than through CP/M's `SECTRAN`, so `XLT` says nothing
  either way. The real translation — an interleave of four within each side —
  was recovered by booting the disk, having its own CP/M `TYPE` a file, and
  locating each of the file's 128-byte records in the image by exact text match.
  The single-sided format interleaves differently again: skew belongs to the
  BIOS, not to the medium.

  Every one of the four disks is checked by a new live gate that compares our
  reader's bytes against the guest's own reading of the same file, including one
  file big enough to cross the double-sided boundary. A directory that parses
  proves nothing on its own, which is the whole reason that gate exists.

- **A vendor volume label no longer makes a disk "not CP/M".** The ITC disks
  carry one as their first directory entry — user byte `0x81`, name "Userdisk" —
  and identification rejected the whole disk on it. CP/M itself never matches
  such an entry, because a directory search compares that byte against the
  current user number; ignoring it is the emulation rather than a relaxation of
  one. Both directory checks skip these records now and still demand a real file
  entry before trusting the disk. Cromemco **CDOS** disks come along as a result
  — their filesystem is CP/M-compatible, and it is verified against CDOS's own
  `TYPE`, so the readme no longer offers CDOS as an example of a disk that is
  the right size and not this filesystem.

- **CP/M can print now, and you get a document out of it.** New `cpm_printer`
  (`off` — the default — `odt` or `text`) captures what CP/M software sends to
  its `LST:` device and leaves an **OpenDocument** or plain-text file in a
  `printer` folder inside the transfer directory, ready to collect over XMODEM,
  ZMODEM, Kermit or any of the others. On all three configuration surfaces:
  telnet Other Settings → `E` → `P`, the web "AI, Browser, Weather & CP/M —
  More" panel, and the GUI.

  Like `cpm_cpu` it reaches **both** CP/M machines, and like that one it gets
  there by two completely different roads. In the emulator the printer is an
  operating-system *service* — BDOS function 5 and the BIOS `LIST` vector — so
  WordStar, MBASIC's `LPRINT` and `PIP LST:=FILE.TXT` all arrive without
  anything to configure. A **booted** disk owns the machine and drives a printer
  *board*, so the gateway has to be one: `cpm_printer_port` says which, and
  today that is the Altair line printer at data register `03h`.

  A job ends after **five seconds of silence**. CP/M has no end-of-print signal
  — a printer is a stream of bytes with no "close", and on real hardware the
  person standing there decided it was finished — so silence is the only signal
  there is. The emulator has a second, exact one: returning to `A>` closes the
  job immediately, so a program that prints and exits does not make you wait out
  a timeout.

  **The awkward part was measured, not reasoned, and it had to be.** Whether a
  bare carriage return advances the paper is not something the byte stream can
  say: a CR that returns the head *without* advancing is how period software
  makes bold and underline, by overstriking; a CR that advances is how a great
  deal of other software ends a line. Real Centronics interfaces carried a DIP
  switch for exactly this. Booting Altair Hard Disk BASIC and watching the port
  settled it — two `LPRINT`s send `ALPHA<CR>BETA<CR>` and no line feed anywhere,
  so with the switch off `BETA` prints on top of `ALPHA` and an entire report
  collapses onto one line. `altair_c` therefore has it **on**, and absorbs the
  line feed of a CR LF pair rather than double-spacing. That measurement is a
  live gate, so a disk that ever disagrees says so.

  Merely *initialising* a printer does not produce a document: answering
  `LINEPRINTER? C` writes a handshake byte to the data port before anything is
  printed, and a job with no printable character in it is dropped rather than
  handed over as an empty file with a notice to match.

  No bold or underline yet — the overstrike is resolved into correct, complete
  *text* rather than into styling, which is the order that cannot silently lose
  content on the way to producing real bold later.

- **The CP/M machines run a Z80 or an 8080, your choice.** New `cpm_cpu`
  (`z80` — the default — or `8080`) on all three configuration surfaces: telnet
  Other Settings → `E` → `B`, the web "AI, Browser, Weather & CP/M — More"
  panel, and the GUI. It is the **only CP/M setting that reaches both
  machines**: where the console, the backspace key and the boot image describe
  a booted disk, and the modem profile describes the emulator, the processor is
  underneath both — the emulator's transient programs and a booted disk's whole
  operating system run on it.

  The Z80 stays the default because it is a strict superset that runs the 8080
  software these disks are made of, because Altairs were very commonly fitted
  with a Z80 upgrade board, and because **EGT80 is Z80 code and declares itself
  so**: on an 8080 the terminal this gateway places on CP/M drive A: loads,
  runs a Z80-only opcode as something else, and takes CP/M down with it. That
  is a real cost of choosing the 8080 rather than a reason to withhold it, and
  it is what the label on all three screens says. Choose the 8080 when you are
  running period 8080 software — notably diagnostics that identify the CPU from
  `DCR A` setting parity rather than overflow, which are therefore *right* to
  fail on a Z80. iz80's 8080 mode is a faithful one, not a relabelled Z80: real
  parity instead of overflow, the 8080's subtract half-carry, its own `DAA`,
  and the unused flag bits forced.

  The machine says which processor it is when it is not the default, and only
  then: the emulator's sign-on adds `8080 selected. EGT80 needs Z80.`, `VER`
  reports `iz80 8080 core`, and a booted disk prints `CPU: 8080.` on the same
  rule its console line already followed. `VER` used to say `Z80 core`
  whatever was running, which is the wrong answer in the one place someone
  looks when an instruction decodes oddly.

- **Cromemco disks boot — the fourth and last board on the disk-controller
  plan.** `src/cpm/cromemco.rs`, the 4FDC/16FDC, and the second user of the
  FD1771 module the Tarbell put in place. All three sample images come up, take
  a `DIR` and run three *different* operating systems: `CDOS version 02.58`,
  `MICAH 64k CP/M version 2.2` and Intelligent Terminals Corp's `56k CP/M`. It
  is measured from the disks' own boot sectors and drivers, the same clean-room
  posture as every board here except the deliberately-derived z80pack device.
  - The console is a **Cromemco TU-ART** at `00h`/`01h` — bit 6 RX, bit 7 TX,
    active high, a convention no other console here uses — so `cpm_boot_machine`
    gains a `cromemco` machine carrying that console and this board. All three
    disks select it under `auto`, which matters most for `CDISK01`: at 256,256
    bytes it is the same size as a Tarbell disk *and* a z80pack disk, so nothing
    but its own boot loader's registers could name it.
  - Two chip features the earlier boards never needed: **512-byte sectors**, and
    a disk that is **two densities at once** — track 0 of a Cromemco
    double-density floppy is recorded single-density so that a single-density
    boot ROM can read it. The arithmetic is what confirms it: 3,328 + 76 × 8,192
    is exactly `CDISK02`'s length, and both double-density directories then
    begin at 11,520.
  - **A fix to multiple-record transfers**, which these loaders need because
    they read a whole track per command. A transfer fetches its next sector on
    the read that *empties* the previous one — a read that has already handed
    the guest a byte — and the machine was re-answering it, losing exactly one
    byte in every 128 and loading an operating system that was almost right and
    did nothing. `HostRequest::ReadAhead` now distinguishes the two.
  - Three things this project had predicted the board would need turned out not
    to exist: a 4 KB ROM monitor at `C000`, the synthesised-ROM mechanism the
    CUTER stub introduced, and memory bank switching. All three came from
    reading a listing carried on a disk rather than the code that runs; the boot
    sectors' `OUT 40h` is what *removes* the ROM, and `CDISK03` then loads its
    operating system straight through `C000`.
- **The console now models a character time, on every booted machine.** CDOS
  reads its console data register twice per character — a lookahead that on a
  real serial line finds the wire still empty. Our console handed over its queue
  as fast as a guest could ask, so the second read consumed the next keystroke
  and discarded it: `DIR` arrived as `DR`, and `ABCDEFGH` as `ACEG`. A person
  typing never provokes it; **pasting a command does**, as does anything driving
  a guest from a script. A received byte is now unreadable until about a
  character's worth of instructions has passed.
- **A disk you drop in the images folder just works, unrenamed.** Two halves,
  both driven by the same idea: read what the disk says about itself instead of
  requiring the operator to say it.
  - **Mounting.** An image needed a format prefix in its filename to be
    writable; without one it mounted read-only. That was stricter than the
    evidence — no two supported formats are the same size, so a size names a
    format outright. An unnamed image is now mounted **read-write** when its
    whole CP/M directory holds together (every allocation block inside the disk,
    no block claimed twice, record counts matching the blocks claimed), and
    read-only *with the reason* when it does not. That distinction matters: a
    UCSD p-System disk is also 256,256 bytes, and so is a Cromemco CDOS one —
    both are correctly refused, as are the Altair Disk Extended BASIC images.
    A prefix is now an override, not a requirement. Two further fixes came out
    of it: an ordinary filename containing an underscore (`my_backup.dsk`) was
    being rejected outright as naming an unknown format, and a blank disk the
    gateway formats itself was refused as "not a CP/M disk".
  - **Booting.** `cpm_boot_machine` now defaults to **`auto`**, which reads the
    ports the disk's own boot loader drives — the same class of evidence as the
    88-HDSK volume label. Four of the five real Tarbell disks and all nine
    bootable z80pack library disks now reach a prompt with nothing configured.
    It reads a declaration rather than guessing: only ports belonging to exactly
    one board count, and when the evidence does not name one machine the
    operator's setting stands. It deliberately will **not** choose a console for
    the Altair boards, because MITS software picks its console from the
    front-panel sense switches at run time and its BIOS carries drivers for
    consoles it never uses — `DISK0E` was detected wrongly on that evidence and
    went silent, so detection is limited to what it can actually prove.
- **z80pack `cpmsim` disks boot, including MP/M and UCSD p-System.** A fourth
  disk device, and the first that is not hardware: it is the interface Udo Munk
  invented for z80pack's `cpmsim`, so unlike every other board here there is no
  manufacturer's manual and the simulator's source is the only specification
  that exists. `src/cpm/z80pack.rs` is therefore **derived work rather than
  clean-room**, labelled as such, with z80pack's MIT notice carried in
  `THIRD-PARTY-NOTICES.md`. Reaches `TDISK03` (Comal 80) and nine disks in
  z80pack's own library: CP/M 1.3, 1.4, a 1975 build, 2.2, 62K-HD, **CP/M 3.0**,
  **MP/M** and **UCSD p-System IV.0**.
  - The machine now picks its **disk controllers** as well as its console,
    because this device answers on `0Ah`–`11h` — which contains both the
    88-DCDD's data register and the 88-2SIO console — and a machine answers disk
    controllers before its console. It is also what settles a size two boards
    both claim: 256,256 bytes is an IBM 3740 to the Tarbell and an 8" SSSD to
    `cpmsim`, and before this the Tarbell took every `cpmsim` disk and could not
    boot one.
  - It is a **DMA** device, with no data register at all — the guest latches an
    address and the sector appears in its memory — so `HostRequest` gained a
    `Dma` variant, the only one that names an address in the guest's own address
    space.
  - Its console **blocks**: the CBIOS reads the data port with no status poll and
    relies on the port to stall the processor. Answering such a read anyway hands
    the CCP a keystroke per instruction, which is exactly what happened — a
    perfect sign-on followed by NULs without end. A blocked guest now re-runs its
    read instead, and that also became the machine's idle signal, since a guest
    with a blocking console never polls console status.
- **Tarbell 1011 floppy disks boot.** A third emulated disk controller, and the
  first whose chip is shared — the Western Digital FD1771 lives in its own
  module because Cromemco's 4FDC and 16FDC use the same part. `TDISK01` reaches
  `TARBELL 62K CPM V1.3 OF 8-13-77` and `TDISK02` reaches
  `Micro Resources 62K CP/M Ver. 2.2 of 1/15/82`, with `DIR`, `STAT`, a file
  that survives a reboot, and `PIP` between two drives. Unlike the Altair
  boards the FD1771's status register means different things depending on the
  command in flight, so the board remembers which command is running and
  assembles status from it.
- **A booted disk's machine is selectable, and two more disks come up.** The
  new `cpm_boot_machine` key says where a booted disk finds its console: the
  Altair 88-2SIO at `10h`/`11h` (the default, and what every disk that booted
  before this used), the 88-SIO at `00h`/`01h`, a board at `04h`/`05h` whose
  status is active low, or that same board printing through a Processor
  Technology **CUTER** monitor ROM, which the gateway synthesises at `C019`.
  That last one brings `TDISK05` to `Tarbell 48K CPM 2.2`, an `A>` prompt,
  `DIR`, `STAT` and a file written with `PIP` that survives a reboot. A disk
  that loads its operating system and then goes silent is usually looking for a
  console that is not there, and this is the setting for it — deliberately a
  setting and never a detection, because what a guest polls cannot distinguish
  the machine it wants from another machine's keyboard at the same address.
- **Boot a disk image.** A `.dsk` can now be cold-booted on an emulated MITS
  88-DCDD controller, and the disk's own operating system takes the whole
  machine — 64 KB, the controller, an 88-2SIO console and the front-panel sense
  switches. Twenty of the twenty-six 88-DCDD images in the Altair-Duino sample
  set reach a sign-on: CP/M 2.2, **CP/M 3.0**, CP/M 2.2AT, **Altair DOS**,
  **Altair Disk Extended BASIC** and **Time Sharing BASIC** V1.1 and V2. The
  three that stay quiet are programs disks — data, not system disks.

  **Booting is not mounting**, and the difference is worth stating: a mounted
  image is one drive among sixteen with our BDOS underneath, while a booted one
  owns every drive and talks to hardware. Inside a booted disk there is no
  jail, no `A>` from us, no EGT80 and no `EXIT`. Two things do carry over: the
  image is opened **read-only** unless you say otherwise, and a **double-`ESC`**
  always gets you back to the gateway.

  **EGT80 runs inside a booted disk and its serial ports line up with the
  gateway's.** Those are two independent settings that have to name the same
  hardware, and now a test walks EGT80's own menus to select a port and then
  moves bytes both ways over it: the 88-2SIO port B (`altair_2sio2`), the
  gateway's own emulated port (`rc2014_1b`) and the original 88-SIO
  (`altair_sio`) all pass, and a deliberately mismatched pair passes nothing —
  which is what makes the other three mean anything.

  Proven end to end and byte for byte: a booted Altair CP/M receives EGT80 with
  its own `PCGET.COM` over the virtual modem at `0x12`/`0x13`, writes it with
  its own BDOS, lists it in its own `DIR`, and sends it back with `PCPUT` —
  18,048 bytes, identical. That exercises the controller both ways, the
  bootstrap, the CPU, the console, the guest's filesystem and both directions of
  our modem port, and needs no knowledge of the Altair block layout at all.

  Reached two ways. The new `cpm_boot_image` key decides what the CP/M menu
  item runs — empty (the default) is the emulator, a filename in `CPM/images`
  boots that disk — and it is a cycling selector on the telnet CP/M settings
  screen (`B`), a **CP/M runs** dropdown on the web page, and the same dropdown
  in the desktop UI. Or pick a disk for one visit from the telnet boot picker,
  which is also where you can allow writes.
- **The virtual modem works inside a booted disk.** A real Altair put its modem
  on the second port of its 88-2SIO, which is exactly the `altair_2sio2`
  profile at `0x12`/`0x13` — so comms software running under a booted Altair
  CP/M finds a UART where it expects one and dials out through the gateway, and
  an inbound `CPM@<ip>` call reaches it. The `aux` and `hbios_*` profiles
  cannot come along, because they are our own BDOS device and RomWBW's
  firmware and a booted disk brings its own of both; nor can a profile whose
  ports would sit on the console or the disk controller. In each case the boot
  banner says so rather than leaving a modem silently missing.
- **CP/M disk images.** A `.dsk` image can be mounted on any drive A:–P:, and
  that drive then reads and writes the CP/M filesystem *inside the image*
  instead of its folder under `CPM/`. Mounting hides a drive folder rather than
  touching it — the files are exactly where they were and come back on unmount.
  Three formats, each measured from real images rather than transcribed:
  `ibm3740` (8" single density — Tarbell, Cromemco, IMSAI/z80pack), `altair8`
  (the MITS Altair 88-DCDD 8" floppy) and `altairhd` (Altair 88-HDSK hard disk,
  the Altair-Duino set).
- **The Altair 88-DCDD floppy reads and writes.** This one was withdrawn for a
  long time — its directory read correctly while file content past the first
  half of a track did not — and it took a different kind of evidence to settle,
  not a better guess. The gateway can now boot these disks, so a booted Altair
  was made to read its own files with its own BIOS and send them out over the
  virtual modem, and every 128-byte slice was located in the image by exact byte
  match: 447 records, eight files, twenty tracks, unambiguous. Two causes, which
  is why no single hypothesis ever fitted. The BIOS translation recovered from
  the disk is correct but maps a record to a *sector ID*, and a sector ID is not
  its position in the file: on the data tracks the odd sectors sit half a
  revolution from where their number says, which every sector states in its own
  header. Tracks 0–5 are written in boot format and have no such shift, so the
  sector translation changes at track 6 — the same boundary as the framing, for
  the same reason.

  Writing works too, which needs two more things. The disk's BIOS states
  `EXM 0` where the standard derivation gives 1, so `Format` now carries an
  explicit extent mask; this is also why `cpmtools` cannot write these disks
  correctly and should not be used on them. And every Altair sector carries a
  checksum the BIOS verifies, so a write refreshes it — both formulas measured,
  and holding for every sector of six real disks. A write touches the 128 data
  bytes and the check byte and nothing else, which means it edits an already
  formatted image; nothing here authors Altair sector headers from scratch.

  Proven the only way that means anything: EGT80 is written into a copy of
  `DISK01.DSK` **from the host**, the disk is then booted, and the guest's own
  `DIR` lists it and its own `PCPUT` sends all 18,048 bytes back byte-identical.
  Each of the three things above fails silently on its own, and that one test
  catches all three. Written up in the new `web/diskreference.html`, which also
  documents how the emulated 88-DCDD controller works — the three ports, the
  rotational sector position and why it had to be built first, the cold start
  and why the payload address is not a guess.
- **Altair hard disks boot.** A 4.9 MB `HDSK*.DSK` now runs its own CP/M on an
  emulated MITS 88-HDSK "Datakeeper" controller — `63K CP/M 2.2b ver 1.5 / For
  MITS 88-HDSK`, an `A>` prompt, `DIR` listing all 48 files and `STAT`
  reporting 3744k free.

  The board is nothing like the floppy controller and that shaped the design.
  The 88-DCDD is a bare board polled through a rotating sector counter; the
  Datakeeper is an intelligent controller with its own processor and four
  256-byte buffers reached through an 88-4PIO, so a sector takes *two*
  commands — platter to buffer, then buffer to the Altair. The booted machine
  now holds a set of controllers rather than one board, each claiming its own
  ports and its own media size, and the seam between them is a byte range
  rather than a track and a sector, because neither board's vocabulary would
  survive being imposed on the other.

  Written from the published manual and its errata, cross-checked against
  observed behaviour — the same clean-room posture as Punter, HBIOS and EGT80.
  Three things came only from the errata sheets, and each is now a test: every
  bit of the error byte reads as 1 on the first read after power-on; a transfer
  length of 0 means 256 bytes, not none; and reading the error byte is what
  clears Controller Ready, which the manual's own sample routine never does and
  is described in the errata as nonfunctional because of it.

  The disk turned out to document itself. HDSK03 carries the assembler source
  of its own boot loader in plain ASCII, and it settles what the manual leaves
  ambiguous: CP/M starts at sector 2, fits entirely in cylinder zero, and is
  loaded by a first-stage program the PROM puts at address zero. Sector 7 is
  that program — its opening `31 00 D7` is `LXI SP,0D700h`, and a few bytes
  later `DB FF` reads the front-panel sense switches to choose a platter,
  exactly as its own comments describe.
- **The 88-HDSK's whole command set, taken from the disks' own source.** Four of
  the hard disks carry the assembler source of the 88-HDSK software itself — the
  CP/M BIOS, and on some of them a controller diagnostic with commands for
  reading and writing IV bytes and a platter formatter — and it defines the
  command set outright. Three of the eight commands had been decoded wrongly:

  - **Read Status** (`CRSTAT 60h`) was unrecognised, so the controller reported
    "finished, no error" and never offered the status byte; a guest polling for
    it would have waited forever.
  - **Format** (`CFORMT C0h`) and **Initialize** (`CINIT E0h`) both decoded as
    Set Byte, because the decoder tested bit 15 before the command nibble — the
    reading the manual invites, since `80h` is the only value it shows with that
    bit set. Both then *succeeded while doing nothing*, which is the same silent
    failure the write bit caused before it.

  All eight now decode on the full nibble, and the two new ones do their jobs:
  Format erases one whole recording surface with the fill byte the images
  themselves show (`E5`, measured across 9,744 uniform sectors), honouring the
  read-only default; Initialize resets the board and, per the errata's own error
  table, needs no ready drive. **Set Byte and Read Status are a matched pair over
  a 256-byte IV store** — a write is remembered and a read returns it, which is
  what the diagnostic's IV byte test asks for. Set Byte's data byte now has a
  phase to arrive in; without one it was being taken as half of the next command.

  The same source also gives the write bit a **second, documentary witness**:
  `CWRSEC equ 020h`, commented "same bit fields as CRDSECT". That is the software
  that shipped with the hardware, agreeing with what we had observed from a
  running guest. It is still recorded as a deviation rather than "the manual is
  wrong" — both witnesses are witnesses to what MITS-lineage software *sends*,
  and the bits may simply be numbered the other way round somewhere in the chain.
- **All eleven Altair hard-disk images boot**, not the four we had tried. Seven
  more turned up on a backup volume and every one reaches `63K CP/M 2.2b`.
- **A blank hard disk now says it is blank.** An erased platter has an erased
  volume label, so the two words that name its boot program both read `E5E5` and
  point 15 MB into a 4.9 MB disk. That was reported as "the boot program runs
  past the end", which reads as a fault in the gateway; and a hard disk naming no
  boot program at all was reported as "this disk is on a controller that cannot
  cold-start one yet", which sends the reader after missing code of ours. Both
  now say **"it is data, not a system disk"**, and the bound is applied where the
  medium's size is known.

- **A booted disk now gets all your mounted images**, each on the controller
  unit its drive letter names — B: is unit 1, C: is unit 2, F: is unit 5. So you
  can mount several disks, boot one, and copy between them with the guest's own
  `PIP`. Verified end to end: two files copied onto two different mounted disks
  inside a booted Altair, each landing in its own image file, both byte-identical
  when read back out.

  **How many the guest can reach is the disk's decision, not ours.** A drive
  letter is a CP/M software concept owned by whichever OS is running — our BDOS
  hands out A: to P: because we wrote it, and stock Altair CP/M hands out A: to
  D: because MITS wrote it. Measured: both 2.2mits and 2.2b answer `Bdos Err On
  E: Select` at E:. The controller offers sixteen units and served fifteen disks
  without complaint; what appears is up to the guest, and nothing here patches
  somebody else's BIOS to change it.

  **Your mounts are lent to the boot, not copied.** A mounted image is a live
  object with its directory cached in memory, and a booted guest rewrites the
  whole file when it leaves — so a drive the boot takes goes out of service for
  the duration and is opened again, fresh, when the session ends, however it
  ends. While it is lent it still counts as yours: it keeps its place in
  `cpm_mounts`, it is shown as held in all three screens, and nothing can change
  it underneath the guest. One session per image is enforced in every direction
  now — a booted image cannot also be mounted, and one image cannot be on two
  drives.

  The boot disk is always unit 0, because although the bootstrap can load a
  system from any unit — measured — the system it loads comes up as its own A:
  and reads unit 0 from then on. Anything mounted on A: sits behind it and the
  boot banner says so. A mounted disk is writable only if the boot session was
  opened for writing *and* the mount is; the stricter wins. An empty unit
  between two full ones answers nothing, exactly as the real board does, so a
  guest that selects one appears to hang — the banner warns which units are
  empty, and ESC twice still gets you out.
- **Make a new blank disk.** An empty, formatted image, from the mount screen
  in all three interfaces — telnet `I` then `N`, and a *New blank disk* row on
  the web and desktop **Mount CP/M Drives** screens. You name the disk and the
  gateway names the file `<format>_<name>.dsk`, because the format prefix is
  what makes an image mount read-write; a blank disk you could not write to
  would be a puzzle rather than a feature. Nothing is ever overwritten.

  For the two unframed formats a blank really is nothing but `0xE5`. The Altair
  is not, and this is the case where "looks fine" is worth nothing: a file full
  of `0xE5` mounts, lists as empty and accepts writes, and is refused by the
  first real machine that reads it, because there is not one sector header on
  it. So it was measured like everything else — MITS's own `FORMAT.COM` was run
  inside a booted Altair against 337,568 bytes of nothing, and its `FULL`
  command initialised and then verified all 77 tracks through our emulated
  controller with no errors. What we generate is required by test to hash
  identically to what it wrote. Two things fell out for free: `FORMAT` prints
  the disk's parameters and they are exactly the DPB read out of the BIOS, and
  its verify pass is a stronger statement about the controller than any test
  here. The loop is closed at the other end too — a disk created and filled
  entirely on the host boots, and the guest's own `DIR` lists the file and its
  own `PCPUT` sends it back byte-identical.
- Mount and unmount from all three interfaces: a wizard on the telnet CP/M
  settings screen (`I`), and a **Mount CP/M Drives** screen in the web and
  desktop UIs. Changes take effect immediately in every session and are saved
  to the new `cpm_mounts` key. A drive somebody is using cannot have its disk
  changed, and the screens show which drives are in use and why.
- `CPM/images`, created with the drive folders, holding a generated
  `readme.txt` that explains the naming convention and what to rename an
  Altair-Duino or IMSAI disk to. Its format table is rendered from the code, so
  it cannot drift.

### Changed

- The CP/M drive folders are created when the emulator is **enabled** rather
  than when someone first launches it — the folders are where you put software
  and images before a first session, not after. Nothing is ever overwritten.

### Fixed

- **Opening a file at an extent opened the wrong one, so every file was a 16 KB
  file.** CP/M 2.2's way of reaching past the first 16 KB is to put the extent
  number in the FCB and call Open; the BDOS positions to it and reports that
  extent's record count. Ours forced the extent, module and record-count fields
  to zero, so a caller asking for extent 1 was handed extent 0 — and because
  sequential reads then returned the right *number* of bytes from the wrong
  place, nothing ever errored. The data was simply wrong.

  Found by running WordStar 3.0 under the emulator: its print overlay is 34 KB,
  it opens `WSOVLY1.OVR` at extent 1 and then 2, and it reported
  `E39 BAD OVERLAY FILE, OR WRONG VERSION OVERLAY FILE` — an error message about
  the *disk*, for a fault in the BDOS underneath it. The same WordStar on the
  same image printed perfectly when the disk was **booted** and its own CP/M did
  the reading, and that is what identified the fault as ours rather than the
  disk's. WordStar prints correctly under the emulator now.

  Anything reading a file over 16 KB by extent was affected, not only WordStar.

- **The images-folder readme is refreshed on upgrade instead of being frozen
  at whatever version you first ran.** It was written once and never touched
  again, on the reasoning that an operator might have annotated it — but the
  file is *instructions*, and this project's own copy was three months stale:
  it still said an image without a format prefix mounts READ-ONLY and must be
  renamed to be writable. That stopped being true when identification learned
  to verify a filesystem — an unprefixed image whose CP/M directory checks out
  mounts read-write just the same — and the stale advice is exactly why this
  repository's own images folder had accumulated hand-renamed disks. It was
  also missing the entire "MOUNTING IS NOT BOOTING" section, which is most of
  what a reader needs. A readme that still starts with our own header is now
  brought up to date and the refresh is logged; a file that does not is the
  operator's and is never touched.

- **The detection survey identified disks by filename, and filenames are not
  unique.** `test_detect_every_real_image` keyed its expectations on the bare
  name, and three basenames collide across the sample sets — `cpm13.dsk`,
  `cpm14.dsk` and `cpm22.dsk` each exist in two z80pack libraries as *different
  disks*. Pointed at a folder it was not written for, it failed on a disk that
  works perfectly: z80pack altairsim's `cpm13.dsk` is "TARBELL 62K CPM V1.3",
  boots correctly, and was reported as a detection bug because cpmsim's
  unrelated `cpm13.dsk` sits in the table. Expectations are keyed on the CRC-32
  of the contents now, and the survey prints how many of the images it scanned
  it actually had an expectation for — a folder none of them is in used to look
  like a pass.

- **The backspace key inside a booted disk is now yours to set.** Type
  `TESTING` into a booted Altair disk, backspace over it, and the screen used to
  read `TESTINGGNIT`. Your terminal's Backspace key sends DEL (0x7F) and most of
  these operating systems read that as a Teletype *rubout* — they delete the
  character and then print the character they deleted, which is right for a
  printing terminal and wrong for a screen. New `cpm_boot_backspace`
  (`backspace` — the default — or `rubout`) says which byte a booted guest is
  handed, and the telnet boot picker asks again per disk, seeded from it.

  There is no answer that suits every disk, and that was **measured across two
  whole disk folders** rather than reasoned: of the 38 images that reach a
  prompt, 29 erase on BS and 7 on DEL, but CP/M 1.3, 1.4 and the 1975 build are
  the *opposite* — the rubout is their editing key and BS prints a literal `^H`,
  so translating breaks something that already worked. Digital Research's own
  CP/M 2.2, MP/M and UCSD p-System accept either. A Commodore's DEL key (PETSCII
  0x14) is folded to whichever byte the setting names, because no guest in
  either survey recognises 0x14 at all — leaving it alone gave a C64 no editing
  key rather than the disk's own one.

  The other half is the way out: a guest's `BS SPACE BS` now reaches a Commodore
  as cursor-left, space, cursor-left rather than as the destructive PETSCII DEL
  that would pull the line about. That makes three PETSCII output translators in
  the gateway, and the test that holds the first two to the same rule now covers
  the third.

  The CP/M **emulator** is unaffected: it reads its own console line and has
  always accepted both bytes.

- **The CPU passes the *undocumented*-flag exerciser too.** `EXZ80ALL` — the
  same ZEXALL family with bits 3 and 5 of `F` pinned as well — reports all 79
  groups OK, under the banner `Undefined status bits taken into account`. This
  had been recorded as a known gap on the grounds that iz80 "does not claim to
  reproduce" those bits; that was wrong. It implements them throughout, from
  Sean Young's *The Undocumented Z80 Documented*, including the two cases where
  they are not a plain copy of the result — the block instructions and 16-bit
  add. Nothing needed changing; the gap was in the notes, not the core.
  `EXZ80ALL` ships on no disk, so the test's documentation now records how to
  assemble it from `ex.mac` and how to validate the toolchain first.
- **The CPU now passes its conformance suite completely.** `EXZ80DOC` — the
  ZEXALL exerciser, documented flags — reports **all 79 instruction groups OK**
  and ends `All tests successful.`, and Supersoft's Diagnostics II reports
  `CPU IS Z80` / `CPU TESTS OK`. The one group that had been failing,
  `<ini,outi,ind,outd><,r>`, turned out not to be an instruction fault at all:
  those instructions copy a byte *from an I/O port* into memory and set the `N`
  flag from its top bit, so whatever an unclaimed port returns lands in the
  group's CRC.
- **A port nothing answers at now reads `0xFF`, not zero.** That is the real
  fix behind the CRC, and it matters well beyond a test. Zero is a *plausible*
  reading — an idle status register, a device present and ready — so guest
  software probing for hardware found boards that were not there. `0xFF` is
  what an unloaded bus gives, because it floats high. Our own booted-disk
  machine already answered `0xFF`; only the CP/M emulator disagreed, on the
  grounds that its guest is "software we chose", which is not true of a feature
  whose purpose is running arbitrary `.COM` files. Every one of z80pack's
  machines defines `IO_DATA_UNUSED 0xff`, and its `cpmsim` records why —
  "unused I/O ports need to return FF, see survey.mac".
- **Two data disks were run as programs instead of being refused.** `DISK0B`
  ("Time Sharing Basic V2 programs") and `DISK0F` ("Altair Mini-Disk DOS
  programs") are the data companions of two disks that boot, and they carry no
  boot program at all — DISK0B's first sector is its volume label,
  `VOL±TS2FILES`, followed by 112 zero bytes, and DISK0F's is two stray bytes
  and 126. Both slipped past the "does this look like a boot sector" check, so
  the machine ran them: DISK0B executed its own label as instructions and
  DISK0F ran through a field of NOPs into cleared memory, and both then sat
  silent — which reads like a disk the gateway cannot boot rather than a disk
  with nothing on it to boot. They are now refused with the same message their
  sibling `DISK0D` already got: "this image has no boot sector — it is data,
  not a system disk".
  - The rule is that a payload which is **more than four-fifths a single
    repeated byte** is padding rather than a program, and the fraction is
    measured rather than argued: across every image in the Altair and z80pack
    collections, taking the payload each controller really extracts, the disks
    that boot run from 5% to 63% and the ones with no boot program are 89% and
    above. Two thresholds reasoned out before measuring were both wrong — a
    half-way rule would have refused the three Altair hard disks, and a
    trailing-zero-run rule would have refused z80pack's `mpm-2`, whose loader
    is short and zero-padded.
- **A mis-named image could be trusted, and the gateway recommended the
  rename.** Naming a format with a filename prefix is an override — it skips
  the directory inspection and mounts read-write — and its size check had only
  a lower bound. A 625,920-byte Cromemco double-density image called
  `ibm3740_x.dsk` cleared the 256,256 that format needs twice over and mounted
  writable, read as single density with its directory landing in the middle of
  a data track. Worse, the refusal an unmountable size produced said "rename it
  with a format prefix, e.g. `ibm3740_mydisk.dsk`" — advice that cannot work,
  since a prefix names the layout rather than the size, and that produced
  exactly that mount if followed. Both ends are now bounded, and the message
  points at the remedy that does exist: a disk we cannot mount is often still
  bootable.
- **Three real disks were refused for being 96 bytes too long.** `DISK13`,
  `DISK14` and `DISK16` in the widely circulated Altair set are an `altair8`
  disk plus a 96-byte trailer, and all three boot. Mounting demanded an exact
  size and turned them away on the file length before reading their directory.
  Both mount paths now allow anything short of one whole record, past which a
  file is a different geometry rather than a padded one — the identical
  96-byte trailer had already been found and fixed on the boot path and left
  here. The three now mount read-write and list coherent directories: two
  CP/M 3 system disks and a CP/M 2.2 tools disk. Nothing was loosened to do it,
  because size was never what made a disk writable — whatever the size lets
  through still has its whole directory checked.

- **A hard-disk image with a few bytes of trailer is no longer refused.** The
  88-HDSK demanded an exact 4,988,928 bytes while the floppy allowed a short
  trailer — the same mistake that had already cost seven perfectly good floppies,
  including both CP/M 3 images. Every controller's media list now states its own
  trailer allowance once and the size test is *derived* from it, so the two
  cannot drift apart again.
- **A virtual-modem profile can no longer be silently shadowed by the hard-disk
  controller.** The list of ports a booted machine reserves for its own hardware
  was written down beside the controllers instead of asked of them, and it named
  only the floppy's three — so a profile landing on `A0`–`A7` was accepted and
  then answered by the controller in the port dispatch, leaving a modem that was
  simply mute. It is now derived from the boards themselves.
- **The images-folder readme said only 88-DCDD floppies boot**, for as long as
  the hard disk had been booting them, because it built its own list from the
  floppy's geometry table. It now renders from the machine's controllers, so a
  board that can boot a medium documents itself. The user manual's mount-format
  list was stale in the same way — it still said the Altair 88-DCDD floppy was
  unsupported, which stopped being true when `altair8` was solved.

- **A booted disk now runs on a Z80 core rather than an 8080.** The Altair
  shipped with an 8080, so that is the more literal machine — but the Z80 is a
  superset that runs all of it, Altairs were commonly fitted with Z80 upgrade
  boards, and the CP/M emulator next door was already a Z80. The deciding case
  was our own: EGT80 is Z80 code and says so in its version line, so on an 8080
  core it loaded, executed a Z80-only opcode as something else and took CP/M
  down with it — the sign-on came back corrupted on the warm boot. All twenty
  bootable images produce byte-identical sign-ons on the Z80 core.
- **A sector was written to whichever track the head had moved to, not the one
  it was written on.** CP/M writes a directory entry on the directory track and
  then seeks away to write the file's data; the controller read the drive's
  *current* track when handing the sector back, so the directory sector landed
  on the data track. A sector whose own header said track 2 was committed to
  track 69, and the guest's next read of it failed its checksum with
  `Bdos Err On A: Bad Sector`. The write is now committed to where it began,
  and a seek or head-unload hands back anything still pending rather than
  carrying it to the new track. Found by having a booted Altair CP/M receive a
  file with its own `PCGET.COM` over the virtual modem — the first time a real
  guest had ever driven the write path.
- `I` — *Mount/unmount disk images* — was listed on the telnet CP/M settings
  screen but only ever handled on the parent menu, so pressing it there
  answered "Press E, C, D, U, or Q." The screen's own error hint had also
  drifted away from the keys it displays.
- The user manual said the CP/M emulator ships **disabled**; it has been on by
  default since 0.8.0. It also gave `cpm_emu_uart`'s default as `off` in the
  table while its own prose said `rc2014_1b`, which is the truth.
- Two malformed tags in the user manual — a `<strong>` opened around an
  `<aside>` and its partner left orphaned — which had swallowed the emphasis
  from a sentence about per-gateway serial configuration.

## [0.8.1] - 2026-08-01

Two months of work on top of 0.8.0, in three strands. **CP/M** grew up: real
`SUBMIT` batch jobs, a `DIR` that honours its operand, the last two BDOS return
values, a clock, and a lock so two sessions cannot write one file at once —
plus the DMA leak between programs that made `PIP` followed by `DUMP` print the
wrong thing. **EGT80** gained transfers from inside a session, sane defaults for
a plain terminal, and lost a filename bug that could open a file the user never
named. And a long **quality pass** across the rest: a log that survives a full
disk instead of stopping silently, a config page that works on a phone, the
real cause of the C64 long-line corruption, and a batch of doc claims that had
quietly stopped being true.

It also started as a response to an external quality review of the tree at
`6c2ed36`, and a follow-up stability pass of our own.

### Added

- **The CP/M emulator now has a clock.** CP/M 2.2 has none of its own, so
  RomWBW software asks the firmware — and our HBIOS answered only the serial
  group. `RTCGETTIM` (function `0x20`) now fills the six-byte buffer at `HL`
  with the host's date and time, each byte BCD encoded per the published
  interface, and `SYSGET`'s `RTCCNT` reports one clock so software that probes
  before asking finds it. Local time where the platform can report it (UTC on
  Windows, which needs an API we do not otherwise link). Setting the clock is
  refused rather than silently accepted: the time is the host's. Verified the
  way the software would use it: a test program assembled *inside the emulator*
  by Digital Research's own `ASM`, turned into a `.COM` by `LOAD`, and run — it
  prints the host's time to the second, in about 23 ms round trip from typing
  the command to getting the prompt back.

- **BDOS 16 (Close File) reported success for a file that was not there.** It
  returned a flat `0`. Writes in this emulator are write-through and there is no
  directory to rewrite, so close genuinely has no work to do — but the *return
  code* still has to be right, and CP/M 2.2's `BDOS22.ASM` has an explicit exit
  for it: "ERROR EXIT: RETURN PARAMETER SET TO 0FFH / MEANING THAT FILE CANNOT BE
  CLOSED", matching the documented contract of 255 when the name is not in the
  directory. Closing an FCB whose file had been deleted, or was never created,
  now reports `0FFH`.

  Two cases return success *without* looking for the file, taken from the same
  listing rather than assumed — `CLOSEF` does `CALL GETRO / RNZ` and
  `CALL FCB14 / ANI 80H / RNZ` with the return parameter still zero: a software
  write-protected drive (a R/O drive is not a close error, there is simply
  nothing to write back), and an FCB whose byte 14 carries CP/M's
  "no directory update needed" flag. Nothing in the existing suite depended on
  close always succeeding.
- **BDOS 13 now returns the temporary-file flag it is supposed to.** Reset Disk
  System always returned `0` in A. Real CP/M 2.2 returns `0FFH` when the drive it
  logs in holds a temporary file, and that value is load-bearing rather than
  decorative: `CCP22.ASM` calls function 13 and stores A straight into its submit
  flag, which is how a command processor discovers a `SUBMIT` batch is already in
  progress. Returning a flat `0` was only *accidentally* harmless because our own
  command processor checks for `A:$$$.SUB` directly.

  Taken from `BDOS22.ASM`'s drive-login scan rather than from memory, which
  changed the rule: the real BDOS compares only the **first filename byte**
  (`SUI '$'`, commented "some sort of TEMPORARY FILE OF THE $$$.EXT VARIETY"), so
  `$WORK.TMP` sets the flag just as `$$$.SUB` does. Checking for `$$$.SUB`
  specifically would have been a plausible-looking deviation. The flag describes
  the drive the reset logs in — A: — not whichever drive was current when the
  call was made.
- **The CP/M emulator runs `SUBMIT` batch jobs.** While `A:$$$.SUB` exists, the
  command processor takes each command from it instead of the keyboard, echoes
  it as it runs, consumes it a record at a time, and erases the file when the
  batch finishes. `SUBMIT.COM` itself is a transient the operator supplies — it
  ships with CP/M 2.2 distributions and with RomWBW — so nothing is bundled.

  Both the file format and the behaviour were established from primary sources
  rather than memory, which mattered because the format is peculiar. DRI's real
  `SUBMIT.COM` was run inside this emulator and the file it wrote was dumped:
  128-byte records, byte 0 a character count, and **the records are in reverse
  order** — so the *last* record is the *next* command. Everything after the
  counted text is uninitialised buffer residue (the dump showed `$ *.COM\r\n`
  leftovers after the NUL), so the count byte is the only safe delimiter and a
  count that cannot fit the record is rejected rather than executed. CP/M 2.2's
  own `CCP22.ASM` confirms both points: it reads record `RC-1` and comments
  "Yes $$$.SUB files are backwards".

  Three inherited behaviours are deliberate, not shortcuts:
  - **An unrecognised command ends the batch** and returns to the keyboard —
    `CCP22.ASM`: "if an error is encountered, the $$$.SUB file is erased".
  - **Batches only work from drive A:.** The processor reads `$$$.SUB` from A:
    whatever drive is current, while `SUBMIT.COM` writes it to the *current*
    drive — verified by running the real binary from B:, which produced
    `B:$$$.SUB` that nothing reads. That mismatch is the origin of the
    historical "SUBMIT only works from A:" rule, and it is reproduced rather
    than papered over.
  - **Leaving the emulator abandons a running batch** instead of resuming it in
    a later session.

  CP/M 3's richer SUBMIT (conditionals, `$1`–`$9` parameters, `PROFILE.SUB`) is
  not CP/M 2.2 and is not emulated; feeding keystrokes to a running *program*
  needed 2.2's separate `XSUB.COM`, also not supported. Documented in the
  manual and `cpmreference.html`.
- **The terminal size reported to a gateway's remote host is now configurable,
  and the SSH gateway no longer ignores what the client told us.** Two new keys,
  `gateway_term_width` and `gateway_term_height`, both `0` for automatic. They
  set what the SSH Gateway sends in its PTY request and what the Telnet Gateway
  sends as NAWS. Automatic means the size the local client negotiated over
  NAWS, falling back to the terminal-type default — 40x25 for PETSCII, 80x24
  for ANSI/ASCII — so nothing changes for anyone who doesn't set them.

  Two problems prompted this. The first is a plain inconsistency: the Telnet
  Gateway already honoured the client's NAWS, while the **SSH gateway
  hardcoded** its geometry from the terminal *type* alone and never looked at
  it. The second is that terminal type does not imply terminal width, and the
  gap lands squarely on the retro hardware this gateway exists for. A C64
  running CCGMS in ASCII mode sends `0x08` for backspace, so it is detected as
  ANSI and was told it had **80 columns for a physically 40-column screen**;
  CCGMS's soft 80-column mode is the same mistake in reverse under PETSCII. A
  WiFi modem or `tcpser` never sends NAWS on the C64's behalf, so nothing but
  an operator setting can carry the truth. When the remote has the wrong width,
  readline computes every redraw past the real margin for a screen that isn't
  there: wrap in the wrong column, backspace deleting the wrong character on
  screen, leftovers after a history recall or a Tab. Nothing is lost on the
  wire — but correcting what you *see* can then run a command you didn't mean.

  Each dimension resolves independently, so pinning a C64's 40 columns leaves
  the row count automatic. An override deliberately beats client NAWS: the
  whole point is to correct a client that cannot report itself. `0` is
  load-bearing on both — it is the only way back to automatic — so neither is
  floored to 1 in any surface. Set from the telnet Gateway Configuration menu
  (`W` / `R`), or Server → More in the GUI and web UI; applies to the next
  gateway connection with no restart. Documented in the manual's new §16.14,
  which is cross-linked with §16.9 both ways because the two share a symptom
  and have different fixes — a bit-banged C64 corrupts the *bytes*, a width
  mismatch leaves them perfectly correct and misplaces only the drawing.

  One resolver (`gateway_window`) answers for both gateways and for the
  `[gw-diag]` line, which now reports the resolved geometry **and which input
  won** (`onward geometry: 40x25 (from config override)`) — previously that
  diagnostic restated "80x24" as a hardcoded string of its own, and would have
  gone on saying it. The web and GUI hint likewise comes from a single
  `Config::gateway_term_hint`.
- **The log file now begins at the beginning.** The log path comes from the
  config, so nothing could be written to disk until the config had been read —
  which meant the version banner and every `load_or_create_config` diagnostic
  (including the FATAL "exists but could not be read" refusal, and "Created
  default configuration") were emitted to stderr only. The file always began
  mid-story, missing exactly the startup diagnostics an operator reading a log
  after the fact wants most. Those lines are now held in a bounded pre-arm
  backlog and written into the file, in order, as soon as one is armed.
  Collection stops at the first `configure_file_logging` call — that is what
  "pre-arm" means, and it is also when we learn whether a file was wanted at all,
  so a process running with file logging off does not accumulate lines nothing
  will read. The window closes for good, so a restart cycle cannot replay the
  startup block; verified with three consecutive `SIGHUP` reloads, after which
  the banner appears exactly once and each cycle's arming line re-anchors the
  version. The drain and the bound are both tested against owned state rather
  than the process globals, so neither test depends on suite ordering.
- **A master that cannot accept relays now says so wherever it is configured.**
  The SSH relay listens on the SSH server's port, so `master_accept_relays` is
  inert while `ssh_enabled` is false. That was warned about at startup and by a
  popup at the moment the role was switched to Master — but neither covers the
  case that actually stranded people: a master set up earlier whose SSH server
  was turned off since, which fires no role-change event and whose startup
  warning has long scrolled away. All three config surfaces now show it
  persistently: telnet renders `Accept relays: ENABLED (SSH off!)` (the qualifier
  rides the existing status row because that screen is at 21 of its 22 PETSCII
  rows, and its help now names both requirements without adding a line), and the
  GUI and web show a warning beside the checkbox. As with the popup, nothing
  switches SSH on for you. The four-part condition is one method,
  `Config::relays_blocked_by_ssh_off`, rather than a copy per surface — the
  lesson from the log keys, where three copies of an "is it on?" rule did drift
  apart. It includes the `relay_transport` check the startup warning already had,
  which the UI copies would have omitted.
- **A size-bounded log file that deletes its old generations.** The gateway
  wrote no log file at all: `logger::log` did `eprintln!` plus two capped
  in-memory rings, so nothing survived a restart and there were no log-related
  config keys. Four new keys, **on by default**: `log_to_file` (`true`),
  `log_file` (`ethernetgateway.log`), `log_max_size_kb` (`1024`) and
  `log_max_files` (`5`). The active file rotates once a write would take it past
  the size cap, `.1` becomes `.2`, and anything past `log_max_files` is
  **deleted** — so worst-case disk use is `log_max_size_kb × (log_max_files + 1)`,
  6 MB with the defaults. `logger::max_disk_kb` states that bound in one place,
  which the three config UIs and the startup line all read rather than each
  multiplying it out. Setting either limit to `0` has a documented meaning (no
  size rotation / keep no history), so neither field is floored to 1 the way the
  other numeric fields are. The file is owner-only (0600 on Unix) because log
  lines name hosts, ports and usernames, and is reopened in *append* mode so a
  restart extends the log instead of discarding the previous run.

  **Deliberately not rate-limited.** A limiter was built and then removed:
  `verbose` logs per protocol block, which legitimately exceeds any sane
  lines-per-second ceiling during a fast transfer, so it would have discarded
  exactly the lines an operator turned verbose on to see. Bounding growth by
  rotation loses *old* data instead of current data. The reasoning is recorded in
  `logger.rs` so it isn't re-litigated. Note this duplicates journald's own
  rotation under systemd; set `log_to_file = false` to rely on the journal alone.

  All four keys are in **all three config surfaces**, per the standing rule:
  telnet Other Settings → `L` (a submenu, because that screen is at its 22-row
  PETSCII budget — the `U` weather-units help entry was folded onto one line to
  pay for the new `L` entry without spilling the help onto a second page), and
  a new **General → More…** panel in both the GUI and the web UI. Each shows the
  computed worst-case disk figure rather than leaving the operator to multiply
  it out, and says so plainly when `log_max_size_kb = 0` makes the file
  unbounded. Documented in `usermanual.html` (both key tables) and
  `web/index.html`.

  The General frame had no More button before this; its button is right-justified
  on the frame's **third line**, sharing the row with the Gateway-debug and
  Show-GUI toggles rather than taking a fourth line — a fourth line makes the
  frame taller than the one beside it and the two stop lining up (measured: the
  General and Serial Port A frames are both 109 px again). It is positioned by an
  auto margin in a wrapping flex row (web) and egui's `right_to_left` layout
  (GUI), neither of which can push the button past its container — deliberately
  *not* the CSS-Grid mechanism whose `1fr` column collapsed to zero and put the
  Server frame's button outside its frame. Re-measured in headless Chrome: all
  six More buttons stay inside their frames across 15 viewport widths from
  1600 px down to 400 px (90 checks, 0 problems).
- **`EGT80.COM` is pinned by hash.** It is compiled into every release with
  `include_bytes!` but no CI runner can rebuild it, so the committed binary —
  not `EGT80.Z80` — is what users actually run. The existing tests check the
  artifact's shape and its version string, which by their own admission
  "cannot catch a code change made without touching the version".
  `test_bundled_egt80_matches_pinned_hash` closes that: the bytes cannot change
  unless the same commit updates the hash. `EGT80/README.md` documents the
  refresh step.
- **Tests for the five Kermit client commands that had none.**
  `kermit_client_delete`, `_rename`, `_type`, `_mkdir` and `_rmdir` had no
  caller *and* no test while compiling into every binary — the
  `#[allow(dead_code)]` note claiming "the unit tests below exercise it
  end-to-end" was true of their nine siblings but not of them. All five now
  round-trip against our own server, including the server-refusal paths
  (missing file, non-empty directory) and the local argument guards.
- **Tests for the CP/M emulator driver's own logic.** `src/telnet/cpm_emu.rs`
  had the tree's thinnest coverage, and its existing tests all validated the
  bundled EGT80 artifact rather than the module. Added coverage of the CCP-lite
  built-ins — `DIR` (empty drive and 8.3 columns), `ERA` (silent success,
  no-match, no-operand), `REN` (both `new=old` and `new old` forms, refusing to
  clobber, missing source), `TYPE` (stops at `^Z`, refuses binary) — plus
  `pending_csi_arrow`, whose split-sequence case matters because it runs on
  buffered input where the rest of an arrow may not have arrived yet.
  `cpmemu_run_program` and `cpmemu_oob_drain` remain uncovered; both need a
  running guest.
- **The CP/M emulator honours read-only files, and BDOS 28 / 29 / 30 / 37.**
  Previously the three functions fell through to the unknown-function arm and
  returned a fake success, and a read-only file was not protected at all: a Unix
  `unlink` is governed by the *directory's* write bit rather than the file's, so
  `ERA` deleted a `chmod -w` file the host user had deliberately locked. Now a
  host read-only file reports CP/M's `t1'` attribute (so `STAT` shows `R/O`) and
  is refused by erase, rename and truncating open. `28` (Write Protect Disk)
  software-write-protects the current drive until the next disk reset, enforced
  in all four mutating paths; `29` (Get R/O Vector) reports the real bitmap
  rather than a hardcoded zero, so a program can see the protection it just
  requested; `30` (Set File Attributes) maps `t1'` onto the host permission, and
  accepts-and-ignores System and Archive rather than faking them, because a host
  directory has nowhere to keep them and a sidecar file would litter the folders
  users drop their own files into; `37` (Reset Drive) releases the write-protect
  for the drives in its `DE` bitmap — its "log the drive out" half is a genuine
  no-op, not a stub, since the directory is synthesised live on every search.
  `ERA` and `REN` now report `File R/O` and `Bdos Err On d: R/O` instead of a
  misleading `No file`.
- **An advisory CI job runs the 25 `#[ignore]`d interop tests.** They spawn real
  lrzsz and C-Kermit peers, and are the only send-path cover for XMODEM/YMODEM
  and the only check that our wire format satisfies somebody else's reader — the
  checked-in replay fixtures lock the receive path, but a fixture can only
  confirm what we recorded, so a send-side regression was invisible to CI.
  Advisory on purpose: the peers come from the Ubuntu archive and can be
  upgraded under us, so a distro bump surfaces as a red X to triage rather than
  a blocked merge. The job records the peer versions, because "did we regress or
  did the peer change?" is unanswerable after the fact without them, and splits
  the two peers into separate steps so one failing still reports the other.

- **Serial-port pickers now name the hardware behind a device path.** A bare
  `/dev/ttyUSB0` or `COM3` says nothing about *which* adapter it is, and the
  moment a machine has two, picking the wrong one produces a port that is simply
  silent with nothing on screen to distinguish them.
  - **Telnet** summarises inline, fitted to the terminal:
    `/dev/ttyUSB0 -- FTDI`. The path is never sacrificed to make room — the
    description is trimmed first, dropped entirely rather than shown as a stub,
    and only a path that overflows on its own is truncated (unchanged
    behaviour). Widths still respected: 30 columns PETSCII, 50 ANSI/ASCII.
  - **Web and GUI** put the short label in the visible row and the full
    description on hover, and hovering the *closed* selector lists every
    detected port with its hardware — the answer to "which ttyUSB is my
    adapter?" without opening the list and reading it item by item.
  - In every UI the value that gets **saved is still the bare device path**; the
    descriptions are display-only.
  - One implementation for all three surfaces: new `serial::DetectedPort` plus
    `list_serial_ports_detailed()`, which `gui::detect_serial_ports` (the web
    server's source too) delegates to. The paths-only `list_serial_ports` is
    gone, having lost its last caller. The list is sorted by path so it cannot
    reshuffle under the cursor between scans — the OS promises no order.
  - The manufacturer leads the short label because that is the word an operator
    recognises ("FTDI", "Prolific"); product strings often repeat across a whole
    family. Full detail reads
    `FT232R USB UART — FTDI (SN A5XK3RJT) [USB 0403:6001]`.
  - An unlabelled built-in UART (a Pi's `ttyAMA0`, an ISA 16550) reports no
    summary on purpose, so its row stays the bare path instead of being labelled
    "Unknown".
  - The `/serial-ports` refresh JSON carries the descriptions now, because
    otherwise clicking refresh silently stripped the names off a page that had
    rendered with them. USB descriptor strings are whatever the device claims,
    so they are JSON-escaped on the wire and HTML-escaped into attributes.
  - `describe_port_type` is pure and unit-tested across the cases a developer's
    desk will not reproduce on demand: manufacturer-only, product-only, neither,
    a maker that merely repeats the product, an unlabelled port, PCI and
    Bluetooth.
  - The GUI's *closed* selector names the hardware too, not just its open list —
    matching the web UI's selected option, since the one state an operator looks
    at most was the one saying the least. Falls back to the bare path for an
    undescribed port, or a saved port that isn't currently plugged in.
- **A slave's Kermit-server-mode port is served by the master.** Previously such
  a port ran a Kermit server on the slave, so a user at the attached machine saw
  and wrote the *slave's* transfer directory — the one place files were never
  supposed to land in master/slave mode, and there was no relay involvement at
  all. The slave now pipes the wire to the master and the **master** runs its
  Kermit server on that channel (new `serial-relay <port> kermit` verb,
  `relay::run_master_relay_kermit`), so a user at the device is talking to the
  master's server: `remote dir` lists the master's transfer directory, an upload
  lands there, and a download is read from there. The slave serves nothing itself
  and keeps no copy, which makes "files always land on the master" true by
  construction rather than by synchronising two directories — no file protocol
  over the relay was needed, because `kermit_server_with_outcome` is generic over
  the stream and the directory belongs to whichever machine runs it.
  - The channel opens as soon as the link allows, since a Kermit server is only
    useful if it is already there when the device starts a transfer, and it
    reconnects with the usual backoff. While the master is unreachable the port
    serves **nothing**: a local fallback would let a transfer appear to succeed
    with the file on the wrong machine, which is the surprise this removes.
  - New master-side key **`allow_relay_kermit`**, off by default and wired into
    all three config UIs. Kermit's server mode has no authentication of its own
    — the same reason `allow_atdt_kermit` and `kermit_server_enabled` are opt-in
    — so serving it to a remote wire is the operator's decision, even though this
    path already requires a slave that authenticated over SSH with
    `master_accept_relays` on.
  - Tested end to end in-process: our Kermit client stands in for the device
    against `run_master_relay_kermit`, proving an upload lands in the master's
    directory and that `remote dir` and a download resolve there — against files
    that exist *only* on the master, so nothing else could have supplied them.
    Plus a grammar round-trip and a refusal test for the gate.

### Changed

- **EGT80 quotes Romans 6:23 on the way out.** Printed after the sign-off,
  before CP/M takes the screen back — wrapped to forty columns rather than the
  eighty the program's other text assumes, because the C64s that reach it
  through the gateway have half a screen and a verse that wraps mid-word is not
  much of a parting word.
- **The README never mentioned the CP/M emulator.** It described the Gateway
  Shell — the pure-Rust `A>` file manager — and stopped there, so the headline
  feature of running real Z80 `.COM` software was absent from the project's
  front page entirely. Added to the summary, the feature list (naming the
  distinction between the two, since both say "CP/M") and the documentation
  links.
- **The CP/M emulator's help now fits the screen it is printed on.** It was the
  one help screen in the gateway that never went through the paginator, and the
  file-loading section added earlier in this release took it to 21 lines, five
  of them over 50 characters — so on a 40-column, 22-row C64 the top scrolled
  away while the bottom wrapped mid-word. It is now paginated like every other
  help, with its lines extracted so a test asserts the real text.

  Twelve help screens had a width test each and fourteen had none, which is how
  this got through: **one test now iterates every screen**, so a new one is
  covered as soon as it is added to the list. The fourteen untested screens all
  turned out to fit — the emulator's was the only offender.
- **The main menu help said the CP/M emulator was "off by default"** (it ships
  on) **and that File Transfer used "the XMODEM protocol"** (it has spoken
  YMODEM, ZMODEM, Kermit and Punter for months). Both corrected.
- **A refused CP/M write no longer holds the name.** The cross-session claim
  has to be taken *before* the write — that is what stops two sessions both
  getting through — but a claim kept for a write that never happened would let
  a guest writing to names that do not exist accumulate entries in a
  process-global map, and lock those names against everyone else for nothing.
  Both the write and create paths release again when the operation fails.
- **The emulator's HBIOS clock scrambles `HL` like every other such call.**
  `RTCGETTIM` takes a buffer address in `HL` and documents only `A` as a
  return, so leaving `HL` intact was looser than real RomWBW — the precise
  permissiveness that once let an `HL` bug in EGT80's own transfers reach real
  hardware while passing here.
- **Two sessions can no longer write the same CP/M file at once.** Every
  session has its own Z80 and its own 64 KB, but they share one set of drive
  folders — and the BDOS here opens the host file per record, so two writers'
  records interleaved into one file and the loser's data vanished with no error
  reported to either. A session now claims a file on its first write (or when
  it creates, erases or renames it) and holds the claim until it closes the
  file or leaves the emulator; another session's write is refused, which
  reaches the guest as an ordinary CP/M failure. Reads are deliberately not
  locked, so sharing a library of `.COM` files stays free. A session entering
  while another is already inside is now told the drives are shared.
- **The emulator's `HELP` says how to get files onto a drive.** It was always
  possible — the drives are folders, so changing directory to `CPM/A` in the
  File Transfer menu and uploading puts the file on drive A: directly — but
  nothing said so, and the alternative guess (upload, then hunt for a way to
  import) has no answer.
- **EGT80's terminal-mode menu no longer offers Settings.** It did, and it did
  not work: the settings screen drew, but keystrokes were still going to the
  remote, so nothing could be selected and the way out was not obvious. The menu
  key now gives `E)xit`, `H)elp`, `U)pload` and `D)ownload` — transfers stay,
  because starting one mid-call without dropping the line is the whole point of
  that menu, while settings are a sit-down task that `E` reaches properly.
- **EGT80 starts in ASCII mode instead of ANSI.** The two failure modes are not
  symmetric: an ANSI terminal shows plain ASCII perfectly, so this costs that
  user only colour, which Settings → `A` turns on; a plain or PETSCII terminal
  showed ANSI as litter printed over every screen, which cost that user a program
  too garbled to find the setting that would fix it. The clear-screen dialect is
  a separate setting and now defaults to the ADM-3A `^Z` to match: shipping an
  ANSI clear under an ASCII default meant the one program that promises not to
  emit escape sequences opened by emitting one. The gateway translates `^Z` for
  whichever client is connected, so an ASCII terminal now receives no escape
  byte at all where it used to receive `ESC [ 2 J`. ANSI remains one keystroke
  away in Settings → `C`, for real hardware with a modern terminal.

  Both are in `EGT80.Z80`, so `EGT80.COM` was rebuilt with the period SLR
  assembler and re-gated through M80+L80 and ZMAC, and its pinned hash updated.
  **Existing installs are unaffected**: the copy on drive A: is never
  overwritten, because it holds your saved settings. Delete `EGT80.COM` to get
  the new defaults.

- **Release builds now keep integer overflow checks on**
  (`[profile.release] overflow-checks = true`). The whole suite runs in the dev
  profile, where an overflow panics, so every test validated semantics the
  shipped binary did not have — and the load-bearing code here is five
  hand-rolled wire parsers doing offset and length arithmetic on bytes from an
  untrusted peer, exactly where a silent wrap becomes a wrong length instead of
  a loud failure. Validated by running the full suite under `--release`: 1599
  passed, 0 failed, no path overflows.
- **CI's deep-fuzz step now covers all 20 property tests, not 3.** The step
  filtered on `qmethod_proptest`, which reached only the telnet
  option-negotiation state machine, leaving every hand-rolled wire parser at
  proptest's default case count. Broadening the filter alone would not have
  worked: `ProptestConfig { cases: N, ..default() }` *overrides*
  `PROPTEST_CASES`, because the env var is only read inside `default()` and a
  struct-literal field wins. The explicit `cases:` is gone where it merely
  restated proptest's own default of 256, and the two deliberately-lower 128s
  (Kermit and ZMODEM, which build a tokio runtime per case) now yield to the
  env var when it is set.

### Fixed

- **A place name from the weather API could carry escape sequences to your
  terminal.** The geocoder's `name` / `region` / `country` / `timezone` strings
  are third-party data printed straight onto the screen, and JSON encodes a
  control character perfectly well as a `\u` escape — so a hostile or
  compromised response could have moved the cursor or cleared a C64's screen
  instead of naming a city. They are now sanitised on the way in, so any future
  display of a geocoded name is covered by construction. The AI-chat path
  already did this to its own API's text; the browser turns out to be covered
  incidentally (html2text drops C0 controls while rendering — verified, and now
  pinned by a test, because that is a dependency's behaviour rather than ours).
- **EGT80 could build a filename out of the previous prompt's text.** BDOS 10
  reports how many characters were typed but leaves the rest of the buffer
  holding the last line, and the filename parser read one byte past the count:
  a name longer than eight characters ending in a dot consumed its dot early
  and then tested that stale byte as the separator, wrapping its counter from
  0 to 255 and filling the type field from whatever was typed before.
  Demonstrated live — typing `ABCDEFGHIJKLM.TXT` and then `LONGFILENAME.`
  opened `LONGFILE.TXT`, a file the user never named. Bounded (three bytes
  into a three-byte FCB field, so nothing was corrupted), but on a download it
  would have offered to overwrite the wrong file.
- **EGT80's upload spent one junk-byte budget for a whole transfer.** The
  receive side refreshes its tolerance for stray bytes on every block; the send
  side set it once, before the handshake. A line that dribbled the odd byte
  between blocks could therefore fail an upload on its 255th one with every
  block acknowledged. Both sides now refresh per block.
- **EGT80's damaged-block fallback is a table, not a register dance.** It
  restored the defaults with a run of loads that shared one accumulator between
  fields, so the value left in A for one field silently became the default for
  the next — and both settings changed earlier in this release hit it: the
  ASCII display mode and the ADM-3A clear were each about to be undone by an
  `XOR A` meant for a different field. It is now one table copied over the
  block, with a CI test pinning it against the shipped defaults field by field
  so the two cannot drift again. Verified by running a deliberately corrupted
  copy: every restored default is correct on screen.
- **The CP/M emulator leaked the DMA address from one program to the next.**
  CP/M's own CCP resets the DMA to 0080H immediately before running a transient
  (`CCP22.ASM`'s `TRANS7` calls `SETDMA` on the line before `CALL TPA`); we did
  not. `PIP` moves the DMA to its own buffer, so a following `DUMP TEST.TXT`
  printed the stale contents of 0080H — the command tail — instead of the file,
  with no error to say so. Every program that reads a record without setting its
  own DMA first was exposed, which is most of them. Only visible when programs
  were run in sequence: each one alone in a fresh session was always correct,
  which is how it survived this long. Found by running the real DRI transients
  from a CP/M 2.2 distribution back to back rather than one at a time.
- **The web config page scrolled sideways on a phone.** Its frames carry a
  deliberate 500 px floor — a frame narrower than its own widest row is what
  pushed the More button outside it — so the page was a fixed 516 px wide
  whatever the screen: still 516 px in a 345 px viewport. Below the floor that
  trade no longer exists (the frame cannot fit either way), so narrow screens
  now re-flow instead of scrolling: one listener per line rather than two, the
  rows that are pinned to a single line allowed to wrap, and nothing wider than
  the screen. Desktop rendering is untouched — every rule is inside a
  `max-width: 640px` query, and the rows, the 7-column listener grid and the
  two-column frame layout were re-measured above the breakpoint to confirm it.
  Measured in headless Chrome at eight widths from 320 px to 1440 px, with
  every popup opened: no horizontal overflow anywhere. (Distinct from the
  earlier fix that kept the More button inside its frame, and from the
  phone-fit pass on the reference doc pages — this is the live config page.)
- **A failed log rotation deleted the log it was rotating.** `rotate()` warned
  about a rename it could not perform and then reopened the active file with
  `truncate(true)` regardless, so a log that could not be moved aside was
  emptied instead — the exact outcome rotation exists to prevent. A read-only
  directory, a `.1` generation held open by another process, or a `.1` that is
  not a file was enough to trigger it. Any failed rename now abandons the
  rotation and says so, leaving every byte already on disk alone.
- **One write error stopped file logging until the gateway was restarted.** The
  sink was thrown away on the first failure, so a full disk, an unplugged
  drive or a momentary NFS hiccup silently ended file logging for the life of
  the process — with nothing in the log to say why it stopped. The sink now
  keeps its settings and *pauses*: it reopens the file after 30 seconds,
  doubling to a 5-minute ceiling, and records how many lines were lost while it
  was away — in the file, and on the console panes, since the GUI and web
  consoles are where an operator would actually notice logging had stopped and
  neither of them shows stderr. This is not the per-line retry rejected earlier — a
  failing disk costs one reopen attempt per interval, not one failed write per
  message — and re-saving an unchanged configuration no longer resets a live
  backoff.
- **A 6- or 7-digit log size was clipped in the web UI.** `log_max_size_kb` is
  a `u64` in KB, so a 1 GB cap is seven digits, but every numeric input was
  pinned to a five-digit width. The value was intact and the box scrolled, yet
  a config screen showing `104857` for a setting of `1048576` reads as data
  loss. Numeric boxes now size to the value they hold, with the tight
  five-digit box as the floor — one rule for every field rather than a second
  CSS class per key. Measured in a browser at four widths, since the visible
  width comes from the CSS, not the `size` attribute.
- **The long-standing `test_cpm_dir_operand` flake is root-caused and fixed.**
  It failed roughly once in thirty full-suite runs and had never been caught in
  the act. It was not the config race everyone assumed: `cpm_dir` resolves the
  configured `transfer_dir` against the process's current directory, the
  shipped value is the relative `transfer`, and a bookmark test changes the CWD
  process-wide and then deletes the directory it changed into. Nothing
  serialised the two, because the config lock guards the config, not the CWD.
  Caught by making the test print its own state on failure, then racing it
  against the bookmark test: 1 failure in 60, reporting `transfer_dir` unmoved
  and the subtree simply gone — which is what ruled the config out. The test now
  uses an absolute transfer directory, which a CWD change cannot re-base:
  0 failures in 120 of the same paired runs.
- **A CRLF checkout made a source-scanning test read the wrong setting.**
  `test_numeric_confirmations_fit_every_screen` delimits each call it scans
  with a `)\n` anchor, which matches nothing in a Windows checkout: the scan
  ran to the end of the file and reported a limit belonging to another
  setting. It failed on windows-latest only. Note the asymmetry, because it is
  easy to over-correct: a *leading* `\n` anchor is safe (it still matches
  inside `\r\n`); only a trailing one breaks.
- **The CP/M emulator's `DIR` ignored its operand.** CP/M's `DIR` takes an
  optional filespec — `DIR afn`, `DIR d:`, `DIR d:afn` — and the built-in
  accepted none of them: it listed the whole current drive whatever you typed.
  That is worse than an error, because `DIR *.COM` returned every file on the
  drive and looked like it had filtered, on the most-used command in the system.

  `DIR` now takes all four forms. Matching goes through `Fcb::matches`, the same
  predicate the BDOS directory search and the built-in `ERA` use, so
  `DIR *.COM` cannot disagree with `ERA *.COM` about what a wildcard covers; the
  listing is deduplicated because a file over 16 KB owns one directory entry per
  extent. A malformed filespec is reported (`TOOLONGNM.TXT?`) rather than
  silently widened to everything — conflating "no operand" with "bad operand"
  would have reintroduced the original bug. `DIR B:` reads the other drive
  without selecting it, as real CP/M does. Verified in a live session across all
  six cases; the directory walk now has one implementation rather than two
  (`list_current` is gone, superseded by `list_matching`).
- **Five numeric settings printed a confirmation wider than a C64 screen.**
  `xmodem_set_numeric` emits its "X set to N unit." line with `send_line`,
  which does not wrap, so an over-long line broke wherever the terminal ran out
  — mid-word, on a 40-column screen. Kermit's idle timeout was the worst at 51
  characters ("Idle timeout set to 86400 seconds (0 = disabled)."), and the four
  protocol negotiation timeouts sat 1 character over at 41. Found by measuring
  every one of the 24 call sites rather than by reading them.

  Fixed in the helper instead of in the callers, so a future caller with a long
  label cannot reintroduce it: a new `numeric_confirmation_lines` composes the
  line and, when it will not fit, splits it so the label stands alone and the
  value clause follows. Nothing is shortened and nothing is lost — the previous
  alternative, trimming labels and units, would have cost the "(0 = disabled)"
  that tells an operator what zero does. Verified on a live PETSCII session:
  the worst case now renders as 14 and 38 characters. A test scrapes all 24 call
  sites out of the source and checks each one's **worst-case** value against both
  screen widths, so raising a setting's ceiling is caught too.
- **The transfer-directory prompt echoed an unbounded path.** Same class,
  found by measuring the rest of the file after fixing the numeric
  confirmations: `Current directory:` and `Transfer dir set to:` printed a path
  of any length through non-wrapping `send_line`, so a real path
  (`/var/lib/ethernetgateway/transfer` is 33 characters before the label) broke
  mid-name on a C64. Both are now truncated to the screen exactly as the
  log-file submenu already truncates `log_file`; an 80-column terminal still
  shows the path in full.
- **The two PETSCII output translators disagreed about ASCII DEL.** After the
  backspace fix the gateway's filter mapped `0x7F` to cursor-left while
  `serial.rs` let it through to render as an arbitrary C64 glyph — a split that
  contradicted the rest of the codebase, where `is_backspace_key` and terminal
  detection both accept `0x08` and `0x7F` as the same key. `serial.rs` now
  treats the pair identically, as the gateway already did.
- **A C64 on the gateway had every long command mangled, because a host's
  backspace was translated into a *destructive* PETSCII delete.** The PETSCII
  output filter mapped ASCII `0x08` to PETSCII `0x14`, and those are not
  equivalents: ASCII BS moves the cursor left and erases nothing, while PETSCII
  `0x14` deletes the character to its left and pulls the rest of the line back.
  The real equivalent is `0x9D` (CRSR LEFT), which is what the gateway now
  sends. This affected every C64 gateway session in PETSCII mode, on any serial
  interface — it is not the bit-banged-timing fault documented in the manual's
  §16.9, though the two look alike.

  **Two independent sites had the identical defect**, so fixing one would have
  been half a fix: `filter_gateway_output` (the SSH and Telnet gateways) and
  `translate_ascii_to_petscii_byte` in `serial.rs` (a C64 dialling out through
  the modem emulator with `AT+PETSCII=1`). Both are corrected, and a test scans
  both translators' bodies so the destructive mapping cannot return to either.
  The CP/M emulator's terminal layer (`cpm_term.rs`) already had it right —
  `PET_LEFT = 0x9D` — which is independent corroboration that the two older
  sites were the outliers. The modem emulator's *own* AT-command echo still
  writes a bare `0x14` on purpose and is correct: there it originates a
  destructive erase rather than translating someone else's stream.

  A host uses backspace two ways and the old mapping broke both, which was
  established by measuring a real `bash` at `TERM=dumb` in a 40-column window
  rather than by reading the spec. The moment a typed line passes the margin,
  readline redraws it and then emits a **run of bare backspaces purely to
  reposition the cursor** — eleven of them in one observed case — and each one
  used to *delete* a character the host still believed was on screen. Separately,
  readline erases with the universal `BS SPACE BS`, which became
  `DEL SPACE DEL`: delete a character, insert a space, delete that. Short
  commands never trigger a redraw, which is exactly why they always looked fine
  and long ones did not. With `0x9D`, `BS SPACE BS` renders as left, space, left
  — the character is overwritten with a blank and the cursor ends up before it,
  which is what the host means.

  The same measurement cleared the obvious suspect: at `TERM=dumb` readline emits
  **no CSI sequences at all** (it has no clr_eol or cursor-addressing capability
  to use), so the PETSCII path's escape-sequence stripping was never involved.
  The test that had pinned the old mapping asserted `0x14` with no stated reason
  and never covered `BS SPACE BS` at all; it now states why `0x14` is wrong and
  covers both uses. Documented in the manual's new §16.15.
- **The PDF manual was silently clipping content, and had been for some time.**
  Adding one sentence to a config-key row exposed it: the five-column key appendix
  is wider than the printable text block, and WeasyPrint neither shrinks nor clips
  a table to fit — it overflows the page, so the **entire Description column ran
  off the right edge**. Rows ended mid-sentence ("Outbound telnet gateway:") with
  nothing to indicate anything was missing, across roughly a hundred rows. The
  cause was long unbreakable `<code>` keys setting the column's minimum width;
  `overflow-wrap: anywhere` on cell code lets them split so the table fits. That
  scope is load-bearing in both directions and each alternative was rendered and
  inspected: applied to every *cell*, the narrow Type / Default / Range columns
  collapse to one character per line; applied only to the first cell, the table
  overflows again because `password` / `key or password` are unbreakable too.
- **Two `<pre>` blocks in the PDF were clipped the same way**, including the
  all-listeners-failed sample, which lost its port list after "telnet 2323, we".
  `pre` carried `overflow-x: auto` — a scrollbar in a browser, but print has
  nowhere to scroll. WeasyPrint had been reporting this on every single build
  ("Ignored `overflow-x: auto`, unknown property") and the warning was being
  filtered out of the build output; it was describing exactly this defect.
  `white-space: pre-wrap` now wraps those lines instead of losing them.

  Both fixes verified by measurement rather than inspection: every page is
  rendered to a bitmap and the right-most inked pixel compared against the text
  block's edge. **106 pages, 0 overflowing** — from 2 pages plus the appendix
  table before.
- **The web let you tick TTYPE/NAWS negotiation while raw-TCP mode was on**, where
  the GUI greys it — raw TCP has no IAC layer, so the setting is meaningless
  there. Found by comparing every `add_enabled_ui` gate in the GUI against the
  web's rendering, which left exactly this one gap once the log fields were
  matched. This is the *dangerous* shape rather than the safe one: being a
  **checkbox**, greying it means the browser omits it, and "absent means false"
  would store `false` over the operator's setting — so turning raw mode on and
  saving would silently lose it, and turning raw mode back off would leave
  negotiation unexpectedly disabled. All three places are wired: the renderer
  greys it, `updateGatewayFields()` re-enables it the moment raw is unticked, and
  the save skips it while it is greyed.

  The skip was a role-specific list (`BOOL_KEYS_SKIPPED_OUTSIDE_MASTER`) that
  could not express "greyed by raw mode", so it is now a predicate,
  `bool_checkbox_gated_off(key, submitted_form)`. It reads the condition from the
  **submitted** form rather than the stored config, because the operator may have
  changed the gating control in the same save and it is the submitted state that
  decides what the browser was able to send. The general guard now asks that
  predicate instead of the old constant — an earlier version checked the constant
  directly and would have rejected this key, whose gate is raw mode rather than the
  role. Verified live: greyed on load with raw on, un-greys on untick, re-greys on
  re-tick, and a save in raw mode left `telnet_gateway_negotiate = true`
  untouched. Mutation-tested — removing the skip fails two independent tests.
- **The web left the log path/size/keep fields editable while file logging was
  off**, where the GUI greys its equivalents. They now grey to match, and the
  earlier decision not to do this was over-cautious: it treated the
  `allow_relay_kermit` bug as a general hazard of greying, when in fact that bug
  turned on `allow_relay_kermit` being a **checkbox**. An absent checkbox is the
  canonical `false`, so a greyed box *clobbered* the stored value — whereas
  `collect_form_updates` writes a **plain key** only when the form contains it, so
  an unsubmitted text or numeric field leaves what is stored alone. These three
  are plain keys and need no entry in the save's skip-list. Proven end-to-end
  rather than argued: saving the whole form with the fields greyed leaves
  `log_file`, `log_max_size_kb` and `log_max_files` byte-for-byte unchanged in
  `egateway.conf`, and ticking the box then editing them writes the new values.
  The other half of the relay bug — rendered `disabled`, never re-enabled by JS —
  is covered by `updateLogFields()`, verified in a real browser: the fields
  un-grey the instant the box is ticked and re-grey when it is unticked, with no
  reload. Gated on `log_to_file` alone rather than on
  `logger::file_logging_enabled`, because a blank path also means "off" and
  greying the path field *because* it is blank would make it impossible to fill in
  — the GUI gates on the same flag.

  The guard that should have caught this only scanned **checkboxes**, so text
  inputs gaining a `disabled` attribute were invisible to it. The new
  `test_disabled_inputs_are_re_enabled_by_js` scans every disabled input, requires
  JS that can re-enable each one, and asserts the checkbox/plain-key asymmetry in
  both directions so the reasoning is pinned rather than remembered. Rendered in
  the `standalone` role deliberately — a *slave* leaves the four `slave_*` fields
  enabled, so that role would cover fewer inputs; standalone greys all nine the
  page can ever grey. Mutation-tested: dropping a field from the JS list, and
  inverting the gate, each fail with the right diagnosis.
- **A new test looked like coverage and proved nothing.** The guard for "arming a
  log file must not replay the pre-arm backlog on a later restart" asserted only
  that two `Option::take` calls in a row yield `None` — which passes no matter
  what the production code does, and by the time it ran another test had usually
  already closed the window, so both takes returned `None` and the assertion was
  trivially true. Demonstrated by mutation: replacing the retire's `take()` with
  `clone()`, so the backlog *would* be replayed into every subsequent log file,
  left the test passing. It now drives the real thing — seed the backlog, arm one
  file and assert the line lands, arm a second and assert it does not — and fails
  on that mutation with a message naming the consequence.
- **The web page still told operators the log settings were under Server.** The
  panel moved to General in the same commit that updated `usermanual.html`, but
  `web/index.html` was missed, so the two docs disagreed about where to find a
  setting.
- **The log-state hint existed twice and had already drifted.** The web and the
  GUI each carried their own copy of the same three-branch message, and they had
  diverged: the GUI's read "the console **above** only", which is wrong in a popup
  — the console pane is behind it, not above it. Both now call
  `logger::log_state_hint`, which also owns the "a blank path is off too" rule, so
  the wording and the on/off decision cannot disagree between surfaces. The
  numbers are arguments rather than read from the config, which is what lets the
  GUI's figure track a half-typed field instead of lagging a keystroke behind.
- **Two of the new logger tests raced each other through the process-global log
  sink.** `configure_file_logging` replaces one process-wide sink, and nothing
  serialised the two tests that arm it, so one test's writes landed in the
  other's file while its rotation sequence stopped advancing — reproduced as
  `left: ["t.log", "t.log.2"]`, the `.1` generation missing, on the first
  iteration of stressing `logger::tests` with four threads. It had already been
  seen once as a 1-in-19 failure of the full suite. Serialising the sink-arming
  tests turned out to be necessary but *not* sufficient: nothing serialises the
  ~1650 tests that merely call `log()`, and each of those appends to whichever
  sink is armed, which against a 200-byte cap rotated the newest line out of the
  file the test was asserting on. So `write_to_file` is now a thin wrapper over
  `write_line_to(&mut FileSink, line)`, and the rotation test drives a
  **locally-owned** sink — removing the shared state instead of racing it. The
  remaining test that genuinely exercises the global path takes a
  `FileLogTestGuard` that both serialises and, importantly, disarms the sink from
  `Drop`: a failed assertion returns early, and a sink left armed at a
  since-deleted temp directory would make every later `log()` in the suite fail.
  Same lesson as `ConfigTestGuard` — cleanup belongs inside the critical section.
  Verified with 40 consecutive stress runs where it previously failed on the
  first.
- **The startup banner could claim "Logging to " with no filename.** A blank
  `log_file` disables file logging exactly as `log_to_file = false` does, but the
  banner consulted only the flag — so a blank path printed a line promising a
  file, a rotation size and a disk bound while nothing was being written. The
  same rule had been open-coded in three places (banner, web hint, GUI hint),
  which is precisely how they came to disagree; there is now one predicate,
  `logger::file_logging_enabled`, and all four surfaces (including the telnet
  screen, which now shows `(not set)` and `n/a`) ask it. Pinned by a test that
  asserts the predicate and the policy agree on every combination.
- **The log file did not record which build wrote it.** The version banner is
  emitted before the config exists — necessarily, since the log path comes from
  the config — so the file began mid-story, and because it is *appended* across
  restarts a reader could not tell which build produced a given stretch. The
  arming line now carries the version, and it is the first line in the file.
- **The four new log keys could not be set from the telnet or web UI at all.**
  They had a parser, a writer, a struct field and a default — everything that
  makes a key look wired — but no arm in `apply_config_key`, which is the path
  `update_config_value` / `update_config_values` take. So both of those UIs
  collected the value, wrote it into the update batch, and had it silently
  dropped by the `_ => {}` fallthrough. The GUI was unaffected, because it
  persists the whole `Config` struct instead — which is precisely what hid the
  bug: the setting demonstrably worked in one surface. Caught by driving a real
  save through a headless browser against a scratch gateway and then reading
  `egateway.conf`, not by a unit test; the form-collection test passed
  throughout, because the drop happened one layer below it.
  `test_every_written_key_can_be_applied` now closes the whole class rather than
  these four keys: it scans this file's own source for the keys the writer emits
  and the keys `apply_config_key` matches, and fails with the offending names if
  any key is persisted but unsettable. (A `match` can't be introspected at
  runtime, and the fallthrough makes an unhandled key indistinguishable from a
  handled one, so source-scanning is the only guard that works here — the same
  technique that found the third over-wide `show_error` literal.)
- **The known flaky Kermit test had a general cause, and it is now structurally
  impossible.** Tests that point `transfer_dir` at a scratch directory restored
  it with a bare `update_config_value("transfer_dir", "transfer")` *after* the
  locked helper had already returned — cleanup outside the critical section.
  That is a race, and a wide one: `update_config_value` does a full
  read-modify-write of the on-disk config, so the restore takes milliseconds
  during which the next test can take the lock, point `transfer_dir` at its own
  directory and start its server, only for the previous test's restore to land
  on top and aim the running server back at `transfer`. The victim fails with a
  wrong listing and passes on re-run. A new `ConfigTestGuard` takes the lock,
  snapshots the real previous value, and restores it from `Drop` — before the
  lock is released. All 40 acquisition sites and all 43 hand-written restores
  are gone. `test_server_g_dir_returns_listing` was the one that had been
  observed flaking; the mechanism applied to every test here that sets a
  `transfer_dir`.
- **An unreadable dialup-mapping file silently deleted every mapping.**
  `load_dialup_mappings` returned an empty list for *any* read failure, and the
  telnet "add mapping" screen does load → modify → `save_dialup_mappings`,
  which rewrites the file wholesale. So with a present-but-unreadable file, an
  operator adding one number replaced all the others with it, with no error
  shown. This is the same defect class as the `save_known_host` one below,
  found by sweeping for it. New `try_load_dialup_mappings` surfaces the failure;
  read-only callers (dial lookup, the listing screen) still degrade quietly,
  while the mutating path refuses and says why. A missing file is still first
  run and still seeds the default mapping.
- **Two new error messages were too wide for a 40-column PETSCII screen.**
  `show_error` emits a single unwrapped line; the host-key and dialup refusals
  added in this release were 99 and 123 characters. Both now use
  `show_error_lines` with pre-split lines, matching the convention everywhere
  else.
- **`web/index.html` repeated the Kermit "client and server modes" claim** that
  README was corrected for. Both now describe send/receive plus server mode.
- **The test suite took nine minutes, and two tests were 96% of it.** Measured
  before and after: `kermit::tests::test_client_cwd_unsafe_path_returns_error`
  300.02s → 0.01s and
  `xmodem::tests::test_xmodem_receive_nak_on_out_of_sequence_block`
  220.02s → 0.01s, taking the whole suite from **539.53s to 53.51s**. Both now
  use `#[tokio::test(start_paused = true)]`, already the house pattern. Neither
  was a product bug — both were waiting out a real timeout that the code was
  right to impose. The Kermit test is the only `test_client_*` that doesn't end
  its session, and the shared helper awaits the server task, so it sat through
  `kermit_idle_timeout`; the XMODEM one sends a single CAN, which is correctly
  *not* an abort (CAN×2 is required), so the receiver spent its full retry
  budget. CI runs this suite on three operating systems on every push.
- **An unreadable known-hosts file no longer reads as "new host".**
  `check_known_host` collapsed every `read_to_string` error into `Unknown`,
  conflating "no file yet, pin on first contact" with "a file that exists,
  may hold a pin for this host, and won't open". There is a new
  `HostKeyStatus::Unreadable`, and only `ErrorKind::NotFound` still means first
  contact. It matters most on the **relay** path, which auto-pins with no
  prompt and then sends the master's unified credentials — a transient read
  failure there silently re-pinned whatever key was presented. Both callers now
  refuse: the relay with an `Auth`-class error (so it backs off rather than
  hammers), the interactive SSH gateway with a message naming the real problem
  instead of a "host key not recognized" prompt that would have been untrue.
  `save_known_host` had the matching flaw — it read with `unwrap_or_default()`
  and then rewrites the whole file, so an unreadable-but-present file would have
  been treated as empty and replaced by a single entry, discarding every other
  pinned host. It now declines to write and says why.
- **`SECURITY.md` listed 0.3.x as the supported release**, five minor versions
  behind, so a user on the current version didn't appear in the table at all.
  Reworded to "latest released 0.x" so it cannot go stale again, and added to
  `versionchange.txt`.
- **`README.md` understated the minimum Rust version** as 1.85 against
  `Cargo.toml`'s `rust-version = "1.87"`, so following it on 1.85 or 1.86 hit an
  MSRV error at the first build. The README's toolchain line is now on the
  `versionchange.txt` checklist too.
- **`README.md` overstated Kermit client support.** It advertised "client **and**
  server modes"; the client G-command state machine is real and tested but
  reachable from no telnet, web or GUI surface. The bullet now describes what a
  user can actually use — send/receive plus server mode.
- **Clearing a CP/M file's R/O attribute no longer widens host permissions.**
  `Permissions::set_readonly(false)` sets *every* write bit on Unix, so the
  obvious spelling would have turned a private `0o600` file into a
  world-writable `0o666` one merely because a guest cleared `t1'`. Clearing now
  grants owner-write only and leaves the group/other bits untouched.
- **Two `show_error` strings wrapped on a 40-column PETSCII screen, and a third
  was far worse.** `show_error` emits one *unwrapped* line. The file-transfer
  menu's invalid-key hint had grown to eleven keys (43 columns with its indent)
  and the Kermit-server disk-space refusal was 42; both are now two lines. The
  third was found by the new guard rather than by eye: the SSH gateway's
  key-authentication failure was a backslash-continued string that reached the
  wire as 207 unbroken characters, wrapping even at 80 columns.
- **The PETSCII width guard could not see message drift.** The fit test
  hand-copied its list of messages, so it kept happily asserting about
  `"Press U, D, X, C, I, R, Q, or H."` — a string the code had stopped using —
  while the live 43-column replacement wrapped unnoticed.
  `test_show_error_literals_fit_petscii` reads the telnet module's own source
  with `include_str!` and checks every literal passed to `show_error` /
  `show_error_lines`, so a new or lengthened message is covered the moment it is
  written, with no second place to remember to update.
- **The SSH session-slot claim had three copies.** Password auth, relay `exec`
  and console register each wrote out the same `fetch_add` + rollback, and each
  owns its own release paths; a divergence between them is how the accounting
  would flip from over-counting (safe) to under-counting (fails open). Now one
  `try_claim_slot`, tested — including under thread contention — and the
  accepted M-11 over-count (a relay channel claims a slot on top of its
  connection's auth slot) is pinned by a test that states the tradeoff, so a
  later "fix" cannot silently flip it.
- **Every relay Kermit transfer permanently leaked a master session slot.**
  A relay channel claims a slot against `max_sessions`, and the relay task
  released it as its *last statement* — but the Kermit-server branch returns
  early, so it never got there, and nothing else releases a relay channel's slot.
  With the default cap of 50, fifty such transfers left the master refusing
  **every** new telnet, SSH and relay session until it was restarted. The slot is
  now released by an RAII `SlotGuard`, which a branch added later cannot bypass
  and which also survives a panic mid-session.
- **The web UI's "More…" buttons could sit outside their frame.** Two
  independent causes, both width- and browser-dependent, which is why it only
  showed up sometimes. The numeric inputs were sized purely by the HTML `size`
  attribute, which every browser maps to a different pixel width (and which also
  depends on which font in the stack resolved), so the File Transfer tunables row
  could overflow and carry its right-floated button out of the frame. Separately,
  the Server frame's grid put the button in a `1fr` column behind six
  `max-content` columns — grid doesn't shrink-to-fit like flexbox, so on a narrow
  frame the columns held their width, the grid overflowed, and the `1fr` column
  collapsed to zero. The numeric boxes now carry explicit widths, the button
  column is `minmax(max-content, 1fr)` so it can't collapse, and the frame grid's
  floor is raised to the Server row's measured intrinsic width. Verified in
  headless Chrome from 1600px down to 400px: all five buttons flush inside their
  frame, zero overflow.
- **The CP/M emulator config submenu's invalid-key hint omitted `D`.** The menu
  displays `D` and the code handles it, but the hint a user sees after a wrong
  keypress listed only "E, C, U, or Q" — found by cross-checking every telnet
  menu's accepted keys against the keys its hint names, the same drift class as
  the file-transfer hint fixed earlier today.
- **BDOS 34 / 40 (Write Random) were undocumented.** `web/cpmreference.html`
  listed Read Random but never its write counterpart, though both have been
  implemented for some time. The BDOS tables now cover every function the
  emulator services.
- **The web UI could not enable "serve Kermit to slave ports", and saving as a
  non-master cleared it.** Its checkbox renders `disabled` outside the Master
  role, but it was never added to the `updateRelayFields()` JS that re-enables
  the other Master-only field when the role dropdown changes — so switching to
  Master left the box greyed out, a disabled input submits nothing, and because
  the *submitted* role was now master the save no longer skipped the key and
  wrote `false` over a previously-enabled setting. The renderer, the JS and the
  save-skip list must all agree; a new test derives the set from the rendered
  HTML and checks the other two, so a fourth such field can't repeat it.
- **The Master/Slave screen's row-count guard had gone stale.** It still
  asserted the pre-`9f72b85` shape ("5 status + 3 item rows"), missing both the
  Kermit-to-slaves status row and its `K` item row — and its "5 status" was the
  slave shape, not the master one. The total coincidentally still came to 21, so
  nothing overflowed a 22-row PETSCII screen, but the guard could not have
  noticed. It now counts both role shapes, asserts the master remains the worst
  case, and records that headroom is exactly one row.
- **`K` (Kermit to slaves) is offered in every role but only `A` said "(master
  only)".** The non-master view now labels both, so neither master-only toggle
  is left unexplained.
- **Three separate test mutexes guarded the one global `Config`.** `kermit`,
  `relay`'s onward-dial tests and `relay`'s Kermit tests each had their own lock;
  the last two also repoint `transfer_dir`, the very key the first one changes.
  Individual writes are atomic, but the snapshot/restore *pairing* is not: two
  guards under different locks interleave, and one guard's `Drop` restores a
  value captured before the other test's change, repointing the key while that
  test is still running. All three now share `config::CONFIG_TEST_LOCK`.
- **A read-only CP/M file was writable when the gateway runs as root.**
  `delete`, `rename` and `make` checked the attribute explicitly, but
  `write_record` left it to the host — and root bypasses file permissions
  (`CAP_DAC_OVERRIDE`), which is exactly how this gateway runs under systemd.
  A guest could overwrite a file it was not allowed to erase or rename. All four
  mutating paths now enforce the attribute the same way.
- **The config test lock is now crate-wide** (`config::CONFIG_TEST_LOCK`). It
  lived in `kermit`'s test submodule, but the state it guards is global: the
  kermit tests repoint `transfer_dir`, so a test in *any* module that reads it —
  directly, or through a function calling `get_config()` internally — raced them.
  A per-module lock would have reintroduced the
  `test_server_g_dir_returns_listing` flake across module boundaries instead of
  fixing it.
- **`DIR` in the Gateway Shell resolved its operand twice.** Written as a match
  guard, which cannot bind what it resolves, so `DIR SUB` ran two
  `canonicalize` calls plus a case-insensitive component walk, then threw the
  result away and did it again. Behaviour is unchanged — the same new test
  passes against the old code — and `DIR` now has end-to-end coverage of all
  five operand paths, which it had none of.

- **A macOS-only CI failure: two `bindwatch` tests raced each other.** Caught by
  checking CI before tagging rather than trusting the local suite &mdash; exactly the
  case the release checklist warns about, since ubuntu and Windows passed on
  scheduling luck. `test_a_late_failure_supersedes_an_optimistic_bound` panicked
  `no entry found for key`: it and `test_registry_records_and_resets` both drive
  the one process-wide registry, and cargo runs tests on parallel threads, so one
  test's `reset()` landed between the other's `expect()` and its read. They are
  now serialised by a dedicated test mutex. Test isolation only &mdash; production
  calls `reset()` / `expect()` synchronously from the single startup path before
  any listener task is spawned, so the race cannot occur there.
- **The idle pacing missed a guest holding unread modem bytes.** Follow-up from a
  second review pass over the fix below: it cleared its idle counter whenever the
  receive ring was *non-empty*, rather than when bytes actually *arrived*. A guest
  can sit polling for a keypress while peer bytes go unread &mdash; a "press any key"
  prompt during an inbound burst &mdash; and that state would have reset the counter
  on every pass and span the host exactly as before the fix. The reset now keys on
  the ring depth increasing. Bytes being *consumed* needs no special case: the
  trap that reads them is not a status poll, so it already clears the count.
  Throughput re-measured unchanged (console output tens of thousands of char/s,
  modem burst ~1.4&nbsp;KB/s).
- **An idle CP/M session span the host CPU at over a core.** Caught reviewing the
  timer removal below, by measuring rather than reasoning: an idle EGT80 terminal
  sat at **161% CPU**. The two timers that fix had removed were, incidentally,
  the only thing pacing a guest's idle loop — a comms program waits for activity
  by polling a status call, and because every such call ends the CPU batch, each
  turn of that loop costs a full driver pass. Making the passes cheap left
  nothing to slow the loop down.
  - Fixed by pacing *only* the demonstrably idle case, so throughput is
    untouched: a pass that answers "nothing available" is counted, any pass doing
    real work resets the count to zero, and once a loop is established as idle it
    naps 1 ms, backing off to 8 ms after about half a second. Both tiers stay far
    below the threshold of noticing a keypress — measured worst case 8 ms.
  - Every way a guest can be told "nothing waiting" had to be recognised, which
    is what the first attempt got wrong: marking only the console-status calls
    fixed a port profile but left **HBIOS at 164%**, because a RomWBW program
    alternates console-status with HBIOS input-status and the unmarked call reset
    the counter every other pass. Now covered: BIOS CONST, BDOS 11, BDOS 6's
    status form, HBIOS input status, and the `^Z`-means-nothing reads of BDOS 3 /
    BIOS 7 that an `AUX:` profile polls.
  - Verified across all three access families with EGT80 driven in the emulator:
    idle CPU **2.2%** for a port profile, HBIOS and `AUX:` alike (from 161% and
    164% respectively), with `AT` → `OK` still answered in 6&ndash;9 ms. Console
    output stays at tens of thousands of char/s, the modem burst at ~1.5 KB/s,
    and a double-`ESC` still breaks out of a compute-bound loop in 4 ms.
  - The pacing rule is a pure function (`idle_nap`) so the thing worth pinning is
    testable: that a working program never naps, that an idle one does, that the
    second tier backs off further, and that neither tier could grow into a
    perceptible delay.
- **The CP/M emulator ran at roughly 150 baud, and none of it was the CPU.** Two
  tokio timers sat on the emulator's *per-character* path. The driver loop
  regains control at every BDOS/BIOS/HBIOS trap, so anything it does per pass is
  paid per emulated character — and both of these waited on a clock:
  - `cpmemu_oob_drain` (the double-`ESC` break-out probe) asked "is a byte
    waiting?" with `tokio::time::timeout(Duration::ZERO, …)`. A zero duration
    reads like "don't wait", but tokio rounds every deadline up to its next timer
    tick, so each call really cost **~1.1 ms** — measured at 1.118 ms against
    ~6 ns for a non-blocking probe. Replaced with a single poll of the future
    (`poll_once`), which has the same cancellation semantics: the timeout already
    dropped an unready read, and `session_read_byte` is explicitly resumable at
    this seam. Console output went from **840 to 47,000–81,000 char/s**.
  - the virtual modem's `poll_connection` waited `READ_POLL` (3 ms) on every
    pass, *including* while the guest was working through a burst that had
    already arrived — capping the display of received text at a few hundred
    char/s however fast the peer delivered it. It now probes without waiting when
    the guest still holds unread bytes (`guest_has_rx`). The wait is deliberately
    kept for the empty-ring case: that is what stops a guest polling in a tight
    trap loop from spinning on the socket and burning a core.
  - Measured end to end with EGT80 in terminal mode dialing the gateway's own
    menu, which is the case that felt like 150 baud: **111 bytes/s** before.
    A screen-painting program suffered most because it traps several times per
    character (`CONST` to poll the keyboard, `CONOUT` to print), and that is
    independent of the `cpm_emu_uart` profile — a port profile's `IN` polling is
    handled inside the CPU core and never reaches this loop.
  - For scale, the Z80 core was never the constraint: it steps at **33.7 MIPS**
    (~65× a 4 MHz Z80) and services **6.4 M CONOUT traps/s**.
  - Both fixes carry a regression test whose timing bound is keyed to the bug's
    cost (~1.1 s and ~60 ms for the respective call counts) rather than to
    jitter, so a timer creeping back onto either path fails loudly without the
    test being flaky.

### Documentation

- **New `web/cpmreference.html`** — a CP/M emulator reference so this material
  doesn't have to be rediscovered from the source each time: the `cpm_emu_*`
  config keys, the CCP built-in commands, every `cpm_emu_uart` profile with its
  port addresses, the bundled EGT80 terminal (menu, and the keystrokes that
  select each profile), which driver to pick for which machine, the RomWBW HBIOS
  calls we service and the two return-convention details that bit us, BDOS/BIOS
  coverage including what is deliberately absent, and the virtual modem's AT
  commands and dial targets. Linked from the user manual and added to the
  References grid on every sibling reference page.

## [0.8.0] - 2026-07-26

### Added
- **CP/M emulator — Z80 CPU + interactive console (in progress).** A
  new default-off config key `cpm_emu_enabled` (wired into the telnet, web, and
  GUI config UIs) gates a `K  CP/M System` main-menu item. Selecting it boots a
  real Z80 CPU (the BSD-licensed [`iz80`](https://crates.io/crates/iz80) crate)
  in a 64 KB machine driven by our own CP/M 2.2 BDOS, and drops into a Rust
  CCP-lite `A>` prompt. The full console BDOS group (character/string output,
  console input with echo, direct console I/O, read-console-buffer, console
  status, version) is wired to the telnet/SSH session, so interactive Z80
  programs can read and write the console; built-in `HELLO` and `ECHO` demos
  exercise it. Works correctly on PETSCII (C64) terminals as well as ANSI/ASCII.
  On launch it creates the drive folders `CPM/A`..`CPM/H` under `transfer_dir`.
  An interactive program can be aborted with a double-`ESC`, and a runaway is
  bounded by an instruction budget. Completely separate from the Gateway Shell,
  which emulates no CPU; the item is hidden and the key rejected while the
  toggle is off.
  - **Filesystem, part 1 — FCB + drives + sequential file I/O.** The emulator
    now has a directory-backed CP/M filesystem: each drive A:–H: is a folder in
    the `CPM/` container, and the BDOS file calls for opening, creating,
    closing, and sequentially reading/writing files (via the DMA buffer and a
    parsed 36-byte FCB) are implemented and jailed under `transfer_dir`. Drive
    select / current-disk / set-DMA are wired, 8.3 filenames are enforced
    (case-insensitive; host files that aren't valid 8.3 are invisible to CP/M),
    and the `A>` prompt gained `A:`..`H:` drive-change commands with a
    drive-aware prompt.
  - **Filesystem, part 2 — directory search + erase.** The BDOS
    search-first/search-next calls enumerate a drive's files (with `?`
    wildcards, synthesizing CP/M directory entries per 16 KB extent) and the
    delete call removes matching files. The CCP-lite gained the authentic
    built-ins `DIR` (list the current drive) and `ERA name` (erase, with a
    `*.*` confirmation), so files uploaded into a `CPM/` drive can be listed
    and removed interactively.
  - **Filesystem, part 3 — random-record I/O, file size, rename.** The BDOS
    random read/write calls seek to a record number (with the sequential
    position kept in sync), compute-file-size reports a file's length in
    records, and rename moves a file to a new 8.3 name (no clobber). This
    completes the CP/M 2.2 file BDOS surface, so real utilities like `PIP`
    and `STAT` become runnable once `.COM` loading lands.
  - **Run a real `.COM` from a drive.** A command at the `A>` prompt that
    isn't a built-in is now resolved as `<verb>.COM` on the drive (honoring a
    `B:` drive prefix), loaded into the TPA, and executed — so actual CP/M
    software (PIP, STAT, ASM, …) uploaded into a `CPM/` drive runs over
    telnet/SSH. Page zero is set up exactly as the CCP does before launch: the
    command tail is placed at 0x0080 and the first two arguments are parsed
    into the default FCBs at 0x005C / 0x006C. The program image is loaded
    jailed under `transfer_dir` (canonical-prefix + symlink checks) and bounded
    by the per-file size cap.
  - **Full CP/M resident command set.** The `A>` prompt now implements all
    six authentic CP/M 2.2 resident commands — `DIR`, `ERA`, `REN`, `TYPE`,
    `SAVE`, `USER` (plus the `d:` drive change) — so no upload is needed for
    everyday file work. `REN` renames (no clobber); `TYPE` streams a text file
    and stops at the `^Z` end-of-file marker (binary files refused); `USER`
    selects an area (only area 0 exists — one flat area per drive). To make
    `SAVE` authentic, the emulated machine now stays resident across commands:
    the transient program area survives a warm boot back to `A>` (as on real
    CP/M), so `SAVE n file` dumps the image a previous program (e.g. `DDT`)
    left in memory. Low-memory vectors are reinstalled on each program load so
    a program that trashes page zero can't corrupt the next one.
  - **Virtual-modem UART port selection.** A new config key `cpm_emu_uart`
    (wired into the telnet, web, and GUI config UIs, each showing a description
    beside every choice) selects which machine/port address the emulated CP/M's
    modem answers at — `off` (default), the RC2014/RomWBW Z80 SIO/2 channels
    (`rc2014_1a`…`rc2014_2b`, 0x80–0x87), or the Altair 88‑2SIO / 88‑SIO
    (`altair_2sio1`/`altair_2sio2`/`altair_sio`). Addresses and status-bit
    conventions are taken from the RomWBW SIO driver and David Hansel's Altair
    simulator. With a profile selected, `CpmMachine`'s port I/O answers at those
    addresses with a valid idle UART (transmit ready, nothing received) so
    comms software can probe and initialise the port.
  - **Virtual modem — outbound dialling.** The CP/M modem now speaks Hayes
    `AT` and can place calls: `ATD A` / `ATD B` dial the gateway's own serial
    Port A / Port B (via the existing peer-dial plumbing, like one machine
    calling another), and `ATDT host:port` opens a TCP connection. On answer it
    reports `CONNECT` and becomes a transparent data pipe; `+++` returns to
    command mode and `ATH` hangs up. A new `aux` profile choice puts the modem
    on the CP/M BDOS `AUX:` device (functions 3/4) — the hardware-independent
    path for SC126/RomWBW software (a Z180 ASCI *port* profile can't work: the
    Z80 core doesn't implement the Z180 `IN0`/`OUT0` instructions the ASCI
    uses). The modem is a self-contained async layer bridged to the guest's
    synchronous UART/AUX byte rings at the CPU batch seam.
- **EGT80 — "Ethernet Gateway Terminal", a CP/M terminal of our own.** Every
  period CP/M terminal is built for one machine's serial port — commonly a
  separate binary per port, or a build for one specific card, and some generic
  builds carry no serial driver at all — and pairing one with the wrong port
  produces silence rather than an error. `EGT80.COM` (new `EGT80/` directory,
  Z80 assembly, CP/M 2.2 and CP/M 3) asks instead: a menu picks the port at run
  time, from **five families** — Z80 SIO/2, 6850 ACIA, RomWBW HBIOS (`RST 8`),
  Z180 ASCI and the CP/M `AUX:` device — with a free-form address entry for
  boards at unusual ports. It carries a console layer chosen once at startup
  (BIOS vectors on 2.2, BDOS 6 on 3), menus and help written for someone who
  does not yet know which port their machine uses, terminal mode with an
  escape-key menu, an ANSI/ASCII inbound filter (pass escape sequences through,
  or strip them and the high bit for a printing terminal), **settings saved into
  its own `.COM`** (a 128-byte-aligned patch area rewritten with a
  random-record write, signature- and range-checked at startup and falling back
  to defaults if damaged), and **XMODEM transfer both ways** — 128-byte blocks,
  CRC-16 with the checksum fallback an older peer may insist on, one buffer
  serving as both protocol block and CP/M record, abortable with a keypress, and
  refusing to leave a partial file behind when a download fails.

  **Shipped ready to use:** the `.COM` is compiled into the gateway binary and
  placed on CP/M drive A: when the emulator first creates its drive folders, so
  it is simply there. That copy is never overwritten afterwards, because the
  settings live inside it; deleting it restores the shipped copy. Release
  archives also carry the loose `EGT80.COM` for sending to real hardware.

  The port screen names the port in force and offers **`D` — the default port**
  (the one the gateway also defaults to, so the pair works together again in one
  keystroke), and the menu says `(changed — press V to keep it for next time)`
  while a setting is live but not yet written to the file. **`^C`** leaves the
  wrong-port notice — which swallows everything else you type, so a half-typed
  line can no longer run menu commands by accident — aborts a transfer in
  progress, and cancels at the filename prompt.

  Built by running the real period assembler (SLR `Z80ASM`) under `zxcc`, so
  SLR80 compatibility is structural rather than hoped for, with M80+L80 and ZMAC
  as portability gates. Wrong choices are diagnosed rather than left to hang: a
  port that never accepts a byte is named and explained, HBIOS is refused on a
  machine with no `RST 8` vector, and the Z180 ASCI family is refused on a
  processor that is not a Z180 (`MLT` tells them apart). Two paths cannot be
  exercised by any test here and are documented as reasoned-not-run: the Z180
  ASCI driver (our Z80 core has no `IN0`/`OUT0`) and the CP/M 3 console path.
  - **Virtual modem — RomWBW HBIOS access (`hbios_1` / `hbios_2`).** Some CP/M
    comms software doesn't drive a UART at all: software built for RomWBW asks
    the firmware to move the byte, issuing an `RST 8` with a function number in
    `B` and a serial unit in `C`. On a port profile such a program hangs before
    printing anything — its first call goes nowhere — which no port address can
    fix; RomWBW-targeted builds of period comms software are exactly this
    case. Two new `cpm_emu_uart` choices answer that
    API for one serial unit (the virtual modem), so those builds now run: `AT` →
    `OK`, `ATDT host:port` → `CONNECT`, with data flowing both ways. The `RST 8`
    vector is installed *only* for these profiles, so every other setting keeps
    the untouched page zero of a plain CP/M 2.2 machine; a call for a unit other
    than the selected one is refused, so a mismatch fails the same recognisable
    way a wrong port address does. Implemented: the serial (character device)
    group — in, out, input/output status, initialise, query, describe — plus the
    version and serial-unit-count calls. Refused with an error result: bank
    switching / memory management, disk, RTC, video, sound, DSKY (RomWBW
    *hardware* services with no counterpart here) — an honest failure at the
    call beats stranding a program later. A blocking call whose device isn't
    ready parks the guest on the trap and is re-reported after the driver's
    seam work, so it stays interruptible by the double-`ESC` break-out and
    doesn't spin the host. Written from the published HBIOS interface
    description (function numbers and register conventions); no RomWBW code is
    included. New `src/cpm/hbios.rs` with 11 unit tests, plus a fidelity fix the
    guests depend on: an HBIOS return now sets the flags from the result byte
    (as `OR A` leaves them) instead of leaving the guest's stale flags. A guest
    that hands the status straight to a `JR Z` was reading our stale flags as
    "transmitter not ready" and waiting forever.
  - **Virtual modem — dialable as `CPM@<ip>` (inbound).** The CP/M emulator is
    now a third dialable peer endpoint named `CPM`, alongside Ports A/B: from
    another modem on the gateway, `ATD CPM@<ip>` rings it exactly as
    `A@<ip>`/`B@<ip>` ring the serial ports. The running CP/M comms program
    sees `RING` and answers with `ATA` (or auto-answers after `ATS0=`*n*
    rings), then the machines are joined transparently. Implemented additively
    with a parallel global call slot and a `CPM@host` dial parser — the A/B
    peer-dial slots and routing are untouched (208 serial tests still pass).
    Gated by `allow_peer_dial`; the endpoint answers while a comms program is
    running (that's when the ring is polled).
  - **Virtual modem — reachable over the master/slave relay.** A device on a
    slave gateway can dial `CPM@<master-ip>`: the slave relays the address to
    the master, whose relay peer-dial handler resolves it to its own local
    CP/M endpoint (the CP/M analog of resolving `A@`/`B@` to a local port), so
    CP/M running on the master is reachable from every attached machine. Both
    directions were verified end-to-end over a live gateway: a `.COM` dialing
    a TCP host via `ATDT` (CONNECT + data round-trip), and an external modem
    dialing `CPM@<ip>` (the CP/M program rang, auto-answered, and received the
    caller's data).
  - **Virtual modem — slave-hosted CP/M reachable via the crossbar.** CP/M
    running on a *slave* is now dialable as `CPM@<slave-ip>` from the master
    (or another slave) exactly as its A/B ports are: while a modem CP/M shell
    is active the slave registers the label `CPM` with the master
    (`serial-register CPM`) and, on a peer-dial claim, rings its own local
    endpoint and bridges. `parse_remote_peer_addr` accepts the `CPM` label; a
    master/standalone `ATD CPM@<slave-ip>` claims it through the crossbar; and
    an async slave-side announcer (the CP/M analog of the physical-port
    `modem_slave_announce_tick`, tied to the shell's lifetime) does the
    registration. So `CPM@<ip>` now works wherever `A@<ip>`/`B@<ip>` do.
    Additive — A/B and the existing relay are unchanged.
  - **Out-of-band break-out; remote outbound dialing.** A double-`ESC` now
    returns to `A>` at any time — not just at a console prompt but also from a
    compute-bound program that never reads the console (the gateway watches the
    wire out-of-band between CPU bursts), so a runaway no longer has to run out
    the instruction ceiling. The CP/M-System banner shows "Press ESC twice to
    stop a program." The CP/M modem can also dial a serial port on *another*
    gateway — `ATD A@<remote-ip>` / `B@<remote-ip>` routes via the master/slave
    relay (same routing and `allow_peer_dial` gate as the physical modem);
    previously only the gateway's own ports were reachable. The CP/M modem's
    peer-dial is now gated by `allow_peer_dial` like the physical modem.
  - **Virtual modem — fidelity polish.** Five additive enhancements to the CP/M
    modem, none of which change the working polled path: (A) the AT command
    layer parses a chained init string (`ATE0Q0V1X4S0=1`) and applies each
    clause — echo, quiet, verbose/numeric and `X`-level result codes,
    S-register set/query (`S0` auto-answer, `S7` peer-dial carrier wait), `&C`/`&D`,
    `ATZ`/`AT&F` reset, `ATI` — instead of matching a few fixed strings; (B)
    carrier is surfaced to the guest as the UART's DCD bit (SIO RR0 bit3, 6850
    bit2), active-high so the idle status byte is unchanged; (C) flow control
    both ways — the UART reports transmit-not-ready when the TX ring is full and
    the peer read is capped to the guest RX ring's free space, so a speed
    mismatch back-pressures instead of dropping bytes; (D) a fuller Z80 SIO
    register model (WR0 read-pointer + `RR1`/`RR2`), a strict superset of the
    RR0-only behaviour; (E) `CPM@<ip>` is now an answer *pool* — every
    modem-enabled CP/M session can answer the next inbound call (a hunt group),
    with one session owning the slave→master crossbar announcement.
  - **ADM-3A terminal translation.** The emulator presents CP/M programs with
    a Lear Siegler ADM-3A terminal and translates its screen-control stream to
    the connected client: ANSI cursor sequences for a modern terminal, native
    cursor codes for a Commodore 64 (PETSCII), best-effort for a dumb ASCII
    TTY. Client arrow keys are translated the other way into the ADM-3A cursor
    codes the program reads. This lets full-screen software (WordStar, Turbo
    Pascal, editors) installed for an ADM-3A render correctly. The decoder and
    per-terminal renderers are a self-contained, unit-tested module.
  - **Configurable runaway ceiling.** A new config key `cpm_emu_max_minstr`
    (millions of Z80 instructions per program run, default 2000 = 2 billion;
    wired into the telnet, web, and GUI config UIs) bounds a compute-bound
    `.COM` that never reads the console, so the `A>` prompt always returns.
    Interactive programs remain escapable with a double-`ESC` at any input
    prompt. In the GUI and web UIs the CP/M controls (enable + ceiling) moved
    into the "AI, Browser & Weather — More" panel to keep the main screen
    uncluttered; in the telnet UI they live in a CP/M submenu under Other
    Settings → `E`.
  - **Free-TPA report.** The boot banner and `VER` now print the size of the
    transient program area (`63K TPA free (0100-FDFF)`), the way a real CP/M
    system reports its memory on cold start, so a user can see how much room a
    `.COM` actually gets. Derived from the emulator's own TPA constants.
- **Gateway Shell: three new commands.** `CLS` / `CLEAR` clears the screen;
  `VER` / `VERSION` prints the shell identity and gateway version; and
  `FIND <pattern>` / `WHERE` recursively searches all of drive A: (not just the
  current directory) for files whose name matches a wildcard, printing each
  hit's A: path. The `FIND` walk is bounded (scan and result caps) and never
  follows symlinks, so it stays inside the transfer-directory jail.
- **First-run setup wizard (desktop GUI).** A fresh install now opens the GUI
  window into a nine-screen wizard instead of a configuration editor the
  operator has to reverse-engineer: credentials (typed twice, with a
  show-password toggle and the cleartext-storage warning), telnet, SSH, access
  control, the web server, the transfer directory, the CP/M emulator and its
  virtual modem, and the gateway role — with a master-connection screen on the
  slave path. It closes with a review listing what will be saved, the actual
  commands to connect with, and **the inbound TCP ports to allow on the
  firewall**. Beyond password-match it validates port syntax and refuses a port
  another listener already claims (including the standalone Kermit server it
  never asks about), and warns about sub-1024 ports and about an unauthenticated
  gateway with IP safety off. Choosing the Master role arms
  `master_accept_relays` but never silently enables the SSH server the relay
  needs: the role screen explains why a master wants it and offers a button, and
  the review screen warns if it is still off. The wizard edits a draft, so
  nothing reaches the config or the running server until its final *Save and
  Restart Server*; exiting or skipping writes exactly one key. Re-runnable from
  *Server — More… → Run setup wizard…*. It is deliberately GUI-only — telnet and
  the web UI already expose every key it touches. New key
  `setup_wizard_completed`, asymmetric on purpose: `false` for a config the
  gateway creates itself (a fresh install sees the wizard), but a config file
  that *lacks* the key reads as `true`, so an upgrade is never dropped into it.

- **A loud warning when the listeners don't come up.** Each listener already
  logged its own bind failure, but one line in the startup chatter is easy to
  miss and the process kept running afterwards, quietly serving nothing — the
  failure mode being a second copy of the gateway started without stopping the
  first, where the old process holds the ports and everything you connect to is
  still served by the old binary. It looks exactly like "my settings changed
  nothing". `src/bindwatch.rs` now collects each listener's outcome and, once
  they have all reported, says so: **NONE of the N configured listener(s) could
  bind**, which ports, that the process is serving nothing, that another copy is
  almost certainly holding them, and the commands to find and stop it
  (`pgrep`/`ss`/`pkill`, or `netstat`/Task Manager on Windows). A partial
  failure gets a shorter note; a total failure that is *not* address-in-use
  (e.g. ports below 1024 without root) warns without blaming a second copy; a
  serial-only setup with no listeners stays quiet.
- **The SSH server binds its own socket** (`run_on_socket` rather than
  `run_on_address`, which is literally bind-then-run_on_socket). A bind failure
  is now reported as one: the old form logged "SSH server listening on port
  2222" *first* and then surfaced "Address already in use" as a generic
  post-hoc "SSH server error", which read as though the port had come up.

- **A slave's modem ports now register with the master at startup, so the
  master can ring them.** The registration machinery existed but was gated
  behind `allow_peer_dial`, which meant a slave with the default settings could
  never be reached *from its own master* — the master's Serial Gateway menu
  simply never listed the port, and nothing contacted the master until the
  attached device happened to dial out. That gate was the wrong one:
  `allow_peer_dial` governs this gateway *dialing* arbitrary peers, whereas
  master/slave is already an explicit, mutual, authenticated pairing (the slave
  holds the master's credentials; the master sets `master_accept_relays`). A
  third party reaching the port through the master's crossbar is still gated, on
  the master. The announcer's log lines now describe registration rather than
  peer-dial.
- **The slave's CP/M endpoint is registered for the server's lifetime**, the way
  Ports A and B are — not only while someone happens to have the emulator open.
  It used to announce per CP/M session, so `CPM@<slave-ip>` blinked in and out of
  the master's Serial Gateway list and the master could not see that the endpoint
  existed at all. A call arriving with no session running rings the answer pool,
  finds it empty and is reported unanswered — exactly like dialing a modem port
  whose device is switched off. Declines to register when the emulator is off or
  its virtual modem is `off`, since nothing could ever answer.
- **The slave-link summary stopped giving a stale reason.** It printed
  `CPM  emulator  not announced (needs allow_peer_dial)` — true when the
  announcer was gated on that flag, and after the fix below merely misleading: it
  sent an operator hunting for a setting to change. It now names the real reason.
- **The router-block fallback is per address family.** The `x.x.x.1` fallback
  keyed off "no router detected at all", so a host where only an *IPv6* default
  route was found would stop applying it to IPv4 — quietly weaker than the
  operator asked for, since the IPv4 router was still unknown. It now keys off
  whether *that family's* router is known.
- **Two config keys were in no documentation table:** `allow_peer_dial` (a
  security-relevant one) and `kermit_wait_for_receiver`, though Appendix A claims
  to list every key the parser recognises. Both are documented now, along with
  the auto-managed `gui_window_geometry`, and `allow_peer_dial`'s description
  states what it does *not* gate (registration).
- **The slave's CP/M endpoint registers with the master too.** Same wrong gate
  as the serial ports: `cpm_slave_announce` and its spawn site both required
  `allow_peer_dial`, so on a default slave the `CPM` endpoint never announced and
  `CPM@<slave-ip>` was unreachable from the master. Registration is now
  automatic whenever the role is `slave` and a master host is set (the CP/M
  guest *dialing out* to a peer port stays gated, in `cpm_modem.rs`), and it
  re-announces on a master-settings change like the ports do.
- **A config change re-registers both slave ports.** A registration is a
  standing claim — the master holds an idle channel and rings it later — so one
  made under settings that have since changed is worse than none. Both the
  modem announcer and the console register loop now watch a fingerprint of what
  a registration depends on (role, the master's host/port/username/password, and
  that port's own enabled flag, mode and device path) and re-register within
  about a second of any of it changing, whichever UI made the change. Unrelated
  edits — a weather location, a timeout — deliberately don't disturb a live
  registration, and a call in progress is never interrupted (the check only runs
  while idle).

### Changed
- **Gateway Shell now surfaces the CP/M "destination first" operand order.**
  `COPY` and `MOVE` take the destination *before* the source (`COPY dst src`) —
  the reverse of the order most users expect today. The shell now prints two
  reminder examples on entry, and a failing `COPY`/`MOVE` (e.g. "File not
  found." after the operands were swapped) echoes the correct form
  (`e.g. COPY dst src (dest first)`) so the mistake is self-correcting.
- **CP/M settings are named where they live, and the way out is spelled out.**
  The GUI frame + its "More" popup and the web card + its modal that hold the
  CP/M enable toggle, runaway ceiling, and virtual-modem port are now titled
  "AI Chat, Browser, Weather & CP/M" (and "… & CP/M — More"), so the CP/M
  settings are discoverable rather than hidden behind an AI/Browser/Weather
  label. The emulator's entry banner now shows a prominent "Type EXIT to return
  to the gateway." line beside the "Press ESC twice to stop a program." hint.
- **The CP/M virtual modem is documented as polled-only.** The emulated UART is
  polled (the guest reads the status register for RX/TX readiness); the core
  never raises a serial interrupt in any Z80 interrupt mode. This holds for
  every port profile — the family (Z80 SIO / 6850 ACIA / 8080 88-SIO) only
  selects the I/O port address and status-bit layout, not interrupt support —
  so polled comms software works on any profile while interrupt-driven serial
  software is unsupported. Noted in the manual and the `uart` module.
- **The CP/M emulator and its virtual modem are on by default.**
  `cpm_emu_enabled` was default-off while the emulator was being built out; it is
  on now because the feature is bounded (guest jailed to `transfer_dir/CPM`,
  runaway stopped by `cpm_emu_max_minstr`, always escapable with a double-`ESC`,
  no path to a host command) and it ships with a terminal of its own. The
  virtual modem now defaults to `rc2014_1b` — the port EGT80 expects — so the
  emulator and its terminal work together untouched. Both remain settable:
  `cpm_emu_enabled = false` for no guest code at all, `cpm_emu_uart = off` for an
  emulator with no modem (guest code can dial out when a port is selected).
- **A one-click “Default port” for the CP/M virtual modem** in all three UIs
  (telnet `D` in the CP/M panel, a button in the web CP/M section, a button
  beside the GUI combo): resets `cpm_emu_uart` to the default whatever it was
  showing — the answer to “I changed something and now the terminal cannot
  connect”. Guarded by a test that fails if EGT80's own default port and the
  gateway's default profile ever drift apart, since that pairing is the whole
  point and neither build would otherwise notice.
- **“Block connections from the router” (`disable_gateway_connections`)** in all
  three UIs, on the row under the login checkmarks. Off by default, which
  **changes behaviour**: a connection from the local router is now allowed while
  the IP allowlist stays in force. It used to be refused outright, which left
  `disable_ip_safety` (dropping the allowlist entirely) as an operator's only way
  in; the narrow rule is now the opt-in. Loopback is exempt either way, public
  addresses are still refused, and the toggle applies on the next connection with
  no restart.
  - **The router's address is queried from the OS, not guessed.** Earlier work
    assumed the router was `x.x.x.1` — a convention, not a fact, which both
    missed a router living on `.254` and refused an ordinary machine that
    happened to sit on `.1`. `src/router.rs` reads the default route's next hop
    (Linux: `/proc/net/route` + `/proc/net/ipv6_route` directly, no subprocess;
    macOS/BSD: `route -n get default`; Windows: `route print`, matched on the
    addresses rather than the localised column headings) and the rule blocks
    exactly that, IPv4 and IPv6, including both routers on a multi-homed host.
    The query runs on a background thread at startup and re-runs when the cached
    answer ages past five minutes, so a DHCP change is picked up without a
    restart; it is **never** run on the connection path, which only reads the
    cache. The detected address is logged at startup and named in the checkbox
    label in every UI — "Block connections from the router (192.168.1.1)". Where
    it cannot be determined the rule falls back to the `x.x.x.1` convention (per
    address family), so it is never silently weaker; loopback stays exempt, and
    no detected address can widen the allowlist. Every parser is a pure function
    tested from captured output on all three platforms, including a localised
    (German) Windows route table.
- **A slave's log now tells the whole story.** Each attempt to reach the master
  is logged with its number (`announcing to master 10.1.2.3:2223 as 'relay'
  (attempt 5)`), so a slave that cannot connect no longer looks identical to one
  sitting idle — previously the failure reason was deduped (rightly, it repeats
  forever) and nothing showed the retries still happening. On success there is a
  plain `CONNECTED to master …` line followed by one consolidated block naming
  every port, the mode it is in, and what the link is doing:

  ```
  Slave link to master 10.1.2.3:2223 —
      Port A  mode=modem   registered — awaiting a pick from the master
      Port B  mode=kermit  bridging — a master user is attached
      CPM       emulator  announced — dialable as CPM@this-host
  ```

  Disabled ports are listed as disabled rather than omitted, because "why is
  port B missing?" is exactly what a summary should answer. The CP/M announcer
  also gained what the serial loops already had: its failure reason is now
  logged (it used to be discarded, leaving identical lines with nothing to act
  on) and its retry backs off 1s→30s instead of hammering a dead master once a
  second.
- **EGT80 v0.7 &mdash; the screen is cleared, and everything configurable is under
  Settings.** The screen is cleared and the "Ethernet Gateway Terminal" banner
  redrawn before the main menu and on entering terminal mode, so it stays clear
  which program you are talking to once a remote system has filled the screen;
  the terminal-mode banner now also names the menu key *and* says that the key
  followed by `E` returns to the main menu. CP/M has no standard clear-screen, so
  a new Settings item picks the dialect: ANSI `ESC [ 2 J` (the default &mdash;
  what a terminal emulator over USB serial and the gateway's own ANSI clients
  understand), ADM-3A `^Z` (a period terminal, or a PETSCII C64 through the
  gateway, whose translation re-renders it), or off for a printing terminal. `^Z`
  was the first default, on the reasoning that its failure mode is silent rather
  than `[2J` printed as litter; it moved to ANSI because silent-failure is what a
  modern terminal on real CP/M hardware actually got. Because a clear can wipe a message before it is read, the
  places where a message is the only feedback — a damaged settings block, a save,
  a refused port family — now pause for a keypress.

  The **serial-port selector moved off the main menu into Settings**, which now
  shows the port, the menu key and the clear dialect alongside the filter mode.
  The **menu key is any control key you press** rather than a cycle through three
  fixed choices, because which key is free depends on the remote system (`^Y` is
  WordStar's delete-line, `^]` telnet's escape, `^\` Kermit's). Five keys are
  refused with the reason: `^C` backs out of every screen, CR/LF/TAB are ordinary
  typing, and `ESC` begins the arrow-key sequences, so a cursor key would open the
  menu. A saved key is validated at startup too, since an invalid one would trap
  that key for ever.
  - **A wrong HBIOS unit froze EGT80 instead of reporting a dead port.**
    Reported from an SC126: with the wrong port selected, the "nothing was sent"
    notice appeared as designed — and then `^C` did nothing and the machine had
    to be reset. The published API is explicit that `CIOIST`/`CIOOST` return a
    count in `A` and that "negative values (bit 7 set) indicate a standard HBIOS
    result (error) code". Under EGT80's vector contract, non-zero means ready, so
    an *error* read as "a character is waiting"; the driver then called `CIOIN`,
    which the API says "will wait indefinitely". The terminal loop never got back
    to the keyboard, so the menu key and `^C` were both dead. Those two status
    calls now treat a negative result as not-ready, which turns the hang into the
    diagnosis EGT80 already had: the transmitter never comes free, `PSEND` gives
    up, and the wrong-port notice appears with `^C` live. The emulator's own
    `CIOIST`/`CIOOST` now cap their counts at `0x7F` for the same reason — a
    count with bit 7 set would read as a failure on a healthy port.
  - **A refused AT command is logged verbatim, and debris before `AT` is
    skipped.** `ATDT ethernetgateway` intermittently answered `ERROR` from an
    SC126 in two different terminal programs, working when retyped. The new log
    line caught it on the first occurrence: `refused command "CCatdt
    ethernetgateway" [43 43 61 74 …]` — two XMODEM `C` handshake bytes, left in
    flight by a download that stopped early, arriving as the first characters of
    the next command line. Nothing legitimate precedes `AT` on a command line, so
    the command is now found rather than refused, with the skipped bytes logged.
    A real modem would answer `ERROR`; being stricter than the hardware here buys
    nothing and costs the user an unexplainable intermittent fault.
  - **Transfers were silently corrupted on the HBIOS and `AUX:` drivers.**
    Reported from an SC126: a downloaded file came back the correct length and
    **entirely zero**, with no error at either end. The cause is register
    discipline. The console entry points (`CST`/`CIN`) have always saved `BC`,
    `DE` and `HL` around the driver; the *port* entry points (`PST`/`PIN`/
    `POST`/`POUT`) were bare jumps that saved nothing. Three drivers touch only
    `A` and `BC`, so callers got away with holding a pointer in `HL` across a
    port call — but **HBIOS is an `RST 8` into RomWBW's firmware and `AUX:` is a
    BDOS call**, and neither promises to preserve anything beyond its documented
    returns (real RomWBW returns values in `HL` for several functions).
    Both XMODEM inner loops walk the buffer with `HL` across exactly those
    calls, and the failure is quiet and total: on the send side the byte
    transmitted *and* the byte folded into the CRC both come from the wandering
    pointer, so they agree, the receiver's check passes, and a file of exactly
    the right length arrives full of whatever `HL` had wandered onto. On the
    receive side the bytes are stored through the wandering pointer while the CRC
    is computed on the byte in `A`, so again every block "passes". The four port
    entry points now save and restore `BC`/`DE`/`HL` exactly as the console ones
    do.
  - **The emulator's HBIOS no longer preserves `HL` either.** This bug could not
    be reproduced here because our `RST 8` only set the registers the API
    documents, so `HL` survived — an emulator looser than the hardware turns a
    reproducible bug into a field report. The character-I/O and management calls
    now scramble `HL` (`CIOQUERY` and `CIODEVICE` are exempt: they return `L`).
    Verified afterwards with an independent XMODEM peer over TCP: a download and
    an upload on the port drivers are both byte-identical.
  - **The post-transfer notice no longer costs a keystroke, and the hangup
    always finishes.** Two faults reported from an SC126, both introduced by
    earlier fixes in this same area. The "press a key to return to the session"
    prompt collided with the far end's own prompt whenever that survived the
    line settle, so one event cost two keystrokes and the first appeared to do
    nothing but move the cursor down a line. It is now a statement rather than a
    prompt — `Back in the session - press a key if the far end is waiting for
    one.` — and the far end owns the keystroke it asked for. And the hangup's
    settle waited for silence that a talkative peer need never provide, so
    `Hanging up...` could be the last thing the program printed and the machine
    had to be restarted; it is now bounded to three drains and abandonable with
    `^C`, because an exit path has to terminate whatever the other end does.
  - **EGT80 hangs up when it exits.** Leaving the program used to leave the call
    up: the gateway held the session open, and on a real phone line it would hold
    the line. There is no DTR to drop — the SIO and ACIA drivers do not touch the
    modem-control bits, HBIOS does not expose them, and CP/M `AUX:` has no notion
    of them — so the modem is asked, with the Hayes escape. That means honouring
    the guard time, and the guard is a *duration*, not an instruction count: the
    settle runs four timeout passes rather than one, because a poll loop that
    takes a second on a 4 MHz Z80 takes a fifth of that on an 18 MHz Z180, and an
    escape sent too early is not an escape — it is three plus signs typed at the
    far end. If there is no modem at all (a null-modem cable to another machine)
    the far end receives the characters as text; that is the price of not being
    able to tell, and it is smaller than walking away from a live call.
  - **The virtual modem no longer forwards the `+++` escape to the peer.** A real
    modem swallows those characters and only sends them on if the sequence turns
    out not to be an escape — and this gateway's *physical* modem already did
    exactly that. The CP/M one forwarded them as they arrived, so every peer saw
    `+++` whenever a guest hung up, which a guest that hangs up on exit makes
    every session. Held characters are flushed in order if the run breaks, so
    nothing is lost; a `+` inside a data stream is still ordinary data, protected
    by the same guard as before.
  - **After an in-session transfer, EGT80 says a key is needed.** Settling the
    line throws away whatever the far end said as the transfer ended — that burst
    is where a truncated escape sequence comes from, and a BBS's "press any key"
    lives in the same burst. The result was a screen showing `Received.` and then
    silence, with nothing to explain what to do, which reads as a hang. Both
    directions now end with `Press a key to return to the session.` and wait for
    one key. The key is *not* forwarded: the far end may not be waiting for
    anything, and a stray byte pushed into someone's menu is worse than pressing
    a key twice.
  - **The line is settled after a transfer, so a lost `ESC` cannot print as
    litter.** Reported from an SC126: after a download the screen showed `2J`
    and did not clear — on a terminal that is definitely ANSI. The cause is the
    boundary, not the terminal. The far end starts talking the moment the last
    block is acknowledged (a BBS prints its own prompt, then redraws its menu)
    while this end is still closing the file — a BDOS close is disk I/O — and a
    polled UART holds exactly one byte, so those first bytes are overwritten and
    lost. `ESC [ 2 J` arriving without its first two bytes *is* the text `2J`.
    EGT80 already had a line-purge routine, used before a receive and after a bad
    block but never at the end of a transfer; it now settles the line on every
    transfer exit, in both directions and from both the menu and terminal mode,
    and clears any half-seen escape sequence out of the filter so the session
    resumes on a boundary the far end will send whole.
  - **A transfer inside a session no longer asks for a key.** Reported from real
    use: after a download EGT80 said "Received.", waited for a key, and then
    *something asked again*. The second prompt was the far end's — the gateway's
    own File Transfer menu ends with "Press any key to continue." — so two
    programs each wanted a keystroke, back to back, which reads as a bug however
    it is explained. Every EGT80 transfer exit path was checked: none of them
    waits internally, the wait was in the caller. A transfer started from inside
    terminal mode now prints its result and returns straight to the session,
    which is what period terminals do and what a BBS expects: the BBS's own
    prompt and menu redraw follow naturally, and nothing is lost because
    returning to a session clears nothing. A transfer started from EGT80's *own*
    menu still waits for one key, because the menu redraw clears the screen and
    the result would otherwise vanish.
  - **Coloured menus, and a terminal menu that explains itself.** Headings,
    labels, values and the key letter of every menu line are now coloured.
    Colour follows the ANSI/ASCII setting already in Settings — that setting
    means exactly what colour depends on — so ASCII mode emits not one escape
    byte and a printing terminal or PETSCII console sees the plain text it always
    did. The key letters are coloured by a printer that recognises the shape
    every menu line already has (two spaces, key, two spaces, text), so the menus
    remain single blocks of text in the source and continuation lines are left
    alone; the escape sequences live in their own strings so the screen-fit test
    still measures printable width. The terminal-mode menu no longer says
    `(twice=send)`, which told the user nothing: it now reads
    `Menu: E)xit  H)elp  S)ettings  U)pload  D)ownload` /
    `^Y again sends ^Y itself.   Choice:`, naming the actual key both times.
  - **Assembly quality pass.** A mechanical sweep of `EGT80.Z80` found and fixed:
    two source lines over the 80-column house limit; `HBCNT` written but never
    called (the HBIOS unit list scanned a fixed 0–3 instead of asking how many
    units exist — it now asks, lists only those, and refuses a unit the firmware
    does not list); an orphaned label left behind when the detection hint replaced
    the older notice; and **two label pairs that collide within their first six
    characters** — `MASCHB`/`MASCHB2` (new) and `XREADY`/`XREADY1` (latent since
    the transfer code was written). The six-character rule is in the editing rules
    because a stricter assembler may treat such a pair as *one* label and merge
    them silently; both are renamed. Also `SHRATE` fell through to the baud table
    for the 88-SIO, printing a rate for a chip with no rate register — every
    switch on the port family was audited for the new driver. Screen conventions
    are now uniform: every full screen clears first, with the one deliberate
    exception commented (the ASCI menu must not clear, or it would wipe the base
    it just reported).
  - **The port menu names machines rather than chips.** Choosing a port meant
    choosing a chip family first, which is a question only someone who already
    knows their board can answer — and two entries claimed an SC126 (Z180 ASCI
    *and* RomWBW HBIOS), with only one of them able to work. The top level now
    reads: the gateway's own emulated port (option 1, and the shipped default, so
    EGT80 and the gateway work together with nothing set), RomWBW firmware, the
    Altair 88-2SIO, the Altair 88-SIO, "other hardware", and CP/M `AUX:`. The old
    chip list survives intact one level down, free-form address entry included, so
    nothing became unreachable. A line above the list states which key follows
    from what the program can determine about the machine — RomWBW present, a Z180
    without it, or neither — which is as far as detection can honestly go, since
    probing an unmapped port on real hardware is not something to do blind.
  - **New driver: the Altair 88-SIO** (the original MITS board, 0x00/0x01 or any
    address), as its own menu item. It is a different board from the 88-2SIO, not
    just a different address: it reports ready by pulling a bit *low*, so a 6850
    driver pointed at it reads every test inverted and the port appears
    permanently busy and permanently empty at the same time. Verified in the
    emulator against the `altair_sio` profile: `AT` → `OK`, `ATI` identifying the
    modem.
  - **Z180 ASCI reaches the port the way the machine actually works.** Reported
    from an SC126: `atdt` produced no error but a few random characters, while
    other comms software on the same wire was fine. The reason is not addressing — it is
    ownership. RomWBW's SC126 build enables its ASCI driver with interrupts in
    mode 2, so an interrupt handler is draining the receiver into the firmware's
    own buffer; a program polling the same registers races that handler, which
    reads the data register first. The transmitter still accepts bytes, hence no
    error. This is exactly why RomWBW-targeted builds of such software exist.
    Choosing a channel under option 4 therefore now asks the firmware whether it
    serves that channel, and if it does, reaches it through HBIOS the way
    RomWBW-targeted software does — saying so, and naming the unit. Direct register
    access remains for a Z180 the firmware is *not* driving, where it is the only
    way in. The port menu also states when RomWBW is detected, so a user need not
    know which of the two options applies to their machine.
  - **Setting the ASCI I/O base no longer looks like selecting the port.**
    Reported from real use: choose Z180 ASCI, set the base, back out — and the
    port menu still showed the old port, because the family is only selected
    when a *channel* is chosen. The base is now filled in from RomWBW on the way
    into that menu (so on a RomWBW machine it is not a step at all), the menu
    shows the base in force and says nothing is selected until a channel is
    picked, both base setters end with "now pick the channel", and `Q` says it
    leaves the port as it is.
  - **The opening menu names the menu key**, so terminal mode is visibly not a
    one-way door before you enter it: `Keys: ^Y leaves terminal mode for the
    menu; then E returns here.` The Settings item for the clear-screen dialect is
    now just `Clear Screen`.
  - **On a RomWBW machine EGT80 asks the firmware instead of guessing.** The
    HBIOS unit prompt now lists the character units that actually exist, each
    with the device type and base I/O address RomWBW reports for it
    (`CIOCNT` + `CIODEVICE`, both published calls), so "which unit?" is read off
    the screen rather than guessed; a unit that does not exist says so. The Z180
    ASCI menu gained `R`, which takes the register base from that same answer —
    necessary because `C0` is only mostly right (RomWBW uses `C0` on the Small
    Computer, RC2014-Z180, SZ180, GMZ180, DYNO and EPITX platforms, but `40` on
    N8, MK4, N8PC and RPH). The reported base belongs to a physical channel and
    channel 1 sits one address above channel 0, so the channel is subtracted to
    recover the block base.
  - **The emulator's HBIOS no longer claims to be a 16C550.** `CIODEVICE`
    reported device type `0x00` on the reasoning that zero meant "no driver" —
    but `0x00` is `CIODEV_UART` in the published list, so the answer was a
    definite claim to be a chip we are not. Nothing noticed until EGT80 began
    displaying that field and the virtual modem listed itself as
    `UART base 00`. It now reports a type outside the published range, which
    decodes as "not one of the known drivers" — the truth for a TCP connection.
  - **The Z180 ASCI menu offers the RomWBW I/O base by name.** The Z180's serial
    registers live inside the CPU and the register block is relocatable; RomWBW
    puts it at `C0` on Small Computer Central boards, so EGT80's default of `00`
    addressed nothing there and failed the way that family always fails —
    silently. `C` now sets `C0` with the boards named on the menu, `B` still
    takes any base by hand, and the help says which is which. (On a RomWBW
    machine the HBIOS family sidesteps the question entirely: the firmware knows
    where its own registers are. That remains the recommended path, and the one
    RomWBW-targeted builds use.)
  - **Every EGT80 screen is now checked to fit a 24×80 terminal**, by a new test
    that parses the `DB` strings out of `EGT80.Z80` — a source-level check, so it
    runs in CI even though the binary itself cannot be rebuilt there. It found
    help page 3 had been two rows over since it was written and page 2 going over
    as line settings were described in it, both of which pushed the page's own
    heading off the top; both are trimmed, and each help page now clears the
    screen instead of scrolling on below the previous one.
  - **Line settings (baud rate, data bits, parity, stop bits)** on a new Settings
    submenu, applied where a terminal genuinely can and refused with an
    explanation where it cannot. **RomWBW HBIOS** takes speed and framing in one
    `CIOINIT` call — the published line-characteristics word, whose baud field is
    an exponent pair (`V = 75 × 2^X × 3^Y`, bits `YXXXX`) rather than a rate, with
    RTS and DTR asserted deliberately because clearing DTR on a real modem drops
    the call. **Z180 ASCI** sets the rate and framing in CNTLA/CNTLB by
    read-modify-write, preserving the receiver and transmitter enables (reasoned,
    not run: our Z80 core has no `IN0`/`OUT0`). A **6850 ACIA** sets framing and
    the ÷1/÷16/÷64 clock divider; the eight combinations it lacks (7 data bits
    without parity, 8 with parity and two stop bits) are refused by name rather
    than rounded to the nearest, because a framing mismatch presents as garbage
    characters. A **Z80 SIO/2 has no baud generator at all** — the bit rate comes
    from the board's clock or CTC and its registers are write-only, so EGT80
    declines rather than reprogram from a guess — and CP/M `AUX:` belongs to the
    OS. Against the gateway itself none of it applies in either direction: a TCP
    connection has no bit rate, so the emulated UART accepts line-configuration
    writes and ignores them. The default is therefore **program nothing at all**,
    with `R` to return to it: the port keeps whatever the ROM, firmware or OS set
    up. Applying makes it stick — it is re-applied whenever the port is selected,
    including at startup, guarded so a config carried to the wrong machine cannot
    `RST 8` or `IN0` on hardware that has neither. Verified end to end in the
    emulator: after applying 19200 7E2 through HBIOS, a `CIOQUERY` probe read back
    `289E`, exactly that framing plus RTS+DTR. The four ASCI register operands
    are patched by `ASCPAT` along with every other one in that family — a review
    caught them missing, which would have sent the CNTLB writes to CNTLA, whose
    receiver/transmitter enables would then have silenced the port.
- **EGT80: `^C` backs out of every menu**, not just the notice screens — the port
  family list, all four per-family prompts, and Settings — so one habit works
  everywhere. `Q` still does the same, and the menus say so.

### Fixed
- **`ATDT ethernetgateway` now works from inside CP/M.** The physical serial
  modem has always answered the gateway's own dial targets — the keywords
  `ethernetgateway` / `ethernet-gateway` / `ethernet gateway` and the built-in
  number `1001000` — but the CP/M emulator's virtual modem knew nothing about
  them: the keyword fell through to the TCP path, failed to parse as
  `host:port`, and came back `NO CARRIER`. Dialing it from EGT80 now spawns a
  session on this gateway's own menu over an in-memory duplex, exactly as the
  physical modem does, with raw serial semantics (no telnet IAC negotiation,
  whose bytes would reach the guest as garbage). Two more parity gaps went with
  it: a **bare hostname no longer needs an explicit port** (a target with no
  `:port` defaults to telnet's 23, so `ATDT bbs.example.com` dials instead of
  failing), and a **phone number is looked up in the dialup phonebook** as on
  the physical modem.
- **The CP/M virtual modem's AT settings now persist, as the physical ports'
  always have.** `AT&W` was swallowed by the command parser's catch-all: it
  answered `OK` having stored nothing, and the modem was rebuilt at factory
  defaults on every entry to the emulator, so a comms program's init string had
  to be retyped each time. `AT&W` now writes the profile — echo, verbose, quiet,
  result-code level, DCD mode and all 28 S-registers — to new `cpm_emu_*` keys,
  and the modem powers up with it. `ATZ` restores the saved profile rather than
  the factory one, matching both a real modem and this gateway's serial ports,
  while `AT&F` remains the command that ignores it. Values are clamped and an
  unparsable S-register list falls back to the power-on registers, so a
  hand-edited file cannot produce a state the AT layer never would. The profile
  is editable in the web and GUI config editors beside the other CP/M settings —
  the same treatment the ports' `AT&W` fields get, and for the same reason.
  Verified end to end: `ATE0 S0=3 S7=20 &C0` then `AT&W`, gateway restarted, and
  the emulator came back with echo off, `S0=3`, `S7=20`.
- **`ATD <host>` no longer eats the first letter of the hostname.** The CP/M
  modem stripped a leading tone/pulse modifier *after* trimming, so any host
  beginning with `T` or `P` lost that letter when dialled without a modifier:
  `ATD telnetbible.com` silently dialled `elnetbible.com` and reported NO
  CARRIER. A `T`/`P` is now only a modifier where it sits against the `D`
  (`ATDT host`), which is how the physical modem has always behaved — it honours
  modifiers only inside a phone-like string. Found reviewing the dial path, and
  confirmed against a real BBS.
- **Backspace works at the CP/M modem's `AT` prompt.** The byte was removed
  from the command line, but the echo was the raw `BS`/`DEL` — a bare `BS` only
  walks the cursor left and leaves the character on the screen, and EGT80's
  filter drops `DEL` outright as printing-terminal litter. Either way the line
  looked uneditable, so a typo could only be fixed by starting again. The echo
  is now a destructive erase (`BS SPACE BS`), and nothing is echoed when the
  line is already empty, as a real modem does.
- **The CP/M virtual modem no longer chokes on `CR NUL` line endings.** An NVT
  telnet client writes a bare Return as `CR NUL` (RFC 854). The `CR` ended the
  command correctly, but the `NUL` stayed in the modem's line buffer and became
  the first character of the *next* command — so it no longer began with `AT` and
  came back `ERROR`. The first command of a session worked and every one after it
  failed, which made a parsing bug look like a dialling problem: a correct
  `ATDT host:port` was refused while the identical first attempt had been
  accepted. `NUL` is now ignored exactly as `LF` already was, which is what a
  real modem does with padding. Found from a user's screen, not from the tests —
  the test driver sent a bare `CR`, so nothing exercised the case; the CR-NUL
  sequence is now a regression test.
- **Kermit refuses a binary file declared as text, even with no length given.**
  The length check added earlier can only fire when the peer declares a size,
  and the CP/M clients that hit this in practice do not: some send the file
  type but no length, and others send no attribute packets at all. A peer
  that declares TEXT mode and then sends bytes plain text never contains (NUL,
  and the other C0 codes that are not `BEL`/`BS`/`TAB`/`LF`/`VT`/`FF`/`CR`/`ESC`
  /`^Z`) is sending a binary file that stopped at the first `^Z`, so the upload
  is now refused in-band on the content alone. The test is deliberately narrow —
  ANSI art, tabs, a trailing `^Z` and high-bit text (WordStar, PETSCII, UTF-8)
  are all still accepted as text. A peer that declares nothing cannot be
  distinguished from a legitimate binary upload, so that case is logged with a
  warning rather than refused.
- **The CP/M emulator no longer reports a RomWBW system when none is
  configured.** The `RST 8` vector is only installed for an `hbios_*` profile,
  but the trap address itself is always live, so a guest that reached it another
  way (a `CALL` straight at it, or a stray jump) got a successful `VER` reply on
  a port profile — telling a program that probes before choosing how to reach
  its modem exactly the wrong thing. Every HBIOS function now fails when no
  HBIOS access mode is selected.
- **A CP/M program parked on a blocking HBIOS call now honours the session idle
  timeout.** The timeout lived only in the console read path; the parked path
  polls the modem instead of blocking on the wire, so an abandoned session could
  sit in a 2 ms poll loop indefinitely. Any progress or keystroke resets it, so
  a program legitimately waiting for an inbound call is only closed when the
  user has actually gone away.
- **Kermit uploads that arrive incomplete are no longer saved as if whole.**
  Three gaps let a corrupt upload reach disk silently — every packet's block
  check passed, the sender said "end of file", and we wrote what we had:
  - **Truncated files are now refused.** The receiver compares the byte count
    it collected against the length the sender declared in its attribute
    packet; a short file gets an E-packet (so the peer's user sees the failure)
    and is never committed. Arriving *longer* than declared stays legal — a
    text-mode sender declares its on-disk size and then expands line ends on
    the wire — and is logged, not refused.
  - **A sender-abandoned file is now discarded, per spec §4.7.** An EOF packet
    carrying `D` means the sender gave up part-way through; we previously ACKed
    it and kept the partial bytes, committing a truncated file. The record is
    now dropped and the session continues, so the rest of a batch still lands.
  - **A peer uploading in text mode is now called out in the log.** When the
    attribute packet declares ASCII/text file type, the receiver warns that
    binary files (`.COM`, game data) will be corrupted and names the one-line
    fix on the peer (`SET FILE TYPE BINARY`) — CP/M Kermit defaults to text
    mode, where the sender stops at the first `^Z`.
- **Kermit server no longer retains every uploaded file in memory for the
  whole session.** The server-mode dispatch loop now frees each received
  file's payload as soon as the `on_file` hook has committed it to disk, so a
  long-lived session on the always-on serial or standalone-TCP Kermit server
  (both reachable without authentication) can't accumulate every upload's full
  contents in memory across an unbounded number of transfers. Filenames and
  metadata are still returned for the post-session summary; no behavior change
  for callers (all committing already went through `on_file`).
- **CP/M emulator — correctness/stability fixes from a full review of the new
  emulator.** None affect a released version (the emulator is new in 0.8.0):
  - **Interactive programs no longer hang on console-status polling.** BDOS 11
    (console status) and BDOS 6 sub-function `0xFE` reported "no key ready"
    even when a keystroke was already buffered, so the standard
    `LD C,11 / CALL 5 / OR A / JR Z` poll idiom spun until the instruction
    ceiling — hanging full-screen / interactive `.COM`s. They now report a
    buffered key (both are non-blocking); BDOS 6 direct console *input*
    (`E=0xFF`) stays blocking (the common single-key / `Y-N` idiom), with the
    non-blocking poll served by the `0xFE` status call and BDOS 11.
  - **A telnet `CR NUL` Enter no longer skips a launched program's first
    prompt.** A telnet client transmits a bare Enter as the NVT pair `CR NUL`;
    the command-line reader consumed the `CR` but left the `NUL` queued, so a
    `.COM` launched from that line (e.g. `CLRDIR B:`) had its first console
    read — often a `Y/N` confirmation — satisfied by the stray `NUL` and never
    waited for the user. The line reader now also drains the trailing `NUL`
    (and `LF`), so no terminator byte leaks to the next read.
  - **A single `ESC` at a program's line prompt no longer drops the session.**
    BDOS 10 (read-console-buffer) is now read through the same console path as
    the other calls: `CR` terminates, backspace edits, and a double-`ESC`
    aborts the program back to `A>` (a lone `ESC` was previously mistaken for a
    disconnect, and the "ESC twice to stop" promise did nothing mid-line).
  - **A BDOS call made via the `0x0006` entry-address pointer is now serviced.**
    Only the `0x0005` entry was trapped, so a program that called the BDOS
    address read from `0x0006` ran off into uninitialised memory.
  - **The CP/M inbound-call request is cancel-safe.** A `request_cpm_call`
    cancelled mid-wait (the slave announcer aborted on shell exit, or a dial
    racing shutdown) no longer leaves a stale call in the endpoint slot — which,
    with two or more concurrent CP/M sessions, could spuriously report BUSY to
    real callers or "answer" a dead call. Reclaimed via an RAII guard mirroring
    the A/B peer slot.
  - **Existing files resolve case-insensitively.** An operator-placed lowercase
    host file (`foo.txt`) that appeared in `DIR` can now actually be opened /
    `TYPE`d / renamed, not just listed — CP/M's uppercase 8.3 name is matched
    case-insensitively (new files are still created uppercase).
  - **`+++` escape guard time.** The online `+++` escape now requires a
    preceding idle gap (S12), so a `+++` inside a binary data stream is treated
    as data instead of dropping the guest to command mode mid-transfer.
  - **Altair 88-SIO honours transmit-not-ready.** The `altair_sio` profile now
    clears its TX-ready bit when the transmit ring is full, so the no-byte-loss
    flow-control guarantee holds for it as it already did for the SIO / ACIA
    profiles.
  - **`STAT` now reports real free space instead of 0 bytes.** The disk-info
    BDOS calls `STAT` uses — 31 (Get Addr(DPB)) and 27 (Get Addr(Alloc)) —
    were unimplemented and returned 0, so `STAT` read a garbage disk-parameter
    block / allocation vector from address 0 and reported "0 bytes remaining"
    on every drive. The emulator now synthesizes a fixed 8 MB / 4 KB-block DPB
    and an allocation vector whose used bits reflect the drive's actual file
    usage, so free space is reported correctly.
  - **More BDOS calls implemented (were silent no-ops returning 0).** Function
    40 (write random with zero fill) now aliases function 34 instead of
    returning fake success while dropping the write — a real data-loss fix for
    any program that writes via 40. Function 24 (return login vector) reports
    all eight drives A:–H: active. Function 5 (list / `LST:` output) routes to
    the console so a program's printer output stays visible. Function 32
    (get/set user number) is now tracked and shared with the `USER` command, so
    a program's save/restore-user sequence is self-consistent (files remain a
    single flat area, not segregated by user — a documented simplification).
    Functions 7/8 (get/set I/O byte) now read and write the IOBYTE at its
    page-zero home (0x0003) so a set-then-get round-trip is consistent (device
    redirection has no effect in the single-console model, but the value is no
    longer silently dropped).
  - **BIOS jump table for direct-console software.** Programs that bypass BDOS
    and do console I/O straight through the BIOS jump table (MBASIC, WordStar,
    Turbo Pascal, Infocom games) now work: a real 17-entry CP/M 2.2 BIOS jump
    table is laid in high memory, the warm-boot pointer at 0x0001 points at its
    WBOOT entry, and each vector traps to the host, which services the console
    group (CONST/CONIN/CONOUT/LIST/PUNCH/READER/LISTST) against the live
    session. Also fixed: `STAT B:` no longer strands you at the `B>` prompt —
    a transient's internal drive select is now undone when it exits (the CCP
    re-selects its own default each command cycle, as real CP/M does), so only
    a bare `d:` command changes drives; and `STAT`'s allocation vector no
    longer overran the new BIOS jump table.
  - **All sixteen CP/M drives A:–P: now available** (was A:–H:). CP/M 2.2's
    architectural maximum is 16 drives; the emulator now auto-creates a folder
    for each under `CPM/` and reports all sixteen in the login vector. Each is
    a formatted, empty drive the instant its folder exists — the CP/M directory
    is synthesized from the folder's real files, so there is no format/`CLRDIR`
    step.
  - **`TYPE` now paginates** in the emulator. It previously streamed the whole
    file past the screen in one go; it now stops each screenful with the same
    `--More-- (SPACE, RET, Q)` viewer the Gateway Shell uses (SPACE = next page,
    RETURN = one line, Q/ESC = quit), expanding tabs and wrapping long lines to
    the terminal width.

### Documentation
- **Documented that the Kermit *client* must be set to binary file mode before
  transferring programs or data.** Vintage Kermit clients default to text
  (ASCII) mode, and a binary file sent that way silently loses bytes: a CP/M
  client reads the file in 128-byte records and in text mode treats `^Z` (0x1A,
  the CP/M end-of-file pad) as padding rather than data, so a `^Z` that falls on
  a record boundary is dropped and every following byte shifts down one
  position. Nothing in the protocol catches it — every packet's block check
  passes and the client reports success — so the file that lands is subtly wrong
  rather than obviously truncated. Diagnosed from a real SC126 upload where an
  8,704-byte `WITNESS.COM` and a 104,960-byte `WITNESS.DAT` arrived 1 and 8
  bytes short: nine dropped bytes out of 113,664, enough to stop the game
  running but far too few to notice by comparing file sizes casually.
  Re-uploading after `SET FILE MODE BINARY` produced files byte-identical to the
  originals. The gateway itself always transfers binary, byte-for-byte, in both
  directions — it never translates line endings and never trims padding — so
  this is purely a client-side setting, which is why a download can be perfect
  while an upload of the very same file is damaged. Documented in four places
  with the per-client commands (kercpm3 / CP/M Kermit `SET FILE MODE BINARY`,
  C-Kermit `set file type binary` / `-i`, MS-DOS Kermit): a new aside in user
  manual §8.8, a new user-manual troubleshooting entry §16.11 ("Transferred File
  Won't Run (Wrong Size by a Few Bytes)") framed around the symptom as a user
  meets it, and callouts on the `web/kermit.html` and `web/kermitreference.html`
  reference pages. Each notes that the short-file refusal added earlier in this
  release only fires when the client declares a length in its attribute packet —
  many small clients send none, leaving the receiver nothing to check the
  arriving size against — so binary mode is the habit to keep rather than
  something to rely on the guard for.
- **Documented that packet counts are not an integrity check.** The same pages
  now warn against comparing the packet count of an upload against a download of
  the same file: control bytes and high-bit bytes are quoted into two or three
  wire characters each, so the expansion — and therefore the packet count —
  legitimately differs with the negotiated packet length and with which side is
  sending. A differing count is not evidence of data loss and a matching one is
  not evidence of success; compare checksums instead.
- Regenerated `usermanual.pdf` from the updated HTML per `versionchange.txt`
  (WeasyPrint, `Producer` unchanged). The rebuild also picks up earlier HTML
  edits that had never been rebuilt, so the PDF grows by more than the entries
  above account for.

## [0.7.0] - 2026-07-17

### Added
- **Gateway Shell — a CP/M-inspired file manager over telnet/SSH.** A new
  `S  Gateway Shell` item on the File Transfer menu opens an `A>` command prompt
  that presents the transfer directory as drive A: (pure Rust, **no** Z80 or
  `.COM` emulation). Resident commands `DIR`/`LS`, `TYPE`, `DUMP`, `ERA`
  (`DEL`/`RM`), `REN`, `COPY` (`PIP`/`CP`), `MOVE` (`MV`), `MKDIR` (`MD`),
  `RMDIR` (`RD`), `CD`, `PWD`, `STAT`, `HELP` (`?`), and `EXIT` cover full file
  management, including **cross-directory** copy/move via a `/`-separated path
  syntax the base CP/M command set can't express, and `*`/`?` wildcards for
  `DIR`/`ERA`/`COPY`. `TYPE`/`DUMP`/`DIR` paginate with a `--More--` prompt, and
  `TYPE` refuses binary files. Every operand is jailed to the transfer directory
  (validated + canonicalized; `..`/absolute/symlink escapes are refused); copy/
  move honor the 8 MB transfer cap and the `TYPE`/`DUMP` viewers cap reads at
  1 MiB. Works identically over telnet and SSH. Documented in user manual §8.10.
  (A real Z80 CP/M 2.2 emulator remains deferred.)
- **Third-party license notices and a license-policy gate.** `THIRD-PARTY-NOTICES.md`
  (generated by [`cargo-about`](https://github.com/EmbarkStudios/cargo-about) from
  `about.toml` + `about.hbs`) reproduces every dependency's copyright notice and
  license text. A new CI `licenses` job runs
  [`cargo-deny`](https://github.com/EmbarkStudios/cargo-deny) against the
  GPL-3-compatible allowlist in `deny.toml` and **gates** the build (no
  `continue-on-error`), so a GPL-3-incompatible or unknown-licensed dependency
  can't land silently. README documents the workflow.

### Changed
- Added a full "as is" / no-warranty disclaimer at the top **and** bottom of
  both `README.md` and the user manual (`usermanual.html`), including a note
  that portions of the project were developed with the assistance of AI tools.
- **`README.md` condensed to a quick-start + feature overview** (~1815 → ~190
  lines). The reference material that duplicated the user manual (the annotated
  `egateway.conf` dump, AT-command / S-register tables, telnet RFC compliance
  matrix, release-verification and systemd walkthroughs, per-distro build
  matrix) now lives only in the manual, with links; the repo-unique hardware
  quick-start, security posture, and license are kept, and the screenshot is
  surfaced near the top.
- **User manual: documented kercpm3's `Password:` prompt on `remote cd`.**
  CP/M Kermit clients prompt for the optional Kermit CWD password (Protocol
  Manual §6.7 second field) before sending the `G C` packet; the gateway's
  Kermit server is unauthenticated by design and ignores that field, so the
  directory change always succeeds — press Enter. Also noted the
  recognised-but-unsupported `USER` command in the Gateway Shell (§8.10).

### Fixed
- **Gateway Shell: `DIR SUB` now lists the subdirectory's contents** (like
  `DIR SUB/`) instead of just showing the `<DIR>` entry — a wildcard-free
  operand that names an existing directory is listed, matching the DOS/Unix
  expectation. `DIR name`, `DIR *.pat`, and `DIR file` are unchanged.
- **Gateway Shell: name resolution is now case-insensitive.** `DIR` shows names
  uppercased, so a directory stored on disk as `z80asm` displayed as `Z80ASM`
  and `CD Z80ASM` then failed "No such directory." (only the lowercase form
  worked) — and PETSCII terminals swap case on the wire, compounding it.
  `CD`/`TYPE`/`DUMP`/`ERA`/`STAT`/`REN`/`COPY`/`MOVE` source operands now match
  an existing name case-insensitively (exact case wins, else the first
  case-insensitive hit) and resolve to the real on-disk name; newly created
  names keep the case as typed. Still fully jailed to the transfer directory.
- **Gateway Shell: the `A>` prompt after HELP now appears on its own line.** The
  help pager's "Press any key" was dismissed with the cursor mid-line, so the
  returning prompt was glued to it (`Press any key.A>`); the pager now advances
  to a fresh line before returning (harmless for the menu callers, which
  redraw).
- **PETSCII: declining color no longer drops a Commodore terminal to ASCII.**
  Color was tracked implicitly by the terminal type, so answering "N" to the
  color prompt forced `TerminalType::Ascii` — which also discarded PETSCII's
  40-column, case-swapped, ANSI-stripped layout, leaving a C64 caller in an
  80-column ASCII view. Color is now a separate `color_enabled` flag: declining
  color keeps the detected terminal type (PETSCII stays PETSCII) and simply
  emits plain text. Also makes the SSH/telnet gateway's PETSCII handling correct
  for a no-color C64. Side effect for **ANSI** terminals: declining color now
  means "no color in the gateway's own menus" rather than "downgrade to ASCII,"
  so remote-host ANSI passed through the SSH/telnet gateway is no longer stripped
  and the onward terminal is advertised as `xterm` instead of `dumb` — the
  correct behavior for a terminal that answered the ANSI-color prompt.
- **Serial: a serial-manager thread can no longer panic on a dropped runtime
  across a config restart (round-7 review).** The detached serial threads
  `block_on` the tokio runtime, but a SIGHUP restart dropped the runtime
  without joining them; a thread stuck in the synchronous `connect_timeout`
  (an in-flight ATDT/peer dial to an unresponsive host, up to 60 s) would then
  panic on its next `block_on`. The dial now connects asynchronously, raced
  against the shutdown/restart flag (aborting within ~100 ms instead of being
  blind for the whole carrier wait), and `main` bounded-joins the serial
  threads before dropping the runtime. Self-healing before; airtight now.
- **Web browser: a hostile page can no longer soft-DoS the render thread with
  form-label lookups (round-6 F1).** Each form field with an `id` but no
  placeholder/aria-label/title triggered a full recursive walk of the form
  subtree looking for `<label for="id">` — O(fields × subtree), so a page of
  tens of thousands of bare `<input id=…>` (under the 1 MB body cap) cost
  quadratic CPU on a shared `spawn_blocking` thread with no render time budget.
  Labels are now collected in a single O(subtree) pass into an id→label map;
  per-field lookup is O(1).
- **Weather: a malformed forecast timestamp no longer panics the fetch
  (round-6).** The MET.no forecast parse sliced the first 10 bytes of the
  `time` field guarded by byte length only; a timestamp with a multibyte char
  in the first 10 bytes would panic on a mid-character boundary. It now uses a
  char-boundary-safe accessor and skips a malformed entry. (Was contained to a
  single weather fetch by `spawn_blocking`, never a process crash.)
- **Serial: dialing a console-mode port relayed to the master no longer wedges
  the caller's serial thread (round-5 review).** On a slave gateway with
  `allow_peer_dial` on, `ATD <ConsolePort>@<local-ip>` reached a local console
  bridge that nothing services (the console port runs the master-registration
  loop, not the local bridge), so the caller's thread blocked forever on the
  bridge oneshot — unrecoverable short of a full restart. `connect_local_peer`
  now fails that dial fast with NO CARRIER (mirroring the Serial Gateway
  picker's exclusion), and the console-bridge request is additionally raced
  against a shutdown/restart poll so no path can pin the thread.
- **Relay: IPv6 onward-dial targets are handled correctly (F1).** The onward-
  dial path split/rebuilt `host:port` with a bare `rsplit_once(':')`, leaving
  brackets on an IPv6 host so `connect` failed. A shared `split_dial_host_port`
  now parses `[2001:db8::1]:6400` into a bare literal, the slave brackets IPv6
  on the wire, and both halves agree (unbracketed IPv6 is rejected as
  ambiguous). IPv4/hostname dialing is unchanged.
- **Telnet: a session-slot / broadcast-writer leak on a panicking session is
  now prevented (F3).** Slot release and writer de-registration ran only after
  `session.run()` returned, so a future reachable panic would leak a
  `max_sessions` slot and grow the broadcast list unbounded. A RAII backstop
  (`SessionSlotGuard`) now reclaims both on unwind; the normal path defuses it
  after the graceful async cleanup. (No panic is reachable today — this is
  defensive hardening consistent with the SSH/relay Drop guards.)
- **Kermit receive: windowed receiver now ACKs buffered out-of-order packets
  (selective repeat, spec §5.5) (K1).** Previously it buffered a correctly-
  received future packet but only NAKed the missing `expected_seq` — once per
  future — never ACKing the good packet; the windowed sender counted each
  duplicate NAK as a retry and re-sent packets it didn't need to. The receiver
  now ACKs each future by its own seq and NAKs the gap once, matching the spec
  and removing the redundant retransmissions (and the retry-budget pressure a
  reordering link could put on a large window). Live C-Kermit 10.0 sliding-
  window interop unchanged.
- **Punter receive: a premature-retransmit duplicate no longer corrupts the
  file (P1/P2).** On a slow/jittery link a data block whose first byte was
  delayed past the byte-wait could trigger an early `S/B` resend; the delayed
  block and a re-sent copy would both arrive, and the receiver — which ignored
  the block index for sequencing — appended the interior block **twice**,
  returning a silently one-block-too-long file. The receiver now dedups on the
  checksum-protected `NUMPOS` block index (dropping a block whose index did not
  advance), bounded so a peer stuck re-sending one block still gives up. Verified
  against the live CCGMS reference (both directions) — the dedup never triggers
  on conforming traffic.
- **Punter: a mid-IAC-sequence timeout in a handshake window is now recoverable
  (P3).** `accept_code` treats the `tnio` IAC-timeout (N4) as a soft "no code
  this round" re-probe — matching `read_block` — instead of aborting the whole
  transfer; other read errors still abort.
- **Serial: a boot-time thread-spawn failure no longer panics the whole
  process (N5).** `start_serial` logs the failure and continues, so the rest
  of the gateway (telnet/SSH/web and the other serial port) still comes up;
  only the affected port is disabled.
- **Telnet I/O: a truncated IAC sequence can no longer wedge a reader (N4).**
  Bytes read *inside* an already-started IAC sequence (the command byte, and a
  WILL/WONT/DO/DONT option byte) are now bounded by a 5 s timeout — matching
  the existing SB-drain bound — so a peer that sends a lone `0xFF` and stalls
  can't block `read_exact` forever. The first-byte wait is still caller-timed.
- **ZMODEM send: the post-ZEOF ZRPOS recovery reuses the main data-send path
  (Z6).** The recovery previously duplicated the ZDATA/subpacket loop inline
  (a drift risk with weaker ACK handling); it now calls the same
  `send_zdata_run` helper as the initial data phase.
- **ZMODEM send: ZFILE now advertises binary conversion (Z3).** The ZFILE ZF0
  byte carries `ZCBIN` instead of 0, so a text-defaulting receiver won't apply
  newline translation to a binary payload.
- **ZMODEM: slow-link timeouts are no longer capped by hardcoded literals
  (Z5).** The between-files header wait now uses the configured
  `zmodem_frame_timeout` (was a fixed 10 s), and the post-ZEOF ZRINIT wait
  keeps its 15 s fsync floor but rises to a tuned-up `zmodem_frame_timeout`.
- **XMODEM receive: the 8 MB file cap is enforced exactly (X2).** The size
  check now runs before appending each block, so the buffer never exceeds
  `MAX_FILE_SIZE` even transiently (previously a file could grow one block
  past the limit before the top-of-loop check fired). Exactly 8 MB is still
  accepted.
- **Web browser: the DOM text-extraction dependency is pinned against silent
  breakage (A2).** Title/form-text extraction parses html2text's debug DOM
  rendering (html2text 0.14 exposes no Text-node walk); a canary test now
  guards that format so a dependency bump fails loudly in CI instead of
  silently returning empty titles/labels.
- **XMODEM receive: auto-detect no longer stalls 60 s against a strict
  lock-step checksum-only sender (X1).** On the first block, when the session
  is in CRC mode but the sender emits a single 1-byte checksum trailer and then
  waits for our ACK/NAK (vintage Christensen 1977 / CP/M MODEM7 / C64 BBS
  uploaders that ignore our `C`), the CRC low-byte read is now gated behind a
  short grace window: if no second trailer byte arrives, the receiver falls
  back to 1-byte-checksum validation and locks to checksum mode instead of
  blocking until the full block-body timeout. A genuine CRC sender's low byte
  arrives back-to-back and is unaffected; after the first block the mode is
  locked and the read blocks unconditionally. The symmetric checksum-mode
  auto-detect branch (the extra CRC-probe read on a first-block checksum
  mismatch) is gated the same way, so a lock-step checksum sender with a
  corrupt first block is NAKed promptly rather than after 60 s.
- **ZMODEM receive: the data-phase retry counter is bounded consistently.**
  `nak_or_abort` now tolerates `max_retries` consecutive errors (`>`), matching
  the ZFILE-subpacket and XMODEM counters and its own "bounded by max_retries"
  contract, instead of aborting one retry early (`>=`).
- **ZMODEM receive: a corrupt ZFILE info subpacket no longer aborts the whole
  batch (Z1).** The filename/size subpacket is now read with the same
  ZNAK/retry discipline as the data phase — per Forsberg §7 the receiver ZNAKs
  and the sender retransmits the ZFILE frame — so a single bit-flip or
  truncation in the metadata is recovered instead of killing the transfer.
  Bounded by `zmodem_max_retries`, so a permanently broken link still cancels.
- **ZMODEM receive: the sender's "OO" over-and-out trailer is now drained
  (Z2).** After replying to ZFIN the receiver consumes the two `O` bytes the
  sender emits per §8.4; previously they leaked into the terminal session that
  resumed after the transfer as spurious `OO` input. Best-effort with a short
  timeout — a peer that omits OO is unaffected.
- **SSH: reject auth when the configured username or password is empty (N2).**
  Because `constant_time_eq(b"", b"")` is `true` and SSH has no
  unauthenticated mode, an operator who blanked the password would otherwise
  turn the SSH port into an open shell bridge. Auth is now refused outright
  when either stored credential is empty.

## [0.6.4] - 2026-07-14

### Added
- **Serial ports gain a third mode: Kermit Server.** Alongside *Modem
  (AT Command) Mode* and *Telnet-Serial Mode*, each serial port (A/B) can
  now run as an always-on Kermit server: as soon as the port is enabled it
  listens for Kermit packets directly on the wire — no AT commands, no
  dialing, no menu. It is the same server `ATDT KERMIT` reaches from the
  modem emulator, but always on and with no AT layer; received files land
  in `transfer_dir`, and it re-arms after every FINISH/BYE so the wire stays
  a live server. The port reopens automatically if the device disappears
  (matching modem mode). Selectable from the GUI Mode dropdown, the web
  config's per-port "More…" popup, and the telnet per-port **T** toggle
  (which now cycles Modem → Console → Kermit). Persists as
  `serial_a_mode` / `serial_b_mode = kermit`. Auth and the telnet menu are
  bypassed by design — enable only on trusted serial lines (same posture as
  `allow_atdt_kermit`).
- **YMODEM receives multi-file batches (`sb file1 file2 …`).** The receiver
  previously ran the end-of-batch handshake right after the first file's EOT,
  so a batch sender lost every file after the first (and could hang waiting for
  the receiver). It now reads the next block 0 at each EOT — a named block 0
  starts the next file, the null block 0 ends the batch — and returns every
  file. The first file keeps the user-entered name; files 2..N use the sender's
  (sanitized) block-0 name, saved atomically like ZMODEM/Kermit batches. A
  corrupt inter-file block 0 is NAK-retried (bounded), the batch is capped at
  1000 files, and a non-UTF-8 file name is received under a generated name
  rather than truncating the batch.
- **Weather works worldwide, not just US zip codes.** The Weather menu now
  accepts any city name or postal code (`London`, `SW1A 1AA`, `Zürich`,
  `62051`), percent-encoding the query so spaces and non-ASCII are safe. A
  `City, Country` or `City, Region` qualifier disambiguates common names
  (`London, GB` vs `London, Ontario`; `Paris, France` vs `Paris, Texas`), and
  the matched country is shown. A new **`weather_units`** setting — `auto`
  (default: Fahrenheit/mph for the US, Celsius/km/h elsewhere), `us`, or
  `metric` — controls display units; press **U** on the weather screen to cycle
  them in place (no re-fetch). Wired into the telnet Other-Settings menu, the
  web config page, and the GUI. The config key `weather_zip` is renamed to
  **`weather_location`**; an existing `weather_zip` value migrates automatically
  on first load, and any saved location persists across sessions as before.
- **Configurable desktop GUI display scale (`gui_zoom`).** The console window
  now honors a `gui_zoom` setting: `auto` (default) follows the monitor's own
  scale factor as before, while a number (e.g. `1.0`, `1.25`, `0.8`) pins the
  window's pixels-per-point absolutely so a display that reports an inflated
  DPI no longer renders the GUI oversized. Selectable as "Display scale" from
  the GUI's Server → More panel and the web config's Server → More page
  (Auto / 75% / 100% / 125% / 150% / 200%), and clamped to 0.5–3.0.
- **Show the file being downloaded on the SELECT PROTOCOL screen.** The
  download protocol picker now displays the file name (truncated to the
  terminal width) and byte size above the protocol list, so the user can
  confirm the right file before choosing a protocol.
- **Make directories from the telnet file-transfer menu.** A new **M** option
  creates a subdirectory inside the current transfer working directory (the
  name is validated like a filename — a single component, no `..` or `/`), then
  asks whether to make it the working directory.
- **Weather falls back to MET Norway when Open-Meteo is unreachable.** If the
  Open-Meteo forecast host can't be reached, the Weather menu now automatically
  retries the forecast against MET Norway (`api.met.no` Locationforecast 2.0 —
  free, no API key, independent infrastructure), reusing the coordinates
  already geocoded via Open-Meteo (worldwide coverage, so the fallback works for
  any location). MET's data is kept in metric and converted to your chosen units
  at display time, and its symbol codes mapped to descriptions; you only see an
  error if both providers fail.
- **Wait for the receiver before starting a Kermit download
  (`kermit_wait_for_receiver`, default on).** A Kermit transfer is
  receiver-driven at the start — the receiving side sends a `NAK` to solicit the
  sender's Send-Init (Frank da Cruz, *Kermit Protocol Manual* §4). The gateway
  now holds its Send-Init until that poke arrives and then sends exactly one, on
  both interactive downloads and Kermit server GET responses. A client that
  never pokes (e.g. C-Kermit) falls through a short bounded wait and gets an
  unprompted Send-Init as before. Wired into the telnet Kermit-settings menu
  (**G**), the web config page, and the GUI.
- **Verbose Kermit receive-path logging.** With `verbose = true`, the Kermit
  upload/receive path now emits periodic per-packet progress plus a
  per-file summary (bytes, packets, block-check type), matching the diagnostic
  style of the XMODEM/YMODEM/ZMODEM paths. Off by default.

### Changed
- **Warning popups are now dark red (GUI and web).** Security/confirmation
  warnings previously looked identical to ordinary popups, so it wasn't obvious
  the modal was blocking the next click and had to be acknowledged. The GUI's
  four warning popups (ATDT-KERMIT, Kermit-server, disable-IP-safety,
  master-needs-SSH) now use a dark-red panel + red border. On the web, the
  native `confirm()`/`alert()` warnings (disable web server, change web port,
  master-needs-SSH) are replaced with matching dark-red modal dialogs whose
  overlay blocks the form until the operator chooses Continue/Cancel. The web
  also gains the enable-guard warnings it was missing versus the GUI —
  **Disable IP Safety**, **Kermit Server**, and **Allow ATDT KERMIT** now raise
  the same red confirmation before they take effect.
- **ZMODEM batch receive is capped at 1000 files** (`MAX_BATCH_FILES`, matching
  YMODEM and Kermit), so a peer that streams endless files can't grow the
  in-memory batch without bound; the receiver cancels and errors past the cap.
- **Config UI: tidier frames via "More" popups.** The web config page now keeps
  the **Master/Slave** relay settings under the Server frame's **More…** popup
  (they were a separate card), matching the GUI and returning the page to six
  frames. The **AI Chat, Browser, and Weather** frame (both web and GUI) is now
  three rows — API key and homepage on the frame, with a **More…** button that
  opens the weather location and units.
- **Weather fetch fails fast with a clearer message.** The Open-Meteo request
  now uses a 5 s connect timeout (was a single 15 s global) and retries once on
  a transient transport failure, so an unreachable/blocked forecast host no
  longer hangs the Weather menu for 15 s. Errors are distinguished:
  "Not found - try 'City, Country'." (no geocoder match) vs "Weather service
  unreachable. Try again later." (network/host down) vs "Weather service
  returned bad data." (parse).

### Fixed
- **Serial Kermit Server Mode transfers at full speed.** Kermit Server Mode was
  far slower than the same server launched from the File Transfer menu. The
  bridge that pumps bytes between the wire and the Kermit server
  (`run_console_bridge`) drained its outbound queue only *between* wire reads,
  and Kermit is stop-and-wait — so while the gateway composed each reply the
  wire sat idle and the bridge blocked out the full wire-read timeout before
  writing that reply, adding a fixed delay to every gateway-originated packet
  (each ACK when receiving, each DATA when sending — hence the slowdown in both
  directions). The menu/`ATDT KERMIT` server never had this because its pump
  produces and flushes the reply in the same iteration that consumed the
  request. Server Mode now uses that same inline pump (`run_kermit_bridge_inline`,
  modeled on the modem online-mode path) instead of the decoupled two-thread
  bridge, so a reply leaves the wire the moment it's produced. (The interactive
  Serial Gateway console still uses `run_console_bridge`, whose backpressure
  design is intentional there.)
- **Kermit server uploads no longer drop a file on a name collision.** When an
  uploaded file's name already exists in `transfer_dir`, every Kermit-server
  receive path (the telnet-menu server, the standalone TCP listener, and the
  new serial Kermit Server Mode / `ATDT KERMIT`) now renames the incoming file
  DOS/CP-M-Kermit style instead of skipping it — the base name is numbered
  within 8 characters the way CP/M Kermit clients (e.g. kercpm3) do on a
  download collision: `abcdefgh.txt` → `abcdefg0.txt` … `abcdefg9.txt` →
  `abcdef10.txt`, and a shorter name such as `hi.txt` → `hi0.txt`. The original
  file is never overwritten, and a verified resume still replaces its own
  partial in place. (The pre-existing telnet-menu and TCP-listener paths
  previously skipped such a collision with an "already exists" note.)
- **`ATDT KERMIT` uploads now actually save to disk.** The serial Kermit-server
  dial path passed a no-op file-commit hook to the server, but the Kermit
  receiver only buffers uploaded files in memory and relies on that hook to
  persist them — so a client `send`/`put` over `ATDT KERMIT` completed on the
  wire but left nothing in `transfer_dir` (downloads/`get`, which read from
  disk, were unaffected). Both the always-on serial Kermit Server Mode (new,
  above) and the `ATDT KERMIT` dial now commit each received file with the same
  filename / subdir path-safety validation as the telnet and TCP-listener
  Kermit server paths.
- **Kermit server GET is now case-insensitive.** A client requesting a file in
  a different case than it is stored on disk — CP/M clients such as kercpm3
  uppercase filenames — no longer fails "File not found" and burns a retry
  re-requesting under another case. The server prefers an exact match, then
  falls back to a case-insensitive match among the transfer directory's direct
  entries, so the path-traversal protection is unchanged.
- **Kermit server downloads no longer provoke spurious retransmissions on
  vintage receivers.** The server was sending its Send-Init unsolicited; a
  receiver-driven client (e.g. kercpm3 on CP/M) pokes with a `NAK` to solicit
  it, that poke crossed our Send-Init on the wire, and we resent it — delivering
  a duplicate the client tallied as a retry. The server now waits for the poke
  and answers with a single Send-Init. Combined with the case-insensitive fix
  above, this cuts the retry count such clients report on a clean download from
  2–3 down to the single, unavoidable initiating `NAK` that the Kermit
  receiver-driven start requires (uploads read 0 — there the client is the
  sender and never pokes). Documented in `usermanual.html` and `kermit.html`.
- **Kermit sender no longer cascades retransmits on a duplicate `ACK`.** On a
  serial download to a hardware CP/M client (kercpm3 / Kermit-80), the
  receiver-driven start makes the gateway send its Send-Init twice, so the
  client's first `ACK` arrives duplicated — and the sender treated that stale,
  already-satisfied ACK by retransmitting the current packet, whose re-ACK
  became the next stale ACK: a self-perpetuating cascade that showed as a burst
  of dozens of "retries" before the transfer settled and ran clean. A
  stale/duplicate ACK is now discarded without retransmitting (the sender keeps
  reading for the ACK that advances the window); a retransmit still fires only
  on a `NAK` for the current sequence or a read timeout.
- **Kermit `remote dir` / `remote help` replies no longer staircase on CP/M
  clients.** The server built those listings with bare-LF line endings;
  C-Kermit on Unix masks this via the tty's `ONLCR`, but a hardware CP/M client
  (Kermit-80) does no translation, so each line stepped down without returning
  to column 0. Both bodies are now CRLF-encoded before transfer (existing CRLFs
  left intact; `TYPE`'s verbatim file bytes are deliberately untouched).
- **Web browser surfaces the real error when the AI chat API rejects a
  request.** The Groq client treated every non-2xx response as an opaque
  transport error (`http status: 401`) and discarded the JSON body, so its
  code to extract Groq's descriptive `error.message` (e.g. "Invalid API Key",
  rate-limit text) never ran. It now reads the body on error responses and
  reports Groq's own message.
- **ZMODEM downloads are no longer throttled to ~5 KB/s on fast links.** When
  reading a hex header the receiver drained up to three trailing bytes (CR, LF,
  and an optional XON), but Forsberg's `zsendhdr` omits the XON for `ZACK` and
  `ZFIN` frames — so on those the drain blocked the full 200 ms per frame
  waiting for an XON that never comes. Because our sender ACK-gates every
  1 KB subpacket (`ZCRCQ` → read `ZACK`), that phantom wait capped throughput
  near one subpacket per 200 ms regardless of link speed. The receiver now
  waits for the third trailing byte only for frame types that actually carry
  it. No wire bytes change; slow retro links are unaffected (a subpacket's own
  transmission already dwarfs 200 ms there).
- **Plain XMODEM sends no longer report a false failure at `xmodem_max_retries
  = 1`.** A Forsberg-compliant receiver NAKs the *first* `EOT` to verify
  end-of-file and ACKs only the resent one (our own receiver does this), so
  completing the handshake requires at least two `EOT` attempts. The send-side
  `EOT` loop was bounded by `xmodem_max_retries`, so at the minimum setting of
  1 it sent a single `EOT`, took the expected verification `NAK` as failure,
  and reported an error on a transfer that had actually succeeded. The `EOT`
  budget now floors at 2; a receiver that ACKs the first `EOT` still returns on
  the first pass, so the common case is unchanged.
- **Serial dial-out stays responsive to shutdown and config restarts.** When
  an `ATDT` target resolved to several unreachable addresses, the modem tried
  each in turn and could block the serial thread for (address count × the S7
  timeout) — during which a server shutdown or a per-port config restart was
  stalled. The dial loop now checks the shutdown/restart flags between address
  attempts and bails with `NO CARRIER`. The peer-dial answer-wait is likewise
  clamped to the same 60 s ceiling `ATDT` uses, so a large S7 can't pin the
  caller's port for up to 255 s while a local peer rings.
- **`SIGHUP` reloads instead of shutting the service down.** SIGHUP was wired
  to the same shutdown flag as SIGINT/SIGTERM, so `systemctl reload` cleanly
  stopped the gateway — and because the exit was clean (code 0), `Restart=on-failure`
  did not bring it back, leaving the service down. SIGHUP now arms the
  restart/reload path (re-reading config) instead of exiting, matching the
  shipped systemd unit's `ExecReload`.
- **Kermit CAPAS long-packet / sliding-window bits corrected.** The capability
  mask had `LONGPKT` and `SLIDING` transposed versus the canonical layout
  (C-Kermit `ckcmai.c`: long = 0x02, sliding = 0x04). Gateway↔gateway sessions
  and the test suite were self-consistent and unaffected, but a third-party peer
  advertising one capability without the other (e.g. G-Kermit, MS-DOS Kermit —
  long packets, no windows) was misread, desyncing the rest of the Send-Init.
  Now fixed and pinned against the C-Kermit source.
- **Serial console/modem no longer busy-loops at 100% CPU on a port EOF
  (macOS).** `run_console_bridge` and `command_mode_tick` treated a
  zero-length read (`Ok(0)`) as "no data" and re-polled immediately. The port
  is opened with a read timeout, so an idle read is `Err(TimedOut)` — `Ok(0)`
  actually means the device closed (e.g. a PTY master after its slave exits,
  where loss surfaces as EOF rather than the `Err(EIO)` a real ttyUSB gives).
  Both now treat it as a disconnect (reopening in modem mode), matching the
  online-path readers. Inert on Linux.
- **ZMODEM receiver no longer emits the sender's `OO` trailer.** Per Forsberg
  §8.4 the receiver replies ZFIN and then *reads* the sender's `OO`; emitting
  our own was a role inversion (harmless in practice — the peer had already
  sent its own and exited).

### Security
- **SSH server refuses to overwrite an unreadable host key.** If the host-key
  file existed but failed to parse (e.g. truncated by a full disk), the server
  silently generated a new key and wrote it over the old one — changing the
  server's SSH identity and tripping every client's "REMOTE HOST
  IDENTIFICATION HAS CHANGED" warning (and potentially clobbering a merely
  truncated, recoverable key). It now refuses to start the SSH server in that
  case, leaving the file untouched for the operator to restore or remove, the
  way `sshd` treats a bad host key. A *missing* key file is still generated
  normally on first run.
- **Punter receive can no longer be hung by a flood of empty blocks.** A peer
  that streamed valid-checksum, non-final, zero-payload blocks would spin the
  receive loop forever: an empty block never grows the output (so the file-size
  cap never trips) and passes the checksum (so the bad-block cap never trips).
  A conformant C1 sender emits exactly one header-only block per phase (block 0,
  which only announces block 1's size), so the receiver now bounds the number
  of accepted empty non-final blocks and gives up on a peer that exceeds it.
- **Text-mode web browser can no longer be crashed by a deeply-nested page.**
  A page whose HTML nested tags tens of thousands deep (e.g. unclosed `<div>`s,
  well under the 1 MB body cap) parsed into a DOM so deep that the browser's
  recursive title/form extractors overflowed the worker-thread stack and
  aborted the **entire gateway process** (all telnet/SSH sessions), a
  remotely-content-triggered denial of service. The browser now rejects a
  document nested deeper than 512 element levels ("Page is too deeply nested to
  render.") before those walks run.
- **Refreshed dependencies to clear RustSec advisories.** `cargo update`
  moved `aes` (yanked) → 0.9.1, `memmap2` (RUSTSEC-2026-0186 unsound) → 0.9.11,
  dropped `anyhow` (RUSTSEC-2026-0190 unsound), and bumped the egui/eframe stack
  to 0.34.3 and `russh` to 0.60.3. The two `quick-xml` DoS advisories
  (RUSTSEC-2026-0194/0195) are waived in `.cargo/audit.toml`: `quick-xml` is a
  build-time proc-macro dependency (`wayland-scanner`) that parses trusted
  Wayland protocol XML at compile time — it is not in the shipped binary and the
  gateway does no runtime XML parsing, so neither DoS path is reachable.
- **Web config UI: enabling login no longer widens IP exposure.** The
  private-IP allowlist now applies whenever `disable_ip_safety` is off,
  regardless of whether "Require Login" is on. Previously, enabling security
  *dropped* the allowlist — accepting any source IP, gated only by
  cleartext-HTTP Basic auth on a page that renders the login password and Groq
  API key into form fields. Login-gated access from arbitrary IPs is now an
  explicit `disable_ip_safety = true` opt-in. (The telnet listener is
  unchanged and intentionally still opens to any IP under `security_enabled` —
  it echoes no secrets and is the retro-hardware access path.)
- **Relay onward-dial now requires the master's `allow_peer_dial`.** A slave's
  Model-B onward-dial — asking the master to open an outbound TCP connection to
  an arbitrary external `host:port` — was gated only by `gateway_role = master`
  + `master_accept_relays`. It now also requires the master's `allow_peer_dial`
  (the same opt-in that already governs peer-dial), closing an authenticated
  SSRF/pivot/port-scan primitive available to any holder of the shared
  credentials.
- **Text-mode web browser no longer re-sends a form POST over cleartext.** On a
  TLS error an HTTPS form submission was transparently retried over `http://`,
  re-sending the form fields (possibly credentials) in the clear before the
  downgrade notice was shown — an active MITM could force a TLS error to strip
  encryption and capture the body. A POST is now refused on a TLS error;
  idempotent GET page loads still downgrade with a warning banner.
- **Web-browser page text is sanitized before it reaches the terminal.** Remote
  content (HTML, `text/plain`, and gopher) now passes through the same
  `sanitize_for_terminal` filter as the AI-chat path, stripping ANSI/CSI/OSC
  escape sequences a malicious or MITM'd page could use to manipulate a retro
  terminal. Link-number sentinels are preserved. Coverage also includes the
  page URL (a gopher selector can carry escapes into the status line) and all
  rendered form text — form/field labels, Select option text (sanitized in
  place), and displayed field values (sanitized at display time so the
  submitted value stays byte-exact) — which the form view/edit UI prints.
- **An unreadable existing config is no longer reset to insecure defaults.** If
  `egateway.conf` is present but can't be read (non-UTF-8, corruption, or a
  permission/I/O error), the gateway now refuses to start rather than
  overwriting it with `security_enabled = false` / password `changeme`. Config
  and dial-map saves also `fsync` before the atomic rename, so a crash or power
  loss between write and rename can't publish a truncated file (which would
  then trip the new fail-loud guard on the next start). An existing file that
  parses to *no* recognized settings (empty, whitespace-only, or comments-only
  — e.g. an external truncation to zero bytes) is likewise treated as
  unreadable rather than as "all defaults," so it can't silently downgrade the
  gateway either.
- **Startup warns on the wide-open combination.** `disable_ip_safety = true`
  together with `security_enabled = false` — an unauthenticated gateway
  reachable from any IP — now emits a startup warning, matching the guard the
  GUI/telnet toggle popups already apply.
- **ZMODEM: bound control-frame floods that make no forward progress.** The
  45 s negotiation deadline and per-read timeout bound *silence*, not a peer
  that streams valid control frames. The receiver now bounds progressless
  control frames (ZRQINIT/ZSINIT/ZFREECNT/ZSTDERR/unknown), reset by a real
  ZFILE, and the sender bounds stale-ZRINIT drains per ZFILE attempt, so a
  chatty-but-progressless peer can no longer keep a session alive indefinitely.
- **Telnet: the session subnegotiation reader is now slowloris-bounded.**
  `read_subneg_payload` bounds each read with `SB_DRAIN_TIMEOUT`, so a peer that
  sends `IAC SB` then stalls without `IAC SE` can no longer pin the session and
  its `max_sessions` slot when `idle_timeout_secs = 0`. This matches the two
  gateway-path SB readers, which were already bounded.
- **Serial: the direct peer-dial ring is now shutdown/restart-aware.** While an
  `ATD <Port>@<ip>` to a local modem port was ringing unanswered, the caller's
  serial thread parked in a blocking wait and ignored shutdown/restart for up to
  the clamped S7 window. The ring now races a shutdown/restart poll (the same
  idiom the modem-port announcer uses), so a config restart or shutdown is
  responsive within ~100 ms.

## [0.6.3] - 2026-07-03

### Added
- **The desktop GUI remembers its window position and size.** The
  configuration window now reopens where you last left it — its outer position
  and inner size are saved (debounced) to `gui_window_geometry` in
  `egateway.conf` and restored on the next launch. It is auto-managed: there is
  no config-UI field for it, and an empty value means "use the default size and
  let the window manager place it." Works on X11/Windows/macOS; Wayland
  compositors don't expose a window's position, so it isn't remembered there.
- **Peer-dial: call another serial port directly.** With the new
  `allow_peer_dial` opt-in (default off; wired into telnet **Configuration > M >
  P**, web, and GUI), a modem-mode port can dial another port by address —
  `ATD <Port>@<IP>` (e.g. `ATD B@192.168.1.50`) — or select that port in the
  Serial Gateway menu, and bridge straight through to the device on it (the
  gateway equivalent of calling a friend's modem). A **modem-mode** target
  *rings* and answers per its own AT rules (`S0` auto-answer / manual `ATA`); a
  **console-mode** target connects directly. The connection is a transparent
  byte pipe, so a file-transfer protocol runs end to end between the two
  devices. Result codes follow ATX (`CONNECT`/`BUSY`/`NO ANSWER`/`NO CARRIER`).
  Works on the same gateway and, **over the master/slave relay, from a slave
  device to a port on its master** (`ATD <Port>@<master-ip>`): the slave relays
  the call and the master resolves the address to one of its own ports and
  rings/connects it (gated by the master's `master_accept_relays` +
  `allow_peer_dial`). Cross-gateway is symmetric: the master routes a peer
  address to **any** port a slave has registered — a slave's **console** port
  and its **modem** port (a slave modem port announces itself to the master and,
  when dialed, *rings* the attached device) — so `<Port>@<slave-ip>` reaches a
  slave's port from the master or, via the master as a crossbar, from another
  slave (device ↔ slave-A ↔ master ↔ slave-B ↔ device). Addressing is by IP, so
  gateways need distinct addresses (normal for separate machines). See README
  "Peer-Dial" and user manual §9.2.3.
- **Live relay status in the telnet Master/Slave screen.** A master now lists
  the remote console ports slaves have registered (so you can see connected
  slaves at a glance); a slave shows each console port's link state to its
  master (`down`/`connecting`/`registered`/`bridging`) — relay connectivity is
  now visible without reading the logs.
- **Relay channel handshake / protocol version.** The master now writes a small
  hello (`EGR` magic + a protocol-version byte) as the first bytes on every
  accepted master/slave relay or console-registration channel; the slave
  validates it before using the channel. A master/slave version skew now fails
  cleanly with an "upgrade the older gateway" message instead of desyncing, and
  a slave pointed at a master that is declining relays (`standalone`,
  `master_accept_relays=false`, or at capacity) now detects the refusal — the
  absence of the hello — and backs off with a clear message, instead of
  mistaking the refused-but-open channel for a live registration and idling.
- **Optional hardware carrier (DCD) signalling.** New per-port opt-in
  `serial_a_drive_carrier` / `serial_b_drive_carrier` (default `false`; also a
  checkbox in the GUI/web config and the **C** key in the telnet per-port modem
  menu). When enabled, the modem emulator drives **DTR** as a carrier proxy
  (a PC/USB-serial adapter is a DTE and can't drive a DCD *output*, so you cross
  DTR→DCD in a null-modem cable, as tcpser does), following `AT&C`: `&C0` forces
  it always asserted while the port is open, `&C1` (default) asserts on
  `CONNECT` and drops on `NO CARRIER` / `ATH` / hangup / relay-link-loss (so a
  slave-attached machine sees loss-of-carrier in hardware too). **When off, the
  gateway makes zero modem-control-line calls**, so ports without DCD wiring are
  byte-for-byte unaffected. Modem mode only.
- **Master/Slave serial extender (optional).** A gateway set to
  `gateway_role = slave` extends its serial ports to a `master` gateway over
  the master's existing SSH port; the serial device reaches the master's menu,
  file transfer, and dial-out as if attached to the master, and **files always
  land on the master**. Default `gateway_role = standalone` leaves the feature
  entirely inert. Modem-mode ports relay on connect (the slave resolves its
  *local* dial map; the master dials onward — "resolve local, dial central");
  console-mode ports register with the master and appear in the master's Serial
  Gateway picker (local ports + registered remote ports). New config keys
  (telnet/web/GUI): `gateway_role`, `master_accept_relays`, `slave_master_host`,
  `slave_master_port`, `slave_master_username`, `slave_master_password`,
  `relay_transport` (only `ssh` implemented). The slave authenticates with the
  master's unified username/password and pins the master's SSH host key (TOFU,
  in `gateway_hosts`); relay connections are gated by `master_accept_relays` and
  count against the session cap. The slave's main menu shows a SLAVE-mode notice
  with the master address, and reconnects automatically if the link drops.
- **Serial sessions can now receive administrative broadcasts.** A process-global
  broadcast channel (`serial::broadcast_to_serial`) fans a message out to every
  open serial port, delivered at the **command prompt only** — an in-call
  (online) serial session, which may be carrying a binary file transfer, drains
  its queued messages when it next returns to command mode (`+++`, hangup, or
  call end) so a notice can never corrupt a transfer. This is the serial-side
  counterpart to the telnet/SSH/relay `broadcast_to_sessions` list, completing
  broadcast coverage across all connection types. The shutdown "Goodbye" keeps
  its own reliable shutdown-flag write (which fires even mid-online) and is not
  routed through this channel. Modem mode only. (Extension point: no production
  broadcast is routed to it yet — the first admin-notice feature plugs in here.)

### Fixed
- **Serial `AT&C` now updates the hardware carrier (DCD/DTR) line immediately.**
  With `serial_X_drive_carrier` enabled, changing `AT&C` at the command prompt
  used to take effect only at the next connect/hangup; it now re-applies the
  DCD line right away — `&C0` asserts DTR (carrier forced on regardless of call
  state) and `&C1` restores follow-the-carrier — matching the documented
  contract and the existing `ATZ`/`AT&F` behavior. Found during on-hardware DCD
  validation (DTR→DCD crossover).
- **GUI console started as a boot service now waits for the window manager.**
  When launched as a boot-time systemd service, the console window could come
  up undecorated (no title bar / minimize / close) or with its title bar tucked
  under the desktop panel, because it opened as soon as the X server accepted a
  connection — before the window manager had taken over decoration and
  placement. The display-wait now also waits (bounded, X11-only) for an EWMH
  window manager (`_NET_SUPPORTING_WM_CHECK` on the root window) before opening
  the window. Degrades safely: no `xprop`, a bare X server, or a non-EWMH WM
  falls through after a short cap and opens anyway, and the server is never
  delayed (only the window waits). Non-X11 targets (Windows, macOS, headless,
  pure-Wayland) are unaffected — the wait returns immediately without `DISPLAY`.
- **Serial Gateway menu shows peer-dial addresses without spaces around `@`.**
  Remote (slave) port entries are now displayed as `<Port>@<ip>` — exactly the
  string you type to dial them (`ATDT <Port>@<ip>`). The previous spaced form
  (`<Port> @ <ip>`) invited mistyped dial strings with embedded spaces. The
  remote-bridge screen title and the master's registered-ports status list were
  unspaced to match.
- **Master/Slave configuration now guides the operator by role.** Across the
  telnet menu, web, and desktop GUI, fields that don't apply to the selected
  role are greyed out / disabled: *accept relays* is editable only for a
  **Master** (and now defaults **on** when you switch to Master, since a master
  with it off can't accept slaves), while the master host / port / user / pass
  are editable only for a **Slave**. Switching to Master while the SSH server is
  off now surfaces a warning (a popup in web/GUI, a dedicated screen in telnet)
  explaining that slaves connect over SSH — it points you at the setting but
  never toggles SSH for you.
- **Peer-dial now reminds you about local echo.** A peer-dial connection is a
  transparent link with no host echoing keystrokes back, so the Serial Gateway
  picker shows a "enable local echo to see typing" tip, and the README /
  user-manual peer-dial sections explain that each terminal needs local echo
  (half-duplex) — and that `ATE` does not affect the online data path.
- **Shutdown "Goodbye" now reaches every session, not just when telnet is
  enabled.** The shutdown broadcast used to live inside the telnet accept loop,
  so an SSH-only deployment (`telnet_enabled = false`) tore SSH and relay
  sessions down with no notice. It is now a transport-neutral broadcast invoked
  centrally at shutdown, so telnet, SSH, and master/slave relay sessions all
  receive it for any combination of enabled servers (serial ports already emit
  their own notice). The mechanism is reusable for future all-session messages.
- **File transfers over telnet no longer apply NVT CR-NUL stuffing**, which
  corrupted binary transfers through telnet↔serial bridges (e.g. tcpser) and
  telnet-aware WiFi modems that don't symmetrically un-stuff. The shared
  transfer I/O layer (`tnio`, used by XMODEM/YMODEM/ZMODEM/Kermit/Punter) now
  escapes only IAC (`0xFF` → `IAC IAC`) and passes every other byte —
  including CR (`0x0D`) — through literally, matching RFC 856 binary-transmission
  semantics that 8-bit file transfer requires. CR-NUL stuffing (RFC 854 §2.2)
  is a text-mode rule and was inserting/deleting `0x00` bytes around `0x0D`,
  which manifested as endless mid-transfer checksum failures and a hung peer
  (a Commodore Punter sender, whose `S/B` wait loops are unbounded, would
  strand). Validated against the genuine CCGMS Punter reference
  (`ccgmsterm/test/punter.c`) in both directions, including through a
  telnet-bridge emulation. IAC escaping (the **I** toggle) is unchanged.
- **GUI: external changes to the Kermit idle-timeout are no longer reverted on
  save.** `kermit_idle_timeout` was rendered and saved in the desktop config
  editor but missing from its refresh-from-global and dirty-detection paths, so
  a value changed via the web/telnet UI while the GUI was open could be silently
  overwritten by the GUI's stale field on the next Save.
- **Serial modem mode now auto-reconnects when the device behind the port
  disappears** (e.g. a `socat`/USB-serial bridge that exits when its attached
  terminal closes). Command-mode previously hit a hard I/O error and re-looped,
  spamming the error ~twice/second forever with no recovery; it now logs the
  outage once, backs off 1 s, and reopens the port automatically when the device
  returns — matching console mode.
- **`ATDT` to a hostname now tries every resolved address.** Dialing resolved
  via `to_socket_addrs()` but only attempted the first address, so a host whose
  DNS returns an unreachable IPv6 record first could fail with a silent
  `NO CARRIER` even when a working IPv4 address followed. It now attempts each
  resolved address until one connects, and logs the failure reason instead of
  failing silently.
- **Config save failures are now surfaced.** `write_config_file`/`save_config`
  return a `Result`; the explicit-save paths (desktop GUI Save buttons, telnet
  reset-to-defaults) report a failure instead of always logging success.
- **Hand-edited `serial_*_parity` / `serial_*_flowcontrol` values are honored.**
  Both are now normalized (trim + lowercase) on read and apply, consistent with
  `mode`, so e.g. `serial_a_parity = Even` no longer silently reverts.
- **Config values round-trip without whitespace drift.** `sanitize_value` now
  trims surrounding whitespace (the reader already trimmed), and the dialup
  number/host are sanitized on save so an embedded newline can't corrupt
  `dialup.conf` framing.
- **GUI waits for the X display before opening the console window**, fixing the
  headless drop when the gateway is started as a boot-time service before the
  desktop session's X auth cookie is ready. The wait is adaptive (no delay on a
  normal manual launch) and degrades safely when there is no display.
- **Kermit's async server/receive paths no longer stall a runtime worker** —
  blocking `std::fs` calls moved to `tokio::fs` and the directory listing
  offloaded via `spawn_blocking`.

### Security
- **SSH: warn when a pre-existing host/client private key is group- or
  world-readable.** New keys are written `0600`; a key restored from a backup or
  created by an older build could be more permissive. The gateway now logs a
  `chmod 600` recommendation on load (warn-only — it does not refuse the key,
  matching the trusted-LAN threat model).
- **ZMODEM: bound consecutive empty data subpackets** (`MAX_EMPTY_SUBPACKETS`)
  so a peer can't tar-pit the receive loop with CRC-valid zero-progress
  subpackets.
- **Telnet: bound in-subnegotiation reads** (`SB_DRAIN_TIMEOUT`) so a peer that
  opens an `IAC SB` and then stalls can't pin the reader (slowloris); the outer
  idle wait is unchanged.

## [0.6.2] - 2026-06-19

### Added
- **Session cap and idle timeout are now editable from the telnet Server
  Configuration menu** (the `C` and `D` keys), matching the desktop GUI and the
  web configuration page that already exposed `max_sessions` /
  `idle_timeout_secs` — completing three-UI parity for both settings. Idle
  timeout accepts `0` to disable the idle disconnect. The screen's detected-IP
  hint list is now capped so the new row keeps the PETSCII menu within its
  22-row budget even on a multi-homed host (it previously overflowed at three or
  more private addresses).

### Security
- **Fixed an SSRF-guard bypass for IPv6-literal URLs in the text-mode web browser.** `guard_public_url` classified IP literals with `IpAddr::parse`, but `url::Url::host_str()` returns IPv6 literals *bracketed* (e.g. `[::1]`), which fails that parse and fell through to the resolver path — allowing `http://[::1]/`, `http://[::ffff:127.0.0.1]/`, and the like to reach loopback / link-local / internal IPv6 services (initial request and every redirect hop). The guard now strips the brackets before classifying, blocking the entire internal IPv6 space. Regression test added. IPv4 literals and DNS names were already handled correctly.
- **SSH: an unauthenticated connection no longer consumes a session slot.** `new_client` incremented the session counter for every inbound TCP connection, before authentication, so a peer that opened many transport handshakes and stalled could exhaust `max_sessions` and lock out real users. The slot is now claimed only on a successful login (atomic `fetch_add` + rollback, mirroring the telnet accept loop) and released only if it was claimed — and the cap is now exactly `max_sessions` (was off-by-one, `max_sessions + 1`).
- **Web config: `POST /save` now enforces a same-origin check (CSRF defense-in-depth).** A request whose `Origin`/`Referer` doesn't match our `Host` is rejected with 403, blocking a malicious page from riding the operator's cached Basic-auth credentials to rewrite config (including disabling auth). Requests with neither header (non-browser clients such as `curl`, which can't be a CSRF vector) are still allowed; Basic auth continues to gate them. Lenient-on-absent by design for the trusted-LAN threat model.
- **Kermit server: defense-in-depth subdir re-validation on save.** Both the in-session receiver and the standalone (auth-bypassing) Kermit listener now re-check `rx.subdir` with `is_safe_relative_subdir` before joining it to the transfer dir. No live traversal existed (subdir is only set after that same check inside the Kermit module), but re-validating at the save site closes the door on any future producer-side bypass — the same belt-and-suspenders rationale as the existing filename re-check.

### Fixed
- **Serial console bridge: a stalled telnet peer can no longer wedge server shutdown / port restart.** The dedicated serial-reader thread used an unbounded `blocking_send` onto a bounded channel; when a bridged peer stopped reading and the channel filled, the thread parked past its shutdown/restart checks. It now polls with `try_send` + a short sleep, bailing on shutdown/restart or when the async pump drops its receiver.
- **Serial modem online mode (TCP): a remote host that stops reading no longer blocks shutdown.** `online_mode_tcp` set only a read timeout, so a full remote receive window parked `write_all` indefinitely with the loop's shutdown/restart checks unreachable. A 5 s write timeout is now set (matching the duplex path); an expiry drops carrier (NO CARRIER).
- **XMODEM/YMODEM: YMODEM block 0 is now always validated as CRC-16.** If block 0 took enough retries to cross the negotiation's CRC→checksum fallback point, the block-0 body (and then the data phase) could be misread as a 1-byte checksum, NAK-looping a CRC-only YMODEM sender to exhaustion. The block-0 read and the post-block-0 data phase are now pinned to CRC-16.
- **Logging survives a poisoned lock.** `logger` now recovers a poisoned mutex (`into_inner`) instead of silently dropping the line — matching `config.rs` / `gui.rs`, and most valuable exactly when a thread has just panicked.
- **Kermit streaming: a sequence-aliased NAK now aborts cleanly instead of silently corrupting the file.** In streaming mode the whole file sits in the sender's outstanding-packet set with wrapping (mod-64) sequence numbers, so a file larger than ~64 chunks aliases each seq across many packets. On a genuine mid-stream NAK/loss the sender matched the NAKed seq to the *first* (oldest) outstanding packet sharing it and retransmitted that stale packet; the receiver appends D-packets by sequence with no position field, so it landed the wrong data at the wrong offset. This was benign on lossless TCP/SSH (streaming's intended transport, where NAKs don't occur) and only reachable on an unreliable link such as a serial bridge. An unresolvable NAK now aborts with an actionable error ("disable `kermit_streaming` for this peer"); the timeout-driven retransmit path skips aliased seqs for the same reason. The reliable-transport happy path is unchanged.
- **ZMODEM: `ZFERR` (0x0C) is now handled instead of ignored.** A sender's file read/write-error frame aborts the receive cleanly with an informative error rather than falling through to the ignore arm and waiting out a frame timeout. Every Forsberg 1988 frame is now handled.
- **Text-mode web browser: fixed a remote-triggerable panic on Back.** Returning to a previous page whose re-fetched content is shorter than the saved scroll position could index past the page and panic the session task. The scroll position is now clamped on restore and again defensively at render time.

### Documentation
- **Documented ZMODEM `ZCOMMAND` (frame 0x12) as the one optional spec frame deliberately not implemented** — it is recognized but always refused (non-zero `ZCOMPL`), since arbitrary `/bin/sh -c` execution on a shared, long-lived host is an unacceptable default; use SSH for shell access. Noted in the user manual and the ZMODEM web reference.
- Documented previously-undocumented config keys: `web_enabled`, `web_port`, `gateway_debug`, and `ssh_gateway_auth` in the README config reference, and `punter_max_bad_rounds` / `punter_hangup_on_failure` in the user manual. Added the now-handled `ZFERR` frame to the ZMODEM web reference, and corrected the SSH reference's `auth_password` lifecycle description to match the new claim-slot-on-successful-login behavior.
- README config-reference completeness pass: the "All options" `egateway.conf` sample now lists `disable_ip_safety` and the per-port `serial_a_petscii_translate` / `serial_b_petscii_translate` keys (all three are written by the config saver), the telnet Server-Configuration menu walkthrough documents the new session-cap / idle-timeout keys, and the Other Settings list now includes the gateway debug-trace toggle.

## [0.6.1] - 2026-06-06

### Added
- **Raspberry Pi 4+ (aarch64 Linux) build** — releases now ship an
  `Ethernet_Gateway-aarch64.AppImage` alongside the existing
  x86_64 Linux / Windows / macOS artifacts, built on a native arm64
  runner. Two ARM-only desktop-GUI fixes make it run on the Pi's
  VideoCore/V3D GPU: the wgpu device now requests exactly the limits
  the adapter advertises (so startup no longer aborts with
  "Limit 'max_color_attachments' value 8 is better than allowed 4" or
  the equivalent for other limits), and the GUI prefers the OpenGL ES
  backend instead of the Pi's incomplete Vulkan driver (which panicked
  with "Requested feature is not available on this device").
  `WGPU_BACKEND` still overrides. Other platforms are unaffected.
- **Punter (C1) file-transfer protocol** — the protocol CCGMS /
  Novaterm / StrikeTerm speak natively on Commodore BBSes, added
  alongside XMODEM/YMODEM/ZMODEM/Kermit. Single-file C1 with the full
  two-phase (file-type then data) handshake, both block checksums
  (16-bit additive + cyclic), the "size of next block" framing, and
  the three-`S/B` end-off real C1 endpoints expect. Selectable in the
  telnet upload/download protocol pickers; the outbound PRG/SEQ file
  type is auto-detected from the filename. New `punter_*` tunables
  (block size, timeouts, retries) are editable from the telnet File
  Transfer settings menu, the web configuration page, and the desktop
  GUI, and persist to `egateway.conf`. The send/receive entry points
  take an open stream so a future Multi-Punter (MPP) batch wrapper can
  layer on without touching the wire code.
- **Serial modem `AT+PETSCII=n` command** — toggles PETSCII⇄ASCII
  translation on direct-TCP dials (`AT+PETSCII=1` on, `AT+PETSCII=0`
  off) so a Commodore 64/PET dialing `ATDT host:port` sees readable
  text instead of raw ASCII. Set-only, in the ITU-T V.250 `+`
  extension namespace (`&P` is the pulse-dial make/break ratio on real
  Hayes modems, so it is intentionally left alone). `AT+PETSCII=1`
  persists the setting immediately; `AT&V` reports it as `+PETSCII:n`.
- **PETSCII translation is now editable from every configuration
  surface** — the per-port modem screen in the telnet/serial-console
  menu, the web configuration page, and the desktop GUI — in addition
  to the AT command. It is a per-serial-port setting saved to
  `egateway.conf`.
- Serial: inbound PETSCII punctuation normalizer, and the C64 PETSCII
  DEL key (0x14, INST/DEL) is accepted as a command-line backspace
  when PETSCII translation is active. `+++` escape sequences are
  traced when the gateway debug trace is on.
- **Persisted `gateway_debug` byte-trace flag**, toggleable from the
  GUI/web General frame and the telnet Other Settings / Serial
  Configuration menus. Read fresh per gateway session (no restart
  needed); `EGATEWAY_GATEWAY_DEBUG` still forces it on. The trace
  timestamps each input byte, emits a one-shot `[gw-diag]` terminal
  diagnostic per session (detected type and how it was decided, the
  announced TERMINAL-TYPE, the color decision, advertised telnet
  options, NAWS window size, and — for serial callers — the port's baud
  and PETSCII-translate state, the most common cause of missing ANSI
  color on a serial line), and logs every AT command the modem emulator
  runs alongside a plain-English description of its effect.
- **Web protocol reference pages** served by the configuration web
  server — per-protocol references (XMODEM, YMODEM, ZMODEM, Kermit, the
  Hayes AT command set, and telnet), each documenting that protocol's
  retry/recovery behavior, plus character-set and ANSI escape-sequence
  references, reachable from a new References nav entry.
- **Kermit resume and locking-shift settings are now editable** from
  the telnet Kermit settings menu, the web configuration page, and the
  desktop GUI (previously `egateway.conf`-only).
- **`punter_hangup_on_failure`** — optional drop-carrier-on-give-up for
  Punter, editable from the telnet / web / GUI Punter settings. Because
  C1 has no in-band abort, a give-up otherwise leaves the C64 hung;
  enabling this drops carrier so it sees loss-of-carrier instead.
- **Cooperative TTYPE/NAWS negotiation is now toggleable from the telnet
  session's Gateway Configuration menu** (the `C` key), matching the web
  configuration page and desktop GUI that already exposed
  `telnet_gateway_negotiate`. The menu now shows its on/off state next to
  the telnet-mode and SSH-auth rows.

### Fixed
- AI chat: a follow-up question that merely starts with a menu command
  letter (e.g. "Quantum…") is no longer swallowed by the answer-screen
  navigation. A lone command letter still navigates; any longer line
  is sent to the model.
- **Transfer retry/recovery brought to strict spec.** XMODEM/YMODEM now
  NAK on a data-phase inter-block timeout (re-prompting the sender) and
  cancel with CAN×3 on a non-duplicate block-sequence error instead of
  NAK-looping; ZMODEM routes every data-phase error through one bounded
  counter that re-sends ZRPOS and resets on progress (no infinite ZRPOS
  loop on a permanently-corrupt stream); Kermit emits an Error packet
  when it gives up so the peer is told rather than left waiting.
- **Punter no longer strands a peer on a failed transfer.** A cancel /
  restart from the C64 side is tolerated (longer pre-transfer input
  drain), and corrupt-block recovery is bounded by its own larger round
  cap rather than quitting early and leaving the peer hung.
- **Plain XMODEM now verifies EOT (Forsberg NAK-first-EOT).** The
  receiver NAKs the first EOT and accepts end-of-file only on a resent,
  confirming EOT, so a stray `0x04` from UART line noise in the
  inter-block gap can no longer be mistaken for end-of-file and silently
  truncate an upload to a C64 / CP/M / RC2014 peer. The duplicate-block
  re-arm logic also keeps a non-standard "resend last block on NAK"
  sender from looping. YMODEM keeps immediate-ACK on EOT — its block-0
  size field and end-of-batch handshake already detect a short file.
- **Serial AT parsing hardened.** A command-mode byte ≥ `0x80` (PETSCII
  line noise, or a C64 in lower/upper-case mode sending shifted letters)
  no longer panics the tokenizer and kills that port's modem thread:
  `parse_at_command` returns `ERROR` on non-ASCII input and high bytes
  are filtered at the command-buffer inputs. CR+LF / LF+CR pairs collapse
  to a single terminator so a CRLF terminal no longer runs a spurious
  empty command, and the ring-wait loop honors a per-port restart.
- **Web configuration server lockout / POST hardening.** Credential-less
  requests — the first half of an HTTP Basic challenge plus the
  subresource probes that repeat it — no longer count toward the shared
  per-IP brute-force lockout (only a present-but-wrong credential does),
  so ordinary page loads can't lock out a first-time user. A malformed
  `POST /save` body (non-UTF-8 or zero-length) is now refused instead of
  writing an all-`false` field set that silently disabled
  telnet / SSH / web / security in one shot.

### Changed
- Removed the duplicate Port A/B status banner from the main
  configuration menu — per-port mode is already shown under Serial
  Configuration.
- **Punter bad-block cap decoupled** — `punter_max_bad_rounds` (default
  30) bounds consecutive corrupt-block resend rounds separately from
  `punter_max_retries`, since a real C64 peer never caps resends and a
  low shared cap made the gateway give up first and strand it.

### Security
- **Updated `russh` 0.60.2 → 0.60.3** to clear two high-severity
  (CVSS 7.5) allocation-DoS advisories in the SSH stack:
  RUSTSEC-2026-0154 (`russh` unbounded 32-bit allocation) and
  RUSTSEC-2026-0153 (`russh-cryptovec` unchecked `CryptoVec`
  allocation/growth). A malicious SSH client could otherwise drive
  unbounded memory allocation on the SSH listener.
- **Closed a web-browser POST-redirect SSRF.** The text browser's
  form-submit path used the HTTP client's automatic redirect, so a
  public form action that 30x-redirected to an internal address
  (loopback, link-local metadata, or LAN) was dialed before the SSRF
  guard ran — the final-URL check blocked only rendering, not the
  connection. POST redirects now follow through the same fully-guarded
  fetch path as GET, so the connection itself is refused.

## [0.6.0] - 2026-05-24

### Added

#### Configuration web server
- **Optional HTTP listener** that renders the same settings page the
  desktop GUI does, in a browser.  Off by default; toggle in the GUI
  Server frame (new "Web Server" row between Telnet and Kermit) or
  the telnet `Configuration > Server Configuration` menu's
  `W` / `B` keys.  Port defaults to 8080.
- **Hand-rolled HTTP/1.1 on tokio** (no new dependencies) implementing
  `GET /` (settings page), `GET /logo.png` (the same logo the GUI
  uses), `GET /logs` (2-second polled log tail), `GET /serial-ports`
  (live device enumeration for the dropdown refresh), and
  `POST /save` (config persist + optional restart).
- **Per-frame Save buttons** matching the GUI's three behaviors:
  Server's *Save and Restart* (full server restart cycles through
  `main.rs` exactly the way the GUI does), Serial's *Save* (just
  reloads serial managers via `serial::restart_all_serial`), and the
  plain *Save* on every other frame (persist only).  Unknown action
  values fall back to plain Save so a hand-crafted POST with a typo
  can't accidentally restart the server.
- **POST → 303 See Other → GET** pattern: the save handler redirects
  to `/?notice=Configuration%20saved.` so a browser reload after
  submit doesn't resubmit the form.  Client-side
  `history.replaceState` strips the `?notice=` query right after
  render so the banner appears once per save instead of persisting
  across refreshes.
- **Serial-port dropdown + refresh button** populated server-side
  from `serialport::available_ports()` (the same source the GUI
  ComboBox uses); a small ↻ button next to each port re-scans via
  `GET /serial-ports` and rewrites both selects' options in-place
  without a full page reload.  Operator's selection is preserved
  across refreshes, and a saved port that isn't currently detected
  stays visible with a `(saved)` suffix.
- **CSS Grid Server-frame layout** so the two `Port:` colons in each
  column line up across rows; per-port inputs sized to 6 chars (any
  valid TCP port fits) so the More button fits on row 1 alongside
  Telnet + Web Server.
- **JS modal popups for the More views**, plus inline confirmation
  dialogs that warn before disabling the web server or changing the
  web port — both actions break the operator's current connection.
- **Connection-breaking notice** included in the post-save banner
  when the operator's just-confirmed change will sever the browser
  session (e.g. "Web server port changed to 9090. Reconnect at the
  new port.").

#### Web auth and lockout
- **HTTP Basic Auth** gated on the same `security_enabled` flag that
  guards telnet.  Uses the project's existing length-leak-resistant
  `constant_time_eq` from `telnet.rs`.
- **Shared brute-force lockout map** with telnet and SSH.  Three
  failures across any of the three protocols trip a 5-minute IP ban
  (the same `LockoutMap` the telnet listener uses); failed web
  attempts respond with `429 Too Many Requests` + `Retry-After: 300`
  once the threshold is crossed.  The 429 fires *before* the auth
  check on every subsequent request, so a banned IP can't keep us
  busy parsing malformed POSTs either.
- **Same IP-safety allowlist as telnet**: when login is not required
  and `disable_ip_safety` is off, only private / loopback /
  link-local source IPs are accepted (and `*.*.*.1` gateway
  addresses are rejected).

#### Web defense-in-depth
- 30-second read timeout on `read_request` to stop slow-loris clients
  from parking a tokio task indefinitely.
- `MAX_INFLIGHT = 16` concurrent connections with a `Drop`-guarded
  slot release; excess connections get a `503 Service Unavailable` +
  `Retry-After: 5` rather than being parked behind the read timeout.
- 16 KB cap on request headers, 64 KB cap on POST body — bounded so
  a hostile peer can't drive the per-connection buffer to OOM.
- UTF-8 round-trip safe: `url_decode` accumulates percent-decoded
  bytes into a `Vec<u8>` then runs `from_utf8_lossy`, so values like
  `weather_zip = 日本語` survive the form → config-file → form
  cycle without corruption.

### Changed

#### Unified telnet / SSH / web credentials
- **One username / password pair** now covers the telnet menu, the
  SSH server, and the web configuration UI.  The old per-protocol
  `ssh_username` / `ssh_password` config keys are gone.  Defaults
  unchanged at `admin` / `changeme`.
- **One-time migration**: if the operator's `egateway.conf` still has
  non-default `ssh_username` / `ssh_password` values *and* the
  unified `username` / `password` are still at the factory defaults,
  the legacy SSH values are adopted into the unified pair on load
  (with a `Note: migrating legacy ssh_username=…` log line).  Once
  the next save runs, the legacy keys disappear from the written
  file.  If both pairs were already customized, the unified pair
  wins (the legacy SSH values are silently dropped).
- **GUI Security frame** collapses from two rows (separate Telnet /
  SSH credential rows) to one `Login User / Pass` row + a spacer
  that keeps the frame the same height as the adjacent Server frame.
- **Telnet Security menu** drops the `S` (Set SSH username) /
  `W` (Set SSH password) items; the remaining `U` / `P` items now
  read `Set username` / `Set password` (no more "telnet"
  qualifier).  Status shows a single `Username:` / `Password:`
  pair instead of two.
- **Help screens** under `Configuration > Security` and
  `Configuration > Server Configuration` updated: the security
  help notes "One username/password covers telnet, SSH, and the
  web UI" and the server help describes the new `W` (Toggle Web) /
  `B` (Set Web port) keys.

#### GUI Server frame
- Fixed-width listener column slots so the two `Port:` colons line
  up between rows — the same colon-alignment the web frame gets
  from CSS Grid.  The earlier hand-tuned `add_space(16.0)` left the
  colons at different X positions because "Telnet" / "SSH" and
  "Web Server" / "Kermit Server" have different intrinsic widths.
- **More button moved up to row 1** (with Telnet + Web Server),
  mirroring the web layout.

#### GUI Serial Ports frame (web-side parity adjustments)
- Web Serial frame's header now carries both ports' Enabled
  checkboxes alongside per-port titles ("Serial Port A" / "Serial
  Port B"), matching the GUI's layout exactly.  Per-port rows are
  now `Port X: [select ▼] [↻] Baud: [...] [More...]` with the More
  button kept on the same line via a no-wrap row class.

#### Logger
- Added a parallel non-draining `snapshot(max)` API alongside the
  existing `drain()`.  The GUI keeps using `drain()` for its
  per-frame console accumulator; the web `/logs` endpoint polls
  `snapshot()` so the two views don't compete for log lines.

## [0.5.5] - 2026-05-10

### Added

#### Dual serial-port support
- **Two physically independent serial ports** — `Port A` and `Port B` —
  each with its own enabled flag, mode (modem emulator or telnet-serial
  console), device path, baud, AT/S-register state, and stored
  phone-number slots. The two ports run in separate manager threads,
  persist AT&W state separately, and host independent console-bridge
  slots, so the operator can run a Hayes modem on one wire and a
  telnet-serial bridge on the other (or any other mix) without
  cross-talk.
- **A/B picker submenus** — the `Configuration > M` entry is now
  *Serial Configuration* and opens a picker listing both ports' status;
  selecting a port drops into that port's settings. The main-menu
  *Serial Gateway* (G) likewise opens an A/B picker before bridging,
  showing both ports' status (ineligible ports are dimmed) so the user
  can see *why* a port isn't available.
- **Per-port mode toggle** moved from the Configuration menu to the
  per-port settings menu (T item).  Hidden from sessions that arrived
  over a serial port itself, since flipping that port to console mode
  would tear down the caller's own connection before they could
  acknowledge.
- **GUI Serial Port frame** redesigned: header row carries both ports'
  *Enabled* checkboxes and a shared *Save* button; one row per port
  beneath with a device-path dropdown, baud field, and per-port
  *More…* button into an advanced popup (mode, framing, flow, full
  Hayes AT state). Both popups are independent so settings can be
  compared side-by-side.

### Changed

- **Config schema split** into per-port keys: every former `serial_*`
  key is now `serial_a_*` or `serial_b_*`. Legacy single-port configs
  auto-migrate into Port A on first read; the next save rewrites the
  file in dual-port form. Existing single-port deployments upgrade
  transparently with Port B disabled by default.
- **Serial Gateway main-menu visibility** — now requires at least one
  port to be in console mode (so the menu can't dead-end at an empty
  picker).
- **Dialup mapping** stays a single shared `dialup.conf` consulted by
  both ports' modems — phone-number lookups are global, not per-port.
- **Documentation refreshed** end-to-end (`README.md`,
  `usermanual.html`, `index.html`) for the dual-port architecture,
  including config-key tables, GUI screenshots/descriptions, and the
  Console Mode walkthrough.
- **`ATI0` / `ATI3` identification strings** now advertise the modem as
  Hayes-compatible, matching the behavior callers (BBS dialers, vintage
  terminal software) expect from a Hayes ID query.

### Fixed

- **PETSCII width compliance** in the new pickers and per-port menu
  titles: replaced em-dashes with ASCII hyphens and switched the
  picker layout to two lines per port (role label + device/baud) so
  worst-case lines fit the 40-col PETSCII budget.
- **Stale help text** in `console_show_help` that told users to
  "Press T at the Configuration menu" — T moved into the per-port
  settings menu.

### Security

- **AI-chat output sanitization** — replies from the Groq API are now
  normalized (`\r\n`/`\r` → `\n`) and passed through a
  `sanitize_for_terminal` filter before display, stripping ANSI escape
  sequences, control bytes, lone CRs, and telnet IAC so a prompt-injected
  reply can't smuggle terminal-control payloads through the chat surface.
- **Auth-lockout map bounded** — `record_auth_failure` now sweeps entries
  past the lockout window on every call, so a long-running public-facing
  instance can no longer accumulate one entry per distinct attacker IP
  indefinitely.

## [0.5.4] - 2026-05-06

### Added

#### Serial Console Mode
- **Telnet-serial bridge** as a second role for the serial port,
  alongside the existing Hayes AT modem emulator. Selectable via the
  new `serial_mode` config key (`modem` / `console`). The existing
  `G  Serial Gateway` main-menu item now bridges the telnet/SSH session
  straight to the wire so an operator can drive a microcontroller,
  RS-232 device, or other serial console remotely.
- **Hot mode switch** — flipping `serial_mode` (from the GUI dropdown,
  the new `T  Toggle Modem/Console mode` entry on the Configuration
  menu, or `egateway.conf` directly) reconfigures the running serial
  thread within one manager-poll interval. No restart required. The
  menu toggle is refused for callers connected over the modem itself,
  since switching to console mode would tear down their own session
  before they could acknowledge — flip the mode from a telnet, SSH, or
  system-console session instead.

### Changed

- **Configuration menu** reorganized to surface the new mode toggle and
  to relabel `M  Modem Emulator` ↔ `M  Serial Console` based on
  current `serial_mode`. The new menu walkthrough is documented in
  user-manual §5.6.
- **Documentation pass**: §3.2 of the user manual gained 22 previously
  undocumented config keys (the full `kermit_*` family,
  `ssh_gateway_auth`, `disable_ip_safety`, `allow_atdt_kermit`,
  `kermit_server_enabled` / `_port`); `index.html` grew a Kermit
  subsection in the file-transfer config tables and added cross-links
  to `kermit.html` from each protocol-prompt step; the chapter-8 intro
  now correctly describes five protocols (the old "three related
  protocols" framing predated the ZMODEM and Kermit chapters).

### Fixed

#### Console bridge hardening
- **`run_console_bridge` could wedge** indefinitely when the telnet
  peer's TCP write buffer was full: the spawned async task's
  `duplex_write.write_all().await` would park with no wake-up source,
  stranding the manager thread until process restart. Bounded with a
  200 ms timeout then `abort()`.
- **Orphaned bridge requests** on serial-mode flip: a request that
  arrived in the slot just before `SERIAL_RESTART` fired could be
  silently abandoned because `console_manager_tick` returned without
  polling the slot, leaving the requester's `rx.await` blocked forever.
  Slot is now drained with `Err("Serial mode changed")` on every exit
  path.
- **TOCTOU between request-eligibility check and slot insert**:
  `request_console_bridge` now re-checks
  `check_console_bridge_eligible` under the slot lock so an operator
  flipping `serial_mode` (or disabling serial, or clearing the port
  path) and calling `restart_serial()` in the narrow window between
  the fast-path check and the slot insert can no longer leave a
  request stuck until shutdown.
- **Unbounded `session_to_port` channel** replaced with a bounded
  `tokio::sync::mpsc::channel(64)`; a flow-controlled wire (CTS-low,
  slow peer) plus a fast typist or paste can no longer balloon
  in-memory queue depth. The async-side `.send().await` now
  backpressures `duplex_read`, which backpressures the telnet peer.
- **Slot-cleanup duplication** removed from the `Err(_)` arm of
  `rx.await`; let `ConsoleSlotGuard`'s drop own slot teardown.

#### Serial mode switch responsiveness
- **Modem online loops** (`online_mode_tcp`, `online_mode_duplex`) now
  honor `SERIAL_RESTART` on every iteration; previously a mode flip
  could lag by one block-read interval before the loop noticed.

#### Menu UX & doc-vs-code drift
- **`G  Serial Gateway`** and **`T  Toggle Modem/Console mode`** are
  now hidden from sessions that arrived over the serial port itself.
  The handler-side rejections remain as defense in depth (a serial-side
  caller can still type the letter blind), but the menu no longer
  advertises items that always error.
- **Manual cross-references** to "chapter 9.10" corrected to "9.13"
  (Console Mode lives at 9.13; 9.10 is Chained Command Lines).
- **`AT&K1`** redescribed as Auto-detect (stored, no wire effect)
  instead of "Reserved"; the parser at `src/serial.rs:1140` accepts
  `&K1` and emits `FlowSet(1)`. Missing `&K1` row added to Appendix
  B.4.
- **`AT&F`** entry now notes that it drops the active connection,
  matching the `AtResult::Reset` return.
- **Bare `kermit` alias** for `ATDT KERMIT` documented alongside the
  existing `kermit-server` / `kermit server` aliases.

## [0.5.3] - 2026-05-03

### Added

#### Kermit server expansion
- **Standalone TCP listener** for Kermit server mode on its own port
  (default `2424`, configurable via `kermit_server_port` and
  `kermit_server_enabled`). Lets a peer connect directly to a server-mode
  endpoint without going through the telnet menu — the way real
  `kermit -j host` expects to talk to a remote server.
- **`ATDT KERMIT` dial shortcut** (and aliases `ATDT kermit-server` /
  `ATDT kermit server`) drops a serial-modem caller straight into Kermit
  server mode, indistinguishable on the wire from a real `kermit -j host`
  left in `server` mode. Off by default; enabled via the new
  `allow_atdt_kermit` config flag — it bypasses the telnet menu's auth
  gate, so the toggle is gated behind a security-warning modal in both
  the GUI and the telnet menu.
- **Direct Kermit-server entry** over telnet/SSH — connecting to the
  gateway's Kermit listener drops straight into server-mode dispatch
  with no menu.
- **Additional Kermit server commands**: `remote space`,
  `remote kermit version`, plus full `remote cwd` semantics (subdir-aware
  uploads, `cdup` via bare `..`, refusal of non-existent targets), and
  `remote dir` listing fixes.
- **`AT` command chaining** in the Hayes modem emulator (e.g. `ATE0V1Q0`
  parsed as a single line).

#### Network safety toggles
- **`disable_ip_safety` config flag** — when `security_enabled` is false,
  telnet normally rejects non-private and `*.*.*.1` source IPs. This
  flag opts out of the allowlist. Toggleable from the GUI Security frame
  and the telnet Server Configuration menu, both gated behind a
  security-warning confirmation. Read per connection so changes take
  effect immediately without a restart.
- **`kermit_idle_timeout` config key** (default 300 s, `0` disables).
  Split out from `kermit_negotiation_timeout` so a long-running C-Kermit
  session that idles for hours can suppress the default disconnect.
  Surfaced in the GUI Kermit panel and the telnet Kermit settings menu.

### Changed

- **Kermit settings menu split** into Status and Settings pages,
  navigable via `M`/`V`, so each fits the 22-row × 40-col PETSCII
  budget.
- **Server Configuration menu** combined `I` and `R` into one row to
  keep the PETSCII budget at N=3 detected IPs.
- **GUI logo** swapped from the 1024×512 source (downscaled at runtime)
  to a pre-sized 366×183 asset for a 1:1 blit at standard DPI;
  eliminates the faint mauve cast on dark-blue gradients we previously
  worked around with `mipmap_mode: None`.
- **`russh` updated** 0.60.0 → 0.60.2; RustCrypto transitive deps
  realigned to the versions russh 0.60.2 tests against.
- **Private-file writes** (SSH host key, outgoing client key,
  `egateway.conf`, `dialup.conf`) now use `OpenOptions::create_new` +
  `mode(0o600)` from inception rather than create-then-chmod, closing
  the brief 0o644 window between the two calls. Per-process atomic
  counter applied uniformly so two threads can't clobber each other's
  tmp file.

### Fixed

#### Kermit vintage-receiver interop (AnzioWin canary)
- **Vintage-receiver fallback**: `kermit_send` now retries with classic
  80-byte / CHKT=1 / window=1 capabilities if the extended Send-Init
  exhausts all retries with no response. Vintage Kermits (AnzioWin,
  original CP/M Kermit, MS-DOS Kermit pre-CAPAS, embedded targets)
  always handle classic; modern peers respond on attempt 1 and pay no
  cost.
- **Send-Init ACK** is now built from the negotiated session
  intersection rather than our proposal, so quirky vintage receivers no
  longer see CAPAS bytes / extension fields they didn't propose.
- **Stale ACKs** (peer ACKing an older seq than we asked for) are now
  discarded instead of aborting the transfer. AnzioWin re-emits ACKs
  from prior packets after we've moved on.
- **YMODEM end-of-batch** handshake is now bounded to ~6 s worst case
  (3 s × 2 attempts) instead of the prior 200 s default. Fixes AnzioWin
  (and any receiver that sends post-EOT `'C'` then drops to terminal
  mode) showing the IAC-doubled `0xFF` complement byte rendered as `ÿ`
  on every retry.

#### Kermit server correctness
- **Files save inline** per S-dispatch instead of buffering until
  session end — closes the data-loss window where a peer disconnect or
  idle timeout would strand received files in memory.
- **F-packet** now refuses sender filenames that won't survive
  `validate_filename` ([A-Za-z0-9._-]) before consuming any D-packet
  body. Was silently dropping the whole upload at save time, so a
  literal-mode `put My File.txt` looked successful on the wire but
  vanished from disk.
- **`kermit_resume_partial`** now actually writes back to disk; the
  saver atomic-replaces via tmp+rename when a partial was pre-loaded.
  Previously the create-new save hit `AlreadyExists`, dropped the
  merged data, and left the partial untouched.
- **GET filename round-trip with `#` (default QCTL)**: the server's
  R-handler and `kermit_client_get` now control-quote per spec §6.4.
  Real C-Kermit's GET sender encodes via `encstr` (ckcfn2.c:2474), so a
  filename containing `#` arrived doubled — our server then looked up
  `temp##1.bin` on disk while the file actually saved as `temp#1.bin`.
- **`remote cwd <path>` (G-C)** field-decodes the argument per spec
  §6.7 (a `tochar(N)` length byte + N path bytes); short paths whose
  length byte lands on `tochar(3)='#'` are now control-quoted on the
  wire.
- **Uploads honor `remote cd`**: telnet save callback joins
  `target_dir/<subdir>/<filename>` instead of dropping the per-session
  subdir on the floor.
- **`remote cd ..` (cdup)** is now special-cased — pops one component
  from the per-session subdir, no-op at root, never escapes the
  sandbox. Other `..` forms (`foo/..`, `../etc`) still hit
  `is_safe_relative_subdir` and refuse.
- **`remote cd <typo>`** is now refused with E-packet
  "Directory not found" instead of being silently ACKed and dropping
  subsequent uploads into a phantom path.
- **Idle-timeout disconnect** now ends the telnet session cleanly.
  Pre-fix the gateway sent an "idle timeout" E-packet then returned to
  the file-transfer menu with the TCP socket still open; the next
  `remote ...` from C-Kermit landed on a non-protocol menu and surfaced
  as "too many retries" in the peer's UI. Server now flushes the writer
  after the E-packet, returns `io::ErrorKind::TimedOut`, and the menu
  handler ends the session.

#### Stability
- **GUI Ctrl-C hang when window is minimized**: signal-watcher now
  sends `ViewportCommand::Close` directly instead of relying on
  `request_repaint()` — some WMs throttled repaint delivery for
  minimized windows so `update()` never ran. Plus
  `runtime.shutdown_timeout(2 s)` after `block_on` returns as a
  defensive cap on tokio runtime drop.
- **Connection-rejection greetings** (max sessions, insecure-IP policy)
  now actually reach the client. Replaced non-blocking `try_write` with
  a bounded `write_all` + `flush` + `shutdown` capped at 2 seconds,
  spawned as an independent task so the accept loop doesn't serialize
  at ~0.5 conn/sec under flood.
- **Telnet `session_count`** uses `fetch_add → check → fetch_sub`
  instead of `load → fetch_add`, mirroring the SSH pattern; closes the
  cap-bust TOCTOU.

#### XMODEM / YMODEM / ZMODEM polish
- **YMODEM block-0 CRC error** now NAK-and-retries within negotiation
  instead of falling out and NAK-looping the retransmit as a
  block-number mismatch.
- **YMODEM empty-file** goes straight to EOT instead of emitting a
  SUB-padded data block.
- **XMODEM/YMODEM duplicate-block detection** now ACKs both expected-1
  AND expected-2 per Forsberg's "any already-seen block" recommendation.
- **XMODEM first-block mode auto-detect**: a trailer-format mismatch on
  the very first block falls back to the alternate mode (CRC↔checksum)
  and locks the session. Closes the negotiation timing race against
  vintage Christensen 1977 / CP/M MODEM7 / C64 BBS senders that ignore
  `'C'` until NAK'd, AND the modern slow-startup race where the
  receiver flips to checksum mid-flight against a CRC-capable sender.
- **ZMODEM inter-file header CRC mismatches** now ZNAK-and-retry
  (bounded by `max_retries`) instead of silently truncating the rest of
  a long batch on a single bit-flip.
- **ZMODEM phase-1 negotiation** no longer counts stale ZRQINIT /
  unexpected frames against the retry budget — chatty receivers were
  burning retries on bytes that proved the link was alive.
- **ZMODEM `0x98`** added to the ZDLE escape table (8-bit dual of
  ZDLE/0x18 per Forsberg §10 Table 4).
- **ZMODEM ZSINIT TESCCTL/TESC8** parsing per Forsberg §11.3; receiver
  now ACKs ZSINIT instead of silently ignoring the flag.

#### Web browser
- **HTTPS→HTTP downgrade** is now signalled to the user with a
  `[!] HTTPS failed — fetched over plain HTTP` banner instead of being
  silent. Both `fetch_and_render` and the form-submit POST path were
  transparently retrying over plain HTTP on TLS error.
- **Gopher selector** filters CR/LF/NUL on user-supplied selectors to
  prevent protocol-line injection in search queries (TAB preserved as
  the legitimate item-type-7 separator).

### Tests

- **997 lib + 1 binary e2e tests** pass, 0 failed; clippy clean on
  Linux + `x86_64-pc-windows-gnu`.

## [0.5.2] - 2026-04-29

### Fixed

#### ZMODEM autostart actually works
- The menu-input state machine detected the `** ZDLE [ABC]` prefix and
  called `handle_zmodem_autostart`, which previously sent the spec'd
  abort sequence and printed "ZMODEM is not yet supported" — even
  though `zmodem.rs` has shipped full ZMODEM support. The handler now
  drains the residual ZRQINIT bytes, validates the transfer dir, and
  calls `zmodem_receive`, with a save flow + summary screen matching
  the menu-initiated upload path.

#### ZMODEM receive metadata
- `parse_zfile_info` now returns a `ZfileInfo` struct (Forsberg §11 —
  length is decimal, mtime + mode are octal). `ZmodemReceive` carries
  the matching `modtime` + `mode` fields so the saved file gets the
  correct mtime / permissions instead of the prior `None` / default.
- `modtime=0` / `mode=0` are filtered to `None` in the parser. Our own
  `zmodem_send` and most other senders (including `lrzsz`) write
  `"<len> 0 0 0 0 <len>"` when they don't have those values;
  propagating `Some(0)` would have driven `apply_ymodem_meta` to set
  the saved file's mtime to epoch and mode to 0 (no permissions for
  anyone) — worse than ignoring the field altogether.

#### Atomic batch-receive saves
- The ZMODEM-autostart, ZMODEM/Kermit-batch-upload, and Kermit-server
  save loops all used a non-atomic `exists()` + `std::fs::write`
  pattern with a TOCTOU window. New async `save_received_file` helper
  opens with `create_new(true)` for atomic create-only semantics and
  uses `tokio::fs` for non-blocking I/O. Returns
  `SaveError::AlreadyExists` / `SaveError::WriteFailed` so each caller
  maps to its own per-file skip wording. All four batch-receive save
  sites now share one code path.
- Sync `std::fs::write` of up to 8 MB was blocking the tokio executor
  for tens of milliseconds on long telnet sessions — replaced with the
  async helper above.

#### Cross-platform CI
- **Windows `compute_resume_offset` tests**: `set_modified` on Windows
  requires the file handle to have write permission
  (`FILE_WRITE_ATTRIBUTES`); `File::open` opens read-only so the call
  was failing with permission denied. Replaced the three affected
  mtime-mutation helpers with `OpenOptions::new().write(true).open(...)`.
- **Windows symlink-resume test** unused-variable lint — moved
  `link_path` declaration inside the `#[cfg(unix)]` block alongside the
  symlink call.
- **Rust 1.95 clippy `collapsible_match`** on the seven A-packet
  single-byte sub-attribute arms in `parse_attributes` — converted to
  match guards. Behavior unchanged.

### Changed

- **`MAX_FILE_SIZE` consolidated** to `crate::tnio::MAX_FILE_SIZE`
  (single `u64` constant); xmodem / zmodem / kermit / telnet now
  import it.
- **IAC-escape control surface unified**: removed the vestigial
  `kermit_iac_escape` config field everywhere (struct, parser, writer,
  default, GUI checkbox, telnet menu toggle, settings screen,
  `egateway.conf` docstring). The three Kermit call sites now read
  `self.xmodem_iac` like XMODEM and ZMODEM already do — the menu
  toggle is the single operator-visible source of truth.
- **Kermit error strings** normalized from `"Kermit recv: ..."` to
  `"Kermit: ..."` at six sites.
- **Module docstrings** rewritten for the Ethernet Gateway scope;
  stale "no batch mode" / "full server-mode is not implemented"
  comments and self-referential commit/Gap markers cleaned out.

### Tests

- **935 lib + 1 binary e2e tests** pass; clippy clean on Linux +
  `x86_64-pc-windows-gnu`.

## [0.5.1] - 2026-04-28

### Added

#### Kermit protocol support
- **Full Kermit send and receive** implemented in `src/kermit.rs` per
  Frank da Cruz, "Kermit Protocol Manual" (1987) + C-Kermit extensions.
  S/F/A/D/Z/B/E/C packet dispatch, CHKT 1/2/3 (single-byte / two-byte /
  three-byte CRC), Send-Init capabilities negotiation, long packets,
  eighth-bit prefix, repeat-count compression, and locking-shifts.
- **Sliding window** (selective-repeat ARQ): D-packets ride a windowed
  sender with per-seq retransmit timer and selective NAK retransmit;
  receiver buffers out-of-order packets and NAKs the missing seq.
  Window size 1–31 (spec max 31 < 32 = half of mod-64 seq space, so
  forward/back disambiguation is unambiguous). Control packets
  (S/F/A/Z/B) stay stop-and-wait.
- **Streaming Kermit** (CAPAS byte 3 bit 2): D-packets pushed
  back-to-back with no per-D ACK; receiver suppresses D-ACKs. Z-ACK
  confirms the whole stream. Mid-stream NAKs trigger selective
  retransmit, then resume.
- **Peer TIME field** honored as our retransmit timeout (spec §3.2).
  `TIME=0` falls back to `kermit_packet_timeout` config, floored at
  1 second.
- **Server mode** (S/R/G/I/B/E/C dispatch) — `remote dir`,
  `remote cwd`, `remote help`, `get`, `send`, `bye`, `finish`.
- **Five extended A-packet sub-attributes** per spec §5.1: `&`
  long-form file length (decimal u64), `1` character set, `*` encoding,
  `,` record format, `-` record length. Parsed and surfaced in verbose
  logs; receiver uses `length.or(long_length)` for `declared_size`.
  Encoder emits the existing six tags (`!` length, `#` date, `+` mode,
  `.` system_id, `"` file_type, `@` disposition) plus the new four.
- **Detected Kermit flavor** (C-Kermit, G-Kermit, Kermit-95, …)
  surfaced in the upload-complete summary line.
- **Telnet File Transfer menu** entry for Kermit alongside XMODEM /
  XMODEM-1K / YMODEM / ZMODEM. The first-line hint is now generic
  "(More for others)" since the popup covers every protocol.

### Changed

- **Shared raw I/O extracted** to `src/tnio.rs`: `ReadState`,
  `is_can_abort`, `raw_read_byte`, `nvt_read_byte`,
  `consume_telnet_command`, `raw_write_bytes` plus IAC/SB/SE/WILL/WONT/
  DO/DONT/CAN constants. The byte-stream layer that handles telnet IAC
  unescaping, NVT CR-NUL stripping, Forsberg's CAN×2 abort rule, and
  the matching write-side escaping was duplicated near-verbatim across
  `xmodem.rs` / `zmodem.rs` / `kermit.rs` (~140 lines per module). Net
  delta: 583 lines removed, 289 added.

### Fixed

- **Send-Init `WINDO`/`MAXLX` fields** are now conditional per spec
  §4.4: `WINDO` emitted iff `window > 1`, `MAXLX1`/`MAXLX2` emitted iff
  `long_packets`. Parser reads `WINDO` iff the sliding bit is set in
  CAPAS byte 1, reads `MAXLX` iff the long bit is set. Self-tests
  passed because both sides used the same buggy layout, but a session
  with `long_packets=true, sliding=false` would have advertised an
  extra `WINDO=1` byte that a strict-spec G-Kermit / E-Kermit peer
  would have misread as `MAXLX1=1`, collapsing our advertised MAXL
  from ~4096 to ~138.
- **C0/C1/DEL control range** in `is_kermit_control` was missing
  `0x80..=0x9F` and `0xFF`. Per spec §6.4, these high-bit equivalents
  must also be QCTL-prefixed. The encoder was emitting them raw; the
  decoder now also unctls bodies in the high-bit ctl range when no
  QBIN is active.
- **Long-packet `extended_len`** was being encoded as
  "5 + DATA + CHECK" (including the 5 header bytes after LEN); per
  spec it's "the length of everything in the packet that follows the
  HCHECK" — i.e., DATA + CHECK only. This is what real C-Kermit
  emits, and the mismatch caused every long-packet CRC verification
  to fail in interop.
- **`peer_id` parser**: real C-Kermit's Send-Init buries vendor-specific
  CAPAS extension bytes (CHECKPOINT, WHATAMI, …) in the trailing slot;
  our parser accepted the binary bytes as a string and produced
  garbage like `0___^"U1A`, defeating downstream flavor detection.
  Tightened the heuristic to require a 4-character ASCII letter run
  before treating the trailing bytes as an identifier; otherwise leave
  `peer_id` as `None` and let `detect_flavor` classify by capability
  bits.
- **`record_lrzsz_fixtures`** is now gated behind
  `ZMODEM_RECORD_FIXTURES=1`. The fixture-recorder was `#[ignore]`d
  but `cargo test -- --ignored` was inadvertently running it and
  silently rewriting the committed binary fixtures with timestamp-
  bearing equivalents.

### Documentation

- README and user manual extended with Kermit coverage alongside the
  existing XMODEM / YMODEM / ZMODEM sections.

### Tests

- **+218 tests** for Kermit (CRC + checksum vectors, packet
  round-trips, Send-Init negotiation, sliding-window happy path +
  lossy NAK recovery, streaming round-trips including 64 KB /
  all-bytes / multi-file / lossy, A-packet sub-attribute round-trips,
  server-mode dispatch). Three `#[ignore]` C-Kermit subprocess
  interop tests (stop-and-wait, sliding-window, streaming) drive the
  real `kermit` binary over TCP. Total: **930** unit + proptest
  tests, all green.

## [0.4.0] - 2026-04-25

### Changed

#### Project rename: XMODEM Gateway → Ethernet Gateway
- The product is now **Ethernet Gateway**. The original name no longer
  reflected the scope (SSH, web browser, AI chat, weather, modem
  emulator, gateway proxies — only one of which is XMODEM).
  Functionality is unchanged; this is purely a naming refresh.
- Cargo package renamed `xmodem-gateway` → `ethernetgateway`.
- GitHub repository moved to
  [`rickybryce/ethernetgateway`](https://github.com/rickybryce/ethernetgateway).
- Configuration file renamed `xmodem.conf` → `egateway.conf`.
- SSH host key file renamed `xmodem_ssh_host_key` → `ethernet_ssh_host_key`.
- Outbound SSH gateway client key renamed `xmodem_gateway_ssh_key` →
  `ethernet_gateway_ssh_key`.
- AppImage renamed `XMODEM_Gateway-x86_64.AppImage` →
  `Ethernet_Gateway-x86_64.AppImage`.
- systemd unit renamed `xmodem-gateway.service` → `ethernetgateway.service`.
- Telnet menu prompt path renamed `xmodem> ` → `ethernet> ` (and all
  sub-paths: `ethernet/xfer`, `ethernet/web`, `ethernet/config/...`).
- Hayes dial shortcut: `ATDT xmodem-gateway` → `ATDT ethernetgateway`
  (the `1001000` shortcut number is unchanged).
- HTTP browser User-Agent: `XmodemGateway/1.0` → `EthernetGateway/1.0`.

**Migration**: existing deployments that want to preserve identity should
rename `xmodem.conf` → `egateway.conf` and `xmodem_ssh_host_key` →
`ethernet_ssh_host_key` (and the gateway client key) before first start.
Otherwise the gateway will create fresh files and SSH clients will see a
"host key changed" warning.

#### GUI refresh
- New logo (`ethernetgatewaylogo.png`, 1774×887, 2:1 aspect ratio) displayed at
  366×183 with trilinear (mipmap) texture filtering for clean
  downscaling.
- Window/panel background darkened from `#050E1A` to `#000510` to match
  the new logo's deep-navy backdrop.

### Added

#### ZMODEM polish (continuation of 0.3.5 work)
- **`ZRINIT` drain**: receiver now consumes the trailing ZRINIT/handshake
  bytes some senders (notably lrzsz `sz`) emit before they go quiet.
  Eliminates a 5-second stall at the end of a successful ZMODEM receive.
- **`ZSINIT` handler** on receive — sender-supplied attention/escape
  configuration is parsed and ack'd per Forsberg §11.5, so senders that
  block waiting for the ACK now proceed.
- **lrzsz interop suite**: 13 captured-wire replay fixtures (tiny / exact-
  1 KB / all-bytes / 2-file batch / ZSKIP / aborted-mid-batch) plus two
  `#[ignore]` subprocess tests that drive real `sz` / `rz` end-to-end.

#### XMODEM/YMODEM/ZMODEM compliance pass
- **CAN×2 abort handling** per Forsberg's recommendation: a single CAN is
  no longer treated as an abort; two consecutive CANs (with no
  intervening data) are required. Routes through a shared
  `is_can_abort` helper so all three protocols agree.
- **Spec-citation tests** (63 new tests across the four files) that
  reference exact section numbers in the Forsberg specs and validate
  edge-case behavior (block-zero NAK retry, zero-length payloads,
  trailing-`SUB` preservation, etc.).
- **YMODEM maximal compliance** — full Forsberg §6.1 block-0 metadata
  (filename, size, mtime, mode, serial number) is parsed on receive and
  applied (mtime + mode) on save. Send path emits the same set.

#### End-to-end test infrastructure
- **Binary-level e2e test** (`tests/binary_e2e.rs`): launches the actual
  release binary as a subprocess, drives the telnet UI through the web
  browser flow against a hermetic localhost HTTP server, and asserts on
  the rendered output. Catches integration regressions that unit tests
  alone miss.
- **Hermetic e2e tests** for the HTTP and Gopher browsers: spin up
  loopback servers, run the parser/renderer end-to-end, assert on
  PETSCII/ANSI/ASCII rendering invariants.

### Fixed

- Logo rendering aspect ratio is now correct after the asset swap.
  Previously the new 2:1 logo was being squashed into a 1.6:1 box.

### Tests

- Total: **719** unit + proptest tests (718 lib + 1 binary e2e), 0
  failed, 15 ignored. All green on Linux / macOS / Windows.

## [0.3.5] - 2026-04-23

### Added

#### ZMODEM protocol support
- **Full ZMODEM send and receive** implemented per the Forsberg 1988
  specification in `src/zmodem.rs` — ZDLE escape layer, hex / binary16 /
  binary32 headers, CRC-16 and CRC-32, batch transfer per §4, receiver
  `ZSKIP` to decline individual files per §7, and `rz\r` auto-start
  trigger so Qodem, ZOC, and other auto-detecting terminals begin the
  transfer without a separate `rz` command.
- **File transfer menu entry** for ZMODEM alongside XMODEM / XMODEM-1K /
  YMODEM. Stop-and-wait flow control (ZCRCQ mid-frame + ZCRCE
  end-of-frame); our `ZRINIT` advertises `CANFDX|CANOVIO|CANFC32` without
  requiring streaming.
- **Additional file-transfer configuration options** surfaced in the
  Gateway Configuration menu.

### Fixed

- **Windows CI**: ZMODEM fixture binaries are now marked as binary in
  `.gitattributes` so the CRLF auto-conversion on Windows runners does
  not corrupt them. Fixes the sporadic Windows CI failure on
  `test_lrzsz_rz_zskip_interop` and the captured-wire replay tests.
- **CI runner configuration**: resolved transient runner errors that
  were preventing reliable green builds.
- **GUI**: copy/paste now works as expected in the configuration editor
  text fields.

### Documentation

- README updated with NULL-modem adapter guidance and a clarified telnet
  command example.
- User manual extended with ZMODEM coverage alongside the existing
  XMODEM / YMODEM sections.

### Tests

- **+46 tests** added for the ZMODEM implementation (CRC vectors, ZDLE
  round-trips, header round-trips, subpacket round-trips, ZFILE parser,
  full send↔receive round-trips, batch / skip handling, ZABORT, non-zero
  `ZRPOS` resume, proptest fuzzers on adversarial bytes) plus two
  `#[ignore]` lrzsz subprocess interop tests. Total: **617** unit +
  proptest tests, all green.

## [0.3.4] - 2026-04-18

### Fixed

#### XMODEM / YMODEM over telnet — full RFC 854 NVT compliance
- **CR-NUL stuffing on both send and receive.** Bare `0x0D` (CR) in file data
  is now emitted on the wire as `CR NUL` per RFC 854 §2.2, and the receive
  path strips trailing `NUL` after `CR`. Without this, any block containing
  a `0x0D` data byte (common in binary files — EXE, PDF, compressed
  archives) desynced the stream by one byte per CR. Visible symptom was
  "Transfer stalls at 3–4 blocks, client repeatedly sends `'C'`".
- **IAC escape/unescape on both directions** matches the existing telnet
  NVT rule already applied to `IAC` itself; the two transforms are now
  always active together when `xmodem_iac` is on.
- **YMODEM end-of-batch handshake on receive.** After ACKing the final
  `EOT`, the server now sends `'C'` and consumes the "null block 0"
  (filename starts with `NUL`) that strict senders emit per Forsberg §7.4.
  Fixes "YMODEM upload completes all data but client hangs" on ExtraPuTTY,
  Tera Term, and lrzsz's `sb`.
- **YMODEM size-based truncation.** After a YMODEM transfer the receiver
  now truncates to the exact `size` field from block 0 instead of stripping
  trailing `SUB` (0x1A) padding. Fixes files that legitimately end in
  `0x1A` bytes (EXEs, some archives) being silently truncated.

### Added

#### Session-side configuration
- **Gateway Configuration menu** at `Configuration → G` in the telnet
  session: toggles the outbound Telnet mode (Telnet / Raw TCP) and the
  outbound SSH auth mode (Key / Password) at runtime, persists to
  `egateway.conf`, and takes effect on the next gateway connection with no
  server restart. Replaces the per-connection interactive prompts that
  used to live inside the Telnet Gateway and SSH Gateway flows.
- **Config key `ssh_gateway_auth`** (`"key"` or `"password"`, default
  `"password"`) drives the SSH Gateway auth choice. No silent fallback —
  failures now clearly point the user at Server → More or Config → G.
- **Pre-transfer overwrite prompt.** On upload, if the target filename is
  already present the server asks `Overwrite? (Y/N)` *before* the transfer
  starts. Avoids running a multi-MB transfer only to fail at the final
  write step.

#### GUI console
- **"More..." popups** on the Server and Serial Modem frames expose the
  full set of persistent settings that didn't fit on the main panel —
  telnet gateway mode + negotiation, SSH gateway auth (with the gateway's
  public key shown read-only when Key mode is selected), the extended
  Hayes AT profile (E/V/Q, X-level, &C/&D/&K), all 27 S-registers, and
  the four stored phone-number slots. Each popup has its own **Save**
  button that persists without restarting the server.
- **Popup styling** distinct from main panels — deep forest-green panel
  background, brighter-green text-entry fields — so the user immediately
  sees which surface they're editing.

### Changed

#### XMODEM transforms auto-default
- **Default now picked from detected terminal type.** After terminal
  detection, `xmodem_iac` is auto-set to **on** for ANSI sessions
  (PuTTY / ExtraPuTTY, Tera Term, C-Kermit, SecureCRT — all escape per
  RFC) and **off** for PETSCII and ASCII sessions (retro clients like
  IMP8, CCGMS, StrikeTerm, AltairDuino firmware that speak raw bytes
  despite the port-23 connection). User can still flip per-session with
  the `I` key in the File Transfer menu.

#### UX polish
- **Post-transfer settle window.** Error messages after a failed upload
  (transfer failure, save I/O error, duplicate filename) now honour the
  same 1-second pause the success path already used, so ExtraPuTTY's
  transfer dialog has time to close before our message prints. Also
  drains stray bytes from the client's post-transfer chatter so
  `wait_for_key` actually waits for a human keypress.
- **Select Protocol menu** on download now clears the screen instead of
  appending after the download list.
- **Default `ssh_gateway_auth` flipped from `key` to `password`** — works
  out of the box with any SSH server that allows password auth; Key mode
  requires a one-time `authorized_keys` setup.

### Removed

- The interactive `T`-toggle prompt inside the Telnet Gateway flow and
  the `K`-show-pubkey prompt inside the SSH Gateway flow. Both options
  now live in config (editable via GUI Server → More or Config → G).

### Documentation

- User manual §8.3, §8.6 rewritten to reflect NVT symmetry, the auto-IAC
  default, and the overwrite prompt. `index.html` brought in line.
- Modem Emulator help in-session now lists `AT&Zn=s` / `ATDSn` /
  `ATIn` / `ATXn` / `AT&C/&D/&K` / `A/` alongside the pre-existing
  quick reference.

### Tests

- +1 regression test: `test_ymodem_round_trip_preserves_trailing_sub_bytes`
  verifies YMODEM size-truncation preserves a payload that legitimately
  ends in `0x1A` bytes. Total: **571** unit + proptest tests, all green.

## [0.3.3] - 2026-04-18

### Added

#### Telnet server — additional RFC compliance
- **RFC 854 EC / EL**: `IAC EC` now surfaces to line-editors as `DEL` (0x7F)
  and `IAC EL` as `NAK` (0x15), with the `read_input_loop` handling NAK as
  "erase the current line."
- **RFC 859 STATUS** (option 5): `DO STATUS` is answered with `WILL STATUS`;
  `SB STATUS SEND` returns an `SB STATUS IS <state>` dump of every option
  advertised and not yet denied. Works with the Unix `telnet` client's
  `status` / `send status` subcommands.
- **RFC 860 TIMING-MARK** (option 6): `DO TIMING-MARK` is answered with
  `WILL TIMING-MARK` after flushing pending output, providing clients a
  processing-synchronization point.

#### Outgoing Telnet Gateway
- **IAC escape/unescape** in both directions; literal 0xFF data bytes now
  survive the wire without being mistaken for IAC.
- **Full RFC 1143 six-state Q-method** (`No`, `Yes`, `WantYes`,
  `WantYesOpposite`, `WantNo`, `WantNoOpposite`) for option negotiation.
- **Cooperative mode** (`telnet_gateway_negotiate = true`): proactively
  offers `WILL TTYPE`, `WILL NAWS`, and `DO ECHO` at connect; responds to
  `SB TTYPE SEND` with the local user's terminal type; responds to
  `DO NAWS` with the local user's current window size; forwards NAWS
  updates mid-session when the local user resizes.
- **Raw-TCP escape hatch** (`telnet_gateway_raw = true`): bypasses the
  telnet IAC layer entirely for destinations that aren't really telnet.
  Toggleable live from the Telnet Gateway menu with the **T** key; choice
  persists to `egateway.conf`.
- **8 KiB subnegotiation body cap**: malicious remotes cannot exhaust
  memory by sending huge `SB` bodies without a terminating `IAC SE`.
- **Property-based fuzz test** (`qmethod_proptest`) covers the full Q-method
  state machine with randomized sequences. Regression corpus checked into
  `proptest-regressions/telnet.txt`.

#### Outgoing SSH Gateway
- **Public-key authentication** with auto-generated Ed25519 client keypair
  (`ethernet_gateway_ssh_key`, 0o600 on Unix). Tried before password; on
  acceptance, the password prompt is skipped entirely.
- **"Show gateway public key" menu**: press **K** at the SSH Gateway
  menu to display the one-line OpenSSH-format public key for pasting
  into a remote's `~/.ssh/authorized_keys`.
- **Audit log for host-key trust decisions**: TOFU-accept, key-update,
  and key-reject events are written to `glog!` with host, port,
  algorithm, and SHA-256 fingerprint.

#### Hayes modem emulator
- **`A/` repeat-last-command** (no `AT` prefix, no CR required).
- **`ATI0`–`ATI7`** identification variants (product code, ROM checksum,
  ROM test, firmware, OEM, country, diagnostics, product info).
- **Stored phone-number slots**: `AT&Zn=s` stores a number in slot
  `n ∈ {0,1,2,3}`; `ATDS` / `ATDS<n>` dials it. Persisted by `AT&W`,
  restored by `ATZ`. Preserves hostname case so `AT&Z1=Pine.Example.com`
  works.
- **S-registers expanded to S0–S26**: S13–S24 are reserved-zero
  placeholders for legacy init strings; S25 (DTR detect time) and
  S26 (RTS/CTS delay) match Hayes defaults.
- **Dial-string modifiers**: `,` (pause by S8), `W` (wait-for-dialtone by
  S6), `;` (stay in command mode), `*`/`#` (preserved DTMF digits),
  `P`/`T`/`@`/`!` (accepted, ignored). Hostname heuristic prevents
  stripping `P`/`T`/`W` from names like `pine.example.com`.
- **ATX0–ATX4** result-code verbosity per RFC.
- **`AT&C` / `AT&D` / `AT&K`**: parsed, stored, persisted, displayed in
  `AT&V`. Actual hardware pins are not driven; see README limitations.
- **Silent-OK fallback** for unknown commands (`ATB`, `ATC`, `ATL`,
  `ATM`, `AT&B`, `AT&G`, `AT&J`, `AT&S`, `AT&T`, `AT&Y`, …) so legacy
  init strings don't halt mid-setup.

### Security

- **Shared per-IP brute-force lockout** across telnet and SSH servers.
  After 3 failed authentication attempts in 5 minutes, the source IP is
  blocked for 5 minutes across both protocols — an attacker can't bounce
  between them to reset the counter.
- **0o600 file permissions on Unix** for all sensitive files:
  `egateway.conf`, `dialup.conf`, `gateway_hosts`, `ethernet_ssh_host_key`,
  `ethernet_gateway_ssh_key`.
- **Per-PID temporary filenames** for atomic config writes; closes a
  TOCTOU window on shared working directories.
- **`save_config` now acquires the `CONFIG` mutex before disk write**,
  so a concurrent session-side `update_config_values` can't clobber the
  GUI-initiated write.
- **SSH Gateway** now calls `session.disconnect` on every early-return
  path after authentication, preventing orphaned authenticated sessions
  on the remote.

### Fixed

- Q-method refusal flags (`sent_dont` / `sent_wont`) are now cleared on
  every contradicting-verb emission and set on every refusal emission
  (including the `WantYesOpposite → WantNo` transitions). Prevents
  duplicate refusal replies to a misbehaving peer. Caught by the
  proptest fuzzer.
- `gateway_telnet` local → remote direction now IAC-escapes outbound 0xFF
  data bytes correctly.
- `gateway_telnet` remote → local direction now parses inbound IAC rather
  than leaking protocol bytes to the user's terminal.

### Changed

- `gateway_ssh` prompt order: host/port/username first, then try pubkey
  auth, prompt for password only if pubkey is rejected. Matches how
  OpenSSH from the command line behaves.
- Hayes S7 default is now `15` seconds (capped internally at 60); the
  Hayes `50` second default was too slow for gateway users.

## [0.3.2] - earlier

- RFC compliance features for Telnet (RFC 854 / 855 / 857 / 858 /
  1073 / 1091 / 1143).
- Drain before "Press any key" to avoid CRLF stickiness.
- Security fixes and minor bug fixes.

## [0.3.1] - earlier

- Added web browser for user manual.
- Minor UI polish.

## [0.3.0] - earlier

- Added configuration options for telnet/SSH/serial servers.
- GUI for configuration editing (eframe/egui).
- Ring emulator and dialup directory.
- Windows build fix for `GetDiskFreeSpaceExW`.
- S-register persistence via `AT&W`.

[Unreleased]: https://github.com/rickybryce/ethernetgateway/compare/v0.9.5...HEAD
[0.9.6]: https://github.com/rickybryce/ethernetgateway/compare/v0.9.5...HEAD
[0.9.5]: https://github.com/rickybryce/ethernetgateway/releases/tag/v0.9.5
[0.9.4]: https://github.com/rickybryce/ethernetgateway/releases/tag/v0.9.4
[0.9.3]: https://github.com/rickybryce/ethernetgateway/releases/tag/v0.9.3
[0.9.2]: https://github.com/rickybryce/ethernetgateway/releases/tag/v0.9.2
[0.9.1]: https://github.com/rickybryce/ethernetgateway/releases/tag/v0.9.1
[0.9.0]: https://github.com/rickybryce/ethernetgateway/releases/tag/v0.9.0
[0.8.1]: https://github.com/rickybryce/ethernetgateway/releases/tag/v0.8.1
[0.8.0]: https://github.com/rickybryce/ethernetgateway/releases/tag/v0.8.0
[0.7.0]: https://github.com/rickybryce/ethernetgateway/releases/tag/v0.7.0
[0.6.4]: https://github.com/rickybryce/ethernetgateway/releases/tag/v0.6.4
[0.6.3]: https://github.com/rickybryce/ethernetgateway/releases/tag/v0.6.3
[0.6.2]: https://github.com/rickybryce/ethernetgateway/releases/tag/v0.6.2
[0.6.1]: https://github.com/rickybryce/ethernetgateway/releases/tag/v0.6.1
[0.5.4]: https://github.com/rickybryce/ethernetgateway/releases/tag/v0.5.4
[0.5.3]: https://github.com/rickybryce/ethernetgateway/releases/tag/v0.5.3
[0.5.2]: https://github.com/rickybryce/ethernetgateway/releases/tag/v0.5.2
[0.5.1]: https://github.com/rickybryce/ethernetgateway/releases/tag/v0.5.1
[0.4.0]: https://github.com/rickybryce/ethernetgateway/releases/tag/v0.4.0
[0.3.5]: https://github.com/rickybryce/ethernetgateway/releases/tag/v0.3.5
[0.3.4]: https://github.com/rickybryce/ethernetgateway/releases/tag/v0.3.4
[0.3.3]: https://github.com/rickybryce/ethernetgateway/releases/tag/v0.3.3
[0.3.2]: https://github.com/rickybryce/ethernetgateway/releases/tag/v0.3.2
[0.3.1]: https://github.com/rickybryce/ethernetgateway/releases/tag/v0.3.1
[0.3.0]: https://github.com/rickybryce/ethernetgateway/releases/tag/v0.3.0
