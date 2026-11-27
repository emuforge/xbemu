//! The Xbox gamepad (Duke / Controller S): an XID device (class 0x58) with a
//! 20-byte input report on interrupt IN 2 and the rumble report on interrupt
//! OUT 2.
//!
//! The report layouts live in [`report`] and nowhere else.
//!
//! Input enters through [`PadPorts`], a handle the front end keeps: it holds
//! each port's [`PadState`] and the rumble strengths the title last sent.

use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use xb_types::pad::{PORTS, PadState, Rumble};

use super::{DeviceCore, UsbDevice, UsbError, UsbResult};

/// The XID report layouts: the one place that knows them.
///
/// Input report (device to host, interrupt IN endpoint 2 or `GET_REPORT`),
/// [`INPUT_LEN`](report::INPUT_LEN) bytes, little endian:
///
/// | Offset | Size | Field |
/// |---|---|---|
/// | 0 | 1 | report id, 0 |
/// | 1 | 1 | report length, 20 |
/// | 2 | 2 | digital buttons ([`xb_types::pad::button`] bits) |
/// | 4 | 8 | analog buttons: A, B, X, Y, black, white, left trigger, right trigger ([`xb_types::pad::analog`] order) |
/// | 12 | 2 | left stick X, signed |
/// | 14 | 2 | left stick Y, signed |
/// | 16 | 2 | right stick X, signed |
/// | 18 | 2 | right stick Y, signed |
///
/// Output report (host to device, interrupt OUT endpoint 2 or `SET_REPORT`),
/// [`OUTPUT_LEN`](report::OUTPUT_LEN) bytes:
///
/// | Offset | Size | Field |
/// |---|---|---|
/// | 0 | 1 | report id, 0 |
/// | 1 | 1 | report length, 6 |
/// | 2 | 2 | left (low frequency) motor speed |
/// | 4 | 2 | right (high frequency) motor speed |
pub mod report {
    use xb_types::pad::{PadState, Rumble};

    /// Length of the input report.
    pub const INPUT_LEN: usize = 20;
    /// Length of the output (rumble) report.
    pub const OUTPUT_LEN: usize = 6;

    /// The input report for `pad`.
    #[must_use]
    pub fn encode_input(pad: &PadState) -> [u8; INPUT_LEN] {
        let mut r = [0u8; INPUT_LEN];
        r[1] = INPUT_LEN as u8;
        r[2..4].copy_from_slice(&pad.buttons.to_le_bytes());
        r[4..12].copy_from_slice(&pad.analog);
        r[12..14].copy_from_slice(&pad.left_x.to_le_bytes());
        r[14..16].copy_from_slice(&pad.left_y.to_le_bytes());
        r[16..18].copy_from_slice(&pad.right_x.to_le_bytes());
        r[18..20].copy_from_slice(&pad.right_y.to_le_bytes());
        r
    }

    /// The pad state an input report carries; `None` if it is too short.
    /// The inverse of [`encode_input`].
    #[must_use]
    pub fn decode_input(r: &[u8]) -> Option<PadState> {
        let r: &[u8; INPUT_LEN] = r.get(..INPUT_LEN)?.try_into().ok()?;
        let word = |i: usize| [r[i], r[i + 1]];
        let mut analog = [0u8; 8];
        analog.copy_from_slice(&r[4..12]);
        Some(PadState {
            buttons: u16::from_le_bytes(word(2)),
            analog,
            left_x: i16::from_le_bytes(word(12)),
            left_y: i16::from_le_bytes(word(14)),
            right_x: i16::from_le_bytes(word(16)),
            right_y: i16::from_le_bytes(word(18)),
        })
    }

    /// The output report that sets `rumble`.
    #[must_use]
    pub fn encode_output(rumble: Rumble) -> [u8; OUTPUT_LEN] {
        let mut r = [0u8; OUTPUT_LEN];
        r[1] = OUTPUT_LEN as u8;
        r[2..4].copy_from_slice(&rumble.left.to_le_bytes());
        r[4..6].copy_from_slice(&rumble.right.to_le_bytes());
        r
    }

    /// The motor speeds an output report sets; `None` if it is too short.
    #[must_use]
    pub fn decode_output(r: &[u8]) -> Option<Rumble> {
        if r.len() < OUTPUT_LEN {
            return None;
        }
        Some(Rumble {
            left: u16::from_le_bytes([r[2], r[3]]),
            right: u16::from_le_bytes([r[4], r[5]]),
        })
    }
}

static DEVICE_DESCRIPTOR: [u8; 18] = [
    18, 1, 0x10, 0x01, // USB 1.1
    0, 0, 0, // class per interface
    0x40, 0x5e, 0x04, 0x02, 0x02, // Microsoft, controller (0x0202)
    0x00, 0x01, 1, 2, 3, 1,
];

static CONFIG_DESCRIPTOR: [u8; 32] = [
    9, 2, 32, 0, 1, 1, 0, 0x80, 50, //
    9, 4, 0, 0, 2, 0x58, 0x42, 0, 0, // XID interface
    7, 5, 0x82, 0x03, 0x20, 0, 4, // report: interrupt IN 2
    7, 5, 0x02, 0x03, 0x20, 0, 4, // rumble: interrupt OUT 2
];

/// The XID descriptor (`GET_DESCRIPTOR` 0x4200 on the interface).
static XID_DESCRIPTOR: [u8; 16] = [
    16,
    0x42,
    0x00,
    0x01, // XID 1.00
    1,
    1, // gamepad
    report::INPUT_LEN as u8,
    report::OUTPUT_LEN as u8,
    0xff,
    0xff,
    0xff,
    0xff,
    0xff,
    0xff,
    0xff,
    0xff,
];

static STRINGS: [&str; 3] = ["xbemu", "Microsoft Xbox Controller", "1"];

#[derive(Clone, Copy, Default)]
struct Slot {
    pad: PadState,
    rumble: Rumble,
}

struct Shared {
    slots: Mutex<[Slot; PORTS]>,
    /// Bit n: a controller should be plugged into port n.
    connected: AtomicU8,
    /// Bumped by [`PadPorts::resend`].
    epoch: AtomicU32,
}

/// The four controller ports as the front end sees them: pad state in,
/// rumble out, controllers plugged in and pulled out.
///
/// A cheap handle (`Clone`), usable from any thread. Ports are numbered from
/// 0 (player 1). The host controller picks changes up at its next frame.
#[derive(Clone)]
pub struct PadPorts(Arc<Shared>);

impl PadPorts {
    /// Ports with a controller plugged into each port whose bit is set in
    /// `connected` (bit 0: port 0).
    #[must_use]
    pub fn new(connected: u8) -> Self {
        Self(Arc::new(Shared {
            slots: Mutex::new([Slot::default(); PORTS]),
            connected: AtomicU8::new(connected & 0xF),
            epoch: AtomicU32::new(0),
        }))
    }

    fn slots(&self) -> MutexGuard<'_, [Slot; PORTS]> {
        self.0.slots.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Sets the state of the controller on `port` (0-3); the title reads it
    /// at its next poll. Ignored for other ports.
    pub fn set_pad(&self, port: usize, state: &PadState) {
        if let Some(slot) = self.slots().get_mut(port) {
            slot.pad = *state;
        }
    }

    /// The state last set for `port`.
    #[must_use]
    pub fn pad(&self, port: usize) -> PadState {
        self.slots().get(port).map(|s| s.pad).unwrap_or_default()
    }

    /// The motor speeds the title last sent to the controller on `port`.
    #[must_use]
    pub fn rumble(&self, port: usize) -> Rumble {
        self.slots().get(port).map(|s| s.rumble).unwrap_or_default()
    }

    /// Plugs a controller into `port` (0-3) or pulls it out. The title sees
    /// the port's connection change, as on the console.
    pub fn set_connected(&self, port: usize, connected: bool) {
        if port < PORTS {
            if connected {
                self.0.connected.fetch_or(1 << port, Ordering::AcqRel);
            } else {
                self.0.connected.fetch_and(!(1 << port), Ordering::AcqRel);
            }
        }
    }

    /// The mask of ports that should have a controller (bit 0: port 0).
    #[must_use]
    pub fn connected(&self) -> u8 {
        self.0.connected.load(Ordering::Acquire)
    }

    /// Makes every gamepad's next poll report, changed or not (after a
    /// savestate load: a pad otherwise sends only changes).
    pub fn resend(&self) {
        self.0.epoch.fetch_add(1, Ordering::AcqRel);
    }

    fn epoch(&self) -> u32 {
        self.0.epoch.load(Ordering::Acquire)
    }

    fn take_rumble(&self, port: usize, data: &[u8]) {
        if let Some(rumble) = report::decode_output(data) {
            self.slots()[port].rumble = rumble;
        }
    }
}

/// One gamepad: the device on port 1 of a controller's hub.
pub struct Gamepad {
    core: DeviceCore,
    ports: PadPorts,
    port: usize,
    /// The report last sent and the resend epoch then; `None` after a reset.
    last: Option<([u8; report::INPUT_LEN], u32)>,
    polled: bool,
}

impl Gamepad {
    /// The gamepad for controller port `port` (0-3), fed from `ports`.
    #[must_use]
    pub fn new(ports: PadPorts, port: usize) -> Self {
        Self {
            core: DeviceCore::new("gamepad", &DEVICE_DESCRIPTOR, &CONFIG_DESCRIPTOR, &STRINGS),
            ports,
            port: port % PORTS,
            last: None,
            polled: false,
        }
    }

    fn input_report(&self) -> [u8; report::INPUT_LEN] {
        report::encode_input(&self.ports.pad(self.port))
    }
}

/// Copies as much of `src` as fits into `data`; the length copied.
fn answer(data: &mut [u8], src: &[u8]) -> usize {
    let n = data.len().min(src.len());
    data[..n].copy_from_slice(&src[..n]);
    n
}

impl UsbDevice for Gamepad {
    fn state(&mut self, io: &mut xb_types::state::StateIo) {
        use xb_types::state::Stateful;
        self.core.state(io);
        self.polled.state(io);
        if io.loading() {
            self.last = None; // the next poll reports the pad as it is now
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
        match (setup[0], setup[1], value) {
            // HID GET_REPORT (input)
            (0xA1, 0x01, 0x0100) => Ok(answer(data, &self.input_report())),
            // HID SET_REPORT (output)
            (0x21, 0x09, 0x0200) => {
                self.ports.take_rumble(self.port, data);
                Ok(0)
            }
            // XID GET_DESCRIPTOR
            (0xC1, 0x06, 0x4200) => Ok(answer(data, &XID_DESCRIPTOR)),
            // XID GET_CAPABILITIES: every bit and axis is live.
            (0xC1, 0x01, 0x0100) => {
                let mut caps = [0xff; report::INPUT_LEN];
                caps[0] = 0;
                caps[1] = report::INPUT_LEN as u8;
                Ok(answer(data, &caps))
            }
            (0xC1, 0x01, 0x0200) => {
                let mut caps = [0xff; report::OUTPUT_LEN];
                caps[0] = 0;
                caps[1] = report::OUTPUT_LEN as u8;
                Ok(answer(data, &caps))
            }
            (kind, req, _) => {
                log::debug!(target: "usb", "xid: unhandled request {kind:02x} {req:02x} value {value:04x}");
                Err(UsbError::Stall)
            }
        }
    }

    fn data(&mut self, is_in: bool, ep: u8, buf: &mut [u8]) -> UsbResult {
        if ep != 2 {
            return Err(UsbError::Stall);
        }
        if !is_in {
            self.ports.take_rumble(self.port, buf);
            return Ok(buf.len());
        }
        if !self.polled {
            self.polled = true;
            log::info!(target: "usb", "gamepad {}: polled", self.port);
        }
        let r = self.input_report();
        // A pad answers its interrupt IN only when its state changed, else
        // NAKs and the request stays (the XDK's XINPUT_STATE: dwPacketNumber
        // moves only when the state does). Reported on every poll, the packet
        // number climbs with nothing touched, and a title waiting for it to
        // stand still (an attract mode, an idle timer) never sees it do so.
        // A bus reset or `PadPorts::resend` sends the next one.
        let epoch = self.ports.epoch();
        if self.last == Some((r, epoch)) {
            return Err(UsbError::Nak);
        }
        if let Some((last, _)) = self.last
            && last[2..4] != r[2..4]
        {
            log::debug!(
                target: "usb",
                "gamepad {}: buttons {:02x}{:02x} -> {:02x}{:02x}",
                self.port, last[3], last[2], r[3], r[2]
            );
        }
        self.last = Some((r, epoch));
        Ok(answer(buf, &r))
    }

    /// A bus reset: the next poll reports.
    fn on_reset(&mut self) {
        self.last = None;
    }
}
