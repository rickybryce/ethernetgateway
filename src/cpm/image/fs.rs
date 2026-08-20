//! The CP/M 2.2 filesystem inside a mounted disk image.
//!
//! This is the layer that turns "bytes at an offset" into "record 7 of
//! `STAT.COM`".  It reads the on-disk directory, threads a file's extents
//! together in order, and walks the allocation map to find the record asked
//! for.
//!
//! Reads and writes.  The rules that keep a write from destroying a disk are
//! stated where the write code begins; the short version is that data is
//! always committed before the directory that claims it, a block is never
//! handed out twice, and a damaged disk is mounted read-only rather than
//! written to.
//!
//! Three things about CP/M directories are worth stating plainly, because each
//! is a place a reader's intuition goes wrong:
//!
//! * A file is not one directory entry.  It is a *sequence* of entries called
//!   extents, each covering a fixed span of records, and they are not
//!   necessarily adjacent or in order on the disk.  A file's true length comes
//!   from the highest-numbered extent, not from counting entries.
//!
//! * "Free" is not one byte value.  A never-used directory area may be `0xE5`
//!   (what a format program writes) or all zeros (what some do instead).  Both
//!   were found on real images here; treating only `0xE5` as free makes a disk
//!   look full of nameless garbage.
//!
//! * Block 0 can never belong to a file — it is where the directory itself
//!   lives — so a zero in an allocation map means "not allocated", not "block
//!   zero".  That is what makes a sparse file readable at all.

use super::super::fcb::Fcb;
use super::format::Format;
use super::media::Media;

/// A raw 32-byte CP/M directory entry.
pub type RawEntry = [u8; 32];

/// Bytes in one directory entry.
const ENTRY_SIZE: usize = 32;

/// Directory entries per 128-byte record.
const ENTRIES_PER_RECORD: usize = 128 / ENTRY_SIZE;

/// Marker for a deleted or never-used directory entry.
const E5: u8 = 0xE5;

/// Records in one logical CP/M extent — fixed by the format at 16K.
const RECORDS_PER_EXTENT: u32 = 128;

/// One parsed directory entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirSlot {
    /// Index of this entry in the directory (0-based).
    pub index: u16,
    /// CP/M user number, 0–15.
    pub user: u8,
    /// Filename, space-padded, attribute bits stripped.
    pub name: [u8; 8],
    /// Extension, space-padded, attribute bits stripped.
    pub ext: [u8; 3],
    /// Full extent number: `EX + 32 * S2`.
    pub extent: u32,
    /// Records used in this entry's final logical extent.
    pub rc: u8,
    /// Allocation map, already widened to 16 bits and with trailing
    /// unallocated slots kept in place (a zero means "not allocated").
    pub blocks: Vec<u16>,
    /// R/O attribute — high bit of the first extension byte.
    pub read_only: bool,
    /// System attribute — high bit of the second extension byte.  A SYS file
    /// is hidden from `DIR` but is otherwise an ordinary file.
    pub system: bool,
    /// The entry exactly as it sits on the disk.
    ///
    /// Kept so an update can be a *edit* of the real bytes rather than a
    /// rebuild from the fields above.  Directory entries carry things this
    /// code does not model — `S1`, the CP/M 3 archive bit, whatever a
    /// particular vendor put in the spare bits — and rebuilding an entry from
    /// scratch would quietly drop every one of them.  Editing in place cannot.
    pub raw: RawEntry,
}

/// Geometry derived from a [`Format`], computed once at mount.
#[derive(Debug, Clone, Copy)]
pub struct Params {
    /// 128-byte records in one allocation block.
    pub records_per_block: u32,
    /// Highest valid block number (the classic DSM).
    pub max_block: u16,
    /// Block numbers in an allocation map: 16 when 8-bit, 8 when 16-bit.
    pub map_slots: usize,
    /// True when block numbers occupy two bytes each.
    pub wide_blocks: bool,
    /// Extent mask (the classic EXM): one directory entry covers
    /// `exm + 1` logical extents.
    pub exm: u32,
    /// Records the directory occupies.
    pub dir_records: u32,
}

impl Params {
    /// Derive the parameters a format implies.
    ///
    /// The two conditional quantities — block-number width and extent mask —
    /// are *derived* rather than tabulated.  CP/M's published DPB tables give
    /// them as a grid of block size against disk size, which is easy to
    /// mis-transcribe; both fall out of one fact, that an allocation map is 16
    /// bytes wide however you choose to divide it up.
    pub fn derive(fmt: &Format) -> Params {
        let records_per_block = fmt.blocksize / 128;
        // Through `data_blocks`, not divided out here: a format may declare
        // fewer blocks than its medium holds, and the allocator must agree with
        // the directory check about which it is.
        let total_blocks = fmt.data_blocks();
        // Block 0 is the directory, so the highest block number is one less
        // than the count.  `saturating_sub` keeps a nonsense format from
        // wrapping to 65535 usable blocks.
        let max_block = total_blocks.saturating_sub(1).min(u16::MAX as u32) as u16;
        let wide_blocks = max_block > 255;
        let slots_in_map = if wide_blocks { 8 } else { 16 };
        // One entry addresses `slots_in_map` blocks; how many 128-record extents
        // is that?
        let records_per_entry = slots_in_map as u32 * records_per_block;
        // The disk gets the last word.  A BIOS that states an EXM the standard
        // rule does not produce is not a broken disk — the MITS Altair floppy
        // says 0 where the rule says 1 — and believing the rule over the disk
        // puts a file in an entry CP/M will not list.
        let exm = match fmt.exm {
            Some(e) => e,
            None => (records_per_entry / RECORDS_PER_EXTENT).max(1) - 1,
        };
        // With EXM stated rather than derived, an entry may use fewer slots
        // than the 16-byte map has room for: it addresses exactly the blocks
        // its extents cover, and the rest of the map stays zero.
        let map_slots = (((exm + 1) * RECORDS_PER_EXTENT) / records_per_block.max(1))
            .min(slots_in_map as u32) as usize;
        Params {
            records_per_block,
            max_block,
            map_slots,
            wide_blocks,
            exm,
            dir_records: fmt.dir_records(),
        }
    }
}

/// A CP/M filesystem read from a mounted image.
pub struct ImageFs {
    media: Box<dyn Media>,
    fmt: &'static Format,
    params: Params,
    /// Every live directory entry, in directory order.
    dir: Vec<DirSlot>,
    /// Allocation bitmap, one entry per block, rebuilt from the directory
    /// after every mutation.  Index 0 is block 0.
    used: Vec<bool>,
    /// Set when this mount refuses every write.  Either the operator asked for
    /// it, or the disk arrived in a state where writing would make things
    /// worse — see [`ImageFs::mount`].
    read_only: bool,
}

impl ImageFs {
    /// Mount `media` as `fmt`, reading the directory.
    ///
    /// Fails when the image is too short for the format's geometry — better a
    /// refusal at mount time, naming the mismatch, than a drive that lists
    /// files and then fails on every read.
    ///
    /// A mount can come back **read-only even when read-write was asked for**,
    /// and that is deliberate.  If the directory that arrived is already
    /// inconsistent — an entry naming a block off the end of the disk, or two
    /// files sharing one block — then the disk is damaged, and the one thing
    /// guaranteed to turn damage into total loss is to start allocating on top
    /// of it.  The caller can see which it got from
    /// [`ImageFs::is_read_only`] and say so in the UI.
    pub fn mount(
        mut media: Box<dyn Media>,
        fmt: &'static Format,
        read_only: bool,
    ) -> std::io::Result<ImageFs> {
        let need = fmt.min_bytes();
        if media.len() < need {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "image is {} bytes but {} needs {}",
                    media.len(),
                    fmt.token,
                    need
                ),
            ));
        }
        let params = Params::derive(fmt);
        // The extent-mask arithmetic — `extent & !exm` for the start of an
        // entry's data — is only a mask if `exm + 1` is a power of two.  Every
        // real CP/M block size makes it one; a format table typo could not, and
        // would silently put records in the wrong place.
        if !(params.exm + 1).is_power_of_two() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{}: extent mask {} is not a power of two", fmt.token, params.exm + 1),
            ));
        }
        let dir = Self::read_directory(&mut *media, fmt, &params)?;
        let mut fs = ImageFs {
            media,
            fmt,
            params,
            dir,
            used: Vec::new(),
            read_only,
        };
        fs.rebuild_bitmap();
        if !read_only {
            if let Some(reason) = fs.inconsistency() {
                fs.read_only = true;
                crate::glog!(
                    "CP/M: mounting {} read-only — {}",
                    fmt.token,
                    reason
                );
            }
        }
        Ok(fs)
    }

    /// Describe the first sign that this directory is damaged, if any.
    ///
    /// Deliberately not a repair: guessing which of two files really owns a
    /// shared block is how a backup gets destroyed.  We report, refuse to
    /// write, and leave the disk exactly as it was found so the operator can
    /// take it somewhere that can look properly.
    fn inconsistency(&self) -> Option<String> {
        let mut owner: Vec<Option<[u8; 11]>> = vec![None; self.params.max_block as usize + 1];
        let dir_blocks = self.params.dir_records.div_ceil(self.params.records_per_block);
        for e in &self.dir {
            let mut who = [b' '; 11];
            who[..8].copy_from_slice(&e.name);
            who[8..].copy_from_slice(&e.ext);
            for &b in &e.blocks {
                if b == 0 {
                    continue;
                }
                if b > self.params.max_block {
                    return Some(format!(
                        "{} names block {b}, past the last block ({})",
                        String::from_utf8_lossy(&who).trim_end(),
                        self.params.max_block
                    ));
                }
                if (b as u32) < dir_blocks {
                    return Some(format!(
                        "{} claims block {b}, which is part of the directory",
                        String::from_utf8_lossy(&who).trim_end()
                    ));
                }
                match owner[b as usize] {
                    Some(prev) if prev != who => {
                        return Some(format!(
                            "block {b} is claimed by both {} and {}",
                            String::from_utf8_lossy(&prev).trim_end(),
                            String::from_utf8_lossy(&who).trim_end()
                        ));
                    }
                    _ => owner[b as usize] = Some(who),
                }
            }
        }
        None
    }

    /// Read one raw record from the data area by logical record number.
    fn read_data_record(
        media: &mut dyn Media,
        fmt: &Format,
        rec: u32,
    ) -> std::io::Result<[u8; 128]> {
        let off = fmt.data_record_offset(rec).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!("record {rec} is past the end of the disk"),
            )
        })?;
        let mut buf = [0u8; 128];
        media.read_at(off, &mut buf)?;
        Ok(buf)
    }

    /// Read and parse every live directory entry.
    fn read_directory(
        media: &mut dyn Media,
        fmt: &'static Format,
        params: &Params,
    ) -> std::io::Result<Vec<DirSlot>> {
        let mut out = Vec::new();
        for rec in 0..params.dir_records {
            let buf = Self::read_data_record(media, fmt, rec)?;
            for slot in 0..ENTRIES_PER_RECORD {
                let raw: RawEntry = buf[slot * ENTRY_SIZE..(slot + 1) * ENTRY_SIZE]
                    .try_into()
                    .expect("32-byte window of a 128-byte record");
                let index = (rec as usize * ENTRIES_PER_RECORD + slot) as u16;
                if let Some(e) = parse_entry(index, &raw, params) {
                    out.push(e);
                }
            }
        }
        Ok(out)
    }

    /// All extents of one file, lowest extent first.
    fn extents_of(&self, user: u8, name: &[u8; 8], ext: &[u8; 3]) -> Vec<&DirSlot> {
        let mut v: Vec<&DirSlot> = self
            .dir
            .iter()
            .filter(|e| e.user == user && &e.name == name && &e.ext == ext)
            .collect();
        v.sort_by_key(|e| e.extent);
        v
    }

    /// True if the named file exists for `user`.
    pub fn exists(&self, user: u8, name: &[u8; 8], ext: &[u8; 3]) -> bool {
        !self.extents_of(user, name, ext).is_empty()
    }

    /// Length of a file in 128-byte records, or `None` if it does not exist.
    ///
    /// Taken from the highest extent present, not from the number of extents:
    /// a file whose middle extent is missing (a damaged disk) still reports the
    /// length its last extent claims, which is what CP/M itself would do.
    pub fn file_records(&self, user: u8, name: &[u8; 8], ext: &[u8; 3]) -> Option<u32> {
        let extents = self.extents_of(user, name, ext);
        if extents.is_empty() {
            return None;
        }
        Some(
            extents
                .iter()
                .map(|e| self.extent_start(e) + self.extent_count(e))
                .max()
                .unwrap_or(0),
        )
    }

    /// First record of the data an entry holds.
    fn extent_start(&self, e: &DirSlot) -> u32 {
        (e.extent & !self.params.exm) * RECORDS_PER_EXTENT
    }

    /// Records this entry holds, counting the full logical extents below its
    /// own plus the partial count in its last one.
    fn extent_count(&self, e: &DirSlot) -> u32 {
        (e.extent & self.params.exm) * RECORDS_PER_EXTENT + e.rc as u32
    }

    /// Read record `rec` of a file.
    ///
    /// Returns `Ok(None)` for a record past the end of the file, or one inside
    /// a hole (an allocation slot that was never filled) — CP/M reads a hole as
    /// end-of-file, and so do we.
    pub fn read_record(
        &mut self,
        user: u8,
        name: &[u8; 8],
        ext: &[u8; 3],
        rec: u32,
    ) -> std::io::Result<Option<[u8; 128]>> {
        // Locate the extent covering `rec`.  Copied out of the borrow so the
        // media can be borrowed mutably for the read below.
        let found = {
            let extents = self.extents_of(user, name, ext);
            if extents.is_empty() {
                return Ok(None);
            }
            extents.iter().find_map(|e| {
                let start = self.extent_start(e);
                let count = self.extent_count(e);
                if rec >= start && rec < start + count {
                    Some((start, e.blocks.clone()))
                } else {
                    None
                }
            })
        };
        let (start, blocks) = match found {
            Some(v) => v,
            None => return Ok(None),
        };

        let within = rec - start;
        let slot = (within / self.params.records_per_block) as usize;
        let offset_in_block = within % self.params.records_per_block;
        let block = match blocks.get(slot) {
            // Zero means unallocated: block 0 holds the directory and can
            // never belong to a file.
            Some(0) | None => return Ok(None),
            Some(&b) => b,
        };
        if block > self.params.max_block {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("directory names block {block}, past the end of the disk"),
            ));
        }
        let data_rec = block as u32 * self.params.records_per_block + offset_in_block;
        let buf = Self::read_data_record(&mut *self.media, self.fmt, data_rec)?;
        Ok(Some(buf))
    }

    // ---- writing --------------------------------------------------------
    //
    // Every mutation below obeys the same three rules, and they are the whole
    // of why a write here cannot corrupt a disk:
    //
    // 1. **Data before directory.**  A block's contents are written and
    //    flushed *before* the directory entry that claims it.  Interrupt the
    //    sequence anywhere and the worst outcome is a block that no entry
    //    points at — which the next mount silently reclaims, because the
    //    allocation bitmap is rebuilt from the directory and nothing else.
    //    The opposite order would leave a live directory entry pointing at
    //    whatever the block held before, i.e. another file's data.
    //
    // 2. **Allocate only what the bitmap says is free.**  The bitmap is built
    //    from the directory at mount, includes the directory's own blocks as
    //    permanently taken, and is checked *again* at the moment of use.  A
    //    block is never handed out twice, so two files can never come to share
    //    one — the failure that silently destroys both.
    //
    // 3. **Edit entries, never rebuild them.**  A directory record is read,
    //    the 32 bytes of one entry are modified in place, and the record is
    //    written back.  The other three entries in that record, and every
    //    field of this one that we do not model, survive untouched.

    /// Rebuild the allocation bitmap from the directory.
    ///
    /// The directory is the only source of truth about what is allocated —
    /// exactly as it is for real CP/M, which has no free list either.  Anything
    /// no entry points at is free, which is what makes a half-finished write
    /// self-healing rather than a leak.
    fn rebuild_bitmap(&mut self) {
        let mut used = vec![false; self.params.max_block as usize + 1];
        // The directory's own blocks can never be handed to a file.
        let dir_blocks = self.params.dir_records.div_ceil(self.params.records_per_block);
        for b in 0..dir_blocks.min(used.len() as u32) {
            used[b as usize] = true;
        }
        for e in &self.dir {
            for &b in &e.blocks {
                if b != 0 {
                    if let Some(slot) = used.get_mut(b as usize) {
                        *slot = true;
                    }
                }
            }
        }
        self.used = used;
    }

    /// True when this image refuses writes.
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// Refuse a mutation on a read-only mount.
    fn check_writable(&self) -> std::io::Result<()> {
        if self.read_only {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "this disk image is mounted read-only",
            ));
        }
        Ok(())
    }

    /// Claim a free block, or `None` when the disk is full.
    ///
    /// Marks it used before returning, so two calls can never yield the same
    /// block even if the caller fails partway and never records it.  A block
    /// leaked that way costs space until the next mount and corrupts nothing.
    fn alloc_block(&mut self) -> Option<u16> {
        // Block 0 is the directory; start past the whole directory area.
        let first = self.params.dir_records.div_ceil(self.params.records_per_block);
        for b in first..=self.params.max_block as u32 {
            if !self.used[b as usize] {
                self.used[b as usize] = true;
                return Some(b as u16);
            }
        }
        None
    }

    /// Write one 128-byte record into an allocation block.
    fn write_block_record(
        &mut self,
        block: u16,
        offset_in_block: u32,
        data: &[u8; 128],
    ) -> std::io::Result<()> {
        if block == 0 || block > self.params.max_block {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("refusing to write block {block}: outside the data area"),
            ));
        }
        let rec = block as u32 * self.params.records_per_block + offset_in_block;
        let phys = self.fmt.data_physical_record(rec).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!("block {block} record {offset_in_block} is past the end of the disk"),
            )
        })?;
        self.write_physical_record(phys, data)
    }

    /// Write the 128 bytes of one physical record, and refresh whatever the
    /// format keeps alongside them.
    ///
    /// Everything else in the sector — the track and sector numbers in its
    /// header, the stop byte, anything we have not identified — is left exactly
    /// as it was found.  That is deliberate: the only bytes we understand well
    /// enough to author are the data and the check byte, so the disk keeps its
    /// own formatting and a write is the smallest edit that can be correct.
    /// It also means writing is only supported *into an already formatted
    /// image*; there is no path here that creates Altair sector headers from
    /// nothing, and a blank file is not a blank floppy.
    fn write_physical_record(&mut self, phys: u64, data: &[u8; 128]) -> std::io::Result<()> {
        let off = self.fmt.framing.record_offset(phys);
        self.media.write_at(off, data)?;
        let Some((check_at, also)) = self.fmt.framing.sector_check(phys) else {
            return Ok(());
        };
        let mut sum = data.iter().fold(0u8, |a, &b| a.wrapping_add(b));
        for extra in also {
            let mut byte = [0u8; 1];
            self.media.read_at(extra, &mut byte)?;
            sum = sum.wrapping_add(byte[0]);
        }
        self.media.write_at(check_at, &[sum])
    }

    /// Write a 32-byte directory entry back to the disk, then read it back and
    /// confirm it landed.
    ///
    /// The read-back is cheap — one 128-byte record — and it is the only way to
    /// notice a medium that accepted a write and did not keep it (a full disk,
    /// a dying card, an image on a filesystem that lied about the flush).  A
    /// directory that does not say what we think it says is precisely the state
    /// in which the *next* write destroys a file, so it is worth the read.
    fn write_dir_entry(&mut self, index: u16, raw: &RawEntry) -> std::io::Result<()> {
        let max = (self.params.dir_records * ENTRIES_PER_RECORD as u32) as u16;
        if index >= max {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("directory entry {index} is past the end of the directory"),
            ));
        }
        let rec = index as u32 / ENTRIES_PER_RECORD as u32;
        let slot = index as usize % ENTRIES_PER_RECORD;
        let phys = self.fmt.data_physical_record(rec).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "directory record is past the end of the disk",
            )
        })?;
        let off = self.fmt.framing.record_offset(phys);
        // Read-modify-write: the other three entries in this record are
        // somebody else's file and must come through untouched.
        let mut buf = [0u8; 128];
        self.media.read_at(off, &mut buf)?;
        buf[slot * ENTRY_SIZE..(slot + 1) * ENTRY_SIZE].copy_from_slice(raw);
        self.write_physical_record(phys, &buf)?;
        self.media.flush()?;

        let mut check = [0u8; 128];
        self.media.read_at(off, &mut check)?;
        if check != buf {
            return Err(std::io::Error::other(
                "directory write did not take — the image may be damaged",
            ));
        }
        Ok(())
    }

    /// Find a free directory slot, or `None` when the directory is full.
    ///
    /// Scans the on-disk directory rather than trusting the in-memory list: a
    /// slot is free only if the disk says so.
    fn free_dir_slot(&mut self) -> std::io::Result<Option<u16>> {
        for rec in 0..self.params.dir_records {
            let buf = Self::read_data_record(&mut *self.media, self.fmt, rec)?;
            for slot in 0..ENTRIES_PER_RECORD {
                let raw = &buf[slot * ENTRY_SIZE..(slot + 1) * ENTRY_SIZE];
                let free = raw[0] == E5 || (raw[0] == 0 && raw[1..12].iter().all(|&c| c == 0));
                if free {
                    return Ok(Some((rec as usize * ENTRIES_PER_RECORD + slot) as u16));
                }
            }
        }
        Ok(None)
    }

    /// Build a fresh directory entry for one extent of a file.
    fn new_raw_entry(user: u8, name: &[u8; 8], ext: &[u8; 3], extent: u32) -> RawEntry {
        let mut raw = [0u8; 32];
        raw[0] = user;
        raw[1..9].copy_from_slice(name);
        raw[9..12].copy_from_slice(ext);
        raw[12] = (extent % 32) as u8;
        raw[14] = (extent / 32) as u8;
        raw[15] = 0;
        raw
    }

    /// Records one directory entry can address.
    fn records_per_entry(&self) -> u32 {
        (self.params.exm + 1) * RECORDS_PER_EXTENT
    }

    /// Create an empty file.  Fails if it already exists or the directory is
    /// full.
    pub fn create(&mut self, user: u8, name: &[u8; 8], ext: &[u8; 3]) -> std::io::Result<()> {
        self.check_writable()?;
        if self.exists(user, name, ext) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "file exists",
            ));
        }
        let Some(index) = self.free_dir_slot()? else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::StorageFull,
                "directory is full",
            ));
        };
        let raw = Self::new_raw_entry(user, name, ext, 0);
        self.write_dir_entry(index, &raw)?;
        self.apply_entry(index, &raw);
        Ok(())
    }

    /// Write record `rec` of a file, allocating blocks and extents as needed.
    pub fn write_record(
        &mut self,
        user: u8,
        name: &[u8; 8],
        ext: &[u8; 3],
        rec: u32,
        data: &[u8; 128],
    ) -> std::io::Result<()> {
        self.check_writable()?;
        if !self.exists(user, name, ext) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no such file",
            ));
        }
        if self.find_entry(user, name, ext).is_some_and(|e| e.read_only) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "file is R/O",
            ));
        }

        let rpe = self.records_per_entry();
        let entry_seq = rec / rpe;
        let base_extent = entry_seq * (self.params.exm + 1);
        let within = rec - entry_seq * rpe;
        let slot = (within / self.params.records_per_block) as usize;
        let offset_in_block = within % self.params.records_per_block;

        // Locate this file's entry for that extent range, or make one.
        let existing = self
            .dir
            .iter()
            .find(|e| {
                e.user == user
                    && &e.name == name
                    && &e.ext == ext
                    && (e.extent & !self.params.exm) == base_extent
            })
            .map(|e| (e.index, e.raw));
        let (index, mut raw) = match existing {
            Some(v) => v,
            None => {
                let Some(index) = self.free_dir_slot()? else {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::StorageFull,
                        "directory is full",
                    ));
                };
                (index, Self::new_raw_entry(user, name, ext, base_extent))
            }
        };

        // Which block holds this record?  Reuse the allocated one, or claim a
        // new one.
        let mut blocks = decode_blocks(&raw, &self.params);
        let block = match blocks.get(slot).copied() {
            Some(b) if b != 0 => {
                if b > self.params.max_block {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("directory names block {b}, past the end of the disk"),
                    ));
                }
                b
            }
            Some(_) => {
                let Some(b) = self.alloc_block() else {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::StorageFull,
                        "disk is full",
                    ));
                };
                blocks[slot] = b;
                b
            }
            None => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "record is beyond what one directory entry can address",
                ))
            }
        };

        // Rule 1: data first, and flushed, before anything claims the block.
        self.write_block_record(block, offset_in_block, data)?;
        self.media.flush()?;

        // Now the directory: allocation map, then the extent/record count if
        // this write extended the file.
        encode_blocks(&mut raw, &blocks, &self.params);
        let used_now = within + 1;
        let cur_extent = raw[12] as u32 + 32 * (raw[14] & 0x3F) as u32;
        let old_used = (cur_extent & self.params.exm) * RECORDS_PER_EXTENT + raw[15] as u32;
        if used_now > old_used {
            let last_logical = (used_now - 1) / RECORDS_PER_EXTENT;
            let extent = base_extent + last_logical;
            raw[12] = (extent % 32) as u8;
            raw[14] = (extent / 32) as u8;
            raw[15] = (used_now - last_logical * RECORDS_PER_EXTENT) as u8;
        }
        self.write_dir_entry(index, &raw)?;
        self.apply_entry(index, &raw);
        Ok(())
    }

    /// Erase a file — every extent of it.  Returns how many entries went.
    pub fn delete(&mut self, user: u8, name: &[u8; 8], ext: &[u8; 3]) -> std::io::Result<usize> {
        self.check_writable()?;
        let targets: Vec<(u16, RawEntry)> = self
            .dir
            .iter()
            .filter(|e| e.user == user && &e.name == name && &e.ext == ext)
            .map(|e| (e.index, e.raw))
            .collect();
        if targets.iter().any(|(_, raw)| raw[9] & 0x80 != 0) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "file is R/O",
            ));
        }
        let n = targets.len();
        for (index, mut raw) in targets {
            // CP/M erases by stamping the user byte only; the rest of the
            // entry stays, which is what makes an undelete tool possible.
            raw[0] = E5;
            self.write_dir_entry(index, &raw)?;
        }
        self.reload()?;
        Ok(n)
    }

    /// Rename a file, keeping every extent in step.
    pub fn rename(
        &mut self,
        user: u8,
        name: &[u8; 8],
        ext: &[u8; 3],
        new_name: &[u8; 8],
        new_ext: &[u8; 3],
    ) -> std::io::Result<bool> {
        self.check_writable()?;
        if self.exists(user, new_name, new_ext) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "file exists",
            ));
        }
        let targets: Vec<(u16, RawEntry)> = self
            .dir
            .iter()
            .filter(|e| e.user == user && &e.name == name && &e.ext == ext)
            .map(|e| (e.index, e.raw))
            .collect();
        if targets.is_empty() {
            return Ok(false);
        }
        if targets.iter().any(|(_, raw)| raw[9] & 0x80 != 0) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "file is R/O",
            ));
        }
        for (index, mut raw) in targets {
            // Keep the attribute bits, which live in the high bits of the name
            // and extension we are about to overwrite.
            let attrs: Vec<u8> = raw[1..12].iter().map(|c| c & 0x80).collect();
            raw[1..9].copy_from_slice(new_name);
            raw[9..12].copy_from_slice(new_ext);
            for (slot, a) in raw[1..12].iter_mut().zip(attrs) {
                *slot |= a;
            }
            self.write_dir_entry(index, &raw)?;
        }
        self.reload()?;
        Ok(true)
    }

    /// Set or clear a file's R/O attribute.
    pub fn set_read_only(
        &mut self,
        user: u8,
        name: &[u8; 8],
        ext: &[u8; 3],
        ro: bool,
    ) -> std::io::Result<bool> {
        self.check_writable()?;
        let targets: Vec<(u16, RawEntry)> = self
            .dir
            .iter()
            .filter(|e| e.user == user && &e.name == name && &e.ext == ext)
            .map(|e| (e.index, e.raw))
            .collect();
        if targets.is_empty() {
            return Ok(false);
        }
        for (index, mut raw) in targets {
            if ro {
                raw[9] |= 0x80;
            } else {
                raw[9] &= 0x7F;
            }
            self.write_dir_entry(index, &raw)?;
        }
        self.reload()?;
        Ok(true)
    }

    /// Fold one just-written directory entry into the in-memory picture.
    ///
    /// Used instead of a full [`ImageFs::reload`] on the paths that run in a
    /// loop — writing a file record by record — because a reload re-reads the
    /// whole directory, and doing that per record costs
    /// `records x directory-records`.  On the small formats that is merely
    /// wasteful; on a large one (a hard disk with a 512-entry directory) it is
    /// minutes rather than seconds to write a file.
    ///
    /// This is not a weaker guarantee than reloading.  The bytes folded in here
    /// are the bytes [`ImageFs::write_dir_entry`] just wrote *and read back and
    /// compared*, so the in-memory picture still cannot drift from the disk —
    /// which is the property that stops the next allocation overwriting a live
    /// file.  The paths that touch several entries at once (erase, rename)
    /// still reload, because they are rare and the simpler code is worth more
    /// there than the microseconds.
    fn apply_entry(&mut self, index: u16, raw: &RawEntry) {
        let parsed = parse_entry(index, raw, &self.params);
        match self.dir.iter().position(|e| e.index == index) {
            Some(pos) => match parsed {
                Some(slot) => self.dir[pos] = slot,
                None => {
                    self.dir.remove(pos);
                }
            },
            None => {
                if let Some(slot) = parsed {
                    // Keep the list in directory order, the order a fresh read
                    // would produce.
                    let at = self
                        .dir
                        .iter()
                        .position(|e| e.index > index)
                        .unwrap_or(self.dir.len());
                    self.dir.insert(at, slot);
                }
            }
        }
        // Blocks only ever become used here — an entry that gave one up is an
        // erase, and those reload in full.
        if let Some(slot) = self.dir.iter().find(|e| e.index == index) {
            let blocks = slot.blocks.clone();
            for b in blocks {
                if b != 0 {
                    if let Some(u) = self.used.get_mut(b as usize) {
                        *u = true;
                    }
                }
            }
        }
    }

    /// Re-read the directory and rebuild the allocation bitmap.
    ///
    /// Run after the mutations that touch several entries at once.  It costs a
    /// directory read, and it buys the guarantee that the in-memory picture
    /// cannot drift from the disk — drift being the thing that makes the *next*
    /// allocation overwrite a live file.
    fn reload(&mut self) -> std::io::Result<()> {
        self.dir = Self::read_directory(&mut *self.media, self.fmt, &self.params)?;
        self.rebuild_bitmap();
        Ok(())
    }

    /// The first extent of a file, if it exists.
    fn find_entry(&self, user: u8, name: &[u8; 8], ext: &[u8; 3]) -> Option<&DirSlot> {
        self.extents_of(user, name, ext).into_iter().next()
    }

    // ---- what the BDOS layer needs on top of single-file access ----------

    /// Distinct files matching a (possibly wildcarded) FCB, in name order.
    ///
    /// Matching goes through [`Fcb::matches`] — the same predicate the
    /// folder-backed filesystem uses — so `DIR *.COM` and `ERA *.COM` cannot
    /// disagree about which files a wildcard covers, whichever kind of drive
    /// they are aimed at.
    ///
    /// Deduplicated by name: a file over 16 KB has one directory entry per
    /// extent, and a listing must still show it once.
    pub fn matching(&self, user: u8, fcb: &Fcb) -> Vec<([u8; 8], [u8; 3])> {
        let mut out: Vec<([u8; 8], [u8; 3])> = self
            .dir
            .iter()
            .filter(|e| e.user == user && fcb.matches(&e.name, &e.ext))
            .map(|e| (e.name, e.ext))
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    /// True if any file for `user` is R/O and matches the FCB.
    pub fn matching_read_only(&self, user: u8, fcb: &Fcb) -> usize {
        let ro: Vec<([u8; 8], [u8; 3])> = self
            .dir
            .iter()
            .filter(|e| e.user == user && e.read_only && fcb.matches(&e.name, &e.ext))
            .map(|e| (e.name, e.ext))
            .collect();
        let mut uniq = ro;
        uniq.sort_unstable();
        uniq.dedup();
        uniq.len()
    }

    /// The raw directory entries a BDOS search should return, in name then
    /// extent order.
    ///
    /// These are the disk's **real** entries, not synthesized ones — an image
    /// has a genuine CP/M directory, so a program that inspects the allocation
    /// map or the extent numbering sees the truth rather than a plausible
    /// fiction.  Only the user byte is normalized to 0, because a search
    /// returns entries for the calling user and CP/M programs expect to see
    /// their own user number there.
    pub fn dir_entries_matching(&self, user: u8, fcb: &Fcb) -> Vec<RawEntry> {
        let mut hits: Vec<&DirSlot> = self
            .dir
            .iter()
            .filter(|e| e.user == user && fcb.matches(&e.name, &e.ext))
            .collect();
        hits.sort_by_key(|e| (e.name, e.ext, e.extent));
        hits.iter()
            .map(|e| {
                let mut raw = e.raw;
                raw[0] = 0;
                raw
            })
            .collect()
    }

    /// One raw byte of the image, for tests that need to see what a write put
    /// on the medium rather than what reading it back says.
    #[cfg(test)]
    fn peek(&mut self, offset: u64) -> u8 {
        let mut b = [0u8; 1];
        self.media.read_at(offset, &mut b).expect("in range");
        b[0]
    }

    /// Read a whole file, up to `cap` bytes.  `Ok(None)` if it does not exist.
    pub fn read_whole(
        &mut self,
        user: u8,
        name: &[u8; 8],
        ext: &[u8; 3],
        cap: u64,
    ) -> std::io::Result<Option<Vec<u8>>> {
        let Some(records) = self.file_records(user, name, ext) else {
            return Ok(None);
        };
        if records as u64 * 128 > cap {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "file exceeds max CP/M file size",
            ));
        }
        let mut out = Vec::with_capacity(records as usize * 128);
        for rec in 0..records {
            match self.read_record(user, name, ext, rec)? {
                Some(buf) => out.extend_from_slice(&buf),
                None => break,
            }
        }
        Ok(Some(out))
    }

    /// Shrink a file to `records` records, freeing whatever that releases.
    ///
    /// Used by the CCP to consume a `$$$.SUB` a line at a time.  Extents that
    /// fall entirely past the new end are erased; the one straddling it has its
    /// record count reduced.  Blocks come back through the ordinary rebuild, so
    /// there is no separate free path to get wrong.
    pub fn truncate_to_records(
        &mut self,
        user: u8,
        name: &[u8; 8],
        ext: &[u8; 3],
        records: u32,
    ) -> std::io::Result<Option<u32>> {
        self.check_writable()?;
        if self.file_records(user, name, ext).is_none() {
            return Ok(None);
        }
        let targets: Vec<(u16, RawEntry, u32, u32)> = self
            .extents_of(user, name, ext)
            .iter()
            .map(|e| (e.index, e.raw, self.extent_start(e), self.extent_count(e)))
            .collect();
        for (index, mut raw, start, count) in targets {
            if start >= records {
                raw[0] = E5; // wholly past the new end
                self.write_dir_entry(index, &raw)?;
            } else if start + count > records {
                let keep = records - start;
                let last_logical = keep.saturating_sub(1) / RECORDS_PER_EXTENT;
                let base = (raw[12] as u32 + 32 * (raw[14] & 0x3F) as u32) & !self.params.exm;
                let extent = base + last_logical;
                raw[12] = (extent % 32) as u8;
                raw[14] = (extent / 32) as u8;
                raw[15] = (keep - last_logical * RECORDS_PER_EXTENT) as u8;
                // Release the allocation slots the truncation gave up, so the
                // blocks come back on the rebuild below.
                let slots_kept = keep.div_ceil(self.params.records_per_block) as usize;
                let mut blocks = decode_blocks(&raw, &self.params);
                for slot in blocks.iter_mut().skip(slots_kept) {
                    *slot = 0;
                }
                encode_blocks(&mut raw, &blocks, &self.params);
                self.write_dir_entry(index, &raw)?;
            }
        }
        self.reload()?;
        Ok(Some(records))
    }

    /// Bytes still free on this disk.
    pub fn free_bytes(&self) -> u64 {
        self.free_blocks() as u64 * self.params.records_per_block as u64 * 128
    }

    /// Blocks allocated to files, counted as *distinct* block numbers.
    ///
    /// Not a sum of allocation slots: a cross-linked disk (the same block
    /// claimed twice) would otherwise report more space in use than the disk
    /// has.  Used by the consistency tests.
    #[allow(dead_code)]
    pub fn used_blocks(&self) -> u32 {
        // `max_block + 1` slots, because block numbers run `0..=max_block` —
        // the same sizing `rebuild_bitmap` uses. This had an extra slot that
        // nothing could ever set; harmless, but two spellings of one length
        // invite a reader to conclude one of them knows something.
        let mut seen = vec![false; self.params.max_block as usize + 1];
        for e in &self.dir {
            for &b in &e.blocks {
                if b != 0 {
                    if let Some(slot) = seen.get_mut(b as usize) {
                        *slot = true;
                    }
                }
            }
        }
        seen.iter().filter(|s| **s).count() as u32
    }

    /// Free blocks remaining.
    pub fn free_blocks(&self) -> u32 {
        self.used.iter().filter(|u| !**u).count() as u32
    }

}

/// The read-only surface the mount UIs use to describe a mounted disk.
///
/// Separated out and scoped rather than left to trip the dead-code lint one
/// method at a time: these are consumed by the mount screens in the next step,
/// and a blanket allow over the whole module would go on hiding real dead code
/// long after that.
impl ImageFs {
    /// Every live directory entry.  Read by the mount screens and the
    /// image-inspection tests.
    #[allow(dead_code)]
    pub fn entries(&self) -> &[DirSlot] {
        &self.dir
    }

}

/// Read the 16-byte allocation map out of a directory entry.
///
/// The map is 16 bytes however you divide it: sixteen 8-bit block numbers on a
/// small disk, eight little-endian 16-bit ones once the disk needs more than
/// 255 blocks.  Which it is depends on the disk, not on the entry.
fn decode_blocks(raw: &RawEntry, params: &Params) -> Vec<u16> {
    if params.wide_blocks {
        raw[16..32]
            .as_chunks::<2>()
            .0
            .iter()
            .take(params.map_slots)
            .map(|p| u16::from_le_bytes(*p))
            .collect()
    } else {
        raw[16..32].iter().take(params.map_slots).map(|&b| b as u16).collect()
    }
}

/// Write an allocation map back into a directory entry.
///
/// The inverse of [`decode_blocks`], and paired with it by a round-trip test —
/// an encode that disagrees with the decode would hand a file somebody else's
/// blocks, which is the worst thing this module could do.
fn encode_blocks(raw: &mut RawEntry, blocks: &[u16], params: &Params) {
    if params.wide_blocks {
        for (slot, &b) in raw[16..32].as_chunks_mut::<2>().0.iter_mut().zip(blocks) {
            *slot = b.to_le_bytes();
        }
    } else {
        for (slot, &b) in raw[16..32].iter_mut().zip(blocks) {
            // Unreachable: a narrow map means the disk has at most 256 blocks,
            // so the allocator cannot produce a number that does not fit.  If
            // it ever did, truncating would silently point the file at a
            // *different, valid* block — somebody else's data — so write zero
            // (unallocated) instead, which merely reads as end-of-file.
            debug_assert!(b <= 0xFF, "block {b} does not fit a narrow allocation map");
            *slot = if b <= 0xFF { b as u8 } else { 0 };
        }
    }
}

/// Parse one raw directory entry, or `None` if the slot is free or unusable.
fn parse_entry(index: u16, raw: &RawEntry, params: &Params) -> Option<DirSlot> {
    // Free comes in two spellings — see the module comment.
    if raw[0] == E5 {
        return None;
    }
    if raw[0] == 0 && raw[1..12].iter().all(|&c| c == 0) {
        return None;
    }
    // User numbers run 0–15.  Anything else is not a directory entry: on a
    // real disk it is a label or a timestamp record (CP/M 3 uses 0x20 and
    // 0x21), and on a misidentified one it is file data.  Either way it is not
    // ours to interpret.
    if raw[0] > 15 {
        return None;
    }
    let mut name = [b' '; 8];
    let mut ext = [b' '; 3];
    for (slot, &src) in name.iter_mut().zip(&raw[1..9]) {
        *slot = src & 0x7F;
    }
    for (slot, &src) in ext.iter_mut().zip(&raw[9..12]) {
        *slot = src & 0x7F;
    }
    // A name must be printable; anything else means we are not looking at a
    // directory.
    if name.iter().chain(ext.iter()).any(|&c| !(0x20..0x7F).contains(&c)) {
        return None;
    }
    let extent = raw[12] as u32 + 32 * (raw[14] & 0x3F) as u32;
    let blocks = decode_blocks(raw, params);
    Some(DirSlot {
        index,
        user: raw[0],
        name,
        ext,
        extent,
        rc: raw[15],
        blocks,
        read_only: raw[9] & 0x80 != 0,
        system: raw[10] & 0x80 != 0,
        raw: *raw,
    })
}

#[cfg(test)]
mod tests {
    use super::super::format::by_token;
    use super::super::media::MemMedia;
    use super::*;

    fn name_of(s: &str) -> ([u8; 8], [u8; 3]) {
        let mut n = [b' '; 8];
        let mut e = [b' '; 3];
        let (base, ext) = s.split_once('.').unwrap_or((s, ""));
        for (slot, c) in n.iter_mut().zip(base.bytes()) {
            *slot = c;
        }
        for (slot, c) in e.iter_mut().zip(ext.bytes()) {
            *slot = c;
        }
        (n, e)
    }

    /// Build a blank image of `fmt`, directory filled with 0xE5.
    fn blank(fmt: &Format) -> Vec<u8> {
        vec![E5; fmt.min_bytes() as usize]
    }

    /// Write a directory entry into a blank image at directory slot `index`.
    #[allow(clippy::too_many_arguments)]
    fn put_entry(
        img: &mut [u8],
        fmt: &Format,
        index: u16,
        user: u8,
        fname: &str,
        extent: u32,
        rc: u8,
        blocks: &[u8],
    ) {
        let rec = index as u32 / ENTRIES_PER_RECORD as u32;
        let slot = index as usize % ENTRIES_PER_RECORD;
        let off = fmt.data_record_offset(rec).unwrap() as usize + slot * ENTRY_SIZE;
        let (n, e) = name_of(fname);
        let mut raw = [0u8; 32];
        raw[0] = user;
        raw[1..9].copy_from_slice(&n);
        raw[9..12].copy_from_slice(&e);
        raw[12] = (extent % 32) as u8;
        raw[14] = (extent / 32) as u8;
        raw[15] = rc;
        // Through the real encoder, so a format with 16-bit block numbers gets
        // them written the way it will read them back.
        let wide: Vec<u16> = blocks.iter().map(|&b| b as u16).collect();
        encode_blocks(&mut raw, &wide, &Params::derive(fmt));
        img[off..off + 32].copy_from_slice(&raw);
    }

    /// Write 128 bytes into a data block.
    fn put_block_record(img: &mut [u8], fmt: &Format, block: u8, rec_in_block: u32, data: &[u8]) {
        let p = Params::derive(fmt);
        let rec = block as u32 * p.records_per_block + rec_in_block;
        let off = fmt.data_record_offset(rec).unwrap() as usize;
        let mut buf = [0x1Au8; 128];
        buf[..data.len()].copy_from_slice(data);
        img[off..off + 128].copy_from_slice(&buf);
    }

    fn mount(img: Vec<u8>, fmt: &'static Format) -> ImageFs {
        ImageFs::mount(Box::new(MemMedia::new(img)), fmt, false).unwrap()
    }

    /// The interop tests must exist.
    ///
    /// They are `#[ignore]`, so they do not run in CI and a deleted one does
    /// not fail anything — it simply stops existing, and the suite still says
    /// "ok".  One of them *was* deleted by a careless scripted edit and the
    /// loss went unnoticed for two commits, because a missing test is invisible
    /// in a passing run.  This counts them, so removing one is a decision
    /// somebody has to make on purpose.
    #[test]
    fn test_the_interop_tests_still_exist() {
        let src = include_str!("fs.rs");
        for name in [
            "fn test_our_writes_are_readable_by_cpmtools",
            "fn test_our_hard_disk_writes_are_readable_by_cpmtools",
            "fn test_real_image_matches_cpmtools",
            "fn test_hard_disk_matches_cpmtools",
        ] {
            assert!(
                src.contains(name),
                "{name} is gone — cpmtools interop is the only independent check \
                 this module has; restore it rather than deleting this assertion"
            );
        }
    }

    #[test]
    fn test_params_for_the_measured_formats() {
        let ibm = Params::derive(by_token("ibm3740").unwrap());
        assert_eq!(ibm.records_per_block, 8, "1K blocks");
        assert_eq!(ibm.max_block, 242, "243 blocks on an 8\" SSSD");
        assert!(!ibm.wide_blocks, "fits in 8-bit block numbers");
        assert_eq!(ibm.exm, 0, "1K blocks, one extent per entry");
        assert_eq!(ibm.dir_records, 16, "64 entries, 4 per record");

        let hd = Params::derive(by_token("altairhd").unwrap());
        assert_eq!(hd.records_per_block, 32, "4K blocks");
        assert!(hd.max_block > 255, "a 4.8M disk needs 16-bit block numbers");
        assert!(hd.wide_blocks);
        assert_eq!(hd.dir_records, 48, "192 entries, 4 per record");
    }

    #[test]
    fn test_blank_image_has_no_files() {
        let fmt = by_token("ibm3740").unwrap();
        let fs = mount(blank(fmt), fmt);
        assert!(fs.entries().is_empty());
        assert_eq!(fs.used_blocks(), 0);
    }

    /// A directory area of zeros — the other spelling of "free" — must also
    /// read as empty, not as a disk full of nameless entries.
    #[test]
    fn test_zero_filled_directory_reads_as_empty() {
        let fmt = by_token("ibm3740").unwrap();
        let mut img = blank(fmt);
        for rec in 0..fmt.dir_records() {
            let off = fmt.data_record_offset(rec).unwrap() as usize;
            img[off..off + 128].fill(0);
        }
        let fs = mount(img, fmt);
        assert!(fs.entries().is_empty(), "zeros are free space, not files");
    }

    #[test]
    fn test_single_extent_file_round_trip() {
        let fmt = by_token("ibm3740").unwrap();
        let mut img = blank(fmt);
        // One file, one extent, 3 records, living in block 5.
        put_entry(&mut img, fmt, 0, 0, "HELLO.TXT", 0, 3, &[5]);
        put_block_record(&mut img, fmt, 5, 0, b"first");
        put_block_record(&mut img, fmt, 5, 1, b"second");
        put_block_record(&mut img, fmt, 5, 2, b"third");

        let mut fs = mount(img, fmt);
        let (n, e) = name_of("HELLO.TXT");
        assert!(fs.exists(0, &n, &e));
        assert_eq!(fs.file_records(0, &n, &e), Some(3));
        assert_eq!(&fs.read_record(0, &n, &e, 0).unwrap().unwrap()[..5], b"first");
        assert_eq!(&fs.read_record(0, &n, &e, 2).unwrap().unwrap()[..5], b"third");
        assert!(
            fs.read_record(0, &n, &e, 3).unwrap().is_none(),
            "past the end of the file"
        );
        assert_eq!(fs.used_blocks(), 1);
    }

    /// A file's records must follow it across an extent boundary, and the
    /// extents must be threaded by extent *number*, not by directory order.
    #[test]
    fn test_multi_extent_file_reads_in_order_regardless_of_directory_order() {
        let fmt = by_token("ibm3740").unwrap();
        let mut img = blank(fmt);
        // Extent 1 is written into the *earlier* directory slot on purpose.
        put_entry(&mut img, fmt, 0, 0, "BIG.DAT", 1, 1, &[20]);
        put_entry(&mut img, fmt, 1, 0, "BIG.DAT", 0, 128, &[10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 21, 22, 23, 24, 25, 26]);
        put_block_record(&mut img, fmt, 10, 0, b"start");
        put_block_record(&mut img, fmt, 20, 0, b"second-extent");

        let mut fs = mount(img, fmt);
        let (n, e) = name_of("BIG.DAT");
        assert_eq!(fs.file_records(0, &n, &e), Some(129), "128 + 1");
        assert_eq!(&fs.read_record(0, &n, &e, 0).unwrap().unwrap()[..5], b"start");
        assert_eq!(
            &fs.read_record(0, &n, &e, 128).unwrap().unwrap()[..13],
            b"second-extent",
            "first record of extent 1"
        );
    }

    /// With 2K blocks one directory entry covers two logical extents, so the
    /// extent mask has to be applied — otherwise a file's second half lands at
    /// the wrong record.
    #[test]
    fn test_extent_mask_applies_on_multi_extent_entries() {
        let fmt = by_token("altairhd").unwrap();
        let mut img = blank(fmt);
        // Entry with extent 1 and EXM 1: covers records 0..(128 + rc).
        put_entry(&mut img, fmt, 0, 0, "WIDE.BIN", 1, 4, &[3, 4]);
        put_block_record(&mut img, fmt, 3, 0, b"rec0");
        // The second allocation slot begins one whole block in.
        let rpb = Params::derive(fmt).records_per_block;
        put_block_record(&mut img, fmt, 4, 0, b"rec-b2");

        let mut fs = mount(img, fmt);
        let (n, e) = name_of("WIDE.BIN");
        assert_eq!(fs.file_records(0, &n, &e), Some(132), "1*128 + rc 4");
        assert_eq!(&fs.read_record(0, &n, &e, 0).unwrap().unwrap()[..4], b"rec0");
        assert_eq!(
            &fs.read_record(0, &n, &e, rpb).unwrap().unwrap()[..6],
            b"rec-b2",
            "the second allocation slot"
        );
    }

    /// A zero in the allocation map is a hole, not block zero — block zero is
    /// the directory, and reading it as file data would hand the guest the
    /// directory itself.
    #[test]
    fn test_unallocated_slot_reads_as_end_of_file_not_block_zero() {
        let fmt = by_token("ibm3740").unwrap();
        let mut img = blank(fmt);
        // Claims 16 records but allocates nothing.
        put_entry(&mut img, fmt, 0, 0, "SPARSE.DAT", 0, 16, &[]);
        let mut fs = mount(img, fmt);
        let (n, e) = name_of("SPARSE.DAT");
        assert!(fs.read_record(0, &n, &e, 0).unwrap().is_none());
    }

    /// A directory naming a block past the end of the disk is corruption; it
    /// must be an error, not a read of whatever follows in the file.
    #[test]
    fn test_block_past_end_of_disk_is_an_error() {
        let fmt = by_token("ibm3740").unwrap();
        let mut img = blank(fmt);
        put_entry(&mut img, fmt, 0, 0, "BAD.DAT", 0, 8, &[250]);
        let mut fs = mount(img, fmt);
        let (n, e) = name_of("BAD.DAT");
        assert!(fs.read_record(0, &n, &e, 0).is_err());
    }

    #[test]
    fn test_user_numbers_separate_files() {
        let fmt = by_token("ibm3740").unwrap();
        let mut img = blank(fmt);
        put_entry(&mut img, fmt, 0, 0, "SAME.COM", 0, 1, &[5]);
        put_entry(&mut img, fmt, 1, 3, "SAME.COM", 0, 1, &[6]);
        put_block_record(&mut img, fmt, 5, 0, b"user0");
        put_block_record(&mut img, fmt, 6, 0, b"user3");

        let mut fs = mount(img, fmt);
        let (n, e) = name_of("SAME.COM");
        assert_eq!(&fs.read_record(0, &n, &e, 0).unwrap().unwrap()[..5], b"user0");
        assert_eq!(&fs.read_record(3, &n, &e, 0).unwrap().unwrap()[..5], b"user3");
        assert!(!fs.exists(7, &n, &e), "no such file for user 7");
    }

    #[test]
    fn test_attributes_are_decoded() {
        let fmt = by_token("ibm3740").unwrap();
        let mut img = blank(fmt);
        put_entry(&mut img, fmt, 0, 0, "PROT.COM", 0, 1, &[5]);
        // Set the R/O and SYS attribute bits by hand.
        let off = fmt.data_record_offset(0).unwrap() as usize;
        img[off + 9] |= 0x80;
        img[off + 10] |= 0x80;
        let fs = mount(img, fmt);
        let e = &fs.entries()[0];
        assert!(e.read_only);
        assert!(e.system);
        assert_eq!(&e.name[..4], b"PROT", "attribute bits are not part of the name");
    }

    /// Cross-linked blocks must not inflate the used count past the disk size.
    #[test]
    fn test_used_blocks_counts_distinct_blocks() {
        let fmt = by_token("ibm3740").unwrap();
        let mut img = blank(fmt);
        put_entry(&mut img, fmt, 0, 0, "A.DAT", 0, 8, &[7]);
        put_entry(&mut img, fmt, 1, 0, "B.DAT", 0, 8, &[7]);
        let fs = mount(img, fmt);
        assert_eq!(fs.used_blocks(), 1, "same block claimed twice is one block");
    }

    #[test]
    fn test_short_image_is_refused_at_mount() {
        let fmt = by_token("ibm3740").unwrap();
        let short = vec![E5; (fmt.min_bytes() - 1) as usize];
        match ImageFs::mount(Box::new(MemMedia::new(short)), fmt, false) {
            Ok(_) => panic!("a truncated image must not mount"),
            Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::InvalidData),
        }
    }

    // ---- writing --------------------------------------------------------

    #[test]
    fn test_create_write_read_back() {
        let fmt = by_token("ibm3740").unwrap();
        let mut fs = mount(blank(fmt), fmt);
        let (n, e) = name_of("NEW.TXT");
        fs.create(0, &n, &e).unwrap();
        assert!(fs.exists(0, &n, &e));
        assert_eq!(fs.file_records(0, &n, &e), Some(0), "created empty");

        let mut rec = [0x1Au8; 128];
        rec[..5].copy_from_slice(b"hello");
        fs.write_record(0, &n, &e, 0, &rec).unwrap();
        assert_eq!(fs.file_records(0, &n, &e), Some(1));
        assert_eq!(fs.read_record(0, &n, &e, 0).unwrap().unwrap(), rec);
    }

    #[test]
    fn test_create_refuses_duplicate() {
        let fmt = by_token("ibm3740").unwrap();
        let mut fs = mount(blank(fmt), fmt);
        let (n, e) = name_of("DUP.TXT");
        fs.create(0, &n, &e).unwrap();
        assert_eq!(
            fs.create(0, &n, &e).unwrap_err().kind(),
            std::io::ErrorKind::AlreadyExists
        );
    }

    /// Write enough records to spill past one directory entry and confirm the
    /// second extent is created and threaded correctly.
    #[test]
    fn test_write_spills_into_a_second_extent() {
        let fmt = by_token("ibm3740").unwrap();
        let mut fs = mount(blank(fmt), fmt);
        let (n, e) = name_of("BIG.DAT");
        fs.create(0, &n, &e).unwrap();
        // 1K blocks, exm 0 => one entry covers 128 records.
        for rec in [0u32, 127, 128, 200] {
            let mut buf = [0u8; 128];
            buf[..4].copy_from_slice(&rec.to_le_bytes());
            fs.write_record(0, &n, &e, rec, &buf).unwrap();
        }
        assert_eq!(fs.file_records(0, &n, &e), Some(201));
        for rec in [0u32, 127, 128, 200] {
            let got = fs.read_record(0, &n, &e, rec).unwrap().unwrap();
            assert_eq!(
                u32::from_le_bytes(got[..4].try_into().unwrap()),
                rec,
                "record {rec} came back as another record"
            );
        }
    }

    /// The whole point of the exercise: two files writing at the same time must
    /// never be handed the same block.
    #[test]
    fn test_two_files_never_share_a_block() {
        let fmt = by_token("ibm3740").unwrap();
        let mut fs = mount(blank(fmt), fmt);
        let (a, ae) = name_of("A.DAT");
        let (b, be) = name_of("B.DAT");
        fs.create(0, &a, &ae).unwrap();
        fs.create(0, &b, &be).unwrap();
        // Interleave the writes, which is what two sessions would do.
        for rec in 0..40u32 {
            let mut buf = [0u8; 128];
            buf[..4].copy_from_slice(&rec.to_le_bytes());
            buf[4] = b'A';
            fs.write_record(0, &a, &ae, rec, &buf).unwrap();
            buf[4] = b'B';
            fs.write_record(0, &b, &be, rec, &buf).unwrap();
        }
        for rec in 0..40u32 {
            assert_eq!(fs.read_record(0, &a, &ae, rec).unwrap().unwrap()[4], b'A');
            assert_eq!(fs.read_record(0, &b, &be, rec).unwrap().unwrap()[4], b'B');
        }
        assert!(fs.inconsistency().is_none(), "the disk must stay consistent");
    }

    /// Erasing a file must return its blocks and must not disturb its
    /// neighbours — the classic way an allocator eats the next file along.
    #[test]
    fn test_delete_frees_blocks_without_touching_other_files() {
        let fmt = by_token("ibm3740").unwrap();
        let mut fs = mount(blank(fmt), fmt);
        let (a, ae) = name_of("GONE.DAT");
        let (b, be) = name_of("KEEP.DAT");
        fs.create(0, &a, &ae).unwrap();
        fs.create(0, &b, &be).unwrap();
        let mut buf = [0u8; 128];
        buf[..4].copy_from_slice(b"keep");
        for rec in 0..20u32 {
            fs.write_record(0, &a, &ae, rec, &[0xAA; 128]).unwrap();
            fs.write_record(0, &b, &be, rec, &buf).unwrap();
        }
        let before = fs.free_blocks();
        assert_eq!(fs.delete(0, &a, &ae).unwrap(), 1);
        assert!(fs.free_blocks() > before, "erase must return blocks");
        assert!(!fs.exists(0, &a, &ae));
        for rec in 0..20u32 {
            assert_eq!(
                &fs.read_record(0, &b, &be, rec).unwrap().unwrap()[..4],
                b"keep",
                "the surviving file was damaged by the erase"
            );
        }
        assert!(fs.inconsistency().is_none());
    }

    /// Space freed by an erase must be reusable, and reusing it must not
    /// resurrect the old contents into the new file.
    #[test]
    fn test_freed_blocks_are_reused_cleanly() {
        let fmt = by_token("ibm3740").unwrap();
        let mut fs = mount(blank(fmt), fmt);
        let (a, ae) = name_of("FIRST.DAT");
        fs.create(0, &a, &ae).unwrap();
        for rec in 0..16u32 {
            fs.write_record(0, &a, &ae, rec, &[0xAA; 128]).unwrap();
        }
        fs.delete(0, &a, &ae).unwrap();

        let (b, be) = name_of("SECOND.DAT");
        fs.create(0, &b, &be).unwrap();
        fs.write_record(0, &b, &be, 0, &[0x55; 128]).unwrap();
        assert_eq!(fs.read_record(0, &b, &be, 0).unwrap().unwrap(), [0x55; 128]);
        assert!(fs.inconsistency().is_none());
    }

    #[test]
    fn test_rename_keeps_every_extent_in_step() {
        let fmt = by_token("ibm3740").unwrap();
        let mut fs = mount(blank(fmt), fmt);
        let (n, e) = name_of("OLD.DAT");
        fs.create(0, &n, &e).unwrap();
        fs.write_record(0, &n, &e, 0, &[1; 128]).unwrap();
        fs.write_record(0, &n, &e, 200, &[2; 128]).unwrap();
        assert_eq!(fs.extents_of(0, &n, &e).len(), 2, "two extents to rename");

        let (nn, ne) = name_of("NEW.DAT");
        assert!(fs.rename(0, &n, &e, &nn, &ne).unwrap());
        assert!(!fs.exists(0, &n, &e));
        assert_eq!(fs.file_records(0, &nn, &ne), Some(201));
        assert_eq!(fs.read_record(0, &nn, &ne, 200).unwrap().unwrap(), [2; 128]);
    }

    #[test]
    fn test_rename_refuses_to_clobber() {
        let fmt = by_token("ibm3740").unwrap();
        let mut fs = mount(blank(fmt), fmt);
        let (a, ae) = name_of("A.DAT");
        let (b, be) = name_of("B.DAT");
        fs.create(0, &a, &ae).unwrap();
        fs.create(0, &b, &be).unwrap();
        assert_eq!(
            fs.rename(0, &a, &ae, &b, &be).unwrap_err().kind(),
            std::io::ErrorKind::AlreadyExists
        );
        assert!(fs.exists(0, &a, &ae), "the source survives a refused rename");
    }

    #[test]
    fn test_read_only_attribute_blocks_write_delete_rename() {
        let fmt = by_token("ibm3740").unwrap();
        let mut fs = mount(blank(fmt), fmt);
        let (n, e) = name_of("PROT.DAT");
        fs.create(0, &n, &e).unwrap();
        fs.write_record(0, &n, &e, 0, &[7; 128]).unwrap();
        assert!(fs.set_read_only(0, &n, &e, true).unwrap());

        assert!(fs.write_record(0, &n, &e, 1, &[8; 128]).is_err());
        assert!(fs.delete(0, &n, &e).is_err());
        let (nn, ne) = name_of("OTHER.DAT");
        assert!(fs.rename(0, &n, &e, &nn, &ne).is_err());
        assert_eq!(fs.read_record(0, &n, &e, 0).unwrap().unwrap(), [7; 128]);

        // ...and clearing it lets the file be written again.
        assert!(fs.set_read_only(0, &n, &e, false).unwrap());
        assert!(fs.write_record(0, &n, &e, 1, &[8; 128]).is_ok());
    }

    /// A read-only mount must refuse every mutation, not merely most of them.
    #[test]
    fn test_read_only_mount_refuses_every_mutation() {
        let fmt = by_token("ibm3740").unwrap();
        let mut img = blank(fmt);
        put_entry(&mut img, fmt, 0, 0, "THERE.DAT", 0, 1, &[5]);
        let mut fs =
            ImageFs::mount(Box::new(MemMedia::new(img)), fmt, true).unwrap();
        assert!(fs.is_read_only());
        let (n, e) = name_of("THERE.DAT");
        let (nn, ne) = name_of("NEW.DAT");
        assert!(fs.create(0, &nn, &ne).is_err());
        assert!(fs.write_record(0, &n, &e, 0, &[1; 128]).is_err());
        assert!(fs.delete(0, &n, &e).is_err());
        assert!(fs.rename(0, &n, &e, &nn, &ne).is_err());
        assert!(fs.set_read_only(0, &n, &e, true).is_err());
        // and the file is still there, unchanged
        assert!(fs.exists(0, &n, &e));
    }

    /// A disk that arrives cross-linked is damaged.  Mounting it read-write
    /// must quietly downgrade to read-only rather than allocate on top of the
    /// damage and finish the job.
    #[test]
    fn test_cross_linked_disk_mounts_read_only() {
        let fmt = by_token("ibm3740").unwrap();
        let mut img = blank(fmt);
        put_entry(&mut img, fmt, 0, 0, "A.DAT", 0, 8, &[7]);
        put_entry(&mut img, fmt, 1, 0, "B.DAT", 0, 8, &[7]);
        let fs = ImageFs::mount(Box::new(MemMedia::new(img)), fmt, false).unwrap();
        assert!(fs.is_read_only(), "a cross-linked disk must not be written to");
    }

    #[test]
    fn test_disk_naming_a_block_past_the_end_mounts_read_only() {
        let fmt = by_token("ibm3740").unwrap();
        let mut img = blank(fmt);
        put_entry(&mut img, fmt, 0, 0, "BAD.DAT", 0, 8, &[250]);
        let fs = ImageFs::mount(Box::new(MemMedia::new(img)), fmt, false).unwrap();
        assert!(fs.is_read_only());
    }

    /// A file must never be given a block belonging to the directory — doing so
    /// overwrites the directory with file data and loses the whole disk.
    #[test]
    fn test_allocation_never_touches_the_directory() {
        let fmt = by_token("ibm3740").unwrap();
        let mut fs = mount(blank(fmt), fmt);
        let (n, e) = name_of("FILL.DAT");
        fs.create(0, &n, &e).unwrap();
        // 64 entries / 4 per record = 16 records = 2 blocks of directory.
        let dir_blocks = fs.params.dir_records.div_ceil(fs.params.records_per_block);
        for rec in 0..64u32 {
            fs.write_record(0, &n, &e, rec, &[0xFF; 128]).unwrap();
        }
        for slot in &fs.dir[0].blocks {
            assert!(
                *slot == 0 || *slot as u32 >= dir_blocks,
                "block {slot} overlaps the directory"
            );
        }
        // The directory still reads as a directory.
        assert!(fs.exists(0, &n, &e));
    }

    /// Fill the disk and confirm it reports full rather than wrapping around
    /// and overwriting block 0.
    #[test]
    fn test_disk_full_is_reported_not_wrapped() {
        let fmt = by_token("ibm3740").unwrap();
        let mut fs = mount(blank(fmt), fmt);
        let (n, e) = name_of("HOG.DAT");
        fs.create(0, &n, &e).unwrap();
        let mut rec = 0u32;
        let err = loop {
            match fs.write_record(0, &n, &e, rec, &[0xEE; 128]) {
                Ok(()) => rec += 1,
                Err(e) => break e,
            }
            assert!(rec < 100_000, "the disk never filled up");
        };
        assert!(
            matches!(
                err.kind(),
                std::io::ErrorKind::StorageFull | std::io::ErrorKind::InvalidInput
            ),
            "expected a full-disk error, got {err:?}"
        );
        assert!(fs.inconsistency().is_none(), "filling the disk corrupted it");
        // Everything written before the disk filled must still read back.
        for r in (0..rec).step_by(37) {
            assert_eq!(fs.read_record(0, &n, &e, r).unwrap().unwrap(), [0xEE; 128]);
        }
    }

    /// A full directory must be reported, not silently written past the end
    /// into the first data block.
    #[test]
    fn test_directory_full_is_reported() {
        let fmt = by_token("ibm3740").unwrap();
        let mut fs = mount(blank(fmt), fmt);
        let mut made = 0;
        for i in 0..200 {
            let (n, e) = name_of(&format!("F{i:05}.DAT"));
            match fs.create(0, &n, &e) {
                Ok(()) => made += 1,
                Err(err) => {
                    assert_eq!(err.kind(), std::io::ErrorKind::StorageFull);
                    break;
                }
            }
        }
        assert_eq!(made, 64, "an 8\" SSSD holds 64 directory entries");
        assert!(fs.inconsistency().is_none());
    }

    /// A directory update must leave the other three entries in its record
    /// alone — they belong to other files.
    #[test]
    fn test_directory_update_preserves_neighbouring_entries() {
        let fmt = by_token("ibm3740").unwrap();
        let mut fs = mount(blank(fmt), fmt);
        let names: Vec<_> = (0..4).map(|i| name_of(&format!("N{i}.DAT"))).collect();
        for (n, e) in &names {
            fs.create(0, n, e).unwrap();
        }
        // These four share one 128-byte directory record.
        fs.write_record(0, &names[1].0, &names[1].1, 0, &[9; 128]).unwrap();
        for (n, e) in &names {
            assert!(fs.exists(0, n, e), "a neighbouring entry was lost");
        }
    }

    /// The encode/decode pair for allocation maps must round-trip exactly.  A
    /// mismatch would hand a file another file's blocks.
    #[test]
    fn test_block_map_round_trips() {
        for fmt in [by_token("ibm3740").unwrap(), by_token("altairhd").unwrap()] {
            let p = Params::derive(fmt);
            let blocks: Vec<u16> = (0..p.map_slots as u16).map(|i| i * 3 + 1).collect();
            let mut raw = [0u8; 32];
            encode_blocks(&mut raw, &blocks, &p);
            let back = decode_blocks(&raw, &p);
            assert_eq!(&back[..blocks.len()], &blocks[..], "{}", fmt.token);
        }
    }

    /// Writing must not disturb bytes outside the record it was aimed at —
    /// neither its neighbours in the same block nor the boot tracks.
    #[test]
    fn test_write_touches_only_its_own_record() {
        let fmt = by_token("ibm3740").unwrap();
        let mut img = blank(fmt);
        // Stamp the reserved area so we can prove it survives.
        img[..fmt.reserved_records as usize * 128].fill(0x5A);
        let before = img[..fmt.reserved_records as usize * 128].to_vec();

        let mut fs = ImageFs::mount(Box::new(MemMedia::new(img)), fmt, false).unwrap();
        let (n, e) = name_of("ONE.DAT");
        fs.create(0, &n, &e).unwrap();
        fs.write_record(0, &n, &e, 0, &[1; 128]).unwrap();
        fs.write_record(0, &n, &e, 2, &[3; 128]).unwrap();
        // Record 1 was never written: it must read as the blank fill, not as a
        // copy of a neighbour.
        assert_eq!(fs.read_record(0, &n, &e, 1).unwrap().unwrap(), [E5; 128]);

        let mut after = vec![0u8; fmt.reserved_records as usize * 128];
        fs.media.read_at(0, &mut after).unwrap();
        assert_eq!(after, before, "the boot tracks were modified");
    }

    // A "does every text file contain only printable bytes?" test used to live
    // here.  It is gone deliberately.  It was too weak to be worth the
    // confidence it gave: a *jumbled* text file is still all text, so it passed
    // on a format whose blocks were being assembled in the wrong order, and
    // that false assurance is what put an unsound format in the shipped table.
    // The byte-for-byte comparisons against cpmtools below cover the same
    // images properly.  Prefer an oracle that knows the right answer over one
    // that only knows what a wrong answer tends to look like.

    /// The corruption test that matters: write a disk with our code, then hand
    /// it to `cpmtools` and require *it* to agree — about the file list, about
    /// every byte of every file, and about the disk being consistent.
    ///
    /// Reading a disk correctly only proves we understand the format.  Writing
    /// one that a wholly separate implementation still reads is what proves we
    /// have not quietly corrupted it, and it is the check that would catch an
    /// allocation map written in the wrong width, an extent numbered wrongly,
    /// or a directory entry landing one slot out.
    ///
    /// Ignored: needs `cpmtools` installed.
    #[test]
    #[ignore]
    fn test_our_writes_are_readable_by_cpmtools() {
        let work = std::env::temp_dir().join("egw_cpm_write_interop");
        let _ = std::fs::remove_dir_all(&work);
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(
            work.join("diskdefs"),
            "diskdef t\n seclen 128\n tracks 77\n sectrk 26\n blocksize 1024\n \
             maxdir 64\n skew 6\n boottrk 2\n os 2.2\nend\n",
        )
        .unwrap();

        run_write_interop(&work, "ibm3740");
    }

    /// The same check against the hard disk.
    ///
    /// It needs its own run because `altairhd` is the format with **16-bit
    /// block numbers** and two records per physical sector — the allocation map
    /// is encoded differently and skew moves records in pairs, neither of which
    /// the 8-bit floppy exercises.  Read interop already covered it; writes did
    /// not, and this pass is where that was noticed.
    #[test]
    #[ignore]
    fn test_our_hard_disk_writes_are_readable_by_cpmtools() {
        let work = std::env::temp_dir().join("egw_cpm_write_interop_hd");
        let _ = std::fs::remove_dir_all(&work);
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(
            work.join("diskdefs"),
            "diskdef t\n seclen 256\n tracks 812\n sectrk 24\n blocksize 4096\n \
             maxdir 192\n skewtab 00,07,14,21,04,11,18,01,08,15,22,05,12,19,02,09,\
16,23,06,13,20,03,10,17\n boottrk 2\n os 2.2\nend\n",
        )
        .unwrap();
        run_write_interop(&work, "altairhd");
    }

    /// Write a disk with our code, then require `cpmtools` to agree about every
    /// byte.  `work` must already hold a `diskdefs` describing the format as
    /// `t`.
    fn run_write_interop(work: &std::path::Path, token: &'static str) {
        let fmt = by_token(token).unwrap();
        let img_path = work.join("out.dsk");
        std::fs::write(&img_path, blank(fmt)).unwrap();

        // Contents chosen to exercise the awkward cases: a file that spills
        // past one extent, one that is exactly a block, and one holding every
        // byte value.
        let payloads: Vec<(String, Vec<u8>)> = vec![
            ("SMALL.TXT".into(), b"hello from the gateway".to_vec()),
            ("BLOCK.BIN".into(), vec![0x42; 1024]),
            ("BIG.DAT".into(), (0..200 * 128).map(|i| (i % 251) as u8).collect()),
            ("ALLBYTE.BIN".into(), (0..=255u8).cycle().take(4096).collect()),
        ];

        {
            let media = super::super::media::FileMedia::open(&img_path, false).unwrap();
            let mut fs = ImageFs::mount(Box::new(media), fmt, false).unwrap();
            assert!(!fs.is_read_only(), "a blank disk must mount writable");
            for (fname, data) in &payloads {
                let (n, e) = name_of(fname);
                fs.create(0, &n, &e).unwrap();
                for (rec, chunk) in data.chunks(128).enumerate() {
                    let mut buf = [0x1Au8; 128];
                    buf[..chunk.len()].copy_from_slice(chunk);
                    fs.write_record(0, &n, &e, rec as u32, &buf).unwrap();
                }
            }
            assert!(fs.inconsistency().is_none(), "our own writes made it inconsistent");
        }

        // Now let cpmtools have it.
        let out = work.join("extracted");
        std::fs::create_dir_all(&out).unwrap();
        let status = std::process::Command::new("cpmcp")
            .current_dir(work)
            .arg("-f")
            .arg("t")
            .arg(&img_path)
            .arg("0:*.*")
            .arg(&out)
            .status();
        match status {
            Ok(s) if s.success() => {}
            Ok(s) => panic!("{token}: cpmcp could not read a disk we wrote: {s}"),
            Err(e) => {
                eprintln!("cpmtools not installed ({e}) — skipping");
                return;
            }
        }

        let mut compared = 0usize;
        for (fname, data) in &payloads {
            let path = out.join(fname.to_lowercase());
            let theirs = std::fs::read(&path)
                .unwrap_or_else(|e| panic!("cpmtools did not produce {fname}: {e}"));
            // CP/M records are 128 bytes and the tail is ^Z-padded, so compare
            // the payload we actually wrote.
            assert!(
                theirs.len() >= data.len(),
                "{fname}: cpmtools read {} bytes, we wrote {}",
                theirs.len(),
                data.len()
            );
            assert_eq!(
                &theirs[..data.len()],
                &data[..],
                "{token}/{fname}: cpmtools disagrees with what we wrote"
            );
            compared += 1;
        }

        assert_eq!(compared, payloads.len(), "{token}: not every file came back");
        let _ = std::fs::remove_dir_all(work);
    }

    /// Ground-truth interop: read every file off a real image with our code and
    /// with `cpmtools`, and require the bytes to be identical.
    ///
    /// This is the strong check — the "is it printable?" heuristic above can
    /// only catch a layout that produces obvious garbage, while this catches a
    /// single record landing one block out.  `cpmtools` is an independent
    /// implementation of the same 1970s specification, so it plays the part
    /// `lrzsz` plays for the XMODEM and ZMODEM suites here.
    ///
    /// Only formats `cpmtools` can read natively are covered: it has no notion
    /// of the Altair's 137-byte framing, so `altair8` is verified by the
    /// printable-text check above instead.
    ///
    /// Ignored: needs `cpmtools` installed and `CPM_IMAGE_DIR` set.
    #[test]
    #[ignore]
    fn test_real_image_matches_cpmtools() {
        let Ok(dir) = std::env::var("CPM_IMAGE_DIR") else {
            eprintln!("set CPM_IMAGE_DIR to run this test");
            return;
        };
        let img = std::path::PathBuf::from(&dir).join("TDISK01.DSK");
        let work = std::env::temp_dir().join("egw_cpmtools_interop");
        let _ = std::fs::remove_dir_all(&work);
        std::fs::create_dir_all(&work).unwrap();

        // cpmtools reads `diskdefs` from the working directory.
        std::fs::write(
            work.join("diskdefs"),
            "diskdef t\n seclen 128\n tracks 77\n sectrk 26\n blocksize 1024\n \
             maxdir 64\n skew 6\n boottrk 2\n os 2.2\nend\n",
        )
        .unwrap();
        let out = work.join("out");
        std::fs::create_dir_all(&out).unwrap();
        let status = std::process::Command::new("cpmcp")
            .current_dir(&work)
            .arg("-f")
            .arg("t")
            .arg(&img)
            .arg("0:*.*")
            .arg(&out)
            .status();
        match status {
            Ok(s) if s.success() => {}
            Ok(s) => panic!("cpmcp failed: {s}"),
            Err(e) => {
                eprintln!("cpmtools not installed ({e}) — skipping");
                return;
            }
        }

        let fmt = by_token("ibm3740").unwrap();
        let mut fs = mount(std::fs::read(&img).unwrap(), fmt);
        let mut compared = 0;
        for entry in std::fs::read_dir(&out).unwrap().flatten() {
            let theirs = std::fs::read(entry.path()).unwrap();
            let fname = entry.file_name().to_string_lossy().to_uppercase();
            let (n, e) = name_of(&fname);
            let total = fs.file_records(0, &n, &e).unwrap_or_else(|| {
                panic!("{fname}: cpmtools found it, we did not")
            });
            let mut ours = Vec::new();
            for rec in 0..total {
                match fs.read_record(0, &n, &e, rec).unwrap() {
                    Some(buf) => ours.extend_from_slice(&buf),
                    None => break,
                }
            }
            // cpmtools trims a text file at the ^Z that CP/M pads the final
            // record with; compare only the length it produced.
            let common = ours.len().min(theirs.len());
            assert!(common > 0 || theirs.is_empty(), "{fname}: read nothing");
            assert_eq!(
                &ours[..common],
                &theirs[..common],
                "{fname}: our bytes differ from cpmtools"
            );
            compared += 1;
        }
        assert!(compared > 5, "expected a disk full of files, compared {compared}");
        let _ = std::fs::remove_dir_all(&work);
    }

    /// The same byte-for-byte comparison against the Altair-Duino hard disk.
    ///
    /// Worth its own test because this is the only format here whose physical
    /// sectors hold *two* CP/M records, so skew moves them in pairs.  Get that
    /// wrong and every second record lands in the wrong place — which a
    /// directory listing would not show, but this does.
    ///
    /// Ignored: needs `cpmtools` and `CPM_HDSK_IMAGE` pointing at an image.
    #[test]
    #[ignore]
    fn test_hard_disk_matches_cpmtools() {
        let Ok(img) = std::env::var("CPM_HDSK_IMAGE") else {
            eprintln!("set CPM_HDSK_IMAGE to run this test");
            return;
        };
        let img = std::path::PathBuf::from(img);
        let work = std::env::temp_dir().join("egw_hdsk_interop");
        let _ = std::fs::remove_dir_all(&work);
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(
            work.join("diskdefs"),
            "diskdef t\n seclen 256\n tracks 812\n sectrk 24\n blocksize 4096\n \
             maxdir 192\n skewtab 00,07,14,21,04,11,18,01,08,15,22,05,12,19,02,09,16,23,\
06,13,20,03,10,17\n boottrk 2\n os 2.2\nend\n",
        )
        .unwrap();
        let out = work.join("out");
        std::fs::create_dir_all(&out).unwrap();
        let status = std::process::Command::new("cpmcp")
            .current_dir(&work)
            .arg("-f")
            .arg("t")
            .arg(&img)
            .arg("0:*.*")
            .arg(&out)
            .status();
        match status {
            Ok(s) if s.success() => {}
            Ok(s) => panic!("cpmcp failed: {s}"),
            Err(e) => {
                eprintln!("cpmtools not installed ({e}) — skipping");
                return;
            }
        }

        let fmt = by_token("altairhd").unwrap();
        let mut fs = mount(std::fs::read(&img).unwrap(), fmt);
        let mut compared = 0;
        for entry in std::fs::read_dir(&out).unwrap().flatten() {
            let theirs = std::fs::read(entry.path()).unwrap();
            let fname = entry.file_name().to_string_lossy().to_uppercase();
            let (n, e) = name_of(&fname);
            let Some(total) = fs.file_records(0, &n, &e) else {
                panic!("{fname}: cpmtools found it, we did not");
            };
            let mut ours = Vec::new();
            for rec in 0..total {
                match fs.read_record(0, &n, &e, rec).unwrap() {
                    Some(buf) => ours.extend_from_slice(&buf),
                    None => break,
                }
            }
            let common = ours.len().min(theirs.len());
            assert_eq!(
                &ours[..common],
                &theirs[..common],
                "{fname}: our bytes differ from cpmtools"
            );
            compared += 1;
        }
        assert!(compared > 20, "expected a full hard disk, compared {compared}");
        let _ = std::fs::remove_dir_all(&work);
    }

    /// CP/M 3 label and timestamp records sit in the directory with a user
    /// byte of 0x20/0x21.  They are not files and must not be listed.
    #[test]
    fn test_label_and_timestamp_records_are_skipped() {
        let fmt = by_token("ibm3740").unwrap();
        let mut img = blank(fmt);
        put_entry(&mut img, fmt, 0, 0x20, "LABEL.   ", 0, 0, &[]);
        put_entry(&mut img, fmt, 1, 0x21, "STAMP.   ", 0, 0, &[]);
        put_entry(&mut img, fmt, 2, 0, "REAL.COM", 0, 1, &[5]);
        let fs = mount(img, fmt);
        assert_eq!(fs.entries().len(), 1, "only the real file");
        assert_eq!(&fs.entries()[0].name[..4], b"REAL");
    }
    /// The gate on the Altair block mapping: read every file this reader can
    /// find on a real `DISK01.DSK` and require the bytes to match what the
    /// disk's own CP/M produced when it read the same files out over a virtual
    /// modem.
    ///
    /// The expected values are **hashes, not content**.  These files come from
    /// third-party images we do not redistribute, so a fixture of their bytes
    /// would be redistributing them; a SHA-256 proves the match and carries
    /// nothing.  The ground truth behind each hash was captured by
    /// `cpm::boot_machine::tests::test_capture_altair_ground_truth`.
    ///
    /// Eight files, 447 records, chosen to cover both sides of the track-6
    /// format change: `L80.COM`, `ED.COM` and `DUMP.COM` live in the
    /// boot-format tracks, the rest in the data tracks, and `MBASIC.COM` spans
    /// two directory entries so it also exercises the disk's stated EXM 0.
    ///
    /// Ignored: set `CPM_ALTAIR_IMAGE` to a real `DISK01.DSK` (337,568 bytes).
    #[test]
    #[ignore]
    fn test_altair_extraction_matches_the_booted_guest() {
        use sha2::{Digest, Sha256};
        /// (file, SHA-256 of what the guest's own CP/M sent out).
        const EXPECT: &[(&str, &str)] = &[
            ("DEMO.ASM", "b5fffdf431c9b9673e00b6f8e18c29ce772be8e0a45970f8def43e3e1f634bdb"),
            ("DEMO.PRN", "3b7c7ec0364ba3f8cdab74580bfb0d0adc8d473bae0d2515dd6a080a92b23b06"),
            ("DUMP.COM", "43d745e0eadb36d9fe9f1e2a927e9cb091622a74bb36c0d68afd7d21b6ca69b1"),
            ("ED.COM", "6c201ae1195bfcd216c0604a0cb6b15cf553e46db13eb57b3e0de9461aa4d84c"),
            ("L80.COM", "7407f61e7788660550ea0a12ba44794f9786235c0a58aafb6d6c4bc3329d2831"),
            ("MBASIC.COM", "29d957fc6899c24f6296a1662a27eca545d85ee3f7d70d2794c9d045d92ff157"),
            ("PIP.COM", "583dfebfb7e69372810f957527cb259f376c11369fa6703945cd1454a85b8707"),
            ("WM.HLP", "4a85e67caf3ac765d0f1c962c7bd5321ed1c7d3ae96e93147b582656cfd3fe5f"),
        ];
        let Ok(path) = std::env::var("CPM_ALTAIR_IMAGE") else {
            eprintln!("set CPM_ALTAIR_IMAGE to a real DISK01.DSK to run this");
            return;
        };
        let fmt = by_token("altair8").unwrap();
        let mut fs = mount(std::fs::read(&path).unwrap(), fmt);
        for (file, want) in EXPECT {
            let (n, e) = name_of(file);
            let got = fs
                .read_whole(0, &n, &e, 8 << 20)
                .unwrap()
                .unwrap_or_else(|| panic!("{file} is not on the disk"));
            let sum = format!("{:x}", Sha256::digest(&got));
            assert_eq!(&sum, want, "{file}: {} bytes, wrong content", got.len());
        }
        println!("{} files match the guest's own reading", EXPECT.len());
    }
    /// A write to an Altair image must leave a correct sector check byte, on
    /// both sides of the track-6 format change.
    ///
    /// This runs in CI, unlike the live test that boots a real disk, and it is
    /// the only cover for the boot-format half of the arithmetic — the guest
    /// never reads a boot-track record as a file, so a real disk cannot fail on
    /// that path even when it is wrong.
    ///
    /// Every sector the write touched is checked rather than a predicted few:
    /// a blank image is all `0xE5`, so any sector whose data is not is one we
    /// wrote, and that finds the directory records as well as the file's.
    #[test]
    fn test_altair_writes_refresh_the_sector_check_byte() {
        let fmt = by_token("altair8").unwrap();
        let mut fs = mount(blank(fmt), fmt);
        let (n, e) = name_of("CHK.DAT");
        fs.create(0, &n, &e).unwrap();
        // Enough records to run past the boot-format tracks.  The data area
        // starts on track 2 and the format changes at track 6, so four tracks
        // of 32 records is the first record that can land in data format.
        let last: u32 = 4 * 32 + 5;
        for rec in 0..=last {
            // Never 0xE5, so "differs from blank" means "we wrote it".
            fs.write_record(0, &n, &e, rec, &[(rec as u8).wrapping_add(1); 128]).unwrap();
        }

        let mut boot_side = 0;
        let mut data_side = 0;
        for phys in 0..fmt.total_records as u64 {
            let at = fmt.framing.record_offset(phys);
            let data: Vec<u8> = (0..128).map(|i| fs.peek(at + i)).collect();
            if data.iter().all(|&b| b == E5) {
                continue; // untouched
            }
            // Written out from the measurement, NOT from `sector_check` — a
            // test that asks the code under test where the byte goes agrees
            // with it however wrong it is, which is exactly what this one did
            // before a mutation showed it passing with the offset moved.
            let sector = phys * 137;
            let (check_at, also): (u64, &[u64]) = if phys / 32 < 6 {
                (sector + 132, &[])
            } else {
                (sector + 4, &[sector + 2, sector + 3, sector + 5, sector + 6])
            };
            let mut want = data.iter().fold(0u8, |a, &b| a.wrapping_add(b));
            for extra in also {
                want = want.wrapping_add(fs.peek(*extra));
            }
            assert_eq!(
                fs.peek(check_at),
                want,
                "physical record {phys} (track {}) has a stale check byte",
                phys / 32
            );
            if phys / 32 < 6 {
                boot_side += 1;
            } else {
                data_side += 1;
            }
        }
        assert!(boot_side > 0, "no boot-format sector was written");
        assert!(data_side > 0, "no data-format sector was written");

        // And the file still reads back, so refreshing the check byte did not
        // land on top of the record it protects.
        for rec in [0u32, 1, last] {
            assert_eq!(
                fs.read_record(0, &n, &e, rec).unwrap().unwrap(),
                [(rec as u8).wrapping_add(1); 128]
            );
        }
    }

    /// A disk that states its own EXM is believed over the standard
    /// derivation, and the whole point is the number of allocation slots one
    /// directory entry gets.  The Altair says 0 where the rule gives 1.
    #[test]
    fn test_stated_exm_overrides_the_derivation() {
        let alt = by_token("altair8").unwrap();
        assert_eq!(alt.exm, Some(0), "the Altair BIOS states EXM 0");
        let p = Params::derive(alt);
        assert_eq!(p.exm, 0);
        assert_eq!(p.map_slots, 8, "EXM 0 with 2K blocks uses eight of sixteen slots");
        // Without the override the same geometry would derive EXM 1.
        let mut derived = alt.clone();
        derived.exm = None;
        assert_eq!(Params::derive(&derived).exm, 1, "the standard rule gives 1 here");
        assert_eq!(Params::derive(&derived).map_slots, 16);
        // Every other format is unaffected — the override must be a no-op where
        // the disk agrees with the rule.
        for f in super::super::format::FORMATS.iter().filter(|f| f.exm.is_none()) {
            let mut forced = f.clone();
            forced.exm = Some(Params::derive(f).exm);
            assert_eq!(Params::derive(&forced).map_slots, Params::derive(f).map_slots);
        }
    }
}
