//! The machine: boots a title from a disc image and runs it. This is the one
//! API a front end uses; the standalone app and a libretro core are both thin
//! shells over [`Core`].
//!
//! A front end supplies four things: a way to create the Vulkan surface
//! ([`SurfaceProvider`]), an audio sink ([`AudioSink`]; [`ring_sink`] gives a
//! ready-made one to drain from an audio callback), controller state each
//! frame, and a data directory. Everything else, the in-game menu included,
//! is the core's.
//!
//! ```text
//! let core = Core::boot(config)?;
//! loop {                                  // the front end's render thread
//!     let frame = core.frame(width, height, &pads);
//!     match frame.state { CoreState::Running => {}, _ => break }
//!     if !frame.presented { sleep a few ms }
//! }
//! core.shutdown();
//! ```
//!
//! Inside, [`Core::boot`] starts the guest-CPU thread, which builds the
//! machine (`guest`): the CPU backend, the kernel, the GPU, the APU and the
//! register-level devices, joined by the bus and the platform (`bus`) and by
//! the state all threads share (`machine`). [`Core::frame`] is the front
//! end's side: the menu, the pads, the overlay, presenting, pause and
//! settings. `README.md` has the invariants and the order of boot, pause and
//! shutdown.
//!
//! `unsafe` is allowed in this crate: it hands the RAM mapping to the
//! vendored C behind `xb-gpu` and `xb-apu` and reads two host clocks.

mod bus;
mod guest;
mod host;
mod machine;
mod pacing;
mod script;
mod slots;
mod state;

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use xb_devices::PadPorts;
use xb_gpu::{Gpu, OverlayImage};
use xb_menu::{Canvas, Capabilities, Menu, Outcome, Picture, Setting, SlotRow, Status, draw_hud};

pub use slots::{STATE_SLOTS, SlotInfo, Thumbnail, age_text, thumbnail};
pub use state::{StateInfo, info as state_info};
pub use xb_apu::{
    AudioSink, Channels, DrainSignal, NullSink, RingSink, RingSource, StreamSpec, ring_sink,
};
pub use xb_gpu::{DrawStats, SurfaceProvider, VkInstance, VkSurfaceKHR};
/// Where the renderer looks for the Vulkan library on macOS; a front end
/// that creates the window's surface itself looks in the same order.
#[cfg(target_os = "macos")]
pub use xb_gpu::vulkan_libraries;
pub use xb_types::pad::{self, PORTS, PadState, Rumble};
pub use xb_types::settings::{Aspect, CpuBackend, Settings};

use machine::{Phase, Shared, lock};
use script::{Event, Script};

/// The environment variable holding a scripted session (see `script.rs` and
/// `docs/development.md`).
const SCRIPT_ENV: &str = "XBEMU_SCRIPT";

/// How long a pause request waits for the guest CPU to stop.
const PAUSE_WAIT: Duration = Duration::from_secs(2);

/// How often the statistics line is logged.
const STATS_EVERY: Duration = Duration::from_secs(5);

/// What a front end gives the core to boot a title.
pub struct CoreConfig {
    /// The disc image to boot (`.cci`, `.iso`).
    pub disc: PathBuf,
    /// Where the core keeps the console's hard disk (`hdd/`, with the EEPROM
    /// beside it), shader caches (`cache/`) and the settings file
    /// (`settings.txt`). Created if missing.
    pub data_dir: PathBuf,
    /// Settings to start with; `None` loads `data_dir/settings.txt` (or the
    /// defaults when there is none). The core writes that file whenever the
    /// menu changes a setting.
    pub settings: Option<Settings>,
    /// A CPU backend for this run only, whatever the settings say. It is not
    /// saved: the settings file keeps the user's own choice.
    pub cpu_override: Option<CpuBackend>,
    /// Creates the window's Vulkan surface.
    pub surface: Arc<dyn SurfaceProvider>,
    /// Receives the console's audio.
    pub audio: Arc<dyn AudioSink>,
    /// Which controller ports have a pad plugged in at boot (bit n: port n).
    pub pads_connected: u8,
}

/// Why a boot failed, in words for the user.
#[derive(Debug)]
pub struct BootError(pub String);

impl std::fmt::Display for BootError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BootError {}

/// What the machine is doing, as of the last [`Core::frame`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CoreState {
    /// The title is running (or paused under the menu).
    Running,
    /// The user chose Quit in the menu: the front end should shut down.
    QuitRequested,
    /// The user chose Reset in the menu: the front end should shut the core
    /// down and start the same disc again in a new process (the renderer
    /// cannot be created twice in one process).
    ResetRequested,
    /// The title ended on its own (returned to the dashboard, rebooted).
    Exited,
    /// The title cannot continue; the text says why.
    Fatal(String),
}

/// The result of one [`Core::frame`] call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    /// A picture was drawn to the surface. When `false` nothing was ready and
    /// the caller should wait a few milliseconds before calling again.
    pub presented: bool,
    /// The machine's state.
    pub state: CoreState,
}

/// Facts about the running session, for a window title or a log line.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionInfo {
    /// The title's name from its executable.
    pub title: String,
    /// The title's id.
    pub title_id: u32,
    /// The CPU backend in use (`"kvm"`, `"x64"`).
    pub cpu_backend: String,
    /// Frames a second the title is presenting.
    pub fps: f32,
}

/// What the overlay texture holds now.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Overlay {
    None,
    Menu,
    Hud(String),
}

/// Counter values at the last statistics line.
#[derive(Clone, Copy, Default)]
struct StatsMark {
    traps: u64,
    exits: u64,
    mmio: u64,
    faults: u64,
    raised: u64,
    flips: u64,
    presents: u64,
    audio_frames: u64,
    first_writes: u64,
    stale: u64,
    apu_acks: u64,
    mailbox_waits: u64,
    usb: u64,
    guest_cpu_ns: u64,
    draws: DrawStats,
    uneven: u64,
}

/// The front end thread's state: the menu, the overlay and the settings.
struct Ui {
    menu: Menu,
    /// The settings as saved (the CPU choice is the user's, not a run's
    /// override).
    settings: Settings,
    paused: bool,
    /// A Quit, Reset or fatal condition, once seen: reported from then on.
    ended: Option<CoreState>,
    overlay: Overlay,
    overlay_size: (u32, u32),
    pixels: Vec<u8>,
    last_frame: Instant,
    first_frame: Option<Instant>,
    script: Script,
    shots: Vec<PathBuf>,
    /// The savestate slot the front end's keys save to and load from.
    slot: usize,
    /// A slot's picture to take once the menu is off the screen.
    slot_shot: Option<PathBuf>,
    /// A line to show for a moment: its text, and from when until when.
    toast: Option<(String, Instant, Instant)>,
    /// The slots page: the slot whose picture is loaded, and the picture
    /// (`None`: it has none).
    slot_picture: Option<(usize, Option<Thumbnail>)>,
    /// When the menu's slot list was last read from the folder.
    slots_at: Option<Instant>,
    /// Whether the frame-latency hold is taken, judged each second from
    /// the flips and vblanks since `pace_at`.
    pace: pacing::HoldPolicy,
    pace_at: Instant,
    pace_mark: (u64, u64),
    stats_at: Instant,
    stats: StatsMark,
    presents: u64,
    shown: bool,
    /// The title's frame rate when it last ran.
    fps: f32,
}

/// A booted console.
pub struct Core {
    shared: Arc<Shared>,
    pads: PadPorts,
    data_dir: PathBuf,
    title: String,
    title_id: u32,
    cpu_backend: &'static str,
    /// The widescreen flag the console booted with (the setting then).
    console_widescreen: bool,
    guest: Mutex<Option<JoinHandle<()>>>,
    ui: Mutex<Ui>,
}

/// The slot a script's `save=` or `load=` names when it is a number.
fn slot_of(path: &Path) -> Option<usize> {
    path.to_str()?.parse().ok().filter(|&slot| slot < STATE_SLOTS)
}

/// The settings file in a data directory.
fn settings_path(data_dir: &Path) -> PathBuf {
    data_dir.join("settings.txt")
}

/// The saved settings, or the defaults when there is no (readable) file.
fn load_settings(data_dir: &Path) -> Settings {
    match std::fs::read_to_string(settings_path(data_dir)) {
        Ok(text) => Settings::from_text(&text),
        Err(_) => Settings::default(),
    }
}

fn save_settings(data_dir: &Path, settings: &Settings) {
    let path = settings_path(data_dir);
    match std::fs::write(&path, settings.to_text()) {
        Ok(()) => log::info!(target: "core", "settings saved to {}", path.display()),
        Err(e) => log::warn!(target: "core", "settings not saved to {}: {e}", path.display()),
    }
}

/// Which settings the machine cannot change while a title runs. The CPU
/// backend is chosen at boot and a title reads the console's widescreen flag
/// when it starts; the render scale reaches the renderer at once but takes
/// effect when the title next creates or resets its Direct3D device, which
/// for most titles is the next boot.
fn capabilities() -> Capabilities {
    Capabilities::all_live().next_boot(Setting::Cpu).next_boot(Setting::Widescreen)
}

/// What the menu says of the picture the title draws: its own shape, as its
/// Direct3D presentation flags gave it, against the widescreen flag the
/// console had when it booted. No title says in advance whether it has a
/// 16:9 mode; one that was told the console is widescreen and still draws
/// 4:3 has none (or keeps its own option for it).
const fn picture_note(title_wide: Option<bool>, console_wide: bool) -> &'static str {
    match (title_wide, console_wide) {
        (Some(true), _) => "draws 16:9",
        (Some(false), true) => "draws 4:3, ignores widescreen",
        (Some(false), false) => "draws 4:3",
        (None, _) => "",
    }
}

/// The overlay's size for a window: whole multiples of the menu's logical
/// 320x240 that fit the window's height (at least 640x480), as wide as the
/// picture's shape under `aspect`, so the menu's pixels stay square.
fn overlay_size(width: u32, height: u32, aspect: Aspect) -> (u32, u32) {
    let scale = (height / xb_menu::LOGICAL_HEIGHT).clamp(2, 8);
    let h = xb_menu::LOGICAL_HEIGHT * scale;
    let w = match aspect {
        Aspect::Standard => h * 4 / 3,
        Aspect::Wide => h * 16 / 9,
        Aspect::Stretch if height != 0 => {
            (u64::from(h) * u64::from(width) / u64::from(height)).clamp(320, 8192) as u32
        }
        Aspect::Stretch => h * 4 / 3,
    };
    (w.max(xb_menu::LOGICAL_WIDTH * scale), h)
}

impl Core {
    /// Boots the disc image: opens it, loads the title, chooses the CPU
    /// backend, starts the devices and the guest. Returns once the guest is
    /// running; the picture arrives through [`Core::frame`].
    ///
    /// # Errors
    /// The image cannot be read, has no bootable executable, or the host
    /// lacks something the core needs (a Vulkan device, a CPU backend).
    pub fn boot(config: CoreConfig) -> Result<Self, BootError> {
        let err = |what: &str, e: &dyn std::fmt::Display| BootError(format!("{what}: {e}"));
        // The machine's threads ask the host for their class as they start;
        // this one presents the picture (`Core::frame` runs on it).
        host::install();
        xb_types::thread::pacing();
        let data_dir = config.data_dir;
        let hdd_root = data_dir.join("hdd");
        let cache_dir = data_dir.join("cache");
        for dir in [&hdd_root, &cache_dir] {
            std::fs::create_dir_all(dir)
                .map_err(|e| err(&format!("cannot create {}", dir.display()), &e))?;
        }
        log::info!(target: "core", "data directory {}", data_dir.display());

        let settings = config.settings.unwrap_or_else(|| load_settings(&data_dir));
        let mut boot_settings = settings.clone();
        if let Some(cpu) = config.cpu_override {
            log::info!(target: "core", "cpu: {cpu:?} for this run (the saved choice is {:?})", settings.cpu);
            boot_settings.cpu = cpu;
        }

        let disc = xb_formats::disc::open(&config.disc)
            .map_err(|e| err(&format!("cannot open {}", config.disc.display()), &e))?;
        log::info!(target: "core", "disc {} ({:?})", config.disc.display(), disc.kind);
        let ram_size = xb_kernel::ram_size_for(Some(&disc.source));
        let ram = xb_mem::Ram::new(ram_size).map_err(|e| err("cannot create guest RAM", &e))?;

        let pads = PadPorts::new(config.pads_connected);
        let build = guest::Build {
            disc: disc.source,
            ram,
            settings: boot_settings,
            hdd_root,
            cache_dir,
            surface: config.surface,
            audio: config.audio,
            pads: pads.clone(),
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("guest-cpu".into())
            .spawn(move || {
                xb_types::thread::pacing();
                guest::thread_main(build, &tx);
            })
            .map_err(|e| err("cannot start the guest CPU thread", &e))?;
        let booted = match rx.recv() {
            Ok(Ok(booted)) => booted,
            Ok(Err(why)) => {
                let _ = thread.join();
                log::error!(target: "core", "boot failed: {why}");
                return Err(BootError(why));
            }
            Err(_) => {
                let _ = thread.join();
                return Err(BootError("the guest CPU thread died while booting".into()));
            }
        };

        let (script, bad) = match std::env::var(SCRIPT_ENV) {
            Ok(text) => Script::parse(&text),
            Err(_) => (Script::default(), Vec::new()),
        };
        for step in bad {
            log::warn!(target: "core", "{SCRIPT_ENV}: step '{step}' is not understood");
        }
        if !script.is_empty() {
            log::info!(target: "core", "{SCRIPT_ENV}: a scripted session");
        }
        let now = Instant::now();
        Ok(Self {
            shared: booted.shared,
            pads,
            data_dir,
            title: booted.title,
            title_id: booted.title_id,
            cpu_backend: booted.cpu_backend,
            console_widescreen: settings.widescreen,
            guest: Mutex::new(Some(thread)),
            ui: Mutex::new(Ui {
                menu: Menu::new(settings.clone(), capabilities()),
                settings,
                paused: false,
                ended: None,
                overlay: Overlay::None,
                overlay_size: (0, 0),
                pixels: Vec::new(),
                last_frame: now,
                first_frame: None,
                script,
                shots: Vec::new(),
                slot: 0,
                slot_shot: None,
                toast: None,
                slot_picture: None,
                slots_at: None,
                pace: pacing::HoldPolicy::default(),
                pace_at: now,
                pace_mark: (0, 0),
                stats_at: now,
                stats: StatsMark::default(),
                presents: 0,
                shown: false,
                fps: 0.0,
            }),
        })
    }

    fn gpu(&self) -> Option<&Gpu> {
        self.shared.gpu.get()
    }

    /// Pauses the guest (the menu opened) or lets it go on.
    fn set_paused(&self, ui: &mut Ui, paused: bool) {
        if ui.paused == paused {
            return;
        }
        ui.paused = paused;
        if paused {
            // The guest first: it may be waiting for the renderer, which
            // must keep working until the CPU has stopped.
            let stopped = {
                let mut run = lock(&self.shared.run);
                run.want_pause = true;
                self.shared.control.request_pause();
                let deadline = Instant::now() + PAUSE_WAIT;
                while run.phase == Phase::Running {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        break;
                    }
                    run = self
                        .shared
                        .run_cv
                        .wait_timeout(run, left)
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .0;
                }
                run.phase != Phase::Running
            };
            if !stopped {
                log::warn!(target: "core", "pause: the guest CPU did not stop within {PAUSE_WAIT:?}");
            }
            if let Some(gpu) = self.gpu() {
                gpu.set_paused(true);
            }
            if let Some(apu) = self.shared.apu()
                && !apu.pause()
            {
                log::warn!(target: "core", "pause: the APU's frame thread did not park");
            }
            log::info!(target: "core", "paused (guest time {:.3} s)", self.shared.clock.now_ns() as f64 / 1e9);
        } else {
            let stood = self.shared.clock.now_ns();
            if let Some(apu) = self.shared.apu() {
                apu.resume();
            }
            if let Some(gpu) = self.gpu() {
                gpu.set_paused(false);
            }
            log::debug!(target: "core", "resume: guest time stood at {:.3} s", stood as f64 / 1e9);
            lock(&self.shared.run).want_pause = false;
            self.shared.run_cv.notify_all();
            log::info!(target: "core", "resumed (guest time {:.3} s)", self.shared.clock.now_ns() as f64 / 1e9);
        }
    }

    /// Makes `new` the settings in effect: applied where they can be while
    /// the title runs, and saved.
    fn apply_settings(&self, ui: &mut Ui, new: Settings) {
        let old = std::mem::replace(&mut ui.settings, new.clone());
        if old == new {
            return;
        }
        log::info!(target: "core", "settings changed: {}", describe_change(&old, &new));
        if let Some(gpu) = self.gpu() {
            gpu.apply_settings(&new);
            // The frame rate is the core's overlay, not the presenter's.
            gpu.set_show_fps(false);
        }
        if old.render_scale != new.render_scale {
            // The renderer makes everything again from guest memory, which
            // the guest must not be writing meanwhile.
            let was_paused = ui.paused;
            self.set_paused(ui, true);
            let done = self.gpu().is_some_and(Gpu::rescale);
            log::info!(
                target: "core",
                "render scale {}x: {}",
                new.render_scale,
                if done { "applied" } else { "the renderer takes it when it next makes its device" }
            );
            self.set_paused(ui, was_paused);
        }
        if old.fast_disc != new.fast_disc {
            if guest::disc_speed_forced() {
                log::info!(target: "core", "disc speed: XBEMU_DISC_SPEED is set and stays");
            } else {
                self.shared.control.set_device_timing(guest::disc_speed(new.fast_disc));
            }
        }
        if old.volume != new.volume
            && let Some(apu) = self.shared.apu()
        {
            apu.set_volume(f32::from(new.volume) / 100.0);
        }
        save_settings(&self.data_dir, &new);
    }

    /// Brings the overlay texture up to date with the menu and the HUD.
    fn update_overlay(&self, ui: &mut Ui, width: u32, height: u32, fps: f32) {
        let Some(gpu) = self.gpu() else { return };
        let presenter = gpu.presenter();
        let size = overlay_size(width, height, ui.settings.aspect);
        let want = if ui.menu.is_open() {
            Overlay::Menu
        } else if let Some((text, _, _)) =
            ui.toast.as_ref().filter(|(_, from, until)| (*from..*until).contains(&Instant::now()))
        {
            Overlay::Hud(text.clone())
        } else if ui.settings.show_fps {
            Overlay::Hud(format!("{fps:.0} FPS"))
        } else {
            Overlay::None
        };
        let resized = size != ui.overlay_size;
        let redraw = match &want {
            Overlay::None => false,
            Overlay::Menu => ui.menu.is_dirty() || resized || ui.overlay != Overlay::Menu,
            Overlay::Hud(_) => resized || ui.overlay != want,
        };
        if redraw {
            ui.overlay_size = size;
            ui.pixels.resize(size.0 as usize * size.1 as usize * 4, 0);
            let Some(mut canvas) = Canvas::new(&mut ui.pixels, size.0, size.1) else { return };
            match &want {
                Overlay::Menu => {
                    let picture = ui
                        .slot_picture
                        .as_ref()
                        .and_then(|(_, p)| p.as_ref())
                        .map(|t| Picture { width: t.width, height: t.height, rgba: &t.rgba });
                    ui.menu.render_with(&mut canvas, picture);
                }
                Overlay::Hud(text) => draw_hud(&mut canvas, text),
                Overlay::None => {}
            }
            presenter.set_overlay(Some(OverlayImage {
                width: size.0,
                height: size.1,
                rgba: &ui.pixels,
            }));
        } else if want == Overlay::None && ui.overlay != Overlay::None {
            presenter.set_overlay(None);
        }
        ui.overlay = want;
    }

    /// The statistics line, every few seconds.
    fn log_stats(&self, ui: &mut Ui, now: Instant) {
        let dt = now.duration_since(ui.stats_at);
        if dt < STATS_EVERY {
            return;
        }
        ui.stats_at = now;
        let c = &self.shared.counters;
        let tracker = self.shared.tracker.stats();
        let gpu = self.gpu().map(|g| g.stats()).unwrap_or_default();
        let mark = StatsMark {
            traps: self.shared.cpu.traps.load(Ordering::Relaxed),
            exits: self.shared.cpu.exits.load(Ordering::Relaxed),
            mmio: c.mmio.load(Ordering::Relaxed),
            faults: c.faults.load(Ordering::Relaxed),
            raised: c.raised.load(Ordering::Relaxed),
            flips: gpu.flips,
            draws: gpu.draws,
            uneven: gpu.frame.uneven,
            presents: ui.presents,
            audio_frames: self.shared.apu().map_or(0, |a| a.frames_pushed()),
            first_writes: tracker.first_writes,
            stale: tracker.stale_accesses,
            apu_acks: c.apu_acks.load(Ordering::Relaxed),
            mailbox_waits: c.mailbox_waits.load(Ordering::Relaxed),
            usb: c.usb.load(Ordering::Relaxed),
            guest_cpu_ns: machine::guest_thread_cpu_ns(),
        };
        let last = std::mem::replace(&mut ui.stats, mark);
        let s = dt.as_secs_f64();
        // (saturating: a savestate loaded since the last line puts the
        // title's own counts back)
        let per = |now: u64, then: u64| now.saturating_sub(then) as f64 / s;
        let holds = self.shared.clock.take_hold_stats();
        let draws = mark.draws.draws.saturating_sub(last.draws.draws);
        let of_draws =
            |now: u64, then: u64| now.saturating_sub(then) as f64 * 100.0 / draws.max(1) as f64;
        log::info!(
            target: "core",
            "per s: {:.1} flips, {:.1} presents, {:.0} traps, {:.0} cpu exits, {:.0} mmio, {:.0} page faults ({:.0} first writes, {:.0} stale, {:.0} the title's), {:.0} audio frames, {:.0} APU interrupt acks, {:.0} mailbox waits, {:.0} USB register reads | holds {} (longest {:.1} ms, {:.1} ms left out) | guest CPU thread {:.0}% of a host core | {:.0} uneven frames, worst gap {:.0} ms | {:.0} draws ({:.0} a flip, {:.0}% sharing constants, {:.0}% joined into {:.0} batches) | irq pending {:04x}, guest time {:.1} s{}",
            per(mark.flips, last.flips),
            per(mark.presents, last.presents),
            per(mark.traps, last.traps),
            per(mark.exits, last.exits),
            per(mark.mmio, last.mmio),
            per(mark.faults, last.faults),
            per(mark.first_writes, last.first_writes),
            per(mark.stale, last.stale),
            per(mark.raised, last.raised),
            per(mark.audio_frames, last.audio_frames),
            per(mark.apu_acks, last.apu_acks),
            per(mark.mailbox_waits, last.mailbox_waits),
            per(mark.usb, last.usb),
            holds.holds,
            holds.longest_ns as f64 / 1e6,
            holds.left_out_ns as f64 / 1e6,
            mark.guest_cpu_ns.saturating_sub(last.guest_cpu_ns) as f64 / (s * 1e7),
            per(mark.uneven, last.uneven),
            gpu.frame.worst_frame_ms,
            draws as f64 / s,
            draws as f64 / mark.flips.saturating_sub(last.flips).max(1) as f64,
            of_draws(mark.draws.shared_constants, last.draws.shared_constants),
            of_draws(mark.draws.joined, last.draws.joined),
            per(mark.draws.batches, last.draws.batches),
            self.shared.hub.pending(),
            self.shared.clock.now_ns() as f64 / 1e9,
            if ui.paused { ", paused" } else { "" },
        );
    }

    /// The machine's state for the front end; a fatal condition is logged in
    /// full the first time it is seen.
    fn state(&self, ui: &mut Ui) -> CoreState {
        if let Some(ended) = &ui.ended {
            return ended.clone();
        }
        let ended = match lock(&self.shared.run).phase.clone() {
            Phase::Fatal(why) => Some(CoreState::Fatal(why)),
            Phase::Exited => Some(CoreState::Exited),
            Phase::Running | Phase::Paused | Phase::Stopped => None,
        };
        let ended = ended.or_else(|| {
            let fatal = self.gpu()?.fatal()?;
            log::error!(target: "core", "the GPU cannot continue: {fatal}");
            Some(CoreState::Fatal(fatal.to_string()))
        });
        if let Some(state) = &ended {
            if let CoreState::Fatal(why) = state {
                log::error!(target: "core", "fatal: {why}");
            }
            ui.ended = ended.clone();
        }
        ended.unwrap_or(CoreState::Running)
    }

    /// Call once per display frame on the thread that owns the window's
    /// surface, with the window's size in pixels and the state of the four
    /// controller ports. Feeds input to the title, runs the in-game menu
    /// (which may pause the title and hide input from it), and presents the
    /// picture with the menu over it. Blocks for about one frame at most.
    pub fn frame(&self, width: u32, height: u32, pads: &[PadState; PORTS]) -> Frame {
        let ui = &mut *lock(&self.ui);
        let now = Instant::now();
        let dt = now.duration_since(ui.last_frame);
        ui.last_frame = now;
        let first = *ui.first_frame.get_or_insert(now);

        let mut pads = *pads;
        let events = ui.script.advance(now - first, ui.settings.menu_combo, &mut pads[0]);
        for event in events {
            match event {
                Event::Menu => {
                    let open = !ui.menu.is_open();
                    self.open_menu(ui, open);
                }
                Event::Shot(path) => {
                    self.take_screenshot(ui, &path);
                }
                Event::Plug(port, plugged) => {
                    log::info!(target: "core", "controller port {}: {} (script)", port + 1,
                        if plugged { "plugged in" } else { "unplugged" });
                    self.pads.set_connected(port, plugged);
                }
                // A number in place of a path is a slot, as a key saves it.
                Event::Save(path) if slot_of(&path).is_some() => {
                    let _ = self.save_slot_in(ui, slot_of(&path).unwrap_or(0));
                }
                Event::Load(path) if slot_of(&path).is_some() => {
                    let _ = self.load_slot_in(ui, slot_of(&path).unwrap_or(0));
                }
                Event::Save(path) => {
                    let (tx, rx) = std::sync::mpsc::channel();
                    // To a path: the state carries the disk's files itself.
                    let saved = self
                        .state_job(ui, state::StateJob::Save(None, tx), &rx)
                        .and_then(|s| std::fs::write(&path, s.bytes).map_err(|e| e.to_string()));
                    if let Err(why) = saved {
                        log::warn!(target: "core", "no state saved to {}: {why}", path.display());
                    }
                }
                Event::Load(path) => {
                    let (tx, rx) = std::sync::mpsc::channel();
                    let loaded = std::fs::read(&path)
                        .map_err(|e| e.to_string())
                        .and_then(|b| {
                            let store = Some(self.disk_store());
                            self.state_job(ui, state::StateJob::Load(b, store, tx), &rx)
                        })
                        .map(drop);
                    if let Err(why) = loaded {
                        log::warn!(target: "core", "no state loaded from {}: {why}", path.display());
                    }
                }
                Event::Quit => ui.ended = Some(CoreState::QuitRequested),
            }
        }

        // While the guest is paused nothing is flipped; the menu keeps showing
        // the rate the title ran at.
        if !ui.paused {
            ui.fps = self.gpu().map_or(0.0, |g| g.presenter().frame_stats().fps);
        }
        let fps = ui.fps;
        let title_wide = self.gpu().and_then(Gpu::title_wide);
        ui.menu.set_status(&Status {
            cpu_backend: self.cpu_backend,
            gpu: "Vulkan",
            title: &self.title,
            game: picture_note(title_wide, self.console_widescreen),
            fps,
        });
        match ui.menu.update(dt, &pads) {
            Outcome::None => {}
            Outcome::Opened => {
                log::info!(target: "core", "menu opened");
                self.set_paused(ui, true);
            }
            Outcome::Closed => {
                log::info!(target: "core", "menu closed");
                self.set_paused(ui, false);
            }
            Outcome::SettingsChanged(settings) => self.apply_settings(ui, settings),
            Outcome::SaveSlot(slot) => {
                // The menu has closed: the title goes on, and the picture
                // is the game's.
                self.set_paused(ui, false);
                ui.slot = slot;
                let _ = self.save_slot_in(ui, slot);
            }
            Outcome::LoadSlot(slot) => {
                self.set_paused(ui, false);
                ui.slot = slot;
                let _ = self.load_slot_in(ui, slot);
            }
            Outcome::ResetRequested => {
                log::info!(target: "core", "reset chosen in the menu");
                ui.ended = Some(CoreState::ResetRequested);
            }
            Outcome::QuitRequested => {
                log::info!(target: "core", "quit chosen in the menu");
                ui.ended = Some(CoreState::QuitRequested);
            }
        }
        for (port, pad) in pads.iter().enumerate() {
            self.pads.set_pad(port, &ui.menu.filter_pad(*pad));
        }

        self.update_slots_page(ui, Instant::now());
        self.update_overlay(ui, width, height, fps);
        let presented = self.gpu().is_some_and(|g| g.presenter().present(width, height));
        if presented {
            ui.presents += 1;
            if !ui.shown && self.gpu().is_some_and(|g| g.presenter().shown()) {
                ui.shown = true;
                log::info!(target: "core", "the title's first frame is on screen ({:.1} s after the first present call)",
                    (now - first).as_secs_f64());
            }
        }
        if !ui.menu.is_open()
            && ui.overlay != Overlay::Menu
            && let Some(picture) = ui.slot_shot.take()
        {
            self.take_screenshot(ui, &picture);
        }
        self.finish_screenshots(ui);
        self.update_pacing(ui, now);
        self.log_stats(ui, now);
        Frame { presented, state: self.state(ui) }
    }

    fn open_menu(&self, ui: &mut Ui, open: bool) {
        if ui.menu.is_open() == open {
            return;
        }
        if open {
            ui.menu.open();
            log::info!(target: "core", "menu opened (by the front end)");
        } else {
            ui.menu.close();
            log::info!(target: "core", "menu closed (by the front end)");
        }
        self.set_paused(ui, open);
    }

    fn take_screenshot(&self, ui: &mut Ui, path: &Path) -> bool {
        let Some(gpu) = self.gpu() else { return false };
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            let _ = std::fs::create_dir_all(dir);
        }
        if !gpu.presenter().screenshot(path) {
            log::warn!(target: "core", "screenshot: {} cannot be used as a path", path.display());
            return false;
        }
        ui.shots.push(path.to_owned());
        true
    }

    /// Logs how the screenshots asked for ended.
    fn finish_screenshots(&self, ui: &mut Ui) {
        let Some(gpu) = self.gpu() else { return };
        let presenter = gpu.presenter();
        ui.shots.retain(|path| match presenter.screenshot_result(path) {
            None => true,
            Some(Ok(())) => {
                log::info!(target: "core", "screenshot saved to {}", path.display());
                false
            }
            Some(Err(why)) => {
                log::warn!(target: "core", "screenshot {} failed: {why}", path.display());
                false
            }
        });
    }

    /// Tells the core a host controller for `port` (0 to 3) came or went.
    ///
    /// The console's controllers are the core's own, not the host's: one
    /// is plugged into a port the first time a host controller is there for
    /// it (or a savestate says it was), and stays. A host controller that
    /// goes away leaves the port's input at rest; the title does not see a
    /// controller pulled, and a savestate does not depend on which pads the
    /// host has. [`Core::unplug_pad`] pulls one on purpose.
    pub fn set_pad_connected(&self, port: usize, connected: bool) {
        if connected {
            if self.pads.connected() >> port & 1 == 0 {
                log::info!(target: "core", "controller port {}: plugged in", port + 1);
            }
            self.pads.set_connected(port, true);
        } else {
            log::info!(target: "core", "controller port {}: the host's controller is gone; the port keeps its controller, at rest", port + 1);
        }
    }

    /// Pulls the controller out of `port` (0 to 3), as a player leaving
    /// would. The title sees it go.
    pub fn unplug_pad(&self, port: usize) {
        log::info!(target: "core", "controller port {}: unplugged", port + 1);
        self.pads.set_connected(port, false);
    }

    /// The rumble motor speeds the title last set for `port`. Zero while the
    /// title is paused.
    #[must_use]
    pub fn rumble(&self, port: usize) -> Rumble {
        if lock(&self.ui).paused { Rumble::default() } else { self.pads.rumble(port) }
    }

    /// Opens or closes the in-game menu, for a front end's own key or
    /// button (the standalone app: F1 and a gamepad's Guide button). This is
    /// how the menu opens unless [`Settings::menu_combo`] names a combo.
    pub fn set_menu_open(&self, open: bool) {
        self.open_menu(&mut lock(&self.ui), open);
    }

    /// Whether the in-game menu is open (the pad closes it too, with Resume,
    /// B or Start, so a front end's hotkey asks before it toggles).
    #[must_use]
    pub fn menu_open(&self) -> bool {
        lock(&self.ui).menu.is_open()
    }

    /// Saves a picture of the window as the next [`Core::frame`] draws it
    /// (the title's frame with the overlay) to `path`: a JPEG if the name
    /// ends in `.jpg`, else a 32-bit BMP. The result is logged. Returns
    /// `false` if the request could not be made.
    pub fn screenshot(&self, path: &Path) -> bool {
        self.take_screenshot(&mut lock(&self.ui), path)
    }

    /// The window's surface was lost or replaced: the next frame creates the
    /// swapchain again through the [`SurfaceProvider`].
    pub fn surface_changed(&self) {
        if let Some(gpu) = self.gpu() {
            gpu.presenter().reset_surface();
        }
    }

    /// The settings in effect.
    #[must_use]
    pub fn settings(&self) -> Settings {
        lock(&self.ui).settings.clone()
    }

    /// Replaces the settings (a front end's own options interface). Applied
    /// at once where possible and saved.
    pub fn set_settings(&self, settings: Settings) {
        let ui = &mut *lock(&self.ui);
        ui.menu.set_settings(settings.clone());
        self.apply_settings(ui, settings);
    }

    /// Facts about the session.
    #[must_use]
    pub fn info(&self) -> SessionInfo {
        SessionInfo {
            title: self.title.clone(),
            title_id: self.title_id,
            cpu_backend: self.cpu_backend.to_owned(),
            fps: self.gpu().map_or(0.0, |g| g.presenter().frame_stats().fps),
        }
    }

    /// The whole machine as bytes: a savestate, which [`Core::load_state`]
    /// puts back while this title runs (in this session or a later one).
    /// The title is paused for it and goes on afterwards. The shape a
    /// libretro core's `retro_serialize` needs. The bytes stand alone: they
    /// hold the files of the title's cache partitions too, which can be
    /// hundreds of megabytes (a slot keeps those beside it instead,
    /// [`Core::save_slot`]).
    ///
    /// # Errors
    /// The title has ended, or part of the machine did not stop in time;
    /// nothing has changed then.
    pub fn save_state(&self) -> Result<Vec<u8>, String> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.state_job(&mut lock(&self.ui), state::StateJob::Save(None, tx), &rx).map(|s| s.bytes)
    }

    /// Puts a state of this title back ([`Core::save_state`]'s bytes, or a
    /// slot's file while the slots' store has its hard disk files). The
    /// title's cache partitions become the state's; its saved games stay as
    /// they are, and the log says when they differ from the state's.
    ///
    /// # Errors
    /// The bytes are not a state of the running title on this RAM size, are
    /// cut short, of a newer build or from before states held the hard
    /// disk, or name hard disk files that are neither in them nor in the
    /// store: nothing has changed. Or a section
    /// did not load, which leaves the machine unusable: the text says so,
    /// and [`Core::frame`] reports the title as it then is.
    pub fn load_state(&self, bytes: &[u8]) -> Result<(), String> {
        let (tx, rx) = std::sync::mpsc::channel();
        let job = state::StateJob::Load(bytes.to_vec(), Some(self.disk_store()), tx);
        self.state_job(&mut lock(&self.ui), job, &rx).map(drop)
    }

    /// Where the slots of the running title keep their hard disk files.
    fn disk_store(&self) -> PathBuf {
        slots::store(&self.data_dir, self.title_id)
    }

    /// The savestate slot [`Core::save_slot`] and [`Core::load_slot`] use
    /// when a front end's keys ask (0 to [`STATE_SLOTS`] - 1).
    #[must_use]
    pub fn state_slot(&self) -> usize {
        lock(&self.ui).slot
    }

    /// Chooses the slot, and says so on screen for a moment.
    pub fn set_state_slot(&self, slot: usize) {
        let ui = &mut *lock(&self.ui);
        ui.slot = slot.min(STATE_SLOTS - 1);
        let used = slots::list(&self.data_dir, self.title_id)[ui.slot].used;
        Self::toast(ui, format!("Slot {}{}", ui.slot, if used { "" } else { " (empty)" }), 0);
    }

    /// What each slot of the running title holds.
    #[must_use]
    pub fn state_slots(&self) -> Vec<SlotInfo> {
        slots::list(&self.data_dir, self.title_id)
    }

    /// Saves a state into `slot` and takes its picture at the next frame
    /// shown without the menu.
    ///
    /// # Errors
    /// As [`Core::save_state`], or the file could not be written.
    pub fn save_slot(&self, slot: usize) -> Result<(), String> {
        self.save_slot_in(&mut lock(&self.ui), slot)
    }

    /// Loads the state in `slot`.
    ///
    /// # Errors
    /// The slot is empty, or as [`Core::load_state`].
    pub fn load_slot(&self, slot: usize) -> Result<(), String> {
        self.load_slot_in(&mut lock(&self.ui), slot)
    }

    fn save_slot_in(&self, ui: &mut Ui, slot: usize) -> Result<(), String> {
        let slot = slot.min(STATE_SLOTS - 1);
        let (path, picture) = slots::paths(&self.data_dir, self.title_id, slot);
        let (tx, rx) = std::sync::mpsc::channel();
        let store = self.disk_store();
        let job = state::StateJob::Save(Some(store.clone()), tx);
        let saved = self.state_job(ui, job, &rx).and_then(|s| {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
            }
            std::fs::write(&path, s.bytes).map_err(|e| format!("{}: {e}", path.display()))?;
            slots::keep(&path, &store, &s.disk);
            Ok(())
        });
        match &saved {
            Ok(()) => {
                log::info!(target: "core", "state saved to slot {slot} ({})", path.display());
                ui.slot_shot = Some(picture);
                // After the picture: it must not be in it.
                Self::toast(ui, format!("Saved to slot {slot}"), 250);
            }
            Err(why) => {
                log::warn!(target: "core", "slot {slot} not saved: {why}");
                Self::toast(ui, format!("Slot {slot} not saved"), 0);
            }
        }
        saved
    }

    fn load_slot_in(&self, ui: &mut Ui, slot: usize) -> Result<(), String> {
        let slot = slot.min(STATE_SLOTS - 1);
        let (path, _) = slots::paths(&self.data_dir, self.title_id, slot);
        let loaded = match std::fs::read(&path) {
            Ok(bytes) => {
                let (tx, rx) = std::sync::mpsc::channel();
                let job = state::StateJob::Load(bytes, Some(self.disk_store()), tx);
                self.state_job(ui, job, &rx)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err("the slot is empty".into()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        };
        match &loaded {
            Ok(saves) => {
                log::info!(target: "core", "state loaded from slot {slot}");
                let note = if saves.is_empty() { "" } else { " (saved games differ)" };
                Self::toast(ui, format!("Loaded slot {slot}{note}"), 0);
            }
            Err(why) => {
                log::warn!(target: "core", "slot {slot} not loaded: {why}");
                Self::toast(ui, format!("Slot {slot}: {why}"), 0);
            }
        }
        loaded.map(drop)
    }

    /// Once a second: whether the title keeps its rate, and with that
    /// whether the frame-latency hold is taken (`pacing.rs`).
    fn update_pacing(&self, ui: &mut Ui, now: Instant) {
        let dt = now.duration_since(ui.pace_at).as_secs_f64();
        if dt < 1.0 {
            return;
        }
        let Some(stats) = self.gpu().map(Gpu::stats) else { return };
        let mark = (stats.flips, stats.vblanks);
        let last = std::mem::replace(&mut ui.pace_mark, mark);
        ui.pace_at = now;
        if ui.paused {
            return;
        }
        // (saturating: a savestate loaded puts the counts back)
        let per = |now: u64, then: u64| now.saturating_sub(then) as f64 / dt;
        let judged = ui.pace.second(per(mark.0, last.0), per(mark.1, last.1));
        // XBEMU_FRAME_HOLD=on|off (to compare): always, or never.
        let on = match std::env::var("XBEMU_FRAME_HOLD").as_deref() {
            Ok("on") => true,
            Ok("off") => false,
            _ => judged,
        };
        if on != self.shared.hold_wanted.swap(on, Ordering::Relaxed) {
            log::info!(
                target: "core",
                "frame-latency hold {}: {:.0} frames a second at {:.0} refreshes",
                if on {
                    "on (the title keeps its rate)"
                } else {
                    "off (the title is not keeping its rate with it)"
                },
                per(mark.0, last.0),
                per(mark.1, last.1)
            );
        }
    }

    /// Keeps the menu's slots page told: what each slot holds (read from
    /// the folder now and then while the page is up) and the highlighted
    /// slot's picture, scaled once when the highlight moves.
    fn update_slots_page(&self, ui: &mut Ui, now: Instant) {
        let Some(slot) = ui.menu.shown_slot() else {
            ui.slot_picture = None;
            ui.slots_at = None;
            return;
        };
        if ui.slots_at.is_none_or(|at| now.duration_since(at) >= Duration::from_secs(1)) {
            ui.slots_at = Some(now);
            let labels: Vec<(bool, String)> = slots::list(&self.data_dir, self.title_id)
                .iter()
                .map(|s| (s.used, age_text(s.age_secs)))
                .collect();
            let rows: Vec<SlotRow<'_>> =
                labels.iter().map(|(used, label)| SlotRow { used: *used, label }).collect();
            ui.menu.set_slots(&rows);
        }
        if ui.slot_picture.as_ref().is_none_or(|(of, _)| *of != slot) {
            let (_, picture) = slots::paths(&self.data_dir, self.title_id, slot);
            let thumb = std::fs::read(picture).ok().and_then(|bmp| thumbnail(&bmp, 480, 360));
            ui.slot_picture = Some((slot, thumb));
        }
    }

    /// Shows `text` for two seconds, starting `after_ms` from now.
    fn toast(ui: &mut Ui, text: String, after_ms: u64) {
        let from = Instant::now() + Duration::from_millis(after_ms);
        ui.toast = Some((text, from, from + Duration::from_secs(2)));
    }

    /// Hands the guest-CPU thread a savestate job at a pause and waits for
    /// its answer.
    fn state_job<T>(
        &self,
        ui: &mut Ui,
        job: state::StateJob,
        answer: &std::sync::mpsc::Receiver<Result<T, String>>,
    ) -> Result<T, String> {
        let was_paused = ui.paused;
        self.set_paused(ui, true);
        let posted = {
            let mut run = lock(&self.shared.run);
            if run.phase == Phase::Paused {
                run.state_job = Some(job);
                self.shared.run_cv.notify_all();
                true
            } else {
                false
            }
        };
        let result = if posted {
            answer
                .recv_timeout(Duration::from_secs(20))
                .unwrap_or_else(|_| Err("the guest CPU thread did not answer".into()))
        } else {
            Err("the title is not running".into())
        };
        self.set_paused(ui, was_paused);
        result
    }

    /// Counts of the title's draws since boot (all zero before the renderer
    /// runs): how many shared constants and were joined into batches.
    #[must_use]
    pub fn draw_stats(&self) -> DrawStats {
        self.gpu().map(|g| g.stats().draws).unwrap_or_default()
    }

    /// Stops the guest, saves the shader caches and settings, and releases
    /// the devices. Call before the window is destroyed.
    pub fn shutdown(self) {
        self.stop();
    }

    /// The shutdown, in an order in which nothing waits on a part that is
    /// already gone: the guest CPU first (it may be waiting for the renderer
    /// or the APU), then the APU's frame thread, then the renderer, then the
    /// link from write tracking to the renderer.
    fn stop(&self) {
        let Some(thread) = lock(&self.guest).take() else { return };
        log::info!(target: "core", "shutting down");
        {
            let mut run = lock(&self.shared.run);
            run.quit = true;
            run.want_pause = false;
        }
        self.shared.control.request_stop();
        self.shared.run_cv.notify_all();
        if thread.join().is_err() {
            log::error!(target: "core", "the guest CPU thread panicked");
        }
        if let Some(apu) = lock(&self.shared.apu).take() {
            if !apu.stop() {
                log::warn!(target: "core", "the APU's frame thread did not park");
            }
            log::info!(target: "core", "audio: {} frames went to the sink", apu.frames_pushed());
        }
        if let Some(gpu) = self.gpu() {
            gpu.presenter().set_overlay(None);
            let stats = gpu.stats();
            log::info!(target: "core", "video: {} frames rendered, {} flipped, {} presents, {} vblanks",
                stats.frames, stats.flips, stats.presents, stats.vblanks);
            gpu.shutdown();
        }
        let waits = self.shared.counters.frame_waits.load(Ordering::Relaxed);
        if waits != 0 {
            let held = self.shared.counters.frame_holds.load(Ordering::Relaxed);
            log::info!(target: "core", "the renderer asked {waits} times for a frame-latency hold; {held} were taken");
        }
        self.shared.tracker.set_events(None);
        save_settings(&self.data_dir, &lock(&self.ui).settings);
        log::info!(target: "core", "shut down");
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The fields that differ between two settings, for the log.
fn describe_change(old: &Settings, new: &Settings) -> String {
    let mut parts = Vec::new();
    let mut note = |name: &str, a: String, b: String| {
        if a != b {
            parts.push(format!("{name} {a} -> {b}"));
        }
    };
    note("render scale", old.render_scale.to_string(), new.render_scale.to_string());
    note("aspect", format!("{:?}", old.aspect), format!("{:?}", new.aspect));
    note("widescreen", old.widescreen.to_string(), new.widescreen.to_string());
    note("FXAA", old.fxaa.to_string(), new.fxaa.to_string());
    note("anisotropy", old.anisotropy.to_string(), new.anisotropy.to_string());
    note("vsync", old.vsync.to_string(), new.vsync.to_string());
    note("show FPS", old.show_fps.to_string(), new.show_fps.to_string());
    note("fullscreen", old.fullscreen.to_string(), new.fullscreen.to_string());
    note("fast disc", old.fast_disc.to_string(), new.fast_disc.to_string());
    note("volume", old.volume.to_string(), new.volume.to_string());
    note("cpu", format!("{:?}", old.cpu), format!("{:?}", new.cpu));
    note("menu combo", format!("{:04x}", old.menu_combo), format!("{:04x}", new.menu_combo));
    parts.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_menu_says_which_shape_the_title_draws_and_when_it_ignores_widescreen() {
        assert_eq!(picture_note(None, true), "");
        assert_eq!(picture_note(Some(true), true), "draws 16:9");
        assert_eq!(picture_note(Some(true), false), "draws 16:9");
        assert_eq!(picture_note(Some(false), false), "draws 4:3");
        assert_eq!(picture_note(Some(false), true), "draws 4:3, ignores widescreen");
    }

    #[test]
    fn the_overlay_is_a_whole_multiple_of_the_menu_in_the_picture_s_shape() {
        assert_eq!(overlay_size(640, 480, Aspect::Standard), (640, 480));
        assert_eq!(overlay_size(1920, 1080, Aspect::Standard), (1280, 960));
        assert_eq!(overlay_size(1920, 1080, Aspect::Wide), (1706, 960));
        assert_eq!(overlay_size(1920, 1080, Aspect::Stretch), (1706, 960));
        // A window smaller than the menu still gets a whole menu.
        assert_eq!(overlay_size(100, 50, Aspect::Standard), (640, 480));
        assert_eq!(overlay_size(0, 0, Aspect::Stretch), (640, 480));
    }

    #[test]
    fn saved_settings_come_back_and_a_missing_file_gives_the_defaults() {
        let dir = std::env::temp_dir().join(format!("xb-core-settings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(load_settings(&dir), Settings::default());
        std::fs::create_dir_all(&dir).expect("a temporary directory");
        let s = Settings { render_scale: 2, show_fps: true, volume: 30, ..Settings::default() };
        save_settings(&dir, &s);
        assert_eq!(load_settings(&dir), s);
        std::fs::remove_dir_all(&dir).expect("cleaning up");
    }

    #[test]
    fn a_change_is_described_by_the_fields_that_differ() {
        let old = Settings::default();
        let new = Settings { show_fps: true, volume: 50, ..Settings::default() };
        assert_eq!(describe_change(&old, &new), "show FPS false -> true, volume 100 -> 50");
    }
}
