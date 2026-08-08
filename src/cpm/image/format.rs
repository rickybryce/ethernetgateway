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
//!    does not miss a revolution between them.  [`Skew`] is that translation.
//!
//! The skew is mostly a property of the CP/M BIOS that shipped on the disk, and
//! that is why a hardware emulator needs no skew table at all (the guest's own
//! BIOS does the translation, and the emulator just serves physical sectors)
//! while we — reading the filesystem directly, with no guest in the loop —
//! cannot do without one.  It is *not* purely a BIOS property, though: where a
//! sector physically sits also depends on how the track was formatted, and on
//! the Altair 88-DCDD those two effects compose and change partway down the
//! disk.  See [`Skew::Split`].
//!
//! Every entry in [`FORMATS`] was measured from a real image rather than
//! transcribed, and the geometry arithmetic was checked against the file size.
//! Two different strengths of check are represented here, and it is worth
//! knowing which you are relying on:
//!
//! * **Weak** — extract a text file and require zero non-printable bytes.  This
//!   is what the Altair entry passed for months while being wrong, because a
//!   scrambled text file is still all text.  Never trust it alone again.
//! * **Strong** — boot the disk on the emulated controller, have its own
//!   operating system read a file out over a virtual modem, and match every
//!   128-byte slice against the image byte for byte.  No heuristic, no scoring,
//!   and it settled the Altair layout in one run.
//!
//! The published descriptions of this hardware (the Altair 8800 simulator
//! sources, the `cpmtools` `diskdefs` database) were used only to cross-check
//! those measurements — the same clean-room posture the Punter and HBIOS
//! implementations here were written under.

/// The byte CP/M leaves everywhere it has not written: an empty directory entry
/// and an unused data byte are both `0xE5`, because that is what a bulk-erased
/// floppy read back as.
pub const FILL: u8 = 0xE5;

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
    #[allow(dead_code)]
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
    #[allow(dead_code)]
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

    /// The per-sector check byte a write has to refresh, if this format has
    /// one: `(absolute offset of the check byte, other bytes that enter the
    /// sum)`.  The 128 data bytes always enter it and are not listed.
    ///
    /// Only the Altair 88-DCDD has one, and it is not optional there.  Its BIOS
    /// verifies every sector it reads, so a record written with a stale check
    /// byte comes back to the guest as `Bdos Err On A: Bad Sector` — the write
    /// looks like it worked and the disk is unreadable on the machine it was
    /// written for.
    ///
    /// Both layouts were measured, and both hold for every sector of six real
    /// disks (192/192 boot sectors and 2272/2272 data sectors each):
    ///
    /// ```text
    /// tracks 0-5    data at 3   byte 132 = sum(data)
    /// tracks 6-76   data at 7   byte 4   = sum(data) + bytes 2, 3, 5, 6
    /// ```
    ///
    /// The two offsets in the returned pair are absolute, so a caller never has
    /// to know which side of the split it is on.
    ///
    /// The positions below are tied to the measured data offsets of 3 and 7.  A
    /// future `AltairSplit` with different ones would get the wrong check byte,
    /// so `test_the_only_split_framing_is_the_one_the_checksums_were_measured_on`
    /// refuses to let one into `FORMATS` unnoticed.
    pub fn sector_check(&self, rec: u64) -> Option<(u64, Vec<u64>)> {
        match *self {
            Framing::Raw | Framing::Framed { .. } => None,
            Framing::AltairSplit { seclen, sectrk, split_track, .. } => {
                let base = rec * seclen as u64;
                let track = rec / sectrk as u64;
                if track < split_track as u64 {
                    Some((base + 132, Vec::new()))
                } else {
                    Some((base + 4, vec![base + 2, base + 3, base + 5, base + 6]))
                }
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
    ///
    /// Unused so far: every format measured to date interleaves.  Kept because
    /// "this disk has no skew" is a real answer that a hard-disk image is
    /// likely to give, not a placeholder.
    #[allow(dead_code)]
    None,
    /// An explicit permutation, one entry per sector in a track.  Recovered
    /// from the BIOS on the disk itself; see the module comment.
    Table(&'static [u16]),

    /// Two permutations, with the disk changing from one to the other partway
    /// down — the Altair 88-DCDD case.
    ///
    /// This exists because a disk can be *formatted* two different ways in two
    /// different regions, which is not the same thing as having two BIOS
    /// tables.  On these disks the system area and the data area were written
    /// by different code, and the data area also shifted every odd sector half
    /// a revolution from where its number says.  The boundary is the same
    /// `split_track` as [`Framing::AltairSplit`], and it is the same cause: one
    /// region is in "boot format" and the other is not.
    ///
    /// Getting this wrong is quiet.  The two tables below agree on logical
    /// sectors 0-15, and a 64-entry directory is exactly sixteen records — so
    /// the directory reads perfectly either way and only file content past the
    /// first half of a track comes back scrambled.
    Split {
        /// First track (absolute, counting the boot area) that uses `rest`.
        split_track: u16,
        /// Translation below `split_track`.
        first: &'static [u16],
        /// Translation from `split_track` on.
        rest: &'static [u16],
    },
}

impl Skew {
    /// Physical sector for a logical sector within one *absolute* track.
    ///
    /// The track is absolute — counted from the start of the disk, boot area
    /// included — because that is what a `split_track` boundary is measured in
    /// and the only reading that is unambiguous.
    ///
    /// Out-of-range input maps to itself rather than panicking: a corrupt
    /// directory can produce a wild sector number, and a mounted image must
    /// fail as a read error, never as a crash of the whole gateway.
    pub fn physical(&self, track: u32, logical: u16) -> u16 {
        let table: &[u16] = match self {
            Skew::None => return logical,
            Skew::Table(t) => t,
            Skew::Split { split_track, first, rest } => {
                if track < *split_track as u32 {
                    first
                } else {
                    rest
                }
            }
        };
        table.get(logical as usize).copied().unwrap_or(logical)
    }

    /// Every permutation this translation can use, for the checks that must
    /// hold of all of them — a `Split` has two, and both have to be a full
    /// permutation of the track or the tail of it silently maps to itself.
    #[cfg(test)]
    pub fn tables(&self) -> Vec<&'static [u16]> {
        match *self {
            Skew::None => Vec::new(),
            Skew::Table(t) => vec![t],
            Skew::Split { first, rest, .. } => vec![first, rest],
        }
    }
}

/// The Altair 88-DCDD **BIOS** sector translation, recovered from the BIOS in
/// the boot tracks of `DISK01.DSK` at de-framed offset `0x1cb8` (found there as
/// a 1-based permutation; stored 0-based here).  A four-way interleave: every
/// fourth sector, four times over, evens then odds.
///
/// This is a *logical record → sector ID* map, and on its own it is not enough
/// to find a record in the file — see [`ALTAIR_SECTOR_ORDER`].
pub const ALTAIR_BIOS_XLT: &[u16] = &[
    0, 8, 16, 24, 2, 10, 18, 26, 4, 12, 20, 28, 6, 14, 22, 30,
    1, 9, 17, 25, 3, 11, 19, 27, 5, 13, 21, 29, 7, 15, 23, 31,
];

/// Where each Altair 88-DCDD sector **ID** sits in a *data* track, as an
/// ID-indexed table of positions in the image file.
///
/// A `.dsk` stores the 32 sectors of a track in rotational-position order, and
/// on tracks 6 and up the position a sector occupies is **not** its number: ID
/// *n* is at position *n* when *n* is even and at position *(n + 16) mod 32*
/// when it is odd — the odd sectors are half a revolution from where their
/// numbering suggests.
///
/// Not inferred.  Every 137-byte sector on a data track carries its own ID in
/// the second byte of its header, and reading them back says so directly: on
/// every data track of every CP/M disk in the Altair-Duino set the positions
/// hold IDs `0, 17, 2, 19, 4, 21, …` rather than `0, 1, 2, 3, …`.  Tracks 0-5
/// are written in boot format, with that byte left at zero and no such shift —
/// which is why the skew has to change at track 6 and not before.
///
/// Not consulted at run time — [`ALTAIR_SKEW`] is the composition, computed
/// once here and pinned by a test.  It is kept because it is half of *why* that
/// table is what it is, and losing that would mean measuring it again.
#[allow(dead_code)]
pub const ALTAIR_SECTOR_ORDER: &[u16] = &[
    0, 17, 2, 19, 4, 21, 6, 23, 8, 25, 10, 27, 12, 29, 14, 31,
    16, 1, 18, 3, 20, 5, 22, 7, 24, 9, 26, 11, 28, 13, 30, 15,
];

/// The Altair 88-DCDD translation for the **data** tracks: the BIOS table
/// composed with the on-disk sector placement, mapping a logical record
/// straight to a position in the image file.
///
/// `ALTAIR_SKEW[l] == ALTAIR_SECTOR_ORDER[ALTAIR_BIOS_XLT[l]]`, pinned by
/// `test_altair_data_skew_is_the_composition_of_its_two_causes` so this table
/// can never drift from the two measurements it came from.
///
/// It is also measured end to end, and that is the part that matters.  A booted
/// Altair was made to read its own files with its own BIOS and send them out
/// over a virtual modem, and every 128-byte slice of the result was located in
/// the image by exact byte match — 447 records across eight files and twenty
/// tracks, all of them where these tables say.  That is logical record →
/// physical position as a *measurement*, with no scoring heuristic anywhere in
/// it, which is what every earlier attempt lacked.  See
/// `boot_machine::tests::test_capture_altair_ground_truth`.
pub const ALTAIR_SKEW: &[u16] = &[
    0, 8, 16, 24, 2, 10, 18, 26, 4, 12, 20, 28, 6, 14, 22, 30,
    17, 25, 1, 9, 19, 27, 3, 11, 21, 29, 5, 13, 23, 31, 7, 15,
];

/// The IBM 3740 8" single-density translation — a plain skew of 6, written out
/// so every format in the table carries an explicit permutation and the reader
/// never has to remember which convention a bare number implies.
pub const IBM3740_SKEW: &[u16] = &[
    0, 6, 12, 18, 24, 4, 10, 16, 22, 2, 8, 14, 20, 1, 7, 13,
    19, 25, 5, 11, 17, 23, 3, 9, 15, 21,
];

/// The Cromemco single-sided double-density translation, read out of the disk's
/// own DPH — the `XLT` pointer at DPH+0, sixteen entries for the sixteen
/// 512-byte sectors of a double-density track.
///
/// Found there as a **1-based** permutation, `1 12 7 2 13 8 3 14 9 4 15 10 5 16
/// 11 6`, and stored 0-based here — the same convention as [`ALTAIR_SKEW`], and
/// the reason that convention is stated rather than assumed. Sixteen entries and
/// not sixty-four: CP/M's `SPT` counts 128-byte records, but this BIOS reaches a
/// physical sector by dividing by four, so what it translates is sectors. The
/// length was found by requiring the entries to be a permutation, which adjacent
/// bytes do not form by accident.
///
/// Identical on both MICAH disks measured. The double-sided Cromemco format does
/// **not** translate at all — see [`Skew::None`] and the format below it.
pub const CROMEMCO_DD_SKEW: &[u16] =
    &[0, 11, 6, 1, 12, 7, 2, 13, 8, 3, 14, 9, 4, 15, 10, 5];

/// The Cromemco double-sided double-density translation — an interleave of four
/// within each side, thirty-two sectors to a CP/M track.
///
/// **Not in the disk's DPH.** Its `XLT` pointer is zero, which normally means a
/// disk does not translate, and taking that at face value produced a format that
/// mounted, listed its directory correctly, and returned scrambled file content —
/// the exact failure the Altair mapping took four hypotheses to escape. This
/// BIOS translates inside its own `SETSEC` instead of through CP/M's `SECTRAN`,
/// so `XLT` says nothing about it either way.
///
/// Recovered the only way that works: boot the disk, have its own CP/M `TYPE` a
/// file, and locate each of the file's 128-byte records in the image by exact
/// text match. That gives logical → physical directly. Logical sectors 0, 1, 2, 3
/// were measured at physical 0, 4, 8, 12 and logical 4 at physical 1 — an
/// interleave of four, repeating every sixteen sectors, which is one side.
///
/// A CP/M track here is a *cylinder*, both sides, so the table is the sixteen
/// measured entries and the same pattern again offset by sixteen for the second
/// side. That second half was extrapolation when it was written and is
/// **measured now**: `CPMCRT.ASM` is 15,744 bytes and crosses the side boundary,
/// and the gate below reads it back through the guest.
pub const CROMEMCO_DSDD_SKEW: &[u16] = &[
    0, 4, 8, 12, 1, 5, 9, 13, 2, 6, 10, 14, 3, 7, 11, 15,
    16, 20, 24, 28, 17, 21, 25, 29, 18, 22, 26, 30, 19, 23, 27, 31,
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
    /// 128-byte records inside one *physical* sector.
    ///
    /// One on a 128-byte-sector disk, which is most of them.  A hard disk with
    /// 256-byte sectors holds two, and the distinction matters because **skew
    /// translates whole sectors, not records** — the drive can only start
    /// reading at a sector boundary, so a 256-byte sector's two records always
    /// travel together.  Getting this wrong scatters every second record.
    pub records_per_sector: u16,
    /// Records before the data area — the boot/system tracks.  The directory
    /// begins here.
    pub reserved_records: u32,
    /// Allocation block size in bytes.
    pub blocksize: u32,
    /// Maximum directory entries.
    pub maxdir: u16,
    /// The classic EXM, when the disk states one that the usual derivation does
    /// not produce.  `None` means "derive it", which is right for every disk
    /// whose BIOS followed the published rule.
    ///
    /// **Explicit on purpose.**  Deriving EXM from block size and disk size is
    /// exactly what went wrong with the Altair floppy: the rule gives 1, the
    /// disk's own DPB says 0, and the difference is one directory entry
    /// covering eight allocation slots instead of sixteen.  `cpmtools` cannot
    /// express EXM at all, which is why it writes these disks into an entry
    /// CP/M will not list.  Other vendors made the same unusual choice, so this
    /// is a field rather than a special case.
    pub exm: Option<u32>,
    /// Allocation blocks in the data area, when the disk uses fewer than its
    /// medium would hold.  `None` means "all of them", which is the usual case.
    ///
    /// **Explicit for the same reason as [`Format::exm`]**: the disk says so and
    /// the derivation does not. The Cromemco single-sided double-density format
    /// declares `DSM 253` — 254 blocks, 520,192 bytes — on a medium with room
    /// for 300, leaving the last eleven and a half tracks outside the
    /// filesystem. Both MICAH disks measured say it, so it is the format and not
    /// one odd disk.
    ///
    /// Getting this wrong would be a *write* defect and a quiet one: deriving
    /// 300 blocks would let us allocate past block 253, into space the guest's
    /// own BIOS will not address, and the file would be unreadable on the
    /// machine the disk belongs to while looking perfectly fine here.
    pub declared_blocks: Option<u32>,
    /// Physical layout of records inside the file.
    pub framing: Framing,
    /// Logical-to-physical sector translation within a data track.
    pub skew: Skew,
    /// Exact image size in bytes, when the format has one.  Used to narrow
    /// down candidates when sniffing an unprefixed file.
    ///
    /// **No two formats here are the same size**, so a size names a format
    /// outright rather than merely shortlisting one.  It is still not
    /// sufficient on its own: a file can be exactly the right size and not hold
    /// this filesystem at all — a UCSD p-System disk is 256,256 bytes, and so
    /// is a Cromemco CDOS one — which is what the directory inspection in
    /// `identify` decides.  (This used to say that two formats shared a size
    /// and two more differed in layout at one size.  Neither was true, and the
    /// same false justification had been copied into `cpmreference.html`.)
    pub exact_size: Option<u64>,
}

impl Format {
    /// Bytes the image must be at least, for this format to be readable.
    pub fn min_bytes(&self) -> u64 {
        self.framing.image_bytes(self.total_records as u64)
    }

    /// The largest a file may be and still be this format.
    ///
    /// A trailer is real: several images in circulation carry a few bytes past
    /// their last record, and on the boot path a size test that rejected a
    /// 96-byte one was a genuine defect.  So the bound is not an exact match —
    /// it allows anything short of **one whole record**, past which the file is
    /// not this geometry with something stuck on the end, it is a different
    /// geometry.
    ///
    /// This exists because naming a format is an *override*: it skips the
    /// directory inspection and mounts read-write.  Without an upper bound the
    /// only check was that the file was big *enough*, so a 625,920-byte
    /// Cromemco double-density image named `ibm3740_*.dsk` was accepted and
    /// read as a 256,256-byte single-density one — writable, with its directory
    /// landing in the middle of a data track.
    pub fn max_bytes(&self) -> u64 {
        self.min_bytes() + self.framing.image_bytes(1) - 1
    }

    /// Byte offset of a logical record within the data area, applying skew.
    ///
    /// `rec` is a logical record index counted from the start of the data area
    /// (record 0 is the first directory record).  Returns `None` past the end
    /// of the disk.
    pub fn data_record_offset(&self, rec: u32) -> Option<u64> {
        Some(self.framing.record_offset(self.data_physical_record(rec)?))
    }

    /// The *physical* record a logical one lands on — its index in the file,
    /// counting from the first record of the boot area, before framing.
    ///
    /// Separate from [`Format::data_record_offset`] because a write needs to
    /// find the whole physical sector, not just the 128 bytes inside it: the
    /// Altair sectors carry a checksum that a write has to refresh.
    pub fn data_physical_record(&self, rec: u32) -> Option<u64> {
        let track = rec / self.sectrk as u32;
        let within = (rec % self.sectrk as u32) as u16;
        // Skew moves whole sectors.  On a disk whose sectors hold more than one
        // record, the records inside a sector keep their order and travel with
        // it — see `records_per_sector`.
        let rps = self.records_per_sector.max(1);
        let logical_sector = within / rps;
        let sub = within % rps;
        // Absolute track — the boot area included — because a skew that changes
        // partway down the disk changes at an absolute track.
        let abs_track = self.reserved_records / self.sectrk as u32 + track;
        let physical_sector = self.skew.physical(abs_track, logical_sector);
        let abs = self.reserved_records as u64
            + track as u64 * self.sectrk as u64
            + physical_sector as u64 * rps as u64
            + sub as u64;
        if abs >= self.total_records as u64 {
            return None;
        }
        Some(abs)
    }

    /// Records in the data area (everything after the boot tracks).
    pub fn data_records(&self) -> u32 {
        self.total_records.saturating_sub(self.reserved_records)
    }

    /// Allocation blocks in the data area — what the disk says, or what the
    /// medium holds when it says nothing.
    ///
    /// The one place this question is answered, because it used to be answered
    /// in four (`fs.rs` and three sites in `identify.rs`), and a disk that
    /// declares fewer blocks than it could hold needs every one of them to
    /// agree or the directory check and the allocator disagree about the disk.
    pub fn data_blocks(&self) -> u32 {
        self.declared_blocks
            .unwrap_or_else(|| self.data_records() / (self.blocksize / 128).max(1))
    }

    /// Can a blank of this format be made at all?
    ///
    /// Separate from [`Format::blank_image`] because the UIs ask this question
    /// far more often than they need an actual disk — the desktop mount screen
    /// asks it on *every frame* — and building the answer by generating the
    /// images costs 5.6 MB of allocation and about 4 ms a time, which is a
    /// measurable slice of a 60 Hz frame budget spent on a list of three
    /// labels.
    ///
    /// `blank_image` calls this first, so the two cannot disagree about which
    /// formats are supported; `test_can_make_blank_agrees_with_blank_image`
    /// pins that.
    pub fn can_make_blank(&self) -> bool {
        match self.framing {
            // No per-sector headers to author: a blank is 0xE5 and the whole
            // question is arithmetic.
            Framing::Raw => true,
            // Never measured, and a guess is worse than a refusal.
            Framing::Framed { .. } => false,
            Framing::AltairSplit { seclen, sectrk, first_off, rest_off, .. } => {
                // The byte positions the writer uses — the stop bytes, the
                // tail, the check byte — were measured on this geometry and
                // hold for nothing else.  `sectrk` is in the check because
                // `ALTAIR_SECTOR_ORDER` has exactly 32 entries and is indexed
                // by the position in a track.
                (seclen, sectrk, first_off, rest_off) == (137, 32, 3, 7)
                    // And a disk deeper than 256 tracks cannot state its own
                    // track number in a byte, so there is no header to write.
                    // The bound is on the *highest track index*, not the track
                    // count: a `total_records` that is not a whole number of
                    // tracks put the last record one track past the check, and
                    // its header byte wrapped to 0 with no error.
                    && self.total_records.saturating_sub(1) as u64 / sectrk as u64
                        <= u8::MAX as u64
            }
        }
    }

    /// A freshly-formatted, empty image of this format — what a blank floppy
    /// out of the box looks like, ready to mount and write files to.
    ///
    /// `None` for a format where nobody has measured what a real format program
    /// writes.  Filling a file with `0xE5` and hoping is precisely the kind of
    /// plausible-but-wrong artefact this module exists to avoid: on a disk with
    /// per-sector headers it produces a file that mounts, lists as empty, and
    /// is rejected by the first machine that reads it.
    ///
    /// Where a format *is* supported the bytes are not invented either.  The
    /// Altair layout below is what MITS's own `FORMAT.COM` produced when it was
    /// pointed at 337,568 bytes of nothing inside a booted Altair — including
    /// its verify pass reporting `NO ERRORS FOUND ON THIS DISKETTE` — and
    /// `test_our_blank_altair_matches_the_guests_own_format` requires our output
    /// to equal it byte for byte.
    pub fn blank_image(&self) -> Option<Vec<u8>> {
        if !self.can_make_blank() {
            return None;
        }
        let total = self.total_records as u64;
        match self.framing {
            // No headers at all: the empty directory and the empty data area
            // are both just 0xE5, which is what a formatted disk of this shape
            // holds and what CP/M reads as "unused".
            Framing::Raw => Some(vec![FILL; (total * 128) as usize]),
            // Never measured.  Nothing in FORMATS uses it, and a guess here
            // would be worse than a refusal.
            Framing::Framed { .. } => None,
            Framing::AltairSplit { seclen, sectrk, split_track, first_off, rest_off } => {
                // The byte positions below — the stop bytes, the tail, and the
                // check byte via `sector_check` — were measured on 137-byte
                // sectors with the data at 3 and 7, and hold for nothing else.
                // A future `AltairSplit` with another geometry is refused here
                // rather than indexed out of range, which keeps "an unmeasured
                // format is not offered" a property of the code and not of a
                // test that could be deleted.
                let mut out = vec![0u8; (total * seclen as u64) as usize];
                for rec in 0..total {
                    let track = (rec / sectrk as u64) as u8;
                    let pos = (rec % sectrk as u64) as usize;
                    let base = (rec * seclen as u64) as usize;
                    let sec = &mut out[base..base + seclen as usize];
                    let boot_format = (track as u16) < split_track;
                    let data_off = if boot_format { first_off } else { rest_off } as usize;

                    sec[0] = track | 0x80;
                    // On a data track the sector states its own ID, and the ID
                    // at a position is `ALTAIR_SECTOR_ORDER` read the other way
                    // — which is the same table, because that permutation is
                    // its own inverse.  Boot tracks leave it zero.
                    sec[1] = if boot_format { 0 } else { ALTAIR_SECTOR_ORDER[pos] as u8 };
                    sec[2] = 0x01;
                    sec[data_off..data_off + 128].fill(FILL);
                    if boot_format {
                        sec[131] = 0xFF; // stop byte
                        sec[133..137].fill(0x00);
                    } else {
                        sec[3] = FILL;
                        sec[5] = FILL;
                        sec[6] = FILL;
                        sec[135] = 0xFF; // stop byte
                        sec[136] = 0x00;
                    }
                    // The check byte last, over whatever the rest of this
                    // sector ended up being — never a second copy of the rule.
                    if let Some((at, also)) = self.framing.sector_check(rec) {
                        let mut sum = sec[data_off..data_off + 128]
                            .iter()
                            .fold(0u8, |a, &b| a.wrapping_add(b));
                        for extra in also {
                            sum = sum.wrapping_add(sec[(extra - base as u64) as usize]);
                        }
                        sec[(at - base as u64) as usize] = sum;
                    }
                }
                Some(out)
            }
        }
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
    // ---- Cromemco 8" double density, single sided, 625,920 bytes ----------
    //
    // Track 0 is recorded SINGLE density so a single-density boot ROM can read
    // it at all; every track after it is double density with 16 sectors of 512
    // bytes.  3,328 + 76 x 8,192 is 625,920 exactly.  That much was already
    // measured for the boot path -- see the `src/cpm/cromemco.rs` module
    // comment, which is also where the 512-byte sector comes from (CDISK03's
    // BIOS reaches one by `SRL A / SRL A`, four 128-byte records to a sector).
    //
    // The filesystem parameters are the disk's own DPB, read out of a booted
    // guest by calling its BIOS's SELDSK and following DPH+10 -- a declaration,
    // the same class of evidence `detect.rs` reads from a boot loader.  There is
    // no external cross-check available: `cpmtools` has no Cromemco definition.
    //
    //     SPT 64   BSH 4  BLM 15  EXM 0
    //     DSM 253  DRM 127  AL0 0xC0  AL1 0x00  CKS 32  OFF 2
    //
    // Two independent disks agree byte for byte on all of it -- `CDISK02` from
    // the Altair-Duino collection and `micah-cpm.dsk` from z80pack's cromemcosim
    // -- so this is the format, not one disk's quirk.  Both are MICAH 64k CP/M
    // 2.2 with SUPER BIOS 2.53.
    //
    // `DSM 253` is 254 blocks, 520,192 bytes, on a medium that would hold 300 --
    // see `declared_blocks`.  `OFF 2` reserves two tracks, and because track 0
    // is the short single-density one those two tracks are 3,328 + 8,192 =
    // 11,520 bytes, which is exactly where both disks' directories begin.
    Format {
        token: "cromemcodd",
        label: "Cromemco 8\" SSDD, 508K (MICAH CP/M 2.2)",
        total_records: 4890, // 26 + 76 x 64 records
        sectrk: 64,
        records_per_sector: 4, // 512-byte sectors
        reserved_records: 90,  // track 0 single density (26) + track 1 (64)
        blocksize: 2048,
        maxdir: 128,
        declared_blocks: Some(254),
        framing: Framing::Raw,
        skew: Skew::Table(CROMEMCO_DD_SKEW),
        exm: None,
        exact_size: Some(625_920),
    },
    // ---- Cromemco 8" double density, double sided, 1,256,704 bytes --------
    //
    // The same medium with both sides used: 3,328 + 153 x 8,192.  Its BIOS
    // counts a *cylinder* as a track -- SPT 128 is two 8,192-byte sides -- so
    // its `OFF 1` reserves one cylinder, which is again 3,328 + 8,192 = 11,520
    // bytes and again exactly where the directory starts.  The two formats
    // therefore describe their identical reserved area with different numbers,
    // which is why `reserved_records` is stored in records here and not tracks.
    //
    //     SPT 128  BSH 4  BLM 15  EXM 0
    //     DSM 607  DRM 255  AL0 0xF0  AL1 0x00  CKS 64  OFF 1
    //
    // Agreed on by `CDISK03` (Intelligent Terminals Corp 56k CP/M 2.2, release
    // 5b) and z80pack's `itc-cpm.dsk`.  608 blocks x 2,048 is 1,245,184, which
    // is the medium less the reserved cylinder exactly -- this one uses all of
    // its disk, so no `declared_blocks`.
    //
    // **Its XLT pointer is zero and it translates anyway.**  Zero normally means
    // a disk does not translate, and believing it gave a format that mounted,
    // listed its directory perfectly and returned scrambled file content -- this
    // BIOS interleaves inside its own SETSEC rather than through CP/M's SECTRAN,
    // so XLT says nothing either way.  Caught by the guest-comparison gate and
    // by nothing else; see `CROMEMCO_DSDD_SKEW`.  Worth noticing that the
    // single-sided format above interleaves differently again: two disks of one
    // physical geometry, and skew belongs to the BIOS, not to the medium.
    Format {
        token: "cromemcodsdd",
        label: "Cromemco 8\" DSDD, 1216K (ITC CP/M 2.2)",
        total_records: 9818, // 26 + 153 x 64 records
        sectrk: 128,         // a cylinder: both sides
        records_per_sector: 4,
        reserved_records: 90, // one cylinder: 26 single-density + 64
        blocksize: 2048,
        maxdir: 256,
        declared_blocks: None,
        framing: Framing::Raw,
        skew: Skew::Table(CROMEMCO_DSDD_SKEW),
        exm: None,
        exact_size: Some(1_256_704),
    },
    // The IBM 3740 layout, and the closest thing CP/M had to a universal disk.
    // Both the Tarbell and the Cromemco single-density 8" images are this.
    Format {
        token: "ibm3740",
        label: "IBM 3740 8\" SSSD, 241K (Tarbell, Cromemco SD)",
        total_records: 2002, // 77 tracks x 26 sectors
        sectrk: 26,
        records_per_sector: 1,
        reserved_records: 52, // 2 boot tracks
        blocksize: 1024,
        maxdir: 64,
        framing: Framing::Raw,
        skew: Skew::Table(IBM3740_SKEW),
        declared_blocks: None,
        exm: None,
        exact_size: Some(256_256),
    },
    // ---- Altair 88-DCDD 8" single density, 337,568 bytes -------------------
    // 77 tracks x 32 sectors x 137 bytes.  This entry was withdrawn for a long
    // time, and it is worth recording why and what settled it, because the
    // failure was a *quiet* one.
    //
    // The directory and the first half of every track read correctly, so a
    // file listing looked right and text files came back all-text — a jumbled
    // text file is still text.  What was wrong was the second half of each data
    // track.  Two facts had to be measured before that could be seen:
    //
    //   * the disk's own DPB, in the BIOS on the boot tracks at de-framed
    //     offset 0x1ca9 (fourteen bytes before the translation table, the usual
    //     BIOS arrangement of DPB then XLT):
    //
    //         SPT 32   BSH 4   BLM 15   EXM 0
    //         DSM 149  DRM 63  AL0 0xC0  AL1 0x00  OFF 2
    //
    //     EXM 0 rather than the 1 the standard derivation gives, which is what
    //     `exm: Some(0)` below is for and why `cpmtools` cannot write these
    //     disks correctly at all;
    //
    //   * that the *skew changes at track 6*, exactly where the framing does.
    //     See `Skew::Split` and `ALTAIR_SECTOR_ORDER`.
    //
    // Earlier attempts stalled because every hypothesis was scored against a
    // heuristic — "do this assembler listing's addresses ascend?" — which
    // cannot tell "nearly right" from "right", and the answer sat at 81%, which
    // is exactly where such a score stops discriminating.  What broke it open
    // was not a better hypothesis but a better oracle: boot the disk, have its
    // own CP/M read its own files out over a virtual modem, and find each
    // 128-byte slice in the image by exact byte match.  That yields logical
    // record → physical sector directly, as a measurement.  Ruled out along the
    // way, so nobody spends the time again: EXM (real, but not the cause for
    // the file being scored), the directory size, an absent skew, an inverted
    // skew, and the second permutation candidate at de-framed offset 0x1cb7.
    Format {
        token: "altair8",
        label: "Altair 88-DCDD 8\" SSSD, 300K (MITS)",
        total_records: 2464, // 77 tracks x 32 sectors
        sectrk: 32,
        records_per_sector: 1,
        reserved_records: 64, // 2 boot tracks, per the disk's own OFF
        blocksize: 2048,
        maxdir: 64, // the disk's DRM is 63
        framing: Framing::AltairSplit {
            seclen: 137,
            sectrk: 32,
            split_track: 6,
            first_off: 3,
            rest_off: 7,
        },
        skew: Skew::Split {
            split_track: 6,
            first: ALTAIR_BIOS_XLT,
            rest: ALTAIR_SKEW,
        },
        // The disk's BIOS states EXM 0 where the usual derivation gives 1.
        declared_blocks: None,
        exm: Some(0),
        exact_size: Some(337_568),
    },

    // ---- Altair 88-HDSK hard disk (the Altair-Duino disk set) --------------
    // 256-byte sectors, so two CP/M records ride in each and skew moves them
    // as a pair.  The 24-entry translation is a three-way interleave.
    Format {
        token: "altairhd",
        label: "Altair 88-HDSK hard disk, 4.8M (Altair-Duino)",
        total_records: 38_976, // 812 tracks x 24 sectors x 2 records
        sectrk: 48,            // 128-byte records per track
        records_per_sector: 2, // 256-byte physical sectors
        reserved_records: 96,  // 2 boot tracks
        blocksize: 4096,
        maxdir: 192,
        framing: Framing::Raw,
        skew: Skew::Table(ALTAIR_HDSK_SKEW),
        declared_blocks: None,
        exm: None,
        exact_size: Some(4_988_928),
    },
];

/// The Altair 88-HDSK sector translation — 24 physical sectors per track, a
/// three-way interleave.  Confirmed against the geometry of the Altair-Duino
/// disk set, whose images this reads.
pub const ALTAIR_HDSK_SKEW: &[u16] = &[
    0, 7, 14, 21, 4, 11, 18, 1, 8, 15, 22, 5,
    12, 19, 2, 9, 16, 23, 6, 13, 20, 3, 10, 17,
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
        for t in [ALTAIR_SKEW, ALTAIR_BIOS_XLT, ALTAIR_SECTOR_ORDER] {
            assert_eq!(t.len(), 32);
            let mut seen: Vec<u16> = t.to_vec();
            seen.sort_unstable();
            assert_eq!(seen, (0..32).collect::<Vec<u16>>());
        }
    }

    /// The data-track table is not an independent guess: it is what you get by
    /// composing the two things that were separately measured — the BIOS's
    /// logical-record-to-sector-ID table, and where each ID physically sits.
    /// Pinning the composition means a future edit to either half that forgets
    /// the other one fails here instead of scrambling files.
    #[test]
    fn test_altair_data_skew_is_the_composition_of_its_two_causes() {
        for l in 0..32usize {
            assert_eq!(
                ALTAIR_SKEW[l],
                ALTAIR_SECTOR_ORDER[ALTAIR_BIOS_XLT[l] as usize],
                "logical {l}: composition disagrees with the table"
            );
        }
    }

    /// The odd sectors of a data track are half a revolution from where their
    /// number says, and the even ones are not.  That single fact is the whole
    /// difference between the two Altair tables, so state it directly rather
    /// than leaving a reader to diff two 32-entry lists.
    #[test]
    fn test_altair_sector_order_shifts_only_the_odd_sectors() {
        for id in 0..32u16 {
            let want = if id % 2 == 0 { id } else { (id + 16) % 32 };
            assert_eq!(ALTAIR_SECTOR_ORDER[id as usize], want, "sector id {id}");
        }
    }

    /// The two Altair tables agree on the first sixteen logical sectors — which
    /// is exactly why this was invisible for so long.  A 64-entry directory is
    /// sixteen records, so it reads correctly under either table and only file
    /// content past the first half of a track comes back wrong.  Pinned so the
    /// next person does not conclude from a good directory that the mapping is
    /// right.
    #[test]
    fn test_the_two_altair_tables_agree_over_the_whole_directory() {
        assert_eq!(ALTAIR_SKEW[..16], ALTAIR_BIOS_XLT[..16]);
        assert_ne!(ALTAIR_SKEW[16..], ALTAIR_BIOS_XLT[16..]);
        let alt = by_token("altair8").unwrap();
        assert_eq!(alt.dir_records(), 16, "the directory is one half-track");
    }

    /// `Framing::sector_check` places the Altair check byte from constants that
    /// only hold for the data offsets it was measured on.  Anything else in
    /// `FORMATS` claiming `AltairSplit` would get a wrong checksum on every
    /// write and be rejected by its own BIOS, so it has to be caught here.
    #[test]
    fn test_the_only_split_framing_is_the_one_the_checksums_were_measured_on() {
        for f in FORMATS {
            let Framing::AltairSplit { seclen, first_off, rest_off, .. } = f.framing else {
                continue;
            };
            assert_eq!(
                (seclen, first_off, rest_off),
                (137, 3, 7),
                "{}: sector_check's byte positions were measured on 137/3/7 only",
                f.token
            );
        }
    }

    /// Both Altair check bytes must land inside the sector and outside the 128
    /// data bytes — placing one on top of the data would corrupt the record it
    /// is meant to protect.
    #[test]
    fn test_altair_check_byte_is_inside_the_sector_and_clear_of_the_data() {
        let alt = by_token("altair8").unwrap();
        for (rec, data_off) in [(0u64, 3u64), (6 * 32, 7)] {
            let (at, also) = alt.framing.sector_check(rec).expect("the Altair has a check byte");
            let base = rec * 137;
            let data = base + data_off;
            for off in std::iter::once(at).chain(also) {
                assert!(off >= base && off < base + 137, "byte {off} is outside its sector");
                assert!(
                    off < data || off >= data + 128,
                    "byte {off} sits on the record's own data"
                );
            }
        }
    }

    /// Our blank Altair image must be byte-for-byte what MITS's own
    /// `FORMAT.COM` writes.
    ///
    /// The hash is of an image produced by booting a real Altair CP/M disk,
    /// running `FORMAT`, pointing it at 337,568 bytes of zeros in drive B: and
    /// letting its `FULL` command initialise and then verify all 77 tracks —
    /// which it did, reporting no errors. A hash rather than a fixture because
    /// this is generated, not copied: it is cheap to regenerate and there is no
    /// reason to carry 330 KB in the repository to check 330 KB we can compute.
    ///
    /// This is the check that stops "blank image" meaning "a file full of
    /// 0xE5". Such a file mounts, lists as empty, and is refused by the first
    /// real BIOS that reads it, because there is not a single sector header on
    /// it.
    #[test]
    fn test_our_blank_altair_matches_the_guests_own_format() {
        use sha2::{Digest, Sha256};
        let alt = by_token("altair8").unwrap();
        let blank = alt.blank_image().expect("the Altair has a measured blank");
        assert_eq!(blank.len(), 337_568);
        assert_eq!(
            format!("{:x}", Sha256::digest(&blank)),
            "a950b6638d426ecb0266e63767945d928962599f13c2af5ddb86916bf00a1132",
            "our blank Altair image is not what FORMAT.COM produces"
        );
    }

    /// `blank_image` writes fixed byte positions and indexes a 32-entry sector
    /// table, so it must refuse any split geometry it was not measured on
    /// rather than run off the end of a sector or of that table.  A test that
    /// keeps such a format out of `FORMATS` is not enough — this has to hold in
    /// the code, because the code is what a future format would call.
    #[test]
    fn test_blank_refuses_a_split_geometry_it_was_not_measured_on() {
        let alt = by_token("altair8").unwrap();
        let with = |framing| Format { framing, ..alt.clone() };
        // The measured one works.
        assert!(alt.blank_image().is_some());
        // Everything else is refused, not guessed at.
        for bad in [
            Framing::AltairSplit { seclen: 128, sectrk: 32, split_track: 6, first_off: 3, rest_off: 7 },
            Framing::AltairSplit { seclen: 137, sectrk: 26, split_track: 6, first_off: 3, rest_off: 7 },
            Framing::AltairSplit { seclen: 137, sectrk: 64, split_track: 6, first_off: 3, rest_off: 7 },
            Framing::AltairSplit { seclen: 137, sectrk: 32, split_track: 6, first_off: 0, rest_off: 7 },
            Framing::AltairSplit { seclen: 137, sectrk: 32, split_track: 6, first_off: 3, rest_off: 9 },
        ] {
            assert!(with(bad).blank_image().is_none(), "{bad:?} must be refused");
        }
        // And a disk too deep to state its own track number in a byte.
        // Exactly 256 tracks is the last one that fits (indices 0..=255).
        let full = Format { total_records: 256 * 32, ..alt.clone() };
        assert!(full.blank_image().is_some(), "256 tracks is track index 255, which fits");
        let deep = Format { total_records: 257 * 32, ..alt.clone() };
        assert!(deep.blank_image().is_none(), "past 256 tracks there is no header to write");
        // The boundary the off-by-one lived on: not a whole number of tracks,
        // so the count rounds down inside the limit while the last record sits
        // one track past it and its header byte wraps to zero.
        let ragged = Format { total_records: 256 * 32 + 1, ..alt.clone() };
        assert!(
            ragged.blank_image().is_none(),
            "a part-track past the limit still has a record whose track will not fit in a byte"
        );
        // `Framed` has never been measured at all.
        assert!(with(Framing::Framed { seclen: 137, data_off: 3 }).blank_image().is_none());
    }

    /// Spot-check the same blank in a form a human can read against a hex dump,
    /// so a hash mismatch is diagnosable rather than just red.
    #[test]
    fn test_blank_altair_sector_headers() {
        let alt = by_token("altair8").unwrap();
        let blank = alt.blank_image().unwrap();
        let sec = |rec: usize| &blank[rec * 137..(rec + 1) * 137];

        // Track 0, boot format: no sector ID, stop byte at 131, sum of 128
        // 0xE5 bytes at 132, and a zero tail.
        let t0 = sec(0);
        assert_eq!(&t0[..3], &[0x80, 0x00, 0x01]);
        assert!(t0[3..131].iter().all(|&b| b == FILL));
        assert_eq!(t0[131], 0xFF);
        assert_eq!(t0[132], 0x80, "128 x 0xE5 sums to 0x80");
        assert_eq!(&t0[133..137], &[0, 0, 0, 0]);

        // Track 6, data format: the sector ID appears, the check byte moves to
        // 4 and takes in bytes 2, 3, 5 and 6, and the stop byte moves to 135.
        let t6 = sec(6 * 32);
        assert_eq!(&t6[..3], &[0x86, 0x00, 0x01]);
        assert_eq!(t6[3], FILL);
        assert_eq!(t6[4], 0x30, "0x80 + 0x01 + three 0xE5 header bytes");
        assert_eq!(&t6[5..7], &[FILL, FILL]);
        assert!(t6[7..135].iter().all(|&b| b == FILL));
        assert_eq!(t6[135], 0xFF);
        assert_eq!(t6[136], 0x00);

        // And the IDs round a data track are the measured placement, which is
        // the third independent sighting of it: the BIOS table, the shipped
        // disks, and now a fresh format.
        let ids: Vec<u16> = (0..32).map(|p| sec(6 * 32 + p)[1] as u16).collect();
        assert_eq!(ids, ALTAIR_SECTOR_ORDER, "a fresh format lays IDs out this way");
    }

    /// `blank_image` reads the sector placement out of `ALTAIR_SECTOR_ORDER`
    /// backwards — position to ID rather than ID to position — and gets away
    /// with using the same table only because that permutation is its own
    /// inverse.  If it ever stops being one, the blank images go silently wrong.
    #[test]
    fn test_altair_sector_order_is_its_own_inverse() {
        for id in 0..32usize {
            assert_eq!(
                ALTAIR_SECTOR_ORDER[ALTAIR_SECTOR_ORDER[id] as usize], id as u16,
                "sector {id}: the placement table is no longer an involution"
            );
        }
    }

    /// A blank must be mountable as the format that made it, and it must be a
    /// legal size — an image whose geometry does not agree with its own format
    /// is refused at mount, and generating one would be a strange way to fail.
    #[test]
    fn test_every_blank_is_the_size_its_format_expects() {
        for f in FORMATS {
            let Some(blank) = f.blank_image() else { continue };
            assert_eq!(
                blank.len() as u64,
                f.min_bytes(),
                "{}: blank is the wrong size for its own geometry",
                f.token
            );
            if let Some(size) = f.exact_size {
                assert_eq!(blank.len() as u64, size, "{}", f.token);
            }
        }
    }

    /// The Altair skew changes at the same track as its framing does, because
    /// it is the same cause: the system area is written in boot format and the
    /// data area is not.  Two constants that must move together.
    #[test]
    fn test_altair_skew_and_framing_split_at_the_same_track() {
        let alt = by_token("altair8").unwrap();
        let Framing::AltairSplit { split_track: fs, .. } = alt.framing else {
            panic!("altair8 should be split-framed");
        };
        let Skew::Split { split_track: ss, .. } = alt.skew else {
            panic!("altair8 should be split-skewed");
        };
        assert_eq!(fs, ss, "framing and skew must change on the same track");
    }

    /// The measurement itself, pinned: a handful of the 447 (file record →
    /// position) pairs that a booted Altair produced when it read its own disk
    /// out over the virtual modem.  Two of them straddle the track-6 boundary,
    /// which is the pair that would catch a regression to a single table.
    ///
    /// Offsets are stated as (absolute track, position in track) so they can be
    /// checked against a hex dump of a real `DISK01.DSK` by hand.
    #[test]
    fn test_altair_offsets_match_the_booted_guest_measurement() {
        let alt = by_token("altair8").unwrap();
        let at = |track: u64, pos: u64| Some(alt.framing.record_offset(track * 32 + pos));
        // L80.COM, blocks 2-5: boot-format tracks, the plain BIOS table.
        // Logical record 32 is track 3 sector 0; record 48 is track 3 sector 16,
        // which the BIOS table sends to position 1.
        assert_eq!(alt.data_record_offset(32), at(3, 0));
        assert_eq!(alt.data_record_offset(48), at(3, 1));
        assert_eq!(alt.data_record_offset(49), at(3, 9));
        // DEMO.PRN, block 130: data-format track 67, the shifted table.  The
        // same logical sector 16 now lands on position 17, not 1.
        assert_eq!(alt.data_record_offset(2080), at(67, 0));
        assert_eq!(alt.data_record_offset(2096), at(67, 17));
        assert_eq!(alt.data_record_offset(2097), at(67, 25));
        // PIP.COM, block 59: logical record 944 is track 31 sector 16.
        assert_eq!(alt.data_record_offset(944), at(31, 17));
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
        assert_eq!(s.physical(0, 99), 99);
        let split = Skew::Split { split_track: 6, first: ALTAIR_BIOS_XLT, rest: ALTAIR_SKEW };
        assert_eq!(split.physical(0, 99), 99);
        assert_eq!(split.physical(70, 99), 99);
    }

    /// A split translation must switch tables *at* its boundary track, not one
    /// either side of it — an off-by-one here scrambles exactly one track.
    #[test]
    fn test_split_skew_switches_at_its_boundary_track() {
        let s = Skew::Split { split_track: 6, first: ALTAIR_BIOS_XLT, rest: ALTAIR_SKEW };
        assert_eq!(s.physical(5, 16), ALTAIR_BIOS_XLT[16], "track 5 is boot format");
        assert_eq!(s.physical(6, 16), ALTAIR_SKEW[16], "track 6 is data format");
        assert_ne!(s.physical(5, 16), s.physical(6, 16));
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

    /// A boot area that is a whole number of data tracks — **required only of a
    /// `Split` skew**, and that distinction is the whole point of the test.
    ///
    /// The one place a track number is derived from `reserved_records` is
    /// `abs_track` in [`Format::data_physical_record`], and the only thing that
    /// reads `abs_track` is a [`Skew::Split`] boundary. Where the skew is a
    /// single table, or none, a reserved area that does not divide evenly is
    /// arithmetically harmless.
    ///
    /// And it is not hypothetical: both Cromemco double-density formats record
    /// **track 0 in single density** so a single-density boot ROM can read the
    /// disk at all, which makes their reserved area 26 + 64 records — 11,520
    /// bytes, exactly where both disks' directories begin, and not a whole
    /// number of the 64-record data tracks. The `Format` doc for `sectrk` says
    /// this case must fit; this test used to say it must not.
    #[test]
    fn test_a_split_skews_boot_area_lands_on_a_track_boundary() {
        for f in FORMATS {
            if !matches!(f.skew, Skew::Split { .. }) {
                continue;
            }
            assert_eq!(
                f.reserved_records % f.sectrk as u32,
                0,
                "{}: a Split skew resolves its boundary from an absolute track, so its \
                 boot area has to be a whole number of tracks",
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
        let hd = by_token("altairhd").unwrap();
        assert_eq!(hd.data_record_offset(0), Some(96 * 128));
    }


    /// A disk whose sectors hold two records must keep those two together
    /// when skew moves the sector.  Splitting them scatters every second
    /// record — invisible in a directory listing, fatal to file contents.
    #[test]
    fn test_skew_moves_whole_sectors_on_a_two_record_sector_disk() {
        let hd = by_token("altairhd").unwrap();
        assert_eq!(hd.records_per_sector, 2);
        // Logical records 0 and 1 share logical sector 0, which the table maps
        // to physical sector 0 — so they stay adjacent.
        let r0 = hd.data_record_offset(0).unwrap();
        let r1 = hd.data_record_offset(1).unwrap();
        assert_eq!(r1, r0 + 128, "the pair inside one sector stays together");
        // Records 2 and 3 are logical sector 1, which maps to physical 7.
        let r2 = hd.data_record_offset(2).unwrap();
        assert_eq!(
            r2,
            r0 + 7 * 2 * 128,
            "the next sector lands where the skew table says"
        );
        assert_eq!(hd.data_record_offset(3).unwrap(), r2 + 128);
    }

    #[test]
    fn test_hard_disk_skew_is_a_permutation() {
        assert_eq!(ALTAIR_HDSK_SKEW.len(), 24);
        let mut seen: Vec<u16> = ALTAIR_HDSK_SKEW.to_vec();
        seen.sort_unstable();
        assert_eq!(seen, (0..24).collect::<Vec<u16>>());
    }

    /// A skew table must have exactly one entry per sector in a track, or the
    /// out-of-range fallback silently turns into an identity mapping for the
    /// tail of every track.
    #[test]
    fn test_skew_tables_match_their_sector_count() {
        for f in FORMATS {
            let sectors = f.sectrk / f.records_per_sector.max(1);
            for t in f.skew.tables() {
                assert_eq!(
                    t.len(),
                    sectors as usize,
                    "{}: {} sectors per track but {} skew entries",
                    f.token,
                    sectors,
                    t.len()
                );
                let mut seen: Vec<u16> = t.to_vec();
                seen.sort_unstable();
                assert_eq!(
                    seen,
                    (0..sectors).collect::<Vec<u16>>(),
                    "{}: a skew table that is not a permutation loses sectors",
                    f.token
                );
            }
        }
    }


    #[test]
    fn test_data_record_offset_applies_skew() {
        let hd = by_token("altairhd").unwrap();
        // Logical records 0 and 1 share physical sector 0; record 2 begins the
        // next logical sector, which the table maps to physical 7.
        let r0 = hd.data_record_offset(0).unwrap();
        assert_eq!(hd.data_record_offset(2), Some(r0 + 7 * 2 * 128));
    }

    #[test]
    fn test_data_record_offset_stops_at_end_of_disk() {
        let hd = by_token("altairhd").unwrap();
        assert!(hd.data_record_offset(hd.data_records() - 1).is_some());
        assert_eq!(hd.data_record_offset(hd.data_records()), None);
    }

    #[test]
    fn test_token_parsing() {
        assert_eq!(token_of("altairhd_games.dsk"), Some("altairhd"));
        assert_eq!(token_of("ibm3740_cpm22.dsk"), Some("ibm3740"));
        assert_eq!(token_of("games.dsk"), None, "no underscore is not a token");
        assert_eq!(token_of("_leading.dsk"), None, "empty token");
        assert_eq!(token_of("my-disk_a.dsk"), None, "token must be alphanumeric");
    }

    #[test]
    fn test_by_token_is_case_insensitive() {
        assert!(by_token("ALTAIRHD").is_some());
        assert!(by_token("altairhd").is_some());
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

    /// **No two formats may claim overlapping sizes**, trailer included.
    ///
    /// This is the invariant the whole "drop a disk in and it works" behaviour
    /// stands on: a size names one format outright, so identification never has
    /// to choose between two and the `Ambiguous` branch stays unreachable. It
    /// was true of the bare sizes by luck of history; it became something that
    /// could be *broken* the moment sizes turned into ranges, because a trailer
    /// allowance widens every one of them by a record.
    ///
    /// A new format landing a record away from an existing one would not fail
    /// loudly — it would make one disk sometimes mount as the other, with every
    /// offset computed from the wrong geometry. Hence a test rather than a note.
    #[test]
    fn test_no_two_formats_claim_overlapping_sizes() {
        for a in FORMATS {
            let Some(a_lo) = a.exact_size else { continue };
            for b in FORMATS {
                if std::ptr::eq(a, b) {
                    continue;
                }
                let Some(b_lo) = b.exact_size else { continue };
                assert!(
                    a_lo > b.max_bytes() || b_lo > a.max_bytes(),
                    "{} ({}..={}) overlaps {} ({}..={}) — a size would no longer \
                     name one format, and identification would have to guess",
                    a.token,
                    a_lo,
                    a.max_bytes(),
                    b.token,
                    b_lo,
                    b.max_bytes(),
                );
            }
        }
    }

    /// The declared exact size is also the size the geometry needs.
    ///
    /// `max_bytes` is built on `min_bytes` while identification's lower bound is
    /// `exact_size`, so the two must agree or the tolerance would sit at the
    /// wrong end of the range.
    #[test]
    fn test_the_declared_size_is_the_size_the_geometry_needs() {
        for f in FORMATS {
            if let Some(exact) = f.exact_size {
                assert_eq!(exact, f.min_bytes(), "{}", f.token);
            }
            assert!(f.max_bytes() > f.min_bytes(), "{}: no room for a trailer", f.token);
        }
    }
}
