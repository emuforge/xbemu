# vendor/xbased-apu: the MCPX APU, vendored C

The emulation of the Xbox's MCPX audio processing unit used by the crate
`xb-apu` (`crates/xb-apu`), which builds it (`build.rs`) and is its only user.

## Source

- **xbased 1.2.5**, directory `native/dev/apu`, commit
  `2031611e0aebed3c3852af16a63108cabd9554a0` of
  <https://github.com/crazyriddler/xbased>.
- xbased's APU is xemu's device (`hw/xbox/mcpx/apu`) ported from QEMU's
  services onto xbased's own; the DSP56300 emulator inside it comes from
  Hatari through xemu.

Three files under `native/dev/apu` are changed (`apu.c`, `dsp/dsp_rec.cpp`,
`dsp/gp_ep.c`); `PATCHES.md` lists the changes. Every other file under
`native/` is **byte for byte** the file at that path in xbased.

## Layout

| Path | What | From |
|---|---|---|
| `native/dev/apu/` | the device: registers and frame thread (`apu.c`), voice processor (`vp/`), DSP56300 emulator, recompiler and the global/encode processors (`dsp/`), the savestate (`state.c`), plus `out.c`, kept for reference and not built | xbased `native/dev/apu/` |
| `native/cpu/x64asm.h` | the small x86-64 assembler the DSP recompiler emits code with | xbased `native/cpu/x64asm.h` |
| `shim/include/native/…` | stand-ins for the five xbased core headers the APU includes (`core/nc.h`, `core/irq.h`, `core/mmio.h`, `core/vm.h`, `dev/dev.h`) and the three `state/` headers `state.c` includes, declaring only what the APU uses | written for xbemu |
| `shim/apu_shim.c` | the only C written for xbemu: `nc_log`'s formatting, the device's teardown and the savestate's buffer functions | written for xbemu |
| `third_party/zydis/` | the encoder half of Zydis (x86-64 instruction encoder) and the Zycore headers it needs, for the recompiler | Zydis commit `a2278f1` (v4.1.1 line), Zycore `0b2432c` (1.5.2) |
| `licenses/` | the license texts | xbased `licenses/`, `COPYING`, `LICENSE` |

## Licenses

Each file keeps the notice in its header. As xbased's `LICENSE`
(`licenses/xbased-LICENSE.txt`) summarizes them:

| License | Files (under `native/dev/apu/` unless said) |
|---|---|
| LGPL-2.1-or-later (`licenses/LGPL-2.1.txt`; the headers say "version 2 of the License, or (at your option) any later version" of the Lesser GPL) | `apu.c`, `apu_int.h`, `apu_regs.h`, `apu_debug.h`, `fpconv.h`, `dsp/dsp_dma.c`, `dsp/dsp_dma.h`, `dsp/dsp_dma_regs.h`, `dsp/gp_ep.c`, `dsp/gp_ep.h`, `vp/vp.c`, `vp/vp.h`, `vp/hrtf.h` |
| GPL-2.0-or-later (`licenses/GPL-2.0.txt`): the DSP56300 emulator, adapted from Hatari's DSP M56001 emulation | `dsp/dsp.c`, `dsp/dsp.h`, `dsp/dsp_cpu.c`, `dsp/dsp_cpu.h`, `dsp/dsp_cpu_regs.h`, `dsp/dsp_state.h`, `dsp/dsp_emu.c.inc`, `dsp/dsp_dis.c.inc`, `dsp/debug.c`, `dsp/debug.h` |
| GPL (`licenses/GPL-2.0.txt`): adapted from the SWH LADSPA plugins. xemu's header says "GPLv2"; the original (`svf_1214.xml`) says only "GPL", which xbased reads as any version | `vp/svf.h` |
| BSD-3-Clause (`licenses/adpcm-xq.license.txt`): the ADPCM-XQ decoder, David Bryant | `vp/adpcm.h` |
| GPL-3.0-or-later (`licenses/GPL-3.0.txt`): xbased's and hakuX's own work | `apu_base.h`, `out.c`, `state.c`, `dsp/dsp_rec.cpp`, `dsp/dsp_rec.h`, `vp/mix_inline.h`, `vp/resample.c`, `vp/resample.h`, and `native/cpu/x64asm.h` |
| MIT (`third_party/zydis/LICENSE`, `third_party/zydis/zycore/LICENSE`) | `third_party/zydis/` |
| GPL-3.0-or-later, as xbemu | `shim/` |

The combination is distributed under GPL-3.0-or-later.

## What is built

`apu.c`, `dsp/debug.c`, `dsp/dsp.c`, `dsp/dsp_cpu.c`, `dsp/dsp_dma.c`,
`dsp/gp_ep.c`, `state.c`, `vp/resample.c`, `vp/vp.c` and `shim/apu_shim.c`;
on x86-64 with the crate's `dsp-recompiler` feature also `dsp/dsp_rec.cpp`
and five Zydis sources. Not built: `out.c` (SDL/AAudio output; `xb-apu`'s
`sink.rs` and `ffi.rs` replace it).

## Updating

Copy `native/dev/apu` and `native/cpu/x64asm.h` from the new xbased over the
ones here, re-apply `PATCHES.md`, update the commit above, re-check the list of outside symbols
(`nm -u` on the built archive must show only libc, libm, libpthread, libstdc++,
the `apu_out_*` functions, `g_apu_out_channels`, `g_apu_frame6` and the
`xb_apu_*` names) and run `cargo test -p xb-apu`.
