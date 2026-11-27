//! The NV2A as the guest sees it: the register window at `0xFD00_0000`, its
//! interrupt line, the vertical blank and PTIMER's alarm.
//!
//! This is a register file (reads return what was last written) plus the
//! behavior the XDK's Direct3D needs:
//!
//! | Register | Offset | Behavior |
//! |---|---|---|
//! | `PMC_BOOT_0` | `0x000000` | chip identification |
//! | `PMC_INTR_0` | `0x000100` | pending summary (bit 24 PCRTC, 20 PTIMER, 12 PGRAPH), read-only |
//! | `PMC_INTR_EN_0` | `0x000140` | bit 0: interrupts to the CPU |
//! | `PFIFO_RUNOUT_STATUS`, `CACHE1_STATUS`, `CACHE1_DMA_PUSH` | `0x002400`, `0x003214`, `0x003220` | idle unless the decoder is behind |
//! | `PVIDEO_BUFFER`, `PVIDEO_STOP` | `0x008700`, `0x008704` | the video overlay's handshake |
//! | `PTIMER` | `0x009100`..`0x009420` | guest time, alarm interrupt |
//! | `PFB_CSTATUS` | `0x10020C` | installed memory |
//! | `PGRAPH_INTR`, `_EN`, `INCREMENT`, `FIFO` | `0x400100`.. | traps, the flip counters, FIFO access |
//! | `PCRTC_INTR_0`, `_EN`, `START`, `RASTER` | `0x600100`.. | vblank, the frame buffer shown, the scanline |
//! | `PRMDIO` palette | `0x6813C8` | the gamma ramp |
//! | `NV_USER` | `0x800040`.. | channel 0's DMA_PUT/GET/REF; a PUT write kicks the decoder |
//!
//! The renderer consumes the push buffer, so the FIFO looks idle whenever
//! Direct3D waits for it. Once the renderer has taken the channel
//! ("push-buffer mode", [`Nv2a::set_pb_mode`]) the FIFO registers follow the
//! decoder's GET and the registers above call into it ([`Decoder`]).
//!
//! The type is generic over the decoder and the clock so its behavior can be
//! tested without the renderer or real time.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use xb_types::irq::{Irq, IrqHub};

use crate::regs::{NV2A_SIZE, RegFile, reg};

/// One frame at 59.94 Hz, nanoseconds.
pub const FRAME_NS_60: u64 = 16_683_333;
/// One frame at 50 Hz, nanoseconds.
pub const FRAME_NS_50: u64 = 20_000_000;
/// Scanlines per frame as `PCRTC_RASTER` counts them.
const LINES: u64 = 525;

/// An alarm the guest's time passed less than this ago fires at once instead
/// of after the low word wraps (4.3 s): the guest set it late and Direct3D's
/// handler checks the 64-bit time itself.
const ALARM_LATE_NS: u32 = 1_000_000_000;
/// How often the alarm thread looks at the guest's clock again while it
/// waits (the clock can stop or jump).
const ALARM_POLL_NS: u64 = 20_000_000;

/// The renderer's push-buffer decoder as the register front end drives it
/// (the `hle_pb_*` functions of `pb/pbmode.h` and `pb/pbdraw.h`). Every
/// method may be called from any thread.
pub trait Decoder: Send + Sync {
    /// The decoder should look at the FIFO again (`hle_pb_kick`).
    fn kick(&self);
    /// The guest wrote channel 0's DMA_PUT (`hle_pb_put`).
    fn put(&self);
    /// The guest wrote a DMA_GET (`hle_pb_get`).
    fn get(&self);
    /// The guest is about to make DMA_PUT `put` (`hle_pb_pushed`).
    fn pushed(&self, put: u32);
    /// The guest wrote CACHE1's DMA_SUBROUTINE (`hle_pb_subroutine_written`).
    fn subroutine_written(&self, value: u32);
    /// The guest finished programming the DAC palette (`hle_pb_gamma`).
    fn gamma(&self, rgb: &[[u8; 256]; 3]);
    /// The guest advanced the flip read counter (`hle_pb_flip_read`).
    fn flip_read(&self);
    /// ... with the display starting at `start` (`hle_pb_flipped`).
    fn flipped(&self, start: u32);
    /// The guest showed or stopped the video overlay
    /// (`hle_pb_overlay_changed`).
    fn overlay_changed(&self, show: bool);
    /// The guest wrote `PCRTC_START` (`hle_pb_display_start`).
    fn display_start(&self, pa: u32);
    /// A vertical blank came (`hle_pb_vblank`).
    fn vblank(&self);
}

/// The clocks the front end runs on.
pub trait Clock: Send + Sync {
    /// The host's monotonic clock, nanoseconds (the renderer's own clock).
    fn now_ns(&self) -> u64;
    /// The guest's clock, nanoseconds ([`crate::GpuHost::guest_time_ns`]).
    fn guest_ns(&self) -> u64;
    /// The host instant at which the guest's clock reads `guest_ns`
    /// ([`crate::GpuHost::guest_to_host_ns`]).
    fn guest_to_host_ns(&self, guest_ns: u64) -> u64;
    /// Sleeps until `host_ns` on the host clock.
    fn sleep_until(&self, host_ns: u64);
}

/// The DAC palette as the guest programs it: an index write, then R, G, B
/// bytes for each entry.
struct Dac {
    index: u8,
    comp: u8,
    rgb: [[u8; 256]; 3],
}

/// PTIMER's alarm, shared with its thread.
pub(crate) struct Alarm {
    /// The guest wrote `ALARM_0` at least once; before that no alarm fires.
    pub(crate) armed: bool,
    /// `ALARM_0` was written (or the emulation resumed): compute `due` again.
    pub(crate) rearm: bool,
    /// Guest time at which the alarm fires next.
    pub(crate) due: u64,
}

/// The NV2A register front end. One per machine.
pub struct Nv2a<D: Decoder, C: Clock> {
    pub(crate) regs: RegFile,
    pub(crate) decoder: D,
    pub(crate) clock: C,
    irq: Arc<IrqHub>,
    /// Makes "look at the registers, set the line" one step. The vblank and
    /// alarm threads, the decoder (a trap) and the guest (its handler
    /// clearing what it handled) each change a register and then evaluate the
    /// line; done apart, a line could be left low with an interrupt pending.
    irq_mu: Mutex<()>,
    /// The renderer consumes channel 0's FIFO.
    pub(crate) pbmode: AtomicBool,
    dac: Mutex<Dac>,

    pub(crate) vblanks: AtomicU64,
    pub(crate) frame_ns: AtomicU64,
    /// When the last vblank was due (host ns); the raster counts from it.
    pub(crate) last_vblank_ns: AtomicU64,
    mode_hz: AtomicU32,
    av_mode: AtomicU32,
    av_title: AtomicU32,

    // The display's vsyncs and the flip pacing; see `timing.rs`.
    pub(crate) disp_vsync_ns: AtomicU64,
    pub(crate) disp_period_ns: AtomicU64,
    pub(crate) disp_nominal_ns: AtomicU64,
    pub(crate) sync_vsync_ns: AtomicU64,
    pub(crate) sync_period_ns: AtomicU64,
    pub(crate) sync_lead_ns: AtomicU64,
    pub(crate) sync_fed_ns: AtomicU64,
    pub(crate) vblank_locked: AtomicBool,
    pub(crate) vblank_timed: AtomicBool,
    pub(crate) pick_off_ns: AtomicI64,
    pub(crate) pick_turn: Mutex<crate::timing::PickTurn>,
    pub(crate) trap_slot_ns: AtomicU64,
    pub(crate) trap_every_ns: AtomicU64,
    pub(crate) slot_given_ns: AtomicU64,
    pub(crate) late_wakes: AtomicU32,

    pub(crate) alarm: Mutex<Alarm>,
    pub(crate) alarm_cv: Condvar,
    paused: AtomicBool,
    pause_mu: Mutex<()>,
    pause_cv: Condvar,
    stop: AtomicBool,
}

/// Locks `m`, ignoring poisoning: the state behind these locks stays
/// consistent across a panic (plain values, no invariants between fields).
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl<D: Decoder, C: Clock> Nv2a<D, C> {
    /// A front end with the registers at their power-on values: the chip
    /// identification and `ram_size` bytes of memory.
    pub fn new(irq: Arc<IrqHub>, decoder: D, clock: C, ram_size: u32) -> Self {
        let regs = RegFile::new();
        regs.store(reg::PMC_BOOT_0, 0x02A0_00A1);
        regs.store(reg::PFB_CSTATUS, ram_size);
        Self {
            regs,
            decoder,
            clock,
            irq,
            irq_mu: Mutex::new(()),
            pbmode: AtomicBool::new(false),
            dac: Mutex::new(Dac { index: 0, comp: 0, rgb: [[0; 256]; 3] }),
            vblanks: AtomicU64::new(0),
            frame_ns: AtomicU64::new(FRAME_NS_60),
            last_vblank_ns: AtomicU64::new(0),
            mode_hz: AtomicU32::new(60),
            av_mode: AtomicU32::new(0),
            av_title: AtomicU32::new(0),
            disp_vsync_ns: AtomicU64::new(0),
            disp_period_ns: AtomicU64::new(0),
            disp_nominal_ns: AtomicU64::new(0),
            sync_vsync_ns: AtomicU64::new(0),
            sync_period_ns: AtomicU64::new(0),
            sync_lead_ns: AtomicU64::new(0),
            sync_fed_ns: AtomicU64::new(0),
            vblank_locked: AtomicBool::new(false),
            vblank_timed: AtomicBool::new(false),
            pick_off_ns: AtomicI64::new(-1),
            pick_turn: Mutex::new(crate::timing::PickTurn::default()),
            trap_slot_ns: AtomicU64::new(0),
            trap_every_ns: AtomicU64::new(0),
            slot_given_ns: AtomicU64::new(0),
            late_wakes: AtomicU32::new(0),
            alarm: Mutex::new(Alarm { armed: false, rearm: true, due: 0 }),
            alarm_cv: Condvar::new(),
            paused: AtomicBool::new(false),
            pause_mu: Mutex::new(()),
            pause_cv: Condvar::new(),
            stop: AtomicBool::new(false),
        }
    }

    // ---- the interrupt line ----

    /// `PMC_INTR_0`: which units have an enabled interrupt pending.
    fn pmc_intr(&self) -> u32 {
        let r = &self.regs;
        let mut v = 0;
        if r.load(reg::PCRTC_INTR_0) & r.load(reg::PCRTC_INTR_EN_0) != 0 {
            v |= reg::PMC_INTR_PCRTC;
        }
        // PGRAPH's interrupts are the decoder's software traps.
        if self.pb_mode() && r.load(reg::PGRAPH_INTR) & r.load(reg::PGRAPH_INTR_EN) != 0 {
            v |= reg::PMC_INTR_PGRAPH;
        }
        if r.load(reg::PTIMER_INTR_0) & r.load(reg::PTIMER_INTR_EN_0) & reg::PTIMER_ALARM != 0 {
            v |= reg::PMC_INTR_PTIMER;
        }
        v
    }

    /// Sets the interrupt line to what the registers say. Any thread, after
    /// changing an interrupt status or enable register.
    pub fn update_irq(&self) {
        let _one_step = lock(&self.irq_mu);
        let summary = self.pmc_intr();
        // Kept in the register file too: a backend that lets the guest read
        // the page directly never calls `mmio_read`.
        self.regs.store(reg::PMC_INTR_0, summary);
        let level = self.regs.load(reg::PMC_INTR_EN_0) & 1 != 0 && summary != 0;
        self.irq.set(Irq::Gpu, level);
    }

    /// Brings the stored `PMC_INTR_0` summary in line with the units'
    /// registers without touching the line.
    fn refresh_summary(&self) {
        let _one_step = lock(&self.irq_mu);
        self.regs.store(reg::PMC_INTR_0, self.pmc_intr());
        self.regs.store(reg::PFB_WBC, 0);
    }

    /// Whether `off` is a register `PMC_INTR_0` is computed from.
    #[inline]
    fn feeds_summary(off: u32) -> bool {
        matches!(
            off & !3,
            reg::PCRTC_INTR_0
                | reg::PCRTC_INTR_EN_0
                | reg::PGRAPH_INTR
                | reg::PGRAPH_INTR_EN
                | reg::PTIMER_INTR_0
                | reg::PTIMER_INTR_EN_0
        )
    }

    /// The ranges of the register window, as `(offset, length, host
    /// address)`, whose reads have no behaviour beyond "what the register
    /// file holds": a CPU backend may map them for the guest to read
    /// without leaving it. Writes still go to [`Nv2a::mmio_write`]. The
    /// summary of interrupts and the FIFO's computed reads are kept in the
    /// file as they change; left out are the pages with reads computed from
    /// the time (PTIMER, the raster).
    pub fn read_through(&self) -> Vec<(u32, u32, *const u8)> {
        self.refresh_summary();
        self.refresh_fifo();
        // All but PTIMER's page (the time) and PCRTC's (the raster).
        [(0x00_0000u32, 0x00_9000u32), (0x00_A000, 0x60_0000), (0x60_1000, NV2A_SIZE)]
            .into_iter()
            .map(|(a, b)| (a, b - a, self.regs.page_ptr(a)))
            .collect()
    }

    // ---- push-buffer mode ----

    /// Whether the renderer consumes channel 0's FIFO.
    #[inline]
    pub fn pb_mode(&self) -> bool {
        self.pbmode.load(Ordering::Acquire)
    }

    /// The renderer takes (or gives back) channel 0's FIFO
    /// (`hle_host_pb_mode`).
    pub fn set_pb_mode(&self, on: bool) {
        self.pbmode.store(on, Ordering::Release);
        self.refresh_summary();
        self.refresh_fifo();
        log::info!(target: "gpu", "nv2a: push-buffer mode {}", if on { "on" } else { "off" });
    }

    /// The FIFO is empty: always outside push-buffer mode, else when the
    /// decoder has consumed up to PUT.
    fn fifo_empty(&self) -> bool {
        !self.pb_mode()
            || self.regs.load(reg::PFIFO_CACHE1_DMA_GET)
                == self.regs.load(reg::PFIFO_CACHE1_DMA_PUT)
    }

    // ---- guest register access ----

    /// A guest read of `size` (1, 2 or 4) bytes at `offset` in the window.
    /// Guest-CPU thread.
    pub fn mmio_read(&self, offset: u32, size: u8) -> u32 {
        let off = offset & (NV2A_SIZE - 1);
        let width_mask = if size >= 4 { u32::MAX } else { (1u32 << (u32::from(size) * 8)) - 1 };
        // The idle FIFO bits answer any access width inside the register
        // (Direct3D tests CACHE1_STATUS with a byte read).
        let word = off & !3;
        let shift = (off & 3) * 8;
        if (off & 3) + u32::from(size) <= 4 {
            let special = match word {
                reg::PFIFO_RUNOUT_STATUS | reg::PFIFO_CACHE1_STATUS => {
                    Some(if self.fifo_empty() { reg::PFIFO_LOW_MARK } else { 0 })
                }
                reg::PFIFO_CACHE1_DMA_PUSH => Some(if self.fifo_empty() {
                    self.regs.load(word) & !reg::PFIFO_DMA_BUSY
                } else {
                    self.regs.load(word) | reg::PFIFO_DMA_BUSY
                }),
                reg::USER_DMA_PUT | reg::USER_DMA_GET | reg::USER_REF if self.pb_mode() => {
                    Some(self.regs.load(word - reg::USER_DMA_PUT + reg::PFIFO_CACHE1_DMA_PUT))
                }
                // No write-combine flush pending (Riddick spins on it).
                reg::PFB_WBC => Some(0),
                _ => None,
            };
            if let Some(v) = special {
                return (v >> shift) & width_mask;
            }
        }
        let v = match off {
            reg::PMC_INTR_0 => self.pmc_intr(),
            reg::PTIMER_TIME_0 => self.clock.guest_ns() as u32 & !0x1F,
            reg::PTIMER_TIME_1 => (self.clock.guest_ns() >> 32) as u32 & 0x1FFF_FFFF,
            reg::PCRTC_RASTER => {
                // The line since the last vblank.
                let since =
                    self.clock.now_ns().wrapping_sub(self.last_vblank_ns.load(Ordering::Acquire));
                let frame = self.frame_ns.load(Ordering::Relaxed);
                ((since % frame) * LINES / frame) as u32
            }
            _ => return self.regs.read(off, size),
        };
        v & width_mask
    }

    /// A guest write of the low `size` (1, 2 or 4) bytes of `value` at
    /// `offset` in the window. Guest-CPU thread. A write to channel 0's
    /// DMA_PUT hands the push buffer up to there to the decoder and may wait
    /// for it (frame latency).
    pub fn mmio_write(&self, offset: u32, size: u8, value: u32) {
        self.mmio_write_inner(offset, size, value);
        let off = offset & (NV2A_SIZE - 1);
        if Self::feeds_fifo(off)
            || Self::feeds_fifo(off.wrapping_add(u32::from(size)).wrapping_sub(1))
        {
            self.refresh_fifo();
        }
    }

    /// Whether `off` is a register the FIFO's computed reads come from or
    /// are kept in.
    #[inline]
    fn feeds_fifo(off: u32) -> bool {
        matches!(
            off & !3,
            reg::PFIFO_CACHE1_DMA_PUT
                | reg::PFIFO_CACHE1_DMA_GET
                | reg::PFIFO_CACHE1_REF
                | reg::PFIFO_CACHE1_DMA_PUSH
                | reg::PFIFO_CACHE1_STATUS
                | reg::PFIFO_RUNOUT_STATUS
                | reg::USER_DMA_PUT
                | reg::USER_DMA_GET
                | reg::USER_REF
        )
    }

    /// Stores what the FIFO's computed registers read as (the idle bits,
    /// `NV_USER`'s view of CACHE1) in the register file, for a backend that
    /// lets the guest read the pages directly. Any thread, after a change to
    /// PUT, GET, REF or the mode. Repeats until what it stored agrees with
    /// what the registers say, so two racing calls cannot leave a stale
    /// answer behind.
    ///
    /// A register is written only when its value changes. The decoder
    /// publishes GET at every packet of the push buffer, tens of thousands of
    /// times a frame, and of the six registers kept here only the view of GET
    /// differs from one of those to the next: the others' stores, one of
    /// them a locked exchange, took the pages' cache lines from the guest's
    /// CPU each time for nothing.
    fn refresh_fifo(&self) {
        let r = &self.regs;
        let keep = |off: u32, v: u32| {
            if r.load(off) != v {
                r.store(off, v);
            }
        };
        loop {
            let seen = (
                self.pb_mode(),
                r.load(reg::PFIFO_CACHE1_DMA_PUT),
                r.load(reg::PFIFO_CACHE1_DMA_GET),
                r.load(reg::PFIFO_CACHE1_REF),
            );
            let (pb, put, get, rf) = seen;
            let empty = !pb || put == get;
            let status = if empty { reg::PFIFO_LOW_MARK } else { 0 };
            keep(reg::PFIFO_RUNOUT_STATUS, status);
            keep(reg::PFIFO_CACHE1_STATUS, status);
            if (r.load(reg::PFIFO_CACHE1_DMA_PUSH) & reg::PFIFO_DMA_BUSY == 0) != empty {
                r.update(reg::PFIFO_CACHE1_DMA_PUSH, |v| {
                    if empty { v & !reg::PFIFO_DMA_BUSY } else { v | reg::PFIFO_DMA_BUSY }
                });
            }
            if pb {
                keep(reg::USER_DMA_PUT, put);
                keep(reg::USER_DMA_GET, get);
                keep(reg::USER_REF, rf);
            }
            let now = (
                self.pb_mode(),
                r.load(reg::PFIFO_CACHE1_DMA_PUT),
                r.load(reg::PFIFO_CACHE1_DMA_GET),
                r.load(reg::PFIFO_CACHE1_REF),
            );
            if now == seen {
                return;
            }
        }
    }

    fn mmio_write_inner(&self, offset: u32, size: u8, value: u32) {
        let off = offset & (NV2A_SIZE - 1);
        let r = &self.regs;
        if self.pb_mode() {
            if size == 1 && (off == reg::PRMDIO_WRITE_INDEX || off == reg::PRMDIO_DATA) {
                self.dac_write(off, value as u8);
                return;
            }
            if size == 4 && self.pb_write(off, value) {
                return;
            }
        }
        match off {
            // A summary of the units' registers; no write-combine flush is
            // ever pending.
            reg::PMC_INTR_0 | reg::PFB_WBC => {}
            reg::PCRTC_INTR_0 | reg::PTIMER_INTR_0 => {
                // Write 1 to clear.
                r.fetch_clear(off, value);
                self.update_irq();
            }
            reg::PTIMER_INTR_EN_0 | reg::PMC_INTR_EN_0 | reg::PCRTC_INTR_EN_0 => {
                r.store(off, value);
                self.update_irq();
            }
            reg::PTIMER_ALARM_0 => {
                r.store(off, value);
                self.alarm_set();
            }
            reg::PVIDEO_BUFFER if self.pb_mode() => {
                // A buffer to show stays in use until the overlay stops;
                // Direct3D stops it only when a buffer is in use (Forza).
                r.fetch_or(off, value);
                if value & 1 != 0 {
                    self.decoder.overlay_changed(true);
                }
            }
            reg::PVIDEO_STOP if self.pb_mode() => {
                // The overlay stops: no buffer pending, as Direct3D waits to
                // see (EnableOverlay(FALSE) spins on BUFFER until 0: Forza).
                r.store(off, value);
                if value & 1 != 0 {
                    r.store(reg::PVIDEO_BUFFER, 0);
                    self.decoder.overlay_changed(false);
                }
            }
            reg::PCRTC_START => {
                if self.pb_mode() {
                    // The display shows that memory.
                    self.decoder.display_start(value);
                }
                r.write(off, size, value);
            }
            _ => r.write(off, size, value),
        }
    }

    /// The 32-bit writes the decoder takes part in. Returns whether the write
    /// was handled.
    fn pb_write(&self, off: u32, value: u32) -> bool {
        let r = &self.regs;
        match off {
            reg::PFIFO_CACHE1_DMA_SUBROUTINE => {
                r.store(off, value);
                // Direct3D left a run of the push buffer (XDK 4928).
                self.decoder.subroutine_written(value);
                true
            }
            reg::PFIFO_CACHE1_DMA_PUT | reg::PFIFO_CACHE1_DMA_GET => {
                log::debug!(
                    target: "gpu",
                    "nv2a: CACHE1 {} <- {value:08x} (GET {:08x} PUT {:08x})",
                    if off == reg::PFIFO_CACHE1_DMA_PUT { "PUT" } else { "GET" },
                    r.load(reg::PFIFO_CACHE1_DMA_GET),
                    r.load(reg::PFIFO_CACHE1_DMA_PUT),
                );
                if off == reg::PFIFO_CACHE1_DMA_GET {
                    // A channel set-up: the decoder's scout syncs again.
                    self.decoder.get();
                }
                // The register itself is stored like any other.
                false
            }
            reg::USER_DMA_PUT | reg::USER_DMA_GET | reg::USER_REF => {
                if off == reg::USER_DMA_PUT {
                    // Before the decoder can see the new PUT.
                    self.decoder.pushed(value);
                }
                r.store(off - reg::USER_DMA_PUT + reg::PFIFO_CACHE1_DMA_PUT, value);
                if off == reg::USER_DMA_PUT {
                    self.decoder.put();
                } else if off == reg::USER_DMA_GET {
                    self.decoder.get();
                }
                true
            }
            reg::PGRAPH_FIFO => {
                r.store(off, value);
                // Access back: the decoder goes on.
                self.decoder.kick();
                true
            }
            reg::PGRAPH_INTR | reg::PGRAPH_INTR_EN => {
                // The handler clears what it handled (1s), and a decoder
                // waiting on a trap goes on.
                if off == reg::PGRAPH_INTR {
                    r.fetch_clear(off, value);
                } else {
                    r.store(off, value);
                }
                self.update_irq();
                self.decoder.kick();
                true
            }
            reg::PGRAPH_INCREMENT => {
                // The vblank handler's flip: the read counter advances
                // (modulo), and a FLIP_STALL in the push buffer may go on.
                if value & 2 != 0 {
                    let n = r.update(reg::PGRAPH_SURFACE, |s| {
                        let modulo = (s >> 28) & 7;
                        let read = (s >> 24) & 7;
                        let next = if modulo != 0 { (read + 1) % modulo } else { 0 };
                        (s & !0x0700_0000) | (next << 24)
                    });
                    log::trace!(
                        target: "gpu",
                        "nv2a: flip read -> {} (write {}, display {:08x})",
                        (n >> 24) & 7,
                        (n >> 20) & 7,
                        r.load(reg::PCRTC_START),
                    );
                    self.decoder.flip_read();
                    self.decoder.flipped(r.load(reg::PCRTC_START));
                    self.decoder.kick();
                }
                true
            }
            _ => false,
        }
    }

    /// Direct3D's gamma ramp goes to the DAC's palette (index 0, then 768
    /// data bytes); the whole table becomes the display's ramp. Without it
    /// Riddick's gameplay draws too dark.
    fn dac_write(&self, off: u32, v: u8) {
        let mut dac = lock(&self.dac);
        if off == reg::PRMDIO_WRITE_INDEX {
            dac.index = v;
            dac.comp = 0;
            return;
        }
        let (comp, index) = (usize::from(dac.comp), usize::from(dac.index));
        dac.rgb[comp][index] = v;
        dac.comp += 1;
        if dac.comp == 3 {
            dac.comp = 0;
            dac.index = dac.index.wrapping_add(1);
            if dac.index == 0 {
                // The 256th entry: the table is whole.
                self.decoder.gamma(&dac.rgb);
            }
        }
    }

    // ---- the renderer's view of the registers ----

    /// The register at `off` as the guest last wrote it (`hle_host_gpu_reg`).
    #[inline]
    pub fn gpu_reg(&self, off: u32) -> u32 {
        if off < NV2A_SIZE - 3 { self.regs.load(off) } else { 0 }
    }

    /// A write as the GPU makes one (`hle_host_gpu_reg_set`).
    #[inline]
    pub fn gpu_reg_set(&self, off: u32, v: u32) {
        if off < NV2A_SIZE - 3 {
            self.regs.store(off, v);
            if Self::feeds_summary(off) {
                self.refresh_summary();
            }
            if Self::feeds_fifo(off) {
                self.refresh_fifo();
            }
        }
    }

    /// Sets the bits of `mask` to `bits`, atomically
    /// (`hle_host_gpu_reg_mask`).
    #[inline]
    pub fn gpu_reg_mask(&self, off: u32, mask: u32, bits: u32) {
        if off < NV2A_SIZE - 3 {
            self.regs.update(off, |v| (v & !mask) | (bits & mask));
            if Self::feeds_summary(off) {
                self.refresh_summary();
            }
            if Self::feeds_fifo(off) {
                self.refresh_fifo();
            }
        }
    }

    // ---- the video mode ----

    /// The video mode's refresh rate, 50 or 60 Hz (the kernel's
    /// `AvSetDisplayMode`): the vblank period follows it.
    pub fn set_refresh(&self, hz: u32) {
        let hz = if hz == 50 { 50 } else { 60 };
        self.mode_hz.store(hz, Ordering::Relaxed);
        let frame = if hz == 50 { FRAME_NS_50 } else { FRAME_NS_60 };
        if self.frame_ns.swap(frame, Ordering::Relaxed) != frame {
            log::info!(target: "gpu", "nv2a: vblank at {hz} Hz (the video mode)");
        }
    }

    /// The refresh rate of the video mode, 50 or 60.
    pub fn refresh_hz(&self) -> u32 {
        self.mode_hz.load(Ordering::Relaxed)
    }

    /// The AV mode word the title last set (0: none yet).
    pub fn av_mode(&self) -> u32 {
        self.av_mode.load(Ordering::Relaxed)
    }

    /// Records the AV mode word of `AvSetDisplayMode`.
    pub fn set_av_mode(&self, mode: u32) {
        self.av_mode.store(mode, Ordering::Relaxed);
    }

    /// The width of the AV mode last set, where its timing (the mode's bits
    /// 15:8) tells it, as the reference has them: 640 for the
    /// standard-definition and 480p modes whose frame buffers show that
    /// width, 720 for the others of those, and 0 for the rest (HD among
    /// them), where the frame buffer's pitch tells. Max Payne keeps
    /// its graphic novel in a buffer wider than the mode shows.
    pub fn av_mode_width(&self) -> u32 {
        match (self.av_mode() >> 8) & 0xFF {
            0x01 | 0x03 | 0x05 | 0x07 | 0x0D | 0x0E | 0x0F => 640,
            0x02 | 0x04 | 0x08 => 720,
            _ => 0,
        }
    }

    /// The picture shape the title asked Direct3D for: 0 unknown, 1 4:3,
    /// 2 16:9.
    pub fn av_title(&self) -> u32 {
        self.av_title.load(Ordering::Relaxed)
    }

    /// Records the title's picture shape (0, 1 or 2).
    pub fn set_av_title(&self, shape: u32) {
        self.av_title.store(shape, Ordering::Relaxed);
    }

    /// Vertical blanks since power-on.
    pub fn vblank_count(&self) -> u64 {
        self.vblanks.load(Ordering::Relaxed)
    }

    // ---- PTIMER's alarm ----

    /// `ALARM_0` was written: the alarm is due again.
    fn alarm_set(&self) {
        let mut a = lock(&self.alarm);
        if !a.armed {
            a.armed = true;
            log::info!(
                target: "gpu",
                "nv2a: PTIMER alarm in use (alarm {:08x}, interrupt {})",
                self.regs.load(reg::PTIMER_ALARM_0),
                if self.regs.load(reg::PTIMER_INTR_EN_0) & reg::PTIMER_ALARM != 0 { "on" } else { "off" },
            );
        }
        a.rearm = true;
        self.alarm_cv.notify_one();
    }

    /// One look at the alarm: fires it if the guest's clock reached it.
    /// Returns the host instant to look again at, or `None` when it fired
    /// (look again at once).
    ///
    /// The alarm interrupt comes when the time's low word reaches `ALARM_0`:
    /// Direct3D times its vblank work with it in the modes whose vblank it
    /// does not take from the encoder (Project Zero II's PAL disc at 60 Hz),
    /// and its timer callbacks.
    pub(crate) fn alarm_poll(&self, a: &mut Alarm) -> Option<u64> {
        let now = self.clock.guest_ns();
        if a.rearm {
            a.rearm = false;
            a.due = alarm_due(self.regs.load(reg::PTIMER_ALARM_0), now);
        }
        if now >= a.due {
            self.regs.fetch_or(reg::PTIMER_INTR_0, reg::PTIMER_ALARM);
            self.update_irq();
            // The low word comes round to it again.
            a.due += 1 << 32;
            return None;
        }
        let until = self.clock.guest_to_host_ns(a.due);
        Some(until.min(self.clock.now_ns() + ALARM_POLL_NS))
    }

    /// The alarm thread's body: returns when [`Nv2a::stop`] is called.
    pub fn run_alarm(&self) {
        let mut a = lock(&self.alarm);
        while !self.stopped() {
            if !a.armed {
                a = self.alarm_cv.wait(a).unwrap_or_else(PoisonError::into_inner);
                continue;
            }
            if self.is_paused() {
                drop(a);
                self.wait_while_paused();
                a = lock(&self.alarm);
                // The guest's clock may have moved (a resumed session).
                a.rearm = true;
                continue;
            }
            if let Some(until) = self.alarm_poll(&mut a) {
                let wait = until.saturating_sub(self.clock.now_ns());
                a = self
                    .alarm_cv
                    .wait_timeout(a, Duration::from_nanos(wait))
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
            }
        }
    }

    // ---- the vblank thread ----

    /// One vertical blank: waits until it is due, raises it, and returns the
    /// instant the next one counts from. `prev` is the last one's instant.
    pub fn vblank_step(&self, prev: u64) -> u64 {
        let due = self.next_vblank(prev);
        self.clock.sleep_until(due);
        self.last_vblank_ns.store(due, Ordering::Release);
        self.regs.fetch_or(reg::PCRTC_INTR_0, reg::PCRTC_VBLANK);
        self.vblanks.fetch_add(1, Ordering::Relaxed);
        self.update_irq();
        if self.pb_mode() {
            // The scanout shows the memory, flipped or not.
            self.decoder.vblank();
        }
        let step = if due > prev { due - prev } else { self.frame_ns.load(Ordering::Relaxed) };
        self.late_skip(due, step)
    }

    /// The vblank thread's body: a vblank every frame of the video mode on
    /// the host clock, none while paused. Returns when [`Nv2a::stop`] is
    /// called.
    pub fn run_vblank(&self) {
        let mut next = self.clock.now_ns();
        while !self.stopped() {
            if self.is_paused() {
                // No vblanks while paused, and from now on resuming, not the
                // missed ones one after the other.
                self.wait_while_paused();
                next = self.clock.now_ns();
                continue;
            }
            next = self.vblank_step(next);
        }
    }

    // ---- pause and shutdown ----

    /// Stops (or resumes) the vblank and the alarm while the guest is paused.
    pub fn set_paused(&self, paused: bool) {
        let _g = lock(&self.pause_mu);
        self.paused.store(paused, Ordering::Release);
        self.pause_cv.notify_all();
    }

    /// Whether [`Nv2a::set_paused`] holds the timers.
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Acquire)
    }

    fn wait_while_paused(&self) {
        let mut g = lock(&self.pause_mu);
        while self.is_paused() && !self.stopped() {
            g = self.pause_cv.wait(g).unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Makes [`Nv2a::run_vblank`] and [`Nv2a::run_alarm`] return.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        {
            let _g = lock(&self.pause_mu);
            self.pause_cv.notify_all();
        }
        let _a = lock(&self.alarm);
        self.alarm_cv.notify_all();
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }
}

/// When the time's low word, at guest time `now`, next reaches `alarm`.
fn alarm_due(alarm: u32, now: u64) -> u64 {
    let d = alarm.wrapping_sub(now as u32 & !0x1F);
    if d >= ALARM_LATE_NS.wrapping_neg() { now } else { now + u64::from(d) }
}

impl<D: Decoder, C: Clock> Nv2a<D, C> {
    /// The front end's part of a savestate: the register file, the vblank's
    /// count and period, the video mode, the DAC's palette and PTIMER's
    /// alarm. The display's vsyncs and the flip pacing are the host's and
    /// stay. Call with the guest paused; loading, the interrupt line follows
    /// the registers.
    pub fn state(&self, io: &mut xb_types::state::StateIo) {
        use xb_types::state::Stateful;
        fn atom<T: Stateful + Copy>(
            io: &mut xb_types::state::StateIo,
            get: impl Fn() -> T,
            set: impl Fn(T),
        ) {
            let mut v = get();
            v.state(io);
            if io.loading() {
                set(v);
            }
        }
        const R: Ordering = Ordering::Relaxed;
        self.regs.state(io);
        atom(io, || self.vblanks.load(R), |v| self.vblanks.store(v, R));
        atom(io, || self.frame_ns.load(R), |v| self.frame_ns.store(v, R));
        atom(io, || self.mode_hz.load(R), |v| self.mode_hz.store(v, R));
        atom(io, || self.av_mode.load(R), |v| self.av_mode.store(v, R));
        atom(io, || self.av_title.load(R), |v| self.av_title.store(v, R));
        atom(io, || self.pbmode.load(R), |v| self.pbmode.store(v, R));
        {
            let mut dac = lock(&self.dac);
            dac.index.state(io);
            dac.comp.state(io);
            dac.rgb.state(io);
        }
        {
            let mut alarm = lock(&self.alarm);
            alarm.armed.state(io);
            alarm.due.state(io);
            if io.loading() {
                alarm.rearm = alarm.armed; // its due time is found again from the registers
            }
        }
        if io.loading() {
            self.alarm_cv.notify_all();
            self.refresh_summary();
            self.refresh_fifo();
            self.update_irq();
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// What the mock decoder was asked to do, in order.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(crate) enum Call {
        Kick,
        Put,
        Get,
        Pushed(u32),
        Subroutine(u32),
        Gamma(u8, u8, u8),
        FlipRead,
        Flipped(u32),
        Overlay(bool),
        DisplayStart(u32),
        Vblank,
    }

    #[derive(Default)]
    pub(crate) struct MockDecoder {
        pub(crate) calls: Mutex<Vec<Call>>,
    }

    impl MockDecoder {
        fn log(&self, c: Call) {
            lock(&self.calls).push(c);
        }
    }

    impl Decoder for Arc<MockDecoder> {
        fn kick(&self) {
            self.log(Call::Kick);
        }
        fn put(&self) {
            self.log(Call::Put);
        }
        fn get(&self) {
            self.log(Call::Get);
        }
        fn pushed(&self, put: u32) {
            self.log(Call::Pushed(put));
        }
        fn subroutine_written(&self, value: u32) {
            self.log(Call::Subroutine(value));
        }
        fn gamma(&self, rgb: &[[u8; 256]; 3]) {
            self.log(Call::Gamma(rgb[0][255], rgb[1][128], rgb[2][0]));
        }
        fn flip_read(&self) {
            self.log(Call::FlipRead);
        }
        fn flipped(&self, start: u32) {
            self.log(Call::Flipped(start));
        }
        fn overlay_changed(&self, show: bool) {
            self.log(Call::Overlay(show));
        }
        fn display_start(&self, pa: u32) {
            self.log(Call::DisplayStart(pa));
        }
        fn vblank(&self) {
            self.log(Call::Vblank);
        }
    }

    /// A clock the test moves: sleeping until an instant jumps to it, and the
    /// guest's clock runs `guest_behind` nanoseconds behind the host's.
    #[derive(Default)]
    pub(crate) struct MockClock {
        pub(crate) now: AtomicU64,
        pub(crate) guest_behind: AtomicU64,
        /// Added to the instant a sleep ends at: a thread that wakes late.
        pub(crate) oversleep: AtomicU64,
    }

    impl Clock for Arc<MockClock> {
        fn now_ns(&self) -> u64 {
            self.now.load(Ordering::SeqCst)
        }
        fn guest_ns(&self) -> u64 {
            self.now_ns() - self.guest_behind.load(Ordering::SeqCst)
        }
        fn guest_to_host_ns(&self, guest_ns: u64) -> u64 {
            guest_ns + self.guest_behind.load(Ordering::SeqCst)
        }
        fn sleep_until(&self, host_ns: u64) {
            let wake = host_ns + self.oversleep.swap(0, Ordering::SeqCst);
            self.now.fetch_max(wake, Ordering::SeqCst);
        }
    }

    pub(crate) type TestNv2a = Nv2a<Arc<MockDecoder>, Arc<MockClock>>;

    pub(crate) struct Rig {
        pub(crate) nv: TestNv2a,
        pub(crate) dec: Arc<MockDecoder>,
        pub(crate) clock: Arc<MockClock>,
        pub(crate) irq: Arc<IrqHub>,
    }

    impl Rig {
        pub(crate) fn calls(&self) -> Vec<Call> {
            std::mem::take(&mut *lock(&self.dec.calls))
        }
        pub(crate) fn line(&self) -> bool {
            self.irq.pending() & Irq::Gpu.mask() != 0
        }
    }

    pub(crate) fn rig() -> Rig {
        let dec = Arc::new(MockDecoder::default());
        let clock = Arc::new(MockClock::default());
        clock.now.store(1_000_000_000, Ordering::SeqCst);
        let irq = Arc::new(IrqHub::new());
        let nv = Nv2a::new(irq.clone(), dec.clone(), clock.clone(), 64 << 20);
        Rig { nv, dec, clock, irq }
    }

    #[test]
    fn power_on_registers_identify_the_chip_and_its_memory() {
        let r = rig();
        assert_eq!(r.nv.mmio_read(reg::PMC_BOOT_0, 4), 0x02A0_00A1);
        assert_eq!(r.nv.mmio_read(reg::PFB_CSTATUS, 4), 64 << 20);
        assert_eq!(r.nv.mmio_read(reg::PMC_INTR_0, 4), 0);
    }

    #[test]
    fn a_plain_register_reads_back_what_was_written_at_every_width() {
        let r = rig();
        r.nv.mmio_write(0x00_1804, 4, 0xDEAD_BEEF);
        assert_eq!(r.nv.mmio_read(0x00_1804, 4), 0xDEAD_BEEF);
        assert_eq!(r.nv.mmio_read(0x00_1805, 1), 0xBE);
        assert_eq!(r.nv.mmio_read(0x00_1806, 2), 0xDEAD);
        r.nv.mmio_write(0x00_1805, 1, 0x12);
        assert_eq!(r.nv.mmio_read(0x00_1804, 4), 0xDEAD_12EF);
        // The renderer sees the same value.
        assert_eq!(r.nv.gpu_reg(0x00_1804), 0xDEAD_12EF);
    }

    #[test]
    fn the_fifo_reads_idle_until_the_decoder_owns_it_and_is_behind() {
        let r = rig();
        r.nv.mmio_write(reg::PFIFO_CACHE1_DMA_PUT, 4, 0x100);
        // Outside push-buffer mode nobody consumes the FIFO: always idle.
        assert_eq!(r.nv.mmio_read(reg::PFIFO_CACHE1_STATUS, 4), reg::PFIFO_LOW_MARK);
        assert_eq!(r.nv.mmio_read(reg::PFIFO_RUNOUT_STATUS, 1), reg::PFIFO_LOW_MARK);
        assert_eq!(r.nv.mmio_read(reg::PFIFO_CACHE1_DMA_PUSH, 4) & reg::PFIFO_DMA_BUSY, 0);

        r.nv.set_pb_mode(true);
        r.nv.mmio_write(reg::USER_DMA_PUT, 4, 0x200);
        assert_eq!(r.nv.mmio_read(reg::PFIFO_CACHE1_STATUS, 1), 0);
        assert_eq!(
            r.nv.mmio_read(reg::PFIFO_CACHE1_DMA_PUSH, 4) & reg::PFIFO_DMA_BUSY,
            reg::PFIFO_DMA_BUSY
        );
        // The decoder reports GET as it consumes.
        r.nv.gpu_reg_set(reg::PFIFO_CACHE1_DMA_GET, 0x200);
        assert_eq!(r.nv.mmio_read(reg::PFIFO_CACHE1_STATUS, 1), reg::PFIFO_LOW_MARK);
        assert_eq!(r.nv.mmio_read(reg::PFIFO_CACHE1_DMA_PUSH, 4) & reg::PFIFO_DMA_BUSY, 0);
    }

    #[test]
    fn the_register_file_holds_what_the_fifos_computed_registers_read_as() {
        // A backend that maps the pages reads the file, not `mmio_read`: the
        // two agree after every step of a push buffer being filled and
        // consumed, a step that changes nothing included.
        let r = rig();
        let computed = [
            reg::PFIFO_RUNOUT_STATUS,
            reg::PFIFO_CACHE1_STATUS,
            reg::PFIFO_CACHE1_DMA_PUSH,
            reg::USER_DMA_PUT,
            reg::USER_DMA_GET,
            reg::USER_REF,
        ];
        let agree = |step: &str| {
            for off in computed {
                assert_eq!(r.nv.gpu_reg(off), r.nv.mmio_read(off, 4), "{off:#08x} after {step}");
            }
        };
        r.nv.set_pb_mode(true);
        agree("the decoder takes the FIFO");
        r.nv.mmio_write(reg::USER_DMA_PUT, 4, 0x300);
        agree("a PUT");
        for get in [0x100, 0x100, 0x200, 0x300, 0x300] {
            r.nv.gpu_reg_set(reg::PFIFO_CACHE1_DMA_GET, get);
            agree("a GET");
        }
        r.nv.gpu_reg_set(reg::PFIFO_CACHE1_REF, 7);
        agree("a REF");
        r.nv.mmio_write(reg::USER_DMA_PUT, 4, 0x400);
        agree("a PUT past an empty FIFO");
        r.nv.set_pb_mode(false);
        for off in [reg::PFIFO_RUNOUT_STATUS, reg::PFIFO_CACHE1_STATUS, reg::PFIFO_CACHE1_DMA_PUSH]
        {
            assert_eq!(
                r.nv.gpu_reg(off),
                r.nv.mmio_read(off, 4),
                "{off:#08x} with the FIFO given back"
            );
        }
    }

    #[test]
    fn a_dma_put_write_reaches_the_decoder_through_cache1() {
        let r = rig();
        // Outside push-buffer mode the channel registers are plain storage.
        r.nv.mmio_write(reg::USER_DMA_PUT, 4, 0x40);
        assert_eq!(r.nv.mmio_read(reg::USER_DMA_PUT, 4), 0x40);
        assert_eq!(r.nv.gpu_reg(reg::PFIFO_CACHE1_DMA_PUT), 0);
        assert!(r.calls().is_empty());

        r.nv.set_pb_mode(true);
        r.nv.mmio_write(reg::USER_DMA_PUT, 4, 0x1234);
        // The scout hears of the new PUT, then the decoder is kicked.
        assert_eq!(r.calls(), [Call::Pushed(0x1234), Call::Put]);
        assert_eq!(r.nv.gpu_reg(reg::PFIFO_CACHE1_DMA_PUT), 0x1234);
        assert_eq!(r.nv.mmio_read(reg::USER_DMA_PUT, 4), 0x1234);
        // GET and REF read the decoder's.
        r.nv.gpu_reg_set(reg::PFIFO_CACHE1_DMA_GET, 0x1000);
        r.nv.gpu_reg_set(reg::PFIFO_CACHE1_REF, 7);
        assert_eq!(r.nv.mmio_read(reg::USER_DMA_GET, 4), 0x1000);
        assert_eq!(r.nv.mmio_read(reg::USER_REF, 4), 7);
        r.nv.mmio_write(reg::USER_DMA_GET, 4, 0);
        assert_eq!(r.calls(), [Call::Get]);
        assert_eq!(r.nv.gpu_reg(reg::PFIFO_CACHE1_DMA_GET), 0);
    }

    #[test]
    fn the_line_follows_pending_and_enabled_interrupts() {
        let r = rig();
        // Pending but not enabled anywhere: line low.
        r.nv.regs.fetch_or(reg::PCRTC_INTR_0, reg::PCRTC_VBLANK);
        r.nv.update_irq();
        assert!(!r.line());
        assert_eq!(r.nv.mmio_read(reg::PMC_INTR_0, 4), 0);
        // The unit's enable alone shows in the summary, not on the line.
        r.nv.mmio_write(reg::PCRTC_INTR_EN_0, 4, 1);
        assert_eq!(r.nv.mmio_read(reg::PMC_INTR_0, 4), reg::PMC_INTR_PCRTC);
        assert!(!r.line());
        r.nv.mmio_write(reg::PMC_INTR_EN_0, 4, 1);
        assert!(r.line());
        // The summary cannot be written.
        r.nv.mmio_write(reg::PMC_INTR_0, 4, 0);
        assert!(r.line());
        // Write 1 to clear.
        r.nv.mmio_write(reg::PCRTC_INTR_0, 4, reg::PCRTC_VBLANK);
        assert!(!r.line());
        assert_eq!(r.nv.mmio_read(reg::PCRTC_INTR_0, 4), 0);
    }

    #[test]
    fn pgraph_interrupts_count_only_in_push_buffer_mode() {
        let r = rig();
        r.nv.mmio_write(reg::PMC_INTR_EN_0, 4, 1);
        r.nv.mmio_write(reg::PGRAPH_INTR_EN, 4, 0x10);
        r.nv.gpu_reg_mask(reg::PGRAPH_INTR, 0x10, 0x10);
        r.nv.update_irq();
        assert!(!r.line());

        r.nv.set_pb_mode(true);
        r.nv.update_irq();
        assert!(r.line());
        assert_eq!(r.nv.mmio_read(reg::PMC_INTR_0, 4), reg::PMC_INTR_PGRAPH);
        // The handler clears the trap: line low, decoder goes on.
        r.calls();
        r.nv.mmio_write(reg::PGRAPH_INTR, 4, 0x10);
        assert!(!r.line());
        assert_eq!(r.calls(), [Call::Kick]);
    }

    #[test]
    fn increment_advances_the_flip_read_counter_modulo_and_reports_the_flip() {
        let r = rig();
        r.nv.set_pb_mode(true);
        r.nv.mmio_write(reg::PCRTC_START, 4, 0x03C0_0000);
        r.calls();
        // Modulo 3, read 1, write 2.
        r.nv.gpu_reg_set(reg::PGRAPH_SURFACE, (3 << 28) | (1 << 24) | (2 << 20) | 0x55);
        r.nv.mmio_write(reg::PGRAPH_INCREMENT, 4, 2);
        assert_eq!(r.nv.gpu_reg(reg::PGRAPH_SURFACE), (3 << 28) | (2 << 24) | (2 << 20) | 0x55);
        assert_eq!(r.calls(), [Call::FlipRead, Call::Flipped(0x03C0_0000), Call::Kick]);
        r.nv.mmio_write(reg::PGRAPH_INCREMENT, 4, 2);
        assert_eq!((r.nv.gpu_reg(reg::PGRAPH_SURFACE) >> 24) & 7, 0);
        // Without bit 1 nothing moves.
        r.calls();
        r.nv.mmio_write(reg::PGRAPH_INCREMENT, 4, 1);
        assert_eq!((r.nv.gpu_reg(reg::PGRAPH_SURFACE) >> 24) & 7, 0);
        assert!(r.calls().is_empty());
    }

    #[test]
    fn the_whole_dac_palette_becomes_the_gamma_ramp() {
        let r = rig();
        r.nv.set_pb_mode(true);
        r.nv.mmio_write(reg::PRMDIO_WRITE_INDEX, 1, 0);
        for i in 0..256u32 {
            r.nv.mmio_write(reg::PRMDIO_DATA, 1, i);
            r.nv.mmio_write(reg::PRMDIO_DATA, 1, 255 - i);
            r.nv.mmio_write(reg::PRMDIO_DATA, 1, i / 2 + 9);
            if i < 255 {
                assert!(r.calls().is_empty(), "the ramp is applied only when whole");
            }
        }
        assert_eq!(r.calls(), [Call::Gamma(255, 127, 9)]);
    }

    #[test]
    fn the_video_overlay_stays_in_use_until_stopped() {
        let r = rig();
        r.nv.set_pb_mode(true);
        r.nv.mmio_write(reg::PVIDEO_BUFFER, 4, 1);
        assert_eq!(r.nv.mmio_read(reg::PVIDEO_BUFFER, 4), 1);
        r.nv.mmio_write(reg::PVIDEO_STOP, 4, 1);
        assert_eq!(r.nv.mmio_read(reg::PVIDEO_BUFFER, 4), 0);
        assert_eq!(r.calls(), [Call::Overlay(true), Call::Overlay(false)]);
    }

    #[test]
    fn ptimer_reads_the_guest_clock_in_its_register_format() {
        let r = rig();
        r.clock.guest_behind.store(250_000_000, Ordering::SeqCst);
        r.clock.now.store((7 << 32) + 0x1234_567F + 250_000_000, Ordering::SeqCst);
        assert_eq!(r.nv.mmio_read(reg::PTIMER_TIME_0, 4), 0x1234_5660);
        assert_eq!(r.nv.mmio_read(reg::PTIMER_TIME_1, 4), 7);
    }

    #[test]
    fn the_alarm_fires_when_guest_time_reaches_it_and_again_a_wrap_later() {
        let r = rig();
        r.nv.mmio_write(reg::PMC_INTR_EN_0, 4, 1);
        r.nv.mmio_write(reg::PTIMER_INTR_EN_0, 4, reg::PTIMER_ALARM);
        let now = r.clock.now.load(Ordering::SeqCst);
        r.nv.mmio_write(reg::PTIMER_ALARM_0, 4, (now as u32 & !0x1F).wrapping_add(5_000_000));
        let mut a = lock(&r.nv.alarm);
        assert!(a.armed);
        // Not yet due: sleep at most the poll interval.
        let until = r.nv.alarm_poll(&mut a).expect("not due yet");
        assert!(until > now && until <= now + ALARM_POLL_NS);
        assert!(!r.line());
        r.clock.now.store(now + 5_000_000, Ordering::SeqCst);
        assert_eq!(r.nv.alarm_poll(&mut a), None);
        assert!(r.line());
        assert_eq!(r.nv.mmio_read(reg::PMC_INTR_0, 4), reg::PMC_INTR_PTIMER);
        // Due again when the low word comes round.
        assert_eq!(a.due - (now & !0x1F), 5_000_000 + (1 << 32));
        drop(a);
        r.nv.mmio_write(reg::PTIMER_INTR_0, 4, reg::PTIMER_ALARM);
        assert!(!r.line());
    }

    #[test]
    fn an_alarm_set_just_too_late_fires_at_once() {
        let now = 0x5_0000_1000u64;
        // In the future: when the low word gets there.
        assert_eq!(alarm_due(0x0000_2000, now), now + 0x1000);
        // Half a second ago: now, not 4.3 s later.
        assert_eq!(alarm_due((now as u32).wrapping_sub(500_000_000), now), now);
        // More than a second "ago" is the next wrap.
        let far = (now as u32).wrapping_sub(2_000_000_000);
        assert_eq!(alarm_due(far, now), now + u64::from(far.wrapping_sub(now as u32)));
    }

    #[test]
    fn vblanks_come_one_frame_apart_and_raise_the_line() {
        let r = rig();
        r.nv.mmio_write(reg::PMC_INTR_EN_0, 4, 1);
        r.nv.mmio_write(reg::PCRTC_INTR_EN_0, 4, 1);
        let start = r.clock.now.load(Ordering::SeqCst);
        let mut next = start;
        for n in 1..=5u64 {
            next = r.nv.vblank_step(next);
            assert_eq!(next, start + n * FRAME_NS_60);
            assert_eq!(r.nv.vblank_count(), n);
            assert_eq!(r.nv.last_vblank_ns.load(Ordering::SeqCst), next);
            assert!(r.line());
            r.nv.mmio_write(reg::PCRTC_INTR_0, 4, reg::PCRTC_VBLANK);
            assert!(!r.line());
        }
        // No decoder vblank work outside push-buffer mode.
        assert!(r.calls().is_empty());
        r.nv.set_pb_mode(true);
        r.nv.vblank_step(next);
        assert_eq!(r.calls(), [Call::Vblank]);
    }

    #[test]
    fn the_raster_counts_lines_since_the_last_vblank() {
        let r = rig();
        let next = r.nv.vblank_step(r.clock.now.load(Ordering::SeqCst));
        assert_eq!(r.nv.mmio_read(reg::PCRTC_RASTER, 4), 0);
        r.clock.now.store(next + FRAME_NS_60 / 2, Ordering::SeqCst);
        let line = r.nv.mmio_read(reg::PCRTC_RASTER, 4);
        assert!((261..=263).contains(&line), "half a frame is half the lines, got {line}");
        r.clock.now.store(next + FRAME_NS_60 - 1, Ordering::SeqCst);
        assert_eq!(r.nv.mmio_read(reg::PCRTC_RASTER, 4), 524);
    }

    #[test]
    fn a_50_hz_mode_slows_the_vblank() {
        let r = rig();
        r.nv.set_refresh(50);
        assert_eq!(r.nv.refresh_hz(), 50);
        let start = r.clock.now.load(Ordering::SeqCst);
        assert_eq!(r.nv.vblank_step(start), start + FRAME_NS_50);
        // Anything else is the 60 Hz family.
        r.nv.set_refresh(59);
        assert_eq!(r.nv.refresh_hz(), 60);
    }

    #[test]
    fn a_late_wake_drops_the_missed_vblanks_and_keeps_the_grid() {
        let r = rig();
        let start = r.clock.now.load(Ordering::SeqCst);
        // The thread sleeps through three and a half frames.
        r.clock.oversleep.store(FRAME_NS_60 * 7 / 2, Ordering::SeqCst);
        let next = r.nv.vblank_step(start);
        assert_eq!(r.nv.vblank_count(), 1, "one vblank is raised, not four in a row");
        assert_eq!(
            next,
            start + 4 * FRAME_NS_60,
            "the next one counts from the grid point before now"
        );
        assert_eq!(r.nv.vblank_step(next), start + 5 * FRAME_NS_60);
        assert_eq!(r.nv.vblank_count(), 2);
    }

    #[test]
    fn stopping_ends_the_timer_threads_even_while_paused() {
        let r = Arc::new(rig());
        r.nv.set_paused(true);
        let (a, b) = (r.clone(), r.clone());
        let vblank = std::thread::spawn(move || a.nv.run_vblank());
        let alarm = std::thread::spawn(move || b.nv.run_alarm());
        r.nv.stop();
        vblank.join().expect("vblank thread");
        alarm.join().expect("alarm thread");
        assert_eq!(r.nv.vblank_count(), 0, "no vblank while paused");
    }

    /// Pins what a savestate holds of this: see `xb_types::state::StateIo::probe`.
    #[test]
    fn the_saved_layout_of_the_gpu_front_end_is_the_recorded_one() {
        let mut io = xb_types::state::StateIo::probe();
        let r = rig();
        r.nv.state(&mut io);
        assert_eq!(
            io.shape_hash(),
            0xE064_155F_BEEF_2521,
            "the saved layout changed. Older states would be misread: give the section a new version, \
             read the old one where it differs, then record {:#018x} here",
            io.shape_hash()
        );
    }
}
