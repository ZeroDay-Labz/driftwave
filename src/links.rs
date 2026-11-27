//! Recognize SoundCloud links in whatever form people paste them, and build
//! links to share.

/// A SoundCloud link, cleaned up for `/resolve`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub url: String,
    /// Where to start playing (`#t=1:23`), in milliseconds.
    pub start_ms: Option<u64>,
}

impl Link {
    /// A short link from the app's Share menu: needs a redirect to find
    /// the real page.
    pub fn is_short(&self) -> bool {
        self.url.starts_with("https://on.soundcloud.com/")
    }
}

/// Find a SoundCloud link in `input`: a bare link, or one inside pasted text
/// ("Listen to … on #SoundCloud https://on.soundcloud.com/…"). Accepts
/// http(s) or no scheme, `www.`/`m.` hosts, `on.soundcloud.com` short links,
/// `w.soundcloud.com/player` embeds and `api.soundcloud.com` URLs. Tracking
/// parameters are dropped; a `t=` timestamp is kept as `start_ms`.
pub fn parse(input: &str) -> Option<Link> {
    input.split_whitespace().find_map(|word| {
        let word = word.trim_matches(|c: char| "<>()[]{}\"'`,;".contains(c));
        parse_one(word)
    })
}

fn parse_one(word: &str) -> Option<Link> {
    let lower = word.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"))
        .map(|r| &word[word.len() - r.len()..])
        .unwrap_or(word);
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let host = host.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    let host = match host {
        "soundcloud.com" | "m.soundcloud.com" => "soundcloud.com",
        "on.soundcloud.com" | "api.soundcloud.com" => host,
        "w.soundcloud.com" => return parse_embed(path),
        _ => return None,
    };

    let (path, fragment) = path.split_once('#').unwrap_or((path, ""));
    let (path, query) = path.split_once('?').unwrap_or((path, ""));
    let path = path.trim_end_matches('/');
    if path.is_empty() {
        return None; // the homepage isn't something to play
    }

    let mut start_ms = None;
    let mut keep = Vec::new();
    for param in query.split('&').chain(fragment.split('&')) {
        let (k, v) = param.split_once('=').unwrap_or((param, ""));
        match k {
            "t" => start_ms = parse_time(v).or(start_ms),
            // Old-style private links carry their key here.
            "secret_token" if !v.is_empty() => keep.push(format!("secret_token={v}")),
            _ => {} // si, utm_*, ref, in, …
        }
    }
    let mut url = format!("https://{host}/{path}");
    if !keep.is_empty() {
        url = format!("{url}?{}", keep.join("&"));
    }
    Some(Link { url, start_ms })
}

/// `w.soundcloud.com/player/?url=<encoded link>&…`
fn parse_embed(path: &str) -> Option<Link> {
    let query = path.split_once('?')?.1;
    let inner = query.split('&').find_map(|p| p.strip_prefix("url="))?;
    parse_one(&percent_decode(inner))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = || std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok().and_then(|h| u8::from_str_radix(h, 16).ok());
        match bytes[i] {
            b'%' if hex().is_some() => {
                out.push(hex().unwrap());
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// "90", "1:30", "1:02:03", "1m30s", "1h2m3s", "30s" → milliseconds.
pub fn parse_time(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let secs = if s.contains(':') {
        let parts: Vec<u64> = s.split(':').map(|p| p.parse().ok()).collect::<Option<_>>()?;
        if parts.len() > 3 {
            return None;
        }
        parts.iter().fold(0, |acc, p| acc * 60 + p)
    } else if s.chars().all(|c| c.is_ascii_digit()) {
        s.parse().ok()?
    } else {
        let (mut total, mut num) = (0u64, String::new());
        for c in s.chars() {
            match c {
                '0'..='9' => num.push(c),
                'h' | 'm' | 's' => {
                    let n: u64 = num.parse().ok()?;
                    total += n * match c {
                        'h' => 3600,
                        'm' => 60,
                        _ => 1,
                    };
                    num.clear();
                }
                _ => return None,
            }
        }
        if !num.is_empty() {
            return None;
        }
        total
    };
    Some(secs * 1000)
}

/// "1:05" or "1:02:03", as SoundCloud writes `#t=`.
pub fn format_time(ms: u64) -> String {
    let s = ms / 1000;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// A link to share, starting at `at_ms` if given.
pub fn share_url(permalink: &str, at_ms: Option<u64>) -> String {
    match at_ms {
        Some(ms) if ms >= 1000 => format!("{permalink}#t={}", format_time(ms)),
        _ => permalink.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Option<String> {
        parse(s).map(|l| l.url)
    }

    #[test]
    fn recognizes_link_forms() {
        let want = Some("https://soundcloud.com/artist/track".to_string());
        assert_eq!(url("https://soundcloud.com/artist/track"), want);
        assert_eq!(url("http://soundcloud.com/artist/track"), want);
        assert_eq!(url("soundcloud.com/artist/track"), want);
        assert_eq!(url("www.soundcloud.com/artist/track/"), want);
        assert_eq!(url("https://m.soundcloud.com/artist/track"), want);
        assert_eq!(url("HTTPS://SoundCloud.com/artist/track"), want);
        assert_eq!(
            url("https://soundcloud.com/artist/track?si=abc123&utm_source=clipboard&utm_medium=text&utm_campaign=social_sharing"),
            want
        );
        assert_eq!(url("https://soundcloud.com/artist/track?in=artist/sets/album"), want);
        assert_eq!(url("<https://soundcloud.com/artist/track>"), want);
        assert_eq!(
            url("https://w.soundcloud.com/player/?url=https%3A%2F%2Fsoundcloud.com%2Fartist%2Ftrack&color=%23ff5500&auto_play=false"),
            want
        );
        assert_eq!(url("https://soundcloud.com/artist"), Some("https://soundcloud.com/artist".into()));
        assert_eq!(url("https://soundcloud.com/artist/sets/album"), Some("https://soundcloud.com/artist/sets/album".into()));
    }

    #[test]
    fn keeps_private_and_short_links() {
        assert_eq!(url("https://soundcloud.com/artist/track/s-AbC123?si=x"), Some("https://soundcloud.com/artist/track/s-AbC123".into()));
        assert_eq!(
            url("https://soundcloud.com/artist/track?secret_token=s-XyZ&utm_source=x"),
            Some("https://soundcloud.com/artist/track?secret_token=s-XyZ".into())
        );
        let short = parse("https://on.soundcloud.com/AbCdEf123").unwrap();
        assert!(short.is_short());
        assert_eq!(short.url, "https://on.soundcloud.com/AbCdEf123");
        assert_eq!(url("https://api.soundcloud.com/tracks/123"), Some("https://api.soundcloud.com/tracks/123".into()));
    }

    #[test]
    fn finds_link_inside_shared_text() {
        let text = "Listen to Psycho Dreams by ENCASSATOR on #SoundCloud\nhttps://on.soundcloud.com/AbC123";
        assert_eq!(url(text), Some("https://on.soundcloud.com/AbC123".into()));
    }

    #[test]
    fn rejects_non_links() {
        for s in ["daft punk", "#folk punk", "https://soundcloud.com", "https://soundcloud.com/", "https://example.com/x", "notsoundcloud.com/a/b"] {
            assert_eq!(parse(s), None, "{s}");
        }
    }

    #[test]
    fn timestamps() {
        let at = |s: &str| parse(s).and_then(|l| l.start_ms);
        assert_eq!(at("https://soundcloud.com/a/b#t=1:30"), Some(90_000));
        assert_eq!(at("https://soundcloud.com/a/b#t=90"), Some(90_000));
        assert_eq!(at("https://soundcloud.com/a/b?t=1m30s"), Some(90_000));
        assert_eq!(at("https://soundcloud.com/a/b?si=x#t=1:02:03"), Some(3_723_000));
        assert_eq!(at("https://soundcloud.com/a/b#t=junk"), None);
        assert_eq!(at("https://soundcloud.com/a/b"), None);
        assert_eq!(parse_time("1h2m3s"), Some(3_723_000));
        assert_eq!(parse_time("45s"), Some(45_000));
        assert_eq!(parse_time("1:2:3:4"), None);
    }

    #[test]
    fn share_links() {
        assert_eq!(format_time(65_400), "1:05");
        assert_eq!(format_time(3_723_000), "1:02:03");
        assert_eq!(share_url("https://soundcloud.com/a/b", Some(65_400)), "https://soundcloud.com/a/b#t=1:05");
        assert_eq!(share_url("https://soundcloud.com/a/b", Some(400)), "https://soundcloud.com/a/b");
        assert_eq!(share_url("https://soundcloud.com/a/b", None), "https://soundcloud.com/a/b");
        // A shared link round-trips.
        let link = parse(&share_url("https://soundcloud.com/a/b", Some(83_000))).unwrap();
        assert_eq!(link, Link { url: "https://soundcloud.com/a/b".into(), start_ms: Some(83_000) });
    }
}
