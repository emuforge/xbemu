//! Rumble through Core Haptics: the GameController framework gives each of
//! a pad's actuators its own haptic engine.
//!
//! A pad whose two handles have their own actuators (an Xbox pad's heavy
//! left and light right motors) gets one engine each, so the console's two
//! motors stay apart; any other pad gets the default engine, at the
//! stronger of the two speeds. Each engine plays one continuous event that
//! never ends on its own, whose strength follows the console's motor
//! through the dynamic intensity control, and which is stopped while the
//! motor is still. This is the arrangement SDL uses on Apple hosts.
#![expect(unsafe_code, reason = "Core Haptics and GameController calls through objc2")]

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, msg_send};
use objc2_core_haptics::{
    CHHapticDynamicParameter, CHHapticDynamicParameterIDHapticIntensityControl, CHHapticEngine,
    CHHapticEvent, CHHapticEventParameter, CHHapticEventParameterIDHapticIntensity,
    CHHapticEventTypeHapticContinuous, CHHapticPattern, CHHapticPatternPlayer,
    CHHapticTimeImmediate,
};
use objc2_foundation::{NSArray, NSError};
use objc2_game_controller::{
    GCController, GCDeviceHaptics, GCHapticDurationInfinite, GCHapticsLocality,
    GCHapticsLocalityDefault, GCHapticsLocalityLeftHandle, GCHapticsLocalityRightHandle,
};
use xb_core::Rumble;

/// One actuator: its engine and the player of its continuous event.
struct Motor {
    engine: Retained<CHHapticEngine>,
    player: Retained<ProtocolObject<dyn CHHapticPatternPlayer>>,
    /// The event is playing (a still motor has it stopped).
    playing: bool,
}

/// The text of a framework error, for the log.
fn describe(e: &NSError) -> String {
    e.localizedDescription().to_string()
}

impl Motor {
    /// The engine of the actuators at `locality`, started, with its event
    /// ready to play; `None` if the pad has none there.
    fn open(haptics: &GCDeviceHaptics, locality: &GCHapticsLocality) -> Option<Self> {
        // SAFETY: `createEngineWithLocality:` takes a locality string and
        // returns a new engine or nil; it is in the framework on macOS 11
        // and later (its header says so) but the bindings declare it for
        // iOS alone, hence the direct message.
        let engine: Option<Retained<CHHapticEngine>> =
            unsafe { msg_send![haptics, createEngineWithLocality: locality] };
        let engine = engine?;
        let started = (|| {
            // SAFETY: each call creates an object from plain values and the
            // framework's own constants, or starts the engine just made;
            // the arrays outlive the calls that take them.
            unsafe {
                engine.startAndReturnError()?;
                let intensity = CHHapticEventParameter::initWithParameterID_value(
                    CHHapticEventParameter::alloc(),
                    CHHapticEventParameterIDHapticIntensity,
                    1.0,
                );
                let event = CHHapticEvent::initWithEventType_parameters_relativeTime_duration(
                    CHHapticEvent::alloc(),
                    CHHapticEventTypeHapticContinuous,
                    &NSArray::from_retained_slice(&[intensity]),
                    0.0,
                    f64::from(GCHapticDurationInfinite),
                );
                let pattern = CHHapticPattern::initWithEvents_parameters_error(
                    CHHapticPattern::alloc(),
                    &NSArray::from_retained_slice(&[event]),
                    &NSArray::new(),
                )?;
                engine.createPlayerWithPattern_error(&pattern)
            }
        })();
        match started {
            Ok(player) => Some(Self { engine, player, playing: false }),
            Err(e) => {
                log::debug!(target: "app", "rumble: the haptic engine did not start: {}", describe(&e));
                None
            }
        }
    }

    /// Runs the actuator at `speed` (0.0 still to 1.0 full), restarting the
    /// engine once if the framework stopped it (it does when the app is
    /// sent to the background). False if it cannot be run.
    fn set(&mut self, speed: f32) -> bool {
        match self.try_set(speed) {
            Ok(()) => true,
            Err(_) => {
                self.playing = false;
                // SAFETY: starting a live engine takes no arguments.
                let restarted = unsafe { self.engine.startAndReturnError() };
                match restarted.and_then(|()| self.try_set(speed)) {
                    Ok(()) => true,
                    Err(e) => {
                        log::debug!(target: "app", "rumble: {}", describe(&e));
                        false
                    }
                }
            }
        }
    }

    fn try_set(&mut self, speed: f32) -> Result<(), Retained<NSError>> {
        // SAFETY: the player is live; each call takes a plain time, or an
        // array of one parameter made here from the framework's constant
        // and a value in the control's range (0.0 to 1.0).
        unsafe {
            if speed <= 0.0 {
                if self.playing {
                    self.player.stopAtTime_error(CHHapticTimeImmediate)?;
                    self.playing = false;
                }
                return Ok(());
            }
            let control = CHHapticDynamicParameter::initWithParameterID_value_relativeTime(
                CHHapticDynamicParameter::alloc(),
                CHHapticDynamicParameterIDHapticIntensityControl,
                speed.min(1.0),
                0.0,
            );
            if !self.playing {
                self.player.startAtTime_error(CHHapticTimeImmediate)?;
                self.playing = true;
            }
            self.player.sendParameters_atTime_error(
                &NSArray::from_retained_slice(&[control]),
                CHHapticTimeImmediate,
            )
        }
    }
}

/// A console motor's speed as an actuator's, 0.0 to 1.0.
fn speed(magnitude: u16) -> f32 {
    f32::from(magnitude) / f32::from(u16::MAX)
}

/// The actuators of one pad.
pub struct Haptics(Actuators);

/// How a pad's actuators stand for the console's two motors.
enum Actuators {
    /// One for each handle: the console's left motor and its right.
    Handles(Motor, Motor),
    /// One for the whole pad.
    Single(Motor),
}

impl Haptics {
    /// The pad's actuators, or `None` if it has none the framework drives.
    pub fn open(controller: &GCController) -> Option<Self> {
        // SAFETY: a property getter of a live controller, with no arguments.
        let haptics = unsafe { controller.haptics() }?;
        // SAFETY: the framework's locality constants, read as the `&'static`
        // strings they are declared as.
        let (left, right, default) = unsafe {
            (GCHapticsLocalityLeftHandle, GCHapticsLocalityRightHandle, GCHapticsLocalityDefault)
        };
        match (Motor::open(&haptics, left), Motor::open(&haptics, right)) {
            (Some(l), Some(r)) => Some(Self(Actuators::Handles(l, r))),
            _ => Motor::open(&haptics, default).map(|m| Self(Actuators::Single(m))),
        }
    }

    /// Runs the actuators as the console's motors ask. False if they cannot
    /// be run: the caller drops them and opens them again on the next ask.
    pub fn set(&mut self, rumble: Rumble) -> bool {
        match &mut self.0 {
            // The console's left motor is the heavy one, as the left handle's is.
            Actuators::Handles(l, r) => l.set(speed(rumble.left)) & r.set(speed(rumble.right)),
            Actuators::Single(m) => m.set(speed(rumble.left.max(rumble.right))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::speed;

    #[test]
    fn a_motor_speed_spans_the_actuators_whole_range() {
        assert_eq!(speed(0), 0.0);
        assert_eq!(speed(u16::MAX), 1.0);
        assert!((speed(u16::MAX / 2) - 0.5).abs() < 1e-4);
    }
}
