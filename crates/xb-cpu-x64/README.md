# xb-cpu-x64

The guest CPU backend that translates the title's x86-32 code to x86-64 host
code and runs it in-process (`X64Cpu`, implementing `xb_cpu::Cpu`). Fallback
for hosts without KVM. Linux x86-64 only so far; on other targets the crate is
empty.

Design: `docs/hw/cpu-x64.md`.

## Owns

* The guest's 4 GiB address-space reservation and the views of RAM mapped in
  it (`Mapper`), including their host protection (`PageProtector`).
* The translation cache and everything in it.
* A process-wide handler for SIGSEGV, SIGBUS, SIGFPE and SIGILL. It chains to
  the previous handler for faults outside translated code.
* The `GS` base of the guest-CPU thread.

## Invariants

* Translated code never calls Rust. It leaves through one exit stub and `run`
  does the work; the `Bus` is only called from `run`.
* The signal handler touches only atomics, the interrupted context and the
  frame of the translator whose code faulted.
* Every sequence the translator emits for a guest instruction leaves guest
  registers, flags and memory unchanged until its last faulting host
  instruction has run, so a faulting instruction can be run again from its
  start.
* Nothing emitted between guest instructions changes the host's flags.
* The guest base is a multiple of 4 GiB.
* Blocks are killed only from Rust, outside translated code.
* At most 16 translators per process.

## `unsafe`

Allowed and used: generated code, `mmap`/`mprotect`, signal handling, raw
access to the frame and the RAM view. Every block has a `// SAFETY:` comment.

## Tests

```
cargo test -p xb-cpu-x64
```

The tests run this crate's snippets (`src/testing.rs`, exported with the
`testing` feature) and the backend-neutral cases of `xb-cpu-kvm`; when
`/dev/kvm` is usable the same cases also run on `KvmCpu` and the exact ones
must agree. `-- --nocapture` prints the trap round-trip time.
