//! Live spectrum analyser: a `Source` wrapper taps the samples that are being
//! handed to the sound card, and `Spectrum` turns the most recent ones into
//! log-spaced frequency bars.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rodio::source::SeekError;
use rodio::{ChannelCount, Sample, SampleRate, Source};
use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};

const FFT_SIZE: usize = 4096;
/// Tuned on real tracks so bars average about half height and rarely clip.
const GAIN_DB: f32 = 12.0;
const RANGE_DB: f32 = 60.0;
/// Samples are handed to the UI thread in batches to keep locking rare.
const BATCH: usize = 512;

/// Most recent mono samples of whatever is playing.
pub struct Ring {
    samples: VecDeque<f32>,
    rate: u32,
}

impl Ring {
    pub fn shared() -> Arc<Mutex<Ring>> {
        Arc::new(Mutex::new(Ring {
            samples: VecDeque::with_capacity(FFT_SIZE * 2),
            rate: 44_100,
        }))
    }

    pub fn clear(&mut self) {
        self.samples.clear();
    }

    fn push(&mut self, batch: &[f32], rate: u32) {
        self.rate = rate;
        self.samples.extend(batch);
        let excess = self.samples.len().saturating_sub(FFT_SIZE);
        self.samples.drain(..excess);
    }
}

/// Passes audio through unchanged while copying a mono mixdown into a `Ring`.
pub struct Tap<S> {
    inner: S,
    ring: Arc<Mutex<Ring>>,
    batch: Vec<f32>,
    frame_sum: f32,
    frame_pos: u16,
}

impl<S: Source> Tap<S> {
    pub fn new(inner: S, ring: Arc<Mutex<Ring>>) -> Self {
        Self { inner, ring, batch: Vec::with_capacity(BATCH), frame_sum: 0.0, frame_pos: 0 }
    }
}

impl<S: Source> Iterator for Tap<S> {
    type Item = Sample;

    fn next(&mut self) -> Option<Sample> {
        let s = self.inner.next()?;
        let channels = self.inner.channels().get();
        self.frame_sum += s;
        self.frame_pos += 1;
        if self.frame_pos >= channels {
            self.batch.push(self.frame_sum / channels as f32);
            self.frame_sum = 0.0;
            self.frame_pos = 0;
            if self.batch.len() >= BATCH {
                // Never block the audio thread; dropping a batch is harmless.
                if let Ok(mut ring) = self.ring.try_lock() {
                    ring.push(&self.batch, self.inner.sample_rate().get());
                }
                self.batch.clear();
            }
        }
        Some(s)
    }
}

impl<S: Source> Source for Tap<S> {
    fn current_span_len(&self) -> Option<usize> {
        self.inner.current_span_len()
    }

    fn channels(&self) -> ChannelCount {
        self.inner.channels()
    }

    fn sample_rate(&self) -> SampleRate {
        self.inner.sample_rate()
    }

    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }

    fn try_seek(&mut self, pos: Duration) -> Result<(), SeekError> {
        self.inner.try_seek(pos)
    }
}

pub struct Spectrum {
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    scratch: Vec<Complex<f32>>,
    /// Smoothed bar heights, 0.0..=1.0.
    pub bars: Vec<f32>,
    /// Slowly falling peak markers, 0.0..=1.0.
    pub peaks: Vec<f32>,
    last_update: Option<Instant>,
}

impl Spectrum {
    pub fn new() -> Self {
        let window = (0..FFT_SIZE)
            .map(|i| {
                let x = i as f32 / (FFT_SIZE - 1) as f32;
                0.5 - 0.5 * (2.0 * std::f32::consts::PI * x).cos() // Hann
            })
            .collect();
        Self {
            fft: FftPlanner::new().plan_fft_forward(FFT_SIZE),
            window,
            scratch: vec![Complex::default(); FFT_SIZE],
            bars: Vec::new(),
            peaks: Vec::new(),
            last_update: None,
        }
    }

    /// Recompute `n` bars. With `active == false` the bars just fall.
    pub fn update(&mut self, ring: &Mutex<Ring>, n: usize, active: bool) {
        self.bars.resize(n, 0.0);
        self.peaks.resize(n, 0.0);
        let targets = if active { self.analyse(ring, n) } else { vec![0.0; n] };

        // Motion is time-based so it looks the same at any frame rate.
        let now = Instant::now();
        let dt = self.last_update.map_or(1.0 / 60.0, |t| (now - t).as_secs_f32()).min(0.1);
        self.last_update = Some(now);
        let attack = 1.0 - 0.02_f32.powf(dt); // ~70% of the way per 1/30s
        let (fall, peak_fall) = (1.4 * dt, 0.35 * dt); // screen-heights per second

        for ((bar, peak), target) in self.bars.iter_mut().zip(&mut self.peaks).zip(targets) {
            *bar = if target > *bar { *bar + (target - *bar) * attack } else { (*bar - fall).max(target) };
            *peak = if *bar >= *peak { *bar } else { (*peak - peak_fall).max(0.0) };
        }
    }

    fn analyse(&mut self, ring: &Mutex<Ring>, n: usize) -> Vec<f32> {
        let rate = {
            let ring = ring.lock().unwrap();
            let pad = FFT_SIZE - ring.samples.len();
            for (i, slot) in self.scratch.iter_mut().enumerate() {
                let s = if i < pad { 0.0 } else { ring.samples[i - pad] };
                *slot = Complex::new(s * self.window[i], 0.0);
            }
            ring.rate as f32
        };
        self.fft.process(&mut self.scratch);

        let bin_hz = rate / FFT_SIZE as f32;
        let (lo, hi) = (35.0_f32, 16_000.0_f32.min(rate / 2.0));
        (0..n)
            .map(|i| {
                let f0 = lo * (hi / lo).powf(i as f32 / n as f32);
                let f1 = lo * (hi / lo).powf((i + 1) as f32 / n as f32);
                let b0 = ((f0 / bin_hz) as usize).max(1);
                let b1 = ((f1 / bin_hz) as usize).max(b0 + 1).min(FFT_SIZE / 2);
                let bins = &self.scratch[b0..b1];
                let power = bins.iter().map(|c| c.norm_sqr()).sum::<f32>() / bins.len() as f32;
                let mag = power.sqrt();
                // Music loses energy toward the treble; tilt +2 dB/octave
                // (around 1 kHz) so the right side of the display isn't flat.
                let tilt = 2.0 * ((f0 * f1).sqrt() / 1000.0).log2();
                let db = 20.0 * (mag / (FFT_SIZE as f32 / 4.0)).max(1e-9).log10() + tilt + GAIN_DB;
                ((db + RANGE_DB) / RANGE_DB).clamp(0.0, 1.0)
            })
            .collect()
    }
}
