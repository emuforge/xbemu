//! Disc image and executable formats: CCI, XDVDFS (XISO), XBE.
//!
//! Everything here parses untrusted input. Every length and offset taken from
//! a file is bounds checked, and malformed data is an [`Error`], never a
//! panic. Nothing is loaded into guest memory: the XBE parser describes the
//! image and the loader in a higher layer copies it.
//!
//! The pieces stack:
//!
//! - [`block::BlockSource`] is random access to a run of bytes (a file, a
//!   decompressed image, a window of another source).
//! - [`disc::open`] turns a path into the block source of the plain disc
//!   image, going through [`cci::CciImage`] when the file is a CCI.
//! - [`xdvdfs::Xdvdfs`] reads the file system on that image.
//! - [`xbe::Xbe`] parses an executable read through any block source.
#![forbid(unsafe_code)]

pub mod block;
pub mod cci;
pub mod disc;
mod error;
pub mod xbe;
pub mod xdvdfs;

pub use block::{BlockSource, FileSource, MemSource, SubSource};
pub use error::{Error, Result};

/// Bytes in a disc sector. XDVDFS addresses data in sectors of this size and a
/// CCI compresses one sector per block.
pub const SECTOR_SIZE: u32 = 2048;

/// Little-endian `u16` at `off` of `b`, or `None` when it does not fit.
pub(crate) fn le16(b: &[u8], off: usize) -> Option<u16> {
    let s = b.get(off..off.checked_add(2)?)?;
    Some(u16::from_le_bytes([s[0], s[1]]))
}

/// Little-endian `u32` at `off` of `b`, or `None` when it does not fit.
pub(crate) fn le32(b: &[u8], off: usize) -> Option<u32> {
    let s = b.get(off..off.checked_add(4)?)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// Little-endian `u64` at `off` of `b`, or `None` when it does not fit.
pub(crate) fn le64(b: &[u8], off: usize) -> Option<u64> {
    let lo = le32(b, off)?;
    let hi = le32(b, off.checked_add(4)?)?;
    Some(u64::from(lo) | u64::from(hi) << 32)
}

#[cfg(test)]
pub(crate) mod testutil;
