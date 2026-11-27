//! Mock [`Mapper`] and [`PageProtector`] implementations for the tests: they
//! record calls and model what a CPU backend would enforce.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

use xb_types::mem::{Access, Mapper, PageProtector};
use xb_types::{PAGE_SHIFT, PAGE_SIZE};

use crate::space::AddressSpace;

/// One `Mapper` call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Call {
    Map(u32, u32, u32, Access),
    MapMmio(u32, u32, u32),
    Unmap(u32, u32),
}

/// A mapper that records its calls and keeps the page table a backend would
/// build from them: virtual page -> (physical address, access, device?).
#[derive(Default)]
pub(crate) struct ModelMapper {
    pub(crate) calls: Vec<Call>,
    pages: BTreeMap<u32, (u32, Access, bool)>,
}

impl ModelMapper {
    /// Asserts that the table built from the calls equals `space`'s.
    pub(crate) fn assert_mirrors(&self, space: &AddressSpace) {
        let mut present = 0;
        for vp in 0..(1u32 << (32 - PAGE_SHIFT)) {
            let e = space.entry(vp << PAGE_SHIFT);
            if !e.is_present() {
                continue;
            }
            present += 1;
            let got = self.pages.get(&vp);
            assert_eq!(
                got,
                Some(&(e.pa(), e.access(), e.is_mmio())),
                "page {:08x}",
                vp << PAGE_SHIFT
            );
        }
        assert_eq!(present, self.pages.len(), "the mapper has pages the table does not");
    }
}

impl Mapper for ModelMapper {
    fn map(&mut self, va: u32, pa: u32, len: u32, access: Access) {
        assert!((va | pa | len).is_multiple_of(PAGE_SIZE) && len != 0);
        self.calls.push(Call::Map(va, pa, len, access));
        for off in (0..len).step_by(PAGE_SIZE as usize) {
            self.pages.insert((va + off) >> PAGE_SHIFT, (pa + off, access, false));
        }
    }

    fn map_mmio(&mut self, va: u32, pa: u32, len: u32) {
        assert!((va | pa | len).is_multiple_of(PAGE_SIZE) && len != 0);
        self.calls.push(Call::MapMmio(va, pa, len));
        for off in (0..len).step_by(PAGE_SIZE as usize) {
            self.pages.insert((va + off) >> PAGE_SHIFT, (pa + off, Access::ReadWrite, true));
        }
    }

    fn unmap(&mut self, va: u32, len: u32) {
        assert!((va | len).is_multiple_of(PAGE_SIZE) && len != 0);
        self.calls.push(Call::Unmap(va, len));
        for off in (0..len).step_by(PAGE_SIZE as usize) {
            self.pages.remove(&((va + off) >> PAGE_SHIFT));
        }
    }
}

fn code(a: Access) -> u8 {
    match a {
        Access::None => 0,
        Access::Read => 1,
        Access::ReadWrite => 2,
    }
}

/// A protector that records calls and models the guest's side: each page has
/// the access last set and a count of guest stores that succeeded.
///
/// `protect` and a guest access exclude each other, which is the trait's
/// promise: once `protect` returns, no access the new setting forbids
/// succeeds.
pub(crate) struct MockProtector {
    access: Vec<AtomicU8>,
    stores: Vec<AtomicU64>,
    gate: Mutex<()>,
    pub(crate) calls: Mutex<Vec<(u32, u32, Access)>>,
    /// Makes `protect` give up its time slice before it takes effect, to
    /// widen the window between a caller's bookkeeping and the protection.
    pub(crate) slow: AtomicBool,
}

impl MockProtector {
    pub(crate) fn new(pages: u32) -> Self {
        Self {
            access: (0..pages).map(|_| AtomicU8::new(code(Access::ReadWrite))).collect(),
            stores: (0..pages).map(|_| AtomicU64::new(0)).collect(),
            gate: Mutex::new(()),
            calls: Mutex::new(Vec::new()),
            slow: AtomicBool::new(false),
        }
    }

    /// The access last set for the page holding `pa`.
    pub(crate) fn access(&self, pa: u32) -> Access {
        match self.access[(pa >> PAGE_SHIFT) as usize].load(Ordering::SeqCst) {
            0 => Access::None,
            1 => Access::Read,
            _ => Access::ReadWrite,
        }
    }

    /// A guest store to `pa`: performed (the page's count goes up) if the
    /// page is writable, else `false` (the guest would fault).
    pub(crate) fn guest_store(&self, pa: u32) -> bool {
        let _g = self.gate.lock().unwrap();
        if self.access(pa) != Access::ReadWrite {
            return false;
        }
        self.stores[(pa >> PAGE_SHIFT) as usize].fetch_add(1, Ordering::SeqCst);
        true
    }

    /// Whether a guest load from `pa` would succeed.
    pub(crate) fn guest_can_read(&self, pa: u32) -> bool {
        self.access(pa) != Access::None
    }

    /// Guest stores to the page holding `pa` so far.
    pub(crate) fn stores(&self, pa: u32) -> u64 {
        self.stores[(pa >> PAGE_SHIFT) as usize].load(Ordering::SeqCst)
    }

    /// Takes the recorded calls.
    pub(crate) fn take_calls(&self) -> Vec<(u32, u32, Access)> {
        std::mem::take(&mut self.calls.lock().unwrap())
    }
}

impl PageProtector for MockProtector {
    fn protect(&self, pa: u32, len: u32, access: Access) {
        assert!((pa | len).is_multiple_of(PAGE_SIZE) && len != 0, "protect({pa:#x}, {len:#x})");
        if self.slow.load(Ordering::Relaxed) {
            std::thread::yield_now();
        }
        let _g = self.gate.lock().unwrap();
        self.calls.lock().unwrap().push((pa, len, access));
        for p in (pa >> PAGE_SHIFT)..((pa + len) >> PAGE_SHIFT) {
            self.access[p as usize].store(code(access), Ordering::SeqCst);
        }
    }
}
