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
use crate::cpm::wizard::{self, DiskFacts, Plan, Runs, Step};

/// What the window asks its host to do after this frame.
pub(super) enum Outcome {
    Continue,
    /// Closed without saving, or closed after.
    Close,
    /// Saved: the host must take the CP/M keys from the global config into its
    /// own copy, or its next Save would write the old ones back.
    Saved,
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
    loading: Option<Receiver<Vec<DiskFacts>>>,
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
        let (tx, rx) = std::sync::mpsc::channel();
        let dir = transfer_dir.to_string();
        std::thread::spawn(move || {
            let _ = tx.send(wizard::all_disks(&wizard::cpm_base(&dir)));
        });
        CpmWizard {
            transfer_dir: transfer_dir.to_string(),
            step: Step::Runs,
            done: false,
            trail: Vec::new(),
            disks: None,
            loading: Some(rx),
            saving: None,
            result: None,
            plan: Plan::emulator(),
            viewing: None,
            msg: String::new(),
            ready_lines: Vec::new(),
        }
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
        self.trail.push(self.step);
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
                Ok(d) => {
                    self.disks = Some(d);
                    self.loading = None;
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
                    if ui.add(egui::Button::new(egui::RichText::new(label).strong().color(AMBER_BRIGHT))).clicked() {
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
        egui::ScrollArea::vertical().max_height(360.0).id_salt("cpmw_listing").show(ui, |ui| {
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
            Step::Runs => {
                ui.heading("What should CP/M run?");
                Self::para(ui, wizard::RUNS_HELP);
                let can_boot = disks.iter().any(|d| d.boots);
                // Through `choose_runs`, which knows what each answer keeps.
                let mut runs = self.plan.runs;
                ui.radio_value(&mut runs, Runs::Emulator, wizard::EMULATOR_LABEL);
                if can_boot {
                    ui.radio_value(&mut runs, Runs::Boot, wizard::BOOT_LABEL);
                }
                if runs != self.plan.runs {
                    self.plan.choose_runs(runs);
                }
                if !can_boot {
                    ui.label(
                        egui::RichText::new(
                            "No disk image here can boot yet: Download Sample Disks first, then open the wizard again to boot one.",
                        )
                        .color(AMBER_DIM),
                    );
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
