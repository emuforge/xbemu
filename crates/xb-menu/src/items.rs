//! The menu's rows as data: one table, [`ITEMS`], that navigation and
//! rendering both walk.
//!
//! Adding a setting is one [`Setting`] variant and one [`Item`] entry: its
//! label, the values it takes, and a getter and a setter on
//! [`Settings`]. Nothing else in the crate names individual settings.

use core::fmt::Write as _;

use xb_types::settings::{Aspect, CpuBackend, Settings};

use crate::text::TextBuf;

/// The settings the menu edits, used to say which of them apply immediately
/// (see [`Capabilities`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Setting {
    /// [`Settings::render_scale`].
    RenderScale,
    /// [`Settings::aspect`].
    Aspect,
    /// [`Settings::widescreen`].
    Widescreen,
    /// [`Settings::fxaa`].
    Fxaa,
    /// [`Settings::anisotropy`].
    Anisotropy,
    /// [`Settings::vsync`].
    Vsync,
    /// [`Settings::show_fps`].
    ShowFps,
    /// [`Settings::fullscreen`].
    Fullscreen,
    /// [`Settings::fast_disc`].
    FastDisc,
    /// [`Settings::volume`].
    Volume,
    /// [`Settings::cpu`].
    Cpu,
}

impl Setting {
    const fn bit(self) -> u16 {
        1 << self as u16
    }
}

/// Which settings take effect only at the next boot.
///
/// The core builds this from what its parts can do while a title runs, and
/// the menu marks the rows that need a restart. It changes nothing else: a
/// "next boot" setting is edited and reported like any other.
///
/// The default marks [`Setting::Cpu`] and [`Setting::Widescreen`] (a title
/// reads the console's widescreen flag when it starts) and treats the rest
/// as live.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    next_boot: u16,
}

impl Default for Capabilities {
    fn default() -> Self {
        Self::all_live().next_boot(Setting::Cpu).next_boot(Setting::Widescreen)
    }
}

impl Capabilities {
    /// Every setting applies immediately.
    #[must_use]
    pub const fn all_live() -> Self {
        Self { next_boot: 0 }
    }

    /// Marks `setting` as applying at the next boot.
    #[must_use]
    pub const fn next_boot(self, setting: Setting) -> Self {
        Self { next_boot: self.next_boot | setting.bit() }
    }

    /// Marks `setting` as applying immediately.
    #[must_use]
    pub const fn live(self, setting: Setting) -> Self {
        Self { next_boot: self.next_boot & !setting.bit() }
    }

    /// Whether `setting` waits for the next boot.
    #[must_use]
    pub const fn needs_restart(&self, setting: Setting) -> bool {
        self.next_boot & setting.bit() != 0
    }
}

/// The values a setting row steps through. A row's getter and setter speak
/// in the `i32` this defines.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Values {
    /// Off (0) or on (1).
    Toggle,
    /// One of the named choices; the value is the index.
    Choice(&'static [&'static str]),
    /// A number from `min` to `max` in steps of `step`, shown with `unit`
    /// appended.
    Range { min: i32, max: i32, step: i32, unit: &'static str },
}

impl Values {
    /// `current` brought into the valid range, for settings that arrive out
    /// of range from a hand-built [`Settings`].
    fn clamp(self, current: i32) -> i32 {
        match self {
            Self::Toggle => current.clamp(0, 1),
            Self::Choice(names) => current.clamp(0, names.len() as i32 - 1),
            Self::Range { min, max, .. } => current.clamp(min, max),
        }
    }

    /// The value one step from `current` in direction `dir` (negative is
    /// down). At an end, `wrap` continues from the other end and otherwise
    /// the value stays. A toggle flips in either direction. A range value
    /// off the step grid moves to the next grid value in that direction.
    pub(crate) fn step(self, current: i32, dir: i32, wrap: bool) -> i32 {
        let current = self.clamp(current);
        let (min, max, next) = match self {
            Self::Toggle => return 1 - current,
            Self::Choice(names) => (0, names.len() as i32 - 1, current + dir.signum()),
            Self::Range { min, max, step, .. } => {
                let off = current - min;
                let next = if dir > 0 {
                    (off.div_euclid(step) + 1) * step
                } else {
                    (off + step - 1).div_euclid(step) * step - step
                };
                (min, max, min + next)
            }
        };
        if wrap && next > max && current == max {
            min
        } else if wrap && next < min && current == min {
            max
        } else {
            next.clamp(min, max)
        }
    }

    /// Whether a step in direction `dir`, without wrapping, changes the value.
    pub(crate) fn can_step(self, current: i32, dir: i32) -> bool {
        self.step(current, dir, false) != self.clamp(current)
    }

    /// Writes how `current` is shown.
    pub(crate) fn format<const N: usize>(self, current: i32, out: &mut TextBuf<N>) {
        let current = self.clamp(current);
        match self {
            Self::Toggle => out.push_str(if current != 0 { "On" } else { "Off" }),
            Self::Choice(names) => out.push_str(names.get(current as usize).copied().unwrap_or("")),
            Self::Range { unit, .. } => {
                // TextBuf's writer truncates rather than fails.
                let _ = write!(out, "{current}{unit}");
            }
        }
    }
}

/// An action that asks for confirmation first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    /// Restart the title.
    Reset,
    /// Leave the emulator.
    Quit,
}

impl Action {
    /// The question the confirm box asks.
    pub(crate) const fn question(self) -> &'static str {
        match self {
            Self::Reset => "Reset the game?",
            Self::Quit => "Quit?",
        }
    }
}

/// A line of the caller's status shown inside the list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Info {
    /// The CPU backend running now.
    ActiveCpu,
}

/// What a row does.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Row {
    /// Closes the menu.
    Resume,
    /// Edits one setting.
    Setting {
        /// Identity, for [`Capabilities`].
        id: Setting,
        /// The values it steps through.
        values: Values,
        /// Reads the setting as a [`Values`] number.
        get: fn(&Settings) -> i32,
        /// Stores a [`Values`] number, already in range.
        set: fn(&mut Settings, i32),
    },
    /// Read-only; the selection skips it.
    Info(Info),
    /// Asks for confirmation, then reports the action.
    Confirm(Action),
    /// Opens the page of savestate slots.
    Slots,
}

impl Row {
    /// Whether the selection can rest on this row.
    pub(crate) const fn selectable(&self) -> bool {
        !matches!(self, Self::Info(_))
    }
}

/// One row of the menu.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Item {
    /// Text at the left of the row.
    pub(crate) label: &'static str,
    /// What the row does.
    pub(crate) row: Row,
}

/// Forced anisotropy levels in menu order; 0 is "the title's own".
const ANISOTROPY: [u8; 5] = [0, 2, 4, 8, 16];

/// The menu, top to bottom.
pub(crate) static ITEMS: &[Item] = &[
    Item { label: "Resume", row: Row::Resume },
    Item {
        label: "Render scale",
        row: Row::Setting {
            id: Setting::RenderScale,
            values: Values::Range { min: 1, max: 4, step: 1, unit: "x" },
            get: |s| i32::from(s.render_scale),
            set: |s, v| s.render_scale = v as u8,
        },
    },
    Item {
        label: "Fullscreen",
        row: Row::Setting {
            id: Setting::Fullscreen,
            values: Values::Toggle,
            get: |s| i32::from(s.fullscreen),
            set: |s, v| s.fullscreen = v != 0,
        },
    },
    Item {
        label: "Aspect",
        row: Row::Setting {
            id: Setting::Aspect,
            values: Values::Choice(&["4:3", "16:9", "Stretch"]),
            get: |s| match s.aspect {
                Aspect::Standard => 0,
                Aspect::Wide => 1,
                Aspect::Stretch => 2,
            },
            set: |s, v| {
                s.aspect = match v {
                    1 => Aspect::Wide,
                    2 => Aspect::Stretch,
                    _ => Aspect::Standard,
                }
            },
        },
    },
    Item {
        label: "Widescreen",
        row: Row::Setting {
            id: Setting::Widescreen,
            values: Values::Toggle,
            get: |s| i32::from(s.widescreen),
            set: |s, v| s.widescreen = v != 0,
        },
    },
    Item {
        label: "FXAA",
        row: Row::Setting {
            id: Setting::Fxaa,
            values: Values::Toggle,
            get: |s| i32::from(s.fxaa),
            set: |s, v| s.fxaa = v != 0,
        },
    },
    Item {
        label: "Anisotropic filter",
        row: Row::Setting {
            id: Setting::Anisotropy,
            values: Values::Choice(&["Game", "2x", "4x", "8x", "16x"]),
            get: |s| ANISOTROPY.iter().position(|&a| a == s.anisotropy).unwrap_or(0) as i32,
            set: |s, v| s.anisotropy = ANISOTROPY[v as usize],
        },
    },
    Item {
        label: "VSync",
        row: Row::Setting {
            id: Setting::Vsync,
            values: Values::Toggle,
            get: |s| i32::from(s.vsync),
            set: |s, v| s.vsync = v != 0,
        },
    },
    Item {
        label: "Show FPS",
        row: Row::Setting {
            id: Setting::ShowFps,
            values: Values::Toggle,
            get: |s| i32::from(s.show_fps),
            set: |s, v| s.show_fps = v != 0,
        },
    },
    Item {
        label: "Disc speed",
        row: Row::Setting {
            id: Setting::FastDisc,
            values: Values::Choice(&["Console", "Fast"]),
            get: |s| i32::from(s.fast_disc),
            set: |s, v| s.fast_disc = v != 0,
        },
    },
    Item {
        label: "Volume",
        row: Row::Setting {
            id: Setting::Volume,
            values: Values::Range { min: 0, max: 100, step: 5, unit: "%" },
            get: |s| i32::from(s.volume),
            set: |s, v| s.volume = v as u8,
        },
    },
    Item {
        label: "CPU backend",
        row: Row::Setting {
            id: Setting::Cpu,
            values: Values::Choice(&["Auto", "KVM", "Translator"]),
            get: |s| match s.cpu {
                CpuBackend::Auto => 0,
                CpuBackend::Kvm => 1,
                CpuBackend::Translator => 2,
            },
            set: |s, v| {
                s.cpu = match v {
                    1 => CpuBackend::Kvm,
                    2 => CpuBackend::Translator,
                    _ => CpuBackend::Auto,
                }
            },
        },
    },
    Item { label: "  running now", row: Row::Info(Info::ActiveCpu) },
    Item { label: "Save states", row: Row::Slots },
    Item { label: "Reset game", row: Row::Confirm(Action::Reset) },
    Item { label: "Quit", row: Row::Confirm(Action::Quit) },
];

#[cfg(test)]
mod tests {
    use super::*;

    const VOLUME: Values = Values::Range { min: 0, max: 100, step: 5, unit: "%" };
    const SCALE: Values = Values::Range { min: 1, max: 4, step: 1, unit: "x" };
    const THREE: Values = Values::Choice(&["a", "b", "c"]);

    #[test]
    fn stepping_stops_at_the_ends_unless_wrapping() {
        assert_eq!(SCALE.step(4, 1, false), 4);
        assert_eq!(SCALE.step(4, 1, true), 1);
        assert_eq!(SCALE.step(1, -1, false), 1);
        assert_eq!(SCALE.step(1, -1, true), 4);
        assert_eq!(THREE.step(2, 1, false), 2);
        assert_eq!(THREE.step(2, 1, true), 0);
        assert_eq!(THREE.step(0, -1, true), 2);
        assert_eq!(VOLUME.step(100, 1, true), 0);
        assert_eq!(VOLUME.step(0, -1, false), 0);
    }

    #[test]
    fn a_toggle_flips_in_both_directions() {
        for dir in [-1, 1] {
            assert_eq!(Values::Toggle.step(0, dir, false), 1);
            assert_eq!(Values::Toggle.step(1, dir, false), 0);
        }
    }

    #[test]
    fn an_off_grid_range_value_moves_to_the_neighbouring_grid_value() {
        assert_eq!(VOLUME.step(37, 1, false), 40);
        assert_eq!(VOLUME.step(37, -1, false), 35);
        assert_eq!(VOLUME.step(98, 1, true), 100);
        assert_eq!(VOLUME.step(2, -1, true), 0);
    }

    #[test]
    fn out_of_range_values_are_brought_into_range_before_stepping() {
        assert_eq!(SCALE.step(0, 1, false), 2);
        assert_eq!(SCALE.step(9, -1, false), 3);
        assert_eq!(THREE.step(-4, 1, false), 1);
        assert_eq!(VOLUME.step(250, -1, false), 95);
    }

    #[test]
    fn stepping_up_then_down_returns_to_any_grid_value() {
        for values in [VOLUME, SCALE, THREE] {
            let (min, max, step) = match values {
                Values::Range { min, max, step, .. } => (min, max, step),
                Values::Choice(n) => (0, n.len() as i32 - 1, 1),
                Values::Toggle => unreachable!(),
            };
            for v in (min..max).step_by(step as usize) {
                assert_eq!(values.step(values.step(v, 1, false), -1, false), v);
            }
        }
    }

    #[test]
    fn values_are_shown_with_their_names_and_units() {
        let show = |values: Values, v| {
            let mut out = TextBuf::<16>::new();
            values.format(v, &mut out);
            out.as_str().to_owned()
        };
        assert_eq!(show(VOLUME, 85), "85%");
        assert_eq!(show(SCALE, 3), "3x");
        assert_eq!(show(THREE, 1), "b");
        assert_eq!(show(Values::Toggle, 1), "On");
        assert_eq!(show(Values::Toggle, 0), "Off");
    }

    #[test]
    fn every_setting_row_round_trips_each_of_its_values() {
        for item in ITEMS {
            let Row::Setting { values, get, set, .. } = item.row else { continue };
            let mut s = Settings::default();
            let first = values.step(get(&s), -1, true);
            let mut v = first;
            let mut seen = 0;
            loop {
                set(&mut s, v);
                assert_eq!(get(&s), v, "{}", item.label);
                seen += 1;
                v = values.step(v, 1, true);
                if v == first {
                    break;
                }
                assert!(seen < 200, "{} never cycles", item.label);
            }
            assert!(seen >= 2, "{}", item.label);
        }
    }

    #[test]
    fn each_setting_appears_once_and_the_first_row_is_selectable() {
        let ids: Vec<Setting> = ITEMS
            .iter()
            .filter_map(|i| match i.row {
                Row::Setting { id, .. } => Some(id),
                _ => None,
            })
            .collect();
        for (n, id) in ids.iter().enumerate() {
            assert!(!ids[..n].contains(id), "{id:?} twice");
        }
        assert!(ITEMS[0].row.selectable());
    }

    #[test]
    fn capabilities_mark_and_unmark_single_settings() {
        let caps = Capabilities::all_live().next_boot(Setting::Fxaa);
        assert!(caps.needs_restart(Setting::Fxaa));
        assert!(!caps.needs_restart(Setting::Vsync));
        assert!(!caps.live(Setting::Fxaa).needs_restart(Setting::Fxaa));
        assert!(Capabilities::default().needs_restart(Setting::Cpu));
        assert!(!Capabilities::default().needs_restart(Setting::Volume));
    }
}
