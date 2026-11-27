//! Timers and DPCs.
//!
//! KTIMER (nxdk xboxkrnl.h, CC0): Header 0x00 (Type 8 notification / 9
//! synchronization, Inserted 0x03, SignalState 0x04), DueTime 0x10 (interrupt
//! time), TimerListEntry 0x18, Dpc 0x20, Period 0x24 (ms); 0x28 bytes.
//! KDPC: Type 0x00, Inserted 0x02, DpcListEntry 0x04, DeferredRoutine 0x0C,
//! DeferredContext 0x10, SystemArgument1 0x14, SystemArgument2 0x18.
//!
//! Inserted timers are kept on the host with a guest-time due time. They are
//! checked at kernel entries (the clock thread makes one when a timer is
//! due) and while the scheduler idles; an expired timer is signaled, queues
//! its DPC and, if periodic, is inserted again.
//!
//! DPCs run as guest code on a kernel thread that outranks every guest
//! thread, at DISPATCH_LEVEL: DeferredRoutine(Dpc, DeferredContext,
//! SystemArgument1, SystemArgument2), returning to a sentinel that runs the
//! next one. They are dispatched when the scheduler runs below
//! DISPATCH_LEVEL.

use std::collections::VecDeque;

use crate::apc::Apc;
use crate::gm::KResult;
use crate::kernel::{
    Args, KPCR_DPC_PENDING, Kernel, ORD_DPC_RETURN, ObType, STATUS_INSUFFICIENT_RESOURCES,
    STATUS_INVALID_HANDLE, STATUS_SUCCESS,
};
use crate::platform::Platform;
use crate::ps::ThreadState;

const KT_INSERTED: u32 = 0x03;
const KT_SIGNAL: u32 = 0x04;
const KT_DUE: u32 = 0x10;
const KT_LIST: u32 = 0x18;
const KT_DPC: u32 = 0x20;
const KT_PERIOD: u32 = 0x24;
const KTIMER_SIZE: u32 = 0x28;

const DPC_INSERTED: u32 = 0x02;
const DPC_ROUTINE: u32 = 0x0C;
const DPC_CONTEXT: u32 = 0x10;
const DPC_ARG1: u32 = 0x14;
const DPC_ARG2: u32 = 0x18;

const DISPATCH_LEVEL: u8 = 2;
const DPC_STACK: u32 = 0x4000;
const DPC_PRIORITY: i32 = 1000;

const MAX_TIMERS: usize = 512;
const MAX_DPCS: usize = 256;
const DPC_SLOW_NS: u64 = 8_000_000;

#[derive(Default)]
struct TimerEntry {
    timer: u32,
    due_ns: u64,
    period_ns: u64,
    /// NtSetTimerEx's TimerApcRoutine(TimerContext, low, high), queued to
    /// the thread that set it (ETHREAD) each time the timer expires; 0: none.
    apc: u32,
    apc_ctx: u32,
    apc_thread: u32,
}

/// The timers' and DPCs' state.
#[derive(Default)]
pub(crate) struct State {
    timers: Vec<TimerEntry>,
    /// Earliest inserted timer (guest ns).
    pub due: Option<u64>,
    /// Queued DPCs and when each went in.
    dpcs: VecDeque<(u32, u64)>,
    /// Runs DPC routines; created on first use.
    pub dpc_thread: Option<usize>,
    pub in_dpc: bool,
    dpc_saved_irql: u8,
    run_routine: u32,
    run_start_ns: u64,
    run_wait_ns: u64,
    slow_logged: u32,
    slow_window_ns: u64,
}

impl Kernel {
    fn timer_next_due(&mut self) {
        self.timer.due = self.timer.timers.iter().map(|t| t.due_ns).min();
        self.clock_arm();
    }

    // ---------------------------------------------------------------- DPCs

    /// Whether DPCs are queued, where the guest-code KfLowerIrql reads it
    /// (`fs:[0x800]`): it enters the kernel only then.
    fn dpc_sync(&self) {
        self.gm.kw8(self.kpcr_va + KPCR_DPC_PENDING, u8::from(!self.timer.dpcs.is_empty()));
    }

    /// Queues a DPC with its system arguments; false if it was already
    /// queued.
    pub(crate) fn dpc_queue(&mut self, dpc: u32, arg1: u32, arg2: u32) -> KResult<bool> {
        if self.gm.r8(dpc + DPC_INSERTED)? != 0 || self.timer.dpcs.len() == MAX_DPCS {
            return Ok(false);
        }
        self.gm.w8(dpc + DPC_INSERTED, 1)?;
        self.gm.w32(dpc + DPC_ARG1, arg1)?;
        self.gm.w32(dpc + DPC_ARG2, arg2)?;
        let now = self.now();
        self.timer.dpcs.push_back((dpc, now));
        self.dpc_sync();
        if self.irql() < DISPATCH_LEVEL {
            self.resched = true;
        }
        Ok(true)
    }

    pub(crate) fn dpc_pending(&self) -> bool {
        !self.timer.dpcs.is_empty()
    }

    fn dpc_unqueue(&mut self, dpc: u32) -> bool {
        let Some(i) = self.timer.dpcs.iter().position(|d| d.0 == dpc) else { return false };
        self.timer.dpcs.remove(i);
        self.dpc_sync();
        let _ = self.gm.w8(dpc + DPC_INSERTED, 0);
        true
    }

    /// Readies the DPC thread for the next queued DPC (scheduler, below
    /// DISPATCH_LEVEL).
    pub(crate) fn dpc_dispatch(&mut self) {
        if self.timer.dpcs.is_empty() || self.timer.in_dpc || self.irql() >= DISPATCH_LEVEL {
            return;
        }
        if self.timer.dpc_thread.is_none() {
            self.timer.dpc_thread = self.kernel_thread(DPC_STACK, DPC_PRIORITY);
        }
        let Some(ti) = self.timer.dpc_thread else { return };
        let Some((dpc, queued)) = self.timer.dpcs.pop_front() else { return };
        self.dpc_sync();
        let _ = self.gm.w8(dpc + DPC_INSERTED, 0);
        let rd = |off: u32| self.gm.r32(dpc + off).unwrap_or(0);
        let (routine, context, arg1, arg2) =
            (rd(DPC_ROUTINE), rd(DPC_CONTEXT), rd(DPC_ARG1), rd(DPC_ARG2));
        let now = self.now();
        self.timer.run_routine = routine;
        self.timer.run_start_ns = now;
        self.timer.run_wait_ns = now.saturating_sub(queued);

        let esp = self.threads[ti].frame_top() - 16 - 20;
        let _ = self.gm.w32(esp, Self::trap(ORD_DPC_RETURN));
        let _ = self.gm.w32(esp + 4, dpc);
        let _ = self.gm.w32(esp + 8, context);
        let _ = self.gm.w32(esp + 12, arg1);
        let _ = self.gm.w32(esp + 16, arg2);
        let ctx = self.context_init(routine, esp);
        let t = &mut self.threads[ti];
        t.ctx = ctx;
        t.fresh = true;
        t.state = ThreadState::Ready;
        self.timer.in_dpc = true;
        self.timer.dpc_saved_irql = self.irql();
        self.set_irql(DISPATCH_LEVEL);
    }

    /// A DPC routine returned to the sentinel.
    pub(crate) fn dpc_return(&mut self) {
        let now = self.now();
        let run = now.saturating_sub(self.timer.run_start_ns);
        let wait = self.timer.run_wait_ns;
        self.stats.dpc_wait_max_ns = self.stats.dpc_wait_max_ns.max(wait);
        self.stats.dpc_run_max_ns = self.stats.dpc_run_max_ns.max(run);
        if wait > DPC_SLOW_NS || run > DPC_SLOW_NS {
            self.stats.dpc_slow += 1;
            if now - self.timer.slow_window_ns > 1_000_000_000 {
                self.timer.slow_window_ns = now;
                self.timer.slow_logged = 0;
            }
            self.timer.slow_logged += 1;
            if self.timer.slow_logged <= 20 {
                log::debug!(target: "kernel", "dpc {:08x} waited {:.1} ms, ran {:.1} ms",
                    self.timer.run_routine, wait as f64 / 1e6, run as f64 / 1e6);
            }
        }
        self.timer.in_dpc = false;
        self.set_irql(self.timer.dpc_saved_irql);
        if let Some(ti) = self.timer.dpc_thread {
            self.threads[ti].state = ThreadState::Suspended;
        }
        self.resched = true;
    }

    // -------------------------------------------------------------- timers

    fn timer_find(&self, timer: u32) -> Option<usize> {
        self.timer.timers.iter().position(|t| t.timer == timer)
    }

    fn timer_cancel(&mut self, timer: u32) -> bool {
        let _ = self.gm.w8(timer + KT_INSERTED, 0);
        let Some(i) = self.timer_find(timer) else { return false };
        self.timer.timers.swap_remove(i);
        self.timer_next_due();
        true
    }

    /// A timer object out of the timer queue before its memory goes (ob.rs).
    pub(crate) fn timer_forget(&mut self, timer: u32) {
        self.timer_cancel(timer);
    }

    fn timer_init(&mut self, timer: u32, ty: u32) -> KResult<()> {
        self.timer_cancel(timer);
        self.gm.fill(timer, 0, KTIMER_SIZE)?;
        self.gm.w8(timer, 8 + (ty & 1) as u8)?;
        self.gm.w8(timer + 2, (KTIMER_SIZE / 4) as u8)?;
        self.gm.w32(timer + 0x08, timer + 0x08)?;
        self.gm.w32(timer + 0x0C, timer + 0x08)?;
        self.gm.w32(timer + KT_LIST, timer + KT_LIST)?;
        self.gm.w32(timer + KT_LIST + 4, timer + KT_LIST)
    }

    /// Inserts the timer: `due` is an NT time (negative relative, else
    /// absolute system time, in 100 ns), period in ms. Returns whether it
    /// was inserted before.
    fn timer_set(&mut self, timer: u32, due: i64, period_ms: u32, dpc: u32) -> KResult<bool> {
        let was = self.timer_cancel(timer);
        self.gm.w32(timer + KT_SIGNAL, 0)?;
        self.gm.w32(timer + KT_DPC, dpc)?;
        self.gm.w32(timer + KT_PERIOD, period_ms)?;
        let rel =
            if due < 0 { due.wrapping_neg() } else { due.wrapping_sub(self.system_time() as i64) }
                .max(0) as u64;
        self.gm.w64(timer + KT_DUE, self.interrupt_time().wrapping_add(rel))?;
        if self.timer.timers.len() == MAX_TIMERS {
            log::error!(target: "kernel", "ke: out of timer slots");
            return Ok(was);
        }
        let due_ns = self.now().saturating_add(rel.saturating_mul(100));
        self.timer.timers.push(TimerEntry {
            timer,
            due_ns,
            period_ns: u64::from(period_ms) * 1_000_000,
            apc: 0,
            apc_ctx: 0,
            apc_thread: 0,
        });
        self.gm.w8(timer + KT_INSERTED, 1)?;
        self.timer_next_due();
        Ok(was)
    }

    /// Fires expired timers; returns the next due time and whether one fired.
    pub(crate) fn timer_poll(&mut self) -> (Option<u64>, bool) {
        let Some(due) = self.timer.due else { return (None, false) };
        let now = self.now();
        if now < due {
            return (Some(due), false);
        }
        let systime = self.system_time();
        let mut fired = false;
        let mut i = 0;
        while i < self.timer.timers.len() {
            if self.timer.timers[i].due_ns > now {
                i += 1;
                continue;
            }
            let (timer, apc, apc_ctx, apc_thread) = {
                let t = &self.timer.timers[i];
                (t.timer, t.apc, t.apc_ctx, t.apc_thread)
            };
            let _ = self.gm.w32(timer + KT_SIGNAL, 1);
            let dpc = self.gm.r32(timer + KT_DPC).unwrap_or(0);
            if dpc != 0 {
                let _ = self.dpc_queue(dpc, systime as u32, (systime >> 32) as u32);
            }
            if apc != 0
                && let Some(at) = self.thread_of(apc_thread)
            {
                // delivered when the thread next waits alertably, as NT's user APCs
                self.apc_queue(
                    at,
                    Apc {
                        kapc: 0,
                        normal: apc,
                        ctx: apc_ctx,
                        a1: systime as u32,
                        a2: (systime >> 32) as u32,
                    },
                );
            }
            fired = true;
            let t = &mut self.timer.timers[i];
            if t.period_ns != 0 {
                let next = t.due_ns + t.period_ns;
                t.due_ns = if next > now { next } else { now + t.period_ns };
                i += 1;
            } else {
                let _ = self.gm.w8(timer + KT_INSERTED, 0);
                self.timer.timers.swap_remove(i);
            }
        }
        self.timer_next_due();
        (self.timer.due, fired)
    }

    fn timer_of(&self, handle: u32) -> u32 {
        match self.ob_lookup(handle) {
            Some((obj, ObType::Timer)) => obj,
            _ => 0,
        }
    }
}

/// KeInsertQueueDpc(Dpc, SystemArgument1, SystemArgument2) -> TRUE if queued now
fn ke_insert_queue_dpc(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    Ok(u64::from(k.dpc_queue(a[0], a[1], a[2])?))
}

/// KeRemoveQueueDpc(Dpc) -> TRUE if it was queued
fn ke_remove_queue_dpc(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    Ok(u64::from(k.dpc_unqueue(a[0])))
}

/// KeIsExecutingDpc()
fn ke_is_executing_dpc(k: &mut Kernel, _p: &mut dyn Platform, _a: &Args) -> KResult<u64> {
    Ok(u64::from(k.timer.in_dpc))
}

/// KeInitializeTimerEx(Timer, Type)
fn ke_initialize_timer_ex(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    k.timer_init(a[0], a[1])?;
    Ok(0)
}

fn due_arg(a: &Args) -> i64 {
    (u64::from(a[1]) | u64::from(a[2]) << 32) as i64
}

/// KeSetTimer(Timer, DueTime (by value), Dpc) -> was inserted
fn ke_set_timer(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    Ok(u64::from(k.timer_set(a[0], due_arg(a), 0, a[3])?))
}

/// KeSetTimerEx(Timer, DueTime (by value), Period, Dpc) -> was inserted
fn ke_set_timer_ex(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    Ok(u64::from(k.timer_set(a[0], due_arg(a), a[3], a[4])?))
}

/// KeCancelTimer(Timer) -> was inserted
fn ke_cancel_timer(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    Ok(u64::from(k.timer_cancel(a[0])))
}

/// NtCreateTimer(TimerHandle, ObjectAttributes, TimerType)
fn nt_create_timer(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let timer = k.ob_create(ObType::Timer, KTIMER_SIZE);
    if timer == 0 {
        return Ok(u64::from(STATUS_INSUFFICIENT_RESOURCES));
    }
    k.timer_init(timer, a[2])?;
    let h = k.ob_handle(timer, ObType::Timer);
    k.gm.w32(a[0], h)?;
    Ok(u64::from(STATUS_SUCCESS))
}

/// NtSetTimerEx(TimerHandle, DueTime*, TimerApcRoutine, ApcMode, TimerContext, ResumeTimer,
///   Period, PreviousState*)
fn nt_set_timer_ex(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let timer = k.timer_of(a[0]);
    if timer == 0 {
        return Ok(u64::from(STATUS_INVALID_HANDLE));
    }
    if a[7] != 0 {
        k.gm.w8(a[7], u8::from(k.gm.r32(timer + KT_SIGNAL)? != 0))?;
    }
    let due = k.gm.r64(a[1])? as i64;
    k.timer_set(timer, due, a[6], 0)?;
    if a[2] != 0
        && let Some(cur) = k.current
        && let Some(i) = k.timer_find(timer)
    {
        let e = &mut k.timer.timers[i];
        e.apc = a[2];
        e.apc_ctx = a[4];
        e.apc_thread = k.threads[cur].ethread;
    }
    Ok(u64::from(STATUS_SUCCESS))
}

/// NtCancelTimer(TimerHandle, CurrentState*)
fn nt_cancel_timer(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let timer = k.timer_of(a[0]);
    if timer == 0 {
        return Ok(u64::from(STATUS_INVALID_HANDLE));
    }
    k.timer_cancel(timer);
    if a[1] != 0 {
        k.gm.w8(a[1], u8::from(k.gm.r32(timer + KT_SIGNAL)? != 0))?;
    }
    Ok(u64::from(STATUS_SUCCESS))
}

/// NtQueryTimer(TimerHandle, TimerInformation): TIMER_BASIC_INFORMATION { RemainingTime, TimerState }
fn nt_query_timer(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let timer = k.timer_of(a[0]);
    if timer == 0 {
        return Ok(u64::from(STATUS_INVALID_HANDLE));
    }
    let now = k.now();
    let left =
        k.timer_find(timer).map_or(0, |i| k.timer.timers[i].due_ns.saturating_sub(now) / 100);
    k.gm.w64(a[1], left)?;
    k.gm.w8(a[1] + 8, u8::from(k.gm.r32(timer + KT_SIGNAL)? != 0))?;
    Ok(u64::from(STATUS_SUCCESS))
}

pub(crate) fn register(k: &mut Kernel) {
    k.imp("KeInitializeTimerEx", ke_initialize_timer_ex);
    k.imp("KeSetTimer", ke_set_timer);
    k.imp("KeSetTimerEx", ke_set_timer_ex);
    k.imp("KeCancelTimer", ke_cancel_timer);
    k.imp("NtCreateTimer", nt_create_timer);
    k.imp("NtSetTimerEx", nt_set_timer_ex);
    k.imp("NtCancelTimer", nt_cancel_timer);
    k.imp("NtQueryTimer", nt_query_timer);
    k.imp("KeInsertQueueDpc", ke_insert_queue_dpc);
    k.imp("KeRemoveQueueDpc", ke_remove_queue_dpc);
    k.imp("KeIsExecutingDpc", ke_is_executing_dpc);
}

xb_types::state_fields!(TimerEntry { timer, due_ns, period_ns, apc, apc_ctx, apc_thread });
xb_types::state_fields!(State {
    timers,
    due,
    dpcs,
    dpc_thread,
    in_dpc,
    dpc_saved_irql,
    run_routine,
    run_start_ns,
    run_wait_ns
});
