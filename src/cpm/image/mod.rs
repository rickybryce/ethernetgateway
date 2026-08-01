//! Mounting a CP/M disk image (`.dsk`) as an emulated drive.
//!
//! When a drive has an image mounted, the emulator reads and writes the CP/M
//! filesystem *inside that image* instead of the drive's host folder under
//! `transfer_dir/CPM/`.  The folder's files are untouched and come back the
//! moment the image is unmounted.
//!
//! The work splits in two:
//!
//! * [`format`] — geometry.  Where the 128-byte CP/M records sit inside the
//!   file, and the CP/M parameters (block size, directory size, sector skew)
//!   that describe the filesystem laid over them.
//!
//! * [`media`] — the byte store, and the one place every access is
//!   bounds-checked against the real length of the file.
//!
//! * [`fs`] — the filesystem itself: directory entries, extents and allocation
//!   blocks.  Read-only so far; allocation and erase come next, after which it
//!   is presented through the same record-oriented API that the folder-backed
//!   [`super::fs::CpmFs`] already offers the BDOS layer.

// Staged build: the read path and its tests are complete, but nothing outside
// this module calls it yet — `CpmFs` gains the per-drive backend switch, and
// the config/UI layers gain the mount controls, in the steps after this one.
// CI treats warnings as errors, so the allow keeps the tree green in between.
// **Remove this once the mount path is wired up**; it would otherwise go on
// hiding genuinely dead code in here forever.
#![allow(dead_code)]

pub mod format;
pub mod fs;
pub mod media;
