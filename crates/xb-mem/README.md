# xb-mem

Layer L1. Depends on `xb-types`, `log` and (on Unix hosts) `libc`.

## What it owns

| Type | What | Threads |
|---|---|---|
| `Ram` | Guest physical RAM: one shared memory object (`memfd_create` on Linux) mapped once read-write in the host. A cheap `Clone` handle implementing `xb_types::mem::PhysMem`; exposes the descriptor for CPU backends and a raw pointer for the vendored C. | any |
| `AddressSpace` | The guest's page table (one `u32` entry per 4 KiB page of the 4 GiB space), the reverse map from physical page to virtual address, host access by guest virtual address, and the allocators that relocate pages. | guest CPU |
| `PhysAllocator` | Which RAM pages are in use and which are pinned. | guest CPU |
| `WriteTracker` | Per-physical-page write tracking with the reference's semantics, over an `Arc<dyn PageProtector>`. | any |

It does not own policy: regions, reserve/commit, which window a contiguous
block may take and when to fall back to compaction are the kernel's
(`xb-kernel`).

## Host access to guest memory

This crate never dereferences a guest virtual address and keeps no
per-virtual-address host mappings: the CPU backend enforces the guest's view
through `Mapper` (mappings) and `PageProtector` (write tracking), and host
code reaches guest virtual memory by walking the page table into the RAM
view.

Consequences:

- Host access never faults. `AddressSpace::read`/`write` stop at the first
  page that is not RAM and return `MemFault { va, done }`.
- Host access ignores the guest's page protection and does not tell write
  tracking. A store made on the guest's behalf is reported with
  `WriteTracker::host_store` over `AddressSpace::phys_runs`; a device write
  with `WriteTracker::set_dirty`.
- A `PageProtector` covers every guest mapping of a physical page at once,
  so write tracking has no per-view state.

## Invariants

- Every page-table change is mirrored to the `Mapper` passed to the call, and
  nothing else changes the mapper. A refused request (`MapError`) changes
  neither.
- A RAM entry always points inside RAM.
- `AddressSpace::va_of(pa)` only names an address that maps the page now.
  Window mappings (`map_window`: `0x8000_0000`, `0xF000_0000`) are never
  named, so a page reached only through a window never moves.
- `PhysAllocator::free_pages()` equals the number of unused pages.
- Write tracking: the guest's access to a physical page is `None` if the page
  is without access, else `Read` if it is clean, else `ReadWrite`. State and
  protection change together under one lock. A store racing with
  `test_and_clear` is never lost.
- No `TrackerEvents` callback runs with the tracker's lock held.

## What a `PageProtector` must not do

`WriteTracker` calls `protect` while holding its lock, and `on_fault` takes
that lock. A protector must not wait for the guest-CPU thread to return from
`Bus::page_fault`; a CPU thread inside the handler is already outside guest
code.

## Data races on RAM

RAM is shared with the running guest and with device threads, as on hardware
(DMA). Every access in this crate is a raw, bounds-checked copy between the
mapping and a caller's buffer; no Rust reference into the mapping exists. A
copy racing with a guest store sees a mix of old and new bytes. Stop the guest
for a consistent snapshot.

## `unsafe`

Allowed, and confined to two places; the crate root has
`#![deny(unsafe_code)]` and these two modules opt in:

- `src/host/linux.rs`: `memfd_create`, `mmap`, `munmap`, `Send`/`Sync` for
  the mapping.
- `src/ram.rs`: the raw copies into and out of the mapping.

Another host adds a module under `src/host/` exporting the same `SharedMem`
(Windows: `CreateFileMappingW` / `MapViewOfFile`) and the matching accessor
on `Ram` next to `Ram::fd`.

## Tests

```
cargo test -p xb-mem
```

The tests use a mock `Mapper` that rebuilds the page table from the calls it
receives and compares it with the real one, and a mock `PageProtector` that
models guest stores. No game image is needed.
