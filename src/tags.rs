//! Genre / tag browsing helpers: parsing SoundCloud tag lists, and deriving
//! related tags and artists from a genre's tracks.

use std::collections::HashMap;

use crate::api::{Track, User};
use crate::lyrics::normalize;

/// Parse a SoundCloud `tag_list`: space-separated, multi-word tags quoted
/// (`"folk punk" emo acoustic`). Machine tags (`soundcloud:source=…`) and
/// one-letter junk are dropped. Tags come back lowercase.
pub fn parse_tag_list(list: &str) -> Vec<String> {
    let mut tags = Vec::new();
    let mut rest = list.trim();
    while !rest.is_empty() {
        let (tag, after) = if let Some(quoted) = rest.strip_prefix('"') {
            match quoted.split_once('"') {
                Some((t, a)) => (t, a),
                None => (quoted, ""),
            }
        } else {
            rest.split_once(char::is_whitespace).unwrap_or((rest, ""))
        };
        let tag = tag.trim().to_lowercase();
        if tag.chars().count() >= 2 && !tag.contains([':', '=']) && !tag.chars().all(|c| c.is_ascii_digit()) {
            tags.push(tag);
        }
        rest = after.trim_start();
    }
    tags
}

/// All tags on a track: its genre plus its tag list, without duplicates.
fn track_tags(t: &Track) -> Vec<String> {
    let mut tags: Vec<String> = t.genre.iter().map(|g| g.trim().to_lowercase()).filter(|g| g.len() >= 2).collect();
    for tag in parse_tag_list(t.tag_list.as_deref().unwrap_or("")) {
        if !tags.iter().any(|x| normalize(x) == normalize(&tag)) {
            tags.push(tag);
        }
    }
    tags
}

/// Tags that show up alongside `tag` on these tracks, most common first,
/// with how many tracks carry each.
pub fn related(tracks: &[Track], tag: &str, max: usize) -> Vec<(String, usize)> {
    let own = normalize(tag);
    let mut counts: HashMap<String, (String, usize)> = HashMap::new();
    for t in tracks {
        for tag in track_tags(t) {
            let key = normalize(&tag);
            if key.is_empty() || key == own {
                continue;
            }
            counts.entry(key).or_insert((tag, 0)).1 += 1;
        }
    }
    let mut out: Vec<(String, usize)> = counts.into_values().filter(|(_, n)| *n >= 2).collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    out.truncate(max);
    out
}

/// The artists behind these tracks, most tracks first (then most followers).
pub fn artists(tracks: &[Track]) -> Vec<User> {
    let mut seen: HashMap<u64, (User, usize)> = HashMap::new();
    for t in tracks {
        seen.entry(t.user.id).or_insert((t.user.clone(), 0)).1 += 1;
    }
    let mut out: Vec<(User, usize)> = seen.into_values().collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then(b.0.followers_count.cmp(&a.0.followers_count)));
    out.into_iter().map(|(u, _)| u).collect()
}

/// Percent-encode a tag for use in a URL path.
pub fn encode_path(tag: &str) -> String {
    tag.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(id: u64, user: u64, genre: &str, tags: &str) -> Track {
        serde_json::from_value(serde_json::json!({
            "id": id, "title": "t", "duration": 1, "genre": genre, "tag_list": tags,
            "user": {"id": user, "username": format!("u{user}")}
        }))
        .unwrap()
    }

    #[test]
    fn tag_lists() {
        assert_eq!(
            parse_tag_list(r#""folk punk" Emo  acoustic soundcloud:source=web x 2024 "midwest emo""#),
            ["folk punk", "emo", "acoustic", "midwest emo"]
        );
        assert!(parse_tag_list("").is_empty());
    }

    #[test]
    fn related_tags_and_artists() {
        let tracks = [
            track(1, 10, "Nerdcore", r#"hackercore "video games""#),
            track(2, 10, "hackercore", "nerdcore chiptune"),
            track(3, 11, "Hip-hop & Rap", r#"nerdcore hackercore "video games""#),
        ];
        let rel = related(&tracks, "nerdcore", 10);
        assert_eq!(rel[0], ("hackercore".into(), 3));
        assert_eq!(rel[1], ("video games".into(), 2));
        assert!(rel.iter().all(|(t, _)| t != "nerdcore"), "the tag itself is excluded");
        assert!(rel.iter().all(|(_, n)| *n >= 2), "one-offs are dropped");
        let who: Vec<u64> = artists(&tracks).iter().map(|u| u.id).collect();
        assert_eq!(who, [10, 11]);
        assert_eq!(encode_path("folk punk/ä"), "folk%20punk%2F%C3%A4");
    }
}
