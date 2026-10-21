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

/// The last screen's instruction, on every surface.
pub const FINISH_LINE: &str = "Press K on the main menu to start.";

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
    /// Read everything about one image.  Blocking -- it reads the whole file,
    /// and the first bootability answer for a file cold-starts it -- so an
    /// async caller goes through `spawn_blocking`.
    pub fn read(cpm_base: &Path, name: &str) -> Option<DiskFacts> {
        if !super::image::is_safe_image_name(name) {
            return None;
        }
        let path = super::image::images_dir(cpm_base).join(name);
        let bytes = std::fs::read(&path).ok()?;
        let len = bytes.len() as u64;
        let files = super::image::files_on(&path);
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
        facts.boots = super::boot::image_can_boot_on(
            &path,
            super::console::AUTO_MACHINE,
            recommended_cpu(&facts).0,
        );
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
        ("z80", "the Z80 runs nearly all 8080 software")
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
pub fn extra_disk_choices<'a>(boot: &DiskFacts, machine: &str, all: &'a [DiskFacts]) -> Vec<&'a DiskFacts> {
    let board = super::boot_machine::BootMachine::board_for(Some(machine), boot.bytes);
    all.iter()
        .filter(|d| d.name != boot.name)
        .filter(|d| d.mountable())
        .filter(|d| board.is_some() && super::boot_machine::BootMachine::board_for(Some(machine), d.bytes) == board)
        .collect()
}

/// How many disks can ride along with `boot` on `machine` -- the board's
/// slots less the one it boots from.
pub fn extra_disk_room(boot: &DiskFacts, machine: &str) -> u8 {
    super::boot_machine::BootMachine::slot_count(Some(machine), boot.bytes)
        .unwrap_or(1)
        .saturating_sub(1)
}

/// What a slot is called for `boot` on `machine` -- the board's own words.
pub fn slot_name(boot: &DiskFacts, machine: &str, slot: u8) -> String {
    super::boot_machine::BootMachine::slot_label(Some(machine), boot.bytes, slot)
        .unwrap_or_else(|| format!("slot {slot}"))
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

/// Make the plan the configuration.  Blocking (it writes the config file and
/// opens images to mount them).  Returns notes worth showing -- a drive that
/// could not be changed because a session is using it, say -- or the reason
/// nothing was changed.
///
/// **The mount table becomes exactly the plan's**: a mount not in it is taken
/// off.  The wizard's last screen says so before it is confirmed, because the
/// point of a wizard is that its answers are the whole configuration, and a
/// mount left over from an earlier setup is the kind of thing that makes a
/// booted disk find a stranger in its slot 1.
pub fn apply(plan: &Plan, transfer_dir: &str) -> Result<Vec<String>, String> {
    let base = super::layout::cpm_dir(transfer_dir);
    if let Some(disk) = &plan.boot_disk
        && !super::image::is_safe_image_name(disk)
    {
        return Err(format!("{disk} is not a disk image name"));
    }
    let pairs = plan.settings();
    let borrowed: Vec<(&str, &str)> = pairs.iter().map(|(k, v)| (*k, v.as_str())).collect();
    crate::config::update_config_values(&borrowed);
    let (mut notes, errors) = super::image::apply_mount_selection(&base, &plan.disks);
    crate::config::update_config_value("cpm_mounts", &super::image::current_mounts_value());
    notes.extend(errors);
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
        assert_eq!(slot_name(&hd, &hd.machine, 1), "unit 0.1");

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
        let f = &src[src.find("pub fn read(cpm_base: &Path, name: &str)").expect("read")..];
        let f = &f[..f.find("\n    }\n").expect("its end")];
        assert!(f.contains("image_can_boot_on("), "bootability is not asked of the plan's machine");
        assert!(!f.contains(concat!("image_can_", "boot(&path)")), "asked under the configured machine and CPU");
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
