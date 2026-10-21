//! The CP/M setup wizard's questions, answers and recommendations.
//!
//! **One wizard, three ways to draw it.**  Telnet, the web page and the
//! desktop each draw these steps, and none of them decides anything: what is
//! asked, in what order, which disks are offered at each step, what is
//! recommended and why, and the settings the answers come to all live here.
//! Three copies of "which disks may ride along with this boot disk" is how the
//! surfaces would come to disagree, and every other CP/M rule in this project
//! is held in one place for that reason.
//!
//! The order follows what each answer makes irrelevant.  The first question
//! is what CP/M runs: the emulator needs no boot disk and asks only for drive
//! letters, while a booted disk asks for its machine, its processor, a monitor
//! ROM if it needs one, and the disks to ride along in its board's other slots.
//!
//! Nothing here writes a setting until [`apply`] is called with the finished
//! [`Plan`]: a wizard abandoned half way leaves the configuration as it was.
//!
//! The explanations are written to be read on a 40-column Commodore as well as
//! a browser, so each is a list of lines no wider than [`TEXT_WIDTH`]; a wide
//! surface joins them into one paragraph.

use std::path::{Path, PathBuf};

/// The widest line of any explanation here: a C64's 40 columns, less the
/// two-space indent every telnet screen uses and the two it keeps clear.
pub const TEXT_WIDTH: usize = 36;

/// What the first question decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Runs {
    /// Our CP/M 2.2, with folders as drives and EGT8080 on A:.
    Emulator,
    /// A disk image's own operating system, on an emulated machine.
    Boot,
}

/// Why the first question matters, in the words every surface shows.
pub const RUNS_HELP: &[&str] = &[
    "CP/M can run two ways here.",
    "",
    "The CP/M Emulator is this gateway's",
    "own CP/M 2.2. Drives A: to P: are",
    "folders, the EGT8080 terminal is on",
    "A:, and disk images can be added on",
    "drive letters. Start here if unsure.",
    "",
    "Booting a disk hands an emulated",
    "Altair, Cromemco or z80pack machine",
    "to the operating system ON a disk",
    "image: CP/M 3, Altair BASIC, CDOS.",
];

/// What choosing the emulator means, under the choice.
pub const EMULATOR_LABEL: &str = "The CP/M Emulator";
/// What choosing a boot disk means, under the choice.
pub const BOOT_LABEL: &str = "Boot a disk image";

/// The emulator's drive step.
pub const EMULATOR_DRIVES_HELP: &[&str] = &[
    "A disk image can sit on a drive",
    "letter, where CP/M programs see its",
    "files instead of that drive's",
    "folder. The folder is not touched",
    "and comes back when it is removed.",
    "A: is left alone: the terminals",
    "live there. This step is optional.",
];

/// The boot disk step.
pub const BOOT_DISK_HELP: &[&str] = &[
    "Only disks that have been seen to",
    "start here are listed. The one you",
    "pick takes the whole machine, and",
    "its own system decides what it can",
    "do.",
];

/// The machine step.
pub const MACHINE_HELP: &[&str] = &[
    "Each disk was written for one kind",
    "of machine, and looks for its",
    "console and its disk controller",
    "where that machine kept them. The",
    "wizard reads the disk to tell.",
    "Keep its answer unless the disk",
    "says otherwise.",
];

/// The processor step.
pub const CPU_HELP: &[&str] = &[
    "The Z80 runs nearly all 8080",
    "software and is the usual choice.",
    "The 8080 is the Altair's own chip:",
    "choose it for Altair BASIC, and for",
    "8080 test programs.",
];

/// The extra disks step.
pub const EXTRA_DISKS_HELP: &[&str] = &[
    "The board can hold more disks than",
    "the one it starts from. Only disks",
    "the same board reads are offered.",
    "The disk's own system may use fewer",
    "of them: stock Altair CP/M knows",
    "four.",
];

/// The monitor ROM step.  One ROM is on offer, so it is named (see
/// [`rom_for`]).
pub const ROM_HELP: &[&str] = &[
    "This disk looks for the CUTER",
    "monitor at C000 and stops without",
    "it. The ROM is fetched with the",
    "sample disks, from its author's",
    "collection.",
];

/// Why the Z80 is advised, wherever it is: for the emulator, and for a disk
/// that is not Altair BASIC.
pub const Z80_WHY: &str = "the Z80 runs nearly all 8080 software";

/// What a disk screen says about an Altair BASIC disk.
pub const BASIC_NOTE: &str = "Altair BASIC: the 8080 is advised.";
/// What a disk screen says about a disk that wants the monitor ROM.
pub const ROM_NOTE: &str = "Needs the CUTER monitor ROM.";

/// The last screen's instruction, on every surface -- shown only once the
/// settings are saved: after "Not saved" it would send the operator to boot
/// the configuration they came to replace.
pub const FINISH_LINE: &str = "Press K on the main menu to start.";

/// The wizard's steps.
///
/// **The order is decided here, once.**  Each surface used to keep its own
/// step machine, and they had already drifted (telnet's `E` threw away the
/// drive picks the other two kept); now a surface draws a step and asks
/// [`Step::next`] where to go, and the web page is sent the same answers as
/// a table ([`Step::table`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Runs,
    EmulatorDrives,
    BootDisk,
    Machine,
    Cpu,
    Rom,
    ExtraDisks,
    Ready,
}

impl Step {
    pub const ALL: [Step; 8] = [
        Step::Runs,
        Step::EmulatorDrives,
        Step::BootDisk,
        Step::Machine,
        Step::Cpu,
        Step::Rom,
        Step::ExtraDisks,
        Step::Ready,
    ];

    /// The step's name on the web page.
    pub fn key(self) -> &'static str {
        match self {
            Step::Runs => "runs",
            Step::EmulatorDrives => "drives",
            Step::BootDisk => "bootdisk",
            Step::Machine => "machine",
            Step::Cpu => "cpu",
            Step::Rom => "rom",
            Step::ExtraDisks => "extra",
            Step::Ready => "ready",
        }
    }

    /// The step after this one, for a plan that `runs` this way and whose
    /// boot disk does or does not want a monitor ROM.  `None` after Ready.
    pub fn next(self, runs: Runs, needs_rom: bool) -> Option<Step> {
        Some(match (self, runs) {
            (Step::Runs, Runs::Emulator) => Step::EmulatorDrives,
            (Step::Runs, Runs::Boot) => Step::BootDisk,
            (Step::EmulatorDrives, _) => Step::Cpu,
            (Step::BootDisk, _) => Step::Machine,
            (Step::Machine, _) => Step::Cpu,
            (Step::Cpu, Runs::Emulator) => Step::Ready,
            (Step::Cpu, Runs::Boot) if needs_rom => Step::Rom,
            (Step::Cpu, Runs::Boot) => Step::ExtraDisks,
            (Step::Rom, _) => Step::ExtraDisks,
            (Step::ExtraDisks, _) => Step::Ready,
            (Step::Ready, _) => return None,
        })
    }

    /// [`Step::next`] for every case, as `step|runs|rom` keys (`rom` is 0 or
    /// 1) and the next step's key: what the web page walks by.
    pub fn table() -> Vec<(String, &'static str)> {
        let mut out = Vec::new();
        for step in Step::ALL {
            for (runs, rk) in [(Runs::Emulator, "emulator"), (Runs::Boot, "boot")] {
                for rom in [false, true] {
                    if let Some(to) = step.next(runs, rom) {
                        out.push((format!("{}|{rk}|{}", step.key(), rom as u8), to.key()));
                    }
                }
            }
        }
        out
    }
}

/// What one disk image is, as far as the wizard needs to know.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskFacts {
    /// The file name inside `CPM/images`.
    pub name: String,
    /// Its size, which is how a board is chosen.
    pub bytes: u64,
    /// Seen to start here (see [`super::boot::image_can_boot`]).
    pub boots: bool,
    /// The disk's own directory, every user area; `None` when the disk has no
    /// CP/M filesystem -- its own system keeps its files its own way.
    pub files: Option<Vec<String>>,
    /// The catalogue's one-line description, in the catalogue's words.
    pub summary: String,
    /// The machine the disk is for, as the boot would decide it.
    pub machine: String,
    /// Why that machine, when detection could say.
    pub machine_why: Option<String>,
    /// The board the disk sits on, on that machine.
    pub board: Option<&'static str>,
    /// How many disks that board can carry, this one included.
    pub slots: Option<u8>,
    /// MITS Altair BASIC is the disk's operating system.
    pub altair_basic: bool,
    /// The disk tests for a monitor ROM and stops without one.
    pub needs_rom: bool,
}

impl DiskFacts {
    /// Read everything about one image.  Blocking -- the first read of a file
    /// reads all of it, and its bootability answer cold-starts it -- so an
    /// async caller goes through `spawn_blocking`.
    ///
    /// **Cached against the file's identity** (path, length, modification
    /// time), as [`super::boot::image_can_boot`] is: every surface reads the
    /// whole folder each time the wizard opens, and so does [`apply`], and a
    /// hard disk is 4.9 MB on a Pi.  Every fact here is the file's alone --
    /// bootability is asked of the machine read from the disk, never of the
    /// settings in force -- so a file nobody touched has nothing new to say.
    pub fn read(cpm_base: &Path, name: &str) -> Option<DiskFacts> {
        use std::collections::HashMap;
        use std::sync::{Mutex, OnceLock};
        if !super::image::is_safe_image_name(name) {
            return None;
        }
        let path = super::image::images_dir(cpm_base).join(name);
        let meta = std::fs::metadata(&path).ok()?;
        type Key = (PathBuf, u64, Option<std::time::SystemTime>);
        static CACHE: OnceLock<Mutex<HashMap<Key, DiskFacts>>> = OnceLock::new();
        let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
        let key = (path.clone(), meta.len(), meta.modified().ok());
        if let Some(hit) = cache.lock().ok().and_then(|c| c.get(&key).cloned()) {
            return Some(hit);
        }
        let facts = Self::measure(&path, name)?;
        if let Ok(mut c) = cache.lock() {
            // Capped for the reason `image_can_boot`'s is: the key carries an
            // mtime, and a disk rewritten for months would grow it.
            if c.len() >= 512 {
                c.clear();
            }
            c.insert(key, facts.clone());
        }
        Some(facts)
    }

    /// [`DiskFacts::read`] uncached: the file is read once, and those bytes
    /// answer every question that needs them, the bootability one included.
    fn measure(path: &Path, name: &str) -> Option<DiskFacts> {
        // Before the read, so a file still being written is remembered under
        // the identity it had *then*: the next look sees it changed.
        let meta = std::fs::metadata(path).ok()?;
        let bytes = std::fs::read(path).ok()?;
        let len = bytes.len() as u64;
        let files = super::image::files_on(path);
        let (machine, machine_why) =
            super::detect::machine_for(super::console::AUTO_MACHINE, &bytes);
        let mut facts = DiskFacts {
            name: name.to_string(),
            bytes: len,
            boots: false,
            summary: String::new(),
            board: super::boot_machine::BootMachine::board_for(Some(&machine), len),
            slots: super::boot_machine::BootMachine::slot_count(Some(&machine), len),
            altair_basic: super::detect::image_carries_altair_basic(&bytes, files.is_some()),
            needs_rom: super::detect::image_needs_monitor_rom(&bytes),
            files,
            machine,
            machine_why,
        };
        // Measured as the wizard will set it up -- the machine read from the
        // disk, the processor it will advise -- and not under the settings in
        // force, which are the ones the operator came here to change: left on a
        // Cromemco, or on the 8080, those hid the disks the wizard is for.
        let cpu = recommended_cpu(&facts).0;
        facts.boots = super::boot::image_can_boot_on_with(path, &meta, bytes, super::console::AUTO_MACHINE, cpu);
        facts.summary = summary(facts.boots, facts.files.as_deref());
        Some(facts)
    }

    /// The listing as lines to show: the files, or what the disk is instead.
    pub fn listing(&self) -> Vec<String> {
        match &self.files {
            Some(f) if f.is_empty() => vec!["(an empty CP/M directory)".to_string()],
            Some(f) => f.clone(),
            None if self.boots => vec![
                "(no CP/M filesystem -- this disk".to_string(),
                " boots its own system, which keeps".to_string(),
                " its files its own way)".to_string(),
            ],
            None => vec!["(no CP/M filesystem)".to_string()],
        }
    }

    /// Can this disk sit on an emulator drive letter?  Only a disk with a CP/M
    /// filesystem: the emulator's BDOS reads the image's directory itself.
    pub fn mountable(&self) -> bool {
        self.files.is_some()
    }
}

/// The catalogue's summary line for a disk, as `repodisks.txt` words it.
fn summary(boots: bool, files: Option<&[String]>) -> String {
    let what = match files {
        None if boots => "its own operating system, no CP/M filesystem".to_string(),
        None => "data in another system's format".to_string(),
        Some([]) => "an empty CP/M directory".to_string(),
        Some(f) => {
            let what = super::layout::describe_contents(f)
                .unwrap_or_else(|| "assorted CP/M files".to_string());
            let n = f.len();
            format!("{what}, {n} file{}", if n == 1 { "" } else { "s" })
        }
    };
    let can = match (boots, files.is_some()) {
        (true, _) => "boots",
        (false, true) => "mount only",
        (false, false) => "neither",
    };
    format!("{can} -- {what}")
}

/// The processor the wizard recommends for a disk, and why, in one line.
pub fn recommended_cpu(disk: &DiskFacts) -> (&'static str, &'static str) {
    if disk.altair_basic {
        ("8080", "Altair BASIC was written for the 8080")
    } else {
        ("z80", Z80_WHY)
    }
}

/// The processor advised for a plan: the boot disk's, or the emulator's.
pub fn advised_cpu(runs: Runs, boot: Option<&DiskFacts>) -> (&'static str, &'static str) {
    match (runs, boot) {
        (Runs::Boot, Some(d)) => recommended_cpu(d),
        _ => (EMULATOR_CPU, Z80_WHY),
    }
}

/// The processor the wizard recommends for the emulator.
pub const EMULATOR_CPU: &str = "z80";

/// The machines a disk could be booted on -- those with a board that takes an
/// image its size -- detected one first.
pub fn machine_choices(disk: &DiskFacts) -> Vec<&'static super::console::MachineChoice> {
    let mut out: Vec<&'static super::console::MachineChoice> = super::console::MACHINE_CHOICES
        .iter()
        .filter(|m| super::boot_machine::BootMachine::machine_accepts(m.key, disk.bytes))
        .collect();
    if let Some(i) = out.iter().position(|m| m.key == disk.machine) {
        let detected = out.remove(i);
        out.insert(0, detected);
    }
    out
}

/// The monitor ROM a disk needs, when it needs one.
///
/// There is one ROM on offer and one disk measured to need it, so this names
/// it rather than searching: a second ROM would bring a second reason, and the
/// question of which disk wants which.
pub fn rom_for(disk: &DiskFacts) -> Option<&'static str> {
    disk.needs_rom.then_some("cuter_vdm")
}

/// The disks that can ride along with `boot` on `machine`: the same board,
/// never the boot disk itself -- and only disks with a CP/M filesystem.
///
/// **That last one was found live, not reasoned.**  The guest reads a
/// riding disk itself, so it seemed any format would do; but a disk rides
/// along by being *mounted* (`cpm_mounts`, lent to the boot), and a mount opens
/// the image's CP/M directory.  An Altair BASIC hard disk offered here was
/// refused at Save with "no CP/M directory found" (2026-10-21).
///
/// A board is looked up once per image size, not once per disk: a lookup
/// builds the machine's controllers, and the web page asks this for every
/// disk on every machine.
pub fn extra_disk_choices<'a>(boot: &DiskFacts, machine: &str, all: &'a [DiskFacts]) -> Vec<&'a DiskFacts> {
    let board_for = |len: u64| super::boot_machine::BootMachine::board_for(Some(machine), len);
    let Some(board) = board_for(boot.bytes) else {
        return Vec::new();
    };
    let mut same: std::collections::HashMap<u64, bool> = std::collections::HashMap::new();
    all.iter()
        .filter(|d| d.name != boot.name)
        .filter(|d| d.mountable())
        .filter(|d| *same.entry(d.bytes).or_insert_with(|| board_for(d.bytes) == Some(board)))
        .collect()
}

/// How many disks can ride along with `boot` on `machine` -- the board's
/// slots less the one it boots from.
pub fn extra_disk_room(boot: &DiskFacts, machine: &str) -> u8 {
    super::boot_machine::BootMachine::slot_count(Some(machine), boot.bytes)
        .unwrap_or(1)
        .saturating_sub(1)
}

/// What every slot is called for `boot` on `machine`, the boot disk's first
/// and then the [`extra_disk_room`] it leaves -- the board built once.
pub fn slot_names(boot: &DiskFacts, machine: &str) -> Vec<String> {
    let room = extra_disk_room(boot, machine);
    super::boot_machine::BootMachine::slot_labels(Some(machine), boot.bytes, room)
        .unwrap_or_else(|| (0..=room).map(|s| format!("slot {s}")).collect())
}

/// Every image in the images folder, read.  Blocking, see [`DiskFacts::read`].
pub fn all_disks(cpm_base: &Path) -> Vec<DiskFacts> {
    super::image::available_images(cpm_base)
        .iter()
        .filter_map(|n| DiskFacts::read(cpm_base, n))
        .collect()
}

/// The finished answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub runs: Runs,
    /// The disk to boot; `None` under the emulator.
    pub boot_disk: Option<String>,
    /// The machine to boot it on, as a config value -- `auto` when the
    /// operator kept what the disk was detected as, so the setting follows the
    /// disk if it is changed by hand later.
    pub machine: String,
    pub cpu: String,
    /// The monitor ROM, or `off`.
    pub rom: String,
    /// Images to mount, by drive (under the emulator) or slot (under a boot) --
    /// the two are one number here: slot N is drive letter N.
    pub disks: Vec<(u8, String)>,
}

impl Plan {
    /// A plan that runs the emulator with nothing mounted.
    pub fn emulator() -> Plan {
        Plan {
            runs: Runs::Emulator,
            boot_disk: None,
            machine: super::console::AUTO_MACHINE.to_string(),
            cpu: EMULATOR_CPU.to_string(),
            rom: super::rom::ROM_OFF.to_string(),
            disks: Vec::new(),
        }
    }

    /// A plan that boots `disk` with every recommendation taken.
    pub fn boot(disk: &DiskFacts) -> Plan {
        Plan {
            runs: Runs::Boot,
            boot_disk: Some(disk.name.clone()),
            machine: super::console::AUTO_MACHINE.to_string(),
            cpu: recommended_cpu(disk).0.to_string(),
            rom: rom_for(disk).unwrap_or(super::rom::ROM_OFF).to_string(),
            disks: Vec::new(),
        }
    }

    /// The first answer.  Only a chosen boot disk is forgotten: coming back
    /// to the emulator from one starts it afresh, but looking at Boot and
    /// backing out before a disk was picked keeps the drives already chosen
    /// -- nothing boot-specific was set.  A boot keeps what it has until its
    /// disk is chosen ([`Plan::choose_boot_disk`] starts it afresh).
    pub fn choose_runs(&mut self, runs: Runs) {
        match runs {
            Runs::Emulator if self.boot_disk.is_some() => *self = Plan::emulator(),
            _ => self.runs = runs,
        }
    }

    /// The boot disk.  The same disk again keeps what was chosen for it; a
    /// different one starts from its recommendations.
    pub fn choose_boot_disk(&mut self, disk: &DiskFacts) {
        if self.runs != Runs::Boot || self.boot_disk.as_deref() != Some(disk.name.as_str()) {
            *self = Plan::boot(disk);
        }
    }

    /// The machine, by key.  The one the disk was read as is kept as `auto`,
    /// so the setting follows the disk if it is changed by hand later.  A
    /// different machine can be a different board, and disks chosen for the
    /// old one would not fit it, so they go; the same one keeps them.
    pub fn choose_machine(&mut self, key: &str, boot: &DiskFacts) {
        let m = if key == boot.machine { super::console::AUTO_MACHINE } else { key };
        if m != self.machine {
            self.disks.clear();
        }
        self.machine = m.to_string();
    }

    /// The machine the boot will use: the plan's, or the disk's own when the
    /// plan says to detect it.
    pub fn machine_in_force(&self, boot: &DiskFacts) -> String {
        if self.machine == super::console::AUTO_MACHINE {
            boot.machine.clone()
        } else {
            self.machine.clone()
        }
    }

    /// Whether the plan's boot disk wants a monitor ROM, for [`Step::next`].
    pub fn needs_rom(&self, all: &[DiskFacts]) -> bool {
        self.runs == Runs::Boot && self.boot_in(all).is_some_and(|d| d.needs_rom)
    }

    /// The plan's boot disk among `all`.
    pub fn boot_in<'a>(&self, all: &'a [DiskFacts]) -> Option<&'a DiskFacts> {
        let name = self.boot_disk.as_deref()?;
        all.iter().find(|d| d.name == name)
    }

    /// The settings these answers come to, as config key/value pairs.
    ///
    /// `cpm_emu_enabled` is switched on too: the wizard ends by sending the
    /// operator to `K`, and with the emulator off that item is not on the menu.
    pub fn settings(&self) -> Vec<(&'static str, String)> {
        vec![
            ("cpm_emu_enabled", "true".to_string()),
            ("cpm_boot_image", self.boot_disk.clone().unwrap_or_default()),
            ("cpm_boot_machine", self.machine.clone()),
            ("cpm_cpu", self.cpu.clone()),
            ("cpm_boot_rom", self.rom.clone()),
        ]
    }

    /// What the plan will do, line by line, for the last screen.
    pub fn describe(&self, transfer_dir: &str) -> Vec<String> {
        let mut out = Vec::new();
        // The booted disk's machine and size, so a slot is named by its board.
        let mut board_of: Option<(String, u64)> = None;
        match (&self.runs, &self.boot_disk) {
            (Runs::Boot, Some(disk)) => {
                out.push(format!("Boot {disk}"));
                let base = super::layout::cpm_dir(transfer_dir);
                let path = super::image::images_dir(&base).join(disk);
                let machine = if self.machine == super::console::AUTO_MACHINE {
                    super::boot::machine_for_image(&path, &self.machine)
                } else {
                    self.machine.clone()
                };
                out.push(format!("on {}", super::console::machine_label(&machine)));
                let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                board_of = Some((machine, len));
            }
            _ => out.push(EMULATOR_LABEL.to_string()),
        }
        out.push(format!("Processor: {}", super::cpu::cpu_short(&self.cpu)));
        if self.rom != super::rom::ROM_OFF {
            out.push(format!("Monitor ROM: {}", super::rom::short_label(&self.rom)));
        }
        if self.disks.is_empty() {
            out.push("No other disks".to_string());
        }
        for (n, name) in &self.disks {
            let at = match (&self.runs, &board_of) {
                (Runs::Boot, Some((machine, len))) => {
                    super::boot_machine::BootMachine::slot_label(Some(machine), *len, *n)
                        .map(|l| format!("{l}:"))
                        .unwrap_or_else(|| format!("slot {n}:"))
                }
                _ => format!("{}:", (b'A' + n) as char),
            };
            out.push(format!("{at} {name}"));
        }
        out
    }
}

/// Is this plan one the wizard could have built from `all`?  `Err` is what to
/// tell the operator.
///
/// **Every surface's rule, enforced in one place.**  The pickers only offer
/// what may be chosen, but a picker is a convenience, not a check: the web
/// page's answers arrive as a form anyone on the LAN can write.  So this is
/// what [`apply`] asks before it writes anything, and what each surface asks
/// at its disks step to say what is wrong while the operator is still there:
///
/// * the processor and ROM are ones their keys accept;
/// * under the emulator, nothing boots and each image is on a drive B: to P:
///   and has a CP/M filesystem to show there;
/// * under a boot, the disk has been seen to boot, the machine is one that
///   carries it, and the riding disks are among [`extra_disk_choices`], in
///   the board's slots 1 up, with no gap -- an empty slot between two full
///   ones looks like a hang to a guest that picks it;
/// * no image twice, no drive or slot twice.
pub fn check_plan(plan: &Plan, all: &[DiskFacts]) -> Result<(), String> {
    if !matches!(plan.cpu.as_str(), "z80" | "8080") {
        return Err("choose a processor".to_string());
    }
    if !super::rom::is_valid_rom_key(&plan.rom) {
        return Err("choose a monitor ROM, or none".to_string());
    }
    for (i, (n, name)) in plan.disks.iter().enumerate() {
        if plan.disks[i + 1..].iter().any(|(_, b)| b == name) {
            return Err(format!("{name} is chosen twice; one image can be in one place."));
        }
        if plan.disks[i + 1..].iter().any(|(m, _)| m == n) {
            return Err("Two disks are on one drive.".to_string());
        }
    }
    let find = |name: &str| all.iter().find(|d| d.name == name);
    match plan.runs {
        Runs::Emulator => {
            if plan.boot_disk.is_some() || plan.rom != super::rom::ROM_OFF {
                return Err("the emulator boots no disk".to_string());
            }
            for (n, name) in &plan.disks {
                if !(1..super::NUM_DRIVES).contains(n) {
                    return Err(format!("{name} cannot go on drive {n}"));
                }
                if !find(name).is_some_and(|d| d.mountable()) {
                    return Err(format!("{name} has no CP/M files to put on a drive."));
                }
            }
        }
        Runs::Boot => {
            let name = plan.boot_disk.as_deref().unwrap_or_default();
            let Some(boot) = find(name) else {
                return Err("choose a boot disk".to_string());
            };
            if !boot.boots {
                return Err(format!("{name} has not been seen to boot here."));
            }
            if plan.machine != super::console::AUTO_MACHINE
                && !machine_choices(boot).iter().any(|m| m.key == plan.machine)
            {
                return Err(format!("{name} does not fit that machine."));
            }
            let machine = plan.machine_in_force(boot);
            let room = extra_disk_room(boot, &machine);
            let choices = extra_disk_choices(boot, &machine, all);
            let mut slots: Vec<&(u8, String)> = plan.disks.iter().collect();
            slots.sort();
            for (i, (n, x)) in slots.into_iter().enumerate() {
                if *n == 0 || *n > room {
                    return Err(format!("The board has room for {room} more disk{}.", if room == 1 { "" } else { "s" }));
                }
                if *n as usize != i + 1 {
                    return Err("Fill the slots in order, with no empty slot between two disks.".to_string());
                }
                if !choices.iter().any(|d| d.name == *x) {
                    return Err(format!("{x} is not a disk this board reads."));
                }
            }
        }
    }
    Ok(())
}

/// Make the plan the configuration.  Blocking (it reads the images, writes
/// the config file and opens images to mount them).
///
/// **The plan is checked first** ([`check_plan`], against the disks as they
/// are now), and an `Err` is the reason nothing was changed.  Past that, the
/// settings are written and then the mounts made; a mount can still fail --
/// a drive a session is using -- and the settings are not taken back for it,
/// so the notes then *begin* by saying the save was partial, and which disks
/// are not where the plan put them.
///
/// **The mount table becomes exactly the plan's**: a mount not in it is taken
/// off.  The wizard's last screen says so before it is confirmed, because the
/// point of a wizard is that its answers are the whole configuration, and a
/// mount left over from an earlier setup is the kind of thing that makes a
/// booted disk find a stranger in its slot 1.
pub fn apply(plan: &Plan, transfer_dir: &str) -> Result<Vec<String>, String> {
    let base = super::layout::cpm_dir(transfer_dir);
    check_plan(plan, &all_disks(&base))?;
    let pairs = plan.settings();
    let borrowed: Vec<(&str, &str)> = pairs.iter().map(|(k, v)| (*k, v.as_str())).collect();
    crate::config::update_config_values(&borrowed);
    let (mounted, errors) = super::image::apply_mount_selection(&base, &plan.disks);
    crate::config::update_config_value("cpm_mounts", &super::image::current_mounts_value());
    let mut notes = Vec::new();
    if !errors.is_empty() {
        notes.push("The settings are saved, but these disks are not where the plan put them:".to_string());
        notes.extend(errors);
    }
    notes.extend(mounted);
    if plan.rom != super::rom::ROM_OFF && super::rom::missing(&base, &plan.rom) {
        notes.push(
            "The monitor ROM is not here yet: fetch it from CP/M settings (or Download sample disks)."
                .to_string(),
        );
    }
    Ok(notes)
}

/// Where the images live, for a surface that has only `transfer_dir`.
pub fn cpm_base(transfer_dir: &str) -> PathBuf {
    super::layout::cpm_dir(transfer_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every explanation fits a C64 with the telnet screens' indent.
    #[test]
    fn test_every_explanation_fits_a_narrow_screen() {
        for (what, text) in [
            ("runs", RUNS_HELP),
            ("emulator drives", EMULATOR_DRIVES_HELP),
            ("boot disk", BOOT_DISK_HELP),
            ("machine", MACHINE_HELP),
            ("cpu", CPU_HELP),
            ("extra disks", EXTRA_DISKS_HELP),
            ("rom", ROM_HELP),
            ("notes", &[BASIC_NOTE, ROM_NOTE]),
        ] {
            for line in text {
                assert!(line.chars().count() <= TEXT_WIDTH, "{what}: {line:?}");
                assert!(line.is_ascii(), "{what}: {line:?} is not ASCII");
            }
        }
        assert!(FINISH_LINE.len() + 2 <= 40, "{FINISH_LINE:?}");
    }

    fn disk(name: &str, basic: bool, rom: bool) -> DiskFacts {
        DiskFacts {
            name: name.into(),
            bytes: 337_568,
            boots: true,
            files: None,
            summary: String::new(),
            machine: "altair_2sio".into(),
            machine_why: None,
            board: Some("MITS 88-DCDD floppy"),
            slots: Some(16),
            altair_basic: basic,
            needs_rom: rom,
        }
    }

    /// The 8080 is recommended for Altair BASIC and only for it; a disk that
    /// needs the monitor ROM gets it, and no other disk does.
    #[test]
    fn test_the_recommendations_follow_the_disk() {
        assert_eq!(recommended_cpu(&disk("DISK03.DSK", true, false)).0, "8080");
        assert_eq!(recommended_cpu(&disk("DISK01.DSK", false, false)).0, "z80");
        let p = Plan::boot(&disk("DISK03.DSK", true, false));
        assert_eq!((p.cpu.as_str(), p.rom.as_str()), ("8080", "off"));
        let p = Plan::boot(&disk("DISK11.DSK", false, true));
        assert_eq!(p.rom, "cuter_vdm");
        assert!(crate::cpm::rom::is_valid_rom_key(&p.rom), "the ROM key the wizard names exists");
    }

    /// The steps in order, for each way through.
    #[test]
    fn test_the_steps_follow_the_answers() {
        let walk = |runs: Runs, rom: bool| {
            let mut out = vec![Step::Runs];
            while let Some(to) = out.last().unwrap().next(runs, rom) {
                out.push(to);
            }
            out
        };
        use Step as S;
        assert_eq!(walk(Runs::Emulator, false), [S::Runs, S::EmulatorDrives, S::Cpu, S::Ready]);
        assert_eq!(walk(Runs::Boot, false), [S::Runs, S::BootDisk, S::Machine, S::Cpu, S::ExtraDisks, S::Ready]);
        assert_eq!(
            walk(Runs::Boot, true),
            [S::Runs, S::BootDisk, S::Machine, S::Cpu, S::Rom, S::ExtraDisks, S::Ready]
        );
        // The page's table says the same, under the page's names.
        let table = Step::table();
        let at = |k: &str| table.iter().find(|(x, _)| x == k).map(|(_, v)| *v);
        assert_eq!(at("cpu|boot|1"), Some("rom"));
        assert_eq!(at("cpu|boot|0"), Some("extra"));
        assert_eq!(at("cpu|emulator|0"), Some("ready"));
        assert_eq!(at("ready|boot|0"), None);
    }

    /// What each answer keeps.  Telnet's `E` used to start the emulator plan
    /// afresh even when it was the emulator already, losing the drives the
    /// other two surfaces kept.
    #[test]
    fn test_an_answer_keeps_what_it_should() {
        let mut p = Plan::emulator();
        p.disks = vec![(2, "TOOLS.DSK".into())];
        p.choose_runs(Runs::Emulator);
        assert_eq!(p.disks.len(), 1, "choosing the emulator again lost its drives");
        // Boot looked at, backed out of before a disk was picked.
        p.choose_runs(Runs::Boot);
        p.choose_runs(Runs::Emulator);
        assert_eq!((p.runs, p.disks.len()), (Runs::Emulator, 1), "a look at Boot lost the drives");
        let d = disk("DISK01.DSK", false, false);
        p.choose_runs(Runs::Boot);
        p.choose_boot_disk(&d);
        assert_eq!(p.boot_disk.as_deref(), Some("DISK01.DSK"));
        assert!(p.disks.is_empty(), "a new boot disk starts afresh");
        p.disks = vec![(1, "DISK12.DSK".into())];
        p.choose_boot_disk(&d);
        assert_eq!(p.disks.len(), 1, "the same boot disk again lost its choices");
        p.choose_machine("altair_2sio", &d);
        assert_eq!(p.machine, "auto", "the detected machine is kept as auto");
        assert_eq!(p.disks.len(), 1, "the same machine lost its disks");
        p.choose_machine("altair_sio", &d);
        assert_eq!(p.machine, "altair_sio");
        assert!(p.disks.is_empty(), "another machine kept the old board's disks");
        p.choose_runs(Runs::Emulator);
        assert_eq!(p, Plan::emulator(), "the emulator from a boot starts afresh");
    }

    /// Everything the pickers prevent, refused again by the check -- the web
    /// form can say anything.
    #[test]
    fn test_the_check_refuses_what_no_picker_offers() {
        let floppy = |name: &str| DiskFacts { files: Some(vec!["PIP.COM".into()]), ..disk(name, false, false) };
        let all = vec![
            floppy("BOOT.DSK"),
            floppy("B.DSK"),
            floppy("C.DSK"),
            DiskFacts { boots: false, ..floppy("DATA.DSK") },
            disk("NOFS.DSK", false, false),
            DiskFacts { bytes: 4_988_928, ..floppy("HARD.DSK") },
            DiskFacts { boots: false, ..floppy("DEAD.DSK") },
        ];
        let boot = |disks: &[(u8, &str)]| {
            let mut p = Plan::boot(&all[0]);
            p.disks = disks.iter().map(|(n, s)| (*n, s.to_string())).collect();
            p
        };
        assert_eq!(check_plan(&boot(&[(1, "B.DSK"), (2, "C.DSK")]), &all), Ok(()));
        assert_eq!(check_plan(&boot(&[]), &all), Ok(()));
        for (why, p) in [
            ("a gap", boot(&[(1, "B.DSK"), (3, "C.DSK")])),
            ("no slot 1", boot(&[(2, "B.DSK")])),
            ("slot 0 is the boot disk's", boot(&[(0, "B.DSK")])),
            ("past the board's room", boot(&[(16, "B.DSK")])),
            ("one image twice", boot(&[(1, "B.DSK"), (2, "B.DSK")])),
            ("the boot disk riding along", boot(&[(1, "BOOT.DSK")])),
            ("no CP/M filesystem", boot(&[(1, "NOFS.DSK")])),
            ("another board", boot(&[(1, "HARD.DSK")])),
            ("not in the folder", boot(&[(1, "GONE.DSK")])),
        ] {
            assert!(check_plan(&p, &all).is_err(), "{why} was accepted");
        }
        // Sixteen in order is past a board of sixteen slots, the boot disk's
        // among them -- the room bound, not the gap rule, refuses it.
        let mut many = all.clone();
        many.extend((1..=16).map(|i| floppy(&format!("F{i:02}.DSK"))));
        let mut p = boot(&[]);
        p.disks = (1..=16).map(|i| (i, format!("F{i:02}.DSK"))).collect();
        assert!(check_plan(&p, &many).is_err(), "more disks than the board has slots");
        p.disks.pop();
        assert_eq!(check_plan(&p, &many), Ok(()), "a full board");
        let mut p = boot(&[]);
        p.boot_disk = Some("DEAD.DSK".into());
        assert!(check_plan(&p, &all).is_err(), "a disk never seen to boot");
        let mut p = boot(&[]);
        p.machine = "cromemco".into();
        assert!(check_plan(&p, &all).is_err(), "a machine with no board for the disk");
        let mut p = boot(&[]);
        p.cpu = "6502".into();
        assert!(check_plan(&p, &all).is_err(), "a processor");

        let emu = |disks: &[(u8, &str)]| {
            let mut p = Plan::emulator();
            p.disks = disks.iter().map(|(n, s)| (*n, s.to_string())).collect();
            p
        };
        assert_eq!(check_plan(&emu(&[(1, "B.DSK"), (5, "DATA.DSK"), (15, "HARD.DSK")]), &all), Ok(()));
        for (why, p) in [
            ("drive A:", emu(&[(0, "B.DSK")])),
            ("past P:", emu(&[(16, "B.DSK")])),
            ("no CP/M filesystem", emu(&[(1, "NOFS.DSK")])),
            ("one image twice", emu(&[(1, "B.DSK"), (2, "B.DSK")])),
            ("one drive twice", emu(&[(1, "B.DSK"), (1, "C.DSK")])),
        ] {
            assert!(check_plan(&p, &all).is_err(), "{why} was accepted");
        }
    }

    /// `apply` checks the plan before it writes a setting, and a save that
    /// mounted only some disks says so first.
    #[test]
    fn test_apply_checks_before_it_writes() {
        let src = include_str!("wizard.rs").replace('\r', "");
        let f = &src[src.find("pub fn apply(plan: &Plan").expect("apply")..];
        let f = &f[..f.find("\n}\n").expect("its end")];
        let check = f.find("check_plan(").expect("apply does not check the plan");
        let write = f.find("update_config_values(").expect("apply writes the settings");
        assert!(check < write, "the settings are written before the plan is checked");
        let partial = f.find("not where the plan put them").expect("a partial save is not said");
        assert!(partial < f.find("notes.extend(mounted)").expect("the mount notes"), "the partial save is not said first");
    }

    /// The emulator plan boots nothing and switches the emulator on; a boot
    /// plan names its disk.  Every value is one its key accepts.
    #[test]
    fn test_a_plan_comes_to_valid_settings() {
        let check = |pairs: Vec<(&'static str, String)>| {
            let mut cfg = crate::config::Config::default();
            for (k, v) in &pairs {
                crate::config::apply_config_key(&mut cfg, k, v);
            }
            cfg
        };
        let cfg = check(Plan::emulator().settings());
        assert!(cfg.cpm_emu_enabled);
        assert_eq!(cfg.cpm_boot_image, "");
        assert_eq!(cfg.cpm_cpu, "z80");
        let mut p = Plan::boot(&disk("DISK03.DSK", true, false));
        p.machine = "altair_2sio".into();
        let cfg = check(p.settings());
        assert_eq!(cfg.cpm_boot_image, "DISK03.DSK");
        assert_eq!(cfg.cpm_cpu, "8080");
        assert_eq!(cfg.cpm_boot_machine, "altair_2sio");
    }

    /// **What the wizard says about real disks.**  Over the Altair8800
    /// collection: an Altair BASIC disk is recommended the 8080, a CP/M disk
    /// the Z80; `DISK11` asks for its monitor ROM; the hard disk is on the
    /// 88-HDSK with sixteen platter slots; and a floppy's extra-disk choices
    /// are floppies on its board, never the hard disk.
    ///
    /// Ignored -- it needs the collection:
    ///     REPODISKS_HOME=~ cargo test --bin ethernetgateway \
    ///         test_the_wizard_reads_real_disks -- --ignored
    #[cfg(unix)]
    #[test]
    #[ignore]
    fn test_the_wizard_reads_real_disks() {
        let home = std::env::var("REPODISKS_HOME").or_else(|_| std::env::var("HOME")).expect("a home");
        let src = Path::new(&home).join("AltairRepos/Altair8800/disks");
        assert!(src.is_dir(), "no {}", src.display());
        // A transfer folder whose CPM/images is the collection.
        let td = std::env::temp_dir().join(format!("egw_wizard_real_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&td);
        std::fs::create_dir_all(td.join("CPM")).unwrap();
        std::os::unix::fs::symlink(&src, td.join("CPM").join(crate::cpm::image::IMAGES_DIR)).unwrap();
        let transfer_dir = td.to_string_lossy().to_string();
        let base = cpm_base(&transfer_dir);

        let basic = DiskFacts::read(&base, "DISK03.DSK").unwrap();
        assert!(basic.boots && basic.altair_basic && basic.files.is_none(), "{basic:?}");
        assert_eq!(recommended_cpu(&basic).0, "8080");
        let cpm = DiskFacts::read(&base, "DISK01.DSK").unwrap();
        assert!(cpm.boots && !cpm.altair_basic && cpm.mountable(), "{cpm:?}");
        assert_eq!(recommended_cpu(&cpm).0, "z80");
        assert!(cpm.summary.starts_with("boots -- "), "{}", cpm.summary);
        assert!(cpm.listing().iter().any(|f| f == "PIP.COM"), "{:?}", cpm.listing());
        let rom = DiskFacts::read(&base, "DISK11.DSK").unwrap();
        assert_eq!(rom_for(&rom), Some("cuter_vdm"));
        let hd = DiskFacts::read(&base, "HDSK01.DSK").unwrap();
        assert_eq!((hd.board, hd.slots), (Some("MITS 88-HDSK hard disk"), Some(16)), "{hd:?}");
        assert_eq!(extra_disk_room(&hd, &hd.machine), 15);
        assert_eq!(slot_names(&hd, &hd.machine)[1], "unit 0.1");
        assert_eq!(slot_names(&hd, &hd.machine).len(), 16, "the boot disk's slot and fifteen more");

        let all = all_disks(&base);
        let extra = extra_disk_choices(&cpm, &cpm.machine, &all);
        assert!(extra.iter().any(|d| d.name == "DISK12.DSK"), "another CP/M floppy rides along");
        assert!(!extra.iter().any(|d| d.name == "DISK02.DSK"), "a floppy with no CP/M filesystem was offered");
        assert!(!extra.iter().any(|d| d.name.starts_with("HDSK")), "a hard disk was offered to a floppy");
        assert!(!extra.iter().any(|d| d.name == "DISK01.DSK"), "the boot disk was offered twice");
        // A disk rides along by being mounted, so a disk with no CP/M
        // filesystem cannot: the Altair BASIC hard disk is not offered to a
        // CP/M hard disk on the same board.
        let hd3 = DiskFacts::read(&base, "HDSK03.DSK").unwrap();
        let extra = extra_disk_choices(&hd3, &hd3.machine, &all);
        assert!(!extra.iter().any(|d| d.name == "HDSK01.DSK"), "a disk no mount can open was offered");
        assert!(extra.iter().all(|d| d.mountable()));
        // And the summary names slots the board's way.
        let mut plan = Plan::boot(&hd3);
        plan.disks = vec![(1, "HDSK05.DSK".into())];
        let lines = plan.describe(&transfer_dir);
        assert!(lines.iter().any(|l| l == "unit 0.1: HDSK05.DSK"), "{lines:?}");
        let _ = std::fs::remove_dir_all(&td);
    }

    /// Bootability is measured as the wizard will set the disk up, never under
    /// the settings it is about to replace.
    #[test]
    fn test_bootability_is_not_asked_under_the_settings_in_force() {
        let src = include_str!("wizard.rs").replace('\r', "");
        let f = &src[src.find("fn measure(path: &Path, name: &str)").expect("measure")..];
        let f = &f[..f.find("\n    }\n").expect("its end")];
        assert!(f.contains("image_can_boot_on_with("), "bootability is not asked of the plan's machine");
        assert!(!f.contains(concat!("image_can_", "boot(path)")), "asked under the configured machine and CPU");
    }

    /// A file's identity is taken before it is read: taken after, a disk
    /// caught mid-write would be remembered by its partial bytes under its
    /// finished identity, and stay hidden until it changed again.
    #[test]
    fn test_a_disk_is_stat_before_it_is_read() {
        let src = include_str!("wizard.rs").replace('\r', "");
        let f = &src[src.find("fn measure(path: &Path, name: &str)").expect("measure")..];
        let f = &f[..f.find("\n    }\n").expect("its end")];
        let stat = f.find("metadata(path)").expect("measure takes the file's identity");
        let read = f.find("fs::read(path)").expect("measure reads the file");
        assert!(stat < read, "the identity is taken after the read");
        assert!(f.contains("image_can_boot_on_with(path, &meta,"), "bootability is not keyed by that identity");
    }

    /// The summary line reads as the catalogue's does.
    #[test]
    fn test_the_summary_uses_the_catalogue_words() {
        assert_eq!(summary(true, None), "boots -- its own operating system, no CP/M filesystem");
        assert_eq!(summary(false, None), "neither -- data in another system's format");
        let files: Vec<String> = vec!["MBASIC.COM".into(), "PIP.COM".into()];
        assert!(summary(false, Some(&files)).starts_with("mount only -- BASIC"), "{}", summary(false, Some(&files)));
    }
}
