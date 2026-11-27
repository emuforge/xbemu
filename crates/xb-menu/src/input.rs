//! Turning pad state into menu input: the open/close combo, the navigation
//! buttons with edge detection, and auto-repeat for held directions.
//!
//! All timing comes from the `dt` the caller passes; nothing here reads a
//! clock.

use core::time::Duration;

use xb_types::pad::{PORTS, PadState, analog, button};

/// How long the combo must stay held to open the menu.
///
/// A combo is made of buttons titles bind. With both stick clicks: in Halo
/// the left click crouches (held) and the right click zooms, so a crouched
/// player zooming holds both for a moment. Half a second is longer than such an
/// overlap and short enough to feel deliberate. Closing needs no hold: with
/// the menu up, the combo has no other meaning.
pub const COMBO_HOLD: Duration = Duration::from_millis(500);

/// How long a direction is held before it starts repeating.
pub const REPEAT_DELAY: Duration = Duration::from_millis(400);

/// Time between repeats of a held direction.
pub const REPEAT_INTERVAL: Duration = Duration::from_millis(100);

/// Left-stick deflection that starts a move, about 60% of full travel.
const STICK_ON: i16 = 20000;
/// Deflection below which a stick move ends. Lower than [`STICK_ON`] so a
/// stick resting near the threshold does not chatter.
const STICK_OFF: i16 = 12000;

/// Navigation inputs as bits.
pub(crate) mod nav {
    pub(crate) const UP: u8 = 1 << 0;
    pub(crate) const DOWN: u8 = 1 << 1;
    pub(crate) const LEFT: u8 = 1 << 2;
    pub(crate) const RIGHT: u8 = 1 << 3;
    pub(crate) const A: u8 = 1 << 4;
    pub(crate) const B: u8 = 1 << 5;
    pub(crate) const START: u8 = 1 << 6;
    pub(crate) const X: u8 = 1 << 7;
    /// The inputs that auto-repeat.
    pub(crate) const DIRECTIONS: u8 = UP | DOWN | LEFT | RIGHT;
}

/// The pads the menu reads: the first [`PORTS`] of the slice.
pub(crate) fn ports(pads: &[PadState]) -> &[PadState] {
    &pads[..pads.len().min(PORTS)]
}

/// Which buttons are down on any pad.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Held {
    /// Digital buttons ([`button`] bits).
    pub(crate) buttons: u16,
    /// Analog buttons past the "down" threshold, bit `i` for index `i` of
    /// [`PadState::analog`].
    pub(crate) analog: u8,
}

impl Held {
    /// Collects the buttons down across `pads`.
    pub(crate) fn of(pads: &[PadState]) -> Self {
        let mut held = Self::default();
        for pad in pads {
            held.buttons |= pad.buttons;
            for index in 0..pad.analog.len() {
                held.analog |= u8::from(pad.analog_down(index)) << index;
            }
        }
        held
    }
}

/// The navigation inputs down across `pads`, leaving out the buttons in
/// `hidden`. `prev` is the previous result; it gives the stick hysteresis.
pub(crate) fn nav_bits(pads: &[PadState], hidden: Held, prev: u8) -> u8 {
    let held = Held::of(pads);
    let buttons = held.buttons & !hidden.buttons;
    let analogs = held.analog & !hidden.analog;
    // The left stick points one way at a time, along the axis it leans
    // further on: a diagonal push must not both move the selection and
    // change the row's value.
    let stick = |bit: u8, towards: fn(&PadState) -> i32, across: fn(&PadState) -> i32| {
        let threshold = if prev & bit != 0 { STICK_OFF } else { STICK_ON };
        pads.iter().any(|pad| {
            let along = towards(pad);
            along >= i32::from(threshold) && along >= across(pad).abs()
        })
    };
    fn x(p: &PadState) -> i32 {
        i32::from(p.left_x)
    }
    fn y(p: &PadState) -> i32 {
        i32::from(p.left_y)
    }
    let mut bits = 0;
    let mut set = |bit: u8, down: bool| bits |= if down { bit } else { 0 };
    set(nav::UP, buttons & button::DPAD_UP != 0 || stick(nav::UP, y, x));
    set(nav::DOWN, buttons & button::DPAD_DOWN != 0 || stick(nav::DOWN, |p| -y(p), x));
    set(nav::LEFT, buttons & button::DPAD_LEFT != 0 || stick(nav::LEFT, |p| -x(p), y));
    set(nav::RIGHT, buttons & button::DPAD_RIGHT != 0 || stick(nav::RIGHT, x, y));
    set(nav::START, buttons & button::START != 0);
    set(nav::A, analogs & 1 << analog::A != 0);
    set(nav::B, analogs & 1 << analog::B != 0);
    set(nav::X, analogs & 1 << analog::X != 0);
    bits
}

/// Detects the open/close combo: once per press, and only after every combo
/// button was released on the pad that presses it.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ComboDetector {
    /// Bit per port: the pad has had all combo buttons up since the last
    /// trigger. Starts clear, so a combo held at power-on does nothing.
    armed: u8,
    /// How long an armed pad has held the full combo.
    held_for: Duration,
}

impl ComboDetector {
    /// Feeds one frame of pad state. Returns `true` on the frame the combo
    /// triggers: at once when `hold` is zero, else after it has been held
    /// that long. All of `combo` must be down on a single pad.
    pub(crate) fn update(
        &mut self,
        combo: u16,
        pads: &[PadState],
        dt: Duration,
        hold: Duration,
    ) -> bool {
        let mut complete = false;
        for (port, pad) in pads.iter().enumerate() {
            if pad.buttons & combo == 0 {
                self.armed |= 1 << port;
            } else if self.armed & 1 << port != 0 && pad.all_down(combo) {
                complete = true;
            }
        }
        if !complete {
            self.held_for = Duration::ZERO;
            return false;
        }
        self.held_for = self.held_for.saturating_add(dt);
        if self.held_for < hold {
            return false;
        }
        self.armed = 0;
        self.held_for = Duration::ZERO;
        true
    }
}

/// Auto-repeat for the direction inputs.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Repeat {
    /// The direction being repeated, one [`nav`] bit or 0.
    direction: u8,
    /// Time until its next repeat.
    wait: Duration,
}

impl Repeat {
    /// Forgets the held direction; it must be pressed again to act.
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    /// Returns the direction to act on this frame, or 0: a newly pressed
    /// direction at once, then the same one after [`REPEAT_DELAY`] and every
    /// [`REPEAT_INTERVAL`] while it stays down. At most one per call, however
    /// long `dt` is, so a stalled frame cannot run the selection away.
    pub(crate) fn update(&mut self, down: u8, pressed: u8, dt: Duration) -> u8 {
        let pressed = pressed & nav::DIRECTIONS;
        if pressed != 0 {
            // Lowest bit first: up, down, left, right.
            self.direction = pressed & pressed.wrapping_neg();
            self.wait = REPEAT_DELAY;
            return self.direction;
        }
        if down & self.direction == 0 {
            self.reset();
            return 0;
        }
        if dt < self.wait {
            self.wait -= dt;
            return 0;
        }
        self.wait = REPEAT_INTERVAL;
        self.direction
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMBO: u16 = button::LEFT_THUMB | button::RIGHT_THUMB;
    const FRAME: Duration = Duration::from_millis(10);

    fn pad(buttons: u16) -> PadState {
        PadState { buttons, ..PadState::default() }
    }

    #[test]
    fn the_combo_triggers_once_however_long_it_is_held() {
        let mut combo = ComboDetector::default();
        assert!(!combo.update(COMBO, &[pad(0)], FRAME, Duration::ZERO));
        let fired = (0..500).filter(|_| combo.update(COMBO, &[pad(COMBO)], FRAME, Duration::ZERO));
        assert_eq!(fired.count(), 1);
    }

    #[test]
    fn the_combo_needs_a_full_release_before_it_triggers_again() {
        let mut combo = ComboDetector::default();
        let mut step = |buttons| combo.update(COMBO, &[pad(buttons)], FRAME, Duration::ZERO);
        assert!(!step(0));
        assert!(step(COMBO));
        // One button up and down again is not a new press.
        assert!(!step(button::LEFT_THUMB));
        assert!(!step(COMBO));
        assert!(!step(0));
        assert!(step(COMBO));
    }

    #[test]
    fn a_combo_held_from_the_first_frame_does_nothing() {
        let mut combo = ComboDetector::default();
        for _ in 0..200 {
            assert!(!combo.update(COMBO, &[pad(COMBO)], FRAME, Duration::ZERO));
        }
    }

    #[test]
    fn the_hold_time_counts_only_unbroken_holding() {
        let mut combo = ComboDetector::default();
        let hold = Duration::from_millis(100);
        let mut step = |buttons| combo.update(COMBO, &[pad(buttons)], FRAME, hold);
        assert!(!step(0));
        for _ in 0..9 {
            assert!(!step(COMBO));
        }
        // A break restarts the count.
        assert!(!step(button::RIGHT_THUMB));
        for _ in 0..9 {
            assert!(!step(COMBO));
        }
        assert!(step(COMBO));
    }

    #[test]
    fn the_combo_must_be_on_one_pad_and_any_port_may_press_it() {
        let mut combo = ComboDetector::default();
        let split = [pad(button::LEFT_THUMB), pad(button::RIGHT_THUMB)];
        assert!(!combo.update(COMBO, &[pad(0), pad(0)], FRAME, Duration::ZERO));
        assert!(!combo.update(COMBO, &split, FRAME, Duration::ZERO));
        assert!(!combo.update(COMBO, &[pad(0), pad(0), pad(0)], FRAME, Duration::ZERO));
        assert!(combo.update(COMBO, &[pad(0), pad(0), pad(COMBO)], FRAME, Duration::ZERO));
    }

    #[test]
    fn extra_buttons_do_not_prevent_the_combo() {
        let mut combo = ComboDetector::default();
        assert!(!combo.update(COMBO, &[pad(0)], FRAME, Duration::ZERO));
        assert!(combo.update(COMBO, &[pad(COMBO | button::DPAD_UP)], FRAME, Duration::ZERO));
    }

    #[test]
    fn a_held_direction_repeats_after_the_delay_at_the_interval() {
        let mut repeat = Repeat::default();
        let ms = Duration::from_millis(1);
        assert_eq!(repeat.update(nav::DOWN, nav::DOWN, ms), nav::DOWN);
        let mut fired_at = Vec::new();
        for t in 1..=1000u64 {
            if repeat.update(nav::DOWN, 0, ms) != 0 {
                fired_at.push(Duration::from_millis(t));
            }
        }
        assert_eq!(fired_at[0], REPEAT_DELAY);
        for pair in fired_at.windows(2) {
            assert_eq!(pair[1] - pair[0], REPEAT_INTERVAL);
        }
        let expected =
            1 + (Duration::from_secs(1) - REPEAT_DELAY).as_millis() / REPEAT_INTERVAL.as_millis();
        assert_eq!(fired_at.len() as u128, expected);
    }

    #[test]
    fn releasing_a_direction_stops_the_repeat() {
        let mut repeat = Repeat::default();
        assert_eq!(repeat.update(nav::UP, nav::UP, FRAME), nav::UP);
        assert_eq!(repeat.update(0, 0, REPEAT_DELAY * 3), 0);
        assert_eq!(repeat.update(nav::UP, 0, REPEAT_DELAY * 3), 0);
    }

    #[test]
    fn a_long_frame_gives_one_repeat_not_a_burst() {
        let mut repeat = Repeat::default();
        repeat.update(nav::UP, nav::UP, FRAME);
        assert_eq!(repeat.update(nav::UP, 0, Duration::from_secs(30)), nav::UP);
        assert_eq!(repeat.update(nav::UP, 0, FRAME), 0);
    }

    #[test]
    fn a_new_direction_takes_over_the_repeat() {
        let mut repeat = Repeat::default();
        repeat.update(nav::UP, nav::UP, FRAME);
        assert_eq!(repeat.update(nav::UP | nav::RIGHT, nav::RIGHT, FRAME), nav::RIGHT);
        assert_eq!(repeat.update(nav::UP | nav::RIGHT, 0, REPEAT_DELAY), nav::RIGHT);
    }

    #[test]
    fn the_stick_moves_with_hysteresis() {
        let stick = |left_y| [PadState { left_y, ..PadState::default() }];
        let none = Held::default();
        assert_eq!(nav_bits(&stick(STICK_ON - 1), none, 0), 0);
        assert_eq!(nav_bits(&stick(STICK_ON), none, 0), nav::UP);
        assert_eq!(nav_bits(&stick(STICK_OFF), none, nav::UP), nav::UP);
        assert_eq!(nav_bits(&stick(STICK_OFF - 1), none, nav::UP), 0);
        assert_eq!(nav_bits(&stick(i16::MIN), none, 0), nav::DOWN);
    }

    #[test]
    fn the_stick_points_left_and_right_too_and_one_way_at_a_time() {
        let stick = |left_x, left_y| [PadState { left_x, left_y, ..PadState::default() }];
        let none = Held::default();
        assert_eq!(nav_bits(&stick(STICK_ON, 0), none, 0), nav::RIGHT);
        assert_eq!(nav_bits(&stick(-STICK_ON, 0), none, 0), nav::LEFT);
        assert_eq!(nav_bits(&stick(STICK_ON - 1, 0), none, 0), 0);
        assert_eq!(nav_bits(&stick(STICK_OFF, 0), none, nav::RIGHT), nav::RIGHT);
        // A diagonal is the direction it leans further in.
        assert_eq!(nav_bits(&stick(30000, 25000), none, 0), nav::RIGHT);
        assert_eq!(nav_bits(&stick(-25000, 30000), none, 0), nav::UP);
        assert_eq!(nav_bits(&stick(i16::MIN, i16::MIN + 1), none, 0), nav::LEFT);
    }

    #[test]
    fn hidden_buttons_are_not_navigation_input() {
        let mut p = pad(button::START | button::DPAD_LEFT);
        p.analog[analog::A] = 255;
        p.analog[analog::B] = 255;
        let all = nav::START | nav::LEFT | nav::A | nav::B;
        assert_eq!(nav_bits(&[p], Held::default(), 0), all);
        let hidden = Held { buttons: button::START, analog: 1 << analog::A };
        assert_eq!(nav_bits(&[p], hidden, 0), nav::LEFT | nav::B);
    }

    #[test]
    fn pads_past_the_last_port_are_ignored() {
        let pads = [PadState::default(); PORTS + 2];
        assert_eq!(ports(&pads).len(), PORTS);
        assert_eq!(ports(&pads[..1]).len(), 1);
    }
}
