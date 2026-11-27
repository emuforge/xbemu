//! Which memory an x86-32 instruction may touch, for locating write-tracking
//! faults.
//!
//! When the guest touches a RAM page the protector closed, `KVM_RUN` fails
//! with `EFAULT`. Kernels that annotate the failure (`KVM_EXIT_MEMORY_FAULT`)
//! give the physical address; Linux 6.12 does not for this path, so the
//! backend works the address out from the instruction at EIP.
//!
//! This is not a full decoder. It finds the addresses an instruction *may*
//! touch (its ModRM operand, absolute operand, string pointers, the stack) and
//! the caller picks the first one that lies in a protected page. Naming a
//! page the instruction does not touch is harmless: the tracker records one
//! page too many, the instruction is retried and faults again on the right
//! one. Missing the page is handled by the caller's fallback.

/// A linear address range an instruction may access.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Operand {
    /// First byte.
    pub va: u32,
    /// Length in bytes (an upper bound).
    pub len: u32,
}

/// The longest ordinary memory operand (an SSE register).
const WIDEST: u32 = 16;

/// One-byte opcodes followed by a ModRM byte.
fn one_byte_has_modrm(op: u8) -> bool {
    (op < 0x40 && op & 7 < 4)
        || matches!(
            op,
            0x62 | 0x63
                | 0x69
                | 0x6B
                | 0x80..=0x8F
                | 0xC0
                | 0xC1
                | 0xC4..=0xC7
                | 0xD0..=0xD3
                | 0xD8..=0xDF
                | 0xF6
                | 0xF7
                | 0xFE
                | 0xFF
        )
}

/// `0F xx` opcodes followed by a ModRM byte that can name memory.
fn two_byte_has_modrm(op: u8) -> bool {
    matches!(
        op,
        0x00..=0x03
            | 0x0D
            | 0x10..=0x1F
            | 0x28..=0x2F
            | 0x40..=0x76
            | 0x78..=0x7F
            | 0x90..=0x9F
            | 0xA3..=0xA5
            | 0xAB..=0xAF
            | 0xB0..=0xB7
            | 0xBA..=0xBF
            | 0xC0..=0xC7
            | 0xD0..=0xFF
    )
}

/// The addresses the instruction at `code` (which starts at `eip`) may
/// access, most specific first. `gpr` is indexed in the x86 encoding order.
pub(crate) fn operands(code: &[u8], eip: u32, gpr: &[u32; 8], fs_base: u32) -> Vec<Operand> {
    let (eax, ebx, esp, ebp, esi, edi) = (gpr[0], gpr[3], gpr[4], gpr[5], gpr[6], gpr[7]);
    let mut out = Vec::with_capacity(8);
    // The instruction's own bytes: a closed page cannot be fetched from.
    out.push(Operand { va: eip, len: 15 });

    let mut at = 0;
    let mut seg_base = 0u32;
    let mut addr16 = false;
    while let Some(&b) = code.get(at) {
        match b {
            0x64 => seg_base = fs_base,
            0x67 => addr16 = true,
            0x26 | 0x2E | 0x36 | 0x3E | 0x65 | 0x66 | 0xF0 | 0xF2 | 0xF3 => {}
            _ => break,
        }
        at += 1;
    }
    let byte = |i: usize| code.get(i).copied().unwrap_or(0);
    let dword = |i: usize| u32::from_le_bytes([byte(i), byte(i + 1), byte(i + 2), byte(i + 3)]);
    let word = |i: usize| u32::from(u16::from_le_bytes([byte(i), byte(i + 1)]));

    let op = byte(at);
    at += 1;
    let mut len = WIDEST;
    let has_modrm = if op == 0x0F {
        let op2 = byte(at);
        at += 1;
        match op2 {
            0x38 | 0x3A => {
                at += 1;
                true
            }
            // FXSAVE and FXRSTOR move 512 bytes.
            0xAE => {
                len = 512;
                true
            }
            // MASKMOVQ stores at EDI.
            0xF7 => {
                out.push(Operand { va: seg_base.wrapping_add(edi), len: WIDEST });
                true
            }
            _ => two_byte_has_modrm(op2),
        }
    } else {
        match op {
            // Absolute moves: the address follows the opcode.
            0xA0..=0xA3 => {
                let abs = if addr16 { word(at) } else { dword(at) };
                out.push(Operand { va: seg_base.wrapping_add(abs), len: 4 });
            }
            // String instructions: MOVS, CMPS, STOS, LODS, SCAS, INS, OUTS.
            0xA4..=0xA7 | 0xAA..=0xAF | 0x6C..=0x6F => {
                out.push(Operand { va: edi, len: 4 });
                out.push(Operand { va: seg_base.wrapping_add(esi), len: 4 });
            }
            // XLAT reads at EBX + AL.
            0xD7 => out
                .push(Operand { va: seg_base.wrapping_add(ebx).wrapping_add(eax & 0xFF), len: 1 }),
            // LEAVE pops through EBP.
            0xC9 => out.push(Operand { va: ebp, len: 4 }),
            // FNSAVE, FRSTOR, FLDENV and FNSTENV move up to 108 bytes.
            0xD9 | 0xDD => len = 108,
            _ => {}
        }
        one_byte_has_modrm(op)
    };

    if has_modrm {
        let modrm = byte(at);
        at += 1;
        let (mode, rm) = (modrm >> 6, usize::from(modrm & 7));
        let ea = if mode == 3 {
            None
        } else if addr16 {
            let (bx, si, di, bp) = (ebx & 0xFFFF, esi & 0xFFFF, edi & 0xFFFF, ebp & 0xFFFF);
            let base = match rm {
                0 => bx + si,
                1 => bx + di,
                2 => bp + si,
                3 => bp + di,
                4 => si,
                5 => di,
                6 if mode == 0 => 0,
                6 => bp,
                _ => bx,
            };
            let disp = match mode {
                0 if rm == 6 => word(at),
                1 => i32::from(byte(at) as i8) as u32,
                2 => word(at),
                _ => 0,
            };
            Some(base.wrapping_add(disp) & 0xFFFF)
        } else {
            let mut base = 0u32;
            let mut disp32 = mode == 2;
            if rm == 4 {
                let sib = byte(at);
                at += 1;
                let (scale, index, sib_base) =
                    (sib >> 6, usize::from((sib >> 3) & 7), usize::from(sib & 7));
                if index != 4 {
                    base = base.wrapping_add(gpr[index] << scale);
                }
                if sib_base == 5 && mode == 0 {
                    disp32 = true;
                } else {
                    base = base.wrapping_add(gpr[sib_base]);
                }
            } else if rm == 5 && mode == 0 {
                disp32 = true;
            } else {
                base = gpr[rm];
            }
            let disp = if disp32 {
                dword(at)
            } else if mode == 1 {
                i32::from(byte(at) as i8) as u32
            } else {
                0
            };
            Some(base.wrapping_add(disp))
        };
        if let Some(ea) = ea {
            out.push(Operand { va: seg_base.wrapping_add(ea), len });
        }
    }

    // Pushes, pops, calls, returns and exception delivery; PUSHA and an
    // exception frame reach furthest.
    out.push(Operand { va: esp.wrapping_sub(4), len: 12 });
    out.push(Operand { va: esp.wrapping_sub(32), len: 64 });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const GPR: [u32; 8] = [0x10, 0x20, 0x30, 0x4000, 0x8_0000, 0x7_0000, 0x1000, 0x2000];

    /// The operands between the code bytes (always first) and the stack
    /// (always last two).
    fn specific(code: &[u8]) -> Vec<Operand> {
        let all = operands(code, 0x1_0000, &GPR, 0xF000_0000);
        assert_eq!(all[0], Operand { va: 0x1_0000, len: 15 });
        all[1..all.len() - 2].to_vec()
    }

    #[test]
    fn a_register_indirect_store_names_its_target() {
        // mov dword [edi], 0x2222
        assert_eq!(specific(&[0xC7, 0x07, 0x22, 0x22, 0, 0]), [Operand { va: 0x2000, len: 16 }]);
        // inc dword [edi]
        assert_eq!(specific(&[0xFF, 0x07]), [Operand { va: 0x2000, len: 16 }]);
    }

    #[test]
    fn displacements_and_scaled_indexes_are_added() {
        // mov [esi + 0x70], ecx
        assert_eq!(specific(&[0x89, 0x4E, 0x70])[0].va, 0x1070);
        // mov eax, [ebx + ecx*4 - 8]
        assert_eq!(specific(&[0x8B, 0x44, 0x8B, 0xF8])[0].va, 0x4000 + 0x20 * 4 - 8);
        // mov eax, [0x12345678 + esi*2]
        assert_eq!(
            specific(&[0x8B, 0x04, 0x75, 0x78, 0x56, 0x34, 0x12])[0].va,
            0x1234_5678 + 0x2000
        );
        // mov ebx, [0x40000004]
        assert_eq!(specific(&[0x8B, 0x1D, 4, 0, 0, 0x40])[0].va, 0x4000_0004);
    }

    #[test]
    fn absolute_moves_and_fs_overrides_are_followed() {
        // mov [0x40000004], eax
        assert_eq!(specific(&[0xA3, 4, 0, 0, 0x40]), [Operand { va: 0x4000_0004, len: 4 }]);
        // mov eax, fs:[0x28]
        assert_eq!(specific(&[0x64, 0xA1, 0x28, 0, 0, 0]), [Operand { va: 0xF000_0028, len: 4 }]);
        // mov fs:[eax], ecx
        assert_eq!(specific(&[0x64, 0x89, 0x08])[0].va, 0xF000_0010);
    }

    #[test]
    fn string_and_sse_instructions_are_covered() {
        // rep movsd
        assert_eq!(
            specific(&[0xF3, 0xA5]),
            [Operand { va: 0x2000, len: 4 }, Operand { va: 0x1000, len: 4 }]
        );
        // movups [edi + 8], xmm3
        assert_eq!(specific(&[0x0F, 0x11, 0x5F, 0x08]), [Operand { va: 0x2008, len: 16 }]);
    }

    #[test]
    fn register_only_instructions_name_only_code_and_stack() {
        // push eax; add eax, ecx; jmp short
        for code in [&[0x50][..], &[0x01, 0xC8], &[0xEB, 0xFE]] {
            assert_eq!(specific(code), []);
        }
        let all = operands(&[0x50], 0x1_0000, &GPR, 0);
        assert_eq!(all[1], Operand { va: 0x8_0000 - 4, len: 12 });
    }
}
