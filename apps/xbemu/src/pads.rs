//! Controllers: the host's gamepads, assigned to the console's four ports
//! in the order they are connected, with the keyboard as a second source
//! for port 1.
//!
//! Port 1 always has a pad plugged in (the keyboard is enough to drive it);
//! ports 2 to 4 have one while a gamepad is assigned to them. A gamepad that
//! is unplugged frees its port and the next one connected takes the lowest
//! free port.
//!
//! A gamepad's Guide button is the app's, not the console's: pressing it on
//! any connected gamepad (one without a port too) asks for the in-game menu
//! to be opened or closed.
//!
//! The devices are read by one source a host: Apple's GameController
//! framework on macOS ([`macos`]), whose drivers are the only way some pads
//! are read there (an Xbox pad on USB shows no buttons to anything else),
//! and `gilrs` elsewhere ([`portable`]). Both answer in `gilrs`'s names for
//! buttons and axes, so `input.rs` maps every host's pads the same way.

use xb_core::{PORTS, PadState, Rumble};

use crate::input;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(not(target_os = "macos"))]
mod portable;

#[cfg(target_os = "macos")]
use macos::Source;
#[cfg(not(target_os = "macos"))]
use portable::Source;

/// A gamepad as its source names it.
pub type DeviceId = u64;

/// What a source saw happen since it was last asked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeviceEvent {
    /// A gamepad appeared.
    Connected(DeviceId),
    /// A gamepad went away.
    Disconnected(DeviceId),
    /// A gamepad's button went down.
    Pressed(DeviceId, gilrs::Button),
}

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
    source: Option<Source>,
    ports: Ports<DeviceId>,
    rumble: [Rumble; PORTS],
}

impl PadHub {
    /// Opens the host's gamepad interface and assigns the gamepads already
    /// connected. Must be used on the thread that created it.
    pub fn new() -> Self {
        let source = Source::open();
        let mut hub = Self { source, ports: Ports::new(), rumble: [Rumble::default(); PORTS] };
        let present = hub.source.as_ref().map(Source::present).unwrap_or_default();
        for id in present {
            hub.plug(id);
        }
        hub
    }

    fn plug(&mut self, id: DeviceId) -> Option<PortChange> {
        let source = self.source.as_ref()?;
        let name = source.name(id);
        match source.guide_button(id) {
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

    fn unplug(&mut self, id: DeviceId) -> Option<PortChange> {
        let port = self.ports.unplug(id)?;
        if let Some(source) = self.source.as_mut() {
            source.set_rumble(id, Rumble::default());
        }
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
    /// Every button that goes down is logged at DEBUG by its source, so
    /// `--log debug` shows what a pad's centre button is taken for.
    pub fn poll(&mut self) -> Polled {
        let mut events = Vec::new();
        if let Some(source) = self.source.as_mut() {
            source.events(&mut events);
        }
        let mut changes = Vec::new();
        let mut pressed = Vec::new();
        for event in events {
            let change = match event {
                DeviceEvent::Connected(id) => self.plug(id),
                DeviceEvent::Disconnected(id) => self.unplug(id),
                DeviceEvent::Pressed(id, button) => {
                    if input::is_menu_button(button) {
                        log::debug!(
                            target: "app",
                            "gamepad {id} (port {}): the Guide button, toggles the menu",
                            self.ports.port_of(id).map_or_else(|| "none".to_owned(), |p| (p + 1).to_string()),
                        );
                    }
                    pressed.push(button);
                    None
                }
            };
            changes.extend(change);
        }
        Polled { changes, menu_toggle: menu_toggle(pressed) }
    }

    /// The state of the four ports: each port's gamepad, and for port 1 the
    /// keyboard (`keyboard`) merged with it.
    pub fn pads(&self, keyboard: &PadState) -> [PadState; PORTS] {
        let mut pads = [PadState::default(); PORTS];
        if let Some(source) = self.source.as_ref() {
            for (port, pad) in pads.iter_mut().enumerate() {
                if let Some(state) = self.ports.device(port).and_then(|id| source.pad(id)) {
                    *pad = state;
                }
            }
        }
        pads[0] = input::merge(&pads[0], keyboard);
        pads
    }

    /// Sets the rumble motors of the gamepad in `port`, if it has them. Does
    /// nothing when the speeds are the ones already set. Best effort: a
    /// device that refuses is logged at DEBUG by its source and left alone.
    pub fn set_rumble(&mut self, port: usize, rumble: Rumble) {
        if port >= PORTS || self.rumble[port] == rumble {
            return;
        }
        self.rumble[port] = rumble;
        if let (Some(source), Some(id)) = (self.source.as_mut(), self.ports.device(port)) {
            source.set_rumble(id, rumble);
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
