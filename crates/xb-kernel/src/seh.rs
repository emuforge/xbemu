//! Structured exception handling: RtlRaiseException and RtlUnwind, as the
//! Xbox kernel runs x86 SEH.
//!
//! Handlers are title code, so the kernel's loops over the registration
//! chain (`fs:[0]`) run one handler at a time: each is called on the guest
//! stack with its return address at the SEH-return sentinel, and the loop
//! goes on when it returns there. A thread keeps a level per dispatch or
//! unwind in progress, since handlers raise and unwind in turn (a C++
//! handler unwinds from inside the dispatch that found its catch). Around
//! each call the kernel links a registration of its own, as NT's
//! RtlpExecuteHandler2 does: an exception raised inside a handler reports
//! the frame being handled (nested), and an unwind that runs into one in
//! progress takes over where it was (collided).
//!
//! A C++ catch leaves by jumping to its continuation and never returns to
//! the dispatch: a level whose frame lies below the stack pointer is gone.

use xb_cpu::reg;
use xb_types::PAGE_SIZE;

use crate::gm::KResult;
use crate::kernel::{
    Args, Kernel, ORD_SEH_COLLIDED, ORD_SEH_NESTED, ORD_SEH_RETURN, STATUS_ACCESS_VIOLATION,
};
use crate::platform::Platform;

// EXCEPTION_RECORD
const ER_CODE: u32 = 0x00;
const ER_FLAGS: u32 = 0x04;
const ER_ADDRESS: u32 = 0x0C;
const ER_NPARAMS: u32 = 0x10;
const ER_INFO: u32 = 0x14;
const ER_SIZE: u32 = 0x50;

const EXCEPTION_NONCONTINUABLE: u32 = 0x01;
const EXCEPTION_UNWINDING: u32 = 0x02;
const EXCEPTION_EXIT_UNWIND: u32 = 0x04;
const EXCEPTION_STACK_INVALID: u32 = 0x08;
const EXCEPTION_NESTED_CALL: u32 = 0x10;

const CONTINUE_EXECUTION: u32 = 0;
const CONTINUE_SEARCH: u32 = 1;
const NESTED_EXCEPTION: u32 = 2;
const COLLIDED_UNWIND: u32 = 3;

// CONTEXT, the Xbox's: FloatSave is the FXSAVE image plus Cr0NpxState; no
// debug registers and no data segment registers.
const CX_FLAGS: u32 = 0x000;
const CX_FLOAT: u32 = 0x004;
const CX_EDI: u32 = 0x208;
const CX_ESI: u32 = 0x20C;
const CX_EBX: u32 = 0x210;
const CX_EDX: u32 = 0x214;
const CX_ECX: u32 = 0x218;
const CX_EAX: u32 = 0x21C;
const CX_EBP: u32 = 0x220;
const CX_EIP: u32 = 0x224;
const CX_CS: u32 = 0x228;
const CX_EFLAGS: u32 = 0x22C;
const CX_ESP: u32 = 0x230;
const CX_SS: u32 = 0x234;
const CX_SIZE: u32 = 0x238;
const CONTEXT_FULL: u32 = 0x10007; // control, integer, segments

const CHAIN_END: u32 = 0xFFFF_FFFF;
const STATUS_UNWIND: u32 = 0xC000_0027;
const CXX_EXCEPTION: u32 = 0xE06D_7363; // MSVC's throw

// A level's guest frame, from its base up:
//   +0   return address, the SEH-return sentinel (ESP as the handler starts)
//   +4   ExceptionRecord, EstablisherFrame, ContextRecord, DispatcherContext
//   +20  DispatcherContext: the frame a nested raise or collided unwind reports
//   +24  the kernel's registration around the call: Next, Handler, the frame
//        being handled
//   +36  CONTEXT; then the EXCEPTION_RECORD of an unwind given none
const LV_DISP: u32 = 20;
const LV_REG: u32 = 24;
const LV_CTX: u32 = 36;
const LV_REC: u32 = LV_CTX + CX_SIZE;
const LV_SIZE: u32 = LV_REC + ER_SIZE;

/// Levels a thread may have in progress at once. NT has no limit but the
/// stack. Unreal Engine 2 titles (XIII) catch and rethrow in each guarded
/// function an error passes through: a failed package lookup at boot went 9
/// deep.
const MAX_SEH: usize = 64;

/// An exception dispatch or unwind in progress.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SehLevel {
    unwind: bool,
    /// Its guest frame, below the raising code's stack.
    base: u32,
    /// EXCEPTION_RECORD, CONTEXT.
    rec: u32,
    ctx: u32,
    /// The registration whose handler runs.
    reg: u32,
    /// Dispatch: the frame a nested raise came from.
    nested: u32,
    /// Unwind: TargetFrame.
    target: u32,
}

impl Kernel {
    fn chain_head(&self) -> u32 {
        self.gm.kr32(self.kpcr_va) // NT_TIB.ExceptionList
    }

    fn set_chain_head(&self, reg: u32) {
        self.gm.kw32(self.kpcr_va, reg);
    }

    /// The CONTEXT a dispatch or unwind starts from: the caller's registers,
    /// returning to `eip` with `esp`.
    fn seh_capture(&self, ctx: u32, eip: u32, esp: u32) -> KResult<()> {
        let mut c = vec![0u8; CX_SIZE as usize];
        let mut put = |off: u32, v: u32| {
            c[off as usize..off as usize + 4].copy_from_slice(&v.to_le_bytes());
        };
        put(CX_FLAGS, CONTEXT_FULL);
        put(CX_FLOAT, 0x027F);
        put(CX_FLOAT + 0x18, 0x1F80);
        put(CX_EDI, self.reg(reg::EDI));
        put(CX_ESI, self.reg(reg::ESI));
        put(CX_EBX, self.reg(reg::EBX));
        put(CX_EDX, self.reg(reg::EDX));
        put(CX_ECX, self.reg(reg::ECX));
        put(CX_EAX, self.reg(reg::EAX));
        put(CX_EBP, self.reg(reg::EBP));
        put(CX_EIP, eip);
        put(CX_CS, 0x08);
        put(CX_EFLAGS, self.frame.regs.eflags);
        put(CX_ESP, esp);
        put(CX_SS, 0x10);
        self.gm.write(ctx, &c)
    }

    /// NtContinue: the guest runs on from the CONTEXT's control and integer
    /// registers.
    fn seh_resume(&mut self, ctx: u32) {
        let rd = |off: u32| self.gm.r32(ctx + off).unwrap_or(0);
        let regs = [
            (reg::EDI, rd(CX_EDI)),
            (reg::ESI, rd(CX_ESI)),
            (reg::EBX, rd(CX_EBX)),
            (reg::EDX, rd(CX_EDX)),
            (reg::ECX, rd(CX_ECX)),
            (reg::EAX, rd(CX_EAX)),
            (reg::EBP, rd(CX_EBP)),
            (reg::ESP, rd(CX_ESP)),
        ];
        let (fl, eip) = (rd(CX_EFLAGS), rd(CX_EIP));
        for (r, v) in regs {
            self.set_reg(r, v);
        }
        self.frame.regs.eflags = fl;
        self.set_eip(eip);
    }

    /// What a C++ exception throws: the first catchable type's decorated name.
    fn cxx_type(&self, rec: u32) -> String {
        let rd = |va: u32| self.gm.r32(va).unwrap_or(0);
        if rd(rec + ER_CODE) != CXX_EXCEPTION || rd(rec + ER_NPARAMS) < 3 {
            return "?".into();
        }
        let info = rd(rec + ER_INFO + 8);
        let types = if info != 0 { rd(info + 12) } else { 0 };
        let first = if types != 0 && rd(types) != 0 { rd(types + 4) } else { 0 };
        let td = if first != 0 { rd(first + 4) } else { 0 };
        if td == 0 {
            return "?".into();
        }
        let name = self.gm.cstr(td + 8, 95).unwrap_or_default();
        name.iter().take_while(|c| (0x20..0x7F).contains(*c)).map(|&c| c as char).collect()
    }

    /// A new level below `esp`; levels a jump out of a handler left behind go.
    fn seh_push_level(&mut self, ti: usize, esp: u32) -> Option<usize> {
        let t = &mut self.threads[ti];
        while t.seh.last().is_some_and(|l| l.base < esp) {
            t.seh.pop();
        }
        if t.seh.len() == MAX_SEH {
            self.request_stop("seh: exceptions nested too deep".into());
            return None;
        }
        let base = (esp - LV_SIZE) & !15;
        t.seh.push(SehLevel { base, ctx: base + LV_CTX, ..SehLevel::default() });
        Some(t.seh.len() - 1)
    }

    fn seh_unhandled(&mut self, l: SehLevel, why: &str) {
        let rd = |va: u32| self.gm.r32(va).unwrap_or(0);
        let code = rd(l.rec + ER_CODE);
        let mut info = String::new();
        if code == STATUS_ACCESS_VIOLATION && rd(l.rec + ER_NPARAMS) >= 2 {
            info = format!(
                ", {} of {:08x}",
                if rd(l.rec + ER_INFO) != 0 { "write" } else { "read" },
                rd(l.rec + ER_INFO + 4)
            );
        }
        let cxx = if code == CXX_EXCEPTION {
            format!(", C++ {}", self.cxx_type(l.rec))
        } else {
            String::new()
        };
        self.request_stop(format!(
            "seh: unhandled exception {code:08x} at {:08x}{info} ({why}){cxx}",
            rd(l.rec + ER_ADDRESS)
        ));
    }

    /// Calls the handler of the level's registration, or ends the level.
    fn seh_step(&mut self, ti: usize) {
        let Some(&l) = self.threads[ti].seh.last() else { return };
        if l.unwind && (l.reg == l.target || (l.reg == CHAIN_END && l.target == 0)) {
            self.threads[ti].seh.pop();
            self.seh_resume(l.ctx); // back from RtlUnwind
            return;
        }
        if l.reg == CHAIN_END {
            if l.unwind {
                self.request_stop(format!("seh: unwind to {:08x} ran off the chain", l.target));
            } else {
                self.seh_unhandled(l, "no handler took it");
            }
            return;
        }
        if l.unwind && l.target != 0 && l.target < l.reg {
            self.request_stop(format!(
                "seh: unwind target {:08x} is below the chain ({:08x})",
                l.target, l.reg
            ));
            return;
        }
        let (base, limit) = (self.gm.kr32(self.kpcr_va + 4), self.gm.kr32(self.kpcr_va + 8));
        let handler = self.gm.r32(l.reg.wrapping_add(4));
        let bad = l.reg & 3 != 0 || l.reg < limit || l.reg.wrapping_add(8) > base;
        let Some(handler) = handler.ok().filter(|_| !bad) else {
            let fl = self.gm.r32(l.rec + ER_FLAGS).unwrap_or(0);
            let _ = self.gm.w32(l.rec + ER_FLAGS, fl | EXCEPTION_STACK_INVALID);
            if l.unwind {
                self.request_stop(format!(
                    "seh: registration {:08x} outside the stack {limit:08x}-{base:08x}",
                    l.reg
                ));
            } else {
                self.seh_unhandled(l, "registration outside the stack");
            }
            return;
        };
        let b = l.base;
        let own = if l.unwind { ORD_SEH_COLLIDED } else { ORD_SEH_NESTED };
        let frame = [
            Self::trap(ORD_SEH_RETURN),
            l.rec,
            l.reg,
            l.ctx,
            b + LV_DISP,
            0, // LV_DISP
            self.chain_head(),
            Self::trap(own),
            l.reg,
        ];
        let bytes: Vec<u8> = frame.iter().flat_map(|v| v.to_le_bytes()).collect();
        if self.gm.write(b, &bytes).is_err() {
            self.request_stop(format!("seh: no stack for the dispatch at {b:08x}"));
            return;
        }
        self.set_chain_head(b + LV_REG);
        self.set_esp(b);
        self.set_eip(handler);
    }

    /// An exception the processor raises in the title's code (an access
    /// violation, a divide error, an invalid opcode, a breakpoint, BOUND's
    /// and INTO's), from the frame at that instruction: dispatched to the
    /// thread's handlers as the Xbox kernel's trap handlers do, with the
    /// record a title's __except filter reads (code, address, and for an
    /// access violation whether it wrote and where). `next_eip`: where the
    /// context goes on if a handler continues it. False, and nothing
    /// changed, when no handler is registered or the stack below has no room
    /// for the dispatch: the caller reports it as the fault no title code
    /// took.
    pub(crate) fn seh_fault(&mut self, code: u32, next_eip: u32, params: &[u32]) -> bool {
        let Some(ti) = self.current else { return false };
        let (esp, eip) = (self.esp(), self.eip());
        let low = esp.wrapping_sub(LV_SIZE) & !15;
        if low >= esp || self.chain_head() == CHAIN_END {
            return false;
        }
        if !self.gm.mapped_range(low & !(PAGE_SIZE - 1), esp - (low & !(PAGE_SIZE - 1))) {
            return false;
        }
        let Some(li) = self.seh_push_level(ti, esp) else {
            return true; // the stop is asked for
        };
        let base = self.threads[ti].seh[li].base;
        let rec = base + LV_REC;
        let _ = self.gm.fill(rec, 0, ER_SIZE);
        let _ = self.gm.w32(rec + ER_CODE, code);
        let _ = self.gm.w32(rec + ER_ADDRESS, eip);
        let _ = self.gm.w32(rec + ER_NPARAMS, params.len() as u32);
        for (i, p) in params.iter().enumerate() {
            let _ = self.gm.w32(rec + ER_INFO + 4 * i as u32, *p);
        }
        let ctx = self.threads[ti].seh[li].ctx;
        let _ = self.seh_capture(ctx, next_eip, esp);
        let head = self.chain_head();
        let l = &mut self.threads[ti].seh[li];
        l.rec = rec;
        l.reg = head;
        if self.seh_logged < 32 {
            self.seh_logged += 1;
            let r = &self.frame.regs.gpr;
            log::info!(target: "kernel", "seh: t{} takes exception {code:08x} at {eip:08x} ({:08x} {:08x}): to its handlers | frames{} | eax {:08x} ebx {:08x} ecx {:08x} edx {:08x} esi {:08x} edi {:08x} ebp {:08x} esp {esp:08x}",
                self.threads[ti].id, params.first().copied().unwrap_or(0), params.get(1).copied().unwrap_or(0),
                self.frame_chain(), r[reg::EAX], r[reg::EBX], r[reg::ECX], r[reg::EDX], r[reg::ESI], r[reg::EDI], r[reg::EBP]);
        }
        self.seh_step(ti);
        true
    }

    /// A handler returned to the SEH-return sentinel with its disposition in
    /// EAX.
    fn seh_handler_returned(&mut self) {
        let esp = self.esp();
        let Some(ti) = self.current else { return };
        {
            let t = &mut self.threads[ti];
            while t.seh.last().is_some_and(|l| l.base + 4 < esp) {
                t.seh.pop();
            }
        }
        let Some(l) = self.threads[ti].seh.last().copied().filter(|l| l.base + 4 == esp) else {
            self.request_stop(format!(
                "seh: a handler returned with no dispatch of its own (esp {esp:08x})"
            ));
            return;
        };
        let li = self.threads[ti].seh.len() - 1;
        let rd = |va: u32| self.gm.r32(va).unwrap_or(0);
        self.set_chain_head(rd(l.base + LV_REG)); // the kernel's registration goes
        let disp = self.reg(reg::EAX);
        let reported = rd(l.base + LV_DISP);
        let mut flags = rd(l.rec + ER_FLAGS);
        let handler = rd(l.reg.wrapping_add(4));
        let next_reg = rd(l.reg);
        if !l.unwind {
            let mut nested = l.nested;
            if nested == l.reg {
                flags &= !EXCEPTION_NESTED_CALL;
                nested = 0;
            }
            match disp {
                CONTINUE_EXECUTION => {
                    let _ = self.gm.w32(l.rec + ER_FLAGS, flags);
                    if flags & EXCEPTION_NONCONTINUABLE != 0 {
                        self.seh_unhandled(l, "a handler continued it, but it is noncontinuable");
                        return;
                    }
                    self.threads[ti].seh.pop();
                    self.seh_resume(l.ctx);
                    return;
                }
                CONTINUE_SEARCH => {}
                NESTED_EXCEPTION => {
                    flags |= EXCEPTION_NESTED_CALL;
                    nested = nested.max(reported);
                }
                _ => {
                    self.request_stop(format!(
                        "seh: handler at {handler:08x} returned disposition {disp}"
                    ));
                    return;
                }
            }
            let _ = self.gm.w32(l.rec + ER_FLAGS, flags);
            let lv = &mut self.threads[ti].seh[li];
            lv.nested = nested;
            lv.reg = next_reg;
        } else {
            let mut reg = l.reg;
            if disp == COLLIDED_UNWIND {
                reg = reported;
            } else if disp != CONTINUE_SEARCH {
                self.request_stop(format!(
                    "seh: unwind handler at {handler:08x} returned disposition {disp}"
                ));
                return;
            }
            let reg = self.gm.r32(reg).unwrap_or(CHAIN_END);
            self.threads[ti].seh[li].reg = reg;
            self.set_chain_head(reg); // the frame unwound is unlinked
        }
        self.seh_step(ti);
    }

    /// The kernel's own registrations' handlers (cdecl: ExceptionRecord,
    /// EstablisherFrame, ContextRecord, DispatcherContext): during a
    /// dispatch, a raise from inside a handler is nested; during an unwind,
    /// another unwind reaching it has collided. Either reports the frame
    /// being handled.
    fn seh_own_handler(&mut self, unwind: bool) {
        let esp = self.esp();
        let rd = |va: u32| self.gm.r32(va).unwrap_or(0);
        let (ret, rec, frame, dctx) = (rd(esp), rd(esp + 4), rd(esp + 8), rd(esp + 16));
        let unwinding = rd(rec + ER_FLAGS) & (EXCEPTION_UNWINDING | EXCEPTION_EXIT_UNWIND) != 0;
        let mut r = CONTINUE_SEARCH;
        if unwinding == unwind {
            let _ = self.gm.w32(dctx, rd(frame + 8));
            r = if unwind { COLLIDED_UNWIND } else { NESTED_EXCEPTION };
        }
        self.set_reg(reg::EAX, r);
        self.set_esp(esp + 4);
        self.set_eip(ret);
    }

    /// A SEH sentinel was reached.
    pub(crate) fn seh_stub(&mut self, ordinal: u32) {
        if ordinal == ORD_SEH_RETURN {
            self.seh_handler_returned();
        } else {
            self.seh_own_handler(ordinal == ORD_SEH_COLLIDED);
        }
    }
}

/// RtlRaiseException(ExceptionRecord): first-chance dispatch from the caller.
fn rtl_raise_exception(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let Some(ti) = k.current else { return Ok(0) };
    let esp = k.esp();
    let ret = k.gm.r32(esp)?;
    // touch the record before any state changes: a fault reruns the call
    let code = k.gm.r32(a[0] + ER_CODE)?;
    let Some(li) = k.seh_push_level(ti, esp) else { return Ok(0) };
    let ctx = k.threads[ti].seh[li].ctx;
    if let Err(f) = k.seh_capture(ctx, ret, esp + 8).and_then(|()| k.gm.w32(a[0] + ER_ADDRESS, ret))
    {
        k.threads[ti].seh.pop();
        return Err(f);
    }
    let head = k.chain_head();
    let l = &mut k.threads[ti].seh[li];
    l.rec = a[0];
    l.reg = head;
    k.redirected = true;
    if k.seh_logged < 32 {
        k.seh_logged += 1;
        let cxx = if code == CXX_EXCEPTION {
            format!(", C++ {}", k.cxx_type(a[0]))
        } else {
            String::new()
        };
        log::info!(target: "kernel", "seh: t{} raises {code:08x} at {ret:08x}{cxx} | frames{}",
            k.threads[ti].id, k.frame_chain());
    }
    k.seh_step(ti);
    Ok(0)
}

/// RtlUnwind(TargetFrame, TargetIp, ExceptionRecord, ReturnValue): calls the
/// handlers above TargetFrame to unwind and unlinks them, then returns.
fn rtl_unwind(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let Some(ti) = k.current else { return Ok(0) };
    let esp = k.esp();
    let ret = k.gm.r32(esp)?;
    if a[2] != 0 {
        k.gm.r32(a[2] + ER_FLAGS)?;
    }
    let Some(li) = k.seh_push_level(ti, esp) else { return Ok(0) };
    let (base, ctx) = (k.threads[ti].seh[li].base, k.threads[ti].seh[li].ctx);
    let mut rec = a[2];
    let r = (|| {
        k.seh_capture(ctx, ret, esp + 4 + 16)?;
        k.gm.w32(ctx + CX_EAX, a[3])?;
        if rec == 0 {
            rec = base + LV_REC;
            k.gm.fill(rec, 0, ER_SIZE)?;
            k.gm.w32(rec + ER_CODE, STATUS_UNWIND)?;
            k.gm.w32(rec + ER_ADDRESS, ret)?;
        }
        let fl = k.gm.r32(rec + ER_FLAGS)?;
        let exit = if a[0] != 0 { 0 } else { EXCEPTION_EXIT_UNWIND };
        k.gm.w32(rec + ER_FLAGS, fl | EXCEPTION_UNWINDING | exit)
    })();
    if let Err(f) = r {
        k.threads[ti].seh.pop();
        return Err(f);
    }
    let head = k.chain_head();
    let l = &mut k.threads[ti].seh[li];
    l.unwind = true;
    l.target = a[0];
    l.rec = rec;
    l.reg = head;
    k.redirected = true;
    k.seh_step(ti);
    Ok(0)
}

pub(crate) fn register(k: &mut Kernel) {
    k.imp("RtlRaiseException", rtl_raise_exception);
    k.imp("RtlUnwind", rtl_unwind);
}

xb_types::state_fields!(SehLevel { unwind, base, rec, ctx, reg, nested, target });
