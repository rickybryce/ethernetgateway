//! The telnet mount/unmount wizard for CP/M disk images.
//!
//! Reached from the CP/M settings screen.  Deliberately a short sequence of
//! single-question screens rather than one dense form: this is driven from a
//! 40-column PETSCII terminal as often as from an 80-column one, and a screen
//! that asks one thing fits both.
//!
//! Mount is three questions — which image, which drive, and a confirmation that
//! spells out what mounting *does* — because it is the destructive-looking one.
//! Nothing is actually destroyed: the drive folder's files are hidden, not
//! touched, and they come back on unmount.  Saying so at the point of decision
//! is the difference between a feature people use and one they are afraid of.

use super::*;
use crate::cpm::image;

impl TelnetSession {
    /// The `CPM/` container this gateway is configured for.
    pub(in crate::telnet) fn cpmmount_base(&self) -> std::path::PathBuf {
        let cfg = config::get_config();
        crate::cpm::layout::cpm_dir(&cfg.transfer_dir)
    }

    /// Entry point: mount or unmount?
    pub(in crate::telnet) async fn cpm_mount_wizard(&mut self) -> Result<(), std::io::Error> {
        loop {
            let mounts = image::registry::all();
            let usage = image::registry::usage();

            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            self.send_line(&format!("  {}", self.yellow("CP/M DISK IMAGES")))
                .await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;

            // What is mounted right now, so the operator sees the state before
            // being asked to change it.
            let lent = image::registry::boot_loans();
            let any = mounts.iter().any(|m| m.is_some()) || !lent.is_empty();
            if any {
                self.send_line("  Mounted:").await?;
                for (i, m) in mounts.iter().enumerate() {
                    let Some(m) = m else { continue };
                    let letter = (b'A' + i as u8) as char;
                    let ro = if m.read_only { " (R/O)" } else { "" };
                    let busy = usage
                        .get(i)
                        .and_then(|u| u.describe())
                        .map(|d| format!(" - {d}"))
                        .unwrap_or_default();
                    let width = if self.terminal_type == TerminalType::Petscii { 28 } else { 60 };
                    self.send_line(&format!(
                        "   {}: {}{}{}",
                        self.cyan(&letter.to_string()),
                        self.amber(&truncate_to_width(&m.filename, width)),
                        ro,
                        self.dim(&busy),
                    ))
                    .await?;
                }
                for (drive0, name) in &lent {
                    let letter = (b'A' + drive0) as char;
                    let width = if self.terminal_type == TerminalType::Petscii { 20 } else { 52 };
                    self.send_line(&format!(
                        "   {}: {} {}",
                        self.cyan(&letter.to_string()),
                        self.amber(&truncate_to_width(name, width)),
                        self.dim("(booted)"),
                    ))
                    .await?;
                }
            } else {
                self.send_line(&format!("  {}", self.dim("No images mounted.")))
                    .await?;
            }
            self.send_line("").await?;

            self.send_line(&format!("  {}  Mount an image", self.cyan("M")))
                .await?;
            self.send_line(&format!("  {}  Boot an image (runs its own OS)", self.cyan("B")))
                .await?;
            self.send_line(&format!("  {}  New blank disk", self.cyan("N")))
                .await?;
            if any {
                self.send_line(&format!("  {}  Unmount a drive", self.cyan("U")))
                    .await?;
            }
            self.send_line("").await?;
            self.send_line(&format!("  {}", self.action_prompt("Q", "Back")))
                .await?;

            let prompt = format!("{}> ", self.cyan("ethernet/config/cpm/disks"));
            self.send(&prompt).await?;
            self.flush().await?;

            match self.get_menu_input(false).await? {
                Some(s) if s == "m" => self.cpmmount_pick_image().await?,
                Some(s) if s == "b" => self.cpmmount_pick_boot().await?,
                Some(s) if s == "n" => self.cpmmount_new_blank().await?,
                Some(s) if s == "u" && any => self.cpmmount_pick_unmount().await?,
                Some(s) if !s.is_empty() => {}
                _ => return Ok(()),
            }
        }
    }

    /// Make a new, empty, formatted disk image.
    ///
    /// Two questions: which format, and what to call it.  The file is named
    /// `<format>_<name>.dsk` for the operator rather than typed out, because
    /// the prefix is what lets the image mount read-write — a disk you just
    /// created and cannot write to would be a puzzle, not a feature.
    async fn cpmmount_new_blank(&mut self) -> Result<(), std::io::Error> {
        let formats = image::creatable_formats();
        if formats.is_empty() {
            return Ok(());
        }

        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;
        self.send_line(&format!("  {}", self.yellow("NEW BLANK DISK"))).await?;
        self.send_line(&sep).await?;
        self.send_line("").await?;
        self.send_line(&format!("  {}", self.dim("An empty, formatted disk for files."))).await?;
        self.send_line("").await?;
        let width = if self.terminal_type == TerminalType::Petscii { 30 } else { 60 };
        for (i, (_, label)) in formats.iter().enumerate() {
            self.send_line(&format!(
                "  {}  {}",
                self.cyan(&(i + 1).to_string()),
                self.amber(&truncate_to_width(label, width)),
            ))
            .await?;
        }
        self.send_line("").await?;
        self.send_line(&format!("  {}", self.action_prompt("Q", "Back"))).await?;
        self.send(&format!("{}> ", self.cyan("format"))).await?;
        self.flush().await?;

        let Some(input) = self.get_menu_input(false).await? else {
            return Ok(());
        };
        let Ok(n) = input.trim().parse::<usize>() else {
            return Ok(());
        };
        let Some((token, _)) = formats.get(n.wrapping_sub(1)) else {
            return Ok(());
        };

        self.send_line("").await?;
        self.send_line(&format!(
            "  {}",
            self.dim("A short name. The file becomes")
        ))
        .await?;
        self.send_line(&format!("  {}", self.dim("<format>_<name>.dsk"))).await?;
        self.send(&format!("  {}: ", self.cyan("Disk name"))).await?;
        self.flush().await?;
        let Some(name) = self.get_line_input().await? else {
            return Ok(());
        };
        if name.trim().is_empty() {
            return Ok(());
        }

        let base = self.cpmmount_base();
        let token = token.to_string();
        let result = tokio::task::spawn_blocking(move || {
            image::create_blank_image(&base, &token, &name)
        })
        .await
        .unwrap_or_else(|e| Err(format!("create failed: {e}")));

        self.send_line("").await?;
        match result {
            Ok(note) => {
                glog!("CP/M: {}", note);
                self.send_line(&format!("  {}", self.green(&note))).await?;
                self.send_line(&format!(
                    "  {}",
                    self.dim("Mount it with M to start using it.")
                ))
                .await?;
            }
            Err(e) => self.send_line(&format!("  {}", self.red(&e))).await?,
        }
        self.send_line("").await?;
        self.send("  Press any key to continue.").await?;
        self.flush().await?;
        let _ = self.wait_for_key().await;
        Ok(())
    }

    /// Choose an image to boot.
    ///
    /// Booting is not mounting, and the screen says so: a booted disk runs its
    /// own operating system and owns the hardware, so it is a different thing
    /// from putting an image on drive B:.  The mounted images do come along —
    /// each at the unit its drive letter names — but what they are *called*
    /// then belongs to the guest, and so does how many of them it can reach.
    async fn cpmmount_pick_boot(&mut self) -> Result<(), std::io::Error> {
        let base = self.cpmmount_base();
        let images = crate::cpm::image::available_images(&base);
        let bootable: Vec<String> = images
            .into_iter()
            .filter(|n| {
                std::fs::metadata(crate::cpm::image::images_dir(&base).join(n))
                    .ok()
                    .and_then(|m| crate::cpm::boot_machine::geometry_for(m.len()))
                    .is_some()
            })
            .collect();

        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;
        self.send_line(&format!("  {}", self.yellow("BOOT A DISK IMAGE"))).await?;
        self.send_line(&sep).await?;
        self.send_line("").await?;
        if bootable.is_empty() {
            self.send_line(&format!("  {}", self.amber("No bootable images found."))).await?;
            self.send_line("").await?;
            self.send_line("  Only Altair 88-DCDD floppies can boot:").await?;
            self.send_line("  337,568 bytes (8in) or 76,720 (mini).").await?;
            self.send_line("").await?;
            self.send("  Press any key to continue.").await?;
            self.flush().await?;
            let _ = self.wait_for_key().await;
            return Ok(());
        }
        self.send_line(&format!("  {}", self.dim("The disk runs its OWN operating"))).await?;
        self.send_line(&format!("  {}", self.dim("system. Its drive FOLDERS do not"))).await?;
        self.send_line(&format!("  {}", self.dim("apply; mounted images do, each on"))).await?;
        self.send_line(&format!("  {}", self.dim("the unit its letter names."))).await?;
        self.send_line("").await?;
        let width = if self.terminal_type == TerminalType::Petscii { 30 } else { 60 };
        for (i, n) in bootable.iter().take(Self::TRANSFER_PAGE_SIZE).enumerate() {
            self.send_line(&format!(
                "  {}  {}",
                self.cyan(&(i + 1).to_string()),
                self.amber(&truncate_to_width(n, width))
            ))
            .await?;
        }
        self.send_line("").await?;
        self.send_line(&format!("  {}", self.action_prompt("Q", "Back"))).await?;
        self.send(&format!("{}> ", self.cyan("boot"))).await?;
        self.flush().await?;

        let Some(input) = self.get_menu_input(false).await? else {
            return Ok(());
        };
        let Ok(n) = input.trim().parse::<usize>() else {
            return Ok(());
        };
        let Some(name) = bootable.get(n.wrapping_sub(1)) else {
            return Ok(());
        };
        let path = crate::cpm::image::images_dir(&base).join(name);
        // Bring `cpm_mounts` up before booting.  Mounting is otherwise lazy —
        // it happens when a session first enters the emulator — so booting
        // straight from this picker on a fresh gateway would hand the guest a
        // machine with one disk in it and no sign that the rest were meant to
        // be there.
        self.cpmemu_ensure_drives().await?;

        // Writing is a decision, not a default: a booted guest writes raw
        // sectors and nothing above it would notice a mistake.
        self.send_line("").await?;
        self.send(&format!("  Allow writes to this image? {}: ", self.cyan("y/N")))
            .await?;
        self.flush().await?;
        let ans = self.get_menu_input(false).await?.unwrap_or_default();
        let writable = ans.starts_with('y');

        self.cpm_boot_session(&path, writable).await
    }

    /// Unmount: list what is mounted, take one off.
    async fn cpmmount_pick_unmount(&mut self) -> Result<(), std::io::Error> {
        let mounts = image::registry::all();
        let usage = image::registry::usage();
        let listed: Vec<(u8, String, bool)> = mounts
            .iter()
            .enumerate()
            .filter_map(|(i, m)| m.as_ref().map(|m| (i as u8, m.filename.clone(), m.read_only)))
            .collect();
        if listed.is_empty() {
            return Ok(());
        }

        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;
        self.send_line(&format!("  {}", self.yellow("UNMOUNT A DRIVE")))
            .await?;
        self.send_line(&sep).await?;
        self.send_line("").await?;
        for (n, (drive0, name, ro)) in listed.iter().enumerate() {
            let letter = (b'A' + drive0) as char;
            let busy = usage
                .get(*drive0 as usize)
                .and_then(|u| u.describe())
                .map(|d| format!("  [{d}]"))
                .unwrap_or_default();
            let width = if self.terminal_type == TerminalType::Petscii { 22 } else { 48 };
            self.send_line(&format!(
                "  {}  {}: {}{}{}",
                self.cyan(&(n + 1).to_string()),
                letter,
                self.amber(&truncate_to_width(name, width)),
                if *ro { " (R/O)" } else { "" },
                self.dim(&busy),
            ))
            .await?;
        }
        self.send_line("").await?;
        self.send_line(&format!(
            "  {}",
            self.dim("The drive folder's files come back")
        ))
        .await?;
        self.send_line("").await?;
        self.send_line(&format!("  {}", self.action_prompt("Q", "Back")))
            .await?;
        self.send(&format!("{}> ", self.cyan("unmount"))).await?;
        self.flush().await?;

        let Some(input) = self.get_menu_input(false).await? else {
            return Ok(());
        };
        let Ok(n) = input.trim().parse::<usize>() else {
            return Ok(());
        };
        let Some((drive0, name, _)) = listed.get(n.wrapping_sub(1)) else {
            return Ok(());
        };

        match image::unmount_drive(*drive0) {
            Ok(note) => {
                self.cpmmount_persist();
                self.cpmmount_report(&note, false).await?;
            }
            Err(e) => self.cpmmount_report(&e, true).await?,
        }
        let _ = name;
        Ok(())
    }

    /// Mount, step 1: a paginated list of the images folder.
    async fn cpmmount_pick_image(&mut self) -> Result<(), std::io::Error> {
        let base = self.cpmmount_base();
        let images = image::available_images(&base);
        if images.is_empty() {
            self.clear_screen().await?;
            self.send_line("").await?;
            self.send_line(&format!("  {}", self.amber("No disk images found.")))
                .await?;
            self.send_line("").await?;
            self.send_line("  Put .dsk files in the transfer dir").await?;
            self.send_line("  under CPM/images, then try again.").await?;
            self.send_line("").await?;
            self.send_line(&format!("  {}", self.dim("readme.txt there explains the names.")))
                .await?;
            self.send_line("").await?;
            self.send("  Press any key to continue.").await?;
        self.flush().await?;
        let _ = self.wait_for_key().await;
            return Ok(());
        }

        let mut page: usize = 0;
        loop {
            let total_pages = images.len().div_ceil(Self::TRANSFER_PAGE_SIZE).max(1);
            if page >= total_pages {
                page = total_pages - 1;
            }
            let offset = page * Self::TRANSFER_PAGE_SIZE;
            let end = (offset + Self::TRANSFER_PAGE_SIZE).min(images.len());
            let shown = &images[offset..end];

            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            self.send_line(&format!("  {}", self.yellow("CHOOSE A DISK IMAGE")))
                .await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;
            let width = if self.terminal_type == TerminalType::Petscii { 30 } else { 60 };
            for (i, name) in shown.iter().enumerate() {
                self.send_line(&format!(
                    "  {}  {}",
                    self.cyan(&(i + 1).to_string()),
                    self.amber(&truncate_to_width(name, width)),
                ))
                .await?;
            }
            self.send_line("").await?;
            self.send_line(&format!("  Page {} of {}", page + 1, total_pages))
                .await?;
            self.send_line("").await?;
            let mut nav = Vec::new();
            if page > 0 {
                nav.push(self.action_prompt("P", "Prev"));
            }
            if page + 1 < total_pages {
                nav.push(self.action_prompt("N", "Next"));
            }
            nav.push(self.action_prompt("Q", "Back"));
            self.send_line(&format!("  {}", nav.join("  "))).await?;
            self.send(&format!("{}> ", self.cyan("image"))).await?;
            self.flush().await?;

            let Some(input) = self.get_menu_input(false).await? else {
                return Ok(());
            };
            match input.as_str() {
                "p" => page = page.saturating_sub(1),
                "n" => {
                    if page + 1 < total_pages {
                        page += 1;
                    }
                }
                "q" | "" => return Ok(()),
                other => {
                    if let Ok(n) = other.trim().parse::<usize>() {
                        if let Some(name) = shown.get(n.wrapping_sub(1)) {
                            let name = name.clone();
                            if self.cpmmount_pick_drive(&name).await? {
                                return Ok(());
                            }
                        }
                    }
                }
            }
        }
    }

    /// Mount, step 2: which drive?  Returns true when a mount happened.
    async fn cpmmount_pick_drive(&mut self, filename: &str) -> Result<bool, std::io::Error> {
        let mounts = image::registry::all();
        let usage = image::registry::usage();

        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;
        self.send_line(&format!("  {}", self.yellow("CHOOSE A DRIVE")))
            .await?;
        self.send_line(&sep).await?;
        self.send_line("").await?;
        let width = if self.terminal_type == TerminalType::Petscii { 30 } else { 60 };
        self.send_line(&format!("  {}", self.amber(&truncate_to_width(filename, width))))
            .await?;
        self.send_line("").await?;

        // Drives in use cannot be changed, and are shown saying why rather than
        // silently missing — a drive that vanished from the list would read as
        // a bug.
        for i in 0..crate::cpm::NUM_DRIVES {
            let letter = (b'A' + i) as char;
            let busy = usage.get(i as usize).and_then(|u| u.describe());
            let held = mounts.get(i as usize).and_then(|m| m.as_ref());
            let mut note = String::new();
            if let Some(m) = held {
                note.push_str(&format!(" - holds {}", m.filename));
            }
            if let Some(b) = &busy {
                note.push_str(&format!(" - {b}"));
            }
            if i == 0 {
                note.push_str(" - hides EGT80");
            }
            let line = format!("  {}{}", self.cyan(&letter.to_string()), self.dim(&note));
            self.send_line(&truncate_to_width(&line, 200)).await?;
        }
        self.send_line("").await?;
        self.send_line(&format!("  {}", self.action_prompt("Q", "Back")))
            .await?;
        self.send(&format!("{}> ", self.cyan("drive"))).await?;
        self.flush().await?;

        let Some(input) = self.get_menu_input(false).await? else {
            return Ok(false);
        };
        let letter = input.trim().chars().next().unwrap_or(' ').to_ascii_uppercase();
        if !letter.is_ascii_alphabetic() {
            return Ok(false);
        }
        let drive0 = letter as u8 - b'A';
        if drive0 >= crate::cpm::NUM_DRIVES {
            return Ok(false);
        }

        if self.cpmmount_confirm(filename, drive0).await? {
            let base = self.cpmmount_base();
            let name = filename.to_string();
            let result =
                tokio::task::spawn_blocking(move || image::mount_image(&base, drive0, &name))
                    .await
                    .unwrap_or_else(|e| Err(format!("mount failed: {e}")));
            match result {
                Ok(note) => {
                    self.cpmmount_persist();
                    self.cpmmount_report(&note, false).await?;
                }
                Err(e) => self.cpmmount_report(&e, true).await?,
            }
            return Ok(true);
        }
        Ok(false)
    }

    /// Mount, step 3: say plainly what this does, and get a yes.
    async fn cpmmount_confirm(
        &mut self,
        filename: &str,
        drive0: u8,
    ) -> Result<bool, std::io::Error> {
        let letter = (b'A' + drive0) as char;
        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;
        self.send_line(&format!("  {}", self.yellow("CONFIRM MOUNT")))
            .await?;
        self.send_line(&sep).await?;
        self.send_line("").await?;
        let width = if self.terminal_type == TerminalType::Petscii { 30 } else { 60 };
        self.send_line(&format!(
            "  Mount {}",
            self.amber(&truncate_to_width(filename, width))
        ))
        .await?;
        self.send_line(&format!("  on drive {}:", self.cyan(&letter.to_string())))
            .await?;
        self.send_line("").await?;
        // Two lines so it fits 40 columns.
        self.send_line(&format!("  Drive {letter}: will use the files inside")).await?;
        self.send_line("  the image instead of the files in").await?;
        self.send_line(&format!("  its CPM/{letter} folder.")).await?;
        self.send_line("").await?;
        self.send_line(&format!(
            "  {}",
            self.dim("Those files are not touched, and come")
        ))
        .await?;
        self.send_line(&format!("  {}", self.dim("back when you unmount.")))
            .await?;
        if drive0 == 0 {
            self.send_line("").await?;
            self.send_line(&format!("  {}", self.amber("EGT80 lives in CPM/A and will be")))
                .await?;
            self.send_line(&format!("  {}", self.amber("hidden while this is mounted.")))
                .await?;
        }
        self.send_line("").await?;
        self.send(&format!("  Mount it? {}: ", self.cyan("y/N"))).await?;
        self.flush().await?;
        let answer = self.get_menu_input(false).await?.unwrap_or_default();
        Ok(answer.starts_with('y'))
    }

    /// Write the live mount table back to `cpm_mounts` so it survives a restart.
    fn cpmmount_persist(&self) {
        // The shared helper, not a local rebuild of the same list.  This screen
        // used to assemble it from `registry::all()` alone, which omits a drive
        // lent to a booted session — so mounting anything here while somebody
        // was booted rewrote `cpm_mounts` without their drives, and they were
        // gone after the next restart.  The web and desktop screens already
        // went through `current_mounts_value`; this was the same defect
        // surviving in a second copy of the rule.
        let value = image::current_mounts_value();
        std::thread::spawn(move || {
            config::update_config_value("cpm_mounts", &value);
        });
    }

    /// Show the outcome and wait, so a refusal is read rather than flashed past.
    async fn cpmmount_report(&mut self, text: &str, error: bool) -> Result<(), std::io::Error> {
        self.send_line("").await?;
        // Wrap to the screen: these messages explain themselves and are long.
        let width = if self.terminal_type == TerminalType::Petscii { 36 } else { 74 };
        for line in crate::aichat::wrap_line(text, width) {
            let shown = if error { self.red(&line) } else { self.green(&line) };
            self.send_line(&format!("  {shown}")).await?;
        }
        self.send_line("").await?;
        self.send("  Press any key to continue.").await?;
        self.flush().await?;
        let _ = self.wait_for_key().await;
        Ok(())
    }
}
