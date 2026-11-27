//! The Xbox I/O namespace over host storage: disc, hard disk partitions, files and directories.
//!
//! This is the storage half of the kernel's I/O manager, kept apart so it is
//! pure and testable. It deals in Rust types, byte buffers and NTSTATUS
//! values; handles, guest memory, asynchronous completion and waits belong to
//! `xb-kernel`, which wraps it with the `Nt*` and `Io*` exports.
//!
//! - [`FileSystem`] is the namespace: symbolic links (`\??\D:`), the devices
//!   (`\Device\CdRom0`, `\Device\Harddisk0\PartitionN`) and the volumes
//!   mounted on them. It resolves a name and opens it.
//! - [`File`] is an open file, directory or device: reads and writes at an
//!   offset, the information classes as guest bytes, directory enumeration.
//! - [`Volume`] and [`VolumeFile`] are what a mounted file system implements.
//!   [`HostDisk`] gives the hard disk's partitions over a host folder,
//!   [`DiscVolume`] the disc over a [`DiscTree`] (implemented elsewhere over
//!   the XDVDFS parser), [`MemVolume`] a partition in memory for tests.
//! - [`DeviceTiming`] says how long the console's drives would take.
//!
//! `docs/hw/io-namespace.md` describes the behavior and its sources.
#![forbid(unsafe_code)]

mod disc;
mod fs;
mod host;
pub mod ioctl;
mod mem;
pub mod name;
mod namespace;
mod status;
mod sys;
mod timing;
pub mod types;
mod volume;

#[cfg(test)]
mod tests;

pub use disc::{
    DISC_SECTOR, DISC_TOTAL_SECTORS, DiscNode, DiscTree, DiscVolume, MemDisc, RawBlocks,
    disc_lacks_language, language_fallback,
};
pub use fs::{DeviceRef, File, FileState, FileSystem, OpenError, Root};
pub use host::{
    DEFAULT_CLUSTER_SECTORS, HDD_BYTES, HDD_SECTOR, HostDisk, PARTITION_NUMBERS, PARTITIONS,
    RAW_KEEP,
};
pub use mem::MemVolume;
pub use namespace::{Device, DeviceKind, LinkTable, MAX_LINKS, device_of, unknown_device_status};
pub use status::NtStatus;
pub use timing::{
    DeviceRate, DeviceTiming, FS_PAGE, SYNC_LATER_MIN, TimingMode, title_timing_mode,
};
pub use types::{CreateAction, CreateDisposition};
pub use volume::{
    DirEntry, FileFacts, LISTING_MAX, LISTING_NAME_MAX, OpenParams, SortKey, Volume, VolumeFile,
};

/// Locks a mutex, taking the data as it is if a thread panicked holding it:
/// the state here stays consistent between statements, and a poisoned lock
/// must not take every later file operation down with it.
fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}
