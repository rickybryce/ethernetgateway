# Changelog

All notable changes to **ethernetgateway** are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

Acting on an external quality review of the tree at `6c2ed36`, then a
follow-up quality/stability pass of our own.

### Fixed
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

### Changed
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

### Added
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

## [0.8.1] - 2026-07-28

### Added
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

### Fixed
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

[Unreleased]: https://github.com/rickybryce/ethernetgateway/compare/v0.8.1...HEAD
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
