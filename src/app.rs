use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use crate::ipc::{self, ToPlayer, ToWindow};
use crate::lyrics::{self, LyricsState};
use crate::lyrics_window;
use crate::mpris::{self, Mpris, SeekDirection};
use crate::notify::Notifier;
use crate::{player, ui};
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};
use ratatui::widgets::ListState;

use crate::api::{Item, OpenStream, Playlist, SoundCloud, Track, User};
use crate::player::{Player, Prepared};
use crate::queue::{Queue, Repeat};
use crate::settings::{self, FIELDS};
use crate::state::State;
use std::sync::atomic::{AtomicU64, Ordering};
use crate::viz::Spectrum;

pub enum Reply {
    /// Contents for one tab of a view.
    Items { view: u64, tab: usize, result: Result<Vec<Item>> },
    Resolved(Result<Box<Item>>),
    Audio { generation: u64, result: Result<Prepared> },
    Prefetched { track: u64, result: Result<OpenStream> },
    Lyrics { track: u64, result: Result<Option<lyrics::Found>> },
    Radio(Result<Vec<Track>>),
    /// A genre page's top tracks, which also give its artists and related tags.
    TagTop { view: u64, tag: String, result: Result<Vec<Track>> },
    Art { track: u64, result: Result<(std::path::PathBuf, image::DynamicImage)> },
    Downloaded(Result<std::path::PathBuf>),
    /// Another upload to play instead of a DRM-only track.
    Alternative { generation: u64, original: Box<Track>, result: Result<Option<Box<Track>>> },
}

/// An official download in progress.
struct Download {
    title: String,
    done: Arc<AtomicU64>,
    total: Arc<AtomicU64>,
}

pub struct Tab {
    pub name: &'static str,
    pub items: Vec<Item>,
    pub state: ListState,
    pub loading: bool,
    pub error: Option<String>,
}

#[derive(PartialEq)]
pub enum ViewKind {
    Browse,
    /// Live view of the play queue.
    Queue,
    Settings,
}

/// One screen: search results, an artist, an album/playlist, or the queue.
pub struct View {
    id: u64,
    pub kind: ViewKind,
    pub title: String,
    pub tabs: Vec<Tab>,
    pub tab: usize,
    /// For the queue view: the `Queue::version` it last showed.
    seen_version: u64,
}

impl View {
    pub fn current(&self) -> &Tab {
        &self.tabs[self.tab]
    }

    fn current_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.tab]
    }

    fn selected(&self) -> Option<&Item> {
        let tab = self.current();
        tab.items.get(tab.state.selected()?)
    }
}

#[derive(PartialEq)]
pub enum Mode {
    Normal,
    Search,
}

pub struct App {
    pub mode: Mode,
    pub input: String,
    pub views: Vec<View>,
    next_view_id: u64,

    /// What n/p/autoplay walk through: the list playback was started from.
    pub queue: Queue,
    /// Related tracks are being fetched for radio mode.
    radio_loading: bool,
    /// The queue ran out while radio was loading; play when it arrives.
    waiting_for_radio: bool,
    pub loading: bool,
    pub quality: Option<&'static str>,
    /// Bumped on every play request so stale downloads are ignored.
    generation: u64,
    /// The next track, already downloading so autoplay starts instantly.
    prefetch: Option<(u64, OpenStream)>,

    pub status: String,
    /// When `status` last changed, so messages can fade out.
    status_seen: String,
    status_since: Instant,
    pub player: Player,
    pub spectrum: Spectrum,
    /// Animation counter, advanced once per frame.
    pub frame: u64,
    /// Screen regions from the last draw, for mouse hit-testing.
    pub hits: ui::Hits,
    /// Time and row of the last list click, to detect double-clicks.
    last_click: Option<(Instant, usize)>,

    pub lyrics_open: bool,
    /// The "all keys" overlay.
    pub keys_open: bool,
    /// Selected row on the Settings screen, and the text being edited.
    pub settings_sel: usize,
    pub settings_edit: Option<String>,
    download: Option<Download>,
    /// Link to pop-out lyrics windows, started the first time one is opened.
    ipc: Option<ipc::Server>,
    /// (track id, lyrics status, timing corrections) last sent to the window.
    ipc_sent: Option<(u64, u8, crate::lyrics::LyricSync)>,
    ipc_pos_at: Instant,
    /// Per track id; fetched when a track starts playing.
    pub lyrics: HashMap<u64, LyricsState>,
    /// Scroll position of the lyrics popup (follows the song).
    pub lyrics_scroll: ui::LyricsScroll,
    /// Cover art, set up by `main` once the terminal is ready.
    pub art: Option<crate::art::Art>,
    /// Per track id: cached cover file (for notifications / media widget).
    pub art_files: HashMap<u64, std::path::PathBuf>,
    /// Media keys and the desktop's media controls.
    mpris: Option<Mpris>,
    notifier: Notifier,
    /// A track that started and still needs its notification (we wait
    /// briefly for its cover art).
    notify_pending: Option<(u64, Instant)>,

    /// Settings and history, saved to disk.
    pub state: State,
    state_dirty: bool,
    last_save: Instant,

    sc: Arc<SoundCloud>,
    tx: Sender<Reply>,
    rx: Receiver<Reply>,
    pub quit: bool,
}

impl App {
    pub fn new(sc: Arc<SoundCloud>, player: Player, query: String, state: State) -> Self {
        let (tx, rx) = mpsc::channel();
        player.set_volume(state.volume());
        let mut queue = Queue::default();
        (queue.shuffle, queue.repeat, queue.radio) = (state.shuffle, state.repeat, state.radio);
            App {
            mode: Mode::Search,
            input: query,
            views: Vec::new(),
            next_view_id: 0,
            queue,
            radio_loading: false,
            waiting_for_radio: false,
            loading: false,
            quality: None,
            generation: 0,
            prefetch: None,
            status: String::new(),
            status_seen: String::new(),
            status_since: Instant::now(),
            player,
            spectrum: Spectrum::new(),
            frame: 0,
            hits: ui::Hits::default(),
            last_click: None,
            lyrics_open: false,
            keys_open: false,
            settings_sel: 0,
            settings_edit: None,
            download: None,
            ipc: None,
            ipc_sent: None,
            ipc_pos_at: Instant::now(),
            lyrics: HashMap::new(),
            lyrics_scroll: ui::LyricsScroll::default(),
            art: None,
            art_files: HashMap::new(),
            mpris: Mpris::new(),
            notifier: Notifier::default(),
            notify_pending: None,
            state,
            state_dirty: false,
            last_save: Instant::now(),
            sc,
            tx,
            rx,
            quit: false,
        }
    }

    pub fn now_playing(&self) -> Option<&Track> {
        self.queue.current()
    }

    /// Run `f` on a background thread and deliver its reply to the UI loop.
    fn spawn(&self, f: impl FnOnce(&SoundCloud) -> Reply + Send + 'static) {
        let (sc, tx) = (self.sc.clone(), self.tx.clone());
        thread::spawn(move || {
            let _ = tx.send(f(&sc));
        });
    }

    fn push_view(&mut self, title: String, tabs: &[&'static str]) -> u64 {
        self.next_view_id += 1;
        let tabs = tabs
            .iter()
            .map(|name| Tab {
                name,
                items: Vec::new(),
                state: ListState::default(),
                loading: true,
                error: None,
            })
            .collect();
        self.views.push(View {
            id: self.next_view_id,
            kind: ViewKind::Browse,
            title,
            tabs,
            tab: 0,
            seen_version: u64::MAX,
        });
        self.next_view_id
    }

    /// Fill tab `tab` of view `view` from a background request.
    fn load<T: 'static>(
        &self,
        view: u64,
        tab: usize,
        fetch: impl FnOnce(&SoundCloud) -> Result<Vec<T>> + Send + 'static,
        wrap: fn(T) -> Item,
    ) {
        self.spawn(move |sc| Reply::Items {
            view,
            tab,
            result: fetch(sc).map(|v| v.into_iter().map(wrap).collect()),
        });
    }

    pub fn search(&mut self) {
        let q = self.input.trim().to_string();
        if q.is_empty() {
            return;
        }
        if let Some(tag) = q.strip_prefix('#').map(str::trim).filter(|t| !t.is_empty()) {
            self.state.last_query = q.clone();
            self.state_dirty = true;
            self.views.clear();
            self.open_tag(tag.to_string());
            return;
        }
        if q.starts_with("https://") {
            self.status = "Resolving link…".into();
            self.spawn(move |sc| Reply::Resolved(sc.resolve(&q).map(Box::new)));
            return;
        }
        self.state.last_query = q.clone();
        self.state_dirty = true;
        self.views.clear();
        let v = self.push_view(format!("Search “{q}”"), &["Tracks", "Artists", "Albums", "Playlists", "Genre"]);
        let q = Arc::new(q);
        let (q2, q3, q4, q5) = (q.clone(), q.clone(), q.clone(), q.clone());
        self.load(v, 0, move |sc| sc.search_tracks(&q), Item::Track);
        self.load(v, 1, move |sc| sc.search_users(&q2), Item::User);
        self.load(v, 2, move |sc| sc.search_albums(&q3), Item::Playlist);
        self.load(v, 3, move |sc| sc.search_playlists(&q4), Item::Playlist);
        // Tracks actually tagged with what was typed ("nerdcore", "folk punk").
        self.load(v, 4, move |sc| sc.tag_tracks(&q5), Item::Track);
    }

    /// What to show at startup when no search was given.
    pub fn start_screen(&mut self) {
        if self.state.history.is_empty() && !self.state.last_query.is_empty() {
            self.input = self.state.last_query.clone();
            self.mode = Mode::Normal;
            self.search();
        } else if !self.state.history.is_empty() {
            self.mode = Mode::Normal;
            self.open_history();
        }
    }

    /// Recently played tracks, refreshed from the API so they're playable.
    fn open_history(&mut self) {
        let ids: Vec<u64> = self.state.history.iter().map(|t| t.id).collect();
        let v = self.push_view("Recently played".into(), &["History"]);
        self.load(v, 0, move |sc| sc.tracks_by_ids(&ids), Item::Track);
    }

    /// A genre/tag page: top and newest tracks, its artists, playlists, and
    /// the tags that go with it.
    fn open_tag(&mut self, tag: String) {
        let tag = tag.trim().trim_start_matches('#').to_lowercase();
        if tag.is_empty() {
            return;
        }
        let v = self.push_view(format!("#{tag}"), &["Top", "Newest", "Artists", "Playlists", "Related tags"]);
        let (t1, t2, t3) = (tag.clone(), tag.clone(), tag.clone());
        self.spawn(move |sc| Reply::TagTop { view: v, result: sc.tag_tracks(&t1), tag: t1 });
        self.load(v, 1, move |sc| sc.tag_recent(&t2), Item::Track);
        self.load(v, 3, move |sc| sc.tag_playlists(&t3), Item::Playlist);
    }

    /// Open the genre of the selected track (or the selected tag).
    fn open_selected_tag(&mut self) {
        let tag = match self.views.last().and_then(View::selected) {
            Some(Item::Tag { name, .. }) => Some(name.clone()),
            Some(Item::Track(t)) => t.genre.clone().filter(|g| !g.trim().is_empty()),
            _ => self.now_playing().and_then(|t| t.genre.clone()),
        };
        match tag {
            Some(tag) => self.open_tag(tag),
            None => self.status = "This track has no genre; type #genre in search instead".into(),
        }
    }

    fn open_artist(&mut self, user: User) {
        let title = format!("{}{}", user.username, if user.verified == Some(true) { " ✔" } else { "" });
        let v = self.push_view(title, &["Popular", "All tracks", "Albums", "Playlists"]);
        let id = user.id;
        self.load(v, 0, move |sc| sc.user_top_tracks(id), Item::Track);
        self.load(v, 1, move |sc| sc.user_tracks(id), Item::Track);
        self.load(v, 2, move |sc| sc.user_albums(id), Item::Playlist);
        self.load(v, 3, move |sc| sc.user_playlists(id), Item::Playlist);
    }

    fn open_playlist(&mut self, playlist: Playlist) {
        let title = format!("{} · {}", playlist.title, playlist.user.username);
        let v = self.push_view(title, &["Tracks"]);
        self.load(v, 0, move |sc| sc.playlist_tracks(&playlist), Item::Track);
    }

    fn open(&mut self, item: Item) {
        match item {
            Item::User(u) => self.open_artist(u),
            Item::Tag { name, .. } => self.open_tag(name),
            Item::Playlist(p) => self.open_playlist(p),
            Item::Track(t) => {
                self.push_view(t.title.clone(), &["Track"]);
                let tab = &mut self.views.last_mut().unwrap().tabs[0];
                tab.items = vec![Item::Track(t)];
                tab.loading = false;
                tab.state.select(Some(0));
                self.play_selected();
            }
        }
    }

    /// Enter on the selected row: play a track, or drill into artist/set.
    fn activate(&mut self) {
        let Some(item) = self.views.last().and_then(View::selected).cloned() else { return };
        match item {
            Item::Track(_) => self.play_selected(),
            other => self.open(other),
        }
    }

    /// Jump to the artist of the selected row (or of what's playing).
    fn goto_artist(&mut self) {
        let user = match self.views.last().and_then(View::selected) {
            Some(Item::Track(t)) => t.user.clone(),
            Some(Item::Playlist(p)) => p.user.clone(),
            Some(Item::User(u)) => u.clone(),
            Some(Item::Tag { .. }) => return,
            None => match self.now_playing() {
                Some(t) => t.user.clone(),
                None => return,
            },
        };
        self.open_artist(user);
    }

    /// Make the current tab's tracks the queue and start at the selection.
    /// In the queue view, jump to the selected entry instead.
    fn play_selected(&mut self) {
        let Some(view) = self.views.last() else { return };
        let tab = view.current();
        let Some(sel) = tab.state.selected() else { return };
        if view.kind == ViewKind::Queue {
            if self.queue.jump(sel).is_some() {
                self.play_current();
            }
            return;
        }
        let mut pos = 0;
        let mut tracks = Vec::new();
        for (i, item) in tab.items.iter().enumerate() {
            if let Item::Track(t) = item {
                if i == sel {
                    pos = tracks.len();
                }
                tracks.push(t.clone());
            }
        }
        self.queue.set(tracks, pos);
        self.play_current();
    }

    /// Start playing `queue.current()`.
    fn play_current(&mut self) {
        let Some(track) = self.queue.current().cloned() else { return };
        self.generation += 1;
        self.loading = true;
        self.quality = None;
        self.player.stop();
        if track.is_drm_only() {
            // SoundCloud won't stream this one unencrypted; look for another
            // upload of the same song instead.
            self.status = format!("{} is DRM-protected; looking for another upload…", track.title);
            let generation = self.generation;
            self.spawn(move |sc| Reply::Alternative {
                generation,
                result: sc.playable_alternative(&track).map(|t| t.map(Box::new)),
                original: Box::new(track),
            });
            return;
        }
        self.status = format!("Loading {}…", track.title);
        self.lyrics_scroll = ui::LyricsScroll::default();
        self.request_lyrics(&track);
        if !self.art_files.contains_key(&track.id) {
            let t = track.clone();
            let keep_in = self.state.library.clone().filter(|_| self.state.save_art);
            self.spawn(move |sc| {
                let result = sc.artwork(&t);
                if let (Ok((cached, _)), Some(lib)) = (&result, keep_in) {
                    let _ = crate::library::save_cover(&lib, &t, cached);
                }
                Reply::Art { track: t.id, result }
            });
        }
        let generation = self.generation;
        match self.prefetch.take() {
            Some((id, stream)) if id == track.id => {
                self.spawn(move |_| Reply::Audio { generation, result: player::prepare(stream) });
            }
            other => {
                if let Some((_, stale)) = other {
                    stale.cancel();
                }
                self.spawn(move |sc| Reply::Audio {
                    generation,
                    result: sc.open_stream(&track).and_then(player::prepare),
                });
            }
        }
        self.maybe_extend_radio();
    }

    /// In radio mode, top the queue up with related tracks before it ends.
    fn maybe_extend_radio(&mut self) {
        if !self.queue.radio || self.radio_loading || self.queue.remaining() > 2 {
            return;
        }
        let Some(seed) = self.queue.current().map(|t| t.id) else { return };
        self.radio_loading = true;
        self.spawn(move |sc| Reply::Radio(sc.related_tracks(seed)));
    }

    /// Add the selected track to the queue (`next`: right after the current one).
    fn enqueue(&mut self, next: bool) {
        let Some(Item::Track(t)) = self.views.last().and_then(View::selected).cloned() else { return };
        if self.queue.current().is_none() {
            self.queue.set(vec![t], 0);
            self.play_current();
            return;
        }
        self.status = format!("{} {}", if next { "Playing next:" } else { "Added to queue:" }, t.title);
        if next {
            self.queue.push_next(t);
        } else {
            self.queue.push(t);
        }
        self.prefetch_next();
    }

    fn open_queue(&mut self) {
        if self.views.last().is_some_and(|v| v.kind == ViewKind::Queue) {
            return;
        }
        self.push_view("Queue".into(), &["Up next"]);
        let v = self.views.last_mut().unwrap();
        v.kind = ViewKind::Queue;
        v.tabs[0].loading = false;
        self.refresh_queue_view();
    }

    /// Mirror settings into `state` and save now and then when they change.
    fn sync_state(&mut self) {
        let current = (Some(self.player.volume()), self.queue.shuffle, self.queue.repeat, self.queue.radio);
        let saved = (self.state.volume_level, self.state.shuffle, self.state.repeat, self.state.radio);
        if current != saved {
            (self.state.volume_level, self.state.shuffle, self.state.repeat, self.state.radio) = current;
            self.state_dirty = true;
        }
        if self.state_dirty && self.last_save.elapsed() > Duration::from_secs(5) {
            self.save_state();
        }
    }

    pub fn save_state(&mut self) {
        self.state.save();
        self.state_dirty = false;
        self.last_save = Instant::now();
    }

    /// Keep an open queue view in sync with the queue.
    fn refresh_queue_view(&mut self) {
        let version = self.queue.version;
        let Some(view) = self.views.iter_mut().find(|v| v.kind == ViewKind::Queue) else { return };
        if view.seen_version == version {
            return;
        }
        let first_time = view.seen_version == u64::MAX;
        view.seen_version = version;
        let (tracks, current) = self.queue.in_order();
        let tab = &mut view.tabs[0];
        tab.items = tracks.into_iter().map(Item::Track).collect();
        let sel = tab.state.selected().filter(|_| !first_time).or(current);
        tab.state.select(sel.map(|s| s.min(tab.items.len().saturating_sub(1))));
    }

    /// Remove the selected entry from the queue (queue view only).
    fn remove_from_queue(&mut self) {
        let Some(view) = self.views.last() else { return };
        if view.kind != ViewKind::Queue {
            return;
        }
        if let Some(sel) = view.current().state.selected()
            && self.queue.remove(sel)
        {
            self.prefetch_next();
        }
    }

    /// Start downloading the track after the current one.
    fn prefetch_next(&mut self) {
        let Some(next) = self.queue.peek_next().cloned() else { return };
        if self.prefetch.as_ref().is_some_and(|(id, _)| *id == next.id) {
            return;
        }
        if let Some((_, old)) = self.prefetch.take() {
            old.cancel();
        }
        let id = next.id;
        self.spawn(move |sc| Reply::Prefetched { track: id, result: sc.open_stream(&next) });
    }

    fn request_lyrics(&mut self, track: &Track) {
        if self.lyrics.contains_key(&track.id) {
            return;
        }
        self.lyrics.insert(track.id, LyricsState::Loading);
        let (id, track) = (track.id, track.clone());
        let lookup = lyrics::Lookup {
            genius: self.state.genius,
            library: self.state.library.clone(),
            save: self.state.save_lyrics,
        };
        self.spawn(move |_| Reply::Lyrics { track: id, result: lyrics::find(&track, &lookup) });
    }

    fn toggle_lyrics(&mut self) {
        self.lyrics_open = !self.lyrics_open;
        if let Some(t) = self.now_playing().cloned() {
            self.request_lyrics(&t);
        }
    }

    /// Open the lyrics in their own terminal window, next to this one.
    fn pop_out_lyrics(&mut self) {
        if self.ipc.is_none() {
            match ipc::Server::start() {
                Ok(server) => self.ipc = Some(server),
                Err(e) => {
                    self.status = format!("Couldn't open lyrics window: {e:#}");
                    return;
                }
            }
        }
        if self.ipc.as_ref().is_some_and(ipc::Server::has_clients) {
            self.status = "The lyrics window is already open".into();
            return;
        }
        self.lyrics_open = false;
        if let Some(t) = self.now_playing().cloned() {
            self.request_lyrics(&t);
        }
        let socket = self.ipc.as_ref().unwrap().path().to_path_buf();
        self.status = match lyrics_window::launch(&socket, self.state.terminal.as_deref()) {
            Ok(term) => format!("Lyrics opened in a new {term} window"),
            Err(e) => format!("Couldn't open lyrics window: {e:#}"),
        };
    }

    /// Keep any pop-out lyrics window in step with playback.
    fn sync_lyrics_window(&mut self) {
        let Some(ipc) = self.ipc.take() else { return };
        for msg in ipc.poll() {
            match msg {
                ToPlayer::Seek { ms } => self.player.seek_to(Duration::from_millis(ms)),
                ToPlayer::Nudge { ms } => self.nudge_lyrics(ms),
                ToPlayer::Pin { line } => self.pin_lyric_line(line),
            }
        }
        let fresh = ipc.take_new_client();
        if ipc.has_clients() {
            let current = self.now_playing().map(|t| {
                let status = match self.lyrics.get(&t.id) {
                    None | Some(LyricsState::Loading) => 0,
                    Some(LyricsState::Ready(None)) => 1,
                    Some(LyricsState::Ready(Some(_))) => 2,
                    Some(LyricsState::Failed(_)) => 3,
                };
                (t.id, status, self.state.lyrics_sync.get(&t.id).cloned().unwrap_or_default())
            });
            if fresh || current != self.ipc_sent {
                self.ipc_sent = current;
                ipc.send(&match self.now_playing() {
                    Some(t) => ToWindow::Track {
                        title: t.title.clone(),
                        artist: t.publisher_artist().unwrap_or(&t.user.username).to_string(),
                        lyrics: self.lyrics.get(&t.id).cloned().unwrap_or(LyricsState::Loading),
                        duration_ms: t.duration,
                        sync: self.state.lyrics_sync.get(&t.id).cloned().unwrap_or_default(),
                    },
                    None => ToWindow::NoTrack,
                });
            }
            if fresh || self.ipc_pos_at.elapsed() > Duration::from_millis(100) {
                self.ipc_pos_at = Instant::now();
                ipc.send(&ToWindow::Pos {
                    ms: self.player.position().as_millis() as u64,
                    paused: self.player.is_paused() || self.loading,
                });
            }
        } else {
            self.ipc_sent = None;
        }
        self.ipc = Some(ipc);
    }

    /// Handle media keys and publish what's playing to the desktop.
    fn sync_mpris(&mut self) {
        let Some(mut m) = self.mpris.take() else { return };
        for event in m.events() {
            let paused = self.player.is_paused();
            match event {
                mpris::Event::Play if paused => self.player.toggle_pause(),
                mpris::Event::Pause | mpris::Event::Stop if !paused => self.player.toggle_pause(),
                mpris::Event::Toggle => self.player.toggle_pause(),
                mpris::Event::Next => self.next(),
                mpris::Event::Previous => self.previous(),
                mpris::Event::Seek(SeekDirection::Forward) => self.player.seek_by(10),
                mpris::Event::Seek(SeekDirection::Backward) => self.player.seek_by(-10),
                mpris::Event::SeekBy(dir, d) => {
                    let secs = d.as_secs() as i64;
                    self.player.seek_by(if dir == SeekDirection::Forward { secs } else { -secs });
                }
                mpris::Event::SetPosition(p) => self.player.seek_to(p.0),
                mpris::Event::SetVolume(v) => self.player.set_volume(v as f32),
                mpris::Event::Quit => self.quit = true,
                _ => {}
            }
        }
        m.set_volume(self.player.volume());
        let track = self.queue.current();
        let art = track.and_then(|t| self.art_files.get(&t.id)).map(|p| p.as_path());
        m.update(track, art, self.player.is_paused(), self.player.position());
        self.mpris = Some(m);
    }

    fn open_settings(&mut self) {
        if self.views.last().is_some_and(|v| v.kind == ViewKind::Settings) {
            return;
        }
        self.push_view("Settings".into(), &["Settings"]);
        let v = self.views.last_mut().unwrap();
        v.kind = ViewKind::Settings;
        v.tabs[0].loading = false;
    }

    /// Enter on a Settings row: toggle, cycle, start editing, or run it.
    fn activate_setting(&mut self) {
        let field = FIELDS[self.settings_sel];
        if let Some(text) = field.edit_text(&self.state) {
            self.settings_edit = Some(text);
        } else if field == settings::Field::OpenLibrary {
            match &self.state.library {
                Some(lib) => {
                    let _ = std::fs::create_dir_all(lib);
                    let _ = std::process::Command::new("xdg-open")
                        .arg(lib)
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .spawn();
                }
                None => self.status = "Set a library folder first".into(),
            }
        } else {
            if settings::toggle(field, &mut self.state) {
                self.settings_edit = Some(String::new()); // custom terminal command
            }
            self.settings_changed(field);
        }
    }

    fn commit_setting(&mut self) {
        let Some(text) = self.settings_edit.take() else { return };
        let field = FIELDS[self.settings_sel];
        match settings::commit(field, &text, &mut self.state) {
            Ok(msg) => {
                self.status = msg;
                self.settings_changed(field);
            }
            Err(e) => {
                self.status = e;
                self.settings_edit = Some(text); // let them fix it
            }
        }
    }

    fn settings_changed(&mut self, field: settings::Field) {
        self.state_dirty = true;
        if matches!(field, settings::Field::Genius | settings::Field::Library) {
            // Look again for tracks that had no lyrics before.
            self.lyrics.retain(|_, s| matches!(s, LyricsState::Ready(Some(_))));
            if let Some(t) = self.now_playing().cloned() {
                self.request_lyrics(&t);
            }
        }
    }

    /// Official download of the selected (or playing) track.
    fn download_selected(&mut self) {
        let track = match self.views.last().and_then(View::selected) {
            Some(Item::Track(t)) => Some(t.clone()),
            _ => self.now_playing().cloned(),
        };
        let Some(track) = track else { return };
        if self.download.is_some() {
            self.status = "A download is already running".into();
            return;
        }
        if !track.can_download() {
            self.status = "The uploader hasn't enabled downloads for this track".into();
            return;
        }
        let Some(token) = self.state.oauth_token.clone() else {
            self.status = "Downloads need your SoundCloud session: Settings (,) → SoundCloud session".into();
            return;
        };
        let Some(dir) = self.state.library.as_ref().map(|l| l.join("Downloads")) else {
            self.status = "Set a library folder in Settings (,) first".into();
            return;
        };
        let (done, total) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
        self.download = Some(Download { title: track.title.clone(), done: done.clone(), total: total.clone() });
        self.spawn(move |sc| {
            let result = std::fs::create_dir_all(&dir).map_err(Into::into).and_then(|_| {
                sc.download(&track, &token, &dir, |n, t| {
                    done.store(n, Ordering::Relaxed);
                    total.store(t.unwrap_or(0), Ordering::Relaxed);
                })
            });
            Reply::Downloaded(result)
        });
    }

    /// The current track's lyrics as plain text.
    fn lyrics_text(&self) -> Option<String> {
        let t = self.now_playing()?;
        let Some(LyricsState::Ready(Some(found))) = self.lyrics.get(&t.id) else { return None };
        match &found.lyrics {
            lyrics::Lyrics::Plain(text) => Some(text.clone()),
            lyrics::Lyrics::Synced(lines) => Some(lines.iter().map(|(_, l)| l.as_str()).collect::<Vec<_>>().join("\n")),
            lyrics::Lyrics::Instrumental => None,
        }
    }

    fn copy_lyrics(&mut self) {
        self.status = match self.lyrics_text() {
            Some(text) => {
                let how = crate::clipboard::copy(&text);
                format!("Copied {} lines of lyrics ({how})", text.lines().count())
            }
            None => "No lyrics to copy".into(),
        };
    }

    /// Shift the current track's lyrics timing.
    fn nudge_lyrics(&mut self, ms: i64) {
        let Some(id) = self.now_playing().map(|t| t.id) else { return };
        let sync = self.state.lyrics_sync.entry(id).or_default();
        sync.offset_ms += ms;
        self.status = format!("Lyrics timing {:+.1} s", sync.offset_ms as f64 / 1000.0);
        self.state_dirty = true;
    }

    /// The user says lyric line `line` is being sung right now.
    fn pin_lyric_line(&mut self, line: usize) {
        let Some(t) = self.now_playing() else { return };
        let Some(LyricsState::Ready(Some(found))) = self.lyrics.get(&t.id) else { return };
        let now = self.player.position().as_millis() as u64;
        let id = t.id;
        let sync = self.state.lyrics_sync.entry(id).or_default();
        match &found.lyrics {
            // Real timing: the whole song is off by the same amount.
            lyrics::Lyrics::Synced(lines) => {
                let Some((raw, _)) = lines.get(line) else { return };
                sync.offset_ms = now as i64 - *raw as i64;
                self.status = format!("Lyrics synced ({:+.1} s)", sync.offset_ms as f64 / 1000.0);
            }
            // Estimated timing: pin this line; drop pins that contradict it.
            lyrics::Lyrics::Plain(_) => {
                sync.offset_ms = 0;
                sync.anchors.retain(|&(l, t)| l != line && (l < line) == (t < now));
                sync.anchors.push((line, now));
                sync.anchors.sort();
                self.status = format!("Line pinned; {} pinned so far", sync.anchors.len());
            }
            lyrics::Lyrics::Instrumental => return,
        }
        self.lyrics_scroll.follow();
        self.state_dirty = true;
    }

    /// Genius search for what's playing (or the selected track).
    fn open_genius(&self) {
        let track = self.now_playing().or(match self.views.last().and_then(View::selected) {
            Some(Item::Track(t)) => Some(t),
            _ => None,
        });
        if let Some(t) = track {
            lyrics::open_on_genius(t);
        }
    }

    fn next(&mut self) {
        if self.queue.advance(false).is_some() {
            self.play_current();
        }
    }

    /// Previous track, or back to the start if we're more than 3 s in.
    fn previous(&mut self) {
        if self.player.position() > Duration::from_secs(3) {
            self.player.seek_to(Duration::ZERO);
        } else if self.queue.back().is_some() {
            self.play_current();
        }
    }

    fn handle_reply(&mut self, reply: Reply) {
        match reply {
            Reply::Items { view, tab, result } => {
                let Some(v) = self.views.iter_mut().find(|v| v.id == view) else { return };
                let t = &mut v.tabs[tab];
                t.loading = false;
                match result {
                    Ok(items) => {
                        t.state.select((!items.is_empty()).then_some(0));
                        t.items = items;
                    }
                    Err(e) => t.error = Some(format!("{e:#}")),
                }
            }
            Reply::Resolved(Ok(item)) => {
                self.status.clear();
                self.open(*item);
            }
            Reply::Resolved(Err(e)) => self.status = format!("Couldn't open link: {e:#}"),
            Reply::Lyrics { track, result } => {
                let state = match result {
                    Ok(found) => LyricsState::Ready(found),
                    Err(e) => LyricsState::Failed(format!("{e:#}")),
                };
                self.lyrics.insert(track, state);
            }
            Reply::Audio { generation, result } if generation != self.generation => {
                if let Ok(stale) = result {
                    stale.cancel();
                }
            }
            Reply::Audio { result: Ok(prepared), .. } => {
                self.loading = false;
                if let Some(t) = self.queue.current() {
                    self.state.record_play(t);
                    self.state_dirty = true;
                    self.notify_pending = Some((t.id, Instant::now()));
                }
                self.quality = Some(prepared.label());
                self.player.play(prepared);
                // Keep explanations (e.g. "playing another upload"); drop "Loading…".
                if self.status.starts_with("Loading ") {
                    self.status.clear();
                }
                self.prefetch_next();
            }
            Reply::Prefetched { track, result } => {
                let wanted = self.queue.peek_next().map(|t| t.id);
                match result {
                    Ok(stream) if wanted == Some(track) => {
                        if let Some((_, old)) = self.prefetch.replace((track, stream)) {
                            old.cancel();
                        }
                    }
                    Ok(stream) => stream.cancel(),
                    Err(_) => {} // it'll be retried when that track actually plays
                }
            }
            Reply::Art { track, result: Ok((path, image)) } => {
                self.art_files.insert(track, path);
                if let Some(art) = &mut self.art {
                    art.insert(track, image);
                }
            }
            Reply::Art { result: Err(_), .. } => {}
            Reply::Downloaded(result) => {
                self.download = None;
                self.status = match result {
                    Ok(path) => format!("⤓ Saved {}", path.display()),
                    Err(e) => format!("Download failed: {e:#}"),
                };
            }
            Reply::TagTop { view, tag, result } => {
                let Some(v) = self.views.iter_mut().find(|v| v.id == view) else { return };
                match result {
                    Ok(tracks) => {
                        let artists: Vec<Item> = crate::tags::artists(&tracks).into_iter().map(Item::User).collect();
                        let related: Vec<Item> = crate::tags::related(&tracks, &tag, 60)
                            .into_iter()
                            .map(|(name, count)| Item::Tag { name, count })
                            .collect();
                        let top: Vec<Item> = tracks.into_iter().map(Item::Track).collect();
                        for (i, items) in [(0, top), (2, artists), (4, related)] {
                            let t = &mut v.tabs[i];
                            t.loading = false;
                            t.state.select((!items.is_empty()).then_some(0));
                            t.items = items;
                        }
                    }
                    Err(e) => {
                        for i in [0, 2, 4] {
                            v.tabs[i].loading = false;
                            v.tabs[i].error = Some(format!("{e:#}"));
                        }
                    }
                }
            }
            Reply::Radio(result) => {
                self.radio_loading = false;
                let added = result.map(|t| self.queue.extend_radio(t)).unwrap_or(0);
                if self.waiting_for_radio {
                    self.waiting_for_radio = false;
                    if added > 0 && self.queue.advance(true).is_some() {
                        self.play_current();
                    } else {
                        self.status = "Radio: no more related tracks".into();
                    }
                } else if added > 0 {
                    self.prefetch_next();
                }
            }
            Reply::Audio { result: Err(e), .. } => {
                self.loading = false;
                self.status = format!("Can't play this track: {e:#}");
            }
            Reply::Alternative { generation, .. } if generation != self.generation => {}
            Reply::Alternative { original, result, .. } => match result {
                Ok(Some(alt)) => {
                    self.queue.replace_current(*alt);
                    self.play_current();
                    if let Some(t) = self.queue.current() {
                        self.status = format!(
                            "\"{}\" is DRM-protected; playing {}'s upload instead",
                            original.title, t.user.username
                        );
                    }
                }
                _ => {
                    self.loading = false;
                    self.status = format!("\"{}\" is DRM-protected and no other upload plays; skipping", original.title);
                    if self.queue.advance(false).is_some() {
                        let msg = std::mem::take(&mut self.status);
                        self.play_current();
                        self.status = msg;
                    }
                }
            },
        }
    }

    pub fn on_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        if mods.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        if self.mode == Mode::Search {
            match code {
                KeyCode::Enter => {
                    self.mode = Mode::Normal;
                    self.search();
                }
                KeyCode::Esc => self.mode = Mode::Normal,
                KeyCode::Backspace => {
                    self.input.pop();
                }
                KeyCode::Char(c) => self.input.push(c),
                _ => {}
            }
            return;
        }

        if let Some(buf) = &mut self.settings_edit {
            match code {
                KeyCode::Enter => self.commit_setting(),
                KeyCode::Esc => self.settings_edit = None,
                KeyCode::Backspace => {
                    buf.pop();
                }
                KeyCode::Char(c) => buf.push(c),
                _ => {}
            }
            return;
        }
        if self.keys_open {
            self.keys_open = false;
            return;
        }
        if code == KeyCode::Char('?') {
            self.keys_open = true;
            return;
        }

        if self.lyrics_open {
            match code {
                KeyCode::Esc | KeyCode::Char('l') => self.lyrics_open = false,
                KeyCode::Down | KeyCode::Char('j') => self.lyrics_scroll.scroll_by(1),
                KeyCode::Up | KeyCode::Char('k') => self.lyrics_scroll.scroll_by(-1),
                KeyCode::PageDown => self.lyrics_scroll.scroll_by(10),
                KeyCode::PageUp => self.lyrics_scroll.scroll_by(-10),
                KeyCode::Char('f') => self.lyrics_scroll.follow(),
                _ => {}
            }
            if matches!(
                code,
                KeyCode::Esc | KeyCode::Char('l' | 'j' | 'k' | 'f') | KeyCode::Up | KeyCode::Down
                    | KeyCode::PageUp | KeyCode::PageDown
            ) {
                return;
            }
        }

        if self.views.last().is_some_and(|v| v.kind == ViewKind::Settings) {
            match code {
                KeyCode::Up | KeyCode::Char('k') => self.settings_sel = self.settings_sel.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => self.settings_sel = (self.settings_sel + 1).min(FIELDS.len() - 1),
                KeyCode::Enter => self.activate_setting(),
                _ => {}
            }
            if matches!(code, KeyCode::Up | KeyCode::Down | KeyCode::Enter | KeyCode::Char('j' | 'k')) {
                return;
            }
        }

        if let Some(view) = self.views.last_mut() {
            let n_tabs = view.tabs.len();
            match code {
                KeyCode::Tab => view.tab = (view.tab + 1) % n_tabs,
                KeyCode::BackTab => view.tab = (view.tab + n_tabs - 1) % n_tabs,
                KeyCode::Char(c @ '1'..='9') if (c as usize - '1' as usize) < n_tabs => {
                    view.tab = c as usize - '1' as usize;
                }
                KeyCode::Down | KeyCode::Char('j') => view.current_mut().state.select_next(),
                KeyCode::Up | KeyCode::Char('k') => view.current_mut().state.select_previous(),
                KeyCode::PageDown => view.current_mut().state.scroll_down_by(10),
                KeyCode::PageUp => view.current_mut().state.scroll_up_by(10),
                KeyCode::Char('g') | KeyCode::Home => view.current_mut().state.select_first(),
                KeyCode::Char('G') | KeyCode::End => view.current_mut().state.select_last(),
                _ => {}
            }
        }

        match code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('/') | KeyCode::Char('s') => {
                self.mode = Mode::Search;
                self.input.clear();
            }
            KeyCode::Enter => self.activate(),
            KeyCode::Char('a') => self.goto_artist(),
            KeyCode::Esc | KeyCode::Backspace if self.views.len() > 1 => {
                self.views.pop();
            }
            KeyCode::Char(' ') => self.player.toggle_pause(),
            KeyCode::Char('n') => self.next(),
            KeyCode::Char('p') => self.previous(),
            KeyCode::Char('e') => self.enqueue(false),
            KeyCode::Char('E') => self.enqueue(true),
            KeyCode::Char('u') => self.open_queue(),
            KeyCode::Char('h') => self.open_history(),
            KeyCode::Char('#') => {
                self.mode = Mode::Search;
                self.input = "#".into();
            }
            KeyCode::Char('t') => self.open_selected_tag(),
            KeyCode::Char(',') | KeyCode::F(2) => self.open_settings(),
            KeyCode::Char('D') => self.download_selected(),
            KeyCode::Char('d') => self.remove_from_queue(),
            KeyCode::Char('x') => self.toggle_shuffle(),
            KeyCode::Char('r') => self.cycle_repeat(),
            KeyCode::Char('R') => self.toggle_radio(),
            KeyCode::Right => self.player.seek_by(10),
            KeyCode::Left => self.player.seek_by(-10),
            KeyCode::Char('l') => self.toggle_lyrics(),
            KeyCode::Char('[') => self.nudge_lyrics(-500),
            KeyCode::Char(']') => self.nudge_lyrics(500),
            KeyCode::Char('c') => self.copy_lyrics(),
            KeyCode::Char('L') => self.pop_out_lyrics(),
            KeyCode::Char('o') => self.open_genius(),
            KeyCode::Char('+') | KeyCode::Char('=') => self.player.change_volume(0.05),
            KeyCode::Char('-') => self.player.change_volume(-0.05),
            _ => {}
        }
    }

    pub fn on_mouse(&mut self, m: MouseEvent) {
        let at = Position::new(m.column, m.row);
        let hits = &self.hits;
        let in_list = hits.list.contains(at);
        let in_player = hits.now_playing.contains(at);

        match m.kind {
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                let down = m.kind == MouseEventKind::ScrollDown;
                if self.lyrics_open && hits.popup.contains(at) {
                    self.lyrics_scroll.scroll_by(if down { 2 } else { -2 });
                } else if in_list && let Some(view) = self.views.last_mut() {
                    let state = &mut view.current_mut().state;
                    if down { state.scroll_down_by(3) } else { state.scroll_up_by(3) }
                } else if in_player {
                    self.player.change_volume(if down { -0.05 } else { 0.05 });
                }
            }
            // Click or drag on the progress bar seeks.
            MouseEventKind::Down(MouseButton::Left) | MouseEventKind::Drag(MouseButton::Left)
                if hits.progress.contains(at)
                    || (matches!(m.kind, MouseEventKind::Drag(_))
                        && (hits.progress.y..hits.progress.bottom()).contains(&m.row)) =>
            {
                let bar = hits.progress;
                if let Some(t) = self.now_playing() && bar.width > 1 {
                    let x = m.column.clamp(bar.x, bar.right() - 1) - bar.x;
                    let frac = x as f64 / (bar.width - 1) as f64;
                    self.player.seek_to(Duration::from_millis((t.duration as f64 * frac) as u64));
                }
            }
            MouseEventKind::Down(MouseButton::Left) => self.on_click(at),
            // Right-click a lyric line: "this line is being sung now".
            MouseEventKind::Down(MouseButton::Right) if self.lyrics_open => {
                if let Some(&(_, _, line)) = self.hits.lyric_lines.iter().find(|(y, _, _)| *y == at.y) {
                    self.pin_lyric_line(line);
                }
            }
            _ => {}
        }
    }

    fn on_click(&mut self, at: Position) {
        if self.keys_open {
            self.keys_open = false;
            return;
        }
        let hits = &self.hits;
        if hits.lyrics_button.contains(at) {
            self.toggle_lyrics();
            return;
        }
        if hits.genius_button.contains(at) {
            self.open_genius();
            return;
        }
        if hits.popout_button.contains(at) || hits.popup_popout.contains(at) {
            return self.pop_out_lyrics();
        }
        if hits.shuffle_button.contains(at) {
            return self.toggle_shuffle();
        }
        if hits.repeat_button.contains(at) {
            return self.cycle_repeat();
        }
        if hits.radio_button.contains(at) {
            return self.toggle_radio();
        }
        if hits.queue_button.contains(at) {
            return self.open_queue();
        }
        if hits.settings_button.contains(at) {
            return self.open_settings();
        }
        if self.settings_edit.is_some() && !hits.settings_rows.contains(at) {
            self.settings_edit = None;
        }
        if self.lyrics_open {
            if hits.popup_close.contains(at) || !hits.popup.contains(at) {
                self.lyrics_open = false;
            } else if hits.lyrics_follow.contains(at) {
                self.lyrics_scroll.follow();
            } else if hits.popup_copy.contains(at) {
                self.copy_lyrics();
            } else if let Some(&(_, ms, _)) = hits.lyric_lines.iter().find(|(y, _, _)| *y == at.y) {
                // Click a line to jump there.
                self.player.seek_to(Duration::from_millis(ms));
                self.lyrics_scroll.follow();
            }
            return;
        }
        if hits.search.contains(at) {
            self.mode = Mode::Search;
            return;
        }
        self.mode = Mode::Normal;

        if at.y == hits.tabs_y
            && let Some(&(_, _, i)) = hits.tabs.iter().find(|(x0, x1, _)| (*x0..*x1).contains(&at.x))
            && let Some(view) = self.views.last_mut()
        {
            view.tab = i;
        } else if at.y + 1 == hits.list.y
            && let Some(&(_, _, i)) = hits.crumbs.iter().find(|(x0, x1, _)| (*x0..*x1).contains(&at.x))
        {
            self.views.truncate(i + 1);
        } else if hits.settings_rows.contains(at) {
            let row = (at.y - hits.settings_rows.y) as usize;
            if row < FIELDS.len() {
                let now = Instant::now();
                let double = matches!(self.last_click,
                    Some((t, i)) if i == row && now.duration_since(t) < Duration::from_millis(400));
                self.last_click = if double { None } else { Some((now, row)) };
                self.settings_sel = row;
                if double {
                    self.activate_setting();
                }
            }
        } else if let Some((_, genre)) = hits.genre_links.iter().find(|(r, _)| r.contains(at)) {
            // Click a track's genre to open that genre.
            let genre = genre.clone();
            self.open_tag(genre);
        } else if hits.list.contains(at) {
            self.click_row(hits.list, at);
        } else if hits.spectrum.contains(at) {
            self.player.toggle_pause();
        }
    }

    fn toggle_shuffle(&mut self) {
        self.queue.toggle_shuffle();
        self.status = format!("Shuffle {}", if self.queue.shuffle { "on" } else { "off" });
        self.prefetch_next();
    }

    fn cycle_repeat(&mut self) {
        self.queue.repeat = self.queue.repeat.cycle();
        self.status = match self.queue.repeat {
            Repeat::Off => "Repeat off",
            Repeat::All => "Repeat all",
            Repeat::One => "Repeat one track",
        }
        .into();
        self.prefetch_next();
    }

    fn toggle_radio(&mut self) {
        self.queue.radio = !self.queue.radio;
        self.status = format!("Radio {}", if self.queue.radio { "on: related tracks keep playing" } else { "off" });
        self.maybe_extend_radio();
    }

    /// Single click selects a row; clicking it again quickly plays/opens it.
    fn click_row(&mut self, list: Rect, at: Position) {
        let Some(view) = self.views.last_mut() else { return };
        let tab = view.current_mut();
        let idx = tab.state.offset() + (at.y - list.y) as usize;
        if idx >= tab.items.len() {
            return;
        }
        tab.state.select(Some(idx));
        let now = Instant::now();
        let double = matches!(self.last_click,
            Some((t, i)) if i == idx && now.duration_since(t) < Duration::from_millis(400));
        self.last_click = if double { None } else { Some((now, idx)) };
        if double {
            self.activate();
        }
    }

    pub fn tick(&mut self) {
        self.frame = self.frame.wrapping_add(1);
        while let Ok(reply) = self.rx.try_recv() {
            self.handle_reply(reply);
        }
        if let Some(d) = &self.download {
            let (done, total) = (d.done.load(Ordering::Relaxed), d.total.load(Ordering::Relaxed));
            self.status = match total {
                0 => format!("⤓ Downloading {}… {:.1} MB", d.title, done as f64 / 1e6),
                t => format!("⤓ Downloading {}… {}%", d.title, done * 100 / t),
            };
        }
        if let Some(e) = self.player.take_error() {
            self.status = format!("Playback stopped: {e}");
        }
        if !self.loading && !self.waiting_for_radio && self.player.finished() {
            if self.queue.advance(true).is_some() {
                self.play_current();
            } else if self.radio_loading {
                self.waiting_for_radio = true;
                self.status = "Radio: finding related tracks…".into();
            } else {
                self.player.stop();
            }
        }
        self.refresh_queue_view();
        self.sync_state();
        self.sync_lyrics_window();
        self.sync_mpris();
        self.send_notification();
        self.expire_status();
    }

    /// Messages clear after a few seconds, unless they describe something
    /// still going on (loading, downloading, finding radio tracks).
    fn expire_status(&mut self) {
        if self.status != self.status_seen {
            self.status_seen = self.status.clone();
            self.status_since = Instant::now();
        } else if !self.status.is_empty()
            && !self.loading
            && self.download.is_none()
            && !self.waiting_for_radio
            && self.status_since.elapsed() > Duration::from_secs(5)
        {
            self.status.clear();
        }
    }

    /// Announce a new track once its cover is here (or after 1.5 s without).
    fn send_notification(&mut self) {
        let Some((id, since)) = self.notify_pending else { return };
        let cover = self.art_files.get(&id);
        if cover.is_none() && since.elapsed() < Duration::from_millis(1500) {
            return;
        }
        self.notify_pending = None;
        let enabled = self.state.notifications && std::env::var_os("DRIFTWAVE_NO_NOTIFY").is_none();
        if let Some(t) = self.queue.current().filter(|t| t.id == id && enabled) {
            self.notifier.show(t, cover.map(|p| p.as_path()));
        }
    }
}

