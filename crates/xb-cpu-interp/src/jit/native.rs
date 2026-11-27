//! AArch64 code for a block, and what that code calls.
//!
//! # The code
//!
//! A compiled block is a C function `fn(cpu, ops) -> status`. It keeps the
//! CPU in `x19` and the block's compiled instructions ([`Uop`]) in `x20`;
//! the guest's registers and flags stay in the [`InterpCpu`], read and
//! written at their field offsets. Per instruction it emits one of:
//!
//! * **Host instructions** for the forms title code is made of: 8-, 16-
//!   and 32-bit moves, `MOVZX`/`MOVSX`, `LEA`, `ADD`/`SUB`/`AND`/`OR`/
//!   `XOR`/`CMP`/`TEST`, `INC`/`DEC`, shifts by a constant and `IMUL`, with
//!   the arithmetic flags recorded lazily exactly as [`Flags::lazy`]
//!   records them, pushes and pops, and the block's final jump, call,
//!   return or conditional jump.
//! * **NEON instructions** for the MMX forms ([`Gen::mmx`]), which keep
//!   their registers in place in the x87 registers' exact 80-bit fields,
//!   and **double-precision instructions** for the common x87 forms
//!   ([`Gen::x87`]): loads, stores, arithmetic, compares and the stack, on
//!   the registers' doubles. What is rare (an empty or full register, a
//!   divisor of zero, a NaN out of numbers) is the interpreter's.
//! * **A memory access** for those forms' memory operands. The common case
//!   is made in place with the checks `Mem` makes: the access lies in one
//!   page, the page-table entry allows it and is not a device's, the
//!   physical page is in RAM, write tracking has not restricted it and, for
//!   a store, no decoded code is on it. Anything else calls a helper
//!   ([`h_load`], [`h_store`]), which is the interpreter's own `load` and
//!   `store`: faults, device registers and self-modifying code are theirs.
//! * **A call to the instruction's handler** ([`h_uop`]) for everything
//!   else. The generator therefore never lacks an instruction the
//!   interpreter has.
//!
//! # From block to block
//!
//! A block that runs to its end stores `eip` and branches to the
//! dispatcher, one routine at the start of the code buffer
//! ([`dispatcher`]). It does in host code what the run loop does between
//! blocks: it leaves for the run loop if the CPU was kicked, if `eip` is in
//! the trap range or if write tracking has a restriction the thread has not
//! acknowledged; it then finds `eip`'s block in the cache's
//! [`Link`](crate::cache::Link)s and, when the block is compiled and its
//! page-table entries still hold, records its index and branches into its
//! code past the prologue (the frame is the same for every block). Anything
//! else returns 0 to the run loop, which carries on as for one block.
//!
//! # Leaving the code
//!
//! The function returns 0 when a block ran to its end, with `eip` stored.
//! A helper that cannot simply continue returns a status the code passes
//! straight back: [`STOPPED`] with the instruction's index when the
//! instruction ended in anything but "next" (a fault, an exit, a jump taken
//! by a handler), its result left for [`State::take_step`]; or [`ENDED`]
//! when a store changed code and the block must not go on, with `eip`
//! stored past the instruction. Host instructions change guest state only
//! after their helpers have succeeded, so a stopped instruction has changed
//! nothing and the run loop treats it as it treats the interpreter's.
//!
//! # Safety
//!
//! Generated code is entered only through [`run`], which holds exclusive
//! borrows of the CPU and the bus for the call and hands the code raw
//! pointers to them. The helpers turn those pointers back into references;
//! nothing else touches either object until the code returns.
//!
//! The code reads the page table, the restriction and code-flag tables and
//! guest RAM through the addresses of `Mem::tables`, which outlive every
//! block (the cache and the code buffer belong to the same CPU as the
//! tables). It indexes them only with a guest page number (the page table
//! has an entry for every one) or a physical page it has compared with the
//! number of RAM pages. RAM is shared with other threads as it is for
//! `xb_mem::Ram`'s copies: no Rust reference to it exists.
#![expect(unsafe_code, reason = "runs generated code, which calls back with raw pointers")]

use std::mem::offset_of;
use std::ops::ControlFlow;
use std::ptr::NonNull;

use iced_x86::{Code as C, Mnemonic, OpKind, Register};
use xb_cpu::Bus;

use super::a64::Asm;
use super::macos::CodeBuf;
use crate::cpu::{Flow, InterpCpu, Snap, Step, Stop};
use crate::fast::{ADD, AND, CMP, Ea, NONE, OR, SUB, TEST, Uop, XOR, ea_of, is_jcc, small_alu};
use crate::flags::{ARITH, Op};

/// Bytes of generated code kept before everything is dropped.
const CODE_BYTES: usize = 64 << 20;

/// Status: an instruction stopped (see the module docs).
const STOPPED: u64 = super::STOPPED << 32;
/// Status: the block ended early with `eip` stored.
const ENDED: u64 = 2 << 32;

/// Indices into [`State::helpers`].
const H_UOP: u32 = 0;
const H_LOAD: u32 = 1;
const H_STORE: u32 = 2;
const H_COND: u32 = 3;
const H_CF: u32 = 4;
const H_LOAD64: u32 = 5;
const H_STORE64: u32 = 6;
const H_FAPPLY: u32 = 7;

/// Instructions of a block before its body: the prologue and the loads of
/// the tables and the dispatcher's pointers ([`generate`]). The dispatcher
/// enters a block here.
const PROLOGUE: usize = 16;

/// Registers holding [`State::chain`] for the dispatcher: the links, the
/// kick flag and the generation counter.
const LINKS: u32 = 25;
const KICK: u32 = 26;
const GENERATION: u32 = 27;

/// A block's code past the prologue, which the dispatcher branches to.
pub(crate) fn body(entry: usize) -> usize {
    entry + 4 * PROLOGUE
}

const _: () = assert!(crate::cache::FAST == 1 << 14, "the dispatcher masks with 0x3fff");
const _: () = assert!(size_of::<crate::cache::Link>() == 32, "the dispatcher indexes with lsl #5");

/// Where generated code finds what it uses in the [`InterpCpu`].
struct Off {
    gpr: u32,
    eip: u32,
    fs: u32,
    bits: u32,
    op: u32,
    size: u32,
    a: u32,
    b: u32,
    res: u32,
    helpers: u32,
    tables: u32,
    /// The x87 registers (`Fpu::st`), TOP and the tag bits.
    st: u32,
    top: u32,
    valid: u32,
    /// A 64-bit value a helper leaves for the code ([`State::tmp`]).
    tmp: u32,
    /// The x87 control and status words.
    fcw: u32,
    fsw: u32,
    /// [`State::chain`], [`State::trap_lo`], [`State::trap_hi`],
    /// [`State::cur`] and `Mem::seen`.
    chain: u32,
    trap_lo: u32,
    trap_hi: u32,
    cur: u32,
    seen: u32,
}

impl Off {
    fn get() -> Self {
        let o = |n: usize| n as u32;
        Self {
            gpr: o(offset_of!(InterpCpu, gpr)),
            eip: o(offset_of!(InterpCpu, eip)),
            fs: o(offset_of!(InterpCpu, fs_base)),
            bits: o(offset_of!(InterpCpu, flags.bits)),
            op: o(offset_of!(InterpCpu, flags.op)),
            size: o(offset_of!(InterpCpu, flags.size)),
            a: o(offset_of!(InterpCpu, flags.a)),
            b: o(offset_of!(InterpCpu, flags.b)),
            res: o(offset_of!(InterpCpu, flags.res)),
            helpers: o(offset_of!(InterpCpu, jit.helpers)),
            tables: o(offset_of!(InterpCpu, jit.tables)),
            st: o(offset_of!(InterpCpu, fpu.st)),
            top: o(offset_of!(InterpCpu, fpu.top)),
            valid: o(offset_of!(InterpCpu, fpu.valid)),
            fcw: o(offset_of!(InterpCpu, fpu.fcw)),
            fsw: o(offset_of!(InterpCpu, fpu.fsw)),
            tmp: o(offset_of!(InterpCpu, jit.tmp)),
            chain: o(offset_of!(InterpCpu, jit.chain)),
            trap_lo: o(offset_of!(InterpCpu, jit.trap_lo)),
            trap_hi: o(offset_of!(InterpCpu, jit.trap_hi)),
            cur: o(offset_of!(InterpCpu, jit.cur)),
            seen: o(offset_of!(InterpCpu, mem.seen)),
        }
    }

    /// Whether every offset fits the addressing forms the generator uses.
    fn fits(&self) -> bool {
        let word = |v: u32| v.is_multiple_of(4) && v < 16384;
        [self.gpr + 28, self.eip, self.fs, self.bits, self.size, self.a, self.b, self.res]
            .into_iter()
            .all(word)
            && self.op < 4096
            // Byte registers are reached with `ldrb`/`strb`.
            && self.gpr + 32 < 4096
            // The x87 registers' flags with `strb`, their words with `ldr x`.
            && self.st.is_multiple_of(8)
            && self.st + 8 * FPR < 4096
            && self.top.is_multiple_of(4)
            && self.top < 16384
            && self.valid < 4096
            && [self.fcw, self.fsw].into_iter().all(|v| v.is_multiple_of(2) && v < 8192)
            && [self.tmp, self.chain, self.trap_hi, self.seen]
                .into_iter()
                .all(|v| v.is_multiple_of(8) && v + 16 < 32768)
            && [self.trap_lo, self.cur].into_iter().all(|v| v.is_multiple_of(4) && v < 16384)
            && [self.helpers, self.tables]
                .into_iter()
                .all(|v| v.is_multiple_of(8) && v + 64 < 32768)
    }
}

/// The code generator's part of an [`InterpCpu`].
pub(crate) struct State {
    buf: Option<CodeBuf>,
    off: Off,
    /// The bus of the [`run`] in progress.
    bus: Option<NonNull<dyn Bus>>,
    /// The result of the instruction generated code stopped at.
    step: Option<Step>,
    /// A panic a helper caught, for [`run`] to resume.
    panic: Option<Box<dyn std::any::Any + Send>>,
    /// The helpers' addresses, which generated code loads from here.
    helpers: [usize; 8],
    /// The value [`h_load64`] loaded.
    tmp: u64,
    /// For the dispatcher: the cache's link table, the kick flag and the
    /// write-tracking generation counter.
    chain: [usize; 3],
    /// The trap range, `[trap_lo, trap_hi)`, as the run loop has it.
    trap_lo: u32,
    trap_hi: u64,
    /// The index of the block whose code is running.
    cur: u32,
    /// The dispatcher's address, when there is a code buffer.
    dispatch: usize,
    /// The addresses of the page table, the restriction per physical page,
    /// RAM and the code flag per physical page (`Mem::tables`), which
    /// generated code loads from here on entry.
    tables: [usize; 4],
    /// Physical pages of RAM.
    pages: u32,
    /// Blocks run as host code, for the tests.
    runs: u64,
}

// SAFETY: `bus` is set and cleared inside one `run` call and dereferenced
// only by helpers that call makes; between calls it is `None`. The rest is
// `Send`.
unsafe impl Send for State {}

impl State {
    /// `tables` and `pages` are `Mem::tables` of the CPU this state is for.
    /// `chain` is what [`dispatcher`] reads: the cache's links, the kick
    /// flag and the generation counter, each at an address that lasts as
    /// long as the CPU.
    pub(crate) fn new(tables: [usize; 4], pages: u32, chain: [usize; 3]) -> Self {
        let off = Off::get();
        let buf = if off.fits() {
            match CodeBuf::new(CODE_BYTES) {
                Ok(buf) => Some(buf),
                Err(e) => {
                    log::info!(target: "cpu", "interp: no memory for generated code ({e}): interpreting only");
                    None
                }
            }
        } else {
            log::info!(target: "cpu", "interp: the CPU's layout does not fit the code generator: interpreting only");
            None
        };
        let mut state = Self {
            buf,
            off,
            bus: None,
            step: None,
            panic: None,
            helpers: [
                h_uop as *const () as usize,
                h_load as *const () as usize,
                h_store as *const () as usize,
                h_cond as *const () as usize,
                h_cf as *const () as usize,
                h_load64 as *const () as usize,
                h_store64 as *const () as usize,
                h_fapply as *const () as usize,
            ],
            tmp: 0,
            chain,
            trap_lo: 0,
            trap_hi: 0,
            cur: 0,
            dispatch: 0,
            tables,
            pages,
            runs: 0,
        };
        state.place_dispatcher();
        state
    }

    /// Puts the dispatcher at the start of the (empty) code buffer.
    fn place_dispatcher(&mut self) {
        let code = dispatcher(&self.off);
        self.dispatch = self.buf.as_mut().and_then(|b| b.push(&code)).unwrap_or(0);
        if self.dispatch == 0 {
            self.buf = None;
        }
    }

    /// The trap range, as `InterpCpu::set_trap_range` sets it.
    pub(crate) fn set_trap(&mut self, trap: (u32, u64)) {
        (self.trap_lo, self.trap_hi) = trap;
    }

    /// The block whose code ran last, for a status that stopped in it.
    pub(crate) fn current(&self) -> u32 {
        self.cur
    }

    /// Compiles a block: `Continue(Some(entry))`, `Continue(None)` when
    /// this CPU generates no code, or `Break` when the code buffer is full
    /// (the caller drops every block and calls [`State::reset`]).
    pub(crate) fn compile(&mut self, ops: &[Uop]) -> ControlFlow<(), Option<usize>> {
        let Some(buf) = &mut self.buf else {
            return ControlFlow::Continue(None);
        };
        let code = generate(ops, &self.off, self.pages, buf.next(), self.dispatch);
        match buf.push(&code) {
            Some(entry) => ControlFlow::Continue(Some(entry)),
            None => ControlFlow::Break(()),
        }
    }

    /// Forgets all generated code. No block may keep an entry.
    pub(crate) fn reset(&mut self) {
        if let Some(buf) = &mut self.buf {
            buf.reset();
            self.place_dispatcher();
        }
    }

    /// The result of the instruction the last [`run`] stopped at.
    pub(crate) fn take_step(&mut self) -> Option<Step> {
        self.step.take()
    }

    /// Blocks run as host code so far, if this CPU generates code.
    #[cfg(test)]
    pub(crate) fn native_runs(&self) -> Option<u64> {
        self.buf.as_ref().map(|_| self.runs)
    }
}

/// Runs the generated code at `entry`, compiled from exactly `ops` (block
/// `block`'s), and the compiled blocks it goes on to; returns the status
/// (module docs).
pub(crate) fn run(
    entry: usize,
    cpu: &mut InterpCpu,
    ops: &[Uop],
    block: u32,
    bus: &mut dyn Bus,
) -> u64 {
    // SAFETY: only the trait object's lifetime changes. The pointer is
    // cleared below, before the borrow it came from ends.
    let bus = unsafe {
        std::mem::transmute::<NonNull<dyn Bus + '_>, NonNull<dyn Bus + 'static>>(NonNull::from(bus))
    };
    cpu.jit.bus = Some(bus);
    cpu.jit.cur = block;
    cpu.jit.runs += 1;
    // SAFETY: `entry` is an address `State::compile` returned for these
    // ops, in a buffer not reset since (the cache drops every entry when it
    // resets the buffer), and the code there is a C function of this type.
    let code: unsafe extern "C" fn(*mut InterpCpu, *const Uop) -> u64 =
        unsafe { std::mem::transmute(entry) };
    // SAFETY: the code reaches the CPU, the ops and (through the helpers)
    // the bus only by these pointers, all from borrows this function holds
    // and does not use until the call returns.
    let status = unsafe { code(std::ptr::from_mut(cpu), ops.as_ptr()) };
    cpu.jit.bus = None;
    if let Some(payload) = cpu.jit.panic.take() {
        std::panic::resume_unwind(payload);
    }
    status
}

/// Runs `f` with the CPU and bus of the [`run`] in progress and returns its
/// status. A panic in `f` (a bus handler's, say) must not unwind through
/// generated code: it is kept, the code is told to return, and [`run`]
/// resumes it.
///
/// # Safety
/// Called from a helper generated code called, with the CPU pointer that
/// code was given.
unsafe fn guarded(
    cpu: *mut InterpCpu,
    i: u32,
    f: impl FnOnce(&mut InterpCpu, &mut dyn Bus) -> u64,
) -> u64 {
    let run = std::panic::AssertUnwindSafe(|| {
        // SAFETY: `run` passed this pointer from its exclusive borrow, which
        // it does not use while the code runs, and generated code holds no
        // reference into the CPU across a call.
        let cpu = unsafe { &mut *cpu };
        // SAFETY: as above, for the bus `run` stored: a different object.
        match cpu.jit.bus.map(|mut b| unsafe { b.as_mut() }) {
            Some(bus) => f(cpu, bus),
            None => stopped(cpu, i, Stop::Fatal("interp: generated code ran outside a run".into())),
        }
    });
    match std::panic::catch_unwind(run) {
        Ok(status) => status,
        Err(payload) => {
            // SAFETY: the references made above ended with the closure.
            unsafe { (*cpu).jit.panic = Some(payload) };
            STOPPED | u64::from(i)
        }
    }
}

/// Leaves `stop` as instruction `i`'s result, with the registers it found.
fn stopped(cpu: &mut InterpCpu, i: u32, stop: Stop) -> u64 {
    cpu.snap = Snap { gpr: cpu.gpr, flags: cpu.flags };
    cpu.jit.step = Some(Err(stop));
    STOPPED | u64::from(i)
}

/// Runs instruction `i` of the block by its handler.
///
/// # Safety
/// Called by generated code with the pointers [`run`] gave it; `u` is
/// instruction `i` of the block's ops.
unsafe extern "C" fn h_uop(cpu: *mut InterpCpu, u: *const Uop, i: u32) -> u64 {
    // SAFETY: `u` points into the ops `run` borrows for the whole call.
    let u = unsafe { &*u };
    let body = |cpu: &mut InterpCpu, bus: &mut dyn Bus| {
        cpu.snap = Snap { gpr: cpu.gpr, flags: cpu.flags };
        match (u.h)(cpu, u, bus) {
            Ok(Flow::Next) => {
                if cpu.mem.smc.is_empty() {
                    0
                } else {
                    cpu.eip = u.next;
                    ENDED
                }
            }
            step => {
                cpu.jit.step = Some(step);
                STOPPED | u64::from(i)
            }
        }
    };
    // SAFETY: the caller's contract.
    unsafe { guarded(cpu, i, body) }
}

/// Loads `bits` (8, 16 or 32) at `va` for instruction `i`: the value, zero
/// extended, or a status with bit 32 set.
///
/// # Safety
/// As [`h_uop`].
unsafe extern "C" fn h_load(cpu: *mut InterpCpu, va: u32, i: u32, bits: u32) -> u64 {
    let body = |cpu: &mut InterpCpu, bus: &mut dyn Bus| match cpu.load(va, bits, bus) {
        Ok(v) => u64::from(v),
        Err(stop) => stopped(cpu, i, stop),
    };
    // SAFETY: the caller's contract.
    unsafe { guarded(cpu, i, body) }
}

/// Stores the low `bits` (8, 16 or 32) of `v` at `va` for instruction `i`:
/// 0, [`STOPPED`] (nothing was stored), or [`ENDED`] when the store changed
/// code (`eip` is then stored past the instruction; one that is not
/// complete yet stores its own).
///
/// # Safety
/// As [`h_uop`].
unsafe extern "C" fn h_store(
    cpu: *mut InterpCpu,
    va: u32,
    v: u32,
    u: *const Uop,
    i: u32,
    bits: u32,
) -> u64 {
    // SAFETY: `u` points into the ops `run` borrows for the whole call.
    let next = unsafe { (*u).next };
    let body = |cpu: &mut InterpCpu, bus: &mut dyn Bus| match cpu.store(va, bits, v, bus) {
        Ok(()) if cpu.mem.smc.is_empty() => 0,
        Ok(()) => {
            cpu.eip = next;
            ENDED
        }
        Err(stop) => stopped(cpu, i, stop),
    };
    // SAFETY: the caller's contract.
    unsafe { guarded(cpu, i, body) }
}

/// Loads 64 bits at `va` for instruction `i` into [`State::tmp`]: 0, or a
/// [`STOPPED`] status.
///
/// # Safety
/// As [`h_uop`].
unsafe extern "C" fn h_load64(cpu: *mut InterpCpu, va: u32, i: u32) -> u64 {
    let body = |cpu: &mut InterpCpu, bus: &mut dyn Bus| match cpu.load64(va, bus) {
        Ok(v) => {
            cpu.jit.tmp = v;
            0
        }
        Err(stop) => stopped(cpu, i, stop),
    };
    // SAFETY: the caller's contract.
    unsafe { guarded(cpu, i, body) }
}

/// Stores the 64 bits `v` at `va` for instruction `i`, with the statuses
/// of [`h_store`].
///
/// # Safety
/// As [`h_uop`].
unsafe extern "C" fn h_store64(cpu: *mut InterpCpu, va: u32, v: u64, u: *const Uop, i: u32) -> u64 {
    // SAFETY: `u` points into the ops `run` borrows for the whole call.
    let next = unsafe { (*u).next };
    let body = |cpu: &mut InterpCpu, bus: &mut dyn Bus| match cpu.store64(va, v, bus) {
        Ok(()) if cpu.mem.smc.is_empty() => 0,
        Ok(()) => {
            cpu.eip = next;
            ENDED
        }
        Err(stop) => stopped(cpu, i, stop),
    };
    // SAFETY: the caller's contract.
    unsafe { guarded(cpu, i, body) }
}

/// `ST(d) = ST(d) op s` as the interpreter does it (`s` as a double's bits),
/// for the cases generated code leaves to it: a divisor of zero and a NaN
/// result.
///
/// # Safety
/// As [`h_uop`].
unsafe extern "C" fn h_fapply(cpu: *mut InterpCpu, op: u32, d: u32, s: u64) -> u64 {
    // SAFETY: the caller's contract; no bus access.
    let cpu = unsafe { &mut *cpu };
    cpu.fapply(op, d, f64::from_bits(s));
    0
}

/// Whether condition `cc` holds.
///
/// # Safety
/// As [`h_uop`].
unsafe extern "C" fn h_cond(cpu: *mut InterpCpu, cc: u32) -> u32 {
    // SAFETY: the caller's contract; only the flags are read.
    let cpu = unsafe { &*cpu };
    u32::from(cpu.flags.cond(cc))
}

/// The carry flag, 0 or 1, for `INC` and `DEC`, which keep it.
///
/// # Safety
/// As [`h_uop`].
unsafe extern "C" fn h_cf(cpu: *mut InterpCpu) -> u32 {
    // SAFETY: the caller's contract; only the flags are read.
    let cpu = unsafe { &*cpu };
    cpu.flags.cf()
}

/// The arithmetic forms compiled to host instructions: the operation and
/// whether the form is `r OP= rm` (0), `rm OP= r` (1) or `rm OP= imm` (2).
fn alu_form(code: C) -> Option<(u8, u8)> {
    Some(match code {
        C::Add_r32_rm32 => (ADD, 0),
        C::Or_r32_rm32 => (OR, 0),
        C::And_r32_rm32 => (AND, 0),
        C::Sub_r32_rm32 => (SUB, 0),
        C::Xor_r32_rm32 => (XOR, 0),
        C::Cmp_r32_rm32 => (CMP, 0),
        C::Add_rm32_r32 => (ADD, 1),
        C::Or_rm32_r32 => (OR, 1),
        C::And_rm32_r32 => (AND, 1),
        C::Sub_rm32_r32 => (SUB, 1),
        C::Xor_rm32_r32 => (XOR, 1),
        C::Cmp_rm32_r32 => (CMP, 1),
        C::Test_rm32_r32 => (TEST, 1),
        C::Add_rm32_imm32 | C::Add_rm32_imm8 | C::Add_EAX_imm32 => (ADD, 2),
        C::Or_rm32_imm32 | C::Or_rm32_imm8 | C::Or_EAX_imm32 => (OR, 2),
        C::And_rm32_imm32 | C::And_rm32_imm8 | C::And_EAX_imm32 => (AND, 2),
        C::Sub_rm32_imm32 | C::Sub_rm32_imm8 | C::Sub_EAX_imm32 => (SUB, 2),
        C::Xor_rm32_imm32 | C::Xor_rm32_imm8 | C::Xor_EAX_imm32 => (XOR, 2),
        C::Cmp_rm32_imm32 | C::Cmp_rm32_imm8 | C::Cmp_EAX_imm32 => (CMP, 2),
        C::Test_rm32_imm32 | C::Test_rm32_imm32_F7r1 | C::Test_EAX_imm32 => (TEST, 2),
        _ => return None,
    })
}

/// The NEON instructions of a binary MMX form, `v0 = v0 OP v1` on the low
/// 64 bits (the destination in `v0`, the source in `v1`; `v2` is scratch).
/// Each word is what Apple's assembler gives for the text beside it; the
/// lanes and rules are the interpreter's (`simd::mmx_int`).
fn mmx_words(code: C) -> Option<&'static [u32]> {
    Some(match code {
        C::Paddb_mm_mmm64 => &[0x0E21_8400],   // add v0.8b, v0.8b, v1.8b
        C::Paddw_mm_mmm64 => &[0x0E61_8400],   // add v0.4h
        C::Paddd_mm_mmm64 => &[0x0EA1_8400],   // add v0.2s
        C::Paddsb_mm_mmm64 => &[0x0E21_0C00],  // sqadd v0.8b
        C::Paddsw_mm_mmm64 => &[0x0E61_0C00],  // sqadd v0.4h
        C::Paddusb_mm_mmm64 => &[0x2E21_0C00], // uqadd v0.8b
        C::Paddusw_mm_mmm64 => &[0x2E61_0C00], // uqadd v0.4h
        C::Psubb_mm_mmm64 => &[0x2E21_8400],   // sub v0.8b
        C::Psubw_mm_mmm64 => &[0x2E61_8400],   // sub v0.4h
        C::Psubd_mm_mmm64 => &[0x2EA1_8400],   // sub v0.2s
        C::Psubsb_mm_mmm64 => &[0x0E21_2C00],  // sqsub v0.8b
        C::Psubsw_mm_mmm64 => &[0x0E61_2C00],  // sqsub v0.4h
        C::Psubusb_mm_mmm64 => &[0x2E21_2C00], // uqsub v0.8b
        C::Psubusw_mm_mmm64 => &[0x2E61_2C00], // uqsub v0.4h
        C::Pand_mm_mmm64 => &[0x0E21_1C00],    // and v0.8b
        C::Pandn_mm_mmm64 => &[0x0E60_1C20],   // bic v0.8b, v1.8b, v0.8b
        C::Por_mm_mmm64 => &[0x0EA1_1C00],     // orr v0.8b
        C::Pxor_mm_mmm64 => &[0x2E21_1C00],    // eor v0.8b
        C::Pcmpeqb_mm_mmm64 => &[0x2E21_8C00], // cmeq v0.8b
        C::Pcmpeqw_mm_mmm64 => &[0x2E61_8C00], // cmeq v0.4h
        C::Pcmpeqd_mm_mmm64 => &[0x2EA1_8C00], // cmeq v0.2s
        C::Pcmpgtb_mm_mmm64 => &[0x0E21_3400], // cmgt v0.8b
        C::Pcmpgtw_mm_mmm64 => &[0x0E61_3400], // cmgt v0.4h
        C::Pcmpgtd_mm_mmm64 => &[0x0EA1_3400], // cmgt v0.2s
        C::Pmullw_mm_mmm64 => &[0x0E61_9C00],  // mul v0.4h
        // smull v2.4s, v0.4h, v1.4h; shrn v0.4h, v2.4s, #16
        C::Pmulhw_mm_mmm64 => &[0x0E61_C002, 0x0F10_8440],
        // umull v2.4s, v0.4h, v1.4h; shrn v0.4h, v2.4s, #16
        C::Pmulhuw_mm_mmm64 => &[0x2E61_C002, 0x0F10_8440],
        // smull v2.4s, v0.4h, v1.4h; addp v0.4s, v2.4s, v2.4s
        C::Pmaddwd_mm_mmm64 => &[0x0E61_C002, 0x4EA2_BC40],
        C::Pavgb_mm_mmm64 => &[0x2E21_1400],  // urhadd v0.8b
        C::Pavgw_mm_mmm64 => &[0x2E61_1400],  // urhadd v0.4h
        C::Pmaxsw_mm_mmm64 => &[0x0E61_6400], // smax v0.4h
        C::Pminsw_mm_mmm64 => &[0x0E61_6C00], // smin v0.4h
        C::Pmaxub_mm_mmm64 => &[0x2E21_6400], // umax v0.8b
        C::Pminub_mm_mmm64 => &[0x2E21_6C00], // umin v0.8b
        // uabd v0.8b, v0.8b, v1.8b; uaddlv h0, v0.8b
        C::Psadbw_mm_mmm64 => &[0x2E21_7400, 0x2E30_3800],
        C::Punpcklbw_mm_mmm32 => &[0x0E01_3800], // zip1 v0.8b
        C::Punpckhbw_mm_mmm64 => &[0x0E01_7800], // zip2 v0.8b
        C::Punpcklwd_mm_mmm32 => &[0x0E41_3800], // zip1 v0.4h
        C::Punpckhwd_mm_mmm64 => &[0x0E41_7800], // zip2 v0.4h
        C::Punpckldq_mm_mmm32 => &[0x0E81_3800], // zip1 v0.2s
        C::Punpckhdq_mm_mmm64 => &[0x0E81_7800], // zip2 v0.2s
        // mov v0.d[1], v1.d[0]; sqxtn v0.8b, v0.8h
        C::Packsswb_mm_mmm64 => &[0x6E18_0420, 0x0E21_4800],
        // mov v0.d[1], v1.d[0]; sqxtun v0.8b, v0.8h
        C::Packuswb_mm_mmm64 => &[0x6E18_0420, 0x2E21_2800],
        // mov v0.d[1], v1.d[0]; sqxtn v0.4h, v0.4s
        C::Packssdw_mm_mmm64 => &[0x6E18_0420, 0x0E61_4800],
        _ => return None,
    })
}

/// An MMX shift by an immediate: lane bits and kind (0 left, 1 logical
/// right, 2 arithmetic right), as `simd::shift_lanes` takes them.
fn mmx_shift(code: C) -> Option<(u32, u8)> {
    Some(match code {
        C::Psllw_mm_imm8 => (16, 0),
        C::Pslld_mm_imm8 => (32, 0),
        C::Psllq_mm_imm8 => (64, 0),
        C::Psrlw_mm_imm8 => (16, 1),
        C::Psrld_mm_imm8 => (32, 1),
        C::Psrlq_mm_imm8 => (64, 1),
        C::Psraw_mm_imm8 => (16, 2),
        C::Psrad_mm_imm8 => (32, 2),
        _ => return None,
    })
}

/// Whether `m` is a `SETcc`.
fn is_setcc(m: Mnemonic) -> bool {
    use Mnemonic as M;
    matches!(
        m,
        M::Seto
            | M::Setno
            | M::Setb
            | M::Setae
            | M::Sete
            | M::Setne
            | M::Setbe
            | M::Seta
            | M::Sets
            | M::Setns
            | M::Setp
            | M::Setnp
            | M::Setl
            | M::Setge
            | M::Setle
            | M::Setg
    )
}

/// A 32-bit general register's number, 0-7.
fn gpr32(r: Register) -> Option<u8> {
    let n = r as u32;
    (Register::EAX as u32..=Register::EDI as u32)
        .contains(&n)
        .then(|| (n - Register::EAX as u32) as u8)
}

/// An MMX register operand's number, 0-7.
fn mm_reg(r: Register) -> Option<u8> {
    let n = r as u32;
    (Register::MM0 as u32..=Register::MM7 as u32)
        .contains(&n)
        .then(|| (n - Register::MM0 as u32) as u8)
}

/// The dispatcher (module docs): entered by a branch from a block's end,
/// with the frame and the registers of [`generate`]'s prologue and `w9` =
/// the `eip` the block stored.
fn dispatcher(o: &Off) -> Vec<u32> {
    let mut a = Asm::default();
    let mut out = Vec::new();
    // Kicked.
    a.ldrb(10, KICK, 0);
    out.push(a.cbnz_w(10));
    // In the trap range.
    a.ldr_w(10, 19, o.trap_lo);
    a.cmp_w(9, 10);
    let below = a.b_cond(3); // b.lo
    a.ldr_x(10, 19, o.trap_hi);
    a.word(0xEB0A_013F); // cmp x9, x10
    out.push(a.b_cond(3)); // b.lo
    let here = a.pos();
    a.patch(below, here);
    // A restriction not yet acknowledged. A plain load is enough to see it
    // (soon after it is made): the run loop acknowledges it, with the
    // ordering `Mem::poll` gives.
    a.ldr_x(10, GENERATION, 0);
    a.ldr_x(11, 19, o.seen);
    a.word(0xEB0B_015F); // cmp x10, x11
    out.push(a.b_cond(1)); // b.ne
    // The link for eip.
    a.word(0x1200_352B); // and w11, w9, #0x3fff
    a.word(0x8B0B_172A); // add x10, x25, x11, lsl #5
    a.ldr_w(12, 10, 0);
    a.cmp_w(12, 9);
    out.push(a.b_cond(1));
    a.ldr_x(13, 10, 16);
    out.push(a.cbz_x(13));
    // Its page-table entries still hold.
    a.lsr12(12, 9);
    a.ldr_w_idx4(12, PT, 12);
    a.ldr_w(14, 10, 4);
    a.cmp_w(12, 14);
    out.push(a.b_cond(1));
    a.ldr_w(14, 10, 8);
    let one_page = a.cbz_w(14);
    a.lsr12(12, 9);
    a.add_w_imm(12, 12, 1);
    a.word(0x1200_4D8C); // and w12, w12, #0xfffff
    a.ldr_w_idx4(12, PT, 12);
    a.cmp_w(12, 14);
    out.push(a.b_cond(1));
    let here = a.pos();
    a.patch(one_page, here);
    // Into it.
    a.ldr_w(12, 10, 12);
    a.str_w(12, 19, o.cur);
    a.ldr_x(20, 10, 24);
    a.word(0xD61F_01A0); // br x13
    let here = a.pos();
    for at in out {
        a.patch(at, here);
    }
    a.zero_x0();
    a.epilogue();
    a.code
}

/// Generates the code of a block for a machine with `pages` pages of RAM,
/// to be placed at `base`, with the dispatcher at `dispatch`.
fn generate(ops: &[Uop], o: &Off, pages: u32, base: usize, dispatch: usize) -> Vec<u32> {
    let mut g =
        Gen { a: Asm::default(), o, pages, exits: Vec::new(), operands: None, base, dispatch };
    g.a.prologue();
    for k in 0..4 {
        g.a.ldr_x(PT + k, 19, o.tables + 8 * k);
    }
    for (k, reg) in [LINKS, KICK, GENERATION].into_iter().enumerate() {
        g.a.ldr_x(reg, 19, o.chain + 8 * k as u32);
    }
    debug_assert_eq!(g.a.pos(), PROLOGUE);
    let mut open = true;
    for (i, u) in ops.iter().enumerate() {
        open = g.uop(i as u32, u);
    }
    if open && let Some(last) = ops.last() {
        g.set_eip(last.next);
        g.finish();
    }
    let exit = g.a.pos();
    g.a.epilogue();
    for at in std::mem::take(&mut g.exits) {
        g.a.patch(at, exit);
    }
    g.a.code
}

/// An `Asm` load or store of a register at an offset (`ldrb`, `strh`, ...).
type Access = fn(&mut Asm, u32, u32, u32);

/// Bytes of one x87 register ([`crate::f80::Fpr`]).
const FPR: u32 = size_of::<crate::f80::Fpr>() as u32;

/// Guest ESP's register number.
const ESP: u8 = 4;

/// The registers holding the tables of [`State::tables`], in its order.
const PT: u32 = 21;
const RESTRICTION: u32 = 22;
const RAM: u32 = 23;
const CODE: u32 = 24;

/// Bit numbers of the page-table entry flags (`mem`).
const PTE_READ: u32 = crate::mem::READ.trailing_zeros();
const PTE_WRITE: u32 = crate::mem::WRITE.trailing_zeros();
const PTE_MMIO: u32 = crate::mem::MMIO.trailing_zeros();

/// One block's code under construction. Scratch registers: `w0`-`w4` for
/// helper arguments and results, `w9` for constants and addresses, `w10`
/// to `w15` for values; none is kept across a helper call.
struct Gen<'a> {
    a: Asm,
    o: &'a Off,
    pages: u32,
    /// Branches to the epilogue, patched when its place is known.
    exits: Vec<usize>,
    /// The instruction whose operands are still in `w10`, `w11` and `w12`
    /// and how its flags were recorded: a conditional jump right after it
    /// can test them with host flags.
    operands: Option<(u32, Op)>,
    /// Where the code will be placed, and the dispatcher's address.
    base: usize,
    dispatch: usize,
}

impl Gen<'_> {
    fn gpr(&self, r: u8) -> u32 {
        self.o.gpr + 4 * u32::from(r)
    }

    /// The byte of 8-bit register `r` (AL CL DL BL AH CH DH BH): the low
    /// byte of the first four registers, or the second byte for AH to BH.
    fn gpr8(&self, r: u8) -> u32 {
        self.o.gpr + 4 * u32::from(r & 3) + u32::from(r >> 2)
    }

    /// Returns `x0` if it is not 0.
    fn exit_if_status(&mut self) {
        let at = self.a.cbnz_x(0);
        self.exits.push(at);
    }

    /// Returns `x0` if it is a [`STOPPED`] status.
    fn exit_if_stopped(&mut self) {
        let at = self.a.tbnz_bit32(0);
        self.exits.push(at);
    }

    /// The block is over, `eip` is stored and `w9` holds it: on to the
    /// dispatcher.
    fn finish(&mut self) {
        let here = self.base + 4 * self.a.pos();
        self.a.b_far(here, self.dispatch);
    }

    fn call(&mut self, helper: u32) {
        self.a.ldr_x(16, 19, self.o.helpers + 8 * helper);
        self.a.blr(16);
    }

    /// `xD` = the address of instruction `i`'s [`Uop`].
    fn uop_ptr(&mut self, i: u32, d: u32) {
        self.a.mov_w_imm(9, i * size_of::<Uop>() as u32);
        self.a.add_x(d, 20, 9);
    }

    fn set_eip(&mut self, eip: u32) {
        self.a.mov_w_imm(9, eip);
        self.a.str_w(9, 19, self.o.eip);
    }

    /// `wD` = the address of a memory operand, as `fast`'s `addr` computes
    /// it.
    fn addr(&mut self, e: &Ea, d: u32) {
        if e.base == NONE {
            self.a.mov_w_imm(d, e.disp);
        } else {
            self.a.ldr_w(d, 19, self.gpr(e.base));
            if e.disp != 0 {
                if e.disp < 4096 {
                    self.a.add_w_imm(d, d, e.disp);
                } else if e.disp.wrapping_neg() < 4096 {
                    self.a.sub_w_imm(d, d, e.disp.wrapping_neg());
                } else {
                    self.a.mov_w_imm(9, e.disp);
                    self.a.add_w(d, d, 9);
                }
            }
        }
        if e.index != NONE {
            self.a.ldr_w(9, 19, self.gpr(e.index));
            self.a.add_w_lsl(d, d, 9, u32::from(e.shift));
        }
        if e.fs {
            self.a.ldr_w(9, 19, self.o.fs);
            self.a.add_w(d, d, 9);
        }
    }

    /// The start of a `bytes`-byte access to the address in `w1` that the
    /// code makes itself: `x9` = the offset in the page, `w11` = the
    /// page-table entry, `x12` = the physical page, in RAM. Returns the
    /// branches taken when the access is not such a one (it crosses a
    /// page, or the page is not RAM, or the entry lacks `need`), which go
    /// to the helper.
    fn translate(&mut self, need: u32, bytes: u32) -> Vec<usize> {
        let mut slow = Vec::with_capacity(8);
        self.a.low12(9, 1);
        self.a.cmp_w_imm(9, 0x1000 - bytes);
        slow.push(self.a.b_hi());
        self.a.lsr12(10, 1);
        self.a.ldr_w_idx4(11, PT, 10);
        slow.push(self.a.tbz_w(11, need));
        slow.push(self.a.tbnz_w(11, PTE_MMIO));
        self.a.lsr12(12, 11);
        self.a.mov_w_imm(13, self.pages);
        self.a.cmp_w(12, 13);
        slow.push(self.a.b_hs());
        self.a.ldrb_idx(13, RESTRICTION, 12);
        slow
    }

    /// `w0` = the `bits` (8, 16 or 32) at the address in `w1`, zero
    /// extended; returns from the block if the load stopped. RAM that write
    /// tracking has not closed is read in place, as `Mem::read` would;
    /// anything else is the helper's.
    fn load(&mut self, i: u32, bits: u32) {
        let mut slow = self.translate(PTE_READ, bits / 8);
        // Closed (2) stops a load; read-only (1) does not.
        slow.push(self.a.tbnz_w(13, 1));
        self.a.add_x_lsl12(12, 9, 12);
        match bits {
            8 => self.a.ldrb_idx(0, RAM, 12),
            16 => self.a.ldrh_idx(0, RAM, 12),
            _ => self.a.ldr_w_idx(0, RAM, 12),
        }
        let done = self.a.b();
        let here = self.a.pos();
        for at in slow {
            self.a.patch(at, here);
        }
        self.a.mov_x(0, 19);
        self.a.mov_w_imm(2, i);
        self.a.mov_w_imm(3, bits);
        self.call(H_LOAD);
        self.exit_if_stopped();
        let here = self.a.pos();
        self.a.patch(done, here);
    }

    /// Stores the low `bits` (8, 16 or 32) of `w2` at the address in `w1`;
    /// `x0` is then 0 or the helper's status, which the caller checks. RAM
    /// with no restriction and no decoded code on its page is written in
    /// place, as `Mem::write_n` would; anything else is the helper's.
    fn store(&mut self, i: u32, bits: u32) {
        let mut slow = self.translate(PTE_WRITE, bits / 8);
        slow.push(self.a.cbnz_w(13));
        self.a.ldrb_idx(13, CODE, 12);
        slow.push(self.a.cbnz_w(13));
        self.a.add_x_lsl12(12, 9, 12);
        match bits {
            8 => self.a.strb_idx(2, RAM, 12),
            16 => self.a.strh_idx(2, RAM, 12),
            _ => self.a.str_w_idx(2, RAM, 12),
        }
        self.a.zero_x0();
        let done = self.a.b();
        let here = self.a.pos();
        for at in slow {
            self.a.patch(at, here);
        }
        self.a.mov_x(0, 19);
        self.uop_ptr(i, 3);
        self.a.mov_w_imm(4, i);
        self.a.mov_w_imm(5, bits);
        self.call(H_STORE);
        let here = self.a.pos();
        self.a.patch(done, here);
    }

    /// Runs instruction `i` by its handler.
    fn handler(&mut self, i: u32) {
        self.a.mov_x(0, 19);
        self.uop_ptr(i, 1);
        self.a.mov_w_imm(2, i);
        self.call(H_UOP);
        self.exit_if_status();
    }

    /// `w12 = w10 OP w11`; returns how the flags are recorded.
    fn alu(&mut self, op: u8) -> Op {
        match op {
            ADD => {
                self.a.add_w(12, 10, 11);
                Op::Add
            }
            SUB | CMP => {
                self.a.sub_w(12, 10, 11);
                Op::Sub
            }
            AND | TEST => {
                self.a.and_w(12, 10, 11);
                Op::Logic
            }
            OR => {
                self.a.orr_w(12, 10, 11);
                Op::Logic
            }
            _ => {
                self.a.eor_w(12, 10, 11);
                Op::Logic
            }
        }
    }

    /// Records a `size`-bit operation (`a` in `w10`, `b` in `w11`, the
    /// result in `w12`) as `Flags::lazy` does for every kind but INC and
    /// DEC ([`Gen::incdec`]).
    fn lazy(&mut self, kind: Op, size: u32) {
        // Stored flags give up their arithmetic bits.
        self.a.ldrb(13, 19, self.o.op);
        let lazy_already = self.a.cbnz_w(13);
        self.a.ldr_w(14, 19, self.o.bits);
        self.a.mov_w_imm(15, !ARITH);
        self.a.and_w(14, 14, 15);
        self.a.str_w(14, 19, self.o.bits);
        let here = self.a.pos();
        self.a.patch(lazy_already, here);
        self.a.mov_w_imm(13, kind as u32);
        self.a.strb(13, 19, self.o.op);
        self.a.mov_w_imm(13, size);
        self.a.str_w(13, 19, self.o.size);
        self.a.str_w(10, 19, self.o.a);
        self.a.str_w(11, 19, self.o.b);
        self.a.str_w(12, 19, self.o.res);
    }

    /// For a conditional jump at `i` on condition `cc`, right after an
    /// instruction whose operands are still in registers: sets the host
    /// flags from them and returns the host condition equal to `cc`.
    ///
    /// After `SUB`/`CMP`, a host `cmp a, b` has the guest's ZF, SF and OF in
    /// Z, N and V, and the guest's CF inverted in C, so each guest condition
    /// but parity has a host one. After a logic operation the guest's CF
    /// and OF are 0 and ZF and SF the result's: `cmp result, #0` (C = 1,
    /// V = 0) gives the same table the same meaning. `ADD`'s carry is not
    /// inverted, so its jumps use the helper.
    fn host_cond(&mut self, i: u32, cc: u8) -> Option<u32> {
        let (at, kind) = self.operands?;
        if at + 1 != i {
            return None;
        }
        // Guest conditions O NO B AE E NE BE A S NS P NP L GE LE G as host
        // VS VC LO HS EQ NE LS HI MI PL - - LT GE LE GT.
        const HOST: [Option<u32>; 16] = [
            Some(6),
            Some(7),
            Some(3),
            Some(2),
            Some(0),
            Some(1),
            Some(9),
            Some(8),
            Some(4),
            Some(5),
            None,
            None,
            Some(11),
            Some(10),
            Some(13),
            Some(12),
        ];
        let cond = HOST[usize::from(cc & 15)]?;
        match kind {
            Op::Sub => self.a.cmp_w(10, 11),
            Op::Logic => self.a.cmp_w_imm(12, 0),
            _ => return None,
        }
        Some(cond)
    }

    /// `INC`/`DEC` of a register: the carry is kept (the helper reads it
    /// from the flags as they are, before anything changes them), the rest
    /// recorded as `Flags::lazy` records them.
    fn incdec(&mut self, r: u8, inc: bool) {
        self.a.mov_x(0, 19);
        self.call(H_CF);
        self.a.ldr_w(14, 19, self.o.bits);
        self.a.mov_w_imm(15, !ARITH);
        self.a.and_w(14, 14, 15);
        self.a.orr_w(14, 14, 0);
        self.a.str_w(14, 19, self.o.bits);
        self.a.ldr_w(10, 19, self.gpr(r));
        if inc {
            self.a.add_w_imm(12, 10, 1);
        } else {
            self.a.sub_w_imm(12, 10, 1);
        }
        self.a.str_w(12, 19, self.gpr(r));
        self.a.mov_w_imm(13, if inc { Op::Inc } else { Op::Dec } as u32);
        self.a.strb(13, 19, self.o.op);
        self.a.mov_w_imm(13, 32);
        self.a.str_w(13, 19, self.o.size);
        self.a.mov_w_imm(11, 1);
        self.a.str_w(10, 19, self.o.a);
        self.a.str_w(11, 19, self.o.b);
        self.a.str_w(12, 19, self.o.res);
    }

    /// `w0` = the zero-extended byte or word operand `rm` of instruction
    /// `i`: a register (an 8-bit one if `bits` is 8) or memory.
    fn rm_small(&mut self, i: u32, u: &Uop, bits: u32) {
        if u.rm == NONE {
            self.addr(&u.ea, 1);
            self.load(i, bits);
        } else if bits == 8 {
            self.a.ldrb(0, 19, self.gpr8(u.rm));
        } else {
            self.a.ldrh(0, 19, self.gpr(u.rm));
        }
    }

    /// Offsets of MMX register `m`'s fields: its double, its exact
    /// significand, sign-exponent word and exact flag.
    fn mm_f(&self, m: u8) -> u32 {
        self.o.st + FPR * u32::from(m) + offset_of!(crate::f80::Fpr, f) as u32
    }
    fn mm_mant(&self, m: u8) -> u32 {
        self.o.st + FPR * u32::from(m) + offset_of!(crate::f80::Fpr, mant) as u32
    }
    fn mm_se(&self, m: u8) -> u32 {
        self.o.st + FPR * u32::from(m) + offset_of!(crate::f80::Fpr, se) as u32
    }
    fn mm_exact(&self, m: u8) -> u32 {
        self.o.st + FPR * u32::from(m) + offset_of!(crate::f80::Fpr, has_exact) as u32
    }

    /// What every MMX instruction does to the x87 state: TOP = 0, every
    /// register valid (`simd::mmx_enter`).
    fn mmx_enter(&mut self) {
        self.a.str_w(31, 19, self.o.top);
        self.a.mov_w_imm(11, 0xFF);
        self.a.strb(11, 19, self.o.valid);
    }

    /// Writes `x14` to MMX register `m` as `simd::set_mm` does: the exact
    /// significand, the exponent field all ones (sign set), and the double
    /// `F80::to_f64` gives for that: -infinity when the fraction is 0, else
    /// a quiet NaN with the fraction's top 51 bits.
    fn mm_write(&mut self, m: u8) {
        self.a.word(0xD34B_F9CA); // ubfx x10, x14, #11, #52
        self.a.word(0xB24D_314A); // orr x10, x10, #0xfff8000000000000
        self.a.word(0xD2FF_FE0B); // movz x11, #0xfff0, lsl #48
        self.a.word(0xF240_F9DF); // tst x14, #0x7fffffffffffffff
        self.a.word(0x9A8B_114A); // csel x10, x10, x11, ne
        self.a.str_x(10, 19, self.mm_f(m));
        self.a.str_x(14, 19, self.mm_mant(m));
        self.a.mov_w_imm(11, 0xFFFF);
        self.a.strh(11, 19, self.mm_se(m));
        self.a.mov_w_imm(11, 1);
        self.a.strb(11, 19, self.mm_exact(m));
    }

    /// `d1` = the 64 bits at the address in `w1`; returns from the block if
    /// the load stopped. As [`Gen::load`], with [`h_load64`] for the rest.
    fn load64(&mut self, i: u32) {
        let mut slow = self.translate(PTE_READ, 8);
        slow.push(self.a.tbnz_w(13, 1));
        self.a.add_x_lsl12(12, 9, 12);
        self.a.ldr_d_idx(1, RAM, 12);
        let done = self.a.b();
        let here = self.a.pos();
        for at in slow {
            self.a.patch(at, here);
        }
        self.a.mov_x(0, 19);
        self.a.mov_w_imm(2, i);
        self.call(H_LOAD64);
        self.exit_if_stopped();
        self.a.ldr_d(1, 19, self.o.tmp);
        let here = self.a.pos();
        self.a.patch(done, here);
    }

    /// Stores `x2` at the address in `w1`; `x0` is then 0 or the helper's
    /// status. As [`Gen::store`], with [`h_store64`] for the rest.
    fn store64(&mut self, i: u32) {
        let mut slow = self.translate(PTE_WRITE, 8);
        slow.push(self.a.cbnz_w(13));
        self.a.ldrb_idx(13, CODE, 12);
        slow.push(self.a.cbnz_w(13));
        self.a.add_x_lsl12(12, 9, 12);
        self.a.str_x_idx(2, RAM, 12);
        self.a.zero_x0();
        let done = self.a.b();
        let here = self.a.pos();
        for at in slow {
            self.a.patch(at, here);
        }
        self.a.mov_x(0, 19);
        self.uop_ptr(i, 3);
        self.a.mov_w_imm(4, i);
        self.call(H_STORE64);
        let here = self.a.pos();
        self.a.patch(done, here);
    }

    /// Emits MMX instruction `i` with NEON and in-place register access, if
    /// it is a form this covers; false (nothing emitted) if not. An MMX
    /// register it reads that holds an x87 value rather than exact bits
    /// sends the instruction to its handler, which converts it.
    fn mmx(&mut self, i: u32, u: &Uop) -> bool {
        let ins = &u.ins;
        let code = ins.code();
        let mem = ins.op_count() > 1 && ins.op_kind(1) == OpKind::Memory
            || ins.op_kind(0) == OpKind::Memory;
        let ea = if mem {
            match ea_of(ins) {
                Some(ea) => Some(ea),
                None => return false,
            }
        } else {
            None
        };
        // What the form reads and writes: (registers read, emitter).
        enum Form {
            Binary(&'static [u32], u8, Option<u8>),
            Shift(u8, u32, u8, u32),
            MovdIn(u8),
            MovdOut(u8),
            MovqIn(u8, Option<u8>),
            MovqOut(u8),
            Emms,
        }
        let form = if let Some(words) = mmx_words(code) {
            let Some(d) = mm_reg(ins.op_register(0)) else { return false };
            let s = if mem {
                None
            } else {
                match mm_reg(ins.op_register(1)) {
                    Some(s) => Some(s),
                    None => return false,
                }
            };
            Form::Binary(words, d, s)
        } else if let Some((w, kind)) = mmx_shift(code) {
            let Some(d) = mm_reg(ins.op_register(0)) else { return false };
            Form::Shift(d, w, kind, u32::from(ins.immediate8()))
        } else {
            match code {
                C::Movd_mm_rm32 => match mm_reg(ins.op_register(0)) {
                    Some(d) => Form::MovdIn(d),
                    None => return false,
                },
                C::Movd_rm32_mm => match mm_reg(ins.op_register(1)) {
                    Some(s) => Form::MovdOut(s),
                    None => return false,
                },
                C::Movq_mm_mmm64 => {
                    let Some(d) = mm_reg(ins.op_register(0)) else { return false };
                    if mem {
                        Form::MovqIn(d, None)
                    } else {
                        match mm_reg(ins.op_register(1)) {
                            Some(s) => Form::MovqIn(d, Some(s)),
                            None => return false,
                        }
                    }
                }
                C::Movq_mmm64_mm | C::Movntq_m64_mm => {
                    let Some(s) = mm_reg(ins.op_register(1)) else { return false };
                    if mem {
                        Form::MovqOut(s)
                    } else {
                        match mm_reg(ins.op_register(0)) {
                            Some(d) => Form::MovqIn(d, Some(s)),
                            None => return false,
                        }
                    }
                }
                C::Emms => Form::Emms,
                _ => return false,
            }
        };
        let reads: Vec<u8> = match &form {
            Form::Binary(_, d, s) => std::iter::once(*d).chain(*s).collect(),
            Form::Shift(d, ..) => vec![*d],
            Form::MovdOut(s) | Form::MovqOut(s) | Form::MovqIn(_, Some(s)) => vec![*s],
            Form::MovdIn(_) | Form::MovqIn(_, None) | Form::Emms => Vec::new(),
        };
        let mut inexact = Vec::new();
        for m in reads {
            self.a.ldrb(9, 19, self.mm_exact(m));
            inexact.push(self.a.cbz_w(9));
        }
        match form {
            Form::Binary(words, d, s) => {
                match (s, ea) {
                    (Some(s), _) => self.a.ldr_d(1, 19, self.mm_mant(s)),
                    (None, Some(ea)) => {
                        self.addr(&ea, 1);
                        self.load64(i);
                    }
                    (None, None) => unreachable!("a binary form reads a register or memory"),
                }
                self.a.ldr_d(0, 19, self.mm_mant(d));
                for &w in words {
                    self.a.word(w);
                }
                self.a.fmov_x_d(14, 0);
                self.mmx_enter();
                self.mm_write(d);
            }
            Form::Shift(d, w, kind, n) => {
                self.a.ldr_d(0, 19, self.mm_mant(d));
                match kind {
                    0 if n >= w => self.a.word(0x2F00_E400), // movi d0, #0
                    0 if n > 0 => self.a.shl_v0(w, n),
                    1 if n >= w => self.a.word(0x2F00_E400),
                    1 if n > 0 => self.a.shr_v0(w, n, false),
                    2 if n > 0 => self.a.shr_v0(w, n.min(w - 1), true),
                    _ => {}
                }
                self.a.fmov_x_d(14, 0);
                self.mmx_enter();
                self.mm_write(d);
            }
            Form::MovdIn(d) => {
                match ea {
                    Some(ea) => {
                        self.addr(&ea, 1);
                        self.load(i, 32);
                        self.a.mov_w(14, 0);
                    }
                    None => {
                        let Some(g) = gpr32(ins.op_register(1)) else { unreachable!() };
                        self.a.ldr_w(14, 19, self.gpr(g));
                    }
                }
                self.mmx_enter();
                self.mm_write(d);
            }
            Form::MovdOut(s) => {
                self.a.ldr_w(2, 19, self.mm_mant(s));
                self.mmx_enter();
                match ea {
                    Some(ea) => {
                        self.addr(&ea, 1);
                        self.store(i, 32);
                        self.exit_if_status();
                    }
                    None => {
                        let Some(g) = gpr32(ins.op_register(0)) else { unreachable!() };
                        self.a.str_w(2, 19, self.gpr(g));
                    }
                }
            }
            Form::MovqIn(d, s) => {
                match (s, ea) {
                    (Some(s), _) => self.a.ldr_x(14, 19, self.mm_mant(s)),
                    (None, Some(ea)) => {
                        self.addr(&ea, 1);
                        self.load64(i);
                        self.a.fmov_x_d(14, 1);
                    }
                    (None, None) => unreachable!("a load reads a register or memory"),
                }
                self.mmx_enter();
                self.mm_write(d);
            }
            Form::MovqOut(s) => {
                let Some(ea) = ea else { unreachable!() };
                self.a.ldr_x(2, 19, self.mm_mant(s));
                self.addr(&ea, 1);
                self.store64(i);
                // The interpreter enters MMX state once the store is done,
                // also when it changed code.
                self.exit_if_stopped();
                self.mmx_enter();
                self.exit_if_status();
            }
            Form::Emms => {
                self.a.strb(31, 19, self.o.valid);
            }
        }
        if !inexact.is_empty() {
            let done = self.a.b();
            let here = self.a.pos();
            for at in inexact {
                self.a.patch(at, here);
            }
            self.handler(i);
            let here = self.a.pos();
            self.a.patch(done, here);
        }
        true
    }

    /// `wP` = the physical index of ST(i) and `xB` = the CPU's address plus
    /// 24 times it, so that `[xB, #st + field]` is that register's field.
    fn fp_phys(&mut self, i: u32, p: u32, base: u32) {
        self.a.ldr_w(p, 19, self.o.top);
        if i != 0 {
            self.a.add_w_imm(p, p, i);
        }
        self.a.word(0x1200_0800 | p << 5 | p); // and wP, wP, #7
        self.a.word(0x8B00_0400 | p << 16 | p << 5 | base); // add xB, xP, xP, lsl #1
        self.a.word(0x8B00_0C00 | base << 16 | 19 << 5 | base); // add xB, x19, xB, lsl #3
    }

    /// `w12` bit 0 = whether physical register `wP` is in use.
    fn fp_bit(&mut self, p: u32) {
        self.a.ldrb(12, 19, self.o.valid);
        self.a.word(0x1AC0_2400 | p << 16 | 12 << 5 | 12); // lsr w12, w12, wP
    }

    /// A branch taken if physical register `wP` is empty.
    fn fp_empty(&mut self, p: u32) -> usize {
        self.fp_bit(p);
        self.a.tbz_w(12, 0)
    }

    /// A branch taken if physical register `wP` is in use.
    fn fp_full(&mut self, p: u32) -> usize {
        self.fp_bit(p);
        self.a.tbnz_w(12, 0)
    }

    /// Marks physical register `wP` in use or empty.
    fn fp_mark(&mut self, p: u32, used: bool) {
        self.a.ldrb(12, 19, self.o.valid);
        self.a.mov_w_imm(13, 1);
        self.a.word(0x1AC0_2000 | p << 16 | 13 << 5 | 13); // lsl w13, w13, wP
        if used {
            self.a.orr_w(12, 12, 13);
        } else {
            self.a.word(0x0A2D_018C); // bic w12, w12, w13
        }
        self.a.strb(12, 19, self.o.valid);
    }

    /// The register at `xB` = `d0`, a result (no exact bits), as
    /// `Fpr::from_f64` makes it.
    fn fp_write(&mut self, base: u32) {
        self.a.str_d(0, base, self.o.st);
        self.a.str_x(31, base, self.o.st + 8);
        self.a.str_w(31, base, self.o.st + 16);
    }

    /// Copies the register at `xFrom` to `xTo`, exact bits and all.
    fn fp_copy(&mut self, from: u32, to: u32) {
        self.a.ldr_x(12, from, self.o.st);
        self.a.ldr_x(13, from, self.o.st + 8);
        self.a.ldr_w(15, from, self.o.st + 16);
        self.a.str_x(12, to, self.o.st);
        self.a.str_x(13, to, self.o.st + 8);
        self.a.str_w(15, to, self.o.st + 16);
    }

    /// Pops the stack: ST0 empty, TOP up by one.
    fn fp_pop(&mut self) {
        self.a.ldr_w(9, 19, self.o.top);
        self.fp_mark(9, false);
        self.a.add_w_imm(9, 9, 1);
        self.a.word(0x1200_0929); // and w9, w9, #7
        self.a.str_w(9, 19, self.o.top);
    }

    /// `d0` rounded to single when the control word's precision is single
    /// (`round_pc`).
    fn fp_round(&mut self) {
        self.a.ldrh(12, 19, self.o.fcw);
        self.a.word(0x7218_059F); // tst w12, #0x300
        let wide = self.a.b_cond(1); // b.ne
        self.a.word(0x1E62_4000); // fcvt s0, d0
        self.a.word(0x1E22_C000); // fcvt d0, s0
        let here = self.a.pos();
        self.a.patch(wide, here);
    }

    /// Emits x87 instruction `i`, or one of the forms with no effect, if it
    /// is a form this covers; false (nothing emitted) if not. Each form
    /// does what its handler in `x87` does, in the same order.
    #[expect(clippy::too_many_lines, reason = "one table of forms")]
    fn x87(&mut self, i: u32, u: &Uop) -> bool {
        use crate::x87::{FDIV, FDIVR, arith_form, st_index};
        let ins = &u.ins;
        let code = ins.code();
        let mem_ea = || {
            (ins.op_count() > 0 && ins.op_kind(0) == OpKind::Memory
                || ins.op_count() > 1 && ins.op_kind(1) == OpKind::Memory)
                .then(|| ea_of(ins))
                .flatten()
        };
        let has_mem = (0..ins.op_count()).any(|k| ins.op_kind(k) == OpKind::Memory);
        if has_mem && mem_ea().is_none() {
            return false;
        }
        // Branches to the handler, for what this leaves to the interpreter;
        // they are taken before anything changes.
        let mut slow = Vec::new();
        match code {
            _ if matches!(
                ins.mnemonic(),
                Mnemonic::Nop
                    | Mnemonic::Pause
                    | Mnemonic::Prefetchnta
                    | Mnemonic::Prefetcht0
                    | Mnemonic::Prefetcht1
                    | Mnemonic::Prefetcht2
                    | Mnemonic::Prefetchw
                    | Mnemonic::Prefetch
                    | Mnemonic::Sfence
                    | Mnemonic::Lfence
                    | Mnemonic::Mfence
                    | Mnemonic::Wait
            ) =>
            {
                return true;
            }
            _ if arith_form(code).is_some() => {
                let Some((op, form)) = arith_form(code) else { unreachable!() };
                let (d, src) = match form {
                    0 | 1 => (0, None),
                    2 => (0, Some(st_index(ins.op_register(1)))),
                    _ => (st_index(ins.op_register(0)), Some(0)),
                };
                self.fp_phys(d, 9, 10);
                slow.push(self.fp_empty(9));
                if let Some(s) = src {
                    self.fp_phys(s, 14, 11);
                    slow.push(self.fp_empty(14));
                }
                match (form, mem_ea()) {
                    (0, Some(ea)) => {
                        self.addr(&ea, 1);
                        self.load(i, 32);
                        self.a.word(0x1E27_0000); // fmov s0, w0
                        self.a.word(0x1E22_C001); // fcvt d1, s0
                    }
                    (1, Some(ea)) => {
                        self.addr(&ea, 1);
                        self.load64(i);
                    }
                    _ => {
                        let Some(s) = src else { return false };
                        self.fp_phys(s, 14, 11);
                        self.a.ldr_d(1, 11, self.o.st);
                    }
                }
                self.fp_phys(d, 9, 10);
                self.a.ldr_d(0, 10, self.o.st);
                let mut rare = Vec::new();
                if op == FDIV {
                    self.a.word(0x1E60_2028); // fcmp d1, #0.0
                    rare.push(self.a.b_cond(0));
                } else if op == FDIVR {
                    self.a.word(0x1E60_2008); // fcmp d0, #0.0
                    rare.push(self.a.b_cond(0));
                }
                self.a.word(match op {
                    0 => 0x1E61_2800, // fadd d0, d0, d1
                    1 => 0x1E61_0800, // fmul d0, d0, d1
                    2 => 0x1E61_3800, // fsub d0, d0, d1
                    3 => 0x1E60_3820, // fsub d0, d1, d0
                    4 => 0x1E61_1800, // fdiv d0, d0, d1
                    _ => 0x1E60_1820, // fdiv d0, d1, d0
                });
                self.a.word(0x1E60_2000); // fcmp d0, d0
                rare.push(self.a.b_cond(6)); // b.vs: a NaN
                self.fp_round();
                self.fp_write(10);
                let done = self.a.b();
                let here = self.a.pos();
                for at in rare {
                    self.a.patch(at, here);
                }
                self.a.mov_x(0, 19);
                self.a.mov_w_imm(1, op);
                self.a.mov_w_imm(2, d);
                self.a.word(0x9E66_0023); // fmov x3, d1
                self.call(H_FAPPLY);
                let here = self.a.pos();
                self.a.patch(done, here);
                if form == 4 {
                    self.fp_pop();
                }
            }
            C::Fld_m32fp | C::Fld_m64fp | C::Fild_m32int => {
                self.fp_phys(7, 9, 10);
                slow.push(self.fp_full(9));
                let Some(ea) = mem_ea() else { return false };
                self.addr(&ea, 1);
                if code == C::Fld_m64fp {
                    self.load64(i);
                    self.a.word(0x1E60_4020); // fmov d0, d1
                } else {
                    self.load(i, 32);
                    if code == C::Fild_m32int {
                        self.a.word(0x1E62_0000); // scvtf d0, w0
                    } else {
                        self.a.word(0x1E27_0000); // fmov s0, w0
                        self.a.word(0x1E22_C000); // fcvt d0, s0
                    }
                }
                self.fp_phys(7, 9, 10);
                self.fp_write(10);
                self.fp_mark(9, true);
                self.a.str_w(9, 19, self.o.top);
            }
            C::Fld_sti => {
                self.fp_phys(st_index(ins.op_register(0)), 14, 11);
                slow.push(self.fp_empty(14));
                self.fp_phys(7, 9, 10);
                slow.push(self.fp_full(9));
                self.fp_copy(11, 10);
                self.fp_mark(9, true);
                self.a.str_w(9, 19, self.o.top);
            }
            C::Fst_m32fp | C::Fstp_m32fp | C::Fst_m64fp | C::Fstp_m64fp => {
                self.fp_phys(0, 9, 10);
                slow.push(self.fp_empty(9));
                self.a.ldr_d(0, 10, self.o.st);
                let Some(ea) = mem_ea() else { return false };
                let single = matches!(code, C::Fst_m32fp | C::Fstp_m32fp);
                if single {
                    self.a.word(0x1E62_4000); // fcvt s0, d0
                    self.a.word(0x1E26_0002); // fmov w2, s0
                } else {
                    self.a.word(0x9E66_0002); // fmov x2, d0
                }
                self.addr(&ea, 1);
                if single {
                    self.store(i, 32);
                } else {
                    self.store64(i);
                }
                self.exit_if_stopped();
                if matches!(code, C::Fstp_m32fp | C::Fstp_m64fp) {
                    self.fp_pop();
                }
                self.exit_if_status();
            }
            C::Fst_sti | C::Fstp_sti => {
                self.fp_phys(0, 9, 10);
                slow.push(self.fp_empty(9));
                self.fp_phys(st_index(ins.op_register(0)), 14, 11);
                self.fp_copy(10, 11);
                self.fp_mark(14, true);
                if code == C::Fstp_sti {
                    self.fp_pop();
                }
            }
            C::Fcom_m32fp
            | C::Fcomp_m32fp
            | C::Fcom_m64fp
            | C::Fcomp_m64fp
            | C::Fcom_st0_sti
            | C::Fcomp_st0_sti
            | C::Fucom_st0_sti
            | C::Fucomp_st0_sti => {
                self.fp_phys(0, 9, 10);
                slow.push(self.fp_empty(9));
                let reg = !has_mem;
                if reg {
                    self.fp_phys(st_index(ins.op_register(1)), 14, 11);
                    slow.push(self.fp_empty(14));
                    self.a.ldr_d(1, 11, self.o.st);
                } else {
                    let Some(ea) = mem_ea() else { return false };
                    self.addr(&ea, 1);
                    if matches!(code, C::Fcom_m32fp | C::Fcomp_m32fp) {
                        self.load(i, 32);
                        self.a.word(0x1E27_0000); // fmov s0, w0
                        self.a.word(0x1E22_C001); // fcvt d1, s0
                    } else {
                        self.load64(i);
                    }
                    self.fp_phys(0, 9, 10);
                }
                self.a.ldr_d(0, 10, self.o.st);
                self.a.word(0x1E61_2000); // fcmp d0, d1
                // C3 C2 C0 as `fcompare` sets them: greater 000, less 001,
                // equal 100, unordered 111.
                let mut done = Vec::new();
                self.a.mov_w_imm(11, 0);
                done.push(self.a.b_cond(12)); // b.gt
                self.a.mov_w_imm(11, 0x100);
                done.push(self.a.b_cond(4)); // b.mi
                self.a.mov_w_imm(11, 0x4000);
                done.push(self.a.b_cond(0)); // b.eq
                self.a.mov_w_imm(11, 0x4500);
                let here = self.a.pos();
                for at in done {
                    self.a.patch(at, here);
                }
                self.a.ldrh(12, 19, self.o.fsw);
                self.a.mov_w_imm(13, 0x4700);
                self.a.word(0x0A2D_018C); // bic w12, w12, w13
                self.a.word(0x2A0B_018C); // orr w12, w12, w11
                self.a.strh(12, 19, self.o.fsw);
                if matches!(
                    code,
                    C::Fcomp_m32fp | C::Fcomp_m64fp | C::Fcomp_st0_sti | C::Fucomp_st0_sti
                ) {
                    self.fp_pop();
                }
            }
            C::Fnstsw_AX => {
                self.a.ldrh(11, 19, self.o.fsw);
                self.a.mov_w_imm(12, 0x3800);
                self.a.word(0x0A2C_016B); // bic w11, w11, w12
                self.a.ldr_w(12, 19, self.o.top);
                self.a.word(0x2A0C_2D6B); // orr w11, w11, w12, lsl #11
                self.a.strh(11, 19, self.gpr(0));
            }
            C::Fxch_st0_sti => {
                self.fp_phys(0, 9, 10);
                slow.push(self.fp_empty(9));
                self.fp_phys(st_index(ins.op_register(1)), 14, 11);
                slow.push(self.fp_empty(14));
                self.a.ldr_x(2, 11, self.o.st);
                self.a.ldr_x(3, 11, self.o.st + 8);
                self.a.ldr_w(4, 11, self.o.st + 16);
                self.fp_copy(10, 11);
                self.a.str_x(2, 10, self.o.st);
                self.a.str_x(3, 10, self.o.st + 8);
                self.a.str_w(4, 10, self.o.st + 16);
                self.a.ldrh(12, 19, self.o.fsw);
                self.a.mov_w_imm(13, 0x200);
                self.a.word(0x0A2D_018C); // bic w12, w12, w13
                self.a.strh(12, 19, self.o.fsw);
            }
            C::Fchs | C::Fabs => {
                self.fp_phys(0, 9, 10);
                slow.push(self.fp_empty(9));
                self.a.ldr_d(0, 10, self.o.st);
                self.a.word(if code == C::Fchs { 0x1E61_4000 } else { 0x1E60_C000 }); // fneg/fabs d0, d0
                self.a.str_d(0, 10, self.o.st);
                // Exact bits keep their significand and change their sign.
                self.a.ldrb(12, 10, self.o.st + 18);
                let inexact = self.a.cbz_w(12);
                self.a.ldrh(12, 10, self.o.st + 16);
                self.a.word(if code == C::Fchs { 0x5211_018C } else { 0x1200_398C }); // eor #0x8000 / and #0x7fff
                self.a.strh(12, 10, self.o.st + 16);
                let here = self.a.pos();
                self.a.patch(inexact, here);
            }
            C::Fldcw_m2byte => {
                let Some(ea) = mem_ea() else { return false };
                self.addr(&ea, 1);
                self.load(i, 16);
                self.a.strh(0, 19, self.o.fcw);
            }
            C::Fnstcw_m2byte => {
                let Some(ea) = mem_ea() else { return false };
                self.a.ldrh(2, 19, self.o.fcw);
                self.addr(&ea, 1);
                self.store(i, 16);
                self.exit_if_status();
            }
            C::Mov_r8_imm8 => {
                let n = (ins.op_register(0) as u32).wrapping_sub(Register::AL as u32);
                if n > 7 {
                    return false;
                }
                self.a.mov_w_imm(9, u32::from(ins.immediate8()));
                self.a.strb(9, 19, self.gpr8(n as u8));
            }
            _ if matches!(ins.mnemonic(), m if is_setcc(m)) && !has_mem => {
                let n = (ins.op_register(0) as u32).wrapping_sub(Register::AL as u32);
                if n > 7 {
                    return false;
                }
                self.a.mov_x(0, 19);
                self.a.mov_w_imm(1, ins.condition_code() as u32 - 1);
                self.call(H_COND);
                self.a.strb(0, 19, self.gpr8(n as u8));
            }
            _ => return false,
        }
        if !slow.is_empty() {
            let done = self.a.b();
            let here = self.a.pos();
            for at in slow {
                self.a.patch(at, here);
            }
            self.handler(i);
            let here = self.a.pos();
            self.a.patch(done, here);
        }
        true
    }

    /// ESP moves by 4: down (a push) or up.
    fn move_esp(&mut self, down: bool) {
        let esp = self.gpr(ESP);
        self.a.ldr_w(9, 19, esp);
        if down {
            self.a.sub_w_imm(9, 9, 4);
        } else {
            self.a.add_w_imm(9, 9, 4);
        }
        self.a.str_w(9, 19, esp);
    }

    /// Pushes `w2`: the store first, ESP once it has succeeded. `x0` is
    /// then 0 or [`ENDED`].
    fn push(&mut self, i: u32) {
        self.a.ldr_w(1, 19, self.gpr(ESP));
        self.a.sub_w_imm(1, 1, 4);
        self.store(i, 32);
        self.exit_if_stopped();
        self.move_esp(true);
    }

    /// Emits instruction `i`; returns whether control goes on to the next.
    fn uop(&mut self, i: u32, u: &Uop) -> bool {
        if self.x87(i, u) {
            return true;
        }
        if !u.fast {
            if !self.mmx(i, u) {
                self.handler(i);
            }
            return true;
        }
        let code = u.ins.code();
        let mem = u.rm == NONE;
        if let Some((op, form)) = alu_form(code) {
            let writes = op != CMP && op != TEST;
            if form == 0 {
                // r OP= rm
                if mem {
                    self.addr(&u.ea, 1);
                    self.load(i, 32);
                    self.a.mov_w(11, 0);
                } else {
                    self.a.ldr_w(11, 19, self.gpr(u.rm));
                }
                self.a.ldr_w(10, 19, self.gpr(u.r));
                let kind = self.alu(op);
                if writes {
                    self.a.str_w(12, 19, self.gpr(u.r));
                }
                self.lazy(kind, 32);
                self.operands = Some((i, kind));
                return true;
            }
            // rm OP= r, rm OP= imm
            if mem {
                if writes {
                    // Load, store and flags that must all take effect or
                    // none: the handler's.
                    self.handler(i);
                    return true;
                }
                self.addr(&u.ea, 1);
                self.load(i, 32);
                self.a.mov_w(10, 0);
            } else {
                self.a.ldr_w(10, 19, self.gpr(u.rm));
            }
            if form == 2 {
                self.a.mov_w_imm(11, u.imm);
            } else {
                self.a.ldr_w(11, 19, self.gpr(u.r));
            }
            let kind = self.alu(op);
            if writes {
                self.a.str_w(12, 19, self.gpr(u.rm));
            }
            self.lazy(kind, 32);
            self.operands = Some((i, kind));
            return true;
        }
        match code {
            _ if small_alu(code).is_some() => {
                let Some((op, bits, form)) = small_alu(code) else { unreachable!() };
                let writes = op != CMP && op != TEST;
                let (ld, st): (Access, Access) =
                    if bits == 8 { (Asm::ldrb, Asm::strb) } else { (Asm::ldrh, Asm::strh) };
                let reg = |g: &Self, r: u8| if bits == 8 { g.gpr8(r) } else { g.gpr(r) };
                if form == 0 {
                    // r OP= rm
                    self.rm_small(i, u, bits);
                    self.a.mov_w(11, 0);
                    let off = reg(self, u.r);
                    ld(&mut self.a, 10, 19, off);
                    let kind = self.alu(op);
                    if writes {
                        let off = reg(self, u.r);
                        st(&mut self.a, 12, 19, off);
                    }
                    self.lazy(kind, bits);
                } else if mem && writes {
                    // Load, store and flags that must all take effect or
                    // none: the handler's.
                    self.handler(i);
                } else {
                    self.rm_small(i, u, bits);
                    self.a.mov_w(10, 0);
                    if form == 1 {
                        let off = reg(self, u.r);
                        ld(&mut self.a, 11, 19, off);
                    } else {
                        self.a.mov_w_imm(11, u.imm & if bits == 8 { 0xFF } else { 0xFFFF });
                    }
                    let kind = self.alu(op);
                    if writes {
                        let off = reg(self, u.rm);
                        st(&mut self.a, 12, 19, off);
                    }
                    self.lazy(kind, bits);
                }
            }
            C::Shl_rm32_CL | C::Shr_rm32_CL | C::Sar_rm32_CL => {
                self.a.ldr_w(10, 19, self.gpr(u.rm));
                self.a.ldr_w(11, 19, self.gpr(1));
                self.a.word(0x1200_116B); // and w11, w11, #31
                let none = self.a.cbz_w(11);
                self.a.word(match code {
                    C::Shl_rm32_CL => 0x1ACB_214C, // lsl w12, w10, w11
                    C::Shr_rm32_CL => 0x1ACB_254C, // lsr w12, w10, w11
                    _ => 0x1ACB_294C,              // asr w12, w10, w11
                });
                self.a.str_w(12, 19, self.gpr(u.rm));
                let kind = match code {
                    C::Shl_rm32_CL => Op::Shl,
                    C::Shr_rm32_CL => Op::Shr,
                    _ => Op::Sar,
                };
                self.lazy(kind, 32);
                let here = self.a.pos();
                self.a.patch(none, here);
            }
            C::Neg_rm32 => {
                self.a.ldr_w(11, 19, self.gpr(u.rm));
                self.a.mov_w_imm(10, 0);
                self.a.sub_w(12, 10, 11);
                self.a.str_w(12, 19, self.gpr(u.rm));
                self.lazy(Op::Sub, 32);
                self.operands = Some((i, Op::Sub));
            }
            C::Not_rm32 => {
                self.a.ldr_w(9, 19, self.gpr(u.rm));
                self.a.word(0x2A29_03E9); // mvn w9, w9
                self.a.str_w(9, 19, self.gpr(u.rm));
            }
            C::Leaved => {
                self.a.ldr_w(1, 19, self.gpr(5));
                self.load(i, 32);
                self.a.ldr_w(9, 19, self.gpr(5));
                self.a.add_w_imm(9, 9, 4);
                self.a.str_w(9, 19, self.gpr(ESP));
                self.a.str_w(0, 19, self.gpr(5));
            }
            C::Push_rm32 => {
                if mem {
                    self.addr(&u.ea, 1);
                    self.load(i, 32);
                    self.a.mov_w(2, 0);
                } else {
                    self.a.ldr_w(2, 19, self.gpr(u.rm));
                }
                self.push(i);
                self.exit_if_status();
            }
            C::Jmp_rm32 | C::Call_rm32 => {
                // The target first: a load that faults changes nothing.
                if mem {
                    self.addr(&u.ea, 1);
                    self.load(i, 32);
                } else {
                    self.a.ldr_w(0, 19, self.gpr(u.rm));
                }
                if code == C::Jmp_rm32 {
                    self.a.str_w(0, 19, self.o.eip);
                    self.a.mov_w(9, 0);
                } else {
                    // Kept across the push, whose helper may be called.
                    self.a.str_w(0, 19, self.o.tmp);
                    self.a.mov_w_imm(2, u.next);
                    self.push(i);
                    self.a.ldr_w(9, 19, self.o.tmp);
                    self.a.str_w(9, 19, self.o.eip);
                    // A push that changed code goes back to the run loop.
                    self.exit_if_status();
                }
                self.finish();
                return false;
            }
            C::Mov_r8_rm8 | C::Mov_AL_moffs8 => {
                self.rm_small(i, u, 8);
                self.a.strb(0, 19, self.gpr8(u.r));
            }
            C::Mov_rm8_r8 | C::Mov_moffs8_AL | C::Mov_rm8_imm8 => {
                if code != C::Mov_rm8_imm8 {
                    self.a.ldrb(2, 19, self.gpr8(u.r));
                } else {
                    self.a.mov_w_imm(2, u.imm & 0xFF);
                }
                if mem {
                    self.addr(&u.ea, 1);
                    self.store(i, 8);
                    self.exit_if_status();
                } else {
                    self.a.strb(2, 19, self.gpr8(u.rm));
                }
            }
            C::Mov_r16_rm16 => {
                self.rm_small(i, u, 16);
                self.a.strh(0, 19, self.gpr(u.r));
            }
            C::Mov_rm16_r16 => {
                self.a.ldrh(2, 19, self.gpr(u.r));
                if mem {
                    self.addr(&u.ea, 1);
                    self.store(i, 16);
                    self.exit_if_status();
                } else {
                    self.a.strh(2, 19, self.gpr(u.rm));
                }
            }
            C::Movzx_r32_rm8 | C::Movsx_r32_rm8 | C::Movzx_r32_rm16 | C::Movsx_r32_rm16 => {
                let bits = if matches!(code, C::Movzx_r32_rm8 | C::Movsx_r32_rm8) { 8 } else { 16 };
                self.rm_small(i, u, bits);
                match code {
                    C::Movsx_r32_rm8 => self.a.sxtb_w(0, 0),
                    C::Movsx_r32_rm16 => self.a.sxth_w(0, 0),
                    _ => {}
                }
                self.a.str_w(0, 19, self.gpr(u.r));
            }
            C::Cmp_rm8_imm8
            | C::Cmp_rm8_imm8_82
            | C::Cmp_AL_imm8
            | C::Test_rm8_imm8
            | C::Test_rm8_imm8_F6r1
            | C::Test_AL_imm8 => {
                self.rm_small(i, u, 8);
                self.a.mov_w(10, 0);
                self.a.mov_w_imm(11, u.imm & 0xFF);
                let kind = if matches!(code, C::Cmp_rm8_imm8 | C::Cmp_rm8_imm8_82 | C::Cmp_AL_imm8)
                {
                    self.a.sub_w(12, 10, 11);
                    Op::Sub
                } else {
                    self.a.and_w(12, 10, 11);
                    Op::Logic
                };
                self.lazy(kind, 8);
            }
            C::Inc_r32 | C::Inc_rm32 | C::Dec_r32 | C::Dec_rm32 if !mem => {
                self.incdec(u.rm, matches!(code, C::Inc_r32 | C::Inc_rm32));
            }
            C::Shl_rm32_imm8
            | C::Shl_rm32_1
            | C::Shr_rm32_imm8
            | C::Shr_rm32_1
            | C::Sar_rm32_imm8
            | C::Sar_rm32_1 => {
                // `fast` takes these only for a register and a count of 1
                // to 31.
                self.a.ldr_w(10, 19, self.gpr(u.rm));
                let kind = match code {
                    C::Shl_rm32_imm8 | C::Shl_rm32_1 => {
                        self.a.lsl_w_imm(12, 10, u.imm);
                        Op::Shl
                    }
                    C::Shr_rm32_imm8 | C::Shr_rm32_1 => {
                        self.a.lsr_w_imm(12, 10, u.imm);
                        Op::Shr
                    }
                    _ => {
                        self.a.asr_w_imm(12, 10, u.imm);
                        Op::Sar
                    }
                };
                self.a.str_w(12, 19, self.gpr(u.rm));
                self.a.mov_w_imm(11, u.imm);
                self.lazy(kind, 32);
            }
            C::Imul_r32_rm32 | C::Imul_r32_rm32_imm32 | C::Imul_r32_rm32_imm8 => {
                if mem {
                    self.addr(&u.ea, 1);
                    self.load(i, 32);
                    self.a.mov_w(10, 0);
                } else {
                    self.a.ldr_w(10, 19, self.gpr(u.rm));
                }
                if code == C::Imul_r32_rm32 {
                    self.a.ldr_w(11, 19, self.gpr(u.r));
                } else {
                    self.a.mov_w_imm(11, u.imm);
                }
                self.a.smull(12, 10, 11);
                // Truncated if the product is not its low half sign-extended.
                self.a.cmp_x_sxtw(12, 12);
                self.a.cset_w(10, 1);
                self.a.str_w(12, 19, self.gpr(u.r));
                self.lazy(Op::Mul, 32);
            }
            C::Mov_r32_imm32 => {
                self.a.mov_w_imm(9, u.imm);
                self.a.str_w(9, 19, self.gpr(u.r));
            }
            C::Mov_r32_rm32 | C::Mov_EAX_moffs32 => {
                if mem {
                    self.addr(&u.ea, 1);
                    self.load(i, 32);
                    self.a.str_w(0, 19, self.gpr(u.r));
                } else {
                    self.a.ldr_w(9, 19, self.gpr(u.rm));
                    self.a.str_w(9, 19, self.gpr(u.r));
                }
            }
            C::Mov_rm32_r32 | C::Mov_moffs32_EAX => {
                if mem {
                    self.addr(&u.ea, 1);
                    self.a.ldr_w(2, 19, self.gpr(u.r));
                    self.store(i, 32);
                    self.exit_if_status();
                } else {
                    self.a.ldr_w(9, 19, self.gpr(u.r));
                    self.a.str_w(9, 19, self.gpr(u.rm));
                }
            }
            C::Mov_rm32_imm32 => {
                if mem {
                    self.addr(&u.ea, 1);
                    self.a.mov_w_imm(2, u.imm);
                    self.store(i, 32);
                    self.exit_if_status();
                } else {
                    self.a.mov_w_imm(9, u.imm);
                    self.a.str_w(9, 19, self.gpr(u.rm));
                }
            }
            C::Lea_r32_m => {
                self.addr(&u.ea, 10);
                self.a.str_w(10, 19, self.gpr(u.r));
            }
            C::Push_r32 => {
                self.a.ldr_w(2, 19, self.gpr(u.r));
                self.push(i);
                self.exit_if_status();
            }
            C::Pushd_imm8 | C::Pushd_imm32 => {
                self.a.mov_w_imm(2, u.imm);
                self.push(i);
                self.exit_if_status();
            }
            C::Pop_r32 => {
                self.a.ldr_w(1, 19, self.gpr(ESP));
                self.load(i, 32);
                self.move_esp(false);
                self.a.str_w(0, 19, self.gpr(u.r));
            }
            C::Jmp_rel8_32 | C::Jmp_rel32_32 => {
                self.set_eip(u.imm);
                self.finish();
                return false;
            }
            C::Call_rel32_32 => {
                self.a.mov_w_imm(2, u.next);
                self.push(i);
                // A push that changed code goes back to the run loop, which
                // drops the blocks it changed, rather than on to the
                // dispatcher.
                self.set_eip(u.imm);
                self.exit_if_status();
                self.finish();
                return false;
            }
            C::Retnd | C::Retnd_imm16 => {
                let esp = self.gpr(ESP);
                self.a.ldr_w(1, 19, esp);
                self.load(i, 32);
                self.a.str_w(0, 19, self.o.eip);
                self.a.ldr_w(9, 19, esp);
                self.a.mov_w_imm(10, 4 + u.imm);
                self.a.add_w(9, 9, 10);
                self.a.str_w(9, 19, esp);
                self.a.mov_w(9, 0);
                self.finish();
                return false;
            }
            _ if is_jcc(u.ins.mnemonic()) && u.ins.op0_kind() == OpKind::NearBranch32 => {
                if let Some(cond) = self.host_cond(i, u.r) {
                    // The flags of the instruction before, from its
                    // operands (constants do not disturb host flags).
                    self.a.mov_w_imm(9, u.imm);
                    let taken = self.a.b_cond(cond);
                    self.a.mov_w_imm(9, u.next);
                    let here = self.a.pos();
                    self.a.patch(taken, here);
                    self.a.str_w(9, 19, self.o.eip);
                    self.finish();
                    return false;
                }
                self.a.mov_x(0, 19);
                self.a.mov_w_imm(1, u32::from(u.r));
                self.call(H_COND);
                self.a.mov_w_imm(9, u.next);
                let not_taken = self.a.cbz_w(0);
                self.a.mov_w_imm(9, u.imm);
                let here = self.a.pos();
                self.a.patch(not_taken, here);
                self.a.str_w(9, 19, self.o.eip);
                self.finish();
                return false;
            }
            _ => self.handler(i),
        }
        true
    }
}
