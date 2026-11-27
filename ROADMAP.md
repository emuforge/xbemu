# Roadmap

Where xbemu is going. `docs/status.md` says where it is.

## Aims

- Run original Xbox games and homebrew straight from a disc image, with the
  console's kernel reimplemented on the host: no BIOS, no dashboard.
- A core in pure Rust. The GPU renderer and the audio processor are vendored
  C today and are the two pieces left to port.
- Full speed on modest hardware, and wide compatibility, measured title
  by title.
- One small core API with thin front ends: a standalone app now, a libretro
  core next.
- Builds for every platform the CPU and GPU back ends reach, published from
  GitHub: a pre-release from every commit to `main`, a stable release from
  every version tag.

## Order of work

1. **Compatibility.** More titles: missing kernel exports first, then what
   each title needs.
2. **libretro core** over `xb-core`.
3. **Performance**, where a measurement shows a title bound by the emulator.
4. **The renderer and the APU in Rust**, piece by piece, each checked
   against the C it replaces.
5. **Platforms**, below.

## Platforms

| Platform | CPU back end | State |
|---|---|---|
| Linux x86-64 (glibc) | KVM, translator | supported; AppImage and tarball from CI |
| Linux x86-64 (musl) | KVM, translator | planned: its own build, for musl distributions |
| Windows x86-64 | translator (a hypervisor back end later) | planned |
| Linux arm64 | a recompiler, not written | later |
| Android arm64 | a recompiler, not written | later |
| macOS (arm64, x86-64) | a recompiler; 16 KiB pages and MoltenVK to solve | later |

Host-specific code sits behind `cfg` in named modules so each of these is an
addition, not a restructuring.

## Testing and automation

- Unit and cross-check tests run in CI on every change. CI has no game data
  and no GPU.
- Tests that boot a real title run locally, on the developer's own images
  (`titles/README.md`).
- Planned: fuzzing of the formats and savestates, homebrew test programs in
  CI on a software Vulkan driver, and benchmarks tracked over time.
