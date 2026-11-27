//! Tests of the translator: the snippets of [`crate::testing`], the
//! backend-neutral conformance cases of `xb-cpu-kvm` (run here, and compared
//! with the KVM backend when the host has it), and what only this backend
//! has (device operand sizes, the translation cache, native comparison).

use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::sync::Arc;

use iced_x86::code_asm::*;
use xb_cpu::{Access, Bus, Context, Cpu, Exit, FaultAction, Mapper, RunFlags, eflags, reg};
use xb_types::mem::PhysMem;

use crate::X64Cpu;
use crate::testing::{self as t, NoBus, Snippet};

/// Guest RAM: a memfd and the test's own view of it.
struct Ram {
    fd: OwnedFd,
    ptr: *mut u8,
    len: usize,
}

// SAFETY: a shared mapping; tests coordinate their accesses.
unsafe impl Send for Ram {}
// SAFETY: as above.
unsafe impl Sync for Ram {}

impl Ram {
    fn new(len: u32) -> Arc<Self> {
        // SAFETY: creating and mapping a fresh memory object.
        unsafe {
            let fd = libc::memfd_create(c"xb-cpu-x64-test".as_ptr(), libc::MFD_CLOEXEC);
            assert!(fd >= 0);
            assert_eq!(libc::ftruncate(fd, libc::off_t::from(len)), 0);
            let ptr = libc::mmap(
                std::ptr::null_mut(),
                len as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            );
            assert_ne!(ptr, libc::MAP_FAILED);
            Arc::new(Self { fd: OwnedFd::from_raw_fd(fd), ptr: ptr.cast(), len: len as usize })
        }
    }

    #[expect(clippy::mut_from_ref, reason = "test-only view of shared memory")]
    fn bytes(&self) -> &mut [u8] {
        // SAFETY: the mapping is `len` bytes; the tests are the only users.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl PhysMem for Ram {
    fn size(&self) -> u32 {
        self.len as u32
    }
    fn read(&self, pa: u32, buf: &mut [u8]) -> bool {
        let Some(s) = self.bytes().get(pa as usize..pa as usize + buf.len()) else { return false };
        buf.copy_from_slice(s);
        true
    }
    fn write(&self, pa: u32, data: &[u8]) -> bool {
        let Some(s) = self.bytes().get_mut(pa as usize..pa as usize + data.len()) else {
            return false;
        };
        s.copy_from_slice(data);
        true
    }
}

fn machine() -> (X64Cpu, Arc<Ram>) {
    let ram = Ram::new(t::RAM_SIZE);
    let mut cpu = X64Cpu::new(ram.fd.as_fd(), t::RAM_SIZE).expect("backend");
    t::map_machine(&mut cpu);
    (cpu, ram)
}

fn asm32(build: impl FnOnce(&mut CodeAssembler) -> Result<(), IcedError>) -> Snippet {
    Snippet::new("test", build)
}

/// A bus that records device and port traffic and answers from a table.
#[derive(Default)]
struct RecBus {
    log: Vec<(char, u32, u8, u32)>,
    read_value: u32,
    tsc: u64,
}

impl Bus for RecBus {
    fn mmio_read(&mut self, pa: u32, size: u8) -> u32 {
        self.log.push(('r', pa, size, 0));
        self.read_value
    }
    fn mmio_write(&mut self, pa: u32, size: u8, value: u32) {
        self.log.push(('w', pa, size, value));
    }
    fn io_read(&mut self, port: u16, size: u8) -> u32 {
        self.log.push(('i', u32::from(port), size, 0));
        self.read_value
    }
    fn io_write(&mut self, port: u16, size: u8, value: u32) {
        self.log.push(('o', u32::from(port), size, value));
    }
    fn page_fault(&mut self, _va: u32, _pa: Option<u32>, _write: bool) -> FaultAction {
        FaultAction::Raise
    }
    fn tsc(&mut self) -> u64 {
        self.tsc
    }
}

#[test]
fn every_snippet_ends_in_its_expected_state() {
    let (mut cpu, ram) = machine();
    for s in t::snippets() {
        s.run(&mut cpu, ram.bytes(), &mut NoBus).unwrap_or_else(|e| panic!("{e}"));
        // And again from the cache's point of view: same code, fresh load.
        s.run(&mut cpu, ram.bytes(), &mut NoBus).unwrap_or_else(|e| panic!("second run: {e}"));
    }
}

#[test]
fn snippets_run_when_the_kernel_sets_the_segment_base() {
    let ram = Ram::new(t::RAM_SIZE);
    let mut cpu = X64Cpu::with_segment_base(ram.fd.as_fd(), t::RAM_SIZE, false).expect("backend");
    t::map_machine(&mut cpu);
    for s in t::snippets() {
        s.run(&mut cpu, ram.bytes(), &mut NoBus).unwrap_or_else(|e| panic!("{e}"));
    }
}

#[test]
fn the_conformance_cases_pass_and_agree_with_kvm() {
    use xb_cpu_kvm::testing as k;
    let kvm = xb_cpu_kvm::KvmCpu::probe();
    if let Err(e) = &kvm {
        println!("KVM is not available ({e}): the cases run on the translator only");
    }
    let mut disagreements = Vec::new();
    for case in k::cases() {
        let ram = Ram::new(k::RAM_SIZE);
        let mut cpu = X64Cpu::new(ram.fd.as_fd(), k::RAM_SIZE).expect("backend");
        let mine = k::run_case(case, &mut cpu, ram.clone())
            .unwrap_or_else(|e| panic!("case {} on x64: {e}", case.name));
        if kvm.is_ok() {
            let ram = Ram::new(k::RAM_SIZE);
            let mut cpu =
                xb_cpu_kvm::KvmCpu::new(ram.fd.as_fd(), k::RAM_SIZE).expect("kvm backend");
            let theirs = k::run_case(case, &mut cpu, ram.clone())
                .unwrap_or_else(|e| panic!("case {} on kvm: {e}", case.name));
            if case.exact && mine != theirs {
                disagreements.push(format!("{}:\n  x64 {mine:x?}\n  kvm {theirs:x?}", case.name));
            }
        }
    }
    assert!(disagreements.is_empty(), "backends disagree:\n{}", disagreements.join("\n"));
}

#[test]
fn device_registers_are_read_and_written_in_every_operand_size() {
    const MMIO: u32 = 0xFD00_0000;
    let (mut cpu, ram) = machine();
    cpu.map_mmio(MMIO, MMIO, 0x1000);
    let s = asm32(|c| {
        c.mov(esi, MMIO)?;
        c.mov(al, byte_ptr(esi + 1))?;
        c.mov(cx, word_ptr(esi + 2))?;
        c.mov(edx, dword_ptr(esi + 4))?;
        c.movzx(edi, byte_ptr(esi + 8))?;
        c.mov(byte_ptr(esi + 0x10), 0x12)?;
        c.mov(word_ptr(esi + 0x12), cx)?;
        c.mov(dword_ptr(esi + 0x14), 0xDEAD_BEEFu32)?;
        c.or(dword_ptr(esi + 0x18), 0x0F00_0000)?; // read, modify, write
        c.test(dword_ptr(esi + 0x1C), 0x80)?; // read only
        c.setnz(bl)?;
        c.movq(mm0, qword_ptr(esi + 0x20))?; // two doublewords
        c.movq(qword_ptr(esi + 0x28), mm0)?;
        c.emms()?;
        c.push(dword_ptr(esi + 0x30))?;
        c.pop(ebp)
    });
    let mut bus = RecBus { read_value: 0x8899_AABB, ..RecBus::default() };
    let ctx = s.run(&mut cpu, ram.bytes(), &mut bus).unwrap();
    assert_eq!(ctx.regs.gpr[reg::EAX] & 0xFF, 0xBB);
    assert_eq!(ctx.regs.gpr[reg::ECX], 0xAABB);
    assert_eq!(ctx.regs.gpr[reg::EDX], 0x8899_AABB);
    assert_eq!(ctx.regs.gpr[reg::EDI], 0xBB);
    assert_eq!(ctx.regs.gpr[reg::EBX] & 0xFF, 1);
    assert_eq!(ctx.regs.gpr[reg::EBP], 0x8899_AABB);
    let v = 0x8899_AABB;
    assert_eq!(
        bus.log,
        vec![
            ('r', MMIO + 1, 1, 0),
            ('r', MMIO + 2, 2, 0),
            ('r', MMIO + 4, 4, 0),
            ('r', MMIO + 8, 1, 0),
            ('w', MMIO + 0x10, 1, 0x12),
            ('w', MMIO + 0x12, 2, 0xAABB),
            ('w', MMIO + 0x14, 4, 0xDEAD_BEEF),
            ('r', MMIO + 0x18, 4, 0),
            ('w', MMIO + 0x18, 4, v | 0x0F00_0000),
            ('r', MMIO + 0x1C, 4, 0),
            ('r', MMIO + 0x20, 4, 0),
            ('r', MMIO + 0x24, 4, 0),
            ('w', MMIO + 0x28, 4, v),
            ('w', MMIO + 0x2C, 4, v),
            ('r', MMIO + 0x30, 4, 0),
        ]
    );
    assert_eq!(cpu.stats().mmio, 12);
}

#[test]
fn port_io_rdtsc_and_cpuid_go_through_the_bus() {
    let (mut cpu, ram) = machine();
    let s = asm32(|c| {
        c.mov(edx, 0xCF8)?;
        c.mov(eax, 0x8000_0010u32)?;
        c.out(dx, eax)?;
        c.in_(al, 0x61)?;
        c.mov(edi, eax)?;
        c.rdtsc()?;
        c.mov(esi, eax)?;
        c.mov(ebp, edx)?;
        c.mov(eax, 1)?;
        c.cpuid()
    });
    let mut bus = RecBus { read_value: 0x77, tsc: 0x1234_5678_9ABC, ..RecBus::default() };
    let ctx = s.run(&mut cpu, ram.bytes(), &mut bus).unwrap();
    assert_eq!(bus.log, vec![('o', 0xCF8, 4, 0x8000_0010), ('i', 0x61, 1, 0)]);
    assert_eq!(ctx.regs.gpr[reg::EDI], 0x8000_0077);
    assert_eq!((ctx.regs.gpr[reg::EBP], ctx.regs.gpr[reg::ESI]), (0x1234, 0x5678_9ABC));
    let id = xb_cpu::cpuid(1);
    assert_eq!(
        [
            ctx.regs.gpr[reg::EAX],
            ctx.regs.gpr[reg::EBX],
            ctx.regs.gpr[reg::ECX],
            ctx.regs.gpr[reg::EDX]
        ],
        id
    );
}

#[test]
fn cli_sti_and_popfd_track_if_and_exit_when_interruptible() {
    let (mut cpu, ram) = machine();
    let s = asm32(|c| {
        c.cli()?;
        c.pushfd()?;
        c.pop(eax)?;
        c.inc(ecx)?;
        c.sti()?;
        c.inc(ecx)?;
        c.cli()?;
        c.push(0x202)?;
        c.popfd()?;
        c.inc(ecx)
    });
    s.load(&mut cpu, ram.bytes());
    let mut ctx = s.context();
    let wait = RunFlags { exit_when_interruptible: true };
    // Already interruptible: at once, nothing run.
    assert_eq!(cpu.run(&mut ctx, &mut NoBus, wait), Exit::Kicked);
    assert_eq!(ctx.regs.eip, t::CODE);
    ctx.regs.eflags &= !eflags::IF;
    assert_eq!(cpu.run(&mut ctx, &mut NoBus, wait), Exit::Kicked);
    assert_eq!(ctx.regs.gpr[reg::EAX], 0x002, "pushfd shows IF clear");
    assert_eq!(ctx.regs.gpr[reg::ECX], 1, "stopped right after sti");
    assert_ne!(ctx.regs.eflags & eflags::IF, 0);
    // Without the flag the sti passes; the kernel asks again once IF is off.
    ctx.regs.eflags &= !eflags::IF;
    ctx.regs.eip = t::CODE;
    ctx.regs.gpr[reg::ECX] = 0;
    assert_eq!(cpu.run(&mut ctx, &mut NoBus, RunFlags::default()), Exit::Trap);
    assert_eq!(ctx.regs.gpr[reg::ECX], 3);
    assert_ne!(ctx.regs.eflags & eflags::IF, 0);
    // popfd enabling interrupts stops too.
    ctx.regs.eip = t::CODE + 6; // the second cli
    ctx.regs.eflags &= !eflags::IF;
    let at = (0..40).map(|o| t::CODE + o).find(|&a| ram.bytes()[a as usize] == 0x9D).unwrap();
    assert_eq!(cpu.run(&mut ctx, &mut NoBus, wait), Exit::Kicked);
    assert_eq!(ctx.regs.eip, at + 1, "stopped right after popfd");
}

#[test]
fn x87_and_sse_state_belongs_to_the_context() {
    let (mut cpu, ram) = machine();
    // Leaves values in ST0, MM-free x87 and XMM3, then traps.
    let load = asm32(|c| {
        c.fld(qword_ptr(ebx))?;
        c.movups(xmm3, xmmword_ptr(ebx + 8))
    })
    .mem(t::DATA, &2.5f64.to_le_bytes())
    .mem(t::DATA + 8, &[0xA5; 16]);
    let mut a = load.run(&mut cpu, ram.bytes(), &mut NoBus).unwrap();
    assert_eq!(&a.fx.0[160 + 48..160 + 64], &[0xA5; 16], "XMM3 in the FXSAVE image");
    assert_eq!(a.fx.0[4], 0x80, "one x87 register in use");
    // Another context runs code that fills its own state.
    let other = asm32(|c| {
        c.fldz()?;
        c.fldpi()?;
        c.xorps(xmm3, xmm3)
    });
    let b = other.run(&mut cpu, ram.bytes(), &mut NoBus).unwrap();
    assert_eq!(&b.fx.0[160 + 48..160 + 64], &[0; 16]);
    // Back in the first: its ST0 and XMM3 are as it left them.
    let store = asm32(|c| {
        c.fstp(qword_ptr(ebx + 0x40))?;
        c.movups(xmmword_ptr(ebx + 0x48), xmm3)
    });
    store.load(&mut cpu, ram.bytes());
    a.regs.eip = t::CODE;
    assert_eq!(cpu.run(&mut a, &mut NoBus, RunFlags::default()), Exit::Trap);
    let d = t::DATA as usize;
    assert_eq!(&ram.bytes()[d + 0x40..d + 0x48], &2.5f64.to_le_bytes());
    assert_eq!(&ram.bytes()[d + 0x48..d + 0x58], &[0xA5; 16]);
    assert_eq!(a.fx.0[4], 0, "the x87 stack is empty again");
}

#[test]
fn modifying_an_instruction_that_already_ran_runs_the_new_one() {
    let (mut cpu, ram) = machine();
    // l: mov eax, 1 (the immediate becomes 2 after the first pass through
    // the contiguous window, another view of the same page); two passes.
    let s = asm32(|c| {
        let mut l = c.create_label();
        c.mov(ecx, 2)?;
        c.set_label(&mut l)?;
        c.mov(eax, 1)?;
        c.add(edx, eax)?;
        c.mov(byte_ptr(t::WINDOW + t::CODE + 6), 2)?;
        c.dec(ecx)?;
        c.jnz(l)
    });
    let ctx = s.run(&mut cpu, ram.bytes(), &mut NoBus).unwrap();
    assert_eq!(ctx.regs.gpr[reg::EDX], 3);
    assert!(cpu.stats().code_writes >= 1);
}

#[test]
fn a_page_of_data_beside_code_stops_being_watched() {
    let (mut cpu, ram) = machine();
    // A counter stored next to the loop that increments it.
    let s = asm32(|c| {
        let mut l = c.create_label();
        c.mov(ecx, 1000)?;
        c.set_label(&mut l)?;
        c.inc(dword_ptr(t::CODE + 0x800))?;
        c.dec(ecx)?;
        c.jnz(l)
    });
    ram.bytes()[t::CODE as usize + 0x800..][..4].fill(0);
    s.run(&mut cpu, ram.bytes(), &mut NoBus).unwrap();
    assert_eq!(ram.read_u32(t::CODE + 0x800), 1000);
    let writes = cpu.stats().code_writes;
    assert!((1..200).contains(&writes), "{writes} faulting writes: the page was given up on");
}

#[test]
fn the_cache_flushes_under_live_calls_and_goes_on() {
    let (mut cpu, ram) = machine();
    cpu.set_code_limit(0);
    // 6000 tiny functions called through a table of addresses, from inside
    // a call: more code than the reduced cache holds.
    let mut code = Vec::new();
    let funcs = t::CODE + 0x1000;
    let s = asm32(|c| {
        let mut body = c.create_label();
        let mut l = c.create_label();
        let mut end = c.create_label();
        c.call(body)?;
        c.jmp(end)?;
        c.set_label(&mut body)?;
        c.mov(esi, funcs)?;
        c.mov(ecx, 6000)?;
        c.set_label(&mut l)?;
        c.call(esi)?;
        c.add(esi, 16)?;
        c.dec(ecx)?;
        c.jnz(l)?;
        c.ret()?;
        c.set_label(&mut end)?;
        c.nop()
    });
    for _ in 0..6000 {
        code.extend_from_slice(&[0x40, 0x83, 0xC2, 0x02, 0x90, 0x90, 0xEB, 0x00, 0xC3]); // inc eax; add edx, 2; jmp +0; ret
        code.extend_from_slice(&[0x90; 7]);
    }
    ram.bytes()[funcs as usize..funcs as usize + code.len()].copy_from_slice(&code);
    let ctx = s.run(&mut cpu, ram.bytes(), &mut NoBus).unwrap();
    assert_eq!((ctx.regs.gpr[reg::EAX], ctx.regs.gpr[reg::EDX]), (6000, 12000));
    assert!(cpu.stats().flushes >= 1, "{:?}", cpu.stats());
}

#[test]
fn calls_never_returned_from_do_not_overflow_the_shadow_stack() {
    let (mut cpu, ram) = machine();
    // call $+5; pop eax, half a million times in one stretch of guest code.
    let s = asm32(|c| {
        let mut l = c.create_label();
        c.mov(ecx, 500_000)?;
        c.set_label(&mut l)?;
        c.db(&[0xE8, 0, 0, 0, 0])?;
        c.pop(eax)?;
        c.dec(ecx)?;
        c.jnz(l)
    });
    let ctx = s.run(&mut cpu, ram.bytes(), &mut NoBus).unwrap();
    assert_eq!(ctx.regs.gpr[reg::ESP], t::STACK_TOP);
    assert_eq!(ctx.regs.gpr[reg::EAX], t::CODE + 10);
}

#[test]
fn an_instruction_without_a_translation_is_fatal_with_its_bytes() {
    let (mut cpu, ram) = machine();
    // inc eax; then a far jump, which nothing here runs.
    let s = Snippet::bytes("far jump", vec![0x40, 0xEA, 0x00, 0x10, 0x00, 0x00, 0x08, 0x00]);
    s.load(&mut cpu, ram.bytes());
    let mut ctx = s.context();
    let Exit::Fatal(msg) = cpu.run(&mut ctx, &mut NoBus, RunFlags::default()) else {
        panic!("not fatal")
    };
    assert!(msg.contains("ea 00 10 00 00 08 00"), "{msg}");
    assert_eq!(ctx.regs.eip, t::CODE + 1);
    assert_eq!(ctx.regs.gpr[reg::EAX], 1);
}

#[test]
fn a_page_fault_reports_the_instruction_and_registers_then_retries() {
    const HOLE: u32 = 0x4000_0000;
    struct Faults(Vec<(u32, Option<u32>, bool)>);
    impl Bus for Faults {
        fn mmio_read(&mut self, _: u32, _: u8) -> u32 {
            0
        }
        fn mmio_write(&mut self, _: u32, _: u8, _: u32) {}
        fn io_read(&mut self, _: u16, _: u8) -> u32 {
            0
        }
        fn io_write(&mut self, _: u16, _: u8, _: u32) {}
        fn page_fault(&mut self, va: u32, pa: Option<u32>, write: bool) -> FaultAction {
            self.0.push((va, pa, write));
            FaultAction::Raise
        }
        fn tsc(&mut self) -> u64 {
            0
        }
    }
    let (mut cpu, ram) = machine();
    let s = asm32(|c| {
        c.mov(eax, 5)?;
        c.stc()?;
        c.push(eax)?;
        c.add(dword_ptr(esi + 0x10), eax)?; // faults; nothing changed
        c.pop(ecx)?;
        c.adc(eax, 0)
    })
    .reg(reg::ESI, HOLE);
    s.load(&mut cpu, ram.bytes());
    let mut ctx = s.context();
    let mut bus = Faults(Vec::new());
    let exit = cpu.run(&mut ctx, &mut bus, RunFlags::default());
    assert_eq!(exit, Exit::Exception { vector: 14, error_code: 2, address: HOLE + 0x10 });
    // A read-modify-write instruction faults as a write, as on the processor.
    assert_eq!(bus.0, vec![(HOLE + 0x10, None, true)]);
    assert_eq!(ctx.regs.eip, t::CODE + 7, "the add");
    assert_eq!(ctx.regs.gpr[reg::ESP], t::STACK_TOP - 4);
    assert_eq!(ctx.regs.gpr[reg::EAX], 5);
    assert_ne!(ctx.regs.eflags & eflags::CF, 0);
    // The kernel maps the page; the same context goes on.
    cpu.map(HOLE, 0x40_0000, 0x1000, Access::ReadWrite);
    ram.write_u32(0x40_0010, 100);
    assert_eq!(cpu.run(&mut ctx, &mut bus, RunFlags::default()), Exit::Trap);
    assert_eq!(ram.read_u32(0x40_0010), 105);
    assert_eq!(ctx.regs.gpr[reg::EAX], 5, "adc saw the add's carry, not the stc's");
    assert_eq!(ctx.regs.gpr[reg::ECX], 5);
}

/// Runs `code` (register-only instructions that mean the same in 64-bit
/// mode) on the host: EAX, ECX, EDX, EBX and the arithmetic flags in and out.
fn native(code: &[u8], regs: [u32; 4], flags: u32) -> ([u32; 4], u32) {
    let mut a = CodeAssembler::new(64).unwrap();
    // fn(state: *mut [u32; 5]) with state = eax, ecx, edx, ebx, flags
    a.push(rbx).unwrap();
    a.mov(r8, rdi).unwrap();
    a.mov(eax, dword_ptr(r8)).unwrap();
    a.mov(ecx, dword_ptr(r8 + 4)).unwrap();
    a.mov(edx, dword_ptr(r8 + 8)).unwrap();
    a.mov(ebx, dword_ptr(r8 + 12)).unwrap();
    a.mov(r9d, dword_ptr(r8 + 16)).unwrap();
    a.push(r9).unwrap();
    a.popfq().unwrap();
    a.db(code).unwrap();
    a.pushfq().unwrap();
    a.pop(r9).unwrap();
    a.mov(dword_ptr(r8), eax).unwrap();
    a.mov(dword_ptr(r8 + 4), ecx).unwrap();
    a.mov(dword_ptr(r8 + 8), edx).unwrap();
    a.mov(dword_ptr(r8 + 12), ebx).unwrap();
    a.mov(dword_ptr(r8 + 16), r9d).unwrap();
    a.pop(rbx).unwrap();
    a.ret().unwrap();
    let region = crate::host::ExecRegion::new(4096).unwrap();
    let bytes = a.assemble(region.ptr() as u64).unwrap();
    let mut state = [regs[0], regs[1], regs[2], regs[3], flags | 0x202];
    // SAFETY: the region holds the function just assembled for its address,
    // which follows the System V convention and touches only `state`.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), region.ptr(), bytes.len());
        let f: extern "sysv64" fn(*mut [u32; 5]) = std::mem::transmute(region.ptr());
        f(&mut state);
    }
    ([state[0], state[1], state[2], state[3]], state[4])
}

#[test]
fn register_arithmetic_matches_the_host_processor_flags_included() {
    const ARITH: u32 = 0x8D5;
    let forms: &[(&str, &[u8], u32)] = &[
        ("add", &[0x01, 0xC8], ARITH),
        ("adc", &[0x11, 0xD0], ARITH),
        ("sub", &[0x29, 0xCA], ARITH),
        ("sbb", &[0x19, 0xC1], ARITH),
        ("cmp", &[0x39, 0xD8], ARITH),
        ("neg", &[0xF7, 0xD8], ARITH),
        ("and/or/xor", &[0x21, 0xC8, 0x09, 0xD3, 0x31, 0xDA], ARITH),
        ("imul", &[0x0F, 0xAF, 0xC2], 0x801),
        ("imul imm", &[0x6B, 0xC9, 0x7B], 0x801),
        ("mul", &[0xF7, 0xE1], 0x801),
        ("shl cl", &[0xD3, 0xE0], 0x0C5),
        ("shr 1", &[0xD1, 0xE8], ARITH & !0x10),
        ("sar 3", &[0xC1, 0xF8, 0x03], 0x0C5),
        ("rol 7 / rcr 1", &[0xC1, 0xC0, 0x07, 0xD1, 0xDA], 0x001),
        ("shld / shrd", &[0x0F, 0xA4, 0xD0, 0x05, 0x0F, 0xAC, 0xC2, 0x07], 0x0C5),
        ("8-bit with high bytes", &[0x00, 0xE0, 0x10, 0xF5, 0x28, 0xDE, 0xFE, 0xC4], ARITH),
        ("16-bit", &[0x66, 0x01, 0xC8, 0x66, 0x29, 0xD3, 0x66, 0xF7, 0xDA], ARITH),
        ("bt family", &[0x0F, 0xA3, 0xC8, 0x0F, 0xAB, 0xCA, 0x0F, 0xBB, 0xC3], 0x001),
        ("lahf / sahf / cmc", &[0x9F, 0x80, 0xF4, 0xD5, 0x9E, 0xF5], ARITH & !0x800),
        ("setcc / cmov", &[0x39, 0xC8, 0x0F, 0x9C, 0xC2, 0x0F, 0x42, 0xD9], ARITH),
        ("xadd / bswap / cmpxchg", &[0x0F, 0xC1, 0xCA, 0x0F, 0xC8, 0x0F, 0xB1, 0xD9], ARITH),
    ];
    let (mut cpu, ram) = machine();
    let mut seed = 0x1234_5678_9ABC_DEF0u64;
    let mut rnd = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        // Mix in edge values.
        match seed % 7 {
            0 => 0,
            1 => u32::MAX,
            2 => 0x8000_0000,
            _ => (seed >> 16) as u32,
        }
    };
    for &(name, code, mask) in forms {
        for _ in 0..24 {
            let regs = [rnd(), rnd(), rnd(), rnd()];
            let flags = rnd() & ARITH;
            let (want, want_flags) = native(code, regs, flags);
            let mut s = Snippet::bytes("native comparison", code.to_vec());
            s.regs[..4].copy_from_slice(&regs);
            s.eflags = flags | 0x202;
            let ctx = s.run(&mut cpu, ram.bytes(), &mut NoBus).unwrap();
            assert_eq!(&ctx.regs.gpr[..4], &want, "{name} of {regs:x?} with flags {flags:x}");
            assert_eq!(ctx.regs.eflags & mask, want_flags & mask, "{name}: flags of {regs:x?}");
        }
    }
}

#[test]
fn a_trap_round_trip_is_cheap() {
    let (mut cpu, ram) = machine();
    // l: call [ebx] (an import: an address in the trap range); jmp l
    let s = asm32(|c| {
        let mut l = c.create_label();
        c.set_label(&mut l)?;
        c.call(dword_ptr(ebx))?;
        c.jmp(l)
    })
    .mem32(t::DATA, &[t::TRAP + 0x40]);
    s.load(&mut cpu, ram.bytes());
    let mut ctx = s.context();
    let rounds = 200_000u32;
    let go = |cpu: &mut X64Cpu, ctx: &mut Context| {
        assert_eq!(cpu.run(ctx, &mut NoBus, RunFlags::default()), Exit::Trap);
        assert_eq!(ctx.regs.eip, t::TRAP + 0x40);
        // What the kernel does: pop the return address, resume there.
        ctx.regs.eip = ram.read_u32(ctx.regs.gpr[reg::ESP]);
        ctx.regs.gpr[reg::ESP] += 4;
    };
    for _ in 0..1000 {
        go(&mut cpu, &mut ctx);
    }
    let start = std::time::Instant::now();
    for _ in 0..rounds {
        go(&mut cpu, &mut ctx);
    }
    let ns = start.elapsed().as_nanos() as f64 / f64::from(rounds);
    println!("trap round trip: {ns:.0} ns ({:?})", cpu.stats());
    assert!(ns < 20_000.0, "a trap round trip took {ns:.0} ns");
}
