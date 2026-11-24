//! Add-Ons: programs the operator drops into `ethernetgateway-data/AddOns/<name>/`,
//! offered from the Second Menu like BBS doors.
//!
//! Each add-on is one folder holding its program, an `addon.conf` manifest and
//! whatever else it needs.  This module is the part every surface shares: the
//! manifest, discovery, and the check that a command stays inside its own
//! folder.  The telnet list page is `telnet/addons_ui.rs`.
//!
//! **Found again on every look, never cached.**  The folder is read each time
//! the list (or the menu that offers it) is drawn, so an add-on copied in is
//! there at the next look with no restart -- and one deleted is gone.  A few
//! `read_dir`s per menu draw is nothing beside a terminal's line rate.
//!
//! **A bad add-on is skipped and said once, never fatal.**  A folder with no
//! manifest, one that does not parse, or a command that is missing or leads
//! out of its folder is left out of the list and logged the first time it is
//! seen -- not on every menu draw, which would bury the log.

pub(crate) mod fetch;
pub(crate) mod update;
pub(crate) mod gui;

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

/// The folder under the data directory that holds one folder per add-on.
pub(crate) const ADDONS_DIR: &str = "AddOns";
/// Each add-on's manifest, in its own folder.
pub(crate) const MANIFEST: &str = "addon.conf";

/// Where the add-ons live: `ethernetgateway-data/AddOns`, relative to the
/// launch directory like everything else the gateway keeps.
pub(crate) fn addons_root() -> PathBuf {
    Path::new(crate::config::DATA_DIR).join(ADDONS_DIR)
}

/// What an add-on offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// A program bridged to the caller's session.
    Terminal,
    /// A program that opens a window on the gateway machine's screen.
    Gui,
    /// Both: the terminal side, and a window beside it.
    Both,
}

impl Mode {
    pub(crate) fn has_terminal(self) -> bool {
        matches!(self, Mode::Terminal | Mode::Both)
    }

    pub(crate) fn has_gui(self) -> bool {
        matches!(self, Mode::Gui | Mode::Both)
    }
}

/// What a Commodore caller's session does with the add-on's output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Petscii {
    /// The add-on writes ANSI and the gateway turns it into PETSCII, as the
    /// SSH gateway does.
    Translate,
    /// The add-on speaks PETSCII itself: a raw pipe, byte for byte.
    ///
    /// **Chosen in the manifest, never guessed.**  The gateway does not look
    /// at an add-on's bytes to decide (see gateway-and-terminals: no
    /// structural detectors on a pipe).
    Native,
}

/// How the caller's typing reaches the add-on.
///
/// **A pipe is not a terminal**, so nothing on the add-on's side echoes what
/// is typed or handles Backspace -- a shell `read` over a pipe would take a
/// name the caller never saw.  `Line` gives the add-on the cooked line a
/// terminal would: the gateway echoes and edits, and sends the line on
/// Enter.  `Raw` sends every key as it is pressed, for a program that reacts
/// to single keys and draws its own echo.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Input {
    Line,
    Raw,
}

/// Which terminal types an add-on supports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Terminals {
    pub ansi: bool,
    pub ascii: bool,
    pub petscii: bool,
}

impl Terminals {
    pub(crate) const ALL: Terminals = Terminals { ansi: true, ascii: true, petscii: true };
}

/// One add-on's `addon.conf`, read and checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Manifest {
    /// The folder's name: what identifies the add-on.
    pub id: String,
    pub name: String,
    pub description: String,
    pub version: String,
    pub author: String,
    pub mode: Mode,
    /// The terminal side's program, relative to the add-on's folder, with
    /// this platform's override already applied.
    pub command: Option<String>,
    pub args: Vec<String>,
    /// The GUI side's program, the same way.
    pub gui_command: Option<String>,
    pub gui_args: Vec<String>,
    pub terminals: Terminals,
    /// Refused on a narrower screen (an 80-column program on a C64).
    pub min_columns: u16,
    pub petscii: Petscii,
    pub input: Input,
    /// Ended after this many minutes with no traffic either way; 0 = the
    /// session's own idle timeout (an add-on cannot switch that off).
    pub idle_minutes: u32,
    /// Ended after this many minutes in all; 0 = never.
    pub max_minutes: u32,
    /// The telnet side waits for the add-on's window to close.
    pub gui_wait: bool,
    /// Only one copy of the terminal side at a time.
    pub single_instance: bool,
    /// The oldest gateway it runs on, as given (`1.1` is `[1, 1]`).
    pub min_gateway: Option<Vec<u32>>,
    /// The newest gateway it runs on, as given: `1.4` covers every 1.4.x.
    pub max_gateway: Option<Vec<u32>>,
}

/// The suffix of the per-platform keys this build reads: `command_linux` and
/// so on.  "linux" covers every Unix that is not macOS -- the BSDs run the
/// same kind of program a Linux build does.
fn platform_key() -> &'static str {
    if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    }
}

/// Split an `args` value into arguments, the way a shell would but without
/// running one: words separated by spaces, `'...'` taken literally, `"..."`
/// literally except for `\"` and `\\`.
///
/// **A backslash outside quotes is an ordinary character**, so a Windows path
/// (`data\scores.txt`) arrives as written.  No variable is expanded, no `*`
/// is globbed and nothing is executed: the words go straight to the program.
pub(crate) fn split_args(s: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(ch) => word.push(ch),
                        None => return Err("a ' quote is not closed".into()),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') if matches!(chars.peek(), Some('"') | Some('\\')) => {
                            word.push(chars.next().unwrap_or('\\'));
                        }
                        Some(ch) => word.push(ch),
                        None => return Err("a \" quote is not closed".into()),
                    }
                }
            }
            c if c.is_whitespace() => {
                if in_word {
                    out.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    if in_word {
        out.push(word);
    }
    Ok(out)
}

fn parse_bool(key: &str, v: &str) -> Result<bool, String> {
    match v.to_ascii_lowercase().as_str() {
        "yes" | "true" | "on" | "1" => Ok(true),
        "no" | "false" | "off" | "0" => Ok(false),
        _ => Err(format!("{key} must be yes or no, not \"{v}\"")),
    }
}

/// A gateway version in a manifest: one to three dot-separated whole
/// numbers, kept as written so `max_gateway = 1.4` can mean "any 1.4.x".
pub(crate) fn parse_version(key: &str, v: &str) -> Result<Vec<u32>, String> {
    let bad = || format!("{key} must be a version like 1.1.0, not \"{v}\"");
    let parts = v
        .split('.')
        .map(|p| {
            if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
                return Err(bad());
            }
            p.parse::<u32>().map_err(|_| bad())
        })
        .collect::<Result<Vec<u32>, String>>()?;
    if parts.len() > 3 {
        return Err(bad());
    }
    Ok(parts)
}

pub(crate) fn show_version(v: &[u32]) -> String {
    v.iter().map(u32::to_string).collect::<Vec<_>>().join(".")
}

/// This gateway's version, `[major, minor, patch]`.
pub(crate) fn this_gateway() -> Vec<u32> {
    crate_version(env!("CARGO_PKG_VERSION"))
}

/// A crate version as numbers; a pre-release (`1.0.0-rc2`) counts as its
/// release, so an add-on for 1.0.0 runs on the candidate being tested.
fn crate_version(v: &str) -> Vec<u32> {
    let core = v.split(['-', '+']).next().unwrap_or(v);
    parse_version("version", core).unwrap_or_default()
}

/// Whether gateway `ours` is inside the add-on's range.  Each bound is
/// compared on the parts it gives: `min_gateway = 1.4` admits 1.4.0 and
/// later, `max_gateway = 1.4` admits up to the last 1.4.x.
pub(crate) fn gateway_fits(m: &Manifest, ours: &[u32]) -> Result<(), String> {
    range_fits(m.min_gateway.as_deref(), m.max_gateway.as_deref(), ours)
}

/// [`gateway_fits`] on the bounds alone: the updater judges a catalogue
/// entry the same way before anything is downloaded.
pub(crate) fn range_fits(min: Option<&[u32]>, max: Option<&[u32]>, ours: &[u32]) -> Result<(), String> {
    let cut = |n: usize| {
        let mut v = ours.to_vec();
        v.resize(3, 0);
        v.truncate(n);
        v
    };
    if let Some(min) = min {
        if cut(min.len()).as_slice() < min {
            return Err(format!(
                "it needs Ethernet Gateway {} or later; this is {}",
                show_version(min),
                show_version(ours)
            ));
        }
    }
    if let Some(max) = max {
        if cut(max.len()).as_slice() > max {
            return Err(format!(
                "it is for Ethernet Gateway up to {}; this is {}",
                show_version(max),
                show_version(ours)
            ));
        }
    }
    Ok(())
}

fn parse_num<T: std::str::FromStr>(key: &str, v: &str) -> Result<T, String> {
    v.parse::<T>().map_err(|_| format!("{key} must be a whole number, not \"{v}\""))
}

/// Read an `addon.conf`.  `id` is the add-on's folder name.
///
/// The same `key = value` lines as `egateway.conf`, with `#` comments.  A
/// key given twice keeps the last.  **Unknown keys are ignored**, so a
/// manifest written for a later gateway still loads here; a misspelt
/// `command` shows up anyway, as the missing command it leaves behind.
pub(crate) fn parse_manifest(id: &str, text: &str) -> Result<Manifest, String> {
    let mut kv: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            return Err(format!("\"{line}\" is not a key = value line"));
        };
        kv.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
    }
    let get = |k: &str| kv.get(k).map(String::as_str).filter(|v| !v.is_empty());
    // This platform's override first, then the plain key.
    let pick = |base: &str| {
        get(&format!("{base}_{}", platform_key())).or_else(|| get(base)).map(str::to_string)
    };

    let mode = match get("mode").map(str::to_ascii_lowercase).as_deref() {
        None | Some("terminal") => Mode::Terminal,
        Some("gui") => Mode::Gui,
        Some("both") => Mode::Both,
        Some(other) => return Err(format!("mode must be terminal, gui or both, not \"{other}\"")),
    };
    let command = pick("command");
    let gui_command = pick("gui_command");
    if mode.has_terminal() && command.is_none() {
        return Err("no command: the program to run is not named".into());
    }
    if mode.has_gui() && gui_command.is_none() {
        return Err("no gui_command: the window's program is not named".into());
    }
    let args = split_args(&pick("args").unwrap_or_default()).map_err(|e| format!("args: {e}"))?;
    let gui_args =
        split_args(&pick("gui_args").unwrap_or_default()).map_err(|e| format!("gui_args: {e}"))?;

    let terminals = match get("terminals") {
        None => Terminals::ALL,
        Some(list) => {
            let mut t = Terminals { ansi: false, ascii: false, petscii: false };
            for word in list.split(',').map(|w| w.trim().to_ascii_lowercase()) {
                match word.as_str() {
                    "ansi" => t.ansi = true,
                    "ascii" => t.ascii = true,
                    "petscii" => t.petscii = true,
                    "" => {}
                    other => {
                        return Err(format!(
                            "terminals lists \"{other}\": use ansi, ascii and petscii"
                        ))
                    }
                }
            }
            if t == (Terminals { ansi: false, ascii: false, petscii: false }) {
                return Err("terminals names no terminal type".into());
            }
            t
        }
    };
    let petscii = match get("petscii").map(str::to_ascii_lowercase).as_deref() {
        None | Some("translate") => Petscii::Translate,
        Some("native") => Petscii::Native,
        Some(other) => {
            return Err(format!("petscii must be translate or native, not \"{other}\""))
        }
    };

    let input = match get("input").map(str::to_ascii_lowercase).as_deref() {
        None | Some("line") => Input::Line,
        Some("raw") => Input::Raw,
        Some(other) => return Err(format!("input must be line or raw, not \"{other}\"")),
    };

    Ok(Manifest {
        id: id.to_string(),
        name: get("name").unwrap_or(id).to_string(),
        description: get("description").unwrap_or_default().to_string(),
        version: get("version").unwrap_or_default().to_string(),
        author: get("author").unwrap_or_default().to_string(),
        mode,
        command,
        args,
        gui_command,
        gui_args,
        terminals,
        min_columns: get("min_columns").map(|v| parse_num("min_columns", v)).transpose()?.unwrap_or(0),
        petscii,
        input,
        idle_minutes: get("idle_minutes").map(|v| parse_num("idle_minutes", v)).transpose()?.unwrap_or(0),
        max_minutes: get("max_minutes").map(|v| parse_num("max_minutes", v)).transpose()?.unwrap_or(0),
        gui_wait: get("gui_wait").map(|v| parse_bool("gui_wait", v)).transpose()?.unwrap_or(false),
        single_instance: get("single_instance")
            .map(|v| parse_bool("single_instance", v))
            .transpose()?
            .unwrap_or(false),
        min_gateway: get("min_gateway").map(|v| parse_version("min_gateway", v)).transpose()?,
        max_gateway: get("max_gateway").map(|v| parse_version("max_gateway", v)).transpose()?,
    })
}

/// The program `cmd` names, as a path that is **inside `dir`**.
///
/// Refused: an absolute path, any `..`, and anything that canonicalizes to
/// somewhere outside the folder -- a symlink out included, which only
/// canonicalizing catches.  On Unix the file must also be executable, so
/// the operator is told "chmod +x" here rather than meeting a bare
/// "permission denied" at launch.
///
/// **Checked again at every launch, not only at discovery.**  The folder can
/// change between the list being drawn and a number being typed, and the
/// launch is the step that relies on the answer.
pub(crate) fn resolve_program(dir: &Path, cmd: &str) -> Result<PathBuf, String> {
    let rel = Path::new(cmd);
    if cmd.trim().is_empty() {
        return Err("the command is empty".into());
    }
    // Only plain names (and `.`): this also refuses an absolute path, whose
    // root or drive prefix is a component of its own on every platform.
    if rel.components().any(|c| !matches!(c, Component::Normal(_) | Component::CurDir)) {
        return Err(format!("{cmd} must be a file inside the add-on's own folder"));
    }
    let canon_dir = dir
        .canonicalize()
        .map_err(|e| format!("its folder cannot be read: {e}"))?;
    let canon = dir
        .join(rel)
        .canonicalize()
        .map_err(|_| format!("{cmd} is not in the add-on's folder"))?;
    if !canon.starts_with(&canon_dir) {
        return Err(format!("{cmd} leads outside the add-on's folder"));
    }
    if !canon.is_file() {
        return Err(format!("{cmd} is not a file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = canon.metadata().map(|m| m.permissions().mode()).unwrap_or(0);
        if mode & 0o111 == 0 {
            return Err(format!("{cmd} is not executable (chmod +x {cmd})"));
        }
    }
    Ok(canon)
}

/// One add-on found on disk: its folder and its manifest.
#[derive(Clone, Debug)]
pub(crate) struct AddOn {
    pub dir: PathBuf,
    pub manifest: Manifest,
}

/// Read one add-on's folder: its manifest, then each program it names.
pub(crate) fn load(dir: &Path) -> Result<AddOn, String> {
    let id = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let text = std::fs::read_to_string(dir.join(MANIFEST))
        .map_err(|_| format!("no {MANIFEST} in it"))?;
    let manifest = parse_manifest(&id, &text)?;
    // Before the programs: one built for another gateway is not offered,
    // whatever else is wrong with it.
    gateway_fits(&manifest, &this_gateway())?;
    if let Some(cmd) = &manifest.command {
        resolve_program(dir, cmd)?;
    }
    if let Some(cmd) = &manifest.gui_command {
        resolve_program(dir, cmd)?;
    }
    Ok(AddOn { dir: dir.to_path_buf(), manifest })
}

/// Every usable add-on under `root`, sorted by name without case.
///
/// Hidden folders (a leading `.`) and plain files (the README) are not
/// add-ons and are passed over without a word; a folder that fails `load` is
/// left out and logged once (see the module comment).
pub(crate) fn discover_in(root: &Path) -> Vec<AddOn> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || !path.is_dir() {
            continue;
        }
        match load(&path) {
            Ok(a) => found.push(a),
            Err(why) => log_once(&name, &why),
        }
    }
    found.sort_by(|a, b| {
        a.manifest
            .name
            .to_lowercase()
            .cmp(&b.manifest.name.to_lowercase())
            .then_with(|| a.manifest.id.cmp(&b.manifest.id))
    });
    found
}

/// Every add-on installed in `ethernetgateway-data/AddOns`, offered or not:
/// what the settings screens list.  Callers are shown [`offered`].
pub(crate) fn discover() -> Vec<AddOn> {
    discover_in(&addons_root())
}

/// The folder names in an `addons_disabled` value.
pub(crate) fn disabled_ids(list: &str) -> Vec<&str> {
    list.split(',').map(str::trim).filter(|n| !n.is_empty()).collect()
}

/// `installed` less what the settings switch off: everything, when add-ons
/// are off as a whole, and each one named in `addons_disabled`.  A function
/// of its inputs so the filter is tested without a config file.
pub(crate) fn offered_from(installed: Vec<AddOn>, enabled: bool, disabled: &str) -> Vec<AddOn> {
    if !enabled {
        return Vec::new();
    }
    let off = disabled_ids(disabled);
    installed.into_iter().filter(|a| !off.contains(&a.manifest.id.as_str())).collect()
}

/// The add-ons a caller is offered.
pub(crate) fn offered() -> Vec<AddOn> {
    let cfg = crate::config::get_config();
    offered_from(discover(), cfg.addons_enabled, &cfg.addons_disabled)
}

/// Whether there is any add-on to offer.
pub(crate) fn any() -> bool {
    !offered().is_empty()
}

/// `disabled` with `id` switched on or off: the new `addons_disabled`.
pub(crate) fn with_switched(disabled: &str, id: &str, on: bool) -> String {
    let mut ids: Vec<&str> = disabled_ids(disabled).into_iter().filter(|n| *n != id).collect();
    if !on {
        ids.push(id);
    }
    crate::config::normalize_addon_list(&ids.join(","))
}

/// Log why an add-on was skipped -- once per folder and reason.
fn log_once(id: &str, why: &str) {
    static SEEN: Mutex<Option<HashSet<String>>> = Mutex::new(None);
    let key = format!("{id}\u{0}{why}");
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    if seen.get_or_insert_with(HashSet::new).insert(key) {
        crate::logger::log(format!("Add-on \"{id}\" skipped: {why}."));
    }
}

/// The README placed in the add-ons folder: what goes there, in brief.  The
/// manual's "Writing an add-on" is the full story.
pub(crate) const README_TEXT: &str = "\
Ethernet Gateway add-ons
========================

Put each add-on in its own folder here:

    AddOns/
      hello/
        addon.conf      <- the manifest (required)
        hello.sh        <- the program it names
        ...             <- anything else it needs

A minimal addon.conf:

    name = Hello
    description = Says hello and shows what the gateway tells it
    command = hello.sh

Callers find add-ons on the Second Menu (2 on the main menu), under
A  Add-Ons.  A new folder shows up the next time the list is opened;
no restart is needed.  A folder that cannot be used is left out of the
list and the reason is written to the gateway's log.

Add-ons run as the same user as the gateway, with its rights.  Only put
programs here that you trust.

Ready-made add-ons: the setup wizard can download them, C (Check for
add-ons) on the Add-Ons list offers new and updated ones, or copy them by
hand from https://github.com/rickybryce/ethernetgateway-addons
The user manual's \"Writing an add-on\" section lists every manifest key.
";

/// Create the add-ons folder and its README if they are missing.
///
/// **Never overwrites the README**: `create_new` fails on an existing file,
/// so an operator's own notes in it survive every upgrade.
pub(crate) fn ensure_folder() {
    ensure_folder_in(&addons_root());
}

pub(crate) fn ensure_folder_in(root: &Path) {
    if let Err(e) = std::fs::create_dir_all(root) {
        crate::logger::log(format!("Add-ons folder {} could not be created: {e}", root.display()));
        return;
    }
    let readme = root.join("README.txt");
    if let Ok(mut f) = std::fs::OpenOptions::new().write(true).create_new(true).open(&readme) {
        use std::io::Write;
        let _ = f.write_all(README_TEXT.as_bytes());
    }
}

/// The gateway's environment variables an add-on is given; nothing else
/// of the gateway's own environment reaches it.
///
/// **An allowlist, not a blocklist.**  `env_clear` first, then these: what a
/// program needs to find its tools, its home and its language.  Whatever
/// else the gateway was started with -- a token in a service unit, a key in
/// a shell profile -- is not an add-on's business, and a blocklist would
/// have to know every name in advance.
pub(crate) const PASSED_ENV: &[&str] = &[
    "PATH", "HOME", "USER", "LOGNAME", "LANG", "LC_ALL", "LC_CTYPE", "TZ", "TMPDIR",
    // Windows: a program cannot start without the first three.
    "SYSTEMROOT", "SYSTEMDRIVE", "WINDIR", "COMSPEC", "PATHEXT", "USERPROFILE", "TEMP", "TMP",
];

/// The environment an add-on runs with: [`PASSED_ENV`] from the gateway's
/// own, then `extra` (the `EG_*` set and `TERM`), which wins on a clash.
pub(crate) fn add_on_env(extra: &[(String, String)]) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = PASSED_ENV
        .iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
        .collect();
    for (k, v) in extra {
        env.retain(|(ek, _)| ek != k);
        env.push((k.clone(), v.clone()));
    }
    env
}

/// The command for one side of an add-on: its program, checked again (see
/// `resolve_program`), run from its own folder with only `env`.
///
/// **No shell, except the one Windows insists on.**  The program is started
/// directly with its arguments as words, so nothing in a manifest is
/// interpreted by a shell -- but Windows runs a `.cmd`/`.bat` only through
/// `cmd.exe`, so for those the arguments do pass through it.  Rust escapes
/// them for `cmd.exe` and refuses any it cannot escape safely (the
/// CVE-2024-24576 fix, well inside our MSRV), so a manifest still cannot
/// inject a command; an argument with `%` or `^` may arrive altered.  On Unix it
/// leads a process group of its own, so leaving takes its children too
/// (`Tree`); `kill_on_drop` covers a session that ends any other way.
pub(crate) fn command(
    dir: &Path,
    cmd: &str,
    args: &[String],
    env: &[(String, String)],
) -> Result<tokio::process::Command, String> {
    let mut c = tokio::process::Command::from(std_command(dir, cmd, args, env)?);
    c.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    Ok(c)
}

/// [`command`]'s common part, as a `std` command: the GUI side starts from
/// it too, from the desktop window's thread as well as from a session, and
/// so cannot need a runtime.  Stdio is left to the caller.
pub(crate) fn std_command(
    dir: &Path,
    cmd: &str,
    args: &[String],
    env: &[(String, String)],
) -> Result<std::process::Command, String> {
    let program = resolve_program(dir, cmd)?;
    let mut c = std::process::Command::new(program);
    c.args(args).current_dir(dir).env_clear().envs(env.iter().map(|(k, v)| (k, v)));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        c.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // No console window flashing up on the gateway's screen.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    Ok(c)
}

/// A running add-on's whole process tree: the add-on and everything it
/// starts, ended as one.
///
/// **Recorded at spawn, not asked for at the end.**  The first version asked
/// the child for its id when it was time to kill, and tokio answers `None`
/// once the child has been reaped -- so an add-on that started a program in
/// the background and then exited by itself (`game &`, then `exit`) left the
/// game running for the life of the machine.  Found in review, 2026-11-22.
///
/// Unix: the add-on leads its own process group (`command`), whose id is its
/// pid, and the group outlives its leader while any member is alive, so
/// `killpg` reaches the background program after the add-on has gone.
/// Windows: the add-on is put in a job object set to end every process in it
/// when the job is closed -- `taskkill /T` cannot walk a tree whose root has
/// exited, and was a blocking call in async code besides.  A program the
/// add-on starts in its first instant, before it is assigned, can escape the
/// job; that is the documented limit of assigning after spawn.
pub(crate) struct Tree {
    #[cfg(unix)]
    pgid: Option<i32>,
    /// The job's `HANDLE`, kept as an integer so `Tree` is `Send` and can be
    /// held across an `.await`.
    #[cfg(windows)]
    job: Option<isize>,
}

impl Tree {
    /// Spawn `cmd` and take hold of its tree.
    pub(crate) fn spawn(cmd: &mut tokio::process::Command) -> std::io::Result<(tokio::process::Child, Tree)> {
        let child = cmd.spawn()?;
        #[cfg(unix)]
        let tree = Tree { pgid: child.id().map(|p| p as i32) };
        #[cfg(windows)]
        let tree = Tree { job: child.raw_handle().and_then(|h| windows_job(h as isize)) };
        #[cfg(not(any(unix, windows)))]
        let tree = Tree {};
        Ok((child, tree))
    }

    /// The tree of a `std` child (the GUI side), taken at once after spawn.
    pub(crate) fn of_std(child: &std::process::Child) -> Tree {
        #[cfg(unix)]
        {
            Tree { pgid: Some(child.id() as i32) }
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            Tree { job: windows_job(child.as_raw_handle() as isize) }
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = child;
            Tree {}
        }
    }

    /// End everything in the tree, the add-on included, without its `Child`
    /// (which the GUI side's waiting thread owns).  Once, as `kill`.
    pub(crate) fn kill_all(&mut self) {
        #[cfg(unix)]
        if let Some(pgid) = self.pgid.take() {
            // SAFETY: as in `kill`.
            unsafe {
                libc::killpg(pgid, libc::SIGKILL);
            }
        }
        #[cfg(windows)]
        if let Some(job) = self.job.take() {
            use windows_sys::Win32::Foundation::CloseHandle;
            use windows_sys::Win32::System::JobObjects::TerminateJobObject;
            // SAFETY: as in `kill`.
            unsafe {
                TerminateJobObject(job as _, 1);
                CloseHandle(job as _);
            }
        }
    }

    /// End the add-on and everything it started.  Once: a second call does
    /// nothing, so a recycled id is never signalled.
    pub(crate) fn kill(&mut self, child: &mut tokio::process::Child) {
        self.kill_all();
        let _ = child.start_kill();
    }
}

impl Drop for Tree {
    /// A tree nobody ended explicitly is still ended: a session can be
    /// dropped, or return early on a failed write, between the spawn and the
    /// bridge's own `kill`.  `kill_on_drop` ends only the add-on itself.
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pgid) = self.pgid.take() {
            // SAFETY: as in `kill`.
            unsafe {
                libc::killpg(pgid, libc::SIGKILL);
            }
        }
        #[cfg(windows)]
        if let Some(job) = self.job.take() {
            // SAFETY: as in `kill`.
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(job as _);
            }
        }
    }
}

/// A job object that ends its processes when closed, with the process whose
/// handle is `process` in it.
#[cfg(windows)]
fn windows_job(process: isize) -> Option<isize> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    // SAFETY: plain Win32 calls on a handle we own; every failure path closes
    // the job it created.
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return None;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let set = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if set == 0 || AssignProcessToJobObject(job, process as _) == 0 {
            CloseHandle(job);
            return None;
        }
        Some(job as isize)
    }
}

/// The add-ons running right now that asked to be the only copy.
static RUNNING_SINGLE: Mutex<Option<HashSet<String>>> = Mutex::new(None);

/// Held while a `single_instance` add-on runs; dropping it frees the slot,
/// however the session ends.
pub(crate) struct SingleInstance(String);

impl SingleInstance {
    /// `None` when that add-on is already running.
    pub(crate) fn claim(id: &str) -> Option<SingleInstance> {
        let mut set = RUNNING_SINGLE.lock().unwrap_or_else(|e| e.into_inner());
        set.get_or_insert_with(HashSet::new)
            .insert(id.to_string())
            .then(|| SingleInstance(id.to_string()))
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        let mut set = RUNNING_SINGLE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(s) = set.as_mut() {
            s.remove(&self.0);
        }
    }
}

/// Runs of each add-on under way now, any caller.
static IN_USE: Mutex<Option<std::collections::HashMap<String, usize>>> = Mutex::new(None);

/// Held for a terminal run of an add-on: while any is held, the updater
/// leaves that add-on's folder alone.
pub(crate) struct InUse(String);

impl InUse {
    pub(crate) fn enter(id: &str) -> InUse {
        let mut map = IN_USE.lock().unwrap_or_else(|e| e.into_inner());
        *map.get_or_insert_with(Default::default).entry(id.to_string()).or_insert(0) += 1;
        InUse(id.to_string())
    }
}

impl Drop for InUse {
    fn drop(&mut self) {
        let mut map = IN_USE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(m) = map.as_mut()
            && let Some(n) = m.get_mut(&self.0)
        {
            *n -= 1;
            if *n == 0 {
                m.remove(&self.0);
            }
        }
    }
}

/// Whether anyone is running `id` now.
pub(crate) fn in_use(id: &str) -> bool {
    IN_USE.lock().unwrap_or_else(|e| e.into_inner()).as_ref().is_some_and(|m| m.contains_key(id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("egw-addons-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Write an executable program into `dir`.
    fn program(dir: &Path, name: &str) {
        let p = dir.join(name);
        std::fs::write(&p, "#!/bin/sh\necho hi\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    fn addon(root: &Path, id: &str, conf: &str) -> PathBuf {
        let d = root.join(id);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(MANIFEST), conf).unwrap();
        d
    }

    /// **The defaults are the design's**: a terminal add-on for every
    /// terminal type, translated for PETSCII, no timeouts, returning at once.
    #[test]
    fn test_a_minimal_manifest_takes_the_documented_defaults() {
        let m = parse_manifest("hello", "command = hello.sh\n").unwrap();
        assert_eq!(m.name, "hello", "the name defaults to the folder");
        assert_eq!(m.mode, Mode::Terminal);
        assert_eq!(m.command.as_deref(), Some("hello.sh"));
        assert!(m.args.is_empty());
        assert_eq!(m.terminals, Terminals::ALL);
        assert_eq!(m.petscii, Petscii::Translate);
        assert_eq!(m.input, Input::Line, "a pipe needs the gateway's echo by default");
        assert_eq!((m.min_columns, m.idle_minutes, m.max_minutes), (0, 0, 0));
        assert!(!m.gui_wait && !m.single_instance);
    }

    #[test]
    fn test_every_manifest_key_is_read() {
        let m = parse_manifest(
            "chess",
            "# a comment\n\
             name = Chess\n\
             description = Play the gateway\n\
             version = 1.2\n\
             author = Someone\n\
             mode = both\n\
             command = chess\n\
             args = --level 3 \"two words\"\n\
             gui_command = board\n\
             gui_args = 'a b'\n\
             terminals = ansi, petscii\n\
             min_columns = 80\n\
             petscii = native\n\
             input = raw\n\
             idle_minutes = 10\n\
             max_minutes = 60\n\
             gui_wait = yes\n\
             single_instance = yes\n\
             min_gateway = 1.1\n\
             max_gateway = 2.0.3\n",
        )
        .unwrap();
        assert_eq!((m.name.as_str(), m.description.as_str()), ("Chess", "Play the gateway"));
        assert_eq!((m.version.as_str(), m.author.as_str()), ("1.2", "Someone"));
        assert_eq!(m.mode, Mode::Both);
        assert_eq!(m.args, vec!["--level", "3", "two words"]);
        assert_eq!(m.gui_command.as_deref(), Some("board"));
        assert_eq!(m.gui_args, vec!["a b"]);
        assert_eq!(m.terminals, Terminals { ansi: true, ascii: false, petscii: true });
        assert_eq!(m.petscii, Petscii::Native);
        assert_eq!(m.input, Input::Raw);
        assert_eq!((m.min_columns, m.idle_minutes, m.max_minutes), (80, 10, 60));
        assert!(m.gui_wait && m.single_instance);
        assert_eq!((m.min_gateway, m.max_gateway), (Some(vec![1, 1]), Some(vec![2, 0, 3])));
    }

    /// This platform's `command_<os>` wins over the plain key; another
    /// platform's is ignored.
    #[test]
    fn test_the_platform_override_wins() {
        let text = format!("command = plain\ncommand_{} = mine\n", platform_key());
        assert_eq!(parse_manifest("x", &text).unwrap().command.as_deref(), Some("mine"));
        let other = if platform_key() == "windows" { "linux" } else { "windows" };
        let text = format!("command = plain\ncommand_{other} = theirs\n");
        assert_eq!(parse_manifest("x", &text).unwrap().command.as_deref(), Some("plain"));
    }

    #[test]
    fn test_a_bad_manifest_says_what_is_wrong() {
        let err = |t: &str| parse_manifest("x", t).unwrap_err();
        assert!(err("name = X\n").contains("no command"));
        assert!(err("mode = gui\n").contains("no gui_command"));
        assert!(err("command = a\nmode = window\n").contains("mode must be"));
        assert!(err("command = a\npetscii = maybe\n").contains("petscii must be"));
        assert!(err("command = a\ninput = cooked\n").contains("input must be"));
        assert!(err("command = a\nterminals = vt100\n").contains("vt100"));
        assert!(err("command = a\nterminals = ,\n").contains("no terminal type"));
        assert!(err("command = a\nmin_columns = wide\n").contains("whole number"));
        assert!(err("command = a\ngui_wait = perhaps\n").contains("yes or no"));
        assert!(err("command = a\nargs = \"open\n").contains("not closed"));
        assert!(err("just words\n").contains("key = value"));
        for bad in ["1.x", "1..2", "v1.1", "+1", "1.2.3.4", "1.1.0-beta"] {
            assert!(err(&format!("command = a\nmin_gateway = {bad}\n")).contains("min_gateway must be a version"), "{bad}");
        }
        assert!(err("command = a\nmax_gateway = 2.\n").contains("max_gateway must be a version"));
        // Unknown keys are not an error: a later gateway's manifest loads.
        assert!(parse_manifest("x", "command = a\nfuture_key = 1\n").is_ok());
    }

    #[test]
    fn test_args_split_like_a_shell_without_one() {
        assert_eq!(split_args("  a  b ").unwrap(), vec!["a", "b"]);
        assert_eq!(split_args("'$HOME *' \"x\\\"y\"").unwrap(), vec!["$HOME *", "x\"y"]);
        assert_eq!(split_args(r"data\scores.txt").unwrap(), vec![r"data\scores.txt"]);
        assert_eq!(split_args("''").unwrap(), vec![""], "an empty quoted word is a word");
        assert!(split_args("'open").is_err());
    }

    /// **The containment check**: nothing outside the add-on's own folder,
    /// by any route -- absolute, `..`, or a symlink that leads out.
    #[test]
    fn test_a_command_must_stay_inside_its_folder() {
        let root = tmp("contain");
        let dir = root.join("a");
        std::fs::create_dir_all(&dir).unwrap();
        program(&dir, "ok.sh");
        program(&root, "outside.sh");
        assert!(resolve_program(&dir, "ok.sh").is_ok());
        assert!(resolve_program(&dir, "./ok.sh").is_ok());
        assert!(resolve_program(&dir, "../outside.sh").unwrap_err().contains("inside"));
        let abs = root.join("outside.sh").to_string_lossy().into_owned();
        assert!(resolve_program(&dir, &abs).unwrap_err().contains("inside"));
        assert!(resolve_program(&dir, "missing.sh").unwrap_err().contains("not in"));
        assert!(resolve_program(&dir, "").is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root.join("outside.sh"), dir.join("link.sh")).unwrap();
            assert!(resolve_program(&dir, "link.sh").unwrap_err().contains("outside"));
            std::fs::write(dir.join("plain.sh"), "x").unwrap();
            assert!(resolve_program(&dir, "plain.sh").unwrap_err().contains("chmod +x"));
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Bad folders are skipped, good ones are found and sorted by name.
    #[test]
    fn test_discovery_skips_what_cannot_be_used() {
        let root = tmp("discover");
        program(&addon(&root, "zeta", "name = alpha\ncommand = z.sh\n"), "z.sh");
        program(&addon(&root, "beta", "name = Beta\ncommand = b.sh\n"), "b.sh");
        addon(&root, "broken", "mode = sideways\ncommand = x\n");
        addon(&root, "nocmd", "name = Missing\ncommand = gone.sh\n");
        std::fs::create_dir_all(root.join("empty")).unwrap();
        let hidden = addon(&root, ".hidden", "command = h.sh\n");
        program(&hidden, "h.sh");
        std::fs::write(root.join("README.txt"), "x").unwrap();
        let found: Vec<String> = discover_in(&root).into_iter().map(|a| a.manifest.id).collect();
        assert_eq!(found, vec!["zeta", "beta"], "sorted by name (alpha, Beta), bad and hidden left out");
        assert!(discover_in(&root.join("nowhere")).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **Each bound is compared on the parts it gives**: `min_gateway = 1.4`
    /// starts at 1.4.0, `max_gateway = 1.4` runs to the last 1.4.x, and no
    /// bound at all fits every gateway.
    #[test]
    fn test_the_gateway_range_compares_the_parts_given() {
        let fits = |conf: &str, ours: &[u32]| {
            gateway_fits(&parse_manifest("x", &format!("command = a\n{conf}")).unwrap(), ours)
        };
        assert!(fits("", &[0, 1, 0]).is_ok() && fits("", &[99, 0, 0]).is_ok());
        assert!(fits("min_gateway = 1.4\n", &[1, 4, 0]).is_ok());
        assert!(fits("min_gateway = 1.4\n", &[1, 3, 9]).unwrap_err().contains("needs Ethernet Gateway 1.4 or later; this is 1.3.9"));
        assert!(fits("min_gateway = 1.1.2\n", &[1, 1, 1]).is_err());
        assert!(fits("min_gateway = 1.1.2\n", &[1, 1, 2]).is_ok());
        assert!(fits("min_gateway = 2\n", &[1, 99, 99]).is_err());
        assert!(fits("max_gateway = 1.4\n", &[1, 4, 99]).is_ok(), "1.4 covers every 1.4.x");
        assert!(fits("max_gateway = 1.4\n", &[1, 5, 0]).unwrap_err().contains("up to 1.4; this is 1.5.0"));
        assert!(fits("max_gateway = 1.4.2\n", &[1, 4, 3]).is_err());
        assert!(fits("max_gateway = 1\n", &[1, 9, 9]).is_ok());
        assert!(fits("min_gateway = 1.1\nmax_gateway = 1.4\n", &[1, 2, 0]).is_ok());
        assert!(fits("min_gateway = 1.1\nmax_gateway = 1.4\n", &[1, 0, 9]).is_err());
        assert_eq!(this_gateway().len(), 3, "the crate version parses");
        assert_eq!(crate_version("1.0.0-rc2"), vec![1, 0, 0], "a candidate counts as its release");
        assert_eq!(crate_version("1.2.3+build.7"), vec![1, 2, 3]);
    }

    /// **An add-on for another gateway is never offered**: `load` drops it
    /// and says why, before it looks at the programs.
    #[test]
    fn test_an_add_on_for_another_gateway_is_left_out() {
        let root = tmp("range");
        let ours = show_version(&this_gateway());
        let next = { let v = this_gateway(); show_version(&[v[0] + 1]) };
        program(&addon(&root, "fits", &format!("command = f.sh\nmin_gateway = {ours}\nmax_gateway = {ours}\n")), "f.sh");
        program(&addon(&root, "later", &format!("command = l.sh\nmin_gateway = {next}\n")), "l.sh");
        program(&addon(&root, "older", "command = o.sh\nmax_gateway = 0.9\n"), "o.sh");
        let found: Vec<String> = discover_in(&root).into_iter().map(|a| a.manifest.id).collect();
        assert_eq!(found, vec!["fits"]);
        assert!(load(&root.join("later")).unwrap_err().contains("or later"));
        assert!(load(&root.join("older")).unwrap_err().contains("up to 0.9"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **Nothing of the gateway's own environment reaches an add-on**
    /// except the allowlist: a secret in the gateway's environment stays
    /// there, and the `EG_*` values win over a passed one of the same name.
    #[test]
    fn test_an_add_on_gets_only_the_allowlisted_environment() {
        // SAFETY: test-only; no other test reads this variable.
        unsafe { std::env::set_var("EGW_TEST_SECRET_GROQ", "sk-secret") };
        let env = add_on_env(&[("EG_TERMINAL".into(), "ansi".into()), ("HOME".into(), "/x".into())]);
        assert!(!env.iter().any(|(k, v)| k.contains("SECRET") || v == "sk-secret"));
        assert!(env.iter().all(|(k, _)| PASSED_ENV.contains(&k.as_str()) || k == "EG_TERMINAL"));
        assert_eq!(env.iter().filter(|(k, _)| k == "HOME").count(), 1);
        assert!(env.contains(&("HOME".into(), "/x".into())));
        if std::env::var("PATH").is_ok() {
            assert!(env.iter().any(|(k, _)| k == "PATH"), "a program cannot find its tools");
        }
    }

    #[test]
    fn test_a_single_instance_add_on_runs_once() {
        let first = SingleInstance::claim("egw-test-solo").expect("first claim");
        assert!(SingleInstance::claim("egw-test-solo").is_none(), "a second copy started");
        drop(first);
        assert!(SingleInstance::claim("egw-test-solo").is_some(), "the slot was never freed");
    }

    /// Leaving takes the add-on's children with it, not just the add-on.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_leaving_kills_the_add_ons_children_too() {
        let root = tmp("killtree");
        let script = root.join("parent.sh");
        let pidfile = root.join("child.pid");
        std::fs::write(
            &script,
            format!("#!/bin/sh\nsleep 300 &\necho $! > {}\nwait\n", pidfile.display()),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let (mut child, mut tree) = Tree::spawn(&mut command(&root, "parent.sh", &[], &add_on_env(&[])).unwrap()).unwrap();
        let mut pid = String::new();
        for _ in 0..100 {
            pid = std::fs::read_to_string(&pidfile).unwrap_or_default();
            if !pid.trim().is_empty() { break; }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let grandchild: i32 = pid.trim().parse().expect("the script never started its child");
        tree.kill(&mut child);
        let _ = child.wait().await;
        let mut gone = false;
        for _ in 0..100 {
            // SAFETY: signal 0 only asks whether the process exists.
            if unsafe { libc::kill(grandchild, 0) } != 0 { gone = true; break; }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(gone, "the add-on's child outlived it");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **An add-on that exits by itself does not leave its background
    /// programs running.**  The first `kill_tree` asked the child for its id
    /// at the end, and tokio has none to give once the child is reaped -- so
    /// the group was never signalled.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_an_add_on_that_exits_by_itself_takes_its_children_too() {
        use std::os::unix::fs::PermissionsExt;
        let root = tmp("selfexit");
        let pidfile = root.join("child.pid");
        std::fs::write(
            root.join("run.sh"),
            format!("#!/bin/sh\nsleep 300 >/dev/null 2>&1 &\necho $! > {}\nexit 0\n", pidfile.display()),
        )
        .unwrap();
        std::fs::set_permissions(root.join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let (mut child, mut tree) = Tree::spawn(&mut command(&root, "run.sh", &[], &add_on_env(&[])).unwrap()).unwrap();
        let _ = child.wait().await;
        assert!(child.id().is_none(), "the precondition: tokio has forgotten the id");
        let grandchild: i32 = std::fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
        tree.kill(&mut child);
        let mut gone = false;
        for _ in 0..100 {
            // SAFETY: signal 0 only asks whether the process exists.
            if unsafe { libc::kill(grandchild, 0) } != 0 { gone = true; break; }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(gone, "the add-on's background program outlived it");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **The real spawn clears the gateway's environment.**  The allowlist
    /// test above checks `add_on_env` alone, and passed with `env_clear`
    /// deleted from `command`; this one runs `env` the way an add-on runs.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_a_spawned_add_on_sees_none_of_the_gateways_secrets() {
        use std::os::unix::fs::PermissionsExt;
        let root = tmp("realenv");
        std::fs::write(root.join("run.sh"), "#!/bin/sh\nenv\n").unwrap();
        std::fs::set_permissions(root.join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
        // SAFETY: test-only; no other test reads this variable.
        unsafe { std::env::set_var("EGW_TEST_SECRET_SPAWN", "sk-spawned") };
        let mut c = command(&root, "run.sh", &[], &add_on_env(&[("EG_TERMINAL".into(), "ansi".into())])).unwrap();
        let out = c.output().await.unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(!text.contains("sk-spawned"), "the gateway's environment reached the add-on:\n{text}");
        assert!(text.contains("EG_TERMINAL=ansi"), "{text}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The README is written once and never over an operator's own.
    #[test]
    fn test_the_readme_is_never_overwritten() {
        let root = tmp("readme").join(ADDONS_DIR);
        ensure_folder_in(&root);
        assert_eq!(std::fs::read_to_string(root.join("README.txt")).unwrap(), README_TEXT);
        std::fs::write(root.join("README.txt"), "my notes").unwrap();
        ensure_folder_in(&root);
        assert_eq!(std::fs::read_to_string(root.join("README.txt")).unwrap(), "my notes");
        let _ = std::fs::remove_dir_all(root.parent().unwrap());
    }

    #[test]
    fn test_offered_leaves_out_what_the_settings_switch_off() {
        let mk = |id: &str| AddOn {
            dir: PathBuf::from("/nonexistent"),
            manifest: parse_manifest(id, "command = x.sh\n").unwrap(),
        };
        let all = || vec![mk("hello"), mk("guess"), mk("chess")];
        let ids = |v: Vec<AddOn>| v.into_iter().map(|a| a.manifest.id).collect::<Vec<_>>();
        assert_eq!(ids(offered_from(all(), true, "")), ["hello", "guess", "chess"]);
        assert_eq!(ids(offered_from(all(), true, "guess, chess")), ["hello"]);
        assert!(offered_from(all(), false, "").is_empty(), "off as a whole offers nothing");
    }

    #[test]
    fn test_switching_an_add_on_edits_only_its_own_name() {
        assert_eq!(with_switched("", "hello", false), "hello");
        assert_eq!(with_switched("hello", "hello", false), "hello", "no repeat");
        assert_eq!(with_switched("guess,hello", "hello", true), "guess");
        assert_eq!(with_switched("guess", "hello", true), "guess");
    }

}
