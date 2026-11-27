//! The AArch64 instructions the code generator emits, and nothing more.
//!
//! Registers are numbers (0-30; 31 is SP or the zero register by
//! instruction). Branches to a place not yet known are emitted with a zero
//! offset and patched with [`Asm::patch`] once it is.

/// The frame pointer, link register and stack pointer.
const FP: u32 = 29;
const LR: u32 = 30;
const SP: u32 = 31;

/// A growing run of instructions.
#[derive(Default)]
pub(super) struct Asm {
    pub(super) code: Vec<u32>,
}

impl Asm {
    fn emit(&mut self, w: u32) -> usize {
        self.code.push(w);
        self.code.len() - 1
    }

    /// The index the next instruction will have.
    pub(super) fn pos(&self) -> usize {
        self.code.len()
    }

    /// `ldr wT, [xN, #off]`; `off` is a multiple of 4 below 16384.
    pub(super) fn ldr_w(&mut self, t: u32, n: u32, off: u32) {
        debug_assert!(off.is_multiple_of(4) && off < 16384);
        self.emit(0xB940_0000 | (off / 4) << 10 | n << 5 | t);
    }

    /// `str wT, [xN, #off]`.
    pub(super) fn str_w(&mut self, t: u32, n: u32, off: u32) {
        debug_assert!(off.is_multiple_of(4) && off < 16384);
        self.emit(0xB900_0000 | (off / 4) << 10 | n << 5 | t);
    }

    /// `ldrb wT, [xN, #off]`; `off` is below 4096.
    pub(super) fn ldrb(&mut self, t: u32, n: u32, off: u32) {
        debug_assert!(off < 4096);
        self.emit(0x3940_0000 | off << 10 | n << 5 | t);
    }

    /// `strb wT, [xN, #off]`.
    pub(super) fn strb(&mut self, t: u32, n: u32, off: u32) {
        debug_assert!(off < 4096);
        self.emit(0x3900_0000 | off << 10 | n << 5 | t);
    }

    /// `ldr xT, [xN, #off]`; `off` is a multiple of 8 below 32768.
    pub(super) fn ldr_x(&mut self, t: u32, n: u32, off: u32) {
        debug_assert!(off.is_multiple_of(8) && off < 32768);
        self.emit(0xF940_0000 | (off / 8) << 10 | n << 5 | t);
    }

    /// Loads a 32-bit constant into `wD` (one or two instructions).
    pub(super) fn mov_w_imm(&mut self, d: u32, imm: u32) {
        let (lo, hi) = (imm & 0xFFFF, imm >> 16);
        if lo == 0 && hi != 0 {
            self.emit(0x5280_0000 | 1 << 21 | hi << 5 | d);
            return;
        }
        self.emit(0x5280_0000 | lo << 5 | d);
        if hi != 0 {
            self.emit(0x7280_0000 | 1 << 21 | hi << 5 | d);
        }
    }

    /// `movz x0, #0`.
    pub(super) fn zero_x0(&mut self) {
        self.emit(0xD280_0000);
    }

    /// `add wD, wN, wM`.
    pub(super) fn add_w(&mut self, d: u32, n: u32, m: u32) {
        self.emit(0x0B00_0000 | m << 16 | n << 5 | d);
    }

    /// `add wD, wN, wM, lsl #shift`.
    pub(super) fn add_w_lsl(&mut self, d: u32, n: u32, m: u32, shift: u32) {
        self.emit(0x0B00_0000 | m << 16 | shift << 10 | n << 5 | d);
    }

    /// `sub wD, wN, wM`.
    pub(super) fn sub_w(&mut self, d: u32, n: u32, m: u32) {
        self.emit(0x4B00_0000 | m << 16 | n << 5 | d);
    }

    /// `and wD, wN, wM`.
    pub(super) fn and_w(&mut self, d: u32, n: u32, m: u32) {
        self.emit(0x0A00_0000 | m << 16 | n << 5 | d);
    }

    /// `orr wD, wN, wM`.
    pub(super) fn orr_w(&mut self, d: u32, n: u32, m: u32) {
        self.emit(0x2A00_0000 | m << 16 | n << 5 | d);
    }

    /// `eor wD, wN, wM`.
    pub(super) fn eor_w(&mut self, d: u32, n: u32, m: u32) {
        self.emit(0x4A00_0000 | m << 16 | n << 5 | d);
    }

    /// `add wD, wN, #imm`; `imm` is below 4096.
    pub(super) fn add_w_imm(&mut self, d: u32, n: u32, imm: u32) {
        debug_assert!(imm < 4096);
        self.emit(0x1100_0000 | imm << 10 | n << 5 | d);
    }

    /// `sub wD, wN, #imm`; `imm` is below 4096.
    pub(super) fn sub_w_imm(&mut self, d: u32, n: u32, imm: u32) {
        debug_assert!(imm < 4096);
        self.emit(0x5100_0000 | imm << 10 | n << 5 | d);
    }

    /// `add xD, xN, xM`.
    pub(super) fn add_x(&mut self, d: u32, n: u32, m: u32) {
        self.emit(0x8B00_0000 | m << 16 | n << 5 | d);
    }

    /// `mov wD, wM`.
    pub(super) fn mov_w(&mut self, d: u32, m: u32) {
        self.emit(0x2A00_03E0 | m << 16 | d);
    }

    /// `mov xD, xM`.
    pub(super) fn mov_x(&mut self, d: u32, m: u32) {
        self.emit(0xAA00_03E0 | m << 16 | d);
    }

    /// `ubfx wD, wN, #0, #12`: the low 12 bits.
    pub(super) fn low12(&mut self, d: u32, n: u32) {
        self.emit(0x5300_2C00 | n << 5 | d);
    }

    /// `lsr wD, wN, #12`.
    pub(super) fn lsr12(&mut self, d: u32, n: u32) {
        self.emit(0x530C_7C00 | n << 5 | d);
    }

    /// `cmp wN, #imm`; `imm` is below 4096.
    pub(super) fn cmp_w_imm(&mut self, n: u32, imm: u32) {
        debug_assert!(imm < 4096);
        self.emit(0x7100_001F | imm << 10 | n << 5);
    }

    /// `cmp wN, wM`.
    pub(super) fn cmp_w(&mut self, n: u32, m: u32) {
        self.emit(0x6B00_001F | m << 16 | n << 5);
    }

    /// `ldr wT, [xN, xM, lsl #2]`.
    pub(super) fn ldr_w_idx4(&mut self, t: u32, n: u32, m: u32) {
        self.emit(0xB860_7800 | m << 16 | n << 5 | t);
    }

    /// `ldr wT, [xN, xM]`.
    pub(super) fn ldr_w_idx(&mut self, t: u32, n: u32, m: u32) {
        self.emit(0xB860_6800 | m << 16 | n << 5 | t);
    }

    /// `str wT, [xN, xM]`.
    pub(super) fn str_w_idx(&mut self, t: u32, n: u32, m: u32) {
        self.emit(0xB820_6800 | m << 16 | n << 5 | t);
    }

    /// `ldrb wT, [xN, xM]`.
    pub(super) fn ldrb_idx(&mut self, t: u32, n: u32, m: u32) {
        self.emit(0x3860_6800 | m << 16 | n << 5 | t);
    }

    /// `add xD, xN, xM, lsl #12`.
    pub(super) fn add_x_lsl12(&mut self, d: u32, n: u32, m: u32) {
        self.emit(0x8B00_3000 | m << 16 | n << 5 | d);
    }

    /// `b.<cond> <later>`; `cond` is the AArch64 condition number.
    pub(super) fn b_cond(&mut self, cond: u32) -> usize {
        self.emit(0x5400_0000 | cond)
    }

    /// `b.hi <later>` (unsigned higher).
    pub(super) fn b_hi(&mut self) -> usize {
        self.emit(0x5400_0008)
    }

    /// `b.hs <later>` (unsigned higher or same).
    pub(super) fn b_hs(&mut self) -> usize {
        self.emit(0x5400_0002)
    }

    /// `tbz wT, #bit, <later>`; `bit` is below 32.
    pub(super) fn tbz_w(&mut self, t: u32, bit: u32) -> usize {
        self.emit(0x3600_0000 | bit << 19 | t)
    }

    /// `tbnz wT, #bit, <later>`; `bit` is below 32.
    pub(super) fn tbnz_w(&mut self, t: u32, bit: u32) -> usize {
        self.emit(0x3700_0000 | bit << 19 | t)
    }

    /// `cbz wT, <later>`: returns the position to patch.
    pub(super) fn cbz_w(&mut self, t: u32) -> usize {
        self.emit(0x3400_0000 | t)
    }

    /// `cbnz wT, <later>`.
    pub(super) fn cbnz_w(&mut self, t: u32) -> usize {
        self.emit(0x3500_0000 | t)
    }

    /// `cbnz xT, <later>`.
    pub(super) fn cbnz_x(&mut self, t: u32) -> usize {
        self.emit(0xB500_0000 | t)
    }

    /// `tbnz xT, #32, <later>`.
    pub(super) fn tbnz_bit32(&mut self, t: u32) -> usize {
        self.emit(0xB700_0000 | t)
    }

    /// `b <later>`.
    pub(super) fn b(&mut self) -> usize {
        self.emit(0x1400_0000)
    }

    /// `blr xN`.
    pub(super) fn blr(&mut self, n: u32) {
        self.emit(0xD63F_0000 | n << 5);
    }

    /// Points the branch at `at` to the instruction at `target`.
    pub(super) fn patch(&mut self, at: usize, target: usize) {
        let delta = (target as i64 - at as i64) as u32;
        let w = &mut self.code[at];
        *w |= match *w >> 24 & 0x7F {
            // b
            0x14..=0x17 => delta & 0x03FF_FFFF,
            // tbz, tbnz
            0x36 | 0x37 => (delta & 0x3FFF) << 5,
            // cbz, cbnz
            _ => (delta & 0x7_FFFF) << 5,
        };
    }

    /// Saves the frame and x19 to x24, and takes the two arguments into
    /// x19 and x20: `stp x29, x30, [sp, #-64]!; mov x29, sp; stp x19, x20,
    /// [sp, #16]; stp x21, x22, [sp, #32]; stp x23, x24, [sp, #48];
    /// mov x19, x0; mov x20, x1`.
    pub(super) fn prologue(&mut self) {
        self.emit(0xA980_0000 | (0x78 << 15) | LR << 10 | SP << 5 | FP);
        self.emit(0x9100_03FD);
        self.emit(0xA901_53F3);
        self.emit(0xA902_5BF5);
        self.emit(0xA903_63F7);
        self.mov_x(19, 0);
        self.mov_x(20, 1);
    }

    /// Restores what [`Asm::prologue`] saved and returns:
    /// `ldp x23, x24, [sp, #48]; ldp x21, x22, [sp, #32]; ldp x19, x20,
    /// [sp, #16]; ldp x29, x30, [sp], #64; ret`.
    pub(super) fn epilogue(&mut self) {
        self.emit(0xA943_63F7);
        self.emit(0xA942_5BF5);
        self.emit(0xA941_53F3);
        self.emit(0xA8C0_0000 | (8 << 15) | LR << 10 | SP << 5 | FP);
        self.emit(0xD65F_03C0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(f: impl FnOnce(&mut Asm)) -> Vec<u32> {
        let mut a = Asm::default();
        f(&mut a);
        a.code
    }

    /// The words are what Apple's assembler gives for the text beside them.
    #[test]
    fn every_form_has_the_assembler_s_encoding() {
        assert_eq!(one(|a| a.ldr_w(3, 19, 40)), [0xB940_2A63]); // ldr w3, [x19, #40]
        assert_eq!(one(|a| a.str_w(3, 19, 40)), [0xB900_2A63]); // str w3, [x19, #40]
        assert_eq!(one(|a| a.ldrb(9, 19, 5)), [0x3940_1669]); // ldrb w9, [x19, #5]
        assert_eq!(one(|a| a.strb(9, 19, 5)), [0x3900_1669]); // strb w9, [x19, #5]
        assert_eq!(one(|a| a.ldr_x(16, 19, 48)), [0xF940_1A70]); // ldr x16, [x19, #48]
        // movz w1, #0x1234; movk w1, #0x5678, lsl #16
        assert_eq!(one(|a| a.mov_w_imm(1, 0x5678_1234)), [0x5282_4681, 0x72AA_CF01]);
        assert_eq!(one(|a| a.mov_w_imm(1, 0x1234)), [0x5282_4681]);
        assert_eq!(one(Asm::zero_x0), [0xD280_0000]); // movz x0, #0
        assert_eq!(one(|a| a.add_w(2, 0, 1)), [0x0B01_0002]); // add w2, w0, w1
        assert_eq!(one(|a| a.sub_w(2, 0, 1)), [0x4B01_0002]); // sub w2, w0, w1
        assert_eq!(one(|a| a.and_w(2, 0, 1)), [0x0A01_0002]); // and w2, w0, w1
        assert_eq!(one(|a| a.orr_w(2, 0, 1)), [0x2A01_0002]); // orr w2, w0, w1
        assert_eq!(one(|a| a.eor_w(2, 0, 1)), [0x4A01_0002]); // eor w2, w0, w1
        assert_eq!(one(|a| a.add_w_lsl(0, 0, 9, 3)), [0x0B09_0C00]); // add w0, w0, w9, lsl #3
        assert_eq!(one(|a| a.add_w_imm(9, 9, 4)), [0x1100_1129]); // add w9, w9, #4
        assert_eq!(one(|a| a.sub_w_imm(9, 9, 4)), [0x5100_1129]); // sub w9, w9, #4
        assert_eq!(one(|a| a.add_x(1, 20, 9)), [0x8B09_0281]); // add x1, x20, x9
        assert_eq!(one(|a| a.mov_x(0, 19)), [0xAA13_03E0]); // mov x0, x19
        assert_eq!(one(|a| a.mov_w(10, 0)), [0x2A00_03EA]); // mov w10, w0
        assert_eq!(one(|a| a.blr(16)), [0xD63F_0200]); // blr x16
        assert_eq!(one(|a| a.low12(9, 1)), [0x5300_2C29]); // ubfx w9, w1, #0, #12
        assert_eq!(one(|a| a.lsr12(12, 11)), [0x530C_7D6C]); // lsr w12, w11, #12
        assert_eq!(one(|a| a.cmp_w_imm(9, 0xFFC)), [0x713F_F13F]); // cmp w9, #0xffc
        assert_eq!(one(|a| a.cmp_w(12, 13)), [0x6B0D_019F]); // cmp w12, w13
        assert_eq!(one(|a| a.ldr_w_idx4(11, 21, 10)), [0xB86A_7AAB]); // ldr w11, [x21, x10, lsl #2]
        assert_eq!(one(|a| a.ldr_w_idx(0, 23, 12)), [0xB86C_6AE0]); // ldr w0, [x23, x12]
        assert_eq!(one(|a| a.str_w_idx(2, 23, 12)), [0xB82C_6AE2]); // str w2, [x23, x12]
        assert_eq!(one(|a| a.ldrb_idx(13, 22, 12)), [0x386C_6ACD]); // ldrb w13, [x22, x12]
        assert_eq!(one(|a| a.add_x_lsl12(12, 9, 12)), [0x8B0C_312C]); // add x12, x9, x12, lsl #12
        // stp x29, x30, [sp, #-64]!; mov x29, sp; stp x19, x20, [sp, #16];
        // stp x21, x22, [sp, #32]; stp x23, x24, [sp, #48]; mov x19, x0;
        // mov x20, x1
        assert_eq!(
            one(Asm::prologue),
            [
                0xA9BC_7BFD,
                0x9100_03FD,
                0xA901_53F3,
                0xA902_5BF5,
                0xA903_63F7,
                0xAA00_03F3,
                0xAA01_03F4
            ]
        );
        // ldp x23, x24, [sp, #48]; ldp x21, x22, [sp, #32]; ldp x19, x20,
        // [sp, #16]; ldp x29, x30, [sp], #64; ret
        assert_eq!(
            one(Asm::epilogue),
            [0xA943_63F7, 0xA942_5BF5, 0xA941_53F3, 0xA8C4_7BFD, 0xD65F_03C0]
        );
    }

    #[test]
    fn a_patched_branch_has_the_assembler_s_encoding() {
        let branch = |f: fn(&mut Asm) -> usize, ahead: usize| {
            let mut a = Asm::default();
            let at = f(&mut a);
            a.patch(at, at + ahead);
            a.code[0]
        };
        assert_eq!(branch(|a| a.cbz_w(0), 4), 0x3400_0080); // cbz w0, .+16
        assert_eq!(branch(|a| a.cbnz_w(9), 2), 0x3500_0049); // cbnz w9, .+8
        assert_eq!(branch(|a| a.cbnz_x(0), 3), 0xB500_0060); // cbnz x0, .+12
        assert_eq!(branch(Asm::b, 5), 0x1400_0005); // b .+20
        assert_eq!(branch(|a| a.tbnz_bit32(0), 10), 0xB700_0140); // tbnz x0, #32, .+40
        assert_eq!(branch(Asm::b_hi, 2), 0x5400_0048); // b.hi .+8
        assert_eq!(branch(|a| a.b_cond(8), 2), 0x5400_0048); // b.hi .+8
        assert_eq!(branch(|a| a.b_cond(2), 3), 0x5400_0062); // b.hs .+12
        assert_eq!(branch(Asm::b_hs, 3), 0x5400_0062); // b.hs .+12
        assert_eq!(branch(|a| a.tbz_w(11, 1), 2), 0x3608_004B); // tbz w11, #1, .+8
        assert_eq!(branch(|a| a.tbnz_w(11, 3), 2), 0x3718_004B); // tbnz w11, #3, .+8
    }
}
