# Architecture

## What the core is

A title is loaded straight from its disc image and run on a guest CPU. The Xbox
kernel (`xboxkrnl.exe`) is not guest code: its exports are implemented on the
host. Hardware the title's own statically linked libraries drive directly (the
NV2A GPU, the MCPX audio processor, the USB controllers) is emulated at the
level those libraries use it. There is no BIOS, no boot ROM and no dashboard.

## Threads

| Thread | Runs | Rules |
|---|---|---|
| guest CPU | guest code, kernel exports, device register handlers, interrupt dispatch | the only thread that touches the `Cpu`, the kernel state and the guest's address space. `xb-core` creates it at boot; the machine is built on it |
| GPU | the vendored renderer's decoder and Vulkan back end | talks to the machine only through the host interface in `xb-gpu` |
| APU | audio frames (voice processor, DSP) | reads and writes guest RAM by physical address |
| I/O workers | slow file and disc reads | complete requests back to the guest CPU thread |
| front end | window, input, audio output | calls the `xb-core` API; the thread that calls `Core::frame` is the presenting thread |

Other threads reach the guest CPU thread in two ways only: asserting an
interrupt line (`xb_types::irq::IrqHub`), or queueing work and kicking the CPU
(`xb_cpu::Kicker`, through the kernel's `WakeHandle`). Pausing and stopping
are requests of that second kind (`RunControl`): the front end's thread asks,
`Kernel::run` returns on the guest CPU thread, and that thread waits to be
let go.

The console has one processor, so there is one guest CPU thread. Guest threads
are contexts the kernel's scheduler switches between.

## Memory

- **Physical RAM** is one shared memory object (`xb-mem`), 64 MiB on a retail
  console. The host always has a full, never-faulting view of it (`PhysMem`).
  The kernel, devices and the GPU use that view.
- **The guest's address space** is a page table the kernel maintains in
  `xb-mem` (one entry per 4 KiB page: physical address and flags). Every change
  is mirrored to the CPU backend through `xb_types::mem::Mapper`. The host side
  reads guest virtual addresses by walking that table into the physical view;
  it never dereferences a guest virtual address directly. The kernel's table
  belongs to the guest CPU thread; `xb-core` wraps the CPU backend and keeps a
  copy of what passes through `Mapper` that any thread may read (the
  renderer's threads read guest memory by virtual address).
- **Device registers** are pages mapped with `Mapper::map_mmio`; the CPU backend
  forwards each access to `xb_cpu::Bus`.
- **Write tracking.** The GPU needs to know which RAM pages the guest wrote
  (textures, vertex data). `xb-mem` keeps the per-page tracking state and uses
  `xb_types::mem::PageProtector`, which the CPU backend provides, to take write
  (or all) access away from the guest for a physical page. The guest's next
  access calls `Bus::page_fault`; the tracker records it, restores access and
  the instruction is retried. `PageProtector` is callable from any thread and
  is synchronous.

Layout facts every crate may rely on:

| Guest virtual | What |
|---|---|
| `0x0001_0000` | the title's XBE image |
| `0x8000_0000` | all of physical RAM, contiguous (`CONTIG_WINDOW + pa`) |
| `0xD000_0000` | kernel-allocated stacks and system memory |
| `0xF000_0000` | NV2A's view of RAM |
| `0xFD00_0000` | NV2A registers (16 MiB) |
| `0xFE00_0000` | the other devices' windows, mapped as one run up to `0xFFFE_0000`; an address no device below claims is a register file that holds what is written |
| `0xFE80_0000` | MCPX APU registers |
| `0xFEC0_0000` | AC'97 registers |
| `0xFED0_0000` | USB OHCI controller 0 |
| `0xFEF0_0000` | network controller |
| `0xFFFE_0000` | 3 pages reserved by the KVM backend; never mapped by the kernel |

Guest physical `0xE000_0000` to `0xE040_5000` is reserved by the KVM backend
(its page tables); no device sits there.

## CPU backends

`xb-cpu` defines the `Cpu` trait. Two backends implement it:

- **`xb-cpu-kvm`** runs the guest in a hardware VM. It is used when `/dev/kvm`
  can be opened read-write and offers what the backend needs; the check is made
  at boot, like choosing a GPU. Guest code runs natively, so x87, SSE and
  self-modifying code need no emulation.
- **`xb-cpu-x64`** translates guest x86-32 code to x86-64 host code. It needs
  nothing from the host OS and is the fallback on x86-64 hosts.

A later backend (interpreter plus a portable JIT) covers arm64 hosts.

`Settings::cpu` chooses: `auto` prefers KVM and falls back to the translator.
A front end can override the choice for one run (`CoreConfig::cpu_override`)
without changing what is saved.

The two backends are tested against each other: the same guest code and
initial context must give the same final context and memory.

### Kernel entry

The title calls kernel exports through its import table. The loader points each
import at an address in the **trap range**, a block of guest addresses the
kernel reserves (`Cpu::set_trap_range`). When the guest's instruction pointer
enters the range, `run` returns `Exit::Trap` with `eip` naming the export. The
kernel reads arguments from the guest stack, does the work, sets `eax`, pops the
return address and arguments, and resumes. The kernel also uses trap addresses
as return sentinels when it calls guest code (thread start routines, interrupt
service routines, DPCs, APCs).

### Interrupts

Devices assert lines on the `IrqHub`, which kicks the CPU. `run` returns
`Exit::Kicked`; if the guest has interrupts enabled the kernel dispatches the
connected service routine by building a call frame on the guest stack. If
not, the kernel resumes with `RunFlags::exit_when_interruptible`.

## Crates

| Crate | Owns |
|---|---|
| `xb-types` | shared vocabulary |
| `xb-formats` | CCI, XDVDFS, XBE parsing |
| `xb-testkit` | test support: finds the game images in the titles stash |
| `xb-mem` | RAM, page table, physical page allocator, write tracking |
| `xb-fs` | the Xbox I/O namespace over host files and the disc |
| `xb-cpu` | CPU interface |
| `xb-cpu-kvm` | KVM backend |
| `xb-cpu-x64` | translator backend |
| `xb-kernel` | kernel exports, objects, scheduler, memory manager |
| `xb-devices` | OHCI, hub, XID pad, legacy ports, AC'97, NIC stub |
| `xb-gpu` | NV2A register front end (Rust) and the renderer (vendored C) |
| `xb-apu` | MCPX APU (vendored C) with a Rust interface |
| `xb-menu` | in-game menu state and its RGBA overlay |
| `xb-core` | the machine and the public API |
| `apps/xbemu` | window, pads, audio output |

### Vendored C

`vendor/xbased-hle` (the GPU renderer, about 40k lines) and `vendor/xbased-apu`
(about 25k lines) are the only C in the tree. Each is built by its crate's
`build.rs` and reached through one Rust interface. The renderer includes
nothing from the rest of xbased; it calls the machine through `host.h`, which
`xb-gpu` implements in Rust.

Both are to be replaced with Rust, piece by piece (`ROADMAP.md`).

## The menu

The menu is part of the core so every front end gets it. A front end
forwards pad state and says when to open it: `Core::set_menu_open` from a
button or key of its own that the console's controller does not have (the
standalone app: a gamepad's Guide button, F1 or Escape). A front end with no
such button sets `Settings::menu_combo` instead, console buttons that held
together for half a second open the menu; `xb-menu` watches the pad for
them. No combo (0) is the default. While the menu is open
the guest is paused, the title sees a released pad, and the core composites the
menu's RGBA image over the last frame. The D-pad moves, A changes or enters, B
goes back.

Pausing is the core's: it asks the kernel to pause, waits until the guest CPU
has left `Kernel::run` (guest time then stands still), and only then holds
the GPU's vertical blank and the APU's frame thread, so a guest that was
waiting for the renderer is not left waiting. The frame-rate line, when on,
is drawn by the same overlay while the menu is closed. With the guest paused
no new frame arrives, so the presenter draws a refresh for the overlay's
sake whenever an overlay image is new to it.

## Front-end API

`xb-core` exposes one type that boots a disc image and runs it. A front end
supplies a way to create the Vulkan surface, pad state, an audio sink and a
directory for the console's hard disk and settings. The standalone app and a
future libretro core are both thin shells over it.

The front end calls `Core::boot` and then `Core::frame` once per display
refresh on one thread; `frame` takes the pads, runs the menu, feeds the
title's controller ports, keeps the overlay current and presents. The data
directory holds `hdd/` (the hard disk's partitions), `eeprom.bin`, `cache/`
(the renderer's caches) and `settings.txt`. `docs/development.md` has the
layout and the hooks for automated runs.
