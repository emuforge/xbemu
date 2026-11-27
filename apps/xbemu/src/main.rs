//! Standalone xbemu front end: a window, gamepads, audio and a command
//! line around `xb-core`. All settings live in the core's in-game menu.
//!
//! # Threads
//!
//! * The **main thread** owns the `winit` event loop and the window. It does
//!   no emulation: it records the window's size and the keys held in
//!   atomics ([`window::Shared`]) and handles the app's hotkeys. It also
//!   owns the audio stream.
//! * The **emulation thread** ([`session::run`]) creates the [`xb_core::Core`],
//!   calls `Core::frame` in a loop and shuts the core down. Every core call
//!   is made there, so the core never crosses a thread, and it is the thread
//!   the core creates the window's surface on. It also owns the gamepads
//!   (`gilrs`), polled right before each frame so input is as fresh as it
//!   can be, and forwards rumble.
//! * The **audio device's thread** (`cpal`'s) drains the ring the core's
//!   audio goes into ([`audio::Converter`]). It never blocks.
//!
//! Requests go from the event loop to the emulation thread as flags it
//! reads once a frame (menu, surface, stop). The emulation thread tells the
//! event loop that the session is over through an `EventLoopProxy`.
//!
//! # Start and shutdown order
//!
//! The emulation thread boots the core before the event loop starts, so a
//! boot failure is reported without a window ever opening. The core makes
//! its surface when it first presents, by which time the window exists.
//!
//! The core must be shut down before the window goes. However a session
//! ends (the window is closed, the menu's Quit, a fatal guest condition,
//! `--frames`), the event loop only *leaves*; the main thread then sets the
//! stop flag and joins the emulation thread, which calls `Core::shutdown`
//! as its last act. The window is still alive throughout: the surface
//! provider holds it, and the core holds the provider. Only after the join
//! is the audio stream closed and the process ended.
//!
//! # Exit status
//!
//! 0 a normal end, 1 the core did not boot, 2 a usage error, 3 a fatal
//! guest condition.
#![deny(unsafe_code)]

mod args;
mod audio;
mod input;
mod logger;
mod pads;
mod picker;
mod platform;
mod selftest;
mod session;
mod surface;
mod window;

use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use winit::event_loop::EventLoop;
use winit::raw_window_handle::{HasDisplayHandle, RawDisplayHandle};
use xb_core::{AudioSink, NullSink};

use crate::args::{Command, Options};
use crate::session::{Outcome, Session};
use crate::surface::WindowSurface;
use crate::window::{App, Shared, Wake};

/// The core did not boot, or the host lacks something the app needs.
const EXIT_BOOT: u8 = 1;
/// The command line was wrong.
const EXIT_USAGE: u8 = 2;
/// The title hit a fatal condition.
const EXIT_FATAL: u8 = 3;

/// Runs the program and returns its exit status.
fn run(options: Options) -> u8 {
    let event_loop = match EventLoop::<Wake>::with_user_event().build() {
        Ok(event_loop) => event_loop,
        Err(e) => {
            eprintln!("xbemu: cannot reach a display: {e}");
            return EXIT_BOOT;
        }
    };
    let display = match event_loop.display_handle() {
        Ok(display) => display.as_raw(),
        Err(e) => {
            eprintln!("xbemu: cannot reach a display: {e}");
            return EXIT_BOOT;
        }
    };
    let system = match display {
        RawDisplayHandle::Wayland(_) => "Wayland",
        RawDisplayHandle::Xlib(_) | RawDisplayHandle::Xcb(_) => "X11",
        _ => "another window system",
    };
    log::info!(target: "app", "display: {system}");
    let surface = match WindowSurface::new(display) {
        Ok(surface) => Arc::new(surface),
        Err(e) => {
            eprintln!("xbemu: {e}");
            return EXIT_BOOT;
        }
    };

    if options.selftest {
        return selftest::run(event_loop, surface, options.fullscreen);
    }
    let Some(disc) = options.disc else { return EXIT_USAGE };
    let Some(data_dir) = options.data_dir.or_else(platform::default_data_dir) else {
        eprintln!("xbemu: no home directory to keep data in; use --data-dir");
        return EXIT_BOOT;
    };
    log::info!(target: "app", "data directory: {}", data_dir.display());
    let screenshot_dir = options.screenshot_dir.unwrap_or_else(|| data_dir.join("screenshots"));

    // Without a device the core still needs its audio taken at the right
    // pace; the null sink does that.
    let (sink, audio): (Arc<dyn AudioSink>, _) = match audio::open() {
        Ok((sink, output)) => (Arc::new(sink), Some(output)),
        Err(e) => {
            log::warn!(target: "app", "no audio: {e}");
            (Arc::new(NullSink::default()), None)
        }
    };

    let shared = Arc::new(Shared::default());
    let session = Session {
        disc,
        cpu: options.cpu,
        screenshot_dir,
        screenshot_at: options.screenshot_at,
        data_dir,
        surface: surface.clone(),
        audio: sink,
        frame_limit: options.frames,
        fullscreen: options.fullscreen,
        state: options.state.clone(),
        shared: shared.clone(),
        wake: event_loop.create_proxy(),
    };
    let (booted_tx, booted) = std::sync::mpsc::channel();
    let emulation = std::thread::Builder::new()
        .name("emulation".into())
        .spawn(move || session::run(session, &booted_tx));
    let emulation = match emulation {
        Ok(thread) => thread,
        Err(e) => {
            eprintln!("xbemu: cannot start the emulation thread: {e}");
            return EXIT_BOOT;
        }
    };

    let mut app = App::new(shared.clone(), surface, options.fullscreen, None);
    // An `Err` here is the boot failing (or the thread dying before it said
    // so); either way the join below has the reason.
    if booted.recv() == Ok(Ok(()))
        && let Err(e) = event_loop.run_app(&mut app)
    {
        log::error!(target: "app", "the event loop failed: {e}");
    }

    // The core goes first, the window (held by `app` and the surface
    // provider) and the audio stream after it.
    shared.stop.store(true, Ordering::Relaxed);
    let outcome = emulation.join();
    if let Some(output) = &audio {
        let stats = output.source.stats();
        log::info!(
            target: "app",
            "audio: the device took {} frames in {} callbacks (about {} each), {} ran short, {} blocks dropped; it plays a callback's audio {} ms later, with {} frames queued before that",
            stats.consumed_frames,
            stats.callbacks,
            stats.consumed_frames / stats.callbacks.max(1),
            stats.short_callbacks,
            stats.overflows,
            output.device_latency_us.load(Ordering::Relaxed) / 1000,
            stats.queued_frames,
        );
    }
    drop(audio);

    match outcome {
        Ok(Outcome::BootFailed(why)) => {
            eprintln!("xbemu: cannot boot: {why}");
            EXIT_BOOT
        }
        Ok(Outcome::Fatal(why)) => {
            eprintln!("xbemu: fatal: {why}");
            EXIT_FATAL
        }
        Ok(Outcome::Reset) => {
            log::info!(target: "app", "reset: starting again");
            match platform::restart() {
                Ok(status) => u8::try_from(status).unwrap_or(EXIT_BOOT),
                Err(e) => {
                    eprintln!("xbemu: cannot start again: {e}");
                    EXIT_BOOT
                }
            }
        }
        Ok(Outcome::Finished) => match app.error {
            Some(why) => {
                eprintln!("xbemu: {why}");
                EXIT_BOOT
            }
            None => 0,
        },
        Err(_) => {
            eprintln!("xbemu: fatal: the emulation thread panicked");
            EXIT_FATAL
        }
    }
}

fn main() -> ExitCode {
    let mut command_line: Vec<_> = std::env::args_os().skip(1).collect();
    // Started with nothing (from the Finder or the Dock): ask for a disc
    // where the host has a picker.
    if command_line.is_empty() {
        match picker::choose_disc() {
            picker::Choice::Disc(path) => command_line.push(path.into_os_string()),
            picker::Choice::Cancelled => return ExitCode::SUCCESS,
            picker::Choice::Unavailable => {}
        }
    }
    let options = match args::parse(command_line) {
        Ok(Command::Run(options)) => options,
        Ok(Command::Help) => {
            print!("{}", args::USAGE);
            return ExitCode::SUCCESS;
        }
        Ok(Command::Version) => {
            println!("{}", args::version());
            return ExitCode::SUCCESS;
        }
        Err(why) => {
            eprint!("xbemu: {why}\n\n{}", args::USAGE);
            return ExitCode::from(EXIT_USAGE);
        }
    };
    logger::init(options.log);
    ExitCode::from(run(options))
}
