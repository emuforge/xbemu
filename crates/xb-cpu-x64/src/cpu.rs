//! [`X64Cpu`]: the `xb_cpu::Cpu` implementation, its run loop and the
//! resolution of faults and system instructions.
//!
//! # How a fault in translated code is handled
//!
//! `run` receives the machine as `&mut dyn Bus`, but a guest access that the
//! host cannot complete (a device page, a write-tracked page, a missing
//! page, a divide error) arrives as a signal. The signal handler does not
//! touch the bus. It records the fault in the frame and redirects the
//! interrupted thread to the fault stub, which stores the guest's registers
//! exactly as the faulting instruction found them and returns from the enter
//! stub. `run` then decides, in ordinary context with its `&mut dyn Bus`:
//!
//! 1. a device page: the instruction is translated again with its memory
//!    operand pointing at a buffer and run once (`Bus::mmio_read` before,
//!    `Bus::mmio_write` after);
//! 2. a page the mapping or write tracking forbids: `Bus::page_fault`, and
//!    on `Retry` the instruction runs again;
//! 3. a page this backend write protected because it holds translated code:
//!    the translations go, the page opens, the instruction runs once on its
//!    own (so a block that modifies itself continues in fresh code);
//! 4. anything else: `Exit::Exception` with the guest state at the faulting
//!    instruction.
//!
//! System instructions take the same road without the signal: translated
//! code leaves with a reason and `run` executes the instruction on the saved
//! state.

use std::collections::HashSet;
use std::os::fd::BorrowedFd;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use iced_x86::{
    Code, Formatter, Instruction, InstructionInfoFactory, IntelFormatter, Mnemonic, OpAccess,
    OpKind, Register,
};
use xb_cpu::{
    Access, Bus, Context, Cpu, Exit, FaultAction, Kicker, Mapper, PageProtector, RunFlags,
};

use crate::cache::Cache;
use crate::emulate::{Adjust, adjust};
use crate::frame::{Frame, FrameCell, reason};
use crate::host::{self, CpuRegistration, ShadowStack};
use crate::mem::Memory;
use crate::translate::{
    DescriptorTables, Env, FLAG_AC, FLAG_IF, GUEST_FLAGS, Mode, decode, effective_address,
    translate,
};

/// The same fault at the same place this many times in a row without
/// anything explaining it ends the run instead of spinning.
const SPURIOUS_LIMIT: u32 = 64;

const ESP: usize = xb_cpu::reg::ESP;
const EBP: usize = xb_cpu::reg::EBP;
const EAX: usize = xb_cpu::reg::EAX;
const ECX: usize = xb_cpu::reg::ECX;
const EDX: usize = xb_cpu::reg::EDX;
const EBX: usize = xb_cpu::reg::EBX;
const ESI: usize = xb_cpu::reg::ESI;
const EDI: usize = xb_cpu::reg::EDI;

/// What `run` does after handling an exit from translated code.
enum Next {
    /// Go on at `frame.eip` through the block lookup.
    Lookup,
    /// Go on at this host address (an instruction to run again in place, or
    /// the single-step thunk).
    At(u64),
    /// Return to the caller.
    Leave(Exit),
}

/// A device write to finish once the single-step thunk has run.
#[derive(Clone, Copy)]
struct PendingWrite {
    pa: u32,
    size: u32,
}

/// Counters for the log and the tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Blocks translated.
    pub blocks: u64,
    /// Times the whole translation cache was thrown away.
    pub flushes: u64,
    /// Entries into translated code.
    pub enters: u64,
    /// Host faults taken in translated code.
    pub faults: u64,
    /// Device register accesses emulated.
    pub mmio: u64,
    /// Writes to pages holding translated code.
    pub code_writes: u64,
}

/// The x86-64 translator backend. See the crate's documentation.
pub struct X64Cpu {
    pub(crate) mem: Arc<Memory>,
    frame: Arc<FrameCell>,
    pub(crate) cache: Cache,
    _stack: ShadowStack,
    slot: usize,
    trap: (u32, u32),
    tables: DescriptorTables,
    wrgsbase: bool,
    mxcsr_mask: u32,
    /// The last page fault's address, what `MOV r32, CR2` reads.
    cr2: u32,
    pending_write: Option<PendingWrite>,
    spurious: (u32, u32, u32),
    untranslated_logged: HashSet<u32>,
    info: InstructionInfoFactory,
    enters: u64,
    faults: u64,
    mmio: u64,
    code_writes: u64,
}

/// The host's MXCSR mask (which bits FXRSTOR accepts).
fn host_mxcsr_mask() -> u32 {
    #[repr(C, align(16))]
    struct Fx([u8; 512]);
    let mut fx = Fx([0; 512]);
    // SAFETY: every x86-64 processor has FXSAVE, and the buffer is 512
    // bytes, 16-byte aligned.
    unsafe { core::arch::x86_64::_fxsave(fx.0.as_mut_ptr()) };
    let m = u32::from_le_bytes([fx.0[28], fx.0[29], fx.0[30], fx.0[31]]);
    if m == 0 { 0xFFBF } else { m }
}

impl X64Cpu {
    /// Creates the backend for a guest whose physical RAM is the memory
    /// object `ram_fd` of `ram_size` bytes (the fd is duplicated; the caller
    /// keeps its own).
    ///
    /// Reserves the guest's 4 GiB of address space, allocates the
    /// translation cache and installs the process-wide fault handler.
    pub fn new(ram_fd: BorrowedFd<'_>, ram_size: u32) -> Result<Self, String> {
        Self::with_segment_base(ram_fd, ram_size, host::has_fast_segment_base())
    }

    /// As [`X64Cpu::new`], choosing whether translated code sets the guest
    /// segment's base itself (`WRGSBASE`) or `run` asks the kernel to.
    pub(crate) fn with_segment_base(
        ram_fd: BorrowedFd<'_>,
        ram_size: u32,
        wrgsbase: bool,
    ) -> Result<Self, String> {
        let mem = Arc::new(Memory::new(ram_fd, ram_size)?);
        let cache = Cache::new(wrgsbase)?;
        let stack = ShadowStack::new()?;
        let frame = Arc::new(FrameCell::new());
        // The bottom of the shadow stack: a pair no guest return matches but
        // one to address 0, and that one goes to `ret_reset`.
        let top = stack.top() - 16;
        // SAFETY: the two words are the stack's highest, inside its mapping;
        // the frame is not shared with anything yet.
        unsafe {
            (top as *mut u64).write(cache.stubs().ret_reset);
            (top as *mut u64).add(1).write(0);
            let f = &mut *frame.get();
            f.shadow_top = top as u64;
            f.mem_base = mem.base() as u64;
        }
        let slot = host::register_cpu(&CpuRegistration {
            code: cache.code_range(),
            stubs: cache.stubs_range(),
            guard: stack.guard(),
            stack_top: top,
            fault_stub: cache.stubs().fault as usize,
            frame: frame.get(),
        })?;
        log::info!(target: "cpu",
            "x64 translator: guest memory at host {:#x}, {} MiB of RAM, segment base by {}",
            mem.base(), ram_size >> 20, if wrgsbase { "WRGSBASE" } else { "arch_prctl" });
        Ok(Self {
            mem,
            frame,
            cache,
            _stack: stack,
            slot,
            trap: (0, 0),
            tables: DescriptorTables::default(),
            wrgsbase,
            mxcsr_mask: host_mxcsr_mask(),
            cr2: 0,
            pending_write: None,
            spurious: (0, 0, 0),
            untranslated_logged: HashSet::new(),
            info: InstructionInfoFactory::new(),
            enters: 0,
            faults: 0,
            mmio: 0,
            code_writes: 0,
        })
    }

    /// Sets what `SIDT` and `SGDT` store (the kernel's tables). Translations
    /// made before keep the old values.
    pub fn set_descriptor_tables(&mut self, tables: DescriptorTables) {
        self.tables = tables;
    }

    /// Counters since creation.
    #[must_use]
    pub fn stats(&self) -> Stats {
        let (blocks, flushes) = self.cache.counters();
        Stats {
            blocks,
            flushes,
            enters: self.enters,
            faults: self.faults,
            mmio: self.mmio,
            code_writes: self.code_writes,
        }
    }

    /// Makes the translation cache flush after `bytes` of code (tests).
    #[cfg(test)]
    pub(crate) fn set_code_limit(&mut self, bytes: usize) {
        self.cache.set_code_limit(bytes, &self.mem);
    }

    fn f(&mut self) -> &mut Frame {
        // SAFETY: the frame's non-atomic fields belong to the guest-CPU
        // thread, which is the one holding `&mut self`; translated code and
        // the fault handler use them only inside `enter`, while no reference
        // made here is alive.
        unsafe { &mut *self.frame.get() }
    }

    fn fatal(&mut self, what: &str) -> Next {
        let eip = self.f().eip;
        let msg = format!("x64: {what} at {eip:#010x}");
        log::error!(target: "cpu", "{msg}");
        Next::Leave(Exit::Fatal(msg))
    }

    fn describe(&self, eip: u32) -> String {
        let mut bytes = [0u8; 15];
        let n = self.mem.read(eip, &mut bytes);
        let (i, _) = decode(&self.mem, eip);
        let mut text = String::new();
        if !i.is_invalid() {
            IntelFormatter::new().format(&i, &mut text);
        }
        let len = if i.is_invalid() { n } else { i.len().min(n) };
        let hex: Vec<String> = bytes[..len].iter().map(|b| format!("{b:02x}")).collect();
        format!("{} ({text})", hex.join(" "))
    }

    /// The host code for the block at `eip`, translating it if needed.
    fn code_for(&mut self, eip: u32) -> Result<u64, Exit> {
        if eip >= self.trap.0 && eip < self.trap.1 {
            return Err(Exit::Trap);
        }
        if let Some(code) = self.cache.lookup(eip) {
            return Ok(code);
        }
        let host = self.cache.next_block_addr(&self.mem);
        let stubs = *self.cache.stubs();
        let env = Env { stubs: &stubs, mem: &self.mem, trap: self.trap, tables: self.tables };
        let t = translate(&env, eip, host, Mode::Block).map_err(Exit::Fatal)?;
        if !t.probe {
            return Ok(self.cache.insert(eip, t, &self.mem));
        }
        // Not kept: made again once the missing page is there.
        let t =
            translate(&env, eip, self.cache.scratch_addr(), Mode::Block).map_err(Exit::Fatal)?;
        self.cache
            .set_scratch(&t)
            .ok_or_else(|| Exit::Fatal("x64: the fetch probe does not fit".into()))
    }

    /// Translates the instruction at `eip` alone into the scratch area.
    fn step(&mut self, eip: u32, device: Option<u64>) -> Next {
        let stubs = *self.cache.stubs();
        let env = Env { stubs: &stubs, mem: &self.mem, trap: self.trap, tables: self.tables };
        match translate(&env, eip, self.cache.scratch_addr(), Mode::Step(device)) {
            Ok(t) => match self.cache.set_scratch(&t) {
                Some(entry) => Next::At(entry),
                None => {
                    self.fatal("a single instruction's translation does not fit the scratch area")
                }
            },
            Err(e) => Next::Leave(Exit::Fatal(e)),
        }
    }

    // ---- guest memory from Rust ----

    fn raise_pf(&mut self, va: u32, present: bool, write: bool) -> Exit {
        self.cr2 = va;
        Exit::Exception {
            vector: 14,
            error_code: u32::from(present) | u32::from(write) << 1,
            address: va,
        }
    }

    /// Reads guest memory for an instruction run in Rust, asking the bus
    /// about pages that are missing.
    fn read_guest(&mut self, va: u32, buf: &mut [u8], bus: &mut dyn Bus) -> Result<(), Exit> {
        loop {
            let n = self.mem.read(va, buf);
            if n == buf.len() {
                return Ok(());
            }
            let at = va.wrapping_add(n as u32);
            if self.mem.pte(at).is_mmio() {
                return Err(Exit::Fatal(format!(
                    "x64: instruction data in device registers at {at:#010x}"
                )));
            }
            if bus.page_fault(at, None, false) == FaultAction::Raise {
                return Err(self.raise_pf(at, false, false));
            }
        }
    }

    /// Writes guest memory for an instruction run in Rust, with what a
    /// guest store means: write tracking is asked, and translations of a
    /// page written to go.
    fn write_guest(&mut self, va: u32, data: &[u8], bus: &mut dyn Bus) -> Result<(), Exit> {
        let mut done = 0;
        while done < data.len() {
            let at = va.wrapping_add(done as u32);
            let info = self.mem.info(at);
            if info.pte.is_mmio() {
                return Err(Exit::Fatal(format!(
                    "x64: instruction data in device registers at {at:#010x}"
                )));
            }
            let pa = info.pte.pa() | (at & 0xFFF);
            if !info.pte.is_ram() || info.allowed != Access::ReadWrite {
                let known = info.pte.is_ram().then_some(pa);
                if bus.page_fault(at, known, true) == FaultAction::Raise {
                    return Err(self.raise_pf(at, known.is_some(), true));
                }
                continue;
            }
            if info.code {
                self.code_writes += 1;
                self.cache.code_written(pa, &self.mem);
            }
            let n = (data.len() - done).min((0x1000 - (at & 0xFFF)) as usize);
            self.mem.write_phys(pa, &data[done..done + n]);
            done += n;
        }
        Ok(())
    }

    fn read_u32(&mut self, va: u32, bus: &mut dyn Bus) -> Result<u32, Exit> {
        let mut b = [0u8; 4];
        self.read_guest(va, &mut b, bus)?;
        Ok(u32::from_le_bytes(b))
    }

    /// One element of a string instruction: a device register where the
    /// address is one, memory elsewhere.
    fn load_any(&mut self, va: u32, size: u32, bus: &mut dyn Bus) -> Result<u32, Exit> {
        let pte = self.mem.pte(va);
        if pte.is_mmio() && (va & 0xFFF) + size <= 0x1000 {
            self.mmio += 1;
            return Ok(bus.mmio_read(pte.pa() | (va & 0xFFF), size as u8));
        }
        let mut b = [0u8; 4];
        self.read_guest(va, &mut b[..size as usize], bus)?;
        Ok(u32::from_le_bytes(b))
    }

    fn store_any(&mut self, va: u32, size: u32, v: u32, bus: &mut dyn Bus) -> Result<(), Exit> {
        let pte = self.mem.pte(va);
        if pte.is_mmio() && (va & 0xFFF) + size <= 0x1000 {
            self.mmio += 1;
            bus.mmio_write(pte.pa() | (va & 0xFFF), size as u8, v);
            return Ok(());
        }
        self.write_guest(va, &v.to_le_bytes()[..size as usize], bus)
    }

    // ---- faults ----

    fn fault(&mut self, bus: &mut dyn Bus) -> Next {
        self.faults += 1;
        self.pending_write = None;
        let (rip, addr, trapno, err, r13) = {
            let f = self.f();
            (f.fault_rip, f.fault_addr, f.fault_trapno, f.fault_err, f.fault_r13)
        };
        let Some(site) = self.cache.site(rip) else {
            return self.fatal(&format!(
                "host fault (vector {trapno}) outside any translation, host rip {rip:#x}"
            ));
        };
        let generation = self.cache.generation();
        {
            let f = self.f();
            f.eip = site.insn.eip;
            if site.insn.borrow != 0 {
                // Faulted while a low register held the operand's address.
                f.gregs[usize::from(site.insn.borrow) - 1] = r13 & 0xFFFF_FFFF;
            }
            for g in &mut f.gregs {
                // String instructions run with the base added to ESI/EDI.
                *g &= 0xFFFF_FFFF;
            }
        }
        let eip = site.insn.eip;
        let exception =
            |vector: u8| Next::Leave(Exit::Exception { vector, error_code: 0, address: 0 });
        match trapno {
            14 => {}
            0 | 6 | 16 | 19 => return exception(trapno as u8),
            // The host's general protection fault for what is one on the
            // console too: an SSE operand that is not aligned.
            13 => return exception(13),
            _ => return self.fatal(&format!("unexpected host fault, vector {trapno}")),
        }
        let Some(va) = self.mem.guest_of(addr as usize) else {
            return self.fatal(&format!("translated code touched host address {addr:#x}"));
        };
        let write = err & 2 != 0;
        let info = self.mem.info(va);
        let again = |cpu: &Self| match site.resume {
            Some(at) if cpu.cache.generation() == generation => Next::At(at),
            _ => Next::Lookup,
        };
        if info.pte.is_mmio() {
            return self.device_access(eip, va, bus);
        }
        if !info.pte.is_ram() {
            return match bus.page_fault(va, None, write) {
                FaultAction::Retry => again(self),
                FaultAction::Raise => Next::Leave(self.raise_pf(va, false, write)),
            };
        }
        let pa = info.pte.pa() | (va & 0xFFF);
        let needed = if write { Access::ReadWrite } else { Access::Read };
        let forbidden =
            matches!((info.allowed, needed), (Access::None, _) | (Access::Read, Access::ReadWrite));
        if forbidden {
            return match bus.page_fault(va, Some(pa), write) {
                FaultAction::Retry => again(self),
                FaultAction::Raise => Next::Leave(self.raise_pf(va, true, write)),
            };
        }
        if write && info.code {
            // Translated code is being written over. The instruction runs
            // once on its own with the page open, so that what follows it,
            // which it may just have changed, is translated afresh.
            self.code_writes += 1;
            self.cache.code_written(pa, &self.mem);
            return self.step(eip, None);
        }
        // Nothing forbids the access any more (another thread opened the
        // page between the fault and now): again.
        if self.spurious.0 == eip && self.spurious.1 == va {
            self.spurious.2 += 1;
            if self.spurious.2 > SPURIOUS_LIMIT {
                return self
                    .fatal(&format!("an access to {va:#010x} keeps faulting for no known reason"));
            }
        } else {
            self.spurious = (eip, va, 1);
        }
        again(self)
    }

    /// A guest instruction touched a device page: run it against a buffer.
    fn device_access(&mut self, eip: u32, va: u32, bus: &mut dyn Bus) -> Next {
        let (i, _) = decode(&self.mem, eip);
        if i.is_invalid() {
            return self.fatal("device access by bytes that do not decode");
        }
        if matches!(
            i.mnemonic(),
            Mnemonic::Movsb
                | Mnemonic::Movsw
                | Mnemonic::Movsd
                | Mnemonic::Stosb
                | Mnemonic::Stosw
                | Mnemonic::Stosd
                | Mnemonic::Lodsb
                | Mnemonic::Lodsw
                | Mnemonic::Lodsd
        ) && i.op_kinds().any(|k| matches!(k, OpKind::MemorySegESI | OpKind::MemoryESEDI))
        {
            return match self.device_string(&i, bus) {
                Ok(()) => Next::Lookup,
                Err(e) => Next::Leave(e),
            };
        }
        let Some(k) = (0..i.op_count()).find(|&k| i.op_kind(k) == OpKind::Memory) else {
            let d = self.describe(eip);
            return self
                .fatal(&format!("device access by an instruction without a memory operand: {d}"));
        };
        let (gregs, fs_base) = {
            let f = self.f();
            (f.gregs, f.fs_base as u32)
        };
        let ea = effective_address(&i, &gregs, fs_base);
        let size = i.memory_size().size() as u32;
        let access = self.info.info(&i).op_access(k);
        let (reads, writes) = match access {
            OpAccess::Read | OpAccess::CondRead => (true, false),
            OpAccess::Write | OpAccess::CondWrite => (false, true),
            OpAccess::ReadWrite | OpAccess::ReadCondWrite => (true, true),
            _ => (false, false),
        };
        let pte = self.mem.pte(ea);
        let whole = matches!(size, 1 | 2 | 4 | 8 | 16)
            && pte.is_mmio()
            && (ea & 0xFFF) + size <= 0x1000
            && va.wrapping_sub(ea) < size
            && (reads || writes);
        if !whole {
            let d = self.describe(eip);
            return self
                .fatal(&format!("device access at {va:#010x} that cannot be replayed: {d}"));
        }
        let pa = pte.pa() | (ea & 0xFFF);
        let mut buf = [0u8; 16];
        if reads {
            // 8 and 16 bytes (MMX, SSE moves) as doublewords.
            let unit = size.min(4);
            for o in (0..size).step_by(unit as usize) {
                let v = bus.mmio_read(pa + o, unit as u8);
                buf[o as usize..(o + unit) as usize]
                    .copy_from_slice(&v.to_le_bytes()[..unit as usize]);
            }
        }
        self.mmio += 1;
        // SAFETY: the scratch data buffer is 16 bytes inside the code
        // region, used only by the single-step thunk, which is not running.
        unsafe { std::ptr::copy_nonoverlapping(buf.as_ptr(), self.cache.scratch_data(), 16) };
        let next = self.step(eip, Some(self.cache.scratch_data() as u64));
        if writes && matches!(next, Next::At(_)) {
            self.pending_write = Some(PendingWrite { pa, size });
        }
        next
    }

    /// The single-step thunk ran a device access to its end: the store.
    fn finish_device_write(&mut self, w: PendingWrite, bus: &mut dyn Bus) {
        let mut buf = [0u8; 16];
        // SAFETY: as in `device_access`.
        unsafe { std::ptr::copy_nonoverlapping(self.cache.scratch_data(), buf.as_mut_ptr(), 16) };
        let unit = w.size.min(4);
        for o in (0..w.size).step_by(unit as usize) {
            let mut b = [0u8; 4];
            b[..unit as usize].copy_from_slice(&buf[o as usize..(o + unit) as usize]);
            bus.mmio_write(w.pa + o, unit as u8, u32::from_le_bytes(b));
        }
    }

    /// MOVS, STOS or LODS (REP or not) with a device on one side, element
    /// by element. The registers are updated after each element, so a fault
    /// part way leaves the instruction restartable, as on the processor.
    fn device_string(&mut self, i: &Instruction, bus: &mut dyn Bus) -> Result<(), Exit> {
        if i.segment_prefix() == Register::FS {
            return Err(Exit::Fatal(
                "x64: a string instruction from FS on device registers".into(),
            ));
        }
        let size = i.memory_size().size() as u32;
        let rep = i.has_rep_prefix() || i.has_repne_prefix();
        let down = self.f().eflags & 0x400 != 0;
        let step = if down { 0u32.wrapping_sub(size) } else { size };
        let mask = if size == 4 { u32::MAX } else { (1u32 << (8 * size)) - 1 };
        loop {
            let g = self.f().gregs;
            if rep && g[ECX] as u32 == 0 {
                break;
            }
            let (si, di) = (g[ESI] as u32, g[EDI] as u32);
            match i.mnemonic() {
                Mnemonic::Movsb | Mnemonic::Movsw | Mnemonic::Movsd => {
                    let v = self.load_any(si, size, bus)?;
                    self.store_any(di, size, v, bus)?;
                    let f = self.f();
                    f.gregs[ESI] = u64::from(si.wrapping_add(step));
                    f.gregs[EDI] = u64::from(di.wrapping_add(step));
                }
                Mnemonic::Stosb | Mnemonic::Stosw | Mnemonic::Stosd => {
                    self.store_any(di, size, g[EAX] as u32 & mask, bus)?;
                    self.f().gregs[EDI] = u64::from(di.wrapping_add(step));
                }
                _ => {
                    let v = self.load_any(si, size, bus)?;
                    let f = self.f();
                    f.gregs[EAX] = u64::from((g[EAX] as u32 & !mask) | (v & mask));
                    f.gregs[ESI] = u64::from(si.wrapping_add(step));
                }
            }
            if !rep {
                break;
            }
            let f = self.f();
            f.gregs[ECX] = u64::from((g[ECX] as u32).wrapping_sub(1));
        }
        let f = self.f();
        f.eip = f.eip.wrapping_add(i.len() as u32);
        Ok(())
    }

    // ---- instructions run in Rust ----

    fn sysop(&mut self, bus: &mut dyn Bus) -> Next {
        let eip = self.f().eip;
        let (i, _) = decode(&self.mem, eip);
        if i.is_invalid() {
            return Next::Leave(Exit::Exception { vector: 6, error_code: 0, address: 0 });
        }
        let next = eip.wrapping_add(i.len() as u32);
        match self.sysop_run(&i, eip, next, bus) {
            Ok(Some(exit)) => Next::Leave(exit),
            Ok(None) => Next::Lookup,
            Err(exit) => {
                // The instruction did not complete: it is the one to resume.
                self.f().eip = eip;
                Next::Leave(exit)
            }
        }
    }

    /// Runs one system instruction. `Ok(None)`: done, `frame.eip` is where
    /// the guest continues.
    fn sysop_run(
        &mut self,
        i: &Instruction,
        eip: u32,
        next: u32,
        bus: &mut dyn Bus,
    ) -> Result<Option<Exit>, Exit> {
        let g = self.f().gregs;
        let eax = g[EAX] as u32;
        let exception = |vector: u8| Exit::Exception { vector, error_code: 0, address: 0 };
        self.f().eip = next;
        match i.mnemonic() {
            Mnemonic::In | Mnemonic::Out => {
                let out = i.mnemonic() == Mnemonic::Out;
                let (data, port_op) = if out { (1, 0) } else { (0, 1) };
                let size = i.op_register(data).size() as u32;
                let port = match i.op_kind(port_op) {
                    OpKind::Immediate8 => u16::from(i.immediate8()),
                    _ => g[EDX] as u16,
                };
                let mask = if size == 4 { u32::MAX } else { (1u32 << (8 * size)) - 1 };
                if out {
                    bus.io_write(port, size as u8, eax & mask);
                } else {
                    let v = bus.io_read(port, size as u8);
                    self.f().gregs[EAX] = u64::from((eax & !mask) | (v & mask));
                }
            }
            Mnemonic::Insb
            | Mnemonic::Insw
            | Mnemonic::Insd
            | Mnemonic::Outsb
            | Mnemonic::Outsw
            | Mnemonic::Outsd => {
                // Element by element, the registers updated after each, so
                // a fault part way leaves the instruction restartable.
                let out =
                    matches!(i.mnemonic(), Mnemonic::Outsb | Mnemonic::Outsw | Mnemonic::Outsd);
                if i.segment_prefix() == Register::FS
                    || i.op_kinds().any(|k| matches!(k, OpKind::MemorySegSI | OpKind::MemoryESDI))
                {
                    return Err(Exit::Fatal(format!(
                        "x64: no translation for {}",
                        self.describe(eip)
                    )));
                }
                let size = i.memory_size().size() as u32;
                let rep = i.has_rep_prefix() || i.has_repne_prefix();
                let step =
                    if self.f().eflags & 0x400 != 0 { 0u32.wrapping_sub(size) } else { size };
                let port = g[EDX] as u16;
                self.f().eip = eip;
                loop {
                    let g = self.f().gregs;
                    if rep && g[ECX] as u32 == 0 {
                        break;
                    }
                    if out {
                        let v = self.load_any(g[ESI] as u32, size, bus)?;
                        bus.io_write(port, size as u8, v);
                        self.f().gregs[ESI] = u64::from((g[ESI] as u32).wrapping_add(step));
                    } else {
                        // The store is tried before the port is read: a
                        // faulting store must not consume device data.
                        let old = self.load_any(g[EDI] as u32, size, bus).unwrap_or(0);
                        self.store_any(g[EDI] as u32, size, old, bus)?;
                        let v = bus.io_read(port, size as u8);
                        self.store_any(g[EDI] as u32, size, v, bus)?;
                        self.f().gregs[EDI] = u64::from((g[EDI] as u32).wrapping_add(step));
                    }
                    if !rep {
                        break;
                    }
                    self.f().gregs[ECX] = u64::from((g[ECX] as u32).wrapping_sub(1));
                }
                self.f().eip = next;
            }
            Mnemonic::Cpuid => {
                let r = xb_cpu::cpuid(eax);
                let f = self.f();
                f.gregs[EAX] = u64::from(r[0]);
                f.gregs[EBX] = u64::from(r[1]);
                f.gregs[ECX] = u64::from(r[2]);
                f.gregs[EDX] = u64::from(r[3]);
            }
            Mnemonic::Rdtsc => {
                let t = bus.tsc();
                let f = self.f();
                f.gregs[EAX] = t & 0xFFFF_FFFF;
                f.gregs[EDX] = t >> 32;
            }
            Mnemonic::Rdmsr => {
                let v = bus.rdmsr(g[ECX] as u32);
                let f = self.f();
                f.gregs[EAX] = v & 0xFFFF_FFFF;
                f.gregs[EDX] = v >> 32;
            }
            Mnemonic::Wrmsr => bus.wrmsr(g[ECX] as u32, g[EDX] << 32 | u64::from(eax)),
            Mnemonic::Int => {
                let n = i.immediate8();
                return Ok(Some(if n == 3 { exception(3) } else { Exit::SoftwareInterrupt(n) }));
            }
            Mnemonic::Int3 => return Ok(Some(exception(3))),
            Mnemonic::Int1 => return Ok(Some(exception(1))),
            Mnemonic::Into => {
                if self.f().eflags & 0x800 != 0 {
                    return Ok(Some(exception(4)));
                }
            }
            Mnemonic::Mov => match i.code() {
                Code::Mov_r32_cr => {
                    // CR0: PG WP NE ET MP PE, as the Xbox kernel leaves it;
                    // CR4: PSE OSFXSR OSXMMEXCPT (as the reference does).
                    let v = match i.op_register(1) {
                        Register::CR0 => 0x8001_003B,
                        Register::CR2 => self.cr2,
                        Register::CR4 => 0x610,
                        _ => 0,
                    };
                    let n = i.op_register(0).number();
                    self.f().gregs[n & 7] = u64::from(v);
                }
                // A write (a CR3 reload to flush the TLB): nothing to do.
                Code::Mov_cr_r32 => {}
                _ => {
                    return Err(Exit::Fatal(format!(
                        "x64: no translation for {}",
                        self.describe(eip)
                    )));
                }
            },
            Mnemonic::Iretd => {
                // IRETD to the same ring: EIP, CS, EFLAGS off the stack.
                // Fable's page fault handler leaves the fault's context this
                // way (as the reference does it).
                let esp = g[ESP] as u32;
                let to = self.read_u32(esp, bus)?;
                let cs = self.read_u32(esp.wrapping_add(4), bus)?;
                let fl = self.read_u32(esp.wrapping_add(8), bus)?;
                if cs & 3 == 3 {
                    return Err(Exit::Fatal(format!("x64: IRETD to ring 3 at {eip:#010x}")));
                }
                let f = self.f();
                f.gregs[ESP] = u64::from(esp.wrapping_add(12));
                f.eflags = u64::from((fl & GUEST_FLAGS) | 0x202);
                f.vif = fl & FLAG_IF;
                f.vac = fl & FLAG_AC;
                f.eip = to;
                if f.exit_if != 0 && f.vif != 0 {
                    return Ok(Some(Exit::Kicked));
                }
            }
            Mnemonic::Bound => {
                // The index register against a pair of signed bounds.
                let fs = self.f().fs_base as u32;
                let ea = effective_address(i, &g, fs);
                let r = i.op_register(0);
                let v = g[r.full_register32().number() & 7] as u32;
                let (idx, lo, hi) = if r.is_gpr16() {
                    let mut b = [0u8; 4];
                    self.read_guest(ea, &mut b, bus)?;
                    (
                        i32::from(v as u16 as i16),
                        i32::from(i16::from_le_bytes([b[0], b[1]])),
                        i32::from(i16::from_le_bytes([b[2], b[3]])),
                    )
                } else {
                    (
                        v as i32,
                        self.read_u32(ea, bus)? as i32,
                        self.read_u32(ea.wrapping_add(4), bus)? as i32,
                    )
                };
                if idx < lo || idx > hi {
                    return Err(exception(5));
                }
            }
            Mnemonic::Enter => {
                // With a nesting level (the plain one is translated): the
                // outer frames' pointers copied down, as the processor does.
                let size = u32::from(i.immediate16());
                let level = u32::from(i.immediate8_2nd()) & 31;
                let mut esp = g[ESP] as u32;
                let mut ebp = g[EBP] as u32;
                let mut words = vec![ebp];
                let frame = esp.wrapping_sub(4);
                for _ in 1..level {
                    ebp = ebp.wrapping_sub(4);
                    words.push(self.read_u32(ebp, bus)?);
                }
                if level > 0 {
                    words.push(frame);
                }
                // All reads are done; the stores, lowest address last so a
                // fault leaves ESP and EBP untouched.
                for w in words {
                    esp = esp.wrapping_sub(4);
                    self.write_guest(esp, &w.to_le_bytes(), bus)?;
                }
                let f = self.f();
                f.gregs[EBP] = u64::from(frame);
                f.gregs[ESP] = u64::from(esp.wrapping_sub(size));
            }
            Mnemonic::Leave => {
                // 66 C9: ESP from EBP whole (the stack's address size is
                // 32), BP popped as a word.
                let ebp = g[EBP] as u32;
                let mut b = [0u8; 2];
                self.read_guest(ebp, &mut b, bus)?;
                let f = self.f();
                f.gregs[EBP] = u64::from((ebp & 0xFFFF_0000) | u32::from(u16::from_le_bytes(b)));
                f.gregs[ESP] = u64::from(ebp.wrapping_add(2));
            }
            _ => {
                let fl = self.f().eflags as u32;
                match adjust(i, eax, fl) {
                    Adjust::Done(a, flags) => {
                        let f = self.f();
                        f.gregs[EAX] = u64::from(a);
                        f.eflags = u64::from(flags);
                    }
                    Adjust::DivideError => return Err(exception(0)),
                    Adjust::NotMine => {
                        return Err(Exit::Fatal(format!(
                            "x64: no translation for {}",
                            self.describe(eip)
                        )));
                    }
                }
            }
        }
        Ok(None)
    }

    fn untranslated(&mut self) -> Next {
        let eip = self.f().eip;
        let (i, _) = decode(&self.mem, eip);
        let text = self.describe(eip);
        if self.untranslated_logged.insert(i.code() as u32) {
            log::error!(target: "cpu", "x64: no translation for {text} at {eip:#010x}");
        }
        Next::Leave(Exit::Fatal(format!("x64: no translation for {text} at {eip:#010x}")))
    }
}

impl Drop for X64Cpu {
    fn drop(&mut self) {
        host::unregister_cpu(self.slot);
    }
}

impl Mapper for X64Cpu {
    fn map(&mut self, va: u32, pa: u32, len: u32, access: Access) {
        self.cache.invalidate_virtual(va, len, &self.mem);
        self.mem.map(va, pa, len, access);
    }

    fn map_mmio(&mut self, va: u32, pa: u32, len: u32) {
        self.cache.invalidate_virtual(va, len, &self.mem);
        self.mem.map_mmio(va, pa, len);
    }

    fn unmap(&mut self, va: u32, len: u32) {
        self.cache.invalidate_virtual(va, len, &self.mem);
        self.mem.unmap(va, len);
    }
}

impl Cpu for X64Cpu {
    fn name(&self) -> &'static str {
        "x64"
    }

    fn run(&mut self, ctx: &mut Context, bus: &mut dyn Bus, flags: RunFlags) -> Exit {
        if self.frame.kick().swap(0, Ordering::SeqCst) != 0 {
            return Exit::Kicked;
        }
        if flags.exit_when_interruptible && ctx.regs.eflags & FLAG_IF != 0 {
            return Exit::Kicked;
        }
        host::ensure_signal_stack();
        if !self.wrgsbase
            && let Err(e) = host::set_guest_segment_base(self.mem.base())
        {
            return Exit::Fatal(format!("x64: {e}"));
        }
        // FXRSTOR faults on MXCSR bits the host does not have.
        let mxcsr = u32::from_le_bytes([ctx.fx.0[24], ctx.fx.0[25], ctx.fx.0[26], ctx.fx.0[27]]);
        if mxcsr & !self.mxcsr_mask != 0 {
            ctx.fx.0[24..28].copy_from_slice(&(mxcsr & self.mxcsr_mask).to_le_bytes());
        }
        {
            let fx = (&raw mut ctx.fx) as u64;
            let f = self.f();
            for (d, s) in f.gregs.iter_mut().zip(ctx.regs.gpr) {
                *d = u64::from(s);
            }
            f.eip = ctx.regs.eip;
            f.eflags = u64::from((ctx.regs.eflags & GUEST_FLAGS) | 0x202);
            f.vif = ctx.regs.eflags & FLAG_IF;
            f.vac = ctx.regs.eflags & FLAG_AC;
            f.exit_if = u32::from(flags.exit_when_interruptible);
            f.fs_base = u64::from(ctx.regs.fs_base);
            f.fx_ptr = fx;
        }
        self.pending_write = None;
        self.spurious = (0, 0, 0);
        let enter = self.cache.stubs().enter;
        // SAFETY: `enter` is the address of the enter stub, which follows
        // the System V calling convention for this signature.
        let enter: extern "sysv64" fn(*mut Frame) = unsafe { std::mem::transmute(enter) };
        let mut next = Next::Lookup;
        let exit = loop {
            let code = match next {
                Next::Leave(exit) => break exit,
                Next::At(code) => code,
                Next::Lookup => {
                    let eip = self.f().eip;
                    match self.code_for(eip) {
                        Ok(code) => code,
                        Err(exit) => break exit,
                    }
                }
            };
            self.f().next_code = code;
            self.enters += 1;
            // Translated code runs with the guest's registers, on its own
            // stack, and comes back through the exit stub with the frame and
            // `ctx.fx` holding the guest's state. No reference into the
            // frame or `ctx` is alive across the call.
            enter(self.frame.get());
            let why = self.f().reason;
            if why != reason::FAULT {
                self.spurious = (0, 0, 0);
            }
            next = match why {
                reason::LOOKUP => Next::Lookup,
                reason::STEP => {
                    if let Some(w) = self.pending_write.take() {
                        self.finish_device_write(w, bus);
                    }
                    Next::Lookup
                }
                reason::KICK => {
                    self.frame.kick().store(0, Ordering::SeqCst);
                    Next::Leave(Exit::Kicked)
                }
                reason::HLT => Next::Leave(Exit::Halt),
                reason::FAULT => self.fault(bus),
                reason::SYSOP => self.sysop(bus),
                reason::INVALID => {
                    Next::Leave(Exit::Exception { vector: 6, error_code: 0, address: 0 })
                }
                reason::UNTRANSLATED => self.untranslated(),
                other => self.fatal(&format!("translated code left with unknown reason {other}")),
            };
        };
        let f = self.f();
        for (d, s) in ctx.regs.gpr.iter_mut().zip(f.gregs) {
            *d = s as u32;
        }
        ctx.regs.eip = f.eip;
        ctx.regs.eflags = (f.eflags as u32 & GUEST_FLAGS) | xb_cpu::eflags::FIXED | f.vif | f.vac;
        exit
    }

    fn kicker(&self) -> Kicker {
        let frame = Arc::clone(&self.frame);
        Kicker::new(move || frame.kick().store(1, Ordering::SeqCst))
    }

    fn protector(&self) -> Arc<dyn PageProtector> {
        Arc::clone(&self.mem) as Arc<dyn PageProtector>
    }

    fn set_trap_range(&mut self, va: u32, len: u32) {
        self.trap = (va, va.saturating_add(len));
        // Code translated before the range existed must not run into it.
        self.cache.invalidate_virtual(va, len, &self.mem);
    }

    fn invalidate_code(&mut self, pa: u32, len: u32) {
        self.cache.invalidate_physical(pa, len, &self.mem);
    }
}
