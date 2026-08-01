//! The byte store under a mounted image.
//!
//! A mounted image is a file on the host, but the filesystem code above has no
//! business knowing that: it asks for bytes at an offset and gets them.  The
//! indirection buys two things — unit tests that build an image in memory and
//! never touch the disk, and a single place where every read is bounds-checked
//! against the real length of the file.
//!
//! That bounds check is the point.  A truncated or lying image is the ordinary
//! case here (someone copies half a `.dsk`, or a format's geometry does not
//! match the file), and a read past the end must surface as a CP/M read error,
//! never as a panic or as whatever bytes happened to follow.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};

/// A seekable byte store holding a disk image.
pub trait Media: Send {
    /// Total length in bytes.
    fn len(&self) -> u64;

    /// Fill `buf` from `offset`.  Fails if the range runs past the end.
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<()>;

    /// Write `data` at `offset`.  Fails if the range runs past the end — a
    /// mounted image is a fixed-size medium, exactly like the floppy it stands
    /// for, so growing it is not a thing that can happen.
    fn write_at(&mut self, offset: u64, data: &[u8]) -> std::io::Result<()>;

    /// Push any buffered writes to the host.
    fn flush(&mut self) -> std::io::Result<()>;

    /// True when the store holds no bytes at all.  Present because a type with
    /// `len` and no `is_empty` is a lint, and because "is this image empty?" is
    /// a question the mount UIs will ask.
    #[allow(dead_code)]
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The error every out-of-range access produces.
fn out_of_range(offset: u64, want: usize, len: u64) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::UnexpectedEof,
        format!("disk image access at {offset}+{want} runs past the end ({len} bytes)"),
    )
}

/// Shared bounds check: does `offset .. offset+want` fit inside `len`?
fn in_range(offset: u64, want: usize, len: u64) -> bool {
    match offset.checked_add(want as u64) {
        Some(end) => end <= len,
        None => false,
    }
}

/// An image held in memory.  Used by the tests, and by nothing else — a real
/// mount is always file-backed so two sessions see each other's writes.
#[cfg(test)]
pub struct MemMedia {
    bytes: Vec<u8>,
}

#[cfg(test)]
impl MemMedia {
    pub fn new(bytes: Vec<u8>) -> MemMedia {
        MemMedia { bytes }
    }

    /// Borrow the whole image, for tests that want to inspect raw bytes.
    #[cfg(test)]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

#[cfg(test)]
impl Media for MemMedia {
    fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
        if !in_range(offset, buf.len(), self.len()) {
            return Err(out_of_range(offset, buf.len(), self.len()));
        }
        let start = offset as usize;
        buf.copy_from_slice(&self.bytes[start..start + buf.len()]);
        Ok(())
    }

    fn write_at(&mut self, offset: u64, data: &[u8]) -> std::io::Result<()> {
        if !in_range(offset, data.len(), self.len()) {
            return Err(out_of_range(offset, data.len(), self.len()));
        }
        let start = offset as usize;
        self.bytes[start..start + data.len()].copy_from_slice(data);
        Ok(())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// An image backed by a host file.
///
/// The length is captured at open time and never re-read: the geometry of a
/// mounted disk is fixed for the life of the mount, and a file that changes
/// size underneath us is a corruption we would rather refuse than follow.
pub struct FileMedia {
    file: File,
    len: u64,
}

impl FileMedia {
    /// Open an image for reading and writing.
    pub fn open(path: &std::path::Path, read_only: bool) -> std::io::Result<FileMedia> {
        let file = File::options()
            .read(true)
            .write(!read_only)
            .open(path)?;
        let len = file.metadata()?.len();
        Ok(FileMedia { file, len })
    }
}

impl Media for FileMedia {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
        if !in_range(offset, buf.len(), self.len) {
            return Err(out_of_range(offset, buf.len(), self.len));
        }
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.read_exact(buf)
    }

    fn write_at(&mut self, offset: u64, data: &[u8]) -> std::io::Result<()> {
        if !in_range(offset, data.len(), self.len) {
            return Err(out_of_range(offset, data.len(), self.len));
        }
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(data)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mem_round_trip() {
        let mut m = MemMedia::new(vec![0u8; 256]);
        m.write_at(64, &[1, 2, 3, 4]).unwrap();
        let mut buf = [0u8; 4];
        m.read_at(64, &mut buf).unwrap();
        assert_eq!(buf, [1, 2, 3, 4]);
    }

    #[test]
    fn test_read_past_end_is_an_error_not_a_panic() {
        let mut m = MemMedia::new(vec![0u8; 128]);
        let mut buf = [0u8; 128];
        assert!(m.read_at(1, &mut buf).is_err(), "127 bytes left, 128 wanted");
        assert!(m.read_at(128, &mut buf).is_err());
        assert!(m.read_at(0, &mut buf).is_ok());
    }

    #[test]
    fn test_write_past_end_is_refused() {
        let mut m = MemMedia::new(vec![0u8; 128]);
        assert!(m.write_at(120, &[0u8; 16]).is_err());
        // and the refusal left nothing behind
        assert_eq!(m.bytes()[120..], [0u8; 8]);
    }

    /// An offset near `u64::MAX` must not wrap into a valid-looking range.
    #[test]
    fn test_offset_overflow_is_refused() {
        let mut m = MemMedia::new(vec![0u8; 128]);
        let mut buf = [0u8; 8];
        assert!(m.read_at(u64::MAX - 2, &mut buf).is_err());
        assert!(m.write_at(u64::MAX - 2, &[0u8; 8]).is_err());
    }

    #[test]
    fn test_file_media_round_trip() {
        let dir = std::env::temp_dir().join("egw_media_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("img.dsk");
        std::fs::write(&path, vec![0u8; 512]).unwrap();

        let mut m = FileMedia::open(&path, false).unwrap();
        assert_eq!(m.len(), 512);
        m.write_at(256, b"HELLO").unwrap();
        m.flush().unwrap();
        let mut buf = [0u8; 5];
        m.read_at(256, &mut buf).unwrap();
        assert_eq!(&buf, b"HELLO");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_read_only_open_refuses_writes() {
        let dir = std::env::temp_dir().join("egw_media_test_ro");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("img.dsk");
        std::fs::write(&path, vec![0u8; 256]).unwrap();

        let mut m = FileMedia::open(&path, true).unwrap();
        assert!(m.read_at(0, &mut [0u8; 16]).is_ok(), "reads still work");
        assert!(m.write_at(0, b"nope").is_err(), "opened without write access");

        let _ = std::fs::remove_file(&path);
    }
}
