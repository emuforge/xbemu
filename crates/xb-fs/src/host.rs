//! The hard disk: partitions as host folders, matched case-insensitively as
//! FATX does, plus the few raw sectors that must survive a reboot.
//!
//! Layout under the hard disk folder:
//!
//! | Host path | What |
//! |---|---|
//! | `PartitionN/` | the files of `\Device\Harddisk0\PartitionN`, N from 1 to 9 |
//! | `raw-partitionN.bin` | the first 512 KiB of partition N read raw (N = 0: the disk's config area) |
//! | `.overwrite/` | old contents of overwritten files and formatted cache drives, on their way out |

use std::any::Any;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::lock;
use crate::name::{fatx_name_valid, name_from_host, name_to_host};
use crate::namespace::DeviceKind;
use crate::status::NtStatus;
use crate::sys;
use crate::types::{
    CreateAction, CreateDisposition, DISC_TIME, FileFsSizeInformation, attributes,
    filetime_from_unix, filetime_to_system_time, options, share_granted,
};
use crate::volume::{
    DirEntry, FileFacts, LISTING_MAX, LISTING_NAME_MAX, OpenParams, SortKey, Volume, VolumeFile,
    share_conflict,
};

/// Bytes of the retail hard disk (8 GB, 512-byte sectors).
pub const HDD_BYTES: u64 = 0x1_DD15_6000;

/// Bytes in a hard disk sector.
pub const HDD_SECTOR: u32 = 512;

/// The retail disk's layout: byte offset and length of partitions 0 to 5.
/// 0 is the config area, 1 the data drive (E:), 2 the system drive (C:), 3
/// to 5 the cache drives (X:, Y:, Z:).
pub const PARTITIONS: [(u64, u64); 6] = [
    (0x0000_0000, 0x0008_0000),
    (0xABE8_0000, 0x1_312D_6000),
    (0x8CA8_0000, 0x1F40_0000),
    (0x0008_0000, 0x2EE0_0000),
    (0x2EE8_0000, 0x2EE0_0000),
    (0x5DC8_0000, 0x2EE0_0000),
];

/// Partition numbers a path can name (`Partition0` to `Partition9`).
pub const PARTITION_NUMBERS: usize = 10;

/// Bytes kept of a partition read raw: the disk's config area before the
/// first partition (XAPI's cache drive records, the refurb info) and the head
/// of each cache partition (its FATX superblock). Past them raw reads give
/// zeros and raw writes are dropped.
pub const RAW_KEEP: u64 = 0x8_0000;

/// FATX's cluster size in sectors (16 KiB) for a partition never formatted
/// by a title.
pub const DEFAULT_CLUSTER_SECTORS: u32 = 32;

/// The config area's sector holding `XBOX_REFURB_INFO`.
const REFURB_SECTOR: u64 = 3;
/// `'RFRB'`: marks the refurb sector as written.
const REFURB_SIGNATURE: u32 = 0x5246_5242;

/// How long a partition's scanned use stands before files changing make it
/// scan again.
const USAGE_RESCAN: Duration = Duration::from_secs(2);

/// How a path inside a partition resolved on the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Found {
    /// Every component exists.
    Exists,
    /// Only the last component is missing.
    LastMissing,
    /// A component before the last is missing.
    ParentMissing,
}

#[derive(Clone, Copy, Default)]
struct Usage {
    used: u64,
    scanned: Option<Instant>,
    generation: u32,
    /// Formats so far: a scan begun before one does not answer for it.
    formats: u32,
    valid: bool,
    scanning: bool,
}

/// One open handle's share access on a file.
struct Handle {
    id: u64,
    granted: u8,
    allowed: u8,
}

/// What the handles open on one host file have in common: where it is now
/// (a rename moves it for all of them), their share access, and whether it
/// goes at the last close.
struct Shared {
    path: Mutex<PathBuf>,
    handles: Mutex<Vec<Handle>>,
    delete: AtomicBool,
}

struct DiskInner {
    root: PathBuf,
    /// The title the cache partitions (3 to 5) belong to, once known; 0
    /// before ([`HostDisk::set_cache_title`]).
    cache_title: AtomicU32,
    /// Bumped whenever the disk's files change, so a partition's use is
    /// scanned again at its next query.
    generation: AtomicU32,
    /// Sectors per cluster each partition was last formatted with; 0 unknown.
    cluster_sectors: [AtomicU32; PARTITION_NUMBERS],
    usage: Mutex<[Usage; 6]>,
    usage_done: Condvar,
    /// The files with handles open, by host path.
    open: Mutex<HashMap<PathBuf, Arc<Shared>>>,
    serial: AtomicU32,
    /// Side files a dead process left were removed.
    swept: AtomicBool,
    next_id: AtomicU64,
    enforce_fatx: AtomicBool,
    mismatch_logged: AtomicU32,
}

/// The console's hard disk over a host folder.
///
/// Cloning gives another handle on the same disk. Mount its partitions with
/// [`HostDisk::partition`]; [`FileSystem::with_hard_disk`](crate::FileSystem::with_hard_disk)
/// does that for all ten.
#[derive(Clone)]
pub struct HostDisk {
    inner: Arc<DiskInner>,
}

impl HostDisk {
    /// Opens the hard disk kept in `root`, creating the folder if needed, and
    /// starts measuring the partitions' use in the background (a walk over
    /// every file; a query waits only for a partition's first).
    ///
    /// # Errors
    /// The folder cannot be created.
    pub fn new(root: impl Into<PathBuf>) -> io::Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        let inner = Arc::new(DiskInner {
            root,
            cache_title: AtomicU32::new(0),
            generation: AtomicU32::new(1),
            cluster_sectors: Default::default(),
            usage: Mutex::new([Usage::default(); 6]),
            usage_done: Condvar::new(),
            open: Mutex::new(HashMap::new()),
            serial: AtomicU32::new(0),
            swept: AtomicBool::new(false),
            next_id: AtomicU64::new(1),
            enforce_fatx: AtomicBool::new(false),
            mismatch_logged: AtomicU32::new(0),
        });
        inner.prescan();
        Ok(Self { inner })
    }

    /// The host folder.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.inner.root
    }

    /// The volume of `\Device\Harddisk0\PartitionN`: the raw disk for 0, a
    /// folder of files for 1 to 9.
    ///
    /// # Panics
    /// `number` is above 9.
    #[must_use]
    pub fn partition(&self, number: u8) -> Arc<dyn Volume> {
        assert!(usize::from(number) < PARTITION_NUMBERS, "no Partition{number}");
        if number == 0 {
            Arc::new(RawDiskVolume { disk: Arc::clone(&self.inner) })
        } else {
            Arc::new(HostVolume { disk: Arc::clone(&self.inner), part: number })
        }
    }

    /// Reads `buf.len()` bytes at `offset` of partition `part` read raw. What
    /// is not kept ([`RAW_KEEP`]) or was never written reads as zeros.
    pub fn raw_read(&self, part: u8, offset: u64, buf: &mut [u8]) {
        self.inner.raw_read(part, offset, buf);
    }

    /// Writes `data` at `offset` of partition `part` read raw; bytes past
    /// [`RAW_KEEP`] are dropped. No format is detected here: that is a write
    /// through a cache partition's handle.
    pub fn raw_write(&self, part: u8, offset: u64, data: &[u8]) {
        self.inner.raw_write(part, offset, data);
    }

    /// `ExReadWriteRefurbInfo` reading: the 16 bytes of `XBOX_REFURB_INFO`
    /// (`Signature`, `PowerCycleCount`, `FirstSetTime`) from the config
    /// area's sector 3, or zeros if it was never written.
    #[must_use]
    pub fn refurb_info(&self) -> [u8; 16] {
        let mut sector = [0u8; HDD_SECTOR as usize];
        self.inner.raw_read(0, REFURB_SECTOR * u64::from(HDD_SECTOR), &mut sector);
        let mut info = [0u8; 16];
        if sector[..4] == REFURB_SIGNATURE.to_le_bytes() {
            info.copy_from_slice(&sector[..16]);
        }
        info
    }

    /// `ExReadWriteRefurbInfo` writing: stores `info` in the config area's
    /// sector 3 under the signature.
    pub fn set_refurb_info(&self, info: &[u8; 16]) {
        let at = REFURB_SECTOR * u64::from(HDD_SECTOR);
        let mut sector = [0u8; HDD_SECTOR as usize];
        self.inner.raw_read(0, at, &mut sector);
        sector[..16].copy_from_slice(info);
        sector[..4].copy_from_slice(&REFURB_SIGNATURE.to_le_bytes());
        self.inner.raw_write(0, at, &sector);
    }

    /// A partition's cluster size in sectors: from its last format (this
    /// run's, or the superblock kept from an earlier one), else 32.
    #[must_use]
    pub fn cluster_sectors(&self, part: u8) -> u32 {
        self.inner.cluster_sectors(part)
    }

    /// Bytes the files of partition `part` (1 to 5) take, in whole clusters,
    /// as of the last scan. Scans again in the background once files have
    /// changed and the last scan is over two seconds old.
    #[must_use]
    pub fn partition_used(&self, part: u8) -> u64 {
        self.inner.part_used(part)
    }

    /// Measures partition `part` (1 to 5) now, in the caller's thread, and
    /// returns its use. For tests and tools; the kernel uses
    /// [`partition_used`](Self::partition_used).
    #[must_use]
    pub fn rescan_usage(&self, part: u8) -> u64 {
        lock(&self.inner.usage)[usize::from(part)].scanning = true;
        self.inner.scan(usize::from(part));
        self.inner.part_used(part)
    }

    /// Where a scan of partition `part` starts from: the disk's generation
    /// and the partition's formats so far.
    #[cfg(test)]
    pub(crate) fn scan_began(&self, part: u8) -> (u32, u32) {
        let formats = lock(&self.inner.usage)[usize::from(part)].formats;
        (self.inner.generation.load(Ordering::Relaxed), formats)
    }

    /// The answer of a scan that started at [`HostDisk::scan_began`]'s.
    #[cfg(test)]
    pub(crate) fn scan_answers(&self, part: u8, used: u64, generation: u32, formats: u32) {
        self.inner.scanned(usize::from(part), used, generation, formats);
    }

    /// Refuses creating names FATX does not take
    /// ([`crate::name::fatx_name_valid`]) with
    /// `STATUS_OBJECT_NAME_INVALID`. Off by default: the reference creates
    /// whatever the host takes.
    pub fn set_enforce_fatx_names(&self, enforce: bool) {
        self.inner.enforce_fatx.store(enforce, Ordering::Relaxed);
    }

    /// Gives the cache partitions (3 to 5) to title `title_id`: from here on
    /// they are that title's own folder, `cache/<title id>/` under the root.
    /// Call once the title is known and before it runs; nothing may be open
    /// on a cache partition yet.
    pub fn set_cache_title(&self, title_id: u32) {
        self.inner.cache_title.store(title_id, Ordering::Relaxed);
        for part in 3..=5u8 {
            self.inner.cluster_sectors[usize::from(part)].store(0, Ordering::Relaxed);
            let _ = fs::create_dir_all(self.inner.partition_dir(part));
        }
        self.inner.changed();
        log::info!(target: "fs", "cache partitions: {}", self.inner.cache_root().display());
    }

    /// The host folder that holds the running title's cache partitions and
    /// what is kept of their raw sectors: what a savestate keeps of the disk.
    #[must_use]
    pub fn cache_root(&self) -> PathBuf {
        self.inner.cache_root()
    }

    /// The host folders of a title's saved games and of its own data
    /// (`Partition1\UDATA\<id>` and `TDATA\<id>`).
    #[must_use]
    pub fn title_data_dirs(&self, title_id: u32) -> [PathBuf; 2] {
        // The names as the title made them: FATX ignores case, the host
        // may not.
        fn named(dir: &Path, name: &str) -> PathBuf {
            fs::read_dir(dir)
                .ok()
                .and_then(|entries| {
                    entries
                        .flatten()
                        .find(|e| e.file_name().to_string_lossy().eq_ignore_ascii_case(name))
                })
                .map_or_else(|| dir.join(name), |e| e.path())
        }
        let part = self.inner.partition_dir(1);
        let id = format!("{title_id:08x}");
        ["UDATA", "TDATA"].map(|d| named(&named(&part, d), &id))
    }

    /// The cache partitions' files were replaced behind the volumes (a
    /// savestate put its own back): what was learnt of them is forgotten.
    pub fn cache_replaced(&self) {
        for part in 3..=5u8 {
            self.inner.cluster_sectors[usize::from(part)].store(0, Ordering::Relaxed);
        }
        self.inner.changed();
    }

    /// The cache partitions (3 to 5) that have a file open, as a bit mask
    /// (bit N for partition N).
    #[must_use]
    pub fn open_cache_partitions(&self) -> u32 {
        let mut mask = 0;
        for path in lock(&self.inner.open).keys() {
            for part in 3..=5u8 {
                if path.starts_with(self.inner.partition_dir(part)) {
                    mask |= 1 << part;
                }
            }
        }
        mask
    }
}

/// Deletes everything under `path`, and `path` itself if `and_self`.
fn remove_tree(path: &Path, and_self: bool) {
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            let p = entry.path();
            if fs::symlink_metadata(&p).is_ok_and(|m| m.is_dir()) {
                remove_tree(&p, true);
            } else {
                let _ = fs::remove_file(&p);
            }
        }
    }
    if and_self {
        let _ = fs::remove_dir(path);
    }
}

/// Bytes the files under `dir` take in clusters of `cluster` bytes.
fn usage_of(dir: &Path, cluster: u64) -> u64 {
    let mut used = 0;
    let Ok(entries) = fs::read_dir(dir) else { return 0 };
    for entry in entries.flatten() {
        let Ok(md) = fs::symlink_metadata(entry.path()) else { continue };
        if md.is_dir() {
            used += usage_of(&entry.path(), cluster);
        } else if md.is_file() {
            used += md.len().div_ceil(cluster) * cluster;
        }
    }
    used
}

impl DiskInner {
    fn changed(&self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    /// The folder of the running title's cache partitions: on the console
    /// they are one title's at a time and wiped when another claims them;
    /// here each title has its own, so none finds another's files in its
    /// cache (Halo's maps under Halo 2's names) and a savestate can keep and
    /// put back exactly its title's. The root itself before a title is known.
    fn cache_root(&self) -> PathBuf {
        match self.cache_title.load(Ordering::Relaxed) {
            0 => self.root.clone(),
            id => self.root.join("cache").join(format!("{id:08x}")),
        }
    }

    fn partition_dir(&self, part: u8) -> PathBuf {
        if part >= 3 {
            return self.cache_root().join(format!("Partition{part}"));
        }
        self.root.join(format!("Partition{part}"))
    }

    fn overwrite_dir(&self) -> PathBuf {
        self.root.join(".overwrite")
    }

    fn raw_file(&self, part: u8) -> PathBuf {
        if part >= 3 {
            return self.cache_root().join(format!("raw-partition{part}.bin"));
        }
        self.root.join(format!("raw-partition{part}.bin"))
    }

    fn raw_read(&self, part: u8, offset: u64, buf: &mut [u8]) {
        buf.fill(0);
        let keep = RAW_KEEP.saturating_sub(offset).min(buf.len() as u64) as usize;
        if keep == 0 {
            return;
        }
        let Ok(file) = fs::File::open(self.raw_file(part)) else { return };
        if let Err(e) = sys::read_at(&file, &mut buf[..keep], offset) {
            log::warn!(target: "fs", "raw partition{part} read +{offset:x}: {e}");
        }
    }

    fn raw_write(&self, part: u8, offset: u64, data: &[u8]) {
        let keep = RAW_KEEP.saturating_sub(offset).min(data.len() as u64) as usize;
        if keep == 0 {
            return;
        }
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.raw_file(part));
        if let Err(e) = file.and_then(|f| sys::write_all_at(&f, &data[..keep], offset)) {
            log::warn!(target: "fs", "raw partition{part} write +{offset:x}: {e}");
        }
    }

    fn cluster_sectors(&self, part: u8) -> u32 {
        let slot = &self.cluster_sectors[usize::from(part)];
        if slot.load(Ordering::Relaxed) == 0 && part >= 3 {
            let mut sb = [0u8; 12];
            self.raw_read(part, 0, &mut sb);
            if &sb[..4] == b"FATX" {
                slot.store(u32::from_le_bytes([sb[8], sb[9], sb[10], sb[11]]), Ordering::Relaxed);
            }
        }
        match slot.load(Ordering::Relaxed) {
            0 => DEFAULT_CLUSTER_SECTORS,
            n => n,
        }
    }

    /// Host path for a path inside a partition, matching existing names in
    /// any case, as the console names them ([`name_from_host`]). A name that
    /// exists as typed is taken without listing its folder; otherwise the
    /// folder is listed and the first name that matches wins. Missing
    /// components are spelled as [`name_to_host`] has them.
    fn host_path(&self, part: u8, rel: &str) -> (PathBuf, Found) {
        let mut out = self.partition_dir(part);
        if !out.is_dir() {
            let _ = fs::create_dir_all(&out);
        }
        let mut parts = rel.split('\\').filter(|p| !p.is_empty()).peekable();
        while let Some(part) = parts.next() {
            let last = parts.peek().is_none();
            let encoded = name_to_host(part);
            let typed = out.join(&encoded);
            if fs::symlink_metadata(&typed).is_ok() {
                out = typed;
                continue;
            }
            let matched = fs::read_dir(&out).ok().and_then(|entries| {
                entries
                    .flatten()
                    .map(|e| e.file_name())
                    .find(|name| name_from_host(&name.to_string_lossy()).eq_ignore_ascii_case(part))
            });
            match matched {
                Some(name) => out.push(name),
                None => {
                    out.push(encoded);
                    return (out, if last { Found::LastMissing } else { Found::ParentMissing });
                }
            }
        }
        (out, Found::Exists)
    }

    /// Moves an overwritten file's old contents to `.overwrite`, outside every
    /// partition. The first time in a run, side files a dead process left
    /// there go.
    fn set_aside(&self, host: &Path) -> Option<PathBuf> {
        let dir = self.overwrite_dir();
        if !self.swept.swap(true, Ordering::Relaxed) {
            remove_tree(&dir, false);
        }
        let _ = fs::create_dir(&dir);
        let serial = self.serial.fetch_add(1, Ordering::Relaxed) + 1;
        let old = dir.join(format!("{}-{serial}", std::process::id()));
        match fs::rename(host, &old) {
            Ok(()) => Some(old),
            Err(e) => {
                log::warn!(target: "fs", "overwrite {}: keeping its old bytes failed: {e}", host.display());
                None
            }
        }
    }

    /// Empties a folder at once: its contents move aside whole and are
    /// deleted there by a thread of their own. Deleted in place, file by
    /// file, a cache drive's format (Project Gotham Racing 2's, as it starts)
    /// held the console as long as a slow drive took. In place if the move
    /// fails.
    fn empty_later(&self, host: &Path) {
        let dir = self.overwrite_dir();
        let _ = fs::create_dir(&dir);
        let serial = self.serial.fetch_add(1, Ordering::Relaxed);
        let aside = dir.join(format!("format-{}-{serial}", std::process::id()));
        self.changed();
        if fs::rename(host, &aside).is_err() || fs::create_dir(host).is_err() {
            remove_tree(host, false);
            return;
        }
        let moved = aside.clone();
        let spawned = std::thread::Builder::new()
            .name("xb-fs-format".into())
            .spawn(move || remove_tree(&moved, true));
        if spawned.is_err() {
            remove_tree(&aside, true);
        }
    }

    fn scan(&self, part: usize) {
        let generation = self.generation.load(Ordering::Relaxed);
        let formats = lock(&self.usage)[part].formats;
        let cluster = u64::from(self.cluster_sectors(part as u8)) * u64::from(HDD_SECTOR);
        let used = usage_of(&self.partition_dir(part as u8), cluster);
        self.scanned(part, used, generation, formats);
    }

    /// A scan's answer, begun at `generation` and `formats`. One begun
    /// before a format is dropped: the format's answer stands
    /// ([`DiskInner::used_none`]).
    fn scanned(&self, part: usize, used: u64, generation: u32, formats: u32) {
        let mut usage = lock(&self.usage);
        if usage[part].formats == formats {
            usage[part] = Usage {
                used,
                scanned: Some(Instant::now()),
                generation,
                formats,
                valid: true,
                scanning: false,
            };
        } else {
            usage[part].scanning = false;
        }
        self.usage_done.notify_all();
    }

    /// A file of `size` bytes was deleted from a partition: its clusters
    /// come off the use at once, not at a later scan's answer, as in the
    /// reference. A title that trims its cache until the drive reports room
    /// deletes an entry and asks again (Chase: Hollywood Stunt Driver): with
    /// the answer from before the deletes it deletes every entry and then
    /// faults on its empty list.
    fn used_drop(&self, part: u8, size: u64) {
        if !(1..=5).contains(&part) {
            return;
        }
        let cluster = u64::from(self.cluster_sectors(part)) * u64::from(HDD_SECTOR);
        let bytes = size.div_ceil(cluster) * cluster;
        let mut usage = lock(&self.usage);
        let u = &mut usage[usize::from(part)];
        if u.valid {
            u.used = u.used.saturating_sub(bytes);
        }
    }

    /// A partition was formatted: nothing is used, as of now, as in the
    /// reference. The scan made as the disk was opened saw what another
    /// title had left on a cache drive, and its answer would go on after
    /// the format (Chase: Hollywood Stunt Driver, given a drive another
    /// title had filled, formats it, is told it is full, and finds no entry
    /// of its own to delete).
    fn used_none(&self, part: u8) {
        if !(1..=5).contains(&part) {
            return;
        }
        let mut usage = lock(&self.usage);
        let u = &mut usage[usize::from(part)];
        u.used = 0;
        u.formats = u.formats.wrapping_add(1);
        u.generation = self.generation.load(Ordering::Relaxed);
        u.scanned = Some(Instant::now());
        u.valid = true;
        // A query waiting for the partition's first scan has its answer.
        self.usage_done.notify_all();
    }

    fn scan_in_background(self: &Arc<Self>, parts: &[usize]) -> bool {
        let (me, parts) = (Arc::clone(self), parts.to_vec());
        std::thread::Builder::new()
            .name("xb-fs-usage".into())
            .spawn(move || parts.iter().for_each(|&p| me.scan(p)))
            .is_ok()
    }

    fn prescan(self: &Arc<Self>) {
        let parts = [1, 2, 3, 4, 5];
        let mut usage = lock(&self.usage);
        parts.iter().for_each(|&p| usage[p].scanning = true);
        if !self.scan_in_background(&parts) {
            parts.iter().for_each(|&p| usage[p].scanning = false);
        }
    }

    fn part_used(self: &Arc<Self>, part: u8) -> u64 {
        let part = usize::from(part).min(5);
        let mut usage = lock(&self.usage);
        if !usage[part].valid {
            if !usage[part].scanning {
                usage[part].scanning = true;
                drop(usage);
                self.scan(part);
                usage = lock(&self.usage);
            }
            while !usage[part].valid {
                usage =
                    self.usage_done.wait(usage).unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        } else if usage[part].generation != self.generation.load(Ordering::Relaxed)
            && !usage[part].scanning
            && usage[part].scanned.is_none_or(|t| t.elapsed() > USAGE_RESCAN)
        {
            // The answer is the last scan's meanwhile.
            usage[part].scanning = self.scan_in_background(&[part]);
        }
        usage[part].used
    }

    /// The size a partition reports: its own, less what its files hold (a
    /// title filling a cache drive sees it fill), and no more free than the
    /// host has. Partitions other than 1 to 5 report as partition 1.
    fn size_information(self: &Arc<Self>, part: u8) -> FileFsSizeInformation {
        let part = if (1..=5).contains(&part) { part } else { 1 };
        let sectors = self.cluster_sectors(part);
        let cluster = u64::from(sectors) * u64::from(HDD_SECTOR);
        let size = PARTITIONS[usize::from(part)].1;
        let mut free = size.saturating_sub(self.part_used(part));
        if let Some(host_free) = sys::free_bytes(&self.root) {
            free = free.min(host_free);
        }
        FileFsSizeInformation {
            total_allocation_units: size / cluster,
            available_allocation_units: free / cluster,
            sectors_per_allocation_unit: sectors,
            bytes_per_sector: HDD_SECTOR,
        }
    }
}

/// A hard disk partition with a file system: a host folder.
struct HostVolume {
    disk: Arc<DiskInner>,
    part: u8,
}

impl HostVolume {
    /// An open whose `FILE_DIRECTORY_FILE` / `FILE_NON_DIRECTORY_FILE` does
    /// not fit what the path is: logged (the first 20), as the title may
    /// retry it.
    fn dir_mismatch(&self, rel: &str, options: u32, status: NtStatus) -> NtStatus {
        if self.disk.mismatch_logged.fetch_add(1, Ordering::Relaxed) < 20 {
            log::info!(target: "fs", "open Partition{}{rel} (options {options:x}): {status}", self.part);
        }
        status
    }
}

impl Volume for HostVolume {
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
        let disk = &self.disk;
        let (host, found) = disk.host_path(self.part, rel);
        if found == Found::ParentMissing {
            return Err(NtStatus::OBJECT_PATH_NOT_FOUND);
        }
        let exists = found == Found::Exists;
        let metadata = if exists { fs::metadata(&host).ok() } else { None };
        let mut is_dir = metadata.as_ref().is_some_and(fs::Metadata::is_dir) || rel.is_empty();
        let (opts, disposition) = (params.options, params.disposition);
        let granted = share_granted(params.desired_access);
        let allowed = (params.share_access & 7) as u8;

        let mut open = lock(&disk.open);
        let shared = if exists && !is_dir { open.get(&host).cloned() } else { None };
        if shared.as_ref().is_some_and(|s| s.delete.load(Ordering::Relaxed)) {
            log::info!(target: "fs", "open Partition{}{rel}: delete pending", self.part);
            return Err(NtStatus::DELETE_PENDING);
        }
        if exists && disposition == CreateDisposition::Create {
            return Err(NtStatus::OBJECT_NAME_COLLISION);
        }
        if let Some(s) = &shared
            && lock(&s.handles)
                .iter()
                .any(|h| share_conflict((granted, allowed), (h.granted, h.allowed)))
        {
            return Err(NtStatus::SHARING_VIOLATION);
        }
        if !exists && matches!(disposition, CreateDisposition::Open | CreateDisposition::Overwrite)
        {
            return Err(NtStatus::OBJECT_NAME_NOT_FOUND);
        }
        if exists && is_dir && opts & options::FILE_NON_DIRECTORY_FILE != 0 {
            return Err(self.dir_mismatch(rel, opts, NtStatus::FILE_IS_A_DIRECTORY));
        }
        if exists && !is_dir && opts & options::FILE_DIRECTORY_FILE != 0 {
            return Err(self.dir_mismatch(rel, opts, NtStatus::NOT_A_DIRECTORY));
        }
        if !exists
            && disk.enforce_fatx.load(Ordering::Relaxed)
            && !fatx_name_valid(rel.rsplit('\\').next().unwrap_or(""))
        {
            return Err(NtStatus::OBJECT_NAME_INVALID);
        }

        let mut file = None;
        let mut old = None;
        let action;
        if !exists && opts & options::FILE_DIRECTORY_FILE != 0 {
            if fs::create_dir(&host).is_err() {
                return Err(NtStatus::ACCESS_DENIED);
            }
            is_dir = true;
            action = CreateAction::Created;
        } else if !is_dir {
            let mut how = fs::OpenOptions::new();
            how.read(true).write(true);
            if !exists {
                how.create(true);
                action = CreateAction::Created;
            } else if disposition.overwrites() {
                // Empty now; the old bytes wait in a side file (`HostNode::old`).
                let old_size = metadata.as_ref().map_or(0, fs::Metadata::len);
                old = disk.set_aside(&host).map(|path| (path, old_size));
                how.create(true).truncate(true);
                action = if disposition == CreateDisposition::Supersede {
                    CreateAction::Superseded
                } else {
                    CreateAction::Overwritten
                };
            } else {
                action = CreateAction::Opened;
            }
            match how.open(&host) {
                Ok(f) => file = Some(f),
                Err(e) => {
                    log::info!(target: "fs", "open Partition{}{rel} ({}): {e}", self.part, host.display());
                    if let Some((old_path, _)) = old {
                        let _ = fs::rename(old_path, &host); // the file as it was
                    }
                    return Err(NtStatus::ACCESS_DENIED);
                }
            }
        } else {
            action = CreateAction::Opened;
        }

        let id = disk.next_id.fetch_add(1, Ordering::Relaxed);
        let shared = (!is_dir).then(|| {
            let shared = shared.unwrap_or_else(|| {
                let s = Arc::new(Shared {
                    path: Mutex::new(host.clone()),
                    handles: Mutex::new(Vec::new()),
                    delete: AtomicBool::new(false),
                });
                open.insert(host.clone(), Arc::clone(&s));
                s
            });
            lock(&shared.handles).push(Handle { id, granted, allowed });
            shared
        });
        drop(open);
        if action != CreateAction::Opened {
            disk.changed();
        }
        Ok((
            Box::new(HostNode {
                disk: Arc::clone(disk),
                part: AtomicU8::new(self.part),
                id,
                file,
                shared,
                dir_path: Mutex::new(host),
                is_root: rel.is_empty(),
                old: Mutex::new(old),
                wrote: AtomicBool::new(false),
            }),
            action,
        ))
    }

    fn delete(&self, rel: &str) -> NtStatus {
        let disk = &self.disk;
        disk.changed();
        let (host, found) = disk.host_path(self.part, rel);
        if found != Found::Exists {
            return NtStatus::OBJECT_NAME_NOT_FOUND;
        }
        let is_dir = fs::metadata(&host).is_ok_and(|m| m.is_dir());
        let open = lock(&disk.open);
        if !is_dir && let Some(shared) = open.get(&host) {
            shared.delete.store(true, Ordering::Relaxed); // it goes with the last handle on it
            log::info!(target: "fs", "delete Partition{}{rel}: at its last close", self.part);
            return NtStatus::SUCCESS;
        }
        let size = if is_dir { None } else { fs::metadata(&host).ok().map(|m| m.len()) };
        let done = if is_dir { fs::remove_dir(&host) } else { fs::remove_file(&host) };
        match done {
            Ok(()) => {
                if let Some(size) = size {
                    disk.used_drop(self.part, size);
                }
                log::info!(target: "fs", "delete Partition{}{rel}: done", self.part);
                NtStatus::SUCCESS
            }
            Err(e) => {
                log::info!(target: "fs", "delete Partition{}{rel}: {e}", self.part);
                NtStatus::ACCESS_DENIED
            }
        }
    }

    fn rename(
        &self,
        file: &dyn VolumeFile,
        dest: &dyn Volume,
        dest_rel: &str,
        replace_if_exists: bool,
    ) -> NtStatus {
        let disk = &self.disk;
        let Some(node) = file.as_any().downcast_ref::<HostNode>() else {
            return NtStatus::ACCESS_DENIED;
        };
        disk.changed();
        let Some(dest) =
            dest.as_any().downcast_ref::<HostVolume>().filter(|d| Arc::ptr_eq(&d.disk, disk))
        else {
            return NtStatus::OBJECT_PATH_NOT_FOUND;
        };
        let (host, found) = disk.host_path(dest.part, dest_rel);
        match found {
            Found::ParentMissing => return NtStatus::OBJECT_PATH_NOT_FOUND,
            Found::Exists if !replace_if_exists => return NtStatus::OBJECT_NAME_COLLISION,
            _ => {}
        }
        let mut open = lock(&disk.open);
        let from = node.host();
        if fs::rename(&from, &host).is_err() {
            return NtStatus::ACCESS_DENIED;
        }
        match &node.shared {
            Some(shared) => {
                if open.get(&from).is_some_and(|s| Arc::ptr_eq(s, shared)) {
                    open.remove(&from);
                }
                *lock(&shared.path) = host.clone();
                open.insert(host, Arc::clone(shared));
            }
            None => *lock(&node.dir_path) = host,
        }
        node.part.store(dest.part, Ordering::Relaxed);
        NtStatus::SUCCESS
    }

    fn size_information(&self) -> FileFsSizeInformation {
        self.disk.size_information(self.part)
    }

    fn host_file(&self, rel: &str) -> Option<PathBuf> {
        if !(1..=5).contains(&self.part) || rel.is_empty() {
            return None;
        }
        let (host, found) = self.disk.host_path(self.part, rel);
        (found == Found::Exists && fs::metadata(&host).is_ok_and(|m| m.is_file())).then_some(host)
    }
}

/// An open file or directory on a hard disk partition.
struct HostNode {
    disk: Arc<DiskInner>,
    /// The partition the file is on now (a rename may move it).
    part: AtomicU8,
    id: u64,
    /// The host file; `None` for a directory.
    file: Option<fs::File>,
    /// `None` for a directory.
    shared: Option<Arc<Shared>>,
    /// A directory's host path.
    dir_path: Mutex<PathBuf>,
    is_root: bool,
    /// Opened with an overwrite disposition: the old contents moved to a side
    /// file outside the partitions (its path and size), and the file itself
    /// is empty on the host from the start, so the disk always holds what the
    /// guest sees. FATX does not zero what a file grows back into: growing it
    /// again (`SetEndOfFile`, a write past the end) brings the old bytes back
    /// in that range. Forza trims its unpacked cabs so. The side file goes
    /// when the handle closes.
    old: Mutex<Option<(PathBuf, u64)>>,
    wrote: AtomicBool,
}

impl HostNode {
    fn host(&self) -> PathBuf {
        match &self.shared {
            Some(shared) => lock(&shared.path).clone(),
            None => lock(&self.dir_path).clone(),
        }
    }

    fn part(&self) -> u8 {
        self.part.load(Ordering::Relaxed)
    }

    fn metadata(&self) -> Option<fs::Metadata> {
        match &self.file {
            Some(file) => file.metadata().ok(),
            None => fs::metadata(self.host()).ok(),
        }
    }

    /// An overwritten file growing into [from, to): FATX shows what its
    /// clusters held, the old contents.
    fn regrow(&self, file: &fs::File, from: u64, to: u64) {
        let old = lock(&self.old);
        let Some((path, old_size)) = old.as_ref() else { return };
        let to = to.min(*old_size);
        let Ok(side) = fs::File::open(path) else { return };
        let mut buf = vec![0u8; 1 << 16];
        let mut at = from;
        while at < to {
            let want = ((to - at) as usize).min(buf.len());
            match sys::read_at(&side, &mut buf[..want], at) {
                Ok(n) if n > 0 && sys::write_all_at(file, &buf[..n], at).is_ok() => at += n as u64,
                _ => break,
            }
        }
    }
}

impl VolumeFile for HostNode {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn is_dir(&self) -> bool {
        self.file.is_none()
    }

    fn is_raw(&self) -> bool {
        // A cache drive read or formatted raw (XMountUtilityDrive).
        self.is_root && self.part() >= 3
    }

    fn size(&self) -> u64 {
        self.file.as_ref().and_then(|f| f.metadata().ok()).map_or(0, |m| m.len())
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, NtStatus> {
        if self.is_raw() {
            self.disk.raw_read(self.part(), offset, buf);
            return Ok(buf.len());
        }
        let file = self.file.as_ref().ok_or(NtStatus::INVALID_DEVICE_REQUEST)?;
        sys::read_at(file, buf, offset).map_err(|e| {
            log::info!(target: "fs", "read {} +{offset:x} ({}) failed: {e}", self.host().display(), buf.len());
            NtStatus::DEVICE_DATA_ERROR
        })
    }

    fn write_at(&self, offset: u64, data: &[u8]) -> Result<usize, NtStatus> {
        self.disk.changed();
        if self.is_raw() {
            let part = self.part();
            self.disk.raw_write(part, offset, data);
            // The FATX superblock: "FATX", volume id, sectors per cluster.
            // Writing it empties the folder; the head of the partition is kept.
            if offset == 0 && data.len() >= 12 && &data[..4] == b"FATX" {
                let sectors = u32::from_le_bytes([data[8], data[9], data[10], data[11]]);
                self.disk.cluster_sectors[usize::from(part)].store(sectors, Ordering::Relaxed);
                let host = self.host();
                log::info!(target: "fs", "Partition{part} formatted ({sectors}-sector clusters): emptied {}", host.display());
                self.disk.empty_later(&host);
                self.disk.used_none(part);
            }
            return Ok(data.len());
        }
        let file = self.file.as_ref().ok_or(NtStatus::ACCESS_DENIED)?;
        let size = self.size();
        if offset > size {
            self.regrow(file, size, offset); // the gap a write past the end leaves
        }
        match sys::write_all_at(file, data, offset) {
            Ok(()) => {
                if !data.is_empty() {
                    self.wrote.store(true, Ordering::Relaxed);
                }
                Ok(data.len())
            }
            Err(e) => {
                log::info!(target: "fs", "write {} +{offset:x} ({}) failed: {e}", self.host().display(), data.len());
                Err(NtStatus::DISK_FULL)
            }
        }
    }

    fn set_end_of_file(&self, size: u64) -> NtStatus {
        self.disk.changed();
        let Some(file) = &self.file else { return NtStatus::ACCESS_DENIED };
        let was = self.size();
        if file.set_len(size).is_err() {
            return NtStatus::ACCESS_DENIED;
        }
        self.wrote.store(true, Ordering::Relaxed);
        if size > was {
            self.regrow(file, was, size);
        }
        NtStatus::SUCCESS
    }

    fn flush(&self) -> NtStatus {
        if let Some(file) = &self.file
            && let Err(e) = file.sync_all()
        {
            log::info!(target: "fs", "sync {}: {e}", self.host().display());
        }
        NtStatus::SUCCESS
    }

    fn facts(&self) -> FileFacts {
        match self.metadata() {
            Some(md) => {
                let secs = |t: io::Result<std::time::SystemTime>| {
                    filetime_from_unix(t.map(sys::unix_secs).unwrap_or(0), 0)
                };
                let write = secs(md.modified());
                FileFacts {
                    creation_time: filetime_from_unix(sys::creation_secs(&md), 0),
                    last_access_time: secs(md.accessed()),
                    last_write_time: write,
                    change_time: write,
                    size: if md.is_dir() { 0 } else { md.len() },
                    attributes: if md.is_dir() {
                        attributes::FILE_ATTRIBUTE_DIRECTORY
                    } else {
                        attributes::FILE_ATTRIBUTE_NORMAL
                    },
                }
            }
            None => FileFacts {
                creation_time: DISC_TIME,
                last_access_time: DISC_TIME,
                last_write_time: DISC_TIME,
                change_time: DISC_TIME,
                size: self.size(),
                attributes: if self.is_dir() {
                    attributes::FILE_ATTRIBUTE_DIRECTORY
                } else {
                    attributes::FILE_ATTRIBUTE_NORMAL
                },
            },
        }
    }

    fn index_number(&self) -> u64 {
        self.metadata().map_or(0, |md| sys::inode(&md))
    }

    fn sort_key(&self) -> SortKey {
        if self.is_dir() {
            return SortKey::None;
        }
        SortKey::Fatx { first_cluster: self.index_number() as u32 }
    }

    fn set_times(&self, last_access: Option<u64>, last_write: Option<u64>) -> NtStatus {
        self.disk.changed();
        // Forza's installer stamps each copy with its source's time and
        // recopies a Z: file whose LastWriteTime is over 4 s off it.
        if let Some(file) = &self.file {
            let mut times = fs::FileTimes::new();
            if let Some(t) = last_access {
                times = times.set_accessed(filetime_to_system_time(t));
            }
            if let Some(t) = last_write {
                times = times.set_modified(filetime_to_system_time(t));
            }
            if (last_access.is_some() || last_write.is_some())
                && let Err(e) = file.set_times(times)
            {
                log::info!(target: "fs", "set times on {}: {e}", self.host().display());
            }
        }
        NtStatus::SUCCESS
    }

    fn set_delete_on_close(&self, delete: bool) -> NtStatus {
        self.disk.changed();
        match &self.shared {
            // The file stays, as the Xbox's does, until the last handle on it
            // closes. Sudeki deletes its boot.baf with a handle on it still open.
            Some(shared) => {
                shared.delete.store(delete, Ordering::Relaxed);
                NtStatus::SUCCESS
            }
            None if delete => match fs::remove_dir(self.host()) {
                Ok(()) => NtStatus::SUCCESS,
                Err(e) if e.kind() == io::ErrorKind::DirectoryNotEmpty => {
                    NtStatus::DIRECTORY_NOT_EMPTY
                }
                Err(_) => NtStatus::ACCESS_DENIED,
            },
            None => NtStatus::SUCCESS,
        }
    }

    fn delete_pending(&self) -> bool {
        self.shared.as_ref().is_some_and(|s| s.delete.load(Ordering::Relaxed))
    }

    fn list(&self, keep: &dyn Fn(&str) -> bool) -> Vec<DirEntry> {
        let mut out = Vec::new();
        let Ok(entries) = fs::read_dir(self.host()) else { return out };
        for entry in entries.flatten() {
            let name = name_from_host(&entry.file_name().to_string_lossy());
            if name.chars().count() >= LISTING_NAME_MAX || out.len() >= LISTING_MAX || !keep(&name)
            {
                continue;
            }
            let md = fs::metadata(entry.path()).ok();
            let is_dir = md.as_ref().is_some_and(fs::Metadata::is_dir);
            let size = if is_dir { 0 } else { md.map_or(0, |m| m.len()) };
            out.push(DirEntry { name, size, is_dir });
        }
        out
    }

    fn wrote(&self) -> bool {
        self.wrote.load(Ordering::Relaxed)
    }

    fn sync_on_close(&self) -> bool {
        // The saves' partitions; a cache drive's files are not synced: the
        // console wipes them as it likes, and a game filling one (Project
        // Gotham Racing 2's cars) synced file after file.
        self.file.is_some() && self.wrote() && !(3..=5).contains(&self.part())
    }
}

impl Drop for HostNode {
    fn drop(&mut self) {
        drop(self.file.take());
        let old = self.old.get_mut().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((old_path, _)) = old.take() {
            let _ = fs::remove_file(old_path);
        }
        let Some(shared) = &self.shared else { return };
        let mut open = lock(&self.disk.open);
        let mut handles = lock(&shared.handles);
        handles.retain(|h| h.id != self.id);
        if handles.is_empty() {
            let path = lock(&shared.path).clone();
            if open.get(&path).is_some_and(|s| Arc::ptr_eq(s, shared)) {
                open.remove(&path);
            }
            if shared.delete.load(Ordering::Relaxed) {
                // The last handle deletes it.
                let size = fs::metadata(&path).ok().map(|m| m.len());
                if fs::remove_file(&path).is_ok()
                    && let Some(size) = size
                {
                    self.disk.used_drop(self.part(), size);
                }
                self.disk.changed();
            }
        }
    }
}

/// `\Device\Harddisk0\Partition0`: the whole disk, opened raw for its config
/// sectors. They read as zeros until written (no refurb or cache records).
struct RawDiskVolume {
    disk: Arc<DiskInner>,
}

impl Volume for RawDiskVolume {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::HardDisk
    }

    fn open(
        &self,
        rel: &str,
        _params: &OpenParams,
    ) -> Result<(Box<dyn VolumeFile>, CreateAction), NtStatus> {
        if !rel.is_empty() {
            return Err(NtStatus::OBJECT_NAME_NOT_FOUND);
        }
        Ok((Box::new(RawDiskNode { disk: Arc::clone(&self.disk) }), CreateAction::Opened))
    }

    fn delete(&self, _rel: &str) -> NtStatus {
        NtStatus::ACCESS_DENIED // no file system to delete in
    }

    fn rename(&self, _: &dyn VolumeFile, _: &dyn Volume, _: &str, _: bool) -> NtStatus {
        NtStatus::ACCESS_DENIED
    }

    fn size_information(&self) -> FileFsSizeInformation {
        self.disk.size_information(0)
    }
}

struct RawDiskNode {
    disk: Arc<DiskInner>,
}

impl VolumeFile for RawDiskNode {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn is_dir(&self) -> bool {
        false
    }

    fn is_volume(&self) -> bool {
        true
    }

    fn is_raw(&self) -> bool {
        true
    }

    fn size(&self) -> u64 {
        0
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, NtStatus> {
        self.disk.raw_read(0, offset, buf);
        Ok(buf.len())
    }

    fn write_at(&self, offset: u64, data: &[u8]) -> Result<usize, NtStatus> {
        self.disk.changed();
        self.disk.raw_write(0, offset, data);
        Ok(data.len())
    }

    fn set_end_of_file(&self, _size: u64) -> NtStatus {
        NtStatus::ACCESS_DENIED
    }

    fn flush(&self) -> NtStatus {
        NtStatus::SUCCESS
    }

    fn facts(&self) -> FileFacts {
        FileFacts {
            creation_time: DISC_TIME,
            last_access_time: DISC_TIME,
            last_write_time: DISC_TIME,
            change_time: DISC_TIME,
            size: 0,
            attributes: attributes::FILE_ATTRIBUTE_READONLY,
        }
    }

    fn index_number(&self) -> u64 {
        0
    }

    fn sort_key(&self) -> SortKey {
        SortKey::None
    }

    fn set_times(&self, _: Option<u64>, _: Option<u64>) -> NtStatus {
        NtStatus::SUCCESS
    }

    fn set_delete_on_close(&self, delete: bool) -> NtStatus {
        if delete { NtStatus::ACCESS_DENIED } else { NtStatus::SUCCESS }
    }

    fn list(&self, _keep: &dyn Fn(&str) -> bool) -> Vec<DirEntry> {
        Vec::new()
    }
}
