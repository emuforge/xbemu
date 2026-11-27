//! Shared pieces of the integration tests: a synthetic XBE on a synthetic
//! disc, a tiny x86 emitter for guest programs, and a test bus.
#![allow(dead_code, reason = "each test file uses a part")]

use std::path::PathBuf;
use std::sync::Arc;

use xb_cpu::{Bus, FaultAction};
use xb_formats::block::{BlockSource, MemSource};
use xb_types::irq::{Irq, IrqHub};

/// The image's base address.
pub const BASE: u32 = 0x0001_0000;
/// Where the kernel thunk table is.
pub const THUNKS: u32 = BASE + 0x1000;
/// Where the guest program starts (the entry point).
pub const CODE: u32 = BASE + 0x1100;
/// Zeroed, writable scratch memory of the program.
pub const DATA: u32 = BASE + 0x3000;

const ENTRY_KEY_RETAIL: u32 = 0xA8FC_57AB;
const THUNK_KEY_RETAIL: u32 = 0x5B6D_40B6;

/// A guest program under construction: bytes at [`CODE`] with labels.
#[derive(Default)]
pub struct Asm {
    pub bytes: Vec<u8>,
    imports: Vec<u32>,
}

impl Asm {
    pub fn new() -> Self {
        Self::default()
    }

    /// The address the next byte gets.
    pub fn here(&self) -> u32 {
        CODE + self.bytes.len() as u32
    }

    pub fn raw(&mut self, b: &[u8]) -> &mut Self {
        self.bytes.extend_from_slice(b);
        self
    }

    fn imm(&mut self, v: u32) -> &mut Self {
        self.bytes.extend_from_slice(&v.to_le_bytes());
        self
    }

    /// The thunk slot of kernel export `ordinal` (added to the import table
    /// on first use).
    pub fn thunk(&mut self, ordinal: u32) -> u32 {
        let i = match self.imports.iter().position(|o| *o == ordinal) {
            Some(i) => i,
            None => {
                self.imports.push(ordinal);
                self.imports.len() - 1
            }
        };
        THUNKS + 4 * i as u32
    }

    /// `call [thunk of ordinal]`
    pub fn call_export(&mut self, ordinal: u32) -> &mut Self {
        let t = self.thunk(ordinal);
        self.raw(&[0xFF, 0x15]).imm(t)
    }

    /// `push imm32`
    pub fn push(&mut self, v: u32) -> &mut Self {
        self.raw(&[0x68]).imm(v)
    }

    /// `push dword [addr]`
    pub fn push_mem(&mut self, addr: u32) -> &mut Self {
        self.raw(&[0xFF, 0x35]).imm(addr)
    }

    /// `mov [addr], eax`
    pub fn store_eax(&mut self, addr: u32) -> &mut Self {
        self.raw(&[0xA3]).imm(addr)
    }

    /// `mov [addr], esp`
    pub fn store_esp(&mut self, addr: u32) -> &mut Self {
        self.raw(&[0x89, 0x25]).imm(addr)
    }

    /// `mov dword [addr], imm32`
    pub fn store_imm(&mut self, addr: u32, v: u32) -> &mut Self {
        self.raw(&[0xC7, 0x05]).imm(addr).imm(v)
    }

    /// `mov ecx, imm32`
    pub fn mov_ecx(&mut self, v: u32) -> &mut Self {
        self.raw(&[0xB9]).imm(v)
    }

    /// `mov edx, imm32`
    pub fn mov_edx(&mut self, v: u32) -> &mut Self {
        self.raw(&[0xBA]).imm(v)
    }

    /// Spins until the dword at `addr` is nonzero:
    /// `L: cmp dword [addr], 0 ; je L`
    pub fn spin_until(&mut self, addr: u32) -> &mut Self {
        self.raw(&[0x83, 0x3D]).imm(addr).raw(&[0x00, 0x74, 0xF7])
    }

    /// Fills with `int3` up to `addr`, so the next routine starts there.
    pub fn pad_to(&mut self, addr: u32) -> &mut Self {
        assert!(addr >= self.here());
        self.bytes.resize((addr - CODE) as usize, 0xCC);
        self
    }

    /// The end marker: `cli ; hlt`, which stops the run with a fatal report
    /// naming a HLT with interrupts off.
    pub fn done(&mut self) -> &mut Self {
        self.raw(&[0xFA, 0xF4])
    }

    /// The XBE file holding the program, and its imports.
    pub fn xbe(&self) -> Vec<u8> {
        let mut f = vec![0u8; 0x5000];
        let put = |f: &mut Vec<u8>, off: usize, v: u32| {
            f[off..off + 4].copy_from_slice(&v.to_le_bytes());
        };
        f[0..4].copy_from_slice(b"XBEH");
        put(&mut f, 0x104, BASE);
        put(&mut f, 0x108, 0x1000); // SizeOfHeaders
        put(&mut f, 0x10C, 0x5000); // SizeOfImage
        put(&mut f, 0x110, 0x178);
        put(&mut f, 0x118, BASE + 0x180); // certificate
        put(&mut f, 0x11C, 1); // sections
        put(&mut f, 0x120, BASE + 0x400);
        put(&mut f, 0x128, CODE ^ ENTRY_KEY_RETAIL);
        put(&mut f, 0x130, 0x10000); // stack commit
        put(&mut f, 0x158, THUNKS ^ THUNK_KEY_RETAIL);
        put(&mut f, 0x164, BASE + 0x600); // library versions (none)
        put(&mut f, 0x180, 0x1D0);
        put(&mut f, 0x188, 0xFFFF_0001); // title id
        for (i, c) in "kvm test".encode_utf16().enumerate() {
            f[0x18C + 2 * i..0x18E + 2 * i].copy_from_slice(&c.to_le_bytes());
        }
        put(&mut f, 0x180 + 0xA0, 1); // region: North America
        let s0 = 0x400;
        put(&mut f, s0, 0x02 | 0x04 | 0x01); // preload, executable, writable
        put(&mut f, s0 + 4, BASE + 0x1000);
        put(&mut f, s0 + 8, 0x4000);
        put(&mut f, s0 + 12, 0x1000);
        put(&mut f, s0 + 16, 0x4000);
        put(&mut f, s0 + 20, BASE + 0x500);
        put(&mut f, s0 + 28, BASE + 0x480);
        put(&mut f, s0 + 32, BASE + 0x484);
        f[0x500..0x505].copy_from_slice(b".text");
        assert!(self.imports.len() < 60 && self.bytes.len() < 0x1F00);
        for (i, o) in self.imports.iter().enumerate() {
            put(&mut f, 0x1000 + 4 * i, 0x8000_0000 | o);
        }
        f[0x1100..0x1100 + self.bytes.len()].copy_from_slice(&self.bytes);
        f
    }

    /// A disc image with the program as `default.xbe`.
    pub fn disc(&self) -> Arc<dyn BlockSource> {
        disc_with(&self.xbe())
    }
}

/// An XDVDFS image holding `default.xbe` at the root.
pub fn disc_with(xbe: &[u8]) -> Arc<dyn BlockSource> {
    const SECTOR: usize = 2048;
    let xbe_sector = 40usize;
    let mut img = vec![0u8; (xbe_sector + xbe.len().div_ceil(SECTOR) + 1) * SECTOR];
    let vd = 32 * SECTOR;
    img[vd..vd + 20].copy_from_slice(b"MICROSOFT*XBOX*MEDIA");
    img[vd + 20..vd + 24].copy_from_slice(&33u32.to_le_bytes()); // root sector
    img[vd + 24..vd + 28].copy_from_slice(&(SECTOR as u32).to_le_bytes());
    img[vd + 0x7EC..vd + 0x800].copy_from_slice(b"MICROSOFT*XBOX*MEDIA");
    let d = 33 * SECTOR;
    img[d..d + SECTOR].fill(0xFF);
    img[d..d + 4].copy_from_slice(&[0, 0, 0, 0]);
    img[d + 4..d + 8].copy_from_slice(&(xbe_sector as u32).to_le_bytes());
    img[d + 8..d + 12].copy_from_slice(&(xbe.len() as u32).to_le_bytes());
    img[d + 12] = 0x20;
    img[d + 13] = 11;
    img[d + 14..d + 25].copy_from_slice(b"default.xbe");
    img[xbe_sector * SECTOR..xbe_sector * SECTOR + xbe.len()].copy_from_slice(xbe);
    Arc::new(MemSource(img))
}

/// A device access the guest made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    MmioRead(u32, u8),
    MmioWrite(u32, u8, u32),
    PortRead(u16, u8),
    PortWrite(u16, u8, u32),
}

/// A bus that logs device accesses and answers zeros. A write to port
/// [`TestBus::ACK_PORT`] lowers the interrupt line whose number is written,
/// as a device's interrupt acknowledge would.
pub struct TestBus {
    pub hub: Arc<IrqHub>,
    pub log: Vec<Access>,
    pub faults: Vec<(u32, bool)>,
    /// Stop logging after this many accesses (they still count).
    pub log_limit: usize,
    pub count: u64,
}

impl TestBus {
    pub const ACK_PORT: u16 = 0x1234;

    pub fn new(hub: Arc<IrqHub>) -> Self {
        Self { hub, log: Vec::new(), faults: Vec::new(), log_limit: 4096, count: 0 }
    }

    fn note(&mut self, a: Access) {
        self.count += 1;
        if self.log.len() < self.log_limit {
            self.log.push(a);
        }
    }
}

pub fn irq_line(n: u32) -> Option<Irq> {
    Some(match n {
        0 => Irq::Timer,
        1 => Irq::Usb0,
        3 => Irq::Gpu,
        4 => Irq::Nic,
        5 => Irq::Apu,
        6 => Irq::Aci,
        9 => Irq::Usb1,
        11 => Irq::Smbus,
        14 => Irq::Ide,
        _ => return None,
    })
}

impl Bus for TestBus {
    fn mmio_read(&mut self, pa: u32, size: u8) -> u32 {
        self.note(Access::MmioRead(pa, size));
        0
    }

    fn mmio_write(&mut self, pa: u32, size: u8, value: u32) {
        self.note(Access::MmioWrite(pa, size, value));
    }

    fn io_read(&mut self, port: u16, size: u8) -> u32 {
        self.note(Access::PortRead(port, size));
        0
    }

    fn io_write(&mut self, port: u16, size: u8, value: u32) {
        self.note(Access::PortWrite(port, size, value));
        if port == Self::ACK_PORT
            && let Some(line) = irq_line(value & 0xFF)
        {
            self.hub.lower(line);
        }
    }

    fn page_fault(&mut self, va: u32, _pa: Option<u32>, write: bool) -> FaultAction {
        self.faults.push((va, write));
        FaultAction::Raise
    }

    fn tsc(&mut self) -> u64 {
        0
    }
}

/// A fresh, empty folder for a test's hard disk; removed by [`TempDir`]'s drop.
pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!("xb-kernel-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("temp dir");
        Self(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A logger for `--nocapture` runs: `XBEMU_TEST_LOG=trace|debug|info` prints
/// the crate's log lines to stderr.
pub fn init_log() {
    struct L;
    impl log::Log for L {
        fn enabled(&self, _: &log::Metadata<'_>) -> bool {
            true
        }
        fn log(&self, r: &log::Record<'_>) {
            eprintln!("{}: {}", r.target(), r.args());
        }
        fn flush(&self) {}
    }
    static LOGGER: L = L;
    let level = match std::env::var("XBEMU_TEST_LOG").as_deref() {
        Ok("trace") => log::LevelFilter::Trace,
        Ok("debug") => log::LevelFilter::Debug,
        Ok("info") => log::LevelFilter::Info,
        _ => return,
    };
    if log::set_logger(&LOGGER).is_ok() {
        log::set_max_level(level);
    }
}
