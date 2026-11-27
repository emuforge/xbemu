//! Guest physical RAM, the guest address space's page table, the physical
//! page allocator and write tracking.
//!
//! One structural difference from the reference: it reserves the guest's
//! 4 GiB in the host process and dereferences guest virtual addresses. Here
//! the host never does. The CPU backend enforces the guest's view
//! ([`xb_types::mem::Mapper`], [`xb_types::mem::PageProtector`]); this crate
//! keeps the authoritative bookkeeping and the host's own never-faulting view
//! of RAM.
//!
//! * [`Ram`]: physical RAM, one shared memory object mapped once in the host.
//!   A cheap handle usable from any thread ([`xb_types::mem::PhysMem`]).
//! * [`AddressSpace`]: the guest's page table. Every change is mirrored to a
//!   `Mapper`; host code reads and writes guest virtual addresses by walking
//!   it into the RAM view. Guest-CPU thread only.
//! * [`PhysAllocator`]: which pages of RAM are in use, and which are pinned.
//!   Guest-CPU thread only.
//! * [`WriteTracker`]: per-physical-page write tracking for the GPU, with the
//!   reference's semantics. Any thread.
//!
//! `unsafe` is confined to [`ram`] (raw copies into the mapping) and the
//! host module (creating and mapping the memory object).
#![deny(unsafe_code)]

mod host;
pub mod phys;
pub mod ram;
pub mod space;
pub mod track;

#[cfg(test)]
mod testing;

pub use phys::{PhysAllocator, PhysStats, PinState, TOP_RESERVE};
pub use ram::Ram;
pub use space::{AddressSpace, Contig, ContigRequest, MapError, MemFault, Pte, RelocateHooks};
pub use track::{FaultOutcome, TrackerEvents, TrackerStats, WriteTracker};
pub use xb_types::mem::{Access, Mapper, PageProtector, PhysMem};
pub use xb_types::{PAGE_SHIFT, PAGE_SIZE};
