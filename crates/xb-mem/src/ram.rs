//! Guest physical RAM.
//!
//! One shared memory object and the host's view of it, which write tracking
//! never protects.
//!
//! # Data races
//!
//! RAM is shared with the running guest and with device threads, exactly as a
//! device's DMA shares it on hardware. Every access here is a raw copy between
//! the mapping and a caller-owned buffer; no reference into the mapping is
//! ever created, so the compiler assumes nothing about its contents. A copy
//! that races with a guest store sees some mix of old and new bytes. Callers
//! needing a consistent snapshot stop the guest first.
#![expect(unsafe_code, reason = "raw copies between the RAM mapping and caller buffers")]

use std::io;
use std::sync::Arc;

use xb_types::PAGE_SIZE;
use xb_types::mem::PhysMem;

use crate::host::SharedMem;

/// Guest physical RAM: one shared memory object, mapped once read-write in
/// the host.
///
/// A handle: `Clone` is a reference-count bump, and every clone addresses the
/// same bytes. The mapping lives until the last clone is dropped. CPU
/// backends build the guest's views from the memory object itself
/// ([`Ram::fd`]).
#[derive(Clone)]
pub struct Ram {
    mem: Arc<SharedMem>,
    size: u32,
}

impl Ram {
    /// The largest RAM accepted: what fits between the contiguous window
    /// (`0x8000_0000`) and the end of the guest's address space.
    pub const MAX_SIZE: u32 = 0x8000_0000;

    /// Creates zero-filled RAM of `size` bytes (a non-zero multiple of the
    /// guest page size, at most [`Ram::MAX_SIZE`]).
    pub fn new(size: u32) -> io::Result<Self> {
        if size == 0 || !size.is_multiple_of(PAGE_SIZE) || size > Self::MAX_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("guest RAM size {size:#x} is not a page multiple in (0, 2 GiB]"),
            ));
        }
        let mem = SharedMem::new(size as usize)?;
        log::debug!(target: "mem", "guest RAM: {} MiB at host {:p}", size >> 20, mem.as_ptr());
        Ok(Self { mem: Arc::new(mem), size })
    }

    /// Size in bytes.
    #[must_use]
    pub fn size(&self) -> u32 {
        self.size
    }

    /// Size in guest pages.
    #[must_use]
    pub fn pages(&self) -> u32 {
        self.size / PAGE_SIZE
    }

    /// The memory object behind RAM. A CPU backend maps its own views from it
    /// (`mmap` with `MAP_SHARED`; offset = physical address).
    #[cfg(unix)]
    #[must_use]
    pub fn fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.mem.fd()
    }

    /// Host address of physical address 0, for code that needs `uint8_t *ram`
    /// (the vendored GPU renderer and APU). [`Ram::len`] bytes are mapped
    /// read-write.
    ///
    /// The pointer stays valid while any clone of this `Ram` is alive; the
    /// caller keeps one for as long as it uses the pointer. Dereferencing it
    /// is the caller's `unsafe`: the bytes change under it (see the module
    /// docs), so it must not be turned into a Rust reference.
    #[must_use]
    pub fn as_ptr(&self) -> *mut u8 {
        self.mem.as_ptr()
    }

    /// Length of the mapping at [`Ram::as_ptr`] in bytes (equals `size()`).
    #[must_use]
    pub fn len(&self) -> usize {
        self.mem.len()
    }

    /// Always false: RAM is never empty. Present because [`Ram::len`] is.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        false
    }

    /// Whether `[pa, pa + len)` lies inside RAM.
    #[inline]
    #[must_use]
    pub fn contains(&self, pa: u32, len: usize) -> bool {
        (pa as usize) <= self.mem.len() && len <= self.mem.len() - pa as usize
    }

    /// Copies `buf.len()` bytes from physical `pa`. Returns `false`, copying
    /// nothing, if the range leaves RAM.
    #[inline]
    pub fn read(&self, pa: u32, buf: &mut [u8]) -> bool {
        if !self.contains(pa, buf.len()) {
            return false;
        }
        // SAFETY: the source range was just checked to lie inside the mapping,
        // which lives as long as `self`; `buf` is a distinct Rust allocation,
        // so the ranges cannot overlap.
        unsafe {
            std::ptr::copy_nonoverlapping(
                self.mem.as_ptr().add(pa as usize),
                buf.as_mut_ptr(),
                buf.len(),
            );
        }
        true
    }

    /// Copies `data` to physical `pa`. Returns `false`, writing nothing, if
    /// the range leaves RAM. Write tracking is not told (see
    /// [`crate::WriteTracker::set_dirty`]).
    #[inline]
    pub fn write(&self, pa: u32, data: &[u8]) -> bool {
        if !self.contains(pa, data.len()) {
            return false;
        }
        // SAFETY: the destination range lies inside the live mapping (checked
        // above); `data` is a distinct Rust allocation.
        unsafe {
            std::ptr::copy_nonoverlapping(
                data.as_ptr(),
                self.mem.as_ptr().add(pa as usize),
                data.len(),
            );
        }
        true
    }

    /// Sets `len` bytes at `pa` to `byte`. Returns `false`, writing nothing,
    /// if the range leaves RAM.
    pub fn fill(&self, pa: u32, byte: u8, len: usize) -> bool {
        if !self.contains(pa, len) {
            return false;
        }
        // SAFETY: the range lies inside the live mapping (checked above).
        unsafe {
            std::ptr::write_bytes(self.mem.as_ptr().add(pa as usize), byte, len);
        }
        true
    }

    /// Copies `len` bytes from `src` to `dst` inside RAM; the ranges may
    /// overlap (`memmove`). Returns `false`, copying nothing, if either range
    /// leaves RAM.
    pub fn copy_within(&self, src: u32, dst: u32, len: usize) -> bool {
        if !self.contains(src, len) || !self.contains(dst, len) {
            return false;
        }
        // SAFETY: both ranges lie inside the live mapping (checked above), and
        // `copy` allows them to overlap.
        unsafe {
            let base = self.mem.as_ptr();
            std::ptr::copy(base.add(src as usize), base.add(dst as usize), len);
        }
        true
    }

    /// Reads `len` bytes at `pa` into a new vector, or `None` if the range
    /// leaves RAM.
    #[must_use]
    pub fn read_vec(&self, pa: u32, len: usize) -> Option<Vec<u8>> {
        if !self.contains(pa, len) {
            return None;
        }
        let mut v = vec![0u8; len];
        self.read(pa, &mut v);
        Some(v)
    }

    /// Reads a byte, or `None` outside RAM.
    #[inline]
    #[must_use]
    pub fn read_u8(&self, pa: u32) -> Option<u8> {
        let mut b = [0u8; 1];
        self.read(pa, &mut b).then(|| b[0])
    }

    /// Reads a little-endian `u16`, or `None` outside RAM.
    #[inline]
    #[must_use]
    pub fn read_u16(&self, pa: u32) -> Option<u16> {
        let mut b = [0u8; 2];
        self.read(pa, &mut b).then(|| u16::from_le_bytes(b))
    }

    /// Reads a little-endian `u32`, or `None` outside RAM. (The trait's
    /// [`PhysMem::read_u32`] returns 0 there instead.)
    #[inline]
    #[must_use]
    pub fn read_u32(&self, pa: u32) -> Option<u32> {
        let mut b = [0u8; 4];
        self.read(pa, &mut b).then(|| u32::from_le_bytes(b))
    }

    /// Reads a little-endian `u64`, or `None` outside RAM.
    #[inline]
    #[must_use]
    pub fn read_u64(&self, pa: u32) -> Option<u64> {
        let mut b = [0u8; 8];
        self.read(pa, &mut b).then(|| u64::from_le_bytes(b))
    }

    /// Writes a byte; `false` outside RAM.
    #[inline]
    pub fn write_u8(&self, pa: u32, v: u8) -> bool {
        self.write(pa, &[v])
    }

    /// Writes a little-endian `u16`; `false` outside RAM.
    #[inline]
    pub fn write_u16(&self, pa: u32, v: u16) -> bool {
        self.write(pa, &v.to_le_bytes())
    }

    /// Writes a little-endian `u32`; `false` outside RAM.
    #[inline]
    pub fn write_u32(&self, pa: u32, v: u32) -> bool {
        self.write(pa, &v.to_le_bytes())
    }

    /// Writes a little-endian `u64`; `false` outside RAM.
    #[inline]
    pub fn write_u64(&self, pa: u32, v: u64) -> bool {
        self.write(pa, &v.to_le_bytes())
    }
}

impl PhysMem for Ram {
    fn size(&self) -> u32 {
        self.size
    }

    fn read(&self, pa: u32, buf: &mut [u8]) -> bool {
        Ram::read(self, pa, buf)
    }

    fn write(&self, pa: u32, data: &[u8]) -> bool {
        Ram::write(self, pa, data)
    }
}

impl std::fmt::Debug for Ram {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ram").field("size", &self.size).finish_non_exhaustive()
    }
}

impl Ram {
    /// Saves all of RAM, or replaces it with the stream's. The caller keeps
    /// the guest and every device that writes RAM stopped meanwhile. A
    /// stream of another size fails and changes nothing.
    pub fn state(&self, io: &mut xb_types::state::StateIo) {
        use xb_types::state::Stateful;
        let mut size = self.size;
        size.state(io);
        if io.loading() && size != self.size {
            io.fail(format!(
                "the state holds {} MiB of RAM, this machine has {}",
                size >> 20,
                self.size >> 20
            ));
            return;
        }
        // In pieces: the stream's own buffer is the only copy made.
        const PIECE: usize = 1 << 20;
        let mut piece = vec![0u8; PIECE];
        for pa in (0..self.size).step_by(PIECE) {
            let n = PIECE.min((self.size - pa) as usize);
            if io.loading() {
                io.bytes(&mut piece[..n]);
                if io.failed().is_some() {
                    return;
                }
                self.write(pa, &piece[..n]);
            } else {
                self.read(pa, &mut piece[..n]);
                io.bytes(&mut piece[..n]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_size_that_is_not_a_page_multiple_is_refused() {
        assert!(Ram::new(0).is_err());
        assert!(Ram::new(PAGE_SIZE + 1).is_err());
        assert!(Ram::new(Ram::MAX_SIZE + PAGE_SIZE).is_err());
    }

    #[test]
    fn new_ram_reads_as_zero_and_round_trips_writes() {
        let ram = Ram::new(16 * PAGE_SIZE).unwrap();
        assert_eq!(ram.read_u64(0x3FF8), Some(0));
        assert!(ram.write_u32(0x1FFE, 0xDEAD_BEEF)); // crosses a page
        assert_eq!(ram.read_u32(0x1FFE), Some(0xDEAD_BEEF));
        assert_eq!(ram.read_u16(0x2000), Some(0xDEAD));
        assert_eq!(ram.read_u8(0x1FFE), Some(0xEF));
    }

    #[test]
    fn a_range_leaving_ram_copies_nothing() {
        let ram = Ram::new(2 * PAGE_SIZE).unwrap();
        let end = ram.size();
        assert!(ram.write(end - 4, &[1, 2, 3, 4]));
        assert!(!ram.write(end - 3, &[9, 9, 9, 9]));
        assert_eq!(ram.read_vec(end - 4, 4).unwrap(), [1, 2, 3, 4]);
        let mut buf = [7u8; 8];
        assert!(!ram.read(end - 4, &mut buf));
        assert_eq!(buf, [7u8; 8]);
        assert!(!ram.read(u32::MAX, &mut buf));
        assert!(ram.read(end, &mut []));
        assert_eq!(ram.read_u32(end - 3), None);
        assert!(!ram.fill(end - 1, 0, 2));
    }

    #[test]
    fn the_trait_object_and_every_clone_see_the_same_bytes() {
        let ram = Ram::new(4 * PAGE_SIZE).unwrap();
        let other: Arc<dyn PhysMem> = Arc::new(ram.clone());
        other.write_u32(0x100, 0x1234_5678);
        assert_eq!(ram.read_u32(0x100), Some(0x1234_5678));
        assert_eq!(other.size(), ram.size());
        // The trait's default helpers read 0 outside RAM.
        assert_eq!(other.read_u32(ram.size()), 0);
    }

    #[test]
    fn a_second_mapping_of_the_memory_object_sees_writes_made_through_the_handle() {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::FileExt;
        let ram = Ram::new(4 * PAGE_SIZE).unwrap();
        ram.write(0x2010, b"shared");
        // Reading the memory object by descriptor stands in for the view a
        // CPU backend maps from it.
        let file = std::fs::File::from(ram.fd().try_clone_to_owned().unwrap());
        assert!(file.as_raw_fd() >= 0);
        let mut buf = [0u8; 6];
        file.read_exact_at(&mut buf, 0x2010).unwrap();
        assert_eq!(&buf, b"shared");
        file.write_all_at(b"back", 0x3000).unwrap();
        assert_eq!(ram.read_vec(0x3000, 4).unwrap(), b"back");
    }

    #[test]
    fn copy_within_handles_overlap_like_memmove() {
        let ram = Ram::new(PAGE_SIZE).unwrap();
        ram.write(0, &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert!(ram.copy_within(0, 2, 6));
        assert_eq!(ram.read_vec(0, 8).unwrap(), [1, 2, 1, 2, 3, 4, 5, 6]);
        assert!(ram.fill(4, 0xAA, 2));
        assert_eq!(ram.read_vec(3, 4).unwrap(), [2, 0xAA, 0xAA, 5]);
    }

    #[test]
    fn handles_can_be_used_from_several_threads() {
        let ram = Ram::new(8 * PAGE_SIZE).unwrap();
        std::thread::scope(|s| {
            for t in 0..4u32 {
                let ram = ram.clone();
                s.spawn(move || {
                    for i in 0..1000u32 {
                        ram.write_u32(t * PAGE_SIZE + (i % 512) * 4, t << 16 | i);
                    }
                });
            }
        });
        for t in 0..4u32 {
            assert_eq!(ram.read_u32(t * PAGE_SIZE + 487 * 4), Some(t << 16 | 999));
        }
    }
}
