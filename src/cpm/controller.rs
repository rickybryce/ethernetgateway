//! The seam between a booted machine and the disk controller it carries.
//!
//! There is one controller today — the MITS 88-DCDD floppy board — and this
//! module exists because there is about to be more than one. The plan for the
//! 88-HDSK hard disk, the Tarbell and the Cromemco says to do this *before* the
//! second controller rather than after, and the reason is arithmetic: adapting
//! one implementation to a trait is a mechanical change, while unpicking three
//! interleaved ones is not.
//!
//! # What a controller is, from the machine's point of view
//!
//! Very little, deliberately:
//!
//! * it **claims a range of ports** and answers reads and writes on them;
//! * it says whether it can **carry an image of a given size**, which is how a
//!   `.dsk` is matched to hardware — the boards took different media and the
//!   file length is the only thing available before anything is running;
//! * when it needs bytes off the medium it asks the machine for a **byte range**
//!   rather than a track and a sector.
//!
//! That last one is the important choice. The 88-DCDD thinks in 137-byte
//! sectors addressed by track and sector; the 88-HDSK thinks in 256-byte
//! sectors addressed by cylinder, head and sector, and reaches them through a
//! command protocol rather than a rotating position register. Nothing above the
//! controller should have to know either. So the controller does its own
//! address arithmetic and hands down an offset, and the machine's job shrinks
//! to "copy these bytes out of, or into, that image" — which is the same job
//! for every board there will ever be.
//!
//! # What is deliberately *not* here
//!
//! Cold-starting a disk. The 88-DCDD bootstrap is the sequence a PROM would
//! run, and the hard disk's is a different sequence entirely; folding both
//! behind one method before the second one has been written would be inventing
//! an abstraction from a single example. [`crate::cpm::boot`] stays as it is
//! until there is a second bootstrap to generalise against.

/// What a controller needs the machine to do with the medium.
///
/// Byte ranges rather than tracks and sectors — see the module comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostRequest {
    /// Nothing to do.
    None,
    /// Fill the controller's buffer for `drive` from `offset`.
    Read { drive: u8, offset: u64, len: usize },
    /// Write the controller's buffer for `drive` back at `offset`.
    Write { drive: u8, offset: u64, len: usize },
}

/// A disk controller a booted machine can carry.
pub trait Controller: Send {
    /// What to call it, in a message or in the generated readme.
    fn name(&self) -> &'static str;

    /// Does this controller answer at `port`?
    ///
    /// Asked before every port access the machine cannot handle itself, so a
    /// second controller costs nothing but its own answer here — which is the
    /// whole point of the exercise.
    fn owns_port(&self, port: u8) -> bool;

    /// Read one of its ports.
    fn port_in(&mut self, port: u8) -> (u8, HostRequest);

    /// Write one of its ports.
    fn port_out(&mut self, port: u8, value: u8) -> HostRequest;

    /// Can this controller carry an image this size, and what is that medium
    /// called?
    ///
    /// `None` means "not mine", which is how an image is matched to hardware.
    /// Implementations are expected to allow a short trailer: several images in
    /// circulation carry a few bytes past the last sector, and rejecting those
    /// on an exact match cost seven perfectly good disks once already.
    fn accepts(&self, image_len: u64) -> Option<&'static str>;

    /// Put a disk of `image_len` bytes in a drive. `Err` if it will not fit
    /// this controller.
    fn insert(&mut self, drive: u8, image_len: u64, read_only: bool) -> Result<(), String>;

    /// Hand the controller the bytes a [`HostRequest::Read`] asked for.
    fn buffer_loaded(&mut self, drive: u8, bytes: &[u8]);

    /// The bytes a [`HostRequest::Write`] wants written back.
    fn buffer(&self, drive: u8) -> Option<&[u8]>;

    /// The 88-DCDD behind this controller, when it is one.
    ///
    /// An escape hatch, and an honest one. Cold-starting a disk is still
    /// board-specific: [`crate::cpm::boot`] drives the floppy controller's port
    /// state machine directly, because that is the sequence a PROM would run,
    /// and the hard disk's is a different sequence reached through a command
    /// protocol. Folding both behind a trait method now would mean designing
    /// the abstraction from one example — which is how the wrong abstraction
    /// gets made. This becomes a real trait method when there is a second
    /// bootstrap to generalise against; until then a controller that cannot
    /// boot simply answers `None` and the machine reports that it will not.
    fn as_dcdd(&mut self) -> Option<&mut crate::cpm::dcdd::Dcdd> {
        None
    }

    /// Where this controller's first-stage boot program lives, and where its
    /// PROM would put it: `(byte offset in the image, load address)`.
    ///
    /// `None` for a board whose cold start is more than "read one sector and
    /// jump" — the 88-DCDD's is, because its PROM drives the port state machine
    /// through a rotating sector counter, and that stays in
    /// [`crate::cpm::boot`] behind [`Controller::as_dcdd`].
    ///
    /// The 88-HDSK's really is that simple, and the disk says so itself: its
    /// boot loader source, carried in plain ASCII on HDSK03, records that "the
    /// hard disk bootloader ROM (HDBL) loads this program into memory at
    /// address zero", and that program then loads CP/M through the controller
    /// on its own.
    fn boot_program(&self) -> Option<(u64, u16)> {
        None
    }

    /// How many times a guest has polled for something that never arrived.
    ///
    /// Every controller here has *some* wait a guest can sit in forever — a
    /// sector that never comes round, a ready flag that never sets — and a
    /// guest stuck in one looks exactly like a crashed CPU. Reporting it is
    /// what tells the two apart, and the 88-DCDD bring-up needed it.
    fn stuck_polls(&self) -> u32;
}
