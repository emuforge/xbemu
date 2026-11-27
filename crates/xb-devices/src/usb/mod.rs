//! USB device side: the device model, endpoint 0 control transfers and the
//! standard requests (USB 1.1, chapter 9).
//!
//! A device answers its standard requests from descriptor tables held in its
//! [`DeviceCore`]; class and vendor requests go to [`UsbDevice::control`],
//! the other endpoints to [`UsbDevice::data`]. A hub also finds the devices
//! downstream of it. The host controller ([`ohci`]) moves packets by calling
//! [`setup`], [`ep0_in`] and [`ep0_out`] for endpoint 0 and
//! [`UsbDevice::data`] for the rest.

pub mod hub;
pub mod ohci;
pub mod xid;

#[cfg(test)]
mod tests;

/// Why a device moved no data for a packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsbError {
    /// The request is refused; the endpoint halts.
    Stall,
    /// Nothing to send yet; the host tries again in a later frame.
    Nak,
}

/// Bytes moved by a packet (0 for an empty one), or why none were.
pub type UsbResult = Result<usize, UsbError>;

/// Size of the endpoint 0 transfer buffer, the most data one control
/// transfer carries.
pub const EP0_BUFFER: usize = 512;

const REQ_GET_STATUS: u8 = 0x00;
const REQ_CLEAR_FEATURE: u8 = 0x01;
const REQ_SET_FEATURE: u8 = 0x03;
const REQ_SET_ADDRESS: u8 = 0x05;
const REQ_GET_DESCRIPTOR: u8 = 0x06;
const REQ_GET_CONFIGURATION: u8 = 0x08;
const REQ_SET_CONFIGURATION: u8 = 0x09;
const REQ_GET_INTERFACE: u8 = 0x0A;
const REQ_SET_INTERFACE: u8 = 0x0B;

/// The state every USB device has: its address, its descriptors and the
/// endpoint 0 transfer in progress.
pub struct DeviceCore {
    /// Name for the log.
    pub name: &'static str,
    /// Bus address; 0 until `SET_ADDRESS`.
    pub addr: u8,
    /// The `SET_CONFIGURATION` value; 0 while unconfigured.
    pub config: u8,
    /// Whether the device is a low-speed one (the port reports it).
    pub low_speed: bool,
    dev_desc: &'static [u8],
    cfg_desc: &'static [u8],
    /// String descriptors 1.. (index 0 is the language table).
    strings: &'static [&'static str],

    setup: [u8; 8],
    buf: [u8; EP0_BUFFER],
    len: usize,
    pos: usize,
    is_in: bool,
    data_stage: bool,
    stalled: bool,
    pending_addr: Option<u8>,
}

impl DeviceCore {
    /// A device at address 0 with these descriptors.
    #[must_use]
    pub fn new(
        name: &'static str,
        dev_desc: &'static [u8],
        cfg_desc: &'static [u8],
        strings: &'static [&'static str],
    ) -> Self {
        Self {
            name,
            addr: 0,
            config: 0,
            low_speed: false,
            dev_desc,
            cfg_desc,
            strings,
            setup: [0; 8],
            buf: [0; EP0_BUFFER],
            len: 0,
            pos: 0,
            is_in: false,
            data_stage: false,
            stalled: false,
            pending_addr: None,
        }
    }
}

/// A USB device behind the host controller.
pub trait UsbDevice: Send {
    /// The device's common state.
    fn core(&self) -> &DeviceCore;
    /// The device's common state, mutable.
    fn core_mut(&mut self) -> &mut DeviceCore;
    /// A class or vendor request. For an IN request `data` is the space for
    /// the answer (`wLength` bytes, at most [`EP0_BUFFER`]) and the result is
    /// the answer's length; for an OUT request `data` holds the bytes the
    /// host sent and the result is `Ok(0)`.
    fn control(&mut self, setup: &[u8; 8], data: &mut [u8]) -> UsbResult;
    /// A packet on endpoint `ep` (1-15). IN fills `buf` and returns the bytes
    /// written; OUT returns the bytes taken from `buf`.
    fn data(&mut self, is_in: bool, ep: u8, buf: &mut [u8]) -> UsbResult;
    /// Hubs: the downstream device with this address (after a port reset, the
    /// one at address 0).
    fn find_downstream(&mut self, _addr: u8) -> Option<&mut dyn UsbDevice> {
        None
    }
    /// A bus reset reached the device (its address and configuration are
    /// already cleared).
    fn on_reset(&mut self) {}

    /// The device's part of a savestate: its own state and that of the
    /// devices below it, which stay the ones attached (a state does not
    /// plug or unplug anything).
    fn state(&mut self, io: &mut xb_types::state::StateIo);
}

xb_types::state_fields!(DeviceCore {
    addr,
    config,
    setup,
    buf,
    len,
    pos,
    is_in,
    data_stage,
    stalled,
    pending_addr
});

fn string_desc(core: &mut DeviceCore, index: usize) -> UsbResult {
    if index == 0 {
        core.buf[..4].copy_from_slice(&[4, 3, 0x09, 0x04]); // en-US
        return Ok(4);
    }
    let s = core.strings.get(index - 1).ok_or(UsbError::Stall)?;
    let mut n = 2;
    for &c in s.as_bytes() {
        if n + 2 > EP0_BUFFER {
            break;
        }
        core.buf[n] = c;
        core.buf[n + 1] = 0;
        n += 2;
    }
    core.buf[0] = n as u8;
    core.buf[1] = 3;
    Ok(n)
}

/// A standard request; `None` if `setup` is not one (class, vendor or
/// unknown). IN results go to the core's buffer.
fn standard(core: &mut DeviceCore, setup: &[u8; 8]) -> Option<UsbResult> {
    let (kind, req) = (setup[0], setup[1]);
    let value = u16::from_le_bytes([setup[2], setup[3]]);
    if kind & 0x60 != 0 {
        return None;
    }
    Some(match req {
        REQ_GET_DESCRIPTOR => {
            if kind != 0x80 {
                return None;
            }
            match value >> 8 {
                1 => {
                    let d = core.dev_desc;
                    core.buf[..d.len()].copy_from_slice(d);
                    Ok(d.len())
                }
                2 => {
                    let d = core.cfg_desc;
                    core.buf[..d.len()].copy_from_slice(d);
                    Ok(d.len())
                }
                3 => string_desc(core, usize::from(value & 0xff)),
                _ => return None,
            }
        }
        REQ_SET_ADDRESS => {
            // Takes effect after the status stage.
            core.pending_addr = Some((value & 0x7f) as u8);
            Ok(0)
        }
        REQ_SET_CONFIGURATION => {
            core.config = value as u8;
            log::info!(target: "usb", "{}@{}: configuration {}", core.name, core.addr, core.config);
            Ok(0)
        }
        REQ_GET_CONFIGURATION => {
            core.buf[0] = core.config;
            Ok(1)
        }
        REQ_GET_STATUS => {
            core.buf[..2].fill(0);
            Ok(2)
        }
        REQ_GET_INTERFACE => {
            core.buf[0] = 0;
            Ok(1)
        }
        REQ_SET_INTERFACE | REQ_CLEAR_FEATURE | REQ_SET_FEATURE => Ok(0),
        _ => return None,
    })
}

/// Runs the request in the core's setup packet: a standard one from the
/// descriptor tables, anything else through [`UsbDevice::control`] with
/// `len` bytes of the transfer buffer.
fn run_request(d: &mut dyn UsbDevice, len: usize) -> UsbResult {
    let setup = d.core().setup;
    if let Some(r) = standard(d.core_mut(), &setup) {
        return r;
    }
    let mut data = d.core().buf;
    let r = d.control(&setup, &mut data[..len]);
    d.core_mut().buf = data;
    r
}

/// The setup stage of a control transfer. Requests without OUT data run now;
/// IN data is ready for the data stage. Returns the 8 bytes taken.
pub fn setup(d: &mut dyn UsbDevice, packet: &[u8; 8]) -> UsbResult {
    let wlen = usize::from(u16::from_le_bytes([packet[6], packet[7]]));
    let core = d.core_mut();
    core.setup = *packet;
    core.is_in = packet[0] & 0x80 != 0;
    core.pos = 0;
    core.len = 0;
    core.stalled = false;
    core.pending_addr = None;
    core.data_stage = wlen > 0;
    if core.is_in || wlen == 0 {
        match run_request(d, wlen.min(EP0_BUFFER)) {
            Ok(n) => d.core_mut().len = n.min(wlen),
            Err(_) => {
                d.core_mut().stalled = true;
                return Err(UsbError::Stall);
            }
        }
    }
    Ok(8)
}

/// An IN packet on endpoint 0: the next part of an IN request's data, or the
/// status stage of an OUT request (which runs it, with its data, now).
pub fn ep0_in(d: &mut dyn UsbDevice, buf: &mut [u8]) -> UsbResult {
    let core = d.core_mut();
    if core.stalled {
        return Err(UsbError::Stall);
    }
    if core.is_in && core.data_stage {
        let n = buf.len().min(core.len - core.pos);
        buf[..n].copy_from_slice(&core.buf[core.pos..core.pos + n]);
        core.pos += n;
        return Ok(n);
    }
    if !core.is_in && core.data_stage {
        let len = core.len;
        if run_request(d, len).is_err() {
            d.core_mut().stalled = true;
            return Err(UsbError::Stall);
        }
    }
    let core = d.core_mut();
    if let Some(addr) = core.pending_addr.take() {
        core.addr = addr;
        log::info!(target: "usb", "{}: address {}", core.name, core.addr);
    }
    Ok(0)
}

/// An OUT packet on endpoint 0: the next part of an OUT request's data, or
/// the status stage of an IN request.
pub fn ep0_out(d: &mut dyn UsbDevice, buf: &[u8]) -> UsbResult {
    let core = d.core_mut();
    if core.stalled {
        return Err(UsbError::Stall);
    }
    if !core.is_in && core.data_stage {
        let n = buf.len().min(EP0_BUFFER - core.len);
        core.buf[core.len..core.len + n].copy_from_slice(&buf[..n]);
        core.len += n;
        return Ok(n);
    }
    Ok(0)
}

/// A port reset: address 0, unconfigured.
pub fn reset(d: &mut dyn UsbDevice) {
    let core = d.core_mut();
    core.addr = 0;
    core.config = 0;
    core.stalled = false;
    core.pending_addr = None;
    d.on_reset();
}

/// `d`, or a device below it, with this address.
pub fn find(d: &mut dyn UsbDevice, addr: u8) -> Option<&mut dyn UsbDevice> {
    if d.core().addr == addr {
        return Some(d);
    }
    d.find_downstream(addr)
}
