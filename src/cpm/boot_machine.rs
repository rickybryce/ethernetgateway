//! The machine a booted disk runs on.
//!
//! This is the other half of the boot path: 64 KB of memory, the 88-DCDD on
//! ports 08h–0Ah, an 88-2SIO console on 10h/11h, the front-panel sense switches
//! on FFh, and — when the operator has selected a port profile that fits — the
//! virtual modem. No BDOS, no page-zero vectors, no CCP: the disk's own
//! operating system supplies all of that. Our part is to be plausible hardware
//! and get out of the way.
//!
//! # How this differs from the CP/M emulator next door
//!
//! `cpm::mod` traps BDOS and BIOS calls and services them against a filesystem
//! we control. Here nothing is trapped. That is the trade the booted path
//! makes, and it has consequences worth stating rather than discovering:
//!
//! * The guest owns the drives. Mounted images are handed to it, each at the
//!   unit its drive letter names, but it names them itself and reaches only as
//!   many as its own BIOS knows — stock Altair CP/M knows four.
//!   Folder-backed drives, the jail, `EXIT` and the
//!   Gateway Shell do not exist inside it.
//! * The blast radius is the images in the drives — narrower than the
//!   filesystem path's, and easier to state, but the per-file write claim that
//!   stops two sessions interleaving records has no meaning here. A booted
//!   image is therefore held by one session and opened read-only unless the
//!   operator says otherwise.
//! * Unknown ports read as `0xFF` (an idle bus) rather than as whatever was
//!   last driven, so a guest probing for hardware we do not have sees nothing
//!   instead of an echo of itself.

use super::boot::{cold_boot, BootError};
use super::controller::{ColdStart, Controller, HostRequest};
use super::dcdd::{Dcdd, SECTOR_LEN};
use super::modem_port::ModemPort;
use super::uart::ModemAccess;
use iz80::{Cpu, Machine};

/// How long a received character takes to come down the line, in instructions.
///
/// A real console is a serial line: characters are spaced by a character time,
/// and software written for one is entitled to assume that reading the data
/// register twice in quick succession cannot produce two different keystrokes.
/// CDOS 2.58 assumes exactly that — see [`BootMachine::rx_ready`] for the
/// measurement.
///
/// The value only has to exceed the few instructions between a guest's two
/// reads, and it costs nothing: even a long pasted line is a few hundred
/// thousand instructions, against the tens of millions a single `DIR` takes.
/// 2,000 is around a character time at 9,600 baud on a machine of this speed,
/// and a probe showed 1,000 already sufficient — so this is that with margin
/// rather than a figure tuned until one disk passed.
const RX_CHARACTER_TIME: u32 = 2_000;

/// The live controller a board name stands for.
///
/// The one place that turns [`super::console::Board`] into hardware, so a new
/// board is a `Controller` impl plus one arm here — and so the machine list can
/// stay a `const` while controllers are live objects with latched registers.
fn boards_to_controller(board: super::console::Board) -> Box<dyn Controller> {
    use super::console::Board;
    match board {
        Board::Dcdd => Box::new(Dcdd::new()),
        Board::Hdsk => Box::new(super::hdsk::Hdsk::new()),
        Board::Tarbell => Box::new(super::tarbell::Tarbell::new()),
        Board::Z80pack => Box::new(super::z80pack::Z80pack::new()),
        Board::Cromemco => Box::new(super::cromemco::Cromemco::new()),
    }
}

/// Every board any machine here can carry, for the questions that are about the
/// gateway rather than about one machine.
///
/// [`BootMachine::medium_for`] and [`BootMachine::bootable_media`] answer "could
/// this file be booted *at all*", which is a different question from "does the
/// currently configured machine carry it" — the boot picker and the generated
/// readme both want the former. Built from the machine list rather than written
/// out, so a board reachable from no machine cannot claim to be bootable.
fn all_controllers() -> Vec<Box<dyn Controller>> {
    let mut seen = Vec::new();
    let mut out = Vec::new();
    for m in super::console::MACHINE_CHOICES {
        for b in m.boards {
            if !seen.contains(b) {
                seen.push(*b);
                out.push(boards_to_controller(*b));
            }
        }
    }
    out
}

/// The front-panel sense switches, read on port FFh.
pub const SENSE_SWITCH_PORT: u8 = 0xFF;

/// What the sense switches say when nobody has set them.
///
/// This is not a neutral choice and it is not zero-by-accident. MITS system
/// software — Altair DOS, Disk BASIC, Time Sharing BASIC — asks the *front
/// panel* which console board the machine has, because in 1976 that genuinely
/// varied from machine to machine. It reads port FFh and picks a terminal
/// driver from the low four bits: 88-SIO on ports 00h/01h, 88-ACR on 06h/07h,
/// 4PIO on 20h-23h, or the 88-2SIO on 10h/11h.
///
/// The 88-2SIO is the one we emulate, and zero is the setting that selects it.
/// Leaving the port floating at FFh instead selected the 88-SIO — so Altair DOS
/// booted perfectly, wrote its sign-on to port 01h, and we dropped every byte
/// on the floor. The disk was never the problem; the front panel was.
///
/// It is a constant rather than a setting because there is nothing yet for an
/// operator to set it from. When the boot path gets its config key and its
/// screens, the switches belong there too: they are the one control that
/// decides what hardware MITS software believes it is talking to.
pub const DEFAULT_SENSE_SWITCHES: u8 = 0x00;

/// Console bytes a guest may have waiting before further ones are dropped.
///
/// Matches the emulator's pending-input bound. Generous next to anything a
/// person types, and small enough that a client streaming into a guest which
/// never reads its console cannot grow the host's memory.
const KEY_QUEUE_CAP: usize = 4096;

/// What happened when the configured virtual modem was offered to a booted
/// machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModemAttach {
    /// The operator has no virtual modem selected.
    Off,
    /// Wired up at these status and data ports.
    Ports(u8, u8),
    /// Selected, but it cannot exist in a booted machine — with the reason,
    /// which is shown to whoever is booting rather than logged and forgotten.
    Unavailable(String),
}

/// One image in a drive.
///
/// No geometry here any more: where a byte lives is the controller's
/// arithmetic, and the machine's only job is to copy the range it is given.
struct Mounted {
    bytes: Vec<u8>,
    read_only: bool,
    dirty: bool,
}

/// Memory, ports and drives for a booted disk.
pub struct BootMachine {
    mem: Vec<u8>,
    /// Which controller took the disk in each drive.
    ///
    /// Recorded at insert time because a drive number alone is ambiguous once
    /// there is more than one board: drive 0 of the floppy controller and
    /// drive 0 of the hard disk are different drives. Without this, booting a
    /// hard disk asked the *floppy* to cold-start and got told its drive was
    /// empty — true, and completely misleading.
    disk_controller: Vec<Option<usize>>,

    /// The controllers this machine carries, each claiming its own ports.
    ///
    /// A list rather than a field per board: adding the 88-HDSK should be
    /// writing a `Controller` and pushing it here, not surgery on the port
    /// dispatch.  One entry today.
    controllers: Vec<Box<dyn Controller>>,
    disks: Vec<Option<Mounted>>,
    /// Bytes the guest has printed.
    tx: Vec<u8>,
    /// Bytes waiting for the guest to read.
    rx: std::collections::VecDeque<u8>,
    /// Instructions left before the next received character has "arrived".
    ///
    /// See [`BootMachine::rx_ready`]. Counted in instructions rather than in
    /// anything finer because that is the only clock this machine has, and the
    /// quantity being modelled — a character time — only has to be long enough
    /// that a guest's lookahead finds the line empty.
    rx_hold: u32,
    /// Reads of the console status register since the last one that reported
    /// anything. A guest waiting forever on a key is normal; a guest waiting
    /// forever on a *disk* is not, and the two are told apart by this and
    /// `Dcdd::polls_on_sector`.
    idle_status_reads: u64,
    /// What the front panel reports on port FFh.
    sense_switches: u8,
    /// Set when the guest read the console data register with nothing waiting,
    /// on a machine whose console *blocks* rather than being polled.
    ///
    /// See [`BootMachine::step`], which is the only correct way to run this
    /// machine's CPU.
    console_blocked: bool,
    /// The console board this machine carries.
    ///
    /// A field rather than the two constants it replaced, because "where is the
    /// console" is a property of the machine an operator chose and not of this
    /// module. Everything that used to match on the constants now compares
    /// against this — including [`BootMachine::reserved_port`], so a virtual-
    /// modem profile is refused for clashing with *this* machine's console
    /// rather than with an Altair's.
    console: super::console::ConsoleBoard,
    /// The virtual modem, if the operator selected a port profile that can
    /// exist here.  Shared with the CP/M emulator's machine, so the rings and
    /// the status bits behave identically in both.
    modem: ModemPort,
    /// Accesses to the disk controller's ports, ever.
    ///
    /// The driver's "is this guest doing anything?" signal for the one piece of
    /// work it cannot otherwise see. Console bytes and modem bytes pass through
    /// its own hands; a disk load does not, and pacing a guest that is busy
    /// reading a track would make every `DIR` crawl.
    disk_accesses: u64,
    /// Diagnostic: how many times each port was touched.
    #[cfg(test)]
    port_hits: std::collections::BTreeMap<u8, u64>,
}

impl BootMachine {
    pub fn new() -> BootMachine {
        BootMachine {
            mem: vec![0; 0x10000],
            // The default machine's boards.  Which boards a machine carries is
            // the machine's business now (see `set_machine`), because z80pack's
            // device claims ports the Altair boards and the 88-2SIO console use
            // — so "all of them, always" stopped being a coherent machine.
            controllers: super::console::resolve_machine(super::console::DEFAULT_MACHINE)
                .boards
                .iter()
                .map(|b| boards_to_controller(*b))
                .collect(),
            disk_controller: (0..16).map(|_| None).collect(),
            disks: (0..16).map(|_| None).collect(),
            tx: Vec::new(),
            rx: std::collections::VecDeque::new(),
            rx_hold: 0,
            idle_status_reads: 0,
            sense_switches: DEFAULT_SENSE_SWITCHES,
            console_blocked: false,
            console: super::console::resolve_console(super::console::DEFAULT_MACHINE),
            modem: ModemPort::new(),
            disk_accesses: 0,
            #[cfg(test)]
            port_hits: std::collections::BTreeMap::new(),
        }
    }

    /// The CPU a booted disk runs on.
    ///
    /// A **Z80**, and one place decides so the driver and the tests cannot
    /// disagree. The Altair shipped with an 8080 and every MITS disk here is
    /// 8080 code, so an 8080 core is the more literal machine — but the Z80 is
    /// a superset that runs all of it, Altairs were very commonly fitted with
    /// Z80 upgrade boards, and the CP/M emulator next door is already a Z80.
    ///
    /// The deciding case is our own: EGT80 is Z80 code and declares itself so.
    /// On an 8080 core it loads, executes a Z80-only opcode as something else,
    /// and takes CP/M down with it — the sign-on comes back corrupted on the
    /// warm boot. A machine that cannot run the terminal we ship with it is the
    /// wrong machine.
    pub fn new_cpu() -> Cpu {
        Cpu::new_z80()
    }

    /// Offer the configured virtual modem to this machine.
    ///
    /// A booted disk *can* have our modem, and on a real Altair it is obvious
    /// where: the 88-2SIO is a two-port board, the console is port A, and the
    /// modem goes on port B — which is exactly the `altair_2sio2` profile at
    /// 12h/13h. Software running under a booted Altair CP/M then finds a UART
    /// where it expects one and dials out through us.
    ///
    /// Two kinds of profile cannot come along, and saying so plainly beats a
    /// modem that silently is not there:
    ///
    /// * `AUX:` and HBIOS are not hardware. They are our own BDOS device and
    ///   RomWBW's firmware call, and a booted disk brings its own of both — so
    ///   there is nothing for us to answer.
    /// * A profile whose ports land on the disk controller, the console or the
    ///   front panel would fight this machine's own hardware. `altair_2sio1` is
    ///   the one that catches people out: it is 10h/11h, which *is* the
    ///   console, because on a real Altair the console is 2SIO port A.
    pub fn attach_modem(&mut self, access: ModemAccess) -> ModemAttach {
        match access {
            ModemAccess::Off => ModemAttach::Off,
            ModemAccess::Aux => ModemAttach::Unavailable(
                "the AUX: device belongs to our BDOS, and a booted disk brings its own".into(),
            ),
            ModemAccess::Hbios { .. } => ModemAttach::Unavailable(
                "HBIOS is RomWBW firmware, which a booted Altair disk does not have".into(),
            ),
            ModemAccess::Ports(u) => {
                if let Some((clash, what)) = [u.status_port, u.data_port]
                    .into_iter()
                    .find_map(|p| self.reserved_port(p).map(|w| (p, w)))
                {
                    let hint = match self.suggested_modem_profile() {
                        Some(k) => format!(" — try {k}"),
                        None => String::new(),
                    };
                    return ModemAttach::Unavailable(format!(
                        "port {clash:#04x} is {what} on this machine{hint}"
                    ));
                }
                self.modem.set_access(access);
                ModemAttach::Ports(u.status_port, u.data_port)
            }
        }
    }

    /// The virtual modem's rings, for the driver to pump between CPU batches.
    pub fn modem(&mut self) -> &mut ModemPort {
        &mut self.modem
    }

    /// Be the machine this config value names.
    ///
    /// Called before [`BootMachine::boot`], and before
    /// [`BootMachine::attach_modem`] — the modem's clash check asks what this
    /// machine's console ports are, so a machine set afterwards would have its
    /// modem vetted against the wrong console. An unknown value resolves to the
    /// default machine rather than to no console, so a typo in a hand-edited
    /// config file leaves the gateway working instead of mute.
    pub fn set_machine(&mut self, key: &str) {
        // Asserted, not merely documented. The ordering is a real correctness
        // requirement — `attach_modem` refuses a profile that lands on *this*
        // machine's console, and the port dispatch offers the console first — so
        // choosing the console after a modem is attached would leave the modem
        // silently shadowed, present in the config and mute in the machine. A
        // rule written only in a doc comment is the shape of defect this
        // codebase has produced three times; this one fails a test instead.
        debug_assert!(
            !self.modem.is_attached(),
            "set_machine must come before attach_modem: the modem was vetted \
             against a different console's ports"
        );
        // And before any disk goes in, for the same class of reason: `insert`
        // offers an image to each controller in turn and records which one took
        // it, so replacing the controllers afterwards would leave `disk_controller`
        // pointing at boards that no longer exist.
        debug_assert!(
            self.disks.iter().all(|d| d.is_none()),
            "set_machine must come before insert: the disks were matched against \
             a different machine's controllers"
        );
        let machine = super::console::resolve_machine(key);
        self.console = machine.console;
        self.controllers = machine.boards.iter().map(|b| boards_to_controller(*b)).collect();
    }

    /// The console this machine carries.
    pub fn console(&self) -> super::console::ConsoleBoard {
        self.console
    }

    /// Execute one instruction. **Use this rather than
    /// `cpu.execute_instruction(machine)`.**
    ///
    /// It exists for one thing: a console that *blocks*. The Altair boards are
    /// polled — the guest reads a status register until a key is ready and only
    /// then reads the data register — so nothing has to wait. z80pack's console
    /// is not: its CBIOS reads the data port unconditionally and relies on the
    /// port itself to stall the processor until a character arrives. Hand such a
    /// guest a zero and its CCP treats it as a keystroke, forever; TDISK03 signs
    /// on beautifully and then prints NULs without end.
    ///
    /// We cannot stall a CPU, so the guest waits the other way: if the
    /// instruction read an empty console, the program counter goes back to where
    /// it started and the read happens again next time. A blocked guest re-runs
    /// one `IN` until a byte is there, which is what the hardware does.
    ///
    /// **This is only sound because the instruction is a bare `IN`,** which has
    /// no effect but the read. A block-input instruction (`INI`, `INIR`) would
    /// have already moved `HL` and `B`, and replaying it would corrupt them. No
    /// CBIOS here uses one for the console, and if one ever does, this needs to
    /// hold the byte back rather than replay the instruction.
    pub fn step(&mut self, cpu: &mut Cpu) {
        let before = cpu.registers().pc();
        self.rx_hold = self.rx_hold.saturating_sub(1);
        cpu.execute_instruction(self);
        if self.console_blocked {
            self.console_blocked = false;
            cpu.registers().set_pc(before);
        }
    }

    /// Is a received character available to the guest *yet*?
    ///
    /// Not the same question as "is one queued". A real console arrives down a
    /// serial line one character at a time with a character time between them,
    /// and a queue handed over as fast as the guest can read it is a machine no
    /// software was written for.
    ///
    /// **This is a measured defect, not a refinement.** CDOS 2.58 reads the data
    /// register twice per character — a lookahead that on real hardware finds
    /// the line still empty and costs nothing. Given a queue that refills
    /// instantly it finds a character every time and throws it away, so `DIR`
    /// arrived as `DR` and a burst of `ABCDEFGH` came out as `ACEG`: exactly
    /// every other one, with the queue drained, which is what proves it is two
    /// reads and not a lost byte. A person typing never provokes it. **Pasting a
    /// command does**, which is why this is worth fixing rather than pacing the
    /// tests.
    fn rx_ready(&self) -> bool {
        !self.rx.is_empty() && self.rx_hold == 0
    }

    /// Does a disk controller answer at this port?
    ///
    /// Exposed so that the console choices can be *tested* against the real
    /// boards rather than against a written-down range of ports. A list of
    /// reserved ports in a second place is exactly what `reserved_port` stopped
    /// being, and for the same reason.
    #[cfg(test)]
    pub fn owns_disk_port(&self, port: u8) -> bool {
        self.controllers.iter().any(|c| c.owns_port(port))
    }

    /// Lay down the monitor ROM this machine's console prints through, if any.
    ///
    /// After the boot program is in memory, not before: a ROM is not something a
    /// loaded program may overwrite, and a bootstrap that reached this high would
    /// otherwise take the console with it. Nothing in the sample set does, but
    /// the ordering costs nothing and the failure it prevents would present as a
    /// disk that signs on and then goes silent.
    fn place_rom(&mut self) {
        let Some(image) = self.console.rom.image(self.console.data_port) else {
            return;
        };
        for (at, bytes) in image.chunks {
            let start = at as usize;
            let end = (start + bytes.len()).min(self.mem.len());
            self.mem[start..end].copy_from_slice(&bytes[..end - start]);
        }
    }


    /// Put an image in a drive.
    ///
    /// The whole image is held in memory. These are floppies — 308 KB, or 4.8 MB
    /// for a hard disk — and a booted guest seeks constantly, so paging every
    /// sector off the host would turn every `DIR` into a storm of small reads.
    /// Writes are collected and given back with [`BootMachine::take_dirty`].
    pub fn insert(&mut self, drive: u8, bytes: Vec<u8>, read_only: bool) -> Result<(), String> {
        // Offered to each controller in turn; the first that recognises the
        // size takes it.  That is how an image is matched to hardware — the
        // boards took different media, and before anything is running the file
        // length is all there is to go on.
        let len = bytes.len() as u64;
        let which = self.controllers.iter().position(|c| c.accepts(len).is_some());
        let taken = which.map(|i| self.controllers[i].insert(drive, len, read_only));
        match taken {
            Some(Ok(())) => {
                if let Some(slot) = self.disk_controller.get_mut(drive as usize) {
                    *slot = which;
                }
            }
            Some(Err(e)) => return Err(e),
            None => {
                // Name the hardware rather than just refusing: with more than
                // one controller "wrong size" is not enough to act on, and the
                // operator needs to know what this machine actually takes.
                let carries: Vec<&str> = self.controllers.iter().map(|c| c.name()).collect();
                return Err(format!(
                    "{len} bytes is not a disk this machine can carry — it has: {}",
                    carries.join(", ")
                ));
            }
        }
        if let Some(slot) = self.disks.get_mut(drive as usize) {
            *slot = Some(Mounted { bytes, read_only, dirty: false });
        }
        Ok(())
    }

    /// Images that the guest has written to, for the caller to persist.
    ///
    /// Handing them back rather than writing them ourselves keeps host file
    /// access on the caller's side, where the read-only rules and the mount
    /// bookkeeping already live.
    pub fn take_dirty(&mut self) -> Vec<(u8, Vec<u8>)> {
        let mut out = Vec::new();
        for (i, slot) in self.disks.iter_mut().enumerate() {
            if let Some(m) = slot {
                if m.dirty && !m.read_only {
                    m.dirty = false;
                    out.push((i as u8, m.bytes.clone()));
                }
            }
        }
        out
    }

    /// Cold-boot with a specific sector step, for diagnosing a disk whose
    /// loader is not laid out the way the bootstrap assumes.
    ///
    /// Every Altair disk we have — CP/M and MITS alike — uses the 2:1 step
    /// [`super::boot::BOOT_INTERLEAVE`], so nothing in the product needs this.
    /// It exists because trying another step is the first thing you would want
    /// to do with a disk that loads and then runs off into nothing.
    #[cfg(test)]
    fn boot_with_step(&mut self, cpu: &mut Cpu, drive: u8, step: u8) -> Result<(), BootError> {
        let BootMachine { disks, controllers, mem, disk_controller, .. } = self;
        let disks = &*disks;
        let which = disk_controller
            .get(drive as usize)
            .copied()
            .flatten()
            .ok_or(BootError::NoDisk(drive))?;
        let dcdd = controllers[which].as_dcdd().ok_or(BootError::NoBootstrap)?;
        let mut chunks: Vec<(u16, Vec<u8>)> = Vec::new();
        let entry = super::boot::cold_boot_with_step(
            dcdd,
            drive,
            step,
            |d, t, s| {
                let m = disks
                    .get(d as usize)
                    .and_then(|x| x.as_ref())
                    .ok_or_else(|| format!("drive {d} is empty"))?;
                // Recomputed from the image rather than stored beside it: where
                // a byte lives is the controller's arithmetic now, and the
                // bootstrap is the one place left that still needs it directly.
                let geometry = super::dcdd::geometry_for(m.bytes.len() as u64)
                    .ok_or_else(|| format!("drive {d} does not hold an 88-DCDD image"))?;
                let off = geometry.offset(t, s) as usize;
                m.bytes
                    .get(off..off + SECTOR_LEN)
                    .map(|b| b.to_vec())
                    .ok_or_else(|| format!("track {t} sector {s} is past the end of the image"))
            },
            |addr, bytes| chunks.push((addr, bytes.to_vec())),
        )?;
        for (addr, bytes) in chunks {
            let at = addr as usize;
            let end = (at + bytes.len()).min(mem.len());
            mem[at..end].copy_from_slice(&bytes[..end - at]);
        }
        cpu.registers().set_pc(entry);
        Ok(())
    }

    /// Cold-boot from a drive, leaving the CPU ready to run.
    ///
    /// Two steps, and the second one is easy to forget: the disk's own loader
    /// goes into memory, and then the machine's monitor ROM goes in on top of it,
    /// because a ROM is not memory the loader owns.
    pub fn boot(&mut self, cpu: &mut Cpu, drive: u8) -> Result<(), BootError> {
        self.load_boot_program(cpu, drive)?;
        self.place_rom();
        Ok(())
    }

    /// The cold start proper: whatever this drive's controller says its PROM
    /// would do, done.
    fn load_boot_program(&mut self, cpu: &mut Cpu, drive: u8) -> Result<(), BootError> {
        let BootMachine { disks, controllers, mem, disk_controller, .. } = self;
        let disks = &*disks;
        // The controller holding *this* drive, not just any controller: drive 0
        // of the floppy board and drive 0 of the hard disk are different
        // drives, and asking the wrong one produced a true but useless "that
        // drive is empty".
        let which = disk_controller
            .get(drive as usize)
            .copied()
            .flatten()
            .ok_or(BootError::NoDisk(drive))?;
        // Only the floppy has a bootstrap; see `Controller::as_dcdd`.
        // A board whose PROM simply reads one sector and jumps says so; the
        // floppy's does not, and keeps its own sequence below.
        let m = disks
            .get(drive as usize)
            .and_then(|x| x.as_ref())
            .ok_or(BootError::NoDisk(drive))?;
        match controllers[which].cold_start(&m.bytes) {
            ColdStart::Program { offset, len, load, entry } => {
                let at = offset as usize;
                let program = m.bytes.get(at..at + len).ok_or_else(|| {
                    BootError::Unreadable("the boot program runs past the end".into())
                })?;
                // The same test the floppy's bootstrap applies to its boot
                // sector, rather than a weaker one of this path's own: a disk
                // whose label names a program that turns out to be text is data
                // with a plausible label, and running text on a Z80 does
                // something that is never useful.
                if !super::boot::looks_bootable(&program[..super::boot::BOOT_DATA_LEN.min(len)]) {
                    return Err(BootError::NotBootable);
                }
                let start = load as usize;
                let end = (start + program.len()).min(mem.len());
                mem[start..end].copy_from_slice(&program[..end - start]);
                cpu.registers().set_pc(entry);
                // Leave the board as its own PROM would have — see
                // `Controller::cold_started`. A synthesised load skips the real
                // read, and the state that read leaves behind is state the loader
                // is entitled to find.
                controllers[which].cold_started(drive);
                return Ok(());
            }
            // The controller loads a program the disk names, and this disk names
            // none. That is a data disk, and saying "no controller can
            // cold-start this" instead — which is what an `Option` here used to
            // make it say — sends the reader after missing code of ours.
            ColdStart::NoProgram => return Err(BootError::NotBootable),
            ColdStart::Own => {}
        }
        let dcdd = controllers[which].as_dcdd().ok_or(BootError::NoBootstrap)?;
        // Every chunk with the address it belongs at.  The bootstrap stores
        // more than one, and keeping only the last — or ignoring the address —
        // silently loads a partial loader that runs off its own end.
        let mut chunks: Vec<(u16, Vec<u8>)> = Vec::new();
        let entry = cold_boot(
            dcdd,
            drive,
            |d, t, s| {
                let m = disks
                    .get(d as usize)
                    .and_then(|x| x.as_ref())
                    .ok_or_else(|| format!("drive {d} is empty"))?;
                // Recomputed from the image rather than stored beside it: where
                // a byte lives is the controller's arithmetic now, and the
                // bootstrap is the one place left that still needs it directly.
                let geometry = super::dcdd::geometry_for(m.bytes.len() as u64)
                    .ok_or_else(|| format!("drive {d} does not hold an 88-DCDD image"))?;
                let off = geometry.offset(t, s) as usize;
                m.bytes
                    .get(off..off + SECTOR_LEN)
                    .map(|b| b.to_vec())
                    .ok_or_else(|| format!("track {t} sector {s} is past the end of the image"))
            },
            |addr, bytes| chunks.push((addr, bytes.to_vec())),
        )?;
        for (addr, bytes) in chunks {
            let at = addr as usize;
            let end = (at + bytes.len()).min(mem.len());
            mem[at..end].copy_from_slice(&bytes[..end - at]);
        }
        cpu.registers().set_pc(entry);
        Ok(())
    }

    /// Give the guest a byte of console input.
    ///
    /// Bounded, and it has to be: a real console has no queue at all, and a
    /// guest that is not reading its own console — sitting in a compute loop, or
    /// simply wedged — would otherwise let a client stream bytes into this
    /// buffer for as long as it liked. Dropping the excess is what a real UART
    /// does when nobody reads the receive register, and it is the same bound the
    /// emulator puts on its own pending-input queue.
    pub fn send_key(&mut self, byte: u8) {
        if self.rx.len() < KEY_QUEUE_CAP {
            self.rx.push_back(byte);
        }
    }

    /// Take everything the guest has printed.
    pub fn take_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.tx)
    }

    /// Console status reads since anything last happened.
    ///
    /// A guest waiting forever on a key is normal; a guest waiting forever on a
    /// *disk* is a fault, and this tells the two apart with
    /// [`BootMachine::stuck_polls`] reporting the other side.  The driver paces
    /// on observed activity instead (see `cpm_boot_ui`), so this is the test's
    /// way of proving the two are not confused.
    #[cfg(test)]
    pub fn idle_status_reads(&self) -> u64 {
        self.idle_status_reads
    }

    /// Accesses to the disk controller's ports since the machine was made.
    pub fn disk_accesses(&self) -> u64 {
        self.disk_accesses
    }

    /// Position-register reads without the disk moving on.
    pub fn stuck_polls(&self) -> u32 {
        self.controllers.iter().map(|c| c.stuck_polls()).max().unwrap_or(0)
    }

    /// What medium an image of this size is, if any controller here takes it.
    ///
    /// The one place that answers "can this file be booted", so the boot
    /// picker, the survey and the generated readme cannot disagree about it —
    /// and so a new controller becomes bootable everywhere by existing, rather
    /// than by being added to three lists.
    pub fn medium_for(image_len: u64) -> Option<&'static str> {
        // Every board, not the default machine's — "can this be booted" must not
        // depend on which machine happens to be configured, or a z80pack disk
        // would be missing from the boot picker until the operator had already
        // switched machines to see it.
        all_controllers().iter().find_map(|c| c.accepts(image_len))
    }

    /// Every medium a booted machine here can carry, board by board.
    ///
    /// For the generated readme, so that a new controller becomes documented by
    /// existing rather than by somebody remembering to add it to a list. The
    /// readme built its own from the floppy's geometry table and therefore told
    /// operators that only 88-DCDD floppies boot for as long as the hard disk
    /// had been booting them.
    pub fn bootable_media() -> Vec<super::controller::Medium> {
        all_controllers().iter().flat_map(|c| c.media()).collect()
    }

    /// Can the machine `key` names carry an image this size at all?
    ///
    /// Asked by `detect::machine_for` so that an unclear detection does not land
    /// on a machine which would refuse the disk outright.
    pub fn machine_accepts(key: &str, image_len: u64) -> bool {
        super::console::resolve_machine(key)
            .boards
            .iter()
            .any(|b| boards_to_controller(*b).accepts(image_len).is_some())
    }

    /// Which controller answers at this port, if any.
    fn controller_for(&self, port: u8) -> Option<usize> {
        self.controllers.iter().position(|c| c.owns_port(port))
    }

    /// Serve whatever the controller asked for after a port access.
    fn service(&mut self, req: HostRequest, ctrl: usize) {
        match req {
            HostRequest::None => {}
            // Identical work; they differ only in whether the port read that
            // caused it still owes the guest an answer. See `HostRequest`.
            HostRequest::Read { drive, offset, len }
            | HostRequest::ReadAhead { drive, offset, len } => {
                let bytes = self
                    .disks
                    .get(drive as usize)
                    .and_then(|x| x.as_ref())
                    .and_then(|m| {
                        let off = offset as usize;
                        m.bytes.get(off..off + len).map(|b| b.to_vec())
                    });
                // A read past the end of the image gives the guest an erased
                // sector rather than a panic: a real drive returns *something*
                // from unformatted media, and the guest's own error handling
                // is better placed to react than we are.
                self.controllers[ctrl]
                    .buffer_loaded(drive, &bytes.unwrap_or_else(|| vec![0xE5; len]));
            }
            HostRequest::Write { drive, offset, len } => {
                let Some(buf) = self.controllers[ctrl].buffer(drive).map(|b| b.to_vec()) else {
                    return;
                };
                if let Some(m) = self.disks.get_mut(drive as usize).and_then(|x| x.as_mut()) {
                    if m.read_only {
                        return;
                    }
                    let off = offset as usize;
                    if let Some(dst) = m.bytes.get_mut(off..off + len) {
                        dst.copy_from_slice(&buf[..len]);
                        m.dirty = true;
                    }
                }
            }
            // A transfer straight between the image and guest memory. The
            // controller never sees the bytes, which is the whole point: it has
            // no data register for them to pass through.
            HostRequest::Dma { drive, offset, len, addr, to_memory } => {
                if to_memory {
                    let bytes = self
                        .disks
                        .get(drive as usize)
                        .and_then(|x| x.as_ref())
                        .and_then(|m| {
                            let off = offset as usize;
                            m.bytes.get(off..off + len).map(|b| b.to_vec())
                        })
                        // Past the end of the image gives the guest an erased
                        // sector, the same posture as the other read paths: a
                        // real drive returns *something* from unformatted media
                        // and the guest's own error handling is better placed to
                        // react than we are.
                        .unwrap_or_else(|| vec![0xE5; len]);
                    for (i, b) in bytes.iter().enumerate() {
                        // Wrapping, because the guest's address is sixteen bits
                        // and a transfer that runs off the top of memory wraps
                        // on real hardware rather than faulting.
                        let at = addr.wrapping_add(i as u16) as usize;
                        self.mem[at] = *b;
                    }
                } else {
                    let bytes: Vec<u8> = (0..len)
                        .map(|i| self.mem[addr.wrapping_add(i as u16) as usize])
                        .collect();
                    if let Some(m) = self.disks.get_mut(drive as usize).and_then(|x| x.as_mut()) {
                        if m.read_only {
                            return;
                        }
                        let off = offset as usize;
                        if let Some(dst) = m.bytes.get_mut(off..off + len) {
                            dst.copy_from_slice(&bytes);
                            m.dirty = true;
                        }
                    }
                }
            }
            HostRequest::Fill { drive, offset, chunk, stride, count, byte } => {
                let Some(m) = self.disks.get_mut(drive as usize).and_then(|x| x.as_mut()) else {
                    return;
                };
                if m.read_only {
                    return;
                }
                for i in 0..count {
                    let at = offset.saturating_add(stride.saturating_mul(i as u64)) as usize;
                    // A run past the end of the image is skipped rather than
                    // clamped or panicked on: the same posture as a read past
                    // the end, and a controller asking for one is a bug in the
                    // controller, not something to half-do.
                    if let Some(dst) = m.bytes.get_mut(at..at + chunk) {
                        dst.fill(byte);
                        m.dirty = true;
                    }
                }
            }
        }
    }

    /// What this machine's own hardware answers at `port`, if anything.
    ///
    /// Derived from the controllers rather than listed, because a list is a
    /// second place for the same rule and this one had already fallen behind:
    /// it named the floppy's three ports and knew nothing of the hard disk's
    /// eight, so a modem profile landing on A0–A7 would have been accepted here
    /// and then silently shadowed by the controller in the port dispatch — a
    /// modem that is simply mute, with nothing said about why.
    fn reserved_port(&self, port: u8) -> Option<&'static str> {
        if self.controllers.iter().any(|c| c.owns_port(port)) {
            return Some("the disk controller");
        }
        if port == self.console.status_port || port == self.console.data_port {
            return Some("the console");
        }
        match port {
            SENSE_SWITCH_PORT => Some("the front panel"),
            _ => None,
        }
    }

    /// A virtual-modem profile that could exist on this machine, for the hint in
    /// [`BootMachine::attach_modem`]'s refusal.
    ///
    /// Computed rather than written down, because the console moves now: naming
    /// `altair_2sio2` unconditionally was right while the console was always at
    /// `10h`/`11h`, and on a machine whose console is at `04h`/`05h` the honest
    /// suggestion is `altair_2sio1` — the port that just became free. A refusal
    /// that recommends something this machine would also refuse is worse than no
    /// suggestion at all.
    ///
    /// **A profile of the console's own family is preferred**, and that is not
    /// cosmetic tidiness. On an Altair the modem belongs on the *second port of
    /// the same 88-2SIO board* — one card, two channels, which is how these
    /// machines were really fitted — so `altair_2sio2` is the answer a person
    /// wants, not merely a free pair of ports. Taking the first profile that
    /// happens to fit suggested an RC2014 SIO/2 board to an Altair owner, which
    /// would work and is still the wrong advice.
    fn suggested_modem_profile(&self) -> Option<&'static str> {
        let fits = |c: &&super::uart::UartChoice| {
            let ModemAccess::Ports(u) = c.access else { return false };
            self.reserved_port(u.status_port).is_none()
                && self.reserved_port(u.data_port).is_none()
        };
        let same_family = |c: &&super::uart::UartChoice| {
            matches!(c.access, ModemAccess::Ports(u) if u.family == self.console.family)
        };
        super::uart::UART_CHOICES
            .iter()
            .find(|c| same_family(c) && fits(c))
            .or_else(|| super::uart::UART_CHOICES.iter().find(fits))
            .map(|c| c.key)
    }
}


impl Machine for BootMachine {
    fn peek(&mut self, address: u16) -> u8 {
        self.mem[address as usize]
    }

    fn poke(&mut self, address: u16, value: u8) {
        self.mem[address as usize] = value;
    }

    fn port_in(&mut self, address: u16) -> u8 {
        let port = address as u8;
        #[cfg(test)]
        {
            *self.port_hits.entry(port).or_insert(0) += 1;
        }
        match port {
            p if self.controller_for(p).is_some() => {
                let ctrl = self.controller_for(port).expect("just matched");
                self.disk_accesses = self.disk_accesses.saturating_add(1);
                let (v, req) = self.controllers[ctrl].port_in(port);
                let was_fill = matches!(req, HostRequest::Read { .. });
                self.service(req, ctrl);
                self.idle_status_reads = 0;
                if was_fill {
                    // The controller asked for the sector *because* the guest
                    // wanted its first byte, so it had none to give and
                    // answered 0xFF.  Now that the sector is loaded, ask again
                    // and hand over the real byte.  Returning the placeholder
                    // would eat the first byte of every sector the guest reads
                    // — which boots far enough to look like it is working and
                    // then produces silence.
                    let (v2, req2) = self.controllers[ctrl].port_in(port);
                    self.service(req2, ctrl);
                    return v2;
                }
                v
            }
            // The console this machine carries.  Its family decides the status
            // bits, and the polarity is not cosmetic: two of the boards here
            // report a waiting key with a bit *clear*, and reading that
            // backwards makes a guest claim a keypress on every poll and
            // consume garbage — which looks like a corrupt disk, not a
            // mis-set console.
            p if p == self.console.status_port => {
                self.idle_status_reads = self.idle_status_reads.saturating_add(1);
                // Transmit is always ready — our "wire" is a buffer, so there is
                // nothing to be busy about.
                self.console.family.status(self.rx_ready(), true, true)
            }
            p if p == self.console.data_port => {
                // `rx_ready`, not "is the queue non-empty" — see its comment.
                // A character still inside its character time has not arrived
                // yet, and handing it over early is how a guest's lookahead
                // swallows every other keystroke.
                match self.rx_ready().then(|| self.rx.pop_front()).flatten() {
                    Some(b) => {
                        self.idle_status_reads = 0;
                        self.rx_hold = RX_CHARACTER_TIME;
                        b
                    }
                    None => {
                        // Nothing waiting. On a polled console the guest checked
                        // status first and would not be here, so zero is a fine
                        // answer. On a *blocking* one it never checks — z80pack's
                        // CBIOS is `CONIN: IN A,(1) / RET` — and handing back zero
                        // feeds the CCP an endless stream of NULs. Say so instead,
                        // and let `step` make the guest wait.
                        //
                        // Counted as an idle console read, because that is what it
                        // is: a guest with a blocking console never touches the
                        // status register, so without this `idle_status_reads`
                        // would report such a guest as busy for ever.
                        //
                        // This is *not* what paces a live session — the driver
                        // paces on printed output, keystrokes, modem bytes and
                        // disk accesses, none of which a blocked guest produces,
                        // so it naps correctly either way (asserted in
                        // `test_a_blocked_guest_looks_idle_to_the_driver`). This
                        // keeps the diagnostic honest, nothing more.
                        if self.console.blocking {
                            self.console_blocked = true;
                            self.idle_status_reads = self.idle_status_reads.saturating_add(1);
                        } else {
                            self.idle_status_reads = 0;
                        }
                        0
                    }
                }
            }
            // The front panel. Input only on real hardware, and the reason
            // every MITS operating system can find its console.
            SENSE_SWITCH_PORT => self.sense_switches,
            // The virtual modem, if the operator gave this machine one. Asked
            // after the fixed hardware above, though `attach_modem` has already
            // refused any profile that could overlap it.
            //
            // Deliberately no idle bookkeeping here. A comms program polling
            // its UART is not console activity, and whether it is *idle* is a
            // question only the driver can answer — it is the one that knows
            // whether the pump actually moved a byte this batch.
            other => self.modem.port_in(other).unwrap_or(0xFF),
        }
    }

    fn port_out(&mut self, address: u16, value: u8) {
        let port = address as u8;
        #[cfg(test)]
        {
            *self.port_hits.entry(port | 0x80).or_insert(0) += 1;
        }
        match port {
            p if self.controller_for(p).is_some() => {
                let ctrl = self.controller_for(port).expect("just matched");
                self.disk_accesses = self.disk_accesses.saturating_add(1);
                let req = self.controllers[ctrl].port_out(port, value);
                self.service(req, ctrl);
                self.idle_status_reads = 0;
            }
            // The console's data register, written.  On the boards whose driver
            // only ever *reads* these two ports — the `04h`/`05h` machines print
            // through a monitor ROM instead — this is where the ROM stub's own
            // `OUT` lands, which is precisely how a synthesised CUTER reaches a
            // real terminal without anything in this dispatch knowing it exists.
            p if p == self.console.data_port => {
                self.tx.push(value & 0x7F);
                self.idle_status_reads = 0;
            }
            // The control register and anything else: offered to the modem,
            // then accepted and discarded.
            other => {
                self.modem.port_out(other, value);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cpm::dcdd::{geometry_for, Geometry};

    /// The Altair 88-2SIO console's ports, spelled out rather than imported.
    ///
    /// Deliberately literal. These used to be constants the machine itself read,
    /// and now the machine gets its console from
    /// [`crate::cpm::console::MACHINE_CHOICES`] — so if these were an import, the
    /// assertions below would compare the default machine against itself and pass
    /// no matter what it changed to. Written out, they pin the one thing that must
    /// never move: every disk that boots today boots because its console is here.
    const CONSOLE_STATUS_PORT: u8 = 0x10;
    const CONSOLE_DATA_PORT: u8 = 0x11;

    /// The EGT80 binary, for the gates that put it on a disk and then check what
    /// came back. Module-level so the helper that writes it and the assertions
    /// that compare against it cannot end up looking at different bytes.
    const EGT80_COM: &[u8] = include_bytes!("../../EGT80/EGT80.COM");

    fn image(geom: Geometry) -> Vec<u8> {
        vec![0u8; geom.image_len() as usize]
    }

    /// Code-like filler for a synthetic boot sector's unused tail.
    ///
    /// The opening bytes of a real Altair boot sector, repeated. Never
    /// executed — see [`bootable_image`].
    const BOOT_FILLER: [u8; 7] = [0x31, 0x00, 0xDF, 0xF3, 0xAF, 0xD3, 0x08];

    /// An image whose boot sector has the byte distribution of a real one.
    ///
    /// A hand-built fixture is a few opcodes and then zeros, which is over 90%
    /// one byte — the exact shape of a *data* disk's header-and-padding, and
    /// refused as such by [`super::super::boot::looks_bootable`]. That is
    /// correct rather than unfortunate: the sparsest boot sector measured
    /// across the Altair and z80pack collections is 63%, while the disks with
    /// no boot program on them are 89% and up. Four tests here were built on
    /// the unrealistic shape and all failed together when the check learned to
    /// tell the two apart.
    ///
    /// Callers overwrite the front with whatever code they are testing; the
    /// filler only occupies the tail.
    fn bootable_image(geom: Geometry) -> Vec<u8> {
        use super::super::boot::{BOOT_DATA_LEN, BOOT_DATA_OFFSET};
        let mut img = image(geom);
        for (i, b) in img[BOOT_DATA_OFFSET..BOOT_DATA_OFFSET + BOOT_DATA_LEN]
            .iter_mut()
            .enumerate()
        {
            *b = BOOT_FILLER[i % BOOT_FILLER.len()];
        }
        img
    }

    #[test]
    fn test_geometry_is_recognised_by_size() {
        assert_eq!(geometry_for(337_568), Some(Geometry::EIGHT_INCH));
        assert_eq!(geometry_for(35 * 16 * 137), Some(Geometry::MINIDISK));
        assert_eq!(geometry_for(256_256), None, "not an 88-DCDD image");
        assert_eq!(geometry_for(0), None);
    }

    /// Real images in circulation carry a few bytes past the last sector, and
    /// refusing them on an exact size match locked out both CP/M 3 disks and
    /// every minidisk.
    #[test]
    fn test_a_short_trailer_does_not_hide_the_geometry() {
        assert_eq!(geometry_for(337_664), Some(Geometry::EIGHT_INCH), "96-byte trailer");
        assert_eq!(geometry_for(76_800), Some(Geometry::MINIDISK), "80 bytes of 1A");
        assert_eq!(
            geometry_for(337_568 + SECTOR_LEN as u64),
            None,
            "a whole extra sector is a different disk, not a trailer"
        );
        assert_eq!(geometry_for(337_567), None, "and short is never rounded up");
    }

    /// The point of the refactor, asserted rather than asserted-in-a-comment:
    /// a second controller is a `Controller` pushed into the list, and the
    /// machine's port dispatch and media matching pick it up with no change of
    /// their own.
    ///
    /// A stand-in board rather than the real 88-HDSK, because the question here
    /// is whether the *seam* works — that ports outside the floppy's range
    /// reach a second controller, that an image size the floppy refuses is
    /// offered to it, and that its byte-range requests are served from the
    /// right image. Wiring the real thing is the next change.
    struct FakeBoard {
        /// Shared with the test so it can see what the board was told, without
        /// having to reach back through the trait object.
        last_write: std::sync::Arc<std::sync::Mutex<Option<(u8, u8)>>>,
        buf: Vec<u8>,
        inserted: Option<u8>,
    }

    /// Sized so nothing the floppy controller takes can be confused with it.
    const FAKE_IMAGE_LEN: u64 = 4096;

    impl Controller for FakeBoard {
        fn name(&self) -> &'static str {
            "test board"
        }
        fn owns_port(&self, port: u8) -> bool {
            // Ports no real board here claims: 0xA0-0xA7 belong to the 88-HDSK
            // now, and a collision would prove nothing except that the first
            // controller to claim a port wins.
            (0xB0..=0xB7).contains(&port)
        }
        fn port_in(&mut self, _port: u8) -> (u8, HostRequest) {
            // Ask for the second 128 bytes of the image, to prove the offset
            // travels rather than being assumed to be zero.
            (0x5A, HostRequest::Read { drive: 1, offset: 128, len: 128 })
        }
        fn port_out(&mut self, port: u8, value: u8) -> HostRequest {
            *self.last_write.lock().unwrap() = Some((port, value));
            HostRequest::None
        }
        fn media(&self) -> Vec<super::super::controller::Medium> {
            vec![super::super::controller::Medium {
                bytes: FAKE_IMAGE_LEN,
                label: "test medium",
                trailer: 0,
                shape: "one sector".into(),
            }]
        }
        fn insert(&mut self, drive: u8, image_len: u64, _ro: bool) -> Result<(), String> {
            if image_len != FAKE_IMAGE_LEN {
                return Err("not mine".into());
            }
            self.inserted = Some(drive);
            Ok(())
        }
        fn buffer_loaded(&mut self, _drive: u8, bytes: &[u8]) {
            self.buf = bytes.to_vec();
        }
        fn buffer(&self, _drive: u8) -> Option<&[u8]> {
            Some(&self.buf)
        }
        fn stuck_polls(&self) -> u32 {
            0
        }
    }

    /// The monitor ROM must be in memory once the disk has booted, and it must
    /// be *code* that a guest can call.
    ///
    /// Placed after the loader, not before: a ROM is not memory a boot program
    /// owns. Nothing in the sample set loads that high, but the failure it
    /// prevents would present as a disk that signs on and then goes silent,
    /// which is the hardest kind of fault to attribute.
    #[test]
    fn test_a_rom_console_machine_has_its_rom_in_memory_after_boot() {
        let mut img = bootable_image(Geometry::EIGHT_INCH);
        img[3..3 + 8].copy_from_slice(&[0x31, 0x00, 0xDF, 0xF3, 0xAF, 0xD3, 0x08, 0xDB]);
        let mut m = BootMachine::new();
        m.set_machine("console_04_cuter");
        m.insert(0, img, true).unwrap();
        let mut cpu = BootMachine::new_cpu();
        m.boot(&mut cpu, 0).expect("boots");

        // PUSH AF / MOV A,B / OUT (05h),A / POP AF / RET at CUTER's OUTADDR.
        let rom: Vec<u8> = (0..6).map(|i| m.peek(crate::cpm::console::CUTER_CHAR_OUT + i)).collect();
        assert_eq!(rom, vec![0xF5, 0x78, 0xD3, 0x05, 0xF1, 0xC9], "the CUTER stub is not in memory");

        // And it really runs: call it with a character in B, as the guest does,
        // and the byte must come out of the console with every register intact.
        cpu.registers().set8(iz80::Reg8::B, b'Q');
        cpu.registers().set_a(0x5A);
        cpu.registers().set_pc(crate::cpm::console::CUTER_CHAR_OUT);
        for _ in 0..5 {
            m.step(&mut cpu);
        }
        assert_eq!(m.take_output(), b"Q".to_vec(), "the stub did not print through the console");
        assert_eq!(cpu.registers().a(), 0x5A, "the stub clobbered A, which its caller forbids");
    }

    /// A machine with a port console must place nothing at all. Six stray bytes
    /// at `C019` would be invisible on a 48K guest and land in the middle of a
    /// 64K one's memory.
    #[test]
    fn test_a_port_console_machine_places_no_rom() {
        let mut img = bootable_image(Geometry::EIGHT_INCH);
        img[3..3 + 8].copy_from_slice(&[0x31, 0x00, 0xDF, 0xF3, 0xAF, 0xD3, 0x08, 0xDB]);
        let mut m = BootMachine::new();
        m.insert(0, img, true).unwrap();
        let mut cpu = BootMachine::new_cpu();
        m.boot(&mut cpu, 0).expect("boots");
        for i in 0..6 {
            assert_eq!(
                m.peek(crate::cpm::console::CUTER_CHAR_OUT + i),
                0,
                "an Altair machine must leave C019 alone"
            );
        }
    }

    /// A fresh machine is the Altair it always was, and the setting moves it.
    ///
    /// The first half matters more than the second: every disk that boots today
    /// boots because its console is at `10h`/`11h`, so a default that drifted
    /// would silence a working gateway on upgrade with no error anywhere.
    #[test]
    fn test_the_console_defaults_to_the_altair_and_the_setting_moves_it() {
        let mut m = BootMachine::new();
        assert_eq!(m.console().status_port, CONSOLE_STATUS_PORT);
        assert_eq!(m.console().data_port, CONSOLE_DATA_PORT);

        m.set_machine("console_04");
        assert_eq!(m.console().status_port, 0x04);
        assert_eq!(m.console().data_port, 0x05);
        // And the dispatch follows it, both ways round: the new ports work...
        m.send_key(b'Z');
        assert_ne!(m.port_in(0x04) & 0x01, 0x01, "a key is waiting (active low)");
        assert_eq!(m.port_in(0x05), b'Z');
        m.port_out(0x05, b'!');
        assert_eq!(m.take_output(), b"!".to_vec());
        // ...and the Altair's ports are no longer the console. 0x11 now falls
        // through to the modem/idle-bus path, so nothing is echoed to the user.
        m.port_out(0x11, b'X');
        assert!(m.take_output().is_empty(), "0x11 is not this machine's console");
    }

    /// An unrecognised setting must leave a working console rather than none.
    /// The likeliest way here is a typo in a hand-edited config file, and a
    /// gateway that boots to permanent silence is a bad answer to a typo.
    #[test]
    fn test_an_unknown_machine_still_has_a_console() {
        let mut m = BootMachine::new();
        m.set_machine("no-such-machine");
        assert_eq!(m.console().status_port, CONSOLE_STATUS_PORT);
        m.port_out(CONSOLE_DATA_PORT as u16, b'k');
        assert_eq!(m.take_output(), b"k".to_vec());
    }

    /// The modem's clash check must ask *this* machine's console, not an
    /// Altair's — and its hint must name a profile this machine would accept.
    ///
    /// Both halves were bugs waiting to happen when the console became a
    /// setting: `altair_2sio1` at `10h`/`11h` is the console on a default
    /// machine and free on a `04h`/`05h` one, so a fixed answer is wrong for
    /// one of them, and a refusal that suggests something also refused is worse
    /// than saying nothing.
    #[test]
    fn test_the_modem_clash_check_follows_the_machine() {
        // Default machine: 0x10/0x11 IS the console, so that profile is refused.
        let mut m = BootMachine::new();
        let refused = m.attach_modem(crate::cpm::resolve_access("altair_2sio1"));
        let ModemAttach::Unavailable(why) = &refused else {
            panic!("altair_2sio1 must clash with an Altair console, got {refused:?}");
        };
        assert!(why.contains("the console"), "{why}");
        assert!(!why.contains("altair_2sio1"), "must not suggest the profile it just refused");

        // Same profile, machine with its console at 0x04/0x05: now it fits.
        let mut m = BootMachine::new();
        m.set_machine("console_04");
        assert_eq!(
            m.attach_modem(crate::cpm::resolve_access("altair_2sio1")),
            ModemAttach::Ports(0x10, 0x11),
            "0x10/0x11 is free once the console moves off it"
        );

        // A modem attached elsewhere must not shadow the console. The port
        // dispatch offers the console first, but a machine whose console moved
        // is exactly where that ordering could go wrong unnoticed.
        let mut m = BootMachine::new();
        m.set_machine("console_04");
        assert!(matches!(
            m.attach_modem(crate::cpm::resolve_access("rc2014_1b")),
            ModemAttach::Ports(0x82, 0x83)
        ));
        m.send_key(b'y');
        assert_eq!(m.port_in(0x05), b'y', "the console still answers with a modem fitted");
    }

    /// Choosing the machine after attaching a modem is a programming error, and
    /// it fails rather than producing a mute modem.
    ///
    /// The failure it prevents is invisible: the config says a modem is on
    /// `0x10/0x11`, `attach_modem` accepted it against the old console, and then
    /// the console moves onto those ports and wins in the dispatch. Guarded by
    /// `debug_assert!`, so this test is what makes the guard real.
    ///
    /// **Debug builds only, and deliberately.** A `debug_assert!` is compiled out
    /// in release, so a `#[should_panic]` test for one *fails* under
    /// `cargo test --release` — which is exactly how this was found, since CI runs
    /// the debug suite and never saw it. The alternative, promoting the guard to a
    /// real `assert!`, would put a panic path into a live session to catch a
    /// programming error that cannot survive one debug test run.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "set_machine must come before attach_modem")]
    fn test_choosing_the_machine_after_the_modem_is_refused() {
        let mut m = BootMachine::new();
        m.set_machine("console_04");
        assert!(matches!(
            m.attach_modem(crate::cpm::resolve_access("altair_2sio1")),
            ModemAttach::Ports(0x10, 0x11)
        ));
        // 0x10/0x11 is the console on this machine, so the modem accepted above
        // would now be shadowed. Must not be allowed to happen quietly.
        m.set_machine("altair_2sio");
    }

    /// The suggestion in a refusal must be a profile this machine really would
    /// accept. Computed, not written down — the console moves now.
    #[test]
    fn test_the_modem_hint_names_a_profile_that_would_work() {
        for key in ["altair_2sio", "altair_sio", "console_04", "console_04_cuter"] {
            let mut m = BootMachine::new();
            m.set_machine(key);
            let Some(hint) = m.suggested_modem_profile() else {
                panic!("{key}: no profile at all fits this machine");
            };
            // The hint must survive the very check that produced it.
            let mut m2 = BootMachine::new();
            m2.set_machine(key);
            assert!(
                matches!(m2.attach_modem(crate::cpm::resolve_access(hint)), ModemAttach::Ports(..)),
                "{key}: suggested {hint}, which this machine then refuses"
            );
        }
    }

    #[test]
    fn test_a_second_controller_needs_no_change_to_the_machine() {
        let mut m = BootMachine::new();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        m.controllers.push(Box::new(FakeBoard {
            last_write: seen.clone(),
            buf: Vec::new(),
            inserted: None,
        }));

        // An image the floppy controller refuses is offered on to the next one.
        let mut img = vec![0u8; FAKE_IMAGE_LEN as usize];
        img[128..256].fill(0xC3);
        m.insert(1, img, true).expect("the second controller takes it");

        // Its ports reach it...
        m.port_out(0xB3, 0x80);
        assert_eq!(*seen.lock().unwrap(), Some((0xB3, 0x80)));
        let v = m.port_in(0xB5);
        assert_eq!(v, 0x5A, "the second controller answered its own port");
        // ...and the floppy's ports still reach the floppy, untouched.
        m.port_in(0x08);

        // The byte-range request was served from the right image at the right
        // offset — the arithmetic the controller did, not the machine.
        let fake = m.controllers.last().unwrap().buffer(1).expect("buffered").to_vec();
        assert_eq!(fake.len(), 128);
        assert!(fake.iter().all(|&b| b == 0xC3), "wrong bytes or wrong offset");
    }

    #[test]
    fn test_inserting_a_wrong_sized_image_is_refused() {
        let mut m = BootMachine::new();
        let err = m.insert(0, vec![0; 1234], true).unwrap_err();
        assert!(err.contains("not a disk this machine can carry"), "{err}");
        // And it names the hardware, which is the part an operator can act on
        // once there is more than one controller to be wrong about.
        assert!(err.contains("88-DCDD"), "{err}");
    }

    /// Console output must reach the caller, and input must reach the guest.
    #[test]
    fn test_console_round_trip() {
        let mut m = BootMachine::new();
        // Nothing waiting: receive-full clear, transmit ready set.
        let s = m.port_in(CONSOLE_STATUS_PORT as u16);
        assert_eq!(s & 0x01, 0, "no input yet");
        assert_ne!(s & 0x02, 0, "always ready to print");

        m.send_key(b'A');
        assert_ne!(m.port_in(CONSOLE_STATUS_PORT as u16) & 0x01, 0, "a key is waiting");
        assert_eq!(m.port_in(CONSOLE_DATA_PORT as u16), b'A');
        assert_eq!(m.port_in(CONSOLE_STATUS_PORT as u16) & 0x01, 0, "consumed");

        m.port_out(CONSOLE_DATA_PORT as u16, b'H');
        m.port_out(CONSOLE_DATA_PORT as u16, b'i');
        assert_eq!(m.take_output(), b"Hi");
        assert!(m.take_output().is_empty(), "output is taken, not copied");
    }

    /// A guest probing for hardware we do not have must see an idle bus, not an
    /// echo of whatever it last wrote.
    #[test]
    fn test_unknown_ports_read_as_an_idle_bus() {
        let mut m = BootMachine::new();
        m.port_out(0x40, 0x55);
        assert_eq!(m.port_in(0x40), 0xFF);
    }

    /// The operator's modem must reach a booted guest at the ports they chose.
    ///
    /// This is the whole point of `attach_modem`: a booted Altair CP/M running
    /// comms software has to find a UART where such software looks for one.
    #[test]
    fn test_the_configured_modem_ports_reach_a_booted_guest() {
        use crate::cpm::resolve_access;

        let mut m = BootMachine::new();
        assert_eq!(
            m.attach_modem(resolve_access("altair_2sio2")),
            ModemAttach::Ports(0x12, 0x13),
            "the second 2SIO port is where an Altair's modem lives"
        );
        // What the peer sent reaches the guest...
        m.modem().queue_rx(b"OK");
        assert_eq!(m.port_in(0x13), b'O');
        assert_eq!(m.port_in(0x13), b'K');
        // ...and what the guest writes comes back for the driver to forward.
        m.port_out(0x13, b'A');
        m.port_out(0x13, b'T');
        assert_eq!(m.modem().drain_tx(), b"AT");
    }

    /// The profile that catches people out: on a real Altair the console *is*
    /// 2SIO port A, so pointing the modem at it would have the two fighting
    /// over `0x10`/`0x11`.
    #[test]
    fn test_a_modem_profile_that_lands_on_our_hardware_is_refused() {
        use crate::cpm::resolve_access;

        let mut m = BootMachine::new();
        match m.attach_modem(resolve_access("altair_2sio1")) {
            ModemAttach::Unavailable(why) => {
                assert!(why.contains("console"), "must name the clash: {why}");
                assert!(why.contains("altair_2sio2"), "and what to use instead: {why}");
            }
            other => panic!("0x10/0x11 is the console, got {other:?}"),
        }
        // And the console still works, rather than answering as a UART.
        assert_ne!(m.port_in(CONSOLE_STATUS_PORT as u16), 0);
    }

    /// `AUX:` and HBIOS are our BDOS device and RomWBW's firmware.  A booted
    /// disk brings its own of both, so there is nothing for us to answer — and
    /// saying why beats a modem that is silently absent.
    #[test]
    fn test_the_non_hardware_modem_modes_explain_themselves() {
        use crate::cpm::resolve_access;

        for key in ["aux", "hbios_1", "hbios_2"] {
            let mut m = BootMachine::new();
            match m.attach_modem(resolve_access(key)) {
                ModemAttach::Unavailable(why) => assert!(!why.is_empty(), "{key} must say why"),
                other => panic!("{key} cannot exist in a booted machine, got {other:?}"),
            }
        }
        let mut m = BootMachine::new();
        assert_eq!(m.attach_modem(resolve_access("off")), ModemAttach::Off);
    }

    /// A client streaming at a guest that never reads its console must not be
    /// able to grow the host's memory.  A real UART simply drops what nobody
    /// collects, and so do we.
    #[test]
    fn test_console_input_is_bounded() {
        let mut m = BootMachine::new();
        for i in 0..(KEY_QUEUE_CAP * 2) {
            m.send_key((i & 0xFF) as u8);
        }
        assert_eq!(m.rx.len(), KEY_QUEUE_CAP, "the queue stops growing");
        // And the bytes kept are the *earliest*, so a guest that starts reading
        // sees the beginning of what was typed rather than an arbitrary window.
        // A character time between the two reads because the console models a
        // serial line, not a byte queue — reading straight back would find the
        // line still empty, which is the whole point of `rx_ready`.
        assert_eq!(m.port_in(CONSOLE_DATA_PORT as u16), 0);
        pass_a_character_time(&mut m);
        assert_eq!(m.port_in(CONSOLE_DATA_PORT as u16), 1);
    }

    /// With no modem attached, its ports must read as an idle bus like any
    /// other hardware we do not have — not as zero, and not as an echo.
    #[test]
    fn test_modem_ports_are_inert_until_one_is_attached() {
        let mut m = BootMachine::new();
        m.port_out(0x13, 0x55);
        assert_eq!(m.port_in(0x12), 0xFF);
        assert_eq!(m.port_in(0x13), 0xFF);
    }

    /// The front panel must answer, and must not answer with the idle bus.
    ///
    /// Regression, and an expensive one: port FFh fell through to the
    /// unknown-port `0xFF`, which is a valid sense-switch reading meaning
    /// "console on the 88-SIO". Altair DOS, Disk BASIC and Time Sharing BASIC
    /// all booted correctly and then printed to ports 00h/01h, which we do not
    /// emulate — so a fully working guest looked like a disk that would not
    /// boot. The switches must select the 88-2SIO console we actually provide.
    #[test]
    fn test_the_sense_switches_select_the_console_we_emulate() {
        let mut m = BootMachine::new();
        let sw = m.port_in(SENSE_SWITCH_PORT as u16);
        assert_eq!(sw, DEFAULT_SENSE_SWITCHES);
        assert_ne!(
            sw, 0xFF,
            "a floating FFh sends MITS software to a console we do not have"
        );
    }

    /// Waiting on the console is normal and must not look like a stuck disk.
    #[test]
    fn test_idle_console_polling_is_counted_separately_from_disk_polling() {
        let mut m = BootMachine::new();
        for _ in 0..10 {
            m.port_in(CONSOLE_STATUS_PORT as u16);
        }
        assert!(m.idle_status_reads() >= 10, "console waiting is counted");
        assert_eq!(m.stuck_polls(), 0, "and is not confused with the disk");
        m.send_key(b'x');
        m.port_in(CONSOLE_DATA_PORT as u16);
        assert_eq!(m.idle_status_reads(), 0, "reset once something happens");
    }

    /// A sector written by the guest must come back for the caller to persist,
    /// and only once.
    #[test]
    fn test_writes_are_collected_for_the_caller() {
        let mut m = BootMachine::new();
        m.insert(0, image(Geometry::EIGHT_INCH), false).unwrap();
        m.port_out(0x08, 0);
        m.port_out(0x09, 0x04); // head load
        m.port_out(0x09, 0x80); // write enable
        for i in 0..SECTOR_LEN {
            m.port_out(0x0A, i as u8);
        }
        for _ in 0..4 {
            m.port_in(0x09); // rotate until the sector passes
        }
        let dirty = m.take_dirty();
        assert_eq!(dirty.len(), 1, "the written image comes back");
        assert_eq!(dirty[0].0, 0);
        assert_eq!(dirty[0].1[5], 5, "the bytes landed in the image");
        assert!(m.take_dirty().is_empty(), "taken once, not repeatedly");
    }

    /// A read-only image must never come back as dirty, however hard the guest
    /// tries.
    #[test]
    fn test_a_read_only_image_is_never_written() {
        let mut m = BootMachine::new();
        m.insert(0, image(Geometry::EIGHT_INCH), true).unwrap();
        m.port_out(0x08, 0);
        m.port_out(0x09, 0x04);
        m.port_out(0x09, 0x80);
        for i in 0..SECTOR_LEN {
            m.port_out(0x0A, i as u8);
        }
        for _ in 0..4 {
            m.port_in(0x09);
        }
        assert!(m.take_dirty().is_empty(), "a protected image stays clean");
    }

    /// What a controller *claims* it can carry and what it will actually take
    /// must be the same set.
    ///
    /// `accepts` is now derived from `media`, but `insert` is still each board's
    /// own code — the floppy's has to look up a geometry, so it cannot simply be
    /// the same call. That leaves two expressions of one rule, which is the
    /// arrangement that produced the exact-length bug in the first place. This
    /// pins them together at every boundary rather than trusting them to agree:
    /// the disagreement that matters is `accepts` saying yes to a disk `insert`
    /// then refuses, because the machine picks a controller with the first and
    /// then reports the second's error as though no board wanted the disk.
    #[test]
    fn test_what_each_board_accepts_is_what_it_will_insert() {
        // Built from a factory list so that adding a board means adding one line
        // here, not remembering to extend two places — `insert` mutates, so each
        // size needs a fresh board and the list has to be constructible twice.
        type Factory = fn() -> Box<dyn Controller>;
        let factories: &[Factory] = &[
            || Box::new(Dcdd::new()),
            || Box::new(crate::cpm::hdsk::Hdsk::new()),
            || Box::new(crate::cpm::tarbell::Tarbell::new()),
        ];
        assert_eq!(
            factories.len(),
            BootMachine::new().controllers.len(),
            "every controller the machine carries must be covered here"
        );
        for make in factories {
            let board = make();
            let mut sizes = vec![0u64, 1, 137, 256, 256_256, 1_000_000];
            for m in board.media() {
                sizes.extend([
                    m.bytes - 1,
                    m.bytes,
                    m.bytes + 1,
                    m.bytes + m.trailer,
                    m.bytes + m.trailer + 1,
                ]);
            }
            for size in sizes {
                let claimed = board.accepts(size).is_some();
                // A fresh board each time: `insert` mutates, and a drive that
                // already holds a disk is a different question.
                let taken = make().insert(0, size, true).is_ok();
                assert_eq!(
                    claimed,
                    taken,
                    "{}: accepts({size}) = {claimed} but insert = {taken}",
                    board.name(),
                );
            }
        }
    }

    /// Two boards in one machine must not read each other's disks.
    ///
    /// The two controllers' unit numbers share **one index space** — `disks[1]`
    /// is "unit 1" for whichever board took the disk at drive letter B: — so
    /// this is the shape of a silent-corruption bug: the hard disk asking for
    /// unit 1 at a 4.9 MB offset, served out of a 337 KB floppy image, or the
    /// floppy handed hard-disk sectors. What stops it is that each board keeps
    /// its own present-flags and `insert` only ever sets them for a disk of its
    /// own size, so a board asked for a unit it was never given refuses instead
    /// of reaching into the array.
    ///
    /// Asserted rather than reasoned, because "it happens to be guarded" and "it
    /// is guaranteed" look identical right up to the day someone adds a third
    /// board.
    #[test]
    fn test_two_controllers_do_not_read_each_others_disks() {
        use crate::cpm::hdsk::{IMAGE_LEN, SECTOR_LEN as HD_SECTOR};

        let mut m = BootMachine::new();
        // A floppy at drive 0 and a hard disk at drive 1, each filled with a
        // recognisable byte.
        m.insert(0, vec![0x11; Geometry::EIGHT_INCH.image_len() as usize], false).unwrap();
        m.insert(1, vec![0x22; IMAGE_LEN as usize], false).unwrap();

        // The hard disk reads *its* unit 1 and gets its own bytes.
        m.port_out(0xA1, 0); // a status read first, to clear the power-on byte
        m.port_in(0xA1);
        m.port_out(0xA7, 0x00);
        m.port_out(0xA3, 0x30 | 0x04); // read sector 0, head 0, unit 1
        m.port_out(0xA7, 0x00);
        m.port_out(0xA3, 0x50); // read buffer, all 256
        let mut got = Vec::new();
        for _ in 0..HD_SECTOR {
            got.push(m.port_in(0xA5));
        }
        assert!(got.iter().all(|&b| b == 0x22), "the hard disk must read its own image");

        // And *its* unit 0 — where the floppy is — is refused, not served with
        // floppy bytes at a hard-disk offset.
        m.port_out(0xA7, 0x00);
        m.port_out(0xA3, 0x30); // read sector 0, unit 0
        let err = m.port_in(0xA1);
        assert_eq!(
            err & crate::cpm::hdsk::error::NOT_READY,
            crate::cpm::hdsk::error::NOT_READY,
            "unit 0 holds a floppy, so the hard disk has no disk there: {err:#04x}"
        );

        // The floppy still reads its own drive 0, unaffected.
        m.port_out(0x08, 0);
        m.port_out(0x09, 0x04);
        for _ in 0..64 {
            if m.port_in(0x09) & 0x01 == 0 {
                break;
            }
        }
        let first = m.port_in(0x0A);
        let second = m.port_in(0x0A);
        assert!(
            [first, second].contains(&0x11),
            "the floppy must still read its own image, got {first:#04x} {second:#04x}"
        );
        // Nothing was written anywhere by reading.
        assert!(m.take_dirty().is_empty(), "reads must not dirty an image");
    }

    /// A blank hard disk is refused as the data disk it is.
    ///
    /// It used to be refused as "this disk is on a controller that cannot
    /// cold-start one yet", because a hard disk naming no boot program and a
    /// board with no bootstrap were the same `None`. The message sent the reader
    /// after missing code of ours when the truth was on the disk: an erased
    /// platter has an erased volume label, so there is nothing there to say
    /// where a boot program lives.
    #[test]
    fn test_a_blank_hard_disk_is_refused_as_data() {
        use crate::cpm::hdsk::IMAGE_LEN;

        for (fill, what) in [(0xE5u8, "formatted and empty"), (0x00, "all zeros")] {
            let mut m = BootMachine::new();
            m.insert(0, vec![fill; IMAGE_LEN as usize], true).unwrap();
            let mut cpu = BootMachine::new_cpu();
            let err = m.boot(&mut cpu, 0).expect_err("a blank disk cannot boot");
            assert_eq!(err, BootError::NotBootable, "{what}");
            assert!(err.to_string().contains("data, not a system disk"), "{what}: {err}");
        }
    }

    /// A format erases one whole recording surface of the image, and nothing of
    /// the other one.
    ///
    /// The interleaving is the part worth pinning: one head's sectors sit once
    /// per cylinder, a cylinder apart, so a fill that treated a surface as
    /// contiguous would erase the first half of the disk — both heads — and
    /// leave the second untouched.
    #[test]
    fn test_a_format_erases_one_surface_and_leaves_the_other() {
        use crate::cpm::hdsk::{IMAGE_LEN, SECTORS, SECTOR_LEN as HD_SECTOR};

        let track = SECTORS as usize * HD_SECTOR;
        let mut m = BootMachine::new();
        m.insert(0, vec![0x42; IMAGE_LEN as usize], false).unwrap();
        // Format head 1 on unit 0: operands in the low byte, bit 5 = side.
        m.port_out(0xA7, 1 << 5);
        m.port_out(0xA3, 0xC0);

        let dirty = m.take_dirty();
        assert_eq!(dirty.len(), 1, "the erased image comes back to be persisted");
        let bytes = &dirty[0].1;
        // Cylinder 0: head 0 untouched, head 1 erased. And the same at the end
        // of the disk, which is what proves the stride rather than a prefix.
        assert!(bytes[..track].iter().all(|&b| b == 0x42), "head 0 of cylinder 0 kept");
        assert!(bytes[track..2 * track].iter().all(|&b| b == 0xE5), "head 1 erased");
        let last = (IMAGE_LEN as usize) - 2 * track;
        assert!(bytes[last..last + track].iter().all(|&b| b == 0x42), "head 0 of the last");
        assert!(bytes[last + track..].iter().all(|&b| b == 0xE5), "head 1 of the last");
        assert_eq!(
            bytes.iter().filter(|&&b| b == 0xE5).count(),
            IMAGE_LEN as usize / 2,
            "exactly half the disk — one surface of two"
        );
    }

    /// And a protected image is not formatted, however the guest asks.
    #[test]
    fn test_a_read_only_image_is_never_formatted() {
        use crate::cpm::hdsk::IMAGE_LEN;

        let mut m = BootMachine::new();
        m.insert(0, vec![0x42; IMAGE_LEN as usize], true).unwrap();
        m.port_out(0xA7, 0);
        m.port_out(0xA3, 0xC0);
        assert!(m.take_dirty().is_empty(), "a protected image stays clean");
    }

    /// The reserved-port list is derived from the controllers, not written down
    /// beside them.
    ///
    /// It was written down, and it had already fallen behind: it named the
    /// floppy's three ports and knew nothing of the hard disk's eight, so a
    /// modem profile on A0–A7 would have been accepted and then shadowed by the
    /// controller in the port dispatch — a mute modem with nothing said about
    /// why.
    #[test]
    fn test_every_controller_port_is_refused_to_a_modem() {
        use crate::cpm::uart::{UartFamily, UartProfile};

        let mut m = BootMachine::new();
        for port in [0x08u8, 0x09, 0x0A, 0xA0, 0xA3, 0xA7] {
            let profile = UartProfile { status_port: port, data_port: port + 1, family: UartFamily::Acia };
            match m.attach_modem(ModemAccess::Ports(profile)) {
                ModemAttach::Unavailable(why) => {
                    assert!(why.contains("disk controller"), "{port:#04x}: {why}");
                }
                other => panic!("{port:#04x} belongs to a controller, got {other:?}"),
            }
        }
        // A port no board here claims is still fine.
        let profile = UartProfile { status_port: 0x12, data_port: 0x13, family: UartFamily::Acia };
        assert_eq!(m.attach_modem(ModemAccess::Ports(profile)), ModemAttach::Ports(0x12, 0x13));
    }

    /// Reading past the end of an image gives an erased sector rather than
    /// panicking — a real drive returns something from unformatted media.
    #[test]
    fn test_reading_past_the_end_yields_an_erased_sector() {
        let mut m = BootMachine::new();
        m.insert(0, image(Geometry::MINIDISK), true).unwrap();
        m.port_out(0x08, 0);
        m.port_out(0x09, 0x04);
        // Step well past the last track, then read.
        for _ in 0..100 {
            m.port_out(0x09, 0x01);
        }
        m.port_in(0x0A);
        let v = m.port_in(0x0A);
        assert!(v == 0xE5 || v == 0, "no panic, and a defined byte: {v:#04x}");
    }

    /// The first byte of a sector must reach the guest.
    ///
    /// Regression: reading the data port for a sector not yet buffered made the
    /// controller ask the host for it and answer 0xFF, and the caller then
    /// filled the buffer — so the byte the guest had asked for was dropped and
    /// every sector arrived one byte short. A disk boots far enough to look
    /// like it works and then goes silent.
    #[test]
    fn test_the_first_byte_of_a_sector_is_not_lost() {
        let mut img = image(Geometry::EIGHT_INCH);
        for (i, b) in img[..SECTOR_LEN].iter_mut().enumerate() {
            *b = (i as u8).wrapping_add(1); // distinct, and never 0xFF at [0]
        }
        let mut m = BootMachine::new();
        m.insert(0, img, true).unwrap();
        m.port_out(0x08, 0);
        m.port_out(0x09, 0x04);
        let first = m.port_in(0x0A);
        assert_eq!(first, 1, "the guest must get byte 0, not a placeholder");
        assert_eq!(m.port_in(0x0A), 2, "and then byte 1");
        assert_eq!(m.port_in(0x0A), 3);
    }

    /// After booting, the drive must report that the head may move.
    ///
    /// Regression: the bootstrap reads the data port to fetch the boot sector,
    /// which opens a transfer. An open transfer holds "safe to move the head"
    /// low, and the first thing a boot sector does is seek to track 0 — so the
    /// guest span forever at its first instruction that touched the drive.
    #[test]
    fn test_the_head_may_move_once_the_bootstrap_is_done() {
        let mut img = bootable_image(Geometry::EIGHT_INCH);
        img[3..3 + 8].copy_from_slice(&[0x31, 0x00, 0xDF, 0xF3, 0xAF, 0xD3, 0x08, 0xDB]);
        let mut m = BootMachine::new();
        m.insert(0, img, true).unwrap();
        let mut cpu = BootMachine::new_cpu();
        m.boot(&mut cpu, 0).expect("boots");
        // Status is active low: the move-OK bit reads 0 when it is safe.
        let status = m.port_in(0x08);
        assert_eq!(
            status & 0x02,
            0,
            "the head must be free to seek right after boot, got {status:#04x}"
        );
    }

    /// Booting sets the program counter to the entry point the bootstrap
    /// reports, and leaves the payload where the CPU will find it.
    #[test]
    fn test_boot_places_the_payload_and_sets_pc() {
        let mut img = bootable_image(Geometry::EIGHT_INCH);
        // A plausible boot sector: LXI SP,0DF00h / DI / XRA A / OUT 08h.
        let code = [0x31u8, 0x00, 0xDF, 0xF3, 0xAF, 0xD3, 0x08, 0xDB];
        img[3..3 + code.len()].copy_from_slice(&code);
        let mut m = BootMachine::new();
        m.insert(0, img, true).unwrap();
        let mut cpu = BootMachine::new_cpu();
        m.boot(&mut cpu, 0).expect("boots");
        assert_eq!(cpu.registers().pc(), 0x0000);
        assert_eq!(m.peek(0x0000), 0x31, "the payload is in memory");
        assert_eq!(m.peek(0x0007), 0xDB, "and its jump targets line up");
    }

    #[test]
    fn test_booting_an_empty_drive_reports_it() {
        let mut m = BootMachine::new();
        let mut cpu = BootMachine::new_cpu();
        assert!(matches!(m.boot(&mut cpu, 0), Err(BootError::NoDisk(0))));
    }

    /// Run a booted guest until it stops printing, and return what it said.
    ///
    /// "Stops printing" rather than a fixed instruction count: a CP/M command
    /// takes wildly different times depending on whether it touches the disk,
    /// and a fixed budget either truncates `DIR` or wastes seconds on a prompt.
    #[cfg(test)]
    fn run_until_quiet(m: &mut BootMachine, cpu: &mut Cpu, budget: u64) -> Vec<u8> {
        let mut out = Vec::new();
        let mut quiet: u64 = 0;
        for _ in 0..budget {
            m.step(cpu);
            let o = m.take_output();
            if o.is_empty() {
                quiet += 1;
                // Long enough to cover a seek and a track read.
                if quiet > 3_000_000 && !out.is_empty() {
                    break;
                }
            } else {
                quiet = 0;
                out.extend(o);
            }
        }
        out
    }

    #[cfg(test)]
    /// The "Space: 3744k" figure out of a CP/M `STAT` line.
    ///
    /// Its own function so the before and after readings are parsed by exactly
    /// the same code — a comparison between two different parsers proves
    /// nothing about the disk.
    fn free_k(stat: &str) -> Option<u32> {
        let tail = stat.split("Space:").nth(1)?;
        let digits: String = tail.trim_start().chars().take_while(|c| c.is_ascii_digit()).collect();
        digits.parse().ok()
    }

    fn printable(bytes: &[u8]) -> String {
        bytes
            .iter()
            .map(|&b| if (0x20..0x7F).contains(&b) || b == b'\n' { b as char } else { '.' })
            .collect()
    }

    /// The end-to-end test that exercises everything at once: boot a real disk,
    /// use its own operating system to list its own directory, run **our**
    /// terminal on it, and check that our virtual modem's ports reach it.
    ///
    /// EGT80 is the sharpest probe available for this. It is a real CP/M
    /// program, built by a period assembler, that drives a UART directly — so
    /// if it comes up and talks to `0x12`/`0x13` inside a booted Altair, then
    /// the controller, the bootstrap, the CPU, the console, the guest's own
    /// BDOS and our modem ports are all working together.
    ///
    /// **Blocked on getting EGT80 onto an Altair floppy in the first place**,
    /// which is the same unsolved mapping that keeps `altair8` out of
    /// `image::format::FORMATS`. Writing the file in with `cpmtools` — using
    /// the measured geometry and the skew recovered from the disk's own boot
    /// tracks — produces a directory entry for **extent 1 only**, with no
    /// extent 0 anywhere on the disk. CP/M's `DIR` lists extent-0 entries, so
    /// the guest correctly does not show it, and the file would be truncated
    /// even if it did. That is a fact about the Altair block mapping, not about
    /// booting: the same disk boots, runs its own `DIR` and lists its own
    /// forty-one files perfectly from the bytes we never touched.
    ///
    /// The way in that does not need us to understand the layout at all is the
    /// guest's own: these disks carry `PCGET.COM`, Mike Douglas's XMODEM
    /// receiver for the 88-2SIO, so the guest can pull EGT80 in over our
    /// virtual modem port and write it with its own BDOS. That is the next
    /// thing to try, and it would test more of the path than this does.
    ///
    /// Ignored: set `CPM_DATA_IMAGE` to an Altair CP/M floppy — EGT80 is written
    /// into a copy of it by our own writer, so this also exercises that path
    /// end to end. Framing an image back up needs two sector checksums, both
    /// measured from the disks themselves and holding for every sector of
    /// DISK01 (192/192 and 2272/2272): tracks 0-5 keep a plain sum of the 128
    /// data bytes at byte 132; tracks 6-76 keep the sum of the data plus header
    /// bytes 2, 3, 5 and 6, at byte 4.
    #[test]
    #[ignore]
    fn test_run_egt80_inside_a_booted_disk() {
        // Built here rather than demanded of the operator.  This asked for
        // `CPM_BOOT_IMAGE` to be "an image carrying EGT80.COM", which no test
        // produces and none keeps — so it could not be run, and an `#[ignore]`
        // test that cannot be run looks exactly like one that passes.
        let Ok(path) = std::env::var("CPM_DATA_IMAGE") else {
            eprintln!("set CPM_DATA_IMAGE to an Altair CP/M floppy (EGT80 is written into a copy)");
            return;
        };
        let bytes = altair_floppy_carrying_egt80(&path);
        let mut m = BootMachine::new();
        m.insert(0, bytes, true).expect("an 88-DCDD image");

        // The modem where a real Altair would have put it: 2SIO port B.
        assert_eq!(
            m.attach_modem(crate::cpm::resolve_access("altair_2sio2")),
            ModemAttach::Ports(0x12, 0x13),
        );

        let mut cpu = BootMachine::new_cpu();
        m.boot(&mut cpu, 0).expect("boots");

        let banner = printable(&run_until_quiet(&mut m, &mut cpu, 60_000_000));
        println!("--- sign-on ---\n{banner}");
        assert!(banner.contains("CP/M"), "no sign-on: {banner:?}");

        // The guest's own DIR, which is its filesystem answering, not ours.
        for &b in b"DIR\r" {
            m.send_key(b);
        }
        let dir = printable(&run_until_quiet(&mut m, &mut cpu, 200_000_000));
        println!("--- DIR ---\n{dir}");
        assert!(
            dir.to_ascii_uppercase().contains("EGT80"),
            "the guest's own DIR does not list EGT80: {dir:?}"
        );

        // Now run it.
        for &b in b"EGT80\r" {
            m.send_key(b);
        }
        let screen = printable(&run_until_quiet(&mut m, &mut cpu, 200_000_000));
        println!("--- EGT80 ---\n{screen}");
        assert!(
            screen.to_ascii_uppercase().contains("EGT80"),
            "EGT80 did not start: {screen:?}"
        );
        println!(
            "modem: guest wrote {:?}, rx free {}",
            printable(&m.modem().drain_tx()),
            m.modem().rx_free()
        );
    }

    /// Send a file to a booted guest with XMODEM, over the virtual modem's
    /// ports, while the guest runs.
    ///
    /// A sender rather than a receiver because that is the direction that puts
    /// a file *onto* a disk we cannot write ourselves. Deliberately written out
    /// here in the classic form rather than driven through `crate::xmodem`:
    /// that one speaks to an async stream, and what is on the other side of
    /// this is a synchronous pair of rings being clocked by a CPU. Keeping the
    /// two apart also means this test would notice if our own implementation
    /// drifted, instead of agreeing with it.
    ///
    /// Returns what the guest printed while the transfer ran.
    #[cfg(test)]
    fn xmodem_send_to_guest(
        m: &mut BootMachine,
        cpu: &mut Cpu,
        file: &[u8],
        budget: u64,
    ) -> (bool, Vec<u8>) {
        const SOH: u8 = 0x01;
        const EOT: u8 = 0x04;
        const ACK: u8 = 0x06;
        const NAK: u8 = 0x15;
        const CRC: u8 = b'C';

        let blocks: Vec<Vec<u8>> = file
            .chunks(128)
            .map(|c| {
                let mut b = c.to_vec();
                // The last block is padded, as XMODEM has no length field.
                b.resize(128, 0x1A);
                b
            })
            .collect();

        let mut console = Vec::new();
        let mut sent = 0usize; // blocks fully acknowledged
        let mut crc_mode = None; // decided by the receiver's first prompt
        let mut in_flight = false;
        let mut done = false;
        let mut eot_sent = false;

        for _ in 0..budget {
            m.step(cpu);
            console.extend(m.take_output());

            // Whatever the guest wrote at its UART is the receiver talking.
            for b in m.modem().drain_tx() {
                match b {
                    NAK | CRC if crc_mode.is_none() => crc_mode = Some(b == CRC),
                    ACK if in_flight => {
                        in_flight = false;
                        sent += 1;
                    }
                    ACK if eot_sent => done = true,
                    NAK if in_flight => in_flight = false, // resend the same block
                    _ => {}
                }
            }
            if done {
                break;
            }
            let Some(use_crc) = crc_mode else {
                continue; // the receiver has not asked yet
            };
            if in_flight || eot_sent {
                continue;
            }
            if sent == blocks.len() {
                m.modem().queue_rx(&[EOT]);
                eot_sent = true;
                continue;
            }

            let n = sent;
            let mut pkt = vec![SOH, (n as u8).wrapping_add(1), !(n as u8).wrapping_add(1)];
            pkt.extend_from_slice(&blocks[n]);
            if use_crc {
                // CRC-16/XMODEM, written out here rather than borrowed from
                // `crate::xmodem`: an independent implementation is the whole
                // value of a cross-check, and one that calls the code under
                // test would agree with it however wrong it was.
                let mut c: u16 = 0;
                for &byte in &blocks[n] {
                    c ^= (byte as u16) << 8;
                    for _ in 0..8 {
                        c = if c & 0x8000 != 0 { (c << 1) ^ 0x1021 } else { c << 1 };
                    }
                }
                pkt.push((c >> 8) as u8);
                pkt.push(c as u8);
            } else {
                pkt.push(blocks[n].iter().fold(0u8, |a, &b| a.wrapping_add(b)));
            }
            m.modem().queue_rx(&pkt);
            in_flight = true;
        }
        (done, console)
    }

    /// Receive a file from a booted guest with XMODEM, over the modem's ports.
    ///
    /// The other direction, and the only way to check that what the guest wrote
    /// to its disk is what we sent: read it back with the guest's own reader.
    #[cfg(test)]
    fn xmodem_receive_from_guest(
        m: &mut BootMachine,
        cpu: &mut Cpu,
        budget: u64,
    ) -> (Option<Vec<u8>>, Vec<u8>) {
        const SOH: u8 = 0x01;
        const EOT: u8 = 0x04;
        const ACK: u8 = 0x06;
        const NAK: u8 = 0x15;

        let mut console = Vec::new();
        let mut file = Vec::new();
        let mut wire: Vec<u8> = Vec::new();
        let mut done = false;
        // Checksum mode: the plainest thing every sender supports.
        let mut prompted = 0u64;

        for i in 0..budget {
            m.step(cpu);
            console.extend(m.take_output());
            wire.extend(m.modem().drain_tx());

            // Prod the sender until it starts, the way a real receiver does.
            if wire.is_empty() && file.is_empty() && i.wrapping_sub(prompted) > 20_000_000 {
                prompted = i;
                m.modem().queue_rx(&[NAK]);
            }

            loop {
                match wire.first() {
                    Some(&EOT) => {
                        wire.remove(0);
                        m.modem().queue_rx(&[ACK]);
                        done = true;
                    }
                    Some(&SOH) if wire.len() >= 132 => {
                        let pkt: Vec<u8> = wire.drain(..132).collect();
                        let sum = pkt[3..131].iter().fold(0u8, |a, &b| a.wrapping_add(b));
                        if pkt[1] == !pkt[2] && sum == pkt[131] {
                            file.extend_from_slice(&pkt[3..131]);
                            m.modem().queue_rx(&[ACK]);
                        } else {
                            m.modem().queue_rx(&[NAK]);
                        }
                    }
                    // Anything that is not the start of a packet is noise
                    // between frames; drop it rather than desynchronise.
                    Some(&b) if b != SOH => {
                        wire.remove(0);
                        continue;
                    }
                    _ => break,
                }
            }
            if done {
                break;
            }
        }
        (if done { Some(file) } else { None }, console)
    }

    /// The whole path, end to end, using nothing but the guest's own software:
    /// boot an Altair CP/M disk, run **its** XMODEM receiver, push our terminal
    /// into it through **our** virtual modem's ports, and see the file appear
    /// in the disk's own directory.
    ///
    /// This is the answer to a problem rather than a demonstration. We cannot
    /// write an Altair floppy from the host — `cpmtools` derives `EXM 1` where
    /// the disk's BIOS says `EXM 0`, so its directory entry is one the guest
    /// will not look at — and the block mapping past the first allocation block
    /// is still unsolved. None of that matters here, because the guest does its
    /// own filesystem work. It also tests more than the mount path ever could:
    /// the controller, the bootstrap, the CPU, the console, the guest's BDOS,
    /// its disk *writes*, and our modem ports, all at once.
    ///
    /// `PCGET.COM` is Mike Douglas's XMODEM receiver, on the Altair CP/M disks.
    /// Given `B` it uses 2SIO port B at 12h/13h — which is exactly the
    /// `altair_2sio2` profile, and why that is the one to pick for a booted
    /// Altair.
    ///
    /// Ignored: set `CPM_BOOT_IMAGE` to an Altair CP/M image carrying PCGET.COM.
    #[test]
    #[ignore]
    fn test_pcget_pulls_egt80_in_over_the_virtual_modem() {
        /// Our own terminal, the thing worth putting on the disk.
        const EGT80_COM: &[u8] = include_bytes!("../../EGT80/EGT80.COM");

        let Ok(path) = std::env::var("CPM_BOOT_IMAGE") else {
            eprintln!("set CPM_BOOT_IMAGE to an Altair CP/M image carrying PCGET.COM");
            return;
        };
        let bytes = std::fs::read(&path).unwrap();
        let mut m = BootMachine::new();
        // Writable: the point is that the guest writes to it.
        m.insert(0, bytes, false).expect("an 88-DCDD image");
        assert_eq!(
            m.attach_modem(crate::cpm::resolve_access("altair_2sio2")),
            ModemAttach::Ports(0x12, 0x13),
            "PCGET's port B is our altair_2sio2"
        );
        // A modem that is off-hook, since the guest is entitled to look.
        m.modem().set_carrier(true);

        let mut cpu = BootMachine::new_cpu();
        m.boot(&mut cpu, 0).expect("boots");
        let signon = printable(&run_until_quiet(&mut m, &mut cpu, 60_000_000));
        assert!(signon.contains("CP/M"), "no sign-on: {signon:?}");

        // Ask the guest to receive, on the port our modem is wired to.
        for &b in b"PCGET EGT80.COM B\r" {
            m.send_key(b);
        }
        let prompt = printable(&run_until_quiet(&mut m, &mut cpu, 200_000_000));
        println!("--- PCGET ---\n{prompt}");
        assert!(
            prompt.contains("XMODEM"),
            "PCGET did not ask for the file: {prompt:?}"
        );

        let (done, during) = xmodem_send_to_guest(&mut m, &mut cpu, EGT80_COM, 4_000_000_000);
        println!("--- transfer ---\n{}", printable(&during));
        // Whatever the guest managed to write, for inspection when this fails:
        // a sector the guest wrote and cannot read back is a fault in our
        // controller, and the only way to tell is to look at the bytes.
        if let Ok(dump) = std::env::var("CPM_BOOT_DUMP") {
            if let Some((_, img)) = m.take_dirty().first() {
                std::fs::write(&dump, img).unwrap();
                println!("(wrote the guest's image to {dump})");
            } else {
                println!("(the guest wrote nothing at all)");
            }
        }
        assert!(done, "the transfer never completed");

        let after = printable(&run_until_quiet(&mut m, &mut cpu, 400_000_000));
        println!("--- after ---\n{after}");
        assert!(
            after.contains("Transfer Complete"),
            "PCGET did not report success: {after:?}"
        );

        // The disk's own directory is the only witness that counts.
        for &b in b"DIR EGT80.COM\r" {
            m.send_key(b);
        }
        let dir = printable(&run_until_quiet(&mut m, &mut cpu, 400_000_000));
        println!("--- DIR ---\n{dir}");
        assert!(
            dir.to_ascii_uppercase().contains("EGT80"),
            "the guest wrote the file but does not list it: {dir:?}"
        );

        // Now read it back with the guest's own sender, which is the only
        // check that the bytes on the disk are the bytes we sent: it uses the
        // guest's filesystem, its block mapping and its BIOS, none of which we
        // understand well enough to verify ourselves.
        for &b in b"PCPUT EGT80.COM B\r" {
            m.send_key(b);
        }
        let ready = printable(&run_until_quiet(&mut m, &mut cpu, 400_000_000));
        println!("--- PCPUT ---\n{ready}");
        let (got, _) = xmodem_receive_from_guest(&mut m, &mut cpu, 4_000_000_000);
        let got = got.expect("the guest never sent the file back");
        assert!(
            got.len() >= EGT80_COM.len(),
            "got {} bytes back, sent {}",
            got.len(),
            EGT80_COM.len()
        );
        // XMODEM pads the last block, so compare only what we sent.
        assert_eq!(
            &got[..EGT80_COM.len()],
            EGT80_COM,
            "the file came back different from the one we sent"
        );
        println!("round trip: {} bytes, identical", EGT80_COM.len());

        // And it is in the image the caller would persist.
        let dirty = m.take_dirty();
        assert_eq!(dirty.len(), 1, "the written image comes back for saving");
        assert!(
            dirty[0].1.windows(5).any(|w| w == b"EGT80"),
            "the directory entry is in the image we would write out"
        );
    }

    /// Type at a booted guest and let it settle.
    #[cfg(test)]
    fn type_at(m: &mut BootMachine, cpu: &mut Cpu, keys: &[u8], budget: u64) -> String {
        for &b in keys {
            m.send_key(b);
        }
        printable(&run_until_quiet(m, cpu, budget))
    }

    /// Boot a disk, pull EGT80 onto it with the guest's own `PCGET`, and hand
    /// back the resulting image.
    ///
    /// Done once and reused, because it is the slow part and every case below
    /// wants the same disk.
    #[cfg(test)]
    fn image_with_egt80(path: &str, egt80: &[u8]) -> Vec<u8> {
        let mut m = BootMachine::new();
        m.insert(0, std::fs::read(path).unwrap(), false).expect("an 88-DCDD image");
        assert_eq!(
            m.attach_modem(crate::cpm::resolve_access("altair_2sio2")),
            ModemAttach::Ports(0x12, 0x13)
        );
        m.modem().set_carrier(true);
        let mut cpu = BootMachine::new_cpu();
        m.boot(&mut cpu, 0).expect("boots");
        run_until_quiet(&mut m, &mut cpu, 60_000_000);
        type_at(&mut m, &mut cpu, b"PCGET EGT80.COM B\r", 200_000_000);
        let (done, _) = xmodem_send_to_guest(&mut m, &mut cpu, egt80, 4_000_000_000);
        assert!(done, "could not put EGT80 on the disk");
        run_until_quiet(&mut m, &mut cpu, 400_000_000);
        m.take_dirty().pop().expect("the guest wrote the image").1
    }

    /// **Every comms port EGT80 offers, driven from inside a booted disk.**
    ///
    /// The question this answers is not "does the modem work" — `PCGET` already
    /// showed that — but "does the port the *operator* picks in EGT80 line up
    /// with the port they picked in the gateway". Those are two independent
    /// settings that have to name the same hardware, and nothing until now
    /// checked that they do.
    ///
    /// Each case boots the disk fresh, runs EGT80, walks its menus to select a
    /// port, and then moves bytes both ways over that port. The mismatch case
    /// at the end is the control: if it passed, the others would prove nothing,
    /// because a modem answering at every address would satisfy them all.
    ///
    /// Ignored: set `CPM_BOOT_IMAGE` to an Altair CP/M image carrying PCGET.COM.
    #[test]
    #[ignore]
    fn test_egt80_comms_ports_inside_a_booted_disk() {
        const EGT80_COM: &[u8] = include_bytes!("../../EGT80/EGT80.COM");
        let Ok(path) = std::env::var("CPM_BOOT_IMAGE") else {
            eprintln!("set CPM_BOOT_IMAGE to an Altair CP/M image carrying PCGET.COM");
            return;
        };
        let disk = image_with_egt80(&path, EGT80_COM);
        println!("EGT80 is on the disk; now testing its ports.\n");

        // (gateway profile, EGT80 menu keys, what its Port: line should say,
        //  whether the two should reach each other)
        // The expected text is EGT80's own "Port:" wording — the chip family
        // *and* the address — because a bare address matches all sorts of
        // unrelated things on that screen and would let a case pass without the
        // port having been selected at all.
        let cases: &[(&str, &[u8], &str, bool)] = &[
            // The pairing a booted Altair wants: 2SIO port B at both ends.
            ("altair_2sio2", b"SP32", "6850 ACIA at 12", true),
            // The gateway's own default port, which EGT80 offers by our name.
            ("rc2014_1b", b"SP1", "Z80 SIO/2 at 82", true),
            // The original MITS board.
            // `4` then `1`: EGT80 asks which address, as it does for the 2SIO.
            ("altair_sio", b"SP41", "Altair 88-SIO at 00", true),
            // The control: both ends working, aimed at different addresses.
            ("altair_2sio2", b"SP1", "Z80 SIO/2 at 82", false),
        ];

        for (uart, keys, want_port, should_reach) in cases {
            let mut m = BootMachine::new();
            m.insert(0, disk.clone(), true).unwrap();
            let attach = m.attach_modem(crate::cpm::resolve_access(uart));
            assert!(
                matches!(attach, ModemAttach::Ports(_, _)),
                "{uart} should be usable in a booted machine, got {attach:?}"
            );
            m.modem().set_carrier(true);

            let mut cpu = BootMachine::new_cpu();
            m.boot(&mut cpu, 0).expect("boots");
            run_until_quiet(&mut m, &mut cpu, 60_000_000);

            let start = type_at(&mut m, &mut cpu, b"EGT80\r", 400_000_000);
            assert!(
                start.contains("Ethernet Gateway Terminal"),
                "EGT80 did not start under {uart}: {start:?}"
            );

            // Settings -> Serial port -> the choice for this case.
            let picked = type_at(&mut m, &mut cpu, keys, 200_000_000);
            if !picked.contains(want_port) {
                // The screen is the evidence: a menu that asked something we
                // did not answer looks identical to a port that did not take.
                println!("--- {uart}: EGT80 after {} ---\n{picked}", String::from_utf8_lossy(keys));
                panic!("EGT80 does not report {want_port:?} under {uart}");
            }

            // Back out to the main menu and into terminal mode.
            type_at(&mut m, &mut cpu, b"Q", 100_000_000);
            type_at(&mut m, &mut cpu, b"T", 100_000_000);

            // Peer -> guest: what we queue should appear on EGT80's screen.
            m.modem().queue_rx(b"PING-FROM-GATEWAY");
            let seen = printable(&run_until_quiet(&mut m, &mut cpu, 200_000_000));

            // Guest -> peer: what we type should leave through the modem.
            let _ = m.modem().drain_tx();
            type_at(&mut m, &mut cpu, b"xyz", 100_000_000);
            let sent = String::from_utf8_lossy(&m.modem().drain_tx()).to_string();

            let reached = seen.contains("PING-FROM-GATEWAY");
            let replied = sent.contains("xyz");
            println!(
                "  {uart:<14} EGT80 {:<6} port {want_port}: in={} out={}",
                String::from_utf8_lossy(keys),
                if reached { "yes" } else { "no " },
                if replied { "yes" } else { "no" },
            );
            if *should_reach {
                assert!(reached, "{uart}: EGT80 never showed what we sent — {seen:?}");
                assert!(replied, "{uart}: typing never reached the modem — {sent:?}");
            } else {
                assert!(
                    !reached && !replied,
                    "{uart} answered a port EGT80 was not pointed at — \
                     the matching cases prove nothing if this one passes"
                );
            }
        }
    }

    /// The deep test of the port a booted Altair actually uses: move a real
    /// file through **EGT80's own XMODEM**, at volume, and read it back.
    ///
    /// The port matrix proves each port is wired to the right addresses, but it
    /// does so with a burst of a few bytes. That is not the same as a working
    /// link: a UART that drops a byte under load, or gets its transmit-ready
    /// bit wrong, passes a short burst and fails a file. This sends 4 KB — 32
    /// XMODEM blocks, each acknowledged — through the terminal we ship, has the
    /// guest write it to its own disk, and then has EGT80 read it back off that
    /// disk and send it out again, and
    /// compares. Every byte has to survive EGT80's receiver, our modem rings,
    /// the guest's filesystem, the 88-DCDD write and read paths, and EGT80's
    /// sender — and come back identical.
    ///
    /// Ignored: set `CPM_BOOT_IMAGE` to an Altair CP/M image carrying PCGET.COM.
    #[test]
    #[ignore]
    fn test_egt80_transfers_a_file_over_2sio2() {
        const EGT80_COM: &[u8] = include_bytes!("../../EGT80/EGT80.COM");
        let Ok(path) = std::env::var("CPM_BOOT_IMAGE") else {
            eprintln!("set CPM_BOOT_IMAGE to an Altair CP/M image carrying PCGET.COM");
            return;
        };
        // Deliberately not EGT80's own bytes: a file that happened to be left
        // on the disk would otherwise let this pass without transferring
        // anything. A pattern that is not 8.3-ish text also shows up plainly if
        // it lands in the wrong place.
        let payload: Vec<u8> = (0..4096u32).map(|i| (i as u8) ^ 0x5A).collect();

        let disk = image_with_egt80(&path, EGT80_COM);
        let mut m = BootMachine::new();
        m.insert(0, disk, false).unwrap();
        assert_eq!(
            m.attach_modem(crate::cpm::resolve_access("altair_2sio2")),
            ModemAttach::Ports(0x12, 0x13)
        );
        m.modem().set_carrier(true);
        let mut cpu = BootMachine::new_cpu();
        m.boot(&mut cpu, 0).expect("boots");
        run_until_quiet(&mut m, &mut cpu, 60_000_000);

        // EGT80, on 88-2SIO port B.
        let start = type_at(&mut m, &mut cpu, b"EGT80\r", 400_000_000);
        assert!(start.contains("Ethernet Gateway Terminal"), "{start:?}");
        let picked = type_at(&mut m, &mut cpu, b"SP32", 200_000_000);
        assert!(picked.contains("6850 ACIA at 12"), "{picked:?}");
        type_at(&mut m, &mut cpu, b"Q", 100_000_000);

        // Its own XMODEM receive, into a file on the guest's disk.
        let ask = type_at(&mut m, &mut cpu, b"D", 200_000_000);
        assert!(
            ask.contains("Receive as which file?"),
            "EGT80 did not ask for a name: {ask:?}"
        );
        type_at(&mut m, &mut cpu, b"XFER.DAT\r", 200_000_000);

        let (done, seen) = xmodem_send_to_guest(&mut m, &mut cpu, &payload, 4_000_000_000);
        assert!(done, "EGT80 never finished receiving: {}", printable(&seen));
        let after = printable(&run_until_quiet(&mut m, &mut cpu, 400_000_000));
        println!("--- EGT80 receive ---\n{}{after}", printable(&seen));
        assert!(after.contains("Received."), "EGT80 did not report success: {after:?}");

        // Verified from the image itself rather than by driving EGT80's exit
        // path.  The first attempt typed `X` then `DIR XFER.DAT`, and when the
        // exit did not land where expected those keystrokes went into EGT80's
        // own menu — where the *echo* of what was typed contained "XFER" and
        // made the check pass while proving nothing.  The bytes the guest
        // committed to the disk cannot be faked by an echo.
        let img = m.take_dirty().pop().expect("the guest wrote the disk").1;
        let entry = img
            .windows(11)
            .position(|w| w == b"XFER    DAT")
            .expect("no directory entry for XFER.DAT on the disk");
        println!("directory entry for XFER.DAT at byte {entry}");
        assert!(
            img.windows(64).any(|w| w == &payload[..64]),
            "the file's own bytes are not on the disk"
        );
        println!("{} bytes through EGT80's own XMODEM, onto the guest's disk", payload.len());

        // The other half: EGT80's *send* path, reading the file back off the
        // guest's disk and pushing it out the same port.  That closes the loop
        // through the terminal we ship rather than through the disk's own
        // tools.
        //
        // EGT80 ends a transfer with "Press any key." before it returns to its
        // menu, so a key comes first.  Getting that wrong is what made an
        // earlier attempt type its next command into the menu instead of at
        // `A>`.
        let back_at_menu = type_at(&mut m, &mut cpu, b" ", 200_000_000);
        assert!(
            back_at_menu.contains("Choice:"),
            "EGT80 did not come back to its menu: {back_at_menu:?}"
        );
        let ask = type_at(&mut m, &mut cpu, b"U", 200_000_000);
        if !ask.contains("Send which file?") {
            println!("--- EGT80 after U ---\n{ask}");
            panic!("EGT80 did not ask which file to send");
        }
        type_at(&mut m, &mut cpu, b"XFER.DAT\r", 400_000_000);

        let (back, sending) = xmodem_receive_from_guest(&mut m, &mut cpu, 4_000_000_000);
        println!("--- EGT80 send ---\n{}", printable(&sending));
        let back = back.expect("EGT80 never sent the file back");
        assert!(
            back.len() >= payload.len(),
            "EGT80 sent {} bytes of {}",
            back.len(),
            payload.len()
        );
        // XMODEM pads the last block, so compare only what was sent in.
        assert_eq!(
            &back[..payload.len()],
            &payload[..],
            "what EGT80 sent back differs from what it received"
        );
        println!(
            "{} bytes in through EGT80's XMODEM, onto the disk, and back out again",
            payload.len()
        );
    }

    /// The Altair hard disk boots its own CP/M, off a 4.9 MB image, through a
    /// controller built from the published manual.
    ///
    /// The strong oracle again: a sign-on cannot be produced by a plausible
    /// wrong answer. Reaching it means the 4PIO handshake, the command word
    /// layout, the two-step buffer transfer, the sector addressing and the
    /// first-stage boot program are all right together — and `DIR` afterwards
    /// means the guest's own BIOS agrees about where its files are.
    ///
    /// Ignored: set `CPM_HDSK_IMAGE` to an Altair 88-HDSK CP/M image
    /// (HDSK03.DSK or HDSK04.DSK in the Altair-Duino set).
    #[test]
    #[ignore]
    fn test_an_altair_hard_disk_boots_and_lists_its_files() {
        let Ok(path) = std::env::var("CPM_HDSK_IMAGE") else {
            eprintln!("set CPM_HDSK_IMAGE to an 88-HDSK CP/M image to run this");
            return;
        };
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.len() as u64, crate::cpm::hdsk::IMAGE_LEN, "not a 4.9 MB hard disk");

        let mut m = BootMachine::new();
        m.insert(0, bytes, true).expect("the hard-disk controller takes it");
        let mut cpu = BootMachine::new_cpu();
        m.boot(&mut cpu, 0).expect("boots");

        let signon = printable(&run_until_quiet(&mut m, &mut cpu, 200_000_000));
        println!("--- sign-on ---\n{signon}");
        assert!(signon.contains("CP/M"), "no sign-on: {signon:?}");
        assert!(signon.contains("88-HDSK"), "not the hard-disk system: {signon:?}");

        // Its own filesystem, answering.  The sign-on alone only proves the
        // loader ran; this proves the BIOS can still find sectors afterwards.
        let dir = type_at(&mut m, &mut cpu, b"DIR\r", 400_000_000);
        println!("--- DIR ---\n{dir}");
        assert!(dir.to_ascii_uppercase().contains("PIP"), "no directory: {dir:?}");

        // And free space, which needs the whole 4.9 MB addressed correctly
        // rather than just cylinder zero.
        let stat = type_at(&mut m, &mut cpu, b"STAT\r", 400_000_000);
        println!("--- STAT ---\n{stat}");
        assert!(stat.contains("Space:"), "no free space reported: {stat:?}");
    }

    /// Write a file inside a booted hard disk, then boot the result and find it
    /// still there.
    ///
    /// The read path had a strong oracle already — a sign-on the guest's own
    /// loader produces, and a `DIR` its own BIOS answers. This is the same trick
    /// pointed at the write path, and it needs to be *two* sessions to mean
    /// anything. One session proves only that the controller's buffer can be
    /// read back out of itself, which it could do while writing to entirely the
    /// wrong sector; the guest would still see its own directory because it is
    /// reading back the same wrong place. Coming up in a *second* boot, from
    /// bytes that went out to a file and came back, is what pins the addressing:
    /// the directory sector, the data blocks and the allocation bitmap all have
    /// to have landed where this disk's own BIOS goes looking for them.
    ///
    /// `SAVE` rather than `PIP` deliberately — it is one command with no
    /// end-of-file to feed, so nothing about the test depends on the console.
    ///
    /// Ignored: set `CPM_HDSK_IMAGE` to an 88-HDSK CP/M image (HDSK03/HDSK04).
    #[test]
    #[ignore]
    fn test_a_file_written_in_a_booted_hard_disk_survives_a_reboot() {
        let Ok(path) = std::env::var("CPM_HDSK_IMAGE") else {
            eprintln!("set CPM_HDSK_IMAGE to an 88-HDSK CP/M image to run this");
            return;
        };
        let original = std::fs::read(&path).unwrap();

        // Session one: save two pages under a name nothing on the disk uses.
        let mut m = BootMachine::new();
        m.insert(0, original.clone(), false).expect("the hard-disk controller takes it");
        let mut cpu = BootMachine::new_cpu();
        m.boot(&mut cpu, 0).expect("boots");
        let _ = run_until_quiet(&mut m, &mut cpu, 200_000_000);

        // Free space *before* anything is written.  An absolute figure would be
        // no test at all: these images ship with wildly different amounts free
        // (3744k on one, 1060k on another), so any constant threshold passes on
        // one disk without the write having happened at all.
        let before = free_k(&type_at(&mut m, &mut cpu, b"STAT\r", 400_000_000))
            .expect("STAT reports free space before the write");

        let saved = type_at(&mut m, &mut cpu, b"SAVE 2 ZZTEST.COM\r", 400_000_000);
        println!("--- SAVE ---\n{saved}");
        let listed = type_at(&mut m, &mut cpu, b"STAT ZZTEST.COM\r", 400_000_000);
        println!("--- STAT in session one ---\n{listed}");
        assert!(listed.to_ascii_uppercase().contains("ZZTEST"), "the save did not take: {listed:?}");

        // The bytes as they would reach the host's `.dsk` file.
        let after = m.take_dirty().into_iter().next().expect("the guest dirtied the disk").1;
        assert_eq!(after.len(), original.len(), "the image changed size");
        assert_ne!(after, original, "nothing was written");

        // Session two: a fresh machine, a fresh CPU, and only those bytes.
        let mut m2 = BootMachine::new();
        m2.insert(0, after, true).unwrap();
        let mut cpu2 = BootMachine::new_cpu();
        m2.boot(&mut cpu2, 0).expect("the written image still boots");
        let signon = printable(&run_until_quiet(&mut m2, &mut cpu2, 200_000_000));
        assert!(signon.contains("CP/M"), "the written image lost its loader: {signon:?}");

        let dir = type_at(&mut m2, &mut cpu2, b"DIR ZZTEST.COM\r", 400_000_000);
        println!("--- DIR in session two ---\n{dir}");
        assert!(dir.to_ascii_uppercase().contains("ZZTEST"), "the file did not survive: {dir:?}");

        // And the allocation bitmap moved with it — a directory entry alone
        // could be there with the blocks still marked free, which is the shape
        // of corruption that only shows up on the *next* file written.
        let stat = type_at(&mut m2, &mut cpu2, b"STAT\r", 400_000_000);
        println!("--- STAT in session two ---\n{stat}");
        let after_free = free_k(&stat).expect("STAT reports free space after the reboot");
        assert!(
            after_free < before,
            "free space went {before}k -> {after_free}k, so the blocks were never claimed: {stat:?}"
        );
    }

    /// A guest writes another disk's **system tracks**, and the result boots.
    ///
    /// This is the last write path on the floppy that nothing else reached. The
    /// two framing regions keep their sector checksums differently — tracks 0-5
    /// put the sum of the 128 data bytes at byte 132, tracks 6-76 put the sum of
    /// the data *plus* header bytes 2, 3, 5 and 6 at byte 4 — and every other
    /// test writes files, which live in the second region. `SYSGEN` writes the
    /// first.
    ///
    /// The oracle is as strong as this project gets: the guest writes the system
    /// with its own utility through our controller, and then that image is booted
    /// and has to sign on. A wrong checksum, a wrong offset, or a sector
    /// committed to the wrong track all fail it, and each of those has happened
    /// here at least once.
    ///
    /// Ignored:
    ///   `CPM_TOOL_IMAGE=...DISK01.DSK`   boots, carries SYSGEN
    ///   `CPM_DATA_IMAGE=...DISK05.DSK`   a different CP/M, whose system is replaced
    #[test]
    #[ignore]
    fn test_a_system_track_written_by_a_guest_still_boots() {
        let (Ok(tool), Ok(data)) = (
            std::env::var("CPM_TOOL_IMAGE"),
            std::env::var("CPM_DATA_IMAGE"),
        ) else {
            eprintln!("set CPM_TOOL_IMAGE (has SYSGEN) and CPM_DATA_IMAGE to run this");
            return;
        };

        let mut m = BootMachine::new();
        m.insert(0, std::fs::read(&tool).unwrap(), true).expect("an 88-DCDD tool image");
        m.insert(1, std::fs::read(&data).unwrap(), false).expect("an 88-DCDD data image");
        let mut cpu = BootMachine::new_cpu();
        m.boot(&mut cpu, 0).expect("boots");
        let banner = printable(&run_until_quiet(&mut m, &mut cpu, 60_000_000));
        assert!(banner.contains("CP/M"), "no sign-on: {banner:?}");

        // SYSGEN reads the system off A: and writes it to B:, one prompt at a
        // time. Its own words are checked, because "Function complete" is the
        // only thing that distinguishes a write from a polite refusal.
        let out = type_at(&mut m, &mut cpu, b"SYSGEN\r", 200_000_000);
        assert!(out.contains("Source drive"), "SYSGEN did not start: {out:?}");
        let out = type_at(&mut m, &mut cpu, b"A", 200_000_000);
        assert!(out.contains("Source on A"), "{out:?}");
        let read = type_at(&mut m, &mut cpu, b"\r", 400_000_000);
        assert!(read.contains("Function complete"), "the read failed: {read:?}");
        let out = type_at(&mut m, &mut cpu, b"B", 200_000_000);
        assert!(out.contains("Destination on B"), "{out:?}");
        let wrote = type_at(&mut m, &mut cpu, b"\r", 400_000_000);
        assert!(wrote.contains("Function complete"), "the write failed: {wrote:?}");

        let written = m
            .take_dirty()
            .into_iter()
            .find(|(unit, _)| *unit == 1)
            .expect("unit 1 was written")
            .1;

        // The checksums of the region that was just written, checked here rather
        // than trusted: this is the formula for tracks 0-5, and it is not the one
        // the rest of the disk uses.
        for track in 0..6u8 {
            for sector in 0..32u8 {
                let off = Geometry::EIGHT_INCH.offset(track, sector) as usize;
                let s = &written[off..off + SECTOR_LEN];
                let sum = s[3..131].iter().fold(0u8, |a, &b| a.wrapping_add(b));
                assert_eq!(s[132], sum, "track {track} sector {sector} checksum");
            }
        }

        // And the acceptance test: the disk the guest wrote must boot on its own.
        let mut m2 = BootMachine::new();
        m2.insert(0, written, true).expect("still an 88-DCDD image");
        let mut cpu2 = BootMachine::new_cpu();
        m2.boot(&mut cpu2, 0).expect("the guest-written system boots");
        let banner2 = printable(&run_until_quiet(&mut m2, &mut cpu2, 60_000_000));
        println!("--- sign-on from the guest-written system ---\n{banner2}");
        assert!(banner2.contains("CP/M"), "no sign-on after SYSGEN: {banner2:?}");

        // Its own files must still be there — SYSGEN writes the system tracks and
        // nothing else, so a directory that came back empty would mean we had
        // written over the data region.
        let dir = type_at(&mut m2, &mut cpu2, b"DIR\r", 400_000_000);
        println!("--- DIR ---\n{dir}");
        assert!(dir.contains("COM"), "the data tracks did not survive: {dir:?}");
    }

    /// A Tarbell disk boots, lists its files, and a file written in it survives a
    /// reboot.
    ///
    /// The acceptance test for the whole board, and it boots **twice** on purpose
    /// for the same reason the hard disk's does: one session proves almost
    /// nothing, because a guest can write to the wrong sector and still find its
    /// file afterwards by reading the directory back from the same wrong place.
    /// The second boot starts from bytes that left the machine.
    ///
    /// Everything the board does is on the path: the PROM's synthesised load and
    /// its entry at 7Dh, the FD1771's Type I seeks and Type II transfers, the
    /// status register typed per command, the WAIT port, the drive latch, and the
    /// sector arithmetic — a mistake in any of them ends in `Bdos Err` rather than
    /// a directory.
    ///
    /// Ignored: set `CPM_TARBELL_IMAGE` to a Tarbell CP/M image with space on it.
    #[test]
    #[ignore]
    fn test_a_file_written_in_a_booted_tarbell_disk_survives_a_reboot() {
        let Ok(path) = std::env::var("CPM_TARBELL_IMAGE") else {
            eprintln!("set CPM_TARBELL_IMAGE to a Tarbell CP/M image");
            return;
        };
        let original = std::fs::read(&path).unwrap();
        assert_eq!(
            original.len() as u64,
            crate::cpm::tarbell::IMAGE_LEN,
            "not a Tarbell image"
        );

        // ---- session one: sign on, list, and write ------------------------
        let mut m = BootMachine::new();
        m.insert(0, original.clone(), false).expect("the Tarbell controller takes it");
        let mut cpu = BootMachine::new_cpu();
        m.boot(&mut cpu, 0).expect("boots");
        let banner = printable(&run_until_quiet(&mut m, &mut cpu, 60_000_000));
        println!("--- sign-on ---\n{banner}");
        assert!(banner.contains("CP/M") || banner.contains("CPM"), "no sign-on: {banner:?}");

        // The guest's own directory: this is its filesystem answering, through
        // its own BIOS, and it is what a wrong sector mapping fails.
        let dir = type_at(&mut m, &mut cpu, b"DIR\r", 400_000_000);
        println!("--- DIR ---\n{dir}");
        assert!(dir.contains("COM"), "the guest's own DIR listed nothing: {dir:?}");

        // Free space before and after, never a threshold: a fixed number would
        // pass vacuously on a disk that happened to start below it.
        let before = free_k(&type_at(&mut m, &mut cpu, b"STAT\r", 400_000_000))
            .expect("STAT reports free space");
        let saved = type_at(&mut m, &mut cpu, b"SAVE 2 ZZTARB.COM\r", 400_000_000);
        println!("--- SAVE ---\n{saved}");
        let listed = type_at(&mut m, &mut cpu, b"STAT ZZTARB.COM\r", 400_000_000);
        println!("--- STAT ZZTARB.COM ---\n{listed}");
        assert!(
            listed.to_ascii_uppercase().contains("ZZTARB"),
            "the guest cannot see the file it just wrote: {listed:?}"
        );

        let after = m.take_dirty().into_iter().next().expect("the guest dirtied the disk").1;
        assert_ne!(after, original, "the image really changed");

        // ---- session two: from the bytes that came out --------------------
        let mut m2 = BootMachine::new();
        m2.insert(0, after, false).expect("still a Tarbell image");
        let mut cpu2 = BootMachine::new_cpu();
        m2.boot(&mut cpu2, 0).expect("the written image still boots");
        let banner2 = printable(&run_until_quiet(&mut m2, &mut cpu2, 60_000_000));
        assert!(
            banner2.contains("CP/M") || banner2.contains("CPM"),
            "no sign-on second time: {banner2:?}"
        );

        let dir2 = type_at(&mut m2, &mut cpu2, b"DIR ZZTARB.COM\r", 400_000_000);
        println!("--- DIR ZZTARB.COM, session two ---\n{dir2}");
        assert!(
            dir2.to_ascii_uppercase().contains("ZZTARB"),
            "the file did not survive the reboot: {dir2:?}"
        );
        let stat2 = type_at(&mut m2, &mut cpu2, b"STAT\r", 400_000_000);
        let after_free = free_k(&stat2).expect("STAT reports free space after the reboot");
        assert!(
            after_free < before,
            "free space went {before}k -> {after_free}k, so the blocks were never claimed"
        );
    }

    /// Boot every image in a folder and print what each one says.
    ///
    /// The survey behind the single-disk test: one run tells you which
    /// operating systems reach a sign-on and which go quiet, which is the only
    /// way to see a bring-up move forwards or backwards as a whole. Ignored —
    /// set `CPM_BOOT_DIR` to a folder of `.dsk` files.
    #[test]
    #[ignore]
    fn test_boot_every_image_and_report() {
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
        let mut spoke = 0;
        for name in &names {
            let bytes = std::fs::read(std::path::Path::new(&dir).join(name)).unwrap();
            let Some(medium) = BootMachine::medium_for(bytes.len() as u64) else {
                println!("  skipped  {name}  ({} bytes — no controller takes it)", bytes.len());
                continue;
            };
            let _ = medium;
            let mut m = BootMachine::new();
            // The survey is the one place that sees a bring-up move as a whole,
            // so it has to be able to survey a *machine* and not just a disk:
            // three of the Tarbell disks differ from the Altair only in where
            // their console is.
            // `auto` here too, so the survey shows what an operator who set
            // nothing at all would get.
            let configured = std::env::var("CPM_BOOT_MACHINE")
                .unwrap_or_else(|_| crate::cpm::console::AUTO_MACHINE.to_string());
            let (key, _why) = crate::cpm::detect::machine_for(&configured, &bytes);
            m.set_machine(&key);
            // Reported rather than unwrapped: a machine that cannot carry this
            // size is a legitimate outcome (a z80pack tool disk under the Altair
            // boards, say), and panicking here killed the survey partway and hid
            // every disk after it -- which read as a boot regression.
            if let Err(e) = m.insert(0, bytes, true) {
                println!("  skipped  {name}  ({e})");
                continue;
            }
            let mut cpu = BootMachine::new_cpu();
            if let Err(e) = m.boot(&mut cpu, 0) {
                println!("  refused  {name}: {e}");
                continue;
            }
            let mut out = m.take_output();
            for _ in 0..20_000_000u64 {
                m.step(&mut cpu);
                out.extend(m.take_output());
                if out.len() > 120 {
                    break;
                }
            }
            let text: String = out
                .iter()
                .map(|&b| if (0x20..0x7F).contains(&b) { b as char } else { '.' })
                .collect();
            if out.is_empty() {
                println!(
                    "  SILENT   {name}  pc={:#06x} stuck_polls={}",
                    cpu.registers().pc(),
                    m.stuck_polls()
                );
            } else {
                spoke += 1;
                println!("  spoke    {name}: {text}");
            }
        }
        assert!(spoke > 0, "no image in {dir} said anything");
    }

    /// **A guest that prints through a monitor ROM reaches its prompt and takes
    /// commands.**
    ///
    /// The gate for the synthesised CUTER entry, and it is the strong kind: not
    /// "did a plausible string appear" but "did the disk's own operating system
    /// sign on, accept a command, and run a program off the disk to produce the
    /// right answer". Every part has to work for that — the Tarbell board, the
    /// `04h`/`05h` console's active-low status, the ROM stub's register
    /// discipline, and the console data port accepting the stub's `OUT`.
    ///
    /// TDISK05 is the case. Its BIOS assembles with `VIDEO EQU TRUE` and prints
    /// with one `CALL 0C019h`, having put the character in `B` and cleared `A` to
    /// select CUTER's output device 0. Its system tracks contain that single call
    /// into `C0xx` and no other, so the stub is the whole of what it needs.
    ///
    /// Ignored: set `CPM_CUTER_IMAGE` to TDISK05.DSK.
    #[test]
    #[ignore]
    fn test_a_rom_console_guest_signs_on_and_runs_a_command() {
        let Ok(path) = std::env::var("CPM_CUTER_IMAGE") else {
            eprintln!("set CPM_CUTER_IMAGE to run this");
            return;
        };
        let mut m = BootMachine::new();
        m.set_machine("console_04_cuter");
        m.insert(0, std::fs::read(&path).unwrap(), true).expect("a bootable image");
        let mut cpu = BootMachine::new_cpu();
        m.boot(&mut cpu, 0).expect("boots");

        let signon = printable(&run_until_quiet(&mut m, &mut cpu, 60_000_000));
        println!("--- sign-on ---\n{signon}");
        assert!(
            signon.contains("Tarbell 48K CPM 2.2"),
            "the guest never signed on through the ROM; it printed: {signon:?}"
        );
        assert!(signon.contains("A>"), "it signed on but never reached its prompt: {signon:?}");

        // A command, so this cannot pass on a sign-on alone. `DIR` is the CCP's
        // own, and it has to read the directory off the disk to answer.
        let dir = type_at(&mut m, &mut cpu, b"DIR\r", 400_000_000);
        println!("--- DIR ---\n{dir}");
        assert!(dir.contains("COM"), "DIR listed no programs: {dir:?}");

        // And a transient: `STAT` is a `.COM` file the CCP has to find, load and
        // run, which exercises far more of the guest than the CCP alone.
        let stat = type_at(&mut m, &mut cpu, b"STAT\r", 400_000_000);
        println!("--- STAT ---\n{stat}");
        assert!(
            stat.contains("Bytes Remaining") || stat.contains("R/W") || stat.contains("R/O"),
            "STAT did not run or said nothing recognisable: {stat:?}"
        );
    }

    /// **A ROM-console guest writes a file, and the rewritten image boots
    /// again.**
    ///
    /// The completeness gate for a machine whose console is a monitor ROM, and
    /// the reason it is separate from the sign-on test: signing on proves the
    /// console, and nothing more. This drives a whole session — create a file
    /// with the guest's own utility, hand the image back, boot the *result* on a
    /// fresh machine and read the directory — so the console, the Tarbell
    /// controller's write path, the image write-back and the ROM placement on the
    /// second boot all have to work together.
    ///
    /// The ROM placement is the part worth having a test for. It happens after
    /// the boot program is loaded, on *every* boot, so a second boot of a
    /// rewritten image is exactly where a one-shot placement would show up — and
    /// it would present as a disk that worked once and then went silent.
    ///
    /// Ignored: set `CPM_CUTER_IMAGE` to TDISK05.DSK.
    #[test]
    #[ignore]
    fn test_a_rom_console_guest_writes_a_file_that_survives_a_reboot() {
        let Ok(path) = std::env::var("CPM_CUTER_IMAGE") else {
            eprintln!("set CPM_CUTER_IMAGE to run this");
            return;
        };
        let original = std::fs::read(&path).unwrap();

        let mut m = BootMachine::new();
        m.set_machine("console_04_cuter");
        // Writable this time — the whole point.
        m.insert(0, original.clone(), false).expect("a bootable image");
        let mut cpu = BootMachine::new_cpu();
        m.boot(&mut cpu, 0).expect("boots");
        let signon = printable(&run_until_quiet(&mut m, &mut cpu, 60_000_000));
        assert!(signon.contains("A>"), "never reached its prompt: {signon:?}");

        // `PIP` copies a file the disk already has to a new name, so the guest
        // allocates blocks and writes a directory entry using its own BIOS.
        let pip = type_at(&mut m, &mut cpu, b"PIP NEWFILE.TXT=SYSGEN.TXT\r", 400_000_000);
        println!("--- PIP ---\n{pip}");
        let dir = type_at(&mut m, &mut cpu, b"DIR NEWFILE.TXT\r", 400_000_000);
        println!("--- DIR ---\n{dir}");
        assert!(dir.contains("NEWFILE"), "the guest did not create the file: {dir:?}");

        // The image as the guest left it.
        let written = m.take_dirty().into_iter().find(|(u, _)| *u == 0).map(|(_, b)| b);
        let written = written.expect("the guest wrote to the image");
        assert_ne!(written, original, "the image came back unchanged");
        assert_eq!(written.len(), original.len(), "the image changed size");

        // Boot what the guest wrote, on a brand-new machine.
        let mut m2 = BootMachine::new();
        m2.set_machine("console_04_cuter");
        m2.insert(0, written, true).expect("the rewritten image is still a disk");
        let mut cpu2 = BootMachine::new_cpu();
        m2.boot(&mut cpu2, 0).expect("the rewritten image still boots");
        let signon2 = printable(&run_until_quiet(&mut m2, &mut cpu2, 60_000_000));
        assert!(
            signon2.contains("Tarbell 48K CPM 2.2"),
            "the rewritten image did not sign on: {signon2:?}"
        );
        let dir2 = type_at(&mut m2, &mut cpu2, b"DIR NEWFILE.TXT\r", 400_000_000);
        println!("--- DIR after reboot ---\n{dir2}");
        assert!(dir2.contains("NEWFILE"), "the file did not survive the reboot: {dir2:?}");
    }

    /// The DMA path indexes memory raw, and this is why that is safe.
    ///
    /// `HostRequest::Dma` walks `self.mem[addr.wrapping_add(i) as usize]` with no
    /// bounds check, which is sound only because the address is a `u16` and the
    /// memory is exactly 64 KB. Written down as a test because the safety of that
    /// code is invisible at the point of use — someone shrinking `mem`, or
    /// widening an address, would turn a wrap into a panic on a guest's disk read.
    #[test]
    fn test_memory_is_exactly_the_sixteen_bit_address_space() {
        let m = BootMachine::new();
        assert_eq!(m.mem.len(), 0x1_0000, "the DMA path relies on this");
        // The extremes a wrapping u16 can reach, both in range.
        assert_eq!(u16::MAX as usize, m.mem.len() - 1);
    }

    /// A guest blocked on a console read must look **idle** to the driver, or a
    /// session sitting at its prompt costs a core.
    ///
    /// This is asserted rather than reasoned about because the reasoning was
    /// wrong once: a comment here claimed the driver paces on
    /// `idle_status_reads`, and it does not — it paces on printed output,
    /// keystrokes, modem traffic and disk accesses. A blocked guest happens to
    /// produce none of those, which is *why* it naps. That is a property worth
    /// pinning, since it is the difference between an idle session at 0% and one
    /// at 100%, and nothing else would catch it changing.
    #[test]
    fn test_a_blocked_guest_looks_idle_to_the_driver() {
        // A tiny program that does what z80pack's CBIOS does: read the console
        // data port and loop. On a blocking console it never gets past the read.
        let mut img = vec![0u8; 256_256];
        // A dense boot sector — see `bootable_image` for why the tail matters.
        // This device's sector is the front of the file, not offset 3.
        for (i, b) in img[..128].iter_mut().enumerate() {
            *b = BOOT_FILLER[i % BOOT_FILLER.len()];
        }
        // IN A,(01h) / JP 0000
        img[..5].copy_from_slice(&[0xDB, 0x01, 0xC3, 0x00, 0x00]);
        let mut m = BootMachine::new();
        m.set_machine("z80pack");
        m.insert(0, img, true).expect("a cpmsim image");
        let mut cpu = BootMachine::new_cpu();
        m.boot(&mut cpu, 0).expect("boots");

        let disks_before = m.disk_accesses();
        for _ in 0..10_000 {
            m.step(&mut cpu);
        }
        // The three signals the driver actually watches must all say "nothing
        // happened", which is what makes it nap.
        assert!(m.take_output().is_empty(), "a blocked guest prints nothing");
        assert_eq!(m.disk_accesses(), disks_before, "and touches no disk");
        assert_eq!(m.modem().rx_len(), 0, "and moves no modem bytes");
        // It is genuinely stuck on the read rather than having run away: the PC
        // is back at the `IN` every time.
        assert_eq!(cpu.registers().pc(), 0x0000, "the read is being replayed in place");
        // And the diagnostic counter reflects it, which is the other half.
        assert!(m.idle_status_reads() > 0, "a blocked read counts as an idle console read");

        // Give it a key and it must move on immediately — a guard that only
        // proved it blocks would be satisfied by a machine that never unblocks.
        m.send_key(b'X');
        m.step(&mut cpu);
        assert_ne!(cpu.registers().pc(), 0x0000, "a waiting key must let the read complete");
    }

    /// **A z80pack `cpmsim` disk boots, takes commands, and runs its software.**
    ///
    /// The gate for the fourth disk device and for the two machine features it
    /// needed: a DMA transfer straight into guest memory, and a console that
    /// *blocks*. Both are exercised on every keystroke here, and both fail
    /// visibly — a broken DMA gives no sign-on at all, and a console that answers
    /// a blocking read gives an endless stream of NULs after the prompt.
    ///
    /// TDISK03 is the case: Comal 80, on a disk whose system tracks say
    /// `Z80 CBIOS V1.2 for Z80SIM`. It is not a Tarbell disk at all, despite
    /// living with them and sharing their size.
    ///
    /// Ignored: set `CPM_Z80PACK_IMAGE` to TDISK03.DSK or a `cpmsim` library disk.
    #[test]
    #[ignore]
    fn test_a_z80pack_disk_boots_and_takes_commands() {
        let Ok(path) = std::env::var("CPM_Z80PACK_IMAGE") else {
            eprintln!("set CPM_Z80PACK_IMAGE to run this");
            return;
        };
        let mut m = BootMachine::new();
        m.set_machine("z80pack");
        m.insert(0, std::fs::read(&path).unwrap(), true).expect("a cpmsim image");
        let mut cpu = BootMachine::new_cpu();
        m.boot(&mut cpu, 0).expect("boots");

        let signon = printable(&run_until_quiet(&mut m, &mut cpu, 60_000_000));
        println!("--- sign-on ---\n{signon}");
        assert!(signon.contains("CP/M"), "no sign-on: {signon:?}");
        assert!(signon.contains("A>"), "no prompt: {signon:?}");
        // The failure this catches is specific and was real: a console that
        // answers a blocking read hands the CCP a NUL per instruction, so the
        // prompt is followed by thousands of them.
        let nuls = signon.matches('\0').count();
        assert!(nuls == 0, "the console answered a blocking read: {nuls} NULs after the prompt");

        // A command, so this cannot pass on a sign-on alone. Every keystroke
        // goes through the blocking-read replay, and `DIR` reads the directory
        // through the DMA path.
        let dir = type_at(&mut m, &mut cpu, b"DIR\r", 400_000_000);
        println!("--- DIR ---\n{dir}");
        assert!(dir.contains("COM"), "DIR listed no programs: {dir:?}");
        assert!(dir.contains("A>"), "DIR never returned to the prompt: {dir:?}");
    }

    /// Two keystrokes sent together must not both be readable at once.
    ///
    /// The regression guard for a measured defect. CDOS 2.58 reads the console
    /// data register twice per character — harmless on a serial line, where the
    /// second read finds nothing — so a console that hands over its whole queue
    /// as fast as the guest can ask swallows every other keystroke. `DIR` became
    /// `DR`; a burst of `ABCDEFGH` came back as `ACEG`.
    ///
    /// Driven through the ports rather than through a disk, so it holds for
    /// every machine and needs no image. See [`BootMachine::rx_ready`].
    #[test]
    fn test_a_second_keystroke_does_not_arrive_before_its_character_time() {
        let mut m = BootMachine::new();
        m.set_machine("cromemco");
        let status = m.console.status_port;
        let data = m.console.data_port;
        let ready = |m: &mut BootMachine| m.port_in(status as u16) & 0x40 != 0;

        m.send_key(b'A');
        m.send_key(b'B');
        assert!(ready(&mut m), "the first one is there straight away");
        assert_eq!(m.port_in(data as u16), b'A');
        assert!(!ready(&mut m), "and the second has not come down the line yet");
        // The lookahead read CDOS makes at this exact moment must find nothing,
        // not the next keystroke.
        assert_eq!(m.port_in(data as u16), 0, "a lookahead must not consume B");

        pass_a_character_time(&mut m);
        assert!(ready(&mut m), "after a character time it has arrived");
        assert_eq!(m.port_in(data as u16), b'B', "and it is still B, not lost");
    }

    /// Let one character time pass on the console's line.
    ///
    /// Time passes here by *running the CPU*, because instructions are the only
    /// clock this machine has — see [`RX_CHARACTER_TIME`]. A freshly made
    /// machine's memory is all zeroes, which the Z80 reads as NOPs, so a test
    /// that has not loaded a program can advance the clock without arranging
    /// anything for the CPU to do.
    fn pass_a_character_time(m: &mut BootMachine) {
        let mut cpu = BootMachine::new_cpu();
        for _ in 0..RX_CHARACTER_TIME {
            m.step(&mut cpu);
        }
    }

    /// **A Cromemco disk boots, signs on and takes a command.**
    ///
    /// The gate for the fourth and last board on the disk-controller plan, and
    /// for the two chip features it needed: sectors that are not 128 bytes, and
    /// multiple-record transfers. Both are exercised before the sign-on appears
    /// — the loader reads each track with a single command, and on a
    /// double-density disk every track after the first is 16 × 512.
    ///
    /// It also pins the boot-sector load address. `CDISK01` enters its operating
    /// system with a relative branch, so a sector loaded anywhere else lands in
    /// the wrong place and this test is what says so.
    ///
    /// Ignored: set `CPM_CROMEMCO_IMAGE` to CDISK01/02/03.DSK. `CPM_CROMEMCO_EXPECT`
    /// overrides the string looked for in the sign-on.
    #[test]
    #[ignore]
    fn test_a_cromemco_disk_boots_and_takes_commands() {
        let Ok(path) = std::env::var("CPM_CROMEMCO_IMAGE") else {
            eprintln!("set CPM_CROMEMCO_IMAGE to run this");
            return;
        };
        let mut m = BootMachine::new();
        m.set_machine("cromemco");
        m.insert(0, std::fs::read(&path).unwrap(), true).expect("a Cromemco image");
        let mut cpu = BootMachine::new_cpu();
        m.boot(&mut cpu, 0).expect("boots");

        let signon = printable(&run_until_quiet(&mut m, &mut cpu, 200_000_000));
        println!("--- sign-on ---\n{signon}");
        println!("pc={:#06x} stuck_polls={}", cpu.registers().pc(), m.stuck_polls());
        // Deliberately not a brand name. The three sample disks carry three
        // different operating systems — CDOS 2.58, MICAH 64k CP/M and ITC's
        // CP/M — and a gate that named one of them would only ever guard one
        // disk. `CPM_CROMEMCO_EXPECT` is there for pinning a specific image.
        let want = std::env::var("CPM_CROMEMCO_EXPECT").ok();
        match &want {
            Some(w) => assert!(signon.contains(w), "no sign-on containing {w:?}: {signon:?}"),
            None => assert!(
                signon.len() > 20,
                "the disk printed nothing worth calling a sign-on: {signon:?}"
            ),
        }

        // The real gate. A sign-on comes off the system tracks the loader
        // already read, so it can pass on a driver that never works again;
        // `DIR` is the first thing that goes back to the disk through the
        // guest's *own* BIOS, at a track the boot never touched.
        let dir = type_at(&mut m, &mut cpu, b"DIR\r", 400_000_000);
        println!("--- DIR ---\n{dir}");
        assert!(dir.contains("COM"), "DIR listed no programs: {dir:?}");
        // `DIR` is four keystrokes delivered at once, which is also the whole of
        // what a paste is. CDOS reads the data register twice per character, so
        // before the console modelled a character time this arrived as `DR` and
        // the guest answered "Program not found" — a passing sign-on and a
        // console that cannot be typed at. See `BootMachine::rx_ready`.
        assert!(
            !dir.contains("not found"),
            "the command arrived mangled — a keystroke was swallowed: {dir:?}"
        );
    }

    /// A VDM-1 guest is not mute because it failed — it is painting a screen we
    /// do not show yet.
    ///
    /// This exists so the deferred VDM-1 work is a *measured* gap rather than a
    /// mystery for whoever picks it up. TDISK04's CP/M assembles with
    /// `VDM EQU TRUE` and prints by storing bytes into the Processor Technology
    /// VDM-1's window at `CC00`, 64 columns by 16 lines, scrolling with the
    /// register on port `C8`. It never writes a console character to any port —
    /// verified by scanning its system tracks for `OUT 05h`, `OUT 01h` and
    /// `OUT 11h`, none of which appear. So with the right console it takes
    /// keystrokes perfectly and still shows nothing.
    ///
    /// Give it that console, run it, and its sign-on is sitting in screen
    /// memory. That is the whole of what remains: sample the window and paint it.
    ///
    /// Ignored: set `CPM_VDM_IMAGE` to TDISK04.DSK (or another VDM-1 disk).
    #[test]
    #[ignore]
    fn test_a_vdm_guest_writes_its_signon_into_screen_memory() {
        let Ok(path) = std::env::var("CPM_VDM_IMAGE") else {
            eprintln!("set CPM_VDM_IMAGE to run this");
            return;
        };
        /// The VDM-1's screen window, and its shape.
        const VDM_BASE: u16 = 0xCC00;
        const VDM_COLS: usize = 64;
        const VDM_ROWS: usize = 16;

        let bytes = std::fs::read(&path).unwrap();
        let mut m = BootMachine::new();
        // The keyboard half of the same board. Without it the guest never gets
        // past its first `CONIN` and never prints at all, so this test would
        // pass an empty screen for the wrong reason.
        m.set_machine("console_04");
        m.insert(0, bytes, true).expect("a bootable image");
        let mut cpu = BootMachine::new_cpu();
        m.boot(&mut cpu, 0).expect("boots");
        for _ in 0..20_000_000u64 {
            m.step(&mut cpu);
        }

        assert!(m.take_output().is_empty(), "a VDM-1 guest prints to no port at all");

        let mut screen = String::new();
        for row in 0..VDM_ROWS {
            let line: String = (0..VDM_COLS)
                .map(|col| {
                    // Bit 7 is the VDM-1's own business (inverse video); the
                    // character is the low seven bits.
                    let b = m.peek(VDM_BASE + (row * VDM_COLS + col) as u16) & 0x7F;
                    if (0x20..0x7F).contains(&b) { b as char } else { '.' }
                })
                .collect();
            screen.push_str(line.trim_end());
            screen.push('\n');
        }
        println!("--- VDM-1 screen at {VDM_BASE:#06x} ---\n{screen}");
        println!("pc={:#06x} (its CONIN loop, waiting for a key)", cpu.registers().pc());

        // `CPM`, not `CP/M`: the one disk this gate exists for signs on as
        // `TARBELL 48K CPM V1.4 OF 2-15-78`, with no slash. The default used to
        // be the spelling every *other* gate here looks for, which no VDM disk
        // ever paints — so running this test the documented way failed on a
        // guest that had worked perfectly, which is the most expensive kind of
        // wrong a test default can be.
        let want = std::env::var("CPM_VDM_EXPECT").unwrap_or_else(|_| "CPM".into());
        assert!(
            screen.contains(&want),
            "the guest never painted {want:?} into screen memory; it holds:\n{screen}"
        );
    }

    /// The end-to-end check the plan asked for: boot a real disk, run it, and
    /// see whether the guest's own operating system says anything.
    ///
    /// This is the strong oracle — it exercises the controller, the bootstrap,
    /// the CPU and the console together, and cannot be satisfied by a plausible
    /// wrong answer the way a "does this look like text" check can.
    ///
    /// Ignored: set `CPM_BOOT_IMAGE` to a `.dsk`.
    #[test]
    #[ignore]
    fn test_boot_a_real_disk_and_run_it() {
        let Ok(path) = std::env::var("CPM_BOOT_IMAGE") else {
            eprintln!("set CPM_BOOT_IMAGE to run this");
            return;
        };
        let bytes = std::fs::read(&path).unwrap();
        let mut m = BootMachine::new();
        if let Ok(k) = std::env::var("CPM_BOOT_MACHINE") {
            m.set_machine(&k);
            println!("(machine {k}: console {:#04x}/{:#04x})", m.console().status_port, m.console().data_port);
        }
        m.insert(0, bytes, true).expect("an 88-DCDD image");
        let mut cpu = BootMachine::new_cpu();
        match std::env::var("CPM_BOOT_STEP").ok().and_then(|s| s.parse::<u8>().ok()) {
            Some(step) => {
                m.boot_with_step(&mut cpu, 0, step).expect("boots");
                println!("(sector step {step}, forced)");
            }
            None => m.boot(&mut cpu, 0).expect("boots"),
        }

        // What the loader actually is, before watching it run.  Set
        // `CPM_BOOT_DISASM=addr:count` — the loaded boot code is the only
        // authority on what hardware it expects, and reading it is how the
        // three faults in the CP/M path were found.
        if let Ok(spec) = std::env::var("CPM_BOOT_DISASM") {
            let (addr, count) = spec.split_once(':').unwrap_or((spec.as_str(), "40"));
            let addr = u16::from_str_radix(addr.trim_start_matches("0x"), 16).unwrap();
            let count: usize = count.parse().unwrap();
            let saved = cpu.registers().pc();
            cpu.registers().set_pc(addr);
            for _ in 0..count {
                let at = cpu.registers().pc();
                println!("  {at:04x}  {}", cpu.disasm_instruction(&mut m));
            }
            cpu.registers().set_pc(saved);
        }

        // A short PC trace first: where a boot goes wrong is a control-flow
        // question, and the answer is always in the first hundred instructions.
        if std::env::var("CPM_BOOT_TRACE").is_ok() {
            let mut trace = Vec::new();
            for _ in 0..400u64 {
                trace.push(cpu.registers().pc());
                m.step(&mut cpu);
            }
            println!("first PCs: {:04x?}", &trace[..60.min(trace.len())]);
        }
        let mut out = Vec::new();
        for _ in 0..20_000_000u64 {
            m.step(&mut cpu);
            out.extend(m.take_output());
            if out.len() > 400 {
                break;
            }
        }
        let text: String = out
            .iter()
            .map(|&b| if (0x20..0x7F).contains(&b) || b == b'\n' { b as char } else { '.' })
            .collect();
        println!("--- {} ---\n{}", path, text);
        println!("port hits (0x80 bit = OUT): {:?}", m.port_hits);
        println!("mem[0..12] = {:02x?}", (0..12).map(|a| m.peek(a)).collect::<Vec<_>>());
        let st = m.port_in(0x08);
        println!("status={st:#04x}  track0 bit={}  moveok bit={}",
                 if st & 0x40 == 0 { "AT TRACK 0" } else { "not track 0" },
                 if st & 0x02 == 0 { "may move" } else { "busy" });
        println!(
            "pc={:#06x} stuck_polls={} idle_console={}",
            cpu.registers().pc(),
            m.stuck_polls(),
            m.idle_status_reads()
        );
        // The oracle the plan asked for: the guest's own operating system must
        // say who it is.  This cannot be satisfied by a plausible wrong answer
        // the way a "does this look like text" check can — the controller, the
        // bootstrap, the CPU and the console all have to work for a sign-on to
        // appear at all.
        let want = std::env::var("CPM_BOOT_EXPECT").unwrap_or_else(|_| "CP/M".into());
        assert!(
            text.contains(&want),
            "the guest never said {want:?}; it printed: {text:?}"
        );
    }

    /// Capture the *true* contents of files on an Altair floppy, by asking the
    /// disk's own operating system for them.
    ///
    /// This is the measurement the block-mapping work needs and never had. Every
    /// previous attempt scored a hypothesis against a heuristic — "do this
    /// assembler listing's addresses ascend?" — which cannot tell "nearly right"
    /// from "right", and 81% is exactly where such a score stops discriminating.
    /// A booted guest has no such problem: it reads the file with the BIOS that
    /// was written for this disk and sends it out byte for byte.
    ///
    /// Two drives, because the disk being *measured* must not be touched. Drive
    /// 0 is a tool disk carrying `PCPUT.COM` (DISK07 in the Altair-Duino set);
    /// drive 1 is the disk under study, mounted read-only, and files are named
    /// `B:NAME.EXT`.
    ///
    /// Ignored:
    ///   `CPM_TOOL_IMAGE=...DISK07.DSK`   boots, has PCPUT.COM
    ///   `CPM_DATA_IMAGE=...DISK01.DSK`   the disk to measure, drive B:
    ///   `CPM_GT_FILES=DEMO.PRN,PIP.COM`  files to pull off B:
    ///   `CPM_GT_DIR=/tmp/gt`             where to write them
    #[test]
    #[ignore]
    fn test_capture_altair_ground_truth() {
        let (Ok(tool), Ok(data), Ok(out)) = (
            std::env::var("CPM_TOOL_IMAGE"),
            std::env::var("CPM_DATA_IMAGE"),
            std::env::var("CPM_GT_DIR"),
        ) else {
            eprintln!("set CPM_TOOL_IMAGE, CPM_DATA_IMAGE and CPM_GT_DIR to run this");
            return;
        };
        let files = std::env::var("CPM_GT_FILES").unwrap_or_else(|_| "DEMO.PRN".into());
        std::fs::create_dir_all(&out).unwrap();

        let mut m = BootMachine::new();
        m.insert(0, std::fs::read(&tool).unwrap(), true).expect("an 88-DCDD tool image");
        m.insert(1, std::fs::read(&data).unwrap(), true).expect("an 88-DCDD data image");
        assert_eq!(
            m.attach_modem(crate::cpm::resolve_access("altair_2sio2")),
            ModemAttach::Ports(0x12, 0x13),
        );
        m.modem().set_carrier(true);

        let mut cpu = BootMachine::new_cpu();
        m.boot(&mut cpu, 0).expect("boots");
        let signon = printable(&run_until_quiet(&mut m, &mut cpu, 60_000_000));
        println!("--- sign-on ---\n{signon}");
        assert!(signon.contains("CP/M"), "no sign-on: {signon:?}");

        for name in files.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let cmd = format!("PCPUT B:{name} B\r");
            let ready = type_at(&mut m, &mut cpu, cmd.as_bytes(), 400_000_000);
            println!("--- PCPUT B:{name} ---\n{ready}");
            let (got, during) = xmodem_receive_from_guest(&mut m, &mut cpu, 4_000_000_000);
            let Some(got) = got else {
                panic!("the guest never sent {name}: {}", printable(&during));
            };
            let path = std::path::Path::new(&out).join(name);
            std::fs::write(&path, &got).unwrap();
            println!("{name}: {} bytes -> {}", got.len(), path.display());
            // Back to the prompt before the next one.
            run_until_quiet(&mut m, &mut cpu, 400_000_000);
        }
    }

    /// Drive a booted guest from the command line and hand back what its disks
    /// look like afterwards.
    ///
    /// A workbench rather than a check.  Answering "what does the disk's own
    /// software actually do here" is the technique that settled the Altair block
    /// mapping, and it needs a way to type at a guest and dump the result
    /// without a five-minute rebuild between each guess.
    ///
    /// Ignored:
    ///   `CPM_BOOT_IMAGE=...`   drive A:, boots, read-only
    ///   `CPM_BOOT_IMAGE2=...`  drive B:, WRITABLE — or `blank:<n>` for `n`
    ///                          bytes of nothing at all, which is what you want
    ///                          when the question is about formatting
    ///   `CPM_KEYS=FORMAT\r;B;Y`  typed in order, `;`-separated, output printed
    ///                          after each
    ///   `CPM_DUMP=/tmp/out.dsk`  where to write drive B: at the end
    #[test]
    #[ignore]
    fn test_drive_a_booted_guest() {
        let Ok(path) = std::env::var("CPM_BOOT_IMAGE") else {
            eprintln!("set CPM_BOOT_IMAGE to run this");
            return;
        };
        let mut m = BootMachine::new();
        // Read-only unless asked, like every other path here.  `CPM_BOOT_RW=1`
        // is what lets this workbench prove a *write*.
        let ro = std::env::var_os("CPM_BOOT_RW").is_none();
        m.insert(0, std::fs::read(&path).unwrap(), ro).expect("a bootable image");
        // Further drives, `,`-separated, filling units 1 upwards.  `blank:<n>`
        // for an unformatted one; an empty slot leaves that unit empty, so
        // `,,x.dsk` puts a disk in unit 3 and nothing in 1 or 2.
        let more = std::env::var("CPM_BOOT_IMAGES")
            .or_else(|_| std::env::var("CPM_BOOT_IMAGE2"))
            .unwrap_or_default();
        for (i, spec) in more.split(',').enumerate() {
            if spec.is_empty() {
                continue;
            }
            let bytes = match spec.strip_prefix("blank:") {
                Some(n) => vec![0u8; n.parse().expect("a byte count")],
                None => std::fs::read(spec).unwrap_or_else(|e| panic!("{spec}: {e}")),
            };
            m.insert(i as u8 + 1, bytes, false)
                .unwrap_or_else(|e| panic!("{spec}: {e}"));
        }
        let unit: u8 = std::env::var("CPM_BOOT_UNIT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let mut cpu = BootMachine::new_cpu();
        m.boot(&mut cpu, unit).expect("boots");
        println!(
            "--- sign-on (booted from unit {unit}) ---\n{}",
            printable(&run_until_quiet(&mut m, &mut cpu, 60_000_000))
        );

        for key in std::env::var("CPM_KEYS").unwrap_or_default().split(';') {
            if key.is_empty() {
                continue;
            }
            let keys = key.replace("\\r", "\r").replace("\\n", "\n");
            println!("--- typed {key:?} ---\n{}", type_at(&mut m, &mut cpu, keys.as_bytes(), 2_000_000_000));
        }
        if let Ok(dump) = std::env::var("CPM_DUMP") {
            match m.take_dirty().into_iter().next() {
                Some((_, bytes)) => {
                    std::fs::write(&dump, &bytes).unwrap();
                    println!("(wrote a dirty drive to {dump}, {} bytes)", bytes.len());
                }
                None => println!("(the guest wrote nothing to B:)"),
            }
        }
    }

    /// Make an Altair floppy **out of nothing** on the host — format and all —
    /// put a file on it, and have a real Altair CP/M read it.
    ///
    /// The last gap in host-side disk handling. `test_host_written_altair_
    /// floppy_is_read_by_the_guest` writes into a disk somebody else formatted;
    /// this one owns every byte of the image, so it also covers the sector
    /// headers, the sector IDs and the initial empty directory.
    ///
    /// A blank disk is the case where "looks fine" is worth nothing: a file
    /// full of `0xE5` mounts, lists as empty and accepts writes, and is refused
    /// by the first real BIOS that reads it because there is not one sector
    /// header on it. Only a guest can tell you the difference.
    ///
    /// Ignored: set `CPM_TOOL_IMAGE` to an Altair CP/M image carrying PCPUT.COM.
    #[test]
    #[ignore]
    fn test_a_blank_altair_floppy_made_here_works_in_the_guest() {
        use crate::cpm::image::format::by_token;
        use crate::cpm::image::fs::ImageFs;
        use crate::cpm::image::media::FileMedia;

        const EGT80_COM: &[u8] = include_bytes!("../../EGT80/EGT80.COM");

        let Ok(tool) = std::env::var("CPM_TOOL_IMAGE") else {
            eprintln!("set CPM_TOOL_IMAGE to run this");
            return;
        };

        // ---- a disk that did not exist a moment ago -----------------------
        let fmt = by_token("altair8").expect("altair8 is a format");
        let scratch = std::env::temp_dir().join("egw-altair-blank-test.dsk");
        std::fs::write(&scratch, fmt.blank_image().expect("a blank Altair")).unwrap();

        let mut fs =
            ImageFs::mount(Box::new(FileMedia::open(&scratch, false).unwrap()), fmt, false)
                .expect("our own blank mounts read-write");
        assert!(!fs.is_read_only(), "a fresh blank must not arrive damaged");
        assert!(fs.entries().is_empty(), "a fresh blank has no files on it");
        let mut name = [b' '; 8];
        name[..5].copy_from_slice(b"EGT80");
        let ext = *b"COM";
        fs.create(0, &name, &ext).unwrap();
        for (rec, chunk) in EGT80_COM.chunks(128).enumerate() {
            let mut buf = [0x1Au8; 128];
            buf[..chunk.len()].copy_from_slice(chunk);
            fs.write_record(0, &name, &ext, rec as u32, &buf).unwrap();
        }
        drop(fs);
        let made = std::fs::read(&scratch).unwrap();
        let _ = std::fs::remove_file(&scratch);

        // ---- now let a real Altair CP/M be the judge ----------------------
        let mut m = BootMachine::new();
        m.insert(0, std::fs::read(&tool).unwrap(), true).expect("an 88-DCDD tool image");
        m.insert(1, made, true).expect("what we made is an 88-DCDD image");
        assert_eq!(
            m.attach_modem(crate::cpm::resolve_access("altair_2sio2")),
            ModemAttach::Ports(0x12, 0x13),
        );
        m.modem().set_carrier(true);
        let mut cpu = BootMachine::new_cpu();
        m.boot(&mut cpu, 0).expect("boots");
        assert!(
            printable(&run_until_quiet(&mut m, &mut cpu, 60_000_000)).contains("CP/M"),
            "no sign-on"
        );

        let dir = type_at(&mut m, &mut cpu, b"DIR B:\r", 400_000_000);
        println!("--- DIR B: ---\n{dir}");
        assert!(
            dir.to_ascii_uppercase().contains("EGT80"),
            "the guest does not list the file on the disk we made: {dir:?}"
        );

        let ready = type_at(&mut m, &mut cpu, b"PCPUT B:EGT80.COM B\r", 400_000_000);
        println!("--- PCPUT ---\n{ready}");
        assert!(
            !ready.contains("Bad Sector"),
            "the guest's BIOS rejected a sector we formatted: {ready:?}"
        );
        let (got, during) = xmodem_receive_from_guest(&mut m, &mut cpu, 4_000_000_000);
        let got = got
            .unwrap_or_else(|| panic!("the guest never sent it back: {}", printable(&during)));
        assert_eq!(
            &got[..EGT80_COM.len()],
            EGT80_COM,
            "the file came back different from the one we put on our own disk"
        );
        println!(
            "a {} KB floppy formatted and filled on the host, read by the guest's own CP/M",
            337_568 / 1024
        );
    }

    /// Write an Altair floppy **from the host** and have the machine it was
    /// written for agree that it worked.
    ///
    /// This is the check that matters for host-side writing, and nothing weaker
    /// would do.  Our writer has to get three separate things right at once —
    /// the block mapping, the disk's stated `EXM 0`, and the per-sector
    /// checksum — and each of them fails *quietly* on its own: a wrong mapping
    /// A copy of `data_image` with EGT80 written into it by *our own* writer.
    ///
    /// Extracted because two gates need the same disk and only one of them used
    /// to make it. The other asked for `CPM_BOOT_IMAGE` to be "an Altair CP/M
    /// image carrying EGT80.COM" — an image nothing produces and nothing keeps,
    /// since the test that writes one deletes it again. So that gate could not
    /// be run at all, which is the quiet way a test stops being evidence: it is
    /// listed, it is never green, and nobody notices because `#[ignore]` hides
    /// both states equally.
    ///
    /// The payload is EGT80 because it is byte-exact, to hand, and a genuinely
    /// useful thing to have on one of these disks.
    #[cfg(test)]
    fn altair_floppy_carrying_egt80(data_image: &str) -> Vec<u8> {
        use crate::cpm::image::format::by_token;
        use crate::cpm::image::fs::ImageFs;
        use crate::cpm::image::media::FileMedia;

        // A scratch copy — the sample images are read-only ground truth and this
        // writes.  Named per-process so two gates running at once cannot write
        // the same file.
        let scratch = std::env::temp_dir()
            .join(format!("egw-altair-write-test-{}.dsk", std::process::id()));
        std::fs::write(&scratch, std::fs::read(data_image).unwrap()).unwrap();

        let fmt = by_token("altair8").expect("altair8 is a format again");
        let mut fs = ImageFs::mount(
            Box::new(FileMedia::open(&scratch, false).unwrap()),
            fmt,
            false,
        )
        .expect("mounts read-write");
        assert!(!fs.is_read_only(), "the image arrived in a writable state");
        let mut name = [b' '; 8];
        name[..5].copy_from_slice(b"EGT80");
        let ext = *b"COM";
        fs.create(0, &name, &ext).expect("creates the file");
        for (rec, chunk) in EGT80_COM.chunks(128).enumerate() {
            let mut buf = [0x1Au8; 128];
            buf[..chunk.len()].copy_from_slice(chunk);
            fs.write_record(0, &name, &ext, rec as u32, &buf).expect("writes a record");
        }
        drop(fs);
        let written = std::fs::read(&scratch).unwrap();
        let _ = std::fs::remove_file(&scratch);
        written
    }

    /// scrambles content that still looks like a file, a wrong EXM writes a
    /// directory entry CP/M declines to list, and a stale checksum turns into
    /// `Bdos Err On A: Bad Sector` only when a real BIOS reads the sector.
    /// Booting the disk afterwards catches all three, because the guest's own
    /// `DIR` and its own reader are the acceptance test.
    ///
    /// The payload is EGT80, which is a genuinely useful thing to put on one of
    /// these disks and is also byte-exact and to hand.
    ///
    /// Ignored:
    ///   `CPM_TOOL_IMAGE=...DISK07.DSK`   boots, has PCPUT.COM
    ///   `CPM_DATA_IMAGE=...DISK01.DSK`   written by us, then read as B:
    #[test]
    #[ignore]
    fn test_host_written_altair_floppy_is_read_by_the_guest() {
        let (Ok(tool), Ok(data)) = (
            std::env::var("CPM_TOOL_IMAGE"),
            std::env::var("CPM_DATA_IMAGE"),
        ) else {
            eprintln!("set CPM_TOOL_IMAGE and CPM_DATA_IMAGE to run this");
            return;
        };

        // ---- write it with our own writer, on the host --------------------
        let written = altair_floppy_carrying_egt80(&data);

        // ---- now let the machine it was written for judge it --------------
        let mut m = BootMachine::new();
        m.insert(0, std::fs::read(&tool).unwrap(), true).expect("an 88-DCDD tool image");
        m.insert(1, written, true).expect("our written image is still an 88-DCDD image");
        assert_eq!(
            m.attach_modem(crate::cpm::resolve_access("altair_2sio2")),
            ModemAttach::Ports(0x12, 0x13),
        );
        m.modem().set_carrier(true);
        let mut cpu = BootMachine::new_cpu();
        m.boot(&mut cpu, 0).expect("boots");
        assert!(
            printable(&run_until_quiet(&mut m, &mut cpu, 60_000_000)).contains("CP/M"),
            "no sign-on"
        );

        // The guest's own directory: this is what a wrong EXM fails.
        let dir = type_at(&mut m, &mut cpu, b"DIR B:\r", 400_000_000);
        println!("--- DIR B: ---\n{dir}");
        assert!(
            dir.to_ascii_uppercase().contains("EGT80"),
            "the guest does not list the file we wrote: {dir:?}"
        );

        // The guest's own reader: this is what a wrong checksum or a wrong
        // block mapping fails.
        let ready = type_at(&mut m, &mut cpu, b"PCPUT B:EGT80.COM B\r", 400_000_000);
        println!("--- PCPUT ---\n{ready}");
        assert!(
            !ready.contains("Bad Sector"),
            "the guest's BIOS rejected a sector we wrote: {ready:?}"
        );
        let (got, during) = xmodem_receive_from_guest(&mut m, &mut cpu, 4_000_000_000);
        let got = got.unwrap_or_else(|| panic!("the guest never sent it back: {}", printable(&during)));
        assert!(
            got.len() >= EGT80_COM.len(),
            "got {} bytes back, wrote {}",
            got.len(),
            EGT80_COM.len()
        );
        assert_eq!(
            &got[..EGT80_COM.len()],
            EGT80_COM,
            "what we wrote from the host is not what the guest reads back"
        );
        println!(
            "{} bytes written by the host, read back byte-identical by the guest's own CP/M",
            EGT80_COM.len()
        );
    }

    /// **What a booted guest actually does with each spelling of Backspace.**
    ///
    /// The measurement behind `telnet/cpm_boot_ui.rs`'s `boot_key_for_guest`.
    /// A modern client's Backspace key sends DEL (0x7F), and every operating
    /// system on these disks reads that as a Teletype *rubout*: it deletes the
    /// character and then prints the character it deleted, so backspacing over
    /// `TESTING` leaves `TESTINGGNIT` on the screen. Plain BS (0x08) is what
    /// they all erase on, answering with the universal `BS SPACE BS`.
    ///
    /// Measured, not reasoned, and on more than one guest on purpose — the
    /// three disks below are three different operating systems (Digital
    /// Research's BDOS and two MITS BASICs) and they agree, which is what makes
    /// the translation safe to apply to every booted disk rather than to a
    /// list of them.
    ///
    /// Ignored: set `CPM_BOOT_IMAGE` to a bootable image. Run it against
    /// several — `DISK01` (MITS CP/M 2.2), `DISK03` (Altair Disk Extended
    /// BASIC) and `HDSK01` (Altair Hard Disk BASIC) were the three used.
    #[test]
    #[ignore]
    fn test_a_booted_guest_erases_for_backspace_not_del() {
        let Ok(path) = std::env::var("CPM_BOOT_IMAGE") else {
            eprintln!("set CPM_BOOT_IMAGE to a bootable image");
            return;
        };
        let bytes = std::fs::read(&path).unwrap();

        // Type a word, then the key, and see what comes back for the key alone.
        let echo_of = |key: u8| -> Vec<u8> {
            let mut m = BootMachine::new();
            // Read-only: this asks the guest a question, it does not write.
            m.insert(0, bytes.clone(), true).expect("a bootable image");
            let mut cpu = BootMachine::new_cpu();
            m.boot(&mut cpu, 0).expect("boots");
            let signon = run_until_quiet(&mut m, &mut cpu, 200_000_000);
            assert!(!signon.is_empty(), "the disk never said anything");
            for &b in b"TESTING" {
                m.send_key(b);
            }
            let typed = run_until_quiet(&mut m, &mut cpu, 50_000_000);
            assert_eq!(typed, b"TESTING", "the guest is not echoing what we type");
            m.send_key(key);
            run_until_quiet(&mut m, &mut cpu, 50_000_000)
        };

        let bs = echo_of(0x08);
        let del = echo_of(0x7F);
        println!("BS 0x08 -> {bs:02X?}\nDEL 0x7F -> {del:02X?}");
        assert_eq!(
            bs, b"\x08 \x08",
            "BS must erase with the universal BS SPACE BS, got {bs:02X?}"
        );
        assert!(
            del.contains(&b'G'),
            "DEL is expected to echo the deleted character back — if this disk \
             does something else, boot_key_for_guest is worth re-reading: {del:02X?}"
        );
    }
}
