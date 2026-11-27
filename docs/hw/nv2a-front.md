# The NV2A front end and the renderer's host interface

`xb-gpu` is two things joined at one C header. The **register front end**
(Rust) is the NV2A as the guest's Direct3D sees it. The **renderer** (C,
`vendor/xbased-hle`) decodes the push buffer and draws with Vulkan. The
renderer asks for everything through `hw/xbox/hle/host.h`; `xb-gpu`
implements that header and forwards what only the machine can answer to the
`GpuHost` trait.

## Register front end

The window is 16 MiB at physical `0xFD00_0000`. It is a register file: a
read returns what was last written, except for the registers below. All
accesses are little endian; sizes 1, 2 and 4 are accepted at any offset.

| Register | Offset | Read | Write |
|---|---|---|---|
| `PMC_BOOT_0` | `0x000000` | `0x02A000A1` | stored |
| `PMC_INTR_0` | `0x000100` | summary: bit 24 PCRTC, 20 PTIMER, 12 PGRAPH, each "pending and enabled in its unit" | ignored |
| `PMC_INTR_EN_0` | `0x000140` | stored | stored; line re-evaluated |
| `PFIFO_RUNOUT_STATUS`, `PFIFO_CACHE1_STATUS` | `0x002400`, `0x003214` | bit 4 (empty) when the FIFO is idle, else 0; any width | stored |
| `PFIFO_CACHE1_DMA_PUSH` | `0x003220` | stored value with bit 4 (busy) set only while the FIFO is not idle | stored |
| `PFIFO_CACHE1_DMA_PUT`/`GET` | `0x003240`/`0x003244` | stored (the decoder updates GET) | stored; a GET write resyncs the decoder |
| `PFIFO_CACHE1_DMA_SUBROUTINE` | `0x00324C` | stored | stored; decoder told |
| `PVIDEO_BUFFER` | `0x008700` | stored | bits ORed in; bit 0 shows the overlay |
| `PVIDEO_STOP` | `0x008704` | stored | stored; bit 0 clears `BUFFER` and hides the overlay |
| `PTIMER_INTR_0` | `0x009100` | bit 0: alarm pending | write 1 to clear |
| `PTIMER_INTR_EN_0` | `0x009140` | stored | stored; line re-evaluated |
| `PTIMER_TIME_0` | `0x009400` | guest time in ns, low word, low 5 bits zero | stored |
| `PTIMER_TIME_1` | `0x009410` | guest time, bits 32..60 | stored |
| `PTIMER_ALARM_0` | `0x009420` | stored | stored; arms the alarm |
| `PFB_CSTATUS` | `0x10020C` | RAM size in bytes | stored |
| `PFB_WBC` | `0x100410` | 0 (Riddick spins on it) | stored |
| `PGRAPH_INTR` | `0x400100` | stored (the decoder sets trap bits) | write 1 to clear; decoder kicked |
| `PGRAPH_INTR_EN` | `0x400140` | stored | stored; decoder kicked |
| `PGRAPH_INCREMENT` | `0x40071C` | stored | bit 1: the flip read counter in `PGRAPH_SURFACE` (bits 24..26) advances modulo bits 28..30; decoder told of the flip. Never stored |
| `PGRAPH_FIFO` | `0x400720` | stored | stored; decoder kicked |
| `PCRTC_INTR_0` | `0x600100` | bit 0: vblank pending | write 1 to clear |
| `PCRTC_INTR_EN_0` | `0x600140` | stored | stored; line re-evaluated |
| `PCRTC_START` | `0x600800` | stored | stored; the display shows that frame buffer |
| `PCRTC_RASTER` | `0x600808` | scanline (0..524) since the last vblank | stored |
| `PRMDIO` palette | `0x6813C8`, `0x6813C9` | stored | byte writes: index, then R, G, B per entry; the 256th entry applies the table as the gamma ramp |
| `NV_USER` `DMA_PUT`/`GET`/`REF` | `0x800040`/`44`/`48` | CACHE1's `DMA_PUT`/`GET`/`REF` | stored into CACHE1's; **a `DMA_PUT` write is the kick** |

The rows involving the decoder apply in **push-buffer mode**, which the
renderer turns on at title start (`hle_host_pb_mode`). Before that, and
after `Gpu::shutdown`, those registers are plain storage, PGRAPH raises no
interrupt and the FIFO always reads idle. "Idle" in push-buffer mode means
CACHE1's GET equals its PUT: the decoder has consumed everything.

### The kick

A write to channel 0's `DMA_PUT` runs on the guest-CPU thread:
`hle_pb_pushed(put)` (the decoder's scout reads what was pushed, for the
visibility-report gate), the value is stored in CACHE1's `DMA_PUT`, then
`hle_pb_put()` wakes the decode thread. The write returns without waiting
for the GPU, except that the renderer may ask the machine to hold the guest
thread until the next flip (`GpuHost::frame_wait`, frame latency).

### The interrupt line

The NV2A has one line, `Irq::Gpu` (3). Its level is
`PMC_INTR_EN_0 bit 0 && PMC_INTR_0 != 0`. Four parties change the inputs:
the vblank thread, the alarm thread, the decoder (a software trap sets a
`PGRAPH_INTR` bit and calls `hle_host_gpu_irq_update`) and the guest (its
handler clearing what it handled). Each changes a register and then calls
`update_irq`, which reads the registers and sets the line under one mutex,
so a level computed from older register values can never be applied after
a newer one.

### Vertical blank

A host thread (`nv2a-vblank`, started by `Gpu::new`) raises `PCRTC_INTR_0`
bit 0 once per frame of the video mode on the host's monotonic clock:
16.683333 ms (59.94 Hz), or 20 ms after `AvSetDisplayMode` selects a 50 Hz
mode (`Gpu::av_set_display_mode`). It also counts vblanks, records the
instant for `PCRTC_RASTER` and the renderer, and tells the decoder
(`hle_pb_vblank`). As in the reference, this thread drives the timing; the
guest CPU thread only sees the interrupt.

- If the thread wakes more than a period late, the vblanks it missed are
  dropped and the grid kept, rather than sent back to back.
- While paused (`Gpu::set_paused(true)`) no vblank is raised; on resume the
  grid restarts from now.
- If something reports the host display's refreshes (`Gpu::display_vsync`,
  or a presenter through `hle_host_display_sync`) and a frame is within 1 %
  of one to four refresh periods, the vblank locks to the display, nudged
  at most 0.5 ms per vblank. Nothing reports them on Linux, so the vblank
  runs free and the presenter shows frames as they come.

### Flips

Direct3D flips in its vblank handler (the `PGRAPH_INCREMENT` write) or, for
IMMEDIATE presents, in a software trap's handler. The renderer asks when a
flipped frame is due on screen (`hle_host_flip_vblank_ns`: the last vblank,
or the next one for a flip in the last quarter of a frame) and paces trap
flips (`hle_host_flip_min_ns`: one vblank period; `hle_host_flip_slot`).

### PTIMER's alarm

`PTIMER_TIME` is the guest's clock (`GpuHost::guest_time_ns`), which the
machine stops while the guest is paused or held. After the first write to
`ALARM_0` a host thread (`nv2a-ptimer`) sets `PTIMER_INTR_0` bit 0 when the
time's low word reaches the alarm, and again each time it wraps (4.3 s). An
alarm less than a second in the past fires at once. Direct3D uses it for its
timer callbacks and, in some video modes, its vblank work.

### Not implemented

- The "uncapped" frame-rate modes (early vblanks while PGRAPH waits in
  FLIP_STALL, frame limits, fast forward) and the per-title pace table that
  goes with them. The vblank always runs at the video mode's rate.
- The register census and the GPU event trace (diagnostic builds).
- Savestates.
- `shapes.txt` (a title's picture shape remembered across runs, for old
  savestates) and the vblank multiplier for mods.
- The catch-all register file for the other device windows (`0xFE000000`):
  those devices belong to `xb-devices` and `xb-apu`.

## Host interface

`host.h` declares the functions below. All are exported from
`crates/xb-gpu/src/ffi.rs`. "Front end" means answered inside `xb-gpu`;
otherwise the `GpuHost` method it forwards to.

| `host.h` | Answered by |
|---|---|
| `hle_host_cpu` | `GpuHost::cpu_regs`, copied into a static snapshot |
| `hle_host_kick` | `GpuHost::kick` |
| `hle_host_guest_hold` | `GpuHost::guest_hold` |
| `hle_host_code_changed`, `hle_host_code_flushes` | `GpuHost::code_changed`, `code_flushes` |
| `hle_va_rw` | `GpuHost::va_read` / `va_write` |
| `hle_va_store` | `GpuHost::va_store` |
| `hle_host_mem_dirty`, `_open`, `_test_and_clear`, `_set_dirty`, `_landed`, `_rearm`, `_noaccess` | the `GpuHost::mem_*` method of the same name |
| `hle_host_vblank_count`, `_vblank_period_ns`, `_vblank_ns`, `_flip_vblank_ns`, `_flip_min_ns`, `_flip_slot`, `_display_sync`, `_vblank_timed` | front end (`timing.rs`) |
| `hle_host_frame_wait`, `hle_host_frame_flipped` | `GpuHost::frame_wait`, `frame_flipped` |
| `hle_host_gpu_reg`, `_reg_set`, `_reg_mask` | front end (the register file) |
| `hle_host_gpu_irq_update` | front end (`update_irq`) |
| `hle_host_pb_mode` | front end |
| `hle_host_gpu_lost` | recorded; `Gpu::fatal()` reports it |
| `hle_host_base_path` | `GpuConfig::cache_dir` |
| `hle_host_render_scale`, `hle_host_anisotropy` | `GpuConfig`, `Gpu::set_render_scale`, `set_anisotropy` |
| `hle_host_vk_loader` | NULL (the system's loader) |
| `hle_host_disc_stats` | `GpuHost::disc_stats` |
| `hle_host_gpu_trace`, `_trace_mark`, `_trace_dump` | not built: they exist only with `XBASED_DIAG=1` |

Added by this tree (`vendor/xbased-hle/PATCHES.md`): `hle_host_log` (the
renderer's log lines, to the `log` crate under target `gpu`),
`hle_host_surface_extensions` and `hle_host_surface_create` (the window's
Vulkan surface from `SurfaceProvider`), and `__system_property_get` (the
renderer's debug properties, read from the environment).

### Write tracking

The renderer reads textures, vertex data and frame buffers out of guest RAM
and needs to know when the guest writes them. The tracker lives in `xb-mem`
and works on physical pages; `xb-gpu` only relays, in both directions:

- **Renderer to tracker**: the `hle_host_mem_*` family, forwarded to
  `GpuHost::mem_*` with the reference's semantics (a page is clean and
  write-protected, or written; `noaccess` additionally takes read access).
- **Tracker to renderer**: `MemEvents` (from `Gpu::mem_events()`), which the
  machine calls from its page-fault path and its own RAM writes:

| When | Call | C function |
|---|---|---|
| guest store faults on a clean page, before the page is opened | `first_write(pa, len)` | `hle_mem_first_write` |
| guest access faults on a page without access | `stale_neighbour(lo, hi)` if the access span is known, then `stale_access(pa, write)` unless it said `Emulate` (host performs the access) or `Reopened` (retry) | `hle_mem_stale_neighbour`, `hle_mem_stale_access_rw` |
| inside `mem_set_dirty` and `mem_landed`, before protection changes | `host_wrote(pa, len)` | `hle_mem_host_wrote` |
| inside `mem_set_dirty` when a page of the range has no guest access | `unguard_range(pa, len)` | `hle_barrier_unguard_range` |
| the kernel frees guest memory | `freed(pa, len)` | `hle_barrier_freed`, `hle_tl_land_freed` |
| a worker thread will write guest RAM later (overlapped read) | `write_fence(pa, len)`, then `wait_fence(v)` on the worker | `hle_mem_write_fence`, `hle_mem_wait_fence` |

`first_write` and `stale_access` can wait for GPU work on the calling
(guest-CPU) thread; the renderer brackets the wait with
`GpuHost::guest_hold(true/false)` so the machine can keep the wait out of
guest time. `stale_access` gives the page back itself by calling
`GpuHost::mem_noaccess(.., false)`.

`MemEvents` is inert until `Gpu::new` has initialized the renderer, so the
tracker can be wired before the GPU exists.

Not relayed: `hle_pbd_keep` / `hle_pbd_keep_wait` (memory a title persists
for the next title it launches, `MmPersistContiguousMemory`).

## Lifecycle

1. `Gpu::new(host, ram, ram_size, irq, config, surface)`: registers at
   power-on values, the vblank and alarm threads start, the renderer's
   memory side is initialized (`hle_guestmem_init`, `hle_init`). No Vulkan.
2. The machine maps the window (`Mapper::map_mmio`) and forwards accesses.
3. `Gpu::title_start()` on the guest-CPU thread once the XBE is mapped at
   `0x00010000`: the renderer reads the XBE headers through
   `GpuHost::va_read`, creates the Vulkan instance (with the surface's
   extensions) and device, loads the title's caches, optionally prebuilds
   its recorded pipelines, starts its render, timeline and decode threads
   and turns push-buffer mode on. Errors come back as `Fatal`.
4. The kernel's `AvSetDisplayMode` calls `Gpu::av_set_display_mode`.
5. The front end's presenting thread loops on `Presenter::present(w, h)`.
6. `Gpu::shutdown()` after the guest CPU has stopped.

## Presentation

`Presenter::present(width, height)` is one refresh of the window, called in
a loop on one thread of the front end's choosing. It creates the surface
through `SurfaceProvider` on first use, makes the swapchain again when the
size changes, and draws: the newest flipped frame letterboxed to the aspect
setting (4:3 for standard-definition frames, their own shape for HD frames,
16:9, or stretched), FXAA if set, the gamma ramp, the frame-rate digits if
set, then the overlay. With nothing new to draw it waits up to 4 ms for a
frame instead of redrawing.

The overlay (`Presenter::set_overlay`) uses the mechanism the reference's
own in-game interface is drawn with (`hle_present_set_overlay`, `hle_ov_tex_*`
in `vk/swapchain.c`): one texture holding the RGBA image (straight alpha)
and one quad over the picture's rectangle, composited by the presenter
after the picture. While an overlay is set every `present` draws, so the
menu stays live while the guest is paused and no new frames arrive. Before
the title has shown a frame the overlay takes the rectangle a 640x480
picture would.
