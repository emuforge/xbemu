//! Where the APU's mixed output goes: the [`AudioSink`] trait, a ring buffer
//! for real audio devices and a sink that plays to nobody.
//!
//! Opening a device's stream is the front end's business.
//!
//! # How the output paces the APU
//!
//! The frame thread has no clock of its own that matters in the long run: it
//! makes audio as fast as the output takes it. Every 256 sample frames (one
//! EP frame, 5.33 ms) it looks at how much the sink still holds:
//!
//! * at or above the **high watermark** (four device periods) it waits until
//!   the device has taken some ([`DrainSignal`] wakes it early);
//! * above the **low watermark** (two periods) it paces itself against the
//!   host clock, one block per 5.33 ms, nudged by a microsecond a block
//!   towards the middle of the two marks;
//! * at or below the low watermark it runs flat out to refill.
//!
//! So the device's sample clock, not the host's, sets the long-term rate, and
//! a sink must report its fill level honestly ([`AudioSink::queued_frames`]).
//! A sink that reports less than it holds makes the APU run ahead (growing
//! latency, then an overflow); one that never drains stalls the APU, and with
//! it every title that waits on voice notifications.

use std::sync::atomic::{AtomicBool, AtomicI16, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// The APU's output rate in sample frames a second.
pub const SAMPLE_RATE: u32 = 48_000;

/// Sample frames in one [`AudioSink::push`]: an EP frame, eight GP frames of
/// 32 samples (5.33 ms).
pub const BLOCK_FRAMES: usize = 256;

/// The most sample frames a [`ring_sink`] holds: the largest ring the C can
/// be configured for (`NC_APU_FIFO_FRAMES`, at most 128 blocks).
pub const RING_MAX_FRAMES: usize = 128 * BLOCK_FRAMES;

/// The speaker layout of the output stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Channels {
    /// Two channels, left then right. The six speaker outputs are folded
    /// into the pair as the console's analog output does.
    #[default]
    Stereo,
    /// Six channels in the order FL, FR, C, LFE, BL, BR (the order of the
    /// GP DSP's speaker outputs and of DirectSound's mixbins).
    Surround51,
}

impl Channels {
    /// Samples in one sample frame.
    #[must_use]
    pub const fn count(self) -> usize {
        match self {
            Self::Stereo => 2,
            Self::Surround51 => 6,
        }
    }
}

/// The stream an [`AudioSink`] is opened with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamSpec {
    /// Sample frames a second: always [`SAMPLE_RATE`].
    pub sample_rate: u32,
    /// The speaker layout; samples are interleaved signed 16-bit.
    pub channels: Channels,
    /// Sample frames in each push: always [`BLOCK_FRAMES`].
    pub block_frames: usize,
    /// The most sample frames the APU will ever leave queued; the sink needs
    /// room for this many (48 blocks, 12288 frames, by default).
    pub capacity_frames: usize,
    /// The device period (sample frames a callback) the APU's defaults are
    /// tuned for: 2048 on Linux, where a smaller one underruns on a desktop
    /// sound server, 512 elsewhere. A sink is free to use another.
    pub period_frames: usize,
}

/// Tells the frame thread that the sink has just drained some audio, so a
/// wait at the high watermark ends at once instead of at its next poll
/// (5.33 ms). Cheap to clone and callable from a device's audio thread: it
/// takes no lock that anything holds for long and never blocks on the APU.
#[derive(Clone, Default)]
pub struct DrainSignal(Option<Arc<dyn Fn() + Send + Sync>>);

impl DrainSignal {
    /// A signal that wakes nobody (the frame thread then polls).
    #[must_use]
    pub fn none() -> Self {
        Self(None)
    }

    /// A signal that calls `wake`.
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Self {
        Self(Some(Arc::new(wake)))
    }

    /// Wakes the frame thread if it waits for the sink to drain.
    pub fn notify(&self) {
        if let Some(wake) = &self.0 {
            wake();
        }
    }
}

impl std::fmt::Debug for DrainSignal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_some() { "DrainSignal(set)" } else { "DrainSignal(none)" })
    }
}

/// The destination of the APU's output: 48 kHz interleaved `i16`.
///
/// `open` is called once from [`crate::Apu::new`]; `push`, `queued_frames`
/// and `largest_period_frames` from the APU's frame thread only; `set_paused`
/// from whichever thread pauses the APU. None of them may block: `push` in
/// particular is called with the APU's lock held.
///
/// Front ends normally do not implement this: [`ring_sink`] gives a sink and
/// the matching [`RingSource`] for the device's callback.
pub trait AudioSink: Send + Sync {
    /// Prepares for a stream. `drained` should be notified whenever queued
    /// audio has been consumed. Returns the device's period in sample frames
    /// (how much it takes at a time), which sets the watermarks; 0 means
    /// "use `spec.period_frames`".
    ///
    /// # Errors
    /// A message saying why the stream cannot be taken (for example a ring
    /// made for another channel layout); [`crate::Apu::new`] then fails.
    fn open(&self, spec: &StreamSpec, drained: DrainSignal) -> Result<usize, String>;

    /// Queues one block: `spec.block_frames` sample frames, interleaved. The
    /// APU pushes only while `queued_frames` is below the high watermark, so
    /// a sink with `spec.capacity_frames` of room never has to drop.
    fn push(&self, samples: &[i16]);

    /// Sample frames pushed and not yet consumed by the device.
    fn queued_frames(&self) -> usize;

    /// The largest amount, in sample frames, the device has taken at once so
    /// far, if that can exceed what `open` returned (PulseAudio and PipeWire
    /// choose their own periods). The watermarks grow to follow it.
    fn largest_period_frames(&self) -> usize {
        0
    }

    /// The APU was paused or resumed. While paused the sink should keep what
    /// is queued and play silence (or stop the device), so that resuming
    /// continues where the audio left off.
    fn set_paused(&self, _paused: bool) {}
}

/// The device's first fills are left out of the largest period seen: a
/// stream asks for its whole buffer as it starts (out.c `fill`).
const WARMUP_CALLBACKS: u32 = 64;

/// One producer (the frame thread), one consumer (the device's callback):
/// two counters and a buffer, as in out.c.
struct Ring {
    channels: Channels,
    /// Samples; the slot of sample `n` is `n % buf.len()`.
    buf: Box<[AtomicI16]>,
    /// Samples written (the producer's) and read (the consumer's).
    head: AtomicU64,
    tail: AtomicU64,
    paused: AtomicBool,
    drained: RwLock<DrainSignal>,
    /// Callbacks since the first block came, up to [`WARMUP_CALLBACKS`].
    fed_callbacks: AtomicU32,
    largest_period: AtomicUsize,
    callbacks: AtomicU64,
    short_callbacks: AtomicU64,
    overflows: AtomicU64,
    consumed_frames: AtomicU64,
}

impl Ring {
    fn new(channels: Channels) -> Self {
        Self {
            channels,
            buf: (0..RING_MAX_FRAMES * channels.count()).map(|_| AtomicI16::new(0)).collect(),
            head: AtomicU64::new(0),
            tail: AtomicU64::new(0),
            paused: AtomicBool::new(false),
            drained: RwLock::new(DrainSignal::none()),
            fed_callbacks: AtomicU32::new(0),
            largest_period: AtomicUsize::new(0),
            callbacks: AtomicU64::new(0),
            short_callbacks: AtomicU64::new(0),
            overflows: AtomicU64::new(0),
            consumed_frames: AtomicU64::new(0),
        }
    }

    fn queued_samples(&self) -> usize {
        let head = self.head.load(Ordering::Acquire);
        (head - self.tail.load(Ordering::Acquire)) as usize
    }

    fn push(&self, samples: &[i16]) {
        let head = self.head.load(Ordering::Relaxed);
        let used = (head - self.tail.load(Ordering::Acquire)) as usize;
        if used + samples.len() > self.buf.len() {
            // The watermarks keep the APU well below the ring's size; this is
            // a sink reporting a fill level that is not its own.
            if self.overflows.fetch_add(1, Ordering::Relaxed) == 0 {
                log::warn!(target: "apu", "output ring full: a block of audio dropped");
            }
            return;
        }
        let len = self.buf.len() as u64;
        for (i, &s) in samples.iter().enumerate() {
            self.buf[((head + i as u64) % len) as usize].store(s, Ordering::Relaxed);
        }
        self.head.store(head + samples.len() as u64, Ordering::Release);
    }

    /// The device's callback (out.c `fill`): what the ring has, the rest
    /// silence. Never waits. Returns the sample frames that were audio.
    fn fill(&self, out: &mut [i16]) -> usize {
        let ch = self.channels.count();
        if self.paused.load(Ordering::Acquire) {
            out.fill(0);
            return 0;
        }
        let head = self.head.load(Ordering::Acquire);
        if self.fed_callbacks.load(Ordering::Relaxed) < WARMUP_CALLBACKS {
            self.fed_callbacks.fetch_add(u32::from(head != 0), Ordering::Relaxed);
        } else {
            self.largest_period.fetch_max(out.len() / ch, Ordering::Relaxed);
        }
        let tail = self.tail.load(Ordering::Relaxed);
        // whole sample frames only: a callback of an odd size must not leave
        // the channels swapped for the next one
        let n = out.len().min((head - tail) as usize) / ch * ch;
        let len = self.buf.len() as u64;
        for (i, o) in out[..n].iter_mut().enumerate() {
            *o = self.buf[((tail + i as u64) % len) as usize].load(Ordering::Relaxed);
        }
        self.tail.store(tail + n as u64, Ordering::Release);
        self.callbacks.fetch_add(1, Ordering::Relaxed);
        if n < out.len() {
            self.short_callbacks.fetch_add(1, Ordering::Relaxed);
            out[n..].fill(0);
        }
        self.consumed_frames.fetch_add((n / ch) as u64, Ordering::Relaxed);
        if let Ok(drained) = self.drained.read() {
            drained.notify();
        }
        n / ch
    }
}

/// The producer half of a ring buffer between the APU and an audio device:
/// the [`AudioSink`] to give to [`crate::Apu::new`].
pub struct RingSink {
    ring: Arc<Ring>,
    period_frames: usize,
}

/// The consumer half of a ring buffer: the device's audio callback calls
/// [`RingSource::fill`]. Cheap to clone.
#[derive(Clone)]
pub struct RingSource {
    ring: Arc<Ring>,
}

/// Counters of a ring, for logs and tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RingStats {
    /// Sample frames queued now.
    pub queued_frames: usize,
    /// Sample frames the device has taken since the ring was made.
    pub consumed_frames: u64,
    /// Calls of [`RingSource::fill`] while not paused.
    pub callbacks: u64,
    /// Those the ring could not fill completely (an audible gap).
    pub short_callbacks: u64,
    /// Blocks dropped because the ring was full.
    pub overflows: u64,
}

/// A ring buffer for a real audio device.
///
/// `period_frames` is how many sample frames the device is expected to take
/// in one callback, or 0 for the APU's default ([`StreamSpec::period_frames`]).
/// It only seeds the watermarks: the ring measures the callbacks it actually
/// gets and the APU follows the largest.
///
/// The device should be opened at 48 kHz, 16-bit, with `channels`. Its
/// callback is one line: `source.fill(buffer)`.
#[must_use]
pub fn ring_sink(channels: Channels, period_frames: usize) -> (RingSink, RingSource) {
    let ring = Arc::new(Ring::new(channels));
    (RingSink { ring: ring.clone(), period_frames }, RingSource { ring })
}

impl AudioSink for RingSink {
    fn open(&self, spec: &StreamSpec, drained: DrainSignal) -> Result<usize, String> {
        if spec.channels != self.ring.channels {
            return Err(format!(
                "the ring was made for {:?} and the APU plays {:?}",
                self.ring.channels, spec.channels
            ));
        }
        if spec.capacity_frames > RING_MAX_FRAMES {
            return Err(format!(
                "the APU wants room for {} frames, the ring has {RING_MAX_FRAMES}",
                spec.capacity_frames
            ));
        }
        if let Ok(mut slot) = self.ring.drained.write() {
            *slot = drained;
        }
        self.ring.fed_callbacks.store(0, Ordering::Relaxed);
        self.ring.largest_period.store(0, Ordering::Relaxed);
        Ok(self.period_frames)
    }

    fn push(&self, samples: &[i16]) {
        self.ring.push(samples);
    }

    fn queued_frames(&self) -> usize {
        self.ring.queued_samples() / self.ring.channels.count()
    }

    fn largest_period_frames(&self) -> usize {
        self.ring.largest_period.load(Ordering::Relaxed)
    }

    fn set_paused(&self, paused: bool) {
        self.ring.paused.store(paused, Ordering::Release);
    }
}

impl RingSource {
    /// Fills a device buffer of interleaved samples: queued audio first, then
    /// silence if the ring runs dry. While the APU is paused it gives silence
    /// and keeps what is queued. Never blocks and never allocates, so it may
    /// be called from a real-time audio thread. Returns how many sample
    /// frames were audio.
    pub fn fill(&self, out: &mut [i16]) -> usize {
        self.ring.fill(out)
    }

    /// The layout the ring was made for.
    #[must_use]
    pub fn channels(&self) -> Channels {
        self.ring.channels
    }

    /// The ring's counters.
    #[must_use]
    pub fn stats(&self) -> RingStats {
        RingStats {
            queued_frames: self.ring.queued_samples() / self.ring.channels.count(),
            consumed_frames: self.ring.consumed_frames.load(Ordering::Relaxed),
            callbacks: self.ring.callbacks.load(Ordering::Relaxed),
            short_callbacks: self.ring.short_callbacks.load(Ordering::Relaxed),
            overflows: self.ring.overflows.load(Ordering::Relaxed),
        }
    }
}

/// A sink with no audio device: a thread takes the audio at the rate a
/// device would, by the host's clock, and throws it away. The APU keeps its
/// timing (voices end, notifications and interrupts come when they should),
/// so a title runs the same with or without sound.
pub struct NullSink {
    ring: Arc<Ring>,
    period_frames: usize,
    drain: Mutex<Option<Drain>>,
}

struct Drain {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

impl NullSink {
    /// A null sink for `channels` that drains `period_frames` sample frames
    /// at a time (0: the APU's default period).
    #[must_use]
    pub fn new(channels: Channels, period_frames: usize) -> Self {
        Self { ring: Arc::new(Ring::new(channels)), period_frames, drain: Mutex::new(None) }
    }

    /// The counters of the ring the drain thread empties.
    #[must_use]
    pub fn stats(&self) -> RingStats {
        RingSource { ring: self.ring.clone() }.stats()
    }

    fn stop_drain(&self) {
        let drain = self.drain.lock().ok().and_then(|mut d| d.take());
        if let Some(d) = drain {
            d.stop.store(true, Ordering::Release);
            d.thread.thread().unpark();
            let _ = d.thread.join();
        }
    }
}

impl Default for NullSink {
    fn default() -> Self {
        Self::new(Channels::Stereo, 0)
    }
}

impl AudioSink for NullSink {
    fn open(&self, spec: &StreamSpec, drained: DrainSignal) -> Result<usize, String> {
        if spec.channels != self.ring.channels {
            return Err(format!(
                "the null sink was made for {:?} and the APU plays {:?}",
                self.ring.channels, spec.channels
            ));
        }
        self.stop_drain();
        if let Ok(mut slot) = self.ring.drained.write() {
            *slot = drained;
        }
        let period = if self.period_frames == 0 { spec.period_frames } else { self.period_frames };
        let period = period.clamp(1, RING_MAX_FRAMES);
        let stop = Arc::new(AtomicBool::new(false));
        let (ring, stop_flag) = (self.ring.clone(), stop.clone());
        let thread = thread::Builder::new()
            .name("apu.drain".into())
            .spawn(move || {
                xb_types::thread::pacing();
                drain_thread(&ring, period, &stop_flag);
            })
            .map_err(|e| format!("the drain thread did not start: {e}"))?;
        if let Ok(mut d) = self.drain.lock() {
            *d = Some(Drain { stop, thread });
        }
        Ok(period)
    }

    fn push(&self, samples: &[i16]) {
        self.ring.push(samples);
    }

    fn queued_frames(&self) -> usize {
        self.ring.queued_samples() / self.ring.channels.count()
    }

    fn set_paused(&self, paused: bool) {
        self.ring.paused.store(paused, Ordering::Release);
    }
}

impl Drop for NullSink {
    fn drop(&mut self) {
        self.stop_drain();
    }
}

/// One period of audio taken every period of host time, on absolute
/// deadlines so that a late wake-up does not slow the rate.
fn drain_thread(ring: &Ring, period_frames: usize, stop: &AtomicBool) {
    let mut scratch = vec![0i16; period_frames * ring.channels.count()];
    let period =
        Duration::from_nanos(period_frames as u64 * 1_000_000_000 / u64::from(SAMPLE_RATE));
    let mut next = Instant::now();
    while !stop.load(Ordering::Acquire) {
        next += period;
        loop {
            let left = next.saturating_duration_since(Instant::now());
            if left.is_zero() || stop.load(Ordering::Acquire) {
                break;
            }
            thread::park_timeout(left);
        }
        ring.fill(&mut scratch);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(channels: Channels) -> StreamSpec {
        StreamSpec {
            sample_rate: SAMPLE_RATE,
            channels,
            block_frames: BLOCK_FRAMES,
            capacity_frames: 48 * BLOCK_FRAMES,
            period_frames: 2048,
        }
    }

    #[test]
    fn the_ring_returns_what_was_pushed_in_order_and_pads_with_silence() {
        let (sink, source) = ring_sink(Channels::Stereo, 0);
        sink.open(&spec(Channels::Stereo), DrainSignal::none()).unwrap();
        let block: Vec<i16> = (1..=(BLOCK_FRAMES as i16 * 2)).collect();
        sink.push(&block);
        assert_eq!(sink.queued_frames(), BLOCK_FRAMES);

        let mut out = vec![7i16; 300 * 2];
        assert_eq!(source.fill(&mut out), BLOCK_FRAMES);
        assert_eq!(&out[..block.len()], &block[..]);
        assert!(out[block.len()..].iter().all(|&s| s == 0));
        assert_eq!(sink.queued_frames(), 0);
        let stats = source.stats();
        assert_eq!(
            (stats.callbacks, stats.short_callbacks, stats.consumed_frames),
            (1, 1, BLOCK_FRAMES as u64)
        );
    }

    #[test]
    fn the_ring_wraps_without_losing_or_reordering_samples() {
        let (sink, source) = ring_sink(Channels::Stereo, 0);
        sink.open(&spec(Channels::Stereo), DrainSignal::none()).unwrap();
        let mut next_in = 0i16;
        let mut next_out = 0i16;
        // far more than the ring's size goes through it, in pieces that do
        // not divide it
        for _ in 0..(RING_MAX_FRAMES / BLOCK_FRAMES) * 3 {
            let block: Vec<i16> = (0..BLOCK_FRAMES * 2)
                .map(|_| {
                    next_in = next_in.wrapping_add(1);
                    next_in
                })
                .collect();
            sink.push(&block);
            let mut out = vec![0i16; 200 * 2];
            let got = source.fill(&mut out);
            for &s in &out[..got * 2] {
                next_out = next_out.wrapping_add(1);
                assert_eq!(s, next_out);
            }
        }
        assert_eq!(source.stats().overflows, 0);
    }

    #[test]
    fn a_paused_ring_plays_silence_and_keeps_its_audio() {
        let (sink, source) = ring_sink(Channels::Stereo, 0);
        sink.open(&spec(Channels::Stereo), DrainSignal::none()).unwrap();
        sink.push(&vec![5i16; BLOCK_FRAMES * 2]);
        sink.set_paused(true);
        let mut out = vec![1i16; 64];
        assert_eq!(source.fill(&mut out), 0);
        assert!(out.iter().all(|&s| s == 0));
        assert_eq!(sink.queued_frames(), BLOCK_FRAMES);
        sink.set_paused(false);
        assert_eq!(source.fill(&mut out), 32);
        assert!(out.iter().all(|&s| s == 5));
    }

    #[test]
    fn a_full_ring_drops_the_block_instead_of_overwriting() {
        let (sink, source) = ring_sink(Channels::Stereo, 0);
        sink.open(&spec(Channels::Stereo), DrainSignal::none()).unwrap();
        let block = vec![1i16; BLOCK_FRAMES * 2];
        for _ in 0..RING_MAX_FRAMES / BLOCK_FRAMES + 2 {
            sink.push(&block);
        }
        assert_eq!(sink.queued_frames(), RING_MAX_FRAMES);
        assert_eq!(source.stats().overflows, 2);
    }

    #[test]
    fn the_largest_period_ignores_the_devices_first_fills() {
        let (sink, source) = ring_sink(Channels::Stereo, 0);
        sink.open(&spec(Channels::Stereo), DrainSignal::none()).unwrap();
        sink.push(&vec![0i16; BLOCK_FRAMES * 2]);
        let mut big = vec![0i16; 4096 * 2];
        source.fill(&mut big);
        assert_eq!(sink.largest_period_frames(), 0);
        let mut small = vec![0i16; 600 * 2];
        for _ in 0..WARMUP_CALLBACKS + 1 {
            source.fill(&mut small);
        }
        assert_eq!(sink.largest_period_frames(), 600);
    }

    #[test]
    fn a_ring_refuses_a_stream_of_another_layout() {
        let (sink, _source) = ring_sink(Channels::Stereo, 0);
        assert!(sink.open(&spec(Channels::Surround51), DrainSignal::none()).is_err());
    }

    #[test]
    fn a_drain_signal_is_raised_by_each_callback() {
        let (sink, source) = ring_sink(Channels::Stereo, 0);
        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        sink.open(
            &spec(Channels::Stereo),
            DrainSignal::new(move || {
                c.fetch_add(1, Ordering::SeqCst);
            }),
        )
        .unwrap();
        let mut out = vec![0i16; 64];
        source.fill(&mut out);
        source.fill(&mut out);
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn the_null_sink_drains_at_the_sample_rate_by_the_clock() {
        // 10 ms periods; kept fed for 0.6 s, it must take 48000 frames a
        // second: no faster (it is the APU's only brake) and no slower.
        let sink = NullSink::new(Channels::Stereo, 480);
        assert_eq!(sink.open(&spec(Channels::Stereo), DrainSignal::none()).unwrap(), 480);
        let block = vec![0i16; BLOCK_FRAMES * 2];
        let start = Instant::now();
        let before = sink.stats().consumed_frames;
        while start.elapsed() < Duration::from_millis(600) {
            while sink.queued_frames() < 4 * 480 {
                sink.push(&block);
            }
            thread::sleep(Duration::from_millis(1));
        }
        let consumed = (sink.stats().consumed_frames - before) as f64;
        let expected = start.elapsed().as_secs_f64() * f64::from(SAMPLE_RATE);
        assert!(
            (consumed - expected).abs() <= 3.0 * 480.0,
            "consumed {consumed} frames where the clock says {expected}"
        );
    }

    #[test]
    fn a_paused_null_sink_takes_nothing() {
        let sink = NullSink::new(Channels::Stereo, 480);
        sink.open(&spec(Channels::Stereo), DrainSignal::none()).unwrap();
        sink.set_paused(true);
        sink.push(&vec![0i16; BLOCK_FRAMES * 2]);
        thread::sleep(Duration::from_millis(60));
        assert_eq!(sink.queued_frames(), BLOCK_FRAMES);
        sink.set_paused(false);
        thread::sleep(Duration::from_millis(60));
        assert_eq!(sink.queued_frames(), 0);
    }
}
