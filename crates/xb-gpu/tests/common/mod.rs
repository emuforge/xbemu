//! A stand-in machine for the renderer's tests and the windowed example:
//! 64 MiB of RAM mapped one to one, every page always "written", and a
//! minimal XBE header at `0x0001_0000` so the renderer starts a title.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

use xb_gpu::{CpuRegs, GpuHost};

/// Title id in the synthetic XBE's certificate.
pub const TITLE_ID: u32 = 0x5842_0001;
/// The synthetic XBE's time stamp.
pub const TIMEDATE: u32 = 0x1234_5678;
/// Size of the mock machine's RAM.
pub const RAM_SIZE: u32 = 64 << 20;

/// The machine: RAM by raw pointer, identity page table below `RAM_SIZE`
/// and the contiguous window at `0x8000_0000`.
pub struct MockHost {
    ram: *mut u8,
    start: Instant,
    /// How often the renderer kicked the guest CPU.
    pub kicks: AtomicU32,
}

// SAFETY: `ram` points at a leaked allocation that lives for the process and
// is only accessed with raw copies (racing accesses are the device-DMA
// semantics the real machine has too).
unsafe impl Send for MockHost {}
// SAFETY: as above.
unsafe impl Sync for MockHost {}

impl MockHost {
    /// A machine with zeroed RAM holding the synthetic XBE header.
    pub fn new() -> Self {
        let ram: &'static mut [u8] = Box::leak(vec![0u8; RAM_SIZE as usize].into_boxed_slice());
        let h = &mut ram[0x1_0000..0x1_1000];
        let mut put = |off: usize, v: u32| h[off..off + 4].copy_from_slice(&v.to_le_bytes());
        put(0x000, 0x4845_4258); // "XBEH"
        put(0x104, 0x0001_0000); // base address
        put(0x108, 0x1000); // size of headers
        put(0x10C, 0x0010_0000); // size of image
        put(0x110, 0x178); // size of the image header
        put(0x114, TIMEDATE);
        put(0x118, 0x0001_0000 + 0x200); // certificate
        put(0x11C, 0); // sections
        put(0x120, 0x0001_0000 + 0x400);
        put(0x128, 0x0001_0000 + 0x1000); // entry point, stored decoded
        put(0x160, 0); // library versions
        put(0x164, 0x0001_0000 + 0x500);
        put(0x200 + 8, TITLE_ID);
        Self { ram: ram.as_mut_ptr(), start: Instant::now(), kicks: AtomicU32::new(0) }
    }

    /// The RAM pointer for `Gpu::new`.
    pub fn ram(&self) -> *mut u8 {
        self.ram
    }

    fn pa(va: u32, len: usize) -> Option<usize> {
        let pa = if va >= 0x8000_0000 { va - 0x8000_0000 } else { va } as usize;
        (va < 0xFD00_0000 && pa.checked_add(len)? <= RAM_SIZE as usize).then_some(pa)
    }
}

impl GpuHost for MockHost {
    fn va_read(&self, va: u32, buf: &mut [u8]) -> bool {
        let Some(pa) = Self::pa(va, buf.len()) else { return false };
        // SAFETY: `[pa, pa + len)` is inside the leaked RAM (checked).
        unsafe { std::ptr::copy_nonoverlapping(self.ram.add(pa), buf.as_mut_ptr(), buf.len()) };
        true
    }
    fn va_write(&self, va: u32, data: &[u8]) -> bool {
        let Some(pa) = Self::pa(va, data.len()) else { return false };
        // SAFETY: as above.
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), self.ram.add(pa), data.len()) };
        true
    }
    fn va_store(&self, _va: u32, _len: u32) {}
    fn mem_dirty(&self, _pa: u32) -> bool {
        true
    }
    fn mem_open(&self, _pa: u32) -> bool {
        true
    }
    fn mem_test_and_clear(&self, _pa: u32, _len: u32) -> bool {
        true
    }
    fn mem_set_dirty(&self, _pa: u32, _len: u32) {}
    fn mem_landed(&self, _pa: u32, _len: u32) {}
    fn mem_rearm(&self, _pa: u32, _len: u32) {}
    fn mem_noaccess(&self, _pa: u32, _len: u32, _on: bool) {}
    fn kick(&self) {
        self.kicks.fetch_add(1, Ordering::Relaxed);
    }
    fn guest_hold(&self, _on: bool) {}
    fn frame_wait(&self) {}
    fn frame_flipped(&self) {}
    fn frame_holds_timed_out(&self) -> u32 {
        0
    }
    fn cpu_regs(&self) -> Option<CpuRegs> {
        Some(CpuRegs { eip: 0x0001_1000, eflags: 0x202, ..CpuRegs::default() })
    }
    fn guest_time_ns(&self) -> u64 {
        self.start.elapsed().as_nanos() as u64
    }
    fn guest_to_host_ns(&self, guest_ns: u64) -> u64 {
        // The mock's guest clock started `start` ago on the host clock.
        let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: `ts` is a valid timespec to fill.
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
        let host_now = ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64;
        host_now - self.guest_time_ns() + guest_ns
    }
}

/// Prints the `log` crate's records on stderr, so a test run shows the
/// renderer's start-up story.
pub struct StderrLog;

impl log::Log for StderrLog {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }
    fn log(&self, r: &log::Record<'_>) {
        eprintln!("[{:5} {}] {}", r.level(), r.target(), r.args());
    }
    fn flush(&self) {}
}

/// Installs [`StderrLog`] at INFO.
pub fn init_log() {
    static LOG: StderrLog = StderrLog;
    if log::set_logger(&LOG).is_ok() {
        log::set_max_level(log::LevelFilter::Info);
    }
}
