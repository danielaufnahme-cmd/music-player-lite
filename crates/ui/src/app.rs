//! The controller: owns the app state on the UI thread, talks to the server
//! on the tokio runtime and pushes results into the Slint globals.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::{Duration, Instant};

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use crate::api::{Api, Download, Lyrics, Playlist, PlaylistPreview, SyncStatus, Track};
use crate::audio::Audio;
use crate::palette::{CoverPalette, DesktopScheme};
use crate::settings::Settings;
use crate::waves::WaveClock;
use crate::{AppWindow, CardItem, Lib, Nav, Page, Player, PlaylistItem, SectionItem, TrackItem};

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
    /// Latest download per song, read by the `Lib.dl-state` binding.
    pub static DL: RefCell<HashMap<String, (i32, f32)>> = RefCell::new(HashMap::new());
    /// Wave clock, read by the seek line binding.
    pub static CLOCK: RefCell<WaveClock> = RefCell::new(WaveClock::default());
}

/// Run `f` with the app state. Re-entrant calls are deferred to the next
/// event-loop turn instead of panicking.
pub fn with_app(f: impl FnOnce(&mut App) + 'static) {
    let f = APP.with(|a| match a.try_borrow_mut() {
        Ok(mut g) => {
            if let Some(app) = g.as_mut() {
                f(app);
            }
            None
        }
        Err(_) => Some(f),
    });
    if let Some(f) = f {
        slint::Timer::single_shot(Duration::ZERO, move || with_app(f));
    }
}

/// Synchronous access (for callbacks that must return a value).
pub fn try_with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|a| a.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

/// From any thread: run `f` on the UI thread with the app state.
pub fn post(f: impl FnOnce(&mut App) + Send + 'static) {
    let _ = slint::invoke_from_event_loop(move || with_app(f));
}

#[derive(Clone, Debug, PartialEq)]
pub enum Route {
    Home,
    Search,
    Library,
    Downloads,
    Playlist(i64),
    Artist(String),
    Album(String, String),
    Settings,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Repeat {
    Off,
    All,
    One,
}

#[derive(Default)]
pub struct Queue {
    pub tracks: Vec<Track>,
    pub index: i64,
    pub original: Vec<Track>,
    pub shuffle: bool,
}

pub enum MenuCtx {
    None,
    Track(Track, Option<i64>),
    Theme,
    Sort,
    Playlist,
}

pub enum DialogCtx {
    None,
    DeletePlaylist(Playlist),
    RenamePlaylist(Playlist),
    RemoveTrack(Track),
    Picker(Track, Vec<Playlist>),
    NewPlaylist { allow_import: bool, then_add: Option<Track> },
    ImportInto(Playlist),
    Devices,
    RemoveDevice(String),
}

pub struct Import {
    pub seq: u64,
    pub url: String,
    pub preview: Option<PlaylistPreview>,
    pub existing: Option<HashSet<String>>,
    pub name_edited: bool,
}

#[derive(Default)]
pub struct SyncState {
    pub available: Option<bool>,
    pub status: Option<SyncStatus>,
    pub running: bool,
    pub silent: bool,
    pub new_songs: i64,
    pub errors: Vec<String>,
    pub settle: Option<slint::Timer>,
}

pub struct App {
    pub win: AppWindow,
    pub api: Api,
    pub rt: tokio::runtime::Handle,
    pub settings: Settings,

    pub nav: Vec<Route>,

    // library data
    pub library: Vec<Track>,
    pub library_loaded: bool,
    pub library_error: Option<String>,
    pub filtered: Vec<Track>,
    pub recent: Option<Vec<Track>>,
    pub top: Option<Vec<Track>>,
    pub recs: Option<Vec<Track>>,
    pub recs_loading: bool,
    pub playlists: Option<Vec<Playlist>>,
    pub artists: Vec<crate::api::Artist>,
    pub albums: Vec<crate::api::Album>,
    pub home_lists: HashMap<String, Vec<Track>>,
    pub load_seq: u64,

    // search
    pub search_seq: u64,
    pub search_results: Vec<Track>,
    pub preview: Option<(String, PlaylistPreview)>,

    // playlist / track list pages
    pub playlist: Option<Playlist>,
    pub pl_seq: u64,
    pub tracklist: Vec<Track>,

    // downloads
    pub downloads: Vec<Download>,
    pub selected: HashSet<String>,
    pub anchor: Option<String>,
    pub refresh_debounce: Option<slint::Timer>,

    // player
    pub audio: Audio,
    pub queue: Queue,
    pub repeat: Repeat,
    pub playing: bool,
    pub position: f64,
    pub duration: f64,
    pub volume: f32,
    pub lyrics: HashMap<String, Lyrics>,
    pub current_lyrics: Option<Lyrics>,
    pub palettes: HashMap<String, CoverPalette>,
    pub cover_palette: Option<CoverPalette>,
    pub preloaded: Option<String>,

    // theme
    pub desktop: Option<DesktopScheme>,

    // waves
    pub wave_timer: slint::Timer,
    pub wave_last: Instant,
    pub wave_bufs: crate::visuals::Buffers,

    // menus / dialogs
    pub menu: MenuCtx,
    pub dialog: DialogCtx,
    pub import: Import,
    pub dev_timer: Option<slint::Timer>,
    pub sync: SyncState,
    pub private_listening: Option<bool>,
    pub snack_timer: slint::Timer,

    // models
    pub m_library: Rc<VecModel<TrackItem>>,
    pub m_search: Rc<VecModel<TrackItem>>,
    pub m_playlist: Rc<VecModel<TrackItem>>,
    pub m_tracklist: Rc<VecModel<TrackItem>>,
    pub m_upnext: Rc<VecModel<TrackItem>>,
}

pub fn track_item(t: &Track) -> TrackItem {
    TrackItem {
        id: t.video_id.as_str().into(),
        title: if t.title.is_empty() { "Unknown title".into() } else { t.title.as_str().into() },
        artist: t.artist.as_str().into(),
        album: t.album.as_str().into(),
        duration: if t.duration_sec > 0 { fmt_duration(t.duration_sec as f64).into() } else { SharedString::new() },
        downloaded: t.downloaded,
    }
}

pub fn fmt_duration(secs: f64) -> String {
    let s = secs.max(0.0) as i64;
    let (h, m, s) = (s / 3600, (s / 60) % 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

pub fn fmt_total(secs: i64) -> String {
    if secs >= 3600 {
        format!("{} hr {} min", secs / 3600, (secs / 60) % 60)
    } else if secs >= 60 {
        format!("{} min", secs / 60)
    } else {
        format!("{secs} sec")
    }
}

pub fn plural(n: i64, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

pub fn greeting(hour: u32) -> &'static str {
    match hour {
        0..=4 => "Good night",
        5..=11 => "Good morning",
        12..=17 => "Good afternoon",
        _ => "Good evening",
    }
}

pub fn local_hour() -> u32 {
    #[cfg(unix)]
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if !libc::localtime_r(&t, &mut tm).is_null() {
            return tm.tm_hour as u32;
        }
    }
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    ((secs / 3600) % 24) as u32
}

/// Give freed memory back to the OS a moment after bursts (page changes, startup).
pub fn trim_heap_soon() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        thread_local!(static T: slint::Timer = slint::Timer::default());
        T.with(|t| {
            t.start(slint::TimerMode::SingleShot, Duration::from_secs(3), || unsafe {
                libc::malloc_trim(0);
            })
        });
    }
}

fn model<T: Clone + 'static>(v: Vec<T>) -> ModelRc<T> {
    ModelRc::new(VecModel::from(v))
}

fn strings(v: &[String]) -> ModelRc<SharedString> {
    model(v.iter().map(|s| SharedString::from(s.as_str())).collect())
}

impl App {
    pub fn install(win: AppWindow, api: Api, rt: tokio::runtime::Handle) {
        let settings = Settings::load();
        let null_sink = std::env::var("GEONIX_NULL_AUDIO").map(|v| v == "1").unwrap_or(false);
        let audio = Audio::new(null_sink, |e| post(move |a| a.on_audio(e)));
        let volume = settings.start_volume();
        let app = App {
            win: win.clone_strong(),
            api,
            rt,
            settings,
            nav: vec![Route::Home],
            library: Vec::new(),
            library_loaded: false,
            library_error: None,
            filtered: Vec::new(),
            recent: None,
            top: None,
            recs: None,
            recs_loading: false,
            playlists: None,
            artists: Vec::new(),
            albums: Vec::new(),
            home_lists: HashMap::new(),
            load_seq: 0,
            search_seq: 0,
            search_results: Vec::new(),
            preview: None,
            playlist: None,
            pl_seq: 0,
            tracklist: Vec::new(),
            downloads: Vec::new(),
            selected: HashSet::new(),
            anchor: None,
            refresh_debounce: None,
            audio,
            queue: Queue { index: -1, ..Default::default() },
            repeat: Repeat::Off,
            playing: false,
            position: 0.0,
            duration: 0.0,
            volume,
            lyrics: HashMap::new(),
            current_lyrics: None,
            palettes: HashMap::new(),
            cover_palette: None,
            preloaded: None,
            desktop: None,
            wave_timer: slint::Timer::default(),
            wave_last: Instant::now(),
            wave_bufs: Default::default(),
            menu: MenuCtx::None,
            dialog: DialogCtx::None,
            import: Import { seq: 0, url: String::new(), preview: None, existing: None, name_edited: false },
            dev_timer: None,
            sync: SyncState::default(),
            private_listening: None,
            snack_timer: slint::Timer::default(),
            m_library: Rc::new(VecModel::default()),
            m_search: Rc::new(VecModel::default()),
            m_playlist: Rc::new(VecModel::default()),
            m_tracklist: Rc::new(VecModel::default()),
            m_upnext: Rc::new(VecModel::default()),
        };
        {
            let lib = win.global::<Lib>();
            lib.set_library_tracks(ModelRc::from(app.m_library.clone()));
            lib.set_search_results(ModelRc::from(app.m_search.clone()));
            lib.set_pl_tracks(ModelRc::from(app.m_playlist.clone()));
            lib.set_tl_tracks(ModelRc::from(app.m_tracklist.clone()));
            lib.set_greeting(greeting(local_hour()).into());
            let p = win.global::<Player>();
            p.set_up_next(ModelRc::from(app.m_upnext.clone()));
            p.set_volume(volume * 100.0);
        }
        app.audio.set_volume(volume);
        crate::wire::wire(&win);
        APP.with(|a| *a.borrow_mut() = Some(app));
        with_app(|a| {
            a.apply_theme();
            a.refresh_all();
            a.load_recs();
            a.start_events();
            a.load_private_listening(20);
            a.sync_startup(20);
            a.watch_desktop_scheme();
            a.render_static();
            a.prefs_init();
        });
        // after the first loads settle
        slint::Timer::single_shot(Duration::from_secs(5), trim_heap_soon);
    }

    pub fn spawn<F>(&self, f: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        self.rt.spawn(f);
    }

    pub fn snack(&mut self, text: impl Into<String>) {
        let ui = self.win.global::<crate::Ui>();
        ui.set_snack(text.into().into());
        ui.set_snack_visible(true);
        self.snack_timer.start(slint::TimerMode::SingleShot, Duration::from_millis(4000), || {
            with_app(|a| a.win.global::<crate::Ui>().set_snack_visible(false));
        });
    }

    // ------------------------------------------------------------ navigation

    pub fn route(&self) -> Route {
        self.nav.last().cloned().unwrap_or(Route::Home)
    }

    pub fn go(&mut self, r: Route) {
        self.nav = vec![r];
        self.show_route();
    }

    pub fn push(&mut self, r: Route) {
        self.nav.push(r);
        self.show_route();
    }

    pub fn pop(&mut self) {
        if self.nav.len() > 1 {
            self.nav.pop();
            self.show_route();
        }
    }

    pub fn show_route(&mut self) {
        let route = self.route();
        let nav = self.win.global::<Nav>();
        nav.set_can_pop(self.nav.len() > 1);
        nav.set_playlist_id(if let Route::Playlist(id) = route { id as i32 } else { -1 });
        nav.set_page(match route {
            Route::Home => Page::Home,
            Route::Search => Page::Search,
            Route::Library => Page::Library,
            Route::Downloads => Page::Downloads,
            Route::Playlist(_) => Page::Playlist,
            Route::Artist(_) => Page::Artist,
            Route::Album(..) => Page::Album,
            Route::Settings => Page::Settings,
        });
        match route {
            Route::Playlist(id) => self.load_playlist(id, true),
            Route::Artist(_) | Route::Album(..) => self.update_tracklist(),
            Route::Home => self.win.global::<crate::Lib>().set_greeting(greeting(local_hour()).into()),
            Route::Settings => self.settings_page_opened(),
            Route::Search => {
                let w = self.win.as_weak();
                slint::Timer::single_shot(Duration::from_millis(30), move || {
                    if let Some(w) = w.upgrade() {
                        w.invoke_focus_search();
                    }
                });
            }
            _ => {}
        }
        if route != Route::Search {
            self.win.invoke_focus_keys();
        }
        if route != Route::Downloads {
            self.selected.clear();
            self.anchor = None;
        }
        trim_heap_soon();
    }

    // ------------------------------------------------------------ data loading

    /// Reload everything that shows library contents (refreshLibrary).
    pub fn refresh_all(&mut self) {
        self.load_seq += 1;
        let seq = self.load_seq;
        let api = self.api.clone();
        let sort = self.settings_sort();
        self.spawn(async move {
            let (lib, recent, top, artists, albums, playlists) =
                tokio::join!(api.tracks(&sort), api.tracks("recent"), api.tracks("plays"), api.artists(), api.albums(), api.playlists());
            post(move |a| {
                if seq != a.load_seq {
                    return;
                }
                match lib {
                    Ok(l) => {
                        a.library = l;
                        a.library_error = None;
                    }
                    Err(e) => a.library_error = Some(e.to_string()),
                }
                a.library_loaded = true;
                a.recent = recent.ok().map(|t| t.into_iter().filter(|t| t.last_played.is_some()).take(16).collect());
                a.top = top.ok().map(|t| t.into_iter().filter(|t| t.play_count > 0).take(16).collect());
                a.artists = artists.unwrap_or_default();
                a.albums = albums.unwrap_or_default();
                if let Ok(p) = playlists {
                    a.playlists = Some(p);
                }
                a.update_library();
                a.update_playlists();
                a.update_home();
                a.update_tracklist();
                if let Route::Playlist(id) = a.route() {
                    a.load_playlist(id, false);
                }
            });
        });
    }

    fn settings_sort(&self) -> String {
        self.win.global::<crate::Lib>().get_library_sort().to_string()
    }

    pub fn reload_library_tracks(&mut self) {
        let api = self.api.clone();
        let sort = self.settings_sort();
        self.spawn(async move {
            let r = api.tracks(&sort).await;
            post(move |a| {
                if let Ok(l) = r {
                    a.library = l;
                    a.update_library();
                    a.update_home();
                }
            });
        });
    }

    pub fn reload_playlists(&mut self) {
        let api = self.api.clone();
        self.spawn(async move {
            let r = api.playlists().await;
            post(move |a| {
                if let Ok(p) = r {
                    a.playlists = Some(p);
                    a.update_playlists();
                    a.update_home();
                }
                if let Route::Playlist(id) = a.route() {
                    a.load_playlist(id, false);
                }
            });
        });
    }

    pub fn load_recs(&mut self) {
        let api = self.api.clone();
        self.spawn(async move {
            let r = api.recommendations().await;
            post(move |a| {
                a.recs = Some(r.unwrap_or_default());
                a.update_home();
            });
        });
    }

    pub fn refresh_recs(&mut self) {
        if self.recs_loading {
            return;
        }
        self.recs_loading = true;
        self.win.global::<crate::Lib>().set_recs_loading(true);
        let api = self.api.clone();
        self.spawn(async move {
            let r = api.refresh_recommendations().await;
            post(move |a| {
                a.recs_loading = false;
                a.win.global::<crate::Lib>().set_recs_loading(false);
                match r {
                    Ok(t) => {
                        a.recs = Some(t);
                        a.update_home();
                        a.reload_playlists();
                    }
                    Err(e) => a.snack(e.to_string()),
                }
            });
        });
    }

    pub fn update_library(&mut self) {
        let q = self.win.global::<crate::Lib>().get_library_filter().to_lowercase();
        let q = q.trim();
        self.filtered = if q.is_empty() {
            self.library.clone()
        } else {
            self.library
                .iter()
                .filter(|t| t.title.to_lowercase().contains(q) || t.artist.to_lowercase().contains(q) || t.album.to_lowercase().contains(q))
                .cloned()
                .collect()
        };
        self.m_library.set_vec(self.filtered.iter().map(track_item).collect::<Vec<_>>());
        let lib = self.win.global::<crate::Lib>();
        lib.set_library_state(if self.library_error.is_some() && self.library.is_empty() {
            2
        } else if self.library_loaded {
            1
        } else {
            0
        });
        lib.set_library_error(self.library_error.clone().unwrap_or_default().into());
        lib.set_library_count(if self.library_loaded { plural(self.library.len() as i64, "song").into() } else { SharedString::new() });
        lib.set_artists(model(
            self.artists
                .iter()
                .map(|x| CardItem {
                    kind: 2,
                    id: x.cover_track_id.as_str().into(),
                    title: if x.name.is_empty() { "Unknown artist".into() } else { x.name.as_str().into() },
                    subtitle: plural(x.track_count, "song").into(),
                    ..Default::default()
                })
                .collect(),
        ));
        lib.set_albums(model(
            self.albums
                .iter()
                .map(|x| CardItem {
                    kind: 3,
                    id: x.cover_track_id.as_str().into(),
                    title: x.name.as_str().into(),
                    subtitle: x.artist.as_str().into(),
                    ..Default::default()
                })
                .collect(),
        ));
    }

    fn playlist_card(p: &Playlist) -> CardItem {
        CardItem {
            kind: 1,
            id: SharedString::new(),
            title: p.name.as_str().into(),
            subtitle: if p.is_discover() { "Mix from your listening".into() } else { plural(p.track_count, "song").into() },
            downloadable: false,
            pid: p.id as i32,
            covers: strings(&p.cover_track_ids),
            discover: p.is_discover(),
        }
    }

    pub fn update_playlists(&mut self) {
        let lib = self.win.global::<crate::Lib>();
        lib.set_playlists_loading(self.playlists.is_none());
        let list = self.playlists.clone().unwrap_or_default();
        lib.set_playlists(model(
            list.iter()
                .map(|p| PlaylistItem {
                    id: p.id as i32,
                    name: p.name.as_str().into(),
                    subtitle: if p.is_discover() { "Mix from your listening".into() } else { plural(p.track_count, "song").into() },
                    discover: p.is_discover(),
                    covers: strings(&p.cover_track_ids),
                })
                .collect(),
        ));
        let mut cards =
            vec![CardItem { kind: 4, title: "New playlist".into(), subtitle: "Start empty or import one".into(), ..Default::default() }];
        cards.extend(list.iter().map(Self::playlist_card));
        lib.set_playlist_cards(model(cards));
    }

    /// Rebuild the home rows (home_screen.dart).
    pub fn update_home(&mut self) {
        let lib = self.win.global::<crate::Lib>();
        lib.set_home_state(if self.library_error.is_some() && self.library.is_empty() {
            2
        } else if self.library_loaded && self.library.is_empty() {
            3
        } else {
            1
        });
        lib.set_home_error(self.library_error.clone().unwrap_or_default().into());
        let mut sections = Vec::new();
        let mut lists = HashMap::new();
        fn row(
            sections: &mut Vec<SectionItem>,
            lists: &mut HashMap<String, Vec<Track>>,
            key: &str,
            title: String,
            sub: &str,
            v: Option<&Vec<Track>>,
            downloadable: bool,
        ) {
            match v {
                None => sections.push(SectionItem {
                    key: key.into(),
                    title: title.into(),
                    subtitle: sub.into(),
                    loading: true,
                    cards: model(vec![]),
                }),
                Some(v) if !v.is_empty() => {
                    sections.push(SectionItem {
                        key: key.into(),
                        title: title.into(),
                        subtitle: sub.into(),
                        loading: false,
                        cards: model(
                            v.iter()
                                .map(|t| CardItem {
                                    kind: 0,
                                    id: t.video_id.as_str().into(),
                                    title: t.title.as_str().into(),
                                    subtitle: t.artist.as_str().into(),
                                    downloadable: downloadable && !t.downloaded,
                                    ..Default::default()
                                })
                                .collect(),
                        ),
                    });
                    lists.insert(key.into(), v.clone());
                }
                _ => {}
            }
        }
        row(&mut sections, &mut lists, "recent", "Recently played".into(), "", self.recent.as_ref(), false);
        row(&mut sections, &mut lists, "top", "Most played".into(), "", self.top.as_ref(), false);
        row(&mut sections, &mut lists, "recs", "Recommended".into(), "", self.recs.as_ref(), true);
        // "Because you listened to <Artist>": the biggest groups of recommendations.
        if let Some(recs) = &self.recs {
            let mut groups: Vec<(String, Vec<Track>)> = Vec::new();
            for t in recs {
                if let Some(a) = t.because_artist.as_ref().filter(|a| !a.is_empty()) {
                    match groups.iter_mut().find(|g| &g.0 == a) {
                        Some(g) => g.1.push(t.clone()),
                        None => groups.push((a.clone(), vec![t.clone()])),
                    }
                }
            }
            groups.retain(|g| g.1.len() >= 3);
            groups.sort_by(|a, b| b.1.len().cmp(&a.1.len()));
            for (i, g) in groups.iter().take(2).enumerate() {
                row(&mut sections, &mut lists, &format!("because{i}"), format!("More like {}", g.0), "", Some(&g.1), true);
            }
        }
        if let Some(pl) = self.playlists.as_ref().filter(|p| !p.is_empty()) {
            sections.push(SectionItem {
                key: "home-playlists".into(),
                title: "Playlists".into(),
                subtitle: SharedString::new(),
                loading: false,
                cards: model(pl.iter().map(Self::playlist_card).collect()),
            });
        }
        let added: Option<Vec<Track>> = if self.library_loaded { Some(self.library.iter().take(16).cloned().collect()) } else { None };
        row(&mut sections, &mut lists, "added", "Recently added".into(), "", added.as_ref(), false);
        self.home_lists = lists;
        self.win.global::<crate::Lib>().set_home_sections(model(sections));
    }

    pub fn list(&self, key: &str) -> Vec<Track> {
        match key {
            "library" => self.filtered.clone(),
            "search" => self.search_results.clone(),
            "playlist" => self.playlist.as_ref().map(|p| p.tracks.clone()).unwrap_or_default(),
            "tracklist" => self.tracklist.clone(),
            "upnext" => self.up_next().to_vec(),
            k => self.home_lists.get(k).cloned().unwrap_or_default(),
        }
    }

    // ------------------------------------------------------------ search

    pub fn set_search(&mut self, q: String) {
        let lib = self.win.global::<crate::Lib>();
        lib.set_search_query(q.as_str().into());
        self.search_seq += 1;
        let seq = self.search_seq;
        let q = q.trim().to_string();
        if q.is_empty() {
            lib.set_search_state(0);
            self.search_results.clear();
            self.m_search.set_vec(vec![]);
            return;
        }
        let playlist = crate::api::looks_like_playlist_url(&q);
        let song_link = !playlist && crate::api::looks_like_youtube_link(&q);
        lib.set_search_state(if playlist { 5 } else { 1 });
        let api = self.api.clone();
        self.spawn(async move {
            if playlist {
                let r = api.playlist_info(&q).await;
                post(move |a| {
                    if seq != a.search_seq {
                        return;
                    }
                    match r {
                        Ok(p) => {
                            let missing = p.tracks.iter().filter(|t| !t.downloaded).count();
                            let lib = a.win.global::<crate::Lib>();
                            lib.set_preview_title(p.title.as_str().into());
                            lib.set_preview_summary(format!("{} songs · {} to download", p.tracks.len(), missing).into());
                            let covers: Vec<String> = p.tracks.iter().take(4).map(|t| t.video_id.clone()).collect();
                            lib.set_preview_covers(strings(&covers));
                            a.search_results = p.tracks.clone();
                            a.m_search.set_vec(p.tracks.iter().map(track_item).collect::<Vec<_>>());
                            a.preview = Some((q, p));
                            // The header lives in the first row; with no songs show the error state.
                            lib.set_search_state(if a.search_results.is_empty() { 7 } else { 6 });
                            if a.search_results.is_empty() {
                                lib.set_search_error("That playlist has no songs that can be downloaded.".into());
                            }
                        }
                        Err(e) => {
                            a.win.global::<crate::Lib>().set_search_error(crate::api::friendly_import_error(&e).into());
                            a.win.global::<crate::Lib>().set_search_state(7);
                        }
                    }
                });
            } else if song_link {
                // A pasted song link: show that one song, ready to download.
                let r = api.song_from_link(&q).await;
                post(move |a| {
                    if seq != a.search_seq {
                        return;
                    }
                    match r {
                        Ok(t) => {
                            a.win.global::<crate::Lib>().set_search_state(2);
                            a.m_search.set_vec(vec![track_item(&t)]);
                            a.search_results = vec![t];
                        }
                        Err(e) => {
                            a.win.global::<crate::Lib>().set_search_error(e.to_string().into());
                            a.win.global::<crate::Lib>().set_search_state(4);
                        }
                    }
                });
            } else {
                // Debounce: wait until typing pauses.
                tokio::time::sleep(Duration::from_millis(350)).await;
                let still = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
                let (tx, rx) = tokio::sync::oneshot::channel();
                let s2 = still.clone();
                post(move |a| {
                    s2.store(seq == a.search_seq, std::sync::atomic::Ordering::SeqCst);
                    let _ = tx.send(());
                });
                let _ = rx.await;
                if !still.load(std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                let r = api.search(&q).await;
                post(move |a| {
                    if seq != a.search_seq {
                        return;
                    }
                    match r {
                        Ok(t) => {
                            a.win.global::<crate::Lib>().set_search_state(if t.is_empty() { 3 } else { 2 });
                            a.m_search.set_vec(t.iter().map(track_item).collect::<Vec<_>>());
                            a.search_results = t;
                        }
                        Err(e) => {
                            a.win.global::<crate::Lib>().set_search_error(e.to_string().into());
                            a.win.global::<crate::Lib>().set_search_state(4);
                        }
                    }
                });
            }
        });
    }

    pub fn import_preview(&mut self) {
        let Some((url, _)) = self.preview.clone() else { return };
        self.win.global::<crate::Lib>().set_import_busy(true);
        let api = self.api.clone();
        self.spawn(async move {
            let r = api.download_playlist(&url, None, None).await;
            post(move |a| {
                a.win.global::<crate::Lib>().set_import_busy(false);
                match r {
                    Ok(r) => {
                        a.reload_playlists();
                        a.snack(if r.queued == 0 {
                            "Playlist imported — every song was already downloaded".to_string()
                        } else {
                            format!(
                                "Importing playlist: downloading {}{}",
                                plural(r.queued, "song"),
                                if r.skipped > 0 { format!(" ({} already in library)", r.skipped) } else { String::new() }
                            )
                        });
                        a.go(Route::Playlist(r.playlist_id));
                    }
                    Err(e) => a.snack(format!("Import failed: {e}")),
                }
            });
        });
    }

    // ------------------------------------------------------------ playlist / artist / album

    pub fn load_playlist(&mut self, id: i64, show_loading: bool) {
        self.pl_seq += 1;
        let seq = self.pl_seq;
        if show_loading && self.playlist.as_ref().map(|p| p.id) != Some(id) {
            self.win.global::<crate::Lib>().set_pl_state(0);
        }
        let api = self.api.clone();
        self.spawn(async move {
            let r = api.playlist(id).await;
            post(move |a| {
                if seq != a.pl_seq {
                    return;
                }
                match r {
                    Ok(p) => a.show_playlist(p),
                    Err(e) => {
                        a.win.global::<crate::Lib>().set_home_error(e.to_string().into());
                        a.win.global::<crate::Lib>().set_pl_state(2);
                    }
                }
            });
        });
    }

    pub fn show_playlist(&mut self, p: Playlist) {
        let lib = self.win.global::<crate::Lib>();
        lib.set_pl_id(p.id as i32);
        lib.set_pl_name(p.name.as_str().into());
        lib.set_pl_kind(p.kind.as_str().into());
        lib.set_pl_label(
            match p.kind.as_str() {
                "discover" => "Discover mix",
                "imported" => "Imported from YouTube Music",
                _ => "Playlist",
            }
            .into(),
        );
        let mut meta = plural(p.track_count, "song");
        if p.duration_sec > 0 {
            meta += &format!(" · {}", fmt_total(p.duration_sec));
        }
        lib.set_pl_meta(meta.into());
        lib.set_pl_covers(strings(&p.cover_track_ids));
        lib.set_pl_not_downloaded(p.tracks.iter().filter(|t| !t.downloaded).count() as i32);
        let mut rows = vec![TrackItem::default()];
        rows.extend(p.tracks.iter().map(track_item));
        self.m_playlist.set_vec(rows);
        lib.set_pl_state(1);
        self.playlist = Some(p);
    }

    pub fn update_tracklist(&mut self) {
        let (is_artist, name, artist) = match self.route() {
            Route::Artist(n) => (true, n.clone(), n),
            Route::Album(n, a) => (false, n, a),
            _ => return,
        };
        let al = artist.to_lowercase();
        let nl = name.to_lowercase();
        self.tracklist = self
            .library
            .iter()
            .filter(
                |t| if is_artist { t.artist.to_lowercase() == al } else { t.album.to_lowercase() == nl && t.artist.to_lowercase() == al },
            )
            .cloned()
            .collect();
        let total: i64 = self.tracklist.iter().map(|t| t.duration_sec).sum();
        let lib = self.win.global::<crate::Lib>();
        lib.set_tl_artist(is_artist);
        lib.set_tl_label(if is_artist { "Artist".into() } else { format!("Album by {artist}").into() });
        lib.set_tl_title(if name.is_empty() { "Unknown".into() } else { name.as_str().into() });
        lib.set_tl_meta(format!("{} · {}", plural(self.tracklist.len() as i64, "song"), fmt_total(total)).into());
        lib.set_tl_cover(self.tracklist.first().map(|t| t.video_id.as_str()).unwrap_or("").into());
        let mut rows = vec![TrackItem::default()];
        rows.extend(self.tracklist.iter().map(track_item));
        self.m_tracklist.set_vec(rows);
    }

    pub fn reorder_playlist(&mut self, from: i64, to: i64) {
        let Some(p) = self.playlist.as_mut() else { return };
        let n = p.tracks.len() as i64;
        let to = to.clamp(0, n - 1);
        if from == to || from < 0 || from >= n {
            // still refresh rows so the dragged row snaps back
            let rows: Vec<TrackItem> = std::iter::once(TrackItem::default()).chain(p.tracks.iter().map(track_item)).collect();
            self.m_playlist.set_vec(rows);
            return;
        }
        let before = p.tracks.clone();
        let t = p.tracks.remove(from as usize);
        p.tracks.insert(to as usize, t);
        let ids: Vec<String> = p.tracks.iter().map(|t| t.video_id.clone()).collect();
        let id = p.id;
        let p2 = p.clone();
        self.show_playlist(p2);
        let api = self.api.clone();
        self.spawn(async move {
            let r = api.reorder_playlist(id, &ids).await;
            post(move |a| match r {
                Ok(()) => a.reload_playlists(),
                Err(e) => {
                    if let Some(mut p) = a.playlist.clone() {
                        p.tracks = before;
                        a.show_playlist(p);
                    }
                    a.snack(format!("Could not reorder: {e}"));
                }
            });
        });
    }

    // ------------------------------------------------------------ downloads

    pub fn start_events(&mut self) {
        let api = self.api.clone();
        self.spawn(async move {
            loop {
                if let Ok(list) = api.downloads().await {
                    post(move |a| a.set_downloads(list));
                }
                let _ = api
                    .events(|e| {
                        post(move |a| match e {
                            crate::api::Event::Download(d) => a.apply_download(d),
                            crate::api::Event::Resync => a.reload_downloads(),
                            crate::api::Event::Sync(s) => a.on_sync_event(s),
                        })
                    })
                    .await;
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });
    }

    pub fn reload_downloads(&mut self) {
        let api = self.api.clone();
        self.spawn(async move {
            if let Ok(list) = api.downloads().await {
                post(move |a| a.set_downloads(list));
            }
        });
    }

    pub fn set_downloads(&mut self, list: Vec<Download>) {
        self.downloads = list;
        self.update_downloads();
    }

    pub fn apply_download(&mut self, d: Download) {
        let prev = self.downloads.iter().position(|x| x.id == d.id);
        let was_done = prev.map(|i| self.downloads[i].status == "done").unwrap_or(false);
        match prev {
            Some(i) => self.downloads[i] = d.clone(),
            None => self.downloads.insert(0, d.clone()),
        }
        if !d.is_failed() {
            // The backend drops older failures of a song once it's tried again.
            self.downloads.retain(|x| x.id == d.id || x.video_id != d.video_id || !x.is_failed());
        }
        if d.status == "done" && !was_done {
            crate::covers::retry_failed();
            // A song joined the library: refresh views, batched for playlist imports.
            let t = slint::Timer::default();
            t.start(slint::TimerMode::SingleShot, Duration::from_millis(600), || {
                with_app(|a| {
                    a.refresh_all();
                    a.load_recs();
                    a.mark_downloaded();
                });
            });
            self.refresh_debounce = Some(t);
        }
        self.update_downloads();
    }

    /// Mark finished downloads as downloaded in search results and the queue.
    fn mark_downloaded(&mut self) {
        let done: HashSet<String> = self.downloads.iter().filter(|d| d.status == "done").map(|d| d.video_id.clone()).collect();
        let mut changed = false;
        for t in self.search_results.iter_mut() {
            if !t.downloaded && done.contains(&t.video_id) {
                t.downloaded = true;
                changed = true;
            }
        }
        if changed {
            self.m_search.set_vec(self.search_results.iter().map(track_item).collect::<Vec<_>>());
        }
    }

    pub fn update_downloads(&mut self) {
        use crate::DownloadRow;
        // dl-state map: latest download per song
        DL.with(|m| {
            let mut m = m.borrow_mut();
            m.clear();
            for d in self.downloads.iter().rev() {
                let st = match d.status.as_str() {
                    "done" | "skipped" => 1,
                    "queued" => 2,
                    "downloading" => 3,
                    "failed" => 4,
                    _ => 0,
                };
                m.insert(d.video_id.clone(), (st, (d.progress / 100.0) as f32));
            }
        });
        let lib = self.win.global::<crate::Lib>();
        lib.set_dl_rev(lib.get_dl_rev().wrapping_add(1));

        let mut active: Vec<&Download> = self.downloads.iter().filter(|d| d.is_active()).collect();
        active.sort_by_key(|d| if d.status == "downloading" { 0 } else { 1 });
        let failed: Vec<&Download> = self.downloads.iter().filter(|d| d.is_failed()).collect();
        let finished: Vec<&Download> = self.downloads.iter().filter(|d| d.is_finished()).collect();
        let selectable: HashSet<&str> = failed.iter().chain(finished.iter()).map(|d| d.id.as_str()).collect();
        self.selected.retain(|id| selectable.contains(id.as_str()));
        lib.set_active_downloads(active.len() as i32);
        lib.set_downloads_subtitle(
            if active.is_empty() { "Nothing downloading".to_string() } else { format!("{} downloading, 3 at a time", active.len()) }.into(),
        );
        lib.set_downloads_empty(self.downloads.is_empty());
        lib.set_selected_count(self.selected.len() as i32);
        lib.set_failed_count(failed.len() as i32);
        let mut rows = Vec::new();
        let section = |title: &str, key: &str, items: &[&Download], selectable: bool, rows: &mut Vec<DownloadRow>| {
            if items.is_empty() {
                return;
            }
            let picked = items.iter().filter(|d| self.selected.contains(&d.id)).count();
            rows.push(DownloadRow {
                kind: 0,
                title: format!("{title} · {}", items.len()).into(),
                section: key.into(),
                selectable,
                picked: if picked == 0 {
                    0
                } else if picked == items.len() {
                    2
                } else {
                    1
                },
                ..Default::default()
            });
            for d in items {
                let status_text = match d.status.as_str() {
                    "downloading" => format!("{:.0}%{}", d.progress, d.speed.as_ref().map(|s| format!(" · {s}")).unwrap_or_default()),
                    "queued" => "Waiting in queue".into(),
                    "failed" => d.error.clone().unwrap_or_else(|| "Failed".into()),
                    "skipped" => "Already in your library".into(),
                    _ => "Downloaded".into(),
                };
                rows.push(DownloadRow {
                    kind: 1,
                    id: d.id.as_str().into(),
                    video_id: d.video_id.as_str().into(),
                    title: if d.title.is_empty() { d.video_id.as_str().into() } else { d.title.as_str().into() },
                    artist: d.artist.as_str().into(),
                    status: d.status.as_str().into(),
                    progress: d.progress as f32,
                    status_text: status_text.into(),
                    selected: self.selected.contains(&d.id),
                    selectable,
                    section: key.into(),
                    picked: 0,
                });
            }
        };
        section("Downloading", "active", &active, false, &mut rows);
        section("Failed", "failed", &failed, true, &mut rows);
        section("Completed", "done", &finished, true, &mut rows);
        if !selectable.is_empty() {
            rows.push(DownloadRow { kind: 2, ..Default::default() });
        }
        // Reuse the model when the row count is unchanged (live progress).
        let lib = self.win.global::<crate::Lib>();
        let current = lib.get_downloads();
        if current.row_count() == rows.len() {
            if let Some(vm) = current.as_any().downcast_ref::<VecModel<DownloadRow>>() {
                for (i, r) in rows.into_iter().enumerate() {
                    if vm.row_data(i).as_ref() != Some(&r) {
                        vm.set_row_data(i, r);
                    }
                }
                return;
            }
        }
        lib.set_downloads(ModelRc::new(VecModel::from(rows)));
    }

    /// Ids in display order that can be selected (failed, then finished).
    fn selectable_ids(&self) -> Vec<String> {
        self.downloads
            .iter()
            .filter(|d| d.is_failed())
            .chain(self.downloads.iter().filter(|d| d.is_finished()))
            .map(|d| d.id.clone())
            .collect()
    }

    pub fn dl_toggle(&mut self, id: String, shift: bool) {
        let ordered = self.selectable_ids();
        match (&self.anchor, shift) {
            (Some(anchor), true) => {
                let (a, b) = (ordered.iter().position(|x| x == anchor), ordered.iter().position(|x| *x == id));
                match (a, b) {
                    (Some(a), Some(b)) => {
                        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
                        self.selected.extend(ordered[lo..=hi].iter().cloned());
                    }
                    _ => {
                        self.selected.insert(id);
                    }
                }
            }
            _ => {
                if !self.selected.remove(&id) {
                    self.selected.insert(id.clone());
                }
                self.anchor = Some(id);
            }
        }
        self.update_downloads();
    }

    pub fn dl_section_toggle(&mut self, section: String) {
        let ids: Vec<String> = self
            .downloads
            .iter()
            .filter(|d| if section == "failed" { d.is_failed() } else { d.is_finished() })
            .map(|d| d.id.clone())
            .collect();
        let all = !ids.is_empty() && ids.iter().all(|i| self.selected.contains(i));
        for i in ids {
            if all {
                self.selected.remove(&i);
            } else {
                self.selected.insert(i);
            }
        }
        self.update_downloads();
    }

    pub fn dl_select_all(&mut self) {
        self.selected = self.selectable_ids().into_iter().collect();
        self.update_downloads();
    }

    pub fn dl_remove(&mut self, ids: Vec<String>, announce: bool) {
        let removable: Vec<String> =
            self.downloads.iter().filter(|d| ids.contains(&d.id) && !d.is_active()).map(|d| d.id.clone()).collect();
        if removable.is_empty() {
            return;
        }
        let n = removable.len();
        let api = self.api.clone();
        self.spawn(async move {
            let r = if removable.len() == 1 {
                api.delete_download(&removable[0]).await
            } else {
                api.clear_downloads(None, Some(removable.clone())).await.map(|_| ())
            };
            post(move |a| match r {
                Ok(()) => {
                    a.downloads.retain(|d| !removable.contains(&d.id));
                    a.selected.retain(|s| !removable.contains(s));
                    a.update_downloads();
                    if announce {
                        a.snack(format!("Removed {n} from the list. Your music files are kept."));
                    }
                }
                Err(e) => {
                    a.snack(format!("Could not remove: {e}"));
                    a.reload_downloads();
                }
            });
        });
    }

    pub fn dl_clear(&mut self, status: String) {
        let api = self.api.clone();
        self.spawn(async move {
            let r = api.clear_downloads(Some(&status), None).await;
            post(move |a| match r {
                Ok(n) => {
                    a.downloads.retain(|d| !(if status == "failed" { d.is_failed() } else { d.is_finished() }));
                    a.selected.clear();
                    a.update_downloads();
                    if n > 0 {
                        a.snack(if status == "failed" {
                            format!("Cleared {n} failed download{}", if n == 1 { "" } else { "s" })
                        } else {
                            format!("Cleared {n} completed download{}", if n == 1 { "" } else { "s" })
                        });
                    }
                }
                Err(e) => {
                    a.snack(format!("Could not clear: {e}"));
                    a.reload_downloads();
                }
            });
        });
    }

    pub fn dl_retry(&mut self, ids: Vec<String>) {
        let api = self.api.clone();
        self.spawn(async move {
            for id in ids {
                let r = api.retry_download(&id).await;
                post(move |a| match r {
                    Ok(d) => a.apply_download(d),
                    Err(e) => a.snack(format!("Retry failed: {e}")),
                });
            }
        });
    }

    pub fn download(&mut self, t: Track) {
        let api = self.api.clone();
        self.spawn(async move {
            let r = api.download_track(&t.video_id).await;
            post(move |a| match r {
                Ok(d) => {
                    let skipped = d.status == "skipped";
                    a.apply_download(d);
                    if skipped {
                        a.snack(format!("“{}” is already in your library", t.title));
                    }
                }
                Err(e) => a.snack(format!("Download failed: {e}")),
            });
        });
    }

    pub fn download_all(&mut self, tracks: Vec<Track>) {
        let todo: Vec<Track> = tracks.into_iter().filter(|t| !t.downloaded).collect();
        if todo.is_empty() {
            self.snack("Everything here is already downloaded");
            return;
        }
        let n = todo.len() as i64;
        for t in todo {
            self.download(t);
        }
        self.snack(format!("Downloading {}", plural(n, "song")));
    }

    // ------------------------------------------------------------ private listening

    pub fn load_private_listening(&mut self, attempts: u32) {
        let api = self.api.clone();
        self.spawn(async move {
            for _ in 0..attempts {
                match api.private_listening().await {
                    Ok(v) => {
                        post(move |a| {
                            a.private_listening = Some(v);
                            a.win.global::<crate::Lib>().set_private_listening(if v { 2 } else { 1 });
                        });
                        return;
                    }
                    Err(e) if !e.offline() => return,
                    Err(_) => tokio::time::sleep(Duration::from_secs(3)).await,
                }
            }
        });
    }

    pub fn toggle_private(&mut self) {
        let Some(before) = self.private_listening else { return };
        self.private_listening = Some(!before);
        self.win.global::<crate::Lib>().set_private_listening(if before { 1 } else { 2 });
        let api = self.api.clone();
        self.spawn(async move {
            let r = api.set_private_listening(!before).await;
            post(move |a| {
                let v = match r {
                    Ok(v) => v,
                    Err(e) => {
                        a.snack(format!("Could not change private listening: {e}"));
                        before
                    }
                };
                a.private_listening = Some(v);
                a.win.global::<crate::Lib>().set_private_listening(if v { 2 } else { 1 });
                a.refresh_prefs();
            });
        });
    }
}
