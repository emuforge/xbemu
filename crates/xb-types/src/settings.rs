//! The core's user settings: what the in-game menu shows and a front end may
//! set through its own interface (libretro core options, a config file).
//!
//! The text form is `key = value` lines so a file can be read and written
//! without a serialization dependency and edited by hand.

/// How the picture is fitted to the window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Aspect {
    /// 4:3, the console's standard picture.
    #[default]
    Standard,
    /// 16:9, for titles rendering anamorphic widescreen.
    Wide,
    /// Fill the window regardless of shape.
    Stretch,
}

/// Which CPU backend runs the guest.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CpuBackend {
    /// Hardware virtualization when the host offers it, else the translator.
    #[default]
    Auto,
    /// Hardware virtualization (KVM); booting fails if it is unavailable.
    Kvm,
    /// The x86-32 to host translator.
    Translator,
}

/// Every setting the core has.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// Rendering resolution as a multiple of the title's own, 1 to 4.
    pub render_scale: u8,
    /// Picture shape.
    pub aspect: Aspect,
    /// Tell titles the console is set to widescreen.
    pub widescreen: bool,
    /// FXAA post-process antialiasing.
    pub fxaa: bool,
    /// Forced anisotropic filtering: 0 (the title's own) or 2, 4, 8, 16.
    pub anisotropy: u8,
    /// Wait for the display's vertical sync when presenting.
    pub vsync: bool,
    /// Show the frame rate in a corner.
    pub show_fps: bool,
    /// The front end's window fills the screen. The core only keeps and
    /// reports it (the menu's row, the settings file); the front end that
    /// owns the window applies it.
    pub fullscreen: bool,
    /// Read the disc and hard disk as fast as the host can instead of at the
    /// console drives' speeds. Loading is several times shorter; the few
    /// titles known to need the console's timing keep it regardless.
    pub fast_disc: bool,
    /// Output volume, 0 to 100.
    pub volume: u8,
    /// CPU backend choice; takes effect at the next boot.
    pub cpu: CpuBackend,
    /// Digital buttons ([`crate::pad::button`] bits) that, held together on
    /// one pad, open the menu; 0 (the default) for no combo.
    ///
    /// A front end with a spare button or key opens the menu itself (the
    /// standalone app: the pad's Guide button and F1). The combo is for one
    /// that has none, such as a libretro core. A settings file written when
    /// both stick clicks (`0x00c0`) were the default still names them, and
    /// that is honoured as the user's choice: the file cannot tell a default
    /// that was saved from one that was chosen.
    pub menu_combo: u16,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            render_scale: 1,
            aspect: Aspect::Standard,
            widescreen: false,
            fxaa: false,
            anisotropy: 0,
            vsync: true,
            show_fps: false,
            fullscreen: false,
            fast_disc: true,
            volume: 100,
            cpu: CpuBackend::Auto,
            menu_combo: 0,
        }
    }
}

impl Settings {
    /// Parses `key = value` lines over the defaults. Unknown keys, malformed
    /// lines and out-of-range values are ignored, so an old or hand-edited
    /// file never prevents a boot.
    #[must_use]
    pub fn from_text(text: &str) -> Self {
        let mut s = Self::default();
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            let Some((key, value)) = line.split_once('=') else { continue };
            let (key, value) = (key.trim(), value.trim());
            let flag = || matches!(value, "1" | "true" | "on" | "yes");
            match key {
                "render_scale" => {
                    if let Ok(v @ 1..=4) = value.parse() {
                        s.render_scale = v;
                    }
                }
                "aspect" => {
                    s.aspect = match value {
                        "wide" => Aspect::Wide,
                        "stretch" => Aspect::Stretch,
                        _ => Aspect::Standard,
                    }
                }
                "widescreen" => s.widescreen = flag(),
                "fxaa" => s.fxaa = flag(),
                "anisotropy" => {
                    if let Ok(v @ (0 | 2 | 4 | 8 | 16)) = value.parse() {
                        s.anisotropy = v;
                    }
                }
                "vsync" => s.vsync = flag(),
                "show_fps" => s.show_fps = flag(),
                "fullscreen" => s.fullscreen = flag(),
                "fast_disc" => s.fast_disc = flag(),
                "volume" => {
                    if let Ok(v @ 0..=100) = value.parse() {
                        s.volume = v;
                    }
                }
                "cpu" => {
                    s.cpu = match value {
                        "kvm" => CpuBackend::Kvm,
                        "translator" => CpuBackend::Translator,
                        _ => CpuBackend::Auto,
                    }
                }
                "menu_combo" => {
                    let hex = value.trim_start_matches("0x");
                    if let Ok(v) = u16::from_str_radix(hex, 16) {
                        s.menu_combo = v;
                    }
                }
                _ => {}
            }
        }
        s
    }

    /// The text form [`Settings::from_text`] reads back.
    #[must_use]
    pub fn to_text(&self) -> String {
        let aspect = match self.aspect {
            Aspect::Standard => "standard",
            Aspect::Wide => "wide",
            Aspect::Stretch => "stretch",
        };
        let cpu = match self.cpu {
            CpuBackend::Auto => "auto",
            CpuBackend::Kvm => "kvm",
            CpuBackend::Translator => "translator",
        };
        format!(
            "render_scale = {}\naspect = {aspect}\nwidescreen = {}\nfxaa = {}\nanisotropy = {}\n\
             vsync = {}\nshow_fps = {}\nfullscreen = {}\nfast_disc = {}\nvolume = {}\ncpu = {cpu}\nmenu_combo = 0x{:04x}\n",
            self.render_scale,
            self.widescreen,
            self.fxaa,
            self.anisotropy,
            self.vsync,
            self.show_fps,
            self.fullscreen,
            self.fast_disc,
            self.volume,
            self.menu_combo,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pad::button;

    #[test]
    fn text_form_round_trips_every_field() {
        let s = Settings {
            render_scale: 3,
            aspect: Aspect::Wide,
            widescreen: true,
            fxaa: true,
            anisotropy: 8,
            vsync: false,
            show_fps: true,
            fullscreen: true,
            fast_disc: false,
            volume: 40,
            cpu: CpuBackend::Translator,
            menu_combo: button::BACK | button::START,
        };
        assert_eq!(Settings::from_text(&s.to_text()), s);
    }

    #[test]
    fn no_combo_is_the_default_and_zero_round_trips() {
        assert_eq!(Settings::default().menu_combo, 0);
        assert_eq!(Settings::from_text("").menu_combo, 0);
        // An explicit zero turns off a combo that came before it.
        assert_eq!(Settings::from_text("menu_combo = 0x00c0\nmenu_combo = 0x0000\n").menu_combo, 0);
        let none = Settings { menu_combo: 0, ..Settings::default() };
        assert_eq!(Settings::from_text(&none.to_text()), none);
    }

    #[test]
    fn a_combo_stored_by_an_older_version_is_kept() {
        let s = Settings::from_text("menu_combo = 0x00c0\n");
        assert_eq!(s.menu_combo, button::LEFT_THUMB | button::RIGHT_THUMB);
        assert_eq!(Settings::from_text(&s.to_text()), s);
        // Not a number: the default.
        assert_eq!(Settings::from_text("menu_combo = sticks\n").menu_combo, 0);
    }

    #[test]
    fn bad_values_fall_back_to_defaults() {
        let s = Settings::from_text("render_scale = 9\nanisotropy = 3\nnonsense\nvolume = 400\n");
        assert_eq!(s, Settings::default());
    }
}
