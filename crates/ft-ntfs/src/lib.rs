//! # ft-ntfs
//!
//! Read-only raw NTFS access for FerrumTree.
//!
//! * [`boot`]    - NTFS boot sector (`$Boot`) parsing.
//! * [`runlist`] - mapping-pairs (data runs) decoder.
//! * [`record`]  - `FILE` record parsing: Update Sequence Array fixup, attribute walk,
//!   extraction of names, parent references, logical and on-disk sizes.
//! * [`volume`]  - `\\.\X:` handle, aligned buffers, overlapped (async) reads.
//! * [`scan`]    - the pipelined MFT scanner: one I/O thread keeps several large reads in
//!   flight while N parser threads decode records directly into a preallocated table.
//!
//! The volume handle is opened with `GENERIC_READ` only, so the scanned volume can never be
//! modified. The crate writes nothing.

pub mod boot;
mod bytes;
pub mod error;
pub mod record;
pub mod runlist;
#[cfg(windows)]
pub mod scan;
#[cfg(windows)]
pub mod volume;

pub use boot::BootSector;
pub use error::{Error, Result};
pub use record::{NameEntry, ParseStats, Rec, Sink, rflags};
pub use runlist::Extent;

/// MFT record number of the root directory (`.`).
pub const ROOT_RECORD: u32 = 5;
