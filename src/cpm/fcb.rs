//! CP/M 2.2 File Control Block (FCB) parsing + 8.3 filename helpers.
//!
//! A BDOS file call passes the address of a 36-byte FCB in `DE`.  We read
//! the relevant fields, act on the host file it names (jailed under a
//! `CPM/<drive>/` directory — see [`super::fs`]), and write back the
//! updated position fields.  Everything here is pure logic so it is
//! unit-testable without a live CPU or session.
//!
//! FCB layout (36 bytes):
//! ```text
//!  0     drive code (0 = default/current, 1 = A: .. 16 = P:)
//!  1..9  filename  (8 bytes, space-padded; high bit of each = attribute)
//!  9..12 filetype  (3 bytes, space-padded; high bits = R/O, SYS, archive)
//! 12     EX  extent number, low
//! 13     S1  reserved
//! 14     S2  extent number, high
//! 15     RC  record count in the current extent (0..128)
//! 16..32 D0..D15  allocation map (unused by our directory-backed model)
//! 32     CR  current record within the extent (0..127)
//! 33..36 R0,R1,R2  random record number (little-endian, R2 = overflow)
//! ```

/// Size of a CP/M FCB in bytes.
pub const FCB_SIZE: usize = 36;

/// A parsed CP/M File Control Block (the fields we act on).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fcb {
    /// Byte 0: 0 = default/current drive, 1 = A:, 2 = B:, …
    pub drive: u8,
    /// Bytes 1..9, high bit masked off, space-padded.
    pub name: [u8; 8],
    /// Bytes 9..12, high bit masked off, space-padded.
    pub ext: [u8; 3],
    /// Extent number, low (byte 12).
    pub ex: u8,
    /// Extent number, high (byte 14).
    pub s2: u8,
    /// Current record within the extent (byte 32).
    pub cr: u8,
    /// Record count in the current extent (byte 15).
    pub rc: u8,
    /// Random record number bytes R0,R1,R2 (bytes 33..36).
    pub r: [u8; 3],
}

impl Fcb {
    /// Parse an FCB from a 36-byte (or longer) slice.  Panics only in debug
    /// if the slice is too short; callers always pass a 36-byte read.
    pub fn from_bytes(b: &[u8]) -> Fcb {
        debug_assert!(b.len() >= FCB_SIZE);
        let mut name = [b' '; 8];
        let mut ext = [b' '; 3];
        for (slot, &src) in name.iter_mut().zip(&b[1..9]) {
            *slot = src & 0x7F; // strip attribute bit
        }
        for (slot, &src) in ext.iter_mut().zip(&b[9..12]) {
            *slot = src & 0x7F;
        }
        Fcb {
            drive: b[0],
            name,
            ext,
            ex: b[12],
            s2: b[14],
            cr: b[32],
            rc: b[15],
            r: [b[33], b[34], b[35]],
        }
    }

    /// Sequential record index = full-extent × 128 + current record.
    /// The full extent combines S2 (high) and EX (low, 5 bits).
    pub fn seq_record(&self) -> u32 {
        let extent = ((self.s2 as u32 & 0x3F) << 5) | (self.ex as u32 & 0x1F);
        extent * 128 + self.cr as u32
    }

    /// The 24-bit random record number (R0 low .. R2 overflow).
    pub fn random_record(&self) -> u32 {
        (self.r[0] as u32) | ((self.r[1] as u32) << 8) | ((self.r[2] as u32) << 16)
    }

    /// Advance the sequential position by one record, propagating the
    /// current-record → extent → module carries (CR 0..127, EX 0..31).
    pub fn advance_record(&mut self) {
        if self.cr < 127 {
            self.cr += 1;
        } else {
            self.cr = 0;
            if self.ex < 31 {
                self.ex += 1;
            } else {
                self.ex = 0;
                self.s2 = self.s2.wrapping_add(1);
            }
        }
    }

    /// Set the sequential position (EX/S2/CR) from an absolute record
    /// index — used to seek after a random operation or on open.
    pub fn set_seq_record(&mut self, record: u32) {
        self.cr = (record % 128) as u8;
        let extent = record / 128;
        self.ex = (extent & 0x1F) as u8;
        self.s2 = ((extent >> 5) & 0x3F) as u8;
    }

    /// Write the mutable position fields (EX,S2,CR,RC) back into a 36-byte
    /// FCB image so the guest sees the advanced position.
    pub fn store_position(&self, b: &mut [u8]) {
        debug_assert!(b.len() >= FCB_SIZE);
        b[12] = self.ex;
        b[14] = self.s2;
        b[15] = self.rc;
        b[32] = self.cr;
    }

    /// Does this FCB's (possibly `?`-wildcarded) name/ext match a concrete
    /// 8.3 candidate?  A `?` in any position matches any character; used by
    /// the search-first/next directory walk.
    pub fn matches(&self, cand_name: &[u8; 8], cand_ext: &[u8; 3]) -> bool {
        for (&pat, &c) in self.name.iter().zip(cand_name.iter()) {
            if pat != b'?' && pat != c {
                return false;
            }
        }
        for (&pat, &c) in self.ext.iter().zip(cand_ext.iter()) {
            if pat != b'?' && pat != c {
                return false;
            }
        }
        true
    }

    /// Does a 32-byte directory entry answer BDOS Search First/Next (17/18)
    /// for this FCB, on a disk whose extent mask is `exm`?
    ///
    /// The name is only part of it. CP/M 2.2's `search` compares fifteen bytes
    /// and the extent is one of them, through `compext`: the low `exm` bits are
    /// masked off both sides, because one directory entry holds `exm + 1`
    /// logical extents, and what is left must agree in its low five bits. So a
    /// search for extent 0 finds the **first entry of each file** and not every
    /// entry -- which is how a wildcard copy sees a 40 KB file once rather than
    /// three times. `func17` also zeroes the module number (S2) unless EX is
    /// `?` (`clrmodnum`), and S2 is then compared like a name byte, high bit
    /// ignored. S1 (byte 13) is never compared. The drive byte is the caller's:
    /// it selects the drive, and a `?` there disables matching altogether.
    pub fn search_matches_entry(&self, entry: &[u8; 32], exm: u8) -> bool {
        let mut name = [0u8; 8];
        let mut ext = [0u8; 3];
        for (slot, &b) in name.iter_mut().zip(&entry[1..9]) {
            *slot = b & 0x7F;
        }
        for (slot, &b) in ext.iter_mut().zip(&entry[9..12]) {
            *slot = b & 0x7F;
        }
        if !self.matches(&name, &ext) {
            return false;
        }
        if self.ex != b'?' && (self.ex & !exm) & 0x1F != (entry[12] & !exm) & 0x1F {
            return false;
        }
        let s2 = if self.ex == b'?' { self.s2 } else { 0 };
        s2 == b'?' || s2.wrapping_sub(entry[14]) & 0x7F == 0
    }
}

/// Is `c` a legal CP/M 8.3 filename character?  Printable ASCII excluding
/// space and the CCP/FCB delimiters.  (`?` and `*` are wildcards, handled
/// by the caller, not stored in a concrete name.)
pub fn is_valid_8_3_char(c: u8) -> bool {
    c.is_ascii_graphic() && !b"<>.,;:=?*[]|/\\".contains(&c)
}

/// Split a host filename into a space-padded, uppercased 8.3 pair, or
/// `None` if it is not a legal CP/M 8.3 name (too long, empty, bad chars,
/// more than one dot).  Host files that fail this are simply invisible to
/// CP/M programs — a documented deviation.
pub fn split_8_3(filename: &str) -> Option<([u8; 8], [u8; 3])> {
    let upper = filename.to_ascii_uppercase();
    let (name_part, ext_part) = match upper.rsplit_once('.') {
        // `FOO.` is FOO with a blank extension -- legal CCP syntax (`TYPE
        // FOO.`, `REN NEW.=OLD.`), so it is accepted here.  A dot *inside an
        // FCB's name field* is a different thing and is refused where a host
        // path is built, in `CpmFs::resolve_name`.
        Some((n, e)) => (n, e),
        None => (upper.as_str(), ""),
    };
    if name_part.is_empty() || name_part.len() > 8 || ext_part.len() > 3 {
        return None;
    }
    let mut name = [b' '; 8];
    let mut ext = [b' '; 3];
    for (i, c) in name_part.bytes().enumerate() {
        if !is_valid_8_3_char(c) {
            return None;
        }
        name[i] = c;
    }
    for (i, c) in ext_part.bytes().enumerate() {
        if !is_valid_8_3_char(c) {
            return None;
        }
        ext[i] = c;
    }
    Some((name, ext))
}

/// Is `name` (the part before any dot, uppercased) a name Windows maps to a
/// device rather than a file?  `CPM\A\COM1.TXT` opens the serial port, not
/// a file in the jail -- possibly the very port the gateway's serial manager
/// holds.  Refused by `CpmFs::resolve_name` on every platform, so one rule is
/// tested everywhere and a disk of files moves between hosts unchanged -- and
/// only there, because inside a mounted image no host path is ever opened.
///
/// Case-folded here, not by the caller: Windows matches `com1` as readily as
/// `COM1`, and an FCB carries whatever case the guest put in it.
pub fn is_host_device_name(name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    let name = name.as_str();
    matches!(name, "CON" | "PRN" | "AUX" | "NUL")
        || ["COM", "LPT"].iter().any(|p| {
            name.strip_prefix(p)
                .is_some_and(|d| d.len() == 1 && (b'1'..=b'9').contains(&d.as_bytes()[0]))
        })
}

/// Parse an *ambiguous* filename (a CCP-style spec that may contain the
/// `*` and `?` wildcards, e.g. `*.TXT`, `FOO?.*`) into a space-padded 8.3
/// name/ext pair where `*` has been expanded to `?` across the rest of its
/// field.  Returns `None` for an illegal spec.  Used by built-ins like
/// `ERA` that take a wildcard operand.
pub fn parse_afn(spec: &str) -> Option<([u8; 8], [u8; 3])> {
    let upper = spec.trim().to_ascii_uppercase();
    if upper.is_empty() {
        return None;
    }
    let (name_part, ext_part) = match upper.rsplit_once('.') {
        Some((n, e)) => (n, e),
        None => (upper.as_str(), ""),
    };
    let name = expand_afn_field::<8>(name_part)?;
    let ext = expand_afn_field::<3>(ext_part)?;
    Some((name, ext))
}

/// Parse one CCP command-line token into a default-FCB triple the way the
/// CCP does when it lays down the two default FCBs before running a program:
/// an optional single-letter drive prefix (`B:FOO.TXT`) becomes the FCB
/// drive byte (0 = default/current, 1 = A:, 2 = B:, …), and the remainder is
/// parsed as an ambiguous 8.3 name/ext (wildcards allowed).  An empty or
/// unparseable token yields an all-blank name on the default drive — exactly
/// what the CCP leaves in an unused default FCB.
pub fn parse_command_fcb(token: &str) -> (u8, [u8; 8], [u8; 3]) {
    let (drive, rest) = split_drive_prefix(token);
    let (name, ext) = parse_afn(rest).unwrap_or(([b' '; 8], [b' '; 3]));
    (drive, name, ext)
}

/// Split an optional single-letter drive prefix off a CCP token, returning
/// the FCB drive byte (0 = default/current, 1 = A:, …) and the remainder.
/// Shared by `parse_command_fcb` and `parse_dir_operand` so the two cannot
/// disagree about what `B:` means.
fn split_drive_prefix(token: &str) -> (u8, &str) {
    match token.split_once(':') {
        Some((d, r)) if d.len() == 1 && d.as_bytes()[0].is_ascii_alphabetic() => {
            (d.as_bytes()[0].to_ascii_uppercase() - b'A' + 1, r)
        }
        _ => (0u8, token),
    }
}

/// Parse the optional operand of the CCP's `DIR`, which accepts all four of
/// `DIR`, `DIR afn`, `DIR d:` and `DIR d:afn`.
///
/// An **absent or bare-drive** operand means "every file", which is the
/// all-`?` pattern — deliberately *not* the all-blank one
/// `parse_command_fcb` leaves in an unused FCB, since blanks match nothing.
///
/// `None` means the operand was present but is not a legal filespec, and the
/// caller must say so. Distinguishing that from "empty" is the whole point:
/// the previous `DIR` ignored its operand entirely, so `DIR *.COM` listed
/// every file on the drive and looked like it had filtered. Silently listing
/// everything for a *malformed* pattern would reintroduce exactly that.
pub fn parse_dir_operand(operand: &str) -> Option<(u8, [u8; 8], [u8; 3])> {
    let (drive, rest) = split_drive_prefix(operand.trim());
    if rest.trim().is_empty() {
        return Some((drive, [b'?'; 8], [b'?'; 3]));
    }
    let (name, ext) = parse_afn(rest)?;
    Some((drive, name, ext))
}

/// Expand one field of an ambiguous filename into `WIDTH` space-padded
/// bytes: `*` fills the remainder of the field with `?`; `?` passes
/// through; other characters must be legal 8.3 characters.  Too-long
/// fields (without a `*`) are rejected.
fn expand_afn_field<const WIDTH: usize>(s: &str) -> Option<[u8; WIDTH]> {
    let mut out = [b' '; WIDTH];
    for (i, c) in s.bytes().enumerate() {
        if c == b'*' {
            for slot in out.iter_mut().skip(i) {
                *slot = b'?';
            }
            return Some(out);
        }
        if i >= WIDTH {
            return None; // too long and no '*' to absorb it
        }
        if c == b'?' {
            out[i] = b'?';
        } else if is_valid_8_3_char(c) {
            out[i] = c;
        } else {
            return None;
        }
    }
    Some(out)
}

/// Format a space-padded 8.3 pair as `NAME.EXT` (or `NAME` if the
/// extension is blank).
pub fn format_8_3(name: &[u8; 8], ext: &[u8; 3]) -> String {
    let n: String = name
        .iter()
        .take_while(|&&c| c != b' ')
        .map(|&c| c as char)
        .collect();
    let e: String = ext
        .iter()
        .take_while(|&&c| c != b' ')
        .map(|&c| c as char)
        .collect();
    if e.is_empty() {
        n
    } else {
        format!("{}.{}", n, e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn padded(n: &str, e: &str) -> ([u8; 8], [u8; 3]) {
        split_8_3(&if e.is_empty() {
            n.to_string()
        } else {
            format!("{}.{}", n, e)
        })
        .unwrap()
    }

    /// The search comparison, field by field, as CP/M 2.2's `search` and
    /// `compext` make it: the extent through the mask, S2 zeroed unless EX is
    /// `?`, S1 ignored, attribute bits ignored on both sides.
    #[test]
    fn test_search_matches_entry_follows_bdos22() {
        let entry = |ex: u8, s1: u8, s2: u8| -> [u8; 32] {
            let mut e = [0u8; 32];
            e[1..12].copy_from_slice(b"FILE    DAT");
            e[9] |= 0x80; // R/O: an attribute, not part of the name
            e[12] = ex;
            e[13] = s1;
            e[14] = s2;
            e
        };
        let mut raw = [0u8; FCB_SIZE];
        raw[1..12].copy_from_slice(b"FILE    DAT");
        let fcb = |ex: u8, s2: u8| {
            let mut r = raw;
            r[12] = ex;
            r[14] = s2;
            Fcb::from_bytes(&r)
        };
        assert!(fcb(0, 0).search_matches_entry(&entry(1, 0, 0), 1), "EXM 1: 0 and 1 are one entry");
        assert!(!fcb(0, 0).search_matches_entry(&entry(1, 0, 0), 0), "EXM 0: they are not");
        assert!(!fcb(0, 0).search_matches_entry(&entry(2, 0, 0), 1));
        assert!(fcb(0, 0).search_matches_entry(&entry(0, 0x55, 0), 0), "S1 is never compared");
        assert!(!fcb(0, 0).search_matches_entry(&entry(0, 0, 1), 0), "S2 is, and is zeroed first");
        assert!(fcb(0, 7).search_matches_entry(&entry(0, 0, 0), 0), "so the caller's S2 is ignored");
        assert!(fcb(b'?', 1).search_matches_entry(&entry(9, 0, 1), 0), "unless EX is ?");
        assert!(!fcb(b'?', 0).search_matches_entry(&entry(9, 0, 1), 0), "when S2 counts");
        let mut other = entry(0, 0, 0);
        other[1] = b'G';
        assert!(!fcb(0, 0).search_matches_entry(&other, 0), "and the name still matters");
    }

    #[test]
    fn test_parse_command_fcb() {
        // Drive prefix -> drive byte (A:=1, B:=2, …), name/ext parsed.
        let (d, n, e) = parse_command_fcb("B:FOO.TXT");
        assert_eq!(d, 2);
        assert_eq!(&n, b"FOO     ");
        assert_eq!(&e, b"TXT");
        // No prefix -> default drive (0).
        let (d, n, e) = parse_command_fcb("bar.dat");
        assert_eq!(d, 0);
        assert_eq!(&n, b"BAR     ");
        assert_eq!(&e, b"DAT");
        // Wildcards are allowed in a default FCB (the CCP fills them).
        let (d, n, e) = parse_command_fcb("*.COM");
        assert_eq!(d, 0);
        assert_eq!(&n, b"????????");
        assert_eq!(&e, b"COM");
        // Empty / unparseable token -> all-blank name on the default drive.
        let (d, n, e) = parse_command_fcb("");
        assert_eq!((d, n, e), (0, [b' '; 8], [b' '; 3]));
    }

    #[test]
    fn test_split_and_format_roundtrip() {
        let (n, e) = split_8_3("pip.com").unwrap();
        assert_eq!(&n, b"PIP     ");
        assert_eq!(&e, b"COM");
        assert_eq!(format_8_3(&n, &e), "PIP.COM");
    }

    #[test]
    fn test_split_no_extension() {
        let (n, e) = split_8_3("readme").unwrap();
        assert_eq!(&n, b"README  ");
        assert_eq!(&e, b"   ");
        assert_eq!(format_8_3(&n, &e), "README");
    }

    #[test]
    fn test_split_rejects_invalid() {
        assert!(split_8_3("").is_none());
        assert!(split_8_3("toolongname.com").is_none()); // >8
        assert!(split_8_3("a.tttt").is_none()); // ext >3
        assert!(split_8_3("a b.c").is_none()); // space not allowed
        assert!(split_8_3("a*.c").is_none()); // wildcard not a concrete name
        assert!(split_8_3(".com").is_none()); // empty name
    }

    /// `FOO.` is CCP syntax for FOO with a blank extension -- `TYPE FOO.`,
    /// `REN NEW.=OLD.`, `SAVE 1 FOO.` all use it -- so the parser accepts it.
    /// Refusing it here broke all three builtins; the host-side refusal lives
    /// in `CpmFs::resolve_name`, on the FCB's own bytes.
    #[test]
    fn test_split_8_3_accepts_the_ccp_blank_extension() {
        assert_eq!(split_8_3("FOO."), split_8_3("FOO"));
        assert!(split_8_3("COM1.TXT").is_some(), "device names are a host rule, not a CP/M one");
    }

    #[test]
    fn test_host_device_names() {
        for bad in ["CON", "PRN", "AUX", "NUL", "COM1", "COM9", "LPT1", "LPT9", "com1", "Con", "lpt3"] {
            assert!(is_host_device_name(bad), "{bad}");
        }
        for ok in ["CONFIG", "COM", "COM0", "COM10", "LPT", "AUXFILE", "NULL", "CON1"] {
            assert!(!is_host_device_name(ok), "{ok}");
        }
    }

    #[test]
    fn test_parse_afn_wildcards() {
        let (n, e) = parse_afn("*.TXT").unwrap();
        assert_eq!(&n, b"????????");
        assert_eq!(&e, b"TXT");
        // '*' only expands within its own field: FOO? stays literal.
        let (n, e) = parse_afn("FOO?.*").unwrap();
        assert_eq!(&n, b"FOO?    ");
        assert_eq!(&e, b"???");
        // 'FOO*' expands the name field.
        let (n, e) = parse_afn("FOO*.TXT").unwrap();
        assert_eq!(&n, b"FOO?????");
        assert_eq!(&e, b"TXT");
        let (n, e) = parse_afn("*.*").unwrap();
        assert_eq!(&n, b"????????");
        assert_eq!(&e, b"???");
        // Concrete name expands with trailing spaces.
        let (n, e) = parse_afn("PIP.COM").unwrap();
        assert_eq!(&n, b"PIP     ");
        assert_eq!(&e, b"COM");
        // Illegal.
        assert!(parse_afn("").is_none());
        assert!(parse_afn("TOOLONGNM.TXT").is_none()); // >8 no '*'
    }

    /// `DIR`'s operand: all four CP/M forms, and — the part that matters —
    /// "absent" must mean every file while "malformed" must be rejected.
    /// Conflating those is the original bug: the old `DIR` ignored its operand,
    /// so `DIR *.COM` listed the whole drive and looked like it had filtered.
    #[test]
    fn test_parse_dir_operand() {
        // `DIR` — no operand: every file on the current drive.
        let (d, n, e) = parse_dir_operand("").unwrap();
        assert_eq!(d, 0, "no drive prefix means the current drive");
        assert_eq!(&n, b"????????");
        assert_eq!(&e, b"???", "absent operand must be all-?, never all-blank");

        // `DIR B:` — bare drive: every file on B:.
        let (d, n, e) = parse_dir_operand("B:").unwrap();
        assert_eq!(d, 2);
        assert_eq!(&n, b"????????");
        assert_eq!(&e, b"???");

        // `DIR *.COM` — pattern on the current drive.
        let (d, n, e) = parse_dir_operand("*.COM").unwrap();
        assert_eq!(d, 0);
        assert_eq!(&n, b"????????");
        assert_eq!(&e, b"COM");

        // `DIR B:*.TXT` — both at once.
        let (d, n, e) = parse_dir_operand("B:*.TXT").unwrap();
        assert_eq!(d, 2);
        assert_eq!(&n, b"????????");
        assert_eq!(&e, b"TXT");

        // A concrete name is a legal (if narrow) pattern.
        let (_, n, e) = parse_dir_operand("EGT8080.COM").unwrap();
        assert_eq!(&n, b"EGT8080 ");
        assert_eq!(&e, b"COM");

        // Case-insensitive, and surrounding space is not an operand.
        let (d, n, e) = parse_dir_operand("  b:*.com  ").unwrap();
        assert_eq!(d, 2);
        assert_eq!(&n, b"????????");
        assert_eq!(&e, b"COM", "operands are upper-cased like every CP/M filespec");

        // Malformed: rejected, NOT silently widened to everything.
        assert!(
            parse_dir_operand("TOOLONGNM.TXT").is_none(),
            "an over-long name must be reported, not listed as every file"
        );
    }

    #[test]
    fn test_fcb_from_bytes_and_filename() {
        let mut b = [0u8; FCB_SIZE];
        b[0] = 1; // drive A:
        b[1..9].copy_from_slice(b"STAT    ");
        b[9..12].copy_from_slice(b"COM");
        // Set an attribute high bit on a name char; parsing must strip it.
        b[1] |= 0x80;
        let fcb = Fcb::from_bytes(&b);
        assert_eq!(fcb.drive, 1);
        assert_eq!(format_8_3(&fcb.name, &fcb.ext), "STAT.COM");
    }

    #[test]
    fn test_seq_record_math_and_advance() {
        let mut b = [0u8; FCB_SIZE];
        b[1..9].copy_from_slice(b"X       ");
        b[9..12].copy_from_slice(b"   ");
        let mut fcb = Fcb::from_bytes(&b);
        assert_eq!(fcb.seq_record(), 0);
        fcb.cr = 5;
        assert_eq!(fcb.seq_record(), 5);
        fcb.ex = 1; // extent 1 = record 128
        fcb.cr = 0;
        assert_eq!(fcb.seq_record(), 128);
        // Advance across the extent boundary (CR 127 -> next extent).
        fcb.ex = 0;
        fcb.cr = 127;
        fcb.advance_record();
        assert_eq!(fcb.cr, 0);
        assert_eq!(fcb.ex, 1);
    }

    #[test]
    fn test_set_seq_record_inverts() {
        let mut b = [0u8; FCB_SIZE];
        b[1..9].copy_from_slice(b"X       ");
        let mut fcb = Fcb::from_bytes(&b);
        for rec in [0u32, 1, 127, 128, 129, 4095, 4096, 100_000] {
            fcb.set_seq_record(rec);
            assert_eq!(fcb.seq_record(), rec, "record {rec} did not round-trip");
        }
    }

    #[test]
    fn test_random_record() {
        let mut b = [0u8; FCB_SIZE];
        b[33] = 0x34;
        b[34] = 0x12;
        b[35] = 0x00;
        let fcb = Fcb::from_bytes(&b);
        assert_eq!(fcb.random_record(), 0x1234);
    }

    #[test]
    fn test_matches_wildcards() {
        // Pattern "??????????" (all '?') matches anything.
        let mut b = [b'?'; FCB_SIZE];
        b[0] = 0;
        b[12] = 0;
        let any = Fcb::from_bytes(&b);
        let (n, e) = padded("STAT", "COM");
        assert!(any.matches(&n, &e));
        // Pattern "*.COM" -> name all '?', ext "COM".
        let mut b2 = [0u8; FCB_SIZE];
        b2[1..9].copy_from_slice(b"????????");
        b2[9..12].copy_from_slice(b"COM");
        let star_com = Fcb::from_bytes(&b2);
        assert!(star_com.matches(&n, &e));
        let (n2, e2) = padded("STAT", "TXT");
        assert!(!star_com.matches(&n2, &e2));
    }

    #[test]
    fn test_store_position() {
        let mut b = [0u8; FCB_SIZE];
        let mut fcb = Fcb::from_bytes(&b);
        fcb.ex = 3;
        fcb.s2 = 1;
        fcb.cr = 9;
        fcb.rc = 42;
        fcb.store_position(&mut b);
        assert_eq!(b[12], 3);
        assert_eq!(b[14], 1);
        assert_eq!(b[15], 42);
        assert_eq!(b[32], 9);
    }
}
