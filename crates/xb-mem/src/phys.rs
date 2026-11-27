//! The physical page allocator: which pages of RAM are in use, and which are
//! pinned.
//!
//! The allocators that relocate pages out of the way need the page table and
//! are methods of [`crate::AddressSpace`].
//!
//! Addresses and lengths are in bytes. A length is rounded up to whole pages;
//! a length of 0 allocates one page, as in the reference.

use xb_types::{PAGE_SHIFT, PAGE_SIZE};

/// The top of RAM that virtual memory leaves until nothing else is free: the
/// GPU's instance memory (`MmClaimGpuInstanceMemory`) is taken from there.
pub const TOP_RESERVE: u32 = 0x10_0000;

const PIN_KNOWN: u8 = 0x80;
const PIN_LOCKS: u8 = 0x7F;

/// Why a page must stay where it is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PinState {
    /// The page's physical address went out to the title
    /// (`MmGetPhysicalAddress`, the title's own page tables).
    pub known: bool,
    /// Outstanding locks (`MmLockUnlockBufferPages`,
    /// `MmLockUnlockPhysicalPage`), saturating at 127.
    pub locks: u8,
}

impl PinState {
    /// Whether the page is pinned at all.
    #[must_use]
    pub fn is_pinned(self) -> bool {
        self.known || self.locks != 0
    }
}

/// Totals for out-of-memory reports and `MmQueryStatistics`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PhysStats {
    /// Pages of RAM.
    pub total_pages: u32,
    /// Pages not in use.
    pub free_pages: u32,
    /// Number of free runs.
    pub free_runs: u32,
    /// Length of the longest free run, in pages.
    pub largest_free_run: u32,
}

/// Which pages of RAM are in use.
///
/// Used on the guest-CPU thread only (the kernel's memory manager owns it).
#[derive(Debug, Clone)]
pub struct PhysAllocator {
    used: Vec<bool>,
    pin: Vec<u8>,
    free: u32,
}

impl PhysAllocator {
    /// An allocator for `ram_size` bytes of RAM with every page free.
    ///
    /// The caller then claims `[0, 0x10000)` with [`PhysAllocator::claim`],
    /// as the reference does: physical memory below the kernel image is never
    /// handed out, because devices whose DMA pointers the title has not set
    /// yet write near address 0.
    #[must_use]
    pub fn new(ram_size: u32) -> Self {
        let pages = (ram_size / PAGE_SIZE) as usize;
        Self { used: vec![false; pages], pin: vec![0; pages], free: pages as u32 }
    }

    /// Pages of RAM.
    #[must_use]
    pub fn pages(&self) -> u32 {
        self.used.len() as u32
    }

    /// Size of RAM in bytes.
    #[must_use]
    pub fn ram_size(&self) -> u32 {
        self.pages() << PAGE_SHIFT
    }

    /// Pages `len` bytes take, or `None` if that is more than RAM (a title
    /// asking for 4 GiB must not wrap to 0).
    fn pages_for(&self, len: u32) -> Option<u32> {
        if len > self.ram_size() {
            return None;
        }
        Some(len.max(1).div_ceil(PAGE_SIZE))
    }

    /// Whether page `p` is in use; pages outside RAM count as used.
    #[inline]
    pub(crate) fn page_used(&self, p: u32) -> bool {
        self.used.get(p as usize).copied().unwrap_or(true)
    }

    /// Marks pages `[p, p + n)` used or free, keeping the free count.
    pub(crate) fn set_pages(&mut self, p: u32, n: u32, used: bool) {
        for u in &mut self.used[p as usize..(p + n) as usize] {
            if *u != used {
                *u = used;
                if used {
                    self.free -= 1;
                } else {
                    self.free += 1;
                }
            }
        }
    }

    /// The pin byte of page `p`.
    pub(crate) fn pin_raw(&self, p: u32) -> u8 {
        self.pin[p as usize]
    }

    /// Whether page `p`'s only pin is that its address is known.
    pub(crate) fn pin_only_known(&self, p: u32) -> bool {
        self.pin[p as usize] == PIN_KNOWN
    }

    /// Moves page `from`'s pins to page `to` (a relocated page whose address
    /// went out stays known where it goes).
    pub(crate) fn move_pin(&mut self, from: u32, to: u32) {
        self.pin[to as usize] = self.pin[from as usize];
        self.pin[from as usize] = 0;
    }

    /// Allocates `len` bytes of contiguous pages inside `[lo, hi)`, best fit:
    /// the smallest free run that holds them, the highest of equals, from the
    /// run's top end.
    ///
    /// Small, long-lived allocations fill holes and leave the large runs
    /// whole for contiguous allocations. `hi` is clamped to the end of RAM
    /// (pass `u32::MAX` for no bound; unlike [`PhysAllocator::alloc_aligned`],
    /// 0 here means an empty range, as in the reference).
    pub fn alloc(&mut self, len: u32, lo: u32, hi: u32) -> Option<u32> {
        let pages = self.pages_for(len)?;
        let first = lo / PAGE_SIZE;
        let last = hi.min(self.ram_size()) / PAGE_SIZE;
        let mut best: Option<(u32, u32)> = None;
        let mut p = first;
        while p < last {
            if self.used[p as usize] {
                p += 1;
                continue;
            }
            let mut q = p;
            while q < last && !self.used[q as usize] {
                q += 1;
            }
            if q - p >= pages && best.is_none_or(|(_, l)| q - p <= l) {
                best = Some((p, q - p));
            }
            p = q;
        }
        let (start, run) = best?;
        let p = start + run - pages;
        self.set_pages(p, pages, true);
        Some(p << PAGE_SHIFT)
    }

    /// Allocates `len` bytes of contiguous free pages inside `[lo, hi)`
    /// aligned to `align` (a power of two; less than a page means a page),
    /// the lowest fitting address first, or the highest with `top_down`.
    ///
    /// `hi` is clamped to the end of RAM; as in the reference, `hi == 0`
    /// means no upper bound.
    pub fn alloc_aligned(
        &mut self,
        len: u32,
        lo: u32,
        hi: u32,
        align: u32,
        top_down: bool,
    ) -> Option<u32> {
        let pages = self.pages_for(len)?;
        let (step, first, last) = self.window_bounds(lo, hi, align);
        if last < pages || first > last - pages {
            return None;
        }
        let p = find_window(pages, step, first, last, top_down, |q| self.used[q as usize])?;
        self.set_pages(p, pages, true);
        Some(p << PAGE_SHIFT)
    }

    /// `(step, first, last)` in pages for a search of `[lo, hi)` at `align`,
    /// as the reference computes them.
    pub(crate) fn window_bounds(&self, lo: u32, hi: u32, align: u32) -> (u32, u32, u32) {
        let step = align.max(PAGE_SIZE) / PAGE_SIZE;
        // `lo` rounded up to the alignment; past the end of the address space
        // it is beyond any window.
        let first =
            u64::from(lo).div_ceil(u64::from(step) * u64::from(PAGE_SIZE)) * u64::from(step);
        let first = u32::try_from(first).unwrap_or(u32::MAX);
        let ram = self.ram_size();
        let last = if hi != 0 && hi < ram { hi } else { ram } / PAGE_SIZE;
        (step, first, last)
    }

    /// Allocates the run of free pages that ends at the highest free page
    /// below [`TOP_RESERVE`] (or, when nothing is free below it, at the
    /// highest free page inside it), extended downwards to at most `len` bytes. Returns
    /// `(pa, bytes)`; the run may be shorter than asked.
    ///
    /// This is how virtual memory takes its pages: it packs at the top of RAM
    /// with the contiguous allocations and leaves low RAM free in one run.
    /// Halo asks for its game state at physical `0x61000` after its image,
    /// heap and stacks have their pages.
    pub fn alloc_high_run(&mut self, len: u32) -> Option<(u32, u32)> {
        let want = len.max(1).div_ceil(PAGE_SIZE);
        let ram_pages = self.pages();
        let reserve = if TOP_RESERVE / PAGE_SIZE < ram_pages { TOP_RESERVE / PAGE_SIZE } else { 0 };
        let below = ram_pages - reserve;
        let top = (0..below)
            .rev()
            .find(|&p| !self.used[p as usize])
            .or_else(|| (below..ram_pages).rev().find(|&p| !self.used[p as usize]))?;
        let mut p = top;
        while p > 0 && !self.used[(p - 1) as usize] && top + 1 - p < want {
            p -= 1;
        }
        let n = top + 1 - p;
        self.set_pages(p, n, true);
        Some((p << PAGE_SHIFT, n << PAGE_SHIFT))
    }

    /// Marks `[pa, pa + len)` (page aligned) as in use, for fixed layouts
    /// such as the kernel's. Returns `false`, claiming nothing, if the range
    /// is unaligned, leaves RAM or has a used page.
    ///
    /// (The reference marks the pages before the first used one and then
    /// fails; no caller relies on that.)
    pub fn claim(&mut self, pa: u32, len: u32) -> bool {
        if !(pa | len).is_multiple_of(PAGE_SIZE)
            || u64::from(pa) + u64::from(len) > u64::from(self.ram_size())
        {
            return false;
        }
        let (p, n) = (pa / PAGE_SIZE, len / PAGE_SIZE);
        if (p..p + n).any(|q| self.used[q as usize]) {
            return false;
        }
        self.set_pages(p, n, true);
        true
    }

    /// Frees the pages of `[pa, pa + len)` and drops their pins. Returns
    /// `false`, freeing nothing, if the range leaves RAM.
    ///
    /// The GPU must drop what it keeps over the memory; the caller tells it.
    /// A page must be unmapped before it is freed
    /// ([`crate::AddressSpace::free_at`] does both in order).
    pub fn free(&mut self, pa: u32, len: u32) -> bool {
        let pages = len.div_ceil(PAGE_SIZE);
        let p = pa / PAGE_SIZE;
        if u64::from(p) + u64::from(pages) > u64::from(self.pages()) {
            return false;
        }
        self.set_pages(p, pages, false);
        self.pin[p as usize..(p + pages) as usize].fill(0);
        true
    }

    /// Whether the page holding `pa` is in use. Addresses outside RAM count
    /// as used.
    #[must_use]
    pub fn is_used(&self, pa: u32) -> bool {
        self.page_used(pa >> PAGE_SHIFT)
    }

    /// Pins the page holding `pa`: `known` records that its address went out;
    /// `lock > 0` adds a lock and `lock < 0` removes one. A pinned page is
    /// never relocated. Ignored outside RAM.
    pub fn pin(&mut self, pa: u32, known: bool, lock: i32) {
        let Some(p) = self.pin.get_mut((pa >> PAGE_SHIFT) as usize) else {
            return;
        };
        let count = *p & PIN_LOCKS;
        if lock > 0 && count < PIN_LOCKS {
            *p = (*p & PIN_KNOWN) | (count + 1);
        } else if lock < 0 && count > 0 {
            *p = (*p & PIN_KNOWN) | (count - 1);
        }
        if known {
            *p |= PIN_KNOWN;
        }
    }

    /// The pins of the page holding `pa` (none outside RAM).
    #[must_use]
    pub fn pin_state(&self, pa: u32) -> PinState {
        let b = self.pin.get((pa >> PAGE_SHIFT) as usize).copied().unwrap_or(0);
        PinState { known: b & PIN_KNOWN != 0, locks: b & PIN_LOCKS }
    }

    /// Pages not in use.
    #[must_use]
    pub fn free_pages(&self) -> u32 {
        self.free
    }

    /// RAM not in use, in bytes.
    #[must_use]
    pub fn free_bytes(&self) -> u32 {
        self.free << PAGE_SHIFT
    }

    /// RAM in use, in bytes.
    #[must_use]
    pub fn used_bytes(&self) -> u32 {
        self.ram_size() - self.free_bytes()
    }

    /// The free runs as `(pa, bytes)`, lowest first.
    pub fn free_runs(&self) -> impl Iterator<Item = (u32, u32)> + '_ {
        let pages = self.pages();
        let mut p = 0;
        std::iter::from_fn(move || {
            while p < pages && self.used[p as usize] {
                p += 1;
            }
            if p >= pages {
                return None;
            }
            let start = p;
            while p < pages && !self.used[p as usize] {
                p += 1;
            }
            Some((start << PAGE_SHIFT, (p - start) << PAGE_SHIFT))
        })
    }

    /// Totals over the whole of RAM.
    #[must_use]
    pub fn stats(&self) -> PhysStats {
        let mut s =
            PhysStats { total_pages: self.pages(), free_pages: self.free, ..PhysStats::default() };
        for (_, len) in self.free_runs() {
            s.free_runs += 1;
            s.largest_free_run = s.largest_free_run.max(len / PAGE_SIZE);
        }
        s
    }

    /// The free runs of 64 KiB or more, their count and the largest, as text
    /// for an out-of-memory report.
    #[must_use]
    pub fn free_runs_text(&self) -> String {
        use std::fmt::Write;
        let mut out = String::new();
        for (pa, len) in self.free_runs().filter(|&(_, len)| len >= 0x1_0000) {
            let _ = write!(out, " {pa:x}+{}K", len >> 10);
        }
        let s = self.stats();
        let _ = write!(
            out,
            " | {} runs, largest {} KiB",
            s.free_runs,
            s.largest_free_run * (PAGE_SIZE >> 10)
        );
        out
    }
}

/// The first window of `pages` pages, at a multiple of `step`, inside
/// `[first, last)` with no used page: the lowest, or the highest with
/// `top_down`. `first` is a multiple of `step` and `pages <= last`.
///
/// A window that meets a used page u fails, and so does every other window
/// over u: the next candidate is the first past u (bottom-up) or the last
/// that ends at u (top-down), so each page is looked at about once (The
/// Warriors sizes its heap with ~2000 requests larger than the free run,
/// 13 ms each if every start is rescanned).
pub(crate) fn find_window(
    pages: u32,
    step: u32,
    first: u32,
    last: u32,
    top_down: bool,
    used: impl Fn(u32) -> bool,
) -> Option<u32> {
    if pages == 0 || last < pages || first > last - pages {
        return None;
    }
    let top = (last - pages) / step * step;
    let mut p = if top_down { top } else { first };
    loop {
        if p < first || p > top {
            return None;
        }
        let mut run = 0;
        while run < pages && !used(p + run) {
            run += 1;
        }
        if run == pages {
            return Some(p);
        }
        let u = p + run;
        if top_down {
            if u < pages {
                return None;
            }
            p = (u - pages) / step * step;
        } else {
            p = (u + 1).div_ceil(step) * step;
        }
    }
}

impl xb_types::state::Stateful for PhysAllocator {
    /// Which pages are in use and their pins. A stream for another RAM size
    /// fails and changes nothing.
    fn state(&mut self, io: &mut xb_types::state::StateIo) {
        let mut pages = self.used.len() as u32;
        pages.state(io);
        if io.loading() && pages as usize != self.used.len() {
            io.fail("the state's page allocator is of another RAM size");
            return;
        }
        for u in &mut self.used {
            u.state(io);
        }
        for p in &mut self.pin {
            p.state(io);
        }
        self.free.state(io);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: u32 = PAGE_SIZE;

    fn alloc_with(pages: u32, used: &[(u32, u32)]) -> PhysAllocator {
        let mut a = PhysAllocator::new(pages * P);
        for &(p, n) in used {
            assert!(a.claim(p * P, n * P));
        }
        a
    }

    /// Recounts free pages from the page array.
    fn recount(a: &PhysAllocator) -> u32 {
        (0..a.pages()).filter(|&p| !a.page_used(p)).count() as u32
    }

    #[test]
    fn best_fit_takes_the_top_of_the_smallest_run_that_holds_the_request() {
        // Free runs: [0,8) 8 pages, [10,13) 3 pages, [20,32) 12.
        let mut a = alloc_with(32, &[(8, 2), (13, 7)]);
        assert_eq!(a.alloc(2 * P, 0, u32::MAX), Some(11 * P));
        // What is left of that run holds one page.
        assert_eq!(a.alloc(P, 0, u32::MAX), Some(10 * P));
        // Now the 8-page run is the smallest that holds 4.
        assert_eq!(a.alloc(4 * P, 0, u32::MAX), Some(4 * P));
        assert_eq!(a.free_pages(), recount(&a));
    }

    #[test]
    fn best_fit_prefers_the_highest_of_equal_runs() {
        let mut a = alloc_with(16, &[(4, 4), (12, 4)]);
        assert_eq!(a.alloc(P, 0, u32::MAX), Some(11 * P));
    }

    #[test]
    fn best_fit_respects_its_bounds() {
        let mut a = alloc_with(32, &[]);
        assert_eq!(a.alloc(4 * P, 8 * P, 16 * P), Some(12 * P));
        assert_eq!(a.alloc(8 * P, 8 * P, 12 * P), None);
    }

    #[test]
    fn a_zero_length_request_takes_one_page_and_more_than_ram_fails() {
        let mut a = alloc_with(8, &[]);
        assert!(a.alloc(0, 0, u32::MAX).is_some());
        assert_eq!(a.free_pages(), 7);
        assert_eq!(a.alloc(u32::MAX, 0, u32::MAX), None);
        assert_eq!(a.alloc_aligned(u32::MAX, 0, 0, P, true), None);
        assert_eq!(a.free_pages(), 7);
    }

    #[test]
    fn aligned_allocation_goes_bottom_up_or_top_down() {
        let mut a = alloc_with(64, &[(0, 1)]);
        assert_eq!(a.alloc_aligned(3 * P, 0, 0, 4 * P, false), Some(4 * P));
        assert_eq!(a.alloc_aligned(3 * P, 0, 0, 4 * P, true), Some(60 * P));
        // The next top-down window must end below the used one, aligned.
        assert_eq!(a.alloc_aligned(3 * P, 0, 0, 4 * P, true), Some(56 * P));
        // An unaligned low bound rounds up to the alignment.
        assert_eq!(a.alloc_aligned(P, 9 * P, 0, 8 * P, false), Some(16 * P));
        assert_eq!(a.free_pages(), recount(&a));
    }

    #[test]
    fn aligned_allocation_skips_past_used_pages() {
        // Only [20, 24) is free and big enough at alignment 4.
        let mut a = alloc_with(32, &[(2, 1), (6, 1), (10, 1), (14, 1), (18, 1), (25, 7)]);
        assert_eq!(a.alloc_aligned(4 * P, 0, 0, 4 * P, false), Some(20 * P));
        a.free(20 * P, 4 * P);
        assert_eq!(a.alloc_aligned(4 * P, 0, 0, 4 * P, true), Some(20 * P));
        assert_eq!(a.alloc_aligned(4 * P, 0, 0, 4 * P, true), None);
        assert_eq!(a.alloc_aligned(4 * P, 0, 0, 4 * P, false), None);
    }

    #[test]
    fn aligned_allocation_stays_below_the_upper_bound() {
        let mut a = alloc_with(64, &[]);
        assert_eq!(a.alloc_aligned(4 * P, 0, 16 * P, P, true), Some(12 * P));
        assert_eq!(a.alloc_aligned(16 * P, 0, 16 * P, P, true), None);
        assert_eq!(a.alloc_aligned(4 * P, 60 * P, 16 * P, P, false), None);
    }

    #[test]
    fn both_search_directions_agree_with_a_brute_force_search() {
        // Relation test: the skipping search finds what trying every aligned
        // window in order finds.
        let mut seed = 0x1234_5678u32;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        for _ in 0..300 {
            let mut a = PhysAllocator::new(96 * P);
            for p in 0..96 {
                if rnd() % 3 == 0 {
                    a.claim(p * P, P);
                }
            }
            let pages = 1 + rnd() % 5;
            let step = 1 << (rnd() % 3);
            let lo = rnd() % 40;
            let hi = 50 + rnd() % 47;
            for top_down in [false, true] {
                let first = lo.div_ceil(step) * step;
                let mut cands: Vec<u32> = (first..=hi.saturating_sub(pages))
                    .step_by(step as usize)
                    .filter(|&s| s + pages <= hi && (s..s + pages).all(|q| !a.page_used(q)))
                    .collect();
                if top_down {
                    cands.reverse();
                }
                let mut b = a.clone();
                let got = b.alloc_aligned(pages * P, lo * P, hi * P, step * P, top_down);
                assert_eq!(
                    got,
                    cands.first().map(|s| s * P),
                    "pages {pages} step {step} lo {lo} hi {hi}"
                );
            }
        }
    }

    #[test]
    fn the_high_run_keeps_out_of_the_top_reserve_until_nothing_else_is_free() {
        // 512 pages = 2 MiB; the reserve is the top 256.
        let mut a = alloc_with(512, &[(0, 250)]);
        // Highest free page below the reserve is 255; the run extends down.
        assert_eq!(a.alloc_high_run(4 * P), Some((252 * P, 4 * P)));
        // Only two pages are left below the reserve: a short run comes back.
        assert_eq!(a.alloc_high_run(4 * P), Some((250 * P, 2 * P)));
        // Then the reserve, from its highest page.
        assert_eq!(a.alloc_high_run(2 * P), Some((510 * P, 2 * P)));
        assert_eq!(a.free_pages(), recount(&a));
    }

    #[test]
    fn the_high_run_stops_at_a_used_page() {
        let mut a = alloc_with(512, &[(200, 1)]);
        assert_eq!(a.alloc_high_run(100 * P), Some((201 * P, 55 * P)));
        assert_eq!(a.alloc_high_run(100 * P), Some((100 * P, 100 * P)));
    }

    #[test]
    fn a_full_allocator_gives_nothing() {
        let mut a = alloc_with(8, &[(0, 8)]);
        assert_eq!(a.alloc(P, 0, u32::MAX), None);
        assert_eq!(a.alloc_aligned(P, 0, 0, P, false), None);
        assert_eq!(a.alloc_high_run(P), None);
        assert_eq!(a.free_bytes(), 0);
    }

    #[test]
    fn claim_is_all_or_nothing() {
        let mut a = alloc_with(16, &[(4, 1)]);
        assert!(!a.claim(2 * P, 4 * P));
        assert!(!a.is_used(2 * P));
        assert!(!a.claim(P + 1, P));
        assert!(!a.claim(15 * P, 2 * P));
        assert!(a.claim(5 * P, 2 * P));
        assert_eq!(a.free_pages(), 13);
    }

    #[test]
    fn freeing_returns_the_pages_and_drops_their_pins() {
        let mut a = alloc_with(16, &[]);
        let pa = a.alloc(3 * P, 0, u32::MAX).unwrap();
        a.pin(pa, true, 1);
        assert!(a.pin_state(pa).is_pinned());
        // A length that is not a page multiple frees whole pages.
        assert!(a.free(pa, 2 * P + 1));
        assert_eq!(a.free_pages(), 16);
        assert_eq!(a.pin_state(pa), PinState::default());
        assert!(!a.free(15 * P, 2 * P));
        assert!(a.is_used(16 * P));
    }

    #[test]
    fn pins_count_locks_and_remember_that_an_address_went_out() {
        let mut a = alloc_with(4, &[]);
        a.pin(P + 5, false, 1);
        a.pin(P, false, 1);
        assert_eq!(a.pin_state(P), PinState { known: false, locks: 2 });
        a.pin(P, true, -1);
        assert_eq!(a.pin_state(P), PinState { known: true, locks: 1 });
        a.pin(P, false, -1);
        a.pin(P, false, -1); // one unlock too many is ignored
        assert_eq!(a.pin_state(P), PinState { known: true, locks: 0 });
        assert!(a.pin_state(P).is_pinned());
        for _ in 0..200 {
            a.pin(2 * P, false, 1);
        }
        assert_eq!(a.pin_state(2 * P).locks, 127);
        a.pin(99 * P, true, 1); // outside RAM: ignored
        assert_eq!(a.pin_state(99 * P), PinState::default());
    }

    #[test]
    fn statistics_describe_the_free_runs() {
        let a = alloc_with(64, &[(0, 16), (40, 8)]);
        assert_eq!(a.free_runs().collect::<Vec<_>>(), [(16 * P, 24 * P), (48 * P, 16 * P)]);
        assert_eq!(
            a.stats(),
            PhysStats { total_pages: 64, free_pages: 40, free_runs: 2, largest_free_run: 24 }
        );
        assert_eq!(a.used_bytes() + a.free_bytes(), a.ram_size());
        assert_eq!(a.free_runs_text(), " 10000+96K 30000+64K | 2 runs, largest 96 KiB");
    }
}
