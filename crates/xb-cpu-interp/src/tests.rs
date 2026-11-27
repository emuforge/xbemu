//! The backend-neutral conformance cases run on the interpreter.

use std::sync::Arc;

use xb_cpu::Cpu;
use xb_mem::Ram;
use xb_types::mem::PhysMem;

use crate::InterpCpu;

/// Runs every contract case on a CPU that compiles a block to host code
/// once it has run `hot_after` times (where the host has a code generator);
/// 0 never compiles.
fn contract_cases(hot_after: u32) {
    let mut failed = Vec::new();
    for case in xb_cpu_kvm::testing::cases() {
        let ram = Ram::new(xb_cpu_kvm::testing::RAM_SIZE).unwrap();
        let mut cpu = InterpCpu::new(ram.clone());
        cpu.cache.hot_after = hot_after;
        let phys: Arc<dyn PhysMem> = Arc::new(ram);
        if let Err(e) = xb_cpu_kvm::testing::run_case(case, &mut cpu, phys) {
            failed.push(format!("{}: {e}", case.name));
        }
    }
    assert!(failed.is_empty(), "{} failed:\n{}", failed.len(), failed.join("\n"));
}

#[test]
fn every_contract_case_passes() {
    contract_cases(0);
}

#[test]
fn every_contract_case_passes_with_every_block_compiled() {
    contract_cases(1);
}

/// Runs every instruction snippet; see [`contract_cases`] for `hot_after`.
fn snippets(hot_after: u32) {
    use xb_cpu_x64::testing as t;
    let mut failed = Vec::new();
    for s in t::snippets() {
        let ram = Ram::new(t::RAM_SIZE).unwrap();
        let mut cpu = InterpCpu::new(ram.clone());
        cpu.cache.hot_after = hot_after;
        t::map_machine(&mut cpu);
        // The snippets work on a byte view of RAM: load into one, copy it
        // in, run, copy it back out and check.
        let mut view = vec![0u8; t::RAM_SIZE as usize];
        s.load(&mut cpu, &mut view);
        ram.write(0, &view);
        let mut ctx = s.context();
        let exit = cpu.run(&mut ctx, &mut t::NoBus, xb_cpu::RunFlags::default());
        ram.read(0, &mut view);
        if let Err(e) = s.check(&exit, &ctx, &view) {
            failed.push(e);
        }
    }
    assert!(failed.is_empty(), "{} failed:\n{}", failed.len(), failed.join("\n"));
}

#[test]
fn every_instruction_snippet_passes() {
    snippets(0);
}

#[test]
fn every_instruction_snippet_passes_with_every_block_compiled() {
    snippets(1);
}

mod compiled {
    //! Host code against the interpreter, one instruction at a time.

    use xb_cpu::{Access, Context, Cpu, Exit, Mapper, RunFlags};
    use xb_mem::Ram;

    use crate::InterpCpu;
    use crate::fast::tests::{FORMS, random_fx};

    const CODE: u32 = 0x0001_0000;
    /// Where the instruction under test sits: near the end of its page, so
    /// little runs after it.
    const AT: u32 = CODE + 0xFC0;
    const DATA: u32 = 0x0010_0000;
    const STACK: u32 = 0x0020_0000;
    /// A page the guest may only read.
    const READ_ONLY: u32 = 0x0040_0000;
    /// Nothing is mapped here.
    const UNMAPPED: u32 = 0x0030_0000;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u32 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 33) as u32
        }
    }

    struct State {
        gpr: [u32; 8],
        eflags: u32,
        data: Vec<u8>,
        stack: Vec<u8>,
        fx: [u8; 512],
    }

    type Outcome = (Exit, [u32; 8], u32, u32, Vec<u8>, Vec<u8>, Vec<u8>);

    /// Runs the instruction `bytes` from `st` until the CPU leaves the code
    /// page: the page is filled with NOPs, with a jump into the trap range
    /// (everything above the page) at its end and before the instruction;
    /// everything below the page is unmapped. `hot_after` is 0 to interpret
    /// only.
    fn run(bytes: &[u8], st: &State, hot_after: u32) -> (Outcome, Option<u64>) {
        let ram = Ram::new(8 << 20).unwrap();
        let mut page = vec![0x90u8; 0x1000];
        let at = (AT - CODE) as usize;
        page[at..at + bytes.len()].copy_from_slice(bytes);
        // mov edx, 0x11111111 right after: a jump over it (the forms'
        // short jumps go 5 bytes ahead) leaves EDX as it was, so taken and
        // not taken end differently.
        let after = at + bytes.len();
        page[after] = 0xBA;
        page[after + 1..after + 5].copy_from_slice(&0x1111_1111u32.to_le_bytes());
        // jmp 0x20000 as the page's last instruction, and another just
        // before the instruction under test: a backward jump must not slide
        // back into it and loop.
        for end in [0x1000usize, at] {
            page[end - 5] = 0xE9;
            let rel = 0x2_0000 - (CODE + end as u32);
            page[end - 4..end].copy_from_slice(&rel.to_le_bytes());
        }
        ram.write(CODE, &page);
        ram.write(DATA, &st.data);
        ram.write(STACK, &st.stack);
        ram.write(READ_ONLY, &st.data);
        let mut cpu = InterpCpu::new(ram.clone());
        cpu.cache.hot_after = hot_after;
        cpu.map(CODE, CODE, 0x1000, Access::ReadWrite);
        cpu.map(DATA, DATA, 0x1000, Access::ReadWrite);
        cpu.map(STACK, STACK, 0x2000, Access::ReadWrite);
        cpu.map(READ_ONLY, READ_ONLY, 0x1000, Access::Read);
        cpu.set_trap_range(CODE + 0x1000, 0xFFFE_F000);
        let mut ctx = Context::default();
        ctx.regs.gpr = st.gpr;
        ctx.regs.eip = AT;
        ctx.regs.eflags = st.eflags;
        ctx.regs.fs_base = DATA;
        ctx.fx.0 = st.fx;
        let exit = cpu.run(&mut ctx, &mut xb_cpu_x64::testing::NoBus, RunFlags::default());
        let outcome = (
            exit,
            ctx.regs.gpr,
            ctx.regs.eip,
            ctx.regs.eflags,
            ram.read_vec(DATA, 0x1000).unwrap(),
            ram.read_vec(STACK, 0x2000).unwrap(),
            ctx.fx.0.to_vec(),
        );
        (outcome, cpu.jit.native_runs())
    }

    /// A compare, test or arithmetic instruction and a conditional jump on
    /// its flags, which host code tests without the interpreter: every
    /// condition, with operands at the boundaries of each flag.
    #[test]
    fn a_jump_on_the_flags_of_the_instruction_before_goes_where_the_interpreter_goes() {
        let mut rng = Rng(0xF1A6);
        const EDGES: [u32; 7] = [0, 1, 2, 0x7FFF_FFFF, 0x8000_0000, 0x8000_0001, 0xFFFF_FFFF];
        // cmp eax,ecx; test eax,ecx; sub eax,ecx; and eax,ecx; add eax,ecx;
        // cmp eax,5; cmp eax,[ebx]
        let firsts: [&[u8]; 7] = [
            &[0x3B, 0xC1],
            &[0x85, 0xC1],
            &[0x2B, 0xC1],
            &[0x23, 0xC1],
            &[0x03, 0xC1],
            &[0x83, 0xF8, 0x05],
            &[0x3B, 0x03],
        ];
        for first in firsts {
            for cc in 0..16u8 {
                let mut bytes = first.to_vec();
                bytes.extend_from_slice(&[0x70 + cc, 0x05]);
                for round in 0..60 {
                    let mut gpr: [u32; 8] = std::array::from_fn(|_| rng.next());
                    let pick = |r: u32| EDGES[(r % 7) as usize];
                    if round % 3 != 0 {
                        gpr[0] = pick(rng.next());
                        gpr[1] = pick(rng.next());
                    }
                    if round % 5 == 0 {
                        gpr[1] = gpr[0];
                    }
                    gpr[3] = DATA + (rng.next() & 0x7F0);
                    gpr[4] = STACK + 0x1000;
                    let mut data: Vec<u8> = (0..0x1000).map(|_| rng.next() as u8).collect();
                    if round % 4 == 0 {
                        // [ebx] equal to eax, for the memory compare.
                        let at = (gpr[3] - DATA) as usize;
                        data[at..at + 4].copy_from_slice(&gpr[0].to_le_bytes());
                    }
                    let st = State {
                        gpr,
                        eflags: 0x2 | (rng.next() & 0x8D5),
                        data,
                        stack: vec![0; 0x2000],
                        fx: xb_cpu::FxState::new().0,
                    };
                    let (want, _) = run(&bytes, &st, 0);
                    let (got, _) = run(&bytes, &st, 1);
                    assert!(
                        want == got,
                        "{bytes:02x?} with eax {:#x} ecx {:#x}: interpreted ends at {:#x} flags {:#x}, compiled at {:#x} flags {:#x}",
                        gpr[0],
                        gpr[1],
                        want.2,
                        want.3,
                        got.2,
                        got.3
                    );
                }
            }
        }
    }

    #[test]
    fn host_code_leaves_what_the_interpreter_leaves() {
        let mut rng = Rng(0xC0DE);
        let mut native = Some(0);
        for bytes in FORMS {
            for round in 0..120 {
                let mut gpr: [u32; 8] = std::array::from_fn(|_| rng.next());
                gpr[1] &= 0xFF;
                // Mostly good pointers; some rounds fault on a load, a
                // store or the stack.
                gpr[3] = match round % 12 {
                    // The last bytes of the page: an access that crosses
                    // into the next one, which is not mapped.
                    7 => DATA + 0xFFD + (rng.next() & 3),
                    8 => DATA + 0xFF0 + (rng.next() & 0xC),
                    9 => UNMAPPED + (rng.next() & 0x7F0),
                    10 => READ_ONLY + (rng.next() & 0x7F0),
                    _ => DATA + (rng.next() & 0x7F0),
                };
                gpr[4] = match round % 12 {
                    11 => UNMAPPED + 0x800,
                    _ => STACK + 0x1000 + (rng.next() & 0x3FC),
                };
                let st = State {
                    gpr,
                    eflags: 0x2 | (rng.next() & 0x8D5),
                    data: (0..0x1000).map(|_| rng.next() as u8).collect(),
                    stack: (0..0x2000).map(|_| rng.next() as u8).collect(),
                    fx: random_fx(&mut || rng.next()),
                };
                let (want, _) = run(bytes, &st, 0);
                let (got, runs) = run(bytes, &st, 1);
                assert!(
                    want == got,
                    "{bytes:02x?} from {gpr:08x?} flags {:#x}:\n interpreted {:?} {:08x?} eip {:#x} flags {:#x}\n compiled    {:?} {:08x?} eip {:#x} flags {:#x}",
                    st.eflags,
                    want.0,
                    want.1,
                    want.2,
                    want.3,
                    got.0,
                    got.1,
                    got.2,
                    got.3
                );
                native = native.zip(runs).map(|(n, r)| n + r);
            }
        }
        match native {
            Some(n) => assert!(n > 0, "no block ran as host code"),
            None => println!("note: this host has no code generator; both runs were interpreted"),
        }
    }
}
