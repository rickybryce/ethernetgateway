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
use crate::cpm::image::registry::booted_not_already_lent;

impl TelnetSession {
    /// The `CPM/` container this gateway is configured for.
    pub(in crate::telnet) fn cpmmount_base(&self) -> std::path::PathBuf {
        let cfg = config::get_config();
        crate::cpm::layout::cpm_dir(&cfg.transfer_dir)
    }

    /// What these screens are mounting into: drive letters or board slots, and
    /// which images the thing that will run could actually reach.
    ///
    /// Resolved once per screen rather than per row — it stats the boot image —
    /// and shared with the web and desktop, so the three cannot disagree about
    /// what a drive is called.
    pub(in crate::telnet) fn cpmmount_context(&self) -> crate::cpm::boot::MountContext {
        let cfg = config::get_config();
        crate::cpm::boot::MountContext::resolve(
            &cfg.transfer_dir,
            &cfg.cpm_boot_image,
            &cfg.cpm_boot_machine,
        )
    }

    /// Entry point: mount or unmount?
    /// Offer to fetch the sample disks, then do it.
    ///
    /// Its own screen, for the reason every value prompt here has one: this
    /// menu grows with the number of mounted images and has no rows to spare
    /// for a conversation.
    ///
    /// The operator is told the count, the size and **where it comes from**
    /// before agreeing.  The disks are not ours — they are David Hansel's and
    /// Jim McNeely's collections, and the software on them belongs to MITS,
    /// Microsoft, Digital Research and Infocom — so an operator who would rather
    /// fetch them themselves should be able to see that and decline.  Both
    /// repositories are named, one per line, from `fetch::source_repos`.
    pub(in crate::telnet) async fn cpmmount_download(&mut self) -> Result<(), std::io::Error> {
        use crate::cpm::fetch;
        let base = self.cpmmount_base();
        let images = base.join(crate::cpm::image::IMAGES_DIR);
        let all = fetch::catalogue();
        let wanted = fetch::missing(&images, &all);

        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;
        self.send_line(&format!("  {}", self.yellow("DOWNLOAD SAMPLE DISKS"))).await?;
        self.send_line(&sep).await?;
        self.send_line("").await?;

        if wanted.is_empty() {
            self.send_line(&format!("  {}", self.green("All of them are already here."))).await?;
            self.send_line("").await?;
            self.send("  Press any key to continue.").await?;
            self.flush().await?;
            self.wait_for_key().await?;
            return Ok(());
        }

        let megabytes = wanted.iter().map(|d| d.bytes).sum::<u64>() as f64 / (1024.0 * 1024.0);
        self.send_line(&format!(
            "  {} disks, {:.0} MB, from",
            self.amber(&wanted.len().to_string()),
            megabytes
        ))
        .await?;
        // One line per repository, not one line joined: the widest is
        // `github.com/jpmcneely/AltairDuino-Disks` at 38 characters, which with
        // the two-space indent is exactly the 40 a PETSCII C64 has.  Joined with
        // " and " it would wrap on the screen this menu is most often read on.
        for repo in fetch::source_repos() {
            self.send_line(&format!("  {}", self.amber(&repo))).await?;
        }
        self.send_line("").await?;
        self.send_line(&format!("  {}", self.dim("Only the disks that are known to"))).await?;
        self.send_line(&format!("  {}", self.dim("run here. They are not ours; this"))).await?;
        self.send_line(&format!("  {}", self.dim("fetches them for you. Anything"))).await?;
        self.send_line(&format!("  {}", self.dim("already there is left alone."))).await?;
        // Said before the operator agrees, because it is a second thing arriving
        // from a second author -- and because a disk that needs it is in the set.
        self.send_line(&format!("  {}", self.dim("Brings the CP/M monitor ROMs too,"))).await?;
        self.send_line(&format!("  {}", self.dim("which one sample disk needs. They"))).await?;
        self.send_line(&format!("  {}", self.dim("are not switched on by arriving."))).await?;
        self.send_line("").await?;
        self.send(&format!("  Download them? {}: ", self.cyan("y/N"))).await?;
        self.flush().await?;
        let answer = self.get_line_input().await?.unwrap_or_default();
        if !answer.trim().eq_ignore_ascii_case("y") {
            return Ok(());
        }

        self.send_line("").await?;
        self.send_line(&format!("  {}", self.dim("Fetching. This takes a minute."))).await?;
        self.flush().await?;

        // Off the async runtime: this is a minute of blocking network and file
        // I/O, and doing it on the session's own task would stall every other
        // session's timers with it.
        let dir = base.clone();
        let report = tokio::task::spawn_blocking(move || {
            fetch::download_missing(&dir, |_name, _i, _n| {})
        })
        .await
        .map_err(std::io::Error::other)?;

        self.send_line("").await?;
        match report {
            Ok(r) => {
                let colour = if r.failed.is_empty() { self.green(&r.summary()) } else { self.amber(&r.summary()) };
                self.send_line(&format!("  {colour}")).await?;
                // Name what failed rather than only counting it: "3 failed" with
                // no names leaves the operator unable to retry anything.
                // Grouped by reason -- with no internet every disk fails the
                // same way, and four identical truncated lines on a 40-column
                // screen is four lines saying nothing.
                for line in r.failure_lines(4) {
                    let w = if self.terminal_type == TerminalType::Petscii { 34 } else { 70 };
                    self.send_line(&format!("  {}", self.red(&truncate_to_width(&line, w))))
                        .await?;
                }
            }
            Err(e) => self.send_line(&format!("  {}", self.red(&e))).await?,
        }
        self.send_line("").await?;
        self.send("  Press any key to continue.").await?;
        self.flush().await?;
        self.wait_for_key().await?;
        Ok(())
    }

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
            //
            // A slot is named for whatever CP/M is set to run, and the naming is
            // not cosmetic.  Under the emulator our BDOS is underneath the
            // drives, so `B:` is a promise we keep.  Under a booted disk the
            // slot is a number on a *board*, and whether the guest reaches it is
            // its own BIOS's business — calling it `B:` there would be us making
            // a promise on the guest's behalf.
            //
            // Resolved against the images folder rather than read off the key:
            // a `cpm_boot_image` naming a disk that has since been deleted runs
            // the emulator, and a screen that named board slots there would be
            // describing a machine nobody is going to get.
            // One context for the whole screen: the naming and the slot names
            // both come from it, so this list cannot disagree with the two
            // screens it leads to.
            let ctx = self.cpmmount_context();
            let naming = ctx.naming.clone();
            let lent = image::registry::boot_loans();
            let any = mounts.iter().any(|m| m.is_some()) || !lent.is_empty();
            // **Which disk holds slot 0**, from the same context as the slot
            // names.  Outside the `any` branch on purpose: with a disk set to
            // boot and nothing mounted -- the ordinary case -- the screen
            // otherwise said only "No images mounted." and never mentioned the
            // disk that has slot 0, which is the state that prompted the
            // question (reported 2026-08-21).  One text for three surfaces, see
            // `MountContext::boot_slot_note`.
            if let Some(note) = ctx.boot_slot_note() {
                let width = if self.terminal_type == TerminalType::Petscii { 26 } else { 58 };
                self.send_line("  Booting:").await?;
                self.send_line(&format!(
                    "   {} {}",
                    self.cyan(&ctx.slot(0)),
                    self.dim(&truncate_to_width(&note, width)),
                ))
                .await?;
                self.send_line("").await?;
            }
            if any {
                self.send_line(match naming {
                    crate::cpm::boot::SlotNaming::Drives => "  Mounted:",
                    crate::cpm::boot::SlotNaming::Boards => "  Mounted (for the booted disk):",
                })
                .await?;
                for (i, m) in mounts.iter().enumerate() {
                    let Some(m) = m else { continue };
                    // Named by the booted disk's board, like every other slot
                    // on these screens.  It was named by *this row's* image, so
                    // a mount made under a different boot setting could still
                    // print `Drive 1` in a column of `unit 0.x` -- the mixture
                    // this was all meant to end.
                    let slot = ctx.slot(i as u8);
                    // Whose read-only answer applies depends on which CP/M is
                    // set to run — see `mount_refuses_writes`.  Marking a disk
                    // R/O here on our BDOS's verdict, while a booted guest was
                    // free to write it, is the mismatch that helper exists for.
                    let ro = if crate::cpm::boot::mount_refuses_writes(&naming, m) {
                        " (R/O)"
                    } else {
                        ""
                    };
                    let busy = usage
                        .get(i)
                        .and_then(|u| u.describe())
                        .map(|d| format!(" - {d}"))
                        .unwrap_or_default();
                    let width = if self.terminal_type == TerminalType::Petscii { 28 } else { 60 };
                    self.send_line(&format!(
                        "   {} {}{}{}",
                        self.cyan(&slot),
                        self.amber(&truncate_to_width(&m.filename, width)),
                        ro,
                        self.dim(&busy),
                    ))
                    .await?;
                    // Slot 0 belongs to the disk being booted, so a mount on A:
                    // is held but unreachable.  Said here, where it can still be
                    // changed, rather than only in the boot screen's notes after
                    // the operator has left.  Its own row: at 40 columns it does
                    // not fit beside a filename.
                    if i == 0 && ctx.booting() {
                        self.send_line(&format!(
                            "     {}",
                            self.dim(crate::cpm::boot::BEHIND_BOOT_DISK_SHORT),
                        ))
                        .await?;
                    }
                }
                for (drive0, name) in &lent {
                    // From the context, not from the file: it is in a booted
                    // session's hands, and a stat of it would name a board for
                    // bytes nobody can rely on.
                    let slot = ctx.slot(*drive0);
                    let width = if self.terminal_type == TerminalType::Petscii { 20 } else { 52 };
                    self.send_line(&format!(
                        "   {} {} {}",
                        self.cyan(&slot),
                        self.amber(&truncate_to_width(name, width)),
                        self.dim("(booted)"),
                    ))
                    .await?;
                }
            } else {
                // "No *other*" when a disk has slot 0: plain "No images
                // mounted." beside a Booting: line reads as a contradiction.
                self.send_line(&format!(
                    "  {}",
                    self.dim(if ctx.booting() {
                        "No other images mounted."
                    } else {
                        "No images mounted."
                    })
                ))
                .await?;
            }

            // Booted images, listed *separately* and without a drive letter.
            //
            // Not folded into "Mounted:" above, and not given a letter, because
            // neither would be true: a booted disk is not on one of our drives
            // at all — it is its board's slot 0, and the guest's own operating
            // system decides what to call it.  Stock Altair CP/M happens to say
            // A:, which is exactly the coincidence that makes writing `A:` here
            // a statement this screen cannot stand behind.
            //
            // It has to be *somewhere*, though: an image booted without having
            // been mounted first is in none of the tables above, so the screen
            // was offering a disk, refusing it with "being run by a booted
            // session", and showing nothing that accounted for the refusal.
            //
            // "Without having been mounted first" is the whole of it, and this
            // block used to ignore that clause: a disk that WAS mounted and
            // then booted is lent, so it is already on the list above with its
            // slot and a `(booted)` of its own, and repeating it here said the
            // same thing twice on a screen with no rows to spare.
            let booted = booted_not_already_lent(image::registry::booted_image_names(), &lent);
            if !booted.is_empty() {
                self.send_line("").await?;
                self.send_line("  Booted:").await?;
                // 26, not 30: the row is three of indent, the name, a space and
                // the nine characters of "(running)" — which at 30 came to 43
                // columns on a 40-column screen.
                let width = if self.terminal_type == TerminalType::Petscii { 26 } else { 62 };
                for name in &booted {
                    self.send_line(&format!(
                        "   {} {}",
                        self.amber(&truncate_to_width(name, width)),
                        self.dim("(running)"),
                    ))
                    .await?;
                }
                // Two lines, not three: this screen grows with the number of
                // mounts and has no row budget left to spend on prose.
                self.send_line(&format!("  {}", self.dim("Running its own OS - not on a drive"))).await?;
                self.send_line(&format!("  {}", self.dim("of ours, and not mountable yet."))).await?;
            }
            self.send_line("").await?;

            self.send_line(&format!("  {}  Mount an image", self.cyan("M")))
                .await?;
            // No `B  Boot an image` here since 0.9.2.  This screen is
            // configuration -- what is mounted where -- and it carried a second
            // way to *start* a machine, one that ran for a visit and remembered
            // nothing while `cpm_boot_image` ran the CP/M menu item and
            // remembered everything.  Two boots that asked different questions
            // and disagreed about what would happen next was the single largest
            // source of confusion in the whole CP/M feature.  One boot now:
            // CP/M Boot Settings decides, the CP/M menu item runs it.
            self.send_line(&format!("  {}  New blank disk", self.cyan("N")))
                .await?;
            // The download offer, before the mount options rather than after:
            // a fresh install has nothing to mount, and "where do I get a disk"
            // is the question this screen otherwise leaves the operator holding.
            // One row, on a screen that grows with the number of mounts — which
            // is why it says what it does and nothing more.
            self.send_line(&format!("  {}  Download sample disks", self.cyan("D")))
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
                Some(s) if s == "n" => self.cpmmount_new_blank().await?,
                Some(s) if s == "d" => self.cpmmount_download().await?,
                Some(s) if s == "u" && any => self.cpmmount_pick_unmount().await?,
                // Q, the key this screen has always *displayed*.  It fell into
                // the "anything else, ignore it" arm below and redrew the menu,
                // so the one documented way out did nothing and only ESC or a
                // bare Enter left.  Every sibling screen spells `q` out; this
                // one relied on the catch-all, and the catch-all does not catch
                // it.  `test_cpm_disk_images_keys_are_displayed_and_handled`
                // now holds the whole row.
                Some(s) if s == "q" => return Ok(()),
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
        let ctx = self.cpmmount_context();
        let all = image::available_images(&base);
        // Only images the thing that is going to run could actually reach.
        // Under the emulator that is all of them; with a disk set to boot it is
        // the ones on that disk's board, because the board is chosen by size and
        // a mount on the wrong one is present, correct and invisible.
        let dir = image::images_dir(&base);
        // Counted only when a disk is booting, and only for files we could
        // actually read.  A file that vanished between the listing and the stat
        // is not "on the wrong board", and saying so with no disk booting
        // explains a state that does not exist.
        let mut hidden = 0usize;
        let mut images: Vec<String> = Vec::new();
        for n in all {
            match std::fs::metadata(dir.join(&n)) {
                Ok(m) if ctx.accepts(m.len()) => images.push(n),
                Ok(_) => hidden += 1,
                Err(_) => {}
            }
        }
        if images.is_empty() {
            self.clear_screen().await?;
            self.send_line("").await?;
            self.send_line(&format!("  {}", self.amber("No disk images found.")))
                .await?;
            self.send_line("").await?;
            if hidden > 0 {
                // Never silently: a folder full of images showing "none" is a
                // mystery, and the operator can act on this one -- change what
                // boots, or fetch a disk of the right kind.
                self.send_line(&format!("  {hidden} are in the folder but are")).await?;
                self.send_line("  not on the booted disk's board,").await?;
                self.send_line("  so its OS could not read them.").await?;
            } else {
                self.send_line("  Put .dsk files in the transfer dir").await?;
                self.send_line("  under CPM/images, then try again.").await?;
            }
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
            // Said whether none or only some are withheld.  It was said only
            // when the list came out empty, which left the commonest case --
            // two of thirty disks offered -- with no explanation at all, on the
            // surface the retro hardware actually uses.
            if hidden > 0 {
                self.send_line(&format!(
                    "  {}",
                    self.dim(&format!("{hidden} more are not on the booted"))
                ))
                .await?;
                self.send_line(&format!("  {}", self.dim("disk's board."))).await?;
                self.send_line("").await?;
            }
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
        let ctx = self.cpmmount_context();
        let booting = ctx.booting();
        self.send_line(&format!(
            "  {}",
            self.yellow(if booting { "CHOOSE A SLOT" } else { "CHOOSE A DRIVE" })
        ))
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
            // Under a booted disk, say what the slot *is* before saying what is
            // in it: the number and the board are the whole answer to "will the
            // guest see this", and the letter is only how `cpm_mounts` spells it.
            // Named after the *booted disk's* board, not this image's.  The
            // image picker only offered images on that same board, so the whole
            // column reads in one vocabulary — which it did not before, when
            // each row was named after whatever was in it and one screen could
            // show `unit 0.0` beside `Drive 1`.
            if booting {
                note.push_str(&format!("  {}", ctx.slot(i)));
            }
            if let Some(m) = held {
                note.push_str(&format!(" - holds {}", m.filename));
            }
            if let Some(b) = &busy {
                note.push_str(&format!(" - {b}"));
            }
            if i == 0 {
                // Two different facts about slot 0, and which one is true
                // depends on what is running.  EGT8080 lives in the gateway's own
                // drive A: folder, which a booted disk never sees.
                //
                // Plural again: drive A: carries the Z80 build beside the
                // 8080 one -- singular through 0.9.2, when only EGT8080
                // shipped -- and a mount over A: hides both, along with
                // whatever else the operator has put there.  The note stays
                // "terminals" rather than naming either: this is the first
                // thing `truncate_to_width` takes away on a 40-column
                // PETSCII row, and a filename here would cost more than it
                // tells.
                note.push_str(if booting {
                    " - booted disk here"
                } else {
                    " - hides terminals"
                });
            }
            // The *note* is bounded, not the finished line.  `truncate_to_width`
            // counts characters, and a coloured line is mostly escape bytes — so
            // the 200 this used to be given never truncated anything, and a row
            // holding a long filename ran off a 40-column screen.  Cutting the
            // escapes instead would be worse than the overflow.
            let width = if self.terminal_type == TerminalType::Petscii { 37 } else { 77 };
            let line =
                format!("  {}{}", self.cyan(&letter.to_string()), self.dim(&truncate_to_width(&note, width)));
            self.send_line(&line).await?;
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
            // Both terminals live there, so the plural is the true statement
            // and it costs no rows: still two, which a 40-column screen needs.
            self.send_line(&format!("  {}", self.amber("Both terminals live in CPM/A and")))
                .await?;
            self.send_line(&format!("  {}", self.amber("are hidden while this is mounted.")))
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

#[cfg(test)]
mod tests {
    use super::booted_not_already_lent;

    /// **Every mount surface must account for a booted image.**
    ///
    /// The rule and its filter lived here, in the telnet module, while the
    /// desktop dialog and the web page listed nothing — so the surface that
    /// answered "what is running?" was the one an operator on a C64 was least
    /// likely to be using, and on the other two an image could be offered,
    /// refused on Save as "being run by a booted session", and accounted for
    /// nowhere (reported 2026-08-21). The function now lives in the registry;
    /// this holds that all three surfaces ask it.
    #[test]
    fn test_every_mount_surface_reports_a_booted_image() {
        for (surface, src) in [
            ("telnet", include_str!("cpm_mount_ui.rs")),
            ("desktop", include_str!("../gui.rs")),
            ("web", include_str!("../webserver.rs")),
        ] {
            assert!(
                src.contains("booted_to_report") || src.contains("booted_not_already_lent"),
                "the {surface} mount screen never asks which images are booted"
            );
        }
    }


    /// **An image that was mounted and then booted is listed once, not twice.**
    ///
    /// It is in two registry tables at once — `boot_loans` because its drive
    /// was taken, `booted_image_names` because it is running — and the disks
    /// screen printed both. On a screen that grows a row per mount and has no
    /// budget to spare, saying the same thing twice costs a row that a real
    /// disk needed.
    #[test]
    fn test_a_mounted_then_booted_image_is_not_listed_twice() {
        let lent = vec![(1u8, "altair8_cpm22.dsk".to_string())];
        assert_eq!(
            booted_not_already_lent(vec!["altair8_cpm22.dsk".to_string()], &lent),
            Vec::<String>::new(),
            "the lent row above already names it, with its slot"
        );
    }

    /// **And the case the block exists for still shows.**
    ///
    /// The control, and the more important half: an image booted straight from
    /// the picker was never mounted, so it is in *no* table the screen shows
    /// above. Filtering it out too would put the screen back where it started
    /// — offering a disk, refusing it as "run by a booted session", and
    /// showing nothing that accounted for the refusal.
    #[test]
    fn test_an_image_booted_without_being_mounted_is_still_listed() {
        let lent = vec![(1u8, "altair8_cpm22.dsk".to_string())];
        assert_eq!(
            booted_not_already_lent(
                vec!["altair8_cpm22.dsk".to_string(), "altair8_games.dsk".to_string()],
                &lent
            ),
            vec!["altair8_games.dsk".to_string()],
        );
        // With nothing lent at all, every booted image is the screen's only
        // account of itself.
        assert_eq!(
            booted_not_already_lent(vec!["a.dsk".into(), "b.dsk".into()], &[]),
            vec!["a.dsk".to_string(), "b.dsk".to_string()],
        );
    }
}
