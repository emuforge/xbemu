//! Guest time, the wake handle, the run control handle and the clock thread.
//!
//! The guest's time is the host's monotonic clock less the time the guest
//! stood still: while paused (outside [`crate::Kernel::run`]) and through the
//! long part of a hold. It is kept as a seqlock the guest-CPU thread writes
//! and any thread reads, so devices and the audio processor can read the
//! guest's time from their own threads at the cost of a few atomic loads.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use xb_cpu::Kicker;

/// A hold's first part goes on as guest time (the reference's length): a wait
/// for the GPU the console had too keeps its pace; beyond it the time stands
/// still so an interrupt the guest times itself with comes at most this late.
const HOLD_RUN_NS: u64 = 8_000_000;

/// The clock interrupt's period: 1 ms, as the Xbox kernel programs its timer.
pub(crate) const CLOCK_PERIOD_NS: u64 = 1_000_000;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct ClockShared {
    origin: Instant,
    seq: AtomicU32,
    still: AtomicBool,
    still_raw: AtomicU64,
    off: AtomicU64,
    hold_raw: AtomicU64,
    held_out_ns: AtomicU64,
    held_max_ns: AtomicU64,
    holds: AtomicU32,
}

/// The guest's clock: monotonic nanoseconds that stop while the emulation is
/// paused.
///
/// A cheap handle (`Clone + Send + Sync`). Any thread may read it; only the
/// guest-CPU thread changes whether time stands still (the kernel's run loop
/// for pauses, the machine for holds).
#[derive(Clone)]
pub struct GuestClock(Arc<ClockShared>);

/// What a hold left out of the guest's time ([`GuestClock::hold_end`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HoldEnd {
    /// Nanoseconds the guest's time stood still.
    pub left_out_ns: u64,
    /// Nanoseconds the hold lasted on the host.
    pub held_ns: u64,
}

/// Holds since the last look ([`GuestClock::take_hold_stats`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HoldStats {
    /// How many holds ended.
    pub holds: u32,
    /// Nanoseconds they left out of the guest's time.
    pub left_out_ns: u64,
    /// The longest one, in host nanoseconds.
    pub longest_ns: u64,
}

impl Default for GuestClock {
    fn default() -> Self {
        Self::new()
    }
}

impl GuestClock {
    /// A clock at guest time 0, standing still until the kernel first runs.
    #[must_use]
    pub fn new() -> Self {
        Self(Arc::new(ClockShared {
            origin: Instant::now(),
            seq: AtomicU32::new(0),
            still: AtomicBool::new(true),
            still_raw: AtomicU64::new(0),
            off: AtomicU64::new(0),
            hold_raw: AtomicU64::new(0),
            held_out_ns: AtomicU64::new(0),
            held_max_ns: AtomicU64::new(0),
            holds: AtomicU32::new(0),
        }))
    }

    /// The host's monotonic time in this clock's terms.
    pub(crate) fn raw_ns(&self) -> u64 {
        self.0.origin.elapsed().as_nanos() as u64
    }

    /// The time's state read whole with the host's time: whether it stands
    /// still (or will), from when, and the time left out.
    fn read(&self) -> (bool, u64, u64, u64) {
        loop {
            let s = self.0.seq.load(Ordering::Acquire);
            if s & 1 != 0 {
                std::hint::spin_loop();
                continue;
            }
            let still = self.0.still.load(Ordering::Relaxed);
            let still_raw = self.0.still_raw.load(Ordering::Relaxed);
            let off = self.0.off.load(Ordering::Relaxed);
            let raw = self.raw_ns();
            if self.0.seq.load(Ordering::Acquire) == s {
                return (still, still_raw, off, raw);
            }
        }
    }

    /// Guest time now, in nanoseconds since the clock was created, less the
    /// time it stood still.
    #[must_use]
    pub fn now_ns(&self) -> u64 {
        let (still, still_raw, off, raw) = self.read();
        // (wrapping: a loaded state's time may be past this run's own)
        (if still && raw > still_raw { still_raw } else { raw }).wrapping_sub(off)
    }

    /// Sets the guest's time, for a savestate loaded while it stands still
    /// (the kernel paused): it goes on from `ns` when the kernel runs again.
    pub(crate) fn set_now(&self, ns: u64) {
        let now = self.now_ns();
        let off = self.0.off.load(Ordering::Relaxed);
        self.0.seq.fetch_add(1, Ordering::AcqRel);
        self.0.off.store(off.wrapping_add(now).wrapping_sub(ns), Ordering::Relaxed);
        self.0.seq.fetch_add(1, Ordering::AcqRel);
    }

    /// Whether the guest's time stands still right now (paused or held).
    #[must_use]
    pub fn is_still(&self) -> bool {
        let (still, still_raw, _, raw) = self.read();
        still && raw > still_raw
    }

    /// The time stands still `keep` ns from now, or goes on; returns the time
    /// left out when it goes on. Guest-CPU thread.
    fn write(&self, still: bool, keep: u64) -> u64 {
        let raw = self.raw_ns();
        let mut out = 0;
        self.0.seq.fetch_add(1, Ordering::AcqRel);
        if still {
            self.0.still_raw.store(raw + keep, Ordering::Relaxed);
        } else {
            out = raw.saturating_sub(self.0.still_raw.load(Ordering::Relaxed));
            self.0.off.fetch_add(out, Ordering::Relaxed);
        }
        self.0.still.store(still, Ordering::Relaxed);
        self.0.seq.fetch_add(1, Ordering::Release);
        out
    }

    /// The emulation pauses: time stands still from now.
    pub(crate) fn pause_begin(&self) {
        if !self.0.still.load(Ordering::Relaxed) {
            self.write(true, 0);
        }
    }

    /// The emulation resumes; returns how long the time stood still.
    pub(crate) fn pause_end(&self) -> u64 {
        if self.0.still.load(Ordering::Relaxed) { self.write(false, 0) } else { 0 }
    }

    /// The guest's CPU is about to be held by the host on its own thread (a
    /// device waiting for the renderer inside a `Bus` callback;
    /// `hle_host_guest_hold`). Guest time goes on for the hold's first 8 ms,
    /// then stands still until [`GuestClock::hold_end`]. Returns `false`, and
    /// does nothing, if time already stood still.
    ///
    /// Call on the guest-CPU thread only.
    pub fn hold_begin(&self) -> bool {
        if self.0.still.load(Ordering::Relaxed) {
            return false;
        }
        self.0.hold_raw.store(self.raw_ns(), Ordering::Relaxed);
        self.write(true, HOLD_RUN_NS);
        true
    }

    /// Ends a hold begun with [`GuestClock::hold_begin`] (which returned
    /// `true`). Guest-CPU thread only.
    pub fn hold_end(&self) -> HoldEnd {
        let out = self.write(false, 0);
        let held = self.raw_ns().saturating_sub(self.0.hold_raw.load(Ordering::Relaxed));
        self.0.held_out_ns.fetch_add(out, Ordering::Relaxed);
        self.0.held_max_ns.fetch_max(held, Ordering::Relaxed);
        self.0.holds.fetch_add(1, Ordering::Relaxed);
        HoldEnd { left_out_ns: out, held_ns: held }
    }

    /// The holds since the last call, for the statistics line.
    pub fn take_hold_stats(&self) -> HoldStats {
        HoldStats {
            holds: self.0.holds.swap(0, Ordering::Relaxed),
            left_out_ns: self.0.held_out_ns.swap(0, Ordering::Relaxed),
            longest_ns: self.0.held_max_ns.swap(0, Ordering::Relaxed),
        }
    }

    /// How long from now, on the host, until guest time `guest_ns`: while the
    /// time stands still, as if it went on now, and a millisecond at least.
    #[must_use]
    pub fn host_delay(&self, guest_ns: u64) -> Duration {
        let (still, still_raw, off, raw) = self.read();
        // (wrapping, as `now_ns`: a loaded state's time may be past this
        // run's own, and the time left out is then below zero)
        let host = guest_ns.wrapping_add(off);
        if !still || host <= still_raw {
            return Duration::from_nanos(host.saturating_sub(raw));
        }
        let at = raw.max(still_raw);
        let d = host - still_raw;
        let d = if raw > still_raw && d < 1_000_000 { 1_000_000 } else { d };
        Duration::from_nanos((at + d).saturating_sub(raw))
    }
}

struct WakeState {
    pending: bool,
    kicker: Option<Kicker>,
}

struct WakeShared {
    state: Mutex<WakeState>,
    cv: Condvar,
}

/// Brings the guest-CPU thread back to the kernel: kicks the CPU out of guest
/// code and ends an idle wait.
///
/// The machine installs it as the `IrqHub`'s kick hook
/// (`hub.set_kick(Box::new(move || wake.wake()))`), and anything that queues
/// work for the guest-CPU thread (an I/O worker's completion) calls it after
/// queueing. Callable from any thread.
#[derive(Clone)]
pub struct WakeHandle(Arc<WakeShared>);

impl WakeHandle {
    pub(crate) fn new() -> Self {
        Self(Arc::new(WakeShared {
            state: Mutex::new(WakeState { pending: false, kicker: None }),
            cv: Condvar::new(),
        }))
    }

    /// Wakes the guest-CPU thread: it looks at interrupt lines, completed I/O
    /// and control requests before running more guest code.
    pub fn wake(&self) {
        let kicker = {
            let mut s = lock(&self.0.state);
            s.pending = true;
            s.kicker.clone()
        };
        self.0.cv.notify_all();
        if let Some(k) = kicker {
            k.kick();
        }
    }

    /// Kicks the CPU without marking work pending (the clock's tick).
    pub(crate) fn kick(&self) {
        let kicker = lock(&self.0.state).kicker.clone();
        if let Some(k) = kicker {
            k.kick();
        }
    }

    /// The CPU whose `run` a wake interrupts.
    pub(crate) fn set_kicker(&self, kicker: Option<Kicker>) {
        lock(&self.0.state).kicker = kicker;
    }

    /// Whether a wake came since the last look; cleared.
    pub(crate) fn take(&self) -> bool {
        std::mem::take(&mut lock(&self.0.state).pending)
    }

    /// The idle wait: until a wake, or `timeout`. Returns whether a wake came.
    pub(crate) fn wait(&self, timeout: Duration) -> bool {
        let mut s = lock(&self.0.state);
        if !s.pending && !timeout.is_zero() {
            s = self
                .0
                .cv
                .wait_timeout_while(s, timeout, |s| !s.pending)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
        std::mem::take(&mut s.pending)
    }
}

struct ControlShared {
    stop: AtomicBool,
    pause: AtomicBool,
    /// A drive timing asked for: 0 none, else 1 + the mode's number
    /// (`timing_code`).
    timing: AtomicU8,
}

/// A [`xb_fs::TimingMode`] as [`ControlShared::timing`] keeps it.
const fn timing_code(mode: xb_fs::TimingMode) -> u8 {
    match mode {
        xb_fs::TimingMode::Off => 1,
        xb_fs::TimingMode::Quarter => 2,
        xb_fs::TimingMode::Console => 3,
    }
}

/// Asks a running [`crate::Kernel::run`] to return, from any thread.
#[derive(Clone)]
pub struct RunControl {
    shared: Arc<ControlShared>,
    wake: WakeHandle,
}

impl RunControl {
    pub(crate) fn new(wake: WakeHandle) -> Self {
        Self {
            shared: Arc::new(ControlShared {
                stop: AtomicBool::new(false),
                pause: AtomicBool::new(false),
                timing: AtomicU8::new(0),
            }),
            wake,
        }
    }

    /// `run` returns [`crate::RunOutcome::Stopped`] at its next safe point (a
    /// guest instruction boundary, or the idle wait).
    pub fn request_stop(&self) {
        self.shared.stop.store(true, Ordering::Release);
        self.wake.wake();
    }

    /// `run` returns [`crate::RunOutcome::Paused`] at its next safe point.
    /// Guest time stands still until `run` is called again.
    pub fn request_pause(&self) {
        self.shared.pause.store(true, Ordering::Release);
        self.wake.wake();
    }

    /// Changes how closely the drives keep the console's speeds while the
    /// title runs (the options' disc speed). Taken at the kernel's next
    /// entry; requests already with a drive keep the time they were given,
    /// and a title that needs the drives' own time keeps it.
    pub fn set_device_timing(&self, mode: xb_fs::TimingMode) {
        self.shared.timing.store(timing_code(mode), Ordering::Release);
        self.wake.wake();
    }

    pub(crate) fn take_device_timing(&self) -> Option<xb_fs::TimingMode> {
        match self.shared.timing.swap(0, Ordering::AcqRel) {
            1 => Some(xb_fs::TimingMode::Off),
            2 => Some(xb_fs::TimingMode::Quarter),
            3 => Some(xb_fs::TimingMode::Console),
            _ => None,
        }
    }

    pub(crate) fn take_stop(&self) -> bool {
        self.shared.stop.swap(false, Ordering::AcqRel)
    }

    pub(crate) fn take_pause(&self) -> bool {
        self.shared.pause.swap(false, Ordering::AcqRel)
    }

    pub(crate) fn pending(&self) -> bool {
        self.shared.stop.load(Ordering::Acquire) || self.shared.pause.load(Ordering::Acquire)
    }
}

struct TickState {
    /// The earliest thing due, in guest time.
    due: Option<u64>,
    /// The kernel is inside `run`.
    running: bool,
    quit: bool,
}

struct TickShared {
    state: Mutex<TickState>,
    cv: Condvar,
    /// Kernel entries so far: a period without one, the guest running its own
    /// code, gets a tick of its own.
    refreshes: AtomicU32,
    in_guest: AtomicBool,
    ticks: AtomicU64,
}

/// The clock thread: kicks the guest CPU when a timer, a wait's timeout, the
/// running thread's slice or a device deadline is due, and once a millisecond
/// while the guest runs its own code without entering the kernel, so that
/// `KeTickCount` advances for a title that polls it (Splinter Cell Chaos
/// Theory waits ten ticks that way at boot).
pub(crate) struct Ticker {
    shared: Arc<TickShared>,
    thread: Option<JoinHandle<()>>,
}

impl Ticker {
    pub(crate) fn new(clock: GuestClock, wake: WakeHandle) -> Self {
        let shared = Arc::new(TickShared {
            state: Mutex::new(TickState { due: None, running: false, quit: false }),
            cv: Condvar::new(),
            refreshes: AtomicU32::new(0),
            in_guest: AtomicBool::new(false),
            ticks: AtomicU64::new(0),
        });
        let s = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("xb-clock".to_owned())
            .spawn(move || {
                xb_types::thread::pacing();
                tick_thread(&s, &clock, &wake);
            })
            .ok();
        if thread.is_none() {
            log::error!(target: "kernel", "the clock thread could not be started: timers fire only at kernel entries");
        }
        Self { shared, thread }
    }

    /// The earliest due time changed.
    pub(crate) fn arm(&self, due: Option<u64>) {
        let mut s = lock(&self.shared.state);
        if s.due != due {
            let earlier = match (due, s.due) {
                (Some(a), Some(b)) => a < b,
                (Some(_), None) => true,
                _ => false,
            };
            s.due = due;
            if earlier {
                self.shared.cv.notify_all();
            }
        }
    }

    /// The kernel enters or leaves `run`: outside it the thread sleeps.
    pub(crate) fn set_running(&self, running: bool) {
        lock(&self.shared.state).running = running;
        self.shared.cv.notify_all();
    }

    /// A kernel entry refreshed the tick count.
    pub(crate) fn refreshed(&self) {
        self.shared.refreshes.fetch_add(1, Ordering::Relaxed);
    }

    /// Guest code is about to run, or stopped running.
    pub(crate) fn set_in_guest(&self, on: bool) {
        self.shared.in_guest.store(on, Ordering::Release);
    }

    /// Ticks given for the time alone, for the statistics line.
    pub(crate) fn ticks(&self) -> u64 {
        self.shared.ticks.load(Ordering::Relaxed)
    }
}

impl Drop for Ticker {
    fn drop(&mut self) {
        lock(&self.shared.state).quit = true;
        self.shared.cv.notify_all();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The longest the clock thread sleeps between looks, in clock periods.
const MAX_BACKOFF: u64 = 4;

fn tick_thread(shared: &TickShared, clock: &GuestClock, wake: &WakeHandle) {
    let mut seen = shared.refreshes.load(Ordering::Relaxed);
    let mut s = lock(&shared.state);
    // The period between looks: a millisecond while the guest runs its own
    // code, backing off to MAX_BACKOFF periods while it keeps entering the
    // kernel (or idles there), where the time is refreshed without a tick.
    let adaptive = std::env::var_os("XBEMU_T4_TICK_ADAPT").is_some();
    let mut periods: u64 = 1;
    loop {
        if s.quit {
            return;
        }
        if !s.running {
            s = shared.cv.wait(s).unwrap_or_else(std::sync::PoisonError::into_inner);
            continue;
        }
        let now = clock.now_ns();
        let period_end = now + CLOCK_PERIOD_NS * periods;
        let wake_at = s.due.map_or(period_end, |d| d.min(period_end));
        let delay = clock.host_delay(wake_at);
        let (guard, timeout) =
            shared.cv.wait_timeout(s, delay).unwrap_or_else(std::sync::PoisonError::into_inner);
        s = guard;
        if !timeout.timed_out() || s.quit || !s.running {
            continue; // re-armed, stopping, or paused
        }
        let woke = clock.now_ns();
        let mut kick = false;
        if s.due.is_some_and(|d| woke >= d) {
            s.due = None;
            kick = true;
        }
        let r = shared.refreshes.load(Ordering::Relaxed);
        if woke >= period_end {
            if r == seen && shared.in_guest.load(Ordering::Acquire) {
                kick = true;
                shared.ticks.fetch_add(1, Ordering::Relaxed);
                periods = 1;
            } else if adaptive {
                periods = (periods * 2).min(MAX_BACKOFF);
            }
            seen = r;
        }
        if kick {
            wake.kick();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_clock_stands_still_at_zero_until_resumed() {
        let c = GuestClock::new();
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(c.now_ns(), 0);
        assert!(c.is_still());
        c.pause_end();
        std::thread::sleep(Duration::from_millis(5));
        let t = c.now_ns();
        assert!((4_000_000..1_000_000_000).contains(&t), "{t}");
    }

    #[test]
    fn a_delay_is_the_same_after_a_state_set_the_time_past_this_runs_own() {
        let c = GuestClock::new();
        // a savestate taken five minutes into a session, loaded at once
        c.set_now(300_000_000_000);
        c.pause_end();
        let now = c.now_ns();
        assert!(now >= 300_000_000_000);
        let d = c.host_delay(now + 5_000_000);
        assert!(d <= Duration::from_millis(5) && d > Duration::from_millis(4), "{d:?}");
        assert_eq!(c.host_delay(now), Duration::ZERO, "a time already past");
    }

    #[test]
    fn a_pause_is_left_out_of_guest_time() {
        let c = GuestClock::new();
        c.pause_end();
        c.pause_begin();
        let before = c.now_ns();
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(c.now_ns(), before);
        let out = c.pause_end();
        assert!(out >= 20_000_000);
        assert!(c.now_ns() - before < 15_000_000);
    }

    #[test]
    fn a_short_hold_leaves_nothing_out_and_a_long_one_stops_time_after_8_ms() {
        let c = GuestClock::new();
        c.pause_end();
        assert!(c.hold_begin());
        let e = c.hold_end();
        assert_eq!(e.left_out_ns, 0);
        assert!(c.hold_begin());
        let t0 = c.now_ns();
        std::thread::sleep(Duration::from_millis(30));
        let during = c.now_ns() - t0;
        assert!((7_000_000..12_000_000).contains(&during), "{during}");
        let e = c.hold_end();
        assert!(e.left_out_ns >= 20_000_000 && e.held_ns >= 30_000_000);
        let st = c.take_hold_stats();
        assert_eq!(st.holds, 2);
        assert_eq!(c.take_hold_stats(), HoldStats::default());
    }

    #[test]
    fn a_hold_while_paused_does_nothing() {
        let c = GuestClock::new();
        assert!(!c.hold_begin());
    }

    #[test]
    fn a_wake_ends_the_idle_wait_and_is_consumed() {
        let w = WakeHandle::new();
        assert!(!w.wait(Duration::from_millis(1)));
        let w2 = w.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            w2.wake();
        });
        assert!(w.wait(Duration::from_secs(10)));
        assert!(!w.take());
        t.join().unwrap();
    }

    #[test]
    fn the_ticker_kicks_when_something_is_due() {
        let clock = GuestClock::new();
        clock.pause_end();
        let wake = WakeHandle::new();
        let kicks = Arc::new(AtomicU32::new(0));
        let k = Arc::clone(&kicks);
        wake.set_kicker(Some(Kicker::new(move || {
            k.fetch_add(1, Ordering::SeqCst);
        })));
        let t = Ticker::new(clock.clone(), wake);
        t.set_running(true);
        t.arm(Some(clock.now_ns() + 3_000_000));
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(
            kicks.load(Ordering::SeqCst),
            1,
            "one kick for the due time, none for ticks outside guest code"
        );
        t.set_in_guest(true);
        std::thread::sleep(Duration::from_millis(20));
        assert!(
            kicks.load(Ordering::SeqCst) > 5,
            "a tick a millisecond while guest code runs unrefreshed"
        );
        assert!(t.ticks() > 4);
    }
}
