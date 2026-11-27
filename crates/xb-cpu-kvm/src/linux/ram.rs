//! Host mappings of guest memory and the write-tracking protector.
//!
//! Guest RAM is a memfd owned by `xb-mem`. This backend maps it twice:
//!
//! * the **slot view** is what KVM's memory slot points at. [`Protector`]
//!   changes its page protection with `mprotect`; KVM resolves guest accesses
//!   through this mapping, so a guest access the protection forbids cannot be
//!   mapped into the VM and `KVM_RUN` fails with `EFAULT` instead.
//! * the **host view** is never protected. The backend uses it to read
//!   exception frames and to finish accesses KVM's instruction emulator could
//!   not make through the slot view.

use std::os::fd::{AsRawFd, BorrowedFd};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use xb_types::mem::{Access, PageProtector};

/// Size of a guest (and x86-64 host) page.
pub(crate) const PAGE: u32 = 0x1000;

/// One `mmap` region, unmapped on drop.
pub(crate) struct Mmap {
    ptr: NonNull<u8>,
    len: usize,
}

// SAFETY: the mapping is plain memory owned by this value; every access goes
// through raw pointers whose users uphold their own synchronization.
unsafe impl Send for Mmap {}
// SAFETY: as for `Send`; the type itself offers no shared mutation.
unsafe impl Sync for Mmap {}

impl Mmap {
    /// Maps `len` bytes of zeroed private memory.
    pub(crate) fn anonymous(len: usize) -> Result<Self, String> {
        Self::map(len, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE, -1)
    }

    /// Maps the first `len` bytes of a shared memory object.
    pub(crate) fn shared(fd: BorrowedFd<'_>, len: usize) -> Result<Self, String> {
        Self::map(len, libc::MAP_SHARED, fd.as_raw_fd())
    }

    fn map(len: usize, flags: libc::c_int, fd: libc::c_int) -> Result<Self, String> {
        // SAFETY: a fresh mapping at an address the kernel chooses aliases
        // nothing this process owns.
        let p = unsafe {
            libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, flags, fd, 0)
        };
        if p == libc::MAP_FAILED {
            return Err(format!("mmap of {len:#x} bytes: {}", std::io::Error::last_os_error()));
        }
        NonNull::new(p.cast()).map(|ptr| Self { ptr, len }).ok_or_else(|| "mmap gave null".into())
    }

    /// Start of the mapping.
    pub(crate) fn as_ptr(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    /// Length in bytes.
    pub(crate) fn len(&self) -> usize {
        self.len
    }
}

impl Drop for Mmap {
    fn drop(&mut self) {
        // SAFETY: the region was mapped by `map` with this length and nothing
        // else unmaps it.
        unsafe { libc::munmap(self.ptr.as_ptr().cast(), self.len) };
    }
}

const P_NONE: u8 = 0;
const P_READ: u8 = 1;
const P_RW: u8 = 2;

fn encode(a: Access) -> u8 {
    match a {
        Access::None => P_NONE,
        Access::Read => P_READ,
        Access::ReadWrite => P_RW,
    }
}

/// The guest's access to each physical RAM page, enforced with `mprotect` on
/// the slot view.
///
/// `mprotect` runs KVM's MMU notifier, which removes the page from the VM's
/// second-level page tables and flushes the running vCPU's TLB before the
/// call returns; that is what makes [`PageProtector::protect`] synchronous.
pub(crate) struct Protector {
    slot_view: Mmap,
    /// Current access per page, read without the lock by the CPU thread.
    state: Box<[AtomicU8]>,
    /// Serializes `protect` calls so `state` and the mapping agree.
    lock: Mutex<()>,
}

impl Protector {
    /// Wraps the slot view; every page starts fully accessible.
    pub(crate) fn new(slot_view: Mmap) -> Self {
        let pages = slot_view.len() / PAGE as usize;
        Self {
            slot_view,
            state: (0..pages).map(|_| AtomicU8::new(P_RW)).collect(),
            lock: Mutex::new(()),
        }
    }

    /// Host address of the mapping KVM's RAM slot uses.
    pub(crate) fn slot_ptr(&self) -> *mut u8 {
        self.slot_view.as_ptr()
    }

    /// Every page that is not fully accessible, as `(pa, access)`.
    pub(crate) fn restricted(&self) -> Vec<(u32, Access)> {
        self.state
            .iter()
            .enumerate()
            .filter(|(_, s)| s.load(Ordering::Acquire) != P_RW)
            .map(|(i, _)| (i as u32 * PAGE, self.access(i as u32 * PAGE)))
            .collect()
    }

    /// The guest's access to the page holding `pa`.
    pub(crate) fn access(&self, pa: u32) -> Access {
        match self.state.get((pa / PAGE) as usize).map(|s| s.load(Ordering::Acquire)) {
            Some(P_NONE) => Access::None,
            Some(P_READ) => Access::Read,
            _ => Access::ReadWrite,
        }
    }

    /// Waits for a `protect` call in flight, so a following [`Self::access`]
    /// agrees with the mapping.
    pub(crate) fn settle(&self) {
        drop(self.guard());
    }

    fn guard(&self) -> MutexGuard<'_, ()> {
        self.lock.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl PageProtector for Protector {
    fn protect(&self, pa: u32, len: u32, access: Access) {
        let first = (pa / PAGE) as usize;
        let last = (pa as usize + len as usize).div_ceil(PAGE as usize).min(self.state.len());
        if first >= last {
            return;
        }
        let want = encode(access);
        let _guard = self.guard();
        // Trackers re-protect pages that are already protected; skip the
        // system call for those.
        let census = super::census::enabled();
        if self.state[first..last].iter().all(|s| s.load(Ordering::Relaxed) == want) {
            if census {
                super::census::note_protect(false, 0, 0, false);
            }
            return;
        }
        let t0 = census.then(std::time::Instant::now);
        let prot = match access {
            Access::None => libc::PROT_NONE,
            Access::Read => libc::PROT_READ,
            Access::ReadWrite => libc::PROT_READ | libc::PROT_WRITE,
        };
        for s in &self.state[first..last] {
            s.store(want, Ordering::Release);
        }
        // SAFETY: the range lies inside the slot view, which this value owns.
        // Only the guest (through KVM) uses that mapping, so taking access
        // away cannot fault host code.
        let r = unsafe {
            libc::mprotect(
                self.slot_view.as_ptr().add(first * PAGE as usize).cast(),
                (last - first) * PAGE as usize,
                prot,
            )
        };
        if let Some(t0) = t0 {
            super::census::note_protect(
                true,
                (last - first) as u64,
                t0.elapsed().as_nanos() as u64,
                access == Access::ReadWrite,
            );
        }
        if r != 0 {
            log::error!(
                target: "cpu",
                "mprotect of guest RAM {pa:#x}+{len:#x} failed: {}",
                std::io::Error::last_os_error()
            );
        }
    }
}
