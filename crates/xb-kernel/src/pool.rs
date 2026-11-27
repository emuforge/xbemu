//! The kernel pool.
//!
//! Guest memory for ExAllocatePool and kernel objects, in physical RAM
//! reached through the contiguous window: an initial range, then 64 KiB
//! chunks added as it fills. First-fit over a sorted free list kept on the
//! host; blocks are 16-byte aligned.

use xb_types::{CONTIG_WINDOW, PAGE_SIZE, align_up};

use crate::kernel::Kernel;

const POOL_ALIGN: u32 = 16;
const POOL_CHUNK: u32 = 0x10000;
const POOL_RANGES: usize = 8192;

/// The pool's bookkeeping.
#[derive(Default)]
pub(crate) struct Pool {
    /// The initial range: it stays (`give_back`).
    first: (u32, u32),
    /// Bytes the pool spans.
    pub len: u32,
    /// Free ranges `(va, len)`, sorted by address.
    pub free: Vec<(u32, u32)>,
    /// Live ExAllocatePool bytes, object bytes, bytes a full free list dropped.
    pub ex: u32,
    pub ob: u32,
    pub lost: u32,
    given_logged: u32,
}

impl Pool {
    /// The range the pool began with, which it keeps: `(va, len)`.
    pub(crate) const fn first(&self) -> (u32, u32) {
        self.first
    }

    /// Bytes on the free list.
    pub(crate) fn free_bytes(&self) -> u32 {
        self.free.iter().map(|r| r.1).sum()
    }
}

impl Kernel {
    pub(crate) fn pool_init(&mut self, va: u32, len: u32) {
        self.pool.first = (va, len);
        self.pool.len = len;
        self.pool.free = vec![(va, len)];
        self.gm.kfill(va, 0, len);
    }

    /// More pool: a chunk of free physical RAM joins the free list.
    fn pool_grow(&mut self, size: u32) -> bool {
        let chunk = align_up(size.max(POOL_CHUNK), POOL_CHUNK);
        let Some(pa) = self.gm.alloc_contig_high(chunk) else { return false };
        self.gm.kfill(CONTIG_WINDOW | pa, 0, chunk);
        self.pool.len += chunk;
        self.pool_free(CONTIG_WINDOW | pa, chunk);
        true
    }

    /// Allocates `size` zeroed bytes; 0 when RAM is out.
    pub(crate) fn pool_alloc(&mut self, size: u32) -> u32 {
        let size = align_up(size.max(1), POOL_ALIGN);
        loop {
            if let Some(i) = self.pool.free.iter().position(|r| r.1 >= size) {
                let r = &mut self.pool.free[i];
                let va = r.0;
                r.0 += size;
                r.1 -= size;
                if r.1 == 0 {
                    self.pool.free.remove(i);
                }
                self.gm.kfill(va, 0, size);
                return va;
            }
            if !self.pool_grow(size) {
                log::error!(target: "kernel", "pool: out of kernel pool for {size} bytes");
                return 0;
            }
        }
    }

    /// Gives `size` bytes at `va` back.
    pub(crate) fn pool_free(&mut self, va: u32, size: u32) {
        let size = align_up(size.max(1), POOL_ALIGN);
        let f = &mut self.pool.free;
        let i = f.partition_point(|r| r.0 < va);
        let join_prev = i > 0 && f[i - 1].0 + f[i - 1].1 == va;
        let join_next = i < f.len() && va + size == f[i].0;
        if join_prev && join_next {
            f[i - 1].1 += size + f[i].1;
            f.remove(i);
        } else if join_prev {
            f[i - 1].1 += size;
        } else if join_next {
            f[i].0 = va;
            f[i].1 += size;
        } else if f.len() < POOL_RANGES {
            f.insert(i, (va, size));
        } else {
            self.pool.lost += size;
        }
    }

    /// The pages the free list holds whole, outside the initial range (the
    /// objects made at boot are there), back to RAM; the bytes given back.
    /// The chunks the pool grew by are never freed otherwise: a contiguous
    /// allocation that finds no window calls this.
    pub(crate) fn pool_give_back(&mut self) -> u32 {
        let mut given = 0;
        let (first_va, first_len) = self.pool.first;
        let first_end = first_va + first_len;
        let mut i = 0;
        while i < self.pool.free.len() {
            let (va, len) = self.pool.free[i];
            let end = va + len;
            let mut a = align_up(va, PAGE_SIZE);
            let mut b = end & !(PAGE_SIZE - 1);
            if a < first_end && b > first_va {
                if a >= first_va {
                    a = first_end;
                } else {
                    b = first_va;
                }
            }
            if a >= b {
                i += 1;
                continue;
            }
            let (head, tail) = (a > va, b < end);
            if head && tail {
                if self.pool.free.len() == POOL_RANGES {
                    i += 1;
                    continue;
                }
                self.pool.free[i].1 = a - va;
                self.pool.free.insert(i + 1, (b, end - b));
                i += 2;
            } else if head {
                self.pool.free[i].1 = a - va;
                i += 1;
            } else if tail {
                self.pool.free[i] = (b, end - b);
                i += 1;
            } else {
                self.pool.free.remove(i);
            }
            self.gm.free_phys(a - CONTIG_WINDOW, b - a);
            given += b - a;
        }
        if given != 0 {
            if self.pool.len >= first_len + given {
                self.pool.len -= given;
            }
            self.pool.given_logged += 1;
            let n = self.pool.given_logged;
            if n <= 4 || n.is_power_of_two() {
                log::info!(target: "kernel", "pool: {} KiB with nothing in them back to RAM (#{n})", given >> 10);
            }
        }
        given
    }
}

xb_types::state_fields!(Pool { first, len, free, ex, ob, lost });
