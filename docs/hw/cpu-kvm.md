# The KVM CPU backend

`xb-cpu-kvm` implements `xb_cpu::Cpu` by running the title natively in a
virtual machine with one vCPU. This page says how the guest environment is
built and why.

## What the VM contains

| Piece | Choice | Why |
|---|---|---|
| vCPUs | one | the console has one processor; guest threads are `Context` values |
| Interrupt controller | none in the kernel, none emulated | the HLE kernel delivers interrupts by building call frames, never through the IDT. Without an in-kernel controller `HLT` exits to user space, which the backend uses as its exit instruction |
| Memory slot 0 | guest RAM at guest-physical 0, the backend's own mapping of the RAM memfd | |
| Memory slot 1 | the hidden slot at guest-physical `0xE000_0000`, about 4 MiB of anonymous host memory | page tables, descriptor tables and stubs; outside RAM and outside every device range (`0xFD00_0000` and up) |
| Device registers | no slot | an access to a guest-physical address without a slot exits as MMIO |

The vCPU state: 32-bit protected mode, ring 0, CR0 = `0x8001_0033` (PE, MP,
ET, NE, WP, PG), CR4 = `0x600` (OSFXSR, OSXMMEXCPT), EFER = 0. WP is set so
that `Access::Read` mappings hold for ring-0 code. CS is selector `0x08`,
DS/ES/SS/GS `0x10`, FS `0x20`, the TSS `0x18`; all flat except FS, whose base
is `Regs::fs_base`. The GDT entry for FS carries the same base, so a title that
saves and reloads FS (`push fs` / `pop fs`) keeps it. `fs_base` is an input
only: `run` never writes it back.

CPUID leaves 0 to 2 come from `xb_cpu::cpuid` (Pentium III, no SSE2). No host
leaves are passed through; KVM needs none for this guest. KVM sets the APIC
feature bit from the APIC base register, so that register is cleared before
the table is installed. SSE2 and later instructions still execute if a title
uses them without asking; hardware virtualization cannot turn them off.

The TSC is scaled to 733,333 kHz with `KVM_SET_TSC_KHZ`. If the host cannot
scale, a warning is logged and `RDTSC` runs at the host's rate. `Bus::tsc` is
never called.

Unknown MSRs go to `Bus::rdmsr` / `Bus::wrmsr` through
`KVM_CAP_X86_USER_SPACE_MSR`. MSRs KVM emulates itself (SYSENTER, PAT, MTRR
and so on) never reach the bus. Without the capability the guest takes #GP.

## The hidden slot

```
0xE000_0000  page directory (all 1024 entries present from the start)
0xE000_1000  system page: GDT, TSS, double-fault TSS, IDT
0xE000_2000  stub page: 0xF4 (HLT) everywhere, plus the TLB flush stub
0xE000_3000  double-fault stack
0xE000_4000  trap page: 0xF4 everywhere
0xE000_5000  the 1024 page tables, one flat array indexed by va >> 12
```

Non-PAE paging, 4 KiB pages. Because every directory entry is present,
`Mapper::map`, `map_mmio` and `unmap` are stores into one flat array; page
tables for address ranges nobody maps are never touched and cost no host
memory. Entries are written with accessed and dirty already set, so the
processor never writes to the tables.

Three guest-virtual pages are reserved for the backend: `SYSTEM_VA =
0xFFFE_0000` (system page), `+0x1000` (stub page), `+0x2000` (double-fault
stack). Mapping requests that touch them are refused with an error in the
log.

### TLB

With nested paging the processor caches guest translations across VM exits
and KVM has no call to flush them. When a present entry is changed or
removed, the next `run` first enters the guest at a two-instruction stub
(`mov cr3, eax; hlt`) and then loads the real context. Mapping a page that is
not present needs no flush. The flush costs one extra exit, and only on the
first `run` after such a change.

## Kernel entry: the trap range

Every page of the trap range maps the trap page, which is filled with `HLT`.
When the guest jumps to a trap address it executes one `HLT` and KVM exits.
The backend sees EIP - 1 inside the trap range, sets EIP back by one and
returns `Exit::Trap`. A `HLT` anywhere else is the title's own and returns
`Exit::Halt`.

General registers travel in the `kvm_run` block (`KVM_CAP_SYNC_REGS`), so a
kernel call is one `KVM_RUN` and one `KVM_GET_FPU`.

### Floating-point state

`run` must return the context's `FxState`, so the state is read after every
run. It is written (`KVM_SET_FPU`) only when the context passed in differs
from what the vCPU holds: the backend keeps a copy of the last state it read
and compares 512 bytes. Running the same thread again therefore skips the
write; passing another thread's context, or one the kernel edited, does not.

`kvm_fpu` carries no MXCSR mask and nothing of the reserved tail of the
FXSAVE image; those bytes of the context are left as they were.

## Exceptions and software interrupts

IDT entry *n* is an interrupt gate to `stub page + n`. The processor pushes
its frame on the guest stack and executes the `HLT` there; the address gives
the vector. The backend then reads the frame, puts ESP, EIP and EFLAGS back
to what they were before delivery and classifies the event:

- The frame has an error code if the vector is one that pushes one **and**
  bit 1 of the frame's third word is clear. That word is CS when a code was
  pushed (a ring-0 selector has bit 1 clear) and EFLAGS when not (bit 1 always
  set). This is how `int 0x0d` is told apart from a real #GP.
- The processor sets the resume flag (RF) in the EFLAGS image it pushes for a
  fault and leaves it clear for traps and `INT n`. So below vector 32: RF set
  means a fault (`Exit::Exception`, EIP at the instruction); RF clear on
  vectors 1, 3 and 4 is the trap (`Exit::Exception`, EIP past it); RF clear on
  any other vector is `INT n` (`Exit::SoftwareInterrupt`). Vector 32 and up is
  always a software interrupt. RF is cleared in the context handed back.
- A page fault reads CR2, calls `Bus::page_fault(va, pa, write)` and either
  resumes the instruction or returns `Exit::Exception`.

Delivery writes the 12 or 16 byte frame below the guest's stack pointer; the
other backend does not, so cross-backend comparisons leave that memory out.

If a kick interrupts the guest between delivery and the stub's `HLT`, the
backend finishes the exception first (the guest is not at a state the kernel
may see) and keeps the kick pending.

### Double and triple faults

If the frame cannot be pushed (a bad guest stack), the processor raises a
double fault. Vector 8 is a task gate: the processor switches to a TSS with
its own stack in the hidden slot, so the handler runs. The backend reads the
dying thread's registers from the outgoing TSS and returns `Exit::Fatal` with
EIP, ESP and CR2 in the text; the context holds those registers. If even that
fails (the title replaced the IDT, say) KVM reports a shutdown and the
backend returns `Exit::Fatal` naming a triple fault; the context is then left
as it was on entry, because AMD hosts reset the vCPU. After either, the next
`run` rebuilds the descriptor tables and system registers.

## Device registers and ports

`map_mmio` pages point at guest-physical addresses without a slot. KVM
decodes the instruction and exits with address, size, direction and data; the
backend calls `Bus::mmio_read` / `mmio_write` and re-enters. An 8-byte access
is split into two dword calls. `REP MOVS` to or from device registers arrives
one element per exit. Port I/O, including the string forms, goes to
`Bus::io_read` / `io_write` the same way. The run loop does not return for
any of these.

## Write tracking (`PageProtector`)

Write tracking is `mprotect` on the host mapping behind the RAM slot. Guest
page-table bits are not used for it: `protect` would have to find every
virtual mapping of a physical page and stop the running vCPU to flush, and a
protected page that holds a stack would turn an ordinary push or exception
delivery into a double fault.

The backend maps the RAM memfd twice. KVM's slot uses one mapping (the slot
view); `protect` changes that mapping's protection. The `mprotect` call runs KVM's
MMU notifier, which removes the page from the VM's nested page tables and
flushes the running vCPU before the call returns, so `protect` is synchronous
from any thread. The guest's next forbidden access cannot be mapped and
`KVM_RUN` fails with `EFAULT` before the instruction has any effect. The
backend calls `Bus::page_fault` and re-enters. Protection is by physical page
whatever the virtual address, it is invisible to the guest (no guest
exception, so pushes and exception frames onto a protected page work), and
the host's own view of RAM is a different mapping and never affected. The
backend's second mapping (the host view) is never protected either; it is
used to read exception frames. A `protect` call that changes nothing returns
without a system call.

### Finding the faulting page

`KVM_EXIT_MEMORY_FAULT` would give the guest-physical address, but Linux
does not fill it in for this failure (`exit_reason` is untouched) even where
it advertises `KVM_CAP_MEMORY_FAULT_INFO`. When the exit is annotated the
backend uses it. Otherwise it derives the page:

1. It decodes the instruction at EIP far enough to list the addresses it may
   touch: the ModRM operand, absolute operands, string pointers, a few
   implicit ones, the code bytes themselves, and the stack around ESP
   (`linux/decode.rs`).
2. The first of those that lies in a protected page is reported.
3. If none does, the fault is retried once (another thread may just have
   re-opened the page). If it repeats, every protected page is reported.

Step 2 can name a page the instruction does not touch (the tracker records
one page too many and the instruction faults again on the right one). Step 3
reports more than is touched. Neither can miss a write.

What the handler is told:

- `pa` is always the right physical page. With the decoder `va` is an address
  the instruction uses in that page; from the annotated exit it is the
  page's most recent mapping (or the contiguous window at
  `0x8000_0000 + pa`), page aligned.
- `write` is true for a page protected `Access::Read` (a read cannot fault
  there). For an `Access::None` page the kind of access is not known and is
  reported as a read; if the handler then opens the page for reading only, a
  write faults again and is reported as a write.

### Accesses made by KVM's emulator

When KVM emulates an instruction (one that touches a device register) and
that instruction also touches protected RAM, KVM cannot use the slot view and
hands the access to user space as if it were MMIO at a RAM address. The
backend calls `Bus::page_fault` and performs the access on the host view. If
the handler answers `Raise` here the run ends with `Exit::Fatal`: the
instruction is half done and cannot be turned into a clean exception.

## Kick and the interrupt window

`Kicker::kick` sets a pending flag and, if the CPU thread is inside `run`,
sends it the signal `SIGRTMIN + 7`. The handler runs on the CPU thread and
sets `kvm_run.immediate_exit`. If the signal lands before the `KVM_RUN` system
call, the call returns at once; if during it, the guest is interrupted.
Either way `KVM_RUN` first completes any pending device access, so the guest
is at an instruction boundary. A kick outside `run` only sets the flag and the
next `run` returns `Exit::Kicked` before entering the guest. An unrelated
signal just re-enters. The process must leave that signal's disposition
alone; the backend unblocks it on the CPU thread.

`RunFlags::exit_when_interruptible` returns at once if the context already
has IF set; otherwise it sets `request_interrupt_window` and KVM exits when
the guest can take an interrupt (after the instruction following `STI`).

## Limits

- Linux x86-64 only.
- The guest can execute instructions the console's processor lacks.
- A title that loads its own GDT, IDT or CR3 is on its own; the environment
  is restored only after a fatal exit.
