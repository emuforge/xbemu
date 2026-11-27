//! The hidden guest memory: page tables, descriptor tables and stub code.
//!
//! One anonymous host mapping is registered as a second KVM memory slot at
//! [`HIDDEN_GPA`], outside RAM and outside every device range. It holds, in
//! order: the page directory, the system page (GDT, two TSSs, IDT), the stub
//! page, the double-fault stack, the trap page and all 1024 page tables.
//! Every page-directory entry is present from the start, so the page tables
//! form one flat array of 2^20 entries indexed by `va >> 12`; pages of it the
//! guest never maps are never touched and cost no host memory.

use xb_types::mem::Access;

use super::ram::{Mmap, PAGE};

/// Guest-physical base of the hidden slot.
pub const HIDDEN_GPA: u32 = 0xE000_0000;
/// Length of the hidden slot.
pub const HIDDEN_LEN: u32 = PT_OFF + 1024 * PAGE;

/// Guest-virtual range this backend keeps for itself: the system page, the
/// stub page and the double-fault stack. `Mapper` calls must stay out of it.
pub const SYSTEM_VA: u32 = 0xFFFE_0000;
/// Length of the reserved range at [`SYSTEM_VA`].
pub const SYSTEM_VA_LEN: u32 = 3 * PAGE;

const PD_OFF: u32 = 0;
const SYS_OFF: u32 = PAGE;
const STUB_OFF: u32 = 2 * PAGE;
const DF_STACK_OFF: u32 = 3 * PAGE;
const TRAP_OFF: u32 = 4 * PAGE;
const PT_OFF: u32 = 5 * PAGE;

/// Page-table entry bits.
const P: u32 = 1;
const RW: u32 = 2;
/// Accessed and dirty are preset so the processor never has to write them.
const AD: u32 = 0x60;

/// `HLT`: the one-byte instruction every stub and trap address holds.
const HLT: u8 = 0xF4;

/// Selectors of the guest's GDT. The values are the ones the Xbox kernel
/// uses, so a title that stores and reloads a selector sees familiar numbers.
pub(crate) const SEL_CODE: u16 = 0x08;
pub(crate) const SEL_DATA: u16 = 0x10;
pub(crate) const SEL_TSS: u16 = 0x18;
pub(crate) const SEL_FS: u16 = 0x20;
const SEL_DF_TSS: u16 = 0x28;
/// Limit of the GDT (six descriptors).
pub(crate) const GDT_LIMIT: u16 = 6 * 8 - 1;

/// Guest-virtual address of the GDT.
pub(crate) const GDT_VA: u32 = SYSTEM_VA;
/// Guest-virtual address of the running task's TSS.
pub(crate) const TSS_VA: u32 = SYSTEM_VA + TSS_OFF;
/// Guest-virtual address of the IDT.
pub(crate) const IDT_VA: u32 = SYSTEM_VA + IDT_OFF;
const TSS_OFF: u32 = 0x080;
const DF_TSS_OFF: u32 = 0x100;
const IDT_OFF: u32 = 0x800;

/// Guest-virtual address of the stub page. Vector `n`'s interrupt gate
/// points at `STUB_VA + n`.
pub(crate) const STUB_VA: u32 = SYSTEM_VA + PAGE;
/// Entry point of the double-fault task.
pub(crate) const DF_STUB_VA: u32 = STUB_VA + 0x100;
/// `mov cr3, eax; hlt`: reloads CR3 to flush the guest's TLB.
pub(crate) const FLUSH_STUB_VA: u32 = STUB_VA + 0x200;
/// Where `FLUSH_STUB_VA` stops.
pub(crate) const FLUSH_STUB_END: u32 = FLUSH_STUB_VA + 4;
const DF_STACK_VA: u32 = SYSTEM_VA + 2 * PAGE;

/// The value of CR3: the page directory's guest-physical address.
pub(crate) const CR3: u32 = HIDDEN_GPA + PD_OFF;

/// Offsets of the registers a task switch saves in a 32-bit TSS.
const TSS_CR3: u32 = 0x1C;
const TSS_EIP: u32 = 0x20;
const TSS_EFLAGS: u32 = 0x24;
const TSS_GPR: u32 = 0x28;
const TSS_SEG: u32 = 0x48;

/// What a guest-virtual page is mapped to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Target {
    /// Nothing.
    None,
    /// Guest RAM at this physical page address.
    Ram { pa: u32, writable: bool },
    /// Device registers or one of this backend's own pages.
    Other,
}

/// The hidden slot and the bookkeeping for it.
pub(crate) struct Tables {
    mem: Mmap,
    ram_size: u32,
    /// For each RAM page, `va | 1` of the mapping made last (0: none). Turns
    /// the physical address of a write-tracking fault back into a virtual one.
    last_va: Vec<u32>,
    trap: (u32, u32),
    /// A present entry changed since the guest last ran: its TLB may hold
    /// the old translation.
    stale_tlb: bool,
}

impl Tables {
    /// Builds the tables for a guest with `ram_size` bytes of RAM.
    pub(crate) fn new(ram_size: u32) -> Result<Self, String> {
        let mem = Mmap::anonymous(HIDDEN_LEN as usize)?;
        let mut t = Self {
            mem,
            ram_size,
            last_va: vec![0; (ram_size / PAGE) as usize],
            trap: (0, 0),
            stale_tlb: false,
        };
        for i in 0..1024 {
            t.write_u32(PD_OFF + i * 4, (HIDDEN_GPA + PT_OFF + i * PAGE) | P | RW | 0x20);
        }
        t.fill(STUB_OFF, HLT);
        t.fill(TRAP_OFF, HLT);
        t.write(STUB_OFF + (FLUSH_STUB_VA - STUB_VA), &[0x0F, 0x22, 0xD8, HLT]);
        t.set_pte(SYSTEM_VA, (HIDDEN_GPA + SYS_OFF) | P | RW | AD);
        t.set_pte(STUB_VA, (HIDDEN_GPA + STUB_OFF) | P | AD);
        t.set_pte(DF_STACK_VA, (HIDDEN_GPA + DF_STACK_OFF) | P | RW | AD);
        t.write_system_page(0);
        Ok(t)
    }

    /// Host address and length of the hidden slot, for KVM.
    pub(crate) fn slot(&self) -> (*mut u8, usize) {
        (self.mem.as_ptr(), self.mem.len())
    }

    /// (Re)writes the GDT, both TSSs and the IDT. A task switch into the
    /// double-fault task marks descriptors busy and saves state in the TSSs,
    /// so this is also how the system page is repaired afterwards.
    pub(crate) fn write_system_page(&mut self, fs_base: u32) {
        self.fill(SYS_OFF, 0);
        let gdt = [
            (SEL_CODE, descriptor(0, 0xF_FFFF, 0x9B, 0xC)),
            (SEL_DATA, descriptor(0, 0xF_FFFF, 0x93, 0xC)),
            // The running task's descriptor is busy (type 11).
            (SEL_TSS, descriptor(TSS_VA, 0x67, 0x8B, 0)),
            (SEL_FS, descriptor(fs_base, 0xF_FFFF, 0x93, 0xC)),
            (SEL_DF_TSS, descriptor(SYSTEM_VA + DF_TSS_OFF, 0x67, 0x89, 0)),
        ];
        for (sel, d) in gdt {
            self.write(SYS_OFF + u32::from(sel), &d);
        }
        // No I/O bitmap in either TSS (offset past the limit).
        self.write(SYS_OFF + TSS_OFF + 0x66, &0x68u16.to_le_bytes());
        let df = SYS_OFF + DF_TSS_OFF;
        self.write_u32(df + TSS_CR3, CR3);
        self.write_u32(df + TSS_EIP, DF_STUB_VA);
        self.write_u32(df + TSS_EFLAGS, 2);
        self.write_u32(df + TSS_GPR + 4 * 4, DF_STACK_VA + PAGE - 16);
        // ES, CS, SS, DS, FS, GS.
        for (i, sel) in
            [SEL_DATA, SEL_CODE, SEL_DATA, SEL_DATA, SEL_FS, SEL_DATA].iter().enumerate()
        {
            self.write_u32(df + TSS_SEG + 4 * i as u32, u32::from(*sel));
        }
        self.write(df + 0x66, &0x68u16.to_le_bytes());

        for v in 0..256u32 {
            let gate = if v == 8 {
                // Double fault: a task gate, so the handler runs on a stack
                // that is known to work.
                [0, 0, SEL_DF_TSS as u8, 0, 0, 0x85, 0, 0]
            } else {
                let off = STUB_VA + v;
                let (lo, hi) = ((off as u16).to_le_bytes(), ((off >> 16) as u16).to_le_bytes());
                [lo[0], lo[1], SEL_CODE as u8, 0, 0, 0x8E, hi[0], hi[1]]
            };
            self.write(SYS_OFF + IDT_OFF + v * 8, &gate);
        }
    }

    /// Points the FS descriptor at `base`, so reloading FS keeps the base.
    pub(crate) fn set_fs_descriptor(&mut self, base: u32) {
        self.write(SYS_OFF + u32::from(SEL_FS), &descriptor(base, 0xF_FFFF, 0x93, 0xC));
    }

    /// The state the processor saved in the main TSS when it switched to the
    /// double-fault task: `(gpr, eip, eflags)`.
    pub(crate) fn double_fault_state(&self) -> ([u32; 8], u32, u32) {
        let tss = SYS_OFF + TSS_OFF;
        let mut gpr = [0; 8];
        for (i, g) in gpr.iter_mut().enumerate() {
            *g = self.read_u32(tss + TSS_GPR + 4 * i as u32);
        }
        (gpr, self.read_u32(tss + TSS_EIP), self.read_u32(tss + TSS_EFLAGS))
    }

    /// Whether the guest's TLB must be flushed before it runs again; clears
    /// the mark.
    pub(crate) fn take_stale_tlb(&mut self) -> bool {
        std::mem::take(&mut self.stale_tlb)
    }

    /// Whether `va` lies in the trap range.
    pub(crate) fn in_trap_range(&self, va: u32) -> bool {
        va.wrapping_sub(self.trap.0) < self.trap.1
    }

    /// Replaces the trap range: every page of it maps the trap page.
    pub(crate) fn set_trap_range(&mut self, va: u32, len: u32) {
        let (old_va, old_len) = self.trap;
        self.for_pages(old_va, old_len, |t, va| t.set_pte(va, 0));
        self.trap = (va, len);
        self.for_pages(va, len, |t, va| t.set_pte(va, (HIDDEN_GPA + TRAP_OFF) | P | AD));
    }

    /// `Mapper::map`.
    pub(crate) fn map(&mut self, va: u32, pa: u32, len: u32, access: Access) {
        let flags = match access {
            Access::None => 0,
            Access::Read => P | AD,
            Access::ReadWrite => P | RW | AD,
        };
        let ram_size = self.ram_size;
        self.for_pages(va, len, |t, page_va| {
            let page_pa = pa.wrapping_add(page_va.wrapping_sub(va));
            if page_pa >= ram_size {
                log::error!(target: "cpu", "map of {page_va:#010x} to {page_pa:#010x}: not RAM");
                t.set_pte(page_va, 0);
                return;
            }
            t.set_pte(page_va, if flags == 0 { 0 } else { page_pa | flags });
            t.last_va[(page_pa / PAGE) as usize] = page_va | 1;
        });
    }

    /// `Mapper::map_mmio`.
    pub(crate) fn map_mmio(&mut self, va: u32, pa: u32, len: u32) {
        let ram_size = self.ram_size;
        self.for_pages(va, len, |t, page_va| {
            let page_pa = pa.wrapping_add(page_va.wrapping_sub(va));
            if page_pa < ram_size || page_pa.wrapping_sub(HIDDEN_GPA) < HIDDEN_LEN {
                log::error!(
                    target: "cpu",
                    "device page {page_va:#010x} at physical {page_pa:#010x} overlaps guest memory"
                );
                t.set_pte(page_va, 0);
                return;
            }
            t.set_pte(page_va, page_pa | P | RW | AD);
        });
    }

    /// `Mapper::unmap`.
    pub(crate) fn unmap(&mut self, va: u32, len: u32) {
        self.for_pages(va, len, |t, va| t.set_pte(va, 0));
    }

    /// What the page holding `va` maps.
    pub(crate) fn lookup(&self, va: u32) -> Target {
        let pte = self.pte(va);
        if pte & P == 0 {
            Target::None
        } else if pte & !0xFFF < self.ram_size {
            Target::Ram { pa: pte & !0xFFF, writable: pte & RW != 0 }
        } else {
            Target::Other
        }
    }

    /// A virtual address that maps physical RAM address `pa`: the mapping
    /// made last, if it still stands.
    pub(crate) fn va_of(&self, pa: u32) -> Option<u32> {
        let hint = self.last_va.get((pa / PAGE) as usize).copied().unwrap_or(0);
        let va = hint & !0xFFF;
        (hint & 1 != 0 && self.pte(va) & (P | !0xFFF) == (pa & !0xFFF) | P)
            .then_some(va | (pa & 0xFFF))
    }

    /// Runs `f` for each page of `[va, va + len)` outside the reserved range.
    fn for_pages(&mut self, va: u32, len: u32, mut f: impl FnMut(&mut Self, u32)) {
        let mut reported = false;
        for i in 0..len.div_ceil(PAGE) {
            let page_va = (va & !0xFFF).wrapping_add(i * PAGE);
            if page_va.wrapping_sub(SYSTEM_VA) < SYSTEM_VA_LEN {
                if !reported {
                    log::error!(
                        target: "cpu",
                        "mapping request {va:#010x}+{len:#x} touches the KVM backend's reserved \
                         pages at {SYSTEM_VA:#010x}; those pages are left alone"
                    );
                    reported = true;
                }
                continue;
            }
            f(self, page_va);
        }
    }

    fn pte(&self, va: u32) -> u32 {
        self.read_u32(PT_OFF + (va >> 12) * 4)
    }

    fn set_pte(&mut self, va: u32, value: u32) {
        let off = PT_OFF + (va >> 12) * 4;
        let old = self.read_u32(off);
        if old != value {
            // The processor caches present translations only.
            self.stale_tlb |= old & P != 0;
            self.write_u32(off, value);
        }
    }

    fn read_u32(&self, off: u32) -> u32 {
        assert!(off as usize + 4 <= self.mem.len());
        // SAFETY: in bounds (checked above); the guest may write this memory
        // concurrently only through the double-fault stack and the TSSs,
        // which are read while it is stopped.
        unsafe { self.mem.as_ptr().add(off as usize).cast::<u32>().read_unaligned() }
    }

    fn write_u32(&mut self, off: u32, v: u32) {
        self.write(off, &v.to_le_bytes());
    }

    fn write(&mut self, off: u32, data: &[u8]) {
        assert!(off as usize + data.len() <= self.mem.len());
        // SAFETY: in bounds (checked above); the vCPU is stopped whenever the
        // tables are edited (`&mut self` on the CPU thread).
        unsafe {
            std::ptr::copy_nonoverlapping(
                data.as_ptr(),
                self.mem.as_ptr().add(off as usize),
                data.len(),
            );
        }
    }

    fn fill(&mut self, page_off: u32, byte: u8) {
        self.write(page_off, &[byte; PAGE as usize]);
    }
}

/// Encodes a segment descriptor. `limit` is the 20-bit limit field, `access`
/// the type byte (present, DPL, S, type), `flags` the granularity nibble.
fn descriptor(base: u32, limit: u32, access: u8, flags: u8) -> [u8; 8] {
    let b = base.to_le_bytes();
    let l = limit.to_le_bytes();
    [l[0], l[1], b[0], b[1], b[2], access, (flags << 4) | (l[2] & 0xF), b[3]]
}
