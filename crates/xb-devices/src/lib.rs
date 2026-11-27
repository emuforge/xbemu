//! Register-level devices titles drive directly: USB OHCI, hub, XID gamepad, legacy ports, AC'97.
//!
//! The Xbox kernel is implemented on the host, but a title's statically
//! linked libraries (XAPI's USB stack, DirectSound, XNet) program some
//! hardware themselves. Those devices live here, each as a plain Rust type
//! the machine (`xb-core`) owns, maps and routes to:
//!
//! | Device | Type | Where | Interrupt |
//! |---|---|---|---|
//! | USB host controller 0 | [`Ohci`] | MMIO [`OHCI_BASE`], [`OHCI_SIZE`] bytes | [`Irq::Usb0`](xb_types::irq::Irq::Usb0) |
//! | AC'97 codec interface | [`Aci`] | MMIO [`ACI_BASE`], [`ACI_SIZE`] bytes | [`Irq::Aci`](xb_types::irq::Irq::Aci) |
//! | nForce Ethernet | [`Nic`] | MMIO [`NIC_BASE`], [`NIC_SIZE`] bytes | [`Irq::Nic`](xb_types::irq::Irq::Nic) |
//! | I/O ports | [`Ports`] | every port (`IN`/`OUT`) | none |
//!
//! No device owns a thread. A device that does work with time (the OHCI's
//! 1 ms frames, the ACI's PCM pacing) has an `advance(now_ns)` method the
//! machine calls with the guest's clock; see the crate's `README.md`.
#![forbid(unsafe_code)]

pub mod aci;
pub mod nic;
pub mod ports;
pub mod usb;

#[cfg(test)]
mod testutil;

pub use aci::{ACI_BASE, ACI_SIZE, Aci};
pub use nic::{NIC_BASE, NIC_SIZE, Nic};
pub use ports::{PORT_ACPI_PM_TIMER, PORT_MCPX_GPIO, Ports};
pub use usb::ohci::{OHCI_BASE, OHCI_SIZE, Ohci};
pub use usb::xid::PadPorts;

/// The guest's time: nanoseconds that stand still while the emulation is
/// paused. The machine implements it (the kernel's clock); devices read it
/// where a register's value is a function of time.
///
/// The `now_ns` passed to a device's `advance` must come from the same clock.
pub trait Clock: Send + Sync {
    /// Nanoseconds since the machine booted.
    fn now_ns(&self) -> u64;
}

impl<F: Fn() -> u64 + Send + Sync> Clock for F {
    fn now_ns(&self) -> u64 {
        self()
    }
}

/// The GPU's count of vertical blanks since boot. The machine implements it
/// over `xb-gpu`; [`Ports`] derives the video field bit from its parity.
pub trait VblankCounter: Send + Sync {
    /// Vertical blanks so far.
    fn vblanks(&self) -> u64;
}

/// The all-ones value of an access `size` (1, 2 or 4) bytes wide: what an
/// unclaimed port or register reads as, and the mask of a sized value.
#[must_use]
pub(crate) const fn size_mask(size: u8) -> u32 {
    if size >= 4 { u32::MAX } else { (1u32 << (8 * size as u32)) - 1 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_device_can_move_to_the_guest_cpu_thread() {
        fn is_send<T: Send>() {}
        is_send::<Ohci>();
        is_send::<Aci>();
        is_send::<Nic>();
        is_send::<Ports>();
        fn is_shared<T: Send + Sync + Clone>() {}
        is_shared::<PadPorts>();
    }
}
