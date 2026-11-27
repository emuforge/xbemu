//! EFLAGS, with the arithmetic flags computed when they are read.
//!
//! Most instructions set CF, PF, AF, ZF, SF and OF and most of those values
//! are never read: the next instruction sets them again. The interpreter
//! therefore records the last flag-setting operation (kind, size, operands,
//! result) and computes a flag only when an instruction asks for it.
//! The 32-bit shifts by a constant and the two- and three-operand `IMUL`
//! are recorded the same way, so compiled code records them with a few
//! stores; instructions whose flags are awkward to derive afterwards
//! (rotates, shifts by `CL`, the other multiplies, BCD adjusts) set them at
//! once with [`Flags::set_arith`].
//!
//! The values of flags the manuals call undefined follow what a Pentium III
//! leaves for the cases tests compare (logic operations clear AF); the
//! backend comparison masks the rest.

/// The arithmetic flags.
pub(crate) const CF: u32 = 1 << 0;
pub(crate) const PF: u32 = 1 << 2;
pub(crate) const AF: u32 = 1 << 4;
pub(crate) const ZF: u32 = 1 << 6;
pub(crate) const SF: u32 = 1 << 7;
pub(crate) const OF: u32 = 1 << 11;
/// Trap, interrupt, direction.
pub(crate) const TF: u32 = 1 << 8;
pub(crate) const IF: u32 = 1 << 9;
pub(crate) const DF: u32 = 1 << 10;
/// The six arithmetic flags.
pub(crate) const ARITH: u32 = CF | PF | AF | ZF | SF | OF;
/// Bits POPFD may change in ring 0 on the console's processor: the
/// arithmetic flags, TF, IF, DF, IOPL, NT, AC and ID (VM and RF are not
/// set by POPF).
pub(crate) const POPF_MASK: u32 = ARITH | TF | IF | DF | 0x3000 | 0x4000 | 0x4_0000 | 0x20_0000;

/// What produced the arithmetic flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Op {
    /// They are stored in [`Flags::bits`]. Generated code tests for this
    /// value as zero.
    Stored = 0,
    /// AND, OR, XOR, TEST: CF = OF = AF = 0.
    Logic,
    /// `res = a + b`.
    Add,
    /// `res = a - b` (SUB, CMP).
    Sub,
    /// `res = a + 1`, CF kept.
    Inc,
    /// `res = a - 1`, CF kept.
    Dec,
    /// `res = a << b`, `b` 1 to 31, 32 bits: CF is the last bit shifted
    /// out, OF is CF xor the result's sign, AF is 0.
    Shl,
    /// `res = a >> b` (logical), `b` 1 to 31, 32 bits: CF is the last bit
    /// shifted out, OF is `a`'s sign, AF is 0.
    Shr,
    /// `res = a >> b` (arithmetic), `b` 1 to 31, 32 bits: CF is the last
    /// bit shifted out, OF and AF are 0.
    Sar,
    /// A truncated signed multiply: `res` is the result and `a` is 1 if it
    /// overflowed (CF = OF = `a`), AF is 0.
    Mul,
}

/// EFLAGS of the running context.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Flags {
    /// Every bit when `op` is [`Op::Stored`]; otherwise every bit but the
    /// arithmetic ones, except CF after INC and DEC, which they keep.
    pub(crate) bits: u32,
    pub(crate) op: Op,
    /// Operand size in bits (8, 16 or 32).
    pub(crate) size: u32,
    pub(crate) a: u32,
    pub(crate) b: u32,
    pub(crate) res: u32,
}

#[inline]
fn mask(size: u32) -> u32 {
    if size == 32 { u32::MAX } else { (1 << size) - 1 }
}

#[inline]
fn sign(size: u32) -> u32 {
    1 << (size - 1)
}

#[inline]
fn parity(v: u32) -> u32 {
    if (v as u8).count_ones().is_multiple_of(2) { PF } else { 0 }
}

impl Flags {
    pub(crate) fn new(bits: u32) -> Self {
        Self { bits, op: Op::Stored, size: 32, a: 0, b: 0, res: 0 }
    }

    /// Records an operation; `res` is the full-width result.
    #[inline]
    pub(crate) fn lazy(&mut self, op: Op, size: u32, a: u32, b: u32, res: u32) {
        if matches!(op, Op::Inc | Op::Dec) {
            let cf = self.cf();
            self.bits = (self.bits & !ARITH) | cf;
        } else if self.op == Op::Stored {
            self.bits &= !ARITH;
        }
        self.op = op;
        self.size = size;
        self.a = a;
        self.b = b;
        self.res = res;
    }

    /// Sets the arithmetic flags in `which` to those in `values`, keeping
    /// the others.
    #[inline]
    pub(crate) fn set_arith(&mut self, which: u32, values: u32) {
        let all = self.get();
        self.bits = (all & !which) | (values & which);
        self.op = Op::Stored;
    }

    /// The whole register.
    #[inline]
    pub(crate) fn get(&self) -> u32 {
        if self.op == Op::Stored {
            return self.bits;
        }
        self.bits & !ARITH | self.compute()
    }

    /// Replaces the whole register.
    pub(crate) fn set(&mut self, bits: u32) {
        self.bits = bits;
        self.op = Op::Stored;
    }

    fn compute(&self) -> u32 {
        let m = mask(self.size);
        let s = sign(self.size);
        let (a, b, r) = (self.a & m, self.b & m, self.res & m);
        let mut f = parity(r);
        if r == 0 {
            f |= ZF;
        }
        if r & s != 0 {
            f |= SF;
        }
        match self.op {
            Op::Stored | Op::Logic => {}
            Op::Add => {
                if r < a {
                    f |= CF;
                }
                if (a ^ r) & (b ^ r) & s != 0 {
                    f |= OF;
                }
                f |= (a ^ b ^ r) & AF;
            }
            Op::Sub => {
                if a < b {
                    f |= CF;
                }
                if (a ^ b) & (a ^ r) & s != 0 {
                    f |= OF;
                }
                f |= (a ^ b ^ r) & AF;
            }
            Op::Inc => {
                f |= self.bits & CF;
                if r == s {
                    f |= OF;
                }
                if r & 0xF == 0 {
                    f |= AF;
                }
            }
            Op::Dec => {
                f |= self.bits & CF;
                if a == s {
                    f |= OF;
                }
                if a & 0xF == 0 {
                    f |= AF;
                }
            }
            Op::Shl | Op::Shr | Op::Sar => {
                let cf = self.cf();
                f |= cf;
                let of = match self.op {
                    Op::Shl => (r >> 31) ^ cf,
                    Op::Shr => a >> 31,
                    _ => 0,
                };
                if of != 0 {
                    f |= OF;
                }
            }
            Op::Mul => {
                if a != 0 {
                    f |= CF | OF;
                }
            }
        }
        f
    }

    /// Carry.
    #[inline]
    pub(crate) fn cf(&self) -> u32 {
        match self.op {
            Op::Stored | Op::Inc | Op::Dec => self.bits & CF,
            Op::Logic => 0,
            Op::Add => {
                let m = mask(self.size);
                u32::from(self.res & m < self.a & m)
            }
            Op::Sub => {
                let m = mask(self.size);
                u32::from(self.a & m < self.b & m)
            }
            Op::Shl => (self.a >> (32 - self.b)) & 1,
            Op::Shr => (self.a >> (self.b - 1)) & 1,
            Op::Sar => ((self.a as i32) >> (self.b - 1)) as u32 & 1,
            Op::Mul => self.a & 1,
        }
    }

    /// Zero.
    #[inline]
    pub(crate) fn zf(&self) -> bool {
        if self.op == Op::Stored { self.bits & ZF != 0 } else { self.res & mask(self.size) == 0 }
    }

    /// Evaluates condition code `cc` (the low nibble of `Jcc`'s opcode).
    #[inline]
    pub(crate) fn cond(&self, cc: u32) -> bool {
        // The two most common cases without computing every flag.
        let r = match cc >> 1 {
            1 if self.op == Op::Sub || self.op == Op::Add => self.cf() != 0,
            2 => self.zf(),
            _ => {
                let f = self.get();
                let cf = f & CF != 0;
                let zf = f & ZF != 0;
                let sf = f & SF != 0;
                let of = f & OF != 0;
                match cc >> 1 {
                    0 => of,
                    1 => cf,
                    3 => cf || zf,
                    4 => sf,
                    5 => f & PF != 0,
                    6 => sf != of,
                    _ => zf || sf != of,
                }
            }
        };
        r != (cc & 1 != 0)
    }
}

/// Flags of a result for [`Flags::set_arith`]: ZF, SF and PF.
#[inline]
pub(crate) fn szp(res: u32, size: u32) -> u32 {
    let r = res & mask(size);
    let mut f = parity(r);
    if r == 0 {
        f |= ZF;
    }
    if r & sign(size) != 0 {
        f |= SF;
    }
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lazy_subtraction_gives_the_flags_of_the_eager_formula() {
        for &(a, b) in &[(0u32, 1u32), (5, 5), (0x80, 1), (0x7F, 0xFF), (3, 0x80)] {
            let mut f = Flags::new(2);
            let r = a.wrapping_sub(b);
            f.lazy(Op::Sub, 8, a, b, r);
            let (a8, b8) = (a as u8, b as u8);
            let (r8, borrow) = a8.overflowing_sub(b8);
            let (_, over) = (a8 as i8).overflowing_sub(b8 as i8);
            assert_eq!(f.get() & CF != 0, borrow, "{a:#x}-{b:#x}");
            assert_eq!(f.get() & OF != 0, over, "{a:#x}-{b:#x}");
            assert_eq!(f.get() & ZF != 0, r8 == 0);
            assert_eq!(f.get() & SF != 0, r8 & 0x80 != 0);
        }
    }

    #[test]
    fn inc_and_dec_keep_the_carry_of_the_operation_before() {
        let mut f = Flags::new(2);
        f.lazy(Op::Sub, 32, 0, 1, u32::MAX);
        f.lazy(Op::Inc, 32, u32::MAX, 1, 0);
        assert_eq!(f.get() & (CF | ZF), CF | ZF);
        f.lazy(Op::Logic, 32, 0, 0, 1);
        f.lazy(Op::Dec, 32, 1, 1, 0);
        assert_eq!(f.get() & CF, 0);
    }

    #[test]
    fn every_condition_agrees_with_the_materialized_flags() {
        let mut f = Flags::new(2);
        for &(a, b) in &[(1u32, 2u32), (2, 1), (0x8000_0000, 1), (7, 7)] {
            f.lazy(Op::Sub, 32, a, b, a.wrapping_sub(b));
            let full = Flags::new(f.get());
            for cc in 0..16 {
                assert_eq!(f.cond(cc), full.cond(cc), "cc {cc} after {a}-{b}");
            }
        }
    }
}
