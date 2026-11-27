//! The translation of guest x86-32 instructions into x86-64 host code.
//!
//! The approach is the reference's: an instruction keeps its guest encoding
//! wherever it can, is decoded as 32-bit code and encoded again for 64-bit
//! mode with the guest's registers in the host registers of the same name
//! (ESP in R12). What differs from the reference, and why:
//!
//! * **Guest memory is `gs:[addr32]`.** The reference keeps the guest at host
//!   address 0; here it sits at a base that is `GS`'s base, so every memory
//!   operand gets a `GS` prefix and a 32-bit address size. The effective
//!   address wraps at 4 GiB as the guest's does and the processor adds the
//!   base. String instructions cannot use the segment for their `ES:[EDI]`
//!   operand, so they run with 64-bit addressing on `RSI`/`RDI` with the
//!   base added for the instruction's duration (the base is a multiple of
//!   4 GiB, so the low halves are the guest's values at every moment).
//! * **The guest's FS base is a register** (`R14`), because a context carries
//!   it, rather than a constant baked into the code.
//! * **PUSHF does not use POPF** and never leaves the host's flags modified
//!   at a point that can fault: a fault in translated code is retried from
//!   the guest instruction's start with the registers and flags it was
//!   interrupted with.
//! * **Instructions the host cannot run** leave to `X64Cpu::run` (a "system
//!   op"), which executes them on the saved state (`emulate`).
//!
//! Every sequence here keeps one invariant the fault path relies on: until
//! the last host instruction that can fault has run, no guest register, flag
//! or memory has been changed (or the change is one the instruction's
//! restart repeats exactly).

use iced_x86::{
    Code, ConditionCode, Decoder, DecoderError, DecoderOptions, Instruction,
    InstructionInfoFactory, MemoryOperand, Mnemonic, OpKind, Register,
};

use crate::asm::{Asm, AsmError, fr, gmem};
use crate::frame::{Frame, reason};
use crate::mem::Memory;
use crate::stubs::Stubs;

/// The guest flags that live in the host's RFLAGS: ID, OF, DF, SF, ZF, AF,
/// PF, CF. IF and AC are virtual; TF, NT, IOPL, RF and VM stay the host's.
pub(crate) const GUEST_FLAGS: u32 = 0x0020_0DD5;
/// EFLAGS.IF, also the value of `Frame::vif` when interrupts are enabled.
pub(crate) const FLAG_IF: u32 = 0x200;
/// EFLAGS.AC, also the value of `Frame::vac` when the guest set it.
pub(crate) const FLAG_AC: u32 = 0x4_0000;

/// Most instructions in one block.
const MAX_INSNS: usize = 256;

/// What `SIDT`, `SGDT` report (the kernel's tables; the translator only
/// stores the numbers).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DescriptorTables {
    /// Linear address of the IDT.
    pub idt_base: u32,
    /// Its limit.
    pub idt_limit: u16,
    /// Linear address of the GDT.
    pub gdt_base: u32,
    /// Its limit.
    pub gdt_limit: u16,
}

/// What the translator is told about its surroundings.
pub(crate) struct Env<'a> {
    pub(crate) stubs: &'a Stubs,
    pub(crate) mem: &'a Memory,
    /// `[start, end)` of the trap range.
    pub(crate) trap: (u32, u32),
    pub(crate) tables: DescriptorTables,
}

/// Where a host offset in a translation belongs in guest code.
#[derive(Clone, Copy, Debug)]
pub(crate) struct InsnMap {
    /// Offset of the instruction's first host byte in the translation.
    pub(crate) host_off: u32,
    /// The guest instruction's address.
    pub(crate) eip: u32,
    /// 1 + the number of the low register that holds a memory operand's
    /// address while the instruction runs; its own value waits in `R13`.
    pub(crate) borrow: u8,
}

/// A direct jump out of a translation, patched to its target's translation
/// when there is one (block chaining).
#[derive(Clone, Copy, Debug)]
pub(crate) struct ExitSite {
    /// Offset of the jump's `rel32`.
    pub(crate) patch: u32,
    /// Offset of the unchained target: `mov r13d, target; jmp dispatch`.
    pub(crate) stub: u32,
    /// Guest address it goes to.
    pub(crate) target: u32,
    /// The target is not above the instruction that jumps: such an edge
    /// enters its target through the kick poll.
    pub(crate) backward: bool,
}

/// One translated block (or single instruction).
pub(crate) struct Translation {
    /// Host code, assembled for the address given to [`translate`].
    pub(crate) code: Vec<u8>,
    /// Offset of the entry that skips the kick poll.
    pub(crate) entry_nopoll: u32,
    /// One entry per guest instruction, in host order.
    pub(crate) map: Vec<InsnMap>,
    /// Its direct jumps.
    pub(crate) exits: Vec<ExitSite>,
    /// One past the last guest byte the translation depends on.
    pub(crate) dep_end: u64,
    /// The block's first instruction could not be fetched: the code touches
    /// the missing byte (a guest page fault) and must not be kept.
    pub(crate) probe: bool,
}

/// How much to translate.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// A block, to be cached and chained.
    Block,
    /// One instruction, to run once from the scratch area. With an address,
    /// the instruction's memory operand is that host buffer instead of guest
    /// memory (a device access).
    Step(Option<u64>),
}

/// Why an instruction got no translation.
#[derive(Debug)]
pub(crate) struct Fail;

impl From<AsmError> for Fail {
    fn from(_: AsmError) -> Self {
        Fail
    }
}

type R<T> = Result<T, Fail>;

enum Flow {
    Continue,
    End,
}

struct PendingExit {
    patch: usize,
    target: u32,
    backward: bool,
}

struct PendingIfCheck {
    /// The `jmp rel32` taken when `exit_if` is set.
    patch: usize,
    /// Where translated code continues when interrupts stay disabled.
    back: usize,
    next: u32,
}

/// 0F xx opcodes that are MMX instructions on a Pentium III (MMX, and SSE's
/// integer ones on MMX registers) and SSE2's with a 66 prefix elsewhere.
fn mmx_opcode(op: u8) -> bool {
    matches!(op,
        0x60..=0x6B | 0x6E | 0x6F | 0x70..=0x76 | 0x7E | 0x7F | 0xC4 | 0xC5
        | 0xD1..=0xD3 | 0xD5 | 0xD7..=0xDF | 0xE0..=0xE5 | 0xE7..=0xEF
        | 0xF1..=0xF3 | 0xF5..=0xFA | 0xFC..=0xFE)
}

/// Decodes the guest instruction at `at`. Returns the instruction (invalid
/// if the bytes are none) and whether decoding ran out of readable bytes.
pub(crate) fn decode(mem: &Memory, at: u32) -> (Instruction, bool) {
    let mut bytes = [0u8; 15];
    let avail = mem.read(at, &mut bytes);
    // The console's processor has no SSE2: an MMX instruction with an
    // operand-size prefix (66 0F xx) is MMX there, the prefix ignored; the
    // host would run SSE2's form (XMM registers, a 16-byte aligned operand).
    // Dr. Muto adds a pixel with 66 0F DC (PADDUSB) from an unaligned
    // address. Decoded without the prefix, as the console runs it.
    let mmx66 = avail >= 3 && bytes[0] == 0x66 && bytes[1] == 0x0F && mmx_opcode(bytes[2]);
    let skip = usize::from(mmx66);
    let mut d = Decoder::with_ip(32, &bytes[skip..avail], u64::from(at), DecoderOptions::NONE);
    let mut i = Instruction::default();
    d.decode_out(&mut i);
    let short = i.is_invalid() && d.last_error() == DecoderError::NoMoreBytes && avail < 15;
    if mmx66 && !i.is_invalid() {
        i.set_len(i.len() + 1);
        i.set_next_ip(u64::from(at.wrapping_add(i.len() as u32)));
    }
    (i, short)
}

fn is_high8(r: Register) -> bool {
    matches!(r, Register::AH | Register::CH | Register::DH | Register::BH)
}

/// ESP's forms to R12's; anything else as it is.
fn host_reg(r: Register) -> Register {
    match r {
        Register::ESP => Register::R12D,
        Register::SP => Register::R12W,
        _ => r,
    }
}

const GPR32: [Register; 8] = [
    Register::EAX,
    Register::ECX,
    Register::EDX,
    Register::EBX,
    Register::R12D,
    Register::EBP,
    Register::ESI,
    Register::EDI,
];
const GPR16: [Register; 8] = [
    Register::AX,
    Register::CX,
    Register::DX,
    Register::BX,
    Register::R12W,
    Register::BP,
    Register::SI,
    Register::DI,
];
const GPR64: [Register; 8] = [
    Register::RAX,
    Register::RCX,
    Register::RDX,
    Register::RBX,
    Register::R12,
    Register::RBP,
    Register::RSI,
    Register::RDI,
];

/// The 32-bit register a 16-bit address register stands for.
fn widen16(r: Register) -> Register {
    match r {
        Register::BX => Register::EBX,
        Register::BP => Register::EBP,
        Register::SI => Register::ESI,
        Register::DI => Register::EDI,
        _ => r,
    }
}

/// The selector a segment register holds in the flat model.
fn selector(s: Register) -> u32 {
    match s {
        Register::CS => 0x08,
        Register::FS => 0x28,
        Register::GS => 0,
        _ => 0x10,
    }
}

/// A memory operand after rewriting for the host.
struct MemPlan {
    base: Register,
    index: Register,
    scale: u32,
    disp: u32,
    seg: Register,
    /// The buffer of a device access (RIP-relative).
    host: Option<u64>,
}

impl MemPlan {
    fn operand(&self) -> MemoryOperand {
        if let Some(a) = self.host {
            return MemoryOperand::with_base_displ(Register::RIP, a as i64);
        }
        let none = self.base == Register::None && self.index == Register::None;
        let displ_size = if none {
            4
        } else if self.disp == 0 {
            0
        } else {
            1
        };
        MemoryOperand::new(
            self.base,
            self.index,
            self.scale,
            i64::from(self.disp as i32),
            displ_size,
            false,
            self.seg,
        )
    }

    fn apply(&self, o: &mut Instruction) {
        if let Some(a) = self.host {
            o.set_memory_base(Register::RIP);
            o.set_memory_index(Register::None);
            o.set_memory_index_scale(1);
            o.set_memory_displacement64(a);
            o.set_memory_displ_size(8);
            o.set_segment_prefix(Register::None);
            return;
        }
        let none = self.base == Register::None && self.index == Register::None;
        o.set_memory_base(self.base);
        o.set_memory_index(self.index);
        o.set_memory_index_scale(self.scale);
        o.set_memory_displacement64(u64::from(self.disp));
        o.set_memory_displ_size(if none {
            4
        } else if self.disp == 0 {
            0
        } else {
            1
        });
        o.set_segment_prefix(self.seg);
    }
}

struct Translator<'a> {
    env: &'a Env<'a>,
    a: Asm,
    mode: Mode,
    map: Vec<InsnMap>,
    exits: Vec<PendingExit>,
    if_checks: Vec<PendingIfCheck>,
    info: InstructionInfoFactory,
}

impl Translator<'_> {
    fn exit_to(&mut self, from: u32, target: u32) {
        let patch = self.a.jmp(0);
        self.exits.push(PendingExit { patch, target, backward: target <= from });
    }

    fn sysop(&mut self, at: u32) -> R<Flow> {
        self.a.exit(reason::SYSOP, at, self.env.stubs.exit)?;
        Ok(Flow::End)
    }

    /// `lea r12d, [r12 + n]`.
    fn esp_add(&mut self, n: i32) -> R<()> {
        self.a.op2(
            Code::Lea_r32_m,
            Register::R12D,
            MemoryOperand::with_base_displ(Register::R12, i64::from(n)),
        )?;
        Ok(())
    }

    /// Pushes a 32-bit host register: the store first (a fault leaves ESP
    /// as it was), then ESP.
    fn push_r(&mut self, r: Register) -> R<()> {
        self.a.op2(Code::Mov_rm32_r32, gmem(Register::R12D, -4), r)?;
        self.esp_add(-4)
    }

    fn push_imm(&mut self, v: u32) -> R<()> {
        self.a.op2(Code::Mov_rm32_imm32, gmem(Register::R12D, -4), v)?;
        self.esp_add(-4)
    }

    /// Rewrites the explicit memory operand of `i` for the host.
    ///
    /// Flat operands keep their registers. FS-relative ones add `R14`; when
    /// the operand already has a base and an index, or uses 16-bit
    /// addressing, the address is computed into `tmp` first. With `force`,
    /// the whole address (FS base included) always goes through `tmp`.
    /// `extra` is added to the displacement.
    fn plan_mem(&mut self, i: &Instruction, tmp: Register, force: bool, extra: u32) -> R<MemPlan> {
        if let Mode::Step(Some(host)) = self.mode {
            return Ok(MemPlan {
                base: Register::None,
                index: Register::None,
                scale: 1,
                disp: 0,
                seg: Register::None,
                host: Some(host + u64::from(extra)),
            });
        }
        let base = i.memory_base();
        let index = i.memory_index();
        let scale = if index == Register::None { 1 } else { i.memory_index_scale() };
        let fs = i.segment_prefix() == Register::FS;
        let addr16 = base.is_gpr16()
            || index.is_gpr16()
            || (base == Register::None && index == Register::None && i.memory_displ_size() == 2);
        if !(base == Register::None || base.is_gpr32() || base.is_gpr16())
            || !(index == Register::None || index.is_gpr32() || index.is_gpr16())
        {
            return Err(Fail);
        }
        let disp = if addr16 {
            i32::from(i.memory_displacement32() as u16 as i16) as u32
        } else {
            i.memory_displacement32()
        }
        .wrapping_add(extra);
        let mut plan = MemPlan {
            base: host_reg(widen16(base)),
            index: widen16(index),
            scale,
            disp,
            seg: Register::GS,
            host: None,
        };
        let both = base != Register::None && index != Register::None;
        if force || addr16 || (fs && both) {
            let flat = MemPlan { seg: Register::None, ..plan };
            self.a.op2(Code::Lea_r32_m, tmp, flat.operand())?;
            if addr16 {
                let t16 = GPR16
                    .iter()
                    .zip(GPR32)
                    .find_map(|(&w, d)| (d == tmp).then_some(w))
                    .unwrap_or(Register::R13W);
                self.a.op2(Code::Movzx_r32_rm16, tmp, t16)?;
            }
            plan = MemPlan {
                base: tmp,
                index: Register::None,
                scale: 1,
                disp: 0,
                seg: Register::GS,
                host: None,
            };
            if fs && force {
                // Through the borrowed register alone: no REX in the
                // instruction that follows.
                self.a.op2(
                    Code::Lea_r32_m,
                    tmp,
                    MemoryOperand::with_base_index(tmp.full_register(), Register::R14),
                )?;
            } else if fs {
                plan.index = Register::R14D;
            }
        } else if fs {
            if plan.base == Register::None {
                plan.base = Register::R14D;
            } else {
                plan.index = Register::R14D;
                plan.scale = 1;
            }
        }
        Ok(plan)
    }

    /// The explicit memory operand of `i` as an encoder operand (through
    /// `R13` where an address has to be computed).
    fn mem_operand(&mut self, i: &Instruction, extra: u32) -> R<MemoryOperand> {
        Ok(self.plan_mem(i, Register::R13D, false, extra)?.operand())
    }

    /// `r13d` (or `r13w`) = operand `k` of `i`: a register, an immediate or
    /// memory.
    fn load_r13(&mut self, i: &Instruction, k: u32, bytes: u32) -> R<()> {
        let wide = bytes == 4;
        let dst = if wide { Register::R13D } else { Register::R13W };
        match i.op_kind(k) {
            OpKind::Register => {
                let r = host_reg(i.op_register(k));
                if wide {
                    self.a.op2(Code::Mov_r32_rm32, dst, r)?;
                } else {
                    self.a.op2(Code::Mov_r16_rm16, dst, r)?;
                }
            }
            OpKind::Memory => {
                let m = self.mem_operand(i, 0)?;
                if wide {
                    self.a.op2(Code::Mov_r32_rm32, dst, m)?;
                } else {
                    self.a.op2(Code::Mov_r16_rm16, dst, m)?;
                }
            }
            OpKind::Immediate8
            | OpKind::Immediate16
            | OpKind::Immediate32
            | OpKind::Immediate8to16
            | OpKind::Immediate8to32 => {
                self.a.op2(Code::Mov_r32_imm32, Register::R13D, i.immediate(k) as u32)?;
            }
            _ => return Err(Fail),
        }
        Ok(())
    }

    /// After STI or POPF: leave with `KICK` if the caller of `run` asked to
    /// be told when interrupts become enabled. Flags and registers are live.
    fn if_check(&mut self, next: u32) -> R<()> {
        self.a.op2(Code::Mov_r64_rm64, Register::R10, Register::RCX)?;
        self.a.op2(Code::Mov_r32_rm32, Register::ECX, fr(Frame::EXIT_IF))?;
        let cont = self.a.jrcxz();
        let patch = self.a.jmp(0);
        self.a.bind8(cont);
        let back = self.a.len();
        self.a.op2(Code::Mov_r64_rm64, Register::RCX, Register::R10)?;
        self.if_checks.push(PendingIfCheck { patch, back, next });
        Ok(())
    }

    /// The host's flags image as the guest's EFLAGS in `r13d`, without
    /// changing the flags: the image is the guest's flags plus the host's IF
    /// (always set in user mode), which the guest's IF and AC replace.
    fn flags_to_r13(&mut self) -> R<()> {
        self.a.op0(Code::Pushfq)?;
        self.a.op1(Code::Pop_r64, Register::R13)?;
        let lea = |disp: i64| MemoryOperand::with_base_displ(Register::R13, disp);
        self.a.op2(Code::Lea_r32_m, Register::R13D, lea(-i64::from(FLAG_IF)))?;
        for off in [Frame::VIF, Frame::VAC] {
            self.a.op2(Code::Mov_r32_rm32, Register::R11D, fr(off))?;
            self.a.op2(
                Code::Lea_r32_m,
                Register::R13D,
                MemoryOperand::with_base_index(Register::R13, Register::R11),
            )?;
        }
        Ok(())
    }

    /// `r13d` (guest EFLAGS) into the virtual IF and AC and the host's
    /// flags. `keep_high`: a 16-bit POPF, the upper half of the flags stays.
    fn r13_to_flags(&mut self, keep_high: bool) -> R<()> {
        if keep_high {
            self.a.op0(Code::Pushfq)?;
            self.a.op1(Code::Pop_r64, Register::R10)?;
            self.a.op2(Code::And_rm32_imm32, Register::R10D, GUEST_FLAGS & 0xFFFF_0000)?;
        }
        self.a.op2(Code::Mov_r32_rm32, Register::R11D, Register::R13D)?;
        self.a.op2(Code::And_rm32_imm32, Register::R11D, FLAG_IF)?;
        self.a.op2(Code::Mov_rm32_r32, fr(Frame::VIF), Register::R11D)?;
        if keep_high {
            self.a.op2(Code::And_rm32_imm32, Register::R13D, GUEST_FLAGS & 0xFFFF)?;
            self.a.op2(Code::Or_rm32_r32, Register::R13D, Register::R10D)?;
        } else {
            self.a.op2(Code::Mov_r32_rm32, Register::R11D, Register::R13D)?;
            self.a.op2(Code::And_rm32_imm32, Register::R11D, FLAG_AC)?;
            self.a.op2(Code::Mov_rm32_r32, fr(Frame::VAC), Register::R11D)?;
            self.a.op2(Code::And_rm32_imm32, Register::R13D, GUEST_FLAGS)?;
        }
        self.a.op2(Code::Or_rm32_imm32, Register::R13D, 0x202u32)?;
        self.a.op1(Code::Push_r64, Register::R13)?;
        self.a.op0(Code::Popfq)?;
        Ok(())
    }

    /// Stores a 16-bit limit then a 32-bit base at a memory operand
    /// (SIDT, SGDT).
    fn store_table_reg(&mut self, i: &Instruction, limit: u16, base: u32) -> R<()> {
        let m0 = self.mem_operand(i, 0)?;
        self.a.op2(Code::Mov_rm16_imm16, m0, u32::from(limit))?;
        let m2 = self.mem_operand(i, 2)?;
        self.a.op2(Code::Mov_rm32_imm32, m2, base)?;
        Ok(())
    }

    /// A string instruction: 64-bit addressing with the base added to the
    /// index registers it uses, taken off again afterwards (`mov esi, esi`
    /// keeps the low half, which is the guest's value throughout).
    fn string(&mut self, i: &Instruction) -> R<()> {
        if i.segment_prefix() == Register::FS {
            return Err(Fail);
        }
        let mut o = *i;
        let (mut si, mut di) = (false, false);
        for k in 0..i.op_count() {
            match i.op_kind(k) {
                OpKind::MemorySegESI => {
                    o.set_op_kind(k, OpKind::MemorySegRSI);
                    si = true;
                }
                OpKind::MemoryESEDI => {
                    o.set_op_kind(k, OpKind::MemoryESRDI);
                    di = true;
                }
                OpKind::Register => {}
                _ => return Err(Fail),
            }
        }
        o.set_segment_prefix(Register::None);
        let add = |r| MemoryOperand::with_base_index(r, Register::R8);
        if si {
            self.a.op2(Code::Lea_r64_m, Register::RSI, add(Register::RSI))?;
        }
        if di {
            self.a.op2(Code::Lea_r64_m, Register::RDI, add(Register::RDI))?;
        }
        self.a.ins(&o)?;
        if si {
            self.a.op2(Code::Mov_r32_rm32, Register::ESI, Register::ESI)?;
        }
        if di {
            self.a.op2(Code::Mov_r32_rm32, Register::EDI, Register::EDI)?;
        }
        Ok(())
    }

    /// A low register (EBP, ESI, EDI, EBX, EDX, ECX, EAX in that order)
    /// the instruction does not use in any way, as its number.
    fn unused_low(&mut self, i: &Instruction) -> R<usize> {
        let info = self.info.info(i);
        let used = info.used_registers();
        [5usize, 6, 7, 3, 2, 1, 0]
            .into_iter()
            .find(|&n| {
                let full = if n == 4 { Register::ESP } else { GPR32[n] };
                !used.iter().any(|u| u.register().full_register32() == full)
            })
            .ok_or(Fail)
    }

    /// The instruction re-encoded for 64-bit mode (see the module's
    /// documentation).
    fn generic(&mut self, i: &Instruction) -> R<()> {
        if i.stack_pointer_increment() != 0 {
            // The stack used implicitly by an instruction the cases in
            // `insn` do not take: encoded as it is, it would push or pop the
            // shadow stack.
            return Err(Fail);
        }
        let mut o = *i;
        // Forms that exist only outside 64-bit mode, or that cannot take
        // the rewritten operand.
        let code = match i.code() {
            Code::Inc_r16 => Code::Inc_rm16,
            Code::Inc_r32 => Code::Inc_rm32,
            Code::Dec_r16 => Code::Dec_rm16,
            Code::Dec_r32 => Code::Dec_rm32,
            Code::Add_rm8_imm8_82 => Code::Add_rm8_imm8,
            Code::Or_rm8_imm8_82 => Code::Or_rm8_imm8,
            Code::Adc_rm8_imm8_82 => Code::Adc_rm8_imm8,
            Code::Sbb_rm8_imm8_82 => Code::Sbb_rm8_imm8,
            Code::And_rm8_imm8_82 => Code::And_rm8_imm8,
            Code::Sub_rm8_imm8_82 => Code::Sub_rm8_imm8,
            Code::Xor_rm8_imm8_82 => Code::Xor_rm8_imm8,
            Code::Cmp_rm8_imm8_82 => Code::Cmp_rm8_imm8,
            Code::Mov_AL_moffs8 => Code::Mov_r8_rm8,
            Code::Mov_AX_moffs16 => Code::Mov_r16_rm16,
            Code::Mov_EAX_moffs32 => Code::Mov_r32_rm32,
            Code::Mov_moffs8_AL => Code::Mov_rm8_r8,
            Code::Mov_moffs16_AX => Code::Mov_rm16_r16,
            Code::Mov_moffs32_EAX => Code::Mov_rm32_r32,
            c => c,
        };
        o.set_code(code);

        let (mut high8, mut rex_reg, mut mem, mut implicit) = (false, false, false, false);
        for k in 0..i.op_count() {
            match i.op_kind(k) {
                OpKind::Register => {
                    let r = i.op_register(k);
                    if r.is_segment_register() || r.is_cr() || r.is_dr() || r.is_tr() {
                        return Err(Fail);
                    }
                    high8 |= is_high8(r);
                    let h = host_reg(r);
                    if h != r {
                        o.set_op_register(k, h);
                        rex_reg = true;
                    }
                }
                OpKind::Memory => mem = true,
                OpKind::MemorySegESI | OpKind::MemoryESEDI | OpKind::MemorySegEDI => {
                    implicit = true
                }
                OpKind::Immediate8
                | OpKind::Immediate8_2nd
                | OpKind::Immediate16
                | OpKind::Immediate32
                | OpKind::Immediate8to16
                | OpKind::Immediate8to32 => {}
                _ => return Err(Fail),
            }
        }
        if high8 && rex_reg {
            return Err(Fail);
        }
        if implicit {
            return match i.mnemonic() {
                Mnemonic::Movsb
                | Mnemonic::Movsw
                | Mnemonic::Movsd
                | Mnemonic::Cmpsb
                | Mnemonic::Cmpsw
                | Mnemonic::Cmpsd
                | Mnemonic::Stosb
                | Mnemonic::Stosw
                | Mnemonic::Stosd
                | Mnemonic::Lodsb
                | Mnemonic::Lodsw
                | Mnemonic::Lodsd
                | Mnemonic::Scasb
                | Mnemonic::Scasw
                | Mnemonic::Scasd => self.string(i),
                Mnemonic::Maskmovq if i.segment_prefix() != Register::FS => {
                    o.set_segment_prefix(Register::GS);
                    Ok(self.a.ins(&o)?)
                }
                _ => Err(Fail),
            };
        }
        if !mem {
            o.set_segment_prefix(Register::None);
            return Ok(self.a.ins(&o)?);
        }
        let fs = i.segment_prefix() == Register::FS;
        match i.mnemonic() {
            Mnemonic::Lea => {
                // The address only: no segment, FS's base not added.
                let base = i.memory_base();
                let index = i.memory_index();
                if base.is_gpr16() || index.is_gpr16() || i.memory_displ_size() == 2 {
                    // 16-bit addressing: the address computed and cut to
                    // 16 bits through R13.
                    let flat = MemPlan {
                        base: host_reg(widen16(base)),
                        index: widen16(index),
                        scale: 1,
                        disp: i32::from(i.memory_displacement32() as u16 as i16) as u32,
                        seg: Register::None,
                        host: None,
                    };
                    self.a.op2(Code::Lea_r32_m, Register::R13D, flat.operand())?;
                    let d = host_reg(i.op_register(0));
                    if d.is_gpr16() {
                        self.a.op2(Code::Mov_r16_rm16, d, Register::R13W)?;
                    } else {
                        self.a.op2(Code::Movzx_r32_rm16, d, Register::R13W)?;
                    }
                    return Ok(());
                }
                o.set_segment_prefix(Register::None);
                o.set_memory_base(host_reg(base));
                return Ok(self.a.ins(&o)?);
            }
            Mnemonic::Xlatb => {
                if fs || matches!(self.mode, Mode::Step(Some(_))) {
                    return Err(Fail);
                }
                o.set_segment_prefix(Register::GS);
                return Ok(self.a.ins(&o)?);
            }
            _ => {}
        }
        let device = matches!(self.mode, Mode::Step(Some(_)));
        let addr16 = i.memory_base().is_gpr16()
            || i.memory_index().is_gpr16()
            || (i.memory_base() == Register::None
                && i.memory_index() == Register::None
                && i.memory_displ_size() == 2);
        let needs_rex = !device && (fs || addr16 || i.memory_base() == Register::ESP);
        if high8 && needs_rex {
            // AH-DH cannot be encoded beside R12-R14: the operand's address
            // goes through a low register the instruction does not use,
            // whose own value waits in R13 meanwhile (the map's `borrow`,
            // for a fault in between).
            let t = self.unused_low(i)?;
            self.a.op2(Code::Mov_r64_rm64, Register::R13, GPR64[t])?;
            let plan = self.plan_mem(i, GPR32[t], true, 0)?;
            plan.apply(&mut o);
            if let Some(m) = self.map.last_mut() {
                m.borrow = t as u8 + 1;
            }
            self.a.ins(&o)?;
            self.a.op2(Code::Mov_r64_rm64, GPR64[t], Register::R13)?;
            return Ok(());
        }
        let plan = self.plan_mem(i, Register::R13D, false, 0)?;
        plan.apply(&mut o);
        Ok(self.a.ins(&o)?)
    }

    /// One instruction at `at`.
    fn insn(&mut self, i: &Instruction, at: u32) -> R<Flow> {
        let next = at.wrapping_add(i.len() as u32);
        let stubs = *self.env.stubs;
        match i.mnemonic() {
            // ---- control flow ----
            Mnemonic::Jmp => match i.code() {
                Code::Jmp_rel8_32 | Code::Jmp_rel32_32 => {
                    self.exit_to(at, i.near_branch32());
                    Ok(Flow::End)
                }
                Code::Jmp_rm32 => {
                    self.load_r13(i, 0, 4)?;
                    self.a.jmp(stubs.dispatch);
                    Ok(Flow::End)
                }
                _ => Err(Fail),
            },
            Mnemonic::Call => {
                // The guest's return address on its stack; on the host's,
                // its negation and a host call: a return to it is a host RET
                // the processor predicts (RET below), back here, to a
                // chained jump to the return address's block.
                let neg = 0u32.wrapping_sub(next) as i32;
                match i.code() {
                    Code::Call_rel32_32 => {
                        self.push_imm(next)?;
                        self.a.ins(&Instruction::with1(Code::Pushq_imm32, neg).map_err(|_| Fail)?)?;
                        let patch = self.a.call(0);
                        let target = i.near_branch32();
                        self.exits.push(PendingExit { patch, target, backward: target <= at });
                    }
                    Code::Call_rm32 => {
                        self.load_r13(i, 0, 4)?;
                        self.push_imm(next)?;
                        self.a.ins(&Instruction::with1(Code::Pushq_imm32, neg).map_err(|_| Fail)?)?;
                        self.a.call(stubs.dispatch);
                    }
                    _ => return Err(Fail),
                }
                self.exit_to(at, next);
                Ok(Flow::End)
            }
            Mnemonic::Ret => {
                let extra = match i.code() {
                    Code::Retnd => 0,
                    Code::Retnd_imm16 => i32::from(i.immediate16()),
                    _ => return Err(Fail),
                };
                self.a.op2(Code::Mov_r32_rm32, Register::R13D, gmem(Register::R12D, 0))?;
                self.esp_add(4 + extra)?;
                // The shadow stack's top predicts it: [rsp] the host return,
                // [rsp+8] the guest one negated; equal (lea ecx, [rcx + r13]
                // zero, the flags untouched): RET 8. Else the shadow stack
                // goes (a longjmp, a thread switch, a return address
                // replaced) and the dispatcher finds the block.
                self.a.op2(Code::Mov_r64_rm64, Register::R10, Register::RCX)?;
                self.a.op2(
                    Code::Mov_r32_rm32,
                    Register::ECX,
                    MemoryOperand::with_base_displ(Register::RSP, 8),
                )?;
                self.a.op2(
                    Code::Lea_r32_m,
                    Register::ECX,
                    MemoryOperand::with_base_index(Register::RCX, Register::R13),
                )?;
                let hit = self.a.jrcxz();
                self.a.op2(Code::Mov_r64_rm64, Register::RCX, Register::R10)?;
                self.a.op2(Code::Mov_r64_rm64, Register::RSP, fr(Frame::SHADOW_TOP))?;
                self.a.jmp(stubs.dispatch);
                self.a.bind8(hit);
                self.a.op2(Code::Mov_r64_rm64, Register::RCX, Register::R10)?;
                self.a.op1(Code::Retnq_imm16, 8u32)?;
                Ok(Flow::End)
            }
            Mnemonic::Jo
            | Mnemonic::Jno
            | Mnemonic::Jb
            | Mnemonic::Jae
            | Mnemonic::Je
            | Mnemonic::Jne
            | Mnemonic::Jbe
            | Mnemonic::Ja
            | Mnemonic::Js
            | Mnemonic::Jns
            | Mnemonic::Jp
            | Mnemonic::Jnp
            | Mnemonic::Jl
            | Mnemonic::Jge
            | Mnemonic::Jle
            | Mnemonic::Jg => {
                if !matches!(i.op_kind(0), OpKind::NearBranch32) {
                    return Err(Fail);
                }
                let cc = match i.condition_code() {
                    ConditionCode::o => 0,
                    ConditionCode::no => 1,
                    ConditionCode::b => 2,
                    ConditionCode::ae => 3,
                    ConditionCode::e => 4,
                    ConditionCode::ne => 5,
                    ConditionCode::be => 6,
                    ConditionCode::a => 7,
                    ConditionCode::s => 8,
                    ConditionCode::ns => 9,
                    ConditionCode::p => 10,
                    ConditionCode::np => 11,
                    ConditionCode::l => 12,
                    ConditionCode::ge => 13,
                    ConditionCode::le => 14,
                    ConditionCode::g => 15,
                    ConditionCode::None => return Err(Fail),
                };
                let target = i.near_branch32();
                let patch = self.a.jcc(cc, 0);
                self.exits.push(PendingExit { patch, target, backward: target <= at });
                self.exit_to(at, next);
                Ok(Flow::End)
            }
            Mnemonic::Jecxz | Mnemonic::Jcxz | Mnemonic::Loop | Mnemonic::Loope | Mnemonic::Loopne => {
                if !matches!(i.op_kind(0), OpKind::NearBranch32) {
                    return Err(Fail);
                }
                let target = i.near_branch32();
                let counts = !matches!(i.mnemonic(), Mnemonic::Jecxz | Mnemonic::Jcxz);
                let cx16 = matches!(
                    i.code(),
                    Code::Jcxz_rel8_32 | Code::Loop_rel8_32_CX | Code::Loope_rel8_32_CX | Code::Loopne_rel8_32_CX
                );
                if cx16 {
                    // The count is CX (67 E0-E3): CX less one where it
                    // counts, its zero tested through RCX (JRCXZ), no flag
                    // touched; the guest's ECX waits in R10.
                    if counts {
                        self.a.op2(
                            Code::Lea_r32_m,
                            Register::R11D,
                            MemoryOperand::with_base_displ(Register::RCX, -1),
                        )?;
                        self.a.op2(Code::Mov_r16_rm16, Register::CX, Register::R11W)?;
                    }
                    self.a.op2(Code::Mov_r64_rm64, Register::R10, Register::RCX)?;
                    self.a.op2(Code::Movzx_r32_rm16, Register::ECX, Register::CX)?;
                    let zero = self.a.jrcxz();
                    let flag = match i.mnemonic() {
                        Mnemonic::Loope => Some(self.a.short(0x75)),
                        Mnemonic::Loopne => Some(self.a.short(0x74)),
                        _ => None,
                    };
                    // Not zero: LOOP's jump, JCXZ's fall through.
                    self.a.op2(Code::Mov_r64_rm64, Register::RCX, Register::R10)?;
                    self.exit_to(at, if counts { target } else { next });
                    self.a.bind8(zero);
                    if let Some(f) = flag {
                        self.a.bind8(f);
                    }
                    self.a.op2(Code::Mov_r64_rm64, Register::RCX, Register::R10)?;
                    self.exit_to(at, if counts { next } else { target });
                    return Ok(Flow::End);
                }
                let opc = match i.mnemonic() {
                    Mnemonic::Loop => 0xE2,
                    Mnemonic::Loope => 0xE1,
                    Mnemonic::Loopne => 0xE0,
                    _ => 0xE3,
                };
                // 67 op +2; jmp +5 (not taken); jmp rel32 (taken)
                self.a.raw(&[0x67, opc, 0x02, 0xEB, 0x05]);
                self.exit_to(at, target);
                self.exit_to(at, next);
                Ok(Flow::End)
            }
            Mnemonic::Iretd => self.sysop(at),

            // ---- the stack ----
            Mnemonic::Push => {
                let wide = match i.stack_pointer_increment() {
                    -4 => true,
                    -2 => false,
                    _ => return Err(Fail),
                };
                match i.op_kind(0) {
                    OpKind::Register if i.op_register(0).is_segment_register() => {
                        let sel = selector(i.op_register(0));
                        if wide {
                            self.push_imm(sel)?;
                        } else {
                            self.a.op2(Code::Mov_rm16_imm16, gmem(Register::R12D, -2), sel)?;
                            self.esp_add(-2)?;
                        }
                    }
                    OpKind::Register if wide => self.push_r(host_reg(i.op_register(0)))?,
                    OpKind::Register => {
                        // A word pushed (66 50+r): ESP down by 2.
                        // Castlevania: Curse of Darkness pushes DX as its
                        // title screen ends.
                        let r = host_reg(i.op_register(0));
                        self.a.op2(Code::Mov_rm16_r16, gmem(Register::R12D, -2), r)?;
                        self.esp_add(-2)?;
                    }
                    OpKind::Memory => {
                        // Its address taken with ESP as it was.
                        self.load_r13(i, 0, if wide { 4 } else { 2 })?;
                        if wide {
                            self.push_r(Register::R13D)?;
                        } else {
                            self.a.op2(Code::Mov_rm16_r16, gmem(Register::R12D, -2), Register::R13W)?;
                            self.esp_add(-2)?;
                        }
                    }
                    _ if wide => self.push_imm(i.immediate(0) as u32)?,
                    _ => {
                        let v = u32::from(i.immediate(0) as u16);
                        self.a.op2(Code::Mov_rm16_imm16, gmem(Register::R12D, -2), v)?;
                        self.esp_add(-2)?;
                    }
                }
                Ok(Flow::Continue)
            }
            Mnemonic::Pop => {
                let n: i32 = match i.stack_pointer_increment() {
                    4 => 4,
                    2 => 2,
                    _ => return Err(Fail),
                };
                let wide = n == 4;
                match i.op_kind(0) {
                    OpKind::Register if i.op_register(0).is_segment_register() => self.esp_add(n)?,
                    OpKind::Register => {
                        // (ESP itself: the value popped is its new value.)
                        let r = host_reg(i.op_register(0));
                        if wide {
                            self.a.op2(Code::Mov_r32_rm32, r, gmem(Register::R12D, 0))?;
                        } else {
                            self.a.op2(Code::Mov_r16_rm16, r, gmem(Register::R12D, 0))?;
                        }
                        if r != Register::R12D && r != Register::R12W {
                            self.esp_add(n)?;
                        }
                    }
                    OpKind::Memory => {
                        // The store before ESP moves: a store that faults
                        // (a write-tracked page) restarts the instruction,
                        // which must not find ESP popped already. The
                        // address is the one after the pop (ESP + n as the
                        // base); the value in R9, as an operand's address
                        // may take R13.
                        let after = if i.memory_base() == Register::ESP { n as u32 } else { 0 };
                        if wide {
                            self.a.op2(Code::Mov_r32_rm32, Register::R9D, gmem(Register::R12D, 0))?;
                            let m = self.mem_operand(i, after)?;
                            self.a.op2(Code::Mov_rm32_r32, m, Register::R9D)?;
                        } else {
                            self.a.op2(Code::Mov_r16_rm16, Register::R9W, gmem(Register::R12D, 0))?;
                            let m = self.mem_operand(i, after)?;
                            self.a.op2(Code::Mov_rm16_r16, m, Register::R9W)?;
                        }
                        self.esp_add(n)?;
                    }
                    _ => return Err(Fail),
                }
                Ok(Flow::Continue)
            }
            Mnemonic::Pushad => {
                for (k, r) in GPR32.into_iter().enumerate() {
                    self.a.op2(Code::Mov_rm32_r32, gmem(Register::R12D, -4 * (k as i32 + 1)), r)?;
                }
                self.esp_add(-32)?;
                Ok(Flow::Continue)
            }
            Mnemonic::Popad => {
                for k in (0..8).rev().filter(|&k| k != 4) {
                    self.a.op2(Code::Mov_r32_rm32, GPR32[k], gmem(Register::R12D, 4 * (7 - k as i32)))?;
                }
                self.esp_add(32)?;
                Ok(Flow::Continue)
            }
            Mnemonic::Pusha => {
                // 66 60: the 16-bit registers, 2 bytes each, SP as it was
                // (Rockstar's timer reads wrap RDTSC in PUSHAW / POPAW).
                for (k, r) in GPR16.into_iter().enumerate() {
                    self.a.op2(Code::Mov_rm16_r16, gmem(Register::R12D, -2 * (k as i32 + 1)), r)?;
                }
                self.esp_add(-16)?;
                Ok(Flow::Continue)
            }
            Mnemonic::Popa => {
                // 66 61: the low halves only, SP's word skipped.
                for k in (0..8).rev().filter(|&k| k != 4) {
                    self.a.op2(Code::Mov_r16_rm16, GPR16[k], gmem(Register::R12D, 2 * (7 - k as i32)))?;
                }
                self.esp_add(16)?;
                Ok(Flow::Continue)
            }
            Mnemonic::Pushfd => {
                self.flags_to_r13()?;
                self.push_r(Register::R13D)?;
                Ok(Flow::Continue)
            }
            Mnemonic::Pushf => {
                // 66 9C: the low word. America's Army: Rise of a Soldier
                // runs one after its intro.
                self.flags_to_r13()?;
                self.a.op2(Code::Mov_rm16_r16, gmem(Register::R12D, -2), Register::R13W)?;
                self.esp_add(-2)?;
                Ok(Flow::Continue)
            }
            Mnemonic::Popfd => {
                self.a.op2(Code::Mov_r32_rm32, Register::R13D, gmem(Register::R12D, 0))?;
                self.esp_add(4)?;
                self.r13_to_flags(false)?;
                self.if_check(next)?;
                Ok(Flow::Continue)
            }
            Mnemonic::Popf => {
                self.a.op2(Code::Movzx_r32_rm16, Register::R13D, gmem(Register::R12D, 0))?;
                self.esp_add(2)?;
                self.r13_to_flags(true)?;
                self.if_check(next)?;
                Ok(Flow::Continue)
            }
            Mnemonic::Leave => {
                if i.code() != Code::Leaved {
                    return self.sysop(at);
                }
                self.a.op2(Code::Mov_r32_rm32, Register::R13D, gmem(Register::EBP, 0))?;
                self.a.op2(
                    Code::Lea_r32_m,
                    Register::R12D,
                    MemoryOperand::with_base_displ(Register::RBP, 4),
                )?;
                self.a.op2(Code::Mov_r32_rm32, Register::EBP, Register::R13D)?;
                Ok(Flow::Continue)
            }
            Mnemonic::Enter => {
                if i.code() != Code::Enterd_imm16_imm8 {
                    return Err(Fail);
                }
                if i.immediate8_2nd() != 0 {
                    return self.sysop(at);
                }
                self.push_r(Register::EBP)?;
                self.a.op2(Code::Mov_r32_rm32, Register::EBP, Register::R12D)?;
                self.esp_add(-i32::from(i.immediate16()))?;
                Ok(Flow::Continue)
            }

            // ---- system ----
            Mnemonic::Hlt => {
                self.a.exit(reason::HLT, next, stubs.exit)?;
                Ok(Flow::End)
            }
            Mnemonic::Cli => {
                self.a.frame_set(Frame::VIF, 0)?;
                Ok(Flow::Continue)
            }
            Mnemonic::Sti => {
                self.a.frame_set(Frame::VIF, FLAG_IF)?;
                self.if_check(next)?;
                Ok(Flow::Continue)
            }
            Mnemonic::Ud0 | Mnemonic::Ud1 | Mnemonic::Ud2 => {
                self.a.exit(reason::INVALID, at, stubs.exit)?;
                Ok(Flow::End)
            }
            Mnemonic::In
            | Mnemonic::Out
            | Mnemonic::Insb
            | Mnemonic::Insw
            | Mnemonic::Insd
            | Mnemonic::Outsb
            | Mnemonic::Outsw
            | Mnemonic::Outsd
            | Mnemonic::Int
            | Mnemonic::Int3
            | Mnemonic::Into
            | Mnemonic::Int1
            | Mnemonic::Rdmsr
            | Mnemonic::Wrmsr
            | Mnemonic::Cpuid
            | Mnemonic::Rdtsc
            // What 64-bit mode has no encoding for.
            | Mnemonic::Aaa
            | Mnemonic::Aad
            | Mnemonic::Aam
            | Mnemonic::Aas
            | Mnemonic::Daa
            | Mnemonic::Das
            | Mnemonic::Bound
            | Mnemonic::Salc => self.sysop(at),
            // No caches or TLB to flush; a title's own GDT is not loaded
            // (the flat model's segments stay).
            Mnemonic::Wbinvd | Mnemonic::Invd | Mnemonic::Invlpg | Mnemonic::Lgdt => Ok(Flow::Continue),
            Mnemonic::Sidt => {
                let t = self.env.tables;
                self.store_table_reg(i, t.idt_limit, t.idt_base)?;
                Ok(Flow::Continue)
            }
            Mnemonic::Sgdt => {
                let t = self.env.tables;
                self.store_table_reg(i, t.gdt_limit, t.gdt_base)?;
                Ok(Flow::Continue)
            }
            Mnemonic::Smsw => {
                // The low word of CR0 as the Xbox kernel leaves it.
                match i.op_kind(0) {
                    OpKind::Register => {
                        let r = host_reg(i.op_register(0).full_register32());
                        self.a.op2(Code::Mov_r32_imm32, r, 0x3Bu32)?;
                    }
                    _ => {
                        let m = self.mem_operand(i, 0)?;
                        self.a.op2(Code::Mov_rm16_imm16, m, 0x3Bu32)?;
                    }
                }
                Ok(Flow::Continue)
            }
            Mnemonic::Lds | Mnemonic::Les => {
                // A far pointer loaded (C5 /r, C4 /r): the register takes
                // the offset; the segment register stays the flat model's.
                let r = host_reg(i.op_register(0));
                let m = self.mem_operand(i, 0)?;
                if r.is_gpr16() {
                    self.a.op2(Code::Mov_r16_rm16, r, m)?;
                } else {
                    self.a.op2(Code::Mov_r32_rm32, r, m)?;
                }
                Ok(Flow::Continue)
            }
            Mnemonic::Mov => {
                // Control and debug registers, segment registers.
                for k in 0..i.op_count() {
                    if i.op_kind(k) != OpKind::Register {
                        continue;
                    }
                    let r = i.op_register(k);
                    if r.is_cr() {
                        return self.sysop(at);
                    }
                    if r.is_dr() || r.is_tr() {
                        return Err(Fail);
                    }
                    if r.is_segment_register() {
                        if k == 0 {
                            // A segment load: the flat model's selectors stay.
                            return Ok(Flow::Continue);
                        }
                        let sel = selector(r);
                        match i.op_kind(0) {
                            OpKind::Register => {
                                let d = host_reg(i.op_register(0));
                                if d.is_gpr16() {
                                    self.a.op2(Code::Mov_r16_imm16, d, sel)?;
                                } else {
                                    self.a.op2(Code::Mov_r32_imm32, d, sel)?;
                                }
                            }
                            _ => {
                                let m = self.mem_operand(i, 0)?;
                                self.a.op2(Code::Mov_rm16_imm16, m, sel)?;
                            }
                        }
                        return Ok(Flow::Continue);
                    }
                }
                self.generic(i)?;
                Ok(Flow::Continue)
            }
            _ => {
                self.generic(i)?;
                Ok(Flow::Continue)
            }
        }
    }
}

/// Translates guest code at `eip` into host code for address `host`.
///
/// Returns an error only for a failure of the translator itself (a stub or
/// fixed sequence that does not assemble). Guest instructions without a
/// translation become exits that report them.
pub(crate) fn translate(
    env: &Env<'_>,
    eip: u32,
    host: u64,
    mode: Mode,
) -> Result<Translation, String> {
    let mut t = Translator {
        env,
        a: Asm::new(host),
        mode,
        map: Vec::new(),
        exits: Vec::new(),
        if_checks: Vec::new(),
        info: InstructionInfoFactory::new(),
    };
    let stubs = *env.stubs;
    let block = mode == Mode::Block;

    // The kick poll (flags and every register live): `kick` nonzero leaves
    // through the stub at the block's end.
    let mut kick_patch = None;
    if block {
        t.a.op2(Code::Mov_r64_rm64, Register::R10, Register::RCX)?;
        t.a.op2(Code::Mov_r32_rm32, Register::ECX, fr(Frame::KICK))?;
        let clear = t.a.jrcxz();
        kick_patch = Some(t.a.jmp(0));
        t.a.bind8(clear);
        t.a.op2(Code::Mov_r64_rm64, Register::RCX, Register::R10)?;
    }
    let entry_nopoll = t.a.len() as u32;

    let mut at = eip;
    let mut n = 0usize;
    // The guest bytes the block was made from end here: the instruction it
    // stops at without translating is one of them (were it not, a block of
    // only that at a page's first byte would be listed under no page).
    let mut dep_end = u64::from(eip) + 1;
    let mut probe = false;
    loop {
        if n > 0
            && (n >= MAX_INSNS
                || (n > 32 && (at ^ eip) & !0xFFF != 0 && at & 0xFFF == 0)
                || (at >= env.trap.0 && at < env.trap.1))
        {
            t.exit_to(0, at);
            break;
        }
        let (i, short) = decode(env.mem, at);
        if i.is_invalid() {
            if n > 0 {
                // Its own block says what is wrong with it, if it is reached.
                t.exit_to(0, at);
                break;
            }
            t.map.push(InsnMap { host_off: t.a.len() as u32, eip: at, borrow: 0 });
            if short {
                // The instruction's first missing byte, touched as the
                // guest's fetch would: a page fault at that address with EIP
                // here. Once the page is there the block is made again.
                let mut probe_buf = [0u8; 15];
                let have = env.mem.read(at, &mut probe_buf) as u32;
                let missing = at.wrapping_add(have);
                t.a.op2(
                    Code::Mov_r8_rm8,
                    Register::R11L,
                    MemoryOperand::new(
                        Register::None,
                        Register::None,
                        1,
                        i64::from(missing as i32),
                        4,
                        false,
                        Register::GS,
                    ),
                )?;
                t.a.exit(reason::LOOKUP, at, stubs.exit)?;
                probe = true;
            } else {
                t.a.exit(reason::INVALID, at, stubs.exit)?;
            }
            break;
        }
        dep_end = dep_end.max(u64::from(at) + i.len() as u64);
        let start = t.a.len();
        let (nx, ni) = (t.exits.len(), t.if_checks.len());
        t.map.push(InsnMap { host_off: start as u32, eip: at, borrow: 0 });
        match t.insn(&i, at) {
            Ok(flow) => {
                n += 1;
                at = at.wrapping_add(i.len() as u32);
                if matches!(flow, Flow::End) {
                    break;
                }
                if !block {
                    t.a.exit(reason::STEP, at, stubs.exit)?;
                    break;
                }
            }
            Err(Fail) => {
                t.a.truncate(start);
                t.exits.truncate(nx);
                t.if_checks.truncate(ni);
                if n > 0 {
                    t.map.pop();
                    t.exit_to(0, at);
                } else {
                    if let Some(m) = t.map.last_mut() {
                        m.borrow = 0;
                    }
                    t.a.exit(reason::UNTRANSLATED, at, stubs.exit)?;
                }
                break;
            }
        }
    }

    // After the code: the exits' unchained targets, the interrupt checks'
    // slow paths and the kick's way out.
    let mut exits = Vec::with_capacity(t.exits.len());
    for e in std::mem::take(&mut t.exits) {
        let stub = t.a.len();
        t.a.op2(Code::Mov_r32_imm32, Register::R13D, e.target)?;
        t.a.jmp(stubs.dispatch);
        t.a.patch(e.patch, host + stub as u64);
        exits.push(ExitSite {
            patch: e.patch as u32,
            stub: stub as u32,
            target: e.target,
            backward: e.backward,
        });
    }
    for c in std::mem::take(&mut t.if_checks) {
        let here = t.a.ip();
        t.a.patch(c.patch, here);
        t.a.op2(Code::Mov_r32_rm32, Register::ECX, fr(Frame::VIF))?;
        let off = t.a.jrcxz();
        t.a.op2(Code::Mov_r64_rm64, Register::RCX, Register::R10)?;
        t.a.exit(reason::KICK, c.next, stubs.exit)?;
        t.a.bind8(off);
        // `back` restores RCX itself.
        t.a.jmp(host + c.back as u64);
    }
    if let Some(p) = kick_patch {
        let here = t.a.ip();
        t.a.patch(p, here);
        t.a.op2(Code::Mov_r64_rm64, Register::RCX, Register::R10)?;
        t.a.exit(reason::KICK, eip, stubs.exit)?;
    }

    Ok(Translation { code: t.a.bytes().to_vec(), entry_nopoll, map: t.map, exits, dep_end, probe })
}

/// The guest address of the explicit memory operand of `i`, computed from
/// saved registers as the processor would (segment base included).
pub(crate) fn effective_address(i: &Instruction, gregs: &[u64; 8], fs_base: u32) -> u32 {
    let reg = |r: Register| -> u32 {
        if r == Register::None {
            return 0;
        }
        let full = r.full_register32();
        let n = full.number();
        let v = gregs[n & 7] as u32;
        if r.is_gpr16() { v & 0xFFFF } else { v }
    };
    let base = i.memory_base();
    let index = i.memory_index();
    let addr16 = base.is_gpr16()
        || index.is_gpr16()
        || (base == Register::None && index == Register::None && i.memory_displ_size() == 2);
    let scale = if index == Register::None { 0 } else { i.memory_index_scale() };
    let mut ea = reg(base)
        .wrapping_add(reg(index).wrapping_mul(scale))
        .wrapping_add(i.memory_displacement32());
    if addr16 {
        ea &= 0xFFFF;
    }
    if i.segment_prefix() == Register::FS {
        ea = ea.wrapping_add(fs_base);
    }
    ea
}
