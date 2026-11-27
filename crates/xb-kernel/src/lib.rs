//! High-level reimplementation of the Xbox kernel (`xboxkrnl.exe`'s exports)
//! and the title loader.
//!
//! The kernel is not guest code: a title's kernel imports point into a trap
//! range, the CPU backend returns [`xb_cpu::Exit::Trap`] there, and the
//! export runs on the host against the guest's memory. Guest threads are
//! contexts this crate schedules onto the one guest CPU.
//!
//! `README.md` describes the public API and the threading rules, `STUBS.md`
//! the exports that do less than the console's, and
//! `docs/hw/kernel-exports.md` the export table and the kernel's guest-memory
//! layout.
//!
//! * [`Kernel::new`] builds the address space and the kernel's area.
//! * [`Kernel::boot`] loads the title from its disc and readies its first
//!   thread.
//! * [`Kernel::run`] runs the guest until something the machine must see.
//! * [`Platform`] is what the kernel asks of the machine.
#![forbid(unsafe_code)]

mod apc;
mod clock;
mod ex;
pub mod exports;
mod fast;
mod fixups;
mod frame;
mod gm;
mod intr;
mod io;
mod ke;
mod kernel;
mod loader;
mod mm;
mod ob;
mod platform;
mod pool;
mod ps;
mod rtl;
mod run;
mod seh;
mod spin;
mod state;
mod timer;
mod xc;
mod xc_crypto;

#[cfg(test)]
mod testing;
#[cfg(test)]
mod tests;

pub use clock::{GuestClock, HoldEnd, HoldStats, RunControl, WakeHandle};
pub use frame::FrameHold;
pub use io::{DiskFile, DiskHash, DiskState, disk_hash_name};
pub use kernel::{InitError, KERNEL_PA, KERNEL_VA, Kernel, KernelConfig, TRAP_LEN, TRAP_VA};
pub use loader::{BootError, BootInfo, BootOptions, Persisted, ram_size_for};
pub use platform::{DisplayMode, NullPlatform, Platform};
pub use run::{AvPack, ConsoleConfig, FatalReport, RunOutcome, TitleExit, video};
pub use state::STATE_LAYOUT;

/// Ordinals of the data exports the kernel keeps current at every entry.
mod ordinals {
    pub(crate) const KE_INTERRUPT_TIME: usize = 120;
    pub(crate) const KE_SYSTEM_TIME: usize = 154;
    pub(crate) const KE_TICK_COUNT: usize = 156;
}
