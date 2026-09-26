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
    /// releases): the plain MP3 it lists doesn't exist, so we can't play it.
    pub fn is_drm_only(&self) -> bool {
        self.media.transcodings.iter().any(|x| x.format.protocol.contains("encrypted"))
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
        let want = crate::lyrics::normalize(&title);
        let len = track.full_duration.unwrap_or(track.duration) as i64;
        let tolerance = (len / 12).max(10_000); // ~8%, at least 10 s
        const EDITS: [&str; 17] = [
            "remix", "rmx", "cover", "slowed", "sped", "nightcore", "reverb", "instrumental", "8d",
            "bass boosted", "bootleg", "flip", "vip", "mashup", "rework", "refix", "edit",
        ];
        let original = track.title.to_lowercase();
        let is_edit = |t: &Track| {
            let lower = t.title.to_lowercase();
            EDITS.iter().any(|e| lower.contains(e) && !original.contains(e))
        };
        let mut matches: Vec<Track> = candidates
            .into_iter()
            .filter(|t| t.id != track.id && t.is_playable() && !t.is_drm_only() && !t.is_preview() && !is_edit(t))
            .filter(|t| {
                // The title must be this song, give or take the artist's name
                // and a little extra ("x Blockhead"), not a longer variant.
                let got = crate::lyrics::normalize(&t.title);
                let leftover = got.replacen(&want, "", 1).replacen(&crate::lyrics::normalize(&artist), "", 1);
                got.contains(&want) && leftover.chars().count() <= 14
            })
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
    pub fn resolve(&self, url: &str) -> Result<Item> {
        let body = self.get_api(&format!("{API}/resolve"), &[("url", url)])?;
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
    /// Go+ subscription: unencrypted AAC 160k if offered, else MP3 128k.
    /// Returns as soon as the first request succeeds; downloading continues
    /// in the background.
    pub fn open_stream(&self, track: &Track) -> Result<OpenStream> {
        let t = &track.media.transcodings;
        let find = |preset: &str, protocol: &str| {
            t.iter().find(|x| {
                x.preset.as_deref() == Some(preset) && x.format.protocol == protocol && !x.snipped
            })
        };

        // AAC on monetized tracks is only offered DRM-encrypted
        // (cbc/ctr-encrypted-hls); those fall through to MP3.
        if let Some(aac) = find("aac_160k", "hls")
            && let Ok(segments) = self.stream_url(track, aac).and_then(|u| {
                let playlist = self.http.get(&u).send()?.error_for_status()?.text()?;
                Segments::start(&self.http, hls::parse_playlist(&playlist)?)
            })
        {
            return Ok(OpenStream { source: StreamSource::Aac(segments), label: "AAC 160k" });
        }

        let has_drm = t.iter().any(|x| x.format.protocol.contains("encrypted"));
        self.open_mp3(track).map_err(|e| {
            if has_drm {
                anyhow!("DRM-protected label track, no unencrypted stream")
            } else {
                e
            }
        })
    }

    fn open_mp3(&self, track: &Track) -> Result<OpenStream> {
        let t = &track.media.transcodings;
        let is_mp3 = |x: &&Transcoding| x.format.mime_type == "audio/mpeg" && !x.snipped;
        let mp3 = t
            .iter()
            .filter(is_mp3)
            .find(|x| x.format.protocol == "progressive")
            .or_else(|| t.iter().filter(is_mp3).find(|x| x.format.protocol == "hls"))
            .ok_or_else(|| anyhow!("no unencrypted stream available"))?;
        let media_url = self.stream_url(track, mp3)?;

        let buffer = if mp3.format.protocol == "progressive" {
            let mut resp = self.http.get(&media_url).send()?.error_for_status()?;
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
            let playlist = self.http.get(&media_url).send()?.error_for_status()?.text()?;
            let buffer = StreamBuffer::new(None);
            let (b, http) = (buffer.clone(), self.http.clone());
            std::thread::spawn(move || {
                let urls = playlist.lines().filter(|l| !l.is_empty() && !l.starts_with('#'));
                for url in urls {
                    match http.get(url).send().and_then(|r| r.error_for_status()?.bytes()) {
                        Ok(bytes) if b.append(&bytes) => {}
                        Ok(_) => break,
                        Err(e) => return b.finish(Some(e.to_string())),
                    }
                }
                b.finish(None);
            });
            buffer
        };
        Ok(OpenStream { source: StreamSource::Mp3(buffer), label: "MP3 128k" })
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
                .send()?;
            match resp.status() {
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN if attempt == 0 => continue,
                s if !s.is_success() => bail!("SoundCloud returned {s} for {url}"),
                _ => return Ok(resp.text()?),
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
