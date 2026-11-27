# xb-core

Layer L3. The machine: it wires every other crate into a console, boots a
title from a disc image and runs it. `Core` is the one API a front end uses.

## What it owns

| Module | What |
|---|---|
| `lib.rs` | `Core` and the front-end API; the per-frame work (menu, pads, overlay, present, statistics); pause and resume; settings and their file; shutdown |
| `guest.rs` | the guest-CPU thread: choosing the CPU backend, building the machine, booting the title, the run loop |
| `machine.rs` | the state every thread may reach (`Shared`) and the traits the leaf crates ask the machine to implement: `GpuHost`, `ApuHost`, `TrackerEvents`, `RelocateHooks`, the devices' clock and vblank counter; the shared copy of the guest's page table |
| `bus.rs` | `xb_cpu::Bus` (registers by physical address, ports, the page-fault policy, the TSC) and `xb_kernel::Platform` |
| `script.rs` | `XBEMU_SCRIPT`, a scripted session for automated runs |

## Public API

```rust
Core::boot(CoreConfig) -> Result<Core, BootError>
    // CoreConfig { disc, data_dir, settings: Option<Settings>, cpu_override: Option<CpuBackend>,
    //              surface: Arc<dyn SurfaceProvider>, audio: Arc<dyn AudioSink>, pads_connected: u8 }
Core::frame(&self, width, height, &[PadState; 4]) -> Frame { presented, state: CoreState }
Core::set_pad_connected(&self, port, connected)
Core::rumble(&self, port) -> Rumble
Core::set_menu_open(&self, open);  Core::menu_open(&self) -> bool
Core::screenshot(&self, path) -> bool          // .jpg, or a 32-bit BMP
Core::surface_changed(&self)
Core::settings(&self) -> Settings;  Core::set_settings(&self, Settings)
Core::info(&self) -> SessionInfo { title, title_id, cpu_backend, fps }
Core::shutdown(self)                           // also on drop
```

`Core::frame` is called in a loop on the thread that owns the window's
surface. `Frame::presented` is the presenter's answer: it is also `true` for
a refresh that had no new frame of the title to show, so it counts window
refreshes, not the title's frames (`SessionInfo::fps` is the title's rate).

## Threads

| Thread | Made by | Does |
|---|---|---|
| front end's | the front end | every `Core` method; presenting |
| `guest-cpu` | `Core::boot` | owns the CPU backend, the kernel, the OHCI, AC'97, NIC and port devices; runs `Kernel::run` |
| the kernel's, the GPU's, the APU's | those crates | see their READMEs |

`Core::boot` returns after the guest-CPU thread has built the machine and
booted the title; a failure there is the boot's error. The two threads share
`machine::Shared` (an `Arc`): atomics, the write tracker, the kernel's
handles, the GPU, and the pause protocol (`RunState` under a mutex with a
condition variable).

## What boot does, in order

1. Data directory: `hdd/` (the hard disk; the kernel keeps the EEPROM beside
   it, so `eeprom.bin` is in the data directory), `cache/` (the renderer's
   caches), `settings.txt`.
2. Opens the disc; RAM of `xb_kernel::ram_size_for` bytes.
3. On the guest-CPU thread: the CPU backend (`Settings::cpu`, or
   `CoreConfig::cpu_override`; Auto takes KVM when `KvmCpu::probe` and
   `KvmCpu::new` succeed, else the translator, and logs which and why), the
   write tracker over `cpu.protector()`, the kernel, the GPU, the APU, the
   register-level devices.
4. Device windows mapped: the NV2A's 16 MiB at `0xFD00_0000`, and
   `0xFE00_0000..0xFFFE_0000` as one run (the KVM backend keeps the pages
   above).
5. `Kernel::boot`, `Kernel::sync_cpu` (so the renderer can read the XBE's
   headers through the page table), `Gpu::title_start`, `Apu::start`.

## Invariants

- **The CPU backend, the kernel and the register-level devices are touched
  only on the guest-CPU thread.** Other threads use `Shared`.
- **The shared page table is what the CPU enforces.** `MachineCpu` wraps the
  backend and copies every `Mapper` call into `PageTable` before passing it
  on; nothing else writes that table. The renderer's reads by virtual address
  (`GpuHost::va_read`) walk it into RAM from any thread and never fault.
- **A page fault is the title's unless write tracking claims it.** No
  mapping, or an access the title's own protection forbids at an address that
  maps the faulting page: `Raise`. Otherwise `WriteTracker::on_fault` decides.
- **Register routing follows the reference** (`mmio.c`): the first window
  that holds the address, in the order NV2A, APU, OHCI, AC'97, NIC, then the
  catch-all register file over `0xFE00_0000..` (holds what is written, zero
  before). Of an access that reaches past a window's end the bytes inside go
  to the device one at a time and the rest read as ones. An address in no
  window reads as ones.
- **Pausing stops the guest first.** `request_pause`, wait for `Kernel::run`
  to return (the guest may be waiting for the renderer, which must keep
  working until then), then `Gpu::set_paused(true)` and `Apu::pause`. Guest
  time stands still while paused. Resuming goes the other way round.
- **Shutdown order:** the guest-CPU thread (stop and join; the kernel, CPU
  and devices are dropped there), the APU (stop, then dropped), the overlay
  and `Gpu::shutdown`, `tracker.set_events(None)`, the settings file.
- **The frame rate HUD is the core's overlay** (`xb_menu::draw_hud`); the
  presenter's own counter stays off.
- `GpuHost::guest_hold` acts only on the guest-CPU thread (a thread-local
  marks it) and nests; the outermost pair brackets `GuestClock::hold_begin` /
  `hold_end`, and time left out is passed to `Apu::guest_time_held`.

## The menu's "next boot" marks

CPU backend and Widescreen (a title reads the console's flag when it starts),
and Render scale: the renderer takes the new scale at once but uses it when
the title next creates its Direct3D device, which for Halo is the next boot.
Everything else applies while the title runs.

## Not done

- `GpuHost::frame_wait` (the renderer's frame-latency hold) only counts: the
  kernel has no hold-until-flip wait.
- `Apu::try_mmio_write`'s retry is not used: `xb_cpu::Bus::mmio_write` cannot
  ask for the store to run again, so a busy voice is waited for.
- `TitleExit::Relaunch` (a title starting another XBE) ends the session: it
  needs a new kernel and, because the renderer cannot be created twice, a new
  process.
- `Platform::smbus` answers 0 (the kernel answers the devices titles ask for).
- `GpuHost::disc_stats` and `code_changed` keep their defaults.
- Hosts other than Linux x86-64: `guest::make_cpu` and the monotonic clock in
  `machine.rs` are the two host-specific places.

## `unsafe`

Allowed. Five blocks, each with a `// SAFETY:` comment: `Gpu::new` and
`GuestRam::new` (handing the RAM mapping to the C), and three libc clock
calls in `machine.rs`.

## Logs

Target `core`, INFO: the data directory, the disc, the CPU backend and why,
the boot, the first frame on screen, the menu, pause and resume with the
guest's time, settings changes, screenshots, a statistics line every five
seconds, the end of the session. DEBUG: the first 400 device register
accesses in the reference's shape (`mmio: nv2a r+000100 -> 00000000 (4)`,
to compare with its register log) and page faults that are not write
tracking's.

## Tests

```
cargo test -p xb-core
cargo test -p xb-core --test title_session -- --nocapture
```

Unit tests cover the shared page table, the fault policy's table check, the
relocation hooks, register routing, the register file, the script, the
overlay's size and the settings file. `tests/title_session.rs` boots a real
title through the public API without a window (it needs `/dev/kvm` or the
translator and a Vulkan device), waits for a steady frame rate, pauses and
resumes it through the menu, checks that the stick clicks open the menu only
once `menu_combo` names them, changes a setting and shuts down. It passes
with a printed note when the titles stash has no `halo-ce`
(`titles/README.md`).
