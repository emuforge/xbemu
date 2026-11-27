//! The namespace with its volumes mounted, and the open file.
//!
//! Name resolution, opening, `NtQueryFullAttributesFile`, `NtDeleteFile`, the
//! symbolic link and device name tables, and the parts of reading, writing,
//! `NtQueryInformationFile`, `NtSetInformationFile`, `NtQueryDirectoryFile`
//! and volume information that do not touch guest memory, handles or threads.

use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use crate::disc::{DISC_SECTOR, DiscTree, DiscVolume};
use crate::host::{HostDisk, PARTITION_NUMBERS};
use crate::ioctl::{Control, device_control};
use crate::lock;
use crate::name::{FATX_MAX_NAME, wildcard_match};
use crate::namespace::{Device, DeviceKind, LinkTable, device_of, unknown_device_status};
use crate::status::NtStatus;
use crate::timing::FS_PAGE;
use crate::types::{
    CreateAction, DISC_TIME, FILE_USE_FILE_POINTER_POSITION, FILE_WRITE_TO_END_OF_FILE,
    FileBasicInformation, FileDirectoryInformation, FileFsAttributeInformation,
    FileFsDeviceInformation, FileFsVolumeInformation, FileNetworkOpenInformation,
    FileStandardInformation, access, allocation_size, attributes, file_class, fs_class, options,
    share,
};
use crate::volume::{DirEntry, FileFacts, OpenParams, SortKey, Volume, VolumeFile};

/// Device objects the name table holds; one past that is created unnamed.
const MAX_DEVICE_NAMES: usize = 32;

/// What a name is relative to: `OBJECT_ATTRIBUTES.RootDirectory`.
#[derive(Clone, Copy)]
pub enum Root<'a> {
    /// No root: the name is a full object path.
    None,
    /// The `\??` directory ([`OB_DOS_DEVICES_DIRECTORY`](crate::types::OB_DOS_DEVICES_DIRECTORY)):
    /// XAPI passes `D:\x` under it.
    DosDevices,
    /// An open file (a directory, for a name to exist): the name is joined to
    /// its path.
    File(&'a File),
}

/// A failed open: the status and what goes in the IoStatusBlock's
/// information.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpenError {
    /// Why the open failed.
    pub status: NtStatus,
    /// 5 (`FILE_DOES_NOT_EXIST`) once the path had a canonical form, 0 for a
    /// drive letter with no link.
    pub information: u32,
}

impl From<OpenError> for NtStatus {
    fn from(e: OpenError) -> NtStatus {
        e.status
    }
}

/// A device object found by name (`ObReferenceObjectByName`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceRef {
    /// A device the title created (`IoCreateDevice`): the value registered
    /// for it, the kernel's guest object.
    Registered(u32),
    /// The disc or a partition, which has no object yet: the kernel makes one
    /// (device type 2 for the disc, 7 for a disk) and registers it under the
    /// device's path.
    Builtin(Device),
}

/// The Xbox object namespace for storage: symbolic links, the disc, the hard
/// disk's partitions.
///
/// All methods take `&self` and may be called from any thread.
///
/// At boot the namespace holds the links the kernel itself makes: `\??\D:`
/// and `\??\CdRom0:`, both to `\Device\CdRom0`. The title's own drives are not
/// made here: XAPI's startup code in every title calls `IoCreateSymbolicLink`
/// for `T:` (`…\Partition1\TDATA\<title id>`), `U:` (`…\UDATA\<title id>`)
/// and, when asked, `Z:` (a cache partition), after creating those folders
/// with `NtCreateFile`.
pub struct FileSystem {
    links: RwLock<LinkTable>,
    disc: RwLock<Option<Arc<dyn Volume>>>,
    partitions: RwLock<[Option<Arc<dyn Volume>>; PARTITION_NUMBERS]>,
    device_names: Mutex<Vec<(String, u32)>>,
    hard_disk: Option<HostDisk>,
    unknown_logged: AtomicU32,
}

impl Default for FileSystem {
    fn default() -> Self {
        Self::new()
    }
}

fn read<T>(l: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    l.read().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn write<T>(l: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    l.write().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl FileSystem {
    /// A namespace with the boot links and nothing mounted: the drive's tray
    /// is empty and there is no hard disk.
    #[must_use]
    pub fn new() -> Self {
        let mut links = LinkTable::new();
        links.set(r"\??\D:", r"\Device\CdRom0");
        // The kernel's own, as on the Xbox (the dashboard's).
        links.set(r"\??\CdRom0:", r"\Device\CdRom0");
        log::info!(target: "fs", r"link \??\D: -> \Device\CdRom0");
        log::info!(target: "fs", r"link \??\CdRom0: -> \Device\CdRom0");
        Self {
            links: RwLock::new(links),
            disc: RwLock::new(None),
            partitions: RwLock::new(Default::default()),
            device_names: Mutex::new(Vec::new()),
            hard_disk: None,
            unknown_logged: AtomicU32::new(0),
        }
    }

    /// A namespace with the hard disk kept in the host folder `root`:
    /// `Partition0` raw and `Partition1` to `Partition9` as folders.
    ///
    /// # Errors
    /// The folder cannot be created.
    pub fn with_hard_disk(root: impl Into<PathBuf>) -> io::Result<Self> {
        let disk = HostDisk::new(root)?;
        let mut fs = Self::new();
        for n in 0..PARTITION_NUMBERS as u8 {
            fs.mount(Device::Partition(n), disk.partition(n));
        }
        fs.hard_disk = Some(disk);
        Ok(fs)
    }

    /// The hard disk given to [`with_hard_disk`](Self::with_hard_disk), for
    /// its raw sectors and refurb info.
    #[must_use]
    pub fn hard_disk(&self) -> Option<&HostDisk> {
        self.hard_disk.as_ref()
    }

    /// Mounts `volume` as `device`, replacing what was there.
    pub fn mount(&self, device: Device, volume: Arc<dyn Volume>) {
        match device {
            Device::CdRom0 => *write(&self.disc) = Some(volume),
            Device::Partition(n) => write(&self.partitions)[usize::from(n)] = Some(volume),
        }
    }

    /// Puts a disc in the drive: mounts a [`DiscVolume`] over `tree`.
    pub fn insert_disc(&self, tree: Arc<dyn DiscTree>) {
        self.mount(Device::CdRom0, Arc::new(DiscVolume::new(tree)));
    }

    /// Takes `device`'s volume away (the disc out of the drive). Files open
    /// on it stay usable.
    pub fn unmount(&self, device: Device) -> Option<Arc<dyn Volume>> {
        match device {
            Device::CdRom0 => write(&self.disc).take(),
            Device::Partition(n) => write(&self.partitions)[usize::from(n)].take(),
        }
    }

    /// The volume mounted as `device`.
    #[must_use]
    pub fn volume(&self, device: Device) -> Option<Arc<dyn Volume>> {
        match device {
            Device::CdRom0 => read(&self.disc).clone(),
            Device::Partition(n) => read(&self.partitions).get(usize::from(n))?.clone(),
        }
    }

    /// `IoCreateSymbolicLink`: makes or replaces the link `name` (as given,
    /// `\??\T:`) to `target`, stored in canonical form.
    ///
    /// Fails with `STATUS_OBJECT_PATH_NOT_FOUND` when the target names a
    /// drive letter with no link, `STATUS_INSUFFICIENT_RESOURCES` when the
    /// table is full.
    pub fn create_symbolic_link(&self, name: &str, target: &str) -> NtStatus {
        let mut links = write(&self.links);
        let Some(target) = links.canonicalize(target) else {
            return NtStatus::OBJECT_PATH_NOT_FOUND;
        };
        if !links.set(name, &target) {
            return NtStatus::INSUFFICIENT_RESOURCES;
        }
        log::info!(target: "fs", "link {name} -> {target}");
        NtStatus::SUCCESS
    }

    /// `IoDeleteSymbolicLink`: `STATUS_OBJECT_NAME_NOT_FOUND` if there is no
    /// such link.
    pub fn delete_symbolic_link(&self, name: &str) -> NtStatus {
        if write(&self.links).remove(name) {
            NtStatus::SUCCESS
        } else {
            NtStatus::OBJECT_NAME_NOT_FOUND
        }
    }

    /// The target of the link `name` (`NtOpenSymbolicLinkObject` keeps it for
    /// `NtQuerySymbolicLinkObject`; a link changed later does not change an
    /// open link object).
    #[must_use]
    pub fn symbolic_link_target(&self, name: &str) -> Option<String> {
        read(&self.links).get(name).map(str::to_owned)
    }

    /// Every link as (name, target).
    #[must_use]
    pub fn symbolic_links(&self) -> Vec<(String, String)> {
        read(&self.links).iter().map(|(n, t)| (n.to_owned(), t.to_owned())).collect()
    }

    /// Points `D:` at the folder an XBE on the hard disk was launched from,
    /// as the launch path's directory part becomes `D:` on the Xbox. A disc
    /// title's stays the disc's root.
    pub fn set_launch_dir(&self, dir: &str) {
        write(&self.links).set(r"\??\D:", dir);
        log::info!(target: "fs", r"link \??\D: -> {dir}");
    }

    /// The canonical device path of `name` relative to `root`; `None` when it
    /// names a drive letter with no link.
    #[must_use]
    pub fn canonicalize(&self, root: Root<'_>, name: &str) -> Option<String> {
        let joined;
        let full = match root {
            Root::None => name,
            Root::DosDevices => {
                joined = format!(r"\??\{name}");
                &joined
            }
            Root::File(file) => {
                joined = format!(r"{}\{name}", file.path());
                &joined
            }
        };
        read(&self.links).canonicalize(full)
    }

    /// `NtCreateFile` / `NtOpenFile`: opens `name` relative to `root`.
    ///
    /// # Errors
    /// The status of the failure with the IoStatusBlock's information; see
    /// `docs/hw/io-namespace.md` for which condition gives which status, in
    /// what order.
    pub fn open(
        &self,
        root: Root<'_>,
        name: &str,
        params: &OpenParams,
    ) -> Result<(File, CreateAction), OpenError> {
        let Some(path) = self.canonicalize(root, name) else {
            log::info!(target: "fs", "open {name}: no such drive");
            return Err(OpenError { status: NtStatus::OBJECT_PATH_NOT_FOUND, information: 0 });
        };
        let fail = |status| OpenError { status, information: CreateAction::DoesNotExist as u32 };
        let unknown = || {
            let status = unknown_device_status(&path);
            if self.unknown_logged.fetch_add(1, Ordering::Relaxed) < 64 {
                log::info!(target: "fs", "open {path}: unknown device ({status})");
            }
            fail(status)
        };
        let Some((device, rel)) = device_of(&path) else { return Err(unknown()) };
        let Some(volume) = self.volume(device) else {
            return Err(match device {
                // The dashboard booted with the tray empty.
                Device::CdRom0 => fail(NtStatus::NO_MEDIA_IN_DEVICE),
                Device::Partition(_) => unknown(),
            });
        };
        let (node, action) = volume.open(rel, params).map_err(|status| {
            log::debug!(target: "fs", "open {path}: {status}");
            fail(status)
        })?;
        log::info!(
            target: "fs",
            "open {path} -> {}{}",
            if node.is_dir() { "dir " } else { "" },
            match action {
                CreateAction::Created => "(created)",
                CreateAction::Overwritten => "(overwritten)",
                CreateAction::Superseded => "(superseded)",
                _ => "",
            }
        );
        Ok((File::new(path, device, volume, node, params), action))
    }

    /// Opens again a file a savestate kept ([`File::saved`]): by its path,
    /// with the access it had, as it is on the volume now (never created or
    /// truncated), and put where it was.
    ///
    /// # Errors
    /// The open's status: the file is gone, or its volume is.
    pub fn reopen(&self, state: &FileState) -> Result<File, NtStatus> {
        let params = OpenParams::open(state.access, state.share, state.options);
        let (file, _) = self.open(Root::None, &state.path, &params).map_err(|e| e.status)?;
        file.restore(state);
        Ok(file)
    }

    /// Replaces the symbolic links with a savestate's
    /// ([`FileSystem::symbolic_links`]).
    pub fn set_symbolic_links(&self, links: &[(String, String)]) {
        let mut table = write(&self.links);
        let old: Vec<String> = table.iter().map(|(n, _)| n.to_owned()).collect();
        for name in old {
            table.remove(&name);
        }
        for (name, target) in links {
            table.set(name, target);
        }
    }

    /// `NtQueryFullAttributesFile`: the times, size and attributes of `name`,
    /// found by opening it (for no access, sharing everything) and closing it
    /// again.
    ///
    /// # Errors
    /// The open's status.
    pub fn query_full_attributes(
        &self,
        root: Root<'_>,
        name: &str,
    ) -> Result<FileNetworkOpenInformation, NtStatus> {
        let (file, _) = self.open(root, name, &OpenParams::open(0, share::ALL, 0))?;
        Ok(file.network_open_information())
    }

    /// `NtDeleteFile`: deletes the file or empty directory `name`. A file
    /// with handles open goes when the last one closes. The disc and the raw
    /// disk have no file system to delete in (`STATUS_ACCESS_DENIED`).
    pub fn delete_file(&self, root: Root<'_>, name: &str) -> NtStatus {
        let Some(path) = self.canonicalize(root, name) else {
            log::info!(target: "fs", "delete {name}: no such drive");
            return NtStatus::ACCESS_DENIED;
        };
        match device_of(&path) {
            Some((device @ Device::Partition(1..), rel)) => match self.volume(device) {
                Some(volume) => volume.delete(rel),
                None => NtStatus::ACCESS_DENIED,
            },
            _ => {
                log::info!(target: "fs", "delete {path}: no such drive");
                NtStatus::ACCESS_DENIED
            }
        }
    }

    /// `FileRenameInformation`: moves `file` to `new_name`, which is relative
    /// to `root` as an open's name is. XAPI's `MoveFile` passes `Z:\x` under
    /// `\??`; Riddick's file cache renames each finished `X.tmp` to `X`.
    ///
    /// Only hard disk files and directories move (`STATUS_ACCESS_DENIED`
    /// otherwise). A new name on no partition or under a missing folder is
    /// `STATUS_OBJECT_PATH_NOT_FOUND`; one that exists, without
    /// `replace_if_exists`, `STATUS_OBJECT_NAME_COLLISION`.
    pub fn rename(
        &self,
        file: &File,
        root: Root<'_>,
        new_name: &str,
        replace_if_exists: bool,
    ) -> NtStatus {
        let status = self.rename_inner(file, root, new_name, replace_if_exists);
        if status != NtStatus::SUCCESS {
            log::info!(
                target: "fs",
                "rename {} to {new_name} (replace {replace_if_exists}) -> {status}",
                file.path()
            );
        }
        status
    }

    fn rename_inner(&self, file: &File, root: Root<'_>, new_name: &str, replace: bool) -> NtStatus {
        if !file.is_hard_disk_file() {
            return NtStatus::ACCESS_DENIED;
        }
        let Some(path) = self.canonicalize(root, new_name) else {
            return NtStatus::OBJECT_PATH_NOT_FOUND;
        };
        let Some((device @ Device::Partition(1..), rel)) = device_of(&path) else {
            return NtStatus::OBJECT_PATH_NOT_FOUND;
        };
        let Some(dest) = self.volume(device) else { return NtStatus::OBJECT_PATH_NOT_FOUND };
        let volume = lock(&file.location).volume.clone();
        let status = volume.rename(file.node.as_ref(), dest.as_ref(), rel, replace);
        if status == NtStatus::SUCCESS {
            *lock(&file.location) = Location { path: path.clone(), device, volume: dest };
        }
        status
    }

    /// The host file behind a path on partitions 1 to 5, if it exists as a
    /// regular file: how the core finds an XBE to boot from the hard disk (a
    /// title update in `E:\TDATA\<title>\$u`, the dashboard).
    #[must_use]
    pub fn host_file(&self, xbox_path: &str) -> Option<PathBuf> {
        let path = read(&self.links).canonicalize(xbox_path)?;
        let (device, rel) = device_of(&path)?;
        self.volume(device)?.host_file(rel)
    }

    /// `IoCreateDevice`'s naming: records that the device object `object` (the
    /// kernel's guest address for it) is named `name`.
    ///
    /// # Errors
    /// `STATUS_OBJECT_PATH_NOT_FOUND` for a name under a drive letter with no
    /// link, `STATUS_OBJECT_NAME_COLLISION` for a name taken.
    pub fn register_device(&self, name: &str, object: u32) -> Result<String, NtStatus> {
        let path = read(&self.links).canonicalize(name).ok_or(NtStatus::OBJECT_PATH_NOT_FOUND)?;
        let mut names = lock(&self.device_names);
        if names.iter().any(|(n, _)| n.eq_ignore_ascii_case(&path)) {
            return Err(NtStatus::OBJECT_NAME_COLLISION);
        }
        if names.len() < MAX_DEVICE_NAMES {
            names.push((path.clone(), object));
        } else {
            log::info!(target: "fs", "device table full: {path} not named");
        }
        Ok(path)
    }

    /// `IoDeleteDevice`'s naming: forgets every name of `object`.
    pub fn unregister_device(&self, object: u32) {
        lock(&self.device_names).retain(|&(_, o)| o != object);
    }

    /// `ObReferenceObjectByName` for devices: a registered device by its
    /// name, or the disc or a partition named exactly.
    ///
    /// # Errors
    /// `STATUS_OBJECT_NAME_NOT_FOUND`.
    pub fn reference_device(&self, name: &str) -> Result<DeviceRef, NtStatus> {
        let path = read(&self.links).canonicalize(name).ok_or(NtStatus::OBJECT_NAME_NOT_FOUND)?;
        if let Some(&(_, object)) =
            lock(&self.device_names).iter().find(|(n, _)| n.eq_ignore_ascii_case(&path))
        {
            return Ok(DeviceRef::Registered(object));
        }
        match device_of(&path) {
            Some((device, "")) => Ok(DeviceRef::Builtin(device)),
            _ => {
                log::info!(target: "fs", "reference by name {name}: not found");
                Err(NtStatus::OBJECT_NAME_NOT_FOUND)
            }
        }
    }

    /// The cache partitions (3 to 5) in use, as a bit mask: those a link
    /// points into (the utility drive `XMountUtilityDrive` made) and those
    /// with a file open.
    #[must_use]
    pub fn cache_partitions(&self) -> u32 {
        let mut mask = self.hard_disk.as_ref().map_or(0, HostDisk::open_cache_partitions);
        for (_, target) in read(&self.links).iter() {
            if let Some((Device::Partition(n @ 3..=5), _)) = device_of(target) {
                mask |= 1 << n;
            }
        }
        mask
    }
}

struct Location {
    path: String,
    device: Device,
    volume: Arc<dyn Volume>,
}

#[derive(Default)]
struct DirState {
    /// The snapshot the enumeration works on, taken when it starts.
    /// XDeleteSaveGame deletes each file as the listing returns it, and a live
    /// listing shifted under it and skipped every other file.
    listing: Option<Vec<DirEntry>>,
    next: usize,
    mask: String,
}

/// An open file, directory or device: what a file handle refers to.
///
/// `File` is `Send + Sync` and every method takes `&self`, so the kernel can
/// keep it in an `Arc` and let I/O workers read while the guest CPU thread
/// goes on. Reads and writes are positional and do not wait for one another.
/// Dropping the value closes the handle: a file marked for deletion goes
/// when its last handle is dropped, and the old bytes of an overwritten file
/// are discarded.
pub struct File {
    node: Box<dyn VolumeFile>,
    location: Mutex<Location>,
    kind: DeviceKind,
    position: AtomicU64,
    dir: Mutex<DirState>,
    /// The file's pages in the file system's cache, [lo, hi): those its last
    /// timed read from the drive brought in.
    cached: Mutex<(u64, u64)>,
    is_async: bool,
    no_buffering: bool,
    can_write: bool,
    /// The access, sharing and options it was opened with (a savestate
    /// opens it again with them).
    opened: (u32, u32, u32),
    refused_logged: AtomicBool,
}

/// What a savestate keeps of an open [`File`]: enough to open it again by
/// path and put it where it was. The file's contents are the volume's, not
/// the state's.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileState {
    /// Canonical path (`\Device\...`).
    pub path: String,
    /// Desired access, share access and create options of the open.
    pub access: u32,
    /// See `access`.
    pub share: u32,
    /// See `access`.
    pub options: u32,
    /// The file pointer.
    pub position: u64,
    /// A directory enumeration is under way...
    pub dir_started: bool,
    /// ... past this many entries of the folder's whole listing...
    pub dir_next: u64,
    /// ... with this pattern.
    pub dir_mask: String,
}

xb_types::state_fields!(FileState {
    path,
    access,
    share,
    options,
    position,
    dir_started,
    dir_next,
    dir_mask
});

impl File {
    fn new(
        path: String,
        device: Device,
        volume: Arc<dyn Volume>,
        node: Box<dyn VolumeFile>,
        params: &OpenParams,
    ) -> Self {
        Self {
            node,
            kind: volume.device_kind(),
            location: Mutex::new(Location { path, device, volume }),
            position: AtomicU64::new(0),
            dir: Mutex::new(DirState::default()),
            cached: Mutex::new((0, 0)),
            is_async: params.options & options::FILE_SYNCHRONOUS_IO == 0,
            no_buffering: params.options & options::FILE_NO_INTERMEDIATE_BUFFERING != 0,
            can_write: params.desired_access & access::WRITES != 0,
            opened: (params.desired_access, params.share_access, params.options),
            refused_logged: AtomicBool::new(false),
        }
    }

    /// What a savestate keeps of this open file.
    #[must_use]
    pub fn saved(&self) -> FileState {
        let dir = lock(&self.dir);
        FileState {
            path: self.path(),
            access: self.opened.0,
            share: self.opened.1,
            options: self.opened.2,
            position: self.position(),
            dir_started: dir.listing.is_some(),
            dir_next: self.whole_next(&dir) as u64,
            dir_mask: dir.mask.clone(),
        }
    }

    /// The volume's listing of this directory ([`VolumeFile::list`]).
    #[cfg(test)]
    pub(crate) fn list(&self, keep: &dyn Fn(&str) -> bool) -> Vec<DirEntry> {
        self.node.list(keep)
    }

    /// How far an enumeration is into the folder's whole listing, taken
    /// now: past the entry it last gave. That is what a savestate holds, so
    /// a state does not depend on the enumeration's own listing holding
    /// only what its mask matches.
    fn whole_next(&self, dir: &DirState) -> usize {
        let last = dir.next.checked_sub(1).and_then(|i| dir.listing.as_ref()?.get(i));
        let Some(last) = last else { return 0 };
        let whole = self.node.list(&|_| true);
        whole.iter().position(|e| e.name == last.name).map_or(whole.len(), |i| i + 1)
    }

    /// Puts a file just opened again where `state` says the saved one was:
    /// its pointer, and a directory enumeration past the same entries of a
    /// listing taken now.
    fn restore(&self, state: &FileState) {
        self.set_position(state.position);
        if state.dir_started {
            let mut dir = lock(&self.dir);
            let whole = self.node.list(&|_| true);
            let passed = (state.dir_next as usize).min(whole.len());
            let matches = |e: &DirEntry| wildcard_match(&state.dir_mask, &e.name);
            dir.next = whole[..passed].iter().filter(|e| matches(e)).count();
            dir.mask.clone_from(&state.dir_mask);
            dir.listing = Some(whole.into_iter().filter(matches).collect());
        }
    }

    /// The canonical device path the file was opened by (or renamed to), in
    /// the title's spelling.
    #[must_use]
    pub fn path(&self) -> String {
        lock(&self.location).path.clone()
    }

    /// The device the file is on.
    #[must_use]
    pub fn device(&self) -> Device {
        lock(&self.location).device
    }

    /// Disc or hard disk.
    #[must_use]
    pub fn device_kind(&self) -> DeviceKind {
        self.kind
    }

    /// A directory (a device's root included).
    #[must_use]
    pub fn is_dir(&self) -> bool {
        self.node.is_dir()
    }

    /// The device itself: `\Device\CdRom0` or `\Device\Harddisk0\Partition0`.
    #[must_use]
    pub fn is_volume(&self) -> bool {
        self.node.is_volume()
    }

    /// A file or directory on a hard disk partition with a file system (the
    /// reference's `F_HOST`).
    fn is_hard_disk_file(&self) -> bool {
        self.kind == DeviceKind::HardDisk && !self.node.is_volume()
    }

    /// Opened without `FILE_SYNCHRONOUS_IO_*`: an overlapped handle.
    #[must_use]
    pub fn is_async(&self) -> bool {
        self.is_async
    }

    /// Opened with `FILE_NO_INTERMEDIATE_BUFFERING`.
    #[must_use]
    pub fn no_buffering(&self) -> bool {
        self.no_buffering
    }

    /// The handle's access lets it change the file's data or size.
    #[must_use]
    pub fn can_write(&self) -> bool {
        self.can_write
    }

    /// The file's size; 0 for directories and devices.
    #[must_use]
    pub fn size(&self) -> u64 {
        self.node.size()
    }

    /// The file position (`FilePositionInformation`).
    #[must_use]
    pub fn position(&self) -> u64 {
        self.position.load(Ordering::Relaxed)
    }

    /// Sets the file position.
    pub fn set_position(&self, position: u64) {
        self.position.store(position, Ordering::Relaxed);
    }

    /// The offset a read works at: `byte_offset` (the guest's `ByteOffset`,
    /// `None` for a null pointer), or the file position for none or
    /// `FILE_USE_FILE_POINTER_POSITION`.
    #[must_use]
    pub fn read_offset(&self, byte_offset: Option<u64>) -> u64 {
        match byte_offset {
            Some(o) if o != FILE_USE_FILE_POINTER_POSITION => o,
            _ => self.position(),
        }
    }

    /// The offset a write works at: as [`read_offset`](Self::read_offset),
    /// and `FILE_WRITE_TO_END_OF_FILE` is the file's size.
    #[must_use]
    pub fn write_offset(&self, byte_offset: Option<u64>) -> u64 {
        match byte_offset {
            Some(FILE_WRITE_TO_END_OF_FILE) if !self.node.is_raw() => self.size(),
            _ => self.read_offset(byte_offset),
        }
    }

    /// A read or write ended at `end`: the file position goes there through a
    /// handle opened for synchronous I/O, or when the request named no offset
    /// of its own. An overlapped request that names its offset leaves the
    /// position to the title: Burnout 2 reads its traffic data in overlapped
    /// reads and moves the position itself after each.
    pub fn advance(&self, byte_offset: Option<u64>, end: u64) {
        let named = matches!(byte_offset, Some(o) if o != FILE_USE_FILE_POINTER_POSITION);
        if !self.is_async || !named {
            self.set_position(end);
        }
    }

    /// Reads at `offset` into `buf` and returns the bytes read, fewer than
    /// asked only at the end of the file.
    ///
    /// Zero bytes succeed, past the end too: Halo's map streaming queues reads
    /// of what is left, 0 at the end, and waits for each one's completion. A
    /// handle on raw sectors (the raw disk, a cache partition's root) fills
    /// the buffer, with zeros where nothing is kept.
    ///
    /// # Errors
    /// `STATUS_END_OF_FILE` at or past the end, `STATUS_INVALID_DEVICE_REQUEST`
    /// for a directory or a device without raw access,
    /// `STATUS_DEVICE_DATA_ERROR` when the host's read fails.
    pub fn read(&self, offset: u64, buf: &mut [u8]) -> Result<usize, NtStatus> {
        if self.node.is_raw() {
            return self.node.read_at(offset, buf);
        }
        if self.node.is_dir() || self.node.is_volume() {
            return Err(NtStatus::INVALID_DEVICE_REQUEST);
        }
        if buf.is_empty() {
            return Ok(0);
        }
        let size = self.node.size();
        if offset >= size {
            return Err(NtStatus::END_OF_FILE);
        }
        let n = (size - offset).min(buf.len() as u64) as usize;
        match self.node.read_at(offset, &mut buf[..n]) {
            Ok(read) if read == n => Ok(n),
            Ok(_) => Err(NtStatus::DEVICE_DATA_ERROR),
            Err(status) => Err(status),
        }
    }

    /// Writes `data` at `offset`, growing the file as needed, and returns the
    /// bytes written.
    ///
    /// # Errors
    /// `STATUS_ACCESS_DENIED` for anything but a hard disk file opened with
    /// write access (Splinter Cell relies on a read-only handle refusing),
    /// `STATUS_DISK_FULL` when the host's write fails.
    pub fn write(&self, offset: u64, data: &[u8]) -> Result<usize, NtStatus> {
        if self.node.is_raw() {
            return self.node.write_at(offset, data);
        }
        if !self.is_hard_disk_file() || self.node.is_dir() || !self.writable("write to") {
            return Err(NtStatus::ACCESS_DENIED);
        }
        self.node.write_at(offset, data)
    }

    fn writable(&self, what: &str) -> bool {
        if !self.can_write && !self.refused_logged.swap(true, Ordering::Relaxed) {
            log::info!(
                target: "fs",
                "{what} {} through a handle opened without write access: refused",
                self.path()
            );
        }
        self.can_write
    }

    /// `NtFlushBuffersFile`: writes the file through to the host's storage
    /// (a sync, which can take tens of milliseconds; the kernel runs it on an
    /// I/O worker). Always succeeds.
    pub fn flush(&self) -> NtStatus {
        self.node.flush()
    }

    /// Data or size changed through this handle.
    #[must_use]
    pub fn wrote(&self) -> bool {
        self.node.wrote()
    }

    /// The file should be [`flush`](Self::flush)ed before the handle is
    /// dropped: it was written and is on a partition that holds saves, so an
    /// app killed later cannot leave a half-written save. Cache drives' files
    /// are not.
    #[must_use]
    pub fn sync_on_close(&self) -> bool {
        self.node.sync_on_close()
    }

    /// True while the file is marked to go at its last close.
    #[must_use]
    pub fn delete_pending(&self) -> bool {
        self.node.delete_pending()
    }

    /// Times, size and attributes.
    #[must_use]
    pub fn facts(&self) -> FileFacts {
        self.node.facts()
    }

    /// `FileBasicInformation`.
    #[must_use]
    pub fn basic_information(&self) -> FileBasicInformation {
        let f = self.node.facts();
        FileBasicInformation {
            creation_time: f.creation_time,
            last_access_time: f.last_access_time,
            last_write_time: f.last_write_time,
            change_time: f.change_time,
            file_attributes: f.attributes,
        }
    }

    /// `FileStandardInformation`. `DeletePending` is always false, as in the
    /// reference.
    #[must_use]
    pub fn standard_information(&self) -> FileStandardInformation {
        let f = self.node.facts();
        FileStandardInformation {
            allocation_size: allocation_size(f.size),
            end_of_file: f.size,
            number_of_links: 1,
            delete_pending: false,
            directory: self.node.is_dir(),
        }
    }

    /// `FileNetworkOpenInformation`.
    #[must_use]
    pub fn network_open_information(&self) -> FileNetworkOpenInformation {
        let f = self.node.facts();
        FileNetworkOpenInformation {
            creation_time: f.creation_time,
            last_access_time: f.last_access_time,
            last_write_time: f.last_write_time,
            change_time: f.change_time,
            allocation_size: allocation_size(f.size),
            end_of_file: f.size,
            file_attributes: f.attributes,
        }
    }

    /// `FileInternalInformation`'s index number: a disc file's start sector, a
    /// hard disk file's inode. `GetFileInformationByHandle` asks for it and
    /// gives up without it; DOA3 sized its cache copy from the garbage left.
    #[must_use]
    pub fn index_number(&self) -> u64 {
        self.node.index_number()
    }

    /// Where the file is, for the control block `XGetFilePhysicalSortKey`
    /// reads.
    #[must_use]
    pub fn sort_key(&self) -> SortKey {
        self.node.sort_key()
    }

    /// `NtQueryInformationFile`: the guest bytes of information class `class`
    /// ([`file_class`]), for a buffer of `length` bytes. Answered: basic (4),
    /// standard (5), internal (6), position (14), alignment (17), network
    /// open (34).
    ///
    /// # Errors
    /// `STATUS_INVALID_INFO_CLASS` for another class, then
    /// `STATUS_BUFFER_TOO_SMALL` when `length` is under the class's size.
    pub fn query_information(&self, class: u32, length: usize) -> Result<Vec<u8>, NtStatus> {
        let bytes = match class {
            file_class::BASIC => self.basic_information().to_bytes().to_vec(),
            file_class::STANDARD => self.standard_information().to_bytes().to_vec(),
            file_class::INTERNAL => self.index_number().to_le_bytes().to_vec(),
            file_class::POSITION => self.position().to_le_bytes().to_vec(),
            file_class::ALIGNMENT => 0u32.to_le_bytes().to_vec(),
            file_class::NETWORK_OPEN => self.network_open_information().to_bytes().to_vec(),
            _ => {
                log::info!(target: "fs", "query information class {class} on {} not implemented", self.path());
                return Err(NtStatus::INVALID_INFO_CLASS);
            }
        };
        if length < bytes.len() { Err(NtStatus::BUFFER_TOO_SMALL) } else { Ok(bytes) }
    }

    /// `NtSetInformationFile` for the classes that need nothing from the
    /// kernel: position (14), allocation (19, accepted and ignored: the size
    /// is the file system's to keep), basic (4, the last access and last
    /// write times of a hard disk file; 0 and -1 keep one), end of file (20),
    /// disposition (13). `data` is the class's structure from guest memory.
    ///
    /// Rename (10) needs the name read from guest memory and goes to
    /// [`FileSystem::rename`]; completion (30) is the kernel's own. Both, and
    /// any other class, get `STATUS_INVALID_INFO_CLASS` here. A structure
    /// shorter than its class needs is `STATUS_INVALID_PARAMETER`.
    pub fn set_information(&self, class: u32, data: &[u8]) -> NtStatus {
        let u64_at = |at: usize| {
            data.get(at..at + 8).map(|b| u64::from_le_bytes(b.try_into().unwrap_or([0; 8])))
        };
        let host_file = self.is_hard_disk_file() && !self.node.is_dir();
        match class {
            file_class::POSITION => match u64_at(0) {
                Some(v) => {
                    self.set_position(v);
                    NtStatus::SUCCESS
                }
                None => NtStatus::INVALID_PARAMETER,
            },
            file_class::ALLOCATION => {
                if host_file && !self.writable("allocation size of") {
                    NtStatus::ACCESS_DENIED
                } else {
                    NtStatus::SUCCESS
                }
            }
            file_class::BASIC => {
                let (Some(access), Some(write)) = (u64_at(8), u64_at(0x10)) else {
                    return NtStatus::INVALID_PARAMETER;
                };
                if host_file {
                    let keep = |v: u64| (v != 0 && v != u64::MAX).then_some(v);
                    self.node.set_times(keep(access), keep(write));
                }
                NtStatus::SUCCESS
            }
            file_class::END_OF_FILE => match u64_at(0) {
                Some(_) if !host_file || !self.writable("end of file of") => {
                    NtStatus::ACCESS_DENIED
                }
                Some(v) => self.node.set_end_of_file(v),
                None => NtStatus::INVALID_PARAMETER,
            },
            file_class::DISPOSITION => {
                let Some(&delete) = data.first() else { return NtStatus::INVALID_PARAMETER };
                let status = self.node.set_delete_on_close(delete != 0);
                if delete != 0 {
                    // Deletes are few, and a save that loses a file is hard to trace back.
                    log::info!(target: "fs", "delete {} -> {:x}", self.path(), status.0);
                }
                status
            }
            _ => {
                log::info!(target: "fs", "set information class {class} on {} not implemented", self.path());
                NtStatus::INVALID_INFO_CLASS
            }
        }
    }

    /// `NtQueryDirectoryFile` (`FileDirectoryInformation`, the only class):
    /// the next entry matching the mask, one per call, for a buffer of
    /// `length` bytes.
    ///
    /// The directory is listed once when an enumeration starts: on the first
    /// call, when `restart_scan` is set, or when a different mask is given.
    /// The listing holds only the entries the mask matches, as the
    /// reference's does ([`VolumeFile::list`]).
    /// `mask` is the guest's `FileMask`: `None` (FindNextFile) goes on with
    /// the enumeration's mask, an empty one matches everything. Riddick's
    /// `FindFirstFile("_filler???")` / `FindNextFile` loop deletes what it is
    /// given and must not be given the rest of the save.
    ///
    /// # Errors
    /// `STATUS_NO_SUCH_FILE` when nothing matches from the start,
    /// `STATUS_NO_MORE_FILES` after the last match,
    /// `STATUS_BUFFER_TOO_SMALL` when the entry does not fit (the entry is
    /// passed over all the same), `STATUS_INVALID_DEVICE_REQUEST` for a
    /// handle that is not a directory's.
    pub fn query_directory(
        &self,
        mask: Option<&str>,
        restart_scan: bool,
        length: usize,
    ) -> Result<FileDirectoryInformation, NtStatus> {
        if !self.node.is_dir() {
            return Err(NtStatus::INVALID_DEVICE_REQUEST);
        }
        let mut dir = lock(&self.dir);
        let mask = match mask {
            Some("") => "*".to_owned(),
            Some(m) => m.to_owned(),
            None if dir.listing.is_some() => dir.mask.clone(),
            None => "*".to_owned(),
        };
        if restart_scan || mask != dir.mask || dir.listing.is_none() {
            dir.next = 0;
            dir.listing = Some(self.node.list(&|name| wildcard_match(&mask, name)));
            dir.mask = mask;
        }
        let DirState { listing, next, mask } = &mut *dir;
        let entries = listing.as_deref().unwrap_or_default();
        let i = *next;
        let Some(entry) = entries.get(i) else {
            if i == 0 {
                log::debug!(target: "fs", "list {} mask '{mask}': nothing matches", self.path());
                return Err(NtStatus::NO_SUCH_FILE);
            }
            return Err(NtStatus::NO_MORE_FILES);
        };
        *next = i + 1;
        let info = FileDirectoryInformation {
            file_index: i as u32,
            creation_time: DISC_TIME,
            last_access_time: DISC_TIME,
            last_write_time: DISC_TIME,
            change_time: DISC_TIME,
            end_of_file: entry.size,
            allocation_size: allocation_size(entry.size),
            file_attributes: if entry.is_dir {
                attributes::FILE_ATTRIBUTE_DIRECTORY
            } else if self.kind == DeviceKind::HardDisk {
                attributes::FILE_ATTRIBUTE_NORMAL
            } else {
                attributes::FILE_ATTRIBUTE_READONLY
            },
            file_name: entry.name.clone(),
        };
        if length < info.byte_len() { Err(NtStatus::BUFFER_TOO_SMALL) } else { Ok(info) }
    }

    /// `NtQueryVolumeInformationFile` / `IoQueryVolumeInformation`: the guest
    /// bytes of volume information class `class` ([`fs_class`]) for the
    /// volume the file is on: volume (1), size (3), device (4), attribute (5),
    /// full size (7).
    ///
    /// The file system's name is `GDFX` for the disc and `FATX` for the hard
    /// disk: `XGetFilePhysicalSortKey` knows the control block by it, and
    /// Fatal Frame II sorts its load queue by those keys.
    ///
    /// # Errors
    /// `STATUS_INVALID_INFO_CLASS` for another class.
    pub fn query_volume_information(&self, class: u32) -> Result<Vec<u8>, NtStatus> {
        let disc = self.kind == DeviceKind::Disc;
        Ok(match class {
            fs_class::VOLUME => FileFsVolumeInformation {
                volume_creation_time: DISC_TIME,
                volume_serial_number: if disc { 0x1234_5678 } else { 0x5678_ABCD },
            }
            .to_bytes()
            .to_vec(),
            fs_class::SIZE | fs_class::FULL_SIZE => {
                let volume = lock(&self.location).volume.clone();
                let size = volume.size_information();
                if class == fs_class::SIZE {
                    size.to_bytes().to_vec()
                } else {
                    size.to_full_bytes().to_vec()
                }
            }
            fs_class::DEVICE => FileFsDeviceInformation {
                device_type: if disc { 2 } else { 7 }, // FILE_DEVICE_CD_ROM / FILE_DEVICE_DISK
                characteristics: u32::from(disc),      // FILE_REMOVABLE_MEDIA
            }
            .to_bytes()
            .to_vec(),
            fs_class::ATTRIBUTE => FileFsAttributeInformation {
                file_system_attributes: 0,
                maximum_component_name_length: FATX_MAX_NAME as u32,
                file_system_name: if disc { "GDFX" } else { "FATX" }.to_owned(),
            }
            .to_bytes(),
            _ => {
                log::info!(target: "fs", "volume information class {class} not implemented");
                return Err(NtStatus::INVALID_INFO_CLASS);
            }
        })
    }

    /// `NtDeviceIoControlFile` / `NtFsControlFile` on this file: see
    /// [`device_control`]. `output_len` is 0 when the title gave no output
    /// buffer.
    ///
    /// # Errors
    /// As [`device_control`].
    pub fn control(&self, code: u32, output_len: usize) -> Result<Control, NtStatus> {
        let result = device_control(self.device().partition(), code, output_len);
        log::info!(target: "fs", "control {code:08x} on {} (out {output_len}): {result:?}", self.path());
        result
    }

    /// A read of `len` bytes at `offset` goes through the file system's
    /// cache, which reads in the caller's thread: it completes in the call,
    /// overlapped or not. That is a disc read of part of a sector; only whole
    /// sectors go to the drive. Wreckless reads a 12-byte header overlapped
    /// and hangs if that read pends.
    #[must_use]
    pub fn read_completes_in_cache(&self, offset: u64, len: u32) -> bool {
        self.kind == DeviceKind::Disc
            && !self.node.is_volume()
            && (offset | u64::from(len)) & u64::from(DISC_SECTOR - 1) != 0
    }

    /// A successful read of `len` bytes at `offset` returns `STATUS_PENDING`
    /// with its result in the IoStatusBlock, rather than completing in the
    /// call: the handle is overlapped, the read does not complete in the
    /// cache, and it has an event or APC (`has_event_or_apc`) or goes to a
    /// drive that pends every transfer (the DVD drive; the hard disk past the
    /// cache). Otogi counts a disc read's bytes only after
    /// `ERROR_IO_PENDING`. A read that fails up front (end of file) never
    /// pends and leaves the IoStatusBlock, event and APC alone.
    #[must_use]
    pub fn read_pends(&self, offset: u64, len: u32, has_event_or_apc: bool) -> bool {
        let drive = match self.kind {
            DeviceKind::Disc => !self.node.is_volume(),
            DeviceKind::HardDisk => self.is_hard_disk_file() && self.no_buffering,
        };
        self.is_async && !self.read_completes_in_cache(offset, len) && (has_event_or_apc || drive)
    }

    /// A successful write returns `STATUS_PENDING`: only an overlapped handle
    /// opened with `FILE_NO_INTERMEDIATE_BUFFERING`, whose writes go to the
    /// disk. Sniper Elite waits for `ERROR_IO_PENDING` on those; a write
    /// through the cache completes in the call whatever event or APC it has
    /// (Breakdown takes a pending one for a failed copy).
    #[must_use]
    pub fn write_pends(&self) -> bool {
        self.is_async && self.no_buffering && self.is_hard_disk_file()
    }

    /// Console timing: the file system's cache holds the `len` bytes at
    /// `offset` (pages this file's last timed read brought in), so a
    /// synchronous read of them takes no drive time.
    #[must_use]
    pub fn cache_holds(&self, offset: u64, len: u32) -> bool {
        let (lo, hi) = *lock(&self.cached);
        offset >= lo && offset + u64::from(len) <= hi
    }

    /// Console timing: a drive read of the `len` bytes at `offset` brings
    /// their pages into the cache in place of the file's last ones. Returns
    /// the bytes the drive transfers, the count to charge
    /// ([`DeviceTiming::cost`](crate::DeviceTiming::cost)).
    pub fn cache_fill(&self, offset: u64, len: u32) -> u32 {
        let lo = offset & !(FS_PAGE - 1);
        let hi = (offset + u64::from(len) + FS_PAGE - 1) & !(FS_PAGE - 1);
        *lock(&self.cached) = (lo, hi);
        (hi - lo) as u32
    }
}

impl std::fmt::Debug for File {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("File")
            .field("path", &self.path())
            .field("is_dir", &self.is_dir())
            .field("position", &self.position())
            .finish_non_exhaustive()
    }
}
