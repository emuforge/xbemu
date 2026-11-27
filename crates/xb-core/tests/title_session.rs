//! The whole machine with a real title, when the titles stash has `halo-ce`
//! (`titles/README.md`): booted through the public API with no window (the surface
//! provider has none to give), run until the title is flipping frames, paused
//! and resumed through the menu, the menu opened by a combo only when one
//! is set, and shut down.
//!
//! One test only: a process can create the renderer once.

use std::ffi::CStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use xb_core::pad::button;
use xb_core::{
    Core, CoreConfig, CoreState, NullSink, PORTS, PadState, Settings, SurfaceProvider, VkInstance,
    VkSurfaceKHR,
};

/// A front end without a window.
struct NoWindow;

impl SurfaceProvider for NoWindow {
    fn instance_extensions(&self) -> Vec<&'static CStr> {
        Vec::new()
    }

    unsafe fn create_surface(&self, _instance: VkInstance) -> Option<VkSurfaceKHR> {
        None
    }
}

/// Calls `Core::frame` at a display's pace for `time`, or until `done`.
fn frames(core: &Core, time: Duration, mut done: impl FnMut(&Core) -> bool) -> bool {
    let pads = [PadState::default(); PORTS];
    let end = Instant::now() + time;
    while Instant::now() < end {
        let frame = core.frame(640, 480, &pads);
        assert_eq!(frame.state, CoreState::Running, "the title stopped");
        if done(core) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(8));
    }
    false
}

#[test]
fn halo_boots_flips_frames_pauses_under_the_menu_and_shuts_down() {
    let Some(image) = xb_testkit::title("halo-ce") else { return };
    let data_dir = std::env::temp_dir().join(format!("xb-core-halo-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data_dir);

    let core = Core::boot(CoreConfig {
        disc: image,
        data_dir: data_dir.clone(),
        settings: Some(Settings::default()),
        cpu_override: None,
        surface: Arc::new(NoWindow),
        audio: Arc::new(NullSink::default()),
        pads_connected: 1,
    })
    .expect("the title boots");
    let info = core.info();
    println!("booted '{}' ({:08x}) on {}", info.title, info.title_id, info.cpu_backend);
    assert_eq!(info.title_id, 0x4D53_0004, "titles/halo-ce is not Halo: Combat Evolved");

    // The title's Direct3D comes up and flips at the video mode's rate.
    let flipping = frames(&core, Duration::from_secs(30), |core| core.info().fps >= 15.0);
    println!("frame rate {:.1}", core.info().fps);
    assert!(flipping, "the title never reached a steady frame rate");

    // The menu pauses the guest: nothing is flipped while it is open.
    core.set_menu_open(true);
    assert!(core.menu_open());
    frames(&core, Duration::from_secs(3), |_| false);
    assert_eq!(core.info().fps, 0.0, "frames were flipped while the menu was open");
    core.set_menu_open(false);
    assert!(!core.menu_open());
    let again = frames(&core, Duration::from_secs(10), |core| core.info().fps >= 15.0);
    assert!(again, "the title did not go on after the menu closed");

    // No combo is set by default: the stick clicks are the title's alone.
    let clicks = button::LEFT_THUMB | button::RIGHT_THUMB;
    let mut held = [PadState::default(); PORTS];
    held[0].buttons = clicks;
    let hold = |core: &Core, pads: &[PadState; PORTS]| {
        let end = Instant::now() + Duration::from_millis(1500);
        while Instant::now() < end && !core.menu_open() {
            core.frame(640, 480, pads);
            std::thread::sleep(Duration::from_millis(8));
        }
        core.menu_open()
    };
    assert_eq!(core.settings().menu_combo, 0);
    assert!(!hold(&core, &held), "the stick clicks opened the menu with no combo set");
    frames(&core, Duration::from_millis(100), |_| false);
    // A front end without a button of its own sets one, and it then works.
    core.set_settings(Settings { menu_combo: clicks, ..core.settings() });
    assert!(hold(&core, &held), "the combo did not open the menu");
    // Still held as the menu comes up, then let go: the menu stays.
    assert!(hold(&core, &held));
    frames(&core, Duration::from_millis(300), |_| false);
    assert!(core.menu_open(), "the menu closed by itself");
    core.set_menu_open(false);

    // A changed setting is saved where the next boot reads it.
    let changed = Settings { volume: 35, ..core.settings() };
    core.set_settings(changed.clone());
    let saved = std::fs::read_to_string(data_dir.join("settings.txt")).expect("a settings file");
    assert_eq!(Settings::from_text(&saved), changed);
    // The console's hard disk and EEPROM are in the data directory.
    assert!(data_dir.join("eeprom.bin").is_file());
    assert!(data_dir.join("hdd").is_dir());

    core.shutdown();
    let _ = std::fs::remove_dir_all(&data_dir);
}
