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
    /// Set `count` runs of `chunk` bytes, `stride` apart, to `byte`.
    ///
    /// What a format is: one whole recording surface erased. It is strided
    /// because a surface is not contiguous in an image — one head's sectors sit
    /// once per cylinder, a cylinder apart — and it is expressed as arithmetic
    /// the controller does rather than as a list of ranges, so this stays a
    /// plain `Copy` value like the other two.
    Fill { drive: u8, offset: u64, chunk: usize, stride: u64, count: usize, byte: u8 },
}

/// What a controller's PROM would do with an image, when asked to cold-start it.
///
/// Three answers rather than an `Option`, because the two ways of not booting
/// are different facts and a caller has to tell a user which one happened. They
/// were conflated once: a hard disk with no boot program reported "this disk is
/// on a controller that cannot cold-start one yet", which is untrue and sends
/// the reader looking for missing code rather than at their disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColdStart {
    /// This board's cold start is its own sequence, not a run of sectors.
    ///
    /// The 88-DCDD's is: its PROM drives the port state machine against a
    /// rotating sector counter, which stays in [`crate::cpm::boot`] behind
    /// [`Controller::as_dcdd`].
    Own,
    /// Load `len` bytes from `offset` at `load`, and enter there.
    Program { offset: u64, len: usize, load: u16 },
    /// This board loads a program the disk names, and this disk names none.
    NoProgram,
}

/// One kind of medium a controller takes.
///
/// Exists so that "what can this machine boot" has a single answer with a
/// single owner. The generated readme used to build its own list from the
/// floppy's geometry table, which is why it told operators that only 88-DCDD
/// floppies boot for as long as the hard disk had been booting them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Medium {
    /// Bytes in a full image of it.
    pub bytes: u64,
    /// What to call it, to a person.
    pub label: &'static str,
    /// The largest trailer past the last sector still taken as this medium.
    ///
    /// Always less than one sector: past that the size no longer identifies the
    /// medium, and accepting it would mean reading a disk we have not actually
    /// recognised.
    pub trailer: u64,
    /// How the size is made up, for a readme — "77 tracks x 32 sectors x 137".
    pub shape: String,
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

    /// Every medium this board takes.
    fn media(&self) -> Vec<Medium>;

    /// Can this controller carry an image this size, and what is that medium
    /// called?
    ///
    /// `None` means "not mine", which is how an image is matched to hardware.
    ///
    /// **Derived from [`Controller::media`], and not overridden.** The trailer
    /// allowance is the reason: it is easy to write an exact-length test here
    /// and not notice, because every image on hand is exact — and then a disk
    /// that some copying tool padded is refused as "not a disk this machine can
    /// carry". That happened to the floppy (seven disks, including both CP/M 3
    /// images) and then happened again to the hard disk. Stating the medium once
    /// and computing the test from it is what stops a third time.
    fn accepts(&self, image_len: u64) -> Option<&'static str> {
        self.media().into_iter().find_map(|m| {
            (image_len >= m.bytes && image_len - m.bytes <= m.trailer).then_some(m.label)
        })
    }

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

    /// What this controller's PROM would load out of `image`, and from where.
    ///
    /// The image is passed in because a controller knows its own on-disk
    /// conventions and the machine does not. The 88-HDSK needs that: the
    /// location is not fixed but recorded in the disk's own volume label, which
    /// is how one bootstrap serves both the CP/M disks and the Disk BASIC ones
    /// that live somewhere else entirely.
    fn cold_start(&self, _image: &[u8]) -> ColdStart {
        ColdStart::Own
    }

    /// How many times a guest has polled for something that never arrived.
    ///
    /// Every controller here has *some* wait a guest can sit in forever — a
    /// sector that never comes round, a ready flag that never sets — and a
    /// guest stuck in one looks exactly like a crashed CPU. Reporting it is
    /// what tells the two apart, and the 88-DCDD bring-up needed it.
    fn stuck_polls(&self) -> u32;
}
