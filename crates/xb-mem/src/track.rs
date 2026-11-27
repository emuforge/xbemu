//! Write tracking of guest RAM, by physical page.
//!
//! The semantics are the reference's.
//!
//! The GPU asks whether the guest wrote RAM since it last looked, and must
//! hear about the first store to a page it cleaned, before the store happens
//! (the first-write barrier may wait for GPU work that reads the page in
//! place). A clean page is therefore write-protected for the guest through
//! the CPU backend's [`PageProtector`]; the guest's first store faults, the
//! machine calls [`WriteTracker::on_fault`], which calls
//! [`TrackerEvents::first_write`], marks the page dirty, restores access, and
//! the store runs again.
//!
//! # State
//!
//! Each page has three bits:
//!
//! * **clean**: protected against guest stores and unwritten since. Not
//!   clean means dirty and untracked: stores go unseen until the page is
//!   protected again. Every page starts dirty.
//! * **sticky**: written before [`WriteTracker::rearm`] protected it again
//!   (or written by an image landing), which a `test_and_clear` still
//!   reports.
//! * **no access**: the guest may not even read it
//!   ([`WriteTracker::noaccess`]); its next access goes to
//!   [`TrackerEvents::stale_access`].
//!
//! Invariant: the guest's access to a page is `None` if no-access, else
//! `Read` if clean, else `ReadWrite`.
//!
//! The reference has a third tracking state, "written through one view",
//! because it protects each host mapping of a page separately. A
//! `PageProtector` covers every guest mapping of a physical page at once, so
//! that state is dirty here.
//!
//! # Concurrency
//!
//! Every method may be called from any thread while the guest runs. State
//! and protection change together under one internal lock, so they cannot be
//! left disagreeing by two racing calls (in the reference a host write racing
//! a `test_and_clear` can leave a page protected but unmarked, which its
//! fault handler papers over by retrying).
//!
//! Two orderings inside the lock matter to the fault path, which looks at a
//! page's state without it: a page is marked before access is taken away and
//! unmarked after access is given back, so a fault always finds its page
//! marked.
//!
//! A store racing with `test_and_clear` is never lost. `protect` is
//! synchronous: a store that succeeded did so before `test_and_clear`
//! returned, so the caller, reading the page after, sees its bytes; a store
//! that comes later faults and marks the page dirty for the next call.
//!
//! `set_dirty` on pages that are all dirty already changes nothing and does
//! not take the lock (the reasoning is at the look).
//!
//! No callback is made with the lock held, so [`TrackerEvents`] methods may
//! call back into the tracker and may block (they wait for the GPU).
//!
//! `PageProtector::protect` is called with the lock held, and `on_fault`
//! takes the lock. A protector must therefore not wait for the guest-CPU
//! thread to leave `Bus::page_fault`; a CPU thread inside the handler is
//! already outside guest code, which is all `protect` needs.

use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};

use xb_types::mem::{Access, PageProtector};
use xb_types::{PAGE_SHIFT, PAGE_SIZE};

const CLEAN: u8 = 1 << 0;
const STICKY: u8 = 1 << 1;
const NOACCESS: u8 = 1 << 2;

/// Guest faults at one address on a page nobody holds protected that are
/// answered with a retry before the fault is given up as not ours, as in the
/// reference.
const SPURIOUS_RETRIES: u64 = 4;

/// How often a host access asks for a page without access before going on
/// (a page several ranges share takes one call each), as in the reference.
const STALE_OPEN_TRIES: u32 = 16;

/// What the tracker tells its consumer (the GPU's first-write barrier).
/// Called on whichever thread caused the event, never with the tracker's
/// lock held.
pub trait TrackerEvents: Send + Sync {
    /// The guest (or the host on its behalf) is about to store into
    /// `[pa, pa + len)` on a clean page. The store has not happened; when
    /// this returns the page becomes dirty and the store runs. May block
    /// until GPU work reading the page has finished. A guest fault does not
    /// know the store's size and passes 4, as the reference does.
    fn first_write(&self, pa: u32, len: u32);

    /// An access to a page without guest access (`write`: a store, or
    /// unknown). The handler puts what the memory should hold there and
    /// gives the page back with [`WriteTracker::noaccess`]`(.., false)`. A
    /// page still closed when the guest retries faults here again.
    fn stale_access(&self, pa: u32, write: bool);

    /// The host or a device wrote `[pa, pa + len)` through the RAM view
    /// (from `set_dirty` and `landed`).
    fn host_wrote(&self, _pa: u32, _len: u32) {}

    /// `[pa, pa + len)` was written by the host or a device and has a page
    /// without guest access: the memory is newer than any image guarded over
    /// it (from `set_dirty`).
    fn unguard_range(&self, _pa: u32, _len: u32) {}
}

/// What [`WriteTracker::on_fault`] did with a guest fault.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultOutcome {
    /// A store to a clean page: the first-write event ran and the page is
    /// writable again. Retry the instruction. (A translating CPU backend
    /// also drops code translated from the page.)
    FirstWrite,
    /// An access to a page without guest access: the stale-access event ran.
    /// Retry the instruction.
    StaleAccess,
    /// Nobody holds the page protected now: another thread opened it between
    /// the fault and this look. Retry the instruction.
    Raced,
    /// Not write tracking's fault (outside RAM, or the same address faulted
    /// again and again on an open page): raise it to the title.
    Unhandled,
}

impl FaultOutcome {
    /// Whether the instruction should be retried (`FaultAction::Retry`).
    #[must_use]
    pub fn handled(self) -> bool {
        self != Self::Unhandled
    }
}

/// Counters since creation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TrackerStats {
    /// First-write events (guest faults and host stores on clean pages).
    pub first_writes: u64,
    /// Stale-access events.
    pub stale_accesses: u64,
    /// Guest faults on pages nobody held protected.
    pub spurious_faults: u64,
    /// `PageProtector::protect` calls made.
    pub protect_calls: u64,
}

/// Per-physical-page write tracking over a [`PageProtector`].
///
/// See the module docs for the state machine and the threading rules.
pub struct WriteTracker {
    protector: Arc<dyn PageProtector>,
    state: Box<[AtomicU8]>,
    lock: Mutex<()>,
    events: RwLock<Option<Arc<dyn TrackerEvents>>>,
    /// The last spurious fault: `pa << 32 | count`.
    spurious: AtomicU64,
    first_writes: AtomicU64,
    stale_accesses: AtomicU64,
    spurious_faults: AtomicU64,
    protect_calls: AtomicU64,
}

fn access_of(state: u8) -> Access {
    if state & NOACCESS != 0 {
        Access::None
    } else if state & CLEAN != 0 {
        Access::Read
    } else {
        Access::ReadWrite
    }
}

impl WriteTracker {
    /// A tracker for `ram_size` bytes of RAM with every page dirty and
    /// untracked (every page starts unprotected). The protector is not
    /// called until a page's access changes.
    #[must_use]
    pub fn new(ram_size: u32, protector: Arc<dyn PageProtector>) -> Self {
        Self {
            protector,
            state: (0..ram_size / PAGE_SIZE).map(|_| AtomicU8::new(0)).collect(),
            lock: Mutex::new(()),
            events: RwLock::new(None),
            spurious: AtomicU64::new(0),
            first_writes: AtomicU64::new(0),
            stale_accesses: AtomicU64::new(0),
            spurious_faults: AtomicU64::new(0),
            protect_calls: AtomicU64::new(0),
        }
    }

    /// Registers (or with `None` removes) the consumer of the tracker's
    /// events. Without one, a first write just opens the page and an access
    /// to a page without access gives the page back.
    ///
    /// The consumer usually holds the tracker, so remove it at teardown to
    /// break the cycle.
    pub fn set_events(&self, events: Option<Arc<dyn TrackerEvents>>) {
        *self.events.write().unwrap_or_else(|e| e.into_inner()) = events;
    }

    fn events(&self) -> Option<Arc<dyn TrackerEvents>> {
        self.events.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn guard(&self) -> MutexGuard<'_, ()> {
        self.lock.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Size of the tracked RAM in bytes.
    #[must_use]
    pub fn ram_size(&self) -> u32 {
        (self.state.len() as u32) << PAGE_SHIFT
    }

    fn load(&self, pa: u32) -> Option<u8> {
        self.state.get((pa >> PAGE_SHIFT) as usize).map(|s| s.load(Ordering::Acquire))
    }

    /// The pages of `[pa, pa + len)` that lie in RAM.
    fn pages(&self, pa: u32, len: u32) -> std::ops::Range<usize> {
        let end = (u64::from(pa) + u64::from(len)).min(u64::from(self.ram_size()));
        let first = u64::from(pa >> PAGE_SHIFT);
        let last = end.div_ceil(u64::from(PAGE_SIZE));
        if len == 0 || first >= last { 0..0 } else { first as usize..last as usize }
    }

    fn protect(&self, first: usize, n: usize, access: Access) {
        self.protect_calls.fetch_add(1, Ordering::Relaxed);
        self.protector.protect((first as u32) << PAGE_SHIFT, (n as u32) << PAGE_SHIFT, access);
    }

    /// Applies `f` to the state of each page in `pages` under the lock and
    /// brings the guest's access in line, one `protect` per run of
    /// consecutive pages that get the same access.
    ///
    /// `f` returns the new state, or `None` to leave the page. It must be
    /// idempotent. `tighten` says which way access moves for this operation:
    /// when taking access away, states are stored before the pages are
    /// protected; when giving it back, after (module docs).
    fn transition(
        &self,
        pages: std::ops::Range<usize>,
        tighten: bool,
        mut f: impl FnMut(u8) -> Option<u8>,
    ) {
        if pages.is_empty() {
            return;
        }
        let _g = self.guard();
        let mut run: Option<(usize, usize, Access)> = None;
        let flush = |run: &mut Option<(usize, usize, Access)>,
                     f: &mut dyn FnMut(u8) -> Option<u8>| {
            let Some((start, n, access)) = run.take() else {
                return;
            };
            self.protect(start, n, access);
            if !tighten {
                for s in &self.state[start..start + n] {
                    if let Some(new) = f(s.load(Ordering::Acquire)) {
                        s.store(new, Ordering::Release);
                    }
                }
            }
        };
        for p in pages {
            let s = &self.state[p];
            let old = s.load(Ordering::Acquire);
            let Some(new) = f(old).filter(|&new| new != old) else {
                flush(&mut run, &mut f);
                continue;
            };
            let access = access_of(new);
            if access == access_of(old) {
                s.store(new, Ordering::Release);
                flush(&mut run, &mut f);
                continue;
            }
            if tighten {
                s.store(new, Ordering::Release);
            }
            match &mut run {
                Some((start, n, a)) if *start + *n == p && *a == access => *n += 1,
                _ => {
                    flush(&mut run, &mut f);
                    run = Some((p, 1, access));
                }
            }
        }
        flush(&mut run, &mut f);
    }

    /// Whether the page holding `pa` was written since the last
    /// `test_and_clear`. An untracked page counts as written, and so does an
    /// address outside RAM.
    #[must_use]
    pub fn dirty(&self, pa: u32) -> bool {
        self.load(pa).is_none_or(|s| s & CLEAN == 0 || s & STICKY != 0)
    }

    /// Whether the page holding `pa` is untracked and open: the guest's
    /// stores to it go unseen until a `test_and_clear` or `rearm` protects it
    /// again. A page without guest access is not open (its stores fault).
    /// True outside RAM.
    #[must_use]
    pub fn open(&self, pa: u32) -> bool {
        self.load(pa).is_none_or(|s| s & (CLEAN | NOACCESS) == 0)
    }

    /// Whether the page holding `pa` is without guest access. False outside
    /// RAM.
    #[must_use]
    pub fn has_noaccess(&self, pa: u32) -> bool {
        self.load(pa).is_some_and(|s| s & NOACCESS != 0)
    }

    /// Whether any page of `[pa, pa + len)` was written since the last call,
    /// and from now on every page of it is tracked again: the guest's next
    /// store to each is seen.
    ///
    /// When this returns, the pages are protected: the caller may read them
    /// and will be told (by the next call) of any store made after. False
    /// for a range outside RAM.
    pub fn test_and_clear(&self, pa: u32, len: u32) -> bool {
        let mut written = false;
        self.transition(self.pages(pa, len), true, |s| {
            written |= s & CLEAN == 0 || s & STICKY != 0;
            Some((s | CLEAN) & !STICKY)
        });
        written
    }

    /// `[pa, pa + len)` was written by a device or the host through the RAM
    /// view: its pages become dirty and untracked.
    ///
    /// The consumer hears [`TrackerEvents::host_wrote`] and, if a page of
    /// the range is without guest access, [`TrackerEvents::unguard_range`]
    /// (the memory is newer than the image guarded over it). A page still
    /// without access afterwards stays closed but dirty.
    pub fn set_dirty(&self, pa: u32, len: u32) {
        let pages = self.pages(pa, len);
        if let Some(ev) = self.events() {
            ev.host_wrote(pa, len);
            if self.state[pages.clone()].iter().any(|s| s.load(Ordering::Acquire) & NOACCESS != 0) {
                ev.unguard_range(pa, len);
            }
        }
        // Nothing to open: no lock. The APU's DSP reports 55,000 writes a
        // second to delay lines that are dirty already. A `test_and_clear`
        // racing with this either stored its clean mark before the look
        // (the lock is taken as before) or reports the page written and its
        // caller reads the bytes after, which were stored before this call.
        if self.state[pages.clone()].iter().all(|s| s.load(Ordering::Acquire) & CLEAN == 0) {
            return;
        }
        self.transition(pages, false, |s| (s & CLEAN != 0).then_some(s & !CLEAN));
    }

    /// `[pa, pa + len)` was written by a render target's image landing
    /// there: dirty for the next `test_and_clear`, each page's protection
    /// left as it is. A tracked page still faults at the guest's next store,
    /// and a guarded one keeps its guard.
    pub fn landed(&self, pa: u32, len: u32) {
        if let Some(ev) = self.events() {
            ev.host_wrote(pa, len);
        }
        self.transition(self.pages(pa, len), true, |s| (s & CLEAN != 0).then_some(s | STICKY));
    }

    /// Protects `[pa, pa + len)` again so the guest's next store to each
    /// page is seen ([`TrackerEvents::first_write`]), without losing that
    /// the pages were written: a later `test_and_clear` still reports them.
    pub fn rearm(&self, pa: u32, len: u32) {
        self.transition(self.pages(pa, len), true, |s| {
            (s & CLEAN == 0).then_some(s | CLEAN | STICKY)
        });
    }

    /// Takes all guest access, reads included, from the pages of
    /// `[pa, pa + len)` (`on`), or gives back what write tracking wants. The
    /// host's view keeps its access. As in the reference, a range that
    /// leaves RAM is ignored whole.
    pub fn noaccess(&self, pa: u32, len: u32, on: bool) {
        if len == 0 || u64::from(pa) + u64::from(len) > u64::from(self.ram_size()) {
            return;
        }
        self.transition(self.pages(pa, len), on, |s| {
            Some(if on { s | NOACCESS } else { s & !NOACCESS })
        });
    }

    /// A page's clean bit dropped and its access restored, after a first
    /// write.
    fn make_dirty(&self, page: usize) {
        self.transition(page..page + 1, false, |s| (s & CLEAN != 0).then_some(s & !CLEAN));
    }

    fn first_write(&self, events: Option<&Arc<dyn TrackerEvents>>, pa: u32, len: u32) {
        self.first_writes.fetch_add(1, Ordering::Relaxed);
        if let Some(ev) = events {
            ev.first_write(pa, len);
        }
        self.make_dirty((pa >> PAGE_SHIFT) as usize);
    }

    /// The fault entry: the guest touched physical `pa` (`write`: a store)
    /// in a way the page's protection forbids.
    ///
    /// The machine calls it from `Bus::page_fault` when a mapping exists and
    /// the page table alone allows the access
    /// ([`crate::AddressSpace::allows`]); a store to a page the title itself
    /// made read-only is the title's fault and never comes here.
    ///
    /// * A page without access: [`TrackerEvents::stale_access`], then retry.
    ///   (The reference also serves accesses to a guest's own bytes beside a
    ///   guarded range by emulating the instruction; here every such access
    ///   takes the stale path.)
    /// * A store to a clean page: [`TrackerEvents::first_write`], the page
    ///   made dirty and writable, then retry.
    /// * Anything else is a fault on a page nobody holds protected. Another
    ///   thread may have opened it since the fault (a device write reported
    ///   with `set_dirty`), so the access is retried; the fifth fault in a
    ///   row at the same address is given up as not ours.
    pub fn on_fault(&self, pa: u32, write: bool) -> FaultOutcome {
        let Some(state) = self.load(pa) else {
            return FaultOutcome::Unhandled;
        };
        if state & NOACCESS != 0 {
            self.spurious.store(0, Ordering::Relaxed);
            self.stale_accesses.fetch_add(1, Ordering::Relaxed);
            match self.events() {
                Some(ev) => ev.stale_access(pa, write),
                None => self.noaccess(pa & !(PAGE_SIZE - 1), PAGE_SIZE, false),
            }
            return FaultOutcome::StaleAccess;
        }
        if write && state & CLEAN != 0 {
            self.spurious.store(0, Ordering::Relaxed);
            let len = 4.min(self.ram_size() - pa);
            self.first_write(self.events().as_ref(), pa, len);
            return FaultOutcome::FirstWrite;
        }
        self.spurious_faults.fetch_add(1, Ordering::Relaxed);
        let key = (u64::from(pa) << 32) | 1 << 31;
        let last = self.spurious.load(Ordering::Relaxed);
        let count = if last & !0x7FFF_FFFF == key { (last & 0x7FFF_FFFF) + 1 } else { 1 };
        self.spurious.store(key | count, Ordering::Relaxed);
        if count <= SPURIOUS_RETRIES {
            return FaultOutcome::Raced;
        }
        if count == SPURIOUS_RETRIES + 1 {
            log::warn!(target: "mem", "{} fault at pa {pa:08x}, a page tracking doesn't hold protected",
                if write { "write" } else { "read" });
        }
        FaultOutcome::Unhandled
    }

    /// Asks the consumer for a page without access, as a guest access's
    /// fault would, until it is given back.
    fn stale_open(&self, events: Option<&Arc<dyn TrackerEvents>>, pa: u32, write: bool) {
        let Some(ev) = events else {
            return;
        };
        for _ in 0..STALE_OPEN_TRIES {
            if !self.has_noaccess(pa) {
                break;
            }
            self.stale_accesses.fetch_add(1, Ordering::Relaxed);
            ev.stale_access(pa, write);
        }
    }

    /// `[pa, pa + len)` clipped to each page it touches.
    fn per_page(&self, pa: u32, len: u32) -> impl Iterator<Item = (u32, u32)> + use<> {
        let end = (u64::from(pa) + u64::from(len)).min(u64::from(self.ram_size()));
        let mut a = u64::from(pa);
        std::iter::from_fn(move || {
            if a >= end {
                return None;
            }
            let next = ((a >> PAGE_SHIFT) + 1) << PAGE_SHIFT;
            let piece = (a as u32, (next.min(end) - a) as u32);
            a = next;
            Some(piece)
        })
    }

    /// The host is about to store into `[pa, pa + len)` on the guest's
    /// behalf (a kernel call filling a guest buffer): counted as the guest's
    /// own store would be.
    ///
    /// Call it before writing through the RAM view. For each page: one
    /// without access is asked for first ([`TrackerEvents::stale_access`]),
    /// and a clean one gets [`TrackerEvents::first_write`] and becomes
    /// dirty. Guest-CPU thread, as the events may wait for the GPU.
    pub fn host_store(&self, pa: u32, len: u32) {
        let events = self.events();
        for (a, n) in self.per_page(pa, len) {
            self.stale_open(events.as_ref(), a, true);
            if self.load(a).is_some_and(|s| s & CLEAN != 0) {
                self.first_write(events.as_ref(), a, n);
            }
        }
    }

    /// The host is about to read `[pa, pa + len)` on the guest's behalf (a
    /// kernel call writing a guest buffer to a file): pages without access
    /// get what the consumer puts there first, as a guest read's fault would.
    pub fn host_load(&self, pa: u32, len: u32) {
        let events = self.events();
        for (a, _) in self.per_page(pa, len) {
            self.stale_open(events.as_ref(), a, false);
        }
    }

    /// Counters since creation.
    #[must_use]
    pub fn stats(&self) -> TrackerStats {
        TrackerStats {
            first_writes: self.first_writes.load(Ordering::Relaxed),
            stale_accesses: self.stale_accesses.load(Ordering::Relaxed),
            spurious_faults: self.spurious_faults.load(Ordering::Relaxed),
            protect_calls: self.protect_calls.load(Ordering::Relaxed),
        }
    }
}

impl std::fmt::Debug for WriteTracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WriteTracker").field("pages", &self.state.len()).finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::MockProtector;
    use std::sync::atomic::AtomicBool;

    const P: u32 = PAGE_SIZE;
    const PAGES: u32 = 64;

    fn tracker() -> (Arc<WriteTracker>, Arc<MockProtector>) {
        let prot = Arc::new(MockProtector::new(PAGES));
        (Arc::new(WriteTracker::new(PAGES * P, prot.clone())), prot)
    }

    /// Records events; optionally acts on the tracker from inside them.
    #[derive(Default)]
    struct Recorder {
        log: Mutex<Vec<String>>,
        /// Give a page back from `stale_access`, as the GPU's barrier does.
        reopen: Mutex<Option<Arc<WriteTracker>>>,
    }

    impl Recorder {
        fn take(&self) -> Vec<String> {
            std::mem::take(&mut self.log.lock().unwrap())
        }
    }

    impl TrackerEvents for Recorder {
        fn first_write(&self, pa: u32, len: u32) {
            self.log.lock().unwrap().push(format!("first_write {pa:x} {len}"));
        }
        fn stale_access(&self, pa: u32, write: bool) {
            self.log.lock().unwrap().push(format!("stale {pa:x} {write}"));
            if let Some(t) = &*self.reopen.lock().unwrap() {
                t.noaccess(pa & !(P - 1), P, false);
            }
        }
        fn host_wrote(&self, pa: u32, len: u32) {
            self.log.lock().unwrap().push(format!("host_wrote {pa:x} {len}"));
        }
        fn unguard_range(&self, pa: u32, len: u32) {
            self.log.lock().unwrap().push(format!("unguard {pa:x} {len}"));
        }
    }

    fn with_events(t: &Arc<WriteTracker>, reopen: bool) -> Arc<Recorder> {
        let r = Arc::new(Recorder::default());
        if reopen {
            *r.reopen.lock().unwrap() = Some(t.clone());
        }
        t.set_events(Some(r.clone()));
        r
    }

    /// The module's invariant: protection follows state.
    fn assert_protection_matches(t: &WriteTracker, prot: &MockProtector) {
        for p in 0..PAGES {
            let pa = p * P;
            let want = if t.has_noaccess(pa) {
                Access::None
            } else if t.open(pa) {
                Access::ReadWrite
            } else {
                Access::Read
            };
            assert_eq!(prot.access(pa), want, "page {p}");
        }
    }

    #[test]
    fn every_page_starts_dirty_and_open_and_nothing_is_protected() {
        let (t, prot) = tracker();
        assert!(t.dirty(0) && t.open(0) && !t.has_noaccess(0));
        assert!(prot.take_calls().is_empty());
        // Outside RAM: dirty and open.
        assert!(t.dirty(PAGES * P) && t.open(u32::MAX) && !t.has_noaccess(u32::MAX));
    }

    #[test]
    fn set_dirty_on_pages_already_dirty_takes_no_lock_and_still_tells_the_consumer() {
        let (t, prot) = tracker();
        let r = with_events(&t, false);
        t.test_and_clear(0, 4 * P);
        t.set_dirty(P, 2 * P);
        prot.take_calls();
        r.take();
        {
            // with the lock held elsewhere a second report would wait for good
            let _held = t.guard();
            t.set_dirty(P + 8, P);
        }
        assert_eq!(r.take(), ["host_wrote 1008 4096"]);
        assert!(prot.take_calls().is_empty());
        // a page cleaned since is opened again, as before
        t.test_and_clear(P, P);
        prot.take_calls();
        t.set_dirty(P + 8, P);
        assert_eq!(prot.take_calls(), [(P, P, Access::ReadWrite)]);
        assert!(t.dirty(P) && t.dirty(2 * P) && !t.dirty(0));
        assert_protection_matches(&t, &prot);
    }

    #[test]
    fn test_and_clear_reports_once_and_protects_the_pages_in_one_call() {
        let (t, prot) = tracker();
        assert!(t.test_and_clear(4 * P + 0x10, 3 * P));
        assert_eq!(prot.take_calls(), [(4 * P, 4 * P, Access::Read)]);
        assert!(!t.dirty(4 * P) && !t.dirty(7 * P + 0xFFF) && t.dirty(8 * P));
        assert!(!t.open(5 * P));
        assert!(!t.test_and_clear(4 * P + 0x10, 3 * P));
        assert!(prot.take_calls().is_empty());
        assert_protection_matches(&t, &prot);
    }

    #[test]
    fn test_and_clear_protects_only_the_runs_that_were_open() {
        let (t, prot) = tracker();
        t.test_and_clear(2 * P, P);
        t.test_and_clear(5 * P, P);
        prot.take_calls();
        assert!(t.test_and_clear(0, 8 * P));
        assert_eq!(
            prot.take_calls(),
            [(0, 2 * P, Access::Read), (3 * P, 2 * P, Access::Read), (6 * P, 2 * P, Access::Read)]
        );
    }

    #[test]
    fn ranges_are_clipped_to_ram_and_empty_ranges_do_nothing() {
        let (t, prot) = tracker();
        assert!(t.test_and_clear((PAGES - 1) * P, 16 * P));
        assert_eq!(prot.take_calls(), [((PAGES - 1) * P, P, Access::Read)]);
        assert!(!t.test_and_clear(PAGES * P, P));
        assert!(!t.test_and_clear(0xFFFF_F000, 0x2000));
        assert!(!t.test_and_clear(8 * P, 0));
        t.set_dirty(PAGES * P, P);
        t.rearm(u32::MAX, 16);
        t.landed(PAGES * P + 5, 1);
        // A range that leaves RAM is ignored whole.
        t.noaccess((PAGES - 1) * P, 2 * P, true);
        assert!(!t.has_noaccess((PAGES - 1) * P));
        assert!(prot.take_calls().is_empty());
    }

    #[test]
    fn a_guest_store_to_a_clean_page_reports_first_write_then_opens_the_page() {
        let (t, prot) = tracker();
        let r = with_events(&t, false);
        t.test_and_clear(8 * P, P);
        prot.take_calls();
        assert!(!prot.guest_store(8 * P + 0x40));
        assert!(prot.guest_can_read(8 * P));
        assert_eq!(t.on_fault(8 * P + 0x40, true), FaultOutcome::FirstWrite);
        assert_eq!(r.take(), ["first_write 8040 4"]);
        assert_eq!(prot.take_calls(), [(8 * P, P, Access::ReadWrite)]);
        assert!(prot.guest_store(8 * P + 0x40));
        assert!(t.dirty(8 * P) && t.open(8 * P));
        assert!(t.test_and_clear(8 * P, P));
        assert_eq!(t.stats().first_writes, 1);
    }

    #[test]
    fn the_first_write_event_comes_before_the_page_is_writable() {
        let (t, prot) = tracker();
        struct Check(Arc<MockProtector>, AtomicBool);
        impl TrackerEvents for Check {
            fn first_write(&self, pa: u32, _len: u32) {
                // The barrier waits for the GPU here: the store must not be
                // able to happen yet.
                assert!(!self.0.guest_store(pa));
                self.1.store(true, Ordering::SeqCst);
            }
            fn stale_access(&self, _pa: u32, _write: bool) {}
        }
        let c = Arc::new(Check(prot.clone(), AtomicBool::new(false)));
        t.set_events(Some(c.clone()));
        t.test_and_clear(0, P);
        assert!(t.on_fault(0x10, true).handled());
        assert!(c.1.load(Ordering::SeqCst));
        assert!(prot.guest_store(0x10));
    }

    #[test]
    fn a_fault_without_a_consumer_still_opens_the_page() {
        let (t, prot) = tracker();
        t.test_and_clear(0, P);
        assert_eq!(t.on_fault(0, true), FaultOutcome::FirstWrite);
        assert!(prot.guest_store(0));
        t.noaccess(P, P, true);
        assert_eq!(t.on_fault(P, false), FaultOutcome::StaleAccess);
        assert!(!t.has_noaccess(P));
        assert_protection_matches(&t, &prot);
    }

    #[test]
    fn set_dirty_opens_tracked_pages_and_tells_the_consumer() {
        let (t, prot) = tracker();
        let r = with_events(&t, false);
        t.test_and_clear(0, 4 * P);
        prot.take_calls();
        t.set_dirty(P + 8, P);
        assert_eq!(r.take(), ["host_wrote 1008 4096"]);
        assert_eq!(prot.take_calls(), [(P, 2 * P, Access::ReadWrite)]);
        assert!(!t.dirty(0) && t.dirty(P) && t.dirty(2 * P) && !t.dirty(3 * P));
        // Already dirty: the consumer still hears of the write, the
        // protector of nothing.
        t.set_dirty(P, P);
        assert_eq!(r.take(), ["host_wrote 1000 4096"]);
        assert!(prot.take_calls().is_empty());
        assert_protection_matches(&t, &prot);
    }

    #[test]
    fn rearm_protects_again_without_losing_that_the_pages_were_written() {
        let (t, prot) = tracker();
        let r = with_events(&t, false);
        t.test_and_clear(0, 2 * P);
        t.set_dirty(0, P);
        prot.take_calls();
        r.take();
        t.rearm(0, 2 * P);
        // Only the dirty page changes; the clean one stays clean.
        assert_eq!(prot.take_calls(), [(0, P, Access::Read)]);
        assert!(t.dirty(0) && !t.open(0));
        assert!(!t.dirty(P));
        // The guest's next store is seen...
        assert!(!prot.guest_store(0));
        assert_eq!(t.on_fault(0, true), FaultOutcome::FirstWrite);
        assert_eq!(r.take(), ["first_write 0 4"]);
        // ...and test_and_clear still reports the earlier write, once.
        t.rearm(0, P);
        assert!(t.test_and_clear(0, P));
        assert!(!t.test_and_clear(0, P));
        assert!(!t.test_and_clear(P, P));
        assert_protection_matches(&t, &prot);
    }

    #[test]
    fn a_landing_makes_clean_pages_dirty_but_leaves_them_protected() {
        let (t, prot) = tracker();
        let r = with_events(&t, false);
        t.test_and_clear(0, P);
        prot.take_calls();
        t.landed(0x100, 2 * P);
        assert_eq!(r.take(), ["host_wrote 100 8192"]);
        assert!(prot.take_calls().is_empty());
        assert!(t.dirty(0) && !t.open(0));
        // A tracked page still faults at the guest's next store.
        assert!(!prot.guest_store(0));
        assert!(t.test_and_clear(0, P));
        assert!(!t.dirty(0));
        // An open page stays as it was: dirty already.
        assert!(t.dirty(P) && t.open(P));
    }

    #[test]
    fn noaccess_closes_pages_for_reads_and_gives_back_what_tracking_wants() {
        let (t, prot) = tracker();
        t.test_and_clear(P, P); // page 1 clean, pages 0 and 2 dirty
        prot.take_calls();
        t.noaccess(0x800, 2 * P, true); // rounds out to pages 0..=2
        assert_eq!(prot.take_calls(), [(0, 3 * P, Access::None)]);
        assert!(!prot.guest_can_read(0) && !prot.guest_can_read(2 * P));
        assert!(t.has_noaccess(0) && t.has_noaccess(2 * P) && !t.has_noaccess(3 * P));
        // A page without access is not open.
        assert!(!t.open(0));
        // Whether it is dirty is unaffected.
        assert!(t.dirty(0) && !t.dirty(P));
        t.noaccess(0, 3 * P, false);
        assert_eq!(
            prot.take_calls(),
            [(0, P, Access::ReadWrite), (P, P, Access::Read), (2 * P, P, Access::ReadWrite)]
        );
        assert_protection_matches(&t, &prot);
    }

    #[test]
    fn tracking_changes_never_open_a_page_without_access() {
        // Pages without access stay so.
        let (t, prot) = tracker();
        t.noaccess(P, P, true);
        prot.take_calls();
        assert!(t.test_and_clear(0, 3 * P));
        assert_eq!(prot.take_calls(), [(0, P, Access::Read), (2 * P, P, Access::Read)]);
        assert_eq!(prot.access(P), Access::None);
        t.set_dirty(0, 3 * P);
        assert_eq!(prot.access(P), Access::None);
        assert!(t.dirty(P));
        t.rearm(0, 3 * P);
        assert_eq!(prot.access(P), Access::None);
        // Given back, it gets what tracking wants by then: clean.
        t.noaccess(P, P, false);
        assert_eq!(prot.access(P), Access::Read);
        assert_protection_matches(&t, &prot);
    }

    #[test]
    fn an_access_to_a_page_without_access_goes_to_the_stale_handler() {
        let (t, prot) = tracker();
        let r = with_events(&t, true);
        t.noaccess(3 * P, P, true);
        assert_eq!(t.on_fault(3 * P + 4, false), FaultOutcome::StaleAccess);
        assert_eq!(r.take(), ["stale 3004 false"]);
        assert!(prot.guest_can_read(3 * P));
        // A handler that keeps the page closed sees the retry fault again.
        *r.reopen.lock().unwrap() = None;
        t.noaccess(3 * P, P, true);
        assert_eq!(t.on_fault(3 * P, true), FaultOutcome::StaleAccess);
        assert_eq!(t.on_fault(3 * P, true), FaultOutcome::StaleAccess);
        assert_eq!(r.take(), ["stale 3000 true", "stale 3000 true"]);
        assert_eq!(t.stats().stale_accesses, 3);
    }

    #[test]
    fn a_device_write_over_a_guarded_page_drops_the_guard_first() {
        let (t, _prot) = tracker();
        let r = with_events(&t, false);
        t.noaccess(2 * P, P, true);
        t.set_dirty(P, 2 * P);
        assert_eq!(r.take(), ["host_wrote 1000 8192", "unguard 1000 8192"]);
        t.set_dirty(4 * P, P);
        assert_eq!(r.take(), ["host_wrote 4000 4096"]);
    }

    #[test]
    fn a_fault_on_an_open_page_is_retried_a_few_times_then_given_up() {
        // A store that faults again and again at the same address is someone
        // else's protection.
        let (t, _prot) = tracker();
        for _ in 0..4 {
            assert_eq!(t.on_fault(0x1234, true), FaultOutcome::Raced);
        }
        assert_eq!(t.on_fault(0x1234, true), FaultOutcome::Unhandled);
        assert!(!FaultOutcome::Unhandled.handled() && FaultOutcome::Raced.handled());
        // Another address starts a new count...
        assert_eq!(t.on_fault(0x1238, true), FaultOutcome::Raced);
        // ...and so does a tracked page's fault in between.
        for _ in 0..3 {
            assert_eq!(t.on_fault(0x1238, true), FaultOutcome::Raced);
        }
        t.test_and_clear(5 * P, P);
        assert_eq!(t.on_fault(5 * P, true), FaultOutcome::FirstWrite);
        assert_eq!(t.on_fault(0x1238, true), FaultOutcome::Raced);
        // A read fault on a clean page is not tracking's either.
        t.test_and_clear(6 * P, P);
        assert_eq!(t.on_fault(6 * P, false), FaultOutcome::Raced);
        assert_eq!(t.on_fault(PAGES * P, true), FaultOutcome::Unhandled);
    }

    #[test]
    fn a_host_store_counts_as_the_guest_s_own() {
        let (t, prot) = tracker();
        let r = with_events(&t, true);
        t.test_and_clear(0, 2 * P); // pages 0, 1 clean; 2 open
        t.noaccess(P, P, true); // page 1 guarded as well
        prot.take_calls();
        t.host_store(0xFF0, 2 * P + 0x20);
        assert_eq!(r.take(), ["first_write ff0 16", "stale 1000 true", "first_write 1000 4096"]);
        assert!(t.open(0) && t.open(P) && t.open(2 * P));
        assert_protection_matches(&t, &prot);
        // Nothing to do the second time.
        t.host_store(0xFF0, 2 * P + 0x20);
        assert!(r.take().is_empty());
    }

    #[test]
    fn a_host_load_asks_for_pages_without_access_only() {
        let (t, prot) = tracker();
        let r = with_events(&t, true);
        t.test_and_clear(0, 2 * P);
        t.noaccess(P, P, true);
        t.host_load(0x10, 2 * P);
        assert_eq!(r.take(), ["stale 1000 false"]);
        // The clean pages stay clean: a read is not a write.
        assert!(!t.dirty(0) && !t.dirty(P));
        assert_protection_matches(&t, &prot);
    }

    #[test]
    fn a_host_access_gives_up_on_a_page_the_handler_never_opens() {
        // 16 tries.
        let (t, _prot) = tracker();
        let r = with_events(&t, false);
        t.noaccess(0, P, true);
        t.host_load(0, 1);
        assert_eq!(r.take().len(), 16);
    }

    #[test]
    fn events_may_call_back_into_the_tracker() {
        // The GPU's barrier re-arms and reopens pages from inside its handlers.
        let (t, prot) = tracker();
        struct Reenter(Mutex<Option<Arc<WriteTracker>>>);
        impl TrackerEvents for Reenter {
            fn first_write(&self, pa: u32, _len: u32) {
                let t = self.0.lock().unwrap().clone().unwrap();
                t.rearm(pa & !(P - 1), P);
                assert!(!t.dirty(pa + P));
                t.test_and_clear(pa + P, P);
            }
            fn stale_access(&self, pa: u32, _write: bool) {
                let t = self.0.lock().unwrap().clone().unwrap();
                t.noaccess(pa, P, false);
                t.set_dirty(pa, P);
            }
            fn host_wrote(&self, pa: u32, _len: u32) {
                let t = self.0.lock().unwrap().clone().unwrap();
                let _ = t.dirty(pa);
            }
        }
        let ev = Arc::new(Reenter(Mutex::new(Some(t.clone()))));
        t.set_events(Some(ev.clone()));
        t.test_and_clear(0, 2 * P);
        assert!(t.on_fault(0, true).handled());
        t.noaccess(4 * P, P, true);
        assert!(t.on_fault(4 * P, false).handled());
        assert_protection_matches(&t, &prot);
        *ev.0.lock().unwrap() = None;
        t.set_events(None);
    }

    /// A guest store as the machine performs it: a fault goes to the tracker
    /// and the store is retried.
    fn guest_store(t: &WriteTracker, prot: &MockProtector, pa: u32) {
        let mut tries = 0;
        while !prot.guest_store(pa) {
            assert!(t.on_fault(pa, true).handled(), "the tracker gave up a fault it caused");
            tries += 1;
            assert!(tries < 1000, "a store never got through");
        }
    }

    #[test]
    fn a_store_racing_with_test_and_clear_is_never_lost() {
        // The guarantee the lock's ordering exists for. A guest thread stores
        // to a few pages while a third thread throws device writes, re-arms
        // and landings at them. Two GPU threads test-and-clear their own
        // pages and sample each page's store count before and after every
        // call. A call that says "not written" means no store happened since
        // the previous call, so the count sampled after the previous call
        // and the one sampled before this call must be equal.
        let (t, prot) = tracker();
        let stop = AtomicBool::new(false);
        const N: u32 = 8;
        std::thread::scope(|s| {
            s.spawn(|| {
                let mut i = 0u32;
                loop {
                    guest_store(&t, &prot, (i % N) * P + 0x10);
                    i = i.wrapping_add(1);
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                }
            });
            s.spawn(|| {
                let mut i = 0u32;
                while !stop.load(Ordering::Relaxed) {
                    match i % 3 {
                        0 => t.set_dirty((i % N) * P, P),
                        1 => t.rearm((i % N) * P, 2 * P),
                        _ => t.landed((i % N) * P, P),
                    }
                    i = i.wrapping_add(1);
                }
            });
            let observers: Vec<_> = (0..2u32)
                .map(|o| {
                    let (t, prot) = (&t, &prot);
                    s.spawn(move || {
                        let mine: Vec<u32> = (0..N).filter(|p| p % 2 == o).collect();
                        let mut after_last = vec![0u64; N as usize];
                        let mut clean_reports = 0u64;
                        for round in 0..40_000u32 {
                            let p = mine[round as usize % mine.len()];
                            let before = prot.stores(p * P);
                            if !t.test_and_clear(p * P, P) {
                                clean_reports += 1;
                                assert_eq!(
                                    before, after_last[p as usize],
                                    "a store to page {p} went unseen"
                                );
                            }
                            after_last[p as usize] = prot.stores(p * P);
                        }
                        clean_reports
                    })
                })
                .collect();
            // The other threads are stopped before a failed observer is
            // reported, or the scope would wait for them for ever.
            let results: Vec<_> = observers.into_iter().map(|h| h.join()).collect();
            stop.store(true, Ordering::Relaxed);
            let clean: u64 = results.into_iter().map(|r| r.unwrap()).sum();
            // The check above was exercised, not skipped.
            assert!(clean > 0, "no call ever reported a clean page");
        });
        // Quiescent: protection agrees with state, and stores happened.
        assert_protection_matches(&t, &prot);
        assert!((0..N).map(|p| prot.stores(p * P)).sum::<u64>() > 0);
    }

    #[test]
    fn concurrent_callers_leave_protection_agreeing_with_state() {
        // Every entry point from several threads at once, noaccess included;
        // the stale handler reopens pages as the GPU's barrier does. Two
        // racing calls must not leave a page's protection and state
        // disagreeing (protected but unmarked), which is checked
        // each time the threads have stopped: a later call may repair a
        // disagreement, so checking only at the end would miss most.
        let (t, prot) = tracker();
        with_events(&t, true);
        prot.slow.store(true, Ordering::Relaxed);
        for round in 0..400u32 {
            std::thread::scope(|s| {
                for th in 0..4u32 {
                    let (t, prot) = (&t, &prot);
                    s.spawn(move || {
                        let mut seed = (0x1234_5678u32 ^ th.wrapping_mul(0x9E37_79B9))
                            .wrapping_add(round.wrapping_mul(0x85EB_CA6B))
                            | 1;
                        for _ in 0..48 {
                            seed ^= seed << 13;
                            seed ^= seed >> 17;
                            seed ^= seed << 5;
                            let pa = (seed >> 8) % (4 * P);
                            let len = 1 + (seed >> 3) % (2 * P);
                            match seed % 9 {
                                0 => {
                                    t.test_and_clear(pa, len);
                                }
                                1 => t.set_dirty(pa, len),
                                2 => t.rearm(pa, len),
                                3 => t.landed(pa, len),
                                4 => t.noaccess(pa, len, true),
                                5 => t.noaccess(pa, len, false),
                                6 => t.host_store(pa, len),
                                7 => {
                                    let _ = (t.dirty(pa), t.open(pa));
                                    t.host_load(pa, len);
                                }
                                _ => {
                                    if !prot.guest_store(pa) {
                                        t.on_fault(pa, true);
                                    }
                                }
                            }
                        }
                    });
                }
            });
            assert_protection_matches(&t, &prot);
        }
        t.set_events(None);
    }
}
