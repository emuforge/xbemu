//! A USB 1.1 hub (USB 1.1 chapter 11): the one inside each Xbox controller,
//! with the gamepad on its port 1 and the two expansion slots on ports 2-3.
//!
//! Its descriptors and port behaviour are those of the hub the Xbox USB stack
//! is known to enumerate (QEMU's usb-hub, as xemu attaches it): no power
//! switching, so ports are powered and report connections from reset.

use super::{DeviceCore, UsbDevice, UsbError, UsbResult};

/// Port status bits (`wPortStatus`).
pub mod port_status {
    /// A device is connected.
    pub const CONNECTION: u16 = 0x0001;
    /// The port is enabled (after a port reset).
    pub const ENABLE: u16 = 0x0002;
    /// The port is suspended.
    pub const SUSPEND: u16 = 0x0004;
    /// The port is in reset.
    pub const RESET: u16 = 0x0010;
    /// The port is powered.
    pub const POWER: u16 = 0x0100;
    /// The connected device is low speed.
    pub const LOW_SPEED: u16 = 0x0200;
}

/// Port change bits (`wPortChange`).
pub mod port_change {
    /// The connection changed.
    pub const CONNECTION: u16 = 0x0001;
    /// The port was disabled by an error.
    pub const ENABLE: u16 = 0x0002;
    /// Resume completed.
    pub const SUSPEND: u16 = 0x0004;
    /// The over-current indicator changed.
    pub const OVER_CURRENT: u16 = 0x0008;
    /// Reset completed.
    pub const RESET: u16 = 0x0010;
}

/// Port feature selectors (`wValue` of `SET_FEATURE` / `CLEAR_FEATURE`).
pub mod feature {
    /// `PORT_ENABLE`.
    pub const PORT_ENABLE: u16 = 1;
    /// `PORT_SUSPEND`.
    pub const PORT_SUSPEND: u16 = 2;
    /// `PORT_RESET`.
    pub const PORT_RESET: u16 = 4;
    /// `PORT_POWER`.
    pub const PORT_POWER: u16 = 8;
    /// `C_PORT_CONNECTION`.
    pub const C_PORT_CONNECTION: u16 = 16;
    /// `C_PORT_ENABLE`.
    pub const C_PORT_ENABLE: u16 = 17;
    /// `C_PORT_SUSPEND`.
    pub const C_PORT_SUSPEND: u16 = 18;
    /// `C_PORT_OVER_CURRENT`.
    pub const C_PORT_OVER_CURRENT: u16 = 19;
    /// `C_PORT_RESET`.
    pub const C_PORT_RESET: u16 = 20;
}

/// Most downstream ports a hub has.
pub const MAX_PORTS: usize = 4;

static DEVICE_DESCRIPTOR: [u8; 18] = [
    18, 1, 0x10, 0x01, // USB 1.1
    9, 0, 0, // hub class
    8, // max packet 0
    0x09, 0x04, 0xaa, 0x55, // 0409:55aa
    0x01, 0x01, 1, 2, 3, 1,
];

static CONFIG_DESCRIPTOR: [u8; 25] = [
    9, 2, 25, 0, 1, 1, 0, 0xE0, 0, // config: self powered, remote wakeup
    9, 4, 0, 0, 1, 9, 0, 0, 0, // interface: hub class, one endpoint
    7, 5, 0x81, 0x03, 2, 0, 255, // status change: interrupt IN
];

static STRINGS: [&str; 3] = ["xbemu", "USB Hub", "1"];

#[derive(Default)]
struct Port {
    child: Option<Box<dyn UsbDevice>>,
    status: u16,
    change: u16,
}

/// A hub with up to [`MAX_PORTS`] downstream ports, numbered from 1.
pub struct Hub {
    core: DeviceCore,
    ports: Vec<Port>,
}

impl Hub {
    /// A hub with `nports` empty downstream ports.
    #[must_use]
    pub fn new(nports: usize) -> Self {
        let mut ports = Vec::new();
        ports.resize_with(nports.min(MAX_PORTS), Port::default);
        let mut hub = Self {
            core: DeviceCore::new("hub", &DEVICE_DESCRIPTOR, &CONFIG_DESCRIPTOR, &STRINGS),
            ports,
        };
        hub.on_reset();
        hub
    }

    /// Puts `child` on downstream port `port` (1-based) and resets the hub's
    /// ports, so every connection is reported as new.
    pub fn attach(&mut self, port: usize, child: Box<dyn UsbDevice>) {
        if let Some(p) = port.checked_sub(1).and_then(|i| self.ports.get_mut(i)) {
            p.child = Some(child);
            self.on_reset();
        }
    }

    /// Port `index` (1-based, from a request's `wIndex`).
    fn port(&mut self, index: u16) -> Result<&mut Port, UsbError> {
        usize::from(index).checked_sub(1).and_then(|i| self.ports.get_mut(i)).ok_or(UsbError::Stall)
    }
}

/// Copies as much of `src` as fits into `data`; the length copied.
fn answer(data: &mut [u8], src: &[u8]) -> usize {
    let n = data.len().min(src.len());
    data[..n].copy_from_slice(&src[..n]);
    n
}

impl UsbDevice for Hub {
    fn state(&mut self, io: &mut xb_types::state::StateIo) {
        use xb_types::state::Stateful;
        self.core.state(io);
        for port in &mut self.ports {
            port.status.state(io);
            port.change.state(io);
            let mut child = port.child.is_some();
            child.state(io);
            match &mut port.child {
                Some(dev) if child => dev.state(io),
                None if !child => {}
                _ => io.fail("the state has other gamepads plugged in than this session"),
            }
        }
    }

    fn core(&self) -> &DeviceCore {
        &self.core
    }

    fn core_mut(&mut self) -> &mut DeviceCore {
        &mut self.core
    }

    fn control(&mut self, setup: &[u8; 8], data: &mut [u8]) -> UsbResult {
        let value = u16::from_le_bytes([setup[2], setup[3]]);
        let index = u16::from_le_bytes([setup[4], setup[5]]);
        match (setup[0], setup[1]) {
            // GET_DESCRIPTOR (hub)
            (0xA0, 0x06) => {
                let desc = [
                    9,
                    0x29,
                    self.ports.len() as u8,
                    0x0a, // no power switching, per-port over-current
                    0x00,
                    1, // power on to power good, 2 ms units
                    0,
                    0x00, // removable devices
                    0xFF,
                ];
                Ok(answer(data, &desc))
            }
            // GET_STATUS (hub)
            (0xA0, 0x00) => Ok(answer(data, &[0; 4])),
            // GET_STATUS (port)
            (0xA3, 0x00) => {
                let p = self.port(index)?;
                let word = u32::from(p.status) | u32::from(p.change) << 16;
                Ok(answer(data, &word.to_le_bytes()))
            }
            // CLEAR_FEATURE / SET_FEATURE (port)
            (0x23, req @ (0x01 | 0x03)) => {
                let set = req == 0x03;
                let p = self.port(index)?;
                match value {
                    feature::PORT_ENABLE => {
                        if !set {
                            p.status &= !port_status::ENABLE;
                        }
                    }
                    feature::PORT_SUSPEND => {
                        if set {
                            p.status |= port_status::SUSPEND;
                        } else {
                            p.status &= !port_status::SUSPEND;
                        }
                    }
                    feature::PORT_RESET => {
                        if set && let Some(child) = p.child.as_deref_mut() {
                            super::reset(child);
                            p.status |= port_status::ENABLE;
                            p.change |= port_change::RESET;
                        }
                    }
                    feature::PORT_POWER => {
                        if set {
                            p.status |= port_status::POWER;
                            if let Some(child) = &p.child
                                && p.status & port_status::CONNECTION == 0
                            {
                                p.status |= port_status::CONNECTION;
                                if child.core().low_speed {
                                    p.status |= port_status::LOW_SPEED;
                                }
                                p.change |= port_change::CONNECTION;
                            }
                        } else {
                            p.status &= !(port_status::POWER | port_status::ENABLE);
                        }
                    }
                    feature::C_PORT_CONNECTION => p.change &= !port_change::CONNECTION,
                    feature::C_PORT_ENABLE => p.change &= !port_change::ENABLE,
                    feature::C_PORT_SUSPEND => p.change &= !port_change::SUSPEND,
                    feature::C_PORT_OVER_CURRENT => p.change &= !port_change::OVER_CURRENT,
                    feature::C_PORT_RESET => p.change &= !port_change::RESET,
                    _ => {}
                }
                Ok(0)
            }
            // CLEAR_FEATURE / SET_FEATURE (hub)
            (0x20, 0x01 | 0x03) => Ok(0),
            _ => Err(UsbError::Stall),
        }
    }

    /// The status change endpoint: bit n for a change on port n.
    fn data(&mut self, is_in: bool, ep: u8, buf: &mut [u8]) -> UsbResult {
        if !is_in || ep != 1 || buf.is_empty() {
            return Err(UsbError::Stall);
        }
        let bits = self
            .ports
            .iter()
            .enumerate()
            .filter(|(_, p)| p.change != 0)
            .fold(0u8, |bits, (i, _)| bits | 1 << (i + 1));
        if bits == 0 {
            return Err(UsbError::Nak);
        }
        buf[0] = bits;
        Ok(1)
    }

    fn find_downstream(&mut self, addr: u8) -> Option<&mut dyn UsbDevice> {
        for p in &mut self.ports {
            if p.status & port_status::ENABLE == 0 || p.status & port_status::SUSPEND != 0 {
                continue;
            }
            if let Some(child) = p.child.as_deref_mut()
                && let Some(found) = super::find(child, addr)
            {
                return Some(found);
            }
        }
        None
    }

    fn on_reset(&mut self) {
        for p in &mut self.ports {
            p.status = port_status::POWER;
            p.change = 0;
            if let Some(child) = &p.child {
                p.status |= port_status::CONNECTION;
                if child.core().low_speed {
                    p.status |= port_status::LOW_SPEED;
                }
                p.change |= port_change::CONNECTION;
            }
        }
    }
}
