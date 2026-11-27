//! The state of an Xbox controller as a front end reports it.
//!
//! This is the logical state (which buttons are down, where the sticks are),
//! independent of the USB XID report `xb-devices` builds from it.

/// Digital buttons, as bits of [`PadState::buttons`]. The values are the XID
/// report's `wButtons` bits, so the device copies the field through.
pub mod button {
    /// D-pad up.
    pub const DPAD_UP: u16 = 0x0001;
    /// D-pad down.
    pub const DPAD_DOWN: u16 = 0x0002;
    /// D-pad left.
    pub const DPAD_LEFT: u16 = 0x0004;
    /// D-pad right.
    pub const DPAD_RIGHT: u16 = 0x0008;
    /// Start.
    pub const START: u16 = 0x0010;
    /// Back.
    pub const BACK: u16 = 0x0020;
    /// Left stick pressed.
    pub const LEFT_THUMB: u16 = 0x0040;
    /// Right stick pressed.
    pub const RIGHT_THUMB: u16 = 0x0080;
}

/// Indices into [`PadState::analog`], the pressure-sensitive buttons, in the
/// XID report's order.
pub mod analog {
    /// A.
    pub const A: usize = 0;
    /// B.
    pub const B: usize = 1;
    /// X.
    pub const X: usize = 2;
    /// Y.
    pub const Y: usize = 3;
    /// Black.
    pub const BLACK: usize = 4;
    /// White.
    pub const WHITE: usize = 5;
    /// Left trigger.
    pub const LEFT_TRIGGER: usize = 6;
    /// Right trigger.
    pub const RIGHT_TRIGGER: usize = 7;
}

/// Number of controller ports.
pub const PORTS: usize = 4;

/// One controller's state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PadState {
    /// Digital buttons ([`button`] bits).
    pub buttons: u16,
    /// Pressure of the analog buttons and triggers, 0 (released) to 255,
    /// indexed by [`analog`].
    pub analog: [u8; 8],
    /// Left stick X, -32768 (left) to 32767 (right).
    pub left_x: i16,
    /// Left stick Y, -32768 (down) to 32767 (up).
    pub left_y: i16,
    /// Right stick X.
    pub right_x: i16,
    /// Right stick Y.
    pub right_y: i16,
}

impl PadState {
    /// Whether every digital button in `mask` is down.
    #[must_use]
    pub const fn all_down(&self, mask: u16) -> bool {
        self.buttons & mask == mask
    }

    /// Whether the analog button `index` is pressed past the threshold games
    /// treat as "down".
    #[must_use]
    pub const fn analog_down(&self, index: usize) -> bool {
        self.analog[index] >= 0x40
    }
}

/// Rumble motor speeds a title last set, 0 (off) to 65535.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rumble {
    /// Left (low frequency) motor.
    pub left: u16,
    /// Right (high frequency) motor.
    pub right: u16,
}
