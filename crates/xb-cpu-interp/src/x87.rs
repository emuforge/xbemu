//! x87 instructions.
//!
//! Values live in the registers as [`F80`]; arithmetic is done in `f64`
//! (module docs of [`crate::f80`]). When a thread selects single precision
//! (control word PC = 0) results are rounded to `f32`: titles of the
//! Direct3D era set it, and Burnout 3 hangs while a race loads when its
//! voice sort compares a float it stored with a wider value still in a
//! register.
//!
//! Exceptions are always treated as masked (the kernel's control word masks
//! them all): an invalid operation gives the indefinite NaN and sets IE in
//! the status word; a stack fault also sets SF.

use iced_x86::{Instruction, Mnemonic as M, OpKind, Register};
use xb_cpu::Bus;

use crate::cpu::{Flow, InterpCpu, Step, Stop};
use crate::fast::{Handler, Uop, ea_of};
use crate::f80::{F80, Fpr, Round, round_f64};
use crate::flags::{ARITH, CF, PF, ZF};

/// Status word bits.
/// 2^63: FSIN, FCOS, FPTAN and FSINCOS leave larger operands alone.
const TWO_63: f64 = 9_223_372_036_854_775_808.0;
const IE: u16 = 1 << 0;
const ZE: u16 = 1 << 2;
const SFAULT: u16 = 1 << 6;
const C0: u16 = 1 << 8;
const C1: u16 = 1 << 9;
const C2: u16 = 1 << 10;
const C3: u16 = 1 << 14;

impl InterpCpu {
    // ---- the register stack ----

    fn phys(&self, i: u32) -> usize {
        ((self.fpu.top + i) & 7) as usize
    }

    /// ST(i), or the indefinite with IE and SF set when it is empty.
    #[inline]
    fn st(&mut self, i: u32) -> Fpr {
        let p = self.phys(i);
        if self.fpu.valid & (1 << p) == 0 {
            self.fpu.fsw |= IE | SFAULT;
            self.fpu.fsw &= !C1;
            return Fpr::from_f80(F80::INDEFINITE);
        }
        self.fpu.st[p]
    }

    #[inline]
    fn stf(&mut self, i: u32) -> f64 {
        self.st(i).f
    }

    #[inline]
    fn set_st(&mut self, i: u32, v: Fpr) {
        let p = self.phys(i);
        self.fpu.st[p] = v;
        self.fpu.valid |= 1 << p;
    }

    #[inline]
    fn set_stf(&mut self, i: u32, v: f64) {
        let v = self.round_pc(v);
        self.set_st(i, Fpr::from_f64(v));
    }

    fn fpush(&mut self, v: Fpr) {
        self.fpu.top = (self.fpu.top + 7) & 7;
        let p = self.phys(0);
        if self.fpu.valid & (1 << p) != 0 {
            // Stack overflow.
            self.fpu.fsw |= IE | SFAULT | C1;
            self.fpu.st[p] = Fpr::from_f80(F80::INDEFINITE);
        } else {
            self.fpu.st[p] = v;
        }
        self.fpu.valid |= 1 << p;
    }

    fn fpushf(&mut self, v: f64) {
        self.fpush(Fpr::from_f64(v));
    }

    fn fpop(&mut self) {
        let p = self.phys(0);
        self.fpu.valid &= !(1 << p);
        self.fpu.top = (self.fpu.top + 1) & 7;
    }

    fn rc(&self) -> Round {
        Round::from_bits(u32::from(self.fpu.fcw >> 10))
    }

    /// Rounds an arithmetic result to the precision the control word asks
    /// for: single (PC = 0) or, for double and extended, the `f64` itself.
    fn round_pc(&self, v: f64) -> f64 {
        if (self.fpu.fcw >> 8) & 3 == 0 { f64::from(v as f32) } else { v }
    }

    /// The x87 status word with TOP.
    pub(crate) fn fsw(&self) -> u16 {
        self.fpu.fsw & !0x3800 | ((self.fpu.top as u16) << 11)
    }

    /// Reads a memory operand as a value: m32, m64, m80 real or m16, m32,
    /// m64 integer for the `FI*` forms.
    fn fmem(&mut self, ins: &Instruction, int: bool, bus: &mut dyn Bus) -> Result<Fpr, Stop> {
        let a = self.ea(ins);
        let size = ins.memory_size().size();
        Ok(match (int, size) {
            (false, 4) => Fpr::from_f64(f64::from(f32::from_bits(self.load(a, 32, bus)?))),
            (false, 8) => Fpr::from_f64(f64::from_bits(self.load64(a, bus)?)),
            (false, _) => Fpr::from_f80(F80::from_bytes(&self.mem.read::<10>(a, bus)?)),
            (true, 2) => Fpr::from_f64(f64::from(self.load(a, 16, bus)? as i16)),
            (true, 4) => Fpr::from_f64(f64::from(self.load(a, 32, bus)? as i32)),
            (true, _) => Fpr::from_f80(F80::from_i64(self.load64(a, bus)? as i64)),
        })
    }

    /// The source of a two-operand arithmetic form: memory, or ST(i).
    fn fsrc(&mut self, ins: &Instruction, bus: &mut dyn Bus) -> Result<(u32, u32, f64), Stop> {
        // (destination index, source index or u32::MAX for memory, source)
        match ins.op_count() {
            1 if ins.op_kind(0) == OpKind::Memory => {
                let int = matches!(
                    ins.mnemonic(),
                    M::Fiadd
                        | M::Fisub
                        | M::Fisubr
                        | M::Fimul
                        | M::Fidiv
                        | M::Fidivr
                        | M::Ficom
                        | M::Ficomp
                );
                let v = self.fmem(ins, int, bus)?.f;
                Ok((0, u32::MAX, v))
            }
            1 => {
                let s = st_index(ins.op_register(0));
                Ok((0, s, self.stf(s)))
            }
            0 => Ok((1, 0, self.stf(0))),
            _ => {
                let d = st_index(ins.op_register(0));
                let s = st_index(ins.op_register(1));
                Ok((d, s, self.stf(s)))
            }
        }
    }

    fn fcompare(&mut self, a: f64, b: f64) {
        self.fpu.fsw &= !(C0 | C1 | C2 | C3);
        self.fpu.fsw |= if a.is_nan() || b.is_nan() {
            C0 | C2 | C3
        } else if a > b {
            0
        } else if a < b {
            C0
        } else {
            C3
        };
    }

    /// Stores an integer conversion of ST(0) to memory (FIST, FISTP).
    fn fist(&mut self, ins: &Instruction, bus: &mut dyn Bus) -> Result<(), Stop> {
        let a = self.ea(ins);
        let bits = (ins.memory_size().size() * 8) as u32;
        let v = self.st(0);
        let rc = if ins.mnemonic() == M::Fisttp { Round::Zero } else { self.rc() };
        let lim = 1i128 << (bits - 1);
        let int = match v.exact() {
            Some(x) => x.to_int(rc),
            None if v.f.is_finite() => Some(round_f64(v.f, rc) as i128),
            None => None,
        };
        let out = match int {
            Some(i) if i >= -lim && i < lim => i as i64,
            _ => {
                self.fpu.fsw |= IE;
                i64::MIN >> (64 - bits)
            }
        };
        match bits {
            16 => self.store(a, 16, out as u32, bus)?,
            32 => self.store(a, 32, out as u32, bus)?,
            _ => self.store64(a, out as u64, bus)?,
        }
        Ok(())
    }

    /// The full 16-bit tag word from the abridged one and the registers.
    fn full_tags(&self) -> u16 {
        let mut t = 0u16;
        for p in 0..8 {
            let tag = if self.fpu.valid & (1 << p) == 0 {
                3
            } else {
                let v = self.fpu.st[p].f80();
                if v.is_zero() {
                    1
                } else if v.is_nan() || v.is_inf() || v.is_denormal() || v.mant >> 63 == 0 {
                    2
                } else {
                    0
                }
            };
            t |= tag << (2 * p);
        }
        t
    }

    /// FNSTENV's 28-byte protected-mode image.
    fn store_env(&mut self, a: u32, bus: &mut dyn Bus) -> Result<(), Stop> {
        let mut b = [0u8; 28];
        b[0..2].copy_from_slice(&self.fpu.fcw.to_le_bytes());
        b[4..6].copy_from_slice(&self.fsw().to_le_bytes());
        b[8..10].copy_from_slice(&self.full_tags().to_le_bytes());
        self.mem.write(a, &b[..16], bus)?;
        self.mem.write(a.wrapping_add(16), &b[16..], bus)?;
        Ok(())
    }

    fn load_env(&mut self, a: u32, bus: &mut dyn Bus) -> Result<(), Stop> {
        let b = self.mem.read::<12>(a, bus)?;
        self.fpu.fcw = u16::from_le_bytes([b[0], b[1]]);
        let fsw = u16::from_le_bytes([b[4], b[5]]);
        self.fpu.top = u32::from(fsw >> 11) & 7;
        self.fpu.fsw = fsw & !0x3800;
        let tags = u16::from_le_bytes([b[8], b[9]]);
        self.fpu.valid =
            (0..8).fold(0, |v, p| if (tags >> (2 * p)) & 3 == 3 { v } else { v | 1 << p });
        Ok(())
    }

    fn finit(&mut self) {
        self.fpu.fcw = 0x037F;
        self.fpu.fsw = 0;
        self.fpu.top = 0;
        self.fpu.valid = 0;
    }

    /// x87 instructions; anything else goes on to MMX and SSE.
    pub(crate) fn step_fpu(&mut self, ins: &Instruction, bus: &mut dyn Bus) -> Step {
        let next = Ok(Flow::Next);
        let mn = ins.mnemonic();
        match mn {
            M::Fld => {
                let v = if ins.op_kind(0) == OpKind::Memory {
                    self.fmem(ins, false, bus)?
                } else {
                    self.st(st_index(ins.op_register(0)))
                };
                self.fpush(v);
                next
            }
            M::Fild => {
                let v = self.fmem(ins, true, bus)?;
                self.fpush(v);
                next
            }
            M::Fbld => Err(self.unimplemented(ins)),
            M::Fld1 => {
                self.fpushf(1.0);
                next
            }
            M::Fldz => {
                self.fpushf(0.0);
                next
            }
            M::Fldpi => {
                self.fpushf(std::f64::consts::PI);
                next
            }
            M::Fldl2e => {
                self.fpushf(std::f64::consts::LOG2_E);
                next
            }
            M::Fldl2t => {
                self.fpushf(std::f64::consts::LOG2_10);
                next
            }
            M::Fldlg2 => {
                self.fpushf(std::f64::consts::LOG10_2);
                next
            }
            M::Fldln2 => {
                self.fpushf(std::f64::consts::LN_2);
                next
            }
            M::Fst | M::Fstp => {
                let v = self.st(0);
                if ins.op_kind(0) == OpKind::Memory {
                    let a = self.ea(ins);
                    match ins.memory_size().size() {
                        4 => self.store(a, 32, (v.f as f32).to_bits(), bus)?,
                        8 => self.store64(a, v.f.to_bits(), bus)?,
                        _ => self.mem.write(a, &v.f80().to_bytes(), bus)?,
                    }
                } else {
                    self.set_st(st_index(ins.op_register(0)), v);
                }
                if mn == M::Fstp {
                    self.fpop();
                }
                next
            }
            M::Fist | M::Fistp | M::Fisttp => {
                self.fist(ins, bus)?;
                if mn != M::Fist {
                    self.fpop();
                }
                next
            }
            M::Fbstp => Err(self.unimplemented(ins)),
            M::Fxch => {
                let i = if ins.op_count() == 0 {
                    1
                } else {
                    st_index(ins.op_register(1).max(ins.op_register(0)))
                };
                let a = self.st(0);
                let b = self.st(i);
                self.set_st(0, b);
                self.set_st(i, a);
                self.fpu.fsw &= !C1;
                next
            }

            M::Fadd | M::Faddp | M::Fiadd => self.farith(ins, bus, |d, s| d + s),
            M::Fsub | M::Fsubp | M::Fisub => self.farith(ins, bus, |d, s| d - s),
            M::Fsubr | M::Fsubrp | M::Fisubr => self.farith(ins, bus, |d, s| s - d),
            M::Fmul | M::Fmulp | M::Fimul => self.farith(ins, bus, |d, s| d * s),
            M::Fdiv | M::Fdivp | M::Fidiv => {
                self.zero_divide_check(ins, bus, false)?;
                self.farith(ins, bus, |d, s| d / s)
            }
            M::Fdivr | M::Fdivrp | M::Fidivr => {
                self.zero_divide_check(ins, bus, true)?;
                self.farith(ins, bus, |d, s| s / d)
            }

            M::Fchs => {
                let v = self.st(0).neg();
                self.set_st(0, v);
                next
            }
            M::Fabs => {
                let v = self.st(0).abs();
                self.set_st(0, v);
                next
            }
            M::Fsqrt => {
                let v = self.stf(0);
                if v < 0.0 {
                    self.fpu.fsw |= IE;
                }
                self.set_stf(0, v.sqrt());
                next
            }
            M::Frndint => {
                let v = self.stf(0);
                self.set_st(0, Fpr::from_f64(round_f64(v, self.rc())));
                next
            }
            M::Fsin | M::Fcos | M::Fptan | M::Fsincos => {
                let v = self.stf(0);
                // Operands beyond 2^63 are left alone with C2 set.
                if v.abs() >= TWO_63 || !v.is_finite() {
                    self.fpu.fsw |= C2;
                    return next;
                }
                self.fpu.fsw &= !C2;
                match mn {
                    M::Fsin => self.set_stf(0, v.sin()),
                    M::Fcos => self.set_stf(0, v.cos()),
                    M::Fptan => {
                        self.set_stf(0, v.tan());
                        self.fpushf(1.0);
                    }
                    _ => {
                        self.set_stf(0, v.sin());
                        let c = self.round_pc(v.cos());
                        self.fpushf(c);
                    }
                }
                next
            }
            M::Fpatan => {
                let x = self.stf(0);
                let y = self.stf(1);
                self.set_stf(1, y.atan2(x));
                self.fpop();
                next
            }
            M::F2xm1 => {
                let v = self.stf(0);
                self.set_stf(0, v.exp2() - 1.0);
                next
            }
            M::Fyl2x => {
                let x = self.stf(0);
                let y = self.stf(1);
                self.set_stf(1, y * x.log2());
                self.fpop();
                next
            }
            M::Fyl2xp1 => {
                let x = self.stf(0);
                let y = self.stf(1);
                self.set_stf(1, y * x.ln_1p() / std::f64::consts::LN_2);
                self.fpop();
                next
            }
            M::Fscale => {
                let v = self.stf(0);
                let s = self.stf(1).trunc();
                let s = s.clamp(-4096.0, 4096.0) as i32;
                self.set_stf(0, v * 2f64.powi(s));
                next
            }
            M::Fxtract => {
                let v = self.st(0).f80();
                if v.is_zero() {
                    self.fpu.fsw |= ZE;
                    self.set_stf(0, f64::NEG_INFINITY);
                    self.fpush(Fpr::from_f80(v));
                } else {
                    let x = v.to_f64();
                    let e = x.abs().log2().floor();
                    let m = x / 2f64.powf(e);
                    self.set_stf(0, e);
                    self.fpushf(m);
                }
                next
            }
            M::Fprem | M::Fprem1 => {
                let x = self.stf(0);
                let y = self.stf(1);
                let q = if mn == M::Fprem { (x / y).trunc() } else { (x / y).round_ties_even() };
                let r = x - q * y;
                let r = if mn == M::Fprem { x % y } else { r };
                self.set_st(0, Fpr::from_f64(r));
                let qi = q.abs() as u64;
                self.fpu.fsw &= !(C0 | C1 | C2 | C3);
                if qi & 1 != 0 {
                    self.fpu.fsw |= C1;
                }
                if qi & 2 != 0 {
                    self.fpu.fsw |= C3;
                }
                if qi & 4 != 0 {
                    self.fpu.fsw |= C0;
                }
                next
            }

            M::Fcom | M::Fcomp | M::Fucom | M::Fucomp | M::Ficom | M::Ficomp => {
                let (_, _, s) = self.fsrc(ins, bus)?;
                let s = if ins.op_count() == 0 { self.stf(1) } else { s };
                let a = self.stf(0);
                self.fcompare(a, s);
                if matches!(mn, M::Fcomp | M::Fucomp | M::Ficomp) {
                    self.fpop();
                }
                next
            }
            M::Fcompp | M::Fucompp => {
                let a = self.stf(0);
                let b = self.stf(1);
                self.fcompare(a, b);
                self.fpop();
                self.fpop();
                next
            }
            M::Fcomi | M::Fcomip | M::Fucomi | M::Fucomip => {
                let i = st_index(ins.op_register(1));
                let a = self.stf(0);
                let b = self.stf(i);
                let f = if a.is_nan() || b.is_nan() {
                    ZF | PF | CF
                } else if a > b {
                    0
                } else if a < b {
                    CF
                } else {
                    ZF
                };
                self.flags.set_arith(ARITH, f);
                if matches!(mn, M::Fcomip | M::Fucomip) {
                    self.fpop();
                }
                next
            }
            M::Ftst => {
                let a = self.stf(0);
                self.fcompare(a, 0.0);
                next
            }
            M::Fxam => {
                let p = self.phys(0);
                let v = self.fpu.st[p].f80();
                self.fpu.fsw &= !(C0 | C1 | C2 | C3);
                if v.sign() {
                    self.fpu.fsw |= C1;
                }
                self.fpu.fsw |= if self.fpu.valid & (1 << p) == 0 {
                    C3 | C0
                } else if v.is_nan() {
                    C0
                } else if v.is_inf() {
                    C2 | C0
                } else if v.is_zero() {
                    C3
                } else if v.is_denormal() {
                    C3 | C2
                } else if v.mant >> 63 == 0 {
                    0
                } else {
                    C2
                };
                next
            }
            M::Fcmovb
            | M::Fcmove
            | M::Fcmovbe
            | M::Fcmovu
            | M::Fcmovnb
            | M::Fcmovne
            | M::Fcmovnbe
            | M::Fcmovnu => {
                let f = self.flags.get();
                let (cf, zf, pf) = (f & CF != 0, f & ZF != 0, f & PF != 0);
                let take = match mn {
                    M::Fcmovb => cf,
                    M::Fcmove => zf,
                    M::Fcmovbe => cf || zf,
                    M::Fcmovu => pf,
                    M::Fcmovnb => !cf,
                    M::Fcmovne => !zf,
                    M::Fcmovnbe => !cf && !zf,
                    _ => !pf,
                };
                if take {
                    let v = self.st(st_index(ins.op_register(1)));
                    self.set_st(0, v);
                }
                next
            }
            M::Ffree | M::Ffreep => {
                let p = self.phys(st_index(ins.op_register(0)));
                self.fpu.valid &= !(1 << p);
                if mn == M::Ffreep {
                    self.fpop();
                }
                next
            }
            M::Fincstp => {
                self.fpu.top = (self.fpu.top + 1) & 7;
                next
            }
            M::Fdecstp => {
                self.fpu.top = (self.fpu.top + 7) & 7;
                next
            }
            M::Fnop => next,
            M::Fninit | M::Finit => {
                self.finit();
                next
            }
            M::Fnclex | M::Fclex => {
                // The exception flags, ES and B.
                self.fpu.fsw &= !0x80FF;
                next
            }
            M::Fnstsw | M::Fstsw => {
                let v = u32::from(self.fsw());
                if ins.op_kind(0) == OpKind::Register {
                    self.set_reg(Register::AX, v);
                } else {
                    self.wr(ins, 0, 16, v, bus)?;
                }
                next
            }
            M::Fnstcw | M::Fstcw => {
                self.wr(ins, 0, 16, u32::from(self.fpu.fcw), bus)?;
                next
            }
            M::Fldcw => {
                let v = self.rd(ins, 0, 16, bus)?;
                self.fpu.fcw = v as u16;
                next
            }
            M::Fnstenv | M::Fstenv => {
                let a = self.ea(ins);
                self.store_env(a, bus)?;
                // FNSTENV masks every exception afterwards.
                self.fpu.fcw |= 0x3F;
                next
            }
            M::Fldenv => {
                let a = self.ea(ins);
                self.load_env(a, bus)?;
                next
            }
            M::Fnsave | M::Fsave => {
                let a = self.ea(ins);
                let mut regs = [0u8; 80];
                for i in 0..8 {
                    let v = self.fpu.st[self.phys(i)];
                    regs[10 * i as usize..10 * i as usize + 10]
                        .copy_from_slice(&v.f80().to_bytes());
                }
                // The registers first: a fault then leaves the state as it was.
                for (k, c) in regs.chunks(16).enumerate() {
                    self.mem.write(a.wrapping_add(28 + 16 * k as u32), c, bus)?;
                }
                self.store_env(a, bus)?;
                self.finit();
                next
            }
            M::Frstor => {
                let a = self.ea(ins);
                let mut regs = [F80::default(); 8];
                for (i, r) in regs.iter_mut().enumerate() {
                    *r = F80::from_bytes(
                        &self.mem.read::<10>(a.wrapping_add(28 + 10 * i as u32), bus)?,
                    );
                }
                self.load_env(a, bus)?;
                for (i, r) in regs.into_iter().enumerate() {
                    let p = self.phys(i as u32);
                    self.fpu.st[p] = Fpr::from_f80(r);
                }
                next
            }
            M::Fxsave => {
                let a = self.ea(ins);
                let img = self.fx_image();
                for (k, c) in img.chunks(16).enumerate() {
                    self.mem.write(a.wrapping_add(16 * k as u32), c, bus)?;
                }
                next
            }
            M::Fxrstor => {
                let a = self.ea(ins);
                let mut img = [0u8; 512];
                for (k, c) in img.chunks_mut(16).enumerate() {
                    c.copy_from_slice(&self.mem.read::<16>(a.wrapping_add(16 * k as u32), bus)?);
                }
                self.set_fx_image(&img);
                next
            }
            _ => self.step_simd(ins, bus),
        }
    }

    /// A division whose divisor is zero sets ZE (the result is the
    /// infinity `f64` division gives).
    fn zero_divide_check(
        &mut self,
        ins: &Instruction,
        bus: &mut dyn Bus,
        reverse: bool,
    ) -> Result<(), Stop> {
        let (d, s, v) = self.fsrc(ins, bus)?;
        let divisor = if reverse { self.stf(d) } else { v };
        let _ = s;
        if divisor == 0.0 {
            self.fpu.fsw |= ZE;
        }
        Ok(())
    }

    fn farith(
        &mut self,
        ins: &Instruction,
        bus: &mut dyn Bus,
        f: impl Fn(f64, f64) -> f64,
    ) -> Step {
        let (d, _, s) = self.fsrc(ins, bus)?;
        let a = self.stf(d);
        let r = f(a, s);
        if r.is_nan() && !a.is_nan() && !s.is_nan() {
            self.fpu.fsw |= IE;
            self.set_st(d, Fpr::from_f80(F80::INDEFINITE));
        } else {
            self.set_stf(d, r);
        }
        let pops = matches!(
            ins.mnemonic(),
            M::Faddp | M::Fsubp | M::Fsubrp | M::Fmulp | M::Fdivp | M::Fdivrp
        );
        if pops {
            self.fpop();
        }
        Ok(Flow::Next)
    }

    /// The FXSAVE image of the current state.
    pub(crate) fn fx_image(&self) -> [u8; 512] {
        let mut ctx = xb_cpu::Context::default();
        self.store_fx(&mut ctx.fx.0);
        ctx.fx.0
    }

    /// Loads the x87, MMX and SSE state from an FXSAVE image.
    pub(crate) fn set_fx_image(&mut self, fx: &[u8; 512]) {
        self.load_fx(fx);
    }
}

/// The index of ST(i) from its register.
fn st_index(r: Register) -> u32 {
    (r as u32).wrapping_sub(Register::ST0 as u32) & 7
}

// ---- forms with their own handlers (`fast`) ----
//
// The forms a title's floating-point code is mostly made of: loads, stores,
// arithmetic and compares with a single or double in memory or a register
// of the stack. Each does what its arm of `step_fpu` does, in the same
// order, with the operand already resolved: a memory form's address parts
// are in the instruction's `ea`, a register form's ST index in `r`, and an
// arithmetic form's operation in `imm`.

/// The arithmetic operations, as `Uop::imm` holds them.
const FADD: u32 = 0;
const FMUL: u32 = 1;
const FSUB: u32 = 2;
const FSUBR: u32 = 3;
const FDIV: u32 = 4;
const FDIVR: u32 = 5;

/// The operation and form of an arithmetic instruction with its own
/// handler: a single (0) or double (1) in memory, `ST0 op= ST(i)` (2),
/// `ST(i) op= ST0` (3) or that and a pop (4).
fn arith_form(code: iced_x86::Code) -> Option<(u32, u8)> {
    use iced_x86::Code as C;
    Some(match code {
        C::Fadd_m32fp => (FADD, 0),
        C::Fadd_m64fp => (FADD, 1),
        C::Fadd_st0_sti => (FADD, 2),
        C::Fadd_sti_st0 => (FADD, 3),
        C::Faddp_sti_st0 => (FADD, 4),
        C::Fmul_m32fp => (FMUL, 0),
        C::Fmul_m64fp => (FMUL, 1),
        C::Fmul_st0_sti => (FMUL, 2),
        C::Fmul_sti_st0 => (FMUL, 3),
        C::Fmulp_sti_st0 => (FMUL, 4),
        C::Fsub_m32fp => (FSUB, 0),
        C::Fsub_m64fp => (FSUB, 1),
        C::Fsub_st0_sti => (FSUB, 2),
        C::Fsub_sti_st0 => (FSUB, 3),
        C::Fsubp_sti_st0 => (FSUB, 4),
        C::Fsubr_m32fp => (FSUBR, 0),
        C::Fsubr_m64fp => (FSUBR, 1),
        C::Fsubr_st0_sti => (FSUBR, 2),
        C::Fsubr_sti_st0 => (FSUBR, 3),
        C::Fsubrp_sti_st0 => (FSUBR, 4),
        C::Fdiv_m32fp => (FDIV, 0),
        C::Fdiv_m64fp => (FDIV, 1),
        C::Fdiv_st0_sti => (FDIV, 2),
        C::Fdiv_sti_st0 => (FDIV, 3),
        C::Fdivp_sti_st0 => (FDIV, 4),
        C::Fdivr_m32fp => (FDIVR, 0),
        C::Fdivr_m64fp => (FDIVR, 1),
        C::Fdivr_st0_sti => (FDIVR, 2),
        C::Fdivr_sti_st0 => (FDIVR, 3),
        C::Fdivrp_sti_st0 => (FDIVR, 4),
        _ => return None,
    })
}

/// The handler of an x87 form that has its own, with `u`'s operand fields
/// set for it.
pub(crate) fn pick(ins: &Instruction, u: &mut Uop) -> Option<Handler> {
    use iced_x86::Code as C;
    let code = ins.code();
    let sti = |op: u32| st_index(ins.op_register(op)) as u8;
    if let Some((op, form)) = arith_form(code) {
        u.imm = op;
        return Some(match form {
            0 => {
                u.ea = ea_of(ins)?;
                arith_m::<4>
            }
            1 => {
                u.ea = ea_of(ins)?;
                arith_m::<8>
            }
            2 => {
                u.r = sti(1);
                arith_st0_sti
            }
            3 => {
                u.r = sti(0);
                arith_sti_st0::<false>
            }
            _ => {
                u.r = sti(0);
                arith_sti_st0::<true>
            }
        });
    }
    let handler: Handler = match code {
        C::Fld_m32fp => fld_m::<4>,
        C::Fld_m64fp => fld_m::<8>,
        C::Fst_m32fp => fst_m::<4, false>,
        C::Fstp_m32fp => fst_m::<4, true>,
        C::Fst_m64fp => fst_m::<8, false>,
        C::Fstp_m64fp => fst_m::<8, true>,
        C::Fcom_m32fp => fcom_m::<4, false>,
        C::Fcomp_m32fp => fcom_m::<4, true>,
        C::Fcom_m64fp => fcom_m::<8, false>,
        C::Fcomp_m64fp => fcom_m::<8, true>,
        C::Fild_m32int => fild_m32,
        C::Fld_sti => {
            u.r = sti(0);
            return Some(fld_st);
        }
        C::Fst_sti => {
            u.r = sti(0);
            return Some(fst_st::<false>);
        }
        C::Fstp_sti => {
            u.r = sti(0);
            return Some(fst_st::<true>);
        }
        C::Fcom_st0_sti | C::Fucom_st0_sti => {
            u.r = sti(1);
            return Some(fcom_st::<false>);
        }
        C::Fcomp_st0_sti | C::Fucomp_st0_sti => {
            u.r = sti(1);
            return Some(fcom_st::<true>);
        }
        C::Fxch_st0_sti => {
            u.r = sti(1);
            return Some(fxch);
        }
        C::Fnstsw_AX => return Some(fnstsw_ax),
        _ => return None,
    };
    // The memory forms.
    u.ea = ea_of(ins)?;
    Some(handler)
}

impl InterpCpu {
    /// A single (`N` = 4) or double (8) in memory, as a double.
    #[inline(always)]
    fn fload<const N: usize>(&mut self, u: &Uop, bus: &mut dyn Bus) -> Result<f64, Stop> {
        let a = self.addr(&u.ea);
        Ok(if N == 4 {
            f64::from(f32::from_bits(self.load(a, 32, bus)?))
        } else {
            f64::from_bits(self.load64(a, bus)?)
        })
    }

    /// `ST(d) = ST(d) op s`, as `farith` and `zero_divide_check` do it.
    #[inline(always)]
    fn fapply(&mut self, op: u32, d: u32, s: f64) {
        let a = self.stf(d);
        let r = match op {
            FADD => a + s,
            FMUL => a * s,
            FSUB => a - s,
            FSUBR => s - a,
            FDIV => {
                if s == 0.0 {
                    self.fpu.fsw |= ZE;
                }
                a / s
            }
            _ => {
                if a == 0.0 {
                    self.fpu.fsw |= ZE;
                }
                s / a
            }
        };
        if r.is_nan() && !a.is_nan() && !s.is_nan() {
            self.fpu.fsw |= IE;
            self.set_st(d, Fpr::from_f80(F80::INDEFINITE));
        } else {
            self.set_stf(d, r);
        }
    }
}

fn fld_m<const N: usize>(c: &mut InterpCpu, u: &Uop, bus: &mut dyn Bus) -> Step {
    let v = c.fload::<N>(u, bus)?;
    c.fpush(Fpr::from_f64(v));
    Ok(Flow::Next)
}

fn fld_st(c: &mut InterpCpu, u: &Uop, _: &mut dyn Bus) -> Step {
    let v = c.st(u32::from(u.r));
    c.fpush(v);
    Ok(Flow::Next)
}

fn fild_m32(c: &mut InterpCpu, u: &Uop, bus: &mut dyn Bus) -> Step {
    let a = c.addr(&u.ea);
    let v = c.load(a, 32, bus)? as i32;
    c.fpush(Fpr::from_f64(f64::from(v)));
    Ok(Flow::Next)
}

fn fst_m<const N: usize, const POP: bool>(c: &mut InterpCpu, u: &Uop, bus: &mut dyn Bus) -> Step {
    let v = c.st(0);
    let a = c.addr(&u.ea);
    if N == 4 {
        c.store(a, 32, (v.f as f32).to_bits(), bus)?;
    } else {
        c.store64(a, v.f.to_bits(), bus)?;
    }
    if POP {
        c.fpop();
    }
    Ok(Flow::Next)
}

fn fst_st<const POP: bool>(c: &mut InterpCpu, u: &Uop, _: &mut dyn Bus) -> Step {
    let v = c.st(0);
    c.set_st(u32::from(u.r), v);
    if POP {
        c.fpop();
    }
    Ok(Flow::Next)
}

fn arith_m<const N: usize>(c: &mut InterpCpu, u: &Uop, bus: &mut dyn Bus) -> Step {
    let s = c.fload::<N>(u, bus)?;
    c.fapply(u.imm, 0, s);
    Ok(Flow::Next)
}

fn arith_st0_sti(c: &mut InterpCpu, u: &Uop, _: &mut dyn Bus) -> Step {
    let s = c.stf(u32::from(u.r));
    c.fapply(u.imm, 0, s);
    Ok(Flow::Next)
}

fn arith_sti_st0<const POP: bool>(c: &mut InterpCpu, u: &Uop, _: &mut dyn Bus) -> Step {
    let s = c.stf(0);
    c.fapply(u.imm, u32::from(u.r), s);
    if POP {
        c.fpop();
    }
    Ok(Flow::Next)
}

fn fcom_m<const N: usize, const POP: bool>(c: &mut InterpCpu, u: &Uop, bus: &mut dyn Bus) -> Step {
    let s = c.fload::<N>(u, bus)?;
    let a = c.stf(0);
    c.fcompare(a, s);
    if POP {
        c.fpop();
    }
    Ok(Flow::Next)
}

fn fcom_st<const POP: bool>(c: &mut InterpCpu, u: &Uop, _: &mut dyn Bus) -> Step {
    let s = c.stf(u32::from(u.r));
    let a = c.stf(0);
    c.fcompare(a, s);
    if POP {
        c.fpop();
    }
    Ok(Flow::Next)
}

fn fxch(c: &mut InterpCpu, u: &Uop, _: &mut dyn Bus) -> Step {
    let i = u32::from(u.r);
    let a = c.st(0);
    let b = c.st(i);
    c.set_st(0, b);
    c.set_st(i, a);
    c.fpu.fsw &= !C1;
    Ok(Flow::Next)
}

fn fnstsw_ax(c: &mut InterpCpu, _: &Uop, _: &mut dyn Bus) -> Step {
    let v = u32::from(c.fsw());
    c.set_reg(Register::AX, v);
    Ok(Flow::Next)
}
