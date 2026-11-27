//! Tests of the namespace and the volumes together. The hard disk rules run
//! against both the host folder volume and the in-memory one, which must
//! agree.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::ioctl::{Control, FSCTL_DISMOUNT_VOLUME, IOCTL_DISK_GET_PARTITION_INFO, partition_info};
use crate::types::{
    DISC_TIME, FILE_USE_FILE_POINTER_POSITION, FILE_WRITE_TO_END_OF_FILE, FileBasicInformation,
    FileFsAttributeInformation, FileFsDeviceInformation, FileFsSizeInformation,
    FileFsVolumeInformation, FileNetworkOpenInformation, FileStandardInformation, access,
    attributes, file_class, filetime_from_unix, fs_class, options, share,
};
use crate::*;

/// A folder under the system's temporary directory, removed when dropped.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static SERIAL: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "xb-fs-test-{}-{}-{tag}",
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

const RW: u32 = access::GENERIC_READ | access::GENERIC_WRITE;
const SYNC: u32 = options::FILE_SYNCHRONOUS_IO_NONALERT;
const E: &str = r"\Device\Harddisk0\Partition1";
const Z: &str = r"\Device\Harddisk0\Partition5";

fn host_fs(tag: &str) -> (TempDir, FileSystem) {
    let dir = TempDir::new(tag);
    let fs = FileSystem::with_hard_disk(dir.path()).unwrap();
    (dir, fs)
}

fn mem_fs() -> FileSystem {
    let fs = FileSystem::new();
    for n in 1..=5 {
        fs.mount(Device::Partition(n), Arc::new(MemVolume::new()));
    }
    fs
}

/// Runs `test` on a host-folder hard disk and on an in-memory one.
fn on_both(tag: &str, test: impl Fn(&FileSystem)) {
    let (_dir, fs) = host_fs(tag);
    test(&fs);
    test(&mem_fs());
}

fn open(
    fs: &FileSystem,
    name: &str,
    desired_access: u32,
    disposition: CreateDisposition,
    opts: u32,
) -> Result<(File, CreateAction), NtStatus> {
    fs.open(Root::None, name, &OpenParams::create(desired_access, share::ALL, disposition, opts))
        .map_err(|e| e.status)
}

fn put(fs: &FileSystem, name: &str, bytes: &[u8]) {
    let (f, _) = open(fs, name, RW, CreateDisposition::OverwriteIf, SYNC).unwrap();
    assert_eq!(f.write(0, bytes), Ok(bytes.len()));
}

fn get(fs: &FileSystem, name: &str) -> Result<Vec<u8>, NtStatus> {
    let (f, _) = open(fs, name, access::GENERIC_READ, CreateDisposition::Open, SYNC)?;
    let mut buf = vec![0; f.size() as usize + 16];
    let n = if f.size() == 0 { 0 } else { f.read(0, &mut buf)? };
    buf.truncate(n);
    Ok(buf)
}

fn mkdir(fs: &FileSystem, name: &str) {
    let (d, _) =
        open(fs, name, 0, CreateDisposition::OpenIf, options::FILE_DIRECTORY_FILE | SYNC).unwrap();
    assert!(d.is_dir());
}

fn open_dir(fs: &FileSystem, name: &str) -> File {
    open(
        fs,
        name,
        access::GENERIC_READ,
        CreateDisposition::Open,
        options::FILE_DIRECTORY_FILE | SYNC,
    )
    .unwrap()
    .0
}

fn names(dir: &File, mask: Option<&str>) -> Vec<String> {
    let mut out = Vec::new();
    let mut restart = true;
    loop {
        match dir.query_directory(if restart { mask } else { None }, restart, 0x200) {
            Ok(info) => out.push(info.file_name),
            Err(NtStatus::NO_MORE_FILES | NtStatus::NO_SUCH_FILE) => break,
            Err(e) => panic!("{e}"),
        }
        restart = false;
    }
    out.sort();
    out
}

#[test]
fn open_files_can_cross_threads() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<File>();
    assert_send_sync::<FileSystem>();
    assert_send_sync::<HostDisk>();
}

#[test]
fn names_resolve_through_links_and_roots() {
    on_both("links", |fs| {
        mkdir(fs, r"\Device\Harddisk0\partition1\TDATA");
        mkdir(fs, r"\Device\Harddisk0\partition1\TDATA\4d530004");
        assert_eq!(
            fs.create_symbolic_link(r"\??\T:", r"\Device\Harddisk0\partition1\TDATA\4d530004"),
            NtStatus::SUCCESS
        );
        put(fs, r"\??\T:\settings.bin", b"abc");
        // XAPI's form: "T:\x" under the \?? directory, in any case.
        let (f, action) = fs
            .open(
                Root::DosDevices,
                r"t:\SETTINGS.BIN",
                &OpenParams::open(access::GENERIC_READ, 1, SYNC),
            )
            .unwrap();
        assert_eq!(action, CreateAction::Opened);
        assert_eq!(f.path(), r"\Device\Harddisk0\partition1\TDATA\4d530004\SETTINGS.BIN");
        assert_eq!(f.device(), Device::Partition(1));
        // Relative to an open directory.
        let dir = open_dir(fs, r"\??\T:\");
        let (g, _) = fs
            .open(
                Root::File(&dir),
                "settings.bin",
                &OpenParams::open(access::GENERIC_READ, 1, SYNC),
            )
            .unwrap();
        assert_eq!(g.path(), r"\Device\Harddisk0\partition1\TDATA\4d530004\settings.bin");
        assert_eq!(
            get(fs, r"\Device\Harddisk0\Partition1\tdata\4D530004\Settings.Bin").unwrap(),
            b"abc"
        );
        // A link to a link's path, and the link gone again.
        assert_eq!(fs.create_symbolic_link(r"\??\Q:", r"\??\T:\.."), NtStatus::SUCCESS);
        assert_eq!(
            fs.symbolic_link_target(r"\??\q:").as_deref(),
            Some(r"\Device\Harddisk0\partition1\TDATA")
        );
        assert_eq!(fs.delete_symbolic_link(r"\??\Q:"), NtStatus::SUCCESS);
        assert_eq!(fs.delete_symbolic_link(r"\??\Q:"), NtStatus::OBJECT_NAME_NOT_FOUND);
        let e = fs.open(Root::DosDevices, r"Q:\x", &OpenParams::open(0, 7, 0)).unwrap_err();
        assert_eq!(e, OpenError { status: NtStatus::OBJECT_PATH_NOT_FOUND, information: 0 });
        assert_eq!(
            fs.create_symbolic_link(r"\??\R:", r"\??\Q:\x"),
            NtStatus::OBJECT_PATH_NOT_FOUND
        );
    });
}

#[test]
fn the_boot_links_point_at_the_disc() {
    let fs = FileSystem::new();
    assert_eq!(fs.symbolic_link_target(r"\??\D:").as_deref(), Some(r"\Device\CdRom0"));
    assert_eq!(fs.symbolic_link_target(r"\??\CDROM0:").as_deref(), Some(r"\Device\CdRom0"));
    assert_eq!(fs.symbolic_links().len(), 2);
    fs.set_launch_dir(r"\Device\Harddisk0\Partition2\apps");
    assert_eq!(
        fs.canonicalize(Root::DosDevices, r"D:\default.xbe").as_deref(),
        Some(r"\Device\Harddisk0\Partition2\apps\default.xbe")
    );
}

#[test]
fn objects_on_no_device_are_missing_names_or_missing_paths() {
    let fs = mem_fs();
    let status = |name| fs.open(Root::None, name, &OpenParams::open(0, 7, 0)).unwrap_err();
    assert_eq!(
        status(r"\??\x.dds"),
        OpenError { status: NtStatus::OBJECT_NAME_NOT_FOUND, information: 5 }
    );
    assert_eq!(status(r"\Device\Floppy0").status, NtStatus::OBJECT_NAME_NOT_FOUND);
    assert_eq!(status(r"\Device\Floppy0\x").status, NtStatus::OBJECT_PATH_NOT_FOUND);
    // Nothing mounted there: the tray is empty; a partition that is not there is no device.
    assert_eq!(status(r"\Device\CdRom0\default.xbe").status, NtStatus::NO_MEDIA_IN_DEVICE);
    assert_eq!(status(r"\Device\Harddisk0\Partition7\x").status, NtStatus::OBJECT_PATH_NOT_FOUND);
    assert_eq!(status(r"\Device\Harddisk0\Partition7").status, NtStatus::OBJECT_PATH_NOT_FOUND);
}

#[test]
fn existing_host_files_open_in_any_case() {
    let (dir, fs) = host_fs("case");
    let save = dir.path().join("Partition1/UDATA/4D530004/122A17771B9E");
    std::fs::create_dir_all(&save).unwrap();
    std::fs::write(save.join("SaveMeta.XBX"), b"meta").unwrap();
    for name in [
        r"\Device\Harddisk0\Partition1\UDATA\4d530004\122A17771B9E\SaveMeta.xbx",
        r"\Device\Harddisk0\partition1\udata\4D530004\122a17771b9e\SAVEMETA.XBX",
    ] {
        assert_eq!(get(&fs, name).unwrap(), b"meta");
    }
    // A new file beside it keeps the title's spelling, in the existing folder.
    put(&fs, r"\Device\Harddisk0\Partition1\udata\4d530004\New.Bin", b"n");
    assert!(dir.path().join("Partition1/UDATA/4D530004/New.Bin").is_file());
    assert_eq!(
        fs.host_file(r"\Device\Harddisk0\Partition1\UDATA\4D530004\new.bin"),
        Some(dir.path().join("Partition1/UDATA/4D530004/New.Bin"))
    );
    assert_eq!(fs.host_file(r"\Device\Harddisk0\Partition1\UDATA"), None);
    assert_eq!(fs.host_file(r"\Device\Harddisk0\Partition1\nothing.xbe"), None);
}

#[test]
fn names_windows_cannot_hold_are_escaped_on_the_host() {
    let (dir, fs) = host_fs("escape");
    mkdir(&fs, &format!(r"{E}\ "));
    put(&fs, &format!(r"{E}\ \con"), b"1");
    put(&fs, &format!(r"{E}\trail."), b"2");
    assert!(dir.path().join("Partition1/=20/=63on").is_file());
    assert!(dir.path().join("Partition1/trail=2E").is_file());
    assert_eq!(get(&fs, &format!(r"{E}\ \CON")).unwrap(), b"1");
    assert_eq!(names(&open_dir(&fs, E), None), [" ", "trail."]);
    assert_eq!(names(&open_dir(&fs, &format!(r"{E}\ ")), None), ["con"]);
}

#[test]
fn every_create_disposition_reports_what_it_did() {
    use CreateAction::{Created, Opened, Overwritten, Superseded};
    use CreateDisposition::{Create, Open, OpenIf, Overwrite, OverwriteIf, Supersede};
    on_both("dispositions", |fs| {
        let table = [
            (Supersede, Ok(Superseded), Ok(Created)),
            (Open, Ok(Opened), Err(NtStatus::OBJECT_NAME_NOT_FOUND)),
            (Create, Err(NtStatus::OBJECT_NAME_COLLISION), Ok(Created)),
            (OpenIf, Ok(Opened), Ok(Created)),
            (Overwrite, Ok(Overwritten), Err(NtStatus::OBJECT_NAME_NOT_FOUND)),
            (OverwriteIf, Ok(Overwritten), Ok(Created)),
        ];
        for (i, (disposition, existing, missing)) in table.into_iter().enumerate() {
            let there = format!(r"{E}\there{i}.bin");
            let absent = format!(r"{E}\absent{i}.bin");
            put(fs, &there, b"old contents");
            let on_existing = open(fs, &there, RW, disposition, SYNC).map(|(f, a)| (f.size(), a));
            assert_eq!(on_existing.map(|(_, a)| a), existing, "{disposition:?} on a file");
            if let Ok((size, action)) = on_existing {
                assert_eq!(size, if action == Opened { 12 } else { 0 }, "{disposition:?}");
            }
            let on_missing = open(fs, &absent, RW, disposition, SYNC).map(|(_, a)| a);
            assert_eq!(on_missing, missing, "{disposition:?} on nothing");
            assert_eq!(get(fs, &absent).is_ok(), missing.is_ok());
        }
        // The information a failed open leaves.
        let e = fs.open(Root::None, &format!(r"{E}\nope"), &OpenParams::open(0, 7, 0)).unwrap_err();
        assert_eq!(e.information, CreateAction::DoesNotExist as u32);
        // A missing parent is a missing path, whatever the disposition.
        for (disposition, ..) in table {
            assert_eq!(
                open(fs, &format!(r"{E}\no\such\file"), RW, disposition, SYNC).map(|(_, a)| a),
                Err(NtStatus::OBJECT_PATH_NOT_FOUND)
            );
        }
    });
}

#[test]
fn directory_options_must_fit_what_the_path_is() {
    use CreateDisposition::{Create, Open, OpenIf, OverwriteIf};
    on_both("dirs", |fs| {
        let dir = format!(r"{E}\saved");
        let file = format!(r"{E}\saved\a.bin");
        let d = options::FILE_DIRECTORY_FILE;
        let nd = options::FILE_NON_DIRECTORY_FILE;
        assert_eq!(
            open(fs, &dir, 0, Open, d).map(|(_, a)| a),
            Err(NtStatus::OBJECT_NAME_NOT_FOUND)
        );
        assert_eq!(
            open(fs, &dir, 0, Create, d).map(|(f, a)| (f.is_dir(), a)),
            Ok((true, CreateAction::Created))
        );
        assert_eq!(
            open(fs, &dir, 0, Create, d).map(|(_, a)| a),
            Err(NtStatus::OBJECT_NAME_COLLISION)
        );
        assert_eq!(open(fs, &dir, 0, OpenIf, d).map(|(_, a)| a), Ok(CreateAction::Opened));
        assert_eq!(open(fs, &dir, 0, Open, nd).map(|(_, a)| a), Err(NtStatus::FILE_IS_A_DIRECTORY));
        // An overwrite disposition on a directory just opens it.
        assert_eq!(
            open(fs, &dir, RW, OverwriteIf, 0).map(|(f, a)| (f.is_dir(), a)),
            Ok((true, CreateAction::Opened))
        );
        put(fs, &file, b"x");
        assert_eq!(open(fs, &file, 0, Open, d).map(|(_, a)| a), Err(NtStatus::NOT_A_DIRECTORY));
        assert_eq!(open(fs, &file, 0, Open, nd).map(|(_, a)| a), Ok(CreateAction::Opened));
        // The partition's root is a directory that always exists.
        let (root, action) = open(fs, E, 0, OpenIf, d).unwrap();
        assert!(root.is_dir() && !root.is_volume());
        assert_eq!(action, CreateAction::Opened);
        assert_eq!(open(fs, E, 0, Create, d).map(|(_, a)| a), Err(NtStatus::OBJECT_NAME_COLLISION));
        // A directory reads and writes as an invalid request and a refusal.
        let (dirh, _) = open(fs, &dir, RW, Open, 0).unwrap();
        assert_eq!(dirh.read(0, &mut [0; 4]), Err(NtStatus::INVALID_DEVICE_REQUEST));
        assert_eq!(dirh.write(0, b"x"), Err(NtStatus::ACCESS_DENIED));
        let (fh, _) = open(fs, &file, RW, Open, 0).unwrap();
        assert_eq!(fh.query_directory(None, true, 0x100), Err(NtStatus::INVALID_DEVICE_REQUEST));
    });
}

#[test]
fn reads_and_writes_follow_offsets_sizes_and_access() {
    on_both("rw", |fs| {
        let name = format!(r"{E}\data.bin");
        let (f, _) = open(fs, &name, RW, CreateDisposition::Create, SYNC).unwrap();
        assert_eq!(f.write(f.write_offset(None), b"hello"), Ok(5));
        f.advance(None, 5);
        assert_eq!(f.write_offset(Some(FILE_USE_FILE_POINTER_POSITION)), 5);
        assert_eq!(f.write(f.write_offset(Some(FILE_WRITE_TO_END_OF_FILE)), b" world"), Ok(6));
        assert_eq!(f.size(), 11);
        assert!(f.wrote());
        let mut buf = [0u8; 32];
        assert_eq!(f.read(0, &mut buf), Ok(11));
        assert_eq!(&buf[..11], b"hello world");
        assert_eq!(f.read(6, &mut buf[..3]), Ok(3));
        assert_eq!(&buf[..3], b"wor");
        assert_eq!(f.read(11, &mut buf), Err(NtStatus::END_OF_FILE));
        assert_eq!(f.read(500, &mut buf), Err(NtStatus::END_OF_FILE));
        // Zero bytes succeed, past the end too.
        assert_eq!(f.read(500, &mut []), Ok(0));
        // A write past the end leaves a gap of zeros.
        assert_eq!(f.write(14, b"!"), Ok(1));
        assert_eq!(f.read(9, &mut buf), Ok(6));
        assert_eq!(&buf[..6], b"ld\0\0\0!");
        // A handle opened to read refuses writes and size changes, and the file is as it was.
        let (r, _) = open(fs, &name, access::GENERIC_READ, CreateDisposition::Open, SYNC).unwrap();
        assert!(!r.can_write());
        assert_eq!(r.write(0, b"X"), Err(NtStatus::ACCESS_DENIED));
        assert_eq!(
            r.set_information(file_class::END_OF_FILE, &4u64.to_le_bytes()),
            NtStatus::ACCESS_DENIED
        );
        assert_eq!(
            r.set_information(file_class::ALLOCATION, &4u64.to_le_bytes()),
            NtStatus::ACCESS_DENIED
        );
        assert_eq!(r.size(), 15);
        assert_eq!(
            f.set_information(file_class::END_OF_FILE, &4u64.to_le_bytes()),
            NtStatus::SUCCESS
        );
        assert_eq!(
            f.set_information(file_class::ALLOCATION, &4u64.to_le_bytes()),
            NtStatus::SUCCESS
        );
        assert_eq!(get(fs, &name).unwrap(), b"hell");
        assert_eq!(f.flush(), NtStatus::SUCCESS);
    });
}

#[test]
fn the_position_moves_for_synchronous_handles_and_unnamed_offsets() {
    on_both("position", |fs| {
        let name = format!(r"{E}\pos.bin");
        put(fs, &name, &[1; 64]);
        let (sync, _) = open(fs, &name, RW, CreateDisposition::Open, SYNC).unwrap();
        let (overlapped, _) = open(fs, &name, RW, CreateDisposition::Open, 0).unwrap();
        assert!(!sync.is_async() && overlapped.is_async());
        assert_eq!(sync.read_offset(Some(16)), 16);
        sync.advance(Some(16), 24);
        assert_eq!(sync.position(), 24);
        assert_eq!(sync.read_offset(None), 24);
        // Burnout 2: an overlapped read that names its offset leaves the position alone.
        overlapped.advance(Some(16), 24);
        assert_eq!(overlapped.position(), 0);
        overlapped.advance(Some(FILE_USE_FILE_POINTER_POSITION), 8);
        assert_eq!(overlapped.position(), 8);
        overlapped.advance(None, 12);
        assert_eq!(overlapped.position(), 12);
        assert_eq!(
            sync.set_information(file_class::POSITION, &40u64.to_le_bytes()),
            NtStatus::SUCCESS
        );
        assert_eq!(
            sync.query_information(file_class::POSITION, 8),
            Ok(40u64.to_le_bytes().to_vec())
        );
        assert_eq!(
            sync.set_information(file_class::POSITION, &[0; 4]),
            NtStatus::INVALID_PARAMETER
        );
    });
}

#[test]
fn an_overwritten_file_gets_its_old_bytes_back_as_it_grows() {
    on_both("regrow", |fs| {
        let name = format!(r"{Z}\cab.bin");
        put(fs, &name, b"0123456789");
        let (f, action) = open(fs, &name, RW, CreateDisposition::OverwriteIf, SYNC).unwrap();
        assert_eq!(action, CreateAction::Overwritten);
        assert_eq!(f.size(), 0);
        assert_eq!(get(fs, &name).unwrap(), b"");
        // Forza: SetEndOfFile brings back what the clusters held.
        assert_eq!(
            f.set_information(file_class::END_OF_FILE, &6u64.to_le_bytes()),
            NtStatus::SUCCESS
        );
        assert_eq!(get(fs, &name).unwrap(), b"012345");
        // A write past the end: the gap is old bytes, then zeros past the old size.
        assert_eq!(f.write(12, b"Z"), Ok(1));
        assert_eq!(get(fs, &name).unwrap(), b"0123456789\0\0Z");
        drop(f);
        // A later handle starts clean: nothing of the old file comes back.
        let (g, _) = open(fs, &name, RW, CreateDisposition::Supersede, SYNC).unwrap();
        assert_eq!(
            g.set_information(file_class::END_OF_FILE, &3u64.to_le_bytes()),
            NtStatus::SUCCESS
        );
        assert_eq!(get(fs, &name).unwrap(), b"012");
        drop(g);
        let (h, _) = open(fs, &name, RW, CreateDisposition::Open, SYNC).unwrap();
        assert_eq!(
            h.set_information(file_class::END_OF_FILE, &5u64.to_le_bytes()),
            NtStatus::SUCCESS
        );
        assert_eq!(get(fs, &name).unwrap(), b"012\0\0");
    });
}

#[test]
fn the_side_file_of_an_overwrite_goes_when_the_handle_closes() {
    let (dir, fs) = host_fs("side");
    let name = format!(r"{Z}\x.bin");
    put(&fs, &name, b"old");
    let (f, _) = open(&fs, &name, RW, CreateDisposition::OverwriteIf, SYNC).unwrap();
    let side = dir.path().join(".overwrite");
    assert_eq!(std::fs::read_dir(&side).unwrap().count(), 1);
    drop(f);
    assert_eq!(std::fs::read_dir(&side).unwrap().count(), 0);
    assert!(dir.path().join("Partition5/x.bin").is_file());
}

#[test]
fn share_access_is_checked_between_handles() {
    on_both("share", |fs| {
        let name = format!(r"{Z}\level.bin");
        put(fs, &name, b"copying");
        let params = |desired_access, share_access| {
            OpenParams::create(desired_access, share_access, CreateDisposition::Open, SYNC)
        };
        // Mission: Impossible: the copy's file is held open unshared.
        let (copy, _) = fs.open(Root::None, &name, &params(RW, 0)).unwrap();
        let status = |p| fs.open(Root::None, &name, &p).map(|_| ()).map_err(|e| e.status);
        assert_eq!(
            status(params(access::GENERIC_READ, share::ALL)),
            Err(NtStatus::SHARING_VIOLATION)
        );
        // Attributes only: no part in sharing.
        assert_eq!(status(params(access::SYNCHRONIZE, 0)), Ok(()));
        assert!(fs.query_full_attributes(Root::None, &name).is_ok());
        drop(copy);
        let (reader, _) = fs
            .open(Root::None, &name, &params(access::GENERIC_READ, share::FILE_SHARE_READ))
            .unwrap();
        assert_eq!(status(params(access::GENERIC_READ, share::FILE_SHARE_READ)), Ok(()));
        // It wants to write, which the reader does not share; or it would not let the reader read.
        assert_eq!(status(params(RW, share::ALL)), Err(NtStatus::SHARING_VIOLATION));
        assert_eq!(
            status(params(access::GENERIC_READ, share::FILE_SHARE_WRITE)),
            Err(NtStatus::SHARING_VIOLATION)
        );
        drop(reader);
        assert_eq!(status(params(RW, 0)), Ok(()));
    });
}

#[test]
fn directories_enumerate_with_masks_and_restart() {
    on_both("enum", |fs| {
        let dir = format!(r"{E}\save");
        mkdir(fs, &dir);
        for name in ["_filler001", "_filler002", "_filler0030", "SaveMeta.xbx", "blam.sav", "noext"]
        {
            put(fs, &format!(r"{dir}\{name}"), &[0; 5000]);
        }
        mkdir(fs, &format!(r"{dir}\sub"));
        let d = open_dir(fs, &dir);
        assert_eq!(
            names(&d, None),
            ["SaveMeta.xbx", "_filler001", "_filler002", "_filler0030", "blam.sav", "noext", "sub"]
        );
        assert_eq!(names(&d, Some("")), names(&d, Some("*")));
        assert_eq!(names(&d, Some("_filler???")), ["_filler001", "_filler002"]);
        assert_eq!(names(&d, Some("*.*")), ["SaveMeta.xbx", "blam.sav"]);
        assert_eq!(names(&d, Some("BLAM.SAV")), ["blam.sav"]);
        assert_eq!(names(&d, Some("*.map")), Vec::<String>::new());

        // The first call that matches nothing says so; after matches, no more files.
        assert_eq!(d.query_directory(Some("*.map"), true, 0x100), Err(NtStatus::NO_SUCH_FILE));
        assert_eq!(d.query_directory(Some("*.map"), false, 0x100), Err(NtStatus::NO_SUCH_FILE));
        let first = d.query_directory(Some("*.sav"), false, 0x100).unwrap();
        assert_eq!(first.file_name, "blam.sav");
        assert_eq!(first.end_of_file, 5000);
        assert_eq!(first.allocation_size, 8192);
        assert_eq!(first.file_attributes, attributes::FILE_ATTRIBUTE_NORMAL);
        assert_eq!(first.creation_time, DISC_TIME);
        assert_eq!(d.query_directory(None, false, 0x100), Err(NtStatus::NO_MORE_FILES));
        assert_eq!(d.query_directory(None, false, 0x100), Err(NtStatus::NO_MORE_FILES));
        // The same mask again goes on; restart starts over.
        assert_eq!(d.query_directory(Some("*.sav"), false, 0x100), Err(NtStatus::NO_MORE_FILES));
        assert_eq!(d.query_directory(Some("*.sav"), true, 0x100).unwrap().file_name, "blam.sav");
        // A different mask starts a new enumeration without restart.
        let sub = d.query_directory(Some("sub"), false, 0x100).unwrap();
        assert_eq!(sub.file_attributes, attributes::FILE_ATTRIBUTE_DIRECTORY);
        assert_eq!(sub.end_of_file, 0);

        // The mask of the first call stays for calls without one (Riddick's fillers).
        let mut seen = vec![d.query_directory(Some("_filler???"), true, 0x100).unwrap().file_name];
        while let Ok(info) = d.query_directory(None, false, 0x100) {
            seen.push(info.file_name);
        }
        seen.sort();
        assert_eq!(seen, ["_filler001", "_filler002"]);

        // An entry that does not fit is reported so and passed over.
        let fresh = open_dir(fs, &dir);
        assert_eq!(
            fresh.query_directory(Some("_filler00?"), true, 0x40 + 9),
            Err(NtStatus::BUFFER_TOO_SMALL)
        );
        assert!(fresh.query_directory(None, false, 0x40 + 10).is_ok());
        assert_eq!(fresh.query_directory(None, false, 0x40 + 10), Err(NtStatus::NO_MORE_FILES));
    });
}

#[test]
fn an_enumeration_works_on_a_snapshot_while_files_are_deleted() {
    on_both("snapshot", |fs| {
        let dir = format!(r"{E}\UDATA");
        mkdir(fs, &dir);
        for i in 0..10 {
            put(fs, &format!(r"{dir}\file{i}"), b"x");
        }
        // XDeleteSaveGame: delete each file as the listing returns it.
        let d = open_dir(fs, &dir);
        let mut restart = true;
        let mut deleted = 0;
        while let Ok(info) = d.query_directory(None, restart, 0x100) {
            restart = false;
            assert_eq!(fs.delete_file(Root::File(&d), &info.file_name), NtStatus::SUCCESS);
            deleted += 1;
        }
        assert_eq!(deleted, 10);
        assert!(names(&d, None).is_empty());
        // The emptied directory can go; one with entries cannot.
        put(fs, &format!(r"{dir}\last"), b"x");
        assert_eq!(d.set_information(file_class::DISPOSITION, &[1]), NtStatus::DIRECTORY_NOT_EMPTY);
        assert_eq!(fs.delete_file(Root::None, &dir), NtStatus::ACCESS_DENIED);
        assert_eq!(fs.delete_file(Root::None, &format!(r"{dir}\last")), NtStatus::SUCCESS);
        assert_eq!(d.set_information(file_class::DISPOSITION, &[1]), NtStatus::SUCCESS);
        assert_eq!(get(fs, &dir).map(|_| ()), Err(NtStatus::OBJECT_NAME_NOT_FOUND));
    });
}

#[test]
fn a_deleted_file_stays_until_its_last_handle_closes() {
    on_both("delete", |fs| {
        let name = format!(r"{Z}\boot.baf");
        put(fs, &name, b"baf");
        let open_it = || open(fs, &name, RW | access::DELETE, CreateDisposition::Open, SYNC);
        let (a, _) = open_it().unwrap();
        let (b, _) = open_it().unwrap();
        // Sudeki: deleted through one handle with another still open.
        assert_eq!(a.set_information(file_class::DISPOSITION, &[1]), NtStatus::SUCCESS);
        assert!(a.delete_pending() && b.delete_pending());
        assert_eq!(open_it().map(|_| ()), Err(NtStatus::DELETE_PENDING));
        assert_eq!(
            open(fs, &name, RW, CreateDisposition::OverwriteIf, SYNC).map(|_| ()),
            Err(NtStatus::DELETE_PENDING)
        );
        drop(a);
        // Still there, still readable through the other handle.
        let mut buf = [0; 3];
        assert_eq!(b.read(0, &mut buf), Ok(3));
        assert_eq!(names(&open_dir(fs, Z), None), ["boot.baf"]);
        drop(b);
        assert_eq!(open_it().map(|_| ()), Err(NtStatus::OBJECT_NAME_NOT_FOUND));
        assert!(names(&open_dir(fs, Z), None).is_empty());

        // The mark can be taken back.
        put(fs, &name, b"baf");
        let (c, _) = open_it().unwrap();
        assert_eq!(c.set_information(file_class::DISPOSITION, &[1]), NtStatus::SUCCESS);
        assert_eq!(c.set_information(file_class::DISPOSITION, &[0]), NtStatus::SUCCESS);
        drop(c);
        assert_eq!(get(fs, &name).unwrap(), b"baf");

        // NtDeleteFile: at once with no handle open, at the last close with one.
        let (d, _) = open_it().unwrap();
        assert_eq!(fs.delete_file(Root::None, &name), NtStatus::SUCCESS);
        assert_eq!(open_it().map(|_| ()), Err(NtStatus::DELETE_PENDING));
        drop(d);
        assert_eq!(fs.delete_file(Root::None, &name), NtStatus::OBJECT_NAME_NOT_FOUND);
        put(fs, &name, b"baf");
        assert_eq!(fs.delete_file(Root::DosDevices, r"Z:\boot.baf"), NtStatus::ACCESS_DENIED);
        fs.create_symbolic_link(r"\??\Z:", Z);
        assert_eq!(fs.delete_file(Root::DosDevices, r"Z:\BOOT.BAF"), NtStatus::SUCCESS);
        assert_eq!(get(fs, &name).map(|_| ()), Err(NtStatus::OBJECT_NAME_NOT_FOUND));
    });
}

#[test]
fn files_and_directories_rename() {
    on_both("rename", |fs| {
        fs.create_symbolic_link(r"\??\Z:", Z);
        mkdir(fs, &format!(r"{Z}\cache"));
        put(fs, &format!(r"{Z}\cache\level.tmp"), b"level");
        put(fs, &format!(r"{Z}\cache\other"), b"other");
        let (f, _) =
            open(fs, &format!(r"{Z}\cache\level.tmp"), RW, CreateDisposition::Open, SYNC).unwrap();
        // Riddick: MoveFile passes "Z:\x" under \??.
        assert_eq!(
            fs.rename(&f, Root::DosDevices, r"Z:\cache\other", false),
            NtStatus::OBJECT_NAME_COLLISION
        );
        assert_eq!(
            fs.rename(&f, Root::DosDevices, r"Z:\nowhere\level", false),
            NtStatus::OBJECT_PATH_NOT_FOUND
        );
        assert_eq!(
            fs.rename(&f, Root::DosDevices, r"Q:\level", false),
            NtStatus::OBJECT_PATH_NOT_FOUND
        );
        assert_eq!(
            fs.rename(&f, Root::None, r"\Device\CdRom0\level", false),
            NtStatus::OBJECT_PATH_NOT_FOUND
        );
        assert_eq!(f.path(), format!(r"{Z}\cache\level.tmp"));
        assert_eq!(fs.rename(&f, Root::DosDevices, r"Z:\cache\level", false), NtStatus::SUCCESS);
        assert_eq!(f.path(), format!(r"{Z}\cache\level"));
        assert_eq!(
            get(fs, &format!(r"{Z}\cache\level.tmp")).map(|_| ()),
            Err(NtStatus::OBJECT_NAME_NOT_FOUND)
        );
        // The open handle follows the file.
        assert_eq!(f.write(5, b"!"), Ok(1));
        assert_eq!(get(fs, &format!(r"{Z}\cache\LEVEL")).unwrap(), b"level!");
        // Over an existing file, when asked.
        assert_eq!(fs.rename(&f, Root::DosDevices, r"Z:\cache\other", true), NtStatus::SUCCESS);
        assert_eq!(names(&open_dir(fs, &format!(r"{Z}\cache")), None), ["other"]);
        // Delete-on-close follows the new name.
        assert_eq!(f.set_information(file_class::DISPOSITION, &[1]), NtStatus::SUCCESS);
        drop(f);
        assert!(names(&open_dir(fs, &format!(r"{Z}\cache")), None).is_empty());

        // A directory, relative to its parent's handle.
        put(fs, &format!(r"{Z}\cache\inside"), b"in");
        let root = open_dir(fs, Z);
        let (d, _) = open(
            fs,
            &format!(r"{Z}\cache"),
            RW,
            CreateDisposition::Open,
            options::FILE_DIRECTORY_FILE,
        )
        .unwrap();
        assert_eq!(fs.rename(&d, Root::File(&root), "done", false), NtStatus::SUCCESS);
        assert_eq!(d.path(), format!(r"{Z}\done"));
        assert_eq!(names(&d, None), ["inside"]);
        assert_eq!(get(fs, &format!(r"{Z}\done\inside")).unwrap(), b"in");
    });
}

#[test]
fn a_rename_can_cross_partitions_of_one_disk() {
    let (dir, fs) = host_fs("cross");
    put(&fs, &format!(r"{Z}\a.bin"), b"a");
    let (f, _) = open(&fs, &format!(r"{Z}\a.bin"), RW, CreateDisposition::Open, SYNC).unwrap();
    assert_eq!(fs.rename(&f, Root::None, &format!(r"{E}\b.bin"), false), NtStatus::SUCCESS);
    assert_eq!(f.device(), Device::Partition(1));
    assert!(dir.path().join("Partition1/b.bin").is_file());
    // Now on a partition that holds saves: synced before it closes, once written.
    assert!(!f.sync_on_close());
    assert_eq!(f.write(0, b"b"), Ok(1));
    assert!(f.sync_on_close());
    let (z, _) = open(&fs, &format!(r"{Z}\z.bin"), RW, CreateDisposition::Create, SYNC).unwrap();
    assert_eq!(z.write(0, b"z"), Ok(1));
    assert!(z.wrote() && !z.sync_on_close());
}

#[test]
fn information_classes_have_their_guest_layouts() {
    on_both("info", |fs| {
        let name = format!(r"{E}\info.bin");
        put(fs, &name, &[9; 5000]);
        let (f, _) = open(fs, &name, RW, CreateDisposition::Open, SYNC).unwrap();

        let basic = FileBasicInformation::from_bytes(
            &f.query_information(file_class::BASIC, 0x28).unwrap(),
        )
        .unwrap();
        assert_eq!(basic.file_attributes, attributes::FILE_ATTRIBUTE_NORMAL);
        assert_eq!(basic.last_write_time, basic.change_time);
        assert_eq!(basic.last_write_time % 10_000_000, 0);
        assert!(basic.last_write_time > DISC_TIME);

        let std_bytes = f.query_information(file_class::STANDARD, 0x18).unwrap();
        assert_eq!(std_bytes.len(), 0x18);
        assert_eq!(
            FileStandardInformation::from_bytes(&std_bytes).unwrap(),
            FileStandardInformation {
                allocation_size: 8192,
                end_of_file: 5000,
                number_of_links: 1,
                delete_pending: false,
                directory: false
            }
        );

        let net = FileNetworkOpenInformation::from_bytes(
            &f.query_information(file_class::NETWORK_OPEN, 0x38).unwrap(),
        )
        .unwrap();
        assert_eq!((net.end_of_file, net.allocation_size), (5000, 8192));
        assert_eq!(net.last_write_time, basic.last_write_time);
        assert_eq!(fs.query_full_attributes(Root::None, &name), Ok(net));
        assert_eq!(f.network_open_information(), net);

        let index = f.query_information(file_class::INTERNAL, 8).unwrap();
        assert_ne!(index, [0; 8]);
        let (again, _) =
            open(fs, &name, access::GENERIC_READ, CreateDisposition::Open, SYNC).unwrap();
        assert_eq!(again.query_information(file_class::INTERNAL, 8).unwrap(), index);
        assert_eq!(f.sort_key(), again.sort_key());
        assert!(matches!(f.sort_key(), SortKey::Fatx { .. }));
        assert_eq!(f.query_information(file_class::ALIGNMENT, 4), Ok(vec![0; 4]));

        // Too small a buffer, and classes not answered.
        for (class, size) in [(4, 0x28), (5, 0x18), (6, 8), (14, 8), (17, 4), (34, 0x38)] {
            assert_eq!(
                f.query_information(class, size - 1),
                Err(NtStatus::BUFFER_TOO_SMALL),
                "class {class}"
            );
            assert_eq!(
                f.query_information(class, size).map(|b| b.len()),
                Ok(size),
                "class {class}"
            );
        }
        assert_eq!(f.query_information(9, 0x100), Err(NtStatus::INVALID_INFO_CLASS));
        assert_eq!(f.set_information(file_class::RENAME, &[0; 16]), NtStatus::INVALID_INFO_CLASS);
        assert_eq!(
            f.set_information(file_class::COMPLETION, &[0; 8]),
            NtStatus::INVALID_INFO_CLASS
        );

        // Forza stamps each copy with its source's time and compares within 4 s.
        let stamp = filetime_from_unix(1_000_000_000, 0);
        let set = FileBasicInformation { last_write_time: stamp, ..Default::default() };
        assert_eq!(f.set_information(file_class::BASIC, &set.to_bytes()), NtStatus::SUCCESS);
        let after = f.basic_information();
        assert_eq!(after.last_write_time, stamp);
        assert_eq!(after.last_access_time, basic.last_access_time);
        // 0 and -1 keep a time.
        let keep = FileBasicInformation { last_write_time: u64::MAX, ..Default::default() };
        assert_eq!(f.set_information(file_class::BASIC, &keep.to_bytes()), NtStatus::SUCCESS);
        assert_eq!(f.basic_information().last_write_time, stamp);

        // A directory.
        let d = open_dir(fs, E);
        let std_dir = FileStandardInformation::from_bytes(
            &d.query_information(file_class::STANDARD, 0x18).unwrap(),
        )
        .unwrap();
        assert!(std_dir.directory);
        assert_eq!((std_dir.end_of_file, std_dir.allocation_size), (0, 0));
        assert_eq!(d.basic_information().file_attributes, attributes::FILE_ATTRIBUTE_DIRECTORY);
        assert_eq!(d.sort_key(), SortKey::None);
    });
}

#[test]
fn volume_information_describes_fatx_on_the_hard_disk() {
    on_both("volinfo", |fs| {
        let f = open_dir(fs, E);
        let vol = FileFsVolumeInformation::from_bytes(
            &f.query_volume_information(fs_class::VOLUME).unwrap(),
        )
        .unwrap();
        assert_eq!(
            vol,
            FileFsVolumeInformation {
                volume_creation_time: DISC_TIME,
                volume_serial_number: 0x5678_ABCD
            }
        );
        let attr = FileFsAttributeInformation::from_bytes(
            &f.query_volume_information(fs_class::ATTRIBUTE).unwrap(),
        )
        .unwrap();
        assert_eq!(
            (attr.file_system_name.as_str(), attr.maximum_component_name_length),
            ("FATX", 42)
        );
        let dev = FileFsDeviceInformation::from_bytes(
            &f.query_volume_information(fs_class::DEVICE).unwrap(),
        )
        .unwrap();
        assert_eq!(dev, FileFsDeviceInformation { device_type: 7, characteristics: 0 });
        let size =
            FileFsSizeInformation::from_bytes(&f.query_volume_information(fs_class::SIZE).unwrap())
                .unwrap();
        let full = FileFsSizeInformation::from_full_bytes(
            &f.query_volume_information(fs_class::FULL_SIZE).unwrap(),
        )
        .unwrap();
        assert_eq!(size, full);
        assert_eq!((size.sectors_per_allocation_unit, size.bytes_per_sector), (32, 512));
        assert_eq!(size.total_allocation_units, PARTITIONS[1].1 / 16384);
        assert!(size.available_allocation_units <= size.total_allocation_units);
        assert_eq!(f.query_volume_information(2), Err(NtStatus::INVALID_INFO_CLASS));
    });
}

#[test]
fn a_partition_fills_as_its_files_grow() {
    let (_dir, fs) = host_fs("usage");
    let disk = fs.hard_disk().unwrap().clone();
    let free = |fs: &FileSystem| {
        let d = open_dir(fs, Z);
        FileFsSizeInformation::from_bytes(&d.query_volume_information(fs_class::SIZE).unwrap())
            .unwrap()
            .available_allocation_units
    };
    assert_eq!(disk.rescan_usage(5), 0);
    let before = free(&fs);
    put(&fs, &format!(r"{Z}\one"), &[0; 1]);
    put(&fs, &format!(r"{Z}\big"), &[0; 40_000]);
    mkdir(&fs, &format!(r"{Z}\sub"));
    put(&fs, &format!(r"{Z}\sub\exact"), &[0; 16384]);
    // Whole clusters: 1 + 3 + 1.
    assert_eq!(disk.rescan_usage(5), 5 * 16384);
    assert_eq!(disk.partition_used(5), 5 * 16384);
    let after = free(&fs);
    // The host's own free space may be the limit; then the files' real size moved it.
    assert!(after < before, "{after} < {before}");
    assert!(before - after <= 5);
    assert_eq!(disk.rescan_usage(4), 0);

    // The in-memory volume counts the same way and refuses what does not fit.
    let small = MemVolume::with_capacity(4 * 16384);
    let fs = FileSystem::new();
    fs.mount(Device::Partition(5), Arc::new(small.clone()));
    assert_eq!(free(&fs), 4);
    put(&fs, &format!(r"{Z}\one"), &[0; 1]);
    put(&fs, &format!(r"{Z}\big"), &[0; 40_000]);
    assert_eq!(free(&fs), 0);
    let (f, _) = open(&fs, &format!(r"{Z}\more"), RW, CreateDisposition::Create, SYNC).unwrap();
    assert_eq!(f.write(0, b"x"), Err(NtStatus::DISK_FULL));
    assert_eq!(small.file_bytes(r"\one"), Some(vec![0]));
}

#[test]
fn the_raw_disk_keeps_its_config_sectors() {
    let (dir, fs) = host_fs("raw");
    let name = r"\Device\Harddisk0\partition0";
    let (raw, action) = open(&fs, name, RW, CreateDisposition::Open, SYNC).unwrap();
    assert_eq!(action, CreateAction::Opened);
    assert!(raw.is_volume() && !raw.is_dir());
    // Never written: zeros.
    let mut buf = [0xFFu8; 1024];
    assert_eq!(raw.read(0x600, &mut buf), Ok(1024));
    assert_eq!(buf, [0; 1024]);
    assert_eq!(raw.write(0x600, &[7; 512]), Ok(512));
    assert_eq!(raw.read(0x500, &mut buf), Ok(1024));
    assert_eq!(buf[..0x100], [0; 0x100]);
    assert_eq!(buf[0x100..0x300], [7; 0x200]);
    assert_eq!(buf[0x300..], [0; 0x100]);
    assert!(dir.path().join("raw-partition0.bin").is_file());
    // Past the kept area: writes drop, reads give zeros; a write across the edge keeps its head.
    assert_eq!(raw.write(RAW_KEEP - 2, &[9; 4]), Ok(4));
    let mut edge = [1u8; 4];
    assert_eq!(raw.read(RAW_KEEP - 2, &mut edge), Ok(4));
    assert_eq!(edge, [9, 9, 0, 0]);
    assert_eq!(std::fs::metadata(dir.path().join("raw-partition0.bin")).unwrap().len(), RAW_KEEP);
    // It survives a reboot.
    drop(raw);
    let again = FileSystem::with_hard_disk(dir.path()).unwrap();
    let (raw, _) = open(&again, name, RW, CreateDisposition::Open, SYNC).unwrap();
    let mut buf = [0u8; 512];
    assert_eq!(raw.read(0x600, &mut buf), Ok(512));
    assert_eq!(buf, [7; 512]);
    // Not a file system: nothing under it, no listing, no file facts.
    assert_eq!(
        open(&again, r"\Device\Harddisk0\Partition0\x", RW, CreateDisposition::OpenIf, SYNC)
            .map(|_| ()),
        Err(NtStatus::OBJECT_NAME_NOT_FOUND)
    );
    assert_eq!(raw.query_directory(None, true, 0x100), Err(NtStatus::INVALID_DEVICE_REQUEST));
    assert_eq!(raw.basic_information().file_attributes, attributes::FILE_ATTRIBUTE_READONLY);
    assert_eq!(raw.set_information(file_class::DISPOSITION, &[1]), NtStatus::ACCESS_DENIED);
    assert_eq!(again.delete_file(Root::None, name), NtStatus::ACCESS_DENIED);
    assert_eq!(
        raw.control(IOCTL_DISK_GET_PARTITION_INFO, 32),
        Ok(Control::Output(partition_info(0).unwrap().to_vec()))
    );
}

#[test]
fn refurb_info_lives_in_the_config_area() {
    let (_dir, fs) = host_fs("refurb");
    let disk = fs.hard_disk().unwrap();
    assert_eq!(disk.refurb_info(), [0; 16]);
    let mut info = [0u8; 16];
    info[4..8].copy_from_slice(&42u32.to_le_bytes());
    info[8..16].copy_from_slice(&DISC_TIME.to_le_bytes());
    disk.set_refurb_info(&info);
    let read = disk.refurb_info();
    assert_eq!(&read[..4], b"BRFR");
    assert_eq!(read[4..], info[4..]);
    // Through the raw disk's handle it is sector 3.
    let (raw, _) =
        open(&fs, r"\Device\Harddisk0\Partition0", RW, CreateDisposition::Open, SYNC).unwrap();
    let mut sector = [0u8; 512];
    assert_eq!(raw.read(3 * 512, &mut sector), Ok(512));
    assert_eq!(sector[..16], read);
}

#[test]
fn formatting_a_cache_partition_empties_it_and_sets_its_cluster_size() {
    let (dir, fs) = host_fs("format");
    mkdir(&fs, &format!(r"{Z}\old"));
    put(&fs, &format!(r"{Z}\old\cached.map"), &[1; 100]);
    put(&fs, &format!(r"{E}\save.bin"), b"keep");
    // XMountUtilityDrive: lock, write the superblock to the partition, dismount.
    let (part, _) = open(&fs, Z, RW, CreateDisposition::Open, SYNC).unwrap();
    assert!(part.is_dir());
    assert_eq!(part.control(FSCTL_DISMOUNT_VOLUME, 0), Ok(Control::Output(Vec::new())));
    let mut superblock = vec![0u8; 4096];
    superblock[..4].copy_from_slice(b"FATX");
    superblock[4..8].copy_from_slice(&0x1234_5678u32.to_le_bytes());
    superblock[8..12].copy_from_slice(&8u32.to_le_bytes());
    assert_eq!(part.write(0, &superblock), Ok(4096));
    assert!(names(&open_dir(&fs, Z), None).is_empty());
    assert_eq!(std::fs::read_dir(dir.path().join("Partition5")).unwrap().count(), 0);
    assert_eq!(get(&fs, &format!(r"{E}\save.bin")).unwrap(), b"keep");
    // The head of the partition reads back raw, and the cluster size shows.
    let mut head = [0u8; 12];
    assert_eq!(part.read(0, &mut head), Ok(12));
    assert_eq!(head, superblock[..12]);
    let size =
        FileFsSizeInformation::from_bytes(&part.query_volume_information(fs_class::SIZE).unwrap())
            .unwrap();
    assert_eq!(size.sectors_per_allocation_unit, 8);
    assert_eq!(size.total_allocation_units, PARTITIONS[5].1 / 4096);
    // The superblock is kept across a reboot.
    let again = FileSystem::with_hard_disk(dir.path()).unwrap();
    assert_eq!(again.hard_disk().unwrap().cluster_sectors(5), 8);
    assert_eq!(again.hard_disk().unwrap().cluster_sectors(4), 32);
    // A data partition's root is not raw.
    let (data, _) = open(&fs, E, RW, CreateDisposition::Open, SYNC).unwrap();
    assert_eq!(data.read(0, &mut head), Err(NtStatus::INVALID_DEVICE_REQUEST));
    assert_eq!(data.write(0, &superblock), Err(NtStatus::ACCESS_DENIED));
    assert_eq!(get(&fs, &format!(r"{E}\save.bin")).unwrap(), b"keep");
}

#[test]
fn fatx_names_can_be_enforced_on_creation() {
    let (_dir, fs) = host_fs("fatx");
    let long = format!(r"{E}\{}", "n".repeat(43));
    put(&fs, &long, b"as the reference: whatever the host takes");
    fs.hard_disk().unwrap().set_enforce_fatx_names(true);
    let create = |name: &str| open(&fs, name, RW, CreateDisposition::OpenIf, SYNC).map(|(_, a)| a);
    assert_eq!(create(&format!(r"{E}\{}", "m".repeat(43))), Err(NtStatus::OBJECT_NAME_INVALID));
    assert_eq!(create(&format!(r"{E}\a+b")), Err(NtStatus::OBJECT_NAME_INVALID));
    assert_eq!(create(&format!(r"{E}\{}", "m".repeat(42))), Ok(CreateAction::Created));
    // What exists still opens.
    assert_eq!(create(&long), Ok(CreateAction::Opened));
}

fn disc() -> MemDisc {
    let mut d = MemDisc::new();
    d.add_file("default.xbe", b"XBEH0123")
        .add_file(r"bink\intro.bik", &[0xB1; 5000])
        .add_file(r"XDemos\XDemos.xbe", b"demo")
        .add_file(r"sound\english\voice.wav", b"voice")
        .add_dir("maps");
    d
}

#[test]
fn the_disc_is_read_only_gdfx() {
    let fs = mem_fs();
    assert_eq!(get(&fs, r"\??\D:\default.xbe").map(|_| ()), Err(NtStatus::NO_MEDIA_IN_DEVICE));
    fs.insert_disc(Arc::new(disc()));
    let (f, action) = fs
        .open(Root::DosDevices, r"D:\BINK\Intro.bik", &OpenParams::open(access::GENERIC_READ, 1, 0))
        .unwrap();
    assert_eq!(action, CreateAction::Opened);
    assert_eq!(f.path(), r"\Device\CdRom0\BINK\Intro.bik");
    assert_eq!((f.device(), f.device_kind()), (Device::CdRom0, DeviceKind::Disc));
    assert_eq!(f.size(), 5000);
    let mut buf = vec![0u8; 4096];
    assert_eq!(f.read(2048, &mut buf), Ok(2952));
    assert_eq!(buf[..2952], [0xB1; 2952]);
    assert_eq!(f.read(5000, &mut buf), Err(NtStatus::END_OF_FILE));
    assert_eq!(f.write(0, b"x"), Err(NtStatus::ACCESS_DENIED));
    assert_eq!(f.set_information(file_class::END_OF_FILE, &[0; 8]), NtStatus::ACCESS_DENIED);
    assert_eq!(f.set_information(file_class::DISPOSITION, &[1]), NtStatus::ACCESS_DENIED);
    assert_eq!(fs.rename(&f, Root::DosDevices, r"D:\x", true), NtStatus::ACCESS_DENIED);
    assert_eq!(fs.delete_file(Root::DosDevices, r"D:\default.xbe"), NtStatus::ACCESS_DENIED);

    let net = f.network_open_information();
    assert_eq!(net.creation_time, DISC_TIME);
    assert_eq!((net.end_of_file, net.allocation_size), (5000, 8192));
    assert_eq!(net.file_attributes, attributes::FILE_ATTRIBUTE_READONLY);
    let sector = disc().lookup(r"bink\intro.bik").unwrap().sector;
    assert_eq!(f.index_number(), u64::from(sector));
    assert_eq!(f.sort_key(), SortKey::Gdfx { start_sector: sector });

    // Anything but opening is a write to protected media, even for a file that is there.
    for disposition in [
        CreateDisposition::Supersede,
        CreateDisposition::Create,
        CreateDisposition::Overwrite,
        CreateDisposition::OverwriteIf,
    ] {
        assert_eq!(
            open(&fs, r"\Device\CdRom0\default.xbe", RW, disposition, 0).map(|_| ()),
            Err(NtStatus::MEDIA_WRITE_PROTECTED)
        );
    }
    assert_eq!(
        open(&fs, r"\Device\CdRom0\new.bin", RW, CreateDisposition::OpenIf, 0).map(|_| ()),
        Err(NtStatus::OBJECT_NAME_NOT_FOUND)
    );
    assert_eq!(
        open(
            &fs,
            r"\Device\CdRom0\maps",
            0,
            CreateDisposition::Open,
            options::FILE_NON_DIRECTORY_FILE
        )
        .map(|_| ()),
        Err(NtStatus::FILE_IS_A_DIRECTORY)
    );
    assert_eq!(
        open(
            &fs,
            r"\Device\CdRom0\default.xbe",
            0,
            CreateDisposition::Open,
            options::FILE_DIRECTORY_FILE
        )
        .map(|_| ()),
        Err(NtStatus::NOT_A_DIRECTORY)
    );
    // A language the disc lacks falls back to English; the handle keeps the title's path.
    let (voice, _) = open(
        &fs,
        r"\Device\CdRom0\sound\spanish\voice.wav",
        access::GENERIC_READ,
        CreateDisposition::Open,
        0,
    )
    .unwrap();
    assert_eq!(voice.size(), 5);
    assert_eq!(voice.path(), r"\Device\CdRom0\sound\spanish\voice.wav");

    // The root, as a directory and as the device.
    let root = open_dir(&fs, r"\??\CDROM0:\");
    assert!(root.is_dir() && root.is_volume());
    assert_eq!(names(&root, None), ["XDemos", "bink", "default.xbe", "maps", "sound"]);
    assert_eq!(names(&root, Some("*.xbe")), ["default.xbe"]);
    let entry = root.query_directory(Some("default.xbe"), true, 0x100).unwrap();
    assert_eq!(entry.file_attributes, attributes::FILE_ATTRIBUTE_READONLY);
    assert_eq!(root.basic_information().file_attributes, 0x11);
    assert_eq!(root.sort_key(), SortKey::None);
    assert_eq!(root.read(0, &mut buf), Err(NtStatus::INVALID_DEVICE_REQUEST));

    let attr = FileFsAttributeInformation::from_bytes(
        &f.query_volume_information(fs_class::ATTRIBUTE).unwrap(),
    )
    .unwrap();
    assert_eq!(attr.file_system_name, "GDFX");
    let dev =
        FileFsDeviceInformation::from_bytes(&f.query_volume_information(fs_class::DEVICE).unwrap())
            .unwrap();
    assert_eq!(dev, FileFsDeviceInformation { device_type: 2, characteristics: 1 });
    let size =
        FileFsSizeInformation::from_bytes(&f.query_volume_information(fs_class::SIZE).unwrap())
            .unwrap();
    assert_eq!(
        size,
        FileFsSizeInformation {
            total_allocation_units: 7 * 1024 * 1024 * 1024 / 2048,
            available_allocation_units: 0,
            sectors_per_allocation_unit: 1,
            bytes_per_sector: 2048
        }
    );
    let vol =
        FileFsVolumeInformation::from_bytes(&f.query_volume_information(fs_class::VOLUME).unwrap())
            .unwrap();
    assert_eq!(vol.volume_serial_number, 0x1234_5678);
    assert_eq!(f.control(IOCTL_DISK_GET_PARTITION_INFO, 32), Err(NtStatus::INVALID_DEVICE_REQUEST));
    assert_eq!(f.control(ioctl::IOCTL_SCSI_PASS_THROUGH_DIRECT, 0), Ok(Control::ScsiPassThrough));

    // The disc out of the drive: open files stay, new opens find no media.
    assert!(fs.unmount(Device::CdRom0).is_some());
    assert_eq!(f.read(0, &mut buf[..8]), Ok(8));
    assert_eq!(get(&fs, r"\??\D:\default.xbe").map(|_| ()), Err(NtStatus::NO_MEDIA_IN_DEVICE));
}

#[test]
fn the_disc_device_reads_raw_when_given_its_blocks() {
    let fs = FileSystem::new();
    let image: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
    fs.mount(
        Device::CdRom0,
        Arc::new(DiscVolume::with_raw(Arc::new(disc()), Arc::new(image.clone()))),
    );
    let (dev, _) =
        open(&fs, r"\Device\CdRom0", access::GENERIC_READ, CreateDisposition::Open, 0).unwrap();
    let mut buf = [0u8; 4096];
    assert_eq!(dev.read(2048, &mut buf), Ok(4096));
    assert_eq!(buf[..], image[2048..6144]);
    // Past the end of the device: zeros.
    assert_eq!(dev.read(8192, &mut buf), Ok(4096));
    assert_eq!(buf[..1808], image[8192..]);
    assert_eq!(buf[1808..], [0; 2288]);
    assert_eq!(dev.write(0, &buf), Err(NtStatus::ACCESS_DENIED));
    // Files are not raw.
    let (f, _) =
        open(&fs, r"\Device\CdRom0\default.xbe", access::GENERIC_READ, CreateDisposition::Open, 0)
            .unwrap();
    assert_eq!(f.read(0, &mut buf), Ok(8));
    assert_eq!(&buf[..8], b"XBEH0123");
}

#[test]
fn completion_rules_follow_the_handle_and_the_drive() {
    let (_dir, fs) = host_fs("pend");
    fs.insert_disc(Arc::new(disc()));
    let disc_file = |opts| {
        open(
            &fs,
            r"\Device\CdRom0\bink\intro.bik",
            access::GENERIC_READ,
            CreateDisposition::Open,
            opts,
        )
        .unwrap()
        .0
    };
    let (overlapped, sync) = (disc_file(0), disc_file(SYNC));
    // The DVD drive pends every whole-sector transfer of an overlapped handle (Otogi)...
    assert!(overlapped.read_pends(0, 2048, false));
    assert!(overlapped.read_pends(4096, 4096, true));
    // ...and part of a sector comes from the cache, in the call (Wreckless).
    assert!(overlapped.read_completes_in_cache(0, 12));
    assert!(overlapped.read_completes_in_cache(100, 2048));
    assert!(!overlapped.read_pends(0, 12, true));
    assert!(!sync.read_pends(0, 2048, true));

    put(&fs, &format!(r"{Z}\c.bin"), &[0; 4096]);
    let hdd =
        |opts| open(&fs, &format!(r"{Z}\c.bin"), RW, CreateDisposition::Open, opts).unwrap().0;
    let (cached, direct, sync) = (hdd(0), hdd(options::FILE_NO_INTERMEDIATE_BUFFERING), hdd(SYNC));
    assert!(!cached.read_completes_in_cache(0, 12));
    // Through the cache: pends only with an event or APC; writes never (Breakdown).
    assert!(cached.read_pends(0, 12, true) && !cached.read_pends(0, 12, false));
    assert!(!cached.write_pends());
    // Past the cache: reads and writes pend (Sniper Elite).
    assert!(direct.no_buffering() && direct.read_pends(0, 512, false) && direct.write_pends());
    assert!(!sync.read_pends(0, 512, true) && !sync.write_pends());

    // Console timing's cache window: the pages of the last drive read.
    assert!(!sync.cache_holds(0, 1));
    assert_eq!(sync.cache_fill(5000, 100), 4096);
    assert!(sync.cache_holds(4096, 4096) && sync.cache_holds(5000, 100));
    assert!(!sync.cache_holds(4095, 2) && !sync.cache_holds(8191, 2));
    assert_eq!(sync.cache_fill(4000, 200), 8192);
    let timing = DeviceTiming::default();
    assert!(timing.cost(sync.device_kind(), 8192) < timing.cost(overlapped.device_kind(), 8192));
}

#[test]
fn device_objects_are_found_by_name() {
    let fs = mem_fs();
    assert_eq!(fs.reference_device(r"\Device\CdRom0"), Ok(DeviceRef::Builtin(Device::CdRom0)));
    assert_eq!(fs.reference_device(r"\??\D:"), Ok(DeviceRef::Builtin(Device::CdRom0)));
    assert_eq!(
        fs.reference_device(r"\device\harddisk0\partition1\"),
        Ok(DeviceRef::Builtin(Device::Partition(1)))
    );
    assert_eq!(
        fs.reference_device(r"\Device\CdRom0\default.xbe"),
        Err(NtStatus::OBJECT_NAME_NOT_FOUND)
    );
    assert_eq!(fs.reference_device(r"\Device\MU_0"), Err(NtStatus::OBJECT_NAME_NOT_FOUND));
    assert_eq!(fs.reference_device(r"\??\Q:"), Err(NtStatus::OBJECT_NAME_NOT_FOUND));
    assert_eq!(fs.register_device(r"\Device\MU_0", 0x8001_0000).as_deref(), Ok(r"\Device\MU_0"));
    assert_eq!(
        fs.register_device(r"\device\mu_0", 0x8002_0000),
        Err(NtStatus::OBJECT_NAME_COLLISION)
    );
    assert_eq!(fs.register_device(r"\??\Q:\x", 1), Err(NtStatus::OBJECT_PATH_NOT_FOUND));
    assert_eq!(fs.reference_device(r"\DEVICE\MU_0"), Ok(DeviceRef::Registered(0x8001_0000)));
    fs.unregister_device(0x8001_0000);
    assert_eq!(fs.reference_device(r"\Device\MU_0"), Err(NtStatus::OBJECT_NAME_NOT_FOUND));
}

#[test]
fn cache_partitions_in_use_are_those_linked_or_open() {
    let (_dir, fs) = host_fs("cachemask");
    assert_eq!(fs.cache_partitions(), 0);
    fs.create_symbolic_link(r"\??\Z:", Z);
    assert_eq!(fs.cache_partitions(), 1 << 5);
    put(&fs, r"\Device\Harddisk0\Partition3\x", b"x");
    let (f, _) =
        open(&fs, r"\Device\Harddisk0\Partition3\x", RW, CreateDisposition::Open, SYNC).unwrap();
    assert_eq!(fs.cache_partitions(), 1 << 5 | 1 << 3);
    drop(f);
    assert_eq!(fs.cache_partitions(), 1 << 5);
}

#[test]
fn one_file_reads_from_many_threads_at_once() {
    let (_dir, fs) = host_fs("threads");
    let name = format!(r"{Z}\stream.bin");
    let data: Vec<u8> =
        (0..1 << 20).map(|i: u32| (i.wrapping_mul(2_654_435_761) >> 24) as u8).collect();
    put(&fs, &name, &data);
    let file =
        Arc::new(open(&fs, &name, access::GENERIC_READ, CreateDisposition::Open, 0).unwrap().0);
    let data = Arc::new(data);
    let threads: Vec<_> = (0..8usize)
        .map(|t| {
            let (file, data) = (Arc::clone(&file), Arc::clone(&data));
            std::thread::spawn(move || {
                let mut buf = vec![0u8; 4096];
                for i in 0..64usize {
                    let offset = (t * 131_071 + i * 16_381) % (data.len() - buf.len());
                    assert_eq!(file.read(offset as u64, &mut buf), Ok(buf.len()));
                    assert_eq!(buf[..], data[offset..offset + buf.len()]);
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    assert_eq!(file.position(), 0);
}

/// Replays the `xbased-native-io` lines of the reference's Halo boot log
/// against a hard disk in a temporary folder: every link must resolve to the
/// target the reference logged, and every open must give the path and the
/// result it logged (a directory, created, overwritten or just opened).
#[test]
fn the_halo_reference_log_replays_with_the_same_results() {
    let Some(log) = xb_testkit::reference_log("halo-ce") else {
        println!("note: no halo-ce.ref.log in the titles stash; the log replay did not run");
        return;
    };
    let lines: Vec<&str> =
        log.lines().filter_map(|l| l.strip_prefix("xbased-native-io: ")).collect();
    let (_dir, fs) = host_fs("halo");
    // The disc holds whatever the log opens on it.
    let mut disc = MemDisc::new();
    for line in &lines {
        if let Some(rest) = line.strip_prefix(r"open \Device\CdRom0\")
            && let Some((path, _)) = rest.split_once(" -> ")
        {
            disc.add_file(path, b"disc file");
        }
    }
    fs.insert_disc(Arc::new(disc));

    let ensure_parents = |path: &str| {
        let (device, rel) = device_of(path).unwrap();
        let parts: Vec<&str> = rel.split('\\').filter(|p| !p.is_empty()).collect();
        let mut dir = device.path();
        for part in &parts[..parts.len().saturating_sub(1)] {
            dir = format!(r"{dir}\{part}");
            mkdir(&fs, &dir);
        }
    };
    let (mut links, mut opens, mut deletes) = (0, 0, 0);
    for line in &lines {
        if let Some(rest) = line.strip_prefix("link ") {
            let (name, target) = rest.split_once(" -> ").unwrap();
            assert_eq!(fs.create_symbolic_link(name, target), NtStatus::SUCCESS, "{line}");
            assert_eq!(fs.symbolic_link_target(name).as_deref(), Some(target), "{line}");
            links += 1;
        } else if let Some(rest) = line.strip_prefix("open ") {
            let Some((path, result)) = rest.split_once(" -> ") else { continue };
            let on_disc = device_of(path).unwrap().0 == Device::CdRom0;
            let raw = device_of(path) == Some((Device::Partition(0), ""));
            let is_dir = result.starts_with("dir ");
            let what = result.trim_start_matches("dir ").trim();
            if !on_disc && !raw {
                ensure_parents(path);
            }
            let dir_opt = if is_dir {
                options::FILE_DIRECTORY_FILE
            } else {
                options::FILE_NON_DIRECTORY_FILE
            };
            let (disposition, expect) = match what {
                "(created)" => (CreateDisposition::Create, CreateAction::Created),
                "(overwritten)" => (CreateDisposition::OverwriteIf, CreateAction::Overwritten),
                "(superseded)" => (CreateDisposition::Supersede, CreateAction::Superseded),
                _ => (CreateDisposition::Open, CreateAction::Opened),
            };
            // What the reference found there from before the log.
            if !on_disc && !raw && expect != CreateAction::Created {
                let existing = open(&fs, path, RW, CreateDisposition::OpenIf, dir_opt | SYNC);
                assert!(existing.is_ok(), "{line}: {existing:?}");
            }
            let (file, action) =
                open(&fs, path, RW, disposition, if raw { SYNC } else { dir_opt | SYNC })
                    .unwrap_or_else(|e| panic!("{line}: {e}"));
            assert_eq!(action, expect, "{line}");
            assert_eq!(file.path(), path, "{line}");
            assert_eq!(file.is_dir(), is_dir, "{line}");
            opens += 1;
        } else if let Some(rest) = line.strip_prefix("delete ") {
            let Some((path, result)) = rest.split_once(" -> ") else { continue };
            // Deleted through a handle's disposition; the file goes at its close.
            let (file, _) =
                open(&fs, path, RW | access::DELETE, CreateDisposition::Open, SYNC).unwrap();
            let status = file.set_information(file_class::DISPOSITION, &[1]);
            assert_eq!(format!("{:x}", status.0), result.split(' ').next().unwrap(), "{line}");
            deletes += 1;
        } else if line.starts_with("console timing") {
            assert_eq!(*line, DeviceTiming::default().describe());
        }
    }
    println!(
        "replayed {links} links, {opens} opens, {deletes} deletes from the Halo reference log"
    );
    assert!(links >= 5 && opens > 100 && deletes > 10);

    // The links the title made resolve XAPI's names as the log shows them.
    for (name, expect) in [
        (r"Z:\saved\hdmu.map", r"\Device\Harddisk0\Partition5\saved\hdmu.map"),
        (
            r"U:\122A17771B9E\blam.sav",
            r"\Device\Harddisk0\partition1\UDATA\4d530004\122A17771B9E\blam.sav",
        ),
        (r"T:\", r"\Device\Harddisk0\partition1\TDATA\4d530004"),
        (r"D:\bink\intro.bik", r"\Device\CdRom0\bink\intro.bik"),
    ] {
        assert_eq!(fs.canonicalize(Root::DosDevices, name).as_deref(), Some(expect));
        let (file, _) = fs
            .open(Root::DosDevices, name, &OpenParams::open(access::GENERIC_READ, share::ALL, SYNC))
            .unwrap_or_else(|e| panic!("{name}: {e:?}"));
        assert_eq!(file.path(), expect);
    }
}

/// Pins what a savestate holds of this: see `xb_types::state::StateIo::probe`.
#[test]
fn the_saved_layout_of_an_open_file_is_the_recorded_one() {
    let mut io = xb_types::state::StateIo::probe();
    let mut v = crate::FileState::default();
    io.io(&mut v);
    assert_eq!(
        io.shape_hash(),
        0x03FA_79AE_423A_CCC4,
        "the saved layout changed. Older states would be misread: give the section a new version, \
         read the old one where it differs, then record {:#018x} here",
        io.shape_hash()
    );
}
