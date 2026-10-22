//! The CP/M setup wizard on telnet.
//!
//! Every question, recommendation and setting comes from
//! [`crate::cpm::wizard`]; this file only draws it, one question to a screen,
//! inside the 22 rows and 40 PETSCII / 80 ANSI columns every telnet screen
//! keeps to.  Long lists are paged rather than allowed to scroll: a disk
//! picker, a disk's file listing (on a screen of its own, reached with `L`,
//! that returns to the step it was opened from) and the list of disks chosen.
//!
//! `Q` goes back one step everywhere; on the first step it leaves the wizard.
//! Nothing is saved until the last screen's `Y`.

use super::*;
use crate::cpm::wizard::{self, DiskFacts, Plan, Runs, Step};

/// Every screen's row budget, the prompt row included.
const ROWS: usize = 22;

/// What a disk screen was asked.
enum DiskAnswer {
    Use,
    Back,
}

/// Where a step's screen leaves the wizard.
enum Move {
    /// On to the step [`Step::next`] names.
    On,
    /// Back one step.
    Back,
}

impl TelnetSession {
    /// Text columns on this terminal, inside the two-space indent.
    fn wiz_width(&self) -> usize {
        if self.terminal_type == TerminalType::Petscii { wizard::TEXT_WIDTH } else { 76 }
    }

    /// Clear and draw the heading: separator, title, separator, blank -- four
    /// rows, which every screen below counts.
    async fn wiz_heading(&mut self, title: &str) -> Result<(), std::io::Error> {
        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;
        self.send_line(&format!("  {}", self.yellow(&format!("CP/M SETUP: {title}")))).await?;
        self.send_line(&sep).await?;
        self.send_line("").await
    }

    /// Lines of help, two-space indented.
    async fn wiz_text(&mut self, lines: &[&str]) -> Result<(), std::io::Error> {
        for line in lines {
            if line.is_empty() {
                self.send_line("").await?;
            } else {
                self.send_line(&format!("  {line}")).await?;
            }
        }
        Ok(())
    }

    /// The prompt row: the step's keys, then `>`.  One row, as on the mount
    /// screens, so Back never costs a row of its own.
    async fn wiz_prompt(&mut self, keys: &[(&str, &str)]) -> Result<Option<String>, std::io::Error> {
        self.wiz_prompt_digits(keys, false).await
    }

    /// [`Self::wiz_prompt`], choosing whether a digit is a key on its own.
    /// Where the numbers are a list -- a disk, a machine -- they are typed and
    /// ended with Enter, so `12` can be picked; where a digit is one choice
    /// among letters (`8` for the 8080) it has to answer at once, or the
    /// screen sits waiting for an Enter nothing asked for.
    async fn wiz_prompt_digits(
        &mut self,
        keys: &[(&str, &str)],
        instant_digits: bool,
    ) -> Result<Option<String>, std::io::Error> {
        let keys: Vec<String> = keys.iter().map(|(k, d)| self.action_prompt(k, d)).collect();
        self.send(&format!("  {} {}> ", keys.join(" "), self.cyan("wizard"))).await?;
        self.flush().await?;
        self.get_menu_input(instant_digits).await
    }

    /// The CP/M setup wizard: the questions in order, then the settings.
    pub(in crate::telnet) async fn cpm_wizard(&mut self) -> Result<(), std::io::Error> {
        let cfg = config::get_config();
        let base = wizard::cpm_base(&cfg.transfer_dir);

        // Reading every disk can take a few seconds the first time -- whether
        // a disk boots is answered by starting it -- so it is said, and done
        // off the runtime.
        self.wiz_heading("READING DISKS").await?;
        self.send_line("  Looking at the disk images...").await?;
        self.flush().await?;
        let read_base = base.clone();
        let (mut disks, offer) =
            tokio::task::spawn_blocking(move || (wizard::all_disks(&read_base), wizard::sample_offer(&read_base)))
                .await
                .map_err(std::io::Error::other)?;

        let mut plan = Plan::opening(&disks);
        let mut step = Step::first(offer.as_ref());
        let mut trail: Vec<Step> = Vec::new();
        loop {
            // Each arm answers the step; where it goes from there is
            // `Step::next`'s, shared with the other two surfaces.
            let moved = match step {
                // The download screen the mount menu has -- what, how much and
                // from where, then y/N -- since a wizard-styled one with the
                // step's own text comes to more than a screen.  Either answer
                // goes on; the first answer is worked out afresh from the
                // disks now here.
                Step::Fetch => {
                    self.cpmmount_download().await?;
                    let read_base = base.clone();
                    disks = tokio::task::spawn_blocking(move || wizard::all_disks(&read_base))
                        .await
                        .map_err(std::io::Error::other)?;
                    plan = Plan::opening(&disks);
                    Move::On
                }
                // Boot is offered even with nothing here that boots: the
                // operator may have images of their own to put in the folder,
                // and the boot-disk step says where.
                Step::Runs => match self.wiz_runs(wizard::opening_runs(&disks), trail.is_empty()).await? {
                    Some('e') => {
                        plan.choose_runs(Runs::Emulator);
                        Move::On
                    }
                    Some('b') => {
                        plan.choose_runs(Runs::Boot);
                        Move::On
                    }
                    _ => Move::Back,
                },
                Step::EmulatorDrives => {
                    let choices: Vec<&DiskFacts> = disks.iter().filter(|d| d.mountable()).collect();
                    if self.wiz_emulator_drives(&mut plan, &choices, &disks).await? { Move::On } else { Move::Back }
                }
                Step::BootDisk => {
                    let bootable: Vec<&DiskFacts> = disks.iter().filter(|d| d.boots).collect();
                    if bootable.is_empty() {
                        self.wiz_no_boot_disk().await?;
                        Move::Back
                    } else {
                        match self.wiz_pick_boot_disk(&bootable).await? {
                            Some(i) => {
                                plan.choose_boot_disk(bootable[i]);
                                Move::On
                            }
                            None => Move::Back,
                        }
                    }
                }
                Step::Machine => {
                    let disk = boot_disk(&disks, &plan);
                    match self.wiz_machine(&disk).await? {
                        Some(m) => {
                            plan.choose_machine(&m, &disk);
                            Move::On
                        }
                        None => Move::Back,
                    }
                }
                Step::Cpu => {
                    let (cpu, why) = wizard::advised_cpu(plan.runs, plan.boot_in(&disks));
                    match self.wiz_cpu(cpu, why).await? {
                        Some(c) => {
                            plan.cpu = c;
                            Move::On
                        }
                        None => Move::Back,
                    }
                }
                Step::Rom => match self.wiz_rom().await? {
                    Some(yes) => {
                        plan.choose_rom(yes, &boot_disk(&disks, &plan));
                        Move::On
                    }
                    None => Move::Back,
                },
                Step::ExtraDisks => {
                    let disk = boot_disk(&disks, &plan);
                    let machine = plan.machine_in_force(&disk);
                    let choices = wizard::extra_disk_choices(&disk, &machine, &disks);
                    let room = wizard::extra_disk_room(&disk, &machine);
                    let on = self.wiz_extra_disks(&mut plan, &disk, &machine, &choices, room, &disks).await?;
                    if on { Move::On } else { Move::Back }
                }
                Step::Ready => match self.wiz_summary(&plan, &cfg.transfer_dir).await? {
                    Some(true) => {
                        let p = plan.clone();
                        let dir = cfg.transfer_dir.clone();
                        let outcome = tokio::task::spawn_blocking(move || wizard::apply(&p, &dir))
                            .await
                            .map_err(std::io::Error::other)?;
                        self.wiz_done(outcome).await?;
                        return Ok(());
                    }
                    _ => Move::Back,
                },
            };
            match moved {
                Move::On => {
                    if let Some(to) = step.next(plan.runs, plan.needs_rom(&disks)) {
                        trail.push(step);
                        step = to;
                    }
                }
                Move::Back => match trail.pop() {
                    Some(prev) => step = prev,
                    None => return Ok(()),
                },
            }
        }
    }

    /// Step 1: the emulator, or a disk of its own -- both always offered,
    /// the [`wizard::opening_runs`] one marked.  `first`: nothing before it,
    /// so `Q` cancels rather than going back to the download.
    async fn wiz_runs(&mut self, advised: Runs, first: bool) -> Result<Option<char>, std::io::Error> {
        loop {
            self.wiz_heading("WHAT RUNS").await?;
            self.wiz_text(wizard::RUNS_HELP).await?;
            self.send_line("").await?;
            let star = |r: Runs| if r == advised { " (advised)" } else { "" };
            self.send_line(&format!("  {}  {}{}", self.cyan("E"), wizard::EMULATOR_LABEL, star(Runs::Emulator))).await?;
            self.send_line(&format!("  {}  {}{}", self.cyan("B"), wizard::BOOT_LABEL, star(Runs::Boot))).await?;
            self.send_line("").await?;
            let Some(input) = self.wiz_prompt(&[("Q", if first { "Cancel" } else { "Back" })]).await? else {
                return Ok(None);
            };
            match input.as_str() {
                "e" => return Ok(Some('e')),
                "b" => return Ok(Some('b')),
                "q" | "" => return Ok(None),
                _ => {}
            }
        }
    }

    /// Boot was chosen and no disk here boots: where the operator's own go.
    async fn wiz_no_boot_disk(&mut self) -> Result<(), std::io::Error> {
        self.wiz_heading("BOOT DISK").await?;
        self.wiz_text(wizard::NO_BOOT_DISK).await?;
        self.send_line("").await?;
        self.send("  Press any key to go back.").await?;
        self.flush().await?;
        self.wait_for_key().await?;
        Ok(())
    }

    /// A disk: what it is, the start of its listing, and `L` for all of it.
    async fn wiz_disk(
        &mut self,
        disk: &DiskFacts,
        use_label: &str,
        for_boot: bool,
    ) -> Result<DiskAnswer, std::io::Error> {
        loop {
            let w = self.wiz_width();
            self.wiz_heading("A DISK").await?;
            let mut used = 4;
            self.send_line(&format!("  {}", self.amber(&truncate_to_width(&safe(&disk.name), w)))).await?;
            used += 1;
            for line in crate::aichat::wrap_line(&disk.summary, w).into_iter().take(2) {
                self.send_line(&format!("  {}", self.dim(&line))).await?;
                used += 1;
            }
            if for_boot {
                let m = truncate_to_width(crate::cpm::console::machine_label(&disk.machine), w - 5);
                self.send_line(&format!("  For: {m}")).await?;
                used += 1;
            }
            if disk.altair_basic {
                self.send_line(&format!("  {}", self.amber(wizard::BASIC_NOTE))).await?;
                used += 1;
            }
            if disk.needs_rom {
                self.send_line(&format!("  {}", self.amber(wizard::ROM_NOTE))).await?;
                used += 1;
            }
            self.send_line("").await?;
            used += 1;
            // Below: blank, two option rows, the prompt.
            let room = ROWS.saturating_sub(used + 4);
            let listing = terminal_safe(&disk.listing());
            let grid = grid_rows(&listing, w);
            let shown = grid.len().min(room);
            let cut = shown < grid.len();
            let shown = if cut { room.saturating_sub(1) } else { shown };
            for row in &grid[..shown] {
                self.send_line(&format!("  {}", self.dim(row))).await?;
            }
            if cut {
                let more = listing.len() - shown * grid_columns(&listing, w);
                self.send_line(&format!("  {}", self.dim(&format!("...and {more} more"))))
                    .await?;
            }
            self.send_line("").await?;
            self.send_line(&format!("  {}  {}", self.cyan("U"), use_label)).await?;
            self.send_line(&format!(
                "  {}  Show the whole listing ({} line{})",
                self.cyan("L"),
                listing.len(),
                if listing.len() == 1 { "" } else { "s" }
            ))
            .await?;
            let Some(input) = self.wiz_prompt(&[("Q", "Back")]).await? else {
                return Ok(DiskAnswer::Back);
            };
            match input.as_str() {
                "u" => return Ok(DiskAnswer::Use),
                "l" => self.wiz_listing(disk).await?,
                "q" | "" => return Ok(DiskAnswer::Back),
                _ => {}
            }
        }
    }

    /// A disk's whole listing, paged, on its own screen.  Returns to whoever
    /// opened it, which redraws where the operator was.
    async fn wiz_listing(&mut self, disk: &DiskFacts) -> Result<(), std::io::Error> {
        let w = self.wiz_width();
        let listing = terminal_safe(&disk.listing());
        let grid = grid_rows(&listing, w);
        // Heading 4, the name, a blank above the nav row, the nav row.
        let per_page = ROWS - 4 - 1 - 1 - 1;
        let pages = grid.len().div_ceil(per_page).max(1);
        let mut page = 0;
        loop {
            self.wiz_heading("DISK LISTING").await?;
            self.send_line(&format!(
                "  {}  {}",
                self.amber(&truncate_to_width(&safe(&disk.name), w.saturating_sub(12))),
                self.dim(&format!("page {}/{}", page + 1, pages))
            ))
            .await?;
            let start = page * per_page;
            for row in grid.iter().skip(start).take(per_page) {
                self.send_line(&format!("  {row}")).await?;
            }
            self.send_line("").await?;
            let mut keys = Vec::new();
            if page > 0 {
                keys.push(("P", "Prev"));
            }
            if page + 1 < pages {
                keys.push(("N", "Next"));
            }
            keys.push(("Q", "Back"));
            let Some(input) = self.wiz_prompt(&keys).await? else {
                return Ok(());
            };
            match input.as_str() {
                "n" if page + 1 < pages => page += 1,
                "p" if page > 0 => page -= 1,
                "q" | "" => return Ok(()),
                _ => {}
            }
        }
    }

    /// A paged list of disks; the number picked, as an index into `disks`.
    async fn wiz_pick_from(
        &mut self,
        title: &str,
        help: &[&str],
        disks: &[&DiskFacts],
    ) -> Result<Option<usize>, std::io::Error> {
        let w = self.wiz_width();
        // Heading 4, the help, the page line, blank, the rows, blank, prompt.
        let per_page = ROWS - 4 - help.len() - 1 - 1 - 1 - 1;
        let pages = disks.len().div_ceil(per_page).max(1);
        let mut page = 0;
        loop {
            self.wiz_heading(title).await?;
            self.wiz_text(help).await?;
            self.send_line(&format!(
                "  {}",
                self.dim(&format!("Pick a disk by number. Page {}/{}", page + 1, pages))
            ))
            .await?;
            self.send_line("").await?;
            let start = page * per_page;
            for (i, d) in disks.iter().enumerate().skip(start).take(per_page) {
                let num = format!("{:>2}", i + 1);
                let name_w = 14.min(w.saturating_sub(4));
                let rest = w.saturating_sub(3 + name_w + 1);
                let about = d.summary.split(" -- ").nth(1).unwrap_or("");
                self.send_line(&format!(
                    "  {} {:<name_w$} {}",
                    self.cyan(&num),
                    truncate_to_width(&safe(&d.name), name_w),
                    self.dim(&truncate_to_width(about, rest)),
                ))
                .await?;
            }
            self.send_line("").await?;
            let mut keys = Vec::new();
            if page > 0 {
                keys.push(("P", "Prev"));
            }
            if page + 1 < pages {
                keys.push(("N", "Next"));
            }
            keys.push(("Q", "Back"));
            let Some(input) = self.wiz_prompt(&keys).await? else {
                return Ok(None);
            };
            match input.as_str() {
                "n" if page + 1 < pages => page += 1,
                "p" if page > 0 => page -= 1,
                "q" | "" => return Ok(None),
                other => {
                    if let Ok(n) = other.trim().parse::<usize>()
                        && (1..=disks.len()).contains(&n)
                    {
                        return Ok(Some(n - 1));
                    }
                }
            }
        }
    }

    /// The boot disk: a picker of the disks seen to start, then the disk.
    /// The index of the one chosen.
    async fn wiz_pick_boot_disk(&mut self, bootable: &[&DiskFacts]) -> Result<Option<usize>, std::io::Error> {
        loop {
            let Some(i) = self.wiz_pick_from("BOOT DISK", wizard::BOOT_DISK_HELP, bootable).await? else {
                return Ok(None);
            };
            if let DiskAnswer::Use = self.wiz_disk(bootable[i], "Boot this disk", true).await? {
                return Ok(Some(i));
            }
        }
    }

    /// The emulator's drives: images on letters B: to P:, optional.
    /// `true` to go on, `false` to go back.
    async fn wiz_emulator_drives(
        &mut self,
        plan: &mut Plan,
        choices: &[&DiskFacts],
        all: &[DiskFacts],
    ) -> Result<bool, std::io::Error> {
        loop {
            let rows: Vec<String> =
                plan.disks.iter().map(|(d, n)| format!("{}: {n}", (b'A' + d) as char)).collect();
            let free = (1..crate::cpm::NUM_DRIVES).filter(|d| !plan.disks.iter().any(|(x, _)| x == d)).count();
            match self
                .wiz_assignments("DRIVES", wizard::EMULATOR_DRIVES_HELP, &rows, free, !choices.is_empty())
                .await?
            {
                Some('a') => {
                    let Some(i) = self.wiz_pick_from("ADD A DISK", &["Disks with CP/M files on them."], choices).await? else {
                        continue;
                    };
                    let disk = choices[i];
                    if let DiskAnswer::Use = self.wiz_disk(disk, "Put this disk on a drive", false).await?
                        && let Some(d) = self.wiz_drive_letter(plan).await?
                    {
                        plan.disks.retain(|(x, n)| *x != d && *n != disk.name);
                        plan.disks.push((d, disk.name.clone()));
                        plan.disks.sort();
                    }
                }
                Some('r') => self.wiz_remove(plan).await?,
                Some('n') => {
                    if self.wiz_checked(plan, all).await? {
                        return Ok(true);
                    }
                }
                _ => return Ok(false),
            }
        }
    }

    /// Which letter, B: to P:.
    async fn wiz_drive_letter(&mut self, plan: &Plan) -> Result<Option<u8>, std::io::Error> {
        loop {
            self.wiz_heading("WHICH DRIVE").await?;
            self.send_line("  Type the drive letter, B to P.").await?;
            self.send_line("  A: is the terminals' drive.").await?;
            self.send_line("").await?;
            let taken: Vec<String> =
                plan.disks.iter().map(|(d, _)| format!("{}:", (b'A' + d) as char)).collect();
            if !taken.is_empty() {
                let w = self.wiz_width();
                let t = truncate_to_width(&format!("In use: {} (replaced)", taken.join(" ")), w);
                self.send_line(&format!("  {}", self.dim(&t))).await?;
                self.send_line("").await?;
            }
            let Some(input) = self.wiz_prompt(&[("Q", "Back")]).await? else {
                return Ok(None);
            };
            let c = input.trim().chars().next().unwrap_or(' ').to_ascii_uppercase();
            if input.trim().len() == 1 && ('B'..='P').contains(&c) {
                return Ok(Some(c as u8 - b'A'));
            }
            if matches!(input.as_str(), "q" | "") {
                return Ok(None);
            }
        }
    }

    /// Take one disk out of the plan.
    async fn wiz_remove(&mut self, plan: &mut Plan) -> Result<(), std::io::Error> {
        if plan.disks.is_empty() {
            return Ok(());
        }
        let w = self.wiz_width();
        self.wiz_heading("REMOVE A DISK").await?;
        // Heading 4, the rows, blank, the prompt: sixteen fit.
        for (i, (_, n)) in plan.disks.iter().enumerate().take(ROWS - 6) {
            self.send_line(&format!("  {} {}", self.cyan(&format!("{:>2}", i + 1)), truncate_to_width(&safe(n), w - 3)))
                .await?;
        }
        self.send_line("").await?;
        if let Some(input) = self.wiz_prompt(&[("Q", "Back")]).await?
            && let Ok(n) = input.trim().parse::<usize>()
            && (1..=plan.disks.len()).contains(&n)
        {
            plan.disks.remove(n - 1);
        }
        Ok(())
    }

    /// The disks chosen so far for drives or slots, and A/R/N.  With none
    /// chosen the step's explanation fills the space the list will use.
    async fn wiz_assignments(
        &mut self,
        title: &str,
        help: &[&str],
        rows: &[String],
        room: usize,
        can_add: bool,
    ) -> Result<Option<char>, std::io::Error> {
        let w = self.wiz_width();
        self.wiz_heading(title).await?;
        // Heading 4, the room line, blank, three option rows, the prompt.
        let space = ROWS - 4 - 1 - 1 - 3 - 1;
        if rows.is_empty() {
            self.wiz_text(&help[..help.len().min(space)]).await?;
        } else {
            let cut = rows.len() > space;
            let shown = if cut { space - 1 } else { rows.len() };
            for r in &rows[..shown] {
                self.send_line(&format!("  {}", truncate_to_width(&safe(r), w))).await?;
            }
            if cut {
                self.send_line(&format!("  {}", self.dim(&format!("...and {} more", rows.len() - shown))))
                    .await?;
            }
        }
        self.send_line(&format!(
            "  {}",
            self.dim(&format!("Room for {room} more. Q goes back."))
        ))
        .await?;
        self.send_line("").await?;
        if can_add && room > 0 {
            self.send_line(&format!("  {}  Add a disk", self.cyan("A"))).await?;
        } else {
            self.send_line(&format!("  {}", self.dim("(no more disks can be added)"))).await?;
        }
        self.send_line(&format!("  {}  Remove one", self.cyan("R"))).await?;
        self.send_line(&format!("  {}  Next step", self.cyan("N"))).await?;
        let Some(input) = self.wiz_prompt(&[("Q", "Back")]).await? else {
            return Ok(None);
        };
        Ok(match input.as_str() {
            "a" if can_add && room > 0 => Some('a'),
            "r" => Some('r'),
            "n" => Some('n'),
            "q" | "" => None,
            _ => Some('?'),
        })
    }

    /// The disks to ride along with the boot disk, slot by slot, up to the
    /// board's room.  `true` to go on, `false` to go back.
    async fn wiz_extra_disks(
        &mut self,
        plan: &mut Plan,
        boot: &DiskFacts,
        machine: &str,
        choices: &[&DiskFacts],
        room: u8,
        all: &[DiskFacts],
    ) -> Result<bool, std::io::Error> {
        // Every slot's name with the board built once, not once a row.
        let names = wizard::slot_names(boot, machine);
        loop {
            let rows: Vec<String> = plan
                .disks
                .iter()
                .map(|(s, n)| {
                    let at = names.get(*s as usize).cloned().unwrap_or_else(|| format!("slot {s}"));
                    format!("{at}: {n}")
                })
                .collect();
            let left = (room as usize).saturating_sub(plan.disks.len());
            let free: Vec<&&DiskFacts> =
                choices.iter().filter(|d| !plan.disks.iter().any(|(_, n)| *n == d.name)).collect();
            match self.wiz_assignments("MORE DISKS", wizard::EXTRA_DISKS_HELP, &rows, left, !free.is_empty()).await? {
                Some('a') => {
                    let free: Vec<&DiskFacts> = free.into_iter().copied().collect();
                    let Some(i) = self.wiz_pick_from("ADD A DISK", &["Disks the same board can read."], &free).await? else {
                        continue;
                    };
                    let disk = free[i];
                    if let DiskAnswer::Use = self.wiz_disk(disk, "Add it in the next slot", false).await? {
                        // The next slot, so there is never a gap: an empty slot
                        // between two full ones looks like a hang to a guest
                        // that selects it.
                        let slot = (1..=room).find(|s| !plan.disks.iter().any(|(x, _)| x == s));
                        if let Some(slot) = slot {
                            plan.disks.push((slot, disk.name.clone()));
                        }
                    }
                }
                Some('r') => {
                    self.wiz_remove(plan).await?;
                    // Closed up again, for the same reason.
                    for (i, d) in plan.disks.iter_mut().enumerate() {
                        d.0 = i as u8 + 1;
                    }
                }
                Some('n') => {
                    if self.wiz_checked(plan, all).await? {
                        return Ok(true);
                    }
                }
                Some(_) => {}
                None => return Ok(false),
            }
        }
    }

    /// The machine a disk boots on: the detected one, or another that takes it.
    async fn wiz_machine(&mut self, disk: &DiskFacts) -> Result<Option<String>, std::io::Error> {
        let choices = wizard::machine_choices(disk);
        loop {
            let w = self.wiz_width();
            self.wiz_heading("MACHINE").await?;
            self.wiz_text(wizard::MACHINE_HELP).await?;
            self.send_line("").await?;
            for (i, m) in choices.iter().enumerate() {
                let mark = if m.key == disk.machine { " *" } else { "" };
                let label = truncate_to_width(m.description, w.saturating_sub(5 + mark.len()));
                self.send_line(&format!("  {}  {label}{}", self.cyan(&(i + 1).to_string()), self.green(mark)))
                    .await?;
            }
            self.send_line(&format!("  {}", self.dim("* is what the disk was read as."))).await?;
            self.send_line("").await?;
            let Some(input) = self.wiz_prompt(&[("K", "Keep *"), ("Q", "Back")]).await? else {
                return Ok(None);
            };
            match input.as_str() {
                "k" => return Ok(Some(crate::cpm::console::AUTO_MACHINE.to_string())),
                "q" | "" => return Ok(None),
                other => {
                    if let Ok(n) = other.trim().parse::<usize>()
                        && let Some(m) = choices.get(n.wrapping_sub(1))
                    {
                        // `Plan::choose_machine` keeps the detected one as
                        // `auto`.
                        return Ok(Some(m.key.to_string()));
                    }
                }
            }
        }
    }

    /// The processor, with the recommendation and its reason.
    async fn wiz_cpu(&mut self, advised: &str, why: &str) -> Result<Option<String>, std::io::Error> {
        loop {
            let w = self.wiz_width();
            self.wiz_heading("PROCESSOR").await?;
            self.wiz_text(wizard::CPU_HELP).await?;
            self.send_line("").await?;
            use crate::cpm::cpu::{CPU_CHOICES, choice_name};
            let advised_name = CPU_CHOICES.iter().find(|(v, _)| *v == advised).map_or(advised, |(_, l)| choice_name(l));
            let line = format!("Advised: {advised_name}, {why}.");
            for l in crate::aichat::wrap_line(&line, w).into_iter().take(2) {
                self.send_line(&format!("  {}", self.amber(&l))).await?;
            }
            self.send_line("").await?;
            // Every processor the settings take, keyed by its name's first
            // character: `Z` for the Z80, `8` for the 8080.
            let key = |l: &str| choice_name(l).chars().next().unwrap_or('?').to_ascii_lowercase();
            for (v, l) in CPU_CHOICES {
                let star = if *v == advised { " (advised)" } else { "" };
                let k = key(l).to_ascii_uppercase().to_string();
                self.send_line(&format!("  {}  {}{star}", self.cyan(&k), choice_name(l))).await?;
            }
            self.send_line("").await?;
            let Some(input) = self.wiz_prompt_digits(&[("Q", "Back")], true).await? else {
                return Ok(None);
            };
            if matches!(input.as_str(), "q" | "") {
                return Ok(None);
            }
            if let Some((v, _)) = CPU_CHOICES.iter().find(|(_, l)| input.chars().eq([key(l)])) {
                return Ok(Some(v.to_string()));
            }
        }
    }

    /// The monitor ROM, for the one disk measured to need it.
    async fn wiz_rom(&mut self) -> Result<Option<bool>, std::io::Error> {
        loop {
            self.wiz_heading("MONITOR ROM").await?;
            self.wiz_text(wizard::ROM_HELP).await?;
            self.send_line("").await?;
            self.send_line(&format!("  {}  Load the ROM (advised)", self.cyan("Y"))).await?;
            self.send_line(&format!("  {}  No ROM", self.cyan("N"))).await?;
            self.send_line("").await?;
            let Some(input) = self.wiz_prompt(&[("Q", "Back")]).await? else {
                return Ok(None);
            };
            match input.as_str() {
                "y" => return Ok(Some(true)),
                "n" => return Ok(Some(false)),
                "q" | "" => return Ok(None),
                _ => {}
            }
        }
    }

    /// [`wizard::check_plan`] at a disks step's Next: `true` when the plan
    /// passes, else the reason is shown until a key is pressed.  The pickers
    /// here offer only what may be chosen, so this is the same rule the save
    /// will apply, said while the operator can still change the answer.
    async fn wiz_checked(&mut self, plan: &Plan, all: &[DiskFacts]) -> Result<bool, std::io::Error> {
        let Err(e) = wizard::check_plan(plan, all) else {
            return Ok(true);
        };
        let w = self.wiz_width();
        self.wiz_heading("NOT YET").await?;
        for l in crate::aichat::wrap_line(&safe(&e), w).into_iter().take(4) {
            self.send_line(&format!("  {}", self.red(&l))).await?;
        }
        self.send_line("").await?;
        self.send("  Press any key to continue.").await?;
        self.flush().await?;
        self.wait_for_key().await?;
        Ok(false)
    }

    /// The last question: these settings?  `Some(true)` to save.
    async fn wiz_summary(&mut self, plan: &Plan, transfer_dir: &str) -> Result<Option<bool>, std::io::Error> {
        let w = self.wiz_width();
        let dir = transfer_dir.to_string();
        let p = plan.clone();
        let lines = tokio::task::spawn_blocking(move || p.describe(&dir))
            .await
            .map_err(std::io::Error::other)?;
        loop {
            self.wiz_heading("READY").await?;
            // Heading 4, the lines, blank, two note rows, blank, Y row, prompt.
            let space = ROWS - 4 - 1 - 2 - 1 - 1 - 1;
            let cut = lines.len() > space;
            let shown = if cut { space - 1 } else { lines.len() };
            for l in &lines[..shown] {
                self.send_line(&format!("  {}", self.amber(&truncate_to_width(&safe(l), w)))).await?;
            }
            if cut {
                self.send_line(&format!("  {}", self.dim(&format!("...and {} more", lines.len() - shown))))
                    .await?;
            }
            self.send_line("").await?;
            self.send_line(&format!("  {}", self.dim("Disks mounted now and not listed"))).await?;
            self.send_line(&format!("  {}", self.dim("here are taken off."))).await?;
            self.send_line("").await?;
            self.send_line(&format!("  {}  Save these settings", self.cyan("Y"))).await?;
            let Some(input) = self.wiz_prompt(&[("Q", "Back")]).await? else {
                return Ok(None);
            };
            match input.as_str() {
                "y" => return Ok(Some(true)),
                "q" | "" => return Ok(None),
                _ => {}
            }
        }
    }

    /// Saved: what to do next, and anything that did not go as planned.
    async fn wiz_done(&mut self, outcome: Result<Vec<String>, String>) -> Result<(), std::io::Error> {
        let w = self.wiz_width();
        self.wiz_heading("DONE").await?;
        // Heading 4, two result rows, blank, the notes, blank, finish, blank, key.
        let space = ROWS - 4 - 2 - 1 - 1 - 1 - 1 - 1;
        match outcome {
            Ok(notes) => {
                self.send_line(&format!("  {}", self.green("Saved."))).await?;
                self.send_line("").await?;
                self.send_line("").await?;
                let lines: Vec<String> = notes
                    .iter()
                    .flat_map(|n| crate::aichat::wrap_line(&crate::aichat::display_for_terminal(n), w))
                    .collect();
                for l in lines.iter().take(space) {
                    self.send_line(&format!("  {}", self.dim(l))).await?;
                }
                // Only once saved: after "Not saved" it would send the
                // operator to boot the configuration they came to replace.
                self.send_line("").await?;
                self.send_line(&format!("  {}", self.yellow(wizard::FINISH_LINE))).await?;
            }
            Err(e) => {
                for l in crate::aichat::wrap_line(&format!("Not saved: {}", safe(&e)), w).into_iter().take(2) {
                    self.send_line(&format!("  {}", self.red(&l))).await?;
                }
            }
        }
        self.send_line("").await?;
        self.send("  Press any key to continue.").await?;
        self.flush().await?;
        self.wait_for_key().await
    }
}

/// The plan's boot disk, as read.  The plan names one only once it has been
/// picked from `disks`, so it is there; a stand-in keeps a later step from
/// panicking if the folder changed under the wizard.
fn boot_disk(disks: &[DiskFacts], plan: &Plan) -> DiskFacts {
    plan.boot_in(disks).cloned().unwrap_or_else(|| DiskFacts {
        name: plan.boot_disk.clone().unwrap_or_default(),
        bytes: 0,
        boots: true,
        files: None,
        summary: String::new(),
        machine: crate::cpm::console::AUTO_MACHINE.to_string(),
        machine_why: None,
        board: None,
        slots: None,
        altair_basic: false,
        needs_rom: false,
    })
}

/// A disk image's name, or a line naming one, as a terminal may be sent it.
/// Names are the operator's files, and a file can be called anything a
/// filesystem allows -- an ESC included.
fn safe(s: &str) -> String {
    crate::aichat::display_for_terminal(s)
}

/// A disk's listing as a terminal may be sent it.  The names are the disk's own
/// directory bytes, and the disks come from the internet: a name holding an
/// ESC would otherwise reach the terminal as a live escape sequence.
fn terminal_safe(listing: &[String]) -> Vec<String> {
    listing.iter().map(|l| crate::aichat::display_for_terminal(l)).collect()
}

/// How many listing columns fit `width`: each as wide as the longest entry and
/// two spaces.  A single column when entries are long (the notes a disk with no
/// CP/M filesystem shows instead of files).
fn grid_columns(listing: &[String], width: usize) -> usize {
    let cell = listing.iter().map(|s| s.chars().count()).max().unwrap_or(1) + 2;
    (width / cell).max(1)
}

/// The listing laid out in rows of columns, filled across.
fn grid_rows(listing: &[String], width: usize) -> Vec<String> {
    let cols = grid_columns(listing, width);
    let cell = listing.iter().map(|s| s.chars().count()).max().unwrap_or(1) + 2;
    listing
        .chunks(cols)
        .map(|row| {
            let mut line = String::new();
            for (i, s) in row.iter().enumerate() {
                if i + 1 < row.len() {
                    line.push_str(&format!("{s:<cell$}"));
                } else {
                    line.push_str(s);
                }
            }
            truncate_to_width(&line, width)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telnet::TerminalType;

    /// The bytes `draw` sends answering `keys`.
    fn sent<F>(term: TerminalType, keys: &[u8], draw: F) -> Vec<u8>
    where
        F: AsyncFnOnce(&mut TelnetSession),
    {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let (mut session, peer) = crate::telnet::tests::make_test_session_with_peer(term);
            let (mut pr, mut pw) = tokio::io::split(peer);
            pw.write_all(keys).await.unwrap();
            let run = async move { draw(&mut session).await };
            let mut out = Vec::new();
            let (_, r) = tokio::join!(run, pr.read_to_end(&mut out));
            r.unwrap();
            out
        })
    }

    /// Render what `draw` sends answering `keys`, as the rows a terminal holds.
    fn rendered<F>(term: TerminalType, keys: &[u8], draw: F) -> Vec<String>
    where
        F: AsyncFnOnce(&mut TelnetSession),
    {
        let mut out = sent(term, keys, draw);
        if term == TerminalType::Petscii {
            out = out
                .iter()
                .filter(|&&b| matches!(b, b'\r' | b'\n' | 0x20..=0x7F | 0xA0..=0xFF))
                .map(|&b| b as char)
                .collect::<String>()
                .into_bytes();
        }
        crate::telnet::tests::render_rows(&out)
    }

    /// Split a render into screens at each clear (an ESC [2J on ANSI), and
    /// measure each: rows and the widest row.
    fn screens(term: TerminalType, keys: &[u8], draw: impl AsyncFnOnce(&mut TelnetSession)) -> Vec<Vec<String>> {
        // PETSCII has no clear code that survives the render, so the separator
        // row is where each screen begins.
        let rows = rendered(term, keys, draw);
        let mut out: Vec<Vec<String>> = Vec::new();
        let mut cur: Vec<String> = Vec::new();
        let mut seps = 0;
        for r in rows {
            if r.starts_with("=====") {
                seps += 1;
                if seps % 2 == 1 && !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            cur.push(r);
        }
        out.push(cur);
        out.into_iter()
            .map(|s| {
                let s: Vec<String> = s.into_iter().skip_while(|r| r.trim().is_empty()).collect();
                let end = s.iter().rposition(|r| !r.trim().is_empty()).map_or(0, |i| i + 1);
                s[..end].to_vec()
            })
            .filter(|s| !s.is_empty())
            .collect()
    }

    fn fits(term: TerminalType, what: &str, screens: &[Vec<String>]) {
        // 39 on a C64: a 40th column wraps and the CR after it costs a row.
        let cols = if term == TerminalType::Petscii { 39 } else { 80 };
        assert!(!screens.is_empty(), "{what}: nothing drawn");
        for s in screens {
            assert!(s.len() <= ROWS, "{term:?} {what}: {} rows\n{s:#?}", s.len());
            for r in s {
                assert!(r.chars().count() <= cols, "{term:?} {what}: {r:?} is {} columns", r.chars().count());
            }
        }
    }

    /// A disk with a long listing and every flag up: the worst a disk screen
    /// and the listing pages carry.
    fn worst_disk() -> DiskFacts {
        DiskFacts {
            name: "A-VERY-LONG-DISK-IMAGE-NAME-INDEED.DSK".into(),
            bytes: 4_988_928,
            boots: true,
            files: Some((0..345).map(|i| format!("FILE{i:04}.COM  (user {})", i % 16)).collect()),
            summary: "boots -- CP/M 2.2, the Altair hard-disk exerciser, BASIC, development tools, 345 files".into(),
            machine: "altair_2sio".into(),
            machine_why: None,
            board: Some("MITS 88-HDSK hard disk"),
            slots: Some(16),
            altair_basic: true,
            needs_rom: true,
        }
    }

    const TERMS: [TerminalType; 2] = [TerminalType::Petscii, TerminalType::Ansi];

    #[test]
    fn test_the_first_step_fits() {
        for term in TERMS {
            for advised in [Runs::Boot, Runs::Emulator] {
                for first in [true, false] {
                    let s = screens(term, b"q", async move |t| {
                        t.wiz_runs(advised, first).await.unwrap();
                    });
                    fits(term, "what runs", &s);
                    // Either case: PETSCII swaps it on the wire.
                    let all = s.concat().join("\n").to_ascii_lowercase();
                    assert!(all.contains(&wizard::BOOT_LABEL.to_ascii_lowercase()), "{term:?}: Boot is not offered");
                    assert!(all.contains(&wizard::EMULATOR_LABEL.to_ascii_lowercase()), "{term:?}: the emulator is not offered");
                    assert_eq!(all.matches("(advised)").count(), 1, "{term:?}: not one answer marked advised");
                }
            }
        }
    }

    #[test]
    fn test_the_no_boot_disk_screen_fits() {
        for term in TERMS {
            let s = screens(term, b" ", async move |t| {
                t.wiz_no_boot_disk().await.unwrap();
            });
            fits(term, "no boot disk", &s);
        }
    }

    /// The disk screen with the worst disk, and its listing paged to the end
    /// and back -- every page inside the budget.
    #[test]
    fn test_a_disk_and_its_listing_fit() {
        for term in TERMS {
            let d = worst_disk();
            let mut keys = b"l".to_vec();
            keys.extend(std::iter::repeat_n(b'n', 40));
            keys.extend(b"pqq");
            let s = screens(term, &keys, async move |t| {
                t.wiz_disk(&d, "Boot this disk", true).await.unwrap();
            });
            fits(term, "disk and listing", &s);
            assert!(s.len() >= 4, "{term:?}: the listing was not paged ({} screens)", s.len());
            // And the last screen is the disk again: Back returned to it.
            assert!(
                s.last().unwrap().iter().any(|r| r.to_ascii_lowercase().contains("cp/m setup: a disk")),
                "{term:?}: the listing did not come back to the disk"
            );
        }
    }

    /// Every entry in the listing is on some page, once.
    #[test]
    fn test_the_listing_pages_show_every_file() {
        let d = worst_disk();
        for w in [wizard::TEXT_WIDTH, 76] {
            let grid = grid_rows(&d.listing(), w);
            let text = grid.join(" ");
            for f in d.listing() {
                let name = f.split("  ").next().unwrap();
                assert!(text.contains(name), "{name} is on no page at width {w}");
            }
            assert!(grid.iter().all(|r| r.chars().count() <= w));
        }
    }

    /// A picker over many disks pages, and each page fits.
    #[test]
    fn test_a_long_disk_picker_fits() {
        let many: Vec<DiskFacts> = (0..40)
            .map(|i| DiskFacts { name: format!("DISK{i:02}-WITH-A-LONG-NAME.DSK"), ..worst_disk() })
            .collect();
        for term in TERMS {
            let refs: Vec<DiskFacts> = many.clone();
            let s = screens(term, b"nnnpq", async move |t| {
                let r: Vec<&DiskFacts> = refs.iter().collect();
                t.wiz_pick_from("BOOT DISK", wizard::BOOT_DISK_HELP, &r).await.unwrap();
            });
            fits(term, "picker", &s);
        }
    }

    /// The machine, processor, ROM, summary and done screens.
    #[test]
    fn test_the_other_steps_fit() {
        for term in TERMS {
            let d = worst_disk();
            let s = screens(term, b"q", async move |t| {
                t.wiz_machine(&d).await.unwrap();
            });
            fits(term, "machine", &s);
            let s = screens(term, b"q", async move |t| {
                t.wiz_cpu("8080", "Altair BASIC was written for the 8080").await.unwrap();
            });
            fits(term, "cpu", &s);
            let s = screens(term, b"q", async move |t| {
                t.wiz_rom().await.unwrap();
            });
            fits(term, "rom", &s);
            let mut plan = Plan::boot(&worst_disk());
            plan.disks = (1..16).map(|s| (s, format!("ANOTHER-RATHER-LONG-NAME-{s:02}.DSK"))).collect();
            let s = screens(term, b"q", async move |t| {
                t.wiz_summary(&plan, "/nonexistent").await.unwrap();
            });
            fits(term, "summary", &s);
            let notes: Vec<String> = (0..12).map(|i| format!("note {i} that goes on for quite some way to wrap")).collect();
            let s = screens(term, b" ", async move |t| {
                t.wiz_done(Ok(notes)).await.unwrap();
            });
            fits(term, "done", &s);
            // Case-blind: PETSCII swaps case on the wire.
            assert!(
                s[0].iter().any(|r| r.to_ascii_lowercase().contains("main menu")),
                "{term:?}: the K instruction is missing"
            );
        }
    }

    /// "Not saved" never ends by sending the operator to K: that would boot
    /// the configuration they came to replace.  And the reason, which can
    /// name an image, is made safe for the terminal.
    #[test]
    fn test_a_failed_save_does_not_say_press_k() {
        for term in TERMS {
            let s = screens(term, b" ", async move |t| {
                t.wiz_done(Err("BAD\x1b[2JNAME.DSK is not a disk this board reads.".into())).await.unwrap();
            });
            fits(term, "not saved", &s);
            let text = s.concat().join("\n").to_ascii_lowercase();
            assert!(text.contains("not saved"), "{term:?}: {text}");
            assert!(!text.contains("main menu"), "{term:?}: a failed save sends the operator to K");
        }
        // An OSC title from a file name would retitle the operator's window.
        let raw = sent(TerminalType::Ansi, b" ", async move |t| {
            t.wiz_done(Err("BAD\x1b]0;OWNED\x07NAME.DSK".into())).await.unwrap();
        });
        assert!(!raw.windows(3).any(|w| w == b"\x1b]0"), "an escape in a name reached the terminal");
        assert!(raw.windows(5).any(|w| w == b"OWNED"), "positive control: the reason was sent");
    }

    /// The drives step full: sixteen rows of disks, and the step still fits.
    #[test]
    fn test_a_full_assignment_list_fits() {
        for term in TERMS {
            let rows: Vec<String> = (1..16).map(|s| format!("unit {}.{}: A-LONG-DISK-IMAGE-NAME-{s:02}.DSK", s / 4, s % 4)).collect();
            let s = screens(term, b"q", async move |t| {
                t.wiz_assignments("MORE DISKS", wizard::EXTRA_DISKS_HELP, &rows, 0, true).await.unwrap();
            });
            fits(term, "assignments", &s);
            let s = screens(term, b"q", async move |t| {
                t.wiz_assignments("DRIVES", wizard::EMULATOR_DRIVES_HELP, &[], 15, true).await.unwrap();
            });
            fits(term, "assignments empty", &s);
        }
    }
}
