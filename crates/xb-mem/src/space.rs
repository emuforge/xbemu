//! The guest's address space: a page table the kernel maintains.
//!
//! The reference backs the table with host mappings and dereferences guest
//! addresses. Here the table is bookkeeping only: every change is mirrored to
//! the CPU backend through a [`Mapper`], and host code reaches guest virtual
//! memory by walking the table into the RAM view ([`AddressSpace::read`],
//! [`AddressSpace::write`]). Those never fault: they stop at the first
//! unmapped page and say where.
//!
//! Host access ignores the guest's page protection (the kernel reads and
//! writes a title's read-only and no-access pages) and never touches device
//! register pages. It does not tell write tracking either: a caller storing
//! into guest memory on the guest's behalf reports the physical ranges
//! ([`AddressSpace::phys_runs`]) to [`crate::WriteTracker::host_store`].

use xb_types::mem::{Access, Mapper};
use xb_types::{PAGE_SHIFT, PAGE_SIZE};

use crate::phys::{PhysAllocator, TOP_RESERVE, find_window};
use crate::ram::Ram;

const OFFSET_MASK: u32 = PAGE_SIZE - 1;
const VA_PAGES: usize = 1 << (32 - PAGE_SHIFT);

const PTE_PRESENT: u32 = 1 << 0;
const PTE_WRITE: u32 = 1 << 1;
const PTE_MMIO: u32 = 1 << 2;
const PTE_WINDOW: u32 = 1 << 3;
const PTE_NOACCESS: u32 = 1 << 4;
const PTE_FRAME: u32 = !OFFSET_MASK;

/// One page-table entry.
///
/// Encoding (private to this crate; use the accessors): bits 31..12 are the
/// physical page frame, bit 0 present, bit 1 guest-writable, bit 2 device
/// registers (the frame is then a bus address, not RAM), bit 3 a window
/// mapping (an alias of RAM that the reverse map ignores), bit 4 no guest
/// access. An unmapped page is all zeroes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pte(u32);

impl Pte {
    fn new(pa: u32, flags: u32) -> Self {
        Self((pa & PTE_FRAME) | flags | PTE_PRESENT)
    }

    fn for_access(pa: u32, access: Access, window: bool) -> Self {
        let a = match access {
            Access::None => PTE_NOACCESS,
            Access::Read => 0,
            Access::ReadWrite => PTE_WRITE,
        };
        Self::new(pa, a | if window { PTE_WINDOW } else { 0 })
    }

    /// Whether anything is mapped.
    #[inline]
    #[must_use]
    pub fn is_present(self) -> bool {
        self.0 & PTE_PRESENT != 0
    }

    /// Whether the page is RAM (present and not device registers).
    #[inline]
    #[must_use]
    pub fn is_ram(self) -> bool {
        self.0 & (PTE_PRESENT | PTE_MMIO) == PTE_PRESENT
    }

    /// Whether the page is device registers.
    #[inline]
    #[must_use]
    pub fn is_mmio(self) -> bool {
        self.0 & (PTE_PRESENT | PTE_MMIO) == PTE_PRESENT | PTE_MMIO
    }

    /// Whether the mapping is a window ([`AddressSpace::map_window`]).
    #[must_use]
    pub fn is_window(self) -> bool {
        self.0 & PTE_WINDOW != 0
    }

    /// Physical address of the page (page aligned); meaningful when present.
    #[inline]
    #[must_use]
    pub fn pa(self) -> u32 {
        self.0 & PTE_FRAME
    }

    /// What the page table lets the guest do. Device pages report
    /// `ReadWrite`; an unmapped page `None`.
    #[must_use]
    pub fn access(self) -> Access {
        if !self.is_present() || self.0 & PTE_NOACCESS != 0 {
            Access::None
        } else if self.0 & (PTE_WRITE | PTE_MMIO) != 0 {
            Access::ReadWrite
        } else {
            Access::Read
        }
    }
}

/// A mapping request the table refused. Nothing was changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapError {
    /// An address or length is not a multiple of the page size.
    Unaligned,
    /// The virtual range runs past 4 GiB.
    OutOfRange,
    /// The physical range leaves RAM.
    OutsideRam,
    /// Not enough free physical pages (the allocating calls).
    OutOfMemory,
}

impl std::fmt::Display for MapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Unaligned => "address or length not page aligned",
            Self::OutOfRange => "virtual range runs past 4 GiB",
            Self::OutsideRam => "physical range leaves RAM",
            Self::OutOfMemory => "out of physical memory",
        })
    }
}

impl std::error::Error for MapError {}

/// A host access by guest virtual address met a page that is not RAM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemFault {
    /// The first address that could not be accessed.
    pub va: u32,
    /// Bytes copied before it (the access is not undone).
    pub done: usize,
}

impl std::fmt::Display for MemFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "guest address {:#010x} is not mapped ({} bytes done)", self.va, self.done)
    }
}

impl std::error::Error for MemFault {}

/// A contiguous physical allocation that may have relocated pages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Contig {
    /// Physical address of the block.
    pub pa: u32,
    /// Pages moved out of its way.
    pub moved: u32,
}

/// Where a contiguous physical allocation may go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContigRequest {
    /// Bytes wanted (rounded up to pages; 0 takes one page).
    pub len: u32,
    /// Lowest acceptable physical address.
    pub lo: u32,
    /// End of the acceptable range (exclusive), clamped to the end of RAM;
    /// 0 means no upper bound, as in the reference.
    pub hi: u32,
    /// Alignment in bytes (a power of two; less than a page means a page).
    pub align: u32,
}

impl ContigRequest {
    /// `len` bytes anywhere in RAM at page alignment.
    #[must_use]
    pub fn anywhere(len: u32) -> Self {
        Self { len, lo: 0, hi: 0, align: PAGE_SIZE }
    }
}

/// What the relocating allocators ask of write tracking and the GPU when a
/// page moves.
///
/// The machine implements it over [`crate::WriteTracker`] and the GPU:
///
/// * `movable`: `!tracker.has_noaccess(pa)`. A page without guest access is
///   older than what the GPU will put there at the next access; it neither
///   moves nor receives a moved page.
/// * `before_move`: when either page is tracked (not `tracker.open`), wait
///   for the GPU work that still reads them in place (the later of the two
///   pages' write fences).
/// * `after_move`: `tracker.set_dirty` for both pages. Project Gotham
///   Racing 2's image pages, copied unreported onto freed textures, took over
///   their tracking and never moved again.
///
/// `()` implements it with every page movable and no reports, for machines
/// without a GPU and for tests.
pub trait RelocateHooks {
    /// Whether the page at `pa` may move or receive a moved page.
    fn movable(&self, _pa: u32) -> bool {
        true
    }
    /// The page at `from` is about to be copied to `to`.
    fn before_move(&mut self, _from: u32, _to: u32) {}
    /// The page at `from` was copied to `to` and its virtual address now maps
    /// `to`.
    fn after_move(&mut self, _from: u32, _to: u32) {}
}

impl RelocateHooks for () {}

/// The guest's page table: one entry per 4 KiB page of the 4 GiB space.
///
/// Used on the guest-CPU thread only, while the CPU is not running guest
/// code. Every change goes to the [`Mapper`] passed in (the CPU backend),
/// which enforces it for the guest.
pub struct AddressSpace {
    ram: Ram,
    pte: Box<[u32]>,
    /// Per physical page: the non-window virtual address last mapped onto it,
    /// tagged with bit 0, or 0.
    rmap: Box<[u32]>,
    warned: u32,
}

impl AddressSpace {
    /// An empty address space over `ram`.
    ///
    /// Nothing is mapped: the caller maps the contiguous window
    /// (`CONTIG_WINDOW + pa` for all of RAM) and the NV2A's view at
    /// `0xF000_0000` with [`AddressSpace::map_window`], as the reference does.
    #[must_use]
    pub fn new(ram: Ram) -> Self {
        let pages = ram.pages() as usize;
        Self {
            ram,
            pte: vec![0u32; VA_PAGES].into_boxed_slice(),
            rmap: vec![0u32; pages].into_boxed_slice(),
            warned: 0,
        }
    }

    /// The RAM this space maps.
    #[must_use]
    pub fn ram(&self) -> &Ram {
        &self.ram
    }

    fn check_va(va: u32, len: u32) -> Result<(), MapError> {
        if !(va | len).is_multiple_of(PAGE_SIZE) {
            return Err(MapError::Unaligned);
        }
        if u64::from(va) + u64::from(len) > 1 << 32 {
            return Err(MapError::OutOfRange);
        }
        Ok(())
    }

    fn check_ram(&self, va: u32, pa: u32, len: u32) -> Result<(), MapError> {
        Self::check_va(va | (pa & OFFSET_MASK), len)?;
        if u64::from(pa) + u64::from(len) > u64::from(self.ram.size()) {
            return Err(MapError::OutsideRam);
        }
        Ok(())
    }

    /// Drops the reverse-map entry that names `va` for the page `va` maps.
    fn forget(&mut self, va: u32) {
        let e = Pte(self.pte[(va >> PAGE_SHIFT) as usize]);
        if e.is_ram() {
            let r = &mut self.rmap[(e.pa() >> PAGE_SHIFT) as usize];
            if *r == va | 1 {
                *r = 0;
            }
        }
    }

    fn set_ram(&mut self, va: u32, pa: u32, len: u32, access: Access, window: bool) {
        for off in (0..len).step_by(PAGE_SIZE as usize) {
            let (v, p) = (va + off, pa + off);
            self.forget(v);
            self.pte[(v >> PAGE_SHIFT) as usize] = Pte::for_access(p, access, window).0;
            if !window {
                // A page at a second address: two allocations sharing it
                // corrupt each other.
                if let Some(prev) = self.va_of(p)
                    && prev != v
                    && self.warned < 16
                {
                    self.warned += 1;
                    log::warn!(target: "mem", "pa {p:08x} mapped at {prev:08x} is mapped again at {v:08x}");
                }
                self.rmap[(p >> PAGE_SHIFT) as usize] = v | 1;
            }
        }
    }

    /// Maps guest virtual `[va, va + len)` onto RAM `[pa, pa + len)`,
    /// replacing what was mapped there, and records `va` as the address of
    /// each physical page. All arguments are page aligned.
    pub fn map(
        &mut self,
        mapper: &mut dyn Mapper,
        va: u32,
        pa: u32,
        len: u32,
        access: Access,
    ) -> Result<(), MapError> {
        self.check_ram(va, pa, len)?;
        if len != 0 {
            self.set_ram(va, pa, len, access, false);
            mapper.map(va, pa, len, access);
        }
        Ok(())
    }

    /// Maps an alias of RAM that is not a page's own address: the contiguous
    /// window at `0x8000_0000` and the NV2A's view at `0xF000_0000`. The
    /// reverse map ignores it, as the reference's does for those two, so a page
    /// reached only through a window never counts as movable.
    pub fn map_window(
        &mut self,
        mapper: &mut dyn Mapper,
        va: u32,
        pa: u32,
        len: u32,
        access: Access,
    ) -> Result<(), MapError> {
        self.check_ram(va, pa, len)?;
        if len != 0 {
            self.set_ram(va, pa, len, access, true);
            mapper.map(va, pa, len, access);
        }
        Ok(())
    }

    /// Maps `[va, va + len)` as device registers at bus address
    /// `[pa, pa + len)`. Host reads and writes by virtual address stop at
    /// such a page.
    pub fn map_mmio(
        &mut self,
        mapper: &mut dyn Mapper,
        va: u32,
        pa: u32,
        len: u32,
    ) -> Result<(), MapError> {
        Self::check_va(va | (pa & OFFSET_MASK), len)?;
        if u64::from(pa) + u64::from(len) > 1 << 32 {
            return Err(MapError::OutOfRange);
        }
        for off in (0..len).step_by(PAGE_SIZE as usize) {
            self.forget(va + off);
            self.pte[((va + off) >> PAGE_SHIFT) as usize] = Pte::new(pa + off, PTE_MMIO).0;
        }
        if len != 0 {
            mapper.map_mmio(va, pa, len);
        }
        Ok(())
    }

    /// Removes the mappings of `[va, va + len)`. The mapper is told once per
    /// run of mapped pages, so a sparse range (a reserved region with few
    /// pages committed) costs little. Returns the pages that were mapped.
    pub fn unmap(&mut self, mapper: &mut dyn Mapper, va: u32, len: u32) -> Result<u32, MapError> {
        Self::check_va(va, len)?;
        let end = u64::from(va) + u64::from(len);
        let mut removed = 0;
        let mut a = u64::from(va);
        while a < end {
            let (mapped, run) = self.run_at(a as u32, (end - a) as u32);
            if mapped {
                for v in (a as u32..=(a + u64::from(run) - 1) as u32).step_by(PAGE_SIZE as usize) {
                    self.forget(v);
                    self.pte[(v >> PAGE_SHIFT) as usize] = 0;
                }
                mapper.unmap(a as u32, run);
                removed += run / PAGE_SIZE;
            }
            a += u64::from(run);
        }
        Ok(removed)
    }

    /// Changes what the guest may do with the RAM pages mapped in
    /// `[va, va + len)`; unmapped and device pages are skipped. The mapper
    /// gets one `map` per run of pages that changed. Returns the pages
    /// changed.
    ///
    /// This is the title's own protection (`NtProtectVirtualMemory`,
    /// `PAGE_READONLY`, `PAGE_NOACCESS`). A store to a read-only page then
    /// faults with a mapping that forbids it ([`AddressSpace::allows`]) and
    /// belongs to the title, not to write tracking.
    pub fn protect(
        &mut self,
        mapper: &mut dyn Mapper,
        va: u32,
        len: u32,
        access: Access,
    ) -> Result<u32, MapError> {
        Self::check_va(va, len)?;
        let mut changed = 0;
        let mut run: Option<(u32, u32, u32)> = None; // va, pa, len
        for off in (0..len).step_by(PAGE_SIZE as usize) {
            let v = va + off;
            let i = (v >> PAGE_SHIFT) as usize;
            let e = Pte(self.pte[i]);
            if !e.is_ram() || e.access() == access {
                continue;
            }
            self.pte[i] = Pte::for_access(e.pa(), access, e.is_window()).0;
            changed += 1;
            match &mut run {
                Some((rv, rp, rl)) if *rv + *rl == v && *rp + *rl == e.pa() => *rl += PAGE_SIZE,
                _ => {
                    if let Some((rv, rp, rl)) = run.replace((v, e.pa(), PAGE_SIZE)) {
                        mapper.map(rv, rp, rl, access);
                    }
                }
            }
        }
        if let Some((rv, rp, rl)) = run {
            mapper.map(rv, rp, rl, access);
        }
        Ok(changed)
    }

    /// The entry for the page holding `va`.
    #[inline]
    #[must_use]
    pub fn entry(&self, va: u32) -> Pte {
        Pte(self.pte[(va >> PAGE_SHIFT) as usize])
    }

    /// The physical address behind `va`, if a RAM page is mapped there.
    /// Device pages give `None`.
    #[inline]
    #[must_use]
    pub fn translate(&self, va: u32) -> Option<u32> {
        let e = self.entry(va);
        e.is_ram().then(|| e.pa() | (va & OFFSET_MASK))
    }

    /// Whether anything (RAM or device registers) is mapped at `va`.
    #[inline]
    #[must_use]
    pub fn is_mapped(&self, va: u32) -> bool {
        self.entry(va).is_present()
    }

    /// Whether the page table alone lets the guest read (`write` false) or
    /// write `va`. The machine's page-fault handler asks this first: a fault
    /// on an access the table forbids is the title's own, and only one the
    /// table allows can be write tracking's.
    #[must_use]
    pub fn allows(&self, va: u32, write: bool) -> bool {
        match self.entry(va).access() {
            Access::None => false,
            Access::Read => !write,
            Access::ReadWrite => true,
        }
    }

    /// The virtual address that maps the RAM page holding `pa`, other than
    /// the windows: the last one mapped, if it still maps the page. The
    /// offset in the page is kept.
    #[must_use]
    pub fn va_of(&self, pa: u32) -> Option<u32> {
        let r = *self.rmap.get((pa >> PAGE_SHIFT) as usize)?;
        if r == 0 {
            return None;
        }
        let va = r & PTE_FRAME;
        let e = self.entry(va);
        (e.is_ram() && e.pa() == pa & PTE_FRAME).then_some(va | (pa & OFFSET_MASK))
    }

    /// Whether the page at `va` is mapped, and for how many bytes from `va`
    /// (rounded down to its page) that stays the same, at most `limit` bytes.
    fn run_at(&self, va: u32, limit: u32) -> (bool, u32) {
        let first = (va >> PAGE_SHIFT) as usize;
        let n = (limit.div_ceil(PAGE_SIZE) as usize).min(VA_PAGES - first).max(1);
        let mapped = self.pte[first] & PTE_PRESENT != 0;
        let len = self.pte[first..first + n]
            .iter()
            .position(|&e| (e & PTE_PRESENT != 0) != mapped)
            .unwrap_or(n);
        (mapped, (len as u32) << PAGE_SHIFT)
    }

    /// Whether the page at `va` is mapped, and the length in bytes of the run
    /// of pages from it in the same state, looking at most `limit` bytes
    /// ahead (both page aligned). The kernel walks a region's committed and
    /// uncommitted runs with it (`NtQueryVirtualMemory`, commit, decommit).
    #[must_use]
    pub fn mapped_run(&self, va: u32, limit: u32) -> (bool, u32) {
        self.run_at(va, limit)
    }

    /// The number of mapped pages in `[va, va + len)`.
    #[must_use]
    pub fn mapped_pages(&self, va: u32, len: u32) -> u32 {
        let first = (va >> PAGE_SHIFT) as usize;
        let n = ((u64::from(va & OFFSET_MASK) + u64::from(len)).div_ceil(u64::from(PAGE_SIZE))
            as usize)
            .min(VA_PAGES - first);
        self.pte[first..first + n].iter().filter(|&&e| e & PTE_PRESENT != 0).count() as u32
    }

    /// Finds `len` bytes of virtual addresses with nothing mapped inside
    /// `[lo, hi)`, at a multiple of `align` (a power of two; less than a page
    /// means a page): the lowest, or the highest with `top_down`. `hi == 0`
    /// means the end of the address space.
    ///
    /// Only the page table is consulted. Address ranges the kernel has
    /// reserved without committing pages are its own to exclude (it searches
    /// its region list).
    #[must_use]
    pub fn find_free(&self, len: u32, lo: u32, hi: u32, align: u32, top_down: bool) -> Option<u32> {
        let pages = len.max(1).div_ceil(PAGE_SIZE);
        let step = align.max(PAGE_SIZE) / PAGE_SIZE;
        let first = (lo.div_ceil(PAGE_SIZE)).div_ceil(step).checked_mul(step)?;
        let last = if hi == 0 { VA_PAGES as u32 } else { hi / PAGE_SIZE };
        find_window(pages, step, first, last, top_down, |p| self.pte[p as usize] & PTE_PRESENT != 0)
            .map(|p| p << PAGE_SHIFT)
    }

    /// The physical ranges behind `[va, va + len)` as `(pa, bytes)`, each as
    /// long as the pages stay physically consecutive. Stops before the first
    /// page that is not RAM, so the lengths sum to less than `len` then.
    pub fn phys_runs(&self, va: u32, len: u32) -> impl Iterator<Item = (u32, u32)> + '_ {
        let mut a = va;
        let mut left = len;
        std::iter::from_fn(move || {
            if left == 0 {
                return None;
            }
            let start = self.translate(a)?;
            let mut n = 0u32;
            while left != 0 && self.translate(a) == Some(start.wrapping_add(n)) {
                let step = (PAGE_SIZE - (a & OFFSET_MASK)).min(left);
                n += step;
                left -= step;
                a = a.wrapping_add(step);
            }
            Some((start, n))
        })
    }

    // ---- host access by guest virtual address ------------------------------

    /// Copies `buf.len()` bytes from guest virtual `va`. Stops at the first
    /// page that is not RAM and reports it; the bytes before it were copied.
    /// The address wraps at 4 GiB as the guest's does.
    pub fn read(&self, va: u32, buf: &mut [u8]) -> Result<(), MemFault> {
        let mut done = 0;
        let mut a = va;
        while done < buf.len() {
            let n = ((PAGE_SIZE - (a & OFFSET_MASK)) as usize).min(buf.len() - done);
            let Some(pa) = self.translate(a) else {
                return Err(MemFault { va: a, done });
            };
            let ok = self.ram.read(pa, &mut buf[done..done + n]);
            debug_assert!(ok, "a RAM entry points outside RAM");
            done += n;
            a = a.wrapping_add(n as u32);
        }
        Ok(())
    }

    /// Copies `data` to guest virtual `va`. Stops at the first page that is
    /// not RAM and reports it; the bytes before it were written. The guest's
    /// page protection is ignored and write tracking is not told (module
    /// docs).
    pub fn write(&self, va: u32, data: &[u8]) -> Result<(), MemFault> {
        let mut done = 0;
        let mut a = va;
        while done < data.len() {
            let n = ((PAGE_SIZE - (a & OFFSET_MASK)) as usize).min(data.len() - done);
            let Some(pa) = self.translate(a) else {
                return Err(MemFault { va: a, done });
            };
            let ok = self.ram.write(pa, &data[done..done + n]);
            debug_assert!(ok, "a RAM entry points outside RAM");
            done += n;
            a = a.wrapping_add(n as u32);
        }
        Ok(())
    }

    /// Sets `len` bytes at guest virtual `va` to `byte`, with the stopping
    /// rule of [`AddressSpace::write`].
    pub fn fill(&self, va: u32, byte: u8, len: u32) -> Result<(), MemFault> {
        let mut done = 0u32;
        let mut a = va;
        while done < len {
            let n = (PAGE_SIZE - (a & OFFSET_MASK)).min(len - done);
            let Some(pa) = self.translate(a) else {
                return Err(MemFault { va: a, done: done as usize });
            };
            self.ram.fill(pa, byte, n as usize);
            done += n;
            a = a.wrapping_add(n);
        }
        Ok(())
    }

    /// Copies `len` bytes from guest virtual `src` to `dst` (`memmove`: the
    /// ranges may overlap). Nothing is written if a source page is unmapped.
    pub fn copy(&self, dst: u32, src: u32, len: u32) -> Result<(), MemFault> {
        let bytes = self.read_bytes(src, len)?;
        self.write(dst, &bytes)
    }

    /// Reads `len` bytes at `va` into a new vector.
    pub fn read_bytes(&self, va: u32, len: u32) -> Result<Vec<u8>, MemFault> {
        let mut v = vec![0u8; len as usize];
        self.read(va, &mut v)?;
        Ok(v)
    }

    #[inline]
    fn read_array<const N: usize>(&self, va: u32) -> Result<[u8; N], MemFault> {
        let mut b = [0u8; N];
        // One table lookup when the value lies in one page.
        if (va & OFFSET_MASK) as usize + N <= PAGE_SIZE as usize {
            let pa = self.translate(va).ok_or(MemFault { va, done: 0 })?;
            self.ram.read(pa, &mut b);
        } else {
            self.read(va, &mut b)?;
        }
        Ok(b)
    }

    #[inline]
    fn write_array<const N: usize>(&self, va: u32, b: [u8; N]) -> Result<(), MemFault> {
        if (va & OFFSET_MASK) as usize + N <= PAGE_SIZE as usize {
            let pa = self.translate(va).ok_or(MemFault { va, done: 0 })?;
            self.ram.write(pa, &b);
            Ok(())
        } else {
            self.write(va, &b)
        }
    }

    /// Reads a byte at guest virtual `va`.
    #[inline]
    pub fn read_u8(&self, va: u32) -> Result<u8, MemFault> {
        self.read_array::<1>(va).map(|b| b[0])
    }

    /// Reads a little-endian `u16` at guest virtual `va`.
    #[inline]
    pub fn read_u16(&self, va: u32) -> Result<u16, MemFault> {
        self.read_array(va).map(u16::from_le_bytes)
    }

    /// Reads a little-endian `u32` at guest virtual `va`.
    #[inline]
    pub fn read_u32(&self, va: u32) -> Result<u32, MemFault> {
        self.read_array(va).map(u32::from_le_bytes)
    }

    /// Reads a little-endian `u64` at guest virtual `va`.
    #[inline]
    pub fn read_u64(&self, va: u32) -> Result<u64, MemFault> {
        self.read_array(va).map(u64::from_le_bytes)
    }

    /// Writes a byte at guest virtual `va`.
    #[inline]
    pub fn write_u8(&self, va: u32, v: u8) -> Result<(), MemFault> {
        self.write_array(va, [v])
    }

    /// Writes a little-endian `u16` at guest virtual `va`.
    #[inline]
    pub fn write_u16(&self, va: u32, v: u16) -> Result<(), MemFault> {
        self.write_array(va, v.to_le_bytes())
    }

    /// Writes a little-endian `u32` at guest virtual `va`.
    #[inline]
    pub fn write_u32(&self, va: u32, v: u32) -> Result<(), MemFault> {
        self.write_array(va, v.to_le_bytes())
    }

    /// Writes a little-endian `u64` at guest virtual `va`.
    #[inline]
    pub fn write_u64(&self, va: u32, v: u64) -> Result<(), MemFault> {
        self.write_array(va, v.to_le_bytes())
    }

    /// Reads a NUL-terminated byte string at `va`, without the NUL. If no NUL
    /// comes within `max` bytes the first `max` bytes are returned. Pages are
    /// read whole up to the terminator, and none beyond the one holding it.
    pub fn read_cstr(&self, va: u32, max: u32) -> Result<Vec<u8>, MemFault> {
        let mut out = Vec::new();
        let mut a = va;
        while (out.len() as u32) < max {
            let n = (PAGE_SIZE - (a & OFFSET_MASK)).min(max - out.len() as u32) as usize;
            let Some(pa) = self.translate(a) else {
                return Err(MemFault { va: a, done: out.len() });
            };
            let at = out.len();
            out.resize(at + n, 0);
            self.ram.read(pa, &mut out[at..]);
            if let Some(nul) = out[at..].iter().position(|&b| b == 0) {
                out.truncate(at + nul);
                break;
            }
            a = a.wrapping_add(n as u32);
        }
        Ok(out)
    }

    /// Reads a NUL-terminated UTF-16LE string at `va`, without the NUL, of at
    /// most `max_units` code units. The string need not be aligned.
    pub fn read_wstr(&self, va: u32, max_units: u32) -> Result<Vec<u16>, MemFault> {
        let mut out = Vec::new();
        let mut bytes: Vec<u8> = Vec::new();
        let mut a = va;
        let max_bytes = max_units as usize * 2;
        while out.len() < max_units as usize {
            let n = ((PAGE_SIZE - (a & OFFSET_MASK)) as usize).min(max_bytes - bytes.len());
            let Some(pa) = self.translate(a) else {
                return Err(MemFault { va: a, done: bytes.len() });
            };
            let at = bytes.len();
            bytes.resize(at + n, 0);
            self.ram.read(pa, &mut bytes[at..]);
            a = a.wrapping_add(n as u32);
            while out.len() < max_units as usize && (out.len() + 1) * 2 <= bytes.len() {
                let i = out.len() * 2;
                let unit = u16::from_le_bytes([bytes[i], bytes[i + 1]]);
                if unit == 0 {
                    return Ok(out);
                }
                out.push(unit);
            }
        }
        Ok(out)
    }

    /// Reads the bytes of the counted string whose descriptor is at `va`: the
    /// kernel's `STRING` / `ANSI_STRING` / `OBJECT_STRING`
    /// (`u16 Length; u16 MaximumLength; u32 Buffer`).
    pub fn read_ansi_string(&self, va: u32) -> Result<Vec<u8>, MemFault> {
        let len = self.read_u16(va)?;
        let buffer = self.read_u32(va.wrapping_add(4))?;
        self.read_bytes(buffer, u32::from(len))
    }

    /// Reads the code units of the `UNICODE_STRING` whose descriptor is at
    /// `va` (same layout; `Length` is in bytes).
    pub fn read_unicode_string(&self, va: u32) -> Result<Vec<u16>, MemFault> {
        let len = self.read_u16(va)?;
        let buffer = self.read_u32(va.wrapping_add(4))?;
        let bytes = self.read_bytes(buffer, u32::from(len & !1))?;
        Ok(bytes.as_chunks::<2>().0.iter().map(|&c| u16::from_le_bytes(c)).collect())
    }

    /// Writes `s` and a terminating NUL at `va`.
    pub fn write_cstr(&self, va: u32, s: &[u8]) -> Result<(), MemFault> {
        self.write(va, s)?;
        self.write_u8(va.wrapping_add(s.len() as u32), 0)
            .map_err(|f| MemFault { va: f.va, done: s.len() })
    }

    // ---- allocation that needs the page table -------------------------------

    /// Pins as known every page of physical `[pa, pa + len)` that a virtual
    /// address still maps, and returns how many there were.
    ///
    /// Such a page was handed out while its old owner can still write it.
    /// That address is not the new block's, so the page must stay put until
    /// freed: left movable, it was taken from under a block reached through
    /// the contiguous window (Hitman 2 maps all of RAM again at `0x88000000`
    /// through its own page tables). The allocating methods here call it;
    /// the kernel calls it after taking pages from [`PhysAllocator`]
    /// directly.
    pub fn pin_still_mapped(&mut self, phys: &mut PhysAllocator, pa: u32, len: u32) -> u32 {
        let mut n = 0;
        for p in (pa >> PAGE_SHIFT)..(pa >> PAGE_SHIFT) + len.div_ceil(PAGE_SIZE) {
            if let Some(va) = self.va_of(p << PAGE_SHIFT) {
                phys.pin(p << PAGE_SHIFT, true, 0);
                n += 1;
                if self.warned < 16 {
                    self.warned += 1;
                    log::warn!(target: "mem", "allocated pa {:08x} while {va:08x} still maps it", p << PAGE_SHIFT);
                }
            }
        }
        n
    }

    /// Allocates zeroed pages and maps them at `[va, va + len)`, replacing
    /// what was mapped there.
    ///
    /// Any free pages back the range, the highest below [`TOP_RESERVE`]
    /// first ([`PhysAllocator::alloc_high_run`]). All or nothing: if RAM runs
    /// out, the pages already mapped are unmapped and freed.
    pub fn alloc_at(
        &mut self,
        phys: &mut PhysAllocator,
        mapper: &mut dyn Mapper,
        va: u32,
        len: u32,
        access: Access,
    ) -> Result<(), MapError> {
        let len = Self::alloc_len(va, len, phys)?;
        let mut done = 0;
        while done < len {
            let Some((pa, piece)) = phys.alloc_high_run(len - done) else {
                log::warn!(target: "mem", "out of physical memory for {} KiB at {va:08x} ({} KiB free)",
                    len >> 10, phys.free_bytes() >> 10);
                self.free_at(phys, mapper, va, done);
                return Err(MapError::OutOfMemory);
            };
            self.pin_still_mapped(phys, pa, piece);
            self.set_ram(va + done, pa, piece, access, false);
            mapper.map(va + done, pa, piece, access);
            self.ram.fill(pa, 0, piece as usize);
            done += piece;
        }
        Ok(())
    }

    /// Allocates one zeroed physical run and maps it at `[va, va + len)`;
    /// returns its physical address (best fit, [`PhysAllocator::alloc`]).
    pub fn alloc_at_contiguous(
        &mut self,
        phys: &mut PhysAllocator,
        mapper: &mut dyn Mapper,
        va: u32,
        len: u32,
        access: Access,
    ) -> Result<u32, MapError> {
        let len = Self::alloc_len(va, len, phys)?;
        let pa = phys.alloc(len, 0, u32::MAX).ok_or(MapError::OutOfMemory)?;
        self.pin_still_mapped(phys, pa, len);
        if len != 0 {
            self.set_ram(va, pa, len, access, false);
            mapper.map(va, pa, len, access);
            self.ram.fill(pa, 0, len as usize);
        }
        Ok(pa)
    }

    /// `len` rounded up to pages, after the checks the allocating maps share.
    fn alloc_len(va: u32, len: u32, phys: &PhysAllocator) -> Result<u32, MapError> {
        if len > phys.ram_size() {
            return Err(MapError::OutOfMemory); // and the rounding below cannot wrap
        }
        let len = len.div_ceil(PAGE_SIZE) * PAGE_SIZE;
        Self::check_va(va, len)?;
        Ok(len)
    }

    /// Unmaps `[va, va + len)` and frees the RAM pages that were mapped there
    /// (the kernel's decommit: one `Mapper::unmap` per run of mapped pages,
    /// then their physical pages). Returns the pages freed. Window mappings
    /// and device pages are unmapped without freeing anything.
    pub fn free_at(
        &mut self,
        phys: &mut PhysAllocator,
        mapper: &mut dyn Mapper,
        va: u32,
        len: u32,
    ) -> u32 {
        let end =
            (u64::from(va) + u64::from(len)).div_ceil(u64::from(PAGE_SIZE)) * u64::from(PAGE_SIZE);
        let end = end.min(1 << 32);
        let va = va & PTE_FRAME;
        let mut freed = Vec::new();
        for v in (u64::from(va)..end).step_by(PAGE_SIZE as usize) {
            let e = self.entry(v as u32);
            if e.is_ram() && !e.is_window() {
                freed.push(e.pa());
            }
        }
        // Aligned and inside 4 GiB by construction, so this cannot fail.
        let _ = self.unmap(mapper, va, (end - u64::from(va)) as u32);
        for &pa in &freed {
            phys.free(pa, PAGE_SIZE);
        }
        freed.len() as u32
    }

    /// Whether used page `p` can move: mapped at exactly the one address its
    /// reverse map names, not pinned (or, with `known_too`, pinned only by
    /// its address having gone out), and with guest access.
    fn movable(
        &self,
        phys: &PhysAllocator,
        hooks: &dyn RelocateHooks,
        p: u32,
        known_too: bool,
    ) -> bool {
        (phys.pin_raw(p) == 0 || (known_too && phys.pin_only_known(p)))
            && self.va_of(p << PAGE_SHIFT).is_some()
            && hooks.movable(p << PAGE_SHIFT)
    }

    /// Moves page `q`'s contents, and the address mapping it, to free page
    /// `to`. The page keeps its access; its pins go with it.
    fn relocate(
        &mut self,
        phys: &mut PhysAllocator,
        mapper: &mut dyn Mapper,
        hooks: &mut dyn RelocateHooks,
        q: u32,
        to: u32,
    ) -> bool {
        let (from_pa, to_pa) = (q << PAGE_SHIFT, to << PAGE_SHIFT);
        let Some(va) = self.va_of(from_pa) else {
            return false;
        };
        let access = self.entry(va).access();
        hooks.before_move(from_pa, to_pa);
        self.ram.copy_within(from_pa, to_pa, PAGE_SIZE as usize);
        phys.set_pages(to, 1, true);
        self.set_ram(va, to_pa, PAGE_SIZE, access, false);
        mapper.map(va, to_pa, PAGE_SIZE, access);
        phys.set_pages(q, 1, false);
        phys.move_pin(q, to);
        hooks.after_move(from_pa, to_pa);
        true
    }

    /// Allocates contiguous physical pages as the Xbox kernel's
    /// `MmAllocateContiguousMemory` places them: the highest window in
    /// `[lo, hi)` at `align` whose pages are free or can move, the ones that
    /// can moved to the highest free pages outside it, below [`TOP_RESERVE`]
    /// while there are any.
    ///
    /// What never moves packs at the top of RAM, virtual memory below it, and
    /// what is free gathers in one run at the bottom. (Fable's new game asks
    /// for 6.7 MiB with 15 MiB free.) `hi == 0` means no upper bound. `None`
    /// if no window can be freed or there is nowhere to move its pages; the
    /// kernel then tries [`AddressSpace::alloc_contig_compact`].
    pub fn alloc_contig_top(
        &mut self,
        phys: &mut PhysAllocator,
        mapper: &mut dyn Mapper,
        hooks: &mut dyn RelocateHooks,
        req: ContigRequest,
    ) -> Option<Contig> {
        let ContigRequest { len, lo, hi, align } = req;
        if len > phys.ram_size() {
            return None;
        }
        let pages = len.max(1).div_ceil(PAGE_SIZE);
        let (step, first, last) = phys.window_bounds(lo, hi, align);
        if last < pages || first > last - pages {
            return None;
        }
        // A window over a page that can't move fails, and so does every other
        // window over it: the next candidate is the highest that ends below
        // it, so each page is looked at once.
        let mut p = (last - pages) / step * step;
        let mut used;
        loop {
            if p < first {
                return None;
            }
            used = 0;
            let mut blocker = None;
            for q in (p..p + pages).rev() {
                if phys.page_used(q) {
                    if !self.movable(phys, hooks, q, false) {
                        blocker = Some(q);
                        break;
                    }
                    used += 1;
                }
            }
            let Some(u) = blocker else {
                break;
            };
            if u < first + pages {
                return None;
            }
            p = (u - pages) / step * step;
        }
        if used != 0 && phys.free_pages() - (pages - used) < used {
            return None; // nowhere to move them
        }
        let ram_pages = phys.pages();
        let below = if TOP_RESERVE / PAGE_SIZE < ram_pages {
            ram_pages - TOP_RESERVE / PAGE_SIZE
        } else {
            ram_pages
        };
        let mut to = i64::from(below);
        let mut reserve = false;
        let mut moved = 0;
        for q in p..p + pages {
            if !phys.page_used(q) {
                continue;
            }
            loop {
                to -= 1;
                if to < if reserve { i64::from(below) } else { 0 } {
                    if reserve {
                        return None;
                    }
                    reserve = true; // then in the reserve
                    to = i64::from(ram_pages);
                    continue;
                }
                let t = to as u32;
                if !phys.page_used(t)
                    && hooks.movable(t << PAGE_SHIFT)
                    && !(p..p + pages).contains(&t)
                {
                    break;
                }
            }
            if !self.relocate(phys, mapper, hooks, q, to as u32) {
                return None;
            }
            moved += 1;
        }
        self.pin_still_mapped(phys, p << PAGE_SHIFT, pages << PAGE_SHIFT);
        phys.set_pages(p, pages, true);
        Some(Contig { pa: p << PAGE_SHIFT, moved })
    }

    /// Allocates contiguous physical pages when no window is free: the window
    /// in `[lo, hi)` at `align` needing the fewest moves (the highest of
    /// equals) has its used pages moved to the lowest free pages outside it.
    ///
    /// With `known_too`, pages pinned only because their address went out
    /// move as well: the console's kernel pins neither, and with them in the
    /// way the allocation fails, a crash for the title. It is the last
    /// resort, after file I/O in flight has completed: Project Gotham
    /// Racing's 22.6 MiB as a New York race loads.
    pub fn alloc_contig_compact(
        &mut self,
        phys: &mut PhysAllocator,
        mapper: &mut dyn Mapper,
        hooks: &mut dyn RelocateHooks,
        req: ContigRequest,
        known_too: bool,
    ) -> Option<Contig> {
        let ContigRequest { len, lo, hi, align } = req;
        if len > phys.ram_size() {
            return None;
        }
        let pages = len.max(1).div_ceil(PAGE_SIZE);
        let (step, first, last) = phys.window_bounds(lo, hi, align);
        if last < pages || first > last - pages {
            return None;
        }
        let free_all = phys.free_pages();
        let mut best: Option<(u32, u32)> = None; // start, used
        let mut s = i64::from((last - pages) / step * step);
        while s >= i64::from(first) {
            let start = s as u32;
            let mut used = 0;
            let ok = (start..start + pages).all(|p| {
                if !phys.page_used(p) {
                    return true;
                }
                used += 1;
                self.movable(phys, hooks, p, known_too)
            });
            if ok && free_all - (pages - used) >= used && best.is_none_or(|(_, u)| used < u) {
                best = Some((start, used));
            }
            s -= i64::from(step);
        }
        let (best, _) = best?;
        // Each used page goes to the lowest free page outside the window,
        // anywhere in RAM: the range bounds the new block, not the virtual
        // pages moved out of its way (Blinx's model data).
        let ram_pages = phys.pages();
        let mut to = 0;
        let mut moved = 0;
        for p in best..best + pages {
            if !phys.page_used(p) {
                continue;
            }
            while to < ram_pages
                && (phys.page_used(to)
                    || !hooks.movable(to << PAGE_SHIFT)
                    || (best..best + pages).contains(&to))
            {
                to += 1;
            }
            if to >= ram_pages || !self.relocate(phys, mapper, hooks, p, to) {
                return None;
            }
            moved += 1;
        }
        self.pin_still_mapped(phys, best << PAGE_SHIFT, pages << PAGE_SHIFT);
        phys.set_pages(best, pages, true);
        Some(Contig { pa: best << PAGE_SHIFT, moved })
    }

    /// Allocates the kernel's own blocks of more than a page (large pool
    /// blocks), which never move because they are reached through the
    /// contiguous window: placed as a contiguous allocation, below
    /// [`TOP_RESERVE`] if they can be.
    pub fn alloc_contig_high(
        &mut self,
        phys: &mut PhysAllocator,
        mapper: &mut dyn Mapper,
        hooks: &mut dyn RelocateHooks,
        len: u32,
    ) -> Option<u32> {
        let ram = phys.ram_size();
        let below = ram.saturating_sub(TOP_RESERVE);
        let mut got = None;
        if below != 0 {
            got = self.alloc_contig_top(
                phys,
                mapper,
                hooks,
                ContigRequest { hi: below, ..ContigRequest::anywhere(len) },
            );
        }
        got.or_else(|| self.alloc_contig_top(phys, mapper, hooks, ContigRequest::anywhere(len)))
            .map(|c| c.pa)
    }

    /// Allocates a kernel block that is freed again (a large pool block):
    /// the smallest free run that holds it ([`PhysAllocator::alloc`]), below
    /// [`TOP_RESERVE`] if it can be, else anywhere, else as
    /// [`AddressSpace::alloc_contig_high`] (virtual pages moved).
    ///
    /// Such a block is reached through the contiguous window and never
    /// moves, so it takes a hole, as page tables do, rather than the top of
    /// the highest free run, where the next contiguous block goes. As the
    /// reference does: Psi-Ops frees its 1.9 MiB text file as a new game
    /// starts, creates threads, and reads the file into a block of the same
    /// size while it keeps a pointer into the old one. On the console the
    /// block comes back where it was; with the small blocks made in between
    /// at the top of its hole it came lower and the title faulted.
    pub fn alloc_block(
        &mut self,
        phys: &mut PhysAllocator,
        mapper: &mut dyn Mapper,
        hooks: &mut dyn RelocateHooks,
        len: u32,
    ) -> Option<u32> {
        let ram = phys.ram_size();
        let below = if ram > TOP_RESERVE { ram - TOP_RESERVE } else { ram };
        phys.alloc(len, 0, below)
            .or_else(|| phys.alloc(len, 0, ram))
            .or_else(|| self.alloc_contig_high(phys, mapper, hooks, len))
    }
}

impl std::fmt::Debug for AddressSpace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AddressSpace").field("ram", &self.ram).finish_non_exhaustive()
    }
}

impl AddressSpace {
    /// Saves the mappings, or replaces them with the stream's: the page
    /// table and the reverse map. Loading, `mapper` (the CPU backend's view)
    /// is brought from the mappings there were to the ones loaded, page by
    /// page where they differ. A stream for another RAM size fails and
    /// changes nothing.
    pub fn state(&mut self, io: &mut xb_types::state::StateIo, mapper: &mut dyn Mapper) {
        use xb_types::state::Stateful;
        let mut pages = self.rmap.len() as u32;
        pages.state(io);
        if io.loading() && pages as usize != self.rmap.len() {
            io.fail(format!(
                "the state is of a machine with {} MiB of RAM, this one has {}",
                pages >> (20 - PAGE_SHIFT),
                self.rmap.len() >> (20 - PAGE_SHIFT)
            ));
            return;
        }
        if !io.loading() {
            for e in self.pte.iter_mut().chain(self.rmap.iter_mut()) {
                e.state(io);
            }
            return;
        }
        for (page, slot) in self.pte.iter_mut().enumerate() {
            let old = Pte(*slot);
            let mut new = 0u32;
            new.state(io);
            if io.failed().is_some() {
                return;
            }
            if new == old.0 {
                continue;
            }
            *slot = new;
            let (va, new) = ((page as u32) << PAGE_SHIFT, Pte(new));
            if old.is_present() {
                mapper.unmap(va, PAGE_SIZE);
            }
            if new.is_mmio() {
                mapper.map_mmio(va, new.pa(), PAGE_SIZE);
            } else if new.is_present() {
                mapper.map(va, new.pa(), PAGE_SIZE, new.access());
            }
        }
        for e in self.rmap.iter_mut() {
            e.state(io);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{Call, ModelMapper};

    const P: u32 = PAGE_SIZE;
    const RW: Access = Access::ReadWrite;

    fn req(len: u32, lo: u32, hi: u32, align: u32) -> ContigRequest {
        ContigRequest { len, lo, hi, align }
    }

    fn space(pages: u32) -> (AddressSpace, ModelMapper, PhysAllocator) {
        let ram = Ram::new(pages * P).unwrap();
        (AddressSpace::new(ram), ModelMapper::default(), PhysAllocator::new(pages * P))
    }

    #[test]
    fn a_mapping_translates_and_is_mirrored_to_the_mapper() {
        let (mut s, mut m, _) = space(16);
        s.map(&mut m, 0x1_0000, 3 * P, 2 * P, RW).unwrap();
        assert_eq!(s.translate(0x1_0000), Some(3 * P));
        assert_eq!(s.translate(0x1_1ABC), Some(4 * P + 0xABC));
        assert_eq!(s.translate(0x1_2000), None);
        assert_eq!(s.translate(0xFFFF), None);
        assert_eq!(m.calls, [Call::Map(0x1_0000, 3 * P, 2 * P, RW)]);
        m.assert_mirrors(&s);
    }

    #[test]
    fn bad_requests_are_refused_and_change_nothing() {
        let (mut s, mut m, _) = space(16);
        assert_eq!(s.map(&mut m, 0x1001, 0, P, RW), Err(MapError::Unaligned));
        assert_eq!(s.map(&mut m, 0x1000, 0x10, P, RW), Err(MapError::Unaligned));
        assert_eq!(s.map(&mut m, 0x1000, 0, P + 1, RW), Err(MapError::Unaligned));
        assert_eq!(s.map(&mut m, 0x1000, 15 * P, 2 * P, RW), Err(MapError::OutsideRam));
        assert_eq!(s.map(&mut m, 0xFFFF_F000, 0, 2 * P, RW), Err(MapError::OutOfRange));
        assert_eq!(s.unmap(&mut m, 0xFFFF_F000, 2 * P), Err(MapError::OutOfRange));
        assert_eq!(s.map_mmio(&mut m, 0x1000, 0xFFFF_F000, 2 * P), Err(MapError::OutOfRange));
        assert!(m.calls.is_empty());
        assert_eq!(s.mapped_pages(0, u32::MAX), 0);
        // The last page of the address space can be mapped.
        s.map(&mut m, 0xFFFF_F000, 0, P, RW).unwrap();
        assert_eq!(s.translate(0xFFFF_FFFF), Some(0xFFF));
    }

    #[test]
    fn unmapping_tells_the_mapper_once_per_mapped_run() {
        let (mut s, mut m, _) = space(16);
        s.map(&mut m, 0x10_0000, 0, 2 * P, RW).unwrap();
        s.map(&mut m, 0x10_5000, 4 * P, P, RW).unwrap();
        m.calls.clear();
        assert_eq!(s.unmap(&mut m, 0x10_0000, 16 * P), Ok(3));
        assert_eq!(m.calls, [Call::Unmap(0x10_0000, 2 * P), Call::Unmap(0x10_5000, P)]);
        assert_eq!(s.translate(0x10_0000), None);
        assert_eq!(s.va_of(0), None);
        m.assert_mirrors(&s);
        m.calls.clear();
        assert_eq!(s.unmap(&mut m, 0x10_0000, 16 * P), Ok(0));
        assert!(m.calls.is_empty());
    }

    #[test]
    fn device_pages_are_mirrored_but_never_translate_to_ram() {
        let (mut s, mut m, _) = space(16);
        s.map_mmio(&mut m, 0xFD00_0000, 0xFD00_0000, 4 * P).unwrap();
        assert_eq!(m.calls, [Call::MapMmio(0xFD00_0000, 0xFD00_0000, 4 * P)]);
        assert!(s.is_mapped(0xFD00_1000));
        assert!(s.entry(0xFD00_1000).is_mmio());
        assert_eq!(s.entry(0xFD00_1000).pa(), 0xFD00_1000);
        assert_eq!(s.translate(0xFD00_1000), None);
        assert_eq!(s.read_u32(0xFD00_1000), Err(MemFault { va: 0xFD00_1000, done: 0 }));
        m.assert_mirrors(&s);
    }

    #[test]
    fn protection_changes_only_ram_pages_and_reports_runs_to_the_mapper() {
        let (mut s, mut m, _) = space(16);
        s.map(&mut m, 0x2_0000, 2 * P, 2 * P, RW).unwrap();
        s.map(&mut m, 0x2_2000, 8 * P, P, RW).unwrap(); // not physically consecutive
        s.map_mmio(&mut m, 0x2_4000, 0xFEC0_0000, P).unwrap();
        m.calls.clear();
        assert_eq!(s.protect(&mut m, 0x2_0000, 8 * P, Access::Read), Ok(3));
        assert_eq!(
            m.calls,
            [
                Call::Map(0x2_0000, 2 * P, 2 * P, Access::Read),
                Call::Map(0x2_2000, 8 * P, P, Access::Read)
            ]
        );
        assert!(s.allows(0x2_0000, false) && !s.allows(0x2_0000, true));
        assert!(s.allows(0x2_4000, true));
        assert!(!s.allows(0x2_3000, false));
        m.assert_mirrors(&s);
        // Asking again changes nothing and calls nothing.
        m.calls.clear();
        assert_eq!(s.protect(&mut m, 0x2_0000, 8 * P, Access::Read), Ok(0));
        assert!(m.calls.is_empty());
        assert_eq!(s.protect(&mut m, 0x2_1000, P, Access::None), Ok(1));
        assert!(!s.allows(0x2_1000, false));
        // The host still reads a page the guest may not touch.
        assert_eq!(s.translate(0x2_1004), Some(3 * P + 4));
        assert_eq!(s.va_of(3 * P), Some(0x2_1000));
        m.assert_mirrors(&s);
    }

    #[test]
    fn the_reverse_map_names_the_last_address_and_ignores_windows() {
        // Windows are not recorded.
        let (mut s, mut m, _) = space(16);
        s.map_window(&mut m, 0x8000_0000, 0, 16 * P, RW).unwrap();
        assert_eq!(s.va_of(5 * P), None);
        assert!(s.entry(0x8000_5000).is_window());
        s.map(&mut m, 0x3_0000, 5 * P, P, RW).unwrap();
        assert_eq!(s.va_of(5 * P + 0x123), Some(0x3_0123));
        s.map(&mut m, 0x4_0000, 5 * P, P, RW).unwrap();
        assert_eq!(s.va_of(5 * P), Some(0x4_0000));
        // Unmapping the address the map does not name leaves it.
        s.unmap(&mut m, 0x3_0000, P).unwrap();
        assert_eq!(s.va_of(5 * P), Some(0x4_0000));
        // Mapping something else at the named address forgets the page.
        s.map(&mut m, 0x4_0000, 6 * P, P, RW).unwrap();
        assert_eq!(s.va_of(5 * P), None);
        assert_eq!(s.va_of(6 * P), Some(0x4_0000));
        assert_eq!(s.va_of(99 * P), None);
        // The window still translates.
        assert_eq!(s.translate(0x8000_5010), Some(5 * P + 0x10));
        m.assert_mirrors(&s);
    }

    #[test]
    fn reads_and_writes_cross_pages_that_are_not_physically_adjacent() {
        let (mut s, mut m, _) = space(16);
        s.map(&mut m, 0x5_0000, 9 * P, P, RW).unwrap();
        s.map(&mut m, 0x5_1000, 2 * P, P, RW).unwrap();
        s.map(&mut m, 0x5_2000, 12 * P, P, RW).unwrap();
        let data: Vec<u8> = (0..(2 * P + 100)).map(|i| (i * 7 + 3) as u8).collect();
        s.write(0x5_0000 + 0xF00, &data[..P as usize + 0x200]).unwrap();
        // The physical view holds the two halves on their own pages.
        assert_eq!(s.ram().read_vec(9 * P + 0xF00, 0x100).unwrap(), data[..0x100]);
        assert_eq!(s.ram().read_vec(2 * P, 0x80).unwrap(), data[0x100..0x180]);
        assert_eq!(s.ram().read_vec(12 * P, 0x100).unwrap(), data[0x1100..0x1200]);
        // And the virtual read gives back what was written.
        assert_eq!(s.read_bytes(0x5_0F00, P + 0x200).unwrap(), data[..P as usize + 0x200]);
    }

    #[test]
    fn an_access_stops_cleanly_at_an_unmapped_page() {
        let (mut s, mut m, _) = space(16);
        s.map(&mut m, 0x6_0000, 0, P, RW).unwrap();
        let mut buf = [0xEEu8; 32];
        assert_eq!(s.read(0x6_0FF0, &mut buf), Err(MemFault { va: 0x6_1000, done: 16 }));
        assert_eq!(buf[..16], [0u8; 16]);
        assert_eq!(buf[16..], [0xEEu8; 16]);
        assert_eq!(s.write(0x6_0FFC, &[1; 8]), Err(MemFault { va: 0x6_1000, done: 4 }));
        assert_eq!(s.ram().read_u32(0xFFC), Some(0x0101_0101));
        assert_eq!(s.fill(0x6_0FFE, 9, 4), Err(MemFault { va: 0x6_1000, done: 2 }));
        assert_eq!(s.read(0x7_0000, &mut buf), Err(MemFault { va: 0x7_0000, done: 0 }));
        assert_eq!(s.read(0x7_0000, &mut []), Ok(()));
    }

    #[test]
    fn typed_access_round_trips_within_and_across_pages() {
        let (mut s, mut m, _) = space(16);
        s.map(&mut m, 0x7_0000, 4 * P, P, RW).unwrap();
        s.map(&mut m, 0x7_1000, P, P, RW).unwrap();
        for va in [0x7_0000u32, 0x7_0FFC, 0x7_0FFD, 0x7_0FFF, 0x7_1FF8] {
            s.write_u64(va, 0x0123_4567_89AB_CDEF).unwrap();
            assert_eq!(s.read_u64(va), Ok(0x0123_4567_89AB_CDEF));
            assert_eq!(s.read_u32(va), Ok(0x89AB_CDEF));
            assert_eq!(s.read_u16(va.wrapping_add(6)), Ok(0x0123));
            assert_eq!(s.read_u8(va.wrapping_add(7)), Ok(0x01));
            s.write_u32(va, 0xAABB_CCDD).unwrap();
            s.write_u16(va, 0x1122).unwrap();
            s.write_u8(va, 0x33).unwrap();
            assert_eq!(s.read_u32(va), Ok(0xAABB_1133));
        }
        // A value straddling the end of the mapping faults at the next page.
        assert_eq!(s.read_u32(0x7_1FFE), Err(MemFault { va: 0x7_2000, done: 2 }));
        assert_eq!(s.write_u16(0x7_1FFF, 1), Err(MemFault { va: 0x7_2000, done: 1 }));
    }

    #[test]
    fn an_access_wraps_at_the_top_of_the_address_space() {
        let (mut s, mut m, _) = space(16);
        s.map(&mut m, 0xFFFF_F000, 0, P, RW).unwrap();
        s.map(&mut m, 0, P, P, RW).unwrap();
        s.write_u32(0xFFFF_FFFE, 0x4433_2211).unwrap();
        assert_eq!(s.ram().read_u16(0xFFE), Some(0x2211));
        assert_eq!(s.ram().read_u16(P), Some(0x4433));
    }

    #[test]
    fn strings_are_read_across_pages_and_stop_at_the_terminator() {
        let (mut s, mut m, _) = space(16);
        s.map(&mut m, 0x8_0000, 7 * P, P, RW).unwrap();
        s.map(&mut m, 0x8_1000, 3 * P, P, RW).unwrap();
        let name = b"\\Device\\CdRom0\\default.xbe";
        s.write_cstr(0x8_0FF0, name).unwrap();
        assert_eq!(s.read_cstr(0x8_0FF0, 260).unwrap(), name);
        assert_eq!(s.read_cstr(0x8_0FF0, 5).unwrap(), &name[..5]);
        assert_eq!(s.read_cstr(0x8_0FF0, 0).unwrap(), b"");
        // A string that ends on a mapped page does not need the next one.
        s.write_cstr(0x8_1FF0, b"tail").unwrap();
        assert_eq!(s.read_cstr(0x8_1FF0, 4096).unwrap(), b"tail");
        // One that runs into an unmapped page reports where.
        s.fill(0x8_1FF0, b'x', 16).unwrap();
        assert_eq!(s.read_cstr(0x8_1FF0, 4096), Err(MemFault { va: 0x8_2000, done: 16 }));
    }

    #[test]
    fn wide_and_counted_strings_are_read() {
        let (mut s, mut m, _) = space(16);
        s.map(&mut m, 0x9_0000, 7 * P, P, RW).unwrap();
        s.map(&mut m, 0x9_1000, 3 * P, P, RW).unwrap();
        let wide: Vec<u16> = "D:\\maps\\a10.map".encode_utf16().collect();
        let mut bytes: Vec<u8> = wide.iter().flat_map(|u| u.to_le_bytes()).collect();
        bytes.extend([0, 0]);
        // Unaligned, so a code unit straddles the page boundary.
        s.write(0x9_0FF1, &bytes).unwrap();
        assert_eq!(s.read_wstr(0x9_0FF1, 260).unwrap(), wide);
        assert_eq!(s.read_wstr(0x9_0FF1, 3).unwrap(), wide[..3]);

        // ANSI_STRING { Length, MaximumLength, Buffer } near the end of a page.
        s.write(0x9_1800, b"\\??\\D:\\saves").unwrap();
        s.write_u16(0x9_0FFA, 12).unwrap();
        s.write_u16(0x9_0FFC, 64).unwrap();
        s.write_u32(0x9_0FFE, 0x9_1800).unwrap();
        assert_eq!(s.read_ansi_string(0x9_0FFA).unwrap(), b"\\??\\D:\\saves");

        // UNICODE_STRING counts bytes.
        s.write(0x9_1900, &bytes).unwrap();
        s.write_u16(0x9_0100, (wide.len() * 2) as u16).unwrap();
        s.write_u32(0x9_0104, 0x9_1900).unwrap();
        assert_eq!(s.read_unicode_string(0x9_0100).unwrap(), wide);
    }

    #[test]
    fn a_guest_copy_handles_overlap() {
        let (mut s, mut m, _) = space(16);
        s.map(&mut m, 0xA_0000, 0, 2 * P, RW).unwrap();
        let data: Vec<u8> = (0..200u8).collect();
        s.write(0xA_0F80, &data).unwrap();
        s.copy(0xA_0F90, 0xA_0F80, 200).unwrap();
        assert_eq!(s.read_bytes(0xA_0F90, 200).unwrap(), data);
        assert_eq!(s.copy(0xA_0000, 0xA_1FF0, 32), Err(MemFault { va: 0xA_2000, done: 16 }));
    }

    #[test]
    fn physical_runs_split_where_the_pages_stop_being_consecutive() {
        let (mut s, mut m, _) = space(32);
        s.map(&mut m, 0xB_0000, 4 * P, 2 * P, RW).unwrap();
        s.map(&mut m, 0xB_2000, 20 * P, P, RW).unwrap();
        let runs: Vec<_> = s.phys_runs(0xB_0800, 3 * P).collect();
        assert_eq!(runs, [(4 * P + 0x800, 2 * P - 0x800), (20 * P, P)]);
        assert_eq!(s.phys_runs(0xB_0800, 0x10).collect::<Vec<_>>(), [(4 * P + 0x800, 0x10)]);
        // The lengths sum to the request when all of it is mapped.
        let n: u32 = s.phys_runs(0xB_0001, 2 * P + 5).map(|r| r.1).sum();
        assert_eq!(n, 2 * P + 5);
        assert_eq!(s.phys_runs(0xC_0000, P).count(), 0);
        assert_eq!(s.phys_runs(0xB_0000, 0).count(), 0);
    }

    #[test]
    fn mapped_runs_describe_committed_and_uncommitted_stretches() {
        let (mut s, mut m, _) = space(32);
        s.map(&mut m, 0xD_2000, 0, 3 * P, RW).unwrap();
        assert_eq!(s.mapped_run(0xD_0000, 16 * P), (false, 2 * P));
        assert_eq!(s.mapped_run(0xD_2000, 16 * P), (true, 3 * P));
        assert_eq!(s.mapped_run(0xD_2000, 2 * P), (true, 2 * P));
        assert_eq!(s.mapped_run(0xD_5000, 4 * P), (false, 4 * P));
        assert_eq!(s.mapped_pages(0xD_0000, 16 * P), 3);
        assert_eq!(s.mapped_pages(0xD_3FFF, 2), 2);
        assert_eq!(s.mapped_run(0xFFFF_F000, P), (false, P));
    }

    #[test]
    fn a_free_virtual_range_is_found_from_either_end() {
        let (mut s, mut m, _) = space(32);
        // The kernel's bounds and grain.
        const LO: u32 = 0x0001_0000;
        const HI: u32 = 0x7FFE_0000;
        const GRAIN: u32 = 0x1_0000;
        s.map(&mut m, LO, 0, 3 * P, RW).unwrap();
        s.map(&mut m, HI - P, 8 * P, P, RW).unwrap();
        assert_eq!(s.find_free(0x2_0000, LO, HI, GRAIN, false), Some(0x2_0000));
        assert_eq!(s.find_free(0x2_0000, LO, HI, GRAIN, true), Some(HI - 0x3_0000));
        // Page grain uses the gap right after the first mapping.
        assert_eq!(s.find_free(P, LO, HI, P, false), Some(LO + 3 * P));
        // A range too large for the bounds is not found.
        assert_eq!(s.find_free(0x8000_0000, LO, HI, GRAIN, false), None);
        assert_eq!(s.find_free(P, LO, LO + 3 * P, P, false), None);
        // hi == 0 reaches the end of the address space.
        assert_eq!(s.find_free(P, 0xD000_0000, 0, P, true), Some(0xFFFF_F000));
        let got = s.find_free(5 * P, LO, HI, GRAIN, false).unwrap();
        assert_eq!(s.mapped_pages(got, 5 * P), 0);
    }

    #[test]
    fn allocating_at_an_address_maps_zeroed_pages_from_the_top_of_ram() {
        // 512 pages: the reserve is the top 256.
        let (mut s, mut m, mut phys) = space(512);
        phys.claim(0, 16 * P);
        s.ram().fill(250 * P, 0xCC, 6 * P as usize);
        s.alloc_at(&mut phys, &mut m, 0x1_0000, 3 * P + 1, RW).unwrap();
        assert_eq!(s.translate(0x1_0000), Some(252 * P));
        assert_eq!(s.translate(0x1_3000), Some(255 * P));
        assert_eq!(s.read_bytes(0x1_0000, 4 * P).unwrap(), vec![0u8; 4 * P as usize]);
        assert_eq!(phys.free_pages(), 512 - 16 - 4);
        assert!(phys.is_used(252 * P) && !phys.is_used(251 * P));
        assert_eq!(s.va_of(253 * P), Some(0x1_1000));
        m.assert_mirrors(&s);

        assert_eq!(s.free_at(&mut phys, &mut m, 0x1_0000, 4 * P), 4);
        assert_eq!(phys.free_pages(), 512 - 16);
        assert_eq!(s.mapped_pages(0x1_0000, 4 * P), 0);
        m.assert_mirrors(&s);
    }

    #[test]
    fn an_allocation_that_runs_out_of_ram_gives_everything_back() {
        let (mut s, mut m, mut phys) = space(32);
        phys.claim(0, 8 * P);
        phys.claim(12 * P, 4 * P); // fragments: the first piece succeeds
        let free = phys.free_pages();
        assert_eq!(s.alloc_at(&mut phys, &mut m, 0x1_0000, 25 * P, RW), Err(MapError::OutOfMemory));
        assert_eq!(phys.free_pages(), free);
        assert_eq!(s.mapped_pages(0x1_0000, 32 * P), 0);
        m.assert_mirrors(&s);
        assert_eq!(
            s.alloc_at(&mut phys, &mut m, 0x1_0000, u32::MAX, RW),
            Err(MapError::OutOfMemory)
        );
        assert_eq!(s.alloc_at(&mut phys, &mut m, 0x1_0800, P, RW), Err(MapError::Unaligned));
        assert_eq!(phys.free_pages(), free);
    }

    #[test]
    fn a_contiguous_allocation_at_an_address_is_one_physical_run() {
        let (mut s, mut m, mut phys) = space(64);
        phys.claim(10 * P, P);
        let pa = s.alloc_at_contiguous(&mut phys, &mut m, 0xD000_0000, 6 * P, RW).unwrap();
        assert_eq!(s.phys_runs(0xD000_0000, 6 * P).collect::<Vec<_>>(), [(pa, 6 * P)]);
        // Best fit: the 10-page run below the claimed page holds it.
        assert_eq!(pa, 4 * P);
        m.assert_mirrors(&s);
    }

    #[test]
    fn freeing_at_an_address_leaves_window_pages_allocated() {
        let (mut s, mut m, mut phys) = space(16);
        phys.claim(0, 4 * P);
        s.map_window(&mut m, 0x8000_0000, 0, 4 * P, RW).unwrap();
        assert_eq!(s.free_at(&mut phys, &mut m, 0x8000_0000, 4 * P), 0);
        assert_eq!(phys.free_pages(), 12);
        assert_eq!(s.translate(0x8000_0000), None);
    }

    /// Hooks that record moves and can refuse pages.
    #[derive(Default)]
    struct Hooks {
        closed: Vec<u32>,
        log: Vec<(&'static str, u32, u32)>,
    }

    impl RelocateHooks for Hooks {
        fn movable(&self, pa: u32) -> bool {
            !self.closed.contains(&pa)
        }
        fn before_move(&mut self, from: u32, to: u32) {
            self.log.push(("before", from, to));
        }
        fn after_move(&mut self, from: u32, to: u32) {
            self.log.push(("after", from, to));
        }
    }

    /// 512 pages of RAM with virtual pages mapped at `0x10000 + i` onto the
    /// given physical pages, each filled with a byte naming its address.
    fn with_virtual(pages: &[u32]) -> (AddressSpace, ModelMapper, PhysAllocator) {
        let (mut s, mut m, mut phys) = space(512);
        for (i, &p) in pages.iter().enumerate() {
            assert!(phys.claim(p * P, P));
            let va = 0x1_0000 + i as u32 * P;
            s.map(&mut m, va, p * P, P, RW).unwrap();
            s.fill(va, 0x40 + i as u8, P).unwrap();
        }
        m.calls.clear();
        (s, m, phys)
    }

    #[test]
    fn the_top_allocator_takes_the_highest_free_window_without_moving_anything() {
        let (mut s, mut m, mut phys) = with_virtual(&[]);
        let c = s.alloc_contig_top(&mut phys, &mut m, &mut (), req(4 * P, 0, 0, P)).unwrap();
        assert_eq!(c, Contig { pa: 508 * P, moved: 0 });
        let c = s.alloc_contig_top(&mut phys, &mut m, &mut (), req(4 * P, 0, 0, 8 * P)).unwrap();
        assert_eq!(c.pa, 504 * P);
        let c = s.alloc_contig_top(&mut phys, &mut m, &mut (), req(P, 0, 100 * P, P)).unwrap();
        assert_eq!(c.pa, 99 * P);
        assert!(m.calls.is_empty());
    }

    #[test]
    fn the_top_allocator_moves_virtual_pages_out_of_its_window() {
        let (mut s, mut m, mut phys) = with_virtual(&[510, 509]);
        let mut hooks = Hooks::default();
        let free = phys.free_pages();
        let c = s.alloc_contig_top(&mut phys, &mut m, &mut hooks, req(4 * P, 0, 0, P)).unwrap();
        assert_eq!(c, Contig { pa: 508 * P, moved: 2 });
        // Moved to the highest free pages below the top reserve (page 256),
        // in window order: 509 first.
        assert_eq!(s.translate(0x1_1000), Some(255 * P));
        assert_eq!(s.translate(0x1_0000), Some(254 * P));
        // The contents went with them and the mapper heard about it.
        assert_eq!(s.read_u8(0x1_0000), Ok(0x40));
        assert_eq!(s.read_u8(0x1_1FFF), Ok(0x41));
        assert_eq!(
            hooks.log,
            [
                ("before", 509 * P, 255 * P),
                ("after", 509 * P, 255 * P),
                ("before", 510 * P, 254 * P),
                ("after", 510 * P, 254 * P)
            ]
        );
        assert_eq!(phys.free_pages(), free - 4);
        assert!((508..512).all(|p| phys.is_used(p * P)));
        assert!(phys.is_used(255 * P) && phys.is_used(254 * P));
        assert_eq!(s.va_of(509 * P), None);
        assert_eq!(s.va_of(255 * P), Some(0x1_1000));
        m.assert_mirrors(&s);
    }

    #[test]
    fn a_moved_page_keeps_its_protection() {
        let (mut s, mut m, mut phys) = with_virtual(&[511]);
        s.protect(&mut m, 0x1_0000, P, Access::Read).unwrap();
        s.alloc_contig_top(&mut phys, &mut m, &mut (), req(P, 0, 0, P)).unwrap();
        assert_eq!(s.entry(0x1_0000).access(), Access::Read);
        assert_eq!(m.calls.last(), Some(&Call::Map(0x1_0000, 255 * P, P, Access::Read)));
        m.assert_mirrors(&s);
    }

    #[test]
    fn pinned_unmapped_and_closed_pages_do_not_move() {
        // Pinned, reached only through a window, or without guest access.
        let (mut s, mut m, mut phys) = with_virtual(&[511, 505, 499]);
        phys.pin(511 * P, true, 0);
        phys.claim(507 * P, P); // used, no virtual address (a kernel block)
        let mut hooks = Hooks { closed: vec![499 * P], ..Hooks::default() };
        // The highest 2-page window clear of 511 and 507 is [508, 510)... which
        // is free; then [505, 507) needs page 505 moved.
        let a = s.alloc_contig_top(&mut phys, &mut m, &mut hooks, req(2 * P, 0, 0, P)).unwrap();
        assert_eq!(a, Contig { pa: 509 * P, moved: 0 });
        let b = s.alloc_contig_top(&mut phys, &mut m, &mut hooks, req(3 * P, 0, 0, P)).unwrap();
        assert_eq!(b, Contig { pa: 504 * P, moved: 1 });
        // 499 has no guest access: the window stays clear of it.
        let c = s.alloc_contig_top(&mut phys, &mut m, &mut hooks, req(4 * P, 0, 0, P)).unwrap();
        assert_eq!(c, Contig { pa: 500 * P, moved: 0 });
        let d = s.alloc_contig_top(&mut phys, &mut m, &mut hooks, req(2 * P, 0, 0, P)).unwrap();
        assert_eq!(d, Contig { pa: 497 * P, moved: 0 });
        assert_eq!(s.translate(0x1_0000), Some(511 * P));
        assert_eq!(s.translate(0x1_2000), Some(499 * P));
        m.assert_mirrors(&s);
    }

    #[test]
    fn a_closed_free_page_does_not_receive_a_moved_page() {
        let (mut s, mut m, mut phys) = with_virtual(&[511]);
        let mut hooks = Hooks { closed: vec![255 * P], ..Hooks::default() };
        s.alloc_contig_top(&mut phys, &mut m, &mut hooks, req(P, 0, 0, P)).unwrap();
        assert_eq!(s.translate(0x1_0000), Some(254 * P));
    }

    #[test]
    fn moved_pages_go_into_the_top_reserve_only_when_nothing_else_is_free() {
        let (mut s, mut m, mut phys) = with_virtual(&[100]);
        // Everything below the reserve is used except the page to move.
        for p in 0..256 {
            phys.claim(p * P, P);
        }
        let c = s.alloc_contig_top(&mut phys, &mut m, &mut (), req(P, 0, 101 * P, P)).unwrap();
        assert_eq!(c, Contig { pa: 100 * P, moved: 1 });
        assert_eq!(s.translate(0x1_0000), Some(511 * P));
    }

    #[test]
    fn the_top_allocator_fails_when_there_is_nowhere_to_move_pages() {
        let (mut s, mut m, mut phys) = with_virtual(&[3]);
        for p in 0..512 {
            phys.claim(p * P, P);
        }
        assert_eq!(s.alloc_contig_top(&mut phys, &mut m, &mut (), req(P, 0, 4 * P, P)), None);
        assert_eq!(s.alloc_contig_top(&mut phys, &mut m, &mut (), req(u32::MAX, 0, 0, P)), None);
        assert_eq!(s.translate(0x1_0000), Some(3 * P));
    }

    #[test]
    fn compaction_picks_the_window_with_the_fewest_moves() {
        // Used kernel pages at every 4th page leave 3-page gaps; windows of 3
        // between them hold 0, 1 or 2 virtual pages.
        let (mut s, mut m, mut phys) = with_virtual(&[9, 10, 14, 21, 22, 23]);
        for p in (0..512).step_by(4) {
            phys.claim(p * P, P);
        }
        for p in 24..512 {
            phys.claim(p * P, P);
        }
        // Free 3-page windows exist ([1,4), [5,8), [17,20)): no moves needed,
        // the highest wins.
        let c =
            s.alloc_contig_compact(&mut phys, &mut m, &mut (), req(3 * P, 0, 0, P), false).unwrap();
        assert_eq!(c, Contig { pa: 17 * P, moved: 0 });
        phys.claim(P, 3 * P);
        phys.claim(5 * P, 2 * P); // leaves page 7 free
        // Now [13, 16) needs one move (page 14), the others two or three.
        let c =
            s.alloc_contig_compact(&mut phys, &mut m, &mut (), req(3 * P, 0, 0, P), false).unwrap();
        assert_eq!(c, Contig { pa: 13 * P, moved: 1 });
        // The page went to the lowest free page outside the window.
        assert_eq!(s.translate(0x1_2000), Some(7 * P));
        assert_eq!(s.read_u8(0x1_2000), Ok(0x42));
        m.assert_mirrors(&s);
    }

    #[test]
    fn compaction_moves_known_pages_only_as_a_last_resort() {
        let (mut s, mut m, mut phys) = with_virtual(&[2, 6]);
        for p in 8..512 {
            phys.claim(p * P, P);
        }
        phys.pin(2 * P, true, 0); // MmGetPhysicalAddress
        phys.pin(6 * P, true, 1); // and locked for I/O
        let r = req(4 * P, 0, 8 * P, 4 * P);
        assert_eq!(s.alloc_contig_compact(&mut phys, &mut m, &mut (), r, false), None);
        let c = s.alloc_contig_compact(&mut phys, &mut m, &mut (), r, true).unwrap();
        // The locked page's window is never taken; the known page moved and
        // stays known where it went.
        assert_eq!(c, Contig { pa: 0, moved: 1 });
        assert_eq!(s.translate(0x1_0000), Some(4 * P));
        assert!(phys.pin_state(4 * P).known);
        assert!(!phys.pin_state(2 * P).is_pinned());
        assert_eq!(s.translate(0x1_1000), Some(6 * P));
    }

    #[test]
    fn a_block_handed_out_while_an_address_still_maps_it_pins_those_pages() {
        // The page was freed without being unmapped.
        let (mut s, mut m, mut phys) = with_virtual(&[511]);
        phys.free(511 * P, P);
        let c = s.alloc_contig_top(&mut phys, &mut m, &mut (), req(2 * P, 0, 0, P)).unwrap();
        assert_eq!(c.pa, 510 * P);
        assert!(phys.pin_state(511 * P).known);
        assert!(!phys.pin_state(510 * P).known);
    }

    #[test]
    fn kernel_blocks_stay_below_the_top_reserve_while_they_fit() {
        let (mut s, mut m, mut phys) = with_virtual(&[]);
        assert_eq!(s.alloc_contig_high(&mut phys, &mut m, &mut (), 2 * P), Some(254 * P));
        for p in 0..254 {
            phys.claim(p * P, P);
        }
        assert_eq!(s.alloc_contig_high(&mut phys, &mut m, &mut (), 2 * P), Some(510 * P));
    }

    #[test]
    fn a_freed_block_comes_back_where_it_was_when_smaller_ones_have_a_hole_to_fill() {
        let (mut s, mut m, mut phys) = with_virtual(&[]);
        let mut block = |phys: &mut PhysAllocator, pages: u32| {
            s.alloc_block(phys, &mut m, &mut (), pages * P).unwrap()
        };
        // A hole of four pages between two blocks that stay.
        let hole = block(&mut phys, 4);
        let _stays = block(&mut phys, 1);
        phys.free(hole, 4 * P);
        let big = block(&mut phys, 100);
        assert!(big + 100 * P <= 256 * P, "below the top reserve");
        phys.free(big, 100 * P);
        // Smaller blocks made meanwhile take the hole, not the big one's place.
        let small = [block(&mut phys, 3), block(&mut phys, 1)];
        assert!(small.iter().all(|&pa| (hole..hole + 4 * P).contains(&pa)), "{small:x?}");
        assert_eq!(block(&mut phys, 100), big);
        // With nothing free below the reserve a block goes into it, and with
        // no free run long enough virtual pages make room.
        for p in 0..256 {
            phys.claim(p * P, P);
        }
        assert!(block(&mut phys, 2) >= 256 * P);
    }

    #[test]
    fn random_mapping_sequences_keep_the_mapper_s_view_equal_to_the_table() {
        let (mut s, mut m, mut phys) = space(256);
        let mut seed = 0x9E37_79B9u32;
        let mut rnd = move |n: u32| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed % n
        };
        let accesses = [Access::None, Access::Read, RW];
        for _ in 0..600 {
            let va = 0x10_0000 + rnd(64) * P;
            let len = (1 + rnd(8)) * P;
            match rnd(7) {
                0 => {
                    let _ = s.map(&mut m, va, rnd(200) * P, len, accesses[rnd(3) as usize]);
                }
                1 => {
                    let _ = s.unmap(&mut m, va, len);
                }
                2 => {
                    let _ = s.protect(&mut m, va, len, accesses[rnd(3) as usize]);
                }
                3 => {
                    let _ = s.map_mmio(&mut m, va, 0xFD00_0000 + rnd(16) * P, len);
                }
                4 => {
                    let _ = s.alloc_at(&mut phys, &mut m, va, len, RW);
                }
                5 => {
                    s.free_at(&mut phys, &mut m, va, len);
                }
                _ => {
                    let _ = s.alloc_contig_top(&mut phys, &mut m, &mut (), req(len, 0, 0, P));
                }
            }
        }
        m.assert_mirrors(&s);
        // Every reverse-map answer is a real mapping.
        for p in 0..256 {
            if let Some(va) = s.va_of(p * P) {
                assert_eq!(s.translate(va), Some(p * P));
            }
        }
    }
}
