//! Interrupt request lines shared between devices, the kernel and the CPU.
//!
//! Devices run on any host thread and only ever assert or deassert a line.
//! The guest-CPU thread reads the pending set between guest blocks and lets
//! the kernel dispatch the title's interrupt service routine. Lines are level
//! triggered: a device keeps its line asserted until the guest clears the
//! cause in the device's own registers.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};

/// The console's interrupt lines (the PIC's IRQ numbers).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Irq {
    /// System timer (PIT channel 0), 1 kHz on the Xbox kernel.
    Timer = 0,
    /// First USB host controller (the four controller ports).
    Usb0 = 1,
    /// NV2A graphics processor.
    Gpu = 3,
    /// nForce network controller.
    Nic = 4,
    /// MCPX audio processor.
    Apu = 5,
    /// AC'97 audio codec interface.
    Aci = 6,
    /// Second USB host controller.
    Usb1 = 9,
    /// SMBus controller.
    Smbus = 11,
    /// IDE controller (hard disk and DVD drive).
    Ide = 14,
}

impl Irq {
    /// The line's bit in a pending mask.
    #[must_use]
    pub const fn mask(self) -> u32 {
        1 << self as u8
    }
}

/// The set of asserted interrupt lines, shared by every device.
///
/// Asserting a line also kicks the guest CPU out of guest code (through the
/// hook the machine installs with [`IrqHub::set_kick`]) so the interrupt is
/// taken at the next instruction boundary rather than at the next kernel call.
#[derive(Default)]
pub struct IrqHub {
    pending: AtomicU32,
    kick: OnceLock<Box<dyn Fn() + Send + Sync>>,
}

impl IrqHub {
    /// A hub with no line asserted and no kick hook.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Installs the function that makes the guest CPU leave guest code. Only
    /// the first call has an effect.
    pub fn set_kick(&self, kick: Box<dyn Fn() + Send + Sync>) {
        let _ = self.kick.set(kick);
    }

    /// Asserts `line`. Kicks the CPU if the line was not already asserted.
    pub fn raise(&self, line: Irq) {
        let before = self.pending.fetch_or(line.mask(), Ordering::AcqRel);
        if before & line.mask() == 0
            && let Some(kick) = self.kick.get()
        {
            kick();
        }
    }

    /// Deasserts `line`.
    pub fn lower(&self, line: Irq) {
        self.pending.fetch_and(!line.mask(), Ordering::AcqRel);
    }

    /// Asserts or deasserts `line`.
    pub fn set(&self, line: Irq, asserted: bool) {
        if asserted { self.raise(line) } else { self.lower(line) }
    }

    /// The mask of asserted lines ([`Irq::mask`] bits).
    #[must_use]
    pub fn pending(&self) -> u32 {
        self.pending.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn raising_a_line_kicks_once_until_it_is_lowered() {
        let hub = IrqHub::new();
        let kicks = Arc::new(AtomicUsize::new(0));
        let k = kicks.clone();
        hub.set_kick(Box::new(move || {
            k.fetch_add(1, Ordering::SeqCst);
        }));
        hub.raise(Irq::Gpu);
        hub.raise(Irq::Gpu);
        assert_eq!(kicks.load(Ordering::SeqCst), 1);
        assert_eq!(hub.pending(), Irq::Gpu.mask());
        hub.lower(Irq::Gpu);
        assert_eq!(hub.pending(), 0);
        hub.raise(Irq::Gpu);
        assert_eq!(kicks.load(Ordering::SeqCst), 2);
    }
}
