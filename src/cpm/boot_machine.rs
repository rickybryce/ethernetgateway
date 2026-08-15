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
//!   slot its drive letter names — a *drive* on the floppy controllers, a
//!   *platter* on the 88-HDSK, which carries four to a drive. It names them
//!   itself and reaches only as many as its own BIOS knows: stock Altair CP/M
//!   knows four drives, and the 88-HDSK CP/M uses the fixed platter as its B:.
//!   Folder-backed drives, the jail, `EXIT` and the
//!   Gateway Shell do not exist inside it.
//! * The blast radius is the images in the drives — narrower than the
//!   filesystem path's, and easier to state, but the per-file write claim that
//!   stops two sessions interleaving records has no meaning here. A booted
//!   image is therefore held by one session, and every disk in the machine is
//!   opened read-only unless the operator says otherwise — one answer for the
//!   whole machine, which is what a set of write-protect tabs is.
//! * Unknown ports read as `0xFF` (an idle bus) rather than as whatever was
//!   last driven, so a guest probing for hardware we do not have sees nothing
//!   instead of an echo of itself.

use super::boot::{cold_boot, Bootability, BootError};
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
/// [`BootMachine::bootable_media`] answers "could this file be booted *at all*",
/// which is a different question from "does the currently configured machine
/// carry it" — the generated readme wants the former. Built from the machine
/// list rather than written out, so a board reachable from no machine cannot
/// claim to be bootable.
///
/// There was a `medium_for(len)` here that answered the same question for one
/// file, and it was the boot picker's filter. It went with the picker in 0.9.2:
/// a *size* is not a boot program, so it offered every data disk in the
/// collection and every one of them failed when chosen. What replaced it asks
/// the configured machine to actually cold-start the image — see
/// `cpm::boot::image_can_boot`.
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
    /// Bytes the guest has written to the console.
    tx: Vec<u8>,
    /// Bytes the guest has written to the printer, if it has one.
    print: Vec<u8>,
    /// The printer's data register, when `cpm_printer_port` names one.
    ///
    /// `None` means the port is unclaimed and behaves exactly as it did before
    /// printing existed — offered to the modem and then discarded — so a
    /// gateway with printing off is byte-for-byte the machine it always was.
    printer_port: Option<u8>,
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
    /// The VDM-1's scroll register, as the guest last set it.
    ///
    /// Held here rather than in the display because it is machine state: the
    /// card's 1 KB window is guest memory we can sample at any moment, but
    /// which line is at the top exists only as the last byte written to port
    /// `C8h`, and nothing else would have seen it.  Latched unconditionally —
    /// on a machine with no VDM-1 this is a byte that stays zero, and the port
    /// was previously offered to the modem and discarded, so the guest cannot
    /// tell the difference.
    vdm_scroll_latch: u8,
    /// The Dazzler's address register, as the guest last wrote it: the on-bit
    /// and A15..A9 of its picture.
    ///
    /// `None` until the guest writes it, and that is load-bearing rather than
    /// tidy: while it is `None` the status port is *not* claimed, so a machine
    /// that has never seen a Dazzler answers `IN 0Eh` exactly as it did before
    /// this existed. A card the guest has not addressed is a card that is not
    /// in the machine.
    dazzler_address: Option<u8>,
    /// The Dazzler's format register.
    ///
    /// Held separately from the address because it is *animated*: GDEMO
    /// rewrites it twenty-nine times while running, so it is sampled with each
    /// frame rather than configured once.
    dazzler_format: u8,
    /// Instructions executed, which is this machine's only clock.
    ///
    /// Used for one thing: the Dazzler's end-of-frame bit, which software polls
    /// tens of millions of times and which has to *change* or the guest waits
    /// for ever. A real card is timed by a crystal; we have no wall clock on
    /// this path and would not want one, since a paused or slow session must
    /// still see frames go by in the order the guest expects.
    instructions: u64,
    /// Has the guest ever written the scroll register?
    ///
    /// The one honest, evidence-based answer to "is this a VDM-1 guest?" that
    /// costs nothing and infers nothing: a program that drives `C8h` is running
    /// a VDM-1 driver.  It is not used to *gate* anything — the screen can be
    /// sampled either way, because sampling is free and a program may paint the
    /// window without ever scrolling it — only to tell a viewer whether they
    /// are looking at a screen or at whatever happens to live at `CC00`.
    vdm_seen: bool,
    /// Bank switching, for the machines that have it.
    ///
    /// Idle until a guest allocates a bank, so every disk that does not bank
    /// pays one predictable branch per memory access and nothing else.
    mmu: super::mmu::Mmu,
    /// Does this machine carry an MMU at all?  Only z80pack's does, and a port
    /// claimed on a machine that has no such device would take it away from
    /// whatever else lives there.
    has_mmu: bool,
    /// Cromemco's bank select on port `40h`, and whether this machine has it.
    ///
    /// Its own field rather than a mode of the MMU: they are different boards
    /// on different machines with different registers, and the one thing they
    /// share — that memory access has to go through whichever is active — is
    /// handled by [`BootMachine::mem_read`] and [`BootMachine::mem_write`]
    /// instead.
    bank40: super::cromemco_bank::BankSelect,
    has_bank40: bool,
    /// Diagnostic: how many times each port was touched.
    #[cfg(test)]
    port_hits: std::collections::BTreeMap<u8, u64>,
    /// Diagnostic: the writes to each port, with their values.
    ///
    /// A count says a program drove a board; the *value* says how it was
    /// configured, and for a memory-mapped card that value is the only thing
    /// naming where in memory to look. Bounded so a runaway cannot grow it
    /// without limit — but generously, because the cap was 512 once and a
    /// chatty console filled it before the register under investigation was
    /// touched a second time, which reads as "the guest only wrote it once".
    #[cfg(test)]
    port_writes: Vec<(u8, u8)>,
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
            print: Vec::new(),
            printer_port: None,
            vdm_scroll_latch: 0,
            vdm_seen: false,
            dazzler_address: None,
            dazzler_format: 0,
            instructions: 0,
            mmu: super::mmu::Mmu::default(),
            has_mmu: false,
            bank40: super::cromemco_bank::BankSelect::default(),
            has_bank40: false,
            disk_accesses: 0,
            #[cfg(test)]
            port_hits: std::collections::BTreeMap::new(),
            #[cfg(test)]
            port_writes: Vec::new(),
        }
    }

    /// The CPU a booted disk runs on when nobody has said otherwise.
    ///
    /// The **default**, which is a Z80, and one place decides so the driver and
    /// the tests cannot disagree.  The Altair shipped with an 8080 and every
    /// MITS disk here is 8080 code, so an 8080 core is the more literal machine
    /// — but the Z80 is a superset that runs all of it, Altairs were very
    /// commonly fitted with Z80 upgrade boards, and the CP/M emulator next door
    /// is on the same setting.
    ///
    /// This used to be settled by our own terminal: `EGT8080.COM` is Z80 code,
    /// so on an 8080 core it loaded, executed a Z80-only opcode as something
    /// else, and took CP/M down with it — the sign-on came back corrupted on
    /// the warm boot.  `EGT8080.COM` runs on either processor and is placed
    /// beside it, so that argument is gone and the Z80 stays the default on
    /// the milder ones above.  An operator running period 8080 software says
    /// so with `cpm_cpu`, and [`BootMachine::new_cpu_for`] is how the session
    /// asks.
    ///
    /// Test-only now that the session passes the setting: a live boot must
    /// never quietly take the default when the operator has chosen otherwise,
    /// and the compiler is a better guarantee of that than a doc comment.
    #[cfg(test)]
    pub fn new_cpu() -> Cpu {
        Self::new_cpu_for(super::cpu::DEFAULT_CPU)
    }

    /// The CPU a booted disk runs on, as `cpm_cpu` names it.
    pub fn new_cpu_for(cpu_setting: &str) -> Cpu {
        super::cpu::new_cpu(cpu_setting)
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
        // The MMU is z80pack's, and it comes with z80pack's disk device: both
        // are parts of the same simulated machine, so the board set is what says
        // whether this machine can bank.  Claiming ports 14h-17h anywhere else
        // would take them from a real board that might want them.
        self.has_mmu = machine.boards.contains(&super::console::Board::Z80pack);
        // Cromemco's memory boards carry the bank select, so a machine with a
        // Cromemco disk controller is a machine that has one.  Gated, because a
        // guest on any other machine writing `40h` must go on seeing nothing
        // happen — a port this machine does not have is not this machine's.
        self.has_bank40 = machine.boards.contains(&super::console::Board::Cromemco);
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
        self.instructions = self.instructions.wrapping_add(1);
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
        //
        // **First match wins, and its refusal is final**: a later board that
        // would also have taken the image is never offered it. Unambiguous on
        // every machine that exists here — the Altair machines carry
        // DCDD/HDSK/Tarbell, whose media sizes are all distinct, and the two
        // boards that *do* share 256,256 bytes (Tarbell and z80pack) are never
        // on the same machine. A machine that put two boards claiming one size
        // together would need this to try the rest before giving up; noted
        // rather than built, because guessing which board an operator meant is
        // the sort of thing that wants a real case to be right about.
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

    /// **Would this image boot, if it were selected?** — and if not, whose fault.
    ///
    /// The two answers are not interchangeable and the callers must not have to
    /// work the difference out themselves. They did once, each repeating the
    /// same `Err(_) => cannot boot` classification, and all three agreed with
    /// each other and with the bug.
    ///
    /// The question the two boot lists need answered before they offer a disk,
    /// and the only honest way to answer it is to *do* the cold start: build the
    /// machine the operator configured, insert the image read-only, and run the
    /// bootstrap. Nothing here is a second opinion about bootability — it is the
    /// same sequence [`crate::telnet`]'s boot session runs, so it cannot reach a
    /// different verdict than the boot the operator is about to attempt.
    ///
    /// A cheaper rule was available and is deliberately not used: the picker
    /// filtered on [`BootMachine::medium_for`], which asks only whether some
    /// board could *carry* a disk that size. Every data disk in the Altair
    /// collection passes that — `DISK0B` is a normal 337,568-byte 8" image whose
    /// first sector holds a volume label and 112 zero bytes — so all four of them
    /// were offered and all four failed when chosen. A size is not a boot
    /// program.
    ///
    /// **This stops at the entry point and runs no guest code.** Whether the
    /// operating system then reaches a prompt is not decidable here at any price
    /// an interactive list can pay, so this is bounded honestly: it removes the
    /// disks that *cannot* boot, and does not promise that the rest will get
    /// somewhere useful.
    pub fn bootability(bytes: Vec<u8>, machine_setting: &str, cpu_setting: &str) -> Bootability {
        let (machine_key, _note) = super::detect::machine_for(machine_setting, &bytes);
        let mut m = BootMachine::new();
        m.set_machine(&machine_key);
        // Read-only, always: this is a question, and a question must not be able
        // to write to the operator's disk.  Nothing here reaches `take_dirty`
        // either, so there is no path from asking to a write-back.
        //
        // **`insert` failing IS the board mismatch**, and reading it as anything
        // else was a real defect: it refuses when no controller on this machine
        // accepts the image's size, and its message names the media the machine
        // does carry.  Mapping that to a "cannot boot" was enough to make a
        // perfectly good Altair disk vanish from every list under
        // `cpm_boot_machine = cromemco` — the exact outcome the split below
        // exists to prevent.
        if let Err(e) = m.insert(0, bytes, true) {
            return Bootability::NoBoardForIt(e);
        }
        let mut cpu = BootMachine::new_cpu_for(cpu_setting);
        match m.boot(&mut cpu, 0) {
            Ok(()) => Bootability::Boots,
            // This machine has no board that can start it — fixable by changing
            // `cpm_boot_machine`.
            Err(e @ (BootError::NoBootstrap | BootError::NoDisk(_))) => {
                Bootability::NoBoardForIt(e.to_string())
            }
            // The disk itself carries no boot program.  No configuration helps.
            Err(e) => Bootability::NoBootProgram(e.to_string()),
        }
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

    /// Take everything the guest has printed *to the console*.
    pub fn take_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.tx)
    }

    /// Give this machine a printer at `data_port`, or take it away with `None`.
    ///
    /// Set before the guest runs, from `cpm_printer_port` — see
    /// [`crate::cpm::printer::PORT_CHOICES`], where the Altair port is recorded
    /// with the measurement it came from.
    ///
    /// Only the data register. A real interface also has a status register the
    /// guest polls, and we deliberately do not answer it: an unclaimed port
    /// reads `0xFF` here, which every period convention reads as ready, and
    /// that is why Altair BASIC printed at full speed into a board that did not
    /// exist. Claiming the status port to say the same thing would be code that
    /// changes nothing.
    pub fn attach_printer(&mut self, data_port: Option<u8>) {
        self.printer_port = data_port;
    }

    /// Take everything the guest has printed *to the printer*.
    ///
    /// Kept apart from [`BootMachine::take_output`] because they are two
    /// devices: the console goes to the user's terminal and the printer goes to
    /// a file, and a single buffer would put a report on the screen and the
    /// user's typing in the document.
    pub fn take_print(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.print)
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

    /// The VDM-1 scroll register, for the display that samples this machine.
    pub fn vdm_scroll(&self) -> u8 {
        self.vdm_scroll_latch
    }

    /// Has this guest driven the VDM-1's scroll register?
    pub fn vdm_active(&self) -> bool {
        self.vdm_seen
    }

    /// Where this machine is in a Dazzler frame, `0.0..1.0`.
    ///
    /// **Instructions rather than seconds, and the rate is an assumption that
    /// is stated rather than measured** — nothing here can measure a 1976
    /// crystal. A 2 MHz 8080 averages a few cycles per instruction, so ~400,000
    /// instructions a second is the right order, and a 60 Hz frame is then
    /// about 6,700 of them. What the software actually depends on is that the
    /// bit *changes* at a plausible rate, which is the part a test can hold.
    fn dazzler_phase(&self) -> f32 {
        const FRAME_INSTRUCTIONS: u64 = 6_667;
        (self.instructions % FRAME_INSTRUCTIONS) as f32 / FRAME_INSTRUCTIONS as f32
    }

    /// Hand this machine's screen to a watching viewer, if there is one.
    ///
    /// Costs one relaxed atomic load when nobody is watching, which is what
    /// lets the driver call it at every key-poll seam — thousands of times a
    /// second — without the price showing up anywhere.
    ///
    /// The read goes through `peek`, so a banked guest is sampled through its
    /// MMU rather than out of the array behind it.  That distinction has
    /// already been a real defect on this machine once, in the DMA path, and
    /// the fix is the same one: everything that reads guest memory reads it the
    /// way the guest's own CPU would.
    /// Both cards travel together because they are one picture of one machine
    /// at one instant, and a guest can have both — TDISK04 has a VDM-1 for its
    /// console and runs `KSCOPE`, which drives a Dazzler.
    pub fn publish_screen(&mut self, screen: &super::screen::Screen) {
        if !screen.wanted() {
            return;
        }
        let mut window = Box::new([0u8; super::vdm::WINDOW]);
        for (i, b) in window.iter_mut().enumerate() {
            *b = self.peek(super::vdm::BASE.wrapping_add(i as u16));
        }
        let vdm = super::screen::VdmPart {
            window,
            scroll: self.vdm_scroll(),
            active: self.vdm_active(),
        };

        // Only when the guest has switched one on.  An addressed-but-off card
        // is not a black picture, it is no picture, and saying so lets the
        // viewer tell "this machine has no colour card" from "the card is
        // showing black" — which cannot be told apart by looking.
        let dazzler = self.dazzler_address.filter(|a| super::dazzler::is_on(*a)).map(|address| {
            let format = super::dazzler::Format::from_byte(self.dazzler_format);
            let base = super::dazzler::base(address);
            let bytes = (0..format.bytes())
                .map(|i| self.peek(base.wrapping_add(i as u16)))
                .collect();
            super::screen::DazzlerPart { bytes, address, format: self.dazzler_format }
        });

        screen.publish(vdm, dazzler);
    }

    /// Accesses to the disk controller's ports since the machine was made.
    pub fn disk_accesses(&self) -> u64 {
        self.disk_accesses
    }

    /// Position-register reads without the disk moving on.
    pub fn stuck_polls(&self) -> u32 {
        self.controllers.iter().map(|c| c.stuck_polls()).max().unwrap_or(0)
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

    /// Which board on the machine `key` names would take an image this size,
    /// as `(board name, its word for a slot)`.
    ///
    /// `None` if no board on that machine takes it — which is a real answer and
    /// not an error: a hard-disk image on a Cromemco is nobody's disk.
    ///
    /// **The board is chosen by the image's size, not by the slot it is going
    /// into**, exactly as [`BootMachine::insert`] does it — same iteration, same
    /// first-match rule — because the whole point of this function is to say in
    /// advance what `insert` is about to do. That is the fact the disk screens
    /// could not previously show: mount a floppy while booting a hard disk and
    /// it lands on the 88-DCDD *successfully*, while the guest is driving the
    /// 88-HDSK and never looks there.
    /// `key` names the machine; `None` asks every board this gateway has.
    ///
    /// `None` is for the configuration screens, which name slots before any
    /// machine exists and must not read a 4.9 MB boot image to draw a row.
    ///
    /// **It IS a weaker answer, and this used to claim otherwise.** The rule
    /// that no two media may claim overlapping sizes belongs to
    /// [`crate::cpm::image::format`] — the *mountable* format table — and not
    /// here: 256,256 bytes is an IBM 3740 to the Tarbell and an 8" SSSD to
    /// z80pack, which is exactly why [`super::console::MachineChoice::boards`]
    /// exists. With `None` the answer is whichever board `MACHINE_CHOICES`
    /// lists first, so for that one size a config screen can name a board the
    /// running machine does not carry.
    ///
    /// Pass the machine wherever it is known. It was `None` on both sides of
    /// the boot screen's mismatch warning, which made it announce that a
    /// Cromemco single-density disk was "on the Tarbell 1011, not the booted
    /// disk's board" — about a disk the guest reads perfectly.
    pub fn board_for(key: Option<&str>, image_len: u64) -> Option<&'static str> {
        let all;
        let boards: &[Box<dyn Controller>] = match key {
            Some(k) => {
                all = super::console::resolve_machine(k)
                    .boards
                    .iter()
                    .map(|b| boards_to_controller(*b))
                    .collect::<Vec<_>>();
                &all
            }
            None => {
                all = all_controllers();
                &all
            }
        };
        boards
            .iter()
            .find(|c| c.accepts(image_len).is_some())
            .map(|c| c.name())
    }

    /// What the board taking an image this size calls slot `slot`.
    ///
    /// Separate from [`BootMachine::board_for`] rather than another field of
    /// its tuple because a label is *computed* — the 88-HDSK's names a drive
    /// and a platter — so it cannot be a `&'static str`.
    pub fn slot_label(key: Option<&str>, image_len: u64, slot: u8) -> Option<String> {
        let all;
        let boards: &[Box<dyn Controller>] = match key {
            Some(k) => {
                all = super::console::resolve_machine(k)
                    .boards
                    .iter()
                    .map(|b| boards_to_controller(*b))
                    .collect::<Vec<_>>();
                &all
            }
            None => {
                all = all_controllers();
                &all
            }
        };
        boards.iter().find(|c| c.accepts(image_len).is_some()).map(|c| c.slot_label(slot))
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
                    // Both ends bounded, and the buffer end is the one that was
                    // missing. `buf[..len]` was the only unguarded index in this
                    // function: a controller reporting a length longer than its
                    // own buffer would panic the session, and the two sides
                    // agree today only by a coincidence of constants — the
                    // WD1771 clamps its sector length to what the chip can
                    // buffer (512) and Cromemco's largest sector happens to be
                    // 512 too, so the clamp sits on one side of the pair only.
                    //
                    // A short buffer refuses the write rather than writing what
                    // there is. The read paths answer an impossible request with
                    // an erased sector because a real drive returns *something*
                    // from unformatted media; there is no equivalent courtesy
                    // for a write, where a partial sector is worse than none —
                    // it is a sector the guest believes it wrote.
                    let (Some(dst), Some(src)) = (m.bytes.get_mut(off..off + len), buf.get(..len))
                    else {
                        return;
                    };
                    dst.copy_from_slice(src);
                    m.dirty = true;
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
                        //
                        // **Through the MMU**, exactly as the CPU's own writes
                        // go.  z80pack keeps a separate `dma_write` for this and
                        // it honours the bank mapping; writing straight into
                        // bank 0 instead puts every sector where a banked guest
                        // is not looking.  CP/M 3 then reads its own directory
                        // as empty and retries the same sector for ever --
                        // measured, 1,677 status polls and not one console read.
                        let at = addr.wrapping_add(i as u16);
                        self.mem_write(at, *b);
                    }
                } else {
                    // Reads come out of the same mapping for the same reason:
                    // a banked guest writing a sector expects the bytes it can
                    // see to be the bytes that reach the disk.
                    let bytes: Vec<u8> = (0..len)
                        .map(|i| {
                            self.mem_read(addr.wrapping_add(i as u16))
                        })
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
        // The MMU, on the machine that has one.  Nothing in `uart.rs` lands on
        // 14h-17h today, so this is not reachable -- but the console case above
        // exists because a modem that is *present in the config and mute in the
        // machine* is the worst way for this to fail, and the MMU would shadow
        // one exactly the same way.
        if self.has_mmu && super::mmu::Mmu::owns_port(port) {
            return Some("the memory-bank controller");
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


impl BootMachine {
    /// Read guest memory the way this machine's own CPU would.
    ///
    /// **Every path that touches guest memory goes through here**, and that is
    /// the point of it existing. Two machines bank memory by different boards —
    /// z80pack's MMU on `14h`-`17h` and Cromemco's select on `40h` — and the
    /// last time a second path indexed the array directly, a banked CP/M 3 read
    /// its own directory as empty because the disk's DMA wrote where the guest
    /// was not looking. One function, so a third banking scheme cannot miss a
    /// caller.
    ///
    /// The common case is one comparison and the array index it always was.
    fn mem_read(&mut self, address: u16) -> u8 {
        if !self.mmu.is_idle() {
            return self.mmu.read(&self.mem, address);
        }
        if !self.bank40.is_idle() {
            return self.bank40.read(&self.mem, address);
        }
        self.mem[address as usize]
    }

    /// Write guest memory the way this machine's own CPU would. See
    /// [`BootMachine::mem_read`].
    fn mem_write(&mut self, address: u16, value: u8) {
        if !self.mmu.is_idle() {
            self.mmu.write(&mut self.mem, address, value);
            return;
        }
        if !self.bank40.is_idle() {
            self.bank40.write(&mut self.mem, address, value);
            return;
        }
        self.mem[address as usize] = value;
    }
}

impl Machine for BootMachine {
    fn peek(&mut self, address: u16) -> u8 {
        self.mem_read(address)
    }

    fn poke(&mut self, address: u16, value: u8) {
        self.mem_write(address, value);
    }

    fn port_in(&mut self, address: u16) -> u8 {
        let port = address as u8;
        #[cfg(test)]
        {
            *self.port_hits.entry(port).or_insert(0) += 1;
        }
        match port {
            // The MMU before the boards: its ports are its own, and only on a
            // machine that has one.
            p if self.has_mmu && super::mmu::Mmu::owns_port(p) => self.mmu.port_in(p),
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
            // The Dazzler's status, and **only once a guest has addressed one**
            // — until then this port is not ours and falls through to the
            // answer it has always given, so no existing machine changes.
            //
            // This exists because GDEMO reads it **58.8 million times** waiting
            // for the end-of-frame bit (measured, not supposed). An unclaimed
            // port answers 0xFF here, which holds that bit permanently high and
            // means "a frame is never over": the guest polls for ever and looks
            // like a hang. That is the floating-sense-switch mistake exactly —
            // 0xFF is a *reading*, not the absence of one.
            super::dazzler::ADDRESS_PORT if self.dazzler_address.is_some() => {
                // The line parity alternates far faster than the frame does;
                // deriving both from the one clock keeps them consistent.
                let even_line = (self.instructions / 13).is_multiple_of(2);
                super::dazzler::status(self.dazzler_phase(), even_line)
            }
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
            if self.port_writes.len() < 200_000 {
                self.port_writes.push((port, value));
            }
        }
        match port {
            p if self.has_mmu && super::mmu::Mmu::owns_port(p) => {
                self.mmu.port_out(p, value);
            }
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
            // The printer's data register, if this machine has a printer.
            //
            // Checked after the disk controllers and the console but before the
            // modem, because those two are hardware the guest's own BIOS
            // depends on and the printer is a port we have volunteered to
            // claim: a profile that collided with a real device would break the
            // guest, so the devices that make the machine work go first.
            p if Some(p) == self.printer_port => {
                self.print.push(value);
                self.idle_status_reads = 0;
            }
            // The VDM-1's scroll register.  Write-only, like the card: an `IN`
            // here still falls through below, because a real VDM-1 answers
            // nothing and a guest that reads a port we have started claiming
            // must see exactly what it saw before.
            //
            // After the printer for the same reason the printer is after the
            // console: a port the operator has assigned to a device the guest
            // is really using must not be taken away by a display the guest
            // cannot even detect.  Deliberately *not* counted as console
            // activity — painting a memory-mapped screen produces no console
            // bytes at all, which is exactly why a VDM-1 guest looks idle, and
            // the pacing is right about that.
            super::vdm::SCROLL_PORT => {
                self.vdm_scroll_latch = value;
                self.vdm_seen = true;
            }
            // The Cromemco Dazzler's two registers.  **Last of the displays and
            // after the controllers on purpose**: `0Eh` is also a z80pack disk
            // register, and a machine that really has that board must keep it —
            // a colour card the guest never asked for must never cost it a
            // disk.  `controller_for` above has already had its say.
            //
            // Not console activity, for the VDM-1's reason: a card painted by
            // memory writes produces no console bytes, and the pacing is right
            // that such a guest looks idle.
            // Cromemco's bank select.  After the disk controller, like every
            // display here, so a board the operator's machine really has keeps
            // its port; nothing else claims 40h today, but the ordering is the
            // rule rather than the coincidence.  Gated on the machine having
            // Cromemco boards: an Altair guest writing 40h must go on seeing
            // nothing happen.
            super::cromemco_bank::PORT if self.has_bank40 => {
                self.bank40.port_out(value);
            }
            super::dazzler::ADDRESS_PORT => {
                self.dazzler_address = Some(value);
            }
            super::dazzler::FORMAT_PORT => {
                self.dazzler_format = value;
                // Addressing is what puts the card in the machine, but a guest
                // that sets a format first has still declared one, and leaving
                // it out would lose the format of a picture switched on next.
                self.dazzler_address.get_or_insert(0);
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

    /// The EGT8080 binary, for the gates that put it on a disk and then check what
    /// came back. Module-level so the helper that writes it and the assertions
    /// that compare against it cannot end up looking at different bytes.
    const EGT8080_COM: &[u8] = include_bytes!("../../EGT8080/EGT8080.COM");

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
        /// Handed back by the next `port_out`, so a test can make the board ask
        /// the machine for something specific — including something impossible,
        /// which is the only way to reach the machine's own bounds checks.
        next_request: Option<HostRequest>,
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
            self.next_request.take().unwrap_or(HostRequest::None)
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

    /// The VDM-1's one register, and the one thing about it that is easy to get
    /// wrong: it is an output-only latch.  Claiming a port for a display the
    /// guest cannot detect must not change what the guest reads there, or a
    /// machine gains a device that answers `0x00` where it used to find an
    /// empty bus.
    #[test]
    fn test_the_vdm_scroll_register_is_a_write_only_latch() {
        use crate::cpm::vdm;
        let mut m = BootMachine::new();
        assert_eq!(m.vdm_scroll(), 0);
        assert!(!m.vdm_active(), "nothing has driven the card yet");

        let before = m.port_in(vdm::SCROLL_PORT as u16);
        m.port_out(vdm::SCROLL_PORT as u16, 0x0C);
        assert_eq!(m.vdm_scroll(), 0x0C);
        assert!(m.vdm_active(), "a guest that writes C8h is running a VDM-1 driver");
        assert_eq!(
            m.port_in(vdm::SCROLL_PORT as u16),
            before,
            "a real VDM-1 drives nothing onto the bus for an IN, so neither do we"
        );
        // And it is not console activity: a card that is painted by memory
        // writes produces no console bytes, which is exactly why a VDM-1 guest
        // looks idle to the driver — and the driver is right about that.
        assert!(m.take_output().is_empty());
    }

    /// The whole path from the guest's memory to a viewer's frame, in one test:
    /// the guest paints, sets its scroll register, and the screen that comes out
    /// the other end is rotated the way the card would show it.
    ///
    /// Also pins the "nobody is watching costs nothing" contract, which is the
    /// only reason this can run on every booted session unconditionally.
    #[test]
    fn test_a_machine_publishes_the_screen_its_guest_painted() {
        use crate::cpm::{screen, vdm};
        let mut m = BootMachine::new();
        for (i, b) in b"HELLO".iter().enumerate() {
            m.poke(vdm::BASE + i as u16, *b);
        }
        // Line 1 at the top, so what the guest wrote on line 0 comes round to
        // the bottom of the display.
        m.port_out(vdm::SCROLL_PORT as u16, 1);

        let screen = screen::register("boot_machine unit test");
        m.publish_screen(&screen);
        assert!(
            matches!(screen::look(screen.id()), screen::Look::Waiting { .. }),
            "nobody had asked, so nothing was sampled"
        );

        // That `look` was a viewer asking, so the next seam publishes.
        m.publish_screen(&screen);
        let screen::Look::Frame(snap) = screen::look(screen.id()) else {
            panic!("a frame was published");
        };
        assert!(snap.vdm.active);
        assert_eq!(snap.vdm.scroll, 1);
        assert!(snap.dazzler.is_none(), "no guest here has addressed a Dazzler");
        let text = vdm::frame_text(&vdm::frame(&snap.vdm.window, snap.vdm.scroll));
        assert_eq!(text[vdm::ROWS - 1].trim_end(), "HELLO");
        assert!(text[0].trim().is_empty(), "line 1 is at the top and it is blank");
    }

    /// **A Dazzler must not cost a machine its disk controller.** `0Eh` is the
    /// card's address register and also a z80pack disk register, and the guest
    /// that has one of those is not asking for the other. The board is matched
    /// first; this pins that ordering, because the failure it prevents — a
    /// machine that boots and then cannot read a sector — looks nothing like a
    /// graphics card being wrong.
    #[test]
    fn test_a_disk_controller_keeps_the_port_the_dazzler_would_take() {
        use crate::cpm::dazzler;
        let mut m = BootMachine::new();
        m.set_machine("z80pack");
        assert!(
            m.owns_disk_port(dazzler::ADDRESS_PORT),
            "this machine's board really does answer there"
        );
        m.port_out(dazzler::ADDRESS_PORT as u16, 0x81);
        let screen = crate::cpm::screen::register("dazzler port collision");
        // A viewer arrives, then the seam publishes: one request, one snapshot.
        let _ = crate::cpm::screen::look(screen.id());
        m.publish_screen(&screen);
        let crate::cpm::screen::Look::Frame(snap) = crate::cpm::screen::look(screen.id()) else {
            panic!("published")
        };
        assert!(snap.dazzler.is_none(), "the write went to the disk, where it belongs");
    }

    /// The status port is **not claimed until a guest addresses a card**, so a
    /// machine that has never seen a Dazzler answers exactly as it always did.
    /// Adding a device that changes what every other guest reads on a line it
    /// never asked about is how an emulator breaks working software.
    #[test]
    fn test_the_dazzler_status_port_is_silent_until_a_guest_addresses_one() {
        use crate::cpm::dazzler;
        let mut m = BootMachine::new();
        let before = m.port_in(dazzler::ADDRESS_PORT as u16);
        assert_eq!(before, 0xFF, "an unclaimed port on this machine");

        m.port_out(dazzler::ADDRESS_PORT as u16, 0x81);
        // Now it answers, and — the whole point — the end-of-frame bit is not
        // stuck. GDEMO polls this 58.8 million times; a constant reading is a
        // guest that waits for ever.
        let mut cpu = BootMachine::new_cpu();
        let mut seen_low = false;
        let mut seen_high = false;
        for _ in 0..20_000u64 {
            m.step(&mut cpu);
            if m.port_in(dazzler::ADDRESS_PORT as u16) & 0x40 == 0 {
                seen_low = true;
            } else {
                seen_high = true;
            }
        }
        assert!(seen_low && seen_high, "the end-of-frame bit has to change, or a guest hangs");
    }

    /// The picture is sampled from wherever the guest put it, at the size its
    /// format asks for — the two registers together, because either alone
    /// describes a different picture.
    #[test]
    fn test_a_machine_publishes_the_picture_its_guest_addressed() {
        use crate::cpm::{dazzler, screen};
        let mut m = BootMachine::new();
        // KSCOPE's own settings, measured: on at 0200, 64x64 colour in 2K.
        m.port_out(dazzler::FORMAT_PORT as u16, 0x30);
        m.port_out(dazzler::ADDRESS_PORT as u16, 0x81);
        m.poke(0x0200, 0x21);

        let s = screen::register("dazzler publish");
        let _ = screen::look(s.id());
        m.publish_screen(&s);
        let screen::Look::Frame(snap) = screen::look(s.id()) else { panic!("published") };
        let d = snap.dazzler.expect("the guest switched one on");
        assert_eq!(d.bytes.len(), dazzler::LARGE, "2K, because the format says so");
        assert_eq!(d.bytes[0], 0x21, "read from 0200, where the address register points");

        let pic = dazzler::frame(&d.bytes, dazzler::Format::from_byte(d.format));
        assert_eq!((pic.width, pic.height), (64, 64));
        assert_eq!((pic.cells[0], pic.cells[1]), (1, 2));
    }

    /// Switched off is not black: the card leaves the snapshot entirely, so a
    /// viewer can tell "no colour card here" from "the card is showing black".
    #[test]
    fn test_a_dazzler_switched_off_is_absent_rather_than_black() {
        use crate::cpm::{dazzler, screen};
        let mut m = BootMachine::new();
        m.port_out(dazzler::ADDRESS_PORT as u16, 0x81);
        let s = screen::register("dazzler off");
        let _ = screen::look(s.id());
        m.publish_screen(&s);
        let screen::Look::Frame(on) = screen::look(s.id()) else { panic!("published") };
        assert!(on.dazzler.is_some());

        m.port_out(dazzler::ADDRESS_PORT as u16, 0x01); // same address, on-bit clear
        m.publish_screen(&s);
        let screen::Look::Frame(off) = screen::look(s.id()) else { panic!("published") };
        assert!(off.dazzler.is_none());

        // But the status port stays claimed — the card is still in the machine,
        // and a guest that switches it off and on must not lose it in between.
        //
        // Checked by *stepping*, not by reading once: a mid-frame reading on an
        // even line is `0xFF`, exactly what an unclaimed port answers, so a
        // single read cannot tell a live card from an empty bus. Only the bit
        // changing proves anybody is home. Worth knowing before writing the
        // obvious `assert_ne!(…, 0xFF)`, which passes and proves nothing.
        let mut cpu = BootMachine::new_cpu();
        let mut seen_low = false;
        for _ in 0..20_000u64 {
            m.step(&mut cpu);
            if m.port_in(dazzler::ADDRESS_PORT as u16) & 0x40 == 0 {
                seen_low = true;
                break;
            }
        }
        assert!(seen_low, "the card still answers even with its picture switched off");
    }

    /// **Cromemco's bank select, on the machine that has one.**
    ///
    /// A bitmap, not a bank number — bit 1 is bank 1 — and each bank is its own
    /// 64 KB. Measured on the guest that needs it: Cromix writes `40h` before
    /// it does anything else and could not open its console until this existed.
    #[test]
    fn test_a_cromemco_machine_banks_its_memory() {
        use crate::cpm::cromemco_bank::PORT;
        let mut m = BootMachine::new();
        m.set_machine("cromemco");
        m.poke(0x2000, 0xAA);

        m.port_out(PORT as u16, 0x02); // bit 1 = bank 1
        assert_eq!(m.peek(0x2000), 0, "a fresh bank does not see bank 0");
        m.poke(0x2000, 0xBB);

        m.port_out(PORT as u16, 0x01); // bit 0 = bank 0, the power-up bank
        assert_eq!(m.peek(0x2000), 0xAA, "bank 0 kept its own byte");
        m.port_out(PORT as u16, 0x02);
        assert_eq!(m.peek(0x2000), 0xBB, "and bank 1 kept its own");
    }

    /// **A machine without Cromemco boards does not have the port.**
    ///
    /// The gate that matters: an Altair guest writing `40h` — for whatever
    /// reason of its own — must go on seeing exactly nothing happen. A device
    /// added to every machine is a device that can break the ones that were
    /// working.
    #[test]
    fn test_only_a_cromemco_machine_has_the_bank_select() {
        use crate::cpm::cromemco_bank::PORT;
        let mut m = BootMachine::new(); // the default is an Altair
        m.poke(0x2000, 0xAA);
        m.port_out(PORT as u16, 0x02);
        assert_eq!(m.peek(0x2000), 0xAA, "no banking here: the write went nowhere");
        m.poke(0x2000, 0xBB);
        m.port_out(PORT as u16, 0x01);
        assert_eq!(m.peek(0x2000), 0xBB, "still the one flat memory it always was");
    }

    /// **The port is write-only, like the card.**
    ///
    /// The manual describes eight bits *output* and says nothing about reading,
    /// so an `IN` must answer exactly what it answered before this device
    /// existed.
    #[test]
    fn test_the_bank_select_is_write_only() {
        use crate::cpm::cromemco_bank::PORT;
        let mut m = BootMachine::new();
        m.set_machine("cromemco");
        let before = m.port_in(PORT as u16);
        m.port_out(PORT as u16, 0x04);
        assert_eq!(m.port_in(PORT as u16), before, "a real card drives nothing onto the bus");
    }

    /// **Everything that reaches guest memory goes through the same door.**
    ///
    /// `mem_read`/`mem_write` exist so a second banking scheme cannot miss a
    /// caller, and the caller that was missed last time was the disk's DMA:
    /// with the z80pack MMU implemented but DMA writing bank 0 directly, a
    /// banked CP/M 3 read its own directory as empty. This asserts the source
    /// has one path rather than two, which is the only way to check it without
    /// a controller that does DMA on a Cromemco machine — a combination no real
    /// disk here produces.
    #[test]
    fn test_guest_memory_has_exactly_one_access_path() {
        // **Line endings normalised first.** A Windows checkout has CRLF, so a
        // pattern containing a bare `\n` finds nothing and the bound below
        // panics — which is precisely how this failed on the Windows CI job and
        // nowhere else. Any source-scanning test that spans a line break has to
        // do this.
        let src = include_str!("boot_machine.rs").replace("\r\n", "\n");
        // Bounded at the test module, or this matches its own pattern strings
        // and fails on itself — the `pkill -f` mistake, in a test.
        // Proof the normalisation does its job, run here because the platform
        // that needs it is one this test cannot execute on. Without the
        // replace, this find returns None and the bound below panics.
        let crlf = "impl Machine for BootMachine\r\nx\r\n#[cfg(test)]\r\nmod tests";
        assert!(crlf.find("\n#[cfg(test)]\nmod tests").is_none(), "CRLF really does defeat it");
        assert!(crlf.replace("\r\n", "\n").find("\n#[cfg(test)]\nmod tests").is_some());

        let start = src.find("impl Machine for BootMachine").expect("the impl");
        let end = src.find("\n#[cfg(test)]\nmod tests").expect("the test module");
        let body = &src[start..end];
        // Inside the trait impl and everything after it — the DMA service loop
        // included — nothing indexes the memory array directly.
        for pattern in ["self.mem[at as usize]", "self.mem[address as usize]"] {
            assert!(
                !body.contains(pattern),
                "{pattern} bypasses mem_read/mem_write — the CP/M 3 DMA defect, again"
            );
        }
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
            next_request: None,
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

    /// **A controller asking to write more than it is holding must not panic
    /// the session**, and must not write a partial sector either.
    ///
    /// The machine sliced the controller's buffer to the length the controller
    /// reported — `buf[..len]` — which was the only unguarded index in
    /// `service`. Every other bound there is checked and the out-of-range case
    /// has a stated policy. The two sides agree today by a coincidence of
    /// constants: `Wd1771::sector_out` clamps its sector length to
    /// `MAX_SECTOR_LEN` (512) while `Cromemco::serve` computes the length from
    /// its own geometry table, and Cromemco's largest sector is also 512. The
    /// clamp is on one side of the pair only, and its own comment shows the risk
    /// was seen in the chip and not followed out to the machine.
    ///
    /// Reached through a stand-in board because no real one can currently ask
    /// this — which is the point: the coupling is implicit, so the guard is what
    /// makes it explicit.
    #[test]
    fn test_a_write_longer_than_the_controllers_buffer_is_refused() {
        let mut m = BootMachine::new();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        m.controllers.push(Box::new(FakeBoard {
            last_write: seen.clone(),
            // Four bytes held, half a sector claimed.
            buf: vec![0xAA; 4],
            inserted: None,
            next_request: Some(HostRequest::Write { drive: 1, offset: 0, len: 512 }),
        }));

        let img = vec![0x11u8; FAKE_IMAGE_LEN as usize];
        m.insert(1, img, false).expect("the second controller takes it");

        // The request is served here, and must come back rather than unwind.
        m.port_out(0xB3, 0x01);

        let disk = m.disks[1].as_ref().expect("still inserted");
        assert!(
            disk.bytes.iter().all(|&b| b == 0x11),
            "a short buffer must write nothing, not a partial sector"
        );
        assert!(!disk.dirty, "and the image must not be marked as changed");

        // The guard is the *pair* of bounds, so prove the ordinary write still
        // works — a refusal that refuses everything would pass the assert above.
        let ctrl = m.controllers.len() - 1;
        m.controllers[ctrl].buffer_loaded(1, &[0x77; 512]);
        m.service(HostRequest::Write { drive: 1, offset: 0, len: 512 }, ctrl);
        let disk = m.disks[1].as_ref().expect("still inserted");
        assert!(
            disk.bytes[..512].iter().all(|&b| b == 0x77),
            "a write the controller can actually satisfy must still land"
        );
        assert!(disk.dirty, "and must mark the image changed");
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

        // The hard disk reads *its* slot 1 and gets its own bytes. Slot 1 is
        // unit 0's second platter, which the guest reaches by head — head 2,
        // since head is platter x 2 + side.
        m.port_out(0xA1, 0); // a status read first, to clear the power-on byte
        m.port_in(0xA1);
        m.port_out(0xA7, 0x40); // head 2, sector 0
        m.port_out(0xA3, 0x30); // read sector, unit 0
        m.port_out(0xA7, 0x00);
        m.port_out(0xA3, 0x50); // read buffer, all 256
        let mut got = Vec::new();
        for _ in 0..HD_SECTOR {
            got.push(m.port_in(0xA5));
        }
        assert!(got.iter().all(|&b| b == 0x22), "the hard disk must read its own image");

        // And *its* slot 0 — where the floppy is — is refused, not served with
        // floppy bytes at a hard-disk offset.
        m.port_out(0xA7, 0x00);
        m.port_out(0xA3, 0x30); // read sector 0, head 0: unit 0's first platter
        let err = m.port_in(0xA1);
        assert_eq!(
            err & crate::cpm::hdsk::error::NOT_READY,
            crate::cpm::hdsk::error::NOT_READY,
            "slot 0 holds a floppy, so the hard disk has no platter there: {err:#04x}"
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

    /// The "48k" out of a CP/M `STAT B:` line.
    ///
    /// A drive-specific `STAT` says "Bytes Remaining On B: 48k", which is not
    /// the wording a bare `STAT` uses — see [`free_k`]. Its own function for
    /// the same reason that one has: both readings of a before/after
    /// comparison must come from one parser.
    #[cfg(test)]
    fn remaining_k(stat: &str) -> Option<u32> {
        let tail = stat.split("Remaining On").nth(1)?;
        let digits: String = tail
            .chars()
            .skip_while(|c| !c.is_ascii_digit())
            .take_while(|c| c.is_ascii_digit())
            .collect();
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
    /// EGT8080 is the sharpest probe available for this. It is a real CP/M
    /// program, built by a period assembler, that drives a UART directly — so
    /// if it comes up and talks to `0x12`/`0x13` inside a booted Altair, then
    /// the controller, the bootstrap, the CPU, the console, the guest's own
    /// BDOS and our modem ports are all working together.
    ///
    /// **Blocked on getting EGT8080 onto an Altair floppy in the first place**,
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
    /// receiver for the 88-2SIO, so the guest can pull EGT8080 in over our
    /// virtual modem port and write it with its own BDOS. That is the next
    /// thing to try, and it would test more of the path than this does.
    ///
    /// Ignored: set `CPM_DATA_IMAGE` to an Altair CP/M floppy — EGT8080 is written
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
        // `CPM_BOOT_IMAGE` to be "an image carrying EGT8080.COM", which no test
        // produces and none keeps — so it could not be run, and an `#[ignore]`
        // test that cannot be run looks exactly like one that passes.
        let Ok(path) = std::env::var("CPM_DATA_IMAGE") else {
            eprintln!("set CPM_DATA_IMAGE to an Altair CP/M floppy (EGT8080 is written into a copy)");
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
            dir.to_ascii_uppercase().contains("EGT8080"),
            "the guest's own DIR does not list EGT8080: {dir:?}"
        );

        // Now run it.
        for &b in b"EGT8080\r" {
            m.send_key(b);
        }
        let screen = printable(&run_until_quiet(&mut m, &mut cpu, 200_000_000));
        println!("--- EGT8080 ---\n{screen}");
        assert!(
            screen.to_ascii_uppercase().contains("EGT8080"),
            "EGT8080 did not start: {screen:?}"
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
        const EGT8080_COM: &[u8] = include_bytes!("../../EGT8080/EGT8080.COM");

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
        for &b in b"PCGET EGT8080.COM B\r" {
            m.send_key(b);
        }
        let prompt = printable(&run_until_quiet(&mut m, &mut cpu, 200_000_000));
        println!("--- PCGET ---\n{prompt}");
        assert!(
            prompt.contains("XMODEM"),
            "PCGET did not ask for the file: {prompt:?}"
        );

        let (done, during) = xmodem_send_to_guest(&mut m, &mut cpu, EGT8080_COM, 4_000_000_000);
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
        for &b in b"DIR EGT8080.COM\r" {
            m.send_key(b);
        }
        let dir = printable(&run_until_quiet(&mut m, &mut cpu, 400_000_000));
        println!("--- DIR ---\n{dir}");
        assert!(
            dir.to_ascii_uppercase().contains("EGT8080"),
            "the guest wrote the file but does not list it: {dir:?}"
        );

        // Now read it back with the guest's own sender, which is the only
        // check that the bytes on the disk are the bytes we sent: it uses the
        // guest's filesystem, its block mapping and its BIOS, none of which we
        // understand well enough to verify ourselves.
        for &b in b"PCPUT EGT8080.COM B\r" {
            m.send_key(b);
        }
        let ready = printable(&run_until_quiet(&mut m, &mut cpu, 400_000_000));
        println!("--- PCPUT ---\n{ready}");
        let (got, _) = xmodem_receive_from_guest(&mut m, &mut cpu, 4_000_000_000);
        let got = got.expect("the guest never sent the file back");
        assert!(
            got.len() >= EGT8080_COM.len(),
            "got {} bytes back, sent {}",
            got.len(),
            EGT8080_COM.len()
        );
        // XMODEM pads the last block, so compare only what we sent.
        assert_eq!(
            &got[..EGT8080_COM.len()],
            EGT8080_COM,
            "the file came back different from the one we sent"
        );
        println!("round trip: {} bytes, identical", EGT8080_COM.len());

        // And it is in the image the caller would persist.
        let dirty = m.take_dirty();
        assert_eq!(dirty.len(), 1, "the written image comes back for saving");
        assert!(
            dirty[0].1.windows(5).any(|w| w == b"EGT8080"),
            "the directory entry is in the image we would write out"
        );
    }

    /// The processor the surveys and the workbench run on: `CPM_BOOT_CPU`,
    /// defaulting to whatever an unconfigured gateway uses.
    ///
    /// Its own function so the survey and the workbench cannot end up reading
    /// different variables — the same reason the machine is resolved in one
    /// place.
    #[cfg(test)]
    fn survey_cpu() -> String {
        std::env::var("CPM_BOOT_CPU").unwrap_or_else(|_| crate::cpm::cpu::DEFAULT_CPU.to_string())
    }

    /// Type at a booted guest and let it settle.
    #[cfg(test)]
    fn type_at(m: &mut BootMachine, cpu: &mut Cpu, keys: &[u8], budget: u64) -> String {
        for &b in keys {
            m.send_key(b);
        }
        printable(&run_until_quiet(m, cpu, budget))
    }

    /// Boot a disk, pull a terminal onto it with the guest's own `PCGET`, and
    /// hand back the resulting image.
    ///
    /// Done once and reused, because it is the slow part and every case below
    /// wants the same disk.
    ///
    /// `name` because there are two terminals now, and `PCGET` is told the
    /// filename: putting EGT8080 on the disk under EGT8080's name would leave
    /// the 8080 gate running the Z80 build and reporting success.
    #[cfg(test)]
    fn image_with_terminal(path: &str, name: &str, egt80: &[u8]) -> Vec<u8> {
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
        type_at(&mut m, &mut cpu, format!("PCGET {name} B\r").as_bytes(), 200_000_000);
        let (done, _) = xmodem_send_to_guest(&mut m, &mut cpu, egt80, 4_000_000_000);
        assert!(done, "could not put {name} on the disk");
        run_until_quiet(&mut m, &mut cpu, 400_000_000);
        m.take_dirty().pop().expect("the guest wrote the image").1
    }

    /// **EGT8080 runs on the 8080 core, and its DAA path is right there.**
    ///
    /// The gate for the 8080 build. `cpm_cpu = 8080` shipped with the cost that
    /// the bundled terminal was Z80 code and crashed the machine it was on;
    /// EGT8080 removes that, and this is what says so with a real 8080.
    ///
    /// Three things are checked at once, and the third is the one that could
    /// not be reasoned about:
    ///
    /// * **It starts.** A single stray Z80 opcode anywhere on the path from
    ///   `0100H` to the sign-on would put an 8080 into the weeds instead.
    ///   `tools/check8080.py` makes that impossible at build time; this proves
    ///   the built artifact on the actual core.
    /// * **The port menu works**, which walks a long way further into the
    ///   program — the settings screens, the vector patching, `PSSET2`'s
    ///   `EX DE,HL` where the Z80 build has `LD (nn),DE`.
    /// * **`DAA` agrees with the Z80.** `PHEXD` prints a hex digit with
    ///   `AND 0FH / ADD A,90H / DAA / ADC A,40H / DAA`, and DAA is the one
    ///   instruction whose behaviour genuinely differs between the two CPUs —
    ///   the Z80's N flag makes it adjust downward after a subtraction. Both
    ///   of these follow additions, so it *should* be identical, and "should"
    ///   is why this is a test. The `12` in `at 12` is that routine's output:
    ///   get DAA wrong and the address reads as `0C` or as punctuation.
    ///
    /// Ignored: set `CPM_BOOT_IMAGE` to an Altair CP/M image carrying PCGET.COM.
    #[test]
    #[ignore]
    fn test_egt8080_runs_on_an_8080() {
        const EGT8080_COM: &[u8] = include_bytes!("../../EGT8080/EGT8080.COM");
        let Ok(path) = std::env::var("CPM_BOOT_IMAGE") else {
            eprintln!("set CPM_BOOT_IMAGE to an Altair CP/M image carrying PCGET.COM");
            return;
        };
        // Placed with the Z80 core because that is only a file transfer run by
        // the disk's own PCGET; what is under test is EGT8080 *running*.
        let disk = image_with_terminal(&path, "EGT8080.COM", EGT8080_COM);

        // Both cores, because that is the claim: 8080 opcodes are a strict
        // subset of the Z80's, so the 8080 build is the one that runs
        // everywhere and the reason it can be the default. Running it only on
        // an 8080 would leave the *other* half of that untested — and the
        // half that would break silently, since the Z80 core is what every
        // default gateway uses.
        for which in [crate::cpm::cpu::CPU_8080, crate::cpm::cpu::CPU_Z80] {
            let mut m = BootMachine::new();
            m.insert(0, disk.clone(), true).unwrap();
            assert!(matches!(
                m.attach_modem(crate::cpm::resolve_access("altair_2sio2")),
                ModemAttach::Ports(0x12, 0x13)
            ));
            m.modem().set_carrier(true);

            let mut cpu = BootMachine::new_cpu_for(which);
            m.boot(&mut cpu, 0).expect("boots");
            run_until_quiet(&mut m, &mut cpu, 60_000_000);

            let start = type_at(&mut m, &mut cpu, b"EGT8080\r", 400_000_000);
            assert!(
                start.contains("Ethernet Gateway Terminal"),
                "EGT8080 did not start on the {which}: {start:?}"
            );
            assert!(
                start.contains("EGT8080"),
                "the Z80 build seems to be running under the 8080 build's \
                 name on the {which}: {start:?}"
            );

            // Settings -> Serial port -> 6850 ACIA -> Altair 88-2SIO port 2.
            // One key at a time: a four-key burst that half-lands looks
            // exactly like a menu that did not take.
            let mut picked = String::new();
            for k in b"SP32" {
                picked.push_str(&type_at(&mut m, &mut cpu, &[*k], 400_000_000));
            }
            if !picked.contains("6850 ACIA at 12") {
                println!("--- EGT8080 on the {which}, after SP32 ---\n{picked}");
                panic!("EGT8080 does not report the selected port on the {which}");
            }

            // And it moves bytes both ways over it, which is what a terminal is.
            type_at(&mut m, &mut cpu, b"Q", 100_000_000);
            type_at(&mut m, &mut cpu, b"T", 100_000_000);
            m.modem().queue_rx(b"PING-FROM-GATEWAY");
            let seen = printable(&run_until_quiet(&mut m, &mut cpu, 200_000_000));
            let _ = m.modem().drain_tx();
            type_at(&mut m, &mut cpu, b"xyz", 100_000_000);
            let sent = String::from_utf8_lossy(&m.modem().drain_tx()).to_string();
            assert!(seen.contains("PING-FROM-GATEWAY"), "nothing reached EGT8080 on the {which}: {seen:?}");
            assert!(sent.contains("xyz"), "nothing left EGT8080 on the {which}: {sent:?}");
            println!("  {which}: sign-on, port menu, DAA, and bytes both ways.");
        }
    }

    /// **Every comms port EGT8080 offers, driven from inside a booted disk.**
    ///
    /// The question this answers is not "does the modem work" — `PCGET` already
    /// showed that — but "does the port the *operator* picks in EGT8080 line up
    /// with the port they picked in the gateway". Those are two independent
    /// settings that have to name the same hardware, and nothing until now
    /// checked that they do.
    ///
    /// Each case boots the disk fresh, runs EGT8080, walks its menus to select a
    /// port, and then moves bytes both ways over that port. The mismatch case
    /// at the end is the control: if it passed, the others would prove nothing,
    /// because a modem answering at every address would satisfy them all.
    ///
    /// Ignored: set `CPM_BOOT_IMAGE` to an Altair CP/M image carrying PCGET.COM.
    #[test]
    #[ignore]
    fn test_egt80_comms_ports_inside_a_booted_disk() {
        const EGT8080_COM: &[u8] = include_bytes!("../../EGT8080/EGT8080.COM");
        let Ok(path) = std::env::var("CPM_BOOT_IMAGE") else {
            eprintln!("set CPM_BOOT_IMAGE to an Altair CP/M image carrying PCGET.COM");
            return;
        };
        let disk = image_with_terminal(&path, "EGT8080.COM", EGT8080_COM);
        println!("EGT8080 is on the disk; now testing its ports.\n");

        // (gateway profile, EGT8080 menu keys, what its Port: line should say,
        //  whether the two should reach each other)
        // The expected text is EGT8080's own "Port:" wording — the chip family
        // *and* the address — because a bare address matches all sorts of
        // unrelated things on that screen and would let a case pass without the
        // port having been selected at all.
        let cases: &[(&str, &[u8], &str, bool)] = &[
            // The pairing a booted Altair wants: 2SIO port B at both ends.
            ("altair_2sio2", b"SP32", "6850 ACIA at 12", true),
            // The gateway's own default port, which EGT8080 offers by our name.
            ("rc2014_1b", b"SP1", "Z80 SIO/2 at 82", true),
            // The original MITS board.
            // `4` then `1`: EGT8080 asks which address, as it does for the 2SIO.
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

            let start = type_at(&mut m, &mut cpu, b"EGT8080\r", 400_000_000);
            assert!(
                start.contains("Ethernet Gateway Terminal"),
                "EGT8080 did not start under {uart}: {start:?}"
            );

            // Settings -> Serial port -> the choice for this case.
            let picked = type_at(&mut m, &mut cpu, keys, 200_000_000);
            if !picked.contains(want_port) {
                // The screen is the evidence: a menu that asked something we
                // did not answer looks identical to a port that did not take.
                println!("--- {uart}: EGT8080 after {} ---\n{picked}", String::from_utf8_lossy(keys));
                panic!("EGT8080 does not report {want_port:?} under {uart}");
            }

            // Back out to the main menu and into terminal mode.
            type_at(&mut m, &mut cpu, b"Q", 100_000_000);
            type_at(&mut m, &mut cpu, b"T", 100_000_000);

            // Peer -> guest: what we queue should appear on EGT8080's screen.
            m.modem().queue_rx(b"PING-FROM-GATEWAY");
            let seen = printable(&run_until_quiet(&mut m, &mut cpu, 200_000_000));

            // Guest -> peer: what we type should leave through the modem.
            let _ = m.modem().drain_tx();
            type_at(&mut m, &mut cpu, b"xyz", 100_000_000);
            let sent = String::from_utf8_lossy(&m.modem().drain_tx()).to_string();

            let reached = seen.contains("PING-FROM-GATEWAY");
            let replied = sent.contains("xyz");
            println!(
                "  {uart:<14} EGT8080 {:<6} port {want_port}: in={} out={}",
                String::from_utf8_lossy(keys),
                if reached { "yes" } else { "no " },
                if replied { "yes" } else { "no" },
            );
            if *should_reach {
                assert!(reached, "{uart}: EGT8080 never showed what we sent — {seen:?}");
                assert!(replied, "{uart}: typing never reached the modem — {sent:?}");
            } else {
                assert!(
                    !reached && !replied,
                    "{uart} answered a port EGT8080 was not pointed at — \
                     the matching cases prove nothing if this one passes"
                );
            }
        }
    }

    /// The deep test of the port a booted Altair actually uses: move a real
    /// file through **EGT8080's own XMODEM**, at volume, and read it back.
    ///
    /// The port matrix proves each port is wired to the right addresses, but it
    /// does so with a burst of a few bytes. That is not the same as a working
    /// link: a UART that drops a byte under load, or gets its transmit-ready
    /// bit wrong, passes a short burst and fails a file. This sends 4 KB — 32
    /// XMODEM blocks, each acknowledged — through the terminal we ship, has the
    /// guest write it to its own disk, and then has EGT8080 read it back off that
    /// disk and send it out again, and
    /// compares. Every byte has to survive EGT8080's receiver, our modem rings,
    /// the guest's filesystem, the 88-DCDD write and read paths, and EGT8080's
    /// sender — and come back identical.
    ///
    /// Ignored: set `CPM_BOOT_IMAGE` to an Altair CP/M image carrying PCGET.COM.
    #[test]
    #[ignore]
    fn test_egt80_transfers_a_file_over_2sio2() {
        const EGT8080_COM: &[u8] = include_bytes!("../../EGT8080/EGT8080.COM");
        let Ok(path) = std::env::var("CPM_BOOT_IMAGE") else {
            eprintln!("set CPM_BOOT_IMAGE to an Altair CP/M image carrying PCGET.COM");
            return;
        };
        // Deliberately not EGT8080's own bytes: a file that happened to be left
        // on the disk would otherwise let this pass without transferring
        // anything. A pattern that is not 8.3-ish text also shows up plainly if
        // it lands in the wrong place.
        let payload: Vec<u8> = (0..4096u32).map(|i| (i as u8) ^ 0x5A).collect();

        let disk = image_with_terminal(&path, "EGT8080.COM", EGT8080_COM);
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

        // EGT8080, on 88-2SIO port B.
        let start = type_at(&mut m, &mut cpu, b"EGT8080\r", 400_000_000);
        assert!(start.contains("Ethernet Gateway Terminal"), "{start:?}");
        let picked = type_at(&mut m, &mut cpu, b"SP32", 200_000_000);
        assert!(picked.contains("6850 ACIA at 12"), "{picked:?}");
        type_at(&mut m, &mut cpu, b"Q", 100_000_000);

        // Its own XMODEM receive, into a file on the guest's disk.
        let ask = type_at(&mut m, &mut cpu, b"D", 200_000_000);
        assert!(
            ask.contains("Receive as which file?"),
            "EGT8080 did not ask for a name: {ask:?}"
        );
        type_at(&mut m, &mut cpu, b"XFER.DAT\r", 200_000_000);

        let (done, seen) = xmodem_send_to_guest(&mut m, &mut cpu, &payload, 4_000_000_000);
        assert!(done, "EGT8080 never finished receiving: {}", printable(&seen));
        let after = printable(&run_until_quiet(&mut m, &mut cpu, 400_000_000));
        println!("--- EGT8080 receive ---\n{}{after}", printable(&seen));
        assert!(after.contains("Received."), "EGT8080 did not report success: {after:?}");

        // Verified from the image itself rather than by driving EGT8080's exit
        // path.  The first attempt typed `X` then `DIR XFER.DAT`, and when the
        // exit did not land where expected those keystrokes went into EGT8080's
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
        println!("{} bytes through EGT8080's own XMODEM, onto the guest's disk", payload.len());

        // The other half: EGT8080's *send* path, reading the file back off the
        // guest's disk and pushing it out the same port.  That closes the loop
        // through the terminal we ship rather than through the disk's own
        // tools.
        //
        // EGT8080 ends a transfer with "Press any key." before it returns to its
        // menu, so a key comes first.  Getting that wrong is what made an
        // earlier attempt type its next command into the menu instead of at
        // `A>`.
        let back_at_menu = type_at(&mut m, &mut cpu, b" ", 200_000_000);
        assert!(
            back_at_menu.contains("Choice:"),
            "EGT8080 did not come back to its menu: {back_at_menu:?}"
        );
        let ask = type_at(&mut m, &mut cpu, b"U", 200_000_000);
        if !ask.contains("Send which file?") {
            println!("--- EGT8080 after U ---\n{ask}");
            panic!("EGT8080 did not ask which file to send");
        }
        type_at(&mut m, &mut cpu, b"XFER.DAT\r", 400_000_000);

        let (back, sending) = xmodem_receive_from_guest(&mut m, &mut cpu, 4_000_000_000);
        println!("--- EGT8080 send ---\n{}", printable(&sending));
        let back = back.expect("EGT8080 never sent the file back");
        assert!(
            back.len() >= payload.len(),
            "EGT8080 sent {} bytes of {}",
            back.len(),
            payload.len()
        );
        // XMODEM pads the last block, so compare only what was sent in.
        assert_eq!(
            &back[..payload.len()],
            &payload[..],
            "what EGT8080 sent back differs from what it received"
        );
        println!(
            "{} bytes in through EGT8080's XMODEM, onto the disk, and back out again",
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

    /// **A booted guest writes the disk we mounted beside it, and the change
    /// reaches the host file.**
    ///
    /// The gate for allowing writes to mounted images. Two things could go
    /// wrong and only a live guest can tell them apart: the write could never
    /// leave the machine (the slot marked read-only, `take_dirty` silent), or
    /// it could leave under the *wrong slot* and land in the boot disk's file
    /// — which a single session cannot detect, because a guest reading back its
    /// own wrong sector sees exactly what it wrote.
    ///
    /// So this checks the byte streams apart: the boot image must come back
    /// **unchanged** and the mounted one **changed**, and then a second machine
    /// boots the untouched boot disk with the *changed* companion at slot 1 and
    /// the guest's own `DIR B:` has to find the file. Nothing here trusts our
    /// reader — the disk's own BIOS answers.
    ///
    /// `SAVE` rather than `PIP`, for the reason the hard-disk gate gives: one
    /// command, no end-of-file to feed, nothing depending on the console.
    ///
    /// Ignored: set `CPM_FLOPPY_BOOT`/`CPM_FLOPPY_MOUNT` to two Altair CP/M
    /// floppies.
    #[test]
    #[ignore]
    fn test_a_guest_writes_a_mounted_disk_and_it_reaches_the_file() {
        let (Ok(boot), Ok(mount)) =
            (std::env::var("CPM_FLOPPY_BOOT"), std::env::var("CPM_FLOPPY_MOUNT"))
        else {
            eprintln!("set CPM_FLOPPY_BOOT and CPM_FLOPPY_MOUNT to run this");
            return;
        };
        let boot_bytes = std::fs::read(&boot).unwrap();
        let mount_bytes = std::fs::read(&mount).unwrap();

        let mut m = BootMachine::new();
        m.insert(0, boot_bytes.clone(), false).expect("the boot floppy");
        m.insert(1, mount_bytes.clone(), false).expect("the mounted floppy");
        let mut cpu = BootMachine::new_cpu();
        m.boot(&mut cpu, 0).expect("boots");
        assert!(!run_until_quiet(&mut m, &mut cpu, 200_000_000).is_empty(), "never signed on");

        // `STAT B:` and a bare `STAT` word this differently — "Bytes Remaining
        // On B: 48k" against "Space: 3744k" — so this is its own parser and
        // both readings go through it. Comparing a number from one parser with
        // a number from another proves nothing about the disk.
        let stat_before = type_at(&mut m, &mut cpu, b"STAT B:\r", 400_000_000);
        println!("--- STAT B: before ---\n{stat_before}");
        let before = remaining_k(&stat_before).expect("STAT B: reports free space before");
        let saved = type_at(&mut m, &mut cpu, b"SAVE 2 B:ZZMOUNT.COM\r", 400_000_000);
        println!("--- SAVE to B: ---\n{saved}");
        let listed = type_at(&mut m, &mut cpu, b"STAT B:ZZMOUNT.COM\r", 400_000_000);
        println!("--- STAT B: in session one ---\n{listed}");
        assert!(
            listed.to_ascii_uppercase().contains("ZZMOUNT"),
            "the save to the mounted disk did not take: {listed:?}"
        );

        // Which slots came back dirty is the whole question: the *mounted* one
        // and not the boot disk.  Writing under the wrong slot is the failure a
        // single session cannot see, and it would show up right here.
        let dirty = m.take_dirty();
        let slots: Vec<u8> = dirty.iter().map(|(s, _)| *s).collect();
        assert_eq!(slots, vec![1], "only the mounted disk should have changed: {slots:?}");
        let written = dirty.into_iter().next().unwrap().1;
        assert_eq!(written.len(), mount_bytes.len(), "the mounted image changed size");
        assert_ne!(written, mount_bytes, "nothing was written to the mounted disk");

        // Session two: the untouched boot disk, and the companion as it would
        // now be on the host.  The guest's own BIOS has to find the file.
        let mut m2 = BootMachine::new();
        m2.insert(0, boot_bytes, true).unwrap();
        m2.insert(1, written, true).unwrap();
        let mut cpu2 = BootMachine::new_cpu();
        m2.boot(&mut cpu2, 0).expect("the boot disk still boots");
        assert!(!run_until_quiet(&mut m2, &mut cpu2, 200_000_000).is_empty(), "never signed on");
        let dir = type_at(&mut m2, &mut cpu2, b"DIR B:ZZMOUNT.COM\r", 400_000_000);
        println!("--- DIR B: in session two ---\n{dir}");
        assert!(
            dir.to_ascii_uppercase().contains("ZZMOUNT"),
            "the file did not survive on the mounted disk: {dir:?}"
        );

        // And the allocation bitmap moved with it — an entry with its blocks
        // still free is the corruption that only bites the *next* file written.
        let stat = type_at(&mut m2, &mut cpu2, b"STAT B:\r", 400_000_000);
        println!("--- STAT B: in session two ---\n{stat}");
        let after_free = remaining_k(&stat).expect("STAT B: reports free space after");
        assert!(
            after_free < before,
            "free space on B: went {before}k -> {after_free}k, so the blocks were \
             never claimed: {stat:?}"
        );
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
    /// **`CPM_TOOL_IMAGE` is shared with
    /// [`test_capture_altair_ground_truth`], which needs `PCPUT.COM` on the
    /// same disk**, and the two gates used to document *opposite* assignments
    /// of these two variables — so each passed alone and one always failed when
    /// the set was run in one go. DISK05 carries both tools; the disk under
    /// study is DISK01. Nothing on the host is modified either way: unit 1 is
    /// writable in memory only. `tools/cpm-live-gates` sets the whole set.
    ///
    /// Ignored:
    ///   `CPM_TOOL_IMAGE=...DISK05.DSK`   boots, carries SYSGEN *and* PCPUT
    ///   `CPM_DATA_IMAGE=...DISK01.DSK`   a different CP/M, whose system is replaced
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
            // Any board on any machine here, which is what the survey wants:
            // a z80pack disk must not be "skipped" merely because the machine
            // this run happens to be configured for cannot carry it.
            let len = bytes.len() as u64;
            if !BootMachine::bootable_media()
                .iter()
                .any(|m| len >= m.bytes && len <= m.bytes + m.trailer)
            {
                println!("  skipped  {name}  ({} bytes — no controller takes it)", bytes.len());
                continue;
            }
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
            // `CPM_BOOT_CPU=8080` surveys the whole folder on the other
            // processor, which is the only way to answer "what does that
            // setting do to my disks" with a list rather than an opinion.
            let mut cpu = BootMachine::new_cpu_for(&survey_cpu());
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

    /// **What every bootable image in a folder does with each spelling of
    /// Backspace.**
    ///
    /// The wide version of the single-disk measurement, and the reason the
    /// gateway can carry a *default* rather than a list of disks it works on.
    /// One run boots each image, types a word at whatever prompt it reaches, and
    /// reports the bytes it gets back for BS (0x08) and for DEL (0x7F)
    /// separately, so a guest that wants the rubout can be seen rather than
    /// assumed not to exist.
    ///
    /// Each key is measured on its own freshly booted machine: an editing key
    /// changes the line the guest is holding, so measuring both against one boot
    /// would let the first answer shape the second.
    ///
    /// Ignored — set `CPM_BOOT_DIR` to a folder of `.dsk` files.
    #[test]
    #[ignore]
    fn test_survey_backspace_across_every_bootable_image() {
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

        /// Boot one image and see what it echoes for `key` after a typed word.
        /// `None` if the disk never reached a prompt we could type at.
        fn echo_for(bytes: &[u8], key: u8) -> Option<(Vec<u8>, Vec<u8>)> {
            let configured = std::env::var("CPM_BOOT_MACHINE")
                .unwrap_or_else(|_| crate::cpm::console::AUTO_MACHINE.to_string());
            let (machine, _why) = crate::cpm::detect::machine_for(&configured, bytes);
            let mut m = BootMachine::new();
            m.set_machine(&machine);
            m.insert(0, bytes.to_vec(), true).ok()?;
            // `CPM_BOOT_CPU` here too: a survey that silently ran a different
            // processor from the one next door would answer a question nobody
            // asked.
            let mut cpu = BootMachine::new_cpu_for(&survey_cpu());
            m.boot(&mut cpu, 0).ok()?;
            if run_until_quiet(&mut m, &mut cpu, 200_000_000).is_empty() {
                return None; // never signed on
            }
            for &b in b"TESTING" {
                m.send_key(b);
            }
            let typed = run_until_quiet(&mut m, &mut cpu, 50_000_000);
            m.send_key(key);
            Some((typed, run_until_quiet(&mut m, &mut cpu, 50_000_000)))
        }

        // A guest that erases answers the universal BS SPACE BS.  Anything else
        // is worth a human reading the bytes, which is why they are printed.
        const ERASE: &[u8] = b"\x08 \x08";
        let (mut booted, mut bs_erases, mut del_erases) = (0u32, 0u32, 0u32);
        let mut odd: Vec<String> = Vec::new();
        for name in &names {
            let bytes = std::fs::read(std::path::Path::new(&dir).join(name)).unwrap();
            let Some((typed, bs)) = echo_for(&bytes, 0x08) else {
                println!("  --       {name}  (no prompt)");
                continue;
            };
            let del = echo_for(&bytes, 0x7F).map(|(_, d)| d).unwrap_or_default();
            booted += 1;
            if bs == ERASE {
                bs_erases += 1;
            }
            if del == ERASE {
                del_erases += 1;
            }
            if bs != ERASE {
                odd.push(name.clone());
            }
            println!(
                "  {name:<16} typed={:02X?}  BS={:02X?}  DEL={:02X?}",
                typed, bs, del
            );
        }
        println!(
            "\n  {booted} booted to a prompt: {bs_erases} erase on BS, {del_erases} on DEL"
        );
        if !odd.is_empty() {
            println!("  did NOT erase on BS: {odd:?}");
        }
        assert!(booted > 0, "no image in {dir} reached a prompt");
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

    /// A VDM-1 guest is not mute because it failed — it is painting a screen
    /// through a card that has no port to print to.
    ///
    /// The gate for the whole feature, and it runs through the **shipped path**
    /// rather than reading the window itself: the machine publishes, the
    /// registry hands the frame over, and `vdm::frame` renders it — exactly the
    /// three steps a browser's poll takes. A test that sampled memory and then
    /// described what the display *would* do with it is how a plausible-but-
    /// wrong renderer survives; the printer work learned that the expensive way.
    ///
    /// TDISK04's CP/M assembles with
    /// `VDM EQU TRUE` and prints by storing bytes into the Processor Technology
    /// VDM-1's window at `CC00`, 64 columns by 16 lines, scrolling with the
    /// register on port `C8`. It never writes a console character to any port —
    /// verified by scanning its system tracks for `OUT 05h`, `OUT 01h` and
    /// `OUT 11h`, none of which appear. So with the right console it takes
    /// keystrokes perfectly and still shows nothing.
    ///
    /// Give it that console, run it, and its sign-on is sitting in screen
    /// memory — where the web UI's viewer now finds it.
    ///
    /// Ignored: set `CPM_VDM_IMAGE` to TDISK04.DSK (or another VDM-1 disk).
    #[test]
    #[ignore]
    fn test_a_vdm_guest_writes_its_signon_into_screen_memory() {
        use crate::cpm::{screen, vdm};
        let Ok(path) = std::env::var("CPM_VDM_IMAGE") else {
            eprintln!("set CPM_VDM_IMAGE to run this");
            return;
        };

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

        // Through the shipped path, not around it: register a screen the way a
        // booted session does, ask for a frame the way the browser's poll does,
        // publish at a seam the way the driver does, and render with the same
        // function the JSON route calls.
        let live = screen::register("live gate");
        let screen::Look::Waiting { .. } = screen::look(live.id()) else {
            panic!("nothing has been published yet")
        };
        m.publish_screen(&live);
        let screen::Look::Frame(snap) = screen::look(live.id()) else {
            panic!("the seam published the frame the viewer asked for")
        };

        let screen: String = vdm::frame_text(&vdm::frame(&snap.vdm.window, snap.vdm.scroll))
            .iter()
            .map(|l| format!("{}\n", l.trim_end()))
            .collect();
        println!("--- VDM-1 screen at {:#06x} ---\n{screen}", vdm::BASE);
        println!(
            "scroll={:#04x}, driven={} (a guest that has written C8h is running a VDM-1 driver)",
            snap.vdm.scroll, snap.vdm.active
        );
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

    /// **What a Cromemco Dazzler program actually drives.**
    ///
    /// The Dazzler is the VDM-1's problem one card along: a 1976 colour
    /// graphics board that reads its picture out of main memory by DMA, so a
    /// program using one writes *no* console bytes and the session stays blank.
    /// `DISK10.DSK` carries the whole Cromemco library — SPACEWAR, LIFE,
    /// KSCOPE, DMATION and twenty more — and TDISK04's KSCOPE prints only
    /// `CROMEMCO DAZZLER KSCOPE PROGRAM` before going quiet.
    ///
    /// This measures which ports such a program really touches, because the
    /// alternative was my recollection of a manual, and a port address
    /// remembered wrongly is the kind of mistake that looks like a broken
    /// emulation for a day. The boot's own ports are subtracted, so what is
    /// left is the *program's* — the disk controller and console drop out.
    ///
    /// Silence on the console is consistent with a Dazzler and is not proof of
    /// one; this is the proof.
    ///
    /// Ignored: set `CPM_DAZZLER_IMAGE` to a disk carrying Dazzler software,
    /// and `CPM_DAZZLER_CMD` to the program to run (default `KSCOPE`).
    #[test]
    #[ignore]
    fn test_measure_what_a_dazzler_program_drives() {
        let Ok(path) = std::env::var("CPM_DAZZLER_IMAGE") else {
            eprintln!("set CPM_DAZZLER_IMAGE to run this");
            return;
        };
        let cmd = std::env::var("CPM_DAZZLER_CMD").unwrap_or_else(|_| "KSCOPE".into());

        let mut m = BootMachine::new();
        m.insert(0, std::fs::read(&path).unwrap(), false).expect("a bootable image");
        let mut cpu = BootMachine::new_cpu();
        m.boot(&mut cpu, 0).expect("boots");
        let signon = printable(&run_until_quiet(&mut m, &mut cpu, 60_000_000));
        println!("--- sign-on ---\n{signon}");

        // Everything the machine did to get to its prompt: the controller, the
        // console, the loader.  Subtracted below so the program stands alone.
        let before = m.port_hits.clone();
        m.port_writes.clear();

        for &b in format!("{cmd}\r").as_bytes() {
            m.send_key(b);
        }
        let mut out = Vec::new();
        for _ in 0..200_000_000u64 {
            m.step(&mut cpu);
            out.extend(m.take_output());
        }
        println!("--- {cmd} said ---\n{}", printable(&out));

        let mut fresh: Vec<(u8, u64)> = Vec::new();
        for (&p, &n) in m.port_hits.iter() {
            let was = before.get(&p).copied().unwrap_or(0);
            if n > was {
                fresh.push((p, n - was));
            }
        }
        fresh.sort_by_key(|&(_, n)| std::cmp::Reverse(n));
        println!("--- ports {cmd} drove (the boot's own subtracted) ---");
        for (p, n) in &fresh {
            // `port_hits` records a write as `port | 0x80`, which is **lossy
            // above 0x7F**: an OUT to `FFh` and an OUT to `7Fh` land in the
            // same bucket.  Said here rather than left to be misread, because
            // it already has been once.  The value list below is exact.
            let (dir, port) = if p & 0x80 != 0 { ("OUT", p & 0x7F) } else { ("IN ", *p) };
            let note = if p & 0x80 != 0 { "  (or +0x80)" } else { "" };
            println!("  {dir} {port:#04x}   {n:>10}{note}");
        }
        // The values, for the ports that are not the console or the disk — a
        // count proves the board was driven, but only the value says how, and
        // for a card that reads main memory the value is what names the buffer.
        println!("--- what it wrote, port by port (console and disk left out) ---");
        let noisy = [0x08u8, 0x09, 0x0A, 0x10, 0x11];
        for (p, v) in m.port_writes.iter().filter(|(p, _)| !noisy.contains(p)) {
            println!("  OUT {p:#04x}, {v:#04x}   ({v:08b})");
        }
        assert!(!fresh.is_empty(), "{cmd} touched no port at all — it never ran");
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
        // Answer whatever the guest asked, for the systems that will not reach
        // a prompt without being told something first — Cromix asks for its
        // root device before it opens anything. `\r` separates the answers, so
        // `CPM_BOOT_KEYS=1\r0` gets past two questions.
        if let Ok(keys) = std::env::var("CPM_BOOT_KEYS") {
            for answer in keys.split("\\r") {
                for b in answer.bytes() {
                    m.send_key(b);
                }
                m.send_key(b'\r');
                out.clear();
                for _ in 0..40_000_000u64 {
                    m.step(&mut cpu);
                    out.extend(m.take_output());
                }
                println!("--- after typing {answer:?} ---\n{}", printable(&out));
            }
        }
        let text: String = out
            .iter()
            .map(|&b| if (0x20..0x7F).contains(&b) || b == b'\n' { b as char } else { '.' })
            .collect();
        // Where a quiet guest actually is.  A frozen PC and no port traffic
        // reads as "waiting for an interrupt" and that is a guess; this asks.
        // Set `CPM_BOOT_LOOP` to sample the PC after everything above has run,
        // report the hot addresses, and disassemble the code around them.
        if std::env::var("CPM_BOOT_LOOP").is_ok() {
            // The last answer is typed *inside* the trace, not before it: the
            // interesting instant is the transition, and by the time an
            // ordinary run has finished the guest is already sliding — which
            // makes the trace uniform and useless.  Measured that mistake.
            if let Ok(key) = std::env::var("CPM_BOOT_LOOP_KEY") {
                for b in key.bytes() {
                    m.send_key(b);
                }
                m.send_key(b'\r');
            }
            let mut seen: std::collections::BTreeMap<u16, u64> =
                std::collections::BTreeMap::new();
            // The last few addresses before the guest fell into a straight run
            // of increments — which is what executing blank memory looks like.
            // Catching the *edge* is the whole point: the hot list afterwards
            // is uniform noise and says nothing about how it got there.
            let mut recent: std::collections::VecDeque<u16> =
                std::collections::VecDeque::new();
            let mut straight = 0u32;
            let mut fell: Option<Vec<u16>> = None;
            let mut candidate: Option<Vec<u16>> = None;
            for _ in 0..60_000_000u64 {
                let pc = cpu.registers().pc();
                *seen.entry(pc).or_insert(0) += 1;
                if recent.back().map(|p| p.wrapping_add(1) == pc).unwrap_or(false) {
                    straight += 1;
                } else {
                    straight = 0;
                }
                recent.push_back(pc);
                if recent.len() > 40 {
                    recent.pop_front();
                }
                // Snapshot the window as the run *starts*, and keep it only if
                // the run turns out to be long.  Snapshotting at 200 shows the
                // slide itself, which says nothing about how it got there —
                // measured that mistake first.
                if straight == 1 {
                    candidate = Some(recent.iter().copied().collect());
                }
                if straight == 200 && fell.is_none() {
                    fell = candidate.take();
                }
                m.step(&mut cpu);
            }
            if let Some(trail) = &fell {
                println!("--- it fell into a straight run here ---");
                println!("  last 40 PCs: {:04x?}", trail);
            } else {
                println!("--- no straight run of 200: it is not sliding through blank memory ---");
            }
            let mut hot: Vec<(u16, u64)> = seen.into_iter().collect();
            hot.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
            println!("--- where it spends its time ({} distinct addresses) ---", hot.len());
            for (pc, n) in hot.iter().take(12) {
                println!("  {pc:#06x}  {n:>9}");
            }
            let r = cpu.registers();
            println!(
                "  regs: af={:04x} bc={:04x} de={:04x} hl={:04x} sp={:04x}",
                r.get16(iz80::Reg16::AF),
                r.get16(iz80::Reg16::BC),
                r.get16(iz80::Reg16::DE),
                r.get16(iz80::Reg16::HL),
                r.get16(iz80::Reg16::SP),
            );
            // Disassemble the tightest region: the hottest address and what
            // surrounds it.  PC is saved and put back, because disassembling
            // walks it.
            println!("  bank selected at the end: {}", m.bank40.current());
            // Disassemble from bank 0, because the guest died in whatever bank
            // it switched *to* and the code we want to read is the code that
            // did the switching.
            if let Ok(at) = std::env::var("CPM_BOOT_DISASM_AT") {
                let at = u16::from_str_radix(at.trim_start_matches("0x"), 16).unwrap();
                m.bank40.port_out(0x01); // bank 0
                let saved = cpu.registers().pc();
                cpu.registers().set_pc(at);
                println!("--- bank 0 code at {at:#06x} ---");
                for _ in 0..20 {
                    let a = cpu.registers().pc();
                    println!("  {a:04x}  {}", cpu.disasm_instruction(&mut m));
                }
                cpu.registers().set_pc(saved);
            }
            let centre = hot.first().map(|(pc, _)| *pc).unwrap_or(0);
            let from = centre.saturating_sub(16);
            let saved = cpu.registers().pc();
            cpu.registers().set_pc(from);
            println!("--- code around {centre:#06x} ---");
            for _ in 0..24 {
                let at = cpu.registers().pc();
                let mark = if at == centre { " <== hottest" } else { "" };
                println!("  {at:04x}  {}{mark}", cpu.disasm_instruction(&mut m));
            }
            cpu.registers().set_pc(saved);
        }
        println!("--- {} ---\n{}", path, text);
        println!("port hits (0x80 bit = OUT): {:?}", m.port_hits);
        // What a guest actually wrote to a port, in order — a count says a
        // register was driven, and only the values say how.
        if let Ok(p) = std::env::var("CPM_BOOT_WATCH") {
            let want = u8::from_str_radix(p.trim_start_matches("0x"), 16).unwrap();
            let vals: Vec<String> = m
                .port_writes
                .iter()
                .filter(|(port, _)| *port == want)
                .map(|(_, v)| format!("{v:#04x}"))
                .collect();
            println!("--- writes to {want:#04x} ({} of them) ---\n  {}", vals.len(), vals.join(" "));
        }
        println!("mem[0..12] = {:02x?}", (0..12).map(|a| m.peek(a)).collect::<Vec<_>>());
        let st = m.port_in(0x08);
        println!("status={st:#04x}  track0 bit={}  moveok bit={}",
                 if st & 0x40 == 0 { "AT TRACK 0" } else { "not track 0" },
                 if st & 0x02 == 0 { "may move" } else { "busy" });
        println!(
            "pc={:#06x} stuck_polls={} idle_console={} halted={}",
            cpu.registers().pc(),
            m.stuck_polls(),
            m.idle_status_reads(),
            // A guest sitting on `HALT` is not stuck: it is *waiting for an
            // interrupt*, and this machine has never delivered one to anybody.
            // Worth printing rather than inferring from a frozen PC, because
            // "spinning" and "halted" want completely different investigations.
            cpu.is_halted(),
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
    /// 0 is a tool disk carrying `PCPUT.COM`; drive 1 is the disk under study,
    /// mounted read-only, and files are named `B:NAME.EXT`.
    ///
    /// **`CPM_TOOL_IMAGE` is shared with
    /// [`test_a_system_track_written_by_a_guest_still_boots`], which needs
    /// `SYSGEN.COM` on the same disk**, so the tool disk has to carry both —
    /// DISK05 does. This used to name DISK07, which carries PCPUT but *not*
    /// SYSGEN: it satisfied this gate and made the other one fail with
    /// `SYSGEN?`, which reads like a broken guest. The string `SYSGEN` is in
    /// DISK07's bytes, inside another file; a name in the image is not a file
    /// in the directory. `tools/cpm-live-gates` sets the whole set at once.
    ///
    /// Ignored:
    ///   `CPM_TOOL_IMAGE=...DISK05.DSK`   boots, has PCPUT.COM *and* SYSGEN.COM
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
        let bytes = std::fs::read(&path).unwrap();
        // The machine, chosen the way a real boot chooses it — `auto` unless
        // told otherwise.  Without this the workbench silently ran everything
        // on the default Altair console, so a z80pack or Cromemco disk came up
        // mute here while booting perfectly in the survey: the tool used to
        // chase a quiet guest was itself the reason it was quiet.
        let configured = std::env::var("CPM_BOOT_MACHINE")
            .unwrap_or_else(|_| crate::cpm::console::AUTO_MACHINE.to_string());
        let (machine, why) = crate::cpm::detect::machine_for(&configured, &bytes);
        println!(
            "--- machine: {machine} ({}), {} ---",
            why.unwrap_or_else(|| "as configured".into()),
            crate::cpm::cpu::cpu_label(&survey_cpu())
        );
        m.set_machine(&machine);
        m.insert(0, bytes, ro).expect("a bootable image");
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
        let mut cpu = BootMachine::new_cpu_for(&survey_cpu());
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

        const EGT8080_COM: &[u8] = include_bytes!("../../EGT8080/EGT8080.COM");

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
        name[..5].copy_from_slice(b"EGT8080");
        let ext = *b"COM";
        fs.create(0, &name, &ext).unwrap();
        for (rec, chunk) in EGT8080_COM.chunks(128).enumerate() {
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
            dir.to_ascii_uppercase().contains("EGT8080"),
            "the guest does not list the file on the disk we made: {dir:?}"
        );

        let ready = type_at(&mut m, &mut cpu, b"PCPUT B:EGT8080.COM B\r", 400_000_000);
        println!("--- PCPUT ---\n{ready}");
        assert!(
            !ready.contains("Bad Sector"),
            "the guest's BIOS rejected a sector we formatted: {ready:?}"
        );
        let (got, during) = xmodem_receive_from_guest(&mut m, &mut cpu, 4_000_000_000);
        let got = got
            .unwrap_or_else(|| panic!("the guest never sent it back: {}", printable(&during)));
        assert_eq!(
            &got[..EGT8080_COM.len()],
            EGT8080_COM,
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
    /// A copy of `data_image` with EGT8080 written into it by *our own* writer.
    ///
    /// Extracted because two gates need the same disk and only one of them used
    /// to make it. The other asked for `CPM_BOOT_IMAGE` to be "an Altair CP/M
    /// image carrying EGT8080.COM" — an image nothing produces and nothing keeps,
    /// since the test that writes one deletes it again. So that gate could not
    /// be run at all, which is the quiet way a test stops being evidence: it is
    /// listed, it is never green, and nobody notices because `#[ignore]` hides
    /// both states equally.
    ///
    /// The payload is EGT8080 because it is byte-exact, to hand, and a genuinely
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
        name[..5].copy_from_slice(b"EGT8080");
        let ext = *b"COM";
        fs.create(0, &name, &ext).expect("creates the file");
        for (rec, chunk) in EGT8080_COM.chunks(128).enumerate() {
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
    /// The payload is EGT8080, which is a genuinely useful thing to put on one of
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
            dir.to_ascii_uppercase().contains("EGT8080"),
            "the guest does not list the file we wrote: {dir:?}"
        );

        // The guest's own reader: this is what a wrong checksum or a wrong
        // block mapping fails.
        let ready = type_at(&mut m, &mut cpu, b"PCPUT B:EGT8080.COM B\r", 400_000_000);
        println!("--- PCPUT ---\n{ready}");
        assert!(
            !ready.contains("Bad Sector"),
            "the guest's BIOS rejected a sector we wrote: {ready:?}"
        );
        let (got, during) = xmodem_receive_from_guest(&mut m, &mut cpu, 4_000_000_000);
        let got = got.unwrap_or_else(|| panic!("the guest never sent it back: {}", printable(&during)));
        assert!(
            got.len() >= EGT8080_COM.len(),
            "got {} bytes back, wrote {}",
            got.len(),
            EGT8080_COM.len()
        );
        assert_eq!(
            &got[..EGT8080_COM.len()],
            EGT8080_COM,
            "what we wrote from the host is not what the guest reads back"
        );
        println!(
            "{} bytes written by the host, read back byte-identical by the guest's own CP/M",
            EGT8080_COM.len()
        );
    }

    /// **Which backspace setting one disk wants, and why the setting exists.**
    ///
    /// The single-disk version of
    /// [`test_survey_backspace_across_every_bootable_image`], for when a
    /// particular disk misbehaves and the question is what it is asking for.
    ///
    /// It deliberately does **not** assert that BS erases. That was this test's
    /// first form, written when three MITS disks had been measured and agreeing,
    /// and it was wrong in the way an over-strong test is always wrong: it
    /// encoded a rule from a sample instead of reporting what the disk does.
    /// The wide survey then found CP/M 1.3, 1.4 and the 1975 build doing the
    /// exact opposite, and this test failed on them — correctly, but as a
    /// failure rather than as the finding it was. What is actually true is that
    /// a disk falls into one of three measured groups, so that is what this
    /// checks and prints.
    ///
    /// Ignored: set `CPM_BOOT_IMAGE` to a bootable image. `HDSK01` (Altair Hard
    /// Disk BASIC) wants `backspace`; z80pack's `cpm14.dsk` wants `rubout`;
    /// z80pack's `cpm22-1.dsk` does not care.
    #[test]
    #[ignore]
    fn test_a_booted_guest_names_the_backspace_setting_it_wants() {
        let Ok(path) = std::env::var("CPM_BOOT_IMAGE") else {
            eprintln!("set CPM_BOOT_IMAGE to a bootable image");
            return;
        };
        let bytes = std::fs::read(&path).unwrap();

        // Type a word, then the key, and see what comes back for the key alone.
        // A fresh machine per key: an editing key changes the line the guest is
        // holding, so measuring both against one boot would let the first answer
        // shape the second.
        let echo_of = |key: u8| -> Vec<u8> {
            let configured = std::env::var("CPM_BOOT_MACHINE")
                .unwrap_or_else(|_| crate::cpm::console::AUTO_MACHINE.to_string());
            let (machine, _why) = crate::cpm::detect::machine_for(&configured, &bytes);
            let mut m = BootMachine::new();
            m.set_machine(&machine);
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

        const ERASE: &[u8] = b"\x08 \x08";
        let bs = echo_of(0x08);
        let del = echo_of(0x7F);
        println!("BS 0x08 -> {bs:02X?}\nDEL 0x7F -> {del:02X?}");
        let wants = match (bs == ERASE, del == ERASE) {
            (true, true) => "either — both keys erase",
            (true, false) => "cpm_boot_backspace = backspace",
            (false, true) => "cpm_boot_backspace = rubout",
            // CP/M 1.x lands here: nothing erases, but the rubout is still its
            // editing key and BS is not — it prints a literal `^H`.
            (false, false) if del.contains(&b'G') => {
                "cpm_boot_backspace = rubout (nothing erases; the rubout is its editing key)"
            }
            (false, false) => "neither erases — read the bytes above",
        };
        println!("this disk wants: {wants}");
        assert!(
            bs == ERASE || del == ERASE || del.contains(&b'G'),
            "neither key erases and DEL is not the rubout either, so this disk \
             does something the survey has not seen: BS={bs:02X?} DEL={del:02X?}"
        );
    }

    /// **What a booted guest can reach of what we mounted for it — measured on
    /// both kinds of boot.**
    ///
    /// The gateway inserts every mounted image at the board slot its drive
    /// letter names, and then it is the *guest's own BIOS* that decides whether such a
    /// drive exists. Those are two different questions and they have two
    /// different answers here, which is why this test runs both cases:
    ///
    /// * **Floppy boot** (`DISK01` = MITS CP/M 2.2, `DISK05` at unit 1) — works.
    ///   `DIR B:` lists the mounted disk's files.
    /// * **Hard disk boot** (`HDSK03` = "63K CP/M 2.2b ver 1.5, For MITS
    ///   88-HDSK") — also works, and **this is the half that used to read the
    ///   other way**. It was recorded here as a BIOS carrying exactly one
    ///   drive, on the strength of `DIR B:` answering `Bdos Err On B: Bad
    ///   Sector` whether or not a disk was mounted. Both readings were true and
    ///   the conclusion was wrong: its B: is the drive's **fixed platter**,
    ///   heads 2 and 3, and our controller stopped at head 1, so the slot was
    ///   never served no matter what was in it. With platters modelled, the
    ///   same BIOS lists the same slot's files.
    ///
    /// The reason it looked settled is worth keeping: the control *was* right —
    /// mounted and unmounted really did give identical output — and a control
    /// can only tell you the guest is not seeing your disk. Which of the two
    /// sides is at fault is a different question, and "the guest's BIOS is
    /// limited" is the comfortable answer to reach for.
    ///
    /// **Still do not "fix" a guest's BIOS.** The disk being right about its
    /// own hardware is the premise of booting; what changed here is *our*
    /// hardware being wrong. The way in and out of a booted disk is still the
    /// virtual modem and the disk's own `PCGET`/`PCPUT` — see
    /// [`test_pcget_pulls_egt80_in_over_the_virtual_modem`].
    ///
    /// Ignored: set `CPM_FLOPPY_BOOT`/`CPM_FLOPPY_MOUNT` to two CP/M floppies
    /// and `CPM_HD_BOOT` to an 88-HDSK CP/M image.
    #[test]
    #[ignore]
    fn test_what_a_booted_guest_reaches_of_its_mounts() {
        /// Boot `boot`, optionally put `second` at unit 1, and run `cmd`.
        fn boot_with(boot: &str, second: Option<&str>, cmd: &[u8]) -> String {
            let bytes = std::fs::read(boot).unwrap();
            let (machine, _why) =
                crate::cpm::detect::machine_for(crate::cpm::console::AUTO_MACHINE, &bytes);
            let mut m = BootMachine::new();
            m.set_machine(&machine);
            m.insert(0, bytes, true).expect("the boot image");
            if let Some(p) = second {
                // Reported, not unwrapped: whether this machine's boards take
                // the disk at all is part of what the test is measuring.
                if let Err(e) = m.insert(1, std::fs::read(p).unwrap(), true) {
                    return format!("<unit 1 refused: {e}>");
                }
            }
            let mut cpu = BootMachine::new_cpu();
            m.boot(&mut cpu, 0).expect("boots");
            assert!(
                !run_until_quiet(&mut m, &mut cpu, 200_000_000).is_empty(),
                "{boot} never signed on"
            );
            type_at(&mut m, &mut cpu, cmd, 400_000_000)
        }

        if let (Ok(boot), Ok(mount)) =
            (std::env::var("CPM_FLOPPY_BOOT"), std::env::var("CPM_FLOPPY_MOUNT"))
        {
            let dir = boot_with(&boot, Some(&mount), b"DIR B:\r");
            println!("--- floppy boot, floppy at unit 1 ---\n{dir}");
            assert!(
                dir.contains("B: "),
                "a floppy-booted CP/M must reach a mounted floppy at B:, got {dir:?}"
            );
        } else {
            eprintln!("set CPM_FLOPPY_BOOT and CPM_FLOPPY_MOUNT for the floppy case");
        }

        let Ok(hd) = std::env::var("CPM_HD_BOOT") else {
            eprintln!("set CPM_HD_BOOT for the hard-disk case");
            return;
        };
        let stat = boot_with(&hd, None, b"STAT\r");
        println!("--- hard disk boot, STAT ---\n{stat}");
        assert!(stat.contains("A:"), "the boot drive must be there: {stat:?}");
        assert!(
            !stat.contains("B:"),
            "a bare STAT reports the drives that are logged in, and only A: is \
             until something selects B:. If B: appears here without being asked \
             for, this is no longer measuring what it thinks: {stat:?}"
        );

        // The control that makes the claim mean something: with slot 1 empty
        // the guest's B: is a fault, and with a platter there it is a disk.
        // Both readings come from the same BIOS asking for the same heads, so
        // the difference can only be our platter.
        let empty = boot_with(&hd, None, b"DIR B:\r");
        let filled = boot_with(&hd, Some(&hd), b"DIR B:\r");
        println!("--- hard disk boot, DIR B: ---\nempty:  {empty}\nfilled: {filled}");
        // Not `contains("B: ")` in either direction — `Bdos Err On B: Bad
        // Sector` contains it too, which is the trap this test's own doc
        // comment warns about and which I then walked into.
        assert!(
            empty.contains("Bdos Err"),
            "with the fixed platter absent, B: must be a fault: {empty:?}"
        );
        assert!(
            !filled.contains("Bdos Err") && filled.contains("COM"),
            "an image on the fixed platter must show up at the guest's own B:, \
             which is heads 2 and 3 of the same drive: {filled:?}"
        );

        // And it is genuinely that image being read, not A: answered twice: the
        // same file is on both platters here, so the *set* of names must match
        // even though the BIOS reads the two with different skews (its own
        // source: `HD0SKEW equ 1`, `HD1SKEW equ 13`).
        let a_side = boot_with(&hd, Some(&hd), b"DIR\r");
        // Listing rows only — the echoed command and the `A>` prompt are on
        // lines of their own, and taking them made "DIR" and the drive letters
        // into filenames.
        fn names(s: &str) -> Vec<&str> {
            let mut v: Vec<&str> = s
                .lines()
                .filter(|l| l.starts_with("A: ") || l.starts_with("B: "))
                .flat_map(|l| l[3..].split(':'))
                .flat_map(|f| f.split_whitespace())
                .filter(|w| w.len() <= 8 && w.chars().all(|c| c.is_ascii_alphanumeric()))
                .collect();
            v.sort_unstable();
            v.dedup();
            v
        }
        assert_eq!(
            names(&a_side),
            names(&filled),
            "the same image on both platters must list the same files"
        );
    }

    /// **Altair Hard Disk BASIC's `MOUNT` reaches every platter it was told
    /// about.**
    ///
    /// The gate for the platter model, and it is a gate rather than a unit test
    /// because nothing short of the guest's own operating system can tell the
    /// two models apart. Under the old one — a unit carrying exactly one
    /// platter — every measurement we had still passed: the CP/M hard disk
    /// boots, and its BIOS never asks for a head above 1. This disk does.
    ///
    /// `MOUNT` with no argument mounts every disk the operator declared at
    /// `HIGHEST DISK NUMBER?`, and BASIC numbers its disks by **platter**, so
    /// disk 1 is head 2 of unit 0. With one platter per unit that read as "off
    /// the end of the disk" and `MOUNT` failed with `AFMS I/O ERROR CODE=9F`
    /// while `MOUNT 0` worked — which is exactly how it was reported.
    ///
    /// The oracle is the companion disk's own directory, not a string that
    /// might mean anything: `FILES 1` must list a name that `FILES 0` does not,
    /// which only a genuinely different disk can produce. The control is the
    /// same boot with slot 1 empty, where `MOUNT` must still fail — otherwise
    /// this would pass on a build that quietly served the boot disk twice.
    ///
    /// Ignored: set `CPM_HDSK_BASIC` to `HDSK01` (Altair Hard Disk BASIC) and
    /// `CPM_HDSK_DATA` to another 88-HDSK image.
    #[test]
    #[ignore]
    fn test_hard_disk_basic_mounts_every_platter() {
        let (Ok(basic), Ok(data)) =
            (std::env::var("CPM_HDSK_BASIC"), std::env::var("CPM_HDSK_DATA"))
        else {
            eprintln!("set CPM_HDSK_BASIC and CPM_HDSK_DATA to run this");
            return;
        };

        /// Boot Hard Disk BASIC, declare `highest` as the top disk number, and
        /// run `cmds`. `second` goes in slot 1 — unit 0's second platter.
        ///
        /// The sign-on questions are answered by measurement, not by guessing:
        /// `LINEPRINTER?` rejects `N` and every digit and re-asks, so a blank
        /// line here would loop forever and the failure would look like a hung
        /// machine. `C` is a Centronics printer and is accepted.
        fn boot_basic(basic: &str, second: Option<&str>, highest: &str, cmds: &[&str]) -> String {
            let mut m = BootMachine::new();
            m.insert(0, std::fs::read(basic).unwrap(), true).expect("the hard disk");
            if let Some(p) = second {
                m.insert(1, std::fs::read(p).unwrap(), true).expect("the second platter");
            }
            // 8080, because this is a 1979 MITS disk and the machine it ran on
            // had no Z80 in it.
            let mut cpu = BootMachine::new_cpu_for(crate::cpm::cpu::CPU_8080);
            m.boot(&mut cpu, 0).expect("boots");
            assert!(
                run_until_quiet(&mut m, &mut cpu, 200_000_000).contains(&b'?'),
                "{basic} never asked its first question"
            );
            for answer in ["", "C", highest, "", "1", "1", "79"] {
                type_at(&mut m, &mut cpu, format!("{answer}\r").as_bytes(), 200_000_000);
            }
            let mut out = String::new();
            for c in cmds {
                out.push_str(&type_at(&mut m, &mut cpu, format!("{c}\r").as_bytes(), 800_000_000));
            }
            out
        }

        // The control first: two disks declared, only one platter fitted.
        let alone = boot_basic(&basic, None, "1", &["MOUNT"]);
        assert!(
            alone.contains("ERROR"),
            "with nothing on platter 1, MOUNT must fail — otherwise this gate \
             cannot tell a working second platter from the boot disk served \
             twice: {alone:?}"
        );

        let both = boot_basic(&basic, Some(&data), "1", &["MOUNT", "FILES 0", "FILES 1"]);
        assert!(!both.contains("ERROR"), "MOUNT must reach both platters: {both:?}");

        // Split the two listings and prove the second is a different disk.
        let (zero, one) = both.split_once("FILES 1").expect("both listings");
        let names = |s: &str| -> Vec<String> {
            s.lines().filter_map(|l| l.split_whitespace().next()).map(str::to_string).collect()
        };
        let on_zero = names(zero);
        let only_on_one: Vec<String> =
            names(one).into_iter().filter(|n| !on_zero.contains(n)).collect();
        assert!(
            !only_on_one.is_empty(),
            "FILES 1 listed nothing FILES 0 did not, so the guest may be \
             reading one disk twice:\n{both}"
        );
        println!("platter 1 carries {} files of its own, e.g. {:?}", only_on_one.len(), &only_on_one[..only_on_one.len().min(5)]);
    }

    /// **Does each guest's own operating system reach the disk we mounted at
    /// unit 1 — asked of every image in a folder.**
    ///
    /// [`test_what_a_booted_guest_reaches_of_its_mounts`] pins the two cases we
    /// reasoned about. This is the sweep behind it, and it is a separate
    /// measurement because "the second drive works" is a claim about **each
    /// disk's own BIOS**, not about our controller: a MITS floppy CP/M reaches
    /// four units, the 88-HDSK CP/M reaches the drive's fixed platter as its
    /// B:, and a disk that is not CP/M at all has no `DIR` to ask with. Only
    /// booting each one can say which of those any particular disk is.
    ///
    /// This sweep is also where a limit of *ours* can hide as a limit of
    /// theirs. It read the 88-HDSK CP/M as a one-drive BIOS for months, and it
    /// was not — the board stopped at head 1 and never served the platter that
    /// BIOS was asking for. A guest that cannot see a disk is evidence about
    /// the pair, not about the guest.
    ///
    /// The companion is chosen by measurement, never by filename: the first
    /// other image in the folder that is **the same size** (so whatever board
    /// the booted machine carries accepts it at unit 1) and whose directory our
    /// own reader verifies as a real CP/M filesystem holding a file the boot
    /// disk does *not* have.
    ///
    /// That last condition is what makes the answer exact rather than a score.
    /// The expected names come from the companion's own directory, so "reached
    /// it" means the guest listed a file that is genuinely on that disk and
    /// nowhere else in the machine — not that the string `B:` appeared, which
    /// `Bdos Err On B:` also manages, and not that a name appeared which the
    /// boot disk carries too, which a guest quietly listing A: would produce.
    ///
    /// Ignored — set `CPM_BOOT_DIR` to a folder of `.dsk` files.
    #[test]
    #[ignore]
    fn test_survey_second_drive_across_every_bootable_image() {
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

        /// The files a CP/M `DIR` would list on this image, read by our own
        /// reader, as the `NAMEEXT` tokens the guest's columns collapse to.
        ///
        /// `None` when the image is not a filesystem we can verify — which is
        /// exactly when it is no use as an oracle, since a mis-read directory
        /// would supply names that are not on the disk at all.
        fn dir_tokens(bytes: &[u8]) -> Option<Vec<String>> {
            use crate::cpm::image::{fs::ImageFs, identify::identify, media::MemMedia};
            let id = identify("unnamed.dsk", bytes.len() as u64, |fmt| {
                let mut d = Vec::new();
                for rec in 0..fmt.dir_records() {
                    let off = fmt.data_record_offset(rec)? as usize;
                    d.extend_from_slice(bytes.get(off..off + 128)?);
                }
                Some(d)
            })
            .ok()?;
            if id.force_read_only() {
                return None; // identified by size alone — not proven a CP/M disk
            }
            let fs = ImageFs::mount(Box::new(MemMedia::new(bytes.to_vec())), id.format, true).ok()?;
            let mut t: Vec<String> = fs
                .entries()
                .iter()
                // User 0 and extent 0 are what a plain `DIR` shows; a SYS file
                // is deliberately hidden from it, so it cannot be evidence.
                .filter(|e| e.user == 0 && e.extent == 0 && !e.system)
                .map(|e| {
                    format!(
                        "{}{}",
                        String::from_utf8_lossy(&e.name).trim(),
                        String::from_utf8_lossy(&e.ext).trim()
                    )
                    .to_ascii_uppercase()
                })
                .filter(|n| n.len() > 2 && n.chars().all(|c| c.is_ascii_graphic()))
                .collect();
            t.sort();
            t.dedup();
            Some(t)
        }

        // Reported rather than unwrapped, the lesson the boot survey next door
        // learned the hard way: one file this process cannot read — a
        // permission, a half-copied download — would otherwise panic the run and
        // hide every disk after it, which reads exactly like a regression.
        let read = |n: &str| match std::fs::read(std::path::Path::new(&dir).join(n)) {
            Ok(b) => Some(b),
            Err(e) => {
                println!("  {n:<16} unreadable ({e})");
                None
            }
        };
        let (mut asked, mut reached) = (0u32, 0u32);
        let mut missed: Vec<String> = Vec::new();
        for name in &names {
            let Some(bytes) = read(name) else { continue };
            let own = dir_tokens(&bytes).unwrap_or_default();
            let Some((second, want)) = names.iter().filter(|n| *n != name).find_map(|n| {
                let b = read(n)?;
                if b.len() != bytes.len() {
                    return None; // a different medium — not this machine's unit 1
                }
                let uniq: Vec<String> =
                    dir_tokens(&b)?.into_iter().filter(|f| !own.contains(f)).collect();
                (!uniq.is_empty()).then(|| (n.clone(), uniq))
            }) else {
                println!("  {name:<16} skipped — no same-size companion is a verified CP/M disk");
                continue;
            };

            let (machine, _why) =
                crate::cpm::detect::machine_for(crate::cpm::console::AUTO_MACHINE, &bytes);
            let mut m = BootMachine::new();
            m.set_machine(&machine);
            if let Err(e) = m.insert(0, bytes, true) {
                println!("  {name:<16} skipped — {e}");
                continue;
            }
            let Some(companion) = read(&second) else { continue };
            if let Err(e) = m.insert(1, companion, true) {
                println!("  {name:<16} unit 1 refused {second}: {e}");
                continue;
            }
            let mut cpu = BootMachine::new_cpu_for(&survey_cpu());
            if let Err(e) = m.boot(&mut cpu, 0) {
                println!("  {name:<16} does not boot: {e}");
                continue;
            }
            let signon = printable(&run_until_quiet(&mut m, &mut cpu, 200_000_000));
            if signon.is_empty() {
                println!("  {name:<16} no prompt to ask at");
                continue;
            }
            // Some systems ask something before they will take a command —
            // TDISK01 (CP/M 1.3) opens with `HOW MANY DISKS?`. Typing `DIR B:`
            // into that answers the question with a `D` and reports a working
            // disk as unreachable, so the question is answered first. A bare
            // Return rather than a guess at the answer: it is the one reply
            // that means "whatever you had in mind", and it was measured to
            // leave CP/M 1.3 at `A>` with both drives present.
            let mut note = "";
            if signon.trim_end().ends_with('?') {
                note = " (after answering its startup question)";
                m.send_key(b'\r');
                run_until_quiet(&mut m, &mut cpu, 200_000_000);
            }
            asked += 1;
            let listed = type_at(&mut m, &mut cpu, b"DIR B:\r", 400_000_000);
            let squashed: String = listed
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect::<String>()
                .to_ascii_uppercase();
            let hits: Vec<&String> = want.iter().filter(|f| squashed.contains(*f)).collect();
            if hits.is_empty() {
                missed.push(name.clone());
                println!(
                    "  {name:<16} B: <- {second:<16} NOT reached{note} — {}",
                    listed.replace('\n', " ").trim()
                );
            } else {
                reached += 1;
                println!(
                    "  {name:<16} B: <- {second:<16} reached, {} of its own files (e.g. {}){note}",
                    hits.len(),
                    hits[0]
                );
            }
        }
        println!("\n  {reached} of {asked} guests that reached a prompt saw the disk at unit 1");
        if !missed.is_empty() {
            println!("  did NOT reach unit 1: {missed:?}");
        }
        assert!(asked > 0, "no image in {dir} reached a prompt with a companion mounted");
    }

    /// **What a booted guest actually sends to its printer, byte for byte.**
    ///
    /// The whole `src/cpm/printer.rs` page model rests on one question this
    /// answers and nothing else can: how a period program *ends a line*. If
    /// Altair BASIC terminates with a bare CR then a CR has to advance the
    /// paper, and the overstrike rule — a CR returns the head to column 0 of
    /// the line it is on — would collapse an entire report onto one line. If it
    /// sends CR LF, overstrike is safe and means what WordStar means by it.
    ///
    /// Reasoning cannot settle that: both are period-correct, real interfaces
    /// had an "auto line feed on CR" switch precisely because software
    /// disagreed, and the answer is a property of *this* disk. So this prints
    /// two known lines from the guest's own BASIC and dumps what arrived at the
    /// data port.
    ///
    /// Ignored — set `CPM_HDSK_BASIC` to `HDSK01` (Altair Hard Disk BASIC).
    #[test]
    #[ignore]
    fn test_measure_what_altair_basic_sends_to_the_printer() {
        let Ok(basic) = std::env::var("CPM_HDSK_BASIC") else {
            eprintln!("set CPM_HDSK_BASIC to run this");
            return;
        };

        let mut m = BootMachine::new();
        m.insert(0, std::fs::read(&basic).unwrap(), true).expect("the hard disk");
        // The board the printer lives on, before the guest runs — the same
        // thing `cpm_boot_ui` does from `cpm_printer_port`.
        m.attach_printer(Some(
            crate::cpm::printer::port_for("altair_c").expect("the Altair board").data,
        ));
        // 8080: this is a 1979 MITS disk and the machine it ran on had no Z80.
        let mut cpu = BootMachine::new_cpu_for(crate::cpm::cpu::CPU_8080);
        m.boot(&mut cpu, 0).expect("boots");
        assert!(
            run_until_quiet(&mut m, &mut cpu, 200_000_000).contains(&b'?'),
            "{basic} never asked its first question"
        );
        // `LINEPRINTER?` rejects `N` and every digit and re-asks; `C` is
        // accepted.  Answering it is the point — this is the dialog that makes
        // the guest drive the port at all.
        for answer in ["", "C", "0", "", "1", "1", "79"] {
            type_at(&mut m, &mut cpu, format!("{answer}\r").as_bytes(), 200_000_000);
        }
        // Everything sent while answering the sign-on: the driver's own
        // initialisation, which must not read as a document.
        let handshake = m.take_print();

        for line in ["LPRINT \"ALPHA\"", "LPRINT \"BETA\""] {
            type_at(&mut m, &mut cpu, format!("{line}\r").as_bytes(), 800_000_000);
        }
        let printed = m.take_print();

        let show = |b: &[u8]| -> String {
            b.iter()
                .map(|&c| match c {
                    b'\r' => "<CR>".to_string(),
                    b'\n' => "<LF>".to_string(),
                    0x0C => "<FF>".to_string(),
                    0x20..=0x7E => (c as char).to_string(),
                    _ => format!("<{c:02X}>"),
                })
                .collect()
        };
        println!("sign-on wrote to the data port: {}", show(&handshake));
        println!("LPRINT wrote to the data port:  {}", show(&printed));

        assert!(
            !printed.is_empty(),
            "nothing reached the printer port — the guest never drove it, so \
             everything `printer.rs` says about this disk is unmeasured"
        );
        assert!(
            printed.iter().any(|&b| b & 0x7F == b'A'),
            "the text did not arrive: {}",
            show(&printed)
        );

        // **The measurement, pinned.**  What came back was
        // `ALPHA<CR>BETA<CR>` — a bare CR and no line feed anywhere — which is
        // why `altair_c` carries `auto_lf: true`.  Asserted rather than
        // described, so the day a disk disagrees this says so instead of the
        // gateway quietly printing every report on one line.
        let text: Vec<u8> = printed.iter().map(|&b| b & 0x7F).collect();
        assert!(
            !text.contains(&b'\n'),
            "this disk now sends a line feed, so `altair_c`'s auto-line-feed \
             switch would double-space it — see PrinterPort::auto_lf: {}",
            show(&printed)
        );
        assert!(
            text.windows(6).any(|w| w == b"ALPHA\r"),
            "expected ALPHA terminated by a bare CR: {}",
            show(&printed)
        );
        assert!(
            text.windows(5).any(|w| w == b"BETA\r"),
            "expected BETA terminated by a bare CR: {}",
            show(&printed)
        );

        // The handshake, likewise: it is bytes, it is not a document, and
        // `SpoolJob::is_empty` exists because of it.
        assert!(!handshake.is_empty(), "the driver initialisation was not seen at all");
        let mut job = crate::cpm::printer::SpoolJob::with_auto_lf(
            crate::cpm::printer::port_for("altair_c").expect("the board").auto_lf,
        );
        for &b in &handshake {
            job.push(b);
        }
        assert!(
            job.is_empty(),
            "the sign-on handshake ({}) reads as a print job, so merely \
             answering LINEPRINTER? would hand the operator an empty document",
            show(&handshake)
        );

        // And end to end: the board's own switch, through the real spool, gives
        // two lines rather than BETA printed on top of ALPHA.
        for &b in &printed {
            job.push(b);
        }
        let doc = job.plain_text();
        // CRLF: what CP/M's own text is, and what a vintage machine collecting
        // this file over XMODEM needs -- see `SpoolJob::plain_text`.
        assert_eq!(
            doc, "ALPHA\r\nBETA\r\n",
            "the page model does not reproduce what this disk printed"
        );
    }

    /// **Ask a booted disk's own BIOS for its disk parameter block.**
    ///
    /// The mount side needs five numbers a disk image does not carry anywhere
    /// an outsider can read: block size, directory entries, EXM, the reserved
    /// system tracks, and whether logical sectors are translated. Every one of
    /// them is in the DPB the disk's *own* BIOS hands to CP/M, so the way to
    /// learn them is to ask it — a declaration, the same class of evidence
    /// [`crate::cpm::detect`] reads out of a boot loader's port operands, and
    /// the only authority available here: `cpmtools` has no Cromemco
    /// definition to cross-check against.
    ///
    /// The route is CP/M 2.2's own: page zero's warm-boot vector names the
    /// BIOS, `SELDSK` at BIOS+27 returns the disk parameter *header*, and the
    /// DPB address sits at DPH+10. `XLT` at DPH+0 is the sector translate
    /// table, or zero when the disk does not translate at all.
    ///
    /// Ignored — set `CPM_DPB_IMAGE` to a bootable image.
    #[test]
    #[ignore]
    fn test_measure_dpb_of_a_booted_disk() {
        let Ok(path) = std::env::var("CPM_DPB_IMAGE") else {
            eprintln!("set CPM_DPB_IMAGE to run this");
            return;
        };
        let bytes = std::fs::read(&path).expect("the image");
        let (machine, _why) =
            crate::cpm::detect::machine_for(crate::cpm::console::AUTO_MACHINE, &bytes);
        let mut m = BootMachine::new();
        m.set_machine(&machine);
        m.insert(0, bytes, true).expect("the boot image");
        // A companion volume, when the drive being asked about is not the boot
        // disk -- a hard disk, say, whose own parameters live in the BIOS of the
        // system disk that uses it.  `CPM_DPB_SLOT` is the controller slot and
        // `CPM_DPB_DRIVE` the number handed to SELDSK; they are separate because
        // what a slot *is* belongs to the board.
        if let Ok(second) = std::env::var("CPM_DPB_SECOND") {
            let slot: u8 = std::env::var("CPM_DPB_SLOT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(1);
            m.insert(slot, std::fs::read(&second).expect("the companion"), true)
                .unwrap_or_else(|e| panic!("slot {slot} refused {second}: {e}"));
        }
        let mut cpu = BootMachine::new_cpu_for(&survey_cpu());
        m.boot(&mut cpu, 0).expect("boots");
        let banner = printable(&run_until_quiet(&mut m, &mut cpu, 400_000_000));
        assert!(!banner.trim().is_empty(), "{path} never signed on");
        println!("--- {path} on {machine} ---\n{banner}");

        // Page zero: `JMP WBOOT` at 0000h, so the word at 0001h is BIOS+3.
        let wboot = u16::from_le_bytes([m.peek(1), m.peek(2)]);
        let bios = wboot.wrapping_sub(3);
        let seldsk = bios.wrapping_add(27);
        println!("BIOS base {bios:#06x}, SELDSK {seldsk:#06x}");

        // Call SELDSK(C = drive 0, E = 0 meaning "not logged in yet, do the
        // full job").  The guest is idle at its prompt, so its stack is valid;
        // we push a return address of our own and run until it comes back.
        const SENTINEL: u16 = 0x0040; // CP/M's reserved scratch area: never executed
        let sp = cpu.registers().get16(iz80::Reg16::SP).wrapping_sub(2);
        cpu.registers().set16(iz80::Reg16::SP, sp);
        m.poke(sp, SENTINEL as u8);
        m.poke(sp.wrapping_add(1), (SENTINEL >> 8) as u8);
        let drive: u8 = std::env::var("CPM_DPB_DRIVE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        cpu.registers().set8(iz80::Reg8::C, drive);
        cpu.registers().set8(iz80::Reg8::E, 0);
        cpu.registers().set_pc(seldsk);
        let mut steps = 0u64;
        while cpu.registers().pc() != SENTINEL {
            m.step(&mut cpu);
            steps += 1;
            assert!(steps < 5_000_000, "SELDSK never returned");
        }
        let dph = cpu.registers().get16(iz80::Reg16::HL);
        assert_ne!(dph, 0, "SELDSK refused drive {drive}");

        let word = |m: &mut BootMachine, a: u16| {
            u16::from_le_bytes([m.peek(a), m.peek(a.wrapping_add(1))])
        };
        let xlt = word(&mut m, dph);
        let dpb = word(&mut m, dph.wrapping_add(10));
        println!("DPH {dph:#06x}  XLT {xlt:#06x}  DPB {dpb:#06x}");

        let spt = word(&mut m, dpb);
        let bsh = m.peek(dpb.wrapping_add(2));
        let blm = m.peek(dpb.wrapping_add(3));
        let exm = m.peek(dpb.wrapping_add(4));
        let dsm = word(&mut m, dpb.wrapping_add(5));
        let drm = word(&mut m, dpb.wrapping_add(7));
        let al0 = m.peek(dpb.wrapping_add(9));
        let al1 = m.peek(dpb.wrapping_add(10));
        let cks = word(&mut m, dpb.wrapping_add(11));
        let off = word(&mut m, dpb.wrapping_add(13));

        let blocksize = 128u32 << bsh;
        println!("\n=== DPB as the disk states it ===");
        println!("  SPT (128-byte records per track) : {spt}");
        println!("  BSH {bsh}  BLM {blm}  => block size {blocksize} bytes");
        println!("  EXM                              : {exm}");
        println!("  DSM (last block number)          : {dsm}  => {} blocks", dsm + 1);
        println!("  DRM (last dir entry)             : {drm}  => {} entries", drm + 1);
        println!("  AL0 {al0:#04x} AL1 {al1:#04x}  CKS {cks}");
        println!("  OFF (reserved tracks)            : {off}");
        println!("  data area                        : {} bytes", (dsm as u64 + 1) * blocksize as u64);

        if xlt == 0 {
            println!("\n  XLT is 0 — this disk does NOT translate sectors (skew 1:1)");
        } else {
            // How long the table is has to be *found*, not assumed. CP/M's SPT
            // counts 128-byte records, but a BIOS that reaches a 512-byte
            // sector by dividing by four translates **sectors**, so its table
            // is a quarter that long. The right length is the one whose entries
            // form a permutation — a table of n distinct values covering 1..=n
            // (or 0..n-1) is not something adjacent bytes do by accident.
            let read = |m: &mut BootMachine, n: usize| -> Vec<u16> {
                (0..n).map(|i| m.peek(xlt.wrapping_add(i as u16)) as u16).collect()
            };
            let mut found = None;
            for n in [spt as usize / 4, spt as usize / 2, spt as usize] {
                if n == 0 {
                    continue;
                }
                let t = read(&mut m, n);
                let mut s = t.clone();
                s.sort_unstable();
                s.dedup();
                let one_based = s.len() == n && s[0] == 1 && *s.last().unwrap() == n as u16;
                let zero_based = s.len() == n && s[0] == 0 && *s.last().unwrap() == n as u16 - 1;
                if one_based || zero_based {
                    println!(
                        "\n  XLT at {xlt:#06x}: {n} entries, a permutation of {}..={}",
                        if one_based { 1 } else { 0 },
                        if one_based { n } else { n - 1 }
                    );
                    println!("  {t:?}");
                    found = Some(n);
                    break;
                }
            }
            if found.is_none() {
                println!("\n  XLT at {xlt:#06x} — no length in {{spt/4, spt/2, spt}} is a permutation");
                println!("  first {} bytes: {:?}", spt.min(64), read(&mut m, spt.min(64) as usize));
            }
        }
    }


    /// **Does our reader produce the same bytes the disk's own CP/M does?**
    ///
    /// The gate for a newly added format, and the only kind of check worth
    /// having here. A directory that parses is *necessary* and nowhere near
    /// sufficient: the Altair mapping sat at "nearly right" through four ruled
    /// out hypotheses precisely because a wrong skew still yields a readable
    /// directory and text files that are still all text. What settles it is an
    /// exact oracle — the guest's own operating system, reading its own file
    /// through its own BIOS, with its own idea of where the sectors are.
    ///
    /// `TYPE` is that oracle: it goes through the guest's BDOS and BIOS end to
    /// end. A skew we got wrong shows up as scrambled 128-byte chunks, in the
    /// right order and the wrong places, which is exactly what the comparison
    /// below cannot miss and a plausibility check would.
    ///
    /// Ignored — set `CPM_ORACLE_IMAGE` to a mountable, bootable image and
    /// `CPM_ORACLE_FILE` to a text file on it (`NAME.EXT`).
    #[test]
    #[ignore]
    fn test_our_reader_matches_the_guests_own_reading() {
        let (Ok(path), Ok(want)) =
            (std::env::var("CPM_ORACLE_IMAGE"), std::env::var("CPM_ORACLE_FILE"))
        else {
            eprintln!("set CPM_ORACLE_IMAGE and CPM_ORACLE_FILE to run this");
            return;
        };
        let bytes = std::fs::read(&path).expect("the image");
        // The disk being *measured* may not be the one that boots: a data volume
        // has no operating system of its own, so it rides as a companion in the
        // machine of a system disk that knows how to read it.
        let measured = std::env::var("CPM_ORACLE_MOUNT").unwrap_or_else(|_| path.clone());
        let drive_prefix = std::env::var("CPM_ORACLE_DRIVE").unwrap_or_default();

        // --- our side: mount the image and read the file out ---------------
        // Identified exactly as the mount path does it, closure and all, so
        // this measures the product's own answer and not a second opinion.
        let p = std::path::Path::new(&measured);
        let mut probe =
            crate::cpm::image::media::FileMedia::open(p, true).expect("open for probing");
        let size = std::fs::metadata(p).expect("the measured image").len();
        let filename = p.file_name().unwrap().to_string_lossy().to_string();
        let ident = crate::cpm::image::identify::identify(&filename, size, |fmt| {
            let mut dir = Vec::with_capacity(fmt.maxdir as usize * 32);
            for rec in 0..fmt.dir_records() {
                let off = fmt.data_record_offset(rec)?;
                let mut buf = [0u8; 128];
                crate::cpm::image::media::Media::read_at(&mut probe, off, &mut buf).ok()?;
                dir.extend_from_slice(&buf);
            }
            (!dir.is_empty()).then_some(dir)
        })
        .expect("the image identifies");
        drop(probe);
        let media = crate::cpm::image::media::FileMedia::open(p, true).expect("open");
        let mut fs = crate::cpm::image::fs::ImageFs::mount(Box::new(media), ident.format, true)
            .expect("mount");
        let (name, ext) = {
            let (n, e) = want.split_once('.').unwrap_or((want.as_str(), ""));
            let mut nb = [b' '; 8];
            let mut eb = [b' '; 3];
            for (i, c) in n.bytes().take(8).enumerate() {
                nb[i] = c.to_ascii_uppercase();
            }
            for (i, c) in e.bytes().take(3).enumerate() {
                eb[i] = c.to_ascii_uppercase();
            }
            (nb, eb)
        };
        let ours = fs
            .read_whole(0, &name, &ext, 8 << 20)
            .expect("read")
            .unwrap_or_else(|| panic!("{want} is not on {path} as far as our reader can see"));
        println!("our reader: {} bytes of {want} via {}", ours.len(), ident.format.token);

        // --- the guest's side: boot it and TYPE the same file --------------
        let (machine, _why) =
            crate::cpm::detect::machine_for(crate::cpm::console::AUTO_MACHINE, &bytes);
        let mut m = BootMachine::new();
        m.set_machine(&machine);
        m.insert(0, bytes, true).expect("the boot image");
        if measured != path {
            let slot: u8 = std::env::var("CPM_ORACLE_SLOT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(1);
            m.insert(slot, std::fs::read(&measured).expect("the companion"), true)
                .unwrap_or_else(|e| panic!("slot {slot} refused {measured}: {e}"));
        }
        let mut cpu = BootMachine::new_cpu_for(&survey_cpu());
        m.boot(&mut cpu, 0).expect("boots");
        assert!(
            !printable(&run_until_quiet(&mut m, &mut cpu, 400_000_000)).trim().is_empty(),
            "{path} never signed on"
        );
        // Raw bytes, not `type_at`: that helper runs the console through
        // `printable`, which turns a tab into a dot — and this file is
        // tab-indented assembler, so comparing its output would have been
        // comparing two different manglings rather than the text.
        for &b in format!("TYPE {drive_prefix}{want}\r").as_bytes() {
            m.send_key(b);
        }
        // Kept draining: `run_until_quiet` returns on the first long silence,
        // and a floppy seek partway through a file is exactly such a silence.
        // Stopping there truncated the guest's side of the comparison and made
        // a correct format look wrong.
        let mut typed_raw = run_until_quiet(&mut m, &mut cpu, 2_000_000_000);
        for _ in 0..8 {
            let more = run_until_quiet(&mut m, &mut cpu, 200_000_000);
            if more.is_empty() {
                break;
            }
            typed_raw.extend_from_slice(&more);
        }
        let typed = String::from_utf8_lossy(&typed_raw).to_string();

        // --- compare -------------------------------------------------------
        //
        // Both sides are normalised the same way and only in ways that are
        // *transport*, never content: CP/M ends a text file with a run of ^Z
        // padding to the record boundary, the console sees CR LF where the file
        // has CR LF, and the guest echoes the command and prints a prompt. Tabs,
        // spacing and the order of the actual text are left completely alone —
        // they are what a wrong skew would disturb.
        // Compared with **all whitespace removed on both sides**, which is
        // exactly as strong a test of the mapping and immune to the two ways a
        // console legitimately differs from a file: CP/M's BDOS expands a tab to
        // the next eight-column stop, and an 80-column console wraps what is one
        // line in the file into two on the screen. Neither changes a single
        // character of content, and a wrong skew changes plenty — it reorders
        // whole 128-byte records, so the character stream itself comes out
        // different. Trailing `^Z` padding is dropped for the same reason.
        let strip = |s: &str| -> String {
            s.chars().filter(|c| !c.is_whitespace() && *c != '\u{1a}').collect()
        };
        let ours_s = strip(&String::from_utf8_lossy(&ours));
        let guest_s = strip(&typed);
        assert!(
            ours_s.len() > 200,
            "{want} is too small to prove anything ({} chars)",
            ours_s.len()
        );
        assert!(
            guest_s.contains(&ours_s),
            "our reading of {want} ({} chars) is not present in the guest's own TYPE output \
             ({} chars). A mismatch here means the skew or the block mapping is wrong — the \
             directory parsing correctly proves nothing.\n  ours begins: {:?}\n  guest has : {:?}",
            ours_s.len(),
            guest_s.len(),
            &ours_s[..80.min(ours_s.len())],
            &guest_s[..160.min(guest_s.len())]
        );
        println!(
            "MATCH: all {} characters of {want} identical between our reader and the \
             guest's own TYPE, via {}",
            ours_s.len(),
            ident.format.token
        );
    }




    /// **A banked CP/M 3 disk boots and takes a command.**
    ///
    /// The gate for the MMU, and it has to be a gate rather than a unit test
    /// because nothing short of a banked operating system exercises the thing.
    /// z80pack's CP/M 3 was the disk that loaded, printed its sign-on and then
    /// stopped dead for months: its banked BIOS selects a memory bank, and with
    /// no MMU the select went nowhere and it carried on executing whatever was
    /// already there.
    ///
    /// Two separate faults had to go for this to pass, and the second is the one
    /// worth remembering: implementing the MMU alone was **not** enough, because
    /// the disk's DMA still wrote straight into bank 0. The guest read its own
    /// directory as empty and retried the same sector for ever — 1,677 status
    /// polls without a single console read.
    ///
    /// Ignored — set `CPM_CPM3_IMAGE` to z80pack's `cpm3-1.dsk`.
    #[test]
    #[ignore]
    fn test_a_banked_cpm3_disk_boots_and_takes_commands() {
        let Ok(path) = std::env::var("CPM_CPM3_IMAGE") else {
            eprintln!("set CPM_CPM3_IMAGE to run this");
            return;
        };
        let bytes = std::fs::read(&path).expect("the image");
        let (machine, _why) =
            crate::cpm::detect::machine_for(crate::cpm::console::AUTO_MACHINE, &bytes);
        let mut m = BootMachine::new();
        m.set_machine(&machine);
        m.insert(0, bytes, true).expect("the boot image");
        let mut cpu = BootMachine::new_cpu_for(&survey_cpu());
        m.boot(&mut cpu, 0).expect("boots");

        let banner = printable(&run_until_quiet(&mut m, &mut cpu, 800_000_000));
        assert!(
            banner.contains("BANKED BIOS"),
            "this disk did not reach its banked BIOS at all: {banner:?}"
        );
        assert!(
            banner.trim_end().ends_with("A>"),
            "reached the BIOS and then stopped, which is what no MMU looks like: {banner:?}"
        );

        // A prompt is not the same as a working machine: the CCP is banked, so
        // running a command is what proves the mapping holds under a bank
        // switch.  DIR reads the directory, which is what the DMA fault broke.
        let dir = type_at(&mut m, &mut cpu, b"DIR\r", 800_000_000);
        assert!(
            dir.to_ascii_uppercase().contains("CPM3") && dir.contains("SYS"),
            "DIR did not list this disk's own system file: {dir:?}"
        );
    }

}
