//! Host code for hot blocks.
//!
//! A block that has run a few times is compiled to host code, which then
//! runs in place of the interpreter's loop over its instructions. What the
//! code is, and how it stays equal to the interpreter, is in `native`; this
//! module chooses whether a host has it at all.
//!
//! Code generation exists for AArch64 on macOS. Every other host gets a
//! [`State`] that compiles nothing, and the interpreter runs alone: another
//! AArch64 host needs only its own executable-memory module beside `macos`.

#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
mod a64;
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
mod macos;
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
mod native;

#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
pub(crate) use native::{State, body, run};

#[cfg(not(all(target_arch = "aarch64", target_os = "macos")))]
mod none {
    //! The stand-in for hosts without code generation.

    use std::ops::ControlFlow;

    use xb_cpu::Bus;

    use crate::cpu::{InterpCpu, Step};
    use crate::fast::Uop;

    /// Compiles nothing.
    pub(crate) struct State;

    impl State {
        pub(crate) fn new(_tables: [usize; 4], _pages: u32, _chain: [usize; 3]) -> Self {
            Self
        }

        pub(crate) fn set_trap(&mut self, _trap: (u32, u64)) {}

        /// Never called: no block runs as host code.
        pub(crate) fn current(&self) -> u32 {
            0
        }

        /// Never compiles: `Continue(None)`.
        pub(crate) fn compile(&mut self, _ops: &[Uop]) -> ControlFlow<(), Option<usize>> {
            ControlFlow::Continue(None)
        }

        pub(crate) fn reset(&mut self) {}

        pub(crate) fn take_step(&mut self) -> Option<Step> {
            None
        }

        /// `None`: this CPU generates no code.
        #[cfg(test)]
        pub(crate) fn native_runs(&self) -> Option<u64> {
            None
        }
    }

    /// Never called: no block has an entry.
    pub(crate) fn run(
        _entry: usize,
        _cpu: &mut InterpCpu,
        _ops: &[Uop],
        _block: u32,
        _bus: &mut dyn Bus,
    ) -> u64 {
        0
    }

    /// Never called: no block has an entry.
    pub(crate) fn body(entry: usize) -> usize {
        entry
    }
}

#[cfg(not(all(target_arch = "aarch64", target_os = "macos")))]
pub(crate) use none::{State, body, run};

/// The status generated code returns in its high half when instruction
/// `status as u32` stopped with a result the run loop must act on
/// ([`State::take_step`]).
pub(crate) const STOPPED: u64 = 1;
