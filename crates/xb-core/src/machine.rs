//! What the machine's parts share: the state every thread may reach, and the
//! implementations of the traits the leaf crates ask the machine for
//! ([`GpuHost`], [`ApuHost`], [`TrackerEvents`], [`RelocateHooks`], the
//! devices' clock and vblank counter).

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};

use xb_apu::{Apu, ApuHost};
use xb_cpu::{Access, Bus, Context, Cpu, Exit, Kicker, Mapper, PageProtector, Regs, RunFlags};
use xb_gpu::{CpuRegs, Gpu, GpuHost, MemEvents};
use xb_kernel::{FrameHold, GuestClock, RunControl, WakeHandle};
use xb_mem::{Ram, RelocateHooks, TrackerEvents, WriteTracker};
use xb_types::irq::IrqHub;
use xb_types::{PAGE_SHIFT, PAGE_SIZE};

/// Locks `m`, going on with the data if a thread panicked holding it.
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

thread_local! {
    /// Set on the guest-CPU thread, for the calls that are only its own
    /// (`GpuHost::guest_hold`).
    static GUEST_THREAD: Cell<bool> = const { Cell::new(false) };
}

/// Marks the calling thread as the guest-CPU thread.
pub(crate) fn mark_guest_thread() {
    GUEST_THREAD.with(|g| g.set(true));
    cpu_time::remember_this_thread();
}

/// The host CPU time the guest-CPU thread has used, in nanoseconds (0 if it
/// is not known or the thread has ended).
pub(crate) fn guest_thread_cpu_ns() -> u64 {
    cpu_time::guest_ns()
}

/// Forgets the guest-CPU thread's clock; call as the thread ends.
pub(crate) fn unmark_guest_thread() {
    cpu_time::forget();
}

/// The guest-CPU thread's CPU time, for the statistics line, read from
/// another thread: on Linux through the thread's CPU-time clock, on macOS
/// through its Mach port (`thread_info`). Elsewhere it is unknown (0).
mod cpu_time {
    use std::sync::atomic::{AtomicU32, Ordering};

    /// The guest-CPU thread's handle (a clock id or a Mach port); 0 before
    /// the thread has started and after it ends.
    static HANDLE: AtomicU32 = AtomicU32::new(0);

    pub(super) fn forget() {
        HANDLE.store(0, Ordering::Relaxed);
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub(super) fn remember_this_thread() {
        let mut clock: libc::clockid_t = 0;
        // SAFETY: `pthread_self` is the calling thread's handle and `clock` a
        // valid place for the result.
        if unsafe { libc::pthread_getcpuclockid(libc::pthread_self(), &raw mut clock) } == 0 {
            HANDLE.store(clock as u32, Ordering::Relaxed);
        }
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub(super) fn guest_ns() -> u64 {
        let clock = HANDLE.load(Ordering::Relaxed);
        if clock == 0 {
            return 0;
        }
        let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: `ts` is a valid, writable timespec. A clock id of a thread
        // that has ended makes the call fail, which is checked.
        if unsafe { libc::clock_gettime(clock as libc::clockid_t, &raw mut ts) } != 0 {
            return 0;
        }
        ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
    }

    #[cfg(target_os = "macos")]
    pub(super) fn remember_this_thread() {
        // SAFETY: `pthread_self` is the calling thread's handle; the port
        // stays valid while the thread lives (`forget` runs as it ends).
        let port = unsafe { libc::pthread_mach_thread_np(libc::pthread_self()) };
        HANDLE.store(port, Ordering::Relaxed);
    }

    #[cfg(target_os = "macos")]
    pub(super) fn guest_ns() -> u64 {
        let port = HANDLE.load(Ordering::Relaxed);
        if port == 0 {
            return 0;
        }
        let mut info = std::mem::MaybeUninit::<libc::thread_basic_info>::zeroed();
        let mut count = libc::THREAD_BASIC_INFO_COUNT;
        // SAFETY: `info` has room for THREAD_BASIC_INFO_COUNT words, which
        // `count` says; a port of an ended thread makes the call fail.
        let r = unsafe {
            libc::thread_info(
                port,
                libc::THREAD_BASIC_INFO as libc::thread_flavor_t,
                info.as_mut_ptr().cast(),
                &raw mut count,
            )
        };
        if r != libc::KERN_SUCCESS {
            return 0;
        }
        // SAFETY: the call succeeded and filled the structure.
        let info = unsafe { info.assume_init() };
        let us = |t: libc::time_value_t| t.seconds as u64 * 1_000_000 + t.microseconds as u64;
        (us(info.user_time) + us(info.system_time)) * 1000
    }

    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
    pub(super) fn remember_this_thread() {}

    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
    pub(super) fn guest_ns() -> u64 {
        let _ = HANDLE.load(Ordering::Relaxed);
        0
    }
}

fn on_guest_thread() -> bool {
    GUEST_THREAD.with(Cell::get)
}

/// `CLOCK_MONOTONIC` in nanoseconds, the clock the renderer exchanges host
/// instants in.
fn monotonic_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `ts` is a valid, writable timespec; the call writes it only.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &raw mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

const PT_PRESENT: u32 = 1 << 0;
const PT_WRITE: u32 = 1 << 1;
const PT_MMIO: u32 = 1 << 2;
const PT_NOACCESS: u32 = 1 << 3;

/// A copy of the guest's page table that any thread may read.
///
/// The kernel's own table (`xb_mem::AddressSpace`) belongs to the guest-CPU
/// thread, but the renderer's threads read guest memory by virtual address
/// (`hle_va_rw`). Every change the kernel makes reaches the CPU backend
/// through `Mapper`; [`MachineCpu`] sits in that path and records it here,
/// so this table is exactly what the guest's processor sees.
pub(crate) struct PageTable {
    entries: Box<[AtomicU32]>,
}

impl PageTable {
    fn new() -> Self {
        Self { entries: (0..1usize << (32 - PAGE_SHIFT)).map(|_| AtomicU32::new(0)).collect() }
    }

    fn set(&self, va: u32, len: u32, entry: impl Fn(u32) -> u32) {
        let first = (va >> PAGE_SHIFT) as usize;
        let n = len.div_ceil(PAGE_SIZE) as usize;
        for (i, e) in self.entries.iter().skip(first).take(n).enumerate() {
            e.store(entry((i as u32) << PAGE_SHIFT), Ordering::Release);
        }
    }

    fn entry(&self, va: u32) -> u32 {
        self.entries[(va >> PAGE_SHIFT) as usize].load(Ordering::Acquire)
    }

    /// The physical address behind `va` if RAM is mapped there.
    pub(crate) fn translate(&self, va: u32) -> Option<u32> {
        let e = self.entry(va);
        (e & (PT_PRESENT | PT_MMIO) == PT_PRESENT)
            .then_some((e & !(PAGE_SIZE - 1)) | (va & (PAGE_SIZE - 1)))
    }

    /// Whether the page table alone forbids the access: a fault on it is the
    /// title's own, not write tracking's. An address that is not RAM answers
    /// `false` (the caller has nothing to forbid by).
    pub(crate) fn forbids(&self, va: u32, pa: u32, write: bool) -> bool {
        let e = self.entry(va);
        if e & (PT_PRESENT | PT_MMIO) != PT_PRESENT || (e ^ pa) >> PAGE_SHIFT != 0 {
            return false;
        }
        e & PT_NOACCESS != 0 || (write && e & PT_WRITE == 0)
    }
}

/// What the CPU wrapper publishes for other threads.
pub(crate) struct CpuShared {
    pub(crate) table: PageTable,
    regs: Mutex<Regs>,
    ran: AtomicBool,
    /// `Exit::Trap` returns: kernel calls and the kernel's return sentinels.
    pub(crate) traps: AtomicU64,
    /// Every return of `Cpu::run`.
    pub(crate) exits: AtomicU64,
}

impl CpuShared {
    pub(crate) fn new() -> Self {
        Self {
            table: PageTable::new(),
            regs: Mutex::new(Regs::default()),
            ran: AtomicBool::new(false),
            traps: AtomicU64::new(0),
            exits: AtomicU64::new(0),
        }
    }
}

/// The CPU backend as the kernel sees it: the real backend, with every
/// mapping change copied into the shared [`PageTable`] and the registers at
/// each exit kept for the renderer's diagnostics.
pub(crate) struct MachineCpu {
    inner: Box<dyn Cpu>,
    shared: Arc<CpuShared>,
}

impl MachineCpu {
    pub(crate) fn new(inner: Box<dyn Cpu>, shared: Arc<CpuShared>) -> Self {
        Self { inner, shared }
    }
}

impl Mapper for MachineCpu {
    fn map(&mut self, va: u32, pa: u32, len: u32, access: Access) {
        let flags = PT_PRESENT
            | match access {
                Access::None => PT_NOACCESS,
                Access::Read => 0,
                Access::ReadWrite => PT_WRITE,
            };
        self.shared.table.set(va, len, |off| (pa + off) | flags);
        self.inner.map(va, pa, len, access);
    }

    fn map_mmio(&mut self, va: u32, pa: u32, len: u32) {
        self.shared.table.set(va, len, |off| (pa + off) | PT_PRESENT | PT_MMIO);
        self.inner.map_mmio(va, pa, len);
    }

    fn unmap(&mut self, va: u32, len: u32) {
        self.shared.table.set(va, len, |_| 0);
        self.inner.unmap(va, len);
    }
}

impl Cpu for MachineCpu {
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    fn run(&mut self, ctx: &mut Context, bus: &mut dyn Bus, flags: RunFlags) -> Exit {
        let exit = self.inner.run(ctx, bus, flags);
        self.shared.exits.fetch_add(1, Ordering::Relaxed);
        if exit == Exit::Trap {
            self.shared.traps.fetch_add(1, Ordering::Relaxed);
        }
        *lock(&self.shared.regs) = ctx.regs;
        self.shared.ran.store(true, Ordering::Relaxed);
        exit
    }

    fn kicker(&self) -> Kicker {
        self.inner.kicker()
    }

    fn protector(&self) -> Arc<dyn PageProtector> {
        self.inner.protector()
    }

    fn set_trap_range(&mut self, va: u32, len: u32) {
        self.inner.set_trap_range(va, len);
    }

    fn invalidate_code(&mut self, pa: u32, len: u32) {
        self.inner.invalidate_code(pa, len);
    }

    fn set_tsc(&mut self, tsc: u64) {
        self.inner.set_tsc(tsc);
    }

    fn read_through(&mut self, pa: u32, len: u32, host: *const u8) -> bool {
        self.inner.read_through(pa, len, host)
    }
}

/// Where the guest-CPU thread is, for the front end's thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    /// Inside `Kernel::run` (or about to be).
    Running,
    /// `Kernel::run` returned `Paused`; the thread waits to be let go.
    Paused,
    /// The title left by itself.
    Exited,
    /// The title cannot continue.
    Fatal(String),
    /// The thread ended because it was asked to.
    Stopped,
}

/// The pause and stop protocol between the front end's thread and the
/// guest-CPU thread.
pub(crate) struct RunState {
    pub(crate) phase: Phase,
    /// The front end wants the guest paused.
    pub(crate) want_pause: bool,
    /// The front end wants the guest-CPU thread to end.
    pub(crate) quit: bool,
    /// A savestate to take or put back at the pause (`state.rs`).
    pub(crate) state_job: Option<crate::state::StateJob>,
}

/// Counters for the once-a-second statistics line.
#[derive(Default)]
pub(crate) struct Counters {
    /// Device register accesses.
    pub(crate) mmio: AtomicU64,
    /// Guest page faults the CPU backend handed to the bus.
    pub(crate) faults: AtomicU64,
    /// Page faults handed on to the title as its own.
    pub(crate) raised: AtomicU64,
    /// The renderer's requests for a frame-latency hold.
    pub(crate) frame_waits: AtomicU64,
    /// Of them, the ones asked of the kernel (`pacing.rs`).
    pub(crate) frame_holds: AtomicU64,
    /// The DirectSound mailbox fixup's waits on the APU.
    pub(crate) mailbox_waits: AtomicU64,
    /// Guest writes to the APU's interrupt status register: its interrupt
    /// handler acknowledging causes.
    pub(crate) apu_acks: AtomicU64,
    /// Guest accesses to the USB controller's registers.
    pub(crate) usb: AtomicU64,
}

/// The machine's state that more than one thread reaches.
pub(crate) struct Shared {
    pub(crate) ram: Ram,
    pub(crate) tracker: Arc<WriteTracker>,
    pub(crate) cpu: Arc<CpuShared>,
    pub(crate) hub: Arc<IrqHub>,
    pub(crate) clock: GuestClock,
    pub(crate) wake: WakeHandle,
    pub(crate) control: RunControl,
    /// The kernel's frame-latency hold (`GpuHost::frame_wait`).
    pub(crate) frame: FrameHold,
    /// The GPU, once created. It lives until the process ends.
    pub(crate) gpu: OnceLock<Gpu>,
    /// The APU while the machine is up. The guest-CPU thread's bus holds its
    /// own reference for register accesses.
    pub(crate) apu: Mutex<Option<Arc<Apu>>>,
    pub(crate) run: Mutex<RunState>,
    pub(crate) run_cv: Condvar,
    pub(crate) counters: Counters,
    /// Nesting depth of `guest_hold` (guest-CPU thread only).
    holds: AtomicU32,
    /// The outermost hold stopped the guest's clock.
    hold_on: AtomicBool,
    /// The frame-latency hold is taken when the renderer asks (`pacing.rs`).
    pub(crate) hold_wanted: AtomicBool,
}

/// The kernel's handles the machine's other threads use.
pub(crate) struct KernelHandles {
    pub(crate) clock: GuestClock,
    pub(crate) wake: WakeHandle,
    pub(crate) control: RunControl,
    pub(crate) frame: FrameHold,
}

impl Shared {
    pub(crate) fn new(
        ram: Ram,
        tracker: Arc<WriteTracker>,
        cpu: Arc<CpuShared>,
        hub: Arc<IrqHub>,
        kernel: KernelHandles,
    ) -> Self {
        let KernelHandles { clock, wake, control, frame } = kernel;
        Self {
            ram,
            tracker,
            cpu,
            hub,
            clock,
            wake,
            control,
            frame,
            gpu: OnceLock::new(),
            apu: Mutex::new(None),
            run: Mutex::new(RunState {
                phase: Phase::Running,
                want_pause: false,
                quit: false,
                state_job: None,
            }),
            run_cv: Condvar::new(),
            counters: Counters::default(),
            holds: AtomicU32::new(0),
            hold_wanted: AtomicBool::new(true),
            hold_on: AtomicBool::new(false),
        }
    }

    /// The APU, if the machine still has one.
    pub(crate) fn apu(&self) -> Option<Arc<Apu>> {
        lock(&self.apu).clone()
    }

    /// The TSC of the console's 733 MHz processor at guest time `ns`.
    pub(crate) fn tsc(&self) -> u64 {
        (u128::from(self.clock.now_ns()) * 733 / 1000) as u64
    }
}

impl GpuHost for Shared {
    fn va_read(&self, va: u32, buf: &mut [u8]) -> bool {
        let mut va = va;
        let mut done = 0;
        while done < buf.len() {
            let n = ((PAGE_SIZE - (va & (PAGE_SIZE - 1))) as usize).min(buf.len() - done);
            let Some(pa) = self.cpu.table.translate(va).filter(|_| va < 0xFD00_0000) else {
                return false;
            };
            // Outside RAM the C skips the copy and goes on.
            self.ram.read(pa, &mut buf[done..done + n]);
            va = va.wrapping_add(n as u32);
            done += n;
        }
        true
    }

    fn va_write(&self, va: u32, data: &[u8]) -> bool {
        let mut va = va;
        let mut done = 0;
        while done < data.len() {
            let n = ((PAGE_SIZE - (va & (PAGE_SIZE - 1))) as usize).min(data.len() - done);
            let Some(pa) = self.cpu.table.translate(va).filter(|_| va < 0xFD00_0000) else {
                return false;
            };
            if self.ram.write(pa, &data[done..done + n]) {
                self.tracker.set_dirty(pa, n as u32);
            }
            va = va.wrapping_add(n as u32);
            done += n;
        }
        true
    }

    fn va_store(&self, va: u32, len: u32) {
        let mut va = va;
        let mut left = len;
        while left != 0 {
            let n = (PAGE_SIZE - (va & (PAGE_SIZE - 1))).min(left);
            if let Some(pa) = self.cpu.table.translate(va) {
                self.tracker.host_store(pa, n);
            }
            va = va.wrapping_add(n);
            left -= n;
        }
    }

    #[inline]
    fn mem_dirty(&self, pa: u32) -> bool {
        self.tracker.dirty(pa)
    }

    #[inline]
    fn mem_open(&self, pa: u32) -> bool {
        self.tracker.open(pa)
    }

    #[inline]
    fn mem_test_and_clear(&self, pa: u32, len: u32) -> bool {
        self.tracker.test_and_clear(pa, len)
    }

    #[inline]
    fn mem_set_dirty(&self, pa: u32, len: u32) {
        self.tracker.set_dirty(pa, len);
    }

    #[inline]
    fn mem_landed(&self, pa: u32, len: u32) {
        self.tracker.landed(pa, len);
    }

    #[inline]
    fn mem_rearm(&self, pa: u32, len: u32) {
        self.tracker.rearm(pa, len);
    }

    #[inline]
    fn mem_noaccess(&self, pa: u32, len: u32, on: bool) {
        self.tracker.noaccess(pa, len, on);
    }

    fn kick(&self) {
        self.wake.wake();
    }

    fn guest_hold(&self, on: bool) {
        if !on_guest_thread() {
            return;
        }
        if on {
            if self.holds.fetch_add(1, Ordering::Relaxed) == 0 {
                self.hold_on.store(self.clock.hold_begin(), Ordering::Relaxed);
            }
        } else if self.holds.load(Ordering::Relaxed) != 0
            && self.holds.fetch_sub(1, Ordering::Relaxed) == 1
            && self.hold_on.swap(false, Ordering::Relaxed)
        {
            let end = self.clock.hold_end();
            if end.left_out_ns != 0
                && let Some(apu) = self.apu()
            {
                apu.guest_time_held(end.left_out_ns);
            }
        }
    }

    fn frame_wait(&self) {
        // Taken by the kernel at its next kicked entry (`xb_kernel`'s
        // `frame.rs`): the thread that finished the frame waits for a flip.
        self.counters.frame_waits.fetch_add(1, Ordering::Relaxed);
        // Only while the title keeps its rate with it (`pacing.rs`).
        if self.hold_wanted.load(Ordering::Relaxed) {
            self.counters.frame_holds.fetch_add(1, Ordering::Relaxed);
            self.frame.request();
            self.wake.wake();
        }
    }

    fn frame_flipped(&self) {
        if self.frame.flipped() {
            self.wake.wake();
        }
    }

    fn cpu_regs(&self) -> Option<CpuRegs> {
        if !self.cpu.ran.load(Ordering::Relaxed) {
            return None;
        }
        let r = *lock(&self.cpu.regs);
        Some(CpuRegs {
            gpr: r.gpr,
            eip: r.eip,
            eflags: r.eflags,
            fs_base: r.fs_base,
            halted: false,
        })
    }

    fn guest_time_ns(&self) -> u64 {
        self.clock.now_ns()
    }

    fn guest_to_host_ns(&self, guest_ns: u64) -> u64 {
        monotonic_ns() + self.clock.host_delay(guest_ns).as_nanos() as u64
    }
}

impl ApuHost for Shared {
    fn guest_time_ns(&self) -> u64 {
        self.clock.now_ns()
    }

    fn ram_written(&self, pa: u32, len: u32) {
        self.tracker.set_dirty(pa, len);
    }
}

impl xb_devices::VblankCounter for Shared {
    fn vblanks(&self) -> u64 {
        self.gpu.get().map_or(0, Gpu::vblank_count)
    }
}

/// The write tracker's events, forwarded to the renderer.
pub(crate) struct ToRenderer(pub(crate) MemEvents);

impl TrackerEvents for ToRenderer {
    fn first_write(&self, pa: u32, len: u32) {
        self.0.first_write(pa, len);
    }

    fn stale_access(&self, pa: u32, write: bool) {
        self.0.stale_access(pa, write);
    }

    fn host_wrote(&self, pa: u32, len: u32) {
        self.0.host_wrote(pa, len);
    }

    fn unguard_range(&self, pa: u32, len: u32) {
        self.0.unguard_range(pa, len);
    }
}

/// What moving a RAM page asks of write tracking and the renderer
/// (`xb_mem::RelocateHooks`, as its documentation prescribes).
pub(crate) struct Relocate {
    pub(crate) tracker: Arc<WriteTracker>,
    /// The renderer's callbacks, set once the GPU exists.
    pub(crate) events: Arc<OnceLock<MemEvents>>,
}

impl RelocateHooks for Relocate {
    fn movable(&self, pa: u32) -> bool {
        !self.tracker.has_noaccess(pa)
    }

    fn before_move(&mut self, from: u32, to: u32) {
        if self.tracker.open(from) && self.tracker.open(to) {
            return;
        }
        if let Some(ev) = self.events.get() {
            let fence = ev.write_fence(from, PAGE_SIZE).max(ev.write_fence(to, PAGE_SIZE));
            ev.wait_fence(fence);
        }
    }

    fn after_move(&mut self, from: u32, to: u32) {
        self.tracker.set_dirty(from, PAGE_SIZE);
        self.tracker.set_dirty(to, PAGE_SIZE);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A backend that records the mapper calls it receives.
    #[derive(Default)]
    struct Recorder {
        calls: Vec<String>,
    }

    impl Mapper for Recorder {
        fn map(&mut self, va: u32, pa: u32, len: u32, access: Access) {
            self.calls.push(format!("map {va:x} {pa:x} {len:x} {access:?}"));
        }
        fn map_mmio(&mut self, va: u32, pa: u32, len: u32) {
            self.calls.push(format!("mmio {va:x} {pa:x} {len:x}"));
        }
        fn unmap(&mut self, va: u32, len: u32) {
            self.calls.push(format!("unmap {va:x} {len:x}"));
        }
    }

    struct Open;

    impl PageProtector for Open {
        fn protect(&self, _pa: u32, _len: u32, _access: Access) {}
    }

    impl Cpu for Recorder {
        fn name(&self) -> &'static str {
            "recorder"
        }
        fn run(&mut self, ctx: &mut Context, _bus: &mut dyn Bus, _flags: RunFlags) -> Exit {
            ctx.regs.eip = 0x1234;
            Exit::Trap
        }
        fn kicker(&self) -> Kicker {
            Kicker::new(|| {})
        }
        fn protector(&self) -> Arc<dyn PageProtector> {
            Arc::new(Open)
        }
        fn set_trap_range(&mut self, _va: u32, _len: u32) {}
        fn invalidate_code(&mut self, _pa: u32, _len: u32) {}
    }

    fn machine_cpu() -> (MachineCpu, Arc<CpuShared>) {
        let shared = Arc::new(CpuShared::new());
        (MachineCpu::new(Box::new(Recorder::default()), shared.clone()), shared)
    }

    #[test]
    fn the_shared_table_follows_every_mapping_change() {
        let (mut cpu, shared) = machine_cpu();
        let t = &shared.table;
        assert_eq!(t.translate(0x0001_0000), None);
        cpu.map(0x0001_0000, 0x0030_0000, 0x3000, Access::Read);
        assert_eq!(t.translate(0x0001_0004), Some(0x0030_0004));
        assert_eq!(t.translate(0x0001_2FFF), Some(0x0030_2FFF));
        assert_eq!(t.translate(0x0001_3000), None);
        cpu.map_mmio(0xFD00_0000, 0xFD00_0000, 0x1000);
        assert_eq!(t.translate(0xFD00_0000), None, "device pages are not RAM");
        cpu.unmap(0x0001_1000, 0x1000);
        assert_eq!(t.translate(0x0001_1000), None);
        assert_eq!(t.translate(0x0001_2000), Some(0x0030_2000));
    }

    #[test]
    fn only_the_title_s_own_protection_forbids_an_access() {
        let (mut cpu, shared) = machine_cpu();
        let t = &shared.table;
        cpu.map(0x1000, 0x5000, 0x1000, Access::Read);
        cpu.map(0x2000, 0x6000, 0x1000, Access::ReadWrite);
        cpu.map(0x3000, 0x7000, 0x1000, Access::None);
        assert!(t.forbids(0x1000, 0x5000, true));
        assert!(!t.forbids(0x1000, 0x5000, false));
        assert!(!t.forbids(0x2000, 0x6000, true));
        assert!(t.forbids(0x3000, 0x7000, false));
        // An address that maps another page (the KVM backend names an
        // address the instruction uses, not always the faulting one) or
        // nothing says nothing.
        assert!(!t.forbids(0x1000, 0x9000, true));
        assert!(!t.forbids(0x8000, 0x5000, true));
    }

    #[test]
    fn exits_are_counted_and_the_registers_kept_for_the_renderer() {
        struct NoBus;
        impl Bus for NoBus {
            fn mmio_read(&mut self, _pa: u32, _size: u8) -> u32 {
                0
            }
            fn mmio_write(&mut self, _pa: u32, _size: u8, _value: u32) {}
            fn io_read(&mut self, _port: u16, _size: u8) -> u32 {
                0
            }
            fn io_write(&mut self, _port: u16, _size: u8, _value: u32) {}
            fn page_fault(
                &mut self,
                _va: u32,
                _pa: Option<u32>,
                _write: bool,
            ) -> xb_cpu::FaultAction {
                xb_cpu::FaultAction::Raise
            }
            fn tsc(&mut self) -> u64 {
                0
            }
        }
        let (mut cpu, shared) = machine_cpu();
        assert!(!shared.ran.load(Ordering::Relaxed));
        let mut ctx = Context::default();
        assert_eq!(cpu.run(&mut ctx, &mut NoBus, RunFlags::default()), Exit::Trap);
        assert_eq!(shared.traps.load(Ordering::Relaxed), 1);
        assert_eq!(shared.exits.load(Ordering::Relaxed), 1);
        assert_eq!(lock(&shared.regs).eip, 0x1234);
    }

    #[test]
    fn a_moved_page_is_reported_written_at_both_ends() {
        let tracker = Arc::new(WriteTracker::new(0x10_0000, Arc::new(Open)));
        let mut hooks = Relocate { tracker: tracker.clone(), events: Arc::new(OnceLock::new()) };
        // Both pages tracked and clean.
        tracker.test_and_clear(0x3000, 0x1000);
        tracker.test_and_clear(0x5000, 0x1000);
        assert!(!tracker.dirty(0x3000) && !tracker.dirty(0x5000));
        assert!(hooks.movable(0x3000));
        tracker.noaccess(0x8000, 0x1000, true);
        assert!(!hooks.movable(0x8000));
        hooks.before_move(0x3000, 0x5000);
        hooks.after_move(0x3000, 0x5000);
        assert!(tracker.dirty(0x3000) && tracker.dirty(0x5000));
        assert!(tracker.open(0x3000) && tracker.open(0x5000));
    }
}
