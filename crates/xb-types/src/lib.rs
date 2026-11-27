//! Shared vocabulary of the xbemu core: addresses, pad state, settings,
//! interrupt lines and the memory traits the CPU backends implement.
//!
//! This crate sits at the bottom of the workspace. It depends on nothing and
//! holds only what two or more crates must agree on.
#![forbid(unsafe_code)]

pub mod irq;
pub mod mem;
pub mod pad;
pub mod settings;
pub mod state;
pub mod thread;

/// Size of a guest page. The Xbox's Pentium III uses 4 KiB pages (4 MiB large
/// pages exist only in the kernel's own mappings, which this core models as
/// runs of small pages).
pub const PAGE_SIZE: u32 = 4096;
/// `log2(PAGE_SIZE)`.
pub const PAGE_SHIFT: u32 = 12;

/// Physical RAM of a retail console.
pub const RAM_RETAIL: u32 = 64 << 20;
/// Physical RAM of a debug kit or a 128 MiB-modded console.
pub const RAM_DEBUG: u32 = 128 << 20;

/// Guest virtual address where all of physical RAM is mapped contiguously
/// (`MmAllocateContiguousMemory` returns addresses in this window; physical
/// address `p` is at `0x8000_0000 + p`).
pub const CONTIG_WINDOW: u32 = 0x8000_0000;

/// Rounds `v` up to a multiple of `a`, which must be a power of two.
#[must_use]
pub const fn align_up(v: u32, a: u32) -> u32 {
    (v.wrapping_add(a - 1)) & !(a - 1)
}

/// Rounds `v` down to a multiple of `a`, which must be a power of two.
#[must_use]
pub const fn align_down(v: u32, a: u32) -> u32 {
    v & !(a - 1)
}
