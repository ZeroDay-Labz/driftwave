//! Player ↔ lyrics-window link: newline-delimited JSON over a Unix socket.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::lyrics::LyricsState;

#[derive(Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ToWindow {
    Track {
        title: String,
        artist: String,
        lyrics: LyricsState,
        #[serde(default)]
        duration_ms: u64,
        #[serde(default)]
        sync: crate::lyrics::LyricSync,
    },
    NoTrack,
    Pos { ms: u64, paused: bool },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ToPlayer {
    Seek { ms: u64 },
    /// Shift the lyrics timing by this much.
    Nudge { ms: i64 },
    /// "This line is being sung right now."
    Pin { line: usize },
}

pub struct Server {
    path: PathBuf,
    clients: Arc<Mutex<Vec<UnixStream>>>,
    new_client: Arc<AtomicBool>,
    incoming: Receiver<ToPlayer>,
}

impl Server {
    pub fn start() -> Result<Self> {
        let mut dir = dirs::runtime_dir().unwrap_or_else(std::env::temp_dir);
        if let Some(id) = crate::lyrics_window::flatpak_id() {
            // The one part of the runtime dir shared by every instance of
            // the app (the lyrics window is started with `flatpak run`).
            dir = dir.join("app").join(id);
            std::fs::create_dir_all(&dir)?;
        }
        let path = dir.join(format!("driftwave-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path)?;
        let clients: Arc<Mutex<Vec<UnixStream>>> = Arc::default();
        let new_client = Arc::new(AtomicBool::new(false));
        let (tx, incoming) = mpsc::channel();

        let (c, n) = (clients.clone(), new_client.clone());
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                // Never let a stuck window stall the player's UI thread.
                let _ = stream.set_write_timeout(Some(Duration::from_millis(50)));
                let Ok(reader) = stream.try_clone() else { continue };
                let tx = tx.clone();
                thread::spawn(move || {
                    for line in BufReader::new(reader).lines().map_while(Result::ok) {
                        if let Ok(msg) = serde_json::from_str::<ToPlayer>(&line)
                            && tx.send(msg).is_err()
                        {
                            break;
                        }
                    }
                });
                c.lock().unwrap().push(stream);
                n.store(true, Ordering::Release);
            }
        });
        Ok(Self { path, clients, new_client, incoming })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn has_clients(&self) -> bool {
        !self.clients.lock().unwrap().is_empty()
    }

    /// True once after a window connects (so it can be sent the full state).
    pub fn take_new_client(&self) -> bool {
        self.new_client.swap(false, Ordering::AcqRel)
    }

    pub fn send(&self, msg: &ToWindow) {
        let Ok(mut line) = serde_json::to_vec(msg) else { return };
        line.push(b'\n');
        // Drop windows that have closed.
        self.clients.lock().unwrap().retain_mut(|c| c.write_all(&line).is_ok());
    }

    pub fn poll(&self) -> Vec<ToPlayer> {
        self.incoming.try_iter().collect()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip() {
        let msg = ToWindow::Pos { ms: 1234, paused: true };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(json, r#"{"type":"Pos","ms":1234,"paused":true}"#);
        let back: ToPlayer = serde_json::from_str(r#"{"type":"Seek","ms":42}"#).unwrap();
        assert!(matches!(back, ToPlayer::Seek { ms: 42 }));
    }
}
