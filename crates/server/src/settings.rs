//! Server-side settings: `GET /api/settings` and `PATCH /api/settings`.
//!
//! Stored as one JSON object in the `meta` table (key `settings`), except
//! private listening, which keeps its own `private_listening` key so the older
//! `/api/settings/private-listening` endpoint stays in step.
//!
//! What each setting does, and when:
//!   download_dir        folder for new downloads ("" = the server's music dir); next download
//!   audio_kbps          0 (best VBR), 128, 192 or 320; passed to yt-dlp; next download
//!   parallel_downloads  1..=4 at once; live (running downloads finish first)
//!   auto_retry          retry transient yt-dlp failures with other clients; next download
//!   device_sync         starts / stops the device-sync listener; live
//!   auto_sync_on_open   the app syncs paired devices when it opens; read by the app
//!   phone_page          serve the phone web page at `/`; live
//!   daily_mix           daily background refresh of recommendations; live
//!   discover_mix        rebuilding recommendations also refreshes the Discover Mix; live
//!   private_listening   plays are not recorded; live

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Mutex;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::db::Db;
use crate::error::{ApiError, ApiResult};
use crate::routes::AppState;

pub const SETTINGS_KEY: &str = "settings";
pub const AUDIO_KBPS_CHOICES: [u32; 4] = [0, 128, 192, 320];
pub const MAX_PARALLEL: u32 = 4;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerSettings {
    pub download_dir: String,
    pub audio_kbps: u32,
    pub parallel_downloads: u32,
    pub auto_retry: bool,
    pub device_sync: bool,
    pub auto_sync_on_open: bool,
    pub phone_page: bool,
    pub daily_mix: bool,
    pub discover_mix: bool,
    pub private_listening: bool,
}

impl Default for ServerSettings {
    fn default() -> Self {
        Self {
            download_dir: String::new(),
            audio_kbps: 0,
            parallel_downloads: 3,
            auto_retry: true,
            device_sync: true,
            auto_sync_on_open: true,
            phone_page: true,
            daily_mix: true,
            discover_mix: true,
            private_listening: false,
        }
    }
}

impl ServerSettings {
    /// Folder for new downloads.
    pub fn download_dir_or(&self, default: &std::path::Path) -> PathBuf {
        let d = self.download_dir.trim();
        if d.is_empty() {
            default.to_path_buf()
        } else {
            expand_home(d)
        }
    }

    /// Fixed MP3 bitrate for yt-dlp, None = best.
    pub fn kbps(&self) -> Option<u32> {
        (self.audio_kbps > 0).then_some(self.audio_kbps)
    }
}

fn expand_home(p: &str) -> PathBuf {
    if p == "~" {
        return dirs::home_dir().unwrap_or_default();
    }
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(h) = dirs::home_dir() {
            return h.join(rest);
        }
    }
    PathBuf::from(p)
}

pub fn load(db: &Db) -> ServerSettings {
    let mut s: ServerSettings = db.get_meta(SETTINGS_KEY).ok().flatten().and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default();
    s.private_listening = db.private_listening().unwrap_or(false);
    s
}

fn save(db: &Db, s: &ServerSettings) -> ApiResult<()> {
    let json = serde_json::to_string(s).map_err(|e| ApiError::internal(e.to_string()))?;
    db.set_meta(SETTINGS_KEY, &json)?;
    db.set_private_listening(s.private_listening)?;
    Ok(())
}

/// Apply a partial update to `base`. Unknown keys, wrong types and values
/// out of range are refused with a message naming the key.
pub fn apply_patch(base: &ServerSettings, patch: &Map<String, Value>) -> Result<ServerSettings, String> {
    let mut s = base.clone();
    let boolean = |k: &str, v: &Value| v.as_bool().ok_or_else(|| format!("{k} must be true or false"));
    for (k, v) in patch {
        match k.as_str() {
            "download_dir" => {
                let d = v.as_str().ok_or("download_dir must be a string")?.trim().to_string();
                if !d.is_empty() && !(d.starts_with('~') || std::path::Path::new(&d).is_absolute()) {
                    return Err("download_dir must be an absolute path".into());
                }
                s.download_dir = d;
            }
            "audio_kbps" => {
                let q = v.as_u64().ok_or("audio_kbps must be a number")?;
                if !AUDIO_KBPS_CHOICES.iter().any(|c| u64::from(*c) == q) {
                    return Err("audio_kbps must be 0 (best), 128, 192 or 320".into());
                }
                s.audio_kbps = q as u32;
            }
            "parallel_downloads" => {
                let n = v.as_u64().ok_or("parallel_downloads must be a number")?;
                if !(1..=u64::from(MAX_PARALLEL)).contains(&n) {
                    return Err(format!("parallel_downloads must be between 1 and {MAX_PARALLEL}"));
                }
                s.parallel_downloads = n as u32;
            }
            "auto_retry" => s.auto_retry = boolean(k, v)?,
            "device_sync" => s.device_sync = boolean(k, v)?,
            "auto_sync_on_open" => s.auto_sync_on_open = boolean(k, v)?,
            "phone_page" => s.phone_page = boolean(k, v)?,
            "daily_mix" => s.daily_mix = boolean(k, v)?,
            "discover_mix" => s.discover_mix = boolean(k, v)?,
            "private_listening" => s.private_listening = boolean(k, v)?,
            other => return Err(format!("unknown setting: {other}")),
        }
    }
    Ok(s)
}

// ------------------------------------------------------------ handlers

pub async fn get_settings(State(s): State<AppState>) -> ApiResult<Json<ServerSettings>> {
    Ok(Json(load(&s.db)))
}

pub async fn patch_settings(State(s): State<AppState>, Json(body): Json<Value>) -> ApiResult<Json<ServerSettings>> {
    let Value::Object(patch) = body else {
        return Err(ApiError::bad_request("send a JSON object of settings"));
    };
    let old = load(&s.db);
    let new = apply_patch(&old, &patch).map_err(ApiError::bad_request)?;
    if new.download_dir != old.download_dir && !new.download_dir.is_empty() {
        let dir = new.download_dir_or(std::path::Path::new("."));
        std::fs::create_dir_all(&dir)
            .map_err(|e| ApiError::bad_request(format!("cannot use {} as download folder: {e}", dir.display())))?;
    }
    if new.device_sync != old.device_sync {
        if new.device_sync {
            s.sync_listener.start(&s).await.map_err(|e| ApiError::status(StatusCode::CONFLICT, e))?;
        } else {
            s.sync_listener.stop();
        }
    }
    save(&s.db, &new)?;
    if new.parallel_downloads != old.parallel_downloads {
        s.downloads.set_parallel(new.parallel_downloads as usize);
    }
    Ok(Json(new))
}

/// Middleware for the phone page routes: 404 while `phone_page` is off.
pub async fn phone_page_gate(State(s): State<AppState>, req: Request, next: Next) -> Response {
    if load(&s.db).phone_page {
        next.run(req).await
    } else {
        ApiError::not_found("the phone page is turned off in Settings").into_response()
    }
}

/// Middleware for `/api/sync/pair` and `/api/sync/run`: refused while device sync is off.
pub async fn device_sync_gate(State(s): State<AppState>, req: Request, next: Next) -> Response {
    if load(&s.db).device_sync {
        next.run(req).await
    } else {
        ApiError::status(StatusCode::CONFLICT, "device sync is turned off in Settings").into_response()
    }
}

// ------------------------------------------------------------ sync listener

/// The device-sync listener on 0.0.0.0, startable and stoppable at runtime.
pub struct SyncListener {
    /// Configured port; None = sync disabled for this server (tests, embedders).
    port: Option<u16>,
    running: Mutex<Option<(tokio::sync::oneshot::Sender<()>, tokio::task::JoinHandle<()>)>>,
}

impl SyncListener {
    pub fn new(port: Option<u16>) -> Self {
        Self { port, running: Mutex::new(None) }
    }

    pub fn is_running(&self) -> bool {
        self.running.lock().unwrap().as_ref().is_some_and(|(_, h)| !h.is_finished())
    }

    /// Bind and serve the peer API. Ok if it is already running.
    pub async fn start(&self, state: &AppState) -> Result<(), String> {
        if self.is_running() {
            return Ok(());
        }
        let Some(port) = self.port else {
            return Err("device sync is not available in this server".into());
        };
        let l = tokio::net::TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], port)))
            .await
            .map_err(|e| format!("cannot listen on port {port}: {e}"))?;
        self.serve(l, state);
        Ok(())
    }

    /// Serve the peer API on a bound listener.
    pub fn serve(&self, l: tokio::net::TcpListener, state: &AppState) {
        let addr = l.local_addr().ok();
        let app = crate::routes::sync::peer_router(state.clone()).into_make_service_with_connect_info::<SocketAddr>();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            let stopped = async move {
                let _ = rx.await;
            };
            if let Err(e) = axum::serve(l, app).with_graceful_shutdown(stopped).await {
                tracing::error!("device sync listener stopped: {e}");
            }
        });
        if let Some(a) = addr {
            tracing::info!("device sync on {a} (Tailscale and loopback only)");
        }
        if let Some((old_tx, _)) = self.running.lock().unwrap().replace((tx, task)) {
            let _ = old_tx.send(());
        }
    }

    pub fn stop(&self) {
        if let Some((tx, task)) = self.running.lock().unwrap().take() {
            let _ = tx.send(());
            // Peers may hold keep-alive connections; don't wait for them.
            task.abort();
            tracing::info!("device sync listener stopped");
        }
    }
}

impl Drop for SyncListener {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn patch_validates_every_key() {
        let base = ServerSettings::default();
        let s = apply_patch(&base, &obj(json!({"audio_kbps": 192, "parallel_downloads": 1, "phone_page": false}))).unwrap();
        assert_eq!((s.audio_kbps, s.parallel_downloads, s.phone_page), (192, 1, false));
        for bad in [
            json!({"audio_kbps": 100}),
            json!({"audio_kbps": "320"}),
            json!({"parallel_downloads": 0}),
            json!({"parallel_downloads": 5}),
            json!({"auto_retry": "yes"}),
            json!({"download_dir": "relative/path"}),
            json!({"nope": true}),
        ] {
            assert!(apply_patch(&base, &obj(bad.clone())).is_err(), "{bad} must be refused");
        }
    }

    #[test]
    fn stored_settings_round_trip_with_private_listening() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(load(&db), ServerSettings::default());
        let s = ServerSettings { audio_kbps: 320, private_listening: true, daily_mix: false, ..Default::default() };
        save(&db, &s).unwrap();
        assert_eq!(load(&db), s);
        assert!(db.private_listening().unwrap());
    }

    #[test]
    fn download_dir_falls_back_and_expands_home() {
        let d = std::path::Path::new("/music");
        let mut s = ServerSettings::default();
        assert_eq!(s.download_dir_or(d), d);
        s.download_dir = "~/X".into();
        assert!(s.download_dir_or(d).ends_with("X"));
        assert!(s.download_dir_or(d).is_absolute());
    }

    /// Full server over HTTP: GET/PATCH, validation, require_json, and the
    /// live effects (phone page, sync listener, parallel limit, private listening).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn settings_api_over_http() {
        let dir = std::env::temp_dir().join(format!("geonix-lite-settings-{}", uuid::Uuid::new_v4()));
        let cfg = crate::ServerConfig {
            port: 0,
            sync_port: Some(0),
            data_dir: dir.join("data"),
            music_dir: dir.join("music"),
            after_downloads: None,
            daily_recommendations: false,
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(crate::serve(cfg, listener, crate::provider::mock::MockProvider::offline(), async {
            let _ = stopped.await;
        }));
        let http = reqwest::Client::new();
        let url = format!("{base}/api/settings");
        for _ in 0..100 {
            if http.get(&url).send().await.is_ok() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let patch = |body: Value| http.patch(&url).json(&body).send();

        let v: Value = http.get(&url).send().await.unwrap().json().await.unwrap();
        assert_eq!(v, serde_json::to_value(ServerSettings::default()).unwrap());
        assert_eq!(http.get(format!("{base}/")).send().await.unwrap().status(), 200);

        // Needs a JSON content type.
        let r = http.patch(&url).header("content-type", "text/plain").body(r#"{"phone_page":false}"#).send().await.unwrap();
        assert_eq!(r.status(), 415);
        let r = http.patch(&url).body(r#"{"phone_page":false}"#).send().await.unwrap();
        assert_eq!(r.status(), 415);

        // Bad values are refused with a clear message and change nothing.
        for (bad, needle) in [
            (json!({"audio_kbps": 64}), "audio_kbps"),
            (json!({"parallel_downloads": 9}), "parallel_downloads"),
            (json!({"phone_page": "off"}), "phone_page"),
            (json!({"bogus": 1}), "unknown setting"),
            (json!([1, 2]), "object"),
        ] {
            let r = patch(bad.clone()).await.unwrap();
            assert_eq!(r.status(), 400, "{bad}");
            let e: Value = r.json().await.unwrap();
            assert!(e["error"].as_str().unwrap().contains(needle), "{bad}: {e}");
        }
        let v: Value = http.get(&url).send().await.unwrap().json().await.unwrap();
        assert_eq!(v["audio_kbps"], 0);

        // Valid partial update is saved and returned.
        let music2 = dir.join("music2");
        let r = patch(json!({
            "audio_kbps": 192, "parallel_downloads": 1, "auto_retry": false, "phone_page": false,
            "download_dir": music2.to_string_lossy(), "private_listening": true
        }))
        .await
        .unwrap();
        assert_eq!(r.status(), 200);
        let v: Value = r.json().await.unwrap();
        assert_eq!((v["audio_kbps"].as_u64(), v["parallel_downloads"].as_u64()), (Some(192), Some(1)));
        assert!(music2.is_dir(), "download folder is created");
        let v2: Value = http.get(&url).send().await.unwrap().json().await.unwrap();
        assert_eq!(v, v2);

        // Live effects.
        assert_eq!(http.get(format!("{base}/")).send().await.unwrap().status(), 404);
        assert_eq!(http.get(format!("{base}/api/health")).send().await.unwrap().status(), 200);
        let pl: Value = http.get(format!("{base}/api/settings/private-listening")).send().await.unwrap().json().await.unwrap();
        assert_eq!(pl["enabled"], true);
        patch(json!({"phone_page": true})).await.unwrap();
        assert_eq!(http.get(format!("{base}/")).send().await.unwrap().status(), 200);

        // Device sync off: listener stops and runs are refused; on again: it listens.
        let r = patch(json!({"device_sync": false})).await.unwrap();
        assert_eq!(r.status(), 200);
        let run = || http.post(format!("{base}/api/sync/run")).header("content-type", "application/json").send();
        assert_eq!(run().await.unwrap().status(), 409);
        let r = patch(json!({"device_sync": true})).await.unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(run().await.unwrap().status(), 202);

        stop.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), server).await.unwrap().unwrap().unwrap();
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn parallel_limit_follows_the_setting() {
        let db = std::sync::Arc::new(Db::open_in_memory().unwrap());
        let music = crate::provider::Music::new(crate::provider::mock::MockProvider::offline());
        let dir = std::env::temp_dir();
        let dl = crate::downloads::DownloadManager::new(db, music, dir.clone(), dir, None);
        assert_eq!(dl.parallel_limit().await, 3);
        for n in [1, 4, 2, 3] {
            dl.set_parallel(n);
            for _ in 0..50 {
                if dl.parallel_limit().await == n {
                    break;
                }
                tokio::task::yield_now().await;
            }
            assert_eq!(dl.parallel_limit().await, n);
        }
    }
}
