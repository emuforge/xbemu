//! Controllers: gamepads through `gilrs`, assigned to the console's four
//! ports in the order they are connected, with the keyboard as a second
//! source for port 1.
//!
//! Port 1 always has a pad plugged in (the keyboard is enough to drive it);
//! ports 2 to 4 have one while a gamepad is assigned to them. A gamepad that
//! is unplugged frees its port and the next one connected takes the lowest
//! free port.
//!
//! A gamepad's Guide button is the app's, not the console's: pressing it on
//! any connected gamepad (one without a port too) asks for the in-game menu
//! to be opened or closed.

use gilrs::ff::{BaseEffect, BaseEffectType, Effect, EffectBuilder, Repeat, Replay, Ticks};
use gilrs::{EventType, GamepadId, Gilrs};
use xb_core::{PORTS, PadState, Rumble};

use crate::input;

/// Which device sits in which port.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ports<T>([Option<T>; PORTS]);

impl<T: Copy + PartialEq> Ports<T> {
    /// Four empty ports.
    pub const fn new() -> Self {
        Self([None; PORTS])
    }

    /// Puts `device` in the lowest free port and returns the port, or `None`
    /// if it already has one or all four are taken.
    pub fn plug(&mut self, device: T) -> Option<usize> {
        if self.port_of(device).is_some() {
            return None;
        }
        let port = self.0.iter().position(Option::is_none)?;
        self.0[port] = Some(device);
        Some(port)
    }

    /// Frees the port `device` is in and returns it.
    pub fn unplug(&mut self, device: T) -> Option<usize> {
        let port = self.port_of(device)?;
        self.0[port] = None;
        Some(port)
    }

    /// The port `device` is in.
    pub fn port_of(&self, device: T) -> Option<usize> {
        self.0.iter().position(|d| *d == Some(device))
    }

    /// The device in `port`.
    pub fn device(&self, port: usize) -> Option<T> {
        self.0.get(port).copied().flatten()
    }
}

/// A port gained or lost its gamepad.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PortChange {
    /// The port, 0 to 3.
    pub port: usize,
    /// Whether a gamepad is now in it.
    pub connected: bool,
}

/// What [`PadHub::poll`] took in.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Polled {
    /// The ports whose gamepad came or went, in order.
    pub changes: Vec<PortChange>,
    /// The in-game menu should be opened if it is closed, or closed if it
    /// is open: a Guide button went down.
    pub menu_toggle: bool,
}

/// What the Guide button presses among `pressed` (the buttons that went
/// down since the last poll, in order) ask of the menu: each press toggles,
/// so two of them between polls cancel.
fn menu_toggle(pressed: impl IntoIterator<Item = gilrs::Button>) -> bool {
    pressed.into_iter().filter(|b| input::is_menu_button(*b)).count() % 2 == 1
}

/// The gamepads and their ports.
pub struct PadHub {
    /// `None` if the host's gamepad interface could not be opened; the
    /// keyboard still works.
    gilrs: Option<Gilrs>,
    ports: Ports<GamepadId>,
    rumble: [Rumble; PORTS],
    /// The running rumble effect of each port; dropping one stops it.
    effects: [Option<Effect>; PORTS],
}

impl PadHub {
    /// Opens the host's gamepad interface and assigns the gamepads already
    /// connected. Must be used on the thread that created it.
    pub fn new() -> Self {
        let gilrs = match Gilrs::new() {
            Ok(g) => Some(g),
            Err(gilrs::Error::NotImplemented(_)) => {
                log::warn!(target: "app", "gamepads are not supported on this host; keyboard only");
                None
            }
            Err(e) => {
                log::warn!(target: "app", "cannot open gamepads ({e}); keyboard only");
                None
            }
        };
        let mut hub = Self {
            gilrs,
            ports: Ports::new(),
            rumble: [Rumble::default(); PORTS],
            effects: [const { None }; PORTS],
        };
        let present: Vec<GamepadId> =
            hub.gilrs.iter().flat_map(|g| g.gamepads().map(|(id, _)| id)).collect();
        for id in present {
            hub.plug(id);
        }
        hub
    }

    fn plug(&mut self, id: GamepadId) -> Option<PortChange> {
        let name = self.gilrs.as_ref().map(|g| g.gamepad(id).name().to_owned()).unwrap_or_default();
        // What the pad's mapping has for the Guide button, if anything.
        match self.gilrs.as_ref().and_then(|g| g.gamepad(id).button_code(gilrs::Button::Mode)) {
            Some(code) => {
                log::debug!(target: "app", "gamepad '{name}': its Guide button ({code}) toggles the menu");
            }
            None => {
                log::info!(target: "app", "gamepad '{name}' has no Guide button; F1 opens the menu");
            }
        }
        match self.ports.plug(id) {
            Some(port) => {
                log::info!(target: "app", "gamepad '{name}' connected to port {}", port + 1);
                Some(PortChange { port, connected: true })
            }
            None => {
                if self.ports.port_of(id).is_none() {
                    log::info!(target: "app", "gamepad '{name}' ignored: all four ports are taken");
                }
                None
            }
        }
    }

    fn unplug(&mut self, id: GamepadId) -> Option<PortChange> {
        let port = self.ports.unplug(id)?;
        self.effects[port] = None;
        self.rumble[port] = Rumble::default();
        log::info!(target: "app", "gamepad disconnected from port {}", port + 1);
        Some(PortChange { port, connected: false })
    }

    /// The ports with a pad plugged in, as the core wants them at boot
    /// (bit n: port n). Port 1 always has one.
    pub fn connected_mask(&self) -> u8 {
        (0..PORTS).fold(1, |mask, port| mask | (u8::from(self.port_connected(port)) << port))
    }

    /// Whether the console should see a pad in `port`.
    pub fn port_connected(&self, port: usize) -> bool {
        port == 0 || self.ports.device(port).is_some()
    }

    /// Takes in the host's pending gamepad events and returns the ports
    /// whose gamepad came or went and whether a Guide button asked for the
    /// menu. Call once before reading [`PadHub::pads`].
    ///
    /// Every button that goes down is logged at DEBUG with the name `gilrs`
    /// gives it and the host's code, so `--log debug` shows what a pad's
    /// centre button is taken for.
    pub fn poll(&mut self) -> Polled {
        let mut changes = Vec::new();
        let mut pressed = Vec::new();
        while let Some(event) = self.gilrs.as_mut().and_then(Gilrs::next_event) {
            let change = match event.event {
                EventType::Connected => self.plug(event.id),
                EventType::Disconnected => self.unplug(event.id),
                EventType::ButtonPressed(button, code) => {
                    let port = self.ports.port_of(event.id);
                    log::debug!(
                        target: "app",
                        "gamepad {} (port {}): button {button:?} ({code}) pressed{}",
                        event.id,
                        port.map_or_else(|| "none".to_owned(), |p| (p + 1).to_string()),
                        if input::is_menu_button(button) { ": the Guide button, toggles the menu" } else { "" },
                    );
                    pressed.push(button);
                    None
                }
                _ => None,
            };
            changes.extend(change);
        }
        Polled { changes, menu_toggle: menu_toggle(pressed) }
    }

    /// The state of the four ports: each port's gamepad, and for port 1 the
    /// keyboard (`keyboard`) merged with it.
    pub fn pads(&self, keyboard: &PadState) -> [PadState; PORTS] {
        let mut pads = [PadState::default(); PORTS];
        for (port, pad) in pads.iter_mut().enumerate() {
            let gamepad = self.gilrs.as_ref().zip(self.ports.device(port));
            if let Some(g) = gamepad.and_then(|(gilrs, id)| gilrs.connected_gamepad(id)) {
                *pad = input::gamepad_pad(
                    |b| g.is_pressed(b),
                    |b| g.button_data(b).map_or(0.0, gilrs::ev::state::ButtonData::value),
                    |a| g.value(a),
                );
            }
        }
        pads[0] = input::merge(&pads[0], keyboard);
        pads
    }

    /// Sets the rumble motors of the gamepad in `port`, if it has them. Does
    /// nothing when the speeds are the ones already set. Best effort: a
    /// device that refuses is logged at DEBUG and left alone.
    pub fn set_rumble(&mut self, port: usize, rumble: Rumble) {
        if port >= PORTS || self.rumble[port] == rumble {
            return;
        }
        self.rumble[port] = rumble;
        // Dropping the old effect stops it.
        self.effects[port] = None;
        let (Some(gilrs), Some(id)) = (self.gilrs.as_mut(), self.ports.device(port)) else {
            return;
        };
        if rumble == Rumble::default() || !gilrs.gamepad(id).is_ff_supported() {
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
            .gamepads(&[id])
            .finish(gilrs)
            .and_then(|effect| effect.play().map(|()| effect));
        match effect {
            Ok(effect) => self.effects[port] = Some(effect),
            Err(e) => log::debug!(target: "app", "rumble on port {}: {e}", port + 1),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn devices_take_ports_in_the_order_they_connect() {
        let mut ports = Ports::new();
        assert_eq!(
            ["a", "b", "c", "d"].map(|d| ports.plug(d)),
            [Some(0), Some(1), Some(2), Some(3)]
        );
        assert_eq!(ports.plug("e"), None, "a fifth device has no port");
        assert_eq!(ports.device(2), Some("c"));
    }

    #[test]
    fn an_unplugged_port_goes_to_the_next_device() {
        let mut ports = Ports::new();
        for d in ["a", "b", "c"] {
            ports.plug(d);
        }
        assert_eq!(ports.unplug("b"), Some(1));
        assert_eq!(ports.device(1), None);
        assert_eq!(ports.port_of("c"), Some(2), "the others stay where they are");
        assert_eq!(ports.plug("x"), Some(1));
        assert_eq!(ports.unplug("b"), None, "it is no longer plugged in");
    }

    #[test]
    fn a_guide_press_toggles_the_menu_and_other_buttons_do_not() {
        use gilrs::Button;
        assert!(menu_toggle([Button::Mode]));
        assert!(menu_toggle([Button::South, Button::Mode, Button::LeftThumb]));
        assert!(!menu_toggle([]));
        assert!(!menu_toggle([
            Button::South,
            Button::Start,
            Button::Select,
            Button::LeftThumb,
            Button::RightThumb,
            Button::Unknown,
        ]));
        // Two presses between polls open and close again.
        assert!(!menu_toggle([Button::Mode, Button::Mode]));
        assert!(menu_toggle([Button::Mode, Button::Mode, Button::Mode]));
    }

    #[test]
    fn a_device_announced_twice_keeps_its_one_port() {
        let mut ports = Ports::new();
        assert_eq!(ports.plug(7), Some(0));
        assert_eq!(ports.plug(7), None);
        assert_eq!(ports.plug(8), Some(1));
    }
}
