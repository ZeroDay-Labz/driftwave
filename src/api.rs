//! Minimal client for SoundCloud's undocumented `api-v2`, authenticated with the
//! public `client_id` that the soundcloud.com web player itself uses.

use std::collections::HashMap;
use std::path::PathBuf;
use std::io::Read;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use regex::Regex;
use reqwest::StatusCode;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::hls::{self, Segments};
use crate::library;
use crate::links::{self, Link};
use crate::stream::StreamBuffer;

const API: &str = "https://api-v2.soundcloud.com";
const USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64; rv:130.0) Gecko/20100101 Firefox/130.0";

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Track {
    pub id: u64,
    pub title: String,
    /// Milliseconds.
    pub duration: u64,
    pub user: User,
    #[serde(default)]
    pub policy: Option<String>,
    #[serde(default)]
    pub track_authorization: Option<String>,
    #[serde(default)]
    pub media: Media,
    #[serde(default)]
    pub genre: Option<String>,
    /// Space-separated tags, multi-word ones quoted.
    #[serde(default)]
    pub tag_list: Option<String>,
    #[serde(default)]
    pub playback_count: Option<u64>,
    /// Real length when `duration` is only a 30s preview.
    #[serde(default)]
    pub full_duration: Option<u64>,
    #[serde(default)]
    pub artwork_url: Option<String>,
    /// The uploader enabled SoundCloud's own download button.
    #[serde(default)]
    pub downloadable: Option<bool>,
    #[serde(default)]
    pub has_downloads_left: Option<bool>,
    #[serde(default)]
    publisher_metadata: Option<PublisherMetadata>,
    /// The track's soundcloud.com page, for sharing.
    #[serde(default)]
    pub permalink_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct PublisherMetadata {
    #[serde(default)]
    artist: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct User {
    pub id: u64,
    pub username: String,
    #[serde(default)]
    pub full_name: Option<String>,
    #[serde(default)]
    pub followers_count: Option<u64>,
    #[serde(default)]
    pub track_count: Option<u64>,
    #[serde(default)]
    pub verified: Option<bool>,
    #[serde(default)]
    pub avatar_url: Option<String>,
    #[serde(default)]
    pub permalink_url: Option<String>,
}

/// An album, EP, single or user playlist ("set").
#[derive(Debug, Clone, Deserialize)]
pub struct Playlist {
    pub id: u64,
    pub title: String,
    pub user: User,
    #[serde(default)]
    pub track_count: Option<u64>,
    /// Milliseconds.
    #[serde(default)]
    pub duration: Option<u64>,
    #[serde(default)]
    pub set_type: Option<String>,
    #[serde(default)]
    pub is_album: Option<bool>,
    #[serde(default)]
    pub release_date: Option<String>,
    #[serde(default)]
    pub display_date: Option<String>,
    #[serde(default)]
    pub permalink_url: Option<String>,
    /// Only the first few entries are full tracks; the rest are `{id}` stubs.
    #[serde(default)]
    tracks: Vec<Value>,
}

impl Playlist {
    /// "album", "ep", "single", "compilation" or "playlist".
    pub fn kind(&self) -> &str {
        match self.set_type.as_deref() {
            Some(t) if !t.is_empty() => t,
            _ if self.is_album == Some(true) => "album",
            _ => "playlist",
        }
    }

    pub fn year(&self) -> Option<&str> {
        let date = self.release_date.as_deref().or(self.display_date.as_deref())?;
        date.get(..4)
    }
}

#[derive(Debug, Clone)]
pub enum Item {
    Track(Track),
    User(User),
    Playlist(Playlist),
    /// A genre/tag, with how many of the current tracks carry it.
    Tag { name: String, count: usize },
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Media {
    #[serde(default)]
    pub transcodings: Vec<Transcoding>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Transcoding {
    pub url: String,
    #[serde(default)]
    pub preset: Option<String>,
    pub format: Format,
    #[serde(default)]
    pub snipped: bool,
}

impl Transcoding {
    /// Unencrypted, so we can decode it.
    fn is_plain(&self) -> bool {
        matches!(self.format.protocol.as_str(), "hls" | "progressive")
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Format {
    pub protocol: String,
    pub mime_type: String,
}

impl Track {
    /// "BLOCK" tracks are region-locked; "SNIP" tracks are 30s Go+ previews.
    pub fn is_playable(&self) -> bool {
        self.policy.as_deref() != Some("BLOCK")
    }

    /// SoundCloud only streams this track DRM-encrypted (typical for label
    /// releases), so we can't play it.
    pub fn is_drm_only(&self) -> bool {
        let t = &self.media.transcodings;
        t.iter().any(|x| x.format.protocol.contains("encrypted")) && !t.iter().any(Transcoding::is_plain)
    }

    /// The uploader offers an official download.
    pub fn can_download(&self) -> bool {
        self.downloadable == Some(true) && self.has_downloads_left != Some(false)
    }

    pub fn is_preview(&self) -> bool {
        self.policy.as_deref() == Some("SNIP")
    }

    /// The credited artist for label releases, when the uploader provided it.
    pub fn publisher_artist(&self) -> Option<&str> {
        self.publisher_metadata.as_ref()?.artist.as_deref().filter(|a| !a.trim().is_empty())
    }
}

/// A track whose download is under way.
pub enum StreamSource {
    /// AAC 160k over HLS, decoded segment by segment.
    Aac(Arc<Segments>),
    /// An MP3 byte stream (progressive, or HLS segments concatenated).
    Mp3(Arc<StreamBuffer>),
}

pub struct OpenStream {
    pub source: StreamSource,
    /// Human-readable quality, e.g. "AAC 160k".
    pub label: &'static str,
}

impl OpenStream {
    /// Stop downloading (and wake any reader waiting on it).
    pub fn cancel(&self) {
        match &self.source {
            StreamSource::Aac(s) => s.cancel(),
            StreamSource::Mp3(b) => b.cancel(),
        }
    }
}

/// SoundCloud answered with an error status. Its message never includes the
/// URL: those carry the client id and signed stream tokens.
#[derive(Debug)]
pub struct HttpStatus(pub StatusCode);

impl std::fmt::Display for HttpStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let code = self.0.as_u16();
        match code {
            401 | 403 => write!(f, "SoundCloud refused the request ({code})"),
            404 => write!(f, "SoundCloud says it doesn't exist (404)"),
            429 => write!(f, "SoundCloud is rate-limiting us (429); try again in a minute"),
            500..=599 => write!(f, "SoundCloud is having trouble ({code})"),
            _ => write!(f, "SoundCloud returned {code}"),
        }
    }
}

impl std::error::Error for HttpStatus {}

/// GET a URL outside api-v2 (stream playlists, segments), with errors that
/// don't leak the signed URL.
fn fetch(http: &Client, url: &str) -> Result<reqwest::blocking::Response> {
    let resp = http.get(url).send().map_err(reqwest::Error::without_url)?;
    match resp.status() {
        s if s.is_success() => Ok(resp),
        s => Err(HttpStatus(s).into()),
    }
}

/// A short, human explanation of an error for the status line: no URLs,
/// no tokens, no reqwest internals.
pub fn friendly_error(e: &anyhow::Error) -> String {
    for cause in e.chain() {
        if let Some(s) = cause.downcast_ref::<HttpStatus>() {
            return s.to_string();
        }
        if let Some(r) = cause.downcast_ref::<reqwest::Error>() {
            if r.is_timeout() {
                return "SoundCloud took too long to answer (network timeout)".into();
            }
            if r.is_connect() || r.is_request() {
                return "can't reach SoundCloud (network problem?)".into();
            }
        }
    }
    let url = Regex::new(r"\(?(?:https?://|for url)\S*\)?").unwrap();
    let text = url.replace_all(&format!("{e:#}"), "").to_string();
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn is_auth_error(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<HttpStatus>().is_some_and(|s| matches!(s.0.as_u16(), 401 | 403 | 404))
    })
}

#[derive(Deserialize)]
struct Page {
    collection: Vec<Value>,
    #[serde(default)]
    next_href: Option<String>,
}

/// Parse each element on its own so one odd entry can't sink a whole page.
fn parse_each<T: DeserializeOwned>(values: Vec<Value>) -> Vec<T> {
    values.into_iter().filter_map(|v| serde_json::from_value(v).ok()).collect()
}

#[derive(Deserialize)]
struct StreamUrl {
    url: String,
}

pub struct SoundCloud {
    http: Client,
    client_id: Mutex<Option<String>>,
}

impl SoundCloud {
    pub fn new() -> Result<Self> {
        let http = Client::builder().user_agent(USER_AGENT).build()?;
        Ok(Self {
            http,
            client_id: Mutex::new(load_cached_client_id()),
        })
    }

    pub fn search_tracks(&self, q: &str) -> Result<Vec<Track>> {
        self.get_all("/search/tracks", &[("q", q)])
    }

    pub fn search_users(&self, q: &str) -> Result<Vec<User>> {
        self.get_all("/search/users", &[("q", q)])
    }

    pub fn search_albums(&self, q: &str) -> Result<Vec<Playlist>> {
        self.get_all("/search/albums", &[("q", q)])
    }

    pub fn search_playlists(&self, q: &str) -> Result<Vec<Playlist>> {
        self.get_all("/search/playlists_without_albums", &[("q", q)])
    }

    /// The track's cover (or the uploader's avatar), 300×300, cached on disk.
    /// Returns the cached file's path and the decoded image.
    pub fn artwork(&self, track: &Track) -> Result<(PathBuf, image::DynamicImage)> {
        let path = crate::art::cache_path(track.id).context("no cache dir")?;
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(_) => {
                let url = track
                    .artwork_url
                    .as_deref()
                    .or(track.user.avatar_url.as_deref())
                    .context("no artwork")?
                    .replace("-large.", "-t300x300.");
                let b = self.http.get(url).send()?.error_for_status()?.bytes()?.to_vec();
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                let _ = std::fs::write(&path, &b);
                b
            }
        };
        Ok((path, image::load_from_memory(&bytes)?))
    }

    /// Official download (the uploader's file, via SoundCloud's download
    /// button). Needs the user's session token. `progress` gets
    /// (bytes so far, total if known). Returns the saved file.
    pub fn download(
        &self,
        track: &Track,
        token: &str,
        dir: &Path,
        progress: impl Fn(u64, Option<u64>),
    ) -> Result<PathBuf> {
        let id = self.client_id(false)?;
        self.download_from(API, &id, track, token, dir, progress)
    }

    fn download_from(
        &self,
        api: &str,
        client_id: &str,
        track: &Track,
        token: &str,
        dir: &Path,
        progress: impl Fn(u64, Option<u64>),
    ) -> Result<PathBuf> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Redirect {
            redirect_uri: String,
        }
        let resp = self
            .http
            .get(format!("{api}/tracks/{}/download", track.id))
            .query(&[("client_id", client_id)])
            .header("Authorization", format!("OAuth {}", token.trim()))
            .send()?;
        match resp.status() {
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                bail!("SoundCloud refused: your session token is missing or expired (Settings)")
            }
            StatusCode::NOT_FOUND => bail!("this track isn't downloadable"),
            s if !s.is_success() => bail!("SoundCloud returned {s}"),
            _ => {}
        }
        let url = resp.json::<Redirect>()?.redirect_uri;
        let mut file_resp = self.http.get(url).send()?.error_for_status()?;
        let ext = download_extension(&file_resp);
        let path = library::unique_path(dir, &library::base_name(track), &ext);
        let total = file_resp.content_length();
        let tmp = path.with_extension(format!("{ext}.part"));
        let mut out = std::fs::File::create(&tmp)?;
        let (mut buf, mut done) = (vec![0; 256 * 1024], 0u64);
        loop {
            let n = file_resp.read(&mut buf)?;
            if n == 0 {
                break;
            }
            std::io::Write::write_all(&mut out, &buf[..n])?;
            done += n as u64;
            progress(done, total);
        }
        std::fs::rename(&tmp, &path)?;
        Ok(path)
    }

    /// Tracks tagged with a genre/tag (most popular first; search caps at ~300).
    pub fn tag_tracks(&self, tag: &str) -> Result<Vec<Track>> {
        self.get_all("/search/tracks", &[("q", "*"), ("filter.genre_or_tag", tag)])
    }

    /// The newest uploads tagged with a genre/tag.
    pub fn tag_recent(&self, tag: &str) -> Result<Vec<Track>> {
        // This endpoint rejects pages over 50.
        let path = format!("/recent-tracks/{}", crate::tags::encode_path(tag));
        self.get_pages(&path, &[], "50", 200)
    }

    /// Albums and playlists tagged with a genre/tag.
    pub fn tag_playlists(&self, tag: &str) -> Result<Vec<Playlist>> {
        self.get_all("/search/playlists", &[("q", "*"), ("filter.genre_or_tag", tag)])
    }

    /// SoundCloud's "related tracks" (what the website's autoplay uses).
    pub fn related_tracks(&self, track: u64) -> Result<Vec<Track>> {
        self.get_page(&format!("/tracks/{track}/related"), &[("limit", "30")])
    }

    /// Another upload of the same song that we *can* play (for DRM-only
    /// tracks): same title, about the same length, not a remix or edit.
    pub fn playable_alternative(&self, track: &Track) -> Result<Option<Track>> {
        let (artist, title) = crate::lyrics::artist_and_title(track);
        let q = format!("{artist} {title}");
        let candidates: Vec<Track> = self.get_page("/search/tracks", &[("q", q.as_str()), ("limit", "50")])?;
        let len = track.full_duration.unwrap_or(track.duration) as i64;
        let tolerance = (len / 12).max(10_000); // ~8%, at least 10 s
        let mut matches: Vec<Track> = candidates
            .into_iter()
            .filter(|t| t.id != track.id && t.is_playable() && !t.is_drm_only() && !t.is_preview())
            .filter(|t| is_same_version(&track.title, &t.title, &artist, &title))
            .filter(|t| (t.duration as i64 - len).abs() <= tolerance)
            .collect();
        matches.sort_by_key(|t| ((t.duration as i64 - len).abs() / 2000, std::cmp::Reverse(t.playback_count)));
        Ok(matches.into_iter().next())
    }

    pub fn user_top_tracks(&self, user: u64) -> Result<Vec<Track>> {
        self.get_page(&format!("/users/{user}/toptracks"), &[("limit", "50")])
    }

    /// Every public track the user has uploaded, newest first.
    pub fn user_tracks(&self, user: u64) -> Result<Vec<Track>> {
        self.get_all(&format!("/users/{user}/tracks"), &[])
    }

    /// Albums, EPs and singles.
    pub fn user_albums(&self, user: u64) -> Result<Vec<Playlist>> {
        self.get_all(&format!("/users/{user}/albums"), &[])
    }

    pub fn user_playlists(&self, user: u64) -> Result<Vec<Playlist>> {
        self.get_all(&format!("/users/{user}/playlists_without_albums"), &[])
    }

    /// All tracks of a playlist/album, in order, as full playable tracks.
    pub fn playlist_tracks(&self, playlist: &Playlist) -> Result<Vec<Track>> {
        let stubs = if playlist.tracks.is_empty() {
            let body = self.get_api(&format!("{API}/playlists/{}", playlist.id), &[])?;
            serde_json::from_str::<Playlist>(&body)?.tracks
        } else {
            playlist.tracks.clone()
        };

        let mut full: HashMap<u64, Track> = HashMap::new();
        let mut order = Vec::new();
        let mut missing = Vec::new();
        for stub in stubs {
            let Some(id) = stub.get("id").and_then(Value::as_u64) else { continue };
            order.push(id);
            match serde_json::from_value::<Track>(stub) {
                Ok(t) if !t.media.transcodings.is_empty() => {
                    full.insert(id, t);
                }
                _ => missing.push(id),
            }
        }
        full.extend(self.tracks_by_ids(&missing)?.into_iter().map(|t| (t.id, t)));
        Ok(order.into_iter().filter_map(|id| full.remove(&id)).collect())
    }

    /// Fresh, playable copies of tracks by id, in the order given (tracks
    /// that no longer exist are skipped).
    pub fn tracks_by_ids(&self, ids: &[u64]) -> Result<Vec<Track>> {
        let mut found: HashMap<u64, Track> = HashMap::new();
        for chunk in ids.chunks(50) {
            let ids = chunk.iter().map(u64::to_string).collect::<Vec<_>>().join(",");
            let body = self.get_api(&format!("{API}/tracks"), &[("ids", &ids)])?;
            let tracks: Vec<Track> = parse_each(serde_json::from_str(&body)?);
            found.extend(tracks.into_iter().map(|t| (t.id, t)));
        }
        Ok(ids.iter().filter_map(|id| found.remove(id)).collect())
    }

    /// Resolve a soundcloud.com / on.soundcloud.com URL to a track, artist
    /// or playlist.
    pub fn resolve(&self, link: &Link) -> Result<Item> {
        let url = if link.is_short() {
            // Short links redirect to the real page (with tracking params).
            let real = fetch(&self.http, &link.url)?.url().to_string();
            links::parse(&real).map(|l| l.url).unwrap_or(real)
        } else {
            link.url.clone()
        };
        let body = self.get_api(&format!("{API}/resolve"), &[("url", url.as_str())])?;
        let v: Value = serde_json::from_str(&body)?;
        Ok(match v.get("kind").and_then(Value::as_str) {
            Some("track") => Item::Track(serde_json::from_value(v)?),
            Some("user") => Item::User(serde_json::from_value(v)?),
            Some("playlist") => Item::Playlist(serde_json::from_value(v)?),
            other => bail!("unsupported link type: {other:?}"),
        })
    }

    /// One page of a collection endpoint.
    fn get_page<T: DeserializeOwned>(&self, path: &str, params: &[(&str, &str)]) -> Result<Vec<T>> {
        let body = self.get_api(&format!("{API}{path}"), params)?;
        let page: Page = serde_json::from_str(&body).context("unexpected response")?;
        Ok(parse_each(page.collection))
    }

    /// Follow `next_href` until the collection is exhausted. (Search
    /// endpoints stop returning results after ~300, whatever the total.)
    fn get_all<T: DeserializeOwned>(&self, path: &str, params: &[(&str, &str)]) -> Result<Vec<T>> {
        self.get_pages(path, params, "200", 5000)
    }

    /// `get_all` with a smaller page size (some endpoints reject 200) and
    /// an item cap.
    fn get_pages<T: DeserializeOwned>(
        &self,
        path: &str,
        params: &[(&str, &str)],
        page_size: &str,
        max_items: usize,
    ) -> Result<Vec<T>> {
        let mut first = params.to_vec();
        first.extend([("limit", page_size), ("linked_partitioning", "1")]);
        let mut body = self.get_api(&format!("{API}{path}"), &first)?;
        let mut out = Vec::new();
        // Pages are filtered server-side after paging, so one can come back
        // empty mid-list; but search keeps handing out empty pages forever.
        let mut empty_streak = 0;
        loop {
            let page: Page = serde_json::from_str(&body).context("unexpected response")?;
            empty_streak = if page.collection.is_empty() { empty_streak + 1 } else { 0 };
            out.extend(parse_each(page.collection));
            match page.next_href {
                Some(next) if empty_streak < 2 && out.len() < max_items => {
                    body = self.get_api(&next, &[])?
                }
                _ => return Ok(out),
            }
        }
    }

    /// Start streaming the track in the best quality we can get without a
    /// Go+ subscription: unencrypted AAC 160k, else AAC 96k (all SoundCloud
    /// offers for many monetized tracks), else the MP3 a Go+ preview has.
    /// Returns as soon as the first request succeeds; downloading continues
    /// in the background.
    pub fn open_stream(&self, track: &Track) -> Result<OpenStream> {
        match self.open_stream_once(track) {
            // The track's `track_authorization` goes stale after a while (a
            // track that sat in the queue, history or favorites): fetch a
            // fresh copy of it and try once more.
            Err(e) if is_auth_error(&e) => match self.tracks_by_ids(&[track.id])?.into_iter().next() {
                Some(fresh) => self.open_stream_once(&fresh),
                None => Err(e),
            },
            other => other,
        }
    }

    fn open_stream_once(&self, track: &Track) -> Result<OpenStream> {
        let t = &track.media.transcodings;
        let mut last_error = None;
        for (preset, label) in [("aac_160k", "AAC 160k"), ("aac_96k", "AAC 96k")] {
            let Some(tc) = t.iter().find(|x| {
                x.preset.as_deref() == Some(preset) && x.format.protocol == "hls" && !x.snipped
            }) else {
                continue;
            };
            let opened = self.stream_url(track, tc).and_then(|u| {
                let playlist = fetch(&self.http, &u)?.text()?;
                Segments::start(&self.http, hls::parse_playlist(&playlist)?)
            });
            match opened {
                Ok(segments) => return Ok(OpenStream { source: StreamSource::Aac(segments), label }),
                Err(e) => last_error = Some(e),
            }
        }
        if t.iter().any(|x| x.format.mime_type == "audio/mpeg" && x.is_plain()) {
            match self.open_mp3(track) {
                Ok(s) => return Ok(s),
                Err(e) => last_error = Some(e),
            }
        }
        if let Some(e) = last_error {
            return Err(e);
        }
        if t.iter().any(|x| x.format.protocol.contains("encrypted")) {
            bail!("DRM-protected label track, no unencrypted stream");
        }
        bail!("SoundCloud offers no playable stream for this track")
    }

    fn open_mp3(&self, track: &Track) -> Result<OpenStream> {
        let t = &track.media.transcodings;
        let is_mp3 = |x: &&Transcoding| x.format.mime_type == "audio/mpeg" && !x.snipped;
        let is_snippet = |x: &&Transcoding| x.format.mime_type == "audio/mpeg" && x.snipped;
        let mp3 = t
            .iter()
            .filter(is_mp3)
            .find(|x| x.format.protocol == "progressive")
            .or_else(|| t.iter().filter(is_mp3).find(|x| x.format.protocol == "hls"))
            // Go+ tracks only offer a 30-second snippet: better than nothing.
            .or_else(|| t.iter().filter(is_snippet).find(|x| x.format.protocol == "progressive"))
            .or_else(|| t.iter().filter(is_snippet).find(|x| x.format.protocol == "hls"))
            .ok_or_else(|| anyhow!("no unencrypted stream available"))?;
        let media_url = self.stream_url(track, mp3)?;

        let buffer = if mp3.format.protocol == "progressive" {
            let mut resp = fetch(&self.http, &media_url)?;
            let buffer = StreamBuffer::new(resp.content_length());
            let b = buffer.clone();
            std::thread::spawn(move || {
                let mut chunk = vec![0; 64 * 1024];
                let error = loop {
                    match resp.read(&mut chunk) {
                        Ok(0) => break None,
                        Ok(n) if b.append(&chunk[..n]) => {}
                        Ok(_) => break None, // cancelled
                        Err(e) => break Some(e.to_string()),
                    }
                };
                b.finish(error);
            });
            buffer
        } else {
            // Rare fallback: MP3 segments are raw frames, so concatenate them.
            let playlist = fetch(&self.http, &media_url)?.text()?;
            let buffer = StreamBuffer::new(None);
            let (b, http) = (buffer.clone(), self.http.clone());
            std::thread::spawn(move || {
                let urls = playlist.lines().filter(|l| !l.is_empty() && !l.starts_with('#'));
                for url in urls {
                    match http.get(url).send().and_then(|r| r.error_for_status()?.bytes()) {
                        Ok(bytes) if b.append(&bytes) => {}
                        Ok(_) => break,
                        Err(e) => return b.finish(Some(e.without_url().to_string())),
                    }
                }
                b.finish(None);
            });
            buffer
        };
        let label = if mp3.snipped { "MP3 preview" } else { "MP3 128k" };
        Ok(OpenStream { source: StreamSource::Mp3(buffer), label })
    }

    /// Exchange a transcoding's API URL for the actual (signed) media URL.
    fn stream_url(&self, track: &Track, tc: &Transcoding) -> Result<String> {
        let auth = track.track_authorization.as_deref().unwrap_or("");
        let body = self.get_api(&tc.url, &[("track_authorization", auth)])?;
        Ok(serde_json::from_str::<StreamUrl>(&body)?.url)
    }

    /// GET an api-v2 endpoint, re-scraping the client_id once if it was rejected.
    fn get_api(&self, url: &str, params: &[(&str, &str)]) -> Result<String> {
        for attempt in 0..2 {
            let id = self.client_id(attempt > 0)?;
            let resp = self
                .http
                .get(url)
                .query(params)
                .query(&[("client_id", id.as_str())])
                .send()
                .map_err(reqwest::Error::without_url)?;
            match resp.status() {
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN if attempt == 0 => continue,
                s if !s.is_success() => return Err(HttpStatus(s).into()),
                _ => return Ok(resp.text().map_err(reqwest::Error::without_url)?),
            }
        }
        unreachable!()
    }

    fn client_id(&self, force_refresh: bool) -> Result<String> {
        let mut guard = self.client_id.lock().unwrap();
        if let Some(id) = guard.as_ref().filter(|_| !force_refresh) {
            return Ok(id.clone());
        }
        let id = self.scrape_client_id()?;
        save_cached_client_id(&id);
        *guard = Some(id.clone());
        Ok(id)
    }

    /// The web app's client_id is embedded in one of the JS bundles linked
    /// from the homepage (usually one of the last ones).
    fn scrape_client_id(&self) -> Result<String> {
        let html = self.http.get("https://soundcloud.com").send()?.text()?;
        let script_re = Regex::new(r#"https://a-v2\.sndcdn\.com/assets/[^"]+\.js"#)?;
        let id_re = Regex::new(r#"client_id\s*[:=]\s*"([A-Za-z0-9]{32})""#)?;

        let scripts: Vec<&str> = script_re.find_iter(&html).map(|m| m.as_str()).collect();
        for src in scripts.iter().rev() {
            let Ok(js) = self.http.get(*src).send().and_then(|r| r.text()) else {
                continue;
            };
            if let Some(c) = id_re.captures(&js) {
                return Ok(c[1].to_string());
            }
        }
        bail!("could not find a client_id in SoundCloud's JS bundles")
    }
}

/// Is `candidate` (another upload's title) the same version of the song as
/// `original`, not a remix, cover or edit of it? `artist`/`song` are the
/// original's cleaned-up artist and song title.
pub fn is_same_version(original: &str, candidate: &str, artist: &str, song: &str) -> bool {
    use crate::lyrics::normalize;
    let want = normalize(song);
    let got = normalize(candidate);
    if want.is_empty() || !got.contains(&want) {
        return false;
    }
    let strip = |s: String| s.replacen(&want, "", 1).replacen(&normalize(artist), "", 1);
    // What the candidate adds beyond the title and artist ("xblockhead",
    // "amoredit"), minus whatever the original itself adds ("radioedit").
    let extra = strip(got);
    let original_extra = strip(normalize(original));
    let residual = extra.replacen(&original_extra, "", 1).replace("originalmix", "");
    const EDITS: [&str; 18] = [
        "remix", "rmx", "mix", "cover", "slowed", "sped", "nightcore", "reverb", "instrumental",
        "8d", "bassboost", "bootleg", "flip", "vip", "mashup", "rework", "refix", "edit",
    ];
    residual.chars().count() <= 14 && !EDITS.iter().any(|e| residual.contains(e))
}

/// File extension for a download, from Content-Disposition or Content-Type.
fn download_extension(resp: &reqwest::blocking::Response) -> String {
    let header = |name| resp.headers().get(name).and_then(|v| v.to_str().ok()).unwrap_or("");
    let from_name = Regex::new(r#"filename\*?=(?:UTF-8'')?"?[^";]*\.([A-Za-z0-9]{2,5})"?"#)
        .ok()
        .and_then(|re| re.captures(header("content-disposition")).map(|c| c[1].to_lowercase()));
    from_name.unwrap_or_else(|| {
        match header("content-type").split(';').next().unwrap_or("").trim() {
            "audio/wav" | "audio/x-wav" | "audio/wave" => "wav",
            "audio/flac" | "audio/x-flac" => "flac",
            "audio/aiff" | "audio/x-aiff" => "aiff",
            "audio/mp4" | "audio/x-m4a" | "audio/aac" => "m4a",
            "audio/ogg" => "ogg",
            _ => "mp3",
        }
        .into()
    })
}

fn cache_path() -> Option<PathBuf> {
    Some(dirs::cache_dir()?.join("driftwave").join("client_id"))
}

fn load_cached_client_id() -> Option<String> {
    let id = std::fs::read_to_string(cache_path()?).ok()?;
    let id = id.trim();
    (id.len() == 32).then(|| id.to_string())
}

fn save_cached_client_id(id: &str) {
    if let Some(path) = cache_path() {
        let _ = path.parent().map(std::fs::create_dir_all);
        let _ = std::fs::write(path, id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hits the real SoundCloud API: `cargo test -- --ignored`
    #[test]
    #[ignore]
    fn search_and_download() {
        let sc = SoundCloud::new().unwrap();
        let tracks = sc.search_tracks("lofi hip hop").unwrap();
        assert!(!tracks.is_empty());
        let track = tracks.iter().find(|t| t.is_playable()).unwrap();
        let start = std::time::Instant::now();
        let stream = sc.open_stream(track).unwrap();
        let label = stream.label;
        // `prepare` returns once ~0.5 s of audio has been decoded.
        let prepared = crate::player::prepare(stream).unwrap();
        println!("{label}: ready to play after {:.2}s", start.elapsed().as_secs_f32());
        prepared.cancel();
    }

    /// A throwaway HTTP server that answers each request with `respond`.
    fn mock_server(respond: fn(&str, u16) -> String) -> (u16, std::sync::mpsc::Receiver<String>) {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut head = String::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    head.push_str(&line);
                }
                let _ = tx.send(head.clone());
                let _ = (&stream).write_all(respond(&head, port).as_bytes());
            }
        });
        (port, rx)
    }

    #[test]
    fn official_download_flow() {
        let (port, requests) = mock_server(|head, port| {
            let reply = |status: &str, headers: &str, body: &str| {
                format!("HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
            };
            if head.starts_with("GET /tracks/7/download") {
                if !head.to_lowercase().contains("authorization: oauth good-token") {
                    return reply("401 Unauthorized", "", "");
                }
                let body = format!(r#"{{"redirectUri":"http://127.0.0.1:{port}/file"}}"#);
                reply("200 OK", "Content-Type: application/json\r\n", &body)
            } else {
                reply("200 OK", "Content-Type: audio/wav\r\nContent-Disposition: attachment; filename=\"orig.wav\"\r\n", "RIFF-fake-audio")
            }
        });
        let sc = SoundCloud::new().unwrap();
        let track: Track = serde_json::from_value(serde_json::json!({
            "id": 7, "title": "Some Artist - Tune", "duration": 1, "user": {"id": 1, "username": "u"},
            "downloadable": true, "has_downloads_left": true
        }))
        .unwrap();
        assert!(track.can_download());
        let dir = std::env::temp_dir().join(format!("driftwave-dl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let api = format!("http://127.0.0.1:{port}");

        let err = sc.download_from(&api, "cid", &track, "bad", &dir, |_, _| {}).unwrap_err();
        assert!(err.to_string().contains("session token"), "{err}");

        let got = std::cell::Cell::new(0);
        let path = sc.download_from(&api, "cid", &track, " good-token\n", &dir, |n, _| got.set(n)).unwrap();
        assert_eq!(path.file_name().unwrap(), "Some Artist - Tune.wav");
        assert_eq!(std::fs::read(&path).unwrap(), b"RIFF-fake-audio");
        assert_eq!(got.get(), 15, "progress reported");
        let first = requests.try_iter().find(|r| r.contains("good-token")).unwrap();
        assert!(first.contains("client_id=cid"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[ignore]
    fn finds_playable_upload_for_drm_track() {
        let sc = SoundCloud::new().unwrap();
        let tracks = sc.search_tracks("aesop rock trouble trouble").unwrap();
        let drm = tracks.iter().find(|t| t.is_drm_only() && t.title.contains("Trouble")).expect("a DRM-only track");
        let alt = sc.playable_alternative(drm).unwrap().expect("another upload");
        println!("{} ({}) → {} by {}", drm.title, drm.user.username, alt.title, alt.user.username);
        assert!(!alt.is_drm_only());
        let stream = sc.open_stream(&alt).expect("the alternative actually streams");
        stream.cancel();
    }

    /// Prints what every DRM-only Aesop Rock track would fall back to.
    #[test]
    #[ignore]
    fn alternatives_report() {
        let sc = SoundCloud::new().unwrap();
        for t in sc.search_tracks("aesop rock").unwrap().iter().filter(|t| t.is_drm_only()).take(12) {
            match sc.playable_alternative(t).unwrap() {
                Some(a) => println!("{:<22} → {} [{}]", t.title, a.title, a.user.username),
                None => println!("{:<22} → (none, skip)", t.title),
            }
        }
    }

    #[test]
    #[ignore]
    fn genre_endpoints() {
        let sc = SoundCloud::new().unwrap();
        let top = sc.tag_tracks("folk punk").unwrap();
        let newest = sc.tag_recent("folk punk").unwrap();
        let lists = sc.tag_playlists("hackercore").unwrap();
        println!("#folk punk: {} top, {} newest; #hackercore: {} playlists", top.len(), newest.len(), lists.len());
        assert!(top.len() > 50 && newest.len() > 50 && !lists.is_empty());
        let tagged = |t: &Track| {
            let all = format!("{} {}", t.genre.as_deref().unwrap_or(""), t.tag_list.as_deref().unwrap_or("")).to_lowercase();
            all.contains("folk punk") || all.contains("folkpunk") || all.contains("folk-punk")
        };
        let share = top.iter().filter(|t| tagged(t)).count() * 100 / top.len();
        println!("{share}% of top tracks carry the tag");
        assert!(share > 80);
    }

    /// Many monetized tracks only offer AAC 96k (no 160k, no MP3); they
    /// used to fail with "no unencrypted stream available". Decodes ~0.5 s
    /// into memory, plays nothing.
    #[test]
    #[ignore]
    fn plays_aac_96k_only_tracks() {
        let sc = SoundCloud::new().unwrap();
        let only_96k = |t: &&Track| {
            let has = |p: &str| t.media.transcodings.iter().any(|x| x.preset.as_deref() == Some(p) && x.format.protocol == "hls");
            has("aac_96k") && !has("aac_160k")
        };
        let mut tried = 0;
        for q in ["kpop", "drake", "taylor swift"] {
            for t in sc.search_tracks(q).unwrap().iter().filter(only_96k).take(2) {
                assert!(!t.is_drm_only());
                let stream = sc.open_stream(t).unwrap_or_else(|e| panic!("{}: {e:#}", t.title));
                assert_eq!(stream.label, "AAC 96k");
                crate::player::prepare(stream).unwrap().cancel();
                println!("ok: {}", t.title);
                tried += 1;
            }
        }
        assert!(tried > 0, "found no 96k-only tracks to test");
    }

    /// An expired `track_authorization` is replaced with a fresh one.
    #[test]
    #[ignore]
    fn stale_authorization_is_refreshed() {
        let sc = SoundCloud::new().unwrap();
        let mut t = sc.search_tracks("lofi").unwrap().into_iter().find(|t| !t.is_drm_only() && !t.is_preview()).unwrap();
        t.track_authorization = Some("expired".into());
        sc.open_stream(&t).expect("refetched and streamed").cancel();
    }

    fn transcoding(preset: &str, protocol: &str) -> serde_json::Value {
        serde_json::json!({"url": "u", "preset": preset, "format": {"protocol": protocol, "mime_type": "audio/mp4"}})
    }

    #[test]
    fn drm_only_needs_every_stream_encrypted() {
        let track = |tcs: Vec<serde_json::Value>| -> Track {
            serde_json::from_value(serde_json::json!({
                "id": 1, "title": "t", "duration": 1, "user": {"id": 1, "username": "u"}, "media": {"transcodings": tcs}
            }))
            .unwrap()
        };
        assert!(track(vec![transcoding("aac_160k", "cbc-encrypted-hls"), transcoding("aac_96k", "ctr-encrypted-hls")]).is_drm_only());
        assert!(!track(vec![transcoding("aac_160k", "cbc-encrypted-hls"), transcoding("aac_96k", "hls")]).is_drm_only());
        assert!(!track(vec![transcoding("aac_96k", "hls"), transcoding("abr_sq", "hls")]).is_drm_only());
    }

    #[test]
    fn friendly_errors_hide_urls() {
        let e: anyhow::Error = HttpStatus(StatusCode::FORBIDDEN).into();
        assert_eq!(friendly_error(&e.context("opening stream")), "SoundCloud refused the request (403)");
        let e = anyhow!("error decoding https://playback.example/x.m4s?Policy=abc&Signature=xyz (for url https://a/b)");
        let msg = friendly_error(&e);
        assert!(!msg.contains("http") && !msg.contains("Signature"), "{msg}");
        assert!(is_auth_error(&HttpStatus(StatusCode::NOT_FOUND).into()));
        assert!(!is_auth_error(&HttpStatus(StatusCode::TOO_MANY_REQUESTS).into()));
    }

    #[test]
    fn same_version_matching() {
        let same = |orig: &str, cand: &str, artist: &str, song: &str| is_same_version(orig, cand, artist, song);
        assert!(same("Trouble Trouble", "Aesop Rock x Blockhead - Trouble Trouble", "Aesop Rock", "Trouble Trouble"));
        assert!(same("Rings", "Aesop Rock - Rings", "Aesop Rock", "Rings"));
        assert!(same("Strobe", "deadmau5 - Strobe (Original Mix)", "deadmau5", "Strobe"));
        assert!(same("One More Time (Radio Edit)", "One More Time (Radio Edit)", "Daft Punk", "One More Time"));
        assert!(!same("One More Time (Radio Edit)", "Daft Punk- One More Time (Amor Edit) [FREE DL]", "Daft Punk", "One More Time"));
        assert!(!same("Instant Crush (feat. Julian Casablancas)", "Daft Punk - Instant Crush (Atlantis Mix)", "Daft Punk", "Instant Crush"));
        assert!(!same("None Shall Pass", "Aesop Rock - None Shall Pass (Nikora's Constant Value RMX)", "Aesop Rock", "None Shall Pass"));
        assert!(!same("Rings", "Aesop Rock - Rings (Cover by Jon Watts)", "Aesop Rock", "Rings"));
        assert!(!same("Daylight", "Aesop Rock - Daylight (slowed + reverb)", "Aesop Rock", "Daylight"));
        assert!(!same("Rings", "Something Else", "Aesop Rock", "Rings"));
    }

    /// Prints what Go+ preview tracks fall back to, and checks the fallback
    /// is the full song and actually streams.
    #[test]
    #[ignore]
    fn preview_fallbacks() {
        let sc = SoundCloud::new().unwrap();
        let previews: Vec<Track> = sc.search_tracks("daft punk").unwrap().into_iter().filter(Track::is_preview).take(8).collect();
        assert!(!previews.is_empty(), "no Go+ previews found to test with");
        let mut found = 0;
        for t in &previews {
            match sc.playable_alternative(t).unwrap() {
                Some(a) => {
                    found += 1;
                    let full = t.full_duration.unwrap_or(0) / 1000;
                    println!("{:<24} {}s preview → {} [{}] {}s", t.title, t.duration / 1000, a.title, a.user.username, a.duration / 1000);
                    assert!(!a.is_preview() && (a.duration / 1000).abs_diff(full) <= 30);
                }
                None => println!("{:<24} → (none: plays the 30 s preview)", t.title),
            }
        }
        // With no full upload, the snippet itself must still stream.
        let snippet = sc.open_stream(&previews[0]).expect("the preview snippet streams");
        snippet.cancel();
        assert!(found > 0);
    }

    #[test]
    #[ignore]
    fn artist_catalog() {
        let sc = SoundCloud::new().unwrap();
        let artist = &sc.search_users("deadmau5").unwrap()[0];
        let tracks = sc.user_tracks(artist.id).unwrap();
        let albums = sc.user_albums(artist.id).unwrap();
        let top = sc.user_top_tracks(artist.id).unwrap();
        println!("{}: {} tracks, {} albums, {} top", artist.username, tracks.len(), albums.len(), top.len());
        assert!(tracks.len() > 200, "pagination should go past one page");

        let album = albums.iter().max_by_key(|a| a.track_count).unwrap();
        let album_tracks = sc.playlist_tracks(album).unwrap();
        println!("{} ({}): {}/{:?} tracks", album.title, album.kind(), album_tracks.len(), album.track_count);
        assert!(album_tracks.len() > 5, "stub tracks should be hydrated");
        assert!(album_tracks.iter().all(|t| !t.media.transcodings.is_empty()));
    }
}
