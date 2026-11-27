//! `--selftest`: the window, the input polling and the audio stream without
//! a core, for a few seconds. A developer's check that the front end works
//! on a host before (or without) a bootable machine behind it.
//!
//! The window stays blank (on Wayland it may not even be shown: nothing is
//! ever drawn into it without the core's Vulkan device). A quiet tone is
//! played through the same ring and device callback the core's audio takes,
//! and changes of the four ports' pad state are logged.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use winit::event_loop::EventLoop;
use xb_core::{AudioSink, Channels, DrainSignal, PORTS, PadState, RingSink, StreamSpec};

use crate::audio::{self, CORE_RATE};
use crate::input;
use crate::pads::PadHub;
use crate::surface::WindowSurface;
use crate::window::{App, Shared, Wake};

/// How long the self-test runs.
const DURATION: Duration = Duration::from_secs(5);
/// How long the tone lasts.
const TONE: Duration = Duration::from_millis(1500);
/// Sample frames in one push, as the core pushes them.
const BLOCK_FRAMES: usize = 256;

/// Plays a quiet 440 Hz tone into `sink` the way the core's APU would: a
/// block at a time, waiting while the ring holds enough.
fn play_tone(sink: &RingSink, shared: &Shared) {
    let spec = StreamSpec {
        sample_rate: CORE_RATE,
        channels: Channels::Stereo,
        block_frames: BLOCK_FRAMES,
        capacity_frames: 48 * BLOCK_FRAMES,
        period_frames: 2048,
    };
    if let Err(e) = sink.open(&spec, DrainSignal::none()) {
        log::error!(target: "app", "selftest: the ring refused the stream: {e}");
        return;
    }
    let total = (TONE.as_secs_f64() * f64::from(CORE_RATE)) as usize;
    let mut block = [0i16; BLOCK_FRAMES * 2];
    let mut frame = 0usize;
    while frame < total && !shared.stop.load(Ordering::Relaxed) {
        if sink.queued_frames() >= 16 * BLOCK_FRAMES {
            std::thread::sleep(Duration::from_millis(2));
            continue;
        }
        for pair in block.as_chunks_mut::<2>().0 {
            let t = frame as f64 / f64::from(CORE_RATE);
            // A twentieth of full scale, faded out over the last tenth.
            let fade = (total.saturating_sub(frame) as f64 / (total as f64 * 0.1)).min(1.0);
            let s = ((t * 440.0 * std::f64::consts::TAU).sin() * 1638.0 * fade) as i16;
            pair.fill(s);
            frame += 1;
        }
        sink.push(&block);
    }
}

/// Polls the pads as the emulation thread does and logs every change.
fn watch_pads(shared: &Shared) {
    let mut hub = PadHub::new();
    log::info!(target: "app", "selftest: ports with a pad: {:04b}", hub.connected_mask());
    let mut last = [PadState::default(); PORTS];
    let mut polls = 0u64;
    while !shared.stop.load(Ordering::Relaxed) {
        if hub.poll().menu_toggle {
            log::info!(target: "app", "selftest: a Guide button was pressed (it toggles the menu)");
        }
        let pads = hub.pads(&input::keyboard_pad(shared.keys.load(Ordering::Relaxed)));
        for (port, (now, before)) in pads.iter().zip(&last).enumerate() {
            if now != before {
                log::info!(target: "app", "selftest: port {}: {now:?}", port + 1);
            }
        }
        last = pads;
        polls += 1;
        std::thread::sleep(Duration::from_millis(8));
    }
    log::info!(target: "app", "selftest: pads polled {polls} times");
}

/// Runs the self-test and returns the process's exit status: 0 if the
/// window was created (audio and gamepads are optional on a host, so their
/// absence is logged, not failed).
pub fn run(event_loop: EventLoop<Wake>, surface: Arc<WindowSurface>, fullscreen: bool) -> u8 {
    let shared = Arc::new(Shared::default());
    let audio = match audio::open() {
        Ok(opened) => Some(opened),
        Err(e) => {
            log::error!(target: "app", "selftest: no audio: {e}");
            None
        }
    };

    let mut app =
        App::new(shared.clone(), surface.clone(), fullscreen, Some(Instant::now() + DURATION));
    let result = std::thread::scope(|scope| {
        scope.spawn(|| watch_pads(&shared));
        if let Some((sink, _)) = &audio {
            scope.spawn(|| play_tone(sink, &shared));
        }
        let result = event_loop.run_app(&mut app);
        shared.stop.store(true, Ordering::Relaxed);
        result
    });

    if let Some((_, output)) = &audio {
        let stats = output.source.stats();
        log::info!(
            target: "app",
            "selftest: audio: {} callbacks took {} frames ({} ran short), {} still queued",
            stats.callbacks, stats.consumed_frames, stats.short_callbacks, stats.queued_frames
        );
    }
    let size = (shared.width.load(Ordering::Relaxed), shared.height.load(Ordering::Relaxed));
    match (result, &app.error, surface.window()) {
        (Ok(()), None, Some(_)) => {
            log::info!(target: "app", "selftest: window {}x{}; done", size.0, size.1);
            0
        }
        (result, error, _) => {
            log::error!(target: "app", "selftest: the window failed: {result:?} {error:?}");
            1
        }
    }
}
