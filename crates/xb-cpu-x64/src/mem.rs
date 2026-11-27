//! The guest's address space as the translator enforces it.
//!
//! Three things decide what the guest may do with a page of RAM mapped at a
//! virtual address, and the host protection of that view is the most
//! restrictive of them:
//!
//! * the access the kernel's page table gave the mapping (`Mapper::map`);
//! * the restriction write tracking put on the physical page
//!   (`PageProtector::protect`), which applies to every view of it;
//! * the translator's own write protection of physical pages that hold
//!   translated code (see `cache`).
//!
//! The bookkeeping is a flat page table, and for each physical page the list
//! of virtual pages showing it. It sits behind a mutex because the protector
//! is called from other threads; the guest-CPU thread is the only one that
//! changes mappings.

use std::os::fd::BorrowedFd;
use std::sync::{Mutex, MutexGuard, PoisonError};

use xb_cpu::{Access, PageProtector};

use crate::host::GuestSpace;

const PAGE: u32 = 4096;
const PAGES: usize = 1 << 20;

/// One entry of the flat page table: the physical page number in the high 20
/// bits, flags in the low 12.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Pte(u32);

impl Pte {
    const VALID: u32 = 1;
    const READ: u32 = 2;
    const WRITE: u32 = 4;
    const MMIO: u32 = 8;

    fn ram(pa: u32, access: Access) -> Self {
        let f = match access {
            Access::None => 0,
            Access::Read => Self::READ,
            Access::ReadWrite => Self::READ | Self::WRITE,
        };
        Self(pa & !0xFFF | Self::VALID | f)
    }

    /// Whether the page is device registers.
    pub(crate) fn is_mmio(self) -> bool {
        self.0 & Self::MMIO != 0
    }

    /// Whether the page is RAM.
    pub(crate) fn is_ram(self) -> bool {
        self.0 & (Self::VALID | Self::MMIO) == Self::VALID
    }

    /// Physical address of the page.
    pub(crate) fn pa(self) -> u32 {
        self.0 & !0xFFF
    }

    /// The access the mapping itself allows.
    pub(crate) fn access(self) -> Access {
        if self.0 & Self::WRITE != 0 {
            Access::ReadWrite
        } else if self.0 & Self::READ != 0 {
            Access::Read
        } else {
            Access::None
        }
    }
}

fn rank(a: Access) -> u8 {
    match a {
        Access::None => 0,
        Access::Read => 1,
        Access::ReadWrite => 2,
    }
}

fn unrank(r: u8) -> Access {
    match r {
        0 => Access::None,
        1 => Access::Read,
        _ => Access::ReadWrite,
    }
}

/// Per-physical-page state.
#[derive(Clone, Default)]
struct Phys {
    /// Virtual page numbers that show this page.
    views: Vec<u32>,
    /// Write tracking's restriction, as a [`rank`] (2: none).
    tracker: u8,
    /// The page holds translated code and is write protected for it.
    code: bool,
}

struct State {
    pte: Vec<u32>,
    phys: Vec<Phys>,
}

impl State {
    fn effective(&self, pte: Pte) -> Access {
        let p = &self.phys[(pte.pa() / PAGE) as usize];
        let mut r = rank(pte.access()).min(p.tracker);
        if p.code {
            r = r.min(1);
        }
        unrank(r)
    }

    fn drop_view(&mut self, vpn: u32) {
        let old = Pte(self.pte[vpn as usize]);
        if old.is_ram() {
            let views = &mut self.phys[(old.pa() / PAGE) as usize].views;
            if let Some(i) = views.iter().position(|&v| v == vpn) {
                views.swap_remove(i);
            }
        }
        self.pte[vpn as usize] = 0;
    }
}

/// What the translator knows about one guest virtual page.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PageInfo {
    /// The mapping.
    pub(crate) pte: Pte,
    /// What the mapping and write tracking allow, leaving the translator's
    /// own code protection aside.
    pub(crate) allowed: Access,
    /// Whether the physical page is write protected for translated code.
    pub(crate) code: bool,
}

/// The guest address space: reservation, RAM views and their protection.
pub(crate) struct Memory {
    space: GuestSpace,
    state: Mutex<State>,
}

impl Memory {
    /// Reserves the address space for a guest with the given RAM.
    pub(crate) fn new(ram_fd: BorrowedFd<'_>, ram_size: u32) -> Result<Self, String> {
        let space = GuestSpace::new(ram_fd, ram_size)?;
        let state = State {
            pte: vec![0; PAGES],
            phys: vec![
                Phys { views: Vec::new(), tracker: 2, code: false };
                (ram_size / PAGE) as usize
            ],
        };
        Ok(Self { space, state: Mutex::new(state) })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Host address of guest address 0.
    pub(crate) fn base(&self) -> usize {
        self.space.base()
    }

    /// Size of RAM.
    pub(crate) fn ram_size(&self) -> u32 {
        self.space.ram_size()
    }

    /// The guest address a faulting host address stands for.
    pub(crate) fn guest_of(&self, host: usize) -> Option<u32> {
        self.space.guest_of(host)
    }

    /// See `Mapper::map`.
    pub(crate) fn map(&self, va: u32, pa: u32, len: u32, access: Access) {
        if !self.check(va, len) {
            return;
        }
        if pa & 0xFFF != 0 || u64::from(pa) + u64::from(len) > u64::from(self.ram_size()) {
            log::error!(target: "cpu", "map of {va:#010x}+{len:#x} to physical {pa:#010x}: outside RAM");
            return;
        }
        let mut s = self.lock();
        if let Err(e) = self.space.map(va, pa, len, access) {
            log::error!(target: "cpu", "map of {va:#010x}+{len:#x}: {e}");
            return;
        }
        for i in 0..len / PAGE {
            let vpn = va / PAGE + i;
            s.drop_view(vpn);
            let pte = Pte::ram(pa + i * PAGE, access);
            s.pte[vpn as usize] = pte.0;
            s.phys[(pte.pa() / PAGE) as usize].views.push(vpn);
        }
        // Pages that write tracking or translated code restrict further.
        let mut run: Option<(u32, u32, Access)> = None;
        for i in 0..len / PAGE {
            let vpn = va / PAGE + i;
            let eff = s.effective(Pte(s.pte[vpn as usize]));
            let differs = rank(eff) < rank(access);
            match run {
                Some((start, n, a)) if differs && a == eff && start + n == vpn => {
                    run = Some((start, n + 1, a));
                }
                _ => {
                    if let Some((start, n, a)) = run.take() {
                        self.space.protect(start * PAGE, (n * PAGE) as usize, a);
                    }
                    if differs {
                        run = Some((vpn, 1, eff));
                    }
                }
            }
        }
        if let Some((start, n, a)) = run {
            self.space.protect(start * PAGE, (n * PAGE) as usize, a);
        }
    }

    /// See `Mapper::map_mmio`.
    pub(crate) fn map_mmio(&self, va: u32, pa: u32, len: u32) {
        if !self.check(va, len) {
            return;
        }
        let mut s = self.lock();
        if let Err(e) = self.space.unmap(va, len) {
            log::error!(target: "cpu", "map_mmio of {va:#010x}+{len:#x}: {e}");
            return;
        }
        for i in 0..len / PAGE {
            let vpn = va / PAGE + i;
            s.drop_view(vpn);
            s.pte[vpn as usize] = (pa.wrapping_add(i * PAGE) & !0xFFF) | Pte::VALID | Pte::MMIO;
        }
    }

    /// See `Mapper::unmap`.
    pub(crate) fn unmap(&self, va: u32, len: u32) {
        if !self.check(va, len) {
            return;
        }
        let mut s = self.lock();
        if let Err(e) = self.space.unmap(va, len) {
            log::error!(target: "cpu", "unmap of {va:#010x}+{len:#x}: {e}");
            return;
        }
        for i in 0..len / PAGE {
            s.drop_view(va / PAGE + i);
        }
    }

    fn check(&self, va: u32, len: u32) -> bool {
        let ok = va & 0xFFF == 0 && len & 0xFFF == 0 && u64::from(va) + u64::from(len) <= 1 << 32;
        if !ok {
            log::error!(target: "cpu", "bad guest range {va:#010x}+{len:#x}");
        }
        ok && len != 0
    }

    /// The mapping of the page holding `va`.
    pub(crate) fn pte(&self, va: u32) -> Pte {
        Pte(self.lock().pte[(va / PAGE) as usize])
    }

    /// Everything about the page holding `va`.
    pub(crate) fn info(&self, va: u32) -> PageInfo {
        let s = self.lock();
        let pte = Pte(s.pte[(va / PAGE) as usize]);
        if !pte.is_ram() {
            return PageInfo { pte, allowed: Access::None, code: false };
        }
        let p = &s.phys[(pte.pa() / PAGE) as usize];
        PageInfo { pte, allowed: unrank(rank(pte.access()).min(p.tracker)), code: p.code }
    }

    /// Sets or clears the translator's write protection of a physical page.
    pub(crate) fn set_code(&self, pa: u32, on: bool) {
        let mut s = self.lock();
        let idx = (pa / PAGE) as usize;
        if idx >= s.phys.len() || s.phys[idx].code == on {
            return;
        }
        s.phys[idx].code = on;
        self.reprotect(&s, idx as u32, 1);
    }

    /// Applies the effective protection to every view of physical pages
    /// `[first, first + n)`, in as few calls as their layout allows.
    fn reprotect(&self, s: &State, first: u32, n: u32) {
        let mut todo: Vec<(u32, Access)> = Vec::new();
        for ppn in first..first + n {
            for &vpn in &s.phys[ppn as usize].views {
                todo.push((vpn, s.effective(Pte(s.pte[vpn as usize]))));
            }
        }
        todo.sort_unstable_by_key(|&(vpn, _)| vpn);
        let mut i = 0;
        while i < todo.len() {
            let (start, a) = todo[i];
            let mut j = i + 1;
            while j < todo.len() && todo[j].1 == a && todo[j].0 == start + (j - i) as u32 {
                j += 1;
            }
            self.space.protect(start * PAGE, (j - i) * PAGE as usize, a);
            i = j;
        }
    }

    /// Copies guest RAM at virtual `va` into `buf`, page by page through the
    /// page table and the host's view (so host protections do not matter).
    /// Returns how many bytes were readable before a page that is not RAM.
    pub(crate) fn read(&self, va: u32, buf: &mut [u8]) -> usize {
        let mut done = 0;
        while done < buf.len() {
            let at = va.wrapping_add(done as u32);
            let pte = self.pte(at);
            if !pte.is_ram() {
                break;
            }
            let off = at & 0xFFF;
            let n = (buf.len() - done).min((PAGE - off) as usize);
            // SAFETY: the page is inside RAM (checked when it was mapped) and
            // the host view covers all of RAM.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    self.space.phys().add((pte.pa() + off) as usize),
                    buf[done..].as_mut_ptr(),
                    n,
                );
            }
            done += n;
        }
        done
    }

    /// Copies `data` to physical address `pa` (inside one page of RAM).
    pub(crate) fn write_phys(&self, pa: u32, data: &[u8]) {
        debug_assert!(u64::from(pa) + data.len() as u64 <= u64::from(self.ram_size()));
        // SAFETY: the caller got `pa` from a RAM page table entry, and the
        // host view covers all of RAM.
        unsafe {
            std::ptr::copy_nonoverlapping(
                data.as_ptr(),
                self.space.phys().add(pa as usize),
                data.len(),
            );
        }
    }
}

impl PageProtector for Memory {
    fn protect(&self, pa: u32, len: u32, access: Access) {
        let end = u64::from(pa) + u64::from(len);
        if pa & 0xFFF != 0 || len & 0xFFF != 0 || end > u64::from(self.ram_size()) {
            log::error!(target: "cpu", "protect of physical {pa:#010x}+{len:#x}: bad range");
            return;
        }
        let mut s = self.lock();
        let first = pa / PAGE;
        let n = len / PAGE;
        let r = rank(access);
        // Runs of pages whose state changes are applied together.
        let mut start = None;
        for ppn in first..=first + n {
            let changes = ppn < first + n && s.phys[ppn as usize].tracker != r;
            if changes {
                s.phys[ppn as usize].tracker = r;
                start.get_or_insert(ppn);
            } else if let Some(b) = start.take() {
                self.reprotect(&s, b, ppn - b);
            }
        }
    }
}
