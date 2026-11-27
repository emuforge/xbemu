# xb-cpu-interp

The guest CPU backend for hosts the other two cannot serve: `InterpCpu`
implements `xb_cpu::Cpu` by interpreting the title's x86-32 code in portable
Rust, and on AArch64 macOS by compiling hot blocks to host code. It needs no
hardware virtualization and makes no assumption about the host's page size.

## Owns

- The guest's address space in software (`src/mem.rs`): a flat page table
  with one entry per 4 KiB guest page, the write-tracking restriction per
  physical page (`PageProtector`), and every guest load and store.
- Decoded blocks (`src/cache.rs`) and their compiled instructions
  (`src/fast.rs`): each instruction decoded once (`iced-x86`), with a handler
  chosen for its exact form.
- The instruction set: integer and system instructions (`src/exec.rs`), x87
  (`src/x87.rs`, registers in `src/f80.rs`), MMX and the Pentium III's SSE
  (`src/simd.rs`), and the arithmetic flags computed when read
  (`src/flags.rs`).
- The code generator (`src/jit/`): AArch64 code for blocks that have run a
  few times, NEON for the MMX forms, double-precision instructions for the
  common x87 forms, and a dispatcher through which one
  compiled block goes straight on to the next.

## Invariants

- **Restartable instructions.** An instruction reads everything before it
  writes anything and stores to memory before it updates registers; the run
  loop restores the integer registers and flags of one that stops. String
  instructions commit element by element.
- **`PageProtector::protect` is synchronous.** It waits until the guest-CPU
  thread has acknowledged the new restriction, which the thread does before
  each block and between the elements of a string instruction, and never
  while it is inside a `Bus` call. A block is at most 64 instructions.
- **A block is valid while its page-table entries are unchanged and its
  physical pages unwritten.** A guest store to a page holding decoded bytes
  drops that page's blocks before the next block runs; the host's own writes
  to code arrive through `Cpu::invalidate_code`.
- **A NaN result is picked by the x87's rule** (Intel SDM vol. 1, table
  4-7), never by the host's operand order, so it is the same whichever path
  or build computes it.
- **x87 values are doubles.** Arithmetic is done in `f64`, rounded to single
  when a thread selects single precision; a register keeps its exact 80 bits
  only for values that pass through unchanged (`FLD m80`, `FILD m64`, MMX).
- **Packed integer instructions are MMX**, with or without a `66` prefix, as
  on the console's processor.
- **A compiled block goes on to another only through the dispatcher**,
  which returns to the run loop when the CPU is kicked, when `eip` is in
  the trap range, when write tracking has a restriction the thread has not
  acknowledged, or when the next block is not compiled or its page-table
  entries changed. A store that changes code returns to the run loop as
  well. The cache takes a block out of the dispatcher's table whenever it
  stops being valid.
- **An x87 register's exact bits are plain fields** (`f80::Fpr`, C layout):
  generated code reads and writes MMX registers there in place.
- **Generated code equals the interpreter.** It implements a subset of
  forms with host instructions and calls the interpreter's own handlers and
  memory access for everything else; a handler and its generated code must
  leave the same state (`tests::compiled`). Generated code is entered only
  from `jit::run`, on the guest-CPU thread.

## `unsafe`

Allowed in `src/jit/native.rs` (running generated code; the helpers it calls
back with raw pointers to the CPU and the bus) and `src/jit/macos.rs`
(executable memory). The crate root has `#![deny(unsafe_code)]` and those two
modules opt in. Everything else, the whole interpreter included, is safe:
guest RAM is reached through `xb_mem::Ram`'s bounds-checked copies.

## Hosts

The interpreter runs everywhere. Code generation exists for AArch64 macOS;
another AArch64 host needs an executable-memory module beside
`src/jit/macos.rs`. A macOS release signed with the hardened runtime needs
the `com.apple.security.cs.allow-jit` entitlement, without which the
interpreter runs alone.

## Tests

```
cargo test -p xb-cpu-interp
```

The backend-neutral cases of `xb-cpu-kvm` (`testing`) and the instruction
snippets of `xb-cpu-x64` (`testing`) run interpreted and with every block
compiled; each compiled form is compared with the general path and with the
interpreter from random states; the code generator's encodings are compared
with the assembler's. No game image is needed.
