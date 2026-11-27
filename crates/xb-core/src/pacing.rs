//! When the frame-latency hold is worth taking.
//!
//! The hold (`xb_kernel`'s `frame.rs`) makes the thread that finished a
//! frame wait for the next flip before it starts another, so each frame
//! begins at a flip: without it a host faster than the console lets the
//! title run frames ahead until its push buffer is full, and its frames
//! then come unevenly. That costs nothing while a whole frame (the title's
//! work, the decoder's, the renderer's) fits in one refresh. When it does
//! not, the frames ahead are what keeps the rate: the title draws the next
//! frame while the renderer finishes the last (Tiger Woods PGA Tour 2004's
//! lessons on a small host).
//!
//! So the hold is taken only while the title keeps its rate with it: one
//! frame every refresh, or every second one (a 30 Hz title on a 60 Hz mode).
//! Once the rate falls away while holding, the hold is off, and it comes
//! back only after the title has kept the full rate for a while without it.

/// A rate this far under the refresh rate (or under half of it) is kept.
const KEPT: f64 = 0.90;
/// How close to half the refresh rate a steady half-rate title stays.
const HALF_BAND: f64 = 0.08;
/// Without the hold, the rate that says there is room for it again.
const ROOM: f64 = 0.985;
/// Seconds under its rate before the hold goes.
const OFF_AFTER: u32 = 2;
/// Seconds at the full rate before it comes back.
const ON_AFTER: u32 = 8;

/// Decides second by second whether the hold is taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HoldPolicy {
    on: bool,
    /// Consecutive seconds that speak for changing `on`.
    run: u32,
}

impl Default for HoldPolicy {
    fn default() -> Self {
        Self { on: true, run: 0 }
    }
}

impl HoldPolicy {
    /// One more second: `flips` frames were shown in it and the display
    /// mode refreshed `vblanks` times. Returns whether the hold is taken.
    pub(crate) fn second(&mut self, flips: f64, vblanks: f64) -> bool {
        if vblanks < 1.0 {
            return self.on; // paused, or no mode yet: nothing to judge by
        }
        let part = flips / vblanks;
        let change = if self.on {
            // Keeping the refresh rate, or steadily half of it.
            let kept = part >= KEPT || (part - 0.5).abs() <= 0.5 * HALF_BAND;
            !kept
        } else {
            part >= ROOM
        };
        self.run = if change { self.run + 1 } else { 0 };
        if self.run >= if self.on { OFF_AFTER } else { ON_AFTER } {
            self.on = !self.on;
            self.run = 0;
        }
        self.on
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(p: &mut HoldPolicy, flips: f64, seconds: u32) -> bool {
        (0..seconds).map(|_| p.second(flips, 60.0)).last().unwrap()
    }

    #[test]
    fn a_title_that_keeps_its_rate_is_held_and_one_that_falls_away_is_not() {
        let mut p = HoldPolicy::default();
        assert!(run(&mut p, 60.0, 30), "a full rate: held");
        assert!(run(&mut p, 57.0, 30), "a few late frames: still held");
        // Tiger Woods' lesson: 37 with the hold.
        assert!(p.second(37.0, 60.0), "one bad second is not enough");
        assert!(!p.second(37.0, 60.0), "two are");
        // Without it the title runs at 56, which is not room enough to hold
        // again: it stays off, and the rate stays.
        assert!(!run(&mut p, 56.0, 120));
        // A lighter scene keeps the whole rate for a while: held again.
        assert!(!run(&mut p, 60.0, ON_AFTER - 1));
        assert!(p.second(60.0, 60.0));
    }

    #[test]
    fn a_steady_half_rate_title_is_held_and_a_rate_between_is_not() {
        let mut p = HoldPolicy::default();
        assert!(run(&mut p, 30.0, 30), "Halo: 30 on a 60 Hz mode");
        assert!(run(&mut p, 29.0, 30));
        assert!(!run(&mut p, 45.0, 3), "neither rate: frames are being lost to it");
        let mut p = HoldPolicy::default();
        assert!(!run(&mut p, 20.0, 3));
    }

    #[test]
    fn a_pause_changes_nothing() {
        let mut p = HoldPolicy::default();
        for _ in 0..10 {
            assert!(p.second(0.0, 0.0));
        }
        let mut off = HoldPolicy { on: false, run: 0 };
        assert!(!off.second(0.0, 0.0));
    }
}
