//! MMX and SSE, as the console's Pentium III has them.
//!
//! The Pentium III has no SSE2: every packed integer instruction works on
//! the MMX registers. An MMX opcode with a `66` prefix, which a decoder
//! reads as SSE2's form on XMM registers, runs as MMX on the processor (Dr.
//! Muto relies on it), so the integer instructions here take an XMM
//! register operand as the MMX register of the same number and a memory
//! operand as 8 bytes.
//!
//! MMX registers are the significands of the x87 physical registers. An
//! MMX instruction sets TOP to 0 and every tag to valid; a write sets the
//! register's exponent field to all ones.
//!
//! SSE arithmetic follows the processor's rules where they differ from the
//! host's: a NaN operand is returned quieted (the first one if both), an
//! invalid operation gives the default NaN `0xFFC00000`, MIN and MAX return
//! the second operand unless the comparison holds, and MXCSR's FTZ flushes
//! denormal results. `RCPPS` and `RSQRTPS` give the exact quotient where
//! the processor gives a 12-bit approximation.

use iced_x86::{Instruction, Mnemonic as M, OpKind, Register};
use xb_cpu::Bus;

use crate::cpu::{Flow, InterpCpu, Step, Stop};
use crate::f80::F80;
use crate::flags::{ARITH, CF, PF, ZF};

type X = [u32; 4];

const DEFAULT_NAN: u32 = 0xFFC0_0000;
const FTZ: u32 = 1 << 15;

fn quiet(v: u32) -> u32 {
    v | 0x0040_0000
}

fn is_nan(v: u32) -> bool {
    f32::from_bits(v).is_nan()
}

impl InterpCpu {
    // ---- MMX registers ----

    fn mmx_enter(&mut self) {
        self.fpu.top = 0;
        self.fpu.valid = 0xFF;
    }

    fn mm(&self, r: Register) -> u64 {
        self.fpu.st[mm_index(r)].f80().mant
    }

    fn set_mm(&mut self, r: Register, v: u64) {
        self.fpu.st[mm_index(r)] = crate::f80::Fpr::from_f80(F80 { mant: v, se: 0xFFFF });
    }

    /// Operand `i` of an MMX instruction as 64 bits.
    fn mmx_src(&mut self, ins: &Instruction, i: u32, bus: &mut dyn Bus) -> Result<u64, Stop> {
        match ins.op_kind(i) {
            OpKind::Register => {
                let r = ins.op_register(i);
                Ok(if is_gpr(r) { u64::from(self.reg(r)) } else { self.mm(r) })
            }
            OpKind::Memory => {
                let a = self.ea(ins);
                self.load64(a, bus)
            }
            _ => Ok(u64::from(ins.immediate8())),
        }
    }

    // ---- XMM registers ----

    fn xmm(&self, r: Register) -> X {
        self.xmm[xmm_index(r)]
    }

    fn set_xmm(&mut self, r: Register, v: X) {
        self.xmm[xmm_index(r)] = v;
    }

    /// Reads a 16-byte memory operand; `aligned` forms fault (`#GP`) on an
    /// address that is not a multiple of 16.
    fn load128(&mut self, a: u32, aligned: bool, bus: &mut dyn Bus) -> Result<X, Stop> {
        if aligned && a & 15 != 0 {
            return Err(Stop::Exc(13, 0));
        }
        let b = self.mem.read::<16>(a, bus)?;
        Ok(std::array::from_fn(|i| {
            u32::from_le_bytes([b[4 * i], b[4 * i + 1], b[4 * i + 2], b[4 * i + 3]])
        }))
    }

    fn store128(&mut self, a: u32, v: X, aligned: bool, bus: &mut dyn Bus) -> Result<(), Stop> {
        if aligned && a & 15 != 0 {
            return Err(Stop::Exc(13, 0));
        }
        let mut b = [0u8; 16];
        for (i, w) in v.iter().enumerate() {
            b[4 * i..4 * i + 4].copy_from_slice(&w.to_le_bytes());
        }
        self.mem.write(a, &b, bus)?;
        Ok(())
    }

    /// Operand `i` as a full XMM value (packed forms; memory is 16 bytes,
    /// aligned).
    fn xsrc(&mut self, ins: &Instruction, i: u32, bus: &mut dyn Bus) -> Result<X, Stop> {
        match ins.op_kind(i) {
            OpKind::Register => Ok(self.xmm(ins.op_register(i))),
            _ => {
                let a = self.ea(ins);
                self.load128(a, true, bus)
            }
        }
    }

    /// Operand `i` as a scalar single (memory is 4 bytes).
    fn ssrc(&mut self, ins: &Instruction, i: u32, bus: &mut dyn Bus) -> Result<u32, Stop> {
        match ins.op_kind(i) {
            OpKind::Register => Ok(self.xmm(ins.op_register(i))[0]),
            _ => {
                let a = self.ea(ins);
                self.load(a, 32, bus)
            }
        }
    }

    fn ftz(&self, v: u32) -> u32 {
        if self.mxcsr & FTZ != 0 && v & 0x7F80_0000 == 0 && v & 0x007F_FFFF != 0 {
            v & 0x8000_0000
        } else {
            v
        }
    }

    /// One SSE arithmetic operation with the processor's NaN rules.
    fn fop(&self, a: u32, b: u32, f: impl Fn(f32, f32) -> f32) -> u32 {
        if is_nan(a) {
            return quiet(a);
        }
        if is_nan(b) {
            return quiet(b);
        }
        let r = f(f32::from_bits(a), f32::from_bits(b)).to_bits();
        if is_nan(r) { DEFAULT_NAN } else { self.ftz(r) }
    }

    fn cvt_round(&self, x: f32, truncate: bool) -> u32 {
        if !x.is_finite() || x >= 2_147_483_648.0 || x < -2_147_483_648.0 {
            return 0x8000_0000;
        }
        let r = if truncate {
            x.trunc()
        } else {
            match (self.mxcsr >> 13) & 3 {
                0 => x.round_ties_even(),
                1 => x.floor(),
                2 => x.ceil(),
                _ => x.trunc(),
            }
        };
        r as i32 as u32
    }

    /// MMX and SSE instructions.
    pub(crate) fn step_simd(&mut self, ins: &Instruction, bus: &mut dyn Bus) -> Step {
        let mn = ins.mnemonic();
        if let Some(r) = self.mmx_int(ins, bus) {
            return r;
        }
        let next = Ok(Flow::Next);
        match mn {
            M::Emms => {
                self.fpu.valid = 0;
                next
            }
            // ---- moves ----
            M::Movaps | M::Movups | M::Movntps => {
                let aligned = mn != M::Movups;
                if ins.op_kind(0) == OpKind::Memory {
                    let v = self.xmm(ins.op_register(1));
                    let a = self.ea(ins);
                    self.store128(a, v, aligned, bus)?;
                } else {
                    let v = match ins.op_kind(1) {
                        OpKind::Register => self.xmm(ins.op_register(1)),
                        _ => {
                            let a = self.ea(ins);
                            self.load128(a, aligned, bus)?
                        }
                    };
                    self.set_xmm(ins.op_register(0), v);
                }
                next
            }
            M::Movss => {
                if ins.op_kind(0) == OpKind::Memory {
                    let v = self.xmm(ins.op_register(1))[0];
                    let a = self.ea(ins);
                    self.store(a, 32, v, bus)?;
                } else if ins.op_kind(1) == OpKind::Memory {
                    let a = self.ea(ins);
                    let v = self.load(a, 32, bus)?;
                    self.set_xmm(ins.op_register(0), [v, 0, 0, 0]);
                } else {
                    let s = self.xmm(ins.op_register(1))[0];
                    let mut d = self.xmm(ins.op_register(0));
                    d[0] = s;
                    self.set_xmm(ins.op_register(0), d);
                }
                next
            }
            M::Movlps | M::Movhps => {
                let hi = usize::from(mn == M::Movhps) * 2;
                if ins.op_kind(0) == OpKind::Memory {
                    let v = self.xmm(ins.op_register(1));
                    let a = self.ea(ins);
                    self.store64(a, u64::from(v[hi]) | u64::from(v[hi + 1]) << 32, bus)?;
                } else {
                    let a = self.ea(ins);
                    let m = self.load64(a, bus)?;
                    let mut d = self.xmm(ins.op_register(0));
                    d[hi] = m as u32;
                    d[hi + 1] = (m >> 32) as u32;
                    self.set_xmm(ins.op_register(0), d);
                }
                next
            }
            M::Movlhps | M::Movhlps => {
                let s = self.xmm(ins.op_register(1));
                let mut d = self.xmm(ins.op_register(0));
                if mn == M::Movlhps {
                    d[2] = s[0];
                    d[3] = s[1];
                } else {
                    d[0] = s[2];
                    d[1] = s[3];
                }
                self.set_xmm(ins.op_register(0), d);
                next
            }
            M::Movmskps => {
                let s = self.xmm(ins.op_register(1));
                let m = (0..4).fold(0, |m, i| m | (s[i] >> 31) << i);
                self.set_reg(ins.op_register(0), m);
                next
            }
            M::Ldmxcsr => {
                let v = self.rd(ins, 0, 32, bus)?;
                if v & !0xFFBF != 0 {
                    return Err(Stop::Exc(13, 0));
                }
                self.mxcsr = v;
                next
            }
            M::Stmxcsr => {
                self.wr(ins, 0, 32, self.mxcsr, bus)?;
                next
            }

            // ---- packed and scalar arithmetic ----
            M::Addps | M::Subps | M::Mulps | M::Divps | M::Minps | M::Maxps => {
                let b = self.xsrc(ins, 1, bus)?;
                let a = self.xmm(ins.op_register(0));
                let r: X = std::array::from_fn(|i| self.binop(mn, a[i], b[i]));
                self.set_xmm(ins.op_register(0), r);
                next
            }
            M::Addss | M::Subss | M::Mulss | M::Divss | M::Minss | M::Maxss => {
                let b = self.ssrc(ins, 1, bus)?;
                let mut a = self.xmm(ins.op_register(0));
                a[0] = self.binop(mn, a[0], b);
                self.set_xmm(ins.op_register(0), a);
                next
            }
            M::Sqrtps | M::Rcpps | M::Rsqrtps => {
                let b = self.xsrc(ins, 1, bus)?;
                let r: X = std::array::from_fn(|i| self.unop(mn, b[i]));
                self.set_xmm(ins.op_register(0), r);
                next
            }
            M::Sqrtss | M::Rcpss | M::Rsqrtss => {
                let b = self.ssrc(ins, 1, bus)?;
                let mut a = self.xmm(ins.op_register(0));
                a[0] = self.unop(mn, b);
                self.set_xmm(ins.op_register(0), a);
                next
            }
            M::Andps | M::Andnps | M::Orps | M::Xorps => {
                let b = self.xsrc(ins, 1, bus)?;
                let a = self.xmm(ins.op_register(0));
                let r: X = std::array::from_fn(|i| match mn {
                    M::Andps => a[i] & b[i],
                    M::Andnps => !a[i] & b[i],
                    M::Orps => a[i] | b[i],
                    _ => a[i] ^ b[i],
                });
                self.set_xmm(ins.op_register(0), r);
                next
            }
            M::Cmpps | M::Cmpss => {
                let imm = u32::from(ins.immediate8()) & 7;
                let a = self.xmm(ins.op_register(0));
                if mn == M::Cmpps {
                    let b = self.xsrc(ins, 1, bus)?;
                    let r: X = std::array::from_fn(|i| cmp(imm, a[i], b[i]));
                    self.set_xmm(ins.op_register(0), r);
                } else {
                    let b = self.ssrc(ins, 1, bus)?;
                    let mut r = a;
                    r[0] = cmp(imm, a[0], b);
                    self.set_xmm(ins.op_register(0), r);
                }
                next
            }
            M::Comiss | M::Ucomiss => {
                let a = f32::from_bits(self.xmm(ins.op_register(0))[0]);
                let b = f32::from_bits(self.ssrc(ins, 1, bus)?);
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
                next
            }
            M::Shufps => {
                let b = self.xsrc(ins, 1, bus)?;
                let a = self.xmm(ins.op_register(0));
                let imm = u32::from(ins.immediate8());
                let sel = |s: &X, k: u32| s[((imm >> (2 * k)) & 3) as usize];
                self.set_xmm(ins.op_register(0), [sel(&a, 0), sel(&a, 1), sel(&b, 2), sel(&b, 3)]);
                next
            }
            M::Unpcklps | M::Unpckhps => {
                let b = self.xsrc(ins, 1, bus)?;
                let a = self.xmm(ins.op_register(0));
                let r = if mn == M::Unpcklps {
                    [a[0], b[0], a[1], b[1]]
                } else {
                    [a[2], b[2], a[3], b[3]]
                };
                self.set_xmm(ins.op_register(0), r);
                next
            }

            // ---- conversions ----
            M::Cvtpi2ps => {
                let s = self.mmx_src(ins, 1, bus)?;
                if ins.op_kind(1) == OpKind::Register {
                    self.mmx_enter();
                }
                let mut d = self.xmm(ins.op_register(0));
                d[0] = (s as u32 as i32 as f32).to_bits();
                d[1] = ((s >> 32) as u32 as i32 as f32).to_bits();
                self.set_xmm(ins.op_register(0), d);
                next
            }
            M::Cvtps2pi | M::Cvttps2pi => {
                let s = match ins.op_kind(1) {
                    OpKind::Register => {
                        let x = self.xmm(ins.op_register(1));
                        u64::from(x[0]) | u64::from(x[1]) << 32
                    }
                    _ => {
                        let a = self.ea(ins);
                        self.load64(a, bus)?
                    }
                };
                let t = mn == M::Cvttps2pi;
                let lo = self.cvt_round(f32::from_bits(s as u32), t);
                let hi = self.cvt_round(f32::from_bits((s >> 32) as u32), t);
                self.mmx_enter();
                self.set_mm(ins.op_register(0), u64::from(lo) | u64::from(hi) << 32);
                next
            }
            M::Cvtsi2ss => {
                let v = self.rd(ins, 1, 32, bus)?;
                let mut d = self.xmm(ins.op_register(0));
                d[0] = (v as i32 as f32).to_bits();
                self.set_xmm(ins.op_register(0), d);
                next
            }
            M::Cvtss2si | M::Cvttss2si => {
                let s = f32::from_bits(self.ssrc(ins, 1, bus)?);
                let v = self.cvt_round(s, mn == M::Cvttss2si);
                self.set_reg(ins.op_register(0), v);
                next
            }
            _ => Err(self.unimplemented(ins)),
        }
    }

    fn binop(&self, mn: M, a: u32, b: u32) -> u32 {
        match mn {
            M::Addps | M::Addss => self.fop(a, b, |x, y| x + y),
            M::Subps | M::Subss => self.fop(a, b, |x, y| x - y),
            M::Mulps | M::Mulss => self.fop(a, b, |x, y| x * y),
            M::Divps | M::Divss => self.fop(a, b, |x, y| x / y),
            M::Minps | M::Minss => {
                if f32::from_bits(a) < f32::from_bits(b) {
                    a
                } else {
                    b
                }
            }
            _ => {
                if f32::from_bits(a) > f32::from_bits(b) {
                    a
                } else {
                    b
                }
            }
        }
    }

    fn unop(&self, mn: M, b: u32) -> u32 {
        if is_nan(b) {
            return quiet(b);
        }
        let x = f32::from_bits(b);
        let r = match mn {
            M::Sqrtps | M::Sqrtss => x.sqrt(),
            M::Rcpps | M::Rcpss => 1.0 / x,
            _ => 1.0 / x.sqrt(),
        }
        .to_bits();
        if is_nan(r) { DEFAULT_NAN } else { self.ftz(r) }
    }

    /// Packed integer (MMX) instructions; `None` when `ins` is not one.
    fn mmx_int(&mut self, ins: &Instruction, bus: &mut dyn Bus) -> Option<Step> {
        let mn = ins.mnemonic();
        let lane_op: Option<fn(u64, u64) -> u64> = match mn {
            M::Paddb => Some(|a, b| lanes8(a, b, |x, y| x.wrapping_add(y))),
            M::Paddw => Some(|a, b| lanes16(a, b, |x, y| x.wrapping_add(y))),
            M::Paddd => Some(|a, b| lanes32(a, b, |x, y| x.wrapping_add(y))),
            M::Paddsb => Some(|a, b| lanes8(a, b, |x, y| (x as i8).saturating_add(y as i8) as u8)),
            M::Paddsw => {
                Some(|a, b| lanes16(a, b, |x, y| (x as i16).saturating_add(y as i16) as u16))
            }
            M::Paddusb => Some(|a, b| lanes8(a, b, u8::saturating_add)),
            M::Paddusw => Some(|a, b| lanes16(a, b, u16::saturating_add)),
            M::Psubb => Some(|a, b| lanes8(a, b, |x, y| x.wrapping_sub(y))),
            M::Psubw => Some(|a, b| lanes16(a, b, |x, y| x.wrapping_sub(y))),
            M::Psubd => Some(|a, b| lanes32(a, b, |x, y| x.wrapping_sub(y))),
            M::Psubsb => Some(|a, b| lanes8(a, b, |x, y| (x as i8).saturating_sub(y as i8) as u8)),
            M::Psubsw => {
                Some(|a, b| lanes16(a, b, |x, y| (x as i16).saturating_sub(y as i16) as u16))
            }
            M::Psubusb => Some(|a, b| lanes8(a, b, u8::saturating_sub)),
            M::Psubusw => Some(|a, b| lanes16(a, b, u16::saturating_sub)),
            M::Pand => Some(|a, b| a & b),
            M::Pandn => Some(|a, b| !a & b),
            M::Por => Some(|a, b| a | b),
            M::Pxor => Some(|a, b| a ^ b),
            M::Pcmpeqb => Some(|a, b| lanes8(a, b, |x, y| if x == y { 0xFF } else { 0 })),
            M::Pcmpeqw => Some(|a, b| lanes16(a, b, |x, y| if x == y { 0xFFFF } else { 0 })),
            M::Pcmpeqd => Some(|a, b| lanes32(a, b, |x, y| if x == y { u32::MAX } else { 0 })),
            M::Pcmpgtb => {
                Some(|a, b| lanes8(a, b, |x, y| if (x as i8) > (y as i8) { 0xFF } else { 0 }))
            }
            M::Pcmpgtw => {
                Some(|a, b| lanes16(a, b, |x, y| if (x as i16) > (y as i16) { 0xFFFF } else { 0 }))
            }
            M::Pcmpgtd => Some(|a, b| {
                lanes32(a, b, |x, y| if (x as i32) > (y as i32) { u32::MAX } else { 0 })
            }),
            M::Pmullw => Some(|a, b| lanes16(a, b, |x, y| x.wrapping_mul(y))),
            M::Pmulhw => Some(|a, b| {
                lanes16(a, b, |x, y| ((i32::from(x as i16) * i32::from(y as i16)) >> 16) as u16)
            }),
            M::Pmulhuw => {
                Some(|a, b| lanes16(a, b, |x, y| ((u32::from(x) * u32::from(y)) >> 16) as u16))
            }
            M::Pmaddwd => Some(|a, b| {
                let w = |v: u64, i: u32| i32::from((v >> (16 * i)) as u16 as i16);
                let lo = (w(a, 0) * w(b, 0)).wrapping_add(w(a, 1) * w(b, 1)) as u32;
                let hi = (w(a, 2) * w(b, 2)).wrapping_add(w(a, 3) * w(b, 3)) as u32;
                u64::from(lo) | u64::from(hi) << 32
            }),
            M::Pavgb => {
                Some(|a, b| lanes8(a, b, |x, y| ((u16::from(x) + u16::from(y) + 1) >> 1) as u8))
            }
            M::Pavgw => {
                Some(|a, b| lanes16(a, b, |x, y| ((u32::from(x) + u32::from(y) + 1) >> 1) as u16))
            }
            M::Pmaxsw => Some(|a, b| lanes16(a, b, |x, y| (x as i16).max(y as i16) as u16)),
            M::Pminsw => Some(|a, b| lanes16(a, b, |x, y| (x as i16).min(y as i16) as u16)),
            M::Pmaxub => Some(|a, b| lanes8(a, b, u8::max)),
            M::Pminub => Some(|a, b| lanes8(a, b, u8::min)),
            M::Psadbw => Some(|a, b| {
                (0..8)
                    .map(|i| u64::from(((a >> (8 * i)) as u8).abs_diff((b >> (8 * i)) as u8)))
                    .sum()
            }),
            M::Punpcklbw => Some(|a, b| interleave8(a, b, 0)),
            M::Punpckhbw => Some(|a, b| interleave8(a, b, 32)),
            M::Punpcklwd => Some(|a, b| interleave16(a, b, 0)),
            M::Punpckhwd => Some(|a, b| interleave16(a, b, 32)),
            M::Punpckldq => Some(|a, b| (a & 0xFFFF_FFFF) | (b << 32)),
            M::Punpckhdq => Some(|a, b| (a >> 32) | (b & 0xFFFF_FFFF_0000_0000)),
            M::Packsswb => Some(|a, b| pack(a, b, |w| (w as i16).clamp(-128, 127) as u8)),
            M::Packuswb => Some(|a, b| pack(a, b, |w| (w as i16).clamp(0, 255) as u8)),
            M::Packssdw => Some(|a, b| {
                let s = |v: u32| (v as i32).clamp(-32768, 32767) as u16 as u64;
                s(a as u32)
                    | s((a >> 32) as u32) << 16
                    | s(b as u32) << 32
                    | s((b >> 32) as u32) << 48
            }),
            _ => None,
        };
        if let Some(f) = lane_op {
            return Some(self.mmx_binary(ins, bus, f));
        }
        let shift: Option<(u32, u8)> = match mn {
            M::Psllw => Some((16, 0)),
            M::Pslld => Some((32, 0)),
            M::Psllq => Some((64, 0)),
            M::Psrlw => Some((16, 1)),
            M::Psrld => Some((32, 1)),
            M::Psrlq => Some((64, 1)),
            M::Psraw => Some((16, 2)),
            M::Psrad => Some((32, 2)),
            _ => None,
        };
        if let Some((w, kind)) = shift {
            return Some((|| {
                let c = self.mmx_src(ins, 1, bus)?;
                let r0 = ins.op_register(0);
                self.mmx_enter();
                let a = self.mm(r0);
                self.set_mm(r0, shift_lanes(a, c, w, kind));
                Ok(Flow::Next)
            })());
        }
        if !matches!(
            mn,
            M::Movd
                | M::Movq
                | M::Movntq
                | M::Pshufw
                | M::Pextrw
                | M::Pinsrw
                | M::Pmovmskb
                | M::Maskmovq
        ) {
            return None;
        }
        let r: Step = (|| match mn {
            M::Movd => {
                if ins.op_kind(0) == OpKind::Register && !is_gpr(ins.op_register(0)) {
                    let v = self.rd(ins, 1, 32, bus)?;
                    self.mmx_enter();
                    self.set_mm(ins.op_register(0), u64::from(v));
                } else {
                    let v = self.mm(ins.op_register(1)) as u32;
                    self.mmx_enter();
                    self.wr(ins, 0, 32, v, bus)?;
                }
                Ok(Flow::Next)
            }
            M::Movq | M::Movntq => {
                if ins.op_kind(0) == OpKind::Memory {
                    let v = self.mm(ins.op_register(1));
                    let a = self.ea(ins);
                    self.store64(a, v, bus)?;
                    self.mmx_enter();
                } else {
                    let v = self.mmx_src(ins, 1, bus)?;
                    self.mmx_enter();
                    self.set_mm(ins.op_register(0), v);
                }
                Ok(Flow::Next)
            }
            M::Pshufw => {
                let s = self.mmx_src(ins, 1, bus)?;
                let imm = u32::from(ins.immediate8());
                let r = (0..4).fold(0u64, |r, k| {
                    let sel = (imm >> (2 * k)) & 3;
                    r | ((s >> (16 * sel)) & 0xFFFF) << (16 * k)
                });
                self.mmx_enter();
                self.set_mm(ins.op_register(0), r);
                Ok(Flow::Next)
            }
            M::Pextrw => {
                let s = self.mm(ins.op_register(1));
                let k = u32::from(ins.immediate8()) & 3;
                self.mmx_enter();
                self.set_reg(ins.op_register(0), ((s >> (16 * k)) & 0xFFFF) as u32);
                Ok(Flow::Next)
            }
            M::Pinsrw => {
                let v = u64::from(self.rd(ins, 1, 16, bus)? & 0xFFFF);
                let k = u32::from(ins.immediate8()) & 3;
                self.mmx_enter();
                let d = self.mm(ins.op_register(0));
                let d = (d & !(0xFFFF << (16 * k))) | v << (16 * k);
                self.set_mm(ins.op_register(0), d);
                Ok(Flow::Next)
            }
            M::Pmovmskb => {
                let s = self.mm(ins.op_register(1));
                let m = (0..8).fold(0, |m, i| m | (((s >> (8 * i + 7)) & 1) as u32) << i);
                self.mmx_enter();
                self.set_reg(ins.op_register(0), m);
                Ok(Flow::Next)
            }
            M::Maskmovq => {
                let v = self.mm(ins.op_register(0));
                let m = self.mm(ins.op_register(1));
                let mut a = self.gpr[7];
                if ins.segment_prefix() == Register::FS {
                    a = a.wrapping_add(self.fs_base);
                }
                for i in 0..8 {
                    if (m >> (8 * i + 7)) & 1 != 0 {
                        self.store(a.wrapping_add(i), 8, (v >> (8 * i)) as u32, bus)?;
                    }
                }
                self.mmx_enter();
                Ok(Flow::Next)
            }
            _ => unreachable!("checked by is_mmx_other"),
        })();
        Some(r)
    }

    fn mmx_binary(&mut self, ins: &Instruction, bus: &mut dyn Bus, f: fn(u64, u64) -> u64) -> Step {
        let b = self.mmx_src(ins, 1, bus)?;
        self.mmx_enter();
        let r0 = ins.op_register(0);
        let a = self.mm(r0);
        self.set_mm(r0, f(a, b));
        Ok(Flow::Next)
    }
}

/// The comparison predicates of CMPPS (EQ, LT, LE, UNORD, NEQ, NLT, NLE, ORD).
fn cmp(imm: u32, a: u32, b: u32) -> u32 {
    let (x, y) = (f32::from_bits(a), f32::from_bits(b));
    let un = x.is_nan() || y.is_nan();
    let t = match imm {
        0 => x == y,
        1 => x < y,
        2 => x <= y,
        3 => un,
        4 => x != y || un,
        5 => x >= y || un,
        6 => x > y || un,
        _ => !un,
    };
    if t { u32::MAX } else { 0 }
}

fn is_gpr(r: Register) -> bool {
    (1..=44).contains(&(r as u32))
}

/// MM0-7, or XMM0-7 taken as MM0-7 (module docs).
fn mm_index(r: Register) -> usize {
    let n = r as u32;
    if (Register::XMM0 as u32..=Register::XMM7 as u32).contains(&n) {
        (n - Register::XMM0 as u32) as usize
    } else {
        (n.wrapping_sub(Register::MM0 as u32) & 7) as usize
    }
}

fn xmm_index(r: Register) -> usize {
    ((r as u32).wrapping_sub(Register::XMM0 as u32) & 7) as usize
}

fn lanes8(a: u64, b: u64, f: impl Fn(u8, u8) -> u8) -> u64 {
    (0..8).fold(0, |r, i| r | u64::from(f((a >> (8 * i)) as u8, (b >> (8 * i)) as u8)) << (8 * i))
}

fn lanes16(a: u64, b: u64, f: impl Fn(u16, u16) -> u16) -> u64 {
    (0..4).fold(0, |r, i| {
        r | u64::from(f((a >> (16 * i)) as u16, (b >> (16 * i)) as u16)) << (16 * i)
    })
}

fn lanes32(a: u64, b: u64, f: impl Fn(u32, u32) -> u32) -> u64 {
    (0..2).fold(0, |r, i| {
        r | u64::from(f((a >> (32 * i)) as u32, (b >> (32 * i)) as u32)) << (32 * i)
    })
}

fn interleave8(a: u64, b: u64, from: u32) -> u64 {
    (0..4).fold(0, |r, i| {
        let x = (a >> (from + 8 * i)) & 0xFF;
        let y = (b >> (from + 8 * i)) & 0xFF;
        r | x << (16 * i) | y << (16 * i + 8)
    })
}

fn interleave16(a: u64, b: u64, from: u32) -> u64 {
    (0..2).fold(0, |r, i| {
        let x = (a >> (from + 16 * i)) & 0xFFFF;
        let y = (b >> (from + 16 * i)) & 0xFFFF;
        r | x << (32 * i) | y << (32 * i + 16)
    })
}

fn pack(a: u64, b: u64, f: impl Fn(u16) -> u8) -> u64 {
    (0..8).fold(0, |r, i| {
        let src = if i < 4 { a } else { b };
        let w = (src >> (16 * (i % 4))) as u16;
        r | u64::from(f(w)) << (8 * i)
    })
}

/// Shifts each `w`-bit lane of `a` by `c`: kind 0 left, 1 logical right,
/// 2 arithmetic right. Counts at or above the lane width give 0 (or the
/// sign for arithmetic shifts).
fn shift_lanes(a: u64, c: u64, w: u32, kind: u8) -> u64 {
    if w == 64 {
        return match kind {
            0 if c < 64 => a << c,
            1 if c < 64 => a >> c,
            _ => 0,
        };
    }
    let n = 64 / w;
    let m = (1u64 << w) - 1;
    (0..n).fold(0, |r, i| {
        let x = (a >> (w * i)) & m;
        let v = match kind {
            0 => {
                if c >= u64::from(w) {
                    0
                } else {
                    (x << c) & m
                }
            }
            1 => {
                if c >= u64::from(w) {
                    0
                } else {
                    x >> c
                }
            }
            _ => {
                let s = (x << (64 - w)) as i64 >> (64 - w);
                (s >> c.min(u64::from(w) - 1)) as u64 & m
            }
        };
        r | v << (w * i)
    })
}
