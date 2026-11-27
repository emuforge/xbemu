//! The MCPX's nForce Ethernet controller ("nvnet") at [`NIC_BASE`],
//! [`Irq::Nic`], with its PHY at MII address 1: a controller whose cable is
//! unplugged.
//!
//! The register model is xemu's (`hw/xbox/mcpx/nvnet/nvnet.c`, LGPL-2.1+;
//! Copyright (c) 2013 espes, Copyright (c) 2015-2025 Matt Borgerson), its
//! definitions after forcedeth.c.
//!
//! The title's network stack (XNet, linked into it) programs the controller
//! and its PHY. Without a link the PHY reports auto-negotiation complete and
//! no link, so nothing waits for it; a transmit kick sends nothing and
//! nothing is ever received, so the device needs neither guest RAM nor time.
//! The kernel's `PhyGetLinkState` answers from [`Nic::link_state`].

use std::sync::Arc;

use xb_types::irq::{Irq, IrqHub};

use crate::size_mask;

/// Physical address of the controller's registers.
pub const NIC_BASE: u32 = 0xFEF0_0000;
/// Length of the register window.
pub const NIC_SIZE: u32 = 0x400;
/// MII address of the PHY.
pub const PHY_ADDR: u32 = 1;

/// Register offsets (xemu's `nvnet_regs.h`).
pub mod reg {
    /// Interrupt status (write 1 to clear).
    pub const IRQ_STATUS: u32 = 0x000;
    /// Interrupt mask.
    pub const IRQ_MASK: u32 = 0x004;
    /// Transmitter control.
    pub const TRANSMITTER_CONTROL: u32 = 0x084;
    /// Receiver control.
    pub const RECEIVER_CONTROL: u32 = 0x094;
    /// Station address, low four bytes.
    pub const MAC_ADDR_A: u32 = 0x0A8;
    /// Station address, high two bytes.
    pub const MAC_ADDR_B: u32 = 0x0AC;
    /// Transmit ring base.
    pub const TX_RING_PHYS_ADDR: u32 = 0x100;
    /// Receive ring base.
    pub const RX_RING_PHYS_ADDR: u32 = 0x104;
    /// Ring sizes.
    pub const RING_SIZE: u32 = 0x108;
    /// Current transmit descriptor.
    pub const TX_RING_CUR_DESC: u32 = 0x11C;
    /// Current receive descriptor.
    pub const RX_RING_CUR_DESC: u32 = 0x120;
    /// Setup register 5 (forcedeth waits for its bit 31).
    pub const UNKNOWN_SETUP_REG5: u32 = 0x130;
    /// Next transmit descriptor.
    pub const TX_RING_NEXT_DESC: u32 = 0x134;
    /// Next receive descriptor.
    pub const RX_RING_NEXT_DESC: u32 = 0x138;
    /// Transmit / receive control.
    pub const TX_RX_CONTROL: u32 = 0x144;
    /// MII status (write 1 to clear).
    pub const MII_STATUS: u32 = 0x180;
    /// Adapter control.
    pub const ADAPTER_CONTROL: u32 = 0x188;
    /// MDIO address and command.
    pub const MDIO_ADDR: u32 = 0x190;
    /// MDIO data.
    pub const MDIO_DATA: u32 = 0x194;
}

/// `TX_RX_CONTROL` bits.
pub mod txrx {
    /// Start transmitting the valid descriptors.
    pub const KICK: u32 = 0x01;
    /// Clears the interrupt status.
    pub const BIT1: u32 = 0x02;
    /// DMA off.
    pub const BIT2: u32 = 0x04;
    /// The controller is idle (read only).
    pub const IDLE: u32 = 0x08;
    /// Reset the rings.
    pub const RESET: u32 = 0x10;
}

/// `MDIO_ADDR` bits; the PHY address is bits 5-9, the register bits 0-4.
pub mod mdio {
    /// The access is in progress (clears at once here).
    pub const INUSE: u32 = 0x8000;
    /// The access is a write of `MDIO_DATA`.
    pub const WRITE: u32 = 0x0400;
}

/// `ADAPTER_CONTROL`: the link is up.
pub const ADAPTER_CONTROL_LINKUP: u32 = 0x04;
/// `UNKNOWN_SETUP_REG5` bit 31.
pub const SETUP_REG5_BIT31: u32 = 0x8000_0000;

/// PHY (MII) registers.
pub mod mii {
    /// Basic mode control.
    pub const BMCR: usize = 0;
    /// Basic mode status.
    pub const BMSR: usize = 1;
    /// PHY identifier 1.
    pub const PHYID1: usize = 2;
    /// PHY identifier 2.
    pub const PHYID2: usize = 3;
    /// Auto-negotiation advertisement.
    pub const ANAR: usize = 4;
    /// Auto-negotiation link partner ability.
    pub const ANLPAR: usize = 5;
    /// Number of PHY registers.
    pub const COUNT: usize = 6;

    /// `BMCR`: reset (self-clearing).
    pub const BMCR_RESET: u32 = 0x8000;
    /// `BMCR`: auto-negotiation enable.
    pub const BMCR_AUTOEN: u32 = 0x1000;
    /// `BMCR`: restart auto-negotiation (self-clearing).
    pub const BMCR_ANRESTART: u32 = 0x0200;
    /// `BMCR`: full duplex.
    pub const BMCR_FD: u32 = 0x0100;
    /// `BMSR`: auto-negotiation complete.
    pub const BMSR_AN_COMP: u32 = 0x0020;
    /// `BMSR`: able to auto-negotiate.
    pub const BMSR_AUTONEG: u32 = 0x0008;
    /// `BMSR`: link up.
    pub const BMSR_LINK_ST: u32 = 0x0004;
    /// `ANLPAR`: the partner acknowledged.
    pub const ANLPAR_ACK: u32 = 0x4000;
    /// `ANAR` / `ANLPAR`: T4, TX FD, TX, 10 FD, 10.
    pub const ANLPAR_ABILITY: u32 = 0x03E0;
}

/// The Ethernet controller, without a link.
pub struct Nic {
    irq: Arc<IrqHub>,
    regs: [u8; NIC_SIZE as usize],
    phy: [u32; mii::COUNT],
    irq_level: bool,
}

impl Nic {
    /// The controller in its power-on state. `irq` is the hub [`Irq::Nic`]
    /// is asserted on.
    #[must_use]
    pub fn new(irq: Arc<IrqHub>) -> Self {
        let mut nic =
            Self { irq, regs: [0; NIC_SIZE as usize], phy: [0; mii::COUNT], irq_level: false };
        nic.reset();
        nic
    }

    /// Power-on state: registers clear, the controller idle, the PHY done
    /// negotiating without a link, the interrupt line low.
    pub fn reset(&mut self) {
        self.regs.fill(0);
        self.or_reg(reg::TX_RX_CONTROL, txrx::IDLE);
        self.phy_reset();
        self.link_down();
        self.update_irq();
    }

    /// The Ethernet link as `XNET_ETHERNET_LINK_*` flags for the kernel's
    /// `PhyGetLinkState`: 0, no link.
    #[must_use]
    pub fn link_state(&self) -> u32 {
        0
    }

    /// The kernel's `PhyInitialize`: the PHY would negotiate its link again.
    /// Without a network nothing happens and no link-change interrupt comes.
    pub fn phy_initialize(&mut self) {}

    /// Reads `size` (1, 2 or 4) bytes at `offset` from [`NIC_BASE`].
    #[must_use]
    pub fn mmio_read(&mut self, offset: u32, size: u8) -> u32 {
        let (o, n) = (offset as usize, usize::from(size.min(4)));
        let mut b = [0u8; 4];
        if let Some(src) = self.regs.get(o..o + n) {
            b[..n].copy_from_slice(src);
        }
        u32::from_le_bytes(b)
    }

    /// Writes `size` (1, 2 or 4) bytes at `offset` from [`NIC_BASE`].
    pub fn mmio_write(&mut self, offset: u32, size: u8, value: u32) {
        let (o, n) = (offset as usize, usize::from(size.min(4)));
        if o + n > self.regs.len() {
            return;
        }
        let v = value & size_mask(size);
        log::trace!(target: "dev", "nic: write {offset:03x} = {v:08x} ({size})");
        match offset {
            reg::MDIO_ADDR => {
                self.set_reg(offset, v);
                self.mdio(v);
            }
            reg::TX_RX_CONTROL => {
                let idle = self.reg(offset) & txrx::IDLE;
                self.set_reg(offset, idle | (v & !txrx::IDLE));
                // A kick transmits nothing without a link.
                if v & txrx::RESET != 0 {
                    self.reset_rings();
                }
                if v & txrx::BIT1 != 0 {
                    self.set_reg(reg::IRQ_STATUS, 0);
                    self.update_irq();
                } else if v == 0 {
                    self.set_reg(reg::UNKNOWN_SETUP_REG5, SETUP_REG5_BIT31); // forcedeth waits for it
                }
            }
            reg::IRQ_STATUS | reg::MII_STATUS => {
                let old = self.reg(offset);
                self.set_reg(offset, old & !v);
                self.update_irq();
            }
            reg::IRQ_MASK => {
                self.set_reg(offset, v);
                self.update_irq();
            }
            _ => self.regs[o..o + n].copy_from_slice(&v.to_le_bytes()[..n]),
        }
    }

    fn reg(&self, off: u32) -> u32 {
        let o = off as usize;
        u32::from_le_bytes([self.regs[o], self.regs[o + 1], self.regs[o + 2], self.regs[o + 3]])
    }

    fn set_reg(&mut self, off: u32, v: u32) {
        let o = off as usize;
        self.regs[o..o + 4].copy_from_slice(&v.to_le_bytes());
    }

    fn or_reg(&mut self, off: u32, v: u32) {
        self.set_reg(off, self.reg(off) | v);
    }

    fn update_irq(&mut self) {
        let level = self.reg(reg::IRQ_STATUS) & self.reg(reg::IRQ_MASK) != 0;
        if level != self.irq_level {
            self.irq_level = level;
            self.irq.set(Irq::Nic, level);
        }
    }

    // -------------------------------------------------------------- the PHY

    /// No link: auto-negotiation reads as done, without a link, so nothing
    /// waits for it (xemu: the kernel's PhyInitialize waits for AN complete
    /// before it looks at the link).
    fn link_down(&mut self) {
        self.phy[mii::BMSR] = (self.phy[mii::BMSR] & !mii::BMSR_LINK_ST) | mii::BMSR_AN_COMP;
        self.phy[mii::ANLPAR] &= !mii::ANLPAR_ACK;
        let v = self.reg(reg::ADAPTER_CONTROL) & !ADAPTER_CONTROL_LINKUP;
        self.set_reg(reg::ADAPTER_CONTROL, v);
    }

    fn phy_reset(&mut self) {
        self.phy = [0; mii::COUNT];
        self.phy[mii::BMCR] = mii::BMCR_FD | mii::BMCR_AUTOEN;
        self.phy[mii::BMSR] = mii::BMSR_AUTONEG | mii::BMSR_AN_COMP | mii::BMSR_LINK_ST;
        self.phy[mii::ANAR] = mii::ANLPAR_ABILITY;
        self.phy[mii::ANLPAR] = mii::ANLPAR_ABILITY;
    }

    fn phy_write(&mut self, r: usize, v: u32) {
        if r == mii::BMCR && v & (mii::BMCR_RESET | mii::BMCR_ANRESTART) != 0 {
            // Both self-clear; a reset restores the defaults.
            if v & mii::BMCR_RESET != 0 {
                self.phy_reset();
            } else {
                self.phy[mii::BMCR] = v & !mii::BMCR_ANRESTART;
            }
            self.link_down();
            return;
        }
        if let Some(reg) = self.phy.get_mut(r) {
            *reg = v;
        }
    }

    fn mdio(&mut self, v: u32) {
        let (addr, r) = ((v >> 5) & 0x1F, (v & 0x1F) as usize);
        if v & mdio::WRITE != 0 {
            if addr == PHY_ADDR {
                self.phy_write(r, self.reg(reg::MDIO_DATA) & 0xFFFF);
            }
        } else {
            let data =
                if addr == PHY_ADDR { self.phy.get(r).copied().unwrap_or(0) } else { u32::MAX };
            self.set_reg(reg::MDIO_DATA, data);
        }
        let done = self.reg(reg::MDIO_ADDR) & !mdio::INUSE;
        self.set_reg(reg::MDIO_ADDR, done);
    }

    fn reset_rings(&mut self) {
        let tx = self.reg(reg::TX_RING_PHYS_ADDR);
        self.set_reg(reg::TX_RING_CUR_DESC, tx);
        self.set_reg(reg::TX_RING_NEXT_DESC, tx);
        let rx = self.reg(reg::RX_RING_PHYS_ADDR);
        self.set_reg(reg::RX_RING_CUR_DESC, rx);
        self.set_reg(reg::RX_RING_NEXT_DESC, rx);
    }
}

xb_types::state_fields!(Nic { regs, phy, irq_level });

#[cfg(test)]
mod tests {
    use super::*;

    fn nic() -> (Nic, Arc<IrqHub>) {
        let irq = Arc::new(IrqHub::new());
        (Nic::new(irq.clone()), irq)
    }

    /// An MII register read the way forcedeth's `mii_rw` does it: the
    /// address with the in-use bit, a wait for the bit to clear, the data.
    fn mii_read(n: &mut Nic, phy: u32, r: usize) -> u32 {
        n.mmio_write(reg::MDIO_ADDR, 4, mdio::INUSE | phy << 5 | r as u32);
        assert_eq!(n.mmio_read(reg::MDIO_ADDR, 4) & mdio::INUSE, 0, "the access completes at once");
        n.mmio_read(reg::MDIO_DATA, 4)
    }

    fn mii_write(n: &mut Nic, phy: u32, r: usize, v: u32) {
        n.mmio_write(reg::MDIO_DATA, 4, v);
        n.mmio_write(reg::MDIO_ADDR, 4, mdio::INUSE | mdio::WRITE | phy << 5 | r as u32);
        assert_eq!(n.mmio_read(reg::MDIO_ADDR, 4) & mdio::INUSE, 0);
    }

    #[test]
    fn the_phy_reports_negotiation_done_and_no_link() {
        let (mut n, _) = nic();
        let bmsr = mii_read(&mut n, PHY_ADDR, mii::BMSR);
        assert_ne!(bmsr & mii::BMSR_AN_COMP, 0);
        assert_eq!(bmsr & mii::BMSR_LINK_ST, 0);
        assert_eq!(mii_read(&mut n, PHY_ADDR, mii::ANLPAR) & mii::ANLPAR_ACK, 0);
        assert_eq!(n.mmio_read(reg::ADAPTER_CONTROL, 4) & ADAPTER_CONTROL_LINKUP, 0);
        assert_eq!(n.link_state(), 0);
    }

    #[test]
    fn no_other_mii_address_answers() {
        let (mut n, _) = nic();
        for phy in (0..32).filter(|&p| p != PHY_ADDR) {
            assert_eq!(mii_read(&mut n, phy, mii::BMSR), u32::MAX);
        }
    }

    #[test]
    fn a_phy_reset_or_negotiation_restart_still_finds_no_link() {
        let (mut n, irq) = nic();
        n.mmio_write(reg::IRQ_MASK, 4, u32::MAX);
        for command in [mii::BMCR_RESET, mii::BMCR_AUTOEN | mii::BMCR_ANRESTART] {
            mii_write(&mut n, PHY_ADDR, mii::BMCR, command);
            let bmcr = mii_read(&mut n, PHY_ADDR, mii::BMCR);
            assert_eq!(
                bmcr & (mii::BMCR_RESET | mii::BMCR_ANRESTART),
                0,
                "the command bits self-clear"
            );
            let bmsr = mii_read(&mut n, PHY_ADDR, mii::BMSR);
            assert_ne!(bmsr & mii::BMSR_AN_COMP, 0);
            assert_eq!(bmsr & mii::BMSR_LINK_ST, 0);
        }
        n.phy_initialize();
        assert_eq!(irq.pending(), 0, "no link-change interrupt without a link");
    }

    #[test]
    fn ordinary_phy_registers_hold_what_is_written() {
        let (mut n, _) = nic();
        mii_write(&mut n, PHY_ADDR, mii::ANAR, 0x01E1);
        assert_eq!(mii_read(&mut n, PHY_ADDR, mii::ANAR), 0x01E1);
    }

    #[test]
    fn a_transmit_kick_sends_nothing_and_raises_no_interrupt() {
        let (mut n, irq) = nic();
        n.mmio_write(reg::IRQ_MASK, 4, u32::MAX);
        n.mmio_write(reg::TX_RING_PHYS_ADDR, 4, 0x8000);
        n.mmio_write(reg::RING_SIZE, 4, 0x000F_000F);
        n.mmio_write(reg::TRANSMITTER_CONTROL, 4, 1);
        n.mmio_write(reg::TX_RX_CONTROL, 4, txrx::KICK);
        assert_eq!(n.mmio_read(reg::IRQ_STATUS, 4), 0);
        assert_eq!(irq.pending(), 0);
        // The controller stays idle and the guest cannot clear the idle bit.
        assert_ne!(n.mmio_read(reg::TX_RX_CONTROL, 4) & txrx::IDLE, 0);
    }

    #[test]
    fn the_driver_reset_sequence_is_acknowledged() {
        let (mut n, _) = nic();
        n.mmio_write(reg::TX_RING_PHYS_ADDR, 4, 0x8000);
        n.mmio_write(reg::RX_RING_PHYS_ADDR, 4, 0x9000);
        n.mmio_write(reg::TX_RX_CONTROL, 4, txrx::BIT1 | txrx::RESET);
        assert_eq!(n.mmio_read(reg::TX_RING_NEXT_DESC, 4), n.mmio_read(reg::TX_RING_PHYS_ADDR, 4));
        assert_eq!(n.mmio_read(reg::RX_RING_CUR_DESC, 4), n.mmio_read(reg::RX_RING_PHYS_ADDR, 4));
        assert_eq!(n.mmio_read(reg::UNKNOWN_SETUP_REG5, 4) & SETUP_REG5_BIT31, 0);
        n.mmio_write(reg::TX_RX_CONTROL, 4, 0);
        assert_ne!(n.mmio_read(reg::UNKNOWN_SETUP_REG5, 4) & SETUP_REG5_BIT31, 0);
    }

    #[test]
    fn plain_registers_round_trip_at_every_size() {
        let (mut n, _) = nic();
        n.mmio_write(reg::MAC_ADDR_A, 4, 0x1234_5678);
        n.mmio_write(reg::MAC_ADDR_B, 2, 0x9ABC);
        assert_eq!(n.mmio_read(reg::MAC_ADDR_A, 4), 0x1234_5678);
        assert_eq!(n.mmio_read(reg::MAC_ADDR_A + 1, 1), 0x56);
        assert_eq!(n.mmio_read(reg::MAC_ADDR_B, 2), 0x9ABC);
        // An access past the window reads 0 and is dropped.
        n.mmio_write(NIC_SIZE - 2, 4, u32::MAX);
        assert_eq!(n.mmio_read(NIC_SIZE - 2, 4), 0);
    }

    #[test]
    fn reset_returns_to_the_power_on_state() {
        let (mut n, _) = nic();
        n.mmio_write(reg::MAC_ADDR_A, 4, 0x1234_5678);
        mii_write(&mut n, PHY_ADDR, mii::ANAR, 0);
        n.reset();
        assert_eq!(n.mmio_read(reg::MAC_ADDR_A, 4), 0);
        assert_eq!(mii_read(&mut n, PHY_ADDR, mii::ANAR), mii::ANLPAR_ABILITY);
        assert_ne!(n.mmio_read(reg::TX_RX_CONTROL, 4) & txrx::IDLE, 0);
    }

    /// Pins what a savestate holds of this: see `xb_types::state::StateIo::probe`.
    #[test]
    fn the_saved_layout_of_the_network_controller_is_the_recorded_one() {
        let mut io = xb_types::state::StateIo::probe();
        let (mut v, _) = nic();
        io.io(&mut v);
        assert_eq!(
            io.shape_hash(),
            0x2C1C_B471_2E28_6810,
            "the saved layout changed. Older states would be misread: give the section a new version, \
             read the old one where it differs, then record {:#018x} here",
            io.shape_hash()
        );
    }
}
