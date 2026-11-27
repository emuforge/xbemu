//! The machine as the CPU and the kernel see it: device registers and ports
//! ([`xb_cpu::Bus`]), the page-fault policy, and the calls the kernel makes
//! into the devices ([`xb_kernel::Platform`]).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use xb_apu::Apu;
use xb_cpu::{Bus, FaultAction};
use xb_devices::{
    ACI_BASE, ACI_SIZE, Aci, NIC_BASE, NIC_SIZE, Nic, OHCI_BASE, OHCI_SIZE, Ohci, Ports,
};
use xb_gpu::{NV2A_MMIO_BASE, NV2A_MMIO_SIZE};
use xb_kernel::{DisplayMode, Platform};
use xb_types::PAGE_SIZE;

use crate::machine::Shared;

/// The catch-all register file: the device windows above the NV2A that have
/// no device (flash, the second USB controller, ...) hold what is written.
pub(crate) const MISC_BASE: u32 = 0xFE00_0000;

/// The APU's interrupt status register (`NV_PAPU_ISTS`); a write
/// acknowledges causes.
const APU_ISTS: u32 = 0x1000;

/// How many device accesses are logged at DEBUG, as the reference logs its
/// first 400.
const LOG_FIRST: u32 = 400;

/// How often the devices are looked at when none has a deadline (a pad
/// plugged in or pulled out is picked up by `Ohci::advance`).
const IDLE_TICK_NS: u64 = 4_000_000;

/// The devices the guest-CPU thread owns.
pub(crate) struct Devices {
    pub(crate) ohci: Ohci,
    pub(crate) aci: Aci,
    pub(crate) nic: Nic,
    pub(crate) ports: Ports,
    /// Pages of the catch-all register file that were written.
    misc: HashMap<u32, Box<[u8; PAGE_SIZE as usize]>>,
    logged: u32,
}

impl Devices {
    pub(crate) fn new(ohci: Ohci, aci: Aci, nic: Nic, ports: Ports) -> Self {
        Self { ohci, aci, nic, ports, misc: HashMap::new(), logged: 0 }
    }

    fn misc_read(&self, off: u32, size: u8) -> u32 {
        let mut v = 0u32;
        for i in 0..u32::from(size) {
            let o = off.wrapping_add(i);
            let byte = self.misc.get(&(o / PAGE_SIZE)).map_or(0, |p| p[(o % PAGE_SIZE) as usize]);
            v |= u32::from(byte) << (8 * i);
        }
        v
    }

    fn misc_write(&mut self, off: u32, size: u8, value: u32) {
        for i in 0..u32::from(size) {
            let o = off.wrapping_add(i);
            let page =
                self.misc.entry(o / PAGE_SIZE).or_insert_with(|| Box::new([0; PAGE_SIZE as usize]));
            page[(o % PAGE_SIZE) as usize] = (value >> (8 * i)) as u8;
        }
    }
}

/// Which device a physical address belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Window {
    Nv2a,
    Apu,
    Aci,
    Ohci,
    Nic,
    Misc,
}

impl Window {
    /// The window holding `pa` and its base and size, in the reference's
    /// registration order (the catch-all last).
    fn at(pa: u32) -> Option<(Self, u32, u32)> {
        const WINDOWS: [(Window, u32, u32); 6] = [
            (Window::Nv2a, NV2A_MMIO_BASE, NV2A_MMIO_SIZE),
            (Window::Apu, xb_apu::MMIO_BASE, xb_apu::MMIO_LEN),
            (Window::Ohci, OHCI_BASE, OHCI_SIZE),
            (Window::Aci, ACI_BASE, ACI_SIZE),
            (Window::Nic, NIC_BASE, NIC_SIZE),
            (Window::Misc, MISC_BASE, 0x0200_0000),
        ];
        WINDOWS.into_iter().find(|&(_, base, size)| pa.wrapping_sub(base) < size)
    }

    fn name(self) -> &'static str {
        match self {
            Self::Nv2a => "nv2a",
            Self::Apu => "apu",
            Self::Aci => "aci",
            Self::Ohci => "ohci",
            Self::Nic => "nvnet",
            Self::Misc => "misc",
        }
    }
}

/// The CPU's bus.
pub(crate) struct MachineBus {
    shared: Arc<Shared>,
    apu: Arc<Apu>,
    dev: Rc<RefCell<Devices>>,
}

impl MachineBus {
    /// The register-level devices in a savestate: the USB controller and
    /// what is plugged into it, the AC97 codec, the network controller and
    /// the catch-all register pages.
    pub(crate) fn devices_state(&mut self, io: &mut xb_types::state::StateIo, version: u32) {
        use xb_types::state::Stateful;
        let dev = &mut *self.dev.borrow_mut();
        // Section version 1 had the USB devices in their first layout.
        dev.ohci
            .state_as(io, if version >= 2 { xb_devices::usb::ohci::Ohci::STATE_LAYOUT } else { 1 });
        dev.aci.state(io);
        dev.nic.state(io);
        let mut pages: Vec<(u32, Vec<u8>)> = if io.loading() {
            Vec::new()
        } else {
            let mut p: Vec<_> = dev.misc.iter().map(|(k, v)| (*k, v.to_vec())).collect();
            p.sort_unstable_by_key(|e| e.0);
            p
        };
        let mut n = pages.len();
        n.state(io);
        if io.loading() {
            pages = (0..n.min(1 << 16)).map(|_| (0, Vec::new())).collect();
        }
        for (key, bytes) in &mut pages {
            key.state(io);
            io.blob(bytes);
        }
        if io.loading() && io.failed().is_none() {
            dev.misc.clear();
            for (key, bytes) in pages {
                if let Ok(page) =
                    <Box<[u8; PAGE_SIZE as usize]>>::try_from(bytes.into_boxed_slice())
                {
                    dev.misc.insert(key, page);
                }
            }
        }
    }

    pub(crate) fn new(shared: Arc<Shared>, apu: Arc<Apu>, dev: Rc<RefCell<Devices>>) -> Self {
        Self { shared, apu, dev }
    }

    fn read_in(&self, dev: &mut Devices, w: Window, off: u32, size: u8) -> u32 {
        match w {
            Window::Nv2a => self.shared.gpu.get().map_or(0, |g| g.mmio_read(off, size)),
            Window::Apu => self.apu.mmio_read(off, size),
            Window::Aci => dev.aci.mmio_read(off, size),
            Window::Ohci => dev.ohci.mmio_read(off, size),
            Window::Nic => dev.nic.mmio_read(off, size),
            Window::Misc => dev.misc_read(off, size),
        }
    }

    fn write_in(&self, dev: &mut Devices, w: Window, off: u32, size: u8, value: u32) {
        match w {
            Window::Nv2a => {
                if let Some(g) = self.shared.gpu.get() {
                    g.mmio_write(off, size, value);
                }
            }
            // `Bus::mmio_write` cannot ask for the store to be executed
            // again, so a busy voice is waited for (`Apu::try_mmio_write`
            // would need that).
            Window::Apu => self.apu.mmio_write(off, size, value),
            Window::Aci => dev.aci.mmio_write(off, size, value),
            Window::Ohci => dev.ohci.mmio_write(off, size, value),
            Window::Nic => dev.nic.mmio_write(off, size, value),
            Window::Misc => dev.misc_write(off, size, value),
        }
    }

    fn log(dev: &mut Devices, w: Window, off: u32, size: u8, write: bool, value: u32) {
        if dev.logged < LOG_FIRST && log::log_enabled!(target: "core", log::Level::Debug) {
            dev.logged += 1;
            let (dir, arrow) = if write { ("w", "<-") } else { ("r", "->") };
            log::debug!(
                target: "core",
                "mmio: {} {dir}+{off:06x} {arrow} {value:0width$x} ({size})",
                w.name(),
                width = 2 * usize::from(size)
            );
        }
    }
}

impl Bus for MachineBus {
    fn mmio_read(&mut self, pa: u32, size: u8) -> u32 {
        self.shared.counters.mmio.fetch_add(1, Ordering::Relaxed);
        let Some((w, base, len)) = Window::at(pa) else {
            return u32::MAX >> (32 - 8 * u32::from(size.clamp(1, 4)));
        };
        let off = pa - base;
        if w == Window::Ohci {
            self.shared.counters.usb.fetch_add(1, Ordering::Relaxed);
        }
        let dev = &mut *self.dev.borrow_mut();
        // A device is handed only what is inside its window (mmio.c
        // `dev_read`): of an access that reaches past the end the bytes
        // inside go to it one at a time and the rest read as ones.
        let v = if u32::from(size) <= len - off {
            self.read_in(dev, w, off, size)
        } else {
            (0..u32::from(size)).fold(0, |v, i| {
                let byte =
                    if off + i < len { self.read_in(dev, w, off + i, 1) & 0xFF } else { 0xFF };
                v | byte << (8 * i)
            })
        };
        Self::log(dev, w, off, size, false, v);
        v
    }

    fn mmio_write(&mut self, pa: u32, size: u8, value: u32) {
        self.shared.counters.mmio.fetch_add(1, Ordering::Relaxed);
        let Some((w, base, len)) = Window::at(pa) else {
            return;
        };
        let off = pa - base;
        if w == Window::Apu && off == APU_ISTS {
            self.shared.counters.apu_acks.fetch_add(1, Ordering::Relaxed);
        }
        let dev = &mut *self.dev.borrow_mut();
        Self::log(dev, w, off, size, true, value);
        if u32::from(size) <= len - off {
            self.write_in(dev, w, off, size, value);
        } else {
            for i in (0..u32::from(size)).filter(|i| off + i < len) {
                self.write_in(dev, w, off + i, 1, (value >> (8 * i)) & 0xFF);
            }
        }
    }

    fn io_read(&mut self, port: u16, size: u8) -> u32 {
        self.dev.borrow_mut().ports.io_read(port, size)
    }

    fn io_write(&mut self, port: u16, size: u8, value: u32) {
        self.dev.borrow_mut().ports.io_write(port, size, value);
    }

    fn page_fault(&mut self, va: u32, pa: Option<u32>, write: bool) -> FaultAction {
        let c = &self.shared.counters;
        c.faults.fetch_add(1, Ordering::Relaxed);
        // No mapping, or one the title's own protection forbids: the title's
        // fault. Anything else can only be write tracking's.
        let Some(pa) = pa else {
            c.raised.fetch_add(1, Ordering::Relaxed);
            return FaultAction::Raise;
        };
        if self.shared.cpu.table.forbids(va, pa, write) {
            c.raised.fetch_add(1, Ordering::Relaxed);
            return FaultAction::Raise;
        }
        if self.shared.tracker.on_fault(pa, write).handled() {
            FaultAction::Retry
        } else {
            c.raised.fetch_add(1, Ordering::Relaxed);
            log::debug!(target: "core", "page fault at {va:08x} (pa {pa:08x}, {}) is not write tracking's",
                if write { "write" } else { "read" });
            FaultAction::Raise
        }
    }

    fn tsc(&mut self) -> u64 {
        self.shared.tsc()
    }
}

/// The machine as the kernel sees it.
pub(crate) struct MachinePlatform {
    shared: Arc<Shared>,
    apu: Arc<Apu>,
    dev: Rc<RefCell<Devices>>,
}

impl MachinePlatform {
    pub(crate) fn new(shared: Arc<Shared>, apu: Arc<Apu>, dev: Rc<RefCell<Devices>>) -> Self {
        Self { shared, apu, dev }
    }
}

impl Platform for MachinePlatform {
    fn tick(&mut self, now_ns: u64) -> Option<u64> {
        let dev = &mut *self.dev.borrow_mut();
        dev.ohci.advance(now_ns);
        dev.aci.advance(now_ns);
        let next = [dev.ohci.next_deadline_ns(), dev.aci.next_deadline_ns()]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(u64::MAX);
        Some(next.min(now_ns + IDLE_TICK_NS).max(now_ns + 1))
    }

    fn tick_needs_kick(&mut self) -> bool {
        if std::env::var_os("XBEMU_T4_DEV_LAZY").is_none() {
            return true;
        }
        // The controller's frame interrupts only while the guest has one
        // unmasked and pending or about to be; the AC'97's period likewise.
        let dev = &*self.dev.borrow();
        dev.ohci.frame_may_interrupt() || dev.aci.po_running()
    }

    fn idle_deadline(&mut self, now_ns: u64, due: Option<u64>) -> Option<u64> {
        if std::env::var_os("XBEMU_T4_DEV_LAZY").is_none() {
            return due;
        }
        let dev = &*self.dev.borrow();
        let aci = if dev.aci.po_running() { dev.aci.next_deadline_ns() } else { None };
        let next =
            [dev.ohci.idle_deadline_ns(), aci].into_iter().flatten().min().unwrap_or(u64::MAX);
        // (the idle look still bounds it: controllers plugged and pulled)
        Some(next.min(now_ns + 2 * IDLE_TICK_NS).max(now_ns + 1))
    }

    fn set_display_mode(&mut self, mode: DisplayMode) {
        if let Some(gpu) = self.shared.gpu.get() {
            gpu.av_set_display_mode(mode.mode, mode.format, mode.pitch, mode.frame_buffer);
        }
    }

    fn set_title_widescreen(&mut self, wide: bool) {
        if let Some(gpu) = self.shared.gpu.get() {
            gpu.set_title_wide(Some(wide));
        }
    }

    fn title_shape_known(&mut self) -> bool {
        self.shared.gpu.get().is_some_and(|gpu| gpu.title_wide().is_some())
    }

    fn phy_link_state(&mut self) -> u32 {
        self.dev.borrow().nic.link_state()
    }

    fn phy_initialize(&mut self) {
        self.dev.borrow_mut().nic.phy_initialize();
    }

    fn apu_mailbox_wait(&mut self, word_pa: u32) {
        self.shared.counters.mailbox_waits.fetch_add(1, Ordering::Relaxed);
        let _ = self.apu.mailbox_wait(word_pa);
    }

    fn ram_freed(&mut self, pa: u32, len: u32) {
        if let Some(gpu) = self.shared.gpu.get() {
            gpu.mem_events().freed(pa, len);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xb_devices::PadPorts;
    use xb_types::irq::IrqHub;
    use xb_types::mem::PhysMem;

    struct NoRam;

    impl PhysMem for NoRam {
        fn size(&self) -> u32 {
            0
        }
        fn read(&self, _pa: u32, _buf: &mut [u8]) -> bool {
            false
        }
        fn write(&self, _pa: u32, _data: &[u8]) -> bool {
            false
        }
    }

    struct NoVblanks;

    impl xb_devices::VblankCounter for NoVblanks {
        fn vblanks(&self) -> u64 {
            0
        }
    }

    fn devices() -> Devices {
        let hub = Arc::new(IrqHub::new());
        let mem: Arc<dyn PhysMem> = Arc::new(NoRam);
        let clock = Arc::new(|| 0u64);
        Devices::new(
            Ohci::new(mem.clone(), hub.clone(), clock.clone(), PadPorts::new(1)),
            Aci::new(mem, hub.clone()),
            Nic::new(hub),
            Ports::new(clock, Arc::new(NoVblanks)),
        )
    }

    #[test]
    fn each_address_goes_to_its_device_and_the_rest_above_the_nv2a_to_the_register_file() {
        let at = |pa| Window::at(pa).map(|(w, base, _)| (w, pa - base));
        assert_eq!(at(0xFD00_0000), Some((Window::Nv2a, 0)));
        assert_eq!(at(0xFD80_0044), Some((Window::Nv2a, 0x80_0044)));
        assert_eq!(at(0xFDFF_FFFF), Some((Window::Nv2a, 0xFF_FFFF)));
        assert_eq!(at(0xFE80_1000), Some((Window::Apu, 0x1000)));
        assert_eq!(at(0xFE87_FFFF), Some((Window::Apu, 0x7_FFFF)));
        assert_eq!(at(0xFE88_0000), Some((Window::Misc, 0x88_0000)));
        assert_eq!(at(0xFEC0_0130), Some((Window::Aci, 0x130)));
        assert_eq!(at(0xFED0_0054), Some((Window::Ohci, 0x54)));
        assert_eq!(at(0xFED0_8000), Some((Window::Misc, 0xD0_8000)), "the second USB controller");
        assert_eq!(at(0xFEF0_0188), Some((Window::Nic, 0x188)));
        assert_eq!(at(0xFEF0_0400), Some((Window::Misc, 0xF0_0400)));
        assert_eq!(at(0xFFFF_FFFF), Some((Window::Misc, 0x1FF_FFFF)));
        assert_eq!(at(0xFCFF_FFFF), None);
        assert_eq!(at(0x0001_0000), None);
    }

    #[test]
    fn the_register_file_reads_back_what_was_written_and_zero_elsewhere() {
        let mut d = devices();
        assert_eq!(d.misc_read(0xD0_8004, 4), 0);
        d.misc_write(0xD0_8004, 4, 0x1234_5678);
        d.misc_write(0xD0_8006, 1, 0xAB);
        assert_eq!(d.misc_read(0xD0_8004, 4), 0x12AB_5678);
        assert_eq!(d.misc_read(0xD0_8005, 2), 0xAB56);
        // An access across a page of the file.
        d.misc_write(0xD0_8FFE, 4, 0xCAFE_F00D);
        assert_eq!(d.misc_read(0xD0_8FFE, 4), 0xCAFE_F00D);
        assert_eq!(d.misc_read(0xD0_9000, 2), 0xCAFE);
    }
}
