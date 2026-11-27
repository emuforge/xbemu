# xb-kernel

Layer L2. The Xbox kernel (`xboxkrnl.exe`) reimplemented on the host, and the
title loader. Depends on `xb-types`, `xb-cpu`, `xb-mem`, `xb-formats`, `xb-fs`
and `log`; `xb-cpu-kvm` is a dev-dependency for the guest-code tests.

## What it owns

| Module | What |
|---|---|
| `kernel` | the kernel's area in guest memory, export dispatch, callouts, processor exceptions, the clock tick |
| `run` | the run loop, outcomes, the fatal report, console configuration |
| `clock` | guest time, the wake handle, the stop/pause handle, the clock thread |
| `gm` | every access to guest memory, the queue of mapping changes |
| `ps` | threads and the scheduler |
| `ke` | time, dispatcher objects, waits, IRQL, completion ports |
| `timer`, `intr`, `apc` | timers and DPCs, interrupt objects and delivery, APCs |
| `ob`, `pool` | objects, handles, named objects; the kernel pool |
| `seh` | `RtlRaiseException`, `RtlUnwind`, exception dispatch |
| `mm` | virtual, contiguous and system memory, protection, kernel stacks |
| `ex` | pool exports, the EEPROM, sections, AV, HAL, data exports |
| `io` | files, devices, symbolic links over `xb-fs`; the disc adapter; I/O workers |
| `rtl`, `xc`, `xc_crypto` | runtime library; crypto exports and primitives |
| `fixups` | patches to known library code in titles |
| `loader` | XBE loading and the boot sequence |
| `exports`, `fast` | the export table; exports that run as guest code |

Not supported: a title editing its own page tables (the guest's CR3 belongs
to the CPU backend). `STUBS.md` lists exports that do less than the console's;
`docs/hw/kernel-exports.md` has every export's status and the guest-memory
layout.

## Public API

```rust
// Construction and boot (guest-CPU thread).
KernelConfig::new(ram: xb_mem::Ram, irq: Arc<IrqHub>) -> KernelConfig
    // fields: tracker: Option<Arc<WriteTracker>>, relocate_hooks: Box<dyn RelocateHooks + Send>,
    //         console: ConsoleConfig, fast_paths: bool
Kernel::new(cfg: KernelConfig, cpu: &mut dyn Cpu) -> Result<Kernel, InitError>
Kernel::boot(&mut self, cpu: &mut dyn Cpu, opts: BootOptions) -> Result<BootInfo, BootError>
ram_size_for(disc: Option<&Arc<dyn BlockSource>>) -> u32   // 64 or 128 MiB, before the RAM exists
Kernel::map_device(&mut self, va: u32, pa: u32, len: u32) -> bool
Kernel::sync_cpu(&mut self, cpu: &mut dyn Cpu)

// Running.
Kernel::run(&mut self, cpu: &mut dyn Cpu, bus: &mut dyn Bus, platform: &mut dyn Platform) -> RunOutcome
    // RunOutcome::{Stopped, Paused, Exit(TitleExit), Fatal(Box<FatalReport>)}
Kernel::report(&self, reason: &str) -> FatalReport       // on demand (a watchdog)

// Handles for other threads (all Clone + Send + Sync).
Kernel::wake_handle(&self) -> WakeHandle     // .wake(): kick the CPU and end an idle wait
Kernel::control(&self) -> RunControl         // .request_stop(), .request_pause()
Kernel::clock(&self) -> GuestClock           // .now_ns(), .is_still(), .host_delay(guest_ns),
                                             // .hold_begin(), .hold_end(), .take_hold_stats()

// Guest memory by virtual address (guest-CPU thread, outside run).
Kernel::read_guest(&self, va: u32, buf: &mut [u8]) -> bool
Kernel::write_guest(&mut self, va: u32, data: &[u8]) -> bool
Kernel::translate(&self, va: u32) -> Option<u32>

// Export table.
exports::all() / by_ordinal(u32) / by_name(&str);  Kernel::resolve(ordinal) / implemented(ordinal)
```

`Platform` (in `platform.rs`) is what the kernel asks of the machine: `tick`
(devices with deadlines), `set_display_mode`, `set_title_widescreen`,
`phy_link_state`, `phy_initialize`, `apu_mailbox_wait`, `ram_freed`, `smbus`.
Every method has a do-nothing default; `NullPlatform` is the empty machine.

### What the machine wires up

1. Create the RAM (`ram_size_for`), the CPU backend over it, the `IrqHub`,
   the write tracker over `cpu.protector()`.
2. `Kernel::new`, then `hub.set_kick(Box::new({ let w = kernel.wake_handle(); move || w.wake() }))`.
3. `map_device` for each device window (identity: `0xFD00_0000` NV2A, ...).
4. `Kernel::boot`, then loop on `Kernel::run`.
5. `Bus::page_fault` is the machine's: ask `AddressSpace::allows` through its
   own bookkeeping or simply try the tracker (`WriteTracker::on_fault(pa)`)
   and answer `Raise` when it is not the tracker's; a raised fault comes back
   to the kernel as `Exit::Exception` and goes to the title's handlers.
6. A device that holds the guest-CPU thread inside a `Bus` callback (the
   renderer's first-write barrier) brackets the wait with
   `GuestClock::hold_begin` / `hold_end`.

## Threading rules

- `Kernel` is used on the guest-CPU thread only. It is `Send`.
- Other threads reach it through `WakeHandle`, `RunControl`, `GuestClock`
  and the `IrqHub` only.
- The kernel owns two kinds of helper threads: the clock thread (`xb-clock`,
  kicks the CPU when a timer, a wait's timeout, the running thread's slice or
  the machine's `tick` deadline is due, and once a millisecond while guest
  code runs without entering the kernel) and the I/O workers (`xb-io-*`,
  which touch host files only, never guest memory, and hand results back
  through a queue plus `WakeHandle::wake`). All end when the kernel is
  dropped.
- Guest time runs only inside `Kernel::run`. Returning `Paused` or `Stopped`
  stops it; the next `run` goes on from where it stood.

## Invariants

- `#![forbid(unsafe_code)]`. `unsafe` is not allowed here.
- No host pointer to guest memory exists. Every access goes through `gm`,
  which walks `xb-mem`'s page table into RAM, reports stores to the
  `WriteTracker` (`host_store`) and loads of no-access pages (`host_load`),
  and turns a missing page into a `Fault` that the dispatcher raises as the
  title's access violation at the call.
- Mapping changes are queued and applied to the CPU backend before guest
  code next runs; freed RAM is reported to `Platform::ram_freed` then too.
- An export is entered by address: its thunk holds `TRAP_VA + ordinal`, or
  the address of guest code in the kernel's area that ends in a jump there.
- The kernel keeps nothing below a thread's stack pointer.
- Kernel state is plain owned data (no globals), so it can be serialized
  later.

## Tests

```
cargo test -p xb-kernel
```

- Unit tests run every subsystem over a scripted mock CPU (`src/testing.rs`).
- `tests/kvm.rs` runs hand-assembled guest programs under the real kernel on
  `KvmCpu`; each test prints a note and passes when `/dev/kvm` is unusable.
- `tests/title_boot.rs` boots a real title when the titles stash has
  `halo-ce` (`titles/README.md`), with a bus that answers zeros, and
  compares the stages and file opens with `halo-ce.ref.log` when the stash
  has that too:

```
cargo test -p xb-kernel --test title_boot -- --nocapture
```

  `XBEMU_TEST_LOG=info|debug|trace` prints the log (`trace`: every kernel
  call in the reference's `ktrace` shape); `XBEMU_TEST_SECONDS` sets the time
  limit.
