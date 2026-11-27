//! Tests of the kernel's core over the mock CPU: objects and handles, the
//! dispatcher, the scheduler, timers and DPCs, APCs, interrupts, the pool,
//! exception dispatch and the run loop.

use std::time::Duration;

use xb_cpu::{Exit, eflags, reg};
use xb_types::irq::Irq;

use crate::kernel::{
    Kernel, ORD_APC_RETURN, ORD_CALLOUT_RETURN, ORD_DPC_RETURN, ORD_ISR_RETURN, ORD_SEH_RETURN,
    ORD_THREAD_RETURN, STATUS_ACCESS_VIOLATION, STATUS_TIMEOUT, TRAP_VA,
};
use crate::ps::ThreadState;
use crate::run::RunOutcome;
use crate::testing::{self, CALL_RET, NullBus, TestKernel};

const STATUS_USER_APC: u32 = 0xC0;

impl TestKernel {
    fn r32(&self, va: u32) -> u32 {
        self.k.gm.r32(va).expect("guest memory")
    }

    fn w32(&self, va: u32, v: u32) {
        self.k.gm.w32(va, v).expect("guest memory");
    }

    /// A second guest thread, ready (never run: its code is not mapped).
    /// Returns its thread index and a handle to it.
    fn spawn(&mut self, start: u32) -> (usize, u32) {
        let out = self.alloc(16);
        let st = self
            .call32("PsCreateSystemThreadEx", &[out, 0, 0x4000, 0, out + 4, start, 0x77, 0, 0, 0]);
        assert_eq!(st, 0);
        let h = self.r32(out);
        let (obj, _) = self.k.ob_lookup(h).expect("the thread's handle");
        (self.k.thread_of(obj).expect("the thread"), h)
    }

    fn event(&mut self, ty: u32, signaled: bool) -> u32 {
        let out = self.alloc(4);
        assert_eq!(self.call32("NtCreateEvent", &[out, 0, ty, u32::from(signaled)]), 0);
        self.r32(out)
    }

    /// Lets the scheduler run and returns the thread it switched to.
    fn switch(&mut self) -> usize {
        self.k.resched = true;
        self.tail();
        self.k.current.expect("a running thread")
    }

    fn timeout(&mut self, ms: i64) -> u32 {
        let va = self.alloc(8);
        self.k.gm.w64(va, (-ms * 10_000) as u64).unwrap();
        va
    }

    fn start_clock(&mut self) {
        self.k.clock.pause_end();
    }
}

// ------------------------------------------------------------- the area

#[test]
fn the_kernel_area_has_an_export_directory_and_a_kpcr() {
    let t = testing::kernel();
    let k = &t.k;
    assert_eq!(k.gm.kr32(crate::KERNEL_VA) & 0xFFFF, 0x5A4D);
    // AddressOfFunctions: ordinal 255's entry resolves to its trap address
    let rva = k.gm.kr32(crate::KERNEL_VA + 0x300 + 4 * 254);
    assert_eq!(crate::KERNEL_VA.wrapping_add(rva), TRAP_VA + 255);
    assert_eq!(k.resolve(255), TRAP_VA + 255);
    // a data export resolves to kernel memory, a fast-path export to code there
    assert_eq!(k.resolve(156), k.data_addr("KeTickCount"));
    let enter =
        k.resolve(u32::from(crate::exports::by_name("RtlEnterCriticalSection").unwrap().ordinal));
    assert!((crate::KERNEL_VA..crate::KERNEL_VA + 0x20000).contains(&enter));
    assert_eq!(k.gm.kr32(k.kpcr_va + 0x1C), k.kpcr_va, "SelfPcr");
    assert_eq!(k.gm.kr32(k.kpcr_va + 0x28), k.threads[k.current.unwrap()].ethread, "CurrentThread");
    assert_eq!(k.gm.kr32(k.data_addr("KeTimeIncrement")), 10000);
    assert_eq!(k.gm.kr8(k.data_addr("KdDebuggerNotPresent")), 1);
    assert_eq!(t.cpu.trap, (TRAP_VA, crate::TRAP_LEN));
}

#[test]
fn every_fast_path_ends_in_a_jump_to_its_export_s_trap_address() {
    let t = testing::kernel();
    for (name, code) in crate::fast::FAST {
        let e = crate::exports::by_name(name).unwrap();
        let va = t.k.resolve(u32::from(e.ordinal));
        assert_eq!(t.k.gm.read_vec(va, code.len() as u32).unwrap(), *code, "{name}");
        let jmp = va + code.len() as u32;
        assert_eq!(t.k.gm.kr8(jmp), 0xE9);
        let target = (jmp + 5).wrapping_add(t.k.gm.kr32(jmp + 1));
        assert_eq!(target, TRAP_VA + u32::from(e.ordinal), "{name}");
    }
}

#[test]
fn a_kernel_without_fast_paths_enters_the_kernel_for_every_export() {
    let t = testing::kernel_with(|c| c.fast_paths = false);
    for e in crate::exports::all().iter().filter(|e| e.abi != crate::exports::Abi::Data) {
        assert_eq!(t.k.resolve(u32::from(e.ordinal)), TRAP_VA + u32::from(e.ordinal));
    }
}

#[test]
fn a_call_to_an_unknown_or_data_ordinal_stops_the_guest_naming_it() {
    let mut t = testing::kernel();
    t.k.kcall(&mut t.p, 156);
    assert!(t.stop_reason().unwrap().contains("ordinal 156 (not a function)"));
}

#[test]
fn stdcall_and_fastcall_calls_pop_their_own_arguments() {
    let mut t = testing::kernel_with(|c| c.fast_paths = false);
    let esp = t.k.esp();
    let ev = t.alloc(0x20);
    t.call("KeInitializeEvent", &[ev, 1, 1]);
    assert!(t.returned());
    assert_eq!(t.k.esp(), esp);
    assert_eq!(t.call32("KfRaiseIrql", &[7]), 0);
    assert_eq!(t.k.irql(), 7);
    assert_eq!(t.k.esp(), esp);
    let v = t.alloc(4);
    assert_eq!(t.call32("InterlockedCompareExchange", &[v, 9, 0]), 0);
    assert_eq!(t.r32(v), 9);
    assert_eq!(t.k.esp(), esp, "the one stack argument of a fastcall is popped");
}

// ------------------------------------------------------ objects, handles

#[test]
fn handles_are_multiples_of_four_and_a_closed_one_is_not_the_next_given() {
    let mut t = testing::kernel();
    let a = t.event(0, false);
    let b = t.event(0, false);
    assert!(a.is_multiple_of(4) && b == a + 4);
    assert_eq!(t.call32("NtClose", &[a]), 0);
    assert_eq!(t.call32("NtClose", &[a]), 0xC000_0008, "closed twice");
    let c = t.event(0, false);
    assert_eq!(c, b + 4, "taken round the table");
    assert_eq!(t.call32("NtClose", &[2]), 0xC000_0008);
}

#[test]
fn a_duplicate_keeps_the_object_until_the_last_handle_closes() {
    let mut t = testing::kernel();
    let h = t.event(0, false);
    let out = t.alloc(4);
    assert_eq!(t.call32("NtDuplicateObject", &[h, out, 0]), 0);
    let h2 = t.r32(out);
    let (obj, _) = t.k.ob_lookup(h).unwrap();
    assert_eq!(t.k.ob_lookup(h2).unwrap().0, obj);
    assert_eq!(t.r32(obj - 0x10 + 4), 2, "HandleCount");
    t.call("NtClose", &[h]);
    assert_eq!(t.call32("NtSetEvent", &[h2, 0]), 0);
    // the pseudo handle of the current thread becomes a real one
    assert_eq!(t.call32("NtDuplicateObject", &[0xFFFF_FFFE, out, 0]), 0);
    let th = t.r32(out);
    assert_eq!(t.k.thread_of_handle(th), t.k.current);
}

#[test]
fn a_named_object_is_opened_by_name_until_its_last_handle_closes() {
    let mut t = testing::kernel();
    let m = t.alloc(0x100);
    // OBJECT_ATTRIBUTES { RootDirectory, ObjectName, Attributes }, STRING, name
    t.k.gm.write(m + 0x40, b"MyEvent").unwrap();
    t.k.gm.w16(m + 0x20, 7).unwrap();
    t.k.gm.w16(m + 0x22, 8).unwrap();
    t.w32(m + 0x24, m + 0x40);
    t.w32(m + 4, m + 0x20);
    assert_eq!(t.call32("NtCreateEvent", &[m + 0x10, m, 0, 0]), 0);
    let h = t.r32(m + 0x10);
    // a second create collides; with OBJ_OPENIF it opens the same object
    assert_eq!(t.call32("NtCreateEvent", &[m + 0x14, m, 0, 0]), 0xC000_0035);
    t.w32(m + 8, 0x80);
    assert_eq!(t.call32("NtCreateEvent", &[m + 0x14, m, 0, 0]), 0x4000_0000);
    let h2 = t.r32(m + 0x14);
    assert_eq!(t.k.ob_lookup(h2).unwrap().0, t.k.ob_lookup(h).unwrap().0);
    // a mutant of that name is a type mismatch
    assert_eq!(t.call32("NtCreateMutant", &[m + 0x18, m, 0]), 0xC000_0024);
    // opened by name, case-insensitively
    t.k.gm.write(m + 0x40, b"MYEVENT").unwrap();
    let ty = t.k.data_addr("ExEventObjectType");
    assert_eq!(t.call32("ObOpenObjectByName", &[m, ty, 0, m + 0x18]), 0);
    let h3 = t.r32(m + 0x18);
    for h in [h, h2, h3] {
        t.call("NtClose", &[h]);
    }
    assert_eq!(
        t.call32("ObOpenObjectByName", &[m, ty, 0, m + 0x18]),
        0xC000_0034,
        "gone with its last handle"
    );
}

// ------------------------------------------------------------------ pool

#[test]
fn the_pool_reuses_freed_blocks_and_joins_neighbours() {
    let mut t = testing::kernel();
    let free0 = t.k.pool.free_bytes();
    let a = t.k.pool_alloc(24);
    let b = t.k.pool_alloc(100);
    let c = t.k.pool_alloc(8);
    assert!(a.is_multiple_of(16) && b == a + 32 && c == b + 112);
    t.k.pool_free(b, 100);
    assert_eq!(t.k.pool_alloc(100), b, "first fit takes the hole");
    t.k.pool_free(a, 24);
    t.k.pool_free(c, 8);
    t.k.pool_free(b, 100);
    assert_eq!(t.k.pool.free_bytes(), free0);
    assert_eq!(t.k.pool.free.len(), 1, "the ranges joined");
}

#[test]
fn the_pool_grows_by_chunks_and_gives_whole_free_pages_back() {
    let mut t = testing::kernel();
    let (len0, ram0) = (t.k.pool.len, t.k.gm.phys.free_pages());
    let big = t.k.pool_alloc(0x30000);
    assert_ne!(big, 0);
    assert!(t.k.pool.len >= len0 + 0x30000);
    assert!(t.k.gm.phys.free_pages() <= ram0 - 0x30);
    assert_eq!(t.k.gm.kr32(big + 0x2FFFC), 0, "zeroed");
    t.k.pool_free(big, 0x30000);
    let given = t.k.pool_give_back();
    assert!(given >= 0x30000);
    assert_eq!(t.k.gm.phys.free_pages(), ram0, "the chunks' pages are RAM again");
    assert_eq!(t.k.pool.len, len0);
    assert!(!t.k.gm.take_freed().is_empty(), "the machine is told");
}

// ------------------------------------------------------------ dispatcher

#[test]
fn a_synchronization_event_wakes_one_waiter_and_a_notification_event_all() {
    let mut t = testing::kernel();
    let sync = t.event(1, false);
    let note = t.event(0, false);
    let (a, _) = t.spawn(0x2000);
    let (b, _) = t.spawn(0x2000);
    let main = t.k.current.unwrap();
    // main and a wait on the synchronization event, b sets it
    t.call("NtWaitForSingleObject", &[sync, 0, 0]);
    assert_eq!(t.k.threads[main].state, ThreadState::Waiting);
    assert_eq!(t.switch(), a);
    t.call("NtWaitForSingleObject", &[sync, 0, 0]);
    assert_eq!(t.switch(), b);
    assert_eq!(t.call32("NtSetEvent", &[sync, 0]), 0);
    let ready = |t: &TestKernel, i: usize| t.k.threads[i].state == ThreadState::Ready;
    assert!(ready(&t, main) && !ready(&t, a), "one waiter took the signal");
    let (obj, _) = t.k.ob_lookup(sync).unwrap();
    assert_eq!(t.r32(obj + 4), 0, "and it is unsignaled again");
    assert_eq!(t.k.threads[main].ctx.regs.gpr[reg::EAX], 0, "STATUS_WAIT_0");
    assert_eq!(t.k.threads[main].ctx.regs.eip, CALL_RET);
    // the woken thread is boosted above the setter and runs first
    assert_eq!(t.switch(), main);
    t.call("NtWaitForSingleObject", &[note, 0, 0]);
    assert_eq!(t.switch(), b);
    t.call("NtSetEvent", &[sync, 0]);
    assert_eq!(t.switch(), a);
    t.call("NtWaitForSingleObject", &[note, 0, 0]);
    assert_eq!(t.switch(), b);
    t.call("NtSetEvent", &[note, 0]);
    assert!(ready(&t, main) && ready(&t, a), "a notification event wakes every waiter");
    let (obj, _) = t.k.ob_lookup(note).unwrap();
    assert_eq!(t.r32(obj + 4), 1, "and stays signaled");
}

#[test]
fn wait_any_returns_the_index_and_wait_all_needs_every_object() {
    let mut t = testing::kernel();
    let e0 = t.event(1, false);
    let e1 = t.event(1, true);
    let hs = t.alloc(8);
    t.w32(hs, e0);
    t.w32(hs + 4, e1);
    // wait-any (WaitType 1): the second is signaled
    assert_eq!(t.call32("NtWaitForMultipleObjectsEx", &[2, hs, 1, 0, 0, 0]), 1);
    assert!(t.returned());
    // wait-all (WaitType 0) with a zero timeout: e1 was consumed, so it times out
    let zero = t.alloc(8);
    assert_eq!(t.call32("NtWaitForMultipleObjectsEx", &[2, hs, 0, 0, 0, zero]), STATUS_TIMEOUT);
    t.call("NtSetEvent", &[e0, 0]);
    assert_eq!(
        t.call32("NtWaitForMultipleObjectsEx", &[2, hs, 0, 0, 0, zero]),
        STATUS_TIMEOUT,
        "one of two"
    );
    let (o0, _) = t.k.ob_lookup(e0).unwrap();
    assert_eq!(t.r32(o0 + 4), 1, "a failed wait-all takes nothing");
    t.call("NtSetEvent", &[e1, 0]);
    assert_eq!(t.call32("NtWaitForMultipleObjectsEx", &[2, hs, 0, 0, 0, zero]), 0);
    assert_eq!(t.r32(o0 + 4), 0, "both taken");
    t.w32(hs + 4, 0x1234_0000);
    assert_eq!(t.call32("NtWaitForMultipleObjectsEx", &[2, hs, 1, 0, 0, zero]), 0xC000_0008);
}

#[test]
fn a_timed_wait_times_out_and_a_delay_ends_with_success() {
    let mut t = testing::kernel();
    t.start_clock();
    let ev = t.event(1, false);
    let (w, _) = t.spawn(0x2000);
    let main = t.k.current.unwrap();
    let to = t.timeout(3);
    t.call("NtWaitForSingleObject", &[ev, 0, to]);
    assert_eq!(t.k.threads[main].state, ThreadState::Waiting);
    assert!(t.k.ke.wait_due.is_some());
    assert_eq!(t.switch(), w);
    // the worker sleeps: both wait, the scheduler idles on the host until
    // the earlier timeout, and the wait reports STATUS_TIMEOUT
    let to2 = t.timeout(20);
    t.call("KeDelayExecutionThread", &[0, 0, to2]);
    let t0 = std::time::Instant::now();
    t.tail();
    assert_eq!(t.k.current, Some(main));
    assert_eq!(t.k.reg(reg::EAX), STATUS_TIMEOUT);
    let waited = t0.elapsed();
    assert!(waited >= Duration::from_millis(2) && waited < Duration::from_millis(15), "{waited:?}");
    // main waits for good; the worker's delay ends with STATUS_SUCCESS
    t.call("NtWaitForSingleObject", &[ev, 0, 0]);
    t.tail();
    assert_eq!(t.k.current, Some(w));
    assert_eq!(t.k.reg(reg::EAX), 0);
    assert!(t0.elapsed() >= Duration::from_millis(19));
}

#[test]
fn a_mutant_is_owned_recursively_and_handed_to_a_waiter_on_release() {
    let mut t = testing::kernel();
    let out = t.alloc(8);
    assert_eq!(t.call32("NtCreateMutant", &[out, 0, 1]), 0);
    let m = t.r32(out);
    let (obj, _) = t.k.ob_lookup(m).unwrap();
    let main = t.k.current.unwrap();
    assert_eq!(t.r32(obj + 0x18), t.k.threads[main].ethread, "owned at creation");
    assert_eq!(t.call32("NtWaitForSingleObject", &[m, 0, 0]), 0, "the owner enters again");
    let (w, _) = t.spawn(0x2000);
    t.k.threads[main].priority = 7; // let the worker run
    assert_eq!(t.switch(), w);
    t.call("NtWaitForSingleObject", &[m, 0, 0]);
    assert_eq!(t.k.threads[w].state, ThreadState::Waiting);
    assert_eq!(t.switch(), main);
    t.call("NtReleaseMutant", &[m, out]);
    assert_eq!(t.k.threads[w].state, ThreadState::Waiting, "still held once");
    t.call("NtReleaseMutant", &[m, 0]);
    assert_eq!(t.k.threads[w].state, ThreadState::Ready);
    assert_eq!(t.r32(obj + 0x18), t.k.threads[w].ethread, "the waiter owns it now");
}

#[test]
fn a_semaphore_counts_and_refuses_to_pass_its_limit() {
    let mut t = testing::kernel();
    let out = t.alloc(8);
    assert_eq!(t.call32("NtCreateSemaphore", &[out, 0, 1, 2]), 0);
    let s = t.r32(out);
    assert_eq!(t.call32("NtReleaseSemaphore", &[s, 1, out + 4]), 0);
    assert_eq!(t.r32(out + 4), 1, "the previous count");
    assert_eq!(t.call32("NtReleaseSemaphore", &[s, 1, 0]), 0xC000_0047, "past the limit");
    let zero = t.alloc(8);
    assert_eq!(t.call32("NtWaitForSingleObject", &[s, 0, zero]), 0);
    assert_eq!(t.call32("NtWaitForSingleObject", &[s, 0, zero]), 0);
    assert_eq!(t.call32("NtWaitForSingleObject", &[s, 0, zero]), STATUS_TIMEOUT);
}

#[test]
fn a_contended_critical_section_passes_to_the_waiter() {
    let mut t = testing::kernel_with(|c| c.fast_paths = false);
    let cs = t.alloc(0x20);
    t.call("RtlInitializeCriticalSection", &[cs]);
    t.call("RtlEnterCriticalSection", &[cs]);
    t.call("RtlEnterCriticalSection", &[cs]);
    let main = t.k.current.unwrap();
    assert_eq!((t.r32(cs + 0x10), t.r32(cs + 0x14)), (1, 2), "LockCount, RecursionCount");
    let (w, _) = t.spawn(0x2000);
    t.k.threads[main].priority = 7;
    assert_eq!(t.switch(), w);
    assert_eq!(t.call32("RtlTryEnterCriticalSection", &[cs]), 0);
    t.call("RtlEnterCriticalSection", &[cs]);
    assert_eq!(t.k.threads[w].state, ThreadState::Waiting);
    assert_eq!(t.switch(), main);
    t.call("RtlLeaveCriticalSection", &[cs]);
    assert_eq!(t.k.threads[w].state, ThreadState::Waiting);
    t.call("RtlLeaveCriticalSection", &[cs]);
    assert_eq!(t.k.threads[w].state, ThreadState::Ready);
    assert_eq!(t.r32(cs + 0x18), t.k.threads[w].ethread, "OwningThread");
    assert_eq!(t.r32(cs + 0x14), 1);
}

#[test]
fn a_suspended_waiter_does_not_take_the_signal() {
    let mut t = testing::kernel();
    let ev = t.event(1, false);
    let (w, wh) = t.spawn(0x2000);
    let main = t.k.current.unwrap();
    t.k.threads[main].priority = 7;
    assert_eq!(t.switch(), w);
    t.call("NtWaitForSingleObject", &[ev, 0, 0]);
    assert_eq!(t.switch(), main);
    let out = t.alloc(4);
    assert_eq!(t.call32("NtSuspendThread", &[wh, out]), 0);
    t.call("NtSetEvent", &[ev, 0]);
    assert_eq!(t.k.threads[w].state, ThreadState::Waiting);
    let (obj, _) = t.k.ob_lookup(ev).unwrap();
    assert_eq!(t.r32(obj + 4), 1, "the signal stays");
    assert_eq!(t.call32("NtResumeThread", &[wh, out]), 0);
    assert_eq!(t.r32(out), 1, "the previous suspend count");
    assert_eq!(t.k.threads[w].state, ThreadState::Ready);
}

// ------------------------------------------------------------------ APCs

#[test]
fn an_apc_ends_an_alertable_wait_and_runs_before_it_returns() {
    let mut t = testing::kernel();
    let ev = t.event(1, false);
    let out = t.alloc(4);
    t.call("NtDuplicateObject", &[0xFFFF_FFFE, out, 0]);
    let main_h = t.r32(out);
    let (w, _) = t.spawn(0x2000);
    let main = t.k.current.unwrap();
    let esp = t.k.esp();
    // NtWaitForSingleObjectEx(handle, mode, alertable, timeout)
    t.call("NtWaitForSingleObjectEx", &[ev, 0, 1, 0]);
    assert_eq!(t.switch(), w);
    assert_eq!(t.call32("NtQueueApcThread", &[main_h, 0x5000, 1, 2, 3]), 0);
    assert_eq!(t.k.threads[main].state, ThreadState::Ready);
    // the queueing thread sleeps: the APC's thread runs
    let to = t.timeout(10_000);
    t.call("KeDelayExecutionThread", &[0, 0, to]);
    t.tail();
    assert_eq!(t.k.current, Some(main));
    // the thread continues in the routine: [return sentinel, context, arg1, arg2]
    assert_eq!(t.k.eip(), 0x5000);
    let sp = t.k.esp();
    assert!(sp < esp);
    assert_eq!(t.r32(sp), Kernel::trap(ORD_APC_RETURN));
    assert_eq!([t.r32(sp + 4), t.r32(sp + 8), t.r32(sp + 12)], [1, 2, 3]);
    // the routine returns: the wait returns STATUS_USER_APC to its caller
    t.k.set_esp(sp + 16);
    t.k.set_eip(Kernel::trap(ORD_APC_RETURN));
    t.k.kcall(&mut t.p, ORD_APC_RETURN);
    assert_eq!(t.k.eip(), CALL_RET);
    assert_eq!(t.k.esp(), esp);
    assert_eq!(t.k.reg(reg::EAX), STATUS_USER_APC);
}

#[test]
fn queued_apcs_run_in_order_when_the_thread_next_waits_alertably() {
    let mut t = testing::kernel();
    let esp = t.k.esp();
    t.call("NtQueueApcThread", &[0xFFFF_FFFE, 0x5000, 1, 0, 0]);
    t.call("NtQueueApcThread", &[0xFFFF_FFFE, 0x6000, 2, 0, 0]);
    assert!(t.returned(), "not delivered outside an alertable wait");
    let to = t.timeout(1000);
    t.call("KeDelayExecutionThread", &[0, 1, to]);
    assert_eq!(t.k.eip(), 0x5000);
    t.k.kcall(&mut t.p, ORD_APC_RETURN);
    assert_eq!(t.k.eip(), 0x6000);
    assert_eq!(t.r32(t.k.esp() + 4), 2);
    t.k.kcall(&mut t.p, ORD_APC_RETURN);
    assert_eq!((t.k.eip(), t.k.esp(), t.k.reg(reg::EAX)), (CALL_RET, esp, STATUS_USER_APC));
    assert_eq!(
        t.k.threads[t.k.current.unwrap()].state,
        ThreadState::Running,
        "the delay did not begin"
    );
}

// ------------------------------------------------------------- scheduler

#[test]
fn the_highest_priority_ready_thread_runs_and_equals_take_turns_in_ready_order() {
    let mut t = testing::kernel();
    let main = t.k.current.unwrap();
    let (a, _) = t.spawn(0x2000);
    let (b, _) = t.spawn(0x2000);
    let (c, _) = t.spawn(0x2000);
    let et = |t: &TestKernel, i: usize| t.k.threads[i].ethread;
    let (ea, ec) = (et(&t, a), et(&t, c));
    assert_eq!(t.call32("KeSetPriorityThread", &[ec, 12]), 8, "the old priority");
    assert_eq!(t.switch(), c, "the highest priority first");
    // c waits: the others at priority 8 run in the order they became ready;
    // main was switched out while running and keeps its place at the front
    let to = t.timeout(10_000);
    t.call("KeDelayExecutionThread", &[0, 0, to]);
    assert_eq!(t.switch(), main);
    // main's quantum ends with peers ready: it goes to the back
    let now = t.k.now();
    t.k.sched.slice_due = Some(now);
    t.k.slice_end(now);
    assert!(t.k.resched && t.k.threads[main].slice_expired);
    t.tail();
    assert_eq!(t.k.current, Some(a));
    t.call("NtYieldExecution", &[]);
    t.tail();
    assert_eq!(t.k.current, Some(b));
    t.call("NtYieldExecution", &[]);
    t.tail();
    assert_eq!(t.k.current, Some(main));
    // lowering a's priority takes it out of the rotation while others are ready
    t.call("KeSetPriorityThread", &[ea, 5]);
    t.call("NtYieldExecution", &[]);
    t.tail();
    assert_eq!(t.k.current, Some(b));
    assert_eq!(t.k.stats.switches, 6);
}

#[test]
fn a_yield_with_no_other_thread_ready_reports_that_none_was_performed() {
    let mut t = testing::kernel();
    assert_eq!(t.call32("NtYieldExecution", &[]), 0x4000_0024);
    let (w, _) = t.spawn(0x2000);
    let main = t.k.current.unwrap();
    // a scheduling pass puts the new thread in the ready order, behind the
    // running one
    assert_eq!(t.switch(), main);
    assert_eq!(t.call32("NtYieldExecution", &[]), 0);
    t.tail();
    assert_eq!(t.k.current, Some(w));
}

#[test]
fn a_yield_gives_a_lower_priority_thread_a_turn_when_no_peer_is_ready() {
    let mut t = testing::kernel();
    let main = t.k.current.unwrap();
    let (low, _) = t.spawn(0x2000);
    let e = t.k.threads[low].ethread;
    t.call("KeSetPriorityThread", &[e, 7]);
    assert_eq!(t.switch(), main, "a lower priority thread does not preempt");
    assert_eq!(t.call32("NtYieldExecution", &[]), 0);
    t.tail();
    assert_eq!(t.k.current, Some(low), "FIFA World Cup 2006's cache worker gets the CPU");
}

#[test]
fn a_thread_whose_start_routine_returns_is_signaled_and_its_stack_freed() {
    let mut t = testing::kernel();
    let main = t.k.current.unwrap();
    let (w, wh) = t.spawn(0x2000);
    let (limit, obj) = (t.k.threads[w].stack_limit, t.k.threads[w].ethread);
    // the new thread's first frame: the return sentinel and its context
    let sp = t.k.threads[w].ctx.regs.gpr[reg::ESP];
    assert_eq!(t.r32(sp), Kernel::trap(ORD_THREAD_RETURN));
    assert_eq!(t.r32(sp + 4), 0x77);
    assert_eq!(t.k.threads[w].ctx.regs.eflags & eflags::IF, eflags::IF);
    t.k.threads[main].priority = 7;
    assert_eq!(t.switch(), w);
    assert!(t.k.gm.mapped(limit));
    let pages = t.k.gm.phys.free_pages();
    t.k.set_eip(Kernel::trap(ORD_THREAD_RETURN));
    t.entry(Exit::Trap);
    assert_eq!(t.k.current, Some(main));
    assert_eq!(t.k.threads[w].state, ThreadState::Free);
    assert_eq!(t.r32(obj + 4), 1, "the thread object is signaled");
    assert!(!t.k.gm.mapped(limit) && t.k.gm.phys.free_pages() > pages, "its stack is gone");
    let zero = t.alloc(8);
    assert_eq!(t.call32("NtWaitForSingleObject", &[wh, 0, zero]), 0);
    assert_eq!(t.call32("NtResumeThread", &[wh, 0]), 0, "a valid handle still");
}

#[test]
fn a_new_thread_starts_under_its_fx_save_area_and_tls() {
    let mut t = testing::kernel();
    let out = t.alloc(16);
    // TlsDataSize 0x24, a system routine: it gets (StartRoutine, StartContext)
    t.call("PsCreateSystemThreadEx", &[out, 0, 0x8000, 0x24, out + 4, 0x2000, 0x99, 1, 0, 0x3000]);
    let (obj, _) = t.k.ob_lookup(t.r32(out)).unwrap();
    let th = &t.k.threads[t.k.thread_of(obj).unwrap()];
    assert_eq!(th.suspend, 1, "created suspended");
    assert_eq!(th.tib_base, th.stack_base - 0x210, "the TIB's StackBase is the save area's bottom");
    assert_eq!(t.r32(obj + 0x28), th.tib_base - 0x24, "TlsData");
    assert_eq!(t.r32(th.tib_base - 0x24), th.tib_base - 0x20, "its first dword points past itself");
    assert_eq!(t.k.gm.r16(th.tib_base).unwrap(), 0x27F, "the saved x87 control word");
    let sp = th.ctx.regs.gpr[reg::ESP];
    assert!(sp < th.tib_base - 0x24);
    assert_eq!(th.ctx.regs.eip, 0x3000);
    assert_eq!([t.r32(sp + 4), t.r32(sp + 8)], [0x2000, 0x99]);
    assert!(t.k.gm.mapped(th.stack_base + 0x3000), "zeroed pages over the stack's top");
    assert_eq!(t.r32(obj + 0x12C), th.id, "UniqueThread");
}

// ------------------------------------------------------ frame-latency hold

#[test]
fn a_thread_asked_to_hold_waits_for_the_next_flip_with_its_registers_as_they_were() {
    let mut t = testing::kernel();
    t.start_clock();
    let main = t.k.current.unwrap();
    let hold = t.k.frame_hold();
    let wake = t.k.wake_handle();
    let before = (t.k.eip(), t.k.esp(), t.k.frame.regs.gpr[reg::EAX]);
    hold.request();
    // The flip comes 15 ms later from another thread, as the renderer's does.
    let flipper = std::thread::spawn({
        let hold = hold.clone();
        move || {
            std::thread::sleep(Duration::from_millis(15));
            let held = hold.flipped();
            wake.wake();
            held
        }
    });
    let t0 = std::time::Instant::now();
    t.entry(Exit::Kicked); // every thread waits: the kernel idles in here
    let waited = t0.elapsed();
    assert!(flipper.join().unwrap(), "a thread was held when the flip came");
    assert_eq!(t.k.stats.frame_holds, 1);
    assert!(waited >= Duration::from_millis(14), "it went on after {waited:?}, before the flip");
    assert!(waited < Duration::from_millis(45), "the flip ended it, not the deadline ({waited:?})");
    assert_eq!(t.k.current, Some(main));
    assert_eq!((t.k.eip(), t.k.esp(), t.k.frame.regs.gpr[reg::EAX]), before);
    assert!(!hold.flipped(), "nothing is held now");
}

#[test]
fn a_hold_is_not_taken_when_the_flip_came_first_or_above_passive_level() {
    let mut t = testing::kernel();
    let main = t.k.current.unwrap();
    let hold = t.k.frame_hold();
    hold.request();
    hold.flipped();
    t.entry(Exit::Kicked);
    assert_eq!(t.k.current, Some(main), "the flip it would wait for has come");
    t.call("KeRaiseIrqlToDpcLevel", &[]);
    hold.request();
    t.entry(Exit::Kicked);
    assert_eq!(t.k.threads[main].state, ThreadState::Running, "not at DISPATCH_LEVEL");
    // The request was dropped, not kept for later.
    t.call("KfLowerIrql", &[0]);
    t.entry(Exit::Kicked);
    assert_eq!(t.k.current, Some(main));
}

#[test]
fn a_hold_ends_at_its_deadline_when_no_flip_comes() {
    let mut t = testing::kernel();
    t.start_clock();
    let main = t.k.current.unwrap();
    t.k.frame_hold().request();
    let t0 = std::time::Instant::now();
    t.entry(Exit::Kicked);
    let waited = t0.elapsed();
    assert_eq!(t.k.stats.frame_holds, 1);
    assert_eq!(t.k.current, Some(main));
    assert!(
        waited >= Duration::from_millis(45) && waited < Duration::from_millis(200),
        "50 ms, not {waited:?}"
    );
}

// ---------------------------------------------------------- timers, DPCs

#[test]
fn timer_dpcs_run_at_dispatch_level_on_the_dpc_thread_one_after_the_other() {
    let mut t = testing::kernel();
    t.start_clock();
    let main = t.k.current.unwrap();
    let m = t.alloc(0x200);
    let (t1, t2, d1, d2) = (m, m + 0x40, m + 0x80, m + 0xC0);
    for (d, routine, ctx) in [(d1, 0x5000, 0x11), (d2, 0x6000, 0x22)] {
        t.w32(d + 0x0C, routine);
        t.w32(d + 0x10, ctx);
    }
    t.call("KeInitializeTimerEx", &[t1, 0]);
    t.call("KeInitializeTimerEx", &[t2, 1]);
    let due = |ms: i64| ((-ms * 10_000) as u64 as u32, ((-ms * 10_000) as u64 >> 32) as u32);
    let (lo, hi) = due(2);
    assert_eq!(t.call32("KeSetTimer", &[t1, lo, hi, d1]), 0);
    let (lo, hi) = due(3);
    assert_eq!(t.call32("KeSetTimer", &[t2, lo, hi, d2]), 0);
    assert_eq!(t.k.gm.r8(t1 + 3).unwrap(), 1, "Inserted");
    std::thread::sleep(Duration::from_millis(6));
    let esp_main = t.k.esp();
    t.tail();
    // both expired: the DPC thread runs the first one's routine
    let dpc = t.k.timer.dpc_thread.unwrap();
    assert_eq!(t.k.current, Some(dpc));
    assert_eq!((t.r32(t1 + 4), t.r32(t2 + 4)), (1, 1), "both timers are signaled");
    assert_eq!(t.k.gm.r8(t1 + 3).unwrap(), 0, "and out of the queue");
    assert_eq!(t.k.eip(), 0x5000);
    assert_eq!(t.k.irql(), 2);
    let sp = t.k.esp();
    assert_eq!(t.r32(sp), Kernel::trap(ORD_DPC_RETURN));
    assert_eq!([t.r32(sp + 4), t.r32(sp + 8)], [d1, 0x11]);
    assert_eq!(t.call32("KeIsExecutingDpc", &[]), 1);
    assert_eq!(t.k.gm.kr8(t.k.kpcr_va + 0x800), 1, "another DPC is pending");
    // it returns: the next one runs before any thread
    t.k.set_eip(Kernel::trap(ORD_DPC_RETURN));
    t.entry(Exit::Trap);
    assert_eq!((t.k.current, t.k.eip()), (Some(dpc), 0x6000));
    assert_eq!(t.r32(t.k.esp() + 8), 0x22);
    t.k.set_eip(Kernel::trap(ORD_DPC_RETURN));
    t.entry(Exit::Trap);
    assert_eq!(t.k.current, Some(main));
    assert_eq!((t.k.irql(), t.k.esp()), (0, esp_main));
    assert_eq!(t.k.gm.kr8(t.k.kpcr_va + 0x800), 0);
}

#[test]
fn a_dpc_queued_at_raised_irql_runs_when_the_irql_is_lowered() {
    let mut t = testing::kernel_with(|c| c.fast_paths = false);
    let d = t.alloc(0x40);
    t.w32(d + 0x0C, 0x5000);
    t.call("KeRaiseIrqlToDpcLevel", &[]);
    assert_eq!(t.call32("KeInsertQueueDpc", &[d, 1, 2]), 1);
    assert_eq!(t.call32("KeInsertQueueDpc", &[d, 1, 2]), 0, "already queued");
    t.tail();
    assert_ne!(t.k.current, t.k.timer.dpc_thread, "held while the thread is at DISPATCH_LEVEL");
    t.call("KfLowerIrql", &[0]);
    assert!(t.k.resched);
    t.tail();
    assert_eq!(t.k.current, t.k.timer.dpc_thread);
    assert_eq!([t.r32(t.k.esp() + 12), t.r32(t.k.esp() + 16)], [1, 2], "the system arguments");
    assert_eq!(t.call32("KeRemoveQueueDpc", &[d]), 0, "it left the queue as it started");
}

#[test]
fn a_periodic_timer_is_inserted_again_and_a_cancelled_one_does_not_fire() {
    let mut t = testing::kernel();
    t.start_clock();
    let m = t.alloc(0x100);
    t.call("KeInitializeTimerEx", &[m, 0]);
    t.call("KeInitializeTimerEx", &[m + 0x40, 0]);
    let due = (-20_000i64) as u64; // 2 ms
    t.call("KeSetTimerEx", &[m, due as u32, (due >> 32) as u32, 3, 0]);
    t.call("KeSetTimerEx", &[m + 0x40, due as u32, (due >> 32) as u32, 0, 0]);
    assert_eq!(t.call32("KeCancelTimer", &[m + 0x40]), 1);
    assert_eq!(t.call32("KeCancelTimer", &[m + 0x40]), 0);
    std::thread::sleep(Duration::from_millis(4));
    let (next, fired) = t.k.timer_poll();
    assert!(fired);
    assert_eq!(t.r32(m + 4), 1);
    assert_eq!(t.r32(m + 0x44), 0, "the cancelled timer did not fire");
    assert_eq!(t.k.gm.r8(m + 3).unwrap(), 1, "the periodic timer is still inserted");
    let left = next.unwrap().saturating_sub(t.k.now());
    assert!(left <= 3_000_000, "due again within its period: {left}");
}

// ------------------------------------------------------------ interrupts

fn connect(t: &mut TestKernel, level: u32, routine: u32) -> u32 {
    let obj = t.alloc(0x80);
    let irql = t.alloc(4);
    let vector = t.call32("HalGetInterruptVector", &[level, irql]);
    let irql = t.r32(irql) & 0xFF;
    t.call("KeInitializeInterrupt", &[obj, routine, 0xABC, vector, irql, 0, 0]);
    assert_eq!(t.call32("KeConnectInterrupt", &[obj]) & 0xFF, 1);
    obj
}

#[test]
fn an_asserted_line_runs_its_isr_at_its_irql_and_the_thread_resumes_after() {
    let mut t = testing::kernel();
    let main = t.k.current.unwrap();
    let obj = connect(&mut t, 3, 0x7000);
    t.hub.raise(Irq::Gpu);
    t.k.frame.regs.eflags |= eflags::IF;
    let (eip, esp) = (t.k.eip(), t.k.esp());
    t.tail();
    let isr = t.k.intr.isr_thread.unwrap();
    assert_eq!(t.k.current, Some(isr));
    assert_eq!((t.k.eip(), t.k.irql()), (0x7000, 23));
    let sp = t.k.esp();
    assert_eq!(
        [t.r32(sp), t.r32(sp + 4), t.r32(sp + 8)],
        [Kernel::trap(ORD_ISR_RETURN), obj, 0xABC]
    );
    // still asserted while its ISR runs: not delivered again
    t.tail();
    assert_eq!(t.k.intr.interrupts, 1);
    // the device is acknowledged and the ISR returns
    t.hub.lower(Irq::Gpu);
    t.k.set_eip(Kernel::trap(ORD_ISR_RETURN));
    t.entry(Exit::Trap);
    assert_eq!(t.k.current, Some(main));
    assert_eq!((t.k.eip(), t.k.esp(), t.k.irql()), (eip, esp, 0));
}

#[test]
fn a_line_is_held_while_interrupts_are_off_or_the_irql_is_at_its_level() {
    let mut t = testing::kernel_with(|c| c.fast_paths = false);
    connect(&mut t, 3, 0x7000);
    // nothing connected to line 5: asserted, but masked
    t.hub.raise(Irq::Apu);
    t.k.frame.regs.eflags |= eflags::IF;
    t.tail();
    assert!(t.k.intr.isr_thread.is_none() && !t.k.irq_window);
    // interrupts off: the kernel asks the CPU for the interrupt window
    t.hub.raise(Irq::Gpu);
    t.k.frame.regs.eflags &= !eflags::IF;
    t.tail();
    assert!(t.k.irq_window && t.k.intr.interrupts == 0);
    // IRQL at the line's level: held without a window, until it is lowered
    t.k.frame.regs.eflags |= eflags::IF;
    t.call("KfRaiseIrql", &[23]);
    t.tail();
    assert!(!t.k.irq_window && t.k.intr.interrupts == 0);
    t.call("KfLowerIrql", &[0]);
    t.tail();
    assert_eq!(t.k.intr.interrupts, 1);
    assert_eq!(t.k.current, t.k.intr.isr_thread);
}

#[test]
fn code_interrupted_at_dispatch_level_resumes_before_queued_dpcs() {
    let mut t = testing::kernel_with(|c| c.fast_paths = false);
    let main = t.k.current.unwrap();
    connect(&mut t, 1, 0x7000);
    let d = t.alloc(0x40);
    t.w32(d + 0x0C, 0x5000);
    t.call("KeRaiseIrqlToDpcLevel", &[]);
    t.k.frame.regs.eflags |= eflags::IF;
    t.hub.raise(Irq::Usb0);
    t.tail();
    assert_eq!(t.k.current, t.k.intr.isr_thread, "an ISR's entry switches even at DISPATCH_LEVEL");
    assert_eq!(t.k.irql(), 25);
    // the ISR queues a DPC and returns
    t.call("KeInsertQueueDpc", &[d, 0, 0]);
    t.hub.lower(Irq::Usb0);
    t.k.set_eip(Kernel::trap(ORD_ISR_RETURN));
    t.entry(Exit::Trap);
    assert_eq!(t.k.current, Some(main), "the interrupted code first");
    assert_eq!(t.k.irql(), 2);
    t.call("KfLowerIrql", &[0]);
    t.tail();
    assert_eq!(t.k.current, t.k.timer.dpc_thread, "then the DPC");
}

#[test]
fn synchronize_execution_runs_the_routine_at_the_interrupt_s_irql() {
    let mut t = testing::kernel();
    let obj = connect(&mut t, 3, 0x7000);
    let esp = t.k.esp();
    t.call("KeSynchronizeExecution", &[obj, 0x9000, 0x55]);
    assert_eq!((t.k.eip(), t.k.irql()), (0x9000, 23));
    let sp = t.k.esp();
    assert_eq!([t.r32(sp), t.r32(sp + 4)], [Kernel::trap(ORD_CALLOUT_RETURN), 0x55]);
    // the routine returns TRUE: that is the call's result, the IRQL is back
    t.k.set_reg(reg::EAX, 1);
    t.k.set_eip(Kernel::trap(ORD_CALLOUT_RETURN));
    t.k.kcall(&mut t.p, ORD_CALLOUT_RETURN);
    assert_eq!((t.k.eip(), t.k.esp(), t.k.irql(), t.k.reg(reg::EAX)), (CALL_RET, esp, 0, 1));
}

// -------------------------------------------------------------- exceptions

/// Links an exception registration with `handler` on the thread's stack.
fn register_handler(t: &mut TestKernel, handler: u32) -> u32 {
    let reg_va = t.k.esp() - 8;
    t.w32(reg_va, t.k.gm.kr32(t.k.kpcr_va));
    t.w32(reg_va + 4, handler);
    t.k.gm.kw32(t.k.kpcr_va, reg_va);
    t.k.set_esp(reg_va);
    reg_va
}

#[test]
fn a_page_fault_is_dispatched_to_the_registered_handler_with_its_record() {
    let mut t = testing::kernel();
    let reg_va = register_handler(&mut t, 0x4000);
    t.k.set_eip(0x1_1234);
    t.k.set_reg(reg::EBX, 0xB0B);
    let esp = t.k.esp();
    t.entry(Exit::Exception { vector: 14, error_code: 2, address: 0xDEAD_0000 });
    assert!(t.k.stop.is_none());
    assert_eq!(t.k.eip(), 0x4000, "the handler runs");
    // handler(ExceptionRecord, EstablisherFrame, ContextRecord, DispatcherContext)
    let sp = t.k.esp();
    assert!(sp < esp);
    let (rec, frame, ctx) = (t.r32(sp + 4), t.r32(sp + 8), t.r32(sp + 12));
    assert_eq!(t.r32(sp), Kernel::trap(ORD_SEH_RETURN));
    assert_eq!(frame, reg_va);
    assert_eq!(
        [t.r32(rec), t.r32(rec + 0xC), t.r32(rec + 0x10)],
        [STATUS_ACCESS_VIOLATION, 0x1_1234, 2]
    );
    assert_eq!([t.r32(rec + 0x14), t.r32(rec + 0x18)], [1, 0xDEAD_0000], "a write, and where");
    assert_eq!(
        [t.r32(ctx + 0x224), t.r32(ctx + 0x230), t.r32(ctx + 0x210)],
        [0x1_1234, esp, 0xB0B]
    );
    // the kernel's own registration is linked around the call
    let own = t.k.gm.kr32(t.k.kpcr_va);
    assert_eq!([t.r32(own), t.r32(own + 8)], [reg_va, reg_va]);
    // the handler edits the context and continues execution
    t.w32(ctx + 0x224, 0x1_5678);
    t.k.set_reg(reg::EAX, 0);
    t.k.set_esp(sp + 4);
    t.k.kcall(&mut t.p, ORD_SEH_RETURN);
    assert_eq!((t.k.eip(), t.k.esp(), t.k.reg(reg::EBX)), (0x1_5678, esp, 0xB0B));
    assert_eq!(t.k.gm.kr32(t.k.kpcr_va), reg_va, "the chain is as it was");
}

#[test]
fn an_exception_no_handler_takes_stops_the_guest_with_a_report() {
    let mut t = testing::kernel();
    t.k.set_eip(0x1_1234);
    t.entry(Exit::Exception { vector: 14, error_code: 0, address: 0x44 });
    let why = t.stop_reason().unwrap();
    assert!(why.contains("page fault: read of 00000044") && why.contains("00011234"), "{why}");
    // with a handler that declines, the search runs off the chain
    let mut t = testing::kernel();
    register_handler(&mut t, 0x4000);
    t.k.set_eip(0x1_1234);
    t.entry(Exit::Exception { vector: 0, error_code: 0, address: 0 });
    assert_eq!(t.k.eip(), 0x4000);
    t.k.set_reg(reg::EAX, 1); // ExceptionContinueSearch
    t.k.set_esp(t.k.esp() + 4);
    t.k.kcall(&mut t.p, ORD_SEH_RETURN);
    let why = t.stop_reason().unwrap();
    assert!(
        why.contains("unhandled exception c0000094") && why.contains("no handler took it"),
        "{why}"
    );
    let report = t.k.report(&why);
    assert!(report.threads.iter().any(|l| l.contains("running")));
    assert!(report.registers.contains("eip"));
}

#[test]
fn rtl_unwind_calls_the_handlers_above_the_target_and_unlinks_them() {
    let mut t = testing::kernel();
    let outer = register_handler(&mut t, 0x4000);
    let inner = register_handler(&mut t, 0x4100);
    let esp = t.k.esp();
    // RtlUnwind(TargetFrame = outer, TargetIp, ExceptionRecord = NULL, ReturnValue)
    t.call("RtlUnwind", &[outer, 0, 0, 0x5A]);
    assert_eq!(t.k.eip(), 0x4100, "the inner frame's handler is called to unwind");
    let sp = t.k.esp();
    let rec = t.r32(sp + 4);
    assert_eq!(t.r32(sp + 8), inner);
    assert_eq!(t.r32(rec), 0xC000_0027, "STATUS_UNWIND");
    assert_eq!(t.r32(rec + 4) & 2, 2, "EXCEPTION_UNWINDING");
    t.k.set_reg(reg::EAX, 1);
    t.k.set_esp(sp + 4);
    t.k.kcall(&mut t.p, ORD_SEH_RETURN);
    // the target reached: RtlUnwind returns to its caller with ReturnValue
    assert_eq!((t.k.eip(), t.k.esp(), t.k.reg(reg::EAX)), (CALL_RET, esp, 0x5A));
    assert_eq!(t.k.gm.kr32(t.k.kpcr_va), outer, "the unwound frame is unlinked");
}

#[test]
fn a_kernel_call_that_touches_a_missing_page_raises_the_title_s_access_violation() {
    let mut t = testing::kernel();
    // with no handler: a fatal stop naming the export and the address
    t.call("KeInitializeEvent", &[0x6000_0000, 0, 0]);
    let why = t.stop_reason().unwrap();
    assert!(why.contains("KeInitializeEvent touched 60000000"), "{why}");
    // with a handler: dispatched at the call, which runs again afterwards
    let mut t = testing::kernel();
    register_handler(&mut t, 0x4000);
    t.call("KeInitializeEvent", &[0x6000_0000, 0, 0]);
    assert!(t.k.stop.is_none());
    assert_eq!(t.k.eip(), 0x4000);
    let rec = t.r32(t.k.esp() + 4);
    let ctx = t.r32(t.k.esp() + 12);
    let entry = TRAP_VA + u32::from(crate::exports::by_name("KeInitializeEvent").unwrap().ordinal);
    assert_eq!(
        [t.r32(rec), t.r32(rec + 0x14), t.r32(rec + 0x18)],
        [STATUS_ACCESS_VIOLATION, 1, 0x6000_0000]
    );
    assert_eq!(t.r32(ctx + 0x224), entry, "continuing re-enters the export");
}

#[test]
fn the_debug_service_prints_and_skips_its_breakpoint() {
    let mut t = testing::kernel();
    let m = t.alloc(0x100);
    t.k.gm.write(m + 0x10, b"hello\n\xCC").unwrap();
    t.k.gm.w16(m, 6).unwrap();
    t.w32(m + 4, m + 0x10);
    t.k.set_reg(reg::EAX, 1);
    t.k.set_reg(reg::ECX, m);
    t.k.set_eip(m + 0x16); // past the int 2Dh, at the int3
    t.entry(Exit::SoftwareInterrupt(0x2D));
    assert_eq!((t.k.eip(), t.k.reg(reg::EAX)), (m + 0x17, 0));
    t.entry(Exit::SoftwareInterrupt(0x21));
    assert!(t.stop_reason().unwrap().contains("int 21h"));
}

// ---------------------------------------------------------------- run loop

#[test]
fn the_run_loop_dispatches_traps_and_stops_or_pauses_on_request() {
    let mut t = testing::kernel_with(|c| c.fast_paths = false);
    let irql =
        TRAP_VA + u32::from(crate::exports::by_name("KeRaiseIrqlToDpcLevel").unwrap().ordinal);
    let esp = t.k.esp();
    // the guest "calls" an export: the return address is on its stack
    t.cpu.script.push_back(Box::new(move |ctx| {
        ctx.regs.gpr[reg::ESP] -= 4;
        ctx.regs.eip = irql;
        Exit::Trap
    }));
    let control = t.k.control();
    let c2 = control.clone();
    t.cpu.script.push_back(Box::new(move |ctx| {
        assert_eq!(ctx.regs.gpr[reg::EAX], 0, "the old IRQL came back in EAX");
        c2.request_pause();
        Exit::Kicked
    }));
    t.w32(esp - 4, 0x1_2222);
    let clock = t.k.clock();
    assert_eq!(clock.now_ns(), 0);
    let out = t.k.run(&mut t.cpu, &mut NullBus, &mut t.p);
    assert!(matches!(out, RunOutcome::Paused), "{out:?}");
    assert_eq!((t.k.eip(), t.k.esp(), t.k.irql()), (0x1_2222, esp, 2));
    assert_eq!(t.p.ticks, 1, "the machine's devices were given their first tick");
    // paused: guest time stands still
    let at = clock.now_ns();
    assert!(at > 0);
    std::thread::sleep(Duration::from_millis(5));
    assert_eq!(clock.now_ns(), at);
    // a stop asked for before the next run is taken at once
    control.request_stop();
    let out = t.k.run(&mut t.cpu, &mut NullBus, &mut t.p);
    assert!(matches!(out, RunOutcome::Stopped));
    assert_eq!(t.cpu.runs, 2);
    // a CPU failure is fatal, with the last calls in the report
    t.cpu.script.push_back(Box::new(|_| Exit::Fatal("triple fault".into())));
    match t.k.run(&mut t.cpu, &mut NullBus, &mut t.p) {
        RunOutcome::Fatal(r) => {
            assert!(r.reason.contains("triple fault"));
            assert!(r.recent_calls.iter().any(|l| l.contains("KeRaiseIrqlToDpcLevel")));
            assert!(r.to_string().contains("thread t4"));
        }
        other => panic!("{other:?}"),
    }
    assert!(
        matches!(t.k.run(&mut t.cpu, &mut NullBus, &mut t.p), RunOutcome::Fatal(_)),
        "finished for good"
    );
}

#[test]
fn an_idle_kernel_returns_for_a_pause_and_resumes_its_wait() {
    let mut t = testing::kernel();
    let ev = t.event(1, false);
    let main = t.k.current.unwrap();
    // the only thread waits for an event: the scheduler idles
    t.call("NtWaitForSingleObject", &[ev, 0, 0]);
    t.k.control().request_pause();
    t.tail();
    assert!(t.k.sched.picking, "the pick is unfinished");
    let out = t.k.run(&mut t.cpu, &mut NullBus, &mut t.p);
    assert!(matches!(out, RunOutcome::Paused));
    assert_eq!(t.cpu.runs, 0, "no guest code ran");
    // the event is set from outside (an I/O completion would); a wake ends the idle wait
    let (obj, _) = t.k.ob_lookup(ev).unwrap();
    t.w32(obj + 4, 1);
    let control = t.k.control();
    t.cpu.script.push_back(Box::new(move |ctx| {
        assert_eq!(ctx.regs.gpr[reg::EAX], 0, "the wait's status");
        control.request_stop();
        Exit::Kicked
    }));
    let out = t.k.run(&mut t.cpu, &mut NullBus, &mut t.p);
    assert!(matches!(out, RunOutcome::Stopped));
    assert_eq!(t.k.current, Some(main));
    assert_eq!(t.k.threads[main].state, ThreadState::Running);
}

#[test]
fn the_tick_count_and_times_follow_guest_time() {
    let mut t = testing::kernel();
    t.start_clock();
    let tick = t.k.data_addr("KeTickCount");
    let t0 = t.k.gm.kr32(tick);
    std::thread::sleep(Duration::from_millis(12));
    t.tail();
    let dt = t.k.gm.kr32(tick) - t0;
    assert!((11..200).contains(&dt), "{dt} ticks in 12 ms");
    let out = t.alloc(8);
    t.call("KeQuerySystemTime", &[out]);
    let sys = t.k.gm.r64(out).unwrap();
    let it = t.call("KeQueryInterruptTime", &[]);
    assert_eq!(t.k.gm.kr32(t.k.data_addr("KeSystemTime") + 4), (sys >> 32) as u32);
    assert!(sys > 0x01C0_739B_EB8F_8000, "after 2001");
    assert!(it >= 120_000, "100 ns units since boot: {it}");
    // NtSetSystemTime moves the system time, not the interrupt time
    t.k.gm.w64(out, sys + 10_000_000 * 3600).unwrap();
    assert_eq!(t.call32("NtSetSystemTime", &[out, 0]), 0);
    t.call("KeQuerySystemTime", &[out]);
    let moved = t.k.gm.r64(out).unwrap() - sys;
    assert!((36_000_000_000..36_100_000_000u64).contains(&moved));
    assert!(t.call("KeQueryInterruptTime", &[]) - it < 100_000_000);
    let pc = t.call("KeQueryPerformanceCounter", &[]);
    assert!(pc > 0 && t.call("KeQueryPerformanceFrequency", &[]) == 3_375_000);
}

// ------------------------------------------------------------- the docs

/// The export names `STUBS.md` lists (backticked, in its table's first column).
fn stand_ins() -> Vec<String> {
    let text = include_str!("../STUBS.md");
    let mut out = Vec::new();
    for line in text.lines().filter(|l| l.starts_with("| `")) {
        let first = line.split('|').nth(1).unwrap_or("");
        out.extend(first.split('`').skip(1).step_by(2).map(str::to_owned));
    }
    out
}

/// The export table of `docs/hw/kernel-exports.md`, from the kernel as built.
fn export_table() -> String {
    use std::fmt::Write;
    let t = testing::kernel();
    let stubs = stand_ins();
    let (mut done, mut stub, mut missing, mut data) = (0, 0, 0, 0);
    let mut rows = String::new();
    for e in crate::exports::all() {
        let o = u32::from(e.ordinal);
        let abi = match e.abi {
            crate::exports::Abi::Stdcall => format!("stdcall, {} bytes", e.arg_bytes),
            crate::exports::Abi::Fastcall => format!("fastcall, {} bytes", e.arg_bytes),
            crate::exports::Abi::Cdecl => "cdecl".to_owned(),
            crate::exports::Abi::Data => "data".to_owned(),
        };
        let fast = crate::fast::FAST.iter().any(|(n, _)| *n == e.name);
        let status = if e.abi == crate::exports::Abi::Data {
            data += 1;
            format!("data at `{:08x}`", t.k.resolve(o))
        } else if !t.k.implemented(o) {
            missing += 1;
            "unimplemented".to_owned()
        } else if stubs.iter().any(|s| s == e.name) {
            stub += 1;
            "stand-in (STUBS.md)".to_owned()
        } else {
            done += 1;
            if fast {
                "implemented (guest code, kernel for the rest)".to_owned()
            } else {
                "implemented".to_owned()
            }
        };
        let _ = writeln!(rows, "| {o} | `{}` | {abi} | {status} |", e.name);
    }
    format!(
        "{} function exports implemented, {} documented stand-ins, {} unimplemented (a call stops the guest and names the export); {} data exports.\n\n| Ordinal | Export | Convention | Status |\n|---|---|---|---|\n{rows}",
        done, stub, missing, data
    )
}

const TABLE_BEGIN: &str = "<!-- export table: generated by the test below; do not edit -->\n";
const TABLE_END: &str = "<!-- end of the export table -->\n";

/// `XB_KERNEL_WRITE_DOCS=1 cargo test -p xb-kernel the_export_table` rewrites
/// the table in `docs/hw/kernel-exports.md`; without it the test checks that
/// the document says what the kernel does.
#[test]
fn the_export_table_in_the_docs_is_the_kernel_s() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/hw/kernel-exports.md");
    let doc = std::fs::read_to_string(path).expect("docs/hw/kernel-exports.md");
    let a = doc.find(TABLE_BEGIN).expect("the table's begin marker") + TABLE_BEGIN.len();
    let b = doc.find(TABLE_END).expect("the table's end marker");
    let table = export_table();
    if std::env::var_os("XB_KERNEL_WRITE_DOCS").is_some() {
        std::fs::write(path, format!("{}{table}{}", &doc[..a], &doc[b..])).expect("write the docs");
        return;
    }
    assert!(
        doc[a..b] == table,
        "docs/hw/kernel-exports.md is stale: run with XB_KERNEL_WRITE_DOCS=1"
    );
}

#[test]
fn every_stand_in_named_in_stubs_md_is_a_registered_function_export() {
    let t = testing::kernel();
    for name in stand_ins() {
        let e = crate::exports::by_name(&name)
            .unwrap_or_else(|| panic!("STUBS.md names {name}, not an export"));
        assert!(
            t.k.implemented(u32::from(e.ordinal)),
            "STUBS.md names {name}, which is not implemented"
        );
    }
}

/// Pins what a savestate holds of this: see `xb_types::state::StateIo::probe`.
#[test]
fn the_saved_layout_of_the_kernel_is_the_recorded_one() {
    let mut io = xb_types::state::StateIo::probe();
    let mut t = testing::kernel();
    t.k.state(&mut io);
    assert_eq!(
        io.shape_hash(),
        0x1C0A_AA4E_F769_919B,
        "the saved layout changed. Older states would be misread: give the section a new version, \
         read the old one where it differs, then record {:#018x} here",
        io.shape_hash()
    );
}

/// Halo's wait for a vertical blank at `code`: a 64-bit count at `count`
/// against the target in ESI:EDI (zero and `target` here).
fn wait_loop_at(t: &mut TestKernel, target: u32) -> (u32, u32) {
    let code = t.alloc(0x100);
    let count = t.alloc(8);
    let mut c = vec![0xA1];
    c.extend(count.to_le_bytes()); // top: mov eax, [count]
    c.extend([0x39, 0x35]);
    c.extend((count + 4).to_le_bytes()); // cmp [count+4], esi
    c.extend([0x7C, 0xF3, 0x7F, 0x04, 0x3B, 0xC7, 0x72, 0xED, 0xC3]); // jl top; jg out; cmp eax, edi; jb top; out: ret
    t.k.gm.write(code, &c).expect("guest memory");
    t.k.frame.regs.eip = code;
    t.k.frame.regs.eflags = eflags::IF | eflags::FIXED;
    t.k.frame.regs.gpr[reg::ESI] = 0;
    t.k.frame.regs.gpr[reg::EDI] = target;
    (code, count)
}

#[test]
fn a_thread_spinning_on_memory_is_waited_out_until_the_memory_changes() {
    let mut t = testing::kernel();
    let (code, count) = wait_loop_at(&mut t, 3);
    assert!(!t.k.in_wait_loop(), "not looked at before a kick stopped it there");
    t.tail(); // a kick
    assert_eq!(t.k.eip(), code);
    assert!(t.k.in_wait_loop(), "found at the kick");
    let before = t.k.stats.spin_waits;
    t.k.spin_wait(&mut t.p);
    assert_eq!(t.k.stats.spin_waits, before + 1);
    assert_eq!(t.k.eip(), code, "the thread stays where it is");
    assert!(t.k.in_wait_loop(), "and is proven again there without a kick");
    // what the vblank's handler does
    t.w32(count, 3);
    assert!(!t.k.in_wait_loop(), "the count reached the target: the loop runs");
}

#[test]
fn a_wait_loop_is_left_to_run_where_nothing_could_end_it_or_when_turned_off() {
    let mut t = testing::kernel();
    wait_loop_at(&mut t, 3);
    t.tail();
    assert!(t.k.in_wait_loop());
    t.k.frame.regs.eflags &= !eflags::IF;
    assert!(!t.k.in_wait_loop(), "interrupts off");
    t.k.frame.regs.eflags |= eflags::IF;
    t.k.set_irql(2);
    assert!(!t.k.in_wait_loop(), "at DISPATCH_LEVEL no DPC or thread can run");
    t.k.set_irql(0);
    assert!(t.k.in_wait_loop());

    let mut t = testing::kernel_with(|cfg| cfg.wait_loops = false);
    wait_loop_at(&mut t, 3);
    t.tail();
    assert!(!t.k.in_wait_loop(), "turned off");
}

#[test]
fn a_thread_in_a_wait_loop_keeps_its_quantum_with_a_peer_ready() {
    let mut t = testing::kernel();
    let main = t.k.current.unwrap();
    let (peer, _) = t.spawn(0x2000);
    assert_eq!(t.k.threads[peer].state, ThreadState::Ready);
    wait_loop_at(&mut t, 3);
    t.tail();
    assert!(t.k.in_wait_loop());
    t.k.spin_wait(&mut t.p);
    assert_eq!(t.k.current, Some(main), "the peer waits, as with the loop running");
    assert_eq!(t.k.stats.slices, 0, "and the quantum is not cut short");
    // the quantum's end is the scheduler's, as for any thread
    let now = t.k.now();
    t.k.sched.slice_thread = Some(main);
    t.k.sched.slice_due = Some(now);
    t.k.spin_wait(&mut t.p);
    assert_eq!(t.k.stats.slices, 1);
}

#[test]
fn a_thread_held_in_a_wait_loop_still_runs_now_and_then() {
    let mut t = testing::kernel();
    wait_loop_at(&mut t, 3);
    t.tail();
    assert!(t.k.in_wait_loop());
    assert!(t.k.in_wait_loop(), "held while the interval lasts");
    // the interval is over: whatever the proof says, the guest runs once
    t.k.spin.run_at = Some(std::time::Instant::now());
    assert!(!t.k.in_wait_loop(), "its turn to run for real");
    assert!(t.k.in_wait_loop(), "and held again after it, for another interval");
    let again = t.k.spin.run_at.expect("an interval is running");
    assert!(again > std::time::Instant::now());
    // a thread that left the loop starts a new interval when it is next held
    t.k.frame.regs.gpr[reg::EDI] = 0;
    assert!(!t.k.in_wait_loop());
    assert_eq!(t.k.spin.run_at, None);
}
