//! The fixed pieces of host code around translations: the way in, the way
//! out, and the dispatcher.
//!
//! There is one exit path and it always saves the whole state (FXSAVE
//! included), because every exit returns to `X64Cpu::run` rather than
//! calling the host from inside the stub; the translated code runs on its
//! own stack; and the dispatcher's table has two entries per slot.
//!
//! Register use in translated code:
//!
//! | Host | Holds |
//! |---|---|
//! | `RAX RCX RDX RBX RBP RSI RDI` | the guest's registers of the same name |
//! | `R12` | guest `ESP` |
//! | `R15` | the frame |
//! | `R14` | guest FS base |
//! | `R8` | host address of guest address 0 (also `GS`'s base) |
//! | `R9 R10 R11 R13` | scratch (`R13D`: the guest `eip` given to the dispatcher) |
//! | `RSP` | the shadow stack of return predictions |
//! | flags | the guest's arithmetic flags and DF |

use iced_x86::BlockEncoderOptions;
use iced_x86::code_asm::*;

use crate::frame::{Frame, reason};

/// Bits of the dispatcher's table index (taken from the low bits of `eip`).
pub(crate) const L1_BITS: u32 = 16;
/// Bytes per table slot: two `(u32 -eip, u32 pad, u64 code)` entries.
pub(crate) const L1_SLOT: usize = 32;

/// Addresses of the stubs.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Stubs {
    /// `extern "sysv64" fn(frame: *mut Frame)`: runs translated code from
    /// `frame.next_code` until it leaves.
    pub(crate) enter: u64,
    /// Common way out; `frame.reason` and `frame.eip` are already set.
    pub(crate) exit: u64,
    /// Where the fault handler sends a faulting thread.
    pub(crate) fault: u64,
    /// Finds the translation of the guest address in `R13D`.
    pub(crate) dispatch: u64,
    /// Leaves with [`reason::LOOKUP`] for the address in `R13D`; also what
    /// empty table entries point at.
    pub(crate) lookup: u64,
    /// Where a return lands when it pops the bottom of the shadow stack.
    pub(crate) ret_reset: u64,
}

fn ierr(e: IcedError) -> String {
    format!("assembling the stubs: {e}")
}

/// Assembles the stubs for address `at`. `l1` is the dispatcher's table;
/// `wrgsbase` says whether the enter stub sets `GS`'s base itself.
pub(crate) fn build(at: u64, l1: u64, wrgsbase: bool) -> Result<(Vec<u8>, Stubs), String> {
    let mut a = CodeAssembler::new(64).map_err(ierr)?;
    let guest = [rax, rcx, rdx, rbx, r12, rbp, rsi, rdi];
    let callee = [rbx, rbp, r12, r13, r14, r15];
    let f = |off: i64| r15 + off;

    let mut enter = a.create_label();
    let mut exit = a.create_label();
    let mut fault = a.create_label();
    let mut dispatch = a.create_label();
    let mut lookup = a.create_label();
    let mut ret_reset = a.create_label();
    let mut hit0 = a.create_label();
    let mut hit1 = a.create_label();

    (|| -> Result<(), IcedError> {
        // enter(frame)
        a.set_label(&mut enter)?;
        for r in callee {
            a.push(r)?;
        }
        a.mov(r15, rdi)?;
        a.mov(qword_ptr(f(Frame::HOST_RSP)), rsp)?;
        a.stmxcsr(dword_ptr(f(Frame::HOST_MXCSR)))?;
        a.mov(rsp, qword_ptr(f(Frame::SHADOW_TOP)))?;
        a.mov(r8, qword_ptr(f(Frame::MEM_BASE)))?;
        if wrgsbase {
            a.wrgsbase(r8)?;
        }
        a.mov(rax, qword_ptr(f(Frame::FX_PTR)))?;
        a.fxrstor(ptr(rax))?;
        a.mov(r14, qword_ptr(f(Frame::FS_BASE)))?;
        for (i, r) in guest.into_iter().enumerate() {
            a.mov(r, qword_ptr(f(Frame::GREGS + 8 * i as i64)))?;
        }
        a.push(qword_ptr(f(Frame::EFLAGS)))?;
        a.popfq()?;
        a.jmp(qword_ptr(f(Frame::NEXT_CODE)))?;

        // ret_reset: a return popped the bottom of the shadow stack (it
        // matches only a return to address 0): empty it, dispatch.
        a.set_label(&mut ret_reset)?;
        a.mov(rsp, qword_ptr(f(Frame::SHADOW_TOP)))?;
        a.jmp(dispatch)?;

        // fault: the handler redirected a faulting instruction here with
        // every register as it was.
        a.set_label(&mut fault)?;
        a.mov(dword_ptr(f(Frame::REASON)), reason::FAULT)?;
        a.jmp(exit)?;

        // dispatch: guest eip in r13d, flags live. Two probes of the table
        // without touching the flags: an entry holds -eip, and
        // lea ecx, [rcx + r13] is zero on a hit (JRCXZ).
        a.set_label(&mut dispatch)?;
        a.movzx(r11d, r13w)?;
        a.mov(r9, l1)?;
        a.lea(r11, ptr(r11 * 8))?;
        a.lea(r11, ptr(r11 * 4))?;
        a.mov(r10, rcx)?;
        a.mov(ecx, dword_ptr(r9 + r11))?;
        a.lea(ecx, ptr(rcx + r13))?;
        a.jrcxz(hit0)?;
        a.mov(ecx, dword_ptr(r9 + r11 + 16))?;
        a.lea(ecx, ptr(rcx + r13))?;
        a.jrcxz(hit1)?;
        a.mov(rcx, r10)?;
        // lookup: no translation known for r13d.
        a.set_label(&mut lookup)?;
        a.mov(dword_ptr(f(Frame::EIP)), r13d)?;
        a.mov(dword_ptr(f(Frame::REASON)), reason::LOOKUP)?;
        a.jmp(exit)?;
        a.set_label(&mut hit0)?;
        a.mov(rcx, r10)?;
        a.jmp(qword_ptr(r9 + r11 + 8))?;
        a.set_label(&mut hit1)?;
        a.mov(rcx, r10)?;
        a.jmp(qword_ptr(r9 + r11 + 24))?;

        // exit: the guest's registers, flags and x87/SSE state into the
        // frame and the context; the host's environment back (MXCSR, an
        // empty x87 with the default control word, DF clear); return to
        // enter's caller.
        a.set_label(&mut exit)?;
        for (i, r) in guest.into_iter().enumerate() {
            a.mov(qword_ptr(f(Frame::GREGS + 8 * i as i64)), r)?;
        }
        a.pushfq()?;
        a.pop(qword_ptr(f(Frame::EFLAGS)))?;
        a.mov(rax, qword_ptr(f(Frame::FX_PTR)))?;
        a.fxsave(ptr(rax))?;
        a.fninit()?;
        a.ldmxcsr(dword_ptr(f(Frame::HOST_MXCSR)))?;
        a.cld()?;
        a.mov(rsp, qword_ptr(f(Frame::HOST_RSP)))?;
        for r in callee.into_iter().rev() {
            a.pop(r)?;
        }
        a.ret()?;
        Ok(())
    })()
    .map_err(ierr)?;

    let r = a
        .assemble_options(at, BlockEncoderOptions::RETURN_NEW_INSTRUCTION_OFFSETS)
        .map_err(ierr)?;
    let ip = |l: &CodeLabel| r.label_ip(l).map_err(ierr);
    let stubs = Stubs {
        enter: ip(&enter)?,
        exit: ip(&exit)?,
        fault: ip(&fault)?,
        dispatch: ip(&dispatch)?,
        lookup: ip(&lookup)?,
        ret_reset: ip(&ret_reset)?,
    };
    Ok((r.inner.code_buffer, stubs))
}
