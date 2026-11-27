//! The frame-latency hold: a thread that finished a frame while another
//! still waits for the display is held until the next flip.
//!
//! The host CPU runs a title's code several times faster than the
//! console's, so the title finishes frames ahead of the display until its
//! push buffer is full, and then goes on wherever in a frame the room ran
//! out. The renderer sees a frame's end pushed while the frame before waits
//! for its flip and asks for the hold (`hle_host_frame_wait`); the thread
//! then waits, as a slower CPU would still have been drawing, and each
//! frame begins at a flip. The request comes from the guest-CPU thread or
//! the renderer's and is taken at the kernel's next kicked entry, so the
//! thread is held between two instructions with its registers as they are.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::kernel::Kernel;
use crate::ps::{ThreadState, Wait};

/// The longest a thread is held for a flip that does not come.
const FRAME_HOLD_MAX_NS: u64 = 50_000_000;
/// The IRQL from which a thread is not held (`DISPATCH_LEVEL`: the flip it
/// would wait for comes by the vblank's ISR and DPC).
const DISPATCH_LEVEL: u8 = 2;

#[derive(Debug, Default)]
struct Shared {
    /// Flips so far.
    flips: AtomicU32,
    /// A hold asked for: the flip count it was asked at, plus one; 0: none.
    request: AtomicU32,
    /// A thread is held.
    held: AtomicBool,
    /// Holds that ended at their deadline with no flip come.
    timeouts: AtomicU32,
}

/// The renderer's side of the hold: asks for one and reports flips. Clones
/// share one state. Callable from any thread.
#[derive(Clone, Debug, Default)]
pub struct FrameHold(Arc<Shared>);

impl FrameHold {
    /// Asks that the running thread be held until the next flip. The caller
    /// then brings the guest CPU back to the kernel (a kick), where the
    /// request is taken.
    pub fn request(&self) {
        let flips = self.0.flips.load(Ordering::Acquire);
        self.0.request.store(flips.wrapping_add(1), Ordering::Release);
    }

    /// A flip came: a held thread goes on at the kernel's next kicked
    /// entry. True if one is held, and the caller should kick the guest CPU
    /// for it. (The reference leaves the thread to the next thing signaled,
    /// which D3D's vblank DPC provides; a title without one would sit out
    /// the hold's 50 ms.)
    pub fn flipped(&self) -> bool {
        self.0.flips.fetch_add(1, Ordering::AcqRel);
        self.0.held.load(Ordering::Acquire)
    }

    /// The holds so far that ended at their longest with no flip come. The
    /// renderer gives the holds up while this count grows
    /// (`hle_host_frame_holds_timed_out`), as the reference does: a title
    /// whose flips do not come by its holds only loses 50 ms to each (Crazy
    /// Taxi 3 after a few levels).
    #[must_use]
    pub fn timed_out(&self) -> u32 {
        self.0.timeouts.load(Ordering::Acquire)
    }

    /// A savestate's flip count, and whether a thread it holds waits on it.
    pub(crate) fn restore(&self, flips: u32, held: bool) {
        self.0.flips.store(flips, Ordering::Release);
        self.0.request.store(0, Ordering::Release);
        self.0.held.store(held, Ordering::Release);
    }

    /// The flip count a wait compares with.
    pub(crate) fn flips(&self) -> u32 {
        self.0.flips.load(Ordering::Acquire)
    }

    /// The pending request's flip count, taken.
    fn take(&self) -> Option<u32> {
        match self.0.request.swap(0, Ordering::AcqRel) {
            0 => None,
            r => Some(r.wrapping_sub(1)),
        }
    }
}

impl Kernel {
    /// The handle the renderer's host asks for holds and reports flips with.
    #[must_use]
    pub fn frame_hold(&self) -> FrameHold {
        self.flip_hold.clone()
    }

    /// Takes a hold that was asked for: the running thread waits for the
    /// next flip, if it is a thread's own code below `DISPATCH_LEVEL` (not
    /// an ISR's or a DPC's, nor a callout) and the flip has not come
    /// meanwhile.
    pub(crate) fn frame_take(&mut self) {
        let Some(seq) = self.flip_hold.take() else { return };
        let Some(cur) = self.current else { return };
        let t = &self.threads[cur];
        if t.state != ThreadState::Running
            || t.suspend != 0
            || self.intr.in_isr
            || self.timer.in_dpc
            || Some(cur) == self.intr.isr_thread
            || Some(cur) == self.timer.dpc_thread
            || self.redirected
            || self.irql() >= DISPATCH_LEVEL
            || self.flip_hold.flips() != seq
        {
            return;
        }
        let deadline = self.now() + FRAME_HOLD_MAX_NS;
        self.threads[cur].wait =
            Wait { hold: Some(seq), deadline: Some(deadline), ..Wait::default() };
        if self.ke.wait_due.is_none_or(|d| deadline < d) {
            self.ke.wait_due = Some(deadline);
            self.clock_arm();
        }
        self.threads[cur].state = ThreadState::Waiting;
        self.resched = true;
        self.stats.frame_holds += 1;
        self.flip_hold.0.held.store(true, Ordering::Release);
    }

    /// At a kicked entry: a held thread whose flip came goes on.
    pub(crate) fn frame_release(&mut self) {
        if self.flip_hold.0.held.load(Ordering::Acquire) {
            self.check_waits(0);
            self.resched = true;
        }
    }

    /// A hold ended (`check_waits`); `timed_out`: at its deadline, no flip
    /// come.
    pub(crate) fn frame_hold_ended(&mut self, timed_out: bool) {
        self.flip_hold.0.held.store(false, Ordering::Release);
        if timed_out {
            self.flip_hold.0.timeouts.fetch_add(1, Ordering::AcqRel);
        }
    }
}
