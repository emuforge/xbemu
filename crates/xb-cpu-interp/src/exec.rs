//! Integer and system instructions.
//!
//! Every instruction reads what it needs before it changes anything, and
//! makes its memory stores before its register updates where it can, so an
//! instruction that stops on a fault can be run again from its start (the
//! run loop restores the integer registers and flags from the snapshot it
//! took). String instructions advance the snapshot element by element.
//!
//! System instructions behave as the translator's do, which follows the
//! reference: `MOV r32, CRn` reads CR0 `0x8001003B`, CR2 the last page-fault
//! address, CR4 `0x610` and others 0; control register writes, `WBINVD`,
//! `INVD`, `INVLPG` and `LGDT` do nothing; `SIDT`/`SGDT` store zeros;
//! `SMSW` gives `0x3B`.

use iced_x86::{Instruction, Mnemonic as M, OpKind, Register};
use xb_cpu::{Bus, Exit};

use crate::cpu::{Flow, InterpCpu, Snap, Step, Stop};
use crate::flags::{AF, ARITH, CF, DF, IF, OF, Op, PF, POPF_MASK, SF, ZF, szp};

#[inline]
fn mask(bits: u32) -> u32 {
    if bits == 32 { u32::MAX } else { (1 << bits) - 1 }
}

#[inline]
fn msb(v: u32, bits: u32) -> u32 {
    (v >> (bits - 1)) & 1
}

#[inline]
fn sext(v: u32, bits: u32) -> i32 {
    let sh = 32 - bits;
    ((v << sh) as i32) >> sh
}

/// String instructions run this many elements between looks at the kick
/// flag.
const STRING_SLICE: u32 = 4096;

impl InterpCpu {
    /// Runs one instruction.
    pub(crate) fn step(&mut self, ins: &Instruction, bus: &mut dyn Bus) -> Step {
        let next = Ok(Flow::Next);
        match ins.mnemonic() {
            M::Nop
            | M::Pause
            | M::Prefetchnta
            | M::Prefetcht0
            | M::Prefetcht1
            | M::Prefetcht2
            | M::Prefetchw
            | M::Prefetch
            | M::Sfence
            | M::Lfence
            | M::Mfence
            | M::Wbinvd
            | M::Invd
            | M::Invlpg
            | M::Lgdt
            | M::Wait => next,

            M::Mov => self.mov(ins, bus),
            M::Movzx => {
                let sb = Self::op_bits(ins, 1);
                let v = self.rd(ins, 1, sb, bus)?;
                self.set_reg(ins.op_register(0), v & mask(sb));
                next
            }
            M::Movsx => {
                let sb = Self::op_bits(ins, 1);
                let v = self.rd(ins, 1, sb, bus)?;
                self.set_reg(ins.op_register(0), sext(v, sb) as u32);
                next
            }
            M::Lea => {
                let a = self.offset(ins);
                self.set_reg(ins.op_register(0), a);
                next
            }
            M::Xchg => {
                let bits = Self::op_bits(ins, 0);
                let a = self.rd(ins, 0, bits, bus)?;
                let b = self.rd(ins, 1, bits, bus)?;
                // The memory operand, if any, is written first.
                if ins.op_kind(1) == OpKind::Memory {
                    self.wr(ins, 1, bits, a, bus)?;
                    self.wr(ins, 0, bits, b, bus)?;
                } else {
                    self.wr(ins, 0, bits, b, bus)?;
                    self.wr(ins, 1, bits, a, bus)?;
                }
                next
            }

            M::Add => self.alu(ins, bus, Alu::Add),
            M::Or => self.alu(ins, bus, Alu::Or),
            M::Adc => self.alu(ins, bus, Alu::Adc),
            M::Sbb => self.alu(ins, bus, Alu::Sbb),
            M::And => self.alu(ins, bus, Alu::And),
            M::Sub => self.alu(ins, bus, Alu::Sub),
            M::Xor => self.alu(ins, bus, Alu::Xor),
            M::Cmp => self.alu(ins, bus, Alu::Cmp),
            M::Test => self.alu(ins, bus, Alu::Test),
            M::Inc | M::Dec => {
                let bits = Self::op_bits(ins, 0);
                let a = self.rd(ins, 0, bits, bus)?;
                let (op, r) = if ins.mnemonic() == M::Inc {
                    (Op::Inc, a.wrapping_add(1))
                } else {
                    (Op::Dec, a.wrapping_sub(1))
                };
                self.wr(ins, 0, bits, r, bus)?;
                self.flags.lazy(op, bits, a, 1, r);
                next
            }
            M::Neg => {
                let bits = Self::op_bits(ins, 0);
                let a = self.rd(ins, 0, bits, bus)? & mask(bits);
                let r = 0u32.wrapping_sub(a);
                self.wr(ins, 0, bits, r, bus)?;
                self.flags.lazy(Op::Sub, bits, 0, a, r);
                next
            }
            M::Not => {
                let bits = Self::op_bits(ins, 0);
                let a = self.rd(ins, 0, bits, bus)?;
                self.wr(ins, 0, bits, !a, bus)?;
                next
            }
            M::Shl | M::Sal | M::Shr | M::Sar | M::Rol | M::Ror | M::Rcl | M::Rcr => {
                self.shift(ins, bus)
            }
            M::Shld | M::Shrd => self.double_shift(ins, bus),
            M::Mul => self.mul(ins, bus, false),
            M::Imul => self.imul(ins, bus),
            M::Div => self.div(ins, bus, false),
            M::Idiv => self.div(ins, bus, true),

            M::Bt | M::Bts | M::Btr | M::Btc => self.bit_test(ins, bus),
            M::Bsf | M::Bsr => {
                let bits = Self::op_bits(ins, 0);
                let v = self.rd(ins, 1, bits, bus)? & mask(bits);
                if v == 0 {
                    self.flags.set_arith(ZF, ZF);
                } else {
                    let idx = if ins.mnemonic() == M::Bsf {
                        v.trailing_zeros()
                    } else {
                        31 - v.leading_zeros()
                    };
                    self.set_reg(ins.op_register(0), idx);
                    self.flags.set_arith(ZF, 0);
                }
                next
            }
            M::Bswap => {
                let r = ins.op_register(0);
                let v = self.reg(r);
                self.set_reg(r, v.swap_bytes());
                next
            }
            M::Xadd => {
                let bits = Self::op_bits(ins, 0);
                let a = self.rd(ins, 0, bits, bus)?;
                let b = self.rd(ins, 1, bits, bus)?;
                let r = a.wrapping_add(b);
                self.wr(ins, 0, bits, r, bus)?;
                self.set_reg(ins.op_register(1), a);
                self.flags.lazy(Op::Add, bits, a, b, r);
                next
            }
            M::Cmpxchg => {
                let bits = Self::op_bits(ins, 0);
                let d = self.rd(ins, 0, bits, bus)?;
                let s = self.rd(ins, 1, bits, bus)?;
                let acc = self.gpr[0] & mask(bits);
                let d = d & mask(bits);
                if acc == d {
                    self.wr(ins, 0, bits, s, bus)?;
                } else {
                    // The destination is written back either way.
                    if ins.op_kind(0) == OpKind::Memory {
                        self.wr(ins, 0, bits, d, bus)?;
                    }
                    let acc_reg = match bits {
                        8 => Register::AL,
                        16 => Register::AX,
                        _ => Register::EAX,
                    };
                    self.set_reg(acc_reg, d);
                }
                self.flags.lazy(Op::Sub, bits, acc, d, acc.wrapping_sub(d));
                next
            }
            M::Cmpxchg8b => {
                let a = self.ea(ins);
                let m = self.load64(a, bus)?;
                let want = u64::from(self.gpr[2]) << 32 | u64::from(self.gpr[0]);
                if m == want {
                    let new = u64::from(self.gpr[1]) << 32 | u64::from(self.gpr[3]);
                    self.store64(a, new, bus)?;
                    self.flags.set_arith(ZF, ZF);
                } else {
                    self.store64(a, m, bus)?;
                    self.gpr[0] = m as u32;
                    self.gpr[2] = (m >> 32) as u32;
                    self.flags.set_arith(ZF, 0);
                }
                next
            }

            M::Cbw => {
                let v = sext(self.gpr[0] & 0xFF, 8) as u32;
                self.set_reg(Register::AX, v);
                next
            }
            M::Cwde => {
                self.gpr[0] = sext(self.gpr[0] & 0xFFFF, 16) as u32;
                next
            }
            M::Cwd => {
                let v = if self.gpr[0] & 0x8000 != 0 { 0xFFFF } else { 0 };
                self.set_reg(Register::DX, v);
                next
            }
            M::Cdq => {
                self.gpr[2] = if self.gpr[0] & 0x8000_0000 != 0 { u32::MAX } else { 0 };
                next
            }

            M::Push => self.push_op(ins, bus),
            M::Pop => self.pop_op(ins, bus),
            M::Pushad | M::Pusha => {
                let bits = if ins.mnemonic() == M::Pushad { 32 } else { 16 };
                let n = bits / 8;
                let sp = self.gpr[4];
                for (i, r) in (0..8).enumerate() {
                    let v = if r == 4 { sp } else { self.gpr[r] };
                    let at = sp.wrapping_sub(n * (i as u32 + 1));
                    self.store(at, bits, v, bus)?;
                }
                self.gpr[4] = sp.wrapping_sub(8 * n);
                next
            }
            M::Popad | M::Popa => {
                let bits = if ins.mnemonic() == M::Popad { 32 } else { 16 };
                let n = bits / 8;
                let sp = self.gpr[4];
                let mut vals = [0u32; 8];
                for (i, v) in vals.iter_mut().enumerate() {
                    *v = self.load(sp.wrapping_add(n * (7 - i as u32)), bits, bus)?;
                }
                for (r, v) in vals.into_iter().enumerate() {
                    if r == 4 {
                        continue;
                    }
                    if bits == 32 {
                        self.gpr[r] = v;
                    } else {
                        self.gpr[r] = (self.gpr[r] & 0xFFFF_0000) | v;
                    }
                }
                self.gpr[4] = sp.wrapping_add(8 * n);
                next
            }
            M::Pushfd | M::Pushf => {
                let bits = if ins.mnemonic() == M::Pushfd { 32 } else { 16 };
                // VM and RF read as 0 in the image.
                let f = self.flags.get() & !0x3_0000;
                self.push(f, bits, bus)?;
                next
            }
            M::Popfd | M::Popf => {
                let bits = if ins.mnemonic() == M::Popfd { 32 } else { 16 };
                let v = self.pop(bits, bus)?;
                let m = POPF_MASK & mask(bits);
                let f = (self.flags.get() & !m) | (v & m) | 2;
                self.flags.set(f);
                Ok(Flow::End)
            }
            M::Lahf => {
                let f = self.flags.get() & (SF | ZF | AF | PF | CF) | 2;
                self.set_reg(Register::AH, f);
                next
            }
            M::Sahf => {
                let ah = self.reg(Register::AH);
                self.flags.set_arith(SF | ZF | AF | PF | CF, ah);
                next
            }
            M::Clc => {
                self.flags.set_arith(CF, 0);
                next
            }
            M::Stc => {
                self.flags.set_arith(CF, CF);
                next
            }
            M::Cmc => {
                let c = self.flags.cf() ^ 1;
                self.flags.set_arith(CF, c);
                next
            }
            M::Cld => {
                self.flags.bits &= !DF;
                next
            }
            M::Std => {
                self.flags.bits |= DF;
                next
            }
            M::Cli => {
                self.flags.bits &= !IF;
                next
            }
            M::Sti => {
                self.flags.bits |= IF;
                Ok(Flow::End)
            }

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
            | M::Setg => {
                let c = self.cond(ins);
                self.wr(ins, 0, 8, u32::from(c), bus)?;
                next
            }
            M::Cmovo
            | M::Cmovno
            | M::Cmovb
            | M::Cmovae
            | M::Cmove
            | M::Cmovne
            | M::Cmovbe
            | M::Cmova
            | M::Cmovs
            | M::Cmovns
            | M::Cmovp
            | M::Cmovnp
            | M::Cmovl
            | M::Cmovge
            | M::Cmovle
            | M::Cmovg => {
                let bits = Self::op_bits(ins, 0);
                // The source is read whether or not the move happens.
                let v = self.rd(ins, 1, bits, bus)?;
                if self.cond(ins) {
                    self.set_reg(ins.op_register(0), v);
                }
                next
            }
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
            | M::Jg => {
                if self.cond(ins) {
                    Ok(Flow::Jump(Self::branch(ins)))
                } else {
                    next
                }
            }
            M::Jmp => match ins.op0_kind() {
                OpKind::NearBranch32 | OpKind::NearBranch16 => Ok(Flow::Jump(Self::branch(ins))),
                OpKind::Register | OpKind::Memory => {
                    let bits = Self::op_bits(ins, 0);
                    let t = self.rd(ins, 0, bits, bus)?;
                    Ok(Flow::Jump(t))
                }
                _ => Err(self.unimplemented(ins)),
            },
            M::Call => {
                let t = match ins.op0_kind() {
                    OpKind::NearBranch32 | OpKind::NearBranch16 => Self::branch(ins),
                    OpKind::Register | OpKind::Memory => {
                        let bits = Self::op_bits(ins, 0);
                        self.rd(ins, 0, bits, bus)?
                    }
                    _ => return Err(self.unimplemented(ins)),
                };
                self.push(ins.next_ip32(), 32, bus)?;
                Ok(Flow::Jump(t))
            }
            M::Ret => {
                let t = self.pop(32, bus)?;
                if ins.op_count() == 1 {
                    self.gpr[4] = self.gpr[4].wrapping_add(u32::from(ins.immediate16()));
                }
                Ok(Flow::Jump(t))
            }
            M::Loop | M::Loope | M::Loopne => {
                let cx16 =
                    ins.op0_kind() == OpKind::NearBranch16 || ins.op_register(1) == Register::CX;
                let c = if cx16 {
                    let c = (self.gpr[1] & 0xFFFF).wrapping_sub(1) & 0xFFFF;
                    self.gpr[1] = (self.gpr[1] & 0xFFFF_0000) | c;
                    c
                } else {
                    self.gpr[1] = self.gpr[1].wrapping_sub(1);
                    self.gpr[1]
                };
                let take = c != 0
                    && match ins.mnemonic() {
                        M::Loope => self.flags.zf(),
                        M::Loopne => !self.flags.zf(),
                        _ => true,
                    };
                if take { Ok(Flow::Jump(Self::branch(ins))) } else { next }
            }
            M::Jecxz => {
                if self.gpr[1] == 0 {
                    Ok(Flow::Jump(Self::branch(ins)))
                } else {
                    next
                }
            }
            M::Jcxz => {
                if self.gpr[1] & 0xFFFF == 0 {
                    Ok(Flow::Jump(Self::branch(ins)))
                } else {
                    next
                }
            }
            M::Enter => self.enter(ins, bus),
            M::Leave => {
                let bits = if ins.code() == iced_x86::Code::Leavew { 16 } else { 32 };
                let bp = self.gpr[5];
                let v = self.load(bp, bits, bus)?;
                self.gpr[4] = bp.wrapping_add(bits / 8);
                if bits == 32 {
                    self.gpr[5] = v;
                } else {
                    self.gpr[5] = (self.gpr[5] & 0xFFFF_0000) | v;
                }
                next
            }
            M::Xlatb => {
                let mut a = self.gpr[3].wrapping_add(self.gpr[0] & 0xFF);
                if ins.segment_prefix() == Register::FS {
                    a = a.wrapping_add(self.fs_base);
                }
                let v = self.load(a, 8, bus)?;
                self.set_reg(Register::AL, v);
                next
            }

            M::Hlt => Ok(Flow::Exit(Exit::Halt)),
            M::Int => Ok(Flow::Exit(Exit::SoftwareInterrupt(ins.immediate8()))),
            M::Int3 => Ok(Flow::Exit(Exit::Exception { vector: 3, error_code: 0, address: 0 })),
            M::Int1 => Ok(Flow::Exit(Exit::Exception { vector: 1, error_code: 0, address: 0 })),
            M::Into => {
                if self.flags.get() & OF != 0 {
                    Ok(Flow::Exit(Exit::Exception { vector: 4, error_code: 0, address: 0 }))
                } else {
                    next
                }
            }
            M::Iretd => {
                let sp = self.gpr[4];
                let eip = self.load(sp, 32, bus)?;
                let cs = self.load(sp.wrapping_add(4), 32, bus)?;
                let fl = self.load(sp.wrapping_add(8), 32, bus)?;
                if cs & 3 != 0 {
                    return Err(Stop::Fatal(format!(
                        "interp: IRETD to ring {} at {:#010x}",
                        cs & 3,
                        ins.ip32()
                    )));
                }
                self.gpr[4] = sp.wrapping_add(12);
                self.flags.set((fl & POPF_MASK) | 2);
                Ok(Flow::JumpEnd(eip))
            }
            M::Ud2 | M::Ud0 | M::Ud1 => Err(Stop::Exc(6, 0)),
            M::Cpuid => {
                let r = xb_cpu::cpuid(self.gpr[0]);
                self.gpr[0] = r[0];
                self.gpr[3] = r[1];
                self.gpr[1] = r[2];
                self.gpr[2] = r[3];
                next
            }
            M::Rdtsc => {
                let t = self.outside(bus, |b| b.tsc());
                self.gpr[0] = t as u32;
                self.gpr[2] = (t >> 32) as u32;
                next
            }
            M::Rdmsr => {
                let msr = self.gpr[1];
                let v = self.outside(bus, |b| b.rdmsr(msr));
                self.gpr[0] = v as u32;
                self.gpr[2] = (v >> 32) as u32;
                next
            }
            M::Wrmsr => {
                let msr = self.gpr[1];
                let v = u64::from(self.gpr[2]) << 32 | u64::from(self.gpr[0]);
                self.outside(bus, |b| b.wrmsr(msr, v));
                next
            }
            M::In => {
                let bits = Self::op_bits(ins, 0);
                let port = self.port(ins, 1);
                let v = self.outside(bus, |b| b.io_read(port, (bits / 8) as u8));
                self.set_reg(ins.op_register(0), v);
                next
            }
            M::Out => {
                let bits = Self::op_bits(ins, 1);
                let port = self.port(ins, 0);
                let v = self.reg(ins.op_register(1));
                self.outside(bus, |b| b.io_write(port, (bits / 8) as u8, v));
                next
            }
            M::Bound => {
                let bits = Self::op_bits(ins, 0);
                let idx = sext(self.reg(ins.op_register(0)), bits);
                let a = self.ea(ins);
                let lo = sext(self.load(a, bits, bus)?, bits);
                let hi = sext(self.load(a.wrapping_add(bits / 8), bits, bus)?, bits);
                if idx < lo || idx > hi { Err(Stop::Exc(5, 0)) } else { next }
            }
            M::Daa | M::Das | M::Aaa | M::Aas | M::Aam | M::Aad | M::Salc => self.bcd(ins),
            M::Sidt | M::Sgdt => {
                let a = self.ea(ins);
                self.mem.write(a, &[0; 6], bus)?;
                next
            }
            M::Smsw => {
                let bits = Self::op_bits(ins, 0);
                self.wr(ins, 0, bits.min(16), 0x3B, bus)?;
                next
            }

            M::Movsb | M::Movsw | M::Movsd if ins.op0_kind() != OpKind::Register => {
                self.string(ins, bus)
            }
            M::Cmpsb | M::Cmpsw | M::Cmpsd if ins.op0_kind() != OpKind::Register => {
                self.string(ins, bus)
            }
            M::Stosb
            | M::Stosw
            | M::Stosd
            | M::Lodsb
            | M::Lodsw
            | M::Lodsd
            | M::Scasb
            | M::Scasw
            | M::Scasd
            | M::Insb
            | M::Insw
            | M::Insd
            | M::Outsb
            | M::Outsw
            | M::Outsd => self.string(ins, bus),

            _ => self.step_fpu(ins, bus),
        }
    }

    /// The offset part of a memory operand: what `LEA` computes (no segment
    /// base).
    fn offset(&self, ins: &Instruction) -> u32 {
        let a = self.ea(ins);
        if ins.memory_segment() == Register::FS { a.wrapping_sub(self.fs_base) } else { a }
    }

    #[inline]
    fn cond(&self, ins: &Instruction) -> bool {
        self.flags.cond(ins.condition_code() as u32 - 1)
    }

    #[inline]
    fn branch(ins: &Instruction) -> u32 {
        match ins.op0_kind() {
            OpKind::NearBranch16 => u32::from(ins.near_branch16()),
            _ => ins.near_branch32(),
        }
    }

    fn port(&self, ins: &Instruction, i: u32) -> u16 {
        match ins.op_kind(i) {
            OpKind::Immediate8 => u16::from(ins.immediate8()),
            _ => self.gpr[2] as u16,
        }
    }

    fn mov(&mut self, ins: &Instruction, bus: &mut dyn Bus) -> Step {
        let r0 = ins.op_register(0) as u32;
        let r1 = ins.op_register(1) as u32;
        // Control and debug registers.
        if ins.op_kind(0) == OpKind::Register && (185..=200).contains(&r0) {
            return Ok(Flow::Next);
        }
        if ins.op_kind(1) == OpKind::Register && (185..=200).contains(&r1) {
            let v = match r1 - 185 {
                0 => 0x8001_003B,
                2 => self.cr2,
                4 => 0x610,
                _ => 0,
            };
            self.set_reg(ins.op_register(0), v);
            return Ok(Flow::Next);
        }
        if (201..=216).contains(&r0) || (201..=216).contains(&r1) {
            return Err(self.unimplemented(ins));
        }
        // Segment registers: loads are ignored, stores give the flat
        // model's selectors.
        if ins.op_kind(0) == OpKind::Register && (71..=76).contains(&r0) {
            return Ok(Flow::Next);
        }
        if ins.op_kind(1) == OpKind::Register && (71..=76).contains(&r1) {
            let v = self.reg(ins.op_register(1));
            let bits =
                Self::op_bits(ins, 0).min(if ins.op_kind(0) == OpKind::Memory { 16 } else { 32 });
            self.wr(ins, 0, bits, v, bus)?;
            return Ok(Flow::Next);
        }
        let bits = Self::op_bits(ins, 0);
        let v = self.rd(ins, 1, bits, bus)?;
        self.wr(ins, 0, bits, v, bus)?;
        Ok(Flow::Next)
    }

    fn alu(&mut self, ins: &Instruction, bus: &mut dyn Bus, op: Alu) -> Step {
        let bits = Self::op_bits(ins, 0);
        let m = mask(bits);
        let a = self.rd(ins, 0, bits, bus)? & m;
        let b = self.rd(ins, 1, bits, bus)? & m;
        let r = match op {
            Alu::Add => a.wrapping_add(b),
            Alu::Sub | Alu::Cmp => a.wrapping_sub(b),
            Alu::And | Alu::Test => a & b,
            Alu::Or => a | b,
            Alu::Xor => a ^ b,
            Alu::Adc => {
                let c = self.flags.cf();
                let wide = u64::from(a) + u64::from(b) + u64::from(c);
                let r = wide as u32 & m;
                self.wr(ins, 0, bits, r, bus)?;
                let mut f = szp(r, bits) | ((a ^ b ^ r) & AF);
                if wide > u64::from(m) {
                    f |= CF;
                }
                if (a ^ r) & (b ^ r) & (1 << (bits - 1)) != 0 {
                    f |= OF;
                }
                self.flags.set_arith(ARITH, f);
                return Ok(Flow::Next);
            }
            Alu::Sbb => {
                let c = self.flags.cf();
                let r = a.wrapping_sub(b).wrapping_sub(c) & m;
                self.wr(ins, 0, bits, r, bus)?;
                let mut f = szp(r, bits) | ((a ^ b ^ r) & AF);
                if u64::from(a) < u64::from(b) + u64::from(c) {
                    f |= CF;
                }
                if (a ^ b) & (a ^ r) & (1 << (bits - 1)) != 0 {
                    f |= OF;
                }
                self.flags.set_arith(ARITH, f);
                return Ok(Flow::Next);
            }
        };
        if !matches!(op, Alu::Cmp | Alu::Test) {
            self.wr(ins, 0, bits, r, bus)?;
        }
        let kind = match op {
            Alu::Add => Op::Add,
            Alu::Sub | Alu::Cmp => Op::Sub,
            _ => Op::Logic,
        };
        self.flags.lazy(kind, bits, a, b, r);
        Ok(Flow::Next)
    }

    fn shift(&mut self, ins: &Instruction, bus: &mut dyn Bus) -> Step {
        let bits = Self::op_bits(ins, 0);
        let m = mask(bits);
        let a = self.rd(ins, 0, bits, bus)? & m;
        let count = if ins.op_count() > 1 { self.rd(ins, 1, 8, bus)? & 0x1F } else { 1 };
        if count == 0 {
            return Ok(Flow::Next);
        }
        let cf_in = self.flags.cf();
        let (r, cf, of) = match ins.mnemonic() {
            M::Shl | M::Sal => {
                let r = if count >= 32 { 0 } else { (a << count) & m };
                let cf = if count > bits { 0 } else { (a >> (bits - count)) & 1 };
                (r, cf, msb(r, bits) ^ cf)
            }
            M::Shr => {
                let r = if count >= 32 { 0 } else { a >> count };
                let cf = if count > bits { 0 } else { (a >> (count - 1)) & 1 };
                (r, cf, msb(a, bits))
            }
            M::Sar => {
                let s = sext(a, bits);
                let r = (s >> count.min(31)) as u32 & m;
                let cf = ((s >> (count - 1).min(31)) & 1) as u32;
                (r, cf, 0)
            }
            M::Rol => {
                let c = count % bits;
                let r = if c == 0 { a } else { ((a << c) | (a >> (bits - c))) & m };
                let cf = r & 1;
                (r, cf, msb(r, bits) ^ cf)
            }
            M::Ror => {
                let c = count % bits;
                let r = if c == 0 { a } else { ((a >> c) | (a << (bits - c))) & m };
                let cf = msb(r, bits);
                (r, cf, msb(r, bits) ^ ((r >> (bits - 2)) & 1))
            }
            M::Rcl => {
                let c = count % (bits + 1);
                let (mut r, mut cf) = (a, cf_in);
                for _ in 0..c {
                    let out = msb(r, bits);
                    r = ((r << 1) | cf) & m;
                    cf = out;
                }
                (r, cf, msb(r, bits) ^ cf)
            }
            _ => {
                let c = count % (bits + 1);
                let (mut r, mut cf) = (a, cf_in);
                let of = msb(a, bits) ^ cf_in;
                for _ in 0..c {
                    let out = r & 1;
                    r = (r >> 1) | (cf << (bits - 1));
                    cf = out;
                }
                (r, cf, of)
            }
        };
        self.wr(ins, 0, bits, r, bus)?;
        let of = if of != 0 { OF } else { 0 };
        match ins.mnemonic() {
            M::Rol | M::Ror | M::Rcl | M::Rcr => self.flags.set_arith(CF | OF, cf | of),
            _ => self.flags.set_arith(ARITH, szp(r, bits) | cf | of),
        }
        Ok(Flow::Next)
    }

    fn double_shift(&mut self, ins: &Instruction, bus: &mut dyn Bus) -> Step {
        let bits = Self::op_bits(ins, 0);
        let m = mask(bits);
        let d = self.rd(ins, 0, bits, bus)? & m;
        let s = self.reg(ins.op_register(1)) & m;
        let count = if ins.op_kind(2) == OpKind::Immediate8 {
            u32::from(ins.immediate8())
        } else {
            self.gpr[1] & 0xFF
        } & 0x1F;
        if count == 0 {
            return Ok(Flow::Next);
        }
        let wide = bits * 2;
        let (r, cf) = if ins.mnemonic() == M::Shld {
            let v = (u64::from(d) << bits) | u64::from(s);
            let v = v << count;
            let r = (v >> bits) as u32 & m;
            let cf = ((u64::from(d) << bits | u64::from(s)) >> (wide - count)) as u32 & 1;
            (r, cf)
        } else {
            let v = (u64::from(s) << bits) | u64::from(d);
            let r = (v >> count) as u32 & m;
            let cf = (v >> (count - 1)) as u32 & 1;
            (r, cf)
        };
        self.wr(ins, 0, bits, r, bus)?;
        let of = if msb(r, bits) != msb(d, bits) { OF } else { 0 };
        self.flags.set_arith(ARITH, szp(r, bits) | cf | of);
        Ok(Flow::Next)
    }

    fn mul(&mut self, ins: &Instruction, bus: &mut dyn Bus, _signed: bool) -> Step {
        let bits = Self::op_bits(ins, 0);
        let s = self.rd(ins, 0, bits, bus)? & mask(bits);
        let over = match bits {
            8 => {
                let r = (self.gpr[0] & 0xFF) * s;
                self.set_reg(Register::AX, r);
                r > 0xFF
            }
            16 => {
                let r = (self.gpr[0] & 0xFFFF) * s;
                self.set_reg(Register::AX, r);
                self.set_reg(Register::DX, r >> 16);
                r > 0xFFFF
            }
            _ => {
                let r = u64::from(self.gpr[0]) * u64::from(s);
                self.gpr[0] = r as u32;
                self.gpr[2] = (r >> 32) as u32;
                r > u64::from(u32::MAX)
            }
        };
        let f = if over { CF | OF } else { 0 };
        let low = self.gpr[0] & mask(bits);
        self.flags.set_arith(ARITH, f | szp(low, bits));
        Ok(Flow::Next)
    }

    fn imul(&mut self, ins: &Instruction, bus: &mut dyn Bus) -> Step {
        let bits = Self::op_bits(ins, 0);
        if ins.op_count() == 1 {
            let s = i64::from(sext(self.rd(ins, 0, bits, bus)?, bits));
            let a = i64::from(sext(self.gpr[0], bits));
            let r = a * s;
            let over = r != i64::from(sext(r as u32 & mask(bits), bits));
            match bits {
                8 => self.set_reg(Register::AX, r as u32),
                16 => {
                    self.set_reg(Register::AX, r as u32);
                    self.set_reg(Register::DX, (r >> 16) as u32);
                }
                _ => {
                    self.gpr[0] = r as u32;
                    self.gpr[2] = (r >> 32) as u32;
                }
            }
            let f = if over { CF | OF } else { 0 };
            self.flags.set_arith(ARITH, f | szp(r as u32, bits));
            return Ok(Flow::Next);
        }
        let (x, y) = if ins.op_count() == 2 {
            (self.rd(ins, 0, bits, bus)?, self.rd(ins, 1, bits, bus)?)
        } else {
            (self.rd(ins, 1, bits, bus)?, self.rd(ins, 2, bits, bus)?)
        };
        let r = i64::from(sext(x, bits)) * i64::from(sext(y & mask(bits), bits));
        let low = r as u32 & mask(bits);
        let over = r != i64::from(sext(low, bits));
        self.set_reg(ins.op_register(0), low);
        let f = if over { CF | OF } else { 0 };
        self.flags.set_arith(ARITH, f | szp(low, bits));
        Ok(Flow::Next)
    }

    fn div(&mut self, ins: &Instruction, bus: &mut dyn Bus, signed: bool) -> Step {
        let bits = Self::op_bits(ins, 0);
        let s = self.rd(ins, 0, bits, bus)? & mask(bits);
        if s == 0 {
            return Err(Stop::Exc(0, 0));
        }
        let (lo, hi) = match bits {
            8 => (self.gpr[0] & 0xFF, (self.gpr[0] >> 8) & 0xFF),
            16 => (self.gpr[0] & 0xFFFF, self.gpr[2] & 0xFFFF),
            _ => (self.gpr[0], self.gpr[2]),
        };
        let n = (u64::from(hi) << bits) | u64::from(lo);
        let (q, r) = if signed {
            let n = (n << (64 - 2 * bits)) as i64 >> (64 - 2 * bits);
            let d = i64::from(sext(s, bits));
            let q = n.checked_div(d).ok_or(Stop::Exc(0, 0))?;
            let r = n.wrapping_rem(d);
            let lim = 1i64 << (bits - 1);
            if q >= lim || q < -lim {
                return Err(Stop::Exc(0, 0));
            }
            (q as u32 & mask(bits), r as u32 & mask(bits))
        } else {
            let d = u64::from(s);
            let q = n / d;
            if q > u64::from(mask(bits)) {
                return Err(Stop::Exc(0, 0));
            }
            (q as u32, (n % d) as u32)
        };
        match bits {
            8 => self.set_reg(Register::AX, (r << 8) | q),
            16 => {
                self.set_reg(Register::AX, q);
                self.set_reg(Register::DX, r);
            }
            _ => {
                self.gpr[0] = q;
                self.gpr[2] = r;
            }
        }
        Ok(Flow::Next)
    }

    fn bit_test(&mut self, ins: &Instruction, bus: &mut dyn Bus) -> Step {
        let bits = Self::op_bits(ins, 0);
        let off = self.rd(ins, 1, bits, bus)?;
        let (addr, bit) = if ins.op_kind(0) == OpKind::Memory && ins.op_kind(1) == OpKind::Register
        {
            // A register offset reaches outside the operand.
            let so = sext(off, bits);
            let unit = (bits / 8) as i32;
            let a = self
                .ea(ins)
                .wrapping_add((so >> (bits.trailing_zeros())).wrapping_mul(unit) as u32);
            (Some(a), (off & (bits - 1)))
        } else {
            (None, off & (bits - 1))
        };
        let v = match addr {
            Some(a) => self.load(a, bits, bus)?,
            None => self.rd(ins, 0, bits, bus)?,
        };
        let cf = (v >> bit) & 1;
        let nv = match ins.mnemonic() {
            M::Bts => Some(v | (1 << bit)),
            M::Btr => Some(v & !(1 << bit)),
            M::Btc => Some(v ^ (1 << bit)),
            _ => None,
        };
        if let Some(nv) = nv {
            match addr {
                Some(a) => self.store(a, bits, nv, bus)?,
                None => self.wr(ins, 0, bits, nv, bus)?,
            }
        }
        self.flags.set_arith(CF, cf);
        Ok(Flow::Next)
    }

    fn push_op(&mut self, ins: &Instruction, bus: &mut dyn Bus) -> Step {
        match ins.op0_kind() {
            OpKind::Register if ins.op_register(0).is_segment_register() => {
                // A 32-bit push of a segment register writes 16 bits.
                let v = self.reg(ins.op_register(0));
                let sp = self.gpr[4].wrapping_sub(4);
                self.store(sp, 16, v, bus)?;
                self.gpr[4] = sp;
            }
            _ => {
                let bits = match ins.op0_kind() {
                    OpKind::Immediate8to16 | OpKind::Immediate16 => 16,
                    OpKind::Immediate8to32 | OpKind::Immediate32 | OpKind::Immediate8 => 32,
                    _ => Self::op_bits(ins, 0),
                };
                let v = self.rd(ins, 0, bits, bus)?;
                self.push(v, bits, bus)?;
            }
        }
        Ok(Flow::Next)
    }

    fn pop_op(&mut self, ins: &Instruction, bus: &mut dyn Bus) -> Step {
        if ins.op0_kind() == OpKind::Register && ins.op_register(0).is_segment_register() {
            self.gpr[4] = self.gpr[4].wrapping_add(4);
            return Ok(Flow::Next);
        }
        let bits = Self::op_bits(ins, 0);
        let v = self.load(self.gpr[4], bits, bus)?;
        let sp = self.gpr[4].wrapping_add(bits / 8);
        if ins.op0_kind() == OpKind::Memory {
            // The address is computed with ESP already incremented.
            let old = self.gpr[4];
            self.gpr[4] = sp;
            let r = self.wr(ins, 0, bits, v, bus);
            if r.is_err() {
                self.gpr[4] = old;
            }
            r?;
        } else {
            self.gpr[4] = sp;
            self.wr(ins, 0, bits, v, bus)?;
        }
        Ok(Flow::Next)
    }

    fn enter(&mut self, ins: &Instruction, bus: &mut dyn Bus) -> Step {
        let size = u32::from(ins.immediate16());
        let level = u32::from(ins.immediate8_2nd()) & 31;
        let bits = if ins.code() == iced_x86::Code::Enterw_imm16_imm8 { 16 } else { 32 };
        let n = bits / 8;
        let mut sp = self.gpr[4].wrapping_sub(n);
        self.store(sp, bits, self.gpr[5], bus)?;
        let frame = sp;
        if level > 0 {
            let mut bp = self.gpr[5];
            for _ in 1..level {
                bp = bp.wrapping_sub(n);
                let v = self.load(bp, bits, bus)?;
                sp = sp.wrapping_sub(n);
                self.store(sp, bits, v, bus)?;
            }
            sp = sp.wrapping_sub(n);
            self.store(sp, bits, frame, bus)?;
        }
        if bits == 32 {
            self.gpr[5] = frame;
        } else {
            self.gpr[5] = (self.gpr[5] & 0xFFFF_0000) | (frame & 0xFFFF);
        }
        self.gpr[4] = sp.wrapping_sub(size);
        Ok(Flow::Next)
    }

    fn bcd(&mut self, ins: &Instruction) -> Step {
        let al = self.gpr[0] & 0xFF;
        let f = self.flags.get();
        let (cf, af) = (f & CF != 0, f & AF != 0);
        match ins.mnemonic() {
            M::Daa | M::Das => {
                let daa = ins.mnemonic() == M::Daa;
                let mut a = al;
                let mut ncf = false;
                let mut naf = false;
                if (al & 0xF) > 9 || af {
                    a = if daa { a.wrapping_add(6) } else { a.wrapping_sub(6) } & 0xFF;
                    ncf = cf || if daa { al > 0xF9 } else { al < 6 };
                    naf = true;
                }
                if al > 0x99 || cf {
                    a = if daa { a.wrapping_add(0x60) } else { a.wrapping_sub(0x60) } & 0xFF;
                    ncf = true;
                }
                self.set_reg(Register::AL, a);
                let fl = szp(a, 8) | if ncf { CF } else { 0 } | if naf { AF } else { 0 };
                self.flags.set_arith(ARITH, fl);
            }
            M::Aaa | M::Aas => {
                let aaa = ins.mnemonic() == M::Aaa;
                let ax = self.gpr[0] & 0xFFFF;
                let (ax, adj) = if (al & 0xF) > 9 || af {
                    let ax = if aaa { ax.wrapping_add(0x106) } else { ax.wrapping_sub(0x106) };
                    (ax, true)
                } else {
                    (ax, false)
                };
                self.set_reg(Register::AX, ax & 0xFF0F);
                let fl = if adj { CF | AF } else { 0 };
                self.flags.set_arith(CF | AF, fl);
            }
            M::Aam => {
                let base = u32::from(ins.immediate8());
                if base == 0 {
                    return Err(Stop::Exc(0, 0));
                }
                let ah = al / base;
                let a = al % base;
                self.set_reg(Register::AX, (ah << 8) | a);
                self.flags.set_arith(ARITH, szp(a, 8));
            }
            M::Aad => {
                let base = u32::from(ins.immediate8());
                let ah = (self.gpr[0] >> 8) & 0xFF;
                let a = (al + ah * base) & 0xFF;
                self.set_reg(Register::AX, a);
                self.flags.set_arith(ARITH, szp(a, 8));
            }
            _ => {
                // SALC
                self.set_reg(Register::AL, if cf { 0xFF } else { 0 });
            }
        }
        Ok(Flow::Next)
    }

    /// The string instructions, with or without a repeat prefix.
    fn string(&mut self, ins: &Instruction, bus: &mut dyn Bus) -> Step {
        let mn = ins.mnemonic();
        let bits = match mn {
            M::Movsb | M::Cmpsb | M::Stosb | M::Lodsb | M::Scasb | M::Insb | M::Outsb => 8,
            M::Movsw | M::Cmpsw | M::Stosw | M::Lodsw | M::Scasw | M::Insw | M::Outsw => 16,
            _ => 32,
        };
        let n: u32 = bits / 8;
        // 16-bit addressing uses SI, DI and CX.
        let a16 = (0..ins.op_count()).any(|i| {
            matches!(ins.op_kind(i), OpKind::MemorySegSI | OpKind::MemorySegDI | OpKind::MemoryESDI)
        });
        let am = if a16 { 0xFFFF } else { u32::MAX };
        let src_base = if ins.memory_segment() == Register::FS { self.fs_base } else { 0 };
        let rep = ins.has_rep_prefix() || ins.has_repne_prefix();
        let compares =
            matches!(mn, M::Cmpsb | M::Cmpsw | M::Cmpsd | M::Scasb | M::Scasw | M::Scasd);
        let step = if self.flags.bits & DF != 0 { n.wrapping_neg() } else { n };
        let mut done = 0u32;
        loop {
            if rep && self.gpr[1] & am == 0 {
                return Ok(Flow::Next);
            }
            let si = self.gpr[6] & am;
            let di = self.gpr[7] & am;
            let mut adv_si = false;
            let mut adv_di = false;
            match mn {
                M::Movsb | M::Movsw | M::Movsd => {
                    let v = self.load(src_base.wrapping_add(si), bits, bus)?;
                    self.store(di, bits, v, bus)?;
                    adv_si = true;
                    adv_di = true;
                }
                M::Stosb | M::Stosw | M::Stosd => {
                    self.store(di, bits, self.gpr[0], bus)?;
                    adv_di = true;
                }
                M::Lodsb | M::Lodsw | M::Lodsd => {
                    let v = self.load(src_base.wrapping_add(si), bits, bus)?;
                    let r = match bits {
                        8 => Register::AL,
                        16 => Register::AX,
                        _ => Register::EAX,
                    };
                    self.set_reg(r, v);
                    adv_si = true;
                }
                M::Cmpsb | M::Cmpsw | M::Cmpsd => {
                    let a = self.load(src_base.wrapping_add(si), bits, bus)?;
                    let b = self.load(di, bits, bus)?;
                    self.flags.lazy(Op::Sub, bits, a, b, a.wrapping_sub(b));
                    adv_si = true;
                    adv_di = true;
                }
                M::Scasb | M::Scasw | M::Scasd => {
                    let a = self.gpr[0] & mask(bits);
                    let b = self.load(di, bits, bus)?;
                    self.flags.lazy(Op::Sub, bits, a, b, a.wrapping_sub(b));
                    adv_di = true;
                }
                M::Insb | M::Insw | M::Insd => {
                    let port = self.gpr[2] as u16;
                    // The store is checked first: a port read may have an
                    // effect, so it must not be made twice.
                    self.mem.target(di, true).map_err(Stop::Page)?;
                    let v = self.outside(bus, |b| b.io_read(port, n as u8));
                    self.store(di, bits, v, bus)?;
                    adv_di = true;
                }
                _ => {
                    let port = self.gpr[2] as u16;
                    let v = self.load(src_base.wrapping_add(si), bits, bus)?;
                    self.outside(bus, |b| b.io_write(port, n as u8, v));
                    adv_si = true;
                }
            }
            if adv_si {
                self.gpr[6] = (self.gpr[6] & !am) | (si.wrapping_add(step) & am);
            }
            if adv_di {
                self.gpr[7] = (self.gpr[7] & !am) | (di.wrapping_add(step) & am);
            }
            if !rep {
                return Ok(Flow::Next);
            }
            self.gpr[1] = (self.gpr[1] & !am) | (self.gpr[1].wrapping_sub(1) & am);
            // A fault in a later element resumes from here.
            self.snap = Snap { gpr: self.gpr, flags: self.flags };
            if compares {
                let z = self.flags.zf();
                if (ins.has_repe_prefix() && !z) || (ins.has_repne_prefix() && z) {
                    return Ok(Flow::Next);
                }
            }
            done += 1;
            if done.is_multiple_of(STRING_SLICE) {
                self.mem.poll();
                if self.kicked() {
                    // Continue later from this instruction.
                    return Ok(Flow::Jump(ins.ip32()));
                }
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Alu {
    Add,
    Or,
    Adc,
    Sbb,
    And,
    Sub,
    Xor,
    Cmp,
    Test,
}
