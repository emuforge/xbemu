//! Putting the picture on screen: the front frame letterboxed into the
//! window, the overlay over it, and the frame-rate figures.
//!
//! This module does the placement, the eased change of shape, the frame
//! counter and the loop's waits. The drawing itself is the vendored
//! presenter's (`vk/swapchain.c`: swapchain, FXAA, scaling, gamma, the
//! frame-rate digits). Not done: overscan and crop, the CRT filters, FSR
//! upscaling and screenshots, which the core has no settings for.
//!
//! The overlay uses the presenter's own mechanism
//! (`hle_present_set_overlay`, `hle_ov_tex_*`): textured triangles in
//! window pixels composited by the presenter after the picture. The core's
//! menu arrives as one RGBA image; it becomes one texture and one quad over
//! the picture's rectangle, so it scales with the picture.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};

use xb_types::settings::Aspect;

use crate::ffi::{
    self, DOWNSAMPLE_OFF, HLE_OV_BLEND, HLE_UPSCALE_BILINEAR, HleOvCmd, HleOvVert, Shared,
};
use crate::nv2a::lock;
use crate::os;

/// How long a change of aspect setting takes to ease in, nanoseconds.
const TWEEN_NS: f32 = 220e6;

/// Presentation settings, changeable from any thread at any time.
pub(crate) struct PresentSettings {
    aspect: AtomicU8,
    fxaa: AtomicBool,
    show_fps: AtomicBool,
}

impl PresentSettings {
    pub(crate) fn new(aspect: Aspect, fxaa: bool, show_fps: bool) -> Self {
        Self {
            aspect: AtomicU8::new(aspect_code(aspect)),
            fxaa: AtomicBool::new(fxaa),
            show_fps: AtomicBool::new(show_fps),
        }
    }
    pub(crate) fn set_aspect(&self, a: Aspect) {
        self.aspect.store(aspect_code(a), Ordering::Relaxed);
    }
    pub(crate) fn set_fxaa(&self, on: bool) {
        self.fxaa.store(on, Ordering::Relaxed);
    }
    pub(crate) fn set_show_fps(&self, on: bool) {
        self.show_fps.store(on, Ordering::Relaxed);
    }
}

fn aspect_code(a: Aspect) -> u8 {
    match a {
        Aspect::Standard => 0,
        Aspect::Wide => 1,
        Aspect::Stretch => 2,
    }
}

/// An RGBA image for [`crate::Presenter::set_overlay`]: `width * height`
/// pixels, rows top to bottom, four bytes a pixel (R, G, B, A), straight
/// (not premultiplied) alpha.
#[derive(Clone, Copy, Debug)]
pub struct OverlayImage<'a> {
    /// Width in pixels, 1 to 8192.
    pub width: u32,
    /// Height in pixels, 1 to 8192.
    pub height: u32,
    /// `width * height * 4` bytes.
    pub rgba: &'a [u8],
}

/// An overlay change waiting for the presenting thread.
enum OverlayUpdate {
    Clear,
    Image { width: u32, height: u32, rgba: Vec<u8> },
}

/// Frame-rate figures measured from the frames that reached the window.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FrameStats {
    /// Frames the guest flipped to the display per second, over the last
    /// second or so (0 when nothing new was shown for two seconds).
    pub fps: f32,
    /// Mean time between those frames, milliseconds.
    pub avg_frame_ms: f32,
    /// Time between the last two, milliseconds.
    pub last_frame_ms: f32,
    /// Since the presenter started: frames that came more than half a mean
    /// frame late or early (an uneven pace, whatever the rate), and the
    /// longest time between two frames since [`FrameStats::fps`] was last
    /// worked out, milliseconds.
    pub uneven: u64,
    /// See `uneven`.
    pub worst_frame_ms: f32,
}

/// The presenting thread's own state.
#[derive(Default)]
struct PresentState {
    /// Drop the window's surface before the next present.
    reset_surface: bool,
    /// A screenshot asked for, not yet handed to the presenter.
    shot: Option<std::ffi::CString>,
    /// The overlay texture (0: none) and its size.
    tex: u32,
    tex_w: u32,
    tex_h: u32,
    /// The quad last handed to the presenter (window pixels: x0, y0, x1, y1).
    quad: Option<[f32; 4]>,
    // The eased change of shape (present.c `tween`).
    cur: Option<(f32, f32)>,
    from: (f32, f32),
    tween_t0: u64,
    shape: u8,
    // The frame counter (present.c `count_frames`).
    win_presented: u64,
    win_start_ns: u64,
    last_present_ns: u64,
    seen_presented: u64,
    /// The longest gap between two frames in the window being counted.
    win_worst_ms: f32,
    stats: FrameStats,
}

/// What the presenter shares between threads.
pub(crate) struct PresentShared {
    pub(crate) settings: PresentSettings,
    state: Mutex<PresentState>,
    pending_overlay: Mutex<Option<OverlayUpdate>>,
    /// Bit patterns of `FrameStats`, readable without the present lock.
    fps: AtomicU32,
    avg_ms: AtomicU32,
    last_ms: AtomicU32,
    uneven: AtomicU64,
    worst_ms: AtomicU32,
    shown: AtomicBool,
}

impl PresentShared {
    pub(crate) fn new(settings: PresentSettings) -> Self {
        Self {
            settings,
            state: Mutex::new(PresentState::default()),
            pending_overlay: Mutex::new(None),
            fps: AtomicU32::new(0),
            avg_ms: AtomicU32::new(0),
            last_ms: AtomicU32::new(0),
            uneven: AtomicU64::new(0),
            worst_ms: AtomicU32::new(0),
            shown: AtomicBool::new(false),
        }
    }

    pub(crate) fn stats(&self) -> FrameStats {
        FrameStats {
            fps: f32::from_bits(self.fps.load(Ordering::Relaxed)),
            avg_frame_ms: f32::from_bits(self.avg_ms.load(Ordering::Relaxed)),
            last_frame_ms: f32::from_bits(self.last_ms.load(Ordering::Relaxed)),
            uneven: self.uneven.load(Ordering::Relaxed),
            worst_frame_ms: f32::from_bits(self.worst_ms.load(Ordering::Relaxed)),
        }
    }

    pub(crate) fn shown(&self) -> bool {
        self.shown.load(Ordering::Relaxed)
    }

    pub(crate) fn set_overlay(&self, image: Option<OverlayImage<'_>>) -> bool {
        let update = match image {
            None => OverlayUpdate::Clear,
            Some(img) => {
                let bytes = img.width as usize * img.height as usize * 4;
                if img.width == 0
                    || img.height == 0
                    || img.width > 8192
                    || img.height > 8192
                    || img.rgba.len() < bytes
                {
                    return false;
                }
                OverlayUpdate::Image {
                    width: img.width,
                    height: img.height,
                    rgba: img.rgba[..bytes].to_vec(),
                }
            }
        };
        *lock(&self.pending_overlay) = Some(update);
        true
    }

    pub(crate) fn reset_surface(&self) {
        lock(&self.state).reset_surface = true;
    }

    pub(crate) fn screenshot(&self, path: std::ffi::CString) {
        lock(&self.state).shot = Some(path);
    }

    /// Returns once no present is under way.
    pub(crate) fn wait_idle(&self) {
        drop(lock(&self.state));
    }
}

/// The frame's half size in the window in clip space (1: the whole axis),
/// letterboxed to the chosen shape.
///
/// A standard-definition frame (640x480, 720x480, 640x576, 720x576: narrower
/// than 16:10) is the TV's whole picture whatever its pixel count, so it is
/// shown 4:3, or 16:9 when the user says the title draws anamorphic
/// widescreen. A high-definition frame keeps its own shape under
/// [`Aspect::Standard`].
pub(crate) fn place(aspect: Aspect, fw: u32, fh: u32, ww: u32, wh: u32) -> (f32, f32) {
    let wa = ww as f32 / wh.max(1) as f32;
    let own = fw as f32 / fh.max(1) as f32;
    let fa = match aspect {
        Aspect::Standard if own < 1.6 => 4.0 / 3.0,
        Aspect::Standard => own,
        Aspect::Wide => 16.0 / 9.0,
        Aspect::Stretch => wa,
    };
    if wa > fa { (fa / wa, 1.0) } else { (1.0, wa / fa) }
}

impl PresentState {
    /// Eases the frame into its new place over a fifth of a second when the
    /// aspect setting changes; the frame's own changes (a game's mode) jump.
    fn tween(&mut self, shape: u8, now: u64, target: (f32, f32)) -> (f32, f32) {
        match self.cur {
            None => self.shape = shape,
            Some(cur) if shape != self.shape => {
                self.shape = shape;
                self.from = cur;
                self.tween_t0 = now;
            }
            Some(_) => {}
        }
        let mut out = target;
        if self.tween_t0 != 0 {
            let k = (now - self.tween_t0) as f32 / TWEEN_NS;
            if k >= 1.0 {
                self.tween_t0 = 0;
            } else {
                let e = 1.0 - (1.0 - k) * (1.0 - k) * (1.0 - k);
                out = (
                    self.from.0 + (target.0 - self.from.0) * e,
                    self.from.1 + (target.1 - self.from.1) * e,
                );
            }
        }
        self.cur = Some(out);
        out
    }

    /// Whether an overlay texture exists whose quad the presenter has not
    /// been given yet: after the first [`OverlayUpdate::Image`], a change of
    /// its size, or a new surface.
    const fn overlay_waiting(&self) -> bool {
        self.tex != 0 && self.quad.is_none()
    }

    /// Whether this refresh may be left out when the presenter says the
    /// picture is unchanged. It may not while the shape eases, nor while an
    /// overlay waits: the presenter's "unchanged" knows an overlay only once
    /// its quad was handed over, which happens in a drawn refresh. Skipping
    /// then kept the menu off the screen whenever it opened with no overlay
    /// up before it (the frame-rate line off) and the title paused under
    /// it, so no new frame came to force a draw.
    const fn may_skip_draw(&self) -> bool {
        self.tween_t0 == 0 && !self.overlay_waiting()
    }

    /// Frames over the time between the first and the last of them seen, a
    /// second or more apart. Counting in fixed half seconds, a frame more or
    /// less reads as 58 or 62 at a steady 60.
    fn count_frames(&mut self, now: u64, presented: u64) {
        if self.win_start_ns == 0 {
            self.win_start_ns = now;
            self.win_presented = presented;
            self.seen_presented = presented;
            return;
        }
        if presented > self.seen_presented {
            if self.last_present_ns != 0 {
                let gap = (now - self.last_present_ns) as f32 / 1e6;
                self.stats.last_frame_ms = gap / (presented - self.seen_presented) as f32;
                let mean = self.stats.avg_frame_ms;
                if mean > 0.0 && (gap > mean * 1.5 || gap < mean * 0.5) {
                    self.stats.uneven += 1;
                }
                self.win_worst_ms = self.win_worst_ms.max(gap);
            }
            self.last_present_ns = now;
            self.seen_presented = presented;
        }
        if self.last_present_ns > self.win_start_ns
            && self.last_present_ns - self.win_start_ns >= 1_000_000_000
        {
            let n = presented - self.win_presented;
            let ms = (self.last_present_ns - self.win_start_ns) as f32 / 1e6;
            self.stats.fps = n as f32 * 1000.0 / ms;
            self.stats.avg_frame_ms = if n != 0 { ms / n as f32 } else { 0.0 };
            self.stats.worst_frame_ms = std::mem::take(&mut self.win_worst_ms);
            self.win_start_ns = self.last_present_ns;
            self.win_presented = presented;
        } else if now - self.win_start_ns >= 2_000_000_000 {
            // Nothing shown for a while.
            self.stats.fps = 0.0;
            self.stats.avg_frame_ms = 0.0;
            self.win_start_ns = now;
            self.win_presented = presented;
        }
    }
}

/// The overlay's quad: the picture's rectangle in window pixels.
fn overlay_quad(ww: u32, wh: u32, sx: f32, sy: f32) -> [f32; 4] {
    let (w, h) = (ww as f32, wh as f32);
    [w * (1.0 - sx) / 2.0, h * (1.0 - sy) / 2.0, w * (1.0 + sx) / 2.0, h * (1.0 + sy) / 2.0]
}

/// Two triangles covering `quad` with the whole texture.
fn quad_vertices(quad: [f32; 4]) -> [HleOvVert; 6] {
    let [x0, y0, x1, y1] = quad;
    let v = |x, y, u, v| HleOvVert { x, y, u, v, rgba: 0xFFFF_FFFF };
    [
        v(x0, y0, 0.0, 0.0),
        v(x1, y0, 1.0, 0.0),
        v(x1, y1, 1.0, 1.0),
        v(x0, y0, 0.0, 0.0),
        v(x1, y1, 1.0, 1.0),
        v(x0, y1, 0.0, 1.0),
    ]
}

/// One refresh of the window: see [`crate::Presenter::present`].
pub(crate) fn present(s: &'static Shared, width: u32, height: u32) -> bool {
    let p = &s.present;
    let mut st = lock(&p.state);
    // Until the back end runs the presenter's and the display's state does
    // not exist, so nothing below may be called.
    // SAFETY: a plain getter.
    let running = unsafe { ffi::hle_back_running() };
    if s.closed.load(Ordering::Acquire) || width == 0 || height == 0 || !running {
        return false;
    }
    let win = (&raw const s.window).cast_mut();
    s.window.width.store(width as i32, Ordering::Relaxed);
    s.window.height.store(height as i32, Ordering::Relaxed);
    let (mut ww, mut wh) = (0u32, 0u32);

    if std::mem::take(&mut st.reset_surface) {
        // SAFETY: a NULL window makes the presenter drop its surface and
        // swapchain; this is the presenting thread (the state lock is held).
        unsafe { ffi::hle_present_begin(std::ptr::null_mut(), &mut ww, &mut wh) };
        st.quad = None;
    }
    apply_overlay_update(p, &mut st);
    if let Some(path) = st.shot.take() {
        // SAFETY: presenting thread; the presenter copies the path.
        unsafe { ffi::hle_present_shot(path.as_ptr()) };
    }

    // Before the look: a frame coming meanwhile ends the wait below.
    // SAFETY: plain getters, callable from any thread.
    let (fronts, presented) = unsafe { (ffi::hle_disp_fronts(), ffi::hle_disp_stats().presented) };
    let now = os::monotonic_ns();
    st.count_frames(now, presented);
    p.fps.store(st.stats.fps.to_bits(), Ordering::Relaxed);
    p.avg_ms.store(st.stats.avg_frame_ms.to_bits(), Ordering::Relaxed);
    p.last_ms.store(st.stats.last_frame_ms.to_bits(), Ordering::Relaxed);
    p.uneven.store(st.stats.uneven, Ordering::Relaxed);
    p.worst_ms.store(st.stats.worst_frame_ms.to_bits(), Ordering::Relaxed);

    let fxaa = p.settings.fxaa.load(Ordering::Relaxed);
    let shape = p.settings.aspect.load(Ordering::Relaxed);
    let aspect = match shape {
        1 => Aspect::Wide,
        2 => Aspect::Stretch,
        _ => Aspect::Standard,
    };
    // What places and filters the frame, beside the frame and the window.
    let key = u64::from(fxaa) | u64::from(shape) << 8;
    // The vblank does not keep to the display's refreshes (always so on
    // Linux): with nothing new to show, wait for the next frame and show it
    // as it comes.
    let as_they_come = !s.nv.vblank_locked() || s.nv.vblank_timed();

    // SAFETY: `win` points at `Shared::window`, which lives forever; this is
    // the presenting thread.
    if st.may_skip_draw() && unsafe { ffi::hle_present_unchanged(win, key) } {
        if as_they_come {
            let next = s.nv.next_vsync(now);
            // No refreshes reported: a new frame ends the wait, so it may
            // last a whole refresh of the title's mode (the menu and the
            // pads are looked at once a refresh then, not every 4 ms).
            let idle = std::env::var_os("XBEMU_T4_PRESENT_4MS").map_or_else(
                || s.nv.vblank_period_ns().clamp(4_000_000, 17_000_000),
                |_| 4_000_000,
            );
            let until = if next != 0 { next + 500_000 } else { now + idle };
            let ms = if until > now { (until - now).div_ceil(1_000_000) as i32 } else { 1 };
            // SAFETY: waits on the display's own condition; any thread.
            unsafe { ffi::hle_disp_wait_front(fronts, ms) };
        } else {
            // It looked: the vblank keeps clear of it.
            s.nv.display_pickup(now);
            let next = s.nv.next_vsync(now);
            os::sleep_until_ns(if next != 0 { next + 500_000 } else { now + 4_000_000 });
        }
        return true;
    }
    // SAFETY: as above; `ww` and `wh` are valid for writes.
    if !unsafe { ffi::hle_present_begin(win, &mut ww, &mut wh) } {
        return false;
    }
    if !as_they_come {
        s.nv.display_pickup(os::monotonic_ns());
    }
    let (mut fw, mut fh) = (0u32, 0u32);
    // SAFETY: between a successful begin and end on the presenting thread.
    let have = unsafe { ffi::hle_present_frame(&mut fw, &mut fh) };
    // Until the title shows a frame the overlay takes the place a standard
    // picture would.
    let (fw, fh) = if have { (fw, fh) } else { (640, 480) };
    let target = place(aspect, fw, fh, ww, wh);
    let (sx, sy) = st.tween(shape, os::monotonic_ns(), target);
    if have {
        p.shown.store(true, Ordering::Relaxed);
    }
    if st.tex != 0 {
        let quad = overlay_quad(ww, wh, sx, sy);
        if st.quad != Some(quad) {
            st.quad = Some(quad);
            let verts = quad_vertices(quad);
            let cmd =
                HleOvCmd { tex: st.tex, blend: HLE_OV_BLEND, clip: [0; 4], first: 0, count: 6 };
            // SAFETY: the presenter copies the six vertices and the command
            // before returning; presenting thread.
            unsafe { ffi::hle_present_set_overlay(verts.as_ptr(), 6, &cmd, 1, 0) };
        }
    }
    let osd =
        if p.settings.show_fps.load(Ordering::Relaxed) { st.stats.fps.max(0.01) } else { 0.0 };
    // SAFETY: between begin and end on the presenting thread; plain values.
    unsafe {
        ffi::hle_present_set_clip(1.0, 1.0);
        ffi::hle_present_set_osd(osd);
        ffi::hle_present_end(sx, sy, fxaa, HLE_UPSCALE_BILINEAR, true, DOWNSAMPLE_OFF, key);
    }
    true
}

/// Takes a pending [`OverlayUpdate`] into the presenter's overlay texture.
fn apply_overlay_update(p: &PresentShared, st: &mut PresentState) {
    let Some(update) = lock(&p.pending_overlay).take() else { return };
    match update {
        OverlayUpdate::Clear => {
            if st.tex != 0 {
                // SAFETY: presenting thread; the id came from hle_ov_tex_new.
                // A NULL overlay turns it off.
                unsafe {
                    ffi::hle_present_set_overlay(std::ptr::null(), 0, std::ptr::null(), 0, 0);
                    ffi::hle_ov_tex_free(st.tex);
                }
                st.tex = 0;
                st.quad = None;
            }
        }
        OverlayUpdate::Image { width, height, rgba } => {
            if st.tex != 0 && (st.tex_w, st.tex_h) != (width, height) {
                // SAFETY: presenting thread; the id came from hle_ov_tex_new.
                // The overlay is turned off first so no command names the
                // freed texture.
                unsafe {
                    ffi::hle_present_set_overlay(std::ptr::null(), 0, std::ptr::null(), 0, 0);
                    ffi::hle_ov_tex_free(st.tex);
                }
                st.tex = 0;
            }
            if st.tex == 0 {
                // SAFETY: presenting thread; sizes were checked (1..=8192).
                st.tex = unsafe { ffi::hle_ov_tex_new(width, height) };
                st.tex_w = width;
                st.tex_h = height;
                st.quad = None;
                if st.tex == 0 {
                    log::warn!(target: "gpu", "present: no overlay texture left");
                    return;
                }
            }
            // SAFETY: `rgba` holds `width * height * 4` bytes (checked in
            // `set_overlay`), rows `width * 4` apart; the presenter copies
            // them before returning.
            unsafe {
                ffi::hle_ov_tex_put(st.tex, 0, 0, width, height, rgba.as_ptr().cast(), width * 4)
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_standard_frame_is_letterboxed_to_four_by_three_in_a_wide_window() {
        let (sx, sy) = place(Aspect::Standard, 640, 480, 1920, 1080);
        assert!((sx - 0.75).abs() < 1e-6 && sy == 1.0);
        // 720x480 is the same TV picture, not 3:2.
        assert_eq!(place(Aspect::Standard, 720, 480, 1920, 1080), (sx, sy));
        // In a tall window the bars go above and below.
        let (sx, sy) = place(Aspect::Standard, 640, 480, 1000, 1000);
        assert!(sx == 1.0 && (sy - 0.75).abs() < 1e-6);
    }

    #[test]
    fn a_high_definition_frame_keeps_its_own_shape() {
        assert_eq!(place(Aspect::Standard, 1280, 720, 1920, 1080), (1.0, 1.0));
        assert_eq!(place(Aspect::Wide, 640, 480, 1920, 1080), (1.0, 1.0));
        assert_eq!(place(Aspect::Stretch, 640, 480, 1234, 777), (1.0, 1.0));
    }

    #[test]
    fn the_placed_picture_has_the_chosen_shape_in_any_window() {
        for (ww, wh) in [(640, 480), (1920, 1080), (800, 1280), (3440, 1440)] {
            let (sx, sy) = place(Aspect::Standard, 640, 480, ww, wh);
            let shape = (sx * ww as f32) / (sy * wh as f32);
            assert!((shape - 4.0 / 3.0).abs() < 1e-3);
            assert!(sx <= 1.0 && sy <= 1.0 && (sx == 1.0 || sy == 1.0));
        }
    }

    #[test]
    fn the_overlay_quad_is_the_pictures_rectangle() {
        let quad = overlay_quad(1920, 1080, 0.75, 1.0);
        assert_eq!(quad, [240.0, 0.0, 1680.0, 1080.0]);
        let v = quad_vertices(quad);
        // Corners map to the texture's corners.
        assert!(v.iter().all(|p| (p.x == 240.0) == (p.u == 0.0) && (p.y == 0.0) == (p.v == 0.0)));
        assert!(v.iter().all(|p| p.rgba == 0xFFFF_FFFF));
    }

    #[test]
    fn a_change_of_aspect_eases_in_and_ends_at_the_target() {
        let mut st = PresentState::default();
        assert_eq!(st.tween(0, 1_000, (0.75, 1.0)), (0.75, 1.0));
        // The setting changes: starts from where it was...
        let a = st.tween(1, 2_000, (1.0, 1.0));
        assert!((a.0 - 0.75).abs() < 1e-3);
        // ... is under way halfway through ...
        let b = st.tween(1, 2_000 + 110_000_000, (1.0, 1.0));
        assert!(b.0 > 0.75 && b.0 < 1.0);
        // ... and done after 220 ms.
        assert_eq!(st.tween(1, 2_000 + 230_000_000, (1.0, 1.0)), (1.0, 1.0));
        // The frame's own change of shape jumps.
        assert_eq!(st.tween(1, 2_000 + 240_000_000, (0.5, 1.0)), (0.5, 1.0));
    }

    #[test]
    fn frames_are_counted_over_at_least_a_second() {
        let mut st = PresentState::default();
        let frame = 16_666_667u64;
        let mut now = 5_000_000_000u64;
        st.count_frames(now, 100);
        for n in 1..=90u64 {
            now += frame;
            st.count_frames(now, 100 + n);
        }
        assert!((st.stats.fps - 60.0).abs() < 0.1, "fps {}", st.stats.fps);
        assert!((st.stats.avg_frame_ms - 16.667).abs() < 0.01);
        assert!((st.stats.last_frame_ms - 16.667).abs() < 0.01);
        // Nothing new for two seconds reads as 0.
        st.count_frames(now + 2_100_000_000, 190);
        assert_eq!(st.stats.fps, 0.0);
    }

    #[test]
    fn a_refresh_is_drawn_while_an_overlay_waits_for_its_quad() {
        // Nothing up: a still picture need not be drawn again.
        let mut st = PresentState::default();
        assert!(st.may_skip_draw());
        // The first overlay image made a texture; the presenter has no quad
        // for it until a refresh is drawn, however still the picture is.
        st.tex = 1;
        assert!(st.overlay_waiting() && !st.may_skip_draw());
        // Once handed over, the presenter itself knows the overlay.
        st.quad = Some(overlay_quad(640, 480, 1.0, 1.0));
        assert!(!st.overlay_waiting() && st.may_skip_draw());
        // A new surface forgets the quad.
        st.quad = None;
        assert!(!st.may_skip_draw());
        // The overlay gone, nothing waits.
        st.tex = 0;
        assert!(st.may_skip_draw());
    }

    #[test]
    fn an_overlay_image_must_carry_all_its_pixels() {
        let p = PresentShared::new(PresentSettings::new(Aspect::Standard, false, false));
        let px = [0u8; 4 * 6];
        assert!(p.set_overlay(Some(OverlayImage { width: 3, height: 2, rgba: &px })));
        assert!(!p.set_overlay(Some(OverlayImage { width: 3, height: 3, rgba: &px })));
        assert!(!p.set_overlay(Some(OverlayImage { width: 0, height: 2, rgba: &px })));
        assert!(p.set_overlay(None));
    }
}
