//! Files, devices and the object namespace: the `Nt*` and `Io*` exports over
//! the storage crate `xb-fs`.
//!
//! The storage half (the namespace, the volumes, the information classes'
//! layouts, the drives' cost) is `xb-fs`; `docs/hw/io-namespace.md` says which
//! rule is whose.
//!
//! Devices: `\Device\CdRom0` is the title's disc (XDVDFS, read-only);
//! `\Device\Harddisk0\PartitionN` are host folders under the hard disk's
//! root. `\??\X:` names are symbolic links; `D:` starts out at the disc.
//!
//! Xbox names are ANSI: OBJECT_ATTRIBUTES { RootDirectory, ObjectName
//! (ANSI_STRING*), Attributes }. A FILE_OBJECT as the Xbox lays it out: Type
//! +0, FsContext +8, Lock (a KEVENT) +0x28, Event (a KEVENT,
//! [`FO_EVENT`]) +0x38; past its 0x48 bytes, the file system's control block
//! FsContext points at. XAPI's XGetFilePhysicalSortKey reads it by the
//! volume's name: GDFX, the start sector at +0; FATX, flags at +0 (bit 0: no
//! key) and the first cluster at +0x1C.
//!
//! Differences from the reference:
//!
//! * No host pointer into guest memory, and the workers never touch it. A
//!   worker reads into a host buffer; the guest-CPU thread stores it when the
//!   request completes (`io_poll`), which also reports the store to write
//!   tracking. A write's bytes are read from guest memory at the call.
//! * The drives' time is guest time: a request carries a due time, queued
//!   behind its drive's previous request as the reference's reader thread
//!   serves one at a time, and the finished request is held until the guest's
//!   clock reaches it. No thread sleeps for it.
//! * An open file is found by its FILE_OBJECT's address, not by an index
//!   kept in guest memory.
//! * A file the title wrote is synced by the closer thread through a
//!   descriptor of the closer's own, opened by the host path: the handle
//!   itself, and its share of the file, close in the call.

mod disc;
mod disk;
mod saved;
mod worker;

pub use disk::{DiskFile, DiskState, Hash as DiskHash, hash_name as disk_hash_name};

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use xb_formats::block::{BlockSource, FileSource};
use xb_fs::ioctl::{self, Control, ScsiPassThroughDirect};
use xb_fs::types::{
    FileCompletionInformation, FileRenameInformation, OB_DOS_DEVICES_DIRECTORY, ansi_to_string,
    file_class, string_to_ansi,
};
use xb_fs::{
    CreateDisposition, Device, DeviceKind, DeviceRef, DeviceTiming, DiscTree, File, FileSystem,
    NtStatus, OpenParams, Root, SYNC_LATER_MIN, SortKey, TimingMode, device_of,
};
use xb_types::PAGE_SIZE;

use crate::apc::Apc;
use crate::gm::{Fault, KResult};
use crate::ke::FO_EVENT;
use crate::kernel::{
    Args, Kernel, ObType, STATUS_INSUFFICIENT_RESOURCES, STATUS_INVALID_HANDLE,
    STATUS_INVALID_PARAMETER, STATUS_PENDING, STATUS_SUCCESS,
};
use crate::platform::Platform;
use crate::ps::ThreadState;

use self::disc::XdvdfsTree;
use self::worker::{CloseJob, DISC, HARD_DISK, Job, Workers};

const FILE_OBJECT_SIZE: u32 = 0x70;
const FO_FSCONTEXT: u32 = 0x08;
const FO_LOCK: u32 = 0x28;
const FO_FCB: u32 = 0x4C;
const FCB_FATX_FLAGS: u32 = 0x00;
const FCB_FATX_CLUSTER: u32 = 0x1C;
/// FILE_OBJECT.Type.
const IO_TYPE_FILE: u16 = 5;

/// DEVICE_OBJECT (nxdk xboxkrnl.h, CC0): Type 0x00, Size 0x02, ReferenceCount
/// 0x04, DriverObject 0x08, MountedOrSelfDevice 0x0C, CurrentIrp 0x10, Flags
/// 0x14, DeviceExtension 0x18, DeviceType 0x1C, StartIoFlags 0x1D, StackSize
/// 0x1E, DeletePending 0x1F, SectorSize 0x20, AlignmentRequirement 0x24,
/// DeviceQueue 0x28 (KDEVICE_QUEUE: Type, Size, Busy, DeviceListHead),
/// DeviceLock 0x34 (KEVENT), StartIoKey 0x44; 0x48 bytes, the extension after
/// it.
const DEVOBJ_SIZE: u32 = 0x48;
const DO_DEVICE_INITIALIZING: u32 = 0x80;
/// Stands for a device being created in the name table, until its object
/// exists.
const DEVICE_PLACEHOLDER: u32 = u32::MAX;

/// Requests a drive's worker holds at most, queued, in work or done and not
/// yet completed; past it a request runs in place.
const DRIVE_JOBS: u32 = 64;

/// What a file or symbolic-link object stands for.
enum Kind {
    /// An open file, directory or device.
    File(Arc<File>),
    /// An open symbolic link: its target when it was opened.
    Link(String),
}

/// The host's side of a FILE_OBJECT.
struct Open {
    kind: Kind,
    /// Tells this open from an earlier one at the same address.
    serial: u64,
    /// Its I/O completion port (FileCompletionInformation): the port's
    /// object and key, where its reads and writes queue a packet as they
    /// complete.
    port: Option<(u32, u32)>,
    /// Its requests queued and not yet completed.
    jobs: u32,
}

/// The I/O exports' state.
pub(crate) struct State {
    /// The namespace; `None` until `io_mount` or the first use.
    fs: Option<Arc<FileSystem>>,
    /// The disc in the tray.
    disc: Option<Arc<dyn DiscTree>>,
    /// Open files and links by their object's address.
    files: HashMap<u32, Open>,
    serial: u64,
    /// The drives' speeds, and whether synchronous reads wait for them.
    timing: DeviceTiming,
    /// The mode the running title needs whatever the options say, if any.
    title_timing: Option<TimingMode>,
    workers: Option<Workers>,
    /// Requests the workers finished, held for the drive's time or for those
    /// asked before them.
    held: Vec<Job>,
    /// Hashes of the hard disk's files a savestate worked out.
    disk: disk::Index,
    /// Each drive's requests queued and not yet completed.
    busy: [u32; 2],
    /// When each drive's last timed request ends (guest ns).
    drive_free: [u64; 2],
    /// Overlapped requests queued, and completed in that order.
    async_seq: u64,
    async_done: u64,
    /// The earliest due time among the held requests.
    next_due: Option<u64>,
    /// Control codes told as unknown, each once.
    control_told: Vec<u32>,
    gone_logged: bool,
    /// What the drives did since the last statistics line.
    stats: [DriveStats; 2],
}

/// One drive's requests since the last statistics line (DEBUG): what the
/// timing model charged against what the title actually waited, so a load
/// slower than the console's shows where the time went.
#[derive(Clone, Copy, Default)]
struct DriveStats {
    /// Requests that went to the worker, and those done in the call.
    queued: u32,
    in_place: u32,
    /// Bytes read and written, both ways.
    bytes: u64,
    /// The drive time the model charged the queued requests.
    charged_ns: u64,
    /// Host time from the call to the completion, summed over them.
    wall_ns: u64,
    /// Host time the worker's reads and writes took.
    host_ns: u64,
    /// Guest time past the due time at completion: the sum and the worst.
    late_ns: u64,
    late_max_ns: u64,
    /// Host time the guest CPU idled with a request of this drive in flight.
    idle_ns: u64,
}

impl Default for State {
    fn default() -> Self {
        Self {
            fs: None,
            disc: None,
            files: HashMap::new(),
            serial: 0,
            // until the title's timing is set: no drive time for synchronous
            // reads, a quarter of it for overlapped ones (the reference's initial rates)
            timing: DeviceTiming::for_mode(TimingMode::Off),
            title_timing: None,
            workers: None,
            held: Vec::new(),
            disk: disk::Index::default(),
            busy: [0; 2],
            drive_free: [0; 2],
            async_seq: 0,
            async_done: 0,
            next_due: None,
            control_told: Vec::new(),
            gone_logged: false,
            stats: [DriveStats::default(); 2],
        }
    }
}

/// A read or write for a drive's worker.
struct Request {
    write: bool,
    off: u64,
    n: u32,
    runs: Vec<(u32, u32)>,
    data: Vec<u8>,
    /// The calling thread waits for it.
    sync: bool,
    /// The drive's time for it.
    cost: Duration,
}

fn ok(v: u32) -> KResult<u64> {
    Ok(u64::from(v))
}

/// The bytes of the counted string at `s` as a name, at most `cap - 1` of
/// them and up to a NUL.
fn ansi(k: &Kernel, s: u32, cap: usize) -> KResult<String> {
    if s == 0 {
        return Ok(String::new());
    }
    let mut b = k.gm.ansi_string(s)?;
    b.truncate(cap - 1);
    if let Some(n) = b.iter().position(|&c| c == 0) {
        b.truncate(n);
    }
    Ok(ansi_to_string(&b))
}

/// The guest has every page of `[va, va + len)`, or the fault a store or
/// load there takes: checked before a call has any effect, so the title's
/// handler can commit the page and the call runs again (Rainbow Six:
/// Lockdown).
fn resident(k: &Kernel, va: u32, len: u32, write: bool) -> KResult<()> {
    if len == 0 || k.gm.mapped_range(va, len) {
        return Ok(());
    }
    let last = (u64::from(va) + u64::from(len) - 1) / u64::from(PAGE_SIZE);
    let missing = (u64::from(va / PAGE_SIZE)..=last)
        .map(|p| (p as u32).wrapping_mul(PAGE_SIZE))
        .find(|p| !k.gm.mapped(*p))
        .unwrap_or(va);
    Err(Fault { va: missing.max(va), write })
}

fn resident_runs(k: &Kernel, runs: &[(u32, u32)]) -> KResult<()> {
    runs.iter().try_for_each(|&(va, len)| resident(k, va, len, true))
}

/// An IO_STATUS_BLOCK the call will write.
fn iosb_resident(k: &Kernel, iosb: u32) -> KResult<()> {
    if iosb == 0 { Ok(()) } else { resident(k, iosb, 8, true) }
}

/// The runs' first `n` bytes: the runs that hold them, the last cut to fit.
fn runs_cut(runs: &mut Vec<(u32, u32)>, mut n: u32) {
    let mut keep = 0;
    for r in runs.iter_mut() {
        if n == 0 {
            break;
        }
        r.1 = r.1.min(n);
        n -= r.1;
        keep += 1;
    }
    runs.truncate(keep);
}

/// Stores `data` into the guest buffer's runs, in order.
fn store_runs(k: &Kernel, runs: &[(u32, u32)], data: &[u8]) -> KResult<()> {
    let mut at = 0usize;
    for &(va, len) in runs {
        let end = (at + len as usize).min(data.len());
        k.gm.write(va, &data[at..end])?;
        at = end;
    }
    Ok(())
}

/// pended_status: an overlapped request that pends reads STATUS_PENDING in
/// its IoStatusBlock until it completes, as the console's kernel leaves it:
/// XAPI's ReadFileEx does not set the OVERLAPPED itself, and a title that
/// checks it (HasOverlappedIoCompleted) and reuses it took the last read's
/// SUCCESS as this one done. The Nightmare Before Christmas: Oogie's Revenge
/// reads a level's packed file in 256 KiB blocks from threads that end at
/// once, one OVERLAPPED for all, and unpacked a block the drive had not read
/// yet.
fn io_status(k: &Kernel, iosb: u32, status: u32, info: u32) -> KResult<()> {
    if iosb != 0 {
        k.gm.w32(iosb, status)?;
        k.gm.w32(iosb + 4, info)?;
    }
    Ok(())
}

/// Reads and writes of this handle address raw sectors: the raw disk's
/// config area, or a cache partition's root (XMountUtilityDrive formats it
/// so).
fn raw_device(file: &File) -> bool {
    match file.device() {
        Device::Partition(0) => file.is_volume(),
        Device::Partition(3..) => {
            file.is_dir() && device_of(&file.path()).is_some_and(|(_, rel)| rel.is_empty())
        }
        _ => false,
    }
}

/// A file or directory on a hard disk partition with a file system.
fn hard_disk_file(file: &File) -> bool {
    file.device_kind() == DeviceKind::HardDisk && !file.is_volume()
}

/// The running thread may wait in its call: an ordinary thread at
/// PASSIVE_LEVEL, not a DPC or an ISR.
fn can_block(k: &Kernel) -> bool {
    k.current.is_some_and(|t| {
        k.threads[t].state == ThreadState::Running
            && Some(t) != k.timer.dpc_thread
            && Some(t) != k.intr.isr_thread
    }) && !k.intr.in_isr
        && !k.redirected
        && k.irql() == 0
}

impl Kernel {
    /// Builds the I/O namespace: the hard disk's partitions under the host
    /// folder `hdd_root`, the disc (the plain image, if one is in the tray)
    /// at `\Device\CdRom0`, and the links `\??\D:` and `\??\CdRom0:`. An
    /// image with no XDVDFS volume counts as an empty tray.
    pub(crate) fn io_mount(
        &mut self,
        disc: Option<Arc<dyn BlockSource>>,
        hdd_root: &Path,
    ) -> Result<(), String> {
        let fs = FileSystem::with_hard_disk(hdd_root)
            .map_err(|e| format!("the hard disk's folder {}: {e}", hdd_root.display()))?;
        let tree = disc.and_then(|image| match XdvdfsTree::open(image) {
            Ok(tree) => Some(Arc::new(tree) as Arc<dyn DiscTree>),
            Err(e) => {
                log::info!(target: "fs", "the disc image has no file system ({e}): the tray is empty");
                None
            }
        });
        if let Some(tree) = &tree {
            fs.insert_disc(Arc::clone(tree));
        }
        self.io.disc = tree;
        self.io.fs = Some(Arc::new(fs));
        Ok(())
    }

    /// `\??\D:`, the directory the title was launched from (its XBE's).
    pub(crate) fn io_set_launch_dir(&mut self, dir: &str) {
        self.io_fs().set_launch_dir(dir);
    }

    /// A file on a hard disk partition
    /// (`\Device\Harddisk0\Partition2\xboxdash.xbe`, names matched as FATX
    /// does) opened for reading; `None` when the path isn't on a partition
    /// or names no file.
    pub(crate) fn io_host_source(&self, xbox_path: &str) -> Option<Arc<dyn BlockSource>> {
        let host = self.io.fs.as_ref()?.host_file(xbox_path)?;
        match FileSource::open(&host) {
            Ok(source) => Some(Arc::new(source)),
            Err(e) => {
                log::info!(target: "fs", "open {xbox_path} ({}): {e}", host.display());
                None
            }
        }
    }

    /// The drives' own time for the titles that need it, the configuration
    /// not having chosen: the title's mode over the settings', and the drives'
    /// own when neither says.
    pub(crate) fn io_title_timing(&mut self, title_id: u32, name: &str) {
        let title = xb_fs::title_timing_mode(title_id, name);
        let chosen = self.console.device_timing;
        if title.is_some() {
            log::info!(target: "fs", "console timing: the drives' own for {title_id:08x} ({name}){}",
                if chosen.is_some() { ", over the options' disc speed" } else { "" });
        }
        self.io.title_timing = title;
        self.io.timing = DeviceTiming::for_mode(title.or(chosen).unwrap_or(TimingMode::Console));
        log::info!(target: "fs", "{}", self.io.timing.describe());
    }

    /// A disc speed chosen while the title runs
    /// ([`crate::RunControl::set_device_timing`]): used from the next
    /// request on, unless the title needs the drives' own time.
    pub(crate) fn io_take_timing(&mut self) {
        let Some(mode) = self.control.take_device_timing() else { return };
        self.console.device_timing = Some(mode);
        let timing = DeviceTiming::for_mode(self.io.title_timing.unwrap_or(mode));
        if timing != self.io.timing {
            self.io.timing = timing;
            log::info!(target: "fs", "disc speed changed: {}", timing.describe());
        }
    }

    /// The guest CPU idled for `host` with requests in flight (the
    /// scheduler's wait): for the statistics.
    pub(crate) fn io_idled(&mut self, host: Duration) {
        let ns = u64::try_from(host.as_nanos()).unwrap_or(u64::MAX);
        for drive in [DISC, HARD_DISK] {
            if self.io.busy[drive] != 0 {
                self.io.stats[drive].idle_ns += ns;
            }
        }
    }

    /// Whether a request is in flight on either drive.
    pub(crate) fn io_busy(&self) -> bool {
        self.io.busy != [0, 0]
    }

    /// The drives' part of the statistics, at DEBUG, over `s` seconds: one
    /// line per drive that did something.
    pub(crate) fn io_stats_log(&mut self, s: f64) {
        for (drive, name) in [(DISC, "disc"), (HARD_DISK, "hard disk")] {
            let st = std::mem::take(&mut self.io.stats[drive]);
            if st.queued == 0 && st.in_place == 0 {
                continue;
            }
            let ms = |ns: u64| ns as f64 / 1e6;
            let q = f64::from(st.queued.max(1));
            log::debug!(target: "kernel",
                "stats io {name} in {s:.2} s: {} queued + {} in place, {} KiB | drive time charged {:.1} ms, call to completion {:.1} ms ({:.0} us a request), host read/write {:.1} ms | completed late by {:.1} ms in all ({:.0} us a request, worst {:.2} ms) | guest CPU idle with one in flight {:.1} ms",
                st.queued, st.in_place, st.bytes >> 10, ms(st.charged_ns), ms(st.wall_ns),
                st.wall_ns as f64 / 1e3 / q, ms(st.host_ns), ms(st.late_ns),
                st.late_ns as f64 / 1e3 / q, ms(st.late_max_ns), ms(st.idle_ns));
        }
    }

    /// The guest time the earliest held request completes at, for the clock:
    /// the guest's CPU must come back to [`Kernel::io_poll`] by then.
    pub(crate) fn io_next_due(&self) -> Option<u64> {
        self.io.next_due
    }

    /// Completes the I/O the workers finished (cheap when none did): on
    /// every kernel entry and in the scheduler's loop.
    pub(crate) fn io_poll(&mut self) {
        let fresh = self.io.workers.as_ref().is_some_and(Workers::take_pending);
        if fresh || self.io_next_due().is_some_and(|due| self.now() >= due) {
            self.io_complete(fresh, false);
        }
    }

    /// A file or symbolic-link object's last handle closed.
    pub(crate) fn io_close(&mut self, object: u32) {
        if !self.io.files.contains_key(&object) {
            return;
        }
        self.io_drain_file(object); // its reads and writes in flight complete first
        let Some(open) = self.io.files.remove(&object) else { return };
        let Kind::File(file) = open.kind else { return };
        // A request still in flight after the drain keeps the file open
        // until it ends.
        let Ok(file) = Arc::try_unwrap(file) else { return };
        if !file.sync_on_close() || file.delete_pending() {
            return; // closed here; a deleted file goes with this handle
        }
        // What the title wrote is on the disk soon after it closes the file:
        // the closer thread syncs it.
        let path = file.path();
        let job = match self.io_fs().host_file(&path) {
            Some(host) => {
                drop(file);
                CloseJob::Sync(host, path)
            }
            None => CloseJob::Close(file),
        };
        self.io_workers().close(job);
    }

    /// The namespace: an empty one (no disc, no hard disk) if nothing was
    /// mounted.
    pub(crate) fn io_fs(&mut self) -> Arc<FileSystem> {
        Arc::clone(self.io.fs.get_or_insert_with(|| Arc::new(FileSystem::new())))
    }

    fn io_workers(&mut self) -> &mut Workers {
        let wake = self.wake.clone();
        self.io.workers.get_or_insert_with(|| Workers::new(wake))
    }

    /// A new file or symbolic-link object for `kind` and a handle to it, or
    /// 0 (`file_handle`).
    fn io_handle(&mut self, kind: Kind) -> u32 {
        let (ty, key) = match &kind {
            Kind::File(file) => (ObType::File, file.sort_key()),
            Kind::Link(_) => (ObType::Symlink, SortKey::None),
        };
        let obj = self.ob_create(ty, FILE_OBJECT_SIZE);
        if obj == 0 {
            return 0;
        }
        self.gm.kw16(obj, IO_TYPE_FILE);
        self.init_event(obj + FO_LOCK, 1, false);
        // signalled: the create's request completes on it, as NT's
        // (IopParseDevice makes it the create IRP's event), so a file is
        // signalled from its open until an I/O of its own starts. KOF
        // Maximum Impact Maniax closes each file waiting on it with no
        // timeout till it is, a file it read nothing from too: never
        // signalled, it hung on a black screen at boot.
        self.init_event(obj + FO_EVENT, 0, true);
        let fcb = obj + FO_FCB;
        self.gm.kw32(obj + FO_FSCONTEXT, fcb);
        match key {
            SortKey::Gdfx { start_sector } => self.gm.kw32(fcb, start_sector),
            // Host files have no clusters: the inode orders them stably.
            SortKey::Fatx { first_cluster } => self.gm.kw32(fcb + FCB_FATX_CLUSTER, first_cluster),
            SortKey::None => self.gm.kw32(fcb + FCB_FATX_FLAGS, 1),
        }
        self.io.serial += 1;
        self.io.files.insert(obj, Open { kind, serial: self.io.serial, port: None, jobs: 0 });
        let handle = self.ob_handle(obj, ty);
        if handle == 0 {
            // no handle left for it: it isn't open
            self.io.files.remove(&obj);
            self.ob_dereference(obj);
        }
        handle
    }

    /// The open file a handle names, with its object (`file_of`).
    fn file_of(&self, handle: u32) -> Option<(u32, Arc<File>)> {
        match self.ob_lookup(handle)? {
            (obj, ObType::File) => self.file_object(obj).map(|f| (obj, f)),
            _ => None,
        }
    }

    /// The open file behind a FILE_OBJECT.
    fn file_object(&self, object: u32) -> Option<Arc<File>> {
        match &self.io.files.get(&object)?.kind {
            Kind::File(file) => Some(Arc::clone(file)),
            Kind::Link(_) => None,
        }
    }

    /// Runs `f` with `name` as it stands relative to a RootDirectory
    /// (ObjectAttributes', a rename's): `\??` for XAPI's DOS paths (`D:\x`),
    /// else an open file's path. `None` when the handle isn't a file's
    /// (`resolve_name`).
    fn with_root<R>(
        &mut self,
        root: u32,
        name: &str,
        f: impl FnOnce(&FileSystem, Root<'_>, &str) -> R,
    ) -> Option<R> {
        let fs = self.io_fs();
        Some(match root {
            0 => f(&fs, Root::None, name),
            OB_DOS_DEVICES_DIRECTORY => f(&fs, Root::DosDevices, name),
            _ => {
                let (obj, ty) = self.ob_lookup(root)?;
                if !matches!(ty, ObType::File | ObType::Symlink) {
                    return None;
                }
                match &self.io.files.get(&obj)?.kind {
                    Kind::File(dir) => f(&fs, Root::File(dir), name),
                    Kind::Link(target) => f(&fs, Root::None, &format!("{target}\\{name}")),
                }
            }
        })
    }

    /// The port an I/O call's completion goes to: as NT's
    /// IopCompleteRequest, an overlapped one on a file associated with a
    /// port, with no APC routine and an ApcContext (XAPI passes the
    /// OVERLAPPED there, NULL for an event handle with its low bit set).
    fn file_port(&self, a: &Args) -> Option<(u32, u32)> {
        if a[2] != 0 || a[3] == 0 {
            return None;
        }
        let (obj, ObType::File) = self.ob_lookup(a[0])? else { return None };
        let open = self.io.files.get(&obj)?;
        match &open.kind {
            Kind::File(file) if file.is_async() => open.port,
            _ => None,
        }
    }

    /// An I/O call finished: `a` is (FileHandle, Event, ApcRoutine,
    /// ApcContext, IoStatusBlock, ...), as NtReadFile and its siblings take
    /// it. The event is set; the APC routine runs as ApcRoutine(ApcContext,
    /// IoStatusBlock, 0) when the caller next waits alertably (`complete`).
    fn io_call_complete(&mut self, a: &Args) {
        if a[1] != 0 {
            if let Some((event, _)) = self.ob_lookup(a[1]) {
                self.set_event(event);
            }
        } else if let Some((fo, ObType::File)) = self.ob_lookup(a[0]) {
            // no event of its own: the file object's, which a wait on the
            // file handle waits for
            self.set_event(fo + FO_EVENT);
        }
        if a[2] != 0
            && let Some(cur) = self.current
        {
            self.apc_queue(cur, Apc { kapc: 0, normal: a[2], ctx: a[3], a1: a[4], a2: 0 });
        }
        if a[4] != 0
            && let Some((port, key)) = self.file_port(a)
        {
            let status = self.gm.r32(a[4]).unwrap_or(0);
            let info = self.gm.r32(a[4] + 4).unwrap_or(0);
            self.port_queue(port, key, a[3], status, info);
        }
    }

    /// Ends a read or write: its IoStatusBlock, event and APC, and what it
    /// returns. An overlapped request that completes reports STATUS_PENDING;
    /// one that fails up front (end of file) returns the error and, as NT
    /// does for a failure before pending, leaves the IoStatusBlock, event
    /// and APC alone: XAPI set the OVERLAPPED to STATUS_PENDING, which
    /// GetOverlappedResult reads as success. Forza's read-ahead past the end
    /// of a file relies on that. The DVD drive pends every transfer, an
    /// overlapped one with neither event nor APC too: Otogi's stream reader
    /// counts the bytes only after ERROR_IO_PENDING, and took a disc read
    /// that succeeded at once as 0 bytes read, a dirty disc. `pend`: a
    /// success may pend (not a read through the cache). A write that
    /// bypasses the cache (no buffering) goes to the disk and pends the same
    /// way: Sniper Elite copies its sound bank to the cache drive in 128 KiB
    /// overlapped writes and moves on only after ERROR_IO_PENDING; a write
    /// that completed in the call was issued again, forever (`io_finish`).
    fn io_finish(&mut self, file: &File, a: &Args, st: u32, n: u32, pend: bool) -> KResult<u64> {
        if file.is_async() && st >= 0xC000_0000 {
            return ok(st);
        }
        io_status(self, a[4], st, n)?;
        self.io_call_complete(a);
        let device = match file.device_kind() {
            DeviceKind::Disc => !file.is_volume(),
            DeviceKind::HardDisk => hard_disk_file(file) && file.no_buffering(),
        };
        let pends = pend && file.is_async() && (a[1] != 0 || a[2] != 0 || device);
        ok(if pends { STATUS_PENDING } else { st })
    }

    /// Queues a request for its drive's worker; gives it back when it must
    /// happen in place (`disc_read_later`).
    ///
    /// An overlapped disc read runs on the worker and the call reports
    /// STATUS_PENDING at once, as the Xbox's DVD transfers do. Read in place,
    /// one from a slow host drive held the guest's CPU for 10-70 ms,
    /// interrupts included: Dead or Alive 2 Ultimate streams during its
    /// fights, and each read was a dropped frame. Overlapped reads of hard
    /// disk files go the same way (Forza streams its track from the Z: cache
    /// during a race), and long synchronous reads, only their thread
    /// waiting.
    fn io_later(
        &mut self,
        object: u32,
        file: &Arc<File>,
        a: &Args,
        rq: Request,
    ) -> Option<Request> {
        let event = if a[1] != 0 {
            match self.ob_lookup(a[1]) {
                Some((event, _)) => event,
                None => return Some(rq),
            }
        } else {
            match self.ob_lookup(a[0]) {
                Some((fo, ObType::File)) => fo + FO_EVENT,
                _ => 0,
            }
        };
        let drive = if file.device_kind() == DeviceKind::Disc { DISC } else { HARD_DISK };
        if self.io.busy[drive] >= DRIVE_JOBS {
            return Some(rq);
        }
        // The drive serves one request at a time: this one's time starts
        // when the last one's ends.
        let due = (!rq.cost.is_zero()).then(|| {
            let cost = u64::try_from(rq.cost.as_nanos()).unwrap_or(u64::MAX);
            self.now().max(self.io.drive_free[drive]).saturating_add(cost)
        });
        let seq = if rq.sync { 0 } else { self.io.async_seq + 1 };
        let job = Job {
            file: Arc::clone(file),
            object,
            serial: self.io.files.get(&object).map_or(0, |o| o.serial),
            write: rq.write,
            off: rq.off,
            n: rq.n,
            data: rq.data,
            runs: rq.runs,
            due,
            iosb: a[4],
            event,
            apc: a[2],
            apc_ctx: a[3],
            port: self.file_port(a),
            thread: self.current.map(|t| (t, self.threads[t].id)),
            sync: rq.sync,
            seq,
            drive,
            path: file.path(),
            result: Ok(0),
            cost_ns: u64::try_from(rq.cost.as_nanos()).unwrap_or(u64::MAX),
            issued: Instant::now(),
            host_ns: 0,
        };
        if let Err(job) = self.io_workers().submit(job) {
            let job = *job;
            return Some(Request {
                write: job.write,
                off: job.off,
                n: job.n,
                runs: job.runs,
                data: job.data,
                sync: job.sync,
                cost: rq.cost,
            });
        }
        if let Some(due) = due {
            self.io.drive_free[drive] = due;
        }
        if seq != 0 {
            self.io.async_seq = seq;
        }
        self.io.busy[drive] += 1;
        if let Some(open) = self.io.files.get_mut(&object) {
            open.jobs += 1;
        }
        if event != 0 {
            self.clear_event(event); // as NT does when the request starts
        }
        None
    }

    /// A request the drive finished completes: its data into the guest's
    /// buffer, its IoStatusBlock, then its event (the file object's without
    /// one of its own) and its APC, queued to the thread that issued it; a
    /// thread waiting in its call returns (`disc_read_done`).
    fn io_job_done(&mut self, job: Job) {
        self.io.busy[job.drive] = self.io.busy[job.drive].saturating_sub(1);
        let late = job.due.map_or(0, |due| self.now().saturating_sub(due));
        let st = &mut self.io.stats[job.drive];
        st.queued += 1;
        st.bytes += u64::from(job.n);
        st.charged_ns += job.cost_ns;
        st.wall_ns += u64::try_from(job.issued.elapsed().as_nanos()).unwrap_or(u64::MAX);
        st.host_ns += job.host_ns;
        st.late_ns += late;
        st.late_max_ns = st.late_max_ns.max(late);
        if let Some(open) = self.io.files.get_mut(&job.object)
            && open.serial == job.serial
        {
            open.jobs = open.jobs.saturating_sub(1);
        }
        let va = job.runs.first().map_or(0, |r| r.0);
        let (mut st, mut n) = (STATUS_SUCCESS, job.n);
        if !matches!(job.result, Ok(done) if done == job.n as usize) {
            log::info!(target: "fs", "{} {} +{:x} ({}) {} {va:08x} failed: {:?}",
                if job.write { "write" } else { "read" }, job.path, job.off, job.n,
                if job.write { "from" } else { "into" }, job.result);
            st = if job.write { NtStatus::DISK_FULL.0 } else { NtStatus::DEVICE_DATA_ERROR.0 };
            n = 0;
        } else if !job.write
            && let Err(fault) = store_runs(self, &job.runs, &job.data)
            && !self.io.gone_logged
        {
            // The title let the buffer go while the drive read into it: what
            // still has pages got its bytes, and the request completes as it
            // does when the drive's writes land in memory nobody maps.
            self.io.gone_logged = true;
            log::info!(target: "fs", "read {} +{:x} ({}) into {va:08x}: the buffer's page at {:08x} is gone",
                job.path, job.off, job.n, fault.va);
        }
        let _ = io_status(self, job.iosb, st, n);
        if job.event != 0 {
            self.set_event(job.event);
        }
        if let Some((ti, id)) = job.thread
            && self.threads[ti].id == id
        {
            if job.sync && self.threads[ti].state == ThreadState::Waiting {
                self.wake_thread(ti, st); // its NtReadFile returns now
            }
            let state = self.threads[ti].state;
            if job.apc != 0 && state != ThreadState::Free && state != ThreadState::Terminated {
                let apc = Apc { kapc: 0, normal: job.apc, ctx: job.apc_ctx, a1: job.iosb, a2: 0 };
                self.apc_queue(ti, apc);
            }
        }
        if let Some((port, key)) = job.port {
            self.port_queue(port, key, job.apc_ctx, st, n);
        }
    }

    /// The requests the drives finished, completed: their IoStatusBlocks,
    /// events, APCs, waiting threads. One with drive time left is held until
    /// the guest's clock is there (`hurry`: not waited out).
    ///
    /// Overlapped ones complete in the order they were asked for, the two
    /// drives' together: the console's DVD drive and hard disk share one IDE
    /// channel, which serves one request at a time, in turn. Here each drive
    /// has its own thread, and a request one finishes waits (held) for those
    /// asked before it on the other. Psychonauts' first boot unpacks its
    /// data to the cache drive on a worker that waits (alertably, with no
    /// timeout) for its reads' and writes' completion routines, and steps
    /// the copy at each one: it reads ahead from the disc and writes its
    /// last blocks after the last read, whose completion the console's
    /// channel delivers before theirs. With the hard disk on its own, a disc
    /// slower than the PC's writes had those writes done first; the last
    /// read's routine was then the last one, the step it woke left the last
    /// block's write for a later step, and nothing woke the worker again
    /// (`io_complete_done`).
    fn io_complete(&mut self, fresh: bool, hurry: bool) {
        if fresh && let Some(workers) = &self.io.workers {
            let mut done = workers.take_done();
            self.io.held.append(&mut done);
        }
        let now = self.now();
        let ready = |job: &Job| hurry || job.due.is_none_or(|due| now >= due);
        loop {
            let next = self.io.async_done + 1;
            let Some(i) =
                self.io.held.iter().position(|j| ready(j) && (j.seq == 0 || j.seq <= next))
            else {
                break;
            };
            let job = self.io.held.remove(i);
            self.io.async_done = self.io.async_done.max(job.seq);
            self.io_job_done(job);
        }
        let due = self.io.held.iter().filter_map(|j| j.due).filter(|due| *due > now).min();
        if due != self.io.next_due {
            self.io.next_due = due;
            self.clock_arm();
        }
    }

    /// A file's last handle closes with its reads and writes still in
    /// flight: they complete before it goes, as the console's do by then.
    /// Oddworld: Stranger's Wrath writes a 512-byte file overlapped and
    /// unbuffered, its OVERLAPPED on the stack, and closes it at once
    /// without waiting; the write's IoStatusBlock, completed later, landed
    /// where the stack then held a saved EBP and a return address. In the
    /// guest CPU's thread, which waits here: a moment, on any drive
    /// (`io_drain_file`).
    fn io_drain_file(&mut self, object: u32) {
        let jobs = |k: &Kernel| k.io.files.get(&object).map_or(0, |o| o.jobs);
        if jobs(self) == 0 {
            return;
        }
        let t0 = Instant::now();
        while jobs(self) != 0 && t0.elapsed() < Duration::from_secs(10) {
            // the guest's clock standing still, no drive time is left to wait
            let hurry = self.clock.is_still();
            self.io_complete(true, hurry);
            if jobs(self) != 0 {
                std::thread::sleep(Duration::from_micros(200));
            }
        }
        let left = jobs(self);
        if left != 0 {
            let path = self.file_object(object).map(|f| f.path()).unwrap_or_default();
            log::info!(target: "fs", "{path} closed with {left} requests still in flight after 10 s");
        }
    }

    /// NtFlushBuffersFile: the sync on the closer thread, through a
    /// descriptor of its own (the title may close the file meanwhile), in
    /// order with the closes. On the guest's CPU it took up to 211 ms
    /// (Forza's saves in the menus), nothing else running (`flush_later`).
    fn io_flush_later(&mut self, file: &File) {
        if !hard_disk_file(file) || file.is_dir() {
            return;
        }
        let path = file.path();
        match self.io_fs().host_file(&path) {
            Some(host) => self.io_workers().close(CloseJob::Sync(host, path)),
            None => {
                file.flush();
            }
        }
    }

    /// A new device object (`device_new`), or 0.
    fn device_new(&mut self, driver: u32, ext_size: u32, ty: u32) -> u32 {
        let Some(ext) = ext_size.checked_add(15).map(|n| n & !15).filter(|n| *n < 0x0100_0000)
        else {
            return 0;
        };
        let d = self.ob_create(ObType::Device, DEVOBJ_SIZE + ext);
        if d == 0 {
            return 0;
        }
        let gm = &self.gm;
        gm.kw16(d, 3); // IO_TYPE_DEVICE
        gm.kw16(d + 2, (DEVOBJ_SIZE + ext) as u16);
        gm.kw32(d + 0x08, driver);
        gm.kw32(d + 0x14, DO_DEVICE_INITIALIZING);
        gm.kw32(d + 0x18, if ext_size != 0 { d + DEVOBJ_SIZE } else { 0 });
        gm.kw8(d + 0x1C, ty as u8);
        gm.kw8(d + 0x1E, 1); // StackSize
        gm.kw32(d + 0x24, 0);
        gm.kw16(d + 0x28, 0x14); // DeviceQueueObject
        gm.kw8(d + 0x2A, 0x0C / 4);
        gm.kw32(d + 0x2C, d + 0x2C);
        gm.kw32(d + 0x30, d + 0x2C);
        gm.kw8(d + 0x34, 1); // DeviceLock: synchronization event, signaled
        gm.kw8(d + 0x36, 4);
        gm.kw32(d + 0x38, 1);
        gm.kw32(d + 0x3C, d + 0x3C);
        gm.kw32(d + 0x40, d + 0x3C);
        d
    }
}

// ------------------------------------------------------------------ open

/// What NtCreateFile and NtOpenFile pass on.
struct OpenCall {
    handle_out: u32,
    access: u32,
    attrs: u32,
    iosb: u32,
    share: u32,
    disposition: u32,
    options: u32,
}

/// Opens (and creates) a file for NtCreateFile / NtOpenFile: the
/// IoStatusBlock's information is what the open did, 5 (FILE_DOES_NOT_EXIST)
/// when it failed on a path that had a canonical form (`do_open`).
fn do_open(k: &mut Kernel, o: &OpenCall) -> KResult<u64> {
    let name = ansi(k, k.gm.r32(o.attrs + 4)?, 512)?;
    let root = k.gm.r32(o.attrs)?;
    resident(k, o.handle_out, 4, true)?;
    iosb_resident(k, o.iosb)?;
    let Some(disposition) = CreateDisposition::from_u32(o.disposition) else {
        io_status(k, o.iosb, STATUS_INVALID_PARAMETER, 0)?;
        return ok(STATUS_INVALID_PARAMETER);
    };
    let params = OpenParams::create(o.access, o.share, disposition, o.options);
    let opened = k.with_root(root, &name, |fs, root, name| fs.open(root, name, &params));
    let (st, info, handle) = match opened {
        None => {
            log::info!(target: "fs", "open {name} relative to handle {root:08x}: not a directory handle");
            (STATUS_INVALID_HANDLE, 0, 0)
        }
        Some(Err(e)) => (e.status.0, e.information, 0),
        Some(Ok((file, action))) => {
            let path = file.path();
            match k.io_handle(Kind::File(Arc::new(file))) {
                0 => {
                    log::info!(target: "fs", "open {path}: no handle left for it");
                    (STATUS_INSUFFICIENT_RESOURCES, 5, 0)
                }
                handle => (STATUS_SUCCESS, action as u32, handle),
            }
        }
    };
    io_status(k, o.iosb, st, info)?;
    if st == STATUS_SUCCESS {
        k.gm.w32(o.handle_out, handle)?;
    }
    ok(st)
}

/// NtCreateFile(FileHandle, DesiredAccess, ObjectAttributes, IoStatusBlock, AllocationSize,
///   FileAttributes, ShareAccess, CreateDisposition, CreateOptions)
fn nt_create_file(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    do_open(
        k,
        &OpenCall {
            handle_out: a[0],
            access: a[1],
            attrs: a[2],
            iosb: a[3],
            share: a[6],
            disposition: a[7],
            options: a[8],
        },
    )
}

/// NtOpenFile(FileHandle, DesiredAccess, ObjectAttributes, IoStatusBlock, ShareAccess, OpenOptions)
fn nt_open_file(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    do_open(
        k,
        &OpenCall {
            handle_out: a[0],
            access: a[1],
            attrs: a[2],
            iosb: a[3],
            share: a[4],
            disposition: CreateDisposition::Open as u32,
            options: a[5],
        },
    )
}

// ----------------------------------------------------------- read, write

/// ExReadWriteRefurbInfo(RefurbInfo, ValueLength, DoWrite): XBOX_REFURB_INFO
/// { Signature, PowerCycleCount, FirstSetTime } in the config area's sector
/// 3; read without its signature there, zeros.
fn ex_read_write_refurb_info(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    if a[1] != 16 {
        return ok(STATUS_INVALID_PARAMETER);
    }
    let fs = k.io_fs();
    if a[2] & 0xFF != 0 {
        let mut info = [0u8; 16];
        k.gm.read(a[0], &mut info)?;
        if let Some(disk) = fs.hard_disk() {
            disk.set_refurb_info(&info);
        }
    } else {
        let info = fs.hard_disk().map_or([0; 16], xb_fs::HostDisk::refurb_info);
        k.gm.write(a[0], &info)?;
    }
    ok(STATUS_SUCCESS)
}

/// IoDismountVolumeByName(DeviceName): the volumes are host folders, with
/// nothing cached to write back; the dashboard dismounts the disc and the
/// drives it formats.
fn io_dismount_volume_by_name(_k: &mut Kernel, _p: &mut dyn Platform, _a: &Args) -> KResult<u64> {
    ok(STATUS_SUCCESS)
}

/// A request's ByteOffset: `None` for a null pointer.
fn byte_offset(k: &Kernel, va: u32) -> KResult<Option<u64>> {
    if va == 0 { Ok(None) } else { k.gm.r64(va).map(Some) }
}

/// NtReadFile's work for a buffer of runs (`len` bytes in all): `a` as
/// NtReadFile takes it, its Buffer and Length aside (`read_runs`).
fn read_runs(k: &mut Kernel, a: &Args, mut runs: Vec<(u32, u32)>, len: u32) -> KResult<u64> {
    let Some((object, file)) = k.file_of(a[0]) else {
        return ok(STATUS_INVALID_HANDLE);
    };
    iosb_resident(k, a[4])?;
    let at = byte_offset(k, a[7])?;
    let va = runs.first().map_or(0, |r| r.0);
    if raw_device(&file) {
        // The raw disk's config sectors, or a cache partition read raw.
        resident_runs(k, &runs)?;
        let off = file.read_offset(at);
        let mut buf = vec![0u8; len as usize];
        if let Err(st) = file.read(off, &mut buf) {
            io_status(k, a[4], st.0, 0)?;
            return ok(st.0);
        }
        store_runs(k, &runs, &buf)?;
        file.advance(at, off + u64::from(len));
        log::trace!(target: "fs", "read {} raw +{off:x} {len}", file.path());
        return k.io_finish(&file, a, STATUS_SUCCESS, len, true);
    }
    if file.is_dir() || file.is_volume() {
        let st = NtStatus::INVALID_DEVICE_REQUEST.0;
        io_status(k, a[4], st, 0)?;
        return ok(st);
    }
    let off = file.read_offset(at);
    let size = file.size();
    let is_async = file.is_async() && !file.read_completes_in_cache(off, len);
    let (mut st, mut n) = (STATUS_SUCCESS, 0u32);
    if len == 0 {
        // Zero bytes succeed, past the end too, as the file systems check
        // the length first: Halo's map streaming queues reads of what is
        // left, 0 at the end, and waits for each one's completion.
    } else if off >= size {
        st = NtStatus::END_OF_FILE.0;
    } else {
        n = (size - off).min(u64::from(len)) as u32;
        runs_cut(&mut runs, n);
        resident_runs(k, &runs)?;
        // console timing: a synchronous read the cache doesn't hold waits for the drive
        let timing = k.io.timing;
        let kind = file.device_kind();
        let drive = timing.enabled && !is_async && can_block(k) && !file.cache_holds(off, n);
        let later = is_async || drive || (n >= SYNC_LATER_MIN && can_block(k));
        // An overlapped read always takes the drive's time, console timing
        // or not: it lands a little later than the call, as on the Xbox.
        // Total Immersion Racing polls its first video's read once right
        // after issuing it and breaks if the data is already there.
        let cost = if is_async {
            timing.cost(kind, n)
        } else if drive {
            timing.cost(kind, file.cache_fill(off, n))
        } else {
            Duration::ZERO
        };
        let rq = Request { write: false, off, n, runs, data: Vec::new(), sync: !is_async, cost };
        let rq = if later { k.io_later(object, &file, a, rq) } else { Some(rq) };
        let Some(rq) = rq else {
            file.advance(at, off + u64::from(n));
            log::trace!(target: "fs", "read {} +{off:x} {n} into {va:08x} {}{}{}", file.path(),
                if is_async { "pending" } else { "waited for" },
                if a[1] != 0 { " event" } else { "" }, if a[2] != 0 { " apc" } else { "" });
            if is_async {
                io_status(k, a[4], STATUS_PENDING, 0)?; // until it completes
            } else {
                k.block_current(); // the read's status replaces this when it completes
            }
            return ok(STATUS_PENDING);
        };
        let drive = &mut k.io.stats[if kind == DeviceKind::Disc { DISC } else { HARD_DISK }];
        drive.in_place += 1;
        drive.bytes += u64::from(n);
        let mut buf = vec![0u8; n as usize];
        match file.read(off, &mut buf) {
            Ok(done) if done == n as usize => store_runs(k, &rq.runs, &buf)?,
            failed => {
                log::info!(target: "fs", "read {} +{off:x} ({n}) into {va:08x} failed: {failed:?}", file.path());
                st = NtStatus::DEVICE_DATA_ERROR.0;
                n = 0;
            }
        }
    }
    file.advance(at, off + u64::from(n));
    log::trace!(target: "fs", "read {} +{off:x} {len} into {va:08x} -> {st:08x} n={n}{}{}", file.path(),
        if a[1] != 0 { " event" } else { "" }, if a[2] != 0 { " apc" } else { "" });
    k.io_finish(&file, a, st, n, is_async)
}

/// NtReadFile(FileHandle, Event, ApcRoutine, ApcContext, IoStatusBlock, Buffer, Length, ByteOffset)
fn nt_read_file(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    read_runs(k, a, vec![(a[5], a[6])], a[6])
}

/// A scatter/gather call's SegmentArray as runs: a page's address
/// (FILE_SEGMENT_ELEMENT, 4 bytes on the Xbox) for each page of `len`, the
/// device working on whole pages (the address's offset in its page left
/// out).
fn segment_runs(k: &Kernel, array: u32, len: u32) -> KResult<Vec<(u32, u32)>> {
    let pages = len.div_ceil(PAGE_SIZE);
    let elements = k.gm.read_vec(array, pages * 4)?;
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for (i, e) in elements.as_chunks::<4>().0.iter().enumerate() {
        let va = u32::from_le_bytes(*e) & !(PAGE_SIZE - 1);
        let n = (len - i as u32 * PAGE_SIZE).min(PAGE_SIZE);
        match runs.last_mut() {
            Some(last) if last.0.wrapping_add(last.1) == va => last.1 += n,
            _ => runs.push((va, n)),
        }
    }
    Ok(runs)
}

/// NtReadFileScatter(FileHandle, Event, ApcRoutine, ApcContext, IoStatusBlock,
/// SegmentArray, Length, ByteOffset): NtReadFile into pages. XAPI's
/// ReadFileScatter, for handles opened without the cache: Double-S.T.E.A.L
/// 2's streamer reads 24 pages at a time through it (its requests: an
/// OVERLAPPED, then 25 elements, the last NULL).
fn nt_read_file_scatter(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let runs = segment_runs(k, a[5], a[6])?;
    read_runs(k, a, runs, a[6])
}

/// NtWriteFile's work for the bytes `data` (the guest's, read already): `a`
/// as NtWriteFile takes it, its Buffer and Length aside (`va`: the Buffer,
/// for the log). `later`: it may go to the drive's worker (`write_data`).
fn write_data(k: &mut Kernel, a: &Args, data: Vec<u8>, va: u32, later: bool) -> KResult<u64> {
    let Some((object, file)) = k.file_of(a[0]) else {
        return ok(STATUS_INVALID_HANDLE);
    };
    iosb_resident(k, a[4])?;
    let at = byte_offset(k, a[7])?;
    let len = data.len() as u32;
    if file.is_volume() || raw_device(&file) {
        // The raw disk's config sectors, or a cache drive formatted raw
        // (XMountUtilityDrive): the superblock write empties the folder; the
        // head of the partition is kept. A write to the disc's device is
        // dropped.
        if raw_device(&file) {
            let off = file.write_offset(at);
            file.advance(at, off + u64::from(len));
            let _ = file.write(off, &data);
            log::trace!(target: "fs", "write {} raw +{off:x} {len}", file.path());
        }
        return k.io_finish(&file, a, STATUS_SUCCESS, len, true);
    }
    if !hard_disk_file(&file) || file.is_dir() || !file.can_write() {
        // The Xbox kernel refuses a write through a handle opened without
        // write access, and titles rely on it (Splinter Cell's profile).
        let st = file.write(0, &[]).err().unwrap_or(NtStatus::ACCESS_DENIED);
        io_status(k, a[4], st.0, 0)?;
        return ok(st.0);
    }
    let off = file.write_offset(at);
    // On the drive's worker, as a read is: the writing thread waits there (a
    // synchronous handle's) and the rest of the title runs on. In place, a
    // slow drive held the whole console: Project Gotham Racing 2's intro
    // video stood still for minutes while it copied its cars to its cache.
    // An overlapped write only when it pends on the console too: past the
    // cache (no buffering). One through the cache is done in the caller's
    // thread there and completes in the call. Pended, Hunter: The Reckoning
    // - Redeemer took its settings' write as failed; and Breakdown, which
    // copies a level's blocks to the cache drive in 16 KiB overlapped writes
    // with an event, took WriteFile's ERROR_IO_PENDING as a failed copy.
    let pends = if file.is_async() { file.no_buffering() } else { can_block(k) };
    let mut rq = Request {
        write: true,
        off,
        n: len,
        runs: vec![(va, len)],
        data,
        sync: !file.is_async(),
        cost: Duration::ZERO,
    };
    if later && len != 0 && va != 0 && pends {
        match k.io_later(object, &file, a, rq) {
            None => {
                file.advance(at, off + u64::from(len));
                log::trace!(target: "fs", "write {} +{off:x} {len} from {va:08x} {}", file.path(),
                    if file.is_async() { "pending" } else { "waited for" });
                if file.is_async() {
                    io_status(k, a[4], STATUS_PENDING, 0)?; // until it completes
                } else {
                    k.block_current(); // the write's status replaces this when it completes
                }
                return ok(STATUS_PENDING);
            }
            Some(back) => rq = back,
        }
    }
    let (st, n) = match file.write(off, &rq.data) {
        Ok(n) => (STATUS_SUCCESS, n as u32),
        Err(st) => (st.0, 0),
    };
    log::trace!(target: "fs", "write {} +{off:x} {len} from {va:08x} -> {st:08x}", file.path());
    file.advance(at, off + u64::from(n));
    k.io_finish(&file, a, st, n, file.no_buffering()) // cached: in the call
}

/// NtWriteFile(FileHandle, Event, ApcRoutine, ApcContext, IoStatusBlock, Buffer, Length, ByteOffset)
fn nt_write_file(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    if k.file_of(a[0]).is_none() {
        return ok(STATUS_INVALID_HANDLE);
    }
    let data = k.gm.read_vec(a[5], a[6])?;
    write_data(k, a, data, a[5], true)
}

/// NtWriteFileGather(FileHandle, Event, ApcRoutine, ApcContext, IoStatusBlock,
/// SegmentArray, Length, ByteOffset): NtWriteFile from pages
/// (NtReadFileScatter's other half), gathered here into one buffer. XAPI's
/// WriteFileGather, for handles opened without the cache: Operation
/// Flashpoint: Elite writes its cache files through it in 8 KiB pieces at
/// boot.
fn nt_write_file_gather(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    if k.file_of(a[0]).is_none() {
        return ok(STATUS_INVALID_HANDLE);
    }
    let first = if a[6] != 0 { k.gm.r32(a[5])? } else { 0 };
    let mut data = Vec::new();
    for (va, len) in segment_runs(k, a[5], a[6])? {
        data.extend_from_slice(&k.gm.read_vec(va, len)?);
    }
    write_data(k, a, data, first, false)
}

// ------------------------------------------------------------ information

/// NtQueryInformationFile(FileHandle, IoStatusBlock, FileInformation, Length, FileInformationClass)
fn nt_query_information_file(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let Some((_, file)) = k.file_of(a[0]) else {
        return ok(STATUS_INVALID_HANDLE);
    };
    match file.query_information(a[4], a[3] as usize) {
        Ok(bytes) => {
            k.gm.write(a[2], &bytes)?;
            io_status(k, a[1], STATUS_SUCCESS, bytes.len() as u32)?;
            ok(STATUS_SUCCESS)
        }
        Err(NtStatus::BUFFER_TOO_SMALL) => ok(NtStatus::BUFFER_TOO_SMALL.0),
        Err(st) => {
            io_status(k, a[1], st.0, 0)?;
            ok(st.0)
        }
    }
}

/// NtSetInformationFile(FileHandle, IoStatusBlock, FileInformation, Length, FileInformationClass)
fn nt_set_information_file(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let Some((object, file)) = k.file_of(a[0]) else {
        return ok(STATUS_INVALID_HANDLE);
    };
    iosb_resident(k, a[1])?;
    // The class's structure, as much of it as is read.
    let size = match a[4] {
        file_class::POSITION | file_class::END_OF_FILE | file_class::COMPLETION => 8,
        file_class::BASIC => 0x18,
        file_class::DISPOSITION => 1,
        file_class::RENAME => FileRenameInformation::SIZE as u32,
        _ => 0,
    };
    let data = k.gm.read_vec(a[2], size)?;
    let st = match a[4] {
        file_class::COMPLETION => {
            // FileCompletionInformation: Port, Key (CreateIoCompletionPort)
            let info = FileCompletionInformation::from_bytes(&data).unwrap_or_default();
            match k.ob_lookup(info.port) {
                Some((port, ObType::IoCompletion)) => {
                    if let Some(open) = k.io.files.get_mut(&object) {
                        open.port = Some((port, info.key));
                    }
                    STATUS_SUCCESS
                }
                _ => STATUS_INVALID_HANDLE,
            }
        }
        file_class::RENAME => {
            // The new name is relative to RootDirectory as an open's is:
            // XAPI's MoveFile passes "Z:\x" under \?? (Riddick's file cache
            // renames each finished X.tmp to X).
            let info = FileRenameInformation::from_bytes(&data).unwrap_or_default();
            let name = ansi(k, a[2] + 8, 1024)?;
            let renamed = k.with_root(info.root_directory, &name, |fs, root, name| {
                fs.rename(&file, root, name, info.replace_if_exists)
            });
            match renamed {
                Some(st) => st.0,
                None if hard_disk_file(&file) => NtStatus::OBJECT_PATH_NOT_FOUND.0,
                None => NtStatus::ACCESS_DENIED.0,
            }
        }
        class => {
            let st = file.set_information(class, &data).0;
            if class == file_class::DISPOSITION && data.first().is_some_and(|d| *d != 0) {
                // deletes are few, and a save that loses a file is hard to trace back
                let from = k.gm.r32(k.esp()).unwrap_or(0);
                log::info!(target: "fs", "delete {} -> {st:x} (from {from:08x} |{})", file.path(), k.frame_chain());
            }
            st
        }
    };
    io_status(k, a[1], st, 0)?;
    ok(st)
}

/// NtQueryVolumeInformationFile(FileHandle, IoStatusBlock, FsInformation, Length, FsInformationClass)
fn nt_query_volume_information_file(
    k: &mut Kernel,
    _p: &mut dyn Platform,
    a: &Args,
) -> KResult<u64> {
    let Some((_, file)) = k.file_of(a[0]) else {
        return ok(STATUS_INVALID_HANDLE);
    };
    let (st, len) = match file.query_volume_information(a[4]) {
        Ok(bytes) => {
            k.gm.write(a[2], &bytes)?;
            (STATUS_SUCCESS, bytes.len() as u32)
        }
        Err(st) => (st.0, 0),
    };
    io_status(k, a[1], st, len)?;
    ok(st)
}

/// IoQueryVolumeInformation(FileObject, FsInformationClass, Length, FsInformation, ReturnedLength):
/// the same, on a file object from ObReferenceObjectByHandle.
fn io_query_volume_information(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let Some(file) = k.file_object(a[0]) else {
        return ok(STATUS_INVALID_HANDLE);
    };
    let (st, len) = match file.query_volume_information(a[1]) {
        Ok(bytes) => {
            k.gm.write(a[3], &bytes)?;
            (STATUS_SUCCESS, bytes.len() as u32)
        }
        Err(st) => (st.0, 0),
    };
    if a[4] != 0 {
        k.gm.w32(a[4], len)?;
    }
    ok(st)
}

/// NtQueryDirectoryFile(FileHandle, Event, ApcRoutine, ApcContext, IoStatusBlock, FileInformation,
///   Length, FileInformationClass, FileMask, RestartScan): one entry per call
/// (FILE_DIRECTORY_INFORMATION with an ANSI name).
fn nt_query_directory_file(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let Some((_, file)) = k.file_of(a[0]) else {
        return ok(STATUS_INVALID_HANDLE);
    };
    if !file.is_dir() {
        return ok(NtStatus::INVALID_DEVICE_REQUEST.0);
    }
    if a[7] != file_class::DIRECTORY {
        log::info!(target: "fs", "directory information class {} not implemented", a[7]);
        return ok(NtStatus::INVALID_INFO_CLASS.0);
    }
    // FindNextFile passes no name: the enumeration goes on with its first
    // call's mask.
    let mask = if a[8] != 0 { Some(ansi(k, a[8], 128)?) } else { None };
    // The entry is taken from the enumeration before it is stored: its room
    // (the header and a listing's longest name) is there first.
    iosb_resident(k, a[4])?;
    resident(k, a[5], a[6].min(0x40 + xb_fs::LISTING_NAME_MAX as u32), true)?;
    match file.query_directory(mask.as_deref(), a[9] & 0xFF != 0, a[6] as usize) {
        Ok(entry) => {
            let bytes = entry.to_bytes();
            k.gm.write(a[5], &bytes)?;
            io_status(k, a[4], STATUS_SUCCESS, bytes.len() as u32)?;
            k.io_call_complete(a);
            ok(STATUS_SUCCESS)
        }
        Err(st @ (NtStatus::NO_SUCH_FILE | NtStatus::NO_MORE_FILES)) => {
            io_status(k, a[4], st.0, 0)?;
            k.io_call_complete(a);
            ok(st.0)
        }
        Err(st) => ok(st.0),
    }
}

// --------------------------------------------------------- path operations

/// NtQueryFullAttributesFile(ObjectAttributes, FileInformation (FILE_NETWORK_OPEN_INFORMATION))
fn nt_query_full_attributes_file(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let name = ansi(k, k.gm.r32(a[0] + 4)?, 512)?;
    let root = k.gm.r32(a[0])?;
    resident(k, a[1], xb_fs::types::FileNetworkOpenInformation::SIZE as u32, true)?;
    match k.with_root(root, &name, |fs, root, name| fs.query_full_attributes(root, name)) {
        None => {
            log::info!(target: "fs", "open {name} relative to handle {root:08x}: not a directory handle");
            ok(STATUS_INVALID_HANDLE)
        }
        Some(Err(st)) => ok(st.0),
        Some(Ok(info)) => {
            k.gm.write(a[1], &info.to_bytes())?;
            ok(STATUS_SUCCESS)
        }
    }
}

/// NtDeleteFile(ObjectAttributes)
fn nt_delete_file(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let name = ansi(k, k.gm.r32(a[0] + 4)?, 512)?;
    let root = k.gm.r32(a[0])?;
    match k.with_root(root, &name, |fs, root, name| fs.delete_file(root, name)) {
        None => ok(STATUS_INVALID_HANDLE),
        Some(st) => ok(st.0),
    }
}

/// NtFlushBuffersFile(FileHandle, IoStatusBlock)
fn nt_flush_buffers_file(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    iosb_resident(k, a[1])?;
    if let Some((_, file)) = k.file_of(a[0]) {
        k.io_flush_later(&file);
    }
    io_status(k, a[1], STATUS_SUCCESS, 0)?;
    ok(STATUS_SUCCESS)
}

/// IOCTL_SCSI_PASS_THROUGH_DIRECT to the DVD drive: a
/// SCSI_PASS_THROUGH_DIRECT (0x2C bytes: ScsiStatus +2, DataTransferLength
/// +0xC, DataBuffer +0x14, Cdb +0x1C). XAPI checks the disc with MODE
/// SENSE(10) of the Xbox security page and reboots to the dashboard unless
/// the drive reports the Xbox partition authenticated (Panzer Dragoon Orta).
/// READ DVD STRUCTURE of the security sector gets one the drive makes (xc.rs
/// `security_sector`). Other commands read zeros.
fn scsi_pass_through(k: &mut Kernel, sptd: u32, len: u32) -> KResult<()> {
    if sptd == 0 || (len as usize) < ScsiPassThroughDirect::SIZE {
        return Ok(());
    }
    let raw = k.gm.read_vec(sptd, ScsiPassThroughDirect::SIZE as u32)?;
    let Some(pass) = ScsiPassThroughDirect::from_bytes(&raw) else { return Ok(()) };
    let (n, buf) = (pass.data_transfer_length, pass.data_buffer);
    if buf != 0 {
        resident(k, buf, n, true)?;
    }
    k.gm.w8(sptd + ScsiPassThroughDirect::SCSI_STATUS_OFFSET as u32, 0)?; // SCSI status: good
    if buf == 0 || n == 0 {
        return Ok(());
    }
    let data = ioctl::scsi_command(&pass.cdb, n as usize, |d| crate::xc::security_sector(&*k, d));
    k.gm.write(buf, &data)
}

/// NtFsControlFile / NtDeviceIoControlFile(FileHandle, Event, ApcRoutine, ApcContext,
///   IoStatusBlock, ControlCode, InputBuffer, InputBufferLength, OutputBuffer, OutputBufferLength).
/// A code not known here fails as the kernel's own does (zeros and success
/// let a title carry on with them); each told once. 38 titles' first 20 s
/// sent only the ones handled.
fn nt_control_file(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let file = k.file_of(a[0]).map(|(_, f)| f);
    let path = file.as_ref().map_or_else(|| "?".to_owned(), |f| f.path());
    log::info!(target: "fs", "control {:08x} on {path} (in {}, out {})", a[5], a[7], a[9]);
    iosb_resident(k, a[4])?;
    let partition = file.as_ref().and_then(|f| f.device().partition());
    let out_len = if a[8] != 0 { a[9] as usize } else { 0 };
    let (mut status, mut info) = (STATUS_SUCCESS, 0u32);
    match ioctl::device_control(partition, a[5], out_len) {
        Ok(Control::Output(bytes)) => {
            if !bytes.is_empty() {
                k.gm.write(a[8], &bytes)?;
            }
            info = bytes.len() as u32;
        }
        Ok(Control::ScsiPassThrough) => scsi_pass_through(k, a[6], a[7])?,
        Err(st) => {
            let disk = partition.is_some() && a[5] >> 16 == 7; // FILE_DEVICE_DISK: answered or refused there
            if !disk && k.io.control_told.len() < 16 && !k.io.control_told.contains(&a[5]) {
                k.io.control_told.push(a[5]);
                log::info!(target: "fs", "control {:08x} on {path}: not known here (invalid device request)", a[5]);
            }
            status = st.0;
        }
    }
    io_status(k, a[4], status, info)?;
    k.io_call_complete(a);
    ok(status)
}

/// IoSynchronousDeviceIoControlRequest(IoControlCode, DeviceObject, InputBuffer, InputBufferLength,
///   OutputBuffer, OutputBufferLength, ReturnedOutputBufferLength, InternalDeviceIoControl):
/// no driver answers: the output reads zeros.
fn io_synchronous_device_io_control_request(
    k: &mut Kernel,
    _p: &mut dyn Platform,
    a: &Args,
) -> KResult<u64> {
    log::info!(target: "fs", "device control {:08x} on device {:08x} (out {})", a[0], a[1], a[5]);
    if a[4] != 0 && a[5] != 0 {
        k.gm.fill(a[4], 0, a[5])?;
    }
    if a[6] != 0 {
        k.gm.w32(a[6], 0)?;
    }
    ok(STATUS_SUCCESS)
}

// ---------------------------------------------------------- symbolic links

/// IoCreateSymbolicLink(SymbolicLinkName, DeviceName)
fn io_create_symbolic_link(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let name = ansi(k, a[0], 64)?;
    let target = ansi(k, a[1], 256)?;
    ok(k.io_fs().create_symbolic_link(&name, &target).0)
}

/// IoDeleteSymbolicLink(SymbolicLinkName)
fn io_delete_symbolic_link(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let name = ansi(k, a[0], 64)?;
    ok(k.io_fs().delete_symbolic_link(&name).0)
}

/// NtOpenSymbolicLinkObject(LinkHandle, ObjectAttributes)
fn nt_open_symbolic_link_object(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let name = ansi(k, k.gm.r32(a[1] + 4)?, 64)?;
    resident(k, a[0], 4, true)?;
    let Some(target) = k.io_fs().symbolic_link_target(&name) else {
        log::info!(target: "fs", "symbolic link {name}: none");
        return ok(NtStatus::OBJECT_NAME_NOT_FOUND.0);
    };
    let handle = k.io_handle(Kind::Link(target));
    if handle == 0 {
        return ok(STATUS_INSUFFICIENT_RESOURCES);
    }
    k.gm.w32(a[0], handle)?;
    ok(STATUS_SUCCESS)
}

/// NtQuerySymbolicLinkObject(LinkHandle, LinkTarget (ANSI_STRING), ReturnedLength)
fn nt_query_symbolic_link_object(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let target = match k.ob_lookup(a[0]).and_then(|(obj, _)| k.io.files.get(&obj)) {
        Some(Open { kind: Kind::Link(target), .. }) => string_to_ansi(target),
        _ => return ok(STATUS_INVALID_HANDLE),
    };
    let len = target.len().min(usize::from(u16::MAX)) as u16;
    let max = k.gm.r16(a[1] + 2)?;
    if a[2] != 0 {
        k.gm.w32(a[2], u32::from(len))?;
    }
    if max < len {
        return ok(NtStatus::BUFFER_TOO_SMALL.0);
    }
    k.gm.write(k.gm.r32(a[1] + 4)?, &target[..usize::from(len)])?;
    k.gm.w16(a[1], len)?;
    ok(STATUS_SUCCESS)
}

// ---------------------------------------------------------- device objects

/// IoCreateDevice(DriverObject, DeviceExtensionSize, DeviceName, DeviceType, Exclusive, DeviceObject)
fn io_create_device(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let name = ansi(k, a[2], 128)?;
    resident(k, a[5], 4, true)?;
    let fs = k.io_fs();
    // The name first: a name taken makes no object.
    let mut path = String::new();
    if a[2] != 0 {
        match fs.register_device(&name, DEVICE_PLACEHOLDER) {
            Ok(canonical) => path = canonical,
            Err(st) => return ok(st.0),
        }
        fs.unregister_device(DEVICE_PLACEHOLDER);
    }
    let d = k.device_new(a[0], a[1], a[3]);
    if d == 0 {
        return ok(STATUS_INSUFFICIENT_RESOURCES);
    }
    if !path.is_empty() {
        let _ = fs.register_device(&name, d);
    }
    k.gm.w32(a[5], d)?;
    log::info!(target: "fs", "device {} created: type {:02x}, driver {:08x}, extension {}",
        if path.is_empty() { "(unnamed)" } else { &path }, a[3], a[0], a[1]);
    ok(STATUS_SUCCESS)
}

/// IoDeleteDevice(DeviceObject)
fn io_delete_device(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    k.gm.w8(a[0] + 0x1F, 1)?; // DeletePending
    k.io_fs().unregister_device(a[0]);
    Ok(0)
}

/// ObReferenceObjectByName(ObjectName, Attributes, ObjectType, ParseContext, Object): devices only.
/// Created devices by name; the built-in disc and partitions on first reference.
fn ob_reference_object_by_name(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let name = ansi(k, a[0], 512)?;
    resident(k, a[4], 4, true)?;
    let fs = k.io_fs();
    let d = match fs.reference_device(&name) {
        Ok(DeviceRef::Registered(d)) => d,
        Ok(DeviceRef::Builtin(device)) => {
            // FILE_DEVICE_CD_ROM / FILE_DEVICE_DISK
            let d = k.device_new(0, 0, if device == Device::CdRom0 { 0x02 } else { 0x07 });
            if d == 0 {
                return ok(STATUS_INSUFFICIENT_RESOURCES);
            }
            let _ = fs.register_device(&name, d);
            d
        }
        Err(st) => return ok(st.0),
    };
    k.gm.w32(a[4], d)?;
    ok(STATUS_SUCCESS)
}

/// IoCreateFile(FileHandle, DesiredAccess, ObjectAttributes, IoStatusBlock, AllocationSize,
///   FileAttributes, ShareAccess, Disposition, CreateOptions, Options): NtCreateFile is this
/// with no Options on the console; the Options (IO_NO_PARAMETER_CHECKING and the like) change
/// nothing here. Not in the reference, which leaves the export out.
fn io_create_file(k: &mut Kernel, p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    nt_create_file(k, p, a)
}

pub(crate) fn register(k: &mut Kernel) {
    k.imp("ExReadWriteRefurbInfo", ex_read_write_refurb_info);
    k.imp("IoDismountVolumeByName", io_dismount_volume_by_name);

    k.imp("NtCreateFile", nt_create_file);
    k.imp("IoCreateFile", io_create_file);
    k.imp("NtOpenFile", nt_open_file);
    k.imp("NtReadFile", nt_read_file);
    k.imp("NtReadFileScatter", nt_read_file_scatter);
    k.imp("NtWriteFileGather", nt_write_file_gather);
    k.imp("NtWriteFile", nt_write_file);
    k.imp("NtQueryInformationFile", nt_query_information_file);
    k.imp("NtSetInformationFile", nt_set_information_file);
    k.imp("NtQueryVolumeInformationFile", nt_query_volume_information_file);
    k.imp("NtQueryDirectoryFile", nt_query_directory_file);
    k.imp("IoQueryVolumeInformation", io_query_volume_information);
    k.imp("NtQueryFullAttributesFile", nt_query_full_attributes_file);
    k.imp("NtDeleteFile", nt_delete_file);
    k.imp("NtFlushBuffersFile", nt_flush_buffers_file);
    k.imp("NtFsControlFile", nt_control_file);
    k.imp("NtDeviceIoControlFile", nt_control_file);
    k.imp("IoSynchronousDeviceIoControlRequest", io_synchronous_device_io_control_request);
    k.imp("IoCreateSymbolicLink", io_create_symbolic_link);
    k.imp("IoDeleteSymbolicLink", io_delete_symbolic_link);
    k.imp("NtOpenSymbolicLinkObject", nt_open_symbolic_link_object);
    k.imp("NtQuerySymbolicLinkObject", nt_query_symbolic_link_object);
    k.imp("ObReferenceObjectByName", ob_reference_object_by_name);
    k.imp("IoCreateDevice", io_create_device);
    k.imp("IoDeleteDevice", io_delete_device);
}
