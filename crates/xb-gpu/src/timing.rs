//! Display timing of the NV2A front end: where each vertical blank falls,
//! how it keeps to the host display's refreshes when it can, and when a
//! flipped frame is due on screen.
//!
//! The vblank always runs at the video mode's rate. The reference's
//! "uncapped" frame-rate modes (early vblanks while PGRAPH waits in
//! FLIP_STALL, the frame limits) are not implemented.
//!
//! The vblank runs free on the host clock unless something reports the host
//! display's refreshes ([`Nv2a::display_vsync`] from a compositor callback,
//! or [`Nv2a::display_sync`] from a presenter that knows the driver's vsync
//! instants). With a display whose refresh period a frame is a whole
//! multiple of, the vblank then locks to those refreshes: free running at
//! 59.94 Hz beside a 60 Hz panel, two frames now and then fall in one refresh
//! and one is never shown. On Linux nothing feeds it yet (the reference has
//! no source there either), so the vblank runs free.

use std::sync::atomic::Ordering;

use crate::nv2a::{Clock, Decoder, Nv2a};

/// The vblank's phase before a vsync until the presenter's pickups are seen.
const VSYNC_LEAD: i64 = 3_000_000;
/// Vsync reports older than this are not followed.
const VSYNC_STALE: u64 = 200_000_000;
/// A timed present goes this long after the vsync before (displays above
/// 60 Hz)...
const PRESENT_AFTER: i64 = 1_500_000;
/// ... leaving at most this much room before the next one.
const PRESENT_ROOM: i64 = 8_333_333;
/// How near whole refreshes a frame must be to lock to them: 1 %. A 59.94 Hz
/// vblank beside a 60 Hz display is 0.1 % off; at 5 % a 50 Hz title locked
/// to three refreshes of a 144 Hz display and ran 4 % slow (Crash Bandicoot's
/// European disc).
const LOCK_PART: u64 = 100;
/// A step's present comes this long after a vsync when frames are a fraction
/// of a refresh apart.
const CAP_STEP_AFTER: i64 = 500_000;
/// How long around its slot a trap handler's flip is taken to be on it.
const FRAME_DUE_SPAN: u64 = 4_000_000;
/// Slack added to half a pace when deciding a flip came late on its slot.
const EARLY_POLL_NS: u64 = 250_000;

/// How many pickups the turn round the refresh is summed over.
const PICK_TURN_N: usize = 64;

/// The presenter's pickups' turn round the refresh over the last
/// [`PICK_TURN_N`] of them ([`Nv2a::display_pickup`]).
#[derive(Debug)]
pub(crate) struct PickTurn {
    /// Each pickup's step from the one before.
    step: [i64; PICK_TURN_N],
    /// Their sum.
    turn: i64,
    /// The last pickup's place after a vsync; negative: none yet.
    last: i64,
    /// Pickups counted.
    n: usize,
    /// The pickups turn (logged when it changes).
    turning: bool,
}

impl Default for PickTurn {
    fn default() -> Self {
        Self { step: [0; PICK_TURN_N], turn: 0, last: -1, n: 0, turning: false }
    }
}

/// The host display's refreshes as the vblank keeps to them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Grid {
    /// One vsync's instant, host ns.
    pub vsync: u64,
    /// The refresh period (or a whole fraction of it, [`Nv2a::every_grid`]).
    pub period: u64,
    /// The phase a vblank takes after a vsync.
    pub at: i64,
    /// The phase is placed for a presenter's timed presents.
    pub timed: bool,
}

/// `x` wrapped into `(-period / 2, period / 2]`.
fn wrap_half(x: i64, period: i64) -> i64 {
    let mut x = x % period;
    if x <= -period / 2 {
        x += period;
    } else if x > period / 2 {
        x -= period;
    }
    x
}

/// How long before a vsync a timed present goes: half a refresh at 60 Hz and
/// below; above it [`PRESENT_AFTER`] past the vsync before, up to
/// [`PRESENT_ROOM`].
fn present_room(period: u64) -> i64 {
    let half = period as i64 / 2;
    let room = (period as i64 - PRESENT_AFTER).min(PRESENT_ROOM);
    room.max(half)
}

/// The first instant at or after `t` on `grid`.
fn grid_at_or_after(t: u64, grid: &Grid) -> u64 {
    let base = grid.vsync as i64 + grid.at;
    let d = t as i64 - base;
    let p = grid.period as i64;
    let n = if d >= 0 { (d + p - 1) / p } else { -((-d) / p) };
    (base + n * p) as u64
}

impl<D: Decoder, C: Clock> Nv2a<D, C> {
    // ---- what the front end and the presenter report ----

    /// A refresh of the host display happened at `ns` (host clock), as a
    /// compositor callback reports it. The refresh period is measured from
    /// successive calls.
    pub fn display_vsync(&self, ns: u64) {
        let last = self.disp_vsync_ns.load(Ordering::Acquire);
        if last != 0 && ns > last {
            let d = ns - last;
            let mut p = self.disp_period_ns.load(Ordering::Relaxed);
            // A callback that came late spans whole periods.
            let n = (d + p / 2).checked_div(p).unwrap_or(1);
            let one = d.checked_div(n).unwrap_or(d);
            if (1..=4).contains(&n) && one > 5_000_000 && one < 50_000_000 {
                p = if p != 0 { (p as i64 + (one as i64 - p as i64) / 16) as u64 } else { one };
                self.disp_period_ns.store(p, Ordering::Relaxed);
            }
        }
        self.disp_vsync_ns.store(ns, Ordering::Release);
    }

    /// The display mode's nominal refresh period (0: unknown). Reported
    /// vsyncs that disagree with it by more than 2 % are a variable refresh
    /// rate display following the presents, and are not locked to.
    pub fn display_nominal(&self, period_ns: u64) {
        self.disp_nominal_ns.store(period_ns, Ordering::Relaxed);
    }

    /// The vsyncs as a presenter has them from the driver (the last one's
    /// instant and the period) and the lead its timed presents keep after a
    /// frame's vblank; 0 lead when they are not timed.
    /// (`hle_host_display_sync`.)
    pub fn display_sync(&self, vsync_ns: u64, period_ns: u64, lead_ns: u64) {
        self.sync_vsync_ns.store(vsync_ns, Ordering::Relaxed);
        self.sync_period_ns.store(period_ns, Ordering::Relaxed);
        self.sync_lead_ns.store(lead_ns, Ordering::Relaxed);
        self.sync_fed_ns.store(self.clock.now_ns(), Ordering::Release);
    }

    /// The presenter looked for a frame at `ns`: the vblank keeps half a
    /// period clear of where it picks frames up.
    ///
    /// A presenter that picks frames up as its GPU finishes them rather than
    /// by the vsyncs (a GPU at its limit) takes them round the refresh at
    /// its own pace, and an estimate that followed them would take the
    /// vblank's grid along and change the title's frame rate. While the
    /// pickups turn more than a quarter of a refresh over the last
    /// `PICK_TURN_N`, the estimate stays where it was; a presenter's
    /// jitter, back and forth, adds up to less. As the reference does
    /// (Crash Bandicoot with the frame rate limit on a GPU at its limit).
    pub fn display_pickup(&self, ns: u64) {
        let vs = self.disp_vsync_ns.load(Ordering::Acquire);
        let pd = self.disp_period_ns.load(Ordering::Relaxed) as i64;
        if vs == 0 || pd == 0 {
            return;
        }
        let mut off = (ns as i64 - vs as i64) % pd;
        if off < 0 {
            off += pd;
        }
        let est = self.pick_off_ns.load(Ordering::Relaxed);
        {
            let mut t = crate::nv2a::lock(&self.pick_turn);
            let step = if t.last < 0 { 0 } else { wrap_half(off - t.last, pd) };
            t.last = off;
            let slot = t.n % PICK_TURN_N;
            t.turn += step - t.step[slot];
            t.step[slot] = step;
            t.n += 1;
            let turning = t.n >= PICK_TURN_N && t.turn.abs() > pd / 4;
            if turning != t.turning {
                t.turning = turning;
                log::info!(
                    target: "gpu",
                    "the presenter's pickups {} ({:.1} ms over the last {PICK_TURN_N})",
                    if turning {
                        "turn round the refresh (it waits for its GPU): the vblank keeps its place"
                    } else {
                        "keep to the vsyncs again"
                    },
                    t.turn as f64 / 1e6
                );
            }
            if turning && est >= 0 {
                return;
            }
        }
        let est = if est < 0 { off } else { est + wrap_half(off - est, pd) / 16 };
        self.pick_off_ns.store((est % pd + pd) % pd, Ordering::Relaxed);
    }

    /// The first reported vsync after `after`, or 0 when none are reported
    /// (or they went stale).
    pub fn next_vsync(&self, after: u64) -> u64 {
        let vs = self.disp_vsync_ns.load(Ordering::Acquire);
        let pd = self.disp_period_ns.load(Ordering::Relaxed);
        if vs == 0 || pd == 0 || (after > vs && after - vs > VSYNC_STALE) {
            return 0;
        }
        if after < vs { vs } else { vs + ((after - vs) / pd + 1) * pd }
    }

    /// Whether the vblank keeps to the display's refreshes.
    pub fn vblank_locked(&self) -> bool {
        self.vblank_locked.load(Ordering::Relaxed)
    }

    /// ... and is placed for the presenter's timed presents
    /// (`hle_host_vblank_timed`).
    pub fn vblank_timed(&self) -> bool {
        self.vblank_timed.load(Ordering::Relaxed)
    }

    /// The vblank period of the video mode, ns (`hle_host_vblank_period_ns`).
    pub fn vblank_period_ns(&self) -> u64 {
        self.frame_ns.load(Ordering::Relaxed)
    }

    /// When the last vblank was due, host ns (`hle_host_vblank_ns`).
    pub fn vblank_ns(&self) -> u64 {
        self.last_vblank_ns.load(Ordering::Acquire)
    }

    // ---- placing the vblank ----

    /// The display's refreshes to keep to, or `None` while there are none (no
    /// reports, stale ones, or not the display's own: a variable refresh rate
    /// display's follow the presents).
    pub(crate) fn display_grid(&self) -> Option<Grid> {
        let mut vs = self.disp_vsync_ns.load(Ordering::Acquire);
        let mut pd = self.disp_period_ns.load(Ordering::Relaxed);
        // The presenter's own while they come: exact, where the compositor's
        // are a thread's wake.
        let now = self.clock.now_ns();
        let fed = self.sync_fed_ns.load(Ordering::Acquire);
        let exact = fed != 0
            && now.wrapping_sub(fed) < VSYNC_STALE
            && self.sync_period_ns.load(Ordering::Relaxed) != 0;
        let mut lead = 0;
        if exact {
            vs = self.sync_vsync_ns.load(Ordering::Relaxed);
            pd = self.sync_period_ns.load(Ordering::Relaxed);
            lead = self.sync_lead_ns.load(Ordering::Relaxed);
        }
        if vs == 0 || pd == 0 || (!exact && now.wrapping_sub(vs) > VSYNC_STALE) {
            return None;
        }
        let nominal = self.disp_nominal_ns.load(Ordering::Relaxed);
        if nominal != 0 && (pd as i64 - nominal as i64).unsigned_abs() > nominal / 50 {
            return None;
        }
        let pick = self.pick_off_ns.load(Ordering::Relaxed);
        let mut at = if pick < 0 { -VSYNC_LEAD } else { pick + pd as i64 / 2 };
        if lead != 0 {
            // The presenter shows a frame `lead` after its vblank: the vblank
            // that much before the middle of a refresh, so the present falls
            // midway between two vsyncs, as far as can be from both, and each
            // frame is on for the same refreshes (Burnout 3 at 60 Hz showed
            // one frame twice and the next never, once in 45 s, when the
            // phase followed the pickups instead).
            at = pd as i64 - present_room(pd) - (lead % pd) as i64;
        }
        Some(Grid { vsync: vs, period: pd, at, timed: lead != 0 })
    }

    /// The vblank after the one at `prev`: a frame later, or locked to the
    /// display (its period times the refreshes per frame, the phase nudged at
    /// most 0.5 ms a vblank so the guest's clock never jumps). Returns the
    /// instant and whether it is locked and timed.
    fn place_vblank(&self, prev: u64) -> (u64, bool, bool) {
        let frame = self.frame_ns.load(Ordering::Relaxed);
        let Some(g) = self.display_grid() else {
            // No display's vsyncs to keep to: free running.
            return (prev + frame, false, false);
        };
        let k = (frame + g.period / 2) / g.period;
        if !(1..=4).contains(&k) || (k * g.period).abs_diff(frame) > frame / LOCK_PART {
            // 90 Hz and the like: free running.
            return (prev + frame, false, false);
        }
        let next = prev + k * g.period;
        let phase = wrap_half(next as i64 - g.vsync as i64 - g.at, g.period as i64);
        let nudge = (-phase / 8).clamp(-500_000, 500_000);
        ((next as i64 + nudge) as u64, true, g.timed)
    }

    /// The instant of the vblank after the one at `prev`.
    pub(crate) fn next_vblank(&self, prev: u64) -> u64 {
        let (next, locked, timed) = self.place_vblank(prev);
        self.vblank_locked.store(locked, Ordering::Relaxed);
        self.vblank_timed.store(timed, Ordering::Relaxed);
        next
    }

    /// A vblank thread that woke late must not send the vblanks it missed one
    /// after the other: a game that flips at each vblank would flip at that
    /// rate. The console's vblanks come at their time whatever the CPU does,
    /// and a guest that takes its interrupt late takes one. The vblanks
    /// missed by more than a period are dropped and the grid kept.
    pub(crate) fn late_skip(&self, at: u64, step: u64) -> u64 {
        let t = self.clock.now_ns();
        if step == 0 || t <= at + step {
            return at;
        }
        let n = (t - at) / step;
        let count = self.late_wakes.fetch_add(1, Ordering::Relaxed) + 1;
        if count <= 8 || count.is_power_of_two() {
            log::warn!(
                target: "gpu",
                "nv2a: the vblank thread woke {:.1} ms late: {n} vblanks dropped (#{count})",
                (t - at) as f64 / 1e6,
            );
        }
        at + n * step
    }

    // ---- flips ----

    /// The grid frames `every` apart flip on: the display's, or its steps for
    /// a whole fraction of a refresh; `None` when there is no display grid or
    /// the pace is neither.
    fn every_grid(&self, every: u64) -> Option<Grid> {
        if every == 0 {
            return None;
        }
        let mut g = self.display_grid()?;
        let m = (every + g.period / 2) / g.period;
        if m >= 1 && (m * g.period).abs_diff(every) <= every / LOCK_PART {
            return Some(g);
        }
        let n = (g.period + every / 2) / every;
        if !(2..=4).contains(&n) || (g.period / n).abs_diff(every) > every / LOCK_PART {
            return None;
        }
        // Steps of period / n from the display grid's place, all a moment
        // later: the one before a vsync's is then past it too.
        g.period /= n;
        g.at += CAP_STEP_AFTER;
        Some(g)
    }

    /// The least time between two flips a trap handler does (Direct3D's
    /// IMMEDIATE presents): the vblank period (`hle_host_flip_min_ns`).
    pub fn flip_min_ns(&self) -> i64 {
        self.frame_ns.load(Ordering::Relaxed) as i64
    }

    /// A trap handler's flip wants the slot `want` at pace `every`: the place
    /// on the display's vsyncs nearest it when the pace is whole refreshes of
    /// a display the vblank keeps to, else `want` (`hle_host_flip_slot`).
    /// Total Immersion Racing's flips otherwise fall anywhere in a refresh.
    pub fn flip_slot(&self, want: i64, every: i64) -> i64 {
        let mut slot = want;
        if every > 0
            && want > 0
            && let Some(g) = self.every_grid(every as u64)
        {
            slot = grid_at_or_after((want as u64).wrapping_sub(g.period / 2), &g) as i64;
        }
        self.trap_every_ns.store(if every > 0 { every as u64 } else { 0 }, Ordering::Relaxed);
        self.trap_slot_ns
            .store(if every > 0 && slot > 0 { slot as u64 } else { 0 }, Ordering::Release);
        slot
    }

    /// The vblank a flip made now takes effect at, which its frame is due on
    /// screen from: the last one's, or the next one's for a flip in the last
    /// quarter of a frame (`hle_host_flip_vblank_ns`). Total Immersion Racing
    /// flips 1.4 ms before each vblank; due at the one before, its pictures
    /// were always late.
    pub fn flip_vblank_ns(&self) -> u64 {
        let last = self.last_vblank_ns.load(Ordering::Acquire);
        let frame = self.frame_ns.load(Ordering::Relaxed);
        let now = self.clock.now_ns();
        // A trap handler's flip on its pace is due at its slot.
        let ts = self.trap_slot_ns.load(Ordering::Acquire);
        let te = self.trap_every_ns.load(Ordering::Relaxed);
        let late = if te / 2 > FRAME_DUE_SPAN { te / 2 + EARLY_POLL_NS } else { FRAME_DUE_SPAN };
        let given = self.slot_given_ns.load(Ordering::Relaxed);
        if ts != 0 && now + FRAME_DUE_SPAN >= ts && now < ts + late {
            // A slot is one picture's: one that comes before the pace has
            // moved on is due at the slot after the last one given.
            let d = if te != 0 && ts <= given && given - ts < te { given + te } else { ts };
            self.slot_given_ns.store(d, Ordering::Relaxed);
            return d;
        }
        // On the pace but outside its slot's window: the slot after the last
        // one given, when that is within a slot of now (Republic Commando at
        // a 60 Hz limit beside the 59.94 Hz vblank).
        if ts != 0
            && te != 0
            && given != 0
            && now < ts + 3 * te
            && now > given
            && now < given + 2 * te
        {
            self.slot_given_ns.store(given + te, Ordering::Relaxed);
            return given + te;
        }
        let mut d =
            if last != 0 && now >= last + frame - frame / 4 && now < last + frame + frame / 2 {
                last + frame
            } else {
                last
            };
        // A flip on the trap's pace those rules put at or before the slot
        // given last is on the slot after it.
        if ts != 0
            && te != 0
            && given != 0
            && now < ts + 3 * te
            && d < given + te / 2
            && given + te < now + 2 * te
        {
            self.slot_given_ns.store(given + te, Ordering::Relaxed);
            d = given + te;
        }
        d
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nv2a::FRAME_NS_60;
    use crate::nv2a::tests::rig;

    #[test]
    fn wrap_half_folds_into_half_a_period_either_side() {
        assert_eq!(wrap_half(0, 100), 0);
        assert_eq!(wrap_half(50, 100), 50);
        assert_eq!(wrap_half(51, 100), -49);
        assert_eq!(wrap_half(-50, 100), 50);
        assert_eq!(wrap_half(-49, 100), -49);
        assert_eq!(wrap_half(250, 100), 50);
    }

    #[test]
    fn grid_points_are_the_first_at_or_after_an_instant() {
        let g = Grid { vsync: 1000, period: 100, at: 30, timed: false };
        assert_eq!(grid_at_or_after(1030, &g), 1030);
        assert_eq!(grid_at_or_after(1031, &g), 1130);
        assert_eq!(grid_at_or_after(900, &g), 930);
        for t in 800..1400 {
            let p = grid_at_or_after(t, &g);
            assert!(p >= t && p - t < 100 && (p as i64 - 1030).rem_euclid(100) == 0);
        }
    }

    #[test]
    fn without_display_reports_the_vblank_runs_free() {
        let r = rig();
        let now = r.clock.now_ns();
        assert_eq!(r.nv.display_grid(), None);
        assert_eq!(r.nv.next_vblank(now), now + FRAME_NS_60);
        assert!(!r.nv.vblank_locked());
        assert_eq!(r.nv.next_vsync(now), 0);
    }

    /// Reports `n` vsyncs `period` apart ending at the clock's now.
    fn feed(r: &crate::nv2a::tests::Rig, period: u64, n: u64) {
        let now = r.clock.now_ns();
        for i in (0..n).rev() {
            r.nv.display_vsync(now - i * period);
        }
    }

    #[test]
    fn the_vblank_locks_to_a_60_hz_display_and_converges_on_its_phase() {
        let r = rig();
        let period = 16_666_667u64;
        feed(&r, period, 8);
        assert_eq!(r.nv.disp_period_ns.load(Ordering::Relaxed), period);
        let vsync = r.clock.now_ns();
        assert_eq!(r.nv.next_vsync(vsync + 1), vsync + period);

        let mut t = vsync + 5_000_000;
        for _ in 0..200 {
            // Keep the reports fresh as time goes on.
            r.clock.now.store(t, Ordering::SeqCst);
            r.nv.display_vsync(vsync + (t - vsync) / period * period);
            let next = r.nv.next_vblank(t);
            assert!(r.nv.vblank_locked());
            let step = next - t;
            assert!(
                step.abs_diff(period) <= 500_000,
                "one refresh a vblank, nudged at most 0.5 ms"
            );
            t = next;
        }
        // Settled 3 ms before a vsync (no pickups seen yet).
        let phase = wrap_half(t as i64 - vsync as i64 + VSYNC_LEAD, period as i64);
        assert!(phase.abs() < 100_000, "phase error {phase} ns");
    }

    #[test]
    fn pickups_that_turn_round_the_refresh_leave_the_estimate_and_jitter_moves_it() {
        let period = 16_666_667u64;
        // A presenter that picks a frame up every 15 ms beside 60 Hz vsyncs.
        let r = rig();
        feed(&r, period, 8);
        let vsync = r.clock.now_ns();
        let mut held = None;
        for i in 0..400u64 {
            r.nv.display_pickup(vsync + i * 15_000_000);
            let est = r.nv.pick_off_ns.load(Ordering::Relaxed);
            if i >= PICK_TURN_N as u64 {
                assert_eq!(*held.get_or_insert(est), est, "pickup {i} moved the estimate");
            }
        }
        // One that keeps to the vsyncs, 1 ms to either side of 6 ms after.
        let r = rig();
        feed(&r, period, 8);
        let vsync = r.clock.now_ns();
        r.nv.display_pickup(vsync + 12_000_000);
        for i in 1..400u64 {
            let jitter = if i % 2 == 0 { 1_000_000 } else { -1_000_000i64 };
            r.nv.display_pickup((vsync + i * period + 6_000_000).wrapping_add_signed(jitter));
        }
        let est = r.nv.pick_off_ns.load(Ordering::Relaxed);
        assert!((est - 6_000_000).abs() < 1_100_000, "the estimate followed them to {est}");
    }

    #[test]
    fn a_display_that_is_no_multiple_of_a_frame_is_not_locked_to() {
        let r = rig();
        feed(&r, 11_111_111, 8); // 90 Hz
        let now = r.clock.now_ns();
        assert!(r.nv.display_grid().is_some());
        assert_eq!(r.nv.next_vblank(now), now + FRAME_NS_60);
        assert!(!r.nv.vblank_locked());
        // 120 Hz: two refreshes a frame.
        let r = rig();
        feed(&r, 8_333_333, 8);
        let now = r.clock.now_ns();
        let next = r.nv.next_vblank(now);
        assert!(r.nv.vblank_locked());
        assert!((next - now).abs_diff(2 * 8_333_333) <= 500_000);
    }

    #[test]
    fn stale_or_off_nominal_vsyncs_are_ignored() {
        let r = rig();
        feed(&r, 16_666_667, 8);
        assert!(r.nv.display_grid().is_some());
        // A mode that says 144 Hz: the 60 Hz reports are not the panel's.
        r.nv.display_nominal(6_944_444);
        assert_eq!(r.nv.display_grid(), None);
        r.nv.display_nominal(0);
        r.clock.now.fetch_add(VSYNC_STALE + 1, Ordering::SeqCst);
        assert_eq!(r.nv.display_grid(), None);
    }

    #[test]
    fn a_presenters_timed_presents_place_the_vblank_before_mid_refresh() {
        let r = rig();
        let period = 16_666_667u64;
        let now = r.clock.now_ns();
        r.nv.display_sync(now, period, 4_000_000);
        let g = r.nv.display_grid().expect("the presenter's vsyncs");
        assert!(g.timed);
        // Vblank + lead falls half a refresh before the next vsync.
        assert_eq!(g.at + 4_000_000, period as i64 - period as i64 / 2);
        r.nv.next_vblank(now);
        assert!(r.nv.vblank_timed());
    }

    #[test]
    fn a_flip_late_in_a_frame_is_due_at_the_next_vblank() {
        let r = rig();
        let next = r.nv.vblank_step(r.clock.now_ns());
        assert_eq!(r.nv.vblank_ns(), next);
        r.clock.now.store(next + FRAME_NS_60 / 2, Ordering::SeqCst);
        assert_eq!(r.nv.flip_vblank_ns(), next);
        r.clock.now.store(next + FRAME_NS_60 - 1_400_000, Ordering::SeqCst);
        assert_eq!(r.nv.flip_vblank_ns(), next + FRAME_NS_60);
        assert_eq!(r.nv.flip_min_ns(), FRAME_NS_60 as i64);
    }

    #[test]
    fn a_trap_handlers_flips_take_successive_slots_of_their_pace() {
        let r = rig();
        let now = r.clock.now_ns();
        let every = FRAME_NS_60 as i64;
        // No display grid: the slot is the one asked for.
        let slot = r.nv.flip_slot(now as i64 + 1_000_000, every);
        assert_eq!(slot, now as i64 + 1_000_000);
        r.clock.now.store(slot as u64, Ordering::SeqCst);
        assert_eq!(r.nv.flip_vblank_ns(), slot as u64);
        // A second picture before the pace moved on gets the slot after.
        assert_eq!(r.nv.flip_vblank_ns(), slot as u64 + every as u64);
        // A pace of 0 clears the slot.
        assert_eq!(r.nv.flip_slot(5, 0), 5);
        assert_eq!(r.nv.trap_slot_ns.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn slots_snap_to_the_display_grid_when_the_pace_is_whole_refreshes() {
        let r = rig();
        let period = 16_666_667u64;
        feed(&r, period, 8);
        let g = r.nv.display_grid().expect("grid");
        let want = r.clock.now_ns() + 7_000_000;
        let slot = r.nv.flip_slot(want as i64, FRAME_NS_60 as i64) as u64;
        assert_eq!((slot as i64 - g.vsync as i64 - g.at).rem_euclid(period as i64), 0);
        assert!(slot.abs_diff(want) <= period / 2 + 1);
    }
}
