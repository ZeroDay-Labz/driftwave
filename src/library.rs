//! The user's library folder:
//!
//!   <library>/Lyrics/Artist - Title.lrc   (synced; or .txt for plain text)
//!   <library>/Covers/Artist - Title.jpg
//!   <library>/Downloads/Artist - Title.<ext>
//!
//! Lyrics files there are used before any online lookup, so a hand-made
//! .lrc overrides whatever LRCLIB or Genius would return.

use std::path::{Path, PathBuf};

use crate::api::Track;
use crate::lyrics::{self, Found, Lyrics};

pub fn default_dir() -> Option<PathBuf> {
    Some(dirs::audio_dir().or_else(|| Some(dirs::home_dir()?.join("Music")))?.join("driftwave"))
}

/// Expand a leading `~`.
pub fn expand(path: &str) -> PathBuf {
    match path.strip_prefix("~") {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => {
            dirs::home_dir().unwrap_or_default().join(rest.trim_start_matches('/'))
        }
        _ => PathBuf::from(path),
    }
}

/// Make a string safe to use as a file name on any filesystem.
pub fn sanitize(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_control() || r#"/\:*?"<>|"#.contains(c) { '_' } else { c })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.').trim();
    let short: String = trimmed.chars().take(150).collect();
    if short.is_empty() { "untitled".into() } else { short }
}

/// "Artist - Title", the base file name for everything about a track.
pub fn base_name(track: &Track) -> String {
    let (artist, title) = lyrics::artist_and_title(track);
    sanitize(&format!("{artist} - {title}"))
}

pub fn subdir(library: &Path, name: &str) -> std::io::Result<PathBuf> {
    let dir = library.join(name);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Lyrics saved in (or dropped into) the library for this track.
pub fn load_lyrics(library: &Path, track: &Track) -> Option<Found> {
    let base = library.join("Lyrics").join(base_name(track));
    let (lyrics, kind) = if let Ok(text) = std::fs::read_to_string(base.with_extension("lrc")) {
        let lines = lyrics::parse_lrc(&text);
        if lines.is_empty() { (Lyrics::Plain(text), "lrc") } else { (Lyrics::Synced(lines), "lrc") }
    } else {
        (Lyrics::Plain(std::fs::read_to_string(base.with_extension("txt")).ok()?), "txt")
    };
    Some(Found {
        lyrics,
        matched: format!("{}.{kind}", base_name(track)),
        note: None,
        source: "library".into(),
    })
}

/// Save lyrics found online, never overwriting a file that's already there.
pub fn save_lyrics(library: &Path, track: &Track, found: &Found) -> std::io::Result<()> {
    let (ext, text) = match &found.lyrics {
        Lyrics::Synced(lines) => ("lrc", to_lrc(track, lines)),
        Lyrics::Plain(text) => ("txt", text.clone()),
        Lyrics::Instrumental => return Ok(()),
    };
    let dir = subdir(library, "Lyrics")?;
    let base = dir.join(base_name(track));
    if base.with_extension("lrc").exists() || base.with_extension("txt").exists() {
        return Ok(());
    }
    std::fs::write(base.with_extension(ext), text)
}

fn to_lrc(track: &Track, lines: &[(u64, String)]) -> String {
    let (artist, title) = lyrics::artist_and_title(track);
    let mut out = format!("[ar:{artist}]\n[ti:{title}]\n");
    for (ms, text) in lines {
        let cs = ms / 10;
        out.push_str(&format!("[{:02}:{:02}.{:02}]{text}\n", cs / 6000, cs / 100 % 60, cs % 100));
    }
    out
}

/// Copy a cached cover into the library (once).
pub fn save_cover(library: &Path, track: &Track, cached: &Path) -> std::io::Result<()> {
    let dest = subdir(library, "Covers")?.join(base_name(track)).with_extension("jpg");
    if !dest.exists() {
        std::fs::copy(cached, dest)?;
    }
    Ok(())
}

/// A path in `dir` for `name.ext` that doesn't exist yet ("name (2).ext", …).
pub fn unique_path(dir: &Path, name: &str, ext: &str) -> PathBuf {
    let mut path = dir.join(format!("{name}.{ext}"));
    let mut n = 2;
    while path.exists() {
        path = dir.join(format!("{name} ({n}).{ext}"));
        n += 1;
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track() -> Track {
        serde_json::from_value(serde_json::json!({
            "id": 7, "title": "Artist/Name - Song: Part 2 (feat. X) [Free DL]", "duration": 1000,
            "user": {"id": 1, "username": "uploader"}
        }))
        .unwrap()
    }

    #[test]
    fn file_names_are_safe() {
        assert_eq!(sanitize(" a/b\\c:d*e?f\"g<h>i|j\u{7} "), "a_b_c_d_e_f_g_h_i_j_");
        assert_eq!(sanitize("..."), "untitled");
        assert_eq!(base_name(&track()), "Artist_Name - Song_ Part 2");
    }

    #[test]
    fn lrc_round_trip_and_user_files_win() {
        let dir = std::env::temp_dir().join(format!("driftwave-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let t = track();
        let lines = vec![(1_500, "one".to_string()), (62_250, "two".to_string())];
        let found = Found { lyrics: Lyrics::Synced(lines.clone()), matched: String::new(), note: None, source: "LRCLIB".into() };
        save_lyrics(&dir, &t, &found).unwrap();
        match load_lyrics(&dir, &t).unwrap().lyrics {
            Lyrics::Synced(back) => assert_eq!(back, lines),
            _ => panic!("expected synced lyrics"),
        }
        // An existing file (e.g. the user's own) is never overwritten.
        let plain = Found { lyrics: Lyrics::Plain("other".into()), ..found };
        save_lyrics(&dir, &t, &plain).unwrap();
        assert!(matches!(load_lyrics(&dir, &t).unwrap().lyrics, Lyrics::Synced(_)));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn tilde_expands() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(expand("~/Music/x"), home.join("Music/x"));
        assert_eq!(expand("/abs"), PathBuf::from("/abs"));
    }
}
