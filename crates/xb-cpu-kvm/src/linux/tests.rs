//! Tests that run real guest code. Each passes with a printed note when KVM
//! is unavailable.

use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use xb_cpu::{Context, Cpu, Exit, RunFlags, reg};
use xb_types::mem::{Access, PhysMem};

use super::KvmCpu;
use super::ram::Mmap;
use crate::testing::{
    self, CODE_VA, CONTIG_VA, Harness, PAGE_A, RAM_SIZE, RETURN_VA, STACK_TOP, snippets,
};

/// The host's view of the test RAM (what `xb-mem` provides in the machine).
struct TestRam(Mmap);

impl PhysMem for TestRam {
    fn size(&self) -> u32 {
        self.0.len() as u32
    }

    fn read(&self, pa: u32, buf: &mut [u8]) -> bool {
        if pa as usize + buf.len() > self.0.len() {
            return false;
        }
        // SAFETY: in bounds (checked above); guest RAM is shared plain bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(
                self.0.as_ptr().add(pa as usize),
                buf.as_mut_ptr(),
                buf.len(),
            );
        }
        true
    }

    fn write(&self, pa: u32, data: &[u8]) -> bool {
        if pa as usize + data.len() > self.0.len() {
            return false;
        }
        // SAFETY: as for `read`.
        unsafe {
            std::ptr::copy_nonoverlapping(
                data.as_ptr(),
                self.0.as_ptr().add(pa as usize),
                data.len(),
            );
        }
        true
    }
}

/// A fresh VM over a fresh memfd, or `None` (with a note) without KVM.
fn machine() -> Option<(KvmCpu, Arc<dyn PhysMem>)> {
    if let Err(e) = KvmCpu::probe() {
        println!("skipped: KVM is not usable here ({e})");
        return None;
    }
    // SAFETY: `memfd_create` with a valid C string; the descriptor it returns
    // is owned by nothing else.
    let fd = unsafe {
        let raw = libc::memfd_create(c"xb-cpu-kvm-test".as_ptr(), libc::MFD_CLOEXEC);
        assert!(raw >= 0, "memfd_create failed");
        OwnedFd::from_raw_fd(raw)
    };
    // SAFETY: `ftruncate` on a descriptor this function owns.
    let sized = unsafe { libc::ftruncate(fd.as_raw_fd(), i64::from(RAM_SIZE)) };
    assert_eq!(sized, 0, "ftruncate failed");
    let ram = TestRam(Mmap::shared(fd.as_fd(), RAM_SIZE as usize).expect("host view"));
    let cpu = KvmCpu::new(fd.as_fd(), RAM_SIZE).expect("KVM probe passed but the VM failed");
    Some((cpu, Arc::new(ram)))
}

#[test]
fn every_shared_case_passes_on_kvm() {
    let mut failures = Vec::new();
    for case in testing::cases() {
        let Some((mut cpu, ram)) = machine() else { return };
        match testing::run_case(case, &mut cpu, ram) {
            Ok(_) => println!("ok    {}", case.name),
            Err(e) => {
                println!("FAIL  {}: {e}", case.name);
                failures.push(case.name);
            }
        }
    }
    assert!(failures.is_empty(), "failed cases: {failures:?}");
}

#[test]
fn an_exact_case_gives_the_same_outcome_on_a_second_machine() {
    for case in testing::cases().iter().filter(|c| c.exact) {
        let Some((mut cpu, ram)) = machine() else { return };
        let first = testing::run_case(case, &mut cpu, ram);
        let Some((mut cpu, ram)) = machine() else { return };
        let second = testing::run_case(case, &mut cpu, ram);
        assert_eq!(first, second, "case {}", case.name);
    }
}

#[test]
fn the_backend_is_named_kvm() {
    let Some((cpu, _)) = machine() else { return };
    assert_eq!(cpu.name(), "kvm");
}

#[test]
fn a_fault_with_an_error_code_other_than_a_page_fault_is_unwound() {
    let Some((mut cpu, ram)) = machine() else { return };
    let mut h = Harness::new(&mut cpu, ram);
    h.load(snippets::BAD_SELECTOR);
    let mut ctx = h.context();
    // #GP with the selector's index as error code; the fault is at the
    // `mov ds, eax`, five bytes in.
    assert_eq!(h.run(&mut ctx), Exit::Exception { vector: 13, error_code: 0x30, address: 0 });
    assert_eq!(ctx.regs.eip, CODE_VA + 5);
    assert_eq!(ctx.regs.gpr[reg::ESP], STACK_TOP - 16);
}

#[test]
fn reloading_fs_keeps_the_base() {
    let Some((mut cpu, ram)) = machine() else { return };
    let mut h = Harness::new(&mut cpu, ram);
    h.ram.write_u32(PAGE_A + 0x28, 0x1234_5678);
    h.load(snippets::FS_RELOAD);
    let mut ctx = h.context();
    ctx.regs.fs_base = CONTIG_VA + PAGE_A;
    h.run_to_return(&mut ctx).unwrap();
    assert_eq!(ctx.regs.gpr[reg::EAX], 0x1234_5678);
}

#[test]
fn an_exception_on_an_unusable_stack_is_fatal_and_the_cpu_recovers() {
    let Some((mut cpu, ram)) = machine() else { return };
    let mut h = Harness::new(&mut cpu, ram);
    h.load(snippets::UD2);
    let mut ctx = h.context();
    ctx.regs.gpr[reg::ESP] = testing::UNMAPPED_VA + 0x100;
    ctx.regs.gpr[reg::EBX] = 0xB0B0_B0B0;
    let exit = h.run(&mut ctx);
    let Exit::Fatal(text) = &exit else { panic!("expected a fatal exit, got {exit:?}") };
    println!("double fault text: {text}");
    assert!(text.contains("double fault"), "{text}");
    assert!(text.contains(&format!("{:#010x}", testing::UNMAPPED_VA + 0x100)), "{text}");
    // The context shows where the guest died.
    assert_eq!(ctx.regs.eip, CODE_VA);
    assert_eq!(ctx.regs.gpr[reg::EBX], 0xB0B0_B0B0);

    // The vCPU is rebuilt: ordinary code, exceptions and FS still work.
    h.load(snippets::DIVIDE);
    let mut ctx = h.context();
    assert_eq!(h.run(&mut ctx), Exit::Exception { vector: 0, error_code: 0, address: 0 });
    h.load(snippets::ARITH);
    let mut ctx = h.context();
    h.run_to_return(&mut ctx).unwrap();
    assert_eq!(ctx.regs.gpr[reg::EAX], 5050 * 3 - 7);
    // And a second double fault is reported the same way.
    h.load(snippets::UD2);
    let mut ctx = h.context();
    ctx.regs.gpr[reg::ESP] = testing::UNMAPPED_VA + 0x100;
    assert!(matches!(h.run(&mut ctx), Exit::Fatal(_)));
}

#[test]
fn mapping_over_the_reserved_pages_is_refused_and_the_guest_still_runs() {
    let Some((mut cpu, ram)) = machine() else { return };
    let mut h = Harness::new(&mut cpu, ram);
    h.cpu.map(super::SYSTEM_VA - 0x1000, PAGE_A, 0x5000, Access::ReadWrite);
    h.cpu.unmap(super::SYSTEM_VA, super::SYSTEM_VA_LEN);
    h.load(snippets::DIVIDE);
    let mut ctx = h.context();
    assert_eq!(h.run(&mut ctx), Exit::Exception { vector: 0, error_code: 0, address: 0 });
}

#[test]
fn kicks_are_never_lost_or_duplicated_under_load() {
    let Some((mut cpu, ram)) = machine() else { return };
    let mut h = Harness::new(&mut cpu, ram);
    h.load(snippets::SPIN);
    let mut ctx = h.context();
    let kicker = h.cpu.kicker();
    let stop = Arc::new(AtomicBool::new(false));
    let kicks = Arc::new(std::sync::atomic::AtomicU32::new(0));
    // One kick per observed `Kicked`: the kicking thread waits for the CPU
    // thread to acknowledge before it sends the next, at random offsets into
    // and around `run`.
    let acked = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let other = {
        let (stop, kicks, acked) = (Arc::clone(&stop), Arc::clone(&kicks), Arc::clone(&acked));
        std::thread::spawn(move || {
            let mut spin = 1u32;
            while !stop.load(Ordering::SeqCst) {
                for _ in 0..spin % 2000 {
                    std::hint::spin_loop();
                }
                spin = spin.wrapping_mul(1_103_515_245).wrapping_add(12345);
                let n = kicks.fetch_add(1, Ordering::SeqCst) + 1;
                kicker.kick();
                while acked.load(Ordering::SeqCst) < n && !stop.load(Ordering::SeqCst) {
                    std::hint::spin_loop();
                }
            }
        })
    };
    for _ in 0..20_000 {
        // Every run must end with exactly the kick that was sent for it.
        assert_eq!(h.cpu.run(&mut ctx, &mut h.bus, RunFlags::default()), Exit::Kicked);
        assert_eq!(ctx.regs.eip, CODE_VA);
        acked.fetch_add(1, Ordering::SeqCst);
    }
    stop.store(true, Ordering::SeqCst);
    other.join().unwrap();
    let (sent, seen) = (kicks.load(Ordering::SeqCst), acked.load(Ordering::SeqCst));
    assert!(sent == seen || sent == seen + 1, "sent {sent} kicks, saw {seen}");
}

#[test]
fn a_kick_from_a_bus_handler_ends_the_run_after_the_access() {
    struct KickingBus(xb_cpu::Kicker, u32);
    impl xb_cpu::Bus for KickingBus {
        fn mmio_read(&mut self, _: u32, _: u8) -> u32 {
            0
        }
        fn mmio_write(&mut self, _: u32, _: u8, _: u32) {}
        fn io_read(&mut self, _: u16, _: u8) -> u32 {
            0
        }
        fn io_write(&mut self, _: u16, _: u8, _: u32) {
            self.1 += 1;
            if self.1 == 1 {
                self.0.kick();
            }
        }
        fn page_fault(&mut self, _: u32, _: Option<u32>, _: bool) -> xb_cpu::FaultAction {
            xb_cpu::FaultAction::Raise
        }
        fn tsc(&mut self) -> u64 {
            0
        }
    }
    let Some((mut cpu, ram)) = machine() else { return };
    let mut h = Harness::new(&mut cpu, ram);
    h.load(snippets::PORT_IO);
    let mut ctx = h.context();
    let mut bus = KickingBus(h.cpu.kicker(), 0);
    // A device raised an interrupt from its register handler: the `out`
    // completes, then the run ends.
    assert_eq!(h.cpu.run(&mut ctx, &mut bus, RunFlags::default()), Exit::Kicked);
    assert_eq!(ctx.regs.eip, CODE_VA + 4, "stopped right after the first out");
    assert_eq!(h.cpu.run(&mut ctx, &mut bus, RunFlags::default()), Exit::Trap);
    assert_eq!(ctx.regs.eip, RETURN_VA);
    assert_eq!(bus.1, 3);
}

#[test]
fn protect_is_cheap_and_faults_cost_what_is_reported() {
    let Some((mut cpu, ram)) = machine() else { return };
    let mut h = Harness::new(&mut cpu, ram);
    let protector = h.cpu.protector();
    let rounds = 20_000u32;
    let started = std::time::Instant::now();
    for _ in 0..rounds {
        protector.protect(PAGE_A, 0x1000, Access::Read);
        protector.protect(PAGE_A, 0x1000, Access::ReadWrite);
    }
    println!(
        "protect + unprotect of one page, guest stopped: {:?} per pair",
        started.elapsed() / rounds
    );
    let started = std::time::Instant::now();
    for _ in 0..rounds {
        protector.protect(PAGE_A, 0x1000, Access::ReadWrite);
    }
    println!("protect with nothing to change: {:?}", started.elapsed() / rounds);

    // A full write-tracking fault: protect, guest write, handler, retry.
    let reopen = Arc::clone(&protector);
    h.bus.on_fault = Some(Box::new(move |_, pa, _| {
        reopen.protect(pa.unwrap() & !0xFFF, 0x1000, Access::ReadWrite);
        xb_cpu::FaultAction::Retry
    }));
    h.load(snippets::STORE);
    let mut ctx = h.context();
    ctx.regs.gpr[reg::EDI] = CONTIG_VA + PAGE_A;
    h.run_to_return(&mut ctx).unwrap();
    let plain = {
        let started = std::time::Instant::now();
        for _ in 0..2000 {
            h.restart(&mut ctx);
            h.run_to_return(&mut ctx).unwrap();
        }
        started.elapsed() / 2000
    };
    let started = std::time::Instant::now();
    for _ in 0..2000 {
        protector.protect(PAGE_A, 0x1000, Access::Read);
        h.restart(&mut ctx);
        h.run_to_return(&mut ctx).unwrap();
    }
    let tracked = started.elapsed() / 2000;
    println!("run of a one-store snippet: {plain:?}; with a write-tracking fault: {tracked:?}");

    // `protect` from another thread while the guest runs: each call that
    // takes access away interrupts the vCPU to flush its TLB.
    h.bus.on_fault = None;
    h.ram.write_u32(testing::DATA_VA, 0);
    h.load(snippets::COUNT);
    let mut ctx = h.context();
    ctx.regs.gpr[reg::EDI] = CONTIG_VA + PAGE_A;
    ctx.regs.gpr[reg::ESI] = testing::DATA_VA;
    let ram = Arc::clone(&h.ram);
    let other = std::thread::spawn(move || {
        while ram.read_u32(PAGE_A) < 1000 {
            std::hint::spin_loop();
        }
        let started = std::time::Instant::now();
        for _ in 0..rounds {
            protector.protect(testing::PAGE_B, 0x1000, Access::Read);
            protector.protect(testing::PAGE_B, 0x1000, Access::ReadWrite);
        }
        let per_pair = started.elapsed() / rounds;
        ram.write_u32(testing::DATA_VA, 1);
        per_pair
    });
    h.run_to_return(&mut ctx).unwrap();
    println!(
        "protect + unprotect of one page, guest running: {:?} per pair",
        other.join().unwrap()
    );
}

#[test]
fn report_the_cost_of_a_trap_exit_and_of_a_thread_switch() {
    let Some((mut cpu, ram)) = machine() else { return };
    let mut h = Harness::new(&mut cpu, ram);
    // Warm up, then measure.
    testing::bench_trap_round_trip(&mut h, 2000).unwrap();
    let trap = testing::bench_trap_round_trip(&mut h, 200_000).unwrap();
    let switch = testing::bench_thread_switch(&mut h, 200_000).unwrap();
    println!("trap round trip: {} ns", trap.as_nanos());
    println!("trap round trip with a thread switch: {} ns", switch.as_nanos());
    assert!(trap < Duration::from_micros(200), "a trap round trip took {trap:?}");
}

#[test]
fn run_leaves_a_context_it_did_not_enter_untouched() {
    let Some((mut cpu, ram)) = machine() else { return };
    let mut h = Harness::new(&mut cpu, ram);
    h.load(snippets::SPIN);
    let mut ctx: Context = h.context();
    ctx.fx.0[200] = 0x5A;
    let before = (ctx.regs, ctx.fx.0);
    h.cpu.kicker().kick();
    assert_eq!(h.run(&mut ctx), Exit::Kicked);
    assert!(before == (ctx.regs, ctx.fx.0));
}

fn faults_of(seen: &testing::FaultRecord) -> Vec<(Option<u32>, bool)> {
    std::mem::take(&mut *seen.lock().unwrap())
}

#[test]
fn an_exception_frame_can_be_pushed_onto_a_protected_stack_page() {
    let Some((mut cpu, ram)) = machine() else { return };
    let mut h = Harness::new(&mut cpu, ram);
    let seen = testing::tracking_handler(&mut h);
    let stack_page = STACK_TOP - 0x1000;
    h.load(snippets::DIVIDE);
    let mut ctx = h.context();
    h.cpu.protector().protect(stack_page, 0x1000, Access::Read);
    // The processor's own push of the frame faults in KVM, is resolved
    // through the bus and the delivery is repeated.
    assert_eq!(h.run(&mut ctx), Exit::Exception { vector: 0, error_code: 0, address: 0 });
    assert_eq!(ctx.regs.eip, CODE_VA + 9);
    assert_eq!(ctx.regs.gpr[reg::ESP], STACK_TOP - 16);
    assert_eq!(faults_of(&seen), [(Some(stack_page), true)]);
}

#[test]
fn an_emulated_instruction_reaches_the_fault_handler_for_protected_ram() {
    let Some((mut cpu, ram)) = machine() else { return };
    let mut h = Harness::new(&mut cpu, ram);
    let seen = testing::tracking_handler(&mut h);
    let protector = h.cpu.protector();

    // Device to RAM: KVM's emulator performs the store.
    protector.protect(PAGE_A, 0x1000, Access::Read);
    h.load(snippets::MMIO_REP_IN);
    let mut ctx = h.context();
    ctx.regs.gpr[reg::EDI] = CONTIG_VA + PAGE_A + 0x20;
    h.run_to_return(&mut ctx).unwrap();
    assert_eq!(faults_of(&seen), [(Some(PAGE_A), true)]);
    for i in 0..3 {
        let want = testing::device_value(testing::MMIO_VA + 0x200 + 4 * i, 4);
        assert_eq!(h.ram.read_u32(PAGE_A + 0x20 + 4 * i), want);
    }

    // RAM to device: the emulator performs the load.
    h.ram.write_u32(PAGE_A + 0x40, 0x0A0B_0C0D);
    protector.protect(PAGE_A, 0x1000, Access::None);
    h.bus.events.clear();
    h.load(snippets::MMIO_REP);
    h.restart(&mut ctx);
    ctx.regs.gpr[reg::ESI] = CONTIG_VA + PAGE_A + 0x40;
    h.run_to_return(&mut ctx).unwrap();
    assert_eq!(faults_of(&seen), [(Some(PAGE_A), false)]);
    assert_eq!(
        h.bus.events[0],
        testing::BusEvent::MmioWrite { pa: testing::MMIO_VA + 0x100, size: 4, value: 0x0A0B_0C0D }
    );
}

#[test]
fn a_protected_page_the_decoder_cannot_name_is_still_reported() {
    let Some((mut cpu, ram)) = machine() else { return };
    let mut h = Harness::new(&mut cpu, ram);
    let seen = testing::tracking_handler(&mut h);
    let protector = h.cpu.protector();
    // `enter 8, 2` reads [ebp - 4], an operand the decoder does not model.
    // A second protected page stands for the rest of the tracker's set.
    protector.protect(PAGE_A, 0x1000, Access::None);
    protector.protect(testing::PAGE_B, 0x1000, Access::Read);
    h.load(snippets::ENTER_NESTED);
    let mut ctx = h.context();
    ctx.regs.gpr[reg::EBP] = CONTIG_VA + PAGE_A + 0x100;
    h.run_to_return(&mut ctx).unwrap();
    assert_eq!(ctx.regs.gpr[reg::EBP], CONTIG_VA + PAGE_A + 0x100);
    // Every protected page was reported: more than was touched, never less.
    assert_eq!(faults_of(&seen), [(Some(PAGE_A), false), (Some(testing::PAGE_B), true)]);
}

#[test]
fn a_cpu_created_on_one_thread_runs_on_another() {
    let Some((mut cpu, ram)) = machine() else { return };
    let cpu = std::thread::spawn(move || {
        let mut h = Harness::new(&mut cpu, ram);
        h.load(snippets::ARITH);
        let mut ctx = h.context();
        h.run_to_return(&mut ctx).unwrap();
        // A kick aimed at this thread's run must not outlive the thread.
        h.cpu.kicker().kick();
        cpu
    })
    .join()
    .unwrap();
    let mut cpu = cpu;
    let mut ctx = Context::default();
    let mut bus = TestBusStub;
    assert_eq!(cpu.run(&mut ctx, &mut bus, RunFlags::default()), Exit::Kicked);
    ctx.regs.eip = testing::TRAP_VA + 8;
    assert_eq!(cpu.run(&mut ctx, &mut bus, RunFlags::default()), Exit::Trap);
}

/// A bus for runs that must not reach it.
struct TestBusStub;

impl xb_cpu::Bus for TestBusStub {
    fn mmio_read(&mut self, pa: u32, _: u8) -> u32 {
        panic!("unexpected device read at {pa:#x}")
    }
    fn mmio_write(&mut self, pa: u32, _: u8, _: u32) {
        panic!("unexpected device write at {pa:#x}")
    }
    fn io_read(&mut self, port: u16, _: u8) -> u32 {
        panic!("unexpected port read {port:#x}")
    }
    fn io_write(&mut self, port: u16, _: u8, _: u32) {
        panic!("unexpected port write {port:#x}")
    }
    fn page_fault(&mut self, _: u32, _: Option<u32>, _: bool) -> xb_cpu::FaultAction {
        xb_cpu::FaultAction::Raise
    }
    fn tsc(&mut self) -> u64 {
        0
    }
}

#[test]
fn a_triple_fault_is_fatal_and_the_cpu_recovers() {
    let Some((mut cpu, ram)) = machine() else { return };
    let mut h = Harness::new(&mut cpu, ram);
    // An empty IDT: #UD cannot be delivered, nor can the double fault.
    h.ram.write(testing::DATA_VA, &[0; 6]);
    h.load(snippets::LIDT_UD2);
    let mut ctx = h.context();
    ctx.regs.gpr[reg::ESI] = testing::DATA_VA;
    let before = ctx.regs;
    let exit = h.run(&mut ctx);
    let Exit::Fatal(text) = &exit else { panic!("expected a fatal exit, got {exit:?}") };
    println!("triple fault text: {text}");
    assert!(text.contains("triple fault"), "{text}");
    assert!(ctx.regs == before, "the context keeps its state on entry");

    // The environment (including the IDT) is rebuilt for the next run.
    h.load(snippets::DIVIDE);
    let mut ctx = h.context();
    assert_eq!(h.run(&mut ctx), Exit::Exception { vector: 0, error_code: 0, address: 0 });
    assert_eq!(ctx.regs.eip, CODE_VA + 9);
}
