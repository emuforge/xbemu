# xb-gpu

The NV2A graphics processor: the register front end in Rust and the
push-buffer renderer as vendored C (Vulkan).

## Owns

- The 16 MiB register window at physical `0xFD00_0000` (`Gpu::mmio_read`,
  `Gpu::mmio_write`), the GPU's interrupt line (`Irq::Gpu`), the vertical
  blank, PTIMER's alarm and flip pacing (`src/nv2a.rs`, `src/timing.rs`,
  `src/regs.rs`).
- The renderer, `vendor/xbased-hle` (C), built by `build.rs`, and its host
  interface `host.h`, implemented in `src/ffi.rs` and forwarded to the
  machine through the `GpuHost` trait (`src/host.rs`).
- Presentation: the picture letterboxed into the front end's window with the
  core's RGBA overlay composited over it (`Presenter`, `src/present.rs`).
  The drawing is the vendored presenter's.

`docs/hw/nv2a-front.md` describes the register front end and the host
interface. `vendor/xbased-hle/README.md` and `PATCHES.md` describe the
vendored code.

## Invariants

- **One `Gpu` per process.** The renderer's state is C globals and it has no
  teardown. `Gpu::new` fails the second time. `Gpu::shutdown` stops decoding,
  the timers and presentation and saves the shader caches; the Vulkan device
  stays until the process exits.
- The RAM pointer given to `Gpu::new` and the `GpuHost` stay in use for the
  life of the process.
- All foreign functions are declared in `src/ffi.rs`: what Rust calls in C,
  and every C symbol Rust exports. No other module declares `extern`.
- `mmio_read`, `mmio_write` and the `hle_host_*` functions do not allocate.
  The only lock on those paths is the one around evaluating the interrupt
  line (as in the C), and the DAC palette's on its byte writes.
- The register file is only ever accessed through atomics; narrow guest
  writes are read-modify-write of the containing word.
- Host clock values exchanged with the renderer are `CLOCK_MONOTONIC`
  nanoseconds (`src/os.rs`).
- `GpuHost` methods are called from the renderer's threads at any time and
  must not block on the GPU.
- Changes to vendored files are guarded by `XBEMU_HOST` and listed in
  `vendor/xbased-hle/PATCHES.md`.

## `unsafe`

Allowed (the crate calls C and takes a raw RAM pointer). Every `unsafe`
block has a `// SAFETY:` comment.

## Build prerequisites

- `clang` and `clang++` (or `CC`/`CXX`), `cmake`, `make` or `ninja`,
  `python3` (glslang's build), a C++ standard library to link (`libstdc++`).
- Third-party sources in `third_party/` (run `tools/fetch-deps.sh`, which
  needs `git` and network access) or in `XBEMU_DEPS_DIR`: Vulkan-Headers,
  volk, glslang, SPIRV-Reflect, xxHash, nv2a_vsh_cpu, at the reference's pins.
- At run time: a Vulkan loader (`libvulkan.so.1`; on macOS
  `libvulkan.1.dylib`, looked for also in `/opt/homebrew/lib` and
  `/usr/local/lib`) and a Vulkan 1.1+ driver (on macOS, MoltenVK).
  No Vulkan development package is needed to build (headers are fetched,
  the loader is opened at run time by volk).

The C is compiled with `-O3` in every cargo profile. `build.rs` reruns only
when a vendored file, a probe file of a dependency, `build.rs` or
`XBEMU_DEPS_DIR`/`CC`/`CXX` changes.

## Tests

```
cargo test -p xb-gpu
```

- Unit tests (`src/`): register semantics, interrupt line, vblank counting
  and the alarm with an injected clock and a mock decoder, display-lock and
  flip-slot timing, picture placement, frame counting.
- `tests/link.rs`: every exported `host.h` function and the renderer's entry
  points resolve in the final binary.
- `tests/headless.rs`: starts the renderer on the host's Vulkan device
  through the public API with a mock machine and shuts it down. Prints
  `SKIPPED` and passes if there is no usable Vulkan device.
- `examples/window.rs` (not run automatically): a window with the presenter
  and a test overlay, no game.
  `cargo run -p xb-gpu --example window -- --seconds 5 --resize --shot /tmp/shot.bmp`

Renderer debug switches are environment variables: the vendored C's own
(`XBASED_HLE_*`, `XBASED_GPU=N` to pick the Nth Vulkan device) and its
Android properties as `DEBUG_XBASED_<NAME>` (`debug.xbased.caps` is
`DEBUG_XBASED_CAPS`).
