//! The x87 register format: 80-bit extended precision.
//!
//! Registers are kept in this format so that what passes through them
//! unchanged comes out bit for bit: `FLD m80`/`FSTP m80`, `FILD m64`/`FISTP
//! m64` (an 8-byte copy idiom) and MMX, which aliases the significands.
//! Arithmetic converts to `f64`, computes and converts back. The console's
//! kernel runs every thread at 53-bit precision (control word `0x027F`), so
//! an `f64` result has the significand the processor would produce; what
//! differs is the exponent range of intermediate values, which titles do not
//! reach in practice.

/// One 80-bit value: sign and exponent in `se`, the significand (with its
/// explicit integer bit) in `mant`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct F80 {
    pub(crate) mant: u64,
    pub(crate) se: u16,
}

const BIAS: i32 = 16383;
const INT_BIT: u64 = 1 << 63;

/// One x87 register as the interpreter holds it: the value as a double,
/// which arithmetic reads directly, and the exact 80-bit bits when the value
/// arrived unchanged (`FLD m80`, `FILD m64`, an MMX write), so that storing
/// it back gives the same bits.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Fpr {
    pub(crate) f: f64,
    exact: Option<F80>,
}

impl Fpr {
    /// A result: its 80-bit form is the double's.
    #[inline]
    pub(crate) fn from_f64(f: f64) -> Self {
        Self { f, exact: None }
    }

    /// A value that arrived as these 80 bits.
    pub(crate) fn from_f80(x: F80) -> Self {
        Self { f: x.to_f64(), exact: Some(x) }
    }

    /// The register's 80-bit contents.
    pub(crate) fn f80(self) -> F80 {
        self.exact.unwrap_or_else(|| F80::from_f64(self.f))
    }

    /// The bits when exact, for conversions that must not lose them.
    pub(crate) fn exact(self) -> Option<F80> {
        self.exact
    }

    /// The value with its sign flipped (exact bits kept).
    pub(crate) fn neg(self) -> Self {
        Self { f: -self.f, exact: self.exact.map(F80::neg) }
    }

    /// The value with its sign cleared (exact bits kept).
    pub(crate) fn abs(self) -> Self {
        Self { f: self.f.abs(), exact: self.exact.map(F80::abs) }
    }
}

/// The x87 rounding modes (control word bits 10 and 11).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Round {
    Nearest,
    Down,
    Up,
    Zero,
}

impl Round {
    pub(crate) fn from_bits(rc: u32) -> Self {
        match rc & 3 {
            0 => Self::Nearest,
            1 => Self::Down,
            2 => Self::Up,
            _ => Self::Zero,
        }
    }
}

impl F80 {
    /// The QNaN floating-point indefinite.
    pub(crate) const INDEFINITE: Self = Self { mant: 0xC000_0000_0000_0000, se: 0xFFFF };

    /// From the 10 bytes of memory format.
    pub(crate) fn from_bytes(b: &[u8]) -> Self {
        let mut m = [0u8; 8];
        m.copy_from_slice(&b[..8]);
        Self { mant: u64::from_le_bytes(m), se: u16::from_le_bytes([b[8], b[9]]) }
    }

    /// The 10 bytes of memory format.
    pub(crate) fn to_bytes(self) -> [u8; 10] {
        let mut b = [0u8; 10];
        b[..8].copy_from_slice(&self.mant.to_le_bytes());
        b[8..].copy_from_slice(&self.se.to_le_bytes());
        b
    }

    pub(crate) fn sign(self) -> bool {
        self.se & 0x8000 != 0
    }

    fn exp(self) -> i32 {
        i32::from(self.se & 0x7FFF)
    }

    pub(crate) fn is_nan(self) -> bool {
        self.exp() == 0x7FFF && self.mant << 1 != 0
    }

    pub(crate) fn is_inf(self) -> bool {
        self.exp() == 0x7FFF && self.mant << 1 == 0
    }

    pub(crate) fn is_zero(self) -> bool {
        self.mant == 0 && self.exp() != 0x7FFF
    }

    /// Exponent 0 with a non-zero significand.
    pub(crate) fn is_denormal(self) -> bool {
        self.exp() == 0 && self.mant != 0
    }

    /// Exact.
    pub(crate) fn from_f64(x: f64) -> Self {
        let bits = x.to_bits();
        let sign = ((bits >> 63) as u16) << 15;
        let e = ((bits >> 52) & 0x7FF) as i32;
        let frac = bits & ((1 << 52) - 1);
        match e {
            0 if frac == 0 => Self { mant: 0, se: sign },
            0 => {
                let lz = frac.leading_zeros() as i32;
                Self { mant: frac << lz, se: sign | (15372 - lz) as u16 }
            }
            0x7FF if frac == 0 => Self { mant: INT_BIT, se: sign | 0x7FFF },
            0x7FF => Self { mant: INT_BIT | (1 << 62) | (frac << 11), se: sign | 0x7FFF },
            _ => Self { mant: INT_BIT | (frac << 11), se: sign | (e - 1023 + BIAS) as u16 },
        }
    }

    /// Rounded to nearest, ties to even.
    pub(crate) fn to_f64(self) -> f64 {
        let sign = u64::from(self.sign()) << 63;
        let mut e = self.exp();
        let mut m = self.mant;
        if e == 0x7FFF {
            if m << 1 == 0 {
                return f64::from_bits(sign | 0x7FF0_0000_0000_0000);
            }
            return f64::from_bits(
                sign | 0x7FF0_0000_0000_0000 | (1 << 51) | ((m >> 11) & ((1 << 52) - 1)),
            );
        }
        if m == 0 {
            return f64::from_bits(sign);
        }
        if e == 0 {
            e = 1;
        }
        let lz = m.leading_zeros() as i32;
        m <<= lz;
        e -= lz;
        // value = m * 2^(e - BIAS - 63), m in [2^63, 2^64)
        let big = e - BIAS + 1023;
        if big >= 2047 {
            return f64::from_bits(sign | 0x7FF0_0000_0000_0000);
        }
        let shift = if big >= 1 { 11 } else { 11 + (1 - big) } as u32;
        if shift > 64 {
            // Below a quarter of the smallest subnormal: rounds to zero.
            return f64::from_bits(sign);
        }
        let m = u128::from(m);
        let q = (m >> shift) as u64;
        let rem = m & ((1u128 << shift) - 1);
        let half = 1u128 << (shift - 1);
        let q = if rem > half || (rem == half && q & 1 == 1) { q + 1 } else { q };
        if big >= 1 {
            let (q, big) = if q == 1 << 53 { (q >> 1, big + 1) } else { (q, big) };
            if big >= 2047 {
                return f64::from_bits(sign | 0x7FF0_0000_0000_0000);
            }
            f64::from_bits(sign | (big as u64) << 52 | (q & ((1 << 52) - 1)))
        } else {
            f64::from_bits(sign | q)
        }
    }

    /// Exact.
    pub(crate) fn from_i64(v: i64) -> Self {
        if v == 0 {
            return Self { mant: 0, se: 0 };
        }
        let sign = if v < 0 { 0x8000 } else { 0 };
        let a = v.unsigned_abs();
        let lz = a.leading_zeros() as i32;
        Self { mant: a << lz, se: sign | (BIAS + 63 - lz) as u16 }
    }

    /// Rounded to an integer in mode `rc`; `None` for NaN and infinity.
    pub(crate) fn to_int(self, rc: Round) -> Option<i128> {
        if self.exp() == 0x7FFF {
            return None;
        }
        if self.mant == 0 {
            return Some(0);
        }
        let e = self.exp().max(1) - BIAS;
        let neg = self.sign();
        let (int, frac_nonzero, half, above_half): (u128, bool, bool, bool) = if e >= 63 {
            if e > 64 + 63 {
                return Some(if neg { i128::MIN } else { i128::MAX });
            }
            (u128::from(self.mant) << (e - 63).min(64), false, false, false)
        } else if e < 0 {
            // |x| < 1: the integer part is 0.
            let exact_half = e == -1 && self.mant == INT_BIT;
            let above = e == -1 && self.mant > INT_BIT;
            (0, true, exact_half, above)
        } else {
            let sh = 63 - e;
            let int = self.mant >> sh;
            let frac = self.mant << (64 - sh) >> (64 - sh);
            let halfv = 1u64 << (sh - 1);
            (u128::from(int), frac != 0, frac == halfv, frac > halfv)
        };
        let up = match rc {
            Round::Nearest => above_half || (half && int & 1 == 1),
            Round::Zero => false,
            Round::Down => neg && frac_nonzero,
            Round::Up => !neg && frac_nonzero,
        };
        let mag = int + u128::from(up);
        let mag = i128::try_from(mag).unwrap_or(i128::MAX);
        Some(if neg { -mag } else { mag })
    }

    /// The value with its sign flipped.
    pub(crate) fn neg(self) -> Self {
        Self { mant: self.mant, se: self.se ^ 0x8000 }
    }

    /// The value with its sign cleared.
    pub(crate) fn abs(self) -> Self {
        Self { mant: self.mant, se: self.se & 0x7FFF }
    }
}

/// Rounds `x` to an integral value in mode `rc`.
pub(crate) fn round_f64(x: f64, rc: Round) -> f64 {
    match rc {
        Round::Nearest => x.round_ties_even(),
        Round::Down => x.floor(),
        Round::Up => x.ceil(),
        Round::Zero => x.trunc(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doubles_survive_a_round_trip_through_the_register_format() {
        for &x in &[
            0.0f64,
            -0.0,
            1.0,
            -1.5,
            std::f64::consts::PI,
            1e300,
            -1e-300,
            f64::MIN_POSITIVE,
            5e-324,
            f64::MAX,
            f64::INFINITY,
            f64::NEG_INFINITY,
        ] {
            assert_eq!(F80::from_f64(x).to_f64().to_bits(), x.to_bits(), "{x:e}");
        }
        assert!(F80::from_f64(f64::NAN).to_f64().is_nan());
    }

    #[test]
    fn every_64_bit_integer_survives_a_round_trip() {
        for &v in &[0i64, 1, -1, i64::MAX, i64::MIN, 0x1234_5678_9ABC_DEF0, -7, 1 << 62] {
            assert_eq!(F80::from_i64(v).to_int(Round::Nearest), Some(i128::from(v)), "{v}");
        }
    }

    #[test]
    fn a_register_gives_back_the_80_bits_it_was_loaded_with() {
        // A 64-bit integer no double holds, and a long double's extra bits.
        for x in [F80::from_i64(0x1234_5678_9ABC_DEF1), F80 { mant: INT_BIT | 0x7FF, se: 0x3FFF }] {
            let r = Fpr::from_f80(x);
            assert_eq!(r.f80(), x);
            assert_eq!(r.neg().f80(), x.neg());
            assert_eq!(r.neg().neg().f80(), x);
            assert_eq!(r.f, x.to_f64());
        }
        // A result has the double's bits.
        assert_eq!(Fpr::from_f64(1.5).f80(), F80::from_f64(1.5));
    }

    #[test]
    fn known_constants_have_their_memory_format() {
        // 1.0 is 3FFF 8000000000000000.
        assert_eq!(F80::from_f64(1.0), F80 { mant: INT_BIT, se: 0x3FFF });
        assert_eq!(F80::from_i64(-2), F80 { mant: INT_BIT, se: 0xC000 });
    }

    #[test]
    fn rounding_to_an_integer_follows_the_mode() {
        let cases = [(2.5f64, [2, 2, 3, 2]), (-2.5, [-2, -3, -2, -2]), (0.4, [0, 0, 1, 0])];
        let modes = [Round::Nearest, Round::Down, Round::Up, Round::Zero];
        for (x, want) in cases {
            for (m, w) in modes.iter().zip(want) {
                assert_eq!(F80::from_f64(x).to_int(*m), Some(w), "{x} {m:?}");
                assert_eq!(round_f64(x, *m), w as f64, "{x} {m:?}");
            }
        }
    }

    #[test]
    fn a_long_double_rounds_to_the_nearest_double() {
        // 1 + 2^-53 exactly halfway: ties to even gives 1.0.
        let x = F80 { mant: INT_BIT | (1 << 10), se: 0x3FFF };
        assert_eq!(x.to_f64(), 1.0);
        // A little above halfway rounds up.
        let y = F80 { mant: INT_BIT | (1 << 10) | 1, se: 0x3FFF };
        assert_eq!(y.to_f64(), 1.0 + f64::EPSILON);
    }
}
