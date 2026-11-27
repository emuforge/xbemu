# The x86-64 translator (`xb-cpu-x64`)

`X64Cpu` runs the title's x86-32 code by translating it, block by block, into
x86-64 code that executes in the emulator's own process. It is the CPU backend
used when KVM is not available. The approach is the reference
implementation's: the guest and the host are the same architecture family,
so an instruction is not interpreted or lowered to an intermediate form. It
is decoded as 32-bit code and encoded again for 64-bit mode
(`iced-x86` does both), and the host processor computes the result, the flags
and the x87/SSE state itself.

Source map: `translate.rs` (instructions), `stubs.rs` (entry, exit,
dispatcher), `cache.rs` (blocks, chaining, invalidation), `cpu.rs` (the run
loop, faults, instructions run in Rust), `mem.rs` (the guest address space),
`host/linux.rs` (mmap, signals, the segment base).

## Guest memory

The guest's 4 GiB are a `PROT_NONE` reservation in the host (with 1 MiB
guards), whose base is a multiple of 4 GiB. `Mapper::map` maps views of the RAM
memory object into it at `base + va`; `unmap` and `map_mmio` leave the range
inaccessible. A flat page table (one `u32` per guest page) and, per physical
page, the list of virtual pages showing it, record what is where.

Translated code reaches guest address `x` as `gs:[x]` with a 32-bit address
size: the effective address wraps at 4 GiB exactly as the guest's does, and
the processor adds `GS`'s base, which is the reservation's base. 64-bit Linux
leaves `GS` unused. The enter stub sets the base with `WRGSBASE` where the
kernel allows it (Linux 5.9 and later on processors with FSGSBASE); otherwise
`run` calls `arch_prctl(ARCH_SET_GS)` each time. The embedding process must
not use `GS` for anything else on the guest-CPU thread.

The translator itself never dereferences `base + va`. It reads guest code and
runs its own instruction emulation through the page table and a second,
always-writable mapping of RAM.

The host protection of a view is the most restrictive of three things: the
access the page table gave the mapping, the restriction write tracking put on
the physical page (`PageProtector`, applied to every view under one mutex, so
it is synchronous and callable from any thread), and the translator's own
write protection of pages holding translated code.

Each change of protection can split a host mapping; with heavy write tracking
the process can approach `vm.max_map_count` (65530 by default).

## Registers

| Host | Holds |
|---|---|
| `RAX RCX RDX RBX RBP RSI RDI` | the guest registers of the same name, upper halves zero |
| `R12` | guest `ESP` |
| `R14` | the guest's FS base |
| `R15` | the frame (`frame.rs`): saved registers on exit, control state |
| `R8` | host address of guest address 0 |
| `R9 R10 R11 R13` | scratch; `R13D` carries a guest `eip` to the dispatcher |
| `RSP` | the shadow stack (return predictions), not the host's stack |
| RFLAGS | the guest's CF PF AF ZF SF OF DF and ID |
| x87, MMX, XMM0-7, MXCSR | the guest's |

Because guest registers are host registers, most instructions translate to
themselves with a `GS` prefix. What needs care:

* **ESP** becomes `R12`, which needs a REX prefix; AH, CH, DH and BH cannot be
  encoded beside one. For those instructions the memory operand's address is
  computed into a low register the instruction does not use (found with
  iced's register-usage information, implicit uses included), whose own value
  waits in `R13` meanwhile.
* **Forms that exist only outside 64-bit mode** are re-coded: the one-byte
  `INC`/`DEC`, the `82` group, `moffs` moves.
* **16-bit addressing** (`67` prefix) is computed with `LEA` and cut to 16
  bits.
* **An MMX instruction with a `66` prefix** is decoded without the prefix: the
  console's Pentium III runs it as MMX, the host would run SSE2's form
  (Dr. Muto).

## Flags

The host's flags are the guest's. Nothing the translator adds between guest
instructions may change them, so its own tests and arithmetic use `LEA`,
`MOV`, `MOVZX` and `JRCXZ` only (the dispatcher's table stores `-eip` and
tests `lea ecx, [rcx + r13]` for zero).

IF and AC are not in the host's flags (IF is not the guest's to change in user
mode, AC would turn unaligned accesses into faults). They live in the frame as
`vif` (0 or 0x200) and `vac` (0 or 0x40000). `CLI` and `STI` store to `vif`.
`PUSHFD` builds the image without touching the flags (`pushfq; pop`, then
`LEA`s that swap the host's IF for the two virtual bits). `POPFD` splits the
value and loads the rest with `popfq`. TF is not supported and is dropped.

After `STI`, `POPFD` and `IRETD`, if `RunFlags::exit_when_interruptible` is
set and IF is now 1, `run` returns `Exit::Kicked` with `eip` after the
instruction. (The one-instruction interrupt shadow after `STI` is not
modeled.)

## The stack

64-bit pushes and pops move `RSP` by 8, so stack instructions become stores
and loads through `R12`: `PUSH r32` is `mov gs:[r12d-4], r; lea r12d, [r12-4]`.
The store always comes first: a fault then finds ESP unchanged and the
instruction can simply run again. `POP [mem]`, `PUSHAD`/`POPAD`, the 16-bit
forms, `ENTER n, 0` and `LEAVE` are sequences in the same style.

## FS

The Xbox kernel's per-processor block is addressed through FS. A context's
`fs_base` is loaded into `R14` on entry; an FS-relative operand adds it:
`fs:[0x28]` becomes `gs:[r14d + 0x28]`, `fs:[eax + 4]` becomes
`gs:[eax + r14d + 4]`. An operand that already has a base and an index goes
through `lea r13d, [...]` first. Segment registers read as the flat model's
selectors (CS 0x08, DS/ES/SS 0x10, FS 0x28, as in the reference); loads of
them are ignored.

## String instructions

`MOVS`, `STOS`, `CMPS`, `SCAS`, `LODS` cannot take a segment override for
`ES:[EDI]`. They run with 64-bit addressing instead: `lea rsi, [rsi + r8]` and
`lea rdi, [rdi + r8]` before, `mov esi, esi` and `mov edi, edi` after. Since
the base is a multiple of 4 GiB, the low halves of `RSI` and `RDI` are the
guest's values at every moment, including in the middle of a `REP` that
faults.

## x87, MMX, SSE

Run natively. `run` enters translated code with `FXRSTOR` from the context's
image and every exit does `FXSAVE` back into it, then `FNINIT`, the host's
MXCSR and `CLD`, so Rust code (bus handlers included) always runs with a clean
host floating-point environment and the context is complete whenever `run`
returns. MXCSR bits the host does not implement are cleared before `FXRSTOR`.

The image is the 64-bit-mode `FXSAVE` layout: it also carries XMM8-15, and the
x87 instruction and data pointers are host values. Comparisons between
backends should leave out bytes 6..24 and 288..416.

## Blocks, chaining, calls and returns

A block starts at a guest address and ends at a control transfer, after 256
instructions, at a page boundary once it is 32 instructions long, or before an
instruction it cannot translate. Blocks live in a 128 MiB region; when it
fills, everything is dropped.

* **Direct jumps** are `jmp rel32` to a per-exit stub (`mov r13d, target; jmp
  dispatch`) and are patched to the target block once it exists.
* **The dispatcher** probes a 65536-slot, two-way table indexed by the low 16
  bits of `eip`. A miss leaves translated code; `run` finds or makes the block.
* **Calls** store the guest return address on the guest stack, push its
  negation on the shadow stack and make a host `call`. **Returns** load the
  guest return address, compare it with the shadow stack's top without
  touching flags, and on a match execute a host `ret 8`, which the processor's
  return predictor gets right. On a mismatch (a `longjmp`, a replaced return
  address, a thread switch) the shadow stack is emptied and the dispatcher
  takes over. The shadow stack is a private 2 MiB mapping with a guard; a
  guest that calls without ever returning runs into the guard and the fault
  handler empties the stack. Every entry from `run` starts with it empty, so a
  prediction never outlives the code it points into.
* **The trap range** never gets a block: the lookup returns `Exit::Trap`. A
  kernel call costs one dispatcher miss and one exit, most of it
  `FXSAVE`/`FXRSTOR` and the register copies.

## Kicks

`Kicker::kick` sets an atomic flag in the frame. Blocks poll it:
`mov r10, rcx; mov ecx, [r15+kick]; jrcxz +5; jmp kick_exit; mov rcx, r10`.
The poll sits in front of the block's body and a block has two entries. The
dispatcher and backward jumps enter through the poll; forward jumps and
predicted returns skip it. Any endless guest loop contains a backward jump or
an indirect transfer, so it polls, while straight-line chains pay nothing.

A kick is a store to the flag rather than an `mprotect` of a page the code
reads, which would leave a race to manage. A kick that arrives outside `run`
leaves the flag set and the next `run` returns `Kicked` before touching the
context.

## Leaving translated code

There is one way out: the exit stub saves the registers, flags and FXSAVE
image and returns to `run` with a reason in the frame. `run` acts and enters
again. Nothing in translated code calls into Rust, so no Rust frame is ever
above translated code and the bus is only used from `run`.

Reasons: block lookup, kick, `HLT`, a fault, a single step finished, an
instruction `run` executes itself, undecodable bytes or `UD2` (vector 6), an
instruction with no translation (`Exit::Fatal` with its bytes, address and
disassembly, logged once per opcode).

Instructions `run` executes on the saved state: `IN`/`OUT`/`INS`/`OUTS` (the
bus), `CPUID` (`xb_cpu::cpuid`), `RDTSC` (`Bus::tsc`), `RDMSR`/`WRMSR`,
`INT n`, `INT3`, `INT1`, `INTO`, `IRETD` to ring 0, `MOV` to and from control
registers, `BOUND`, `ENTER` with a nesting level, 16-bit `LEAVE`, the BCD
adjusts and `SALC`.

System instructions, following the reference: `WBINVD`, `INVD`, `INVLPG`
and `LGDT` do nothing. `MOV r32, CRn` reads CR0 `0x8001003B`, CR2 the last
page-fault address, CR4 `0x610`, others 0; writes are ignored. `SIDT`/`SGDT`
store what `X64Cpu::set_descriptor_tables` was given; `SMSW` stores `0x3B`.
Debug registers, `CLTS`, `LIDT`, `LLDT`, `LTR`, `LMSW`, `RDPMC`, far transfers,
`SYSENTER`, an `IRETD` to ring 3 and string instructions with an FS source are
not translated (`Exit::Fatal`).

## Faults

A process-wide handler for SIGSEGV, SIGBUS, SIGFPE and SIGILL is installed
once (`SA_SIGINFO | SA_ONSTACK`; `run` gives the thread an alternate stack if
it has none). It keeps a static table of the translators' code ranges. For a
fault whose `rip` is in none of them it calls the previously installed
handler, so Rust's stack-overflow detection and other users keep working.

For a fault in translated code the handler does three things and returns: it
records `rip`, `CR2`, the vector and error code and `R13` in the frame; if the
fault is on the shadow stack's guard it resets `RSP` instead; otherwise it
sets the interrupted `rip` to the fault stub. The stub is the exit stub with
the reason "fault", so the guest registers and flags are stored exactly as the
faulting instruction found them. `run` then maps the host `rip` to the guest
instruction through the block's instruction map (which gives the precise guest
`eip` and whether a register was borrowed) and decides, with its `&mut dyn
Bus` in ordinary context:

1. **A device page.** The instruction is translated alone with its memory
   operand RIP-relative to a 16-byte buffer and run once: `Bus::mmio_read`
   fills the buffer first if the operand is read, `Bus::mmio_write` follows if
   it is written (8- and 16-byte operands as doublewords). `MOVS`/`STOS`/`LODS`
   run element by element in Rust.
2. **A page the mapping or write tracking forbids, or no page.**
   `Bus::page_fault(va, pa, write)`; on `Retry` the instruction runs again in
   place, on `Raise` `run` returns `Exit::Exception` 14 with the faulting
   address and an error code of `present | write << 1`.
3. **A code page** (below).
4. **Other vectors**: divide error (0), invalid opcode (6), general protection
   (13, an unaligned SSE operand), x87 and SIMD exceptions (16, 19) return
   `Exit::Exception` with `eip` at the instruction.

This is sound because the handler never calls the bus or any Rust code that
could be interrupted mid-update: translated code touches no Rust data
structure, and the frame fields the handler writes are only read after the
stub has returned.

## Self-modifying code

A physical page with translated code is write protected in every view. A
guest write to it faults; `run` drops the translations with bytes on that
page, removes the protection, and runs the faulting instruction once on its
own from the scratch area. Execution then continues through the lookup, which
translates from the next instruction, so a block that changes an instruction
further on in itself runs the new one. The page is protected again when code
on it is next translated.

Data beside code (the XDK's libraries keep variables in their code sections)
would make this cycle endless. A write that lands outside every block's bytes
counts against the page; 64 of them within a second and the page is no longer
protected. From then on a change of its code is not noticed. Writes that do
hit translated bytes never count, so a title that reloads code over the same
page for hours stays correct.

`Mapper::map`/`unmap` drop the blocks on the virtual pages they change, and
`invalidate_code` those on a physical range.

## Not done

* Windows host (the `host` module is the seam).
* The translator needs `GS` and RWX memory; hardened hosts that forbid RWX
  mappings cannot run it.
* No "hook" of instructions known to touch devices: every device access
  takes a signal.
