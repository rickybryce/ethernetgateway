//! The CP/M 2.2 filesystem inside a mounted disk image.
//!
//! This is the layer that turns "bytes at an offset" into "record 7 of
//! `STAT.COM`".  It reads the on-disk directory, threads a file's extents
//! together in order, and walks the allocation map to find the record asked
//! for.
//!
//! **Read path only, for now.**  Allocation, extent creation and erase come
//! next; until then a mounted image is a library you can run software from, not
//! one you can write to.
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
        let total_blocks = fmt.data_records() / records_per_block;
        // Block 0 is the directory, so the highest block number is one less
        // than the count.  `saturating_sub` keeps a nonsense format from
        // wrapping to 65535 usable blocks.
        let max_block = total_blocks.saturating_sub(1).min(u16::MAX as u32) as u16;
        let wide_blocks = max_block > 255;
        let map_slots = if wide_blocks { 8 } else { 16 };
        // One entry addresses `map_slots` blocks; how many 128-record extents
        // is that?
        let records_per_entry = map_slots as u32 * records_per_block;
        let exm = (records_per_entry / RECORDS_PER_EXTENT).max(1) - 1;
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
}

impl ImageFs {
    /// Mount `media` as `fmt`, reading the directory.
    ///
    /// Fails when the image is too short for the format's geometry — better a
    /// refusal at mount time, naming the mismatch, than a drive that lists
    /// files and then fails on every read.
    pub fn mount(mut media: Box<dyn Media>, fmt: &'static Format) -> std::io::Result<ImageFs> {
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
        let dir = Self::read_directory(&mut *media, fmt, &params)?;
        Ok(ImageFs { media, fmt, params, dir })
    }

    /// The format this image is mounted as.
    pub fn format(&self) -> &'static Format {
        self.fmt
    }

    /// Derived geometry.
    pub fn params(&self) -> &Params {
        &self.params
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

    /// Every live directory entry.
    pub fn entries(&self) -> &[DirSlot] {
        &self.dir
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

    /// Blocks currently allocated to files, for a free-space report.
    ///
    /// Counts distinct block numbers rather than summing allocation slots: a
    /// cross-linked disk (the same block claimed twice) would otherwise report
    /// more space in use than the disk has.
    pub fn used_blocks(&self) -> u32 {
        let mut seen = vec![false; self.params.max_block as usize + 2];
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
    let blocks = if params.wide_blocks {
        raw[16..32]
            .chunks_exact(2)
            .map(|p| u16::from_le_bytes([p[0], p[1]]))
            .collect()
    } else {
        raw[16..32].iter().map(|&b| b as u16).collect()
    };
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
        for (slot, &b) in raw[16..32].iter_mut().zip(blocks) {
            *slot = b;
        }
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
        ImageFs::mount(Box::new(MemMedia::new(img)), fmt).unwrap()
    }

    #[test]
    fn test_params_for_the_measured_formats() {
        let ibm = Params::derive(by_token("ibm3740").unwrap());
        assert_eq!(ibm.records_per_block, 8, "1K blocks");
        assert_eq!(ibm.max_block, 242, "243 blocks on an 8\" SSSD");
        assert!(!ibm.wide_blocks, "fits in 8-bit block numbers");
        assert_eq!(ibm.exm, 0, "1K blocks, one extent per entry");
        assert_eq!(ibm.dir_records, 16, "64 entries, 4 per record");

        let alt = Params::derive(by_token("altair8").unwrap());
        assert_eq!(alt.records_per_block, 16, "2K blocks");
        assert_eq!(alt.max_block, 149, "150 blocks on an Altair 8\"");
        assert!(!alt.wide_blocks);
        assert_eq!(alt.exm, 1, "2K blocks, two extents per entry");
        assert_eq!(alt.dir_records, 32, "128 entries, 4 per record");
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
    fn test_extent_mask_applies_on_2k_blocks() {
        let fmt = by_token("altair8").unwrap();
        let mut img = blank(fmt);
        // Entry with extent 1 and EXM 1: covers records 0..(128 + rc).
        put_entry(&mut img, fmt, 0, 0, "WIDE.BIN", 1, 4, &[3, 4]);
        put_block_record(&mut img, fmt, 3, 0, b"rec0");
        // Block 4 is the second 2K block => records 16..31 of the file.
        put_block_record(&mut img, fmt, 4, 0, b"rec16");

        let mut fs = mount(img, fmt);
        let (n, e) = name_of("WIDE.BIN");
        assert_eq!(fs.file_records(0, &n, &e), Some(132), "1*128 + rc 4");
        assert_eq!(&fs.read_record(0, &n, &e, 0).unwrap().unwrap()[..4], b"rec0");
        assert_eq!(&fs.read_record(0, &n, &e, 16).unwrap().unwrap()[..5], b"rec16");
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
        match ImageFs::mount(Box::new(MemMedia::new(short)), fmt) {
            Ok(_) => panic!("a truncated image must not mount"),
            Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::InvalidData),
        }
    }

    /// Read a real image end to end and require every text file to come back
    /// clean.  This is the check that catches a wrong data offset or skew
    /// table, neither of which disturbs the file *listing* — the failure mode
    /// that made the Altair format hard to pin down in the first place.
    ///
    /// Ignored because it needs an image on disk: point `CPM_IMAGE_DIR` at a
    /// directory holding `DISK01.DSK` and `TDISK01.DSK` to run it.
    #[test]
    #[ignore]
    fn test_real_images_read_clean() {
        let dir = match std::env::var("CPM_IMAGE_DIR") {
            Ok(d) => std::path::PathBuf::from(d),
            Err(_) => {
                eprintln!("set CPM_IMAGE_DIR to run this test");
                return;
            }
        };
        for (file, token, want) in [("DISK01.DSK", "altair8", "ED      COM")] {
            let path = dir.join(file);
            let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("{path:?}: {e}"));
            let fmt = by_token(token).unwrap();
            let mut fs = mount(bytes, fmt);

            let names: Vec<String> = fs
                .entries()
                .iter()
                .map(|e| {
                    String::from_utf8_lossy(&e.name).to_string()
                        + &String::from_utf8_lossy(&e.ext)
                })
                .collect();
            assert!(
                names.iter().any(|n| n == want),
                "{file}: expected {want:?} in the directory, got {names:?}"
            );

            // Every text file must decode with no control bytes other than the
            // ones CP/M text legitimately uses.
            let text_exts = ["ASM", "PRN", "TXT", "SUB"];
            let mut checked = 0;
            let targets: Vec<(u8, [u8; 8], [u8; 3])> = fs
                .entries()
                .iter()
                .filter(|e| text_exts.contains(&String::from_utf8_lossy(&e.ext).trim()))
                .map(|e| (e.user, e.name, e.ext))
                .collect();
            for (user, n, e) in targets {
                let total = fs.file_records(user, &n, &e).unwrap_or(0);
                for rec in 0..total {
                    let Some(buf) = fs.read_record(user, &n, &e, rec).unwrap() else {
                        break;
                    };
                    for &c in buf.iter() {
                        assert!(
                            (0x20..0x7F).contains(&c) || matches!(c, b'\r' | b'\n' | 9 | 0x1A | 0),
                            "{file}: {} record {rec} has byte {c:#04x} — wrong data offset or skew",
                            String::from_utf8_lossy(&n)
                        );
                    }
                }
                checked += 1;
            }
            assert!(checked > 0, "{file}: no text files found to verify");
        }
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
}
