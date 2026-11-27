//! A windowed smoke test of presentation: opens a window, starts the
//! renderer with a mock machine and no game, and runs the presenter with an
//! animated test overlay. Shows that the surface callback, the swapchain,
//! overlay compositing and resizing work. Not run by `cargo test`.
//!
//! ```text
//! cargo run -p xb-gpu --example window -- [--seconds N] [--resize] [--shot FILE.bmp]
//! ```
//!
//! `--seconds N` closes the window after N seconds; `--resize` changes the
//! window's size halfway through; `--shot` writes a picture of the window as
//! presented (after the resize, if any). The overlay is a translucent panel
//! with a red, green, blue and white corner (top left, top right, bottom
//! left, bottom right) and a bar that moves; behind it the picture's place is
//! black (no game is running).

#[path = "../tests/common/mod.rs"]
mod common;

use std::ffi::CStr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use ash::vk::{self, Handle};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use winit::window::{Window, WindowId};

use xb_gpu::{Gpu, GpuConfig, OverlayImage, Presenter, SurfaceProvider, VkInstance, VkSurfaceKHR};
use xb_types::irq::IrqHub;
use xb_types::settings::Settings;

/// The front end's side of the surface callback: the window, once made.
struct WindowSurface {
    extensions: Vec<&'static CStr>,
    window: OnceLock<Arc<Window>>,
}

impl SurfaceProvider for WindowSurface {
    fn instance_extensions(&self) -> Vec<&'static CStr> {
        self.extensions.clone()
    }

    unsafe fn create_surface(&self, instance: VkInstance) -> Option<VkSurfaceKHR> {
        let window = self.window.get()?;
        // SAFETY: loads the system's Vulkan loader, the same library the
        // renderer's instance came from.
        let entry = unsafe { ash::Entry::load() }.ok()?;
        // SAFETY: `instance` is the renderer's live instance (the trait's
        // contract); this only loads its function pointers.
        let instance = unsafe {
            ash::Instance::load(entry.static_fn(), vk::Instance::from_raw(instance as u64))
        };
        let display = window.display_handle().ok()?.as_raw();
        let handle = window.window_handle().ok()?.as_raw();
        // SAFETY: the handles are the live window's, and the instance was
        // created with the extensions `ash_window` asked for.
        let surface =
            unsafe { ash_window::create_surface(&entry, &instance, display, handle, None) };
        match surface {
            Ok(s) => Some(s.as_raw()),
            Err(e) => {
                eprintln!("window: no surface: {e}");
                None
            }
        }
    }
}

/// The test overlay, `W` by `H`: a translucent panel, a marker in each
/// corner and a bar at `phase` (0..W).
const W: u32 = 320;
const H: u32 = 240;

fn overlay_pixels(phase: u32) -> Vec<u8> {
    let mut px = vec![0u8; (W * H * 4) as usize];
    for y in 0..H {
        for x in 0..W {
            let corner = |left: bool, top: bool| {
                (if left { x < 24 } else { x >= W - 24 })
                    && (if top { y < 24 } else { y >= H - 24 })
            };
            let rgba: [u8; 4] = if corner(true, true) {
                [255, 0, 0, 255]
            } else if corner(false, true) {
                [0, 255, 0, 255]
            } else if corner(true, false) {
                [0, 0, 255, 255]
            } else if corner(false, false) {
                [255, 255, 255, 255]
            } else if x.abs_diff(phase) < 4 {
                [255, 200, 0, 255]
            } else if x < 2 || y < 2 || x >= W - 2 || y >= H - 2 {
                [255, 255, 255, 255]
            } else if (40..W - 40).contains(&x) && (60..H - 60).contains(&y) {
                [20, 40, 90, 160]
            } else {
                [0, 0, 0, 0]
            };
            let i = ((y * W + x) * 4) as usize;
            px[i..i + 4].copy_from_slice(&rgba);
        }
    }
    px
}

struct Options {
    seconds: Option<f32>,
    resize: bool,
    shot: Option<PathBuf>,
}

struct App {
    options: Options,
    gpu: Gpu,
    surface: Arc<WindowSurface>,
    size: Arc<(AtomicU32, AtomicU32)>,
    stop: Arc<AtomicBool>,
    presenting: Option<std::thread::JoinHandle<u64>>,
    started: Instant,
    resized: bool,
    shot_asked: bool,
}

/// The front end's presenting thread: presents until told to stop, updating
/// the overlay ten times a second. Returns how often `present` drew.
fn present_loop(
    presenter: Presenter,
    size: Arc<(AtomicU32, AtomicU32)>,
    stop: Arc<AtomicBool>,
) -> u64 {
    let started = Instant::now();
    let (mut drawn, mut step) = (0u64, u32::MAX);
    while !stop.load(Ordering::Relaxed) {
        let now = (started.elapsed().as_millis() / 100) as u32;
        if now != step {
            step = now;
            let px = overlay_pixels((step * 8) % W);
            presenter.set_overlay(Some(OverlayImage { width: W, height: H, rgba: &px }));
        }
        if presenter.present(size.0.load(Ordering::Relaxed), size.1.load(Ordering::Relaxed)) {
            drawn += 1;
        } else {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    drawn
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.presenting.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("xb-gpu presentation test")
            .with_inner_size(PhysicalSize::new(960, 540));
        let window = Arc::new(event_loop.create_window(attrs).expect("a window"));
        let s = window.inner_size();
        self.size.0.store(s.width, Ordering::Relaxed);
        self.size.1.store(s.height, Ordering::Relaxed);
        let _ = self.surface.window.set(window);
        let (presenter, size, stop) = (self.gpu.presenter(), self.size.clone(), self.stop.clone());
        self.presenting = Some(std::thread::spawn(move || present_loop(presenter, size, stop)));
        self.started = Instant::now();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(s) => {
                println!("window: resized to {}x{}", s.width, s.height);
                self.size.0.store(s.width, Ordering::Relaxed);
                self.size.1.store(s.height, Ordering::Relaxed);
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        event_loop
            .set_control_flow(ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(50)));
        let Some(total) = self.options.seconds else { return };
        let t = self.started.elapsed().as_secs_f32();
        if self.options.resize && !self.resized && t > total * 0.4 {
            self.resized = true;
            // Wayland applies the new size at once and returns it; elsewhere
            // a Resized event follows.
            if let Some(s) = self
                .surface
                .window
                .get()
                .and_then(|w| w.request_inner_size(PhysicalSize::new(1280, 600)))
            {
                println!("window: resized to {}x{} (applied at once)", s.width, s.height);
                self.size.0.store(s.width, Ordering::Relaxed);
                self.size.1.store(s.height, Ordering::Relaxed);
            }
        }
        if !self.shot_asked && t > total * 0.7 {
            self.shot_asked = true;
            if let Some(path) = &self.options.shot {
                self.gpu.presenter().screenshot(path);
            }
        }
        if t > total {
            event_loop.exit();
        }
    }
}

fn main() {
    common::init_log();
    let mut options = Options { seconds: None, resize: false, shot: None };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--seconds" => options.seconds = args.next().and_then(|v| v.parse().ok()),
            "--resize" => options.resize = true,
            "--shot" => options.shot = args.next().map(PathBuf::from),
            other => panic!("unknown argument {other}"),
        }
    }

    let event_loop = EventLoop::new().expect("an event loop");
    let display = event_loop.display_handle().expect("a display").as_raw();
    let names = ash_window::enumerate_required_extensions(display)
        .expect("the window system's surface extensions");
    // SAFETY: `ash_window` returns pointers to its own static extension names.
    let extensions = names.iter().map(|&p| unsafe { CStr::from_ptr(p) }).collect();
    let surface = Arc::new(WindowSurface { extensions, window: OnceLock::new() });

    let host = Arc::new(common::MockHost::new());
    let cache = std::env::temp_dir().join("xb-gpu-window-example");
    let config =
        GpuConfig { show_fps: true, ..GpuConfig::from_settings(&Settings::default(), cache) };
    // SAFETY: the mock's RAM is a leaked allocation of RAM_SIZE bytes.
    let gpu = unsafe {
        Gpu::new(
            host.clone(),
            host.ram(),
            common::RAM_SIZE,
            Arc::new(IrqHub::new()),
            config,
            Some(surface.clone()),
        )
    }
    .expect("the GPU");
    match gpu.title_start() {
        Ok(info) => {
            assert_eq!((info.title_id, info.timedate), (common::TITLE_ID, common::TIMEDATE));
            println!("window: renderer up, {} guest kick(s)", host.kicks.load(Ordering::Relaxed));
        }
        Err(fatal) => {
            eprintln!("window: the renderer did not start: {fatal}");
            return;
        }
    }

    let mut app = App {
        options,
        gpu,
        surface,
        size: Arc::new((AtomicU32::new(0), AtomicU32::new(0))),
        stop: Arc::new(AtomicBool::new(false)),
        presenting: None,
        started: Instant::now(),
        resized: false,
        shot_asked: false,
    };
    event_loop.run_app(&mut app).expect("the event loop");

    if let Some(path) = &app.options.shot {
        // The presenting thread is still running: give the picture a moment.
        let t0 = Instant::now();
        let result = loop {
            match app.gpu.presenter().screenshot_result(path) {
                Some(r) => break Some(r),
                None if t0.elapsed() > Duration::from_secs(2) => break None,
                None => std::thread::sleep(Duration::from_millis(10)),
            }
        };
        println!("window: screenshot {}: {result:?}", path.display());
    }
    app.stop.store(true, Ordering::Relaxed);
    let drawn = app.presenting.take().map(|t| t.join().expect("the presenting thread"));
    let stats = app.gpu.stats();
    println!(
        "window: present() drew {drawn:?} times; the presenter counts {} refreshes; fatal: {:?}",
        stats.presents,
        app.gpu.fatal()
    );
    app.gpu.shutdown();
}
