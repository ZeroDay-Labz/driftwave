//! Lyrics fallback from Genius, used when LRCLIB has nothing.
//!
//! Genius's API doesn't return lyrics, so this uses the search their website
//! uses and reads the lyrics out of the song page. That's against Genius's
//! terms and breaks whenever they change their markup, so it can be turned
//! off in Settings. The text is unsynced.

use std::sync::OnceLock;

use anyhow::{Context, Result};
use reqwest::blocking::Client;
use scraper::{ElementRef, Html, Node, Selector};
use serde::Deserialize;

use crate::lyrics::normalize;

fn http() -> &'static Client {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        Client::builder()
            .user_agent("Mozilla/5.0 (X11; Linux x86_64; rv:130.0) Gecko/20100101 Firefox/130.0")
            .build()
            .expect("HTTP client")
    })
}

#[derive(Deserialize)]
struct SearchResponse {
    response: Sections,
}

#[derive(Deserialize)]
struct Sections {
    sections: Vec<Section>,
}

#[derive(Deserialize)]
struct Section {
    #[serde(rename = "type")]
    kind: String,
    /// Shape depends on the section (songs, artists, videos…), so parse
    /// only the song ones.
    hits: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
struct Song {
    title: String,
    url: String,
    primary_artist: Artist,
    #[serde(default)]
    instrumental: bool,
}

#[derive(Deserialize)]
struct Artist {
    name: String,
}

pub struct GeniusResult {
    /// The song's canonical artist and title on Genius (cleaner than the
    /// SoundCloud upload's, so good for retrying other lookups).
    pub artist: String,
    pub title: String,
    /// `None` for instrumentals.
    pub text: Option<String>,
}

pub fn find(artist: &str, title: &str) -> Result<Option<GeniusResult>> {
    let resp: SearchResponse = http()
        .get("https://genius.com/api/search/multi")
        .query(&[("q", format!("{artist} {title}"))])
        .send()?
        .error_for_status()?
        .json()?;
    let songs: Vec<Song> = resp
        .response
        .sections
        .into_iter()
        .filter(|s| s.kind == "song")
        .flat_map(|s| s.hits)
        .filter_map(|mut h| serde_json::from_value(h.get_mut("result")?.take()).ok())
        .collect();

    let (want_title, want_artist) = (normalize(title), normalize(artist));
    let matches = |a: &str, b: &str| !a.is_empty() && !b.is_empty() && (a.contains(b) || b.contains(a));
    let Some(song) = songs.into_iter().enumerate().find_map(|(i, s)| {
        let title_ok = matches(&normalize(&s.title), &want_title);
        let artist_ok = matches(&normalize(&s.primary_artist.name), &want_artist);
        // The top hit may be credited differently ("deadmau5 & Lights" vs
        // "deadmau5"); further down, require the artist to match too.
        (title_ok && (artist_ok || i == 0)).then_some(s)
    }) else {
        return Ok(None);
    };

    let text = if song.instrumental {
        None
    } else {
        let page = http().get(&song.url).send()?.error_for_status()?.text()?;
        Some(extract_lyrics(&page).context("no lyrics on the Genius page")?)
    };
    Ok(Some(GeniusResult { artist: song.primary_artist.name, title: song.title, text }))
}

/// Pull the lyrics text out of a Genius song page.
pub fn extract_lyrics(html: &str) -> Option<String> {
    let doc = Html::parse_document(html);
    let containers = Selector::parse(r#"div[data-lyrics-container="true"]"#).ok()?;
    let mut out = String::new();
    for container in doc.select(&containers) {
        collect_text(container, &mut out);
        out.push('\n');
    }
    // Tidy: trim lines, and at most one blank line in a row.
    let mut lines: Vec<&str> = Vec::new();
    for line in out.lines().map(str::trim) {
        if line.is_empty() && lines.last().is_none_or(|l| l.is_empty()) {
            continue;
        }
        lines.push(line);
    }
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    (!lines.is_empty()).then(|| lines.join("\n"))
}

fn collect_text(el: ElementRef, out: &mut String) {
    for child in el.children() {
        match child.value() {
            Node::Text(t) => out.push_str(t),
            Node::Element(e) => {
                if e.attr("data-exclude-from-selection").is_some() {
                    continue; // contributor header, "Embed" button, etc.
                }
                if e.name() == "br" {
                    out.push('\n');
                } else if let Some(child_el) = ElementRef::wrap(child) {
                    collect_text(child_el, out);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_lyrics_from_page_markup() {
        // Same structure as a real Genius page (not real lyrics).
        let html = r#"<html><body>
            <div data-lyrics-container="true" class="Lyrics__Container">
              <div data-exclude-from-selection="true"><span>15 Contributors</span>Song Lyrics</div>
              [Verse 1]<br>First line &amp; more<br><a href="/x"><span>Linked line</span></a><br><br><br>
            </div>
            <div class="ad">advert</div>
            <div data-lyrics-container="true">[Chorus]<br><i>Second</i> part</div>
            </body></html>"#;
        assert_eq!(
            extract_lyrics(html).unwrap(),
            "[Verse 1]\nFirst line & more\nLinked line\n\n[Chorus]\nSecond part"
        );
        assert!(extract_lyrics("<html><body>nothing</body></html>").is_none());
    }

    /// Hits genius.com: `cargo test genius -- --ignored`
    #[test]
    #[ignore]
    fn finds_lyrics_live() {
        let g = find("deadmau5", "Ghosts n Stuff").unwrap().expect("a match");
        let text = g.text.expect("lyrics");
        println!("{} – {}: {} lines", g.artist, g.title, text.lines().count());
        assert!(text.lines().count() > 5);
    }
}
