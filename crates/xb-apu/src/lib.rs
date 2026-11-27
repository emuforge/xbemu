//! The MCPX audio processing unit: the device a title's DirectSound drives.
//!
//! The emulation itself is vendored C (`vendor/xbased-apu`, built unmodified
//! by `build.rs`): the register interface, the voice processor, and a
//! DSP56300 emulator (with an x86-64 recompiler) for the global and encode
//! processors. This crate is its Rust face. What the C needs of the machine
//! it gets from here:
//!
//! * guest RAM as a pointer and length ([`GuestRam`]), its writes reported
//!   through [`ApuHost::ram_written`];
//! * the interrupt line on an [`IrqHub`] as [`Irq::Apu`](xb_types::irq::Irq);
//! * the guest's clock through [`ApuHost::guest_time_ns`];
//! * audio output through an [`AudioSink`] instead of SDL.
//!
//! See `docs/hw/mcpx-apu.md` for the device and `README.md` for the crate's
//! invariants.
//!
//! # Threads
//!
//! * The **guest-CPU thread** calls [`Apu::mmio_read`], [`Apu::mmio_write`]
//!   (or [`Apu::try_mmio_write`]), [`Apu::mailbox_wait`] and
//!   [`Apu::guest_time_held`]. They may block briefly on a voice the frame
//!   thread is mixing.
//! * The **frame thread** (`apu.frame`, created by [`Apu::new`], released by
//!   [`Apu::start`]) mixes 32 samples at a time, runs the DSPs, reads and
//!   writes guest RAM, raises and lowers the interrupt line and pushes a
//!   block to the sink every eighth frame. It is paced by the sink (see
//!   [`sink`](crate::AudioSink)).
//! * **Any thread** may call the control methods ([`Apu::start`],
//!   [`Apu::stop`], [`Apu::pause`], [`Apu::resume`], [`Apu::reset`],
//!   [`Apu::set_volume`], [`Apu::set_muted`], [`Apu::set_hrtf`]); they are
//!   serialized internally. `Apu` is `Send + Sync`.
//!
//! There can be only one `Apu` in a process at a time: the C keeps its state
//! in globals.

mod ffi;
mod sink;

use std::sync::{Arc, Mutex};

use xb_types::irq::IrqHub;

pub use sink::{
    AudioSink, BLOCK_FRAMES, Channels, DrainSignal, NullSink, RING_MAX_FRAMES, RingSink,
    RingSource, RingStats, SAMPLE_RATE, StreamSpec, ring_sink,
};

/// Guest physical address of the APU's register window.
pub const MMIO_BASE: u32 = 0xFE80_0000;

/// Length of the register window in bytes: the device registers below
/// `0x20000`, the voice processor's methods at `0x20000`, the global
/// processor at `0x30000` and the encode processor at `0x50000`.
pub const MMIO_LEN: u32 = 0x8_0000;

/// What the APU needs from the machine besides RAM and the interrupt hub.
/// `xb-core` implements it.
pub trait ApuHost: Send + Sync {
    /// The guest's time in nanoseconds: monotonic, stopped while the guest is
    /// paused. The 48 kHz sample counter register (`XGSCNT`, read by
    /// `DirectSoundGetSampleTime` and DirectMusic's synth clock) is derived
    /// from it. Called on the guest-CPU thread, from register accesses.
    fn guest_time_ns(&self) -> u64;

    /// The APU wrote guest RAM `[pa, pa + len)`: voice state, notifier
    /// status bytes, DSP scratch and FIFO output. The machine marks the pages
    /// for the GPU's write tracking. Called from the frame thread and the
    /// guest-CPU thread, up to a few thousand times a second; must not block.
    fn ram_written(&self, pa: u32, len: u32);
}

/// The host's never-faulting view of guest physical RAM.
#[derive(Clone, Copy, Debug)]
pub struct GuestRam {
    ptr: *mut u8,
    len: u32,
}

// SAFETY: a `GuestRam` is an address and a length. `GuestRam::new`'s caller
// promises the memory may be accessed from any thread for as long as the
// value is used.
unsafe impl Send for GuestRam {}
// SAFETY: as above; the type has no interior state.
unsafe impl Sync for GuestRam {}

impl GuestRam {
    /// Wraps the mapping of physical RAM: `ptr` is the host address of guest
    /// physical address 0 and `len` the size of RAM in bytes.
    ///
    /// # Safety
    /// `ptr` must be valid for reads and writes of `len` bytes, aligned to at
    /// least 4 bytes (a mapping of RAM is page aligned), and stay mapped at that address for as long as any [`Apu`] made from
    /// this value exists. The APU reads and writes it from its own threads
    /// while the guest runs, as a device doing DMA does (the value is `Send`
    /// and `Sync` on the strength of this promise); nothing may rely on
    /// Rust's aliasing rules for that memory.
    #[must_use]
    pub const unsafe fn new(ptr: *mut u8, len: u32) -> Self {
        Self { ptr, len }
    }

    /// The host address of guest physical address 0.
    #[must_use]
    pub const fn as_ptr(&self) -> *mut u8 {
        self.ptr
    }

    /// The size of RAM in bytes.
    #[must_use]
    pub const fn len(&self) -> u32 {
        self.len
    }

    /// Whether RAM is empty (it never usefully is).
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// Settings fixed when the APU is created, except where a setter exists.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ApuOptions {
    /// The output layout. Stereo folds the centre and rear speakers into the
    /// front pair; 5.1 delivers the six speaker outputs as they are.
    pub channels: Channels,
    /// HRTF filtering of 3D voices ([`Apu::set_hrtf`] changes it later).
    pub hrtf: bool,
    /// Run the global and the encode processors' programs to completion every
    /// frame and play the encode processor's output (the C's `NC_APU_DSP=1`).
    /// Off, the default, plays the global processor's speaker outputs once a
    /// title's DSP program runs, and the voice processor's own mix before.
    pub full_dsp: bool,
    /// Threads sharing the voice mixing, the frame thread included; 0 picks
    /// the C's default (1 on desktop Linux).
    pub voice_workers: u8,
    /// Output volume, 0.0 to 1.0 ([`Apu::set_volume`]).
    pub volume: f32,
    /// Start muted ([`Apu::set_muted`]).
    pub muted: bool,
}

impl Default for ApuOptions {
    fn default() -> Self {
        Self {
            channels: Channels::Stereo,
            hrtf: true,
            full_dsp: false,
            voice_workers: 0,
            volume: 1.0,
            muted: false,
        }
    }
}

/// Why an [`Apu`] could not be created.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApuError {
    /// Another `Apu` exists in this process; drop it first.
    AlreadyExists,
    /// The sink refused the stream ([`AudioSink::open`]'s message).
    Sink(String),
    /// The C did not come up (no register handlers, or its frame thread did
    /// not start).
    Init,
}

impl std::fmt::Display for ApuError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyExists => f.write_str("an APU already exists in this process"),
            Self::Sink(why) => write!(f, "the audio sink refused the stream: {why}"),
            Self::Init => f.write_str("the APU did not initialize"),
        }
    }
}

impl std::error::Error for ApuError {}

/// What [`Apu::try_mmio_write`] did with a register write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteOutcome {
    /// The write was applied.
    Done,
    /// The write was not applied: the frame thread held the voice it
    /// addresses for more than 0.2 ms. The caller takes any pending
    /// interrupt and executes the guest's store again.
    Retry,
}

/// The run state the control methods keep between them.
#[derive(Default)]
struct Control {
    volume: f32,
    muted: bool,
}

/// The MCPX APU. Dropping it stops the frame thread and frees the device.
pub struct Apu {
    host: Arc<ffi::Host>,
    ram: GuestRam,
    control: Mutex<Control>,
}

impl Apu {
    /// Creates the APU in its reset state. The sink is opened; the frame
    /// thread exists but makes no frames until [`Apu::start`].
    ///
    /// The C also reads a few environment variables of its own when it
    /// initializes (`NC_APU_FIFO_FRAMES`, `NC_APU_SAMPLES`, `NC_APU_DSP`,
    /// `NC_APU_HRTF`, `NC_APU_WORKERS`, `XBASED_DSP_REC`, `XBASED_APU_STATS`);
    /// they override `options`.
    ///
    /// # Errors
    /// [`ApuError::AlreadyExists`] if an `Apu` is alive, [`ApuError::Sink`]
    /// if the sink refuses the stream, [`ApuError::Init`] if the C fails.
    pub fn new(
        ram: GuestRam,
        irq: Arc<IrqHub>,
        host: Arc<dyn ApuHost>,
        sink: Arc<dyn AudioSink>,
        options: &ApuOptions,
    ) -> Result<Self, ApuError> {
        // SAFETY: `GuestRam::new`'s caller promised the mapping outlives
        // every `Apu` made from it; `Drop` hands the host back to `destroy`.
        let host = unsafe { ffi::create(ram.ptr, ram.len, irq, host, sink, options) }?;
        log::info!(
            target: "apu",
            "MCPX APU: {:?} output, DSP {}",
            options.channels,
            if ffi::has_recompiler() { "recompiler" } else { "interpreter" }
        );
        Ok(Self {
            host,
            ram,
            control: Mutex::new(Control { volume: options.volume, muted: options.muted }),
        })
    }

    /// Reads `size` (1, 2 or 4) bytes of the register at `offset` from
    /// [`MMIO_BASE`]. Anything outside the window reads 0. Guest-CPU thread.
    #[must_use]
    pub fn mmio_read(&self, offset: u32, size: u8) -> u32 {
        self.host.mmio_read(offset, size)
    }

    /// Writes the low `size` (1, 2 or 4) bytes of `value` to the register at
    /// `offset` from [`MMIO_BASE`]. A voice method waits for the frame thread
    /// to let go of its voice (normally well under a millisecond, now and
    /// then several). Guest-CPU thread.
    pub fn mmio_write(&self, offset: u32, size: u8, value: u32) {
        self.host.mmio_write(offset, size, value, false);
    }

    /// [`Apu::mmio_write`] for a CPU backend that can execute the guest's
    /// store again: instead of waiting more than 0.2 ms for a voice, the
    /// write is given up and [`WriteOutcome::Retry`] returned, so interrupts
    /// are taken between the attempts (Forza's vblank came 20 to 30 ms late
    /// without this in the reference).
    #[must_use]
    pub fn try_mmio_write(&self, offset: u32, size: u8, value: u32) -> WriteOutcome {
        if self.host.mmio_write(offset, size, value, true) {
            WriteOutcome::Done
        } else {
            WriteOutcome::Retry
        }
    }

    /// Lets the frame thread run. Until the title enables the APU's sample
    /// counter it only watches the registers.
    pub fn start(&self) {
        let _control = self.control();
        self.host.set_started(true);
    }

    /// Holds the frame thread; returns once it is parked. No frame is made,
    /// no guest RAM written and the interrupt line not changed (other than by
    /// register writes) until [`Apu::start`]. Returns `false` if the thread
    /// did not park within two seconds.
    pub fn stop(&self) -> bool {
        let _control = self.control();
        self.host.set_started(false)
    }

    /// Holds the frame thread while the guest is paused (the in-game menu);
    /// returns once it is parked, like [`Apu::stop`]. The sink is told to go
    /// silent and keeps what is queued.
    pub fn pause(&self) -> bool {
        let _control = self.control();
        self.host.set_paused(true)
    }

    /// Ends a [`Apu::pause`]. Audio continues from what the sink still holds;
    /// the frame thread takes up its pacing from the present, so there is no
    /// burst of frames for the time that passed.
    pub fn resume(&self) {
        let _control = self.control();
        self.host.set_paused(false);
    }

    /// Whether the frame thread is allowed to run: started and not paused.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.host.running()
    }

    /// The device's reset: every register zero, the voice processor's state
    /// cleared, the interrupt line lowered. The frame thread is held for the
    /// duration. As in the C, the DSPs' memories are kept; to start another
    /// title, drop the `Apu` and create a new one.
    pub fn reset(&self) {
        let _control = self.control();
        self.host.set_resetting(true);
        self.host.reset();
        self.host.set_resetting(false);
    }

    /// Saves the device's state, or replaces it with the stream's: the
    /// vendored `state.c`'s self-describing bytes (registers, voices, both
    /// DSPs). Call with the APU paused ([`Apu::pause`]). A state the
    /// vendored code refuses fails the stream and leaves the device reset.
    pub fn state(&self, io: &mut xb_types::state::StateIo) {
        let _control = self.control();
        let loading = io.loading();
        io.section(*b"APU ", 1, |io, _| {
            let mut bytes =
                if loading { Vec::new() } else { ffi::state_save().unwrap_or_default() };
            io.blob(&mut bytes);
            if loading && io.failed().is_none() && !ffi::state_load(&bytes) {
                io.fail("the state's audio processor part is not one this build reads");
            }
        });
    }

    /// The output volume, 0.0 (silent) to 1.0 (as mixed). Applied to blocks
    /// as they are pushed, so audio already queued in the sink plays at the
    /// old volume.
    pub fn set_volume(&self, volume: f32) {
        let mut control = self.control();
        control.volume = volume;
        self.host.set_gain(control.volume, control.muted);
    }

    /// Mutes or unmutes the output without changing the volume. The APU keeps
    /// running and pushes silence.
    pub fn set_muted(&self, muted: bool) {
        let mut control = self.control();
        control.muted = muted;
        self.host.set_gain(control.volume, control.muted);
    }

    /// Turns the 3D voices' HRTF filters on or off, from the next frame.
    pub fn set_hrtf(&self, on: bool) {
        self.host.set_hrtf(on);
    }

    /// The kernel's DirectSound mailbox fixup. DirectSound hands the global
    /// processor a command through a word in guest RAM and spins until the
    /// DSP clears it; the kernel patches that loop to call here instead with
    /// the word's physical address. The global processor then runs its frames
    /// to completion for the next 3 ms (it normally gets a slice of 1000
    /// cycles a frame and would take about 40 ms to reach its command check)
    /// and this waits, up to about a millisecond, for the word to clear.
    /// Returns whether it reads zero; the patched loop checks again either
    /// way. Guest-CPU thread.
    #[must_use]
    pub fn mailbox_wait(&self, word_pa: u32) -> bool {
        self.host.mailbox_wait(word_pa)
    }

    /// Tells the APU that the guest's clock ([`ApuHost::guest_time_ns`])
    /// skipped `ns` nanoseconds while the APU kept playing (the guest CPU was
    /// held for host work, not paused). The sample counter moves on by that
    /// much, as DirectMusic's synth clock needs. Guest-CPU thread.
    pub fn guest_time_held(&self, ns: u64) {
        self.host.time_held(ns);
    }

    /// Sample frames pushed to the sink since the APU was created.
    #[must_use]
    pub fn frames_pushed(&self) -> u64 {
        self.host.blocks_pushed() * BLOCK_FRAMES as u64
    }

    /// The RAM the APU was created over.
    #[must_use]
    pub fn ram(&self) -> GuestRam {
        self.ram
    }

    fn control(&self) -> std::sync::MutexGuard<'_, Control> {
        self.control.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Drop for Apu {
    fn drop(&mut self) {
        ffi::destroy(&self.host);
    }
}

/// Whether the global processor's programs are compiled to host code in this
/// build (x86-64 with the `dsp-recompiler` feature) rather than interpreted.
#[must_use]
pub const fn has_dsp_recompiler() -> bool {
    ffi::has_recompiler()
}

/// Runs the DSP recompiler's self-test: `programs` random DSP56300 programs
/// executed by the recompiler and by the interpreter from the same state and
/// compared word for word. Returns the number of programs that differed, or
/// `None` if the recompiler is not in this build or cannot get executable
/// memory. Needs no [`Apu`].
#[must_use]
pub fn dsp_recompiler_selftest(programs: u32, seed: u32) -> Option<u32> {
    ffi::dsp_selftest(programs, seed)
}
