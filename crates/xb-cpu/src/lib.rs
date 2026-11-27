//! The guest CPU interface every backend implements.
//!
//! The Xbox has one Pentium III. A title's code, the title's statically linked
//! libraries and its interrupt handlers all run in ring 0, in a flat 32-bit
//! address space with paging on. The kernel itself is not guest code here: it
//! is reimplemented on the host (`xb-kernel`), so the CPU's job is to run the
//! title until something needs the host and say what.
//!
//! Backends:
//!
//! * `xb-cpu-kvm`: hardware virtualization. The guest runs natively in a VM.
//! * `xb-cpu-x64`: a translator that re-encodes guest x86-32 code for a
//!   64-bit host process.
//!
//! Both present the same model so the kernel and the machine never ask which
//! one is running:
//!
//! * One [`Cpu`] object, used from one host thread (the guest-CPU thread).
//! * Guest threads are [`Context`] values the kernel owns. [`Cpu::run`] takes
//!   the context to run and writes the state back before it returns, so a
//!   thread switch is just calling `run` with another context.
//! * Device registers and ports are served through [`Bus`] callbacks without
//!   leaving `run`; everything else comes back as an [`Exit`].

#![forbid(unsafe_code)]

use std::sync::Arc;

pub use xb_types::mem::{Access, Mapper, PageProtector};

/// Indices into [`Regs::gpr`], in the x86 encoding order.
pub mod reg {
    /// EAX.
    pub const EAX: usize = 0;
    /// ECX.
    pub const ECX: usize = 1;
    /// EDX.
    pub const EDX: usize = 2;
    /// EBX.
    pub const EBX: usize = 3;
    /// ESP.
    pub const ESP: usize = 4;
    /// EBP.
    pub const EBP: usize = 5;
    /// ESI.
    pub const ESI: usize = 6;
    /// EDI.
    pub const EDI: usize = 7;
}

/// EFLAGS bits the host side reads or sets.
pub mod eflags {
    /// Carry.
    pub const CF: u32 = 1 << 0;
    /// Zero.
    pub const ZF: u32 = 1 << 6;
    /// Trap (single step).
    pub const TF: u32 = 1 << 8;
    /// Interrupts enabled.
    pub const IF: u32 = 1 << 9;
    /// Direction.
    pub const DF: u32 = 1 << 10;
    /// Bit 1 always reads as set.
    pub const FIXED: u32 = 1 << 1;
}

/// A guest thread's integer state.
///
/// Segments are not here: CS, DS, ES and SS are flat for every title, and FS
/// differs only by its base, which points at the kernel's per-processor block
/// (the KPCR; `fs:[0x28]` is the current thread and so on).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct Regs {
    /// General registers, indexed by [`reg`].
    pub gpr: [u32; 8],
    /// Address of the next instruction.
    pub eip: u32,
    /// Flags ([`eflags`]).
    pub eflags: u32,
    /// Linear base of the FS segment.
    pub fs_base: u32,
}

/// x87, MMX and SSE state in the processor's `FXSAVE` layout (512 bytes).
#[derive(Clone, Copy)]
#[repr(C, align(16))]
pub struct FxState(pub [u8; 512]);

impl FxState {
    /// The state after `FNINIT` with the control words the Xbox kernel gives a
    /// new thread: x87 control word `0x027F` (53-bit precision, all exceptions
    /// masked) and MXCSR `0x1F80`.
    #[must_use]
    pub fn new() -> Self {
        let mut s = [0u8; 512];
        s[0..2].copy_from_slice(&0x027Fu16.to_le_bytes());
        s[24..28].copy_from_slice(&0x1F80u32.to_le_bytes());
        Self(s)
    }
}

impl Default for FxState {
    fn default() -> Self {
        Self::new()
    }
}

/// Everything the CPU holds for one guest thread.
#[derive(Clone, Copy, Default)]
#[repr(C)]
pub struct Context {
    /// Integer state.
    pub regs: Regs,
    /// Floating-point and vector state.
    pub fx: FxState,
}

/// Why [`Cpu::run`] returned. The context passed to `run` holds the guest's
/// state at the point described.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Exit {
    /// The next instruction lies in the trap range ([`Cpu::set_trap_range`]).
    /// It was not executed; `regs.eip` is its address. The kernel puts its
    /// export entry points and its return sentinels there.
    Trap,
    /// Another thread asked the CPU to stop ([`Kicker::kick`]), or
    /// [`RunFlags::exit_when_interruptible`] was set and interrupts became
    /// enabled. The guest is at an instruction boundary; `regs.eflags` tells
    /// whether interrupts are enabled.
    Kicked,
    /// The guest executed `HLT`; `regs.eip` is past it.
    Halt,
    /// The guest raised a processor exception the backend could not resolve
    /// (through [`Bus::page_fault`] for page faults). For faults `regs.eip` is
    /// the faulting instruction; for traps (`INT3`, `INTO`) it is past it.
    Exception {
        /// Vector number (0 divide error, 3 breakpoint, 6 invalid opcode,
        /// 13 general protection, 14 page fault, ...).
        vector: u8,
        /// The error code the processor pushes, or 0 for vectors without one.
        error_code: u32,
        /// Faulting linear address (page faults only, else 0).
        address: u32,
    },
    /// The guest executed `INT n` (n other than 3); `regs.eip` is past it.
    SoftwareInterrupt(u8),
    /// The backend cannot continue: an instruction it does not support, or a
    /// host failure. The text says what, for the log.
    Fatal(String),
}

/// What a [`Bus::page_fault`] handler decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultAction {
    /// The cause is gone (write tracking opened the page): run the
    /// instruction again.
    Retry,
    /// A real fault: return [`Exit::Exception`] so the kernel dispatches it to
    /// the title's exception handlers.
    Raise,
}

/// The machine as the CPU sees it while it runs: devices and fault policy.
///
/// Called on the guest-CPU thread, inside [`Cpu::run`]. Handlers must not
/// block for long and must not call back into the [`Cpu`].
pub trait Bus {
    /// Reads `size` (1, 2 or 4) bytes from a device register at physical
    /// address `pa` (a page mapped with [`Mapper::map_mmio`]).
    fn mmio_read(&mut self, pa: u32, size: u8) -> u32;
    /// Writes the low `size` bytes of `value` to a device register.
    fn mmio_write(&mut self, pa: u32, size: u8, value: u32);
    /// Reads `size` bytes from I/O port `port` (`IN`).
    fn io_read(&mut self, port: u16, size: u8) -> u32;
    /// Writes to I/O port `port` (`OUT`).
    fn io_write(&mut self, port: u16, size: u8, value: u32);
    /// The guest touched `va` in a way its mapping or the page's protection
    /// ([`PageProtector`]) forbids. `pa` is the physical address behind `va`
    /// when a mapping exists.
    fn page_fault(&mut self, va: u32, pa: Option<u32>, write: bool) -> FaultAction;
    /// The time-stamp counter for `RDTSC`, in ticks of the console's 733 MHz
    /// clock. Backends that run `RDTSC` natively scale the host's counter to
    /// the same rate and never call this.
    fn tsc(&mut self) -> u64;
    /// `RDMSR`. Unknown registers read as 0.
    fn rdmsr(&mut self, _msr: u32) -> u64 {
        0
    }
    /// `WRMSR`. Unknown registers are ignored.
    fn wrmsr(&mut self, _msr: u32, _value: u64) {}
}

/// Options for one [`Cpu::run`] call.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RunFlags {
    /// Return [`Exit::Kicked`] as soon as the guest has interrupts enabled
    /// (at once if it already has). Set when an interrupt is pending but the
    /// guest had them disabled.
    pub exit_when_interruptible: bool,
}

/// Makes a running [`Cpu`] return [`Exit::Kicked`] at the next instruction
/// boundary. Callable from any thread. A kick that arrives while the CPU is
/// not in `run` is remembered and ends the next `run` at once.
#[derive(Clone)]
pub struct Kicker(Arc<dyn Fn() + Send + Sync>);

impl Kicker {
    /// Wraps the backend's kick function.
    pub fn new(f: impl Fn() + Send + Sync + 'static) -> Self {
        Self(Arc::new(f))
    }

    /// Asks the CPU to stop.
    pub fn kick(&self) {
        (self.0)();
    }
}

/// One guest processor.
///
/// The [`Mapper`] supertrait is how the kernel builds the guest's address
/// space. A backend is created with the guest's physical RAM (see `xb-mem`)
/// and is `Send` so the machine can move it to the guest-CPU thread; after
/// that every method is called on that thread.
pub trait Cpu: Mapper + Send {
    /// A short name for the log (`"kvm"`, `"x64"`).
    fn name(&self) -> &'static str;

    /// Runs guest code from `ctx` until something needs the host. On return
    /// `ctx` holds the guest's state.
    fn run(&mut self, ctx: &mut Context, bus: &mut dyn Bus, flags: RunFlags) -> Exit;

    /// A handle that stops a running `run` from another thread.
    fn kicker(&self) -> Kicker;

    /// The guest-access restriction used for write tracking.
    fn protector(&self) -> Arc<dyn PageProtector>;

    /// Declares `[va, va + len)` (page aligned) as the trap range: executing
    /// any address in it returns [`Exit::Trap`] instead. The range needs no
    /// mapping. One range exists at a time.
    fn set_trap_range(&mut self, va: u32, len: u32);

    /// Guest code in physical `[pa, pa + len)` was changed by the host (a
    /// loader patch, a savestate): cached translations of it must go.
    /// Backends that execute guest memory directly do nothing.
    fn invalidate_code(&mut self, pa: u32, len: u32);

    /// The guest's time-stamp counter is `tsc` now: the machine's guest time
    /// stood still (a pause) or moved (a savestate loaded), and `RDTSC`
    /// must follow it. A backend that takes every `RDTSC` from
    /// [`Bus::tsc`] has nothing to do; one whose guest reads the
    /// processor's own counter moves it.
    fn set_tsc(&mut self, _tsc: u64) {}

    /// Lets the guest read the device registers at bus address
    /// `[pa, pa + len)` (page aligned, mapped with `map_mmio`) straight from
    /// the host memory at `host`, without leaving guest code; writes still go
    /// to [`Bus::mmio_write`]. For registers whose reads have no side effect
    /// and whose current value the device keeps in that memory. `host` must
    /// stay mapped and readable for as long as this CPU lives. `false`: not
    /// supported, reads keep going to [`Bus::mmio_read`].
    fn read_through(&mut self, _pa: u32, _len: u32, _host: *const u8) -> bool {
        false
    }
}

/// The `CPUID` results of the console's processor (a 733 MHz Pentium III
/// "Coppermine", family 6 model 8 stepping 10), for backends that emulate the
/// instruction and for KVM's CPUID table. Returns `[eax, ebx, ecx, edx]`.
#[must_use]
pub fn cpuid(leaf: u32) -> [u32; 4] {
    match leaf {
        // Highest basic leaf 2, "GenuineIntel".
        0 => [2, 0x756E_6547, 0x6C65_746E, 0x4965_6E69],
        // Signature 0x68A; features: FPU VME DE PSE TSC MSR PAE MCE CX8 SEP
        // MTRR PGE MCA CMOV PAT PSE36 MMX FXSR SSE.
        1 => [0x0000_068A, 0, 0, 0x0383_F9FF],
        // Cache descriptors of the Coppermine-128 core.
        2 => [0x0301_0101, 0, 0, 0x0C04_0841],
        _ => [0, 0, 0, 0],
    }
}

xb_types::state_fields!(Regs { gpr, eip, eflags, fs_base });
xb_types::state_fields!(Context { regs, fx });

impl xb_types::state::Stateful for FxState {
    fn state(&mut self, io: &mut xb_types::state::StateIo) {
        io.bytes(&mut self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_fx_state_has_the_kernel_s_control_words() {
        let fx = FxState::new();
        assert_eq!(u16::from_le_bytes([fx.0[0], fx.0[1]]), 0x027F);
        assert_eq!(u32::from_le_bytes([fx.0[24], fx.0[25], fx.0[26], fx.0[27]]), 0x1F80);
    }

    #[test]
    fn cpuid_reports_sse_but_not_sse2() {
        let edx = cpuid(1)[3];
        assert_ne!(edx & (1 << 25), 0);
        assert_eq!(edx & (1 << 26), 0);
    }

    /// Pins what a savestate holds of this: see `xb_types::state::StateIo::probe`.
    #[test]
    fn the_saved_layout_of_a_cpu_context_is_the_recorded_one() {
        let mut io = xb_types::state::StateIo::probe();
        let mut v = Context::default();
        io.io(&mut v);
        assert_eq!(
            io.shape_hash(),
            0x335F_E797_9714_6CB5,
            "the saved layout changed. Older states would be misread: give the section a new version, \
             read the old one where it differs, then record {:#018x} here",
            io.shape_hash()
        );
    }
}
