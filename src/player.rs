//! Playback pipeline:
//!
//!   download threads → decoder thread → bounded queue (~2 s) → PlaybackSource
//!
//! Decoding happens on its own thread, so the audio callback only ever
//! copies ready-made samples and never blocks; if the queue runs dry it plays
//! silence instead of stalling. Seeking and position tracking go through
//! atomics, so the UI never waits on the audio thread either.

use std::num::NonZero;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Result, anyhow};
use rodio::cpal::BufferSize;
use rodio::source::SeekError;
use rodio::{ChannelCount, Decoder, DeviceSinkBuilder, MixerDeviceSink, Player as Sink, Sample, SampleRate, Source};

use crate::api::{OpenStream, StreamSource};
use crate::hls::AacStream;
use crate::stream::StreamReader;
use crate::viz::{Ring, Tap};

/// Samples per message between decoder and audio thread (~46 ms stereo).
const PIECE: usize = 4096;
/// Queue capacity in pieces (~2 s of stereo 44.1 kHz).
const QUEUE: usize = 48;
/// Playback starts once this many pieces are decoded (~0.5 s).
const READY: usize = 12;

enum Msg {
    Data(u64, Vec<f32>),
    End(u64),
    Error(u64, String),
}

/// Something that turns a stream into interleaved f32 samples.
trait PcmDecoder: Send {
    fn next_chunk(&mut self) -> Result<Option<Vec<f32>>>;
    fn seek(&mut self, pos: Duration) -> Result<()>;
}

impl PcmDecoder for AacStream {
    fn next_chunk(&mut self) -> Result<Option<Vec<f32>>> {
        AacStream::next_chunk(self)
    }

    fn seek(&mut self, pos: Duration) -> Result<()> {
        AacStream::seek(self, pos);
        Ok(())
    }
}

impl PcmDecoder for Decoder<StreamReader> {
    fn next_chunk(&mut self) -> Result<Option<Vec<f32>>> {
        let chunk: Vec<f32> = self.by_ref().take(PIECE * 2).collect();
        Ok((!chunk.is_empty()).then_some(chunk))
    }

    fn seek(&mut self, pos: Duration) -> Result<()> {
        self.try_seek(pos).map_err(|e| anyhow!("{e}"))
    }
}

/// State shared between the UI thread and the audio thread.
#[derive(Default)]
struct Shared {
    /// Bumped by the UI for every seek; the audio thread notices the change.
    seek_gen: AtomicU64,
    seek_to_ms: AtomicU64,
    /// Playback position, written by the audio thread.
    pos_ms: AtomicU64,
    error: Mutex<Option<String>>,
}

/// A track whose decoder is running with some audio already buffered.
pub struct Prepared {
    rx: Receiver<Msg>,
    seek_tx: Sender<(Duration, u64)>,
    channels: u16,
    rate: u32,
    stream: OpenStream,
}

impl Prepared {
    pub fn label(&self) -> &'static str {
        self.stream.label
    }

    /// Throw it away, stopping its download.
    pub fn cancel(self) {
        self.stream.cancel();
    }
}

/// Start decoding `stream` and wait until ~0.5 s of audio is ready.
/// Blocks, so call it from a background thread.
pub fn prepare(stream: OpenStream) -> Result<Prepared> {
    let (decoder, channels, rate): (Box<dyn PcmDecoder>, u16, u32) = match &stream.source {
        StreamSource::Aac(segments) => {
            let aac = AacStream::new(segments.clone())?;
            let (c, r) = (aac.channels(), aac.sample_rate());
            (Box::new(aac), c, r)
        }
        StreamSource::Mp3(buffer) => {
            let mut builder = Decoder::builder()
                .with_data(buffer.reader())
                .with_hint("mp3")
                .with_mime_type("audio/mpeg")
                .with_seekable(true);
            if let (_, Some(total), _) = buffer.progress() {
                builder = builder.with_byte_len(total);
            }
            let d = builder.build()?;
            let (c, r) = (d.channels().get(), d.sample_rate().get());
            (Box::new(d), c, r)
        }
    };

    let (tx, rx) = mpsc::sync_channel(QUEUE);
    let (seek_tx, seek_rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::channel();
    thread::spawn(move || decode_loop(decoder, tx, seek_rx, ready_tx));
    match ready_rx.recv() {
        Ok(Ok(())) => Ok(Prepared { rx, seek_tx, channels, rate, stream }),
        Ok(Err(e)) => {
            stream.cancel();
            Err(e)
        }
        Err(_) => Err(anyhow!("decoder stopped unexpectedly")),
    }
}

fn latest_seek(rx: &Receiver<(Duration, u64)>) -> Option<(Duration, u64)> {
    let mut latest = None;
    while let Ok(s) = rx.try_recv() {
        latest = Some(s);
    }
    latest
}

fn decode_loop(
    mut dec: Box<dyn PcmDecoder>,
    tx: SyncSender<Msg>,
    seeks: Receiver<(Duration, u64)>,
    ready: Sender<Result<()>>,
) {
    let mut ready = Some(ready);
    fn signal(ready: &mut Option<Sender<Result<()>>>, r: Result<()>) {
        if let Some(tx) = ready.take() {
            let _ = tx.send(r);
        }
    }
    let mut generation = 0;
    let mut sent = 0;
    'decode: loop {
        if let Some((pos, g)) = latest_seek(&seeks) {
            generation = g;
            let _ = dec.seek(pos);
        }
        let chunk = match dec.next_chunk() {
            Ok(Some(c)) => c,
            Ok(None) => {
                signal(&mut ready, Ok(()));
                if tx.send(Msg::End(generation)).is_err() {
                    return;
                }
                // Stay alive so a seek back from the end still works.
                match seeks.recv() {
                    Ok((pos, g)) => {
                        generation = g;
                        let _ = dec.seek(pos);
                        continue;
                    }
                    Err(_) => return,
                }
            }
            Err(e) => {
                if ready.is_some() {
                    signal(&mut ready, Err(e));
                } else {
                    let _ = tx.send(Msg::Error(generation, format!("{e:#}")));
                }
                return;
            }
        };
        for piece in chunk.chunks(PIECE) {
            let mut msg = Msg::Data(generation, piece.to_vec());
            loop {
                match tx.try_send(msg) {
                    Ok(()) => break,
                    Err(TrySendError::Full(m)) => {
                        // Queue full: wait, but react to seeks meanwhile.
                        if let Some((pos, g)) = latest_seek(&seeks) {
                            generation = g;
                            let _ = dec.seek(pos);
                            continue 'decode;
                        }
                        msg = m;
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(TrySendError::Disconnected(_)) => return,
                }
            }
            sent += 1;
            if sent >= READY {
                signal(&mut ready, Ok(()));
            }
        }
    }
}

/// Runs on the audio thread: hands out decoded samples, never blocks.
struct PlaybackSource {
    rx: Receiver<Msg>,
    seek_tx: Sender<(Duration, u64)>,
    shared: Arc<Shared>,
    channels: ChannelCount,
    rate: SampleRate,
    cur: Vec<f32>,
    idx: usize,
    generation: u64,
    /// Zeros still owed to finish a silent frame after an underrun.
    silence: u16,
    base_ms: u64,
    frames: u64,
    in_frame: u16,
}

impl Iterator for PlaybackSource {
    type Item = Sample;

    fn next(&mut self) -> Option<Sample> {
        let g = self.shared.seek_gen.load(Ordering::Acquire);
        if g != self.generation {
            self.generation = g;
            let to = self.shared.seek_to_ms.load(Ordering::Acquire);
            let _ = self.seek_tx.send((Duration::from_millis(to), g));
            self.cur.clear();
            self.idx = 0;
            self.base_ms = to;
            self.frames = 0;
        }
        if self.silence > 0 {
            self.silence -= 1;
            return Some(0.0);
        }
        loop {
            if let Some(&s) = self.cur.get(self.idx) {
                self.idx += 1;
                self.in_frame += 1;
                if self.in_frame == self.channels.get() {
                    self.in_frame = 0;
                    self.frames += 1;
                    if self.frames.is_multiple_of(64) {
                        let ms = self.base_ms + self.frames * 1000 / self.rate.get() as u64;
                        self.shared.pos_ms.store(ms, Ordering::Relaxed);
                    }
                }
                return Some(s);
            }
            match self.rx.try_recv() {
                Ok(Msg::Data(g, v)) if g == self.generation => {
                    self.cur = v;
                    self.idx = 0;
                }
                Ok(Msg::End(g)) if g == self.generation => return None,
                Ok(Msg::Error(g, e)) if g == self.generation => {
                    *self.shared.error.lock().unwrap() = Some(e);
                    return None;
                }
                Ok(_) => {} // left over from before a seek
                Err(TryRecvError::Empty) => {
                    // Underrun (still buffering after a seek, or a slow
                    // network): play one silent frame rather than block.
                    self.silence = self.channels.get() - 1;
                    return Some(0.0);
                }
                Err(TryRecvError::Disconnected) => return None,
            }
        }
    }
}

impl Source for PlaybackSource {
    fn current_span_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> ChannelCount {
        self.channels
    }

    fn sample_rate(&self) -> SampleRate {
        self.rate
    }

    fn total_duration(&self) -> Option<Duration> {
        None
    }

    fn try_seek(&mut self, _: Duration) -> Result<(), SeekError> {
        // Seeks go through `Player::seek_to` instead.
        Err(SeekError::NotSupported { underlying_source: "PlaybackSource" })
    }
}

struct Current {
    shared: Arc<Shared>,
    stream: OpenStream,
}

pub struct Player {
    // Must stay alive for as long as audio should play.
    _device: MixerDeviceSink,
    sink: Sink,
    current: Option<Current>,
    /// Samples as they are played, for the visualiser.
    pub ring: Arc<Mutex<Ring>>,
}

impl Player {
    pub fn new() -> Result<Self> {
        // ~100ms of buffering (rodio defaults to ~50ms) so a busy moment
        // doesn't cause an audible dropout. Underrun errors are ignored:
        // rodio's default prints them over the TUI.
        let mut device = DeviceSinkBuilder::from_default_device()
            .map(|b| b.with_buffer_size(BufferSize::Fixed(4096)).with_error_callback(|_| {}))
            .and_then(|b| b.open_stream())
            .or_else(|_| DeviceSinkBuilder::open_default_sink())?;
        // Otherwise rodio prints to stderr on exit and scribbles over the TUI.
        device.log_on_drop(false);
        let sink = Sink::connect_new(device.mixer());
        sink.set_volume(0.7);
        Ok(Self { _device: device, sink, current: None, ring: Ring::shared() })
    }

    pub fn play(&mut self, p: Prepared) {
        self.stop();
        let shared = Arc::new(Shared::default());
        let source = PlaybackSource {
            rx: p.rx,
            seek_tx: p.seek_tx,
            shared: shared.clone(),
            channels: NonZero::new(p.channels.max(1)).unwrap(),
            rate: NonZero::new(p.rate.max(1)).unwrap(),
            cur: Vec::new(),
            idx: 0,
            generation: 0,
            silence: 0,
            base_ms: 0,
            frames: 0,
            in_frame: 0,
        };
        self.sink.append(Tap::new(source, self.ring.clone()));
        self.sink.play();
        self.current = Some(Current { shared, stream: p.stream });
    }

    pub fn toggle_pause(&self) {
        if self.sink.is_paused() {
            self.sink.play();
        } else {
            self.sink.pause();
        }
    }

    pub fn is_paused(&self) -> bool {
        self.sink.is_paused()
    }

    /// True once a loaded track has played to the end.
    pub fn finished(&self) -> bool {
        self.current.is_some() && self.sink.empty()
    }

    /// A mid-track failure (e.g. the network dropped), reported once.
    pub fn take_error(&self) -> Option<String> {
        self.current.as_ref()?.shared.error.lock().unwrap().take()
    }

    pub fn stop(&mut self) {
        self.sink.clear();
        self.ring.lock().unwrap().clear();
        if let Some(c) = self.current.take() {
            c.stream.cancel();
        }
    }

    pub fn position(&self) -> Duration {
        let ms = self.current.as_ref().map_or(0, |c| c.shared.pos_ms.load(Ordering::Relaxed));
        Duration::from_millis(ms)
    }

    pub fn seek_to(&self, pos: Duration) {
        let Some(c) = &self.current else { return };
        let ms = pos.as_millis() as u64;
        c.shared.seek_to_ms.store(ms, Ordering::Release);
        c.shared.pos_ms.store(ms, Ordering::Relaxed); // show it right away
        c.shared.seek_gen.fetch_add(1, Ordering::AcqRel);
        self.ring.lock().unwrap().clear();
    }

    pub fn seek_by(&self, delta_secs: i64) {
        let ms = self.position().as_millis() as i64 + delta_secs * 1000;
        self.seek_to(Duration::from_millis(ms.max(0) as u64));
    }

    pub fn volume(&self) -> f32 {
        self.sink.volume()
    }

    pub fn set_volume(&self, v: f32) {
        self.sink.set_volume(v.clamp(0.0, 1.5));
    }

    pub fn change_volume(&self, delta: f32) {
        let v = (self.volume() + delta).clamp(0.0, 1.5);
        self.sink.set_volume(v);
    }
}
