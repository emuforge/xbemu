//! Flags, time conversion and the information structures of the Xbox I/O
//! calls, with their guest byte layouts.
//!
//! All layouts are little-endian; the `to_bytes` forms are what the kernel
//! copies into guest memory.

/// `CreateDisposition` of `NtCreateFile`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum CreateDisposition {
    /// Replace the file if it exists, create it if not (`FILE_SUPERSEDE`).
    Supersede = 0,
    /// Open the file; fail if it does not exist (`FILE_OPEN`).
    Open = 1,
    /// Create the file; fail if it exists (`FILE_CREATE`).
    Create = 2,
    /// Open the file, or create it (`FILE_OPEN_IF`).
    OpenIf = 3,
    /// Open and empty the file; fail if it does not exist (`FILE_OVERWRITE`).
    Overwrite = 4,
    /// Open and empty the file, or create it (`FILE_OVERWRITE_IF`).
    OverwriteIf = 5,
}

impl CreateDisposition {
    /// The disposition for a guest value; `None` above `FILE_OVERWRITE_IF`.
    #[must_use]
    pub const fn from_u32(v: u32) -> Option<Self> {
        Some(match v {
            0 => Self::Supersede,
            1 => Self::Open,
            2 => Self::Create,
            3 => Self::OpenIf,
            4 => Self::Overwrite,
            5 => Self::OverwriteIf,
            _ => return None,
        })
    }

    /// True for the dispositions that empty an existing file.
    #[must_use]
    pub const fn overwrites(self) -> bool {
        matches!(self, Self::Supersede | Self::Overwrite | Self::OverwriteIf)
    }
}

/// What an open did: the `Information` of its IoStatusBlock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum CreateAction {
    /// An existing file was replaced (`FILE_SUPERSEDED`).
    Superseded = 0,
    /// An existing file or directory was opened (`FILE_OPENED`).
    Opened = 1,
    /// A new file or directory was made (`FILE_CREATED`).
    Created = 2,
    /// An existing file was emptied (`FILE_OVERWRITTEN`).
    Overwritten = 3,
    /// `FILE_EXISTS`.
    Exists = 4,
    /// `FILE_DOES_NOT_EXIST`: what a failed open leaves in the IoStatusBlock.
    DoesNotExist = 5,
}

/// `CreateOptions` bits of `NtCreateFile` / `OpenOptions` of `NtOpenFile`.
pub mod options {
    /// The path must be, or becomes, a directory.
    pub const FILE_DIRECTORY_FILE: u32 = 0x01;
    /// Reads and writes bypass the file system's cache: they go to the drive
    /// and pend on an overlapped handle.
    pub const FILE_NO_INTERMEDIATE_BUFFERING: u32 = 0x08;
    /// Synchronous I/O, waits alertable.
    pub const FILE_SYNCHRONOUS_IO_ALERT: u32 = 0x10;
    /// Synchronous I/O, waits not alertable.
    pub const FILE_SYNCHRONOUS_IO_NONALERT: u32 = 0x20;
    /// Either synchronous bit: without one the handle is overlapped.
    pub const FILE_SYNCHRONOUS_IO: u32 = 0x30;
    /// The path must not be a directory.
    pub const FILE_NON_DIRECTORY_FILE: u32 = 0x40;
}

/// `DesiredAccess` bits.
pub mod access {
    /// Read the file's data.
    pub const FILE_READ_DATA: u32 = 0x0000_0001;
    /// Write the file's data.
    pub const FILE_WRITE_DATA: u32 = 0x0000_0002;
    /// Append to the file.
    pub const FILE_APPEND_DATA: u32 = 0x0000_0004;
    /// Execute the file.
    pub const FILE_EXECUTE: u32 = 0x0000_0020;
    /// Delete the file.
    pub const DELETE: u32 = 0x0001_0000;
    /// Wait on the handle.
    pub const SYNCHRONIZE: u32 = 0x0010_0000;
    /// Everything the object allows.
    pub const MAXIMUM_ALLOWED: u32 = 0x0200_0000;
    /// All access.
    pub const GENERIC_ALL: u32 = 0x1000_0000;
    /// Execute access.
    pub const GENERIC_EXECUTE: u32 = 0x2000_0000;
    /// Write access.
    pub const GENERIC_WRITE: u32 = 0x4000_0000;
    /// Read access.
    pub const GENERIC_READ: u32 = 0x8000_0000;

    /// The bits that let a handle change a file's data or size. Writes and
    /// `SetEndOfFile` through a handle without one are refused: Splinter Cell
    /// calls `SetEndOfFile` on its profile through a `GENERIC_READ` handle and
    /// relies on that failing.
    pub const WRITES: u32 =
        FILE_WRITE_DATA | FILE_APPEND_DATA | GENERIC_WRITE | GENERIC_ALL | MAXIMUM_ALLOWED;
}

/// `ShareAccess` bits, also the kinds of access a handle is counted for
/// (`IoCheckShareAccess`).
pub mod share {
    /// Others may read.
    pub const FILE_SHARE_READ: u32 = 1;
    /// Others may write.
    pub const FILE_SHARE_WRITE: u32 = 2;
    /// Others may delete.
    pub const FILE_SHARE_DELETE: u32 = 4;
    /// All three.
    pub const ALL: u32 = 7;
}

/// The share bits a `DesiredAccess` is counted for, as `IoCheckShareAccess`
/// has them: reading or executing the data, writing or appending it, deleting
/// the file. 0 means the handle takes no part in sharing (attributes only).
#[must_use]
pub const fn share_granted(desired_access: u32) -> u8 {
    let mut g = 0;
    if desired_access
        & (access::FILE_READ_DATA
            | access::FILE_EXECUTE
            | access::GENERIC_READ
            | access::GENERIC_EXECUTE)
        != 0
    {
        g |= share::FILE_SHARE_READ as u8;
    }
    if desired_access & (access::FILE_WRITE_DATA | access::FILE_APPEND_DATA | access::GENERIC_WRITE)
        != 0
    {
        g |= share::FILE_SHARE_WRITE as u8;
    }
    if desired_access & access::DELETE != 0 {
        g |= share::FILE_SHARE_DELETE as u8;
    }
    if desired_access & (access::GENERIC_ALL | access::MAXIMUM_ALLOWED) != 0 {
        g |= share::ALL as u8;
    }
    g
}

/// File attribute bits.
pub mod attributes {
    /// The file cannot be written (every disc file).
    pub const FILE_ATTRIBUTE_READONLY: u32 = 0x01;
    /// A directory.
    pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
    /// A file with no other attribute.
    pub const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
}

/// `ByteOffset` value meaning "at the file's position".
pub const FILE_USE_FILE_POINTER_POSITION: u64 = 0xFFFF_FFFF_FFFF_FFFE;
/// `ByteOffset` value meaning "at the end of the file" (writes).
pub const FILE_WRITE_TO_END_OF_FILE: u64 = 0xFFFF_FFFF_FFFF_FFFF;

/// `RootDirectory` pseudo-handle of `OBJECT_ATTRIBUTES` for the `\??`
/// directory (`ObDosDevicesDirectory`): XAPI passes `D:\x` under it.
pub const OB_DOS_DEVICES_DIRECTORY: u32 = 0xFFFF_FFFD;

/// The time every disc file and every directory listing entry carries, as a
/// FILETIME: the reference's constant, 02:38 UTC on 2001-01-01.
pub const DISC_TIME: u64 = 0x01C0_739B_EB8F_8000;

/// Seconds between 1601-01-01 (the FILETIME epoch) and 1970-01-01.
pub const FILETIME_UNIX_EPOCH_SECS: i64 = 11_644_473_600;

/// A FILETIME (100 ns units since 1601) for a Unix time. Times before 1601
/// clamp to 0.
#[must_use]
pub fn filetime_from_unix(secs: i64, nanos: u32) -> u64 {
    let ticks = (i128::from(secs) + i128::from(FILETIME_UNIX_EPOCH_SECS)) * 10_000_000
        + i128::from(nanos / 100);
    u64::try_from(ticks).unwrap_or(0)
}

/// The Unix time (seconds, nanoseconds within the second) of a FILETIME.
#[must_use]
pub fn filetime_to_unix(filetime: u64) -> (i64, u32) {
    let unix100 = i128::from(filetime) - i128::from(FILETIME_UNIX_EPOCH_SECS) * 10_000_000;
    let secs = unix100.div_euclid(10_000_000);
    let nanos = unix100.rem_euclid(10_000_000) * 100;
    (secs as i64, nanos as u32)
}

/// A FILETIME for a host time stamp.
#[must_use]
pub fn filetime_from_system_time(t: std::time::SystemTime) -> u64 {
    match t.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => filetime_from_unix(d.as_secs() as i64, d.subsec_nanos()),
        Err(e) => {
            let d = e.duration();
            let (mut secs, mut nanos) = (-(d.as_secs() as i64), 0);
            if d.subsec_nanos() != 0 {
                secs -= 1;
                nanos = 1_000_000_000 - d.subsec_nanos();
            }
            filetime_from_unix(secs, nanos)
        }
    }
}

/// The host time stamp of a FILETIME.
#[must_use]
pub fn filetime_to_system_time(filetime: u64) -> std::time::SystemTime {
    let (secs, nanos) = filetime_to_unix(filetime);
    if secs >= 0 {
        std::time::UNIX_EPOCH + std::time::Duration::new(secs as u64, nanos)
    } else {
        std::time::UNIX_EPOCH - std::time::Duration::from_secs(secs.unsigned_abs())
            + std::time::Duration::from_nanos(u64::from(nanos))
    }
}

/// Xbox names are ANSI bytes. This crate carries them as `str` with each byte
/// mapped to the character of the same code (Latin-1), which loses nothing in
/// either direction.
#[must_use]
pub fn ansi_to_string(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| char::from(b)).collect()
}

/// The ANSI bytes of a name; a character above U+00FF (a host file's name the
/// console could not have made) becomes `?`.
#[must_use]
pub fn string_to_ansi(s: &str) -> Vec<u8> {
    s.chars().map(|c| u8::try_from(u32::from(c)).unwrap_or(b'?')).collect()
}

/// The allocation size reported for a file of `size` bytes: rounded up to
/// 4 KiB, as the reference reports it for every volume.
#[must_use]
pub const fn allocation_size(size: u64) -> u64 {
    size.saturating_add(4095) & !4095
}

fn rd32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn rd64(b: &[u8], at: usize) -> u64 {
    u64::from(rd32(b, at)) | u64::from(rd32(b, at + 4)) << 32
}

fn wr32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

fn wr64(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

/// File information class numbers (`FILE_INFORMATION_CLASS`).
pub mod file_class {
    /// `FileDirectoryInformation` (directory enumeration).
    pub const DIRECTORY: u32 = 1;
    /// `FileBasicInformation`.
    pub const BASIC: u32 = 4;
    /// `FileStandardInformation`.
    pub const STANDARD: u32 = 5;
    /// `FileInternalInformation`.
    pub const INTERNAL: u32 = 6;
    /// `FileRenameInformation`.
    pub const RENAME: u32 = 10;
    /// `FileDispositionInformation`.
    pub const DISPOSITION: u32 = 13;
    /// `FilePositionInformation`.
    pub const POSITION: u32 = 14;
    /// `FileAlignmentInformation`.
    pub const ALIGNMENT: u32 = 17;
    /// `FileAllocationInformation`.
    pub const ALLOCATION: u32 = 19;
    /// `FileEndOfFileInformation`.
    pub const END_OF_FILE: u32 = 20;
    /// `FileCompletionInformation`.
    pub const COMPLETION: u32 = 30;
    /// `FileNetworkOpenInformation`.
    pub const NETWORK_OPEN: u32 = 34;
}

/// Volume information class numbers (`FS_INFORMATION_CLASS`).
pub mod fs_class {
    /// `FileFsVolumeInformation`.
    pub const VOLUME: u32 = 1;
    /// `FileFsSizeInformation`.
    pub const SIZE: u32 = 3;
    /// `FileFsDeviceInformation`.
    pub const DEVICE: u32 = 4;
    /// `FileFsAttributeInformation`.
    pub const ATTRIBUTE: u32 = 5;
    /// `FileFsFullSizeInformation`.
    pub const FULL_SIZE: u32 = 7;
}

/// `FILE_BASIC_INFORMATION`: four times at +0, +8, +0x10, +0x18 and
/// `FileAttributes` at +0x20; 0x28 bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FileBasicInformation {
    /// When the file was created.
    pub creation_time: u64,
    /// When it was last read.
    pub last_access_time: u64,
    /// When its data was last written.
    pub last_write_time: u64,
    /// When it last changed in any way.
    pub change_time: u64,
    /// Attribute bits ([`attributes`]).
    pub file_attributes: u32,
}

impl FileBasicInformation {
    /// Size in guest memory.
    pub const SIZE: usize = 0x28;

    /// The guest layout.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut b = [0; Self::SIZE];
        wr64(&mut b, 0, self.creation_time);
        wr64(&mut b, 8, self.last_access_time);
        wr64(&mut b, 0x10, self.last_write_time);
        wr64(&mut b, 0x18, self.change_time);
        wr32(&mut b, 0x20, self.file_attributes);
        b
    }

    /// Parses the guest layout; `None` if `b` is too short.
    #[must_use]
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        (b.len() >= 0x24).then(|| Self {
            creation_time: rd64(b, 0),
            last_access_time: rd64(b, 8),
            last_write_time: rd64(b, 0x10),
            change_time: rd64(b, 0x18),
            file_attributes: rd32(b, 0x20),
        })
    }
}

/// `FILE_STANDARD_INFORMATION`: `AllocationSize` +0, `EndOfFile` +8,
/// `NumberOfLinks` +0x10, `DeletePending` +0x14, `Directory` +0x15; 0x18 bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FileStandardInformation {
    /// Bytes the file takes on the volume.
    pub allocation_size: u64,
    /// The file's size.
    pub end_of_file: u64,
    /// Always 1.
    pub number_of_links: u32,
    /// The file goes at its last close.
    pub delete_pending: bool,
    /// A directory.
    pub directory: bool,
}

impl FileStandardInformation {
    /// Size in guest memory.
    pub const SIZE: usize = 0x18;

    /// The guest layout.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut b = [0; Self::SIZE];
        wr64(&mut b, 0, self.allocation_size);
        wr64(&mut b, 8, self.end_of_file);
        wr32(&mut b, 0x10, self.number_of_links);
        b[0x14] = u8::from(self.delete_pending);
        b[0x15] = u8::from(self.directory);
        b
    }

    /// Parses the guest layout; `None` if `b` is too short.
    #[must_use]
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        (b.len() >= 0x16).then(|| Self {
            allocation_size: rd64(b, 0),
            end_of_file: rd64(b, 8),
            number_of_links: rd32(b, 0x10),
            delete_pending: b[0x14] != 0,
            directory: b[0x15] != 0,
        })
    }
}

/// `FILE_NETWORK_OPEN_INFORMATION`: four times, `AllocationSize` +0x20,
/// `EndOfFile` +0x28, `FileAttributes` +0x30; 0x38 bytes. Also what
/// `NtQueryFullAttributesFile` returns.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FileNetworkOpenInformation {
    /// When the file was created.
    pub creation_time: u64,
    /// When it was last read.
    pub last_access_time: u64,
    /// When its data was last written.
    pub last_write_time: u64,
    /// When it last changed in any way.
    pub change_time: u64,
    /// Bytes the file takes on the volume.
    pub allocation_size: u64,
    /// The file's size.
    pub end_of_file: u64,
    /// Attribute bits ([`attributes`]).
    pub file_attributes: u32,
}

impl FileNetworkOpenInformation {
    /// Size in guest memory.
    pub const SIZE: usize = 0x38;

    /// The guest layout.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut b = [0; Self::SIZE];
        wr64(&mut b, 0, self.creation_time);
        wr64(&mut b, 8, self.last_access_time);
        wr64(&mut b, 0x10, self.last_write_time);
        wr64(&mut b, 0x18, self.change_time);
        wr64(&mut b, 0x20, self.allocation_size);
        wr64(&mut b, 0x28, self.end_of_file);
        wr32(&mut b, 0x30, self.file_attributes);
        b
    }

    /// Parses the guest layout; `None` if `b` is too short.
    #[must_use]
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        (b.len() >= 0x34).then(|| Self {
            creation_time: rd64(b, 0),
            last_access_time: rd64(b, 8),
            last_write_time: rd64(b, 0x10),
            change_time: rd64(b, 0x18),
            allocation_size: rd64(b, 0x20),
            end_of_file: rd64(b, 0x28),
            file_attributes: rd32(b, 0x30),
        })
    }
}

/// `FILE_DIRECTORY_INFORMATION` with an ANSI name: `NextEntryOffset` +0,
/// `FileIndex` +4, four times +8, `EndOfFile` +0x28, `AllocationSize` +0x30,
/// `FileAttributes` +0x38, `FileNameLength` +0x3C, `FileName` +0x40.
/// `NtQueryDirectoryFile` returns one entry per call, so `NextEntryOffset`
/// is 0.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileDirectoryInformation {
    /// The entry's place in the directory's listing.
    pub file_index: u32,
    /// When the file was created.
    pub creation_time: u64,
    /// When it was last read.
    pub last_access_time: u64,
    /// When its data was last written.
    pub last_write_time: u64,
    /// When it last changed in any way.
    pub change_time: u64,
    /// The file's size.
    pub end_of_file: u64,
    /// Bytes the file takes on the volume.
    pub allocation_size: u64,
    /// Attribute bits ([`attributes`]).
    pub file_attributes: u32,
    /// The entry's name.
    pub file_name: String,
}

impl FileDirectoryInformation {
    /// Bytes before the name.
    pub const HEADER_SIZE: usize = 0x40;

    /// Bytes the entry takes in guest memory.
    #[must_use]
    pub fn byte_len(&self) -> usize {
        Self::HEADER_SIZE + self.file_name.chars().count()
    }

    /// The guest layout: the header and the ANSI name, not terminated.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let name = string_to_ansi(&self.file_name);
        let mut b = vec![0; Self::HEADER_SIZE + name.len()];
        wr32(&mut b, 4, self.file_index);
        wr64(&mut b, 8, self.creation_time);
        wr64(&mut b, 0x10, self.last_access_time);
        wr64(&mut b, 0x18, self.last_write_time);
        wr64(&mut b, 0x20, self.change_time);
        wr64(&mut b, 0x28, self.end_of_file);
        wr64(&mut b, 0x30, self.allocation_size);
        wr32(&mut b, 0x38, self.file_attributes);
        wr32(&mut b, 0x3C, name.len() as u32);
        b[Self::HEADER_SIZE..].copy_from_slice(&name);
        b
    }

    /// Parses the guest layout; `None` if `b` is shorter than the header and
    /// the name it announces.
    #[must_use]
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() < Self::HEADER_SIZE {
            return None;
        }
        let name = b.get(Self::HEADER_SIZE..Self::HEADER_SIZE + rd32(b, 0x3C) as usize)?;
        Some(Self {
            file_index: rd32(b, 4),
            creation_time: rd64(b, 8),
            last_access_time: rd64(b, 0x10),
            last_write_time: rd64(b, 0x18),
            change_time: rd64(b, 0x20),
            end_of_file: rd64(b, 0x28),
            allocation_size: rd64(b, 0x30),
            file_attributes: rd32(b, 0x38),
            file_name: ansi_to_string(name),
        })
    }
}

/// The fixed part of `FILE_RENAME_INFORMATION` as the Xbox has it:
/// `ReplaceIfExists` +0, `RootDirectory` +4, `FileName` (an `ANSI_STRING`:
/// `Length` +8, `MaximumLength` +0xA, `Buffer` +0xC); 0x10 bytes. The kernel
/// reads the name from guest memory at `file_name_buffer`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FileRenameInformation {
    /// Replace a file already at the new name.
    pub replace_if_exists: bool,
    /// The handle the new name is relative to (0, a directory's, or
    /// [`OB_DOS_DEVICES_DIRECTORY`]).
    pub root_directory: u32,
    /// Bytes of the new name.
    pub file_name_length: u16,
    /// Capacity of the name's buffer.
    pub file_name_maximum_length: u16,
    /// Guest address of the name.
    pub file_name_buffer: u32,
}

impl FileRenameInformation {
    /// Size in guest memory.
    pub const SIZE: usize = 0x10;

    /// The guest layout.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut b = [0; Self::SIZE];
        b[0] = u8::from(self.replace_if_exists);
        wr32(&mut b, 4, self.root_directory);
        b[8..10].copy_from_slice(&self.file_name_length.to_le_bytes());
        b[10..12].copy_from_slice(&self.file_name_maximum_length.to_le_bytes());
        wr32(&mut b, 0xC, self.file_name_buffer);
        b
    }

    /// Parses the guest layout; `None` if `b` is too short.
    #[must_use]
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        (b.len() >= Self::SIZE).then(|| Self {
            replace_if_exists: b[0] != 0,
            root_directory: rd32(b, 4),
            file_name_length: u16::from_le_bytes([b[8], b[9]]),
            file_name_maximum_length: u16::from_le_bytes([b[10], b[11]]),
            file_name_buffer: rd32(b, 0xC),
        })
    }
}

/// `FILE_COMPLETION_INFORMATION` as the reference reads it: the port's handle
/// +0 and the completion key +4. Associating the port is the kernel's work.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FileCompletionInformation {
    /// Handle of the I/O completion port.
    pub port: u32,
    /// The key its packets carry.
    pub key: u32,
}

impl FileCompletionInformation {
    /// Size in guest memory.
    pub const SIZE: usize = 8;

    /// The guest layout.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut b = [0; Self::SIZE];
        wr32(&mut b, 0, self.port);
        wr32(&mut b, 4, self.key);
        b
    }

    /// Parses the guest layout; `None` if `b` is too short.
    #[must_use]
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        (b.len() >= Self::SIZE).then(|| Self { port: rd32(b, 0), key: rd32(b, 4) })
    }
}

/// `FILE_FS_VOLUME_INFORMATION` with no label: `VolumeCreationTime` +0,
/// `VolumeSerialNumber` +8, `VolumeLabelLength` +0xC, `SupportsObjects`
/// +0x10; the reference writes 0x12 bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FileFsVolumeInformation {
    /// When the volume was made.
    pub volume_creation_time: u64,
    /// The volume's serial number.
    pub volume_serial_number: u32,
}

impl FileFsVolumeInformation {
    /// Size in guest memory.
    pub const SIZE: usize = 0x12;

    /// The guest layout.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut b = [0; Self::SIZE];
        wr64(&mut b, 0, self.volume_creation_time);
        wr32(&mut b, 8, self.volume_serial_number);
        b
    }

    /// Parses the guest layout; `None` if `b` is too short.
    #[must_use]
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        (b.len() >= 0xC)
            .then(|| Self { volume_creation_time: rd64(b, 0), volume_serial_number: rd32(b, 8) })
    }
}

/// A volume's size: what `FILE_FS_SIZE_INFORMATION` (0x18 bytes) and
/// `FILE_FS_FULL_SIZE_INFORMATION` (0x20 bytes) report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FileFsSizeInformation {
    /// Clusters on the volume.
    pub total_allocation_units: u64,
    /// Clusters free.
    pub available_allocation_units: u64,
    /// Sectors in a cluster.
    pub sectors_per_allocation_unit: u32,
    /// Bytes in a sector.
    pub bytes_per_sector: u32,
}

impl FileFsSizeInformation {
    /// Size of `FILE_FS_SIZE_INFORMATION` in guest memory.
    pub const SIZE: usize = 0x18;
    /// Size of `FILE_FS_FULL_SIZE_INFORMATION` in guest memory.
    pub const FULL_SIZE: usize = 0x20;

    /// `FILE_FS_SIZE_INFORMATION`: total +0, available +8, sectors per unit
    /// +0x10, bytes per sector +0x14.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut b = [0; Self::SIZE];
        wr64(&mut b, 0, self.total_allocation_units);
        wr64(&mut b, 8, self.available_allocation_units);
        wr32(&mut b, 0x10, self.sectors_per_allocation_unit);
        wr32(&mut b, 0x14, self.bytes_per_sector);
        b
    }

    /// `FILE_FS_FULL_SIZE_INFORMATION`: total +0, caller-available +8,
    /// actual available +0x10 (the same number), sectors per unit +0x18,
    /// bytes per sector +0x1C.
    #[must_use]
    pub fn to_full_bytes(&self) -> [u8; Self::FULL_SIZE] {
        let mut b = [0; Self::FULL_SIZE];
        wr64(&mut b, 0, self.total_allocation_units);
        wr64(&mut b, 8, self.available_allocation_units);
        wr64(&mut b, 0x10, self.available_allocation_units);
        wr32(&mut b, 0x18, self.sectors_per_allocation_unit);
        wr32(&mut b, 0x1C, self.bytes_per_sector);
        b
    }

    /// Parses `FILE_FS_SIZE_INFORMATION`; `None` if `b` is too short.
    #[must_use]
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        (b.len() >= Self::SIZE).then(|| Self {
            total_allocation_units: rd64(b, 0),
            available_allocation_units: rd64(b, 8),
            sectors_per_allocation_unit: rd32(b, 0x10),
            bytes_per_sector: rd32(b, 0x14),
        })
    }

    /// Parses `FILE_FS_FULL_SIZE_INFORMATION`; `None` if `b` is too short.
    #[must_use]
    pub fn from_full_bytes(b: &[u8]) -> Option<Self> {
        (b.len() >= Self::FULL_SIZE).then(|| Self {
            total_allocation_units: rd64(b, 0),
            available_allocation_units: rd64(b, 8),
            sectors_per_allocation_unit: rd32(b, 0x18),
            bytes_per_sector: rd32(b, 0x1C),
        })
    }
}

/// `FILE_FS_DEVICE_INFORMATION`: `DeviceType` +0, `Characteristics` +4.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FileFsDeviceInformation {
    /// `FILE_DEVICE_CD_ROM` (2) or `FILE_DEVICE_DISK` (7).
    pub device_type: u32,
    /// `FILE_REMOVABLE_MEDIA` (1) for the disc.
    pub characteristics: u32,
}

impl FileFsDeviceInformation {
    /// Size in guest memory.
    pub const SIZE: usize = 8;

    /// The guest layout.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut b = [0; Self::SIZE];
        wr32(&mut b, 0, self.device_type);
        wr32(&mut b, 4, self.characteristics);
        b
    }

    /// Parses the guest layout; `None` if `b` is too short.
    #[must_use]
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        (b.len() >= Self::SIZE)
            .then(|| Self { device_type: rd32(b, 0), characteristics: rd32(b, 4) })
    }
}

/// `FILE_FS_ATTRIBUTE_INFORMATION` with an ANSI name:
/// `FileSystemAttributes` +0, `MaximumComponentNameLength` +4,
/// `FileSystemNameLength` +8, `FileSystemName` +0xC.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileFsAttributeInformation {
    /// Attribute bits; 0 on the Xbox's file systems.
    pub file_system_attributes: u32,
    /// Longest name of a file or directory.
    pub maximum_component_name_length: u32,
    /// `GDFX` for the disc, `FATX` for the hard disk.
    pub file_system_name: String,
}

impl FileFsAttributeInformation {
    /// The guest layout.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let name = string_to_ansi(&self.file_system_name);
        let mut b = vec![0; 12 + name.len()];
        wr32(&mut b, 0, self.file_system_attributes);
        wr32(&mut b, 4, self.maximum_component_name_length);
        wr32(&mut b, 8, name.len() as u32);
        b[12..].copy_from_slice(&name);
        b
    }

    /// Parses the guest layout; `None` if `b` is shorter than the name it
    /// announces.
    #[must_use]
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() < 12 {
            return None;
        }
        let name = b.get(12..12 + rd32(b, 8) as usize)?;
        Some(Self {
            file_system_attributes: rd32(b, 0),
            maximum_component_name_length: rd32(b, 4),
            file_system_name: ansi_to_string(name),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filetime_and_unix_time_round_trip() {
        for (secs, nanos) in [(0, 0), (978_307_200, 0), (1_700_000_000, 123_456_700), (-5, 500)] {
            let ft = filetime_from_unix(secs, nanos);
            assert_eq!(filetime_to_unix(ft), (secs, nanos - nanos % 100));
            assert_eq!(filetime_from_system_time(filetime_to_system_time(ft)), ft);
        }
    }

    #[test]
    fn the_disc_time_is_on_the_first_day_of_2001() {
        let (secs, _) = filetime_to_unix(DISC_TIME);
        assert!((978_307_200..978_307_200 + 86_400).contains(&secs));
    }

    #[test]
    fn share_access_counts_generic_and_specific_rights() {
        assert_eq!(share_granted(access::GENERIC_READ), 1);
        assert_eq!(share_granted(access::FILE_WRITE_DATA | access::DELETE), 6);
        assert_eq!(share_granted(access::MAXIMUM_ALLOWED), 7);
        assert_eq!(share_granted(access::SYNCHRONIZE), 0);
    }

    #[test]
    fn basic_information_has_times_then_attributes() {
        let i = FileBasicInformation {
            creation_time: 1,
            last_access_time: 2,
            last_write_time: 3,
            change_time: 4,
            file_attributes: 0x80,
        };
        let b = i.to_bytes();
        assert_eq!(b.len(), 0x28);
        assert_eq!((b[0], b[8], b[0x10], b[0x18], b[0x20]), (1, 2, 3, 4, 0x80));
        assert_eq!(FileBasicInformation::from_bytes(&b), Some(i));
    }

    #[test]
    fn standard_information_puts_the_directory_flag_at_0x15() {
        let i = FileStandardInformation {
            allocation_size: 0x2000,
            end_of_file: 0x1234,
            number_of_links: 1,
            delete_pending: false,
            directory: true,
        };
        let b = i.to_bytes();
        assert_eq!(b.len(), 0x18);
        assert_eq!(&b[..2], &[0x00, 0x20]);
        assert_eq!(&b[8..10], &[0x34, 0x12]);
        assert_eq!((b[0x10], b[0x14], b[0x15]), (1, 0, 1));
        assert_eq!(FileStandardInformation::from_bytes(&b), Some(i));
    }

    #[test]
    fn network_open_information_is_0x38_bytes() {
        let i = FileNetworkOpenInformation {
            creation_time: DISC_TIME,
            last_access_time: DISC_TIME,
            last_write_time: DISC_TIME,
            change_time: DISC_TIME,
            allocation_size: 4096,
            end_of_file: 10,
            file_attributes: 1,
        };
        let b = i.to_bytes();
        assert_eq!(b.len(), 0x38);
        assert_eq!((b[0x21], b[0x28], b[0x30]), (0x10, 10, 1));
        assert_eq!(FileNetworkOpenInformation::from_bytes(&b), Some(i));
    }

    #[test]
    fn directory_information_carries_an_ansi_name_at_0x40() {
        let i = FileDirectoryInformation {
            file_index: 7,
            creation_time: DISC_TIME,
            last_access_time: DISC_TIME,
            last_write_time: DISC_TIME,
            change_time: DISC_TIME,
            end_of_file: 5000,
            allocation_size: allocation_size(5000),
            file_attributes: attributes::FILE_ATTRIBUTE_NORMAL,
            file_name: "blam.sav".into(),
        };
        let b = i.to_bytes();
        assert_eq!(b.len(), 0x48);
        assert_eq!(i.byte_len(), 0x48);
        assert_eq!(&b[..4], &[0; 4]);
        assert_eq!(b[4], 7);
        assert_eq!(&b[0x28..0x2A], &5000u16.to_le_bytes());
        assert_eq!(&b[0x30..0x32], &8192u16.to_le_bytes());
        assert_eq!((b[0x38], b[0x3C]), (0x80, 8));
        assert_eq!(&b[0x40..], b"blam.sav");
        assert_eq!(FileDirectoryInformation::from_bytes(&b), Some(i));
        assert_eq!(FileDirectoryInformation::from_bytes(&b[..0x44]), None);
    }

    #[test]
    fn rename_and_completion_information_round_trip() {
        let r = FileRenameInformation {
            replace_if_exists: true,
            root_directory: OB_DOS_DEVICES_DIRECTORY,
            file_name_length: 9,
            file_name_maximum_length: 10,
            file_name_buffer: 0x8001_2340,
        };
        assert_eq!(FileRenameInformation::from_bytes(&r.to_bytes()), Some(r));
        assert_eq!(r.to_bytes()[4..8], [0xFD, 0xFF, 0xFF, 0xFF]);
        let c = FileCompletionInformation { port: 0x44, key: 9 };
        assert_eq!(FileCompletionInformation::from_bytes(&c.to_bytes()), Some(c));
    }

    #[test]
    fn volume_structures_round_trip() {
        let s = FileFsSizeInformation {
            total_allocation_units: 100,
            available_allocation_units: 40,
            sectors_per_allocation_unit: 32,
            bytes_per_sector: 512,
        };
        assert_eq!(FileFsSizeInformation::from_bytes(&s.to_bytes()), Some(s));
        assert_eq!(FileFsSizeInformation::from_full_bytes(&s.to_full_bytes()), Some(s));
        assert_eq!(s.to_full_bytes()[0x10], 40);
        let v =
            FileFsVolumeInformation { volume_creation_time: DISC_TIME, volume_serial_number: 9 };
        assert_eq!(v.to_bytes().len(), 0x12);
        assert_eq!(FileFsVolumeInformation::from_bytes(&v.to_bytes()), Some(v));
        let d = FileFsDeviceInformation { device_type: 2, characteristics: 1 };
        assert_eq!(FileFsDeviceInformation::from_bytes(&d.to_bytes()), Some(d));
        let a = FileFsAttributeInformation {
            file_system_attributes: 0,
            maximum_component_name_length: 42,
            file_system_name: "FATX".into(),
        };
        assert_eq!(a.to_bytes(), [0, 0, 0, 0, 42, 0, 0, 0, 4, 0, 0, 0, b'F', b'A', b'T', b'X']);
        assert_eq!(FileFsAttributeInformation::from_bytes(&a.to_bytes()), Some(a));
    }

    #[test]
    fn ansi_names_survive_the_string_form() {
        let bytes: Vec<u8> = (1..=255).collect();
        assert_eq!(string_to_ansi(&ansi_to_string(&bytes)), bytes);
        assert_eq!(string_to_ansi("a\u{3042}"), b"a?");
    }
}
