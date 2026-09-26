//! Lyrics from LRCLIB (https://lrclib.net), a free, open lyrics database
//! with time-synced (LRC) lyrics and no API key.

use std::sync::OnceLock;

use anyhow::Result;
use regex::Regex;
use reqwest::StatusCode;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};

use crate::api::Track;
use crate::library;
use std::path::PathBuf;

const LRCLIB: &str = "https://lrclib.net/api";

/// A synced match must be within this many seconds of the track's length,
/// otherwise it's a different edit/mix and the timestamps would drift.
const SYNC_TOLERANCE_SECS: f64 = 3.0;
/// Uploads are often a few seconds longer or shorter than the release the
/// lyrics were timed to (silence, a DJ tag). Up to this much, the timing is
/// still far better than guessing, and the user can nudge it.
const LOOSE_SYNC_SECS: f64 = 20.0;

/// The user's timing corrections for one track.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LyricSync {
    /// Added to every line's time ([ and ] nudge it; right-click sets it).
    pub offset_ms: i64,
    /// For unsynced lyrics: (line index, time) pairs the user pinned by
    /// right-clicking a line as it was sung.
    pub anchors: Vec<(usize, u64)>,
}

#[derive(Clone, Serialize, Deserialize)]
pub enum Lyrics {
    /// (start time in ms, line), sorted by time.
    Synced(Vec<(u64, String)>),
    Plain(String),
    Instrumental,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Found {
    pub lyrics: Lyrics,
    /// What LRCLIB matched, e.g. "Deadmau5 – Ghosts 'n' Stuff".
    pub matched: String,
    /// Set when we fell back to unsynced lyrics because the versions differ.
    pub note: Option<String>,
    /// Where they came from: "LRCLIB", "Genius" or "library".
    #[serde(default)]
    pub source: String,
}

/// Lyrics lookup status for one track.
#[derive(Clone, Serialize, Deserialize)]
pub enum LyricsState {
    Loading,
    Ready(Option<Found>),
    Failed(String),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    track_name: String,
    artist_name: String,
    #[serde(default)]
    duration: f64,
    #[serde(default)]
    instrumental: bool,
    plain_lyrics: Option<String>,
    synced_lyrics: Option<String>,
}

fn http() -> &'static Client {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        Client::builder()
            // LRCLIB asks clients to identify themselves.
            .user_agent(concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("HTTP client")
    })
}

/// Best-effort guess at the real artist and song title. SoundCloud titles
/// are often "Artist - Song (feat. X) [Free Download]".
pub fn artist_and_title(track: &Track) -> (String, String) {
    let bracketed = Regex::new(r"\s*[\(\[\{][^\)\]\}]*[\)\]\}]").unwrap();
    let feat = Regex::new(r"(?i)\s+(feat\.?|ft\.?|f\.|featuring|prod\.?)\s.*$").unwrap();

    let (from_title, song) = match track.title.split_once(" - ").or_else(|| track.title.split_once(" – ")) {
        Some((a, s)) => (Some(a.trim()), s),
        None => (None, track.title.as_str()),
    };
    let artist = track
        .publisher_artist()
        .or(from_title)
        .unwrap_or(&track.user.username);
    // "Billie Eilish, Khalid" / "deadmau5 & Lights": the first name searches best.
    let artist = artist.split([',', '&']).next().unwrap_or(artist);
    let artist = feat.replace(artist, "").trim().to_string();

    let clean = bracketed.replace_all(song, "");
    let clean = feat.replace(&clean, "").trim().to_string();
    let title = if clean.is_empty() { song.trim().to_string() } else { clean };
    (artist, title)
}

/// Opens a Genius search for the track in the default browser.
pub fn open_on_genius(track: &Track) {
    let (artist, title) = artist_and_title(track);
    let q: String = format!("{artist} {title}")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_string() } else { format!("%{:02X}", c as u32 & 0xff) })
        .collect();
    let url = format!("https://genius.com/search?q={q}");
    let _ = std::process::Command::new("xdg-open")
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Where to look for lyrics besides LRCLIB.
pub struct Lookup {
    /// Fall back to Genius when LRCLIB has nothing.
    pub genius: bool,
    /// Library folder: checked first, and (with `save`) lyrics found online
    /// are written there.
    pub library: Option<PathBuf>,
    pub save: bool,
}

/// Find lyrics: the library folder, then LRCLIB (synced when possible),
/// then Genius.
pub fn find(track: &Track, lookup: &Lookup) -> Result<Option<Found>> {
    if let Some(found) = lookup.library.as_deref().and_then(|lib| library::load_lyrics(lib, track)) {
        return Ok(Some(found));
    }
    let result = find_online(track, lookup);
    if let (Ok(Some(found)), Some(lib), true) = (&result, &lookup.library, lookup.save) {
        let _ = library::save_lyrics(lib, track, found);
    }
    result
}

fn find_online(track: &Track, lookup: &Lookup) -> Result<Option<Found>> {
    let (artist, title) = artist_and_title(track);
    let lrclib = find_lrclib(track, &artist, &title);
    if let Ok(Some(found)) = lrclib {
        return Ok(Some(found));
    }
    if lookup.genius
        && let Ok(Some(g)) = crate::genius::find(&artist, &title)
    {
        // Genius knows the song's real name; LRCLIB may have it synced
        // under that even though the SoundCloud title didn't match.
        if (g.artist != artist || g.title != title)
            && let Ok(Some(found)) = find_lrclib(track, &g.artist, &g.title)
            && matches!(found.lyrics, Lyrics::Synced(_))
        {
            return Ok(Some(found));
        }
        let matched = format!("{} – {}", g.artist, g.title);
        let lyrics = g.text.map_or(Lyrics::Instrumental, Lyrics::Plain);
        return Ok(Some(Found { lyrics, matched, note: None, source: "Genius".into() }));
    }
    lrclib
}

fn find_lrclib(track: &Track, artist: &str, title: &str) -> Result<Option<Found>> {
    let (artist, title) = (artist.to_string(), title.to_string());
    let secs = track.full_duration.unwrap_or(track.duration) as f64 / 1000.0;

    // 1. Exact lookup (LRCLIB matches duration within a couple of seconds).
    let exact = http()
        .get(format!("{LRCLIB}/get"))
        .query(&[("artist_name", artist.as_str()), ("track_name", title.as_str())])
        .query(&[("duration", secs.round() as u64)])
        .send()?;
    let mut candidates: Vec<Record> = match exact.status() {
        StatusCode::OK => vec![exact.json()?],
        _ => Vec::new(),
    };

    // 2. Fuzzy searches, most specific first.
    if candidates.is_empty() {
        candidates = search(&[("track_name", &title), ("artist_name", &artist)])?;
    }
    if candidates.is_empty() {
        candidates = search(&[("q", &format!("{artist} {title}"))])?;
    }

    let want = normalize(&title);
    candidates.retain(|r| {
        let got = normalize(&r.track_name);
        !got.is_empty() && (got.contains(&want) || want.contains(&got))
    });
    let diff = |r: &Record| (r.duration - secs).abs();
    candidates.sort_by(|a, b| diff(a).total_cmp(&diff(b)));

    let synced = candidates
        .iter()
        .find(|r| r.synced_lyrics.is_some() && diff(r) <= LOOSE_SYNC_SECS);
    if let Some(r) = synced {
        let lines = parse_lrc(r.synced_lyrics.as_deref().unwrap());
        if !lines.is_empty() {
            let note = (diff(r) > SYNC_TOLERANCE_SECS).then(|| {
                let longer = if secs > r.duration { "longer" } else { "shorter" };
                format!(
                    "This upload is {:.0} s {longer} than the timed version: press [ or ], or right-click a line as it's sung",
                    diff(r)
                )
            });
            return Ok(Some(found(r, Lyrics::Synced(lines), note.as_deref())));
        }
    }
    let Some(best) = candidates.first() else { return Ok(None) };
    if best.instrumental {
        return Ok(Some(found(best, Lyrics::Instrumental, None)));
    }
    let text = best.plain_lyrics.clone().or_else(|| {
        best.synced_lyrics
            .as_deref()
            .map(|s| parse_lrc(s).into_iter().map(|(_, l)| l).collect::<Vec<_>>().join("\n"))
    });
    let note = best
        .synced_lyrics
        .is_some()
        .then_some("This upload's length differs from the original, so lyrics aren't synced.");
    Ok(text.map(|t| found(best, Lyrics::Plain(t), note)))
}

fn found(r: &Record, lyrics: Lyrics, note: Option<&str>) -> Found {
    Found {
        lyrics,
        matched: format!("{} – {}", r.artist_name, r.track_name),
        note: note.map(String::from),
        source: "LRCLIB".into(),
    }
}

fn search(params: &[(&str, &str)]) -> Result<Vec<Record>> {
    Ok(http().get(format!("{LRCLIB}/search")).query(params).send()?.error_for_status()?.json()?)
}

pub fn normalize(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// Give unsynced lyrics rough timestamps so they can follow the song:
/// singing is assumed to run from ~8% to ~92% of the track, with time
/// shared out by how much text each line has. Section headers ("[Chorus]")
/// and blank lines take no time of their own.
pub fn estimate_timings(text: &str, duration_ms: u64) -> Vec<(u64, String)> {
    let lines: Vec<&str> = text.lines().collect();
    let weight = |l: &str| {
        let t = l.trim();
        if t.is_empty() || is_section_header(t) { 0 } else { t.chars().filter(|c| !c.is_whitespace()).count().max(4) }
    };
    let total: usize = lines.iter().map(|l| weight(l)).sum();
    let (start, span) = (duration_ms as f64 * 0.08, duration_ms as f64 * 0.84);
    let mut before = 0;
    lines
        .iter()
        .map(|l| {
            let at = if total == 0 { 0.0 } else { start + span * before as f64 / total as f64 };
            before += weight(l);
            (at as u64, l.trim().to_string())
        })
        .collect()
}

/// Timed lines to display: synced lyrics shifted by the user's offset, or
/// an estimate for plain text that honours the user's pinned lines.
/// The flag says whether the timing is real (synced) or estimated.
pub fn timed_lines(lyrics: &Lyrics, duration_ms: u64, sync: &LyricSync) -> (Vec<(u64, String)>, bool) {
    let shift = |t: u64| (t as i64 + sync.offset_ms).max(0) as u64;
    match lyrics {
        Lyrics::Synced(lines) => (lines.iter().map(|(t, l)| (shift(*t), l.clone())).collect(), true),
        Lyrics::Plain(text) => {
            let lines = estimate_with_anchors(text, duration_ms, &sync.anchors);
            (lines.into_iter().map(|(t, l)| (shift(t), l)).collect(), false)
        }
        Lyrics::Instrumental => (Vec::new(), false),
    }
}

/// Like `estimate_timings`, but lines the user pinned get exactly their
/// time, and the lines in between are spread out between the pins.
pub fn estimate_with_anchors(text: &str, duration_ms: u64, anchors: &[(usize, u64)]) -> Vec<(u64, String)> {
    let mut lines = estimate_timings(text, duration_ms);
    let mut pins: Vec<(usize, u64)> = anchors.iter().copied().filter(|(i, _)| *i < lines.len()).collect();
    pins.sort();
    if pins.is_empty() {
        return lines;
    }
    let est: Vec<f64> = lines.iter().map(|(t, _)| *t as f64).collect();
    let map = |i: usize| -> f64 {
        let e = est[i];
        let before = pins.iter().rev().find(|(p, _)| *p <= i);
        let after = pins.iter().find(|(p, _)| *p >= i);
        match (before, after) {
            (Some(&(p, t)), _) if p == i => t as f64,
            (Some(&(pa, ta)), Some(&(pb, tb))) => {
                // Between two pins: keep the estimate's proportions.
                let (ea, eb) = (est[pa], est[pb]);
                let frac = if eb > ea { (e - ea) / (eb - ea) } else { 0.5 };
                ta as f64 + frac * (tb as f64 - ta as f64)
            }
            // Outside the pins: shift the estimate so it meets the nearest pin.
            (Some(&(p, t)), None) | (None, Some(&(p, t))) => e + (t as f64 - est[p]),
            (None, None) => e,
        }
    };
    let mapped: Vec<f64> = (0..lines.len()).map(map).collect();
    for ((t, _), m) in lines.iter_mut().zip(mapped) {
        *t = m.max(0.0) as u64;
    }
    lines
}

pub fn is_section_header(line: &str) -> bool {
    line.starts_with('[') && line.ends_with(']')
}

/// Parse "[mm:ss.xx] line" LRC text; a line may carry several timestamps.
pub fn parse_lrc(lrc: &str) -> Vec<(u64, String)> {
    let tag = Regex::new(r"\[(\d+):(\d+(?:\.\d+)?)\]").unwrap();
    let mut out = Vec::new();
    for line in lrc.lines() {
        let text = tag.replace_all(line, "").trim().to_string();
        for c in tag.captures_iter(line) {
            let (Ok(m), Ok(s)) = (c[1].parse::<u64>(), c[2].parse::<f64>()) else { continue };
            out.push((m * 60_000 + (s * 1000.0) as u64, text.clone()));
        }
    }
    out.sort_by_key(|(t, _)| *t);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lrc_parsing() {
        let l = parse_lrc("[00:01.50]a\n[01:02.00][00:03.25] b \nno tag\n[00:04.00]");
        assert_eq!(
            l,
            vec![(1500, "a".into()), (3250, "b".into()), (4000, "".into()), (62000, "b".into())]
        );
    }

    #[test]
    fn estimated_timings() {
        let text = "[Verse 1]\nshort\na much much longer line here\n\n[Chorus]\nend";
        let t = estimate_timings(text, 100_000);
        let times: Vec<u64> = t.iter().map(|(ms, _)| *ms).collect();
        assert!(times.windows(2).all(|w| w[0] <= w[1]), "monotonic: {times:?}");
        assert_eq!(times[0], 8_000, "singing starts ~8% in");
        assert_eq!(times[0], times[1], "a header takes no time");
        assert!(times[2] - times[1] < times[5] - times[2], "longer lines get more time");
        assert_eq!(times[3], times[4], "blank line and header share the next line's time");
        assert!(times[5] < 92_000);
        assert!(estimate_timings("", 1000).is_empty());
    }

    #[test]
    fn pinned_lines() {
        let text = "a line\nb line\nc line\nd line\ne line";
        let plain = estimate_with_anchors(text, 100_000, &[]);
        // Vocals start right away: pin the first line at 1 s and the fourth at 30 s.
        let pinned = estimate_with_anchors(text, 100_000, &[(0, 1_000), (3, 30_000)]);
        let t: Vec<u64> = pinned.iter().map(|(t, _)| *t).collect();
        assert_eq!((t[0], t[3]), (1_000, 30_000));
        assert!(t[0] < t[1] && t[1] < t[2] && t[2] < t[3] && t[3] < t[4], "{t:?}");
        assert_eq!(t[4] - t[3], plain[4].0 - plain[3].0, "after the last pin, the estimated pace continues");

        let synced = Lyrics::Synced(vec![(10_000, "x".into())]);
        let sync = LyricSync { offset_ms: -12_000, anchors: vec![] };
        assert_eq!(timed_lines(&synced, 0, &sync).0[0].0, 0, "offset never goes below zero");
    }

    #[test]
    fn f_dot_features_are_stripped() {
        let t: Track = serde_json::from_value(serde_json::json!({
            "id": 1, "title": "Castles f. Aesop Rock & Sadistik", "duration": 1,
            "user": {"id": 1, "username": "CunninLynguists"}
        }))
        .unwrap();
        assert_eq!(artist_and_title(&t), ("CunninLynguists".into(), "Castles".into()));
    }

    /// Hits lrclib.net: `cargo test -- --ignored`
    #[test]
    #[ignore]
    fn finds_synced_lyrics() {
        let sc = crate::api::SoundCloud::new().unwrap();
        let tracks = sc.search_tracks("deadmau5 ghosts n stuff").unwrap();
        // The 5:29 upload has a synced match on LRCLIB (the 6:10 "original
        // mix" only gets unsynced lyrics, by design).
        let t = tracks.iter().find(|t| (327_000..331_000).contains(&t.duration)).unwrap();
        println!("query: {:?}", artist_and_title(t));
        let lookup = Lookup { genius: false, library: None, save: false };
        let f = find(t, &lookup).unwrap().expect("lyrics");
        println!("matched {}", f.matched);
        assert!(matches!(f.lyrics, Lyrics::Synced(_)));
    }
}
