//! Guest-code snippets as data, and the small machine they run on.
//!
//! Exported with the `testing` feature so the same snippets can run on
//! another backend (`xb-cpu-kvm`) and the results be compared: everything
//! here is generic over `xb_cpu::Cpu` and touches guest RAM only through a
//! byte slice the caller provides (its host view of physical RAM).
//!
//! The machine: [`RAM_SIZE`] bytes of RAM; code at [`CODE`], data at
//! [`DATA`], one page at address 0 (what 16-bit addressing reaches), a stack below [`STACK_TOP`] and a stand-in for the kernel's
//! per-processor block at [`FS_BASE`], each mapped at the physical address
//! equal to its virtual one; all of RAM again at `0x8000_0000`; the trap
//! range at [`TRAP`]. A snippet is loaded at [`CODE`] with `jmp END`
//! appended, so one that runs to its end returns `Exit::Trap` at [`END`].

use iced_x86::code_asm::*;
use xb_cpu::{Access, Bus, Context, Cpu, Exit, FaultAction, reg};

/// Bytes of guest RAM the snippets need.
pub const RAM_SIZE: u32 = 8 << 20;
/// Where snippets are loaded (virtual and physical).
pub const CODE: u32 = 0x0001_0000;
/// Size of the code mapping.
pub const CODE_LEN: u32 = 0x4_0000;
/// Scratch data (virtual and physical), 64 KiB.
pub const DATA: u32 = 0x0010_0000;
/// The stack's mapping, 64 KiB.
pub const STACK: u32 = 0x0020_0000;
/// Initial ESP.
pub const STACK_TOP: u32 = 0x0020_F000;
/// FS base of every snippet's context; one page is mapped there.
pub const FS_BASE: u32 = 0x0030_0000;
/// The contiguous window: physical `p` at `WINDOW + p`.
pub const WINDOW: u32 = 0x8000_0000;
/// The trap range, 64 KiB.
pub const TRAP: u32 = 0xFFF0_0000;
/// Where a snippet that runs to its end arrives.
pub const END: u32 = TRAP;

/// Maps the snippets' machine into `cpu`.
pub fn map_machine(cpu: &mut dyn Cpu) {
    cpu.map(0, 0, 0x1000, Access::ReadWrite);
    cpu.map(CODE, CODE, CODE_LEN, Access::ReadWrite);
    cpu.map(DATA, DATA, 0x1_0000, Access::ReadWrite);
    cpu.map(STACK, STACK, 0x1_0000, Access::ReadWrite);
    cpu.map(FS_BASE, FS_BASE, 0x1000, Access::ReadWrite);
    cpu.map(WINDOW, 0, RAM_SIZE, Access::ReadWrite);
    cpu.set_trap_range(TRAP, 0x1_0000);
}

/// One piece of guest code with its inputs and what it must leave behind.
#[derive(Clone, Debug)]
pub struct Snippet {
    /// What it exercises.
    pub name: &'static str,
    /// Machine code for address [`CODE`] (`jmp END` is appended on load).
    pub code: Vec<u8>,
    /// Initial general registers ([`xb_cpu::reg`] order).
    pub regs: [u32; 8],
    /// Initial EFLAGS.
    pub eflags: u32,
    /// Bytes written to guest memory (virtual addresses in the identity
    /// mapped areas) before the run.
    pub mem: Vec<(u32, Vec<u8>)>,
    /// Registers that must hold these values at the end.
    pub expect_regs: Vec<(usize, u32)>,
    /// `(mask, value)`: EFLAGS bits that must match at the end.
    pub expect_flags: Option<(u32, u32)>,
    /// Guest memory that must hold these bytes at the end.
    pub expect_mem: Vec<(u32, Vec<u8>)>,
}

impl Snippet {
    /// A snippet assembled from `build` (32-bit code at [`CODE`]) with the
    /// default registers: zero except ESP ([`STACK_TOP`]) and EBX ([`DATA`]).
    ///
    /// # Panics
    /// If `build` produces something that does not assemble.
    #[must_use]
    pub fn new(
        name: &'static str,
        build: impl FnOnce(&mut CodeAssembler) -> Result<(), IcedError>,
    ) -> Self {
        let mut a = CodeAssembler::new(32).expect("32-bit assembler");
        build(&mut a).unwrap_or_else(|e| panic!("{name}: {e}"));
        let code = a.assemble(u64::from(CODE)).unwrap_or_else(|e| panic!("{name}: {e}"));
        Self::bytes(name, code)
    }

    /// A snippet from raw bytes.
    #[must_use]
    pub fn bytes(name: &'static str, code: Vec<u8>) -> Self {
        let mut regs = [0u32; 8];
        regs[reg::ESP] = STACK_TOP;
        regs[reg::EBX] = DATA;
        Self {
            name,
            code,
            regs,
            eflags: 0x202,
            mem: Vec::new(),
            expect_regs: Vec::new(),
            expect_flags: None,
            expect_mem: Vec::new(),
        }
    }

    /// Adds code at `CODE + offset` (the gap filled with NOPs): a place with
    /// a known address for indirect jumps and calls.
    ///
    /// # Panics
    /// If `build` does not assemble or the code so far passes `offset`.
    #[must_use]
    pub fn at(
        mut self,
        offset: u32,
        build: impl FnOnce(&mut CodeAssembler) -> Result<(), IcedError>,
    ) -> Self {
        assert!(self.code.len() <= offset as usize, "{}: code passes {offset:#x}", self.name);
        self.code.resize(offset as usize, 0x90);
        let mut a = CodeAssembler::new(32).expect("32-bit assembler");
        build(&mut a).unwrap_or_else(|e| panic!("{}: {e}", self.name));
        let more =
            a.assemble(u64::from(CODE + offset)).unwrap_or_else(|e| panic!("{}: {e}", self.name));
        self.code.extend_from_slice(&more);
        self
    }

    /// Sets an initial register.
    #[must_use]
    pub fn reg(mut self, r: usize, v: u32) -> Self {
        self.regs[r] = v;
        self
    }

    /// Sets initial memory.
    #[must_use]
    pub fn mem(mut self, va: u32, bytes: &[u8]) -> Self {
        self.mem.push((va, bytes.to_vec()));
        self
    }

    /// Sets initial memory to little-endian doublewords.
    #[must_use]
    pub fn mem32(self, va: u32, words: &[u32]) -> Self {
        let b: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        self.mem(va, &b)
    }

    /// Expects a register's final value.
    #[must_use]
    pub fn expect(mut self, r: usize, v: u32) -> Self {
        self.expect_regs.push((r, v));
        self
    }

    /// Expects final EFLAGS bits.
    #[must_use]
    pub fn flags(mut self, mask: u32, value: u32) -> Self {
        self.expect_flags = Some((mask, value));
        self
    }

    /// Expects final memory.
    #[must_use]
    pub fn expect_mem(mut self, va: u32, bytes: &[u8]) -> Self {
        self.expect_mem.push((va, bytes.to_vec()));
        self
    }

    /// Expects final memory as little-endian doublewords.
    #[must_use]
    pub fn expect_mem32(self, va: u32, words: &[u32]) -> Self {
        let b: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        self.expect_mem(va, &b)
    }

    /// Writes the snippet's code and initial memory into `ram` (the host
    /// view of physical RAM) and tells `cpu` the code changed.
    pub fn load(&self, cpu: &mut dyn Cpu, ram: &mut [u8]) {
        let at = CODE as usize;
        ram[at..at + self.code.len()].copy_from_slice(&self.code);
        // jmp END
        let jmp = at + self.code.len();
        let rel = END.wrapping_sub(jmp as u32 + 5);
        ram[jmp] = 0xE9;
        ram[jmp + 1..jmp + 5].copy_from_slice(&rel.to_le_bytes());
        for (va, bytes) in &self.mem {
            ram[*va as usize..*va as usize + bytes.len()].copy_from_slice(bytes);
        }
        cpu.invalidate_code(CODE, CODE_LEN);
    }

    /// The context the snippet starts from.
    #[must_use]
    pub fn context(&self) -> Context {
        let mut ctx = Context::default();
        ctx.regs.gpr = self.regs;
        ctx.regs.eip = CODE;
        ctx.regs.eflags = self.eflags;
        ctx.regs.fs_base = FS_BASE;
        ctx
    }

    /// Compares a final state with the expectations.
    ///
    /// # Errors
    /// The first difference, as text.
    pub fn check(&self, exit: &Exit, ctx: &Context, ram: &[u8]) -> Result<(), String> {
        if *exit != Exit::Trap || ctx.regs.eip != END {
            return Err(format!("{}: ended with {exit:?} at {:#010x}", self.name, ctx.regs.eip));
        }
        const NAMES: [&str; 8] = ["eax", "ecx", "edx", "ebx", "esp", "ebp", "esi", "edi"];
        for &(r, v) in &self.expect_regs {
            if ctx.regs.gpr[r] != v {
                return Err(format!(
                    "{}: {} is {:#010x}, expected {v:#010x}",
                    self.name, NAMES[r], ctx.regs.gpr[r]
                ));
            }
        }
        if let Some((mask, value)) = self.expect_flags
            && ctx.regs.eflags & mask != value
        {
            {
                return Err(format!(
                    "{}: eflags are {:#x}, expected {value:#x} under mask {mask:#x}",
                    self.name, ctx.regs.eflags
                ));
            }
        }
        for (va, bytes) in &self.expect_mem {
            let got = &ram[*va as usize..*va as usize + bytes.len()];
            if got != bytes.as_slice() {
                return Err(format!(
                    "{}: memory at {va:#010x} is {got:02x?}, expected {bytes:02x?}",
                    self.name
                ));
            }
        }
        Ok(())
    }

    /// Loads the snippet, runs it to its end and checks it.
    ///
    /// # Errors
    /// The first difference, as text.
    pub fn run(
        &self,
        cpu: &mut dyn Cpu,
        ram: &mut [u8],
        bus: &mut dyn Bus,
    ) -> Result<Context, String> {
        self.load(cpu, ram);
        let mut ctx = self.context();
        let exit = cpu.run(&mut ctx, bus, xb_cpu::RunFlags::default());
        self.check(&exit, &ctx, ram)?;
        Ok(ctx)
    }
}

/// A bus for snippets that touch no device: every callback but the time
/// stamp counter is an error.
#[derive(Default)]
pub struct NoBus;

impl Bus for NoBus {
    fn mmio_read(&mut self, pa: u32, _size: u8) -> u32 {
        panic!("unexpected device read at {pa:#010x}")
    }
    fn mmio_write(&mut self, pa: u32, _size: u8, _value: u32) {
        panic!("unexpected device write at {pa:#010x}")
    }
    fn io_read(&mut self, port: u16, _size: u8) -> u32 {
        panic!("unexpected read of port {port:#x}")
    }
    fn io_write(&mut self, port: u16, _size: u8, _value: u32) {
        panic!("unexpected write of port {port:#x}")
    }
    fn page_fault(&mut self, _va: u32, _pa: Option<u32>, _write: bool) -> FaultAction {
        FaultAction::Raise
    }
    fn tsc(&mut self) -> u64 {
        0
    }
}

const CF: u32 = 0x001;
const ZF: u32 = 0x040;
const SF: u32 = 0x080;
const DF: u32 = 0x400;
const OF: u32 = 0x800;

/// The snippets: pure computation on registers, the stack and memory, each
/// with the state it must end in.
#[must_use]
#[expect(clippy::too_many_lines, reason = "a table of test programs")]
pub fn snippets() -> Vec<Snippet> {
    use reg::{EAX, EBP, EBX, ECX, EDI, EDX, ESI, ESP};
    let mut v = Vec::new();
    let (a, b) = (0x8765_4321u32, 0x9ABC_DEF0u32);

    v.push(
        Snippet::new("add, adc, sub, sbb, neg", |c| {
            c.add(eax, ecx)?; // carry out
            c.adc(edx, 0)?;
            c.sub(esi, edi)?; // borrow
            c.sbb(ebp, 0)?;
            c.neg(ecx)
        })
        .reg(EAX, a)
        .reg(ECX, b)
        .reg(EDX, 10)
        .reg(ESI, 1)
        .reg(EDI, 2)
        .reg(EBP, 100)
        .expect(EAX, a.wrapping_add(b))
        .expect(EDX, 11)
        .expect(ESI, u32::MAX)
        .expect(EBP, 99)
        .expect(ECX, b.wrapping_neg())
        .flags(CF | ZF, CF),
    );
    v.push(
        Snippet::new("logic, test, cmp and the flags they leave", |c| {
            c.and(eax, ecx)?;
            c.or(edx, eax)?;
            c.xor(esi, esi)?; // ZF
            c.not(edi)?; // flags untouched
            c.cmp(edi, edi)
        })
        .reg(EAX, a)
        .reg(ECX, b)
        .reg(EDX, 0x0F)
        .reg(ESI, 77)
        .reg(EDI, 0x00FF_00FF)
        .expect(EAX, a & b)
        .expect(EDX, 0x0F | (a & b))
        .expect(ESI, 0)
        .expect(EDI, 0xFF00_FF00)
        .flags(CF | ZF | SF | OF, ZF),
    );
    v.push(
        Snippet::new("inc and dec short forms keep CF", |c| {
            c.stc()?;
            c.inc(eax)?;
            c.dec(ecx)?;
            c.inc(esp)?;
            c.dec(esp)?;
            c.inc(di)?;
            c.dec(si)
        })
        .reg(EAX, u32::MAX)
        .reg(ECX, 1)
        .reg(EDI, 0x1_FFFF)
        .reg(ESI, 0x2_0000)
        .expect(EAX, 0)
        .expect(ECX, 0)
        .expect(ESP, STACK_TOP)
        .expect(EDI, 0x1_0000)
        .expect(ESI, 0x2_FFFF)
        .flags(CF, CF),
    );
    v.push(
        Snippet::new("mul, imul, div, idiv, cdq", |c| {
            c.mul(ecx)?; // edx:eax = a * b
            c.mov(esi, eax)?;
            c.mov(edi, edx)?;
            c.mov(eax, 1000)?;
            c.xor(edx, edx)?;
            c.mov(ecx, 7)?;
            c.div(ecx)?; // 142 r 6
            c.mov(ebp, edx)?;
            c.imul_3(ecx, eax, -3)?; // -426
            c.mov(eax, -1000)?;
            c.cdq()?;
            c.mov(ebx, 7)?;
            c.idiv(ebx) // -142 r -6
        })
        .reg(EAX, a)
        .reg(ECX, b)
        .expect(ESI, (u64::from(a) * u64::from(b)) as u32)
        .expect(EDI, ((u64::from(a) * u64::from(b)) >> 32) as u32)
        .expect(EBP, 6)
        .expect(ECX, (-426i32) as u32)
        .expect(EAX, (-142i32) as u32)
        .expect(EDX, (-6i32) as u32),
    );
    v.push(
        Snippet::new("shifts and rotates, by CL and by constants", |c| {
            c.shl(eax, cl)?;
            c.shr(edx, 4)?;
            c.sar(esi, 4)?;
            c.rol(edi, 8)?;
            c.ror(ebp, cl)?;
            c.shld(ebx, edi, 4)?;
            c.stc()?;
            c.rcl(ecx, 1)
        })
        .reg(EAX, a)
        .reg(ECX, 12)
        .reg(EDX, a)
        .reg(ESI, a)
        .reg(EDI, a)
        .reg(EBP, a)
        .reg(EBX, 0x0000_0001)
        .expect(EAX, a << 12)
        .expect(EDX, a >> 4)
        .expect(ESI, ((a as i32) >> 4) as u32)
        .expect(EDI, a.rotate_left(8))
        .expect(EBP, a.rotate_right(12))
        .expect(EBX, 0x10 | (a.rotate_left(8) >> 28))
        .expect(ECX, 25),
    );
    v.push(
        Snippet::new("bit tests, scans, bswap, xadd, cmpxchg, setcc, cmovcc", |c| {
            c.bts(dword_ptr(ebx), eax)?; // bit 5 of [DATA]
            c.bt(ecx, 31)?; // CF
            c.setc(dl)?;
            c.bsf(esi, ecx)?;
            c.bsr(edi, ecx)?;
            c.bswap(ecx)?;
            c.mov(eax, 7)?;
            c.mov(ebp, 9)?;
            c.xadd(eax, ebp)?; // eax 16, ebp 7
            c.lock().cmpxchg(dword_ptr(ebx + 4), ebp)?; // [DATA+4] is 16: becomes 7
            c.cmove(esp, esp)?;
            c.sete(dh)
        })
        .reg(EAX, 5)
        .reg(ECX, 0x8000_1000)
        .mem32(DATA, &[0, 16])
        .expect(EDX, 0x0101)
        .expect(ESI, 12)
        .expect(EDI, 31)
        .expect(ECX, 0x0010_0080)
        .expect(EAX, 16)
        .expect(EBP, 7)
        .expect(ESP, STACK_TOP)
        .expect_mem32(DATA, &[0x20, 7]),
    );
    v.push(
        Snippet::new("8- and 16-bit operands with AH, CH, DH, BH", |c| {
            c.add(al, ah)?;
            c.xchg(cl, ch)?;
            c.movzx(esi, dh)?;
            c.movsx(edi, dl)?;
            c.add(dx, cx)?;
            c.mov(byte_ptr(ebx), ah)?;
            c.sub(byte_ptr(ebx), 1)?;
            c.mov(bh, byte_ptr(ebx))
        })
        .reg(EAX, 0x1234_56F0)
        .reg(ECX, 0xAAAA_1122)
        .reg(EDX, 0x5555_8081)
        .expect(EAX, 0x1234_5646)
        .expect(ECX, 0xAAAA_2211)
        .expect(ESI, 0x80)
        .expect(EDI, 0xFFFF_FF81)
        .expect(EDX, 0x5555_A292)
        .expect(EBX, (DATA & 0xFFFF_00FF) | 0x5500)
        .expect_mem(DATA, &[0x55]),
    );
    v.push(
        Snippet::new("ESP as an operand, beside AH and in arithmetic", |c| {
            c.push(eax)?;
            c.mov(ah, byte_ptr(esp + 1))?; // 0x33
            c.add(byte_ptr(esp), ah)?; // 0x44 + 0x33
            c.mov(ecx, dword_ptr(esp))?;
            c.lea(esp, dword_ptr(esp - 16))?;
            c.mov(edx, esp)?;
            c.sub(sp, 8)?;
            c.movzx(esi, sp)?;
            c.xchg(edi, esp)?;
            c.xchg(edi, esp)?;
            c.add(esp, 28)
        })
        .reg(EAX, 0x1122_3344)
        .expect(EAX, 0x1122_3344)
        .expect(ECX, 0x1122_3377)
        .expect(EDX, STACK_TOP - 20)
        .expect(ESI, (STACK_TOP - 28) & 0xFFFF)
        .expect(EDI, 0)
        .expect(ESP, STACK_TOP),
    );
    v.push(
        Snippet::new("push and pop of registers, immediates and memory", |c| {
            c.push(eax)?;
            c.push(0x1234_5678)?;
            c.push(-2)?;
            c.push(dword_ptr(ebx))?;
            c.push(esp)?; // the value before the push
            c.pop(edi)?;
            c.pop(dword_ptr(ebx + 4))?;
            c.pop(ecx)?;
            c.pop(edx)?;
            c.push(dword_ptr(esp))?; // eax's copy again
            c.pop(dword_ptr(esp))?; // over itself: [esp] after the pop
            c.pop(esi)
        })
        .reg(EAX, a)
        .mem32(DATA, &[0xCAFE_F00D])
        .expect(EDI, STACK_TOP - 16)
        .expect(ECX, (-2i32) as u32)
        .expect(EDX, 0x1234_5678)
        .expect(ESI, a)
        .expect(ESP, STACK_TOP)
        .expect_mem32(DATA + 4, &[0xCAFE_F00D]),
    );
    v.push(
        Snippet::new("16-bit pushes and pops, pushaw and popaw", |c| {
            c.push(ax)?;
            c.db(&[0x66, 0x68, 0xEF, 0xBE])?; // push word 0xBEEF
            c.pop(cx)?;
            c.pop(dx)?;
            c.db(&[0x66, 0x60])?; // pushaw
            c.mov(eax, 0)?;
            c.mov(ecx, 0)?;
            c.mov(si, word_ptr(esp + 12))?; // cx's slot
            c.db(&[0x66, 0x61]) // popaw
        })
        .reg(EAX, 0xAAAA_1234)
        .reg(ECX, 0xCCCC_0000)
        .reg(EDX, 0xDDDD_0000)
        .reg(ESI, 0x5555_0000)
        .expect(EAX, 0x1234)
        .expect(ECX, 0xBEEF)
        .expect(EDX, 0xDDDD_1234)
        .expect(ESI, 0x5555_0000)
        .expect(ESP, STACK_TOP),
    );
    v.push(
        Snippet::new("pushad and popad", |c| {
            c.pushad()?;
            c.mov(ebp, dword_ptr(esp + 12))?; // esp's slot: the value before
            c.xor(eax, eax)?;
            c.xor(ecx, ecx)?;
            c.xor(edx, edx)?;
            c.xor(esi, esi)?;
            c.mov(dword_ptr(ebx), ebp)?;
            c.popad()
        })
        .reg(EAX, 1)
        .reg(ECX, 2)
        .reg(EDX, 3)
        .reg(EBP, 6)
        .reg(ESI, 7)
        .reg(EDI, 8)
        .expect(EAX, 1)
        .expect(ECX, 2)
        .expect(EDX, 3)
        .expect(EBX, DATA)
        .expect(EBP, 6)
        .expect(ESI, 7)
        .expect(EDI, 8)
        .expect(ESP, STACK_TOP)
        .expect_mem32(DATA, &[STACK_TOP]),
    );
    v.push(
        Snippet::new("pushfd and popfd carry the arithmetic flags and DF", |c| {
            c.std()?;
            c.stc()?;
            c.pushfd()?;
            c.pop(eax)?; // CF DF IF set
            c.cld()?;
            c.clc()?;
            c.push(eax)?;
            c.popfd()?; // CF and DF back
            c.pushfd()?;
            c.pop(ecx)?;
            c.cld()
        })
        .expect(EAX, 0x202 | CF | DF)
        .expect(ECX, 0x202 | CF | DF)
        .flags(CF | DF, CF),
    );
    v.push(
        Snippet::new("the AC and ID flags can be set and read back", |c| {
            c.pushfd()?;
            c.or(dword_ptr(esp), 0x24_0000)?;
            c.popfd()?;
            c.pushfd()?;
            c.pop(eax)?;
            c.and(eax, 0x24_0000)?;
            c.push(0x202)?;
            c.popfd()?;
            c.pushfd()?;
            c.pop(ecx)
        })
        .expect(EAX, 0x24_0000)
        .expect(ECX, 0x202),
    );
    v.push(
        Snippet::new("16-bit pushf and popf", |c| {
            c.stc()?;
            c.db(&[0x66, 0x9C])?; // pushf
            c.pop(ax)?;
            c.clc()?;
            c.push(ax)?;
            c.db(&[0x66, 0x9D])?; // popf
            c.setc(cl)
        })
        .expect(EAX, 0x203)
        .expect(ECX, 1)
        .expect(ESP, STACK_TOP),
    );
    v.push(
        Snippet::new("enter and leave, plain and with a nesting level", |c| {
            c.mov(ebp, 0x1111_1111)?;
            c.enter(32, 0)?;
            c.mov(eax, ebp)?;
            c.mov(ecx, esp)?;
            c.mov(dword_ptr(ebp - 4), 0x77)?;
            c.enter(8, 2)?; // one outer frame pointer copied
            c.mov(edx, ebp)?;
            c.mov(esi, dword_ptr(ebp - 4))?; // the outer frame pointer
            c.mov(edi, esp)?;
            c.leave()?;
            c.leave()
        })
        .expect(EAX, STACK_TOP - 4)
        .expect(ECX, STACK_TOP - 36)
        .expect(EDX, STACK_TOP - 40)
        .expect(ESI, 0x77)
        .expect(EDI, STACK_TOP - 56)
        .expect(EBP, 0x1111_1111)
        .expect(ESP, STACK_TOP),
    );
    v.push(
        Snippet::new("call, ret, ret imm, nested and indirect calls", |c| {
            let mut f = c.create_label();
            let mut g = c.create_label();
            let mut end = c.create_label();
            c.call(f)?; // eax += 2, calls g
            c.mov(esi, CODE + 0x80)?;
            c.push(5)?;
            c.call(esi)?; // h: ecx += arg, ret 4
            c.mov(dword_ptr(ebx), esi)?;
            c.push(6)?;
            c.call(dword_ptr(ebx))?;
            c.jmp(end)?;
            c.set_label(&mut f)?;
            c.inc(eax)?;
            c.call(g)?;
            c.inc(eax)?;
            c.ret()?;
            c.set_label(&mut g)?;
            c.add(edx, 10)?;
            c.ret()?;
            c.set_label(&mut end)?;
            c.jmp(u64::from(CODE) + 0x90)
        })
        .at(0x80, |c| {
            c.add(ecx, dword_ptr(esp + 4))?;
            c.ret_1(4)
        })
        .at(0x90, |c| c.nop())
        .expect(EAX, 2)
        .expect(EDX, 10)
        .expect(ECX, 5 + 6)
        .expect(ESP, STACK_TOP),
    );
    v.push(
        Snippet::new("indirect jumps, jecxz, loop and a return address replaced", |c| {
            c.mov(esi, CODE + 0x40)?;
            c.jmp(esi)?;
            c.mov(eax, 0xBAD)
        })
        .at(0x40, |c| {
            c.mov(dword_ptr(ebx), CODE + 0x60)?;
            c.jmp(dword_ptr(ebx))?;
            c.mov(eax, 0xBAD)
        })
        .at(0x60, |c| {
            let mut l = c.create_label();
            let mut skip = c.create_label();
            c.mov(ecx, 5)?;
            c.set_label(&mut l)?;
            c.inc(edx)?;
            c.loop_(l)?;
            c.jecxz(skip)?;
            c.mov(eax, 0xBAD)?;
            c.set_label(&mut skip)?;
            c.push(CODE + 0xA0)?;
            c.ret()?; // not a return the shadow stack predicts
            c.mov(eax, 0xBAD)
        })
        .at(0xA0, |c| c.inc(ebp))
        .expect(EAX, 0)
        .expect(EDX, 5)
        .expect(ECX, 0)
        .expect(EBP, 1)
        .expect(ESP, STACK_TOP),
    );
    v.push(
        Snippet::new("a counted loop long enough to run chained", |c| {
            let mut l = c.create_label();
            let mut odd = c.create_label();
            c.mov(ecx, 200_000)?;
            c.set_label(&mut l)?;
            c.add(eax, ecx)?;
            c.test(ecx, 1)?;
            c.jnz(odd)?;
            c.inc(edx)?;
            c.set_label(&mut odd)?;
            c.dec(ecx)?;
            c.jnz(l)
        })
        .expect(EAX, (200_000u64 * 200_001 / 2) as u32)
        .expect(EDX, 100_000)
        .expect(ECX, 0),
    );
    v.push(
        Snippet::new("rep movs, stos, lods, scas and cmps, forwards and backwards", |c| {
            c.lea(esi, dword_ptr(ebx))?;
            c.lea(edi, dword_ptr(ebx + 0x100))?;
            c.mov(ecx, 3)?;
            c.rep().movsd()?; // 12 bytes
            c.mov(ecx, 2)?;
            c.rep().movsb()?; // 2 more
            c.mov(eax, 0x5A5A_5A5A)?;
            c.mov(ecx, 2)?;
            c.rep().stosw()?; // at +0x10E
            c.lea(esi, dword_ptr(ebx))?;
            c.lodsb()?; // al = 0x01
            c.lea(edi, dword_ptr(ebx))?;
            c.mov(ecx, 100)?;
            c.mov(al, 0x07)?;
            c.repne().scasb()?; // found at index 6: ecx 93
            c.mov(edx, ecx)?;
            c.lea(esi, dword_ptr(ebx))?;
            c.lea(edi, dword_ptr(ebx + 0x100))?;
            c.mov(ecx, 14)?;
            c.repe().cmpsb()?; // all equal: ecx 0, ZF
            c.setz(ah)?;
            c.std()?;
            c.lea(edi, dword_ptr(ebx + 0x203))?;
            c.mov(ecx, 4)?;
            c.rep().stosb()?; // 0x200..0x203, backwards
            c.cld()
        })
        .mem(DATA, &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16])
        .expect(EAX, 0x5A5A_0107)
        .expect(EDX, 93)
        .expect(ECX, 0)
        .expect(ESI, DATA + 14)
        .expect(EDI, DATA + 0x1FF)
        .expect_mem(
            DATA + 0x100,
            &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 0x5A, 0x5A, 0x5A, 0x5A],
        )
        .expect_mem(DATA + 0x200, &[7, 7, 7, 7]),
    );
    v.push(
        Snippet::new("operand-size and address-size prefixes", |c| {
            c.add(ax, cx)?; // 16-bit: the upper half stays
            c.mov(dx, word_ptr(DATA))?;
            c.mov(word_ptr(DATA + 4), cx)?;
            c.db(&[0x67, 0x8B, 0x37])?; // mov esi, [bx]: 16-bit addressing
            c.db(&[0x67, 0x8B, 0x7F, 0x04])?; // mov edi, [bx+4]
            c.db(&[0x67, 0x8D, 0x6F, 0xFF]) // lea ebp, [bx-1]
        })
        .reg(EAX, 0x1111_FFFF)
        .reg(ECX, 0x2222_0002)
        .reg(EDX, 0x3333_0000)
        .reg(EBX, 0xABCD_0010)
        .mem32(0x10, &[0x1234_5678, 0x9ABC_DEF0])
        .mem32(DATA, &[0x0000_BEEF, 0])
        .expect(EAX, 0x1111_0001)
        .expect(EDX, 0x3333_BEEF)
        .expect(ESI, 0x1234_5678)
        .expect(EDI, 0x9ABC_DEF0)
        .expect(EBP, 0x0F)
        .expect_mem(DATA + 4, &[2, 0]),
    );
    v.push(
        Snippet::new("mov with absolute addresses and scaled indexes", |c| {
            c.mov(eax, dword_ptr(DATA + 0x10))?; // A1 moffs
            c.mov(dword_ptr(DATA + 0x20), eax)?;
            c.mov(al, byte_ptr(DATA + 0x11))?;
            c.mov(esi, dword_ptr(ecx * 4 + (DATA + 0x10)))?;
            c.mov(edi, dword_ptr(ebx + ecx * 8 + 0x10))?;
            c.xlatb()
        })
        .reg(ECX, 2)
        .mem32(DATA + 0x10, &[0x4433_2211, 0, 0x8877_6655, 0, 0xCCBB_AA99])
        .expect(ESI, 0x8877_6655)
        .expect(EDI, 0x4433_2211) // the doubleword just stored at +0x20
        .expect(EAX, 0x4433_2233) // xlat: the byte at +0x22
        .expect_mem32(DATA + 0x20, &[0x4433_2211]),
    );
    v.push(
        Snippet::new("FS-relative operands in every addressing shape", |c| {
            c.mov(eax, dword_ptr(0x28u32).fs())?; // moffs
            c.mov(ecx, dword_ptr(esi + 4).fs())?;
            c.mov(edx, dword_ptr(esi + edi * 4).fs())?;
            c.mov(dword_ptr(edi * 4 + 0x40u32).fs(), esi)?;
            c.push(dword_ptr(0u32).fs())?;
            c.mov(dword_ptr(0u32).fs(), esp)?;
            c.mov(ebp, dword_ptr(0u32).fs())?;
            c.pop(dword_ptr(0u32).fs())?;
            c.mov(bh, byte_ptr(esi + 5).fs())?; // AH-BH beside FS: a borrowed register
            c.mov(bl, byte_ptr(0x29u32).fs())?;
            c.lea(edi, dword_ptr(esi + 0x100).fs()) // the address only
        })
        .reg(ESI, 0x20)
        .reg(EDI, 2)
        .mem32(FS_BASE, &[0xFEED_0000])
        .mem32(FS_BASE + 0x20, &[0, 0x1111_2222, 0x3333_4444])
        .expect(EAX, 0x3333_4444)
        .expect(ECX, 0x1111_2222)
        .expect(EDX, 0x3333_4444)
        .expect(EBP, STACK_TOP - 4)
        .expect(EBX, (DATA & 0xFFFF_0000) | 0x2244)
        .expect(EDI, 0x120)
        .expect(ESP, STACK_TOP)
        .expect_mem32(FS_BASE, &[0xFEED_0000])
        .expect_mem32(FS_BASE + 0x48, &[0x20]),
    );
    v.push(
        Snippet::new("segment registers read as the flat model's selectors", |c| {
            c.mov(eax, cs)?;
            c.mov(ecx, ds)?;
            c.mov(edx, fs)?;
            c.push(ss)?;
            c.pop(esi)?;
            c.mov(es, ax)?; // ignored
            c.mov(word_ptr(ebx), cs)
        })
        .reg(EAX, u32::MAX)
        .expect(EAX, 0x08)
        .expect(ECX, 0x10)
        .expect(EDX, 0x28)
        .expect(ESI, 0x10)
        .expect(ESP, STACK_TOP)
        .expect_mem(DATA, &[0x08, 0]),
    );
    v.push(
        Snippet::new("the adjusts and SALC, which 64-bit mode cannot encode", |c| {
            c.mov(eax, 0x0000_0325)?; // ah 3 al 0x25
            c.aad(10)?; // al = 0x25 + 30 = 0x43
            c.aam(10)?; // 67: ah 6 al 7
            c.mov(ecx, eax)?;
            c.mov(al, 0x19)?;
            c.add(al, 0x28)?; // 0x41, AF
            c.daa()?; // 0x47
            c.mov(dl, al)?;
            c.stc()?;
            c.db(&[0xD6])?; // salc: al = 0xFF
            c.mov(dh, al)?;
            c.xor(eax, eax)?;
            c.db(&[0xCE])?; // into: OF clear, nothing
            c.inc(eax)
        })
        .expect(ECX, 0x0607)
        .expect(EDX, 0xFF47)
        .expect(EAX, 1),
    );
    let fbits = |f: f64| f.to_le_bytes();
    v.push(
        Snippet::new("x87 arithmetic, the control word and stores", |c| {
            c.fld(dword_ptr(ebx))?; // 1.5
            c.fld(qword_ptr(ebx + 8))?; // 2.25
            c.faddp(st1, st0)?; // 3.75
            c.fild(dword_ptr(ebx + 16))?; // 4
            c.fmulp(st1, st0)?; // 15
            c.fld(st0)?;
            c.fsqrt()?;
            c.fstp(qword_ptr(ebx + 0x20))?;
            c.fist(dword_ptr(ebx + 0x28))?;
            c.fstp(qword_ptr(ebx + 0x30))?;
            c.fnstcw(word_ptr(ebx + 0x38))?;
            c.fnstsw(ax)
        })
        .mem(DATA, &1.5f32.to_le_bytes())
        .mem(DATA + 8, &fbits(2.25))
        .mem32(DATA + 16, &[4])
        .expect_mem(DATA + 0x20, &fbits(15f64.sqrt()))
        .expect_mem32(DATA + 0x28, &[15])
        .expect_mem(DATA + 0x30, &fbits(15.0))
        .expect_mem(DATA + 0x38, &[0x7F, 0x02]),
    );
    let ps = |x: [f32; 4]| -> Vec<u8> { x.iter().flat_map(|f| f.to_le_bytes()).collect() };
    v.push(
        Snippet::new("SSE and MMX, with MMX under an operand-size prefix", |c| {
            c.movups(xmm0, xmmword_ptr(ebx))?;
            c.movups(xmm1, xmmword_ptr(ebx + 0x10))?;
            c.mulps(xmm0, xmm1)?;
            c.addps(xmm0, xmm1)?;
            c.movups(xmmword_ptr(ebx + 0x20), xmm0)?;
            c.cvttss2si(eax, xmm0)?;
            c.movq(mm0, qword_ptr(ebx + 0x30))?;
            c.paddb(mm0, qword_ptr(ebx + 0x38))?;
            c.db(&[0x66, 0x0F, 0xDC, 0x43, 0x39])?; // 66 paddusb mm0, [ebx+0x39]: MMX on a Pentium III
            c.movq(qword_ptr(ebx + 0x48), mm0)?;
            c.emms()
        })
        .mem(DATA, &ps([1.0, 2.0, 3.0, 4.0]))
        .mem(DATA + 0x10, &ps([10.0, 20.0, 30.0, 40.0]))
        .mem(DATA + 0x30, &[1, 2, 3, 4, 5, 6, 7, 8])
        .mem(DATA + 0x38, &[0x10; 9])
        .expect(EAX, 20)
        .expect_mem(DATA + 0x20, &ps([20.0, 60.0, 120.0, 200.0]))
        .expect_mem(DATA + 0x48, &[0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28]),
    );
    v.push(
        Snippet::new("code that writes over an instruction ahead in its own block", |c| {
            // mov byte [imm of the mov below], 7; mov eax, 1 (7 once patched)
            c.db(&[0xC6, 0x05])?;
            c.db(&(CODE + 8).to_le_bytes())?;
            c.db(&[0x07, 0xB8, 0x01, 0x00, 0x00, 0x00])
        })
        .expect(EAX, 7),
    );
    v
}
