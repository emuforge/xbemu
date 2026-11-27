//! A scripted session for automated runs: timed button presses, menu
//! toggles, screenshots and a quit, read from the `XBEMU_SCRIPT` environment
//! variable.
//!
//! The script is a list of `milliseconds:action` steps separated by commas,
//! the time counted from the first [`crate::Core::frame`] call:
//!
//! ```text
//! XBEMU_SCRIPT='8000:shot=/tmp/a.jpg,9000:combo,9800:down,10000:a,12000:b,15000:quit'
//! ```
//!
//! | Action | Effect |
//! |---|---|
//! | `a` `b` `x` `y` `black` `white` `lt` `rt` `start` `back` `up` `down` `left` `right` `lthumb` `rthumb` | the button held on port 1 for 120 ms |
//! | `menu` | toggles the menu as a front end's key or button does |
//! | `combo` | the menu combo (`Settings::menu_combo`) held on port 1 for 700 ms; nothing when no combo is set, which is the default |
//! | `shot=PATH` | a screenshot of the next presented frame (`.jpg` or `.bmp`) |
//! | `quit` | ends the session as the menu's Quit does |
//!
//! Presses go through the same path as a real pad: the menu sees them, and
//! the title does unless the menu hides them.

use std::path::PathBuf;
use std::time::Duration;

use xb_types::pad::{PadState, analog, button};

/// How long a scripted button stays down.
const PRESS: Duration = Duration::from_millis(120);
/// How long the scripted combo stays down (the menu wants 500 ms).
const COMBO_PRESS: Duration = Duration::from_millis(700);

/// What a step does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    /// Digital buttons held for a while.
    Buttons(u16),
    /// An analog button pressed fully for a while.
    Analog(usize),
    /// The menu combo held long enough to open the menu (nothing is held
    /// when the settings name no combo).
    Combo,
    /// The menu toggled without the pad.
    Menu,
    /// A screenshot written to the path.
    Shot(PathBuf),
    /// A controller plugged into a port (0 to 3), or pulled.
    Plug(usize, bool),
    /// A savestate written to the path.
    Save(PathBuf),
    /// The savestate at the path loaded.
    Load(PathBuf),
    /// The session ends.
    Quit,
}

/// Something the core must do itself for a step that came due.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Event {
    /// Toggle the menu.
    Menu,
    /// Take a screenshot.
    Shot(PathBuf),
    /// Plug a controller into the port (0 to 3), or pull it.
    Plug(usize, bool),
    /// Save a state to the file.
    Save(PathBuf),
    /// Load the state in the file.
    Load(PathBuf),
    /// End the session.
    Quit,
}

/// A parsed script and its progress.
#[derive(Default)]
pub(crate) struct Script {
    steps: Vec<(Duration, Action)>,
    next: usize,
    /// Presses in effect: what is held and until when.
    held: Vec<(Duration, Action)>,
}

fn parse_action(text: &str) -> Option<Action> {
    if let Some(path) = text.strip_prefix("shot=") {
        return (!path.is_empty()).then(|| Action::Shot(PathBuf::from(path)));
    }
    for (prefix, plugged) in [("plug=", true), ("unplug=", false)] {
        if let Some(port) = text.strip_prefix(prefix) {
            // Ports as the console numbers them, 1 to 4.
            return port
                .parse()
                .ok()
                .filter(|p| (1..=4).contains(p))
                .map(|p: usize| Action::Plug(p - 1, plugged));
        }
    }
    if let Some(path) = text.strip_prefix("save=") {
        return (!path.is_empty()).then(|| Action::Save(PathBuf::from(path)));
    }
    if let Some(path) = text.strip_prefix("load=") {
        return (!path.is_empty()).then(|| Action::Load(PathBuf::from(path)));
    }
    Some(match text {
        "a" => Action::Analog(analog::A),
        "b" => Action::Analog(analog::B),
        "x" => Action::Analog(analog::X),
        "y" => Action::Analog(analog::Y),
        "black" => Action::Analog(analog::BLACK),
        "white" => Action::Analog(analog::WHITE),
        "lt" => Action::Analog(analog::LEFT_TRIGGER),
        "rt" => Action::Analog(analog::RIGHT_TRIGGER),
        "start" => Action::Buttons(button::START),
        "back" => Action::Buttons(button::BACK),
        "up" => Action::Buttons(button::DPAD_UP),
        "down" => Action::Buttons(button::DPAD_DOWN),
        "left" => Action::Buttons(button::DPAD_LEFT),
        "right" => Action::Buttons(button::DPAD_RIGHT),
        "lthumb" => Action::Buttons(button::LEFT_THUMB),
        "rthumb" => Action::Buttons(button::RIGHT_THUMB),
        "combo" => Action::Combo,
        "menu" => Action::Menu,
        "quit" => Action::Quit,
        _ => return None,
    })
}

impl Script {
    /// Parses a script. A step that cannot be read is reported in the
    /// second value and left out.
    pub(crate) fn parse(text: &str) -> (Self, Vec<String>) {
        let mut steps = Vec::new();
        let mut bad = Vec::new();
        for step in text.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let parsed = step.split_once(':').and_then(|(at, action)| {
                Some((Duration::from_millis(at.trim().parse().ok()?), parse_action(action.trim())?))
            });
            match parsed {
                Some(s) => steps.push(s),
                None => bad.push(step.to_owned()),
            }
        }
        steps.sort_by_key(|(at, _)| *at);
        (Self { steps, next: 0, held: Vec::new() }, bad)
    }

    /// Whether the script has no steps.
    pub(crate) fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// Advances to `elapsed`: adds the presses in effect to `pad` (the combo
    /// as `combo`) and returns what the core must do itself.
    pub(crate) fn advance(
        &mut self,
        elapsed: Duration,
        combo: u16,
        pad: &mut PadState,
    ) -> Vec<Event> {
        let mut events = Vec::new();
        while let Some((at, action)) = self.steps.get(self.next) {
            if *at > elapsed {
                break;
            }
            match action {
                Action::Menu => events.push(Event::Menu),
                Action::Shot(path) => events.push(Event::Shot(path.clone())),
                Action::Plug(port, plugged) => events.push(Event::Plug(*port, *plugged)),
                Action::Save(path) => events.push(Event::Save(path.clone())),
                Action::Load(path) => events.push(Event::Load(path.clone())),
                Action::Quit => events.push(Event::Quit),
                Action::Combo => self.held.push((elapsed + COMBO_PRESS, action.clone())),
                Action::Buttons(_) | Action::Analog(_) => {
                    self.held.push((elapsed + PRESS, action.clone()));
                }
            }
            self.next += 1;
        }
        self.held.retain(|(until, _)| *until > elapsed);
        for (_, action) in &self.held {
            match action {
                Action::Buttons(mask) => pad.buttons |= mask,
                Action::Analog(index) => pad.analog[*index] = 0xFF,
                Action::Combo => pad.buttons |= combo,
                Action::Menu
                | Action::Shot(_)
                | Action::Plug(..)
                | Action::Save(_)
                | Action::Load(_)
                | Action::Quit => {}
            }
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_press_is_held_for_a_while_and_then_released() {
        let (mut s, bad) = Script::parse("100:a, 100:down ,400:combo");
        assert!(bad.is_empty());
        let mut pad = PadState::default();
        assert!(s.advance(Duration::from_millis(50), 0xC0, &mut pad).is_empty());
        assert_eq!(pad, PadState::default());
        s.advance(Duration::from_millis(110), 0xC0, &mut pad);
        assert_eq!(pad.analog[analog::A], 0xFF);
        assert_eq!(pad.buttons, button::DPAD_DOWN);
        let mut pad = PadState::default();
        s.advance(Duration::from_millis(300), 0xC0, &mut pad);
        assert_eq!(pad, PadState::default());
        s.advance(Duration::from_millis(1000), 0xC0, &mut pad);
        assert_eq!(pad.buttons, 0xC0);
        let mut pad = PadState::default();
        s.advance(Duration::from_millis(1800), 0xC0, &mut pad);
        assert_eq!(pad, PadState::default());
    }

    #[test]
    fn the_scripted_combo_holds_nothing_when_no_combo_is_set() {
        let (mut s, _) = Script::parse("100:combo");
        let mut pad = PadState::default();
        assert!(s.advance(Duration::from_millis(200), 0, &mut pad).is_empty());
        assert_eq!(pad, PadState::default());
    }

    #[test]
    fn core_events_come_once_in_time_order_and_bad_steps_are_reported() {
        let (mut s, bad) =
            Script::parse("300:quit,100:shot=/tmp/x.jpg,200:menu,7:jump,zz:a,5:shot=");
        assert_eq!(bad, ["7:jump", "zz:a", "5:shot="]);
        let mut pad = PadState::default();
        assert_eq!(
            s.advance(Duration::from_millis(250), 0, &mut pad),
            [Event::Shot(PathBuf::from("/tmp/x.jpg")), Event::Menu]
        );
        assert_eq!(s.advance(Duration::from_millis(250), 0, &mut pad), []);
        assert_eq!(s.advance(Duration::from_millis(350), 0, &mut pad), [Event::Quit]);
    }
}
