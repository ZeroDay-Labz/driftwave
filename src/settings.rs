//! The Settings screen: which rows exist, how they display, how they edit.

use crate::state::State;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Field {
    Library,
    SaveLyrics,
    SaveArt,
    Genius,
    Notifications,
    Terminal,
    Token,
    OpenLibrary,
}

pub const FIELDS: [Field; 8] = [
    Field::Library,
    Field::SaveLyrics,
    Field::SaveArt,
    Field::Genius,
    Field::Notifications,
    Field::Terminal,
    Field::Token,
    Field::OpenLibrary,
];

pub enum Value {
    Toggle(bool),
    Text(String),
    Action,
}

/// Cycle for the lyrics-window terminal ("custom" then asks for a command).
const TERMINALS: [Option<&str>; 4] = [None, Some("kitty"), Some("konsole"), Some("xterm")];

impl Field {
    pub fn label(self) -> &'static str {
        match self {
            Field::Library => "Library folder",
            Field::SaveLyrics => "Save lyrics to library",
            Field::SaveArt => "Save cover art to library",
            Field::Genius => "Genius lyrics fallback",
            Field::Notifications => "Track notifications",
            Field::Terminal => "Lyrics window terminal",
            Field::Token => "SoundCloud session (downloads)",
            Field::OpenLibrary => "Open library folder",
        }
    }

    pub fn help(self) -> &'static str {
        match self {
            Field::Library => {
                "Lyrics/, Covers/ and Downloads/ live here. Your own .lrc/.txt files in Lyrics/ \
                 (named \"Artist - Title\") are used before any online lookup. Empty turns it off."
            }
            Field::SaveLyrics => "Keep every lyric found online as a .lrc (synced) or .txt file.",
            Field::SaveArt => "Keep each track's cover as a .jpg.",
            Field::Genius => {
                "When LRCLIB has nothing, read the lyrics from the song's Genius page \
                 (unsynced; unofficial, may break if Genius changes)."
            }
            Field::Notifications => "A desktop notification with the cover when a track starts.",
            Field::Terminal => "Which terminal the ⧉ pop-out opens in. Auto tries kitty, konsole, xterm.",
            Field::Token => {
                "Only for official downloads (tracks whose uploader enabled them). Sign in on \
                 soundcloud.com, open DevTools → Storage → Cookies, copy \"oauth_token\". \
                 Stored privately in state.json. Empty clears it."
            }
            Field::OpenLibrary => "Open the library folder in your file manager.",
        }
    }

    pub fn value(self, st: &State) -> Value {
        match self {
            Field::Library => Value::Text(st.library.as_ref().map_or("(off)".into(), |p| tilde(&p.display().to_string()))),
            Field::SaveLyrics => Value::Toggle(st.save_lyrics),
            Field::SaveArt => Value::Toggle(st.save_art),
            Field::Genius => Value::Toggle(st.genius),
            Field::Notifications => Value::Toggle(st.notifications),
            Field::Terminal => Value::Text(st.terminal.clone().unwrap_or_else(|| "auto".into())),
            Field::Token => Value::Text(match &st.oauth_token {
                Some(t) if t.len() > 4 => format!("set (…{})", &t[t.len() - 4..]),
                Some(_) => "set".into(),
                None => "not set".into(),
            }),
            Field::OpenLibrary => Value::Action,
        }
    }

    /// Text fields start editing with this; `None` means Enter acts directly.
    pub fn edit_text(self, st: &State) -> Option<String> {
        match self {
            Field::Library => Some(st.library.as_ref().map_or(String::new(), |p| tilde(&p.display().to_string()))),
            Field::Token => Some(String::new()), // never show the secret
            _ => None,
        }
    }
}

/// Toggle or cycle a non-text field. Returns true if the terminal cycle
/// reached "custom" (the caller should start editing it).
pub fn toggle(field: Field, st: &mut State) -> bool {
    match field {
        Field::SaveLyrics => st.save_lyrics = !st.save_lyrics,
        Field::SaveArt => st.save_art = !st.save_art,
        Field::Genius => st.genius = !st.genius,
        Field::Notifications => st.notifications = !st.notifications,
        Field::Terminal => {
            let pos = TERMINALS.iter().position(|t| *t == st.terminal.as_deref());
            match pos {
                Some(i) if i + 1 < TERMINALS.len() => st.terminal = TERMINALS[i + 1].map(String::from),
                Some(_) => return true, // after xterm: custom
                None => st.terminal = None, // was custom: back to auto
            }
        }
        _ => {}
    }
    false
}

/// Apply an edited text value. Returns a message for the status line.
pub fn commit(field: Field, text: &str, st: &mut State) -> Result<String, String> {
    let text = text.trim();
    match field {
        Field::Library if text.is_empty() => {
            st.library = None;
            Ok("Library turned off".into())
        }
        Field::Library => {
            let path = crate::library::expand(text);
            std::fs::create_dir_all(&path).map_err(|e| format!("Can't use {}: {e}", path.display()))?;
            let probe = path.join(".driftwave-write-test");
            std::fs::write(&probe, b"").map_err(|e| format!("{} isn't writable: {e}", path.display()))?;
            let _ = std::fs::remove_file(probe);
            let msg = format!("Library: {}", path.display());
            st.library = Some(path);
            Ok(msg)
        }
        Field::Token if text.is_empty() => {
            st.oauth_token = None;
            Ok("Session token cleared".into())
        }
        Field::Token => {
            // Accept the value as copied in various ways.
            let t = text.trim_start_matches("oauth_token=").trim_start_matches("OAuth ").trim_matches('"');
            if t.len() < 10 || t.contains(char::is_whitespace) {
                return Err("That doesn't look like an oauth_token value".into());
            }
            st.oauth_token = Some(t.to_string());
            Ok("Session token saved".into())
        }
        Field::Terminal => {
            st.terminal = (!text.is_empty()).then(|| text.to_string());
            Ok(format!("Lyrics window terminal: {}", st.terminal.as_deref().unwrap_or("auto")))
        }
        _ => Ok(String::new()),
    }
}

fn tilde(path: &str) -> String {
    match dirs::home_dir().map(|h| h.display().to_string()) {
        Some(home) if path.starts_with(&home) => format!("~{}", &path[home.len()..]),
        _ => path.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editing_settings() {
        let mut st = State::default();
        let dir = std::env::temp_dir().join(format!("driftwave-lib-{}", std::process::id()));
        commit(Field::Library, dir.to_str().unwrap(), &mut st).unwrap();
        assert_eq!(st.library.as_deref(), Some(dir.as_path()));
        assert!(dir.is_dir());
        commit(Field::Library, "  ", &mut st).unwrap();
        assert!(st.library.is_none());
        std::fs::remove_dir_all(&dir).unwrap();

        assert!(commit(Field::Token, "nope", &mut st).is_err());
        commit(Field::Token, "oauth_token=2-123456-78901234-AbCdEfGhIjKl", &mut st).unwrap();
        assert_eq!(st.oauth_token.as_deref(), Some("2-123456-78901234-AbCdEfGhIjKl"));
        assert!(matches!(Field::Token.value(&st), Value::Text(t) if t == "set (…IjKl)"));

        // auto → kitty → konsole → xterm → custom
        assert!(!toggle(Field::Terminal, &mut st));
        assert_eq!(st.terminal.as_deref(), Some("kitty"));
        toggle(Field::Terminal, &mut st);
        toggle(Field::Terminal, &mut st);
        assert!(toggle(Field::Terminal, &mut st), "asks for a custom command");
        commit(Field::Terminal, "alacritty", &mut st).unwrap();
        toggle(Field::Terminal, &mut st);
        assert_eq!(st.terminal, None, "custom cycles back to auto");
    }
}
