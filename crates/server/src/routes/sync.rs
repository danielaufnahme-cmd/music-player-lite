//! Device sync endpoints: the local API the app uses (`/api/sync/*`, on the
//! normal listener) and the peer API other backends call (`/sync/*`, on the
//! sync listener, Tailscale/loopback sources only).

use std::net::SocketAddr;

use axum::body::Body;
use axum::extract::{ConnectInfo, Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::future::join_all;
use serde::Deserialize;
use serde_json::{json, Value};
use tower::ServiceExt;
use tower_http::services::ServeFile;

use super::tracks::check_id;
use super::AppState;
use crate::error::{ApiError, ApiResult};
use crate::sync::{self, DeviceOut, Hello, Manifest, PairRequest, PairResponse, ThisDevice, PROTOCOL_VERSION};

// ------------------------------------------------------------ local API

pub async fn devices(State(s): State<AppState>) -> ApiResult<Json<Value>> {
    let me = sync::this_device(&s.db)?;
    let peers = s.db.peers()?;
    let online = join_all(peers.iter().map(|p| s.sync.is_online(p))).await;
    let devices: Vec<DeviceOut> = peers.iter().zip(online).map(|(p, on)| DeviceOut::new(p, on)).collect();
    Ok(Json(json!({
        "this_device": me,
        "devices": devices,
        "syncing": s.sync.is_syncing(),
    })))
}

pub async fn new_code(State(s): State<AppState>) -> ApiResult<Json<ThisDevice>> {
    Ok(Json(sync::rotate_code(&s.db)?))
}

#[derive(Deserialize)]
pub struct PairBody {
    code: String,
    address: Option<String>,
}

pub async fn pair(State(s): State<AppState>, Json(body): Json<PairBody>) -> ApiResult<Json<DeviceOut>> {
    let peer = s.sync.pair(&body.code, body.address.as_deref()).await?;
    Ok(Json(DeviceOut::new(&peer, true)))
}

pub async fn remove_device(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    s.sync.unpair(&id).await?;
    Ok(Json(json!({ "removed": id })))
}

#[derive(Deserialize, Default)]
pub struct RunBody {
    device_id: Option<String>,
}

/// The body is optional (`{}` or none syncs with every paired device).
pub async fn run(State(s): State<AppState>, body: Option<Json<RunBody>>) -> ApiResult<(StatusCode, Json<Value>)> {
    let device_id = body.and_then(|Json(b)| b.device_id).filter(|id| !id.trim().is_empty());
    s.sync.start_run(device_id)?;
    Ok((StatusCode::ACCEPTED, Json(json!({ "started": true }))))
}

// ------------------------------------------------------------ peer API

pub fn peer_router(state: AppState) -> Router {
    Router::new()
        .route("/sync/hello", get(hello))
        .route("/sync/pair", post(peer_pair).delete(peer_unpair))
        .route("/sync/manifest", get(manifest))
        .route("/sync/file/:video_id", get(file))
        .route("/sync/request", post(request))
        .layer(axum::middleware::from_fn(super::json_errors))
        .layer(axum::middleware::from_fn(ip_filter))
        .with_state(state)
}

/// 403 for any connection that isn't from the tailnet or this machine.
async fn ip_filter(ConnectInfo(remote): ConnectInfo<SocketAddr>, req: Request, next: Next) -> Response {
    if sync::is_allowed_peer_ip(remote.ip()) {
        next.run(req).await
    } else {
        tracing::warn!("sync: refused connection from {remote}");
        ApiError::status(StatusCode::FORBIDDEN, "only devices on your Tailscale network may connect").into_response()
    }
}

async fn hello(State(s): State<AppState>) -> ApiResult<Json<Hello>> {
    let me = sync::this_device(&s.db)?;
    Ok(Json(Hello { device_id: me.id, device_name: me.name, version: PROTOCOL_VERSION }))
}

async fn peer_pair(
    State(s): State<AppState>,
    ConnectInfo(remote): ConnectInfo<SocketAddr>,
    Json(body): Json<PairRequest>,
) -> ApiResult<Json<PairResponse>> {
    Ok(Json(s.sync.accept_pair(remote.ip(), body)?))
}

async fn peer_unpair(
    State(s): State<AppState>,
    ConnectInfo(remote): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let peer = s.sync.authenticate(&headers, remote.ip())?;
    s.db.delete_peer(&peer.id)?;
    tracing::info!("sync: {} unpaired from this device", peer.name);
    Ok(Json(json!({ "removed": peer.id })))
}

async fn manifest(
    State(s): State<AppState>,
    ConnectInfo(remote): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> ApiResult<Json<Manifest>> {
    s.sync.authenticate(&headers, remote.ip())?;
    let db = s.db.clone();
    let music_dir = s.sync.music_dir().to_path_buf();
    let m = tokio::task::spawn_blocking(move || sync::build_manifest(&db, &music_dir))
        .await
        .map_err(|e| ApiError::internal(e.to_string()))??;
    Ok(Json(m))
}

async fn file(
    State(s): State<AppState>,
    ConnectInfo(remote): ConnectInfo<SocketAddr>,
    Path(video_id): Path<String>,
    req: Request,
) -> ApiResult<Response> {
    s.sync.authenticate(req.headers(), remote.ip())?;
    check_id(&video_id)?;
    let track = s.db.get_track(&video_id)?.ok_or_else(|| ApiError::not_found("track not found"))?;
    let path = track
        .file_path
        .as_deref()
        .and_then(|p| sync::file_inside(std::path::Path::new(p), s.sync.music_dir()))
        .ok_or_else(|| ApiError::not_found("song is not downloaded on this device"))?;
    // ServeFile handles Range / HEAD.
    let mut res =
        ServeFile::new(path).oneshot(req).await.map_err(|e| ApiError::internal(format!("serving file failed: {e}")))?.map(Body::new);
    res.headers_mut().insert("cache-control", HeaderValue::from_static("no-store"));
    Ok(res)
}

async fn request(
    State(s): State<AppState>,
    ConnectInfo(remote): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let peer = s.sync.authenticate(&headers, remote.ip())?;
    s.sync.start_incoming(peer)?;
    Ok((StatusCode::ACCEPTED, Json(json!({ "started": true }))))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::future::IntoFuture;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use super::*;
    use crate::db::Db;
    use crate::downloads::DownloadManager;
    use crate::models::Track;
    use crate::provider::{mock::MockProvider, Music};
    use crate::sync::{SyncEvent, SyncManager};

    struct TestBackend {
        state: AppState,
        api: String,
        sync_addr: SocketAddr,
        dir: PathBuf,
        music: PathBuf,
    }

    impl Drop for TestBackend {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.dir).ok();
        }
    }

    async fn spawn_backend(name: &str) -> TestBackend {
        let dir = std::env::temp_dir().join(format!("geonix-sync-{name}-{}", uuid::Uuid::new_v4()));
        let music = dir.join("music");
        let covers = dir.join("covers");
        std::fs::create_dir_all(&music).unwrap();
        std::fs::create_dir_all(&covers).unwrap();
        let db = Arc::new(Db::open(&dir.join("library.db")).unwrap());
        let local = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let peer = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let sync_addr = peer.local_addr().unwrap();
        // Sync never talks to the music provider.
        let provider = Music::new(MockProvider::offline());
        let downloads = DownloadManager::new(db.clone(), provider.clone(), music.clone(), covers.clone(), None);
        let sync = SyncManager::new(db.clone(), music.clone(), downloads.clone(), sync_addr.port()).unwrap();
        sync::set_device_name(&db, name);
        let state = AppState {
            db,
            music: provider,
            downloads,
            sync,
            http: reqwest::Client::new(),
            covers_dir: covers,
            stream_cache: Arc::new(Mutex::new(HashMap::new())),
            recs_lock: Arc::new(tokio::sync::Mutex::new(())),
            sync_listener: Arc::new(crate::settings::SyncListener::new(None)),
        };
        let api = format!("http://{}", local.local_addr().unwrap());
        tokio::spawn(axum::serve(local, super::super::router(state.clone())).into_future());
        tokio::spawn(axum::serve(peer, peer_router(state.clone()).into_make_service_with_connect_info::<SocketAddr>()).into_future());
        TestBackend { state, api, sync_addr, dir, music }
    }

    fn add_song(b: &TestBackend, id: &str, rel: &str, content: &[u8]) {
        let path = b.music.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
        let t = Track {
            video_id: id.into(),
            title: format!("Title {id}"),
            artist: "Artist".into(),
            album: "Album".into(),
            duration_sec: 180,
            ..Default::default()
        };
        b.state.db.mark_downloaded(&t, &path.to_string_lossy(), None).unwrap();
    }

    fn playlist(b: &TestBackend, name: &str, kind: &str, ids: &[&str]) -> i64 {
        let p = b.state.db.create_playlist(name, kind, None).unwrap();
        b.state.db.set_playlist_tracks(p, &ids.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap();
        p
    }

    fn playlist_songs(b: &TestBackend, name: &str) -> Vec<String> {
        let db = &b.state.db;
        let p = db.playlists().unwrap().into_iter().find(|p| p.name == name && p.kind != "discover");
        let Some(p) = p else { return Vec::new() };
        db.playlist(p.id).unwrap().tracks.into_iter().map(|t| t.video_id).collect()
    }

    async fn devices(http: &reqwest::Client, b: &TestBackend) -> Value {
        http.get(format!("{}/api/sync/devices", b.api)).send().await.unwrap().json().await.unwrap()
    }

    async fn wait_idle(b: &TestBackend) {
        for _ in 0..200 {
            if !b.state.sync.is_syncing() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("sync did not finish");
    }

    /// New songs per device from the `done` events received so far.
    fn new_songs(rx: &mut tokio::sync::broadcast::Receiver<SyncEvent>) -> (usize, usize) {
        let (mut dones, mut songs) = (0, 0);
        while let Ok(ev) = rx.try_recv() {
            assert_ne!(ev.state, "error", "unexpected sync error: {ev:?}");
            if ev.state == "done" {
                dones += 1;
                songs += ev.done;
            }
        }
        (dones, songs)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_backends_pair_and_sync_both_ways() {
        let a = spawn_backend("alpha").await;
        let b = spawn_backend("beta").await;
        let http = reqwest::Client::new();

        add_song(&a, "a1", "Artist/a1.mp3", b"song a1 bytes");
        add_song(&a, "a2", "a2.mp3", &vec![7u8; 300_000]);
        add_song(&b, "b1", "Other/b1.mp3", b"song b1");
        playlist(&a, "Road", "user", &["a1", "a2"]);
        let discover_a = playlist(&a, "Discover Mix", "discover", &["a1"]);
        playlist(&b, "Road", "user", &["b1"]);
        playlist(&b, "Chill", "imported", &["b1"]);

        // Wrong code: 404 with the spec's message.
        let b_info = devices(&http, &b).await;
        let b_code = b_info["this_device"]["code"].as_str().unwrap().to_string();
        let wrong = if b_code == "AAAAAA" { "BBBBBB" } else { "AAAAAA" };
        let resp = http
            .post(format!("{}/api/sync/pair", a.api))
            .json(&json!({"code": wrong, "address": b.sync_addr.to_string()}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["error"], sync::NO_DEVICE_WITH_CODE);

        // Pair A -> B by address with B's code (lowercase works too).
        let resp = http
            .post(format!("{}/api/sync/pair", a.api))
            .json(&json!({"code": b_code.to_lowercase(), "address": b.sync_addr.to_string()}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let dev: Value = resp.json().await.unwrap();
        assert_eq!(dev["id"], b_info["this_device"]["id"]);
        assert_eq!(dev["name"], "beta");
        assert_eq!(dev["online"], true);
        assert!(dev["last_sync_at"].is_null());

        let b_after = devices(&http, &b).await;
        assert_ne!(b_after["this_device"]["code"], b_code.as_str(), "code rotates after pairing");
        assert_eq!(b_after["devices"][0]["name"], "alpha");
        assert_eq!(b_after["devices"][0]["online"], true);
        let a_after = devices(&http, &a).await;
        assert_eq!(a_after["devices"].as_array().unwrap().len(), 1);
        assert_eq!(a_after["syncing"], false);

        // Peer API needs the token.
        let resp = http.get(format!("http://{}/sync/manifest", b.sync_addr)).send().await.unwrap();
        assert_eq!(resp.status(), 401);
        let hello: Value = http.get(format!("http://{}/sync/hello", b.sync_addr)).send().await.unwrap().json().await.unwrap();
        assert_eq!(hello["device_name"], "beta");
        assert_eq!(hello["version"], 1);

        // First sync: A pulls from B, then B pulls from A.
        let mut a_events = a.state.sync.subscribe();
        let mut b_events = b.state.sync.subscribe();
        let resp = http.post(format!("{}/api/sync/run", a.api)).json(&json!({})).send().await.unwrap();
        assert_eq!(resp.status(), 202);
        wait_idle(&a).await;
        wait_idle(&b).await;
        assert_eq!(new_songs(&mut a_events), (1, 1));
        assert_eq!(new_songs(&mut b_events), (1, 2));

        assert_eq!(std::fs::read(a.music.join("Other/b1.mp3")).unwrap(), b"song b1");
        assert_eq!(std::fs::read(b.music.join("Artist/a1.mp3")).unwrap(), b"song a1 bytes");
        assert_eq!(std::fs::read(b.music.join("a2.mp3")).unwrap().len(), 300_000);
        assert!(a.state.db.get_track("b1").unwrap().unwrap().downloaded);
        let b_a1 = b.state.db.get_track("a1").unwrap().unwrap();
        assert_eq!(b_a1.title, "Title a1");
        assert_eq!(b_a1.file_path.as_deref(), Some(&*b.music.join("Artist/a1.mp3").to_string_lossy()));

        assert_eq!(playlist_songs(&a, "Road"), ["a1", "a2", "b1"]);
        assert_eq!(playlist_songs(&a, "Chill"), ["b1"]);
        assert_eq!(playlist_songs(&b, "Road"), ["b1", "a1", "a2"]);
        assert_eq!(a.state.db.playlist(discover_a).unwrap().tracks.len(), 1, "Discover Mix untouched");
        assert!(b.state.db.playlists().unwrap().iter().all(|p| p.name != "Discover Mix"), "the Discover Mix is never synced");
        let a_dev = devices(&http, &a).await;
        assert!(a_dev["devices"][0]["last_sync_at"].as_str().unwrap().ends_with('Z'));
        assert!(a_dev["devices"][0]["last_error"].is_null());

        // Second sync downloads nothing on either side.
        let resp = http.post(format!("{}/api/sync/run", a.api)).header("content-type", "application/json").send().await.unwrap();
        assert_eq!(resp.status(), 202);
        wait_idle(&a).await;
        wait_idle(&b).await;
        assert_eq!(new_songs(&mut a_events), (1, 0));
        assert_eq!(new_songs(&mut b_events), (1, 0));
        assert_eq!(playlist_songs(&a, "Road"), ["a1", "a2", "b1"]);
        assert_eq!(playlist_songs(&b, "Road"), ["b1", "a1", "a2"]);

        // Removing the device on A also removes A on B.
        let b_id = dev["id"].as_str().unwrap();
        let resp =
            http.delete(format!("{}/api/sync/devices/{b_id}", a.api)).header("content-type", "application/json").send().await.unwrap();
        assert_eq!(resp.status(), 200);
        assert!(a.state.db.peers().unwrap().is_empty());
        for _ in 0..100 {
            if b.state.db.peers().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(b.state.db.peers().unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wrong_codes_are_rate_limited_and_runs_conflict() {
        let a = spawn_backend("alpha").await;
        let http = reqwest::Client::new();
        let code = sync::this_device(&a.state.db).unwrap().code;
        let wrong = if code == "AAAAAA" { "BBBBBB" } else { "AAAAAA" };
        let url = format!("http://{}/sync/pair", a.sync_addr);
        let body = json!({"code": wrong, "device_id": "other", "device_name": "x", "port": 1});
        for _ in 0..10 {
            assert_eq!(http.post(&url).json(&body).send().await.unwrap().status(), 403);
        }
        assert_eq!(http.post(&url).json(&body).send().await.unwrap().status(), 429);
        let right = json!({"code": code, "device_id": "other", "device_name": "x", "port": 1});
        assert_eq!(http.post(&url).json(&right).send().await.unwrap().status(), 429, "blocked for the minute");

        let new: Value = http
            .post(format!("{}/api/sync/code", a.api))
            .header("content-type", "application/json")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_ne!(new["code"], code.as_str());
        assert!(sync::normalize_code(new["code"].as_str().unwrap()).is_some());

        // Holding the sync lock makes /api/sync/run answer 409.
        let resp = http.post(format!("{}/api/sync/run", a.api)).header("content-type", "application/json").send().await.unwrap();
        assert_eq!(resp.status(), 202);
        let second = http.post(format!("{}/api/sync/run", a.api)).header("content-type", "application/json").send().await.unwrap().status();
        assert!(second == 409 || second == 202, "409 while running ({second})");
        wait_idle(&a).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn private_listening_records_nothing() {
        let a = spawn_backend("alpha").await;
        let http = reqwest::Client::new();
        add_song(&a, "s1", "s1.mp3", b"x");
        let url = format!("{}/api/settings/private-listening", a.api);
        let v: Value = http.get(&url).send().await.unwrap().json().await.unwrap();
        assert_eq!(v, json!({"enabled": false}));

        let played = format!("{}/api/tracks/s1/played", a.api);
        assert_eq!(http.post(&played).send().await.unwrap().status(), 200);
        assert_eq!(a.state.db.get_track("s1").unwrap().unwrap().play_count, 1);

        let v: Value = http.put(&url).json(&json!({"enabled": true})).send().await.unwrap().json().await.unwrap();
        assert_eq!(v, json!({"enabled": true}));
        let resp = http.post(&played).send().await.unwrap();
        assert_eq!(resp.status(), 204);
        assert!(resp.bytes().await.unwrap().is_empty());
        let t = a.state.db.get_track("s1").unwrap().unwrap();
        assert_eq!(t.play_count, 1, "private listens are not counted");
        let plays: i64 = a.state.db.count_plays("s1").unwrap();
        assert_eq!(plays, 1);

        let v: Value = http.put(&url).json(&json!({"enabled": false})).send().await.unwrap().json().await.unwrap();
        assert_eq!(v, json!({"enabled": false}));
        assert_eq!(http.post(&played).send().await.unwrap().status(), 200);
        assert_eq!(a.state.db.get_track("s1").unwrap().unwrap().play_count, 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sync_events_arrive_on_the_downloads_stream() {
        let a = spawn_backend("alpha").await;
        let http = reqwest::Client::new();
        let mut resp = http.get(format!("{}/api/downloads/events", a.api)).send().await.unwrap();
        // Give the SSE handler time to subscribe, then start a run (no devices).
        tokio::time::sleep(Duration::from_millis(100)).await;
        http.post(format!("{}/api/sync/run", a.api)).header("content-type", "application/json").send().await.unwrap();
        let mut text = String::new();
        while !text.contains("\"state\":\"done\"") {
            let chunk = tokio::time::timeout(Duration::from_secs(5), resp.chunk()).await.unwrap().unwrap().unwrap();
            text.push_str(&String::from_utf8_lossy(&chunk));
        }
        assert!(text.contains("event: sync"), "{text}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn changes_need_a_json_content_type() {
        // A cross-site page can send these without a CORS preflight only
        // with a "simple" content type (or none), so those are refused.
        let a = spawn_backend("alpha").await;
        let http = reqwest::Client::new();
        let code = sync::this_device(&a.state.db).unwrap().code;
        let pl = format!("{}/api/settings/private-listening", a.api);
        let calls = [
            http.post(format!("{}/api/sync/run", a.api)),
            http.post(format!("{}/api/sync/code", a.api)),
            http.post(format!("{}/api/sync/pair", a.api)),
            http.delete(format!("{}/api/sync/devices/someone", a.api)),
            http.put(&pl),
        ];
        for req in calls {
            let req = req.build().unwrap();
            let (method, url) = (req.method().clone(), req.url().clone());
            for ct in [None, Some("text/plain"), Some("application/x-www-form-urlencoded"), Some("multipart/form-data")] {
                let mut r = http.request(method.clone(), url.clone()).body(r#"{"enabled":true,"code":"ABCDEF"}"#);
                if let Some(ct) = ct {
                    r = r.header("content-type", ct);
                }
                let resp = r.send().await.unwrap();
                assert_eq!(resp.status(), 415, "{method} {url} with {ct:?}");
                let body: Value = resp.json().await.unwrap();
                assert!(body["error"].is_string());
            }
        }
        assert!(!a.state.db.private_listening().unwrap());
        assert_eq!(sync::this_device(&a.state.db).unwrap().code, code);
        assert!(!a.state.sync.is_syncing());
        // Reads stay open; JSON (with a charset) is accepted.
        assert_eq!(http.get(&pl).send().await.unwrap().status(), 200);
        assert_eq!(http.get(format!("{}/api/sync/devices", a.api)).send().await.unwrap().status(), 200);
        let resp =
            http.put(&pl).header("content-type", "application/json; charset=utf-8").body(r#"{"enabled":true}"#).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        assert!(a.state.db.private_listening().unwrap());
        let resp = http.post(format!("{}/api/sync/code", a.api)).header("content-type", "application/json").send().await.unwrap();
        assert_eq!(resp.status(), 200);
        assert_ne!(sync::this_device(&a.state.db).unwrap().code, code);
    }
}
