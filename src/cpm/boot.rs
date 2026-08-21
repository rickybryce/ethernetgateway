//! Cold-starting a disk image: the 88-DCDD bootstrap.
//!
//! A real Altair boots a disk from a small PROM — the disk bootstrap loader —
//! which selects drive 0, loads the head, waits for sector 0 to come round,
//! copies its data into low memory and jumps there. From that point the disk's
//! own code is in charge, which is the entire point of this path: the layout
//! knowledge stays on the disk, in software that already works.
//!
//! We do not have the PROM, so the sequence is written here instead. That is a
//! deliberate substitution and worth being clear about — it means this code
//! must land the payload at exactly the address the PROM would, or the boot
//! sector's own jumps go to the wrong place.
//!
//! **The address is not a guess.** The boot sector of a real Altair CP/M disk
//! begins `31 00 DF` (`LXI SP,0DF00h`), `F3` (`DI`), then talks to the
//! controller with `D3 08` / `DB 08`. Its absolute jumps target `0007h`,
//! `0015h`, `0020h`, `0030h`, `0048h` — and each of those matches the offset of
//! the corresponding instruction *within the payload itself*. The jump to
//! `0007h`, for instance, lands on the `DB 08` at payload offset 7. That only
//! works if the payload sits at `0000h`, so it is loaded there and entered
//! there.
//!
//! The 128 data bytes sit at offset 3 of the 137-byte sector. That offset is
//! confirmed for the boot region by the same evidence — the code decodes and
//! its jumps line up — and independently by the fact that this is where the
//! CP/M directory is found on these disks.

use super::dcdd::{Dcdd, Request, SECTOR_LEN};
use std::path::PathBuf;

/// What `cpm_boot_image` holds when CP/M means our own emulator.
///
/// Empty rather than a word like `emulator`, so that an existing config file
/// with no such key means exactly what it always meant.
pub const BOOT_EMULATOR: &str = "";

/// What the emulator is called wherever a user has to choose it.
///
/// "CP/M Emulator" and not "CCP-lite": the prompt is one part of a thing that
/// is also a Z80, our BDOS and BIOS, drives A:–P:, EGT8080 and the virtual modem,
/// and `cpm_emu_enabled`, the telnet screen, the web page, the desktop UI and
/// the manual all already call it this. The qualifier is doing the real work,
/// because *booting* an Altair CP/M disk is emulation too — what separates them
/// is whose operating system runs and whose drives you get.
pub const BOOT_EMULATOR_LABEL: &str = "CP/M Emulator (gateway drives A:-P:)";

/// The choices for `cpm_boot_image`: our emulator, then every image on hand.
///
/// One function for all three configuration screens, the way
/// [`super::uart::UART_CHOICES`] serves the virtual-modem port. The list is
/// built from the images folder rather than written down, so telnet, web and
/// desktop cannot drift apart and none of them can offer a disk that is not
/// there.
///
/// Each entry is `(value, label)` — the value is what goes in the config file,
/// the label is what a person reads.
///
/// **A disk with no boot program on it is not offered; a disk this machine has
/// no board for still is.** Those are different failures and the operator fixes
/// them differently, which is why the filter is [`image_can_boot`] rather than
/// "did the cold start succeed".
///
/// This list said the opposite until 2026-08-15, and said so deliberately: the
/// argument was that a persisted setting should fail loudly at boot, naming the
/// boards the machine really has, rather than the file quietly not being in the
/// list. That argument is still right — for a *board mismatch*, which is what it
/// was reasoned about. It does not cover a data disk, which cannot boot on any
/// machine, in any configuration, ever: there is nothing for the operator to go
/// and fix, so offering it is not a useful failure, only a wasted one. Four of
/// them shipped in the collection this gateway downloads.
pub fn boot_choices(cpm_base: &std::path::Path) -> Vec<(String, String)> {
    boot_choices_by(cpm_base, image_can_boot)
}

/// [`boot_choices`] with the bootability question passed in.
///
/// The split is for testing, and for one reason worth naming: the only honest
/// implementation of that question reads an image and cold-starts it, so a unit
/// test of *this* function — ordering, labels, which files count as images —
/// would otherwise need a real bootable disk on disk to check that a `readme.txt`
/// is skipped. The list-building and the filter are separately checkable, and
/// the filter's real cover is a live gate against the collections.
fn boot_choices_by(
    cpm_base: &std::path::Path,
    can_boot: impl Fn(&std::path::Path) -> bool,
) -> Vec<(String, String)> {
    let mut out = vec![(BOOT_EMULATOR.to_string(), BOOT_EMULATOR_LABEL.to_string())];
    let dir = super::image::images_dir(cpm_base);
    for name in super::image::available_images(cpm_base) {
        if !can_boot(&dir.join(&name)) {
            continue;
        }
        out.push((name.clone(), format!("Boot {name}")));
    }
    out
}

/// Could this image boot at all — on some machine, in some configuration?
///
/// **Cached, because both callers draw screens.** [`boot_target`]'s own comment
/// is the rule here: the desktop panel redraws four times a second forever, and
/// a cold start reads the whole image — 4.9 MB for a hard disk. So the answer is
/// kept against the file's identity (path, length, modification time) and the
/// two settings that could change it. An edited disk has a new mtime and is
/// asked again; a disk nobody touched is one `stat` and a hash lookup.
///
/// **The verdict is about the disk, not about this machine's boards** — see
/// [`Bootability`], which is where that distinction is drawn once so that the
/// list, the picker and the manifest generator cannot each draw it differently.
pub fn image_can_boot(path: &std::path::Path) -> bool {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    let cfg = crate::config::get_config();
    let (machine, cpu) = (cfg.cpm_boot_machine.clone(), cfg.cpm_cpu.clone());
    drop(cfg);

    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let key = (
        path.to_path_buf(),
        meta.len(),
        meta.modified().ok(),
        machine.clone(),
        cpu.clone(),
    );

    static CACHE: OnceLock<Mutex<HashMap<CacheKey, bool>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(hit) = cache.lock().ok().and_then(|c| c.get(&key).copied()) {
        return hit;
    }

    // The classification lives in `Bootability::offer`, not here: this was three
    // separate `match` arms in three files, and when they were wrong they were
    // wrong together.
    let verdict = match std::fs::read(path) {
        Err(_) => false,
        Ok(bytes) => super::boot_machine::BootMachine::bootability(bytes, &machine, &cpu).offer(),
    };
    if let Ok(mut c) = cache.lock() {
        // An images folder is tens of entries and the key carries an mtime, so
        // this is bounded by what the operator actually has and how often they
        // edit it — but a gateway left running for months while a script
        // rewrites disks would grow it without limit, so it is capped.  Clearing
        // rather than evicting one: this is a cache of a pure question, so the
        // cost of a cold start again is time, never correctness.
        if c.len() >= 512 {
            c.clear();
        }
        c.insert(key, verdict);
    }
    verdict
}

/// Which machine an image really boots on, resolving `auto` by reading it.
///
/// Cached exactly like [`image_can_boot`], and for the same reason: the mount
/// screens ask this while drawing, `auto` is the default, and answering it means
/// reading the disk's system tracks. Keyed on the file's identity and the
/// configured value, so changing either asks again.
///
/// A non-`auto` setting is returned untouched — the operator has said which
/// machine, and detection does not get to overrule them.
pub fn machine_for_image(path: &std::path::Path, configured: &str) -> String {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    if configured != super::console::AUTO_MACHINE {
        return configured.to_string();
    }
    let Ok(meta) = std::fs::metadata(path) else {
        return configured.to_string();
    };
    let key = (path.to_path_buf(), meta.len(), meta.modified().ok(), configured.to_string());

    /// Its own key type: four fields, not the bootability answer's five — that
    /// one also varies with `cpm_cpu`, which cannot change which machine a disk
    /// is for.
    type MachineKey = (std::path::PathBuf, u64, Option<std::time::SystemTime>, String);
    static CACHE: OnceLock<Mutex<HashMap<MachineKey, String>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(hit) = cache.lock().ok().and_then(|c| c.get(&key).cloned()) {
        return hit;
    }
    let answer = match std::fs::read(path) {
        Ok(bytes) => super::detect::machine_for(configured, &bytes).0,
        Err(_) => configured.to_string(),
    };
    if let Ok(mut c) = cache.lock() {
        if c.len() >= 512 {
            c.clear();
        }
        c.insert(key, answer.clone());
    }
    answer
}

/// The identity a bootability verdict is remembered against.
///
/// Named rather than inlined because it is five fields and one of them is an
/// `Option` that means "this filesystem does not report modification times" —
/// which is a real answer on some platforms, and one that makes the entry
/// effectively permanent for that file rather than wrong.
type CacheKey = (std::path::PathBuf, u64, Option<std::time::SystemTime>, String, String);

/// What to show for the current setting, whether or not the image still exists.
///
/// A disk named in the config and since deleted must still say what it is: the
/// screens are also where an operator finds out *why* their gateway is running
/// the emulator when they asked for a disk.
pub fn boot_choice_label(value: &str) -> String {
    if value.is_empty() {
        BOOT_EMULATOR_LABEL.to_string()
    } else {
        format!("Boot {value}")
    }
}

/// The suffix a setting carries when it is not what is going to run.
///
/// Three, not one, because the fallbacks are different mistakes and an operator
/// fixes them differently: `(missing)` means put the disk back or pick another,
/// `(invalid name)` means the value in the file could never have named a disk at
/// all — most likely a path was typed where a bare filename belongs — and
/// `(not bootable)` means the disk is right there and carries no boot program,
/// so no amount of looking for it will help.  Fifteen characters exactly, which
/// is the whole budget `cpm_runs_row` reserves out of a 26-column PETSCII row —
/// `(will not boot)` read better and was one too long, and the width guard in
/// `test_a_setting_that_will_not_run_says_so` is what said so.
///
/// The third arrived with the boot-list filter and is the reason that filter
/// could not stop at the list. A withheld disk is absent from the choices, but
/// a config file can still *name* one — it was set before the filter existed, or
/// typed in by hand — and the web and desktop screens re-add whatever is set so
/// the setting cannot silently reset itself. Without a mark of its own it
/// re-appeared looking exactly like a disk that was about to boot.
///
/// Empty for the cases that *are* going to run, so a caller can append it
/// unconditionally.
pub fn boot_setting_mark(target: &BootTarget) -> &'static str {
    match target {
        BootTarget::Missing(_) => " (missing)",
        BootTarget::UnsafeName(_) => " (invalid name)",
        BootTarget::NotBootable(_) => " (not bootable)",
        BootTarget::Emulator | BootTarget::Image(_) => "",
    }
}

/// What to show for the current `cpm_boot_image` once it has been resolved.
///
/// [`boot_choice_label`] answers for a value in the *list*, where every entry
/// is by construction a disk that is there; this answers for the value that is
/// **set**, which may not be. All four surfaces use this one — the web page
/// spelled its own `(missing)` for a while and was the only one that said
/// anything at all, which is how the desktop and telnet rows came to claim a
/// disk was booting when the emulator was what started.
pub fn boot_setting_label(target: &BootTarget, value: &str) -> String {
    format!("{}{}", boot_choice_label(value), boot_setting_mark(target))
}

/// What a mount slot should be *called*, which depends on what CP/M is set to
/// run — and is one function so telnet, web and the desktop cannot disagree.
///
/// The same `cpm_mounts` list underneath either way. Two lists would be two
/// config keys saying one thing, which is the shape of defect this project has
/// produced more than once; what genuinely differs is only the name of the slot.
///
/// * **The emulator** owns drives `A:`&ndash;`P:` — our BDOS is underneath them,
///   the jail applies, and a mount is authoritative: if we put an image on `B:`,
///   `B:` is that image.
/// * **A booted disk** owns the hardware. A mount is handed to a *board*, at
///   whatever that board calls a slot, and whether the guest can reach it is
///   decided by its own BIOS. Stock Altair floppy CP/M knows four drives; the
///   88-HDSK CP/M on these disks uses the drive's fixed platter — slot 1 — as
///   its `B:`. Calling either of them `B:` *ourselves* would be a promise the
///   guest never agreed to, which is the whole reason this enum exists.
///
/// A board's slots need not even be a flat row: the 88-HDSK's are a drive and
/// a platter, so it names them `unit 0.1`. That is [`Controller::slot_label`]'s
/// job, not this module's — see [`slot_name`].
///
/// [`Controller::slot_label`]: super::controller::Controller::slot_label
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlotNaming {
    /// Drives `A:`–`P:`, the emulator's own.
    Drives,
    /// Whatever the board calls the slot, on a booted machine.
    Boards,
}

/// What the mount screens are mounting *into*.
///
/// **A drive on the mount screen is not a fixed thing.** Which machine CP/M is
/// configured to run decides both what a slot is called and which images can
/// usefully go in one, and until 0.9.2 the mount screens knew neither: they
/// offered every image in the folder against sixteen drive letters, whatever was
/// booting.
///
/// That was not a cosmetic gap. The board an image lands on is chosen by its
/// *size* — a 337,568-byte floppy to the 88-DCDD, a 4.9 MB image to the 88-HDSK
/// — so mounting a floppy while a hard disk is set to boot put it on a board the
/// guest never reads. Everything worked and nothing was reachable, and the only
/// hint was a warning printed after the boot had already started. Worse, the two
/// namings appeared *together*: one screen could show `unit 0.0` beside
/// `Drive 1`, two vocabularies for one list, because each row was named after
/// its own image's board.
///
/// So the question is asked once, here, and the screens follow it.
#[derive(Debug, Clone)]
pub struct MountContext {
    /// How to name a slot.
    pub naming: SlotNaming,
    /// The machine key in force, for `board_for`/`slot_label`.
    machine: String,
    /// The board the booted disk is on, if a disk is booting.
    ///
    /// `None` means the emulator runs, and then every image is welcome: our own
    /// BDOS is underneath the drive and reads the image's filesystem directly,
    /// so no board has to agree to anything.
    board: Option<&'static str>,
    /// The booted image's size, which is what [`Self::slot`] asks the board by.
    boot_len: Option<u64>,
    /// The file name of the disk that will boot, when one will.
    ///
    /// **Slot 0 is reserved, not empty, and no screen used to say by what.** The
    /// mount screens showed `(drive folder)` in the first row beside a note
    /// reading "the booted disk is here" -- two statements that contradict each
    /// other, neither naming the disk. An operator reasonably reads the row as a
    /// place their boot disk should have appeared (reported 2026-08-21).
    boot_name: Option<String>,
}

impl MountContext {
    /// Resolve from the configuration.
    ///
    /// **`auto` has to be resolved, not passed on.** `resolve_machine` does not
    /// know the sentinel and falls back to the default Altair, so asking it
    /// about boards under the default setting answered for an Altair whatever
    /// disk was booting — which would have hidden every z80pack and Cromemco
    /// image from the mount screens. The boot itself resolves `auto` by reading
    /// the disk (`detect::machine_for`), so this does too, through the same
    /// cache the bootability answer uses.
    pub fn resolve(transfer_dir: &str, boot_image: &str, machine: &str) -> MountContext {
        let target = boot_target(transfer_dir, boot_image);
        let naming = target.slot_naming();
        let (boot_len, machine) = match &target {
            BootTarget::Image(path) => (
                std::fs::metadata(path).ok().map(|md| md.len()),
                machine_for_image(path, machine),
            ),
            _ => (None, machine.to_string()),
        };
        let board = boot_len
            .and_then(|len| super::boot_machine::BootMachine::board_for(Some(&machine), len));
        // The *resolved* target's name, never the configured string: a key
        // naming a deleted image runs the emulator, and a screen that named it
        // as the occupant of slot 0 would be describing a machine nobody gets.
        let boot_name = match &target {
            BootTarget::Image(path) => {
                path.file_name().map(|n| n.to_string_lossy().to_string())
            }
            _ => None,
        };
        MountContext { naming, machine, board, boot_len, boot_name }
    }

    /// Can an image of this size be reached by whatever is going to run?
    ///
    /// Under the emulator, yes — always. Under a booted disk, only if it lands
    /// on the same board the guest is driving; anything else is a disk the
    /// guest cannot see however correctly we mount it.
    pub fn accepts(&self, image_len: u64) -> bool {
        match self.board {
            // Either the emulator runs -- where every image is welcome -- or a
            // disk is booting on a board we could not name.  Both answer "yes":
            // the second is a state we cannot judge, and a filter that hides
            // disks on a hunch is worse than one that does not fire.
            None => true,
            Some(want) => {
                super::boot_machine::BootMachine::board_for(Some(&self.machine), image_len)
                    == Some(want)
            }
        }
    }

    /// What to call slot `slot`.
    ///
    /// Named after the *booted* disk's board rather than the image being placed,
    /// which is the whole point: every image this context accepts is on that one
    /// board, so the column reads in one vocabulary instead of one per row.
    pub fn slot(&self, slot: u8) -> String {
        match self.naming {
            SlotNaming::Drives => format!("{}:", (b'A' + slot) as char),
            SlotNaming::Boards => {
                super::boot_machine::BootMachine::slot_label(Some(&self.machine), self.board_len(), slot)
                    .unwrap_or_else(|| format!("slot {slot}"))
            }
        }
    }

    /// A size on the booted disk's board, for [`Self::slot`].
    ///
    /// `slot_label` asks by size because that is how a board is chosen
    /// everywhere else; here the board is already known, so any size it takes
    /// answers the same question. The booted image's own is the honest one.
    fn board_len(&self) -> u64 {
        self.boot_len.unwrap_or(0)
    }

    /// The board the booted disk is on, for a surface with room to name it.
    pub fn board(&self) -> Option<&'static str> {
        self.board
    }

    /// The disk that will boot, by name, when one will.
    ///
    /// For the surfaces that show slot 0 as a control: an empty selector reads
    /// as a free drive, and this is what it is holding instead.
    pub fn boot_disk_name(&self) -> Option<&str> {
        self.boot_name.as_deref()
    }

    /// What slot 0 should say on a mount screen, in one sentence.
    ///
    /// **One text for three surfaces**, like [`super::uart::UART_CHOICES`] and
    /// the printer's labels: telnet, the web UI and the desktop all show this
    /// row, and the previous wording ("the booted disk is here") was written
    /// three times and named the disk in none of them.
    ///
    /// `None` when the emulator runs, where slot 0 is an ordinary drive.
    pub fn boot_slot_note(&self) -> Option<String> {
        if !self.booting() {
            return None;
        }
        Some(match &self.boot_name {
            Some(name) => format!("{name} boots here"),
            // Booting, but the name could not be read: still not a free drive.
            None => "the booted disk is here".to_string(),
        })
    }

    /// Is a disk booting at all?
    ///
    /// From the naming, not from whether a board could be named: those are
    /// different questions, and answering this one with `board.is_some()` let
    /// them disagree.  A disk that boots on a board this context could not
    /// identify is still a boot, and a screen that called it "CHOOSE A DRIVE"
    /// while another called it a slot is the split this type exists to prevent.
    pub fn booting(&self) -> bool {
        self.naming == SlotNaming::Boards
    }
}

/// Why an image mounted on `A:` is not reachable while a disk boots.
///
/// **Said where the mount is made, not only where the boot starts.** The mount
/// screens let an image be put on `A:` with a disk set to boot, and the only
/// notice was a line printed by `plan_boot_disks` at boot time -- by which point
/// the operator has left the screen where they could have chosen differently.
/// Slot 0 belongs to the disk being booted, so anything else there is held but
/// unreachable.
// A hyphen, not an em dash: these strings reach `send()` on a telnet session,
// where an ASCII terminal writes the UTF-8 bytes untouched and a three-byte
// dash counts as one column. `test_no_operator_facing_string_is_non_ascii`
// holds it, and caught this one.
pub const BEHIND_BOOT_DISK: &str = "behind the boot disk - the guest cannot reach it";

/// The same warning inside a PETSCII row.
///
/// **Two spellings, one place.** A C64 line is 40 columns and this note shares
/// its row with an indent, so the long form cannot be used there -- but a second
/// wording invented at the call site is how two surfaces come to describe one
/// state differently. Both live here, next to each other, where a change to one
/// is visibly a change to the other.
pub const BEHIND_BOOT_DISK_SHORT: &str = "behind the boot disk - unreachable";

/// What CP/M is *actually* going to run for a given configuration.
///
/// `cpm_boot_image` is a preference, not an outcome. An operator can delete the
/// image it names, or hand-edit the key to something that could never be a
/// filename, and in both cases the gateway runs the emulator rather than
/// refusing to open CP/M at all — the setting is about which machine to run,
/// and losing an image should lose the boot, not the whole feature.
///
/// Every screen that describes the machine has to describe *that* answer, and
/// for a while they did not: they read the configured string, so a stale key
/// named the drive rows for a board while our BDOS was what started, and
/// [`mount_refuses_writes`] gave the host's verdict where our BDOS's applied —
/// an image our BDOS write-protects showed no `(R/O)` marker until the write
/// was refused.
///
/// So the resolution lives here, once, and a [`SlotNaming`] can only be had
/// **from a resolved target**: there is deliberately no function that will
/// answer the question from the string alone. This project has twice shipped a
/// rule written in two places and held in one; this is the compiler holding it
/// instead.
///
/// **Nothing here logs.** A screen resolves this every time it draws — and the
/// desktop redraws on a 250 ms heartbeat whether or not anybody is touching it
/// — so a fallback that announced itself from here would fill the console log
/// with one operator's config typo, four lines a second, forever. The emulator
/// entry point resolves it once per session and does the announcing, which is
/// also the only place with a session to announce it *to*.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootTarget {
    /// Nothing is configured to boot — the emulator, as asked for.
    Emulator,
    /// A value that could never name a file in `CPM/images`; the emulator runs.
    UnsafeName(String),
    /// A name that is not in `CPM/images` any more; the emulator runs.
    Missing(String),
    /// The disk is there and carries no boot program; the emulator runs.
    ///
    /// The emulator rather than a boot that fails: the three other fallbacks all
    /// start the emulator, and starting a session that can only print an error
    /// would make this the one setting whose failure costs the operator CP/M
    /// entirely. The screens mark it, which is where the operator finds out.
    NotBootable(String),
    /// The image that will be booted.
    Image(PathBuf),
}

impl BootTarget {
    /// The image that will be booted, if one will be.
    ///
    /// By value because the one caller is starting a boot session with it, and
    /// a borrowing twin would be a second way to ask the same question with
    /// nobody asking it.
    pub fn into_image(self) -> Option<PathBuf> {
        match self {
            BootTarget::Image(path) => Some(path),
            _ => None,
        }
    }

    /// What a mount slot is called on the machine this target starts.
    ///
    /// Both fallbacks name drives, because both of them run the emulator. That
    /// is the whole point of routing the question through here.
    pub fn slot_naming(&self) -> SlotNaming {
        match self {
            BootTarget::Image(_) => SlotNaming::Boards,
            _ => SlotNaming::Drives,
        }
    }
}

/// Resolve `cpm_boot_image` against the images folder under `transfer_dir`.
///
/// **Resolve once per screen and pass the answer down.** It was two syscalls —
/// [`super::layout::cpm_dir`] canonicalizes the container and the image is
/// `stat`ed — and since it also asks [`image_can_boot`] it is two syscalls plus
/// a cache lookup, or, on the *first* call for a given file, a full read and a
/// cold start. That is bounded (the answer is cached against the file's
/// identity) but it is no longer free, so the rule this doc always stated now
/// matters more, not less: one of these per screen, never one per row. The
/// desktop's is on a panel that redraws four times a second forever.
///
/// Callers on an async runtime should reach it through `spawn_blocking` for the
/// same reason the download beside them does — the first call can read a 4.9 MB
/// hard disk, and a runtime worker blocked on that stalls every other session's
/// timers.
pub fn boot_target(transfer_dir: &str, boot_image: &str) -> BootTarget {
    let name = boot_image.trim();
    if name.is_empty() {
        return BootTarget::Emulator;
    }
    if !super::image::is_safe_image_name(name) {
        return BootTarget::UnsafeName(name.to_string());
    }
    let path = super::image::images_dir(&super::layout::cpm_dir(transfer_dir)).join(name);
    if std::fs::metadata(&path).is_err() {
        return BootTarget::Missing(name.to_string());
    }
    // Cached, so the "two syscalls" this doc promises hold from the second call
    // on — and the first is the one that reads the disk, which is the same read
    // the boot itself would do a moment later.
    if !image_can_boot(&path) {
        return BootTarget::NotBootable(name.to_string());
    }
    BootTarget::Image(path)
}

/// Name slot `slot`, for an image of `image_len` bytes if that is known.
///
/// **Short on purpose** — `drive 1`, not `drive 1 (MITS 88-DCDD floppy)`. These
/// rows are read on a 40-column PETSCII screen as often as an 80-column one, and
/// the first draft of this put the board name in here: at 31 characters it made
/// a 45-character row and truncated the *filename* away, which is the one thing
/// on the line the operator needs. The board is available separately from
/// [`MountContext::board`], for the surfaces that have room.
///
/// `image_len` is `None` for an empty slot or one whose file cannot be read;
/// under [`SlotNaming::Boards`] the board is chosen by the image's *size*, so
/// without it the honest answer names the number and stops there.
pub fn slot_name(naming: &SlotNaming, slot: u8, image_len: Option<u64>) -> String {
    match naming {
        SlotNaming::Drives => format!("{}:", (b'A' + slot) as char),
        SlotNaming::Boards => match image_len.and_then(|len| slot_label(len, slot)) {
            Some(label) => label,
            None => format!("slot {slot}"),
        },
    }
}

/// What the board taking an image this size calls slot `slot`.
pub fn slot_label(image_len: u64, slot: u8) -> Option<String> {
    super::boot_machine::BootMachine::slot_label(None, image_len, slot)
}

/// Will this mount refuse writes, for whichever of the two runs CP/M?
///
/// The same split as [`SlotNaming`], and for the same reason: the emulator and
/// a booted disk are asking different questions of the same file.
///
/// * **Under the emulator** the answer is `Mount::read_only` — our BDOS is
///   underneath the drive, so its own doubts about the format or the directory
///   are exactly what decides whether a write is safe.
/// * **Under a booted disk** it is `Mount::host_read_only` and nothing else.
///   The guest owns the format and writes whole sectors; our record-placer is
///   not in the path and has no standing to protect a disk it never touches.
///   What is left is the write-protect tab.
///
/// One function because it is one rule and there are three screens plus the
/// boot planner — the shape of defect this project has produced more than once
/// is a rule stated separately on each surface and updated on some of them.
pub fn mount_refuses_writes(
    naming: &SlotNaming,
    mount: &super::image::registry::Mount,
) -> bool {
    match naming {
        SlotNaming::Drives => mount.read_only,
        SlotNaming::Boards => mount.host_read_only,
    }
}

/// The `cpm_boot_backspace` value that hands a booted guest BS (0x08).
pub const BACKSPACE_ERASE: &str = "backspace";

/// The `cpm_boot_backspace` value that hands the key through as the terminal
/// sent it, which on the disks that want it is the Teletype rubout.
pub const BACKSPACE_RUBOUT: &str = "rubout";

/// What `cpm_boot_backspace` is when nothing says otherwise.
pub const DEFAULT_BACKSPACE: &str = BACKSPACE_ERASE;

/// The choices for `cpm_boot_backspace`, `(value, label)`.
///
/// One list for all three configuration screens *and* the telnet boot picker,
/// the way [`boot_choices`] serves the boot image and [`super::uart::UART_CHOICES`]
/// serves the virtual-modem port — four surfaces that must offer the same two
/// answers and describe them the same way.
///
/// The labels lead with the visible symptom rather than the byte, because the
/// operator meeting this in the boot picker is choosing between two behaviours
/// they can see, not between 0x08 and 0x7F.
pub const BACKSPACE_CHOICES: &[(&str, &str)] = &[
    (BACKSPACE_ERASE, "Backspace erases (most disks)"),
    (BACKSPACE_RUBOUT, "Rubout, as the disk expects (CP/M 1.x)"),
];

/// What to show for the current `cpm_boot_backspace` setting.
///
/// Reads through [`backspace_erases`] rather than matching the string, so a
/// hand-edited typo is *displayed* as the behaviour the gateway is actually
/// giving it — the failure this exists to prevent is a screen that agrees with
/// the config file and disagrees with the machine.
pub fn backspace_label(value: &str) -> &'static str {
    let want = if backspace_erases(value) { BACKSPACE_ERASE } else { BACKSPACE_RUBOUT };
    BACKSPACE_CHOICES.iter().find(|(v, _)| *v == want).map(|(_, l)| *l).unwrap_or(want)
}

/// Whether `value` means "translate the key to BS".
///
/// Anything unrecognised reads as the default rather than refusing: this is
/// hand-editable in `egateway.conf`, and a typo that silently picked the *other*
/// behaviour would be blamed on the disk.
pub fn backspace_erases(value: &str) -> bool {
    !value.trim().eq_ignore_ascii_case(BACKSPACE_RUBOUT)
}

/// Where the bootstrap puts the boot sector, and enters it.
pub const BOOT_LOAD_ADDR: u16 = 0x0000;

/// Offset of the 128 data bytes inside a boot-region sector.
pub const BOOT_DATA_OFFSET: usize = 3;

/// Bytes the bootstrap transfers per sector.
pub const BOOT_DATA_LEN: usize = 128;

/// Sectors the bootstrap loads.
///
/// One is not enough. The CP/M loader in sector 0 calls a read routine at
/// `0092h`, past the 128 bytes that sector holds; the MITS loader copies `0FCh`
/// bytes starting at `0013h`, so it needs `010Fh` of them. Four sectors — 512
/// bytes — covers both with room to spare, and a disk whose loader is shorter
/// simply stops early.
pub const BOOT_SECTORS: u8 = 4;

/// Physical sectors between consecutive boot chunks.
///
/// Fixed at 2, and measured rather than assumed. It was briefly tempting to
/// try several steps and keep whichever produced a running machine, on the
/// theory that the step reflects how fast the loader that wrote the disk could
/// read. It does not need to be guessed: dumping track 0 of an Altair DOS disk
/// shows its loader copying 0FCh bytes from 0013h to 2C00h, and the code that
/// continues it — full of `2Cxx` addresses — sits in physical sectors 2 and 4.
/// The MITS disks are laid out exactly like the CP/M ones.
///
/// Guessing was also actively harmful: run four candidates and take the first
/// that prints anything, and a *wrong* layout that scribbles a few bytes at a
/// console beats the right one that is still loading. That is precisely what
/// happened — step 4 "won" for five disks and produced nothing but noise.
pub const BOOT_INTERLEAVE: u8 = 2;

/// The answer to "would this image boot", and whose fault it is if not.
///
/// **Two failures that look identical on a screen and are not.** A disk with no
/// boot program cannot boot on any machine in any configuration, so offering it
/// wastes the operator's keystrokes. A disk this machine has no *board* for is a
/// `cpm_boot_machine` setting away from working, so withholding it would hide a
/// disk that is fine and leave nothing to act on.
///
/// One type rather than each caller re-deriving it from a `BootError`: three
/// places asked this question, all three collapsed it to `Err(_) => no`, and all
/// three were therefore wrong in the same way at the same time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Bootability {
    /// The cold start reached an entry point.
    Boots,
    /// The disk carries nothing that could be a boot program.
    NoBootProgram(String),
    /// No board on the configured machine can start this image.
    NoBoardForIt(String),
}

impl Bootability {
    /// Should a boot list offer this image?
    ///
    /// Yes for a board mismatch: that failure is worth reaching, because the
    /// boot names the boards this machine has and the operator can change it.
    pub fn offer(&self) -> bool {
        !matches!(self, Bootability::NoBootProgram(_))
    }
}

/// Why a boot did not happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootError {
    /// No disk in the drive the bootstrap was pointed at.
    NoDisk(u8),
    /// The image could not supply the boot sector.
    Unreadable(String),
    /// Sector 0 never came round.
    ///
    /// On real hardware this cannot happen — the disk turns. Here it means the
    /// controller is not advancing, which is the failure the rotation model
    /// exists to prevent, so it is reported as itself rather than left to look
    /// like a hung guest.
    NeverPositioned,
    /// The sector holds nothing that could be code.
    NotBootable,
    /// The disk is on a controller that cannot cold-start one yet.
    ///
    /// Distinct from an empty drive, which is what this used to report and is
    /// a different thing entirely — the disk is there and the controller can
    /// read it, but nothing here knows the sequence its boot PROM would run.
    NoBootstrap,
}

impl std::fmt::Display for BootError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BootError::NoDisk(d) => {
                write!(f, "drive {d} is empty - put a disk image in it first")
            }
            BootError::Unreadable(e) => write!(f, "could not read the boot sector: {e}"),
            BootError::NeverPositioned => {
                write!(f, "the disk never presented sector 0 - the controller is not turning")
            }
            BootError::NotBootable => write!(
                f,
                "this image has no boot sector - it is data, not a system disk"
            ),
            BootError::NoBootstrap => write!(
                f,
                "this disk is on a controller that cannot cold-start one yet"
            ),
        }
    }
}

// WHERE THIS STANDS, for whoever picks it up next.
//
// Altair CP/M boots to its `A>` prompt, and so do the eight other CP/M images
// in the sample set.  The MITS operating systems — Altair DOS, Disk BASIC and
// Time Sharing BASIC — load and run their loaders too.
//
// The diagnostics that got it there are still in the boot test and are worth
// reaching for before theorising: `CPM_BOOT_TRACE` prints the first PCs,
// `CPM_BOOT_DISASM=addr:count` disassembles what actually landed in memory,
// `CPM_BOOT_STEP` forces a different sector step, and the test prints a
// per-port access count.  Every fault found so far was visible in one of them
// within a minute; none was found by reasoning about what the code ought to do.
//
// Two things that are settled, so they are not re-litigated:
//
//   * The sector step is 2 for every Altair disk here, CP/M and MITS alike —
//     see BOOT_INTERLEAVE.  It is not worth autodetecting.
//   * A guest that loads and stays silent is usually looking at the wrong
//     console, not misreading the disk.  MITS software picks its terminal
//     board from the front-panel sense switches on port FFh; see
//     DEFAULT_SENSE_SWITCHES in boot_machine.rs.

/// How many position-register reads to allow before giving up.
///
/// Two per sector, so a full revolution is `2 * sectors`. A generous multiple
/// of that means a working controller always succeeds and a broken one fails
/// quickly instead of spinning.
const MAX_POLLS: usize = 512;

/// Run the bootstrap: leave the boot sector in memory and return the entry
/// point.
///
/// `fetch` supplies a physical sector; the controller asks and the caller
/// reads, so file access stays where its bounds checks are. `store` receives
/// the payload and the address to put it at.
pub fn cold_boot<F, S>(
    dcdd: &mut Dcdd,
    drive: u8,
    fetch: F,
    store: S,
) -> Result<u16, BootError>
where
    F: FnMut(u8, u8, u8) -> Result<Vec<u8>, String>,
    S: FnMut(u16, &[u8]),
{
    cold_boot_with_step(dcdd, drive, BOOT_INTERLEAVE, fetch, store)
}

/// Cold-boot using a specific sector step.
///
/// The step is a parameter only so that a disk which will not boot can be tried
/// another way without editing the code — see [`BOOT_INTERLEAVE`], which is
/// what every real caller passes.
pub fn cold_boot_with_step<F, S>(
    dcdd: &mut Dcdd,
    drive: u8,
    step: u8,
    mut fetch: F,
    mut store: S,
) -> Result<u16, BootError>
where
    F: FnMut(u8, u8, u8) -> Result<Vec<u8>, String>,
    S: FnMut(u16, &[u8]),
{
    if !dcdd.has_disk(drive) {
        return Err(BootError::NoDisk(drive));
    }

    // What the PROM does: select the drive, put the head down, wait for
    // sector 0, read it.
    dcdd.port_out(0x08, drive & 0x0F);
    dcdd.port_out(0x09, 0x04); // head load

    let mut positioned = false;
    for _ in 0..MAX_POLLS {
        let (v, _) = dcdd.port_in(0x09);
        if v == 0xFF {
            continue;
        }
        let sector = (v >> 1) & 0x1F;
        let sector_true = v & 0x01 == 0;
        if sector == 0 && sector_true {
            positioned = true;
            break;
        }
    }
    if !positioned {
        return Err(BootError::NeverPositioned);
    }

    // Reading the data port asks for the sector; satisfy that, then take the
    // bytes straight from the buffer rather than clocking 137 port reads.
    let (_, req) = dcdd.port_in(0x0A);
    let (track, sector) = match req {
        Request::Read { track, sector, .. } => (track, sector),
        _ => (0, 0),
    };
    let raw = fetch(drive, track, sector).map_err(BootError::Unreadable)?;
    if raw.len() < BOOT_DATA_OFFSET + BOOT_DATA_LEN {
        return Err(BootError::Unreadable(format!(
            "boot sector is {} bytes, expected {SECTOR_LEN}",
            raw.len()
        )));
    }
    dcdd.sector_loaded(drive, &raw);

    let payload = &raw[BOOT_DATA_OFFSET..BOOT_DATA_OFFSET + BOOT_DATA_LEN];
    if !looks_bootable(payload) {
        return Err(BootError::NotBootable);
    }
    // The rest of the loader, from the interleaved sectors that follow.  A
    // sector that cannot be read stops the copy rather than failing the boot:
    // a shorter loader is legitimate, and the guest will notice long before we
    // could.
    for i in 1..BOOT_SECTORS {
        let sec = sector + i * step.max(1);
        let Ok(more) = fetch(drive, track, sec) else { break };
        if more.len() < BOOT_DATA_OFFSET + BOOT_DATA_LEN {
            break;
        }
        store(
            BOOT_LOAD_ADDR + i as u16 * BOOT_DATA_LEN as u16,
            &more[BOOT_DATA_OFFSET..BOOT_DATA_OFFSET + BOOT_DATA_LEN],
        );
    }
    // Close the transfer the bootstrap opened.  Leaving it open holds the
    // "safe to move the head" status bit low, and the first thing a boot
    // sector does is seek to track 0 — so the guest would spin at its very
    // first instruction that touches the drive.
    dcdd.end_transfer(drive);
    store(BOOT_LOAD_ADDR, payload);
    Ok(BOOT_LOAD_ADDR)
}

/// Does this payload look like a boot sector rather than data?
///
/// A cheap sanity check, not a verifier. It exists so that booting a data disk
/// says so instead of running whatever bytes happened to be there — an 8080
/// turned loose on text will do something, and what it does is never useful.
///
/// The test is deliberately weak in one direction: it rejects only what cannot
/// be a program. Three shapes are refused — a sector that is **mostly one
/// repeated byte** (an erased sector, or a data disk's short header and then
/// padding), one that is **entirely printable text**, and one **too short to
/// judge**. Anything else is allowed through, because deciding what is *really*
/// a program is not a job a heuristic can do.
///
/// The bias is intentional and worth keeping: refusing a disk that would have
/// run is a fault the operator cannot work around, while letting one through
/// only leaves the old behaviour of running it and going quiet.
pub fn looks_bootable(payload: &[u8]) -> bool {
    if payload.len() < 8 {
        return false;
    }
    // **Mostly one repeated byte is padding, not a program.** An erased or
    // unformatted sector is the obvious case — all `00h`, or all `E5h` — but the
    // one that mattered is subtler: a *data* disk's first sector holds a short
    // header and then nothing. `DISK0B` is "Time Sharing Basic V2 programs" and
    // carries its volume label, `VOL±TS2FILES`, followed by 112 zero bytes;
    // `DISK0F` is "Altair Mini-Disk DOS programs" and carries two stray bytes
    // and 126 zeros. Both used to pass this check, so we ran them: DISK0B
    // executes its own label as instructions, DISK0F NOPs its way off into
    // cleared memory. Either way the machine goes quiet, which reads like a
    // disk we cannot boot rather than a disk with no boot program on it.
    //
    // **The four-fifths is measured, and the first two thresholds reasoned for
    // it were both wrong.** Across every image in the Altair and z80pack
    // collections, taking the payload each controller really extracts: the
    // disks that boot run from 5% to **63%** one-byte (z80pack's `mpm-2`, whose
    // loader is short and zero-padded), and the ones with no boot program are
    // 89%, 98% and 100%. A half-way rule would have killed the three Altair
    // hard disks; a trailing-zero-run rule would have killed `mpm-2`. Four
    // fifths sits 17 points clear of the highest disk that works and 9 below
    // the lowest that does not.
    //
    // Erring lenient is deliberate: too strict refuses a disk that would have
    // run, while too lenient only leaves the old behaviour, which is what this
    // is improving on rather than depending on.
    let mut counts = [0usize; 256];
    let mut top = 0usize;
    for &b in payload {
        counts[b as usize] += 1;
        top = top.max(counts[b as usize]);
    }
    if top > payload.len() * 4 / 5 {
        return false;
    }
    // Entirely printable text is a data sector, not code.
    let printable = payload
        .iter()
        .filter(|&&b| (0x20..0x7F).contains(&b) || b == b'\r' || b == b'\n')
        .count();
    if printable == payload.len() {
        return false;
    }
    true
}

#[cfg(test)]
pub(crate) mod tests {

    /// The emulator names its own drives; a booted machine names a board's.
    ///
    /// One list underneath either way — this is a naming decision, not a second
    /// `cpm_mounts`. What it is protecting is a promise: under the emulator our
    /// BDOS is beneath `B:`, so `B:` means what it says; under a booted disk the
    /// slot is a number on a board and the guest's own BIOS decides what it can
    /// reach, so a letter there would be us answering for the guest.
    #[test]
    fn test_a_slot_is_named_for_whatever_cpm_is_set_to_run() {
        use super::{slot_name, BootTarget, SlotNaming};
        assert_eq!(BootTarget::Emulator.slot_naming(), SlotNaming::Drives);
        assert_eq!(
            BootTarget::Image("altair8_cpm22.dsk".into()).slot_naming(),
            SlotNaming::Boards
        );

        // The emulator's names never mention a board — its drives are ours.
        assert_eq!(slot_name(&SlotNaming::Drives, 0, None), "A:");
        assert_eq!(slot_name(&SlotNaming::Drives, 1, Some(337_568)), "B:");
        assert_eq!(slot_name(&SlotNaming::Drives, 15, None), "P:");
    }

    /// A booted slot is named by the board the image's **size** puts it on, and
    /// the two kinds of disk must not come out with the same word.
    ///
    /// This is the fact the screens could not previously show, and the one that
    /// cost a morning: mount a floppy while booting a hard disk and it lands on
    /// the 88-DCDD perfectly well, while the guest is driving the 88-HDSK.
    /// Everything works; nothing is reachable.
    #[test]
    fn test_a_booted_slot_names_the_board_the_size_chooses() {
        use super::{slot_name, SlotNaming};
        use crate::cpm::boot_machine::BootMachine;

        // Measured sizes, not invented ones: an Altair 8" floppy and one
        // 88-HDSK platter, both straight out of the media tables.
        let floppy = BootMachine::bootable_media()
            .into_iter()
            .find(|m| m.label.contains("88-DCDD") || m.label.to_lowercase().contains("altair"))
            .map(|m| m.bytes)
            .expect("the floppy medium is in the table");
        let platter = 4_988_928u64;

        let f = slot_name(&SlotNaming::Boards, 1, Some(floppy));
        let h = slot_name(&SlotNaming::Boards, 1, Some(platter));
        assert_eq!(f, "drive 1", "a floppy board has drives");
        // Two coordinates, because the 88-HDSK's slots are not a flat row: a
        // drive carries four platters and each is one image, so slot 1 is the
        // *first* drive's *second* platter — which is what Altair Hard Disk
        // BASIC calls disk 1.
        assert_eq!(h, "unit 0.1", "an 88-HDSK slot is a platter on a drive");
        assert_eq!(slot_name(&SlotNaming::Boards, 5, Some(platter)), "unit 1.1");
        assert_ne!(f, h, "the two boards must not read the same at the same slot");

        // The board itself is a separate question, asked by the surfaces with
        // room for the answer — see `test_a_slot_name_leaves_room_for_the_filename`
        // for why it is not in the name.  Asked through `board_for` now: the
        // `slot_board(len)` wrapper went when the screens stopped naming a board
        // from each *row's* image and started asking the booted disk's context,
        // which is what stops one column reading `unit 0.1 on the 88-DCDD`.
        let board = |len| super::super::boot_machine::BootMachine::board_for(None, len);
        assert!(
            board(platter).is_some_and(|b| b.contains("88-HDSK")),
            "the board is still nameable: {:?}",
            board(platter)
        );
        assert_ne!(board(floppy), board(platter));

        // A size no board takes, and an unknown size, both fall back to the bare
        // number — never to a drive letter, which would be the one answer that
        // says something untrue.  The board reads as absent rather than guessed.
        assert_eq!(slot_name(&SlotNaming::Boards, 2, Some(4_242)), "slot 2");
        assert_eq!(board(4_242), None);
        assert_eq!(slot_name(&SlotNaming::Boards, 3, None), "slot 3");
    }

    /// **A slot name has to leave room for the filename beside it.**
    ///
    /// The regression this pins was mine, made in the commit that introduced
    /// these names: the board went *into* `slot_name`, which at
    /// `unit 1 (MITS 88-HDSK hard disk)` is 31 characters and made a 45-character
    /// row on a 40-column PETSCII screen — truncating away the filename, the one
    /// thing on the line the operator actually needs. The board lives in
    /// `slot_board` now, for the surfaces that have room.
    ///
    /// The budget is the narrowest real row: the telnet disks screen indents
    /// three, then prints the slot, a space, and up to 28 characters of
    /// filename, inside 40 columns.
    #[test]
    fn test_a_slot_name_leaves_room_for_the_filename() {
        use super::{slot_name, BootTarget, SlotNaming};
        use crate::cpm::boot_machine::BootMachine;

        const INDENT: usize = 3;
        const FILENAME: usize = 28; // what the PETSCII row allows
        const PETSCII_COLS: usize = 40;

        // Every medium this gateway can boot, so a new board cannot slip in
        // with a long word and quietly overflow the row.
        let mut sizes: Vec<u64> = BootMachine::bootable_media().iter().map(|m| m.bytes).collect();
        sizes.push(4_242); // and a size no board takes
        assert!(sizes.len() > 3, "the media table looks empty: {sizes:?}");

        for naming in [SlotNaming::Drives, SlotNaming::Boards] {
            for &len in &sizes {
                for slot in 0..crate::cpm::NUM_DRIVES {
                    let name = slot_name(&naming, slot, Some(len));
                    let row = INDENT + name.chars().count() + 1 + FILENAME;
                    assert!(
                        row <= PETSCII_COLS,
                        "{naming:?} slot {slot} for {len} bytes is {name:?} — a \
                         {row}-column row on a {PETSCII_COLS}-column screen"
                    );
                }
            }
        }
        // And the naming helper agrees with what the rows are built from.
        assert_eq!(BootTarget::Emulator.slot_naming(), SlotNaming::Drives);
    }
    use super::super::dcdd::{Disk, Geometry};
    use super::*;

    /// The first bytes of a real Altair CP/M boot sector: LXI SP,0DF00h / DI /
    /// XRA A / OUT 08h / IN 08h / ANI 08h / JNZ 0007h.
    const REAL_BOOT_START: &[u8] = &[
        0x31, 0x00, 0xDF, 0xF3, 0xAF, 0xD3, 0x08, 0xDB, 0x08, 0xE6, 0x08, 0xC2, 0x07, 0x00,
    ];

    /// A synthetic boot sector that is *dense*, like a real one.
    ///
    /// It used to be the fourteen real opcodes and then zeros, which is 89% one
    /// byte — the signature of a data disk's header-and-padding, and now
    /// refused as such. That made four tests fail at once when
    /// [`looks_bootable`] learned to tell those apart, and the tests were the
    /// thing that was wrong: no boot sector this project has ever measured is
    /// anywhere near that sparse (the emptiest that really boots is 63%).
    /// Repeating the real bytes gives it the byte distribution of actual code.
    fn boot_sector() -> Vec<u8> {
        let mut s = vec![0u8; SECTOR_LEN];
        s[0] = 0x80; // track 0, high bit set as the controller writes it
        let data = &mut s[BOOT_DATA_OFFSET..BOOT_DATA_OFFSET + BOOT_DATA_LEN];
        for (i, b) in data.iter_mut().enumerate() {
            *b = REAL_BOOT_START[i % REAL_BOOT_START.len()];
        }
        // The entry path still depends on the first instructions being real.
        data[..REAL_BOOT_START.len()].copy_from_slice(REAL_BOOT_START);
        s
    }

    /// A whole 8" image that really cold-starts, for the tests that need a disk
    /// rather than a stand-in for one.
    ///
    /// Those tests wrote an eight-byte file and asserted it would boot. That was
    /// fine while the only question asked of an image was whether it existed;
    /// once the boot lists began cold-starting candidates, an eight-byte file
    /// was correctly judged unbootable and the tests were asserting something
    /// untrue. Building the real thing is cheaper than a seam here, and it makes
    /// "a disk that is really there carries no marker" a fact rather than a
    /// property of a stub.
    ///
    /// The boot sectors go where the controller's own arithmetic puts them, at
    /// [`BOOT_INTERLEAVE`], so this is laid out by the same rules the cold start
    /// reads it back with.
    pub(crate) fn bootable_image() -> Vec<u8> {
        let geom = Geometry::EIGHT_INCH;
        let mut image = vec![0u8; geom.image_len() as usize];
        for i in 0..BOOT_SECTORS {
            let at = geom.offset(0, i * BOOT_INTERLEAVE) as usize;
            image[at..at + SECTOR_LEN].copy_from_slice(&boot_sector());
        }
        image
    }

    fn with_disk() -> Dcdd {
        let mut c = Dcdd::new();
        c.insert(0, Disk { geometry: Geometry::EIGHT_INCH, read_only: false });
        c
    }

    /// The list every configuration screen renders.
    ///
    /// The emulator must be first and must be the empty value: first because it
    /// is what CP/M has always meant here, and empty because a config file
    /// written before this key existed has to keep behaving the same way.
    #[test]
    fn test_the_boot_choices_start_with_the_emulator() {
        let dir = std::env::temp_dir().join("egw_boot_choices_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(super::super::image::IMAGES_DIR)).unwrap();

        let choices = boot_choices_by(&dir, |_| true);
        assert_eq!(choices.len(), 1, "an empty folder still offers the emulator");
        assert_eq!(choices[0].0, BOOT_EMULATOR);
        assert!(choices[0].0.is_empty(), "the emulator is the empty setting");
        assert_eq!(choices[0].1, BOOT_EMULATOR_LABEL);

        let images = dir.join(super::super::image::IMAGES_DIR);
        std::fs::write(images.join("altair8_cpm.dsk"), [0u8; 8]).unwrap();
        std::fs::write(images.join("games.dsk"), [0u8; 8]).unwrap();
        std::fs::write(images.join("readme.txt"), b"not an image").unwrap();

        let choices = boot_choices_by(&dir, |_| true);
        let values: Vec<&str> = choices.iter().map(|(v, _)| v.as_str()).collect();
        assert_eq!(
            values,
            vec!["", "altair8_cpm.dsk", "games.dsk"],
            "the emulator first, then the disks, sorted — and no readme"
        );
        assert!(choices[1].1.starts_with("Boot "), "{}", choices[1].1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **The mount screens follow what is going to run.**
    ///
    /// The defect this closes was reported as "I mounted a disk on B: and the
    /// booted disk cannot see it, and B: says Bad Sector". Nothing was broken:
    /// the board an image lands on is chosen by its *size*, so a floppy mounted
    /// beside a booted hard disk went to the 88-DCDD while the guest talked only
    /// to the 88-HDSK — mounted, correct, unreachable — and the guest's own B:
    /// was an empty platter.
    ///
    /// Two halves, and both are checked here because either alone still leaves
    /// the operator guessing: an image that cannot be reached is not offered,
    /// and a slot is named by the board it will really be on.
    #[test]
    fn test_the_mount_screens_follow_what_will_run() {
        let dir = std::env::temp_dir().join("egw_mount_context_test");
        let _ = std::fs::remove_dir_all(&dir);
        // Under `CPM/`, because that is where `boot_target` looks.
        let images = super::super::image::images_dir(&super::super::layout::cpm_dir(
            &dir.to_string_lossy(),
        ));
        std::fs::create_dir_all(&images).unwrap();

        let floppy = bootable_image();
        std::fs::write(images.join("floppy.dsk"), &floppy).unwrap();
        let hd_len = super::super::boot_machine::BootMachine::bootable_media()
            .into_iter()
            .find(|m| m.label.contains("88-HDSK"))
            .expect("the hard disk is a bootable medium")
            .bytes;

        // **The emulator takes anything**: our BDOS reads the image's own
        // filesystem, so no board has to agree to it, and the slots are drives.
        let emu = MountContext::resolve(&dir.to_string_lossy(), "", "auto");
        assert!(emu.accepts(floppy.len() as u64));
        assert!(emu.accepts(hd_len));
        assert!(!emu.booting());
        assert_eq!(emu.slot(0), "A:");
        assert_eq!(emu.slot(1), "B:");

        // **Booting the floppy**: a hard disk is on a board this guest never
        // reads, so it is not offered -- and the slots stop being drive letters.
        //
        // The floppy is a real bootable image because `boot_target` cold-starts
        // it now; a stub would resolve to `NotBootable`, run the emulator, and
        // this would silently test the emulator twice.
        let on_floppy = MountContext::resolve(&dir.to_string_lossy(), "floppy.dsk", "auto");
        assert!(on_floppy.booting(), "a disk is booting");
        assert!(on_floppy.accepts(floppy.len() as u64), "its own board");
        assert!(!on_floppy.accepts(hd_len), "a hard disk is on another board");
        assert_ne!(on_floppy.slot(1), "B:", "a booted machine has no drive letters");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **Slot 0 is reserved, and the screens have to say by what.**
    ///
    /// Reported 2026-08-21: with `CP/M runs: Boot HDSK04.DSK` the mount dialog's
    /// first row showed `(drive folder)` beside a note reading "the booted disk
    /// is here" — two statements that contradict each other, and neither naming
    /// the disk. The operator reasonably read the row as a place the boot disk
    /// should have appeared.
    ///
    /// The note is one text for three surfaces, and it comes from the **resolved**
    /// target: a `cpm_boot_image` naming a disk that is gone runs the emulator,
    /// where slot 0 is an ordinary drive and there is nothing to reserve.
    #[test]
    fn test_slot_zero_names_the_disk_that_reserved_it() {
        let dir = std::env::temp_dir().join(format!("egw_boot_slot0_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // Under `CPM/`, because that is where `boot_target` looks.
        let images = super::super::image::images_dir(&super::super::layout::cpm_dir(
            &dir.to_string_lossy(),
        ));
        std::fs::create_dir_all(&images).unwrap();
        std::fs::write(images.join("floppy.dsk"), bootable_image()).unwrap();

        // The emulator: slot 0 is A:, and there is no reservation to report.
        let emu = MountContext::resolve(&dir.to_string_lossy(), "", "auto");
        assert_eq!(emu.boot_slot_note(), None);
        assert_eq!(emu.boot_disk_name(), None);

        // A disk booting: the note names it, and the name is available on its
        // own for a surface that shows slot 0 as a control.
        let booting = MountContext::resolve(&dir.to_string_lossy(), "floppy.dsk", "auto");
        assert!(booting.booting());
        assert_eq!(booting.boot_disk_name(), Some("floppy.dsk"));
        let note = booting.boot_slot_note().expect("a booting disk reserves slot 0");
        assert!(note.contains("floppy.dsk"), "the note must name the disk: {note:?}");
        assert!(note.contains("boots here"), "{note:?}");

        // **A setting is not an outcome.** A key naming a disk that is not there
        // runs the emulator, so nothing has reserved slot 0 and the screens must
        // not claim otherwise.
        let gone = MountContext::resolve(&dir.to_string_lossy(), "vanished.dsk", "auto");
        assert!(!gone.booting());
        assert_eq!(gone.boot_slot_note(), None);
        assert_eq!(gone.boot_disk_name(), None);

        // Both spellings of the "behind the boot disk" warning exist, and the
        // narrow one fits a C64 row with its indent.
        assert!(BEHIND_BOOT_DISK.contains("behind the boot disk"));
        assert!(BEHIND_BOOT_DISK_SHORT.contains("behind the boot disk"));
        assert!(
            BEHIND_BOOT_DISK_SHORT.chars().count() + 5 <= 40,
            "{BEHIND_BOOT_DISK_SHORT:?} plus a five-column indent must fit a PETSCII row"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The other half of the reported case, which needs a real hard disk: the
    /// 88-HDSK's slots are **platters**, so mounting beside a booted hard disk
    /// must say `unit 0.1` and must accept another hard disk rather than a
    /// floppy.  A synthesised image cannot stand in -- `boot_target` cold-starts
    /// it, and a zero-filled 4.9 MB file is exactly the data disk it refuses.
    ///
    /// Ignored -- set `CPM_HDSK_IMAGE` to an 88-HDSK image that boots.
    #[test]
    #[ignore]
    fn test_mounting_beside_a_booted_hard_disk_names_platters() {
        let Ok(src) = std::env::var("CPM_HDSK_IMAGE") else {
            eprintln!("set CPM_HDSK_IMAGE to run this");
            return;
        };
        let dir = std::env::temp_dir().join(format!("egmountctx{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let images = super::super::image::images_dir(&super::super::layout::cpm_dir(
            &dir.to_string_lossy(),
        ));
        std::fs::create_dir_all(&images).unwrap();
        let hd = std::fs::read(&src).expect("the hard disk image");
        std::fs::write(images.join("hard.dsk"), &hd).unwrap();

        let ctx = MountContext::resolve(&dir.to_string_lossy(), "hard.dsk", "auto");
        assert!(ctx.booting(), "{src} did not resolve as a booting disk");
        assert!(ctx.accepts(hd.len() as u64), "another hard disk is reachable");
        assert!(
            !ctx.accepts(bootable_image().len() as u64),
            "a floppy is on the 88-DCDD and the guest never reads it"
        );
        assert!(
            ctx.slot(1).contains("unit"),
            "the 88-HDSK's slots are platters, not drives: {}",
            ctx.slot(1)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A board mismatch is not a disk that cannot boot**, and the difference
    /// is measured on a real image rather than asserted.
    ///
    /// This is the finding a review caught after the filter shipped: an Altair
    /// image offered to a machine with only Cromemco boards fails inside
    /// `insert` — no controller accepts 337,568 bytes — and that refusal was
    /// being read as "this disk has no boot program". The disk then vanished
    /// from every boot list and the setting silently fell back to the emulator,
    /// which is precisely what the split was written to prevent. The bug was
    /// invisible because all three callers repeated the same classification.
    #[test]
    fn test_a_disk_this_machine_has_no_board_for_is_still_offered() {
        use super::super::boot_machine::BootMachine;
        use super::Bootability;

        let image = bootable_image();
        // On the machine it belongs to, it boots.
        assert_eq!(
            BootMachine::bootability(image.clone(), "auto", super::super::cpu::DEFAULT_CPU),
            Bootability::Boots
        );

        // On a machine whose boards take other media, it is refused — and the
        // refusal must be about the *machine*, so the disk stays on the lists.
        let elsewhere =
            BootMachine::bootability(image, "cromemco", super::super::cpu::DEFAULT_CPU);
        assert!(
            matches!(elsewhere, Bootability::NoBoardForIt(_)),
            "an Altair disk on a Cromemco machine is a board mismatch, not a dud disk: \
             {elsewhere:?}"
        );
        assert!(elsewhere.offer(), "a board mismatch stays on the boot lists");

        // And the thing that really cannot boot is still withheld, so this test
        // cannot pass by making `offer()` always true.
        let data = vec![0u8; Geometry::EIGHT_INCH.image_len() as usize];
        let dud = BootMachine::bootability(data, "auto", super::super::cpu::DEFAULT_CPU);
        assert!(matches!(dud, Bootability::NoBootProgram(_)), "{dud:?}");
        assert!(!dud.offer(), "a disk with no boot program is withheld");
    }

    /// **A disk that cannot boot is not offered as something to boot.**
    ///
    /// The emulator survives the filter unconditionally — it is not an image and
    /// has no file to ask about — which is the part that would break silently if
    /// the filter were ever moved up a line.
    #[test]
    fn test_a_disk_that_cannot_boot_is_not_offered() {
        let dir = std::env::temp_dir().join("egw_boot_choices_filter_test");
        let _ = std::fs::remove_dir_all(&dir);
        let images = dir.join(super::super::image::IMAGES_DIR);
        std::fs::create_dir_all(&images).unwrap();
        std::fs::write(images.join("altair8_system.dsk"), [0u8; 8]).unwrap();
        std::fs::write(images.join("altair8_data.dsk"), [0u8; 8]).unwrap();

        let choices = boot_choices_by(&dir, |p| {
            !p.file_name().unwrap().to_string_lossy().contains("data")
        });
        let values: Vec<&str> = choices.iter().map(|(v, _)| v.as_str()).collect();
        assert_eq!(
            values,
            vec!["", "altair8_system.dsk"],
            "the data disk is gone and the emulator is still first"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A disk named in the config and since deleted must still describe itself.
    /// The screens are where an operator finds out why their gateway is running
    /// the emulator when they asked for a disk, so the label cannot depend on
    /// the file being there.
    #[test]
    fn test_a_label_exists_for_a_setting_whose_disk_is_gone() {
        assert_eq!(boot_choice_label(""), BOOT_EMULATOR_LABEL);
        assert_eq!(boot_choice_label("vanished.dsk"), "Boot vanished.dsk");
    }

    /// The four surfaces that show `cpm_boot_image` must mark an unrunnable
    /// setting, and mark it the same way.
    ///
    /// The web page spelled its own `(missing)` for a while and was the only
    /// one that said anything, which is exactly how the desktop and telnet rows
    /// came to read `Boot vanished.dsk` while the emulator was what started.
    #[test]
    fn test_a_setting_that_will_not_run_says_so() {
        use super::{boot_setting_label, boot_setting_mark, BootTarget};

        assert_eq!(boot_setting_mark(&BootTarget::Emulator), "");
        assert_eq!(boot_setting_mark(&BootTarget::Image("real.dsk".into())), "");
        // Two different mistakes, fixed two different ways — put the disk back,
        // or stop typing a path where a bare filename belongs.
        assert_eq!(boot_setting_mark(&BootTarget::Missing("x".into())), " (missing)");
        assert_eq!(
            boot_setting_mark(&BootTarget::UnsafeName("x".into())),
            " (invalid name)"
        );
        // The third: the disk is right there and has no boot program on it, so
        // neither looking for it nor retyping the name will help.
        assert_eq!(
            boot_setting_mark(&BootTarget::NotBootable("x".into())),
            " (not bootable)"
        );
        let marks = [
            boot_setting_mark(&BootTarget::Missing("x".into())),
            boot_setting_mark(&BootTarget::UnsafeName("x".into())),
            boot_setting_mark(&BootTarget::NotBootable("x".into())),
        ];
        for (i, a) in marks.iter().enumerate() {
            for b in &marks[i + 1..] {
                assert_ne!(a, b, "two fallbacks must not read the same");
            }
        }

        // Every fallback runs the emulator, so every one of them names the
        // emulator's drives.  A new variant that forgot this would put board
        // slot names on a screen where drive letters are what is running.
        for target in [
            BootTarget::Missing("x".into()),
            BootTarget::UnsafeName("x".into()),
            BootTarget::NotBootable("x".into()),
        ] {
            assert_eq!(target.slot_naming(), SlotNaming::Drives, "{target:?}");
            assert!(target.into_image().is_none(), "a fallback boots nothing");
        }

        // The budget `cpm_runs_row` reserves out of a 26-column PETSCII row.
        // Pinned because that doc states the number, and a longer marker would
        // squeeze the filename toward nothing without anything complaining —
        // the row would still *fit*, which is why no width test would catch it.
        for target in [
            BootTarget::Emulator,
            BootTarget::Image("real.dsk".into()),
            BootTarget::Missing("x".into()),
            BootTarget::UnsafeName("x".into()),
            BootTarget::NotBootable("x".into()),
        ] {
            let mark = boot_setting_mark(&target);
            assert!(
                mark.chars().count() <= 15,
                "{mark:?} is {} chars; cpm_runs_row's doc reserves 15",
                mark.chars().count()
            );
        }

        // The label is the ordinary one with the mark appended, so a surface
        // cannot show the mark without the setting or the setting without it.
        assert_eq!(
            boot_setting_label(&BootTarget::Missing("gone.dsk".into()), "gone.dsk"),
            "Boot gone.dsk (missing)"
        );
        assert_eq!(
            boot_setting_label(&BootTarget::Image("real.dsk".into()), "real.dsk"),
            "Boot real.dsk"
        );
        assert_eq!(boot_setting_label(&BootTarget::Emulator, ""), BOOT_EMULATOR_LABEL);
    }

    /// **A setting is not an outcome.** `cpm_boot_image` can name a disk that
    /// has been deleted since, or a string that could never be a filename, and
    /// in both cases the emulator is what starts — so both must resolve to the
    /// emulator's naming, not the string's.
    ///
    /// That was a real defect: the screens read the key, so a stale one named
    /// the rows `drive 1` for a board nobody was going to boot, and hid the
    /// `(R/O)` marker on an image our BDOS was about to refuse a write to.
    /// The label beside it kept saying `Boot vanished.dsk`, which is correct
    /// for a *setting* and is why the mismatch read as deliberate.
    #[test]
    fn test_a_boot_image_that_is_not_there_names_the_emulators_drives() {
        use super::{boot_target, mount_refuses_writes, slot_name, BootTarget, SlotNaming};

        let dir = std::env::temp_dir().join("egw_boot_target_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("CPM").join(super::super::image::IMAGES_DIR)).unwrap();
        let transfer = dir.to_string_lossy().to_string();
        // Through `cpm_dir`, which canonicalizes — the folder has to exist
        // first or this and `boot_target` would disagree on a platform whose
        // temp dir is a symlink (macOS `/tmp` is `/private/tmp`).
        let images =
            super::super::image::images_dir(&super::super::layout::cpm_dir(&transfer));

        // Nothing configured, and whitespace, are both the emulator as asked for.
        assert_eq!(boot_target(&transfer, ""), BootTarget::Emulator);
        assert_eq!(boot_target(&transfer, "   "), BootTarget::Emulator);

        // The two fallbacks — the defect this test exists for.
        assert_eq!(
            boot_target(&transfer, "gone.dsk"),
            BootTarget::Missing("gone.dsk".to_string()),
            "a disk that is not in the folder cannot be booted"
        );
        assert_eq!(
            boot_target(&transfer, "../../etc/passwd"),
            BootTarget::UnsafeName("../../etc/passwd".to_string()),
            "and a name that escapes the folder is refused before it is stat'ed"
        );
        for stale in ["gone.dsk", "../../etc/passwd"] {
            let naming = boot_target(&transfer, stale).slot_naming();
            assert_eq!(naming, SlotNaming::Drives, "{stale} runs the emulator");
            // The consequence on the rows themselves, not just on the enum.
            assert_eq!(slot_name(&naming, 1, Some(337_568)), "B:");
        }

        // And the disk that *is* there boots, at the path the session opens.
        // A real image, not eight bytes: `boot_target` cold-starts it now.
        std::fs::write(images.join("altair8_cpm22.dsk"), bootable_image()).unwrap();
        let target = boot_target(&transfer, "altair8_cpm22.dsk");
        assert_eq!(target.slot_naming(), SlotNaming::Boards);
        assert_eq!(
            target.clone().into_image().as_deref(),
            Some(images.join("altair8_cpm22.dsk").as_path()),
            "the emulator entry point boots this exact file"
        );
        // Trimmed, so a config file with a trailing space still boots.
        assert_eq!(boot_target(&transfer, " altair8_cpm22.dsk "), target);

        // The second half of the defect: whose read-only verdict applies.  Our
        // BDOS refuses this image; the host would allow the write.  Under a
        // stale key the screens asked the host and showed no marker at all.
        let mount = crate::cpm::image::registry::Mount {
            path: images.join("altair8_cpm22.dsk"),
            filename: "altair8_cpm22.dsk".to_string(),
            format: "altair8",
            read_only: true,
            read_only_reason: "directory does not add up".to_string(),
            host_read_only: false,
            fs: std::sync::Arc::new(std::sync::Mutex::new(
                crate::cpm::image::fs::ImageFs::mount(
                    Box::new(crate::cpm::image::media::MemMedia::new(
                        crate::cpm::image::format::by_token("altair8").unwrap().blank_image().unwrap(),
                    )),
                    crate::cpm::image::format::by_token("altair8").unwrap(),
                    true,
                )
                .unwrap(),
            )),
        };
        assert!(
            mount_refuses_writes(&boot_target(&transfer, "gone.dsk").slot_naming(), &mount),
            "a stale key runs our BDOS, so our BDOS's verdict is the one to show"
        );
        assert!(
            !mount_refuses_writes(&boot_target(&transfer, "altair8_cpm22.dsk").slot_naming(), &mount),
            "and a disk that really boots owns its own format — only the tab stops it"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_cold_boot_loads_the_sector_and_enters_at_zero() {
        let mut c = with_disk();
        let mut stored: Option<(u16, Vec<u8>)> = None;
        let entry = cold_boot(
            &mut c,
            0,
            |_, _, _| Ok(boot_sector()),
            |addr, bytes| stored = Some((addr, bytes.to_vec())),
        )
        .expect("boots");
        assert_eq!(entry, 0x0000, "the boot sector's own jumps assume 0000h");
        let (addr, bytes) = stored.expect("something was stored");
        assert_eq!(addr, 0x0000);
        assert_eq!(bytes.len(), BOOT_DATA_LEN);
        assert_eq!(&bytes[..REAL_BOOT_START.len()], REAL_BOOT_START);
    }

    /// The payload must be taken from offset 3, not from the start of the
    /// sector — the first three bytes are the controller's own header, and
    /// loading them would put `80 00 01` where the entry point belongs.
    #[test]
    fn test_the_sector_header_is_not_part_of_the_payload() {
        let mut c = with_disk();
        let mut stored = Vec::new();
        cold_boot(&mut c, 0, |_, _, _| Ok(boot_sector()), |_, b| stored = b.to_vec()).unwrap();
        assert_eq!(stored[0], 0x31, "must start at the LXI SP, not the header");
        assert_ne!(stored[0], 0x80);
    }

    #[test]
    fn test_booting_an_empty_drive_says_so() {
        let mut c = Dcdd::new();
        let err = cold_boot(&mut c, 0, |_, _, _| Ok(boot_sector()), |_, _| {}).unwrap_err();
        assert_eq!(err, BootError::NoDisk(0));
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn test_an_unreadable_sector_is_reported_not_run() {
        let mut c = with_disk();
        let err = cold_boot(&mut c, 0, |_, _, _| Err("disk on fire".into()), |_, _| {})
            .unwrap_err();
        assert!(matches!(err, BootError::Unreadable(_)));
        assert!(err.to_string().contains("disk on fire"));
    }

    #[test]
    fn test_a_short_sector_is_refused() {
        let mut c = with_disk();
        let err = cold_boot(&mut c, 0, |_, _, _| Ok(vec![0u8; 40]), |_, _| {}).unwrap_err();
        assert!(matches!(err, BootError::Unreadable(_)));
    }

    /// A data disk must be refused rather than entered.  An 8080 turned loose
    /// on text does something, and it is never useful.
    #[test]
    fn test_a_data_disk_is_not_booted() {
        let mut c = with_disk();
        let mut text = vec![0u8; SECTOR_LEN];
        for (i, b) in text[BOOT_DATA_OFFSET..].iter_mut().enumerate() {
            *b = b" ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789"[i % 37];
        }
        let err = cold_boot(&mut c, 0, |_, _, _| Ok(text.clone()), |_, _| {}).unwrap_err();
        assert_eq!(err, BootError::NotBootable);
        assert!(err.to_string().contains("data, not a system disk"));
    }

    #[test]
    fn test_a_blank_sector_is_not_booted() {
        let mut c = with_disk();
        let err = cold_boot(&mut c, 0, |_, _, _| Ok(vec![0xE5; SECTOR_LEN]), |_, _| {})
            .unwrap_err();
        assert_eq!(err, BootError::NotBootable);
    }

    #[test]
    fn test_bootable_heuristic_accepts_code_and_rejects_the_impossible() {
        assert!(looks_bootable(REAL_BOOT_START));
        assert!(!looks_bootable(&[0; 128]), "erased");
        assert!(!looks_bootable(&[0xE5; 128]), "unformatted");
        assert!(!looks_bootable(b"PLAIN TEXT ON A DATA DISK, NOTHING ELSE HERE AT ALL"));
        assert!(!looks_bootable(&[0x31, 0x00]), "too short to judge");
    }

    /// **A data disk's first sector is a header and then padding, and that is
    /// what tells it apart from a boot sector.**
    ///
    /// The two shapes here are the real ones, taken from the images that
    /// motivated the rule. `DISK0B` ("Time Sharing Basic V2 programs") holds its
    /// volume label and 112 zeros; `DISK0F` ("Altair Mini-Disk DOS programs")
    /// holds two bytes and 126. Both used to be *run*: DISK0B executed its own
    /// label as instructions and DISK0F NOPped into cleared memory, and both
    /// then sat silent, which looks like a disk we cannot boot rather than one
    /// with nothing to boot.
    #[test]
    fn test_a_data_disks_header_and_padding_is_not_a_boot_sector() {
        let mut ts2 = vec![0u8; 128];
        ts2[..16].copy_from_slice(b"\x80\x6d\x00\x00VOL\xb1TS2FILES");
        assert!(!looks_bootable(&ts2), "a volume label and padding is not a program");

        let mut mini = vec![0u8; 128];
        mini[..2].copy_from_slice(&[0x15, 0x15]);
        assert!(!looks_bootable(&mini), "two bytes and padding is not a program");
    }

    /// The threshold has to clear the *padded* boot sectors, and those go much
    /// further than they look.
    ///
    /// Measured over every image in the Altair and z80pack collections, using
    /// the payload each controller really extracts: disks that boot run up to
    /// **63%** one repeated byte — z80pack's `mpm-2`, a short loader with a long
    /// zero tail — while the disks with no boot program are 89% and above.
    ///
    /// This is the guard on the number, and it is here because the two
    /// thresholds reasoned out before measuring were *both* wrong: one-half
    /// would have refused the three Altair hard disks, and a
    /// trailing-zero-run rule would have refused `mpm-2`.
    #[test]
    fn test_a_padded_boot_sector_is_still_bootable() {
        // The shape of mpm-2's: real code, then a long tail of zeros.
        let mut padded = vec![0u8; 128];
        padded[..47].copy_from_slice(&[0x31; 47]);
        assert_eq!(padded.iter().filter(|&&b| b == 0).count() * 100 / 128, 63);
        assert!(looks_bootable(&padded), "63% zero boots in real life — mpm-2 does");

        // And the far side: 89% is DISK0B, the tightest disk that must not run.
        let mut sparse = vec![0u8; 128];
        sparse[..14].copy_from_slice(&[0x31; 14]);
        assert_eq!(sparse.iter().filter(|&&b| b == 0).count() * 100 / 128, 89);
        assert!(!looks_bootable(&sparse), "89% zero is padding, not a program");
    }

    /// Boot every real image in a folder and report what happened.
    ///
    /// The end-to-end check for this stage: a genuine Altair disk must load and
    /// return an entry point, and a data disk must be refused.  Ignored — set
    /// `CPM_BOOT_DIR` to a folder of `.dsk` files.
    #[test]
    #[ignore]
    fn test_boot_real_images() {
        let Ok(dir) = std::env::var("CPM_BOOT_DIR") else {
            eprintln!("set CPM_BOOT_DIR to run this");
            return;
        };
        let mut names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.to_ascii_lowercase().ends_with(".dsk"))
            .collect();
        names.sort();
        let mut booted = 0;
        for name in &names {
            let path = std::path::Path::new(&dir).join(name);
            let bytes = std::fs::read(&path).unwrap();
            let geom = if bytes.len() as u64 == Geometry::MINIDISK.image_len() {
                Geometry::MINIDISK
            } else {
                Geometry::EIGHT_INCH
            };
            if bytes.len() as u64 != geom.image_len() {
                println!("  skipped  {name} ({} bytes — not an 88-DCDD image)", bytes.len());
                continue;
            }
            let mut c = Dcdd::new();
            c.insert(0, Disk { geometry: geom, read_only: true });
            let mut first = Vec::new();
            match cold_boot(
                &mut c,
                0,
                |_, t, s| {
                    let off = geom.offset(t, s) as usize;
                    Ok(bytes[off..off + SECTOR_LEN].to_vec())
                },
                |_, b| first = b.to_vec(),
            ) {
                Ok(entry) => {
                    booted += 1;
                    println!(
                        "  BOOTS    {name} -> {entry:#06x}, first bytes {:02x?}",
                        &first[..6]
                    );
                }
                Err(e) => println!("  refused  {name}: {e}"),
            }
        }
        assert!(booted > 0, "no image in {dir} produced a boot sector");
    }

    /// The bootstrap waits for sector 0 specifically, not merely for any
    /// sector — a boot from the wrong sector runs the wrong bytes.
    #[test]
    fn test_the_bootstrap_waits_for_sector_zero() {
        let mut c = with_disk();
        let mut asked = Vec::new();
        cold_boot(
            &mut c,
            0,
            |d, t, s| {
                asked.push((d, t, s));
                Ok(boot_sector())
            },
            |_, _| {},
        )
        .unwrap();
        // Sector 0 first, then the interleaved sectors that hold the rest of
        // the loader.
        assert_eq!(asked[0], (0, 0, 0), "the boot sector comes first");
        assert_eq!(
            asked,
            (0..BOOT_SECTORS)
                .map(|i| (0u8, 0u8, i * BOOT_INTERLEAVE))
                .collect::<Vec<_>>(),
            "the loader is read with the disk's own 2:1 interleave"
        );
    }

    /// **What the boot lists offer is exactly what boots.**
    ///
    /// The live gate for the filter, and the only one that can be: the question
    /// is about real disks, and the whole defect it closes was a *plausible*
    /// filter — a size test — that agreed with reality on every disk anyone had
    /// tried and disagreed on the four nobody had.
    ///
    /// It checks both directions against the same folder, because one direction
    /// alone is not a measurement: a filter that rejects everything passes
    /// "nothing offered fails to boot", and the one this replaces passed
    /// "everything that boots is offered". So every disk is cold-started, and
    /// the verdict must match the offer, name by name.
    ///
    /// Ignored — set `CPM_BOOT_DIR` to a folder of `.dsk` files.
    #[test]
    #[ignore]
    fn test_the_boot_list_offers_exactly_the_disks_that_boot() {
        let Ok(src) = std::env::var("CPM_BOOT_DIR") else {
            eprintln!("set CPM_BOOT_DIR to run this");
            return;
        };
        // Never point a test at the originals: a boot writes nothing, but the
        // list is built from a CPM/images folder and building one here would
        // otherwise mean creating it inside somebody's collection.
        let base = std::env::temp_dir().join(format!("egbootlist{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let images = super::super::image::images_dir(&base);
        std::fs::create_dir_all(&images).unwrap();
        let mut names: Vec<String> = std::fs::read_dir(&src)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.to_ascii_lowercase().ends_with(".dsk"))
            .collect();
        names.sort();
        assert!(!names.is_empty(), "no .dsk files in {src}");
        // Hard-linked, not symlinked: `available_images` asks `DirEntry::file_type`,
        // which does not follow links, so a symlinked image is invisible to the
        // images folder and every disk here would be "withheld" for the wrong
        // reason.  The first draft of this test did exactly that, and the
        // both-directions assert below is what caught it.
        for n in &names {
            let (from, to) = (std::path::Path::new(&src).join(n), images.join(n));
            if std::fs::hard_link(&from, &to).is_err() {
                std::fs::copy(&from, &to).unwrap();
            }
        }

        let offered: Vec<String> =
            boot_choices(&base).into_iter().map(|(v, _)| v).filter(|v| !v.is_empty()).collect();

        let cfg = crate::config::get_config();
        let (machine, cpu) = (cfg.cpm_boot_machine.clone(), cfg.cpm_cpu.clone());
        drop(cfg);
        let mut wrong = Vec::new();
        for n in &names {
            let bytes = std::fs::read(images.join(n)).unwrap();
            let boots = super::super::boot_machine::BootMachine::bootability(bytes, &machine, &cpu)
                .offer();
            let listed = offered.iter().any(|o| o == n);
            if boots != listed {
                wrong.push(format!(
                    "{n}: cold start says {}, the list says {}",
                    if boots { "boots" } else { "cannot boot" },
                    if listed { "offered" } else { "not offered" }
                ));
            }
            println!("  {:<14} {}", n, if listed { "offered" } else { "withheld" });
        }
        let _ = std::fs::remove_dir_all(&base);
        assert!(wrong.is_empty(), "{wrong:#?}");
        assert!(!offered.is_empty(), "every disk was withheld — the filter cannot be right");
    }
}
