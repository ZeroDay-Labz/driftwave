//! Settings and history that survive restarts:
//! `~/.config/driftwave/state.json`.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::api::Track;
use crate::queue::Repeat;

const HISTORY_LEN: usize = 200;

#[derive(Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub volume: f32,
    pub shuffle: bool,
    pub repeat: Repeat,
    pub radio: bool,
    pub last_query: String,
    /// Terminal to open the lyrics window in, e.g. "kitty" (auto-detected if unset).
    pub terminal: Option<String>,
    pub notifications: bool,
    /// Fall back to Genius for lyrics LRCLIB doesn't have.
    pub genius: bool,
    /// Where lyrics, covers and downloads are kept (None: don't keep them).
    pub library: Option<PathBuf>,
    pub save_lyrics: bool,
    pub save_art: bool,
    /// The user's SoundCloud session (`oauth_token` cookie), only used for
    /// official downloads.
    pub oauth_token: Option<String>,
    /// Per track id: the user's lyrics timing corrections.
    pub lyrics_sync: std::collections::BTreeMap<u64, crate::lyrics::LyricSync>,
    /// Most recently played first.
    pub history: Vec<Track>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            volume: 0.7,
            shuffle: false,
            repeat: Repeat::Off,
            radio: false,
            last_query: String::new(),
            terminal: None,
            notifications: true,
            genius: true,
            library: crate::library::default_dir(),
            save_lyrics: true,
            save_art: true,
            oauth_token: None,
            lyrics_sync: Default::default(),
            history: Vec::new(),
        }
    }
}

fn path() -> Option<PathBuf> {
    let config = dirs::config_dir()?;
    let path = config.join("driftwave").join("state.json");
    // Carry settings and history over from before the rename.
    let old = config.join("soundcloudcli").join("state.json");
    if !path.exists() && old.exists() && std::fs::create_dir_all(path.parent()?).is_ok() {
        let _ = std::fs::rename(&old, &path);
        let _ = std::fs::remove_dir(old.parent()?);
    }
    Some(path)
}

impl State {
    /// Load saved state; a missing or unreadable file just means defaults.
    pub fn load() -> Self {
        path()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let Some(path) = path() else { return };
        let Ok(json) = serde_json::to_vec_pretty(self) else { return };
        let _ = std::fs::create_dir_all(path.parent().unwrap());
        // Write then rename, so a crash mid-write can't corrupt the file.
        // Private (0600): it may hold the SoundCloud session token.
        let tmp = path.with_extension("json.tmp");
        let written = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .and_then(|mut f| f.write_all(&json));
        if written.is_ok() {
            let _ = std::fs::rename(tmp, path);
        }
    }

    pub fn record_play(&mut self, track: &Track) {
        self.history.retain(|t| t.id != track.id);
        self.history.insert(0, track.clone());
        self.history.truncate(HISTORY_LEN);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_history() {
        let track = |id: u64| -> Track {
            serde_json::from_value(serde_json::json!({
                "id": id, "title": "t", "duration": 1, "user": {"id": 1, "username": "u"}
            }))
            .unwrap()
        };
        let mut s = State { volume: 0.4, repeat: Repeat::One, ..State::default() };
        for id in [1, 2, 1, 3] {
            s.record_play(&track(id));
        }
        let ids: Vec<u64> = s.history.iter().map(|t| t.id).collect();
        assert_eq!(ids, [3, 1, 2], "newest first, no duplicates");

        let back: State = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back.volume, 0.4);
        assert_eq!(back.repeat, Repeat::One);
        assert_eq!(back.history.len(), 3);

        // Older/partial files still load, with defaults for missing fields.
        let partial: State = serde_json::from_str(r#"{"volume": 0.2}"#).unwrap();
        assert!(partial.notifications && partial.history.is_empty());
    }
}
