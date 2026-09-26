//! The pop-out lyrics window: a second copy of the program, started in a new
//! terminal with `--lyrics-window <socket>`, that follows the player.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, MouseButton, MouseEventKind,
};
use ratatui::layout::Alignment;
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph};

use crate::ipc::{ToPlayer, ToWindow};
use crate::lyrics::LyricsState;
use crate::ui::{LyricsHits, LyricsScroll, LyricsView, render_lyrics};

pub const WINDOW_TITLE: &str = "driftwave lyrics";

/// Open a terminal running the lyrics window. Returns the terminal used.
/// Flatpak app id, when running inside the Flatpak sandbox.
pub fn flatpak_id() -> Option<String> {
    std::env::var("FLATPAK_ID").ok().filter(|id| !id.is_empty())
}

/// Open a terminal running the lyrics window. Returns the terminal used.
pub fn launch(socket: &Path, preferred: Option<&str>) -> Result<String> {
    let flatpak = flatpak_id();
    // What the terminal should run: this program again, in lyrics mode.
    let mut inner: Vec<std::ffi::OsString> = match &flatpak {
        Some(id) => ["flatpak", "run", "--command=driftwave", id.as_str()].map(Into::into).to_vec(),
        None => vec![std::env::current_exe()?.into()],
    };
    inner.extend(["--lyrics-window".into(), socket.as_os_str().to_owned()]);

    let env_choice = std::env::var("DRIFTWAVE_TERMINAL").ok();
    let candidates = env_choice
        .as_deref()
        .or(preferred)
        .into_iter()
        .chain(["kitty", "konsole", "xterm"]);
    for term in candidates {
        // Inside Flatpak, the terminal lives on the host.
        let mut cmd = if flatpak.is_some() {
            if !host_has(term) {
                continue;
            }
            let mut c = Command::new("flatpak-spawn");
            c.args(["--host", term]);
            c
        } else {
            let Some(path) = which(term) else { continue };
            Command::new(path)
        };
        let name = Path::new(term).file_name().and_then(|n| n.to_str()).unwrap_or(term);
        match name {
            "kitty" => cmd.args([
                "--title",
                WINDOW_TITLE,
                "-o",
                "initial_window_width=64c",
                "-o",
                "initial_window_height=34c",
            ]),
            "konsole" => cmd.args(["--separate", "--hide-menubar", "--hide-tabbar", "-e"]),
            "xterm" => cmd.args(["-T", WINDOW_TITLE, "-geometry", "64x34", "-e"]),
            _ => cmd.arg("-e"),
        };
        cmd.args(&inner)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0); // don't die with the player's terminal
        cmd.spawn()?;
        return Ok(name.to_string());
    }
    bail!("no terminal found (set DRIFTWAVE_TERMINAL, e.g. to kitty)")
}

/// Is `program` installed on the host (from inside Flatpak)?
fn host_has(program: &str) -> bool {
    Command::new("flatpak-spawn")
        .args(["--host", "sh", "-c", "command -v \"$1\" >/dev/null", "sh", program])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn which(program: &str) -> Option<std::path::PathBuf> {
    if program.contains('/') {
        return Some(program.into());
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(program))
        .find(|p| p.is_file())
}

enum Incoming {
    Msg(ToWindow),
    Closed,
}

/// Cut `s` to at most `max` characters, ending in "…" if it was cut.
fn shorten(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max.saturating_sub(1)).chain(['…']).collect()
    }
}

fn send(writer: &mut UnixStream, msg: &ToPlayer) {
    if let Ok(mut line) = serde_json::to_vec(msg) {
        line.push(b'\n');
        let _ = writer.write_all(&line);
    }
}

fn copy_lyrics(track: &Option<(String, String, LyricsState)>) -> String {
    use crate::lyrics::Lyrics;
    let text = match track {
        Some((_, _, LyricsState::Ready(Some(found)))) => match &found.lyrics {
            Lyrics::Plain(t) => Some(t.clone()),
            Lyrics::Synced(lines) => Some(lines.iter().map(|(_, l)| l.as_str()).collect::<Vec<_>>().join("\n")),
            Lyrics::Instrumental => None,
        },
        _ => None,
    };
    match text {
        Some(t) => format!("copied {} lines ({})", t.lines().count(), crate::clipboard::copy(&t)),
        None => "no lyrics to copy".into(),
    }
}

/// Entry point for `--lyrics-window <socket>`.
pub fn run(socket: &str) -> Result<()> {
    let stream = UnixStream::connect(socket)?;
    let mut writer = stream.try_clone()?;
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stream).lines().map_while(Result::ok) {
            if let Ok(msg) = serde_json::from_str(&line)
                && tx.send(Incoming::Msg(msg)).is_err()
            {
                return;
            }
        }
        let _ = tx.send(Incoming::Closed);
    });

    // Set the terminal title too, for window rules (kitty's --title covers
    // kitty; this covers the rest).
    print!("\x1b]2;{WINDOW_TITLE}\x07");
    let mut terminal = ratatui::init();
    crossterm::execute!(std::io::stdout(), EnableMouseCapture)?;
    let result = (|| -> Result<()> {
        let mut track: Option<(String, String, LyricsState)> = None;
        let mut duration_ms = 0u64;
        let mut sync = crate::lyrics::LyricSync::default();
        // A short confirmation shown in the footer ("copied", …).
        let mut flash: Option<(String, Instant)> = None;
        let mut copy_button = ratatui::layout::Rect::default();
        let (mut pos_ms, mut pos_at, mut paused) = (0u64, Instant::now(), true);
        let mut closed_at: Option<Instant> = None;
        let mut scroll = LyricsScroll::default();
        let mut hits = LyricsHits::default();
        let mut frame_no = 0u64;
        loop {
            for msg in rx.try_iter() {
                match msg {
                    Incoming::Msg(ToWindow::Track { title, artist, lyrics, duration_ms: d, sync: s }) => {
                        sync = s;
                        if track.as_ref().is_none_or(|(t, a, _)| *t != title || *a != artist) {
                            scroll = LyricsScroll::default();
                        }
                        track = Some((title, artist, lyrics));
                        duration_ms = d;
                    }
                    Incoming::Msg(ToWindow::NoTrack) => track = None,
                    Incoming::Msg(ToWindow::Pos { ms, paused: p }) => {
                        (pos_ms, pos_at, paused) = (ms, Instant::now(), p);
                    }
                    Incoming::Closed => {
                        closed_at.get_or_insert_with(Instant::now);
                    }
                }
            }
            if closed_at.is_some_and(|t| t.elapsed() > Duration::from_millis(1500)) {
                return Ok(());
            }
            // Interpolate between the player's position updates.
            let now_ms = if paused { pos_ms } else { pos_ms + pos_at.elapsed().as_millis() as u64 };
            frame_no += 1;

            terminal.draw(|f| {
                let (title, artist) = match &track {
                    Some((t, a, _)) => (t.as_str(), a.as_str()),
                    None => ("Lyrics", ""),
                };
                let footer = if closed_at.is_some() {
                    Line::from(" player closed ").fg(Color::Rgb(255, 50, 130)).centered()
                } else if let Some((msg, at)) = &flash
                    && at.elapsed() < Duration::from_secs(3)
                {
                    Line::from(format!(" {msg} ")).fg(Color::Rgb(255, 160, 60)).centered()
                } else {
                    // Fit the hints to the window.
                    let hints = [
                        " click: jump · right-click: sync here · [ ] nudge · c copy · q close ",
                        " click jump · right-click sync · [ ] nudge · c copy · q ",
                        " right-click: sync · [ ] · c copy · q ",
                    ];
                    let fits = hints.iter().find(|h| h.chars().count() + 4 <= f.area().width as usize);
                    Line::from(*fits.unwrap_or(&hints[2])).fg(Color::Rgb(115, 115, 130)).centered()
                };
                // Title and artist, shortened so they don't run into the copy button.
                let room = (f.area().width as usize).saturating_sub(" ⎘ copy ".chars().count() + 9);
                let title_text = shorten(title, room);
                let artist_room = room.saturating_sub(title_text.chars().count() + 2);
                let artist_text = if artist_room >= 6 { format!("  {} ", shorten(artist, artist_room)) } else { " ".into() };
                let block = Block::bordered()
                    .border_type(BorderType::Rounded)
                    .border_style(Style::new().fg(Color::Rgb(255, 50, 130)))
                    .title(Line::from(vec![
                        Span::raw(" ♪ ").fg(Color::Rgb(255, 50, 130)),
                        Span::raw(title_text).bold(),
                        Span::raw(artist_text).fg(Color::Rgb(110, 190, 255)),
                    ]))
                    .title_bottom(footer);
                let inner = block.inner(f.area());
                f.render_widget(block, f.area());
                let label = " ⎘ copy ";
                let area = f.area();
                copy_button = ratatui::layout::Rect {
                    x: area.right().saturating_sub(label.chars().count() as u16 + 2),
                    y: area.y,
                    width: label.chars().count() as u16,
                    height: 1,
                };
                f.render_widget(Span::raw(label).fg(Color::Black).bg(Color::Rgb(110, 190, 255)), copy_button);
                if closed_at.is_some() {
                    let msg = Paragraph::new("The player was closed.").alignment(Alignment::Center);
                    f.render_widget(msg, inner);
                    return;
                }
                let view = LyricsView {
                    state: track.as_ref().map(|(_, _, l)| l),
                    playing: track.is_some(),
                    pos_ms: now_ms,
                    duration_ms,
                    sync: &sync,
                    frame: frame_no,
                };
                hits = render_lyrics(f, inner, view, &mut scroll);
            })?;

            if event::poll(Duration::from_millis(16))? {
                match event::read()? {
                    Event::Key(k) if k.kind == KeyEventKind::Press => match k.code {
                        KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                        KeyCode::Down | KeyCode::Char('j') => scroll.scroll_by(1),
                        KeyCode::Up | KeyCode::Char('k') => scroll.scroll_by(-1),
                        KeyCode::PageDown => scroll.scroll_by(10),
                        KeyCode::PageUp => scroll.scroll_by(-10),
                        KeyCode::Char('f') => scroll.follow(),
                        KeyCode::Char('[') => send(&mut writer, &ToPlayer::Nudge { ms: -500 }),
                        KeyCode::Char(']') => send(&mut writer, &ToPlayer::Nudge { ms: 500 }),
                        KeyCode::Char('c') => flash = Some((copy_lyrics(&track), Instant::now())),
                        _ => {}
                    },
                    Event::Mouse(m) => match m.kind {
                        MouseEventKind::Down(MouseButton::Right) => {
                            if let Some(&(_, _, line)) = hits.lines.iter().find(|(y, _, _)| *y == m.row) {
                                send(&mut writer, &ToPlayer::Pin { line });
                                scroll.follow();
                                flash = Some(("synced to this line".into(), Instant::now()));
                            }
                        }
                        MouseEventKind::Down(MouseButton::Left) => {
                            let at = ratatui::layout::Position::new(m.column, m.row);
                            if copy_button.contains(at) {
                                flash = Some((copy_lyrics(&track), Instant::now()));
                            } else if hits.follow.contains(at) {
                                scroll.follow();
                            } else if let Some(&(_, ms, _)) = hits.lines.iter().find(|(y, _, _)| *y == m.row) {
                                send(&mut writer, &ToPlayer::Seek { ms });
                                // Jump locally right away; the player confirms.
                                (pos_ms, pos_at) = (ms, Instant::now());
                                scroll.follow();
                            }
                        }
                        MouseEventKind::ScrollDown => scroll.scroll_by(2),
                        MouseEventKind::ScrollUp => scroll.scroll_by(-2),
                        _ => {}
                    },
                    _ => {}
                }
            }
        }
    })();
    let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}
