//! Gamepads through Apple's GameController framework.
//!
//! macOS's own drivers feed this framework: an Xbox pad on USB, for one,
//! shows the rest of the system only its raw protocol, with no buttons or
//! axes in its HID descriptor. The framework names every pad it supports
//! (Xbox, DualSense, DualShock 4, Switch Pro, MFi) in one layout, the
//! extended gamepad, which this module reads and answers in `gilrs`'s names
//! so `input.rs` maps it as it maps every other host's pads.
//!
//! The framework keeps its list of controllers and their values current on
//! the main thread's run loop, which the window's event loop runs; this
//! module only reads them, once a frame, and finds connections and
//! disconnections by comparing the list with the last one.
//!
//! Rumble goes to the pad's actuators through Core Haptics ([`haptics`]).
//!
//! The bindings' calls are all `unsafe`: this module and its child are the
//! app's only ones with `unsafe` besides `surface.rs`.
#![expect(unsafe_code, reason = "GameController framework calls through objc2")]

use gilrs::{Axis, Button};
use objc2::rc::Retained;
use objc2_game_controller::{
    GCController, GCControllerButtonInput, GCDevice, GCExtendedGamepad, GCSystemGestureState,
};
use xb_core::{PadState, Rumble};

use super::{DeviceEvent, DeviceId};
use crate::input;

mod haptics;

use haptics::Haptics;

/// A controller with an extended gamepad, as last seen.
struct Known {
    id: DeviceId,
    controller: Retained<GCController>,
    /// Whether its Guide button was down at the last poll.
    guide_down: bool,
    /// Its actuators, opened on the first rumble.
    haptics: Option<Haptics>,
    /// The actuators could not be opened: not tried again for this pad.
    no_haptics: bool,
}

/// The host's gamepads as the GameController framework sees them.
pub struct Source {
    known: Vec<Known>,
    next_id: DeviceId,
}

/// The buttons of a reading, in `gilrs`'s names: whether each is down and
/// how far it is pressed (0.0 to 1.0).
const BUTTONS: usize = 16;

/// One gamepad's state at one moment.
struct Reading {
    buttons: [(Button, bool, f32); BUTTONS],
    axes: [(Axis, f32); 4],
}

impl Reading {
    fn button(&self, button: Button) -> Option<(bool, f32)> {
        self.buttons.iter().find(|(b, ..)| *b == button).map(|&(_, down, value)| (down, value))
    }
}

/// Reads every control of `pad`.
fn read(pad: &GCExtendedGamepad) -> Reading {
    let state = |b: Option<Retained<GCControllerButtonInput>>| {
        // SAFETY: `isPressed` and `value` are property getters of a live
        // element (held by `Retained`); they take no arguments.
        b.map_or((false, 0.0), |b| unsafe { (b.isPressed(), b.value()) })
    };
    // SAFETY: each call is a property getter of the live gamepad or of an
    // element it returns; none takes an argument. The framework updates the
    // values on the main thread and a getter returns one whole value.
    unsafe {
        let dpad = pad.dpad();
        let left = pad.leftThumbstick();
        let right = pad.rightThumbstick();
        let buttons = [
            (Button::South, Some(pad.buttonA())),
            (Button::East, Some(pad.buttonB())),
            (Button::West, Some(pad.buttonX())),
            (Button::North, Some(pad.buttonY())),
            (Button::LeftTrigger, Some(pad.leftShoulder())),
            (Button::RightTrigger, Some(pad.rightShoulder())),
            (Button::LeftTrigger2, Some(pad.leftTrigger())),
            (Button::RightTrigger2, Some(pad.rightTrigger())),
            (Button::Start, Some(pad.buttonMenu())),
            (Button::Select, pad.buttonOptions()),
            (Button::LeftThumb, pad.leftThumbstickButton()),
            (Button::RightThumb, pad.rightThumbstickButton()),
            (Button::DPadUp, Some(dpad.up())),
            (Button::DPadDown, Some(dpad.down())),
            (Button::DPadLeft, Some(dpad.left())),
            (Button::DPadRight, Some(dpad.right())),
        ]
        .map(|(name, b)| {
            let (down, value) = state(b);
            (name, down, value)
        });
        Reading {
            buttons,
            // Up and right are positive, as `gilrs` and the console have them.
            axes: [
                (Axis::LeftStickX, left.xAxis().value()),
                (Axis::LeftStickY, left.yAxis().value()),
                (Axis::RightStickX, right.xAxis().value()),
                (Axis::RightStickY, right.yAxis().value()),
            ],
        }
    }
}

/// The controller's extended gamepad, if it has one.
fn extended(controller: &GCController) -> Option<Retained<GCExtendedGamepad>> {
    // SAFETY: a property getter of a live controller, with no arguments.
    unsafe { controller.extendedGamepad() }
}

/// The controller's Guide button, if it has one.
fn guide(controller: &GCController) -> Option<Retained<GCControllerButtonInput>> {
    // SAFETY: a property getter of a live gamepad, with no arguments.
    extended(controller).and_then(|pad| unsafe { pad.buttonHome() })
}

impl Source {
    /// Starts reading the framework's controllers, those connected already
    /// included. The framework is always there, so this does not fail.
    pub fn open() -> Option<Self> {
        // SAFETY: a class property setter taking a plain `bool`. Pads are
        // read whether or not the window has focus, as on every host.
        unsafe { GCController::setShouldMonitorBackgroundEvents(true) };
        let mut source = Self { known: Vec::new(), next_id: 0 };
        source.scan(&mut Vec::new());
        Some(source)
    }

    /// Compares the framework's list of controllers with the last one and
    /// appends a connection or a disconnection for each difference.
    fn scan(&mut self, out: &mut Vec<DeviceEvent>) {
        // SAFETY: a class property getter with no arguments.
        let list = unsafe { GCController::controllers() };
        let now: Vec<Retained<GCController>> =
            list.iter().filter(|c| extended(c).is_some()).collect();
        self.known.retain(|k| {
            let still = now.iter().any(|c| Retained::as_ptr(c) == Retained::as_ptr(&k.controller));
            if !still {
                out.push(DeviceEvent::Disconnected(k.id));
            }
            still
        });
        for controller in now {
            if self
                .known
                .iter()
                .any(|k| Retained::as_ptr(&k.controller) == Retained::as_ptr(&controller))
            {
                continue;
            }
            if let Some(button) = guide(&controller) {
                // SAFETY: a property setter of a live element, taking one of
                // the type's own constants. Without it macOS takes the Guide
                // button for itself (Launchpad) and the menu never sees it.
                unsafe { button.setPreferredSystemGestureState(GCSystemGestureState::Disabled) };
            }
            let id = self.next_id;
            self.next_id += 1;
            self.known.push(Known {
                id,
                controller,
                guide_down: false,
                haptics: None,
                no_haptics: false,
            });
            out.push(DeviceEvent::Connected(id));
        }
    }

    fn controller(&self, id: DeviceId) -> Option<&GCController> {
        self.known.iter().find(|k| k.id == id).map(|k| &*k.controller)
    }

    /// The gamepads connected now.
    pub fn present(&self) -> Vec<DeviceId> {
        self.known.iter().map(|k| k.id).collect()
    }

    /// The gamepad's name: its vendor's name for it, or else the kind of
    /// pad the framework takes it for.
    pub fn name(&self, id: DeviceId) -> String {
        let Some(controller) = self.controller(id) else {
            return String::new();
        };
        // SAFETY: property getters of a live controller, with no arguments.
        let (vendor, category) = unsafe { (controller.vendorName(), controller.productCategory()) };
        vendor.map_or_else(|| category.to_string(), |v| v.to_string())
    }

    /// The gamepad's Guide button, by the framework's name, if it has one.
    pub fn guide_button(&self, id: DeviceId) -> Option<String> {
        guide(self.controller(id)?).map(|_| "buttonHome".to_owned())
    }

    /// Appends what happened since the last call: controllers that came or
    /// went, and Guide buttons that went down (the one button the app acts
    /// on as it goes down; the rest are read whole by [`Source::pad`]).
    pub fn events(&mut self, out: &mut Vec<DeviceEvent>) {
        self.scan(out);
        for known in &mut self.known {
            // SAFETY: a property getter of a live element, with no arguments.
            let down = guide(&known.controller).is_some_and(|b| unsafe { b.isPressed() });
            if down && !known.guide_down {
                log::debug!(target: "app", "gamepad {}: button Mode (buttonHome) pressed", known.id);
                out.push(DeviceEvent::Pressed(known.id, Button::Mode));
            }
            known.guide_down = down;
        }
    }

    /// The console pad the gamepad's state describes, if it is connected.
    pub fn pad(&self, id: DeviceId) -> Option<PadState> {
        let pad = extended(self.controller(id)?)?;
        let reading = read(&pad);
        Some(input::gamepad_pad(
            |b| reading.button(b).is_some_and(|(down, _)| down),
            |b| reading.button(b).map_or(0.0, |(_, value)| value),
            |a| reading.axes.iter().find(|(x, _)| *x == a).map_or(0.0, |(_, v)| *v),
        ))
    }

    /// Runs the gamepad's motors at `rumble` until told otherwise; all zero
    /// stops them. A pad without actuators the framework drives is said
    /// once at INFO and left alone.
    pub fn set_rumble(&mut self, id: DeviceId, rumble: Rumble) {
        let Some(known) = self.known.iter_mut().find(|k| k.id == id) else {
            return;
        };
        if known.haptics.is_none() {
            if rumble == Rumble::default() || known.no_haptics {
                return;
            }
            known.haptics = Haptics::open(&known.controller);
            if known.haptics.is_none() {
                known.no_haptics = true;
                log::info!(target: "app", "gamepad {id}: the title asked for rumble; the pad has no motors macOS drives");
                return;
            }
        }
        if known.haptics.as_mut().is_some_and(|h| !h.set(rumble)) {
            // Opened again on the next change.
            known.haptics = None;
        }
    }
}
