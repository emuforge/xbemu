//! The whole foreign-function surface between this crate and the vendored
//! renderer (`vendor/xbased-hle`): the C functions Rust calls, and the C
//! functions Rust provides.
//!
//! What Rust provides is the renderer's host interface, `host.h` (about 38
//! `hle_host_*` / `hle_va_*` functions), plus four small hooks this tree adds
//! (`PATCHES.md`): the log sink, the window surface, and Android's property
//! getter the renderer's debug switches read.
//!
//! `host.h` has no context argument, so the functions find the one GPU of the
//! process through `shared()`. Before a GPU exists they answer as the C does
//! before boot (no memory, nothing dirty-tracked, registers read 0).

use std::ffi::{CStr, c_char, c_int, c_uint, c_void};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use crate::host::{GpuHost, SurfaceProvider, VkInstance, VkSurfaceKHR};
use crate::nv2a::{Clock, Decoder, Nv2a, lock};
use crate::os;
use crate::present::PresentShared;
use crate::{Fatal, FatalKind};

// ---------------------------------------------------------------- C -> Rust
// The renderer's functions this crate calls. Signatures are those of the
// vendored headers named beside each group.

/// `HleOvVert` (`vk/swapchain.h`): an overlay vertex in window pixels.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct HleOvVert {
    pub x: f32,
    pub y: f32,
    pub u: f32,
    pub v: f32,
    /// `R | G << 8 | B << 16 | A << 24`.
    pub rgba: u32,
}

/// `HleOvCmd` (`vk/swapchain.h`): a run of overlay triangles.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct HleOvCmd {
    pub tex: u32,
    pub blend: u32,
    pub clip: [i32; 4],
    pub first: u32,
    pub count: u32,
}

/// `HLE_OV_BLEND`: straight alpha.
pub(crate) const HLE_OV_BLEND: u32 = 0;
/// `HLE_UPSCALE_BILINEAR`.
pub(crate) const HLE_UPSCALE_BILINEAR: c_int = 0;

/// `HleDispStats` (`vk/display.h`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct HleDispStats {
    pub written: u64,
    pub presented: u64,
    pub dropped: u64,
    pub write_waits: u64,
}

/// `struct ANativeWindow` as `vk/native_window.h` defines it off Android:
/// the window the presenter draws into. Only its address (the window's
/// identity) and its size are used on Linux.
#[repr(C)]
pub(crate) struct NativeWindow {
    pub hinstance: *mut c_void,
    pub hwnd: *mut c_void,
    pub sdl_window: *mut c_void,
    /// `volatile int32_t` in C: written by the presenting thread.
    pub width: AtomicI32,
    pub height: AtomicI32,
}

/// The start of `HleRuntime` (`hle.h`): the posture, then the title's
/// fingerprint and the first fields of its parsed XBE (`core/xbe.h`). Only
/// these fields are read or written from Rust.
#[repr(C)]
pub(crate) struct HleRuntimeHead {
    pub posture: c_int,
    pub fp: u64,
    pub base: u32,
    pub sizeof_headers: u32,
    pub sizeof_image: u32,
    pub timedate: u32,
    pub entry: u32,
    pub kernel_thunk: u32,
    pub debug_keys: bool,
    pub plain_entry: bool,
    pub title_id: u32,
}

/// `HLE_POSTURE_REPLACE`: the renderer draws the titles.
pub(crate) const HLE_POSTURE_REPLACE: c_int = 2;

unsafe extern "C" {
    // hle.h, mem/guestmem.h
    pub(crate) static mut g_hle: HleRuntimeHead;
    pub(crate) static g_hle_unavailable: [c_char; 256];
    pub(crate) fn hle_guestmem_init(ram: *mut u8, size: u64, ram_addr_base: u64);
    pub(crate) fn hle_init();
    pub(crate) fn hle_title_start();
    pub(crate) fn hle_title_stop();
    pub(crate) fn hle_hitch_pause();

    // pb/pbmode.h, pb/pbdraw.h
    pub(crate) fn hle_pb_active() -> bool;
    pub(crate) fn hle_pb_rescale(timeout_ns: u64) -> bool;
    pub(crate) fn hle_pb_park(at_flip: bool, timeout_ns: u64) -> bool;
    pub(crate) fn hle_pb_unpark();
    pub(crate) fn hle_pb_state_bytes(out: *mut u8, cap: usize) -> usize;
    pub(crate) fn hle_pb_state_put(bytes: *const u8, len: usize) -> bool;
    fn hle_pb_kick();
    fn hle_pb_put();
    fn hle_pb_get();
    fn hle_pb_pushed(put: u32);
    fn hle_pb_subroutine_written(v: u32);
    fn hle_pb_gamma(rgb: *const [u8; 256]);
    fn hle_pb_flip_read();
    fn hle_pb_flipped(start: u32);
    fn hle_pb_overlay_changed(show: bool);
    fn hle_pb_display_start(pa: u32);
    fn hle_pb_vblank();
    pub(crate) fn hle_pb_display_mode(pitch: u32, xfmt: u32, hz: c_uint);
    pub(crate) fn hle_pb_display_fb(pa: u32);

    // mem/barrier.h, vk/back.h
    pub(crate) fn hle_mem_first_write(ram_addr: u64, size: c_uint);
    pub(crate) fn hle_mem_stale_access_rw(pa: u32, write: bool);
    pub(crate) fn hle_mem_stale_neighbour(lo: u32, hi: u32) -> c_int;
    pub(crate) fn hle_mem_host_wrote(pa: u32, len: u32);
    pub(crate) fn hle_barrier_unguard_range(pa: u32, len: u32);
    pub(crate) fn hle_barrier_freed(pa: u32, len: u32);
    pub(crate) fn hle_tl_land_freed(pa: u32, len: u32);
    pub(crate) fn hle_mem_write_fence(pa: u32, len: u32) -> u64;
    pub(crate) fn hle_mem_wait_fence(value: u64);
    pub(crate) fn hle_back_running() -> bool;
    pub(crate) fn hle_back_swap_count() -> u64;
    pub(crate) fn hle_back_save_caches();
    pub(crate) fn hle_tl_completed() -> u64;
    pub(crate) fn hle_tl_submitted() -> u64;

    // pb/pbdraw.h, vk/batch.h: the decoder's and the back end's draw counts
    pub(crate) fn hle_pbd_draw_counts(
        draws: *mut u64,
        ff: *mut u64,
        skipped: *mut u64,
        tex_defs: *mut u64,
    );
    pub(crate) fn hle_pbd_const_reuses() -> u64;
    pub(crate) fn hle_batch_stats(batches: *mut u64, merged: *mut u64);

    // vk/pipeasync.h, vk/display.h
    pub(crate) fn hle_pipe_set_options(r#async: bool, prebuild: bool, hybrid: bool);
    pub(crate) fn hle_pipe_progress(done: *mut u32, total: *mut u32);
    pub(crate) fn hle_disp_gamma_by_consumer(on: bool);
    pub(crate) fn hle_disp_stats() -> HleDispStats;
    pub(crate) fn hle_disp_fronts() -> u32;
    pub(crate) fn hle_disp_wait_front(seen: u32, ms: c_int) -> bool;

    // vk/swapchain.h
    pub(crate) fn hle_present_unchanged(win: *mut NativeWindow, key: u64) -> bool;
    pub(crate) fn hle_present_begin(win: *mut NativeWindow, w: *mut u32, h: *mut u32) -> bool;
    pub(crate) fn hle_present_frame(w: *mut u32, h: *mut u32) -> bool;
    pub(crate) fn hle_present_end(
        sx: f32,
        sy: f32,
        fxaa: bool,
        upscale: c_int,
        average: bool,
        key: u64,
    );
    pub(crate) fn hle_present_set_clip(cx: f32, cy: f32);
    pub(crate) fn hle_present_set_vsync(on: bool);
    pub(crate) fn hle_present_set_osd(fps: f32);
    pub(crate) fn hle_present_count() -> u64;
    pub(crate) fn hle_present_shot(path: *const c_char);
    pub(crate) fn hle_present_shot_end(path: *const c_char, why: *mut c_char, n: usize) -> c_int;
    pub(crate) fn hle_ov_tex_new(w: u32, h: u32) -> u32;
    pub(crate) fn hle_ov_tex_put(
        id: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
        rgba: *const c_void,
        stride: u32,
    );
    pub(crate) fn hle_ov_tex_free(id: u32);
    pub(crate) fn hle_present_set_overlay(
        v: *const HleOvVert,
        nv: u32,
        c: *const HleOvCmd,
        nc: u32,
        flags: u32,
    );
}

/// The push-buffer decoder of the vendored renderer.
pub(crate) struct CDecoder;

// SAFETY (all methods): these `hle_pb_*` functions take plain values, are
// documented in pb/pbmode.h and pb/pbdraw.h as callable from any thread, and
// do nothing while push-buffer mode is off. The register front end calls
// them only in push-buffer mode, which the renderer turns on itself after
// its decoder exists.
impl Decoder for CDecoder {
    fn kick(&self) {
        // SAFETY: see the impl.
        unsafe { hle_pb_kick() }
    }
    fn put(&self) {
        // SAFETY: see the impl.
        unsafe { hle_pb_put() }
    }
    fn get(&self) {
        // SAFETY: see the impl.
        unsafe { hle_pb_get() }
    }
    fn pushed(&self, put: u32) {
        // SAFETY: see the impl.
        unsafe { hle_pb_pushed(put) }
    }
    fn subroutine_written(&self, value: u32) {
        // SAFETY: see the impl.
        unsafe { hle_pb_subroutine_written(value) }
    }
    fn gamma(&self, rgb: &[[u8; 256]; 3]) {
        // SAFETY: `rgb` is the `const uint8_t[3][256]` the function copies
        // before it returns.
        unsafe { hle_pb_gamma(rgb.as_ptr()) }
    }
    fn flip_read(&self) {
        // SAFETY: see the impl.
        unsafe { hle_pb_flip_read() }
    }
    fn flipped(&self, start: u32) {
        // SAFETY: see the impl.
        unsafe { hle_pb_flipped(start) }
    }
    fn overlay_changed(&self, show: bool) {
        // SAFETY: see the impl.
        unsafe { hle_pb_overlay_changed(show) }
    }
    fn display_start(&self, pa: u32) {
        // SAFETY: see the impl.
        unsafe { hle_pb_display_start(pa) }
    }
    fn vblank(&self) {
        // SAFETY: see the impl.
        unsafe { hle_pb_vblank() }
    }
}

/// The host clock and the machine's guest clock.
pub(crate) struct HostClock(pub Arc<dyn GpuHost>);

impl Clock for HostClock {
    fn now_ns(&self) -> u64 {
        os::monotonic_ns()
    }
    fn guest_ns(&self) -> u64 {
        self.0.guest_time_ns()
    }
    fn guest_to_host_ns(&self, guest_ns: u64) -> u64 {
        self.0.guest_to_host_ns(guest_ns)
    }
    fn sleep_until(&self, host_ns: u64) {
        os::sleep_until_ns(host_ns);
    }
}

// ------------------------------------------------------------ shared state

/// `HleCpu` (`host.h`) with atomic fields of the same layout, so a snapshot
/// can be refreshed while the renderer's diagnostics read it.
#[repr(C)]
#[derive(Default)]
pub(crate) struct HleCpu {
    regs: [AtomicU32; 8],
    eip: AtomicU32,
    eflags: AtomicU32,
    fs_base: AtomicU32,
    halted: AtomicBool,
}

/// The names of the surface's instance extensions as C wants them.
pub(crate) struct ExtensionList(Vec<*const c_char>);

// SAFETY: the pointers refer to `&'static CStr` data, which is immutable and
// lives forever.
unsafe impl Send for ExtensionList {}
// SAFETY: as above.
unsafe impl Sync for ExtensionList {}

/// Everything the `host.h` functions reach. One per process, never freed:
/// the renderer's threads have no end (its render thread is detached and its
/// back end has no teardown), so they may call in at any time.
pub(crate) struct Shared {
    pub nv: Nv2a<CDecoder, HostClock>,
    pub host: Arc<dyn GpuHost>,
    pub surface: Option<Arc<dyn SurfaceProvider>>,
    extensions: ExtensionList,
    /// `hle_host_base_path`: ends with '/'.
    pub base_path: std::ffi::CString,
    pub render_scale: AtomicI32,
    pub anisotropy: AtomicI32,
    /// `hle_init` ran: the renderer's memory callbacks may be called.
    pub started: AtomicBool,
    /// The GPU was shut down: nothing is presented or forwarded any more.
    pub closed: AtomicBool,
    pub fatal_set: AtomicBool,
    pub fatal: Mutex<Option<Fatal>>,
    cpu: HleCpu,
    pub present: PresentShared,
    pub window: NativeWindow,
}

// SAFETY: `NativeWindow`'s raw pointers are never dereferenced (they stay
// null; the struct is an identity and a size for the C presenter), and every
// other field is `Send + Sync`.
unsafe impl Send for Shared {}
// SAFETY: as above.
unsafe impl Sync for Shared {}

static SHARED: AtomicPtr<Shared> = AtomicPtr::new(std::ptr::null_mut());

/// The process's GPU, once created.
#[inline]
pub(crate) fn shared() -> Option<&'static Shared> {
    // SAFETY: the pointer is null or a leaked `Box<Shared>` (`install`),
    // which is never freed or mutated except through its interior atomics
    // and locks.
    unsafe { SHARED.load(Ordering::Acquire).as_ref() }
}

impl Shared {
    /// Builds the shared state. Does not install it.
    pub(crate) fn new(
        nv: Nv2a<CDecoder, HostClock>,
        host: Arc<dyn GpuHost>,
        surface: Option<Arc<dyn SurfaceProvider>>,
        base_path: std::ffi::CString,
        render_scale: i32,
        anisotropy: i32,
        present: PresentShared,
    ) -> Self {
        let names = surface.as_ref().map(|s| s.instance_extensions()).unwrap_or_default();
        let extensions = ExtensionList(names.iter().map(|n| n.as_ptr()).collect());
        Self {
            nv,
            host,
            surface,
            extensions,
            base_path,
            render_scale: AtomicI32::new(render_scale),
            anisotropy: AtomicI32::new(anisotropy),
            started: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            fatal_set: AtomicBool::new(false),
            fatal: Mutex::new(None),
            cpu: HleCpu::default(),
            present,
            window: NativeWindow {
                hinstance: std::ptr::null_mut(),
                hwnd: std::ptr::null_mut(),
                sdl_window: std::ptr::null_mut(),
                width: AtomicI32::new(0),
                height: AtomicI32::new(0),
            },
        }
    }

    /// Makes `self` the process's GPU. Fails (returning it) if there is one.
    pub(crate) fn install(self: Box<Self>) -> Result<&'static Shared, Box<Self>> {
        let raw = Box::into_raw(self);
        match SHARED.compare_exchange(
            std::ptr::null_mut(),
            raw,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            // SAFETY: `raw` came from `Box::into_raw` just above and is now
            // leaked for the life of the process.
            Ok(_) => Ok(unsafe { &*raw }),
            // SAFETY: the exchange failed, so `raw` was not shared; take the
            // box back.
            Err(_) => Err(unsafe { Box::from_raw(raw) }),
        }
    }

    /// Records why the title cannot go on; the first reason is kept.
    pub(crate) fn set_fatal(&self, kind: FatalKind, text: String) {
        let mut f = lock(&self.fatal);
        if f.is_none() {
            log::error!(target: "gpu", "fatal: {text}");
            *f = Some(Fatal { kind, text });
            self.fatal_set.store(true, Ordering::Release);
        }
    }
}

/// `g_hle_unavailable` as text: why the renderer could not start, or empty.
pub(crate) fn unavailable_text() -> String {
    // SAFETY: `g_hle_unavailable` is a NUL-terminated char array the renderer
    // writes only while it starts (on the thread calling `hle_title_start`,
    // which is the caller here); reading at most its 256 bytes is in bounds.
    let bytes = unsafe { &*(&raw const g_hle_unavailable).cast::<[u8; 256]>() };
    let len = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..len]).into_owned()
}

// ---------------------------------------------------------------- Rust -> C
// host.h, in its order.

/// `hle_host_cpu`: the guest CPU's registers for diagnostics, or NULL.
#[unsafe(no_mangle)]
pub extern "C" fn hle_host_cpu() -> *mut c_void {
    let Some(s) = shared() else { return std::ptr::null_mut() };
    let Some(r) = s.host.cpu_regs() else { return std::ptr::null_mut() };
    for (dst, v) in s.cpu.regs.iter().zip(r.gpr) {
        dst.store(v, Ordering::Relaxed);
    }
    s.cpu.eip.store(r.eip, Ordering::Relaxed);
    s.cpu.eflags.store(r.eflags, Ordering::Relaxed);
    s.cpu.fs_base.store(r.fs_base, Ordering::Relaxed);
    s.cpu.halted.store(r.halted, Ordering::Relaxed);
    (&raw const s.cpu).cast_mut().cast()
}

/// `hle_host_kick`.
#[unsafe(no_mangle)]
pub extern "C" fn hle_host_kick() {
    if let Some(s) = shared() {
        s.host.kick();
    }
}

/// `hle_host_guest_hold`.
#[unsafe(no_mangle)]
pub extern "C" fn hle_host_guest_hold(on: bool) {
    if let Some(s) = shared() {
        s.host.guest_hold(on);
    }
}

/// `hle_host_code_changed`.
#[unsafe(no_mangle)]
pub extern "C" fn hle_host_code_changed() {
    if let Some(s) = shared() {
        s.host.code_changed();
    }
}

/// `hle_host_code_flushes`.
#[unsafe(no_mangle)]
pub extern "C" fn hle_host_code_flushes() -> c_uint {
    shared().map_or(0, |s| s.host.code_flushes())
}

/// `hle_va_rw`: guest memory by virtual address; 0, or -1 if unmapped.
///
/// # Safety
///
/// `buf` must be valid for `len` bytes, for writing when `write` is false.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hle_va_rw(va: u32, buf: *mut c_void, len: usize, write: bool) -> c_int {
    let Some(s) = shared() else { return -1 };
    if len == 0 {
        return 0;
    }
    let ok = if write {
        // SAFETY: the caller guarantees `buf` is readable for `len` bytes.
        s.host.va_write(va, unsafe { std::slice::from_raw_parts(buf.cast::<u8>(), len) })
    } else {
        // SAFETY: the caller guarantees `buf` is writable for `len` bytes and
        // not aliased during the call.
        s.host.va_read(va, unsafe { std::slice::from_raw_parts_mut(buf.cast::<u8>(), len) })
    };
    if ok { 0 } else { -1 }
}

/// `hle_va_store`.
#[unsafe(no_mangle)]
pub extern "C" fn hle_va_store(va: u32, len: u32) {
    if let Some(s) = shared() {
        s.host.va_store(va, len);
    }
}

/// `hle_host_mem_dirty`.
#[unsafe(no_mangle)]
pub extern "C" fn hle_host_mem_dirty(pa: u32) -> bool {
    shared().is_none_or(|s| s.host.mem_dirty(pa))
}

/// `hle_host_mem_open`.
#[unsafe(no_mangle)]
pub extern "C" fn hle_host_mem_open(pa: u32) -> bool {
    shared().is_none_or(|s| s.host.mem_open(pa))
}

/// `hle_host_mem_test_and_clear`.
#[unsafe(no_mangle)]
pub extern "C" fn hle_host_mem_test_and_clear(pa: u32, len: u32) -> bool {
    shared().is_some_and(|s| s.host.mem_test_and_clear(pa, len))
}

/// `hle_host_mem_set_dirty`.
#[unsafe(no_mangle)]
pub extern "C" fn hle_host_mem_set_dirty(pa: u32, len: u32) {
    if let Some(s) = shared() {
        s.host.mem_set_dirty(pa, len);
    }
}

/// `hle_host_mem_landed`.
#[unsafe(no_mangle)]
pub extern "C" fn hle_host_mem_landed(pa: u32, len: u32) {
    if let Some(s) = shared() {
        s.host.mem_landed(pa, len);
    }
}

/// `hle_host_mem_rearm`.
#[unsafe(no_mangle)]
pub extern "C" fn hle_host_mem_rearm(pa: u32, len: u32) {
    if let Some(s) = shared() {
        s.host.mem_rearm(pa, len);
    }
}

/// `hle_host_mem_noaccess`.
#[unsafe(no_mangle)]
pub extern "C" fn hle_host_mem_noaccess(pa: u32, len: u32, on: bool) {
    if let Some(s) = shared() {
        s.host.mem_noaccess(pa, len, on);
    }
}

/// `hle_host_vblank_count`.
#[unsafe(no_mangle)]
pub extern "C" fn hle_host_vblank_count() -> u64 {
    shared().map_or(0, |s| s.nv.vblank_count())
}

/// `hle_host_vblank_period_ns`.
#[unsafe(no_mangle)]
pub extern "C" fn hle_host_vblank_period_ns() -> i64 {
    shared().map_or(crate::nv2a::FRAME_NS_60 as i64, |s| s.nv.vblank_period_ns() as i64)
}

/// `hle_host_vblank_ns`.
#[unsafe(no_mangle)]
pub extern "C" fn hle_host_vblank_ns() -> u64 {
    shared().map_or(0, |s| s.nv.vblank_ns())
}

/// `hle_host_flip_vblank_ns`.
#[unsafe(no_mangle)]
pub extern "C" fn hle_host_flip_vblank_ns() -> u64 {
    shared().map_or(0, |s| s.nv.flip_vblank_ns())
}

/// `hle_host_frame_wait`.
#[unsafe(no_mangle)]
pub extern "C" fn hle_host_frame_wait() {
    if let Some(s) = shared() {
        s.host.frame_wait();
    }
}

/// `hle_host_frame_flipped`.
#[unsafe(no_mangle)]
pub extern "C" fn hle_host_frame_flipped() {
    if let Some(s) = shared() {
        s.host.frame_flipped();
    }
}

/// `hle_host_flip_min_ns`.
#[unsafe(no_mangle)]
pub extern "C" fn hle_host_flip_min_ns() -> i64 {
    shared().map_or(crate::nv2a::FRAME_NS_60 as i64, |s| s.nv.flip_min_ns())
}

/// `hle_host_flip_slot`.
#[unsafe(no_mangle)]
pub extern "C" fn hle_host_flip_slot(want: i64, every: i64) -> i64 {
    shared().map_or(want, |s| s.nv.flip_slot(want, every))
}

/// `hle_host_display_sync`.
#[unsafe(no_mangle)]
pub extern "C" fn hle_host_display_sync(vsync_ns: u64, period_ns: u64, lead_ns: u64) {
    if let Some(s) = shared() {
        s.nv.display_sync(vsync_ns, period_ns, lead_ns);
    }
}

/// `hle_host_vblank_timed`.
#[unsafe(no_mangle)]
pub extern "C" fn hle_host_vblank_timed() -> bool {
    shared().is_some_and(|s| s.nv.vblank_timed())
}

/// `hle_host_gpu_reg`.
#[unsafe(no_mangle)]
pub extern "C" fn hle_host_gpu_reg(off: u32) -> u32 {
    shared().map_or(0, |s| s.nv.gpu_reg(off))
}

/// `hle_host_gpu_reg_set`.
#[unsafe(no_mangle)]
pub extern "C" fn hle_host_gpu_reg_set(off: u32, v: u32) {
    if let Some(s) = shared() {
        s.nv.gpu_reg_set(off, v);
    }
}

/// `hle_host_gpu_reg_mask`.
#[unsafe(no_mangle)]
pub extern "C" fn hle_host_gpu_reg_mask(off: u32, mask: u32, bits: u32) {
    if let Some(s) = shared() {
        s.nv.gpu_reg_mask(off, mask, bits);
    }
}

/// `hle_host_gpu_irq_update`.
#[unsafe(no_mangle)]
pub extern "C" fn hle_host_gpu_irq_update() {
    if let Some(s) = shared() {
        s.nv.update_irq();
    }
}

/// `hle_host_pb_mode`.
#[unsafe(no_mangle)]
pub extern "C" fn hle_host_pb_mode(on: bool) {
    if let Some(s) = shared() {
        s.nv.set_pb_mode(on);
    }
}

/// `hle_host_gpu_lost`: the Vulkan device is gone; nothing more is drawn.
///
/// # Safety
///
/// `why` must be NULL or a NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hle_host_gpu_lost(why: *const c_char) {
    // SAFETY: the caller passes NULL or a NUL-terminated string.
    let text = unsafe { cstr_lossy(why) };
    if let Some(s) = shared() {
        s.set_fatal(FatalKind::GpuLost, format!("the GPU {text}"));
    }
}

/// `hle_host_base_path`: where the renderer keeps its caches; ends with '/'.
#[unsafe(no_mangle)]
pub extern "C" fn hle_host_base_path() -> *const c_char {
    shared().map_or(c"./".as_ptr(), |s| s.base_path.as_ptr())
}

/// `hle_host_render_scale`.
#[unsafe(no_mangle)]
pub extern "C" fn hle_host_render_scale() -> c_int {
    shared().map_or(1, |s| s.render_scale.load(Ordering::Relaxed))
}

/// `hle_host_anisotropy`.
#[unsafe(no_mangle)]
pub extern "C" fn hle_host_anisotropy() -> c_int {
    shared().map_or(0, |s| s.anisotropy.load(Ordering::Relaxed))
}

/// `hle_host_vk_loader`: the Vulkan loader where volk would not find it
/// (`os::vk_loader`), else NULL, and volk loads the system's. A custom
/// driver (Android's Turnip) would go here.
#[unsafe(no_mangle)]
pub extern "C" fn hle_host_vk_loader() -> *mut c_void {
    os::vk_loader()
}

/// `hle_host_disc_stats`.
///
/// # Safety
///
/// The three pointers must be valid for writing a `uint64_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hle_host_disc_stats(ops: *mut u64, bytes: *mut u64, ns: *mut u64) -> bool {
    let Some(d) = shared().and_then(|s| s.host.disc_stats()) else { return false };
    // SAFETY: the caller guarantees the pointers are valid for writes.
    unsafe {
        *ops = d.ops;
        *bytes = d.bytes;
        *ns = d.ns;
    }
    true
}

// ---- the hooks this tree adds (vendor/xbased-hle/PATCHES.md) ----

/// The renderer's log lines (`hle_log` in `hle.c`), sent to the `log` crate
/// under the target `gpu`.
///
/// # Safety
///
/// `tag` and `line` must be NULL or NUL-terminated strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hle_host_log(tag: *const c_char, line: *const c_char) {
    if !log::log_enabled!(target: "gpu", log::Level::Info) {
        return;
    }
    // SAFETY: the caller passes NULL or NUL-terminated strings.
    let (tag, line) = unsafe { (cstr_lossy(tag), cstr_lossy(line)) };
    let tag = tag.strip_prefix("xbased-").unwrap_or(&tag);
    let level = if line.starts_with("FATAL") {
        log::Level::Error
    } else if tag == "hle-pipe"
        && line.starts_with("pipeline ")
        && !line.starts_with("pipeline cache")
    {
        // One line per pipeline built: mechanism, not the session's story.
        log::Level::Debug
    } else {
        log::Level::Info
    };
    log::log!(target: "gpu", level, "{tag}: {line}");
}

/// The instance extensions the window's surface needs, for
/// `vkCreateInstance` (`vk/vkdev.c`); NULL and 0 without a window.
///
/// # Safety
///
/// `count` must be valid for writing a `uint32_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hle_host_surface_extensions(count: *mut u32) -> *const *const c_char {
    let list = shared().map(|s| &s.extensions.0).filter(|l| !l.is_empty());
    // SAFETY: the caller guarantees `count` is valid for a write.
    unsafe { *count = list.map_or(0, |l| l.len() as u32) };
    list.map_or(std::ptr::null(), |l| l.as_ptr())
}

/// Creates the window's surface on the renderer's instance
/// (`vk/swapchain.c`, in place of `SDL_Vulkan_CreateSurface`).
///
/// # Safety
///
/// `inst` must be the renderer's live `VkInstance` and `surf` valid for
/// writing a `VkSurfaceKHR`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hle_host_surface_create(inst: *mut c_void, surf: *mut c_void) -> bool {
    let Some(provider) = shared().and_then(|s| s.surface.as_ref()) else { return false };
    let instance: VkInstance = inst;
    // SAFETY: `instance` is the renderer's live instance, created with the
    // provider's extensions (the caller's contract).
    let Some(surface) = (unsafe { provider.create_surface(instance) }) else { return false };
    // SAFETY: `surf` points at a `VkSurfaceKHR`, a 64-bit non-dispatchable
    // handle on every target (the caller's contract).
    unsafe { surf.cast::<VkSurfaceKHR>().write(surface) };
    true
}

/// Android's `__system_property_get`, which the renderer's debug switches
/// (`debug.xbased.*`, `prop.h`) read. Off Android a property is the
/// environment variable of the same name in capitals with `_` for `.`
/// (`debug.xbased.caps` is `DEBUG_XBASED_CAPS`), as in the reference; unset
/// reads as empty.
///
/// # Safety
///
/// `name` must be a NUL-terminated string and `value` valid for writing
/// `PROP_VALUE_MAX` (92) bytes.
#[cfg(not(target_os = "android"))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __system_property_get(name: *const c_char, value: *mut c_char) -> c_int {
    const PROP_VALUE_MAX: usize = 92;
    // SAFETY: the caller guarantees `value` is writable.
    unsafe { *value = 0 };
    // SAFETY: the caller guarantees `name` is NUL-terminated.
    let name = unsafe { CStr::from_ptr(name) }.to_bytes();
    let mut env = [0u8; 72];
    if name.len() >= env.len() {
        return 0;
    }
    for (dst, &b) in env.iter_mut().zip(name) {
        *dst = if b == b'.' { b'_' } else { b.to_ascii_uppercase() };
    }
    // SAFETY: `env` is NUL-terminated (it is longer than `name`). `getenv`
    // returns NULL or a NUL-terminated string that stays valid while the
    // environment is not modified; this process does not modify it.
    let v = unsafe { libc::getenv(env.as_ptr().cast()) };
    if v.is_null() {
        return 0;
    }
    // SAFETY: `v` is NUL-terminated (see above).
    let v = unsafe { CStr::from_ptr(v) }.to_bytes();
    let n = v.len().min(PROP_VALUE_MAX - 1);
    // SAFETY: `value` holds PROP_VALUE_MAX bytes (the caller's contract) and
    // `n + 1` is at most that.
    unsafe {
        std::ptr::copy_nonoverlapping(v.as_ptr(), value.cast::<u8>(), n);
        *value.add(n) = 0;
    }
    n as c_int
}

/// A C string as text; empty for NULL.
///
/// # Safety
///
/// `p` must be NULL or point at a NUL-terminated string that outlives the
/// returned value.
unsafe fn cstr_lossy<'a>(p: *const c_char) -> std::borrow::Cow<'a, str> {
    if p.is_null() {
        return std::borrow::Cow::Borrowed("");
    }
    // SAFETY: non-null and NUL-terminated per the caller's contract.
    unsafe { CStr::from_ptr(p) }.to_string_lossy()
}

/// Every entry point this module exports, so a test binary can prove they
/// link (the symbols exist with these signatures in the final image).
#[doc(hidden)]
pub fn exported_symbols() -> Vec<(&'static str, usize)> {
    vec![
        ("hle_host_cpu", hle_host_cpu as *const () as usize),
        ("hle_host_kick", hle_host_kick as *const () as usize),
        ("hle_host_guest_hold", hle_host_guest_hold as *const () as usize),
        ("hle_host_code_changed", hle_host_code_changed as *const () as usize),
        ("hle_host_code_flushes", hle_host_code_flushes as *const () as usize),
        ("hle_va_rw", hle_va_rw as *const () as usize),
        ("hle_va_store", hle_va_store as *const () as usize),
        ("hle_host_mem_dirty", hle_host_mem_dirty as *const () as usize),
        ("hle_host_mem_open", hle_host_mem_open as *const () as usize),
        ("hle_host_mem_test_and_clear", hle_host_mem_test_and_clear as *const () as usize),
        ("hle_host_mem_set_dirty", hle_host_mem_set_dirty as *const () as usize),
        ("hle_host_mem_landed", hle_host_mem_landed as *const () as usize),
        ("hle_host_mem_rearm", hle_host_mem_rearm as *const () as usize),
        ("hle_host_mem_noaccess", hle_host_mem_noaccess as *const () as usize),
        ("hle_host_vblank_count", hle_host_vblank_count as *const () as usize),
        ("hle_host_vblank_period_ns", hle_host_vblank_period_ns as *const () as usize),
        ("hle_host_vblank_ns", hle_host_vblank_ns as *const () as usize),
        ("hle_host_flip_vblank_ns", hle_host_flip_vblank_ns as *const () as usize),
        ("hle_host_frame_wait", hle_host_frame_wait as *const () as usize),
        ("hle_host_frame_flipped", hle_host_frame_flipped as *const () as usize),
        ("hle_host_flip_min_ns", hle_host_flip_min_ns as *const () as usize),
        ("hle_host_flip_slot", hle_host_flip_slot as *const () as usize),
        ("hle_host_display_sync", hle_host_display_sync as *const () as usize),
        ("hle_host_vblank_timed", hle_host_vblank_timed as *const () as usize),
        ("hle_host_gpu_reg", hle_host_gpu_reg as *const () as usize),
        ("hle_host_gpu_reg_set", hle_host_gpu_reg_set as *const () as usize),
        ("hle_host_gpu_reg_mask", hle_host_gpu_reg_mask as *const () as usize),
        ("hle_host_gpu_irq_update", hle_host_gpu_irq_update as *const () as usize),
        ("hle_host_pb_mode", hle_host_pb_mode as *const () as usize),
        ("hle_host_gpu_lost", hle_host_gpu_lost as *const () as usize),
        ("hle_host_base_path", hle_host_base_path as *const () as usize),
        ("hle_host_render_scale", hle_host_render_scale as *const () as usize),
        ("hle_host_anisotropy", hle_host_anisotropy as *const () as usize),
        ("hle_host_vk_loader", hle_host_vk_loader as *const () as usize),
        ("hle_host_disc_stats", hle_host_disc_stats as *const () as usize),
        ("hle_host_log", hle_host_log as *const () as usize),
        ("hle_host_surface_extensions", hle_host_surface_extensions as *const () as usize),
        ("hle_host_surface_create", hle_host_surface_create as *const () as usize),
        ("hle_init", hle_init as *const () as usize),
        ("hle_title_start", hle_title_start as *const () as usize),
        ("hle_present_begin", hle_present_begin as *const () as usize),
        ("hle_present_end", hle_present_end as *const () as usize),
        ("hle_present_set_overlay", hle_present_set_overlay as *const () as usize),
        ("hle_mem_first_write", hle_mem_first_write as *const () as usize),
        ("hle_mem_stale_access_rw", hle_mem_stale_access_rw as *const () as usize),
    ]
}
