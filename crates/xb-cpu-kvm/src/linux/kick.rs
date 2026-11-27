//! Stopping a running vCPU from another thread.
//!
//! `KVM_RUN` returns early only for a signal. A kick therefore sets a flag
//! and, when the CPU thread is inside [`run`](xb_cpu::Cpu::run), sends that
//! thread a real-time signal. The handler runs on the CPU thread and sets
//! `kvm_run.immediate_exit`, which closes the race between "the flag was
//! checked" and "the thread entered the guest": a signal that lands before
//! the `KVM_RUN` system call leaves `immediate_exit` set, so the call returns
//! at once; one that lands during it interrupts the guest.

use std::cell::Cell;
use std::ptr;
use std::sync::Once;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use kvm_bindings::kvm_run;

/// The kick signal is `SIGRTMIN + KICK_SIGNAL_OFFSET`. The process must leave
/// its disposition alone.
pub const KICK_SIGNAL_OFFSET: i32 = 7;

thread_local! {
    /// The `kvm_run` block of the vCPU this thread is running, for the signal
    /// handler. Null outside `run`.
    static RUN_BLOCK: Cell<*mut kvm_run> = const { Cell::new(ptr::null_mut()) };
    /// This thread's kernel thread id (0 until first used), and whether the
    /// kick signal has been unblocked on it.
    static THREAD_ID: Cell<i32> = const { Cell::new(0) };
}

fn signal() -> libc::c_int {
    libc::SIGRTMIN() + KICK_SIGNAL_OFFSET
}

extern "C" fn on_kick(_: libc::c_int) {
    // Only async-signal-safe work: a thread-local read and one byte store.
    let run = RUN_BLOCK.with(Cell::get);
    if !run.is_null() {
        // SAFETY: the pointer is set only while its vCPU's `kvm_run` mapping
        // is alive and this thread is inside `run`; the field is a plain byte
        // the kernel reads at the next `KVM_RUN` entry.
        unsafe { ptr::addr_of_mut!((*run).immediate_exit).write_volatile(1) };
    }
}

/// Installs the process-wide handler for the kick signal.
pub(crate) fn install_handler() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: `sigaction` with a zeroed struct plus a valid handler; the
        // handler is async-signal-safe (see `on_kick`). No SA_RESTART, so
        // `KVM_RUN` returns EINTR.
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = on_kick as extern "C" fn(libc::c_int) as usize;
            libc::sigemptyset(&mut sa.sa_mask);
            let mut old: libc::sigaction = std::mem::zeroed();
            if libc::sigaction(signal(), &sa, &mut old) != 0 {
                log::error!(target: "cpu", "installing the kick signal handler failed: {}",
                    std::io::Error::last_os_error());
            } else if old.sa_sigaction != libc::SIG_DFL {
                log::warn!(target: "cpu", "signal {} already had a handler; KVM kicks replace it",
                    signal());
            }
        }
    });
}

/// State shared between the CPU and its [`xb_cpu::Kicker`] handles.
pub(crate) struct KickState {
    /// A kick is waiting to be reported as `Exit::Kicked`.
    pending: AtomicBool,
    /// Kernel thread id of the thread inside `run`, or 0.
    running: AtomicI32,
    pid: libc::pid_t,
}

impl KickState {
    pub(crate) fn new() -> Self {
        // SAFETY: `getpid` has no preconditions.
        let pid = unsafe { libc::getpid() };
        Self { pending: AtomicBool::new(false), running: AtomicI32::new(0), pid }
    }

    /// Requests a stop. Any thread.
    pub(crate) fn kick(&self) {
        // The flag is stored before `running` is read and `enter` stores
        // `running` before it reads the flag (both SeqCst), so at least one
        // side sees the other.
        self.pending.store(true, Ordering::SeqCst);
        let tid = self.running.load(Ordering::SeqCst);
        super::census::note_kick(tid != 0);
        if tid != 0 {
            // SAFETY: `tgkill` on a thread of this process. If the thread has
            // left `run` meanwhile the handler finds no run block and does
            // nothing.
            unsafe { libc::syscall(libc::SYS_tgkill, self.pid, tid, signal()) };
        }
    }

    /// Takes the pending kick, if any.
    pub(crate) fn take(&self) -> bool {
        self.pending.swap(false, Ordering::SeqCst)
    }

    /// Puts back a kick that could not be reported yet.
    pub(crate) fn restore(&self) {
        self.pending.store(true, Ordering::SeqCst);
    }

    /// Marks the calling thread as running the vCPU whose run block is `run`.
    ///
    /// # Safety
    ///
    /// `run` must stay mapped until [`Self::leave`] is called on this thread.
    pub(crate) unsafe fn enter(&self, run: *mut kvm_run) {
        let tid = THREAD_ID.with(|t| {
            if t.get() == 0 {
                // SAFETY: plain system calls; the signal set is initialized
                // by `sigemptyset` before use.
                unsafe {
                    let mut set: libc::sigset_t = std::mem::zeroed();
                    libc::sigemptyset(&mut set);
                    libc::sigaddset(&mut set, signal());
                    libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, ptr::null_mut());
                    // The system call, not the C library's `gettid`: glibc
                    // has that only since 2.30, above the release build's
                    // floor (`docker/Dockerfile`).
                    t.set(libc::syscall(libc::SYS_gettid) as libc::pid_t);
                }
            }
            t.get()
        });
        RUN_BLOCK.with(|r| r.set(run));
        // A stale request from an earlier `run` must not end this one.
        // SAFETY: `run` is mapped per this function's contract.
        unsafe { ptr::addr_of_mut!((*run).immediate_exit).write_volatile(0) };
        self.running.store(tid, Ordering::SeqCst);
    }

    /// Ends what [`Self::enter`] began.
    pub(crate) fn leave(&self) {
        self.running.store(0, Ordering::SeqCst);
        RUN_BLOCK.with(|r| r.set(ptr::null_mut()));
    }
}
