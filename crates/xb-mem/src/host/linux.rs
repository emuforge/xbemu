//! Linux: an anonymous memory file (`memfd_create`) mapped shared.
#![expect(unsafe_code, reason = "system calls and the mapping's lifetime")]

use std::fs::File;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd};
use std::ptr::NonNull;

/// A memory file of fixed length and one read-write shared mapping of all of
/// it, unmapped on drop.
pub(crate) struct SharedMem {
    file: File,
    ptr: NonNull<u8>,
    len: usize,
}

// SAFETY: the mapping is plain shared memory with no thread affinity; the
// pointer is only ever used for bounds-checked raw copies (`ram.rs`).
unsafe impl Send for SharedMem {}
// SAFETY: as above; concurrent access is by raw copies, never references.
unsafe impl Sync for SharedMem {}

impl SharedMem {
    /// Creates a zero-filled memory object of `len` bytes and maps it.
    pub(crate) fn new(len: usize) -> io::Result<Self> {
        // SAFETY: the name is a NUL-terminated string that outlives the call.
        let fd = unsafe { libc::memfd_create(c"xbemu-ram".as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` was just returned by the kernel and nothing else owns it.
        let file = unsafe { File::from_raw_fd(fd) };
        file.set_len(len as u64)?;
        // SAFETY: a fresh mapping at an address the kernel picks; it aliases
        // no Rust object. `file` is a valid descriptor of at least `len` bytes.
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if p == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let ptr = NonNull::new(p.cast::<u8>()).ok_or_else(|| io::Error::other("mmap gave null"))?;
        Ok(Self { file, ptr, len })
    }

    /// Host address of byte 0.
    pub(crate) fn as_ptr(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    /// Length in bytes.
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// The memory object, for mapping further views.
    pub(crate) fn fd(&self) -> BorrowedFd<'_> {
        self.file.as_fd()
    }
}

impl Drop for SharedMem {
    fn drop(&mut self) {
        // SAFETY: `ptr`/`len` are exactly the mapping made in `new`, and no
        // pointer into it outlives `self` (`Ram` hands them out under that
        // contract).
        unsafe {
            libc::munmap(self.ptr.as_ptr().cast(), self.len);
        }
    }
}
