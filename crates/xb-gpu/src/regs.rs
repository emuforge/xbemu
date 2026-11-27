//! The NV2A's register file: 16 MiB of 32-bit registers the guest reads back
//! as it wrote them, shared with the renderer's threads.
//!
//! Every access is an atomic word operation, so sharing the file between the
//! guest-CPU thread and the renderer needs no `unsafe` at the call sites.
//! Narrow and unaligned accesses update the containing words.

use std::sync::atomic::{AtomicU32, Ordering};

/// Size of the register window at physical `0xFD00_0000`.
pub const NV2A_SIZE: u32 = 0x0100_0000;

/// Register offsets and bits the front end gives behavior to. Names follow
/// the reference's NV2A register map.
pub mod reg {
    /// Chip identification.
    pub const PMC_BOOT_0: u32 = 0x00_0000;
    /// Pending interrupt summary, read-only.
    pub const PMC_INTR_0: u32 = 0x00_0100;
    /// Bit 0: interrupts go to the CPU.
    pub const PMC_INTR_EN_0: u32 = 0x00_0140;
    /// FIFO runout status; bit 4 is "empty".
    pub const PFIFO_RUNOUT_STATUS: u32 = 0x00_2400;
    /// CACHE1 status; bit 4 is "empty".
    pub const PFIFO_CACHE1_STATUS: u32 = 0x00_3214;
    /// CACHE1 DMA push state; bit 4 is "busy".
    pub const PFIFO_CACHE1_DMA_PUSH: u32 = 0x00_3220;
    /// CACHE1's DMA put pointer.
    pub const PFIFO_CACHE1_DMA_PUT: u32 = 0x00_3240;
    /// CACHE1's DMA get pointer.
    pub const PFIFO_CACHE1_DMA_GET: u32 = 0x00_3244;
    /// CACHE1's reference counter.
    pub const PFIFO_CACHE1_REF: u32 = 0x00_3248;
    /// CACHE1's subroutine return address.
    pub const PFIFO_CACHE1_DMA_SUBROUTINE: u32 = 0x00_324C;
    /// PVIDEO: bit 0, overlay buffer 0 in use.
    pub const PVIDEO_BUFFER: u32 = 0x00_8700;
    /// PVIDEO: bit 0 stops the overlay.
    pub const PVIDEO_STOP: u32 = 0x00_8704;
    /// PTIMER interrupt status; bit 0 is the alarm.
    pub const PTIMER_INTR_0: u32 = 0x00_9100;
    /// PTIMER interrupt enable.
    pub const PTIMER_INTR_EN_0: u32 = 0x00_9140;
    /// PTIMER time, low word (the low five bits read as zero).
    pub const PTIMER_TIME_0: u32 = 0x00_9400;
    /// PTIMER time, high word (29 bits).
    pub const PTIMER_TIME_1: u32 = 0x00_9410;
    /// PTIMER alarm: compared with the time's low word.
    pub const PTIMER_ALARM_0: u32 = 0x00_9420;
    /// Installed memory size in bytes.
    pub const PFB_CSTATUS: u32 = 0x10_020C;
    /// Write-combine flush state; reads as 0.
    pub const PFB_WBC: u32 = 0x10_0410;
    /// PGRAPH interrupt status.
    pub const PGRAPH_INTR: u32 = 0x40_0100;
    /// PGRAPH interrupt enable.
    pub const PGRAPH_INTR_EN: u32 = 0x40_0140;
    /// PGRAPH surface state: the flip counters (read 24..27, write 20..23,
    /// modulo 28..31).
    pub const PGRAPH_SURFACE: u32 = 0x40_0710;
    /// PGRAPH increment: bit 1 advances the flip read counter.
    pub const PGRAPH_INCREMENT: u32 = 0x40_071C;
    /// PGRAPH FIFO access enable.
    pub const PGRAPH_FIFO: u32 = 0x40_0720;
    /// PCRTC interrupt status; bit 0 is vblank.
    pub const PCRTC_INTR_0: u32 = 0x60_0100;
    /// PCRTC interrupt enable.
    pub const PCRTC_INTR_EN_0: u32 = 0x60_0140;
    /// PCRTC: address of the frame buffer the display scans out.
    pub const PCRTC_START: u32 = 0x60_0800;
    /// PCRTC: current scanline.
    pub const PCRTC_RASTER: u32 = 0x60_0808;
    /// DAC palette: index of the entry to write (a byte register).
    pub const PRMDIO_WRITE_INDEX: u32 = 0x68_13C8;
    /// DAC palette: R, G, B bytes of the entry, auto-incrementing.
    pub const PRMDIO_DATA: u32 = 0x68_13C9;
    /// Channel 0's DMA put pointer; writing it submits the push buffer.
    pub const USER_DMA_PUT: u32 = 0x80_0040;
    /// Channel 0's DMA get pointer.
    pub const USER_DMA_GET: u32 = 0x80_0044;
    /// Channel 0's reference counter.
    pub const USER_REF: u32 = 0x80_0048;

    /// `PMC_INTR_0`: PCRTC has an enabled interrupt pending.
    pub const PMC_INTR_PCRTC: u32 = 1 << 24;
    /// `PMC_INTR_0`: PTIMER has an enabled interrupt pending.
    pub const PMC_INTR_PTIMER: u32 = 1 << 20;
    /// `PMC_INTR_0`: PGRAPH has an enabled interrupt pending.
    pub const PMC_INTR_PGRAPH: u32 = 1 << 12;
    /// `PCRTC_INTR_0`: vertical blank.
    pub const PCRTC_VBLANK: u32 = 1;
    /// `PTIMER_INTR_0`: alarm.
    pub const PTIMER_ALARM: u32 = 1;
    /// `PFIFO_RUNOUT_STATUS` / `PFIFO_CACHE1_STATUS`: empty.
    pub const PFIFO_LOW_MARK: u32 = 1 << 4;
    /// `PFIFO_CACHE1_DMA_PUSH`: busy.
    pub const PFIFO_DMA_BUSY: u32 = 1 << 4;
}

/// The registers as last written, one atomic word per register.
pub struct RegFile {
    /// `NV2A_SIZE / 4` words and one of slack, so a 4-byte access at the last
    /// byte offsets stays inside (the C allocates 16 bytes of slack).
    ///
    /// The memory is a page-aligned anonymous mapping, so whole pages of it
    /// can back the guest's reads directly ([`RegFile::page_ptr`]).
    ptr: *mut AtomicU32,
    len: usize,
}

// SAFETY: the mapping is plain atomics, owned by this value until drop.
unsafe impl Send for RegFile {}
// SAFETY: as for `Send`; every access is an atomic operation.
unsafe impl Sync for RegFile {}

impl Drop for RegFile {
    fn drop(&mut self) {
        // SAFETY: the mapping made in `new`, unmapped once.
        unsafe { libc::munmap(self.ptr.cast(), Self::MAP_LEN) };
    }
}

impl Default for RegFile {
    fn default() -> Self {
        Self::new()
    }
}

impl RegFile {
    /// A zeroed register file. The memory is committed page by page as the
    /// guest writes it.
    pub fn new() -> Self {
        // SAFETY: a fresh anonymous mapping aliases nothing.
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                Self::MAP_LEN,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        assert!(p != libc::MAP_FAILED, "mapping the NV2A register file failed");
        Self { ptr: p.cast(), len: (NV2A_SIZE / 4 + 1) as usize }
    }

    const MAP_LEN: usize = NV2A_SIZE as usize + 4096;

    #[inline]
    fn words(&self) -> &[AtomicU32] {
        // SAFETY: `ptr` addresses `len` zero-initialized words (the mapping
        // is larger), valid as atomics, alive as long as `self`.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    /// Host address of the registers from byte offset `off` (page aligned)
    /// on: the memory a CPU backend may let the guest read directly. Valid
    /// for as long as this value lives.
    #[must_use]
    pub fn page_ptr(&self, off: u32) -> *const u8 {
        assert!(off < NV2A_SIZE && off & 0xFFF == 0);
        // SAFETY: inside the mapping.
        unsafe { self.ptr.cast::<u8>().add(off as usize) }
    }

    /// The word holding byte offset `off`.
    #[inline]
    fn word(&self, off: u32) -> &AtomicU32 {
        // In range for every `off` below `NV2A_SIZE + 4`; callers mask to the
        // window, and the index is checked.
        &self.words()[(off >> 2) as usize]
    }

    /// The register at `off` (rounded down to a word).
    #[inline]
    pub fn load(&self, off: u32) -> u32 {
        self.word(off).load(Ordering::Acquire)
    }

    /// Stores `v` in the register at `off`.
    #[inline]
    pub fn store(&self, off: u32, v: u32) {
        self.word(off).store(v, Ordering::Release);
    }

    /// Sets bits of the register at `off`.
    #[inline]
    pub fn fetch_or(&self, off: u32, bits: u32) {
        self.word(off).fetch_or(bits, Ordering::AcqRel);
    }

    /// Clears bits of the register at `off`.
    #[inline]
    pub fn fetch_clear(&self, off: u32, bits: u32) {
        self.word(off).fetch_and(!bits, Ordering::AcqRel);
    }

    /// Replaces the register at `off` with `f` of its value, atomically.
    /// Returns the new value.
    #[inline]
    pub fn update(&self, off: u32, f: impl Fn(u32) -> u32) -> u32 {
        let w = self.word(off);
        let mut cur = w.load(Ordering::Acquire);
        loop {
            let new = f(cur);
            match w.compare_exchange_weak(cur, new, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return new,
                Err(seen) => cur = seen,
            }
        }
    }

    /// Reads `size` (1, 2 or 4) bytes at byte offset `off`, little endian.
    #[inline]
    pub fn read(&self, off: u32, size: u8) -> u32 {
        if size == 4 && off & 3 == 0 {
            return self.load(off);
        }
        let shift = (off & 3) * 8;
        let lo = u64::from(self.load(off));
        // The next word matters only when the access crosses into it.
        let v = if (off & 3) + u32::from(size) > 4 {
            lo | (u64::from(self.load(off + 4)) << 32)
        } else {
            lo
        };
        ((v >> shift) & mask(size)) as u32
    }

    /// Writes the low `size` (1, 2 or 4) bytes of `value` at byte offset
    /// `off`, little endian. A narrow write is a read-modify-write of the
    /// word; a concurrent word update by the renderer can be lost, as with
    /// the C's `memcpy`.
    #[inline]
    pub fn write(&self, off: u32, size: u8, value: u32) {
        if size == 4 && off & 3 == 0 {
            self.store(off, value);
            return;
        }
        let shift = (off & 3) * 8;
        let m = mask(size) << shift;
        let v = (u64::from(value) << shift) & m;
        self.update(off, |w| (w & !(m as u32)) | v as u32);
        if (m >> 32) != 0 {
            self.update(off + 4, |w| (w & !((m >> 32) as u32)) | (v >> 32) as u32);
        }
    }
}

/// The value mask of an access of `size` bytes.
#[inline]
fn mask(size: u8) -> u64 {
    match size {
        1 => 0xFF,
        2 => 0xFFFF,
        _ => 0xFFFF_FFFF,
    }
}

impl RegFile {
    /// The register file in a savestate, as its 4 KiB pages that are not
    /// all zero: the count, then each one's index and words (as the
    /// reference keeps it).
    pub fn state(&self, io: &mut xb_types::state::StateIo) {
        use xb_types::state::Stateful;
        const PAGE_WORDS: usize = 1024;
        let pages = self.words().len() / PAGE_WORDS;
        if io.loading() {
            let mut n = 0u32;
            n.state(io);
            for w in self.words().iter() {
                w.store(0, Ordering::Relaxed);
            }
            for _ in 0..n {
                let mut page = 0u32;
                page.state(io);
                if io.failed().is_some() || page as usize >= pages {
                    io.fail("the state's GPU registers are cut short");
                    return;
                }
                for w in &self.words()[page as usize * PAGE_WORDS..][..PAGE_WORDS] {
                    let mut v = 0u32;
                    v.state(io);
                    w.store(v, Ordering::Relaxed);
                }
            }
        } else {
            let used: Vec<usize> = (0..pages)
                .filter(|p| {
                    self.words()[p * PAGE_WORDS..][..PAGE_WORDS]
                        .iter()
                        .any(|w| w.load(Ordering::Relaxed) != 0)
                })
                .collect();
            (used.len() as u32).state(io);
            for p in used {
                (p as u32).state(io);
                for w in &self.words()[p * PAGE_WORDS..][..PAGE_WORDS] {
                    w.load(Ordering::Relaxed).state(io);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn narrow_and_unaligned_accesses_agree_with_a_byte_array() {
        let regs = RegFile::new();
        let mut bytes = [0u8; 16];
        let mut seed = 0x1234_5678u32;
        for step in 0..2000u32 {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let size = [1u8, 2, 4][(seed >> 8) as usize % 3];
            let off = (seed >> 16) % (16 - u32::from(size) + 1);
            let value = seed ^ step;
            regs.write(0x1000 + off, size, value);
            bytes[off as usize..off as usize + size as usize]
                .copy_from_slice(&value.to_le_bytes()[..size as usize]);
            for (o, s) in [(0u32, 4u8), (1, 4), (5, 2), (7, 2), (9, 1), (11, 4)] {
                let mut want = [0u8; 4];
                want[..s as usize].copy_from_slice(&bytes[o as usize..o as usize + s as usize]);
                assert_eq!(regs.read(0x1000 + o, s), u32::from_le_bytes(want));
            }
        }
    }

    #[test]
    fn the_last_bytes_of_the_window_are_accessible_at_any_width() {
        let regs = RegFile::new();
        regs.write(NV2A_SIZE - 1, 4, 0xAABB_CCDD);
        assert_eq!(regs.read(NV2A_SIZE - 1, 1), 0xDD);
        assert_eq!(regs.read(NV2A_SIZE - 1, 4), 0xAABB_CCDD);
    }
}
