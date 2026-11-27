//! The interface between the namespace and a mounted volume.
//!
//! A [`Volume`] is one device's file system: the disc, a hard disk partition,
//! the raw disk. It opens paths inside itself and hands back a
//! [`VolumeFile`], the storage behind one open handle. The rules every volume
//! shares (end-of-file clipping, positions, directory enumeration, the
//! information classes' layouts) live above this interface in
//! [`File`](crate::File); a volume answers only what depends on its storage.

use std::any::Any;
use std::path::PathBuf;

use crate::namespace::DeviceKind;
use crate::status::NtStatus;
use crate::types::{CreateAction, CreateDisposition, FileFsSizeInformation};

/// The parameters of an open that reach the volume.
///
/// `AllocationSize` and `FileAttributes` of `NtCreateFile` are not here: the
/// reference ignores both.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpenParams {
    /// `DesiredAccess` ([`access`](crate::types::access) bits).
    pub desired_access: u32,
    /// `ShareAccess` ([`share`](crate::types::share) bits).
    pub share_access: u32,
    /// What to do if the file exists or does not.
    pub disposition: CreateDisposition,
    /// `CreateOptions` ([`options`](crate::types::options) bits).
    pub options: u32,
}

impl OpenParams {
    /// Parameters of `NtOpenFile`: an existing file.
    #[must_use]
    pub const fn open(desired_access: u32, share_access: u32, options: u32) -> Self {
        Self { desired_access, share_access, disposition: CreateDisposition::Open, options }
    }

    /// Parameters of `NtCreateFile`.
    #[must_use]
    pub const fn create(
        desired_access: u32,
        share_access: u32,
        disposition: CreateDisposition,
        options: u32,
    ) -> Self {
        Self { desired_access, share_access, disposition, options }
    }
}

/// One entry of a directory's listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    /// The name, as the console sees it.
    pub name: String,
    /// The file's size; 0 for a directory on the hard disk.
    pub size: u64,
    /// A directory.
    pub is_dir: bool,
}

/// Names this long or longer are left out of directory listings, as the
/// reference's fixed entries leave them out.
pub const LISTING_NAME_MAX: usize = 64;

/// Entries a directory listing holds at most. Super Monkey Ball Deluxe's
/// `media\stage_eu` has 2538 files and needs them all.
pub const LISTING_MAX: usize = 1 << 20;

/// Times, size and attributes of an open file: what the information classes
/// are filled from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileFacts {
    /// Creation time (FILETIME).
    pub creation_time: u64,
    /// Last access time.
    pub last_access_time: u64,
    /// Last write time.
    pub last_write_time: u64,
    /// Change time.
    pub change_time: u64,
    /// Size in bytes; 0 for a directory.
    pub size: u64,
    /// Attribute bits.
    pub attributes: u32,
}

/// What the file system's control block says about where a file is, which
/// XAPI's `XGetFilePhysicalSortKey` reads out of the `FILE_OBJECT` (the
/// kernel lays the block out: GDFX keeps the start sector at +0, FATX flags at
/// +0 with bit 0 meaning "no key" and the first cluster at +0x1C).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKey {
    /// A disc file's first sector.
    Gdfx {
        /// The sector, as on the pressed disc.
        start_sector: u32,
    },
    /// A hard disk file's first cluster (the host's inode number: host files
    /// have no clusters, and the inode orders them stably).
    Fatx {
        /// The cluster stand-in.
        first_cluster: u32,
    },
    /// A directory or a device: no key.
    None,
}

/// The storage behind one open handle.
///
/// Implementations are `Send + Sync` and take `&self`: one file may be read
/// from the guest CPU thread and from I/O workers at once, at different
/// offsets, without one read waiting for another (positional I/O). Dropping
/// the value closes the handle: a delete-on-close takes effect when the last
/// handle on the file is dropped.
pub trait VolumeFile: Send + Sync {
    /// The concrete type, for a volume to recognise its own files.
    fn as_any(&self) -> &dyn Any;

    /// A directory (the disc's and a partition's root included).
    fn is_dir(&self) -> bool;

    /// The device itself rather than a file on it: the disc's root as
    /// `\Device\CdRom0`, the raw disk `Partition0`.
    fn is_volume(&self) -> bool {
        false
    }

    /// Reads and writes of this handle address raw sectors (the raw disk, a
    /// cache partition's root, the disc when a raw reader is attached)
    /// instead of file data. [`read_at`](Self::read_at) then fills the whole
    /// buffer, with zeros where nothing is stored.
    fn is_raw(&self) -> bool {
        false
    }

    /// The file's size in bytes; 0 for directories and devices.
    fn size(&self) -> u64;

    /// Reads into `buf` at `offset` and returns the bytes read. The caller
    /// has clipped the request to the file's size, so fewer bytes than asked
    /// is a failure it reports as `STATUS_DEVICE_DATA_ERROR`.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, NtStatus>;

    /// Writes `data` at `offset`, growing the file as needed, and returns the
    /// bytes written. The caller has checked the handle's access.
    fn write_at(&self, offset: u64, data: &[u8]) -> Result<usize, NtStatus>;

    /// Sets the file's size (`FileEndOfFileInformation`).
    fn set_end_of_file(&self, size: u64) -> NtStatus;

    /// Writes the file's data through to the host's storage.
    fn flush(&self) -> NtStatus;

    /// Times, size and attributes.
    fn facts(&self) -> FileFacts;

    /// `FileInternalInformation`'s index: one number per file on its volume.
    fn index_number(&self) -> u64;

    /// Where the file is, for `XGetFilePhysicalSortKey`.
    fn sort_key(&self) -> SortKey;

    /// Sets the last access and last write times (`FileBasicInformation`);
    /// `None` keeps one.
    fn set_times(&self, last_access: Option<u64>, last_write: Option<u64>) -> NtStatus;

    /// `FileDispositionInformation`: marks the file to go at its last close,
    /// or takes the mark back. A directory is removed at once.
    fn set_delete_on_close(&self, delete: bool) -> NtStatus;

    /// True while the file is marked to go at its last close.
    fn delete_pending(&self) -> bool {
        false
    }

    /// A snapshot of those of a directory's entries whose names `keep`
    /// takes, in the volume's order. Only the kept ones are looked up, so a
    /// listing for one name does not cost a lookup of every file of a host
    /// folder (Armed and Dangerous looks each sound of a level up by name in
    /// its cache partition as the level loads).
    fn list(&self, keep: &dyn Fn(&str) -> bool) -> Vec<DirEntry>;

    /// Data or size changed through this handle.
    fn wrote(&self) -> bool {
        false
    }

    /// The file should be synced to the host's storage before it closes
    /// (written, and not on a cache drive).
    fn sync_on_close(&self) -> bool {
        false
    }
}

/// One device's file system.
pub trait Volume: Send + Sync {
    /// The concrete type, for a rename across volumes of one hard disk.
    fn as_any(&self) -> &dyn Any;

    /// Disc or hard disk.
    fn device_kind(&self) -> DeviceKind;

    /// Opens `rel`, a path inside the volume: `""` for its root, else
    /// components each led by `\`, already canonical. Returns the file and
    /// what the open did.
    fn open(
        &self,
        rel: &str,
        params: &OpenParams,
    ) -> Result<(Box<dyn VolumeFile>, CreateAction), NtStatus>;

    /// `NtDeleteFile`: deletes the file or empty directory at `rel`. A file
    /// with handles open goes at its last close.
    fn delete(&self, rel: &str) -> NtStatus;

    /// `FileRenameInformation`: moves `file`, which this volume opened, to
    /// `dest_rel` on `dest` (this volume or another).
    fn rename(
        &self,
        file: &dyn VolumeFile,
        dest: &dyn Volume,
        dest_rel: &str,
        replace_if_exists: bool,
    ) -> NtStatus;

    /// The volume's size and free space.
    fn size_information(&self) -> FileFsSizeInformation;

    /// The host file behind `rel`, if the volume keeps files on the host and
    /// it exists as a regular file.
    fn host_file(&self, rel: &str) -> Option<PathBuf> {
        let _ = rel;
        None
    }
}

/// True if a new open's share access conflicts with a handle already open:
/// either wants what the other does not share (`IoCheckShareAccess`). A
/// handle granted nothing takes no part.
///
/// Mission: Impossible - Operation Surma holds each level it copies to the
/// cache open unshared until the copy is complete, and reads the level from
/// the disc while its open of the copy fails.
#[must_use]
pub(crate) fn share_conflict(new: (u8, u8), open: (u8, u8)) -> bool {
    let ((granted, allowed), (o_granted, o_allowed)) = (new, open);
    granted != 0 && o_granted != 0 && ((granted & !o_allowed) != 0 || (o_granted & !allowed) != 0)
}
