//! Linux x86-64: `mmap` for the guest's address space, signals for faults,
//! `GS` as the segment register guest memory is addressed through.
//!
//! The memory layout is a `PROT_NONE` reservation with views of the RAM memfd
//! mapped over it. The guest is not at host address 0: translated code
//! reaches guest address `x` as `gs:[x]` with the reservation's base in
//! `GS`'s base. 64-bit Linux userspace leaves `GS` unused (thread-local
//! storage is `FS`).

use std::ffi::c_void;
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};
use std::ptr;
use std::sync::Once;
use std::sync::atomic::{AtomicUsize, Ordering};

use xb_cpu::Access;

use crate::frame::Frame;

/// Unmapped bytes kept on each side of the guest's 4 GiB. An access that
/// runs off either end (a string instruction walking past the top, a
/// multi-byte operand at `0xFFFF_FFFF`) faults there instead of touching
/// host memory.
const GUARD: usize = 1 << 20;
const FOUR_GIB: usize = 1 << 32;

fn os_error(what: &str) -> String {
    format!("{what}: {}", std::io::Error::last_os_error())
}

fn prot_of(access: Access) -> libc::c_int {
    match access {
        Access::None => libc::PROT_NONE,
        Access::Read => libc::PROT_READ,
        Access::ReadWrite => libc::PROT_READ | libc::PROT_WRITE,
    }
}

/// The guest's 4 GiB as a host reservation, and the host's own view of RAM.
pub(crate) struct GuestSpace {
    /// Start of the whole reservation (guards included).
    reservation: *mut u8,
    reservation_len: usize,
    /// Host address of guest address 0; a multiple of 4 GiB, so the low 32
    /// bits of a host address inside the space are the guest address.
    base: *mut u8,
    /// All of RAM, readable and writable whatever the guest's protections.
    phys: *mut u8,
    ram: OwnedFd,
    ram_size: u32,
}

// SAFETY: the raw pointers are addresses of mappings this value owns; the
// mapping calls themselves are thread safe. Callers serialize changes to one
// page range (the `Mutex` in `mem::Memory`).
unsafe impl Send for GuestSpace {}
// SAFETY: as above; shared access only reads the fields.
unsafe impl Sync for GuestSpace {}

impl GuestSpace {
    /// Reserves the address space and maps the host's view of RAM.
    pub(crate) fn new(ram_fd: BorrowedFd<'_>, ram_size: u32) -> Result<Self, String> {
        if ram_size == 0 || ram_size & 0xFFF != 0 {
            return Err(format!("RAM size {ram_size:#x} is not a positive multiple of 4 KiB"));
        }
        let ram =
            ram_fd.try_clone_to_owned().map_err(|e| format!("duplicating the RAM fd: {e}"))?;
        // Three times 4 GiB always contains a 4 GiB-aligned 4 GiB with room
        // for a guard below it.
        let want = 3 * FOUR_GIB;
        // SAFETY: an anonymous reservation at an address the kernel chooses.
        let r = unsafe {
            libc::mmap(
                ptr::null_mut(),
                want,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        if r == libc::MAP_FAILED {
            return Err(os_error("reserving the guest address space"));
        }
        let r = r as usize;
        let base = (r + GUARD).next_multiple_of(FOUR_GIB);
        let lo = base - GUARD;
        let hi = base + FOUR_GIB + GUARD;
        // SAFETY: both ranges lie inside the reservation just made and are
        // not used for anything yet.
        unsafe {
            if lo > r {
                libc::munmap(r as *mut c_void, lo - r);
            }
            if r + want > hi {
                libc::munmap(hi as *mut c_void, r + want - hi);
            }
        }
        // SAFETY: a fresh shared mapping of the caller's memory object.
        let phys = unsafe {
            libc::mmap(
                ptr::null_mut(),
                ram_size as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                ram.as_raw_fd(),
                0,
            )
        };
        if phys == libc::MAP_FAILED {
            let e = os_error("mapping guest RAM");
            // SAFETY: the reservation made above, not yet shared.
            unsafe { libc::munmap(lo as *mut c_void, hi - lo) };
            return Err(e);
        }
        Ok(Self {
            reservation: lo as *mut u8,
            reservation_len: hi - lo,
            base: base as *mut u8,
            phys: phys.cast(),
            ram,
            ram_size,
        })
    }

    /// Host address of guest address 0.
    pub(crate) fn base(&self) -> usize {
        self.base as usize
    }

    /// The host's never-faulting view of RAM.
    pub(crate) fn phys(&self) -> *mut u8 {
        self.phys
    }

    /// Size of RAM in bytes.
    pub(crate) fn ram_size(&self) -> u32 {
        self.ram_size
    }

    /// The guest address a faulting host address stands for, if it lies in
    /// the reservation. Addresses in the guards wrap, as the guest's would.
    pub(crate) fn guest_of(&self, host: usize) -> Option<u32> {
        let lo = self.reservation as usize;
        (host >= lo && host < lo + self.reservation_len)
            .then(|| host.wrapping_sub(self.base as usize) as u32)
    }

    /// Maps RAM `[pa, pa + len)` at guest `[va, va + len)`.
    pub(crate) fn map(&self, va: u32, pa: u32, len: u32, access: Access) -> Result<(), String> {
        // SAFETY: the target range lies inside the reservation (`va + len`
        // cannot pass 4 GiB: the caller checked), so MAP_FIXED replaces only
        // this space's own pages.
        let m = unsafe {
            libc::mmap(
                self.base.add(va as usize).cast(),
                len as usize,
                prot_of(access),
                libc::MAP_SHARED | libc::MAP_FIXED,
                self.ram.as_raw_fd(),
                libc::off_t::from(pa),
            )
        };
        if m == libc::MAP_FAILED {
            return Err(os_error("mapping a view of guest RAM"));
        }
        Ok(())
    }

    /// Returns guest `[va, va + len)` to the inaccessible reservation.
    pub(crate) fn unmap(&self, va: u32, len: u32) -> Result<(), String> {
        // SAFETY: as in `map`: the range is this space's own.
        let m = unsafe {
            libc::mmap(
                self.base.add(va as usize).cast(),
                len as usize,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        if m == libc::MAP_FAILED {
            return Err(os_error("unmapping guest pages"));
        }
        Ok(())
    }

    /// Changes the protection of mapped guest pages.
    pub(crate) fn protect(&self, va: u32, len: usize, access: Access) {
        // SAFETY: the range is inside the reservation; changing protection of
        // this space's own pages affects only guest accesses.
        let r = unsafe { libc::mprotect(self.base.add(va as usize).cast(), len, prot_of(access)) };
        if r != 0 {
            log::error!(target: "cpu", "mprotect of guest {va:#010x}+{len:#x} failed: {}",
                std::io::Error::last_os_error());
        }
    }
}

impl Drop for GuestSpace {
    fn drop(&mut self) {
        // SAFETY: both mappings were made by `new` and nothing refers to them
        // once the owner is gone.
        unsafe {
            libc::munmap(self.phys.cast(), self.ram_size as usize);
            libc::munmap(self.reservation.cast(), self.reservation_len);
        }
    }
}

/// Readable, writable and executable memory for translated code.
pub(crate) struct ExecRegion {
    ptr: *mut u8,
    len: usize,
}

// SAFETY: an address range this value owns.
unsafe impl Send for ExecRegion {}

impl ExecRegion {
    /// Allocates `len` bytes; pages are committed as they are first written.
    pub(crate) fn new(len: usize) -> Result<Self, String> {
        // SAFETY: a fresh anonymous mapping.
        let p = unsafe {
            libc::mmap(
                ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        if p == libc::MAP_FAILED {
            return Err(os_error("allocating executable memory for translated code"));
        }
        Ok(Self { ptr: p.cast(), len })
    }

    /// Start of the region.
    pub(crate) fn ptr(&self) -> *mut u8 {
        self.ptr
    }

    /// Size in bytes.
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// Gives the pages of `[off, off + len)` back to the kernel (their
    /// contents become zero); used when the translation cache is flushed.
    pub(crate) fn discard(&self, off: usize, len: usize) {
        let start = off.next_multiple_of(4096);
        let end = (off + len) & !4095;
        if end > start {
            // SAFETY: a page-aligned range inside the region; the caller no
            // longer uses the code there.
            unsafe { libc::madvise(self.ptr.add(start).cast(), end - start, libc::MADV_DONTNEED) };
        }
    }
}

impl Drop for ExecRegion {
    fn drop(&mut self) {
        // SAFETY: the mapping made by `new`.
        unsafe { libc::munmap(self.ptr.cast(), self.len) };
    }
}

/// The stack translated code runs on, with an inaccessible guard below it.
///
/// Translated code only pushes return predictions here (see `translate`), but
/// a guest can make calls it never returns from; the guard turns the overflow
/// into a fault the handler answers by emptying the stack.
pub(crate) struct ShadowStack {
    ptr: *mut u8,
    len: usize,
}

// SAFETY: an address range this value owns.
unsafe impl Send for ShadowStack {}

impl ShadowStack {
    const GUARD: usize = 64 << 10;
    const SIZE: usize = 2 << 20;

    /// Allocates the stack.
    pub(crate) fn new() -> Result<Self, String> {
        let len = Self::GUARD + Self::SIZE;
        // SAFETY: a fresh anonymous mapping, then a protection change of its
        // first pages.
        let p = unsafe {
            let p = libc::mmap(
                ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            );
            if p == libc::MAP_FAILED {
                return Err(os_error("allocating the translated code's stack"));
            }
            libc::mprotect(p, Self::GUARD, libc::PROT_NONE);
            p
        };
        Ok(Self { ptr: p.cast(), len })
    }

    /// `[lo, hi)` of the guard.
    pub(crate) fn guard(&self) -> (usize, usize) {
        (self.ptr as usize, self.ptr as usize + Self::GUARD)
    }

    /// One past the highest usable byte (16-byte aligned).
    pub(crate) fn top(&self) -> usize {
        self.ptr as usize + self.len
    }
}

impl Drop for ShadowStack {
    fn drop(&mut self) {
        // SAFETY: the mapping made by `new`.
        unsafe { libc::munmap(self.ptr.cast(), self.len) };
    }
}

/// Whether translated code can set `GS`'s base itself (`WRGSBASE`: the
/// processor has FSGSBASE and the kernel enabled it for user mode).
pub(crate) fn has_fast_segment_base() -> bool {
    const HWCAP2_FSGSBASE: libc::c_ulong = 1 << 1;
    // SAFETY: getauxval reads the process's auxiliary vector.
    unsafe { libc::getauxval(libc::AT_HWCAP2) & HWCAP2_FSGSBASE != 0 }
}

/// Sets the calling thread's `GS` base through the kernel, for hosts where
/// [`has_fast_segment_base`] is false.
pub(crate) fn set_guest_segment_base(base: usize) -> Result<(), String> {
    const ARCH_SET_GS: libc::c_int = 0x1001;
    // SAFETY: arch_prctl(ARCH_SET_GS) changes only this thread's GS base,
    // which nothing else in a 64-bit Linux process uses.
    let r = unsafe { libc::syscall(libc::SYS_arch_prctl, ARCH_SET_GS, base) };
    if r != 0 {
        return Err(os_error("arch_prctl(ARCH_SET_GS)"));
    }
    Ok(())
}

/// Makes sure the calling thread has an alternate signal stack. The fault
/// handler asks for one (`SA_ONSTACK`) because a fault can arrive while the
/// thread's stack pointer is at the very end of a [`ShadowStack`].
pub(crate) fn ensure_signal_stack() {
    thread_local! {
        static DONE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    if DONE.replace(true) {
        return;
    }
    const SIZE: usize = 64 << 10;
    // SAFETY: querying the current alternate stack, and installing a fresh
    // mapping as one if the thread has none. The mapping is never freed: it
    // must outlive every signal the thread can take.
    unsafe {
        let mut old: libc::stack_t = std::mem::zeroed();
        if libc::sigaltstack(ptr::null(), &mut old) != 0 || old.ss_flags & libc::SS_DISABLE == 0 {
            return;
        }
        let p = libc::mmap(
            ptr::null_mut(),
            SIZE,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        );
        if p == libc::MAP_FAILED {
            return;
        }
        let ss = libc::stack_t { ss_sp: p, ss_flags: 0, ss_size: SIZE };
        libc::sigaltstack(&ss, ptr::null_mut());
    }
}

/// What the fault handler needs to know about one translator. The ranges are
/// atomics in a static table so the handler can test a faulting `rip`
/// against them from any thread without touching memory a translator owns.
struct Slot {
    /// Translated code: blocks and the single-step thunk. `[lo, hi)`; `hi`
    /// is 0 for a free slot.
    code_lo: AtomicUsize,
    code_hi: AtomicUsize,
    /// The stubs, where only the stack's guard can fault.
    stubs_lo: AtomicUsize,
    stubs_hi: AtomicUsize,
    guard_lo: AtomicUsize,
    guard_hi: AtomicUsize,
    stack_top: AtomicUsize,
    fault_stub: AtomicUsize,
    frame: AtomicUsize,
}

impl Slot {
    const fn new() -> Self {
        Self {
            code_lo: AtomicUsize::new(0),
            code_hi: AtomicUsize::new(0),
            stubs_lo: AtomicUsize::new(0),
            stubs_hi: AtomicUsize::new(0),
            guard_lo: AtomicUsize::new(0),
            guard_hi: AtomicUsize::new(0),
            stack_top: AtomicUsize::new(0),
            fault_stub: AtomicUsize::new(0),
            frame: AtomicUsize::new(0),
        }
    }
}

const SLOTS: usize = 16;
static REGISTRY: [Slot; SLOTS] = [const { Slot::new() }; SLOTS];
/// Guards slot allocation (never taken by the handler).
static REGISTRY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// One translator's entry in the fault handler's table.
pub(crate) struct CpuRegistration {
    /// Blocks and the single-step thunk, `[lo, hi)`.
    pub(crate) code: (usize, usize),
    /// The stubs, `[lo, hi)`.
    pub(crate) stubs: (usize, usize),
    /// The shadow stack's guard, `[lo, hi)`.
    pub(crate) guard: (usize, usize),
    /// The stack pointer an emptied shadow stack has.
    pub(crate) stack_top: usize,
    /// Where a fault in translated code continues.
    pub(crate) fault_stub: usize,
    /// The frame the fault is recorded in.
    pub(crate) frame: *mut Frame,
}

/// Adds a translator to the fault handler's table. Returns its slot.
pub(crate) fn register_cpu(r: &CpuRegistration) -> Result<usize, String> {
    install_fault_handler()?;
    let _g = REGISTRY_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    for (i, s) in REGISTRY.iter().enumerate() {
        if s.code_hi.load(Ordering::Relaxed) != 0 {
            continue;
        }
        s.stubs_lo.store(r.stubs.0, Ordering::Relaxed);
        s.stubs_hi.store(r.stubs.1, Ordering::Relaxed);
        s.guard_lo.store(r.guard.0, Ordering::Relaxed);
        s.guard_hi.store(r.guard.1, Ordering::Relaxed);
        s.stack_top.store(r.stack_top, Ordering::Relaxed);
        s.fault_stub.store(r.fault_stub, Ordering::Relaxed);
        s.frame.store(r.frame as usize, Ordering::Relaxed);
        s.code_lo.store(r.code.0, Ordering::Relaxed);
        // Last: a nonzero `hi` publishes the slot.
        s.code_hi.store(r.code.1, Ordering::SeqCst);
        return Ok(i);
    }
    Err(format!("more than {SLOTS} translators in one process"))
}

/// Removes a translator from the table. Its code must not be running.
pub(crate) fn unregister_cpu(slot: usize) {
    let _g = REGISTRY_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let s = &REGISTRY[slot];
    s.code_hi.store(0, Ordering::SeqCst);
    s.stubs_hi.store(0, Ordering::SeqCst);
}

const SIGNALS: [libc::c_int; 4] = [libc::SIGSEGV, libc::SIGBUS, libc::SIGFPE, libc::SIGILL];

/// The actions that were installed before ours, one per entry of [`SIGNALS`].
struct OldActions(std::cell::UnsafeCell<[libc::sigaction; 4]>);
// SAFETY: written once, by `sigaction` itself, while the handler is being
// installed; read-only afterwards.
unsafe impl Sync for OldActions {}
// SAFETY: an all-zero `sigaction` is `SIG_DFL` with no flags.
static OLD: OldActions = OldActions(std::cell::UnsafeCell::new(unsafe { std::mem::zeroed() }));

/// Installs the process-wide handler for the signals a fault in translated
/// code raises. Idempotent.
fn install_fault_handler() -> Result<(), String> {
    static ONCE: Once = Once::new();
    static FAILED: AtomicUsize = AtomicUsize::new(0);
    ONCE.call_once(|| {
        for (i, &sig) in SIGNALS.iter().enumerate() {
            // SAFETY: installing a handler that is async-signal-safe (it
            // touches only atomics, the interrupted context and a frame owned
            // by the interrupted thread). The previous action is written
            // straight into `OLD`, which nothing reads before the handler
            // can run.
            unsafe {
                let mut sa: libc::sigaction = std::mem::zeroed();
                sa.sa_sigaction = handler as *const () as usize;
                sa.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK | libc::SA_NODEFER;
                libc::sigemptyset(&mut sa.sa_mask);
                let old = (OLD.0.get() as *mut libc::sigaction).add(i);
                if libc::sigaction(sig, &sa, old) != 0 {
                    FAILED.store(1, Ordering::Relaxed);
                }
            }
        }
    });
    if FAILED.load(Ordering::Relaxed) != 0 {
        return Err("installing the fault handler failed".into());
    }
    Ok(())
}

/// The fault handler.
///
/// A fault whose `rip` is in a registered translator's code is recorded in
/// that translator's frame and execution is redirected to its fault stub,
/// which saves the guest's registers and returns to `X64Cpu::run`; all
/// decisions (device access, write tracking, guest exception) are made there,
/// in ordinary context. A fault on the shadow stack's guard empties the
/// stack and runs the instruction again. Anything else goes to the handler
/// that was installed before.
extern "C" fn handler(sig: libc::c_int, info: *mut libc::siginfo_t, uctx: *mut c_void) {
    // SAFETY: the kernel passes a valid ucontext for an SA_SIGINFO handler.
    let gregs = unsafe { &mut (*uctx.cast::<libc::ucontext_t>()).uc_mcontext.gregs };
    let rip = gregs[libc::REG_RIP as usize] as usize;
    let trapno = gregs[libc::REG_TRAPNO as usize] as u32;
    let cr2 = gregs[libc::REG_CR2 as usize] as usize;
    for s in &REGISTRY {
        let in_code =
            rip >= s.code_lo.load(Ordering::Relaxed) && rip < s.code_hi.load(Ordering::SeqCst);
        let in_stubs =
            rip >= s.stubs_lo.load(Ordering::Relaxed) && rip < s.stubs_hi.load(Ordering::SeqCst);
        if !in_code && !in_stubs {
            continue;
        }
        // The thread is executing this translator's code, so it is the one
        // thread inside its `run` and the slot cannot be released under us.
        if trapno == 14
            && cr2 >= s.guard_lo.load(Ordering::Relaxed)
            && cr2 < s.guard_hi.load(Ordering::Relaxed)
        {
            gregs[libc::REG_RSP as usize] = s.stack_top.load(Ordering::Relaxed) as i64;
            return;
        }
        if !in_code {
            break;
        }
        let frame = s.frame.load(Ordering::Relaxed) as *mut Frame;
        // SAFETY: the frame outlives the registration, and only this thread
        // (the code it interrupted, which is not running now) uses these
        // fields.
        unsafe {
            (*frame).fault_rip = rip as u64;
            (*frame).fault_addr = cr2 as u64;
            (*frame).fault_trapno = trapno;
            (*frame).fault_err = gregs[libc::REG_ERR as usize] as u32;
            (*frame).fault_r13 = gregs[libc::REG_R13 as usize] as u64;
        }
        gregs[libc::REG_RIP as usize] = s.fault_stub.load(Ordering::Relaxed) as i64;
        return;
    }
    chain(sig, info, uctx);
}

/// Hands a fault that is not a translator's to the previous handler.
fn chain(sig: libc::c_int, info: *mut libc::siginfo_t, uctx: *mut c_void) {
    let Some(i) = SIGNALS.iter().position(|&s| s == sig) else { return };
    // SAFETY: `OLD` was filled in by `sigaction` before this handler could
    // run and is not written again.
    let old = unsafe { &*(OLD.0.get() as *const libc::sigaction).add(i) };
    let h = old.sa_sigaction;
    if h == libc::SIG_IGN {
        return;
    }
    if h == libc::SIG_DFL {
        // Put the default action back and return: the instruction faults
        // again and the process ends the way it would have without us.
        // SAFETY: restoring an action the kernel gave us.
        unsafe { libc::sigaction(sig, old, ptr::null_mut()) };
        return;
    }
    if old.sa_flags & libc::SA_SIGINFO != 0 {
        // SAFETY: the previous owner registered this as an SA_SIGINFO handler.
        let f: extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut c_void) =
            unsafe { std::mem::transmute(h) };
        f(sig, info, uctx);
    } else {
        // SAFETY: the previous owner registered this as a plain handler.
        let f: extern "C" fn(libc::c_int) = unsafe { std::mem::transmute(h) };
        f(sig);
    }
}
