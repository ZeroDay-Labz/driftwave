//! Copy text to the system clipboard: wl-copy (Wayland), xsel/xclip (X11),
//! or the terminal's OSC 52 escape as a last resort (works over SSH and
//! inside Flatpak with kitty and most modern terminals).

use std::io::Write;
use std::process::{Command, Stdio};

/// Returns how it was copied, for the status line.
pub fn copy(text: &str) -> &'static str {
    let tools: &[(&str, &[&str], &str)] = &[
        ("wl-copy", &[], "WAYLAND_DISPLAY"),
        ("xsel", &["--clipboard", "--input"], "DISPLAY"),
        ("xclip", &["-selection", "clipboard"], "DISPLAY"),
    ];
    for (tool, args, needs) in tools {
        if std::env::var_os(needs).is_none() {
            continue;
        }
        let child = Command::new(tool)
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        if let Ok(mut child) = child {
            let wrote = child.stdin.take().is_some_and(|mut s| s.write_all(text.as_bytes()).is_ok());
            if wrote && child.wait().is_ok_and(|s| s.success()) {
                return tool;
            }
        }
    }
    // OSC 52: ask the terminal itself to set the clipboard.
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b]52;c;{}\x07", base64(text.as_bytes()));
    let _ = out.flush();
    "terminal"
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn base64_matches_reference() {
        assert_eq!(super::base64(b""), "");
        assert_eq!(super::base64(b"f"), "Zg==");
        assert_eq!(super::base64(b"fo"), "Zm8=");
        assert_eq!(super::base64(b"foo"), "Zm9v");
        assert_eq!(super::base64("héllo ♪".as_bytes()), "aMOpbGxvIOKZqg==");
    }
}
