# Changes to the vendored files

The base is xbased 1.2.12 with one later fix (`README.md`).
`crates/xb-gpu/build.rs` defines `XBEMU_HOST=1`. Changes 1 to 13 and 17
to 20 are each inside `#if defined(XBEMU_HOST)` (or widen an existing `#if`
with it); without the define those compile as xbased's do. Changes 14 to 16 are not
yet held to that: not all of their lines are inside the `#if`. Two C files
were added, both xbemu's own, under `shim/` (change 7).

## 1. The window's surface comes from the host, not SDL

xbased's Linux build gets its Vulkan surface from an SDL window. xbemu's
front end must not need SDL: it creates the `VkSurfaceKHR` however it likes
(`xb_gpu::SurfaceProvider`).

- `hw/xbox/hle/vk/native_window.h`: declares the two host functions that
  replace the SDL calls,
  `const char *const *hle_host_surface_extensions(uint32_t *count)` and
  `bool hle_host_surface_create(void *inst, void *surf)`. They are declared
  here rather than in `host.h` so `host.h` stays identical to upstream.
- `hw/xbox/hle/vk/vkdev.c`:
  - includes `native_window.h` instead of `<SDL3/SDL_vulkan.h>`;
  - `hle_vk_init` asks `hle_host_surface_extensions` for the instance
    extensions instead of `SDL_Vulkan_GetInstanceExtensions`. The two loops
    over them use `uint32_t` instead of SDL's `Uint32` (the same type).
- `hw/xbox/hle/vk/swapchain.c`:
  - does not include `<SDL3/SDL_vulkan.h>`;
  - `hle_present_begin` calls `hle_host_surface_create(g_hvk.inst,
    &g_ps.surf)` instead of `SDL_Vulkan_CreateSurface(win->sdl_window, ...)`.
    The `sdl_window` field of `struct ANativeWindow` is left in place and
    unused.

Both functions are implemented in `crates/xb-gpu/src/ffi.rs`. Without a
surface provider the first returns no extensions and the renderer runs
without a swapchain, as xbased's windowless `xboxrun` does.

## 2. Log lines go to the host's log

- `hw/xbox/hle/hle.c`: `hle_log` hands the formatted line to
  `void hle_host_log(const char *tag, const char *line)` (declared in the
  same file) instead of writing it to `stderr`.

Why: xbemu logs through the `log` crate under per-crate targets so a front
end controls where the session's log goes. `hle_log` is variadic and Rust
cannot define a variadic C function, so the formatting stays in C and only
the finished line crosses.

## 3. The carried attributes wait for a method that reads them

- `hw/xbox/hle/pb/pbdraw.c`: `vertex_data` returns false for a method
  outside the vertex-data ranges (`pb/vimm.c`'s: `0x1500..0x16D0` and
  `0x1880..0x1B00`, word-aligned) before it applies the carry.

Why: every method that has an action goes through `vertex_data`. Applying
the attributes a draw left (`g_carry`) advances the constants' generation,
and a draw reuses the previous draw's constants block (`emit`) only while
that generation stands. With the range check first, a method that is not a
vertex-data one (a title's `SET_BEGIN_END` between two draws) leaves the
generation alone, and the next draw shares the block instead of copying it
and binding a descriptor set. Only the vertex-data methods, a draw without
the array and a savestate read the carried values. Fuzion Frenzy issues its
draws as short strips with nothing changing between them. Covered by
`crates/xb-core/tests/title_draws.rs`.

## 4. A texture found unchanged is not checked again in the same pass

- `hw/xbox/hle/pb/pbdraw.c`: `PbTex` has a `seen` stamp; `tex_written`
  returns false at once while `check_stamp()` equals it, and sets it when it
  finds the memory unchanged; `tex_edges_set` clears it; `hle_pbd_front_serve`
  (the start of a decoder pass) advances `g_pass`, half of the stamp.
- `hw/xbox/hle/mem/barrier.c`, `barrier.h`: `hle_mem_host_marks()`, a count
  of `hle_mem_mark_written` and `hle_mem_mark_landed` calls, the other half.

Why: the check of a texture for writes reads its pages' counts and dirty
bits, and for a first or last page left dirty (it is the neighbour's too,
so nothing takes the bit) hashes up to 4 KB. With the stamp that is done
once per texture per decoder pass rather than at every draw that uses it.

Why it is the same picture: a decoder pass reads PUT once, so every draw in
it was pushed before the pass began. A store the title makes and then
draws with is followed by a push, and that draw is decoded in a later pass,
where the stamp differs (Crimson Skies' labels, the case the check at every
use is there for). A pass also ends where the decoder waits for the title (a
trap, a flip stall). A store that lands in the middle of a pass comes after
the push of every draw in it, where the console promises no order either.
The renderer's own writes of guest memory change the stamp.

Not covered by a test: nothing in the tree drives a push buffer without a
title.

## 5. Copied vertices start a whole number of vertices into the arena

- `hw/xbox/hle/pb/pbdraw.c`, `draw_arrays`: a binding's copy is allocated
  one stride longer and begins at the next multiple of the stride (strides
  that are multiples of 4).

Why: the back end reduces a binding's offset by whole vertices into the
draw's first vertex (`vk/draw.c` `first_vertex`), so draws walking one
buffer keep their binding and strips with nothing recorded between them are
joined (`vk/batch.c`). Copies 256 bytes apart reduce to the same offset only
when the stride divides 256. Without guest RAM imported every draw's
vertices are copied, and Fuzion Frenzy's stride does not divide 256; with
the copy aligned to the stride its draws keep their binding and are joined.

## 6. The render scale changes while a title runs

- `hw/xbox/hle/pb/pbmode.c`, `pbmode.h`: `bool hle_pb_rescale(uint64_t
  timeout_ns)` asks the decode thread, which between two passes calls
  `hle_pbd_rescale` and answers.
- `hw/xbox/hle/pb/pbdraw.c`, `pbdraw.h`: `hle_pbd_rescale`.

Why: the back end sizes every image when it gets a device command and
destroys them all for a new scale (`vk/back.c` `cmd_device`); in push-buffer
mode the decoder sends that command once, at boot. `hle_pbd_rescale` does in
place what a savestate saved and loaded would:
`hle_pbd_park` (what the GPU drew goes back to guest memory),
`hle_pbd_state_get`, `hle_pbd_start` (the decoder forgets its targets and
textures), `hle_pbd_state_set`, and all memory marked written. The next
target bound makes the device at the new scale and everything is defined
again from memory. The guest's CPU must be paused: the core pauses it.
Depth buffers are not kept. `crates/xb-core/tests/title_draws.rs` changes the
scale twice on a running title.

## 7. The decoder's savestate section, as the reference writes it

Files of xbased added as they are (xbased 1.2.12), not changed:
`native/state/def.h`, `state.h`, `lock.h`, `sections.h` and `pbd.def`: the
description of the three records of `pbstate.h` field by field, each field
with the section versions it is stored in, and the header that turns it
into tables and stops the build when a record no longer matches it.
`hw/xbox/hle/pb/pbstate.c` is compiled.

Changed:

- `hw/xbox/hle/pb/pbstate.c`: `nc_pbd_state_load` calls
  `hle_pbd_state_replace` instead of `hle_pbd_state_set`, and includes
  `pbdraw.h` for it.
- `hw/xbox/hle/pb/pbdraw.c`, `pbdraw.h`: `hle_pbd_state_replace`, the part
  of change 6 that puts a decoder state in place of the one held (the
  decoder forgets its targets and textures first); the rescale uses it too.
- `hw/xbox/hle/pb/pbmode.c`, `pbmode.h`: `hle_pb_state_bytes` and
  `hle_pb_state_put`: what `nc_pbd_state_save` writes, after a magic and
  the section's version (`NC_STATE_V_PBD`), and `nc_pbd_state_load` with
  that version.

Added, xbemu's own:

- `shim/include/native/core/nc.h`: what `native/state/state.h` includes of
  xbased's core (the log).
- `shim/state_rec.c`: the buffer, reader and record functions `state.h`
  declares, taken from xbased `native/state/state.c` without its file
  container (xbemu's is `xb_types::state`).

Why: xbased loads a state into a decoder that has just booted; xbemu also
loads into one that has been running, which must forget what it holds
first. And the section is kept the reference's so that a later build reads
an earlier build's: a field added to a record gets the next version in
`pbd.def` and older states load with its default. Both functions are called
with the decoder parked (`hle_pb_park`, unchanged).

## 8. Guest RAM imported by its host address on an integrated GPU

- `hw/xbox/hle/vk/vkdev.c`, `vkdev.h`: the capability `host_ptr`
  (`VK_EXT_external_memory_host` on a device of type integrated, without an
  AHB import), the extension enabled with it, and
  `vkGetMemoryHostPointerPropertiesEXT` kept.
- `hw/xbox/hle/vk/guest_ram.c`: `import_host`, the AHB import's twin, and
  `hle_vk_guest_ram_init` using it off Android.

Why: the renderer reads vertices, indices and texels in place when guest
RAM is a Vulkan buffer (`g_hram.zero_copy`), which xbased has on Android
only; otherwise every draw's vertices are copied into the arena. With the
import an integrated GPU on a desktop reads them in place too. Tiger Woods
PGA Tour 2004 draws many vertices in many draws each frame. A discrete GPU
keeps the copy (its memory is not the system's). `XBEMU_GPU_RAM_COPY=1`
forces the copy.

## 9. An overlay that has not changed does not force a present

Not a change: xbased's own `ov_new` in `hw/xbox/hle/vk/swapchain.c` does
this (an overlay that is up counts as a change only when it is new since the
last picture, shows the scene, or has textures to write), and the file
carries nothing of xbemu's for it. xbemu relies on it: its overlay is a
still image set again when it changes (the menu, the frame-rate line), and
with "Show FPS" on an unchanged overlay must not cause a present at every
call. The number is kept so the others keep theirs.

## 10. A constant or program word sent again unchanged changes nothing

- `hw/xbox/hle/pb/pbdraw.c`, `hle_pbd_method`: a
  `SET_TRANSFORM_CONSTANT`, viewport offset or viewport scale word is
  stored and bumps the constants' generation (`cgen`) only when it differs
  from the word held; a `SET_TRANSFORM_PROGRAM` word likewise for the
  program's (`pgen`). The fixed-function matrices a few lines on compare
  the same way.

Why: a draw shares the previous draw's constants block only while `cgen`
stands (change 3), and a changed `pgen` costs a compare of the whole
program. A title that sends the same values again before each draw keeps
both.

## 11. The draw record is not cleared before it is stored

- `hw/xbox/hle/pb/pbdraw.c`, `emit`: takes the ring's space with
  `hle_fe_begin_raw`, as `hle_fe_state` does, instead of `begin`
  (`hle_fe_begin`).

Why: `begin` clears the record's 320 bytes and `*c = *d` then stores all of
them.

## 12. A pipeline drawn by the interpreters is in the title's record

- `hw/xbox/hle/vk/draw.c`, `pipeline_new`: the key of a pipeline the
  interpreters stand in for while the look-ahead builds it is recorded
  (`hle_pipe_record`) when the stand-in is taken, as it is for a pipeline
  built on the render thread or by an asynchronous worker.

Why: in the default mode nearly every pipeline is first drawn through a
stand-in. With those keys in the title's record the next boot's prebuild
builds them before the title runs, and no pipeline is built on the render
thread or beside the title. A key whose build fails later is tried again by
the next prebuild and counted in its "failed".

## 13. A colour surface's words become the depth buffer's only when needed

- `hw/xbox/hle/vk/back.c` (`zwrite_settle`, `dispatch`), `vk/draw.c`
  (`hle_draw_depth_unused`): an `HLE_OP_ZWRITE` command waits instead of
  being carried out when it arrives.

The decoder sends `HLE_OP_ZWRITE` each time a depth buffer is bound whose
memory a colour target also has. Carried out, it converts the colour image
when that is the newer: a pass over the whole scaled depth image writing
depth and stencil per pixel.

Commands that neither read nor write that depth buffer leave the command
waiting (target binds, state, uploads, another target's clear, the swap, a
draw with depth test and stencil off that does not sample it, the same
conversion asked again); a clear of all of its depth and stencil drops it;
any other command has it done first, so a draw that does test against the
converted words (Mercenaries' sky dome, the reason the conversion exists)
finds them.

Why: Halo's scope blur keeps its scratch target in the depth buffer's
memory, binds the depth buffer again for the frame's last draws, which test
neither depth nor stencil, and clears it at the next frame's start
(`flags=0xf3`, no rectangles). The conversion's result is never read there,
and waiting skips it.

## 14. A draw that follows a draw with nothing between is sent as eight bytes

- `hw/xbox/hle/front/cmd.h`, `fe.c`, `fe.h`: `HLE_OP_DRAW_MORE`
  (`HleCmdDrawMore`: the vertices' offset from the run's first draw, and
  their number); `hle_fe_begin_raw` remembers the last record's kind;
  `hle_fe_publish` (a record published without a wake).
- `hw/xbox/hle/pb/pbdraw.c`, `pbdraw.h`, `pbmode.c`: `g_run`. A
  `SET_BEGIN_END` / `DRAW_ARRAYS` / `SET_BEGIN_END` of the same strip, fan
  or line-strip topology that follows a draw with no other method, no new
  submission value, no other ring record, no 2D method, boundary, trap or
  end of pass between is not decoded as a draw: the run is extended.
  `hle_pbd_run_break()` ends a run and is called at every such place.
  Draws whose vertices are copied to the arena join too, for one binding
  with a stride that is a multiple of 4.
- `hw/xbox/hle/vk/back.c`, `back_int.h`, `draw.c`: the render thread
  records a `DRAW_MORE` on the state of the draw before it, straight into
  `vk/batch.c`; when a submission or pass ended between the two, the draw
  is drawn whole from a kept copy of the run's first record.

Why: Fuzion Frenzy issues its draws as short strips with nothing changing
between them. Such draws are joined in `vk/batch.c`, but only after the
decoder's whole path and the whole of `hle_draw_cmd_draw` have run for each
to find it the same as the one before. A run is recognised in the decoder
and neither path runs for the draws that extend it.
`XBASED_HLE_PB_RUNS_JOIN=0` decodes every draw in full;
`XBASED_HLE_RUNLOG=1` logs the runs.

Any new ring record must go through `hle_fe_begin_raw`, and any new way
into the decoder that changes what a draw draws must call
`hle_pbd_run_break()` first. If a run's first draw is dropped (its pipeline
still compiling) the run is dropped with it. Not covered by a test beyond
`crates/xb-core/tests/title_draws.rs`.

## 15. The render thread sleeps for draws and is woken for the rest

- `hw/xbox/hle/vk/back.c`, `back.h`: the thread's short wait before it
  sleeps is 20 us (200 upstream); after a pass that had work it sleeps with
  a 1 ms timeout, and `hle_back_kick_lazy` wakes it from that sleep only
  once the ring holds 32 KiB.
- `hw/xbox/hle/front/fe.c`: a commit of a draw or of a `DRAW_MORE` wakes
  lazily; every other record at once.
- `hw/xbox/hle/pb/pbdraw.c` (a run's end, lazily), `pb/pbmode.c` (before
  the decoder sleeps) and `vk/timeline.c` (`hle_tl_wait`): a wake at once,
  so nothing committed lazily is waited for unseen.

Why: the render thread spends less of its time in the short wait, and draws
reach it in batches. The timeout bounds the sleep should one of the front
end's waits on the render thread not wake it.
`XBEMU_T4_LAZY_BYTES=0 XBEMU_T4_BRIEF_US=200` gives the upstream wait.

## 16. Wake counters

- `hw/xbox/hle/util/thread.c`, `thread.h`, and counts in `pbmode.c`,
  `back.c`: an `hle-t4` log line every five seconds from the decoder (its
  sleeps and kicks, the render thread's short waits, sleeps, timeouts and
  wakes).

## 17. macOS

Each inside `#if defined(XBEMU_HOST) && defined(__APPLE__)`:

- `hw/xbox/hle/util/thread.c`, `thread.h`:
  - no `pthread_condattr_setclock` (macOS has none); the file's
    `pthread_cond_timedwait` calls, each with a deadline on
    `CLOCK_MONOTONIC`, go through `abs_wait`, which waits the time left to
    the deadline with `pthread_cond_timedwait_relative_np`;
  - `HleSem` is a dispatch semaphore: macOS's `sem_init` fails (no unnamed
    POSIX semaphores) and it has no `sem_timedwait`. It is made at 0 and
    posted up to its initial count, as libdispatch faults when one is freed
    below the count it was made with;
  - a thread names itself (`named_start`, and the timer thread):
    macOS's `pthread_setname_np` names only the calling thread.
- `hw/xbox/hle/hitch.c`: no per-thread CPU clock (`pthread_getcpuclockid`
  does not exist): the hitch reports leave the threads' CPU time out.
- `hw/xbox/hle/mem/barrier.c`: `CTR_EL0` is not read (macOS traps the read
  from user space); the cache-line step stays at its 64-byte default, no
  wider than Apple's cores' lines (`hw.cachelinesize` 128), so every line
  of a range is reached.
- `hw/xbox/hle/vk/pipeasync.c`: `hle_pipe_lower_priority` gives the
  pipeline workers the utility class of service, macOS's way of lowering a
  thread, where Linux raises its nice value.

Why: these are what the renderer needs of the host that macOS does not
have, or has under another name. The arm64 paths themselves (the counter
registers, the cache-line cleaning, the NEON swizzle) are hakuX's, kept as
they are; hakuX reads `CTR_EL0` on Android, where the kernel allows it.

## 18. Portability drivers

- `hw/xbox/hle/vk/vkdev.c`, inside `#if defined(XBEMU_HOST)`: the instance
  enables `VK_KHR_portability_enumeration` and sets
  `VK_INSTANCE_CREATE_ENUMERATE_PORTABILITY_BIT_KHR` where the loader has
  it. Inside `#if defined(XBEMU_HOST) && defined(VK_KHR_portability_subset)`:
  a device that offers `VK_KHR_portability_subset` gets it enabled, with
  the subset's features as the device reports them, and the features are
  logged.

Why: the Vulkan loader lists a portability driver (MoltenVK on macOS) only
to an instance that asks for one, and the specification requires the
subset extension to be enabled on a device that offers it.
`VK_KHR_portability_subset` is a beta extension of the headers:
`crates/xb-gpu/build.rs` defines `VK_ENABLE_BETA_EXTENSIONS` on Apple
targets, for volk and the renderer alike (it changes the layout of volk's
function tables).

## 19. Triangle fans as triangle lists on Apple hosts

- `hw/xbox/hle/pb/pbres.c`: under `XBEMU_HOST` and `__APPLE__`
  (`FANS_AS_LISTS`), `hle_draw_prim_topology` gives triangle fans and
  polygons a triangle list, `hle_draw_prim_count` counts `(n - 2) * 3`
  indices for them and `hle_draw_prim_indices` writes each fan triangle
  `0, i, i + 1`. A new `hle_draw_prim_converted` (declared in `pbres.h`,
  inside the define) names the primitives drawn from converted indices.
- `hw/xbox/hle/pb/pbdraw.c`: the array draw's two tests of which primitives
  are converted call `hle_draw_prim_converted` instead of listing them.

Why: Metal has no triangle fans, and MoltenVK's emulation of them draws
only a fan's first triangle: Halo's intro movie, one full-screen fan, is cut
along the diagonal that way and whole as a triangle list. Other hosts keep
the fan topology.

## 20. MoltenVK as the reference's macOS build runs it

- `hw/xbox/hle/vk/vkdev.c`, inside `XBEMU_HOST`: bindless is off when the
  driver is MoltenVK (`VK_DRIVER_ID_MOLTENVK`), as in the reference's macOS
  port. Inside `XBEMU_HOST` and `__APPLE__`: the instance enables
  `VK_EXT_layer_settings` and passes MoltenVK
  `MVK_CONFIG_FAST_MATH_ENABLED = 0`, unless that variable is set in the
  environment (a layer setting would override it).

Why: the reference's macOS port turns both off. Fast math lets Metal's
compiler reorder and approximate float operations, which changes what a
title's shaders compute. The setting goes through the instance rather than
the environment because the renderer starts after other threads are
running.

## Not patches, but worth knowing

- On macOS `hle_host_vk_loader` (`crates/xb-gpu/src/ffi.rs`, `src/os.rs`)
  opens the Vulkan loader itself, also from Homebrew's `/opt/homebrew/lib`
  and `/usr/local/lib`, where volk's search by name does not look.

- `__system_property_get` (declared by the vendored
  `native/compat/linux/include/sys/system_properties.h`, implemented by
  xbased's `native/compat/linux/linux.c`) is implemented in
  `crates/xb-gpu/src/ffi.rs`, and the header is on the include path of every
  host but Android, whose own it is: a property is the environment variable of the
  same name in capitals with `_` for `.`. xbased's `props.ini` file and
  run-time property setting are not implemented.
- `g_hle.posture` is set to `HLE_POSTURE_REPLACE` from Rust after `hle_init`
  instead of through the `XBASED_HLE_MODE` environment variable, so
  `hle_init` logs `init posture=off`. The renderer is on.
