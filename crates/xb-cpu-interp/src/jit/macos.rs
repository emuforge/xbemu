//! Executable memory on macOS (Apple silicon): one `MAP_JIT` region.
//!
//! A `MAP_JIT` mapping is writable or executable per thread, never both:
//! `pthread_jit_write_protect_np(0)` lets the calling thread write it,
//! `(1)` lets it execute again. New code is then flushed from the
//! instruction cache (`sys_icache_invalidate`). Code is written and run on
//! the guest-CPU thread only.
//!
//! A release signed with the hardened runtime needs the
//! `com.apple.security.cs.allow-jit` entitlement for the mapping to succeed;
//! without it [`CodeBuf::new`] fails and the interpreter runs alone.
#![expect(unsafe_code, reason = "maps, writes and unmaps executable memory")]

use std::io;

unsafe extern "C" {
    /// `<libkern/OSCacheControl.h>`: makes `len` bytes at `start` visible
    /// to instruction fetch.
    fn sys_icache_invalidate(start: *mut libc::c_void, len: usize);
}

/// A region of executable memory filled from its start.
pub(super) struct CodeBuf {
    base: *mut u8,
    len: usize,
    used: usize,
}

// SAFETY: the mapping belongs to this value alone and has no thread
// affinity; write access is switched per thread around each write.
unsafe impl Send for CodeBuf {}

impl CodeBuf {
    /// Maps `len` bytes for generated code.
    pub(super) fn new(len: usize) -> io::Result<Self> {
        // SAFETY: a fresh anonymous mapping at an address the kernel picks;
        // it aliases nothing.
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
                libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_JIT,
                -1,
                0,
            )
        };
        if p == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { base: p.cast(), len, used: 0 })
    }

    /// Appends `code` and returns its address, or `None` when it does not
    /// fit.
    pub(super) fn push(&mut self, code: &[u32]) -> Option<usize> {
        let bytes = std::mem::size_of_val(code);
        if bytes > self.len - self.used {
            return None;
        }
        // SAFETY: `used + bytes <= len`, so the destination lies inside the
        // mapping; `code` is a distinct allocation. The mapping is 16 KiB
        // aligned and `used` a multiple of 4, so the words are aligned.
        // Writing is allowed between the two protection calls, which affect
        // this thread only, and no generated code runs on it meanwhile.
        unsafe {
            let at = self.base.add(self.used);
            libc::pthread_jit_write_protect_np(0);
            std::ptr::copy_nonoverlapping(code.as_ptr().cast::<u8>(), at, bytes);
            libc::pthread_jit_write_protect_np(1);
            sys_icache_invalidate(at.cast(), bytes);
            self.used += bytes;
            Some(at as usize)
        }
    }

    /// Forgets all code. Nothing may still hold an address from
    /// [`CodeBuf::push`].
    pub(super) fn reset(&mut self) {
        self.used = 0;
    }
}

impl Drop for CodeBuf {
    fn drop(&mut self) {
        // SAFETY: exactly the mapping made in `new`; the owner has dropped
        // every address into it.
        unsafe {
            libc::munmap(self.base.cast(), self.len);
        }
    }
}
