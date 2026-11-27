//! The APU end to end: the vendored C behind the Rust interface, driven
//! through its registers as a title's DirectSound drives it.
//!
//! Register names and values are the C's `apu_regs.h`. The C allows one APU
//! per process, so every test takes [`serial`] first.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use xb_apu::{
    Apu, ApuError, ApuHost, ApuOptions, AudioSink, BLOCK_FRAMES, Channels, DrainSignal, GuestRam,
    MMIO_LEN, NullSink, SAMPLE_RATE, StreamSpec, WriteOutcome, ring_sink,
};
use xb_types::irq::{Irq, IrqHub};

// Device registers.
const ISTS: u32 = 0x1000;
const ISTS_GINT: u32 = 1 << 0;
const ISTS_FET: u32 = 1 << 4;
const ISTS_FEN: u32 = 1 << 5;
const ISTS_FEV: u32 = 1 << 6;
const IEN: u32 = 0x1004;
const FECTL: u32 = 0x1100;
const FECTL_MODE: u32 = 0xE0;
const FECTL_FREE_RUNNING: u32 = 0x00;
const FECTL_TRAPPED: u32 = 0xE0;
const FENADDR: u32 = 0x115C;
const FEDECMETH: u32 = 0x1300;
const FEDECPARAM: u32 = 0x1304;
const FETFORCE1: u32 = 0x1504;
const FETFORCE1_IDLE_VOICE: u32 = 1 << 15;
const SECTL: u32 = 0x2000;
/// `XCNTMODE` (bits 3-4) non-zero: the sample counter, and with it the
/// frames, run.
const SECTL_XCNT_ON: u32 = 1 << 3;
const XGSCNT: u32 = 0x200C;
const VPVADDR: u32 = 0x202C;
const VPSGEADDR: u32 = 0x2030;
const VPSSLADDR: u32 = 0x2034;
const TVL2D: u32 = 0x2054;
const TVL3D: u32 = 0x2060;
const TVLMP: u32 = 0x206C;

// The voice processor's methods (the window at 0x20000).
const VP: u32 = 0x2_0000;
const PIO_FREE: u32 = 0x10;
const PIO_SET_ANTECEDENT_VOICE: u32 = 0x120;
const ANTECEDENT_LIST_2D_TOP: u32 = 1 << 16;
const PIO_VOICE_ON: u32 = 0x124;
const PIO_VOICE_OFF: u32 = 0x128;
const SE2FE_IDLE_VOICE: u32 = 0x8000;

// The global processor (the window at 0x30000).
const GP: u32 = 0x3_0000;
const GPXMEM: u32 = 0x0000;

// The guest's voice table.
const PAVS_SIZE: u32 = 0x80;
const PAVS_CFG_VBIN: u32 = 0x00;
const PAVS_CFG_FMT: u32 = 0x04;
const PAVS_CFG_FMT_LOOP: u32 = 1 << 25;
const PAVS_CFG_FMT_SAMPLE_S16: u32 = 1 << 28;
const PAVS_CFG_FMT_CONTAINER_B16: u32 = 1 << 30;
const PAVS_CUR_PSL_START: u32 = 0x20;
const PAVS_CUR_PSH_SAMPLE: u32 = 0x24;
const PAVS_PAR_STATE: u32 = 0x54;
const PAVS_PAR_STATE_ACTIVE: u32 = 1 << 21;
const PAVS_PAR_OFFSET: u32 = 0x58;
const PAVS_PAR_NEXT: u32 = 0x5C;
const PAVS_TAR_VOLA: u32 = 0x60;
const PAVS_TAR_VOLB: u32 = 0x64;
const PAVS_TAR_VOLC: u32 = 0x68;
const PAVS_TAR_PITCH_LINK: u32 = 0x7C;
const MAX_VOICES: u32 = 256;
const NOTIFIER_BASE_OFFSET: u32 = 2;
const NOTIFIER_COUNT: u32 = 4;
const NOTIFY_DONE_SUCCESS: u8 = 0x01;

// Where the test puts DirectSound's tables in its RAM.
const RAM_LEN: u32 = 4 << 20;
const VOICES_PA: u32 = 0x1_0000;
const SSL_PA: u32 = 0x2_0000;
const SGE_PA: u32 = 0x4_0000;
const PCM_PA: u32 = 0x5_0000;
const NOTIFY_PA: u32 = 0x8_0000;

/// The session log, kept: the C's lines arrive through the `log` crate
/// under the target `apu`.
struct Lines(Mutex<Vec<String>>);

static LINES: Lines = Lines(Mutex::new(Vec::new()));

impl log::Log for Lines {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        if record.target() == "apu" {
            self.0.lock().unwrap().push(format!("{} {}", record.level(), record.args()));
        }
    }

    fn flush(&self) {}
}

/// The log lines from index `mark` on.
fn logged_since(mark: usize) -> Vec<String> {
    LINES.0.lock().unwrap()[mark..].to_vec()
}

/// One APU per process: the tests run one at a time. Returns the guard and
/// the log's length now.
fn serial() -> (MutexGuard<'static, ()>, usize) {
    static LOCK: Mutex<()> = Mutex::new(());
    static LOGGER: std::sync::Once = std::sync::Once::new();
    let guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    LOGGER.call_once(|| {
        log::set_logger(&LINES).expect("no other logger in the test binary");
        log::set_max_level(log::LevelFilter::Debug);
    });
    (guard, LINES.0.lock().unwrap().len())
}

/// Test RAM: zeroed, 4-aligned, never moved while the test runs.
struct Ram {
    words: Vec<u32>,
}

impl Ram {
    fn new() -> Self {
        Self { words: vec![0; RAM_LEN as usize / 4] }
    }

    fn guest(&mut self) -> GuestRam {
        // SAFETY: the vector is RAM_LEN bytes, 4-aligned, and every test
        // drops its `Apu` before its `Ram`. The test reads and writes it only
        // through `read8`/`write32` below, which go through the same raw
        // pointer discipline as the device.
        unsafe { GuestRam::new(self.words.as_mut_ptr().cast(), RAM_LEN) }
    }

    fn read32(&self, pa: u32) -> u32 {
        u32::from_le_bytes([
            self.read8(pa),
            self.read8(pa + 1),
            self.read8(pa + 2),
            self.read8(pa + 3),
        ])
    }

    fn read8(&self, pa: u32) -> u8 {
        assert!(pa < RAM_LEN);
        // SAFETY: in bounds; a racing write by the APU's thread is a plain
        // byte store, which this volatile read tolerates as a device would.
        unsafe { self.words.as_ptr().cast::<u8>().add(pa as usize).read_volatile() }
    }

    fn write32(&mut self, pa: u32, v: u32) {
        assert!(pa.is_multiple_of(4) && pa + 4 <= RAM_LEN);
        // SAFETY: in bounds and aligned.
        unsafe {
            self.words.as_mut_ptr().cast::<u8>().add(pa as usize).cast::<u32>().write_volatile(v)
        }
    }
}

/// The machine: a guest clock that is the host's, and a record of the RAM
/// writes the APU reports.
struct Machine {
    epoch: Instant,
    writes: Mutex<Vec<(u32, u32)>>,
}

impl Machine {
    fn new() -> Arc<Self> {
        Arc::new(Self { epoch: Instant::now(), writes: Mutex::new(Vec::new()) })
    }

    fn reported(&self, pa: u32) -> bool {
        self.writes.lock().unwrap().iter().any(|&(start, len)| start <= pa && pa < start + len)
    }
}

impl ApuHost for Machine {
    fn guest_time_ns(&self) -> u64 {
        self.epoch.elapsed().as_nanos() as u64
    }

    fn ram_written(&self, pa: u32, len: u32) {
        self.writes.lock().unwrap().push((pa, len));
    }
}

/// A sink that keeps what it is given and behaves like an audio device with
/// a 10 ms period: every period of host time it takes a period's worth of
/// frames if it has them (and plays a gap if it has not).
struct Capture {
    period: usize,
    state: Mutex<CaptureState>,
    pushed_frames: AtomicU64,
    paused: AtomicBool,
    pause_calls: Mutex<Vec<bool>>,
}

struct CaptureState {
    opened: Option<(StreamSpec, Instant)>,
    ticks: u64,
    queued: usize,
    blocks: Vec<(Instant, usize)>,
    peak: i32,
    pushes_while_paused: usize,
}

impl Capture {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            period: 480,
            state: Mutex::new(CaptureState {
                opened: None,
                ticks: 0,
                queued: 0,
                blocks: Vec::new(),
                peak: 0,
                pushes_while_paused: 0,
            }),
            pushed_frames: AtomicU64::new(0),
            paused: AtomicBool::new(false),
            pause_calls: Mutex::new(Vec::new()),
        })
    }

    /// Runs the device up to now.
    fn advance(&self, s: &mut CaptureState) {
        let Some((_, start)) = s.opened else { return };
        let due =
            (start.elapsed().as_secs_f64() * f64::from(SAMPLE_RATE) / self.period as f64) as u64;
        while s.ticks < due {
            s.ticks += 1;
            if !self.paused.load(Ordering::Acquire) {
                s.queued -= s.queued.min(self.period);
            }
        }
    }

    fn pushed(&self) -> u64 {
        self.pushed_frames.load(Ordering::Acquire)
    }

    /// The largest sample magnitude over the blocks pushed from now on,
    /// until `enough` more sample frames have come.
    fn peak_over(&self, enough: u64) -> i32 {
        self.state.lock().unwrap().peak = 0;
        let from = self.pushed();
        wait_for("blocks to listen to", Duration::from_secs(2), || self.pushed() >= from + enough);
        self.state.lock().unwrap().peak
    }

    fn spec(&self) -> StreamSpec {
        self.state.lock().unwrap().opened.expect("the sink was opened").0
    }
}

impl AudioSink for Capture {
    fn open(&self, spec: &StreamSpec, _drained: DrainSignal) -> Result<usize, String> {
        self.state.lock().unwrap().opened = Some((*spec, Instant::now()));
        Ok(self.period)
    }

    fn push(&self, samples: &[i16]) {
        let mut s = self.state.lock().unwrap();
        self.advance(&mut s);
        let channels = s.opened.expect("opened before the first push").0.channels.count();
        s.queued += samples.len() / channels;
        s.blocks.push((Instant::now(), samples.len()));
        s.peak = s.peak.max(samples.iter().map(|&v| i32::from(v).abs()).max().unwrap_or(0));
        s.pushes_while_paused += usize::from(self.paused.load(Ordering::Acquire));
        self.pushed_frames.fetch_add((samples.len() / channels) as u64, Ordering::Release);
    }

    fn queued_frames(&self) -> usize {
        let mut s = self.state.lock().unwrap();
        self.advance(&mut s);
        s.queued
    }

    fn set_paused(&self, paused: bool) {
        // the device's clock runs on through a pause; it just takes nothing
        let mut s = self.state.lock().unwrap();
        self.advance(&mut s);
        self.paused.store(paused, Ordering::Release);
        self.pause_calls.lock().unwrap().push(paused);
    }
}

struct Rig {
    // dropped first: the APU must be gone before its RAM
    apu: Apu,
    irq: Arc<IrqHub>,
    machine: Arc<Machine>,
    ram: Ram,
}

fn rig(sink: Arc<dyn AudioSink>, options: &ApuOptions) -> Rig {
    let mut ram = Ram::new();
    let irq = Arc::new(IrqHub::new());
    let machine = Machine::new();
    let apu = Apu::new(ram.guest(), irq.clone(), machine.clone(), sink, options)
        .expect("the APU is created");
    Rig { apu, irq, machine, ram }
}

impl Rig {
    fn rd(&self, reg: u32) -> u32 {
        self.apu.mmio_read(reg, 4)
    }

    fn wr(&self, reg: u32, v: u32) {
        self.apu.mmio_write(reg, 4, v);
    }

    fn irq_up(&self) -> bool {
        self.irq.pending() & Irq::Apu.mask() != 0
    }

    /// What DirectSound's start-up comes to for the device: its tables in
    /// RAM, empty voice lists, the front end free running and the sample
    /// counter on. From here the frame thread makes frames.
    fn bring_up(&mut self) {
        // every voice's link says "end of list"
        for v in 0..MAX_VOICES {
            self.ram.write32(VOICES_PA + v * PAVS_SIZE + PAVS_TAR_PITCH_LINK, 0xFFFF);
        }
        self.wr(VPVADDR, VOICES_PA);
        self.wr(VPSGEADDR, SGE_PA);
        self.wr(VPSSLADDR, SSL_PA);
        self.wr(FENADDR, NOTIFY_PA);
        for list in [TVL2D, TVL3D, TVLMP] {
            self.wr(list, 0xFFFF);
        }
        self.wr(XGSCNT, 0);
        self.wr(FECTL, FECTL_FREE_RUNNING);
        self.wr(SECTL, SECTL_XCNT_ON);
    }

    /// The interrupt service routine's acknowledge: level triggered, so it
    /// is repeated while the line stays up (a frame in flight can post the
    /// cause once more).
    fn acknowledge(&self, bits: u32) {
        for _ in 0..100 {
            self.wr(ISTS, bits);
            if !self.irq_up() {
                return;
            }
            thread::sleep(Duration::from_millis(1));
        }
        panic!(
            "the interrupt line stays up: ISTS {:08x} FECTL {:08x}",
            self.rd(ISTS),
            self.rd(FECTL)
        );
    }
}

fn wait_for(what: &str, timeout: Duration, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < timeout, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(1));
    }
}

/// Sample frames pushed over `window`, measured from a block boundary on.
fn frames_over(pushed: impl Fn() -> u64, window: Duration) -> (u64, f64) {
    let first = pushed();
    wait_for("a block", Duration::from_secs(2), || pushed() != first);
    let (start, before) = (Instant::now(), pushed());
    thread::sleep(window);
    let (after, elapsed) = (pushed(), start.elapsed());
    (after - before, elapsed.as_secs_f64() * f64::from(SAMPLE_RATE))
}

#[test]
fn the_apu_comes_up_makes_frames_at_48_khz_raises_its_interrupt_and_shuts_down() {
    let (_serial, log_mark) = serial();
    let sink = Capture::new();
    let mut rig = rig(sink.clone(), &ApuOptions::default());

    // --- the C's start-up line, the one the reference logs booting Halo
    // ("xbased-apu: voices: 1 share (the frame thread alone)")
    let log = logged_since(log_mark);
    assert!(log.iter().any(|l| l == "INFO voices: 1 share (the frame thread alone)"), "{log:?}");
    assert!(!log.iter().any(|l| l.starts_with("WARN")), "{log:?}");

    // --- reset state, before anything runs
    let spec = sink.spec();
    assert_eq!(
        (spec.sample_rate, spec.channels, spec.block_frames),
        (SAMPLE_RATE, Channels::Stereo, BLOCK_FRAMES)
    );
    assert!(spec.capacity_frames >= 4 * sink.period, "room for the high watermark");
    assert!(!rig.apu.is_running());
    for reg in [ISTS, IEN, FECTL, SECTL, VPVADDR] {
        assert_eq!(rig.rd(reg), 0, "register {reg:#x} after reset");
    }
    assert_eq!(rig.rd(VP + PIO_FREE), 0x80, "the method FIFO always reads empty");
    assert_eq!(rig.apu.mmio_read(MMIO_LEN, 4), 0, "past the window");
    assert!(!rig.irq_up());

    // --- started, but the title has not enabled the sample counter
    rig.apu.start();
    assert!(rig.apu.is_running());
    thread::sleep(Duration::from_millis(60));
    assert_eq!(sink.pushed(), 0, "no frames while SECTL's XCNTMODE is off");
    assert!(!rig.irq_up(), "nothing is enabled in IEN");

    // --- DirectSound's start-up: frames flow
    rig.bring_up();
    assert_eq!(rig.rd(SECTL), SECTL_XCNT_ON);
    assert_eq!(rig.rd(VPVADDR), VOICES_PA);
    wait_for("the first block", Duration::from_secs(2), || sink.pushed() > 0);

    // The sample counter follows the guest's clock at 48 kHz from the value
    // written (0, in bring_up).
    let (c0, t0) = (rig.rd(XGSCNT), rig.machine.guest_time_ns());
    thread::sleep(Duration::from_millis(50));
    let (c1, t1) = (rig.rd(XGSCNT), rig.machine.guest_time_ns());
    let expected = (t1 - t0) as f64 * 48_000.0 / 1e9;
    assert!(
        (f64::from(c1 - c0) - expected).abs() < 200.0,
        "XGSCNT moved {} in {expected} samples of time",
        c1 - c0
    );

    // Past the initial fill (to four device periods), the frames come at the
    // device's rate: 48000 sample frames a second, in blocks of 256.
    thread::sleep(Duration::from_millis(300));
    let (frames, expected) = frames_over(|| sink.pushed(), Duration::from_secs(1));
    println!(
        "capture sink: {frames} frames pushed in a second, {expected:.0} by the device's clock"
    );
    assert!(
        (frames as f64 - expected).abs() <= 1500.0,
        "{frames} frames pushed where the device took {expected}"
    );
    {
        let s = sink.state.lock().unwrap();
        assert!(
            s.blocks.iter().all(|&(_, samples)| samples == BLOCK_FRAMES * 2),
            "every push is one stereo block"
        );
        assert_eq!(s.peak, 0, "no voice plays: silence");
        // and no faster than real time in the short run either: any 100
        // consecutive blocks (0.53 s of audio) take at least 0.4 s
        for w in s.blocks[20..].windows(100) {
            let span = w[99].0.duration_since(w[0].0);
            assert!(span > Duration::from_millis(400), "100 blocks in {span:?}");
        }
    }
    assert_eq!(rig.apu.frames_pushed(), sink.pushed());

    // --- a voice's notification: VOICE_OFF writes the notifier's status in
    // guest RAM, reports the write, and interrupts once enabled
    let voice = 3;
    let status_pa = NOTIFY_PA + 16 * (NOTIFIER_BASE_OFFSET + voice * NOTIFIER_COUNT) + 15;
    assert_eq!(rig.ram.read8(status_pa), 0);
    rig.wr(VP + PIO_VOICE_OFF, voice);
    assert_eq!(rig.ram.read8(status_pa), NOTIFY_DONE_SUCCESS);
    assert!(rig.machine.reported(status_pa), "the write to guest RAM was reported");
    assert_eq!(rig.rd(ISTS) & (ISTS_FEV | ISTS_FEN), ISTS_FEV | ISTS_FEN);
    thread::sleep(Duration::from_millis(20));
    assert!(!rig.irq_up(), "IEN is still zero: the status is latched, the line stays down");
    rig.wr(IEN, ISTS_GINT | ISTS_FET | ISTS_FEN | ISTS_FEV);
    rig.wr(VP + PIO_VOICE_OFF, voice);
    wait_for("the notify interrupt", Duration::from_secs(1), || rig.irq_up());
    assert_eq!(rig.rd(ISTS) & ISTS_GINT, ISTS_GINT);
    rig.acknowledge(ISTS_FEV | ISTS_FEN);
    assert_eq!(rig.rd(ISTS), 0);

    // --- the voice processor's trap: an idle voice on a list, with the
    // title asking for idle-voice traps, stops the front end and interrupts
    rig.wr(FETFORCE1, FETFORCE1_IDLE_VOICE);
    rig.wr(TVL2D, 7);
    wait_for("the idle-voice trap", Duration::from_secs(1), || rig.irq_up());
    assert_eq!(rig.rd(FECTL) & FECTL_MODE, FECTL_TRAPPED);
    assert_eq!(
        (rig.rd(FEDECMETH), rig.rd(FEDECPARAM)),
        (SE2FE_IDLE_VOICE, 7),
        "the trap names the voice"
    );
    assert_eq!(rig.rd(ISTS) & (ISTS_GINT | ISTS_FET), ISTS_GINT | ISTS_FET);
    // the frames go on while trapped (the DSPs are separate engines)
    let during_trap = sink.pushed();
    wait_for("frames during the trap", Duration::from_secs(1), || {
        sink.pushed() > during_trap + 1024
    });
    // the handler: unlink the voice, release the trap, acknowledge
    rig.wr(TVL2D, 0xFFFF);
    rig.wr(FECTL, FECTL_FREE_RUNNING);
    rig.acknowledge(ISTS_FET);
    assert_eq!(rig.rd(ISTS), 0);
    thread::sleep(Duration::from_millis(30));
    assert!(!rig.irq_up(), "it stays down once the cause is gone");

    // --- a masked cause does not raise the line
    rig.wr(IEN, ISTS_GINT);
    rig.wr(VP + PIO_VOICE_OFF, voice);
    thread::sleep(Duration::from_millis(30));
    assert!(!rig.irq_up());
    rig.wr(ISTS, ISTS_FEV | ISTS_FEN);

    // --- shutdown: the thread is joined and the line left down, and the
    // process can make another APU
    rig.wr(IEN, ISTS_GINT | ISTS_FEN | ISTS_FEV);
    rig.wr(VP + PIO_VOICE_OFF, voice);
    wait_for("an interrupt to leave up", Duration::from_secs(1), || rig.irq_up());
    let Rig { apu, irq, ram, .. } = rig;
    let pushed = sink.pushed();
    drop(apu);
    assert_eq!(irq.pending(), 0, "dropping the APU lowers its line");
    thread::sleep(Duration::from_millis(30));
    assert_eq!(sink.pushed(), pushed, "nothing is pushed after the drop");
    drop(ram);

    let second = self::rig(Capture::new(), &ApuOptions::default());
    assert_eq!(second.rd(ISTS), 0);
}

#[test]
fn a_pause_stops_the_frames_cleanly_and_a_resume_continues_without_a_burst() {
    let (_serial, _) = serial();
    let sink = Capture::new();
    let mut rig = rig(sink.clone(), &ApuOptions::default());
    rig.apu.start();
    rig.bring_up();
    wait_for("frames", Duration::from_secs(2), || sink.pushed() > 4 * 480);
    thread::sleep(Duration::from_millis(200));

    // paused: when `pause` returns the frame thread is parked
    assert!(rig.apu.pause(), "the frame thread parks");
    assert!(!rig.apu.is_running());
    let at_pause = sink.pushed();
    let writes_at_pause = rig.machine.writes.lock().unwrap().len();
    thread::sleep(Duration::from_millis(400));
    assert_eq!(sink.pushed(), at_pause, "no frame is made while paused");
    assert_eq!(
        rig.machine.writes.lock().unwrap().len(),
        writes_at_pause,
        "and no guest RAM written"
    );
    assert_eq!(sink.pause_calls.lock().unwrap().last(), Some(&true), "the sink was told");
    let queued = sink.queued_frames();
    thread::sleep(Duration::from_millis(50));
    assert_eq!(sink.queued_frames(), queued, "the sink keeps its audio through the pause");

    // registers still answer while paused (the menu may outlast a register
    // access in flight)
    assert_eq!(rig.rd(SECTL), SECTL_XCNT_ON);

    // resumed: frames come again at the device's rate, with nothing made up
    // for the 0.45 s that passed (that would be 21600 frames more)
    rig.apu.resume();
    assert!(rig.apu.is_running());
    assert_eq!(sink.pause_calls.lock().unwrap().last(), Some(&false));
    let (start, before) = (Instant::now(), sink.pushed());
    thread::sleep(Duration::from_millis(500));
    let (frames, expected) =
        (sink.pushed() - before, start.elapsed().as_secs_f64() * f64::from(SAMPLE_RATE));
    println!(
        "after the resume: {frames} frames in half a second, {expected:.0} at the device's rate"
    );
    assert!(
        (frames as f64 - expected).abs() <= 2500.0,
        "{frames} frames in the half second after the resume, {expected} at the device's rate"
    );
    assert_eq!(sink.state.lock().unwrap().pushes_while_paused, 0);

    // a pause while already paused, and a resume while running, are harmless
    assert!(rig.apu.pause());
    assert!(rig.apu.pause());
    rig.apu.resume();
    rig.apu.resume();
    let before = sink.pushed();
    wait_for("frames after the second resume", Duration::from_secs(1), || sink.pushed() > before);
}

#[test]
fn stop_holds_the_frame_thread_and_a_pause_survives_a_start() {
    let (_serial, _) = serial();
    let sink = Capture::new();
    let mut rig = rig(sink.clone(), &ApuOptions::default());
    rig.apu.start();
    rig.bring_up();
    wait_for("frames", Duration::from_secs(2), || sink.pushed() > 0);

    assert!(rig.apu.stop());
    let stopped_at = sink.pushed();
    thread::sleep(Duration::from_millis(100));
    assert_eq!(sink.pushed(), stopped_at);

    // paused and stopped: starting alone does not run it
    assert!(rig.apu.pause());
    rig.apu.start();
    assert!(!rig.apu.is_running());
    thread::sleep(Duration::from_millis(100));
    assert_eq!(sink.pushed(), stopped_at);
    rig.apu.resume();
    wait_for("frames after start and resume", Duration::from_secs(1), || {
        sink.pushed() > stopped_at
    });
}

#[test]
fn a_reset_clears_the_registers_lowers_the_line_and_stops_the_frames() {
    let (_serial, _) = serial();
    let sink = Capture::new();
    let mut rig = rig(sink.clone(), &ApuOptions::default());
    rig.apu.start();
    rig.bring_up();
    rig.wr(IEN, ISTS_GINT | ISTS_FEN | ISTS_FEV);
    rig.wr(VP + PIO_VOICE_OFF, 1);
    wait_for("the interrupt", Duration::from_secs(1), || rig.irq_up());

    rig.apu.reset();
    assert!(!rig.irq_up());
    for reg in [ISTS, IEN, SECTL, VPVADDR, TVL2D] {
        assert_eq!(rig.rd(reg), 0, "register {reg:#x} after the reset");
    }
    assert!(rig.apu.is_running(), "a reset leaves the frame thread as it was");
    thread::sleep(Duration::from_millis(50));
    let after_reset = sink.pushed();
    thread::sleep(Duration::from_millis(100));
    assert_eq!(sink.pushed(), after_reset, "the sample counter is off again: no frames");
    assert!(!rig.irq_up());

    // and the title can bring it up again
    rig.bring_up();
    wait_for("frames after the second bring-up", Duration::from_secs(1), || {
        sink.pushed() > after_reset
    });
}

#[test]
fn the_null_sink_paces_the_apu_at_the_sample_rate() {
    let (_serial, _) = serial();
    let sink = Arc::new(NullSink::new(Channels::Stereo, 480));
    let mut rig = rig(sink.clone(), &ApuOptions::default());
    rig.apu.start();
    rig.bring_up();
    wait_for("frames", Duration::from_secs(2), || rig.apu.frames_pushed() > 0);
    thread::sleep(Duration::from_millis(300));

    let (frames, expected) = frames_over(|| rig.apu.frames_pushed(), Duration::from_secs(1));
    println!("null sink: {frames} frames pushed in a second, {expected:.0} by the clock");
    assert!(
        (frames as f64 - expected).abs() <= 1500.0,
        "{frames} frames pushed in a second with no audio device, {expected} by the clock"
    );
    let stats = sink.stats();
    assert_eq!(stats.overflows, 0);
    assert!(
        stats.queued_frames <= 4 * 480 + BLOCK_FRAMES,
        "the queue stays at the high watermark or below"
    );

    // paused, the null device takes nothing and the APU makes nothing
    assert!(rig.apu.pause());
    let (pushed, consumed) = (rig.apu.frames_pushed(), sink.stats().consumed_frames);
    thread::sleep(Duration::from_millis(150));
    assert_eq!(rig.apu.frames_pushed(), pushed);
    assert_eq!(sink.stats().consumed_frames, consumed);
    rig.apu.resume();
    wait_for("frames after the resume", Duration::from_secs(1), || {
        rig.apu.frames_pushed() > pushed
    });
}

#[test]
fn a_ring_sink_feeds_a_device_callback_without_gaps_once_filled() {
    let (_serial, _) = serial();
    let (sink, source) = ring_sink(Channels::Stereo, 512);
    let mut rig = rig(Arc::new(sink), &ApuOptions::default());
    rig.apu.start();
    rig.bring_up();

    // the device: 512 frames every 10.67 ms for half a second
    let mut buf = vec![0i16; 512 * 2];
    let start = Instant::now();
    let mut calls = 0u32;
    while start.elapsed() < Duration::from_millis(500) {
        let due = start + Duration::from_nanos(u64::from(calls + 1) * 512 * 1_000_000_000 / 48_000);
        thread::sleep(due.saturating_duration_since(Instant::now()));
        source.fill(&mut buf);
        calls += 1;
    }
    let stats = source.stats();
    println!("ring sink after half a second of 512-frame callbacks: {stats:?}");
    assert_eq!(stats.overflows, 0);
    assert!(stats.consumed_frames > 0);
    // after the first few callbacks (the ring filling) the device is always
    // served in full
    assert!(
        stats.short_callbacks <= 6,
        "{} of {} callbacks came up short",
        stats.short_callbacks,
        stats.callbacks
    );
    assert!(stats.queued_frames <= 4 * 512 + BLOCK_FRAMES);
    rig.apu.set_hrtf(false);
}

#[test]
fn a_looping_pcm_voice_is_heard_and_volume_and_mute_scale_it() {
    let (_serial, _) = serial();
    let sink = Capture::new();
    let mut rig = rig(sink.clone(), &ApuOptions::default());
    rig.apu.start();
    rig.bring_up();

    // A buffer of 2048 16-bit samples, a 500 Hz square wave at a quarter of
    // full scale, in one page of guest RAM named by the buffer page table.
    const AMPLITUDE: i32 = 8192;
    rig.ram.write32(SGE_PA, PCM_PA);
    for i in 0..1024u32 {
        let s = if (2 * i / 96) % 2 == 0 { AMPLITUDE } else { -AMPLITUDE } as i16 as u16;
        rig.ram.write32(PCM_PA + 4 * i, u32::from(s) | u32::from(s) << 16);
    }
    // A 2D voice (handles from 64 up; below are the 3D voices): mono 16-bit
    // PCM, looping over the whole buffer at 48 kHz (pitch 0), unattenuated
    // into mixbins 0 and 1 (front left and right), its other routes off.
    let voice = 100u32;
    let at = VOICES_PA + voice * PAVS_SIZE;
    rig.ram.write32(at + PAVS_CFG_VBIN, 1 << 5);
    rig.ram.write32(
        at + PAVS_CFG_FMT,
        PAVS_CFG_FMT_LOOP | PAVS_CFG_FMT_SAMPLE_S16 | PAVS_CFG_FMT_CONTAINER_B16,
    );
    rig.ram.write32(at + PAVS_CUR_PSL_START, 0);
    rig.ram.write32(at + PAVS_CUR_PSH_SAMPLE, 0);
    rig.ram.write32(at + PAVS_PAR_NEXT, 2047);
    rig.ram.write32(at + PAVS_TAR_VOLA, 0x000F_000F);
    rig.ram.write32(at + PAVS_TAR_VOLB, 0xFFFF_FFFF);
    rig.ram.write32(at + PAVS_TAR_VOLC, 0xFFFF_FFFF);
    assert_eq!(sink.peak_over(1024), 0, "silence before the voice starts");

    // DirectSound's two methods to start it: at the top of the 2D list
    rig.wr(VP + PIO_SET_ANTECEDENT_VOICE, ANTECEDENT_LIST_2D_TOP | 0xFFFF);
    rig.wr(VP + PIO_VOICE_ON, voice);
    assert_eq!(rig.rd(TVL2D), voice);
    assert_ne!(rig.ram.read32(at + PAVS_PAR_STATE) & PAVS_PAR_STATE_ACTIVE, 0);

    // heard, at the level it was recorded (the envelope is off: full level)
    sink.peak_over(2048);
    let full = sink.peak_over(4096);
    println!("a square wave of {AMPLITUDE} peaks at {full} in the output");
    assert!(
        (AMPLITUDE * 3 / 4..=AMPLITUDE * 5 / 4).contains(&full),
        "peak {full} for a wave of {AMPLITUDE}"
    );
    // the voice's play offset, in guest RAM, moves and its writes are reported
    let cbo = rig.ram.read32(at + PAVS_PAR_OFFSET) & 0x00FF_FFFF;
    wait_for("the play offset to move", Duration::from_secs(1), || {
        rig.ram.read32(at + PAVS_PAR_OFFSET) & 0x00FF_FFFF != cbo
    });
    assert!(rig.machine.reported(at + PAVS_PAR_OFFSET));

    // volume and mute act on the blocks as they are pushed
    rig.apu.set_volume(0.5);
    let half = sink.peak_over(4096);
    assert!((half - full / 2).abs() <= full / 16, "peak {half} at half volume, {full} at full");
    rig.apu.set_muted(true);
    assert_eq!(sink.peak_over(4096), 0, "muted");
    let before = sink.pushed();
    wait_for("frames while muted", Duration::from_secs(1), || sink.pushed() > before + 2048);
    rig.apu.set_muted(false);
    let unmuted = sink.peak_over(4096);
    assert!((unmuted - half).abs() <= full / 16, "the volume is kept through a mute");
    rig.apu.set_volume(1.0);

    // a pause in the middle of a sound: the same sound afterwards
    assert!(rig.apu.pause());
    thread::sleep(Duration::from_millis(100));
    rig.apu.resume();
    let resumed = sink.peak_over(4096);
    assert!((resumed - full).abs() <= full / 16);

    // turned off: silence again, and its notifier says done
    rig.wr(VP + PIO_VOICE_OFF, voice);
    assert_eq!(rig.ram.read32(at + PAVS_PAR_STATE) & PAVS_PAR_STATE_ACTIVE, 0);
    sink.peak_over(1024);
    assert_eq!(sink.peak_over(2048), 0, "silence after VOICE_OFF");
    let status_pa = NOTIFY_PA + 16 * (NOTIFIER_BASE_OFFSET + voice * NOTIFIER_COUNT) + 15;
    assert_eq!(rig.ram.read8(status_pa), NOTIFY_DONE_SUCCESS);
}

#[test]
fn surround_output_pushes_six_channel_blocks() {
    let (_serial, _) = serial();
    let sink = Capture::new();
    let options = ApuOptions { channels: Channels::Surround51, ..ApuOptions::default() };
    let mut rig = rig(sink.clone(), &options);
    assert_eq!(sink.spec().channels, Channels::Surround51);
    rig.apu.start();
    rig.bring_up();
    wait_for("blocks", Duration::from_secs(2), || sink.pushed() >= 4 * BLOCK_FRAMES as u64);
    let s = sink.state.lock().unwrap();
    assert!(s.blocks.iter().all(|&(_, samples)| samples == BLOCK_FRAMES * 6));
}

#[test]
fn only_one_apu_exists_at_a_time_and_a_refusing_sink_fails_creation() {
    let (_serial, _) = serial();
    let first = rig(Capture::new(), &ApuOptions::default());
    let mut ram = Ram::new();
    let again = Apu::new(
        ram.guest(),
        Arc::new(IrqHub::new()),
        Machine::new(),
        Capture::new(),
        &ApuOptions::default(),
    );
    assert_eq!(again.err(), Some(ApuError::AlreadyExists));
    drop(first);

    // a stereo ring cannot take the 5.1 stream
    let (stereo, _source) = ring_sink(Channels::Stereo, 0);
    let options = ApuOptions { channels: Channels::Surround51, ..ApuOptions::default() };
    let refused =
        Apu::new(ram.guest(), Arc::new(IrqHub::new()), Machine::new(), Arc::new(stereo), &options);
    assert!(matches!(refused.err(), Some(ApuError::Sink(_))));

    // and the failure left nothing behind: the next one, with the other DSP
    // setting, comes up and makes frames
    let sink = Capture::new();
    let options = ApuOptions { full_dsp: true, voice_workers: 2, ..ApuOptions::default() };
    let mut third = rig(sink.clone(), &options);
    assert_eq!(third.rd(ISTS), 0);
    third.apu.start();
    third.bring_up();
    wait_for("frames from the third APU", Duration::from_secs(2), || sink.pushed() > 2048);
}

#[test]
fn the_mailbox_wait_returns_when_the_word_clears() {
    let (_serial, _) = serial();
    let mut rig = rig(Capture::new(), &ApuOptions::default());
    rig.apu.start();
    rig.bring_up();
    let word = 0x3_0010;

    assert!(rig.apu.mailbox_wait(word), "no command pending");
    assert!(!rig.apu.mailbox_wait(RAM_LEN), "outside RAM");
    assert!(!rig.apu.mailbox_wait(word + 2), "not a word address");

    // a command the DSP never takes: the wait gives up
    rig.ram.write32(word, 1);
    let start = Instant::now();
    assert!(!rig.apu.mailbox_wait(word));
    let gave_up_after = start.elapsed();
    println!("a mailbox wait on a word that never clears gave up after {gave_up_after:?}");
    assert!(gave_up_after < Duration::from_millis(500), "gave up after {gave_up_after:?}");

    // one that is taken (here by the test, standing in for the DSP): the
    // wait sees it and returns early
    let guest = rig.ram.guest();
    let clearer = thread::spawn(move || {
        thread::sleep(Duration::from_millis(2));
        let mut ram = RamAlias(guest);
        ram.clear32(word);
    });
    let mut cleared = false;
    for _ in 0..50 {
        if rig.apu.mailbox_wait(word) {
            cleared = true;
            break;
        }
    }
    clearer.join().unwrap();
    assert!(cleared);
}

/// A second handle on the test RAM for a helper thread, as the DSP's DMA is.
/// The thread is joined before the rig, and with it the RAM, is dropped.
struct RamAlias(GuestRam);

impl RamAlias {
    fn clear32(&mut self, pa: u32) {
        assert!(pa.is_multiple_of(4) && pa + 4 <= self.0.len());
        // SAFETY: in bounds and aligned; the reader uses atomic loads.
        let word =
            unsafe { &*self.0.as_ptr().add(pa as usize).cast::<std::sync::atomic::AtomicU32>() };
        word.store(0, Ordering::Release);
    }
}

#[test]
fn a_retryable_write_completes_when_no_voice_is_held() {
    let (_serial, _) = serial();
    let mut rig = rig(Capture::new(), &ApuOptions::default());
    rig.apu.start();
    rig.bring_up();
    // with empty voice lists the frame thread holds no voice: every method
    // goes through at the first attempt
    for _ in 0..200 {
        assert_eq!(rig.apu.try_mmio_write(VP + PIO_VOICE_OFF, 4, 5), WriteOutcome::Done);
    }
    assert_eq!(rig.apu.try_mmio_write(IEN, 4, ISTS_GINT), WriteOutcome::Done);
    assert_eq!(rig.rd(IEN), ISTS_GINT);
}

#[test]
fn the_global_processors_memory_is_reached_through_its_window() {
    let (_serial, _) = serial();
    let rig = rig(Capture::new(), &ApuOptions::default());
    // DSP words are 24 bits: the top byte of a host write is not stored
    rig.wr(GP + GPXMEM + 4 * 0x20, 0xFF12_3456);
    assert_eq!(rig.rd(GP + GPXMEM + 4 * 0x20), 0x0012_3456);
    assert_eq!(rig.rd(GP + GPXMEM + 4 * 0x21), 0);
}

#[test]
fn the_dsp_recompiler_agrees_with_the_interpreter_on_random_programs() {
    let (_serial, _) = serial();
    if !xb_apu::has_dsp_recompiler() {
        assert_eq!(xb_apu::dsp_recompiler_selftest(1, 1), None);
        println!("the DSP recompiler is not in this build: nothing to compare");
        return;
    }
    assert_eq!(
        xb_apu::dsp_recompiler_selftest(300, 1),
        Some(0),
        "programs on which the two differ"
    );
}
