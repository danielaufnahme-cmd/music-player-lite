//! Music Player Lite: Slint desktop app. Uses a running server on
//! GEONIX_PORT (default 7902) or starts one in-process.
//! `geonix-lite --headless` runs only the server (for systemd).

// Windows release builds are GUI apps: no console window next to the player.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

slint::include_modules!();

mod api;
mod app;
mod audio;
mod covers;
mod dialogs;
mod offscreen;
mod palette;
mod player;
mod prefs;
mod settings;
mod tour;
mod visuals;
mod waves;
mod wire;

use std::time::Duration;

fn port() -> u16 {
    std::env::var("GEONIX_PORT").ok().and_then(|p| p.trim().parse().ok()).unwrap_or(7902)
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(4)
        .thread_name("geonix-rt")
        .enable_all()
        .build()
        .expect("tokio runtime")
}

#[cfg(feature = "embedded-server")]
fn start_server(rt: &tokio::runtime::Runtime) -> Option<tokio::sync::oneshot::Sender<()>> {
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let cfg = geonix_server::ServerConfig::from_env();
    rt.spawn(async move {
        if let Err(e) = geonix_server::run(cfg, async move {
            let _ = rx.await;
        })
        .await
        {
            eprintln!("geonix-lite: embedded server stopped: {e:#}");
        }
    });
    Some(tx)
}

#[cfg(not(feature = "embedded-server"))]
fn start_server(_rt: &tokio::runtime::Runtime) -> Option<tokio::sync::oneshot::Sender<()>> {
    eprintln!("geonix-lite: no server on port {} and this build has no embedded server", port());
    None
}

fn headless() -> i32 {
    #[cfg(feature = "embedded-server")]
    {
        geonix_server::init_logging();
        let rt = runtime();
        let cfg = geonix_server::ServerConfig::from_env();
        match rt.block_on(geonix_server::run(cfg, geonix_server::shutdown_signal())) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("geonix-lite: {e:#}");
                1
            }
        }
    }
    #[cfg(not(feature = "embedded-server"))]
    {
        eprintln!("geonix-lite: built without the embedded server");
        1
    }
}

fn select_backend() {
    #[allow(unused_mut)]
    let mut renderer = "software";
    // GEONIX_RENDERER=software|femtovg wins over the "GPU renderer" setting.
    #[cfg(feature = "femtovg")]
    if match std::env::var("GEONIX_RENDERER").as_deref() {
        Ok("femtovg") => true,
        Ok("software") => false,
        _ => settings::Settings::load().gpu_renderer,
    } || !cfg!(feature = "software")
    {
        renderer = "femtovg";
    }
    if std::env::var_os("SLINT_BACKEND").is_none() {
        if let Err(e) = slint::BackendSelector::new().backend_name("winit".into()).renderer_name(renderer.into()).select() {
            eprintln!("geonix-lite: could not select the {renderer} renderer: {e}");
            if renderer != "software" && cfg!(feature = "software") {
                // no usable OpenGL: fall back to the CPU renderer
                let _ = slint::BackendSelector::new().backend_name("winit".into()).renderer_name("software".into()).select();
            }
        }
    }
}

/// Print RSS and CPU of this process for the last `secs - 10` seconds, then quit.
fn bench_report(mode: String, secs: u64) {
    fn cpu_ticks() -> u64 {
        std::fs::read_to_string("/proc/self/stat")
            .ok()
            .and_then(|s| s.rsplit(')').next().map(|r| r.split_whitespace().map(String::from).collect::<Vec<_>>()))
            .map(|f| {
                f.get(11).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0) + f.get(12).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0)
            })
            .unwrap_or(0)
    }
    fn status(key: &str) -> String {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| s.lines().find(|l| l.starts_with(key)).map(|l| l[key.len()..].trim().to_string()))
            .unwrap_or_default()
    }
    let warm = 10.min(secs.saturating_sub(5));
    slint::Timer::single_shot(Duration::from_secs(warm), move || {
        let t0 = cpu_ticks();
        let start = std::time::Instant::now();
        slint::Timer::single_shot(Duration::from_secs(secs - warm), move || {
            let dt = start.elapsed().as_secs_f64();
            let hz = 100.0; // USER_HZ
            let cpu = (cpu_ticks() - t0) as f64 / hz / dt * 100.0;
            println!(
                "bench {mode}: RSS {} HWM {} threads {} CPU {:.2}% over {:.0}s",
                status("VmRSS:"),
                status("VmHWM:"),
                status("Threads:"),
                cpu,
                dt
            );
            let _ = slint::quit_event_loop();
        });
    });
}

fn main() {
    // Fewer glibc malloc arenas: less idle RSS with several threads.
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, 2);
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "Usage: geonix-lite [--headless]\n\n  --headless   run only the server (no window, for phone listening / systemd)\n\
             \x20 --screenshot <screen|all> <out.png|dir> [--size WxH]   render offscreen (no window) and save PNGs\n\
             \x20 --bench <idle|shell|np|paused> <seconds> [--size WxH]  offscreen render benchmark (RSS/CPU)\n\n\
             Screens: {}\n\nEnvironment: GEONIX_PORT, GEONIX_SYNC_PORT, GEONIX_DATA_DIR, GEONIX_MUSIC_DIR, GEONIX_RENDERER=femtovg",
            tour::SCREENS.join(", ")
        );
        return;
    }
    if args.iter().any(|a| a == "--headless") {
        std::process::exit(headless());
    }

    // Offscreen modes: never open a window.
    let size = args
        .iter()
        .position(|a| a == "--size")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.split_once('x'))
        .and_then(|(w, h)| Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?)))
        .unwrap_or((1700, 950));
    let arg_after = |flag: &str, k: usize| args.iter().position(|a| a == flag).and_then(|i| args.get(i + k)).cloned();
    let shot = arg_after("--screenshot", 1).zip(arg_after("--screenshot", 2));
    let bench = arg_after("--bench", 1).zip(arg_after("--bench", 2).and_then(|s| s.parse::<u64>().ok()));
    let offscreen = shot.is_some() || bench.is_some();
    if offscreen {
        std::env::set_var("GEONIX_NULL_AUDIO", "1");
        offscreen::install(size.0, size.1, bench.is_some());
    }

    let rt = runtime();
    let base = format!("http://127.0.0.1:{}", port());
    let api = api::Api::new(base);
    let mut shutdown = None;
    if !rt.block_on(api.health()) {
        shutdown = start_server(&rt);
        if shutdown.is_some() {
            // Give it a moment; the UI shows its loading state meanwhile.
            rt.block_on(async {
                for _ in 0..40 {
                    if api.health().await {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            });
        }
    }

    if !offscreen {
        select_backend();
    }
    let win = AppWindow::new().expect("create window");
    if !offscreen {
        let _ = slint::set_xdg_app_id(std::env::var("GEONIX_APP_ID").unwrap_or_else(|_| "geonix-lite".into()));
    }
    covers::init(&win, api.clone(), rt.handle().clone());
    app::App::install(win.clone_strong(), api, rt.handle().clone());
    if let Some((screen, out)) = shot {
        let out = std::path::PathBuf::from(out);
        let target = if screen == "all" {
            let _ = std::fs::create_dir_all(&out);
            tour::Target::All(out)
        } else if tour::SCREENS.contains(&screen.as_str()) {
            tour::Target::One(screen, out)
        } else {
            eprintln!("unknown screen {screen}; one of: all, {}", tour::SCREENS.join(", "));
            std::process::exit(2);
        };
        tour::start(&win, target);
    }
    if let Some((mode, secs)) = bench.clone() {
        tour::autoplay(mode.clone());
        bench_report(mode, secs);
    }
    win.run().expect("run window");

    app::try_with_app(|a| a.settings.save());
    if let Some(tx) = shutdown {
        let _ = tx.send(());
    }
    rt.shutdown_timeout(Duration::from_secs(2));
}

#[cfg(test)]
mod ui_tests {
    use super::*;
    use slint::{ComponentHandle, Model, ModelRc, VecModel};

    /// Key screens build and react to state with the Slint testing backend.
    #[test]
    fn screens_build_and_react() {
        i_slint_backend_testing::init_no_event_loop();
        let w = AppWindow::new().unwrap();
        let theme = w.global::<Theme>();
        theme.set_win_width(1400.0);
        assert!(theme.get_wide_rows());
        theme.set_win_width(1000.0);
        assert!(!theme.get_wide_rows());

        let lib = w.global::<Lib>();
        let track = TrackItem {
            id: "abc".into(),
            title: "Song".into(),
            artist: "A".into(),
            album: "B".into(),
            duration: "3:05".into(),
            downloaded: true,
        };
        lib.set_library_tracks(ModelRc::new(VecModel::from(vec![track.clone(); 3])));
        lib.set_library_state(1);
        lib.on_dl_state(|_, _| 1);
        w.global::<Covers>().on_get(|_, _, _, _| slint::Image::default());
        let nav = w.global::<Nav>();
        for page in [Page::Home, Page::Search, Page::Library, Page::Downloads, Page::Playlist, Page::Artist, Page::Settings] {
            nav.set_page(page);
            w.show().unwrap();
            assert_eq!(nav.get_page(), page);
        }
        let p = w.global::<Player>();
        p.set_has_track(true);
        p.set_title("Song".into());
        p.set_up_next(ModelRc::new(VecModel::from(vec![track; 2])));
        p.set_now_playing_open(true);
        w.show().unwrap();
        assert_eq!(p.get_up_next().row_count(), 2);
        let ui = w.global::<Ui>();
        ui.set_dialog("devices".into());
        assert_eq!(ui.get_dialog(), "devices");
    }
}
