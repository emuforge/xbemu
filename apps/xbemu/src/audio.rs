//! Audio output through `cpal`.
//!
//! The core plays 48 kHz stereo `i16` into a ring ([`xb_core::ring_sink`]);
//! the device's callback drains it with [`RingSource::fill`] and plays
//! silence for whatever the ring did not have. The device is opened at
//! 48 kHz stereo `i16` when it offers that. Otherwise the nearest
//! configuration is taken and the callback converts: the sample format, the
//! channel count (mono gets the average of left and right, more than two
//! channels get left and right in the first two) and, if the device cannot
//! run at 48 kHz at all, the rate, with a plain linear resampler (audible
//! as slightly dull treble; a device that cannot do 48 kHz is rare enough
//! not to justify a better one).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample, StreamConfig};
use xb_core::{Channels, RingSink, RingSource, ring_sink};

/// The core's output rate.
pub const CORE_RATE: u32 = 48_000;

/// The callback size asked of the device, in frames (43 ms). It is the
/// reference's default on Linux: a smaller callback underruns on a desktop
/// sound server, and one left to the server (`BufferSize::Default`) can be
/// buffered for about two seconds, with the sound trailing the picture by
/// as much.
pub const PERIOD_FRAMES: u32 = 2048;

/// Overrides [`PERIOD_FRAMES`] for a run; 0 leaves the size to the device.
const PERIOD_ENV: &str = "XBEMU_AUDIO_PERIOD";

/// The most device frames converted in one step; a longer callback is done
/// in several, so the scratch buffers never grow on the audio thread.
const CHUNK_FRAMES: usize = 16_384;

/// One in 32.32 fixed point.
const ONE: u64 = 1 << 32;

/// A linear resampler from the core's rate to the device's, stereo.
///
/// Fixed point, so the number of source frames a block needs
/// ([`Resampler::needed`]) is exact and splitting a block changes nothing.
/// The output lags the input by one source frame.
#[derive(Clone, Debug)]
pub struct Resampler {
    /// Source frames per output frame, 32.32.
    step: u64,
    /// Where the next output frame lies between `prev` and `next`, 32.32;
    /// at `ONE` or more, source frames are due first.
    phase: u64,
    prev: [i16; 2],
    next: [i16; 2],
}

impl Resampler {
    /// A resampler from `from_rate` to `to_rate` frames a second.
    #[must_use]
    pub fn new(from_rate: u32, to_rate: u32) -> Self {
        let step = (u64::from(from_rate) << 32) / u64::from(to_rate.max(1));
        Self { step, phase: ONE, prev: [0; 2], next: [0; 2] }
    }

    /// The source frames the next `out_frames` output frames consume.
    #[must_use]
    pub fn needed(&self, out_frames: usize) -> usize {
        match out_frames {
            0 => 0,
            n => ((self.phase + self.step * (n as u64 - 1)) >> 32) as usize,
        }
    }

    /// The most source frames a block of [`CHUNK_FRAMES`] can consume, at
    /// any phase.
    fn most_needed(&self) -> usize {
        ((self.step * CHUNK_FRAMES as u64) >> 32) as usize + 1
    }

    /// Fills `out` (interleaved stereo) from `src`, which must hold exactly
    /// [`Resampler::needed`] frames for it.
    pub fn process(&mut self, src: &[i16], out: &mut [i16]) {
        let mut src = src.as_chunks::<2>().0.iter();
        for frame in out.as_chunks_mut::<2>().0 {
            while self.phase >= ONE {
                self.prev = self.next;
                // `needed` frames were given; a short `src` repeats the last.
                self.next = src.next().copied().unwrap_or(self.next);
                self.phase -= ONE;
            }
            for ((o, &a), &b) in frame.iter_mut().zip(&self.prev).zip(&self.next) {
                let (a, b) = (i64::from(a), i64::from(b));
                *o = (a + (((b - a) * self.phase as i64) >> 32)) as i16;
            }
            self.phase += self.step;
        }
    }
}

/// Writes interleaved stereo frames to a device buffer of `channels`
/// channels in the device's sample format.
fn write_frames<T: Sample + FromSample<i16>>(stereo: &[i16], channels: usize, out: &mut [T]) {
    for (s, frame) in stereo.as_chunks::<2>().0.iter().zip(out.chunks_exact_mut(channels)) {
        match frame {
            [mono] => *mono = T::from_sample(((i32::from(s[0]) + i32::from(s[1])) / 2) as i16),
            [left, right, rest @ ..] => {
                *left = T::from_sample(s[0]);
                *right = T::from_sample(s[1]);
                rest.fill(T::EQUILIBRIUM);
            }
            [] => {}
        }
    }
}

/// The device callback's state: drains the ring and converts to what the
/// device plays.
pub struct Converter {
    source: RingSource,
    channels: usize,
    resampler: Option<Resampler>,
    /// Stereo frames at the core's rate, when resampling.
    core_rate: Vec<i16>,
    /// Stereo frames at the device's rate.
    device_rate: Vec<i16>,
}

impl Converter {
    /// A converter from `source` (stereo, 48 kHz) to a device with
    /// `channels` channels at `rate`.
    #[must_use]
    pub fn new(source: RingSource, channels: usize, rate: u32) -> Self {
        let resampler = (rate != CORE_RATE).then(|| Resampler::new(CORE_RATE, rate));
        let core_frames = resampler.as_ref().map_or(0, Resampler::most_needed);
        Self {
            source,
            channels: channels.max(1),
            resampler,
            core_rate: vec![0; core_frames * 2],
            device_rate: vec![0; CHUNK_FRAMES * 2],
        }
    }

    /// Fills a device buffer. Never blocks or allocates.
    pub fn render<T: Sample + FromSample<i16>>(&mut self, out: &mut [T]) {
        for chunk in out.chunks_mut(CHUNK_FRAMES * self.channels) {
            let frames = chunk.len() / self.channels;
            let stereo = &mut self.device_rate[..frames * 2];
            match &mut self.resampler {
                None => {
                    self.source.fill(stereo);
                }
                Some(resampler) => {
                    let src = &mut self.core_rate[..resampler.needed(frames) * 2];
                    if !src.is_empty() {
                        self.source.fill(src);
                    }
                    resampler.process(src, stereo);
                }
            }
            write_frames(stereo, self.channels, chunk);
            // A buffer that is not whole frames ends in silence.
            chunk[frames * self.channels..].fill(T::EQUILIBRIUM);
        }
    }
}

/// A stream configuration the device offers, reduced to what the choice
/// depends on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Candidate {
    /// Channels.
    pub channels: u16,
    /// The rate to open at: 48 kHz if the configuration's range has it,
    /// else the end of the range nearest to it.
    pub rate: u32,
    /// The sample format.
    pub format: SampleFormat,
}

/// How well the callback's conversions suit a sample format: 0 for a format
/// it cannot write, more for less loss.
const fn format_rank(format: SampleFormat) -> u8 {
    match format {
        SampleFormat::I16 => 6,
        SampleFormat::F32 => 5,
        SampleFormat::I32 => 4,
        SampleFormat::F64 => 3,
        SampleFormat::U16 => 2,
        SampleFormat::I8 | SampleFormat::U8 => 1,
        _ => 0,
    }
}

/// The configuration to open, from the device's offers as (channels, lowest
/// rate, highest rate, format). Most important first: running at 48 kHz (no
/// resampling), a rate near it, stereo over more channels over mono, then
/// the sample format.
#[must_use]
pub fn choose(
    offers: impl IntoIterator<Item = (u16, u32, u32, SampleFormat)>,
) -> Option<Candidate> {
    offers
        .into_iter()
        .filter(|&(channels, _, _, format)| channels > 0 && format_rank(format) > 0)
        .map(|(channels, min, max, format)| Candidate {
            channels,
            rate: CORE_RATE.clamp(min, max.max(min)),
            format,
        })
        .max_by_key(|c| {
            let layout = match c.channels {
                2 => (2, 0),
                1 => (0, 0),
                n => (1, -i32::from(n)),
            };
            (std::cmp::Reverse(c.rate.abs_diff(CORE_RATE)), layout, format_rank(c.format))
        })
}

/// An open, playing output stream. Dropping it closes the device.
pub struct AudioOutput {
    _stream: cpal::Stream,
    /// The consumer half of the ring, for its counters.
    pub source: RingSource,
    /// How long after a callback its audio is heard, as the device reports
    /// it: the last callback's figure, in microseconds.
    pub device_latency_us: Arc<AtomicU64>,
}

fn build<T>(
    device: &cpal::Device,
    config: StreamConfig,
    mut converter: Converter,
    latency_us: Arc<AtomicU64>,
) -> Result<cpal::Stream, cpal::Error>
where
    T: SizedSample + FromSample<i16>,
{
    device.build_output_stream(
        config,
        move |data: &mut [T], info: &cpal::OutputCallbackInfo| {
            let at = info.timestamp();
            let ahead = at.playback.duration_since(at.callback);
            latency_us.store(ahead.as_micros() as u64, Ordering::Relaxed);
            converter.render(data);
        },
        |e| match e.kind() {
            cpal::ErrorKind::Xrun => log::debug!(target: "app", "audio: {e}"),
            _ => log::warn!(target: "app", "audio: {e}"),
        },
        None,
    )
}

/// Opens `host`'s default output device and starts it playing from `source`.
fn open_on(
    host: &cpal::Host,
    source: &RingSource,
    latency_us: &Arc<AtomicU64>,
) -> Result<cpal::Stream, String> {
    let host_name = host.id().name();
    let device = host.default_output_device().ok_or(format!("{host_name}: no output device"))?;
    let name = device.description().map_or_else(|_| "?".to_owned(), |d| d.to_string());
    let name = format!("{host_name} '{name}'");
    let offers: Vec<_> = device
        .supported_output_configs()
        .map_err(|e| format!("{name}: cannot list configurations: {e}"))?
        .map(|r| (r.channels(), r.min_sample_rate(), r.max_sample_rate(), r.sample_format()))
        .collect();
    let chosen = choose(offers).ok_or(format!("{name}: no usable configuration"))?;
    let period =
        std::env::var(PERIOD_ENV).ok().and_then(|v| v.parse().ok()).unwrap_or(PERIOD_FRAMES);
    let sizes = [cpal::BufferSize::Fixed(period), cpal::BufferSize::Default];
    let mut opened = Err(format!("{name}: no stream"));
    // The size asked for first; a device that refuses it gets its own.
    for buffer_size in sizes.into_iter().skip(usize::from(period == 0)) {
        let config =
            StreamConfig { channels: chosen.channels, sample_rate: chosen.rate, buffer_size };
        let converter = Converter::new(source.clone(), usize::from(chosen.channels), chosen.rate);
        let built = match chosen.format {
            SampleFormat::I16 => build::<i16>(&device, config, converter, latency_us.clone()),
            SampleFormat::F32 => build::<f32>(&device, config, converter, latency_us.clone()),
            SampleFormat::I32 => build::<i32>(&device, config, converter, latency_us.clone()),
            SampleFormat::F64 => build::<f64>(&device, config, converter, latency_us.clone()),
            SampleFormat::U16 => build::<u16>(&device, config, converter, latency_us.clone()),
            SampleFormat::I8 => build::<i8>(&device, config, converter, latency_us.clone()),
            SampleFormat::U8 => build::<u8>(&device, config, converter, latency_us.clone()),
            other => return Err(format!("{name}: sample format {other} is not supported")),
        };
        match built {
            Ok(stream) => {
                opened = Ok((stream, buffer_size));
                break;
            }
            Err(e) => {
                log::debug!(target: "app", "audio: {name}: buffer size {buffer_size:?} refused: {e}");
                opened = Err(format!("{name}: cannot open a stream: {e}"));
            }
        }
    }
    let (stream, buffer_size) = opened?;
    stream.play().map_err(|e| format!("{name}: cannot start the stream: {e}"))?;

    log::info!(
        target: "app",
        "audio: {name}, {} Hz, {} channel(s), {}, {}{}",
        chosen.rate,
        chosen.channels,
        chosen.format,
        match buffer_size {
            cpal::BufferSize::Fixed(n) => format!("callbacks of {n} frames asked"),
            cpal::BufferSize::Default => "the device's own callback size".to_owned(),
        },
        if chosen.rate == CORE_RATE { "" } else { " (resampled from 48000 Hz, linear)" },
    );
    Ok(stream)
}

/// Opens an output device and starts it playing from a new ring: the
/// default device of the host's preferred audio system, or failing that of
/// the others it has. Returns the ring's producer half, for the core, and
/// the stream, which must be kept until the core has shut down.
///
/// # Errors
/// A sentence saying why there is no audio; the caller logs it and runs the
/// core with a `NullSink`.
pub fn open() -> Result<(RingSink, AudioOutput), String> {
    let (sink, source) = ring_sink(Channels::Stereo, PERIOD_FRAMES as usize);
    let preferred = cpal::default_host();
    let preferred_id = preferred.id();
    let others = cpal::available_hosts()
        .into_iter()
        .filter(move |id| *id != preferred_id)
        .filter_map(|id| cpal::host_from_id(id).ok());
    let mut failures = Vec::new();
    let device_latency_us = Arc::new(AtomicU64::new(0));
    for host in std::iter::once(preferred).chain(others) {
        match open_on(&host, &source, &device_latency_us) {
            Ok(stream) => {
                return Ok((sink, AudioOutput { _stream: stream, source, device_latency_us }));
            }
            Err(why) => failures.push(why),
        }
    }
    Err(failures.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use xb_core::AudioSink;

    /// A stereo test signal: a slow ramp left, its negative right.
    fn ramp(frames: usize) -> Vec<i16> {
        (0..frames).flat_map(|i| [(i * 7 % 2000) as i16, -((i * 7 % 2000) as i16)]).collect()
    }

    fn resample(r: &mut Resampler, src: &[i16], taken: &mut usize, out_frames: usize) -> Vec<i16> {
        let need = r.needed(out_frames);
        let mut out = vec![0; out_frames * 2];
        r.process(&src[*taken * 2..(*taken + need) * 2], &mut out);
        *taken += need;
        out
    }

    #[test]
    fn at_equal_rates_the_resampler_is_a_one_frame_delay() {
        let src = ramp(500);
        let mut r = Resampler::new(48_000, 48_000);
        let mut taken = 0;
        let out = resample(&mut r, &src, &mut taken, 500);
        assert_eq!(taken, 500);
        assert_eq!(&out[..2], &[0, 0]);
        assert_eq!(&out[2..], &src[..998]);
    }

    #[test]
    fn splitting_a_block_changes_neither_the_output_nor_what_is_consumed() {
        for rate in [44_100, 96_000, 22_050, 32_000] {
            let src = ramp(4000);
            let (mut whole, mut parts) =
                (Resampler::new(48_000, rate), Resampler::new(48_000, rate));
            let (mut taken_whole, mut taken_parts) = (0, 0);
            let expected = resample(&mut whole, &src, &mut taken_whole, 1500);
            let mut got = Vec::new();
            for n in [1, 0, 399, 2, 1000, 98] {
                got.extend(resample(&mut parts, &src, &mut taken_parts, n));
            }
            assert_eq!(got, expected, "{rate} Hz");
            assert_eq!(taken_parts, taken_whole, "{rate} Hz");
        }
    }

    #[test]
    fn the_resampler_consumes_source_frames_in_the_ratio_of_the_rates() {
        for rate in [44_100u32, 96_000, 8_000] {
            let mut r = Resampler::new(48_000, rate);
            let src = vec![0i16; 2 * 48_000 + 16];
            let mut taken = 0;
            // One second of output, in device-sized blocks.
            for _ in 0..rate / 100 {
                resample(&mut r, &src, &mut taken, 100);
            }
            // Frames are taken only when an output frame needs them, so the
            // count trails by up to one output frame's worth.
            let slack = (48_000 / rate) as usize + 1;
            assert!(taken.abs_diff(48_000) <= slack, "{rate} Hz took {taken}");
        }
    }

    #[test]
    fn a_steady_level_stays_steady_and_output_stays_between_its_neighbours() {
        let mut r = Resampler::new(48_000, 44_100);
        let steady: Vec<i16> = std::iter::repeat_n([1234i16, -4321], 1000).flatten().collect();
        let mut taken = 0;
        let out = resample(&mut r, &steady, &mut taken, 800);
        // The first frames fade in from the silence before the stream.
        assert!(out[4..].as_chunks::<2>().0.iter().all(|f| *f == [1234, -4321]));

        let mut r = Resampler::new(48_000, 96_000);
        let src = ramp(200);
        let mut taken = 0;
        let out = resample(&mut r, &src, &mut taken, 300);
        let (lo, hi) = (*src.iter().min().unwrap(), *src.iter().max().unwrap());
        assert!(out.iter().all(|s| (lo..=hi).contains(s)));
    }

    #[test]
    fn stereo_goes_to_any_channel_count_and_format() {
        let stereo = [1000i16, -3000, i16::MAX, i16::MAX];
        let mut same = [0i16; 4];
        write_frames(&stereo, 2, &mut same);
        assert_eq!(same, stereo);

        let mut mono = [0i16; 2];
        write_frames(&stereo, 1, &mut mono);
        assert_eq!(mono, [-1000, i16::MAX]);

        let mut surround = [7i16; 12];
        write_frames(&stereo, 6, &mut surround);
        assert_eq!(surround, [1000, -3000, 0, 0, 0, 0, i16::MAX, i16::MAX, 0, 0, 0, 0]);

        let mut float = [9f32; 4];
        write_frames(&stereo, 2, &mut float);
        assert!((float[0] - 1000.0 / 32768.0).abs() < 1e-6 && float[1] < 0.0);
        assert!(float[2] > 0.9999 && float[2] <= 1.0);

        let mut unsigned = [0u16; 2];
        write_frames(&[0, i16::MIN], 2, &mut unsigned);
        assert_eq!(unsigned, [32768, 0]);
    }

    #[test]
    fn the_callback_plays_the_ring_and_then_silence() {
        let (sink, source) = ring_sink(Channels::Stereo, PERIOD_FRAMES as usize);
        let block = ramp(256);
        sink.push(&block);
        let mut converter = Converter::new(source, 2, CORE_RATE);
        let mut out = [1i16; 2 * 300];
        converter.render(&mut out);
        assert_eq!(&out[..512], &block[..]);
        assert!(out[512..].iter().all(|&s| s == 0));
        assert_eq!(sink.queued_frames(), 0);
    }

    #[test]
    fn a_resampling_callback_takes_from_the_ring_at_the_core_rate() {
        let (sink, source) = ring_sink(Channels::Stereo, PERIOD_FRAMES as usize);
        for _ in 0..8 {
            sink.push(&ramp(256));
        }
        let mut converter = Converter::new(source, 1, 24_000);
        let mut out = [0f32; 512];
        converter.render(&mut out);
        // 512 mono frames at half the rate are 1024 of the ring's.
        assert!(sink.queued_frames().abs_diff(8 * 256 - 1024) <= 1);
        // The ramp is left = -right, so the mono mix is silence.
        assert!(out.iter().all(|s| s.abs() < 1e-3));
    }

    #[test]
    fn the_longest_callback_fits_the_scratch_buffers_at_any_rate() {
        for rate in [8_000, 11_025, 44_100, 48_000, 96_000, 192_000] {
            let (_sink, source) = ring_sink(Channels::Stereo, 0);
            let mut converter = Converter::new(source, 2, rate);
            let mut out = vec![1i16; CHUNK_FRAMES * 2 * 2 + 2];
            // Twice, so both the first phase and a later one are covered.
            for _ in 0..2 {
                converter.render(&mut out);
                assert!(out.iter().all(|&s| s == 0), "{rate} Hz: an empty ring is silence");
            }
            if let Some(r) = &converter.resampler {
                assert!(r.needed(CHUNK_FRAMES) <= r.most_needed(), "{rate} Hz");
            }
        }
    }

    #[test]
    fn the_core_format_is_chosen_when_the_device_offers_it() {
        let offers = [
            (2, 44_100, 192_000, SampleFormat::F32),
            (2, 8_000, 96_000, SampleFormat::I16),
            (6, 48_000, 48_000, SampleFormat::I16),
            (1, 48_000, 48_000, SampleFormat::I16),
        ];
        assert_eq!(
            choose(offers),
            Some(Candidate { channels: 2, rate: 48_000, format: SampleFormat::I16 })
        );
    }

    #[test]
    fn the_rate_matters_more_than_the_layout_and_the_layout_more_than_the_format() {
        let offers =
            [(2, 44_100, 44_100, SampleFormat::I16), (6, 48_000, 48_000, SampleFormat::F32)];
        assert_eq!(
            choose(offers),
            Some(Candidate { channels: 6, rate: 48_000, format: SampleFormat::F32 })
        );
        let offers =
            [(1, 48_000, 48_000, SampleFormat::I16), (2, 48_000, 48_000, SampleFormat::U8)];
        assert_eq!(choose(offers).map(|c| c.channels), Some(2));
    }

    #[test]
    fn without_48_khz_the_nearest_rate_is_taken() {
        let offers =
            [(2, 8_000, 44_100, SampleFormat::I16), (2, 88_200, 96_000, SampleFormat::I16)];
        assert_eq!(choose(offers).map(|c| c.rate), Some(44_100));
        assert_eq!(choose([(2, 96_000, 192_000, SampleFormat::F32)]).map(|c| c.rate), Some(96_000));
        assert_eq!(choose([(2, 48_000, 48_000, SampleFormat::I24)]), None);
        assert_eq!(choose([]), None);
    }
}
