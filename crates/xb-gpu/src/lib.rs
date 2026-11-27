//! NV2A GPU: the register front end (Rust) and the push-buffer renderer
//! (vendored C, Vulkan).
//!
//! A title's Direct3D drives the NV2A directly: it writes registers in the
//! 16 MiB window at physical `0xFD00_0000` and fills a push buffer in guest
//! RAM. This crate is that device.
//!
//! * [`nv2a`] and [`timing`] are the registers as the guest sees them, the
//!   interrupt line, the vertical blank and flip pacing.
//! * The renderer under `vendor/xbased-hle` decodes the push buffer and
//!   draws with Vulkan. It is built by `build.rs` and reaches the machine
//!   only through its `host.h`, which [`ffi`] implements and forwards to a
//!   [`GpuHost`] the machine provides.
//! * [`present`] puts the picture and the core's overlay in the front end's
//!   window.
//!
//! # Use
//!
//! The machine creates one [`Gpu`] ([`Gpu::new`]), maps the register window
//! and forwards guest accesses to [`Gpu::mmio_read`] / [`Gpu::mmio_write`],
//! wires its write tracker to [`MemEvents`], and calls [`Gpu::title_start`]
//! once the XBE is mapped. The front end takes a [`Presenter`] and calls
//! [`Presenter::present`] in a loop on one thread.
//!
//! # Threads
//!
//! | Thread | Created by | Does |
//! |---|---|---|
//! | guest CPU | the machine | `mmio_read`/`mmio_write`, `title_start`, the [`MemEvents`] callbacks from page faults |
//! | `nv2a-vblank` | [`Gpu::new`] | raises the vertical blank every frame of the video mode on the host clock |
//! | `nv2a-ptimer` | [`Gpu::new`] | PTIMER's alarm interrupt on the guest's clock |
//! | decode (`hle-pb`), render (`hle.render`), timeline, pipeline workers | the renderer, at `title_start` | the push buffer to Vulkan |
//! | presenting | the front end | [`Presenter::present`] |
//!
//! Every method of [`Gpu`], [`Presenter`] and [`MemEvents`] may be called
//! from any thread unless its documentation names one.
//!
//! # One per process
//!
//! The renderer keeps its state in C globals and has no teardown (its render
//! thread is detached and the Vulkan device lives until the process exits),
//! so a process can create one `Gpu`. [`Gpu::shutdown`] stops the title's
//! decoding, the timers and presentation and saves the shader caches; it
//! does not free the device.
//!
//! `unsafe` is allowed in this crate: it calls C. The foreign functions are
//! all declared in [`ffi`].

pub mod ffi;
pub mod host;
pub mod nv2a;
mod os;
pub mod present;
pub mod regs;
pub mod timing;

use std::ffi::CString;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use xb_types::irq::IrqHub;
use xb_types::settings::{Aspect, Settings};

pub use host::{
    CpuRegs, DiscStats, GpuHost, StaleNeighbour, SurfaceProvider, VkInstance, VkSurfaceKHR,
};
pub use present::{FrameStats, OverlayImage};

use ffi::{CDecoder, HostClock, Shared};
use nv2a::{Nv2a, lock};
use present::{PresentSettings, PresentShared};

/// Physical address of the register window.
pub const NV2A_MMIO_BASE: u32 = 0xFD00_0000;
/// Size of the register window.
pub const NV2A_MMIO_SIZE: u32 = regs::NV2A_SIZE;

/// How the renderer builds the pipelines (shaders) a title needs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ShaderMode {
    /// Each new pipeline is compiled when first drawn with; the frame waits.
    Sync,
    /// New pipelines compile on worker threads; draws that need one are
    /// skipped until it is ready.
    Async,
    /// New pipelines compile on worker threads while interpreter pipelines
    /// draw in their place. The reference's default.
    #[default]
    Hybrid,
}

/// Configuration of a [`Gpu`]. The fields marked "at run time" can be changed
/// later through the `set_*` methods.
#[derive(Clone, Debug, PartialEq)]
pub struct GpuConfig {
    /// Rendering resolution as a multiple of the title's own, 1 to 4. At run
    /// time: takes effect when the title next creates or resets its Direct3D
    /// device.
    pub render_scale: u8,
    /// Forced anisotropic filtering: 0 (the title's own) or 2, 4, 8, 16. At
    /// run time: applies to samplers created afterwards.
    pub anisotropy: u8,
    /// Wait for the display's vertical sync when presenting. At run time.
    pub vsync: bool,
    /// Picture shape. At run time.
    pub aspect: Aspect,
    /// FXAA over each frame. At run time.
    pub fxaa: bool,
    /// Frame rate in the window's corner. At run time.
    pub show_fps: bool,
    /// How pipelines are built. Fixed at creation.
    pub shaders: ShaderMode,
    /// Build the pipelines recorded in earlier runs of the title before it
    /// starts. Fixed at creation.
    pub prebuild_shaders: bool,
    /// Where the renderer keeps its caches (`hle_pipeline_cache/`,
    /// `hle_pipelines/`, `hle_spirv/`, `gpu_precision.txt`). Created if
    /// missing.
    pub cache_dir: PathBuf,
}

impl GpuConfig {
    /// The configuration for the core's `settings`, caches under `cache_dir`.
    #[must_use]
    pub fn from_settings(settings: &Settings, cache_dir: impl Into<PathBuf>) -> Self {
        Self {
            render_scale: settings.render_scale,
            anisotropy: settings.anisotropy,
            vsync: settings.vsync,
            aspect: settings.aspect,
            fxaa: settings.fxaa,
            show_fps: settings.show_fps,
            shaders: ShaderMode::default(),
            prebuild_shaders: true,
            cache_dir: cache_dir.into(),
        }
    }
}

/// Why a [`Gpu`] could not be created.
#[derive(Debug)]
pub enum GpuError {
    /// This process already created a GPU (see "One per process" in the
    /// crate documentation).
    AlreadyCreated,
    /// The cache directory could not be created or its path cannot be given
    /// to C.
    CacheDir(std::io::Error),
}

impl std::fmt::Display for GpuError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyCreated => f.write_str("a GPU already exists in this process"),
            Self::CacheDir(e) => write!(f, "the GPU cache directory is unusable: {e}"),
        }
    }
}

impl std::error::Error for GpuError {}

/// Whose fault a [`Fatal`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FatalKind {
    /// The Vulkan driver lacks what the renderer needs, or there is none.
    Driver,
    /// The title cannot be rendered (no usable XBE in memory).
    Title,
    /// The driver lost the device while the title ran.
    GpuLost,
}

/// Why nothing (more) can be drawn. The text is written for the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fatal {
    /// Whose fault.
    pub kind: FatalKind,
    /// What happened.
    pub text: String,
}

impl std::fmt::Display for Fatal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

impl std::error::Error for Fatal {}

/// What the renderer read from the booted XBE.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TitleInfo {
    /// The certificate's title id.
    pub title_id: u32,
    /// The XBE's time stamp (with the id it names the title's shader caches).
    pub timedate: u32,
}

/// Counters of a running [`Gpu`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GpuStats {
    /// Frames the render thread finished (the title's swaps).
    pub frames: u64,
    /// Frames that became the picture on screen.
    pub flips: u64,
    /// Frames replaced before they were shown.
    pub dropped: u64,
    /// Refreshes the presenter drew into the window.
    pub presents: u64,
    /// Vertical blanks raised.
    pub vblanks: u64,
    /// Frame rate figures from the presenter.
    pub frame: FrameStats,
    /// Pipelines the start-up prebuild has built, and how many it will
    /// (0 and 0 when none runs).
    pub prebuild: (u32, u32),
    /// What the title's draws cost the renderer.
    pub draws: DrawStats,
}

/// Counts of the title's draws since boot, for judging the cost per draw.
///
/// A draw is cheapest when it shares the previous draw's constants block
/// and is joined with its neighbours into one host draw (the vendored
/// `vk/batch.c`); a title whose draws do neither pays for each one on the
/// decoder and the render thread.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrawStats {
    /// Draws the decoder emitted.
    pub draws: u64,
    /// Of them, draws that reused the previous draw's constants block.
    pub shared_constants: u64,
    /// Draws the back end joined into batches.
    pub joined: u64,
    /// The batches they were joined into.
    pub batches: u64,
}

/// The NV2A. One per process.
pub struct Gpu {
    shared: &'static Shared,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

impl Gpu {
    /// Creates the GPU: the register front end, its vblank and alarm
    /// threads, and the renderer's host side. No Vulkan yet; the device is
    /// created at [`Gpu::title_start`].
    ///
    /// `host` is the machine; the GPU keeps it for the life of the process
    /// (the renderer's threads can call it at any time, even after
    /// [`Gpu::shutdown`]). `ram` and `ram_size` are the host's view of
    /// guest physical RAM (the mapping `xb_types::mem::PhysMem` reads).
    /// `irq` is the machine's interrupt hub; the GPU drives `Irq::Gpu`.
    /// `surface` is the front end's window, or `None` to render without
    /// presenting.
    ///
    /// The vblank starts at once. Call [`Gpu::set_paused`] to hold it.
    ///
    /// # Errors
    ///
    /// [`GpuError::AlreadyCreated`] on a second call in one process;
    /// [`GpuError::CacheDir`] if `config.cache_dir` cannot be created.
    ///
    /// # Safety
    ///
    /// `ram` must be valid for reads and writes of `ram_size` bytes from any
    /// thread **for the rest of the process's life**: the renderer's threads
    /// read and write guest RAM through it (as a device's DMA does) and have
    /// no end. The mapping must tolerate those accesses racing with the
    /// guest's (a shared memory mapping does).
    pub unsafe fn new(
        host: Arc<dyn GpuHost>,
        ram: *mut u8,
        ram_size: u32,
        irq: Arc<IrqHub>,
        config: GpuConfig,
        surface: Option<Arc<dyn SurfaceProvider>>,
    ) -> Result<Self, GpuError> {
        std::fs::create_dir_all(&config.cache_dir).map_err(GpuError::CacheDir)?;
        let mut base = config.cache_dir.to_string_lossy().into_owned();
        if !base.ends_with('/') {
            base.push('/');
        }
        let base = CString::new(base).map_err(|e| {
            GpuError::CacheDir(std::io::Error::new(std::io::ErrorKind::InvalidInput, e))
        })?;

        let nv = Nv2a::new(irq, CDecoder, HostClock(host.clone()), ram_size);
        let present =
            PresentShared::new(PresentSettings::new(config.aspect, config.fxaa, config.show_fps));
        let has_window = surface.is_some();
        let shared = Box::new(Shared::new(
            nv,
            host,
            surface,
            base,
            clamp_scale(config.render_scale),
            clamp_anisotropy(config.anisotropy),
            present,
        ));
        let shared = shared.install().map_err(|_| GpuError::AlreadyCreated)?;

        // SAFETY: the renderer's start-up sequence, in the reference's order:
        // RAM first, then `hle_init`. `ram` is valid for `ram_size` bytes for
        // the process's life (the caller's contract). Nothing else has called
        // into the renderer yet (`install` succeeded once), so its globals
        // are ours to initialize; `g_hle.posture` is written through a raw
        // pointer before any renderer thread exists.
        unsafe {
            ffi::hle_guestmem_init(ram, u64::from(ram_size), 0);
            ffi::hle_init();
            (&raw mut ffi::g_hle.posture).write(ffi::HLE_POSTURE_REPLACE);
            ffi::hle_pipe_set_options(
                config.shaders == ShaderMode::Async,
                config.prebuild_shaders,
                config.shaders == ShaderMode::Hybrid,
            );
            // The presenter applies the display's gamma ramp where it draws;
            // without a window the frames carry it themselves.
            ffi::hle_disp_gamma_by_consumer(has_window);
            ffi::hle_present_set_vsync(config.vsync);
        }
        shared.started.store(true, Ordering::Release);
        log::info!(
            target: "gpu",
            "nv2a: {} MiB, render scale {}x, caches in {}",
            ram_size >> 20,
            shared.render_scale.load(Ordering::Relaxed),
            config.cache_dir.display(),
        );

        let spawn = |name: &str, body: fn(&'static Shared)| {
            std::thread::Builder::new()
                .name(name.into())
                .spawn(move || body(shared))
                .expect("spawning a GPU timer thread")
        };
        let threads = vec![
            spawn("nv2a-vblank", |s| s.nv.run_vblank()),
            spawn("nv2a-ptimer", |s| s.nv.run_alarm()),
        ];
        Ok(Self { shared, threads: Mutex::new(threads) })
    }

    // ---- the guest's register accesses ----

    /// A guest read of `size` (1, 2 or 4) bytes at `offset` within the
    /// register window. Guest-CPU thread. Never blocks.
    #[inline]
    #[must_use]
    pub fn mmio_read(&self, offset: u32, size: u8) -> u32 {
        self.shared.nv.mmio_read(offset, size)
    }

    /// The parts of the register window a CPU backend may let the guest
    /// read directly: `(offset, length, host address)`; see
    /// `Nv2a::read_through`. The memory lives as long as the GPU.
    #[must_use]
    pub fn read_through(&self) -> Vec<(u32, u32, *const u8)> {
        self.shared.nv.read_through()
    }

    /// A guest write of the low `size` (1, 2 or 4) bytes of `value` at
    /// `offset` within the register window. Guest-CPU thread.
    ///
    /// A write to channel 0's DMA_PUT (`0x800040`) is the kick: the decoder
    /// takes the push buffer up to there. It can call
    /// [`GpuHost::frame_wait`] before it returns.
    #[inline]
    pub fn mmio_write(&self, offset: u32, size: u8, value: u32) {
        self.shared.nv.mmio_write(offset, size, value);
    }

    // ---- the title ----

    /// The title's XBE is mapped at `0x0001_0000` and its code is about to
    /// run: the renderer reads its headers through [`GpuHost::va_read`],
    /// creates the Vulkan device, builds the title's recorded pipelines (if
    /// configured; this can take seconds, [`Gpu::stats`] reports progress)
    /// and takes over channel 0's push buffer. Call on the guest-CPU thread
    /// before the first guest instruction.
    ///
    /// # Errors
    ///
    /// A [`Fatal`] when the title cannot be drawn: no Vulkan driver or one
    /// that lacks what the renderer needs ([`FatalKind::Driver`]), or no
    /// readable XBE ([`FatalKind::Title`]). The same value is kept for
    /// [`Gpu::fatal`].
    pub fn title_start(&self) -> Result<TitleInfo, Fatal> {
        let s = self.shared;
        // SAFETY: `hle_init` ran in `new`. `hle_title_start` reads guest
        // memory through `hle_va_rw` and starts the renderer's threads; it is
        // not reentrant, and callers are on the one guest-CPU thread.
        unsafe { ffi::hle_title_start() };
        // SAFETY: the title's fields of `g_hle` are written by
        // `hle_title_start` on this thread only; plain reads of initialized
        // integers.
        let (fp, title_id, timedate) = unsafe {
            let g = &raw const ffi::g_hle;
            ((*g).fp, (*g).title_id, (*g).timedate)
        };
        let unavailable = ffi::unavailable_text();
        // SAFETY: plain getters.
        let (active, running) = unsafe { (ffi::hle_pb_active(), ffi::hle_back_running()) };
        let failure = if !unavailable.is_empty() {
            Some((FatalKind::Driver, unavailable))
        } else if fp == 0 || !active {
            Some((
                FatalKind::Title,
                "no usable XBE header at 0x00010000: the renderer is idle".to_owned(),
            ))
        } else if !running {
            Some((FatalKind::Driver, "the Vulkan back end did not start".to_owned()))
        } else {
            None
        };
        if let Some((kind, text)) = failure {
            s.set_fatal(kind, text);
            return Err(self.fatal().expect("a fatal was just recorded"));
        }
        log::info!(target: "gpu", "title {title_id:08x} in push-buffer mode");
        Ok(TitleInfo { title_id, timedate })
    }

    /// The kernel's `AvSetDisplayMode(…, Mode, Format, Pitch, FrameBuffer)`:
    /// the video mode's refresh rate (bit 30 of `mode`: the 50 Hz modes)
    /// sets the vblank period, and the decoder's display takes its size from
    /// the frame buffer's pitch and format. A `mode` of 0 is ignored, as in
    /// the kernel.
    pub fn av_set_display_mode(&self, mode: u32, format: u32, pitch: u32, frame_buffer: u32) {
        if mode == 0 {
            return;
        }
        let hz = if mode & 0x4000_0000 != 0 { 50 } else { 60 };
        log::info!(
            target: "gpu",
            "av: display mode {mode:08x} format {format:08x} pitch {pitch} fb {frame_buffer:08x} ({hz} Hz)"
        );
        self.shared.nv.set_refresh(hz);
        self.shared.nv.set_av_mode(mode);
        if self.shared.started.load(Ordering::Acquire) {
            // SAFETY: plain values; documented as callable from any thread
            // (pb/pbmode.h), and `hle_init` ran.
            unsafe {
                ffi::hle_pb_display_mode(pitch, format, hz);
                if frame_buffer != 0 {
                    ffi::hle_pb_display_fb(frame_buffer);
                }
            }
        }
    }

    /// The AV mode word last set by [`Gpu::av_set_display_mode`] (0: none).
    /// On a standard-definition output bit 28 says the title asked for
    /// widescreen signalling.
    #[must_use]
    pub fn av_mode(&self) -> u32 {
        self.shared.nv.av_mode()
    }

    /// Records the picture shape the title asked Direct3D for
    /// (`D3DPRESENTFLAG_WIDESCREEN`), when the kernel finds it: `Some(true)`
    /// 16:9, `Some(false)` 4:3. The core may use [`Gpu::title_wide`] to pick
    /// an aspect setting.
    pub fn set_title_wide(&self, wide: Option<bool>) {
        self.shared.nv.set_av_title(match wide {
            None => 0,
            Some(false) => 1,
            Some(true) => 2,
        });
    }

    /// What [`Gpu::set_title_wide`] recorded.
    #[must_use]
    pub fn title_wide(&self) -> Option<bool> {
        match self.shared.nv.av_title() {
            1 => Some(false),
            2 => Some(true),
            _ => None,
        }
    }

    // ---- settings at run time ----

    /// Render scale, 1 to 4: what the renderer sizes its images by when it
    /// next makes its device. [`Gpu::rescale`] makes that now.
    pub fn set_render_scale(&self, scale: u8) {
        self.shared.render_scale.store(clamp_scale(scale), Ordering::Relaxed);
    }

    /// Applies a changed render scale to a running title: the renderer
    /// writes what it drew back to guest memory, forgets its targets and
    /// textures and makes them again at the scale from that memory (the
    /// vendored `hle_pb_rescale`). Call with the guest CPU paused; it waits
    /// up to a second for the decoder. False when the decoder did not come
    /// round in that time (it does it when it next runs) or no title runs.
    pub fn rescale(&self) -> bool {
        if !self.shared.started.load(Ordering::Acquire) {
            return false;
        }
        // SAFETY: callable from any thread once the renderer runs; it sets a
        // request, kicks the decode thread and polls for its answer.
        unsafe { ffi::hle_pb_active() && ffi::hle_pb_rescale(1_000_000_000) }
    }

    /// Stops the push-buffer decoder between two passes for a savestate:
    /// everything it took is with the GPU, the GPU is done, and what it drew
    /// is back in guest memory. Call with the guest CPU paused, and
    /// [`Gpu::unpark`] after. False if the decoder did not stop in two
    /// seconds (nothing is parked then).
    pub fn park(&self) -> bool {
        if !self.shared.started.load(Ordering::Acquire) {
            return true;
        }
        // SAFETY: callable from any thread once the renderer runs.
        let parked = unsafe { ffi::hle_pb_park(false, 2_000_000_000) };
        if !parked {
            // SAFETY: as above; withdraws the request.
            unsafe { ffi::hle_pb_unpark() };
        }
        parked
    }

    /// Lets a parked decoder go on.
    pub fn unpark(&self) {
        if self.shared.started.load(Ordering::Acquire) {
            // SAFETY: callable from any thread once the renderer runs.
            unsafe { ffi::hle_pb_unpark() };
        }
    }

    /// Saves the GPU's state, or replaces it with the stream's: the
    /// register front end and the decoder's (the vendored records, as
    /// bytes; a state of another build of them fails). The decoder must be
    /// parked ([`Gpu::park`]). Loading, the decoder forgets its targets and
    /// textures and makes them again from guest memory, which the caller
    /// has already replaced.
    pub fn state(&self, io: &mut xb_types::state::StateIo) {
        let loading = io.loading();
        io.section(*b"NV2A", 1, |io, _| self.shared.nv.state(io));
        io.section(*b"PBD ", 1, |io, _| {
            let started = self.shared.started.load(Ordering::Acquire);
            let mut bytes = if loading || !started {
                Vec::new()
            } else {
                // SAFETY: the decoder is parked (the caller's contract); a
                // NULL buffer only asks for the size.
                let need = unsafe { ffi::hle_pb_state_bytes(std::ptr::null_mut(), 0) };
                let mut b = vec![0u8; need];
                // SAFETY: as above; `b` is valid for `b.len()` bytes.
                let wrote = unsafe { ffi::hle_pb_state_bytes(b.as_mut_ptr(), b.len()) };
                b.truncate(wrote.min(need));
                b
            };
            io.blob(&mut bytes);
            if loading && io.failed().is_none() && started && !bytes.is_empty() {
                // SAFETY: the decoder is parked; `bytes` is valid for its length.
                if !unsafe { ffi::hle_pb_state_put(bytes.as_ptr(), bytes.len()) } {
                    io.fail("the state's renderer part is of another build");
                }
            }
        });
    }

    /// Forced anisotropic filtering (0, 2, 4, 8, 16): applies to samplers
    /// the renderer creates from now on.
    pub fn set_anisotropy(&self, level: u8) {
        self.shared.anisotropy.store(clamp_anisotropy(level), Ordering::Relaxed);
    }

    /// Vertical sync of the window's presents: the swapchain is made again
    /// at the next present.
    pub fn set_vsync(&self, on: bool) {
        // SAFETY: sets two plain flags the presenter reads at its next frame
        // (documented "applied at the next frame", vk/swapchain.h).
        unsafe { ffi::hle_present_set_vsync(on) };
    }

    /// Picture shape; eased in over a fifth of a second.
    pub fn set_aspect(&self, aspect: Aspect) {
        self.shared.present.settings.set_aspect(aspect);
    }

    /// FXAA over each frame.
    pub fn set_fxaa(&self, on: bool) {
        self.shared.present.settings.set_fxaa(on);
    }

    /// The frame rate in the window's corner (drawn by the presenter).
    pub fn set_show_fps(&self, on: bool) {
        self.shared.present.settings.set_show_fps(on);
    }

    /// Applies the run-time GPU settings of `settings` (render scale,
    /// anisotropy, vsync, aspect, FXAA, show-FPS).
    pub fn apply_settings(&self, settings: &Settings) {
        self.set_render_scale(settings.render_scale);
        self.set_anisotropy(settings.anisotropy);
        self.set_vsync(settings.vsync);
        self.set_aspect(settings.aspect);
        self.set_fxaa(settings.fxaa);
        self.set_show_fps(settings.show_fps);
    }

    // ---- pause, status, shutdown ----

    /// The guest is paused (`true`) or runs again: while paused no vblank is
    /// raised and PTIMER's alarm does not fire. Presentation is unaffected:
    /// [`Presenter::present`] keeps showing the last frame and the overlay.
    pub fn set_paused(&self, paused: bool) {
        self.shared.nv.set_paused(paused);
        if paused && self.shared.started.load(Ordering::Acquire) {
            // SAFETY: tells the renderer's stutter monitor not to measure the
            // pause; a no-op when the monitor is not built in.
            unsafe { ffi::hle_hitch_pause() };
        }
    }

    /// The host display refreshed at `ns` (`CLOCK_MONOTONIC`), as a
    /// compositor's frame callback reports it. Optional: with these reports
    /// and a display whose refresh a frame is a whole multiple of, the
    /// vblank locks to the display. See [`timing`].
    pub fn display_vsync(&self, ns: u64) {
        self.shared.nv.display_vsync(ns);
    }

    /// The display mode's nominal refresh period in nanoseconds (0: unknown).
    pub fn display_nominal(&self, period_ns: u64) {
        self.shared.nv.display_nominal(period_ns);
    }

    /// Vertical blanks since creation (the video encoder's field bit is its
    /// lowest bit).
    #[must_use]
    pub fn vblank_count(&self) -> u64 {
        self.shared.nv.vblank_count()
    }

    /// Why nothing more can be drawn, once that is so: the renderer could
    /// not start, or the driver lost the device. Cheap to poll.
    #[must_use]
    pub fn fatal(&self) -> Option<Fatal> {
        if !self.shared.fatal_set.load(Ordering::Acquire) {
            return None;
        }
        lock(&self.shared.fatal).clone()
    }

    /// Counters and frame rate.
    #[must_use]
    pub fn stats(&self) -> GpuStats {
        let s = self.shared;
        let mut st = GpuStats {
            vblanks: s.nv.vblank_count(),
            frame: s.present.stats(),
            ..GpuStats::default()
        };
        // SAFETY: getters documented as callable from any thread; the back
        // end's are read only once it runs (its locks exist then).
        unsafe {
            if ffi::hle_back_running() {
                let d = ffi::hle_disp_stats();
                st.frames = ffi::hle_back_swap_count();
                st.flips = d.presented;
                st.dropped = d.dropped;
                st.presents = ffi::hle_present_count();
                let (mut ff, mut skipped, mut tex_defs) = (0u64, 0u64, 0u64);
                ffi::hle_pbd_draw_counts(&mut st.draws.draws, &mut ff, &mut skipped, &mut tex_defs);
                st.draws.shared_constants = ffi::hle_pbd_const_reuses();
                ffi::hle_batch_stats(&mut st.draws.batches, &mut st.draws.joined);
            }
            let (mut done, mut total) = (0u32, 0u32);
            ffi::hle_pipe_progress(&mut done, &mut total);
            st.prebuild = (done, total);
        }
        st
    }

    /// The handle the front end presents with.
    #[must_use]
    pub fn presenter(&self) -> Presenter {
        Presenter { shared: self.shared }
    }

    /// The renderer's write-tracking callbacks, for the machine's tracker.
    #[must_use]
    pub fn mem_events(&self) -> MemEvents {
        MemEvents { shared: self.shared }
    }

    /// Stops the GPU: presentation, the vblank and alarm threads, and the
    /// title's decoding; then writes the pipeline cache and the record of
    /// the pipelines made. Call after the guest CPU has stopped for good.
    /// Idempotent; also run on drop.
    ///
    /// The Vulkan device and the renderer's idle threads stay until the
    /// process exits (the renderer has no teardown).
    pub fn shutdown(&self) {
        let s = self.shared;
        if s.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        // A present under way finishes first; later ones return at once.
        s.present.wait_idle();
        s.nv.stop();
        for t in lock(&self.threads).drain(..) {
            let _ = t.join();
        }
        if !s.started.load(Ordering::Acquire) {
            return;
        }
        // SAFETY: `hle_init` ran. `hle_title_stop` joins the decode thread;
        // the guest is stopped (the caller's contract), so nothing kicks it
        // meanwhile.
        unsafe { ffi::hle_title_stop() };
        // SAFETY: plain getter.
        if unsafe { ffi::hle_back_running() } && !s.fatal_set.load(Ordering::Acquire) {
            // The render thread finishes what the decoder queued, then idles.
            let deadline = Instant::now() + Duration::from_secs(2);
            // SAFETY: timeline getters, any thread.
            while unsafe { ffi::hle_tl_completed() < ffi::hle_tl_submitted() }
                && Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(2));
            }
            // SAFETY: the decoder is gone and the render thread has no
            // commands, so nothing else touches the cache's bookkeeping; the
            // write itself is the one the renderer's own save worker does off
            // the render thread.
            unsafe { ffi::hle_back_save_caches() };
            log::info!(target: "gpu", "shader caches saved");
        }
        log::info!(target: "gpu", "nv2a: stopped");
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Presents the picture in the front end's window. Cheap to copy.
///
/// The front end calls [`Presenter::present`] in a loop on **one** thread
/// (any thread; its main thread or one made for it). That thread is where
/// the surface is created ([`SurfaceProvider::create_surface`]) and where the
/// swapchain lives.
#[derive(Clone, Copy)]
pub struct Presenter {
    shared: &'static Shared,
}

impl Presenter {
    /// Draws one refresh into the window, whose drawable is `width` by
    /// `height` pixels now: the newest frame the title flipped to,
    /// letterboxed to the aspect setting, with FXAA if set, the display's
    /// gamma ramp, the frame rate if set and the overlay over the picture.
    ///
    /// Blocks for about one frame at most: for the swapchain's next image
    /// when there is something new to draw, or up to 4 ms for a new frame
    /// when there is not (the window keeps the last picture). Passing a new
    /// size is all a resize needs; the swapchain is made again.
    ///
    /// Returns `false` when nothing could be drawn: the renderer is not up
    /// yet (before [`Gpu::title_start`]), there is no surface, the window
    /// has no area, or the GPU was shut down. The caller should wait a few
    /// milliseconds before trying again.
    ///
    /// While the guest is paused this keeps working: the last frame and the
    /// current overlay are shown.
    pub fn present(&self, width: u32, height: u32) -> bool {
        present::present(self.shared, width, height)
    }

    /// Replaces the overlay: an RGBA image composited over the game picture
    /// at every present, stretched to the picture's rectangle (so it keeps
    /// its place and shape under any window size or aspect setting), or
    /// `None` to remove it. The pixels are copied. Any thread; takes effect
    /// at the next present.
    ///
    /// Returns `false` if the image's size is 0 or over 8192 or `rgba` is
    /// shorter than `width * height * 4`.
    pub fn set_overlay(&self, image: Option<OverlayImage<'_>>) -> bool {
        self.shared.present.set_overlay(image)
    }

    /// The window (or its surface) was replaced: the next present drops the
    /// swapchain and the old surface and asks the [`SurfaceProvider`] for a
    /// new one. A surface the driver reports lost is replaced without this.
    pub fn reset_surface(&self) {
        self.shared.present.reset_surface();
    }

    /// Asks for a picture of the window as the next present draws it (the
    /// frame placed and filtered, the overlay over it), written to `path`: a
    /// JPEG if the name ends in `.jpg`, else a 32-bit BMP. Poll
    /// [`Presenter::screenshot_result`] for the outcome. Returns `false` if
    /// the path cannot be given to C (over 500 bytes or an interior NUL).
    pub fn screenshot(&self, path: &std::path::Path) -> bool {
        let Some(c) = path_cstring(path) else { return false };
        self.shared.present.screenshot(c);
        true
    }

    /// How the screenshot asked for `path` ended: `None` not yet (or another
    /// path's), `Some(Ok(()))` written, `Some(Err(why))` not. Reports once.
    #[must_use]
    pub fn screenshot_result(&self, path: &std::path::Path) -> Option<Result<(), String>> {
        let c = path_cstring(path)?;
        // SAFETY: a plain getter; false before the presenter's state exists.
        if !unsafe { ffi::hle_back_running() } {
            return None;
        }
        let mut why = [0 as std::ffi::c_char; 160];
        // SAFETY: `c` is NUL-terminated and `why` is writable for its length;
        // the function reads an atomic and copies a string.
        match unsafe { ffi::hle_present_shot_end(c.as_ptr(), why.as_mut_ptr(), why.len()) } {
            0 => None,
            1 => Some(Ok(())),
            _ => {
                // SAFETY: the function NUL-terminates `why` within its length.
                let text = unsafe { std::ffi::CStr::from_ptr(why.as_ptr()) };
                Some(Err(text.to_string_lossy().into_owned()))
            }
        }
    }

    /// Frame rate figures.
    #[must_use]
    pub fn frame_stats(&self) -> FrameStats {
        self.shared.present.stats()
    }

    /// Whether a frame of the title has been shown yet.
    #[must_use]
    pub fn shown(&self) -> bool {
        self.shared.present.shown()
    }
}

/// The renderer's side of write tracking: what the machine's tracker
/// (`xb-mem`) must call when the guest touches tracked memory, and when the
/// machine itself writes or frees guest RAM. These are the calls into the
/// renderer's write barrier (`mem/barrier.c`). Cheap to copy; every method is
/// a no-op before the renderer is initialized.
///
/// Several of them wait for the GPU on the calling thread (that is their
/// purpose: the guest's store must not overtake GPU work that reads the old
/// bytes). They call [`GpuHost::guest_hold`] around the wait.
#[derive(Clone, Copy)]
pub struct MemEvents {
    shared: &'static Shared,
}

impl MemEvents {
    #[inline]
    fn live(&self) -> bool {
        self.shared.started.load(Ordering::Acquire)
    }

    /// The guest is about to store `len` bytes at physical `pa` on a page
    /// tracking made clean ([`GpuHost::mem_test_and_clear`],
    /// [`GpuHost::mem_rearm`]). Call on the faulting (guest-CPU) thread
    /// before making the page writable; may wait for the GPU.
    /// (`hle_mem_first_write`.)
    #[inline]
    pub fn first_write(&self, pa: u32, len: u32) {
        if self.live() {
            // SAFETY: plain values; `hle_init` ran (`live`).
            unsafe { ffi::hle_mem_first_write(u64::from(pa), len) };
        }
    }

    /// The guest accessed a page without access ([`GpuHost::mem_noaccess`]):
    /// the image newer than the memory is written into it and the page given
    /// back (through [`GpuHost::mem_noaccess`] with `on = false`), then the
    /// access can run again. `write`: a store, or not known. May wait for the
    /// GPU. (`hle_mem_stale_access_rw`.)
    #[inline]
    pub fn stale_access(&self, pa: u32, write: bool) {
        if self.live() {
            // SAFETY: as above.
            unsafe { ffi::hle_mem_stale_access_rw(pa, write) };
        }
    }

    /// Whether guest bytes `[lo, hi)` (physical, on one page without access)
    /// are the guest's own beside a render target's. Ask before
    /// [`MemEvents::stale_access`] when the faulting access's span is known.
    /// (`hle_mem_stale_neighbour`.)
    #[inline]
    #[must_use]
    pub fn stale_neighbour(&self, lo: u32, hi: u32) -> StaleNeighbour {
        if !self.live() {
            return StaleNeighbour::No;
        }
        // SAFETY: as above.
        match unsafe { ffi::hle_mem_stale_neighbour(lo, hi) } {
            1 => StaleNeighbour::Emulate,
            2 => StaleNeighbour::Reopened,
            _ => StaleNeighbour::No,
        }
    }

    /// The host or a device wrote `[pa, pa + len)` through the RAM view: its
    /// CPU cache lines are cleaned before the GPU next reads the pages in
    /// place. The tracker calls this from [`GpuHost::mem_set_dirty`] and
    /// [`GpuHost::mem_landed`]. (`hle_mem_host_wrote`.)
    #[inline]
    pub fn host_wrote(&self, pa: u32, len: u32) {
        if self.live() {
            // SAFETY: as above.
            unsafe { ffi::hle_mem_host_wrote(pa, len) };
        }
    }

    /// Memory in `[pa, pa + len)` written by the host or a device is newer
    /// than any image guarded over it: the guards go. The tracker calls this
    /// from [`GpuHost::mem_set_dirty`] when a page of the range has no guest
    /// access. (`hle_barrier_unguard_range`.)
    #[inline]
    pub fn unguard_range(&self, pa: u32, len: u32) {
        if self.live() {
            // SAFETY: as above.
            unsafe { ffi::hle_barrier_unguard_range(pa, len) };
        }
    }

    /// The kernel freed the guest memory `[pa, pa + len)`: guards over it are
    /// dropped and nothing the GPU drew lands in it later.
    /// (`hle_barrier_freed`, `hle_tl_land_freed`.)
    #[inline]
    pub fn freed(&self, pa: u32, len: u32) {
        if self.live() {
            // SAFETY: as above.
            unsafe {
                ffi::hle_barrier_freed(pa, len);
                ffi::hle_tl_land_freed(pa, len);
            }
        }
    }

    /// Another thread will write `[pa, pa + len)` later (an overlapped file
    /// read): the GPU progress it must wait for first, or 0 for none. That
    /// thread calls [`MemEvents::wait_fence`] with it before writing and
    /// reports the write with [`GpuHost::mem_set_dirty`] after.
    /// (`hle_mem_write_fence`.)
    #[inline]
    #[must_use]
    pub fn write_fence(&self, pa: u32, len: u32) -> u64 {
        if !self.live() {
            return 0;
        }
        // SAFETY: as above.
        unsafe { ffi::hle_mem_write_fence(pa, len) }
    }

    /// Waits for the GPU progress [`MemEvents::write_fence`] returned.
    /// (`hle_mem_wait_fence`.)
    #[inline]
    pub fn wait_fence(&self, value: u64) {
        if value != 0 && self.live() {
            // SAFETY: as above.
            unsafe { ffi::hle_mem_wait_fence(value) };
        }
    }
}

/// `path` for the presenter, whose path buffer holds 512 bytes.
fn path_cstring(path: &std::path::Path) -> Option<CString> {
    let bytes = path.as_os_str().as_encoded_bytes();
    if bytes.len() > 500 {
        return None;
    }
    CString::new(bytes).ok()
}

fn clamp_scale(scale: u8) -> i32 {
    i32::from(scale.clamp(1, 4))
}

/// The level at or below `level` among 0, 2, 4, 8, 16, as the reference
/// rounds it.
fn clamp_anisotropy(level: u8) -> i32 {
    match level {
        16.. => 16,
        8.. => 8,
        4.. => 4,
        2.. => 2,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_handles_cross_threads() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Gpu>();
        assert_send_sync::<Presenter>();
        assert_send_sync::<MemEvents>();
    }

    #[test]
    fn settings_are_clamped_to_what_the_renderer_takes() {
        assert_eq!(clamp_scale(0), 1);
        assert_eq!(clamp_scale(3), 3);
        assert_eq!(clamp_scale(9), 4);
        assert_eq!(
            [0u8, 1, 2, 3, 5, 8, 15, 16, 200].map(clamp_anisotropy),
            [0, 0, 2, 2, 4, 8, 8, 16, 16]
        );
    }

    #[test]
    fn the_configuration_follows_the_core_settings() {
        let s =
            Settings { render_scale: 2, fxaa: true, aspect: Aspect::Wide, ..Settings::default() };
        let c = GpuConfig::from_settings(&s, "/tmp/x");
        assert_eq!((c.render_scale, c.fxaa, c.aspect, c.vsync), (2, true, Aspect::Wide, s.vsync));
        assert_eq!(c.shaders, ShaderMode::Hybrid);
    }
}
