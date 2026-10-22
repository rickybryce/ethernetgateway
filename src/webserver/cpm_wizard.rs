//! The CP/M setup wizard on the web page.
//!
//! Every question, recommendation and setting is [`crate::cpm::wizard`]'s;
//! this is the modal that draws it.  The page asks `/cpm-wizard/disks` once
//! when the wizard opens -- what each disk is, its listing, and for every
//! machine that could boot it, its board, slot count and slot names -- and
//! walks the steps in the browser, so a step back costs nothing.
//!
//! **The browser decides nothing.**  Its plan starts as the gateway's, and
//! each answer goes to `/cpm-wizard/choose`, which applies it with
//! [`Plan`]'s own `choose_*` and sends the plan back -- what an answer keeps
//! and what it starts afresh is decided in one place for all three surfaces.
//! The page only *sets* the drives and slots it shows, which are values, not
//! rules.  The order of the steps is
//! [`wizard::Step::next`]'s, sent as a table; at a disks step and on the way
//! to Ready the answers so far go to `/cpm-wizard/check`, which runs
//! [`wizard::check_plan`] and sends back [`Plan::describe`]'s lines for the
//! Ready screen; and `/cpm-wizard/apply` checks them all again, because a
//! form is only what a browser chose to send.

use super::{html_escape, json_escape};
use crate::cpm::wizard::{self, DiskFacts, Plan, Runs};

/// The facts the wizard needs, as JSON.  Blocking: it reads every image.
pub(super) fn disks_json(transfer_dir: &str) -> String {
    let base = wizard::cpm_base(transfer_dir);
    let disks = wizard::all_disks(&base);
    let mut out = String::from("{\"machines\":[");
    let machines = crate::cpm::console::MACHINE_CHOICES;
    for (i, m) in machines.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!(
            "{{\"key\":\"{}\",\"label\":\"{}\"}}",
            json_escape(m.key),
            json_escape(m.description)
        ));
    }
    // The processors the settings take, so the page offers no other list.
    out.push_str("],\"cpus\":[");
    let cpus: Vec<String> = crate::cpm::cpu::CPU_CHOICES
        .iter()
        .map(|(v, l)| {
            format!("{{\"key\":\"{}\",\"name\":\"{}\"}}", json_escape(v), json_escape(crate::cpm::cpu::choice_name(l)))
        })
        .collect();
    out.push_str(&cpus.join(","));
    out.push_str("],\"disks\":[");
    for (i, d) in disks.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&disk_json(d, &disks));
    }
    // The plan the page starts from, and the first answer to mark advised,
    // so it has no idea of its own of what an untouched wizard holds.
    out.push_str(&format!(
        "],\"advised\":\"{}\",\"plan\":{}",
        runs_key(wizard::opening_runs(&disks)),
        plan_json(&Plan::opening(&disks), &disks)
    ));
    // The download step, while the sample disks are not here.
    match wizard::sample_offer(&base) {
        Some(o) => out.push_str(&format!(
            ",\"offer\":{{\"disks\":{},\"mb\":{:.0},\"from\":\"{}\"}}}}",
            o.disks,
            o.megabytes,
            json_escape(&crate::cpm::fetch::source_repos().join(" and "))
        )),
        None => out.push_str(",\"offer\":null}"),
    }
    out
}

fn runs_key(runs: Runs) -> &'static str {
    match runs {
        Runs::Emulator => "emulator",
        Runs::Boot => "boot",
    }
}

/// The wizard's download step: the sample disks fetched -- blocking, about a
/// minute, as the panel's own Download is -- and the disks read again, with
/// what the run came to as `note`.
pub(super) fn fetch_json(transfer_dir: &str) -> String {
    let base = wizard::cpm_base(transfer_dir);
    let note = match crate::cpm::fetch::download_missing(&base, crate::cpm::fetch::log_progress) {
        Ok(r) => {
            let mut m = r.summary();
            for line in r.failure_lines(3) {
                m.push_str(&format!(" {line}."));
            }
            m
        }
        Err(e) => e,
    };
    crate::glog!("Web: CP/M sample disks (setup wizard): {}", note.trim());
    let disks = disks_json(transfer_dir);
    format!("{{\"note\":\"{}\",{}", json_escape(note.trim()), &disks[1..])
}

/// What `/cpm-wizard/disks` answers when reading the images failed: no disks,
/// and still the plan the page starts from -- without one the page has
/// nothing to draw and stays on "Reading the disk images" -- with the reason
/// to show.
pub(super) fn disks_fallback() -> String {
    format!(
        "{{\"machines\":[],\"cpus\":[],\"disks\":[],\"advised\":\"emulator\",\"plan\":{},\"offer\":null,\"error\":\"{}\"}}",
        plan_json(&Plan::emulator(), &[]),
        json_escape(DISKS_UNREAD)
    )
}

/// What the page says when the images could not be read -- not the "no disk
/// can boot" an empty list would otherwise suggest.
pub(super) const DISKS_UNREAD: &str = "The disk images could not be read; the gateway's log says why.";

/// A plan as the page holds it, with the two things the page draws from it
/// that are rules rather than values: the machine in force (`auto` resolved by
/// [`Plan::machine_in_force`]) and whether the next step is the ROM's.
pub(super) fn plan_json(plan: &Plan, all: &[DiskFacts]) -> String {
    let boot = plan.boot_in(all);
    let in_force = boot.map(|b| plan.machine_in_force(b));
    let disks: Vec<String> =
        plan.disks.iter().map(|(n, name)| format!("\"{n}\":\"{}\"", json_escape(name))).collect();
    let opt = |v: Option<&str>| v.map(|s| format!("\"{}\"", json_escape(s))).unwrap_or_else(|| "null".into());
    format!(
        "{{\"runs\":\"{}\",\"boot\":{},\"machine\":\"{}\",\"inForce\":{},\"cpu\":\"{}\",\"rom\":\"{}\",\
         \"needsRom\":{},\"disks\":{{{}}}}}",
        runs_key(plan.runs),
        opt(plan.boot_disk.as_deref()),
        json_escape(&plan.machine),
        opt(in_force.as_deref()),
        json_escape(&plan.cpu),
        json_escape(&plan.rom),
        plan.needs_rom(all),
        disks.join(",")
    )
}

/// One answer applied to the plan the page holds -- by [`Plan`]'s own rules,
/// so the page has none to drift.  `what` names the question, `value` the
/// answer; anything a picker would not have offered is refused.
pub(super) fn choose(
    form: &std::collections::HashMap<String, String>,
    all: &[DiskFacts],
) -> Result<Plan, String> {
    let mut plan = parse_form(form, false)?;
    let value = form.get("value").map(|s| s.trim()).unwrap_or_default();
    let boot = |plan: &Plan| plan.boot_in(all).cloned().ok_or_else(|| "choose the disk to boot".to_string());
    let what = form.get("what").map(String::as_str).unwrap_or_default();
    // The disk questions belong to a boot: the page asks them only there.
    if matches!(what, "boot" | "machine" | "rom") && plan.runs != Runs::Boot {
        return Err("choose to boot a disk first".to_string());
    }
    match what {
        // Boot even with nothing here that boots: the operator may have images
        // of their own to add, and the boot-disk step says where.
        "runs" => plan.choose_runs(match value {
            "emulator" => Runs::Emulator,
            "boot" => Runs::Boot,
            _ => return Err("Choose one of the two.".to_string()),
        }),
        "boot" => {
            let d = all.iter().find(|d| d.name == value && d.boots).ok_or("Choose the disk to boot.")?;
            plan.choose_boot_disk(d);
        }
        "machine" => {
            let b = boot(&plan)?;
            if !wizard::machine_choices(&b).iter().any(|m| m.key == value) {
                return Err("Choose the machine.".to_string());
            }
            plan.choose_machine(value, &b);
        }
        "rom" => {
            let load = match value {
                "yes" => true,
                "no" => false,
                _ => return Err("Load the ROM, or not.".to_string()),
            };
            let b = boot(&plan)?;
            plan.choose_rom(load, &b);
        }
        "cpu" => plan.choose_cpu(value).map_err(|_| "Choose the processor.".to_string())?,
        _ => return Err("an answer to no question".to_string()),
    }
    Ok(plan)
}

/// The outcome of a choice, as JSON for the page: the plan to hold now.
pub(super) fn choose_json(result: Result<Plan, String>, all: &[DiskFacts]) -> String {
    match result {
        Ok(plan) => format!("{{\"ok\":true,\"plan\":{}}}", plan_json(&plan, all)),
        Err(e) => format!("{{\"ok\":false,\"error\":\"{}\"}}", json_escape(&e)),
    }
}

/// One disk, and how each machine that takes it would carry it -- including
/// which of `all` could ride along with it there, from
/// [`wizard::extra_disk_choices`] so the page has no rule of its own to drift.
fn disk_json(d: &DiskFacts, all: &[DiskFacts]) -> String {
    let (cpu, why) = wizard::recommended_cpu(d);
    let listing: Vec<String> = d.listing().iter().map(|l| format!("\"{}\"", json_escape(l))).collect();
    let mut on = Vec::new();
    for m in wizard::machine_choices(d) {
        let board = crate::cpm::boot_machine::BootMachine::board_for(Some(m.key), d.bytes).unwrap_or("");
        let room = wizard::extra_disk_room(d, m.key);
        let names: Vec<String> =
            wizard::slot_names(d, m.key).iter().map(|n| format!("\"{}\"", json_escape(n))).collect();
        let extra: Vec<String> = if d.boots {
            wizard::extra_disk_choices(d, m.key, all)
                .iter()
                .map(|x| format!("\"{}\"", json_escape(&x.name)))
                .collect()
        } else {
            Vec::new()
        };
        on.push(format!(
            "\"{}\":{{\"board\":\"{}\",\"room\":{room},\"slots\":[{}],\"extra\":[{}]}}",
            json_escape(m.key),
            json_escape(board),
            names.join(","),
            extra.join(",")
        ));
    }
    format!(
        "{{\"name\":\"{}\",\"boots\":{},\"mountable\":{},\"summary\":\"{}\",\"listing\":[{}],\
         \"machine\":\"{}\",\"basic\":{},\"rom\":{},\"cpu\":\"{}\",\"cpuWhy\":\"{}\",\"on\":{{{}}}}}",
        json_escape(&d.name),
        d.boots,
        d.mountable(),
        json_escape(&d.summary),
        listing.join(","),
        json_escape(&d.machine),
        d.altair_basic,
        wizard::rom_for(d).map(|r| format!("\"{}\"", json_escape(r))).unwrap_or_else(|| "null".into()),
        cpu,
        json_escape(why),
        on.join(",")
    )
}

/// The answers, parsed, as a plan.  Every field's shape is checked here --
/// the form came from a browser -- and the plan as a whole by
/// [`wizard::check_plan`], which the check route and [`wizard::apply`] run.
pub(super) fn plan_from_form(form: &std::collections::HashMap<String, String>) -> Result<Plan, String> {
    parse_form(form, true)
}

/// [`plan_from_form`], or -- `finished` false -- a plan still being answered,
/// which may be booting a disk not yet chosen (Boot picked, the disk next).
fn parse_form(form: &std::collections::HashMap<String, String>, finished: bool) -> Result<Plan, String> {
    let get = |k: &str| form.get(k).map(|s| s.trim().to_string()).unwrap_or_default();
    let mut plan = match get("runs").as_str() {
        "emulator" => Plan::emulator(),
        "boot" => {
            let disk = get("boot");
            let mut p = Plan::emulator();
            p.runs = Runs::Boot;
            if !finished && disk.is_empty() {
                p.boot_disk = None;
            } else if !crate::cpm::image::is_safe_image_name(&disk) {
                return Err("choose a boot disk".to_string());
            } else {
                p.boot_disk = Some(disk);
            }
            p
        }
        _ => return Err("choose what CP/M runs".to_string()),
    };
    plan.choose_cpu(&get("cpu"))?;
    if plan.runs == Runs::Boot {
        let machine = get("machine");
        if !crate::cpm::console::is_valid_machine_key(&machine) {
            return Err("choose a machine".to_string());
        }
        plan.machine = machine;
        let rom = get("rom");
        if !crate::cpm::rom::is_valid_rom_key(&rom) {
            return Err("choose a monitor ROM, or none".to_string());
        }
        plan.rom = rom;
    }
    // `disks` is `n=NAME` pairs, `;`-separated: n is the drive under the
    // emulator (1-15, B: to P:) and the slot under a boot.
    for pair in get("disks").split(';').filter(|p| !p.is_empty()) {
        let (n, name) = pair.split_once('=').ok_or("a disk was sent without its drive")?;
        let n: u8 = n.parse().map_err(|_| "a drive number did not parse".to_string())?;
        if !(1..crate::cpm::NUM_DRIVES).contains(&n) || !crate::cpm::image::is_safe_image_name(name) {
            return Err(format!("{name} cannot go on drive {n}"));
        }
        if plan.disks.iter().any(|(d, x)| *d == n || x == name) {
            return Err(format!("{name} is chosen twice, or its drive is"));
        }
        plan.disks.push((n, name.to_string()));
    }
    plan.disks.sort();
    Ok(plan)
}

/// The outcome of a check, as JSON for the page: the Ready screen's lines.
pub(super) fn check_json(result: Result<Vec<String>, String>) -> String {
    match result {
        Ok(lines) => {
            let lines: Vec<String> = lines.iter().map(|l| format!("\"{}\"", json_escape(l))).collect();
            format!("{{\"ok\":true,\"lines\":[{}]}}", lines.join(","))
        }
        Err(e) => format!("{{\"ok\":false,\"error\":\"{}\"}}", json_escape(&e)),
    }
}

/// Check a plan against the disks as they are, and describe it.  Blocking.
pub(super) fn check(plan: &Plan, transfer_dir: &str) -> Result<Vec<String>, String> {
    wizard::check_plan(plan, &wizard::all_disks(&wizard::cpm_base(transfer_dir)))?;
    Ok(plan.describe(transfer_dir))
}

/// [`wizard::Step::table`] as a JSON object for the page.
fn steps_json() -> String {
    let pairs: Vec<String> = wizard::Step::table()
        .iter()
        .map(|(k, v)| format!("\"{}\":\"{}\"", json_escape(k), json_escape(v)))
        .collect();
    format!("{{{}}}", pairs.join(","))
}

/// The outcome of applying, as JSON for the page.
pub(super) fn apply_json(result: Result<Vec<String>, String>) -> String {
    match result {
        Ok(notes) => {
            let notes: Vec<String> = notes.iter().map(|n| format!("\"{}\"", json_escape(n))).collect();
            format!("{{\"ok\":true,\"notes\":[{}]}}", notes.join(","))
        }
        Err(e) => format!("{{\"ok\":false,\"error\":\"{}\"}}", json_escape(&e)),
    }
}

/// A line list of explanation as one paragraph, HTML-escaped: the lines are
/// wrapped for a C64, and a browser wraps its own.
fn para(lines: &[&str]) -> String {
    lines
        .split(|l| l.is_empty())
        .map(|p| format!("<p>{}</p>", html_escape(&p.join(" "))))
        .collect()
}

/// The modal.
///
/// **No control in it is ever submitted.**  It sits inside the page's one form
/// (the page has exactly one, and a second would not nest), so a named input
/// would ride along with an ordinary Save.  Choices are therefore buttons --
/// `type="button"`, which a form never sends -- and the drive and slot lists
/// are nameless selects.  The answers go to `/cpm-wizard/apply` by `fetch`.
pub(super) fn render_modal() -> String {
    format!(
        "<style>.cpmw-opt{{display:block;width:100%;text-align:left;margin:4px 0}}\
         .cpmw-opt.cpmw-on{{outline:2px solid var(--amber-bright)}}\
         .cpmw-files pre{{max-height:40vh;overflow:auto;columns:14em}}\
         #cpmw-msg{{color:var(--warn-border)}}</style>\
         <div class=\"modal\" id=\"cpm-wizard\"><div class=\"modal-body wide\">\
         <div class=\"modal-head\"><span class=\"title\">CP/M Setup Wizard</span>\
         <button type=\"button\" class=\"close\" data-close=\"cpm-wizard\">\u{00d7}</button></div>\
         <p id=\"cpmw-msg\"></p>\
         <div id=\"cpmw-step\" class=\"cpmw\"><p class=\"sub\">Reading the disk images\u{2026}</p></div>\
         <div class=\"modal-foot\">\
         <button type=\"button\" class=\"secondary\" id=\"cpmw-back\">Back</button> \
         <button type=\"button\" class=\"secondary\" id=\"cpmw-next\">Next</button></div>\
         </div></div>\
         <template id=\"cpmw-text\" \
          data-runs=\"{runs}\" data-drives=\"{drives}\" data-bootdisk=\"{bootdisk}\" \
          data-machine=\"{machine}\" data-cpu=\"{cpu}\" data-extra=\"{extra}\" data-rom=\"{rom}\" \
          data-z80why=\"{z80why}\" data-emucpu=\"{emucpu}\" data-next=\"{next}\" \
          data-finish=\"{finish}\" data-emulator=\"{emu}\" data-boot=\"{boot}\" \
          data-fetch=\"{fetch}\" data-fetchlabel=\"{fetchlabel}\" data-decline=\"{decline}\" \
          data-noboot=\"{noboot}\"></template>",
        runs = html_escape(&para(wizard::RUNS_HELP)),
        drives = html_escape(&para(wizard::EMULATOR_DRIVES_HELP)),
        bootdisk = html_escape(&para(wizard::BOOT_DISK_HELP)),
        machine = html_escape(&para(wizard::MACHINE_HELP)),
        cpu = html_escape(&para(wizard::CPU_HELP)),
        extra = html_escape(&para(wizard::EXTRA_DISKS_HELP)),
        rom = html_escape(&para(wizard::ROM_HELP)),
        z80why = html_escape(wizard::Z80_WHY),
        emucpu = html_escape(wizard::EMULATOR_CPU),
        next = html_escape(&steps_json()),
        finish = html_escape(wizard::FINISH_LINE),
        emu = html_escape(wizard::EMULATOR_LABEL),
        boot = html_escape(wizard::BOOT_LABEL),
        fetch = html_escape(&para(wizard::FETCH_HELP)),
        fetchlabel = html_escape(wizard::FETCH_LABEL),
        decline = html_escape(wizard::DECLINE_LABEL),
        noboot = html_escape(&para(wizard::NO_BOOT_DISK)),
    )
}

/// The wizard's script.  Steps are functions that draw into `#cpmw-step` and
/// return the function `Next` runs; `Back` replays the trail.  The disk facts
/// are fetched each time the wizard opens, so a disk downloaded since the page
/// loaded is offered.
pub(super) const SCRIPT: &str = r#"<script>
(function() {
  var T = document.getElementById('cpmw-text');
  if (!T) return;
  var box = document.getElementById('cpmw-step');
  var msg = document.getElementById('cpmw-msg');
  var back = document.getElementById('cpmw-back');
  var next = document.getElementById('cpmw-next');
  var D = null, plan = null, trail = [], step = null, view = null, saved = false, lines = [];
  // **One request at a time, and only for the wizard that asked.**  Every
  // request goes through `request`.  While one is out, Next and Back do
  // nothing, so no answer can land after the operator has moved on and two
  // quick Nexts cannot cross.  Each opening of the wizard is a generation of
  // its own (`opened`): a reply to an earlier opening -- the disk list, an
  // answer, a failure -- is never delivered.  The one exception is a save,
  // which has changed the settings whatever is open by then (see `save`).
  var opened = 0, busy = false;
  // Where each step leads: `wizard::Step::next`, sent as a table.
  var N = JSON.parse(T.dataset.next);
  function nextOf(s) { return N[s + '|' + plan.runs + '|' + (plan.needsRom ? 1 : 0)] || null; }
  // **After a save, the page reloads however the wizard is closed** -- Close,
  // the x, a click outside.  The page's one form still holds the CP/M fields as
  // they were before the wizard, and its next Save would write every one of
  // them back over the wizard's answers.
  new MutationObserver(function() {
    if (saved && !document.getElementById('cpm-wizard').classList.contains('open')) {
      saved = false; location.href = '/';
    }
  }).observe(document.getElementById('cpm-wizard'), { attributes: true, attributeFilter: ['class'] });
  // Quotes too: names go inside attribute values (`data-v`, `<option value>`),
  // and an image's file name may hold either.
  function esc(s) {
    return String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;').replace(/'/g, '&#39;');
  }
  function say(s) { msg.textContent = s || ''; }
  function disk(n) { return D.disks.filter(function(x) { return x.name === n; })[0]; }
  function machineLabel(k) { var m = D.machines.filter(function(x) { return x.key === k; })[0]; return m ? m.label : k; }
  function listing(d) {
    return '<div class="cpmw-files"><b>' + esc(d.name) + '</b> <span class="sub">' + esc(d.summary) +
      '</span><pre>' + d.listing.map(esc).join('\n') + '</pre></div>';
  }
  function show(d) { var v = document.getElementById('cpmw-view'); if (v) v.innerHTML = d ? listing(d) : ''; }
  // A list of choices as buttons: `items` are [value, label, note].
  function choices(group, items, chosen) {
    return '<div data-group="' + group + '">' + items.map(function(it) {
      return '<button type="button" class="secondary cpmw-opt' + (it[0] === chosen ? ' cpmw-on' : '') +
        '" data-v="' + esc(it[0]) + '">' + esc(it[1]) + (it[2] ? ' <span class="sub">' + esc(it[2]) + '</span>' : '') + '</button>';
    }).join('') + '</div>';
  }
  function wire(group, onPick) {
    var g = box.querySelector('[data-group="' + group + '"]');
    if (!g) return;
    g.querySelectorAll('button').forEach(function(b) {
      b.addEventListener('click', function() {
        g.querySelectorAll('button').forEach(function(x) { x.classList.remove('cpmw-on'); });
        b.classList.add('cpmw-on');
        onPick(b.dataset.v);
      });
    });
  }
  function picked(group) {
    var b = box.querySelector('[data-group="' + group + '"] button.cpmw-on');
    return b ? b.dataset.v : null;
  }
  function slotRows(attr, count, first, list, chosen, label) {
    var rows = '';
    for (var n = first; n < first + count; n++) {
      var opts = '<option value="">' + esc(label.empty) + '</option>' + list.map(function(d) {
        return '<option value="' + esc(d.name) + '"' + (chosen[n] === d.name ? ' selected' : '') + '>' + esc(d.name) + '</option>';
      }).join('');
      rows += '<div class="row"><label>' + esc(label.of(n)) + ': <select data-' + attr + '="' + n + '">' + opts + '</select></label></div>';
    }
    return rows;
  }
  // The rows as chosen.  Whether they may be is the gateway's question
  // (`/cpm-wizard/check`), asked when Next leaves the step.
  function readRows(attr) {
    var out = {};
    box.querySelectorAll('select[data-' + attr + ']').forEach(function(s) {
      if (s.value) out[s.dataset[attr]] = s.value;
    });
    return out;
  }
  var steps = {
    // Offered first while the sample disks are not here (`D.offer`), and
    // answered by its own two buttons: Next is hidden on it (see `go`).
    // Never on the trail (`Step::on_trail`): Back from the step after it
    // closes nothing and returns to nothing -- opening again offers it.
    fetch: function() {
      var o = D.offer;
      box.innerHTML = T.dataset.fetch +
        (o ? '<p class="sub">' + esc(o.disks + ' disks, ' + o.mb + ' MB, from ' + o.from + '.') + '</p>' : '') +
        '<p><button type="button" class="cpmw-opt cpmw-on" id="cpmw-get">' + esc(T.dataset.fetchlabel) + '</button> ' +
        '<button type="button" class="secondary cpmw-opt" id="cpmw-skip">' + esc(T.dataset.decline) + '</button></p>';
      document.getElementById('cpmw-get').addEventListener('click', function() {
        if (busy) return;
        say('Downloading… this takes a minute.');
        request('/cpm-wizard/fetch', '').then(function(j) {
          if (j.error) { box.innerHTML = '<p>' + esc(j.error) + '</p>'; say(''); return; }
          D = j; plan = j.plan; go('runs');
          if (j.note) say('Sample disks: ' + j.note);
        }).catch(function() { say('The gateway did not answer.'); });
      });
      document.getElementById('cpmw-skip').addEventListener('click', function() {
        if (!busy) go('runs');
      });
      return function() { return null; };
    },
    // Both answers always -- Boot too with nothing here that boots, for an
    // operator with images of their own (the next step says where) -- and
    // the gateway's first answer (`wizard::opening_runs`) marked.
    runs: function() {
      var items = [['emulator', T.dataset.emulator, D.advised === 'emulator' ? 'advised' : ''],
        ['boot', T.dataset.boot, D.advised === 'boot' ? 'advised' : '']];
      box.innerHTML = T.dataset.runs + choices('runs', items, plan.runs);
      wire('runs', function() {});
      return function() {
        var v = picked('runs');
        if (!v) { say('Choose one of the two.'); return null; }
        return chose('runs', v, 'runs');
      };
    },
    drives: function() {
      var list = D.disks.filter(function(d) { return d.mountable; });
      box.innerHTML = T.dataset.drives + (list.length ?
        slotRows('drive', 15, 1, list, plan.disks, { empty: '(its folder)', of: function(n) { return String.fromCharCode(65 + n); } }) :
        '<p class="sub">No disk image here has CP/M files to put on a drive; this step can be skipped.</p>') +
        '<div id="cpmw-view"></div>';
      box.querySelectorAll('select[data-drive]').forEach(function(s) {
        s.addEventListener('change', function() { show(disk(s.value)); });
      });
      return function() {
        plan.disks = readRows('drive'); return nextOf('drives');
      };
    },
    bootdisk: function() {
      var list = D.disks.filter(function(d) { return d.boots; });
      if (!list.length) {
        box.innerHTML = T.dataset.noboot;
        return function() { say('No disk here boots: go back, or close the wizard.'); return null; };
      }
      box.innerHTML = T.dataset.bootdisk +
        choices('boot', list.map(function(d) { return [d.name, d.name, d.summary.split(' -- ')[1] || '']; }), plan.boot) +
        '<div id="cpmw-view"></div>';
      wire('boot', function(n) { show(disk(n)); });
      if (plan.boot) show(disk(plan.boot));
      return function() {
        var n = picked('boot');
        if (!n) { say('Choose the disk to boot.'); return null; }
        return chose('boot', n, 'bootdisk');
      };
    },
    machine: function() {
      var d = disk(plan.boot);
      var items = Object.keys(d.on).map(function(k) {
        return [k, machineLabel(k), (k === d.machine ? 'what the disk was read as; ' : '') + d.on[k].board];
      });
      box.innerHTML = T.dataset.machine + '<p>The disk reads as <b>' + esc(machineLabel(d.machine)) + '</b>.</p>' +
        choices('machine', items, plan.inForce);
      wire('machine', function() {});
      return function() {
        var v = picked('machine');
        if (!v) { say('Choose the machine.'); return null; }
        return chose('machine', v, 'machine');
      };
    },
    cpu: function() {
      var adv = plan.runs === 'boot' ? disk(plan.boot).cpu : T.dataset.emucpu;
      var why = plan.runs === 'boot' ? disk(plan.boot).cpuWhy : T.dataset.z80why;
      // The processors the settings take (`cpu::CPU_CHOICES`), sent with the disks.
      var name = function(k) { var c = D.cpus.filter(function(x) { return x.key === k; })[0]; return c ? c.name : k; };
      box.innerHTML = T.dataset.cpu + '<p><b>Advised: ' + esc(name(adv)) + '</b>, ' + esc(why) + '.</p>' +
        choices('cpu', D.cpus.map(function(c) { return [c.key, c.name, c.key === adv ? 'advised' : '']; }), plan.cpu);
      wire('cpu', function() {});
      return function() {
        var v = picked('cpu');
        if (!v) { say('Choose the processor.'); return null; }
        return chose('cpu', v, 'cpu');
      };
    },
    rom: function() {
      box.innerHTML = T.dataset.rom +
        choices('rom', [['yes', 'Load the ROM', 'advised'], ['off', 'No ROM', '']], plan.rom === 'off' ? 'off' : 'yes');
      wire('rom', function() {});
      return function() {
        return chose('rom', picked('rom') === 'yes' ? 'yes' : 'no', 'rom');
      };
    },
    extra: function() {
      var d = disk(plan.boot), m = plan.inForce, on = d.on[m];
      var room = on ? on.room : 0;
      // Decided by the gateway (`wizard::extra_disk_choices`), not here.
      var list = (on ? on.extra : []).map(disk).filter(function(x) { return x; });
      box.innerHTML = T.dataset.extra + '<p class="sub">' + esc(d.name) + ' is in ' + esc(on ? on.slots[0] : 'slot 0') + ' of the ' +
        esc(on ? on.board : 'board') + ', which has room for ' + room + ' more. Fill them in order: an empty slot ' +
        'between two full ones looks like a hang to a guest that picks it.</p>' +
        (list.length && room ? slotRows('slot', room, 1, list, plan.disks, { empty: '(empty)', of: function(n) { return on.slots[n]; } }) :
          '<p class="sub">No other disk here is on this board.</p>') + '<div id="cpmw-view"></div>';
      box.querySelectorAll('select[data-slot]').forEach(function(sel) {
        sel.addEventListener('change', function() { show(disk(sel.value)); });
      });
      return function() {
        plan.disks = readRows('slot'); return nextOf('extra');
      };
    },
    // The lines are `Plan::describe`'s, from the check that led here.
    ready: function() {
      box.innerHTML = '<p><b>Ready.</b> These are the settings:</p><ul>' +
        lines.map(function(l) { return '<li>' + esc(l) + '</li>'; }).join('') +
        '</ul><p class="sub">Disks mounted now and not listed here are taken off. Nothing has been saved yet.</p>';
      next.textContent = 'Save';
      return function() { save(); return null; };
    },
    done: function() { return function() { closeModal('cpm-wizard'); location.href = '/'; return null; }; }
  };
  function go(s) {
    step = s; say(''); next.textContent = 'Next';
    next.style.visibility = s === 'fetch' ? 'hidden' : 'visible';
    view = steps[s]();
    back.style.visibility = trail.length ? 'visible' : 'hidden';
  }
  function form() {
    var body = 'runs=' + encodeURIComponent(plan.runs) + '&cpu=' + encodeURIComponent(plan.cpu);
    if (plan.runs === 'boot') {
      body += '&boot=' + encodeURIComponent(plan.boot || '') + '&machine=' + encodeURIComponent(plan.machine) +
        '&rom=' + encodeURIComponent(plan.rom);
    }
    return body + '&disks=' + encodeURIComponent(Object.keys(plan.disks).map(function(k) { return k + '=' + plan.disks[k]; }).join(';'));
  }
  // Busy holds every control, the step's choices included: a pick changed
  // while its answer is out would be shown and then not be the one applied.
  function setBusy(b) {
    busy = b; next.disabled = b; back.disabled = b;
    box.querySelectorAll('button, select').forEach(function(c) { c.disabled = b; });
  }
  // A request to the gateway, answered as JSON.  An answer, or failure, that
  // arrives after the wizard was opened again never settles, so nothing after
  // it runs -- unless `always`: a save's outcome is the operator's whatever
  // has opened since (see `save`).
  function request(url, body, always) {
    var mine = opened;
    setBusy(true);
    var opt = body === null ? undefined :
      { method: 'POST', headers: { 'Content-Type': 'application/x-www-form-urlencoded' }, body: body };
    return fetch(url, opt).then(function(r) { return r.json(); }).then(function(j) {
      if (mine !== opened && !always) return new Promise(function() {});
      setBusy(false); return j;
    }, function(e) {
      if (mine !== opened && !always) return new Promise(function() {});
      setBusy(false); throw e;
    });
  }
  function post(url, extra) { return request(url, form() + (extra || '')); }
  // An answer, applied by the gateway (`Plan::choose_*`): what it keeps and
  // what it starts afresh is decided there, once, for all three surfaces.
  // Resolves to the step after `from`, or null when the answer was refused.
  function chose(what, value, from) {
    return post('/cpm-wizard/choose', '&what=' + encodeURIComponent(what) + '&value=' + encodeURIComponent(value))
      .then(function(j) {
        if (!j.ok) { say(j.error); return null; }
        plan = j.plan; return nextOf(from);
      });
  }
  // On from `step` to `to`, the plan checked first where the disks were
  // chosen or the summary is next: the gateway's rules, said while the
  // operator can still change the answer.
  function advance(to) {
    if (to !== 'ready' && step !== 'drives' && step !== 'extra') { trail.push(step); go(to); return; }
    say('Checking…');
    post('/cpm-wizard/check').then(function(j) {
      if (!j.ok) { say(j.error); return; }
      lines = j.lines; trail.push(step); go(to);
    }).catch(function() { say('The gateway did not answer.'); });
  }
  // **A save's outcome is always shown, and the page reloads only when the
  // wizard is closed after one.**  The page's one form still holds the CP/M
  // fields as they were, and its next Save would write them back -- but a
  // reload the moment a save answers would take whatever the operator has
  // typed in that form since.  So a save that answers after the wizard was
  // closed opens it again on the outcome; one that answers after it was
  // opened again replaces the new wizard with it (the settings it started
  // from are gone).  Saved or not, nothing goes unsaid, and the reload is the
  // observer's, on the close that follows.
  function save() {
    var at = opened;
    // Not saved: said on the summary it was sent from -- or, if the wizard
    // was opened again since, in place of the new opening, which is dropped
    // as a success drops it; its disk list, arriving after, would otherwise
    // draw the first step over the outcome.
    function failed(s) {
      if (!document.getElementById('cpm-wizard').classList.contains('open')) openModal('cpm-wizard');
      if (at !== opened) {
        opened++; trail = []; step = 'failed';
        box.innerHTML = '';
        view = function() { closeModal('cpm-wizard'); return null; };
        next.textContent = 'Close'; next.style.visibility = 'visible'; back.style.visibility = 'hidden';
      }
      say(s);
    }
    say('Saving…');
    request('/cpm-wizard/apply', form(), true)
      .then(function(j) {
        if (!j.ok) { failed('Not saved: ' + j.error); return; }
        if (!document.getElementById('cpm-wizard').classList.contains('open')) openModal('cpm-wizard');
        opened++;
        say(''); trail = []; saved = true;
        box.innerHTML = '<p><b>Saved.</b></p>' +
          (j.notes || []).map(function(n) { return '<p class="sub">' + esc(n) + '</p>'; }).join('') +
          '<p><b>' + esc(T.dataset.finish) + '</b></p>';
        step = 'done'; view = steps.done(); next.textContent = 'Close'; next.style.visibility = 'visible';
        back.style.visibility = 'hidden';
      })
      .catch(function() { failed('The gateway did not answer: the settings may not have been saved.'); });
  }
  next.addEventListener('click', function() {
    if (!view || busy) return;
    // A step answers with the next step, or a promise of it when the gateway
    // has the answer to apply.
    Promise.resolve(view()).then(function(to) { if (to) advance(to); })
      .catch(function() { say('The gateway did not answer.'); });
  });
  back.addEventListener('click', function() { if (trail.length && !busy) go(trail.pop()); });
  document.querySelectorAll('button.more[data-target="cpm-wizard"]').forEach(function(b) {
    b.addEventListener('click', function() {
      box.innerHTML = '<p class="sub">Reading the disk images…</p>';
      opened++; setBusy(false); view = null; trail = []; say('');
      request('/cpm-wizard/disks', null).then(function(j) {
        // Unread images are said, and nothing else is drawn: an empty list
        // would advise downloading disks that are there, and its emulator
        // plan, saved, would take every mounted disk off.
        if (j.error) { box.innerHTML = '<p>' + esc(j.error) + '</p>'; return; }
        D = j; plan = j.plan; go(D.offer ? 'fetch' : 'runs');
      }).catch(function() { say('The gateway did not answer.'); });
    });
  });
})();
</script>"#;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn form(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    /// The answers are checked where they arrive: every value a key would
    /// refuse is refused before anything is written.
    #[test]
    fn test_the_apply_form_is_checked() {
        let ok = plan_from_form(&form(&[("runs", "emulator"), ("cpu", "8080"), ("disks", "2=GAMES.DSK;1=TOOLS.DSK")]))
            .unwrap();
        assert_eq!(ok.runs, Runs::Emulator);
        assert_eq!(ok.disks, vec![(1, "TOOLS.DSK".into()), (2, "GAMES.DSK".into())]);
        let boot = plan_from_form(&form(&[
            ("runs", "boot"),
            ("boot", "DISK03.DSK"),
            ("machine", "auto"),
            ("cpu", "8080"),
            ("rom", "off"),
        ]))
        .unwrap();
        assert_eq!(boot.boot_disk.as_deref(), Some("DISK03.DSK"));
        for bad in [
            vec![("runs", "sideways"), ("cpu", "z80")],
            vec![("runs", "emulator"), ("cpu", "6502")],
            vec![("runs", "boot"), ("boot", "../../etc/passwd"), ("cpu", "z80"), ("machine", "auto"), ("rom", "off")],
            vec![("runs", "boot"), ("boot", "A.DSK"), ("cpu", "z80"), ("machine", "pdp11"), ("rom", "off")],
            vec![("runs", "boot"), ("boot", "A.DSK"), ("cpu", "z80"), ("machine", "auto"), ("rom", "rom-of-doom")],
            vec![("runs", "emulator"), ("cpu", "z80"), ("disks", "0=A.DSK")],
            vec![("runs", "emulator"), ("cpu", "z80"), ("disks", "16=A.DSK")],
            vec![("runs", "emulator"), ("cpu", "z80"), ("disks", "1=../x.dsk")],
            vec![("runs", "emulator"), ("cpu", "z80"), ("disks", "1=A.DSK;2=A.DSK")],
        ] {
            assert!(plan_from_form(&form(&bad)).is_err(), "{bad:?} was accepted");
        }
    }

    /// `esc` escapes both quotes: image names go into attribute values, and
    /// a file named `x" onmouseover="...".dsk` was script in the config page.
    #[test]
    fn test_the_script_escapes_quotes() {
        let f = &SCRIPT[SCRIPT.find("function esc(s)").expect("esc")..];
        let f = &f[..f.find("\n  }\n").expect("its end")];
        for (raw, ent) in [("/\"/g", "&quot;"), ("/'/g", "&#39;"), ("/&/g", "&amp;"), ("/</g", "&lt;")] {
            assert!(f.contains(&format!(".replace({raw}, '{ent}')")), "esc does not turn {raw} into {ent}");
        }
        // `&` first, or the entities the others make are escaped again.
        assert!(f.find("/&/g").unwrap() < f.find("/\"/g").unwrap());
    }

    /// The page walks the steps by the gateway's table, never by its own
    /// rules: no step returns another step's name.
    #[test]
    fn test_the_page_has_no_step_order_of_its_own() {
        for step in wizard::Step::ALL {
            let lit = format!("return '{}'", step.key());
            assert!(!SCRIPT.contains(&lit), "the script decides a step itself: {lit}");
        }
        assert!(SCRIPT.contains("return nextOf(from);"), "positive control: the script asks the table");
        let modal = render_modal();
        assert!(modal.contains("&quot;runs|emulator|0&quot;:&quot;drives&quot;"), "the table is sent");
        assert!(SCRIPT.contains("'/cpm-wizard/check'"), "the page asks the gateway to check");
    }

    /// Every answer is parsed once, in `request`.  A second `.json()` on what
    /// `post` already parsed threw in `save`, so every web Save wrote the
    /// settings and then said the gateway had not answered -- and left the
    /// page holding the old CP/M fields for its next ordinary Save.
    #[test]
    fn test_each_answer_is_parsed_once() {
        let req = &SCRIPT[SCRIPT.find("function request(").expect("request")..];
        let req = &req[..req.find("\n  }\n").expect("its end")];
        assert!(req.contains(".json()"), "positive control: request parses the answer");
        assert_eq!(SCRIPT.matches(".json()").count(), 1, "an answer parsed outside request()");
        let save = &SCRIPT[SCRIPT.find("function save()").expect("save")..];
        let save = &save[..save.find("\n  }\n").expect("its end")];
        assert!(save.contains("request('/cpm-wizard/apply'") && !save.contains(".json()"), "{save}");
    }

    #[test]
    fn test_the_check_answers_as_json() {
        assert_eq!(check_json(Ok(vec!["Boot \"A\".DSK".into()])), "{\"ok\":true,\"lines\":[\"Boot \\\"A\\\".DSK\"]}");
        assert_eq!(check_json(Err("no".into())), "{\"ok\":false,\"error\":\"no\"}");
    }

    fn facts(name: &str, boots: bool, files: bool, needs_rom: bool) -> DiskFacts {
        DiskFacts {
            name: name.into(),
            bytes: 337_568,
            boots,
            files: files.then(|| vec!["PIP.COM".to_string()]),
            summary: String::new(),
            machine: "altair_2sio".into(),
            machine_why: None,
            board: Some("MITS 88-DCDD floppy"),
            slots: Some(16),
            altair_basic: false,
            needs_rom,
        }
    }

    /// Each answer is applied by `Plan`'s own rules, and what no picker
    /// offers is refused.
    #[test]
    fn test_an_answer_is_applied_by_the_wizards_rules() {
        let all = vec![facts("BOOT.DSK", true, true, false), facts("ROM.DSK", true, false, true), facts("DATA.DSK", false, true, false)];
        let ask = |plan: &[(&str, &str)], what: &str, value: &str| {
            let mut f = form(plan);
            f.insert("what".into(), what.into());
            f.insert("value".into(), value.into());
            choose(&f, &all)
        };
        // Boot looked at and backed out of keeps the emulator's drives.
        let p = ask(&[("runs", "emulator"), ("cpu", "z80"), ("disks", "2=DATA.DSK")], "runs", "boot").unwrap();
        assert_eq!((p.runs, p.boot_disk.as_deref(), p.disks.len()), (Runs::Boot, None, 1));
        let p = ask(&[("runs", "boot"), ("cpu", "z80"), ("machine", "auto"), ("rom", "off"), ("disks", "2=DATA.DSK")], "runs", "emulator")
            .unwrap();
        assert_eq!((p.runs, p.disks.len()), (Runs::Emulator, 1), "a look at Boot lost the drives");
        // A boot disk starts from its recommendations; the ROM it wants.
        let p = ask(&[("runs", "boot"), ("cpu", "z80"), ("machine", "auto"), ("rom", "off")], "boot", "ROM.DSK").unwrap();
        assert_eq!((p.boot_disk.as_deref(), p.rom.as_str()), (Some("ROM.DSK"), "cuter_vdm"));
        let boot = [("runs", "boot"), ("boot", "BOOT.DSK"), ("cpu", "z80"), ("machine", "auto"), ("rom", "off"), ("disks", "1=DATA.DSK")];
        // The machine the disk was read as is kept as auto, with its disks.
        let p = ask(&boot, "machine", "altair_2sio").unwrap();
        assert_eq!((p.machine.as_str(), p.disks.len()), ("auto", 1));
        let p = ask(&boot, "machine", "altair_sio").unwrap();
        assert_eq!((p.machine.as_str(), p.disks.len()), ("altair_sio", 0), "another machine kept the old board's disks");
        assert_eq!(ask(&boot, "cpu", "8080").unwrap().cpu, "8080");
        assert_eq!(ask(&boot, "rom", "yes").unwrap().rom, "off", "a ROM for a disk that wants none");
        for (why, what, value) in [
            ("a disk never seen to boot", "boot", "DATA.DSK"),
            ("a disk not here", "boot", "GONE.DSK"),
            ("a machine with no board for the disk", "machine", "cromemco"),
            ("a machine that does not exist", "machine", "pdp11"),
            ("a processor", "cpu", "6502"),
            ("a ROM answer that is neither", "rom", "YES"),
            ("a third way to run", "runs", "sideways"),
            ("no question", "colour", "blue"),
        ] {
            assert!(ask(&boot, what, value).is_err(), "{why} was accepted");
        }
        // The disk questions belong to a boot.
        let emu = [("runs", "emulator"), ("cpu", "z80")];
        for (what, value) in [("boot", "BOOT.DSK"), ("machine", "altair_2sio"), ("rom", "no")] {
            assert!(ask(&emu, what, value).is_err(), "{what} was answered for the emulator");
        }
        // Boot is an answer even when nothing here boots -- every surface
        // offers it, for images of the operator's own -- though no disk is.
        let none = vec![facts("DATA.DSK", false, true, false)];
        let mut f = form(&[("runs", "emulator"), ("cpu", "z80"), ("what", "runs"), ("value", "boot")]);
        assert_eq!(choose(&f, &none).map(|p| p.runs), Ok(Runs::Boot), "Boot refused with nothing that boots");
        f.insert("value".into(), "emulator".into());
        assert!(choose(&f, &none).is_ok());
        let f = form(&[("runs", "boot"), ("cpu", "z80"), ("machine", "auto"), ("rom", "off"), ("what", "boot"), ("value", "DATA.DSK")]);
        assert!(choose(&f, &none).is_err(), "a disk that does not boot was taken");
    }

    /// The page is sent the two things it draws that are rules: the machine
    /// in force, and whether the ROM step comes next.
    #[test]
    fn test_the_plan_sent_carries_what_the_page_would_otherwise_work_out() {
        let all = vec![facts("ROM.DSK", true, false, true)];
        let p = Plan::boot(&all[0]);
        let j = plan_json(&p, &all);
        assert!(j.contains("\"machine\":\"auto\"") && j.contains("\"inForce\":\"altair_2sio\""), "{j}");
        assert!(j.contains("\"needsRom\":true") && j.contains("\"boot\":\"ROM.DSK\""), "{j}");
        let e = plan_json(&Plan::emulator(), &all);
        assert!(e.contains("\"boot\":null") && e.contains("\"inForce\":null") && e.contains("\"needsRom\":false"), "{e}");
        assert!(serde_json::from_str::<serde_json::Value>(&j).is_ok(), "not JSON: {j}");
        // Reading the images failed: the page still gets a plan to start from.
        let f: serde_json::Value = serde_json::from_str(&disks_fallback()).expect("the fallback is JSON");
        assert_eq!(f["plan"]["runs"], "emulator", "{f}");
        assert_eq!(f["disks"].as_array().map(Vec::len), Some(0));
        assert_eq!(f["error"], DISKS_UNREAD, "the page is not told the images were not read");
    }

    /// The page holds no rule about what an answer keeps: the only parts of
    /// the plan it writes are the drives and slots it shows (values), and the
    /// whole plan only as the gateway sends it.
    #[test]
    fn test_the_page_applies_no_answer_itself() {
        let mut writes = 0;
        for (i, _) in SCRIPT.match_indices("plan.") {
            let rest = &SCRIPT[i + "plan.".len()..];
            let field: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
            let after = rest[field.len()..].trim_start();
            if after.starts_with('=') && !after.starts_with("==") {
                writes += 1;
                assert!(rest.starts_with("disks = readRows("), "the page sets plan.{field} itself");
            }
        }
        assert_eq!(writes, 2, "positive control: the drives and the slots");
        for (i, _) in SCRIPT.match_indices("plan = ") {
            let at = &SCRIPT[i..];
            assert!(at.starts_with("plan = j.plan;") || at.starts_with("plan = null,"), "the page builds a plan of its own");
        }
        assert!(SCRIPT.contains("'/cpm-wizard/choose'"), "the answers go to the gateway");
        assert!(!SCRIPT.contains("'auto'"), "the page resolves the detected machine itself");
    }

    /// One request at a time, and only for the wizard that asked.  Every
    /// request goes through `request`; while one is out Next and Back do
    /// nothing; and a reply -- or a failure -- to an earlier opening of the
    /// wizard is never delivered, except a save's, which reloads the page.
    /// Without this an answer backed out of landed in the plan, two quick
    /// Nexts left it booting the first disk picked, and a slow disk list from
    /// an earlier opening threw the operator back to the first step.
    #[test]
    fn test_a_stale_answer_is_dropped() {
        let body = |name: &str| {
            let f = &SCRIPT[SCRIPT.find(name).unwrap_or_else(|| panic!("{name}"))..];
            f[..f.find("\n  }\n").expect("its end")].to_string()
        };
        let line = |start: &str| {
            let f = &SCRIPT[SCRIPT.find(start).unwrap_or_else(|| panic!("{start}"))..];
            f[..f.find('\n').unwrap()].to_string()
        };
        assert_eq!(SCRIPT.matches("fetch(").count(), 1, "a request that does not go through request()");
        let req = body("function request(");
        assert!(req.contains("fetch("), "positive control: request is where the fetch is");
        assert_eq!(req.matches("if (mine !== opened && !always)").count(), 2, "an answer or a failure from an earlier opening gets through");
        assert_eq!(req.matches("return new Promise(function() {})").count(), 2, "a stale reply settles");
        assert!(req.contains("setBusy(true)"), "a request does not hold Next and Back");
        let next = &SCRIPT[SCRIPT.find("next.addEventListener(").expect("Next")..];
        let next = &next[..next.find("\n  });").expect("its end")];
        assert!(next.contains("if (!view || busy) return;"), "Next runs while a request is out");
        assert!(line("back.addEventListener(").contains("!busy"), "Back runs while a request is out");
        let open = &SCRIPT[SCRIPT.find("button.more[data-target=\"cpm-wizard\"]").expect("the opener")..];
        let open = &open[..open.find("\n    });").expect("its end")];
        assert!(open.contains("opened++"), "reopening does not start a generation");
        assert!(open.contains("request('/cpm-wizard/disks'"), "the disk list is not asked through request()");
        assert!(req.matches("&& !always").count() == 2, "a save's outcome is dropped after a reopen");
        let busy = body("function setBusy(");
        assert!(busy.contains("box.querySelectorAll('button, select')"), "a pick can change while its answer is out");
        // A save's outcome is always shown -- the wizard opened again on it
        // if it was closed -- and the page reloads only on the close after a
        // save, never under an operator typing in the page's form.
        let save = body("function save(");
        assert!(save.contains("request('/cpm-wizard/apply', form(), true)"), "a save's outcome can be dropped");
        assert_eq!(save.matches("openModal('cpm-wizard')").count(), 2, "an outcome is said into a closed wizard");
        assert!(!save.contains("location.href"), "a save reloads the page under the operator");
        assert!(save.contains("saved = true"), "positive control: the close after a save reloads");
        // Not saved, into a wizard opened again since: that opening is
        // dropped, or its disk list draws the first step over the outcome.
        assert!(save.contains("var at = opened;"), "a save does not know the opening it was sent from");
        let failed = &save[save.find("function failed(").expect("failed")..];
        let failed = &failed[..failed.find("\n    }\n").expect("its end")];
        let reopened = &failed[failed.find("if (at !== opened) {").expect("a reopen is not looked for")..];
        assert!(reopened[..reopened.find('}').unwrap()].contains("opened++"), "a reopened wizard's disk list wipes a failed save");
        assert!(save.contains("if (!j.ok) { failed("), "a refused save is not said through failed()");
        assert!(save.contains(".catch(function() { failed("), "an unanswered save is not said through failed()");
    }

    /// Unread images are said, and the steps are not drawn: an empty list
    /// would advise downloading disks that are there, and its emulator plan,
    /// saved, would take every mounted disk off.
    #[test]
    fn test_unread_images_stop_the_wizard() {
        let open = &SCRIPT[SCRIPT.find("button.more[data-target=\"cpm-wizard\"]").expect("the opener")..];
        let open = &open[..open.find("\n    });").expect("its end")];
        let err = open.find("if (j.error) {").expect("the error is not looked at");
        let rest = &open[err..];
        assert!(rest[..rest.find('}').unwrap()].contains("return;"), "the steps are drawn after the error");
        assert!(err < open.find("go(").unwrap());
    }

    /// The page is told what the download step offers and which first
    /// answer to hold, and follows both: it opens on the download while one
    /// is offered, hides Next there, and offers Boot whatever is here.
    #[test]
    fn test_the_page_is_told_the_offer_and_the_first_answer() {
        let j: serde_json::Value = serde_json::from_str(&disks_json("/nonexistent-transfer-dir")).expect("JSON");
        assert_eq!(j["offer"]["disks"], crate::cpm::fetch::catalogue().len(), "{j}");
        assert_eq!(j["advised"], "emulator", "nothing here boots");
        assert_eq!(j["plan"]["runs"], "emulator");
        // Nothing boots in a folder that is not there, so the opening plan
        // and the emulator's look alike above: the plan sent is checked at
        // its source, bounded to the function.
        let src = include_str!("cpm_wizard.rs").replace('\r', "");
        let dj = &src[src.find("pub(super) fn disks_json(").unwrap()..];
        let dj = &dj[..dj.find("\n}\n").unwrap()];
        assert!(dj.contains("plan_json(&Plan::opening(&disks), &disks)"), "the page is sent another first answer");
        assert!(dj.contains("runs_key(wizard::opening_runs(&disks))"), "the advised answer is not the gateway's");
        let f: serde_json::Value = serde_json::from_str(&disks_fallback()).unwrap();
        assert!(f["offer"].is_null());
        let open = &SCRIPT[SCRIPT.find("button.more[data-target=\"cpm-wizard\"]").expect("the opener")..];
        assert!(open.contains("go(D.offer ? 'fetch' : 'runs')"), "the page does not open on the download");
        let go = &SCRIPT[SCRIPT.find("function go(").unwrap()..];
        assert!(go[..go.find("\n  }\n").unwrap()].contains("s === 'fetch' ? 'hidden'"), "Next shows on the download step");
        let runs = &SCRIPT[SCRIPT.find("    runs: function").unwrap()..];
        let runs = &runs[..runs.find("\n    },\n").unwrap()];
        assert!(runs.contains("['boot', T.dataset.boot") && !runs.contains("boots"), "Boot is offered only with a disk that boots");
        assert!(runs.contains("D.advised"), "the advised answer is the page's own");
        assert!(SCRIPT.contains("request('/cpm-wizard/fetch', '')"), "the download is not asked through request()");
        assert!(SCRIPT.contains("#cpmw-skip") || SCRIPT.contains("'cpmw-skip'"), "positive control: the download step");
        assert!(!SCRIPT.contains("trail.push('fetch')"), "Back returns to the download (`Step::on_trail`)");
        // A save's outcome can land on a wizard opened again since, on the
        // download step, where Next is hidden: its Close must be shown.
        let save = &SCRIPT[SCRIPT.find("function save()").unwrap()..];
        let save = &save[..save.find("\n  }\n").unwrap()];
        assert_eq!(save.matches("next.textContent = 'Close'").count(), 2, "positive control: both outcomes offer Close");
        assert_eq!(save.matches("next.style.visibility = 'visible'").count(), 2, "a save's Close is left hidden");
    }

    /// The processors the page offers are the settings' own list, sent with
    /// the disks; the script names none of its own.
    #[test]
    fn test_the_page_offers_the_settings_processors() {
        let body = disks_json("/nonexistent-transfer-dir");
        let j: serde_json::Value = serde_json::from_str(&body).expect("JSON");
        let keys: Vec<&str> = j["cpus"].as_array().expect("cpus").iter().map(|c| c["key"].as_str().unwrap()).collect();
        let want: Vec<&str> = crate::cpm::cpu::CPU_CHOICES.iter().map(|(v, _)| *v).collect();
        assert_eq!(keys, want);
        assert_eq!(j["cpus"][1]["name"], "8080");
        for lit in ["'z80'", "'8080'", "'Z80'"] {
            assert!(!SCRIPT.contains(lit), "the script names a processor itself: {lit}");
        }
        let f: serde_json::Value = serde_json::from_str(&disks_fallback()).unwrap();
        assert!(f["cpus"].is_array());
    }

    /// Nothing the wizard draws is ever submitted with the page's one form:
    /// its choices are buttons, which a form never sends, and its selects have
    /// no name.  A named control would ride along with an ordinary Save.
    #[test]
    fn test_the_modal_sends_nothing_with_an_ordinary_save() {
        let all = format!("{}{}", render_modal(), SCRIPT);
        assert!(!all.contains("<input"), "an input in the wizard would be a form control");
        assert!(!all.contains("name=\""), "a named control would be sent with Save");
        assert!(!all.contains("<form"), "the page has one form, and forms do not nest");
        assert!(all.matches("<button type=\"button\"").count() >= 3, "the choices are buttons");
        assert!(!SCRIPT.contains("alert("), "native dialogs are gone from this page");
        assert!(all.contains("id=\"cpm-wizard\""));
    }
}
