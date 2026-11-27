//! Tests of the I/O exports, through the dispatcher, with the structures
//! the title passes in guest memory. The hard disk is `xb-fs`'s in-memory
//! volume and the disc its in-memory tree, except where the mount itself is
//! tested.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use xb_cpu::reg;
use xb_formats::block::MemSource;
use xb_formats::xdvdfs::Xdvdfs;
use xb_fs::types::{
    DISC_TIME, FileBasicInformation, FileDirectoryInformation, FileFsAttributeInformation,
    FileFsDeviceInformation, FileNetworkOpenInformation, FileStandardInformation, access,
    attributes, fs_class, options, share,
};
use xb_fs::{MemDisc, MemVolume};

use super::*;
use crate::testing::{TestKernel, TestPlatform, kernel};

const E: &str = r"\Device\Harddisk0\Partition1";
const CD: &str = r"\Device\CdRom0";
const SYNC: u32 = options::FILE_SYNCHRONOUS_IO_NONALERT;
const RW: u32 = access::GENERIC_READ | access::GENERIC_WRITE;
const READ: u32 = access::GENERIC_READ;
const FILE_SUPERSEDE: u32 = 0;
const FILE_OPEN: u32 = 1;
const FILE_CREATE: u32 = 2;
const FILE_OPEN_IF: u32 = 3;
const FILE_OVERWRITE_IF: u32 = 5;
/// What a test leaves in an IoStatusBlock to see whether a call wrote it.
const UNTOUCHED: u32 = 0xEEEE_EEEE;
/// Guest memory a test has for its structures and buffers.
const SCRATCH: u32 = 0x10_0000;
/// The disc's big file: 0x90000 bytes.
const MAP_LEN: u32 = 0x9_0000;

/// Bytes that differ from place to place.
fn pattern(len: usize, seed: u32) -> Vec<u8> {
    (0..len as u32).map(|i| (i.wrapping_mul(31).wrapping_add(seed) ^ (i >> 8)) as u8).collect()
}

/// A kernel with a namespace in memory: E: with a few files, Z:, a disc.
struct Fx {
    tk: TestKernel,
    e: Arc<MemVolume>,
    disc: Arc<MemDisc>,
    next: u32,
    end: u32,
}

fn fixture() -> Fx {
    let mut tk = kernel();
    let fs = FileSystem::new();
    let e = Arc::new(MemVolume::new());
    e.insert_file(r"info.bin", &pattern(5000, 1));
    e.insert_file(r"list\a.map", &pattern(10, 2));
    e.insert_file(r"list\b.map", &pattern(20, 3));
    e.insert_file(r"list\c.txt", &pattern(30, 4));
    e.insert_file(r"list\sub\x", b"x");
    e.insert_file(r"TDATA\4d530004\keep", b"k");
    fs.mount(Device::Partition(1), Arc::clone(&e) as Arc<dyn xb_fs::Volume>);
    fs.mount(Device::Partition(5), Arc::new(MemVolume::new()));
    let mut disc = MemDisc::new();
    disc.add_file("default.xbe", b"XBEH and the rest")
        .add_file(r"maps\a10.map", &pattern(MAP_LEN as usize, 5));
    let disc = Arc::new(disc);
    fs.insert_disc(Arc::clone(&disc) as Arc<dyn DiscTree>);
    tk.k.io.fs = Some(Arc::new(fs));
    tk.k.clock.pause_end(); // the guest's time runs, as inside Kernel::run
    let next = tk.alloc(SCRATCH);
    Fx { tk, e, disc, next, end: next + SCRATCH }
}

impl Fx {
    /// Zeroed guest memory, 8-byte aligned.
    fn mem(&mut self, len: u32) -> u32 {
        let va = self.next;
        self.next += (len + 7) & !7;
        assert!(self.next <= self.end, "the test's scratch memory is used up");
        va
    }

    /// Whole pages of guest memory.
    fn pages(&mut self, n: u32) -> u32 {
        self.next = (self.next + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        self.mem(n * PAGE_SIZE)
    }

    fn r32(&self, va: u32) -> u32 {
        self.tk.k.gm.r32(va).unwrap()
    }

    fn bytes(&self, va: u32, len: u32) -> Vec<u8> {
        self.tk.k.gm.read_vec(va, len).unwrap()
    }

    fn put(&mut self, data: &[u8]) -> u32 {
        let va = self.mem(data.len() as u32);
        self.tk.k.gm.write(va, data).unwrap();
        va
    }

    /// An ANSI_STRING naming `s`.
    fn ansi(&mut self, s: &str) -> u32 {
        let buf = self.put(s.as_bytes());
        let va = self.mem(8);
        let gm = &self.tk.k.gm;
        gm.w16(va, s.len() as u16).unwrap();
        gm.w16(va + 2, s.len() as u16).unwrap();
        gm.w32(va + 4, buf).unwrap();
        va
    }

    /// OBJECT_ATTRIBUTES { RootDirectory, ObjectName, Attributes }.
    fn attrs(&mut self, root: u32, name: &str) -> u32 {
        let s = self.ansi(name);
        let va = self.mem(12);
        self.tk.k.gm.w32(va, root).unwrap();
        self.tk.k.gm.w32(va + 4, s).unwrap();
        va
    }

    /// An IO_STATUS_BLOCK no call has written.
    fn iosb(&mut self) -> u32 {
        let va = self.mem(8);
        self.tk.k.gm.w32(va, UNTOUCHED).unwrap();
        self.tk.k.gm.w32(va + 4, UNTOUCHED).unwrap();
        va
    }

    fn status(&self, iosb: u32) -> (u32, u32) {
        (self.r32(iosb), self.r32(iosb + 4))
    }

    /// NtCreateFile: (status, IoStatusBlock.Information, handle).
    fn create(
        &mut self,
        root: u32,
        name: &str,
        desired: u32,
        sharing: u32,
        disposition: u32,
        opts: u32,
    ) -> (u32, u32, u32) {
        let (oa, h, iosb) = (self.attrs(root, name), self.mem(4), self.iosb());
        self.tk.k.gm.w32(h, UNTOUCHED).unwrap();
        let st = self
            .tk
            .call32("NtCreateFile", &[h, desired, oa, iosb, 0, 0, sharing, disposition, opts]);
        (st, self.r32(iosb + 4), self.r32(h))
    }

    /// Opens what must open; the handle.
    fn open(&mut self, name: &str, desired: u32, disposition: u32, opts: u32) -> u32 {
        let (st, _, h) = self.create(0, name, desired, share::ALL, disposition, opts);
        assert_eq!(st, STATUS_SUCCESS, "open {name}");
        h
    }

    fn thread(&self) -> usize {
        self.tk.k.current.unwrap()
    }

    /// A call that may park the thread for a drive's worker: the scheduler
    /// then idles until the request is done, and the call's status is the
    /// thread's EAX.
    fn call_sync(&mut self, name: &str, args: &[u32]) -> u32 {
        let st = self.tk.call32(name, args);
        if self.tk.k.threads[self.thread()].state == ThreadState::Waiting {
            self.tk.tail();
            return self.tk.k.reg(reg::EAX);
        }
        st
    }

    /// Polls for completed I/O until `done`.
    fn pump(&mut self, what: &str, done: impl Fn(&Fx) -> bool) {
        let t0 = Instant::now();
        while !done(self) {
            assert!(t0.elapsed() < Duration::from_secs(10), "{what} did not complete");
            std::thread::sleep(Duration::from_millis(1));
            self.tk.k.io_poll();
        }
    }

    /// NtQueryInformationFile: the class's bytes.
    fn query(&mut self, h: u32, class: u32, len: u32) -> Result<Vec<u8>, u32> {
        let (iosb, out) = (self.iosb(), self.mem(0x100));
        match self.tk.call32("NtQueryInformationFile", &[h, iosb, out, len, class]) {
            STATUS_SUCCESS => Ok(self.bytes(out, self.r32(iosb + 4))),
            st => Err(st),
        }
    }

    fn position(&mut self, h: u32) -> u64 {
        u64::from_le_bytes(self.query(h, file_class::POSITION, 8).unwrap().try_into().unwrap())
    }

    /// The names an enumeration returns from here on, and the status that
    /// ended it.
    fn list(&mut self, h: u32, mask: Option<&str>, restart: bool) -> (Vec<String>, u32) {
        let (iosb, out) = (self.iosb(), self.mem(0x200));
        let mut mask = mask.map_or(0, |m| self.ansi(m));
        let mut restart = u32::from(restart);
        let mut names = Vec::new();
        loop {
            let st = self
                .tk
                .call32("NtQueryDirectoryFile", &[h, 0, 0, 0, iosb, out, 0x200, 1, mask, restart]);
            if st != STATUS_SUCCESS {
                return (names, st);
            }
            let len = self.r32(iosb + 4);
            let entry = FileDirectoryInformation::from_bytes(&self.bytes(out, len)).unwrap();
            names.push(entry.file_name);
            (mask, restart) = (0, 0);
        }
    }
}

#[test]
fn a_file_is_created_written_read_back_and_closed_through_the_exports() {
    let mut fx = fixture();
    let pool_free = fx.tk.k.pool.free_bytes();
    let name = format!(r"{E}\save.bin");
    let (st, info, h) = fx.create(0, &name, RW, 0, FILE_CREATE, SYNC);
    assert_eq!((st, info), (STATUS_SUCCESS, 2)); // FILE_CREATED

    let data = pattern(5000, 9);
    let (src, iosb) = (fx.put(&data), fx.iosb());
    let st = fx.call_sync("NtWriteFile", &[h, 0, 0, 0, iosb, src, 5000, 0]);
    assert_eq!(st, STATUS_SUCCESS);
    assert_eq!(fx.status(iosb), (STATUS_SUCCESS, 5000));
    assert_eq!(fx.e.file_bytes("save.bin").unwrap(), data);
    assert_eq!(fx.position(h), 5000);

    // At the position, the end; at an offset of its own, the bytes.
    let (dst, iosb) = (fx.mem(5000), fx.iosb());
    let st = fx.call_sync("NtReadFile", &[h, 0, 0, 0, iosb, dst, 5000, 0]);
    assert_eq!(st, NtStatus::END_OF_FILE.0);
    let off = fx.put(&100u64.to_le_bytes());
    let st = fx.call_sync("NtReadFile", &[h, 0, 0, 0, iosb, dst, 5000, off]);
    assert_eq!(st, STATUS_SUCCESS);
    assert_eq!(fx.status(iosb), (STATUS_SUCCESS, 4900));
    assert_eq!(fx.bytes(dst, 4900), data[100..]);
    assert_eq!(fx.position(h), 5000);

    // The handle closed, the FILE_OBJECT and its host state are gone.
    assert_eq!(fx.tk.k.io.files.len(), 1);
    assert_eq!(fx.tk.call32("NtClose", &[h]), STATUS_SUCCESS);
    assert!(fx.tk.k.io.files.is_empty());
    assert!(fx.tk.k.ob_lookup(h).is_none());
    assert_eq!(fx.tk.k.pool.free_bytes(), pool_free);
    assert_eq!(fx.tk.call32("NtReadFile", &[h, 0, 0, 0, iosb, dst, 1, 0]), STATUS_INVALID_HANDLE);
}

#[test]
fn an_open_reports_what_it_did_or_why_it_could_not() {
    let mut fx = fixture();
    let missing = format!(r"{E}\missing.bin");
    let (st, info, h) = fx.create(0, &missing, READ, share::ALL, FILE_OPEN, SYNC);
    assert_eq!((st, info, h), (NtStatus::OBJECT_NAME_NOT_FOUND.0, 5, UNTOUCHED));
    let (st, ..) = fx.create(0, &format!(r"{E}\nodir\x"), RW, share::ALL, FILE_CREATE, SYNC);
    assert_eq!(st, NtStatus::OBJECT_PATH_NOT_FOUND.0);
    let info_bin = format!(r"{E}\info.bin");
    let (st, ..) = fx.create(0, &info_bin, RW, share::ALL, FILE_CREATE, SYNC);
    assert_eq!(st, NtStatus::OBJECT_NAME_COLLISION.0);
    let (st, ..) = fx.create(0, &info_bin, READ, share::ALL, 9, SYNC);
    assert_eq!(st, STATUS_INVALID_PARAMETER);
    let (st, info, _) = fx.create(0, r"\Device\Nothing\x", READ, share::ALL, FILE_OPEN, SYNC);
    assert_eq!((st, info), (NtStatus::OBJECT_PATH_NOT_FOUND.0, 5));
    let (st, info, _) = fx.create(0, r"\??\Q:\x", READ, share::ALL, FILE_OPEN, SYNC);
    assert_eq!((st, info), (NtStatus::OBJECT_PATH_NOT_FOUND.0, 0));

    // Each disposition's information.
    let new = format!(r"{E}\new.bin");
    for (disposition, info) in
        [(FILE_OPEN_IF, 2), (FILE_OPEN_IF, 1), (FILE_OVERWRITE_IF, 3), (FILE_SUPERSEDE, 0)]
    {
        let (st, got, h) = fx.create(0, &new, RW, share::ALL, disposition, SYNC);
        assert_eq!((st, got), (STATUS_SUCCESS, info), "disposition {disposition}");
        fx.tk.call32("NtClose", &[h]);
    }

    // A handle that shares nothing keeps other opens out while it is open.
    let (st, _, alone) = fx.create(0, &info_bin, READ, 0, FILE_OPEN, SYNC);
    assert_eq!(st, STATUS_SUCCESS);
    let (st, ..) = fx.create(0, &info_bin, READ, share::ALL, FILE_OPEN, SYNC);
    assert_eq!(st, NtStatus::SHARING_VIOLATION.0);
    fx.tk.call32("NtClose", &[alone]);
    let (st, info, _) = fx.create(0, &info_bin, READ, share::ALL, FILE_OPEN, SYNC);
    assert_eq!((st, info), (STATUS_SUCCESS, 1));

    // The disc is read-only, and a file is not a directory.
    let xbe = format!(r"{CD}\default.xbe");
    let (st, ..) = fx.create(0, &xbe, READ, share::ALL, FILE_CREATE, SYNC);
    assert_eq!(st, NtStatus::MEDIA_WRITE_PROTECTED.0);
    let (st, ..) =
        fx.create(0, &xbe, READ, share::ALL, FILE_OPEN, SYNC | options::FILE_DIRECTORY_FILE);
    assert_eq!(st, NtStatus::NOT_A_DIRECTORY.0);
    let (st, ..) = fx.create(
        0,
        &format!(r"{CD}\maps"),
        READ,
        share::ALL,
        FILE_OPEN,
        SYNC | options::FILE_NON_DIRECTORY_FILE,
    );
    assert_eq!(st, NtStatus::FILE_IS_A_DIRECTORY.0);
}

#[test]
fn a_name_is_relative_to_its_root_directory() {
    let mut fx = fixture();
    // XAPI's DOS paths, under \??.
    let (st, info, _) =
        fx.create(OB_DOS_DEVICES_DIRECTORY, r"D:\default.xbe", READ, share::ALL, FILE_OPEN, SYNC);
    assert_eq!((st, info), (STATUS_SUCCESS, 1));
    // An open directory.
    let dir = fx.open(&format!(r"{E}\list"), READ, FILE_OPEN, SYNC | options::FILE_DIRECTORY_FILE);
    let (st, info, _) = fx.create(dir, "inner.bin", RW, share::ALL, FILE_CREATE, SYNC);
    assert_eq!((st, info), (STATUS_SUCCESS, 2));
    assert!(fx.e.file_bytes(r"list\inner.bin").is_some());
    // A handle that is no file's.
    let ev = fx.mem(4);
    assert_eq!(fx.tk.call32("NtCreateEvent", &[ev, 0, 0, 0]), STATUS_SUCCESS);
    let ev = fx.r32(ev);
    let (st, info, _) = fx.create(ev, "x", READ, share::ALL, FILE_OPEN, SYNC);
    assert_eq!((st, info), (STATUS_INVALID_HANDLE, 0));
    // NtOpenFile opens what exists.
    let (oa, h, iosb) = (fx.attrs(dir, "a.map"), fx.mem(4), fx.iosb());
    let st = fx.tk.call32("NtOpenFile", &[h, READ, oa, iosb, share::ALL, SYNC]);
    assert_eq!((st, fx.status(iosb)), (STATUS_SUCCESS, (STATUS_SUCCESS, 1)));
    let oa = fx.attrs(dir, "none.map");
    let st = fx.tk.call32("NtOpenFile", &[h, READ, oa, iosb, share::ALL, SYNC]);
    assert_eq!(st, NtStatus::OBJECT_NAME_NOT_FOUND.0);
}

#[test]
fn the_file_object_is_laid_out_as_xapi_reads_it() {
    let mut fx = fixture();
    let h = fx.open(&format!(r"{CD}\maps\a10.map"), READ, FILE_OPEN, SYNC);
    let (obj, ty) = fx.tk.k.ob_lookup(h).unwrap();
    assert_eq!(ty, ObType::File);
    let sector = fx.disc.lookup(r"maps\a10.map").unwrap().sector;
    assert_eq!(fx.tk.k.gm.r16(obj).unwrap(), IO_TYPE_FILE);
    // FsContext: the GDFX control block starts with the start sector.
    let fcb = fx.r32(obj + FO_FSCONTEXT);
    assert_eq!((fcb, fx.r32(fcb)), (obj + FO_FCB, sector));
    // Signalled from its open on (KOF Maximum Impact Maniax waits for it).
    assert_eq!(fx.r32(obj + FO_EVENT + 4), 1);
    // FileInternalInformation is the same number.
    let index = fx.query(h, file_class::INTERNAL, 8).unwrap();
    assert_eq!(index, u64::from(sector).to_le_bytes());

    // FATX: the first cluster at +0x1C, flags clear; a directory has no key.
    let h = fx.open(&format!(r"{E}\info.bin"), READ, FILE_OPEN, SYNC);
    let (obj, _) = fx.tk.k.ob_lookup(h).unwrap();
    let index = fx.query(h, file_class::INTERNAL, 8).unwrap();
    assert_eq!(fx.bytes(obj + FO_FCB + FCB_FATX_CLUSTER, 4), index[..4]);
    assert_eq!(fx.r32(obj + FO_FCB) & 1, 0);
    let h = fx.open(&format!(r"{E}\list"), READ, FILE_OPEN, SYNC);
    let (obj, _) = fx.tk.k.ob_lookup(h).unwrap();
    assert_eq!(fx.r32(obj + FO_FCB) & 1, 1);
}

#[test]
fn the_information_classes_answer_in_guest_bytes() {
    let mut fx = fixture();
    let info_bin = format!(r"{E}\info.bin");
    let h = fx.open(&info_bin, RW, FILE_OPEN, SYNC);
    let std = fx.query(h, file_class::STANDARD, 0x18).unwrap();
    let std = FileStandardInformation::from_bytes(&std).unwrap();
    assert_eq!((std.end_of_file, std.allocation_size, std.number_of_links), (5000, 8192, 1));
    assert!(!std.directory);
    assert_eq!(fx.query(h, file_class::STANDARD, 0x17), Err(NtStatus::BUFFER_TOO_SMALL.0));
    assert_eq!(fx.query(h, 99, 0x100), Err(NtStatus::INVALID_INFO_CLASS.0));

    // Position, then the end of the file.
    let (iosb, v) = (fx.iosb(), fx.put(&100u64.to_le_bytes()));
    let set = |fx: &mut Fx, h: u32, class: u32| {
        fx.tk.call32("NtSetInformationFile", &[h, iosb, v, 8, class])
    };
    assert_eq!(set(&mut fx, h, file_class::POSITION), STATUS_SUCCESS);
    assert_eq!((fx.position(h), fx.status(iosb)), (100, (STATUS_SUCCESS, 0)));
    assert_eq!(set(&mut fx, h, file_class::END_OF_FILE), STATUS_SUCCESS);
    assert_eq!(fx.e.file_bytes("info.bin").unwrap().len(), 100);
    // Splinter Cell: SetEndOfFile through a handle opened to read fails.
    let reader = fx.open(&info_bin, READ, FILE_OPEN, SYNC);
    assert_eq!(set(&mut fx, reader, file_class::END_OF_FILE), NtStatus::ACCESS_DENIED.0);
    assert_eq!(fx.e.file_bytes("info.bin").unwrap().len(), 100);

    // NtQueryFullAttributesFile agrees with the handle's network open information.
    let by_handle = fx.query(h, file_class::NETWORK_OPEN, 0x38).unwrap();
    let (oa, out) = (fx.attrs(0, &info_bin), fx.mem(0x38));
    assert_eq!(fx.tk.call32("NtQueryFullAttributesFile", &[oa, out]), STATUS_SUCCESS);
    assert_eq!(fx.bytes(out, 0x38), by_handle);
    assert_eq!(FileNetworkOpenInformation::from_bytes(&by_handle).unwrap().end_of_file, 100);
    let oa = fx.attrs(0, &format!(r"{E}\none"));
    let st = fx.tk.call32("NtQueryFullAttributesFile", &[oa, out]);
    assert_eq!(st, NtStatus::OBJECT_NAME_NOT_FOUND.0);

    // A disc file: read-only, the disc's one time.
    let xbe = fx.open(&format!(r"{CD}\default.xbe"), READ, FILE_OPEN, SYNC);
    let basic = fx.query(xbe, file_class::BASIC, 0x28).unwrap();
    let basic = FileBasicInformation::from_bytes(&basic).unwrap();
    assert_eq!(
        (basic.creation_time, basic.file_attributes),
        (DISC_TIME, attributes::FILE_ATTRIBUTE_READONLY)
    );

    // The volumes, by handle and by file object.
    let (iosb, out, ret) = (fx.iosb(), fx.mem(0x40), fx.mem(4));
    for (handle, name, device) in [(h, "FATX", (7, 0)), (xbe, "GDFX", (2, 1))] {
        let st = fx.tk.call32(
            "NtQueryVolumeInformationFile",
            &[handle, iosb, out, 0x40, fs_class::ATTRIBUTE],
        );
        assert_eq!((st, fx.status(iosb)), (STATUS_SUCCESS, (STATUS_SUCCESS, 16)));
        let by_handle = fx.bytes(out, 16);
        assert_eq!(
            FileFsAttributeInformation::from_bytes(&by_handle).unwrap().file_system_name,
            name
        );
        let (obj, _) = fx.tk.k.ob_lookup(handle).unwrap();
        fx.tk.k.gm.fill(out, 0, 0x40).unwrap();
        let st =
            fx.tk.call32("IoQueryVolumeInformation", &[obj, fs_class::ATTRIBUTE, 0x40, out, ret]);
        assert_eq!((st, fx.r32(ret)), (STATUS_SUCCESS, 16));
        assert_eq!(fx.bytes(out, 16), by_handle);
        let st = fx
            .tk
            .call32("NtQueryVolumeInformationFile", &[handle, iosb, out, 0x40, fs_class::DEVICE]);
        assert_eq!(st, STATUS_SUCCESS);
        let d = FileFsDeviceInformation::from_bytes(&fx.bytes(out, 8)).unwrap();
        assert_eq!((d.device_type, d.characteristics), device);
    }
    let st = fx.tk.call32("IoQueryVolumeInformation", &[out, fs_class::ATTRIBUTE, 0x40, out, ret]);
    assert_eq!(st, STATUS_INVALID_HANDLE);
    let st = fx.tk.call32("NtQueryVolumeInformationFile", &[h, iosb, out, 0x40, 99]);
    assert_eq!(st, NtStatus::INVALID_INFO_CLASS.0);
}

#[test]
fn a_file_is_renamed_and_deleted_through_its_handle_or_by_name() {
    let mut fx = fixture();
    let h = fx.open(&format!(r"{E}\info.bin"), RW | access::DELETE, FILE_OPEN, SYNC);
    // FILE_RENAME_INFORMATION { ReplaceIfExists, RootDirectory, FileName }.
    let new = format!(r"{E}\list\moved.bin");
    let name = fx.put(new.as_bytes());
    let info = FileRenameInformation {
        replace_if_exists: false,
        root_directory: 0,
        file_name_length: new.len() as u16,
        file_name_maximum_length: new.len() as u16,
        file_name_buffer: name,
    };
    let (iosb, info) = (fx.iosb(), fx.put(&info.to_bytes()));
    let rename = |fx: &mut Fx| {
        fx.tk.call32("NtSetInformationFile", &[h, iosb, info, 0x10, file_class::RENAME])
    };
    assert_eq!(rename(&mut fx), STATUS_SUCCESS);
    assert!(fx.e.file_bytes("info.bin").is_none());
    assert_eq!(fx.e.file_bytes(r"list\moved.bin").unwrap().len(), 5000);
    // A name under a root that is no file's.
    fx.tk.k.gm.w32(info + 4, 0x7770).unwrap();
    assert_eq!(rename(&mut fx), NtStatus::OBJECT_PATH_NOT_FOUND.0);

    // Deleted through the handle, the file goes when the handle closes.
    let one = fx.put(&[1]);
    let st = fx.tk.call32("NtSetInformationFile", &[h, iosb, one, 1, file_class::DISPOSITION]);
    assert_eq!(st, STATUS_SUCCESS);
    assert!(fx.e.file_bytes(r"list\moved.bin").is_some());
    let (st, ..) = fx.create(0, &new, READ, share::ALL, FILE_OPEN, SYNC);
    assert_eq!(st, NtStatus::DELETE_PENDING.0);
    fx.tk.call32("NtClose", &[h]);
    assert!(fx.e.file_bytes(r"list\moved.bin").is_none());

    // By name: at once.
    let oa = fx.attrs(0, &format!(r"{E}\list\c.txt"));
    assert_eq!(fx.tk.call32("NtDeleteFile", &[oa]), STATUS_SUCCESS);
    assert!(fx.e.file_bytes(r"list\c.txt").is_none());
    let oa = fx.attrs(0, &format!(r"{CD}\default.xbe"));
    assert_eq!(fx.tk.call32("NtDeleteFile", &[oa]), NtStatus::ACCESS_DENIED.0);
}

#[test]
fn a_directory_lists_what_matches_its_mask_one_entry_a_call() {
    let mut fx = fixture();
    let dir = fx.open(&format!(r"{E}\list"), READ, FILE_OPEN, SYNC | options::FILE_DIRECTORY_FILE);
    let (mut names, st) = fx.list(dir, Some("*.MAP"), true);
    names.sort();
    assert_eq!(
        (names, st),
        (vec!["a.map".to_owned(), "b.map".to_owned()], NtStatus::NO_MORE_FILES.0)
    );
    // FindNextFile passes no mask: the enumeration's own goes on.
    let (iosb, out) = (fx.iosb(), fx.mem(0x200));
    let mask = fx.ansi("?.map");
    let next = |fx: &mut Fx, mask: u32, len: u32, restart: u32| {
        fx.tk.call32("NtQueryDirectoryFile", &[dir, 0, 0, 0, iosb, out, len, 1, mask, restart])
    };
    assert_eq!(next(&mut fx, mask, 0x200, 1), STATUS_SUCCESS);
    let first = FileDirectoryInformation::from_bytes(&fx.bytes(out, fx.r32(iosb + 4))).unwrap();
    let (rest, st) = fx.list(dir, None, false);
    assert_eq!((rest.len(), st), (1, NtStatus::NO_MORE_FILES.0));
    assert_ne!(first.file_name, rest[0]);
    assert_eq!(first.file_attributes, attributes::FILE_ATTRIBUTE_NORMAL);
    assert_eq!(first.end_of_file, if first.file_name == "a.map" { 10 } else { 20 });

    // Everything, a folder among it.
    let (all, _) = fx.list(dir, Some("*"), true);
    assert_eq!(all.len(), 4);
    assert!(all.contains(&"sub".to_owned()));
    let (none, st) = fx.list(dir, Some("zz*"), true);
    assert_eq!((none.len(), st), (0, NtStatus::NO_SUCH_FILE.0));

    // An entry that does not fit, another class, a handle that is no directory's.
    let star = fx.ansi("*");
    assert_eq!(next(&mut fx, star, 0x40, 1), NtStatus::BUFFER_TOO_SMALL.0);
    let st = fx.tk.call32("NtQueryDirectoryFile", &[dir, 0, 0, 0, iosb, out, 0x200, 2, 0, 1]);
    assert_eq!(st, NtStatus::INVALID_INFO_CLASS.0);
    let file = fx.open(&format!(r"{E}\info.bin"), READ, FILE_OPEN, SYNC);
    let st = fx.tk.call32("NtQueryDirectoryFile", &[file, 0, 0, 0, iosb, out, 0x200, 1, 0, 1]);
    assert_eq!(st, NtStatus::INVALID_DEVICE_REQUEST.0);

    // The disc's entries are read-only.
    let maps = fx.open(&format!(r"{CD}\maps"), READ, FILE_OPEN, SYNC);
    assert_eq!(next_on(&mut fx, maps, iosb, out), STATUS_SUCCESS);
    let entry = FileDirectoryInformation::from_bytes(&fx.bytes(out, fx.r32(iosb + 4))).unwrap();
    assert_eq!(
        (entry.file_name.as_str(), entry.end_of_file, entry.file_attributes),
        ("a10.map", u64::from(MAP_LEN), attributes::FILE_ATTRIBUTE_READONLY)
    );
}

fn next_on(fx: &mut Fx, dir: u32, iosb: u32, out: u32) -> u32 {
    fx.tk.call32("NtQueryDirectoryFile", &[dir, 0, 0, 0, iosb, out, 0x200, 1, 0, 0])
}

#[test]
fn a_symbolic_link_is_created_followed_queried_and_deleted() {
    let mut fx = fixture();
    let target = format!(r"{E}\TDATA\4d530004");
    let (name, to) = (fx.ansi(r"\??\T:"), fx.ansi(&target));
    assert_eq!(fx.tk.call32("IoCreateSymbolicLink", &[name, to]), STATUS_SUCCESS);
    let (st, info, _) = fx.create(0, r"\??\T:\keep", READ, share::ALL, FILE_OPEN, SYNC);
    assert_eq!((st, info), (STATUS_SUCCESS, 1));
    // A target under a drive that has no link.
    let (bad, nowhere) = (fx.ansi(r"\??\W:"), fx.ansi(r"\??\Q:\x"));
    let st = fx.tk.call32("IoCreateSymbolicLink", &[bad, nowhere]);
    assert_eq!(st, NtStatus::OBJECT_PATH_NOT_FOUND.0);

    let (oa, h) = (fx.attrs(0, r"\??\t:"), fx.mem(4));
    assert_eq!(fx.tk.call32("NtOpenSymbolicLinkObject", &[h, oa]), STATUS_SUCCESS);
    let h = fx.r32(h);
    assert_eq!(fx.tk.k.ob_lookup(h).unwrap().1, ObType::Symlink);
    // LinkTarget: an ANSI_STRING with room, then one without.
    let (buf, out, ret) = (fx.mem(64), fx.mem(8), fx.mem(4));
    fx.tk.k.gm.w16(out + 2, 64).unwrap();
    fx.tk.k.gm.w32(out + 4, buf).unwrap();
    assert_eq!(fx.tk.call32("NtQuerySymbolicLinkObject", &[h, out, ret]), STATUS_SUCCESS);
    let len = fx.tk.k.gm.r16(out).unwrap();
    assert_eq!((u32::from(len), fx.r32(ret)), (target.len() as u32, target.len() as u32));
    assert_eq!(fx.bytes(buf, u32::from(len)), target.as_bytes());
    fx.tk.k.gm.w16(out + 2, 4).unwrap();
    let st = fx.tk.call32("NtQuerySymbolicLinkObject", &[h, out, ret]);
    assert_eq!(st, NtStatus::BUFFER_TOO_SMALL.0);
    // A link is no file, and a file no link.
    let iosb = fx.iosb();
    let st = fx.tk.call32("NtReadFile", &[h, 0, 0, 0, iosb, buf, 4, 0]);
    assert_eq!(st, STATUS_INVALID_HANDLE);
    let file = fx.open(&format!(r"{E}\info.bin"), READ, FILE_OPEN, SYNC);
    let st = fx.tk.call32("NtQuerySymbolicLinkObject", &[file, out, ret]);
    assert_eq!(st, STATUS_INVALID_HANDLE);

    assert_eq!(fx.tk.call32("NtClose", &[h]), STATUS_SUCCESS);
    assert_eq!(fx.tk.call32("IoDeleteSymbolicLink", &[name]), STATUS_SUCCESS);
    let st = fx.tk.call32("IoDeleteSymbolicLink", &[name]);
    assert_eq!(st, NtStatus::OBJECT_NAME_NOT_FOUND.0);
    let st = fx.tk.call32("NtOpenSymbolicLinkObject", &[ret, oa]);
    assert_eq!(st, NtStatus::OBJECT_NAME_NOT_FOUND.0);
    let (st, info, _) = fx.create(0, r"\??\T:\keep", READ, share::ALL, FILE_OPEN, SYNC);
    assert_eq!((st, info), (NtStatus::OBJECT_PATH_NOT_FOUND.0, 0));
}

#[test]
fn an_overlapped_read_pends_and_completes_through_the_poll_with_its_event_set() {
    let mut fx = fixture();
    let h = fx.open(&format!(r"{CD}\maps\a10.map"), READ, FILE_OPEN, 0);
    let data = pattern(MAP_LEN as usize, 5);
    let ev = fx.mem(4);
    assert_eq!(fx.tk.call32("NtCreateEvent", &[ev, 0, 0, 1]), STATUS_SUCCESS);
    let ev = fx.r32(ev);
    let (event, _) = fx.tk.k.ob_lookup(ev).unwrap();

    let (buf, iosb, off) = (fx.mem(4096), fx.iosb(), fx.put(&2048u64.to_le_bytes()));
    let st = fx.tk.call32("NtReadFile", &[h, ev, 0, 0, iosb, buf, 4096, off]);
    assert_eq!(st, STATUS_PENDING);
    // Until it completes: pending in the IoStatusBlock, the event cleared,
    // nothing in the buffer.
    assert_eq!(fx.status(iosb), (STATUS_PENDING, 0));
    assert_eq!(fx.r32(event + 4), 0);
    assert_eq!(fx.bytes(buf, 4096), vec![0; 4096]);
    fx.pump("the overlapped read", |fx| fx.r32(iosb) != STATUS_PENDING);
    assert_eq!(fx.status(iosb), (STATUS_SUCCESS, 4096));
    assert_eq!(fx.bytes(buf, 4096), data[2048..2048 + 4096]);
    assert_eq!(fx.r32(event + 4), 1);
    // It named its offset: the position is the title's (Burnout 2).
    assert_eq!(fx.position(h), 0);
    assert!(fx.tk.k.io.held.is_empty() && fx.tk.k.io_next_due().is_none());

    // With no event of its own, the file object's: cleared, then set.
    let (obj, _) = fx.tk.k.ob_lookup(h).unwrap();
    let st = fx.tk.call32("NtReadFile", &[h, 0, 0, 0, iosb, buf, 2048, off]);
    assert_eq!((st, fx.r32(obj + FO_EVENT + 4)), (STATUS_PENDING, 0));
    fx.pump("the read without an event", |fx| fx.r32(iosb) != STATUS_PENDING);
    assert_eq!(fx.r32(obj + FO_EVENT + 4), 1);

    // Part of a sector goes through the cache and completes in the call
    // (Wreckless); a read past the end fails at once and leaves the
    // IoStatusBlock alone (Forza).
    let (iosb, zero) = (fx.iosb(), fx.put(&0u64.to_le_bytes()));
    let st = fx.tk.call32("NtReadFile", &[h, ev, 0, 0, iosb, buf, 12, zero]);
    assert_eq!((st, fx.status(iosb)), (STATUS_SUCCESS, (STATUS_SUCCESS, 12)));
    assert_eq!(fx.bytes(buf, 12), data[..12]);
    let (iosb, end) = (fx.iosb(), fx.put(&u64::from(MAP_LEN).to_le_bytes()));
    let st = fx.tk.call32("NtReadFile", &[h, ev, 0, 0, iosb, buf, 2048, end]);
    assert_eq!((st, fx.status(iosb)), (NtStatus::END_OF_FILE.0, (UNTOUCHED, UNTOUCHED)));
    // Zero bytes succeed, there too (Halo's map streaming).
    let st = fx.tk.call32("NtReadFile", &[h, ev, 0, 0, iosb, buf, 0, end]);
    assert_eq!((st, fx.status(iosb)), (STATUS_PENDING, (STATUS_SUCCESS, 0)));
}

#[test]
fn an_overlapped_read_queues_its_apc_to_the_thread_that_issued_it() {
    let mut fx = fixture();
    let h = fx.open(&format!(r"{CD}\maps\a10.map"), READ, FILE_OPEN, 0);
    let (buf, iosb, off) = (fx.mem(2048), fx.iosb(), fx.put(&0u64.to_le_bytes()));
    let st = fx.tk.call32("NtReadFile", &[h, 0, 0x0001_2000, 0x1234, iosb, buf, 2048, off]);
    assert_eq!(st, STATUS_PENDING);
    let ti = fx.thread();
    assert!(fx.tk.k.threads[ti].apcs.is_empty());
    fx.pump("the read with an APC", |fx| fx.r32(iosb) != STATUS_PENDING);
    let apcs: Vec<_> =
        fx.tk.k.threads[ti].apcs.iter().map(|a| (a.kapc, a.normal, a.ctx, a.a1, a.a2)).collect();
    assert_eq!(apcs, [(0, 0x0001_2000, 0x1234, iosb, 0)]);
    assert_eq!(fx.bytes(buf, 2048), pattern(2048, 5));
}

#[test]
fn a_port_gets_a_packet_for_an_overlapped_read_of_its_file() {
    let mut fx = fixture();
    let h = fx.open(&format!(r"{CD}\maps\a10.map"), READ, FILE_OPEN, 0);
    let port = fx.mem(4);
    assert_eq!(fx.tk.call32("NtCreateIoCompletion", &[port, 0, 0, 0]), STATUS_SUCCESS);
    let port = fx.r32(port);
    let info = FileCompletionInformation { port, key: 0x77 }.to_bytes();
    let (iosb, info) = (fx.iosb(), fx.put(&info));
    let st = fx.tk.call32("NtSetInformationFile", &[h, iosb, info, 8, file_class::COMPLETION]);
    assert_eq!(st, STATUS_SUCCESS);
    // A handle that is no port's.
    fx.tk.k.gm.w32(info, h).unwrap();
    let st = fx.tk.call32("NtSetInformationFile", &[h, iosb, info, 8, file_class::COMPLETION]);
    assert_eq!(st, STATUS_INVALID_HANDLE);

    let (buf, depth, off) = (fx.mem(2048), fx.mem(4), fx.put(&0u64.to_le_bytes()));
    let st = fx.tk.call32("NtReadFile", &[h, 0, 0, 0x5555, iosb, buf, 2048, off]);
    assert_eq!(st, STATUS_PENDING);
    assert_eq!(fx.tk.call32("NtQueryIoCompletion", &[port, depth]), STATUS_SUCCESS);
    assert_eq!(fx.r32(depth), 0);
    fx.pump("the read on a port", |fx| fx.r32(iosb) != STATUS_PENDING);
    fx.tk.call32("NtQueryIoCompletion", &[port, depth]);
    assert_eq!(fx.r32(depth), 1);
}

#[test]
fn a_synchronous_read_waits_out_the_drives_time_with_its_thread_parked() {
    let mut fx = fixture();
    fx.tk.k.io_title_timing(0x4D53_0004, "Halo"); // the drives' own time
    let timing = fx.tk.k.io.timing;
    assert!(timing.enabled);
    let h = fx.open(&format!(r"{CD}\maps\a10.map"), READ, FILE_OPEN, SYNC);
    let data = pattern(MAP_LEN as usize, 5);
    const LEN: u32 = 0x8_0000;
    let (buf, iosb) = (fx.mem(LEN), fx.iosb());
    let ti = fx.thread();

    let issued = fx.tk.k.now();
    let st = fx.tk.call32("NtReadFile", &[h, 0, 0, 0, iosb, buf, LEN, 0]);
    assert_eq!(st, STATUS_PENDING);
    assert_eq!(fx.tk.k.threads[ti].state, ThreadState::Waiting);
    // As the scheduler does before it looks for completed I/O.
    fx.tk.k.threads[ti].ctx = fx.tk.k.frame;

    // The guest's time stands still short of the read's due: the drive is
    // done (the worker has the bytes) and the thread still waits.
    fx.tk.k.clock.pause_begin();
    let cost = timing.cost(DeviceKind::Disc, LEN).as_nanos() as u64;
    let t0 = Instant::now();
    while fx.tk.k.io.held.is_empty() {
        assert!(t0.elapsed() < Duration::from_secs(10), "the worker did not read");
        std::thread::sleep(Duration::from_millis(1));
        fx.tk.k.io_poll();
    }
    let due = fx.tk.k.io_next_due().expect("a held request has a due time");
    assert!(due >= issued + cost && fx.tk.k.now() < due);
    fx.tk.k.io_poll();
    assert_eq!(fx.tk.k.threads[ti].state, ThreadState::Waiting);
    assert_eq!(fx.status(iosb), (UNTOUCHED, UNTOUCHED));
    assert_eq!(fx.bytes(buf, 16), [0; 16]);

    // The time goes on: at the due, the thread's call returns the status.
    fx.tk.k.clock.pause_end();
    fx.pump("the timed read", |fx| fx.tk.k.threads[fx.thread()].state == ThreadState::Ready);
    assert!(fx.tk.k.now() >= due);
    assert_eq!(fx.tk.k.threads[ti].ctx.regs.gpr[reg::EAX], STATUS_SUCCESS);
    assert_eq!(fx.status(iosb), (STATUS_SUCCESS, LEN));
    assert_eq!(fx.bytes(buf, LEN), data[..LEN as usize]);
    assert_eq!(fx.position(h), u64::from(LEN));
    assert!(fx.tk.k.io_next_due().is_none());

    // The thread runs again. What that read brought into the cache takes no
    // drive time; a read outside it waits for the drive again.
    fx.tk.k.frame = fx.tk.k.threads[ti].ctx;
    fx.tk.k.threads[ti].state = ThreadState::Running;
    let off = fx.put(&0x1000u64.to_le_bytes());
    let st = fx.tk.call32("NtReadFile", &[h, 0, 0, 0, iosb, buf, 4096, off]);
    assert_eq!((st, fx.tk.k.threads[ti].state), (STATUS_SUCCESS, ThreadState::Running));
    assert_eq!(fx.bytes(buf, 4096), data[0x1000..0x2000]);
    let off = fx.put(&u64::from(LEN).to_le_bytes());
    let st = fx.tk.call32("NtReadFile", &[h, 0, 0, 0, iosb, buf, 4096, off]);
    assert_eq!((st, fx.tk.k.threads[ti].state), (STATUS_PENDING, ThreadState::Waiting));
}

#[test]
fn the_drives_statistics_charge_a_timed_read_its_drive_time() {
    let mut fx = fixture();
    fx.tk.k.io_title_timing(0x4D53_0004, "Halo");
    let timing = fx.tk.k.io.timing;
    let h = fx.open(&format!(r"{CD}\maps\a10.map"), READ, FILE_OPEN, SYNC);
    const LEN: u32 = 0x8_0000;
    let (buf, iosb) = (fx.mem(LEN), fx.iosb());
    let ti = fx.thread();
    let st = fx.tk.call32("NtReadFile", &[h, 0, 0, 0, iosb, buf, LEN, 0]);
    assert_eq!(st, STATUS_PENDING);
    fx.tk.k.threads[ti].ctx = fx.tk.k.frame;
    fx.pump("the timed read", |fx| fx.tk.k.threads[fx.thread()].state == ThreadState::Ready);

    let cost = timing.cost(DeviceKind::Disc, LEN).as_nanos() as u64;
    let disc = fx.tk.k.io.stats[DISC];
    assert_eq!((disc.queued, disc.in_place, disc.bytes), (1, 0, u64::from(LEN)));
    assert_eq!(disc.charged_ns, cost);
    // The title waited the drive's time at least, on the host's clock too,
    // and the read itself took a fraction of it.
    assert!(disc.wall_ns >= cost, "{} < {cost}", disc.wall_ns);
    assert!(disc.host_ns < disc.wall_ns);
    assert_eq!(fx.tk.k.io.stats[HARD_DISK].queued, 0);

    // What the cache holds is read in the call and charged nothing.
    fx.tk.k.frame = fx.tk.k.threads[ti].ctx;
    fx.tk.k.threads[ti].state = ThreadState::Running;
    let off = fx.put(&0x1000u64.to_le_bytes());
    let st = fx.tk.call32("NtReadFile", &[h, 0, 0, 0, iosb, buf, 4096, off]);
    assert_eq!(st, STATUS_SUCCESS);
    let disc = fx.tk.k.io.stats[DISC];
    assert_eq!((disc.queued, disc.in_place, disc.charged_ns), (1, 1, cost));

    // The statistics line takes them.
    fx.tk.k.io_stats_log(1.0);
    assert_eq!(fx.tk.k.io.stats[DISC].queued + fx.tk.k.io.stats[DISC].in_place, 0);
}

#[test]
fn the_fast_disc_speed_spares_a_synchronous_read_the_drives_time_except_for_a_title_that_needs_it()
{
    // Chosen fast, Halo's short synchronous read completes in its call.
    let mut fx = fixture();
    fx.tk.k.console.device_timing = Some(TimingMode::Off);
    fx.tk.k.io_title_timing(0x4D53_0004, "Halo");
    assert!(!fx.tk.k.io.timing.enabled);
    let h = fx.open(&format!(r"{CD}\maps\a10.map"), READ, FILE_OPEN, SYNC);
    let (buf, iosb) = (fx.mem(4096), fx.iosb());
    let st = fx.tk.call32("NtReadFile", &[h, 0, 0, 0, iosb, buf, 4096, 0]);
    assert_eq!(st, STATUS_SUCCESS);
    assert_eq!(fx.status(iosb), (STATUS_SUCCESS, 4096));

    // KotOR II keeps the drives' own time whatever was chosen.
    let mut fx = fixture();
    fx.tk.k.console.device_timing = Some(TimingMode::Off);
    fx.tk.k.io_title_timing(0x4C41_0014, "KotOR II");
    assert_eq!(fx.tk.k.io.timing, DeviceTiming::for_mode(TimingMode::Console));
    let h = fx.open(&format!(r"{CD}\maps\a10.map"), READ, FILE_OPEN, SYNC);
    let (buf, iosb) = (fx.mem(4096), fx.iosb());
    let st = fx.tk.call32("NtReadFile", &[h, 0, 0, 0, iosb, buf, 4096, 0]);
    assert_eq!(st, STATUS_PENDING);
}

#[test]
fn a_disc_speed_chosen_while_the_title_runs_is_used_from_the_next_entry() {
    let mut fx = fixture();
    fx.tk.k.console.device_timing = Some(TimingMode::Console);
    fx.tk.k.io_title_timing(0x4D53_0004, "Halo");
    assert_eq!(fx.tk.k.io.timing, DeviceTiming::for_mode(TimingMode::Console));
    let control = fx.tk.k.control();
    control.set_device_timing(TimingMode::Off);
    assert_eq!(
        fx.tk.k.io.timing,
        DeviceTiming::for_mode(TimingMode::Console),
        "not before an entry"
    );
    fx.tk.tail();
    assert_eq!(fx.tk.k.io.timing, DeviceTiming::for_mode(TimingMode::Off));
    control.set_device_timing(TimingMode::Console);
    fx.tk.tail();
    assert_eq!(fx.tk.k.io.timing, DeviceTiming::for_mode(TimingMode::Console));

    // A title that needs the drives' own time keeps it.
    let mut fx = fixture();
    fx.tk.k.io_title_timing(0x4C41_0014, "KotOR II");
    fx.tk.k.control().set_device_timing(TimingMode::Off);
    fx.tk.tail();
    assert_eq!(fx.tk.k.io.timing, DeviceTiming::for_mode(TimingMode::Console));
}

#[test]
fn overlapped_requests_complete_in_the_order_they_were_issued_across_the_drives() {
    let mut fx = fixture();
    let disc = fx.open(&format!(r"{CD}\maps\a10.map"), READ, FILE_OPEN, 0);
    let (st, _, disk) = fx.create(
        0,
        &format!(r"{E}\copy.bin"),
        RW,
        share::ALL,
        FILE_CREATE,
        options::FILE_NO_INTERMEDIATE_BUFFERING,
    );
    assert_eq!(st, STATUS_SUCCESS);
    let (buf, zero) = (fx.pages(2), fx.put(&0u64.to_le_bytes()));
    let (read, write) = (fx.iosb(), fx.iosb());
    let ti = fx.thread();

    // The disc's read first; the guest's time stands still before its drive
    // time is over. The hard disk's write, asked for after it, is done first.
    fx.tk.k.clock.pause_begin();
    let st = fx.tk.call32("NtReadFile", &[disc, 0, 0xA000, 1, read, buf, 4096, zero]);
    assert_eq!(st, STATUS_PENDING);
    let st = fx.tk.call32("NtWriteFile", &[disk, 0, 0xB000, 2, write, buf + 4096, 4096, zero]);
    assert_eq!(st, STATUS_PENDING);
    let t0 = Instant::now();
    while fx.tk.k.io.held.len() < 2 {
        assert!(t0.elapsed() < Duration::from_secs(10), "the workers did not finish");
        std::thread::sleep(Duration::from_millis(1));
        fx.tk.k.io_poll();
    }
    // The write waits for the read, which waits for its drive.
    assert_eq!((fx.r32(read), fx.r32(write)), (STATUS_PENDING, STATUS_PENDING));
    assert_eq!(fx.e.file_bytes("copy.bin").unwrap().len(), 4096);
    assert!(fx.tk.k.threads[ti].apcs.is_empty());

    fx.tk.k.clock.pause_end();
    fx.pump("the two requests", |fx| fx.r32(write) != STATUS_PENDING);
    assert_eq!((fx.status(read), fx.status(write)), ((0, 4096), (0, 4096)));
    let order: Vec<u32> = fx.tk.k.threads[ti].apcs.iter().map(|a| a.normal).collect();
    assert_eq!(order, [0xA000, 0xB000]);
}

#[test]
fn a_close_waits_for_the_files_requests_in_flight() {
    let mut fx = fixture();
    // Oddworld: Stranger's Wrath writes overlapped and unbuffered and closes
    // at once; the write's IoStatusBlock is complete by the close's return.
    let (st, _, h) = fx.create(
        0,
        &format!(r"{E}\quick.bin"),
        RW,
        share::ALL,
        FILE_CREATE,
        options::FILE_NO_INTERMEDIATE_BUFFERING,
    );
    assert_eq!(st, STATUS_SUCCESS);
    let data = pattern(512, 7);
    let (src, iosb, zero) = (fx.put(&data), fx.iosb(), fx.put(&0u64.to_le_bytes()));
    let st = fx.tk.call32("NtWriteFile", &[h, 0, 0, 0, iosb, src, 512, zero]);
    assert_eq!((st, fx.status(iosb)), (STATUS_PENDING, (STATUS_PENDING, 0)));
    assert_eq!(fx.tk.call32("NtClose", &[h]), STATUS_SUCCESS);
    assert_eq!(fx.status(iosb), (STATUS_SUCCESS, 512));
    assert_eq!(fx.e.file_bytes("quick.bin").unwrap(), data);
    assert!(fx.tk.k.io.files.is_empty() && fx.tk.k.io.busy == [0, 0]);

    // The same with drive time left and the guest's clock standing still.
    let h = fx.open(&format!(r"{CD}\maps\a10.map"), READ, FILE_OPEN, 0);
    let (buf, iosb) = (fx.mem(2048), fx.iosb());
    fx.tk.k.clock.pause_begin();
    let st = fx.tk.call32("NtReadFile", &[h, 0, 0, 0, iosb, buf, 2048, zero]);
    assert_eq!(st, STATUS_PENDING);
    assert_eq!(fx.tk.call32("NtClose", &[h]), STATUS_SUCCESS);
    assert_eq!(fx.status(iosb), (STATUS_SUCCESS, 2048));
    assert_eq!(fx.bytes(buf, 2048), pattern(2048, 5));
}

#[test]
fn a_cached_overlapped_write_completes_in_the_call() {
    let mut fx = fixture();
    // Breakdown takes a write that pends for a failed one.
    let (st, _, h) = fx.create(0, &format!(r"{E}\cached.bin"), RW, share::ALL, FILE_CREATE, 0);
    assert_eq!(st, STATUS_SUCCESS);
    let ev = fx.mem(4);
    fx.tk.call32("NtCreateEvent", &[ev, 0, 0, 0]);
    let ev = fx.r32(ev);
    let (event, _) = fx.tk.k.ob_lookup(ev).unwrap();
    let data = pattern(0x4000, 8);
    let (src, iosb, zero) = (fx.put(&data), fx.iosb(), fx.put(&0u64.to_le_bytes()));
    let st = fx.tk.call32("NtWriteFile", &[h, ev, 0, 0, iosb, src, 0x4000, zero]);
    assert_eq!((st, fx.status(iosb)), (STATUS_SUCCESS, (STATUS_SUCCESS, 0x4000)));
    assert_eq!(fx.r32(event + 4), 1);
    assert_eq!(fx.e.file_bytes("cached.bin").unwrap(), data);
    // A handle opened to read refuses, and the disc.
    let reader = fx.open(&format!(r"{E}\cached.bin"), READ, FILE_OPEN, SYNC);
    let iosb = fx.iosb();
    let st = fx.tk.call32("NtWriteFile", &[reader, 0, 0, 0, iosb, src, 16, zero]);
    assert_eq!((st, fx.r32(iosb)), (NtStatus::ACCESS_DENIED.0, NtStatus::ACCESS_DENIED.0));
    let xbe = fx.open(&format!(r"{CD}\default.xbe"), RW, FILE_OPEN, SYNC);
    let st = fx.tk.call32("NtWriteFile", &[xbe, 0, 0, 0, iosb, src, 16, zero]);
    assert_eq!(st, NtStatus::ACCESS_DENIED.0);
    assert_eq!(fx.e.file_bytes("cached.bin").unwrap(), data);
}

#[test]
fn a_buffer_the_guest_does_not_have_faults_before_the_call_has_any_effect() {
    let mut fx = fixture();
    let h = fx.open(&format!(r"{E}\info.bin"), RW, FILE_OPEN, SYNC);
    let iosb = fx.iosb();
    let mut p = TestPlatform::default();
    let mut args: Args = [0; 16];
    args[..8].copy_from_slice(&[h, 0, 0, 0, iosb, 0x7000_0000, 5000, 0]);
    let fault = nt_read_file(&mut fx.tk.k, &mut p, &args);
    assert_eq!(fault, Err(Fault { va: 0x7000_0000, write: true }));
    // A buffer that runs off its last page: the first page that is missing.
    let page = fx.tk.alloc(PAGE_SIZE);
    args[5] = page + PAGE_SIZE - 16;
    let fault = nt_read_file(&mut fx.tk.k, &mut p, &args);
    assert_eq!(fault, Err(Fault { va: page + PAGE_SIZE, write: true }));
    assert_eq!(fx.bytes(page + PAGE_SIZE - 16, 16), [0; 16]);
    // A write's source is loaded, not stored.
    let fault = nt_write_file(&mut fx.tk.k, &mut p, &args);
    assert_eq!(fault, Err(Fault { va: page + PAGE_SIZE, write: false }));
    // Nothing happened: the position, the IoStatusBlock, the thread, the file.
    assert_eq!(fx.position(h), 0);
    assert_eq!(fx.status(iosb), (UNTOUCHED, UNTOUCHED));
    assert_eq!(fx.tk.k.threads[fx.thread()].state, ThreadState::Running);
    assert_eq!(fx.e.file_bytes("info.bin").unwrap(), pattern(5000, 1));
    assert!(fx.tk.k.io.workers.is_none());
    // An IoStatusBlock nowhere, the same.
    args[4] = 0x7000_0000;
    args[5] = page;
    args[6] = 16;
    let fault = nt_read_file(&mut fx.tk.k, &mut p, &args);
    assert_eq!(fault, Err(Fault { va: 0x7000_0000, write: true }));
    assert_eq!(fx.position(h), 0);

    // Through the dispatcher the title's handlers get it; with none, the
    // guest stops there.
    fx.tk.call32("NtReadFile", &[h, 0, 0, 0, iosb, 0x7000_0000, 5000, 0]);
    assert!(fx.tk.stop_reason().is_some());
}

#[test]
fn scatter_and_gather_work_on_the_pages_their_array_names() {
    let mut fx = fixture();
    let (st, _, h) = fx.create(
        0,
        &format!(r"{E}\pages.bin"),
        RW,
        share::ALL,
        FILE_CREATE,
        options::FILE_NO_INTERMEDIATE_BUFFERING,
    );
    assert_eq!(st, STATUS_SUCCESS);
    let src = fx.pages(3);
    let data = pattern(3 * PAGE_SIZE as usize, 11);
    fx.tk.k.gm.write(src, &data).unwrap();
    // The file's pages in the order 2, 0, 1 of the buffer; offsets in the
    // page are left out.
    let order = [2u32, 0, 1];
    let array = fx.mem(16);
    for (i, page) in order.iter().enumerate() {
        fx.tk.k.gm.w32(array + 4 * i as u32, src + page * PAGE_SIZE + 0x123).unwrap();
    }
    let (iosb, zero) = (fx.iosb(), fx.put(&0u64.to_le_bytes()));
    let len = 2 * PAGE_SIZE + 100;
    let st = fx.tk.call32("NtWriteFileGather", &[h, 0, 0, 0, iosb, array, len, zero]);
    assert_eq!((st, fx.status(iosb)), (STATUS_PENDING, (STATUS_SUCCESS, len)));
    let page = |n: u32| &data[(n * PAGE_SIZE) as usize..((n + 1) * PAGE_SIZE) as usize];
    let expect = [page(2), page(0), &page(1)[..100]].concat();
    assert_eq!(fx.e.file_bytes("pages.bin").unwrap(), expect);

    // Read back into other pages, last one first.
    let dst = fx.pages(3);
    for (i, page) in [2u32, 1, 0].iter().enumerate() {
        fx.tk.k.gm.w32(array + 4 * i as u32, dst + page * PAGE_SIZE).unwrap();
    }
    let iosb = fx.iosb();
    let st = fx.tk.call32("NtReadFileScatter", &[h, 0, 0, 0, iosb, array, len, zero]);
    assert_eq!(st, STATUS_PENDING);
    fx.pump("the scattered read", |fx| fx.r32(iosb) != STATUS_PENDING);
    assert_eq!(fx.status(iosb), (STATUS_SUCCESS, len));
    assert_eq!(fx.bytes(dst + 2 * PAGE_SIZE, PAGE_SIZE), expect[..PAGE_SIZE as usize]);
    assert_eq!(
        fx.bytes(dst + PAGE_SIZE, PAGE_SIZE),
        expect[PAGE_SIZE as usize..2 * PAGE_SIZE as usize]
    );
    assert_eq!(fx.bytes(dst, 100), expect[2 * PAGE_SIZE as usize..]);
    assert_eq!(fx.bytes(dst + 100, 16), [0; 16]);
}

#[test]
fn device_controls_answer_for_the_disk_and_the_dvd_drive() {
    let mut fx = fixture();
    let h = fx.open(&format!(r"{E}\info.bin"), READ, FILE_OPEN, SYNC);
    let (iosb, out) = (fx.iosb(), fx.mem(64));
    let control = |fx: &mut Fx, h: u32, code: u32, input: (u32, u32), output: (u32, u32)| {
        fx.tk.call32(
            "NtDeviceIoControlFile",
            &[h, 0, 0, 0, iosb, code, input.0, input.1, output.0, output.1],
        )
    };
    let st = control(&mut fx, h, ioctl::IOCTL_DISK_GET_PARTITION_INFO, (0, 0), (out, 32));
    assert_eq!((st, fx.status(iosb)), (STATUS_SUCCESS, (STATUS_SUCCESS, 32)));
    assert_eq!(fx.bytes(out, 32), ioctl::partition_info(1).unwrap());
    let st = control(&mut fx, h, ioctl::IOCTL_DISK_GET_PARTITION_INFO, (0, 0), (out, 16));
    assert_eq!(st, NtStatus::BUFFER_TOO_SMALL.0);
    let st = control(&mut fx, h, 0x0012_3456, (0, 0), (out, 32));
    assert_eq!((st, fx.r32(iosb)), (NtStatus::INVALID_DEVICE_REQUEST.0, st));
    assert_eq!(fx.tk.k.io.control_told, [0x0012_3456]);
    let st = fx
        .tk
        .call32("NtFsControlFile", &[h, 0, 0, 0, iosb, ioctl::FSCTL_DISMOUNT_VOLUME, 0, 0, 0, 0]);
    assert_eq!((st, fx.status(iosb)), (STATUS_SUCCESS, (STATUS_SUCCESS, 0)));

    // MODE SENSE(10) of the Xbox security page through the pass-through
    // (Panzer Dragoon Orta): authenticated, and a good SCSI status.
    let cd = fx.open(CD, READ, FILE_OPEN, SYNC);
    let data = fx.mem(28);
    let mut cdb = [0u8; 16];
    (cdb[0], cdb[2]) = (0x5A, 0x3E);
    let pass = ScsiPassThroughDirect { data_transfer_length: 28, data_buffer: data, cdb };
    let pass = fx.put(&pass.to_bytes());
    fx.tk.k.gm.w8(pass + 2, 0xFF).unwrap();
    let st = control(&mut fx, cd, ioctl::IOCTL_SCSI_PASS_THROUGH_DIRECT, (pass, 0x2C), (0, 0));
    assert_eq!(st, STATUS_SUCCESS);
    let d = fx.bytes(data, 28);
    assert_eq!((d[8], d[10], d[11], d[12]), (0x3E, 1, 1, 1));
    assert_eq!(fx.tk.k.gm.r8(pass + 2).unwrap(), 0);
    // A data buffer the guest does not have.
    fx.tk.k.gm.w32(pass + 0x14, 0x7000_0000).unwrap();
    let mut p = TestPlatform::default();
    let mut args: Args = [0; 16];
    args[..8].copy_from_slice(&[cd, 0, 0, 0, iosb, 0x4D014, pass, 0x2C]);
    let fault = nt_control_file(&mut fx.tk.k, &mut p, &args);
    assert_eq!(fault, Err(Fault { va: 0x7000_0000, write: true }));

    // IoSynchronousDeviceIoControlRequest: zeros.
    fx.tk.k.gm.fill(out, 0xAA, 64).unwrap();
    let ret = fx.put(&[0xAA; 4]);
    let st = fx.tk.call32(
        "IoSynchronousDeviceIoControlRequest",
        &[0x0007_0000, 0x8001_0000, 0, 0, out, 24, ret, 0],
    );
    assert_eq!((st, fx.r32(ret)), (STATUS_SUCCESS, 0));
    assert_eq!(fx.bytes(out, 25), [[0u8; 24].as_slice(), &[0xAA]].concat());
}

#[test]
fn device_objects_are_created_named_and_found_by_name() {
    let mut fx = fixture();
    let (name, out) = (fx.ansi(r"\Device\MU_0"), fx.mem(4));
    let create = |fx: &mut Fx, name: u32| {
        fx.tk.call32("IoCreateDevice", &[0x001C_F5A8, 0x170, name, 0x3A, 0, out])
    };
    assert_eq!(create(&mut fx, name), STATUS_SUCCESS);
    let d = fx.r32(out);
    let gm = &fx.tk.k.gm;
    assert_eq!((gm.r16(d).unwrap(), gm.r16(d + 2).unwrap()), (3, 0x48 + 0x170));
    assert_eq!((fx.r32(d + 8), fx.r32(d + 0x14), fx.r32(d + 0x18)), (0x001C_F5A8, 0x80, d + 0x48));
    assert_eq!(fx.bytes(d + 0x1C, 4), [0x3A, 0, 1, 0]);
    assert_eq!(create(&mut fx, name), NtStatus::OBJECT_NAME_COLLISION.0);
    assert_eq!(fx.r32(out), d);
    // Unnamed devices are each their own.
    assert_eq!(create(&mut fx, 0), STATUS_SUCCESS);
    assert_ne!(fx.r32(out), d);

    let found = fx.mem(4);
    let by_name = |fx: &mut Fx, name: &str| {
        let name = fx.ansi(name);
        (fx.tk.call32("ObReferenceObjectByName", &[name, 0, 0, 0, found]), fx.r32(found))
    };
    assert_eq!(by_name(&mut fx, r"\device\mu_0"), (STATUS_SUCCESS, d));
    // The disc and the partitions get an object on first reference.
    let (st, cd) = by_name(&mut fx, r"\??\D:");
    assert_eq!((st, fx.tk.k.gm.r8(cd + 0x1C).unwrap()), (STATUS_SUCCESS, 2));
    assert_eq!(by_name(&mut fx, CD), (STATUS_SUCCESS, cd));
    let (st, disk) = by_name(&mut fx, E);
    assert_eq!((st, fx.tk.k.gm.r8(disk + 0x1C).unwrap()), (STATUS_SUCCESS, 7));
    assert_eq!(by_name(&mut fx, r"\Device\Nope").0, NtStatus::OBJECT_NAME_NOT_FOUND.0);

    fx.tk.call32("IoDeleteDevice", &[d]);
    assert_eq!(fx.tk.k.gm.r8(d + 0x1F).unwrap(), 1);
    assert_eq!(by_name(&mut fx, r"\Device\MU_0").0, NtStatus::OBJECT_NAME_NOT_FOUND.0);
}

// ---------------------------------------------------------------- the mount

/// A folder under the system's temporary directory, removed when dropped.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static SERIAL: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "xb-kernel-io-{}-{}-{tag}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A file or directory for [`xdvdfs_image`].
enum Node {
    File(&'static str, Vec<u8>),
    Dir(&'static str, Vec<Node>),
}

const SECTOR: usize = 2048;

fn put(image: &mut Vec<u8>, data: &[u8]) -> u32 {
    let sector = image.len() / SECTOR;
    image.extend_from_slice(data);
    image.resize((sector + data.len().div_ceil(SECTOR).max(1)) * SECTOR, 0);
    sector as u32
}

/// Writes a directory's table (its entries a list by name, each the right
/// child of the one before) and everything under it; its sector and size.
fn put_dir(image: &mut Vec<u8>, nodes: Vec<Node>) -> (u32, u32) {
    if nodes.is_empty() {
        return (0, 0);
    }
    let mut items: Vec<(&str, u32, u32, u8)> = nodes
        .into_iter()
        .map(|n| match n {
            Node::File(name, data) => (name, put(image, &data), data.len() as u32, 0x20),
            Node::Dir(name, kids) => {
                let (sector, size) = put_dir(image, kids);
                (name, sector, size, 0x10)
            }
        })
        .collect();
    items.sort_by_key(|i| i.0.to_ascii_uppercase());
    let mut table = vec![0xFFu8; SECTOR];
    let mut off = 0;
    for (i, (name, sector, size, attr)) in items.iter().enumerate() {
        let next = off + ((14 + name.len() + 3) & !3);
        assert!(next <= SECTOR, "the test's directory is one sector");
        let right = if i + 1 < items.len() { (next / 4) as u16 } else { 0 };
        let e = &mut table[off..next];
        e.fill(0);
        e[2..4].copy_from_slice(&right.to_le_bytes());
        e[4..8].copy_from_slice(&sector.to_le_bytes());
        e[8..12].copy_from_slice(&size.to_le_bytes());
        (e[12], e[13]) = (*attr, name.len() as u8);
        e[14..14 + name.len()].copy_from_slice(name.as_bytes());
        off = next;
    }
    (put(image, &table), SECTOR as u32)
}

/// An extracted XISO holding `root` (`docs/hw/xdvdfs.md`).
fn xdvdfs_image(root: Vec<Node>) -> Arc<dyn BlockSource> {
    let mut image = vec![0u8; 33 * SECTOR];
    let (sector, size) = put_dir(&mut image, root);
    let vd = &mut image[32 * SECTOR..33 * SECTOR];
    vd[..20].copy_from_slice(xb_formats::xdvdfs::MAGIC);
    vd[20..24].copy_from_slice(&sector.to_le_bytes());
    vd[24..28].copy_from_slice(&size.to_le_bytes());
    vd[0x7EC..0x7EC + 20].copy_from_slice(xb_formats::xdvdfs::MAGIC);
    Arc::new(MemSource(image))
}

/// A kernel with nothing mounted yet and scratch memory.
fn unmounted() -> Fx {
    let mut tk = kernel();
    tk.k.clock.pause_end();
    let next = tk.alloc(SCRATCH);
    let none = Arc::new(MemDisc::new());
    Fx { tk, e: Arc::new(MemVolume::new()), disc: none, next, end: next + SCRATCH }
}

#[test]
fn the_mount_puts_the_disc_and_the_hard_disks_folder_behind_the_exports() {
    let hdd = TempDir::new("mount");
    let xbe = pattern(3 * SECTOR + 77, 21);
    let image = xdvdfs_image(vec![
        Node::File("default.xbe", xbe.clone()),
        Node::File("zz.txt", b"last".to_vec()),
        Node::Dir(
            "media",
            vec![Node::Dir("English", vec![Node::File("a.txt", b"hello".to_vec())])],
        ),
        Node::Dir("empty", Vec::new()),
    ]);
    let parsed = Xdvdfs::open(Arc::clone(&image)).unwrap();
    let mut fx = unmounted();
    fx.tk.k.io_mount(Some(image), hdd.path()).unwrap();

    // The disc: a file's bytes, its start sector, a listing in name order.
    let h = fx.open(r"\??\D:\default.xbe", READ, FILE_OPEN, SYNC);
    let (buf, iosb) = (fx.mem(0x2000), fx.iosb());
    let st = fx.call_sync("NtReadFile", &[h, 0, 0, 0, iosb, buf, 0x2000, 0]);
    assert_eq!((st, fx.status(iosb)), (STATUS_SUCCESS, (STATUS_SUCCESS, xbe.len() as u32)));
    assert_eq!(fx.bytes(buf, xbe.len() as u32), xbe);
    let sector = parsed.lookup("default.xbe").unwrap().sector;
    assert_eq!(fx.query(h, file_class::INTERNAL, 8).unwrap(), u64::from(sector).to_le_bytes());
    let root = fx.open(r"\??\CdRom0:\", READ, FILE_OPEN, SYNC | options::FILE_DIRECTORY_FILE);
    let (names, st) = fx.list(root, None, true);
    assert_eq!(names, ["default.xbe", "empty", "media", "zz.txt"]);
    assert_eq!(st, NtStatus::NO_MORE_FILES.0);
    let h = fx.open(&format!(r"{CD}\MEDIA\english\A.TXT"), READ, FILE_OPEN, SYNC);
    let st = fx.call_sync("NtReadFile", &[h, 0, 0, 0, iosb, buf, 5, 0]);
    assert_eq!((st, fx.bytes(buf, 5)), (STATUS_SUCCESS, b"hello".to_vec()));
    let empty = fx.open(&format!(r"{CD}\empty"), READ, FILE_OPEN, SYNC);
    assert_eq!(fx.list(empty, None, true), (Vec::new(), NtStatus::NO_SUCH_FILE.0));
    // English has a folder, German none beside it.
    assert!(fx.tk.k.io_disc_lacks_language(3));
    assert!(!fx.tk.k.io_disc_lacks_language(1));

    // The hard disk: a file written through the exports is a host file.
    let dash = r"\Device\Harddisk0\Partition2\xboxdash.xbe";
    assert!(fx.tk.k.io_host_source(dash).is_none());
    let h = fx.open(dash, RW, FILE_CREATE, SYNC);
    let src = fx.put(&xbe[..1000]);
    let st = fx.call_sync("NtWriteFile", &[h, 0, 0, 0, iosb, src, 1000, 0]);
    assert_eq!(st, STATUS_SUCCESS);
    let st = fx.tk.call32("NtFlushBuffersFile", &[h, iosb]);
    assert_eq!((st, fx.status(iosb)), (STATUS_SUCCESS, (STATUS_SUCCESS, 0)));
    assert_eq!(fx.tk.call32("NtClose", &[h]), STATUS_SUCCESS);
    assert_eq!(std::fs::read(hdd.path().join("Partition2/xboxdash.xbe")).unwrap(), xbe[..1000]);
    let source = fx.tk.k.io_host_source(r"\device\harddisk0\partition2\XBOXDASH.XBE").unwrap();
    let mut back = vec![0u8; 1000];
    source.read_at(0, &mut back).unwrap();
    assert_eq!((source.len(), back.as_slice()), (1000, &xbe[..1000]));
    // Closed, it shares nothing any more: it opens again at once, alone.
    let (st, _, alone) = fx.create(0, dash, RW, 0, FILE_OPEN, SYNC);
    assert_eq!(st, STATUS_SUCCESS);
    fx.tk.call32("NtClose", &[alone]);

    // The launch directory becomes D:.
    fx.tk.k.io_set_launch_dir(r"\Device\Harddisk0\Partition2");
    let (st, ..) = fx.create(0, r"\??\D:\xboxdash.xbe", READ, share::ALL, FILE_OPEN, SYNC);
    assert_eq!(st, STATUS_SUCCESS);

    // The refurb info lives in the config area: zeros until written.
    let info = fx.put(&[0xAA; 16]);
    assert_eq!(fx.tk.call32("ExReadWriteRefurbInfo", &[info, 16, 0]), STATUS_SUCCESS);
    assert_eq!(fx.bytes(info, 16), [0; 16]);
    fx.tk.k.gm.write(info, &pattern(16, 3)).unwrap();
    assert_eq!(fx.tk.call32("ExReadWriteRefurbInfo", &[info, 16, 1]), STATUS_SUCCESS);
    fx.tk.k.gm.fill(info, 0, 16).unwrap();
    assert_eq!(fx.tk.call32("ExReadWriteRefurbInfo", &[info, 16, 0]), STATUS_SUCCESS);
    let stored = fx.bytes(info, 16);
    assert_eq!((&stored[..4], &stored[4..]), (b"BRFR".as_slice(), &pattern(16, 3)[4..]));
    let st = fx.tk.call32("ExReadWriteRefurbInfo", &[info, 12, 0]);
    assert_eq!(st, STATUS_INVALID_PARAMETER);
}

#[test]
fn an_image_with_no_file_system_is_an_empty_tray() {
    let hdd = TempDir::new("empty-tray");
    let mut fx = unmounted();
    let blank: Arc<dyn BlockSource> = Arc::new(MemSource(vec![0u8; 64 * SECTOR]));
    fx.tk.k.io_mount(Some(blank), hdd.path()).unwrap();
    let (st, ..) = fx.create(0, r"\??\D:\default.xbe", READ, share::ALL, FILE_OPEN, SYNC);
    assert_eq!(st, NtStatus::NO_MEDIA_IN_DEVICE.0);
    assert!(!fx.tk.k.io_disc_lacks_language(3));
    // The hard disk is there all the same.
    let (st, info, _) = fx.create(
        0,
        r"\Device\Harddisk0\partition1\TDATA",
        RW,
        share::ALL,
        FILE_OPEN_IF,
        SYNC | options::FILE_DIRECTORY_FILE,
    );
    assert_eq!((st, info), (STATUS_SUCCESS, 2));
    assert!(hdd.path().join("Partition1/TDATA").is_dir());
}

#[test]
fn the_real_disc_opens_its_xbe_and_lists_its_root_through_the_exports() {
    let Some(path) = xb_testkit::any_title() else { return };
    let image = xb_formats::disc::open(&path).unwrap().source;
    let hdd = TempDir::new("real-disc");
    let mut fx = unmounted();
    fx.tk.k.io_mount(Some(image), hdd.path()).unwrap();
    let h = fx.open(r"\Device\CdRom0\default.xbe", READ, FILE_OPEN, SYNC);
    let (buf, iosb) = (fx.mem(4096), fx.iosb());
    let st = fx.call_sync("NtReadFile", &[h, 0, 0, 0, iosb, buf, 4096, 0]);
    assert_eq!((st, fx.bytes(buf, 4)), (STATUS_SUCCESS, b"XBEH".to_vec()));
    let size = fx.query(h, file_class::STANDARD, 0x18).unwrap();
    assert!(FileStandardInformation::from_bytes(&size).unwrap().end_of_file > 4096);
    let root = fx.open(r"\Device\CdRom0", READ, FILE_OPEN, SYNC | options::FILE_DIRECTORY_FILE);
    let (names, st) = fx.list(root, None, true);
    assert_eq!(st, NtStatus::NO_MORE_FILES.0);
    assert!(names.iter().any(|n| n.eq_ignore_ascii_case("default.xbe")), "{names:?}");
    println!("the_real_disc: {} entries in the root", names.len());
}
