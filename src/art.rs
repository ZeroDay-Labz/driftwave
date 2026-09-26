//! Cover art: downloaded once per track, cached on disk (also used for the
//! desktop notification and media widget), and drawn with whatever image
//! protocol the terminal supports (kitty graphics, sixel, or half-blocks).

use std::collections::HashMap;
use std::path::PathBuf;

use image::DynamicImage;
use ratatui::Frame;
use ratatui::layout::{Rect, Size};
use ratatui_image::picker::Picker;
use ratatui_image::picker::cap_parser::QueryStdioOptions;
use ratatui_image::protocol::Protocol;
use ratatui_image::{Image, Resize};

pub struct Art {
    picker: Picker,
    images: HashMap<u64, DynamicImage>,
    /// The encoded image for the last (track, size) drawn.
    encoded: Option<(u64, Size, Protocol)>,
}

impl Art {
    /// Must be called after entering the alternate screen and before
    /// reading terminal events.
    pub fn new() -> Self {
        // Graphics-capable terminals answer in milliseconds; don't make
        // everyone else wait for the default 2 s timeout.
        let options = QueryStdioOptions { timeout_ms: 300, ..QueryStdioOptions::default() };
        let picker = Picker::from_query_stdio_with_options(options).unwrap_or_else(|_| Picker::halfblocks());
        Self { picker, images: HashMap::new(), encoded: None }
    }

    pub fn insert(&mut self, track: u64, image: DynamicImage) {
        self.images.insert(track, image);
    }

    pub fn has(&self, track: u64) -> bool {
        self.images.contains_key(&track)
    }

    /// Draw the track's cover into `area` (if we have it).
    pub fn draw(&mut self, frame: &mut Frame, track: u64, area: Rect) {
        let size = area.as_size();
        let stale = !matches!(&self.encoded, Some((t, s, _)) if *t == track && *s == size);
        if stale {
            let Some(img) = self.images.get(&track) else { return };
            self.encoded = self
                .picker
                .new_protocol(img.clone(), size, Resize::Fit(None))
                .ok()
                .map(|p| (track, size, p));
        }
        if let Some((_, _, protocol)) = &self.encoded {
            frame.render_widget(Image::new(protocol), area);
        }
    }
}

/// Where a track's cover is cached.
pub fn cache_path(track: u64) -> Option<PathBuf> {
    Some(dirs::cache_dir()?.join("driftwave").join("art").join(format!("{track}.jpg")))
}
