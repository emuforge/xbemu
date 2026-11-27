//! The kernel's area in guest memory, kernel calls and the entries from the
//! CPU.
//!
//! Differences from the reference, all following from the CPU interface:
//!
//! * An export is entered by address, not by `int 0x3F`: its thunk points
//!   into the trap range and [`xb_cpu::Exit::Trap`] names it by `eip`. The
//!   return sentinels (thread, DPC, ISR, APC, callout, SEH) are trap
//!   addresses too.
//! * `CLI`, `STI`, `CPUID`, `MOV CRn`, `INVLPG`, `IRETD`, port I/O and MSR
//!   accesses are not system ops: the backend runs them or sends them to the
//!   `Bus`. What the kernel still needs from them is the interrupt flag,
//!   which it reads from `eflags` at each exit.
//! * The guest's IDT and GDT belong to the backend. A GDT and TSS image as
//!   the Xbox kernel's are still written to kernel memory, but `SIDT`/`SGDT`
//!   report the backend's tables and a title cannot hook a vector:
//!   exceptions go to the thread's structured exception handlers only.

use std::sync::Arc;

use xb_cpu::{Context, Exit, eflags, reg};
use xb_mem::{Ram, RelocateHooks, WriteTracker};
use xb_types::CONTIG_WINDOW;
use xb_types::irq::IrqHub;

use crate::clock::{GuestClock, RunControl, Ticker, WakeHandle};
use crate::exports::{self, Abi, ORDINAL_MAX};
use crate::gm::{Fault, GuestMem, KResult};
use crate::platform::Platform;
use crate::run::{ConsoleConfig, Stop};
use crate::{apc, ex, fast, intr, io, ke, loader, mm, ob, pool, ps, seh, timer, xc};

/// Where the kernel's area starts: the address the real kernel image has.
pub const KERNEL_VA: u32 = 0x8001_0000;
/// Physical address of the kernel's area.
pub const KERNEL_PA: u32 = 0x0001_0000;
/// Address space the area spans; only [`K_CLAIMED`] of it is kept.
pub(crate) const KERNEL_SIZE: u32 = 0x0004_0000;

/// The trap range: export entry points and return sentinels. One page with
/// nothing mapped, between the contiguous window (which ends at
/// `0x8800_0000` with 128 MiB) and the kernel's stacks at `0xD000_0000`.
/// The export with ordinal `n` is entered at `TRAP_VA + n`.
pub const TRAP_VA: u32 = 0x8FF0_0000;
/// Length of the trap range.
pub const TRAP_LEN: u32 = 0x1000;

/// The top of RAM kept from boot: the GPU's instance memory (D3D's 20 KiB)
/// and the 64 KiB pad above it.
pub(crate) const GPU_PAD: u32 = 0x0001_0000;
pub(crate) const GPU_TOP: u32 = 0x0000_5000 + GPU_PAD;
/// The NV2A's view of RAM.
pub(crate) const GPU_WINDOW: u32 = 0xF000_0000;

// Pseudo-ordinals: return addresses of routines the kernel runs.
pub(crate) const ORD_THREAD_RETURN: u32 = 0x3FF;
pub(crate) const ORD_DPC_RETURN: u32 = 0x3FE;
pub(crate) const ORD_CALLOUT_RETURN: u32 = 0x3FD;
pub(crate) const ORD_ISR_RETURN: u32 = 0x3FC;
pub(crate) const ORD_APC_RETURN: u32 = 0x3FB;
pub(crate) const ORD_SEH_RETURN: u32 = 0x3F9;
pub(crate) const ORD_SEH_NESTED: u32 = 0x3F8;
pub(crate) const ORD_SEH_COLLIDED: u32 = 0x3F7;
pub(crate) const ORD_APU_SPIN: u32 = 0x3F5;

// Kernel area layout (offsets from KERNEL_VA).
const K_DATA: u32 = 0x2000; // 0x40 bytes per data export
const K_DATA_SLOT: u32 = 0x40;
const K_DATA_BIG: u32 = 0xA000; // data exports larger than a slot
/// The named-object list's head (ob.rs).
pub(crate) const K_OB_NAMES: u32 = 0xA400;
const K_KPCR: u32 = 0x10000;
const K_GDT: u32 = 0x11800;
const K_TSS: u32 = 0x11900;
const K_FAST: u32 = 0x12000; // exports run as guest code
const K_FAST_END: u32 = 0x13000;
/// The physical pages the kernel keeps of its area. The rest is free memory,
/// as the Xbox kernel's area is once its INIT section is let go: Halo lays
/// out its game state from 0x61000, and D3D's second page of visibility-test
/// reports must be below 16 MiB.
const K_CLAIMED: u32 = 0x20000;

/// The pool's first chunk sits just above the kernel, where the Xbox
/// kernel's own memory ends: titles lay out physical memory at fixed addresses
/// from there (Halo, from 0x61000).
const POOL_SIZE: u32 = 0x0001_0000;
const POOL_BASE: u32 = KERNEL_PA + KERNEL_SIZE;

/// In the KPCR's page, past its fields: 1 while DPCs are queued, for the
/// guest-code `KfLowerIrql`.
pub(crate) const KPCR_DPC_PENDING: u32 = 0x800;
const PCR_IRQL: u32 = 0x24;

pub(crate) const STATUS_SUCCESS: u32 = 0x0000_0000;
pub(crate) const STATUS_TIMEOUT: u32 = 0x0000_0102;
pub(crate) const STATUS_PENDING: u32 = 0x0000_0103;
pub(crate) const STATUS_ACCESS_VIOLATION: u32 = 0xC000_0005;
pub(crate) const STATUS_INVALID_HANDLE: u32 = 0xC000_0008;
pub(crate) const STATUS_INVALID_PARAMETER: u32 = 0xC000_000D;
pub(crate) const STATUS_NO_MEMORY: u32 = 0xC000_0017;
pub(crate) const STATUS_ILLEGAL_INSTRUCTION: u32 = 0xC000_001D;
pub(crate) const STATUS_OBJECT_TYPE_MISMATCH: u32 = 0xC000_0024;
pub(crate) const STATUS_ARRAY_BOUNDS_EXCEEDED: u32 = 0xC000_008C;
pub(crate) const STATUS_INTEGER_DIVIDE_BY_ZERO: u32 = 0xC000_0094;
pub(crate) const STATUS_INTEGER_OVERFLOW: u32 = 0xC000_0095;
pub(crate) const STATUS_INSUFFICIENT_RESOURCES: u32 = 0xC000_009A;
pub(crate) const STATUS_BREAKPOINT: u32 = 0x8000_0003;

/// A kernel call's arguments in declaration order.
pub(crate) type Args = [u32; 16];

/// A kernel function: returns EDX:EAX. It may block or end the calling
/// thread, continue in a guest routine ([`Kernel::callout`]) or stop the
/// guest. A [`Fault`] is the title's access violation at the call.
pub(crate) type KFn = fn(&mut Kernel, &mut dyn Platform, &Args) -> KResult<u64>;

/// Guest object types: the kernel's type-object data exports.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum ObType {
    #[default]
    Thread,
    Event,
    Semaphore,
    Mutant,
    Timer,
    File,
    Device,
    Symlink,
    Directory,
    IoCompletion,
}

pub(crate) const OB_TYPES: usize = 10;

impl ObType {
    pub(crate) const ALL: [ObType; OB_TYPES] = [
        ObType::Thread,
        ObType::Event,
        ObType::Semaphore,
        ObType::Mutant,
        ObType::Timer,
        ObType::File,
        ObType::Device,
        ObType::Symlink,
        ObType::Directory,
        ObType::IoCompletion,
    ];

    const NAMES: [&'static str; OB_TYPES] = [
        "PsThreadObjectType",
        "ExEventObjectType",
        "ExSemaphoreObjectType",
        "ExMutantObjectType",
        "ExTimerObjectType",
        "IoFileObjectType",
        "IoDeviceObjectType",
        "ObSymbolicLinkObjectType",
        "ObDirectoryObjectType",
        "IoCompletionObjectType",
    ];
}

/// One of the last kernel calls, for fatal reports.
#[derive(Clone, Copy, Default)]
pub(crate) struct Recent {
    pub ordinal: u16,
    pub thread: u32,
    pub ret: u32,
    pub result: u32,
    pub args: [u32; 5],
    pub running: bool,
}

const RECENT: usize = 16;

/// Rates for the statistics line.
#[derive(Default)]
pub(crate) struct Stats {
    pub kcalls: u64,
    pub entries: u64,
    pub kicked: u64,
    /// Frame-latency holds taken (`frame.rs`).
    pub frame_holds: u64,
    pub schedules: u64,
    pub switches: u64,
    pub idles: u64,
    /// Host waits taken for a proven wait loop (`spin.rs`).
    pub spin_waits: u64,
    pub slices: u64,
    pub isr: [u32; 16],
    pub dpc_wait_max_ns: u64,
    pub dpc_run_max_ns: u64,
    pub dpc_slow: u64,
    pub count: Vec<u32>,
    pub last_ns: u64,
    pub ticks_seen: u64,
    pub apu_seen: u64,
    pub commits_seen: u64,
    pub decommits_seen: u64,
}

/// What the machine gives the kernel at creation.
pub struct KernelConfig {
    /// Guest physical RAM: the object the CPU backend was created over.
    pub ram: Ram,
    /// The interrupt lines the devices assert.
    pub irq: Arc<IrqHub>,
    /// Write tracking of RAM for the GPU, if the machine has one. Kernel
    /// stores into guest memory are reported to it.
    pub tracker: Option<Arc<WriteTracker>>,
    /// What moving a RAM page asks of write tracking and the GPU; `()` for a
    /// machine without a GPU.
    pub relocate_hooks: Box<dyn RelocateHooks + Send>,
    /// The console's settings.
    pub console: ConsoleConfig,
    /// Run the hot exports as guest code in the kernel's area (the default).
    /// Off, every call enters the kernel: for tests of those exports.
    pub fast_paths: bool,
    /// Wait out on the host the loops proven to wait (`spin.rs`; the
    /// default). Off, they run as guest code: to compare.
    pub wait_loops: bool,
}

/// The longest host wait for a proven wait loop before it is proven again:
/// what a loop's memory can be late by when a device changed it without an
/// interrupt (the APU and the USB controller write guest RAM from their own
/// threads).
pub(crate) const SPIN_WAIT_MAX: std::time::Duration = std::time::Duration::from_millis(1);

/// How often a thread in a proven wait loop is run for real all the same.
///
/// The proof is the reason the host may wait, and a proof can be wrong: the
/// first day's took Tiger Woods' straight-line code for a loop and parked
/// its main thread for good. Run now and then, a thread wrongly held moves
/// on (and is no longer where the proof was made), so the worst a wrong
/// proof costs is this long a pause. A thread rightly held spins until the
/// next kick, a millisecond at most: one part in a hundred of what it took
/// before.
pub(crate) const SPIN_RUN_EVERY: std::time::Duration = std::time::Duration::from_millis(100);

/// What the kernel keeps about wait loops (`Kernel::in_wait_loop`).
#[derive(Default)]
pub(crate) struct SpinState {
    pub enabled: bool,
    /// A kick stopped guest code: wherever it stopped is tried.
    pub sample: bool,
    /// Where a loop was last proven from.
    pub eip: u32,
    /// The first loop found was logged.
    pub told: bool,
    /// When the thread held in a wait loop next runs whatever the proof
    /// says ([`SPIN_RUN_EVERY`]); `None` while no loop is being waited out.
    pub run_at: Option<std::time::Instant>,
}

impl KernelConfig {
    /// A configuration with no write tracking and default console settings.
    #[must_use]
    pub fn new(ram: Ram, irq: Arc<IrqHub>) -> Self {
        Self {
            ram,
            irq,
            tracker: None,
            relocate_hooks: Box::new(()),
            console: ConsoleConfig::default(),
            fast_paths: true,
            wait_loops: true,
        }
    }
}

/// The Xbox kernel, reimplemented: its state on the host and its area in
/// guest memory.
///
/// Used on the guest-CPU thread only. Other threads reach it through the
/// handles it gives out: [`Kernel::wake_handle`], [`Kernel::control`],
/// [`Kernel::clock`].
pub struct Kernel {
    pub(crate) gm: GuestMem,
    pub(crate) clock: GuestClock,
    pub(crate) wake: WakeHandle,
    pub(crate) control: RunControl,
    pub(crate) flip_hold: crate::frame::FrameHold,
    pub(crate) ticker: Ticker,
    pub(crate) hub: Arc<IrqHub>,
    pub(crate) console: ConsoleConfig,

    // The kernel's area.
    pub(crate) tramp_va: [u32; ORDINAL_MAX],
    pub(crate) data_va: [u32; ORDINAL_MAX],
    pub(crate) impls: Vec<Option<KFn>>,
    pub(crate) type_va: [u32; OB_TYPES],
    pub(crate) kpcr_va: u32,
    pub(crate) gdt_va: u32,
    pub(crate) tss_va: u32,
    pub(crate) code_next: u32,

    /// The running thread's registers: what the CPU runs from and writes back.
    pub(crate) frame: Context,
    /// Stack bytes the kernel call being run pops.
    pub(crate) call_pop: u32,
    /// It continues in a callout (or another redirection of the frame).
    pub(crate) redirected: bool,
    pub(crate) in_call: bool,
    pub(crate) stop: Option<Stop>,
    pub(crate) calls: u64,
    pub(crate) first_ordinal: u32,
    pub(crate) recent: [Recent; RECENT],
    pub(crate) nrecent: usize,
    pub(crate) failed_logged: u32,
    pub(crate) stats: Stats,
    /// An interrupt waits for the guest to enable interrupts.
    pub(crate) irq_window: bool,
    pub(crate) spin: SpinState,
    /// When the machine wants its next `Platform::tick` (guest ns).
    pub(crate) dev_due: Option<u64>,
    /// The clock thread stops guest code for `dev_due`.
    pub(crate) dev_kick: bool,
    pub(crate) dev_started: bool,
    pub(crate) apu_spins: u64,
    pub(crate) fault_logged: u32,
    pub(crate) dbg_logged: u32,

    // The title.
    pub(crate) title: loader::Title,

    // Threads and the scheduler (ps.rs).
    pub(crate) threads: Vec<ps::Thread>,
    pub(crate) threads_hi: usize,
    pub(crate) current: Option<usize>,
    pub(crate) resched: bool,
    pub(crate) kprocess: u32,
    pub(crate) sched: ps::Sched,

    // Subsystems.
    pub(crate) pool: pool::Pool,
    pub(crate) ob: ob::State,
    pub(crate) ke: ke::State,
    pub(crate) timer: timer::State,
    pub(crate) intr: intr::State,
    pub(crate) seh_logged: u32,
    pub(crate) mm: mm::State,
    pub(crate) ex: ex::State,
    pub(crate) io: io::State,
}

/// Why the kernel could not be set up.
#[derive(Debug)]
pub struct InitError(pub String);

impl std::fmt::Display for InitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for InitError {}

impl Kernel {
    /// Sets up the guest's address space and the kernel's area in it: the
    /// contiguous window and the GPU's view of RAM, the kernel's physical
    /// pages, the export entry points and data, the KPCR and the pool.
    ///
    /// `cpu` is the backend created over `cfg.ram`. The kernel claims the
    /// trap range [`TRAP_VA`] on it; the mappings reach it when
    /// [`Kernel::run`] is first called, or with [`Kernel::sync_cpu`].
    ///
    /// # Errors
    ///
    /// RAM too small for the kernel's fixed layout.
    pub fn new(cfg: KernelConfig, cpu: &mut dyn xb_cpu::Cpu) -> Result<Self, InitError> {
        let ram_size = cfg.ram.size();
        if ram_size < (16 << 20) {
            return Err(InitError(format!("{} KiB of RAM is too little", ram_size >> 10)));
        }
        let clock = GuestClock::new();
        let wake = WakeHandle::new();
        let control = RunControl::new(wake.clone());
        let ticker = Ticker::new(clock.clone(), wake.clone());
        let mut k = Self {
            gm: GuestMem::new(cfg.ram, cfg.tracker, cfg.relocate_hooks),
            clock,
            wake,
            control,
            flip_hold: crate::frame::FrameHold::default(),
            ticker,
            hub: cfg.irq,
            console: cfg.console,
            tramp_va: [0; ORDINAL_MAX],
            data_va: [0; ORDINAL_MAX],
            impls: vec![None; ORDINAL_MAX],
            type_va: [0; OB_TYPES],
            kpcr_va: KERNEL_VA + K_KPCR,
            gdt_va: KERNEL_VA + K_GDT,
            tss_va: KERNEL_VA + K_TSS,
            code_next: KERNEL_VA + K_FAST,
            frame: Context::default(),
            call_pop: 0,
            redirected: false,
            in_call: false,
            stop: None,
            calls: 0,
            first_ordinal: 0,
            recent: [Recent::default(); RECENT],
            nrecent: 0,
            failed_logged: 0,
            stats: Stats { count: vec![0; ORDINAL_MAX], ..Stats::default() },
            irq_window: false,
            spin: SpinState { enabled: cfg.wait_loops, ..SpinState::default() },
            dev_due: None,
            dev_kick: true,
            dev_started: false,
            apu_spins: 0,
            fault_logged: 0,
            dbg_logged: 0,
            title: loader::Title::default(),
            threads: (0..ps::MAX_THREADS).map(|_| ps::Thread::default()).collect(),
            threads_hi: 0,
            current: None,
            resched: false,
            kprocess: 0,
            sched: ps::Sched::default(),
            pool: pool::Pool::default(),
            ob: ob::State::default(),
            ke: ke::State::default(),
            timer: timer::State::default(),
            intr: intr::State::default(),
            seh_logged: 0,
            mm: mm::State::default(),
            ex: ex::State::default(),
            io: io::State::default(),
        };
        k.init_memory(cfg.fast_paths)?;
        cpu.set_trap_range(TRAP_VA, TRAP_LEN);
        k.gm.flush(cpu);
        Ok(k)
    }

    /// Applies mapping changes the kernel made outside [`Kernel::run`] (the
    /// loader's) to the CPU backend now.
    pub fn sync_cpu(&mut self, cpu: &mut dyn xb_cpu::Cpu) {
        self.gm.flush(cpu);
    }

    fn init_memory(&mut self, fast_paths: bool) -> Result<(), InitError> {
        let ram_size = self.gm.ram_size();
        // The contiguous window: VA 0x80000000 + pa for all of RAM. And the
        // NV2A's BAR1 aperture at 0xF0000000, another view of RAM, where
        // D3D's tiled locks point.
        if !self.gm.map_window(CONTIG_WINDOW, 0, ram_size)
            || !self.gm.map_window(GPU_WINDOW, 0, ram_size)
        {
            return Err(InitError("the RAM windows could not be mapped".into()));
        }
        // Physical memory below the kernel image is never handed out, as on
        // the Xbox: devices whose DMA pointers the title hasn't set yet (the
        // APU's notifiers) write near address 0.
        self.gm.phys.claim(0, KERNEL_PA);
        if !self.gm.phys.claim(KERNEL_PA, K_CLAIMED) {
            return Err(InitError("the kernel's physical pages are taken".into()));
        }
        // The top of RAM the console's kernel keeps from its boot on: the
        // GPU's instance memory under a 64 KiB pad. Left free until D3D
        // started, a title's contiguous block asked for before took the last
        // pages (Conflict: Desert Storm writes its heap's guard just past its
        // first 128 KiB block).
        self.gm.phys.claim(ram_size - GPU_TOP, GPU_TOP);
        self.gm.kfill(KERNEL_VA, 0, K_CLAIMED);

        for e in exports::all() {
            let o = usize::from(e.ordinal);
            if e.abi == Abi::Data {
                self.data_va[o] = KERNEL_VA + K_DATA + u32::from(e.ordinal) * K_DATA_SLOT;
                // XboxAlternateSignatureKeys is 16 keys of 16 bytes: in a slot
                // it ran into IdexChannelObject's queue.
                if e.name == "XboxAlternateSignatureKeys" {
                    self.data_va[o] = KERNEL_VA + K_DATA_BIG;
                }
                // the public key (0x11C bytes), past the 16 keys
                if e.name == "XePublicKeyData" {
                    self.data_va[o] = KERNEL_VA + K_DATA_BIG + 0x100;
                }
            } else {
                self.tramp_va[o] = TRAP_VA + u32::from(e.ordinal);
            }
        }
        if fast_paths {
            self.install_fast_paths();
        }

        // A PE header with an export directory, as the real kernel image
        // has: code that looks up exports by ordinal in the image finds the
        // entry points and data.
        let km = KERNEL_VA;
        self.gm.kw32(km, 0x0090_5A4D); // "MZ"
        self.gm.kw32(km + 0x3C, 0x80); // e_lfanew
        self.gm.kw32(km + 0x80, 0x0000_4550); // "PE\0\0"
        self.gm.kw32(km + 0x84, 0x0000_014C); // i386, no sections
        self.gm.kw32(km + 0x94, 0x0102_00E0); // SizeOfOptionalHeader 0xE0, executable 32-bit
        self.gm.kw32(km + 0x98, 0x0000_010B); // PE32
        self.gm.kw32(km + 0x98 + 28, KERNEL_VA);
        self.gm.kw32(km + 0x98 + 92, 16); // NumberOfRvaAndSizes
        self.gm.kw32(km + 0x98 + 96, 0x200); // export directory
        self.gm.kw32(km + 0x98 + 100, 0x28);
        self.gm.kw32(km + 0x200 + 0x10, 1); // ordinal base
        self.gm.kw32(km + 0x200 + 0x14, ORDINAL_MAX as u32 - 1); // functions
        self.gm.kw32(km + 0x200 + 0x1C, 0x300); // AddressOfFunctions
        for o in 1..ORDINAL_MAX {
            let va = if self.tramp_va[o] != 0 { self.tramp_va[o] } else { self.data_va[o] };
            let rva = if va != 0 { va.wrapping_sub(KERNEL_VA) } else { 0 };
            self.gm.kw32(km + 0x300 + 4 * (o as u32 - 1), rva);
        }

        // Processor control region (fs:). KPCR: NT_TIB at 0 (ExceptionList,
        // StackBase, StackLimit), SelfPcr 0x1C, Prcb 0x20, Irql 0x24, PRCB at
        // 0x28 starting with CurrentThread (set by the scheduler).
        let pcr = self.kpcr_va;
        self.gm.kw32(pcr, 0xFFFF_FFFF);
        self.gm.kw32(pcr + 0x1C, pcr);
        self.gm.kw32(pcr + 0x20, pcr + 0x28);
        self.gm.kw8(pcr + PCR_IRQL, 0);

        // The descriptor tables the Xbox kernel has, as data: null, code 08,
        // data 10, TSS 18, KPCR 20 and 28; 256 interrupt gates.
        let d: [(u32, u32, u8, u8); 6] = [
            (0, 0, 0, 0),
            (0, 0xF_FFFF, 0x9B, 0xC),
            (0, 0xF_FFFF, 0x93, 0xC),
            (self.tss_va, 0x67, 0x89, 0),
            (pcr, 0xFFF, 0x93, 0x4),
            (pcr, 0xFFF, 0x93, 0x4),
        ];
        for (i, (base, limit, access, flags)) in d.into_iter().enumerate() {
            let at = self.gdt_va + 8 * i as u32;
            self.gm.kw32(at, (limit & 0xFFFF) | (base & 0xFFFF) << 16);
            self.gm.kw32(
                at + 4,
                ((base >> 16) & 0xFF)
                    | u32::from(access) << 8
                    | ((limit >> 16) & 0xF) << 16
                    | u32::from(flags) << 20
                    | (base & 0xFF00_0000),
            );
        }
        self.gm.kw32(self.tss_va + 8, 0x10); // SS0
        self.gm.kw32(self.tss_va + 0x64, 0x68 << 16); // no I/O map

        // The pool's first range just above the kernel.
        let pool_pa = self
            .gm
            .phys
            .alloc_aligned(POOL_SIZE, POOL_BASE, POOL_BASE + POOL_SIZE, xb_types::PAGE_SIZE, false)
            .or_else(|| {
                let hi = ram_size - xb_mem::TOP_RESERVE;
                self.gm.phys.alloc_aligned(POOL_SIZE, 0, hi, xb_types::PAGE_SIZE, true)
            })
            .ok_or_else(|| InitError("no RAM for the kernel pool".into()))?;
        self.pool_init(CONTIG_WINDOW + pool_pa, POOL_SIZE);

        for (i, name) in ObType::NAMES.iter().enumerate() {
            self.type_va[i] = self.data_addr(name);
        }

        ob::register(self);
        ps::register(self);
        ke::register(self);
        mm::register(self);
        ex::register(self);
        crate::rtl::register(self);
        seh::register(self);
        timer::register(self);
        intr::register(self);
        apc::register(self);
        io::register(self);
        xc::register(self);

        // Data exports read at boot.
        let ver = self.data_addr("XboxKrnlVersion");
        for (i, v) in [1u16, 0, 5838, 1].into_iter().enumerate() {
            self.gm.kw16(ver + 2 * i as u32, v);
        }
        self.gm.kw32(self.data_addr("KeTimeIncrement"), 10000); // 1 ms in 100 ns units
        self.gm.kw32(self.data_addr("HalDiskCachePartitionCount"), 3);
        // The IDE channel's device queue (IDE_CHANNEL_OBJECT +0x24), empty:
        // XAPI's CancelIo walks its list for the file's requests (Forza,
        // after a race), and a zero head took it through a NULL entry.
        let q = self.data_addr("IdexChannelObject") + 0x24;
        self.gm.kw8(q, 0x14);
        self.gm.kw8(q + 2, 0x0C);
        self.gm.kw32(q + 4, q + 4);
        self.gm.kw32(q + 8, q + 4);
        self.gm.kw8(self.data_addr("KdDebuggerNotPresent"), 1);
        Ok(())
    }

    fn install_fast_paths(&mut self) {
        let mut off = K_FAST;
        for (name, code) in fast::FAST {
            let Some(e) = exports::by_name(name) else { continue };
            let va = KERNEL_VA + off;
            self.gm.kwrite(va, code);
            // the kernel call for the rest: jmp to the export's trap address
            let jmp = va + code.len() as u32;
            let target = TRAP_VA + u32::from(e.ordinal);
            self.gm.kw8(jmp, 0xE9);
            self.gm.kw32(jmp + 1, target.wrapping_sub(jmp + 5));
            self.tramp_va[usize::from(e.ordinal)] = va;
            off += (code.len() as u32 + 5 + 15) & !15;
        }
        self.code_next = KERNEL_VA + off;
    }

    /// Room for `len` bytes of guest code in the kernel area (stubs), or 0.
    pub(crate) fn code_alloc(&mut self, len: u32) -> u32 {
        let va = (self.code_next + 15) & !15;
        if va + len > KERNEL_VA + K_FAST_END {
            return 0;
        }
        self.code_next = va + len;
        va
    }

    // ------------------------------------------------------------ exports

    /// Installs the implementation of the function export called `name`.
    pub(crate) fn imp(&mut self, name: &str, f: KFn) {
        match exports::by_name(name) {
            Some(e) if e.abi != Abi::Data => self.impls[usize::from(e.ordinal)] = Some(f),
            _ => panic!("kernel: no function export named {name}"),
        }
    }

    /// The address of the data export called `name`.
    pub(crate) fn data_addr(&self, name: &str) -> u32 {
        match exports::by_name(name) {
            Some(e) if e.abi == Abi::Data => self.data_va[usize::from(e.ordinal)],
            _ => panic!("kernel: no data export named {name}"),
        }
    }

    /// What a kernel thunk-table entry for `ordinal` resolves to, or 0.
    #[must_use]
    pub fn resolve(&self, ordinal: u32) -> u32 {
        match exports::by_ordinal(ordinal) {
            Some(e) if e.abi == Abi::Data => self.data_va[usize::from(e.ordinal)],
            Some(e) => self.tramp_va[usize::from(e.ordinal)],
            None => 0,
        }
    }

    /// Whether `ordinal` is a function export this kernel implements (a call
    /// to one it does not stops the guest).
    #[must_use]
    pub fn implemented(&self, ordinal: u32) -> bool {
        exports::by_ordinal(ordinal).is_some_and(|e| e.abi != Abi::Data)
            && self.impls[ordinal as usize].is_some()
    }

    // -------------------------------------------------------------- frame

    #[inline]
    pub(crate) fn reg(&self, r: usize) -> u32 {
        self.frame.regs.gpr[r]
    }

    #[inline]
    pub(crate) fn set_reg(&mut self, r: usize, v: u32) {
        self.frame.regs.gpr[r] = v;
    }

    #[inline]
    pub(crate) fn esp(&self) -> u32 {
        self.frame.regs.gpr[reg::ESP]
    }

    #[inline]
    pub(crate) fn set_esp(&mut self, v: u32) {
        self.frame.regs.gpr[reg::ESP] = v;
    }

    #[inline]
    pub(crate) fn eip(&self) -> u32 {
        self.frame.regs.eip
    }

    #[inline]
    pub(crate) fn set_eip(&mut self, v: u32) {
        self.frame.regs.eip = v;
    }

    /// A new guest thread's initial state: EFLAGS 0x202, the default x87
    /// control word and MXCSR, FS at the KPCR.
    pub(crate) fn context_init(&self, eip: u32, esp: u32) -> Context {
        let mut c = Context::default();
        c.regs.eip = eip;
        c.regs.gpr[reg::ESP] = esp;
        c.regs.eflags = eflags::IF | eflags::FIXED;
        c.regs.fs_base = self.kpcr_va;
        c
    }

    /// The current IRQL (the KPCR's byte, which guest code reads and writes).
    #[inline]
    pub(crate) fn irql(&self) -> u8 {
        self.gm.kr8(self.kpcr_va + PCR_IRQL)
    }

    #[inline]
    pub(crate) fn set_irql(&mut self, v: u8) {
        self.gm.kw8(self.kpcr_va + PCR_IRQL, v);
    }

    /// The trap address of a return sentinel or export.
    #[inline]
    pub(crate) const fn trap(ordinal: u32) -> u32 {
        TRAP_VA + ordinal
    }

    /// Stops the guest with a reason for the fatal report.
    pub(crate) fn request_stop(&mut self, reason: String) {
        self.stop = Some(Stop::Fatal(reason));
    }

    /// " ret ret ...": up to six return addresses up the guest's EBP frames.
    pub(crate) fn frame_chain(&self) -> String {
        use std::fmt::Write;
        let mut s = String::new();
        let mut bp = self.reg(reg::EBP);
        for _ in 0..6 {
            let (Ok(ret), Ok(up)) = (self.gm.r32(bp.wrapping_add(4)), self.gm.r32(bp)) else {
                break;
            };
            let _ = write!(s, " {ret:08x}");
            if up <= bp {
                break;
            }
            bp = up;
        }
        s
    }

    // ------------------------------------------------------------ callouts

    /// From inside a kernel function: the call returns by first running the
    /// guest `routine(args...)`, then returning to its caller with the
    /// routine's EAX.
    pub(crate) fn callout(&mut self, routine: u32, args: &[u32]) {
        let esp = self.esp();
        let Some(cur) = self.current else {
            self.request_stop(format!("callout to {routine:08x} with no thread"));
            return;
        };
        if self.threads[cur].callouts.len() == ps::MAX_CALLOUTS {
            self.request_stop(format!("callout to {routine:08x}: nesting too deep"));
            return;
        }
        let ret = self.gm.r32(esp).unwrap_or(0);
        self.threads[cur].callouts.push(ps::Callout { ret, esp: esp + self.call_pop, irql: 0xFF });
        // The routine's frame goes below the kernel call's; its stack
        // arguments stay put.
        let sp = esp - 16 - 4 * args.len() as u32;
        let mut ok = self.gm.w32(sp, Self::trap(ORD_CALLOUT_RETURN)).is_ok();
        for (i, a) in args.iter().enumerate() {
            ok &= self.gm.w32(sp + 4 + 4 * i as u32, *a).is_ok();
        }
        if !ok {
            self.request_stop(format!("callout to {routine:08x}: no stack at {sp:08x}"));
            return;
        }
        self.set_esp(sp);
        self.set_eip(routine);
        self.redirected = true;
    }

    /// The same, with the IRQL at `irql` while the routine runs.
    pub(crate) fn callout_at_irql(&mut self, routine: u32, args: &[u32], irql: u8) {
        self.callout(routine, args);
        if let Some(cur) = self.current
            && self.redirected
            && let Some(c) = self.threads[cur].callouts.last_mut()
        {
            c.irql = self.gm.kr8(self.kpcr_va + PCR_IRQL);
            self.set_irql(irql);
        }
    }

    fn callout_return(&mut self) {
        let Some(c) = self.current.and_then(|t| self.threads[t].callouts.pop()) else {
            self.request_stop("callout return with no callout pending".into());
            return;
        };
        // The routine's EAX is the kernel call's result.
        self.set_esp(c.esp);
        self.set_eip(c.ret);
        if c.irql != 0xFF {
            self.set_irql(c.irql);
            // As KfLowerIrql: DPCs queued while the routine ran at its IRQL
            // (KeSynchronizeExecution's, at the interrupt's) run now. They
            // waited for the next interrupt: Halo's vblank DPC a vblank late.
            if c.irql < 2 && self.dpc_pending() {
                self.resched = true;
            }
        }
    }

    // ----------------------------------------------------------- the clock

    /// The earliest of what is due goes to the clock thread, which stops the
    /// running guest for it. Call after a due time changes.
    pub(crate) fn clock_arm(&mut self) {
        let mut due = self.timer.due;
        let mut min = |d: Option<u64>| {
            if let Some(d) = d {
                due = Some(due.map_or(d, |x| x.min(d)));
            }
        };
        min(self.ke.wait_due);
        // a slice only while its thread runs (not an ISR or a DPC, briefly)
        if self.current.is_some() && self.current == self.sched.slice_thread {
            min(self.sched.slice_due);
        }
        // The devices' deadline is the clock thread's only while they ask
        // for it (`Platform::tick_needs_kick`): otherwise kernel entries and
        // the tick find it due.
        if self.dev_kick {
            min(self.dev_due);
        }
        // an I/O completion held for the drive's time (io.rs)
        min(self.io_next_due());
        self.ticker.arm(due);
    }

    /// The machine's devices that work by deadline.
    pub(crate) fn device_tick(&mut self, p: &mut dyn Platform, now: u64) {
        if !self.dev_started || self.dev_due.is_some_and(|d| now >= d) {
            self.dev_started = true;
            self.dev_due = p.tick(now);
            self.dev_kick = p.tick_needs_kick();
        }
    }

    /// Timers and wait timeouts that are due: fire them, wake their threads,
    /// and reschedule if one of those outranks the running thread. Then the
    /// running thread's slice (`clock_tick`).
    pub(crate) fn clock_tick(&mut self, p: &mut dyn Platform) {
        let now = self.now();
        self.device_tick(p, now);
        let timer_due = self.timer.due.is_some_and(|d| now >= d);
        let wait_due = self.ke.wait_due.is_some_and(|d| now >= d);
        let slice_due = self.sched.slice_due.is_some_and(|d| now >= d);
        if timer_due {
            let (_, fired) = self.timer_poll();
            self.resched |= fired;
        }
        if wait_due {
            self.check_waits(0);
            if let Some(cur) = self.current {
                let cp = self.threads[cur].prio();
                if self.threads[..self.threads_hi]
                    .iter()
                    .any(|t| t.state == ps::ThreadState::Ready && t.suspend == 0 && t.prio() > cp)
                {
                    self.resched = true;
                }
            }
        }
        if slice_due {
            self.slice_end(now);
        }
        self.clock_arm();
    }

    // -------------------------------------------------------- kernel calls

    fn recent_add(&mut self, ordinal: u32, ret: u32, args: &Args) -> usize {
        self.stats.kcalls += 1;
        if let Some(c) = self.stats.count.get_mut(ordinal as usize) {
            *c += 1;
        }
        let i = self.nrecent % RECENT;
        self.nrecent += 1;
        let mut a = [0u32; 5];
        a.copy_from_slice(&args[..5]);
        self.recent[i] = Recent {
            ordinal: ordinal as u16,
            thread: self.current.map_or(0, |t| self.threads[t].id),
            ret,
            result: 0,
            args: a,
            running: true,
        };
        i
    }

    pub(crate) fn kcall(&mut self, p: &mut dyn Platform, ordinal: u32) {
        self.tick_time();
        self.clock_tick(p);
        let esp = self.esp();
        match ordinal {
            ORD_DPC_RETURN => return self.dpc_return(),
            ORD_CALLOUT_RETURN => return self.callout_return(),
            ORD_APC_RETURN => return self.apc_return(),
            ORD_ISR_RETURN => return self.intr_return(),
            ORD_SEH_RETURN | ORD_SEH_NESTED | ORD_SEH_COLLIDED => return self.seh_stub(ordinal),
            ORD_APU_SPIN => return self.apu_spin(p),
            ORD_THREAD_RETURN => {
                // As the kernel's thread startup does when the start routine returns.
                log::info!(target: "kernel", "ps: thread {} start routine returned (eax={:08x})",
                    self.current.map_or(0, |t| self.threads[t].id), self.reg(reg::EAX));
                self.terminate_current(STATUS_SUCCESS);
                return;
            }
            _ => {}
        }
        let Ok(ret) = self.gm.r32(esp) else {
            self.request_stop(format!("kernel call {ordinal} with no stack (esp {esp:08x})"));
            return;
        };
        let Some(e) = exports::by_ordinal(ordinal).filter(|e| e.abi != Abi::Data) else {
            self.request_stop(format!(
                "call to kernel ordinal {ordinal} (not a function) from {ret:08x}"
            ));
            return;
        };

        // Arguments in declaration order; cdecl: as many as the log shows.
        let mut args: Args = [0; 16];
        let nargs = if e.abi == Abi::Cdecl { 6 } else { usize::from(e.arg_bytes) / 4 };
        let mut stack_first = 0;
        if e.abi == Abi::Fastcall {
            args[0] = self.reg(reg::ECX);
            args[1] = self.reg(reg::EDX);
            stack_first = 2;
        }
        for (i, a) in args.iter_mut().enumerate().take(nargs.min(16)).skip(stack_first) {
            // a missing page above a cdecl call's real arguments reads as 0
            *a = self.gm.r32(esp + 4 + 4 * (i - stack_first) as u32).unwrap_or(0);
        }

        self.calls += 1;
        if self.first_ordinal == 0 {
            self.first_ordinal = ordinal;
            log::info!(target: "kernel", "stage S2 first kernel call: {} ({ordinal}) from {ret:08x}", e.name);
        }

        let Some(f) = self.impls[ordinal as usize] else {
            let a: Vec<String> = args[..nargs.min(8)].iter().map(|v| format!("{v:08x}")).collect();
            self.request_stop(format!(
                "kunimpl {ordinal} {} caller={ret:08x} args=[{}]",
                e.name,
                a.join(" ")
            ));
            return;
        };

        let mut pop = 4;
        if e.abi == Abi::Stdcall {
            pop += u32::from(e.arg_bytes);
        } else if e.abi == Abi::Fastcall && e.arg_bytes > 8 {
            pop += u32::from(e.arg_bytes) - 8;
        }
        self.call_pop = pop;
        self.redirected = false;

        let caller = self.current;
        let caller_id = caller.map_or(0, |t| self.threads[t].id);
        let slot = self.recent_add(ordinal, ret, &args);
        self.in_call = true;
        let t0 = self.clock.raw_ns();
        let r = f(self, p, &args);
        let took = self.clock.raw_ns().saturating_sub(t0);
        self.in_call = false;
        self.recent[slot].running = false;
        if took >= 8_000_000 {
            // A kernel call that held the guest's CPU for over 8 ms: nothing
            // else ran meanwhile, interrupts included.
            log::debug!(target: "kernel", "slow: {} from {ret:08x} held the CPU {:.1} ms [{:08x} {:08x} {:08x} {:08x} {:08x} {:08x}]",
                e.name, took as f64 / 1e6, args[0], args[1], args[2], args[3], args[4], args[5]);
        }
        let r = match r {
            Ok(r) => r,
            Err(Fault { va, write }) => {
                // It touched a page the guest doesn't have, as the code it
                // stands for would have: an access violation to the thread's
                // exception handlers, as the Xbox kernel's own access raises
                // it. Rainbow Six: Lockdown commits its heap's pages from a
                // handler around its allocations. The faulting instruction is
                // the export's entry: the call runs again once the handler
                // has the page.
                if self.fault_logged < 64 {
                    self.fault_logged += 1;
                    log::info!(target: "kernel", "{} from {ret:08x} touched {va:08x}, a page not in: it runs again", e.name);
                }
                let eip = self.eip();
                if self.stop.is_none()
                    && !self.seh_fault(STATUS_ACCESS_VIOLATION, eip, &[u32::from(write), va])
                {
                    self.request_stop(format!(
                        "{} touched {va:08x}, a page the guest doesn't have",
                        e.name
                    ));
                }
                return;
            }
        };
        self.recent[slot].result = r as u32;

        if log::log_enabled!(target: "kernel", log::Level::Trace) {
            log::trace!(target: "kernel", "ktrace: t{caller_id} {} from {ret:08x} [{:08x} {:08x} {:08x} {:08x} {:08x} {:08x}] -> {:08x}{} | frames{}",
                e.name, args[0], args[1], args[2], args[3], args[4], args[5], r as u32,
                if self.redirected { " (callout)" } else { "" }, self.frame_chain());
        } else if (r as u32) >= 0xC000_0000
            && !self.redirected
            && (e.name.starts_with("Nt") || e.name.starts_with("Io") || e.name.starts_with("Ob"))
        {
            // Failed object and I/O calls are rare and usually tell the
            // story, with the title code that led there; a title failing one
            // in a loop has its first 400 told, then one in 100.
            self.failed_logged += 1;
            if self.failed_logged <= 400 || self.failed_logged.is_multiple_of(100) {
                log::info!(target: "kernel", "ktrace: t{caller_id} {} from {ret:08x} [{:08x} {:08x} {:08x} {:08x} {:08x} {:08x}] -> {:08x} | frames{}{}",
                    e.name, args[0], args[1], args[2], args[3], args[4], args[5], r as u32,
                    self.frame_chain(), if self.failed_logged > 400 { " (one of 100)" } else { "" });
            }
        }
        let terminated =
            caller.is_some_and(|t| self.threads[t].state == ps::ThreadState::Terminated);
        if terminated || self.redirected {
            return; // PsTerminateSystemThread doesn't return; a callout returns later
        }
        self.set_reg(reg::EAX, r as u32);
        self.set_reg(reg::EDX, (r >> 32) as u32);
        self.set_esp(esp + pop);
        self.set_eip(ret);
    }

    /// DirectSound's mailbox wait (fixups.rs): the stub traps here with the
    /// mailbox's base in EAX; the stub checks the word again after.
    fn apu_spin(&mut self, p: &mut dyn Platform) {
        let word = self.reg(reg::EAX).wrapping_add(0x10);
        if let Some(pa) = self.gm.phys_of(word) {
            p.apu_mailbox_wait(pa);
        }
        self.apu_spins += 1;
        let Ok(ret) = self.gm.r32(self.esp()) else {
            self.request_stop("the APU wait stub has no stack".into());
            return;
        };
        // the stub called the trap address: back to it
        self.set_esp(self.esp() + 4);
        self.set_eip(ret);
    }

    /// `int 2Dh`, the debug service (DbgPrint, XAPI's OutputDebugStringA): EAX
    /// the service, ECX and EDX its arguments, and an `int 3` after it. With
    /// no debugger the kernel takes a print (service 1, ECX a STRING) and
    /// the other services, returns STATUS_SUCCESS and resumes past the
    /// `int 3`. `eip` is already past the `int 2Dh`.
    fn debug_service(&mut self) {
        let service = self.reg(reg::EAX);
        let s = self.reg(reg::ECX);
        if service == 1 && s != 0 && self.dbg_logged < 200 {
            #[expect(clippy::collapsible_if, reason = "the read comes after the cheap checks")]
            if let Ok(bytes) = self.gm.ansi_string(s) {
                self.dbg_logged += 1;
                let text: String = bytes
                    .iter()
                    .take(255)
                    .map(|&c| if c == b'\n' || c == b'\r' { ' ' } else { c as char })
                    .collect();
                log::info!(target: "kernel", "dbg: {text}");
            }
        }
        self.set_reg(reg::EAX, 0);
        let next = self.eip();
        if self.gm.r8(next) == Ok(0xCC) {
            self.set_eip(next + 1);
        }
    }

    /// A processor exception in the title's code: to the thread's exception
    /// handlers, as the Xbox kernel's trap handlers hand it.
    fn exception(&mut self, vector: u8, error_code: u32, address: u32) {
        let eip = self.eip();
        let handled = match vector {
            0 => self.seh_fault(STATUS_INTEGER_DIVIDE_BY_ZERO, eip, &[]),
            3 => {
                // a breakpoint: the context stays at it (a handler that
                // continues steps over it); eip is past the one-byte INT3
                self.set_eip(eip.wrapping_sub(1));
                let ecx = self.reg(reg::ECX);
                let ok = self.seh_fault(STATUS_BREAKPOINT, eip.wrapping_sub(1), &[0, ecx, 0]);
                if !ok {
                    self.set_eip(eip);
                }
                ok
            }
            // INTO with overflow set: a trap, the context after it
            4 => self.seh_fault(STATUS_INTEGER_OVERFLOW, eip, &[]),
            5 => self.seh_fault(STATUS_ARRAY_BOUNDS_EXCEEDED, eip, &[]),
            6 => self.seh_fault(STATUS_ILLEGAL_INSTRUCTION, eip, &[]),
            13 => self.seh_fault(STATUS_ACCESS_VIOLATION, eip, &[0, 0xFFFF_FFFF]),
            14 => {
                let write = u32::from(error_code & 2 != 0);
                let ok = self.seh_fault(STATUS_ACCESS_VIOLATION, eip, &[write, address]);
                if ok && self.fault_logged < 4 {
                    // the first few with what led to them: a title that goes
                    // on may still be lost
                    self.fault_logged += 1;
                    log::info!(target: "kernel", "guest access violation: {} of {address:08x} at eip {eip:08x}, taken by the title's handlers",
                        if write != 0 { "write" } else { "read" });
                }
                ok
            }
            _ => false,
        };
        if !handled && self.stop.is_none() {
            let what = match vector {
                0 => "divide error".to_owned(),
                3 => "breakpoint".to_owned(),
                4 => "INTO with overflow".to_owned(),
                5 => "BOUND past its bounds".to_owned(),
                6 => {
                    let b = self.gm.read_vec(eip, 3).unwrap_or_default();
                    format!("invalid opcode ({b:02x?})")
                }
                13 => "general protection fault".to_owned(),
                14 => format!(
                    "page fault: {} of {address:08x} (error {error_code:x})",
                    if error_code & 2 != 0 { "write" } else { "read" }
                ),
                v => format!("exception vector {v} (error {error_code:x})"),
            };
            self.request_stop(format!("{what} at {eip:08x}: the title doesn't handle it"));
        }
    }

    /// One entry from the CPU: the event, then what the way back to the
    /// guest looks at (completed I/O, interrupt lines, the scheduler).
    pub(crate) fn entry(&mut self, p: &mut dyn Platform, exit: Exit) {
        self.stats.entries += 1;
        self.ticker.refreshed();
        match exit {
            Exit::Trap => {
                let ordinal = self.eip().wrapping_sub(TRAP_VA);
                self.kcall(p, ordinal);
            }
            Exit::Kicked => {
                // The guest stopped for an asserted line, the clock or a
                // request; EIP stays.
                self.stats.kicked += 1;
                self.spin.sample = true;
                self.update_time();
                self.clock_tick(p);
                self.io_take_timing();
                self.frame_release();
                self.frame_take();
            }
            Exit::Halt => {
                // HLT: the title idles until the next interrupt.
                if self.frame.regs.eflags & eflags::IF == 0 {
                    let eip = self.eip();
                    self.request_stop(format!(
                        "HLT with interrupts off at {:08x}",
                        eip.wrapping_sub(1)
                    ));
                } else {
                    self.halt_wait(p);
                }
            }
            Exit::Exception { vector, error_code, address } => {
                self.tick_time();
                self.exception(vector, error_code, address);
            }
            Exit::SoftwareInterrupt(0x2D) => self.debug_service(),
            Exit::SoftwareInterrupt(n) => {
                let eip = self.eip();
                self.request_stop(format!("int {n:02x}h at {:08x}", eip.wrapping_sub(2)));
            }
            Exit::Fatal(text) => self.request_stop(format!("cpu: {text}")),
        }

        self.exit_tail(p);
    }

    /// What follows every exit and every wait: completed I/O, interrupts and
    /// the scheduler.
    pub(crate) fn exit_tail(&mut self, p: &mut dyn Platform) {
        // Disc reads done meanwhile complete, and asserted lines are
        // delivered, on the way back to the guest. Not while the thread has
        // just begun a wait: a read completing now would wake it through its
        // saved context (the status, an APC), which the scheduler then
        // overwrites with the live frame. The scheduler, which runs next,
        // saves the frame first and polls then.
        let running =
            self.current.is_some_and(|t| self.threads[t].state == ps::ThreadState::Running);
        if self.stop.is_none() {
            if running {
                self.io_poll();
            }
            let enabled = !running || self.frame.regs.eflags & eflags::IF != 0;
            self.intr_dispatch(enabled);
        }

        // No thread switch while the running thread is at DISPATCH_LEVEL or
        // above (a DPC, or code that raised the IRQL); it happens when the
        // IRQL drops. An ISR's entry is the exception.
        let irql = self.irql();
        if self.resched
            && self.stop.is_none()
            && (self.sched.force_switch || !(running && irql >= 2))
        {
            self.schedule(p);
        }
        self.stats_tick();
    }

    /// The guest executed HLT with interrupts on: the host waits for a line,
    /// work, or the next thing due.
    fn halt_wait(&mut self, p: &mut dyn Platform) {
        let now = self.now();
        self.device_tick(p, now);
        if self.hub.pending() & self.intr.enabled != 0 || self.control.pending() {
            return;
        }
        let mut due = self.timer.due;
        for d in [self.ke.wait_due, self.dev_due, self.io_next_due()].into_iter().flatten() {
            due = Some(due.map_or(d, |x| x.min(d)));
        }
        let max = std::time::Duration::from_millis(100);
        let delay = due.map_or(max, |d| self.clock.host_delay(d).min(max));
        self.stats.idles += 1;
        self.wake.wait(delay);
        self.update_time();
        self.clock_tick(p);
    }

    /// Whether the running thread is in a wait loop (`spin.rs`) that the
    /// host can wait out. Asked before guest code runs: after a kick, which
    /// is where a loop is first found, and whenever the thread is back at
    /// the instruction a loop was last proven from (after the interrupt that
    /// ended the wait, if its handler changed nothing the loop reads).
    ///
    /// Only below DISPATCH_LEVEL with interrupts on, where every interrupt,
    /// DPC and thread that could end the loop can still run.
    pub(crate) fn in_wait_loop(&mut self) -> bool {
        let sampled = std::mem::take(&mut self.spin.sample);
        let regs = self.frame.regs;
        if !(sampled || regs.eip == self.spin.eip) || !self.spin.enabled {
            return false;
        }
        let running =
            self.current.is_some_and(|t| self.threads[t].state == ps::ThreadState::Running);
        if !running || regs.eflags & eflags::IF == 0 || self.irql() >= 2 || self.stop.is_some() {
            return false;
        }
        let gm = &self.gm;
        let read = |va: u32, buf: &mut [u8]| gm.read(va, buf).is_ok();
        let env = crate::spin::Env { read: &read, trap: (TRAP_VA, TRAP_VA + TRAP_LEN) };
        if !crate::spin::is_wait_loop(&regs, &env) {
            self.spin.run_at = None;
            return false;
        }
        let now = std::time::Instant::now();
        match self.spin.run_at {
            None => self.spin.run_at = Some(now + SPIN_RUN_EVERY),
            Some(at) if now >= at => {
                // its turn to run for real (SPIN_RUN_EVERY)
                self.spin.run_at = Some(now + SPIN_RUN_EVERY);
                self.spin.eip = regs.eip;
                return false;
            }
            Some(_) => {}
        }
        if self.spin.eip != regs.eip && !std::mem::replace(&mut self.spin.told, true) {
            log::info!(target: "kernel", "wait loop at {:08x}: the host waits with it (the first found; later ones are not logged)", regs.eip);
        }
        self.spin.eip = regs.eip;
        true
    }

    /// The host's side of a proven wait loop: it sleeps until an interrupt,
    /// the next thing due or [`SPIN_WAIT_MAX`], whichever is first. The
    /// caller proves the loop again before the next wait.
    ///
    /// The spinner keeps its quantum, as it does running: a ready thread of
    /// its priority takes over when the quantum ends and no sooner. Given
    /// the CPU at once, Halo's file reader, which retries a failed read
    /// after a yield, ran sixty thousand times a second against the main
    /// thread's wait for the vertical blank.
    pub(crate) fn spin_wait(&mut self, p: &mut dyn Platform) {
        self.stats.entries += 1;
        self.stats.spin_waits += 1;
        self.ticker.refreshed();
        let now = self.now();
        self.device_tick(p, now);
        if !self.intr_deliverable() && !self.control.pending() {
            let mut due = self.timer.due;
            let others = [self.ke.wait_due, self.dev_due, self.io_next_due(), self.sched.slice_due];
            for d in others.into_iter().flatten() {
                due = Some(due.map_or(d, |x| x.min(d)));
            }
            let delay = due.map_or(SPIN_WAIT_MAX, |d| self.clock.host_delay(d).min(SPIN_WAIT_MAX));
            self.wake.wait(delay);
        }
        self.update_time();
        self.clock_tick(p);
        self.io_take_timing();
        self.frame_release();
        self.frame_take();
        self.exit_tail(p);
    }

    /// The per-second statistics line, at DEBUG.
    fn stats_tick(&mut self) {
        static CENSUS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let census =
            *CENSUS.get_or_init(|| std::env::var_os("XBEMU_CENSUS").is_some_and(|v| v != "0"));
        if self.stats.entries & 0xFF != 0
            || !(census || log::log_enabled!(target: "kernel", log::Level::Debug))
        {
            return;
        }
        let lvl = if census { log::Level::Info } else { log::Level::Debug };
        let now = self.now();
        if self.stats.last_ns == 0 {
            self.stats.last_ns = now.max(1);
            return;
        }
        if now - self.stats.last_ns < 1_000_000_000 {
            return;
        }
        let s = (now - self.stats.last_ns) as f64 / 1e9;
        self.stats.last_ns = now;
        let mut top = String::new();
        for _ in 0..14 {
            let Some((best, &n)) = self
                .stats
                .count
                .iter()
                .enumerate()
                .max_by_key(|(_, n)| **n)
                .filter(|(_, n)| **n > 0)
            else {
                break;
            };
            let name = exports::by_ordinal(best as u32).map_or("?", |e| e.name);
            top.push_str(&format!(" {name}:{:.0}", f64::from(n) / s));
            self.stats.count[best] = 0;
        }
        let mut isr = String::new();
        for (l, n) in self.stats.isr.iter().enumerate() {
            if *n != 0 {
                isr.push_str(&format!(" irq{l}:{:.0}", f64::from(*n) / s));
            }
        }
        let ticks = self.ticker.ticks();
        let holds = self.clock.take_hold_stats();
        let st = &self.stats;
        log::log!(target: "kernel", lvl,
            "stats per s: kcalls {:.0} entries {:.0} (kicked {:.0}, of them ticks {:.0}, frame holds {:.0}) schedules {:.0} switches {:.0} idle {:.0} wait loops {:.0} slices {:.0} apu waits {:.0} vm pages +{:.0} -{:.0} isr{isr} | dpc max wait {:.1} run {:.1} ms, {} over 8 ms | holds {} (longest {:.1} ms), {:.1} ms left out |{top}",
            st.kcalls as f64 / s, st.entries as f64 / s, st.kicked as f64 / s,
            (ticks - st.ticks_seen) as f64 / s, st.frame_holds as f64 / s, st.schedules as f64 / s, st.switches as f64 / s,
            st.idles as f64 / s, st.spin_waits as f64 / s, st.slices as f64 / s, (self.apu_spins - st.apu_seen) as f64 / s,
            (self.mm.commits - st.commits_seen) as f64 / s,
            (self.mm.decommits - st.decommits_seen) as f64 / s,
            st.dpc_wait_max_ns as f64 / 1e6, st.dpc_run_max_ns as f64 / 1e6, st.dpc_slow,
            holds.holds, holds.longest_ns as f64 / 1e6, holds.left_out_ns as f64 / 1e6);
        self.io_stats_log(s);
        let (contig, virt, blocks) = self.mm_usage();
        log::debug!(target: "kernel",
            "stats ram: free {} KiB, contiguous {} KiB ({blocks} blocks), virtual {} KiB, kernel pool {} of {} KiB (Ex {}, objects {}, {} free ranges)",
            self.gm.phys.free_bytes() >> 10, contig >> 10, virt >> 10,
            (self.pool.len - self.pool.free_bytes()) >> 10, self.pool.len >> 10,
            self.pool.ex >> 10, self.pool.ob >> 10, self.pool.free.len());
        let seen = (ticks, self.apu_spins, self.mm.commits, self.mm.decommits);
        let count = std::mem::take(&mut self.stats.count);
        self.stats = Stats {
            count: count.into_iter().map(|_| 0).collect(),
            last_ns: now,
            ticks_seen: seen.0,
            apu_seen: seen.1,
            commits_seen: seen.2,
            decommits_seen: seen.3,
            ..Stats::default()
        };
    }
}

impl xb_types::state::Stateful for ObType {
    fn state(&mut self, io: &mut xb_types::state::StateIo) {
        let mut v = *self as u8;
        v.state(io);
        if io.loading() {
            *self = Self::ALL.get(usize::from(v)).copied().unwrap_or_default();
        }
    }
}
