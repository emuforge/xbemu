//! The translation cache: the code region, the blocks in it, how they are
//! found, chained and thrown away.
//!
//! The self-modifying-code policy is in here too, because this backend owns
//! the protection of guest pages.
//!
//! A block is keyed by the guest virtual address it starts at and depends on
//! two things: the virtual-to-physical mapping of the pages its bytes are on
//! (so `Mapper` changes drop blocks by virtual page) and the bytes themselves
//! (so blocks are also listed by physical page, and a physical page with
//! blocks is write protected in every view of it).
//!
//! Blocks are only ever killed from `X64Cpu`'s Rust code, never while
//! translated code is on the shadow stack (each entry into translated code
//! starts with that stack empty), so a dead block's code is unreachable the
//! moment its incoming jumps are unpatched and its table entries cleared.

use std::collections::{BTreeMap, HashMap};
use std::hash::{BuildHasherDefault, Hasher};
use std::time::{Duration, Instant};

use crate::asm::patch_rel32;
use crate::host::ExecRegion;
use crate::mem::Memory;
use crate::stubs::{self, L1_BITS, L1_SLOT, Stubs};
use crate::translate::{ExitSite, InsnMap, Translation};

/// Size of the code region. Halo's whole executable translates into a
/// fraction of this; a full region is flushed and refilled.
const REGION_SIZE: usize = 128 << 20;
const L1_BYTES: usize = (1 << L1_BITS) * L1_SLOT;
const STUBS_BYTES: usize = 64 << 10;
/// The single-step thunk's code, then (at `SCRATCH_DATA`) its data buffer.
const SCRATCH_BYTES: usize = 4096;
const SCRATCH_DATA: usize = 3072;
/// Room a new block may need; less than this left flushes the cache.
const HEADROOM: usize = 64 << 10;

/// Data writes to a code page within [`SMC_WINDOW`] after which the page is
/// no longer write protected (the reference gives up after 64 writes, ever;
/// the window keeps a page that is rewritten with new code now and then, as
/// Tomb Raider Legend does, watched for good).
const SMC_DATA_HITS: u32 = 64;
const SMC_WINDOW: Duration = Duration::from_secs(1);

/// A multiplicative hasher for `u32` keys (guest addresses, page numbers).
#[derive(Default)]
pub(crate) struct U32Hasher(u64);

impl Hasher for U32Hasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ u64::from(b)).wrapping_mul(0x0100_0000_01B3);
        }
    }
    fn write_u32(&mut self, v: u32) {
        self.0 = u64::from(v).wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(26);
    }
}

type Map<V> = HashMap<u32, V, BuildHasherDefault<U32Hasher>>;

struct Block {
    eip: u32,
    /// Offset of its code in the region, and the code's length.
    code: u32,
    len: u32,
    entry_nopoll: u32,
    map: Vec<InsnMap>,
    exits: Vec<ExitSite>,
    /// One past the last guest byte it depends on.
    dep_end: u64,
    /// The pages its bytes are on: virtual page number, physical page number.
    pages: Vec<(u32, u32)>,
}

#[derive(Default)]
struct PageStats {
    window: Option<Instant>,
    hits: u32,
    unwatched: bool,
}

/// Where a host address inside translated code belongs.
#[derive(Clone, Copy, Debug)]
pub(crate) struct HostSite {
    /// The guest instruction.
    pub(crate) insn: InsnMap,
    /// Host address of the instruction's first byte, to run it again; `None`
    /// in the single-step thunk.
    pub(crate) resume: Option<u64>,
}

/// The translation cache.
pub(crate) struct Cache {
    region: ExecRegion,
    stubs: Stubs,
    code_p: usize,
    code_end: usize,
    blocks: Vec<Option<Block>>,
    free: Vec<u32>,
    by_eip: Map<u32>,
    by_vpage: Map<Vec<u32>>,
    by_ppage: Map<Vec<u32>>,
    /// A bit per virtual page that has blocks: an invalidation of a range
    /// without any (memory mapped or freed, nearly always) looks at a word
    /// per 64 pages.
    vpage_bits: Vec<u64>,
    /// Target address to the exits that go there: block id, exit index.
    incoming: Map<Vec<(u32, u32)>>,
    by_host: BTreeMap<u32, u32>,
    stats: Map<PageStats>,
    scratch_map: Vec<InsnMap>,
    scratch_len: usize,
    /// Bumped whenever a block dies.
    generation: u64,
    flushes: u64,
    translated: u64,
}

impl Cache {
    /// Allocates the region and assembles the stubs.
    pub(crate) fn new(wrgsbase: bool) -> Result<Self, String> {
        let region = ExecRegion::new(REGION_SIZE)?;
        let base = region.ptr() as u64;
        let (code, stubs) = stubs::build(base + L1_BYTES as u64, base, wrgsbase)?;
        if code.len() > STUBS_BYTES {
            return Err("the stubs do not fit their area".into());
        }
        // SAFETY: the stubs' area is inside the fresh region.
        unsafe {
            std::ptr::copy_nonoverlapping(code.as_ptr(), region.ptr().add(L1_BYTES), code.len())
        };
        let code_begin = L1_BYTES + STUBS_BYTES + SCRATCH_BYTES;
        let mut c = Self {
            code_end: region.len(),
            region,
            stubs,
            code_p: code_begin,
            blocks: Vec::new(),
            free: Vec::new(),
            by_eip: Map::default(),
            by_vpage: Map::default(),
            by_ppage: Map::default(),
            vpage_bits: vec![0; 1 << 14],
            incoming: Map::default(),
            by_host: BTreeMap::new(),
            stats: Map::default(),
            scratch_map: Vec::new(),
            scratch_len: 0,
            generation: 0,
            flushes: 0,
            translated: 0,
        };
        c.l1_clear();
        Ok(c)
    }

    /// The stubs' addresses.
    pub(crate) fn stubs(&self) -> &Stubs {
        &self.stubs
    }

    /// `[lo, hi)` of the stubs.
    pub(crate) fn stubs_range(&self) -> (usize, usize) {
        let lo = self.region.ptr() as usize + L1_BYTES;
        (lo, lo + STUBS_BYTES)
    }

    /// `[lo, hi)` of everything a guest instruction can fault in: the
    /// single-step thunk and the blocks.
    pub(crate) fn code_range(&self) -> (usize, usize) {
        let lo = self.region.ptr() as usize + L1_BYTES + STUBS_BYTES;
        (lo, self.region.ptr() as usize + self.region.len())
    }

    /// Changes whenever a block dies: a host address inside a block taken
    /// before is still good if this has not changed since.
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    /// Blocks translated and whole-cache flushes so far.
    pub(crate) fn counters(&self) -> (u64, u64) {
        (self.translated, self.flushes)
    }

    fn addr(&self, off: usize) -> u64 {
        self.region.ptr() as u64 + off as u64
    }

    // ---- the dispatcher's table ----

    fn l1_slot(&self, eip: u32) -> *mut u8 {
        let idx = (eip & ((1 << L1_BITS) - 1)) as usize;
        // SAFETY: the table is the first `L1_BYTES` of the region.
        unsafe { self.region.ptr().add(idx * L1_SLOT) }
    }

    fn l1_write(&self, slot: *mut u8, way: usize, neg_eip: u32, code: u64) {
        // SAFETY: `slot` is a table slot of `L1_SLOT` bytes; each way is a
        // u32 and, 8 bytes on, a u64.
        unsafe {
            slot.add(way * 16).cast::<u32>().write(neg_eip);
            slot.add(way * 16 + 8).cast::<u64>().write(code);
        }
    }

    fn l1_read(&self, slot: *mut u8, way: usize) -> (u32, u64) {
        // SAFETY: as in `l1_write`.
        unsafe {
            (slot.add(way * 16).cast::<u32>().read(), slot.add(way * 16 + 8).cast::<u64>().read())
        }
    }

    /// Enters `eip` as the slot's first way; what was there becomes the
    /// second.
    fn l1_set(&self, eip: u32, code: u64) {
        let neg = 0u32.wrapping_sub(eip);
        let slot = self.l1_slot(eip);
        let (n0, c0) = self.l1_read(slot, 0);
        if n0 == neg && eip != 0 {
            self.l1_write(slot, 0, neg, code);
            return;
        }
        self.l1_write(slot, 1, n0, c0);
        self.l1_write(slot, 0, neg, code);
    }

    fn l1_drop(&self, eip: u32) {
        let neg = 0u32.wrapping_sub(eip);
        let slot = self.l1_slot(eip);
        for way in 0..2 {
            if self.l1_read(slot, way).0 == neg {
                // eip 0 never has a block: a miss.
                self.l1_write(slot, way, 0, self.stubs.lookup);
            }
        }
    }

    fn l1_clear(&mut self) {
        for idx in 0..1u32 << L1_BITS {
            let slot = self.l1_slot(idx);
            self.l1_write(slot, 0, 0, self.stubs.lookup);
            self.l1_write(slot, 1, 0, self.stubs.lookup);
        }
    }

    // ---- blocks ----

    /// The translation of the block at `eip`, if there is one. Also puts it
    /// (back) in the dispatcher's table.
    pub(crate) fn lookup(&mut self, eip: u32) -> Option<u64> {
        let id = *self.by_eip.get(&eip)?;
        let code = self.addr(self.blocks[id as usize].as_ref()?.code as usize);
        self.l1_set(eip, code);
        Some(code)
    }

    /// Where the next block will be placed. Flushes the cache first if it
    /// is nearly full.
    pub(crate) fn next_block_addr(&mut self, mem: &Memory) -> u64 {
        if self.code_end - self.code_p < HEADROOM {
            self.flush(mem);
        }
        self.addr(self.code_p)
    }

    /// Adds a block translated for [`Cache::next_block_addr`]. Returns its
    /// entry.
    pub(crate) fn insert(&mut self, eip: u32, t: Translation, mem: &Memory) -> u64 {
        let code = self.code_p;
        debug_assert!(code + t.code.len() <= self.code_end);
        // SAFETY: the destination is unused space inside the region
        // (`next_block_addr` kept `HEADROOM`, far more than a block).
        unsafe {
            std::ptr::copy_nonoverlapping(
                t.code.as_ptr(),
                self.region.ptr().add(code),
                t.code.len(),
            )
        };
        self.code_p = (code + t.code.len() + 15) & !15;

        let mut pages = Vec::new();
        let last = ((t.dep_end - 1) >> 12).min(0xF_FFFF) as u32;
        for vpn in eip >> 12..=last {
            let pte = mem.pte(vpn << 12);
            if pte.is_ram() {
                pages.push((vpn, pte.pa() >> 12));
            }
        }
        let id = match self.free.pop() {
            Some(id) => id,
            None => {
                self.blocks.push(None);
                self.blocks.len() as u32 - 1
            }
        };
        for &(vpn, ppn) in &pages {
            self.by_vpage.entry(vpn).or_default().push(id);
            self.vpage_bits[(vpn >> 6) as usize] |= 1 << (vpn & 63);
            self.by_ppage.entry(ppn).or_default().push(id);
            if !self.stats.get(&ppn).is_some_and(|s| s.unwatched) {
                mem.set_code(ppn << 12, true);
            }
        }
        let block = Block {
            eip,
            code: code as u32,
            len: t.code.len() as u32,
            entry_nopoll: t.entry_nopoll,
            map: t.map,
            exits: t.exits,
            dep_end: t.dep_end,
            pages,
        };
        // Its exits to blocks that exist; exits of others to it.
        for (k, e) in block.exits.iter().enumerate() {
            if let Some(target) =
                self.by_eip.get(&e.target).and_then(|&b| self.blocks[b as usize].as_ref())
            {
                self.chain(code, e, target);
            }
            self.incoming.entry(e.target).or_default().push((id, k as u32));
        }
        self.by_eip.insert(eip, id);
        self.by_host.insert(code as u32, id);
        self.blocks[id as usize] = Some(block);
        let block = self.blocks[id as usize].as_ref().expect("just stored");
        if let Some(sites) = self.incoming.get(&eip) {
            for &(from, k) in sites {
                if let Some(src) = self.blocks[from as usize].as_ref() {
                    self.chain(src.code as usize, &src.exits[k as usize], block);
                }
            }
        }
        let entry = self.addr(code);
        self.l1_set(eip, entry);
        self.translated += 1;
        entry
    }

    /// Points an exit of the block at region offset `from_code` at `target`:
    /// through the kick poll for a backward edge, past it otherwise.
    fn chain(&self, from_code: usize, e: &ExitSite, target: &Block) {
        let to = target.code as usize + if e.backward { 0 } else { target.entry_nopoll as usize };
        // SAFETY: `patch` is the offset of a rel32 inside a live block, and
        // translated code is not running (see the module's documentation).
        unsafe { patch_rel32(self.region.ptr().add(from_code + e.patch as usize), self.addr(to)) };
    }

    fn unchain(&self, from_code: usize, e: &ExitSite) {
        // SAFETY: as in `chain`.
        unsafe {
            patch_rel32(
                self.region.ptr().add(from_code + e.patch as usize),
                self.addr(from_code + e.stub as usize),
            );
        }
    }

    fn kill(&mut self, id: u32, mem: &Memory) {
        let Some(b) = self.blocks[id as usize].take() else { return };
        self.generation += 1;
        self.free.push(id);
        self.by_host.remove(&b.code);
        if self.by_eip.get(&b.eip) == Some(&id) {
            self.by_eip.remove(&b.eip);
        }
        self.l1_drop(b.eip);
        // Jumps into it go back to their stubs: the dispatcher will find
        // the new translation.
        if let Some(sites) = self.incoming.get(&b.eip) {
            for &(from, k) in sites {
                if let Some(src) = self.blocks[from as usize].as_ref() {
                    self.unchain(src.code as usize, &src.exits[k as usize]);
                }
            }
        }
        for e in &b.exits {
            if let Some(sites) = self.incoming.get_mut(&e.target) {
                sites.retain(|&(from, _)| from != id);
                if sites.is_empty() {
                    self.incoming.remove(&e.target);
                }
            }
        }
        for &(vpn, ppn) in &b.pages {
            if let Some(v) = self.by_vpage.get_mut(&vpn) {
                v.retain(|&x| x != id);
                if v.is_empty() {
                    self.by_vpage.remove(&vpn);
                    self.vpage_bits[(vpn >> 6) as usize] &= !(1 << (vpn & 63));
                }
            }
            if let Some(v) = self.by_ppage.get_mut(&ppn) {
                v.retain(|&x| x != id);
                if v.is_empty() {
                    self.by_ppage.remove(&ppn);
                    mem.set_code(ppn << 12, false);
                }
            }
        }
    }

    /// Drops the blocks with bytes on guest virtual `[va, va + len)`.
    pub(crate) fn invalidate_virtual(&mut self, va: u32, len: u32, mem: &Memory) {
        if len == 0 {
            return;
        }
        let first = va >> 12;
        let last = ((u64::from(va) + u64::from(len) - 1) >> 12).min(0xF_FFFF) as u32;
        for w in first >> 6..=last >> 6 {
            let mut bits = self.vpage_bits[w as usize];
            if w == first >> 6 {
                bits &= !0u64 << (first & 63);
            }
            if w == last >> 6 {
                bits &= !0u64 >> (63 - (last & 63));
            }
            while bits != 0 {
                let vpn = (w << 6) | bits.trailing_zeros();
                bits &= bits - 1;
                for id in self.by_vpage.get(&vpn).cloned().unwrap_or_default() {
                    self.kill(id, mem);
                }
            }
        }
    }

    /// Drops the blocks with bytes on physical `[pa, pa + len)`.
    pub(crate) fn invalidate_physical(&mut self, pa: u32, len: u32, mem: &Memory) {
        if len == 0 {
            return;
        }
        let last = ((u64::from(pa) + u64::from(len) - 1) >> 12) as u32;
        for ppn in pa >> 12..=last {
            for id in self.by_ppage.get(&ppn).cloned().unwrap_or_default() {
                self.kill(id, mem);
            }
        }
    }

    /// The guest wrote to a write-protected code page at physical `pa`: its
    /// translations go and the page becomes writable until code on it is
    /// translated again.
    ///
    /// A write that lands outside every block's bytes is data beside code
    /// (the XDK's libraries keep their variables in their code sections).
    /// A page that takes such writes continuously stops being protected:
    /// each one costs a fault and the retranslation of the page's blocks.
    /// From then on a change of the code on that page is not noticed, as in
    /// the reference.
    pub(crate) fn code_written(&mut self, pa: u32, mem: &Memory) {
        let ppn = pa >> 12;
        let off = pa & 0xFFF;
        let ids = self.by_ppage.get(&ppn).cloned().unwrap_or_default();
        let hits_code = ids.iter().filter_map(|&id| self.blocks[id as usize].as_ref()).any(|b| {
            b.pages.iter().filter(|&&(_, p)| p == ppn).any(|&(vpn, _)| {
                let page = u64::from(vpn) << 12;
                let lo = u64::from(b.eip).max(page) - page;
                let hi = b.dep_end.min(page + 4096) - page;
                // A write may begin up to 15 bytes before the byte reported.
                u64::from(off) + 16 > lo && u64::from(off) < hi
            })
        });
        for id in ids {
            self.kill(id, mem);
        }
        mem.set_code(ppn << 12, false);
        if hits_code {
            return;
        }
        let s = self.stats.entry(ppn).or_default();
        let now = Instant::now();
        if s.window.is_none_or(|w| now.duration_since(w) > SMC_WINDOW) {
            s.window = Some(now);
            s.hits = 0;
        }
        s.hits += 1;
        if s.hits >= SMC_DATA_HITS && !s.unwatched {
            s.unwatched = true;
            log::info!(target: "cpu",
                "code page at physical {:#010x} is written all the time (data beside code): not watched any more",
                ppn << 12);
        }
    }

    /// Everything translated goes (the region is full).
    pub(crate) fn flush(&mut self, mem: &Memory) {
        for &ppn in self.by_ppage.keys() {
            mem.set_code(ppn << 12, false);
        }
        self.blocks.clear();
        self.free.clear();
        self.by_eip.clear();
        self.by_vpage.clear();
        self.by_ppage.clear();
        self.vpage_bits.fill(0);
        self.incoming.clear();
        self.by_host.clear();
        self.l1_clear();
        let begin = L1_BYTES + STUBS_BYTES + SCRATCH_BYTES;
        self.region.discard(begin, self.code_p - begin);
        self.code_p = begin;
        self.generation += 1;
        self.flushes += 1;
        log::debug!(target: "cpu", "translation cache flushed ({} so far)", self.flushes);
    }

    /// Makes the next flush come after `bytes` of translated code (tests).
    #[cfg(test)]
    pub(crate) fn set_code_limit(&mut self, bytes: usize, mem: &Memory) {
        let begin = L1_BYTES + STUBS_BYTES + SCRATCH_BYTES;
        self.code_end = (begin + bytes.max(2 * HEADROOM)).min(self.region.len());
        if self.code_p > self.code_end {
            self.flush(mem);
        }
    }

    // ---- the single-step thunk ----

    /// Address of the single-step thunk.
    pub(crate) fn scratch_addr(&self) -> u64 {
        self.addr(L1_BYTES + STUBS_BYTES)
    }

    /// The thunk's 16-byte data buffer (a device access's operand).
    pub(crate) fn scratch_data(&self) -> *mut u8 {
        // SAFETY: inside the scratch area.
        unsafe { self.region.ptr().add(L1_BYTES + STUBS_BYTES + SCRATCH_DATA) }
    }

    /// Installs a translation made for [`Cache::scratch_addr`]. Returns its
    /// entry, or `None` if it does not fit.
    pub(crate) fn set_scratch(&mut self, t: &Translation) -> Option<u64> {
        if t.code.len() > SCRATCH_DATA {
            return None;
        }
        // SAFETY: the scratch area is reserved for this and nothing is
        // executing it.
        unsafe {
            std::ptr::copy_nonoverlapping(
                t.code.as_ptr(),
                self.region.ptr().add(L1_BYTES + STUBS_BYTES),
                t.code.len(),
            );
        }
        self.scratch_map.clone_from(&t.map);
        self.scratch_len = t.code.len();
        Some(self.scratch_addr() + u64::from(t.entry_nopoll))
    }

    // ---- faults ----

    /// The guest instruction a host address in translated code is part of.
    pub(crate) fn site(&self, rip: u64) -> Option<HostSite> {
        let off = rip.checked_sub(self.region.ptr() as u64)? as usize;
        let pick = |map: &[InsnMap], rel: usize| {
            let k = map.partition_point(|m| m.host_off as usize <= rel);
            (k > 0).then(|| map[k - 1])
        };
        let scratch = L1_BYTES + STUBS_BYTES;
        if off >= scratch && off < scratch + self.scratch_len {
            return pick(&self.scratch_map, off - scratch)
                .map(|insn| HostSite { insn, resume: None });
        }
        let (&code, &id) = self.by_host.range(..=u32::try_from(off).ok()?).next_back()?;
        let b = self.blocks[id as usize].as_ref()?;
        if off >= (code + b.len) as usize {
            return None;
        }
        let insn = pick(&b.map, off - code as usize)?;
        Some(HostSite { insn, resume: Some(self.addr(code as usize + insn.host_off as usize)) })
    }
}
