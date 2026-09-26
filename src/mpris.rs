//! MPRIS (D-Bus media player interface): media keys, headset buttons,
//! KDE's media widget / lock screen, and `playerctl` all control us.

use std::path::Path;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use souvlaki::{MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig};

use crate::api::Track;

pub use souvlaki::{MediaControlEvent as Event, SeekDirection};

pub struct Mpris {
    controls: MediaControls,
    events: Receiver<MediaControlEvent>,
    /// (track id, has cover art) last published.
    shown: Option<(u64, bool)>,
    paused: Option<bool>,
    last_progress: Instant,
}

impl Mpris {
    /// `None` if there's no session bus (e.g. over plain SSH).
    pub fn new() -> Option<Self> {
        if std::env::var_os("DRIFTWAVE_NO_MPRIS").is_some() {
            return None;
        }
        let config = PlatformConfig { display_name: "driftwave", dbus_name: "driftwave", hwnd: None };
        let mut controls = MediaControls::new(config).ok()?;
        let (tx, events) = mpsc::channel();
        controls.attach(move |e| drop(tx.send(e))).ok()?;
        Some(Self { controls, events, shown: None, paused: None, last_progress: Instant::now() })
    }

    pub fn events(&self) -> Vec<MediaControlEvent> {
        self.events.try_iter().collect()
    }

    /// Publish what's playing. Cheap to call every frame.
    pub fn update(&mut self, track: Option<&Track>, art: Option<&Path>, paused: bool, pos: Duration) {
        let key = track.map(|t| (t.id, art.is_some()));
        if key != self.shown {
            self.shown = key;
            let cover = art.map(|p| format!("file://{}", p.display()));
            let _ = self.controls.set_metadata(match track {
                Some(t) => MediaMetadata {
                    title: Some(&t.title),
                    artist: Some(t.publisher_artist().unwrap_or(&t.user.username)),
                    album: None,
                    cover_url: cover.as_deref(),
                    duration: Some(Duration::from_millis(t.duration)),
                },
                None => MediaMetadata::default(),
            });
            self.paused = None; // force a playback update below
        }
        let progress_due = self.last_progress.elapsed() > Duration::from_secs(2);
        if self.paused != Some(paused) || progress_due {
            self.paused = Some(paused);
            self.last_progress = Instant::now();
            let progress = Some(MediaPosition(pos));
            let _ = self.controls.set_playback(match track {
                None => MediaPlayback::Stopped,
                Some(_) if paused => MediaPlayback::Paused { progress },
                Some(_) => MediaPlayback::Playing { progress },
            });
        }
    }
}
