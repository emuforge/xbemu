//! Random-access byte sources.
//!
//! A [`BlockSource`] is what every reader in this crate sits on: a length and
//! a positional read. There is no cursor, so one source serves several
//! threads at once without a lock.

use std::fs::File;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::{Error, Result};

/// Random access to a fixed run of bytes.
///
/// Implementations are positional (no shared seek cursor), so `read_at` may be
/// called from several threads at once.
pub trait BlockSource: Send + Sync {
    /// Size of the source in bytes.
    fn len(&self) -> u64;

    /// True when the source holds no bytes.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Fills all of `buf` with the bytes at `offset`.
    ///
    /// The read is exact: a range that is not wholly inside the source is
    /// [`Error::OutOfRange`] and `buf`'s contents are then unspecified.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()>;

    /// Fills all of `buf` with the bytes at `offset`, with zeros for the part
    /// at or past the source's end.
    ///
    /// This is how a disc image is read, as the reference does: a tool
    /// trimming an XISO may cut it inside its last file's last sector, or
    /// before data a title reads anyway, and the disc has zeros there.
    fn read_padded(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let size = self.len();
        let inside =
            usize::try_from(size.saturating_sub(offset)).map_or(buf.len(), |n| n.min(buf.len()));
        let (head, tail) = buf.split_at_mut(inside);
        if !head.is_empty() {
            self.read_at(offset, head)?;
        }
        tail.fill(0);
        Ok(())
    }
}

/// Checks that `[offset, offset + len)` lies inside a source of `size` bytes.
pub(crate) fn check_range(offset: u64, len: usize, size: u64) -> Result<()> {
    let len = len as u64;
    if offset > size || len > size - offset {
        return Err(Error::OutOfRange { offset, len, size });
    }
    Ok(())
}

impl<T: BlockSource + ?Sized> BlockSource for &T {
    fn len(&self) -> u64 {
        (**self).len()
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        (**self).read_at(offset, buf)
    }
    fn read_padded(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        (**self).read_padded(offset, buf)
    }
}

impl<T: BlockSource + ?Sized> BlockSource for Box<T> {
    fn len(&self) -> u64 {
        (**self).len()
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        (**self).read_at(offset, buf)
    }
    fn read_padded(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        (**self).read_padded(offset, buf)
    }
}

impl<T: BlockSource + ?Sized> BlockSource for Arc<T> {
    fn len(&self) -> u64 {
        (**self).len()
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        (**self).read_at(offset, buf)
    }
    fn read_padded(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        (**self).read_padded(offset, buf)
    }
}

/// Positional reads on a host file, one implementation per host family.
mod host {
    use std::fs::File;
    use std::io;

    /// A file read by position.
    #[cfg(unix)]
    pub(super) struct PosFile(File);

    #[cfg(unix)]
    impl PosFile {
        pub(super) fn new(file: File) -> Self {
            Self(file)
        }

        /// One `pread`: up to `buf.len()` bytes at `offset`.
        pub(super) fn read_some(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
            std::os::unix::fs::FileExt::read_at(&self.0, buf, offset)
        }
    }

    /// A file read by position.
    #[cfg(windows)]
    pub(super) struct PosFile(File);

    #[cfg(windows)]
    impl PosFile {
        pub(super) fn new(file: File) -> Self {
            Self(file)
        }

        /// One overlapped read: up to `buf.len()` bytes at `offset`. It moves
        /// the handle's cursor, which nothing here uses.
        pub(super) fn read_some(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
            std::os::windows::fs::FileExt::seek_read(&self.0, buf, offset)
        }
    }

    /// A file read by position. This host has no positional read, so a lock
    /// keeps each seek with its read.
    #[cfg(not(any(unix, windows)))]
    pub(super) struct PosFile(std::sync::Mutex<File>);

    #[cfg(not(any(unix, windows)))]
    impl PosFile {
        pub(super) fn new(file: File) -> Self {
            Self(std::sync::Mutex::new(file))
        }

        /// A seek and a read under the lock.
        pub(super) fn read_some(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
            use std::io::{Read, Seek, SeekFrom};
            let mut f = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            f.seek(SeekFrom::Start(offset))?;
            f.read(buf)
        }
    }
}

/// A host file as a block source.
///
/// The size is taken once, at open. Reads are positional, so the source is
/// shared between threads freely.
pub struct FileSource {
    file: host::PosFile,
    len: u64,
}

impl FileSource {
    /// Failed reads tolerated in a row before the read is given up.
    const RETRIES: u32 = 4;
    /// Wait between two tries of a failed read.
    const RETRY_WAIT: Duration = Duration::from_millis(20);

    /// Opens `path` read-only.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::from_file(File::open(path)?)
    }

    /// Wraps an open file.
    pub fn from_file(file: File) -> Result<Self> {
        let len = file.metadata()?.len();
        Ok(Self { file: host::PosFile::new(file), len })
    }
}

impl BlockSource for FileSource {
    fn len(&self) -> u64 {
        self.len
    }

    /// Reads until `buf` is full. A failing read is tried again a few times
    /// with a short wait, as the reference does (a drive spinning up, a
    /// scanner holding the file); a read that still fails is the title's disc
    /// error.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        check_range(offset, buf.len(), self.len)?;
        let mut done = 0;
        let mut tries = 0;
        while done < buf.len() {
            match self.file.read_some(&mut buf[done..], offset + done as u64) {
                Ok(0) => {
                    // The file shrank since it was opened.
                    return Err(Error::Io(io::ErrorKind::UnexpectedEof.into()));
                }
                Ok(n) => {
                    done += n;
                    tries = 0;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    tries += 1;
                    if tries > Self::RETRIES {
                        log::info!(target: "disc", "reading {} bytes at {:#x} failed: {e}", buf.len() - done, offset + done as u64);
                        return Err(Error::Io(e));
                    }
                    std::thread::sleep(Self::RETRY_WAIT);
                }
            }
        }
        Ok(())
    }
}

/// Bytes in memory as a block source.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MemSource(pub Vec<u8>);

impl BlockSource for MemSource {
    fn len(&self) -> u64 {
        self.0.len() as u64
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        check_range(offset, buf.len(), self.len())?;
        // In range, so the offset fits a usize.
        let at = offset as usize;
        buf.copy_from_slice(&self.0[at..at + buf.len()]);
        Ok(())
    }
}

/// A window of another source: `len` bytes starting at `start`.
///
/// This is how a file on a disc is handed to a parser (see
/// [`crate::xdvdfs::Xdvdfs::file_source`]). Bytes of the window that lie past
/// the inner source's end read as zeros, as a trimmed disc image's do.
#[derive(Clone, Debug)]
pub struct SubSource<S> {
    inner: S,
    start: u64,
    len: u64,
}

impl<S: BlockSource> SubSource<S> {
    /// The window `[start, start + len)` of `inner`.
    pub fn new(inner: S, start: u64, len: u64) -> Self {
        Self { inner, start, len }
    }

    /// Offset of the window in the inner source.
    pub fn start(&self) -> u64 {
        self.start
    }
}

impl<S: BlockSource> BlockSource for SubSource<S> {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        check_range(offset, buf.len(), self.len)?;
        let at = self.start.checked_add(offset).ok_or(Error::OutOfRange {
            offset,
            len: buf.len() as u64,
            size: self.len,
        })?;
        self.inner.read_padded(at, buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn a_file_source_reads_any_range_of_its_file_and_refuses_ranges_outside_it() {
        let dir = TempDir::new("block");
        let bytes: Vec<u8> = (0..10_000u32).map(|i| (i * 7 + i / 256) as u8).collect();
        let path = dir.path().join("f.bin");
        std::fs::write(&path, &bytes).unwrap();
        let src = FileSource::open(&path).unwrap();
        assert_eq!(src.len(), bytes.len() as u64);
        for (off, len) in [(0usize, 10_000usize), (1, 1), (9_999, 1), (4_000, 4_096), (10_000, 0)] {
            let mut buf = vec![0xAA; len];
            src.read_at(off as u64, &mut buf).unwrap();
            assert_eq!(buf, &bytes[off..off + len]);
        }
        let mut buf = [0u8; 2];
        assert!(matches!(src.read_at(9_999, &mut buf), Err(Error::OutOfRange { .. })));
        assert!(matches!(src.read_at(u64::MAX, &mut buf), Err(Error::OutOfRange { .. })));
    }

    #[test]
    fn a_padded_read_gives_the_bytes_inside_and_zeros_past_the_end() {
        let src = MemSource((1..=10).collect());
        let mut buf = [0xAAu8; 8];
        src.read_padded(6, &mut buf).unwrap();
        assert_eq!(buf, [7, 8, 9, 10, 0, 0, 0, 0]);
        let mut buf = [0xAAu8; 4];
        src.read_padded(1 << 40, &mut buf).unwrap();
        assert_eq!(buf, [0; 4]);
        src.read_padded(u64::MAX, &mut buf).unwrap();
        assert_eq!(buf, [0; 4]);
    }

    #[test]
    fn a_window_reads_relative_to_its_start_and_is_strict_about_its_own_length() {
        let src = MemSource((0..100).collect());
        let win = SubSource::new(&src, 90, 20);
        let mut buf = [0xAAu8; 20];
        win.read_at(0, &mut buf).unwrap();
        // The window runs 10 bytes past the inner source: zeros there.
        assert_eq!(&buf[..10], &(90..100).collect::<Vec<u8>>()[..]);
        assert_eq!(&buf[10..], &[0; 10]);
        assert!(matches!(win.read_at(1, &mut buf), Err(Error::OutOfRange { .. })));
        let far = SubSource::new(&src, u64::MAX, 20);
        assert!(far.read_at(5, &mut buf[..1]).is_err());
    }

    #[test]
    fn one_file_source_serves_several_threads_at_once() {
        let dir = TempDir::new("block-threads");
        let bytes: Vec<u8> = (0..200_000u32).map(|i| (i ^ (i >> 8)) as u8).collect();
        let path = dir.path().join("f.bin");
        std::fs::write(&path, &bytes).unwrap();
        let src = FileSource::open(&path).unwrap();
        std::thread::scope(|s| {
            for t in 0..4usize {
                let (src, bytes) = (&src, &bytes);
                s.spawn(move || {
                    let mut buf = [0u8; 777];
                    for k in 0..200usize {
                        let off = (t * 31_337 + k * 911) % (bytes.len() - buf.len());
                        src.read_at(off as u64, &mut buf).unwrap();
                        assert_eq!(&buf[..], &bytes[off..off + buf.len()]);
                    }
                });
            }
        });
    }
}
