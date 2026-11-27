# xb-apu

The MCPX audio processing unit: the device a title's DirectSound drives.

## What it owns

- The vendored C emulation in `vendor/xbased-apu` (registers, voice
  processor, DSP56300 emulator and its x86-64 recompiler) and its build
  (`build.rs`).
- The Rust interface to it: `Apu` (create, register window, start/stop,
  pause/resume, reset, volume/mute, HRTF, the DirectSound mailbox wait).
- Audio output: the `AudioSink` trait, `ring_sink` (a ring buffer for a real
  device's callback) and `NullSink` (plays to nobody, by the clock).
- What the C needs from the machine: `GuestRam`, `ApuHost` (guest clock,
  reports of writes to guest RAM), the interrupt line on `xb_types::irq::IrqHub`.

The device itself is described in `docs/hw/mcpx-apu.md`; the vendored tree in
`vendor/xbased-apu/README.md` and `PATCHES.md`.

## Invariants

- **One `Apu` per process at a time.** The C keeps its state in globals;
  `Apu::new` returns `ApuError::AlreadyExists` while another is alive. Drop
  one to make the next.
- **The vendored files are not edited.** Everything the C calls outside
  itself is defined in `src/ffi.rs`; a change to a vendored file must be
  listed in `vendor/xbased-apu/PATCHES.md`.
- **All `unsafe` and every `extern` is in `src/ffi.rs`**, except
  `GuestRam::new` (the caller's promise about the RAM mapping), its
  `Send`/`Sync` impls and the one call that passes it on.
- **Threads.** Register accesses, `mailbox_wait` and `guest_time_held` come
  from the guest-CPU thread. The frame thread (`apu.frame`, made by the C)
  mixes, runs the DSPs, reads and writes guest RAM, drives the interrupt line
  and calls the sink. Control methods may come from any thread.
- **The sink paces the APU.** The frame thread runs as fast as the sink
  drains; `AudioSink::queued_frames` must be truthful. See the module docs of
  `src/sink.rs`.
- **When `pause` or `stop` returns `true` the frame thread is parked**: no
  frame is made, no guest RAM written and the interrupt line not touched by
  it until `resume`/`start`.
- The C reads its own environment variables at creation (`NC_APU_*`,
  `XBASED_DSP_REC`, `XBASED_APU_STATS`, `XBASED_DSP_SHADOW`); they override
  `ApuOptions`. `NC_TRACE_APU` must not be set: its debug thread is never
  stopped and would outlive the device.

## `unsafe`

Allowed (FFI to the vendored C, guest RAM by raw pointer). Every block has a
`// SAFETY:` comment.

## Build requirements

clang and clang++ (override with `CC`/`CXX`), a C++20 standard library, and
on x86-64 a CPU with SSE4.1 and POPCNT (the reference's flags). No system
libraries beyond libc, libm, libpthread and libstdc++. The C is compiled
`-O3` in every cargo profile.

The `dsp-recompiler` feature (on by default) compiles the global processor's
program to host code on x86-64; without it, or on other architectures, the
DSP interpreter runs it.

## Tests

```
cargo test -p xb-apu
cargo test -p xb-apu --no-default-features
```

`tests/apu.rs` drives the real C through the registers (bring-up, frame rate,
interrupts, a playing voice, pause/resume, reset, teardown, the recompiler's
self-test); the tests are timing based and run one at a time inside the
binary. `src/sink.rs` has the ring's and the null
sink's unit tests. No game image is needed.
