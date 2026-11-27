//! Time, dispatcher objects, waits, IRQL.
//!
//! Dispatcher objects live in guest memory with the Xbox layouts (nxdk
//! xboxkrnl.h, CC0): DISPATCHER_HEADER { Type, Absolute, Size, Inserted,
//! SignalState 0x04, WaitListHead 0x08 }; KMUTANT OwnerThread 0x18;
//! KSEMAPHORE Limit 0x10. RTL_CRITICAL_SECTION is a synchronization KEVENT
//! followed by LockCount 0x10, RecursionCount 0x14 and OwningThread 0x18.
//!
//! Waits don't use the guest's wait blocks: a waiting thread's objects are
//! kept on the host and rechecked whenever something is signaled and when the
//! scheduler runs. A blocked kernel call returns its status later, written
//! into the thread's saved EAX when the wait ends.
//!
//! Not here: the reference's per-entry clock cache and published time, which
//! save clock reads that cost a trap on its ARM hosts and a few nanoseconds
//! here.

use std::time::Duration;

use xb_cpu::reg;

use crate::gm::KResult;
use crate::kernel::{
    Args, Kernel, ObType, STATUS_INSUFFICIENT_RESOURCES, STATUS_INVALID_HANDLE,
    STATUS_INVALID_PARAMETER, STATUS_OBJECT_TYPE_MISMATCH, STATUS_SUCCESS, STATUS_TIMEOUT,
};
use crate::platform::Platform;
use crate::ps::{MAX_WAIT_OBJECTS, ThreadState, Wait};

const OBJ_SYNCHRONIZATION_EVENT: u8 = 1;
const OBJ_MUTANT: u8 = 2;
const OBJ_QUEUE: u8 = 4;
const OBJ_SEMAPHORE: u8 = 5;
const OBJ_SYNCHRONIZATION_TIMER: u8 = 9;

const HDR_SIGNAL_STATE: u32 = 0x04;
const HDR_WAIT_LIST: u32 = 0x08;
const MUTANT_OWNER: u32 = 0x18;
const SEMAPHORE_LIMIT: u32 = 0x10;
const CS_LOCK_COUNT: u32 = 0x10;
const CS_RECURSION: u32 = 0x14;
const CS_OWNER: u32 = 0x18;

const WAIT_ALL: u32 = 0;

/// FILE_OBJECT.Event (a KEVENT): what a wait on a file handle waits for, set
/// when an I/O on the file issued without an event of its own completes.
pub(crate) const FO_EVENT: u32 = 0x38;

/// The Xbox's performance counter runs at the ACPI timer's 3.375 MHz.
const PERF_FREQUENCY: u64 = 3_375_000;

/// The system time's origin, 2001-01-01 as a FILETIME (100 ns since 1601):
/// the system time is it + the offset + the guest's time since boot.
const BOOT_FILETIME: u64 = 0x01C0_739B_EB8F_8000;

/// NT's EVENT_INCREMENT, SEMAPHORE_INCREMENT, MUTANT_INCREMENT: the Nt calls'
/// (and I/O completion's).
pub(crate) const SIGNAL_INCREMENT: i32 = 1;

const PORT_PACKETS: usize = 512;

#[derive(Clone, Copy, Default)]
struct PortPacket {
    key: u32,
    ctx: u32,
    status: u32,
    info: u32,
}

#[derive(Default)]
struct Port {
    obj: u32,
    packets: std::collections::VecDeque<PortPacket>,
}

/// The dispatcher's state.
#[derive(Default)]
pub(crate) struct State {
    /// Guest time at boot.
    pub boot_ns: u64,
    /// The console's clock at boot past BOOT_FILETIME, and what
    /// NtSetSystemTime moved it since; 100 ns (wraps for negative).
    pub time_offset: u64,
    /// KeTickCount, KeInterruptTime and KeSystemTime were last written for
    /// this time (100 ns since boot): only ever forward.
    times_t: u64,
    /// Earliest wait timeout of a waiting thread.
    pub wait_due: Option<u64>,
    ports: Vec<Port>,
    port_warned: u32,
    sem_told: bool,
}

impl Kernel {
    // ---------------------------------------------------------------- time

    /// Guest time now.
    #[inline]
    pub(crate) fn now(&self) -> u64 {
        self.clock.now_ns()
    }

    fn since_boot_100ns(&self) -> u64 {
        self.now().saturating_sub(self.ke.boot_ns) / 100
    }

    /// Now as NT's system time (FILETIME, 100 ns).
    pub(crate) fn system_time(&self) -> u64 {
        BOOT_FILETIME.wrapping_add(self.ke.time_offset).wrapping_add(self.since_boot_100ns())
    }

    /// Now as NT's interrupt time (100 ns since boot).
    pub(crate) fn interrupt_time(&self) -> u64 {
        self.since_boot_100ns()
    }

    fn wr_ksystem_time(&self, va: u32, t: u64) {
        self.gm.kw32(va + 8, (t >> 32) as u32); // High2Time
        self.gm.kw32(va, t as u32); // LowPart
        self.gm.kw32(va + 4, (t >> 32) as u32); // High1Time
    }

    /// Refreshes KeTickCount, KeInterruptTime and KeSystemTime.
    pub(crate) fn update_time(&mut self) {
        let t = self.since_boot_100ns().max(self.ke.times_t);
        self.ke.times_t = t;
        self.gm.kw32(self.data_addr_tick(), (t / 10000) as u32);
        self.wr_ksystem_time(self.data_va[crate::ordinals::KE_INTERRUPT_TIME], t);
        self.wr_ksystem_time(
            self.data_va[crate::ordinals::KE_SYSTEM_TIME],
            BOOT_FILETIME.wrapping_add(self.ke.time_offset).wrapping_add(t),
        );
    }

    #[inline]
    fn data_addr_tick(&self) -> u32 {
        self.data_va[crate::ordinals::KE_TICK_COUNT]
    }

    /// The same at a kernel entry.
    #[inline]
    pub(crate) fn tick_time(&mut self) {
        self.update_time();
    }

    /// The console's clock at boot: the host's time moved by the settings'
    /// offset, never before BOOT_FILETIME (`clock_boot`).
    fn clock_boot(&mut self) {
        let unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or(Duration::ZERO);
        // Unix time is 11644473600 s past the FILETIME's 1601
        let t = (unix.as_secs() as i64 + 11_644_473_600) * 10_000_000
            + i64::from(unix.subsec_nanos() / 100)
            + self.console.clock_offset_ms * 10_000;
        self.ke.time_offset = if t > BOOT_FILETIME as i64 { t as u64 - BOOT_FILETIME } else { 0 };
        let secs = (BOOT_FILETIME + self.ke.time_offset) / 10_000_000 - 11_644_473_600;
        log::info!(target: "kernel", "ke: console clock {} s after the Unix epoch (UTC), {}", secs,
            if self.console.clock_offset_ms != 0 { "as set in the settings" } else { "the host's time" });
    }

    // ---------------------------------------------------------- dispatcher

    fn signal_state(&self, obj: u32) -> KResult<i32> {
        Ok(self.gm.r32(obj + HDR_SIGNAL_STATE)? as i32)
    }

    fn set_signal_state(&self, obj: u32, v: i32) -> KResult<()> {
        self.gm.w32(obj + HDR_SIGNAL_STATE, v as u32)
    }

    fn is_signaled(&self, obj: u32, ethread: u32) -> bool {
        let Ok(ty) = self.gm.r8(obj) else { return false };
        let sig = self.signal_state(obj).unwrap_or(0);
        if ty == OBJ_MUTANT {
            return sig > 0 || self.gm.r32(obj + MUTANT_OWNER).unwrap_or(0) == ethread;
        }
        sig > 0
    }

    fn acquire(&self, obj: u32, ethread: u32) {
        let Ok(ty) = self.gm.r8(obj) else { return };
        let sig = self.signal_state(obj).unwrap_or(0);
        match ty {
            OBJ_SYNCHRONIZATION_EVENT | OBJ_SYNCHRONIZATION_TIMER => {
                let _ = self.set_signal_state(obj, 0);
            }
            OBJ_SEMAPHORE => {
                let _ = self.set_signal_state(obj, sig - 1);
            }
            OBJ_MUTANT => {
                let _ = self.set_signal_state(obj, sig - 1);
                let _ = self.gm.w32(obj + MUTANT_OWNER, ethread);
            }
            _ => {} // notification objects and threads stay signaled
        }
    }

    /// Index of the object that satisfies the wait (wait-any), 0 for a
    /// satisfied wait-all, or `None`.
    fn satisfied(&self, ethread: u32, w: &Wait) -> Option<usize> {
        if w.wait_all {
            return (!w.objects.is_empty()
                && w.objects.iter().all(|&o| self.is_signaled(o, ethread)))
            .then_some(0);
        }
        w.objects.iter().position(|&o| self.is_signaled(o, ethread))
    }

    fn take(&mut self, ti: usize, w: &Wait, idx: usize) {
        let ethread = self.threads[ti].ethread;
        if !w.wait_all && self.gm.r8(w.objects[idx]) == Ok(OBJ_QUEUE) {
            self.port_take(ti, w.objects[idx]);
            return;
        }
        if w.wait_all {
            for &o in &w.objects {
                self.acquire(o, ethread);
            }
        } else {
            self.acquire(w.objects[idx], ethread);
        }
        if w.cs != 0 {
            let _ = self.gm.w32(w.cs + CS_OWNER, ethread);
            let _ = self.gm.w32(w.cs + CS_RECURSION, 1);
        }
    }

    /// NT's KiUnwaitThread: a thread below the real-time range a signal woke
    /// runs at its priority + the increment (up to 15), unless that is no
    /// more than it has. Without it a thread woken at the priority of one
    /// spinning waited out the spinner's quantum: Nightfire's main loop
    /// polls a counter its worker advances, and the worker ran 20 ms late
    /// every few frames.
    pub(crate) fn boost(&mut self, ti: usize, inc: i32) {
        let t = &mut self.threads[ti];
        if inc <= 0 || t.no_boost || t.priority >= 16 {
            return;
        }
        let b = (t.priority + inc).min(15) - t.priority;
        if b > t.boost {
            t.boost = b;
        }
    }

    /// Ends the waits that are satisfied (`inc`: the signal's increment, 0
    /// for a timer's or none) or timed out; the earliest timeout left. A
    /// suspended thread's wait is left as it is until it is resumed: on NT
    /// its suspend APC took it out of the wait, and the signal goes to
    /// another waiter or stays (AMF Bowling 2004 suspends its cache copier
    /// while it waits on the game's file lock).
    pub(crate) fn check_waits(&mut self, inc: i32) -> Option<u64> {
        let now = self.now();
        let mut earliest: Option<u64> = None;
        for i in 0..self.threads_hi {
            if self.threads[i].state != ThreadState::Waiting || self.threads[i].suspend != 0 {
                continue;
            }
            let ethread = self.threads[i].ethread;
            if let Some(idx) = self.satisfied(ethread, &self.threads[i].wait) {
                let w = std::mem::take(&mut self.threads[i].wait);
                self.take(i, &w, idx);
                let t = &mut self.threads[i];
                t.ctx.regs.gpr[reg::EAX] = idx as u32;
                t.state = ThreadState::Ready;
                self.boost(i, inc);
            } else if let Some(seq) = self.threads[i].wait.hold {
                // A frame-latency hold: until a flip, or its deadline; the
                // thread was between two instructions, its registers stay.
                let d = self.threads[i].wait.deadline.unwrap_or(now);
                let flipped = self.flip_hold.flips() != seq;
                if flipped || now >= d {
                    self.threads[i].wait = Wait::default();
                    self.threads[i].state = ThreadState::Ready;
                    self.frame_hold_ended(!flipped);
                } else {
                    earliest = Some(earliest.map_or(d, |e| e.min(d)));
                }
            } else if let Some(d) = self.threads[i].wait.deadline {
                if now >= d {
                    // A delay ends successfully; a wait times out.
                    let t = &mut self.threads[i];
                    t.ctx.regs.gpr[reg::EAX] =
                        if t.wait.objects.is_empty() { STATUS_SUCCESS } else { STATUS_TIMEOUT };
                    t.state = ThreadState::Ready;
                } else {
                    earliest = Some(earliest.map_or(d, |e| e.min(d)));
                }
            }
        }
        self.ke.wait_due = earliest;
        earliest
    }

    /// After something was signaled (`inc`: its priority increment): wake
    /// waiters, and let a higher-priority thread that became ready run first.
    fn signaled(&mut self, inc: i32) {
        self.check_waits(inc);
        if let Some(cur) = self.current {
            let cp = self.threads[cur].prio();
            if self.threads[..self.threads_hi]
                .iter()
                .any(|t| t.state == ThreadState::Ready && t.prio() > cp)
            {
                self.resched = true;
            }
        }
    }

    fn ready_peer(&self, cur: usize) -> bool {
        let cp = self.threads[cur].prio();
        self.threads[..self.threads_hi].iter().enumerate().any(|(i, t)| {
            i != cur && t.state == ThreadState::Ready && t.suspend == 0 && t.prio() >= cp
        })
    }

    /// A poll (a zero timeout) found nothing signaled. One poll is NT's: the
    /// thread keeps the CPU. A thread polling back to back (Bink's scheduler
    /// in Halo, Panzer Dragoon Orta's loader: a million and a half polls a
    /// second) gives its turn to a ready thread of its priority or above, as
    /// if it had slept, and the thread it waits for runs sooner. With
    /// nothing else to run, after a millisecond it waits for the next
    /// interrupt on the host, 20 us at a time.
    fn poll_spin(&mut self) {
        let Some(cur) = self.current else { return };
        let now = self.now();
        if self.sched.poll_last != Some(cur) || now - self.sched.poll_last_ns > 20_000 {
            self.sched.polls = 0;
            self.sched.poll_since = now;
        }
        self.sched.poll_last = Some(cur);
        self.sched.polls += 1;
        if self.sched.polls >= 64 {
            if self.ready_peer(cur) {
                self.ps_yield(); // behind the ready thread
            }
            if !self.resched && now - self.sched.poll_since > 1_000_000 {
                self.wake.wait(Duration::from_micros(20));
                self.update_time();
            }
        }
        self.sched.poll_last_ns = self.now();
    }

    /// Waits on objects with an NT timeout (PLARGE_INTEGER in 100 ns: NULL
    /// forever, negative relative, positive absolute system time, zero a
    /// poll). An alertable wait runs the thread's queued APCs instead and
    /// returns STATUS_USER_APC (apc.rs).
    pub(crate) fn wait(
        &mut self,
        objs: &[u32],
        wait_all: bool,
        timeout_va: u32,
        cs: u32,
        alertable: bool,
    ) -> KResult<u32> {
        let Some(ti) = self.current else { return Ok(STATUS_SUCCESS) };
        if alertable && self.apc_pending(ti) {
            let esp = self.esp();
            let ret = self.gm.r32(esp)?;
            if self.apc_begin_frame(ti, esp + self.call_pop, ret) {
                self.redirected = true;
                return Ok(0);
            }
        }
        let mut w =
            Wait { objects: objs.to_vec(), wait_all, deadline: None, cs, alertable, hold: None };
        let ethread = self.threads[ti].ethread;
        if let Some(idx) = self.satisfied(ethread, &w) {
            self.take(ti, &w, idx);
            return Ok(idx as u32);
        }
        let count = objs.len();
        let to = if timeout_va != 0 { self.gm.r64(timeout_va)? as i64 } else { 0 };
        if timeout_va != 0 && to == i64::MIN {
            // XAPI's SleepEx(INFINITE) delays by the most negative interval:
            // forever, until alerted.
            if count == 0 {
                self.threads[ti].wait = w;
                self.threads[ti].state = ThreadState::Waiting;
                self.resched = true;
                return Ok(STATUS_SUCCESS);
            }
        } else if timeout_va != 0 {
            let rel_100ns = if to < 0 { -to } else { to.wrapping_sub(self.system_time() as i64) };
            if rel_100ns <= 0 {
                if count != 0 && self.threads[ti].state == ThreadState::Running && !self.redirected
                {
                    self.poll_spin();
                }
                return Ok(if count != 0 { STATUS_TIMEOUT } else { STATUS_SUCCESS });
            }
            let deadline = self.now().saturating_add((rel_100ns as u64).saturating_mul(100));
            w.deadline = Some(deadline);
            if self.ke.wait_due.is_none_or(|d| deadline < d) {
                self.ke.wait_due = Some(deadline);
                self.clock_arm();
            }
        } else if count == 0 {
            return Ok(STATUS_SUCCESS);
        }
        self.threads[ti].wait = w;
        self.threads[ti].state = ThreadState::Waiting;
        self.resched = true;
        Ok(STATUS_TIMEOUT) // replaced when the wait ends
    }

    /// The running thread waits for work the host does for its kernel call
    /// (a read on a worker thread, io.rs): no object, no timeout, not
    /// alertable. The call's status comes with [`Kernel::wake_thread`].
    pub(crate) fn block_current(&mut self) {
        if let Some(ti) = self.current {
            self.threads[ti].wait = Wait::default();
            self.threads[ti].state = ThreadState::Waiting;
            self.resched = true;
        }
    }

    /// Whether a waiting thread waits on `object` (an object freed
    /// meanwhile: ob.rs release).
    pub(crate) fn object_waited(&self, object: u32) -> bool {
        self.threads[..self.threads_hi]
            .iter()
            .any(|t| t.state == ThreadState::Waiting && t.wait.objects.contains(&object))
    }

    /// Ends `ti`'s [`Kernel::block_current`]: status in its EAX, ready, ahead
    /// of the running thread if its priority is higher.
    pub(crate) fn wake_thread(&mut self, ti: usize, status: u32) {
        self.threads[ti].ctx.regs.gpr[reg::EAX] = status;
        self.threads[ti].state = ThreadState::Ready;
        self.boost(ti, 1); // IO_CD_ROM_INCREMENT, IO_DISK_INCREMENT
        if let Some(cur) = self.current
            && self.threads[ti].prio() > self.threads[cur].prio()
        {
            self.resched = true;
        }
    }

    /// The dispatcher object a handle is waited on through.
    pub(crate) fn handle_object(&self, handle: u32) -> u32 {
        // NtCurrentThread (-2): a thread is signalled only once it ends, so
        // a wait on itself is an (alertable) sleep; a Kakuto Chojin thread
        // sleeps so.
        if handle == 0xFFFF_FFFE {
            return self.current.map_or(0, |t| self.threads[t].ethread);
        }
        match self.ob_lookup(handle) {
            // A file is waited on through its Event (FILE_OBJECT +0x38):
            // Bink's reads without an event of their own complete there, and
            // its scheduler polls the file handles (Halo's attract movies).
            Some((obj, ObType::File)) => obj + FO_EVENT,
            Some((obj, _)) => obj,
            None => 0,
        }
    }

    // -------------------------------------------------------------- events

    pub(crate) fn init_header(&self, obj: u32, ty: u8, size_dwords: u8, state: i32) -> KResult<()> {
        self.gm.w8(obj, ty)?;
        self.gm.w8(obj + 1, 0)?;
        self.gm.w8(obj + 2, size_dwords)?;
        self.gm.w8(obj + 3, 0)?;
        self.set_signal_state(obj, state)?;
        self.gm.w32(obj + HDR_WAIT_LIST, obj + HDR_WAIT_LIST)?;
        self.gm.w32(obj + HDR_WAIT_LIST + 4, obj + HDR_WAIT_LIST)
    }

    fn set_event_inc(&mut self, ev: u32, inc: i32) -> KResult<u32> {
        let old = self.signal_state(ev)?;
        self.set_signal_state(ev, 1)?;
        self.signaled(inc);
        Ok(old as u32)
    }

    /// KeSetEvent on a guest event object; returns the previous state. An
    /// event in memory the guest no longer has is left alone.
    pub(crate) fn set_event(&mut self, event: u32) -> u32 {
        self.set_event_inc(event, SIGNAL_INCREMENT).unwrap_or(0)
    }

    /// KeClearEvent.
    pub(crate) fn clear_event(&mut self, event: u32) {
        let _ = self.set_signal_state(event, 0);
    }

    /// KeInitializeEvent: type 0 notification, 1 synchronization.
    pub(crate) fn init_event(&mut self, event: u32, ty: u8, signaled: bool) {
        let _ = self.init_header(event, ty, 4, i32::from(signaled));
    }

    fn release_semaphore(&mut self, sem: u32, count: i32, inc: i32) -> KResult<(u32, u32)> {
        let old = self.signal_state(sem)?;
        let limit = self.gm.r32(sem + SEMAPHORE_LIMIT)? as i32;
        if count <= 0 || old.checked_add(count).is_none_or(|n| n > limit) {
            return Ok((old as u32, 0xC000_0047)); // STATUS_SEMAPHORE_LIMIT_EXCEEDED
        }
        self.set_signal_state(sem, old + count)?;
        self.signaled(inc);
        Ok((old as u32, STATUS_SUCCESS))
    }

    fn init_mutant(&mut self, m: u32, owned: bool) -> KResult<()> {
        self.init_header(m, OBJ_MUTANT, 8, i32::from(!owned))?;
        let owner = if owned { self.current.map_or(0, |t| self.threads[t].ethread) } else { 0 };
        self.gm.w32(m + MUTANT_OWNER, owner)
    }

    fn release_mutant(&mut self, m: u32, inc: i32) -> KResult<u32> {
        let old = self.signal_state(m)?;
        self.set_signal_state(m, old.wrapping_add(1))?;
        if old.wrapping_add(1) > 0 {
            self.gm.w32(m + MUTANT_OWNER, 0)?;
            self.signaled(inc);
        }
        Ok(old as u32)
    }

    // ------------------------------------------------------ I/O completion

    /// A packet on an I/O completion port (its object): an I/O of a file
    /// associated with it finished.
    pub(crate) fn port_queue(&mut self, obj: u32, key: u32, ctx: u32, status: u32, info: u32) {
        let Some(port) = self.ke.ports.iter_mut().find(|p| p.obj == obj) else {
            if self.ke.port_warned < 8 {
                self.ke.port_warned += 1;
                log::warn!(target: "kernel", "ke: completion port {obj:08x} unknown: packet dropped");
            }
            return;
        };
        if port.packets.len() == PORT_PACKETS {
            if self.ke.port_warned < 8 {
                self.ke.port_warned += 1;
                log::warn!(target: "kernel", "ke: completion port {obj:08x} full: packet dropped");
            }
            return;
        }
        port.packets.push_back(PortPacket { key, ctx, status, info });
        let n = port.packets.len() as i32;
        let _ = self.set_signal_state(obj, n);
        self.signaled(SIGNAL_INCREMENT);
    }

    /// A wait on the port satisfied: its first packet to the thread.
    fn port_take(&mut self, ti: usize, obj: u32) {
        let Some(port) = self.ke.ports.iter_mut().find(|p| p.obj == obj) else { return };
        let Some(pk) = port.packets.pop_front() else { return };
        let n = port.packets.len() as i32;
        let _ = self.set_signal_state(obj, n);
        let out = self.threads[ti].port_out;
        if out[0] != 0 {
            let _ = self.gm.w32(out[0], pk.key);
        }
        if out[1] != 0 {
            let _ = self.gm.w32(out[1], pk.ctx);
        }
        if out[2] != 0 {
            let _ = self.gm.w32(out[2], pk.status);
            let _ = self.gm.w32(out[2] + 4, pk.info);
        }
    }

    fn port_handle(&self, handle: u32) -> u32 {
        match self.ob_lookup(handle) {
            Some((obj, ObType::IoCompletion)) => obj,
            _ => 0,
        }
    }
}

fn ok(v: u32) -> KResult<u64> {
    Ok(u64::from(v))
}

/// KeInitializeEvent(Event, Type, State)
fn ke_initialize_event(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    k.init_header(a[0], a[1] as u8, 4, (a[2] & 0xFF) as i32)?;
    Ok(0)
}

/// KeSetEvent(Event, Increment, Wait) -> previous state
fn ke_set_event(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    ok(k.set_event_inc(a[0], a[1] as i32)?)
}

/// KeResetEvent(Event) -> previous state
fn ke_reset_event(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let old = k.signal_state(a[0])?;
    k.set_signal_state(a[0], 0)?;
    ok(old as u32)
}

/// KePulseEvent(Event, Increment, Wait) -> previous state
fn ke_pulse_event(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let old = k.set_event_inc(a[0], a[1] as i32)?;
    k.set_signal_state(a[0], 0)?;
    ok(old)
}

/// NtCreateEvent(EventHandle, ObjectAttributes, EventType, InitialState)
fn nt_create_event(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let name = k.ob_attr_name(a[1])?;
    if !name.is_empty()
        && let Some(status) = k.ob_create_named(a[1], &name, ObType::Event, a[0])?
    {
        return ok(status);
    }
    let ev = k.ob_create(ObType::Event, 0x10);
    if ev == 0 {
        return ok(STATUS_INSUFFICIENT_RESOURCES);
    }
    k.init_header(ev, a[2] as u8, 4, (a[3] & 0xFF) as i32)?;
    if !name.is_empty() {
        k.ob_name(ev, ObType::Event, &name);
    }
    let h = k.ob_handle(ev, ObType::Event);
    k.gm.w32(a[0], h)?;
    ok(STATUS_SUCCESS)
}

/// NtSetEvent(EventHandle, PreviousState)
fn nt_set_event(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let ev = k.handle_object(a[0]);
    if ev == 0 {
        return ok(STATUS_INVALID_HANDLE);
    }
    let old = k.set_event_inc(ev, SIGNAL_INCREMENT)?;
    if a[1] != 0 {
        k.gm.w32(a[1], old)?;
    }
    ok(STATUS_SUCCESS)
}

/// NtClearEvent(EventHandle)
fn nt_clear_event(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let ev = k.handle_object(a[0]);
    if ev == 0 {
        return ok(STATUS_INVALID_HANDLE);
    }
    k.set_signal_state(ev, 0)?;
    ok(STATUS_SUCCESS)
}

/// NtPulseEvent(EventHandle, PreviousState)
fn nt_pulse_event(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let ev = k.handle_object(a[0]);
    if ev == 0 {
        return ok(STATUS_INVALID_HANDLE);
    }
    let old = k.set_event_inc(ev, SIGNAL_INCREMENT)?;
    k.set_signal_state(ev, 0)?;
    if a[1] != 0 {
        k.gm.w32(a[1], old)?;
    }
    ok(STATUS_SUCCESS)
}

/// KeInitializeSemaphore(Semaphore, Count, Limit)
fn ke_initialize_semaphore(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    k.init_header(a[0], OBJ_SEMAPHORE, 5, a[1] as i32)?;
    k.gm.w32(a[0] + SEMAPHORE_LIMIT, a[2])?;
    Ok(0)
}

/// KeReleaseSemaphore(Semaphore, Increment, Adjustment, Wait) -> previous count.
/// Past its limit the Xbox kernel raises STATUS_SEMAPHORE_LIMIT_EXCEEDED (an
/// unhandled one ends the title); here it isn't released, and told once.
fn ke_release_semaphore(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let (old, status) = k.release_semaphore(a[0], a[2] as i32, a[1] as i32)?;
    if status != STATUS_SUCCESS && !k.ke.sem_told {
        k.ke.sem_told = true;
        log::warn!(target: "kernel", "ke: KeReleaseSemaphore {:08x} by {} past its limit {} (count {}): not released",
            a[0], a[2] as i32, k.gm.r32(a[0] + SEMAPHORE_LIMIT)? as i32, old as i32);
    }
    ok(old)
}

/// NtCreateSemaphore(SemaphoreHandle, ObjectAttributes, InitialCount, MaximumCount)
fn nt_create_semaphore(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let name = k.ob_attr_name(a[1])?;
    if !name.is_empty()
        && let Some(status) = k.ob_create_named(a[1], &name, ObType::Semaphore, a[0])?
    {
        return ok(status);
    }
    let sem = k.ob_create(ObType::Semaphore, 0x14);
    if sem == 0 {
        return ok(STATUS_INSUFFICIENT_RESOURCES);
    }
    k.init_header(sem, OBJ_SEMAPHORE, 5, a[2] as i32)?;
    k.gm.w32(sem + SEMAPHORE_LIMIT, a[3])?;
    if !name.is_empty() {
        k.ob_name(sem, ObType::Semaphore, &name);
    }
    let h = k.ob_handle(sem, ObType::Semaphore);
    k.gm.w32(a[0], h)?;
    ok(STATUS_SUCCESS)
}

/// NtReleaseSemaphore(SemaphoreHandle, ReleaseCount, PreviousCount)
fn nt_release_semaphore(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let sem = k.handle_object(a[0]);
    if sem == 0 {
        return ok(STATUS_INVALID_HANDLE);
    }
    let (old, status) = k.release_semaphore(sem, a[1] as i32, SIGNAL_INCREMENT)?;
    if a[2] != 0 && status == STATUS_SUCCESS {
        k.gm.w32(a[2], old)?;
    }
    ok(status)
}

/// KeInitializeMutant(Mutant, InitialOwner)
fn ke_initialize_mutant(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    k.init_mutant(a[0], a[1] & 0xFF != 0)?;
    Ok(0)
}

/// KeReleaseMutant(Mutant, Increment, Abandoned, Wait) -> previous state
fn ke_release_mutant(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    ok(k.release_mutant(a[0], a[1] as i32)?)
}

/// NtCreateMutant(MutantHandle, ObjectAttributes, InitialOwner)
fn nt_create_mutant(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let name = k.ob_attr_name(a[1])?;
    if !name.is_empty()
        && let Some(status) = k.ob_create_named(a[1], &name, ObType::Mutant, a[0])?
    {
        return ok(status);
    }
    let m = k.ob_create(ObType::Mutant, 0x20);
    if m == 0 {
        return ok(STATUS_INSUFFICIENT_RESOURCES);
    }
    k.init_mutant(m, a[2] & 0xFF != 0)?;
    if !name.is_empty() {
        k.ob_name(m, ObType::Mutant, &name);
    }
    let h = k.ob_handle(m, ObType::Mutant);
    k.gm.w32(a[0], h)?;
    ok(STATUS_SUCCESS)
}

/// NtReleaseMutant(MutantHandle, PreviousCount)
fn nt_release_mutant(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let m = k.handle_object(a[0]);
    if m == 0 {
        return ok(STATUS_INVALID_HANDLE);
    }
    let old = k.release_mutant(m, SIGNAL_INCREMENT)?;
    if a[1] != 0 {
        k.gm.w32(a[1], old)?;
    }
    ok(STATUS_SUCCESS)
}

/// NtCreateIoCompletion(IoCompletionHandle, DesiredAccess, ObjectAttributes, Count).
/// An I/O completion port: a KQUEUE's dispatcher header, its signal state
/// the packets queued, the packets themselves on the host. The Thing creates
/// one as it boots.
fn nt_create_io_completion(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    if k.ke.ports.len() == 64 {
        return ok(STATUS_INSUFFICIENT_RESOURCES);
    }
    let obj = k.ob_create(ObType::IoCompletion, 0x28);
    if obj == 0 {
        return ok(STATUS_INSUFFICIENT_RESOURCES);
    }
    k.init_header(obj, OBJ_QUEUE, 10, 0)?;
    k.ke.ports.push(Port { obj, packets: std::collections::VecDeque::new() });
    let h = k.ob_handle(obj, ObType::IoCompletion);
    k.gm.w32(a[0], h)?;
    ok(STATUS_SUCCESS)
}

/// NtRemoveIoCompletion(IoCompletionHandle, KeyContext, ApcContext, IoStatusBlock, Timeout)
fn nt_remove_io_completion(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let obj = k.port_handle(a[0]);
    let Some(cur) = k.current.filter(|_| obj != 0) else {
        return ok(STATUS_INVALID_HANDLE);
    };
    k.threads[cur].port_out = [a[1], a[2], a[3]];
    ok(k.wait(&[obj], false, a[4], 0, false)?)
}

/// NtSetIoCompletion(IoCompletionHandle, KeyContext, ApcContext, IoStatus, IoStatusInformation)
fn nt_set_io_completion(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let obj = k.port_handle(a[0]);
    if obj == 0 {
        return ok(STATUS_INVALID_HANDLE);
    }
    k.port_queue(obj, a[1], a[2], a[3], a[4]);
    ok(STATUS_SUCCESS)
}

/// NtQueryIoCompletion(IoCompletionHandle, IoCompletionInformation): the depth
fn nt_query_io_completion(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let obj = k.port_handle(a[0]);
    let Some(port) = k.ke.ports.iter().find(|p| obj != 0 && p.obj == obj) else {
        return ok(STATUS_INVALID_HANDLE);
    };
    let n = port.packets.len() as u32;
    k.gm.w32(a[1], n)?;
    ok(STATUS_SUCCESS)
}

/// KeWaitForSingleObject(Object, WaitReason, WaitMode, Alertable, Timeout)
fn ke_wait_for_single_object(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    ok(k.wait(&[a[0]], false, a[4], 0, a[3] & 0xFF != 0)?)
}

/// KeWaitForMultipleObjects(Count, Object[], WaitType, WaitReason, WaitMode, Alertable,
///   Timeout, WaitBlockArray)
fn ke_wait_for_multiple_objects(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let n = (a[0] as usize).min(MAX_WAIT_OBJECTS);
    let mut objs = Vec::with_capacity(n);
    for i in 0..n {
        objs.push(k.gm.r32(a[1] + 4 * i as u32)?);
    }
    ok(k.wait(&objs, a[2] == WAIT_ALL, a[6], 0, a[5] & 0xFF != 0)?)
}

/// NtWaitForSingleObject(Handle, Alertable, Timeout)
fn nt_wait_for_single_object(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let obj = k.handle_object(a[0]);
    if obj == 0 {
        return ok(STATUS_INVALID_HANDLE);
    }
    ok(k.wait(&[obj], false, a[2], 0, a[1] & 0xFF != 0)?)
}

/// NtWaitForSingleObjectEx(Handle, WaitMode, Alertable, Timeout)
fn nt_wait_for_single_object_ex(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let obj = k.handle_object(a[0]);
    if obj == 0 {
        return ok(STATUS_INVALID_HANDLE);
    }
    ok(k.wait(&[obj], false, a[3], 0, a[2] & 0xFF != 0)?)
}

/// NtSignalAndWaitForSingleObjectEx(SignalHandle, WaitHandle, WaitMode, Alertable, Timeout):
/// the first object signalled (an event set, a semaphore released by one, a
/// mutant released), then a wait on the second, in the one call (no thread
/// runs between them). A semaphore at its limit is not waited on, as on NT.
fn nt_signal_and_wait(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let sig = k.ob_lookup(a[0]);
    let obj = k.handle_object(a[1]);
    let Some((sig, ty)) = sig.filter(|_| obj != 0) else {
        return ok(STATUS_INVALID_HANDLE);
    };
    let status = match ty {
        ObType::Event => {
            k.set_event_inc(sig, SIGNAL_INCREMENT)?;
            STATUS_SUCCESS
        }
        ObType::Semaphore => k.release_semaphore(sig, 1, SIGNAL_INCREMENT)?.1,
        ObType::Mutant => {
            k.release_mutant(sig, SIGNAL_INCREMENT)?;
            STATUS_SUCCESS
        }
        _ => STATUS_OBJECT_TYPE_MISMATCH,
    };
    if status != STATUS_SUCCESS {
        return ok(status);
    }
    ok(k.wait(&[obj], false, a[4], 0, a[3] & 0xFF != 0)?)
}

/// NtWaitForMultipleObjectsEx(Count, Handles, WaitType, WaitMode, Alertable, Timeout)
fn nt_wait_for_multiple_objects_ex(
    k: &mut Kernel,
    _p: &mut dyn Platform,
    a: &Args,
) -> KResult<u64> {
    let n = (a[0] as usize).min(MAX_WAIT_OBJECTS);
    let mut objs = Vec::with_capacity(n);
    for i in 0..n {
        let o = k.handle_object(k.gm.r32(a[1] + 4 * i as u32)?);
        if o == 0 {
            return ok(STATUS_INVALID_HANDLE);
        }
        objs.push(o);
    }
    ok(k.wait(&objs, a[2] == WAIT_ALL, a[5], 0, a[4] & 0xFF != 0)?)
}

/// KeDelayExecutionThread(WaitMode, Alertable, Interval)
fn ke_delay_execution_thread(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let st = k.wait(&[], false, a[2], 0, a[1] & 0xFF != 0)?;
    if let Some(cur) = k.current
        && k.threads[cur].state == ThreadState::Running
        && !k.redirected
    {
        // A delay that has nothing to wait for (Sleep(0), or one already
        // elapsed) gives the rest of the slice to a ready thread of the
        // same or higher priority, as NT does.
        if k.ready_peer(cur) {
            k.ps_yield(); // behind the ready thread
        }
        // A thread spinning on them with nothing else to run: polling (D3D
        // waits on the GPU with Sleep(0)) gets them back at once for its
        // first millisecond; a spin that goes on (Fable's frame limiter
        // spends what is left of the frame, two million calls a second)
        // waits for the next interrupt on the host, 20 us at a time.
        let now = k.now();
        let spinning =
            !k.resched && k.sched.delay_last == Some(cur) && now - k.sched.delay_last_ns < 20_000;
        if !spinning {
            k.sched.delay_since = now;
        } else if now - k.sched.delay_since > 1_000_000 {
            k.wake.wait(Duration::from_micros(20));
            k.update_time();
        }
        k.sched.delay_last = Some(cur);
        k.sched.delay_last_ns = k.now();
    }
    ok(st)
}

/// NtYieldExecution(): to any ready thread, a lower priority one too, or
/// STATUS_NO_YIELD_PERFORMED with none ready (SwitchToThread's FALSE).
/// Nightmare Before Christmas: Oogie's Revenge polls its loader's event
/// while SwitchToThread finds another thread to run.
fn nt_yield_execution(k: &mut Kernel, _p: &mut dyn Platform, _a: &Args) -> KResult<u64> {
    ok(if k.ps_yield() { STATUS_SUCCESS } else { 0x4000_0024 })
}

/// RtlInitializeCriticalSection(CriticalSection)
fn rtl_initialize_critical_section(
    k: &mut Kernel,
    _p: &mut dyn Platform,
    a: &Args,
) -> KResult<u64> {
    k.init_header(a[0], OBJ_SYNCHRONIZATION_EVENT, 4, 0)?;
    k.gm.w32(a[0] + CS_LOCK_COUNT, 0xFFFF_FFFF)?;
    k.gm.w32(a[0] + CS_RECURSION, 0)?;
    k.gm.w32(a[0] + CS_OWNER, 0)?;
    Ok(0)
}

fn current_ethread(k: &Kernel) -> u32 {
    k.current.map_or(0, |t| k.threads[t].ethread)
}

/// RtlEnterCriticalSection(CriticalSection)
fn rtl_enter_critical_section(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let cs = a[0];
    let me = current_ethread(k);
    let lock = (k.gm.r32(cs + CS_LOCK_COUNT)? as i32).wrapping_add(1);
    let owner = k.gm.r32(cs + CS_OWNER)?;
    let rec = k.gm.r32(cs + CS_RECURSION)?;
    k.gm.w32(cs + CS_LOCK_COUNT, lock as u32)?;
    if lock == 0 {
        k.gm.w32(cs + CS_OWNER, me)?;
        k.gm.w32(cs + CS_RECURSION, 1)?;
    } else if owner == me {
        k.gm.w32(cs + CS_RECURSION, rec.wrapping_add(1))?;
    } else {
        k.wait(&[cs], false, 0, cs, false)?; // the event is at the start of the section
    }
    Ok(0)
}

/// RtlLeaveCriticalSection(CriticalSection)
fn rtl_leave_critical_section(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let cs = a[0];
    let rec = k.gm.r32(cs + CS_RECURSION)?.wrapping_sub(1);
    let lock = (k.gm.r32(cs + CS_LOCK_COUNT)? as i32).wrapping_sub(1);
    k.gm.w32(cs + CS_RECURSION, rec)?;
    k.gm.w32(cs + CS_LOCK_COUNT, lock as u32)?;
    if rec != 0 {
        return Ok(0);
    }
    k.gm.w32(cs + CS_OWNER, 0)?;
    if lock >= 0 {
        k.set_event_inc(cs, SIGNAL_INCREMENT)?; // a waiter takes it over
    }
    Ok(0)
}

/// RtlTryEnterCriticalSection(CriticalSection) -> BOOLEAN
fn rtl_try_enter_critical_section(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let cs = a[0];
    let me = current_ethread(k);
    let lock = k.gm.r32(cs + CS_LOCK_COUNT)?;
    if lock as i32 == -1 {
        k.gm.w32(cs + CS_LOCK_COUNT, 0)?;
        k.gm.w32(cs + CS_OWNER, me)?;
        k.gm.w32(cs + CS_RECURSION, 1)?;
        return Ok(1);
    }
    if k.gm.r32(cs + CS_OWNER)? == me {
        let rec = k.gm.r32(cs + CS_RECURSION)?;
        k.gm.w32(cs + CS_LOCK_COUNT, lock.wrapping_add(1))?;
        k.gm.w32(cs + CS_RECURSION, rec.wrapping_add(1))?;
        return Ok(1);
    }
    Ok(0)
}

// ERWLOCK { LockCount (-1 free), WritersWaitingCount, ReadersWaitingCount,
// ReadersEntryCount, KEVENT WriterEvent, KSEMAPHORE ReaderSemaphore }: the
// Xbox kernel's algorithm (the dashboard's); the guest's one CPU runs a call
// whole, as its kernel does with interrupts off.
const RW_LOCK_COUNT: u32 = 0x00;
const RW_WRITERS_WAIT: u32 = 0x04;
const RW_READERS_WAIT: u32 = 0x08;
const RW_READERS_ENTRY: u32 = 0x0C;
const RW_WRITER_EVENT: u32 = 0x10;
const RW_READER_SEM: u32 = 0x20;

fn rw_add(k: &Kernel, va: u32, d: i32) -> KResult<u32> {
    let v = k.gm.r32(va)?.wrapping_add(d as u32);
    k.gm.w32(va, v)?;
    Ok(v)
}

/// ExInitializeReadWriteLock(ReadWriteLock)
fn ex_initialize_read_write_lock(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    k.gm.w32(a[0] + RW_LOCK_COUNT, 0xFFFF_FFFF)?;
    k.gm.w32(a[0] + RW_WRITERS_WAIT, 0)?;
    k.gm.w32(a[0] + RW_READERS_WAIT, 0)?;
    k.gm.w32(a[0] + RW_READERS_ENTRY, 0)?;
    k.init_header(a[0] + RW_WRITER_EVENT, OBJ_SYNCHRONIZATION_EVENT, 4, 0)?;
    k.init_header(a[0] + RW_READER_SEM, OBJ_SEMAPHORE, 5, 0)?;
    k.gm.w32(a[0] + RW_READER_SEM + SEMAPHORE_LIMIT, 0x7FFF_FFFF)?;
    Ok(0)
}

/// ExAcquireReadWriteLockExclusive(ReadWriteLock)
fn ex_acquire_rw_exclusive(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    if rw_add(k, a[0] + RW_LOCK_COUNT, 1)? != 0 {
        rw_add(k, a[0] + RW_WRITERS_WAIT, 1)?;
        k.wait(&[a[0] + RW_WRITER_EVENT], false, 0, 0, false)?;
    }
    Ok(0)
}

/// ExAcquireReadWriteLockShared(ReadWriteLock): readers wait while a writer
/// holds it or one waits for it
fn ex_acquire_rw_shared(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let held = k.gm.r32(a[0] + RW_LOCK_COUNT)? as i32 >= 0;
    let must_wait =
        held && (k.gm.r32(a[0] + RW_READERS_ENTRY)? == 0 || k.gm.r32(a[0] + RW_WRITERS_WAIT)? != 0);
    rw_add(k, a[0] + RW_LOCK_COUNT, 1)?;
    if must_wait {
        rw_add(k, a[0] + RW_READERS_WAIT, 1)?;
        k.wait(&[a[0] + RW_READER_SEM], false, 0, 0, false)?;
    } else {
        rw_add(k, a[0] + RW_READERS_ENTRY, 1)?;
    }
    Ok(0)
}

/// ExReleaseReadWriteLock(ReadWriteLock): a writer leaving lets every waiting
/// reader in, else the next writer; the last reader leaving lets a writer in
fn ex_release_read_write_lock(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let l = a[0];
    if rw_add(k, l + RW_LOCK_COUNT, -1)? as i32 == -1 {
        k.gm.w32(l + RW_READERS_ENTRY, 0)?;
        return Ok(0);
    }
    if k.gm.r32(l + RW_READERS_ENTRY)? == 0 {
        let readers = k.gm.r32(l + RW_READERS_WAIT)?;
        if readers != 0 {
            k.gm.w32(l + RW_READERS_WAIT, 0)?;
            k.gm.w32(l + RW_READERS_ENTRY, readers)?;
            k.release_semaphore(l + RW_READER_SEM, readers as i32, SIGNAL_INCREMENT)?;
            return Ok(0);
        }
    } else if rw_add(k, l + RW_READERS_ENTRY, -1)? != 0 {
        return Ok(0);
    }
    rw_add(k, l + RW_WRITERS_WAIT, -1)?;
    k.set_event_inc(l + RW_WRITER_EVENT, SIGNAL_INCREMENT)?;
    Ok(0)
}

/// KeEnterCriticalRegion() / KeLeaveCriticalRegion(): APCs are delivered only
/// in alertable waits, never into a region.
fn ke_critical_region(_k: &mut Kernel, _p: &mut dyn Platform, _a: &Args) -> KResult<u64> {
    Ok(0)
}

/// KfRaiseIrql(NewIrql) -> old
fn kf_raise_irql(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let old = k.irql();
    k.set_irql(a[0] as u8);
    Ok(u64::from(old))
}

/// KfLowerIrql(NewIrql)
fn kf_lower_irql(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    // A KIRQL in CL: the rest of ECX is whatever the caller left there
    // (Halo's push-buffer wait: its vblank DPC never ran, the flip stalled).
    let now = a[0] as u8;
    k.set_irql(now);
    if now < 2 && k.dpc_pending() {
        k.resched = true; // DPCs queued while the IRQL was raised run now
    }
    Ok(0)
}

/// KeRaiseIrqlToDpcLevel() -> old
fn ke_raise_irql_to_dpc_level(k: &mut Kernel, _p: &mut dyn Platform, _a: &Args) -> KResult<u64> {
    let old = k.irql();
    k.set_irql(2);
    Ok(u64::from(old))
}

/// KeRaiseIrqlToSynchLevel() -> old
fn ke_raise_irql_to_synch_level(k: &mut Kernel, _p: &mut dyn Platform, _a: &Args) -> KResult<u64> {
    let old = k.irql();
    k.set_irql(28);
    Ok(u64::from(old))
}

/// KeGetCurrentIrql()
fn ke_get_current_irql(k: &mut Kernel, _p: &mut dyn Platform, _a: &Args) -> KResult<u64> {
    Ok(u64::from(k.irql()))
}

/// KeGetCurrentThread(): the KPCR's current thread
fn ke_get_current_thread(k: &mut Kernel, _p: &mut dyn Platform, _a: &Args) -> KResult<u64> {
    Ok(u64::from(k.gm.kr32(k.kpcr_va + 0x28)))
}

// The Interlocked family (fastcall: ECX, EDX, then the stack) runs as guest
// code (fast.rs); these are the same in the kernel, for a kernel without
// fast paths. One guest CPU, and the kernel runs it: nothing to lock.
fn interlocked_increment(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let v = k.gm.r32(a[0])?.wrapping_add(1);
    k.gm.w32(a[0], v)?;
    ok(v)
}

fn interlocked_decrement(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let v = k.gm.r32(a[0])?.wrapping_sub(1);
    k.gm.w32(a[0], v)?;
    ok(v)
}

/// InterlockedExchange(Target, Value) -> the old value
fn interlocked_exchange(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let old = k.gm.r32(a[0])?;
    k.gm.w32(a[0], a[1])?;
    ok(old)
}

/// InterlockedExchangeAdd(Addend, Value) -> the old value
fn interlocked_exchange_add(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let old = k.gm.r32(a[0])?;
    k.gm.w32(a[0], old.wrapping_add(a[1]))?;
    ok(old)
}

/// InterlockedCompareExchange(Destination, ExChange, Comparand) -> the old value
fn interlocked_compare_exchange(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let old = k.gm.r32(a[0])?;
    if old == a[2] {
        k.gm.w32(a[0], a[1])?;
    }
    ok(old)
}

/// KeQuerySystemTime(CurrentTime)
fn ke_query_system_time(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    k.gm.w64(a[0], k.system_time())?;
    Ok(0)
}

/// NtSetSystemTime(SystemTime, PreviousTime)
fn nt_set_system_time(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let old = k.system_time();
    if a[1] != 0 {
        k.gm.w64(a[1], old)?;
    }
    if a[0] != 0 {
        let t = k.gm.r64(a[0])?;
        if (t as i64) < 0 {
            return ok(STATUS_INVALID_PARAMETER);
        }
        k.ke.time_offset = k.ke.time_offset.wrapping_add(t.wrapping_sub(old));
        k.update_time();
    }
    ok(STATUS_SUCCESS)
}

/// KeQueryInterruptTime() -> 100 ns since boot
fn ke_query_interrupt_time(k: &mut Kernel, _p: &mut dyn Platform, _a: &Args) -> KResult<u64> {
    Ok(k.interrupt_time())
}

/// KeQueryPerformanceCounter()
fn ke_query_performance_counter(k: &mut Kernel, _p: &mut dyn Platform, _a: &Args) -> KResult<u64> {
    let ns = k.now().saturating_sub(k.ke.boot_ns);
    Ok((u128::from(ns) * u128::from(PERF_FREQUENCY) / 1_000_000_000) as u64)
}

/// KeQueryPerformanceFrequency()
fn ke_query_performance_frequency(
    _k: &mut Kernel,
    _p: &mut dyn Platform,
    _a: &Args,
) -> KResult<u64> {
    Ok(PERF_FREQUENCY)
}

const BASE_PRIORITY: i32 = 8;

/// A thread's base priority as an increment over the process's: +-16 when
/// saturated (KeSetBasePriorityThread).
fn base_increment(t: &crate::ps::Thread) -> i32 {
    if t.saturation != 0 { 16 * i32::from(t.saturation) } else { t.priority - BASE_PRIORITY }
}

/// KeSetBasePriorityThread(Thread, Increment) -> old increment. As on NT, the
/// title's threads stay in the variable range: an increment of +-16
/// saturates to its top or bottom (15, 1), and any other lands in it,
/// clamped to 1..15. Angelic Concert gives its idle counter +14: at 22 it
/// ran above the main thread, which had raised itself to 15 to measure it
/// and never ran again. A thread that has exited takes priority changes to
/// nowhere, as on NT (Panzer Dragoon Orta raises a finished worker to 15).
fn ke_set_base_priority_thread(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let Some(ti) = k.thread_of(a[0]) else { return Ok(0) };
    let t = &mut k.threads[ti];
    let old = base_increment(t);
    let inc = a[1] as i32;
    t.saturation = if inc >= 16 {
        1
    } else if inc <= -16 {
        -1
    } else {
        0
    };
    let p = match t.saturation {
        1 => 15,
        -1 => 1,
        _ => BASE_PRIORITY.saturating_add(inc),
    };
    t.priority = p.clamp(1, 15);
    t.boost = 0;
    k.resched = true;
    ok(old as u32)
}

/// KeQueryBasePriorityThread(Thread) -> increment
fn ke_query_base_priority_thread(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    ok(k.thread_of(a[0]).map_or(0, |t| base_increment(&k.threads[t]) as u32))
}

/// KeSetPriorityThread(Thread, Priority) -> old priority
fn ke_set_priority_thread(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let Some(ti) = k.thread_of(a[0]) else { return Ok(0) };
    let t = &mut k.threads[ti];
    let old = t.priority;
    t.priority = a[1] as i32;
    t.boost = 0;
    k.resched = true;
    ok(old as u32)
}

/// KeSetDisableBoostThread(Thread, Disable) -> old
fn ke_set_disable_boost_thread(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let Some(ti) = k.thread_of(a[0]) else { return Ok(0) };
    let old = k.threads[ti].no_boost;
    k.threads[ti].no_boost = a[1] & 0xFF != 0;
    Ok(u64::from(old))
}

/// KeBoostPriorityThread(Thread, Increment): a boost, as a wait's
fn ke_boost_priority_thread(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    if let Some(ti) = k.thread_of(a[0]) {
        k.boost(ti, a[1] as i32);
    }
    Ok(0)
}

pub(crate) fn register(k: &mut Kernel) {
    k.ke.boot_ns = k.now();
    k.clock_boot();
    k.update_time();

    k.imp("KeInitializeEvent", ke_initialize_event);
    k.imp("KeSetEvent", ke_set_event);
    k.imp("KeResetEvent", ke_reset_event);
    k.imp("KePulseEvent", ke_pulse_event);
    k.imp("NtCreateEvent", nt_create_event);
    k.imp("NtSetEvent", nt_set_event);
    k.imp("NtClearEvent", nt_clear_event);
    k.imp("NtPulseEvent", nt_pulse_event);

    k.imp("KeInitializeSemaphore", ke_initialize_semaphore);
    k.imp("KeReleaseSemaphore", ke_release_semaphore);
    k.imp("NtCreateSemaphore", nt_create_semaphore);
    k.imp("NtReleaseSemaphore", nt_release_semaphore);
    k.imp("KeInitializeMutant", ke_initialize_mutant);
    k.imp("KeReleaseMutant", ke_release_mutant);
    k.imp("NtCreateMutant", nt_create_mutant);
    k.imp("NtReleaseMutant", nt_release_mutant);
    k.imp("NtCreateIoCompletion", nt_create_io_completion);
    k.imp("NtRemoveIoCompletion", nt_remove_io_completion);
    k.imp("NtSetIoCompletion", nt_set_io_completion);
    k.imp("NtQueryIoCompletion", nt_query_io_completion);

    k.imp("KeWaitForSingleObject", ke_wait_for_single_object);
    k.imp("KeWaitForMultipleObjects", ke_wait_for_multiple_objects);
    k.imp("NtWaitForSingleObject", nt_wait_for_single_object);
    k.imp("NtWaitForSingleObjectEx", nt_wait_for_single_object_ex);
    k.imp("NtSignalAndWaitForSingleObjectEx", nt_signal_and_wait);
    k.imp("NtWaitForMultipleObjectsEx", nt_wait_for_multiple_objects_ex);
    k.imp("KeDelayExecutionThread", ke_delay_execution_thread);
    k.imp("NtYieldExecution", nt_yield_execution);

    k.imp("RtlInitializeCriticalSection", rtl_initialize_critical_section);
    k.imp("RtlEnterCriticalSection", rtl_enter_critical_section);
    k.imp("RtlLeaveCriticalSection", rtl_leave_critical_section);
    k.imp("RtlTryEnterCriticalSection", rtl_try_enter_critical_section);
    k.imp("ExInitializeReadWriteLock", ex_initialize_read_write_lock);
    k.imp("ExAcquireReadWriteLockExclusive", ex_acquire_rw_exclusive);
    k.imp("ExAcquireReadWriteLockShared", ex_acquire_rw_shared);
    k.imp("ExReleaseReadWriteLock", ex_release_read_write_lock);
    k.imp("KeEnterCriticalRegion", ke_critical_region);
    k.imp("KeLeaveCriticalRegion", ke_critical_region);

    k.imp("KfRaiseIrql", kf_raise_irql);
    k.imp("KfLowerIrql", kf_lower_irql);
    k.imp("KeRaiseIrqlToDpcLevel", ke_raise_irql_to_dpc_level);
    k.imp("KeRaiseIrqlToSynchLevel", ke_raise_irql_to_synch_level);
    k.imp("KeGetCurrentIrql", ke_get_current_irql);
    k.imp("KeGetCurrentThread", ke_get_current_thread);
    k.imp("InterlockedIncrement", interlocked_increment);
    k.imp("InterlockedDecrement", interlocked_decrement);
    k.imp("InterlockedExchange", interlocked_exchange);
    k.imp("InterlockedExchangeAdd", interlocked_exchange_add);
    k.imp("InterlockedCompareExchange", interlocked_compare_exchange);

    k.imp("KeQuerySystemTime", ke_query_system_time);
    k.imp("NtSetSystemTime", nt_set_system_time);
    k.imp("KeQueryInterruptTime", ke_query_interrupt_time);
    k.imp("KeQueryPerformanceCounter", ke_query_performance_counter);
    k.imp("KeQueryPerformanceFrequency", ke_query_performance_frequency);

    k.imp("KeSetBasePriorityThread", ke_set_base_priority_thread);
    k.imp("KeQueryBasePriorityThread", ke_query_base_priority_thread);
    k.imp("KeSetPriorityThread", ke_set_priority_thread);
    k.imp("KeSetDisableBoostThread", ke_set_disable_boost_thread);
    k.imp("KeBoostPriorityThread", ke_boost_priority_thread);
}

xb_types::state_fields!(PortPacket { key, ctx, status, info });
xb_types::state_fields!(Port { obj, packets });
xb_types::state_fields!(State { boot_ns, time_offset, times_t, wait_due, ports });
