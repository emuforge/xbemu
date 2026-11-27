//! AArch64 code for a block, and what that code calls.
//!
//! # The code
//!
//! A compiled block is a C function `fn(cpu, ops) -> status`. It keeps the
//! CPU in `x19` and the block's compiled instructions ([`Uop`]) in `x20`;
//! the guest's registers and flags stay in the [`InterpCpu`], read and
//! written at their field offsets. Per instruction it emits one of:
//!
//! * **Host instructions** for the forms title code is made of: 32-bit
//!   moves, `LEA`, `ADD`/`SUB`/`AND`/`OR`/`XOR`/`CMP`/`TEST` with the
//!   arithmetic flags recorded lazily exactly as [`Flags::lazy`] records
//!   them, pushes and pops, and the block's final jump, call, return or
//!   conditional jump.
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
//! # Leaving the code
//!
//! The function returns 0 when the block ran to its end, with `eip` stored.
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

use iced_x86::{Code as C, OpKind};
use xb_cpu::Bus;

use super::a64::Asm;
use super::macos::CodeBuf;
use crate::cpu::{Flow, InterpCpu, Snap, Step, Stop};
use crate::fast::{ADD, AND, CMP, Ea, NONE, OR, SUB, TEST, Uop, XOR, is_jcc};
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
        }
    }

    /// Whether every offset fits the addressing forms the generator uses.
    fn fits(&self) -> bool {
        let word = |v: u32| v.is_multiple_of(4) && v < 16384;
        [self.gpr + 28, self.eip, self.fs, self.bits, self.size, self.a, self.b, self.res]
            .into_iter()
            .all(word)
            && self.op < 4096
            && [self.helpers, self.tables]
                .into_iter()
                .all(|v| v.is_multiple_of(8) && v + 24 < 32768)
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
    helpers: [usize; 4],
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
    pub(crate) fn new(tables: [usize; 4], pages: u32) -> Self {
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
        Self {
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
            ],
            tables,
            pages,
            runs: 0,
        }
    }

    /// Compiles a block: `Continue(Some(entry))`, `Continue(None)` when
    /// this CPU generates no code, or `Break` when the code buffer is full
    /// (the caller drops every block and calls [`State::reset`]).
    pub(crate) fn compile(&mut self, ops: &[Uop]) -> ControlFlow<(), Option<usize>> {
        let Some(buf) = &mut self.buf else {
            return ControlFlow::Continue(None);
        };
        let code = generate(ops, &self.off, self.pages);
        match buf.push(&code) {
            Some(entry) => ControlFlow::Continue(Some(entry)),
            None => ControlFlow::Break(()),
        }
    }

    /// Forgets all generated code. No block may keep an entry.
    pub(crate) fn reset(&mut self) {
        if let Some(buf) = &mut self.buf {
            buf.reset();
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

/// Runs the generated code at `entry`, compiled from exactly `ops`, and
/// returns its status (module docs).
pub(crate) fn run(entry: usize, cpu: &mut InterpCpu, ops: &[Uop], bus: &mut dyn Bus) -> u64 {
    // SAFETY: only the trait object's lifetime changes. The pointer is
    // cleared below, before the borrow it came from ends.
    let bus = unsafe {
        std::mem::transmute::<NonNull<dyn Bus + '_>, NonNull<dyn Bus + 'static>>(NonNull::from(bus))
    };
    cpu.jit.bus = Some(bus);
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

/// Loads 32 bits at `va` for instruction `i`: the value, or a status with
/// bit 32 set.
///
/// # Safety
/// As [`h_uop`].
unsafe extern "C" fn h_load(cpu: *mut InterpCpu, va: u32, i: u32) -> u64 {
    let body = |cpu: &mut InterpCpu, bus: &mut dyn Bus| match cpu.load(va, 32, bus) {
        Ok(v) => u64::from(v),
        Err(stop) => stopped(cpu, i, stop),
    };
    // SAFETY: the caller's contract.
    unsafe { guarded(cpu, i, body) }
}

/// Stores `v` at `va` for instruction `i`: 0, [`STOPPED`] (nothing was
/// stored), or [`ENDED`] when the store changed code (`eip` is then stored
/// past the instruction; one that is not complete yet stores its own).
///
/// # Safety
/// As [`h_uop`].
unsafe extern "C" fn h_store(cpu: *mut InterpCpu, va: u32, v: u32, u: *const Uop, i: u32) -> u64 {
    // SAFETY: `u` points into the ops `run` borrows for the whole call.
    let next = unsafe { (*u).next };
    let body = |cpu: &mut InterpCpu, bus: &mut dyn Bus| match cpu.store(va, 32, v, bus) {
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

/// Whether condition `cc` holds.
///
/// # Safety
/// As [`h_uop`].
unsafe extern "C" fn h_cond(cpu: *mut InterpCpu, cc: u32) -> u32 {
    // SAFETY: the caller's contract; only the flags are read.
    let cpu = unsafe { &*cpu };
    u32::from(cpu.flags.cond(cc))
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

/// Generates the code of a block for a machine with `pages` pages of RAM.
fn generate(ops: &[Uop], o: &Off, pages: u32) -> Vec<u32> {
    let mut g = Gen { a: Asm::default(), o, pages, exits: Vec::new(), operands: None };
    g.a.prologue();
    for k in 0..4 {
        g.a.ldr_x(PT + k, 19, o.tables + 8 * k);
    }
    let mut open = true;
    for (i, u) in ops.iter().enumerate() {
        open = g.uop(i as u32, u);
    }
    if open && let Some(last) = ops.last() {
        g.set_eip(last.next);
        g.a.zero_x0();
    }
    let exit = g.a.pos();
    g.a.epilogue();
    for at in std::mem::take(&mut g.exits) {
        g.a.patch(at, exit);
    }
    g.a.code
}

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
}

impl Gen<'_> {
    fn gpr(&self, r: u8) -> u32 {
        self.o.gpr + 4 * u32::from(r)
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

    /// Returns 0: the block is over and `eip` is stored.
    fn finish(&mut self) {
        self.a.zero_x0();
        let at = self.a.b();
        self.exits.push(at);
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

    /// The start of an access to the address in `w1` that the code makes
    /// itself: `x9` = the offset in the page, `w11` = the page-table entry,
    /// `x12` = the physical page, in RAM. Returns the branches taken when
    /// the access is not such a one (it crosses a page, or the page is not
    /// RAM, or the entry lacks `need`), which go to the helper.
    fn translate(&mut self, need: u32) -> Vec<usize> {
        let mut slow = Vec::with_capacity(8);
        self.a.low12(9, 1);
        self.a.cmp_w_imm(9, 0xFFC);
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

    /// `w0` = the 32 bits at the address in `w1`; returns from the block if
    /// the load stopped. RAM that write tracking has not closed is read in
    /// place, as `Mem::read` would; anything else is the helper's.
    fn load(&mut self, i: u32) {
        let mut slow = self.translate(PTE_READ);
        // Closed (2) stops a load; read-only (1) does not.
        slow.push(self.a.tbnz_w(13, 1));
        self.a.add_x_lsl12(12, 9, 12);
        self.a.ldr_w_idx(0, RAM, 12);
        let done = self.a.b();
        let here = self.a.pos();
        for at in slow {
            self.a.patch(at, here);
        }
        self.a.mov_x(0, 19);
        self.a.mov_w_imm(2, i);
        self.call(H_LOAD);
        self.exit_if_stopped();
        let here = self.a.pos();
        self.a.patch(done, here);
    }

    /// Stores `w2` at the address in `w1`; `x0` is then 0 or the helper's
    /// status, which the caller checks. RAM with no restriction and no
    /// decoded code on its page is written in place, as `Mem::write_n`
    /// would; anything else is the helper's.
    fn store(&mut self, i: u32) {
        let mut slow = self.translate(PTE_WRITE);
        slow.push(self.a.cbnz_w(13));
        self.a.ldrb_idx(13, CODE, 12);
        slow.push(self.a.cbnz_w(13));
        self.a.add_x_lsl12(12, 9, 12);
        self.a.str_w_idx(2, RAM, 12);
        self.a.zero_x0();
        let done = self.a.b();
        let here = self.a.pos();
        for at in slow {
            self.a.patch(at, here);
        }
        self.a.mov_x(0, 19);
        self.uop_ptr(i, 3);
        self.a.mov_w_imm(4, i);
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

    /// Records a 32-bit operation (`a` in `w10`, `b` in `w11`, the result
    /// in `w12`) as `Flags::lazy` does for ADD, SUB and the logic
    /// operations.
    fn lazy(&mut self, kind: Op) {
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
        self.a.mov_w_imm(13, 32);
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
        self.store(i);
        self.exit_if_stopped();
        self.move_esp(true);
    }

    /// Emits instruction `i`; returns whether control goes on to the next.
    fn uop(&mut self, i: u32, u: &Uop) -> bool {
        if !u.fast {
            self.handler(i);
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
                    self.load(i);
                    self.a.mov_w(11, 0);
                } else {
                    self.a.ldr_w(11, 19, self.gpr(u.rm));
                }
                self.a.ldr_w(10, 19, self.gpr(u.r));
                let kind = self.alu(op);
                if writes {
                    self.a.str_w(12, 19, self.gpr(u.r));
                }
                self.lazy(kind);
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
                self.load(i);
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
            self.lazy(kind);
            self.operands = Some((i, kind));
            return true;
        }
        match code {
            C::Mov_r32_imm32 => {
                self.a.mov_w_imm(9, u.imm);
                self.a.str_w(9, 19, self.gpr(u.r));
            }
            C::Mov_r32_rm32 => {
                if mem {
                    self.addr(&u.ea, 1);
                    self.load(i);
                    self.a.str_w(0, 19, self.gpr(u.r));
                } else {
                    self.a.ldr_w(9, 19, self.gpr(u.rm));
                    self.a.str_w(9, 19, self.gpr(u.r));
                }
            }
            C::Mov_rm32_r32 => {
                if mem {
                    self.addr(&u.ea, 1);
                    self.a.ldr_w(2, 19, self.gpr(u.r));
                    self.store(i);
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
                    self.store(i);
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
                self.load(i);
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
                // A store that changed code ends the block, as this
                // instruction does anyway.
                self.set_eip(u.imm);
                self.finish();
                return false;
            }
            C::Retnd | C::Retnd_imm16 => {
                let esp = self.gpr(ESP);
                self.a.ldr_w(1, 19, esp);
                self.load(i);
                self.a.str_w(0, 19, self.o.eip);
                self.a.ldr_w(9, 19, esp);
                self.a.mov_w_imm(10, 4 + u.imm);
                self.a.add_w(9, 9, 10);
                self.a.str_w(9, 19, esp);
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
