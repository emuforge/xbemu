//! Gamepads through `gilrs`: evdev on Linux, the host's own interfaces
//! elsewhere, with the SDL controller database naming each pad's buttons.

use std::collections::HashMap;

use gilrs::ff::{BaseEffect, BaseEffectType, Effect, EffectBuilder, Repeat, Replay, Ticks};
use gilrs::{EventType, Gamepad, GamepadId, Gilrs};
use xb_core::{PadState, Rumble};

use super::{DeviceEvent, DeviceId};
use crate::input;

/// The host's gamepads as `gilrs` sees them.
pub struct Source {
    gilrs: Gilrs,
    /// The running rumble effect of each gamepad; dropping one stops it.
    effects: HashMap<DeviceId, Effect>,
}

/// The id a `gilrs` gamepad goes by.
fn device_id(id: GamepadId) -> DeviceId {
    usize::from(id) as DeviceId
}

impl Source {
    /// Opens `gilrs`, or says why it cannot be and returns `None`.
    pub fn open() -> Option<Self> {
        match Gilrs::new() {
            Ok(gilrs) => Some(Self { gilrs, effects: HashMap::new() }),
            Err(gilrs::Error::NotImplemented(_)) => {
                log::warn!(target: "app", "gamepads are not supported on this host; keyboard only");
                None
            }
            Err(e) => {
                log::warn!(target: "app", "cannot open gamepads ({e}); keyboard only");
                None
            }
        }
    }

    fn gamepad(&self, id: DeviceId) -> Option<Gamepad<'_>> {
        self.gilrs.gamepads().find(|(g, _)| device_id(*g) == id).map(|(_, pad)| pad)
    }

    /// The gamepads connected now.
    pub fn present(&self) -> Vec<DeviceId> {
        self.gilrs.gamepads().map(|(id, _)| device_id(id)).collect()
    }

    /// The gamepad's name.
    pub fn name(&self, id: DeviceId) -> String {
        self.gamepad(id).map(|g| g.name().to_owned()).unwrap_or_default()
    }

    /// The host's code for the gamepad's Guide button, if its mapping has
    /// one.
    pub fn guide_button(&self, id: DeviceId) -> Option<String> {
        self.gamepad(id)?.button_code(gilrs::Button::Mode).map(|code| code.to_string())
    }

    /// Appends what happened since the last call.
    pub fn events(&mut self, out: &mut Vec<DeviceEvent>) {
        while let Some(event) = self.gilrs.next_event() {
            let id = device_id(event.id);
            match event.event {
                EventType::Connected => out.push(DeviceEvent::Connected(id)),
                EventType::Disconnected => {
                    self.effects.remove(&id);
                    out.push(DeviceEvent::Disconnected(id));
                }
                EventType::ButtonPressed(button, code) => {
                    log::debug!(target: "app", "gamepad {id}: button {button:?} ({code}) pressed");
                    out.push(DeviceEvent::Pressed(id, button));
                }
                _ => {}
            }
        }
    }

    /// The console pad the gamepad's state describes, if it is connected.
    pub fn pad(&self, id: DeviceId) -> Option<PadState> {
        let g = self.gamepad(id)?;
        Some(input::gamepad_pad(
            |b| g.is_pressed(b),
            |b| g.button_data(b).map_or(0.0, gilrs::ev::state::ButtonData::value),
            |a| g.value(a),
        ))
    }

    /// Runs the gamepad's motors at `rumble` until told otherwise; all zero
    /// stops them.
    pub fn set_rumble(&mut self, id: DeviceId, rumble: Rumble) {
        // Dropping the old effect stops it.
        self.effects.remove(&id);
        let Some(gamepad_id) =
            self.gilrs.gamepads().find(|(g, _)| device_id(*g) == id).map(|(g, _)| g)
        else {
            return;
        };
        if rumble == Rumble::default() || !self.gilrs.gamepad(gamepad_id).is_ff_supported() {
            return;
        }
        let motor = |kind| BaseEffect {
            kind,
            scheduling: Replay { play_for: Ticks::from_ms(1000), ..Replay::default() },
            ..BaseEffect::default()
        };
        // The console's left motor is the heavy, low-frequency one.
        let effect = EffectBuilder::new()
            .add_effect(motor(BaseEffectType::Strong { magnitude: rumble.left }))
            .add_effect(motor(BaseEffectType::Weak { magnitude: rumble.right }))
            .repeat(Repeat::Infinitely)
            .gamepads(&[gamepad_id])
            .finish(&mut self.gilrs)
            .and_then(|effect| effect.play().map(|()| effect));
        match effect {
            Ok(effect) => {
                self.effects.insert(id, effect);
            }
            Err(e) => log::debug!(target: "app", "rumble on gamepad {id}: {e}"),
        }
    }
}
