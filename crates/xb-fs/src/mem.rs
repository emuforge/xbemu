//! A hard disk partition held in memory, for tests.
//!
//! It follows the rules of the host folder volume (see `host.rs`) so the two
//! can be tested against each other: case-insensitive names, the create
//! dispositions, share access, delete at the last close, old bytes coming
//! back when an overwritten file grows.

use std::any::Any;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use crate::host::{DEFAULT_CLUSTER_SECTORS, HDD_SECTOR, PARTITIONS};
use crate::lock;
use crate::namespace::DeviceKind;
use crate::status::NtStatus;
use crate::types::{
    CreateAction, CreateDisposition, DISC_TIME, FileFsSizeInformation, attributes,
    filetime_from_system_time, options, share_granted,
};
use crate::volume::{
    DirEntry, FileFacts, LISTING_NAME_MAX, OpenParams, SortKey, Volume, VolumeFile, share_conflict,
};

const CLUSTER: u64 = DEFAULT_CLUSTER_SECTORS as u64 * HDD_SECTOR as u64;

#[derive(Default)]
struct MemDir {
    entries: Vec<(String, MemEntry)>,
}

enum MemEntry {
    Dir(MemDir),
    File(Arc<MemData>),
}

struct MemData {
    id: u64,
    bytes: RwLock<Vec<u8>>,
    /// Creation, last access and last write times.
    times: Mutex<[u64; 3]>,
    /// Open handles: id, access granted, access shared.
    handles: Mutex<Vec<(u64, u8, u8)>>,
    delete: AtomicBool,
}

struct MemInner {
    tree: Mutex<MemDir>,
    next_id: AtomicU64,
    capacity: u64,
}

/// An in-memory volume with the hard disk's rules.
#[derive(Clone)]
pub struct MemVolume {
    inner: Arc<MemInner>,
}

impl Default for MemVolume {
    fn default() -> Self {
        Self::new()
    }
}

fn parts(rel: &str) -> Vec<&str> {
    rel.split('\\').filter(|p| !p.is_empty()).collect()
}

fn seconds(filetime: u64) -> u64 {
    filetime / 10_000_000 * 10_000_000
}

impl MemDir {
    fn position(&self, name: &str) -> Option<usize> {
        self.entries.iter().position(|(n, _)| n.eq_ignore_ascii_case(name))
    }

    fn walk(&mut self, path: &[&str]) -> Option<&mut MemDir> {
        let mut dir = self;
        for part in path {
            let at = dir.position(part)?;
            dir = match &mut dir.entries[at].1 {
                MemEntry::Dir(d) => d,
                MemEntry::File(_) => return None,
            };
        }
        Some(dir)
    }

    /// Removes the entry holding `data`, wherever it is.
    fn unlink(&mut self, data: &Arc<MemData>) -> bool {
        if let Some(at) = self
            .entries
            .iter()
            .position(|(_, e)| matches!(e, MemEntry::File(d) if Arc::ptr_eq(d, data)))
        {
            self.entries.remove(at);
            return true;
        }
        self.entries.iter_mut().any(|(_, e)| match e {
            MemEntry::Dir(d) => d.unlink(data),
            MemEntry::File(_) => false,
        })
    }

    /// The path of the entry holding `data`.
    fn path_of(&self, data: &Arc<MemData>, prefix: &mut Vec<String>) -> bool {
        for (name, entry) in &self.entries {
            prefix.push(name.clone());
            let found = match entry {
                MemEntry::File(d) => Arc::ptr_eq(d, data),
                MemEntry::Dir(d) => d.path_of(data, prefix),
            };
            if found {
                return true;
            }
            prefix.pop();
        }
        false
    }

    fn used(&self) -> u64 {
        self.entries
            .iter()
            .map(|(_, e)| match e {
                MemEntry::Dir(d) => d.used(),
                MemEntry::File(f) => {
                    (f.bytes.read().unwrap_or_else(std::sync::PoisonError::into_inner).len() as u64)
                        .div_ceil(CLUSTER)
                        * CLUSTER
                }
            })
            .sum()
    }
}

impl MemVolume {
    /// An empty volume the size of the data partition (E:).
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(PARTITIONS[1].1)
    }

    /// An empty volume of `capacity` bytes. Writes that would take it past
    /// that fail with `STATUS_DISK_FULL`.
    #[must_use]
    pub fn with_capacity(capacity: u64) -> Self {
        Self {
            inner: Arc::new(MemInner {
                tree: Mutex::new(MemDir::default()),
                next_id: AtomicU64::new(1),
                capacity,
            }),
        }
    }

    /// Puts a file at `rel` (components separated by `\`), making its
    /// directories and replacing a file already there. A shortcut for
    /// setting tests up.
    pub fn insert_file(&self, rel: &str, bytes: &[u8]) {
        let path = parts(rel);
        let Some((name, dirs)) = path.split_last() else { return };
        let mut tree = lock(&self.inner.tree);
        let mut dir = &mut *tree;
        for part in dirs {
            let at = match dir.position(part) {
                Some(at) => at,
                None => {
                    dir.entries.push(((*part).to_owned(), MemEntry::Dir(MemDir::default())));
                    dir.entries.len() - 1
                }
            };
            dir = match &mut dir.entries[at].1 {
                MemEntry::Dir(d) => d,
                MemEntry::File(_) => return,
            };
        }
        if let Some(at) = dir.position(name) {
            dir.entries.remove(at);
        }
        let data = self.inner.new_data(bytes.to_vec());
        dir.entries.push(((*name).to_owned(), MemEntry::File(data)));
    }

    /// The bytes of the file at `rel`, if there is one.
    #[must_use]
    pub fn file_bytes(&self, rel: &str) -> Option<Vec<u8>> {
        let path = parts(rel);
        let (name, dirs) = path.split_last()?;
        let mut tree = lock(&self.inner.tree);
        let dir = tree.walk(dirs)?;
        match &dir.entries[dir.position(name)?].1 {
            MemEntry::File(f) => Some(f.bytes.read().ok()?.clone()),
            MemEntry::Dir(_) => None,
        }
    }
}

impl MemInner {
    fn new_data(&self, bytes: Vec<u8>) -> Arc<MemData> {
        let now = filetime_from_system_time(std::time::SystemTime::now());
        Arc::new(MemData {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            bytes: RwLock::new(bytes),
            times: Mutex::new([now; 3]),
            handles: Mutex::new(Vec::new()),
            delete: AtomicBool::new(false),
        })
    }
}

impl Volume for MemVolume {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::HardDisk
    }

    fn open(
        &self,
        rel: &str,
        params: &OpenParams,
    ) -> Result<(Box<dyn VolumeFile>, CreateAction), NtStatus> {
        let inner = &self.inner;
        let path = parts(rel);
        let (opts, disposition) = (params.options, params.disposition);
        let granted = share_granted(params.desired_access);
        let allowed = (params.share_access & 7) as u8;
        let mut tree = lock(&inner.tree);
        let (name, dirs) = path.split_last().map_or((None, &[][..]), |(n, d)| (Some(*n), d));
        let Some(parent) = tree.walk(dirs) else { return Err(NtStatus::OBJECT_PATH_NOT_FOUND) };
        let at = name.and_then(|n| parent.position(n));
        let exists = name.is_none() || at.is_some();
        let file = at.and_then(|at| match &parent.entries[at].1 {
            MemEntry::File(f) => Some(Arc::clone(f)),
            MemEntry::Dir(_) => None,
        });
        let is_dir = exists && file.is_none();

        if file.as_ref().is_some_and(|f| f.delete.load(Ordering::Relaxed)) {
            return Err(NtStatus::DELETE_PENDING);
        }
        if exists && disposition == CreateDisposition::Create {
            return Err(NtStatus::OBJECT_NAME_COLLISION);
        }
        if let Some(f) = &file
            && lock(&f.handles).iter().any(|&(_, g, a)| share_conflict((granted, allowed), (g, a)))
        {
            return Err(NtStatus::SHARING_VIOLATION);
        }
        if !exists && matches!(disposition, CreateDisposition::Open | CreateDisposition::Overwrite)
        {
            return Err(NtStatus::OBJECT_NAME_NOT_FOUND);
        }
        if exists && is_dir && opts & options::FILE_NON_DIRECTORY_FILE != 0 {
            return Err(NtStatus::FILE_IS_A_DIRECTORY);
        }
        if exists && !is_dir && opts & options::FILE_DIRECTORY_FILE != 0 {
            return Err(NtStatus::NOT_A_DIRECTORY);
        }

        let mut old = None;
        let mut data = file;
        let action;
        if !exists && opts & options::FILE_DIRECTORY_FILE != 0 {
            let name = name.unwrap_or_default();
            parent.entries.push((name.to_owned(), MemEntry::Dir(MemDir::default())));
            action = CreateAction::Created;
        } else if let Some(f) = &data {
            if disposition.overwrites() {
                let mut bytes = f.bytes.write().unwrap_or_else(std::sync::PoisonError::into_inner);
                old = Some(std::mem::take(&mut *bytes));
                action = if disposition == CreateDisposition::Supersede {
                    CreateAction::Superseded
                } else {
                    CreateAction::Overwritten
                };
            } else {
                action = CreateAction::Opened;
            }
        } else if !is_dir {
            let created = inner.new_data(Vec::new());
            let name = name.unwrap_or_default();
            parent.entries.push((name.to_owned(), MemEntry::File(Arc::clone(&created))));
            data = Some(created);
            action = CreateAction::Created;
        } else {
            action = CreateAction::Opened;
        }

        let id = inner.next_id.fetch_add(1, Ordering::Relaxed);
        if let Some(f) = &data {
            lock(&f.handles).push((id, granted, allowed));
        }
        Ok((
            Box::new(MemNode {
                vol: Arc::clone(inner),
                id,
                data,
                dir: Mutex::new(path.iter().map(|p| (*p).to_owned()).collect()),
                old: Mutex::new(old),
                wrote: AtomicBool::new(false),
            }),
            action,
        ))
    }

    fn delete(&self, rel: &str) -> NtStatus {
        let path = parts(rel);
        let mut tree = lock(&self.inner.tree);
        let Some((name, dirs)) = path.split_last() else { return NtStatus::ACCESS_DENIED };
        let Some(parent) = tree.walk(dirs) else { return NtStatus::OBJECT_NAME_NOT_FOUND };
        let Some(at) = parent.position(name) else { return NtStatus::OBJECT_NAME_NOT_FOUND };
        match &parent.entries[at].1 {
            MemEntry::Dir(d) if !d.entries.is_empty() => return NtStatus::ACCESS_DENIED,
            MemEntry::File(f) if !lock(&f.handles).is_empty() => {
                f.delete.store(true, Ordering::Relaxed);
                return NtStatus::SUCCESS;
            }
            _ => {}
        }
        parent.entries.remove(at);
        NtStatus::SUCCESS
    }

    fn rename(
        &self,
        file: &dyn VolumeFile,
        dest: &dyn Volume,
        dest_rel: &str,
        replace_if_exists: bool,
    ) -> NtStatus {
        let Some(node) = file.as_any().downcast_ref::<MemNode>() else {
            return NtStatus::ACCESS_DENIED;
        };
        if !dest
            .as_any()
            .downcast_ref::<MemVolume>()
            .is_some_and(|d| Arc::ptr_eq(&d.inner, &self.inner))
        {
            return NtStatus::OBJECT_PATH_NOT_FOUND;
        }
        let to = parts(dest_rel);
        let mut tree = lock(&self.inner.tree);
        // Where the file is now.
        let mut from: Vec<String> = Vec::new();
        match &node.data {
            Some(data) => {
                if !tree.path_of(data, &mut from) {
                    return NtStatus::ACCESS_DENIED;
                }
            }
            None => from = lock(&node.dir).clone(),
        }
        let Some((to_name, to_dirs)) = to.split_last() else {
            return if replace_if_exists {
                NtStatus::ACCESS_DENIED
            } else {
                NtStatus::OBJECT_NAME_COLLISION
            };
        };
        let Some((from_name, from_dirs)) = from.split_last() else {
            return NtStatus::ACCESS_DENIED; // the root
        };
        let from_dirs: Vec<&str> = from_dirs.iter().map(String::as_str).collect();
        let same = |a: &[&str], b: &[&str]| {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.eq_ignore_ascii_case(y))
        };
        let from_all: Vec<&str> = from.iter().map(String::as_str).collect();
        let Some(dest_parent) = tree.walk(to_dirs) else { return NtStatus::OBJECT_PATH_NOT_FOUND };
        let dest_at = dest_parent.position(to_name);
        if dest_at.is_some() && !replace_if_exists {
            return NtStatus::OBJECT_NAME_COLLISION;
        }
        if same(&from_all, &to) {
            return NtStatus::SUCCESS; // onto itself
        }
        if node.data.is_none()
            && to.len() > from_all.len()
            && same(&from_all, &to[..from_all.len()])
        {
            return NtStatus::ACCESS_DENIED; // a directory into itself
        }
        if let Some(at) = dest_at {
            // A file replaces a file, a directory an empty directory.
            let fits = match (&dest_parent.entries[at].1, &node.data) {
                (MemEntry::File(_), Some(_)) => true,
                (MemEntry::Dir(d), None) => d.entries.is_empty(),
                _ => false,
            };
            if !fits {
                return NtStatus::ACCESS_DENIED;
            }
            dest_parent.entries.remove(at);
        }
        let Some(source_parent) = tree.walk(&from_dirs) else { return NtStatus::ACCESS_DENIED };
        let Some(at) = source_parent.position(from_name) else { return NtStatus::ACCESS_DENIED };
        let (_, entry) = source_parent.entries.remove(at);
        match tree.walk(to_dirs) {
            Some(dest_parent) => dest_parent.entries.push(((*to_name).to_owned(), entry)),
            None => return NtStatus::ACCESS_DENIED,
        }
        if node.data.is_none() {
            *lock(&node.dir) = to.iter().map(|p| (*p).to_owned()).collect();
        }
        NtStatus::SUCCESS
    }

    fn size_information(&self) -> FileFsSizeInformation {
        let used = lock(&self.inner.tree).used();
        FileFsSizeInformation {
            total_allocation_units: self.inner.capacity / CLUSTER,
            available_allocation_units: self.inner.capacity.saturating_sub(used) / CLUSTER,
            sectors_per_allocation_unit: DEFAULT_CLUSTER_SECTORS,
            bytes_per_sector: HDD_SECTOR,
        }
    }
}

/// An open file or directory of a [`MemVolume`].
struct MemNode {
    vol: Arc<MemInner>,
    id: u64,
    /// `None` for a directory.
    data: Option<Arc<MemData>>,
    /// A directory's path.
    dir: Mutex<Vec<String>>,
    /// The bytes an overwrite disposition emptied the file of.
    old: Mutex<Option<Vec<u8>>>,
    wrote: AtomicBool,
}

impl MemNode {
    /// The file grew into [from, to): its old bytes come back there.
    fn regrow(&self, bytes: &mut [u8], from: usize, to: usize) {
        if let Some(old) = lock(&self.old).as_ref() {
            let to = to.min(old.len()).min(bytes.len());
            if from < to {
                bytes[from..to].copy_from_slice(&old[from..to]);
            }
        }
    }
}

impl VolumeFile for MemNode {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn is_dir(&self) -> bool {
        self.data.is_none()
    }

    fn size(&self) -> u64 {
        self.data.as_ref().map_or(0, |d| d.bytes.read().map_or(0, |b| b.len() as u64))
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, NtStatus> {
        let data = self.data.as_ref().ok_or(NtStatus::INVALID_DEVICE_REQUEST)?;
        let bytes = data.bytes.read().map_err(|_| NtStatus::DEVICE_DATA_ERROR)?;
        let start = usize::try_from(offset).unwrap_or(usize::MAX).min(bytes.len());
        let n = buf.len().min(bytes.len() - start);
        buf[..n].copy_from_slice(&bytes[start..start + n]);
        Ok(n)
    }

    fn write_at(&self, offset: u64, data: &[u8]) -> Result<usize, NtStatus> {
        let file = self.data.as_ref().ok_or(NtStatus::ACCESS_DENIED)?;
        let offset = usize::try_from(offset).map_err(|_| NtStatus::DISK_FULL)?;
        let end = offset.checked_add(data.len()).ok_or(NtStatus::DISK_FULL)?;
        let used = lock(&self.vol.tree).used();
        let mut bytes = file.bytes.write().map_err(|_| NtStatus::DISK_FULL)?;
        let grows = (end.max(bytes.len()) as u64).div_ceil(CLUSTER) * CLUSTER
            - (bytes.len() as u64).div_ceil(CLUSTER) * CLUSTER;
        if used + grows > self.vol.capacity {
            return Err(NtStatus::DISK_FULL);
        }
        let was = bytes.len();
        if end > was {
            bytes.resize(end, 0);
        }
        if offset > was {
            self.regrow(&mut bytes, was, offset); // the gap a write past the end leaves
        }
        bytes[offset..end].copy_from_slice(data);
        if !data.is_empty() {
            self.wrote.store(true, Ordering::Relaxed);
            lock(&file.times)[2] = filetime_from_system_time(std::time::SystemTime::now());
        }
        Ok(data.len())
    }

    fn set_end_of_file(&self, size: u64) -> NtStatus {
        let Some(file) = &self.data else { return NtStatus::ACCESS_DENIED };
        let Ok(size) = usize::try_from(size) else { return NtStatus::ACCESS_DENIED };
        let Ok(mut bytes) = file.bytes.write() else { return NtStatus::ACCESS_DENIED };
        let was = bytes.len();
        bytes.resize(size, 0);
        self.wrote.store(true, Ordering::Relaxed);
        if size > was {
            self.regrow(&mut bytes, was, size);
        }
        NtStatus::SUCCESS
    }

    fn flush(&self) -> NtStatus {
        NtStatus::SUCCESS
    }

    fn facts(&self) -> FileFacts {
        match &self.data {
            Some(file) => {
                let t = *lock(&file.times);
                FileFacts {
                    creation_time: seconds(t[0]),
                    last_access_time: seconds(t[1]),
                    last_write_time: seconds(t[2]),
                    change_time: seconds(t[2]),
                    size: self.size(),
                    attributes: attributes::FILE_ATTRIBUTE_NORMAL,
                }
            }
            None => FileFacts {
                creation_time: DISC_TIME,
                last_access_time: DISC_TIME,
                last_write_time: DISC_TIME,
                change_time: DISC_TIME,
                size: 0,
                attributes: attributes::FILE_ATTRIBUTE_DIRECTORY,
            },
        }
    }

    fn index_number(&self) -> u64 {
        self.data.as_ref().map_or(0, |d| d.id)
    }

    fn sort_key(&self) -> SortKey {
        match &self.data {
            Some(d) => SortKey::Fatx { first_cluster: d.id as u32 },
            None => SortKey::None,
        }
    }

    fn set_times(&self, last_access: Option<u64>, last_write: Option<u64>) -> NtStatus {
        if let Some(file) = &self.data {
            let mut t = lock(&file.times);
            t[1] = last_access.unwrap_or(t[1]);
            t[2] = last_write.unwrap_or(t[2]);
        }
        NtStatus::SUCCESS
    }

    fn set_delete_on_close(&self, delete: bool) -> NtStatus {
        match &self.data {
            Some(file) => {
                file.delete.store(delete, Ordering::Relaxed);
                NtStatus::SUCCESS
            }
            None if delete => {
                let path = lock(&self.dir).clone();
                let path: Vec<&str> = path.iter().map(String::as_str).collect();
                let Some((name, dirs)) = path.split_last() else { return NtStatus::ACCESS_DENIED };
                let mut tree = lock(&self.vol.tree);
                let Some(parent) = tree.walk(dirs) else { return NtStatus::ACCESS_DENIED };
                let Some(at) = parent.position(name) else { return NtStatus::ACCESS_DENIED };
                match &parent.entries[at].1 {
                    MemEntry::Dir(d) if d.entries.is_empty() => {
                        parent.entries.remove(at);
                        NtStatus::SUCCESS
                    }
                    MemEntry::Dir(_) => NtStatus::DIRECTORY_NOT_EMPTY,
                    MemEntry::File(_) => NtStatus::ACCESS_DENIED,
                }
            }
            None => NtStatus::SUCCESS,
        }
    }

    fn delete_pending(&self) -> bool {
        self.data.as_ref().is_some_and(|d| d.delete.load(Ordering::Relaxed))
    }

    fn list(&self, keep: &dyn Fn(&str) -> bool) -> Vec<DirEntry> {
        if self.data.is_some() {
            return Vec::new();
        }
        let path = lock(&self.dir).clone();
        let path: Vec<&str> = path.iter().map(String::as_str).collect();
        let mut tree = lock(&self.vol.tree);
        let Some(dir) = tree.walk(&path) else { return Vec::new() };
        dir.entries
            .iter()
            .filter(|(name, _)| name.chars().count() < LISTING_NAME_MAX && keep(name))
            .map(|(name, entry)| match entry {
                MemEntry::Dir(_) => DirEntry { name: name.clone(), size: 0, is_dir: true },
                MemEntry::File(f) => DirEntry {
                    name: name.clone(),
                    size: f.bytes.read().map_or(0, |b| b.len() as u64),
                    is_dir: false,
                },
            })
            .collect()
    }

    fn wrote(&self) -> bool {
        self.wrote.load(Ordering::Relaxed)
    }
}

impl Drop for MemNode {
    fn drop(&mut self) {
        let Some(data) = &self.data else { return };
        let mut tree = lock(&self.vol.tree);
        let mut handles = lock(&data.handles);
        handles.retain(|&(id, _, _)| id != self.id);
        if handles.is_empty() && data.delete.load(Ordering::Relaxed) {
            tree.unlink(data); // the last handle deletes it
        }
    }
}
