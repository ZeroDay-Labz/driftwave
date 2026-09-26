mod api;
mod app;
mod genius;
mod art;
mod clipboard;
mod hls;
mod ipc;
mod library;
mod lyrics;
mod lyrics_window;
mod mpris;
mod notify;
mod player;
mod queue;
mod settings;
mod state;
mod stream;
mod tags;
mod ui;
mod viz;

use std::io::IsTerminal;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind};
use ratatui::DefaultTerminal;

use app::{App, Mode};
use api::SoundCloud;
use player::Player;

fn run(terminal: &mut DefaultTerminal, app: &mut App) -> Result<()> {
    // Steady ~60 fps: handle every input that arrives before the next frame
    // is due, then draw once, so bursts of mouse events can't stall redraws.
    const FRAME: Duration = Duration::from_millis(16);
    let mut next_frame = Instant::now();
    while !app.quit {
        app.tick();
        terminal.draw(|f| ui::draw(f, app))?;
        next_frame += FRAME;
        let now = Instant::now();
        if next_frame < now {
            next_frame = now; // fell behind; don't try to catch up
        }
        while let Some(wait) = next_frame.checked_duration_since(Instant::now())
            && event::poll(wait)?
        {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    app.on_key(key.code, key.modifiers)
                }
                Event::Mouse(m) => app.on_mouse(m),
                _ => {}
            }
            if app.quit {
                break;
            }
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--version" | "-V") => {
            println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Some("--help" | "-h") => {
            println!(
                "{} {}: {}\n\nUsage: driftwave [search words | soundcloud.com link]\n\n\
                 Press ? inside the app for all keys.",
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
                env!("CARGO_PKG_DESCRIPTION"),
            );
            return Ok(());
        }
        _ => {}
    }
    if let [flag, socket] = args.as_slice()
        && flag == "--lyrics-window"
    {
        return lyrics_window::run(socket);
    }

    if !std::io::stdout().is_terminal() {
        anyhow::bail!("driftwave is a terminal app: run it in a terminal (see --help)");
    }
    let sc = Arc::new(SoundCloud::new()?);
    let query = args.join(" ");
    let mut app = App::new(sc, Player::new()?, query, state::State::load());
    if app.input.is_empty() {
        app.start_screen();
    } else {
        app.mode = Mode::Normal;
        app.search();
    }

    let mut terminal = ratatui::init();
    // ratatui's panic hook restores the terminal but not mouse reporting.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
        hook(info);
    }));
    app.art = Some(art::Art::new()); // queries the terminal: before reading events
    crossterm::execute!(std::io::stdout(), EnableMouseCapture)?;
    let result = run(&mut terminal, &mut app);
    app.save_state();
    let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}
