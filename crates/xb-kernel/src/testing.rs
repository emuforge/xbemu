//! Test doubles: a scripted CPU and a kernel over it.

use std::collections::VecDeque;
use std::sync::Arc;

use xb_cpu::{Bus, Context, Cpu, Exit, FaultAction, Kicker, RunFlags, reg};
use xb_mem::{Access, Mapper, PageProtector, Ram};
use xb_types::irq::IrqHub;

use crate::exports::{self, Abi};
use crate::kernel::{Kernel, KernelConfig, TRAP_VA};
use crate::platform::{DisplayMode, Platform};

/// A mapping call the kernel made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MapCall {
    Map(u32, u32, u32, Access),
    Mmio(u32, u32, u32),
    Unmap(u32, u32),
}

struct NoProtect;

impl PageProtector for NoProtect {
    fn protect(&self, _pa: u32, _len: u32, _access: Access) {}
}

/// A CPU that records mapper calls and returns scripted exits from `run`.
#[derive(Default)]
pub(crate) struct MockCpu {
    pub maps: Vec<MapCall>,
    pub trap: (u32, u32),
    pub invalidated: Vec<(u32, u32)>,
    /// What `run` does next: edits the context and returns the exit.
    #[allow(clippy::type_complexity, reason = "a test script")]
    pub script: VecDeque<Box<dyn FnMut(&mut Context) -> Exit + Send>>,
    pub runs: u32,
}

impl Mapper for MockCpu {
    fn map(&mut self, va: u32, pa: u32, len: u32, access: Access) {
        self.maps.push(MapCall::Map(va, pa, len, access));
    }

    fn map_mmio(&mut self, va: u32, pa: u32, len: u32) {
        self.maps.push(MapCall::Mmio(va, pa, len));
    }

    fn unmap(&mut self, va: u32, len: u32) {
        self.maps.push(MapCall::Unmap(va, len));
    }
}

impl Cpu for MockCpu {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn run(&mut self, ctx: &mut Context, _bus: &mut dyn Bus, _flags: RunFlags) -> Exit {
        self.runs += 1;
        match self.script.pop_front() {
            Some(mut f) => f(ctx),
            None => Exit::Fatal("the test's script ran out".into()),
        }
    }

    fn kicker(&self) -> Kicker {
        Kicker::new(|| {})
    }

    fn protector(&self) -> Arc<dyn PageProtector> {
        Arc::new(NoProtect)
    }

    fn set_trap_range(&mut self, va: u32, len: u32) {
        self.trap = (va, len);
    }

    fn invalidate_code(&mut self, pa: u32, len: u32) {
        self.invalidated.push((pa, len));
    }
}

/// A bus with no devices.
#[derive(Default)]
pub(crate) struct NullBus;

impl Bus for NullBus {
    fn mmio_read(&mut self, _pa: u32, _size: u8) -> u32 {
        0
    }
    fn mmio_write(&mut self, _pa: u32, _size: u8, _value: u32) {}
    fn io_read(&mut self, _port: u16, _size: u8) -> u32 {
        0
    }
    fn io_write(&mut self, _port: u16, _size: u8, _value: u32) {}
    fn page_fault(&mut self, _va: u32, _pa: Option<u32>, _write: bool) -> FaultAction {
        FaultAction::Raise
    }
    fn tsc(&mut self) -> u64 {
        0
    }
}

/// A platform that records what the kernel asked of it.
#[derive(Default)]
pub(crate) struct TestPlatform {
    pub modes: Vec<DisplayMode>,
    pub widescreen: Vec<bool>,
    pub link_state: u32,
    pub phy_inits: u32,
    pub apu_waits: Vec<u32>,
    pub freed: Vec<(u32, u32)>,
    pub ticks: u32,
}

impl Platform for TestPlatform {
    fn tick(&mut self, _now_ns: u64) -> Option<u64> {
        self.ticks += 1;
        None
    }
    fn set_display_mode(&mut self, mode: DisplayMode) {
        self.modes.push(mode);
    }
    fn set_title_widescreen(&mut self, wide: bool) {
        self.widescreen.push(wide);
    }
    fn phy_link_state(&mut self) -> u32 {
        self.link_state
    }
    fn phy_initialize(&mut self) {
        self.phy_inits += 1;
    }
    fn apu_mailbox_wait(&mut self, word_pa: u32) {
        self.apu_waits.push(word_pa);
    }
    fn ram_freed(&mut self, pa: u32, len: u32) {
        self.freed.push((pa, len));
    }
}

/// Where [`TestKernel::alloc`] hands out scratch guest memory.
const SCRATCH_VA: u32 = 0x0100_0000;
/// The return address [`TestKernel::call`] gives its calls.
pub(crate) const CALL_RET: u32 = 0x0001_2340;

/// A kernel with one running guest thread, over a mock CPU.
pub(crate) struct TestKernel {
    pub k: Kernel,
    pub cpu: MockCpu,
    pub p: TestPlatform,
    pub hub: Arc<IrqHub>,
    next_scratch: u32,
}

/// A 64 MiB test kernel whose first thread is running (never entered: its
/// entry point is not mapped).
pub(crate) fn kernel() -> TestKernel {
    kernel_with(|_| {})
}

/// The same, with the configuration adjusted first.
pub(crate) fn kernel_with(adjust: impl FnOnce(&mut KernelConfig)) -> TestKernel {
    let ram = Ram::new(64 << 20).expect("guest RAM");
    let hub = Arc::new(IrqHub::new());
    let mut cfg = KernelConfig::new(ram, Arc::clone(&hub));
    adjust(&mut cfg);
    let mut cpu = MockCpu::default();
    let mut k = Kernel::new(cfg, &mut cpu).expect("kernel");
    k.boot_thread(0x0001_1000, 0x10000).expect("first thread");
    TestKernel { k, cpu, p: TestPlatform::default(), hub, next_scratch: SCRATCH_VA }
}

impl TestKernel {
    /// Zeroed scratch guest memory of at least `len` bytes (page granular,
    /// with an unmapped page after it).
    pub(crate) fn alloc(&mut self, len: u32) -> u32 {
        let len = xb_types::align_up(len.max(1), xb_types::PAGE_SIZE);
        let va = self.next_scratch;
        assert!(self.k.gm.alloc_at(va, len), "scratch memory");
        self.next_scratch += len + xb_types::PAGE_SIZE;
        va
    }

    /// Calls the export `name` as the running thread would, through the real
    /// dispatcher: arguments placed per its calling convention, the entry at
    /// its trap address. Returns EDX:EAX. Only the call itself runs: no
    /// interrupt delivery or thread switch after it (see [`TestKernel::tail`]),
    /// so a call that blocks leaves the thread waiting.
    pub(crate) fn call(&mut self, name: &str, args: &[u32]) -> u64 {
        let e = exports::by_name(name).unwrap_or_else(|| panic!("no export {name}"));
        let esp0 = self.k.esp();
        let mut stack: Vec<u32> = vec![CALL_RET];
        let mut a = args;
        if e.abi == Abi::Fastcall {
            self.k.set_reg(reg::ECX, args.first().copied().unwrap_or(0));
            self.k.set_reg(reg::EDX, args.get(1).copied().unwrap_or(0));
            a = args.get(2..).unwrap_or(&[]);
        }
        stack.extend_from_slice(a);
        let esp = esp0 - 4 * stack.len() as u32;
        for (i, v) in stack.iter().enumerate() {
            self.k.gm.w32(esp + 4 * i as u32, *v).expect("the test thread's stack");
        }
        self.k.set_esp(esp);
        self.k.set_eip(TRAP_VA + u32::from(e.ordinal));
        self.k.kcall(&mut self.p, u32::from(e.ordinal));
        let r = u64::from(self.k.reg(reg::EAX)) | u64::from(self.k.reg(reg::EDX)) << 32;
        if self.k.eip() == CALL_RET && e.abi == Abi::Cdecl {
            self.k.set_esp(esp0);
        }
        r
    }

    /// The low 32 bits of [`TestKernel::call`]'s result.
    pub(crate) fn call32(&mut self, name: &str, args: &[u32]) -> u32 {
        self.call(name, args) as u32
    }

    /// One entry from the CPU with everything after it: I/O completion,
    /// interrupt delivery, the scheduler.
    pub(crate) fn entry(&mut self, exit: Exit) {
        self.k.entry(&mut self.p, exit);
    }

    /// What a kernel entry does after its event, with no event (a kick).
    pub(crate) fn tail(&mut self) {
        self.entry(Exit::Kicked);
    }

    /// Whether the last [`TestKernel::call`] returned to its caller (it did
    /// not block, redirect into a callout or fault).
    pub(crate) fn returned(&self) -> bool {
        self.k.eip() == CALL_RET
    }

    /// The fatal stop asked for, if any.
    pub(crate) fn stop_reason(&self) -> Option<String> {
        match &self.k.stop {
            Some(crate::run::Stop::Fatal(s)) => Some(s.clone()),
            Some(crate::run::Stop::Exit(_, s)) => Some(s.clone()),
            None => None,
        }
    }
}
