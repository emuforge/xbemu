//! The interpreter's state, operand access and run loop.

use std::ops::ControlFlow;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use iced_x86::{Instruction, OpKind, Register};
use xb_cpu::{
    Access, Bus, Context, Cpu, Exit, FaultAction, Kicker, Mapper, PageProtector, RunFlags,
};
use xb_mem::Ram;

use crate::cache::{Cache, DecodeError};
use crate::f80::{F80, Fpr};
use crate::fast::Uop;
use crate::flags::{Flags, IF};
use crate::jit;
use crate::mem::{Mem, PageFault};

/// Why an instruction stopped before completing. The run loop puts the
/// registers back as they were before it ([`InterpCpu::snap`]), except
/// what a string instruction has finished, and points `eip` at it.
#[derive(Debug)]
pub(crate) enum Stop {
    /// A guest access the page table or write tracking forbids.
    Page(PageFault),
    /// A processor exception (`#DE`, `#UD`, `#GP`, `#MF`, `#XM`).
    Exc(u8, u32),
    /// Something the interpreter does not implement.
    Fatal(String),
}

impl From<PageFault> for Stop {
    fn from(f: PageFault) -> Self {
        Self::Page(f)
    }
}

/// How an instruction that completed continues.
pub(crate) enum Flow {
    /// At the next instruction.
    Next,
    /// At this address (a taken branch).
    Jump(u32),
    /// At this address, after the run loop looked at the state: IF may
    /// have changed (`IRETD`).
    JumpEnd(u32),
    /// At the next instruction, after the run loop looked at the state: IF
    /// may have changed.
    End,
    /// `run` returns this, with `eip` past the instruction.
    Exit(Exit),
}

/// Result of one instruction.
pub(crate) type Step = Result<Flow, Stop>;

/// The registers an instruction is rolled back to when it stops.
#[derive(Clone, Copy)]
pub(crate) struct Snap {
    pub(crate) gpr: [u32; 8],
    pub(crate) flags: Flags,
}

/// x87 state, with the registers in physical order.
#[derive(Clone, Copy)]
pub(crate) struct Fpu {
    pub(crate) st: [Fpr; 8],
    pub(crate) top: u32,
    /// Bit `i` set: physical register `i` is not empty.
    pub(crate) valid: u8,
    pub(crate) fcw: u16,
    /// The status word without TOP.
    pub(crate) fsw: u16,
}

/// The guest CPU backend that interprets guest code.
pub struct InterpCpu {
    pub(crate) mem: Mem,
    pub(crate) cache: Cache,
    pub(crate) gpr: [u32; 8],
    pub(crate) eip: u32,
    pub(crate) flags: Flags,
    pub(crate) fs_base: u32,
    pub(crate) fpu: Fpu,
    pub(crate) xmm: [[u32; 4]; 8],
    pub(crate) mxcsr: u32,
    /// The last page-fault address (`MOV r32, CR2`).
    pub(crate) cr2: u32,
    pub(crate) snap: Snap,
    /// `RunFlags::exit_when_interruptible` of the current `run`.
    pub(crate) exit_if: bool,
    /// The trap range, `[start, end)`; the end may be 2^32.
    trap: (u32, u64),
    kick: Arc<AtomicBool>,
    /// Mnemonics already reported as unimplemented.
    pub(crate) reported: std::collections::HashSet<iced_x86::Mnemonic>,
    /// The code generator's state.
    pub(crate) jit: jit::State,
}

impl InterpCpu {
    /// Creates the backend over the guest's physical RAM.
    #[must_use]
    pub fn new(ram: Ram) -> Self {
        let flags = Flags::new(0x2);
        let mem = Mem::new(ram);
        let (tables, pages) = mem.tables();
        let cache = Cache::new();
        let kick = Arc::new(AtomicBool::new(false));
        // What generated code reads to go from block to block.
        let chain = [cache.links_ptr(), Arc::as_ptr(&kick) as usize, mem.generation_ptr()];
        Self {
            jit: jit::State::new(tables, pages, chain),
            mem,
            cache,
            gpr: [0; 8],
            eip: 0,
            flags,
            fs_base: 0,
            fpu: Fpu { st: [Fpr::default(); 8], top: 0, valid: 0, fcw: 0x037F, fsw: 0 },
            xmm: [[0; 4]; 8],
            mxcsr: 0x1F80,
            cr2: 0,
            snap: Snap { gpr: [0; 8], flags },
            exit_if: false,
            trap: (0, 0),
            kick,
            reported: std::collections::HashSet::new(),
        }
    }

    fn load_ctx(&mut self, ctx: &Context) {
        let r = &ctx.regs;
        self.gpr = r.gpr;
        self.eip = r.eip;
        self.flags = Flags::new(r.eflags | 2);
        self.fs_base = r.fs_base;
        self.load_fx(&ctx.fx.0);
    }

    fn store_ctx(&self, ctx: &mut Context) {
        let r = &mut ctx.regs;
        r.gpr = self.gpr;
        r.eip = self.eip;
        r.eflags = self.flags.get() | 2;
        r.fs_base = self.fs_base;
        self.store_fx(&mut ctx.fx.0);
    }

    /// Loads x87, MMX and SSE state from an FXSAVE image.
    pub(crate) fn load_fx(&mut self, fx: &[u8; 512]) {
        let w16 = |o: usize| u16::from_le_bytes([fx[o], fx[o + 1]]);
        let w32 = |o: usize| u32::from_le_bytes([fx[o], fx[o + 1], fx[o + 2], fx[o + 3]]);
        self.fpu.fcw = w16(0);
        let fsw = w16(2);
        self.fpu.top = u32::from(fsw >> 11) & 7;
        self.fpu.fsw = fsw & !0x3800;
        for i in 0..8 {
            let phys = (self.fpu.top as usize + i) & 7;
            self.fpu.st[phys] = Fpr::from_f80(F80::from_bytes(&fx[32 + 16 * i..42 + 16 * i]));
        }
        // The abridged tag word is in physical order.
        self.fpu.valid = fx[4];
        self.mxcsr = w32(24);
        for (i, x) in self.xmm.iter_mut().enumerate() {
            for (j, w) in x.iter_mut().enumerate() {
                *w = w32(160 + 16 * i + 4 * j);
            }
        }
    }

    /// Writes x87, MMX and SSE state into an FXSAVE image, keeping the
    /// bytes it does not model (the instruction and data pointers).
    pub(crate) fn store_fx(&self, fx: &mut [u8; 512]) {
        fx[0..2].copy_from_slice(&self.fpu.fcw.to_le_bytes());
        let fsw = self.fpu.fsw & !0x3800 | ((self.fpu.top as u16) << 11);
        fx[2..4].copy_from_slice(&fsw.to_le_bytes());
        fx[4] = self.fpu.valid;
        for i in 0..8 {
            let phys = (self.fpu.top as usize + i) & 7;
            fx[32 + 16 * i..42 + 16 * i].copy_from_slice(&self.fpu.st[phys].f80().to_bytes());
            fx[42 + 16 * i..48 + 16 * i].fill(0);
        }
        fx[24..28].copy_from_slice(&self.mxcsr.to_le_bytes());
        for (i, x) in self.xmm.iter().enumerate() {
            for (j, w) in x.iter().enumerate() {
                let o = 160 + 16 * i + 4 * j;
                fx[o..o + 4].copy_from_slice(&w.to_le_bytes());
            }
        }
    }

    /// Asks the bus about a fault: `None` to run the instruction again.
    fn page_fault(&mut self, f: PageFault, bus: &mut dyn Bus) -> Option<Exit> {
        self.mem.leave();
        let action = bus.page_fault(f.va, f.pa, f.write);
        self.mem.enter();
        match action {
            FaultAction::Retry => None,
            FaultAction::Raise => {
                self.cr2 = f.va;
                Some(Exit::Exception {
                    vector: 14,
                    error_code: u32::from(f.pa.is_some()) | u32::from(f.write) << 1,
                    address: f.va,
                })
            }
        }
    }

    /// Drops the blocks on pages guest stores changed.
    fn flush_smc(&mut self) {
        while let Some(p) = self.mem.smc.pop() {
            self.cache.invalidate_page(p, &mut self.mem);
        }
    }

    fn exec(&mut self, bus: &mut dyn Bus) -> Exit {
        self.mem.enter();
        let exit = loop {
            if self.kick.load(Ordering::Relaxed) && self.kick.swap(false, Ordering::AcqRel) {
                break Exit::Kicked;
            }
            let eip = self.eip;
            if eip >= self.trap.0 && u64::from(eip) < self.trap.1 {
                break Exit::Trap;
            }
            let bi = match self.cache.lookup(eip, &self.mem) {
                Some(b) => b,
                None => match self.cache.decode(eip, &mut self.mem, self.trap) {
                    Ok(b) => b,
                    Err(DecodeError::Fault(f)) => match self.page_fault(f, bus) {
                        Some(e) => break e,
                        None => continue,
                    },
                    Err(DecodeError::Invalid) => {
                        break Exit::Exception { vector: 6, error_code: 0, address: 0 };
                    }
                    Err(DecodeError::Device) => {
                        break Exit::Fatal(format!("interp: code at {eip:#010x} is not in RAM"));
                    }
                },
            };
            // The block's ops are moved out while it runs (and back after,
            // if it is still valid), so running it touches no shared count.
            let ops = self.cache.take_ops(bi);
            // A block boundary: `protect` waits at most for this block
            // (string instructions acknowledge as they go).
            self.mem.poll();
            let mut entry = self.cache.entry(bi);
            if entry.is_none() && self.cache.became_hot(bi) {
                match self.jit.compile(&ops) {
                    ControlFlow::Continue(Some(e)) => {
                        self.cache.set_entry(bi, e);
                        entry = Some(e);
                    }
                    ControlFlow::Continue(None) => {}
                    ControlFlow::Break(()) => {
                        // No room for more host code: every block goes, and
                        // with them every address into the code.
                        self.cache.put_ops(bi, ops);
                        self.cache.clear(&mut self.mem);
                        self.jit.reset();
                        continue;
                    }
                }
            }
            let leave = match entry {
                Some(entry) => {
                    self.cache.link(bi, jit::body(entry), ops.as_ptr());
                    self.run_native(entry, &ops, bi, bus)
                }
                None => self.run_ops(&ops, bi, bus),
            };
            self.cache.put_ops(bi, ops);
            if !self.mem.smc.is_empty() {
                self.flush_smc();
            }
            if let Some(e) = leave {
                break e;
            }
        };
        self.mem.leave();
        exit
    }

    /// Interprets a block: `Some` when `run` must return.
    fn run_ops(&mut self, ops: &[Uop], bi: u32, bus: &mut dyn Bus) -> Option<Exit> {
        for u in ops {
            self.snap = Snap { gpr: self.gpr, flags: self.flags };
            match (u.h)(self, u, bus) {
                Ok(Flow::Next) => {}
                step => return self.after(u, step, bus),
            }
            if !self.mem.smc.is_empty() {
                self.flush_smc();
                if !self.cache.is_valid(bi) {
                    self.eip = u.next;
                    return None;
                }
            }
        }
        if let Some(last) = ops.last() {
            self.eip = last.next;
        }
        None
    }

    /// Runs block `bi`'s host code, and the compiled blocks it goes on to:
    /// `Some` when `run` must return.
    fn run_native(
        &mut self,
        entry: usize,
        ops: &[Uop],
        bi: u32,
        bus: &mut dyn Bus,
    ) -> Option<Exit> {
        let status = jit::run(entry, self, ops, bi, bus);
        if status >> 32 != jit::STOPPED {
            return None;
        }
        // The block that stopped: this one, or one the code went on to,
        // whose ops are in the cache.
        let k = status as u32 as usize;
        let cur = self.jit.current();
        let u = if cur == bi { ops.get(k).copied() } else { self.cache.op(cur, k) }?;
        let step = self.jit.take_step()?;
        self.after(&u, step, bus)
    }

    /// Acts on the result of instruction `u`, which ends its block: stores
    /// `eip`, undoes a stopped instruction and asks the bus about a fault.
    /// `Some` when `run` must return.
    fn after(&mut self, u: &Uop, step: Step, bus: &mut dyn Bus) -> Option<Exit> {
        let interruptible = |c: &Self| c.exit_if && c.flags.get() & IF != 0;
        match step {
            Ok(Flow::Next) => {
                self.eip = u.next;
                None
            }
            Ok(Flow::Jump(t)) => {
                self.eip = t;
                None
            }
            Ok(Flow::JumpEnd(t)) => {
                self.eip = t;
                interruptible(self).then_some(Exit::Kicked)
            }
            Ok(Flow::End) => {
                self.eip = u.next;
                interruptible(self).then_some(Exit::Kicked)
            }
            Ok(Flow::Exit(e)) => {
                self.eip = u.next;
                Some(e)
            }
            Err(stop) => {
                self.gpr = self.snap.gpr;
                self.flags = self.snap.flags;
                self.eip = u.ip;
                match stop {
                    Stop::Page(f) => self.page_fault(f, bus),
                    Stop::Exc(vector, error_code) => {
                        Some(Exit::Exception { vector, error_code, address: 0 })
                    }
                    Stop::Fatal(s) => Some(Exit::Fatal(s)),
                }
            }
        }
    }

    // ---- registers ----

    /// Reads a general, segment or control register.
    #[inline]
    pub(crate) fn reg(&self, r: Register) -> u32 {
        let n = r as u32;
        match n {
            37..=44 => self.gpr[(n - 37) as usize],
            21..=28 => self.gpr[(n - 21) as usize] & 0xFFFF,
            1..=4 => self.gpr[(n - 1) as usize] & 0xFF,
            5..=8 => (self.gpr[(n - 5) as usize] >> 8) & 0xFF,
            _ => match r {
                Register::CS => 0x08,
                Register::FS => 0x28,
                Register::GS => 0,
                Register::ES | Register::SS | Register::DS => 0x10,
                _ => 0,
            },
        }
    }

    /// Writes a general register; segment loads are ignored (the flat
    /// model's selectors stay, as in the reference).
    #[inline]
    pub(crate) fn set_reg(&mut self, r: Register, v: u32) {
        let n = r as u32;
        match n {
            37..=44 => self.gpr[(n - 37) as usize] = v,
            21..=28 => {
                let g = &mut self.gpr[(n - 21) as usize];
                *g = (*g & 0xFFFF_0000) | (v & 0xFFFF);
            }
            1..=4 => {
                let g = &mut self.gpr[(n - 1) as usize];
                *g = (*g & !0xFF) | (v & 0xFF);
            }
            5..=8 => {
                let g = &mut self.gpr[(n - 5) as usize];
                *g = (*g & !0xFF00) | ((v & 0xFF) << 8);
            }
            _ => {}
        }
    }

    // ---- operands ----

    /// The linear address of a memory operand.
    #[inline]
    pub(crate) fn ea(&self, ins: &Instruction) -> u32 {
        let base = ins.memory_base();
        let index = ins.memory_index();
        let mut a = ins.memory_displacement32();
        if base != Register::None {
            a = a.wrapping_add(self.reg(base));
        }
        if index != Register::None {
            a = a.wrapping_add(self.reg(index).wrapping_mul(ins.memory_index_scale()));
        }
        let b = base as u32;
        let x = index as u32;
        if (21..=28).contains(&b)
            || (21..=28).contains(&x)
            || (base == Register::None && index == Register::None && ins.memory_displ_size() == 2)
        {
            a &= 0xFFFF;
        }
        if ins.memory_segment() == Register::FS {
            a = a.wrapping_add(self.fs_base);
        }
        a
    }

    /// The size in bits of operand `i` (a register or memory).
    #[inline]
    pub(crate) fn op_bits(ins: &Instruction, i: u32) -> u32 {
        match ins.op_kind(i) {
            OpKind::Register => match ins.op_register(i) as u32 {
                1..=8 => 8,
                21..=28 => 16,
                _ => 32,
            },
            OpKind::Memory => (ins.memory_size().size() * 8) as u32,
            OpKind::Immediate8 => 8,
            OpKind::Immediate16 | OpKind::Immediate8to16 => 16,
            _ => 32,
        }
    }

    /// Loads `bits` (8, 16 or 32) from `va`.
    #[inline]
    pub(crate) fn load(&mut self, va: u32, bits: u32, bus: &mut dyn Bus) -> Result<u32, Stop> {
        Ok(match bits {
            8 => u32::from(self.mem.read::<1>(va, bus)?[0]),
            16 => u32::from(u16::from_le_bytes(self.mem.read::<2>(va, bus)?)),
            _ => u32::from_le_bytes(self.mem.read::<4>(va, bus)?),
        })
    }

    /// Stores the low `bits` of `v` at `va`.
    #[inline]
    pub(crate) fn store(
        &mut self,
        va: u32,
        bits: u32,
        v: u32,
        bus: &mut dyn Bus,
    ) -> Result<(), Stop> {
        let b = v.to_le_bytes();
        match bits {
            8 => self.mem.write_n(va, [b[0]], bus)?,
            16 => self.mem.write_n(va, [b[0], b[1]], bus)?,
            _ => self.mem.write_n(va, b, bus)?,
        }
        Ok(())
    }

    pub(crate) fn load64(&mut self, va: u32, bus: &mut dyn Bus) -> Result<u64, Stop> {
        Ok(u64::from_le_bytes(self.mem.read::<8>(va, bus)?))
    }

    pub(crate) fn store64(&mut self, va: u32, v: u64, bus: &mut dyn Bus) -> Result<(), Stop> {
        self.mem.write_n(va, v.to_le_bytes(), bus)?;
        Ok(())
    }

    /// Reads operand `i` with `bits` bits.
    #[inline]
    pub(crate) fn rd(
        &mut self,
        ins: &Instruction,
        i: u32,
        bits: u32,
        bus: &mut dyn Bus,
    ) -> Result<u32, Stop> {
        match ins.op_kind(i) {
            OpKind::Register => Ok(self.reg(ins.op_register(i))),
            OpKind::Memory => {
                let a = self.ea(ins);
                self.load(a, bits, bus)
            }
            OpKind::Immediate8 => Ok(u32::from(ins.immediate8())),
            OpKind::Immediate8_2nd => Ok(u32::from(ins.immediate8_2nd())),
            OpKind::Immediate16 => Ok(u32::from(ins.immediate16())),
            OpKind::Immediate32 => Ok(ins.immediate32()),
            OpKind::Immediate8to16 => Ok(u32::from(ins.immediate8to16() as u16)),
            OpKind::Immediate8to32 => Ok(ins.immediate8to32() as u32),
            k => Err(Stop::Fatal(format!("interp: operand kind {k:?} read as a value"))),
        }
    }

    /// Writes operand `i` (a register or memory) with `bits` bits.
    #[inline]
    pub(crate) fn wr(
        &mut self,
        ins: &Instruction,
        i: u32,
        bits: u32,
        v: u32,
        bus: &mut dyn Bus,
    ) -> Result<(), Stop> {
        match ins.op_kind(i) {
            OpKind::Register => {
                self.set_reg(ins.op_register(i), v);
                Ok(())
            }
            OpKind::Memory => {
                let a = self.ea(ins);
                self.store(a, bits, v, bus)
            }
            k => Err(Stop::Fatal(format!("interp: operand kind {k:?} written"))),
        }
    }

    // ---- the stack ----

    pub(crate) fn push(&mut self, v: u32, bits: u32, bus: &mut dyn Bus) -> Result<(), Stop> {
        let sp = self.gpr[4].wrapping_sub(bits / 8);
        self.store(sp, bits, v, bus)?;
        self.gpr[4] = sp;
        Ok(())
    }

    pub(crate) fn pop(&mut self, bits: u32, bus: &mut dyn Bus) -> Result<u32, Stop> {
        let v = self.load(self.gpr[4], bits, bus)?;
        self.gpr[4] = self.gpr[4].wrapping_add(bits / 8);
        Ok(v)
    }

    // ---- the bus, from inside an instruction ----

    /// Calls `f` on the bus with the CPU counted as outside guest code.
    pub(crate) fn outside<T>(&mut self, bus: &mut dyn Bus, f: impl FnOnce(&mut dyn Bus) -> T) -> T {
        self.mem.leave();
        let v = f(bus);
        self.mem.enter();
        v
    }

    /// Whether a kick is pending (it stays pending for the run loop).
    pub(crate) fn kicked(&self) -> bool {
        self.kick.load(Ordering::Relaxed)
    }

    /// Reports an instruction the interpreter does not implement (once per
    /// mnemonic in the log) and stops.
    pub(crate) fn unimplemented(&mut self, ins: &Instruction) -> Stop {
        use iced_x86::Formatter;
        let mut text = String::new();
        iced_x86::IntelFormatter::new().format(ins, &mut text);
        let msg = format!("interp: no implementation of `{text}` at {:#010x}", ins.ip32());
        if self.reported.insert(ins.mnemonic()) {
            log::warn!(target: "cpu", "{msg}");
        }
        Stop::Fatal(msg)
    }
}

impl Mapper for InterpCpu {
    fn map(&mut self, va: u32, pa: u32, len: u32, access: Access) {
        self.mem.map(va, pa, len, access);
    }

    fn map_mmio(&mut self, va: u32, pa: u32, len: u32) {
        self.mem.map_mmio(va, pa, len);
    }

    fn unmap(&mut self, va: u32, len: u32) {
        self.mem.unmap(va, len);
    }
}

impl Cpu for InterpCpu {
    fn name(&self) -> &'static str {
        "interp"
    }

    fn run(&mut self, ctx: &mut Context, bus: &mut dyn Bus, flags: RunFlags) -> Exit {
        if self.kick.swap(false, Ordering::AcqRel) {
            return Exit::Kicked;
        }
        if flags.exit_when_interruptible && ctx.regs.eflags & IF != 0 {
            return Exit::Kicked;
        }
        self.exit_if = flags.exit_when_interruptible;
        self.load_ctx(ctx);
        let exit = self.exec(bus);
        self.store_ctx(ctx);
        exit
    }

    fn kicker(&self) -> Kicker {
        let k = self.kick.clone();
        Kicker::new(move || k.store(true, Ordering::Release))
    }

    fn protector(&self) -> Arc<dyn PageProtector> {
        self.mem.protector()
    }

    fn set_trap_range(&mut self, va: u32, len: u32) {
        self.trap = (va, u64::from(va) + u64::from(len));
        self.jit.set_trap(self.trap);
    }

    fn invalidate_code(&mut self, pa: u32, len: u32) {
        if len == 0 {
            return;
        }
        let first = pa / 0x1000;
        let last = pa.saturating_add(len - 1) / 0x1000;
        for p in first..=last {
            self.cache.invalidate_page(p, &mut self.mem);
        }
    }
}
