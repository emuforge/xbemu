//! The KVM backend proper: VM setup and the run loop.
//!
//! See `docs/hw/cpu-kvm.md` for the design. In short: the guest runs in ring 0
//! of a 32-bit protected-mode VM whose page tables, descriptor tables and
//! stub code live in a hidden memory slot ([`tables`]). Kernel entry points
//! and exception vectors all lead to a `HLT`, which KVM hands to user space
//! when the VM has no in-kernel interrupt controller; the address of the
//! `HLT` says which event it was. Registers travel in the shared `kvm_run`
//! block (`KVM_CAP_SYNC_REGS`), so a kernel call costs one `KVM_RUN` plus
//! one `KVM_GET_FPU`.

mod census;
mod decode;
mod kick;
mod ram;
mod tables;

use std::os::fd::{AsRawFd, BorrowedFd};
use std::sync::Arc;

use kvm_bindings::{
    CpuId, KVM_CAP_X86_USER_SPACE_MSR, KVM_EXIT_FAIL_ENTRY, KVM_EXIT_HLT, KVM_EXIT_INTERNAL_ERROR,
    KVM_EXIT_INTR, KVM_EXIT_IO, KVM_EXIT_IO_OUT, KVM_EXIT_IRQ_WINDOW_OPEN, KVM_EXIT_MEMORY_FAULT,
    KVM_EXIT_MMIO, KVM_EXIT_SHUTDOWN, KVM_EXIT_UNKNOWN, KVM_EXIT_X86_RDMSR, KVM_EXIT_X86_WRMSR,
    KVM_MP_STATE_RUNNABLE, KVM_MSR_EXIT_REASON_INVAL, KVM_MSR_EXIT_REASON_UNKNOWN,
    KVM_SYNC_X86_REGS, kvm_cpuid_entry2, kvm_enable_cap, kvm_fpu, kvm_mp_state, kvm_regs, kvm_run,
    kvm_segment, kvm_sregs, kvm_userspace_memory_region, kvm_vcpu_events,
};
use kvm_ioctls::{Cap, Kvm, VcpuFd, VmFd};
use xb_cpu::{Bus, Context, Cpu, Exit, FaultAction, FxState, Kicker, Regs, RunFlags, eflags, reg};
use xb_types::mem::{Access, Mapper, PageProtector};

use self::census::{Census, Why};
pub use self::kick::KICK_SIGNAL_OFFSET;
use self::kick::KickState;
use self::ram::{Mmap, PAGE, Protector};
use self::tables::{
    CR3, DF_STUB_VA, FLUSH_STUB_END, FLUSH_STUB_VA, GDT_LIMIT, GDT_VA, IDT_VA, SEL_CODE, SEL_DATA,
    SEL_FS, SEL_TSS, STUB_VA, TSS_VA, Tables, Target,
};
pub use self::tables::{HIDDEN_GPA, HIDDEN_LEN, SYSTEM_VA, SYSTEM_VA_LEN};

/// The console's time-stamp counter rate in kHz (733.333 MHz; the reference
/// scales `RDTSC` to the same figure).
const TSC_KHZ: u32 = 733_333;

/// `KVM_RUN`: `_IO(KVMIO, 0x80)`.
const KVM_RUN: libc::c_ulong = 0xAE80;

/// CR0: protected mode, paging, native x87 errors, and write protection in
/// ring 0 so read-only mappings hold for the title.
const CR0: u64 = 0x8001_0033;
/// CR4: `OSFXSR` and `OSXMMEXCPT`, which SSE needs.
const CR4: u64 = 0x0000_0600;

/// EFLAGS resume flag: set in the image the processor pushes for a fault,
/// clear for a trap or `INT n`.
const EFLAGS_RF: u32 = 1 << 16;
/// EFLAGS bits a context may set (everything but VM, VIF, VIP and reserved).
const EFLAGS_LOAD_MASK: u32 = 0x0025_7FD5;

/// Where KVM may put its real-mode TSS pages on Intel hosts: inside the
/// console's flash ROM window, which no title maps.
const KVM_TSS_GPA: usize = 0xFFFB_D000;

/// Guest-virtual base of the mapping of all physical RAM.
const CONTIG_WINDOW: u32 = 0x8000_0000;

/// Consecutive unexplained `EFAULT`s after which the backend gives up.
const MAX_SPURIOUS_FAULTS: u32 = 64;

/// The guest processor on KVM.
pub struct KvmCpu {
    // Field order is drop order: the vCPU and VM go before the memory their
    // slots point at.
    vcpu: VcpuFd,
    vm: VmFd,
    /// Memory slots in use (0 RAM, 1 hidden, then read-through ranges).
    slots: u32,
    _kvm: Kvm,
    /// The vCPU's shared `kvm_run` block. Accessed through this pointer only
    /// (never through a reference), because the kick signal handler writes
    /// `immediate_exit` asynchronously.
    run: *mut kvm_run,
    tables: Tables,
    host_view: Mmap,
    protector: Arc<Protector>,
    kick: Arc<KickState>,
    ram_size: u32,
    /// Segment and control registers of the guest environment.
    base_sregs: kvm_sregs,
    /// FS base currently loaded in the vCPU.
    fs_base: u32,
    /// The FPU state the vCPU holds, if known: lets `run` skip `KVM_SET_FPU`
    /// when the caller passes the same thread again.
    fx_loaded: Option<FxState>,
    /// The vCPU's system state was lost (double fault, shutdown) and is
    /// rebuilt before the next run.
    needs_reset: bool,
    spurious_faults: u32,
    census: Census,
}

// SAFETY: `run` points into a mapping owned by `vcpu`; the value is used from
// one thread at a time (`&mut self`), and the only concurrent writer is the
// signal handler on that same thread.
unsafe impl Send for KvmCpu {}

impl KvmCpu {
    /// Checks whether KVM can run the guest on this host: `/dev/kvm` opens
    /// read-write, speaks API version 12 and has the capabilities the backend
    /// relies on.
    ///
    /// # Errors
    ///
    /// The reason KVM cannot be used, for the log.
    pub fn probe() -> Result<(), String> {
        open_kvm().map(drop)
    }

    /// Creates the VM and its one vCPU. `ram_fd` is the shared memory object
    /// holding guest RAM (`ram_size` bytes, page aligned); the backend maps
    /// its own views of it.
    ///
    /// # Errors
    ///
    /// A description of the KVM or mapping failure.
    pub fn new(ram_fd: BorrowedFd<'_>, ram_size: u32) -> Result<Self, String> {
        if ram_size == 0 || !ram_size.is_multiple_of(PAGE) || ram_size > HIDDEN_GPA {
            return Err(format!("unsupported guest RAM size {ram_size:#x}"));
        }
        let kvm = open_kvm()?;
        let vm = kvm.create_vm().map_err(|e| format!("KVM_CREATE_VM: {e}"))?;
        // Needed by Intel hosts without unrestricted-guest support; harmless
        // elsewhere.
        if let Err(e) = vm.set_tss_address(KVM_TSS_GPA) {
            log::debug!(target: "cpu", "KVM_SET_TSS_ADDR: {e}");
        }

        let host_view = Mmap::shared(ram_fd, ram_size as usize)?;
        let protector = Arc::new(Protector::new(Mmap::shared(ram_fd, ram_size as usize)?));
        let tables = Tables::new(ram_size)?;
        let (hidden_ptr, hidden_len) = tables.slot();
        let slots = [
            (0u32, 0u64, protector.slot_ptr(), ram_size as usize),
            (1, u64::from(HIDDEN_GPA), hidden_ptr, hidden_len),
        ];
        for (slot, gpa, ptr, len) in slots {
            let region = kvm_userspace_memory_region {
                slot,
                flags: 0,
                guest_phys_addr: gpa,
                memory_size: len as u64,
                userspace_addr: ptr as u64,
            };
            // SAFETY: the region is a live mapping of `len` bytes that this
            // value keeps mapped until after the VM is dropped (field order).
            unsafe { vm.set_user_memory_region(region) }
                .map_err(|e| format!("KVM_SET_USER_MEMORY_REGION (slot {slot}): {e}"))?;
        }

        let mut vcpu = vm.create_vcpu(0).map_err(|e| format!("KVM_CREATE_VCPU: {e}"))?;
        // Before the CPUID table: KVM sets the table's APIC feature bit from
        // the APIC base register, and the console's processor reports none.
        let mut base_sregs = vcpu.get_sregs().map_err(|e| format!("KVM_GET_SREGS: {e}"))?;
        environment(&mut base_sregs);
        vcpu.set_sregs(&base_sregs).map_err(|e| format!("KVM_SET_SREGS: {e}"))?;
        let entries: Vec<kvm_cpuid_entry2> = (0..=2)
            .map(|leaf| {
                let [eax, ebx, ecx, edx] = xb_cpu::cpuid(leaf);
                kvm_cpuid_entry2 { function: leaf, eax, ebx, ecx, edx, ..Default::default() }
            })
            .collect();
        let cpuid = CpuId::from_entries(&entries).map_err(|e| format!("CPUID table: {e:?}"))?;
        vcpu.set_cpuid2(&cpuid).map_err(|e| format!("KVM_SET_CPUID2: {e}"))?;

        let tsc = if kvm.check_extension(Cap::TscControl) {
            vcpu.set_tsc_khz(TSC_KHZ).map_err(|e| e.to_string())
        } else {
            Err("no KVM_CAP_TSC_CONTROL".to_owned())
        };
        if let Err(e) = &tsc {
            log::warn!(
                target: "cpu",
                "KVM cannot scale the TSC to 733 MHz ({e}); RDTSC runs at the host's rate"
            );
        }
        let user_msrs = kvm.check_extension(Cap::X86UserSpaceMsr)
            && vm
                .enable_cap(&kvm_enable_cap {
                    cap: KVM_CAP_X86_USER_SPACE_MSR,
                    args: [
                        u64::from(KVM_MSR_EXIT_REASON_UNKNOWN | KVM_MSR_EXIT_REASON_INVAL),
                        0,
                        0,
                        0,
                    ],
                    ..Default::default()
                })
                .is_ok();
        if !user_msrs {
            log::warn!(
                target: "cpu",
                "KVM cannot forward unknown MSRs; the guest takes #GP for them"
            );
        }

        let run: *mut kvm_run = vcpu.get_kvm_run();
        // SAFETY: `run` is the vCPU's mapped run block; no reference to it is
        // live.
        unsafe { (*run).kvm_valid_regs = u64::from(KVM_SYNC_X86_REGS) };

        kick::install_handler();
        log::info!(
            target: "cpu",
            "KVM backend: {} MiB RAM, TSC {}, unknown MSRs {}",
            ram_size >> 20,
            if tsc.is_ok() { "scaled to 733 MHz" } else { "at host rate" },
            if user_msrs { "forwarded" } else { "fault" },
        );
        Ok(Self {
            vcpu,
            vm,
            slots: 2,
            _kvm: kvm,
            run,
            tables,
            host_view,
            protector,
            kick: Arc::new(KickState::new()),
            ram_size,
            base_sregs,
            fs_base: 0,
            fx_loaded: None,
            needs_reset: false,
            spurious_faults: 0,
            census: Census::new(),
        })
    }

    /// Brings the vCPU's system state in line with what `ctx` needs.
    fn prepare(&mut self, ctx: &Context) -> Result<(), String> {
        if self.needs_reset {
            self.tables.write_system_page(ctx.regs.fs_base);
            self.fs_base = ctx.regs.fs_base;
            self.base_sregs.fs.base = u64::from(self.fs_base);
            self.vcpu
                .set_mp_state(kvm_mp_state { mp_state: KVM_MP_STATE_RUNNABLE })
                .and_then(|()| self.vcpu.set_vcpu_events(&kvm_vcpu_events::default()))
                .and_then(|()| self.vcpu.set_sregs(&self.base_sregs))
                .map_err(|e| format!("resetting the vCPU: {e}"))?;
            self.fx_loaded = None;
            self.needs_reset = false;
            self.flush_tlb()?;
            self.tables.take_stale_tlb();
        }
        if self.tables.take_stale_tlb() {
            self.flush_tlb()?;
        }
        if ctx.regs.fs_base != self.fs_base {
            // Read the registers back rather than reuse `base_sregs`, so
            // anything the title changed itself survives.
            let mut sregs = self.vcpu.get_sregs().map_err(|e| format!("KVM_GET_SREGS: {e}"))?;
            sregs.fs.base = u64::from(ctx.regs.fs_base);
            self.vcpu.set_sregs(&sregs).map_err(|e| format!("KVM_SET_SREGS: {e}"))?;
            census::FS_SETS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.tables.set_fs_descriptor(ctx.regs.fs_base);
            self.fs_base = ctx.regs.fs_base;
            self.base_sregs.fs.base = sregs.fs.base;
        }
        if self.fx_loaded.as_ref().is_none_or(|l| l.0 != ctx.fx.0) {
            self.vcpu.set_fpu(&fx_to_kvm(&ctx.fx)).map_err(|e| format!("KVM_SET_FPU: {e}"))?;
            census::FPU_SETS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.fx_loaded = Some(ctx.fx);
        }
        Ok(())
    }

    /// Makes the guest reload CR3, which drops every cached translation.
    ///
    /// KVM has no call that flushes a guest's TLB, and with nested paging the
    /// processor caches guest translations across exits; so the guest runs
    /// the two-instruction flush stub.
    fn flush_tlb(&mut self) -> Result<(), String> {
        let regs = kvm_regs {
            rax: u64::from(CR3),
            rip: u64::from(FLUSH_STUB_VA),
            rflags: u64::from(eflags::FIXED),
            ..Default::default()
        };
        self.set_kregs(regs);
        self.set_interrupt_window(false);
        loop {
            match self.kvm_run() {
                Ok(())
                    if self.exit_reason() == KVM_EXIT_HLT
                        && self.kregs().rip == u64::from(FLUSH_STUB_END) =>
                {
                    self.census.why(Why::Flush, false);
                    return Ok(());
                }
                // A kick: it stays pending and is honoured by the caller's
                // loop; finish the flush first.
                Err(libc::EINTR | libc::EAGAIN) => self.clear_immediate_exit(),
                Ok(()) if self.exit_reason() == KVM_EXIT_INTR => self.clear_immediate_exit(),
                other => {
                    return Err(format!(
                        "TLB flush stub ended with {other:?}, exit reason {}, eip {:#010x}",
                        self.exit_reason(),
                        self.kregs().rip
                    ));
                }
            }
        }
    }

    /// Runs the guest until an exit the caller must see.
    fn run_guest(&mut self, ctx: &mut Context, bus: &mut dyn Bus, flags: RunFlags) -> Exit {
        if self.kick.take() {
            self.census.why(Why::KickPending, true);
            return Exit::Kicked;
        }
        if flags.exit_when_interruptible && ctx.regs.eflags & eflags::IF != 0 {
            self.census.why(Why::Interruptible, true);
            return Exit::Kicked;
        }
        if let Err(e) = self.prepare(ctx) {
            return Exit::Fatal(e);
        }
        self.set_kregs(regs_to_kvm(&ctx.regs));
        self.set_interrupt_window(flags.exit_when_interruptible);
        self.spurious_faults = 0;

        let exit = loop {
            if let Some(exit) = self.step(bus) {
                break exit;
            }
        };

        if self.needs_reset
            && matches!(exit, Exit::Fatal(_))
            && self.exit_reason() == KVM_EXIT_SHUTDOWN
        {
            // The processor state is gone; `ctx` keeps what it had on entry.
            return exit;
        }
        regs_from_kvm(&self.kregs(), &mut ctx.regs);
        census::FPU_GETS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        match self.vcpu.get_fpu() {
            Ok(fpu) => {
                fx_from_kvm(&fpu, &mut ctx.fx);
                self.fx_loaded = Some(ctx.fx);
            }
            Err(e) => {
                self.fx_loaded = None;
                return Exit::Fatal(format!("KVM_GET_FPU: {e}"));
            }
        }
        exit
    }

    /// One `KVM_RUN`. `None` means the event was handled and the guest
    /// continues.
    fn step(&mut self, bus: &mut dyn Bus) -> Option<Exit> {
        match self.kvm_run() {
            Ok(()) => {}
            Err(libc::EINTR | libc::EAGAIN) => return self.interrupted(bus),
            Err(libc::EFAULT) => return self.memory_fault(bus),
            Err(e) => {
                return Some(Exit::Fatal(format!(
                    "KVM_RUN failed: {}",
                    std::io::Error::from_raw_os_error(e)
                )));
            }
        }
        self.spurious_faults = 0;
        match self.exit_reason() {
            KVM_EXIT_HLT => {
                let at = (self.kregs().rip as u32).wrapping_sub(1);
                if self.tables.in_trap_range(at) {
                    let mut r = self.kregs();
                    r.rip = u64::from(at);
                    self.set_kregs(r);
                    self.census.why(Why::Trap, true);
                    self.census.trap(at);
                    Some(Exit::Trap)
                } else if at.wrapping_sub(STUB_VA) < 256 {
                    self.exception(at.wrapping_sub(STUB_VA) as u8, bus)
                } else if at == DF_STUB_VA {
                    Some(self.double_fault())
                } else {
                    self.census.why(Why::Halt, true);
                    Some(Exit::Halt)
                }
            }
            KVM_EXIT_MMIO => self.mmio(bus),
            KVM_EXIT_IO => {
                self.census.why(Why::Pio, false);
                self.port_io(bus);
                None
            }
            KVM_EXIT_IRQ_WINDOW_OPEN => {
                self.census.why(Why::IrqWindow, true);
                Some(Exit::Kicked)
            }
            KVM_EXIT_INTR => self.interrupted(bus),
            KVM_EXIT_X86_RDMSR => {
                self.census.why(Why::Msr, false);
                // SAFETY: the exit reason says the `msr` member is the live
                // one; `run` is valid and unreferenced.
                unsafe {
                    let msr = &raw mut (*self.run).__bindgen_anon_1.msr;
                    (*msr).data = bus.rdmsr((*msr).index);
                    (*msr).error = 0;
                }
                None
            }
            KVM_EXIT_X86_WRMSR => {
                self.census.why(Why::Msr, false);
                // SAFETY: as for RDMSR.
                unsafe {
                    let msr = &raw mut (*self.run).__bindgen_anon_1.msr;
                    bus.wrmsr((*msr).index, (*msr).data);
                    (*msr).error = 0;
                }
                None
            }
            KVM_EXIT_SHUTDOWN => {
                // A triple fault. On AMD hosts KVM has already reset the
                // vCPU, so the registers are not reported.
                self.needs_reset = true;
                Some(Exit::Fatal(
                    "guest shut down (triple fault): an exception could not be delivered even \
                     through the double-fault task"
                        .to_owned(),
                ))
            }
            KVM_EXIT_INTERNAL_ERROR => self.emulation_failure(bus),
            KVM_EXIT_FAIL_ENTRY => {
                // SAFETY: the exit reason selects the `fail_entry` member.
                let reason = unsafe {
                    (*self.run).__bindgen_anon_1.fail_entry.hardware_entry_failure_reason
                };
                self.needs_reset = true;
                Some(Exit::Fatal(format!("KVM could not enter the guest (reason {reason:#x})")))
            }
            other => Some(Exit::Fatal(format!(
                "unexpected KVM exit reason {other} at eip {:#010x}",
                self.kregs().rip
            ))),
        }
    }

    /// `KVM_RUN` was interrupted by a signal: a kick, or something unrelated.
    fn interrupted(&mut self, bus: &mut dyn Bus) -> Option<Exit> {
        self.clear_immediate_exit();
        let kicked = self.kick.take();
        self.census.why(if kicked { Why::KickSignal } else { Why::SignalNoKick }, kicked);
        // The signal may have landed after the processor delivered an
        // exception but before the stub's `HLT` ran. The guest is then not at
        // a state the caller may see; finish the exception first.
        let rip = self.kregs().rip as u32;
        let pending = if rip.wrapping_sub(STUB_VA) < 256 {
            Some(self.exception(rip.wrapping_sub(STUB_VA) as u8, bus))
        } else if rip == DF_STUB_VA {
            Some(Some(self.double_fault()))
        } else {
            None
        };
        match pending {
            Some(Some(exit)) => {
                if kicked {
                    self.kick.restore();
                }
                Some(exit)
            }
            // A resolved page fault leaves the guest at the faulting
            // instruction, which is a boundary.
            Some(None) | None => kicked.then_some(Exit::Kicked),
        }
    }

    /// The guest reached exception stub `vector`: undo the delivery and
    /// decide what the event was.
    fn exception(&mut self, vector: u8, bus: &mut dyn Bus) -> Option<Exit> {
        let mut r = self.kregs();
        let esp = r.rsp as u32;
        let mut raw = [0u8; 16];
        if !self.read_guest(esp, &mut raw) {
            return Some(Exit::Fatal(format!(
                "exception {vector}: its frame at esp {esp:#010x} is not in guest RAM"
            )));
        }
        let word = |i: usize| {
            u32::from_le_bytes([raw[i * 4], raw[i * 4 + 1], raw[i * 4 + 2], raw[i * 4 + 3]])
        };
        // With an error code the frame is [code, eip, cs, eflags], without it
        // [eip, cs, eflags]. Bit 1 of EFLAGS is always set and bit 1 of a
        // ring-0 code selector never is, which tells the third word apart.
        // `INT n` to an error-code vector pushes none.
        let has_code = matches!(vector, 8 | 10..=14 | 17 | 21 | 29 | 30) && word(2) & 2 == 0;
        let (code, eip, flags, size) =
            if has_code { (word(0), word(1), word(3), 16) } else { (0, word(0), word(2), 12) };
        r.rip = u64::from(eip);
        r.rsp = u64::from(esp.wrapping_add(size));
        r.rflags = u64::from(flags & !EFLAGS_RF);
        self.set_kregs(r);

        let fault = has_code || flags & EFLAGS_RF != 0;
        if vector >= 32 || (!fault && !matches!(vector, 1 | 3 | 4)) {
            self.census.why(Why::SoftInt, true);
            self.census.vector(true, vector);
            return Some(Exit::SoftwareInterrupt(vector));
        }
        if vector != 14 || !has_code {
            self.census.why(Why::Exception, true);
            self.census.vector(false, vector);
            return Some(Exit::Exception { vector, error_code: code, address: 0 });
        }
        let address = match self.vcpu.get_sregs() {
            Ok(s) => s.cr2 as u32,
            Err(e) => return Some(Exit::Fatal(format!("KVM_GET_SREGS: {e}"))),
        };
        let pa = match self.tables.lookup(address) {
            Target::Ram { pa, .. } => Some(pa | (address & 0xFFF)),
            Target::None | Target::Other => None,
        };
        match bus.page_fault(address, pa, code & 2 != 0) {
            FaultAction::Retry => {
                self.census.why(Why::GuestPageFault, false);
                None
            }
            FaultAction::Raise => {
                self.census.why(Why::Exception, true);
                self.census.vector(false, vector);
                Some(Exit::Exception { vector, error_code: code, address })
            }
        }
    }

    /// The guest entered the double-fault task: report where it died.
    fn double_fault(&mut self) -> Exit {
        let (gpr, eip, flags) = self.tables.double_fault_state();
        let cr2 = self.vcpu.get_sregs().map_or(0, |s| s.cr2 as u32);
        let mut regs = Regs { gpr, eip, eflags: flags & !EFLAGS_RF, fs_base: 0 };
        regs.eflags |= eflags::FIXED;
        self.set_kregs(regs_to_kvm(&regs));
        self.needs_reset = true;
        Exit::Fatal(format!(
            "double fault at eip {eip:#010x}: the guest stack (esp {:#010x}) cannot take an \
             exception frame; last page-fault address {cr2:#010x}",
            gpr[reg::ESP]
        ))
    }

    /// `KVM_RUN` failed with `EFAULT`: the guest touched RAM the protector
    /// closed. The instruction has not executed.
    fn memory_fault(&mut self, bus: &mut dyn Bus) -> Option<Exit> {
        let located = if self.exit_reason() == KVM_EXIT_MEMORY_FAULT {
            // SAFETY: the exit reason selects the `memory_fault` member.
            let gpa = unsafe { (*self.run).__bindgen_anon_1.memory_fault.gpa };
            if gpa >= u64::from(self.ram_size) {
                return Some(Exit::Fatal(format!(
                    "KVM memory fault at {gpa:#x}, outside guest RAM"
                )));
            }
            let pa = gpa as u32;
            let access = self.protector.access(pa);
            (access != Access::ReadWrite).then(|| (self.va_of(pa), pa, access))
        } else {
            self.locate_fault()
        };
        let Some((va, pa, access)) = located else {
            self.census.why(Why::EfaultUnexplained, false);
            return self.unexplained_fault(bus);
        };
        self.census.why(Why::Efault, false);
        self.census.efault_page(pa);
        self.spurious_faults = 0;
        self.report_fault(bus, va, pa, access)
    }

    /// Calls the bus for a write-tracking fault on the page holding `pa`.
    fn report_fault(
        &mut self,
        bus: &mut dyn Bus,
        va: u32,
        pa: u32,
        access: Access,
    ) -> Option<Exit> {
        // KVM reports the failure, not the kind of access. A fault on a
        // read-only page must be a write; on a closed page it is reported as
        // a read, and a write faults again if the handler opens the page for
        // reading only.
        let write = access == Access::Read;
        match bus.page_fault(va, Some(pa), write) {
            FaultAction::Retry => None,
            FaultAction::Raise => Some(Exit::Exception {
                vector: 14,
                error_code: 1 | (u32::from(write) << 1),
                address: va,
            }),
        }
    }

    /// Finds the protected page the instruction at EIP touches, from the
    /// instruction itself: `(va, pa, access)`.
    fn locate_fault(&self) -> Option<(u32, u32, Access)> {
        let r = self.kregs();
        let eip = r.rip as u32;
        let mut code = [0u8; 15];
        // Read what is readable; a short read leaves zeroes, which decode as
        // a harmless register-indirect operand.
        for n in (1..=15).rev() {
            if self.read_guest(eip, &mut code[..n]) {
                break;
            }
        }
        let gpr = [r.rax, r.rcx, r.rdx, r.rbx, r.rsp, r.rbp, r.rsi, r.rdi].map(|v| v as u32);
        for op in decode::operands(&code, eip, &gpr, self.fs_base) {
            let mut va = op.va;
            let end = op.va.wrapping_add(op.len.max(1) - 1);
            loop {
                if let Target::Ram { pa, .. } = self.tables.lookup(va) {
                    let access = self.protector.access(pa);
                    if access != Access::ReadWrite {
                        return Some((va, pa | (va & 0xFFF), access));
                    }
                }
                if va & !0xFFF == end & !0xFFF {
                    break;
                }
                va = (va & !0xFFF).wrapping_add(PAGE);
            }
        }
        None
    }

    /// An `EFAULT` no protected page explains.
    fn unexplained_fault(&mut self, bus: &mut dyn Bus) -> Option<Exit> {
        // Another thread may have re-opened the page between the fault and
        // now, or be in the middle of closing it: let it finish and retry.
        self.protector.settle();
        self.spurious_faults += 1;
        if self.spurious_faults == 1 {
            return None;
        }
        let restricted = self.protector.restricted();
        if restricted.is_empty() || self.spurious_faults > MAX_SPURIOUS_FAULTS {
            return Some(Exit::Fatal(format!(
                "KVM_RUN keeps failing with EFAULT at eip {:#010x} and no protected page explains it",
                self.kregs().rip
            )));
        }
        // The instruction touches a protected page through an operand the
        // decoder does not know. Report every protected page: more than was
        // touched, never less, so write tracking stays correct.
        log::debug!(
            target: "cpu",
            "cannot locate the protected page touched at eip {:#010x}; reporting all {} of them",
            self.kregs().rip,
            restricted.len()
        );
        for (pa, access) in restricted {
            let exit = self.report_fault(bus, self.va_of(pa), pa, access);
            if exit.is_some() {
                return exit;
            }
        }
        None
    }

    /// A virtual address for physical RAM address `pa`: the mapping made
    /// last, else the contiguous window every title has
    /// (`docs/architecture.md`).
    fn va_of(&self, pa: u32) -> u32 {
        self.tables.va_of(pa).unwrap_or(CONTIG_WINDOW | pa)
    }

    /// A device register access, or an access by KVM's instruction emulator
    /// to RAM it could not reach through the protected slot view.
    fn mmio(&mut self, bus: &mut dyn Bus) -> Option<Exit> {
        // SAFETY: the exit reason selects the `mmio` member; `run` is valid
        // and unreferenced.
        let m = unsafe { (*self.run).__bindgen_anon_1.mmio };
        let len = (m.len as usize).min(8);
        let write = m.is_write != 0;
        let mut data = m.data;
        if m.phys_addr + len as u64 <= u64::from(self.ram_size) {
            self.census.why(Why::MmioRam, false);
            let pa = m.phys_addr as u32;
            let allowed = match self.protector.access(pa) {
                Access::ReadWrite => true,
                Access::Read => !write,
                Access::None => false,
            };
            if !allowed && bus.page_fault(self.va_of(pa), Some(pa), write) == FaultAction::Raise {
                return Some(Exit::Fatal(format!(
                    "an emulated instruction at eip {:#010x} touched protected RAM at \
                     {pa:#010x} and the fault handler refused it",
                    self.kregs().rip
                )));
            }
            // SAFETY: the range is inside the host view (checked above),
            // which is never protected; guest RAM is shared plain bytes.
            unsafe {
                let p = self.host_view.as_ptr().add(pa as usize);
                if write {
                    std::ptr::copy_nonoverlapping(data.as_ptr(), p, len);
                } else {
                    std::ptr::copy_nonoverlapping(p, data.as_mut_ptr(), len);
                }
            }
        } else {
            // The bus speaks 1, 2 and 4 bytes; an 8-byte access (MOVQ) is
            // two dwords.
            self.census.why(Why::Mmio, false);
            self.census.mmio(m.phys_addr as u32, write);
            let mut off = 0;
            while off < len {
                let n = match len - off {
                    4.. => 4,
                    2..=3 => 2,
                    _ => 1,
                };
                let pa = (m.phys_addr + off as u64) as u32;
                if write {
                    let mut v = [0u8; 4];
                    v[..n].copy_from_slice(&data[off..off + n]);
                    bus.mmio_write(pa, n as u8, u32::from_le_bytes(v));
                } else {
                    let v = bus.mmio_read(pa, n as u8).to_le_bytes();
                    data[off..off + n].copy_from_slice(&v[..n]);
                }
                off += n;
            }
        }
        if !write {
            // SAFETY: as above; KVM takes the read data from here when the
            // guest is re-entered.
            unsafe { (*self.run).__bindgen_anon_1.mmio.data = data };
        }
        None
    }

    /// `IN`/`OUT`, including the string forms (`count` items in one exit).
    fn port_io(&mut self, bus: &mut dyn Bus) {
        // SAFETY: the exit reason selects the `io` member.
        let io = unsafe { (*self.run).__bindgen_anon_1.io };
        let size = usize::from(io.size).clamp(1, 4);
        self.census.pio(io.port, u32::from(io.direction) == KVM_EXIT_IO_OUT);
        for i in 0..io.count as usize {
            // SAFETY: KVM places `count * size` bytes of port data inside the
            // run mapping at `data_offset`.
            let p = unsafe { self.run.cast::<u8>().add(io.data_offset as usize + i * size) };
            let mut v = [0u8; 4];
            if u32::from(io.direction) == KVM_EXIT_IO_OUT {
                // SAFETY: `p` addresses `size` readable bytes (see above).
                unsafe { std::ptr::copy_nonoverlapping(p, v.as_mut_ptr(), size) };
                bus.io_write(io.port, size as u8, u32::from_le_bytes(v));
            } else {
                v = bus.io_read(io.port, size as u8).to_le_bytes();
                // SAFETY: `p` addresses `size` writable bytes (see above).
                unsafe { std::ptr::copy_nonoverlapping(v.as_ptr(), p, size) };
            }
        }
    }

    /// KVM's emulator gave up on an instruction.
    fn emulation_failure(&mut self, bus: &mut dyn Bus) -> Option<Exit> {
        let eip = self.kregs().rip as u32;
        // The emulator cannot fetch code from a page the protector closed.
        if let Target::Ram { pa, .. } = self.tables.lookup(eip)
            && self.protector.access(pa) == Access::None
            && bus.page_fault(eip, Some(pa | (eip & 0xFFF)), false) == FaultAction::Retry
        {
            return None;
        }
        let mut code = [0u8; 16];
        let bytes = if self.read_guest(eip, &mut code) {
            code.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
        } else {
            "unreadable".to_owned()
        };
        // SAFETY: the exit reason selects the `internal` member.
        let sub = unsafe { (*self.run).__bindgen_anon_1.internal.suberror };
        Some(Exit::Fatal(format!(
            "KVM internal error {sub} (instruction emulation) at eip {eip:#010x}, code: {bytes}"
        )))
    }

    /// Copies guest memory at virtual `va` out of RAM. `false` if any page is
    /// not a RAM mapping.
    fn read_guest(&self, va: u32, buf: &mut [u8]) -> bool {
        let mut done = 0;
        while done < buf.len() {
            let at = va.wrapping_add(done as u32);
            let Target::Ram { pa, .. } = self.tables.lookup(at) else {
                return false;
            };
            let in_page = (PAGE - (at & 0xFFF)) as usize;
            let n = in_page.min(buf.len() - done);
            // SAFETY: `pa` is a RAM page (below `ram_size`), so the range is
            // inside the host view; guest RAM is shared plain bytes.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    self.host_view.as_ptr().add((pa | (at & 0xFFF)) as usize),
                    buf[done..].as_mut_ptr(),
                    n,
                );
            }
            done += n;
        }
        true
    }

    /// Enters the guest. `Err` carries `errno`.
    fn kvm_run(&mut self) -> Result<(), i32> {
        self.census.enter();
        // SAFETY: `run` is valid and unreferenced; `KVM_RUN` takes no
        // argument and acts on the vCPU this value owns.
        let r = unsafe {
            (*self.run).exit_reason = KVM_EXIT_UNKNOWN;
            libc::ioctl(self.vcpu.as_raw_fd(), KVM_RUN, 0)
        };
        self.census.left();
        if r < 0 {
            Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
        } else {
            Ok(())
        }
    }

    fn exit_reason(&self) -> u32 {
        // SAFETY: `run` is valid and unreferenced.
        unsafe { (*self.run).exit_reason }
    }

    /// The registers as of the last exit (or as last set).
    fn kregs(&self) -> kvm_regs {
        // SAFETY: `run` is valid and unreferenced; `regs` is the union's
        // only real member.
        unsafe { (*self.run).s.regs.regs }
    }

    /// Sets the registers the next `KVM_RUN` starts from.
    fn set_kregs(&mut self, regs: kvm_regs) {
        // SAFETY: as for `kregs`.
        unsafe {
            (*self.run).s.regs.regs = regs;
            (*self.run).kvm_dirty_regs = u64::from(KVM_SYNC_X86_REGS);
        }
    }

    fn set_interrupt_window(&mut self, on: bool) {
        // SAFETY: `run` is valid and unreferenced.
        unsafe { (*self.run).request_interrupt_window = u8::from(on) };
    }

    fn clear_immediate_exit(&mut self) {
        // SAFETY: `run` is valid; a volatile store because the signal handler
        // writes the same byte.
        unsafe { (&raw mut (*self.run).immediate_exit).write_volatile(0) };
    }
}

impl Mapper for KvmCpu {
    fn map(&mut self, va: u32, pa: u32, len: u32, access: Access) {
        self.tables.map(va, pa, len, access);
    }

    fn map_mmio(&mut self, va: u32, pa: u32, len: u32) {
        self.tables.map_mmio(va, pa, len);
    }

    fn unmap(&mut self, va: u32, len: u32) {
        self.tables.unmap(va, len);
    }
}

impl Cpu for KvmCpu {
    fn name(&self) -> &'static str {
        "kvm"
    }

    fn run(&mut self, ctx: &mut Context, bus: &mut dyn Bus, flags: RunFlags) -> Exit {
        // SAFETY: `self.run` stays mapped for as long as `self.vcpu` lives,
        // and `leave` is called below before this borrow of `self` ends.
        unsafe { self.kick.enter(self.run) };
        let exit = self.run_guest(ctx, bus, flags);
        self.kick.leave();
        self.census.maybe_dump();
        exit
    }

    fn kicker(&self) -> Kicker {
        let kick = Arc::clone(&self.kick);
        Kicker::new(move || kick.kick())
    }

    fn protector(&self) -> Arc<dyn PageProtector> {
        Arc::clone(&self.protector) as Arc<dyn PageProtector>
    }

    fn set_trap_range(&mut self, va: u32, len: u32) {
        self.tables.set_trap_range(va, len);
    }

    fn invalidate_code(&mut self, _pa: u32, _len: u32) {}

    fn read_through(&mut self, pa: u32, len: u32, host: *const u8) -> bool {
        if len == 0
            || !pa.is_multiple_of(PAGE)
            || !len.is_multiple_of(PAGE)
            || (host as usize) & 0xFFF != 0
            || pa < self.ram_size
            || !self._kvm.check_extension(Cap::ReadonlyMem)
        {
            return false;
        }
        let region = kvm_userspace_memory_region {
            slot: self.slots,
            flags: kvm_bindings::KVM_MEM_READONLY,
            guest_phys_addr: u64::from(pa),
            memory_size: u64::from(len),
            userspace_addr: host as u64,
        };
        // SAFETY: the caller promises `host` stays mapped for `len` bytes as
        // long as this CPU lives (`Cpu::read_through`); the slot is
        // read-only for the guest, whose writes exit as device writes.
        match unsafe { self.vm.set_user_memory_region(region) } {
            Ok(()) => {
                self.slots += 1;
                true
            }
            Err(e) => {
                log::warn!(target: "cpu", "read-through slot at {pa:#010x}+{len:#x}: {e}");
                false
            }
        }
    }

    fn set_tsc(&mut self, tsc: u64) {
        // IA32_TIME_STAMP_COUNTER, in the guest's (scaled) units.
        const MSR_IA32_TSC: u32 = 0x10;
        let entry =
            kvm_bindings::kvm_msr_entry { index: MSR_IA32_TSC, data: tsc, ..Default::default() };
        match kvm_bindings::Msrs::from_entries(&[entry]) {
            Ok(msrs) => {
                if let Err(e) = self.vcpu.set_msrs(&msrs) {
                    log::warn!(target: "cpu", "KVM: the guest's TSC could not be set ({e})");
                }
            }
            Err(e) => log::warn!(target: "cpu", "KVM: the guest's TSC could not be set ({e:?})"),
        }
    }
}

/// Opens `/dev/kvm` and checks everything the backend needs from it.
fn open_kvm() -> Result<Kvm, String> {
    let kvm = Kvm::new().map_err(|e| format!("/dev/kvm cannot be opened read-write: {e}"))?;
    let version = kvm.get_api_version();
    if version != 12 {
        return Err(format!("KVM API version {version}, expected 12"));
    }
    let needed = [
        (Cap::UserMemory, "KVM_CAP_USER_MEMORY"),
        (Cap::ExtCpuid, "KVM_CAP_EXT_CPUID"),
        (Cap::ImmediateExit, "KVM_CAP_IMMEDIATE_EXIT"),
    ];
    for (cap, name) in needed {
        if !kvm.check_extension(cap) {
            return Err(format!("KVM lacks {name}"));
        }
    }
    if kvm.check_extension_int(Cap::SyncRegs) & KVM_SYNC_X86_REGS as i32 == 0 {
        return Err("KVM lacks KVM_CAP_SYNC_REGS for the general registers".to_owned());
    }
    Ok(kvm)
}

/// Fills in the guest environment: flat 32-bit ring 0 with paging on.
fn environment(s: &mut kvm_sregs) {
    let flat = |selector: u16, type_: u8| kvm_segment {
        base: 0,
        limit: 0xFFFF_FFFF,
        selector,
        type_,
        present: 1,
        dpl: 0,
        db: 1,
        s: 1,
        l: 0,
        g: 1,
        avl: 0,
        unusable: 0,
        padding: 0,
    };
    s.cs = flat(SEL_CODE, 0xB);
    s.ds = flat(SEL_DATA, 0x3);
    s.es = s.ds;
    s.ss = s.ds;
    s.gs = s.ds;
    s.fs = flat(SEL_FS, 0x3);
    s.tr = kvm_segment {
        base: u64::from(TSS_VA),
        limit: 0x67,
        selector: SEL_TSS,
        type_: 0xB,
        present: 1,
        ..Default::default()
    };
    s.ldt = kvm_segment { type_: 2, unusable: 1, ..Default::default() };
    s.gdt.base = u64::from(GDT_VA);
    s.gdt.limit = GDT_LIMIT;
    s.idt.base = u64::from(IDT_VA);
    s.idt.limit = 256 * 8 - 1;
    s.cr0 = CR0;
    s.cr2 = 0;
    s.cr3 = u64::from(CR3);
    s.cr4 = CR4;
    s.efer = 0;
    s.apic_base = 0;
}

fn regs_to_kvm(r: &Regs) -> kvm_regs {
    kvm_regs {
        rax: u64::from(r.gpr[reg::EAX]),
        rcx: u64::from(r.gpr[reg::ECX]),
        rdx: u64::from(r.gpr[reg::EDX]),
        rbx: u64::from(r.gpr[reg::EBX]),
        rsp: u64::from(r.gpr[reg::ESP]),
        rbp: u64::from(r.gpr[reg::EBP]),
        rsi: u64::from(r.gpr[reg::ESI]),
        rdi: u64::from(r.gpr[reg::EDI]),
        rip: u64::from(r.eip),
        rflags: u64::from((r.eflags & EFLAGS_LOAD_MASK) | eflags::FIXED),
        ..Default::default()
    }
}

/// Writes the integer state back. `fs_base` is left alone: only the host
/// changes it.
fn regs_from_kvm(k: &kvm_regs, r: &mut Regs) {
    r.gpr[reg::EAX] = k.rax as u32;
    r.gpr[reg::ECX] = k.rcx as u32;
    r.gpr[reg::EDX] = k.rdx as u32;
    r.gpr[reg::EBX] = k.rbx as u32;
    r.gpr[reg::ESP] = k.rsp as u32;
    r.gpr[reg::EBP] = k.rbp as u32;
    r.gpr[reg::ESI] = k.rsi as u32;
    r.gpr[reg::EDI] = k.rdi as u32;
    r.eip = k.rip as u32;
    r.eflags = k.rflags as u32;
}

// Offsets in the FXSAVE image.
const FX_FCW: usize = 0;
const FX_FSW: usize = 2;
const FX_FTW: usize = 4;
const FX_FOP: usize = 6;
const FX_FIP: usize = 8;
const FX_FDP: usize = 16;
const FX_MXCSR: usize = 24;
const FX_ST: usize = 32;
const FX_XMM: usize = 160;

fn fx_to_kvm(fx: &FxState) -> kvm_fpu {
    let b = &fx.0;
    let u16_at = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
    let u64_at = |i: usize| u64::from_le_bytes(b[i..i + 8].try_into().unwrap_or_default());
    let mut k = kvm_fpu {
        fcw: u16_at(FX_FCW),
        fsw: u16_at(FX_FSW),
        ftwx: b[FX_FTW],
        last_opcode: u16_at(FX_FOP),
        last_ip: u64_at(FX_FIP),
        last_dp: u64_at(FX_FDP),
        mxcsr: u32::from_le_bytes([b[FX_MXCSR], b[FX_MXCSR + 1], b[FX_MXCSR + 2], b[FX_MXCSR + 3]]),
        ..Default::default()
    };
    for (i, st) in k.fpr.iter_mut().enumerate() {
        st.copy_from_slice(&b[FX_ST + i * 16..FX_ST + (i + 1) * 16]);
    }
    for (i, xmm) in k.xmm.iter_mut().enumerate() {
        xmm.copy_from_slice(&b[FX_XMM + i * 16..FX_XMM + (i + 1) * 16]);
    }
    k
}

/// Writes the fields `kvm_fpu` carries into the FXSAVE image; the MXCSR mask
/// and the reserved tail keep what the context had.
fn fx_from_kvm(k: &kvm_fpu, fx: &mut FxState) {
    let b = &mut fx.0;
    b[FX_FCW..FX_FCW + 2].copy_from_slice(&k.fcw.to_le_bytes());
    b[FX_FSW..FX_FSW + 2].copy_from_slice(&k.fsw.to_le_bytes());
    b[FX_FTW] = k.ftwx;
    b[FX_FTW + 1] = 0;
    b[FX_FOP..FX_FOP + 2].copy_from_slice(&k.last_opcode.to_le_bytes());
    b[FX_FIP..FX_FIP + 8].copy_from_slice(&k.last_ip.to_le_bytes());
    b[FX_FDP..FX_FDP + 8].copy_from_slice(&k.last_dp.to_le_bytes());
    b[FX_MXCSR..FX_MXCSR + 4].copy_from_slice(&k.mxcsr.to_le_bytes());
    for (i, st) in k.fpr.iter().enumerate() {
        b[FX_ST + i * 16..FX_ST + (i + 1) * 16].copy_from_slice(st);
    }
    for (i, xmm) in k.xmm.iter().enumerate() {
        b[FX_XMM + i * 16..FX_XMM + (i + 1) * 16].copy_from_slice(xmm);
    }
}

#[cfg(test)]
mod tests;
