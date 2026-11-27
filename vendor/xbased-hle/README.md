# vendor/xbased-hle: the GPU renderer

The NV2A push-buffer renderer of **xbased 1.2.5**, vendored for `crates/xb-gpu`.

| | |
|---|---|
| Upstream | https://github.com/crazyriddler/xbased |
| Version | xbased 1.2.5, commit `2031611e0aebed3c3852af16a63108cabd9554a0` |
| License | GPL-3.0-or-later (`COPYING`, `LICENSE`); xbased derives from hakuX and xemu |

It decodes the push buffer a title's Direct3D sends to the NV2A and draws
with Vulkan. It includes nothing from the rest of xbased; everything it needs
from the emulator goes through `hw/xbox/hle/host.h`, which `crates/xb-gpu`
implements in Rust (`src/ffi.rs`).

## What is here

| Path | From xbased | Notes |
|---|---|---|
| `hw/xbox/hle/` | `hw/xbox/hle/` | the renderer, 106 files. The files `PATCHES.md` names are patched; the rest are byte-identical. The two empty packaging scripts (`package_linux.sh`, `package_windows.ps1`, 0 bytes each) were not copied. |
| `native/compat/glib/` | `native/compat/glib/` | xbased's own GLib subset (`glib.h`, `glib.c`) the renderer is written against. Unmodified. |
| `native/state/` | `native/state/` | `def.h`, `state.h`, `lock.h`, `sections.h`, `pbd.def`: the description of the decoder's savestate section that `pb/pbstate.c` is built against. Unmodified. |
| `shim/` | written for xbemu | `include/native/core/nc.h` and `state_rec.c`: what the savestate section needs of xbased's core (`PATCHES.md` 7). |
| `native/compat/linux/include/sys/system_properties.h` | same path | Android's property header for Linux; the renderer's debug switches read properties. Unmodified. The function it declares is implemented in `crates/xb-gpu/src/ffi.rs`. |
| `native/third_party/stb/stb_image_write.h` | same path | stb_image_write (public domain / MIT, notice in the file), used by the presenter's screenshots. Unmodified. xbased carries it in its tree, so it is carried here rather than fetched. |
| `licenses/amd-fsr1.license.txt` | `licenses/` | the license of the FSR 1 shader text embedded in `vk/swapchain.c`. |
| `COPYING`, `LICENSE` | repository root | GPL-3.0 and xbased's notice. |

The directory layout under `vendor/xbased-hle` is xbased's, so the sources'
own include paths (`"hw/xbox/hle/..."`, `"native/third_party/..."`) resolve
with `vendor/xbased-hle` as an include root.

## What is not here

- xbased's kernel, core, CPU, devices and application (`native/`), which
  xbemu reimplements in Rust.
- `native/hle/host_native.c`, `glue.c`, `dirty.c`, `native/dev/nv2a.c`,
  `native/app/present.c`: ported to Rust in `crates/xb-gpu`.
- Third-party libraries (Vulkan-Headers, volk, glslang, SPIRV-Reflect,
  xxHash, nv2a_vsh_cpu). `tools/fetch-deps.sh` fetches them at the revisions
  xbased pins into `third_party/`; nothing of them is committed.
  VulkanMemoryAllocator is on xbased's include path but no renderer source
  includes it, so it is not fetched.

## What is built

`crates/xb-gpu/build.rs` compiles every `.c` file under `hw/xbox/hle`, and
`shim/state_rec.c`.

`pb/pbstate.c`, the renderer's savestate section, is built against the
vendored `native/state` headers; `shim/state_rec.c` stands in for xbased's
`native/state/state.c` (`PATCHES.md` 7).

`core/xbe.c` is compiled here although xbased's `hle.cmake` leaves it to its
core library: the renderer calls `hle_xbe_parse`.

The build defines what xbased's Linux desktop release build defines for
these files (`CONFIG_VULKAN=1 XBASED_HLE_GPU=1 XBASED_NATIVE_CORE=1
NC_DESKTOP=1 VK_NO_PROTOTYPES XBASED_DIAG=0 _GNU_SOURCE NDEBUG`, `-O3
-std=gnu11 -fno-delete-null-pointer-checks -msse4.1 -mpopcnt`) plus
`XBEMU_HOST=1`, which guards the patches. Not carried over: the SDL and
Zydis definitions and include paths, and `-flto=thin`.

## Updating

Copy the new `hw/xbox/hle` and `native/state` over these, re-apply
`PATCHES.md` (which says which changes are guarded by `XBEMU_HOST`), update
the commit above, and check `host.h` for new or changed functions against
`crates/xb-gpu/src/ffi.rs`.
`diff -ru <xbased>/hw/xbox/hle vendor/xbased-hle/hw/xbox/hle` must show only
the patches.
