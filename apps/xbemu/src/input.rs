//! Input mapping: host keys and gamepad controls to the console's
//! [`PadState`]. Everything here is a pure function of the host's state, so
//! it is tested without devices; `pads.rs` does the device work.

use gilrs::{Axis, Button};
use winit::keyboard::KeyCode;
use xb_core::PadState;
use xb_core::pad::{analog, button};

/// What a key does to the pad.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Bind {
    /// Holds a digital button ([`button`] bit).
    Digital(u16),
    /// Presses an analog button fully ([`analog`] index).
    Analog(usize),
    /// Pushes the left stick fully along (x, y).
    LeftStick(i8, i8),
    /// Pushes the right stick fully along (x, y).
    RightStick(i8, i8),
}

/// The keyboard layout, by key position (so it is the same on any keyboard
/// language). The index of a row is the key's bit in a key mask.
const KEYS: [(KeyCode, Bind); 25] = [
    (KeyCode::ArrowUp, Bind::Digital(button::DPAD_UP)),
    (KeyCode::ArrowDown, Bind::Digital(button::DPAD_DOWN)),
    (KeyCode::ArrowLeft, Bind::Digital(button::DPAD_LEFT)),
    (KeyCode::ArrowRight, Bind::Digital(button::DPAD_RIGHT)),
    (KeyCode::Enter, Bind::Digital(button::START)),
    (KeyCode::Backspace, Bind::Digital(button::BACK)),
    (KeyCode::KeyF, Bind::Digital(button::LEFT_THUMB)),
    (KeyCode::KeyH, Bind::Digital(button::RIGHT_THUMB)),
    (KeyCode::KeyZ, Bind::Analog(analog::A)),
    (KeyCode::KeyX, Bind::Analog(analog::B)),
    (KeyCode::KeyC, Bind::Analog(analog::X)),
    (KeyCode::KeyV, Bind::Analog(analog::Y)),
    (KeyCode::Digit1, Bind::Analog(analog::WHITE)),
    (KeyCode::Digit2, Bind::Analog(analog::BLACK)),
    (KeyCode::KeyQ, Bind::Analog(analog::LEFT_TRIGGER)),
    (KeyCode::KeyE, Bind::Analog(analog::RIGHT_TRIGGER)),
    (KeyCode::KeyW, Bind::LeftStick(0, 1)),
    (KeyCode::KeyS, Bind::LeftStick(0, -1)),
    (KeyCode::KeyA, Bind::LeftStick(-1, 0)),
    (KeyCode::KeyD, Bind::LeftStick(1, 0)),
    (KeyCode::KeyI, Bind::RightStick(0, 1)),
    (KeyCode::KeyK, Bind::RightStick(0, -1)),
    (KeyCode::KeyJ, Bind::RightStick(-1, 0)),
    (KeyCode::KeyL, Bind::RightStick(1, 0)),
    (KeyCode::NumpadEnter, Bind::Digital(button::START)),
];

/// The bit of `key` in a key mask, or `None` if the key is not bound.
#[must_use]
pub fn key_bit(key: KeyCode) -> Option<u32> {
    KEYS.iter().position(|&(k, _)| k == key).map(|i| 1 << i)
}

/// A stick axis from the number of keys held towards its positive end less
/// those towards its negative end.
fn key_axis(direction: i32) -> i16 {
    match direction.signum() {
        1 => i16::MAX,
        -1 => i16::MIN,
        _ => 0,
    }
}

/// The pad the held keys (a mask of [`key_bit`]s) describe. Opposite stick
/// keys held together cancel.
#[must_use]
pub fn keyboard_pad(mask: u32) -> PadState {
    let mut pad = PadState::default();
    let (mut lx, mut ly, mut rx, mut ry) = (0i32, 0i32, 0i32, 0i32);
    for (i, &(_, bind)) in KEYS.iter().enumerate() {
        if mask & (1 << i) == 0 {
            continue;
        }
        match bind {
            Bind::Digital(bits) => pad.buttons |= bits,
            Bind::Analog(index) => pad.analog[index] = 255,
            Bind::LeftStick(x, y) => {
                lx += i32::from(x);
                ly += i32::from(y);
            }
            Bind::RightStick(x, y) => {
                rx += i32::from(x);
                ry += i32::from(y);
            }
        }
    }
    pad.left_x = key_axis(lx);
    pad.left_y = key_axis(ly);
    pad.right_x = key_axis(rx);
    pad.right_y = key_axis(ry);
    pad
}

/// The keyboard as the in-game menu reads it.
///
/// In a title Enter is Start and Backspace is Back, the console's own two.
/// In the menu a player expects Enter to choose the highlighted row, and
/// the menu takes Start as "close": Enter closed it. While the menu is open
/// Enter is therefore A (select) and Backspace is B (back); the other keys
/// are as in a title (the arrows move, Z and X are A and B too).
///
/// A key held when the menu closes stays the menu's until it is let go, so
/// the Enter that chose "Resume" does not reach the title as Start.
#[derive(Debug, Default)]
pub struct MenuKeys {
    /// Enter and Backspace keys still held from the menu ([`key_bit`]s).
    held_from_menu: u32,
}

impl MenuKeys {
    /// The pad the held keys describe, in the menu or in the title.
    pub fn pad(&mut self, mask: u32, menu_open: bool) -> PadState {
        let remapped = menu_only_bits();
        if !menu_open {
            self.held_from_menu &= mask;
            return keyboard_pad(mask & !self.held_from_menu);
        }
        self.held_from_menu = mask & remapped;
        let mut pad = keyboard_pad(mask & !remapped);
        for (i, &(key, _)) in KEYS.iter().enumerate() {
            if mask & (1 << i) == 0 {
                continue;
            }
            match key {
                KeyCode::Enter | KeyCode::NumpadEnter => pad.analog[analog::A] = 255,
                KeyCode::Backspace => pad.analog[analog::B] = 255,
                _ => {}
            }
        }
        pad
    }
}

/// The [`key_bit`]s of the keys that mean something else in the menu.
fn menu_only_bits() -> u32 {
    [KeyCode::Enter, KeyCode::NumpadEnter, KeyCode::Backspace]
        .into_iter()
        .filter_map(key_bit)
        .fold(0, |bits, bit| bits | bit)
}

/// Whether `key` opens and closes the in-game menu. F1 is the menu's key;
/// Escape does the same, as the key a player tries first. Neither is bound
/// to the pad ([`key_bit`]), so the title never sees them.
#[must_use]
pub const fn is_menu_key(key: KeyCode) -> bool {
    matches!(key, KeyCode::F1 | KeyCode::Escape)
}

/// Whether a gamepad's `button` opens and closes the in-game menu: the
/// Guide button in the pad's centre (the Xbox button on an Xbox 360 or One
/// pad, PS on a PlayStation pad, Home on a Switch pad), which `gilrs` calls
/// [`Button::Mode`]. The console's controller has no such button, so
/// [`gamepad_pad`] gives it to no control and the title never sees it.
#[must_use]
pub const fn is_menu_button(button: Button) -> bool {
    matches!(button, Button::Mode)
}

/// Host buttons that are the console's digital buttons.
const DIGITAL: [(Button, u16); 8] = [
    (Button::DPadUp, button::DPAD_UP),
    (Button::DPadDown, button::DPAD_DOWN),
    (Button::DPadLeft, button::DPAD_LEFT),
    (Button::DPadRight, button::DPAD_RIGHT),
    (Button::Start, button::START),
    (Button::Select, button::BACK),
    (Button::LeftThumb, button::LEFT_THUMB),
    (Button::RightThumb, button::RIGHT_THUMB),
];

/// Host buttons that are the console's pressure-sensitive buttons. A modern
/// pad has no Black and White; its shoulder buttons sit where a thumb would
/// look for them (the layout the Xbox 360 gave these two when it ran
/// original Xbox titles).
const ANALOG: [(Button, usize); 8] = [
    (Button::South, analog::A),
    (Button::East, analog::B),
    (Button::West, analog::X),
    (Button::North, analog::Y),
    (Button::RightTrigger, analog::BLACK),
    (Button::LeftTrigger, analog::WHITE),
    (Button::LeftTrigger2, analog::LEFT_TRIGGER),
    (Button::RightTrigger2, analog::RIGHT_TRIGGER),
];

/// A stick axis from the host's -1.0 to 1.0 (up and right positive, as the
/// console has them).
fn stick(value: f32) -> i16 {
    let v = value.clamp(-1.0, 1.0);
    // Scale each half by its own limit so both ends are reached.
    (if v < 0.0 { v * 32768.0 } else { v * 32767.0 }).round() as i16
}

/// The pad a gamepad's state describes. `pressed` says whether a button is
/// down, `pressure` how far it is pressed (0.0 to 1.0, when the device says;
/// triggers do, most buttons do not) and `axis` where a stick is.
pub fn gamepad_pad(
    pressed: impl Fn(Button) -> bool,
    pressure: impl Fn(Button) -> f32,
    axis: impl Fn(Axis) -> f32,
) -> PadState {
    let mut pad = PadState::default();
    for (host, bits) in DIGITAL {
        if pressed(host) {
            pad.buttons |= bits;
        }
    }
    for (host, index) in ANALOG {
        let p = (pressure(host).clamp(0.0, 1.0) * 255.0).round() as u8;
        pad.analog[index] = if is_trigger(index) && p > 0 {
            p
        } else if pressed(host) {
            // A control the device reports only as down or up is fully pressed.
            255
        } else {
            0
        };
    }
    pad.left_x = stick(axis(Axis::LeftStickX));
    pad.left_y = stick(axis(Axis::LeftStickY));
    pad.right_x = stick(axis(Axis::RightStickX));
    pad.right_y = stick(axis(Axis::RightStickY));
    pad
}

/// Whether an [`analog`] index is one of the two triggers, the only analog
/// controls a modern pad reports pressure for.
const fn is_trigger(index: usize) -> bool {
    index == analog::LEFT_TRIGGER || index == analog::RIGHT_TRIGGER
}

/// Two sources driving one port (the keyboard and the first gamepad): a
/// button is down if either holds it, pressures take the larger, and stick
/// deflections add (saturating), so either source alone is passed through
/// unchanged.
#[must_use]
pub fn merge(a: &PadState, b: &PadState) -> PadState {
    let mut pad = PadState { buttons: a.buttons | b.buttons, ..PadState::default() };
    for (out, (x, y)) in pad.analog.iter_mut().zip(a.analog.iter().zip(&b.analog)) {
        *out = *x.max(y);
    }
    pad.left_x = a.left_x.saturating_add(b.left_x);
    pad.left_y = a.left_y.saturating_add(b.left_y);
    pad.right_x = a.right_x.saturating_add(b.right_x);
    pad.right_y = a.right_y.saturating_add(b.right_y);
    pad
}

#[cfg(test)]
mod tests {

    #[test]
    fn in_the_menu_enter_selects_and_backspace_goes_back() {
        let enter = key_bit(KeyCode::Enter).unwrap();
        let numpad = key_bit(KeyCode::NumpadEnter).unwrap();
        let backspace = key_bit(KeyCode::Backspace).unwrap();
        let mut keys = MenuKeys::default();
        // in a title they are the console's Start and Back
        let pad = keys.pad(enter | backspace, false);
        assert_eq!(pad.buttons, button::START | button::BACK);
        assert_eq!(pad.analog[analog::A], 0);
        // in the menu, A and B, and neither Start (which closes the menu) nor Back
        for key in [enter, numpad] {
            let pad = MenuKeys::default().pad(key, true);
            assert_eq!((pad.buttons, pad.analog[analog::A], pad.analog[analog::B]), (0, 255, 0));
        }
        let pad = keys.pad(backspace, true);
        assert_eq!((pad.buttons, pad.analog[analog::A], pad.analog[analog::B]), (0, 0, 255));
        // the other keys are as in a title
        let down = key_bit(KeyCode::ArrowDown).unwrap();
        assert_eq!(keys.pad(down, true), keyboard_pad(down));
    }

    #[test]
    fn a_key_held_as_the_menu_closes_does_not_reach_the_title_until_let_go() {
        let enter = key_bit(KeyCode::Enter).unwrap();
        let up = key_bit(KeyCode::ArrowUp).unwrap();
        let mut keys = MenuKeys::default();
        assert_eq!(keys.pad(enter, true).analog[analog::A], 255, "chooses Resume");
        // the menu closed with Enter still down: not the title's Start
        assert_eq!(keys.pad(enter | up, false), keyboard_pad(up));
        assert_eq!(keys.pad(enter, false), PadState::default());
        // let go and pressed again: Start
        assert_eq!(keys.pad(0, false), PadState::default());
        assert_eq!(keys.pad(enter, false).buttons, button::START);
    }

    use super::*;

    fn keys(held: &[KeyCode]) -> PadState {
        keyboard_pad(held.iter().map(|&k| key_bit(k).expect("a bound key")).fold(0, |m, b| m | b))
    }

    /// A gamepad with these buttons down (unpressured) and these axes.
    fn gamepad(down: &[Button], axes: &[(Axis, f32)]) -> PadState {
        gamepad_pad(
            |b| down.contains(&b),
            |_| 0.0,
            |a| axes.iter().find(|(x, _)| *x == a).map_or(0.0, |(_, v)| *v),
        )
    }

    #[test]
    fn every_bound_key_has_its_own_bit_and_changes_the_pad() {
        let mut seen = 0u32;
        for (key, _) in KEYS {
            let bit = key_bit(key).expect("a bound key");
            assert_eq!(seen & bit, 0, "{key:?} shares a bit");
            seen |= bit;
            assert_ne!(keyboard_pad(bit), PadState::default(), "{key:?} does nothing");
        }
        assert_eq!(key_bit(KeyCode::F5), None);
        assert_eq!(keyboard_pad(0), PadState::default());
    }

    #[test]
    fn the_documented_keys_reach_the_documented_controls() {
        let pad = keys(&[KeyCode::ArrowUp, KeyCode::Enter, KeyCode::KeyF, KeyCode::KeyH]);
        assert_eq!(
            pad.buttons,
            button::DPAD_UP | button::START | button::LEFT_THUMB | button::RIGHT_THUMB
        );
        let pad = keys(&[KeyCode::KeyZ, KeyCode::KeyV, KeyCode::KeyQ, KeyCode::KeyE]);
        for index in [analog::A, analog::Y, analog::LEFT_TRIGGER, analog::RIGHT_TRIGGER] {
            assert_eq!(pad.analog[index], 255);
        }
        assert_eq!(pad.analog[analog::B], 0);
        assert_eq!(keys(&[KeyCode::Backspace]).buttons, button::BACK);
    }

    #[test]
    fn stick_keys_push_fully_with_up_positive_and_opposites_cancel() {
        let pad = keys(&[KeyCode::KeyW, KeyCode::KeyD, KeyCode::KeyK, KeyCode::KeyJ]);
        assert_eq!((pad.left_x, pad.left_y), (i16::MAX, i16::MAX));
        assert_eq!((pad.right_x, pad.right_y), (i16::MIN, i16::MIN));
        let pad = keys(&[KeyCode::KeyW, KeyCode::KeyS, KeyCode::KeyA]);
        assert_eq!((pad.left_x, pad.left_y), (i16::MIN, 0));
    }

    #[test]
    fn face_and_shoulder_buttons_map_to_the_analog_buttons() {
        for (host, index) in [
            (Button::South, analog::A),
            (Button::East, analog::B),
            (Button::West, analog::X),
            (Button::North, analog::Y),
            (Button::LeftTrigger, analog::WHITE),
            (Button::RightTrigger, analog::BLACK),
        ] {
            let pad = gamepad(&[host], &[]);
            let mut expected = PadState::default();
            expected.analog[index] = 255;
            assert_eq!(pad, expected, "{host:?}");
            assert!(pad.analog_down(index));
        }
    }

    #[test]
    fn the_guide_button_is_the_menu_s_and_never_reaches_the_title() {
        assert!(is_menu_button(Button::Mode));
        // Held by itself it leaves the console's pad untouched.
        assert_eq!(gamepad(&[Button::Mode], &[]), PadState::default());
        // No button the title sees toggles the menu.
        for (host, _) in DIGITAL {
            assert!(!is_menu_button(host), "{host:?}");
        }
        for (host, _) in ANALOG {
            assert!(!is_menu_button(host), "{host:?}");
        }
        assert!(!is_menu_button(Button::Unknown));
    }

    #[test]
    fn f1_and_escape_are_the_menu_s_keys_and_no_pad_key_is() {
        assert!(is_menu_key(KeyCode::F1));
        assert!(is_menu_key(KeyCode::Escape));
        assert_eq!(key_bit(KeyCode::F1), None);
        assert_eq!(key_bit(KeyCode::Escape), None);
        for (key, _) in KEYS {
            assert!(!is_menu_key(key), "{key:?}");
        }
        assert!(!is_menu_key(KeyCode::F11) && !is_menu_key(KeyCode::F12));
    }

    #[test]
    fn digital_buttons_map_to_their_bits() {
        for (host, bits) in [
            (Button::Start, button::START),
            (Button::Select, button::BACK),
            (Button::LeftThumb, button::LEFT_THUMB),
            (Button::RightThumb, button::RIGHT_THUMB),
            (Button::DPadUp, button::DPAD_UP),
            (Button::DPadDown, button::DPAD_DOWN),
            (Button::DPadLeft, button::DPAD_LEFT),
            (Button::DPadRight, button::DPAD_RIGHT),
        ] {
            assert_eq!(
                gamepad(&[host], &[]),
                PadState { buttons: bits, ..PadState::default() },
                "{host:?}"
            );
        }
    }

    #[test]
    fn triggers_follow_their_pressure_and_a_digital_trigger_is_fully_pressed() {
        let half = gamepad_pad(
            |b| b == Button::LeftTrigger2,
            |b| if b == Button::LeftTrigger2 { 0.5 } else { 0.0 },
            |_| 0.0,
        );
        assert_eq!(half.analog[analog::LEFT_TRIGGER], 128);
        assert_eq!(half.analog[analog::RIGHT_TRIGGER], 0);
        // Below the host's "pressed" threshold the pressure still counts.
        let light = gamepad_pad(
            |_| false,
            |b| if b == Button::RightTrigger2 { 0.25 } else { 0.0 },
            |_| 0.0,
        );
        assert_eq!(light.analog[analog::RIGHT_TRIGGER], 64);
        let digital = gamepad(&[Button::RightTrigger2], &[]);
        assert_eq!(digital.analog[analog::RIGHT_TRIGGER], 255);
    }

    #[test]
    fn sticks_reach_both_ends_and_keep_their_sign() {
        let pad = gamepad(
            &[],
            &[
                (Axis::LeftStickX, -1.0),
                (Axis::LeftStickY, 1.0),
                (Axis::RightStickX, 0.5),
                (Axis::RightStickY, -7.0),
            ],
        );
        assert_eq!((pad.left_x, pad.left_y), (i16::MIN, i16::MAX));
        assert_eq!((pad.right_x, pad.right_y), (16384, i16::MIN));
        assert_eq!(gamepad(&[], &[]), PadState::default());
    }

    #[test]
    fn merging_with_an_idle_source_changes_nothing() {
        let pad = PadState {
            buttons: button::START | button::DPAD_LEFT,
            analog: [1, 2, 3, 4, 5, 6, 7, 255],
            left_x: -300,
            left_y: i16::MAX,
            right_x: i16::MIN,
            right_y: 9,
        };
        assert_eq!(merge(&pad, &PadState::default()), pad);
        assert_eq!(merge(&PadState::default(), &pad), pad);
    }

    #[test]
    fn merging_is_symmetric_and_combines_both_sources() {
        let keyboard = keys(&[KeyCode::KeyZ, KeyCode::ArrowUp, KeyCode::KeyD]);
        let mut gamepad = gamepad(&[Button::Start], &[(Axis::LeftStickX, 0.5)]);
        gamepad.analog[analog::A] = 90;
        gamepad.analog[analog::LEFT_TRIGGER] = 40;
        let merged = merge(&keyboard, &gamepad);
        assert_eq!(merged, merge(&gamepad, &keyboard));
        assert_eq!(merged.buttons, button::DPAD_UP | button::START);
        assert_eq!(merged.analog[analog::A], 255);
        assert_eq!(merged.analog[analog::LEFT_TRIGGER], 40);
        assert_eq!(merged.left_x, i16::MAX, "the sum saturates");
    }
}
