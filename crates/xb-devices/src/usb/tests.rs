//! A small host-side OHCI driver and the tests that run it against the
//! controller, hub and gamepad, the way XAPI's USB stack does: HCCA,
//! endpoint and transfer descriptors in guest RAM, enumeration through the
//! control list, reports through the periodic list.

use std::sync::Arc;

use xb_types::irq::{Irq, IrqHub};
use xb_types::mem::PhysMem;
use xb_types::pad::{PadState, Rumble, analog, button};

use super::hub::{feature, port_change, port_status};
use super::ohci::{FRAME_NS, Ohci, ROOT_PORT_OF_PAD, cc, command, control, interrupt, port, reg};
use super::xid::{PadPorts, report};
use crate::testutil::{TestClock, TestRam};

const HCCA: u32 = 0x1000;
const HCCA_FRAME_NUMBER: u32 = HCCA + 0x80;
const HCCA_DONE_HEAD: u32 = HCCA + 0x84;
/// The control endpoint's ED; interrupt EDs follow at 16-byte steps.
const CONTROL_ED: u32 = 0x2000;
const INTERRUPT_EDS: u32 = 0x2100;
/// TDs are handed out from here, 16 bytes each.
const TD_POOL: u32 = 0x3000;
/// Transfer buffers are handed out from here. The first one is placed to
/// straddle a page boundary so two-page TDs are exercised.
const BUFFER_POOL: u32 = 0x8000 - 6;

const DIR_SETUP: u32 = 0;
const DIR_OUT: u32 = 1;
const DIR_IN: u32 = 2;

const TD_ROUNDING: u32 = 1 << 18;
const TD_NO_INTERRUPT: u32 = 7 << 21;
const TD_TOGGLE_DATA0: u32 = 2 << 24;
const TD_TOGGLE_DATA1: u32 = 3 << 24;
const TD_CC_NOT_ACCESSED: u32 = 0xF << 28;

/// A completed TD as the driver reads it off the done queue.
#[derive(Debug)]
struct Done {
    td: u32,
    cc: u32,
    /// Bytes the TD moved.
    moved: u32,
}

/// An interrupt endpoint the driver keeps a TD queued on.
struct Pipe {
    ed: u32,
    buffer: u32,
    len: u32,
    /// The ED's tail TD: the next one submitted.
    td: u32,
    /// The TD submitted and not yet seen done.
    pending: Option<u32>,
}

struct Driver {
    ohci: Ohci,
    ram: Arc<TestRam>,
    irq: Arc<IrqHub>,
    clock: Arc<TestClock>,
    pads: PadPorts,
    next_td: u32,
    next_buffer: u32,
    next_ed: u32,
    /// Start and length of each queued TD's buffer, to count what moved.
    queued: Vec<(u32, u32, u32)>,
    /// Completions taken off the done queue and not yet claimed.
    inbox: Vec<Done>,
}

impl Driver {
    /// A controller with pads on the ports in `connected`, reset and
    /// started the way a host controller driver does it.
    fn start(connected: u8) -> Self {
        let ram = TestRam::new(0x20000);
        let irq = Arc::new(IrqHub::new());
        let clock = TestClock::new();
        let pads = PadPorts::new(connected);
        let ohci = Ohci::new(ram.clone(), irq.clone(), clock.clone(), pads.clone());
        let mut d = Self {
            ohci,
            ram,
            irq,
            clock,
            pads,
            next_td: TD_POOL,
            next_buffer: BUFFER_POOL,
            next_ed: INTERRUPT_EDS,
            queued: Vec::new(),
            inbox: Vec::new(),
        };
        assert_eq!(d.read(reg::REVISION) & 0xFF, 0x10);
        d.write(reg::COMMAND_STATUS, command::HCR);
        assert_eq!(d.read(reg::COMMAND_STATUS) & command::HCR, 0, "the reset completes at once");
        let interval = d.read(reg::FM_INTERVAL);
        d.write(reg::FM_INTERVAL, interval);
        d.write(reg::HCCA, HCCA);
        d.write(reg::CONTROL_HEAD_ED, CONTROL_ED);
        // An empty control ED: head == tail.
        d.ram.write_u32(CONTROL_ED, 1 << 14);
        d.write(reg::INTERRUPT_DISABLE, u32::MAX);
        d.write(reg::INTERRUPT_ENABLE, interrupt::MIE | interrupt::WDH);
        d.write(reg::CONTROL, control::HCFS_OPERATIONAL | control::PLE | control::CLE);
        d
    }

    fn read(&mut self, r: u32) -> u32 {
        self.ohci.mmio_read(r, 4)
    }

    fn write(&mut self, r: u32, v: u32) {
        self.ohci.mmio_write(r, 4, v);
    }

    /// One millisecond of guest time.
    fn frame(&mut self) {
        let now = self.clock.step(FRAME_NS);
        self.ohci.advance(now);
    }

    fn alloc_buffer(&mut self, len: u32) -> u32 {
        let pa = self.next_buffer;
        self.next_buffer += len.max(1);
        pa
    }

    /// Writes a general TD and returns its address.
    fn td(&mut self, flags: u32, buffer: u32, len: u32, next: u32) -> u32 {
        let pa = self.next_td;
        self.next_td += 16;
        let (cbp, be) = if len == 0 { (0, 0) } else { (buffer, buffer + len - 1) };
        for (i, w) in [flags | TD_CC_NOT_ACCESSED, cbp, next, be].into_iter().enumerate() {
            self.ram.write_u32(pa + 4 * i as u32, w);
        }
        self.queued.retain(|q| q.0 != pa);
        self.queued.push((pa, buffer, len));
        pa
    }

    /// Rewrites the TD at `pa` (an ED's old tail) as a transfer.
    fn fill_td(&mut self, pa: u32, flags: u32, buffer: u32, len: u32, next: u32) {
        for (i, w) in
            [flags | TD_CC_NOT_ACCESSED, buffer, next, buffer + len - 1].into_iter().enumerate()
        {
            self.ram.write_u32(pa + 4 * i as u32, w);
        }
        self.queued.retain(|q| q.0 != pa);
        self.queued.push((pa, buffer, len));
    }

    /// Takes the done queue if the controller wrote it back: checks the
    /// interrupt line, reads the TDs (oldest first) and acknowledges.
    fn take_done(&mut self) -> Vec<Done> {
        if self.read(reg::INTERRUPT_STATUS) & interrupt::WDH == 0 {
            return Vec::new();
        }
        assert_ne!(self.irq.pending() & Irq::Usb0.mask(), 0, "WDH is enabled: the line is up");
        let mut done = Vec::new();
        let mut td = self.ram.read_u32(HCCA_DONE_HEAD) & !0xF;
        while td != 0 {
            let flags = self.ram.read_u32(td);
            let cbp = self.ram.read_u32(td + 4);
            let (_, buffer, len) =
                *self.queued.iter().find(|q| q.0 == td).expect("a TD the driver queued");
            let moved = if cbp == 0 { len } else { self.offset_in_buffer(buffer, len, cbp) };
            done.push(Done { td, cc: flags >> 28, moved });
            td = self.ram.read_u32(td + 8) & !0xF;
        }
        done.reverse();
        self.write(reg::INTERRUPT_STATUS, interrupt::WDH);
        // Acknowledged: the line stays up only for another enabled cause.
        let enabled = self.read(reg::INTERRUPT_ENABLE);
        let other = self.read(reg::INTERRUPT_STATUS) & enabled & !interrupt::MIE != 0;
        assert_eq!(self.irq.pending() & Irq::Usb0.mask() != 0, other);
        done
    }

    /// How far into a TD's buffer `cbp` points. A buffer that crosses a page
    /// is two physical pieces; here they are adjacent, as the pool is.
    fn offset_in_buffer(&self, buffer: u32, len: u32, cbp: u32) -> u32 {
        assert!(cbp >= buffer && cbp < buffer + len, "CBP {cbp:08x} inside {buffer:08x}+{len}");
        cbp - buffer
    }

    /// Runs frames until the done queue comes back, at most `frames`.
    fn wait_done(&mut self, frames: u32) -> Vec<Done> {
        for _ in 0..frames {
            self.frame();
            let done = self.take_done();
            if !done.is_empty() {
                return done;
            }
        }
        Vec::new()
    }

    /// Runs frames until the TD at `td` is done, at most `frames`; other
    /// completions wait in the inbox for whoever queued them.
    fn wait_td(&mut self, td: u32, frames: u32) -> Option<Done> {
        for _ in 0..frames {
            self.frame();
            let done = self.take_done();
            self.inbox.extend(done);
            if let Some(i) = self.inbox.iter().position(|d| d.td == td) {
                return Some(self.inbox.remove(i));
            }
        }
        None
    }

    /// A control transfer to endpoint 0 of the device at `addr`. Returns the
    /// IN data, or the condition code of the TD that failed.
    fn control(&mut self, addr: u8, setup: [u8; 8], out: &[u8]) -> Result<Vec<u8>, u32> {
        let wlen = u32::from(u16::from_le_bytes([setup[6], setup[7]]));
        let is_in = setup[0] & 0x80 != 0;
        let setup_buf = self.alloc_buffer(8);
        self.ram.write(setup_buf, &setup);
        let data_buf = self.alloc_buffer(wlen);
        if !is_in {
            assert_eq!(out.len() as u32, wlen);
            self.ram.write(data_buf, out);
        }

        let tail = self.td(0, 0, 0, 0);
        let status_dir = if is_in && wlen > 0 { DIR_OUT } else { DIR_IN };
        let status = self.td(status_dir << 19 | TD_TOGGLE_DATA1, 0, 0, tail);
        let mut first = status;
        let mut data_td = None;
        if wlen > 0 {
            let dir = if is_in { DIR_IN } else { DIR_OUT };
            let td = self.td(
                dir << 19 | TD_ROUNDING | TD_NO_INTERRUPT | TD_TOGGLE_DATA1,
                data_buf,
                wlen,
                first,
            );
            data_td = Some(td);
            first = td;
        }
        let setup_td =
            self.td(DIR_SETUP << 19 | TD_NO_INTERRUPT | TD_TOGGLE_DATA0, setup_buf, 8, first);

        // Address, endpoint 0, direction from the TDs, 64-byte packets.
        self.ram.write_u32(CONTROL_ED, u32::from(addr) | 64 << 16);
        self.ram.write_u32(CONTROL_ED + 4, tail);
        self.ram.write_u32(CONTROL_ED + 8, setup_td);
        self.ram.write_u32(CONTROL_ED + 12, 0);
        self.write(reg::COMMAND_STATUS, command::CLF);

        // Stages complete in order; an error retires its TD and halts the ED.
        let mut result = Ok(0);
        for td in [Some(setup_td), data_td, Some(status)].into_iter().flatten() {
            let done = self.wait_td(td, 4).expect("each stage completes within a few frames");
            if done.cc != cc::NO_ERROR {
                assert_ne!(self.ram.read_u32(CONTROL_ED + 8) & 1, 0, "an error halts the endpoint");
                result = Err(done.cc);
                break;
            }
            if Some(td) == data_td {
                result = Ok(done.moved);
            }
        }
        let result = result.map(|got| {
            assert_eq!(self.ram.read_u32(CONTROL_ED + 8) & !0xF, tail, "the queue is drained");
            if is_in { self.ram.bytes(data_buf, got as usize) } else { Vec::new() }
        });
        // Unlink the TDs: an empty, skipped ED.
        self.ram.write_u32(CONTROL_ED, 1 << 14);
        self.ram.write_u32(CONTROL_ED + 4, 0);
        self.ram.write_u32(CONTROL_ED + 8, 0);
        result
    }

    fn get_descriptor(&mut self, addr: u8, kind: u8, len: u16) -> Vec<u8> {
        let l = len.to_le_bytes();
        self.control(addr, [0x80, 6, 0, kind, 0, 0, l[0], l[1]], &[]).unwrap()
    }

    /// Standard enumeration of the device answering at address 0: reads its
    /// descriptors, gives it `addr` and configures it. Returns the device
    /// and configuration descriptors.
    fn enumerate(&mut self, addr: u8) -> (Vec<u8>, Vec<u8>) {
        // The first read asks for 8 bytes, to learn the packet size.
        let short = self.get_descriptor(0, 1, 8);
        assert_eq!(short.len(), 8);
        self.control(0, [0x00, 5, addr, 0, 0, 0, 0, 0], &[]).unwrap();
        let device = self.get_descriptor(addr, 1, 18);
        assert_eq!(device.len(), usize::from(device[0]));
        assert_eq!(device[..8], short[..]);
        // The configuration with everything behind it: asked for 255 bytes,
        // the device answers with wTotalLength.
        let config = self.get_descriptor(addr, 2, 255);
        assert_eq!(config.len(), usize::from(u16::from_le_bytes([config[2], config[3]])));
        self.control(addr, [0x00, 9, config[5], 0, 0, 0, 0, 0], &[]).unwrap();
        assert_eq!(self.control(addr, [0x80, 8, 0, 0, 0, 0, 1, 0], &[]).unwrap(), [config[5]]);
        (device, config)
    }

    /// Resets the root port (0-based index) and waits for it to be enabled.
    fn reset_root_port(&mut self, index: usize) {
        let r = reg::RH_PORT_STATUS + 4 * index as u32;
        assert_ne!(self.read(r) & port::CCS, 0, "a device is connected");
        self.write(r, port::PRS);
        let status = self.read(r);
        assert_eq!(status & (port::PES | port::PRSC | port::PRS), port::PES | port::PRSC);
        self.write(r, port::CSC | port::PRSC);
        assert_eq!(self.read(r) & port::CHANGE, 0);
    }

    fn hub_port_status(&mut self, addr: u8, hub_port: u8) -> (u16, u16) {
        let s = self.control(addr, [0xA3, 0, 0, 0, hub_port, 0, 4, 0], &[]).unwrap();
        (u16::from_le_bytes([s[0], s[1]]), u16::from_le_bytes([s[2], s[3]]))
    }

    fn hub_port_feature(&mut self, addr: u8, hub_port: u8, set: bool, feature: u16) {
        let request = if set { 3 } else { 1 };
        self.control(addr, [0x23, request, feature as u8, 0, hub_port, 0, 0, 0], &[]).unwrap();
    }

    /// Opens an interrupt endpoint and links its ED into every slot of the
    /// HCCA interrupt table (polled each frame).
    fn open_interrupt(&mut self, addr: u8, ep: u8, is_in: bool, len: u32) -> Pipe {
        let ed = self.next_ed;
        self.next_ed += 16;
        let dir = if is_in { DIR_IN } else { DIR_OUT };
        let tail = self.td(0, 0, 0, 0);
        self.ram.write_u32(ed, u32::from(addr) | u32::from(ep) << 7 | dir << 11 | 32 << 16);
        self.ram.write_u32(ed + 4, tail);
        self.ram.write_u32(ed + 8, tail);
        // Chain in front of the EDs already in the table.
        self.ram.write_u32(ed + 12, self.ram.read_u32(HCCA));
        for slot in 0..32 {
            self.ram.write_u32(HCCA + 4 * slot, ed);
        }
        let buffer = self.alloc_buffer(len);
        Pipe { ed, buffer, len, td: tail, pending: None }
    }

    /// Queues one TD on an interrupt endpoint: the old tail becomes the TD,
    /// a fresh TD the tail, as OHCI drivers do.
    fn submit(&mut self, pipe: &mut Pipe, data: &[u8]) {
        let len = if data.is_empty() { pipe.len } else { data.len() as u32 };
        self.ram.write(pipe.buffer, data);
        let tail = self.td(0, 0, 0, 0);
        self.fill_td(pipe.td, TD_ROUNDING, pipe.buffer, len, tail);
        self.ram.write_u32(pipe.ed + 4, tail);
        pipe.pending = Some(pipe.td);
        pipe.td = tail;
    }

    /// Keeps an IN request queued and returns what arrives within `frames`,
    /// or `None` while the device NAKs (the request stays queued).
    fn poll(&mut self, pipe: &mut Pipe, frames: u32) -> Option<Vec<u8>> {
        if pipe.pending.is_none() {
            self.submit(pipe, &[]);
        }
        let d = self.wait_td(pipe.pending?, frames)?;
        pipe.pending = None;
        assert_eq!(d.cc, cc::NO_ERROR);
        Some(self.ram.bytes(pipe.buffer, d.moved as usize))
    }

    /// Brings up the controller on pad port `pad` (0-based): root port
    /// reset, hub at `hub_addr`, its port 1 reset, gamepad at `pad_addr`.
    fn enumerate_pad(&mut self, pad: usize, hub_addr: u8, pad_addr: u8) {
        self.reset_root_port(ROOT_PORT_OF_PAD[pad] - 1);
        let (device, _) = self.enumerate(hub_addr);
        assert_eq!(device[4], 9, "a hub");
        assert_ne!(self.hub_port_status(hub_addr, 1).0 & port_status::CONNECTION, 0);
        self.hub_port_feature(hub_addr, 1, true, feature::PORT_RESET);
        self.hub_port_feature(hub_addr, 1, false, feature::C_PORT_RESET);
        self.hub_port_feature(hub_addr, 1, false, feature::C_PORT_CONNECTION);
        let (device, _) = self.enumerate(pad_addr);
        assert_eq!(device[8..12], [0x5e, 0x04, 0x02, 0x02], "the Microsoft controller");
    }
}

fn sample_pad() -> PadState {
    let mut pad = PadState {
        buttons: button::START | button::DPAD_LEFT | button::RIGHT_THUMB,
        left_x: -32768,
        left_y: 32767,
        right_x: 0x1234,
        right_y: -2,
        ..PadState::default()
    };
    pad.analog[analog::A] = 0xFF;
    pad.analog[analog::WHITE] = 0x40;
    pad.analog[analog::RIGHT_TRIGGER] = 0x7F;
    pad
}

#[test]
fn the_input_report_carries_each_field_at_its_documented_offset() {
    let pad = sample_pad();
    let r = report::encode_input(&pad);
    assert_eq!(r[0], 0);
    assert_eq!(usize::from(r[1]), report::INPUT_LEN);
    assert_eq!(u16::from_le_bytes([r[2], r[3]]), pad.buttons);
    assert_eq!(r[4 + analog::A], 0xFF);
    assert_eq!(r[4 + analog::WHITE], 0x40);
    assert_eq!(r[4 + analog::RIGHT_TRIGGER], 0x7F);
    assert_eq!(i16::from_le_bytes([r[12], r[13]]), pad.left_x);
    assert_eq!(i16::from_le_bytes([r[14], r[15]]), pad.left_y);
    assert_eq!(i16::from_le_bytes([r[16], r[17]]), pad.right_x);
    assert_eq!(i16::from_le_bytes([r[18], r[19]]), pad.right_y);
    assert_eq!(report::decode_input(&r), Some(pad));
    assert_eq!(report::decode_input(&r[..19]), None);
}

#[test]
fn the_output_report_round_trips_rumble() {
    let rumble = Rumble { left: 0xBEEF, right: 0x0102 };
    let r = report::encode_output(rumble);
    assert_eq!(usize::from(r[1]), report::OUTPUT_LEN);
    assert_eq!(report::decode_output(&r), Some(rumble));
    assert_eq!(report::decode_output(&r[..5]), None);
}

#[test]
fn only_connected_pads_show_on_their_root_ports() {
    for mask in 0..16u8 {
        let mut d = Driver::start(mask);
        assert_eq!(d.read(reg::RH_DESCRIPTOR_A) & 0xFF, 4);
        for (pad, &root) in ROOT_PORT_OF_PAD.iter().enumerate() {
            let status = d.read(reg::RH_PORT_STATUS + 4 * (root as u32 - 1));
            assert_eq!(status & port::CCS != 0, mask >> pad & 1 != 0, "mask {mask:x} pad {pad}");
            assert_ne!(status & port::PPS, 0);
        }
    }
}

#[test]
fn frames_run_only_while_operational_and_count_in_the_hcca() {
    let mut d = Driver::start(1);
    d.frame();
    let first = d.read(reg::FM_NUMBER);
    for _ in 0..5 {
        d.frame();
    }
    let n = d.read(reg::FM_NUMBER);
    assert_eq!(n - first, 5, "one frame a millisecond");
    assert_eq!(u32::from(d.ram.read_u16(HCCA_FRAME_NUMBER)), n);
    assert_ne!(d.read(reg::INTERRUPT_STATUS) & interrupt::SF, 0);
    assert_eq!(d.ohci.next_deadline_ns(), Some(d.clock.step(0) + FRAME_NS));

    d.write(reg::CONTROL, control::HCFS_SUSPEND);
    assert_eq!(d.ohci.next_deadline_ns(), None);
    for _ in 0..5 {
        d.frame();
    }
    assert_eq!(d.read(reg::FM_NUMBER), n);
}

#[test]
fn frame_remaining_counts_down_within_a_frame() {
    let mut d = Driver::start(1);
    d.frame();
    let interval = d.read(reg::FM_INTERVAL) & 0x3FFF;
    let at_start = d.read(reg::FM_REMAINING);
    d.clock.step(FRAME_NS / 2);
    let halfway = d.read(reg::FM_REMAINING);
    assert_eq!(at_start, interval);
    assert_eq!(halfway, interval / 2);
}

#[test]
fn a_long_stop_is_not_replayed_frame_by_frame() {
    let mut d = Driver::start(1);
    d.frame();
    let before = d.read(reg::FM_NUMBER);
    let now = d.clock.step(3600 * 1_000_000_000);
    d.ohci.advance(now);
    assert!(d.read(reg::FM_NUMBER) - before <= 64);
    assert!(d.ohci.next_deadline_ns().unwrap() > now);
}

#[test]
fn the_done_head_interrupt_follows_its_enable_and_is_cleared_by_the_guest() {
    let mut d = Driver::start(1);
    d.reset_root_port(ROOT_PORT_OF_PAD[0] - 1);
    // take_done checks the line both ways on every transfer.
    assert_eq!(d.irq.pending(), 0);
    assert_eq!(d.get_descriptor(0, 1, 18).len(), 18);
    assert_eq!(d.irq.pending(), 0);

    // With WDH masked the done head is still written back, without the line.
    d.write(reg::INTERRUPT_DISABLE, interrupt::WDH);
    let setup = d.alloc_buffer(8);
    d.ram.write(setup, &[0x80, 6, 0, 1, 0, 0, 18, 0]);
    let tail = d.td(0, 0, 0, 0);
    let td = d.td(DIR_SETUP << 19 | TD_TOGGLE_DATA0, setup, 8, tail);
    d.ram.write_u32(CONTROL_ED, 64 << 16);
    d.ram.write_u32(CONTROL_ED + 4, tail);
    d.ram.write_u32(CONTROL_ED + 8, td);
    d.write(reg::COMMAND_STATUS, command::CLF);
    d.frame();
    assert_ne!(d.read(reg::INTERRUPT_STATUS) & interrupt::WDH, 0);
    assert_eq!(d.ram.read_u32(HCCA_DONE_HEAD) & !0xF, td);
    assert_eq!(d.irq.pending(), 0, "WDH is masked");
    // Enabling it raises the line for the status already pending.
    d.write(reg::INTERRUPT_ENABLE, interrupt::WDH);
    assert_eq!(d.irq.pending(), Irq::Usb0.mask());
    // The master enable gates it.
    d.write(reg::INTERRUPT_DISABLE, interrupt::MIE);
    assert_eq!(d.irq.pending(), 0);
    d.write(reg::INTERRUPT_ENABLE, interrupt::MIE);
    assert_eq!(d.irq.pending(), Irq::Usb0.mask());
    d.write(reg::INTERRUPT_STATUS, interrupt::WDH);
    assert_eq!(d.irq.pending(), 0);
}

#[test]
fn the_done_queue_is_held_until_the_guest_takes_the_last_one() {
    let mut d = Driver::start(1);
    d.enumerate_pad(0, 1, 2);
    let mut input = d.open_interrupt(2, 2, true, 32);
    d.submit(&mut input, &[]);
    d.frame();
    assert_ne!(d.read(reg::INTERRUPT_STATUS) & interrupt::WDH, 0);
    let first = d.ram.read_u32(HCCA_DONE_HEAD);
    // Unacknowledged, a second completion waits in HcDoneHead.
    d.pads.set_pad(0, &sample_pad());
    d.submit(&mut input, &[]);
    d.frame();
    d.frame();
    assert_eq!(d.ram.read_u32(HCCA_DONE_HEAD), first);
    assert_ne!(d.read(reg::DONE_HEAD), 0);
    assert_eq!(d.take_done().len(), 1);
    assert_eq!(d.wait_done(2).len(), 1);
}

#[test]
fn a_driver_enumerates_the_hub_and_gamepad_and_reads_input_and_sends_rumble() {
    let mut d = Driver::start(1);

    // The root hub: one controller, on player 1's root port.
    let root = ROOT_PORT_OF_PAD[0] - 1;
    assert_ne!(d.read(reg::RH_PORT_STATUS + 4 * root as u32) & port::CSC, 0);
    d.reset_root_port(root);
    assert_ne!(d.read(reg::INTERRUPT_STATUS) & interrupt::RHSC, 0, "the port changed");
    d.write(reg::INTERRUPT_STATUS, interrupt::RHSC);

    // The controller's hub.
    let (device, config) = d.enumerate(1);
    assert_eq!(device[4], 9, "hub class");
    assert_eq!(config[9 + 5], 9, "hub interface");
    assert_eq!(config[18 + 2], 0x81, "status change endpoint");
    let hub = d.control(1, [0xA0, 6, 0, 0x29, 0, 0, 9, 0], &[]).unwrap();
    assert_eq!((hub[0], hub[1], hub[2]), (9, 0x29, 3), "a hub descriptor with three ports");
    assert_eq!(d.control(1, [0xA0, 0, 0, 0, 0, 0, 4, 0], &[]).unwrap(), [0; 4]);

    // Its status change endpoint reports the gamepad's port.
    let mut changes = d.open_interrupt(1, 1, true, 1);
    assert_eq!(d.poll(&mut changes, 2), Some(vec![1 << 1]));
    let (status, change) = d.hub_port_status(1, 1);
    assert_eq!(
        status & (port_status::CONNECTION | port_status::POWER),
        port_status::CONNECTION | port_status::POWER
    );
    assert_eq!(status & (port_status::ENABLE | port_status::LOW_SPEED), 0);
    assert_eq!(change, port_change::CONNECTION);
    assert_eq!(
        d.hub_port_status(1, 2).0 & port_status::CONNECTION,
        0,
        "the expansion slots are empty"
    );
    assert_eq!(d.hub_port_status(1, 3).0 & port_status::CONNECTION, 0);
    assert_eq!(d.control(1, [0xA3, 0, 0, 0, 4, 0, 4, 0], &[]), Err(cc::STALL), "no port 4");

    // Nothing answers at address 0 until the port is reset.
    assert_eq!(d.control(0, [0x80, 6, 0, 1, 0, 0, 8, 0], &[]), Err(cc::DEVICE_NOT_RESPONDING));
    d.hub_port_feature(1, 1, false, feature::C_PORT_CONNECTION);
    d.hub_port_feature(1, 1, true, feature::PORT_RESET);
    let (status, change) = d.hub_port_status(1, 1);
    assert_ne!(status & port_status::ENABLE, 0);
    assert_eq!(change, port_change::RESET);
    d.hub_port_feature(1, 1, false, feature::C_PORT_RESET);
    assert_eq!(d.hub_port_status(1, 1).1, 0);
    assert_eq!(d.poll(&mut changes, 3), None, "no change left: the endpoint NAKs");

    // The gamepad.
    let (device, config) = d.enumerate(2);
    assert_eq!(device[8..12], [0x5e, 0x04, 0x02, 0x02]);
    assert_eq!((config[9 + 5], config[9 + 6]), (0x58, 0x42), "XID interface");
    assert_eq!((config[18 + 2], config[25 + 2]), (0x82, 0x02), "report in, rumble out");
    let xid = d.control(2, [0xC1, 6, 0, 0x42, 0, 0, 16, 0], &[]).unwrap();
    assert_eq!((xid[0], xid[1]), (16, 0x42));
    assert_eq!((usize::from(xid[6]), usize::from(xid[7])), (report::INPUT_LEN, report::OUTPUT_LEN));
    let caps = d.control(2, [0xC1, 1, 0, 1, 0, 0, 20, 0], &[]).unwrap();
    assert_eq!(caps.len(), report::INPUT_LEN);
    assert!(caps[2..].iter().all(|&b| b == 0xFF));
    assert_eq!(d.control(2, [0xC1, 1, 0, 2, 0, 0, 6, 0], &[]).unwrap().len(), report::OUTPUT_LEN);
    // The product string, in UTF-16.
    let s = d.control(2, [0x80, 6, 2, 3, 9, 4, 255, 0], &[]).unwrap();
    assert_eq!(usize::from(s[0]), s.len());
    let name: String = s[2..].chunks(2).map(|c| char::from(c[0])).collect();
    assert_eq!(name, "Microsoft Xbox Controller");
    assert_eq!(d.control(2, [0x80, 6, 9, 3, 9, 4, 255, 0], &[]), Err(cc::STALL), "no such string");

    // Input: the first poll reports, then only changes do.
    let mut input = d.open_interrupt(2, 2, true, 32);
    assert_eq!(d.poll(&mut input, 2).unwrap(), report::encode_input(&PadState::default()));
    assert_eq!(d.poll(&mut input, 20), None, "an untouched pad NAKs");
    let pad = sample_pad();
    d.ohci.set_pad(0, &pad);
    let r = d.poll(&mut input, 2).expect("the change is reported at the next frame");
    assert_eq!(report::decode_input(&r), Some(pad));
    assert_eq!(u16::from_le_bytes([r[2], r[3]]), pad.buttons);
    assert_eq!(d.poll(&mut input, 20), None);
    // Another port's pad is not this one's.
    d.pads.set_pad(1, &PadState { buttons: button::BACK, ..PadState::default() });
    assert_eq!(d.poll(&mut input, 20), None);
    // GET_REPORT reads the same state over the control pipe.
    assert_eq!(
        d.control(2, [0xA1, 1, 0, 1, 0, 0, 20, 0], &[]).unwrap(),
        report::encode_input(&pad)
    );
    // After a resend request the next poll reports though nothing changed.
    d.pads.resend();
    assert_eq!(d.poll(&mut input, 2).unwrap(), report::encode_input(&pad));

    // Rumble: the output report on the interrupt OUT endpoint.
    assert_eq!(d.ohci.rumble(0), Rumble::default());
    let mut output = d.open_interrupt(2, 2, false, 6);
    let rumble = Rumble { left: 0xFFFF, right: 0x1234 };
    d.submit(&mut output, &report::encode_output(rumble));
    let done =
        d.wait_td(output.pending.unwrap(), 2).expect("the report is taken at the next frame");
    assert_eq!((done.cc, done.moved), (cc::NO_ERROR, 6));
    assert_eq!(d.ohci.rumble(0), rumble);
    assert_eq!(d.pads.rumble(1), Rumble::default());
    // And as SET_REPORT over the control pipe.
    let rumble = Rumble { left: 0, right: 0x8000 };
    d.control(2, [0x21, 9, 0, 2, 0, 0, 6, 0], &report::encode_output(rumble)).unwrap();
    assert_eq!(d.pads.rumble(0), rumble);
    assert_eq!(
        d.control(2, [0x21, 9, 0, 3, 0, 0, 6, 0], &[0; 6]),
        Err(cc::STALL),
        "not a report it has"
    );
}

#[test]
fn a_short_read_without_rounding_is_a_data_underrun() {
    let mut d = Driver::start(1);
    d.enumerate_pad(0, 1, 2);
    let input = d.open_interrupt(2, 2, true, 32);
    // A 32-byte IN TD without buffer rounding: the 20-byte report is short.
    let tail = d.td(0, 0, 0, 0);
    d.fill_td(input.td, 0, input.buffer, 32, tail);
    d.ram.write_u32(input.ed + 4, tail);
    let done = d.wait_done(2);
    assert_eq!((done[0].cc, done[0].moved), (cc::DATA_UNDERRUN, report::INPUT_LEN as u32));
    assert_ne!(d.ram.read_u32(input.ed + 8) & 1, 0, "the endpoint halts");
}

#[test]
fn four_pads_enumerate_and_report_independently() {
    let mut d = Driver::start(0xF);
    let mut inputs = Vec::new();
    for pad in 0..4 {
        let (hub_addr, pad_addr) = (2 * pad as u8 + 1, 2 * pad as u8 + 2);
        d.enumerate_pad(pad, hub_addr, pad_addr);
        let mut input = d.open_interrupt(pad_addr, 2, true, 32);
        assert!(d.poll(&mut input, 2).is_some());
        inputs.push(input);
    }
    for pad in 0..4 {
        let state = PadState { left_x: 100 * (pad as i16 + 1), ..PadState::default() };
        d.ohci.set_pad(pad, &state);
        for (other, input) in inputs.iter_mut().enumerate() {
            let got = d.poll(input, 3);
            if other == pad {
                assert_eq!(report::decode_input(&got.unwrap()), Some(state));
            } else {
                assert_eq!(got, None, "pad {pad} moved, not pad {other}");
            }
        }
    }
}

#[test]
fn a_pad_can_be_unplugged_and_plugged_back_in_at_run_time() {
    let mut d = Driver::start(1);
    d.write(reg::INTERRUPT_ENABLE, interrupt::RHSC);
    d.write(reg::INTERRUPT_STATUS, interrupt::RHSC);
    d.enumerate_pad(0, 1, 2);
    d.write(reg::INTERRUPT_STATUS, interrupt::RHSC);
    assert_eq!(d.irq.pending(), 0);
    let root = reg::RH_PORT_STATUS + 4 * (ROOT_PORT_OF_PAD[0] as u32 - 1);

    // Pulled out: the root port reports the change and nothing answers.
    d.pads.set_connected(0, false);
    d.frame();
    assert_eq!(d.irq.pending(), Irq::Usb0.mask(), "root hub status change");
    assert_ne!(d.read(reg::INTERRUPT_STATUS) & interrupt::RHSC, 0);
    let status = d.read(root);
    assert_eq!(status & (port::CCS | port::PES | port::CSC), port::CSC);
    d.write(root, port::CSC);
    d.write(reg::INTERRUPT_STATUS, interrupt::RHSC);
    assert_eq!(d.irq.pending(), 0);
    assert_eq!(d.control(2, [0x80, 6, 0, 1, 0, 0, 18, 0], &[]), Err(cc::DEVICE_NOT_RESPONDING));
    assert_eq!(d.control(1, [0x80, 6, 0, 1, 0, 0, 18, 0], &[]), Err(cc::DEVICE_NOT_RESPONDING));

    // Plugged back in: a new connection, enumerated from address 0 again.
    d.pads.set_connected(0, true);
    d.frame();
    assert_eq!(d.irq.pending(), Irq::Usb0.mask());
    assert_eq!(d.read(root) & (port::CCS | port::PES | port::CSC), port::CCS | port::CSC);
    d.write(reg::INTERRUPT_STATUS, interrupt::RHSC);
    d.enumerate_pad(0, 5, 6);
    let mut input = d.open_interrupt(6, 2, true, 32);
    d.pads.set_pad(0, &sample_pad());
    assert_eq!(d.poll(&mut input, 2).unwrap(), report::encode_input(&sample_pad()));

    // A second player joins on their own root port.
    d.pads.set_connected(1, true);
    d.frame();
    d.write(reg::INTERRUPT_STATUS, interrupt::RHSC);
    d.enumerate_pad(1, 7, 8);
}

#[test]
fn a_plug_change_is_seen_even_before_the_controller_is_started() {
    let ram = TestRam::new(0x1000);
    let irq = Arc::new(IrqHub::new());
    let pads = PadPorts::new(0);
    let mut ohci = Ohci::new(ram, irq, TestClock::new(), pads.clone());
    let root = reg::RH_PORT_STATUS + 4 * (ROOT_PORT_OF_PAD[2] as u32 - 1);
    assert_eq!(ohci.mmio_read(root, 4) & port::CCS, 0);
    pads.set_connected(2, true);
    ohci.advance(0);
    assert_ne!(ohci.mmio_read(root, 4) & port::CCS, 0);
}

#[test]
fn a_controller_reset_returns_the_registers_and_the_interrupt_line() {
    let mut d = Driver::start(1);
    d.enumerate_pad(0, 1, 2);
    d.write(reg::INTERRUPT_ENABLE, interrupt::SF);
    d.frame();
    assert_eq!(d.irq.pending(), Irq::Usb0.mask());
    d.ohci.reset();
    assert_eq!(d.irq.pending(), 0);
    assert_eq!(d.read(reg::CONTROL) & control::HCFS, 0);
    assert_eq!(d.read(reg::HCCA), 0);
    assert_eq!(d.read(reg::INTERRUPT_ENABLE), interrupt::MIE);
    let status = d.read(reg::RH_PORT_STATUS + 4 * (ROOT_PORT_OF_PAD[0] as u32 - 1));
    assert_eq!(status & (port::CCS | port::PES | port::CSC), port::CCS | port::CSC);
}

#[test]
fn registers_read_at_byte_and_word_sizes() {
    let mut d = Driver::start(1);
    let whole = d.read(reg::FM_INTERVAL);
    assert_eq!(d.ohci.mmio_read(reg::FM_INTERVAL, 2), whole & 0xFFFF);
    assert_eq!(d.ohci.mmio_read(reg::FM_INTERVAL + 2, 2), whole >> 16);
    assert_eq!(d.ohci.mmio_read(reg::FM_INTERVAL + 1, 1), (whole >> 8) & 0xFF);
}

#[test]
fn data_arrives_whole_in_a_buffer_that_crosses_a_page() {
    let mut d = Driver::start(1);
    d.reset_root_port(ROOT_PORT_OF_PAD[0] - 1);
    let device = d.get_descriptor(0, 1, 18);
    let config = d.get_descriptor(0, 2, 255);
    // The same reads with the data buffer starting 10 bytes before a page
    // boundary: a full one, and a short one that ends in the second page.
    for (kind, len, expected) in [(1, 18, &device), (2, 255, &config)] {
        d.next_buffer = 0xA000 - 8 - 10;
        assert_eq!(&d.get_descriptor(0, kind, len), expected);
    }
}

/// Pins what a savestate holds of this: see `xb_types::state::StateIo::probe`.
#[test]
fn the_saved_layout_of_the_usb_controller_with_one_pad_is_the_recorded_one() {
    let mut io = xb_types::state::StateIo::probe();
    let mut d = Driver::start(0b0001);
    io.io(&mut d.ohci);
    assert_eq!(
        io.shape_hash(),
        0x4B9E_003C_8300_081E,
        "the saved layout changed. Older states would be misread: give the section a new version, \
         read the old one where it differs, then record {:#018x} here",
        io.shape_hash()
    );
}

#[test]
fn a_state_loads_into_a_controller_with_other_pads_plugged_in() {
    use xb_types::state::{StateIo, Stateful};
    // Saved with pads on ports 1 and 2, the second one set up by the title.
    let mut saved = Driver::start(0b0011);
    let mut io = StateIo::save();
    saved.ohci.state(&mut io);
    let bytes = io.into_bytes();

    // Loaded where only port 1 has a pad, and where three have.
    for now in [0b0001u8, 0b0111] {
        let mut d = Driver::start(now);
        let mut io = StateIo::load(bytes.clone());
        d.ohci.state(&mut io);
        assert_eq!(io.failed(), None, "pads {now:04b} now");
        // The controller is as the state had it...
        let plugged = |o: &Ohci, p: usize| o.pad_places(p).0;
        assert!(plugged(&d.ohci, 0) && plugged(&d.ohci, 1) && !plugged(&d.ohci, 2));
        // ... every port's devices are still somewhere...
        for p in 0..4 {
            let (in_port, parked) = d.ohci.pad_places(p);
            assert!(in_port != parked, "port {p}");
        }
        // ... and they stay: the controllers are the console's. One the
        // host has and the state had not is plugged in at the next frame.
        d.ohci.sync_pads_now();
        for p in 0..4 {
            let want = (0b0011 | now) >> p & 1 != 0;
            assert_eq!(plugged(&d.ohci, p), want, "port {p} after the sync");
        }
    }
}
