# Changes to the vendored files

## Files from xbased (`native/`)

Three files under `native/dev/apu` differ from xbased 1.2.12: `apu.c`,
`dsp/dsp_rec.cpp` and `dsp/gp_ep.c`, by the numbered changes below and the
changes under "Changes for speed". The changes are marked `xbemu:` in the
source. Every other file there, and `native/cpu/x64asm.h`, is identical.

1. **`dsp/dsp_rec.cpp`: a DO loop's way back to its beginning is a jump.**
   A loop entered from a block that began before its `DO` comes round to
   the loop's own first word, which is another block's. `Rec::scan` notes
   where the loop of each `LA` begins (`la_start`; none when two loops end
   at one address), and `transfer` compares `SSH` with that as well as with
   its own block's address and goes on as a jump does (`transfer_plain`:
   the accumulators kept, linked once). Without the second compare such a
   loop leaves through the dispatcher (`h_lookup`: the accumulators stored,
   the state checked, the other block entered cold) on every pass. The
   XDK's effects are subroutines of a `DO` over 16 samples each, entered
   that way. The instructions run are the same, to the count
   (`XBASED_APU_STATS=1`).

2. **`apu.c`, `dsp/gp_ep.c`: macOS.** Inside `#if defined(XBEMU_HOST) &&
   defined(__APPLE__)`:
   - `apu_cond_init` does not call `pthread_condattr_setclock`, which macOS
     lacks, and `apu_cond_timedwait_us` waits with
     `pthread_cond_timedwait_relative_np`: the same wait, relative to now
     on a clock wall-clock changes do not move.
   - `apu_thread_start` starts the thread through `apu_named_start`, which
     names it and calls the thread's function: macOS's
     `pthread_setname_np` names only the calling thread.
   - `gp_ep.c`: the `XBASED_APU_STATS` block around the GP's `dsp_run`
     counts TSC ticks, so it is compiled on x86-64 only
     (`defined(__x86_64__)` added to its two `#if`s), as the transfer
     counters it reads already were.

Everything else xbemu needs differently is done outside those files:

| Need | How, without touching the source |
|---|---|
| The APU includes five headers of xbased's core (`native/core/nc.h`, `irq.h`, `mmio.h`, `vm.h`, `native/dev/dev.h`) | `shim/include/native/…` comes first on the include path and declares only what the APU uses: `nc_log`; `nc_irq_set`, `nc_paused`, `nc_pause_wait`, `nc_pause_register`, `nc_pause_idle`; `NcMmioDev`, `nc_mmio_register`, `nc_mmio_retry`; `NcVm` (two fields); the `nc_dev_apu_*` prototypes. |
| Those functions, `nc_ke_now_ns` and `hle_host_mem_set_dirty` (declared in `apu_base.h`) must not collide with another crate's definitions (the GPU renderer's host interface has `hle_host_mem_set_dirty` too) | `build.rs` renames them on the command line (`-Dnc_irq_set=xb_apu_irq_set` and so on); the Rust definitions are in `crates/xb-apu/src/ffi.rs`. |
| No SDL: `out.c` is not wanted | Not compiled. Its five functions (`apu_out_open`, `apu_out_queued`, `apu_out_asked_max`, `apu_out_push`, `apu_out_stats`) and two globals (`g_apu_out_channels`, `g_apu_frame6`) are defined in `ffi.rs`; its ring, callback and drain thread are ported to `sink.rs`. |
| `state.c` (savestates) includes xbased's `native/state` headers | `shim/include/native/state/{state.h,def.h,sections.h}` declare the byte buffer and the one record it uses; `shim/apu_shim.c` implements them and hands the state to Rust as bytes (`xb_apu_shim_state_save`, `_load`, `_free`). `state.c` itself is compiled as it is. Its buffer and record functions and its two entry points are renamed at compile time (`build.rs` `RENAMES`), as the renderer has the reference's own under the same names. |
| The frame thread must not run before `Apu::start`, and must stop for a pause | It already parks in `nc_pause_wait` while `nc_paused()`; Rust implements both. |
| `gp_ep.c` remembers the last output preference in a function static, so a second device in one process could skip applying it | `ffi.rs` applies the opposite preference and then the wanted one after init. |
| The recompiler does not exist off x86-64 (xbased itself builds for x86-64 only) | `dsp_rec.cpp`, Zydis and the x86 flags (`-msse4.1 -mpopcnt`) are left out there (`build.rs`, by target arch) and `ffi.rs` defines `dsp_rec_run` and friends to say "not available", which sends `dsp_run` to the interpreter. That is hakuX's arm64 build: the interpreter alone (`dsp_cpu.c`, no recompiler, no Zydis) and no architecture flags. |

Build flags that differ from the reference's `xbased_apu` target: no
`-flto=thin` (the objects go into a Rust static library), no `-g1`, and the
renderer's defines (`CONFIG_VULKAN`, `VK_NO_PROTOTYPES`, `XBASED_HLE_GPU`,
`XBASED_DIAG`), which nothing in the APU reads, are dropped.

## C written for xbemu (`shim/apu_shim.c`)

Three things Rust cannot do:

1. **`nc_log` is variadic**, and `dsp_rec.cpp` declares it itself, so it has
   to exist as a real C-variadic function. Defining one in Rust is not
   possible at the workspace's minimum Rust version. The shim formats the
   line with `vsnprintf` and calls `xb_apu_log(tag, line)` in Rust.
2. **Teardown.** xbased never destroys its APU; xbemu must (a second title, a
   test). Setting `exiting`, waking and joining the frame thread, and freeing
   the DSPs and the device need `MCPXAPUState`'s layout, which only the C
   headers know (no bindgen: it would add libclang as a build requirement).
   `xb_apu_shim_request_exit`, `xb_apu_shim_destroy` and `xb_apu_shim_kick`
   (wake the frame thread's wait) do that through `apu_int.h`.
3. **The savestate's buffer and record functions** that `state.c` calls, and
   the three functions that hand the state to Rust as bytes (the table
   above).

## Zydis (`third_party/zydis`)

Unmodified files, a subset: `include/` whole, `src/Encoder.c`,
`EncoderData.c`, `Mnemonic.c`, `Register.c`, `SharedData.c` and the nine
generated tables they include, and Zycore's `include/`. The decoder and the
formatter are left out and compiled out (`ZYDIS_DISABLE_DECODER`,
`ZYDIS_DISABLE_FORMATTER`).

## Changes for speed

All in `native/dev/apu/`. `crates/xb-apu/build.rs` defines `XBEMU_HOST=1`.
Not every change is inside `#if defined(XBEMU_HOST)`: `dsp/dsp_rec.cpp` has
no such guard, and `apu.c` reads `XBEMU_T4_APU_BURST` outside one.

- `dsp/dsp_rec.cpp`: `DO`, `JSR`, `RTS` and a loop's end are written out in
  the generated code instead of called. The S bit is tested before it is
  worked out; the lazy flags are kept as their operands; flags no block
  reads past a static edge are not worked out; a `Jcc` after a compare
  uses the host's flags. A `DO` body has a second copy for the passes after
  the first that only counts LC down. A program that rewrites SSH inside a
  loop is not handled by the last; a frame cut by its cycle budget can
  leave flag bits nothing reads stale in SR.
- `dsp/gp_ep.c`: a mix bin that is all zeros is not converted from float;
  the six outputs are read from the mix buffer in place.
- `apu.c`, `throttle`: the eight global-processor frames of an encode
  frame run in one wake (`XBEMU_APU_BATCH`, 1, 2, 4 or 8; 8 unless set, 1
  is upstream's pacing), one at a time again while a guest thread waits on
  memory the APU writes (`mcpx_apu_spin_wait`; Project Gotham Racing 2
  waits that way). A voice or mailbox command is taken up to 4.7 ms later
  than with one frame to a wake.
- `dsp/dsp_rec.cpp`, `apu.c`: debug hooks (`XBASED_APU_STATS=1`, the
  thread's time by part; a state dump and a code dump for an offline
  harness).
- `apu.c`: `XBEMU_T4_APU_BURST` is also read where the next encode frame's
  time is set. Unset it does nothing; set, it disagrees with
  `XBEMU_APU_BATCH`. To be removed.
