//! The MCPX's first OHCI USB host controller (OpenHCI 1.0a): registers at
//! [`OHCI_BASE`], [`Irq::Usb0`], four root ports.
//!
//! The title's own USB stack (XAPI) drives it. Each 1 ms frame does the
//! frame's work: the periodic list from the HCCA interrupt table, then the
//! control and bulk lists, then the done queue. Transfers complete at once
//! (the devices are in-process); a NAK leaves the TD for a later frame.
//!
//! Frames run from [`Ohci::advance`], which the machine calls with the
//! guest's clock; the controller owns no thread.
//!
//! Topology: controllers sit straight on the root ports, as the later
//! consoles wire them. Controller ports 0-3 (players 1-4) are root ports 3,
//! 4, 1, 2. Each controller is a 3-port hub with the gamepad on its port 1
//! and its two expansion slots (empty) on ports 2 and 3.
//!
//! Isochronous endpoints (only the voice headset uses them) are not
//! serviced: their endpoint descriptors are skipped.

use std::sync::Arc;

use xb_types::irq::{Irq, IrqHub};
use xb_types::mem::PhysMem;
use xb_types::pad::{PORTS, PadState, Rumble};

use super::hub::Hub;
use super::xid::{Gamepad, PadPorts};
use super::{UsbDevice, UsbError};
use crate::{Clock, size_mask};

/// Physical address of the controller's registers.
pub const OHCI_BASE: u32 = 0xFED0_0000;
/// Length of the register window.
pub const OHCI_SIZE: u32 = 0x1000;
/// Number of root hub ports.
pub const ROOT_PORTS: usize = 4;
/// Length of a frame.
pub const FRAME_NS: u64 = 1_000_000;

/// The root port (1-based) each controller port (0-3) is wired to.
pub const ROOT_PORT_OF_PAD: [usize; PORTS] = [3, 4, 1, 2];

/// Most frames one [`Ohci::advance`] runs to catch up; a longer gap (the
/// machine was stopped) is dropped rather than replayed.
const MAX_CATCH_UP_FRAMES: u32 = 32;
/// How far ahead an idle guest's controller looks for a frame with work.
const IDLE_FRAMES: u16 = 8;

/// Register offsets.
pub mod reg {
    /// `HcRevision`.
    pub const REVISION: u32 = 0x00;
    /// `HcControl`.
    pub const CONTROL: u32 = 0x04;
    /// `HcCommandStatus`.
    pub const COMMAND_STATUS: u32 = 0x08;
    /// `HcInterruptStatus` (write 1 to clear).
    pub const INTERRUPT_STATUS: u32 = 0x0C;
    /// `HcInterruptEnable` (write 1 to set).
    pub const INTERRUPT_ENABLE: u32 = 0x10;
    /// `HcInterruptDisable` (write 1 to clear the enable).
    pub const INTERRUPT_DISABLE: u32 = 0x14;
    /// `HcHCCA`.
    pub const HCCA: u32 = 0x18;
    /// `HcPeriodCurrentED`.
    pub const PERIOD_CURRENT_ED: u32 = 0x1C;
    /// `HcControlHeadED`.
    pub const CONTROL_HEAD_ED: u32 = 0x20;
    /// `HcControlCurrentED`.
    pub const CONTROL_CURRENT_ED: u32 = 0x24;
    /// `HcBulkHeadED`.
    pub const BULK_HEAD_ED: u32 = 0x28;
    /// `HcBulkCurrentED`.
    pub const BULK_CURRENT_ED: u32 = 0x2C;
    /// `HcDoneHead`.
    pub const DONE_HEAD: u32 = 0x30;
    /// `HcFmInterval`.
    pub const FM_INTERVAL: u32 = 0x34;
    /// `HcFmRemaining`.
    pub const FM_REMAINING: u32 = 0x38;
    /// `HcFmNumber`.
    pub const FM_NUMBER: u32 = 0x3C;
    /// `HcPeriodicStart`.
    pub const PERIODIC_START: u32 = 0x40;
    /// `HcLSThreshold`.
    pub const LS_THRESHOLD: u32 = 0x44;
    /// `HcRhDescriptorA`.
    pub const RH_DESCRIPTOR_A: u32 = 0x48;
    /// `HcRhDescriptorB`.
    pub const RH_DESCRIPTOR_B: u32 = 0x4C;
    /// `HcRhStatus`.
    pub const RH_STATUS: u32 = 0x50;
    /// `HcRhPortStatus[1]`; port n (1-based) is at `+ 4 * (n - 1)`.
    pub const RH_PORT_STATUS: u32 = 0x54;
}

/// `HcControl` bits.
pub mod control {
    /// Periodic list enable.
    pub const PLE: u32 = 1 << 2;
    /// Isochronous enable.
    pub const IE: u32 = 1 << 3;
    /// Control list enable.
    pub const CLE: u32 = 1 << 4;
    /// Bulk list enable.
    pub const BLE: u32 = 1 << 5;
    /// Host controller functional state (mask).
    pub const HCFS: u32 = 3 << 6;
    /// `HCFS`: operational.
    pub const HCFS_OPERATIONAL: u32 = 2 << 6;
    /// `HCFS`: suspended.
    pub const HCFS_SUSPEND: u32 = 3 << 6;
    /// Interrupt routing.
    pub const IR: u32 = 1 << 8;
}

/// `HcCommandStatus` bits.
pub mod command {
    /// Host controller reset.
    pub const HCR: u32 = 1 << 0;
    /// Control list filled.
    pub const CLF: u32 = 1 << 1;
    /// Bulk list filled.
    pub const BLF: u32 = 1 << 2;
    /// Ownership change request.
    pub const OCR: u32 = 1 << 3;
}

/// `HcInterruptStatus` / `HcInterruptEnable` bits.
pub mod interrupt {
    /// Scheduling overrun.
    pub const SO: u32 = 1 << 0;
    /// Writeback of the done head.
    pub const WDH: u32 = 1 << 1;
    /// Start of frame.
    pub const SF: u32 = 1 << 2;
    /// Resume detected.
    pub const RD: u32 = 1 << 3;
    /// Unrecoverable error.
    pub const UE: u32 = 1 << 4;
    /// Frame number overflow.
    pub const FNO: u32 = 1 << 5;
    /// Root hub status change.
    pub const RHSC: u32 = 1 << 6;
    /// Ownership change.
    pub const OC: u32 = 1 << 30;
    /// Master interrupt enable (enable register only).
    pub const MIE: u32 = 1 << 31;
}

/// `HcRhPortStatus` bits. Writing a status bit is a command (the names in
/// parentheses); writing a change bit clears it.
pub mod port {
    /// Current connect status (write: clear port enable).
    pub const CCS: u32 = 1 << 0;
    /// Port enable status (write: set port enable).
    pub const PES: u32 = 1 << 1;
    /// Port suspend status (write: set port suspend).
    pub const PSS: u32 = 1 << 2;
    /// Port over-current indicator (write: clear suspend status).
    pub const POCI: u32 = 1 << 3;
    /// Port reset status (write: set port reset).
    pub const PRS: u32 = 1 << 4;
    /// Port power status.
    pub const PPS: u32 = 1 << 8;
    /// Low-speed device attached.
    pub const LSDA: u32 = 1 << 9;
    /// Connect status change.
    pub const CSC: u32 = 1 << 16;
    /// Port enable status change.
    pub const PESC: u32 = 1 << 17;
    /// Port suspend status change.
    pub const PSSC: u32 = 1 << 18;
    /// Over-current indicator change.
    pub const OCIC: u32 = 1 << 19;
    /// Port reset status change.
    pub const PRSC: u32 = 1 << 20;
    /// The write-to-clear change bits.
    pub const CHANGE: u32 = CSC | PESC | PSSC | OCIC | PRSC;
}

/// Transfer descriptor condition codes.
pub mod cc {
    /// The transfer completed.
    pub const NO_ERROR: u32 = 0;
    /// The endpoint stalled.
    pub const STALL: u32 = 4;
    /// No device answers at the address.
    pub const DEVICE_NOT_RESPONDING: u32 = 5;
    /// The device sent less than the TD's buffer and rounding was off.
    pub const DATA_UNDERRUN: u32 = 9;
}

// HcRhDescriptorA
const RHA_NPS: u32 = 1 << 9;
const RHA_RW_MASK: u32 = 0x0000_1b00 | (0xff << 24);

// HcRhStatus
const RHS_DRWE: u32 = 1 << 15;
const RHS_OCIC: u32 = 1 << 17;
const RHS_CRWE: u32 = 1 << 31;

// Endpoint descriptor
const ED_K: u32 = 1 << 14;
const ED_F: u32 = 1 << 15;
const ED_H: u32 = 1;
const ED_C: u32 = 2;
const PTR_MASK: u32 = 0xffff_fff0;

// General transfer descriptor
const TD_R: u32 = 1 << 18;
const TD_T0: u32 = 1 << 24;
const TD_T1: u32 = 1 << 25;
const TD_EC_MASK: u32 = 3 << 26;
const TD_CC_SHIFT: u32 = 28;

const PID_SETUP: u32 = 0;
const PID_OUT: u32 = 1;
const PID_IN: u32 = 2;

/// Largest TD buffer: two pages.
const TD_BUFFER: usize = 8192;

/// What a device did with a TD's packet.
enum Outcome {
    Moved(u32),
    Stall,
    Nak,
    NoDevice,
}

/// OHCI controller 0 with the four controller ports behind it.
pub struct Ohci {
    mem: Arc<dyn PhysMem>,
    irq: Arc<IrqHub>,
    clock: Arc<dyn Clock>,
    pads: PadPorts,

    ctl: u32,
    status: u32,
    intr_status: u32,
    intr: u32,
    hcca: u32,
    ctrl_head: u32,
    ctrl_cur: u32,
    bulk_head: u32,
    bulk_cur: u32,
    per_cur: u32,
    done: u32,
    done_count: u32,
    fi: u32,
    fsmps: u32,
    fit: u32,
    pstart: u32,
    lst: u32,
    frame_number: u16,
    frame_t0_ns: u64,
    next_frame_ns: u64,
    rhdesc_a: u32,
    rhdesc_b: u32,
    rhstatus: u32,
    port: [u32; ROOT_PORTS],
    /// The device on each root port.
    dev: [Option<Box<dyn UsbDevice>>; ROOT_PORTS],
    /// Each controller port's hub while it is pulled out.
    parked: [Option<Box<dyn UsbDevice>>; PORTS],
    irq_level: bool,
    frames: u64,
    tds: u64,
    tds_logged: u64,
}

impl Ohci {
    /// The controller, with a gamepad plugged into each controller port
    /// whose bit is set in `pads.connected()`.
    ///
    /// `mem` is guest RAM (the HCCA, descriptors and buffers are read and
    /// written there), `irq` the hub [`Irq::Usb0`] is asserted on, `clock`
    /// the guest's time (the same clock [`Ohci::advance`] is called with).
    #[must_use]
    pub fn new(
        mem: Arc<dyn PhysMem>,
        irq: Arc<IrqHub>,
        clock: Arc<dyn Clock>,
        pads: PadPorts,
    ) -> Self {
        let mut ohci = Self {
            mem,
            irq,
            clock,
            pads: pads.clone(),
            ctl: 0,
            status: 0,
            intr_status: 0,
            intr: 0,
            hcca: 0,
            ctrl_head: 0,
            ctrl_cur: 0,
            bulk_head: 0,
            bulk_cur: 0,
            per_cur: 0,
            done: 0,
            done_count: 7,
            fi: 0,
            fsmps: 0,
            fit: 0,
            pstart: 0,
            lst: 0,
            frame_number: 0,
            frame_t0_ns: 0,
            next_frame_ns: 0,
            rhdesc_a: 0,
            rhdesc_b: 0,
            rhstatus: 0,
            port: [0; ROOT_PORTS],
            dev: [const { None }; ROOT_PORTS],
            parked: [const { None }; PORTS],
            irq_level: false,
            frames: 0,
            tds: 0,
            tds_logged: 0,
        };
        let mask = pads.connected();
        for (p, &root) in ROOT_PORT_OF_PAD.iter().enumerate() {
            let mut hub = Hub::new(3);
            hub.attach(1, Box::new(Gamepad::new(pads.clone(), p)));
            let hub: Box<dyn UsbDevice> = Box::new(hub);
            if mask >> p & 1 != 0 {
                ohci.dev[root - 1] = Some(hub);
            } else {
                ohci.parked[p] = Some(hub);
            }
        }
        ohci.reset();
        log::info!(target: "usb", "ohci: controllers on ports {}", port_list(mask));
        ohci
    }

    /// The controller ports: pad state in, rumble out, plug and unplug.
    #[must_use]
    pub fn pads(&self) -> &PadPorts {
        &self.pads
    }

    /// Sets the state of the controller on `port` (0-3).
    pub fn set_pad(&self, port: usize, state: &PadState) {
        self.pads.set_pad(port, state);
    }

    /// The motor speeds the title last sent to the controller on `port`.
    #[must_use]
    pub fn rumble(&self, port: usize) -> Rumble {
        self.pads.rumble(port)
    }

    /// Power-on state: the controller in USB reset, the root hub's ports
    /// reporting their devices as newly connected, the interrupt line low.
    pub fn reset(&mut self) {
        self.soft_reset();
        self.ctl = 0;
        self.roothub_reset();
    }

    /// When the next frame is due on the guest's clock, or `None` while the
    /// controller is not operational (nothing to do until the guest starts
    /// it; plug changes are still picked up by [`Ohci::advance`]).
    #[must_use]
    pub fn next_deadline_ns(&self) -> Option<u64> {
        self.operational().then_some(self.next_frame_ns)
    }

    /// Whether an endpoint of the list at `head` has a transfer queued.
    fn list_ready(&self, head: u32) -> bool {
        let mut pa = head & PTR_MASK;
        for _ in 0..256 {
            if pa == 0 {
                break;
            }
            let Some(ed) = self.read_words::<4>(pa) else {
                break;
            };
            if ed[0] & (ED_K | ED_F) == 0
                && ed[2] & ED_H == 0
                && (ed[2] & PTR_MASK) != (ed[1] & PTR_MASK)
            {
                return true;
            }
            pa = ed[3] & PTR_MASK;
        }
        false
    }

    /// Work every frame does whatever the lists hold: the start-of-frame
    /// interrupt unmasked, a done queue on its way out, a control or bulk
    /// list filled.
    fn frame_always_works(&self) -> bool {
        (self.intr & interrupt::MIE != 0 && self.intr & interrupt::SF != 0)
            || self.done != 0
            || (self.ctl & control::CLE != 0 && self.status & command::CLF != 0)
            || (self.ctl & control::BLE != 0 && self.status & command::BLF != 0)
    }

    /// Whether the frame `k` after the next has something to do as memory
    /// stands: a periodic endpoint with a transfer queued, or the frame
    /// number's overflow.
    fn frame_works(&self, k: u16) -> bool {
        let n = self.frame_number.wrapping_add(1 + k);
        if n & 0x7fff == 0 {
            return true;
        }
        let hcca = u64::from(self.hcca) + 256 <= u64::from(self.mem.size());
        self.ctl & control::PLE != 0
            && hcca
            && self.list_ready(self.mem.read_u32(self.hcca + 4 * u32::from(n & 31)))
    }

    /// Whether the next frame does something the guest can see (and may be
    /// interrupted for), so that it must run on time even while guest code
    /// runs. A frame that only counts can run late with the ones after it.
    #[must_use]
    pub fn frame_may_interrupt(&self) -> bool {
        self.operational() && (self.frame_always_works() || self.frame_works(0))
    }

    /// With the guest CPU idle (no guest code runs, so nothing is queued
    /// meanwhile): when the next frame with something to do is due, at most
    /// `IDLE_FRAMES` ahead. The frames before it only count and are run
    /// with it. `None` while the controller is not operational.
    #[must_use]
    pub fn idle_deadline_ns(&self) -> Option<u64> {
        if !self.operational() {
            return None;
        }
        if self.frame_always_works() {
            return Some(self.next_frame_ns);
        }
        let k = (0..IDLE_FRAMES).find(|&k| self.frame_works(k)).unwrap_or(IDLE_FRAMES);
        Some(self.next_frame_ns + u64::from(k) * FRAME_NS)
    }

    /// Runs the frames due up to `now_ns` (the guest's clock) and applies
    /// controllers plugged in or pulled out through [`PadPorts`]. Call it
    /// about every millisecond while the guest runs.
    pub fn advance(&mut self, now_ns: u64) {
        self.sync_pads();
        if !self.operational() {
            return;
        }
        let mut ran = 0;
        while now_ns >= self.next_frame_ns {
            if ran == MAX_CATCH_UP_FRAMES {
                self.next_frame_ns = now_ns + FRAME_NS;
                break;
            }
            self.frame_t0_ns = self.next_frame_ns;
            self.frame();
            self.next_frame_ns += FRAME_NS;
            ran += 1;
        }
    }

    /// Reads `size` (1, 2 or 4) bytes of the register at `offset` from
    /// [`OHCI_BASE`].
    #[must_use]
    pub fn mmio_read(&mut self, offset: u32, size: u8) -> u32 {
        let v = match offset & !3 {
            reg::REVISION => 0x10,
            reg::CONTROL => self.ctl,
            reg::COMMAND_STATUS => self.status,
            reg::INTERRUPT_STATUS => self.intr_status,
            reg::INTERRUPT_ENABLE | reg::INTERRUPT_DISABLE => self.intr,
            reg::HCCA => self.hcca,
            reg::PERIOD_CURRENT_ED => self.per_cur,
            reg::CONTROL_HEAD_ED => self.ctrl_head,
            reg::CONTROL_CURRENT_ED => self.ctrl_cur,
            reg::BULK_HEAD_ED => self.bulk_head,
            reg::BULK_CURRENT_ED => self.bulk_cur,
            reg::DONE_HEAD => self.done,
            reg::FM_INTERVAL => self.fit << 31 | self.fsmps << 16 | self.fi,
            reg::FM_REMAINING => {
                // Time left in the current 1 ms frame, in bit times.
                let into = self.clock.now_ns().wrapping_sub(self.frame_t0_ns) % FRAME_NS;
                ((u64::from(self.fi) * (FRAME_NS - into)) / FRAME_NS) as u32 & 0x3fff
            }
            reg::FM_NUMBER => u32::from(self.frame_number),
            reg::PERIODIC_START => self.pstart,
            reg::LS_THRESHOLD => self.lst,
            reg::RH_DESCRIPTOR_A => self.rhdesc_a,
            reg::RH_DESCRIPTOR_B => self.rhdesc_b,
            reg::RH_STATUS => self.rhstatus,
            off => match root_port_index(off) {
                Some(i) => self.port[i],
                None => 0,
            },
        };
        (v >> (8 * (offset & 3))) & size_mask(size)
    }

    /// Writes the register at `offset` from [`OHCI_BASE`]. Registers are
    /// written whole: a write that does not start at a register's first byte
    /// is ignored, and a narrower one is taken zero-extended, as the
    /// reference does.
    pub fn mmio_write(&mut self, offset: u32, size: u8, value: u32) {
        let v = value & size_mask(size);
        match offset {
            reg::CONTROL => {
                let old = self.ctl & control::HCFS;
                self.ctl = v;
                if v & control::HCFS != old {
                    log::info!(target: "usb", "ohci: state {} -> {}", old >> 6, (v & control::HCFS) >> 6);
                    if self.operational() {
                        self.frame_t0_ns = self.clock.now_ns();
                        self.next_frame_ns = self.frame_t0_ns;
                    }
                }
            }
            reg::COMMAND_STATUS => {
                if v & command::HCR != 0 {
                    self.soft_reset();
                }
                self.status |= v & (command::CLF | command::BLF | command::OCR);
            }
            reg::INTERRUPT_STATUS => {
                self.intr_status &= !v;
                self.update_irq();
            }
            reg::INTERRUPT_ENABLE => {
                self.intr |= v;
                self.update_irq();
            }
            reg::INTERRUPT_DISABLE => {
                self.intr &= !v;
                self.update_irq();
            }
            reg::HCCA => self.hcca = v & 0xffff_ff00,
            reg::CONTROL_HEAD_ED => self.ctrl_head = v & PTR_MASK,
            reg::CONTROL_CURRENT_ED => self.ctrl_cur = v & PTR_MASK,
            reg::BULK_HEAD_ED => self.bulk_head = v & PTR_MASK,
            reg::BULK_CURRENT_ED => self.bulk_cur = v & PTR_MASK,
            reg::FM_INTERVAL => {
                self.fi = v & 0x3fff;
                self.fsmps = (v >> 16) & 0x7fff;
                self.fit = v >> 31;
            }
            reg::PERIODIC_START => self.pstart = v & 0x3fff,
            reg::LS_THRESHOLD => self.lst = v & 0xfff,
            reg::RH_DESCRIPTOR_A => {
                self.rhdesc_a = (self.rhdesc_a & !RHA_RW_MASK) | (v & RHA_RW_MASK)
            }
            reg::RH_DESCRIPTOR_B => self.rhdesc_b = v,
            reg::RH_STATUS => {
                if v & RHS_OCIC != 0 {
                    self.rhstatus &= !RHS_OCIC;
                }
                if v & RHS_DRWE != 0 {
                    self.rhstatus |= RHS_DRWE;
                }
                if v & RHS_CRWE != 0 {
                    self.rhstatus &= !RHS_DRWE;
                }
            }
            off => {
                if off & 3 == 0
                    && let Some(i) = root_port_index(off)
                {
                    self.port_write(i, v);
                }
            }
        }
    }

    fn operational(&self) -> bool {
        self.ctl & control::HCFS == control::HCFS_OPERATIONAL
    }

    fn update_irq(&mut self) {
        let level =
            self.intr & interrupt::MIE != 0 && self.intr_status & self.intr & !interrupt::MIE != 0;
        if level != self.irq_level {
            self.irq_level = level;
            self.irq.set(Irq::Usb0, level);
        }
    }

    fn set_interrupt(&mut self, bits: u32) {
        self.intr_status |= bits;
        self.update_irq();
    }

    // ------------------------------------------------------------- root hub

    fn port_attach(&mut self, i: usize) {
        if let Some(dev) = &self.dev[i] {
            let lsda = if dev.core().low_speed { port::LSDA } else { 0 };
            self.port[i] |= port::CCS | port::CSC | lsda;
            self.set_interrupt(interrupt::RHSC);
        }
    }

    fn roothub_reset(&mut self) {
        self.rhdesc_a = RHA_NPS | ROOT_PORTS as u32;
        self.rhdesc_b = 0;
        self.rhstatus = 0;
        for i in 0..ROOT_PORTS {
            self.port[i] = port::PPS;
            if let Some(dev) = self.dev[i].as_deref_mut() {
                super::reset(dev);
                self.port_attach(i);
            }
        }
    }

    fn soft_reset(&mut self) {
        self.ctl = (self.ctl & control::IR) | control::HCFS_SUSPEND;
        self.status = 0;
        self.intr_status = 0;
        self.intr = interrupt::MIE;
        self.hcca = 0;
        self.ctrl_head = 0;
        self.ctrl_cur = 0;
        self.bulk_head = 0;
        self.bulk_cur = 0;
        self.per_cur = 0;
        self.done = 0;
        self.done_count = 7;
        self.fsmps = 0x2778;
        self.fi = 0x2edf;
        self.fit = 0;
        self.frame_number = 0;
        self.pstart = 0;
        self.lst = 0x628;
        self.update_irq();
    }

    /// SetPortEnable/Suspend/Reset on a port with nothing on it only sets
    /// CSC. Returns whether `bit` was newly set.
    fn port_set_if_connected(&mut self, i: usize, bit: u32) -> bool {
        if bit == 0 {
            return false;
        }
        if self.port[i] & port::CCS == 0 {
            self.port[i] |= port::CSC;
            return false;
        }
        if self.port[i] & bit != 0 {
            return false;
        }
        self.port[i] |= bit;
        true
    }

    fn port_write(&mut self, i: usize, val: u32) {
        let old = self.port[i];
        self.port[i] &= !(val & port::CHANGE);
        if val & port::CCS != 0 {
            // ClearPortEnable
            self.port[i] &= !port::PES;
        }
        self.port_set_if_connected(i, val & port::PES);
        self.port_set_if_connected(i, val & port::PSS);
        if val & port::POCI != 0 && self.port[i] & port::PSS != 0 {
            // ClearSuspendStatus
            self.port[i] = (self.port[i] & !port::PSS) | port::PSSC;
        }
        if self.port_set_if_connected(i, val & port::PRS)
            && let Some(dev) = self.dev[i].as_deref_mut()
        {
            super::reset(dev);
            self.port[i] &= !port::PRS;
            self.port[i] |= port::PES | port::PRSC;
        }
        // Power is never switched (NPS): the port stays powered.
        if old != self.port[i] {
            self.set_interrupt(interrupt::RHSC);
        }
    }

    /// A controller plugged in or pulled out, as on the console: its root
    /// port shows the connection change and the title's USB stack finds the
    /// gamepad, or sees it go. Only the controllers the players have are
    /// plugged in: one nobody holds would be found like any other, and a
    /// title that listens to the first controller it finds (AirForce Delta
    /// Storm) takes an empty one on root port 1, which enumerates first.
    fn sync_pads(&mut self) {
        let mask = self.pads.connected();
        let mut changed = false;
        for (p, &root) in ROOT_PORT_OF_PAD.iter().enumerate() {
            let i = root - 1;
            let want = mask >> p & 1 != 0;
            if want == self.dev[i].is_some() {
                continue;
            }
            if want {
                self.dev[i] = self.parked[p].take();
                if let Some(dev) = self.dev[i].as_deref_mut() {
                    super::reset(dev);
                }
                self.port[i] = (self.port[i] & port::PPS) | port::CCS | port::CSC;
            } else {
                self.port[i] = (self.port[i] & port::PPS) | port::CSC;
                self.parked[p] = self.dev[i].take();
            }
            changed = true;
        }
        if changed {
            self.set_interrupt(interrupt::RHSC);
            log::info!(target: "usb", "ohci: controllers on ports {}", port_list(mask));
        }
    }

    // ------------------------------------------------------------ transfers

    fn read_words<const N: usize>(&self, pa: u32) -> Option<[u32; N]> {
        let mut bytes = [0u8; 32];
        let bytes = &mut bytes[..4 * N];
        if !self.mem.read(pa, bytes) {
            return None;
        }
        let mut w = [0u32; N];
        for (w, b) in w.iter_mut().zip(bytes.as_chunks::<4>().0) {
            *w = u32::from_le_bytes(*b);
        }
        Some(w)
    }

    fn write_words(&self, pa: u32, words: &[u32]) {
        let mut bytes = [0u8; 32];
        let bytes = &mut bytes[..4 * words.len()];
        for (b, w) in bytes.as_chunks_mut::<4>().0.iter_mut().zip(words) {
            b.copy_from_slice(&w.to_le_bytes());
        }
        self.mem.write(pa, bytes);
    }

    /// Copies a TD's buffer (`cbp` up to `be`, possibly crossing into `be`'s
    /// page) from the guest into `buf`, or `buf` to the guest.
    fn td_copy(&self, cbp: u32, be: u32, buf: &mut [u8], to_guest: bool) {
        let len = buf.len();
        let first =
            if (cbp ^ be) & !0xfff != 0 { len.min(0x1000 - (cbp & 0xfff) as usize) } else { len };
        let (a, b) = buf.split_at_mut(first);
        if to_guest {
            self.mem.write(cbp, a);
            if !b.is_empty() {
                self.mem.write(be & !0xfff, b);
            }
        } else {
            self.mem.read(cbp, a);
            if !b.is_empty() {
                self.mem.read(be & !0xfff, b);
            }
        }
    }

    /// Services the head TD of an ED. Returns false if the ED is waiting
    /// (NAK) or halted.
    fn service_td(&mut self, ed_pa: u32, ed: &mut [u32; 4]) -> bool {
        let td_pa = ed[2] & PTR_MASK;
        let Some(mut td) = self.read_words::<4>(td_pa) else {
            ed[2] |= ED_H;
            return false;
        };
        let pid = match (ed[0] >> 11) & 3 {
            1 => PID_OUT,
            2 => PID_IN,
            _ => (td[0] >> 19) & 3,
        };
        let len = td_len(td[1], td[3]);
        if len as usize > TD_BUFFER {
            ed[2] |= ED_H;
            return false;
        }
        let mut storage = [0u8; TD_BUFFER];
        let buf = &mut storage[..len as usize];
        if pid != PID_IN && len != 0 {
            self.td_copy(td[1], td[3], buf, false);
        }

        let addr = (ed[0] & 0x7f) as u8;
        let ep = ((ed[0] >> 7) & 0xf) as u8;
        let outcome = match find_device(&mut self.dev, &self.port, addr) {
            None => Outcome::NoDevice,
            Some(d) => {
                let r = if ep == 0 {
                    match pid {
                        PID_SETUP => match <&[u8; 8]>::try_from(&*buf) {
                            Ok(packet) => super::setup(d, packet),
                            Err(_) => Err(UsbError::Stall),
                        },
                        PID_IN => super::ep0_in(d, buf),
                        _ => super::ep0_out(d, buf),
                    }
                } else {
                    d.data(pid == PID_IN, ep, buf)
                };
                match r {
                    Ok(n) => Outcome::Moved((n as u32).min(len)),
                    Err(UsbError::Stall) => Outcome::Stall,
                    Err(UsbError::Nak) => Outcome::Nak,
                }
            }
        };
        if matches!(outcome, Outcome::Nak) {
            return false;
        }
        self.tds += 1;
        if log::log_enabled!(target: "usb", log::Level::Trace) {
            let (moved, shown) = match outcome {
                Outcome::Moved(n) => (i64::from(n), &buf[..(n as usize).min(8)]),
                Outcome::Stall => (-1, &buf[..0]),
                _ => (-100, &buf[..0]),
            };
            log::trace!(
                target: "usb",
                "td {td_pa:08x} addr {addr} ep {ep} {} len {len} -> {moved} {shown:02x?} (mps {}{})",
                ["SETUP", "OUT", "IN", "?"][pid as usize & 3],
                (ed[0] >> 16) & 0x7ff,
                if td[0] & TD_R != 0 { ", rounding" } else { "" }
            );
        }

        // Whatever moved, CBP moves past it: all of it clears CBP; a short
        // packet leaves CBP at the first byte that didn't arrive (the host
        // driver counts a short transfer from it, error or not).
        if let Outcome::Moved(r) = outcome {
            if pid == PID_IN && r > 0 {
                self.td_copy(td[1], td[3], &mut buf[..r as usize], true);
            }
            if r == len {
                td[1] = 0;
            } else {
                let first =
                    if (td[1] ^ td[3]) & !0xfff != 0 { 0x1000 - (td[1] & 0xfff) } else { len };
                td[1] = if r < first { td[1] + r } else { (td[3] & !0xfff) + (r - first) };
            }
        }
        let code = match outcome {
            Outcome::Moved(r) if r == len || (pid == PID_IN && td[0] & TD_R != 0) => {
                td[0] |= TD_T1;
                td[0] ^= TD_T0;
                ed[2] &= !ED_C;
                if td[0] & TD_T0 != 0 {
                    ed[2] |= ED_C;
                }
                cc::NO_ERROR
            }
            other => {
                ed[2] |= ED_H;
                match other {
                    Outcome::Moved(_) => cc::DATA_UNDERRUN,
                    Outcome::Stall => cc::STALL,
                    _ => cc::DEVICE_NOT_RESPONDING,
                }
            }
        };
        td[0] = (td[0] & !(TD_EC_MASK | (0xf << TD_CC_SHIFT))) | (code << TD_CC_SHIFT);

        // Retire: off the ED, onto the done queue.
        ed[2] = (td[2] & PTR_MASK) | (ed[2] & (ED_H | ED_C));
        td[2] = self.done;
        self.done = td_pa;
        let di = (td[0] >> 21) & 7;
        self.done_count = self.done_count.min(di);
        if code != cc::NO_ERROR {
            self.done_count = 0; // errors are reported at once
        }
        self.write_words(td_pa, &td);
        self.write_words(ed_pa + 8, &ed[2..3]);
        ed[2] & ED_H == 0
    }

    /// Walks an ED list; returns whether any ED had TDs queued.
    fn service_list(&mut self, head: u32, periodic: bool) -> bool {
        let mut active = false;
        let mut pa = head & PTR_MASK;
        for _ in 0..256 {
            if pa == 0 {
                break;
            }
            let Some(mut ed) = self.read_words::<4>(pa) else {
                break;
            };
            // Skipped (K) and isochronous (F) descriptors are passed over.
            if ed[0] & (ED_K | ED_F) == 0 {
                let mut tds = 0;
                while ed[2] & ED_H == 0 && (ed[2] & PTR_MASK) != (ed[1] & PTR_MASK) && tds < 64 {
                    active = true;
                    tds += 1;
                    if !self.service_td(pa, &mut ed) {
                        break;
                    }
                    if periodic {
                        break; // one packet per interrupt endpoint per frame
                    }
                }
            }
            pa = ed[3] & PTR_MASK;
        }
        active
    }

    fn frame(&mut self) {
        self.frames += 1;
        if self.frames.is_multiple_of(10_000) {
            // The stack's health every 10 s: a title whose USB stops polling
            // (a dead controller, a stalled endpoint) reads no input.
            log::debug!(
                target: "usb",
                "ohci: {} TDs in 10 s, ctl {:08x}, hcca {:08x}, intr {:08x}/{:08x}",
                self.tds - self.tds_logged, self.ctl, self.hcca, self.intr_status, self.intr
            );
            self.tds_logged = self.tds;
        }
        self.frame_number = self.frame_number.wrapping_add(1);
        let hcca = u64::from(self.hcca) + 256 <= u64::from(self.mem.size());
        if hcca {
            self.write_words(self.hcca + 0x80, &[u32::from(self.frame_number)]);
        }
        if self.frame_number & 0x7fff == 0 {
            self.intr_status |= interrupt::FNO;
        }

        if self.ctl & control::PLE != 0 && hcca {
            let head = self.mem.read_u32(self.hcca + 4 * u32::from(self.frame_number & 31));
            self.service_list(head, true);
        }
        if self.ctl & control::CLE != 0
            && self.status & command::CLF != 0
            && !self.service_list(self.ctrl_head, false)
        {
            self.ctrl_cur = 0;
            self.status &= !command::CLF;
        }
        if self.ctl & control::BLE != 0
            && self.status & command::BLF != 0
            && !self.service_list(self.bulk_head, false)
        {
            self.bulk_cur = 0;
            self.status &= !command::BLF;
        }

        // The done queue goes out when its delay runs down and the last one
        // was taken.
        if self.done != 0 && self.done_count == 0 && self.intr_status & interrupt::WDH == 0 && hcca
        {
            let mut head = self.done;
            if self.intr_status & self.intr & !interrupt::WDH != 0 {
                head |= 1;
            }
            self.mem.write_u32(self.hcca + 0x84, head);
            self.done = 0;
            self.done_count = 7;
            self.intr_status |= interrupt::WDH;
        }
        if self.done_count != 7 && self.done_count != 0 {
            self.done_count -= 1;
        }
        self.intr_status |= interrupt::SF;
        self.update_irq();
    }
}

/// The root port a `HcRhPortStatus` register offset names.
fn root_port_index(offset: u32) -> Option<usize> {
    let i = offset.checked_sub(reg::RH_PORT_STATUS)? as usize / 4;
    (i < ROOT_PORTS).then_some(i)
}

/// The device with this address behind an enabled, unsuspended root port.
fn find_device<'a>(
    dev: &'a mut [Option<Box<dyn UsbDevice>>; ROOT_PORTS],
    port: &[u32; ROOT_PORTS],
    addr: u8,
) -> Option<&'a mut dyn UsbDevice> {
    for (d, &status) in dev.iter_mut().zip(port) {
        if status & port::PES == 0 || status & port::PSS != 0 {
            continue;
        }
        if let Some(d) = d.as_deref_mut()
            && let Some(found) = super::find(d, addr)
        {
            return Some(found);
        }
    }
    None
}

/// A TD's buffer length: CBP..BE inclusive, possibly crossing into BE's page.
fn td_len(cbp: u32, be: u32) -> u32 {
    if cbp == 0 {
        0
    } else if (cbp ^ be) & !0xfff != 0 {
        (0x1000 - (cbp & 0xfff)) + (be & 0xfff) + 1
    } else {
        be.wrapping_sub(cbp).wrapping_add(1)
    }
}

/// The controller ports in `mask`, for the log.
fn port_list(mask: u8) -> String {
    let ports: Vec<String> =
        (0..PORTS).filter(|p| mask >> p & 1 != 0).map(|p| p.to_string()).collect();
    if ports.is_empty() { "(none)".into() } else { ports.join(", ") }
}

impl xb_types::state::Stateful for Ohci {
    /// [`Ohci::state_as`] in the layout this build writes.
    fn state(&mut self, io: &mut xb_types::state::StateIo) {
        self.state_as(io, Self::STATE_LAYOUT);
    }
}

impl Ohci {
    /// The layout [`Ohci::state_as`] writes: 2 keeps each controller port's
    /// devices with whether they were plugged in; 1 kept only the devices
    /// that were there, slot by slot.
    pub const STATE_LAYOUT: u32 = 2;

    /// The controller's registers and frame timing, and each controller
    /// port's devices with whether they were plugged in. A state loads
    /// whatever is plugged in now: the title then sees the difference as
    /// pads plugged or pulled. `layout` is the one the stream was written
    /// in (loading), [`Ohci::STATE_LAYOUT`] to save.
    pub fn state_as(&mut self, io: &mut xb_types::state::StateIo, layout: u32) {
        use xb_types::state::Stateful;
        macro_rules! fields {
            ($($f:ident),*) => { $( self.$f.state(io); )* };
        }
        fields!(
            ctl,
            status,
            intr_status,
            intr,
            hcca,
            ctrl_head,
            ctrl_cur,
            bulk_head,
            bulk_cur,
            per_cur,
            done,
            done_count,
            fi,
            fsmps,
            fit,
            pstart,
            lst,
            frame_number,
            frame_t0_ns,
            next_frame_ns,
            rhdesc_a,
            rhdesc_b,
            rhstatus,
            port,
            irq_level,
            frames,
            tds
        );
        if layout < 2 {
            self.devices_v1(io);
            return;
        }
        // The front end's own plugging follows at the next frame
        // (`sync_pads`), as a pad plugged or pulled while the title runs.
        for (p, &root) in ROOT_PORT_OF_PAD.iter().enumerate() {
            let i = root - 1;
            let mut plugged = self.dev[i].is_some();
            plugged.state(io);
            let mut dev = self.dev[i].take().or_else(|| self.parked[p].take());
            if let Some(dev) = dev.as_deref_mut() {
                dev.state(io);
            }
            if plugged {
                self.dev[i] = dev;
            } else {
                self.parked[p] = dev;
            }
        }
        self.told_of_state(io);
    }

    /// After a load: the ports the state had controllers on keep them,
    /// whatever the host has plugged in (the controllers are the console's,
    /// the host's only steer them). One the host has and the state had not
    /// is plugged in at the next frame, as a pad plugged while the title
    /// runs.
    fn told_of_state(&self, io: &xb_types::state::StateIo) {
        if !io.loading() || io.failed().is_some() {
            return;
        }
        let had = ROOT_PORT_OF_PAD
            .iter()
            .enumerate()
            .fold(0u8, |m, (p, &root)| m | u8::from(self.dev[root - 1].is_some()) << p);
        let now = self.pads.connected();
        for p in 0..PORTS {
            if had >> p & 1 != 0 {
                self.pads.set_connected(p, true);
            }
        }
        if had != now {
            log::info!(
                target: "usb",
                "ohci: the state has controllers on ports {} (the host had {}): they stay",
                port_list(had),
                port_list(now)
            );
        }
    }

    /// Layout 1's devices, read only: the root ports in order and then the
    /// parked ones, each a flag and, if there, its state. A pad's devices
    /// are put where the state had them.
    fn devices_v1(&mut self, io: &mut xb_types::state::StateIo) {
        use xb_types::state::Stateful;
        if !io.loading() {
            io.fail("layout 1 of the USB state is only read");
            return;
        }
        let pad_of_root = |i: usize| ROOT_PORT_OF_PAD.iter().position(|&root| root - 1 == i);
        for i in 0..ROOT_PORTS {
            let mut there = false;
            there.state(io);
            let Some(p) = pad_of_root(i).filter(|_| there) else { continue };
            let mut dev = self.dev[i].take().or_else(|| self.parked[p].take());
            if let Some(dev) = dev.as_deref_mut() {
                dev.state(io);
            }
            self.dev[i] = dev;
        }
        for p in 0..PORTS {
            let mut there = false;
            there.state(io);
            if !there {
                continue;
            }
            let i = ROOT_PORT_OF_PAD[p] - 1;
            let mut dev = self.parked[p].take().or_else(|| self.dev[i].take());
            if let Some(dev) = dev.as_deref_mut() {
                dev.state(io);
            }
            self.parked[p] = dev;
        }
        self.told_of_state(io);
    }
}

#[cfg(test)]
impl Ohci {
    /// For the tests beside this module: whether controller port `p`'s
    /// devices are plugged in, and whether they are parked.
    pub(super) fn pad_places(&self, p: usize) -> (bool, bool) {
        (self.dev[ROOT_PORT_OF_PAD[p] - 1].is_some(), self.parked[p].is_some())
    }

    /// For the tests: the plugging brought to what the front end says.
    pub(super) fn sync_pads_now(&mut self) {
        self.sync_pads();
    }
}
