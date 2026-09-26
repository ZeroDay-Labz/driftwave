//! Streaming playback of SoundCloud's AAC-over-HLS (fragmented MP4).
//!
//! Symphonia's MP4 demuxer reads the whole file before producing audio, so
//! instead we download segments in parallel, pull the raw AAC frames out of
//! each fragment ourselves (moof/trun/mdat), and feed them to the AAC codec.
//! The codec keeps its state across segments, so joins are seamless, and
//! seeking is just "jump to the segment containing t".

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use regex::Regex;
use reqwest::blocking::Client;
use symphonia::core::audio::{Channels, SampleBuffer};
use symphonia::core::codecs::{CODEC_TYPE_AAC, CodecParameters, Decoder as _, DecoderOptions};
use symphonia::core::formats::Packet;
use symphonia::default::codecs::AacDecoder;

const WORKERS: usize = 6;

pub struct Playlist {
    pub init: String,
    /// (url, duration in seconds)
    pub segments: Vec<(String, f64)>,
}

pub fn parse_playlist(m3u8: &str) -> Result<Playlist> {
    if m3u8.contains("#EXT-X-KEY") && !m3u8.contains("METHOD=NONE") {
        bail!("stream is encrypted");
    }
    let map = Regex::new(r#"#EXT-X-MAP:.*URI="([^"]+)""#)?;
    let init = map.captures(m3u8).map(|c| c[1].to_string()).context("no init segment")?;
    let mut segments = Vec::new();
    let mut dur = 0.0;
    for line in m3u8.lines() {
        if let Some(d) = line.strip_prefix("#EXTINF:") {
            dur = d.trim_end_matches(',').split(',').next().unwrap_or("0").parse().unwrap_or(0.0);
        } else if !line.is_empty() && !line.starts_with('#') {
            segments.push((line.to_string(), dur));
        }
    }
    Ok(Playlist { init, segments })
}

/// Segment bytes, filled in by download workers in roughly playback order,
/// starting from wherever playback currently is (`focus`).
pub struct Segments {
    pub init: Vec<u8>,
    pub durations: Vec<f64>,
    slots: Mutex<SlotState>,
    ready: Condvar,
    focus: AtomicUsize,
}

struct SlotState {
    data: Vec<Option<Arc<[u8]>>>,
    in_flight: Vec<bool>,
    error: Option<String>,
    cancelled: bool,
}

impl Segments {
    /// Fetch the init segment, then start background workers for the rest.
    pub fn start(http: &Client, playlist: Playlist) -> Result<Arc<Self>> {
        let init = http.get(&playlist.init).send()?.error_for_status()?.bytes()?.to_vec();
        let n = playlist.segments.len();
        let (urls, durations): (Vec<_>, Vec<_>) = playlist.segments.into_iter().unzip();
        let seg = Arc::new(Segments {
            init,
            durations,
            slots: Mutex::new(SlotState {
                data: vec![None; n],
                in_flight: vec![false; n],
                error: None,
                cancelled: false,
            }),
            ready: Condvar::new(),
            focus: AtomicUsize::new(0),
        });
        let urls = Arc::new(urls);
        for _ in 0..WORKERS.min(n.max(1)) {
            let (seg, urls, http) = (seg.clone(), urls.clone(), http.clone());
            thread::spawn(move || seg.worker(&http, &urls));
        }
        Ok(seg)
    }

    fn worker(&self, http: &Client, urls: &[String]) {
        loop {
            let i = {
                let mut s = self.slots.lock().unwrap();
                if s.cancelled || s.error.is_some() {
                    return;
                }
                let focus = self.focus.load(Ordering::Relaxed).min(urls.len());
                let free = |i: &usize| s.data[*i].is_none() && !s.in_flight[*i];
                let Some(i) = (focus..urls.len()).find(free).or_else(|| (0..focus).find(free)) else {
                    return; // everything downloaded or being downloaded
                };
                s.in_flight[i] = true;
                i
            };
            let result = (|| -> Result<Vec<u8>> {
                let mut last = anyhow!("unreachable");
                for attempt in 0..3 {
                    match http.get(&urls[i]).send().and_then(|r| r.error_for_status()?.bytes()) {
                        Ok(b) => return Ok(b.to_vec()),
                        Err(e) => last = e.into(),
                    }
                    thread::sleep(Duration::from_millis(200 * (attempt + 1)));
                }
                Err(last)
            })();
            let mut s = self.slots.lock().unwrap();
            s.in_flight[i] = false;
            match result {
                Ok(bytes) => s.data[i] = Some(bytes.into()),
                Err(e) => s.error = Some(format!("segment {i}: {e:#}")),
            }
            self.ready.notify_all();
        }
    }

    /// Block until segment `i` is downloaded. `None` means cancelled.
    fn get(&self, i: usize) -> Result<Option<Arc<[u8]>>> {
        self.focus.store(i, Ordering::Relaxed);
        let mut s = self.slots.lock().unwrap();
        loop {
            if s.cancelled {
                return Ok(None);
            }
            if let Some(d) = &s.data[i] {
                return Ok(Some(d.clone()));
            }
            if let Some(e) = &s.error {
                bail!("{e}");
            }
            s = self.ready.wait(s).unwrap();
        }
    }

    /// Already-downloaded segments (tests and tools).
    #[cfg(test)]
    pub fn from_parts(init: Vec<u8>, segments: Vec<Vec<u8>>, durations: Vec<f64>) -> Arc<Self> {
        let n = segments.len();
        Arc::new(Segments {
            init,
            durations,
            slots: Mutex::new(SlotState {
                data: segments.into_iter().map(|s| Some(s.into())).collect(),
                in_flight: vec![false; n],
                error: None,
                cancelled: false,
            }),
            ready: Condvar::new(),
            focus: AtomicUsize::new(0),
        })
    }

    pub fn cancel(&self) {
        self.slots.lock().unwrap().cancelled = true;
        self.ready.notify_all();
    }

}

// ---- minimal fragmented-MP4 parsing ----

/// Iterate (type, body, absolute offset of the box start) over boxes in `data`.
fn boxes(data: &[u8]) -> impl Iterator<Item = ([u8; 4], &[u8], usize)> {
    let mut off = 0;
    std::iter::from_fn(move || {
        if off + 8 > data.len() {
            return None;
        }
        let mut size = u32::from_be_bytes(data[off..off + 4].try_into().ok()?) as usize;
        let typ: [u8; 4] = data[off + 4..off + 8].try_into().ok()?;
        let mut hdr = 8;
        if size == 1 {
            size = u64::from_be_bytes(data.get(off + 8..off + 16)?.try_into().ok()?) as usize;
            hdr = 16;
        } else if size == 0 {
            size = data.len() - off;
        }
        if size < hdr || off + size > data.len() {
            return None;
        }
        let item = (typ, &data[off + hdr..off + size], off);
        off += size;
        Some(item)
    })
}

fn find<'a>(data: &'a [u8], path: &[&[u8; 4]]) -> Option<&'a [u8]> {
    let (first, rest) = path.split_first()?;
    let (_, body, _) = boxes(data).find(|(t, _, _)| t == *first)?;
    if rest.is_empty() { Some(body) } else { find(body, rest) }
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

pub struct InitInfo {
    /// AudioSpecificConfig from the esds box.
    pub asc: Vec<u8>,
    pub sample_rate: u32,
    pub channels: u16,
    /// Encoder delay to trim from the start (edit list), in samples.
    pub priming: u64,
}

pub fn parse_init(init: &[u8]) -> Result<InitInfo> {
    let stsd = find(init, &[b"moov", b"trak", b"mdia", b"minf", b"stbl", b"stsd"]).context("no stsd")?;
    // stsd: version/flags(4) entry_count(4), then the mp4a sample entry box.
    let (typ, mp4a, _) = boxes(stsd.get(8..).context("short stsd")?).next().context("empty stsd")?;
    if &typ != b"mp4a" {
        bail!("not AAC ({})", String::from_utf8_lossy(&typ));
    }
    // AudioSampleEntry: 6 reserved + 2 dref idx + 8 reserved, channelcount(2),
    // samplesize(2), 4 reserved, samplerate(16.16), then child boxes.
    let channels = u16::from_be_bytes(mp4a.get(16..18).context("short mp4a")?.try_into()?);
    let sample_rate = be32(mp4a, 24).context("short mp4a")? >> 16;
    let esds = find(mp4a.get(28..).context("short mp4a")?, &[b"esds"]).context("no esds")?;
    let asc = parse_esds(esds.get(4..).context("short esds")?).context("no AudioSpecificConfig")?;

    // Edit list media_time = samples of encoder delay to skip.
    let priming = find(init, &[b"moov", b"trak", b"edts", b"elst"])
        .and_then(|elst| {
            let version = *elst.first()?;
            if version == 1 {
                Some(u64::from_be_bytes(elst.get(16..24)?.try_into().ok()?))
            } else {
                be32(elst, 12).map(u64::from)
            }
        })
        .filter(|&m| m != u32::MAX as u64 && m < 10_000)
        .unwrap_or(0);
    Ok(InitInfo { asc, sample_rate, channels, priming })
}

/// Walk MPEG-4 descriptors: ES(0x03) > DecoderConfig(0x04) > DecSpecificInfo(0x05).
fn parse_esds(mut d: &[u8]) -> Option<Vec<u8>> {
    fn header(d: &[u8]) -> Option<(u8, usize, &[u8])> {
        let tag = *d.first()?;
        let mut len = 0usize;
        let mut i = 1;
        loop {
            let b = *d.get(i)?;
            len = (len << 7) | (b & 0x7f) as usize;
            i += 1;
            if b & 0x80 == 0 || i > 4 {
                break;
            }
        }
        Some((tag, len, d.get(i..)?))
    }
    let (tag, _, body) = header(d)?;
    if tag != 0x03 {
        return None;
    }
    let flags = *body.get(2)?;
    let mut skip = 3;
    if flags & 0x80 != 0 {
        skip += 2;
    }
    if flags & 0x40 != 0 {
        skip += 1 + *body.get(skip)? as usize;
    }
    if flags & 0x20 != 0 {
        skip += 2;
    }
    d = body.get(skip..)?;
    let (tag, _, body) = header(d)?;
    if tag != 0x04 {
        return None;
    }
    let (tag, len, body) = header(body.get(13..)?)?;
    (tag == 0x05).then(|| body.get(..len).map(<[u8]>::to_vec)).flatten()
}

/// Byte ranges of each AAC frame in a media segment.
pub fn segment_frames(seg: &[u8]) -> Result<Vec<std::ops::Range<usize>>> {
    let mut frames = Vec::new();
    for (typ, moof, moof_start) in boxes(seg) {
        if &typ != b"moof" {
            continue;
        }
        let traf = find(moof, &[b"traf"]).context("no traf")?;
        let tfhd = find(traf, &[b"tfhd"]).context("no tfhd")?;
        let tfhd_flags = be32(tfhd, 0).context("short tfhd")? & 0xff_ffff;
        // Optional tfhd fields: base offset(8), desc idx(4), dur(4), size(4), flags(4).
        let mut at = 8;
        let mut base = moof_start as u64;
        if tfhd_flags & 0x1 != 0 {
            base = u64::from_be_bytes(tfhd.get(at..at + 8).context("short tfhd")?.try_into()?);
            at += 8;
        }
        if tfhd_flags & 0x2 != 0 {
            at += 4;
        }
        if tfhd_flags & 0x8 != 0 {
            at += 4;
        }
        let default_size = if tfhd_flags & 0x10 != 0 { be32(tfhd, at) } else { None };

        let trun = find(traf, &[b"trun"]).context("no trun")?;
        let flags = be32(trun, 0).context("short trun")? & 0xff_ffff;
        let count = be32(trun, 4).context("short trun")? as usize;
        let mut at = 8;
        let mut offset = base;
        if flags & 0x1 != 0 {
            offset = (base as i64 + be32(trun, at).context("short trun")? as i32 as i64) as u64;
            at += 4;
        }
        if flags & 0x4 != 0 {
            at += 4;
        }
        let per_sample = [0x100, 0x200, 0x400, 0x800].iter().filter(|f| flags & **f != 0).count() * 4;
        for i in 0..count {
            let entry = at + i * per_sample;
            let size = if flags & 0x200 != 0 {
                let size_at = entry + if flags & 0x100 != 0 { 4 } else { 0 };
                be32(trun, size_at).context("short trun")?
            } else {
                default_size.context("no sample size")?
            } as usize;
            let start = offset as usize;
            if start + size > seg.len() {
                bail!("frame outside segment");
            }
            frames.push(start..start + size);
            offset += size as u64;
        }
    }
    Ok(frames)
}

/// Decodes the stream segment by segment into interleaved f32 samples.
pub struct AacStream {
    seg: Arc<Segments>,
    info: InitInfo,
    codec: AacDecoder,
    next_segment: usize,
    /// Samples (per channel) to drop before output: priming, or the part of
    /// a segment before a seek target.
    skip: u64,
}

impl AacStream {
    pub fn new(seg: Arc<Segments>) -> Result<Self> {
        let info = parse_init(&seg.init)?;
        let mut params = CodecParameters::new();
        params
            .for_codec(CODEC_TYPE_AAC)
            .with_sample_rate(info.sample_rate)
            .with_channels(if info.channels == 1 {
                Channels::FRONT_LEFT
            } else {
                Channels::FRONT_LEFT | Channels::FRONT_RIGHT
            })
            .with_extra_data(info.asc.clone().into_boxed_slice());
        let codec = AacDecoder::try_new(&params, &DecoderOptions::default())?;
        let skip = info.priming;
        Ok(Self { seg, info, codec, next_segment: 0, skip })
    }

    pub fn sample_rate(&self) -> u32 {
        self.info.sample_rate
    }

    pub fn channels(&self) -> u16 {
        self.info.channels.max(1)
    }

    /// Decode the next segment. `Ok(None)` at the end of the track.
    pub fn next_chunk(&mut self) -> Result<Option<Vec<f32>>> {
        let n = self.seg.durations.len();
        if self.next_segment >= n {
            return Ok(None);
        }
        let Some(data) = self.seg.get(self.next_segment)? else { return Ok(None) };
        self.next_segment += 1;

        let mut out = Vec::new();
        let mut sample_buf: Option<SampleBuffer<f32>> = None;
        let ch = self.channels() as usize;
        for range in segment_frames(&data)? {
            let packet = Packet::new_from_slice(0, 0, 1024, &data[range]);
            let decoded = match self.codec.decode(&packet) {
                Ok(d) => d,
                Err(_) => continue, // skip a corrupt frame rather than stopping
            };
            let sb = sample_buf.get_or_insert_with(|| {
                SampleBuffer::new(decoded.capacity() as u64, *decoded.spec())
            });
            sb.copy_interleaved_ref(decoded);
            let mut samples = sb.samples();
            let frames = (samples.len() / ch) as u64;
            if self.skip > 0 {
                let drop = self.skip.min(frames);
                self.skip -= drop;
                samples = &samples[drop as usize * ch..];
            }
            out.extend_from_slice(samples);
        }
        Ok(Some(out))
    }

    pub fn seek(&mut self, pos: Duration) {
        let mut t = pos.as_secs_f64();
        let mut i = 0;
        while i < self.seg.durations.len() && t >= self.seg.durations[i] {
            t -= self.seg.durations[i];
            i += 1;
        }
        self.next_segment = i;
        self.skip = (t * self.info.sample_rate as f64) as u64;
        self.codec.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playlist_parsing() {
        let p = parse_playlist(
            "#EXTM3U\n#EXT-X-MAP:URI=\"https://x/init.mp4\"\n#EXTINF:10.0078,\nhttps://x/0.m4s\n#EXTINF:4.5,\nhttps://x/1.m4s\n#EXT-X-ENDLIST\n",
        )
        .unwrap();
        assert_eq!(p.init, "https://x/init.mp4");
        assert_eq!(p.segments, vec![("https://x/0.m4s".into(), 10.0078), ("https://x/1.m4s".into(), 4.5)]);
        assert!(parse_playlist("#EXT-X-KEY:METHOD=AES-128,URI=\"k\"\n").is_err());
    }

    /// Split a concatenated fMP4 (init + segments) back into its parts.
    fn split(file: &[u8]) -> (Vec<u8>, Vec<Vec<u8>>) {
        let mut init = Vec::new();
        let mut segs: Vec<Vec<u8>> = Vec::new();
        for (typ, _, start) in boxes(file) {
            let size = u32::from_be_bytes(file[start..start + 4].try_into().unwrap()) as usize;
            let bytes = &file[start..start + size];
            match &typ {
                b"ftyp" | b"moov" => init.extend_from_slice(bytes),
                b"styp" => segs.push(bytes.to_vec()),
                _ => segs.last_mut().unwrap().extend_from_slice(bytes),
            }
        }
        (init, segs)
    }

    /// Compare against symphonia's own full-file decode:
    /// `HLS_SAMPLE=path/to/concatenated.mp4 cargo test hls -- --ignored`
    #[test]
    #[ignore]
    fn matches_reference_decoder() {
        let Ok(path) = std::env::var("HLS_SAMPLE") else { return };
        let file = std::fs::read(path).unwrap();
        let reference: Vec<f32> =
            rodio::Decoder::new_mp4(std::io::Cursor::new(file.clone())).unwrap().collect();
        let (init, segs) = split(&file);
        let n = segs.len();
        let seg = Segments::from_parts(init, segs, vec![10.0078; n]);
        let mut s = AacStream::new(seg.clone()).unwrap();
        let mut ours = Vec::new();
        while let Some(chunk) = s.next_chunk().unwrap() {
            ours.extend(chunk);
        }
        println!("segments {n}, reference {} samples, ours {}", reference.len(), ours.len());
        // Align (the reference may or may not trim priming) and compare.
        let best = (0..4096usize)
            .step_by(2)
            .map(|shift| {
                let err: f64 = ours.iter().skip(shift).zip(&reference).take(400_000)
                    .map(|(a, b)| ((a - b) as f64).powi(2)).sum();
                (err, shift)
            })
            .min_by(|a, b| a.0.total_cmp(&b.0))
            .unwrap();
        let signal: f64 = reference.iter().take(400_000).map(|x| (*x as f64).powi(2)).sum();
        let snr = 10.0 * (signal / best.0.max(1e-20)).log10();
        println!("best alignment shift {} samples, SNR {snr:.1} dB", best.1);
        assert!(snr > 60.0, "decoded audio differs from reference");
        assert!((ours.len() as i64 - reference.len() as i64).abs() < 4096);

        // Seeking lands on the right sample.
        s.seek(Duration::from_secs_f64(25.0));
        let after_seek = s.next_chunk().unwrap().unwrap();
        let at = (25.0 * 44_100.0) as usize * 2;
        let err: f64 = after_seek.iter().zip(&ours[at..]).take(20_000).map(|(a, b)| ((a - b) as f64).powi(2)).sum();
        let sig: f64 = ours[at..at + 20_000].iter().map(|x| (*x as f64).powi(2)).sum();
        println!("after seek to 25s: SNR {:.1} dB", 10.0 * (sig / err.max(1e-20)).log10());
    }
}
