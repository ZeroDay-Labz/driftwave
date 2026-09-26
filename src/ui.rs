use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, List, ListItem, Paragraph, Tabs, Wrap};
use unicode_width::UnicodeWidthStr;

use crate::api::{Item, Playlist, Track, User};
use crate::lyrics::Lyrics;
use crate::app::{App, Mode, ViewKind};
use crate::settings::{FIELDS, Value};
use crate::lyrics::LyricsState;
use crate::queue::Repeat;

const ORANGE: Color = Color::Rgb(255, 85, 0);
const AMBER: Color = Color::Rgb(255, 160, 60);
const PINK: Color = Color::Rgb(255, 50, 130);
const VIOLET: Color = Color::Rgb(150, 90, 255);
const SKY: Color = Color::Rgb(110, 190, 255);
const LIME: Color = Color::Rgb(150, 220, 110);
const TEXT: Color = Color::Rgb(225, 225, 232);
const MUTED: Color = Color::Rgb(115, 115, 130);
const FAINT: Color = Color::Rgb(60, 60, 72);
const SELECTED_BG: Color = Color::Rgb(48, 34, 52);

/// Where clickable things ended up on screen last frame, for mouse input.
#[derive(Default)]
pub struct Hits {
    pub search: Rect,
    /// (first column, last column + 1, tab index) on `tabs_y`.
    pub tabs: Vec<(u16, u16, usize)>,
    pub tabs_y: u16,
    /// (first column, last column + 1, view index) on the list's top border.
    pub crumbs: Vec<(u16, u16, usize)>,
    /// Inner area of the list, one row per item.
    pub list: Rect,
    pub now_playing: Rect,
    pub spectrum: Rect,
    /// The seekable part of the progress bar.
    pub progress: Rect,
    pub lyrics_button: Rect,
    pub settings_button: Rect,
    /// The Settings rows, one per line.
    pub settings_rows: Rect,
    /// "⧉" next to the lyrics button, and the same in the popup's title.
    pub popout_button: Rect,
    pub popup_popout: Rect,
    pub genius_button: Rect,
    pub shuffle_button: Rect,
    pub repeat_button: Rect,
    pub radio_button: Rect,
    pub queue_button: Rect,
    pub popup: Rect,
    pub popup_close: Rect,
    /// (row, timestamp) of each visible synced lyric line, for click-to-seek.
    pub lyric_lines: Vec<(u16, u64, usize)>,
    pub popup_copy: Rect,
    /// Genre labels in the list, clickable to open that genre.
    pub genre_links: Vec<(Rect, String)>,
    pub lyrics_follow: Rect,
}

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let now_height = (area.height / 3).clamp(7, 14);
    let [search_area, tabs_area, list_area, now_area, help_area] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Min(4),
        Constraint::Length(now_height),
        Constraint::Length(1),
    ])
    .areas(area);

    app.hits = Hits { search: search_area, now_playing: now_area, ..Hits::default() };
    draw_search(frame, app, search_area);
    draw_tabs(frame, app, tabs_area);
    draw_list(frame, app, list_area);
    draw_now_playing(frame, app, now_area);
    draw_help(frame, app, help_area);
    if app.lyrics_open {
        let over = Rect { height: list_area.bottom() - tabs_area.y, ..tabs_area };
        draw_lyrics(frame, app, over);
    }
    if app.keys_open {
        draw_keys(frame, Rect { height: area.height.saturating_sub(1), ..area });
    }
}

const KEYS: &[(&str, &str)] = &[
    ("/  s", "search, or paste a soundcloud.com link"),
    ("⏎ / double-click", "play track, open artist / album"),
    ("tab  1-4", "switch tabs"),
    ("a", "go to the artist"),
    ("#  / click a genre", "browse a genre or tag (#folk punk)"),
    ("t", "open the selected track's genre"),
    ("esc  ⌫", "back"),
    ("␣ / click visualizer", "pause / resume"),
    ("n  p", "next / previous (p restarts if >3 s in)"),
    ("←  →  / click bar", "seek 10 s / seek anywhere"),
    ("+  -  / wheel on player", "volume"),
    ("e  E", "add to queue / play next"),
    ("u", "show the queue (⏎ jump, d remove)"),
    ("h", "recently played"),
    ("x  r  R", "shuffle / repeat / radio"),
    ("l  L", "lyrics / pop lyrics out to a window"),
    ("[  ]  / right-click", "lyrics timing: nudge / \"this line is now\""),
    ("c", "copy the lyrics"),
    ("o", "search the song on Genius"),
    ("D", "official download (if the uploader allows)"),
    (",  F2", "settings / library folder"),
    ("q", "quit"),
];

fn draw_keys(frame: &mut Frame, over: Rect) {
    let key_w = KEYS.iter().map(|(k, _)| k.width()).max().unwrap_or(0);
    let text_w = KEYS.iter().map(|(_, t)| t.width()).max().unwrap_or(0);
    let width = ((key_w + text_w + 6) as u16).min(over.width);
    let height = (KEYS.len() as u16 + 2).min(over.height);
    let area = Rect {
        x: over.x + (over.width - width) / 2,
        y: over.y + (over.height - height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, area);
    let lines: Vec<Line> = KEYS
        .iter()
        .map(|(k, what)| {
            let pad = " ".repeat(key_w - k.width());
            Line::from(vec![Span::raw(format!(" {pad}{k}  ")).fg(ORANGE).bold(), Span::raw(*what).fg(TEXT)])
        })
        .collect();
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(ORANGE))
        .title(Line::from(" Keys ").fg(ORANGE).bold())
        .title_bottom(Line::from(" any key closes ").fg(MUTED).centered());
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn panel(title: Line<'_>, focused: bool) -> Block<'_> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(if focused { ORANGE } else { FAINT }))
        .title(title)
}

fn draw_search(frame: &mut Frame, app: &mut App, area: Rect) {
    let searching = app.mode == Mode::Search;
    let title = Line::from(vec![
        Span::raw(" ☁ ").fg(ORANGE),
        Span::raw("drift").fg(ORANGE).bold(),
        Span::raw("wave ").fg(AMBER),
    ]);
    let text = if app.input.is_empty() && !searching {
        Line::from("press / to search artists, tracks, albums, #genres (#folk punk), or paste a link").fg(MUTED)
    } else {
        Line::from(app.input.as_str()).fg(TEXT)
    };
    frame.render_widget(Paragraph::new(text).block(panel(title, searching)), area);
    let label = " ⚙ settings ";
    let w = label.width() as u16;
    if area.width > w + 20 {
        let rect = Rect { x: area.right() - w - 2, y: area.y, width: w, height: 1 };
        frame.render_widget(Span::raw(label).fg(MUTED), rect);
        app.hits.settings_button = rect;
    }
    if searching {
        frame.set_cursor_position((area.x + 1 + app.input.width() as u16, area.y + 1));
    }
}

fn draw_tabs(frame: &mut Frame, app: &mut App, area: Rect) {
    let Some(view) = app.views.last() else { return };
    let titles: Vec<Line> = view.tabs.iter().map(|t| {
        if view.kind != ViewKind::Browse && view.kind != ViewKind::Queue {
            return Line::from(format!(" {} ", t.name));
        }
        let count = if t.loading {
            SPINNER[(app.frame / 2) as usize % SPINNER.len()].to_string()
        } else {
            t.items.len().to_string()
        };
        Line::from(vec![Span::raw(format!(" {} ", t.name)), Span::raw(format!("{count} ")).fg(MUTED)])
    }).collect();

    // Mirror Tabs' layout: pad, title, pad, divider.
    app.hits.tabs_y = area.y;
    let mut x = area.x;
    for (i, t) in titles.iter().enumerate() {
        let end = x + t.width() as u16 + 2;
        app.hits.tabs.push((x, end, i));
        x = end + 1;
    }
    frame.render_widget(
        Tabs::new(titles)
            .select(view.tab)
            .style(Style::new().fg(TEXT))
            .highlight_style(Style::new().fg(Color::Black).bg(ORANGE).bold())
            .divider(Span::raw("│").fg(FAINT)),
        area,
    );
}

fn draw_list(frame: &mut Frame, app: &mut App, area: Rect) {
    let crumbs: Vec<String> = app.views.iter().map(|v| v.title.clone()).collect();
    let mut title = vec![Span::raw(" ")];
    let mut x = area.x + 2; // corner + leading space
    for (i, c) in crumbs.iter().enumerate() {
        if i > 0 {
            title.push(Span::raw(" › ").fg(MUTED));
            x += 3;
        }
        let w = c.width() as u16;
        app.hits.crumbs.push((x, x + w, i));
        x += w;
        let last = i + 1 == crumbs.len();
        title.push(Span::raw(c.clone()).fg(if last { AMBER } else { MUTED }));
    }
    title.push(Span::raw(" "));
    let block = panel(Line::from(title), app.mode == Mode::Normal);
    app.hits.list = block.inner(area);

    if app.views.last().is_some_and(|v| v.kind == ViewKind::Settings) {
        draw_settings(frame, app, block, area);
        return;
    }
    let playing_id = app.now_playing().map(|t| t.id);
    let paused = app.player.is_paused();
    let frame_no = app.frame;
    let Some(view) = app.views.last_mut() else {
        let hint = Paragraph::new(vec![
            Line::raw(""),
            Line::from("Search for an artist to browse all their tracks, albums and playlists.").fg(MUTED),
        ])
        .alignment(Alignment::Center)
        .block(block);
        frame.render_widget(hint, area);
        return;
    };

    let tab = &mut view.tabs[view.tab];
    let inner_width = area.width.saturating_sub(4) as usize;
    let message = if tab.loading {
        Some(Line::from(format!("{} Loading…", SPINNER[(frame_no / 2) as usize % SPINNER.len()])).fg(AMBER))
    } else if let Some(e) = &tab.error {
        Some(Line::from(format!("Error: {e}")).fg(PINK))
    } else if tab.items.is_empty() {
        Some(Line::from("Nothing here").fg(MUTED))
    } else {
        None
    };
    if let Some(msg) = message {
        frame.render_widget(Paragraph::new(msg).alignment(Alignment::Center).block(block), area);
        return;
    }

    // (item index, genre, cells from the right, width) for clickable genres.
    let mut genres: Vec<(usize, String, usize, usize)> = Vec::new();
    let items: Vec<ListItem> = tab
        .items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let row = match item {
                Item::Track(t) => {
                    let marker = if playing_id == Some(t.id) {
                        if paused { Span::raw(" ⏸ ").fg(AMBER) } else { playing_marker(frame_no) }
                    } else {
                        Span::raw(format!("{:>3} ", i + 1)).fg(FAINT)
                    };
                    let (line, genre) = track_row(t, marker, inner_width);
                    if let (Some((from_right, w)), Some(g)) = (genre, &t.genre) {
                        genres.push((i, g.trim().to_string(), from_right, w));
                    }
                    line
                }
                Item::Tag { name, count } => tag_row(name, *count, inner_width),
                Item::User(u) => user_row(u, inner_width),
                Item::Playlist(p) => playlist_row(p, inner_width),
            };
            ListItem::new(row)
        })
        .collect();

    frame.render_stateful_widget(
        List::new(items)
            .block(block)
            .highlight_style(Style::new().bg(SELECTED_BG).add_modifier(Modifier::BOLD))
            .highlight_symbol("▌")
            .highlight_spacing(ratatui::widgets::HighlightSpacing::Always),
        area,
        &mut tab.state,
    );

    // Where each visible row's genre label ended up, so it can be clicked.
    let list = app.hits.list;
    let offset = tab.state.offset();
    let text_x = list.x + 1 + inner_width as u16; // after the "▌" column
    for (i, genre, from_right, w) in genres {
        if i < offset || i >= offset + list.height as usize {
            continue;
        }
        let x = text_x.saturating_sub(from_right as u16);
        let rect = Rect { x, y: list.y + (i - offset) as u16, width: w as u16, height: 1 };
        app.hits.genre_links.push((rect, genre));
    }
}

fn draw_settings(frame: &mut Frame, app: &mut App, block: Block, area: Rect) {
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let rows = Rect { x: inner.x, y: inner.y + 1, width: inner.width, height: (FIELDS.len() as u16).min(inner.height) };
    app.hits.settings_rows = rows;
    let label_w = 34;
    for (i, field) in FIELDS.iter().enumerate() {
        let y = rows.y + i as u16;
        if y >= inner.bottom() {
            break;
        }
        let selected = i == app.settings_sel;
        let editing = selected && app.settings_edit.is_some();
        let value = if editing {
            let text = app.settings_edit.as_deref().unwrap_or("");
            let shown = if *field == crate::settings::Field::Token { "•".repeat(text.chars().count()) } else { text.to_string() };
            frame.set_cursor_position((inner.x + 3 + label_w + shown.width() as u16, y));
            Span::raw(shown).fg(AMBER).underlined()
        } else {
            match field.value(&app.state) {
                Value::Toggle(true) => Span::raw("● on").fg(LIME).bold(),
                Value::Toggle(false) => Span::raw("○ off").fg(MUTED),
                Value::Text(t) => Span::raw(t).fg(SKY),
                Value::Action => Span::raw("↗ open").fg(ORANGE),
            }
        };
        let marker = if selected { Span::raw("▌ ").fg(ORANGE) } else { Span::raw("  ") };
        let label = Span::raw(format!("{:<w$}", field.label(), w = label_w as usize)).fg(TEXT);
        let mut line = Line::from(vec![marker, label, Span::raw(" "), value]);
        if selected {
            line = line.bg(SELECTED_BG);
        }
        frame.render_widget(line, Rect { y, height: 1, ..inner });
    }

    // Help for the selected row, below the list.
    let help_y = rows.bottom() + 1;
    if help_y < inner.bottom() {
        let field = FIELDS[app.settings_sel];
        let hint = if app.settings_edit.is_some() { "⏎ save · esc cancel" } else { "⏎ / double-click to change · esc back" };
        let help = Paragraph::new(vec![
            Line::from(field.help()).fg(MUTED),
            Line::raw(""),
            Line::from(hint).fg(FAINT),
        ])
        .wrap(Wrap { trim: true });
        let area = Rect { x: inner.x + 2, y: help_y, width: inner.width.saturating_sub(4), height: inner.bottom() - help_y };
        frame.render_widget(help, area);
    }
}

/// A little dancing equaliser for the row that is playing.
fn playing_marker(frame: u64) -> Span<'static> {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let f = (frame / 3) as usize;
    let s: String = [f * 3, f * 5 + 2, f * 7 + 5].iter().map(|x| BARS[x % 8]).collect();
    Span::raw(format!("{s} ")).fg(ORANGE)
}

/// Lay out `left` and `right` on one line of `width` cells, truncating the
/// first left span (the title) if needed.
fn row<'a>(mut left: Vec<Span<'a>>, right: Vec<Span<'a>>, width: usize) -> Line<'a> {
    let right_w: usize = right.iter().map(Span::width).sum();
    let left_w: usize = left.iter().map(Span::width).sum();
    let budget = width.saturating_sub(right_w + 1);
    if left_w > budget && left.len() > 1 {
        let overflow = left_w - budget;
        let title = &left[1];
        let keep = title.width().saturating_sub(overflow + 1);
        let mut s = String::new();
        for c in title.content.chars() {
            if s.width() + c.to_string().width() > keep {
                break;
            }
            s.push(c);
        }
        s.push('…');
        left[1] = Span::styled(s, title.style);
    }
    let used: usize = left.iter().map(Span::width).sum();
    left.push(Span::raw(" ".repeat(width.saturating_sub(used + right_w))));
    left.extend(right);
    Line::from(left)
}

/// A track's row, plus where its genre label sits: (cells from the row's
/// right edge to the label's start, label width), for click-to-open-genre.
fn track_row(t: &Track, marker: Span<'static>, width: usize) -> (Line<'static>, Option<(usize, usize)>) {
    let mut title = Span::raw(t.title.clone()).fg(TEXT);
    if t.is_drm_only() {
        title = title.fg(MUTED);
    }
    if !t.is_playable() {
        title = title.crossed_out().fg(MUTED);
    }
    let left = vec![marker, title, Span::raw("  "), Span::raw(t.user.username.clone()).fg(SKY)];
    let mut right = Vec::new();
    if t.is_preview() {
        right.push(Span::raw("preview ").fg(PINK));
    }
    if t.is_drm_only() {
        right.push(Span::raw("DRM ").fg(PINK));
    }
    if t.can_download() {
        right.push(Span::raw("⤓ ").fg(LIME));
    }
    let genre_at = right.len();
    let has_genre = t.genre.as_deref().is_some_and(|g| !g.trim().is_empty());
    if let Some(g) = t.genre.as_deref().filter(|g| !g.trim().is_empty()) {
        right.push(Span::raw(format!("{} ", truncate(g, 14))).fg(LIME));
    }
    if let Some(plays) = t.playback_count {
        right.push(Span::raw(format!("▶ {:>6} ", fmt_count(plays))).fg(MUTED));
    }
    right.push(Span::raw(format!("{:>6}", fmt_ms(t.duration))).fg(AMBER));
    let genre = has_genre.then(|| {
        let from_right: usize = right[genre_at..].iter().map(Span::width).sum();
        (from_right, right[genre_at].width() - 1)
    });
    (row(left, right, width), genre)
}

fn tag_row(name: &str, count: usize, width: usize) -> Line<'static> {
    let left = vec![Span::raw(" #  ").fg(LIME).bold(), Span::raw(name.to_string()).fg(TEXT).bold()];
    let right = vec![Span::raw(format!("on {count} of these tracks")).fg(MUTED)];
    row(left, right, width)
}

fn user_row(u: &User, width: usize) -> Line<'static> {
    let mut left = vec![Span::raw(" ◉  ").fg(SKY), Span::raw(u.username.clone()).fg(TEXT).bold()];
    if u.verified == Some(true) {
        left.push(Span::raw(" ✔").fg(SKY));
    }
    if let Some(name) = u.full_name.as_deref().filter(|n| !n.is_empty() && *n != u.username) {
        left.push(Span::raw(format!("  {name}")).fg(MUTED));
    }
    let right = vec![
        Span::raw(format!("{:>7} followers  ", fmt_count(u.followers_count.unwrap_or(0)))).fg(MUTED),
        Span::raw(format!("{:>5} tracks", u.track_count.unwrap_or(0))).fg(AMBER),
    ];
    row(left, right, width)
}

fn playlist_row(p: &Playlist, width: usize) -> Line<'static> {
    let kind = p.kind();
    let color = match kind {
        "album" => VIOLET,
        "ep" => PINK,
        "single" => LIME,
        "compilation" => AMBER,
        _ => SKY,
    };
    let badge = Span::raw(format!(" {:<7}", kind.to_uppercase())).fg(color).bold();
    let left = vec![
        badge,
        Span::raw(p.title.clone()).fg(TEXT),
        Span::raw("  "),
        Span::raw(p.user.username.clone()).fg(SKY),
    ];
    let mut right = Vec::new();
    if let Some(y) = p.year() {
        right.push(Span::raw(format!("{y}  ")).fg(MUTED));
    }
    right.push(Span::raw(format!("{:>3} tracks  ", p.track_count.unwrap_or(0))).fg(MUTED));
    right.push(Span::raw(format!("{:>7}", fmt_ms(p.duration.unwrap_or(0)))).fg(AMBER));
    row(left, right, width)
}

fn draw_now_playing(frame: &mut Frame, app: &mut App, area: Rect) {
    let track = app.now_playing().cloned();
    let paused = app.player.is_paused();

    let title = match &track {
        Some(t) => {
            let icon = if app.loading {
                SPINNER[(app.frame / 2) as usize % SPINNER.len()]
            } else if paused {
                "⏸"
            } else {
                "▶"
            };
            let mut spans = vec![
                Span::raw(format!(" {icon} ")).fg(ORANGE),
                Span::raw(t.title.clone()).fg(TEXT).bold(),
                Span::raw("  "),
                Span::raw(t.user.username.clone()).fg(SKY),
                Span::raw(" "),
            ];
            if let Some(q) = app.quality {
                spans.push(Span::raw(format!(" {q} ")).fg(Color::Black).bg(if q.starts_with("AAC") {
                    LIME
                } else {
                    AMBER
                }));
                spans.push(Span::raw(" "));
            }
            Line::from(spans)
        }
        None => Line::from(" Nothing playing ").fg(MUTED),
    };
    let block = panel(title, false);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    draw_buttons(frame, app, area);
    draw_modes(frame, app, area);
    if inner.height < 2 {
        return;
    }

    // Small square cover on the left (cells are about twice as tall as wide).
    let mut inner = inner;
    if let Some(t) = &track
        && inner.width >= 60
        && let Some(art) = &mut app.art
        && art.has(t.id)
    {
        let h = inner.height.min(7);
        let art_area = Rect { x: inner.x + 1, y: inner.y + (inner.height - h) / 2, width: h * 2, height: h };
        art.draw(frame, t.id, art_area);
        inner.x += art_area.width + 3;
        inner.width -= art_area.width + 3;
    }

    let [viz_area, progress_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);

    // Bars are one cell wide with a one-cell gap.
    let n_bars = (viz_area.width as usize).div_ceil(2);
    let active = track.is_some() && !app.loading && !paused;
    app.spectrum.update(&app.player.ring, n_bars, active);
    draw_spectrum(frame, &app.spectrum.bars, &app.spectrum.peaks, viz_area);
    app.hits.spectrum = viz_area;

    let (pos, dur) = match &track {
        Some(t) => (app.player.position().as_millis() as u64, t.duration),
        None => (0, 0),
    };
    app.hits.progress = draw_progress(frame, pos, dur, app.player.volume(), progress_area);
}

/// "♪ Lyrics" and "Genius ↗" buttons on the right of the player's top border.
fn draw_buttons(frame: &mut Frame, app: &mut App, area: Rect) {
    let state = app.now_playing().and_then(|t| app.lyrics.get(&t.id));
    let spinner = SPINNER[(app.frame / 2) as usize % SPINNER.len()];
    let (label, style) = match state {
        _ if app.lyrics_open => (" ♪ Lyrics ".to_string(), Style::new().fg(Color::Black).bg(PINK).bold()),
        None => (" ♪ Lyrics ".into(), Style::new().fg(FAINT)),
        Some(LyricsState::Loading) => (format!(" {spinner} Lyrics "), Style::new().fg(AMBER)),
        Some(LyricsState::Ready(Some(f))) => {
            let bg = if matches!(f.lyrics, Lyrics::Synced(_)) { ORANGE } else { AMBER };
            (" ♪ Lyrics ".into(), Style::new().fg(Color::Black).bg(bg).bold())
        }
        Some(_) => (" ♪ no lyrics ".into(), Style::new().fg(MUTED)),
    };
    let genius = " Genius ↗ ";
    let genius_style = if app.now_playing().is_some() { Style::new().fg(Color::Black).bg(LIME) } else { Style::new().fg(FAINT) };

    let popout = " ⧉ ";
    let popout_style = if app.now_playing().is_some() { Style::new().fg(Color::Black).bg(PINK) } else { Style::new().fg(FAINT) };

    let gw = genius.width() as u16;
    let lw = label.width() as u16;
    let pw = popout.width() as u16;
    let right = area.right().saturating_sub(2);
    let genius_rect = Rect { x: right.saturating_sub(gw), y: area.y, width: gw, height: 1 };
    let popout_rect = Rect { x: genius_rect.x.saturating_sub(pw + 1), y: area.y, width: pw, height: 1 };
    let lyrics_rect = Rect { x: popout_rect.x.saturating_sub(lw), y: area.y, width: lw, height: 1 };
    if lyrics_rect.x <= area.x + 2 {
        return; // too narrow
    }
    frame.render_widget(Span::styled(label, style), lyrics_rect);
    frame.render_widget(Span::styled(popout, popout_style), popout_rect);
    frame.render_widget(Span::styled(genius, genius_style), genius_rect);
    app.hits.lyrics_button = lyrics_rect;
    app.hits.popout_button = popout_rect;
    app.hits.genius_button = genius_rect;
}

/// Queue position and shuffle / repeat / radio toggles on the player's
/// bottom border.
fn draw_modes(frame: &mut Frame, app: &mut App, area: Rect) {
    let y = area.bottom().saturating_sub(1);
    let on = |active: bool, color: Color| {
        if active { Style::new().fg(Color::Black).bg(color).bold() } else { Style::new().fg(MUTED) }
    };
    let q = &app.queue;
    let repeat = match q.repeat {
        Repeat::Off => " ↻ repeat ",
        Repeat::All => " ↻ all ",
        Repeat::One => " ↻ one ",
    };
    let queue_label = match q.position() {
        Some((i, n)) => format!(" ☰ {i}/{n} "),
        None => " ☰ queue ".into(),
    };
    // Each button shows the key that does the same thing.
    let buttons: [(String, &str, Style); 4] = [
        (queue_label, "u", Style::new().fg(TEXT).bg(FAINT)),
        (" ⇄ shuffle ".into(), "x", on(q.shuffle, SKY)),
        (repeat.into(), "r", on(q.repeat != Repeat::Off, VIOLET)),
        (" ∞ radio ".into(), "R", on(q.radio, LIME)),
    ];
    let mut x = area.x + 2;
    let mut rects = [Rect::default(); 4];
    for (i, (label, key, style)) in buttons.into_iter().enumerate() {
        let key_style = if style.bg.is_some() { style.bold() } else { Style::new().fg(ORANGE).bold() };
        let line = Line::from(vec![Span::styled(label, style), Span::styled(format!("{key} "), key_style)]);
        let w = line.width() as u16;
        if x + w + 2 > area.right() {
            break;
        }
        rects[i] = Rect { x, y, width: w, height: 1 };
        frame.render_widget(line, rects[i]);
        x += w + 1;
    }
    let hits = &mut app.hits;
    [hits.queue_button, hits.shuffle_button, hits.repeat_button, hits.radio_button] = rects;

    // Messages ("Shuffle on", "Loading…") show briefly on the right.
    let room = area.right().saturating_sub(x + 3) as usize;
    if !app.status.is_empty() && room > 8 {
        let text = format!(" {} ", truncate(&app.status, room - 2));
        let w = text.width() as u16;
        let rect = Rect { x: area.right() - 2 - w, y, width: w, height: 1 };
        frame.render_widget(Span::raw(text).fg(Color::Black).bg(AMBER), rect);
    }
}

fn draw_lyrics(frame: &mut Frame, app: &mut App, over: Rect) {
    let width = (over.width * 4 / 5).clamp(40.min(over.width), 100);
    let area = Rect { x: over.x + (over.width - width) / 2, width, ..over };
    app.hits.popup = area;
    frame.render_widget(Clear, area);

    let track = app.now_playing().cloned();
    let title = match &track {
        Some(t) => Line::from(vec![
            Span::raw(" ♪ ").fg(PINK),
            Span::raw(t.title.clone()).fg(TEXT).bold(),
            Span::raw(" "),
        ]),
        None => Line::from(" ♪ Lyrics ").fg(PINK),
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(PINK))
        .title(title)
        .title_bottom(
            Line::from(" click: jump · right-click: sync here · [ ] nudge · c copy · f follow · esc ")
                .fg(MUTED)
                .centered(),
        );
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let close = Rect { x: area.right().saturating_sub(5), y: area.y, width: 3, height: 1 };
    frame.render_widget(Span::raw(" ✕ ").fg(TEXT).bg(FAINT), close);
    app.hits.popup_close = close;
    let popout_label = " ⧉ pop out ";
    let pw = popout_label.width() as u16;
    let popout = Rect { x: close.x.saturating_sub(pw + 1), y: area.y, width: pw, height: 1 };
    frame.render_widget(Span::raw(popout_label).fg(Color::Black).bg(PINK), popout);
    app.hits.popup_popout = popout;
    let copy_label = " ⎘ copy ";
    let cw = copy_label.width() as u16;
    let copy = Rect { x: popout.x.saturating_sub(cw + 1), y: area.y, width: cw, height: 1 };
    frame.render_widget(Span::raw(copy_label).fg(Color::Black).bg(SKY), copy);
    app.hits.popup_copy = copy;
    let sync = track.as_ref().and_then(|t| app.state.lyrics_sync.get(&t.id)).cloned().unwrap_or_default();
    let state = track.as_ref().and_then(|t| app.lyrics.get(&t.id));
    let view = LyricsView {
        state,
        playing: track.is_some(),
        pos_ms: app.player.position().as_millis() as u64,
        duration_ms: track.as_ref().map_or(0, |t| t.duration),
        sync: &sync,
        frame: app.frame,
    };
    let hits = render_lyrics(frame, inner, view, &mut app.lyrics_scroll);
    app.hits.lyric_lines = hits.lines;
    app.hits.lyrics_follow = hits.follow;
}

/// What to show in a lyrics panel (the in-app popup or the pop-out window).
pub struct LyricsView<'a> {
    pub state: Option<&'a LyricsState>,
    pub playing: bool,
    pub pos_ms: u64,
    /// Track length, for estimating where unsynced lyrics are.
    pub duration_ms: u64,
    /// The user's timing corrections for this track.
    pub sync: &'a crate::lyrics::LyricSync,
    /// Animation frame counter, for the spinner.
    pub frame: u64,
}

/// Scroll position of a lyrics panel. It follows the song, except for a
/// few seconds after the user scrolls by hand.
#[derive(Default)]
pub struct LyricsScroll {
    top: f32,
    manual_until: Option<Instant>,
}

impl LyricsScroll {
    pub fn scroll_by(&mut self, rows: i32) {
        self.top = (self.top + rows as f32).max(0.0);
        self.manual_until = Some(Instant::now() + Duration::from_secs(5));
    }

    pub fn follow(&mut self) {
        self.manual_until = None;
    }

    fn following(&self) -> bool {
        self.manual_until.is_none_or(|t| Instant::now() >= t)
    }
}

/// Clickable parts of a lyrics panel.
#[derive(Default)]
pub struct LyricsHits {
    /// (row, timestamp, line index) of each visible line: click seeks,
    /// right-click pins "this line is being sung now".
    pub lines: Vec<(u16, u64, usize)>,
    /// The "↓ follow" button shown while auto-follow is paused.
    pub follow: Rect,
}

/// Word-wrap `text` to `width` cells (long words are split).
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut rows = vec![String::new()];
    for word in text.split_whitespace() {
        let mut word = word.to_string();
        loop {
            let row = rows.last_mut().unwrap();
            let need = if row.is_empty() { word.width() } else { row.width() + 1 + word.width() };
            if need <= width {
                if !row.is_empty() {
                    row.push(' ');
                }
                row.push_str(&word);
                break;
            }
            if row.is_empty() {
                // A single word wider than the panel: split it.
                let cut: String = word.chars().scan(0, |w, c| {
                    *w += c.to_string().width();
                    (*w <= width).then_some(c)
                }).collect();
                let rest = word[cut.len()..].to_string();
                *row = cut;
                rows.push(String::new());
                if rest.is_empty() {
                    break;
                }
                word = rest;
            } else {
                rows.push(String::new());
            }
        }
    }
    if rows.len() > 1 && rows.last().is_some_and(String::is_empty) {
        rows.pop();
    }
    rows
}

/// Draw lyrics into `inner`, following the song: synced lyrics exactly,
/// unsynced ones by an estimate. Returns what can be clicked.
pub fn render_lyrics(frame: &mut Frame, inner: Rect, view: LyricsView, scroll: &mut LyricsScroll) -> LyricsHits {
    let mut hits = LyricsHits::default();
    if inner.height < 3 {
        return hits;
    }

    let message = |text: &str, color: Color| {
        Paragraph::new(vec![Line::raw(""), Line::from(text.to_string()).fg(color)]).alignment(Alignment::Center)
    };
    if !view.playing {
        frame.render_widget(message("Play something to see its lyrics.", MUTED), inner);
        return hits;
    }
    let found = match view.state {
        None | Some(LyricsState::Loading) => {
            let spin = SPINNER[(view.frame / 2) as usize % SPINNER.len()];
            frame.render_widget(message(&format!("{spin} Looking up lyrics…"), AMBER), inner);
            return hits;
        }
        Some(LyricsState::Failed(e)) => {
            frame.render_widget(message(&format!("Lyrics lookup failed: {e}"), PINK).wrap(Wrap { trim: true }), inner);
            return hits;
        }
        Some(LyricsState::Ready(None)) => {
            let p = Paragraph::new(vec![
                Line::raw(""),
                Line::from("No lyrics found for this track.").fg(MUTED),
                Line::raw(""),
                Line::from(vec![
                    Span::raw("Press ").fg(MUTED),
                    Span::raw("o").fg(ORANGE).bold(),
                    Span::raw(" or click ").fg(MUTED),
                    Span::raw(" Genius ↗ ").fg(Color::Black).bg(LIME),
                    Span::raw(" to search in your browser.").fg(MUTED),
                ]),
            ])
            .alignment(Alignment::Center);
            frame.render_widget(p, inner);
            return hits;
        }
        Some(LyricsState::Ready(Some(f))) => f,
    };

    if matches!(found.lyrics, Lyrics::Instrumental) {
        frame.render_widget(message("♪  Instrumental  ♪", VIOLET), inner);
        return hits;
    }
    // Timed lines: exact for synced lyrics, estimated for plain text, both
    // with the user's corrections applied.
    let (lines, exact) = crate::lyrics::timed_lines(&found.lyrics, view.duration_ms, view.sync);
    let lines = lines.as_slice();

    let source = if found.source.is_empty() { "LRCLIB" } else { found.source.as_str() };
    let mut header = vec![Line::from(format!("{} · via {source}", found.matched)).fg(MUTED).centered()];
    if let Some(note) = &found.note {
        header.push(Line::from(note.as_str()).fg(AMBER).centered());
    }
    let follows = exact || view.duration_ms > 0;
    if !exact && follows {
        let hint = if view.sync.anchors.is_empty() {
            "unsynced, following roughly · right-click a line as it's sung to sync"
        } else {
            "synced by you · right-click more lines to refine"
        };
        header.push(Line::from(hint).fg(FAINT).centered());
    }
    if view.sync.offset_ms != 0 {
        let secs = view.sync.offset_ms as f64 / 1000.0;
        header.push(Line::from(format!("timing {secs:+.1} s · [ ] to nudge")).fg(FAINT).centered());
    }
    let header_h = header.len() as u16 + 1;
    frame.render_widget(Paragraph::new(header), inner);
    let body = Rect { y: inner.y + header_h, height: inner.height.saturating_sub(header_h), ..inner };
    if body.height == 0 {
        return hits;
    }

    // Line being sung (the last one whose time has come).
    let current = if follows { lines.partition_point(|(t, _)| *t <= view.pos_ms).checked_sub(1) } else { None };

    // Wrap into screen rows, remembering which line each row belongs to.
    let width = body.width.saturating_sub(6).max(8) as usize;
    let mut rows: Vec<(usize, String)> = Vec::new();
    let mut first_row = Vec::with_capacity(lines.len());
    for (i, (_, text)) in lines.iter().enumerate() {
        first_row.push(rows.len());
        if text.is_empty() {
            rows.push((i, if exact { "♪".into() } else { String::new() }));
        } else {
            rows.extend(wrap(text, width).into_iter().map(|r| (i, r)));
        }
    }

    // Glide toward keeping the current line a third of the way down.
    let h = body.height as usize;
    let max_top = rows.len().saturating_sub(h) as f32;
    if scroll.following() {
        let target = current.map_or(0, |c| first_row[c].saturating_sub(h / 3)) as f32;
        let diff = target.min(max_top) - scroll.top;
        scroll.top += if diff.abs() > h as f32 { diff } else { diff.signum() * diff.abs().min(1.0 + diff.abs() * 0.2) };
    }
    scroll.top = scroll.top.clamp(0.0, max_top);

    let top = scroll.top.round() as usize;
    for (n, (i, text)) in rows.iter().skip(top).take(h).enumerate() {
        let y = body.y + n as u16;
        let header_row = !exact && crate::lyrics::is_section_header(text);
        let line = match current {
            _ if header_row => Line::from(text.as_str()).fg(VIOLET).italic(),
            Some(c) if *i == c && exact => Line::from(vec![
                Span::raw("▸ ").fg(PINK),
                Span::raw(text.as_str()).fg(ORANGE).bold(),
                Span::raw(" ◂").fg(PINK),
            ]),
            Some(c) if *i == c => Line::from(vec![
                Span::raw("› ").fg(FAINT),
                Span::raw(text.as_str()).fg(AMBER).bold(),
                Span::raw(" ‹").fg(FAINT),
            ]),
            Some(c) if *i < c => Line::from(text.as_str()).fg(FAINT),
            Some(c) if *i == c + 1 => Line::from(text.as_str()).fg(TEXT),
            None => Line::from(text.as_str()).fg(TEXT),
            _ => Line::from(text.as_str()).fg(MUTED),
        };
        frame.render_widget(line.centered(), Rect { y, height: 1, ..body });
        if follows {
            hits.lines.push((y, lines[*i].0, *i));
        }
    }

    if !scroll.following() && follows {
        let label = " ↓ follow  f ";
        let w = label.width() as u16;
        let rect = Rect { x: body.right().saturating_sub(w + 1), y: body.bottom() - 1, width: w, height: 1 };
        frame.render_widget(Span::raw(label).fg(Color::Black).bg(AMBER), rect);
        hits.follow = rect;
    }
    hits
}

fn lerp(a: Color, b: Color, t: f32) -> Color {
    let (Color::Rgb(r1, g1, b1), Color::Rgb(r2, g2, b2)) = (a, b) else { return a };
    let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color::Rgb(mix(r1, r2), mix(g1, g2), mix(b1, b2))
}

/// Orange at the bottom, through pink, to violet at the top.
fn gradient(t: f32) -> Color {
    if t < 0.5 { lerp(ORANGE, PINK, t * 2.0) } else { lerp(PINK, VIOLET, (t - 0.5) * 2.0) }
}

fn draw_spectrum(frame: &mut Frame, bars: &[f32], peaks: &[f32], area: Rect) {
    const EIGHTHS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let h = area.height as usize;
    let buf = frame.buffer_mut();
    for (i, (&bar, &peak)) in bars.iter().zip(peaks).enumerate() {
        let x = area.x + (i * 2) as u16;
        if x >= area.right() {
            break;
        }
        let level = (bar * (h * 8) as f32).round() as usize;
        let peak_row = ((peak * h as f32) as usize).min(h - 1);
        for r in 0..h {
            let y = area.bottom() - 1 - r as u16;
            let fill = level.saturating_sub(r * 8).min(8);
            let cell = &mut buf[(x, y)];
            if fill > 0 {
                cell.set_char(EIGHTHS[fill]).set_fg(gradient(r as f32 / h.max(2) as f32));
            } else if r == peak_row && peak > 0.1 && peak > bar + 0.02 {
                cell.set_char('▔').set_fg(lerp(gradient(r as f32 / h as f32), TEXT, 0.5));
            } else if r == 0 {
                cell.set_char('▁').set_fg(FAINT);
            }
        }
    }
}

/// Returns the area of the bar itself (for click-to-seek).
fn draw_progress(frame: &mut Frame, pos_ms: u64, dur_ms: u64, volume: f32, area: Rect) -> Rect {
    let left = format!(" {} ", fmt_ms(pos_ms));
    let right = format!(" {}   vol {:>3.0}% ", fmt_ms(dur_ms), volume * 100.0);
    let bar_w = (area.width as usize).saturating_sub(left.width() + right.width());
    let ratio = if dur_ms > 0 { (pos_ms as f64 / dur_ms as f64).min(1.0) } else { 0.0 };
    let filled = ((bar_w as f64 * ratio) as usize).min(bar_w.saturating_sub(1));

    let left_w = left.width();
    let mut spans = vec![Span::raw(left).fg(AMBER)];
    for i in 0..filled {
        spans.push(Span::raw("━").fg(gradient(i as f32 / bar_w.max(1) as f32 * 0.8)));
    }
    if bar_w > 0 {
        spans.push(Span::raw("●").fg(if dur_ms > 0 { TEXT } else { FAINT }));
        spans.push(Span::raw("─".repeat(bar_w - filled - 1)).fg(FAINT));
    }
    spans.push(Span::raw(right).fg(MUTED));
    frame.render_widget(Line::from(spans), area);
    Rect { x: area.x + left_w as u16, y: area.y, width: bar_w as u16, height: 1 }
}

/// The key legend. Always shown; messages go on the player's border.
fn draw_help(frame: &mut Frame, app: &App, area: Rect) {
    let keys: &[(&str, &str)] = if app.mode == Mode::Search {
        &[("⏎", "search"), ("esc", "cancel")]
    } else {
        &[
            ("/", "search"),
            ("⏎", "open"),
            ("␣", "pause"),
            ("n/p", "skip"),
            ("←→", "seek"),
            ("l", "lyrics"),
            ("e", "enqueue"),
            ("u", "up next"),
            ("q", "quit"),
            ("?", "all keys"),
        ]
    };
    let mut spans = vec![Span::raw(" ")];
    for (k, what) in keys {
        spans.push(Span::raw(*k).fg(ORANGE).bold());
        spans.push(Span::raw(format!(" {what}  ")).fg(MUTED));
    }
    frame.render_widget(Line::from(spans), area);
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max - 1).chain(['…']).collect()
    }
}

fn fmt_count(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{:.1}K", n as f64 / 1e3),
        _ => format!("{:.1}M", n as f64 / 1e6),
    }
}

fn fmt_ms(ms: u64) -> String {
    let s = ms / 1000;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}
