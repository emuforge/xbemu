//! A small x86-64 emitter over `iced-x86`'s encoder.
//!
//! Code is assembled into a buffer for a known final address (so relative
//! operands come out right) and copied into the code region afterwards.
//! Jumps that are patched later (block chaining) are written by hand in
//! their `rel32` forms.

use iced_x86::{Code, Encoder, Instruction, MemoryOperand, Register};

use crate::frame::Frame;

/// An instruction that could not be built or encoded.
pub(crate) type AsmError = String;

/// An operand of an instruction built by [`Asm::op1`] or [`Asm::op2`].
pub(crate) enum Op {
    /// A register.
    R(Register),
    /// Memory.
    M(MemoryOperand),
    /// An immediate (its low bits, for narrower operands).
    I(u32),
}

impl From<Register> for Op {
    fn from(r: Register) -> Self {
        Op::R(r)
    }
}

impl From<MemoryOperand> for Op {
    fn from(m: MemoryOperand) -> Self {
        Op::M(m)
    }
}

impl From<u32> for Op {
    fn from(v: u32) -> Self {
        Op::I(v)
    }
}

impl From<i32> for Op {
    fn from(v: i32) -> Self {
        Op::I(v as u32)
    }
}

/// The emitter.
pub(crate) struct Asm {
    enc: Encoder,
    buf: Vec<u8>,
    base: u64,
}

impl Asm {
    /// An emitter for code that will live at `base`.
    pub(crate) fn new(base: u64) -> Self {
        Self { enc: Encoder::new(64), buf: Vec::with_capacity(1024), base }
    }

    /// Bytes emitted so far.
    pub(crate) fn len(&self) -> usize {
        self.buf.len()
    }

    /// Final address of the next byte.
    pub(crate) fn ip(&self) -> u64 {
        self.base + self.buf.len() as u64
    }

    /// The code.
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.buf
    }

    /// Drops everything after `len` (an instruction whose translation failed
    /// half way).
    pub(crate) fn truncate(&mut self, len: usize) {
        self.buf.truncate(len);
    }

    /// Encodes one instruction.
    pub(crate) fn ins(&mut self, i: &Instruction) -> Result<(), AsmError> {
        let ip = self.ip();
        match self.enc.encode(i, ip) {
            Ok(_) => {
                let mut b = self.enc.take_buffer();
                self.buf.extend_from_slice(&b);
                b.clear();
                self.enc.set_buffer(b);
                Ok(())
            }
            Err(e) => {
                let mut b = self.enc.take_buffer();
                b.clear();
                self.enc.set_buffer(b);
                Err(format!("{:?}: {e}", i.code()))
            }
        }
    }

    /// Builds and encodes `code op0, op1`.
    pub(crate) fn op2(
        &mut self,
        code: Code,
        a: impl Into<Op>,
        b: impl Into<Op>,
    ) -> Result<(), AsmError> {
        let i = match (a.into(), b.into()) {
            (Op::R(a), Op::R(b)) => Instruction::with2(code, a, b),
            (Op::R(a), Op::M(b)) => Instruction::with2(code, a, b),
            (Op::R(a), Op::I(b)) => Instruction::with2(code, a, b),
            (Op::M(a), Op::R(b)) => Instruction::with2(code, a, b),
            (Op::M(a), Op::I(b)) => Instruction::with2(code, a, b),
            _ => return Err(format!("{code:?}: operand combination")),
        }
        .map_err(|e| format!("{code:?}: {e}"))?;
        self.ins(&i)
    }

    /// Builds and encodes `code op0`.
    pub(crate) fn op1(&mut self, code: Code, a: impl Into<Op>) -> Result<(), AsmError> {
        let i = match a.into() {
            Op::R(a) => Instruction::with1(code, a),
            Op::M(a) => Instruction::with1(code, a),
            Op::I(a) => Instruction::with1(code, a),
        }
        .map_err(|e| format!("{code:?}: {e}"))?;
        self.ins(&i)
    }

    /// Encodes an instruction without operands.
    pub(crate) fn op0(&mut self, code: Code) -> Result<(), AsmError> {
        self.ins(&Instruction::with(code))
    }

    /// Raw bytes.
    pub(crate) fn raw(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }

    fn rel32(&mut self, target: u64) -> usize {
        let at = self.buf.len();
        let rel = target.wrapping_sub(self.base + at as u64 + 4) as u32;
        self.buf.extend_from_slice(&rel.to_le_bytes());
        at
    }

    /// `jmp rel32`; returns the offset of the `rel32` for patching.
    pub(crate) fn jmp(&mut self, target: u64) -> usize {
        self.buf.push(0xE9);
        self.rel32(target)
    }

    /// `jcc rel32` (`cc`: the condition's encoding, 0 to 15).
    pub(crate) fn jcc(&mut self, cc: u8, target: u64) -> usize {
        self.buf.extend_from_slice(&[0x0F, 0x80 | (cc & 15)]);
        self.rel32(target)
    }

    /// `call rel32`.
    pub(crate) fn call(&mut self, target: u64) -> usize {
        self.buf.push(0xE8);
        self.rel32(target)
    }

    /// `jrcxz rel8` to a place not known yet; returns the offset of the
    /// `rel8` for [`Asm::bind8`].
    pub(crate) fn jrcxz(&mut self) -> usize {
        self.buf.extend_from_slice(&[0xE3, 0]);
        self.buf.len() - 1
    }

    /// A raw `op rel8` branch (`jz`, `jnz`) to a place not known yet.
    pub(crate) fn short(&mut self, opcode: u8) -> usize {
        self.buf.extend_from_slice(&[opcode, 0]);
        self.buf.len() - 1
    }

    /// Points a `rel8` at the current position.
    pub(crate) fn bind8(&mut self, at: usize) {
        let d = self.buf.len() - (at + 1);
        debug_assert!(d < 128);
        self.buf[at] = d as u8;
    }

    /// Points the `rel32` at offset `at` of this buffer at `target`.
    pub(crate) fn patch(&mut self, at: usize, target: u64) {
        let rel = target.wrapping_sub(self.base + at as u64 + 4) as u32;
        self.buf[at..at + 4].copy_from_slice(&rel.to_le_bytes());
    }

    /// `mov dword [r15 + off], imm`.
    pub(crate) fn frame_set(&mut self, off: i64, v: u32) -> Result<(), AsmError> {
        self.op2(Code::Mov_rm32_imm32, fr(off), v)
    }

    /// Leaves translated code: stores the reason and the guest `eip`, then
    /// jumps to the exit stub.
    pub(crate) fn exit(&mut self, reason: u32, eip: u32, exit_stub: u64) -> Result<(), AsmError> {
        self.frame_set(Frame::EIP, eip)?;
        self.frame_set(Frame::REASON, reason)?;
        self.jmp(exit_stub);
        Ok(())
    }
}

/// `[r15 + off]`: a field of the frame.
pub(crate) fn fr(off: i64) -> MemoryOperand {
    MemoryOperand::with_base_displ(Register::R15, off)
}

/// `gs:[base + disp]` with a 32-bit base register: guest memory.
pub(crate) fn gmem(base: Register, disp: i32) -> MemoryOperand {
    MemoryOperand::new(
        base,
        Register::None,
        1,
        i64::from(disp),
        if disp == 0 { 0 } else { 1 },
        false,
        Register::GS,
    )
}

/// Points the `rel32` stored at host address `at` at `target`.
///
/// # Safety
/// `at` must be the address of a `rel32` inside the code region, and no
/// thread may be executing the instruction it belongs to.
pub(crate) unsafe fn patch_rel32(at: *mut u8, target: u64) {
    let rel = target.wrapping_sub(at as u64 + 4) as u32;
    // SAFETY: the caller guarantees `at` addresses four writable bytes.
    unsafe { at.cast::<u32>().write_unaligned(rel) };
}
