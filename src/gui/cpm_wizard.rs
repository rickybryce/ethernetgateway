//! The CP/M setup wizard on the desktop.
//!
//! Every question, recommendation and setting is [`crate::cpm::wizard`]'s;
//! this window only draws it, one step at a time, with the chosen disk's
//! listing beside the choices.  Reading the disks and saving both happen on a
//! thread of their own, so the window never stalls the frame: the first read
//! of a disk can cold-start it to learn whether it boots.

use std::sync::mpsc::{Receiver, TryRecvError};

use eframe::egui;

use super::{AMBER, AMBER_BRIGHT, AMBER_DIM, WARN_BORDER};
use crate::cpm::wizard::{self, DiskFacts, Plan, Runs, SampleOffer, Step};

/// What the window asks its host to do after this frame.
pub(super) enum Outcome {
    Continue,
    /// Closed without saving, or closed after.
    Close,
    /// Saved: the host must take the CP/M keys from the global config into its
    /// own copy, or its next Save would write the old ones back.
    Saved,
    /// Download the sample disks: the host's one download slot
    /// (`start_cpm_fetch`), shared with the panel's button so two runs can
    /// never overlap.  The host says when it is over ([`CpmWizard::fetched`]).
    Fetch,
}

pub(super) struct CpmWizard {
    transfer_dir: String,
    /// The step on screen; its order is [`Step::next`]'s, shared with the
    /// other two surfaces.
    step: Step,
    /// The save has answered: the outcome is on screen in place of a step.
    done: bool,
    trail: Vec<Step>,
    disks: Option<Vec<DiskFacts>>,
    loading: Option<Receiver<(Vec<DiskFacts>, Option<SampleOffer>)>>,
    /// The sample disks not here, as of the last read.
    offer: Option<SampleOffer>,
    /// A download asked of the host is running.
    fetching: bool,
    /// A Download was refused because the host's slot was taken: when it
    /// frees, the disks are read again, since that other run may have been
    /// the sample disks and the offer would then be stale.
    slot_wait: bool,
    /// The disks are being read again after a download: on to the next step
    /// once they are, rather than to the first.
    after_fetch: bool,
    /// What the download came to, said on the step after it.
    fetch_note: String,
    /// Download was pressed this frame: [`Outcome::Fetch`] to the host.
    want_fetch: bool,
    saving: Option<Receiver<Result<Vec<String>, String>>>,
    result: Option<Result<Vec<String>, String>>,
    plan: Plan,
    /// The disk whose listing is shown beside the choices.
    viewing: Option<String>,
    msg: String,
    /// The Ready step's summary, worked out once on the way in: it reads
    /// files, and the window redraws many times a second.
    ready_lines: Vec<String>,
}

impl CpmWizard {
    pub(super) fn new(transfer_dir: &str) -> CpmWizard {
        CpmWizard {
            transfer_dir: transfer_dir.to_string(),
            step: Step::Runs,
            done: false,
            trail: Vec::new(),
            disks: None,
            loading: Some(Self::read_disks(transfer_dir)),
            offer: None,
            fetching: false,
            slot_wait: false,
            after_fetch: false,
            fetch_note: String::new(),
            want_fetch: false,
            saving: None,
            result: None,
            plan: Plan::emulator(),
            viewing: None,
            msg: String::new(),
            ready_lines: Vec::new(),
        }
    }

    /// Every disk, and what the download step would fetch, read on a thread.
    fn read_disks(transfer_dir: &str) -> Receiver<(Vec<DiskFacts>, Option<SampleOffer>)> {
        let (tx, rx) = std::sync::mpsc::channel();
        let dir = transfer_dir.to_string();
        std::thread::spawn(move || {
            let base = wizard::cpm_base(&dir);
            let _ = tx.send((wizard::all_disks(&base), wizard::sample_offer(&base)));
        });
        rx
    }

    /// Waiting on the host's download slot: a run asked for with
    /// [`Outcome::Fetch`], or the slot to free after one was refused.  The
    /// host polls the slot and calls [`Self::fetched`] once it is empty.
    pub(super) fn waiting(&self) -> bool {
        self.fetching || self.slot_wait
    }

    /// The host's slot is empty again.  After this wizard's own download,
    /// saying `note`: the disks are read again, and the wizard goes on from
    /// the download step with its first answer worked out afresh
    /// ([`Plan::opening`]).  After a refusal: only read again, to start over
    /// on whatever step [`Step::first`] now names.
    pub(super) fn fetched(&mut self, note: &str) {
        if std::mem::take(&mut self.slot_wait) {
            self.msg.clear();
            self.loading = Some(Self::read_disks(&self.transfer_dir));
            return;
        }
        self.fetching = false;
        self.fetch_note = note.trim().to_string();
        self.after_fetch = true;
        self.loading = Some(Self::read_disks(&self.transfer_dir));
    }

    /// The host's download slot was taken -- by the panel's own download or
    /// its monitor-ROM fetch -- so no run was started for this Download.
    /// Waiting on the slot would report that other run as the disks'.
    pub(super) fn fetch_refused(&mut self) {
        self.fetching = false;
        self.slot_wait = true;
        self.msg = "Another download is running: press Download again once it has finished.".to_string();
    }

    fn disk(&self, name: &str) -> Option<&DiskFacts> {
        self.disks.as_ref()?.iter().find(|d| d.name == name)
    }

    /// On to the step after this one.
    fn go_on(&mut self) {
        let disks = self.disks.as_deref().unwrap_or_default();
        let Some(to) = self.step.next(self.plan.runs, self.plan.needs_rom(disks)) else {
            return;
        };
        if self.step.on_trail() {
            self.trail.push(self.step);
        }
        self.step = to;
        self.msg.clear();
        if to == Step::Ready {
            self.ready_lines = self.plan.describe(&self.transfer_dir);
        }
    }

    /// [`wizard::check_plan`] at a disks step's Next: on when the plan
    /// passes, else the reason stays on screen.
    fn go_on_checked(&mut self) {
        match wizard::check_plan(&self.plan, self.disks.as_deref().unwrap_or_default()) {
            Ok(()) => self.go_on(),
            Err(e) => self.msg = e,
        }
    }

    /// A save is running: the window must stay until it has answered, or the
    /// host never learns to take the new settings into its own copy.
    pub(super) fn busy(&self) -> bool {
        self.saving.is_some()
    }

    /// Draw one frame of the wizard.
    pub(super) fn draw(&mut self, ui: &mut egui::Ui) -> Outcome {
        if let Some(rx) = &self.loading {
            match rx.try_recv() {
                Ok((d, offer)) => {
                    self.plan = Plan::opening(&d);
                    self.offer = offer;
                    self.disks = Some(d);
                    self.loading = None;
                    if std::mem::take(&mut self.after_fetch) {
                        self.go_on();
                    } else {
                        self.step = Step::first(self.offer.as_ref());
                    }
                }
                Err(TryRecvError::Empty) => {
                    ui.label("Reading the disk images…");
                    ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
                    return Outcome::Continue;
                }
                Err(TryRecvError::Disconnected) => {
                    self.disks = Some(Vec::new());
                    self.loading = None;
                }
            }
        }
        if self.waiting() {
            // Polled by the host; this only keeps the frames coming.
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
        }
        let mut saved = false;
        if let Some(rx) = &self.saving {
            match rx.try_recv() {
                Ok(r) => {
                    saved = r.is_ok();
                    self.result = Some(r);
                    self.saving = None;
                    self.done = true;
                    self.trail.clear();
                }
                Err(TryRecvError::Empty) => {
                    ui.label("Saving…");
                    ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
                    return Outcome::Continue;
                }
                Err(TryRecvError::Disconnected) => {
                    self.result = Some(Err("the save did not finish".to_string()));
                    self.saving = None;
                    self.done = true;
                }
            }
        }

        let mut close = false;
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width(420.0);
                self.draw_step(ui);
                if !self.msg.is_empty() {
                    ui.label(egui::RichText::new(&self.msg).color(WARN_BORDER));
                }
                ui.add_space(8.0);
                ui.separator();
                ui.horizontal(|ui| {
                    if !self.done
                        && !self.trail.is_empty()
                        && ui.button("Back").clicked()
                    {
                        self.step = self.trail.pop().unwrap_or(Step::Runs);
                        self.msg.clear();
                    }
                    let label = match self.step {
                        _ if self.done => "Close",
                        Step::Ready => "Save",
                        _ => "Next",
                    };
                    // The download step answers with its own two buttons.
                    if (self.done || self.step != Step::Fetch)
                        && ui.add(egui::Button::new(egui::RichText::new(label).strong().color(AMBER_BRIGHT))).clicked()
                    {
                        close = self.next();
                    }
                });
            });
            ui.separator();
            ui.vertical(|ui| {
                self.draw_listing(ui);
            });
        });
        if saved {
            return Outcome::Saved;
        }
        if std::mem::take(&mut self.want_fetch) {
            self.fetching = true;
            return Outcome::Fetch;
        }
        if close {
            return Outcome::Close;
        }
        Outcome::Continue
    }

    /// The disk being looked at: its summary and every file.
    fn draw_listing(&self, ui: &mut egui::Ui) {
        let Some(d) = self.viewing.as_deref().and_then(|n| self.disk(n)) else {
            ui.label(egui::RichText::new("Choose a disk to see what is on it.").color(AMBER_DIM));
            return;
        };
        ui.label(egui::RichText::new(&d.name).strong().color(AMBER_BRIGHT));
        ui.label(egui::RichText::new(&d.summary).color(AMBER_DIM));
        if d.altair_basic {
            ui.label(egui::RichText::new(wizard::BASIC_NOTE).color(AMBER));
        }
        if d.needs_rom {
            ui.label(egui::RichText::new(wizard::ROM_NOTE).color(AMBER));
        }
        // The listing takes whatever width the window has beside the steps:
        // dragging the wizard wider is for long file names.
        egui::ScrollArea::vertical().max_height(360.0).auto_shrink([false, true]).id_salt("cpmw_listing").show(ui, |ui| {
            ui.set_min_width(260.0);
            for f in d.listing() {
                ui.monospace(f);
            }
        });
    }

    fn para(ui: &mut egui::Ui, lines: &[&str]) {
        for p in lines.split(|l| l.is_empty()) {
            ui.label(p.join(" "));
            ui.add_space(4.0);
        }
    }

    /// The step's controls.  The disks are lent to the drawing, not copied:
    /// a collection can carry hundreds of listed files, and this runs every
    /// frame.
    fn draw_step(&mut self, ui: &mut egui::Ui) {
        let disks = self.disks.take().unwrap_or_default();
        self.draw_step_with(ui, &disks);
        self.disks = Some(disks);
    }

    fn draw_step_with(&mut self, ui: &mut egui::Ui, disks: &[DiskFacts]) {
        if self.done {
            self.draw_done(ui);
            return;
        }
        match self.step {
            Step::Fetch => {
                ui.heading("Sample disks");
                Self::para(ui, wizard::FETCH_HELP);
                if let Some(o) = &self.offer {
                    ui.label(
                        egui::RichText::new(format!(
                            "{} disks, {:.0} MB, from {}.",
                            o.disks,
                            o.megabytes,
                            crate::cpm::fetch::source_repos().join(" and ")
                        ))
                        .color(AMBER_DIM),
                    );
                }
                ui.add_space(6.0);
                if self.fetching {
                    ui.label(egui::RichText::new("Downloading… this takes a minute.").color(AMBER));
                } else {
                    ui.horizontal(|ui| {
                        if ui.button(egui::RichText::new(wizard::FETCH_LABEL).strong().color(AMBER_BRIGHT)).clicked() {
                            self.want_fetch = true;
                        }
                        if ui.button(wizard::DECLINE_LABEL).clicked() {
                            self.go_on();
                        }
                    });
                }
            }
            Step::Runs => {
                ui.heading("What should CP/M run?");
                Self::para(ui, wizard::RUNS_HELP);
                // Both always: Boot too with nothing here that boots, for an
                // operator with images of their own (the next step says where).
                // Through `choose_runs`, which knows what each answer keeps.
                let advised = wizard::opening_runs(disks);
                let mark = |r: Runs| if r == advised { " (advised)" } else { "" };
                let mut runs = self.plan.runs;
                ui.radio_value(&mut runs, Runs::Emulator, format!("{}{}", wizard::EMULATOR_LABEL, mark(Runs::Emulator)));
                ui.radio_value(&mut runs, Runs::Boot, format!("{}{}", wizard::BOOT_LABEL, mark(Runs::Boot)));
                if runs != self.plan.runs {
                    self.plan.choose_runs(runs);
                }
                if !self.fetch_note.is_empty() {
                    ui.label(egui::RichText::new(format!("Sample disks: {}", self.fetch_note)).small().color(AMBER_DIM));
                }
            }
            Step::EmulatorDrives => {
                ui.heading("Disk images on drive letters (optional)");
                Self::para(ui, wizard::EMULATOR_DRIVES_HELP);
                let list: Vec<&DiskFacts> = disks.iter().filter(|d| d.mountable()).collect();
                if list.is_empty() {
                    ui.label(egui::RichText::new("No disk image here has CP/M files to put on a drive.").color(AMBER_DIM));
                }
                egui::ScrollArea::vertical().max_height(300.0).id_salt("cpmw_drives").show(ui, |ui| {
                    for drive in 1..crate::cpm::NUM_DRIVES {
                        if list.is_empty() {
                            break;
                        }
                        self.slot_combo(ui, drive, &format!("{}:", (b'A' + drive) as char), "(its folder)", &list);
                    }
                });
            }
            Step::BootDisk => {
                ui.heading("Which disk should boot?");
                if !disks.iter().any(|d| d.boots) {
                    Self::para(ui, wizard::NO_BOOT_DISK);
                    return;
                }
                Self::para(ui, wizard::BOOT_DISK_HELP);
                egui::ScrollArea::vertical().max_height(300.0).id_salt("cpmw_boot").show(ui, |ui| {
                    for d in disks.iter().filter(|d| d.boots) {
                        let on = self.plan.boot_disk.as_deref() == Some(d.name.as_str());
                        let about = d.summary.split(" -- ").nth(1).unwrap_or("");
                        if ui.selectable_label(on, format!("{}   {}", d.name, about)).clicked() {
                            self.viewing = Some(d.name.clone());
                            self.plan.choose_boot_disk(d);
                        }
                    }
                });
            }
            Step::Machine => {
                let Some(boot) = self.plan.boot_in(disks).cloned() else { return };
                ui.heading("Which machine?");
                Self::para(ui, wizard::MACHINE_HELP);
                ui.label(format!("The disk reads as: {}", crate::cpm::console::machine_label(&boot.machine)));
                let current = self.plan.machine_in_force(&boot);
                for m in wizard::machine_choices(&boot) {
                    let detected = m.key == boot.machine;
                    let label = if detected { format!("{}  (what the disk was read as)", m.description) } else { m.description.to_string() };
                    if ui.radio(current == m.key, label).clicked() {
                        self.plan.choose_machine(m.key, &boot);
                    }
                }
            }
            Step::Cpu => {
                ui.heading("Which processor?");
                Self::para(ui, wizard::CPU_HELP);
                let (adv, why) = wizard::advised_cpu(self.plan.runs, self.plan.boot_in(disks));
                ui.label(
                    egui::RichText::new(format!("Advised: {}, {why}.", crate::cpm::cpu::cpu_short(adv))).color(AMBER),
                );
                // Every processor the settings take.
                for (v, l) in crate::cpm::cpu::CPU_CHOICES {
                    let mark = if *v == adv { " (advised)" } else { "" };
                    let name = crate::cpm::cpu::choice_name(l);
                    ui.radio_value(&mut self.plan.cpu, v.to_string(), format!("{name}{mark}"));
                }
            }
            Step::Rom => {
                ui.heading("Monitor ROM");
                Self::para(ui, wizard::ROM_HELP);
                // Through `choose_rom`, which knows which ROM the disk wants.
                let was = self.plan.rom != crate::cpm::rom::ROM_OFF;
                let mut load = was;
                ui.radio_value(&mut load, true, "Load the ROM (advised)");
                ui.radio_value(&mut load, false, "No ROM");
                // The step is reached only with a boot disk that wants a ROM
                // (`Plan::needs_rom`), so it is there.
                if load != was
                    && let Some(boot) = self.plan.boot_in(disks)
                {
                    self.plan.choose_rom(load, boot);
                }
            }
            Step::ExtraDisks => {
                let Some(boot) = self.plan.boot_in(disks).cloned() else { return };
                let machine = self.plan.machine_in_force(&boot);
                let room = wizard::extra_disk_room(&boot, &machine);
                // Every slot's name with the board built once: this runs
                // every frame.
                let names = wizard::slot_names(&boot, &machine);
                let name = |s: u8| names.get(s as usize).cloned().unwrap_or_else(|| format!("slot {s}"));
                ui.heading("More disks for the machine");
                Self::para(ui, wizard::EXTRA_DISKS_HELP);
                ui.label(egui::RichText::new(format!(
                    "{} is in {} of the {}, which has room for {room} more. Fill them in order: an empty slot \
                     between two full ones looks like a hang to a guest that picks it.",
                    boot.name,
                    name(0),
                    // The chosen machine's board, as the room and slot names
                    // beside it are -- not the one the disk was read as.
                    crate::cpm::boot_machine::BootMachine::board_for(Some(&machine), boot.bytes).unwrap_or("board"),
                )).color(AMBER_DIM));
                let list = wizard::extra_disk_choices(&boot, &machine, disks);
                if list.is_empty() {
                    ui.label(egui::RichText::new("No other disk here is on this board.").color(AMBER_DIM));
                }
                egui::ScrollArea::vertical().max_height(300.0).id_salt("cpmw_extra").show(ui, |ui| {
                    for slot in 1..=room {
                        if list.is_empty() {
                            break;
                        }
                        let label = format!("{}:", name(slot));
                        self.slot_combo(ui, slot, &label, "(empty)", &list);
                    }
                });
            }
            Step::Ready => {
                ui.heading("Ready");
                ui.label("These are the settings:");
                for l in &self.ready_lines {
                    ui.label(egui::RichText::new(format!("  {l}")).color(AMBER_BRIGHT));
                }
                ui.label(
                    egui::RichText::new("Disks mounted now and not listed here are taken off. Nothing has been saved yet.")
                        .color(AMBER_DIM),
                );
            }
        }
    }

    /// The save's answer.
    fn draw_done(&self, ui: &mut egui::Ui) {
        match &self.result {
            Some(Ok(notes)) => {
                ui.heading("Saved");
                for n in notes {
                    ui.label(egui::RichText::new(n).color(AMBER_DIM));
                }
                ui.label(egui::RichText::new(wizard::FINISH_LINE).strong().color(AMBER_BRIGHT));
            }
            Some(Err(e)) => {
                ui.label(egui::RichText::new(format!("Not saved: {e}")).color(WARN_BORDER));
            }
            None => {}
        }
    }

    /// One drive or slot: a disk from `list`, or none.
    fn slot_combo(&mut self, ui: &mut egui::Ui, n: u8, label: &str, none: &str, list: &[&DiskFacts]) {
        let current = self.plan.disks.iter().find(|(x, _)| *x == n).map(|(_, s)| s.clone());
        ui.horizontal(|ui| {
            ui.label(label);
            egui::ComboBox::from_id_salt(format!("cpmw_slot_{n}"))
                .width(260.0)
                .selected_text(current.clone().unwrap_or_else(|| none.to_string()))
                .show_ui(ui, |ui| {
                    if ui.selectable_label(current.is_none(), none).clicked() {
                        self.plan.disks.retain(|(x, _)| *x != n);
                    }
                    for d in list {
                        if ui.selectable_label(current.as_deref() == Some(d.name.as_str()), &d.name).clicked() {
                            self.plan.disks.retain(|(x, _)| *x != n);
                            self.plan.disks.push((n, d.name.clone()));
                            self.plan.disks.sort();
                            self.viewing = Some(d.name.clone());
                        }
                    }
                });
        });
    }

    /// `Next` (or Save, or Close).  `true` when the window should close.
    fn next(&mut self) -> bool {
        if self.done {
            return true;
        }
        match self.step {
            Step::EmulatorDrives | Step::ExtraDisks => self.go_on_checked(),
            Step::BootDisk if self.plan.boot_disk.is_none() => {
                self.msg = "Choose the disk to boot.".to_string();
            }
            Step::Ready => {
                let (tx, rx) = std::sync::mpsc::channel();
                let plan = self.plan.clone();
                let dir = self.transfer_dir.clone();
                std::thread::spawn(move || {
                    let _ = tx.send(wizard::apply(&plan, &dir));
                });
                self.saving = Some(rx);
            }
            _ => self.go_on(),
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The download is never on the way back, and a Download pressed while
    /// the host's one slot is taken (the panel's run, or its ROM fetch) is
    /// refused rather than waiting on that other run and reporting it.
    #[test]
    fn test_the_download_step_is_left_behind_and_never_borrowed() {
        let me = include_str!("cpm_wizard.rs").replace('\r', "");
        let go_on = &me[me.find("    fn go_on(&mut self)").expect("go_on")..];
        let go_on = &go_on[..go_on.find("\n    }\n").expect("its end")];
        assert!(go_on.contains("self.trail.push(self.step)"), "positive control: go_on keeps the trail");
        assert!(go_on.contains("if self.step.on_trail() {"), "Back returns to the download");
        let gui = include_str!("../gui.rs").replace('\r', "");
        let arm = &gui[gui.find("cpm_wizard::Outcome::Fetch =>").expect("the host's Fetch arm")..];
        let arm = &arm[..arm.find("\n                }\n").expect("its end")];
        assert!(arm.contains("if !self.start_cpm_fetch(&ctx) {") && arm.contains("w.fetch_refused()"), "{arm}");
        let start = &gui[gui.find("fn start_cpm_fetch(").unwrap()..];
        let start = &start[..start.find("\n    }\n").unwrap()];
        assert!(start.contains("return false;") && start.trim_end().ends_with("true"), "{start}");
        // Refused, it waits for the slot to free and reads the disks again:
        // the run that held it may have been the sample disks.
        let refused = &me[me.find("pub(super) fn fetch_refused(").unwrap()..];
        let refused = &refused[..refused.find("\n    }\n").unwrap()];
        assert!(refused.contains("self.slot_wait = true;"), "a refused Download never looks again");
        let host = &gui[gui.find("if let Some(mut w) = self.cpm_wizard.take()").unwrap()..];
        assert!(host[..host.find("let mut open = true;").unwrap()].contains("if w.waiting() {"), "the slot is not watched after a refusal");
    }

    /// The desktop takes every key the wizard writes back from the global
    /// config after a save.  A key missing from that copy is one the
    /// window's next Save would quietly put back as it was.
    #[test]
    fn test_the_host_takes_every_key_the_wizard_writes() {
        let gui = include_str!("../gui.rs").replace('\r', "");
        let f = &gui[gui.find("fn take_cpm_keys_from_global(").expect("the copy")..];
        let f = &f[..f.find("\n    }\n").expect("its end")];
        let mut keys: Vec<&str> = Plan::emulator().settings().iter().map(|(k, _)| *k).collect();
        keys.push("cpm_mounts");
        for k in keys {
            assert!(f.contains(&format!("cfg.{k} = g.{k}")), "{k} is written by the wizard but not taken back");
        }
    }
}
