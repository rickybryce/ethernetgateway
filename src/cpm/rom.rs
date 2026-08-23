//! Monitor ROMs an operator supplies, loaded into a **booted** machine's memory.
//!
//! Some disks are not self-contained.  They carry an operating system and a
//! BIOS, and then print through a routine that was never on the disk at all,
//! because on the machine they were built for it was already in memory — a
//! monitor in ROM, or loaded from tape before the disk was ever inserted.  Such
//! a disk boots into silence on any emulator that does not have it, and there is
//! nothing wrong with either the disk or the emulator.
//!
//! [`super::console::MonitorRom`] answers a *narrow* version of this: where one
//! entry point is all a disk needs, the entry is **synthesised** — six real
//! instructions at the real address, which the guest cannot tell from the
//! original.  That works for TDISK05 and it does not scale, which was measured
//! rather than assumed.  See [`ROM_CHOICES`] for the case that broke it.
//!
//! # What this is not
//!
//! It is not a general "load a file into memory" facility, and the difference is
//! the [`ROM_CHOICES`] catalogue.  A monitor is only useful at the address it
//! was assembled for, its span has to miss whatever the guest puts there, and
//! an operator has no way to know either from the file.  So the *gateway* owns
//! the address and the operator owns the choice, exactly as with
//! [`super::uart::UART_CHOICES`] and [`super::console::MACHINE_CHOICES`].
//!
//! # Nothing here is shipped
//!
//! These are other people's ROMs.  The gateway carries none of them, mirrors
//! none of them, and offers to fetch one from its author's own repository on the
//! operator's behalf — pinned to a commit and verified by SHA-256, the same
//! posture and for the same reasons as [`super::fetch`].  A file already in the
//! folder is **never** overwritten: it may be the operator's own, and a monitor
//! is exactly the sort of thing a hobbyist patches.

use super::console::RomImage;
use std::path::{Path, PathBuf};

/// Name of the folder ROM files live in, inside `CPM/`.
///
/// Its own folder rather than the images folder, because a ROM is not a disk and
/// the images folder is enumerated: `image::available_images` filters by name,
/// so a `.hex` there would be skipped today and the question would be reopened
/// by the next format we accept.  A reader looking for where a ROM goes should
/// also not have to be told it lives among the disks.
pub const ROMS_DIR: &str = "roms";

/// What `cpm_boot_rom` holds to mean "no monitor ROM".
pub const ROM_OFF: &str = "off";

/// A ROM file: what to fetch, where its bytes belong, and how to know they are
/// the right ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RomFile {
    /// The name it takes in the ROMs folder, and the name it has upstream.
    pub file: &'static str,
    /// The address window its contents must fall inside, inclusive.
    ///
    /// A guard, not a load address — an Intel HEX file says where its own bytes
    /// go, and this is what stops a file that says somewhere else.  A monitor
    /// dropped in under the right name but assembled for a different address
    /// would otherwise be written over the guest's own memory, which presents as
    /// a disk that boots and then behaves impossibly.
    pub span: (u16, u16),
    /// Size in bytes, as the pinned URL serves it.
    pub bytes: u64,
    /// SHA-256 of those bytes.  Checked on download only — see the module note
    /// about a file the operator already has.
    pub sha256: &'static str,
    /// Where it is fetched from, pinned to a commit.
    pub url: &'static str,
}

/// One selectable setting of `cpm_boot_rom`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RomChoice {
    /// Canonical config value (stored in `egateway.conf`).
    pub key: &'static str,
    /// One line shown next to the selection in every UI.
    pub description: &'static str,
    /// A few columns for a status row, where the description will not fit.
    ///
    /// A second, narrower label rather than a truncation of the first, for the
    /// same reason [`super::cpu::cpu_short`] exists: a 40-column screen loses
    /// the *end* of a value silently, and the end is where "CUTER" is.
    pub short: &'static str,
    /// `None` for [`ROM_OFF`], the way [`super::uart::ModemAccess::Off`] is a
    /// profile rather than an absence of one.
    pub rom: Option<RomFile>,
}

/// Every selectable monitor ROM, in UI display order.  Single source of truth
/// for config validation and all three configuration screens.
///
/// # CUTER for VDM-1, and why a stub could not do it
///
/// `DISK11.DSK` in Hansel's Altair collection is a CP/M built for a machine
/// with a Processor Technology VDM-1 and the CUTER monitor loaded at `C000`.
/// Everything below was **measured** out of the disk and out of the ROM, not
/// read off a manual:
///
/// * It **tests for the ROM and says so.**  Its loader is
///   `LDA C000 / CPI 7Fh / RZ`, and when the byte is wrong it prints "This
///   version of CP/M requires CUTER for VDM-1 to be present at C000h." through
///   the 88-2SIO and then `JMP`s to itself for ever.  A disk that names its own
///   missing dependency is a kinder failure than most, and the gateway used to
///   answer it with nothing.
/// * **The signature byte alone is worthless.**  Poking `7F` into `C000` gets
///   past the gate and the guest then goes quiet with no output and a blank
///   screen — it wants the routines, not the sentinel.
/// * It calls **three** entries, not one: `C019` (character out, character in
///   `B`, `A` selecting the pseudo-port — the same contract the synthesised stub
///   honours), `C0F9`, and `C1D7`.  `C0F9` in the real ROM is
///   `LXI H,CC00 / MVI M,A0h` — the VDM-1 screen clear, painting straight into
///   the window [`super::vdm`] renders and `/vdm` serves.
/// * It **patches six bytes inside the ROM's own image** before using it —
///   `C03F`, `C040`, `C042`, `C045`, `C1EE` (an opcode) and `C215` (a `C9`,
///   shorting a routine out) — every one of which falls inside the real file.
///   So this is loaded into RAM and left writable, and no synthesised stub can
///   serve it: the disk depends on the original's instruction layout.
///
/// The upstream note agrees on the last point in its own words — "the memory at
/// C000h must be writable because CP/M needs to slightly patch CUTER" — which
/// is a cross-check, not the source.
pub const ROM_CHOICES: &[RomChoice] = &[
    RomChoice { key: ROM_OFF, description: "Off - no monitor ROM", short: "off", rom: None },
    RomChoice {
        key: "cuter_vdm",
        // 44 columns.  Fits the 80-column screens with room to spare and is
        // checked against the narrow ones by `test_every_rom_label_fits`.
        description: "CUTER for VDM-1 at 0xC000 (needs vdmcuter.hex)",
        short: "CUTER",
        rom: Some(RomFile {
            file: "vdmcuter.hex",
            // `C000`-`C7FC`, contiguous, from the file itself.
            span: (0xC000, 0xC7FC),
            bytes: 5784,
            sha256: "864152aaf90a9605b43ecff3218326bc773f4cf09fd536e0638bbc33ce7b7413",
            url: "https://raw.githubusercontent.com/dhansel/VDM1/\
                  1adf9fd9be5c8645669735a61de79552aaa543d3/programs/HEX/vdmcuter.hex",
        }),
    },
];

/// The choice a config value names, if any.
pub fn choice_for(key: &str) -> Option<&'static RomChoice> {
    ROM_CHOICES.iter().find(|c| c.key == key)
}

/// Is this a value `cpm_boot_rom` may hold?
pub fn is_valid_rom_key(key: &str) -> bool {
    choice_for(key).is_some()
}

/// The ROM file a setting names, if it names one.
pub fn file_for(key: &str) -> Option<&'static RomFile> {
    choice_for(key).and_then(|c| c.rom.as_ref())
}

/// The narrow label for a setting, for a status row.
pub fn short_label(key: &str) -> &'static str {
    choice_for(key).map(|c| c.short).unwrap_or("off")
}

/// The folder ROM files live in.
pub fn roms_dir(cpm_base: &Path) -> PathBuf {
    cpm_base.join(ROMS_DIR)
}

/// Where a setting's file would be.
pub fn path_for(cpm_base: &Path, key: &str) -> Option<PathBuf> {
    file_for(key).map(|f| roms_dir(cpm_base).join(f.file))
}

/// Is the file this setting needs absent?
///
/// `false` for [`ROM_OFF`], which needs no file: "nothing is missing" is the
/// right answer for a setting that asks for nothing, and the alternative made
/// every screen ask whether the setting was on before it could ask this.
pub fn missing(cpm_base: &Path, key: &str) -> bool {
    path_for(cpm_base, key).map(|p| !p.exists()).unwrap_or(false)
}

/// Parse Intel HEX into placements.
///
/// Only what a monitor image uses: data records (type 0) and end-of-file (type
/// 1).  Extended-address records would move the window a `u16` cannot reach and
/// are refused rather than ignored, because ignoring one silently relocates
/// every record after it.
///
/// **Leading whitespace is skipped, and that is measured**: every line of the
/// upstream `vdmcuter.hex` begins with a space.  A parser that required `:` in
/// column one would reject the one file this catalogue names.
///
/// The per-record checksum is verified.  It is a weak check next to the SHA-256
/// on the download, but it is the only one a file the operator brought has, and
/// a hand-patched monitor with a mistyped byte is exactly the case it catches.
pub fn parse_intel_hex(text: &str) -> Result<Vec<(u16, Vec<u8>)>, String> {
    let mut out: Vec<(u16, Vec<u8>)> = Vec::new();
    let mut saw_eof = false;
    for (n, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let at = || format!("line {}", n + 1);
        let body = line.strip_prefix(':').ok_or_else(|| format!("{}: no ':'", at()))?;
        if body.len() < 10 || body.len() % 2 != 0 {
            return Err(format!("{}: truncated record", at()));
        }
        let bytes: Vec<u8> = (0..body.len() / 2)
            .map(|i| u8::from_str_radix(&body[i * 2..i * 2 + 2], 16))
            .collect::<Result<_, _>>()
            .map_err(|_| format!("{}: not hexadecimal", at()))?;
        let count = bytes[0] as usize;
        if bytes.len() != count + 5 {
            return Err(format!("{}: says {count} bytes, carries {}", at(), bytes.len() - 5));
        }
        // Two's complement of the sum of everything before it.
        let sum = bytes[..bytes.len() - 1].iter().fold(0u8, |a, b| a.wrapping_add(*b));
        let want = bytes[bytes.len() - 1];
        if want != (!sum).wrapping_add(1) {
            return Err(format!("{}: checksum {want:02X} does not match the record", at()));
        }
        let addr = u16::from_be_bytes([bytes[1], bytes[2]]);
        match bytes[3] {
            0 => out.push((addr, bytes[4..4 + count].to_vec())),
            1 => {
                saw_eof = true;
                break;
            }
            other => return Err(format!("{}: record type {other:02X} is not supported", at())),
        }
    }
    if !saw_eof {
        return Err("no end-of-file record — the file is incomplete".to_string());
    }
    if out.is_empty() {
        return Err("no data records".to_string());
    }
    Ok(out)
}

/// Turn a file's bytes into placements, refusing anything outside the window.
///
/// Intel HEX when it starts with a `:` (after whitespace), raw binary otherwise
/// — loaded at the window's start, since a `.bin` says nothing about where it
/// belongs.  Both are accepted because the one file this catalogue names is HEX
/// and every other copy in circulation is not.
pub fn image_from_bytes(f: &RomFile, raw: &[u8]) -> Result<RomImage, String> {
    let chunks = match std::str::from_utf8(raw).map(|t| (t.trim_start().starts_with(':'), t)) {
        Ok((true, text)) => parse_intel_hex(text)?,
        _ => {
            if raw.is_empty() {
                return Err("the file is empty".to_string());
            }
            vec![(f.span.0, raw.to_vec())]
        }
    };
    let (lo, hi) = f.span;
    for (at, bytes) in &chunks {
        let last = at
            .checked_add(bytes.len().saturating_sub(1) as u16)
            .ok_or_else(|| format!("a record at {at:04X} runs past the top of memory"))?;
        if *at < lo || last > hi {
            return Err(format!(
                "it puts bytes at {at:04X}-{last:04X}, outside {lo:04X}-{hi:04X} — \
                 this is not the ROM this setting expects"
            ));
        }
    }
    Ok(RomImage { chunks })
}

/// Read the ROM a setting names, ready to place in a machine.
///
/// `Ok(None)` means the setting asks for nothing.  An `Err` names what is wrong
/// in terms an operator can act on, because the caller's job is to say it on a
/// boot banner: a disk that needs a ROM and does not get one goes quiet, and
/// "quiet" is the one outcome that must never be left to look like a bad disk.
pub fn load(cpm_base: &Path, key: &str) -> Result<Option<RomImage>, String> {
    let Some(f) = file_for(key) else {
        // Unknown keys land here too, and that is right: `apply_config_key`
        // refuses them, so a value this does not recognise reached the file by
        // hand and behaving as `off` is the same answer the machine would give.
        return Ok(None);
    };
    let path = roms_dir(cpm_base).join(f.file);
    let raw = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    image_from_bytes(f, &raw).map(Some).map_err(|why| format!("{}: {why}", f.file))
}

/// Fetch the file a setting names, if it is not already there.
///
/// Returns what happened, for a screen to print.  **Never overwrites**, and
/// verifies size and SHA-256 before anything is written — see [`super::fetch`],
/// whose rules these are.
pub fn download(cpm_base: &Path, key: &str) -> Result<String, String> {
    let Some(f) = file_for(key) else {
        return Err("that setting needs no ROM file".to_string());
    };
    let dir = roms_dir(cpm_base);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let done = dir.join(f.file);
    if done.exists() {
        return Ok(format!("{} is already here — left untouched", f.file));
    }
    let body = super::fetch::get(f.url, "EthernetGateway (CP/M monitor ROM)", 1 << 20)?;
    if body.len() as u64 != f.bytes {
        return Err(format!("expected {} bytes, got {}", f.bytes, body.len()));
    }
    let got = super::fetch::sha256(&body);
    if got != f.sha256 {
        return Err(format!("checksum mismatch (got {}…)", &got[..12]));
    }
    // Parsed before it is kept, so a file that cannot be placed is never
    // written: the checksum says the bytes are the ones we tested, and this says
    // this build can still use them.
    image_from_bytes(f, &body)?;
    let tmp = dir.join(format!("{}.part", f.file));
    std::fs::write(&tmp, &body)
        .and_then(|_| std::fs::rename(&tmp, &done))
        .map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("{e}")
        })?;
    Ok(format!("fetched {} ({} bytes)", f.file, f.bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A record, built the way the format says, so the fixtures below are
    /// checksummed rather than transcribed.
    fn record(addr: u16, typ: u8, data: &[u8]) -> String {
        let mut bytes = vec![data.len() as u8, (addr >> 8) as u8, addr as u8, typ];
        bytes.extend_from_slice(data);
        let sum = bytes.iter().fold(0u8, |a, b| a.wrapping_add(*b));
        bytes.push((!sum).wrapping_add(1));
        format!(":{}", bytes.iter().map(|b| format!("{b:02X}")).collect::<String>())
    }

    fn eof() -> String {
        record(0, 1, &[])
    }

    /// The catalogue is what three UIs enumerate, so its shape is pinned.
    #[test]
    fn test_the_catalogue_is_well_formed() {
        assert_eq!(ROM_CHOICES[0].key, ROM_OFF, "off is first, as in every other choice list");
        assert!(ROM_CHOICES[0].rom.is_none());
        let mut keys: Vec<&str> = ROM_CHOICES.iter().map(|c| c.key).collect();
        let before = keys.len();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), before, "two choices share a key");
        for c in ROM_CHOICES.iter().filter_map(|c| c.rom.as_ref()) {
            assert_eq!(c.sha256.len(), 64, "{}: not a SHA-256", c.file);
            assert!(c.url.starts_with("https://"), "{}: {}", c.file, c.url);
            assert!(c.span.0 < c.span.1, "{}: empty window", c.file);
            assert!(!c.url.contains("/master/"), "{}: pin a commit, not a branch", c.file);
        }
    }

    /// Every label has to fit the narrowest screen that shows it — the same rule
    /// as the machine and printer labels, and for the same measured reason: a
    /// PETSCII screen does not wrap a chosen value, it loses the end of it.
    #[test]
    fn test_every_rom_label_fits() {
        for c in ROM_CHOICES {
            assert!(
                c.description.len() <= 46,
                "{}: {} columns is too wide for the narrow screens",
                c.key,
                c.description.len()
            );
            // The telnet boot screen draws `O  ROM: <short>` beside another key
            // on one 40-column row, which leaves this much.
            assert!(
                c.short.len() <= 8,
                "{}: the short label {:?} does not fit the shared status row",
                c.key,
                c.short
            );
        }
        // Two settings that read the same on a C64 are worse than a lost tail —
        // the `cpm_printer` lesson.
        let mut shorts: Vec<&str> = ROM_CHOICES.iter().map(|c| c.short).collect();
        shorts.sort_unstable();
        let before = shorts.len();
        shorts.dedup();
        assert_eq!(shorts.len(), before, "two ROM choices read identically on a narrow screen");
        assert_eq!(short_label("nonsense"), "off", "an unknown value reads as what it does");
    }

    /// `off` is a setting, not a failure: it loads nothing and reports nothing
    /// missing.
    #[test]
    fn test_off_asks_for_nothing() {
        let dir = std::env::temp_dir().join("xmodem_rom_off");
        assert!(is_valid_rom_key(ROM_OFF));
        assert_eq!(load(&dir, ROM_OFF).unwrap(), None);
        assert!(!missing(&dir, ROM_OFF), "off cannot be missing a file");
        assert_eq!(path_for(&dir, ROM_OFF), None);
        assert!(download(&dir, ROM_OFF).is_err(), "there is nothing to fetch");
    }

    /// An unrecognised value behaves as `off` rather than failing a boot.
    #[test]
    fn test_an_unknown_key_is_not_a_rom() {
        assert!(!is_valid_rom_key("cuter"));
        assert_eq!(load(Path::new("/nonexistent"), "cuter").unwrap(), None);
    }

    /// The parser, against a file built to the specification.
    #[test]
    fn test_intel_hex_round_trips_placements() {
        let text = format!(
            "{}\n{}\n{}\n",
            record(0xC000, 0, &[0x7F, 0xC3, 0xD7, 0xC1]),
            record(0xC010, 0, &[0x3A, 0x07, 0xC8]),
            eof()
        );
        let got = parse_intel_hex(&text).expect("parses");
        assert_eq!(
            got,
            vec![(0xC000, vec![0x7F, 0xC3, 0xD7, 0xC1]), (0xC010, vec![0x3A, 0x07, 0xC8])]
        );
    }

    /// **Every line of the real file starts with a space.**  Measured on
    /// `vdmcuter.hex`, and the reason the parser trims rather than requiring
    /// `:` in column one.
    #[test]
    fn test_leading_whitespace_is_not_a_malformed_file() {
        let text = format!(" {}\n \t{}\n", record(0xC000, 0, &[0x7F]), eof());
        assert_eq!(parse_intel_hex(&text).unwrap(), vec![(0xC000, vec![0x7F])]);
    }

    /// Each way a file can be wrong is refused with a reason, and the reasons
    /// are distinct — an operator reading one on a boot banner has to be able to
    /// tell "I have the wrong file" from "the file is damaged".
    #[test]
    fn test_a_malformed_file_is_refused_and_says_why() {
        let good = record(0xC000, 0, &[0x7F, 0xC3]);
        // A flipped byte in the data, checksum left alone.
        let mut bad = good.clone().into_bytes();
        bad[10] = if bad[10] == b'0' { b'1' } else { b'0' };
        let bad = String::from_utf8(bad).unwrap();
        for (text, expect) in [
            (format!("{good}\n"), "no end-of-file record"),
            (format!("{bad}\n{}\n", eof()), "checksum"),
            (format!("{}\n", eof()), "no data records"),
            ("nonsense\n".to_string(), "no ':'"),
            (":10C0\n".to_string(), "truncated record"),
            (format!("{}\n{}\n", record(0xC000, 4, &[0, 0]), eof()), "record type 04"),
        ] {
            let err = parse_intel_hex(&text).expect_err(&format!("{text:?} must be refused"));
            assert!(err.contains(expect), "{text:?} gave {err:?}, wanted {expect:?}");
        }
    }

    /// **The window is a guard, and it is the one that matters.**  A monitor
    /// assembled for somewhere else, dropped in under the expected name, would
    /// otherwise be written over the guest's own memory — which presents as a
    /// disk that boots and then behaves impossibly.
    #[test]
    fn test_bytes_outside_the_window_are_refused() {
        let f = file_for("cuter_vdm").expect("catalogue has it");
        let inside = format!("{}\n{}\n", record(0xC000, 0, &[0x7F]), eof());
        assert!(image_from_bytes(f, inside.as_bytes()).is_ok());
        for (addr, what) in [(0x0000u16, "page zero"), (0xBFFF, "just below"), (0xC7FC, "the end")]
        {
            let text = format!("{}\n{}\n", record(addr, 0, &[0x11, 0x22]), eof());
            let got = image_from_bytes(f, text.as_bytes());
            if addr == 0xC7FC {
                // Two bytes at the last address run one past it.
                assert!(got.is_err(), "{what}: a record may not overrun the window");
            } else {
                assert!(got.is_err(), "{what}: outside the window must be refused");
            }
        }
        // And the honest edge: the last byte exactly at the top is fine.
        let text = format!("{}\n{}\n", record(0xC7FC, 0, &[0x11]), eof());
        assert!(image_from_bytes(f, text.as_bytes()).is_ok(), "the top byte is inside");
    }

    /// A raw binary is taken at the window's start, and one too long for the
    /// window is refused rather than truncated.
    #[test]
    fn test_a_raw_binary_loads_at_the_window_start() {
        let f = file_for("cuter_vdm").unwrap();
        let img = image_from_bytes(f, &[0x7F, 0xC3, 0xD7]).expect("raw bytes load");
        assert_eq!(img.chunks, vec![(0xC000, vec![0x7F, 0xC3, 0xD7])]);
        let too_long = vec![0u8; 0x900];
        assert!(image_from_bytes(f, &too_long).is_err(), "past the window");
        assert!(image_from_bytes(f, &[]).is_err(), "an empty file says nothing");
    }

    /// A file that is not there is an error naming the path, not a silent `off`
    /// — the whole point being that a boot screen can say what to do about it.
    #[test]
    fn test_a_missing_file_names_itself() {
        let dir = std::env::temp_dir().join("xmodem_rom_absent");
        assert!(missing(&dir, "cuter_vdm"));
        let err = load(&dir, "cuter_vdm").expect_err("cannot load what is not there");
        assert!(err.contains("vdmcuter.hex"), "{err}");
        assert_eq!(
            path_for(&dir, "cuter_vdm").unwrap(),
            dir.join(ROMS_DIR).join("vdmcuter.hex")
        );
    }

    /// The signature byte the disks check is the first byte of the file, so a
    /// catalogue entry whose window starts elsewhere would place it wrong.
    #[test]
    fn test_the_cuter_window_starts_where_the_disks_look() {
        let f = file_for("cuter_vdm").unwrap();
        assert_eq!(f.span.0, 0xC000, "DISK11 reads C000 for the 7F signature");
        assert_eq!(
            f.span.1 - f.span.0 + 1,
            0x7FD,
            "the real file is 2045 contiguous bytes, C000-C7FC"
        );
    }
}
