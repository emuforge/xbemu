//! Guest-code cases for any [`Cpu`] backend.
//!
//! Nothing here knows about KVM: a case gets a freshly created backend and
//! the host's view of its RAM, builds a small address space through the
//! `Cpu` trait, runs hand-assembled code ([`snippets`]) and returns an
//! [`Outcome`]. A case checks what the `xb_cpu` contract promises and fails
//! with a message otherwise; cases marked [`Case::exact`] also produce an
//! outcome that is the same on every correct backend, so two backends can be
//! compared with `==`.
//!
//! What an exact outcome leaves out, because backends may legitimately
//! differ: the arithmetic flags an instruction leaves undefined (snippets end
//! with flag-defining instructions, and AF is masked), the x87 instruction
//! and data pointers, memory below the stack pointer (a hardware backend's
//! exception delivery writes a frame there), and the virtual address reported
//! for a write-tracking fault (only the physical page is compared).

pub mod snippets;

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use xb_cpu::{Bus, Context, Cpu, Exit, FaultAction, Regs, RunFlags, eflags, reg};
use xb_types::mem::{Access, PhysMem};

/// RAM the backend under test must be created with.
pub const RAM_SIZE: u32 = 0x0080_0000;
/// Where snippets are loaded and start; mapped one to one, like everything
/// in `[CODE_VA, LOW_END)`.
pub const CODE_VA: u32 = 0x0001_0000;
/// End of the one-to-one mapped low range.
pub const LOW_END: u32 = 0x0010_0000;
/// Scratch data in the low range.
pub const DATA_VA: u32 = 0x0002_0000;
/// Initial stack top; a fresh context's ESP is 16 below, pointing at
/// [`RETURN_VA`].
pub const STACK_TOP: u32 = 0x0008_0000;
/// All of RAM, read-write, at `CONTIG_VA + pa`.
pub const CONTIG_VA: u32 = 0x8000_0000;
/// The trap range.
pub const TRAP_VA: u32 = 0xFA00_0000;
/// Length of the trap range.
pub const TRAP_LEN: u32 = 0x0001_0000;
/// The return sentinel on a fresh context's stack: a snippet's final `RET`
/// ends the run with [`Exit::Trap`] here.
pub const RETURN_VA: u32 = TRAP_VA + 0xFF0;
/// Device registers: virtual and physical address are the same.
pub const MMIO_VA: u32 = 0xFD00_0000;
/// Length of the device range.
pub const MMIO_LEN: u32 = 0x2000;
/// A virtual page nothing maps at the start.
pub const UNMAPPED_VA: u32 = 0x4000_0000;
/// Two physical pages cases use for remapping and protection.
pub const PAGE_A: u32 = 0x0020_0000;
/// See [`PAGE_A`].
pub const PAGE_B: u32 = 0x0020_1000;

const PAGE: u32 = 0x1000;
/// The EFLAGS bits an [`Outcome`] keeps.
const COMPARED_FLAGS: u32 =
    eflags::CF | eflags::ZF | (1 << 2) | (1 << 7) | (1 << 11) | eflags::DF | eflags::IF;
/// How long a case waits for another thread before it gives up.
const PATIENCE: Duration = Duration::from_secs(5);

/// One call the backend made on the [`Bus`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[expect(missing_docs, reason = "the fields mirror the Bus method arguments")]
pub enum BusEvent {
    /// `Bus::mmio_read`.
    MmioRead { pa: u32, size: u8 },
    /// `Bus::mmio_write`.
    MmioWrite { pa: u32, size: u8, value: u32 },
    /// `Bus::io_read`.
    IoRead { port: u16, size: u8 },
    /// `Bus::io_write`.
    IoWrite { port: u16, size: u8, value: u32 },
    /// `Bus::page_fault` with no handler installed (answered `Raise`).
    PageFault { va: u32, pa: Option<u32>, write: bool },
    /// `Bus::rdmsr`.
    Rdmsr { msr: u32 },
    /// `Bus::wrmsr`.
    Wrmsr { msr: u32, value: u64 },
}

/// Faults seen by [`tracking_handler`]: `(physical page, write)`.
pub type FaultRecord = Arc<Mutex<Vec<(Option<u32>, bool)>>>;

/// A page-fault handler a case installs on the [`TestBus`].
pub type FaultHandler = Box<dyn FnMut(u32, Option<u32>, bool) -> FaultAction>;

/// A [`Bus`] that records every call and answers reads with values computed
/// from the address.
pub struct TestBus {
    /// The calls so far, in order.
    pub events: Vec<BusEvent>,
    /// Answers `page_fault` instead of the default (record, then `Raise`).
    pub on_fault: Option<FaultHandler>,
    started: Instant,
}

impl TestBus {
    fn new() -> Self {
        Self { events: Vec::new(), on_fault: None, started: Instant::now() }
    }
}

/// What a device register read returns.
#[must_use]
pub fn device_value(pa: u32, size: u8) -> u32 {
    mask(pa.wrapping_mul(0x9E37_79B1) ^ 0xA5C3_5A3C, size)
}

/// What a port read returns.
#[must_use]
pub fn port_value(port: u16, size: u8) -> u32 {
    mask(u32::from(port).wrapping_mul(0x0101_0101) ^ 0x1357_9BDF, size)
}

/// What `RDMSR` returns.
#[must_use]
pub fn msr_value(msr: u32) -> u64 {
    0x0123_4567_89AB_CDEF ^ u64::from(msr)
}

fn mask(v: u32, size: u8) -> u32 {
    if size >= 4 { v } else { v & ((1 << (u32::from(size) * 8)) - 1) }
}

impl Bus for TestBus {
    fn mmio_read(&mut self, pa: u32, size: u8) -> u32 {
        self.events.push(BusEvent::MmioRead { pa, size });
        device_value(pa, size)
    }

    fn mmio_write(&mut self, pa: u32, size: u8, value: u32) {
        self.events.push(BusEvent::MmioWrite { pa, size, value: mask(value, size) });
    }

    fn io_read(&mut self, port: u16, size: u8) -> u32 {
        self.events.push(BusEvent::IoRead { port, size });
        port_value(port, size)
    }

    fn io_write(&mut self, port: u16, size: u8, value: u32) {
        self.events.push(BusEvent::IoWrite { port, size, value: mask(value, size) });
    }

    fn page_fault(&mut self, va: u32, pa: Option<u32>, write: bool) -> FaultAction {
        if let Some(handler) = &mut self.on_fault {
            return handler(va, pa, write);
        }
        self.events.push(BusEvent::PageFault { va, pa, write });
        FaultAction::Raise
    }

    fn tsc(&mut self) -> u64 {
        (self.started.elapsed().as_nanos() * 733_333_333 / 1_000_000_000) as u64
    }

    fn rdmsr(&mut self, msr: u32) -> u64 {
        self.events.push(BusEvent::Rdmsr { msr });
        msr_value(msr)
    }

    fn wrmsr(&mut self, msr: u32, value: u64) {
        self.events.push(BusEvent::Wrmsr { msr, value });
    }
}

/// What a case observed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    /// Every [`Cpu::run`] result, in order.
    pub exits: Vec<Exit>,
    /// The final integer state, with EFLAGS reduced to CF, PF, ZF, SF, OF, DF
    /// and IF.
    pub regs: Regs,
    /// The bus calls, in order.
    pub events: Vec<BusEvent>,
    /// Case-specific observations (memory words, fault records).
    pub values: Vec<u32>,
}

/// A backend under test with the standard address space built.
pub struct Harness<'a> {
    /// The backend.
    pub cpu: &'a mut dyn Cpu,
    /// The host's view of the backend's RAM.
    pub ram: Arc<dyn PhysMem>,
    /// The bus every run uses.
    pub bus: TestBus,
    /// The result of every run so far.
    pub exits: Vec<Exit>,
}

impl<'a> Harness<'a> {
    /// Builds the address space described by this module's constants on a
    /// fresh backend with [`RAM_SIZE`] bytes of RAM.
    pub fn new(cpu: &'a mut dyn Cpu, ram: Arc<dyn PhysMem>) -> Self {
        cpu.map(CODE_VA, CODE_VA, LOW_END - CODE_VA, Access::ReadWrite);
        cpu.map(CONTIG_VA, 0, RAM_SIZE, Access::ReadWrite);
        cpu.map_mmio(MMIO_VA, MMIO_VA, MMIO_LEN);
        cpu.set_trap_range(TRAP_VA, TRAP_LEN);
        Self { cpu, ram, bus: TestBus::new(), exits: Vec::new() }
    }

    /// Places `code` at [`CODE_VA`].
    pub fn load(&mut self, code: &[u8]) {
        self.ram.write(CODE_VA, code);
        self.cpu.invalidate_code(CODE_VA, code.len() as u32);
    }

    /// A context about to run the loaded snippet: EIP at [`CODE_VA`], a stack
    /// holding [`RETURN_VA`], interrupts off.
    #[must_use]
    pub fn context(&self) -> Context {
        let mut ctx = Context::default();
        ctx.regs.eflags = eflags::FIXED;
        self.restart(&mut ctx);
        ctx
    }

    /// Points `ctx` at the start of the loaded snippet again, on a fresh
    /// stack, keeping its other registers.
    pub fn restart(&self, ctx: &mut Context) {
        ctx.regs.eip = CODE_VA;
        ctx.regs.gpr[reg::ESP] = STACK_TOP - 16;
        self.ram.write_u32(STACK_TOP - 16, RETURN_VA);
    }

    /// Runs `ctx` and records the exit.
    pub fn run(&mut self, ctx: &mut Context) -> Exit {
        self.run_with(ctx, RunFlags::default())
    }

    /// Runs `ctx` with `flags` and records the exit.
    pub fn run_with(&mut self, ctx: &mut Context, flags: RunFlags) -> Exit {
        let exit = self.cpu.run(ctx, &mut self.bus, flags);
        self.exits.push(exit.clone());
        exit
    }

    /// Runs `ctx` and requires the snippet to finish with its final `RET`.
    ///
    /// # Errors
    ///
    /// Any other exit, described.
    pub fn run_to_return(&mut self, ctx: &mut Context) -> Result<(), String> {
        let exit = self.run(ctx);
        ensure(exit == Exit::Trap && ctx.regs.eip == RETURN_VA, || {
            format!("expected the return trap, got {exit:?} at eip {:#010x}", ctx.regs.eip)
        })?;
        ensure(ctx.regs.gpr[reg::ESP] == STACK_TOP - 12, || {
            format!("esp is {:#010x} after the final ret", ctx.regs.gpr[reg::ESP])
        })
    }

    fn outcome(&mut self, ctx: &Context, values: Vec<u32>) -> Outcome {
        let mut regs = ctx.regs;
        regs.eflags &= COMPARED_FLAGS;
        Outcome {
            exits: std::mem::take(&mut self.exits),
            regs,
            events: std::mem::take(&mut self.bus.events),
            values,
        }
    }
}

fn ensure(ok: bool, msg: impl FnOnce() -> String) -> Result<(), String> {
    if ok { Ok(()) } else { Err(msg()) }
}

fn ensure_eq<T: PartialEq + std::fmt::Debug>(what: &str, got: T, want: T) -> Result<(), String> {
    ensure(got == want, || format!("{what}: got {got:x?}, expected {want:x?}"))
}

/// Waits until `cond` holds; `false` when [`PATIENCE`] runs out first.
fn wait_for(mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + PATIENCE;
    while !cond() {
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_micros(200));
    }
    true
}

/// One guest-code scenario.
pub struct Case {
    /// A short name.
    pub name: &'static str,
    /// Whether the outcome is the same on every correct backend. Cases that
    /// depend on timing (kicks, the TSC) or on where an asynchronous stop
    /// lands are not.
    pub exact: bool,
    /// Runs the case on a fresh harness.
    pub run: fn(&mut Harness<'_>) -> Result<Outcome, String>,
}

/// Runs `case` on a freshly created backend.
///
/// # Errors
///
/// What the backend did that the contract forbids.
pub fn run_case(case: &Case, cpu: &mut dyn Cpu, ram: Arc<dyn PhysMem>) -> Result<Outcome, String> {
    (case.run)(&mut Harness::new(cpu, ram))
}

/// Every case.
#[must_use]
pub fn cases() -> &'static [Case] {
    &[
        Case { name: "arithmetic_loop", exact: true, run: arithmetic_loop },
        Case { name: "kernel_call_through_the_trap_range", exact: true, run: kernel_call },
        Case { name: "fpu_state_follows_the_context", exact: true, run: fpu_switch },
        Case { name: "device_registers", exact: true, run: device_registers },
        Case { name: "rep_movs_into_device_registers", exact: true, run: device_rep_movs },
        Case { name: "rep_movs_from_device_registers", exact: true, run: device_rep_movs_in },
        Case { name: "port_io", exact: true, run: port_io },
        Case { name: "port_string_io", exact: true, run: port_string_io },
        Case { name: "page_fault_then_retry_after_mapping", exact: true, run: page_fault },
        Case { name: "write_to_a_read_only_mapping", exact: true, run: read_only_mapping },
        Case { name: "remap_and_unmap_take_effect", exact: true, run: remap },
        Case { name: "protector_closes_a_page", exact: true, run: protector_single_thread },
        Case { name: "protector_covers_stack_pushes", exact: true, run: protected_stack },
        Case { name: "protector_from_another_thread", exact: false, run: protector_threaded },
        Case { name: "kick_during_a_loop", exact: false, run: kick_running },
        Case { name: "kick_before_run", exact: true, run: kick_before_run },
        Case { name: "exit_when_interruptible", exact: false, run: interruptible },
        Case { name: "hlt", exact: true, run: halt },
        Case { name: "software_interrupts", exact: true, run: software_interrupts },
        Case { name: "divide_error_and_invalid_opcode", exact: true, run: faults },
        Case { name: "rdtsc_rate", exact: false, run: rdtsc_rate },
        Case { name: "fs_base", exact: true, run: fs_base },
        Case { name: "self_modifying_code", exact: true, run: self_modifying },
        Case { name: "cpuid", exact: true, run: cpuid },
        Case { name: "unknown_msrs", exact: true, run: msrs },
    ]
}

fn arithmetic_loop(h: &mut Harness<'_>) -> Result<Outcome, String> {
    h.load(snippets::ARITH);
    let mut ctx = h.context();
    h.run_to_return(&mut ctx)?;
    ensure_eq("eax", ctx.regs.gpr[reg::EAX], 5050 * 3 - 7)?;
    ensure_eq("edx", ctx.regs.gpr[reg::EDX], (5050 * 3) >> 4)?;
    ensure_eq("ecx", ctx.regs.gpr[reg::ECX], 0)?;
    Ok(h.outcome(&ctx, vec![]))
}

fn kernel_call(h: &mut Harness<'_>) -> Result<Outcome, String> {
    h.load(snippets::TRAP_CALL);
    let mut ctx = h.context();
    let exit = h.run(&mut ctx);
    ensure_eq("exit", exit, Exit::Trap)?;
    ensure_eq("eip", ctx.regs.eip, TRAP_VA + 0x123)?;
    let want = [
        0x1111_1111,
        0x2222_2222,
        0x3333_3333,
        0x4444_4444,
        STACK_TOP - 16 - 8,
        0x5555_5555,
        0x6666_6666,
        0x7777_7777,
    ];
    ensure_eq("registers at the trap", ctx.regs.gpr, want)?;
    let esp = ctx.regs.gpr[reg::ESP];
    let ret = h.ram.read_u32(esp);
    ensure(ret > CODE_VA && ret < CODE_VA + snippets::TRAP_CALL.len() as u32, || {
        format!("return address on the stack is {ret:#010x}")
    })?;
    ensure_eq("argument", h.ram.read_u32(esp + 4), 0x1234)?;
    // What the kernel does: set the result, pop the return address and the
    // argument, resume.
    ctx.regs.gpr[reg::EAX] = 0xCAFE;
    ctx.regs.gpr[reg::ESP] = esp + 8;
    ctx.regs.eip = ret;
    h.run_to_return(&mut ctx)?;
    ensure_eq("eax", ctx.regs.gpr[reg::EAX], 0xCAFF)?;
    Ok(h.outcome(&ctx, vec![ret]))
}

fn fpu_switch(h: &mut Harness<'_>) -> Result<Outcome, String> {
    const XMM3: usize = 160 + 3 * 16;
    const XMM5: usize = 160 + 5 * 16;
    let vec_a: [u8; 16] = std::array::from_fn(|i| i as u8 + 1);
    let vec_b: [u8; 16] = std::array::from_fn(|i| 0x81 + i as u8);
    let host_set: [u8; 16] = std::array::from_fn(|i| 0x40 ^ (i as u8 * 7));
    h.ram.write(DATA_VA, &1.5f64.to_le_bytes());
    h.ram.write(DATA_VA + 8, &vec_a);
    h.ram.write(DATA_VA + 0x40, &(-2.25f64).to_le_bytes());
    h.ram.write(DATA_VA + 0x48, &vec_b);

    h.load(snippets::FX_LOAD);
    let mut a = h.context();
    let mut b = h.context();
    a.regs.gpr[reg::ESI] = DATA_VA;
    b.regs.gpr[reg::ESI] = DATA_VA + 0x40;
    h.run_to_return(&mut a)?;
    h.run_to_return(&mut b)?;
    ensure_eq("context A's xmm3 after its run", &a.fx.0[XMM3..XMM3 + 16], &vec_a[..])?;
    ensure_eq("context B's xmm3 after its run", &b.fx.0[XMM3..XMM3 + 16], &vec_b[..])?;
    // The kernel may edit a stopped thread's state.
    a.fx.0[XMM5..XMM5 + 16].copy_from_slice(&host_set);

    h.load(snippets::FX_STORE);
    a.regs.gpr[reg::EDI] = DATA_VA + 0x100;
    b.regs.gpr[reg::EDI] = DATA_VA + 0x180;
    // B ran last: A's state must come back, then B's.
    h.restart(&mut a);
    h.run_to_return(&mut a)?;
    h.restart(&mut b);
    h.run_to_return(&mut b)?;

    let mut out_a = [0u8; 40];
    let mut out_b = [0u8; 40];
    h.ram.read(DATA_VA + 0x100, &mut out_a);
    h.ram.read(DATA_VA + 0x180, &mut out_b);
    ensure_eq("A's st0", &out_a[..8], &1.5f64.to_le_bytes()[..])?;
    ensure_eq("A's xmm3", &out_a[8..24], &vec_a[..])?;
    ensure_eq("A's xmm5 (set by the host)", &out_a[24..40], &host_set[..])?;
    ensure_eq("B's st0", &out_b[..8], &(-2.25f64).to_le_bytes()[..])?;
    ensure_eq("B's xmm3", &out_b[8..24], &vec_b[..])?;
    ensure_eq("B's xmm5", &out_b[24..40], &[0u8; 16][..])?;
    Ok(h.outcome(&b, vec![]))
}

fn device_registers(h: &mut Harness<'_>) -> Result<Outcome, String> {
    h.load(snippets::MMIO);
    let mut ctx = h.context();
    h.run_to_return(&mut ctx)?;
    let dword = device_value(MMIO_VA + 0x30, 4);
    let want = vec![
        BusEvent::MmioRead { pa: MMIO_VA + 0x10, size: 1 },
        BusEvent::MmioRead { pa: MMIO_VA + 0x20, size: 2 },
        BusEvent::MmioRead { pa: MMIO_VA + 0x30, size: 4 },
        BusEvent::MmioWrite { pa: MMIO_VA + 0x40, size: 1, value: 0x5A },
        BusEvent::MmioWrite { pa: MMIO_VA + 0x50, size: 2, value: 0xBEEF },
        BusEvent::MmioWrite { pa: MMIO_VA + 0x60, size: 4, value: 0x1234_5678 },
        BusEvent::MmioWrite { pa: MMIO_VA + 0x70, size: 4, value: dword },
    ];
    ensure_eq("bus calls", h.bus.events.clone(), want)?;
    ensure_eq("al", ctx.regs.gpr[reg::EAX] & 0xFF, device_value(MMIO_VA + 0x10, 1))?;
    ensure_eq("bx", ctx.regs.gpr[reg::EBX] & 0xFFFF, device_value(MMIO_VA + 0x20, 2))?;
    ensure_eq("ecx", ctx.regs.gpr[reg::ECX], dword)?;
    Ok(h.outcome(&ctx, vec![]))
}

fn device_rep_movs(h: &mut Harness<'_>) -> Result<Outcome, String> {
    let words = [0x0101_0101u32, 0x2222_2222, 0x3333_0003];
    for (i, w) in words.iter().enumerate() {
        h.ram.write_u32(DATA_VA + 4 * i as u32, *w);
    }
    h.load(snippets::MMIO_REP);
    let mut ctx = h.context();
    ctx.regs.gpr[reg::ESI] = DATA_VA;
    h.run_to_return(&mut ctx)?;
    let want: Vec<_> = (0..3)
        .map(|i| BusEvent::MmioWrite {
            pa: MMIO_VA + 0x100 + 4 * i,
            size: 4,
            value: words[i as usize],
        })
        .collect();
    ensure_eq("bus calls", h.bus.events.clone(), want)?;
    ensure_eq("ecx", ctx.regs.gpr[reg::ECX], 0)?;
    ensure_eq("edi", ctx.regs.gpr[reg::EDI], MMIO_VA + 0x10C)?;
    Ok(h.outcome(&ctx, vec![]))
}

fn device_rep_movs_in(h: &mut Harness<'_>) -> Result<Outcome, String> {
    h.load(snippets::MMIO_REP_IN);
    let mut ctx = h.context();
    ctx.regs.gpr[reg::EDI] = CONTIG_VA + PAGE_A + 0x20;
    h.run_to_return(&mut ctx)?;
    let pas = [MMIO_VA + 0x200, MMIO_VA + 0x204, MMIO_VA + 0x208];
    let want: Vec<_> = pas.iter().map(|&pa| BusEvent::MmioRead { pa, size: 4 }).collect();
    ensure_eq("bus calls", h.bus.events.clone(), want)?;
    let stored: Vec<u32> = (0..3).map(|i| h.ram.read_u32(PAGE_A + 0x20 + 4 * i)).collect();
    ensure_eq("stored words", stored.clone(), pas.iter().map(|&pa| device_value(pa, 4)).collect())?;
    Ok(h.outcome(&ctx, stored))
}

fn port_io(h: &mut Harness<'_>) -> Result<Outcome, String> {
    h.load(snippets::PORT_IO);
    let mut ctx = h.context();
    h.run_to_return(&mut ctx)?;
    let want = vec![
        BusEvent::IoWrite { port: 0x80, size: 1, value: 0x12 },
        BusEvent::IoWrite { port: 0xC000, size: 4, value: 0xAABB_CCDD },
        BusEvent::IoWrite { port: 0xC000, size: 2, value: 0xCCDD },
        BusEvent::IoRead { port: 0x60, size: 1 },
        BusEvent::IoRead { port: 0xC000, size: 2 },
        BusEvent::IoRead { port: 0xC000, size: 4 },
    ];
    ensure_eq("bus calls", h.bus.events.clone(), want)?;
    ensure_eq("bl", ctx.regs.gpr[reg::EBX] & 0xFF, port_value(0x60, 1))?;
    ensure_eq("cx", ctx.regs.gpr[reg::ECX] & 0xFFFF, port_value(0xC000, 2))?;
    ensure_eq("eax", ctx.regs.gpr[reg::EAX], port_value(0xC000, 4))?;
    Ok(h.outcome(&ctx, vec![]))
}

fn port_string_io(h: &mut Harness<'_>) -> Result<Outcome, String> {
    h.ram.write(DATA_VA, b"xbox");
    h.load(snippets::PORT_STRING);
    let mut ctx = h.context();
    ctx.regs.gpr[reg::ESI] = DATA_VA;
    h.run_to_return(&mut ctx)?;
    let want: Vec<_> = b"xbox"
        .iter()
        .map(|&b| BusEvent::IoWrite { port: 0x3F8, size: 1, value: u32::from(b) })
        .collect();
    ensure_eq("bus calls", h.bus.events.clone(), want)?;
    ensure_eq("esi", ctx.regs.gpr[reg::ESI], DATA_VA + 4)?;
    Ok(h.outcome(&ctx, vec![]))
}

fn page_fault(h: &mut Harness<'_>) -> Result<Outcome, String> {
    h.load(snippets::FAULT);
    let mut ctx = h.context();
    let exit = h.run(&mut ctx);
    // Error code 2: a supervisor write to a page that is not present.
    ensure_eq(
        "exit",
        exit,
        Exit::Exception { vector: 14, error_code: 2, address: UNMAPPED_VA + 4 },
    )?;
    ensure_eq("eip at the faulting store", ctx.regs.eip, CODE_VA + 5)?;
    ensure_eq("esp", ctx.regs.gpr[reg::ESP], STACK_TOP - 16)?;
    ensure_eq("eax", ctx.regs.gpr[reg::EAX], 0x1111)?;
    ensure_eq(
        "bus calls",
        h.bus.events.clone(),
        vec![BusEvent::PageFault { va: UNMAPPED_VA + 4, pa: None, write: true }],
    )?;
    h.cpu.map(UNMAPPED_VA, PAGE_A, PAGE, Access::ReadWrite);
    h.run_to_return(&mut ctx)?;
    ensure_eq("ebx", ctx.regs.gpr[reg::EBX], 0x1111)?;
    let stored = h.ram.read_u32(PAGE_A + 4);
    ensure_eq("stored word", stored, 0x1111)?;
    Ok(h.outcome(&ctx, vec![stored]))
}

fn read_only_mapping(h: &mut Harness<'_>) -> Result<Outcome, String> {
    h.ram.write_u32(PAGE_A + 8, 0x0BAD_F00D);
    h.cpu.map(UNMAPPED_VA, PAGE_A, PAGE, Access::Read);
    h.load(snippets::LOAD);
    let mut ctx = h.context();
    ctx.regs.gpr[reg::ESI] = UNMAPPED_VA + 8;
    h.run_to_return(&mut ctx)?;
    ensure_eq("read through the read-only mapping", ctx.regs.gpr[reg::EAX], 0x0BAD_F00D)?;

    h.load(snippets::STORE);
    h.restart(&mut ctx);
    ctx.regs.gpr[reg::EDI] = UNMAPPED_VA + 8;
    let exit = h.run(&mut ctx);
    // Error code 3: a supervisor write to a present page.
    ensure_eq(
        "exit",
        exit,
        Exit::Exception { vector: 14, error_code: 3, address: UNMAPPED_VA + 8 },
    )?;
    ensure_eq("eip", ctx.regs.eip, CODE_VA)?;
    ensure_eq(
        "bus calls",
        h.bus.events.clone(),
        vec![BusEvent::PageFault { va: UNMAPPED_VA + 8, pa: Some(PAGE_A + 8), write: true }],
    )?;
    ensure_eq("the word is untouched", h.ram.read_u32(PAGE_A + 8), 0x0BAD_F00D)?;
    // The handler may also resolve it: answer Retry after opening the page.
    // Mapping changes are not allowed inside the handler, so open it first.
    h.cpu.map(UNMAPPED_VA, PAGE_A, PAGE, Access::ReadWrite);
    h.run_to_return(&mut ctx)?;
    let stored = h.ram.read_u32(PAGE_A + 8);
    ensure_eq("stored word", stored, 0x2222)?;
    Ok(h.outcome(&ctx, vec![stored]))
}

fn remap(h: &mut Harness<'_>) -> Result<Outcome, String> {
    h.ram.write_u32(PAGE_A, 0xAAAA_0001);
    h.ram.write_u32(PAGE_B, 0xBBBB_0002);
    h.load(snippets::REMAP);
    let mut ctx = h.context();
    h.cpu.map(UNMAPPED_VA, PAGE_A, PAGE, Access::Read);
    ensure_eq("first exit", h.run(&mut ctx), Exit::Halt)?;
    h.cpu.map(UNMAPPED_VA, PAGE_B, PAGE, Access::Read);
    ensure_eq("second exit", h.run(&mut ctx), Exit::Halt)?;
    h.cpu.unmap(UNMAPPED_VA, PAGE);
    let exit = h.run(&mut ctx);
    ensure_eq("first read", ctx.regs.gpr[reg::EAX], 0xAAAA_0001)?;
    ensure_eq("read after remapping", ctx.regs.gpr[reg::EBX], 0xBBBB_0002)?;
    ensure_eq(
        "exit after unmapping",
        exit,
        Exit::Exception { vector: 14, error_code: 0, address: UNMAPPED_VA },
    )?;
    ensure_eq("eip", ctx.regs.eip, CODE_VA + 0xD)?;
    Ok(h.outcome(&ctx, vec![]))
}

/// Installs a fault handler that records write-tracking faults as
/// `(physical page, write)` and re-opens the page, like the tracker in
/// `xb-mem`. Returns the record.
pub fn tracking_handler(h: &mut Harness<'_>) -> FaultRecord {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let protector = h.cpu.protector();
    let log = Arc::clone(&seen);
    h.bus.on_fault = Some(Box::new(move |_va, pa, write| {
        log.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((pa.map(|p| p & !0xFFF), write));
        let Some(pa) = pa else { return FaultAction::Raise };
        protector.protect(pa & !0xFFF, PAGE, Access::ReadWrite);
        FaultAction::Retry
    }));
    seen
}

fn protector_single_thread(h: &mut Harness<'_>) -> Result<Outcome, String> {
    let seen = tracking_handler(h);
    let taken = |seen: &Arc<Mutex<Vec<_>>>| {
        std::mem::take(&mut *seen.lock().unwrap_or_else(std::sync::PoisonError::into_inner))
    };
    let protector = h.cpu.protector();
    h.ram.write_u32(PAGE_A + 0x10, 0x5EED_0001);

    // No access: a read faults; the host's view is unaffected.
    protector.protect(PAGE_A, PAGE, Access::None);
    ensure_eq("host view of a closed page", h.ram.read_u32(PAGE_A + 0x10), 0x5EED_0001)?;
    h.ram.write_u32(PAGE_A + 0x14, 0x5EED_0002);
    h.load(snippets::LOAD);
    let mut ctx = h.context();
    ctx.regs.gpr[reg::ESI] = CONTIG_VA + PAGE_A + 0x10;
    h.run_to_return(&mut ctx)?;
    ensure_eq("value read", ctx.regs.gpr[reg::EAX], 0x5EED_0001)?;
    ensure_eq("faults for a read of a closed page", taken(&seen), vec![(Some(PAGE_A), false)])?;

    // Read-only: reads pass, a write faults once.
    protector.protect(PAGE_A, PAGE, Access::Read);
    h.restart(&mut ctx);
    h.run_to_return(&mut ctx)?;
    ensure_eq("faults for a read of a read-only page", taken(&seen), vec![])?;
    h.load(snippets::STORE);
    h.restart(&mut ctx);
    ctx.regs.gpr[reg::EDI] = CONTIG_VA + PAGE_A + 0x10;
    h.run_to_return(&mut ctx)?;
    ensure_eq("faults for a write to a read-only page", taken(&seen), vec![(Some(PAGE_A), true)])?;
    ensure_eq("stored word", h.ram.read_u32(PAGE_A + 0x10), 0x2222)?;
    // Open again: no further faults.
    h.restart(&mut ctx);
    h.run_to_return(&mut ctx)?;
    ensure_eq("faults once the page is open", taken(&seen), vec![])?;

    // A handler that refuses turns the access into a page fault exception at
    // the faulting instruction.
    h.bus.on_fault = None;
    protector.protect(PAGE_B, PAGE, Access::Read);
    h.restart(&mut ctx);
    ctx.regs.gpr[reg::EDI] = CONTIG_VA + PAGE_B;
    let exit = h.run(&mut ctx);
    ensure(matches!(exit, Exit::Exception { vector: 14, .. }) && ctx.regs.eip == CODE_VA, || {
        format!("refused write: {exit:?} at eip {:#010x}", ctx.regs.eip)
    })?;
    h.exits.pop();
    h.bus.events.clear();
    protector.protect(PAGE_B, PAGE, Access::ReadWrite);
    h.run_to_return(&mut ctx)?;
    Ok(h.outcome(&ctx, vec![]))
}

fn protected_stack(h: &mut Harness<'_>) -> Result<Outcome, String> {
    let stack_page = STACK_TOP - PAGE;
    let seen = tracking_handler(h);
    h.load(snippets::TRAP_CALL);
    let mut ctx = h.context();
    h.cpu.protector().protect(stack_page, PAGE, Access::Read);
    ensure_eq("exit", h.run(&mut ctx), Exit::Trap)?;
    let faults = seen.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
    ensure_eq("faults for pushes onto a read-only page", faults, vec![(Some(stack_page), true)])?;
    let esp = ctx.regs.gpr[reg::ESP];
    ensure_eq("esp", esp, STACK_TOP - 16 - 8)?;
    ensure_eq("pushed argument", h.ram.read_u32(esp + 4), 0x1234)?;
    Ok(h.outcome(&ctx, vec![]))
}

fn protector_threaded(h: &mut Harness<'_>) -> Result<Outcome, String> {
    const ROUNDS: u32 = 20;
    const IDLE: u32 = 0;
    const WAITING: u32 = 1;
    const RELEASED: u32 = 2;
    let stage = Arc::new(AtomicU32::new(IDLE));
    let seen: FaultRecord = Arc::new(Mutex::new(Vec::new()));
    let protector = h.cpu.protector();
    {
        let (stage, seen, protector) =
            (Arc::clone(&stage), Arc::clone(&seen), Arc::clone(&protector));
        h.bus.on_fault = Some(Box::new(move |_va, pa, write| {
            seen.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((pa.map(|p| p & !0xFFF), write));
            // Hold the guest here until the other thread has watched the
            // counter stand still.
            stage.store(WAITING, Ordering::SeqCst);
            if !wait_for(|| stage.load(Ordering::SeqCst) == RELEASED) {
                return FaultAction::Raise;
            }
            stage.store(IDLE, Ordering::SeqCst);
            protector.protect(PAGE_A, PAGE, Access::ReadWrite);
            FaultAction::Retry
        }));
    }
    h.ram.write_u32(PAGE_A, 0);
    h.ram.write_u32(DATA_VA, 0);
    h.load(snippets::COUNT);
    let mut ctx = h.context();
    ctx.regs.gpr[reg::EDI] = CONTIG_VA + PAGE_A;
    ctx.regs.gpr[reg::ESI] = DATA_VA;

    let ram = Arc::clone(&h.ram);
    let other = std::thread::spawn(move || -> Result<(), String> {
        let counter = || ram.read_u32(PAGE_A);
        let result = (|| {
            let mut last = 0;
            for round in 0..ROUNDS {
                ensure(wait_for(|| counter().wrapping_sub(last) > 1000), || {
                    format!("round {round}: the guest is not counting")
                })?;
                protector.protect(PAGE_A, PAGE, Access::Read);
                // `protect` is synchronous: from here on no guest write lands.
                let at_protect = counter();
                std::thread::sleep(Duration::from_millis(3));
                last = counter();
                ensure(at_protect == last, || {
                    format!(
                        "round {round}: the guest wrote a protected page ({at_protect} then {last})"
                    )
                })?;
                ensure(wait_for(|| stage.load(Ordering::SeqCst) == WAITING), || {
                    format!("round {round}: the write never reached Bus::page_fault")
                })?;
                ensure_eq("counter while the handler holds the guest", counter(), last)?;
                stage.store(RELEASED, Ordering::SeqCst);
            }
            ensure(wait_for(|| counter().wrapping_sub(last) > 1000), || {
                "the guest did not continue after the handler opened the page".to_owned()
            })
        })();
        // Always let the guest out of its loop.
        stage.store(RELEASED, Ordering::SeqCst);
        ram.write_u32(DATA_VA, 1);
        result
    });
    let run = h.run_to_return(&mut ctx);
    other.join().map_err(|_| "the protecting thread panicked".to_owned())??;
    run?;
    let faults = seen.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
    ensure_eq("faults", faults, vec![(Some(PAGE_A), true); ROUNDS as usize])?;
    Ok(h.outcome(&ctx, vec![]))
}

fn kick_running(h: &mut Harness<'_>) -> Result<Outcome, String> {
    h.load(snippets::SPIN);
    let mut ctx = h.context();
    let kicker = h.cpu.kicker();
    let started = Instant::now();
    let other = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(30));
        kicker.kick();
    });
    let exit = h.run(&mut ctx);
    let took = started.elapsed();
    other.join().map_err(|_| "the kicking thread panicked".to_owned())?;
    ensure_eq("exit", exit, Exit::Kicked)?;
    ensure_eq("eip", ctx.regs.eip, CODE_VA)?;
    ensure(took >= Duration::from_millis(25) && took < Duration::from_millis(1000), || {
        format!("the kick took {took:?}")
    })?;
    // The kick was consumed: the next run goes on until it is kicked again.
    let kicker = h.cpu.kicker();
    let other = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(10));
        kicker.kick();
    });
    let started = Instant::now();
    let exit = h.run(&mut ctx);
    other.join().map_err(|_| "the kicking thread panicked".to_owned())?;
    ensure_eq("second exit", exit, Exit::Kicked)?;
    ensure(started.elapsed() >= Duration::from_millis(8), || {
        "the second run ended before its kick".to_owned()
    })?;
    Ok(h.outcome(&ctx, vec![]))
}

fn kick_before_run(h: &mut Harness<'_>) -> Result<Outcome, String> {
    h.load(snippets::ARITH);
    let mut ctx = h.context();
    let before = ctx.regs;
    h.cpu.kicker().kick();
    ensure_eq("exit", h.run(&mut ctx), Exit::Kicked)?;
    ensure_eq("registers after a remembered kick", ctx.regs, before)?;
    h.run_to_return(&mut ctx)?;
    Ok(h.outcome(&ctx, vec![]))
}

fn interruptible(h: &mut Harness<'_>) -> Result<Outcome, String> {
    let flags = RunFlags { exit_when_interruptible: true };
    h.load(snippets::STI);
    let mut ctx = h.context();
    let exit = h.run_with(&mut ctx, flags);
    ensure_eq("exit", exit, Exit::Kicked)?;
    ensure(ctx.regs.eflags & eflags::IF != 0, || "interrupts are still disabled".to_owned())?;
    ensure_eq("eax", ctx.regs.gpr[reg::EAX], 1)?;
    ensure(ctx.regs.eip >= CODE_VA + 6 && ctx.regs.eip <= CODE_VA + 7, || {
        format!("stopped at eip {:#010x}", ctx.regs.eip)
    })?;
    ensure(ctx.regs.gpr[reg::EBX] <= 1, || {
        format!("ran {} loop rounds with interrupts enabled", ctx.regs.gpr[reg::EBX])
    })?;
    // Already enabled: the run ends before anything executes.
    let before = ctx.regs;
    ensure_eq("exit with interrupts already enabled", h.run_with(&mut ctx, flags), Exit::Kicked)?;
    ensure_eq("registers", ctx.regs, before)?;
    Ok(h.outcome(&ctx, vec![]))
}

fn halt(h: &mut Harness<'_>) -> Result<Outcome, String> {
    h.load(snippets::HLT);
    let mut ctx = h.context();
    ensure_eq("exit", h.run(&mut ctx), Exit::Halt)?;
    ensure_eq("eip past the hlt", ctx.regs.eip, CODE_VA + 6)?;
    ensure_eq("eax", ctx.regs.gpr[reg::EAX], 7)?;
    h.run_to_return(&mut ctx)?;
    ensure_eq("eax", ctx.regs.gpr[reg::EAX], 9)?;
    Ok(h.outcome(&ctx, vec![]))
}

fn software_interrupts(h: &mut Harness<'_>) -> Result<Outcome, String> {
    h.load(snippets::INT_2E);
    let mut ctx = h.context();
    ensure_eq("exit", h.run(&mut ctx), Exit::SoftwareInterrupt(0x2E))?;
    ensure_eq("eip past the int", ctx.regs.eip, CODE_VA + 8)?;
    ensure_eq("esp", ctx.regs.gpr[reg::ESP], STACK_TOP - 16)?;
    ensure(ctx.regs.eflags & eflags::CF != 0, || "carry lost across int 0x2e".to_owned())?;
    h.run_to_return(&mut ctx)?;
    ensure_eq("eax (0x33 plus carry)", ctx.regs.gpr[reg::EAX], 0x34)?;

    h.load(snippets::INT3);
    h.restart(&mut ctx);
    ensure_eq("int3", h.run(&mut ctx), Exit::Exception { vector: 3, error_code: 0, address: 0 })?;
    ensure_eq("eip past the int3", ctx.regs.eip, CODE_VA + 1)?;
    ensure_eq("esp", ctx.regs.gpr[reg::ESP], STACK_TOP - 16)?;
    h.run_to_return(&mut ctx)?;

    h.load(snippets::INT_0D);
    h.restart(&mut ctx);
    ensure_eq("int 0x0d", h.run(&mut ctx), Exit::SoftwareInterrupt(0x0D))?;
    ensure_eq("eip past the int", ctx.regs.eip, CODE_VA + 2)?;
    ensure_eq("esp", ctx.regs.gpr[reg::ESP], STACK_TOP - 16)?;
    h.run_to_return(&mut ctx)?;
    Ok(h.outcome(&ctx, vec![]))
}

fn faults(h: &mut Harness<'_>) -> Result<Outcome, String> {
    h.load(snippets::DIVIDE);
    let mut ctx = h.context();
    ensure_eq("divide", h.run(&mut ctx), Exit::Exception { vector: 0, error_code: 0, address: 0 })?;
    ensure_eq("eip at the div", ctx.regs.eip, CODE_VA + 9)?;
    ensure_eq("esp", ctx.regs.gpr[reg::ESP], STACK_TOP - 16)?;
    ensure_eq("eax", ctx.regs.gpr[reg::EAX], 5)?;
    // The kernel's handler could fix the divisor and resume.
    ctx.regs.gpr[reg::ECX] = 5;
    h.run_to_return(&mut ctx)?;
    ensure_eq("quotient", ctx.regs.gpr[reg::EAX], 1)?;

    h.load(snippets::UD2);
    h.restart(&mut ctx);
    ensure_eq("ud2", h.run(&mut ctx), Exit::Exception { vector: 6, error_code: 0, address: 0 })?;
    ensure_eq("eip at the ud2", ctx.regs.eip, CODE_VA)?;
    ensure_eq("esp", ctx.regs.gpr[reg::ESP], STACK_TOP - 16)?;
    Ok(h.outcome(&ctx, vec![]))
}

fn rdtsc_rate(h: &mut Harness<'_>) -> Result<Outcome, String> {
    h.load(snippets::RDTSC);
    let mut ctx = h.context();
    let mut sample = |h: &mut Harness<'_>| -> Result<(u64, Instant), String> {
        h.restart(&mut ctx);
        h.run_to_return(&mut ctx)?;
        let now = Instant::now();
        Ok(((u64::from(ctx.regs.gpr[reg::EDX]) << 32) | u64::from(ctx.regs.gpr[reg::EAX]), now))
    };
    let (t0, i0) = sample(h)?;
    std::thread::sleep(Duration::from_millis(200));
    let (t1, i1) = sample(h)?;
    let hz = (t1.wrapping_sub(t0)) as f64 / (i1 - i0).as_secs_f64();
    ensure((hz / 733_333_333.0 - 1.0).abs() < 0.02, || {
        format!("RDTSC ticks at {:.3} MHz, expected 733.333", hz / 1e6)
    })?;
    Ok(h.outcome(&ctx, vec![]))
}

fn fs_base(h: &mut Harness<'_>) -> Result<Outcome, String> {
    h.ram.write_u32(PAGE_A + 0x28, 0x1000);
    h.ram.write_u32(PAGE_B + 0x28, 0x2000);
    h.load(snippets::FS);
    let mut a = h.context();
    let mut b = h.context();
    a.regs.fs_base = CONTIG_VA + PAGE_A;
    b.regs.fs_base = CONTIG_VA + PAGE_B;
    h.run_to_return(&mut a)?;
    h.restart(&mut b);
    h.run_to_return(&mut b)?;
    ensure_eq("eax through A's fs", a.regs.gpr[reg::EAX], 0x1001)?;
    ensure_eq("eax through B's fs", b.regs.gpr[reg::EAX], 0x2001)?;
    ensure_eq("A's fs base", a.regs.fs_base, CONTIG_VA + PAGE_A)?;
    let stored = vec![h.ram.read_u32(PAGE_A + 0x2C), h.ram.read_u32(PAGE_B + 0x2C)];
    ensure_eq("stored through fs", stored.clone(), vec![0x1001, 0x2001])?;
    Ok(h.outcome(&b, stored))
}

fn self_modifying(h: &mut Harness<'_>) -> Result<Outcome, String> {
    h.load(snippets::SMC);
    let mut ctx = h.context();
    h.run_to_return(&mut ctx)?;
    // First round adds the original immediate, second the patched one.
    ensure_eq("ebx", ctx.regs.gpr[reg::EBX], 0x101)?;
    Ok(h.outcome(&ctx, vec![]))
}

fn cpuid(h: &mut Harness<'_>) -> Result<Outcome, String> {
    h.load(snippets::CPUID);
    let mut ctx = h.context();
    h.run_to_return(&mut ctx)?;
    let [_, vendor, ..] = xb_cpu::cpuid(0);
    let [signature, ebx, ecx, features] = xb_cpu::cpuid(1);
    ensure_eq("vendor", ctx.regs.gpr[reg::ESI], vendor)?;
    ensure_eq("signature", ctx.regs.gpr[reg::EAX], signature)?;
    ensure_eq("features", ctx.regs.gpr[reg::EDX], features)?;
    ensure_eq("ebx", ctx.regs.gpr[reg::EBX], ebx)?;
    ensure_eq("ecx", ctx.regs.gpr[reg::ECX], ecx)?;
    Ok(h.outcome(&ctx, vec![]))
}

fn msrs(h: &mut Harness<'_>) -> Result<Outcome, String> {
    h.load(snippets::MSR);
    let mut ctx = h.context();
    h.run_to_return(&mut ctx)?;
    let read = msr_value(0xABC);
    ensure_eq("rdmsr low", ctx.regs.gpr[reg::ESI], read as u32)?;
    ensure_eq("rdmsr high", ctx.regs.gpr[reg::EDI], (read >> 32) as u32)?;
    ensure_eq(
        "bus calls",
        h.bus.events.clone(),
        vec![
            BusEvent::Rdmsr { msr: 0xABC },
            BusEvent::Wrmsr { msr: 0xABC, value: 0x2222_0000_1111 },
        ],
    )?;
    Ok(h.outcome(&ctx, vec![]))
}

/// Measures the cost of one kernel-call round trip: a `run` that enters the
/// guest at a trap address and comes straight back with [`Exit::Trap`].
///
/// # Errors
///
/// A run that did not end in the trap.
pub fn bench_trap_round_trip(h: &mut Harness<'_>, rounds: u32) -> Result<Duration, String> {
    let mut ctx = h.context();
    ctx.regs.eip = TRAP_VA + 0x40;
    let started = Instant::now();
    for _ in 0..rounds {
        let exit = h.cpu.run(&mut ctx, &mut h.bus, RunFlags::default());
        ensure(exit == Exit::Trap && ctx.regs.eip == TRAP_VA + 0x40, || format!("got {exit:?}"))?;
    }
    Ok(started.elapsed() / rounds)
}

/// Measures the cost of a round trip that also switches guest threads: two
/// contexts with different floating-point state run alternately.
///
/// # Errors
///
/// A run that did not end in the trap.
pub fn bench_thread_switch(h: &mut Harness<'_>, rounds: u32) -> Result<Duration, String> {
    let mut a = h.context();
    let mut b = h.context();
    a.regs.eip = TRAP_VA + 0x40;
    b.regs.eip = TRAP_VA + 0x80;
    b.fx.0[160] = 0x77;
    let started = Instant::now();
    for i in 0..rounds {
        let ctx = if i % 2 == 0 { &mut a } else { &mut b };
        let exit = h.cpu.run(ctx, &mut h.bus, RunFlags::default());
        ensure(exit == Exit::Trap, || format!("got {exit:?}"))?;
    }
    ensure(a.fx.0[160] == 0 && b.fx.0[160] == 0x77, || "contexts mixed up".to_owned())?;
    Ok(started.elapsed() / rounds)
}
