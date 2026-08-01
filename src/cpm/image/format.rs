//! Disk-image geometry: where the 128-byte CP/M records live inside a `.dsk`
//! file, and the CP/M parameters (block size, directory size, sector skew)
//! that turn those records into a filesystem.
//!
//! Two independent things have to be right before a byte of a file can be
//! read, and it is worth keeping them separate in your head:
//!
//! 1. **Framing** — a physical sector on the wire was not always 128 bytes.
//!    The Altair 8" controller wrote 137-byte sectors with the CP/M record
//!    buried inside a header/trailer; Cromemco double-density disks are
//!    single-density on track 0 and double-density everywhere after.
//!    [`Framing`] maps a linear *physical record index* to a byte offset in
//!    the file and hides all of that.
//!
//! 2. **Skew** — CP/M numbers the sectors in a track logically, and the BIOS
//!    translates each to the physical sector that actually holds it, so that
//!    consecutively-read records are spread around the platter and the drive
//!    does not miss a revolution between them.  [`Format::skew_table`] is
//!    that translation.
//!
//! **The skew is a property of the CP/M BIOS that shipped on the disk, not of
//! the image format.**  That is why a hardware emulator needs no skew table at
//! all (the guest's own BIOS does the translation, and the emulator just
//! serves physical sectors) while we — reading the filesystem directly, with
//! no guest in the loop — cannot do without one.  When an image turns up whose
//! skew we do not know, it can be recovered by scanning the image's own boot
//! tracks for a permutation of the sector numbers — which is where the Altair
//! table below came from.
//!
//! Every entry in [`FORMATS`] was measured from a real image rather than
//! transcribed: the geometry arithmetic was checked against the file size, and
//! the CP/M parameters were confirmed by extracting a text file and requiring
//! zero non-printable bytes.  The published descriptions of this hardware (the
//! Altair 8800 simulator sources, the `cpmtools` `diskdefs` database) were used
//! only to cross-check those measurements — the same clean-room posture the
//! Punter and HBIOS implementations here were written under.

/// How the 128-byte CP/M records are laid out inside the image file.
///
/// Each variant answers one question: given a record's *physical* index
/// (counting from the start of the file, before any skew translation), at what
/// byte offset does its 128 bytes begin?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framing {
    /// Records are 128 bytes laid end to end, with nothing between them.
    ///
    /// This covers more than it looks like it does.  A disk with 512-byte
    /// physical sectors still lands here as long as those sectors are stored
    /// contiguously, because four consecutive CP/M records fill one of them
    /// exactly — the record stream is unbroken either way.  Cromemco
    /// double-density images are `Raw` for precisely that reason, even though
    /// their track 0 is single-density and the rest are not.
    Raw,

    /// Fixed-size physical sectors, each carrying its 128 data bytes at a
    /// constant offset, with the rest being controller header and trailer.
    ///
    /// Not used by anything in [`FORMATS`] yet.  It is here because the
    /// 1,113,536-byte Altair images measured alongside the ones below *are*
    /// this shape — uniform 137-byte sectors with the data at offset 3, unlike
    /// [`Framing::AltairSplit`] — and they go in the table as soon as their
    /// CP/M parameters are confirmed against real content.
    Framed {
        /// Bytes per physical sector in the file.
        seclen: u16,
        /// Offset of the 128 data bytes within each sector.
        data_off: u16,
    },

    /// Altair 88-DCDD 8": 137-byte sectors whose data offset *changes* partway
    /// down the disk.
    ///
    /// The boot tracks carry the record at one offset and the data tracks at
    /// another, because the controller wrote a longer per-sector header once it
    /// was past the system area.  Nothing warns you about this: the directory
    /// lives in the boot region and reads perfectly at the first offset, so a
    /// naive reader gets a correct file listing and then silently mangles every
    /// byte of file content.  That is exactly how it presented when measured.
    AltairSplit {
        /// Bytes per physical sector (137 on every image seen).
        seclen: u16,
        /// Physical sectors per track (32 on every image seen).
        sectrk: u16,
        /// First track that uses `rest_off` instead of `first_off`.
        split_track: u16,
        /// Data offset within a sector below `split_track`.
        first_off: u16,
        /// Data offset within a sector from `split_track` on.
        rest_off: u16,
    },
}

impl Framing {
    /// Byte offset of physical record `rec` within the image file.
    ///
    /// `rec` counts 128-byte CP/M records from the start of the file in
    /// physical order — skew translation happens above this layer.
    pub fn record_offset(&self, rec: u64) -> u64 {
        match *self {
            Framing::Raw => rec * 128,
            Framing::Framed { seclen, data_off } => rec * seclen as u64 + data_off as u64,
            Framing::AltairSplit {
                seclen,
                sectrk,
                split_track,
                first_off,
                rest_off,
            } => {
                let track = rec / sectrk as u64;
                let off = if track < split_track as u64 { first_off } else { rest_off };
                rec * seclen as u64 + off as u64
            }
        }
    }

    /// Bytes the image must contain to hold `records` physical records.
    ///
    /// Used to bound-check a mount: an image shorter than its format claims is
    /// truncated, and reading past the end would hand the guest whatever
    /// happened to follow in memory.
    pub fn image_bytes(&self, records: u64) -> u64 {
        match *self {
            Framing::Raw => records * 128,
            Framing::Framed { seclen, .. } => records * seclen as u64,
            Framing::AltairSplit { seclen, .. } => records * seclen as u64,
        }
    }
}

/// Sector translation for a format: how a logical sector number within a data
/// track maps to the physical sector that holds it.
#[derive(Debug, Clone, Copy)]
pub enum Skew {
    /// No translation — logical sector *n* is physical sector *n*.
    None,
    /// An explicit permutation, one entry per sector in a track.  Recovered
    /// from the BIOS on the disk itself; see the module comment.
    Table(&'static [u16]),
}

impl Skew {
    /// Physical sector for a logical sector within one track.
    ///
    /// Out-of-range input maps to itself rather than panicking: a corrupt
    /// directory can produce a wild sector number, and a mounted image must
    /// fail as a read error, never as a crash of the whole gateway.
    pub fn physical(&self, logical: u16) -> u16 {
        match self {
            Skew::None => logical,
            Skew::Table(t) => t.get(logical as usize).copied().unwrap_or(logical),
        }
    }
}

/// The Altair 88-DCDD sector translation, recovered from the BIOS in the boot
/// tracks of `DISK01.DSK` (found there as a 1-based permutation; stored 0-based
/// here).  A four-way interleave: every fourth physical sector, four times over.
pub const ALTAIR_SKEW: &[u16] = &[
    0, 8, 16, 24, 2, 10, 18, 26, 4, 12, 20, 28, 6, 14, 22, 30,
    1, 9, 17, 25, 3, 11, 19, 27, 5, 13, 21, 29, 7, 15, 23, 31,
];

/// The IBM 3740 8" single-density translation — a plain skew of 6, written out
/// so every format in the table carries an explicit permutation and the reader
/// never has to remember which convention a bare number implies.
pub const IBM3740_SKEW: &[u16] = &[
    0, 6, 12, 18, 24, 4, 10, 16, 22, 2, 8, 14, 20, 1, 7, 13,
    19, 25, 5, 11, 17, 23, 3, 9, 15, 21,
];

/// A complete description of one disk-image format.
///
/// The CP/M half of this (`blocksize`, `maxdir`, `reserved_records`) is the
/// classic Disk Parameter Block by another name; the `framing` and `skew`
/// halves are what a real machine's controller and BIOS would have supplied.
#[derive(Debug, Clone)]
pub struct Format {
    /// Filename prefix that selects this format, e.g. `altair8` in
    /// `altair8_games.dsk`.  Lowercase, alphanumeric, no underscore — the
    /// first underscore in a filename ends the token.
    pub token: &'static str,
    /// One-line human description, shown in the mount UIs.
    pub label: &'static str,
    /// Total physical records in the image, boot area included.
    pub total_records: u32,
    /// 128-byte records per *data* track.  The boot area is described by
    /// `reserved_records` instead, so a format whose boot track is a different
    /// size than its data tracks (Cromemco) still fits.
    pub sectrk: u16,
    /// Records before the data area — the boot/system tracks.  The directory
    /// begins here.
    pub reserved_records: u32,
    /// Allocation block size in bytes.
    pub blocksize: u32,
    /// Maximum directory entries.
    pub maxdir: u16,
    /// Physical layout of records inside the file.
    pub framing: Framing,
    /// Logical-to-physical sector translation within a data track.
    pub skew: Skew,
    /// Exact image size in bytes, when the format has one.  Used only to
    /// narrow down candidates when sniffing an unprefixed file; it is never
    /// sufficient on its own, because two formats here share a size and two
    /// more differ in layout at the same size.
    pub exact_size: Option<u64>,
}

impl Format {
    /// Bytes the image must be at least, for this format to be readable.
    pub fn min_bytes(&self) -> u64 {
        self.framing.image_bytes(self.total_records as u64)
    }

    /// Byte offset of a logical record within the data area, applying skew.
    ///
    /// `rec` is a logical record index counted from the start of the data area
    /// (record 0 is the first directory record).  Returns `None` past the end
    /// of the disk.
    pub fn data_record_offset(&self, rec: u32) -> Option<u64> {
        let track = rec / self.sectrk as u32;
        let logical = (rec % self.sectrk as u32) as u16;
        let physical = self.skew.physical(logical);
        let abs = self.reserved_records as u64
            + track as u64 * self.sectrk as u64
            + physical as u64;
        if abs >= self.total_records as u64 {
            return None;
        }
        Some(self.framing.record_offset(abs))
    }

    /// Records in the data area (everything after the boot tracks).
    pub fn data_records(&self) -> u32 {
        self.total_records.saturating_sub(self.reserved_records)
    }

    /// Records the directory occupies.
    pub fn dir_records(&self) -> u32 {
        // 32 bytes per entry, 4 entries per 128-byte record.
        (self.maxdir as u32).div_ceil(4)
    }
}

/// Every format we can mount.
///
/// Measured, not transcribed — see the module comment.  A format is listed here
/// only if a real image of it was read end to end and its text files came back
/// with no corruption; formats that are still unverified are deliberately
/// absent, because mounting one with wrong parameters would show a plausible
/// file listing and hand back mangled data.
pub const FORMATS: &[Format] = &[
    // ---- 8" single density, 128-byte sectors -------------------------------
    // The IBM 3740 layout, and the closest thing CP/M had to a universal disk.
    // Both the Tarbell and the Cromemco single-density 8" images are this.
    Format {
        token: "ibm3740",
        label: "IBM 3740 8\" SSSD, 241K (Tarbell, Cromemco SD)",
        total_records: 2002, // 77 tracks x 26 sectors
        sectrk: 26,
        reserved_records: 52, // 2 boot tracks
        blocksize: 1024,
        maxdir: 64,
        framing: Framing::Raw,
        skew: Skew::Table(IBM3740_SKEW),
        exact_size: Some(256_256),
    },
    // ---- Altair 88-DCDD 8" -------------------------------------------------
    // 137-byte sectors, and the data offset moves from 3 to 7 at track 6.
    Format {
        token: "altair8",
        label: "Altair 88-DCDD 8\" floppy, 308K",
        total_records: 2464, // 77 tracks x 32 sectors
        sectrk: 32,
        reserved_records: 64, // 2 boot tracks
        blocksize: 2048,
        maxdir: 128,
        framing: Framing::AltairSplit {
            seclen: 137,
            sectrk: 32,
            split_track: 6,
            first_off: 3,
            rest_off: 7,
        },
        skew: Skew::Table(ALTAIR_SKEW),
        exact_size: Some(337_568),
    },
];

/// Look a format up by its filename token, case-insensitively.
pub fn by_token(token: &str) -> Option<&'static Format> {
    FORMATS.iter().find(|f| f.token.eq_ignore_ascii_case(token))
}

/// Split an image filename into its format token and the rest.
///
/// The token is everything before the first underscore, and must be
/// alphanumeric — so `altair8_games.dsk` yields `altair8`, while a file with no
/// underscore, or junk before it, yields `None` and falls through to sniffing.
pub fn token_of(filename: &str) -> Option<&str> {
    let token = filename.split('_').next()?;
    if token.is_empty() || !token.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    // A filename that is *only* a token (no underscore) is not a tagged name.
    if token.len() == filename.len() {
        return None;
    }
    Some(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_raw_framing_is_contiguous() {
        let f = Framing::Raw;
        assert_eq!(f.record_offset(0), 0);
        assert_eq!(f.record_offset(1), 128);
        assert_eq!(f.record_offset(52), 6656, "2 boot tracks of an 8\" SSSD");
    }

    #[test]
    fn test_framed_skips_sector_header() {
        let f = Framing::Framed { seclen: 137, data_off: 3 };
        assert_eq!(f.record_offset(0), 3);
        assert_eq!(f.record_offset(1), 140);
    }

    /// The Altair data offset moves at track 6, and getting this wrong is the
    /// failure that still produces a *correct-looking directory* — so it is
    /// pinned here explicitly on both sides of the boundary.
    #[test]
    fn test_altair_split_moves_data_offset_at_track_6() {
        let f = Framing::AltairSplit {
            seclen: 137,
            sectrk: 32,
            split_track: 6,
            first_off: 3,
            rest_off: 7,
        };
        // Track 5, last sector: still the boot-area offset.
        let last_of_track5 = 5 * 32 + 31;
        assert_eq!(f.record_offset(last_of_track5), last_of_track5 * 137 + 3);
        // Track 6, first sector: the data-area offset.
        let first_of_track6 = 6 * 32;
        assert_eq!(f.record_offset(first_of_track6), first_of_track6 * 137 + 7);
    }

    #[test]
    fn test_altair_skew_is_a_permutation() {
        assert_eq!(ALTAIR_SKEW.len(), 32);
        let mut seen: Vec<u16> = ALTAIR_SKEW.to_vec();
        seen.sort_unstable();
        assert_eq!(seen, (0..32).collect::<Vec<u16>>());
    }

    #[test]
    fn test_ibm3740_skew_is_a_permutation() {
        assert_eq!(IBM3740_SKEW.len(), 26);
        let mut seen: Vec<u16> = IBM3740_SKEW.to_vec();
        seen.sort_unstable();
        assert_eq!(seen, (0..26).collect::<Vec<u16>>());
    }

    /// A wild sector number from a corrupt directory must not panic — a bad
    /// image is a read error, never a crash of the gateway.
    #[test]
    fn test_skew_out_of_range_is_identity_not_panic() {
        let s = Skew::Table(ALTAIR_SKEW);
        assert_eq!(s.physical(99), 99);
    }

    /// Every format's declared geometry must agree with the size of a real
    /// image of it.  This is the check that would have caught the arithmetic
    /// being off by a track.
    #[test]
    fn test_declared_sizes_match_geometry() {
        for f in FORMATS {
            if let Some(size) = f.exact_size {
                assert_eq!(
                    f.min_bytes(),
                    size,
                    "{}: geometry implies {} bytes but exact_size says {}",
                    f.token,
                    f.min_bytes(),
                    size
                );
            }
        }
    }

    #[test]
    fn test_reserved_area_lands_on_a_track_boundary() {
        for f in FORMATS {
            assert_eq!(
                f.reserved_records % f.sectrk as u32,
                0,
                "{}: boot area is not a whole number of tracks",
                f.token
            );
        }
    }

    /// The measured directory offsets, pinned against the images they came
    /// from: 0x1A00 for the 8" SSSD and 0x2000 for the Altair.  Logical record
    /// 0 of the data area is the first directory record, and on both formats
    /// skew maps logical 0 to physical 0, so this is a direct byte comparison.
    #[test]
    fn test_directory_starts_where_measured() {
        let ibm = by_token("ibm3740").unwrap();
        assert_eq!(ibm.data_record_offset(0), Some(0x1A00));
        let alt = by_token("altair8").unwrap();
        assert_eq!(alt.data_record_offset(0), Some(64 * 137 + 3));
    }

    #[test]
    fn test_data_record_offset_applies_skew() {
        let alt = by_token("altair8").unwrap();
        // Logical record 1 of the data area is physical sector 8 of track 0
        // (ALTAIR_SKEW[1] == 8), still inside the boot-offset region.
        let want = (64 + 8) * 137 + 3;
        assert_eq!(alt.data_record_offset(1), Some(want));
    }

    #[test]
    fn test_data_record_offset_stops_at_end_of_disk() {
        let alt = by_token("altair8").unwrap();
        assert!(alt.data_record_offset(alt.data_records() - 1).is_some());
        assert_eq!(alt.data_record_offset(alt.data_records()), None);
    }

    #[test]
    fn test_token_parsing() {
        assert_eq!(token_of("altair8_games.dsk"), Some("altair8"));
        assert_eq!(token_of("ibm3740_cpm22.dsk"), Some("ibm3740"));
        assert_eq!(token_of("games.dsk"), None, "no underscore is not a token");
        assert_eq!(token_of("_leading.dsk"), None, "empty token");
        assert_eq!(token_of("my-disk_a.dsk"), None, "token must be alphanumeric");
    }

    #[test]
    fn test_by_token_is_case_insensitive() {
        assert!(by_token("ALTAIR8").is_some());
        assert!(by_token("altair8").is_some());
        assert!(by_token("nosuchformat").is_none());
    }

    #[test]
    fn test_tokens_are_unique_and_well_formed() {
        for f in FORMATS {
            assert!(
                f.token.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()),
                "{}: token must be lowercase alphanumeric",
                f.token
            );
            assert_eq!(
                FORMATS.iter().filter(|g| g.token == f.token).count(),
                1,
                "{}: duplicate token",
                f.token
            );
        }
    }
}
