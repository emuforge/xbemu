//! The menu's state machine: open/closed, the selected row, the confirm
//! step, and what each frame of pad input does to them.

use core::time::Duration;

use xb_types::pad::PadState;
use xb_types::settings::Settings;

use crate::input::{self, COMBO_HOLD, ComboDetector, Held, Repeat, nav};
use crate::items::{Action, Capabilities, ITEMS, Row};
use crate::text::TextBuf;

/// What one [`Menu::update`] did. At most one thing happens per update.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    /// Nothing the caller must act on.
    None,
    /// The menu opened: pause the title and start compositing the overlay.
    Opened,
    /// The menu closed: resume the title.
    Closed,
    /// The user changed a setting. Carries the complete new settings, to
    /// apply and persist. The menu stays open.
    SettingsChanged(Settings),
    /// The user chose to save a state into this slot. The menu has closed
    /// (the slot's picture is of the game, not of the menu).
    SaveSlot(usize),
    /// The user chose to load this slot's state. The menu has closed.
    LoadSlot(usize),
    /// The user confirmed "Reset game". The menu has closed.
    ResetRequested,
    /// The user confirmed "Quit". The menu has closed.
    QuitRequested,
}

/// What the caller knows that the menu shows but does not edit.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Status<'a> {
    /// The CPU backend running now, for example `"KVM"`.
    pub cpu_backend: &'a str,
    /// The GPU's name.
    pub gpu: &'a str,
    /// The running title's name.
    pub title: &'a str,
    /// What the title is doing that the settings cannot show, in a few
    /// words: the picture shape it draws ("draws 16:9"). Empty: not known.
    pub game: &'a str,
    /// Frames per second. Shown rounded to a whole number.
    pub fps: f32,
}

/// Savestate slots the menu's page lists.
pub const SLOT_ROWS: usize = 10;

/// One slot as the caller tells it ([`Menu::set_slots`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SlotRow<'a> {
    /// A state is saved in it.
    pub used: bool,
    /// What to say of it: when it was saved ("5 min ago").
    pub label: &'a str,
}

/// A picture for the slots page: the highlighted slot's.
#[derive(Clone, Copy, Debug)]
pub struct Picture<'a> {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// `width * height * 4` bytes, R, G, B, A (alpha ignored), top row first.
    pub rgba: &'a [u8],
}

/// Longest status string kept; the layout shows fewer.
pub(crate) const STATUS_LEN: usize = 40;

/// The menu's own copy of a [`Status`], so drawing needs no borrowed strings
/// and a change can be detected by comparison.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StatusCopy {
    pub(crate) cpu_backend: TextBuf<STATUS_LEN>,
    pub(crate) gpu: TextBuf<STATUS_LEN>,
    pub(crate) title: TextBuf<STATUS_LEN>,
    pub(crate) game: TextBuf<STATUS_LEN>,
    pub(crate) fps: u32,
}

/// The confirm box of a destructive action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Confirm {
    /// What is being confirmed.
    pub(crate) action: Action,
    /// Whether "Yes" is highlighted. Starts on "No".
    pub(crate) yes: bool,
}

/// The in-game menu.
///
/// Call [`Menu::update`] once per frame with the pads, give the title the
/// pads through [`Menu::filter_pad`], and while [`Menu::is_open`] draw with
/// [`Menu::render`] whenever [`Menu::is_dirty`].
#[derive(Clone, Debug)]
pub struct Menu {
    pub(crate) settings: Settings,
    pub(crate) caps: Capabilities,
    pub(crate) status: StatusCopy,
    pub(crate) open: bool,
    /// Index into [`ITEMS`] of the highlighted row.
    pub(crate) selected: usize,
    pub(crate) confirm: Option<Confirm>,
    /// The savestate slots page is up, on this slot.
    pub(crate) slots_page: Option<usize>,
    /// The slots as last told: in use, and their label.
    pub(crate) slot_rows: [(bool, TextBuf<24>); SLOT_ROWS],
    pub(crate) dirty: bool,
    combo: ComboDetector,
    repeat: Repeat,
    /// Navigation inputs down at the previous update, for edge detection.
    /// Tracked while closed too, so a button held as the menu opens is not a
    /// press.
    prev_nav: u8,
    /// Buttons withheld from the title (and from navigation) until released.
    hidden: Held,
    /// Buttons down at the last update, for [`Menu::close`].
    last_held: Held,
}

impl Menu {
    /// A closed menu editing `settings`.
    #[must_use]
    pub fn new(settings: Settings, capabilities: Capabilities) -> Self {
        Self {
            settings,
            caps: capabilities,
            status: StatusCopy {
                cpu_backend: TextBuf::new(),
                gpu: TextBuf::new(),
                title: TextBuf::new(),
                game: TextBuf::new(),
                fps: 0,
            },
            open: false,
            selected: 0,
            confirm: None,
            slots_page: None,
            slot_rows: [(false, TextBuf::new()); SLOT_ROWS],
            dirty: true,
            combo: ComboDetector::default(),
            repeat: Repeat::default(),
            prev_nav: 0,
            hidden: Held::default(),
            last_held: Held::default(),
        }
    }

    /// Whether the menu is showing. While it is, the title should be paused
    /// and the overlay composited.
    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.open
    }

    /// The settings as the menu has them.
    #[must_use]
    pub const fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Replaces the settings, for when something other than the menu changed
    /// them (a front end's own options). Reports nothing back.
    pub fn set_settings(&mut self, settings: Settings) {
        if self.settings != settings {
            self.settings = settings;
            self.dirty = true;
        }
    }

    /// Replaces the capabilities, for when the core learns late what applies
    /// live.
    pub fn set_capabilities(&mut self, capabilities: Capabilities) {
        if self.caps != capabilities {
            self.caps = capabilities;
            self.dirty = true;
        }
    }

    /// Updates the status lines. Cheap enough to call every frame: the menu
    /// is marked dirty only when the text shown would change. Strings are
    /// cut to 40 characters and non-ASCII characters shown as `?`.
    pub fn set_status(&mut self, status: &Status<'_>) {
        let fps = if status.fps.is_finite() { status.fps.round().clamp(0.0, 9999.0) } else { 0.0 };
        let copy = StatusCopy {
            cpu_backend: TextBuf::from_text(status.cpu_backend),
            gpu: TextBuf::from_text(status.gpu),
            title: TextBuf::from_text(status.title),
            game: TextBuf::from_text(status.game),
            fps: fps as u32,
        };
        if self.status != copy {
            self.status = copy;
            self.dirty = true;
        }
    }

    /// The digital buttons ([`xb_types::pad::button`] bits) to hide from the
    /// title: all of them while the menu is open; otherwise the ones that
    /// were down when it opened or closed and have not been released since
    /// (the combo's buttons among them).
    ///
    /// [`Menu::filter_pad`] applies this and also covers the analog buttons
    /// and the sticks; prefer it.
    #[must_use]
    pub const fn hidden_buttons(&self) -> u16 {
        if self.open { u16::MAX } else { self.hidden.buttons }
    }

    /// The analog buttons to hide from the title, bit `i` for index `i` of
    /// [`PadState::analog`], on the same terms as [`Menu::hidden_buttons`].
    /// This is what keeps the A that chose "Resume" from reaching the title.
    #[must_use]
    pub const fn hidden_analog(&self) -> u8 {
        if self.open { u8::MAX } else { self.hidden.analog }
    }

    /// The pad state the title should see for `pad`: released entirely while
    /// the menu is open, otherwise `pad` without the hidden buttons.
    ///
    /// Call it after this frame's [`Menu::update`], for every port.
    #[must_use]
    pub fn filter_pad(&self, pad: PadState) -> PadState {
        if self.open {
            return PadState::default();
        }
        let mut pad = pad;
        pad.buttons &= !self.hidden.buttons;
        for (index, pressure) in pad.analog.iter_mut().enumerate() {
            if self.hidden.analog & 1 << index != 0 {
                *pressure = 0;
            }
        }
        pad
    }

    /// Whether the image [`Menu::render`] would draw differs from the one it
    /// last drew. True after opening and after any visible change.
    #[must_use]
    pub const fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Forces the next [`Menu::is_dirty`] to be true, for when the caller
    /// lost or replaced its overlay buffer.
    pub fn invalidate(&mut self) {
        self.dirty = true;
    }

    /// Opens the menu without the combo, for a front end's own hotkey.
    pub fn open(&mut self) {
        if !self.open {
            self.open = true;
            self.selected = 0;
            self.confirm = None;
            self.slots_page = None;
            self.repeat.reset();
            self.dirty = true;
        }
    }

    /// Closes the menu without the combo. Buttons down at the last update
    /// stay hidden from the title until released.
    pub fn close(&mut self) {
        if self.open {
            self.open = false;
            self.confirm = None;
            self.slots_page = None;
            self.repeat.reset();
            self.hidden.buttons |= self.last_held.buttons;
            self.hidden.analog |= self.last_held.analog;
        }
    }

    /// Advances the menu by one frame.
    ///
    /// `dt` is the time since the previous update; it drives the combo's
    /// hold time and auto-repeat. `pads` are the connected controllers as
    /// the front end reports them, unfiltered; the first four are read and
    /// any of them can open and drive the menu.
    pub fn update(&mut self, dt: Duration, pads: &[PadState]) -> Outcome {
        let pads = input::ports(pads);
        let held = Held::of(pads);
        self.last_held = held;
        self.hidden.buttons &= held.buttons;
        self.hidden.analog &= held.analog;

        // Zero is "no combo": the detector never completes on it, and the
        // front end opens the menu with `open`.
        let combo = self.settings.menu_combo;
        let hold = if self.open { Duration::ZERO } else { COMBO_HOLD };
        let toggled = self.combo.update(combo, pads, dt, hold);
        if toggled {
            self.hidden.buttons |= combo;
        }

        let down = input::nav_bits(pads, self.hidden, self.prev_nav);
        let pressed = down & !self.prev_nav;
        self.prev_nav = down;

        if toggled {
            return if self.open {
                self.close();
                Outcome::Closed
            } else {
                self.open();
                Outcome::Opened
            };
        }
        if !self.open {
            return Outcome::None;
        }

        let direction = self.repeat.update(down, pressed, dt);
        if pressed & nav::START != 0 {
            self.close();
            Outcome::Closed
        } else if pressed & nav::B != 0 {
            self.back()
        } else if pressed & nav::X != 0 {
            self.save_slot()
        } else if pressed & nav::A != 0 {
            self.activate()
        } else if direction != 0 {
            self.direction(direction)
        } else {
            Outcome::None
        }
    }

    /// What the savestate slots hold, for the slots page. Up to
    /// [`SLOT_ROWS`] are kept.
    pub fn set_slots(&mut self, rows: &[SlotRow<'_>]) {
        for (kept, row) in self.slot_rows.iter_mut().zip(rows) {
            let new = (row.used, TextBuf::from_text(row.label));
            if *kept != new {
                *kept = new;
                self.dirty |= self.slots_page.is_some();
            }
        }
    }

    /// The slot highlighted on the slots page, while it is up: the one whose
    /// picture [`Menu::render_with`] should be given.
    #[must_use]
    pub const fn shown_slot(&self) -> Option<usize> {
        if self.open { self.slots_page } else { None }
    }

    /// X on the slots page: save into the highlighted slot.
    fn save_slot(&mut self) -> Outcome {
        let Some(slot) = self.slots_page else { return Outcome::None };
        self.close();
        Outcome::SaveSlot(slot)
    }

    /// B: leave the confirm box or the slots page, or close the menu.
    fn back(&mut self) -> Outcome {
        if self.confirm.take().is_some() || self.slots_page.take().is_some() {
            self.dirty = true;
            return Outcome::None;
        }
        self.close();
        Outcome::Closed
    }

    /// A: act on the selected row or the confirm box.
    fn activate(&mut self) -> Outcome {
        if let Some(confirm) = self.confirm.take() {
            self.dirty = true;
            if !confirm.yes {
                return Outcome::None;
            }
            self.close();
            return match confirm.action {
                Action::Reset => Outcome::ResetRequested,
                Action::Quit => Outcome::QuitRequested,
            };
        }
        if let Some(slot) = self.slots_page {
            // A loads the highlighted slot; an empty one has nothing to load.
            if !self.slot_rows[slot].0 {
                return Outcome::None;
            }
            self.close();
            return Outcome::LoadSlot(slot);
        }
        match ITEMS[self.selected].row {
            Row::Slots => {
                self.slots_page = Some(0);
                self.dirty = true;
                Outcome::None
            }
            Row::Resume => {
                self.close();
                Outcome::Closed
            }
            Row::Setting { .. } => self.change_value(1, true),
            Row::Confirm(action) => {
                self.confirm = Some(Confirm { action, yes: false });
                self.dirty = true;
                Outcome::None
            }
            Row::Info(_) => Outcome::None,
        }
    }

    /// A direction press or repeat.
    fn direction(&mut self, direction: u8) -> Outcome {
        if let Some(confirm) = &mut self.confirm {
            // "No" is on the left, "Yes" on the right: the answers sit side
            // by side, so up and down choose nothing.
            let yes = match direction {
                nav::LEFT => false,
                nav::RIGHT => true,
                _ => confirm.yes,
            };
            self.dirty |= confirm.yes != yes;
            confirm.yes = yes;
            return Outcome::None;
        }
        if let Some(slot) = &mut self.slots_page {
            let at = match direction {
                nav::UP => (*slot + SLOT_ROWS - 1) % SLOT_ROWS,
                nav::DOWN => (*slot + 1) % SLOT_ROWS,
                _ => *slot,
            };
            self.dirty |= at != *slot;
            *slot = at;
            return Outcome::None;
        }
        match direction {
            nav::UP => self.move_selection(false),
            nav::DOWN => self.move_selection(true),
            nav::LEFT => return self.change_value(-1, false),
            nav::RIGHT => return self.change_value(1, false),
            _ => {}
        }
        Outcome::None
    }

    /// Moves the highlight to the next selectable row, wrapping at the ends.
    fn move_selection(&mut self, down: bool) {
        let count = ITEMS.len();
        let mut at = self.selected;
        for _ in 0..count {
            at = if down { (at + 1) % count } else { (at + count - 1) % count };
            if ITEMS[at].row.selectable() {
                break;
            }
        }
        self.dirty |= at != self.selected;
        self.selected = at;
    }

    /// Steps the selected setting and reports the change, if there was one.
    fn change_value(&mut self, dir: i32, wrap: bool) -> Outcome {
        let Row::Setting { values, get, set, .. } = ITEMS[self.selected].row else {
            return Outcome::None;
        };
        let before = self.settings.clone();
        let next = values.step(get(&self.settings), dir, wrap);
        set(&mut self.settings, next);
        if self.settings == before {
            return Outcome::None;
        }
        self.dirty = true;
        Outcome::SettingsChanged(self.settings.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{REPEAT_DELAY, REPEAT_INTERVAL};
    use crate::items::Setting;
    use xb_types::pad::{analog, button};
    use xb_types::settings::{Aspect, CpuBackend};

    const FRAME: Duration = Duration::from_millis(10);
    const COMBO: u16 = button::LEFT_THUMB | button::RIGHT_THUMB;

    fn pad(buttons: u16) -> PadState {
        PadState { buttons, ..PadState::default() }
    }

    fn pad_analog(index: usize) -> PadState {
        let mut p = PadState::default();
        p.analog[index] = 255;
        p
    }

    fn a() -> PadState {
        pad_analog(analog::A)
    }

    fn b() -> PadState {
        pad_analog(analog::B)
    }

    /// A menu driven one frame at a time.
    struct Rig {
        menu: Menu,
    }

    impl Rig {
        fn new() -> Self {
            Self::with(Settings::default())
        }

        fn with(settings: Settings) -> Self {
            Self { menu: Menu::new(settings, Capabilities::default()) }
        }

        /// A menu with both stick clicks as its combo, as a front end
        /// without a button of its own for the menu would set it.
        fn with_combo() -> Self {
            Self::with(Settings { menu_combo: COMBO, ..Settings::default() })
        }

        fn opened() -> Self {
            let mut rig = Self::new();
            rig.menu.update(FRAME, &[pad(0)]);
            rig.menu.open();
            rig
        }

        fn frame(&mut self, p: PadState) -> Outcome {
            self.menu.update(FRAME, &[p])
        }

        /// Holds `p` for `time`, returning every outcome that was not `None`.
        fn hold(&mut self, p: PadState, time: Duration) -> Vec<Outcome> {
            let frames = time.as_millis() / FRAME.as_millis();
            (0..frames).map(|_| self.frame(p)).filter(|o| *o != Outcome::None).collect()
        }

        /// Presses and releases `p`, returning the press's outcome.
        fn tap(&mut self, p: PadState) -> Outcome {
            let outcome = self.frame(p);
            assert_eq!(self.frame(pad(0)), Outcome::None);
            outcome
        }

        fn select(&mut self, label: &str) {
            let target = ITEMS.iter().position(|i| i.label == label).unwrap();
            for _ in 0..ITEMS.len() {
                if self.menu.selected == target {
                    return;
                }
                self.tap(pad(button::DPAD_DOWN));
            }
            panic!("{label} cannot be selected");
        }

        fn changed(&mut self, p: PadState) -> Settings {
            match self.tap(p) {
                Outcome::SettingsChanged(s) => {
                    assert_eq!(&s, self.menu.settings());
                    s
                }
                other => panic!("expected a settings change, got {other:?}"),
            }
        }
    }

    // Activation.

    #[test]
    fn holding_the_combo_opens_the_menu_once_after_the_hold_time() {
        let mut rig = Rig::with_combo();
        rig.frame(pad(0));
        let short = COMBO_HOLD - FRAME;
        assert_eq!(rig.hold(pad(COMBO), short), []);
        assert!(!rig.menu.is_open());
        assert_eq!(rig.frame(pad(COMBO)), Outcome::Opened);
        assert!(rig.menu.is_open());
        // Still held: neither closes nor reopens.
        assert_eq!(rig.hold(pad(COMBO), Duration::from_secs(5)), []);
        assert!(rig.menu.is_open());
    }

    #[test]
    fn a_brief_press_of_the_combo_does_not_open_the_menu() {
        // Halo: crouch (left click, held) plus a zoom (right click) tap.
        let mut rig = Rig::with_combo();
        rig.frame(pad(0));
        for _ in 0..20 {
            assert_eq!(rig.hold(pad(COMBO), COMBO_HOLD / 2), []);
            assert_eq!(rig.hold(pad(button::LEFT_THUMB), COMBO_HOLD / 2), []);
        }
        assert!(!rig.menu.is_open());
        assert_eq!(rig.menu.hidden_buttons(), 0);
    }

    #[test]
    fn partial_combos_never_open_the_menu() {
        let mut rig = Rig::with_combo();
        rig.frame(pad(0));
        for buttons in [button::LEFT_THUMB, button::RIGHT_THUMB, button::START | button::BACK] {
            assert_eq!(rig.hold(pad(buttons), COMBO_HOLD * 3), []);
            assert_eq!(rig.menu.hidden_buttons(), 0);
        }
        assert!(!rig.menu.is_open());
    }

    #[test]
    fn the_combo_closes_the_menu_only_after_being_released_and_pressed_again() {
        let mut rig = Rig::with_combo();
        rig.frame(pad(0));
        assert_eq!(rig.hold(pad(COMBO), COMBO_HOLD), [Outcome::Opened]);
        // Letting go of one button and pressing it again is not a new press.
        assert_eq!(rig.frame(pad(button::LEFT_THUMB)), Outcome::None);
        assert_eq!(rig.hold(pad(COMBO), COMBO_HOLD * 2), []);
        assert_eq!(rig.frame(pad(0)), Outcome::None);
        assert_eq!(rig.frame(pad(COMBO)), Outcome::Closed);
        assert!(!rig.menu.is_open());
        // And held on, it does not reopen.
        assert_eq!(rig.hold(pad(COMBO), COMBO_HOLD * 4), []);
        assert_eq!(rig.frame(pad(0)), Outcome::None);
        assert_eq!(rig.hold(pad(COMBO), COMBO_HOLD), [Outcome::Opened]);
    }

    #[test]
    fn the_combo_buttons_stay_hidden_until_all_of_them_are_released() {
        let mut rig = Rig::with_combo();
        rig.frame(pad(0));
        rig.hold(pad(COMBO), COMBO_HOLD);
        assert_eq!(rig.menu.hidden_buttons(), u16::MAX);
        rig.frame(pad(0));
        assert_eq!(rig.frame(pad(COMBO)), Outcome::Closed);
        // Closed with both still down: both hidden, other buttons pass.
        assert_eq!(rig.menu.hidden_buttons(), COMBO);
        let seen = rig.menu.filter_pad(pad(COMBO | button::DPAD_UP));
        assert_eq!(seen.buttons, button::DPAD_UP);
        // One released: only the other is still hidden, and pressing the
        // released one again reaches the title.
        rig.frame(pad(button::LEFT_THUMB));
        assert_eq!(rig.menu.hidden_buttons(), button::LEFT_THUMB);
        rig.frame(pad(COMBO));
        assert_eq!(rig.menu.hidden_buttons(), button::LEFT_THUMB);
        assert_eq!(rig.menu.filter_pad(pad(COMBO)).buttons, button::RIGHT_THUMB);
        rig.frame(pad(0));
        assert_eq!(rig.menu.hidden_buttons(), 0);
        assert_eq!(rig.menu.filter_pad(pad(COMBO)).buttons, COMBO);
    }

    #[test]
    fn nothing_is_hidden_while_the_combo_is_being_held_to_open() {
        let mut rig = Rig::with_combo();
        rig.frame(pad(0));
        rig.hold(pad(COMBO), COMBO_HOLD - FRAME);
        assert_eq!(rig.menu.filter_pad(pad(COMBO)), pad(COMBO));
    }

    #[test]
    fn the_title_sees_a_released_pad_while_the_menu_is_open() {
        let mut rig = Rig::opened();
        let mut busy = pad(button::START | button::DPAD_LEFT);
        busy.analog = [200; 8];
        busy.left_x = 30000;
        busy.right_y = -30000;
        rig.frame(pad(0));
        assert_eq!(rig.menu.filter_pad(busy), PadState::default());
        assert_eq!(rig.menu.hidden_analog(), u8::MAX);
    }

    #[test]
    fn the_button_that_closed_the_menu_is_hidden_until_released() {
        let mut rig = Rig::opened();
        assert_eq!(rig.frame(a()), Outcome::Closed);
        assert_eq!(rig.menu.hidden_analog(), 1 << analog::A);
        let mut held = a();
        held.analog[analog::X] = 255;
        let seen = rig.menu.filter_pad(held);
        assert_eq!(seen.analog[analog::A], 0);
        assert_eq!(seen.analog[analog::X], 255);
        rig.frame(pad(0));
        assert_eq!(rig.menu.hidden_analog(), 0);
        assert_eq!(rig.menu.filter_pad(a()), a());

        let mut rig = Rig::opened();
        assert_eq!(rig.frame(pad(button::START)), Outcome::Closed);
        assert_eq!(rig.menu.hidden_buttons(), button::START);
    }

    #[test]
    fn any_port_can_open_and_drive_the_menu() {
        let mut menu = Rig::with_combo().menu;
        let idle = [pad(0); 4];
        menu.update(FRAME, &idle);
        let mut pads = idle;
        pads[2] = pad(COMBO);
        let opened = (0..100).any(|_| menu.update(FRAME, &pads) == Outcome::Opened);
        assert!(opened);
        menu.update(FRAME, &idle);
        pads[2] = pad(0);
        pads[3] = pad(button::DPAD_DOWN);
        menu.update(FRAME, &pads);
        assert_eq!(menu.selected, 1);
    }

    #[test]
    fn a_custom_combo_is_honoured_and_its_buttons_do_not_navigate() {
        let combo = button::START | button::DPAD_DOWN;
        let mut rig = Rig::with(Settings { menu_combo: combo, ..Settings::default() });
        rig.frame(pad(0));
        assert_eq!(rig.hold(pad(COMBO), COMBO_HOLD * 2), []);
        assert_eq!(rig.hold(pad(combo), COMBO_HOLD), [Outcome::Opened]);
        // Start would close and down would move, were they not hidden.
        assert_eq!(rig.hold(pad(combo), REPEAT_DELAY * 3), []);
        assert!(rig.menu.is_open());
        assert_eq!(rig.menu.selected, 0);
    }

    #[test]
    fn without_a_combo_no_buttons_open_the_menu_and_none_are_hidden() {
        assert_eq!(Settings::default().menu_combo, 0);
        let mut rig = Rig::new();
        rig.frame(pad(0));
        let mut everything = pad(u16::MAX);
        everything.analog = [255; 8];
        for held in [pad(0), pad(COMBO), everything] {
            assert_eq!(rig.hold(held, COMBO_HOLD * 4), []);
            assert!(!rig.menu.is_open());
            assert_eq!(rig.menu.filter_pad(held), held);
            rig.frame(pad(0));
        }
        // The front end's own button still opens and closes it.
        rig.menu.open();
        assert!(rig.menu.is_open());
        assert_eq!(rig.hold(pad(COMBO), COMBO_HOLD * 4), []);
        assert!(rig.menu.is_open());
    }

    /// A real pad at rest: triggers a little above zero, sticks off centre.
    fn resting(buttons: u16, wobble: i16) -> PadState {
        let mut p = pad(buttons);
        p.analog[analog::LEFT_TRIGGER] = 9;
        p.analog[analog::RIGHT_TRIGGER] = 14;
        p.left_x = 2600 + wobble;
        p.left_y = -3100 - wobble;
        p.right_x = -1900 + wobble;
        p.right_y = 4200 - wobble;
        p
    }

    #[test]
    fn a_hand_holding_and_letting_go_of_the_combo_leaves_the_menu_open() {
        let mut rig = Rig::with_combo();
        let mut wobble = 0i16;
        let mut run = |rig: &mut Rig, buttons: u16, tilt: i16, time: Duration| {
            let mut outcomes = Vec::new();
            for _ in 0..time.as_millis() / FRAME.as_millis() {
                wobble = (wobble + 137) % 900;
                let mut p = resting(buttons, wobble);
                // Pressing a stick in also leans it.
                p.left_y += tilt;
                p.right_y -= tilt;
                let outcome = rig.frame(p);
                if outcome != Outcome::None {
                    outcomes.push(outcome);
                }
            }
            outcomes
        };
        assert_eq!(run(&mut rig, 0, 0, Duration::from_secs(1)), []);
        // The thumbs do not land in the same frame.
        assert_eq!(run(&mut rig, button::LEFT_THUMB, 6000, FRAME * 4), []);
        assert_eq!(run(&mut rig, COMBO, 9000, COMBO_HOLD), [Outcome::Opened]);
        // Held on for a while after it opened, then let go one at a time,
        // with a bounce of the last contact.
        assert_eq!(run(&mut rig, COMBO, 9000, Duration::from_millis(800)), []);
        assert_eq!(run(&mut rig, button::RIGHT_THUMB, 5000, FRAME * 3), []);
        assert_eq!(run(&mut rig, 0, 0, FRAME), []);
        assert_eq!(run(&mut rig, button::RIGHT_THUMB, 0, FRAME), []);
        assert_eq!(run(&mut rig, 0, 0, Duration::from_secs(3)), []);
        assert!(rig.menu.is_open());
        assert_eq!(rig.menu.selected, 0, "the selection moved by itself");
        assert_eq!(rig.menu.filter_pad(resting(COMBO, 0)), PadState::default());

        // The pad then drives the menu and only its own buttons close it.
        assert_eq!(run(&mut rig, button::DPAD_DOWN, 0, FRAME * 5), []);
        assert_eq!(rig.menu.selected, 1);
        assert_eq!(run(&mut rig, 0, 0, FRAME * 5), []);
        for other in [button::BACK, button::LEFT_THUMB, button::RIGHT_THUMB] {
            assert_eq!(run(&mut rig, other, 0, FRAME * 20), []);
            assert_eq!(run(&mut rig, 0, 0, FRAME * 5), []);
        }
        let mut analogs = resting(0, 0);
        for index in [analog::X, analog::Y, analog::BLACK, analog::WHITE] {
            analogs.analog[index] = 255;
        }
        analogs.analog[analog::LEFT_TRIGGER] = 255;
        analogs.analog[analog::RIGHT_TRIGGER] = 255;
        assert_eq!(rig.hold(analogs, FRAME * 20), []);
        assert!(rig.menu.is_open());
        assert_eq!(rig.menu.selected, 1);
        rig.frame(resting(0, 0));
        let mut b_down = resting(0, 0);
        b_down.analog[analog::B] = 255;
        assert_eq!(rig.frame(b_down), Outcome::Closed);
    }

    #[test]
    fn a_resting_pad_does_not_drive_a_menu_the_front_end_opened() {
        let mut rig = Rig::new();
        rig.frame(resting(0, 0));
        rig.menu.open();
        let outcomes: Vec<_> = (0..500i16)
            .map(|n| rig.frame(resting(0, (n % 7) * 120)))
            .filter(|o| *o != Outcome::None)
            .collect();
        assert_eq!(outcomes, []);
        assert!(rig.menu.is_open());
        assert_eq!(rig.menu.selected, 0);
    }

    #[test]
    fn a_button_held_while_the_menu_opens_is_not_a_press() {
        let mut rig = Rig::new();
        let mut held = a();
        held.buttons = button::START | button::DPAD_DOWN;
        rig.frame(held);
        rig.menu.open();
        assert_eq!(rig.hold(held, REPEAT_DELAY * 3), []);
        assert!(rig.menu.is_open());
        assert_eq!(rig.menu.selected, 0);
    }

    // Navigation.

    #[test]
    fn up_and_down_visit_every_selectable_row_and_wrap() {
        let mut rig = Rig::opened();
        let selectable: Vec<usize> =
            (0..ITEMS.len()).filter(|&i| ITEMS[i].row.selectable()).collect();
        assert!(selectable.len() < ITEMS.len(), "the table has a read-only row");
        let mut visited = vec![rig.menu.selected];
        for _ in 0..selectable.len() {
            rig.tap(pad(button::DPAD_DOWN));
            visited.push(rig.menu.selected);
        }
        assert_eq!(visited[..selectable.len()], selectable);
        assert_eq!(visited[selectable.len()], selectable[0]);
        rig.tap(pad(button::DPAD_UP));
        assert_eq!(rig.menu.selected, *selectable.last().unwrap());
        for want in selectable.iter().rev().skip(1) {
            rig.tap(pad(button::DPAD_UP));
            assert_eq!(rig.menu.selected, *want);
        }
    }

    #[test]
    fn a_held_direction_moves_once_then_repeats_after_the_delay() {
        let mut rig = Rig::opened();
        let down = pad(button::DPAD_DOWN);
        rig.frame(down);
        assert_eq!(rig.menu.selected, 1);
        rig.hold(down, REPEAT_DELAY - FRAME);
        assert_eq!(rig.menu.selected, 1);
        rig.frame(down);
        assert_eq!(rig.menu.selected, 2);
        rig.hold(down, REPEAT_INTERVAL - FRAME);
        assert_eq!(rig.menu.selected, 2);
        rig.frame(down);
        assert_eq!(rig.menu.selected, 3);
        rig.hold(down, REPEAT_INTERVAL * 2);
        assert_eq!(rig.menu.selected, 5);
        rig.hold(pad(0), REPEAT_DELAY * 2);
        assert_eq!(rig.menu.selected, 5);
    }

    #[test]
    fn a_held_left_or_right_repeats_the_value_change() {
        let mut rig = Rig::opened();
        rig.select("Volume");
        let changes = rig.hold(pad(button::DPAD_LEFT), REPEAT_DELAY + REPEAT_INTERVAL * 3 + FRAME);
        assert_eq!(changes.len(), 5);
        assert_eq!(rig.menu.settings().volume, 75);
    }

    #[test]
    fn the_left_stick_moves_the_selection() {
        let mut rig = Rig::opened();
        let stick = |left_y| PadState { left_y, ..PadState::default() };
        rig.tap(stick(-32768));
        assert_eq!(rig.menu.selected, 1);
        rig.tap(stick(32767));
        assert_eq!(rig.menu.selected, 0);
        // A small deflection is not a move.
        rig.tap(stick(-8000));
        assert_eq!(rig.menu.selected, 0);
    }

    #[test]
    fn b_start_and_resume_each_close_the_menu() {
        for close in [b(), pad(button::START), a()] {
            let mut rig = Rig::opened();
            assert_eq!(rig.tap(close), Outcome::Closed);
            assert!(!rig.menu.is_open());
        }
    }

    #[test]
    fn reopening_starts_on_the_first_row() {
        let mut rig = Rig::opened();
        rig.select("Volume");
        rig.tap(b());
        rig.menu.open();
        assert_eq!(rig.menu.selected, 0);
    }

    #[test]
    fn input_is_ignored_while_the_menu_is_closed() {
        let mut rig = Rig::new();
        for p in [a(), b(), pad(button::DPAD_DOWN), pad(button::DPAD_RIGHT), pad(button::START)] {
            assert_eq!(rig.tap(p), Outcome::None);
        }
        assert_eq!(rig.menu.settings(), &Settings::default());
        assert_eq!(rig.menu.selected, 0);
    }

    // Items.

    /// Drives one setting row through its values with right, left and A,
    /// checking that `field` takes each of `values` in order, that left and
    /// right stop at the ends, that A wraps, and that no other field moves.
    fn check_row<T: PartialEq + core::fmt::Debug + Copy>(
        label: &str,
        field: fn(&mut Settings) -> &mut T,
        values: &[T],
    ) {
        let mut start = Settings::default();
        *field(&mut start) = values[0];
        let mut rig = Rig::with(start.clone());
        rig.menu.update(FRAME, &[pad(0)]);
        rig.menu.open();
        rig.select(label);
        let expect = |s: &Settings, v: T| {
            let mut want = start.clone();
            *field(&mut want) = v;
            assert_eq!(s, &want, "{label}");
        };

        assert_eq!(rig.tap(pad(button::DPAD_LEFT)), Outcome::None, "{label}: left at the low end");
        for &v in &values[1..] {
            expect(&rig.changed(pad(button::DPAD_RIGHT)), v);
        }
        assert_eq!(rig.tap(pad(button::DPAD_RIGHT)), Outcome::None, "{label}: right at the top");
        for &v in values.iter().rev().skip(1) {
            expect(&rig.changed(pad(button::DPAD_LEFT)), v);
        }
        for &v in values[1..].iter().chain(&values[..1]) {
            expect(&rig.changed(a()), v);
        }
        assert!(rig.menu.is_open());
    }

    /// The same for an on/off row, where left, right and A all flip.
    fn check_toggle(label: &str, field: fn(&mut Settings) -> &mut bool) {
        let mut rig = Rig::opened();
        rig.select(label);
        let mut want = Settings::default();
        for press in [pad(button::DPAD_RIGHT), pad(button::DPAD_LEFT), a(), a()] {
            *field(&mut want) ^= true;
            assert_eq!(rig.changed(press), want, "{label}");
        }
    }

    #[test]
    fn render_scale_steps_from_1_to_4() {
        check_row("Render scale", |s| &mut s.render_scale, &[1, 2, 3, 4]);
    }

    #[test]
    fn aspect_steps_through_its_three_shapes() {
        let shapes = [Aspect::Standard, Aspect::Wide, Aspect::Stretch];
        check_row("Aspect", |s| &mut s.aspect, &shapes);
    }

    #[test]
    fn anisotropy_steps_through_the_levels_the_settings_accept() {
        check_row("Anisotropic filter", |s| &mut s.anisotropy, &[0, 2, 4, 8, 16]);
    }

    #[test]
    fn volume_steps_from_0_to_100() {
        let levels: Vec<u8> = (0..=100).step_by(5).collect();
        check_row("Volume", |s| &mut s.volume, &levels);
    }

    #[test]
    fn cpu_backend_steps_through_its_three_choices() {
        let backends = [CpuBackend::Auto, CpuBackend::Kvm, CpuBackend::Translator];
        check_row("CPU backend", |s| &mut s.cpu, &backends);
    }

    #[test]
    fn disc_speed_steps_between_console_and_fast() {
        check_row("Disc speed", |s| &mut s.fast_disc, &[false, true]);
    }

    #[test]
    fn each_switch_flips_its_own_field() {
        check_toggle("Widescreen", |s| &mut s.widescreen);
        check_toggle("FXAA", |s| &mut s.fxaa);
        check_toggle("VSync", |s| &mut s.vsync);
        check_toggle("Show FPS", |s| &mut s.show_fps);
        check_toggle("Fullscreen", |s| &mut s.fullscreen);
    }

    #[test]
    fn every_setting_row_has_an_item_test() {
        // Keeps the per-row tests above in step with the table.
        let tested = [
            "Render scale",
            "Fullscreen",
            "Aspect",
            "Widescreen",
            "FXAA",
            "Anisotropic filter",
            "VSync",
            "Show FPS",
            "Disc speed",
            "Volume",
            "CPU backend",
        ];
        for item in ITEMS {
            if matches!(item.row, Row::Setting { .. }) {
                assert!(tested.contains(&item.label), "{} has no test", item.label);
            }
        }
    }

    #[test]
    fn values_the_menu_produces_survive_the_settings_text_form() {
        // Every value reachable from the menu is one the settings file
        // accepts, or it would be lost at the next start.
        let mut rig = Rig::opened();
        for item in ITEMS {
            if !matches!(item.row, Row::Setting { .. }) {
                continue;
            }
            rig.select(item.label);
            for _ in 0..25 {
                if let Outcome::SettingsChanged(s) = rig.tap(a()) {
                    assert_eq!(Settings::from_text(&s.to_text()), s);
                }
            }
        }
    }

    #[test]
    fn the_combo_setting_is_never_changed_by_the_menu() {
        let combo = button::BACK | button::START;
        let mut rig = Rig::with(Settings { menu_combo: combo, ..Settings::default() });
        rig.menu.update(FRAME, &[pad(0)]);
        rig.menu.open();
        for _ in 0..ITEMS.len() {
            rig.tap(pad(button::DPAD_DOWN));
            rig.tap(pad(button::DPAD_RIGHT));
            assert_eq!(rig.menu.settings().menu_combo, combo);
        }
    }

    // Confirm flow.

    #[test]
    fn reset_and_quit_are_reported_only_after_choosing_yes() {
        for (label, want) in
            [("Reset game", Outcome::ResetRequested), ("Quit", Outcome::QuitRequested)]
        {
            let mut rig = Rig::opened();
            rig.select(label);
            assert_eq!(rig.tap(a()), Outcome::None);
            assert!(rig.menu.confirm.is_some());
            assert_eq!(rig.tap(pad(button::DPAD_RIGHT)), Outcome::None);
            assert_eq!(rig.frame(a()), want);
            assert!(!rig.menu.is_open());
            // The confirming A does not reach the title.
            assert_eq!(rig.menu.filter_pad(a()), PadState::default());
        }
    }

    #[test]
    fn the_confirm_box_starts_on_no_and_a_then_only_dismisses_it() {
        let mut rig = Rig::opened();
        rig.select("Quit");
        rig.tap(a());
        assert_eq!(rig.tap(a()), Outcome::None);
        assert!(rig.menu.confirm.is_none());
        assert!(rig.menu.is_open());
        assert_eq!(ITEMS[rig.menu.selected].label, "Quit");
    }

    #[test]
    fn b_leaves_the_confirm_box_without_closing_the_menu() {
        let mut rig = Rig::opened();
        rig.select("Reset game");
        rig.tap(a());
        rig.tap(pad(button::DPAD_RIGHT));
        assert_eq!(rig.tap(b()), Outcome::None);
        assert!(rig.menu.confirm.is_none());
        assert!(rig.menu.is_open());
        // A second B closes the menu, with nothing requested.
        assert_eq!(rig.tap(b()), Outcome::Closed);
    }

    #[test]
    fn directions_in_the_confirm_box_choose_between_no_and_yes() {
        let mut rig = Rig::opened();
        rig.select("Quit");
        let row = rig.menu.selected;
        rig.tap(a());
        let yes = |rig: &Rig| rig.menu.confirm.unwrap().yes;
        assert!(!yes(&rig));
        rig.tap(pad(button::DPAD_LEFT));
        assert!(!yes(&rig));
        rig.tap(pad(button::DPAD_RIGHT));
        assert!(yes(&rig));
        rig.tap(pad(button::DPAD_RIGHT));
        assert!(yes(&rig));
        // The answers are side by side: up and down leave the choice.
        rig.tap(pad(button::DPAD_DOWN));
        assert!(yes(&rig));
        rig.tap(pad(button::DPAD_UP));
        assert!(yes(&rig));
        rig.tap(pad(button::DPAD_LEFT));
        assert!(!yes(&rig));
        rig.tap(pad(button::DPAD_UP));
        assert!(!yes(&rig));
        // The left stick chooses as the d-pad does.
        rig.tap(PadState { left_x: i16::MAX, ..PadState::default() });
        assert!(yes(&rig));
        rig.tap(PadState { left_x: i16::MIN, ..PadState::default() });
        assert!(!yes(&rig));
        assert_eq!(rig.menu.selected, row);
        assert_eq!(rig.menu.settings(), &Settings::default());
    }

    // Savestate slots.

    #[test]
    fn the_slots_page_loads_with_a_saves_with_x_and_leaves_with_b() {
        let mut rig = Rig::opened();
        let rows: Vec<SlotRow<'_>> = (0..SLOT_ROWS)
            .map(|i| SlotRow { used: i == 2, label: if i == 2 { "5 min ago" } else { "" } })
            .collect();
        rig.menu.set_slots(&rows);
        assert_eq!(rig.menu.shown_slot(), None);
        rig.select("Save states");
        assert_eq!(rig.tap(a()), Outcome::None);
        assert_eq!(rig.menu.shown_slot(), Some(0), "the page opens on the first slot");

        // A on an empty slot does nothing; down twice is the used one.
        assert_eq!(rig.tap(a()), Outcome::None);
        assert!(rig.menu.is_open());
        rig.tap(pad(button::DPAD_DOWN));
        rig.tap(pad(button::DPAD_DOWN));
        assert_eq!(rig.menu.shown_slot(), Some(2));
        // Up from the first slot wraps to the last.
        rig.tap(pad(button::DPAD_UP));
        rig.tap(pad(button::DPAD_UP));
        rig.tap(pad(button::DPAD_UP));
        assert_eq!(rig.menu.shown_slot(), Some(SLOT_ROWS - 1));

        // B goes back to the list, not out of the menu.
        assert_eq!(rig.tap(b()), Outcome::None);
        assert!(rig.menu.is_open());
        assert_eq!(rig.menu.shown_slot(), None);

        // A loads a used slot and closes the menu.
        rig.tap(a());
        rig.tap(pad(button::DPAD_DOWN));
        rig.tap(pad(button::DPAD_DOWN));
        assert_eq!(rig.tap(a()), Outcome::LoadSlot(2));
        assert!(!rig.menu.is_open());

        // X saves into the highlighted slot, used or not, and closes too.
        let mut rig = Rig::opened();
        rig.select("Save states");
        rig.tap(a());
        rig.tap(pad(button::DPAD_DOWN));
        assert_eq!(rig.tap(pad_analog(analog::X)), Outcome::SaveSlot(1));
        assert!(!rig.menu.is_open());
        // X on the list itself is nothing.
        let mut rig = Rig::opened();
        assert_eq!(rig.tap(pad_analog(analog::X)), Outcome::None);
        assert!(rig.menu.is_open());
    }

    #[test]
    fn start_closes_the_menu_from_the_confirm_box_without_confirming() {
        let mut rig = Rig::opened();
        rig.select("Quit");
        rig.tap(a());
        rig.tap(pad(button::DPAD_RIGHT));
        assert_eq!(rig.tap(pad(button::START)), Outcome::Closed);
        rig.menu.open();
        assert!(rig.menu.confirm.is_none());
    }

    // Dirty tracking and the rest of the surface.

    #[test]
    fn the_menu_is_dirty_exactly_when_what_it_shows_changed() {
        let mut rig = Rig::opened();
        assert!(rig.menu.is_dirty());
        let clean = |rig: &mut Rig| {
            let mut buf = vec![0u8; 64 * 48 * 4];
            let mut canvas = crate::Canvas::new(&mut buf, 64, 48).unwrap();
            rig.menu.render(&mut canvas);
            assert!(!rig.menu.is_dirty());
        };
        clean(&mut rig);
        rig.hold(pad(0), Duration::from_secs(1));
        assert!(!rig.menu.is_dirty(), "idle frames");

        rig.tap(pad(button::DPAD_DOWN));
        assert!(rig.menu.is_dirty(), "selection");
        clean(&mut rig);
        rig.tap(pad(button::DPAD_RIGHT));
        assert!(rig.menu.is_dirty(), "value");
        clean(&mut rig);
        rig.hold(pad(button::DPAD_RIGHT), Duration::from_secs(2));
        clean(&mut rig);
        rig.tap(pad(button::DPAD_RIGHT));
        assert!(!rig.menu.is_dirty(), "a value already at its end");

        let status = Status { cpu_backend: "KVM", gpu: "GPU", title: "Game", game: "", fps: 59.6 };
        rig.menu.set_status(&status);
        assert!(rig.menu.is_dirty(), "status");
        clean(&mut rig);
        rig.menu.set_status(&Status { fps: 60.4, ..status });
        assert!(!rig.menu.is_dirty(), "the same status as shown");
        rig.menu.set_status(&Status { fps: 30.0, ..status });
        assert!(rig.menu.is_dirty(), "fps");
        clean(&mut rig);

        rig.menu.set_capabilities(Capabilities::default());
        rig.menu.set_settings(rig.menu.settings().clone());
        assert!(!rig.menu.is_dirty(), "unchanged capabilities and settings");
        rig.menu.set_capabilities(Capabilities::all_live().next_boot(Setting::Fxaa));
        assert!(rig.menu.is_dirty(), "capabilities");
        clean(&mut rig);
        rig.menu.set_settings(Settings { volume: 5, ..Settings::default() });
        assert!(rig.menu.is_dirty(), "settings");
        clean(&mut rig);

        rig.select("Quit");
        clean(&mut rig);
        rig.tap(a());
        assert!(rig.menu.is_dirty(), "confirm box");
        clean(&mut rig);
        rig.tap(pad(button::DPAD_LEFT));
        assert!(!rig.menu.is_dirty(), "already on No");
        rig.tap(pad(button::DPAD_RIGHT));
        assert!(rig.menu.is_dirty(), "Yes");
        clean(&mut rig);
        rig.tap(b());
        assert!(rig.menu.is_dirty(), "confirm box dismissed");
        clean(&mut rig);

        rig.tap(b());
        rig.menu.open();
        assert!(rig.menu.is_dirty(), "reopened");
        clean(&mut rig);
        rig.menu.invalidate();
        assert!(rig.menu.is_dirty(), "invalidated");
    }

    #[test]
    fn status_fps_that_is_not_a_number_is_shown_as_zero() {
        let mut menu = Menu::new(Settings::default(), Capabilities::default());
        for fps in [f32::NAN, f32::INFINITY, -5.0] {
            menu.set_status(&Status { fps, ..Status::default() });
            assert_eq!(menu.status.fps, 0);
        }
    }

    #[test]
    fn open_and_close_by_call_match_the_combo() {
        let mut rig = Rig::with_combo();
        rig.frame(pad(0));
        rig.menu.open();
        assert!(rig.menu.is_open());
        rig.menu.close();
        assert!(!rig.menu.is_open());
        assert_eq!(rig.menu.hidden_buttons(), 0);
        // The combo still works afterwards.
        assert_eq!(rig.hold(pad(COMBO), COMBO_HOLD), [Outcome::Opened]);
    }
}
