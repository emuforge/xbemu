//! The memory manager.
//!
//! Virtual memory (`NtAllocateVirtualMemory` and friends) in the title's
//! `0x0001_0000..0x7FFE_0000` range: reservations at 64 KiB granularity,
//! commits backed page by page with physical RAM (contiguous runs where
//! possible). Contiguous and system memory come from physical RAM top-down and
//! are reached through the `0x8000_0000` window, so VA = `0x8000_0000 | PA`
//! there. The mechanisms (page table, physical allocator, the allocators that
//! relocate pages) are `xb-mem`'s; the policies are here.
//!
//! Protection: a read-only protection (`PAGE_READONLY`, `PAGE_EXECUTE_READ`)
//! of committed pages outside the title's image holds for the guest (a store
//! faults to the title), as the reference does; every other protection is
//! recorded only and the pages stay read-write.
//!
//! Not here: the reference's file of the last title's RAM for a relaunch (a
//! block always starts zeroed) and its files of the kept memory:
//! [`Kernel::mm_keep_collect`] and [`Kernel::mm_keep_restore`] hand the ranges
//! to and from the machine instead.

use std::fmt::Write;

use xb_mem::{Access, ContigRequest};
use xb_types::{CONTIG_WINDOW, PAGE_SIZE, align_up};

use crate::gm::KResult;
use crate::kernel::{
    Args, GPU_PAD, GPU_TOP, Kernel, STATUS_INVALID_PARAMETER, STATUS_NO_MEMORY, STATUS_SUCCESS,
    TRAP_VA,
};
use crate::platform::Platform;

const MEM_COMMIT: u32 = 0x1000;
const MEM_RESERVE: u32 = 0x2000;
const MEM_DECOMMIT: u32 = 0x4000;
const MEM_RELEASE: u32 = 0x8000;
const MEM_FREE: u32 = 0x1_0000;
const MEM_PRIVATE: u32 = 0x2_0000;
const MEM_TOP_DOWN: u32 = 0x10_0000;
const MEM_NOZERO: u32 = 0x80_0000;
const MEM_IMAGE: u32 = 0x100_0000;

const PAGE_NOACCESS: u32 = 0x01;
const PAGE_READWRITE: u32 = 0x04;

const STATUS_CONFLICTING_ADDRESSES: u32 = 0xC000_0018;
const STATUS_UNABLE_TO_FREE_VM: u32 = 0xC000_001A;
const STATUS_NOT_COMMITTED: u32 = 0xC000_002D;
const STATUS_FREE_VM_NOT_AT_BASE: u32 = 0xC000_009F;
const STATUS_MEMORY_NOT_ALLOCATED: u32 = 0xC000_00A0;

/// The title's virtual memory: regions lie in `[USER_LO, USER_HI)`.
const USER_LO: u32 = 0x0001_0000;
const USER_HI: u32 = 0x7FFE_0000;
const VA_GRAIN: u32 = 0x1_0000;
const MAX_REGIONS: usize = 4096;
/// Contiguous blocks: as many as RAM has pages (128 MiB's), so that only the
/// memory runs out, as on the console. SNK vs. Capcom: SVC Chaos makes
/// thousands of 1365-byte ones at boot: past 4096 every one failed with 10
/// MiB free, and a disc read into a block it didn't get sent it back to the
/// dashboard from the Neo Geo screen.
const MAX_CONTIG: usize = 32768;
const MAX_KEPT: usize = 8;

// The region search never reaches the kernel's trap page, the contiguous
// window or the kernel's stacks: all lie above the title's range.
const _: () = assert!(USER_HI <= CONTIG_WINDOW && TRAP_VA >= USER_HI);

/// PAGE_READONLY or PAGE_EXECUTE_READ, the cache bits aside: a store faults.
fn prot_read_only(protect: u32) -> bool {
    protect & 0xFF == 0x02 || protect & 0xFF == 0x20
}

/// A reservation of the title's address space (or its image).
#[derive(Clone, Copy, Default)]
struct Region {
    base: u32,
    size: u32,
    protect: u32,
    image: bool,
}

/// A contiguous block.
#[derive(Clone, Copy, Default)]
struct Block {
    va: u32,
    size: u32,
    /// The caller's return address (who holds the memory).
    ra: u32,
    /// Free pages above it it took (`contig_alloc`), freed with it.
    slack: u32,
    /// Its pages' protection, cache bits included (MmQueryAddressProtect).
    protect: u32,
}

/// The memory manager's state.
#[derive(Default)]
pub(crate) struct State {
    /// Virtual memory pages committed and decommitted (stats).
    pub commits: u64,
    pub decommits: u64,
    regions: Vec<Region>,
    contig: Vec<Block>,
    /// Contiguous memory kept for the next title: `(va, len)`.
    kept: Vec<(u32, u32)>,
    gpu_instance_end: u32,
    /// Contiguous allocations that failed since the last success.
    failed: u32,
    /// Contiguous allocations that relocated pages.
    moves: u32,
    protect_logged: u32,
    free_logged: u32,
    holders_logged: bool,
}

impl State {
    fn region_at(&self, va: u32) -> Option<usize> {
        self.regions.iter().position(|r| va >= r.base && va - r.base < r.size)
    }

    fn range_free(&self, base: u32, size: u32) -> bool {
        let end = u64::from(base) + u64::from(size);
        !self.regions.iter().any(|r| {
            u64::from(base) < u64::from(r.base) + u64::from(r.size) && u64::from(r.base) < end
        })
    }

    fn add_region(&mut self, base: u32, size: u32, protect: u32, image: bool) -> bool {
        if self.regions.len() == MAX_REGIONS {
            return false;
        }
        self.regions.push(Region { base, size, protect, image });
        true
    }

    fn contig_at(&self, va: u32) -> Option<usize> {
        self.contig.iter().position(|c| c.va == va)
    }

    /// The block whose pages hold `va`.
    fn contig_holding(&self, va: u32) -> Option<usize> {
        self.contig.iter().position(|c| va.wrapping_sub(c.va) < c.size)
    }

    /// The highest window for a block in the pages an aligned block's
    /// alignment left above it (its slack, below the block above): pages the
    /// console's allocator leaves free there, and the next contiguous block
    /// it finds room for takes them (`contig_alloc` claims them so that
    /// virtual memory doesn't). Maximum Chase's D3D frame buffers (64 KiB
    /// aligned) leave 8 KiB under its push buffer, where XAPI's USB pool
    /// (8 KiB) goes; placed below them instead, the 1 MiB the title reads
    /// into its 700 KiB sound buffer as a stage begins ran over the pool's
    /// endpoints, the gamepad's control endpoint a packet size of 0, and its
    /// first rumble divided by zero. The window's start in pa and the pages
    /// above it in that slack (the new block's own).
    fn slack_window(&mut self, size: u32, lo: u32, hi: u32, align: u32) -> Option<(u32, u32)> {
        let a = align.max(PAGE_SIZE);
        let mut best: Option<(usize, u32)> = None;
        for (i, c) in self.contig.iter().enumerate() {
            let w0 = (c.va & !CONTIG_WINDOW) + c.size;
            let w1 = w0 + c.slack;
            let top = if hi != 0 && hi < w1 { hi } else { w1 };
            if c.slack < size || top < w0 + size {
                continue;
            }
            let pa = (top - size) & !(a - 1);
            if pa >= w0 && pa >= lo && best.is_none_or(|(_, b)| pa > b) {
                best = Some((i, pa));
            }
        }
        let (i, pa) = best?;
        let c = &mut self.contig[i];
        let w0 = (c.va & !CONTIG_WINDOW) + c.size;
        let w1 = w0 + c.slack;
        c.slack = pa - w0;
        Some((pa, w1 - (pa + size)))
    }
}

impl Kernel {
    /// Physical memory held by contiguous allocations and by committed
    /// virtual memory (bytes), and the number of contiguous blocks.
    pub(crate) fn mm_usage(&self) -> (u32, u32, u32) {
        let c = self.mm.contig.iter().map(|c| c.size).sum();
        let v = self.mm.regions.iter().map(|r| self.committed(r)).sum();
        (c, v, self.mm.contig.len() as u32)
    }

    /// Bytes of `r` with pages behind them.
    fn committed(&self, r: &Region) -> u32 {
        self.gm.space.mapped_pages(r.base, r.size) * PAGE_SIZE
    }

    /// The title's image occupies `[base, base + size)` of the address space.
    pub(crate) fn mm_note_image(&mut self, base: u32, size: u32) {
        self.mm.add_region(base, align_up(size, PAGE_SIZE), PAGE_READWRITE, true);
    }

    /// A contiguous block of `size` bytes for the kernel's own use (the
    /// launch data page); its VA in the contiguous window, or 0.
    pub(crate) fn mm_contig_alloc(&mut self, size: u32) -> u32 {
        self.contig_alloc(size, 0, 0, PAGE_SIZE, PAGE_READWRITE, 0)
    }

    /// Backs `[va, va + len)` with zeroed pages where none are mapped yet;
    /// false when RAM is out.
    pub(crate) fn mm_back(&mut self, va: u32, len: u32) -> bool {
        let end = (u64::from(va) + u64::from(len)).div_ceil(4096) * 4096;
        let mut p = u64::from(va & !0xFFF);
        while p < end {
            if !self.gm.space.is_mapped(p as u32) && !self.gm.alloc_at(p as u32, 4096) {
                return false;
            }
            p += 4096;
        }
        true
    }

    /// The return address of the kernel call being served (at `[esp]`).
    fn caller(&self) -> u32 {
        self.gm.r32(self.esp()).unwrap_or(0)
    }

    /// A free range of the title's address space for a reservation of `size`
    /// bytes, or 0. Beyond the reference's search of the regions, a range with pages
    /// mapped outside any region (memory the loader or a test placed) is
    /// passed over too.
    fn find_free(&self, size: u32, top_down: bool) -> u32 {
        let size = align_up(size, VA_GRAIN);
        let free =
            |va: u32| self.mm.range_free(va, size) && self.gm.space.mapped_pages(va, size) == 0;
        if top_down {
            let mut va = (USER_HI - size) & !(VA_GRAIN - 1);
            while va >= USER_LO {
                if free(va) {
                    return va;
                }
                va -= VA_GRAIN;
            }
        } else {
            let mut va = USER_LO;
            while u64::from(va) + u64::from(size) <= u64::from(USER_HI) {
                if free(va) {
                    return va;
                }
                va += VA_GRAIN;
            }
        }
        0
    }

    /// The guest's access to the pages of `[va, va + len)` below the
    /// contiguous window: read-only or read-write.
    fn guest_ro(&mut self, va: u32, len: u32, ro: bool) {
        let va = va & !(PAGE_SIZE - 1);
        if va >= CONTIG_WINDOW {
            return;
        }
        let len = len.min(CONTIG_WINDOW - va);
        self.gm.protect(va, len, if ro { Access::Read } else { Access::ReadWrite });
    }

    /// All of it or none: a commit that runs out of physical memory gives
    /// back the pages it got. BloodRayne 2 asks for 64 MiB, then half of it,
    /// and so on until one fits: the first one's 54 MiB stayed committed,
    /// every smaller one failed to its 16 KiB, and the title went on without
    /// its heap.
    fn commit(&mut self, va: u32, size: u32, zero: bool) -> bool {
        let want = size - self.gm.space.mapped_pages(va, size) * PAGE_SIZE;
        if want > self.gm.phys.free_bytes() {
            return false;
        }
        let end = va + size;
        let mut got: Vec<(u32, u32)> = Vec::new();
        let mut ok = true;
        let mut p = va;
        while p < end {
            let (mapped, run) = self.gm.space.mapped_run(p, end - p);
            if mapped {
                p += run;
                continue;
            }
            // The largest contiguous physical piece there is for this run.
            let mut piece = run;
            let mut pa = None;
            while piece >= PAGE_SIZE {
                pa = self.gm.phys.alloc(piece, 0, u32::MAX);
                if pa.is_some() {
                    break;
                }
                piece = if piece > PAGE_SIZE { align_up(piece / 2, PAGE_SIZE) } else { 0 };
            }
            let Some(pa) = pa else {
                ok = false;
                break;
            };
            self.gm.space.pin_still_mapped(&mut self.gm.phys, pa, piece);
            if !self.gm.map(p, pa, piece, Access::ReadWrite) {
                self.gm.free_phys(pa, piece);
                ok = false;
                break;
            }
            got.push((p, piece));
            if zero {
                let _ = self.gm.fill(p, 0, piece);
            }
            self.mm.commits += u64::from(piece / PAGE_SIZE);
            p += piece;
        }
        if !ok {
            for (va, len) in got {
                self.decommit(va, len);
            }
        }
        ok
    }

    /// Unmaps the committed pages of `[va, va + size)` and frees their RAM,
    /// a run at a time: Riddick decommits ~20k pages in a second or two
    /// while it loads. Memory given back held code that ran, perhaps (a
    /// module a title loaded itself): its translations go with it, or the
    /// next code put at the same address ran as the old one. Pages of the
    /// RAM windows are not anyone's to decommit and stay.
    fn decommit(&mut self, va: u32, size: u32) {
        let end = (u64::from(va) + u64::from(size)).min(1 << 32);
        let own = |k: &Kernel, p: u64| {
            let e = k.gm.space.entry(p as u32);
            e.is_ram() && !e.is_window()
        };
        let mut p = u64::from(va & !(PAGE_SIZE - 1));
        while p < end {
            if !own(self, p) {
                p += u64::from(PAGE_SIZE);
                continue;
            }
            let mut q = p + u64::from(PAGE_SIZE);
            while q < end && own(self, q) {
                q += u64::from(PAGE_SIZE);
            }
            self.gm.invalidate_code(p as u32, (q - p) as u32);
            let freed = self.gm.free_at(p as u32, (q - p) as u32);
            self.mm.decommits += u64::from(freed);
            p = q;
        }
    }

    // ------------------------------------------------- contiguous and system

    /// The top of RAM, virtual pages moved out of the way (`xb-mem`), then
    /// the window needing the fewest moves; neither when there are fewer
    /// free pages than asked for. The block's pa and the pages moved.
    fn contig_place(&mut self, req: ContigRequest) -> Option<(u32, u32)> {
        if self.gm.phys.free_bytes() < req.len {
            return None;
        }
        let room = self.mm.contig.len() < MAX_CONTIG;
        let mut got = self.gm.alloc_contig_top(req);
        if got.is_none() && room {
            got = self.gm.alloc_contig_compact(req, false);
        }
        if got.is_none() && room {
            // The last resort: pages whose address went out moved too. (The
            // C first lets the file reads and writes in flight complete,
            // their buffers' pages locked meanwhile, and tries once more:
            // Project Gotham Racing copies a city to its cache as it asks
            // for the city's 22.6 MiB, the copy's buffers in the way.)
            got = self.gm.alloc_contig_compact(req, true);
            if let Some(c) = got {
                log::info!(target: "kernel",
                    "mm: contiguous allocation of {} KiB at {:08x} only with pages whose address went out moved ({} pages relocated)",
                    req.len >> 10, c.pa, c.moved);
            }
        }
        got.map(|c| (c.pa, c.moved))
    }

    /// A contiguous block: its VA in the contiguous window, or 0.
    fn contig_alloc(
        &mut self,
        size: u32,
        lo: u32,
        hi: u32,
        align: u32,
        protect: u32,
        ra: u32,
    ) -> u32 {
        let ram = self.gm.ram_size();
        // More than the console has: the failure the sizes below end in (and
        // past 0xFFFFF000 the rounding wrapped to nothing, which "succeeded").
        let size = if size > ram { ram + PAGE_SIZE } else { align_up(size.max(1), PAGE_SIZE) };
        let req = ContigRequest { len: size, lo, hi, align };
        let fits = self.mm.contig.len() < MAX_CONTIG && size <= ram;
        let in_slack = if fits { self.mm.slack_window(size, lo, hi, align) } else { None };
        let mut got = match in_slack {
            Some((pa, _)) => Some((pa, 0)),
            None => self.contig_place(req),
        };
        // No window: the kernel pool's pages with nothing left in them go
        // back to RAM, and the search runs again. Armed and Dangerous asks
        // for 40 MiB as its second level loads, with 49.4 MiB free: its
        // first level's blocks had filled RAM down to 0x24cd000, the pool
        // grew by a 64 KiB chunk just under them, and once they were freed
        // the chunk, empty, stood in every window of that size: the call
        // failed and the title showed its dirty disc screen.
        if got.is_none() && fits && self.pool_give_back() != 0 {
            got = self.contig_place(req);
        }
        if let Some((pa, moved)) = got
            && moved != 0
        {
            self.mm.moves += 1;
            let n = self.mm.moves;
            if n <= 8 || n.is_power_of_two() {
                log::info!(target: "kernel",
                    "mm: contiguous allocation of {} KiB at {:08x}: {} pages relocated (#{})",
                    size >> 10, pa, moved, n);
            }
        }
        let Some((pa, _)) = got.filter(|_| self.mm.contig.len() < MAX_CONTIG) else {
            // A failure must be as quick as the kernel's: titles size their
            // heap by asking for less and less (The Warriors: 46.5 MiB down
            // to 38.5 in 4 KiB steps after its 18.5 MiB block, 2033
            // failures). The first 4 failures since a success, then every
            // power of two, are reported: a run's own failure later (Amped
            // 2's 1.4 MiB at a level's load, after ~2500 sizing its heap at
            // boot) is reported whole.
            self.mm.failed += 1;
            let n = self.mm.failed;
            if n <= 4 || n.is_power_of_two() {
                log::warn!(target: "kernel",
                    "mm: contiguous allocation of {} KiB failed (#{}; free {} KiB:{})",
                    size >> 10, n, self.gm.phys.free_bytes() >> 10, self.gm.phys.free_runs_text());
                self.log_out_of_memory("contiguous allocation failed");
            }
            return 0;
        };
        self.mm.failed = 0;
        let va = CONTIG_WINDOW | pa;
        self.gm.kfill(va, 0, size);
        // An aligned block below another leaves the pages its alignment
        // skipped free between them, and virtual memory takes the highest
        // free pages: the block takes them too. On the console the blocks
        // pack: what runs past one lands in the next. Project Gotham
        // Racing's first boot decompresses a 640x512 loading bitmap into its
        // 640x480 back buffer, 80 KiB past the end; in push-buffer mode
        // D3D's frame buffers (16 KiB aligned) sat 8 KiB below its push
        // buffer, the title's heap took those pages, and the front end died
        // on a null buffer at car select.
        let mut slack = in_slack.map_or(0, |(_, rest)| rest); // (another block's slack: claimed already)
        let top = pa + size;
        while in_slack.is_none()
            && align > PAGE_SIZE
            && slack + PAGE_SIZE < align
            && top + slack < ram
            && self.gm.phys.claim(top + slack, PAGE_SIZE)
        {
            slack += PAGE_SIZE;
        }
        self.mm.contig.push(Block { va, size, ra, slack, protect });
        va
    }

    /// Frees the contiguous block at `va`; `ra` is who asked.
    fn contig_free(&mut self, va: u32, ra: u32) {
        let Some(i) = self.mm.contig_at(va) else {
            // a free that matches no block keeps its memory
            if va != 0 && self.mm.free_logged < 16 {
                self.mm.free_logged += 1;
                log::warn!(target: "kernel", "contiguous free of {va:08x} from {ra:08x}: no such block");
            }
            return;
        };
        let c = self.mm.contig[i];
        let len = c.size + c.slack;
        self.gm.invalidate_code(va, len);
        // A block in the slack of the one below it: its pages back in that
        // slack (still claimed, slack_window), else free.
        let below = self.mm.contig.iter().enumerate().rposition(|(j, b)| {
            j != i && b.slack != 0 && b.va.wrapping_add(b.size).wrapping_add(b.slack) == va
        });
        match below {
            Some(j) => self.mm.contig[j].slack += len,
            None => self.gm.free_phys(va & !CONTIG_WINDOW, len),
        }
        self.mm.contig.swap_remove(i);
    }

    /// Out-of-memory report, at debug level: what holds the small used runs
    /// between free runs (which keep the free memory from being one run),
    /// all of RAM in runs of one kind, and, once, who holds the RAM.
    fn log_out_of_memory(&mut self, why: &str) {
        if !log::log_enabled!(target: "kernel", log::Level::Debug) {
            return;
        }
        let pages = self.gm.phys.pages();
        // per page: the contiguous block it belongs to + 1 (slack included)
        let mut owner = vec![0u32; pages as usize];
        for (i, c) in self.mm.contig.iter().enumerate() {
            let p = (c.va & !CONTIG_WINDOW) / PAGE_SIZE;
            let n = (c.size + c.slack) / PAGE_SIZE;
            for o in owner.iter_mut().skip(p as usize).take(n as usize) {
                *o = i as u32 + 1;
            }
        }
        // What a used page is to compaction: free '.', a contiguous block's
        // 'C', without a user mapping 'K' (the kernel's, the GPU's instance
        // memory), pinned 'P', virtual memory that can move 'v'.
        let kind = |k: &Kernel, p: u32| -> char {
            let pa = p * PAGE_SIZE;
            if !k.gm.phys.is_used(pa) {
                '.'
            } else if owner[p as usize] != 0 {
                'C'
            } else if k.gm.space.va_of(pa).is_none() {
                'K'
            } else if k.gm.phys.pin_state(pa).is_pinned() {
                'P'
            } else {
                'v'
            }
        };
        let region_note = |k: &Kernel, va: u32| match k.mm.region_at(va) {
            Some(r) if k.mm.regions[r].image => " image",
            Some(_) => "",
            None => " (no region)",
        };

        let mut line = String::from("mm:   between free runs:");
        let mut p = 1;
        while p < pages && line.len() < 600 {
            if kind(self, p) == '.' || kind(self, p - 1) != '.' {
                p += 1;
                continue;
            }
            let mut q = p;
            while q < pages && kind(self, q) != '.' {
                q += 1;
            }
            if q < pages && q - p <= 64 {
                let pa = p * PAGE_SIZE;
                let _ = write!(line, " {pa:x}+{}K=", (q - p) * (PAGE_SIZE >> 10));
                let _ = match (owner[p as usize], self.gm.space.va_of(pa)) {
                    (0, None) => write!(line, "kernel"),
                    (0, Some(va)) => write!(
                        line,
                        "va {va:08x}{}{}",
                        region_note(self, va),
                        if self.gm.phys.pin_state(pa).is_pinned() { " pinned" } else { "" }
                    ),
                    (i, _) => {
                        let c = &self.mm.contig[i as usize - 1];
                        write!(line, "contiguous {:08x}+{}K", c.va, c.size >> 10)
                    }
                };
            }
            p = q;
        }
        log::debug!(target: "kernel", "{line}");

        let mut line = String::from("mm:   map:");
        let mut p = 0;
        while p < pages {
            if line.len() > 2400 {
                line.push_str(" ...");
                break;
            }
            let kd = kind(self, p);
            let block = owner[p as usize];
            let holder = |b: u32| self.mm.contig[b as usize - 1].ra;
            let mut q = p + 1;
            let mut blocks = 1;
            let mut prev = block;
            while q < pages
                && kind(self, q) == kd
                && (kd != 'C' || holder(owner[q as usize]) == holder(block))
            {
                blocks += u32::from(kd == 'C' && owner[q as usize] != prev);
                prev = owner[q as usize];
                q += 1;
            }
            let _ = write!(line, " {:x}+{}K=", p * PAGE_SIZE, (q - p) * (PAGE_SIZE >> 10));
            let _ = match kd {
                'C' => {
                    let c = &self.mm.contig[block as usize - 1];
                    if blocks > 1 {
                        write!(line, "C{:08x}.. ({blocks} blocks) by {:08x}", c.va, c.ra)
                    } else {
                        write!(line, "C{:08x} by {:08x}", c.va, c.ra)
                    }
                }
                'P' => {
                    let va = self.gm.space.va_of(p * PAGE_SIZE).unwrap_or(0);
                    write!(line, "P va {va:08x}{}", region_note(self, va))
                }
                _ => write!(line, "{kd}"),
            };
            p = q;
        }
        log::debug!(target: "kernel", "{line}");

        // Who holds the RAM, once: the contiguous blocks summed by caller
        // and the regions with the most committed.
        if std::mem::replace(&mut self.mm.holders_logged, true) {
            return;
        }
        let mut by_caller: Vec<(u32, u32, u32)> = Vec::new(); // ra, bytes, blocks
        for c in &self.mm.contig {
            match by_caller.iter_mut().find(|h| h.0 == c.ra) {
                Some(h) => {
                    h.1 += c.size;
                    h.2 += 1;
                }
                None => by_caller.push((c.ra, c.size, 1)),
            }
        }
        by_caller.sort_by_key(|h| std::cmp::Reverse(h.1));
        let mut line = format!("{why}: contiguous by caller:");
        for (ra, bytes, blocks) in by_caller.iter().take(12) {
            let _ = write!(line, " {ra:08x} {}K/{blocks}", bytes >> 10);
        }
        log::debug!(target: "kernel", "{line}");
        let mut by_committed: Vec<(u32, &Region)> =
            self.mm.regions.iter().map(|r| (self.committed(r), r)).filter(|c| c.0 != 0).collect();
        by_committed.sort_by_key(|c| std::cmp::Reverse(c.0));
        let mut line = format!("{why}: regions by committed (of {}):", self.mm.regions.len());
        for (c, r) in by_committed.iter().take(12) {
            let _ = write!(
                line,
                " {:08x}+{:x}:{}K{}",
                r.base,
                r.size,
                c >> 10,
                if r.image { "(image)" } else { "" }
            );
        }
        log::debug!(target: "kernel", "{line}");
    }

    // ----------------------------------------- memory kept for the next title

    /// The kept ranges' physical addresses and sizes.
    pub(crate) fn mm_kept(&self) -> Vec<(u32, u32)> {
        self.mm.kept.iter().map(|&(va, len)| (va & !CONTIG_WINDOW, len)).collect()
    }

    /// The range at `va` is not kept any more (the launch data page, which
    /// goes to the next title by its own way).
    pub(crate) fn mm_keep_drop(&mut self, va: u32) {
        if let Some(i) = self.mm.kept.iter().position(|k| k.0 == va) {
            self.mm.kept.swap_remove(i);
        }
    }

    /// The kept ranges' physical addresses and bytes, for the next boot. The
    /// machine has the GPU's images over them written back first. A range
    /// outside the contiguous window is left out; one that leaves RAM drops
    /// them all.
    pub(crate) fn mm_keep_collect(&self) -> Vec<(u32, Vec<u8>)> {
        let ram = self.gm.ram_size();
        let mut out = Vec::new();
        let mut bytes = 0u32;
        for &(va, len) in &self.mm.kept {
            if va & CONTIG_WINDOW == 0 {
                log::info!(target: "kernel", "kept memory {va:08x}+{len}: not contiguous memory, not kept");
                continue;
            }
            let pa = va & !CONTIG_WINDOW;
            let data = (pa < ram && len <= ram - pa).then(|| self.gm.read_vec(va, len).ok());
            let Some(data) = data.flatten() else {
                log::warn!(target: "kernel", "kept memory: {} range(s): not written", self.mm.kept.len());
                return Vec::new();
            };
            bytes = bytes.saturating_add(len);
            out.push((pa, data));
        }
        if !out.is_empty() {
            log::info!(target: "kernel", "kept memory: {} range(s), {} KiB for the next title",
                out.len(), bytes >> 10);
        }
        out
    }

    /// Puts the last title's kept ranges back where they were, allocated,
    /// before the title loads.
    pub(crate) fn mm_keep_restore(&mut self, kept: &[(u32, Vec<u8>)]) {
        let ram = self.gm.ram_size();
        for (pa, data) in kept.iter().take(MAX_KEPT) {
            let pa = *pa;
            let va = CONTIG_WINDOW | pa;
            let Ok(len) = u32::try_from(data.len()) else {
                continue;
            };
            let fits = pa < ram && len <= ram && self.mm.contig.len() < MAX_CONTIG && {
                let size = align_up(len.max(1), PAGE_SIZE);
                size <= ram - pa && self.gm.phys.claim(pa, size)
            };
            if !fits {
                log::warn!(target: "kernel", "kept memory {va:08x}+{len}: not restored");
                continue;
            }
            let size = align_up(len.max(1), PAGE_SIZE);
            self.gm.space.pin_still_mapped(&mut self.gm.phys, pa, size);
            self.gm.kfill(va, 0, size);
            self.gm.kwrite(va, data);
            self.mm.contig.push(Block { va, size, ra: 0, slack: 0, protect: PAGE_READWRITE });
            self.mm.kept.push((va, len));
            log::info!(target: "kernel", "kept memory {va:08x}+{len} restored");
        }
    }
}

fn ok(v: u32) -> KResult<u64> {
    Ok(u64::from(v))
}

/// NtAllocateVirtualMemory(BaseAddress, ZeroBits, RegionSize, AllocationType, Protect)
fn nt_allocate_virtual_memory(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let mut base = k.gm.r32(a[0])?;
    let mut size = k.gm.r32(a[2])?;
    let ty = a[3];
    if size == 0 || ty & (MEM_COMMIT | MEM_RESERVE) == 0 {
        return ok(STATUS_INVALID_PARAMETER);
    }
    if size > USER_HI - USER_LO
        || (base != 0 && u64::from(base) + u64::from(size) > u64::from(USER_HI))
    {
        // past the space there is: the sums below wrapped to a region of no
        // pages and the call "succeeded"
        return ok(if base != 0 { STATUS_CONFLICTING_ADDRESSES } else { STATUS_NO_MEMORY });
    }

    // A commit at no address reserves the range too (VirtualAlloc(NULL, n, MEM_COMMIT)).
    let mut reserved = false;
    if ty & MEM_RESERVE != 0 || base == 0 {
        if base != 0 {
            let end = align_up(base + size, PAGE_SIZE);
            base &= !(VA_GRAIN - 1);
            size = end - base;
            if base < USER_LO || end > USER_HI || !k.mm.range_free(base, size) {
                return ok(STATUS_CONFLICTING_ADDRESSES);
            }
        } else {
            size = align_up(size, PAGE_SIZE);
            base = k.find_free(size, ty & MEM_TOP_DOWN != 0);
            if base == 0 {
                return ok(STATUS_NO_MEMORY);
            }
        }
        if !k.mm.add_region(base, size, a[4], false) {
            return ok(STATUS_NO_MEMORY);
        }
        reserved = true;
    } else {
        let end = align_up(base + size, PAGE_SIZE);
        base &= !(PAGE_SIZE - 1);
        size = end - base;
        let r = k.mm.region_at(base).map(|i| k.mm.regions[i]);
        if r.is_none_or(|r| end > r.base + r.size) {
            log::warn!(target: "kernel", "commit {base:08x}+{size:x}: not inside a reservation ({} {:08x}+{:x})",
                if r.is_some() { "region" } else { "no region" },
                r.map_or(0, |r| r.base), r.map_or(0, |r| r.size));
            return ok(STATUS_NOT_COMMITTED); // not reserved
        }
    }
    if ty & MEM_COMMIT != 0 {
        if !k.commit(base, size, ty & MEM_NOZERO == 0) {
            log::warn!(target: "kernel", "commit {base:08x}+{size:x}: out of physical memory ({} KiB free)",
                k.gm.phys.free_bytes() >> 10);
            if reserved {
                k.mm.regions.pop(); // the reservation this call made goes with it
            }
            return ok(STATUS_NO_MEMORY);
        }
        // The pages get the protection asked for, those committed already as
        // well: Rainbow Six: Lockdown commits a page of its swapped memory
        // read-only, and again read-write as the title's store faults (the
        // page written).
        k.guest_ro(base, size, prot_read_only(a[4]));
    }
    k.gm.w32(a[0], base)?;
    k.gm.w32(a[2], size)?;
    // Only the big ones: a title's heap commits and decommits pages by the
    // thousand a second while it loads (the counts are in the stats line).
    if ty & MEM_COMMIT != 0 && size >= 0x4_0000 {
        log::debug!(target: "kernel", "allocate {base:08x}+{size:x} type {ty:x} protect {:x} | frames{}",
            a[4], k.frame_chain());
    }
    ok(STATUS_SUCCESS)
}

/// NtFreeVirtualMemory(BaseAddress, RegionSize, FreeType)
fn nt_free_virtual_memory(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let mut base = k.gm.r32(a[0])?;
    let mut size = k.gm.r32(a[1])?;
    let Some(i) = k.mm.region_at(base).filter(|&i| !k.mm.regions[i].image) else {
        return ok(STATUS_MEMORY_NOT_ALLOCATED);
    };
    let r = k.mm.regions[i];
    let r_end = r.base + r.size;
    if a[2] & MEM_RELEASE != 0 {
        // Size 0 frees the whole reservation, from its base only; a size
        // frees those pages of one reservation, trimming or splitting it
        // (Blood Wake keeps the first page of a 1.9 MiB one and releases the
        // rest).
        let end = if size != 0 { align_up(base.wrapping_add(size), PAGE_SIZE) } else { r_end };
        base &= !(PAGE_SIZE - 1);
        if size == 0 && base != r.base {
            return ok(STATUS_FREE_VM_NOT_AT_BASE);
        }
        if end > r_end || end < base {
            return ok(STATUS_UNABLE_TO_FREE_VM);
        }
        if base > r.base && end < r_end && !k.mm.add_region(end, r_end - end, r.protect, false) {
            return ok(STATUS_NO_MEMORY);
        }
        size = end - base;
        k.decommit(base, size);
        if base > r.base {
            k.mm.regions[i].size = base - r.base;
        } else if end < r_end {
            k.mm.regions[i].base = end;
            k.mm.regions[i].size = r_end - end;
        } else {
            k.mm.regions.swap_remove(i);
        }
    } else if a[2] & MEM_DECOMMIT != 0 {
        // (the reference decommits past the reservation's end if asked to:
        // here the range ends with the reservation)
        let end = if size != 0 { align_up(base.wrapping_add(size), PAGE_SIZE) } else { r_end };
        base &= !(PAGE_SIZE - 1);
        size = end.clamp(base, r_end) - base;
        k.decommit(base, size);
    } else {
        return ok(STATUS_INVALID_PARAMETER);
    }
    k.gm.w32(a[0], base)?;
    k.gm.w32(a[1], size)?;
    ok(STATUS_SUCCESS)
}

/// NtProtectVirtualMemory(BaseAddress, RegionSize, NewProtect, OldProtect).
/// The protection is recorded for NtQueryVirtualMemory and the old one
/// returned; outside the title's image a read-only one holds on the pages (a
/// store faults to the title), the image's are left writable (patches and
/// cheats write there); the range rounds out to pages as the kernel's does
/// (JSRF protects its own pages). The code translated from the range goes: a
/// title patching its code makes it writable, writes, and sets the
/// protection back, and the translations of the old bytes would run on.
/// Rainbow Six 3 points the C runtime's malloc, calloc and free at its own
/// allocator that way at boot; malloc had run already, its old translation
/// kept handing out the system heap's blocks, its free sent them to the
/// allocator, and the first mission's load died on a block it never made.
fn nt_protect_virtual_memory(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let mut base = k.gm.r32(a[0])?;
    let end = align_up(base.wrapping_add(k.gm.r32(a[1])?), PAGE_SIZE);
    k.gm.r32(a[3])?; // the old protection goes there, once the new one is set
    base &= !(PAGE_SIZE - 1);
    let size = end.wrapping_sub(base);
    if size == 0 {
        return ok(STATUS_INVALID_PARAMETER);
    }
    if !k.gm.mapped_range(base, size) {
        return ok(STATUS_NOT_COMMITTED);
    }
    let r = k.mm.region_at(base);
    let old = r.map_or(PAGE_READWRITE, |i| k.mm.regions[i].protect);
    if let Some(i) = r {
        let r = &mut k.mm.regions[i];
        if base == r.base && size == r.size {
            r.protect = a[2];
        }
        if !r.image {
            k.guest_ro(base, size, prot_read_only(a[2]));
        }
    }
    k.gm.w32(a[0], base)?;
    k.gm.w32(a[1], size)?;
    k.gm.w32(a[3], old)?;
    k.gm.invalidate_code(base, size);
    // JSRF flips a region's protection every frame
    if k.mm.protect_logged < 16 {
        k.mm.protect_logged += 1;
        log::debug!(target: "kernel", "protect {base:08x}+{size:x} {:x} (was {old:x}){}", a[2],
            if k.mm.protect_logged == 16 { " (no more protect logs)" } else { "" });
    }
    ok(STATUS_SUCCESS)
}

/// NtQueryVirtualMemory(BaseAddress, MemoryInformation) -> MEMORY_BASIC_INFORMATION
/// { BaseAddress, AllocationBase, AllocationProtect, RegionSize, State, Protect, Type }
fn nt_query_virtual_memory(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let va = a[0] & !(PAGE_SIZE - 1);
    if va >= USER_HI {
        // Past the last address a region can start at: no answer, as the
        // kernel gives none. Each page up there was told free and the call
        // succeeded: a walk of the address space by RegionSize, which ends
        // at this failure on a console, went on to 4 GiB and round again.
        return ok(STATUS_INVALID_PARAMETER);
    }
    let mut info = [va, 0, 0, 0, MEM_FREE, PAGE_NOACCESS, 0];
    match k.mm.region_at(va) {
        None => {
            let end =
                k.mm.regions.iter().map(|r| r.base).filter(|&b| b > va).fold(USER_HI, u32::min);
            info[3] = end - va;
        }
        Some(i) => {
            let r = k.mm.regions[i];
            let (committed, run) = k.gm.space.mapped_run(va, r.base + r.size - va);
            info[1] = r.base;
            info[2] = r.protect;
            info[3] = run;
            info[4] = if committed { MEM_COMMIT } else { MEM_RESERVE };
            info[5] = if committed { r.protect } else { 0 };
            info[6] = if r.image { MEM_IMAGE } else { MEM_PRIVATE };
        }
    }
    let bytes: Vec<u8> = info.iter().flat_map(|v| v.to_le_bytes()).collect();
    k.gm.write(a[1], &bytes)?;
    ok(STATUS_SUCCESS)
}

/// MmAllocateContiguousMemory(NumberOfBytes): read-write (the kernel's Ex
/// with PAGE_READWRITE)
fn mm_allocate_contiguous_memory(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let ra = k.caller();
    ok(k.contig_alloc(a[0], 0, 0, PAGE_SIZE, PAGE_READWRITE, ra))
}

/// MmAllocateContiguousMemoryEx(NumberOfBytes, LowestAcceptableAddress,
///   HighestAcceptableAddress, Alignment, ProtectionType). The bounds are page
/// frames, as the kernel's: the lowest address's page rounded up, the highest
/// address's page the last one the block may take (all of it, past the
/// address too). Fatal Frame II asks for 0x184E400 bytes in exactly
/// 0x00D0B000..0x025593FF, its texture pool, and uses that memory whether it
/// gets it or not: bounded by the address, the block's last page didn't fit,
/// the call failed, and the title's disc loads went to pages the heap had
/// (a heap block's header overwritten: a crash at boot; or a load's request:
/// a hang before the title screen).
fn mm_allocate_contiguous_memory_ex(
    k: &mut Kernel,
    _p: &mut dyn Platform,
    a: &Args,
) -> KResult<u64> {
    let hi = if a[2] >= k.gm.ram_size() - 1 { 0 } else { (a[2] | (PAGE_SIZE - 1)) + 1 };
    let ra = k.caller();
    ok(k.contig_alloc(a[0], a[1], hi, if a[3] != 0 { a[3] } else { PAGE_SIZE }, a[4], ra))
}

/// MmFreeContiguousMemory(BaseAddress)
fn mm_free_contiguous_memory(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let ra = k.caller();
    k.contig_free(a[0], ra);
    Ok(0)
}

/// MmAllocateSystemMemory(NumberOfBytes, Protect)
fn mm_allocate_system_memory(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let ra = k.caller();
    ok(k.contig_alloc(a[0], 0, 0, PAGE_SIZE, a[1], ra))
}

/// MmFreeSystemMemory(BaseAddress, NumberOfBytes) -> pages freed
fn mm_free_system_memory(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let pages = k.mm.contig_at(a[0]).map_or(0, |i| k.mm.contig[i].size / PAGE_SIZE);
    let ra = k.caller();
    k.contig_free(a[0], ra);
    ok(pages)
}

/// MmQueryAllocationSize(BaseAddress)
fn mm_query_allocation_size(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    if let Some(i) = k.mm.contig_at(a[0]) {
        return ok(k.mm.contig[i].size);
    }
    ok(k.mm.region_at(a[0]).map_or(0, |i| k.mm.regions[i].size))
}

/// MmGetPhysicalAddress(BaseAddress)
fn mm_get_physical_address(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let Some(pa) = k.gm.phys_of(a[0]) else {
        return Ok(0);
    };
    k.gm.phys.pin(pa, true, 0); // the title may keep it: the page stays put
    ok(pa)
}

/// MmIsAddressValid(VirtualAddress)
fn mm_is_address_valid(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    ok(u32::from(k.gm.mapped(a[0])))
}

/// MmLockUnlockBufferPages(BaseAddress, NumberOfBytes, UnlockPages): locked
/// pages (DMA) are never relocated.
fn mm_lock_unlock_buffer_pages(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    if a[1] == 0 {
        return Ok(0);
    }
    let end = (u64::from(a[0]) + u64::from(a[1])).min(1 << 32);
    let mut va = u64::from(a[0] & !(PAGE_SIZE - 1));
    while va < end {
        if let Some(pa) = k.gm.phys_of(va as u32) {
            k.gm.phys.pin(pa, false, if a[2] & 0xFF != 0 { -1 } else { 1 });
        }
        va += u64::from(PAGE_SIZE);
    }
    Ok(0)
}

/// MmLockUnlockPhysicalPage(PhysicalAddress, UnlockPage)
fn mm_lock_unlock_physical_page(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    k.gm.phys.pin(a[0], false, if a[1] & 0xFF != 0 { -1 } else { 1 });
    Ok(0)
}

/// MmUnmapIoSpace(BaseAddress, NumberOfBytes): nothing to do.
fn mm_unmap_io_space(_k: &mut Kernel, _p: &mut dyn Platform, _a: &Args) -> KResult<u64> {
    Ok(0)
}

/// MmPersistContiguousMemory(BaseAddress, NumberOfBytes, Persist): contiguous
/// memory kept across a quick reboot. D3D's PersistDisplay keeps the frame on
/// screen for the next title (AvSetSavedDataAddress), which draws its loading
/// screen over it (Splinter Cell Chaos Theory's missions). The machine boots
/// afresh for the next XBE: at the reboot it takes the kept ranges
/// ([`Kernel::mm_keep_collect`]) and the next boot puts them back where they
/// were, allocated, before the title loads ([`Kernel::mm_keep_restore`]).
fn mm_persist_contiguous_memory(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let (va, len) = (a[0], a[1]);
    let keep = a[2] & 0xFF != 0;
    let at = k.mm.kept.iter().position(|r| r.0 == va);
    match (keep, at) {
        (true, Some(i)) => k.mm.kept[i] = (va, len),
        (true, None) if k.mm.kept.len() < MAX_KEPT => k.mm.kept.push((va, len)),
        (false, Some(i)) => {
            k.mm.kept.swap_remove(i);
        }
        _ => {}
    }
    log::info!(target: "kernel", "MmPersistContiguousMemory {va:08x}+{len}: {}",
        if keep { "kept" } else { "released" });
    Ok(0)
}

/// MmQueryAddressProtect(VirtualAddress): the page's protection as it was
/// allocated or set, cache bits included. Ultimate Spider-Man's free() tells
/// its write-combined contiguous blocks (PAGE_READWRITE | PAGE_WRITECOMBINE)
/// by them: told PAGE_READWRITE, it freed one as a heap block, through the
/// dword before it (a fault the title doesn't handle, at boot).
fn mm_query_address_protect(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    if !k.gm.mapped(a[0]) {
        return Ok(0);
    }
    let p = match k.mm.contig_holding(a[0]) {
        Some(i) => k.mm.contig[i].protect,
        None => k.mm.region_at(a[0]).map_or(0, |i| k.mm.regions[i].protect),
    };
    ok(if p != 0 { p } else { PAGE_READWRITE })
}

/// MmSetAddressProtect(BaseAddress, NumberOfBytes, NewProtect): recorded for
/// MmQueryAddressProtect, for the contiguous block it falls in (contiguous
/// memory has no access checks, as in the reference)
fn mm_set_address_protect(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    if let Some(i) = k.mm.contig_holding(a[0]) {
        k.mm.contig[i].protect = a[2];
    }
    Ok(0)
}

/// MmClaimGpuInstanceMemory(NumberOfBytes, NumberOfPaddingBytes): the GPU's
/// instance memory sits at the top of RAM below a 64 KiB pad; the result is
/// the end of that block.
fn mm_claim_gpu_instance_memory(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    if a[1] != 0 {
        k.gm.w32(a[1], GPU_PAD)?;
    }
    if k.mm.gpu_instance_end == 0 {
        k.mm.gpu_instance_end = k.gm.ram_size() - GPU_PAD;
        // kept from boot (GPU_TOP): only a larger block's rest
        let kept = GPU_TOP - GPU_PAD;
        if a[0] > kept && a[0] <= k.mm.gpu_instance_end {
            let size = align_up(a[0], PAGE_SIZE);
            k.gm.phys.claim(k.mm.gpu_instance_end - size, size - kept);
        }
    }
    ok(CONTIG_WINDOW | k.mm.gpu_instance_end)
}

/// MmMapIoSpace(PhysicalAddress, NumberOfBytes, ProtectType): device windows
/// keep their addresses (the machine maps them); RAM is reached through the
/// contiguous window.
fn mm_map_io_space(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    ok(if a[0] < k.gm.ram_size() { CONTIG_WINDOW | a[0] } else { a[0] })
}

/// MmCreateKernelStack(NumberOfBytes, DebuggerThread) -> top of the new stack
fn mm_create_kernel_stack(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    if a[0] > k.gm.ram_size() {
        return Ok(0);
    }
    let size = align_up(a[0], PAGE_SIZE);
    ok(k.stack_alloc(size).map_or(0, |base| base + size))
}

/// MmDeleteKernelStack(StackBase, StackLimit)
fn mm_delete_kernel_stack(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    if a[0] > a[1] {
        k.decommit(a[1], a[0] - a[1]);
        k.stack_free(a[1], a[0] - a[1]);
    }
    Ok(0)
}

/// MmQueryStatistics(MemoryStatistics): MM_STATISTICS { Length (0x24), TotalPhysicalPages,
///   AvailablePages, VirtualMemoryBytesCommitted, VirtualMemoryBytesReserved,
///   CachePagesCommitted, PoolPagesCommitted, StackPagesCommitted, ImagePagesCommitted }
fn mm_query_statistics(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    if k.gm.r32(a[0])? != 0x24 {
        return ok(STATUS_INVALID_PARAMETER);
    }
    let (mut committed, mut reserved, mut image) = (0u32, 0u32, 0u32);
    for r in &k.mm.regions {
        reserved = reserved.wrapping_add(r.size);
        if r.image {
            image += k.committed(r) / PAGE_SIZE;
        } else {
            committed += k.committed(r);
        }
    }
    let s = [
        0x24,
        k.gm.phys.pages(),
        k.gm.phys.free_pages(),
        committed,
        reserved,
        0,
        k.pool.len / PAGE_SIZE,
        0,
        image,
    ];
    let bytes: Vec<u8> = s.iter().flat_map(|v| v.to_le_bytes()).collect();
    k.gm.write(a[0], &bytes)?;
    ok(STATUS_SUCCESS)
}

pub(crate) fn register(k: &mut Kernel) {
    k.imp("NtAllocateVirtualMemory", nt_allocate_virtual_memory);
    k.imp("NtFreeVirtualMemory", nt_free_virtual_memory);
    k.imp("NtQueryVirtualMemory", nt_query_virtual_memory);
    k.imp("NtProtectVirtualMemory", nt_protect_virtual_memory);
    k.imp("MmAllocateContiguousMemory", mm_allocate_contiguous_memory);
    k.imp("MmAllocateContiguousMemoryEx", mm_allocate_contiguous_memory_ex);
    k.imp("MmFreeContiguousMemory", mm_free_contiguous_memory);
    k.imp("MmAllocateSystemMemory", mm_allocate_system_memory);
    k.imp("MmFreeSystemMemory", mm_free_system_memory);
    k.imp("MmQueryAllocationSize", mm_query_allocation_size);
    k.imp("MmGetPhysicalAddress", mm_get_physical_address);
    k.imp("MmIsAddressValid", mm_is_address_valid);
    k.imp("MmPersistContiguousMemory", mm_persist_contiguous_memory);
    k.imp("MmSetAddressProtect", mm_set_address_protect);
    k.imp("MmLockUnlockBufferPages", mm_lock_unlock_buffer_pages);
    k.imp("MmLockUnlockPhysicalPage", mm_lock_unlock_physical_page);
    k.imp("MmUnmapIoSpace", mm_unmap_io_space);
    k.imp("MmQueryAddressProtect", mm_query_address_protect);
    k.imp("MmClaimGpuInstanceMemory", mm_claim_gpu_instance_memory);
    k.imp("MmMapIoSpace", mm_map_io_space);
    k.imp("MmCreateKernelStack", mm_create_kernel_stack);
    k.imp("MmDeleteKernelStack", mm_delete_kernel_stack);
    k.imp("MmQueryStatistics", mm_query_statistics);
}

xb_types::state_fields!(Region { base, size, protect, image });
xb_types::state_fields!(Block { va, size, ra, slack, protect });
xb_types::state_fields!(State {
    commits,
    decommits,
    regions,
    contig,
    kept,
    gpu_instance_end,
    failed,
    moves
});

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{TestKernel, kernel};

    const PAGE_READONLY: u32 = 0x02;
    /// A scratch page for the calls' in/out arguments.
    const IO: u32 = 0x00F0_0000;

    /// A test kernel with the scratch page mapped (so that the helpers below
    /// take no RAM between two readings of the free count).
    fn setup() -> TestKernel {
        let mut t = kernel();
        assert!(t.k.gm.alloc_at(IO, PAGE_SIZE));
        t
    }

    /// NtAllocateVirtualMemory through the dispatcher: status, base, size.
    fn valloc(t: &mut TestKernel, base: u32, size: u32, ty: u32, protect: u32) -> (u32, u32, u32) {
        let io = IO;
        t.k.gm.w32(io, base).unwrap();
        t.k.gm.w32(io + 4, size).unwrap();
        let st = t.call32("NtAllocateVirtualMemory", &[io, 0, io + 4, ty, protect]);
        (st, t.k.gm.r32(io).unwrap(), t.k.gm.r32(io + 4).unwrap())
    }

    fn vfree(t: &mut TestKernel, base: u32, size: u32, ty: u32) -> (u32, u32, u32) {
        let io = IO;
        t.k.gm.w32(io, base).unwrap();
        t.k.gm.w32(io + 4, size).unwrap();
        let st = t.call32("NtFreeVirtualMemory", &[io, io + 4, ty]);
        (st, t.k.gm.r32(io).unwrap(), t.k.gm.r32(io + 4).unwrap())
    }

    /// MEMORY_BASIC_INFORMATION for `va`, or the failure status.
    fn query(t: &mut TestKernel, va: u32) -> Result<[u32; 7], u32> {
        let mbi = IO + 0x40;
        let st = t.call32("NtQueryVirtualMemory", &[va, mbi]);
        if st != STATUS_SUCCESS {
            return Err(st);
        }
        Ok(std::array::from_fn(|i| t.k.gm.r32(mbi + 4 * i as u32).unwrap()))
    }

    fn stats(t: &mut TestKernel) -> [u32; 9] {
        let s = IO + 0x80;
        t.k.gm.w32(s, 0x24).unwrap();
        assert_eq!(t.call32("MmQueryStatistics", &[s]), STATUS_SUCCESS);
        std::array::from_fn(|i| t.k.gm.r32(s + 4 * i as u32).unwrap())
    }

    #[test]
    fn a_reservation_is_reported_reserved_then_committed_then_free() {
        let mut t = setup();
        let (st, base, size) = valloc(&mut t, 0, 0x2_5000, MEM_RESERVE, PAGE_READWRITE);
        assert_eq!(st, STATUS_SUCCESS);
        assert_eq!(size, 0x2_5000);
        assert_eq!(base % VA_GRAIN, 0);
        assert!((USER_LO..USER_HI).contains(&base));
        let q = query(&mut t, base + 0x1234).unwrap();
        assert_eq!(
            q,
            [base + 0x1000, base, PAGE_READWRITE, size - 0x1000, MEM_RESERVE, 0, MEM_PRIVATE]
        );
        assert!(!t.k.gm.mapped(base));

        // commit two pages in the middle: three runs
        let free = t.k.gm.phys.free_pages();
        let (st, cb, cs) = valloc(&mut t, base + 0x2010, 0x1FF0, MEM_COMMIT, PAGE_READWRITE);
        assert_eq!((st, cb, cs), (STATUS_SUCCESS, base + 0x2000, 0x2000));
        assert_eq!(t.k.gm.phys.free_pages(), free - 2);
        assert_eq!(t.k.gm.r32(base + 0x2000), Ok(0));
        t.k.gm.w32(base + 0x3FFC, 0xDEAD_BEEF).unwrap();
        assert_eq!(query(&mut t, base).unwrap()[3..5], [0x2000, MEM_RESERVE]);
        let q = query(&mut t, base + 0x2000).unwrap();
        assert_eq!(q[3..6], [0x2000, MEM_COMMIT, PAGE_READWRITE]);
        assert_eq!(query(&mut t, base + 0x4000).unwrap()[3..5], [size - 0x4000, MEM_RESERVE]);

        // committing again keeps the contents; decommitting gives the pages back
        assert_eq!(
            valloc(&mut t, base + 0x3000, 0x1000, MEM_COMMIT, PAGE_READWRITE).0,
            STATUS_SUCCESS
        );
        assert_eq!(t.k.gm.r32(base + 0x3FFC), Ok(0xDEAD_BEEF));
        assert_eq!(
            vfree(&mut t, base + 0x2000, 0x2000, MEM_DECOMMIT),
            (STATUS_SUCCESS, base + 0x2000, 0x2000)
        );
        assert_eq!(t.k.gm.phys.free_pages(), free);
        assert_eq!(query(&mut t, base).unwrap()[3..5], [size, MEM_RESERVE]);
        assert_eq!((t.k.mm.commits, t.k.mm.decommits), (2, 2));

        // released from its base, the range is free up to the next region
        assert_eq!(vfree(&mut t, base + 0x1000, 0, MEM_RELEASE).0, STATUS_FREE_VM_NOT_AT_BASE);
        assert_eq!(vfree(&mut t, base, 0, MEM_RELEASE), (STATUS_SUCCESS, base, size));
        let q = query(&mut t, base).unwrap();
        assert_eq!(q[..3], [base, 0, 0]);
        assert_eq!(q[4..], [MEM_FREE, PAGE_NOACCESS, 0]);
        assert_eq!(vfree(&mut t, base, 0, MEM_RELEASE).0, STATUS_MEMORY_NOT_ALLOCATED);
    }

    #[test]
    fn a_commit_at_no_address_reserves_and_a_release_frees_its_pages() {
        let mut t = setup();
        let free = t.k.gm.phys.free_pages();
        let (st, base, size) = valloc(&mut t, 0, 0x3001, MEM_COMMIT, PAGE_READWRITE);
        assert_eq!((st, size), (STATUS_SUCCESS, 0x4000));
        assert_eq!(t.k.gm.phys.free_pages(), free - 4);
        assert!(t.k.gm.mapped_range(base, size));
        assert_eq!(t.call32("MmQueryAllocationSize", &[base]), size);
        assert_eq!(vfree(&mut t, base, 0, MEM_RELEASE).0, STATUS_SUCCESS);
        assert_eq!(t.k.gm.phys.free_pages(), free);
        assert!(!t.k.gm.mapped(base));
        assert_eq!(t.call32("MmQueryAllocationSize", &[base]), 0);
    }

    #[test]
    fn reservations_do_not_overlap_and_top_down_ones_come_from_the_top() {
        let mut t = setup();
        let (_, a, _) = valloc(&mut t, 0, 0x1000, MEM_RESERVE, PAGE_READWRITE);
        let (_, b, _) = valloc(&mut t, 0, 0x1000, MEM_RESERVE, PAGE_READWRITE);
        let (st, c, cs) = valloc(&mut t, 0, 0x1_8000, MEM_RESERVE | MEM_TOP_DOWN, PAGE_READWRITE);
        assert_eq!(st, STATUS_SUCCESS);
        assert_ne!(a, b);
        assert!(c > a && c > b);
        assert_eq!(c + align_up(cs, VA_GRAIN), USER_HI);
        // an address inside one conflicts; a free one is taken, rounded down
        assert_eq!(
            valloc(&mut t, a + 0x800, 0x1000, MEM_RESERVE, 4).0,
            STATUS_CONFLICTING_ADDRESSES
        );
        let (st, d, ds) = valloc(&mut t, 0x2000_1234, 0x1000, MEM_RESERVE, PAGE_READWRITE);
        assert_eq!((st, d, ds), (STATUS_SUCCESS, 0x2000_0000, 0x3000));
        // a commit outside any reservation, and nonsense, are refused
        assert_eq!(valloc(&mut t, 0x3000_0000, 0x1000, MEM_COMMIT, 4).0, STATUS_NOT_COMMITTED);
        assert_eq!(valloc(&mut t, 0, 0, MEM_COMMIT, 4).0, STATUS_INVALID_PARAMETER);
        assert_eq!(valloc(&mut t, 0, 0x1000, 0, 4).0, STATUS_INVALID_PARAMETER);
        assert_eq!(valloc(&mut t, 0, 0x8000_0000, MEM_RESERVE, 4).0, STATUS_NO_MEMORY);
        assert_eq!(
            valloc(&mut t, 0x7FFD_0000, 0x2_0000, MEM_RESERVE, 4).0,
            STATUS_CONFLICTING_ADDRESSES
        );
        assert_eq!(query(&mut t, USER_HI), Err(STATUS_INVALID_PARAMETER));
    }

    #[test]
    fn a_search_passes_over_the_trap_page_and_memory_mapped_without_a_region() {
        let mut t = setup();
        let scratch = t.alloc(0x1000);
        // fill the address space with reservations of 256 MiB: none may hold
        // the scratch page or reach the windows
        let mut got = Vec::new();
        loop {
            let (st, base, size) = valloc(&mut t, 0, 0x1000_0000, MEM_RESERVE, PAGE_READWRITE);
            if st != STATUS_SUCCESS {
                assert_eq!(st, STATUS_NO_MEMORY);
                break;
            }
            got.push((base, size));
        }
        assert!(!got.is_empty());
        for &(base, size) in &got {
            assert!(base >= USER_LO && base + size <= USER_HI);
            assert!(!(base..base + size).contains(&scratch));
            assert!(base + size <= TRAP_VA);
        }
    }

    #[test]
    fn a_commit_that_does_not_fit_leaves_nothing_behind() {
        let mut t = setup();
        let free = t.k.gm.phys.free_pages();
        let regions = t.k.mm.regions.len();
        let (st, ..) = valloc(&mut t, 0, 0x400_0000, MEM_COMMIT, PAGE_READWRITE);
        assert_eq!(st, STATUS_NO_MEMORY);
        assert_eq!(t.k.gm.phys.free_pages(), free);
        assert_eq!(t.k.mm.regions.len(), regions);
    }

    #[test]
    fn a_release_of_part_of_a_reservation_trims_or_splits_it() {
        let mut t = setup();
        let (_, base, _) = valloc(&mut t, 0, 0x1_0000, MEM_COMMIT, PAGE_READWRITE);
        let free = t.k.gm.phys.free_pages();
        // the middle goes: two reservations stay
        assert_eq!(vfree(&mut t, base + 0x4000, 0x2000, MEM_RELEASE).0, STATUS_SUCCESS);
        assert_eq!(t.k.gm.phys.free_pages(), free + 2);
        assert_eq!(query(&mut t, base).unwrap()[1..5], [base, PAGE_READWRITE, 0x4000, MEM_COMMIT]);
        assert_eq!(query(&mut t, base + 0x4000).unwrap()[3..5], [0x2000, MEM_FREE]);
        assert_eq!(
            query(&mut t, base + 0x6000).unwrap()[1..5],
            [base + 0x6000, PAGE_READWRITE, 0xA000, MEM_COMMIT]
        );
        // past its end: refused
        assert_eq!(vfree(&mut t, base, 0x5000, MEM_RELEASE).0, STATUS_UNABLE_TO_FREE_VM);
        // the head of the first goes: it starts later
        assert_eq!(vfree(&mut t, base, 0x1000, MEM_RELEASE).0, STATUS_SUCCESS);
        assert_eq!(query(&mut t, base + 0x1000).unwrap()[1], base + 0x1000);
        assert_eq!(t.call32("MmQueryAllocationSize", &[base + 0x1000]), 0x3000);
    }

    #[test]
    fn a_read_only_protection_holds_for_the_guest_and_is_reported() {
        let mut t = setup();
        let (_, base, size) = valloc(&mut t, 0, 0x3000, MEM_COMMIT, PAGE_READWRITE);
        assert!(t.k.gm.space.allows(base, true));
        let io = t.alloc(12);
        let protect = |t: &mut TestKernel, va: u32, len: u32, new: u32| {
            t.k.gm.w32(io, va).unwrap();
            t.k.gm.w32(io + 4, len).unwrap();
            let st = t.call32("NtProtectVirtualMemory", &[io, io + 4, new, io + 8]);
            (st, t.k.gm.r32(io).unwrap(), t.k.gm.r32(io + 4).unwrap(), t.k.gm.r32(io + 8).unwrap())
        };
        // one page: enforced there, the region's recorded protection unchanged
        let r = protect(&mut t, base + 0x1010, 0x20, PAGE_READONLY);
        assert_eq!(r, (STATUS_SUCCESS, base + 0x1000, 0x1000, PAGE_READWRITE));
        assert!(!t.k.gm.space.allows(base + 0x1000, true));
        assert!(t.k.gm.space.allows(base + 0x1000, false));
        assert!(t.k.gm.space.allows(base, true) && t.k.gm.space.allows(base + 0x2000, true));
        assert_eq!(query(&mut t, base).unwrap()[5], PAGE_READWRITE);
        assert_eq!(t.call32("MmQueryAddressProtect", &[base + 0x1000]), PAGE_READWRITE);
        // the whole region: recorded and reported
        assert_eq!(protect(&mut t, base, size, PAGE_READONLY).0, STATUS_SUCCESS);
        let q = query(&mut t, base).unwrap();
        assert_eq!((q[2], q[5]), (PAGE_READONLY, PAGE_READONLY));
        assert_eq!(t.call32("MmQueryAddressProtect", &[base]), PAGE_READONLY);
        assert!(!t.k.gm.space.allows(base, true));
        // and back: PAGE_NOACCESS is recorded only
        assert_eq!(protect(&mut t, base, size, PAGE_NOACCESS).3, PAGE_READONLY);
        assert!(t.k.gm.space.allows(base, true) && t.k.gm.space.allows(base + 0x1000, true));
        assert_eq!(query(&mut t, base).unwrap()[5], PAGE_NOACCESS);
        // a commit sets the protection of pages committed already
        assert_eq!(valloc(&mut t, base, 0x1000, MEM_COMMIT, PAGE_READONLY).0, STATUS_SUCCESS);
        assert!(!t.k.gm.space.allows(base, true));
        assert_eq!(valloc(&mut t, base, 0x1000, MEM_COMMIT, PAGE_READWRITE).0, STATUS_SUCCESS);
        assert!(t.k.gm.space.allows(base, true));
        // not committed, or nothing: refused
        assert_eq!(protect(&mut t, 0x3000_0000, 0x1000, PAGE_READONLY).0, STATUS_NOT_COMMITTED);
        assert_eq!(protect(&mut t, base, 0, PAGE_READONLY).0, STATUS_INVALID_PARAMETER);
    }

    #[test]
    fn the_image_stays_writable_whatever_protection_it_is_given() {
        let mut t = setup();
        let image = 0x0040_0000;
        assert!(t.k.mm_back(image, 0x2000));
        t.k.mm_note_image(image, 0x1800);
        let q = query(&mut t, image).unwrap();
        assert_eq!(q[1..], [image, PAGE_READWRITE, 0x2000, MEM_COMMIT, PAGE_READWRITE, MEM_IMAGE]);
        let io = t.alloc(12);
        t.k.gm.w32(io, image).unwrap();
        t.k.gm.w32(io + 4, 0x2000).unwrap();
        assert_eq!(
            t.call32("NtProtectVirtualMemory", &[io, io + 4, PAGE_READONLY, io + 8]),
            STATUS_SUCCESS
        );
        assert!(t.k.gm.space.allows(image, true));
        assert_eq!(query(&mut t, image).unwrap()[5], PAGE_READONLY);
        assert_eq!(vfree(&mut t, image, 0, MEM_RELEASE).0, STATUS_MEMORY_NOT_ALLOCATED);
        assert_eq!(stats(&mut t)[8], 2);
    }

    #[test]
    fn contiguous_blocks_come_from_the_top_of_ram_and_give_their_pages_back() {
        let mut t = setup();
        let ram = t.k.gm.ram_size();
        let free = t.k.gm.phys.free_pages();
        let a = t.call32("MmAllocateContiguousMemory", &[0x2345]);
        let b = t.call32("MmAllocateContiguousMemory", &[0x1000]);
        assert!(a >= CONTIG_WINDOW && a < CONTIG_WINDOW + ram && a.is_multiple_of(PAGE_SIZE));
        // packed downwards from the pages the kernel keeps for the GPU
        assert_eq!(a + 0x3000, CONTIG_WINDOW + ram - GPU_TOP);
        assert_eq!(b + 0x1000, a);
        assert_eq!(t.k.gm.phys.free_pages(), free - 4);
        assert_eq!(t.call32("MmQueryAllocationSize", &[a]), 0x3000);
        assert_eq!(t.call32("MmQueryAllocationSize", &[a + 0x1000]), 0);
        assert_eq!(t.k.mm_usage().0, 0x4000);
        assert_eq!(t.k.mm_usage().2, 2);
        // zeroed, even after a use
        t.k.gm.w32(a, 0x1234_5678).unwrap();
        t.call32("MmFreeContiguousMemory", &[a]);
        t.call32("MmFreeContiguousMemory", &[a + 0x1000]); // no such block: nothing happens
        assert_eq!(t.k.gm.phys.free_pages(), free - 1);
        let a2 = t.call32("MmAllocateContiguousMemory", &[0x3000]);
        assert_eq!(a2, a);
        assert_eq!(t.k.gm.r32(a2), Ok(0));
        t.call32("MmFreeContiguousMemory", &[a2]);
        t.call32("MmFreeContiguousMemory", &[b]);
        assert_eq!(t.k.gm.phys.free_pages(), free);
        assert_eq!(t.k.mm_usage().2, 0);
    }

    #[test]
    fn a_bounded_aligned_block_lies_in_its_range_at_its_alignment() {
        let mut t = setup();
        let (lo, hi) = (0x0100_0000u32, 0x01FF_F3FFu32);
        let va = t.call32("MmAllocateContiguousMemoryEx", &[0x5000, lo, hi, 0x1_0000, 0x404]);
        assert_ne!(va, 0);
        let pa = va & !CONTIG_WINDOW;
        assert_eq!(pa % 0x1_0000, 0);
        // the highest window: the last page the range's end lies in may be taken
        assert!(pa >= lo && pa + 0x5000 <= (hi | 0xFFF) + 1);
        assert_eq!(pa, 0x01FF_0000);
        assert_eq!(t.call32("MmQueryAddressProtect", &[va + 0x4FFF]), 0x404);
        t.call32("MmSetAddressProtect", &[va + 0x1000, 0x1000, PAGE_READONLY]);
        assert_eq!(t.call32("MmQueryAddressProtect", &[va]), PAGE_READONLY);
        // a range too small for it, and more than RAM: no block
        assert_eq!(t.call32("MmAllocateContiguousMemoryEx", &[0x5000, lo, lo + 0x3FFF, 0, 4]), 0);
        assert_eq!(t.call32("MmAllocateContiguousMemory", &[0xFFFF_F001]), 0);
        assert_eq!(t.call32("MmAllocateContiguousMemory", &[t.k.gm.ram_size()]), 0);
    }

    #[test]
    fn the_pages_an_alignment_skips_go_to_the_next_block_that_fits_them() {
        let mut t = setup();
        let free = t.k.gm.phys.free_pages();
        let top = t.call32("MmAllocateContiguousMemory", &[0x1000]);
        // 64 KiB aligned below it: the pages between are its slack
        let big =
            t.call32("MmAllocateContiguousMemoryEx", &[0x1_2000, 0, 0xFFFF_FFFF, 0x1_0000, 4]);
        assert_eq!(big % 0x1_0000, 0);
        let slack = top - (big + 0x1_2000);
        assert!((0x2000..0x1_0000).contains(&slack), "slack {slack:x}");
        assert_eq!(t.k.gm.phys.free_pages(), free - 1 - (0x1_2000 + slack) / PAGE_SIZE);
        // the next small block lands at the top of that slack, taking no new page
        let small = t.call32("MmAllocateContiguousMemory", &[0x2000]);
        assert_eq!(small + 0x2000, top);
        assert_eq!(t.k.gm.phys.free_pages(), free - 1 - (0x1_2000 + slack) / PAGE_SIZE);
        // freed, its pages are slack again; freed with the big block, all are free
        t.call32("MmFreeContiguousMemory", &[small]);
        assert_eq!(t.k.gm.phys.free_pages(), free - 1 - (0x1_2000 + slack) / PAGE_SIZE);
        assert_eq!(t.call32("MmAllocateContiguousMemory", &[0x2000]), small);
        t.call32("MmFreeContiguousMemory", &[small]);
        t.call32("MmFreeContiguousMemory", &[big]);
        assert_eq!(t.k.gm.phys.free_pages(), free - 1);
    }

    #[test]
    fn a_contiguous_block_moves_virtual_memory_out_of_its_way() {
        let mut t = setup();
        // virtual memory takes the highest free pages
        let (_, base, _) = valloc(&mut t, 0, 0x8000, MEM_COMMIT, PAGE_READWRITE);
        for i in 0..8 {
            t.k.gm.w32(base + i * 0x1000, 0xC0DE_0000 + i).unwrap();
        }
        let free = t.k.gm.phys.free_pages();
        let va = t.call32("MmAllocateContiguousMemory", &[0x10_0000]);
        assert_ne!(va, 0);
        assert_eq!(t.k.gm.phys.free_pages(), free - 0x100);
        let block = (va & !CONTIG_WINDOW)..(va & !CONTIG_WINDOW) + 0x10_0000;
        for i in 0..8 {
            assert_eq!(t.k.gm.r32(base + i * 0x1000), Ok(0xC0DE_0000 + i));
            assert!(!block.contains(&t.k.gm.phys_of(base + i * 0x1000).unwrap()));
        }
    }

    #[test]
    fn system_memory_is_contiguous_memory_with_its_protection() {
        let mut t = setup();
        let free = t.k.gm.phys.free_pages();
        let va = t.call32("MmAllocateSystemMemory", &[0x2800, PAGE_READWRITE]);
        assert!(va >= CONTIG_WINDOW);
        assert_eq!(t.call32("MmQueryAllocationSize", &[va]), 0x3000);
        assert_eq!(t.call32("MmFreeSystemMemory", &[va, 0]), 3);
        assert_eq!(t.call32("MmFreeSystemMemory", &[va, 0]), 0);
        assert_eq!(t.k.gm.phys.free_pages(), free);
    }

    #[test]
    fn physical_addresses_and_validity_follow_the_mappings() {
        let mut t = setup();
        let ram = t.k.gm.ram_size();
        // the window: VA = 0x80000000 | PA
        assert_eq!(t.call32("MmGetPhysicalAddress", &[CONTIG_WINDOW + 0x12_3456]), 0x12_3456);
        assert_eq!(t.call32("MmIsAddressValid", &[CONTIG_WINDOW + ram - 1]), 1);
        assert_eq!(t.call32("MmIsAddressValid", &[CONTIG_WINDOW + ram]), 0);
        assert_eq!(t.call32("MmIsAddressValid", &[TRAP_VA]), 0);
        assert_eq!(t.call32("MmGetPhysicalAddress", &[TRAP_VA]), 0);
        // virtual memory: the page behind it, which then stays put
        let (_, base, _) = valloc(&mut t, 0, 0x2000, MEM_RESERVE, PAGE_READWRITE);
        assert_eq!(t.call32("MmIsAddressValid", &[base]), 0);
        assert_eq!(t.call32("MmGetPhysicalAddress", &[base]), 0);
        valloc(&mut t, base, 0x1000, MEM_COMMIT, PAGE_READWRITE);
        assert_eq!(t.call32("MmIsAddressValid", &[base + 0xFFF]), 1);
        assert_eq!(t.call32("MmIsAddressValid", &[base + 0x1000]), 0);
        let pa = t.call32("MmGetPhysicalAddress", &[base + 0x123]);
        assert_eq!(Some(pa), t.k.gm.phys_of(base + 0x123));
        assert_eq!(pa & 0xFFF, 0x123);
        t.k.gm.w32(base + 0x120, 0xFEED_F00D).unwrap();
        assert_eq!(t.k.gm.r32(CONTIG_WINDOW + (pa & !0xFFF) + 0x120), Ok(0xFEED_F00D));
        assert!(t.k.gm.phys.pin_state(pa).known);
        // RAM is mapped "as I/O space" through the window, devices where they are
        assert_eq!(t.call32("MmMapIoSpace", &[0x1000, 0x1000, 4]), CONTIG_WINDOW + 0x1000);
        assert_eq!(t.call32("MmMapIoSpace", &[0xFD00_0000, 0x1000, 4]), 0xFD00_0000);
    }

    #[test]
    fn buffer_locks_pin_and_unpin_the_pages_behind_a_range() {
        let mut t = setup();
        let (_, base, _) = valloc(&mut t, 0, 0x3000, MEM_COMMIT, PAGE_READWRITE);
        let pa: Vec<u32> = (0..3).map(|i| t.k.gm.phys_of(base + i * 0x1000).unwrap()).collect();
        t.call32("MmLockUnlockBufferPages", &[base + 0xFFF, 2, 0]);
        assert_eq!([1, 1, 0], [0, 1, 2].map(|i| t.k.gm.phys.pin_state(pa[i]).locks));
        t.call32("MmLockUnlockPhysicalPage", &[pa[2], 0]);
        t.call32("MmLockUnlockBufferPages", &[base, 0x3000, 1]);
        t.call32("MmLockUnlockBufferPages", &[base, 0, 0]);
        assert!(pa.iter().all(|&p| !t.k.gm.phys.pin_state(p).is_pinned()));
    }

    #[test]
    fn the_statistics_agree_with_the_allocator_and_the_regions() {
        let mut t = setup();
        let s0 = stats(&mut t);
        assert_eq!(s0[0], 0x24);
        assert_eq!(s0[1], t.k.gm.ram_size() / PAGE_SIZE);
        assert_eq!(s0[2], t.k.gm.phys.free_pages());
        assert_eq!(s0[6], t.k.pool.len / PAGE_SIZE);
        let (_, base, _) = valloc(&mut t, 0, 0x2_0000, MEM_RESERVE, PAGE_READWRITE);
        valloc(&mut t, base, 0x5000, MEM_COMMIT, PAGE_READWRITE);
        let va = t.call32("MmAllocateContiguousMemory", &[0x3000]);
        let s1 = stats(&mut t);
        assert_eq!(s1[2], s0[2] - 5 - 3);
        assert_eq!(s1[3], s0[3] + 0x5000);
        assert_eq!(s1[4], s0[4] + 0x2_0000);
        assert_eq!(t.k.mm_usage(), (0x3000, s1[3] + s1[8] * PAGE_SIZE, 1));
        t.call32("MmFreeContiguousMemory", &[va]);
        vfree(&mut t, base, 0, MEM_RELEASE);
        assert_eq!(stats(&mut t), s0);
        // a structure of another size is refused
        let s = t.alloc(0x24);
        assert_eq!(t.call32("MmQueryStatistics", &[s]), STATUS_INVALID_PARAMETER);
    }

    #[test]
    fn a_kernel_stack_is_mapped_below_its_top_and_its_slot_is_reused() {
        let mut t = setup();
        let free = t.k.gm.phys.free_pages();
        let top = t.call32("MmCreateKernelStack", &[0x2800, 0]);
        assert_ne!(top, 0);
        assert!(t.k.gm.mapped_range(top - 0x3000, 0x3000));
        assert!(!t.k.gm.mapped(top - 0x3001));
        assert_eq!(t.k.gm.phys.free_pages(), free - 3);
        t.k.gm.w32(top - 4, 1).unwrap();
        t.call32("MmDeleteKernelStack", &[top, top - 0x3000]);
        assert!(!t.k.gm.mapped(top - 4));
        assert_eq!(t.k.gm.phys.free_pages(), free);
        assert_eq!(t.call32("MmCreateKernelStack", &[0x3000, 0]), top);
        assert_eq!(t.k.gm.r32(top - 4), Ok(0));
        // the windows are not a stack's to delete
        t.call32("MmDeleteKernelStack", &[CONTIG_WINDOW + 0x2_0000, CONTIG_WINDOW + 0x1_0000]);
        assert!(t.k.gm.mapped(CONTIG_WINDOW + 0x1_0000));
    }

    #[test]
    fn the_gpu_s_instance_memory_ends_under_the_pad_at_the_top_of_ram() {
        let mut t = setup();
        let pad = t.alloc(4);
        let free = t.k.gm.phys.free_pages();
        let end = t.call32("MmClaimGpuInstanceMemory", &[0x5000, pad]);
        assert_eq!(end, CONTIG_WINDOW + t.k.gm.ram_size() - GPU_PAD);
        assert_eq!(t.k.gm.r32(pad), Ok(GPU_PAD));
        // what the kernel keeps from boot is enough for that; a larger block takes the rest
        assert_eq!(t.k.gm.phys.free_pages(), free);
        let mut t = setup();
        let free = t.k.gm.phys.free_pages();
        assert_eq!(t.call32("MmClaimGpuInstanceMemory", &[0x8000, 0]), end);
        assert_eq!(t.k.gm.phys.free_pages(), free - 3);
        assert_eq!(t.call32("MmClaimGpuInstanceMemory", &[0x2_0000, 0]), end);
        assert_eq!(t.k.gm.phys.free_pages(), free - 3);
    }

    #[test]
    fn kept_memory_comes_back_at_its_address_in_the_next_kernel() {
        let mut t = setup();
        let va = t.call32("MmAllocateContiguousMemory", &[0x2000]);
        let other = t.call32("MmAllocateContiguousMemory", &[0x1000]);
        t.k.gm.write(va, b"the last frame").unwrap();
        t.call32("MmPersistContiguousMemory", &[va, 0x1800, 1]);
        t.call32("MmPersistContiguousMemory", &[other, 0x1000, 1]);
        t.call32("MmPersistContiguousMemory", &[va, 0x1801, 1]);
        assert_eq!(
            t.k.mm_kept(),
            vec![(va & !CONTIG_WINDOW, 0x1801), (other & !CONTIG_WINDOW, 0x1000)]
        );
        t.call32("MmPersistContiguousMemory", &[other, 0x1000, 0]);
        t.k.mm_keep_drop(0x1234);
        let kept = t.k.mm_keep_collect();
        assert_eq!(kept.len(), 1);
        assert_eq!((kept[0].0, kept[0].1.len()), (va & !CONTIG_WINDOW, 0x1801));

        let mut n = setup();
        let free = n.k.gm.phys.free_pages();
        n.k.mm_keep_restore(&kept);
        assert_eq!(n.k.gm.phys.free_pages(), free - 2);
        assert_eq!(n.k.gm.read_vec(va, 14).unwrap(), b"the last frame");
        assert_eq!(n.call32("MmQueryAllocationSize", &[va]), 0x2000);
        assert_eq!(n.k.mm_kept(), vec![(va & !CONTIG_WINDOW, 0x1801)]);
        // the next block does not land on it, and the title frees it as its own
        let next = n.call32("MmAllocateContiguousMemory", &[0x2000]);
        assert!(next + 0x2000 <= va || next >= va + 0x2000);
        // a second restore of the same pages is refused
        n.k.mm_keep_restore(&kept);
        assert_eq!(n.k.mm_kept().len(), 1);
        n.k.mm_keep_drop(va);
        assert!(n.k.mm_kept().is_empty());
        n.call32("MmFreeContiguousMemory", &[va]);
        assert_eq!(n.k.gm.phys.free_pages(), free - 2);
    }
}
