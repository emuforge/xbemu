//! What this crate needs from the host that `std` words differently per
//! platform: positional file I/O, inode numbers, change times and free
//! space. Everything host-specific in the crate is here.

use std::fs::{File, Metadata};
use std::io;
use std::path::Path;

/// Reads at `offset` until `buf` is full or the file ends; returns the bytes
/// read. Does not move any file position, so reads of one file from several
/// threads do not wait for one another.
pub(crate) fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    let mut done = 0;
    while done < buf.len() {
        match read_once(file, &mut buf[done..], offset + done as u64) {
            Ok(0) => break,
            Ok(n) => done += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(done)
}

/// Writes all of `data` at `offset`.
pub(crate) fn write_all_at(file: &File, data: &[u8], offset: u64) -> io::Result<()> {
    let mut done = 0;
    while done < data.len() {
        match write_once(file, &data[done..], offset + done as u64) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => done += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn read_once(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(file, buf, offset)
}

#[cfg(unix)]
fn write_once(file: &File, data: &[u8], offset: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::write_at(file, data, offset)
}

#[cfg(windows)]
fn read_once(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(file, buf, offset)
}

#[cfg(windows)]
fn write_once(file: &File, data: &[u8], offset: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_write(file, data, offset)
}

/// Hosts with neither call: seek and read under one lock.
#[cfg(not(any(unix, windows)))]
static SEEK_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(not(any(unix, windows)))]
fn read_once(mut file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::io::{Read, Seek, SeekFrom};
    let _guard = SEEK_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    file.seek(SeekFrom::Start(offset))?;
    file.read(buf)
}

#[cfg(not(any(unix, windows)))]
fn write_once(mut file: &File, data: &[u8], offset: u64) -> io::Result<usize> {
    use std::io::{Seek, SeekFrom, Write};
    let _guard = SEEK_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    file.seek(SeekFrom::Start(offset))?;
    file.write(data)
}

/// A number that identifies the file on its file system and orders files
/// stably: the inode where there is one, else 0.
pub(crate) fn inode(md: &Metadata) -> u64 {
    #[cfg(unix)]
    {
        std::os::unix::fs::MetadataExt::ino(md)
    }
    #[cfg(not(unix))]
    {
        let _ = md;
        0
    }
}

/// The time reported as a file's creation time, in Unix seconds. The
/// reference reports `st_ctime` (the inode's change time) on Unix hosts.
pub(crate) fn creation_secs(md: &Metadata) -> i64 {
    #[cfg(unix)]
    {
        std::os::unix::fs::MetadataExt::ctime(md)
    }
    #[cfg(not(unix))]
    {
        md.created().or_else(|_| md.modified()).map(unix_secs).unwrap_or(0)
    }
}

/// Whole Unix seconds of a host time stamp.
pub(crate) fn unix_secs(t: std::time::SystemTime) -> i64 {
    match t.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64) - i64::from(e.duration().subsec_nanos() != 0),
    }
}

/// Bytes free for an ordinary user on the file system holding `path`: `None`
/// where the host has no call for it here (the caller then does not limit a
/// partition's free space by the host's), `Some(0)` when the call fails, as
/// the reference has it.
pub(crate) fn free_bytes(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        Some(rustix::fs::statvfs(path).map(|v| v.f_bavail.saturating_mul(v.f_frsize)).unwrap_or(0))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}
