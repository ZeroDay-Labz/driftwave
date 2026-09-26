//! "Now playing" desktop notifications.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use notify_rust::{Hint, Notification, Timeout, Urgency};

use crate::api::Track;

/// Shows one notification per track, each replacing the previous one.
#[derive(Default)]
pub struct Notifier {
    /// Id of the last notification, so the next can replace it.
    last_id: Arc<AtomicU32>,
}

impl Notifier {
    pub fn show(&self, track: &Track, cover: Option<&Path>) {
        let mut n = Notification::new();
        n.appname("driftwave")
            .summary(&track.title)
            .body(track.publisher_artist().unwrap_or(&track.user.username))
            .icon(&cover.map_or("audio-x-generic".into(), |p| p.display().to_string()))
            .hint(Hint::Transient(true)) // don't pile up in the history
            .hint(Hint::Category("x-driftwave.track".into()))
            .urgency(Urgency::Low)
            .timeout(Timeout::Milliseconds(4000));
        let last = self.last_id.load(Ordering::Acquire);
        if last != 0 {
            n.id(last);
        }
        // Talking to the notification daemon can take a moment; don't block the UI.
        let last_id = self.last_id.clone();
        std::thread::spawn(move || {
            if let Ok(handle) = n.show() {
                last_id.store(handle.id(), Ordering::Release);
            }
        });
    }
}
