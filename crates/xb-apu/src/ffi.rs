//! The boundary with the vendored C (`vendor/xbased-apu`): every `extern`
//! declaration, every function the C calls back, and all of the crate's
//! `unsafe`.
//!
//! The C is built unmodified. It reaches the machine through a dozen
//! functions; those are defined here (the build renames them with an
//! `xb_apu_` prefix, see `build.rs`):
//!
//! | C name | Here | Does |
//! |---|---|---|
//! | `nc_log` (via `shim/apu_shim.c`) | [`xb_apu_log`] | a log line |
//! | `nc_irq_set` | [`xb_apu_irq_set`] | the interrupt line |
//! | `nc_ke_now_ns` | [`xb_apu_guest_now_ns`] | the guest's clock |
//! | `hle_host_mem_set_dirty` | [`xb_apu_ram_written`] | a write to guest RAM |
//! | `nc_mmio_register`, `nc_mmio_retry` | [`xb_apu_mmio_register`], [`xb_apu_mmio_retry`] | the register window |
//! | `nc_paused`, `nc_pause_wait`, `nc_pause_register`, `nc_pause_idle` | `xb_apu_pause*` | holding the frame thread |
//! | `apu_out_*`, `g_apu_out_channels`, `g_apu_frame6` | same names | the output (replaces `out.c`) |
//!
//! The C keeps its state in globals (`g_state`, `g_apu_ram`, statics in the
//! voice processor), so there is one APU per process at a time; [`create`]
//! refuses a second. The callbacks find their Rust state through [`HOST`].

use std::cell::Cell;
use std::ffi::{CStr, c_char, c_int, c_uint, c_void};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, RwLock};
use std::time::Duration;

use xb_types::irq::{Irq, IrqHub};

use crate::sink::{AudioSink, BLOCK_FRAMES, Channels, DrainSignal, SAMPLE_RATE, StreamSpec};
use crate::{ApuError, ApuHost, ApuOptions, MMIO_LEN};

/// The PIC line of the APU: the C's `MCPX_APU_IRQ` (`apu_int.h`).
const C_IRQ_LINE: c_uint = 5;

/// How long a pause or a stop ([`Host::set_paused`]) waits for the frame thread to park. It checks at
/// least once per EP frame (5.33 ms) unless a guest voice lock is contended.
const HALT_TIMEOUT: Duration = Duration::from_secs(2);

/// `NcVm` as `shim/include/native/core/vm.h` declares it.
#[repr(C)]
struct NcVm {
    phys: *mut u8,
    ram_size: u64,
}

type MmioRead = unsafe extern "C" fn(opaque: *mut c_void, off: u32, size: c_uint) -> u64;
type MmioWrite = unsafe extern "C" fn(opaque: *mut c_void, off: u32, value: u64, size: c_uint);

/// `NcMmioDev` as `shim/include/native/core/mmio.h` declares it.
#[repr(C)]
pub struct NcMmioDev {
    name: *const c_char,
    base: u32,
    size: u32,
    read: Option<MmioRead>,
    write: Option<MmioWrite>,
    opaque: *mut c_void,
}

/// `ApuConfig` (apu_base.h).
#[repr(C)]
struct ApuConfig {
    use_dsp: bool,
    hrtf: bool,
    volume_limit: f32,
    num_workers: c_int,
}

unsafe extern "C" {
    /// apu.c: allocates the device, opens the output, registers the window
    /// and starts the frame thread.
    fn nc_dev_apu_init(vm: *mut NcVm);
    /// apu.c: boosts the GP and waits up to about a millisecond for `*word`
    /// to read zero.
    fn nc_dev_apu_spin_wait(word: *const u32);
    /// apu.c: moves the sample counter on by time the guest's clock skipped.
    fn nc_dev_apu_time_held(ns: u64);
    /// apu.c: the 3D voices' HRTF filters, read every frame.
    fn nc_apu_set_hrtf(on: bool);
    /// apu.c: registers and voice state cleared (takes the device's lock).
    fn mcpx_apu_reset(d: *mut c_void);
    /// gp_ep.c: chooses which stage's output is heard.
    fn mcpx_apu_update_dsp_preference(d: *mut c_void);
    /// shim/apu_shim.c.
    fn xb_apu_shim_request_exit();
    fn xb_apu_shim_destroy();
    fn xb_apu_shim_kick();
    fn xb_apu_shim_state_save(len: *mut usize) -> *mut u8;
    fn xb_apu_shim_state_free(p: *mut u8);
    fn xb_apu_shim_state_load(p: *const u8, len: usize) -> bool;

    /// apu.c: the one device.
    static mut g_state: *mut c_void;
    /// apu.c: the settings, read at init and (HRTF) every frame.
    static mut g_apu_config: ApuConfig;
}

#[cfg(xb_apu_recompiler)]
unsafe extern "C" {
    /// dsp_rec.cpp: random DSP programs on the recompiler and the
    /// interpreter, compared word for word.
    fn dsp_rec_selftest(programs: c_int, seed: u32) -> c_int;
}

/// The handlers the C registered for the register window.
struct Mmio {
    read: MmioRead,
    write: MmioWrite,
    opaque: *mut c_void,
}

/// Why the frame thread is, or is not, allowed to run.
#[derive(Default)]
struct Gate {
    started: bool,
    paused: bool,
    /// Held for the duration of a reset.
    resetting: bool,
    exiting: bool,
    /// The frame thread sits in [`xb_apu_pause_wait`].
    parked: bool,
}

/// The Rust side of the one APU: what the C's callbacks need.
pub(crate) struct Host {
    irq: Arc<IrqHub>,
    machine: Arc<dyn ApuHost>,
    sink: Arc<dyn AudioSink>,
    channels: Channels,
    ram: *mut u8,
    ram_len: u32,
    gate: Mutex<Gate>,
    gate_changed: Condvar,
    /// [`Gate::halted`], for the C's `nc_paused()` (read every frame).
    halted: AtomicBool,
    /// The output gain as `f32` bits.
    gain: AtomicU32,
    mmio: OnceLock<Mmio>,
    open_error: Mutex<Option<String>>,
    blocks_pushed: AtomicU64,
}

// SAFETY: `ram` and `Mmio::opaque` are the only non-`Send`/`Sync` fields.
// `ram` points at guest RAM, which `create`'s caller guarantees stays mapped
// and may be accessed from any thread (it is shared with the running guest
// by design). `opaque` is the C's device state, which the C itself shares
// between the guest-CPU thread and its frame thread under its own locks.
unsafe impl Send for Host {}
// SAFETY: as above; every other field is `Sync`.
unsafe impl Sync for Host {}

/// The live APU's [`Host`], null when there is none. Set before the C is
/// initialized and cleared after its threads are joined, so a callback from
/// the C always finds it.
static HOST: AtomicPtr<Host> = AtomicPtr::new(ptr::null_mut());

/// Taken by the one live APU.
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Whether `g_state` may be kicked: a [`DrainSignal`] can outlive its APU and
/// is raised from audio threads at any time.
static KICKABLE: RwLock<bool> = RwLock::new(false);

thread_local! {
    /// This thread is inside [`Host::try_mmio_write`]: a write may be given up.
    static RETRY_ALLOWED: Cell<bool> = const { Cell::new(false) };
    /// The C gave the write up ([`xb_apu_mmio_retry`]).
    static RETRY_ASKED: Cell<bool> = const { Cell::new(false) };
}

fn host() -> Option<&'static Host> {
    // SAFETY: HOST is either null or points into the `Arc<Host>` that
    // `create` returned; `destroy` clears it only after the C's threads are
    // joined, and every other caller is a C function reached through a live
    // `&Host`. The reference therefore never outlives the allocation it is
    // used within, although its lifetime is spelled `'static`.
    unsafe { HOST.load(Ordering::Acquire).as_ref() }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Wakes the frame thread's waits (the output drained, a pause was asked).
fn kick() {
    if let Ok(alive) = KICKABLE.read()
        && *alive
    {
        // SAFETY: KICKABLE is true only between the end of `create` and the
        // start of `destroy`, while `g_state` and its condition variable
        // exist; `destroy` takes the write lock before freeing them.
        unsafe { xb_apu_shim_kick() };
    }
}

/// The device's state as the vendored `state.c` writes it, or `None` without
/// a device. The frame thread is held off inside the call.
pub(crate) fn state_save() -> Option<Vec<u8>> {
    let alive = KICKABLE.read().ok()?;
    if !*alive {
        return None;
    }
    let mut len = 0usize;
    // SAFETY: the device exists while KICKABLE is true and the read lock is
    // held; the shim returns a malloc'd buffer of `len` bytes or NULL.
    let p = unsafe { xb_apu_shim_state_save(&raw mut len) };
    if p.is_null() {
        return None;
    }
    // SAFETY: `p` is valid for `len` bytes until freed just below.
    let bytes = unsafe { std::slice::from_raw_parts(p, len) }.to_vec();
    // SAFETY: `p` came from the shim's allocation and is freed once.
    unsafe { xb_apu_shim_state_free(p) };
    Some(bytes)
}

/// Replaces the device's state with bytes [`state_save`] made. False when
/// they are not a state this build reads (the device is then as after a
/// reset) or there is no device. Call with the frame thread paused.
pub(crate) fn state_load(bytes: &[u8]) -> bool {
    let Ok(alive) = KICKABLE.read() else { return false };
    // SAFETY: the device exists while KICKABLE is true and the read lock is
    // held; the shim reads `bytes.len()` bytes at most.
    *alive && unsafe { xb_apu_shim_state_load(bytes.as_ptr(), bytes.len()) }
}

/// Creates the one APU over guest RAM `[ram, ram + ram_len)`. The frame
/// thread exists but is held until [`Host::set_started`].
///
/// # Safety
/// `ram` must be valid for reads and writes of `ram_len` bytes, from any
/// thread, until the returned `Host` has been passed to [`destroy`].
pub(crate) unsafe fn create(
    ram: *mut u8,
    ram_len: u32,
    irq: Arc<IrqHub>,
    machine: Arc<dyn ApuHost>,
    sink: Arc<dyn AudioSink>,
    options: &ApuOptions,
) -> Result<Arc<Host>, ApuError> {
    if ACTIVE.swap(true, Ordering::AcqRel) {
        return Err(ApuError::AlreadyExists);
    }
    let host = Arc::new(Host {
        irq,
        machine,
        sink,
        channels: options.channels,
        ram,
        ram_len,
        gate: Mutex::new(Gate::default()),
        gate_changed: Condvar::new(),
        halted: AtomicBool::new(true),
        gain: AtomicU32::new(1.0f32.to_bits()),
        mmio: OnceLock::new(),
        open_error: Mutex::new(None),
        blocks_pushed: AtomicU64::new(0),
    });
    host.set_gain(options.volume, options.muted);
    HOST.store(Arc::as_ptr(&host).cast_mut(), Ordering::Release);

    let mut vm = NcVm { phys: ram, ram_size: u64::from(ram_len) };
    // SAFETY: no C thread exists yet (ACTIVE was clear), so the globals are
    // ours to set. `nc_dev_apu_init` only reads `vm` during the call. Its
    // callbacks find `HOST`, set above. The caller vouches for `ram`.
    unsafe {
        (&raw mut g_apu_out_channels).write(options.channels.count() as c_int);
        (&raw mut g_apu_config).write(ApuConfig {
            use_dsp: options.full_dsp,
            hrtf: options.hrtf,
            volume_limit: 1.0,
            num_workers: c_int::from(options.voice_workers),
        });
        nc_dev_apu_init(&raw mut vm);
    }

    // The frame thread goes straight to its pause wait (`halted` is set);
    // nothing below may run beside it.
    let parked = host.wait_parked();
    let error = lock(&host.open_error).take();
    let failed = if let Some(why) = error {
        Some(ApuError::Sink(why))
    } else if host.mmio.get().is_none() || !parked {
        Some(ApuError::Init)
    } else {
        None
    };
    if let Some(e) = failed {
        destroy(&host);
        return Err(e);
    }

    // SAFETY: the frame thread is parked and no register access can come yet,
    // so nothing else touches the device. gp_ep.c remembers the preference it
    // last applied in a function static; a device created after another was
    // destroyed would keep the monitor point its calloc left (none) when the
    // preference is the same as the old device's last. Applying the opposite
    // setting and then the real one makes the second call a change, whatever
    // the static holds.
    unsafe {
        let d = (&raw const g_state).read();
        let want = (&raw const g_apu_config.use_dsp).read();
        (&raw mut g_apu_config.use_dsp).write(!want);
        mcpx_apu_update_dsp_preference(d);
        (&raw mut g_apu_config.use_dsp).write(want);
        mcpx_apu_update_dsp_preference(d);
    }

    if let Ok(mut alive) = KICKABLE.write() {
        *alive = true;
    }
    Ok(host)
}

/// Stops the frame thread and frees the C's device. `host` must be the value
/// [`create`] returned; a second call does nothing.
pub(crate) fn destroy(host: &Arc<Host>) {
    if !ptr::eq(HOST.load(Ordering::Acquire), Arc::as_ptr(host)) {
        return;
    }
    if let Ok(mut alive) = KICKABLE.write() {
        *alive = false;
    }
    // SAFETY: `g_state` is the device `create` made (HOST still names this
    // host). The frame thread is first told to exit, then released from its
    // pause wait; `xb_apu_shim_destroy` joins it before freeing anything.
    unsafe { xb_apu_shim_request_exit() };
    {
        let mut gate = lock(&host.gate);
        gate.exiting = true;
        host.gate_changed.notify_all();
    }
    // SAFETY: see above; no register access can run concurrently because
    // those borrow the `Apu` this is being dropped from.
    unsafe { xb_apu_shim_destroy() };
    host.irq.lower(Irq::Apu);
    HOST.store(ptr::null_mut(), Ordering::Release);
    ACTIVE.store(false, Ordering::Release);
}

impl Gate {
    fn halted(&self) -> bool {
        !self.started || self.paused || self.resetting
    }
}

impl Host {
    /// A register read at `off` in the window.
    pub(crate) fn mmio_read(&self, off: u32, size: u8) -> u32 {
        let Some(mmio) = self.mmio.get() else { return 0 };
        if !matches!(size, 1 | 2 | 4)
            || off.checked_add(u32::from(size)).is_none_or(|end| end > MMIO_LEN)
        {
            return 0;
        }
        // SAFETY: the handler and its opaque pointer are the C's own pair,
        // alive as long as this `Host` is the live one; the C bounds-checks
        // `off` against each register file and takes its own locks.
        let v = unsafe { (mmio.read)(mmio.opaque, off, c_uint::from(size)) };
        let mask = if size == 4 { u64::from(u32::MAX) } else { (1u64 << (8 * size)) - 1 };
        (v & mask) as u32
    }

    /// A register write. Returns `false` if the C gave it up for the caller
    /// to run again, which it does only when `may_retry`.
    pub(crate) fn mmio_write(&self, off: u32, size: u8, value: u32, may_retry: bool) -> bool {
        let Some(mmio) = self.mmio.get() else { return true };
        if !matches!(size, 1 | 2 | 4)
            || off.checked_add(u32::from(size)).is_none_or(|end| end > MMIO_LEN)
        {
            return true;
        }
        RETRY_ALLOWED.set(may_retry);
        RETRY_ASKED.set(false);
        // SAFETY: as in `mmio_read`.
        unsafe { (mmio.write)(mmio.opaque, off, u64::from(value), c_uint::from(size)) };
        RETRY_ALLOWED.set(false);
        !RETRY_ASKED.replace(false)
    }

    /// Lets the frame thread run (`true`) or holds it (`false`).
    pub(crate) fn set_started(&self, started: bool) -> bool {
        self.change_gate(|g| g.started = started)
    }

    /// Holds the frame thread (`true`) or lets it go on (`false`).
    pub(crate) fn set_paused(&self, paused: bool) -> bool {
        self.change_gate(|g| g.paused = paused)
    }

    /// Holds the frame thread around [`Host::reset`].
    pub(crate) fn set_resetting(&self, resetting: bool) -> bool {
        self.change_gate(|g| g.resetting = resetting)
    }

    /// Whether the frame thread is allowed to run.
    pub(crate) fn running(&self) -> bool {
        !self.halted.load(Ordering::Acquire)
    }

    /// Applies a change to the gate. Going to halted, returns once the frame
    /// thread is parked (`false` if it did not park in time); going to
    /// running, tells the sink first and returns at once.
    fn change_gate(&self, change: impl FnOnce(&mut Gate)) -> bool {
        let halted = {
            let mut gate = lock(&self.gate);
            change(&mut gate);
            let halted = gate.halted();
            if !halted {
                // before the frame thread can push again
                self.sink.set_paused(false);
            }
            self.halted.store(halted, Ordering::Release);
            self.gate_changed.notify_all();
            halted
        };
        if !halted {
            return true;
        }
        kick();
        let parked = self.wait_parked();
        self.sink.set_paused(true);
        parked
    }

    /// Waits for the frame thread to reach its pause wait.
    fn wait_parked(&self) -> bool {
        let gate = lock(&self.gate);
        let (gate, timeout) = self
            .gate_changed
            .wait_timeout_while(gate, HALT_TIMEOUT, |g| !g.parked && !g.exiting)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        drop(gate);
        if timeout.timed_out() {
            log::warn!(target: "apu", "the frame thread did not stop within {HALT_TIMEOUT:?}");
        }
        !timeout.timed_out()
    }

    /// Clears the registers and the voice processor's state. The frame
    /// thread must be parked (the caller halts it).
    pub(crate) fn reset(&self) {
        // SAFETY: `g_state` is the live device; `mcpx_apu_reset` takes its
        // lock, which the parked frame thread does not hold.
        unsafe { mcpx_apu_reset((&raw const g_state).read()) };
        self.irq.lower(Irq::Apu);
    }

    /// The output gain: `volume` in 0..=1, or silence when `muted`.
    pub(crate) fn set_gain(&self, volume: f32, muted: bool) {
        let gain = if muted || volume.is_nan() { 0.0 } else { volume.clamp(0.0, 1.0) };
        self.gain.store(gain.to_bits(), Ordering::Relaxed);
    }

    /// The 3D voices' HRTF filters on or off; applies from the next frame.
    pub(crate) fn set_hrtf(&self, on: bool) {
        // SAFETY: an atomic store into `g_apu_config.hrtf`.
        unsafe { nc_apu_set_hrtf(on) };
    }

    /// The DirectSound mailbox wait: GP frames run to completion for a few
    /// milliseconds, and this waits up to about a millisecond for the word at
    /// physical `word_pa` to clear. Returns whether it reads zero.
    pub(crate) fn mailbox_wait(&self, word_pa: u32) -> bool {
        if !word_pa.is_multiple_of(4) || word_pa.checked_add(4).is_none_or(|end| end > self.ram_len)
        {
            return false;
        }
        // SAFETY: `[word_pa, word_pa + 4)` is inside guest RAM (checked) and
        // 4-aligned (checked; `GuestRam` is); the C only does atomic loads of it.
        unsafe {
            let word = self.ram.add(word_pa as usize).cast::<u32>();
            nc_dev_apu_spin_wait(word);
            (*word.cast::<AtomicU32>()).load(Ordering::Acquire) == 0
        }
    }

    /// The guest's clock skipped `ns` while the APU played on.
    pub(crate) fn time_held(&self, ns: u64) {
        // SAFETY: adjusts one register of the live device with atomic stores.
        unsafe { nc_dev_apu_time_held(ns) };
    }

    /// Blocks of [`BLOCK_FRAMES`] sample frames pushed to the sink so far.
    pub(crate) fn blocks_pushed(&self) -> u64 {
        self.blocks_pushed.load(Ordering::Relaxed)
    }
}

/// Runs the DSP recompiler's self-test: `programs` random DSP programs on
/// the recompiler and on the interpreter. Returns the number of differences,
/// or `None` where the recompiler is not built or has no executable memory.
pub(crate) fn dsp_selftest(programs: u32, seed: u32) -> Option<u32> {
    #[cfg(xb_apu_recompiler)]
    {
        // SAFETY: the self-test makes and frees its own two DSPs and touches
        // no device state.
        let bad =
            unsafe { dsp_rec_selftest(c_int::try_from(programs).unwrap_or(c_int::MAX), seed) };
        u32::try_from(bad).ok()
    }
    #[cfg(not(xb_apu_recompiler))]
    {
        let _ = (programs, seed);
        None
    }
}

/// Whether the DSP recompiler is part of this build.
pub(crate) const fn has_recompiler() -> bool {
    cfg!(xb_apu_recompiler)
}

// ---------------------------------------------------------------------------
// Called by the C.
// ---------------------------------------------------------------------------

/// A log line from the C (`nc_log`, formatted by `shim/apu_shim.c`).
///
/// # Safety
/// `tag` and `msg` must be NUL-terminated strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn xb_apu_log(tag: *const c_char, msg: *const c_char) {
    if tag.is_null() || msg.is_null() {
        return;
    }
    // SAFETY: the caller passes NUL-terminated strings (checked non-null).
    let (tag, msg) = unsafe { (CStr::from_ptr(tag), CStr::from_ptr(msg)) };
    let msg = msg.to_string_lossy();
    if tag.to_bytes() == b"xbased-apu" {
        // a guest value out of range, or the guest leaving the APU waiting
        if msg.contains("dropped") || msg.starts_with("stuck:") {
            log::warn!(target: "apu", "{msg}");
        } else {
            log::info!(target: "apu", "{msg}");
        }
    } else {
        log::debug!(target: "apu", "{msg}");
    }
}

/// The APU's interrupt line, recomputed by the C from `ISTS` and `IEN`.
#[unsafe(no_mangle)]
pub extern "C" fn xb_apu_irq_set(line: c_uint, level: bool) {
    if line == C_IRQ_LINE
        && let Some(host) = host()
    {
        host.irq.set(Irq::Apu, level);
    }
}

/// The guest's time in nanoseconds (the 48 kHz sample counter `XGSCNT`).
#[unsafe(no_mangle)]
pub extern "C" fn xb_apu_guest_now_ns() -> u64 {
    host().map_or(0, |h| h.machine.guest_time_ns())
}

/// The APU wrote guest RAM `[pa, pa + len)` (the GPU's write tracking).
#[unsafe(no_mangle)]
pub extern "C" fn xb_apu_ram_written(pa: u32, len: u32) {
    if let Some(host) = host() {
        host.machine.ram_written(pa, len);
    }
}

/// The C registers its handlers for the register window.
///
/// # Safety
/// `dev` must point to a valid `NcMmioDev`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn xb_apu_mmio_register(dev: *const NcMmioDev) {
    // SAFETY: the C passes a pointer to a live struct of the mirrored layout.
    let Some(dev) = (unsafe { dev.as_ref() }) else { return };
    if let (Some(host), Some(read), Some(write)) = (host(), dev.read, dev.write) {
        debug_assert_eq!((dev.base, dev.size), (crate::MMIO_BASE, MMIO_LEN));
        let _ = host.mmio.set(Mmio { read, write, opaque: dev.opaque });
    }
}

/// A write handler cannot take a voice's lock yet: `true` if the caller of
/// the write will run it again.
#[unsafe(no_mangle)]
pub extern "C" fn xb_apu_mmio_retry() -> bool {
    let allowed = RETRY_ALLOWED.get();
    if allowed {
        RETRY_ASKED.set(true);
    }
    allowed
}

/// Whether the frame thread must hold off.
#[unsafe(no_mangle)]
pub extern "C" fn xb_apu_paused() -> bool {
    host().is_none_or(|h| h.halted.load(Ordering::Acquire))
}

/// The frame thread waits here, between frames and without the device's
/// lock, while it is held; returns when it may run or must exit.
#[unsafe(no_mangle)]
pub extern "C" fn xb_apu_pause_wait() {
    let Some(host) = host() else { return };
    let mut gate = lock(&host.gate);
    gate.parked = true;
    host.gate_changed.notify_all();
    while gate.halted() && !gate.exiting {
        gate = host.gate_changed.wait(gate).unwrap_or_else(std::sync::PoisonError::into_inner);
    }
    gate.parked = false;
}

/// The frame thread announces itself (the reference's savestate quiesce
/// list).
#[unsafe(no_mangle)]
pub extern "C" fn xb_apu_pause_register(_name: *const c_char) {}

/// The frame thread waits for the guest with frames stopped. The reference
/// counts that as parked; here a pause waits for the real pause wait, which
/// the thread reaches within one such wait (5.33 ms, or at once when kicked).
#[unsafe(no_mangle)]
pub extern "C" fn xb_apu_pause_idle(_on: bool) {}

/// The channels of the output: 2, or 6 for 5.1. Read by the C's frame code.
#[unsafe(no_mangle)]
pub static mut g_apu_out_channels: c_int = 2;

/// One EP frame of the six speaker outputs, filled by the C when
/// [`g_apu_out_channels`] is 6 and handed to [`apu_out_push`].
#[unsafe(no_mangle)]
pub static mut g_apu_frame6: [[i16; 6]; BLOCK_FRAMES] = [[0; 6]; BLOCK_FRAMES];

/// Opens the output: the ring the C sized (`capacity_bytes`) and the device
/// period it would ask for (`samples`). Returns the period in bytes.
#[unsafe(no_mangle)]
pub extern "C" fn apu_out_open(capacity_bytes: c_int, samples: c_int, _wake: *mut c_void) -> c_int {
    let Some(host) = host() else { return 0 };
    let frame_bytes = 2 * host.channels.count();
    let spec = StreamSpec {
        sample_rate: SAMPLE_RATE,
        channels: host.channels,
        block_frames: BLOCK_FRAMES,
        capacity_frames: usize::try_from(capacity_bytes).unwrap_or(0) / frame_bytes,
        period_frames: usize::try_from(samples).unwrap_or(0),
    };
    // `_wake` is the device's condition variable; `kick` broadcasts the same
    // one, guarded against the device being gone.
    let period = match host.sink.open(&spec, DrainSignal::new(kick)) {
        Ok(0) => spec.period_frames,
        Ok(frames) => frames,
        Err(why) => {
            *lock(&host.open_error) = Some(why);
            spec.period_frames
        }
    };
    c_int::try_from(period * frame_bytes).unwrap_or(c_int::MAX)
}

/// The bytes queued in the sink.
#[unsafe(no_mangle)]
pub extern "C" fn apu_out_queued() -> c_int {
    host().map_or(0, |h| {
        c_int::try_from(h.sink.queued_frames() * 2 * h.channels.count()).unwrap_or(c_int::MAX)
    })
}

/// The most bytes the device took at once.
#[unsafe(no_mangle)]
pub extern "C" fn apu_out_asked_max() -> c_int {
    host().map_or(0, |h| {
        c_int::try_from(h.sink.largest_period_frames() * 2 * h.channels.count())
            .unwrap_or(c_int::MAX)
    })
}

/// One EP frame from the frame thread: scaled by the output gain and pushed.
///
/// # Safety
/// `frame` must be valid for reads of `bytes` bytes and aligned for `i16`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn apu_out_push(frame: *const c_void, bytes: c_int) {
    let Some(host) = host() else { return };
    let samples = usize::try_from(bytes).unwrap_or(0) / 2;
    if frame.is_null() || samples == 0 || samples > BLOCK_FRAMES * 6 {
        return;
    }
    // SAFETY: the C passes one of its two frame buffers (`int16_t[256][2]` or
    // `[256][6]`) with its size; nothing writes it during the call (the frame
    // thread is the only writer and it is here).
    let input = unsafe { std::slice::from_raw_parts(frame.cast::<i16>(), samples) };
    let gain = f32::from_bits(host.gain.load(Ordering::Relaxed));
    if gain >= 1.0 {
        host.sink.push(input);
    } else {
        let mut scaled = [0i16; BLOCK_FRAMES * 6];
        for (o, &s) in scaled.iter_mut().zip(input) {
            *o = (f32::from(s) * gain) as i16;
        }
        host.sink.push(&scaled[..samples]);
    }
    host.blocks_pushed.fetch_add(1, Ordering::Relaxed);
}

/// The output's callback counters for the C's `XBASED_APU_STATS` line; a
/// sink keeps its own ([`crate::RingSource::stats`]).
///
/// # Safety
/// Both pointers must be valid for a write of a `u32`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn apu_out_stats(callbacks: *mut u32, short_callbacks: *mut u32) {
    // SAFETY: the caller passes two valid `uint32_t` pointers.
    unsafe {
        if !callbacks.is_null() {
            callbacks.write(0);
        }
        if !short_callbacks.is_null() {
            short_callbacks.write(0);
        }
    }
}

/// Without the recompiler (`dsp_rec.cpp`, x86-64 only) the C's calls into it
/// resolve to these: `dsp_run` then always takes the interpreter.
#[cfg(not(xb_apu_recompiler))]
mod no_recompiler {
    use std::ffi::{c_int, c_void};

    /// Never runs anything: the interpreter does.
    #[unsafe(no_mangle)]
    pub extern "C" fn dsp_rec_run(_dsp: *mut c_void, _cycles: c_int) -> bool {
        false
    }

    /// No recompiler here.
    #[unsafe(no_mangle)]
    pub extern "C" fn dsp_rec_available(_dsp: *mut c_void) -> bool {
        false
    }

    /// Nothing compiled to throw away.
    #[unsafe(no_mangle)]
    pub extern "C" fn dsp_rec_invalidate(_dsp: *mut c_void) {}

    /// Nothing to free.
    #[unsafe(no_mangle)]
    pub extern "C" fn dsp_rec_destroy(_dsp: *mut c_void) {}

    /// No counters.
    #[unsafe(no_mangle)]
    pub extern "C" fn dsp_rec_frame_stats(_dsp: *mut c_void) {}

    /// Cannot run.
    #[unsafe(no_mangle)]
    pub extern "C" fn dsp_rec_selftest(_programs: c_int, _seed: u32) -> c_int {
        -1
    }
}
