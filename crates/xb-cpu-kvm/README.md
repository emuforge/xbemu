# xb-cpu-kvm

The guest CPU backend on Linux KVM: `KvmCpu` implements `xb_cpu::Cpu` by
running the title natively in a one-vCPU virtual machine. Design and
rationale: `docs/hw/cpu-kvm.md`.

## Owns

- The VM, its vCPU and the run loop (`src/linux/mod.rs`).
- The guest's real page tables, GDT, IDT, exception stubs and trap page, kept
  in a hidden memory slot (`src/linux/tables.rs`).
- The `PageProtector` used for write tracking: `mprotect` on the mapping
  behind the RAM slot (`src/linux/ram.rs`), and the operand decoder that
  locates the faulting page (`src/linux/decode.rs`).
- The kick signal (`src/linux/kick.rs`).
- `testing` (feature `testing`): guest-code cases written against the
  `xb_cpu::Cpu` trait only, for every backend.

## Invariants

- Linux x86-64 only; everything else is behind `cfg` in `src/linux`. On other
  targets the crate is `KvmCpu::probe()` returning an error, plus `testing`.
- Guest-virtual `SYSTEM_VA .. SYSTEM_VA + SYSTEM_VA_LEN` (`0xFFFE_0000`, three
  pages) and guest-physical `HIDDEN_GPA .. + HIDDEN_LEN` (`0xE000_0000`)
  belong to the backend. `Mapper` calls that touch them are refused and
  logged.
- The process must not change the disposition of `SIGRTMIN +
  KICK_SIGNAL_OFFSET`.
- The `kvm_run` block is accessed through a raw pointer only, never through a
  reference: the signal handler writes one byte of it.
- The mapping KVM's RAM slot uses is touched by nobody but KVM, so taking
  access away from it cannot fault host code.
- `Regs::fs_base` is read by `run` and never written back.

## Unsafe

Allowed (`mmap`, `mprotect`, `ioctl`, the signal handler, raw access to the
shared `kvm_run` block and to guest memory). Every block has a `// SAFETY:`
comment.

## Tests

```
cargo test -p xb-cpu-kvm -- --nocapture
```

The tests run real guest code and print the measured cost of a trap exit, a
thread switch and a write-tracking fault. Without a usable `/dev/kvm` each
test prints a note and passes.
