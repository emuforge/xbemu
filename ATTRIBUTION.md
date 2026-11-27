# Attribution

xbemu is licensed under the GNU General Public License, version 3 or (at your
option) any later version. See `COPYING`.

## Lineage

| Project | Relationship | License |
|---|---|---|
| [xemu](https://github.com/xemu-project/xemu) | QEMU-based original Xbox emulator | GPL-2.0-or-later |
| [hakuX](https://github.com/rfandango/hakuX) | fork of xemu for Android | GPL-3.0-or-later |
| [xbased](https://github.com/crazyriddler/xbased) | started from hakuX; the reference this core was written from | GPL-3.0-or-later |

xbemu is a derived work of xbased 1.2.5. The Rust code in `crates/` and
`apps/` was written from it: much of the kernel, the devices, the disc
formats and the memory model follow its design, and it remains the reference
the core is checked against. That is why xbemu is, and has to be,
GPL-3.0-or-later. The aim is a core in pure Rust; the vendored code below is
what has not been rewritten yet.

Thanks to the authors of xbased, hakuX and xemu, and of the projects xemu
itself builds on (QEMU, and Hatari for the DSP56300 emulator).

## Notices kept in Rust files

`crates/xb-devices/src/nic.rs` follows the register model of xemu's nvnet
device (LGPL-2.1-or-later; Copyright (c) 2013 espes, Copyright (c) 2015-2025
Matt Borgerson). The notice is in the file.

## Vendored code

| Path | From | License |
|---|---|---|
| `vendor/xbased-hle` | xbased `hw/xbox/hle` (the GPU renderer, from hakuX) | GPL-3.0-or-later |
| `vendor/xbased-hle` (parts) | `stb_image_write.h` (public domain / MIT), the FSR1 shader text (MIT) | see the directory's README |
| `vendor/xbased-apu` | xbased `native/dev/apu` (from xemu; DSP56300 emulator from Hatari) | LGPL-2.1-or-later, GPL-2.0-or-later, BSD-3-Clause; see the directory's README |
| `vendor/xbased-apu/third_party/zydis` | Zydis and Zycore (the encoder the DSP recompiler emits code with) | MIT |
| `vendor/pulseaudio` | the `pulseaudio` crate 0.3.2 (the client `cpal`'s PulseAudio host uses), one change | MIT |

Each vendored directory has a `README.md` naming its source revision and a
`PATCHES.md` listing local changes.

## Libraries

Third-party libraries fetched at build time by `tools/fetch-deps.sh` and Rust
crates from crates.io keep their own licenses; all are GPLv3-compatible.

## Shipped beside the binary

| What | Where | License |
|---|---|---|
| [MoltenVK](https://github.com/KhronosGroup/MoltenVK) (Vulkan on Metal), Homebrew's build | `xbemu.app/Contents/Frameworks` in the macOS app, its license in `Contents/Resources/licenses` | Apache-2.0 |
