//! Instructions compiled once, when their block is decoded, into a handler
//! and the operands it needs.
//!
//! Re-reading an instruction's operand kinds, registers and addressing on
//! every execution costs as much as the operation itself. A [`Uop`] keeps
//! them resolved: register numbers, the immediate, the parts of the memory
//! operand's address, and a handler written for that one form. The common
//! forms of title code (32-bit moves, arithmetic and logic, pushes and pops,
//! `LEA`, branches, calls and returns, zero and sign extension, shifts by a
//! constant) have their own handlers; every other instruction keeps the
//! general path ([`InterpCpu::step`]) through [`generic`]. A form with a
//! 16-bit address takes the general path too.
//!
//! The handlers follow the same rules as the general path: everything is
//! read before anything is written, memory stores come before register
//! updates, and the flags are left as the general path leaves them, so the
//! two are interchangeable for any instruction.

use iced_x86::{Code, Instruction, OpKind, Register};
use xb_cpu::Bus;

use crate::cpu::{Flow, InterpCpu, Step};
use crate::flags::{ARITH, Op, szp};

/// Runs one compiled instruction.
pub(crate) type Handler = fn(&mut InterpCpu, &Uop, &mut dyn Bus) -> Step;

/// No register: the operand is in memory (or absent).
pub(crate) const NONE: u8 = 8;

/// A memory operand's address: `disp + base + (index << shift)`, plus the FS
/// base for an FS override.
#[derive(Clone, Copy)]
pub(crate) struct Ea {
    pub(crate) base: u8,
    pub(crate) index: u8,
    pub(crate) shift: u8,
    pub(crate) fs: bool,
    pub(crate) disp: u32,
}

/// One instruction, compiled.
#[derive(Clone, Copy)]
pub(crate) struct Uop {
    pub(crate) h: Handler,
    /// Its address and the next instruction's.
    pub(crate) ip: u32,
    pub(crate) next: u32,
    /// The register operand (0-7), or for a branch its condition code.
    pub(crate) r: u8,
    /// The r/m operand's register (0-7), or [`NONE`] for memory.
    pub(crate) rm: u8,
    /// The immediate, or a branch's target.
    pub(crate) imm: u32,
    pub(crate) ea: Ea,
    /// Whether the form has its own handler, so the fields above are set.
    pub(crate) fast: bool,
    /// The decoded instruction, for the general path and error messages.
    pub(crate) ins: Instruction,
}

/// The general path.
fn generic(cpu: &mut InterpCpu, u: &Uop, bus: &mut dyn Bus) -> Step {
    cpu.step(&u.ins, bus)
}

/// A general register's number (0-7) if `r` is a 32-bit one.
fn r32(r: Register) -> Option<u8> {
    let n = r as u32;
    (37..=44).contains(&n).then(|| (n - 37) as u8)
}

/// A 16-bit general register's number (0-7) if `r` is one.
fn r16(r: Register) -> Option<u8> {
    let n = r as u32;
    (21..=28).contains(&n).then(|| (n - 21) as u8)
}

/// An 8-bit register's number in the x86 encoding (AL CL DL BL AH CH DH BH).
fn r8(r: Register) -> Option<u8> {
    let n = r as u32;
    (1..=8).contains(&n).then(|| (n - 1) as u8)
}

/// The memory operand's address parts, or `None` for 16-bit addressing or
/// a segment other than the flat ones and FS.
pub(crate) fn ea_of(ins: &Instruction) -> Option<Ea> {
    let reg = |r: Register| {
        if r == Register::None { Some(NONE) } else { r32(r) }
    };
    let base = reg(ins.memory_base())?;
    let index = reg(ins.memory_index())?;
    if base == NONE && index == NONE && ins.memory_displ_size() == 2 {
        return None;
    }
    let fs = match ins.memory_segment() {
        Register::FS => true,
        Register::DS | Register::SS | Register::ES | Register::CS => false,
        _ => return None,
    };
    Some(Ea {
        base,
        index,
        shift: ins.memory_index_scale().trailing_zeros() as u8,
        fs,
        disp: ins.memory_displacement32(),
    })
}

/// Compiles one decoded instruction.
pub(crate) fn compile(ins: &Instruction) -> Uop {
    let mut u = Uop {
        h: generic,
        ip: ins.ip32(),
        next: ins.next_ip32(),
        r: NONE,
        rm: NONE,
        imm: 0,
        ea: Ea { base: NONE, index: NONE, shift: 0, fs: false, disp: 0 },
        fast: false,
        ins: *ins,
    };
    if let Some(h) = pick(ins, &mut u) {
        u.h = h;
        u.fast = true;
    }
    u
}

/// Sets `u.rm` (and `u.ea`) from operand `i`, a register or memory. A
/// register operand is an 8-bit one if `byte`, else a 32-bit one or, for
/// the 16-bit sources of `MOVZX`/`MOVSX`, a 16-bit one.
fn rm_from(ins: &Instruction, i: u32, u: &mut Uop, byte: bool) -> Option<()> {
    match ins.op_kind(i) {
        OpKind::Register => {
            let r = ins.op_register(i);
            u.rm = if byte { r8(r)? } else { r32(r).or_else(|| r16(r))? };
        }
        OpKind::Memory => {
            u.rm = NONE;
            u.ea = ea_of(ins)?;
        }
        _ => return None,
    }
    Some(())
}

#[expect(clippy::too_many_lines, reason = "one table of forms")]
fn pick(ins: &Instruction, u: &mut Uop) -> Option<Handler> {
    use Code as C;
    let reg0 = |u: &mut Uop| -> Option<()> {
        u.r = r32(ins.op_register(0))?;
        Some(())
    };
    let reg1 = |u: &mut Uop| -> Option<()> {
        u.r = r32(ins.op_register(1))?;
        Some(())
    };
    Some(match ins.code() {
        // ---- moves ----
        C::Mov_r32_rm32 => {
            reg0(u)?;
            rm_from(ins, 1, u, false)?;
            mov_r_rm
        }
        C::Mov_rm32_r32 => {
            reg1(u)?;
            rm_from(ins, 0, u, false)?;
            mov_rm_r
        }
        C::Mov_r32_imm32 => {
            reg0(u)?;
            u.imm = ins.immediate32();
            mov_r_imm
        }
        C::Mov_rm32_imm32 => {
            rm_from(ins, 0, u, false)?;
            u.imm = ins.immediate32();
            mov_rm_imm
        }
        C::Mov_r8_rm8 => {
            u.r = r8(ins.op_register(0))?;
            rm_from(ins, 1, u, true)?;
            mov8_r_rm
        }
        C::Mov_rm8_r8 => {
            u.r = r8(ins.op_register(1))?;
            rm_from(ins, 0, u, true)?;
            mov8_rm_r
        }
        C::Mov_rm8_imm8 => {
            rm_from(ins, 0, u, true)?;
            u.imm = u32::from(ins.immediate8());
            mov8_rm_imm
        }
        C::Lea_r32_m => {
            reg0(u)?;
            u.ea = ea_of(ins)?;
            if u.ea.fs {
                return None;
            }
            lea
        }
        C::Movzx_r32_rm8 => {
            reg0(u)?;
            rm_from(ins, 1, u, true)?;
            movzx8
        }
        C::Movzx_r32_rm16 => {
            reg0(u)?;
            rm_from(ins, 1, u, false)?;
            movzx16
        }
        C::Movsx_r32_rm8 => {
            reg0(u)?;
            rm_from(ins, 1, u, true)?;
            movsx8
        }
        C::Movsx_r32_rm16 => {
            reg0(u)?;
            rm_from(ins, 1, u, false)?;
            movsx16
        }

        // ---- arithmetic and logic, 32 bits ----
        c @ (C::Add_r32_rm32
        | C::Or_r32_rm32
        | C::And_r32_rm32
        | C::Sub_r32_rm32
        | C::Xor_r32_rm32
        | C::Cmp_r32_rm32) => {
            reg0(u)?;
            rm_from(ins, 1, u, false)?;
            match c {
                C::Add_r32_rm32 => alu_r_rm::<ADD>,
                C::Or_r32_rm32 => alu_r_rm::<OR>,
                C::And_r32_rm32 => alu_r_rm::<AND>,
                C::Sub_r32_rm32 => alu_r_rm::<SUB>,
                C::Xor_r32_rm32 => alu_r_rm::<XOR>,
                _ => alu_r_rm::<CMP>,
            }
        }
        c @ (C::Add_rm32_r32
        | C::Or_rm32_r32
        | C::And_rm32_r32
        | C::Sub_rm32_r32
        | C::Xor_rm32_r32
        | C::Cmp_rm32_r32
        | C::Test_rm32_r32) => {
            reg1(u)?;
            rm_from(ins, 0, u, false)?;
            match c {
                C::Add_rm32_r32 => alu_rm_x::<ADD, false>,
                C::Or_rm32_r32 => alu_rm_x::<OR, false>,
                C::And_rm32_r32 => alu_rm_x::<AND, false>,
                C::Sub_rm32_r32 => alu_rm_x::<SUB, false>,
                C::Xor_rm32_r32 => alu_rm_x::<XOR, false>,
                C::Cmp_rm32_r32 => alu_rm_x::<CMP, false>,
                _ => alu_rm_x::<TEST, false>,
            }
        }
        c @ (C::Add_rm32_imm32
        | C::Or_rm32_imm32
        | C::And_rm32_imm32
        | C::Sub_rm32_imm32
        | C::Xor_rm32_imm32
        | C::Cmp_rm32_imm32
        | C::Test_rm32_imm32
        | C::Test_rm32_imm32_F7r1
        | C::Add_rm32_imm8
        | C::Or_rm32_imm8
        | C::And_rm32_imm8
        | C::Sub_rm32_imm8
        | C::Xor_rm32_imm8
        | C::Cmp_rm32_imm8
        | C::Add_EAX_imm32
        | C::Or_EAX_imm32
        | C::And_EAX_imm32
        | C::Sub_EAX_imm32
        | C::Xor_EAX_imm32
        | C::Cmp_EAX_imm32
        | C::Test_EAX_imm32) => {
            rm_from(ins, 0, u, false)?;
            u.imm = match ins.op_kind(1) {
                OpKind::Immediate8to32 => ins.immediate8to32() as u32,
                _ => ins.immediate32(),
            };
            match c {
                C::Add_rm32_imm32 | C::Add_rm32_imm8 | C::Add_EAX_imm32 => alu_rm_x::<ADD, true>,
                C::Or_rm32_imm32 | C::Or_rm32_imm8 | C::Or_EAX_imm32 => alu_rm_x::<OR, true>,
                C::And_rm32_imm32 | C::And_rm32_imm8 | C::And_EAX_imm32 => alu_rm_x::<AND, true>,
                C::Sub_rm32_imm32 | C::Sub_rm32_imm8 | C::Sub_EAX_imm32 => alu_rm_x::<SUB, true>,
                C::Xor_rm32_imm32 | C::Xor_rm32_imm8 | C::Xor_EAX_imm32 => alu_rm_x::<XOR, true>,
                C::Cmp_rm32_imm32 | C::Cmp_rm32_imm8 | C::Cmp_EAX_imm32 => alu_rm_x::<CMP, true>,
                _ => alu_rm_x::<TEST, true>,
            }
        }
        c @ (C::Cmp_rm8_imm8
        | C::Cmp_rm8_imm8_82
        | C::Test_rm8_imm8
        | C::Test_rm8_imm8_F6r1
        | C::Cmp_AL_imm8
        | C::Test_AL_imm8) => {
            rm_from(ins, 0, u, true)?;
            u.imm = u32::from(ins.immediate8());
            if matches!(c, C::Test_rm8_imm8 | C::Test_rm8_imm8_F6r1 | C::Test_AL_imm8) {
                cmp8_rm_imm::<TEST>
            } else {
                cmp8_rm_imm::<CMP>
            }
        }
        C::Inc_r32 | C::Inc_rm32 => {
            rm_from(ins, 0, u, false)?;
            incdec::<true>
        }
        C::Dec_r32 | C::Dec_rm32 => {
            rm_from(ins, 0, u, false)?;
            incdec::<false>
        }
        c @ (C::Shl_rm32_imm8
        | C::Shr_rm32_imm8
        | C::Sar_rm32_imm8
        | C::Shl_rm32_1
        | C::Shr_rm32_1
        | C::Sar_rm32_1) => {
            // A register operand only: the general path handles memory.
            u.rm = r32(ins.op_register(0)).filter(|_| ins.op_kind(0) == OpKind::Register)?;
            u.imm = if ins.op_count() > 1 && ins.op_kind(1) == OpKind::Immediate8 {
                u32::from(ins.immediate8()) & 31
            } else {
                1
            };
            if u.imm == 0 {
                return None;
            }
            match c {
                C::Shl_rm32_imm8 | C::Shl_rm32_1 => shift_r::<0>,
                C::Shr_rm32_imm8 | C::Shr_rm32_1 => shift_r::<1>,
                _ => shift_r::<2>,
            }
        }

        // ---- the stack ----
        C::Push_r32 => {
            reg0(u)?;
            push_r
        }
        C::Pushd_imm8 | C::Pushd_imm32 => {
            u.imm = match ins.op_kind(0) {
                OpKind::Immediate8to32 => ins.immediate8to32() as u32,
                OpKind::Immediate32 => ins.immediate32(),
                _ => return None,
            };
            push_imm
        }
        C::Push_rm32 => {
            rm_from(ins, 0, u, false)?;
            push_rm
        }
        C::Pop_r32 => {
            reg0(u)?;
            pop_r
        }

        // ---- control transfers ----
        C::Jmp_rel8_32 | C::Jmp_rel32_32 => {
            u.imm = ins.near_branch32();
            jmp
        }
        C::Call_rel32_32 => {
            u.imm = ins.near_branch32();
            call
        }
        C::Call_rm32 => {
            rm_from(ins, 0, u, false)?;
            call_rm
        }
        C::Jmp_rm32 => {
            rm_from(ins, 0, u, false)?;
            jmp_rm
        }
        C::Retnd => ret,
        C::Retnd_imm16 => {
            u.imm = u32::from(ins.immediate16());
            ret
        }
        _ if ins.op0_kind() == OpKind::NearBranch32
            && ins.condition_code() != iced_x86::ConditionCode::None
            && matches!(ins.mnemonic(), m if is_jcc(m)) =>
        {
            u.r = ins.condition_code() as u8 - 1;
            u.imm = ins.near_branch32();
            jcc
        }
        // The x87 forms with their own handlers are beside the general ones.
        _ => return crate::x87::pick(ins, u),
    })
}

pub(crate) fn is_jcc(m: iced_x86::Mnemonic) -> bool {
    use iced_x86::Mnemonic as M;
    matches!(
        m,
        M::Jo
            | M::Jno
            | M::Jb
            | M::Jae
            | M::Je
            | M::Jne
            | M::Jbe
            | M::Ja
            | M::Js
            | M::Jns
            | M::Jp
            | M::Jnp
            | M::Jl
            | M::Jge
            | M::Jle
            | M::Jg
    )
}

/// The arithmetic and logic operations with their own handlers.
pub(crate) const ADD: u8 = 0;
pub(crate) const OR: u8 = 1;
pub(crate) const AND: u8 = 4;
pub(crate) const SUB: u8 = 5;
pub(crate) const XOR: u8 = 6;
pub(crate) const CMP: u8 = 7;
pub(crate) const TEST: u8 = 8;

impl InterpCpu {
    /// The linear address of a compiled memory operand.
    #[inline(always)]
    pub(crate) fn addr(&self, e: &Ea) -> u32 {
        let mut a = e.disp;
        if e.base != NONE {
            a = a.wrapping_add(self.gpr[e.base as usize]);
        }
        if e.index != NONE {
            a = a.wrapping_add(self.gpr[e.index as usize] << e.shift);
        }
        if e.fs {
            a = a.wrapping_add(self.fs_base);
        }
        a
    }

    #[inline(always)]
    fn rm32(&mut self, u: &Uop, bus: &mut dyn Bus) -> Result<u32, crate::cpu::Stop> {
        if u.rm != NONE {
            Ok(self.gpr[u.rm as usize])
        } else {
            let a = self.addr(&u.ea);
            self.load(a, 32, bus)
        }
    }

    #[inline(always)]
    fn set_rm32(&mut self, u: &Uop, v: u32, bus: &mut dyn Bus) -> Result<(), crate::cpu::Stop> {
        if u.rm != NONE {
            self.gpr[u.rm as usize] = v;
            Ok(())
        } else {
            let a = self.addr(&u.ea);
            self.store(a, 32, v, bus)
        }
    }

    #[inline(always)]
    fn reg8(&self, n: u8) -> u32 {
        if n < 4 { self.gpr[n as usize] & 0xFF } else { (self.gpr[(n - 4) as usize] >> 8) & 0xFF }
    }

    #[inline(always)]
    fn set_reg8(&mut self, n: u8, v: u32) {
        if n < 4 {
            let g = &mut self.gpr[n as usize];
            *g = (*g & !0xFF) | (v & 0xFF);
        } else {
            let g = &mut self.gpr[(n - 4) as usize];
            *g = (*g & !0xFF00) | ((v & 0xFF) << 8);
        }
    }

    #[inline(always)]
    fn rm8(&mut self, u: &Uop, bus: &mut dyn Bus) -> Result<u32, crate::cpu::Stop> {
        if u.rm != NONE {
            Ok(self.reg8(u.rm))
        } else {
            let a = self.addr(&u.ea);
            self.load(a, 8, bus)
        }
    }

    #[inline(always)]
    fn rm16(&mut self, u: &Uop, bus: &mut dyn Bus) -> Result<u32, crate::cpu::Stop> {
        if u.rm != NONE {
            Ok(self.gpr[u.rm as usize] & 0xFFFF)
        } else {
            let a = self.addr(&u.ea);
            self.load(a, 16, bus)
        }
    }
}

fn mov_r_rm(c: &mut InterpCpu, u: &Uop, bus: &mut dyn Bus) -> Step {
    let v = c.rm32(u, bus)?;
    c.gpr[u.r as usize] = v;
    Ok(Flow::Next)
}

fn mov_rm_r(c: &mut InterpCpu, u: &Uop, bus: &mut dyn Bus) -> Step {
    let v = c.gpr[u.r as usize];
    c.set_rm32(u, v, bus)?;
    Ok(Flow::Next)
}

fn mov_r_imm(c: &mut InterpCpu, u: &Uop, _: &mut dyn Bus) -> Step {
    c.gpr[u.r as usize] = u.imm;
    Ok(Flow::Next)
}

fn mov_rm_imm(c: &mut InterpCpu, u: &Uop, bus: &mut dyn Bus) -> Step {
    c.set_rm32(u, u.imm, bus)?;
    Ok(Flow::Next)
}

fn mov8_r_rm(c: &mut InterpCpu, u: &Uop, bus: &mut dyn Bus) -> Step {
    let v = c.rm8(u, bus)?;
    c.set_reg8(u.r, v);
    Ok(Flow::Next)
}

fn mov8_rm_r(c: &mut InterpCpu, u: &Uop, bus: &mut dyn Bus) -> Step {
    let v = c.reg8(u.r);
    if u.rm != NONE {
        c.set_reg8(u.rm, v);
    } else {
        let a = c.addr(&u.ea);
        c.store(a, 8, v, bus)?;
    }
    Ok(Flow::Next)
}

fn mov8_rm_imm(c: &mut InterpCpu, u: &Uop, bus: &mut dyn Bus) -> Step {
    if u.rm != NONE {
        c.set_reg8(u.rm, u.imm);
    } else {
        let a = c.addr(&u.ea);
        c.store(a, 8, u.imm, bus)?;
    }
    Ok(Flow::Next)
}

fn lea(c: &mut InterpCpu, u: &Uop, _: &mut dyn Bus) -> Step {
    c.gpr[u.r as usize] = c.addr(&u.ea);
    Ok(Flow::Next)
}

fn movzx8(c: &mut InterpCpu, u: &Uop, bus: &mut dyn Bus) -> Step {
    let v = c.rm8(u, bus)?;
    c.gpr[u.r as usize] = v;
    Ok(Flow::Next)
}

fn movzx16(c: &mut InterpCpu, u: &Uop, bus: &mut dyn Bus) -> Step {
    let v = c.rm16(u, bus)?;
    c.gpr[u.r as usize] = v;
    Ok(Flow::Next)
}

fn movsx8(c: &mut InterpCpu, u: &Uop, bus: &mut dyn Bus) -> Step {
    let v = c.rm8(u, bus)?;
    c.gpr[u.r as usize] = v as u8 as i8 as i32 as u32;
    Ok(Flow::Next)
}

fn movsx16(c: &mut InterpCpu, u: &Uop, bus: &mut dyn Bus) -> Step {
    let v = c.rm16(u, bus)?;
    c.gpr[u.r as usize] = v as u16 as i16 as i32 as u32;
    Ok(Flow::Next)
}

#[inline(always)]
fn alu<const OP: u8>(c: &mut InterpCpu, a: u32, b: u32) -> u32 {
    let (r, kind) = match OP {
        ADD => (a.wrapping_add(b), Op::Add),
        SUB | CMP => (a.wrapping_sub(b), Op::Sub),
        AND | TEST => (a & b, Op::Logic),
        OR => (a | b, Op::Logic),
        _ => (a ^ b, Op::Logic),
    };
    c.flags.lazy(kind, 32, a, b, r);
    r
}

/// `r OP= rm`.
fn alu_r_rm<const OP: u8>(c: &mut InterpCpu, u: &Uop, bus: &mut dyn Bus) -> Step {
    let b = c.rm32(u, bus)?;
    let a = c.gpr[u.r as usize];
    let r = alu::<OP>(c, a, b);
    if OP != CMP {
        c.gpr[u.r as usize] = r;
    }
    Ok(Flow::Next)
}

/// `rm OP= r` or `rm OP= imm`.
fn alu_rm_x<const OP: u8, const IMM: bool>(c: &mut InterpCpu, u: &Uop, bus: &mut dyn Bus) -> Step {
    let b = if IMM { u.imm } else { c.gpr[u.r as usize] };
    if u.rm != NONE {
        let a = c.gpr[u.rm as usize];
        let r = alu::<OP>(c, a, b);
        if OP != CMP && OP != TEST {
            c.gpr[u.rm as usize] = r;
        }
        return Ok(Flow::Next);
    }
    let addr = c.addr(&u.ea);
    let a = c.load(addr, 32, bus)?;
    let saved = c.flags;
    let r = alu::<OP>(c, a, b);
    if OP != CMP
        && OP != TEST
        && let Err(e) = c.store(addr, 32, r, bus)
    {
        c.flags = saved;
        return Err(e);
    }
    Ok(Flow::Next)
}

/// `CMP`/`TEST` of an 8-bit operand with an immediate.
fn cmp8_rm_imm<const OP: u8>(c: &mut InterpCpu, u: &Uop, bus: &mut dyn Bus) -> Step {
    let a = c.rm8(u, bus)?;
    let b = u.imm & 0xFF;
    if OP == TEST {
        c.flags.lazy(Op::Logic, 8, a, b, a & b);
    } else {
        c.flags.lazy(Op::Sub, 8, a, b, a.wrapping_sub(b));
    }
    Ok(Flow::Next)
}

fn incdec<const INC: bool>(c: &mut InterpCpu, u: &Uop, bus: &mut dyn Bus) -> Step {
    let a = c.rm32(u, bus)?;
    let r = if INC { a.wrapping_add(1) } else { a.wrapping_sub(1) };
    c.set_rm32(u, r, bus)?;
    c.flags.lazy(if INC { Op::Inc } else { Op::Dec }, 32, a, 1, r);
    Ok(Flow::Next)
}

/// `SHL`, `SHR`, `SAR` of a register by a constant 1 to 31.
fn shift_r<const KIND: u8>(c: &mut InterpCpu, u: &Uop, _: &mut dyn Bus) -> Step {
    let a = c.gpr[u.rm as usize];
    let n = u.imm;
    let (r, cf, of) = match KIND {
        0 => {
            let r = a << n;
            let cf = (a >> (32 - n)) & 1;
            (r, cf, (r >> 31) ^ cf)
        }
        1 => (a >> n, (a >> (n - 1)) & 1, a >> 31),
        _ => (((a as i32) >> n) as u32, ((a as i32) >> (n - 1)) as u32 & 1, 0),
    };
    c.gpr[u.rm as usize] = r;
    let of = if of != 0 { crate::flags::OF } else { 0 };
    c.flags.set_arith(ARITH, szp(r, 32) | cf | of);
    Ok(Flow::Next)
}

fn push_r(c: &mut InterpCpu, u: &Uop, bus: &mut dyn Bus) -> Step {
    let v = c.gpr[u.r as usize];
    c.push(v, 32, bus)?;
    Ok(Flow::Next)
}

fn push_imm(c: &mut InterpCpu, u: &Uop, bus: &mut dyn Bus) -> Step {
    c.push(u.imm, 32, bus)?;
    Ok(Flow::Next)
}

fn push_rm(c: &mut InterpCpu, u: &Uop, bus: &mut dyn Bus) -> Step {
    let v = c.rm32(u, bus)?;
    c.push(v, 32, bus)?;
    Ok(Flow::Next)
}

fn pop_r(c: &mut InterpCpu, u: &Uop, bus: &mut dyn Bus) -> Step {
    let v = c.pop(32, bus)?;
    c.gpr[u.r as usize] = v;
    Ok(Flow::Next)
}

fn jmp(_: &mut InterpCpu, u: &Uop, _: &mut dyn Bus) -> Step {
    Ok(Flow::Jump(u.imm))
}

fn jcc(c: &mut InterpCpu, u: &Uop, _: &mut dyn Bus) -> Step {
    if c.flags.cond(u32::from(u.r)) { Ok(Flow::Jump(u.imm)) } else { Ok(Flow::Next) }
}

fn call(c: &mut InterpCpu, u: &Uop, bus: &mut dyn Bus) -> Step {
    c.push(u.next, 32, bus)?;
    Ok(Flow::Jump(u.imm))
}

fn call_rm(c: &mut InterpCpu, u: &Uop, bus: &mut dyn Bus) -> Step {
    let t = c.rm32(u, bus)?;
    c.push(u.next, 32, bus)?;
    Ok(Flow::Jump(t))
}

fn jmp_rm(c: &mut InterpCpu, u: &Uop, bus: &mut dyn Bus) -> Step {
    let t = c.rm32(u, bus)?;
    Ok(Flow::Jump(t))
}

fn ret(c: &mut InterpCpu, u: &Uop, bus: &mut dyn Bus) -> Step {
    let t = c.pop(32, bus)?;
    c.gpr[4] = c.gpr[4].wrapping_add(u.imm);
    Ok(Flow::Jump(t))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::flags::Flags;
    use xb_cpu::{Access, Mapper};
    use xb_mem::Ram;

    const DATA: u32 = 0x0010_0000;
    const STACK: u32 = 0x0020_0000;

    /// Encodings of every form with its own handler (and a few that keep
    /// the general path, which must then be the general path).
    pub(crate) const FORMS: &[&[u8]] = &[
        &[0x8B, 0x03],                               // mov eax,[ebx]
        &[0x89, 0x0B],                               // mov [ebx],ecx
        &[0x8B, 0xC1],                               // mov eax,ecx
        &[0x89, 0xC8],                               // mov eax,ecx (89 form)
        &[0xB8, 0x78, 0x56, 0x34, 0x12],             // mov eax,0x12345678
        &[0xC7, 0x43, 0x04, 0x11, 0x22, 0x33, 0x44], // mov dword [ebx+4],imm
        &[0x8A, 0x43, 0x01],                         // mov al,[ebx+1]
        &[0x8A, 0xE1],                               // mov ah,cl
        &[0x88, 0x63, 0x02],                         // mov [ebx+2],ah
        &[0x88, 0xFC],                               // mov ah,bh
        &[0xC6, 0x03, 0x7F],                         // mov byte [ebx],0x7f
        &[0xC6, 0xC5, 0x80],                         // mov ch,0x80
        &[0x8D, 0x44, 0x8B, 0x10],                   // lea eax,[ebx+ecx*4+0x10]
        &[0x8D, 0x04, 0x49],                         // lea eax,[ecx+ecx*2]
        &[0x0F, 0xB6, 0x03],                         // movzx eax,byte [ebx]
        &[0x0F, 0xB7, 0xC1],                         // movzx eax,cx
        &[0x0F, 0xBE, 0xC4],                         // movsx eax,ah
        &[0x0F, 0xBF, 0x03],                         // movsx eax,word [ebx]
        &[0x03, 0xC1],                               // add eax,ecx
        &[0x2B, 0x03],                               // sub eax,[ebx]
        &[0x31, 0x0B],                               // xor [ebx],ecx
        &[0x01, 0x4B, 0x08],                         // add [ebx+8],ecx
        &[0x29, 0xC8],                               // sub eax,ecx
        &[0x3B, 0xC1],                               // cmp eax,ecx
        &[0x39, 0x0B],                               // cmp [ebx],ecx
        &[0x85, 0xC9],                               // test ecx,ecx
        &[0x85, 0x03],                               // test [ebx],eax
        &[0x83, 0xC0, 0xFF],                         // add eax,-1
        &[0x81, 0xE9, 0x00, 0x00, 0x01, 0x00],       // sub ecx,0x10000
        &[0x83, 0x3B, 0x05],                         // cmp dword [ebx],5
        &[0x83, 0x23, 0xF0],                         // and dword [ebx],-16
        &[0x05, 0x01, 0x00, 0x00, 0x80],             // add eax,0x80000001
        &[0x25, 0xFF, 0x00, 0x00, 0x00],             // and eax,0xff
        &[0x0D, 0x00, 0x01, 0x00, 0x00],             // or eax,0x100
        &[0x35, 0xFF, 0xFF, 0xFF, 0xFF],             // xor eax,-1
        &[0x3D, 0x00, 0x00, 0x00, 0x80],             // cmp eax,0x80000000
        &[0xA9, 0x01, 0x00, 0x00, 0x00],             // test eax,1
        &[0xF7, 0xC1, 0x00, 0x80, 0x00, 0x00],       // test ecx,0x8000
        &[0x21, 0xC8],                               // and eax,ecx
        &[0x09, 0xC8],                               // or eax,ecx
        &[0x0B, 0x03],                               // or eax,[ebx]
        &[0x80, 0x3B, 0x41],                         // cmp byte [ebx],0x41
        &[0x3C, 0x7F],                               // cmp al,0x7f
        &[0xA8, 0x80],                               // test al,0x80
        &[0xF6, 0xC4, 0x01],                         // test ah,1
        &[0x40],                                     // inc eax
        &[0x49],                                     // dec ecx
        &[0xFF, 0x03],                               // inc dword [ebx]
        &[0xFF, 0x4B, 0x04],                         // dec dword [ebx+4]
        &[0xC1, 0xE0, 0x05],                         // shl eax,5
        &[0xC1, 0xE9, 0x1F],                         // shr ecx,31
        &[0xC1, 0xF8, 0x03],                         // sar eax,3
        &[0xD1, 0xE0],                               // shl eax,1
        &[0xD1, 0xF9],                               // sar ecx,1
        &[0xD1, 0xE8],                               // shr eax,1
        &[0x50],                                     // push eax
        &[0x6A, 0xFF],                               // push -1
        &[0x68, 0x44, 0x33, 0x22, 0x11],             // push 0x11223344
        &[0xFF, 0x33],                               // push dword [ebx]
        &[0x59],                                     // pop ecx
        &[0x5C],                                     // pop esp
        &[0x54],                                     // push esp
        &[0xEB, 0x10],                               // jmp short
        &[0xE9, 0x00, 0x01, 0x00, 0x00],             // jmp near
        &[0xE8, 0x00, 0x01, 0x00, 0x00],             // call near
        &[0xFF, 0xD1],                               // call ecx
        &[0xFF, 0x23],                               // jmp [ebx]
        &[0xFF, 0x14, 0x24],                         // call [esp]
        &[0xC3],                                     // ret
        &[0xC2, 0x08, 0x00],                         // ret 8
        &[0x74, 0x05],                               // je
        &[0x7C, 0xF0],                               // jl
        &[0x77, 0x05],                               // ja
        &[0x7A, 0x05],                               // jp
        &[0x70, 0x05],                               // jo
        &[0x0F, 0x8F, 0x00, 0x01, 0x00, 0x00],       // jg near
        &[0x64, 0x8B, 0x05, 0x20, 0x00, 0x00, 0x00], // mov eax,fs:[0x20]
        &[0xD9, 0x03], // fld dword [ebx]
        &[0xDD, 0x03], // fld qword [ebx]
        &[0xD9, 0xC2], // fld st(2)
        &[0xD9, 0x13], // fst dword [ebx]
        &[0xD9, 0x1B], // fstp dword [ebx]
        &[0xDD, 0x13], // fst qword [ebx]
        &[0xDD, 0x1B], // fstp qword [ebx]
        &[0xDD, 0xD2], // fst st(2)
        &[0xDD, 0xDA], // fstp st(2)
        &[0xD8, 0x03], // fadd dword [ebx]
        &[0xD8, 0x0B], // fmul dword [ebx]
        &[0xD8, 0x23], // fsub dword [ebx]
        &[0xD8, 0x2B], // fsubr dword [ebx]
        &[0xD8, 0x33], // fdiv dword [ebx]
        &[0xD8, 0x3B], // fdivr dword [ebx]
        &[0xDC, 0x03], // fadd qword [ebx]
        &[0xDC, 0x0B], // fmul qword [ebx]
        &[0xDC, 0x33], // fdiv qword [ebx]
        &[0xD8, 0xC1], // fadd st,st(1)
        &[0xD8, 0xCA], // fmul st,st(2)
        &[0xD8, 0xE1], // fsub st,st(1)
        &[0xD8, 0xE9], // fsubr st,st(1)
        &[0xD8, 0xF1], // fdiv st,st(1)
        &[0xD8, 0xF9], // fdivr st,st(1)
        &[0xDC, 0xC1], // fadd st(1),st
        &[0xDC, 0xC9], // fmul st(1),st
        &[0xDC, 0xE1], // fsubr st(1),st
        &[0xDC, 0xE9], // fsub st(1),st
        &[0xDC, 0xF1], // fdivr st(1),st
        &[0xDC, 0xF9], // fdiv st(1),st
        &[0xDE, 0xC1], // faddp st(1),st
        &[0xDE, 0xC9], // fmulp st(1),st
        &[0xDE, 0xE1], // fsubrp st(1),st
        &[0xDE, 0xE9], // fsubp st(1),st
        &[0xDE, 0xF1], // fdivrp st(1),st
        &[0xDE, 0xF9], // fdivp st(1),st
        &[0xD8, 0x13], // fcom dword [ebx]
        &[0xD8, 0x1B], // fcomp dword [ebx]
        &[0xDC, 0x13], // fcom qword [ebx]
        &[0xDC, 0x1B], // fcomp qword [ebx]
        &[0xD8, 0xD1], // fcom st(1)
        &[0xD8, 0xD9], // fcomp st(1)
        &[0xDD, 0xE1], // fucom st(1)
        &[0xDD, 0xE9], // fucomp st(1)
        &[0xDF, 0xE0], // fnstsw ax
        &[0xDB, 0x03], // fild dword [ebx]
        &[0xD9, 0xC9], // fxch st(1)
        &[0x13, 0xC1],                               // adc eax,ecx (general path)
        &[0x66, 0x8B, 0x03],                         // mov ax,[ebx] (general path)
    ];

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u32 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 33) as u32
        }
    }

    /// A random x87 state as an FXSAVE image: any top of stack, any set of
    /// empty registers, values from zeros to NaNs, double or single
    /// precision.
    pub(crate) fn random_fx(next: &mut dyn FnMut() -> u32) -> [u8; 512] {
        use crate::f80::F80;
        let mut fx = xb_cpu::FxState::new().0;
        if next() & 3 == 0 {
            // Single precision (PC = 0).
            fx[0..2].copy_from_slice(&0x007Fu16.to_le_bytes());
        }
        let top = (next() & 7) as u16;
        fx[2..4].copy_from_slice(&(top << 11).to_le_bytes());
        // Mostly full stacks; one round in four has empty registers.
        fx[4] = if next() & 3 == 0 { next() as u8 } else { 0xFF };
        for i in 0..8 {
            let v = match next() % 10 {
                0 => 0.0,
                1 => -0.0,
                2 => f64::NAN,
                3 => f64::INFINITY,
                4 => 1e300,
                5 => -1e-300,
                6 => f64::from(next() as i32),
                _ => f64::from(next() as i32) / f64::from((next() | 1) as i32),
            };
            fx[32 + 16 * i..42 + 16 * i].copy_from_slice(&F80::from_f64(v).to_bytes());
        }
        fx
    }

    fn machine(ram: &Ram) -> InterpCpu {
        let mut cpu = InterpCpu::new(ram.clone());
        cpu.map(DATA, DATA, 0x1000, Access::ReadWrite);
        cpu.map(STACK, STACK, 0x2000, Access::ReadWrite);
        cpu
    }

    #[test]
    fn every_compiled_form_leaves_the_state_the_general_path_leaves() {
        let mut rng = Rng(0x5EED);
        let ip = 0x0001_0000u32;
        for bytes in FORMS {
            let mut dec = iced_x86::Decoder::with_ip(32, bytes, u64::from(ip), 0);
            let ins = dec.decode();
            assert!(!ins.is_invalid(), "{bytes:02x?}");
            let u = compile(&ins);
            for _ in 0..200 {
                let mut gpr: [u32; 8] = std::array::from_fn(|_| rng.next());
                gpr[1] &= 0xFF;
                gpr[3] = DATA + (rng.next() & 0x7F0);
                gpr[4] = STACK + 0x1000 + (rng.next() & 0x3FC);
                let flags = 0x2 | (rng.next() & 0x8D5);
                let page: Vec<u8> = (0..0x1000).map(|_| rng.next() as u8).collect();
                let stack: Vec<u8> = (0..0x2000).map(|_| rng.next() as u8).collect();
                let fx = random_fx(&mut || rng.next());
                let run = |general: bool| {
                    let ram = Ram::new(4 << 20).unwrap();
                    ram.write(DATA, &page);
                    ram.write(STACK, &stack);
                    let mut cpu = machine(&ram);
                    cpu.gpr = gpr;
                    cpu.flags = Flags::new(flags);
                    cpu.fs_base = DATA;
                    cpu.set_fx_image(&fx);
                    let mut bus = xb_cpu_x64::testing::NoBus;
                    let flow = if general {
                        cpu.step(&u.ins, &mut bus)
                    } else {
                        (u.h)(&mut cpu, &u, &mut bus)
                    };
                    let flow = match flow {
                        Ok(Flow::Next) => "next".to_owned(),
                        Ok(Flow::Jump(t)) => format!("jump {t:#x}"),
                        Ok(_) => "other".to_owned(),
                        Err(e) => format!("{e:?}"),
                    };
                    (
                        flow,
                        cpu.gpr,
                        cpu.flags.get(),
                        ram.read_vec(DATA, 0x1000),
                        ram.read_vec(STACK, 0x2000),
                        cpu.fx_image().to_vec(),
                    )
                };
                let (want, got) = (run(true), run(false));
                assert!(
                    want == got,
                    "{bytes:02x?} from {gpr:08x?} flags {flags:#x}:\n general {:?} {:08x?} {:#x}\n compiled {:?} {:08x?} {:#x}",
                    want.0,
                    want.1,
                    want.2,
                    got.0,
                    got.1,
                    got.2
                );
            }
        }
    }

    #[test]
    fn the_common_forms_get_their_own_handler() {
        let generic_addr = generic as Handler as usize;
        for bytes in &FORMS[..FORMS.len() - 2] {
            let ins = iced_x86::Decoder::with_ip(32, bytes, 0x1_0000, 0).decode();
            assert_ne!(
                compile(&ins).h as usize,
                generic_addr,
                "{bytes:02x?} took the general path"
            );
        }
    }
}
