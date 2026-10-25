//! Screenshot tour (offscreen only, see offscreen.rs): walks through the
//! screens, saves a PNG of each (or just the requested one) and quits.

use std::path::PathBuf;
use std::time::Duration;

use slint::ComponentHandle;

use crate::app::{with_app, App, Route};
use crate::AppWindow;

type Step = (u64, Box<dyn FnOnce(&mut App)>, Option<&'static str>);

/// What to capture: every screen into a directory, or one screen into a file.
pub enum Target {
    All(PathBuf),
    One(String, PathBuf),
}

pub const SCREENS: &[&str] = &[
    "home",
    "search",
    "search-results",
    "library",
    "library-artists",
    "library-playlists",
    "playlist",
    "downloads",
    "podcasts",
    "podcast",
    "home-playing",
    "queue",
    "now-playing",
    "now-playing-lyrics",
    "dialog-new-playlist",
    "dialog-import-unmatched",
    "dialog-devices",
    "dialog-remove",
    "menu",
    "home-light",
    "home-geonix-dark",
    "settings",
    "settings-middle",
    "settings-bottom",
    "settings-light",
];

/// Benchmark modes: start playing the library after load (null audio sink).
pub fn autoplay(mode: String) {
    slint::Timer::single_shot(Duration::from_secs(3), move || {
        with_app(move |a| {
            if mode == "idle" {
                return;
            }
            let t = a.library.clone();
            a.play_list(t, 0, Some(false));
            if mode == "np" {
                a.win.global::<crate::Player>().set_now_playing_open(true);
            }
            if mode == "paused" {
                slint::Timer::single_shot(Duration::from_secs(4), || with_app(|a| a.play_pause()));
            }
        })
    });
}

pub fn start(win: &AppWindow, target: Target) {
    let steps: Vec<Step> = vec![
        (3500, Box::new(|_| {}), Some("home")),
        (0, Box::new(|a| a.go(Route::Search)), None),
        (800, Box::new(|_| {}), Some("search")),
        (0, Box::new(|a| a.set_search("daft punk".into())), None),
        (6000, Box::new(|_| {}), Some("search-results")),
        (0, Box::new(|a| a.set_search(String::new())), None),
        (0, Box::new(|a| a.go(Route::Library)), None),
        (1200, Box::new(|_| {}), Some("library")),
        (0, Box::new(|a| a.win.global::<crate::Lib>().set_library_tab(1)), None),
        (1000, Box::new(|_| {}), Some("library-artists")),
        (0, Box::new(|a| a.win.global::<crate::Lib>().set_library_tab(3)), None),
        (1000, Box::new(|_| {}), Some("library-playlists")),
        (
            0,
            Box::new(|a| {
                a.win.global::<crate::Lib>().set_library_tab(0);
                if let Some(id) = a.playlists.as_ref().and_then(|p| p.iter().find(|p| !p.is_discover()).or(p.first())).map(|p| p.id) {
                    a.go(Route::Playlist(id));
                }
            }),
            None,
        ),
        (1500, Box::new(|_| {}), Some("playlist")),
        (0, Box::new(|a| a.go(Route::Downloads)), None),
        (1000, Box::new(|_| {}), Some("downloads")),
        (0, Box::new(|a| a.go(Route::Podcasts)), None),
        (1800, Box::new(|_| {}), Some("podcasts")),
        (0, Box::new(|a| a.pod_open_biggest()), None),
        (1800, Box::new(|_| {}), Some("podcast")),
        (
            0,
            Box::new(|a| {
                let t = a.library.clone();
                a.go(Route::Home);
                a.play_list(t, 0, Some(false));
            }),
            None,
        ),
        (3500, Box::new(|_| {}), Some("home-playing")),
        (0, Box::new(|a| a.win.global::<crate::Player>().set_queue_open(true)), None),
        (1200, Box::new(|_| {}), Some("queue")),
        (
            0,
            Box::new(|a| {
                a.win.global::<crate::Player>().set_queue_open(false);
                a.win.global::<crate::Player>().set_now_playing_open(true);
                a.start_waves();
            }),
            None,
        ),
        (2500, Box::new(|_| {}), Some("now-playing")),
        (0, Box::new(|a| a.win.global::<crate::Player>().set_np_tab(1)), None),
        (1200, Box::new(|_| {}), Some("now-playing-lyrics")),
        (
            0,
            Box::new(|a| {
                a.win.global::<crate::Player>().set_np_tab(0);
                a.win.global::<crate::Player>().set_now_playing_open(false);
                a.play_pause();
                a.open_new_playlist(true, None);
                a.win.global::<crate::Ui>().set_np_import(true);
                a.update_import_ui();
            }),
            None,
        ),
        (1200, Box::new(|_| {}), Some("dialog-new-playlist")),
        (0, Box::new(|a| a.import_url_edited("https://music.youtube.com/playlist?list=PLtour".into())), None),
        (2500, Box::new(|_| {}), Some("dialog-import-unmatched")),
        (
            0,
            Box::new(|a| {
                a.close_dialog();
                a.open_devices();
            }),
            None,
        ),
        (1500, Box::new(|_| {}), Some("dialog-devices")),
        (
            0,
            Box::new(|a| {
                a.close_dialog();
                if let Some(t) = a.library.first().cloned() {
                    a.confirm_remove_track(t);
                }
            }),
            None,
        ),
        (800, Box::new(|_| {}), Some("dialog-remove")),
        (
            0,
            Box::new(|a| {
                a.close_dialog();
                a.go(Route::Library);
                a.open_track_menu("library".into(), 1, 1500.0, 330.0);
            }),
            None,
        ),
        (1000, Box::new(|_| {}), Some("menu")),
        (
            0,
            Box::new(|a| {
                a.win.global::<crate::Ui>().set_menu_open(false);
                a.settings.theme_source = crate::settings::ThemeSource::Geonix;
                a.settings.light = true;
                a.apply_theme();
                a.go(Route::Home);
            }),
            None,
        ),
        (1500, Box::new(|_| {}), Some("home-light")),
        (
            0,
            Box::new(|a| {
                a.settings.light = false;
                a.apply_theme();
            }),
            None,
        ),
        (1500, Box::new(|_| {}), Some("home-geonix-dark")),
        (0, Box::new(|a| a.go(Route::Settings)), None),
        (1500, Box::new(|_| {}), Some("settings")),
        (0, Box::new(|a| a.win.global::<crate::Prefs>().set_scroll(-1000.0)), None),
        (800, Box::new(|_| {}), Some("settings-middle")),
        (0, Box::new(|a| a.win.global::<crate::Prefs>().set_scroll(-5000.0)), None),
        (800, Box::new(|_| {}), Some("settings-bottom")),
        (
            0,
            Box::new(|a| {
                a.win.global::<crate::Prefs>().set_scroll(0.0);
                a.settings.light = true;
                a.apply_theme();
                a.refresh_prefs();
            }),
            None,
        ),
        (1500, Box::new(|_| {}), Some("settings-light")),
    ];
    run(win.as_weak(), target, steps.into_iter().collect());
}

fn run(win: slint::Weak<AppWindow>, target: Target, steps: std::collections::VecDeque<Step>) {
    step(win, target, steps, false)
}

fn run_primed(win: slint::Weak<AppWindow>, target: Target, steps: std::collections::VecDeque<Step>) {
    step(win, target, steps, true)
}

fn step(win: slint::Weak<AppWindow>, target: Target, mut steps: std::collections::VecDeque<Step>, primed: bool) {
    let Some((delay, action, shot)) = steps.pop_front() else {
        let _ = slint::quit_event_loop();
        return;
    };
    slint::Timer::single_shot(Duration::from_millis(if primed { 0 } else { delay }), move || {
        if let Some(name) = shot {
            let path = match &target {
                Target::All(dir) => Some(dir.join(format!("{name}.png"))),
                Target::One(n, file) if n == name => Some(file.clone()),
                _ => None,
            };
            if let Some(p) = path {
                if !primed {
                    // Offscreen nothing is drawn between shots: render once so
                    // bindings ask for the covers at this size, then shoot.
                    let _ = crate::offscreen::capture();
                    steps.push_front((0, Box::new(|_| {}), shot));
                    let w2 = win.clone();
                    slint::Timer::single_shot(Duration::from_millis(900), move || run_primed(w2, target, steps));
                    return;
                }
                match crate::offscreen::save_png(&p) {
                    Ok(()) => println!("saved {}", p.display()),
                    Err(e) => eprintln!("screenshot failed: {e}"),
                }
                if matches!(target, Target::One(..)) {
                    let _ = slint::quit_event_loop();
                    return;
                }
            }
        }
        with_app(action);
        let w2 = win.clone();
        slint::Timer::single_shot(Duration::from_millis(50), move || run(w2, target, steps));
    });
}
