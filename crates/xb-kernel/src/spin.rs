//! Proof that the running thread is in a wait loop nothing but an interrupt,
//! another thread or a device can end.
//!
//! Titles wait for the next vertical blank, a worker's result or a flag an
//! interrupt handler sets by reading memory in a loop: Halo's main thread
//! compares a 64-bit count its vblank callback advances against a target, in
//! six instructions, for most of every frame. Run as it is, such a loop takes
//! a whole host core to compute nothing.
//!
//! [`is_wait_loop`] interprets the guest's code from the state the thread
//! stopped in. If the code comes back to a state it was in before (the same
//! instruction, registers, flags and stack below the starting stack
//! pointer) and stored nothing else on the way, the same
//! instructions will run again with the same result for as long as the
//! memory they read stays as it is: on the one processor a console has, only
//! an interrupt, another thread or a device changes that. The kernel then
//! waits on the host as it does for `HLT`, and proves the loop again before
//! every further wait, so a loop whose memory changed runs on.
//!
//! The interpreter knows the instructions such loops are made of (loads,
//! compares, branches, calls and the stack) and gives up on any other: an
//! unknown instruction is never a proof. What it allows of stores:
//!
//! * below the stack pointer the thread stopped with, anything. Nothing live
//!   is there (an interrupt's frame goes over it), and a load from there must
//!   find a byte the same walk stored;
//! * anywhere else, only the bytes already there (a caller pushing the same
//!   argument and return address each time round).
//!
//! Device registers are not RAM, so a loop that polls one is never proven,
//! and neither is one that reads the time stamp counter or calls the kernel.
//!
//! The kernel does not stake a session on the proof: a thread it holds is
//! run for real every `SPIN_RUN_EVERY` all the same, so a proof that is
//! wrong costs a pause and not the title.

use xb_cpu::{Regs, reg};

/// The arithmetic flags the interpreter computes and compares: CF, PF, ZF,
/// SF and OF. AF is left out: no instruction it knows reads it.
const FLAGS: u32 = CF | PF | ZF | SF | OF;
const CF: u32 = 1 << 0;
const PF: u32 = 1 << 2;
const ZF: u32 = 1 << 6;
const SF: u32 = 1 << 7;
const OF: u32 = 1 << 11;

/// Instructions walked before giving up. Halo's loop is six; a loop that
/// calls a small function to read its flag stays well under a hundred.
const MAX_STEPS: u32 = 512;

/// How far below the stack pointer the walk may store.
const STACK_ROOM: u32 = 1024;

/// What the prover needs of the machine.
pub(crate) struct Env<'a> {
    /// Copies guest RAM at an address into the buffer; `false` where no RAM
    /// is mapped (a device's registers, a free page).
    pub read: &'a dyn Fn(u32, &mut [u8]) -> bool,
    /// The kernel's entry addresses, `[start, end)`: code there is not guest
    /// code.
    pub trap: (u32, u32),
}

/// Whether the code at `regs` runs into a cycle of states (the same
/// instruction, registers and flags again) having changed nothing: a loop
/// that waits for something outside itself.
///
/// The state compared against is the one a power of two steps back (Brent's
/// cycle search), so the walk may start anywhere: in the loop, with the
/// flags a kick happened to leave, or on the way into it.
pub(crate) fn is_wait_loop(regs: &Regs, env: &Env) -> bool {
    let mut w = Walk {
        r: regs.gpr,
        eip: regs.eip,
        fl: regs.eflags,
        env,
        top: regs.gpr[reg::ESP],
        below: Vec::new(),
    };
    let mut mark = (w.eip, w.r, w.fl & FLAGS);
    let mut mark_below = w.below.clone();
    for n in 1..=MAX_STEPS {
        if w.step().is_none() {
            return false;
        }
        let now = (w.eip, w.r, w.fl & FLAGS);
        // The stack the walk stored is state too. Tiger Woods calls one
        // small function twice in a row from straight-line code: at its
        // first instruction the registers are the same both times and only
        // the return address differs. Taken for a cycle, its main thread
        // was parked for good.
        if now == mark && w.below == mark_below {
            return true;
        }
        if n.is_power_of_two() {
            mark = now;
            mark_below.clone_from(&w.below);
        }
    }
    false
}

/// An operand a ModRM byte names.
#[derive(Clone, Copy)]
enum Rm {
    Reg(usize),
    Mem(u32),
}

struct Walk<'a> {
    r: [u32; 8],
    eip: u32,
    fl: u32,
    env: &'a Env<'a>,
    /// The stack pointer at the start: stores below it are kept in `below`.
    top: u32,
    /// The bytes stored below `top`, one entry an address, in address order
    /// (so two walks' stacks compare equal when they hold the same).
    below: Vec<(u32, u8)>,
}

impl Walk<'_> {
    fn is_below(&self, a: u32) -> bool {
        a.wrapping_sub(self.top.wrapping_sub(STACK_ROOM)) < STACK_ROOM
    }

    fn fetch8(&mut self) -> Option<u8> {
        if self.eip.wrapping_sub(self.env.trap.0) < self.env.trap.1.wrapping_sub(self.env.trap.0) {
            return None;
        }
        let mut b = [0u8];
        (self.env.read)(self.eip, &mut b).then_some(())?;
        self.eip = self.eip.wrapping_add(1);
        Some(b[0])
    }

    fn fetch32(&mut self) -> Option<u32> {
        let mut v = 0u32;
        for i in 0..4 {
            v |= u32::from(self.fetch8()?) << (8 * i);
        }
        Some(v)
    }

    /// An 8-bit displacement or immediate, sign-extended.
    fn fetch8s(&mut self) -> Option<u32> {
        Some(self.fetch8()? as i8 as u32)
    }

    fn load(&self, a: u32, bytes: u32) -> Option<u32> {
        let mut v = 0u32;
        for i in 0..bytes {
            let at = a.wrapping_add(i);
            let b = if self.is_below(at) {
                let i = self.below.binary_search_by_key(&at, |e| e.0).ok()?;
                self.below[i].1
            } else {
                let mut b = [0u8];
                (self.env.read)(at, &mut b).then_some(())?;
                b[0]
            };
            v |= u32::from(b) << (8 * i);
        }
        Some(v)
    }

    fn store(&mut self, a: u32, bytes: u32, v: u32) -> Option<()> {
        for i in 0..bytes {
            let at = a.wrapping_add(i);
            let b = (v >> (8 * i)) as u8;
            let mut now = [0u8];
            (self.env.read)(at, &mut now).then_some(())?; // RAM, not a device
            if self.is_below(at) {
                match self.below.binary_search_by_key(&at, |e| e.0) {
                    Ok(i) => self.below[i].1 = b,
                    Err(i) => self.below.insert(i, (at, b)),
                }
            } else if now[0] != b {
                return None;
            }
        }
        Some(())
    }

    fn reg8(&self, i: usize) -> u32 {
        if i < 4 { self.r[i] & 0xFF } else { (self.r[i - 4] >> 8) & 0xFF }
    }

    fn set_reg8(&mut self, i: usize, v: u32) {
        if i < 4 {
            self.r[i] = (self.r[i] & !0xFF) | (v & 0xFF);
        } else {
            self.r[i - 4] = (self.r[i - 4] & !0xFF00) | ((v & 0xFF) << 8);
        }
    }

    /// The ModRM byte and what follows it: the `reg` field and the operand.
    fn modrm(&mut self) -> Option<(usize, Rm)> {
        let m = self.fetch8()?;
        let (mode, regf, rm) = (m >> 6, usize::from((m >> 3) & 7), usize::from(m & 7));
        if mode == 3 {
            return Some((regf, Rm::Reg(rm)));
        }
        let mut addr = 0u32;
        let mut base = Some(rm);
        if rm == 4 {
            let sib = self.fetch8()?;
            let (scale, index, b) = (sib >> 6, usize::from((sib >> 3) & 7), usize::from(sib & 7));
            if index != 4 {
                addr = self.r[index] << scale;
            }
            base = Some(b);
        }
        if mode == 0 && base == Some(5) {
            base = None;
            addr = addr.wrapping_add(self.fetch32()?);
        }
        if let Some(b) = base {
            addr = addr.wrapping_add(self.r[b]);
        }
        match mode {
            1 => addr = addr.wrapping_add(self.fetch8s()?),
            2 => addr = addr.wrapping_add(self.fetch32()?),
            _ => {}
        }
        Some((regf, Rm::Mem(addr)))
    }

    fn get(&self, rm: Rm, bits: u32) -> Option<u32> {
        match rm {
            Rm::Reg(i) if bits == 8 => Some(self.reg8(i)),
            Rm::Reg(i) => Some(self.r[i]),
            Rm::Mem(a) => self.load(a, bits / 8),
        }
    }

    fn set(&mut self, rm: Rm, bits: u32, v: u32) -> Option<()> {
        match rm {
            Rm::Reg(i) if bits == 8 => self.set_reg8(i, v),
            Rm::Reg(i) => self.r[i] = v,
            Rm::Mem(a) => self.store(a, bits / 8, v)?,
        }
        Some(())
    }

    fn get_reg(&self, i: usize, bits: u32) -> u32 {
        if bits == 8 { self.reg8(i) } else { self.r[i] }
    }

    fn set_reg(&mut self, i: usize, bits: u32, v: u32) {
        if bits == 8 { self.set_reg8(i, v) } else { self.r[i] = v }
    }

    fn imm(&mut self, bits: u32) -> Option<u32> {
        if bits == 8 { Some(u32::from(self.fetch8()?)) } else { self.fetch32() }
    }

    fn push(&mut self, v: u32) -> Option<()> {
        self.r[reg::ESP] = self.r[reg::ESP].wrapping_sub(4);
        self.store(self.r[reg::ESP], 4, v)
    }

    fn pop(&mut self) -> Option<u32> {
        let v = self.load(self.r[reg::ESP], 4)?;
        self.r[reg::ESP] = self.r[reg::ESP].wrapping_add(4);
        Some(v)
    }

    /// The eight operations of the 00h to 3Fh block and of group 1 (ADD, OR,
    /// ADC, SBB, AND, SUB, XOR, CMP): sets the flags, returns the result
    /// (which CMP's caller drops).
    fn alu(&mut self, kind: usize, a: u32, b: u32, bits: u32) -> u32 {
        let mask = if bits == 8 { 0xFFu64 } else { 0xFFFF_FFFF };
        let sign = 1u64 << (bits - 1);
        let (a, b) = (u64::from(a) & mask, u64::from(b) & mask);
        let c = u64::from(self.fl & CF != 0);
        let (res, cf, of) = match kind {
            0 | 2 => {
                let s = a + b + if kind == 2 { c } else { 0 };
                (s & mask, s > mask, (a ^ s) & (b ^ s) & sign != 0)
            }
            3 | 5 | 7 => {
                let sub = b + if kind == 3 { c } else { 0 };
                let d = a.wrapping_sub(sub) & mask;
                (d, a < sub, (a ^ b) & (a ^ d) & sign != 0)
            }
            1 => (a | b, false, false),
            4 => (a & b, false, false),
            _ => (a ^ b, false, false),
        };
        self.fl &= !FLAGS;
        if cf {
            self.fl |= CF;
        }
        if of {
            self.fl |= OF;
        }
        self.result_flags(res, sign);
        res as u32
    }

    /// ZF, SF and PF of a result.
    fn result_flags(&mut self, res: u64, sign: u64) {
        self.fl &= !(ZF | SF | PF);
        if res == 0 {
            self.fl |= ZF;
        }
        if res & sign != 0 {
            self.fl |= SF;
        }
        if (res as u8).count_ones().is_multiple_of(2) {
            self.fl |= PF;
        }
    }

    /// A condition code (the low four bits of Jcc and SETcc).
    fn cond(&self, cc: u8) -> bool {
        let f = |b| self.fl & b != 0;
        let v = match cc >> 1 {
            0 => f(OF),
            1 => f(CF),
            2 => f(ZF),
            3 => f(CF) || f(ZF),
            4 => f(SF),
            5 => f(PF),
            6 => f(SF) != f(OF),
            _ => f(ZF) || f(SF) != f(OF),
        };
        v != (cc & 1 != 0)
    }

    fn jump(&mut self, rel: u32) {
        self.eip = self.eip.wrapping_add(rel);
    }

    /// One instruction; `None` for one this interpreter does not know, a
    /// store it does not allow or memory that is not RAM.
    fn step(&mut self) -> Option<()> {
        let op = self.fetch8()?;
        // byte forms have bit 0 clear in every block used here
        let bits = if op & 1 == 0 { 8 } else { 32 };
        match op {
            0x00..=0x3F if op & 7 < 6 => {
                let kind = usize::from(op >> 3);
                match op & 7 {
                    0 | 1 => {
                        let (regf, rm) = self.modrm()?;
                        let v = self.alu(kind, self.get(rm, bits)?, self.get_reg(regf, bits), bits);
                        if kind != 7 {
                            self.set(rm, bits, v)?;
                        }
                    }
                    2 | 3 => {
                        let (regf, rm) = self.modrm()?;
                        let v = self.alu(kind, self.get_reg(regf, bits), self.get(rm, bits)?, bits);
                        if kind != 7 {
                            self.set_reg(regf, bits, v);
                        }
                    }
                    _ => {
                        let b = self.imm(bits)?;
                        let v = self.alu(kind, self.get_reg(reg::EAX, bits), b, bits);
                        if kind != 7 {
                            self.set_reg(reg::EAX, bits, v);
                        }
                    }
                }
            }
            0x40..=0x4F => {
                // INC and DEC leave CF alone
                let i = usize::from(op & 7);
                let cf = self.fl & CF;
                self.r[i] = self.alu(if op < 0x48 { 0 } else { 5 }, self.r[i], 1, 32);
                self.fl = (self.fl & !CF) | cf;
            }
            0x50..=0x57 => self.push(self.r[usize::from(op & 7)])?,
            0x58..=0x5F => {
                let v = self.pop()?;
                self.r[usize::from(op & 7)] = v;
            }
            0x68 => {
                let v = self.fetch32()?;
                self.push(v)?;
            }
            0x6A => {
                let v = self.fetch8s()?;
                self.push(v)?;
            }
            0x70..=0x7F => {
                let rel = self.fetch8s()?;
                if self.cond(op & 15) {
                    self.jump(rel);
                }
            }
            0x80 | 0x81 | 0x83 => {
                let (kind, rm) = self.modrm()?;
                let b = if op == 0x83 { self.fetch8s()? } else { self.imm(bits)? };
                let v = self.alu(kind, self.get(rm, bits)?, b, bits);
                if kind != 7 {
                    self.set(rm, bits, v)?;
                }
            }
            0x84 | 0x85 => {
                let (regf, rm) = self.modrm()?;
                self.alu(4, self.get(rm, bits)?, self.get_reg(regf, bits), bits);
            }
            0x88 | 0x89 => {
                let (regf, rm) = self.modrm()?;
                self.set(rm, bits, self.get_reg(regf, bits))?;
            }
            0x8A | 0x8B => {
                let (regf, rm) = self.modrm()?;
                let v = self.get(rm, bits)?;
                self.set_reg(regf, bits, v);
            }
            0x8D => {
                let (regf, rm) = self.modrm()?;
                let Rm::Mem(a) = rm else { return None };
                self.r[regf] = a;
            }
            0x90 => {}
            0xF3 => (self.fetch8()? == 0x90).then_some(())?, // PAUSE
            0xA0 | 0xA1 => {
                let a = self.fetch32()?;
                let v = self.load(a, bits / 8)?;
                self.set_reg(reg::EAX, bits, v);
            }
            0xA2 | 0xA3 => {
                let a = self.fetch32()?;
                self.store(a, bits / 8, self.get_reg(reg::EAX, bits))?;
            }
            0xA8 | 0xA9 => {
                let b = self.imm(bits)?;
                self.alu(4, self.get_reg(reg::EAX, bits), b, bits);
            }
            0xB0..=0xB7 => {
                let v = self.imm(8)?;
                self.set_reg8(usize::from(op & 7), v);
            }
            0xB8..=0xBF => self.r[usize::from(op & 7)] = self.fetch32()?,
            0xC2 | 0xC3 => {
                let extra = if op == 0xC2 {
                    u32::from(self.fetch8()?) | u32::from(self.fetch8()?) << 8
                } else {
                    0
                };
                self.eip = self.pop()?;
                self.r[reg::ESP] = self.r[reg::ESP].wrapping_add(extra);
            }
            0xC6 | 0xC7 => {
                let (regf, rm) = self.modrm()?;
                (regf == 0).then_some(())?;
                let v = self.imm(bits)?;
                self.set(rm, bits, v)?;
            }
            0xC9 => {
                self.r[reg::ESP] = self.r[reg::EBP];
                self.r[reg::EBP] = self.pop()?;
            }
            0xE8 => {
                let rel = self.fetch32()?;
                self.push(self.eip)?;
                self.jump(rel);
            }
            0xE9 => {
                let rel = self.fetch32()?;
                self.jump(rel);
            }
            0xEB => {
                let rel = self.fetch8s()?;
                self.jump(rel);
            }
            0xF6 | 0xF7 => {
                let (regf, rm) = self.modrm()?;
                (regf == 0).then_some(())?; // TEST
                let b = self.imm(bits)?;
                self.alu(4, self.get(rm, bits)?, b, bits);
            }
            0xFF => {
                let (regf, rm) = self.modrm()?;
                let v = self.get(rm, 32)?;
                match regf {
                    2 => {
                        self.push(self.eip)?;
                        self.eip = v;
                    }
                    4 => self.eip = v,
                    6 => self.push(v)?,
                    _ => return None,
                }
            }
            0x0F => {
                let op2 = self.fetch8()?;
                match op2 {
                    0x80..=0x8F => {
                        let rel = self.fetch32()?;
                        if self.cond(op2 & 15) {
                            self.jump(rel);
                        }
                    }
                    0x90..=0x9F => {
                        let (_, rm) = self.modrm()?;
                        self.set(rm, 8, u32::from(self.cond(op2 & 15)))?;
                    }
                    0xB6 | 0xB7 | 0xBE | 0xBF => {
                        let from = if op2 & 1 == 0 { 8 } else { 16 };
                        let (regf, rm) = self.modrm()?;
                        let v = match rm {
                            Rm::Reg(i) if from == 8 => self.reg8(i),
                            Rm::Reg(i) => self.r[i] & 0xFFFF,
                            Rm::Mem(a) => self.load(a, from / 8)?,
                        };
                        self.r[regf] = match (op2, from) {
                            (0xBE, _) => v as u8 as i8 as u32,
                            (0xBF, _) => v as u16 as i16 as u32,
                            _ => v,
                        };
                    }
                    _ => return None,
                }
            }
            _ => return None,
        }
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// RAM from 0x1000 on with `code` at 0x1_0000, a stack under 0x8000 and
    /// nothing mapped at 0xFD00_0000 (a device).
    struct Mem(RefCell<Vec<u8>>);

    const CODE: u32 = 0x1_0000;
    const DATA: u32 = 0x2_0000;

    impl Mem {
        fn new(code: &[u8]) -> Self {
            let mut m = vec![0u8; 0x4_0000];
            m[CODE as usize..CODE as usize + code.len()].copy_from_slice(code);
            Self(RefCell::new(m))
        }

        fn w32(&self, a: u32, v: u32) {
            self.0.borrow_mut()[a as usize..a as usize + 4].copy_from_slice(&v.to_le_bytes());
        }

        fn proves(&self, regs: &Regs) -> bool {
            let read = |a: u32, buf: &mut [u8]| {
                let m = self.0.borrow();
                let (a, n) = (a as usize, buf.len());
                if a < 0x1000 || a + n > m.len() {
                    return false;
                }
                buf.copy_from_slice(&m[a..a + n]);
                true
            };
            is_wait_loop(regs, &Env { read: &read, trap: (0x3_0000, 0x3_1000) })
        }
    }

    fn regs_at(eip: u32) -> Regs {
        let mut r = Regs { eip, eflags: 0x202, ..Regs::default() };
        r.gpr[reg::ESP] = 0x8000;
        r
    }

    /// Halo's wait for a vertical blank, as its main thread runs it at the
    /// menu: a 64-bit count at DATA against the target in ESI:EDI.
    fn halo_loop() -> Vec<u8> {
        let mut c = vec![0xA1];
        c.extend(DATA.to_le_bytes()); // top: mov eax, [count]
        c.extend([0x39, 0x35]);
        c.extend((DATA + 4).to_le_bytes()); // cmp [count+4], esi
        c.extend([0x7C, 0xF3]); // jl top
        c.extend([0x7F, 0x04]); // jg out
        c.extend([0x3B, 0xC7]); // cmp eax, edi
        c.extend([0x72, 0xED]); // jb top
        c.push(0xC3); // out: ret
        c
    }

    #[test]
    fn a_loop_that_only_compares_memory_is_proven_from_each_of_its_instructions() {
        let m = Mem::new(&halo_loop());
        m.w32(DATA, 5);
        for at in [0u32, 5, 11, 13, 15, 17] {
            let mut r = regs_at(CODE + at);
            r.gpr[reg::EDI] = 9; // the count has not reached the target
            if at >= 5 {
                r.gpr[reg::EAX] = 5; // loaded already
            }
            if at >= 11 {
                r.eflags |= ZF | PF; // the high halves compared equal
            }
            if at >= 17 {
                r.eflags = 0x202 | CF | SF | PF; // 5 below 9
            }
            assert!(m.proves(&r), "from +{at}");
        }
    }

    #[test]
    fn the_same_loop_is_not_proven_once_the_count_reaches_its_target() {
        let m = Mem::new(&halo_loop());
        let mut r = regs_at(CODE);
        r.gpr[reg::EDI] = 9;
        m.w32(DATA, 8);
        assert!(m.proves(&r));
        m.w32(DATA, 9);
        assert!(!m.proves(&r), "the loop leaves: it must run");
    }

    #[test]
    fn a_loop_that_counts_in_memory_or_in_a_register_is_not_proven() {
        // inc dword [DATA]; jmp -8
        let mut c = vec![0xFF, 0x05];
        c.extend(DATA.to_le_bytes());
        c.extend([0xEB, 0xF8]);
        assert!(!Mem::new(&c).proves(&regs_at(CODE)), "INC of memory is not known at all");
        // add dword [DATA], 1; jmp -9
        let mut c = vec![0x83, 0x05];
        c.extend(DATA.to_le_bytes());
        c.extend([0x01, 0xEB, 0xF7]);
        assert!(!Mem::new(&c).proves(&regs_at(CODE)), "a store that changes memory");
        // inc ecx; jmp -3
        assert!(!Mem::new(&[0x41, 0xEB, 0xFD]).proves(&regs_at(CODE)), "a register that moves");
    }

    #[test]
    fn a_loop_that_calls_a_function_to_read_its_flag_is_proven() {
        // top: push 1; call f; add esp, 4; test eax, eax; jz top; ret
        // f:   push ebp; mov ebp, esp; mov eax, [DATA]; pop ebp; ret
        let mut c =
            vec![0x6A, 0x01, 0xE8, 0x08, 0, 0, 0, 0x83, 0xC4, 0x04, 0x85, 0xC0, 0x74, 0xF2, 0xC3];
        c.extend([0x55, 0x8B, 0xEC, 0xA1]);
        c.extend(DATA.to_le_bytes());
        c.extend([0x5D, 0xC3]);
        let m = Mem::new(&c);
        assert!(m.proves(&regs_at(CODE)), "from the caller");
        // stopped inside f, after its prologue: the caller's pushes are
        // above the stack pointer now and must store what is there
        let mut r = regs_at(CODE + 18);
        r.gpr[reg::ESP] = 0x8000 - 12;
        r.gpr[reg::EBP] = 0x8000 - 12;
        m.w32(0x8000 - 12, 0); // saved ebp
        m.w32(0x8000 - 8, CODE + 7); // return address
        m.w32(0x8000 - 4, 1); // the argument
        assert!(m.proves(&r), "from the function");
        m.w32(DATA, 1);
        assert!(!m.proves(&r), "the flag is set: the loop leaves");
    }

    #[test]
    fn straight_code_that_calls_one_function_twice_is_not_proven() {
        // Tiger Woods at 000b0486: call f; ...; call f; call f; and on. At
        // f's first instruction the second and third time the registers are
        // the same and only the return address on the stack differs.
        //   call f; call f; call f; rdtsc (the walk gives up there)
        //   f: mov eax, 0x22fff0; ret
        let mut c = vec![0xE8, 0x0C, 0, 0, 0, 0xE8, 0x07, 0, 0, 0, 0xE8, 0x02, 0, 0, 0, 0x0F, 0x31];
        c.extend([0xB8, 0xF0, 0xFF, 0x22, 0x00, 0xC3]);
        assert!(!Mem::new(&c).proves(&regs_at(CODE)));
        // the same calls in a loop are a wait loop: the stack repeats too
        //   top: call f; call f; jmp top
        let mut c = vec![0xE8, 0x07, 0, 0, 0, 0xE8, 0x02, 0, 0, 0, 0xEB, 0xF4];
        c.extend([0xB8, 0xF0, 0xFF, 0x22, 0x00, 0xC3]);
        assert!(Mem::new(&c).proves(&regs_at(CODE)));
    }

    #[test]
    fn a_load_of_stack_the_walk_did_not_store_is_no_proof() {
        // mov eax, [esp-8]; jmp -6
        assert!(!Mem::new(&[0x8B, 0x44, 0x24, 0xF8, 0xEB, 0xFA]).proves(&regs_at(CODE)));
    }

    #[test]
    fn a_loop_polling_a_device_register_or_the_kernel_is_not_proven() {
        // mov eax, [0xFD003244]; test eax, eax; jz -9
        let c = [0xA1, 0x44, 0x32, 0x00, 0xFD, 0x85, 0xC0, 0x74, 0xF7];
        assert!(!Mem::new(&c).proves(&regs_at(CODE)), "a device register");
        // call [DATA] with a kernel entry there; test eax, eax; jz -10
        let mut c = vec![0xFF, 0x15];
        c.extend(DATA.to_le_bytes());
        c.extend([0x85, 0xC0, 0x74, 0xF6]);
        let m = Mem::new(&c);
        m.w32(DATA, 0x3_0010);
        assert!(!m.proves(&regs_at(CODE)), "a kernel call");
        // rdtsc; jmp -4
        assert!(
            !Mem::new(&[0x0F, 0x31, 0xEB, 0xFC]).proves(&regs_at(CODE)),
            "the time stamp counter"
        );
    }

    #[test]
    fn straight_code_and_a_long_loop_are_not_proven() {
        // mov eax, 1; ret (to address 0: not RAM)
        assert!(!Mem::new(&[0xB8, 1, 0, 0, 0, 0xC3]).proves(&regs_at(CODE)));
        // 600 NOPs and a jump back: over the walk's length
        let mut c = vec![0x90; 600];
        c.extend([0xE9]);
        c.extend((-605i32).to_le_bytes());
        assert!(!Mem::new(&c).proves(&regs_at(CODE)));
    }

    #[test]
    fn the_flags_are_those_of_the_processor() {
        // (kind, a, b, bits) -> flags, from the instruction set reference
        let read = |_: u32, _: &mut [u8]| false;
        let env = Env { read: &read, trap: (0, 0) };
        let mut w = Walk { r: [0; 8], eip: 0, fl: 0, env: &env, top: 0, below: Vec::new() };
        let mut flags = |kind, a, b, bits| {
            w.fl = 0;
            let v = w.alu(kind, a, b, bits);
            (v, w.fl)
        };
        assert_eq!(flags(5, 5, 9, 32), (0xFFFF_FFFC, CF | SF | PF));
        assert_eq!(flags(5, 9, 9, 32), (0, ZF | PF));
        assert_eq!(flags(5, 0x8000_0000, 1, 32), (0x7FFF_FFFF, OF | PF));
        assert_eq!(flags(0, 0x7F, 1, 8), (0x80, OF | SF));
        assert_eq!(flags(0, 0xFF, 1, 8), (0, CF | ZF | PF));
        assert_eq!(flags(4, 0xF0, 0x0F, 8), (0, ZF | PF));
        assert_eq!(flags(6, 3, 1, 32), (2, 0));
    }
}
