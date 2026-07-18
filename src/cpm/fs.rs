//! Directory-backed CP/M filesystem state: the current drive, the DMA
//! transfer address, and resolution of an FCB to a **jailed** host path
//! under `CPM/<drive>/`.
//!
//! Each emulated drive A:–H: is a folder inside the `CPM/` container under
//! `transfer_dir` (created on launch — see `telnet/cpm_emu.rs`).  Because a
//! resolved filename is always a validated 8.3 name (no separators, no
//! `..`) joined onto a fixed single-letter drive directory beneath the
//! container base, a guest can never escape to the host filesystem — the
//! same jail guarantee the transfer subsystem relies on.

use super::fcb::{split_8_3, Fcb};
use std::path::{Path, PathBuf};

/// Number of emulated drives (A: through H:).
pub const NUM_DRIVES: u8 = 8;

/// Default DMA (disk transfer) address in the guest — the CP/M default
/// buffer at 0x0080 (the second half of the zero page).
pub const DEFAULT_DMA: u16 = 0x0080;

/// Directory-backed CP/M filesystem: which drive is current, where the
/// DMA buffer is, and the `CPM/` container base on the host.
pub struct CpmFs {
    /// Absolute path to the `CPM/` container (holds `A`..`H`).
    base: PathBuf,
    /// Current drive, 0-based (0 = A:, 7 = H:).
    drive: u8,
    /// Current DMA transfer address in guest memory.
    dma: u16,
}

impl CpmFs {
    /// A filesystem rooted at `base` (the `CPM/` container), current drive
    /// A:, DMA at the default 0x0080.
    pub fn new(base: PathBuf) -> CpmFs {
        CpmFs {
            base,
            drive: 0,
            dma: DEFAULT_DMA,
        }
    }

    /// Current drive, 0-based (0 = A:).
    pub fn current_drive(&self) -> u8 {
        self.drive
    }

    /// Current drive as an uppercase letter (`A`..`H`).
    pub fn current_drive_letter(&self) -> char {
        (b'A' + self.drive) as char
    }

    /// Select a drive by 0-based index (BDOS 14 convention: E = 0 → A:).
    /// Returns false (and changes nothing) for an out-of-range drive.
    pub fn select(&mut self, drive0: u8) -> bool {
        if drive0 < NUM_DRIVES {
            self.drive = drive0;
            true
        } else {
            false
        }
    }

    /// Current DMA transfer address.
    pub fn dma(&self) -> u16 {
        self.dma
    }

    /// Set the DMA transfer address (BDOS 26).
    pub fn set_dma(&mut self, addr: u16) {
        self.dma = addr;
    }

    /// Map an FCB's drive byte (0 = current, 1 = A:, …) to a 0-based drive
    /// index, or `None` if it names a drive beyond H:.
    pub fn drive_index_for(&self, fcb_drive: u8) -> Option<u8> {
        match fcb_drive {
            0 => Some(self.drive),
            d if d <= NUM_DRIVES => Some(d - 1),
            _ => None,
        }
    }

    /// The host directory for a 0-based drive index.
    pub fn drive_dir(&self, drive0: u8) -> PathBuf {
        self.base.join(((b'A' + drive0) as char).to_string())
    }

    /// Resolve an FCB to a concrete, jailed host path.  Returns `None` if
    /// the drive is out of range or the FCB does not name a legal, concrete
    /// (non-wildcard) 8.3 file — which, together with the fixed drive
    /// directory, guarantees the path stays inside the `CPM/` container.
    pub fn resolve(&self, fcb: &Fcb) -> Option<PathBuf> {
        let drive0 = self.drive_index_for(fcb.drive)?;
        let filename = fcb.filename();
        // Re-validate as a concrete 8.3 name: rejects wildcards ('?'/'*'
        // are not valid filename chars) and anything with a separator, so
        // the join below cannot traverse out of the drive directory.
        split_8_3(&filename)?;
        let path = self.drive_dir(drive0).join(&filename);
        // Belt-and-suspenders: the resolved path must stay under base.
        if Self::is_within(&self.base, &path) {
            Some(path)
        } else {
            None
        }
    }

    /// True if `path` is lexically within `base` (neither may contain a
    /// `..` that climbs out — our names never do, but check anyway).
    fn is_within(base: &Path, path: &Path) -> bool {
        path.starts_with(base) && !path.components().any(|c| c.as_os_str() == "..")
    }

    /// BDOS "open file" (15): does the FCB name an existing file on its
    /// drive?  Returns the resolved path when it exists, else `None`.
    pub fn open_existing(&self, fcb: &Fcb) -> Option<PathBuf> {
        let path = self.resolve(fcb)?;
        if path.is_file() {
            Some(path)
        } else {
            None
        }
    }

    /// BDOS "make file" (22): create (truncating any existing file) the
    /// file the FCB names, so subsequent writes land in it.  Returns the
    /// path on success.
    pub fn make(&self, fcb: &Fcb) -> Option<PathBuf> {
        let path = self.resolve(fcb)?;
        match std::fs::File::create(&path) {
            Ok(_) => Some(path),
            Err(_) => None,
        }
    }

    /// Read one 128-byte record at `record` from the file the FCB names.
    /// Returns `Ok(None)` at end-of-file (nothing there to read); a short
    /// final record is padded with the CP/M EOF filler (0x1A).
    pub fn read_record(&self, fcb: &Fcb, record: u32) -> std::io::Result<Option<[u8; 128]>> {
        use std::io::{Read, Seek, SeekFrom};
        let path = match self.resolve(fcb) {
            Some(p) => p,
            None => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "unresolved FCB",
                ))
            }
        };
        let mut f = std::fs::File::open(&path)?;
        let offset = record as u64 * 128;
        if offset >= f.metadata()?.len() {
            return Ok(None); // reading at/after EOF
        }
        f.seek(SeekFrom::Start(offset))?;
        let mut buf = [0x1Au8; 128]; // pad a short final record with ^Z
        let mut filled = 0;
        while filled < 128 {
            let n = f.read(&mut buf[filled..])?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        if filled == 0 {
            Ok(None)
        } else {
            Ok(Some(buf))
        }
    }

    /// Write one 128-byte record at `record` into the file the FCB names
    /// (which must already exist via open/make).  Seeking past the current
    /// end zero-fills the gap, matching CP/M's record model.
    pub fn write_record(&self, fcb: &Fcb, record: u32, data: &[u8; 128]) -> std::io::Result<()> {
        use std::io::{Seek, SeekFrom, Write};
        let path = match self.resolve(fcb) {
            Some(p) => p,
            None => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "unresolved FCB",
                ))
            }
        };
        let mut f = std::fs::OpenOptions::new().read(true).write(true).open(&path)?;
        f.seek(SeekFrom::Start(record as u64 * 128))?;
        f.write_all(data)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::fcb::Fcb;
    use super::*;

    fn fcb_named(drive: u8, name: &str, ext: &str) -> Fcb {
        let mut b = [b' '; super::super::fcb::FCB_SIZE];
        b[0] = drive;
        for (i, c) in name.bytes().enumerate() {
            b[1 + i] = c;
        }
        for (i, c) in ext.bytes().enumerate() {
            b[9 + i] = c;
        }
        // Position fields must be zero, not space.
        b[12..].fill(0);
        Fcb::from_bytes(&b)
    }

    #[test]
    fn test_select_and_letter() {
        let mut fs = CpmFs::new(PathBuf::from("/tmp/cpm"));
        assert_eq!(fs.current_drive_letter(), 'A');
        assert!(fs.select(1));
        assert_eq!(fs.current_drive_letter(), 'B');
        assert!(!fs.select(8)); // I: is beyond H:
        assert_eq!(fs.current_drive_letter(), 'B'); // unchanged
    }

    #[test]
    fn test_drive_index_for() {
        let fs = CpmFs::new(PathBuf::from("/tmp/cpm"));
        assert_eq!(fs.drive_index_for(0), Some(0)); // default = current (A)
        assert_eq!(fs.drive_index_for(1), Some(0)); // A:
        assert_eq!(fs.drive_index_for(8), Some(7)); // H:
        assert_eq!(fs.drive_index_for(9), None); // I: unsupported
    }

    #[test]
    fn test_resolve_jailed_path() {
        let base = PathBuf::from("/tmp/xmodem_cpm_base");
        let fs = CpmFs::new(base.clone());
        let fcb = fcb_named(1, "PIP", "COM"); // A:PIP.COM
        let p = fs.resolve(&fcb).unwrap();
        assert_eq!(p, base.join("A").join("PIP.COM"));
    }

    #[test]
    fn test_resolve_default_drive_follows_current() {
        let base = PathBuf::from("/tmp/xmodem_cpm_base");
        let mut fs = CpmFs::new(base.clone());
        fs.select(2); // C:
        let fcb = fcb_named(0, "X", "TXT"); // drive 0 = current = C:
        let p = fs.resolve(&fcb).unwrap();
        assert_eq!(p, base.join("C").join("X.TXT"));
    }

    #[test]
    fn test_resolve_rejects_bad_drive_and_wildcards() {
        let fs = CpmFs::new(PathBuf::from("/tmp/cpm"));
        // Drive beyond H:.
        assert!(fs.resolve(&fcb_named(9, "A", "TXT")).is_none());
        // Wildcard name is not a concrete file.
        assert!(fs.resolve(&fcb_named(1, "??", "COM")).is_none());
    }

    /// Create an isolated `CPM/` base with an `A` drive directory.
    fn temp_base(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("xmodem_cpmfs_{tag}"));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("A")).unwrap();
        base
    }

    #[test]
    fn test_make_write_read_roundtrip() {
        let base = temp_base("rw");
        let fs = CpmFs::new(base.clone());
        let fcb = fcb_named(1, "DATA", "TXT");

        // No file yet: open fails.
        assert!(fs.open_existing(&fcb).is_none());

        // Make, then write two records.
        assert!(fs.make(&fcb).is_some());
        let mut rec0 = [0u8; 128];
        rec0[..5].copy_from_slice(b"HELLO");
        let mut rec1 = [0u8; 128];
        rec1[..5].copy_from_slice(b"WORLD");
        fs.write_record(&fcb, 0, &rec0).unwrap();
        fs.write_record(&fcb, 1, &rec1).unwrap();

        // Now it opens, and reads back what we wrote.
        assert!(fs.open_existing(&fcb).is_some());
        let got0 = fs.read_record(&fcb, 0).unwrap().unwrap();
        assert_eq!(&got0[..5], b"HELLO");
        let got1 = fs.read_record(&fcb, 1).unwrap().unwrap();
        assert_eq!(&got1[..5], b"WORLD");
        // Reading past EOF yields None.
        assert!(fs.read_record(&fcb, 2).unwrap().is_none());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_read_short_final_record_padded_with_ctrl_z() {
        let base = temp_base("pad");
        let fs = CpmFs::new(base.clone());
        let fcb = fcb_named(1, "SHORT", "TXT");
        assert!(fs.make(&fcb).is_some());
        // Write a 3-byte file directly (not a full record).
        std::fs::write(base.join("A").join("SHORT.TXT"), b"abc").unwrap();
        let rec = fs.read_record(&fcb, 0).unwrap().unwrap();
        assert_eq!(&rec[..3], b"abc");
        assert_eq!(rec[3], 0x1A); // padded with ^Z
        assert_eq!(rec[127], 0x1A);
        let _ = std::fs::remove_dir_all(&base);
    }
}
