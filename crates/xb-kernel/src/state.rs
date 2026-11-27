//! The kernel's part of a savestate.
//!
//! The split into what is state and what a boot makes again is the
//! reference's. The encoding is `xb_types::state`.
//!
//! A state is taken and put back at a pause of [`Kernel::run`] (it returned
//! [`crate::RunOutcome::Paused`]), into a kernel that booted the same title:
//! the kernel's area, the export entry points and the title's image are a
//! boot's, the same every time, and are not in the state. Guest RAM is the
//! machine's section, saved before this one.
//!
//! Open files are kept as their path and position and opened again on load;
//! their contents are the volumes'. A request a drive had finished but not
//! yet delivered is kept whole; one still with a drive is waited for first.

use xb_types::irq::Irq;
use xb_types::state::{StateIo, Stateful};

use crate::kernel::Kernel;

/// What a state takes for granted of the machine it is loaded into, as a
/// number: bump it when a boot comes to lay the guest's memory out
/// differently in a way [`Kernel::state_layout`] cannot see (what the
/// loader patches into a title's code, the stacks' placement rule, a
/// structure the kernel keeps in guest memory changing shape). A state of
/// another number is refused.
pub const STATE_LAYOUT: u32 = 1;

/// The interrupt lines, for the hub's part.
const LINES: [Irq; 7] = [Irq::Timer, Irq::Usb0, Irq::Gpu, Irq::Nic, Irq::Apu, Irq::Aci, Irq::Usb1];

impl Kernel {
    /// The identity of what this boot made that a state does not hold and
    /// its guest memory points into: [`STATE_LAYOUT`] and the addresses of
    /// the kernel's area (every export's entry point and data, the object
    /// types, the KPCR, the GDT and TSS, the pool) and of the title's
    /// image. A state is loaded only into a machine with the same one, so
    /// a build that moves any of them refuses older states instead of
    /// running them into the ground.
    #[must_use]
    pub fn state_layout(&self) -> u64 {
        let words = [STATE_LAYOUT, self.kpcr_va, self.gdt_va, self.tss_va]
            .into_iter()
            .chain(self.tramp_va.iter().copied())
            .chain(self.data_va.iter().copied())
            .chain(self.type_va.iter().copied())
            .chain([self.pool.first().0, self.pool.first().1])
            .chain([self.title.base, self.title.image_size]);
        words.flat_map(u32::to_le_bytes).fold(0xCBF2_9CE4_8422_2325, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01B3)
        })
    }

    /// Saves the kernel, or replaces it with the stream's. Call at a pause,
    /// and [`Kernel::state_quiesce`] before a save. After a load the caller
    /// brings the CPU up to date ([`Kernel::sync_cpu`]).
    pub fn state(&mut self, io: &mut StateIo) {
        let loading = io.loading();
        io.section(*b"VM  ", 1, |io, _| {
            self.gm.state(io);
        });
        io.section(*b"KERN", 1, |io, _| {
            let mut now = self.clock.now_ns();
            now.state(io);
            if loading && io.failed().is_none() {
                self.clock.set_now(now);
            }
            self.code_next.state(io);
            self.frame.state(io);
            self.call_pop.state(io);
            self.redirected.state(io);
            self.in_call.state(io);
            self.calls.state(io);
            self.first_ordinal.state(io);
            self.irq_window.state(io);
            self.dev_due.state(io);
            self.dev_started.state(io);
            self.apu_spins.state(io);
            self.current.state(io);
            self.resched.state(io);
            self.kprocess.state(io);
            self.threads_hi.state(io);
            self.sched.state(io);
            self.pool.state(io);
            self.ob.state(io);
            self.ke.state(io);
            self.intr.state(io);
            self.ex.state(io);
            // The lines asserted, and the flips a held thread counts from.
            let mut lines = self.hub.pending();
            lines.state(io);
            let mut flips = self.flip_hold.flips();
            flips.state(io);
            if loading && io.failed().is_none() {
                for line in LINES {
                    self.hub.set(line, lines & line.mask() != 0);
                }
                let held = self.threads.iter().any(|t| t.wait.hold.is_some());
                self.flip_hold.restore(flips, held);
            }
        });
        io.section(*b"THRD", 1, |io, _| {
            let mut n = self.threads_hi;
            n.state(io);
            if loading {
                if n > self.threads.len() {
                    io.fail("the state has more threads than this kernel's table");
                    return;
                }
                for t in &mut self.threads {
                    *t = crate::ps::Thread::default();
                }
            }
            for t in &mut self.threads[..n] {
                t.state(io);
            }
        });
        io.section(*b"TIMR", 1, |io, _| self.timer.state(io));
        io.section(*b"MM  ", 1, |io, _| self.mm.state(io));
        io.section(*b"FILE", 1, |io, _| self.io_state(io));
        if loading && io.failed().is_none() {
            self.stop = None;
            self.stats = crate::kernel::Stats {
                count: vec![0; crate::exports::ORDINAL_MAX],
                ..Default::default()
            };
            self.ticker.refreshed();
            self.clock_arm();
        }
    }
}
