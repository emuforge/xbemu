//! Threads and the scheduler.
//!
//! Every guest thread runs on the one guest CPU. A thread that isn't running
//! keeps its CPU state in [`Thread::ctx`]; switching happens inside a kernel
//! entry by saving the frame and loading another context. Like the Xbox's one
//! CPU, only one guest thread runs at a time; the highest priority ready
//! thread runs, in the order they became ready within a priority.
//!
//! Guest layouts (nxdk xboxkrnl.h, CC0; 32-bit):
//!   KTHREAD (0x110): Header 0x00 (Type, Size 0x02, SignalState 0x04, WaitListHead 0x08),
//!     MutantListHead 0x10, StackBase 0x1C, StackLimit 0x20, KernelStack 0x24, TlsData 0x28,
//!     State 0x2C, NpxState 0x30, Priority 0x32, ApcState.ApcListHead\[2\] 0x34, ApcState.Process 0x44,
//!     Quantum 0x6C, BasePriority 0x70, ThreadListEntry 0x104
//!   KPROCESS (0x1C): ReadyListHead 0x00, ThreadListHead 0x08, StackCount 0x10,
//!     ThreadQuantum 0x14 (XSetProcessQuantumLength: ms * 3), BasePriority 0x18
//!   ETHREAD (0x140): Tcb 0x00, ExitStatus 0x120, UniqueThread 0x12C, StartAddress 0x130
//!   KPCR: NT_TIB (ExceptionList 0x00, StackBase 0x04, StackLimit 0x08),
//!     PRCB at 0x28 starting with CurrentThread

use std::time::Duration;

use xb_cpu::{Context, reg};
use xb_types::{PAGE_SIZE, align_up};

use crate::apc::{Apc, ApcChain};
use crate::gm::KResult;
use crate::kernel::{
    Args, GPU_WINDOW, Kernel, ORD_THREAD_RETURN, ObType, STATUS_INSUFFICIENT_RESOURCES,
    STATUS_INVALID_HANDLE, STATUS_SUCCESS,
};
use crate::platform::Platform;
use crate::seh::SehLevel;

const KT_TYPE: u32 = 0x00;
const KT_SIZE: u32 = 0x02;
const KT_SIGNAL_STATE: u32 = 0x04;
const KT_WAIT_LIST: u32 = 0x08;
const KT_MUTANT_LIST: u32 = 0x10;
const KT_STACK_BASE: u32 = 0x1C;
const KT_STACK_LIMIT: u32 = 0x20;
const KT_KERNEL_STACK: u32 = 0x24;
const KT_TLS_DATA: u32 = 0x28;
const KT_NPX_STATE: u32 = 0x30;
const KT_PRIORITY: u32 = 0x32;
const KT_APC_LISTS: u32 = 0x34;
const KT_APC_PROCESS: u32 = 0x44;
const KT_QUANTUM: u32 = 0x6C;
const KT_BASE_PRIORITY: u32 = 0x70;
const KTHREAD_SIZE: u32 = 0x110;
const ET_EXIT_STATUS: u32 = 0x120;
const ET_UNIQUE_THREAD: u32 = 0x12C;
const ET_START_ADDRESS: u32 = 0x130;
const ETHREAD_SIZE: u32 = 0x140;
const THREAD_OBJECT: u8 = 6;
const KP_READY_LIST: u32 = 0x00;
const KP_THREAD_LIST: u32 = 0x08;
const KP_QUANTUM: u32 = 0x14;
const KP_BASE_PRIORITY: u32 = 0x18;
const KPROCESS_SIZE: u32 = 0x1C;

const PCR_EXCEPTION_LIST: u32 = 0x00;
const PCR_STACK_BASE: u32 = 0x04;
const PCR_STACK_LIMIT: u32 = 0x08;
const PCR_CURRENT_THREAD: u32 = 0x28;

/// FX_SAVE_AREA (nboxkrnl ki.hpp): FLOATING_SAVE_AREA (an FXSAVE image, 512
/// bytes, then Cr0NpxState) and 12 bytes of alignment.
const FX_SAVE_AREA: u32 = 0x210;
const FX_MXCSR: u32 = 0x18;
const FX_CR0_NPX_STATE: u32 = 0x200;
const NPX_STATE_NOT_LOADED: u8 = 0x0A; // CR0_TS | CR0_MP

/// Where kernel stacks live.
pub(crate) const STACK_REGION: u32 = 0xD000_0000;
const STACK_GUARD: u32 = 0x10000;
const DEFAULT_STACK: u32 = 0x10000;
/// The least the kernel makes (nboxkrnl kernel.hpp).
const KERNEL_STACK_SIZE: u32 = 0x3000;
/// Zeroed pages over a thread's stack top.
const STACK_OVER: u32 = 0x4000;
const DEFAULT_PRIORITY: i32 = 8;

pub(crate) const MAX_THREADS: usize = 128;
pub(crate) const MAX_WAIT_OBJECTS: usize = 64;
pub(crate) const MAX_CALLOUTS: usize = 4;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ThreadState {
    #[default]
    Free,
    Ready,
    Running,
    Waiting,
    Suspended,
    Terminated,
}

/// What a waiting thread waits for.
#[derive(Clone, Debug, Default)]
pub(crate) struct Wait {
    /// Guest dispatcher objects.
    pub objects: Vec<u32>,
    pub wait_all: bool,
    /// Guest time; `None`: no timeout.
    pub deadline: Option<u64>,
    /// RTL_CRITICAL_SECTION the thread takes when woken, or 0.
    pub cs: u32,
    /// An APC ends it (STATUS_USER_APC).
    pub alertable: bool,
    /// A frame-latency hold (`frame.rs`): the flip count it began at. It
    /// ends at a flip or its deadline, and the thread's registers stay.
    pub hold: Option<u32>,
}

/// A kernel call finishing in a guest routine.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Callout {
    pub ret: u32,
    pub esp: u32,
    /// IRQL to restore on return, 0xFF: leave.
    pub irql: u8,
}

/// A guest thread.
#[derive(Clone, Default)]
pub(crate) struct Thread {
    pub state: ThreadState,
    /// Guest ETHREAD (KTHREAD at offset 0).
    pub ethread: u32,
    pub id: u32,
    pub priority: i32,
    pub stack_base: u32,
    pub stack_limit: u32,
    /// Under the FX save area at the stack's top: the TIB's StackBase
    /// (`fs:[4]`, the TLS's top), and where the thread's frames begin.
    pub tib_base: u32,
    pub frame_top: u32,
    /// NT_TIB.ExceptionList while not running.
    pub exception_list: u32,
    pub wait: Wait,
    /// Suspend count: runnable only at 0.
    pub suspend: u32,
    pub callouts: Vec<Callout>,
    /// APCs queued, and the chains running for alertable waits (apc.rs).
    pub apcs: std::collections::VecDeque<Apc>,
    pub apc: ApcChain,
    pub apc_outer: Vec<ApcChain>,
    /// Exception dispatches and unwinds in progress (seh.rs).
    pub seh: Vec<SehLevel>,
    /// `ctx` was just set up (an ISR or DPC entry): the running frame must
    /// not be saved over it.
    pub fresh: bool,
    /// Threads of one priority run in the order they became ready
    /// (`ready_seq`, 0: not yet ordered), each for its quantum, which
    /// preemption pauses rather than restarts.
    pub ready_seq: u64,
    pub run_seq: u64,
    pub slice_used_ns: u64,
    pub slice_start_ns: u64,
    pub slice_expired: bool,
    /// NT's wait boost (KiUnwaitThread): a thread a signal woke runs at
    /// priority + the signal's increment (up to 15) until a quantum of its
    /// ends, a step a quantum.
    pub boost: i32,
    pub no_boost: bool,
    /// KeSetBasePriorityThread's +-16: +1, -1, else 0.
    pub saturation: i8,
    /// NtRemoveIoCompletion: where its packet goes (KeyContext, ApcContext,
    /// IoStatusBlock).
    pub port_out: [u32; 3],
    pub ctx: Context,
}

impl Thread {
    /// The priority it is scheduled at: its own, with its wait boost.
    pub(crate) fn prio(&self) -> i32 {
        let p = self.priority + self.boost;
        if self.boost != 0 && self.priority < 16 { p.min(15) } else { self.priority }
    }

    /// Where the thread's frames begin: under its FX save area and TLS.
    pub(crate) fn frame_top(&self) -> u32 {
        if self.frame_top != 0 { self.frame_top } else { self.stack_base }
    }
}

/// The scheduler's state
#[derive(Default)]
pub(crate) struct Sched {
    pub next_stack_va: u32,
    /// Stack slots given back, first fit.
    pub stack_free: Vec<(u32, u32)>,
    pub next_thread_id: u32,
    /// Whose slice runs; `None`: the next thread switched in starts one.
    pub slice_thread: Option<usize>,
    /// When it ends (guest ns).
    pub slice_due: Option<u64>,
    /// `Thread::ready_seq`'s clock.
    pub ready_clock: u64,
    /// Resumes before anything else (was above DISPATCH_LEVEL).
    pub resume_first: Option<usize>,
    /// NtYieldExecution to a lower priority thread: `yield_from` stays out
    /// of the scheduler's pick while `yield_to` runs its quantum.
    pub yield_from: Option<usize>,
    pub yield_to: Option<usize>,
    /// Switch threads even though the IRQL is raised (ISR entry).
    pub force_switch: bool,
    /// The scheduler is looking for a thread: no thread's state is in the
    /// frame (the idle wait may return to the run loop for a pause).
    pub picking: bool,
    /// The thread that was current when the pick began.
    pub pick_from: Option<usize>,
    pub idle_since: Option<u64>,
    // poll_spin's and the delay's spin detection (ke.rs)
    pub poll_last: Option<usize>,
    pub poll_last_ns: u64,
    pub poll_since: u64,
    pub polls: u32,
    pub delay_last: Option<usize>,
    pub delay_last_ns: u64,
    pub delay_since: u64,
}

impl Kernel {
    fn list_init(&self, head: u32) {
        self.gm.kw32(head, head);
        self.gm.kw32(head + 4, head);
    }

    /// STACK_OVER's pages over a stack's top, those not mapped yet, zeroed.
    fn stack_over_map(&mut self, top: u32) {
        for p in (top..top + STACK_OVER).step_by(PAGE_SIZE as usize) {
            if !self.gm.space.is_mapped(p) {
                self.gm.alloc_at(p, PAGE_SIZE);
            }
        }
    }

    /// Stack slots in the kernel stack region, each with an unmapped guard
    /// below. A slot given back (a thread's end, MmDeleteKernelStack) is
    /// taken again by a stack it holds, what it has over kept as a slot of
    /// its own; the region ends at the GPU's view of RAM, where an
    /// allocation fails instead of mapping over it (titles start a thread
    /// per movie and load).
    pub(crate) fn stack_alloc(&mut self, size: u32) -> Option<u32> {
        if self.sched.next_stack_va == 0 {
            self.sched.next_stack_va = STACK_REGION;
        }
        let span = STACK_GUARD + align_up(size, STACK_GUARD);
        for i in 0..self.sched.stack_free.len() {
            let (va, have) = self.sched.stack_free[i];
            if have < span || !self.gm.alloc_at(va + STACK_GUARD, size) {
                continue;
            }
            self.sched.stack_free.swap_remove(i);
            if have - span >= 2 * STACK_GUARD {
                self.sched.stack_free.push((va + span, have - span)); // the rest, a slot of its own
            }
            return Some(va + STACK_GUARD);
        }
        let slot = self.sched.next_stack_va;
        if span > GPU_WINDOW - slot {
            log::warn!(target: "kernel", "ps: the stack region is full ({} KiB more asked)", size >> 10);
            return None;
        }
        if !self.gm.alloc_at(slot + STACK_GUARD, size) {
            return None;
        }
        self.sched.next_stack_va = slot + span;
        Some(slot + STACK_GUARD)
    }

    /// The stack at `base` (`size` bytes, its pages gone) gives its slot
    /// back.
    pub(crate) fn stack_free(&mut self, base: u32, size: u32) {
        let slot = base.wrapping_sub(STACK_GUARD);
        let span = STACK_GUARD + align_up(size, STACK_GUARD);
        if base < STACK_REGION + STACK_GUARD
            || !base.is_multiple_of(STACK_GUARD)
            || size == 0
            || span > self.sched.next_stack_va.wrapping_sub(slot)
            || self.sched.stack_free.len() == 32
        {
            return; // not a slot of the region, or no room to keep it: its addresses stay used
        }
        if self.sched.stack_free.iter().any(|&(va, sp)| slot < va + sp && va < slot + span) {
            return; // given back already
        }
        self.sched.stack_free.push((slot, span));
    }

    fn thread_create(&mut self, stack_size: u32, tls_size: u32, ext_size: u32) -> Option<usize> {
        let Some(ti) = self.threads.iter().position(|t| t.state == ThreadState::Free) else {
            log::error!(target: "kernel", "ps: out of thread slots");
            return None;
        };
        self.threads_hi = self.threads_hi.max(ti + 1);
        let et = self.ob_create(ObType::Thread, ETHREAD_SIZE + ext_size);
        if et == 0 {
            return None;
        }
        // The stack's top as the Xbox kernel lays it out
        // (KiInitializeContextThread; nboxkrnl, after Cxbx-Reloaded): the FX
        // save area right under StackBase, the TLS under it, the thread's
        // frames under that. The TIB's StackBase (fs:[4]) is the save area's
        // bottom: XAPI sizes the TLS as align16(template) + 4 and indexes it
        // from there with a negative TLS index. Without the save area a
        // title's frames sat 0x210 bytes nearer the top than on the console:
        // Silent Hill 2 reads 0x170 bytes over its stack pointer near the
        // bottom of its calls. The kernel makes no stack under
        // KERNEL_STACK_SIZE (Splinter Cell - Double Agent asks for less than
        // a page).
        let tls = align_up(tls_size, 4);
        let want = if stack_size != 0 { stack_size } else { DEFAULT_STACK };
        let size = align_up(want + FX_SAVE_AREA + tls + 16, PAGE_SIZE).max(KERNEL_STACK_SIZE);
        let base = self.stack_alloc(size)?;
        let top = base + size;
        let tib = top - FX_SAVE_AREA;

        // Pages over the top, zeroed: on the console a title's thread starts
        // under more of the kernel's and XAPI's frames, and a read past the
        // top lands in memory there (Dave Mirra Freestyle BMX 2 indexes an
        // array on its stack with a stray byte, 0x80 bytes over the top;
        // Shin Sangoku Musou 3 up to 0x3030 bytes). They go with the stack.
        self.stack_over_map(top);
        // the top page's: a slot may come back used
        let _ = self.gm.fill(tib, 0, FX_SAVE_AREA);
        let _ = self.gm.w16(tib, 0x27F);
        let _ = self.gm.w32(tib + FX_MXCSR, 0x1F80);
        let _ = self.gm.w32(tib + FX_CR0_NPX_STATE, u32::from(NPX_STATE_NOT_LOADED));
        let gm = &self.gm;
        gm.kw8(et + KT_TYPE, THREAD_OBJECT);
        gm.kw8(et + KT_SIZE, (KTHREAD_SIZE / 4) as u8);
        gm.kw8(et + KT_NPX_STATE, NPX_STATE_NOT_LOADED);
        self.list_init(et + KT_WAIT_LIST);
        self.list_init(et + KT_MUTANT_LIST);
        self.list_init(et + KT_APC_LISTS);
        self.list_init(et + KT_APC_LISTS + 8);
        gm.kw32(et + KT_STACK_BASE, top);
        gm.kw32(et + KT_STACK_LIMIT, base);
        gm.kw32(et + KT_KERNEL_STACK, (tib - tls) & !15);
        gm.kw32(et + KT_TLS_DATA, if tls != 0 { tib - tls } else { 0 });
        if tls >= 4 {
            let _ = gm.w32(tib - tls, tib - tls + 4);
        }
        gm.kw8(et + KT_PRIORITY, DEFAULT_PRIORITY as u8);
        gm.kw8(et + KT_BASE_PRIORITY, DEFAULT_PRIORITY as u8);
        gm.kw32(et + KT_QUANTUM, 36);
        if self.kprocess == 0 {
            self.kprocess = self.pool_alloc(KPROCESS_SIZE);
            if self.kprocess != 0 {
                self.list_init(self.kprocess + KP_READY_LIST);
                self.list_init(self.kprocess + KP_THREAD_LIST);
                self.gm.kw32(self.kprocess + KP_QUANTUM, 60); // 20 ms
                self.gm.kw8(self.kprocess + KP_BASE_PRIORITY, DEFAULT_PRIORITY as u8);
            }
        }
        self.gm.kw32(et + KT_APC_PROCESS, self.kprocess);

        self.sched.next_thread_id += 1;
        let id = self.sched.next_thread_id * 4;
        self.threads[ti] = Thread {
            state: ThreadState::Suspended,
            ethread: et,
            id,
            priority: DEFAULT_PRIORITY,
            stack_base: top,
            stack_limit: base,
            tib_base: tib,
            frame_top: (tib - tls) & !15,
            exception_list: 0xFFFF_FFFF,
            ..Thread::default()
        };
        self.gm.kw32(et + ET_UNIQUE_THREAD, id);
        Some(ti)
    }

    /// KPCR fields that follow the running thread.
    fn pcr_enter(&self, ti: usize) {
        let t = &self.threads[ti];
        let pcr = self.kpcr_va;
        self.gm.kw32(pcr + PCR_EXCEPTION_LIST, t.exception_list);
        self.gm.kw32(pcr + PCR_STACK_BASE, if t.tib_base != 0 { t.tib_base } else { t.stack_base });
        self.gm.kw32(pcr + PCR_STACK_LIMIT, t.stack_limit);
        self.gm.kw32(pcr + PCR_CURRENT_THREAD, t.ethread);
    }

    /// Creates the title's first thread at `entry` and makes it the running
    /// one. Returns its stack's limit and base.
    pub(crate) fn boot_thread(&mut self, entry: u32, stack_size: u32) -> Option<(u32, u32)> {
        let ti = self.thread_create(stack_size, 0, 0)?;
        let et = self.threads[ti].ethread;
        self.gm.kw32(et + ET_START_ADDRESS, entry);
        let esp = self.threads[ti].frame_top - 16 - 4;
        let _ = self.gm.w32(esp, Self::trap(ORD_THREAD_RETURN));
        let ctx = self.context_init(entry, esp);
        let t = &mut self.threads[ti];
        t.ctx = ctx;
        t.state = ThreadState::Running;
        let r = (t.stack_limit, t.stack_base);
        self.frame = ctx;
        self.current = Some(ti);
        self.pcr_enter(ti);
        Some(r)
    }

    /// The thread whose ETHREAD is at `ethread`.
    pub(crate) fn thread_of(&self, ethread: u32) -> Option<usize> {
        self.threads.iter().position(|t| t.state != ThreadState::Free && t.ethread == ethread)
    }

    /// A kernel-owned thread, suspended, with no start context yet.
    pub(crate) fn kernel_thread(&mut self, stack_size: u32, priority: i32) -> Option<usize> {
        let ti = self.thread_create(stack_size, 0, 0)?;
        let t = &mut self.threads[ti];
        t.priority = priority;
        log::info!(target: "kernel", "ps: kernel thread {}, priority {priority}, stack {:08x}-{:08x}",
            t.id, t.stack_limit, t.stack_base);
        Some(ti)
    }

    /// The process quantum: KPROCESS ThreadQuantum, in thirds of a
    /// millisecond (60: 20 ms; XSetProcessQuantumLength writes it).
    fn quantum_ns(&self) -> u64 {
        let q = if self.kprocess != 0 { self.gm.kr32(self.kprocess + KP_QUANTUM) } else { 0 };
        u64::from(if q != 0 { q } else { 60 }) * 1_000_000 / 3
    }

    fn is_kernel_thread(&self, ti: usize) -> bool {
        Some(ti) == self.intr.isr_thread || Some(ti) == self.timer.dpc_thread
    }

    /// The running thread's slice is due: a ready thread of its priority or
    /// above takes over, or it gets another. Without slices a thread polling
    /// at that priority kept the CPU for good once interrupts handed it
    /// back: Bink's scheduler in Halo spins on a zero-timeout wait for a
    /// task mutex the title's thread holds, ready at the same priority, and
    /// the movie stopped.
    pub(crate) fn slice_end(&mut self, now: u64) {
        let Some(cur) = self.current else { return };
        if Some(cur) != self.sched.slice_thread || self.threads[cur].state != ThreadState::Running {
            return; // an ISR or a DPC runs: the slice ends once its thread is back
        }
        if self.threads[cur].boost != 0 {
            self.threads[cur].boost -= 1; // NT's quantum end: a step of its wait boost
        }
        let cp = self.threads[cur].prio();
        let other = self.threads[..self.threads_hi].iter().enumerate().any(|(i, t)| {
            i != cur && t.state == ThreadState::Ready && t.suspend == 0 && t.prio() >= cp
        });
        if other {
            // the back of its priority's queue (schedule), a new quantum
            self.threads[cur].slice_expired = true;
            self.resched = true;
            self.sched.slice_due = None;
            self.stats.slices += 1;
            return;
        }
        self.threads[cur].slice_used_ns = 0;
        self.threads[cur].slice_start_ns = now;
        self.sched.slice_due = Some(now + self.quantum_ns());
    }

    /// The running thread gives way to another ready thread; `false`: no
    /// other thread ready, no yield.
    pub(crate) fn ps_yield(&mut self) -> bool {
        let Some(cur) = self.current else {
            self.resched = true;
            return true;
        };
        // With no other thread ready, NT's NtYieldExecution switches to none
        // and returns STATUS_NO_YIELD_PERFORMED, the thread's quantum and
        // boost as they were.
        let candidate = |k: &Kernel, i: usize| {
            let t = &k.threads[i];
            i != cur && t.state == ThreadState::Ready && t.suspend == 0 && !k.is_kernel_thread(i)
        };
        if !(0..self.threads_hi).any(|i| candidate(self, i)) {
            self.resched = true; // the timers, I/O and interrupts looked at all the same
            return false;
        }
        self.threads[cur].slice_expired = true; // behind the threads it gives way to
        // and a step of its wait boost, as NT's NtYieldExecution takes it
        // (KiComputeNewPriority): NFL Fever 2004's loader yields until a
        // worker of its own priority, ready, runs; boosted a step above, it
        // got the CPU back every time.
        if self.threads[cur].boost != 0 {
            self.threads[cur].boost -= 1;
        }
        // With none of its priority or above ready, NT gives the CPU to the
        // highest priority ready thread below it, and that thread keeps it
        // until it waits or its quantum ends (FIFA World Cup 2006's main
        // thread yields until the cache worker a step below its priority
        // reaches a checkpoint).
        let cp = self.threads[cur].prio();
        let mut low: Option<usize> = None;
        for i in 0..self.threads_hi {
            if !candidate(self, i) {
                continue;
            }
            let tp = self.threads[i].prio();
            if tp >= cp {
                low = None; // one of its own priority or above: the usual turn
                break;
            }
            if low.is_none_or(|l| tp > self.threads[l].prio()) {
                low = Some(i);
            }
        }
        if low.is_some() {
            self.sched.yield_from = Some(cur);
            self.sched.yield_to = low;
        }
        self.resched = true;
        true
    }

    /// Saves the running thread and loads the next ready one into the frame.
    /// When every thread waits the guest CPU idles on the host; a pause or
    /// stop request ends the idle wait with the pick unfinished
    /// ([`Sched::picking`]), and the run loop comes back to [`Kernel::pick`].
    pub(crate) fn schedule(&mut self, p: &mut dyn Platform) {
        self.stats.schedules += 1;
        self.resched = false;
        let cur = self.current;
        // The frame always goes back through the saved context: a wait that
        // ends in this call writes its status there, even when the same
        // thread continues.
        if let Some(c) = cur {
            let pcr = self.kpcr_va;
            let t = &mut self.threads[c];
            if t.state != ThreadState::Terminated && !t.fresh {
                t.ctx = self.frame;
                t.exception_list = self.gm.kr32(pcr + PCR_EXCEPTION_LIST);
            }
        }
        // Threads of one priority take turns in the order they became ready,
        // as the NT scheduler's queues have them (Splinter Cell Chaos
        // Theory's DirectSound thread polls at the main thread's priority,
        // and round-robin by slot starved the main thread). A thread
        // switched out while running keeps its place and the rest of its
        // quantum; one whose quantum ran out goes to the back with a new
        // one; one that waits gets a new one and its place when it is ready
        // again.
        let now = self.now();
        if let Some(c) = cur {
            let is_slice = Some(c) == self.sched.slice_thread;
            let t = &mut self.threads[c];
            if t.state == ThreadState::Running {
                if t.slice_expired {
                    self.sched.ready_clock += 1;
                    t.ready_seq = self.sched.ready_clock;
                    t.slice_used_ns = 0;
                } else {
                    t.ready_seq = if t.run_seq != 0 {
                        t.run_seq
                    } else {
                        self.sched.ready_clock += 1;
                        self.sched.ready_clock
                    };
                    if is_slice && t.slice_start_ns != 0 {
                        t.slice_used_ns += now.saturating_sub(t.slice_start_ns);
                    }
                }
                t.slice_expired = false;
                t.state = ThreadState::Ready;
            } else {
                t.slice_used_ns = 0; // it waits or ends: a new quantum when it runs again
                t.slice_expired = false;
                t.ready_seq = 0;
                if is_slice {
                    self.sched.slice_thread = None;
                }
            }
        }
        self.sched.force_switch = false;

        // A yield to a lower priority thread lasts while that thread runs:
        // its wait or its quantum's end gives the CPU back.
        if self.sched.yield_from.is_some()
            && cur.is_some()
            && cur == self.sched.yield_to
            && cur.is_some_and(|c| {
                self.threads[c].state != ThreadState::Ready || self.threads[c].slice_used_ns == 0
            })
        {
            self.sched.yield_from = None;
            self.sched.yield_to = None;
        }
        self.sched.picking = true;
        self.sched.pick_from = cur;
        self.sched.idle_since = None;
        self.pick(p);
    }

    /// The scheduler's loop: code an ISR interrupted above DISPATCH_LEVEL
    /// continues first, then the highest priority ready thread. When every
    /// thread waits, the guest CPU idles until the next timeout, timer or
    /// interrupt.
    pub(crate) fn pick(&mut self, p: &mut dyn Platform) {
        let cur = self.sched.pick_from;
        let start = cur.map_or(0, |c| c + 1);
        let next = loop {
            let (timer_due, _) = self.timer_poll();
            self.io_poll(); // a disc read done sets the event a thread waits on
            self.intr_dispatch(true);
            self.dpc_dispatch();
            let mut deadline = self.check_waits(0);
            if let Some(t) = timer_due {
                deadline = Some(deadline.map_or(t, |d| d.min(t)));
            }
            let mut next: Option<usize> = None;
            if let Some(rf) = self.sched.resume_first
                && self.threads[rf].state == ThreadState::Ready
                && !self.intr.in_isr
            {
                next = Some(rf);
            } else {
                for n in 0..MAX_THREADS {
                    let i = (start + n) % MAX_THREADS;
                    let t = &self.threads[i];
                    if t.state != ThreadState::Ready
                        || t.suspend != 0
                        || Some(i) == self.sched.yield_from
                    {
                        continue;
                    }
                    if t.ready_seq == 0 {
                        self.sched.ready_clock += 1;
                        self.threads[i].ready_seq = self.sched.ready_clock; // ready since the last pass
                    }
                    let t = &self.threads[i];
                    let better = next.is_none_or(|b| {
                        let nb = &self.threads[b];
                        t.prio() > nb.prio()
                            || (t.prio() == nb.prio() && t.ready_seq < nb.ready_seq)
                    });
                    if better {
                        next = Some(i);
                    }
                }
            }
            if let Some(n) = next {
                break n;
            }
            if self.sched.yield_from.is_some() {
                self.sched.yield_from = None; // nothing else to run: the yielding thread
                self.sched.yield_to = None;
                continue;
            }
            // Idle. A pause or a stop asked for meanwhile takes the run loop
            // out; it comes back here.
            if self.stop.is_some() || self.control.pending() {
                return;
            }
            // With no timeout pending only an interrupt can wake a thread:
            // give up after a while without one.
            let now = self.now();
            let since = *self.sched.idle_since.get_or_insert(now);
            if deadline.is_none() && now - since > 30_000_000_000 {
                self.request_stop(
                    "no runnable thread: every thread waits with no timeout (idle 30 s)".into(),
                );
                return;
            }
            self.stats.idles += 1;
            self.device_tick(p, now);
            // No thread runs: the devices say when they next need a look
            // given that nothing the guest does can come before it.
            let dev = p.idle_deadline(now, self.dev_due);
            for d in [dev, self.io_next_due()].into_iter().flatten() {
                deadline = Some(deadline.map_or(d, |x| x.min(d)));
            }
            let max = Duration::from_secs(1);
            let delay = deadline.map_or(max, |d| self.clock.host_delay(d).min(max));
            if self.io_busy() {
                let t0 = std::time::Instant::now();
                self.wake.wait(delay);
                self.io_idled(t0.elapsed());
            } else {
                self.wake.wait(delay);
            }
            self.update_time();
        };
        self.sched.picking = false;
        if Some(next) == self.sched.resume_first {
            self.sched.resume_first = None;
        }
        if self.sched.yield_from.is_some()
            && Some(next) != self.sched.yield_to
            && !self.is_kernel_thread(next)
        {
            self.sched.yield_from = None; // a higher priority thread came first
            self.sched.yield_to = None;
        }
        if Some(next) != cur {
            self.stats.switches += 1;
        }
        if let Some(c) = cur
            && c != next
            && self.threads[c].state == ThreadState::Terminated
        {
            // Off its stack for good: the pages go back (titles start
            // threads per movie and load), the slot can hold a new thread.
            // Its ETHREAD stays, signaled, for whoever still holds a handle.
            let (limit, base) = (self.threads[c].stack_limit, self.threads[c].stack_base);
            self.gm.free_at(limit, base + STACK_OVER - limit);
            self.stack_free(limit, base - limit);
            self.threads[c] = Thread::default();
        }
        self.frame = self.threads[next].ctx;
        self.frame.regs.fs_base = self.kpcr_va;
        self.threads[next].fresh = false;
        self.pcr_enter(next);
        self.current = Some(next);
        let t = &mut self.threads[next];
        t.state = ThreadState::Running;
        t.run_seq = t.ready_seq; // its place, kept if it is switched out
        t.ready_seq = 0;
        if !self.is_kernel_thread(next) {
            // the rest of its quantum (an ISR or a DPC neither ends nor restarts it)
            let q = self.quantum_ns();
            let t0 = self.now();
            self.sched.slice_thread = Some(next);
            let t = &mut self.threads[next];
            t.slice_start_ns = t0;
            let left = if t.slice_used_ns < q { q - t.slice_used_ns } else { 1 };
            self.sched.slice_due = Some(t0 + left);
        }
        self.clock_arm(); // back from an ISR, its slice may be over
    }

    /// Ends the running thread.
    pub(crate) fn terminate_current(&mut self, status: u32) {
        let Some(cur) = self.current else { return };
        let t = &mut self.threads[cur];
        t.state = ThreadState::Terminated;
        let et = t.ethread;
        self.gm.kw32(et + ET_EXIT_STATUS, status);
        self.gm.kw32(et + KT_SIGNAL_STATE, 1); // the thread object is signaled
        self.resched = true;
    }

    /// The thread a handle names (NtCurrentThread is -2).
    pub(crate) fn thread_of_handle(&self, handle: u32) -> Option<usize> {
        if handle == 0xFFFF_FFFE {
            return self.current;
        }
        match self.ob_lookup(handle) {
            Some((obj, ObType::Thread)) => self.thread_of(obj),
            _ => None,
        }
    }

    fn suspend(&mut self, ti: usize) -> u32 {
        let prev = self.threads[ti].suspend;
        self.threads[ti].suspend += 1;
        if Some(ti) == self.current {
            self.resched = true;
        }
        prev
    }

    fn resume(&mut self, ti: usize) -> u32 {
        let prev = self.threads[ti].suspend;
        if prev != 0 {
            self.threads[ti].suspend -= 1;
            if prev == 1 {
                self.resched = true;
                if self.threads[ti].state == ThreadState::Waiting {
                    self.check_waits(0); // its wait, left while it was suspended
                }
            }
        }
        prev
    }

    /// Every thread for a fatal report: what it is doing or waiting for
    /// (dispatcher type, signal state), and the top of its stack.
    pub(crate) fn dump_threads(&self) -> Vec<String> {
        use std::fmt::Write;
        let now = self.now();
        let mut out = Vec::new();
        for (i, t) in self.threads.iter().enumerate() {
            if t.state == ThreadState::Free {
                continue;
            }
            let live = Some(i) == self.current && !self.sched.picking;
            let regs = if live { &self.frame.regs } else { &t.ctx.regs };
            let (eip, esp) = (regs.eip, regs.gpr[reg::ESP]);
            let state = format!("{:?}", t.state).to_lowercase();
            let mut l = format!(
                "  thread t{} {state} prio {}{} eip {eip:08x} esp {esp:08x}",
                t.id,
                t.priority,
                if t.suspend != 0 { " suspended" } else { "" }
            );
            if t.state == ThreadState::Waiting {
                let _ = write!(
                    l,
                    " | wait{}{}",
                    if t.wait.wait_all { " all" } else { "" },
                    if t.wait.alertable { " alertable" } else { "" }
                );
                if let Some(d) = t.wait.deadline {
                    let _ = write!(l, " in {} ms", (d as i64 - now as i64) / 1_000_000);
                }
                for &o in t.wait.objects.iter().take(6) {
                    let ty = self.gm.r8(o).map_or(0xFF, u32::from);
                    let sig = self.gm.r32(o + 4).map_or(-1, |v| v as i32);
                    let _ = write!(l, " {o:08x}(type {ty} sig {sig})");
                }
                if t.wait.cs != 0 {
                    let _ = write!(l, " cs {:08x}", t.wait.cs);
                }
            }
            if self.gm.mapped(esp) {
                l.push_str(" | stack");
                for w in 0..8 {
                    let Ok(v) = self.gm.r32(esp + 4 * w) else { break };
                    let _ = write!(l, " {v:08x}");
                }
                // Return address candidates deeper down: dwords inside the title image.
                l.push_str(" | calls");
                let mut found = 0;
                for w in 8..512 {
                    let Ok(v) = self.gm.r32(esp + 4 * w) else { break };
                    if v >= self.title.base && v < self.title.base + self.title.image_size {
                        let _ = write!(l, " {v:08x}");
                        found += 1;
                        if found == 12 {
                            break;
                        }
                    }
                }
            }
            out.push(l);
        }
        out
    }
}

/// PsCreateSystemThreadEx(ThreadHandle, ThreadExtensionSize, KernelStackSize, TlsDataSize,
///   ThreadId, StartRoutine, StartContext, CreateSuspended, DebuggerThread, SystemRoutine).
/// TLS data sits at the top of the stack. With a SystemRoutine the thread starts in
/// SystemRoutine(StartRoutine, StartContext), else in StartRoutine(StartContext).
fn ps_create_system_thread_ex(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let Some(ti) = k.thread_create(a[2], a[3], a[1]) else {
        return Ok(u64::from(STATUS_INSUFFICIENT_RESOURCES));
    };
    let et = k.threads[ti].ethread;
    k.gm.kw32(et + ET_START_ADDRESS, a[5]);
    let mut esp = k.threads[ti].frame_top - 16;
    let ret = Kernel::trap(ORD_THREAD_RETURN);
    let eip = if a[9] != 0 {
        esp -= 12;
        k.gm.w32(esp, ret)?;
        k.gm.w32(esp + 4, a[5])?;
        k.gm.w32(esp + 8, a[6])?;
        a[9]
    } else {
        esp -= 8;
        k.gm.w32(esp, ret)?;
        k.gm.w32(esp + 4, a[6])?;
        a[5]
    };
    k.threads[ti].ctx = k.context_init(eip, esp);

    let h = k.ob_handle(et, ObType::Thread);
    let id = k.threads[ti].id;
    k.threads[ti].state = ThreadState::Ready;
    k.threads[ti].suspend = u32::from(a[7] & 0xFF != 0);
    let t = &k.threads[ti];
    log::info!(target: "kernel", "ps: thread {id} created: start {:08x} ({}), stack {:08x}-{:08x}, tls {}{}, handle {h:x}",
        a[5], if a[9] != 0 { "via system routine" } else { "direct" }, t.stack_limit, t.stack_base, a[3],
        if t.suspend != 0 { ", suspended" } else { "" });
    if a[0] != 0 {
        k.gm.w32(a[0], h)?;
    }
    if a[4] != 0 {
        k.gm.w32(a[4], id)?;
    }
    Ok(u64::from(STATUS_SUCCESS))
}

/// NtSuspendThread(ThreadHandle, PreviousSuspendCount)
fn nt_suspend_thread(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let Some(t) = k.thread_of_handle(a[0]) else {
        return Ok(u64::from(STATUS_INVALID_HANDLE));
    };
    let prev = k.suspend(t);
    if a[1] != 0 {
        k.gm.w32(a[1], prev)?;
    }
    Ok(u64::from(STATUS_SUCCESS))
}

/// NtResumeThread(ThreadHandle, PreviousSuspendCount)
fn nt_resume_thread(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let Some(t) = k.thread_of_handle(a[0]) else {
        // a thread that has exited: nothing to resume (a valid handle still)
        if !matches!(k.ob_lookup(a[0]), Some((_, ObType::Thread))) {
            return Ok(u64::from(STATUS_INVALID_HANDLE));
        }
        if a[1] != 0 {
            k.gm.w32(a[1], 0)?;
        }
        return Ok(u64::from(STATUS_SUCCESS));
    };
    let prev = k.resume(t);
    if a[1] != 0 {
        k.gm.w32(a[1], prev)?;
    }
    Ok(u64::from(STATUS_SUCCESS))
}

/// KeSuspendThread(Thread) -> previous suspend count
fn ke_suspend_thread(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    Ok(k.thread_of(a[0]).map_or(0, |t| u64::from(k.suspend(t))))
}

/// KeResumeThread(Thread) -> previous suspend count
fn ke_resume_thread(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    Ok(k.thread_of(a[0]).map_or(0, |t| u64::from(k.resume(t))))
}

/// PsTerminateSystemThread(ExitStatus): never returns.
fn ps_terminate_system_thread(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    log::info!(target: "kernel", "ps: thread {} exits ({:08x})",
        k.current.map_or(0, |t| k.threads[t].id), a[0]);
    k.terminate_current(a[0]);
    Ok(0)
}

pub(crate) fn register(k: &mut Kernel) {
    k.imp("PsCreateSystemThreadEx", ps_create_system_thread_ex);
    k.imp("PsTerminateSystemThread", ps_terminate_system_thread);
    k.imp("NtSuspendThread", nt_suspend_thread);
    k.imp("NtResumeThread", nt_resume_thread);
    k.imp("KeSuspendThread", ke_suspend_thread);
    k.imp("KeResumeThread", ke_resume_thread);
}

impl xb_types::state::Stateful for ThreadState {
    fn state(&mut self, io: &mut xb_types::state::StateIo) {
        let mut v = *self as u8;
        v.state(io);
        if io.loading() {
            *self = match v {
                1 => Self::Ready,
                2 => Self::Running,
                3 => Self::Waiting,
                4 => Self::Suspended,
                5 => Self::Terminated,
                _ => Self::Free,
            };
        }
    }
}

xb_types::state_fields!(Wait { objects, wait_all, deadline, cs, alertable, hold });
xb_types::state_fields!(Callout { ret, esp, irql });
xb_types::state_fields!(Thread {
    state,
    ethread,
    id,
    priority,
    stack_base,
    stack_limit,
    tib_base,
    frame_top,
    exception_list,
    wait,
    suspend,
    callouts,
    apcs,
    apc,
    apc_outer,
    seh,
    fresh,
    ready_seq,
    run_seq,
    slice_used_ns,
    slice_start_ns,
    slice_expired,
    boost,
    no_boost,
    saturation,
    port_out,
    ctx
});
xb_types::state_fields!(Sched {
    next_stack_va,
    stack_free,
    next_thread_id,
    slice_thread,
    slice_due,
    ready_clock,
    resume_first,
    yield_from,
    yield_to,
    force_switch,
    picking,
    pick_from,
    idle_since,
    poll_last,
    poll_last_ns,
    poll_since,
    polls,
    delay_last,
    delay_last_ns,
    delay_since
});
