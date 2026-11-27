//! The guest's address space as the interpreter enforces it: a flat page
//! table in software, the write-tracking restriction per physical page, and
//! every guest load and store.
//!
//! Nothing here depends on the host's page size: a guest page is a table
//! entry, not a host mapping, and guest RAM is reached through
//! [`xb_mem::Ram`]'s bounds-checked copies.
//!
//! # Synchronous protection
//!
//! [`PageProtector::protect`] must not return while a guest access it
//! forbids can still succeed. The interpreter checks a page's restriction
//! and then makes the access, so a `protect` from another thread could fall
//! between the two. The two sides therefore shake hands through a
//! generation counter: `protect` publishes the new restriction, advances the
//! generation and waits until the guest-CPU thread has acknowledged it. The
//! CPU thread acknowledges before each block it runs and between the
//! elements of a string instruction ([`Mem::poll`]), and is counted as
//! acknowledging everything while it is outside guest code ([`Mem::leave`]),
//! which includes every call into the `Bus`. `protect` thus waits at most
//! for the rest of one block (64 instructions), never for a fault handler,
//! as `xb-mem` requires.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use xb_cpu::{Access, Bus, PageProtector};
use xb_mem::Ram;

/// Page-table entry bits; the rest of an entry is the physical page base.
pub(crate) const PRESENT: u32 = 1;
/// The guest may read (and execute).
pub(crate) const READ: u32 = 2;
/// The guest may write.
pub(crate) const WRITE: u32 = 4;
/// Device registers: accesses go to the bus.
pub(crate) const MMIO: u32 = 8;

const PAGE: u32 = 0x1000;
const PAGES: usize = 1 << 20;

/// Write-tracking restriction values per physical page.
const OPEN: u8 = 0;
const READ_ONLY: u8 = 1;
const CLOSED: u8 = 2;

/// The CPU thread's acknowledged generation while it is outside guest code.
const OUTSIDE: u64 = u64::MAX;

/// Why a guest access could not complete.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PageFault {
    /// The faulting guest virtual address.
    pub va: u32,
    /// The physical address behind it, when a mapping exists.
    pub pa: Option<u32>,
    /// Whether the access was a write.
    pub write: bool,
}

/// State shared with [`Protector`] handles on other threads.
pub(crate) struct Shared {
    /// Per physical RAM page: [`OPEN`], [`READ_ONLY`] or [`CLOSED`].
    prot: Box<[AtomicU8]>,
    /// Advanced by every `protect`.
    generation: AtomicU64,
    /// The generation the CPU thread has seen, or [`OUTSIDE`].
    acked: AtomicU64,
}

/// The [`PageProtector`] the interpreter hands out.
pub(crate) struct Protector(pub(crate) Arc<Shared>);

impl PageProtector for Protector {
    fn protect(&self, pa: u32, len: u32, access: Access) {
        let v = match access {
            Access::ReadWrite => OPEN,
            Access::Read => READ_ONLY,
            Access::None => CLOSED,
        };
        let first = (pa / PAGE) as usize;
        let n = len.div_ceil(PAGE) as usize;
        for p in self.0.prot.iter().skip(first).take(n) {
            p.store(v, Ordering::Relaxed);
        }
        let g = self.0.generation.fetch_add(1, Ordering::SeqCst) + 1;
        // The CPU thread finishes the instruction it is in and acknowledges
        // at the next boundary; outside guest code it has nothing in flight.
        let mut spins = 0u32;
        loop {
            let a = self.0.acked.load(Ordering::SeqCst);
            if a == OUTSIDE || a >= g {
                break;
            }
            spins += 1;
            if spins < 64 {
                std::hint::spin_loop();
            } else {
                std::thread::yield_now();
            }
        }
    }
}

/// A device access in flight: physical address and size.
#[derive(Clone, Copy)]
pub(crate) enum Target {
    /// RAM at this physical address.
    Ram(u32),
    /// A device register at this physical address.
    Mmio(u32),
}

/// The guest address space and RAM.
pub(crate) struct Mem {
    pub(crate) ram: Ram,
    ram_size: u32,
    /// One entry per guest virtual page.
    pt: Box<[u32]>,
    shared: Arc<Shared>,
    /// The generation last acknowledged by this thread; generated code
    /// compares it with [`Mem::generation_ptr`]'s.
    pub(crate) seen: u64,
    /// Per physical RAM page: whether decoded blocks hold its bytes.
    code: Box<[bool]>,
    /// Physical pages with code that guest stores changed since the run
    /// loop last looked.
    pub(crate) smc: Vec<u32>,
}

impl Mem {
    pub(crate) fn new(ram: Ram) -> Self {
        let ram_size = ram.size();
        let pages = (ram_size / PAGE) as usize;
        Self {
            ram,
            ram_size,
            pt: vec![0; PAGES].into_boxed_slice(),
            shared: Arc::new(Shared {
                prot: (0..pages).map(|_| AtomicU8::new(OPEN)).collect(),
                generation: AtomicU64::new(0),
                acked: AtomicU64::new(OUTSIDE),
            }),
            seen: 0,
            code: vec![false; pages].into_boxed_slice(),
            smc: Vec::new(),
        }
    }

    /// The addresses of the tables generated code reads for a load or a
    /// store it makes itself: the page table, the restriction per physical
    /// page, RAM and the code flag per physical page; and the number of
    /// physical pages. They stay where they are for as long as `self` lives.
    pub(crate) fn tables(&self) -> ([usize; 4], u32) {
        (
            [
                self.pt.as_ptr() as usize,
                self.shared.prot.as_ptr() as usize,
                self.ram.as_ptr() as usize,
                self.code.as_ptr() as usize,
            ],
            self.shared.prot.len() as u32,
        )
    }

    /// The address of the generation counter `protect` advances, which
    /// lives as long as this `Mem`.
    pub(crate) fn generation_ptr(&self) -> usize {
        std::ptr::from_ref(&self.shared.generation) as usize
    }

    pub(crate) fn protector(&self) -> Arc<dyn PageProtector> {
        Arc::new(Protector(self.shared.clone()))
    }

    // ---- the handshake with `Protector::protect` ----

    /// The CPU thread starts (or resumes) running guest instructions.
    pub(crate) fn enter(&mut self) {
        loop {
            let g = self.shared.generation.load(Ordering::SeqCst);
            self.shared.acked.store(g, Ordering::SeqCst);
            if self.shared.generation.load(Ordering::SeqCst) == g {
                self.seen = g;
                return;
            }
        }
    }

    /// The CPU thread leaves guest code (a bus call, the end of `run`).
    pub(crate) fn leave(&self) {
        self.shared.acked.store(OUTSIDE, Ordering::SeqCst);
    }

    /// A block boundary: acknowledges any `protect` since the last.
    #[inline]
    pub(crate) fn poll(&mut self) {
        let g = self.shared.generation.load(Ordering::Acquire);
        if g != self.seen {
            self.seen = g;
            self.shared.acked.store(g, Ordering::Release);
        }
    }

    // ---- the page table (Mapper) ----

    pub(crate) fn map(&mut self, va: u32, pa: u32, len: u32, access: Access) {
        let bits = match access {
            Access::None => PRESENT,
            Access::Read => PRESENT | READ,
            Access::ReadWrite => PRESENT | READ | WRITE,
        };
        self.set(va, pa, len, bits);
    }

    pub(crate) fn map_mmio(&mut self, va: u32, pa: u32, len: u32) {
        self.set(va, pa, len, PRESENT | READ | WRITE | MMIO);
    }

    pub(crate) fn unmap(&mut self, va: u32, len: u32) {
        let first = (va / PAGE) as usize;
        for e in self.pt.iter_mut().skip(first).take(len.div_ceil(PAGE) as usize) {
            *e = 0;
        }
    }

    fn set(&mut self, va: u32, pa: u32, len: u32, bits: u32) {
        let first = (va / PAGE) as usize;
        let n = (len.div_ceil(PAGE) as usize).min(PAGES - first);
        for i in 0..n {
            self.pt[first + i] = pa.wrapping_add((i as u32) * PAGE) & !(PAGE - 1) | bits;
        }
    }

    /// The page-table entry of `va`.
    #[inline]
    pub(crate) fn entry(&self, va: u32) -> u32 {
        self.pt[(va / PAGE) as usize]
    }

    // ---- translation ----

    /// Where a guest access to `va` goes, checking the mapping and write
    /// tracking. The access must not cross a page.
    #[inline]
    pub(crate) fn target(&self, va: u32, write: bool) -> Result<Target, PageFault> {
        let e = self.entry(va);
        let pa = (e & !(PAGE - 1)) | (va & (PAGE - 1));
        let need = if write { WRITE } else { READ };
        if e & need == 0 {
            return Err(PageFault { va, pa: (e & PRESENT != 0).then_some(pa), write });
        }
        if e & MMIO != 0 {
            return Ok(Target::Mmio(pa));
        }
        let Some(p) = self.shared.prot.get((pa / PAGE) as usize) else {
            return Err(PageFault { va, pa: Some(pa), write });
        };
        let p = p.load(Ordering::Relaxed);
        if p == CLOSED || (write && p == READ_ONLY) {
            return Err(PageFault { va, pa: Some(pa), write });
        }
        Ok(Target::Ram(pa))
    }

    // ---- code pages ----

    /// Blocks now hold bytes of physical page `page`.
    pub(crate) fn mark_code(&mut self, page: u32) {
        if let Some(c) = self.code.get_mut(page as usize) {
            *c = true;
        }
    }

    /// No block holds bytes of physical page `page` any more.
    pub(crate) fn clear_code(&mut self, page: u32) {
        if let Some(c) = self.code.get_mut(page as usize) {
            *c = false;
        }
    }

    #[inline]
    fn stored(&mut self, pa: u32, len: u32) {
        let first = pa / PAGE;
        let last = (pa + len - 1) / PAGE;
        for page in first..=last {
            if self.code.get(page as usize).copied().unwrap_or(false) {
                self.code[page as usize] = false;
                self.smc.push(page);
            }
        }
    }

    // ---- loads and stores ----

    /// Reads `N` bytes (1 to 16) at `va`.
    #[inline]
    pub(crate) fn read<const N: usize>(
        &mut self,
        va: u32,
        bus: &mut dyn Bus,
    ) -> Result<[u8; N], PageFault> {
        let mut buf = [0u8; N];
        if (va & (PAGE - 1)) as usize + N <= PAGE as usize {
            match self.target(va, false)? {
                Target::Ram(pa) => {
                    if !self.ram.read(pa, &mut buf) {
                        return Err(PageFault { va, pa: Some(pa), write: false });
                    }
                }
                Target::Mmio(pa) => self.mmio_read(pa, &mut buf, bus),
            }
            return Ok(buf);
        }
        // Across a page: both parts are checked before anything is read, so
        // a fault leaves nothing half done.
        let split = (PAGE - (va & (PAGE - 1))) as usize;
        let hi = va.wrapping_add(split as u32);
        let a = self.target(va, false)?;
        let b = self.target(hi, false)?;
        self.read_part(a, va, &mut buf[..split], bus)?;
        self.read_part(b, hi, &mut buf[split..], bus)?;
        Ok(buf)
    }

    fn read_part(
        &mut self,
        t: Target,
        va: u32,
        buf: &mut [u8],
        bus: &mut dyn Bus,
    ) -> Result<(), PageFault> {
        match t {
            Target::Ram(pa) => {
                if self.ram.read(pa, buf) {
                    Ok(())
                } else {
                    Err(PageFault { va, pa: Some(pa), write: false })
                }
            }
            Target::Mmio(pa) => {
                self.mmio_read(pa, buf, bus);
                Ok(())
            }
        }
    }

    /// Writes `N` bytes at `va`: the common case, a store to RAM within one
    /// page, with the length known to the compiler.
    #[inline]
    pub(crate) fn write_n<const N: usize>(
        &mut self,
        va: u32,
        data: [u8; N],
        bus: &mut dyn Bus,
    ) -> Result<(), PageFault> {
        if (va & (PAGE - 1)) as usize + N <= PAGE as usize
            && let Target::Ram(pa) = self.target(va, true)?
        {
            if !self.ram.write(pa, &data) {
                return Err(PageFault { va, pa: Some(pa), write: true });
            }
            let page = (pa / PAGE) as usize;
            if self.code.get(page).copied().unwrap_or(false) {
                self.stored(pa, N as u32);
            }
            return Ok(());
        }
        self.write(va, &data, bus)
    }

    /// Writes `data` (1 to 16 bytes) at `va`.
    #[inline]
    pub(crate) fn write(
        &mut self,
        va: u32,
        data: &[u8],
        bus: &mut dyn Bus,
    ) -> Result<(), PageFault> {
        let n = data.len();
        if (va & (PAGE - 1)) as usize + n <= PAGE as usize {
            let t = self.target(va, true)?;
            return self.write_part(t, va, data, bus);
        }
        let split = (PAGE - (va & (PAGE - 1))) as usize;
        let hi = va.wrapping_add(split as u32);
        let a = self.target(va, true)?;
        let b = self.target(hi, true)?;
        self.write_part(a, va, &data[..split], bus)?;
        self.write_part(b, hi, &data[split..], bus)
    }

    fn write_part(
        &mut self,
        t: Target,
        va: u32,
        data: &[u8],
        bus: &mut dyn Bus,
    ) -> Result<(), PageFault> {
        match t {
            Target::Ram(pa) => {
                if !self.ram.write(pa, data) {
                    return Err(PageFault { va, pa: Some(pa), write: true });
                }
                self.stored(pa, data.len() as u32);
                Ok(())
            }
            Target::Mmio(pa) => {
                self.mmio_write(pa, data, bus);
                Ok(())
            }
        }
    }

    /// Device reads of 1, 2 or 4 bytes go out as they are; anything else
    /// as doublewords (the translator does the same for 8- and 16-byte
    /// operands).
    fn mmio_read(&mut self, pa: u32, buf: &mut [u8], bus: &mut dyn Bus) {
        self.leave();
        match buf.len() {
            1 | 2 | 4 => {
                let v = bus.mmio_read(pa, buf.len() as u8);
                buf.copy_from_slice(&v.to_le_bytes()[..buf.len()]);
            }
            _ => {
                for (i, c) in buf.chunks_mut(4).enumerate() {
                    let v = bus.mmio_read(pa + 4 * i as u32, c.len() as u8);
                    c.copy_from_slice(&v.to_le_bytes()[..c.len()]);
                }
            }
        }
        self.enter();
    }

    fn mmio_write(&mut self, pa: u32, data: &[u8], bus: &mut dyn Bus) {
        self.leave();
        match data.len() {
            1 | 2 | 4 => {
                let mut b = [0u8; 4];
                b[..data.len()].copy_from_slice(data);
                bus.mmio_write(pa, data.len() as u8, u32::from_le_bytes(b));
            }
            _ => {
                for (i, c) in data.chunks(4).enumerate() {
                    let mut b = [0u8; 4];
                    b[..c.len()].copy_from_slice(c);
                    bus.mmio_write(pa + 4 * i as u32, c.len() as u8, u32::from_le_bytes(b));
                }
            }
        }
        self.enter();
    }

    /// Reads instruction bytes at `va` up to the end of its page, for the
    /// decoder. `Ok(None)` means the page is device memory.
    pub(crate) fn fetch_page(&self, va: u32) -> Result<Option<Vec<u8>>, PageFault> {
        match self.target(va, false)? {
            Target::Mmio(_) => Ok(None),
            Target::Ram(pa) => {
                let len = (PAGE - (va & (PAGE - 1))) as usize;
                Ok(self.ram.read_vec(pa, len))
            }
        }
    }

    /// The physical page behind `va` for code, if it is RAM.
    pub(crate) fn code_page(&self, va: u32) -> Option<u32> {
        match self.target(va, false) {
            Ok(Target::Ram(pa)) if pa < self.ram_size => Some(pa / PAGE),
            _ => None,
        }
    }
}
