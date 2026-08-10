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

/// The booted images that are **not** already on the list above.
///
/// Two registry tables answer two different questions, and an image can be in
/// both. `boot_loans` records a *drive* a boot borrowed — which happens when
/// the image was mounted first, because the boot rewrites the file and the
/// mount has to go out of service. `booted_image_names` records an *image* a
/// session is running, mounted or not.
///
/// So a disk that was mounted and then booted is in both, and the screen said
/// so twice: once as `B: name (booted)` with its slot, and again under
/// `Booted:`. The block's own doc had the scope right all along — "an image
/// that was booted *without* being mounted first appears in none of the tables
/// above" — and the code did not implement that clause.
///
/// Its own function so the clause is testable. Inline it was four lines that
/// read like a formality and would be simplified away by the next person to
/// touch this screen.
fn booted_not_already_lent(booted: Vec<String>, lent: &[(u8, String)]) -> Vec<String> {
    booted
        .into_iter()
        .filter(|n| !lent.iter().any(|(_, name)| name == n))
        .collect()
}

impl TelnetSession {
    /// The `CPM/` container this gateway is configured for.
    pub(in crate::telnet) fn cpmmount_base(&self) -> std::path::PathBuf {
        let cfg = config::get_config();
        crate::cpm::layout::cpm_dir(&cfg.transfer_dir)
    }

    /// Entry point: mount or unmount?
    /// Offer to fetch the sample disks, then do it.
    ///
    /// Its own screen, for the reason every value prompt here has one: this
    /// menu grows with the number of mounted images and has no rows to spare
    /// for a conversation.
    ///
    /// The operator is told the count, the size and **where it comes from**
    /// before agreeing.  The disks are not ours — they are David Hansel's
    /// collection, and the software on them belongs to MITS, Microsoft and
    /// Digital Research — so an operator who would rather fetch them
    /// themselves should be able to see that and decline.
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
        self.send_line(&format!("  {}", self.amber(fetch::ALTAIR_DUINO_SOURCE))).await?;
        self.send_line("").await?;
        self.send_line(&format!("  {}", self.dim("Only the disks that are known to"))).await?;
        self.send_line(&format!("  {}", self.dim("run here. They are not ours; this"))).await?;
        self.send_line(&format!("  {}", self.dim("fetches them for you. Anything"))).await?;
        self.send_line(&format!("  {}", self.dim("already there is left alone."))).await?;
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
        let dir = images.clone();
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
                for (name, why) in r.failed.iter().take(4) {
                    let w = if self.terminal_type == TerminalType::Petscii { 34 } else { 70 };
                    self.send_line(&format!(
                        "  {}",
                        self.red(&truncate_to_width(&format!("{name}: {why}"), w))
                    ))
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
            let cfg = config::get_config();
            let naming =
                crate::cpm::boot::boot_target(&cfg.transfer_dir, &cfg.cpm_boot_image).slot_naming();
            let lent = image::registry::boot_loans();
            let any = mounts.iter().any(|m| m.is_some()) || !lent.is_empty();
            if any {
                self.send_line(match naming {
                    crate::cpm::boot::SlotNaming::Drives => "  Mounted:",
                    crate::cpm::boot::SlotNaming::Boards => "  Mounted (for the booted disk):",
                })
                .await?;
                for (i, m) in mounts.iter().enumerate() {
                    let Some(m) = m else { continue };
                    let slot = crate::cpm::boot::slot_name(
                        &naming,
                        i as u8,
                        std::fs::metadata(&m.path).ok().map(|md| md.len()),
                    );
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
                }
                for (drive0, name) in &lent {
                    // No length: the file is in a booted session's hands, and a
                    // stat of it would name a board for bytes nobody can rely on.
                    let slot = crate::cpm::boot::slot_name(&naming, *drive0, None);
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
                self.send_line(&format!("  {}", self.dim("No images mounted.")))
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
            self.send_line(&format!("  {}  Boot an image (runs its own OS)", self.cyan("B")))
                .await?;
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
                Some(s) if s == "b" => self.cpmmount_pick_boot().await?,
                Some(s) if s == "n" => self.cpmmount_new_blank().await?,
                Some(s) if s == "d" => self.cpmmount_download().await?,
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

    /// One line per bootable medium: its size, then what it is.
    ///
    /// Asked of the machine's controllers rather than written here. This screen
    /// is what a telnet user reads when nothing in their images folder can be
    /// booted, and it said "Only Altair 88-DCDD floppies can boot" — with the
    /// two floppy sizes and no mention of the hard disk — for as long as hard
    /// disks had been booting. That is the *fourth* place the same list had been
    /// written down; the readme and the manual were the others.
    ///
    /// Kept inside PETSCII's 40 columns, which is why the size is not
    /// comma-grouped: 2 indent + 7 size + 1 separator leaves **30 characters
    /// for a `Medium::label`**. Extracted so the width can be measured rather
    /// than argued about, since a runtime `format!` is invisible to the
    /// source-scanning layout tests.
    ///
    /// That budget, and the row count of the screen this prints on, are both
    /// checked in one place — `test_bootable_size_lines_fit_petscii_and_name_every_medium`,
    /// which iterates every board's media. Deliberately not restated at each
    /// board: a limit written down in six modules is a limit enforced in none,
    /// and the labels that overran it were written by someone reading a
    /// neighbouring board rather than this line.
    pub(in crate::telnet) fn bootable_size_lines() -> Vec<String> {
        crate::cpm::boot_machine::BootMachine::bootable_media()
            .into_iter()
            .map(|m| format!("  {:>7} {}", m.bytes, m.label))
            .collect()
    }

    /// Choose an image to boot.
    ///
    /// Booting is not mounting, and the screen says so: a booted disk runs its
    /// own operating system and owns the hardware, so it is a different thing
    /// from putting an image on drive B:.  The mounted images do come along —
    /// each at the board slot its drive letter names — but what a slot *is*
    /// belongs to the board (a drive on the floppy controllers, a platter on
    /// the 88-HDSK), and what it is *called* belongs to the guest, as does how
    /// many of them it can reach.
    async fn cpmmount_pick_boot(&mut self) -> Result<(), std::io::Error> {
        let base = self.cpmmount_base();
        let images = crate::cpm::image::available_images(&base);
        let bootable: Vec<String> = images
            .into_iter()
            .filter(|n| {
                std::fs::metadata(crate::cpm::image::images_dir(&base).join(n))
                    .ok()
                    .and_then(|m| {
                        crate::cpm::boot_machine::BootMachine::medium_for(m.len())
                    })
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
            self.send_line("  A bootable image is one of these").await?;
            self.send_line("  sizes (a short trailer is OK):").await?;
            for line in Self::bootable_size_lines() {
                self.send_line(&line).await?;
            }
            self.send_line("").await?;
            self.send("  Press any key to continue.").await?;
            self.flush().await?;
            let _ = self.wait_for_key().await;
            return Ok(());
        }
        self.send_line(&format!("  {}", self.dim("The disk runs its OWN operating"))).await?;
        self.send_line(&format!("  {}", self.dim("system. Its drive FOLDERS do not"))).await?;
        self.send_line(&format!("  {}", self.dim("apply; mounted images do, each on"))).await?;
        self.send_line(&format!("  {}", self.dim("the board slot its letter names."))).await?;
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
        //
        // "the disks", plural, because this answer has always governed every
        // drive the session takes and not just the one being booted — it was
        // worded as though it were about one image while the mounts were
        // effectively write-protected anyway, and now that they are not, the
        // understatement would be a trap.
        self.send_line("").await?;
        self.send_line(&format!("  {}", self.dim("Covers the mounted disks too."))).await?;
        self.send(&format!("  Allow writes to the disks? {}: ", self.cyan("y/N")))
            .await?;
        self.flush().await?;
        let ans = self.get_menu_input(false).await?.unwrap_or_default();
        let writable = ans.starts_with('y');

        // And how this disk wants Backspace.  Asked here, per boot, because it
        // is a property of the operating system on the disk and not of the
        // gateway: most of them erase on BS and read a terminal's DEL as a
        // Teletype rubout — printing the character they just deleted — but CP/M
        // 1.x is the other way round, and it is one keypress to say so.
        // `cpm_boot_backspace` seeds the default so the common case is Return.
        let default_erase =
            crate::cpm::boot::backspace_erases(&config::get_config().cpm_boot_backspace);
        self.send_line(&format!("  {}", self.dim("Backspace erases (N = rubout, as"))).await?;
        self.send_line(&format!("  {}", self.dim("CP/M 1.x expects)"))).await?;
        self.send(&format!(
            "  Backspace erases? {}: ",
            self.cyan(if default_erase { "Y/n" } else { "y/N" })
        ))
        .await?;
        self.flush().await?;
        let ans = self.get_menu_input(false).await?.unwrap_or_default();
        let erase = match ans.trim().chars().next() {
            Some('y') | Some('Y') => true,
            Some('n') | Some('N') => false,
            _ => default_erase, // bare Return keeps the configured answer
        };

        self.cpm_boot_session(&path, writable, erase).await
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
        let cfg = config::get_config();
        let naming =
            crate::cpm::boot::boot_target(&cfg.transfer_dir, &cfg.cpm_boot_image).slot_naming();
        let booting = naming == crate::cpm::boot::SlotNaming::Boards;
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

        // This image's own size, which is what decides the board it would go on
        // — not the slot it is going into.  Read once for the whole list.
        let image_len = std::fs::metadata(
            crate::cpm::image::images_dir(&self.cpmmount_base()).join(filename),
        )
        .ok()
        .map(|md| md.len());

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
            if booting {
                note.push_str(&format!(
                    "  {}",
                    crate::cpm::boot::slot_name(&naming, i, image_len)
                ));
            }
            if let Some(m) = held {
                note.push_str(&format!(" - holds {}", m.filename));
            }
            if let Some(b) = &busy {
                note.push_str(&format!(" - {b}"));
            }
            if i == 0 {
                // Two different facts about slot 0, and which one is true
                // depends on what is running.  EGT80 lives in the gateway's own
                // drive A: folder, which a booted disk never sees.
                // "terminals", plural: drive A: carries EGT8080.COM and
                // EGT80.COM, and a mount hides both.  Article dropped to keep
                // the row short — this note is appended after the filename and
                // is the first thing `truncate_to_width` takes away.
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
