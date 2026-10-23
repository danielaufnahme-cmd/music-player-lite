//! Geonix Music Player LITE server.
//!
//! HTTP API on 127.0.0.1 for the desktop UI and the phone page. Owns the
//! SQLite library, the download queue, device sync and the in-process music
//! provider (`geonix_yt`). Embed it with [`run`] or start the headless
//! `geonix-server` binary.
//!
//! Environment overrides read by [`ServerConfig::from_env`] (all optional):
//!   GEONIX_PORT       port to listen on (default 7902)
//!   GEONIX_SYNC_PORT  device-sync listener on 0.0.0.0 (default 7903; only
//!                     Tailscale and loopback addresses are served)
//!   GEONIX_DATA_DIR   database + cover cache (default ~/.local/share/geonix-lite)
//!   GEONIX_MUSIC_DIR  where MP3s are saved (default ~/Music/Geonix-lite)
//!   GEONIX_AFTER_DOWNLOADS  shell command run ~10 s after downloads/imports settle
//!   GEONIX_TAILSCALE  path to the tailscale CLI (default: PATH, then the
//!                     usual macOS / Windows install locations)
//!   GEONIX_YTDLP / GEONIX_FFMPEG  read by geonix_yt

mod db;
mod downloads;
mod error;
mod models;
pub mod provider;
mod resolve;
mod routes;
pub mod settings;
mod sync;

use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;

use crate::db::Db;
use crate::downloads::{AfterDownloads, DownloadManager};
use crate::provider::{Music, Provider};
use crate::routes::AppState;

pub use crate::sync::DEFAULT_SYNC_PORT;
pub const DEFAULT_PORT: u16 = 7902;

/// Everything the server needs to start. `from_env()` gives the lite defaults.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// API port on 127.0.0.1 (0 = pick a free one).
    pub port: u16,
    /// Device-sync port on 0.0.0.0; `None` disables device sync.
    pub sync_port: Option<u16>,
    pub data_dir: PathBuf,
    pub music_dir: PathBuf,
    /// Shell command run after downloads/imports settle.
    pub after_downloads: Option<String>,
    /// Run the daily Discover-mix refresh in the background.
    pub daily_recommendations: bool,
}

fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key).filter(|v| !v.is_empty()).map(PathBuf::from)
}

fn env_port(key: &str) -> Option<u16> {
    std::env::var(key).ok().and_then(|p| p.trim().parse().ok())
}

fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

impl ServerConfig {
    pub fn from_env() -> Self {
        let data_dir = env_path("GEONIX_DATA_DIR").unwrap_or_else(|| {
            // ~/.local/share on Linux, Library/Application Support on macOS,
            // AppData\Local on Windows.
            dirs::data_local_dir().unwrap_or_else(|| home().join(".local").join("share")).join("geonix-lite")
        });
        Self {
            port: env_port("GEONIX_PORT").unwrap_or(DEFAULT_PORT),
            sync_port: Some(env_port("GEONIX_SYNC_PORT").unwrap_or(DEFAULT_SYNC_PORT)),
            data_dir,
            music_dir: env_path("GEONIX_MUSIC_DIR").unwrap_or_else(|| home().join("Music").join("Geonix-lite")),
            after_downloads: std::env::var("GEONIX_AFTER_DOWNLOADS").ok().filter(|c| !c.trim().is_empty()),
            daily_recommendations: true,
        }
    }

    pub fn api_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

/// Start the server with the real YouTube Music provider and serve until
/// `shutdown` resolves. Fails early (before touching the database) if the API
/// port is taken, e.g. by another running Geonix server.
pub async fn run(cfg: ServerConfig, shutdown: impl Future<Output = ()> + Send + 'static) -> anyhow::Result<()> {
    let yt = geonix_yt::Yt::new(geonix_yt::YtConfig::from_env(Some(cfg.music_dir.clone())));
    run_with_provider(cfg, yt, shutdown).await
}

/// Like [`run`] but with any [`Provider`] (tests use a mock).
pub async fn run_with_provider(
    cfg: ServerConfig,
    provider: impl Provider,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], cfg.port)))
        .await
        .with_context(|| format!("cannot listen on 127.0.0.1:{}", cfg.port))?;
    serve(cfg, listener, provider, shutdown).await
}

/// Serve on an already bound API listener.
pub async fn serve(
    cfg: ServerConfig,
    listener: tokio::net::TcpListener,
    provider: impl Provider,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let addr = listener.local_addr()?;
    let covers_dir = cfg.data_dir.join("covers");
    for dir in [&cfg.data_dir, &cfg.music_dir, &covers_dir] {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }

    let db = Arc::new(Db::open(&cfg.data_dir.join("library.db")).context("opening library.db")?);
    let interrupted = db.fail_interrupted_downloads()?;
    if interrupted > 0 {
        tracing::info!("{interrupted} unfinished download(s) from the last run marked as failed (retry them in the app)");
    }
    let pruned = db.prune_failed_downloads()?;
    if pruned > 0 {
        tracing::info!("removed {pruned} stale failed download record(s) (song already downloaded or failed again later)");
    }

    let music = Music::new(provider);
    let http = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (X11; Linux x86_64) Geonix/0.1")
        .connect_timeout(Duration::from_secs(15))
        .pool_idle_timeout(Duration::from_secs(30))
        .pool_max_idle_per_host(2)
        .build()?;

    let downloads = DownloadManager::new(
        db.clone(),
        music.clone(),
        cfg.music_dir.clone(),
        covers_dir.clone(),
        cfg.after_downloads.clone().map(AfterDownloads::spawn),
    );
    let initial = settings::load(&db);
    downloads.set_parallel(initial.parallel_downloads as usize);

    // Device sync listener (unless turned off in Settings). Failing to bind it only disables sync.
    let mut sync_listener = None;
    let mut sync_port = cfg.sync_port.unwrap_or(DEFAULT_SYNC_PORT);
    if let Some(port) = cfg.sync_port.filter(|_| initial.device_sync) {
        match tokio::net::TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], port))).await {
            Ok(l) => {
                sync_port = l.local_addr().map(|a| a.port()).unwrap_or(port);
                sync_listener = Some(l);
            }
            Err(e) => tracing::warn!("device sync disabled: cannot listen on port {port}: {e}"),
        }
    }
    let sync =
        sync::SyncManager::new(db.clone(), cfg.music_dir.clone(), downloads.clone(), sync_port).map_err(|e| anyhow::anyhow!("{e}"))?;

    let state = AppState {
        sync_listener: Arc::new(settings::SyncListener::new(cfg.sync_port.map(|_| sync_port))),
        downloads,
        sync,
        db,
        music,
        http,
        covers_dir,
        stream_cache: Arc::new(Mutex::new(HashMap::new())),
        recs_lock: Arc::new(tokio::sync::Mutex::new(())),
    };

    let mut background = Vec::new();
    if cfg.daily_recommendations {
        background.push(tokio::spawn(routes::recommendations::daily_refresh(state.clone())));
    }

    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    if let Some(l) = sync_listener {
        state.sync_listener.serve(l, &state);
    }
    let sync_ctl = state.sync_listener.clone();

    tracing::info!("Music Player Lite server on http://{addr}  data: {}  music: {}", cfg.data_dir.display(), cfg.music_dir.display());
    let mut rx = stop_rx.clone();
    let server = axum::serve(listener, routes::router(state)).with_graceful_shutdown(async move {
        let _ = rx.wait_for(|v| *v).await;
    });
    let server = std::future::IntoFuture::into_future(server);
    tokio::pin!(server);

    tokio::select! {
        r = &mut server => r?,
        _ = shutdown => {
            tracing::info!("shutting down");
            let _ = stop_tx.send(true);
            // Open SSE streams would hold graceful shutdown forever; give
            // requests a moment, then drop the rest.
            if tokio::time::timeout(Duration::from_secs(3), &mut server).await.is_err() {
                tracing::debug!("open connections dropped at shutdown");
            }
        }
    }
    sync_ctl.stop();
    for task in background {
        task.abort();
    }
    Ok(())
}

/// Log to stdout (the journal under systemd) at "info", or as `RUST_LOG` says.
/// Colours only on a terminal. Does nothing if logging is already set up.
pub fn init_logging() {
    use std::io::IsTerminal;
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).with_ansi(std::io::stdout().is_terminal()).try_init();
}

/// Ctrl+C or SIGTERM; for the headless binary and embedders that want it.
pub async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::mock::MockProvider;
    use serde_json::{json, Value};

    fn temp_cfg(name: &str) -> ServerConfig {
        let dir = std::env::temp_dir().join(format!("geonix-lite-{name}-{}", uuid::Uuid::new_v4()));
        ServerConfig {
            port: 0,
            sync_port: None,
            data_dir: dir.join("data"),
            music_dir: dir.join("music"),
            after_downloads: None,
            daily_recommendations: false,
        }
    }

    fn mock() -> MockProvider {
        MockProvider::new(|cmd, p, progress| match cmd {
            "search" => Ok(json!({"tracks": [{
                "video_id": "dQw4w9WgXcQ", "title": "Song", "artist": "Artist",
                "album": "Album", "duration_sec": 212, "thumbnail_url": ""
            }]})),
            "download" => {
                assert_eq!(p["video_id"], "dQw4w9WgXcQ");
                let dest = std::path::PathBuf::from(p["dest_dir"].as_str().unwrap());
                let file = dest.join("Artist").join("Song.mp3");
                std::fs::create_dir_all(file.parent().unwrap()).unwrap();
                std::fs::write(&file, b"ID3 fake mp3").unwrap();
                progress(json!({"percent": 50.0, "speed": "1MiB/s"}));
                Ok(json!({
                    "video_id": "dQw4w9WgXcQ", "title": "Song", "artist": "Artist", "album": "Album",
                    "duration_sec": 212, "thumbnail_url": "", "file_path": file.to_string_lossy(),
                    "cover_path": null
                }))
            }
            other => Err(format!("unsupported in mock: {other}")),
        })
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn embedded_server_serves_searches_downloads_and_shuts_down() {
        let cfg = temp_cfg("run");
        let root = cfg.data_dir.parent().unwrap().to_path_buf();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(serve(cfg.clone(), listener, mock(), async {
            let _ = stopped.await;
        }));
        let http = reqwest::Client::new();

        let health: Value = http.get(format!("{base}/api/health")).send().await.unwrap().json().await.unwrap();
        assert_eq!(health["status"], "ok");

        let found: Value = http.get(format!("{base}/api/search?q=song")).send().await.unwrap().json().await.unwrap();
        assert_eq!(found[0]["video_id"], "dQw4w9WgXcQ");
        assert_eq!(found[0]["downloaded"], false);

        let res = http.post(format!("{base}/api/downloads")).json(&json!({"video_id": "dQw4w9WgXcQ"})).send().await.unwrap();
        assert_eq!(res.status(), 202);

        let mut done = false;
        for _ in 0..200 {
            let tracks: Value = http.get(format!("{base}/api/tracks")).send().await.unwrap().json().await.unwrap();
            if tracks.as_array().is_some_and(|a| a.iter().any(|t| t["video_id"] == "dQw4w9WgXcQ")) {
                done = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(done, "download never reached the library");

        let res = http.get(format!("{base}/api/tracks/dQw4w9WgXcQ/stream")).header("Range", "bytes=0-2").send().await.unwrap();
        assert_eq!(res.status(), 206);
        assert_eq!(&res.bytes().await.unwrap()[..], b"ID3");

        // An unknown command surfaces as 503 with the provider's message.
        let res = http.get(format!("{base}/api/playlist-info?url=https://music.youtube.com/playlist?list=PLx")).send().await.unwrap();
        assert!(res.status().is_server_error() || res.status().is_client_error());

        stop.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), server).await.unwrap().unwrap().unwrap();
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn run_fails_fast_when_the_port_is_taken() {
        let taken = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut cfg = temp_cfg("busy");
        cfg.port = taken.local_addr().unwrap().port();
        let err = run_with_provider(cfg.clone(), MockProvider::offline(), std::future::pending()).await;
        assert!(err.is_err());
        assert!(!cfg.data_dir.exists(), "must not touch the data dir when the port is busy");
    }

    #[test]
    fn lite_defaults() {
        for k in ["GEONIX_PORT", "GEONIX_SYNC_PORT", "GEONIX_DATA_DIR", "GEONIX_MUSIC_DIR"] {
            if std::env::var_os(k).is_some() {
                return; // don't fight a configured environment
            }
        }
        let cfg = ServerConfig::from_env();
        assert_eq!(cfg.port, 7902);
        assert_eq!(cfg.sync_port, Some(7903));
        assert!(cfg.data_dir.ends_with("geonix-lite"));
        assert!(cfg.music_dir.ends_with("Music/Geonix-lite"));
    }
}
