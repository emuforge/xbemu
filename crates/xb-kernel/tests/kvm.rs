//! Small hand-assembled guest programs run under the real kernel on the KVM
//! backend: export calls through thunks (stdcall and fastcall), a wait
//! handed off between two guest threads, a timer's DPC, an interrupt line
//! asserted from another host thread reaching a connected guest ISR, and an
//! exception dispatched to a guest SEH handler.
//!
//! Each test prints a note and passes when KVM cannot be used on the host.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{Asm, CODE, DATA, TempDir, TestBus};
use xb_cpu_kvm::KvmCpu;
use xb_kernel::{BootOptions, Kernel, KernelConfig, NullPlatform, RunOutcome, exports};
use xb_mem::Ram;
use xb_types::irq::{Irq, IrqHub};

fn ord(name: &str) -> u32 {
    u32::from(exports::by_name(name).unwrap_or_else(|| panic!("no export {name}")).ordinal)
}

struct Machine {
    cpu: KvmCpu,
    kernel: Kernel,
    bus: TestBus,
    hub: Arc<IrqHub>,
    _hdd: TempDir,
}

fn machine(tag: &str, asm: &Asm, fast_paths: bool) -> Option<Machine> {
    common::init_log();
    if let Err(why) = KvmCpu::probe() {
        println!("note: KVM is not usable here ({why}); test skipped");
        return None;
    }
    let ram = Ram::new(64 << 20).expect("guest RAM");
    let mut cpu = KvmCpu::new(ram.fd(), ram.size()).expect("KVM vCPU");
    let hub = Arc::new(IrqHub::new());
    let mut cfg = KernelConfig::new(ram, Arc::clone(&hub));
    cfg.fast_paths = fast_paths;
    let mut kernel = Kernel::new(cfg, &mut cpu).expect("kernel");
    let wake = kernel.wake_handle();
    hub.set_kick(Box::new(move || wake.wake()));
    let hdd = TempDir::new(tag);
    kernel
        .boot(
            &mut cpu,
            BootOptions {
                disc: Some(asm.disc()),
                hdd_root: hdd.0.clone(),
                ..BootOptions::default()
            },
        )
        .expect("boot");
    let bus = TestBus::new(Arc::clone(&hub));
    Some(Machine { cpu, kernel, bus, hub, _hdd: hdd })
}

impl Machine {
    /// Runs until the program's end marker (`cli ; hlt`); panics with the
    /// report on anything else, or after `secs` seconds.
    fn run_to_done(&mut self, secs: u64) {
        let control = self.kernel.control();
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let dog = std::thread::spawn(move || {
            if rx.recv_timeout(Duration::from_secs(secs)).is_err() {
                control.request_stop();
            }
        });
        let out = self.kernel.run(&mut self.cpu, &mut self.bus, &mut NullPlatform);
        drop(tx);
        dog.join().unwrap();
        match out {
            RunOutcome::Fatal(r) if r.reason.contains("HLT with interrupts off") => {}
            RunOutcome::Stopped => {
                panic!("the guest did not finish in {secs} s\n{}", self.kernel.report("timeout"))
            }
            other => panic!("unexpected outcome: {other:#?}"),
        }
    }

    fn r32(&self, va: u32) -> u32 {
        let mut b = [0u8; 4];
        assert!(self.kernel.read_guest(va, &mut b), "guest memory at {va:08x}");
        u32::from_le_bytes(b)
    }
}

fn exports_program() -> Asm {
    let mut a = Asm::new();
    a.store_esp(DATA + 28);
    a.call_export(ord("KeQueryPerformanceFrequency")).store_eax(DATA);
    a.mov_ecx(5).call_export(ord("KfRaiseIrql")).store_eax(DATA + 4);
    a.call_export(ord("KeGetCurrentIrql")).store_eax(DATA + 8);
    a.mov_ecx(0).call_export(ord("KfLowerIrql"));
    a.call_export(ord("KeGetCurrentIrql")).store_eax(DATA + 12);
    // NtCreateEvent(&handle, NULL, SynchronizationEvent, FALSE)
    a.push(0).push(1).push(0).push(DATA + 0x20);
    a.call_export(ord("NtCreateEvent")).store_eax(DATA + 16);
    // InterlockedCompareExchange(&v, 7, 0): fastcall with a stack argument
    a.mov_ecx(DATA + 0x30).mov_edx(7).push(0);
    a.call_export(ord("InterlockedCompareExchange")).store_eax(DATA + 0x34);
    // KeQueryPerformanceCounter: a 64-bit result in EDX:EAX
    a.call_export(ord("KeQueryPerformanceCounter")).store_eax(DATA + 0x38);
    a.raw(&[0x89, 0x15]).raw(&(DATA + 0x3C).to_le_bytes()); // mov [..], edx
    a.store_esp(DATA + 24);
    a.done();
    a
}

fn check_exports(m: &Machine) {
    assert_eq!(m.r32(DATA), 3_375_000, "KeQueryPerformanceFrequency");
    assert_eq!(m.r32(DATA + 4), 0, "KfRaiseIrql returns the old IRQL");
    assert_eq!(m.r32(DATA + 8), 5, "the IRQL was raised");
    assert_eq!(m.r32(DATA + 12), 0, "KfLowerIrql lowered it");
    assert_eq!(m.r32(DATA + 16), 0, "NtCreateEvent succeeds");
    let h = m.r32(DATA + 0x20);
    assert!(h != 0 && h.is_multiple_of(4), "a handle: {h:x}");
    assert_eq!(m.r32(DATA + 0x34), 0, "the old value");
    assert_eq!(m.r32(DATA + 0x30), 7, "the exchange happened");
    assert_eq!(m.r32(DATA + 24), m.r32(DATA + 28), "every call popped its own arguments");
    assert!(m.kernel.call_count() >= 2);
}

#[test]
fn a_thread_calls_stdcall_and_fastcall_exports_through_its_thunks() {
    let Some(mut m) = machine("exports", &exports_program(), true) else { return };
    m.run_to_done(20);
    check_exports(&m);
}

#[test]
fn the_same_calls_agree_when_every_export_enters_the_kernel() {
    let Some(mut m) = machine("exports-slow", &exports_program(), false) else { return };
    m.run_to_done(20);
    check_exports(&m);
    assert!(m.kernel.call_count() >= 8, "no guest-code fast paths: {}", m.kernel.call_count());
}

#[test]
fn a_wait_is_handed_off_between_two_guest_threads() {
    const WORKER: u32 = CODE + 0x400;
    let mut a = Asm::new();
    // NtCreateEvent(&event, NULL, SynchronizationEvent, FALSE)
    a.push(0).push(1).push(0).push(DATA);
    a.call_export(ord("NtCreateEvent"));
    // PsCreateSystemThreadEx(&thread, 0, 0x4000, 0, &id, worker, 0x1234, FALSE, FALSE, NULL)
    a.push(0).push(0).push(0).push(0x1234).push(WORKER).push(DATA + 8).push(0).push(0x4000).push(0);
    a.push(DATA + 4);
    a.call_export(ord("PsCreateSystemThreadEx")).store_eax(DATA + 0x10);
    // NtWaitForSingleObject(event, FALSE, NULL): blocks until the worker sets it
    a.push(0).push(0).push_mem(DATA);
    a.call_export(ord("NtWaitForSingleObject")).store_eax(DATA + 0x14);
    // the worker ran first: its mark is there
    a.raw(&[0xA1]).raw(&(DATA + 0x18).to_le_bytes()); // mov eax, [DATA+0x18]
    a.store_eax(DATA + 0x1C);
    // NtWaitForSingleObject(thread, FALSE, NULL): the worker's exit signals its object
    a.push(0).push(0).push_mem(DATA + 4);
    a.call_export(ord("NtWaitForSingleObject")).store_eax(DATA + 0x20);
    a.done();
    a.pad_to(WORKER);
    // worker(context): mark = context; NtSetEvent(event, NULL); return
    a.raw(&[0x8B, 0x44, 0x24, 0x04]); // mov eax, [esp+4]
    a.store_eax(DATA + 0x18);
    a.push(0).push_mem(DATA);
    a.call_export(ord("NtSetEvent"));
    a.raw(&[0xC2, 0x04, 0x00]); // ret 4

    let Some(mut m) = machine("handoff", &a, true) else { return };
    m.run_to_done(20);
    assert_eq!(m.r32(DATA + 0x10), 0, "PsCreateSystemThreadEx succeeds");
    assert_ne!(m.r32(DATA + 8), 0, "a thread id");
    assert_eq!(m.r32(DATA + 0x14), 0, "the wait was satisfied (STATUS_WAIT_0)");
    assert_eq!(
        m.r32(DATA + 0x1C),
        0x1234,
        "the worker ran with its context before the waiter went on"
    );
    assert_eq!(
        m.r32(DATA + 0x20),
        0,
        "the thread object is signaled once its start routine returned"
    );
}

#[test]
fn a_timer_s_dpc_fires_while_the_thread_spins_in_guest_code() {
    const DPC_ROUTINE: u32 = CODE + 0x400;
    const TIMER: u32 = DATA + 0x100;
    const DPC: u32 = DATA + 0x140;
    let mut a = Asm::new();
    // a KDPC by hand: DeferredRoutine 0x0C, DeferredContext 0x10
    a.store_imm(DPC, 19);
    a.store_imm(DPC + 0x0C, DPC_ROUTINE);
    a.store_imm(DPC + 0x10, 0xC0DE);
    // KeInitializeTimerEx(timer, NotificationTimer)
    a.push(0).push(TIMER).call_export(ord("KeInitializeTimerEx"));
    // KeSetTimer(timer, -5 ms (by value), dpc)
    let due = (-50_000i64) as u64;
    a.push(DPC).push((due >> 32) as u32).push(due as u32).push(TIMER);
    a.call_export(ord("KeSetTimer")).store_eax(DATA + 4);
    a.call_export(ord("KeQueryInterruptTime")).store_eax(DATA + 0x20);
    // spin in guest code: only the clock can bring the DPC in
    a.spin_until(DATA);
    a.call_export(ord("KeQueryInterruptTime")).store_eax(DATA + 0x24);
    a.done();
    a.pad_to(DPC_ROUTINE);
    // routine(Dpc, DeferredContext, SystemArgument1, SystemArgument2)
    a.raw(&[0x8B, 0x44, 0x24, 0x08]); // mov eax, [esp+8]
    a.store_eax(DATA + 8);
    a.raw(&[0x8B, 0x44, 0x24, 0x04]); // mov eax, [esp+4]
    a.store_eax(DATA + 12);
    a.call_export(ord("KeGetCurrentIrql")).store_eax(DATA + 16);
    a.store_imm(DATA, 1);
    a.raw(&[0xC2, 0x10, 0x00]); // ret 16

    let Some(mut m) = machine("timer", &a, true) else { return };
    m.run_to_done(20);
    assert_eq!(m.r32(DATA + 4), 0, "the timer was not inserted before");
    assert_eq!(m.r32(DATA + 8), 0xC0DE, "the DPC got its context");
    assert_eq!(m.r32(DATA + 12), DPC, "and its KDPC");
    assert_eq!(m.r32(DATA + 16), 2, "DPCs run at DISPATCH_LEVEL");
    let waited = m.r32(DATA + 0x24).wrapping_sub(m.r32(DATA + 0x20));
    assert!((50_000..5_000_000).contains(&waited), "fired after about 5 ms: {} us", waited / 10);
}

#[test]
fn an_interrupt_line_raised_by_another_thread_reaches_the_guest_s_isr() {
    const ISR: u32 = CODE + 0x400;
    const INTR: u32 = DATA + 0x100;
    let mut a = Asm::new();
    // HalGetInterruptVector(3, &irql)
    a.push(DATA + 0x40).push(3).call_export(ord("HalGetInterruptVector")).store_eax(DATA + 0x44);
    // KeInitializeInterrupt(obj, isr, context, vector, irql, LevelSensitive, FALSE)
    a.push(0).push(0).push_mem(DATA + 0x40).push_mem(DATA + 0x44).push(0xFEED).push(ISR).push(INTR);
    a.call_export(ord("KeInitializeInterrupt"));
    a.push(INTR).call_export(ord("KeConnectInterrupt")).store_eax(DATA + 4);
    a.store_imm(DATA + 0x48, 1); // connected: the test may raise the line
    a.spin_until(DATA);
    a.done();
    a.pad_to(ISR);
    // isr(Interrupt, ServiceContext): acknowledge the device, mark, return TRUE
    a.raw(&[0x8B, 0x44, 0x24, 0x08]); // mov eax, [esp+8]
    a.store_eax(DATA + 8);
    a.call_export(ord("KeGetCurrentIrql")).store_eax(DATA + 12);
    a.raw(&[0x66, 0xBA]).raw(&TestBus::ACK_PORT.to_le_bytes()); // mov dx, port
    a.raw(&[0xB0, 0x03, 0xEE]); // mov al, 3 ; out dx, al
    a.store_imm(DATA, 1);
    a.raw(&[0xB8, 0x01, 0x00, 0x00, 0x00, 0xC2, 0x08, 0x00]); // mov eax, 1 ; ret 8

    let Some(mut m) = machine("irq", &a, true) else { return };
    let hub = Arc::clone(&m.hub);
    let raiser = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        hub.raise(Irq::Gpu);
    });
    m.run_to_done(20);
    raiser.join().unwrap();
    assert_eq!(m.r32(DATA + 0x44), 0x33, "the vector of bus level 3");
    assert_eq!(m.r32(DATA + 0x40) & 0xFF, 23, "its IRQL");
    assert_eq!(m.r32(DATA + 4) & 0xFF, 1, "KeConnectInterrupt");
    assert_eq!(m.r32(DATA + 8), 0xFEED, "the ISR got its context");
    assert_eq!(m.r32(DATA + 12), 23, "the ISR runs at the interrupt's IRQL");
    assert_eq!(m.hub.pending(), 0, "the ISR's acknowledge lowered the line");
    assert!(m.bus.log.contains(&common::Access::PortWrite(TestBus::ACK_PORT, 1, 3)));
}

#[test]
fn an_interrupt_waits_for_the_guest_to_enable_interrupts() {
    const ISR: u32 = CODE + 0x400;
    const INTR: u32 = DATA + 0x100;
    let mut a = Asm::new();
    a.push(0).push(0).push(23).push(0x33).push(0).push(ISR).push(INTR);
    a.call_export(ord("KeInitializeInterrupt"));
    a.push(INTR).call_export(ord("KeConnectInterrupt"));
    a.raw(&[0xFA]); // cli
    a.store_imm(DATA + 0x48, 1);
    // a long stretch with interrupts off: the line rises during it
    a.raw(&[0xB9]).raw(&400_000_000u32.to_le_bytes()); // mov ecx, n
    a.raw(&[0x49, 0x75, 0xFD]); // L: dec ecx ; jnz L
    a.raw(&[0xA1]).raw(&DATA.to_le_bytes()); // mov eax, [DATA]
    a.store_eax(DATA + 4); // still 0: not delivered with IF clear
    a.raw(&[0xFB, 0x90]); // sti ; nop
    a.spin_until(DATA);
    a.done();
    a.pad_to(ISR);
    a.raw(&[0x66, 0xBA]).raw(&TestBus::ACK_PORT.to_le_bytes());
    a.raw(&[0xB0, 0x03, 0xEE]);
    a.store_imm(DATA, 1);
    a.raw(&[0xB8, 0x01, 0x00, 0x00, 0x00, 0xC2, 0x08, 0x00]);

    let Some(mut m) = machine("irq-window", &a, true) else { return };
    let hub = Arc::clone(&m.hub);
    let raiser = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(30));
        hub.raise(Irq::Gpu);
    });
    m.run_to_done(30);
    raiser.join().unwrap();
    assert_eq!(m.r32(DATA + 4), 0, "the ISR did not run while interrupts were off");
    assert_eq!(m.r32(DATA), 1, "and ran once they were on");
}

#[test]
fn an_access_violation_is_dispatched_to_the_guest_s_seh_handler() {
    const HANDLER: u32 = CODE + 0x400;
    const RESUME: u32 = CODE + 0x300;
    let mut a = Asm::new();
    // push handler ; push fs:[0] ; mov fs:[0], esp
    a.push(HANDLER);
    a.raw(&[0x64, 0xFF, 0x35, 0, 0, 0, 0]);
    a.raw(&[0x64, 0x89, 0x25, 0, 0, 0, 0]);
    a.raw(&[0xBB, 0x78, 0x56, 0x34, 0x12]); // mov ebx, 0x12345678
    let fault_at = a.here();
    a.raw(&[0xC7, 0x05, 0x10, 0, 0, 0, 0x01, 0, 0, 0]); // mov dword [0x10], 1: no page there
    a.store_imm(DATA + 0x30, 0xBAD); // skipped: the handler continues at RESUME
    a.pad_to(RESUME);
    a.raw(&[0x89, 0x1D]).raw(&(DATA + 0x20).to_le_bytes()); // mov [..], ebx
    // unlink the registration: pop fs:[0] ; add esp, 4
    a.raw(&[0x64, 0x8F, 0x05, 0, 0, 0, 0, 0x83, 0xC4, 0x04]);
    a.store_esp(DATA + 0x24);
    a.store_imm(DATA, 1);
    a.done();
    a.pad_to(HANDLER);
    // handler(ExceptionRecord, EstablisherFrame, ContextRecord, DispatcherContext), cdecl
    a.raw(&[0x8B, 0x4C, 0x24, 0x04]); // mov ecx, [esp+4]
    a.raw(&[0x8B, 0x01]).store_eax(DATA + 4); // code
    a.raw(&[0x8B, 0x41, 0x0C]).store_eax(DATA + 8); // address
    a.raw(&[0x8B, 0x41, 0x10]).store_eax(DATA + 12); // parameters
    a.raw(&[0x8B, 0x41, 0x14]).store_eax(DATA + 16); // [0]: write
    a.raw(&[0x8B, 0x41, 0x18]).store_eax(DATA + 20); // [1]: address
    a.raw(&[0x8B, 0x44, 0x24, 0x08]).store_eax(DATA + 0x28); // the establisher frame
    a.raw(&[0x8B, 0x4C, 0x24, 0x0C]); // mov ecx, [esp+12]: CONTEXT
    a.raw(&[0xC7, 0x81, 0x24, 0x02, 0, 0]).raw(&RESUME.to_le_bytes()); // Eip = RESUME
    a.raw(&[0x31, 0xC0, 0xC3]); // xor eax, eax (ExceptionContinueExecution) ; ret

    let Some(mut m) = machine("seh", &a, true) else { return };
    m.store_initial_esp();
    m.run_to_done(20);
    assert_eq!(m.r32(DATA), 1, "execution continued where the handler pointed the context");
    assert_eq!(m.r32(DATA + 4), 0xC000_0005, "STATUS_ACCESS_VIOLATION");
    assert_eq!(m.r32(DATA + 8), fault_at, "at the faulting instruction");
    assert_eq!(m.r32(DATA + 12), 2);
    assert_eq!(m.r32(DATA + 16), 1, "a write");
    assert_eq!(m.r32(DATA + 20), 0x10, "of address 0x10");
    assert_eq!(m.r32(DATA + 0x30), 0, "the instruction after the fault was skipped");
    assert_eq!(m.r32(DATA + 0x20), 0x1234_5678, "the context's registers were restored");
    assert_eq!(
        m.r32(DATA + 0x28) + 8,
        m.r32(DATA + 0x24),
        "the registration on the stack was the frame"
    );
}

impl Machine {
    /// Nothing to do: kept so the SEH test reads in order (the program
    /// records its own stack pointer).
    fn store_initial_esp(&mut self) {}
}
