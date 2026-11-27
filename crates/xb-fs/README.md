# xb-fs

The storage half of the Xbox kernel's I/O manager: the object namespace for
devices and files, over host directories and a disc. Layer L1.

## Owns

- The namespace: `\Device\CdRom0`, `\Device\Harddisk0\Partition0..9`, the
  symbolic links of `\??` (`D:`, `T:`, `U:`, `Z:`, `CdRom0:`), canonical
  paths, names relative to a root directory (`FileSystem`).
- Open files: positional reads and writes, the file and volume information
  classes as guest bytes, directory enumeration, delete at last close,
  rename, share access (`File`).
- Volumes behind the `Volume` / `VolumeFile` traits: hard disk partitions as
  host folders with case-insensitive names (`HostDisk`), the read-only disc
  over a `DiscTree` someone else implements (`DiscVolume`), a partition in
  memory for tests (`MemVolume`), the raw disk's kept sectors.
- The storage-level device controls (disk geometry, partition info, the DVD
  drive's SCSI pass-through, volume lock/unlock/dismount) as plain functions
  (`ioctl`).
- The drives' timing model as a cost function (`DeviceTiming`).
- `NtStatus`, the create flags, FILETIME conversion and the information
  structures' guest layouts (`types`).

It does not own handles, guest memory, `OBJECT_ATTRIBUTES` and
`IO_STATUS_BLOCK` marshalling, events, APCs, completion ports, I/O worker
threads or waiting out the drives' time: those are `xb-kernel`'s. It does not
parse XDVDFS: `xb-formats` does, and the integrator adapts it to `DiscTree`.

The behavior and its sources are described in `docs/hw/io-namespace.md`.

## Invariants

- `#![forbid(unsafe_code)]`. `unsafe` is not allowed in this crate.
- Depends on no other workspace crate. External crates: `log`, and `rustix`
  on Unix hosts for the host file system's free space.
- `FileSystem`, `File`, `HostDisk` and every `Volume` are `Send + Sync` and
  take `&self`. Reads and writes of one file from several threads are
  positional and do not serialize.
- Nothing here sleeps or waits for a drive. Two background threads exist:
  one measures the partitions' use (a walk over every file), one deletes a
  formatted cache drive's old contents.
- Everything host-specific is in `src/sys.rs` behind `cfg`.
- Dropping a `File` closes it: a file marked for deletion goes when its last
  handle is dropped. Syncing a written file before that is the caller's
  choice (`File::sync_on_close`, `File::flush`).
- A path never escapes the hard disk folder: canonical paths have no `..`,
  and each component is matched against, or created inside, the folder
  reached so far.

## Test

```
cargo test -p xb-fs
```

The tests use folders under the system's temporary directory and remove
them. One test replays a reference boot log of Halo
(`halo-ce.ref.log` in the titles stash, `titles/README.md`) and passes with
a printed note when the log is absent. No test needs a game image.
