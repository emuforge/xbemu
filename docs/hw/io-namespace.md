# The I/O namespace and storage

How `xb-fs` models the Xbox's storage: the object namespace, the disc, the
hard disk, and the rules titles depend on. The source for everything here is
the reference implementation unless another is named; games named are the
ones whose behavior requires the rule.

## Objects

| Object path | What | Backed by |
|---|---|---|
| `\Device\CdRom0` | the DVD drive: GDFX (XDVDFS), read-only, removable | a `DiscTree` (the parsed disc) |
| `\Device\Harddisk0\Partition0` | the whole disk, opened raw | `<hdd>/raw-partition0.bin`, first 512 KiB |
| `\Device\Harddisk0\Partition1` | data drive (E:): `TDATA`, `UDATA` | `<hdd>/Partition1/` |
| `\Device\Harddisk0\Partition2` | system drive (C:) | `<hdd>/Partition2/` |
| `\Device\Harddisk0\Partition3..5` | cache drives (X: Y: Z:) | `<hdd>/Partition3..5/`, `<hdd>/raw-partitionN.bin` |
| `\Device\Harddisk0\Partition6..9` | a larger disk's extra drives (F: G:) | `<hdd>/Partition6..9/` |

Device names match in any ASCII case; the partition number is one digit.
Memory units (`\Device\MU_0..7`) are device objects the title's own driver
creates with `IoCreateDevice`; nothing backs them.

The retail disk is 0x1DD156000 bytes of 512-byte sectors:

| Partition | Offset | Length |
|---|---|---|
| 0 (config area) | 0 | 0x80000 |
| 1 (E:) | 0xABE80000 | 0x1312D6000 |
| 2 (C:) | 0x8CA80000 | 0x1F400000 |
| 3 (X:) | 0x80000 | 0x2EE00000 |
| 4 (Y:) | 0x2EE80000 | 0x2EE00000 |
| 5 (Z:) | 0x5DC80000 | 0x2EE00000 |

## Symbolic links

Links live in `\??` and are created and deleted at run time
(`IoCreateSymbolicLink`, `IoDeleteSymbolicLink`). At most 64 exist. A link's
name is kept as given and found in any case; its target is stored in
canonical form.

At boot the kernel makes two: `\??\D:` and `\??\CdRom0:`, both to
`\Device\CdRom0`. For an XBE launched from the hard disk, `D:` is the folder
it was launched from (`set_launch_dir`).

Everything else is the title's doing. XAPI's startup code, linked into every
title, opens `\Device\Harddisk0\partition1\TDATA\<title id>` and
`…\UDATA\<title id>` (creating them) and links `T:` and `U:` to them; a
title that mounts a utility drive then opens `Partition0` raw and a cache
partition, and links `Z:` to it. Halo's sequence (the opens between the
links left out):

```
link \??\D: -> \Device\CdRom0
link \??\T: -> \Device\Harddisk0\partition1\TDATA\4d530004
link \??\U: -> \Device\Harddisk0\partition1\UDATA\4d530004
link \??\Z: -> \Device\Harddisk0\Partition5
```

## Names

Xbox names are ANSI. `OBJECT_ATTRIBUTES` gives a root directory and a name:

- no root: the name is a full object path;
- `ObDosDevicesDirectory` (0xFFFFFFFD): the name is under `\??` (XAPI passes
  `D:\x`);
- an open file's handle: the name is joined to that file's path.

The canonical form: `/` read as `\`; a leading `\??\X:` replaced by the
link's target (no such link: the open fails with
`STATUS_OBJECT_PATH_NOT_FOUND`); a leading `\??\name` of any length replaced
if such a link exists and left alone if not (Xbox Music Mixer probes
`\??\CDROM0:\…`); empty components and `.` dropped, `..` removing the one
before it; at most 64 components. Links resolve once.

A canonical path on no storage device fails as the object manager's lookup
would: `STATUS_OBJECT_NAME_NOT_FOUND` when its parent is `\`, `\??` or
`\Device`, else `STATUS_OBJECT_PATH_NOT_FOUND` (GTA III probes
`\??\x.dds` and shows its bad-disc screen on a bad path).

## Opening

On the disc, in order: no disc mounted, `STATUS_NO_MEDIA_IN_DEVICE`; a
disposition other than `FILE_OPEN` / `FILE_OPEN_IF`,
`STATUS_MEDIA_WRITE_PROTECTED`; not found (after trying the English path, see
below), `STATUS_OBJECT_NAME_NOT_FOUND`; `FILE_DIRECTORY_FILE` on a file,
`STATUS_NOT_A_DIRECTORY`; `FILE_NON_DIRECTORY_FILE` on a directory,
`STATUS_FILE_IS_A_DIRECTORY`.

On `Partition0`: any path below it is `STATUS_OBJECT_NAME_NOT_FOUND`; the
device itself opens.

On a partition with files, in order:

1. a missing parent: `STATUS_OBJECT_PATH_NOT_FOUND`;
2. a file marked for deletion: `STATUS_DELETE_PENDING`;
3. exists and `FILE_CREATE`: `STATUS_OBJECT_NAME_COLLISION`;
4. a share conflict with an open handle: `STATUS_SHARING_VIOLATION`;
5. missing and `FILE_OPEN` or `FILE_OVERWRITE`:
   `STATUS_OBJECT_NAME_NOT_FOUND`;
6. a directory and `FILE_NON_DIRECTORY_FILE`: `STATUS_FILE_IS_A_DIRECTORY`;
7. a file and `FILE_DIRECTORY_FILE`: `STATUS_NOT_A_DIRECTORY`;
8. missing and `FILE_DIRECTORY_FILE`: the directory is created;
9. otherwise the file is opened, created or emptied. The host refusing is
   `STATUS_ACCESS_DENIED`.

The IoStatusBlock's information:

| Disposition | File exists | File missing |
|---|---|---|
| `FILE_SUPERSEDE` (0) | 0 superseded | 2 created |
| `FILE_OPEN` (1) | 1 opened | fails |
| `FILE_CREATE` (2) | fails | 2 created |
| `FILE_OPEN_IF` (3) | 1 opened | 2 created |
| `FILE_OVERWRITE` (4) | 3 overwritten | fails |
| `FILE_OVERWRITE_IF` (5) | 3 overwritten | 2 created |

An existing directory is opened (1) whatever the disposition, `FILE_CREATE`
aside. A failed open leaves 5 (`FILE_DOES_NOT_EXIST`). `AllocationSize` and
`FileAttributes` are ignored.

Share access follows `IoCheckShareAccess`: a handle is counted for reading
(`FILE_READ_DATA`, `FILE_EXECUTE`, `GENERIC_READ`, `GENERIC_EXECUTE`), writing
(`FILE_WRITE_DATA`, `FILE_APPEND_DATA`, `GENERIC_WRITE`) and deleting
(`DELETE`); `GENERIC_ALL` and `MAXIMUM_ALLOWED` count for all three. A new
open conflicts with an open handle when either wants what the other does not
share. A handle counted for nothing takes no part. Only hard disk files are
checked. (Mission: Impossible - Operation Surma holds each cache copy open
unshared and reads from the disc while its open of the copy fails.)

A handle may change a file's data or size only if its access has
`FILE_WRITE_DATA`, `FILE_APPEND_DATA`, `GENERIC_WRITE`, `GENERIC_ALL` or
`MAXIMUM_ALLOWED`; otherwise writes, `FileEndOfFileInformation` and
`FileAllocationInformation` are `STATUS_ACCESS_DENIED`. (Splinter Cell calls
`SetEndOfFile` on its profile through a read-only handle and needs it to
fail.)

## The hard disk on the host

FATX names are case-insensitive; host file systems mostly are not. Each
component is looked up in the folder reached so far: a host name spelled as
the title typed it is taken directly; otherwise the folder is listed and the
first name that matches ignoring ASCII case is used. A component that does
not exist is created with the title's spelling. The exact-spelling shortcut
differs from listing the folder for every component only when a folder holds
two names that differ by case alone.

Names the console's FATX takes that Windows cannot hold are escaped in the
host name on every host alike, so a hard disk folder moves between systems:
trailing spaces and dots, the first character of a DOS device name (`CON`,
`PRN`, `AUX`, `NUL`, `CONIN$`, `CONOUT$`, `COM0-9`, `LPT0-9`, with or without
an extension) and `=` itself are written `=XX` in hex. (RalliSport Challenge
makes a folder named `" "` per profile; on the host it is `=20`.)

As in the reference, names are not checked against FATX's rules (42
characters, no `" * + , / : ; < = > ? \ |`, no control characters) by
default. `xb-fs` offers the check (`name::fatx_name_valid`) and can refuse
creating invalid names (`HostDisk::set_enforce_fatx_names`). The character
rule is the homebrew community's description of the console's driver.
Directory listings leave out names of 64 characters or more, as the
reference's fixed-size entries do.

### Overwriting keeps the old bytes for a while

FATX does not zero what a file grows back into. A file opened with an
overwriting disposition is empty at once, but growing it again through the
same handle (`SetEndOfFile`, a write past the end) brings its old bytes back
in the grown range (Forza trims its unpacked cabs so). The old contents are
moved to `<hdd>/.overwrite/<pid>-<n>` and removed when the handle closes;
files a dead process left there are removed at the first overwrite of a run.

### Delete

`FileDispositionInformation` on a file marks it; it goes when the last handle
on it closes, and until then every open of it fails with
`STATUS_DELETE_PENDING` (Sudeki deletes its `boot.baf` with a handle still
open). `NtDeleteFile` deletes at once, or marks the file if handles are open.
A directory is removed at once: `STATUS_DIRECTORY_NOT_EMPTY` through a
handle, `STATUS_ACCESS_DENIED` through `NtDeleteFile` if it has entries. On
the disc and the raw disk deleting is `STATUS_ACCESS_DENIED`.

### Rename

`FileRenameInformation`'s name is relative to its `RootDirectory` as an
open's is (Riddick's file cache renames `Z:\X.tmp` to `Z:\X` under `\??`).
Only hard disk files and directories rename. A target on no partition or
under a missing folder is `STATUS_OBJECT_PATH_NOT_FOUND`; an existing target
without `ReplaceIfExists` is `STATUS_OBJECT_NAME_COLLISION`; the host
refusing is `STATUS_ACCESS_DENIED`. The handle's path follows the file, and
so do the other handles open on it.

### Sizes and free space

Every volume reports a file's allocation size as its size rounded up to
4 KiB. `FileFsSizeInformation` for a partition: 512-byte sectors; the
cluster size of its last format (the superblock kept in
`raw-partitionN.bin`), else 32 sectors (16 KiB); total clusters from the
table above; free clusters from the partition's size less what its files
take in whole clusters, and no more than the host file system has free.
Partitions outside 1 to 5 report as partition 1. The folders themselves have
no limit (Riddick's file cache outgrows a 750 MB drive).

A partition's use is measured by walking its folder, in a background thread:
all five when the disk is opened, and one again when its files have changed
and its last measurement is over two seconds old. A query answers with the
last measurement and waits only for a partition's first.

### Raw access

`Partition0` opened raw reads and writes the first 0x80000 bytes of
`raw-partition0.bin`: the config area, where XAPI keeps its cache drive
records and the kernel the refurb info (`ExReadWriteRefurbInfo`: 16 bytes at
sector 3 under the signature `RFRB`; zeros until written). Past that, reads
give zeros and writes are dropped.

A cache partition's root handle (partitions 3 and up) reads and writes
`raw-partitionN.bin` the same way. A write at offset 0 beginning with `FATX`
is a format (`XMountUtilityDrive`): the sectors-per-cluster field at +8 is
remembered and the partition's folder is emptied (its contents are moved to
`<hdd>/.overwrite/format-…` and deleted by a background thread, so a slow
host drive does not hold the title up; Project Gotham Racing 2 formats its
cache as it starts). Reads of the roots of partitions 1 and 2 fail with
`STATUS_INVALID_DEVICE_REQUEST`, writes with `STATUS_ACCESS_DENIED`.

## The disc

Every disc file's times are one constant (0x01C0739BEB8F8000, early on
2001-01-01), its attributes `FILE_ATTRIBUTE_READONLY` (directories 0x11).
The volume reports 2048-byte sectors, one sector per unit, a full DVD-9 of
units and none free; serial 0x12345678; file system name `GDFX`. (The hard
disk: serial 0x5678ABCD, `FATX`, maximum component length 42 for both.
`XGetFilePhysicalSortKey` knows the file control block by the file system's
name; Fatal Frame II sorts its load queue by those keys.)

`FileInternalInformation` is the file's start sector (the host inode on the
hard disk). The sort key is the start sector too, except for TOCA Race Driver
3's `.big` archives: their header says where they sat on the pressed disc
relative to `default.xbe`, the title compares that with the key, and a
rebuilt image gets the pressed disc's sector reported instead.

A path naming a language the disc lacks (`french`, `fre`, `fr` and the like
as a component or a file name's stem) is retried with English's name in the
same letter case. `disc_lacks_language` tells the kernel whether to report
English as the console's language: the disc has an English folder in its
first three levels but none for the console's language beside one.

Reads of `\Device\CdRom0` itself are refused
(`STATUS_INVALID_DEVICE_REQUEST`), as in the reference, unless the volume
has a `RawBlocks` to serve them from (`DiscVolume::with_raw`).

## Reading and writing

- A read of zero bytes succeeds, past the end too (Halo's map streaming
  queues reads of what is left, 0 at the end). Otherwise a read at or past
  the end is `STATUS_END_OF_FILE`, and one that crosses it returns what is
  there. A directory or device without raw access:
  `STATUS_INVALID_DEVICE_REQUEST`. A host read failing:
  `STATUS_DEVICE_DATA_ERROR`.
- A write to anything but a hard disk file opened for writing is
  `STATUS_ACCESS_DENIED`; a host write failing is `STATUS_DISK_FULL`.
- `ByteOffset` 0xFFFFFFFFFFFFFFFE means the file position; for writes
  0xFFFFFFFFFFFFFFFF means the end of the file.
- The file position moves to the end of a transfer through a synchronous
  handle, or when the request named no offset. An overlapped request that
  names its offset leaves it alone (Burnout 2 moves it itself after each
  overlapped read).

### What pends (for the kernel)

`xb-fs` completes every call synchronously. The kernel decides what the
title sees, with these rules from `io_finish`, which `File::read_pends` and
`File::write_pends` compute:

- A request that fails up front (end of file) returns its error and leaves
  the IoStatusBlock, event and APC alone (Forza's read-ahead past the end).
- A successful read through an overlapped handle returns `STATUS_PENDING`
  when it has an event or APC, or goes to a drive that pends every transfer:
  the DVD drive (Otogi), or the hard disk through a handle opened with
  `FILE_NO_INTERMEDIATE_BUFFERING`.
- A disc read of part of a sector (offset or length not a multiple of 2048)
  goes through the cache and completes in the call, overlapped or not
  (Wreckless).
- A successful write pends only through an overlapped handle with
  `FILE_NO_INTERMEDIATE_BUFFERING` (Sniper Elite waits for it; Breakdown and
  Hunter: The Reckoning take a pending cached write for a failure).
- A request that pends leaves `STATUS_PENDING` in its IoStatusBlock until it
  completes (The Nightmare Before Christmas: Oogie's Revenge).
- Overlapped requests complete in the order they were issued, across both
  drives: the console's drives share one IDE channel (Psychonauts).
- A file's last handle closes only after its requests in flight complete
  (Oddworld: Stranger's Wrath).

## Directory enumeration

`NtQueryDirectoryFile` returns one `FILE_DIRECTORY_INFORMATION` per call
(class 1 only). The directory is listed once when an enumeration starts (the
first call, `RestartScan`, or a mask different from the enumeration's) and
the enumeration works on that snapshot (XDeleteSaveGame deletes each file as
it is returned). A call with no mask goes on with the enumeration's mask
(Riddick deletes `_filler???` with FindFirstFile / FindNextFile); an empty
mask is `*`. Matching ignores ASCII case; `*` is any run of characters, `?`
exactly one, and there are no DOS rules (`*.*` needs a dot).

Nothing matching from the start is `STATUS_NO_SUCH_FILE`; the end after
matches is `STATUS_NO_MORE_FILES`. An entry that does not fit the buffer is
`STATUS_BUFFER_TOO_SMALL` and is passed over. Every entry's four times are
the disc's constant, on the hard disk too; `FileIndex` is the entry's place
in the snapshot; attributes are `DIRECTORY`, else `NORMAL` on the hard disk
and `READONLY` on the disc. A listing holds up to 2^20 entries (Super Monkey
Ball Deluxe has a folder of 2538).

## Information classes

`NtQueryInformationFile`: basic (4, 0x28 bytes), standard (5, 0x18),
internal (6, 8), position (14, 8), alignment (17, 4, always 0), network open
(34, 0x38). Hard disk times come from the host with one-second resolution;
creation is the host's inode change time on Unix. `DeletePending` is always
0. Another class: `STATUS_INVALID_INFO_CLASS`.

`NtSetInformationFile`: basic (4: last access and last write of a hard disk
file; 0 and -1 keep a time; Forza's installer stamps each copy and recopies
one over 4 s off), rename (10), disposition (13), position (14), allocation
(19, accepted and ignored), end of file (20), completion (30, the kernel's).

`NtQueryVolumeInformationFile`: volume (1, 0x12 bytes), size (3, 0x18),
device (4, 8: type 2 with `FILE_REMOVABLE_MEDIA` for the disc, 7 for the
disk), attribute (5, 16), full size (7, 0x20).

Layouts are in `xb_fs::types` with tests of each offset.

## Device controls

| Code | Answer |
|---|---|
| `IOCTL_DISK_GET_DRIVE_GEOMETRY` (0x70000) | 24 bytes: one cylinder per sector of the 8 GB disk, FixedMedia, 512 bytes a sector |
| `IOCTL_DISK_GET_PARTITION_INFO` (0x74004) | 32 bytes: the partition's offset and length (the whole disk for 0), recognized |
| `IOCTL_SCSI_PASS_THROUGH_DIRECT` (0x4D014) | see below |
| `FSCTL_LOCK_VOLUME`, `_UNLOCK_`, `_DISMOUNT_` (0x90018, 1C, 20) | success, nothing to do |
| any other | `STATUS_INVALID_DEVICE_REQUEST` |

Disk controls are answered on any file of partitions 0 to 5. The SCSI
pass-through is answered on any file: MODE SENSE(10) of page 0x3E (the Xbox
security page) reports CDFValid, PartitionArea 1 and Authentication, without
which XAPI reboots to the dashboard (Panzer Dragoon Orta); READ DVD
STRUCTURE of the security sector (0xAD, address 0xFF02FDFF, format 0xFE) is
filled by the kernel, which signs one from the title's certificate; other
commands read zeros with a good status.

## Timing

The console's drives: the DVD drive at 5x takes 500 us a request plus the
transfer at 6900 kB/s; the hard disk 200 us plus 20000 kB/s. Three modes:
off, a quarter of the time (125 us + 27600 kB/s, 50 us + 80000 kB/s), and the
drives' own, which is the default. KotOR II and the Prince of Persia trilogy
need the drives' own whatever the setting (`title_timing_mode`).

`DeviceTiming::cost(kind, bytes)` gives the time; the kernel applies it:

- An overlapped read always takes its cost, in any mode (Total Immersion
  Racing breaks if its first read has landed by the time it polls).
- With timing on, a synchronous read takes the cost of the pages the drive
  transfers unless the file's cached window holds them: each file remembers
  the 4 KiB pages its last timed read brought in (`File::cache_holds`,
  `File::cache_fill`). (Morrowind's loading thread needs the load to wait.)
- Writes take no drive time.

## What is not here

Handles and `FILE_OBJECT`s, guest structures, events, APCs and completion
ports, the I/O worker threads and completion order, alertable waits, the
close-time sync thread, scatter/gather, savestates and disc overlays (mods)
are outside this crate; the first group is `xb-kernel`'s, the last two are
not implemented.
