//! Interrupt objects and interrupt delivery.
//!
//! KINTERRUPT (nxdk xboxkrnl.h, CC0): ServiceRoutine 0x00, ServiceContext 0x04,
//! BusInterruptLevel 0x08, Irql 0x0C, Connected 0x10, ShareVector 0x11,
//! Mode 0x12. Vectors are 0x30 + bus level; a level's IRQL is 26 - level.
//!
//! An asserted line is delivered when an interrupt object is connected to
//! it, its IRQL is above the current one and the running code hasn't
//! disabled interrupts: the ISR, ServiceRoutine(Interrupt, ServiceContext),
//! runs as guest code on a kernel thread that outranks the DPC thread, at
//! the interrupt's IRQL, then returns to a sentinel. The interrupted thread
//! is resumed first if it was at DISPATCH_LEVEL or above; otherwise queued
//! DPCs run, then the scheduler picks as usual. Lines are level triggered:
//! one still asserted after its ISR is delivered again.
//!
//! The lines are the machine's `IrqHub`. A line nobody connected is masked
//! here, like the PIC's: its device may assert it, but it is not pending for
//! the CPU. When the guest has interrupts off (`IF` clear) the next run asks
//! the CPU to come back as soon as they are on.

use crate::gm::KResult;
use crate::kernel::{Args, Kernel, ORD_ISR_RETURN};
use crate::platform::Platform;
use crate::ps::ThreadState;

const KI_ROUTINE: u32 = 0x00;
const KI_CONTEXT: u32 = 0x04;
const KI_LEVEL: u32 = 0x08;
const KI_IRQL: u32 = 0x0C;
const KI_CONNECTED: u32 = 0x10;

const ISR_STACK: u32 = 0x4000;
const ISR_PRIORITY: i32 = 2000;
const IRQ_LINES: usize = 16;

/// The interrupts' state.
#[derive(Default)]
pub(crate) struct State {
    /// Connected KINTERRUPT per bus level.
    obj: [u32; IRQ_LINES],
    /// Lines with an interrupt connected.
    pub enabled: u32,
    /// Runs ISRs; created on first use.
    pub isr_thread: Option<usize>,
    pub in_isr: bool,
    isr_saved_irql: u8,
    /// The thread an ISR interrupted.
    isr_interrupted: Option<usize>,
    pub interrupts: u64,
}

impl Kernel {
    /// Whether an asserted line would be delivered now, interrupts on: it is
    /// enabled, connected and above the IRQL.
    pub(crate) fn intr_deliverable(&self) -> bool {
        let lines = self.hub.pending() & self.intr.enabled;
        if lines == 0 || self.intr.in_isr {
            return false;
        }
        let irql = self.irql();
        (0..IRQ_LINES).any(|level| {
            let obj = self.intr.obj[level];
            lines & (1 << level) != 0
                && obj != 0
                && self.gm.r32(obj + KI_IRQL).unwrap_or(0) as u8 > irql
        })
    }

    /// Delivers the highest-priority deliverable asserted line (readies the
    /// ISR thread and asks for a switch). `cpu_enabled`: the running code
    /// has IF set.
    pub(crate) fn intr_dispatch(&mut self, cpu_enabled: bool) -> bool {
        self.irq_window = false;
        let lines = self.hub.pending() & self.intr.enabled;
        if lines == 0 || self.intr.in_isr {
            return false;
        }
        let irql = self.irql();
        // The highest IRQL first: the lowest bus level.
        for level in 0..IRQ_LINES {
            if lines & (1 << level) == 0 {
                continue;
            }
            let obj = self.intr.obj[level];
            if obj == 0 {
                continue; // nothing connected: the line stays asserted, unserved
            }
            let level_irql = self.gm.r32(obj + KI_IRQL).unwrap_or(0) as u8;
            if level_irql <= irql {
                continue; // delivered when the IRQL drops (KfLowerIrql enters the kernel)
            }
            if !cpu_enabled {
                // interrupts off: the CPU comes back when the guest turns them on
                self.irq_window = true;
                return false;
            }
            if self.intr.isr_thread.is_none() {
                self.intr.isr_thread = self.kernel_thread(ISR_STACK, ISR_PRIORITY);
            }
            let Some(ti) = self.intr.isr_thread else { return false };
            let routine = self.gm.r32(obj + KI_ROUTINE).unwrap_or(0);
            let context = self.gm.r32(obj + KI_CONTEXT).unwrap_or(0);
            let esp = self.threads[ti].frame_top() - 16 - 12;
            let _ = self.gm.w32(esp, Self::trap(ORD_ISR_RETURN));
            let _ = self.gm.w32(esp + 4, obj);
            let _ = self.gm.w32(esp + 8, context);
            let ctx = self.context_init(routine, esp);
            let t = &mut self.threads[ti];
            t.ctx = ctx;
            t.fresh = true; // it may still be the current thread, back from its last ISR
            t.state = ThreadState::Ready;

            self.intr.in_isr = true;
            self.intr.isr_saved_irql = irql;
            self.intr.isr_interrupted = self.current.filter(|c| *c != ti);
            self.set_irql(level_irql);
            self.intr.interrupts += 1;
            self.stats.isr[level] += 1;
            self.sched.force_switch = true;
            self.resched = true;
            return true;
        }
        false
    }

    /// An ISR returned to the sentinel.
    pub(crate) fn intr_return(&mut self) {
        self.intr.in_isr = false;
        let saved = self.intr.isr_saved_irql;
        self.set_irql(saved);
        if let Some(ti) = self.intr.isr_thread {
            self.threads[ti].state = ThreadState::Suspended;
        }
        // Code interrupted at DISPATCH_LEVEL or above continues before anything else.
        if saved >= 2
            && let Some(ti) = self.intr.isr_interrupted
            && self.threads[ti].state == ThreadState::Ready
        {
            self.sched.resume_first = Some(ti);
        }
        self.intr.isr_interrupted = None;
        self.resched = true;
    }
}

/// HalGetInterruptVector(BusInterruptLevel, Irql) -> vector
fn hal_get_interrupt_vector(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    if a[1] != 0 {
        k.gm.w8(a[1], 26u32.wrapping_sub(a[0]) as u8)?;
    }
    Ok(u64::from(0x30u32.wrapping_add(a[0])))
}

/// KeInitializeInterrupt(Interrupt, ServiceRoutine, ServiceContext, Vector, Irql, InterruptMode,
/// ShareVector)
fn ke_initialize_interrupt(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    k.gm.w32(a[0] + KI_ROUTINE, a[1])?;
    k.gm.w32(a[0] + KI_CONTEXT, a[2])?;
    k.gm.w32(a[0] + KI_LEVEL, a[3].wrapping_sub(0x30))?;
    k.gm.w32(a[0] + KI_IRQL, a[4] & 0xFF)?;
    k.gm.w8(a[0] + KI_CONNECTED, 0)?;
    k.gm.w8(a[0] + 0x11, a[6] as u8)?;
    k.gm.w8(a[0] + 0x12, a[5] as u8)?;
    Ok(0)
}

/// KeConnectInterrupt(Interrupt) -> BOOLEAN
fn ke_connect_interrupt(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let level = k.gm.r32(a[0] + KI_LEVEL)? as usize;
    if k.gm.r8(a[0] + KI_CONNECTED)? != 0 || level >= IRQ_LINES || k.intr.obj[level] != 0 {
        return Ok(0);
    }
    k.gm.w8(a[0] + KI_CONNECTED, 1)?;
    k.intr.obj[level] = a[0];
    k.intr.enabled |= 1 << level;
    log::info!(target: "kernel", "ke: interrupt connected: level {level}, irql {}, routine {:08x}",
        k.gm.r32(a[0] + KI_IRQL)?, k.gm.r32(a[0] + KI_ROUTINE)?);
    Ok(1)
}

/// KeDisconnectInterrupt(Interrupt) -> BOOLEAN
fn ke_disconnect_interrupt(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let level = k.gm.r32(a[0] + KI_LEVEL)? as usize;
    if k.gm.r8(a[0] + KI_CONNECTED)? == 0 {
        return Ok(0);
    }
    k.gm.w8(a[0] + KI_CONNECTED, 0)?;
    if level < IRQ_LINES && k.intr.obj[level] == a[0] {
        k.intr.obj[level] = 0;
        k.intr.enabled &= !(1 << level);
    }
    Ok(1)
}

/// KeSynchronizeExecution(Interrupt, SynchronizeRoutine, SynchronizeContext) -> the routine's
/// BOOLEAN, run at the interrupt's IRQL so its ISR can't come in between.
fn ke_synchronize_execution(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let level = k.gm.r32(a[0] + KI_IRQL)? as u8;
    let irql = k.irql();
    k.callout_at_irql(a[1], &[a[2]], level.max(irql));
    Ok(0)
}

pub(crate) fn register(k: &mut Kernel) {
    k.imp("HalGetInterruptVector", hal_get_interrupt_vector);
    k.imp("KeInitializeInterrupt", ke_initialize_interrupt);
    k.imp("KeConnectInterrupt", ke_connect_interrupt);
    k.imp("KeDisconnectInterrupt", ke_disconnect_interrupt);
    k.imp("KeSynchronizeExecution", ke_synchronize_execution);
}

xb_types::state_fields!(State {
    obj,
    enabled,
    isr_thread,
    in_isr,
    isr_saved_irql,
    isr_interrupted,
    interrupts
});
