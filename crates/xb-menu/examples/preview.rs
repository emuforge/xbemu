//! Renders the menu in a few states, composited over a stand-in game frame,
//! to PPM files for a human to look at.
//!
//! ```text
//! cargo run -p xb-menu --example preview -- target/agents/xb-menu/preview
//! ```

use std::time::Duration;
use std::{env, fs, io, path::Path, process::ExitCode};

use xb_menu::{Canvas, Capabilities, Menu, Outcome, Setting, Status, draw_hud};
use xb_types::pad::{PadState, analog, button};
use xb_types::settings::Settings;

const FRAME: Duration = Duration::from_millis(16);

/// A busy, bright picture: the hard case for legibility.
fn game_frame(width: u32, height: u32) -> Vec<u8> {
    let mut rgb = Vec::with_capacity((width * height * 3) as usize);
    for y in 0..height {
        for x in 0..width {
            let checker = if (x / 32 + y / 32) % 2 == 0 { 40 } else { 0 };
            rgb.push((x * 255 / width.max(1)) as u8);
            rgb.push((120 + checker + y * 80 / height.max(1)) as u8);
            rgb.push((255 - y * 255 / height.max(1)) as u8);
        }
    }
    rgb
}

/// Blends the straight-alpha overlay over the frame, as the compositor will,
/// and writes the result as a binary PPM.
fn save(path: &Path, overlay: &[u8], width: u32, height: u32) -> io::Result<()> {
    let mut rgb = game_frame(width, height);
    for (dst, src) in rgb.as_chunks_mut::<3>().0.iter_mut().zip(overlay.as_chunks::<4>().0) {
        let a = u32::from(src[3]);
        for c in 0..3 {
            dst[c] = ((u32::from(src[c]) * a + u32::from(dst[c]) * (255 - a) + 127) / 255) as u8;
        }
    }
    let mut file = format!("P6\n{width} {height}\n255\n").into_bytes();
    file.extend_from_slice(&rgb);
    fs::write(path, file)
}

fn tap(menu: &mut Menu, pad: PadState) -> Outcome {
    let outcome = menu.update(FRAME, &[pad]);
    menu.update(FRAME, &[PadState::default()]);
    outcome
}

fn dpad(bits: u16) -> PadState {
    PadState { buttons: bits, ..PadState::default() }
}

fn a_button() -> PadState {
    let mut pad = PadState::default();
    pad.analog[analog::A] = 255;
    pad
}

fn shoot(dir: &Path, name: &str, menu: &mut Menu, width: u32, height: u32) -> io::Result<()> {
    let mut pixels = vec![0u8; (width * height * 4) as usize];
    let mut canvas = Canvas::new(&mut pixels, width, height).expect("buffer sized above");
    menu.render(&mut canvas);
    let path = dir.join(format!("{name}-{width}x{height}.ppm"));
    save(&path, &pixels, width, height)?;
    println!("{}", path.display());
    Ok(())
}

fn run(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let caps = Capabilities::default().next_boot(Setting::RenderScale);
    let mut menu = Menu::new(Settings::default(), caps);
    menu.set_status(&Status {
        cpu_backend: "KVM",
        gpu: "NVIDIA GeForce RTX 4070 Ti SUPER (Vulkan 1.3)",
        title: "Halo",
        game: "draws 4:3",
        fps: 59.7,
    });
    menu.update(FRAME, &[PadState::default()]);
    menu.open();
    shoot(dir, "1-opened", &mut menu, 640, 480)?;
    shoot(dir, "1-opened", &mut menu, 320, 240)?;
    shoot(dir, "1-opened", &mut menu, 1280, 960)?;
    shoot(dir, "1-opened", &mut menu, 854, 480)?;

    // Down to "Render scale" and up two steps: both arrows show.
    tap(&mut menu, dpad(button::DPAD_DOWN));
    tap(&mut menu, dpad(button::DPAD_RIGHT));
    tap(&mut menu, dpad(button::DPAD_RIGHT));
    shoot(dir, "2-render-scale", &mut menu, 640, 480)?;

    // On to "CPU backend", set to the longest value.
    for _ in 0..8 {
        tap(&mut menu, dpad(button::DPAD_DOWN));
    }
    tap(&mut menu, dpad(button::DPAD_RIGHT));
    tap(&mut menu, dpad(button::DPAD_RIGHT));
    shoot(dir, "3-cpu-backend", &mut menu, 640, 480)?;

    // "Reset game" and its confirm box, then "Yes" highlighted.
    tap(&mut menu, dpad(button::DPAD_DOWN));
    tap(&mut menu, a_button());
    shoot(dir, "4-confirm-no", &mut menu, 640, 480)?;
    tap(&mut menu, dpad(button::DPAD_RIGHT));
    shoot(dir, "5-confirm-yes", &mut menu, 640, 480)?;

    // The FPS counter alone.
    for (width, height) in [(640, 480), (320, 240)] {
        let mut pixels = vec![0u8; (width * height * 4) as usize];
        let mut canvas = Canvas::new(&mut pixels, width, height).expect("buffer sized above");
        draw_hud(&mut canvas, "60 fps");
        let path = dir.join(format!("6-hud-{width}x{height}.ppm"));
        save(&path, &pixels, width, height)?;
        println!("{}", path.display());
    }
    Ok(())
}

fn main() -> ExitCode {
    let Some(dir) = env::args_os().nth(1) else {
        eprintln!("usage: preview <output directory>");
        return ExitCode::FAILURE;
    };
    match run(Path::new(&dir)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("preview: {err}");
            ExitCode::FAILURE
        }
    }
}
