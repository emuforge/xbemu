//! The cost of a title's draws, when the titles stash has `fuzion-frenzy`
//! (`titles/README.md`):
//! the title is booted with no window, Start is pressed now and then to
//! leave its intro, and once it has drawn enough, some of its draws (most
//! of Fuzion Frenzy's) must have shared the previous draw's constants block.
//!
//! Fuzion Frenzy forced this: it draws its meshes as some 300,000 short
//! strips a second with nothing changing between them, and each one got a
//! constants block of its own.
//!
//! The same boot then changes the render scale twice and checks the title
//! goes on drawing (a process can create the renderer once, so it is one
//! test).

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

/// Draws to wait for: a few seconds of a title's menu.
const ENOUGH_DRAWS: u64 = 300_000;

/// Fuzion Frenzy's title id.
const FUZION_FRENZY: u32 = 0x4D53_0002;

#[test]
fn consecutive_draws_with_unchanged_constants_share_one_block() {
    let Some(image) = xb_testkit::title("fuzion-frenzy") else { return };
    let data_dir = std::env::temp_dir().join(format!("xb-core-draws-{}", std::process::id()));
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

    // Start for 120 ms every two seconds: past the intro, into the menus.
    let begin = Instant::now();
    while begin.elapsed() < Duration::from_secs(90) && core.draw_stats().draws < ENOUGH_DRAWS {
        let mut pads = [PadState::default(); PORTS];
        if begin.elapsed().as_millis() % 2000 < 120 {
            pads[0].buttons = button::START;
        }
        let frame = core.frame(640, 480, &pads);
        assert_eq!(frame.state, CoreState::Running, "the title stopped");
        std::thread::sleep(Duration::from_millis(8));
    }
    let stats = core.draw_stats();
    println!(
        "{} draws in {:.1} s, {} sharing constants, {} joined into {} batches",
        stats.draws,
        begin.elapsed().as_secs_f64(),
        stats.shared_constants,
        stats.joined,
        stats.batches
    );

    // The render scale changed while the title runs: the renderer makes its
    // targets and textures again from guest memory, and the title draws on.
    for scale in [2, 1] {
        core.set_settings(Settings { render_scale: scale, ..core.settings() });
        let before = core.draw_stats().draws;
        let end = Instant::now() + Duration::from_secs(4);
        while Instant::now() < end {
            let frame = core.frame(640, 480, &[PadState::default(); PORTS]);
            assert_eq!(frame.state, CoreState::Running, "the title stopped at {scale}x");
            std::thread::sleep(Duration::from_millis(8));
        }
        let drawn = core.draw_stats().draws - before;
        println!("at {scale}x: {drawn} draws in 4 s, {:.1} frames a second", core.info().fps);
        assert!(drawn > 0, "nothing was drawn after the change to {scale}x");
        assert!(core.info().fps > 1.0, "no frame was shown after the change to {scale}x");
    }

    // A savestate: taken, the title run on, the state put back, twice. The
    // title must go on drawing each time, and what it is put back to is the
    // moment it was taken at (guest time, in the state's header).
    let state = core.save_state().expect("a state of the running title");
    let header = xb_core::state_info(&state).expect("its header");
    println!(
        "state: {} KiB of '{}' at guest time {:.1} s",
        state.len() >> 10,
        header.title,
        header.guest_ns as f64 / 1e9
    );
    assert_eq!(header.title_id, info.title_id);
    for round in 1..=2 {
        let run_on = |secs: u64| {
            let before = core.draw_stats().draws;
            let end = Instant::now() + Duration::from_secs(secs);
            while Instant::now() < end {
                let frame = core.frame(640, 480, &[PadState::default(); PORTS]);
                assert_eq!(frame.state, CoreState::Running, "the title stopped (round {round})");
                std::thread::sleep(Duration::from_millis(8));
            }
            core.draw_stats().draws - before
        };
        assert!(run_on(2) > 0);
        core.load_state(&state).expect("the state loads");
        let drawn = run_on(3);
        println!(
            "round {round}: {drawn} draws in the 3 s after the load, {:.1} fps",
            core.info().fps
        );
        assert!(drawn > 0, "nothing was drawn after the load");
        assert!(core.info().fps > 1.0, "no frame was shown after the load");
    }
    // A state cut short or of nothing is refused and changes nothing.
    assert!(core.load_state(&state[..state.len() / 2]).is_err());
    assert!(core.load_state(b"not a state").is_err());
    let frame = core.frame(640, 480, &[PadState::default(); PORTS]);
    assert_eq!(frame.state, CoreState::Running, "a refused state stopped the title");

    core.shutdown();
    let _ = std::fs::remove_dir_all(&data_dir);

    if stats.draws < ENOUGH_DRAWS {
        println!("note: the title drew too little in 90 s to judge; test skipped");
        return;
    }
    // No title sends something between every two draws that changes what
    // the block holds: with none shared, every draw got its own.
    assert!(
        stats.shared_constants > 0,
        "none of {} draws shared a constants block: each draw is paying for its own",
        stats.draws
    );
    // Fuzion Frenzy's strips change nothing between them. (Halo's draws
    // mostly load constants of their own: about 7% share.)
    if info.title_id == FUZION_FRENZY {
        assert!(
            stats.shared_constants * 2 > stats.draws,
            "{} of {} draws shared a constants block",
            stats.shared_constants,
            stats.draws
        );
    }
}
