//! Guest instructions run in Rust on the saved register state.
//!
//! These are the user-mode instructions 64-bit mode has no encoding for, so
//! the translator cannot pass them to the host: the ASCII and decimal
//! adjusts and `SALC`. The instructions that also touch guest memory or the
//! bus (`BOUND`, `ENTER` with a nesting level, the 16-bit `LEAVE`, `IRETD`,
//! port I/O, device string moves) are in `cpu`, next to the memory helpers
//! they need.

use iced_x86::{Instruction, Mnemonic};

const CF: u32 = 0x001;
const PF: u32 = 0x004;
const AF: u32 = 0x010;
const ZF: u32 = 0x040;
const SF: u32 = 0x080;
const OF: u32 = 0x800;

fn parity_even(v: u8) -> u32 {
    if v.count_ones().is_multiple_of(2) { PF } else { 0 }
}

/// SF, ZF and PF of a byte result into `fl` (the others as given).
fn szp8(fl: u32, v: u8) -> u32 {
    (fl & !(SF | ZF | PF)) | u32::from(v & 0x80) | if v == 0 { ZF } else { 0 } | parity_even(v)
}

/// The outcome of an instruction run here.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Adjust {
    /// New EAX and flags.
    Done(u32, u32),
    /// `AAM 0`: a divide error.
    DivideError,
    /// Not one of these instructions.
    NotMine,
}

/// Runs `AAA`, `AAS`, `AAM`, `AAD`, `DAA`, `DAS` or `SALC` on `eax` and the
/// arithmetic flags `fl`.
pub(crate) fn adjust(i: &Instruction, eax: u32, fl: u32) -> Adjust {
    let cf = fl & CF != 0;
    let af = fl & AF != 0;
    let mut al = eax as u8;
    let mut ah = (eax >> 8) as u8;
    let set_ax = |h: u8, l: u8| (eax & 0xFFFF_0000) | u32::from(h) << 8 | u32::from(l);
    match i.mnemonic() {
        Mnemonic::Aam => {
            let base = i.immediate8();
            if base == 0 {
                return Adjust::DivideError;
            }
            ah = al / base;
            al %= base;
            // OF, AF, CF undefined: clear, as a P6 leaves them.
            Adjust::Done(set_ax(ah, al), szp8(fl & !(OF | AF | CF), al))
        }
        Mnemonic::Aad => {
            al = al.wrapping_add(ah.wrapping_mul(i.immediate8()));
            Adjust::Done(set_ax(0, al), szp8(fl & !(OF | AF | CF), al))
        }
        Mnemonic::Aaa | Mnemonic::Aas => {
            let mut ax = eax as u16;
            let adjust = al & 0x0F > 9 || af;
            if adjust {
                ax = if i.mnemonic() == Mnemonic::Aaa {
                    ax.wrapping_add(0x106)
                } else {
                    ax.wrapping_sub(6).wrapping_sub(0x100)
                };
            }
            ax &= 0xFF0F;
            let f = (fl & !(AF | CF)) | if adjust { AF | CF } else { 0 };
            Adjust::Done((eax & 0xFFFF_0000) | u32::from(ax), f)
        }
        Mnemonic::Daa | Mnemonic::Das => {
            let add = i.mnemonic() == Mnemonic::Daa;
            let old = al;
            let (mut ncf, mut naf) = (false, false);
            if al & 0x0F > 9 || af {
                ncf = cf || if add { al > 0xF9 } else { al < 6 };
                al = if add { al.wrapping_add(6) } else { al.wrapping_sub(6) };
                naf = true;
            }
            if old > 0x99 || cf {
                al = if add { al.wrapping_add(0x60) } else { al.wrapping_sub(0x60) };
                ncf = true;
            } else if add {
                ncf = false;
            }
            let f = (fl & !(AF | CF)) | if ncf { CF } else { 0 } | if naf { AF } else { 0 };
            Adjust::Done((eax & 0xFFFF_FF00) | u32::from(al), szp8(f, al))
        }
        Mnemonic::Salc => Adjust::Done((eax & 0xFFFF_FF00) | if cf { 0xFF } else { 0 }, fl),
        _ => Adjust::NotMine,
    }
}
