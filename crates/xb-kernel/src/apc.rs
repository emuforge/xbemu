//! Asynchronous procedure calls.
//!
//! KAPC (nxdk xboxkrnl.h, CC0): Type 0x00, ApcMode 0x02, Inserted 0x03,
//! Thread 0x04, ApcListEntry 0x08, KernelRoutine 0x10, RundownRoutine 0x14,
//! NormalRoutine 0x18, NormalContext 0x1C, SystemArgument1 0x20,
//! SystemArgument2 0x24.
//!
//! A thread's APCs wait on the host until it waits alertably. Then each runs
//! as guest code on that thread: KernelRoutine(Apc, &NormalRoutine,
//! &NormalContext, &SystemArgument1, &SystemArgument2) for a KAPC, then
//! NormalRoutine(NormalContext, SystemArgument1, SystemArgument2), each
//! returning to a sentinel that starts the next; after the last, the wait
//! returns STATUS_USER_APC. An APC queued to a thread already in an alertable
//! wait ends the wait the same way. APCs queued by NtQueueApcThread and by
//! I/O completion (NtReadFile's ApcRoutine) have no KAPC: only the normal
//! routine runs.
//!
//! An APC leaves the queue as its delivery begins, as NT takes a user APC off
//! the list before it calls the routine. An APC routine may wait alertably
//! itself: that wait runs the APCs queued since as a chain of its own and
//! returns to the routine, whose chain then goes on (Project Gotham Racing's
//! read completion opens the next file and waits for its buffer, all in the
//! APC).

use xb_cpu::reg;

use crate::gm::KResult;
use crate::kernel::{Args, Kernel, ORD_APC_RETURN, ObType, STATUS_INVALID_HANDLE, STATUS_SUCCESS};
use crate::platform::Platform;
use crate::ps::ThreadState;

const KA_INSERTED: u32 = 0x03;
const KA_THREAD: u32 = 0x04;
const KA_KERNEL: u32 = 0x10;
const KA_RUNDOWN: u32 = 0x14;
const KA_NORMAL: u32 = 0x18;
const KA_CONTEXT: u32 = 0x1C;
const KA_ARG1: u32 = 0x20;
const KA_ARG2: u32 = 0x24;
const KAPC_SIZE: u32 = 0x28;

const STATUS_USER_APC: u32 = 0x0000_00C0;
const MAX_APC_NEST: usize = 4;

/// A queued APC: a KAPC, or a bare normal routine with its arguments.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Apc {
    pub kapc: u32,
    pub normal: u32,
    pub ctx: u32,
    pub a1: u32,
    pub a2: u32,
}

/// A thread's APCs running for an alertable wait: the one being delivered
/// (out of the queue, as NT takes it) and where the wait returns.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ApcChain {
    cur: Option<Apc>,
    /// `cur`'s: 0 none run, 1 kernel routine, 2 normal routine.
    stage: u8,
    pub in_apc: bool,
    /// Where the wait returns once the APCs ran.
    esp: u32,
    eip: u32,
}

impl Kernel {
    pub(crate) fn apc_pending(&self, ti: usize) -> bool {
        let t = &self.threads[ti];
        !t.apcs.is_empty() && (!t.apc.in_apc || t.apc_outer.len() < MAX_APC_NEST)
    }

    /// The next call of the thread's APC chain as (routine, args); `None`
    /// when done.
    fn apc_next_call(&mut self, ti: usize) -> Option<(u32, Vec<u32>)> {
        loop {
            if self.threads[ti].apc.cur.is_none() {
                let a = self.threads[ti].apcs.pop_front()?; // out of the queue: delivery begins
                self.threads[ti].apc.cur = Some(a);
                self.threads[ti].apc.stage = 0;
                if a.kapc != 0 {
                    let _ = self.gm.w8(a.kapc + KA_INSERTED, 0);
                }
            }
            let mut a = self.threads[ti].apc.cur?;
            let stage = self.threads[ti].apc.stage;
            if stage == 0 && a.kapc != 0 {
                let kernel = self.gm.r32(a.kapc + KA_KERNEL).unwrap_or(0);
                if kernel != 0 {
                    self.threads[ti].apc.stage = 1;
                    let k = a.kapc;
                    return Some((
                        kernel,
                        vec![k, k + KA_NORMAL, k + KA_CONTEXT, k + KA_ARG1, k + KA_ARG2],
                    ));
                }
            }
            if stage < 2 {
                if a.kapc != 0 {
                    // the kernel routine may have changed or cleared them
                    a.normal = self.gm.r32(a.kapc + KA_NORMAL).unwrap_or(0);
                    a.ctx = self.gm.r32(a.kapc + KA_CONTEXT).unwrap_or(0);
                    a.a1 = self.gm.r32(a.kapc + KA_ARG1).unwrap_or(0);
                    a.a2 = self.gm.r32(a.kapc + KA_ARG2).unwrap_or(0);
                    self.threads[ti].apc.cur = Some(a);
                }
                self.threads[ti].apc.stage = 2;
                if a.normal != 0 {
                    return Some((a.normal, vec![a.ctx, a.a1, a.a2]));
                }
            }
            self.threads[ti].apc.cur = None; // this APC is done
        }
    }

    /// Builds the call below the final stack pointer: [return sentinel, args...].
    fn apc_call_frame(&self, below: u32, args: &[u32]) -> u32 {
        let sp = below - 16 - 4 * args.len() as u32;
        let _ = self.gm.w32(sp, Self::trap(ORD_APC_RETURN));
        for (i, a) in args.iter().enumerate() {
            let _ = self.gm.w32(sp + 4 + 4 * i as u32, *a);
        }
        sp
    }

    /// A chain starts for a wait returning to esp / eip; the one running (a
    /// wait in an APC routine) is kept, to go on once this one ends. False
    /// when no more can nest.
    fn apc_chain_start(&mut self, ti: usize, esp: u32, eip: u32) -> bool {
        let t = &mut self.threads[ti];
        if t.apc.in_apc {
            if t.apc_outer.len() == MAX_APC_NEST {
                return false;
            }
            t.apc_outer.push(t.apc);
        }
        t.apc = ApcChain { esp, eip, ..ApcChain::default() };
        true
    }

    /// The chain ended: an interrupted one goes on.
    fn apc_chain_end(&mut self, ti: usize) {
        let t = &mut self.threads[ti];
        t.apc = t.apc_outer.pop().unwrap_or_default();
    }

    /// From an alertable wait of the running thread: run its APCs first,
    /// then continue at `final_eip`/`final_esp` with STATUS_USER_APC.
    pub(crate) fn apc_begin_frame(&mut self, ti: usize, final_esp: u32, final_eip: u32) -> bool {
        if !self.apc_chain_start(ti, final_esp, final_eip) {
            return false;
        }
        let Some((routine, args)) = self.apc_next_call(ti) else {
            self.apc_chain_end(ti);
            return false;
        };
        self.threads[ti].apc.in_apc = true;
        let sp = self.apc_call_frame(final_esp, &args);
        self.set_esp(sp);
        self.set_eip(routine);
        true
    }

    /// A thread waiting alertably: its saved context continues in the APCs.
    fn apc_begin_ctx(&mut self, ti: usize) {
        let (esp, eip) = {
            let r = &self.threads[ti].ctx.regs;
            (r.gpr[reg::ESP], r.eip)
        };
        if !self.apc_chain_start(ti, esp, eip) {
            return;
        }
        let Some((routine, args)) = self.apc_next_call(ti) else {
            self.apc_chain_end(ti);
            self.threads[ti].ctx.regs.gpr[reg::EAX] = STATUS_USER_APC;
            return;
        };
        self.threads[ti].apc.in_apc = true;
        let sp = self.apc_call_frame(esp, &args);
        let r = &mut self.threads[ti].ctx.regs;
        r.gpr[reg::ESP] = sp;
        r.eip = routine;
    }

    /// An APC routine returned to the sentinel.
    pub(crate) fn apc_return(&mut self) {
        let Some(ti) = self.current.filter(|t| self.threads[*t].apc.in_apc) else {
            self.request_stop("APC return with no APC running".into());
            return;
        };
        if let Some((routine, args)) = self.apc_next_call(ti) {
            let sp = self.apc_call_frame(self.threads[ti].apc.esp, &args);
            self.set_esp(sp);
            self.set_eip(routine);
            return;
        }
        let (esp, eip) = (self.threads[ti].apc.esp, self.threads[ti].apc.eip);
        self.apc_chain_end(ti);
        self.set_esp(esp);
        self.set_eip(eip);
        self.set_reg(reg::EAX, STATUS_USER_APC);
    }

    /// Queues an APC to the thread; one waiting alertably wakes to run it.
    pub(crate) fn apc_queue(&mut self, ti: usize, a: Apc) {
        self.threads[ti].apcs.push_back(a);
        let t = &self.threads[ti];
        if t.state == ThreadState::Waiting && t.wait.alertable && self.apc_pending(ti) {
            // The wait ends with STATUS_USER_APC once the APCs ran (one in
            // an APC routine too: its chain goes on after).
            self.threads[ti].state = ThreadState::Ready;
            self.apc_begin_ctx(ti);
            self.resched = true;
        }
    }
}

/// KeInitializeApc(Apc, Thread, KernelRoutine, RundownRoutine, NormalRoutine, ProcessorMode, NormalContext)
fn ke_initialize_apc(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    k.gm.fill(a[0], 0, KAPC_SIZE)?;
    k.gm.w16(a[0], 18)?; // ApcObject
    k.gm.w8(a[0] + 2, if a[4] != 0 { a[5] as u8 } else { 0 })?;
    k.gm.w32(a[0] + KA_THREAD, a[1])?;
    k.gm.w32(a[0] + KA_KERNEL, a[2])?;
    k.gm.w32(a[0] + KA_RUNDOWN, a[3])?;
    k.gm.w32(a[0] + KA_NORMAL, a[4])?;
    k.gm.w32(a[0] + KA_CONTEXT, if a[4] != 0 { a[6] } else { 0 })?;
    Ok(0)
}

/// KeInsertQueueApc(Apc, SystemArgument1, SystemArgument2, Increment) -> queued
fn ke_insert_queue_apc(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let t = k.thread_of(k.gm.r32(a[0] + KA_THREAD)?);
    let Some(ti) = t.filter(|_| k.gm.r8(a[0] + KA_INSERTED) == Ok(0)) else {
        return Ok(0);
    };
    k.gm.w32(a[0] + KA_ARG1, a[1])?;
    k.gm.w32(a[0] + KA_ARG2, a[2])?;
    k.gm.w8(a[0] + KA_INSERTED, 1)?;
    k.apc_queue(ti, Apc { kapc: a[0], ..Apc::default() });
    Ok(1) // Inserted until its delivery begins
}

/// NtQueueApcThread(ThreadHandle, ApcRoutine, ApcArgument1, ApcArgument2, ApcArgument3)
fn nt_queue_apc_thread(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let t = if a[0] == 0xFFFF_FFFE {
        k.current
    } else {
        match k.ob_lookup(a[0]) {
            Some((obj, ObType::Thread)) => k.thread_of(obj),
            _ => None,
        }
    };
    let Some(ti) = t else {
        return Ok(u64::from(STATUS_INVALID_HANDLE));
    };
    k.apc_queue(ti, Apc { kapc: 0, normal: a[1], ctx: a[2], a1: a[3], a2: a[4] });
    Ok(u64::from(STATUS_SUCCESS))
}

/// NtUserIoApcDispatcher(ApcContext, IoStatusBlock, Reserved): the APC routine
/// XAPI gives file I/O for a completion routine (ApcContext). It calls
/// CompletionRoutine(dwErrorCode, dwNumberOfBytesTransfered, lpOverlapped),
/// the OVERLAPPED starting with the IO_STATUS_BLOCK.
fn nt_user_io_apc_dispatcher(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let status = k.gm.r32(a[1])?;
    let err = if status & 0x8000_0000 != 0 { crate::rtl::status_to_dos(status) } else { 0 };
    let n = k.gm.r32(a[1] + 4)?;
    k.callout(a[0], &[err, n, a[1]]);
    Ok(0)
}

/// KeTestAlertThread(ProcessorMode) -> alerted: alerts aren't kept.
fn ke_test_alert_thread(_k: &mut Kernel, _p: &mut dyn Platform, _a: &Args) -> KResult<u64> {
    Ok(0)
}

pub(crate) fn register(k: &mut Kernel) {
    k.imp("KeInitializeApc", ke_initialize_apc);
    k.imp("KeInsertQueueApc", ke_insert_queue_apc);
    k.imp("NtQueueApcThread", nt_queue_apc_thread);
    k.imp("NtUserIoApcDispatcher", nt_user_io_apc_dispatcher);
    k.imp("KeTestAlertThread", ke_test_alert_thread);
}

xb_types::state_fields!(Apc { kapc, normal, ctx, a1, a2 });
xb_types::state_fields!(ApcChain { cur, stage, in_apc, esp, eip });
