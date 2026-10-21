//! The CP/M setup wizard on the web page.
//!
//! Every question, recommendation and setting is [`crate::cpm::wizard`]'s;
//! this is the modal that draws it.  The page asks `/cpm-wizard/disks` once
//! when the wizard opens -- what each disk is, its listing, and for every
//! machine that could boot it, its board, slot count and slot names -- and
//! walks the steps in the browser, so a step back costs nothing.  The answers
//! go to `/cpm-wizard/apply` as one form, which is where they are checked:
//! nothing the browser sends is trusted to be a key the settings accept.

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
    out.push_str("],\"disks\":[");
    for (i, d) in disks.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&disk_json(d, &disks));
    }
    out.push_str("]}");
    out
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
        let names: Vec<String> = (0..=room)
            .map(|s| format!("\"{}\"", json_escape(&wizard::slot_name(d, m.key, s))))
            .collect();
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

/// The answers, checked, as a plan.  Every field is validated here: the form
/// came from a browser, and a value a key would refuse is refused before
/// anything is written.
pub(super) fn plan_from_form(form: &std::collections::HashMap<String, String>) -> Result<Plan, String> {
    let get = |k: &str| form.get(k).map(|s| s.trim().to_string()).unwrap_or_default();
    let mut plan = match get("runs").as_str() {
        "emulator" => Plan::emulator(),
        "boot" => {
            let disk = get("boot");
            if !crate::cpm::image::is_safe_image_name(&disk) {
                return Err("choose a boot disk".to_string());
            }
            let mut p = Plan::emulator();
            p.runs = Runs::Boot;
            p.boot_disk = Some(disk);
            p
        }
        _ => return Err("choose what CP/M runs".to_string()),
    };
    let cpu = get("cpu");
    if !matches!(cpu.as_str(), "z80" | "8080") {
        return Err("choose a processor".to_string());
    }
    plan.cpu = cpu;
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
          data-machine=\"{machine}\" data-cpu=\"{cpu}\" data-extra=\"{extra}\" \
          data-finish=\"{finish}\" data-emulator=\"{emu}\" data-boot=\"{boot}\"></template>",
        runs = html_escape(&para(wizard::RUNS_HELP)),
        drives = html_escape(&para(wizard::EMULATOR_DRIVES_HELP)),
        bootdisk = html_escape(&para(wizard::BOOT_DISK_HELP)),
        machine = html_escape(&para(wizard::MACHINE_HELP)),
        cpu = html_escape(&para(wizard::CPU_HELP)),
        extra = html_escape(&para(wizard::EXTRA_DISKS_HELP)),
        finish = html_escape(wizard::FINISH_LINE),
        emu = html_escape(wizard::EMULATOR_LABEL),
        boot = html_escape(wizard::BOOT_LABEL),
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
  var D = null, plan = null, trail = [], step = null, view = null, saved = false;
  // **After a save, the page reloads however the wizard is closed** -- Close,
  // the x, a click outside.  The page's one form still holds the CP/M fields as
  // they were before the wizard, and its next Save would write every one of
  // them back over the wizard's answers.
  new MutationObserver(function() {
    if (saved && !document.getElementById('cpm-wizard').classList.contains('open')) {
      saved = false; location.href = '/';
    }
  }).observe(document.getElementById('cpm-wizard'), { attributes: true, attributeFilter: ['class'] });
  function esc(s) { var d = document.createElement('div'); d.textContent = s; return d.innerHTML; }
  function say(s) { msg.textContent = s || ''; }
  function disk(n) { return D.disks.filter(function(x) { return x.name === n; })[0]; }
  function machineInForce() { var d = disk(plan.boot); return plan.machine === 'auto' ? d.machine : plan.machine; }
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
  function readRows(attr, orderMatters) {
    var seen = {}, out = {}, clash = null, gap = false, empty = false;
    box.querySelectorAll('select[data-' + attr + ']').forEach(function(s) {
      if (!s.value) { empty = true; return; }
      if (empty) gap = true;
      if (seen[s.value]) clash = s.value;
      seen[s.value] = true; out[s.dataset[attr]] = s.value;
    });
    if (clash) { say(clash + ' is chosen twice; one image can be in one place.'); return null; }
    if (orderMatters && gap) { say('Fill the slots in order, with no empty slot between two disks.'); return null; }
    return out;
  }
  var steps = {
    runs: function() {
      var items = [['emulator', T.dataset.emulator, '']];
      if (D.disks.some(function(d) { return d.boots; })) items.push(['boot', T.dataset.boot, '']);
      box.innerHTML = T.dataset.runs + (items.length > 1 ? '' :
        '<p class="sub">No disk image here can boot yet: Download sample disks on this panel first, then open the wizard again to boot one.</p>') +
        choices('runs', items, plan.runs);
      wire('runs', function() {});
      return function() {
        var v = picked('runs');
        if (v === 'emulator') { if (plan.runs !== 'emulator') plan = fresh('emulator'); return 'drives'; }
        if (v === 'boot') { plan.runs = 'boot'; return 'bootdisk'; }
        say('Choose one of the two.'); return null;
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
        var out = readRows('drive', false);
        if (!out) return null;
        plan.disks = out; return 'cpu';
      };
    },
    bootdisk: function() {
      var list = D.disks.filter(function(d) { return d.boots; });
      box.innerHTML = T.dataset.bootdisk +
        choices('boot', list.map(function(d) { return [d.name, d.name, d.summary.split(' -- ')[1] || '']; }), plan.boot) +
        '<div id="cpmw-view"></div>';
      wire('boot', function(n) { show(disk(n)); });
      if (plan.boot) show(disk(plan.boot));
      return function() {
        var n = picked('boot');
        if (!n) { say('Choose the disk to boot.'); return null; }
        if (n !== plan.boot) {
          var d = disk(n);
          plan.boot = n; plan.machine = 'auto'; plan.cpu = d.cpu; plan.rom = d.rom || 'off'; plan.disks = {};
        }
        return 'machine';
      };
    },
    machine: function() {
      var d = disk(plan.boot);
      var items = Object.keys(d.on).map(function(k) {
        return [k, machineLabel(k), (k === d.machine ? 'what the disk was read as; ' : '') + d.on[k].board];
      });
      box.innerHTML = T.dataset.machine + '<p>The disk reads as <b>' + esc(machineLabel(d.machine)) + '</b>.</p>' +
        choices('machine', items, plan.machine === 'auto' ? d.machine : plan.machine);
      wire('machine', function() {});
      return function() {
        var v = picked('machine');
        if (!v) { say('Choose the machine.'); return null; }
        var m = v === d.machine ? 'auto' : v;
        if (m !== plan.machine) plan.disks = {};
        plan.machine = m; return 'cpu';
      };
    },
    cpu: function() {
      var adv = plan.runs === 'boot' ? disk(plan.boot).cpu : 'z80';
      var why = plan.runs === 'boot' ? disk(plan.boot).cpuWhy : 'the Z80 runs nearly all 8080 software';
      box.innerHTML = T.dataset.cpu + '<p><b>Advised: ' + (adv === '8080' ? '8080' : 'Z80') + '</b>, ' + esc(why) + '.</p>' +
        choices('cpu', [['z80', 'Z80', adv === 'z80' ? 'advised' : ''], ['8080', '8080', adv === '8080' ? 'advised' : '']], plan.cpu);
      wire('cpu', function() {});
      return function() {
        var v = picked('cpu');
        if (!v) { say('Choose the processor.'); return null; }
        plan.cpu = v;
        if (plan.runs === 'emulator') return 'ready';
        return disk(plan.boot).rom ? 'rom' : 'extra';
      };
    },
    rom: function() {
      box.innerHTML = '<p>This disk looks for the CUTER monitor at C000 and stops without it. ' +
        'The ROM is fetched with the sample disks, from its author\'s collection.</p>' +
        choices('rom', [['yes', 'Load the ROM', 'advised'], ['off', 'No ROM', '']], plan.rom === 'off' ? 'off' : 'yes');
      wire('rom', function() {});
      return function() {
        plan.rom = picked('rom') === 'yes' ? (disk(plan.boot).rom || 'off') : 'off';
        return 'extra';
      };
    },
    extra: function() {
      var d = disk(plan.boot), m = machineInForce(), on = d.on[m];
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
        var out = readRows('slot', true);
        if (!out) return null;
        plan.disks = out; return 'ready';
      };
    },
    ready: function() {
      var lines = [];
      if (plan.runs === 'boot') {
        lines.push('Boot ' + plan.boot + ' on ' + machineLabel(machineInForce()));
      } else lines.push(T.dataset.emulator);
      lines.push('Processor: ' + (plan.cpu === '8080' ? '8080' : 'Z80'));
      if (plan.runs === 'boot' && plan.rom !== 'off') lines.push('Monitor ROM: ' + plan.rom);
      var ks = Object.keys(plan.disks).sort(function(a, b) { return a - b; });
      if (!ks.length) lines.push('No other disks');
      ks.forEach(function(k) {
        var at = plan.runs === 'boot' ? disk(plan.boot).on[machineInForce()].slots[k] : String.fromCharCode(65 + Number(k)) + ':';
        lines.push(at + ' ' + plan.disks[k]);
      });
      box.innerHTML = '<p><b>Ready.</b> These are the settings:</p><ul>' +
        lines.map(function(l) { return '<li>' + esc(l) + '</li>'; }).join('') +
        '</ul><p class="sub">Disks mounted now and not listed here are taken off. Nothing has been saved yet.</p>';
      next.textContent = 'Save';
      return function() { save(); return null; };
    },
    done: function() { return function() { closeModal('cpm-wizard'); location.href = '/'; return null; }; }
  };
  function fresh(runs) { return { runs: runs, boot: null, machine: 'auto', cpu: 'z80', rom: 'off', disks: {} }; }
  function go(s) {
    step = s; say(''); next.textContent = 'Next';
    view = steps[s]();
    back.style.visibility = trail.length ? 'visible' : 'hidden';
  }
  function save() {
    var body = 'runs=' + encodeURIComponent(plan.runs) + '&cpu=' + encodeURIComponent(plan.cpu);
    if (plan.runs === 'boot') {
      body += '&boot=' + encodeURIComponent(plan.boot) + '&machine=' + encodeURIComponent(plan.machine) +
        '&rom=' + encodeURIComponent(plan.rom);
    }
    body += '&disks=' + encodeURIComponent(Object.keys(plan.disks).map(function(k) { return k + '=' + plan.disks[k]; }).join(';'));
    say('Saving…');
    fetch('/cpm-wizard/apply', { method: 'POST', headers: { 'Content-Type': 'application/x-www-form-urlencoded' }, body: body })
      .then(function(r) { return r.json(); })
      .then(function(j) {
        if (!j.ok) { say('Not saved: ' + j.error); return; }
        say(''); trail = []; saved = true;
        box.innerHTML = '<p><b>Saved.</b></p>' +
          (j.notes || []).map(function(n) { return '<p class="sub">' + esc(n) + '</p>'; }).join('') +
          '<p><b>' + esc(T.dataset.finish) + '</b></p>';
        step = 'done'; view = steps.done(); next.textContent = 'Close'; back.style.visibility = 'hidden';
      })
      .catch(function() { say('The gateway did not answer.'); });
  }
  next.addEventListener('click', function() {
    if (!view) return;
    var to = view();
    if (to) { trail.push(step); go(to); }
  });
  back.addEventListener('click', function() { if (trail.length) go(trail.pop()); });
  document.querySelectorAll('button.more[data-target="cpm-wizard"]').forEach(function(b) {
    b.addEventListener('click', function() {
      box.innerHTML = '<p class="sub">Reading the disk images…</p>';
      view = null; trail = []; say('');
      fetch('/cpm-wizard/disks').then(function(r) { return r.json(); }).then(function(j) {
        D = j; plan = fresh('emulator'); go('runs');
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
