//! The kernel's access to guest memory.
//!
//! No host mapping of the guest's 4 GiB exists: every access walks the page
//! table in `xb-mem` into the RAM view. This module is the one place that does
//! it, so that two things are never forgotten:
//!
//! * a store made on the guest's behalf is reported to the write tracker
//!   (`WriteTracker::host_store`) and a load of a page without guest access
//!   asks for its contents first (`host_load`);
//! * an access to a page the guest does not have is a [`Fault`], which a
//!   kernel call passes up with `?` and the dispatcher turns into the title's
//!   access violation.
//!
//! Mapping changes are queued ([`MapQueue`]) and applied to the CPU backend
//! before guest code next runs, so kernel code needs no `Cpu` in hand.

use std::sync::Arc;

use xb_cpu::Cpu;
use xb_mem::{
    Access, AddressSpace, Contig, ContigRequest, Mapper, PhysAllocator, Ram, RelocateHooks,
    WriteTracker,
};
use xb_types::{CONTIG_WINDOW, PAGE_SIZE};

/// A kernel access to guest memory met a page that is not RAM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Fault {
    /// The first address that could not be accessed.
    pub va: u32,
    /// Whether the kernel was storing.
    pub write: bool,
}

/// The result of kernel code that touches guest memory.
pub(crate) type KResult<T> = Result<T, Fault>;

enum MapOp {
    Map(u32, u32, u32, Access),
    Mmio(u32, u32, u32),
    Unmap(u32, u32),
}

/// Mapping changes waiting for the CPU backend, in order.
#[derive(Default)]
pub(crate) struct MapQueue {
    ops: Vec<MapOp>,
    invalidate: Vec<(u32, u32)>,
}

impl Mapper for MapQueue {
    fn map(&mut self, va: u32, pa: u32, len: u32, access: Access) {
        self.ops.push(MapOp::Map(va, pa, len, access));
    }

    fn map_mmio(&mut self, va: u32, pa: u32, len: u32) {
        self.ops.push(MapOp::Mmio(va, pa, len));
    }

    fn unmap(&mut self, va: u32, len: u32) {
        self.ops.push(MapOp::Unmap(va, len));
    }
}

/// Guest memory as the kernel uses it: the page table, the physical
/// allocator, the write tracker and the queue of mapping changes.
pub(crate) struct GuestMem {
    pub space: AddressSpace,
    pub phys: PhysAllocator,
    pub maps: MapQueue,
    pub hooks: Box<dyn RelocateHooks + Send>,
    tracker: Option<Arc<WriteTracker>>,
    ram: Ram,
    /// RAM pages freed since the machine was last told.
    freed: Vec<(u32, u32)>,
}

impl GuestMem {
    pub(crate) fn new(
        ram: Ram,
        tracker: Option<Arc<WriteTracker>>,
        hooks: Box<dyn RelocateHooks + Send>,
    ) -> Self {
        Self {
            space: AddressSpace::new(ram.clone()),
            phys: PhysAllocator::new(ram.size()),
            maps: MapQueue::default(),
            hooks,
            tracker,
            ram,
            freed: Vec::new(),
        }
    }

    /// Physical RAM.
    pub(crate) fn ram(&self) -> &Ram {
        &self.ram
    }

    /// Size of RAM in bytes.
    pub(crate) fn ram_size(&self) -> u32 {
        self.ram.size()
    }

    /// Applies the queued mapping changes and code invalidations to the CPU.
    pub(crate) fn flush(&mut self, cpu: &mut dyn Cpu) {
        for op in self.maps.ops.drain(..) {
            match op {
                MapOp::Map(va, pa, len, access) => cpu.map(va, pa, len, access),
                MapOp::Mmio(va, pa, len) => cpu.map_mmio(va, pa, len),
                MapOp::Unmap(va, len) => cpu.unmap(va, len),
            }
        }
        for (pa, len) in self.maps.invalidate.drain(..) {
            cpu.invalidate_code(pa, len);
        }
    }

    /// Whether mapping changes wait for the CPU.
    /// The mappings, the page allocator and the pages freed since the
    /// machine was last told, in a savestate. Loading, the changes to the
    /// CPU's view queue up as any mapping change does.
    pub(crate) fn state(&mut self, io: &mut xb_types::state::StateIo) {
        use xb_types::state::Stateful;
        self.space.state(io, &mut self.maps);
        self.phys.state(io);
        self.freed.state(io);
    }

    pub(crate) fn dirty_maps(&self) -> bool {
        !self.maps.ops.is_empty() || !self.maps.invalidate.is_empty()
    }

    /// Guest code in `[va, va + len)` was changed by the kernel (a loader
    /// patch) or its pages are going away: translations of it must go.
    pub(crate) fn invalidate_code(&mut self, va: u32, len: u32) {
        let runs: Vec<(u32, u32)> = self.space.phys_runs(va, len).collect();
        self.maps.invalidate.extend(runs);
    }

    // ---------------------------------------------------------- tracking

    #[inline]
    fn note_store(&self, pa: u32, len: u32) {
        if let Some(t) = &self.tracker {
            let last = pa.wrapping_add(len.saturating_sub(1));
            if !t.open(pa) || !t.open(last) || len > 2 * PAGE_SIZE {
                t.host_store(pa, len);
            }
        }
    }

    #[inline]
    fn note_load(&self, pa: u32, len: u32) {
        if let Some(t) = &self.tracker {
            let last = pa.wrapping_add(len.saturating_sub(1));
            if t.has_noaccess(pa) || t.has_noaccess(last) || len > 2 * PAGE_SIZE {
                t.host_load(pa, len);
            }
        }
    }

    // ------------------------------------------------------------ queries

    /// The physical address behind `va`, if RAM is mapped there.
    #[inline]
    pub(crate) fn phys_of(&self, va: u32) -> Option<u32> {
        self.space.translate(va)
    }

    /// Whether RAM is mapped at `va`.
    #[inline]
    pub(crate) fn mapped(&self, va: u32) -> bool {
        self.space.translate(va).is_some()
    }

    /// Whether RAM is mapped over all of `[va, va + len)`.
    pub(crate) fn mapped_range(&self, va: u32, len: u32) -> bool {
        if len == 0 {
            return true;
        }
        let first = va / PAGE_SIZE;
        let last = (u64::from(va) + u64::from(len) - 1) / u64::from(PAGE_SIZE);
        (u64::from(first)..=last).all(|p| self.mapped((p as u32).wrapping_mul(PAGE_SIZE)))
    }

    // ------------------------------------------------------- byte access

    /// Copies guest memory at `va` into `buf`.
    pub(crate) fn read(&self, va: u32, buf: &mut [u8]) -> KResult<()> {
        let mut done = 0usize;
        for (pa, n) in self.space.phys_runs(va, buf.len() as u32) {
            self.note_load(pa, n);
            self.ram.read(pa, &mut buf[done..done + n as usize]);
            done += n as usize;
        }
        if done < buf.len() {
            return Err(Fault { va: va.wrapping_add(done as u32), write: false });
        }
        Ok(())
    }

    /// Copies `data` to guest memory at `va`. The bytes before a missing page
    /// are written, as the reference's store up to its fault.
    pub(crate) fn write(&self, va: u32, data: &[u8]) -> KResult<()> {
        let mut done = 0usize;
        for (pa, n) in self.space.phys_runs(va, data.len() as u32) {
            self.note_store(pa, n);
            self.ram.write(pa, &data[done..done + n as usize]);
            done += n as usize;
        }
        if done < data.len() {
            return Err(Fault { va: va.wrapping_add(done as u32), write: true });
        }
        Ok(())
    }

    /// Sets `len` bytes at `va` to `byte`.
    pub(crate) fn fill(&self, va: u32, byte: u8, len: u32) -> KResult<()> {
        let mut done = 0u32;
        for (pa, n) in self.space.phys_runs(va, len) {
            self.note_store(pa, n);
            self.ram.fill(pa, byte, n as usize);
            done += n;
        }
        if done < len {
            return Err(Fault { va: va.wrapping_add(done), write: true });
        }
        Ok(())
    }

    /// `memmove` between guest addresses.
    pub(crate) fn copy(&self, dst: u32, src: u32, len: u32) -> KResult<()> {
        let buf = self.read_vec(src, len)?;
        self.write(dst, &buf)
    }

    /// Reads `len` bytes at `va`.
    pub(crate) fn read_vec(&self, va: u32, len: u32) -> KResult<Vec<u8>> {
        let mut v = vec![0u8; len as usize];
        self.read(va, &mut v)?;
        Ok(v)
    }

    #[inline]
    fn rd<const N: usize>(&self, va: u32) -> KResult<[u8; N]> {
        let mut b = [0u8; N];
        if (va & (PAGE_SIZE - 1)) as usize + N <= PAGE_SIZE as usize {
            let pa = self.space.translate(va).ok_or(Fault { va, write: false })?;
            self.note_load(pa, N as u32);
            self.ram.read(pa, &mut b);
        } else {
            self.read(va, &mut b)?;
        }
        Ok(b)
    }

    #[inline]
    fn wr<const N: usize>(&self, va: u32, b: [u8; N]) -> KResult<()> {
        if (va & (PAGE_SIZE - 1)) as usize + N <= PAGE_SIZE as usize {
            let pa = self.space.translate(va).ok_or(Fault { va, write: true })?;
            self.note_store(pa, N as u32);
            self.ram.write(pa, &b);
            Ok(())
        } else {
            self.write(va, &b)
        }
    }

    /// Reads a byte.
    #[inline]
    pub(crate) fn r8(&self, va: u32) -> KResult<u8> {
        Ok(self.rd::<1>(va)?[0])
    }

    /// Reads a little-endian `u16`.
    #[inline]
    pub(crate) fn r16(&self, va: u32) -> KResult<u16> {
        Ok(u16::from_le_bytes(self.rd(va)?))
    }

    /// Reads a little-endian `u32`.
    #[inline]
    pub(crate) fn r32(&self, va: u32) -> KResult<u32> {
        Ok(u32::from_le_bytes(self.rd(va)?))
    }

    /// Reads a little-endian `u64`.
    #[inline]
    pub(crate) fn r64(&self, va: u32) -> KResult<u64> {
        Ok(u64::from_le_bytes(self.rd(va)?))
    }

    /// Writes a byte.
    #[inline]
    pub(crate) fn w8(&self, va: u32, v: u8) -> KResult<()> {
        self.wr(va, [v])
    }

    /// Writes a little-endian `u16`.
    #[inline]
    pub(crate) fn w16(&self, va: u32, v: u16) -> KResult<()> {
        self.wr(va, v.to_le_bytes())
    }

    /// Writes a little-endian `u32`.
    #[inline]
    pub(crate) fn w32(&self, va: u32, v: u32) -> KResult<()> {
        self.wr(va, v.to_le_bytes())
    }

    /// Writes a little-endian `u64`.
    #[inline]
    pub(crate) fn w64(&self, va: u32, v: u64) -> KResult<()> {
        self.wr(va, v.to_le_bytes())
    }

    /// Reads a NUL-terminated byte string of at most `max` bytes.
    pub(crate) fn cstr(&self, va: u32, max: u32) -> KResult<Vec<u8>> {
        self.space.read_cstr(va, max).map_err(|f| Fault { va: f.va, write: false })
    }

    /// The bytes of the counted string (`STRING`, `ANSI_STRING`,
    /// `OBJECT_STRING`) whose descriptor is at `va`.
    pub(crate) fn ansi_string(&self, va: u32) -> KResult<Vec<u8>> {
        let len = u32::from(self.r16(va)?);
        let buf = self.r32(va + 4)?;
        if len == 0 || buf == 0 {
            return Ok(Vec::new());
        }
        self.read_vec(buf, len)
    }

    // -------------------------------------------- kernel-area fast access
    //
    // The kernel's own memory (its image area, the KPCR, the pool) lies in
    // RAM reached through the contiguous window, which is always mapped:
    // these accessors take such an address and cannot fail.

    #[inline]
    fn kpa(va: u32) -> u32 {
        debug_assert!(va >= CONTIG_WINDOW, "{va:08x} is not a contiguous-window address");
        va.wrapping_sub(CONTIG_WINDOW)
    }

    /// Reads a byte of kernel memory.
    #[inline]
    pub(crate) fn kr8(&self, va: u32) -> u8 {
        self.ram.read_u8(Self::kpa(va)).unwrap_or(0)
    }

    /// Reads a `u32` of kernel memory.
    #[inline]
    pub(crate) fn kr32(&self, va: u32) -> u32 {
        self.ram.read_u32(Self::kpa(va)).unwrap_or(0)
    }

    /// Writes a byte of kernel memory.
    #[inline]
    pub(crate) fn kw8(&self, va: u32, v: u8) {
        let pa = Self::kpa(va);
        self.note_store(pa, 1);
        self.ram.write_u8(pa, v);
    }

    /// Writes a `u16` of kernel memory.
    #[inline]
    pub(crate) fn kw16(&self, va: u32, v: u16) {
        let pa = Self::kpa(va);
        self.note_store(pa, 2);
        self.ram.write_u16(pa, v);
    }

    /// Writes a `u32` of kernel memory.
    #[inline]
    pub(crate) fn kw32(&self, va: u32, v: u32) {
        let pa = Self::kpa(va);
        self.note_store(pa, 4);
        self.ram.write_u32(pa, v);
    }

    /// Writes bytes of kernel memory.
    pub(crate) fn kwrite(&self, va: u32, data: &[u8]) {
        let pa = Self::kpa(va);
        self.note_store(pa, data.len() as u32);
        self.ram.write(pa, data);
    }

    /// Zeroes kernel memory.
    pub(crate) fn kfill(&self, va: u32, byte: u8, len: u32) {
        let pa = Self::kpa(va);
        self.note_store(pa, len);
        self.ram.fill(pa, byte, len as usize);
    }

    // ------------------------------------------------------------ mapping

    /// Allocates zeroed pages at `[va, va + len)`.
    pub(crate) fn alloc_at(&mut self, va: u32, len: u32) -> bool {
        self.space.alloc_at(&mut self.phys, &mut self.maps, va, len, Access::ReadWrite).is_ok()
    }

    /// Unmaps `[va, va + len)` and frees its RAM pages; pages freed.
    pub(crate) fn free_at(&mut self, va: u32, len: u32) -> u32 {
        // every mapped run, not just up to the first hole (a sparse region)
        let end = u64::from(va) + u64::from(len);
        let mut at = u64::from(va & !(PAGE_SIZE - 1));
        while at < end {
            let limit = (end - at).min(0x4000_0000) as u32;
            let limit = (limit + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
            let (mapped, run) = self.space.mapped_run(at as u32, limit);
            if mapped {
                let runs: Vec<(u32, u32)> = self.space.phys_runs(at as u32, run).collect();
                self.freed.extend(runs);
            }
            at += u64::from(run.max(PAGE_SIZE));
        }
        self.space.free_at(&mut self.phys, &mut self.maps, va, len)
    }

    /// Frees physical pages that no address maps (or only a window does).
    pub(crate) fn free_phys(&mut self, pa: u32, len: u32) {
        if self.phys.free(pa, len) {
            self.freed.push((pa, len));
        }
    }

    /// The RAM freed since the last call, for the machine.
    pub(crate) fn take_freed(&mut self) -> Vec<(u32, u32)> {
        std::mem::take(&mut self.freed)
    }

    /// Maps `[va, va + len)` onto RAM at `pa`.
    pub(crate) fn map(&mut self, va: u32, pa: u32, len: u32, access: Access) -> bool {
        self.space.map(&mut self.maps, va, pa, len, access).is_ok()
    }

    /// Maps a window (an alias of RAM that is not a page's own address).
    pub(crate) fn map_window(&mut self, va: u32, pa: u32, len: u32) -> bool {
        self.space.map_window(&mut self.maps, va, pa, len, Access::ReadWrite).is_ok()
    }

    /// Maps device registers.
    pub(crate) fn map_mmio(&mut self, va: u32, pa: u32, len: u32) -> bool {
        self.space.map_mmio(&mut self.maps, va, pa, len).is_ok()
    }

    /// Changes the guest's access to the RAM pages of `[va, va + len)`.
    pub(crate) fn protect(&mut self, va: u32, len: u32, access: Access) {
        let _ = self.space.protect(&mut self.maps, va, len, access);
    }

    /// `MmAllocateContiguousMemory`'s placement.
    pub(crate) fn alloc_contig_top(&mut self, req: ContigRequest) -> Option<Contig> {
        self.space.alloc_contig_top(&mut self.phys, &mut self.maps, self.hooks.as_mut(), req)
    }

    /// The fallback by compaction.
    pub(crate) fn alloc_contig_compact(
        &mut self,
        req: ContigRequest,
        known_too: bool,
    ) -> Option<Contig> {
        self.space.alloc_contig_compact(
            &mut self.phys,
            &mut self.maps,
            self.hooks.as_mut(),
            req,
            known_too,
        )
    }

    /// The kernel's own blocks.
    pub(crate) fn alloc_contig_high(&mut self, len: u32) -> Option<u32> {
        self.space.alloc_contig_high(&mut self.phys, &mut self.maps, self.hooks.as_mut(), len)
    }
}
