//! The emulation thread: boots the core, calls [`Core::frame`] in a loop
//! and shuts the core down. Everything that touches the core or the
//! gamepads happens here; see `main.rs` for the threading design.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use winit::event_loop::EventLoopProxy;
use xb_core::{AudioSink, Core, CoreConfig, CoreState, CpuBackend, PORTS, SessionInfo};

use crate::input;
use crate::pads::PadHub;
use crate::surface::WindowSurface;
use crate::window::{Shared, Wake};

/// What the emulation thread is started with.
pub struct Session {
    /// The disc image.
    pub disc: PathBuf,
    /// The data directory.
    pub data_dir: PathBuf,
    /// `--cpu`: the CPU backend for this run only.
    pub cpu: Option<CpuBackend>,
    /// `--screenshot-dir`: where F12's screenshots go.
    pub screenshot_dir: PathBuf,
    /// `--screenshot-at`: a screenshot when this many frames were presented.
    pub screenshot_at: Option<u64>,
    /// The window's surface provider.
    pub surface: Arc<WindowSurface>,
    /// Where the core's audio goes.
    pub audio: Arc<dyn AudioSink>,
    /// Stop after this many presented frames.
    pub frame_limit: Option<u64>,
    /// `--fullscreen`: turn the fullscreen setting on for this and later runs.
    pub fullscreen: bool,
    /// `--state`: the savestate to start from, a slot's number or a path.
    pub state: Option<String>,
    /// The state shared with the event loop.
    pub shared: Arc<Shared>,
    /// Wakes the event loop when the session ends.
    pub wake: EventLoopProxy<Wake>,
}

/// How a session ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The core did not boot; the text says why.
    BootFailed(String),
    /// The window was closed, the menu's Quit was chosen, the title ended
    /// or the frame limit was reached.
    Finished,
    /// The menu's Reset was chosen: start again in a new process.
    Reset,
    /// The title cannot continue; the text says why.
    Fatal(String),
}

/// Wakes the event loop when dropped, so the main thread leaves its loop
/// however the emulation thread ends (a panic included).
struct WakeOnDrop(EventLoopProxy<Wake>);

impl Drop for WakeOnDrop {
    fn drop(&mut self) {
        // The loop may already be gone; then nobody needs waking.
        let _ = self.0.send_event(());
    }
}

/// The longest and the summed time of one of the loop's steps since the
/// last report.
#[derive(Default)]
struct Step {
    worst: Duration,
    total: Duration,
    calls: u32,
}

impl Step {
    fn add(&mut self, took: Duration) {
        self.worst = self.worst.max(took);
        self.total += took;
        self.calls += 1;
    }
}

/// Says every five seconds when the gamepads cost the loop time.
struct SlowLog {
    poll: Step,
    rumble: Step,
    since: Instant,
}

impl Default for SlowLog {
    fn default() -> Self {
        Self { poll: Step::default(), rumble: Step::default(), since: Instant::now() }
    }
}

impl SlowLog {
    fn report(&mut self) {
        let over = self.since.elapsed();
        if over < Duration::from_secs(5) {
            return;
        }
        let (poll, rumble) = (std::mem::take(&mut self.poll), std::mem::take(&mut self.rumble));
        self.since = Instant::now();
        let worst = poll.worst.max(rumble.worst);
        let level =
            if worst >= Duration::from_millis(2) { log::Level::Info } else { log::Level::Debug };
        log::log!(
            target: "app",
            level,
            "gamepads: polling took {:.1} ms a second (worst call {:.1} ms), rumble {:.1} ms a second (worst {:.1} ms, {} calls)",
            poll.total.as_secs_f64() * 1e3 / over.as_secs_f64(),
            poll.worst.as_secs_f64() * 1e3,
            rumble.total.as_secs_f64() * 1e3 / over.as_secs_f64(),
            rumble.worst.as_secs_f64() * 1e3,
            rumble.calls
        );
    }
}

/// The window's title for a session.
fn window_title(info: &SessionInfo) -> String {
    if info.title.is_empty() {
        "xbemu".to_owned()
    } else {
        format!("{} - {:.0} FPS - xbemu", info.title, info.fps)
    }
}

/// What asked for the menu to be toggled, for the log.
fn menu_toggle_source(key: bool, guide: bool) -> &'static str {
    match (key, guide) {
        (true, true) => "the keyboard (F1 or Escape) and a gamepad's Guide button",
        (true, false) => "the keyboard (F1 or Escape)",
        _ => "a gamepad's Guide button",
    }
}

/// The file name of a screenshot: the title's id, the frame it was taken at
/// and a count that keeps two shots of one frame apart.
fn screenshot_name(info: &SessionInfo, frame: u64, shot: u32) -> String {
    format!("xbemu-{:08x}-{frame:06}-{shot}.jpg", info.title_id)
}

/// Runs a session on the calling thread, from boot to shutdown. `booted`
/// is told once whether the boot succeeded, before the first frame.
pub fn run(session: Session, booted: &Sender<Result<(), String>>) -> Outcome {
    let _wake = WakeOnDrop(session.wake);
    let shared = session.shared;
    let mut hub = PadHub::new();

    log::info!(target: "app", "booting {}", session.disc.display());
    let core = match Core::boot(CoreConfig {
        disc: session.disc,
        data_dir: session.data_dir,
        settings: None,
        cpu_override: session.cpu,
        surface: session.surface.clone(),
        audio: session.audio,
        pads_connected: hub.connected_mask(),
    }) {
        Ok(core) => core,
        Err(e) => {
            let _ = booted.send(Err(e.0.clone()));
            return Outcome::BootFailed(e.0);
        }
    };
    let _ = booted.send(Ok(()));
    let info = core.info();
    log::info!(
        target: "app",
        "running '{}' ({:08X}) on the {} CPU backend",
        info.title, info.title_id, info.cpu_backend
    );

    let stopping = || shared.stop.load(Ordering::Relaxed);
    // The surface is made for the window, which the event loop creates.
    while session.surface.window().is_none() && !stopping() {
        std::thread::sleep(Duration::from_millis(1));
    }

    if session.fullscreen && !core.settings().fullscreen {
        core.set_settings(xb_core::Settings { fullscreen: true, ..core.settings() });
    }
    if let Some(state) = &session.state {
        // Straight into a savestate: the title has booted and not yet run.
        let loaded = match state.parse::<usize>() {
            Ok(slot) => core.load_slot(slot),
            Err(_) => std::fs::read(state)
                .map_err(|e| format!("{state}: {e}"))
                .and_then(|bytes| core.load_state(&bytes)),
        };
        match loaded {
            Ok(()) => log::info!(target: "app", "started from the savestate {state}"),
            Err(why) => log::warn!(target: "app", "--state {state}: {why}; starting from boot"),
        }
    }
    // What the window was last set to; the setting can change in the menu.
    let mut fullscreen: Option<bool> = None;

    let mut outcome = Outcome::Finished;
    // The keyboard's Enter and Backspace are the menu's while it is open.
    let mut menu_keys = input::MenuKeys::default();
    let mut frames = 0u64;
    let mut shots = 0u32;
    let mut title_at = Instant::now();
    // Where this loop's time goes besides the core: a slow gamepad poll or
    // rumble write holds every frame back.
    let mut slow = SlowLog::default();
    while !stopping() && session.frame_limit.is_none_or(|limit| frames < limit) {
        let t_poll = Instant::now();
        let polled = hub.poll();
        slow.poll.add(t_poll.elapsed());
        for change in polled.changes {
            // Port 1 keeps its pad: the keyboard still drives it.
            if change.port != 0 {
                core.set_pad_connected(change.port, change.connected);
            }
        }
        let by_key = shared.menu_toggle.swap(false, Ordering::Relaxed);
        if by_key || polled.menu_toggle {
            // The menu closes itself too (Resume, B, Start): ask, don't remember.
            let open = !core.menu_open();
            log::debug!(
                target: "app",
                "menu: {} by {}",
                if open { "opening" } else { "closing" },
                menu_toggle_source(by_key, polled.menu_toggle)
            );
            core.set_menu_open(open);
        }
        if shared.screenshot.swap(false, Ordering::Relaxed)
            || session.screenshot_at.is_some_and(|at| at == frames && shots == 0)
        {
            shots += 1;
            let path = session.screenshot_dir.join(screenshot_name(&info, frames, shots));
            if !core.screenshot(&path) {
                log::warn!(target: "app", "no screenshot at {}", path.display());
            }
        }
        if shared.surface_changed.swap(false, Ordering::Relaxed) {
            core.surface_changed();
        }
        // Savestates: failures are told on screen and in the core's log.
        let step = shared.slot_step.swap(0, Ordering::Relaxed);
        if step != 0 {
            let slots = xb_core::STATE_SLOTS as i32;
            let slot = (core.state_slot() as i32 + step).rem_euclid(slots);
            core.set_state_slot(slot as usize);
        }
        if shared.state_save.swap(false, Ordering::Relaxed) {
            let _ = core.save_slot(core.state_slot());
        }
        if shared.state_load.swap(false, Ordering::Relaxed) {
            let _ = core.load_slot(core.state_slot());
        }
        if shared.fullscreen_toggle.swap(false, Ordering::Relaxed) {
            let settings = core.settings();
            core.set_settings(xb_core::Settings { fullscreen: !settings.fullscreen, ..settings });
        }
        let want = core.settings().fullscreen;
        if fullscreen != Some(want)
            && let Some(window) = session.surface.window()
        {
            log::info!(target: "app", "window: fullscreen {}", if want { "on" } else { "off" });
            crate::window::set_fullscreen(window.as_ref(), want);
            fullscreen = Some(want);
        }

        let keyboard = menu_keys.pad(shared.keys.load(Ordering::Relaxed), core.menu_open());
        let pads = hub.pads(&keyboard);
        let frame = core.frame(
            shared.width.load(Ordering::Relaxed),
            shared.height.load(Ordering::Relaxed),
            &pads,
        );
        match frame.state {
            CoreState::Running => {}
            CoreState::QuitRequested => {
                log::info!(target: "app", "quit chosen in the menu");
                break;
            }
            CoreState::Exited => {
                log::info!(target: "app", "the title ended");
                break;
            }
            CoreState::ResetRequested => {
                outcome = Outcome::Reset;
                break;
            }
            CoreState::Fatal(why) => {
                outcome = Outcome::Fatal(why);
                break;
            }
        }
        if frame.presented {
            frames += 1;
        } else {
            std::thread::sleep(Duration::from_millis(2));
        }

        let t_rumble = Instant::now();
        for port in 0..PORTS {
            hub.set_rumble(port, core.rumble(port));
        }
        slow.rumble.add(t_rumble.elapsed());
        slow.report();
        if title_at.elapsed() >= Duration::from_secs(1) {
            title_at = Instant::now();
            if let Some(window) = session.surface.window() {
                window.set_title(&window_title(&core.info()));
            }
        }
    }

    log::info!(target: "app", "shutting down after {frames} presented frame(s)");
    // Before the window can go: the core's presenter draws into it.
    core.shutdown();
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_title_shows_the_game_and_its_frame_rate() {
        let info = SessionInfo { title: "Halo".into(), fps: 29.97, ..SessionInfo::default() };
        assert_eq!(window_title(&info), "Halo - 30 FPS - xbemu");
        assert_eq!(window_title(&SessionInfo::default()), "xbemu");
    }

    #[test]
    fn screenshots_are_named_by_title_frame_and_count() {
        let info = SessionInfo { title_id: 0x4D53_0004, ..SessionInfo::default() };
        assert_eq!(screenshot_name(&info, 120, 2), "xbemu-4d530004-000120-2.jpg");
    }
}
