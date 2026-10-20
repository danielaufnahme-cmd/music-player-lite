//! HTTP API. One module per area; this file wires them together and holds
//! the shared state plus the small endpoints (health, search).

pub mod downloads;
pub mod playlists;
pub mod recommendations;
pub mod resolve;
pub mod sync;
pub mod tracks;
pub mod web;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Query, Request, State};
use axum::http::{header, HeaderValue, Method};
use axum::middleware::Next;
use axum::response::Response;
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use tower_http::cors::{AllowOrigin, CorsLayer};

use crate::db::Db;
use crate::downloads::DownloadManager;
use crate::error::{ApiError, ApiResult};
use crate::models::Track;
use crate::provider::Music;

/// A resolved direct audio URL for previewing a song that isn't downloaded.
#[derive(Clone)]
pub struct StreamSource {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub resolved: Instant,
}

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Db>,
    pub music: Music,
    pub downloads: DownloadManager,
    pub sync: crate::sync::SyncManager,
    pub http: reqwest::Client,
    pub covers_dir: PathBuf,
    pub stream_cache: Arc<Mutex<HashMap<String, StreamSource>>>,
    /// Only one recommendation refresh runs at a time.
    pub recs_lock: Arc<tokio::sync::Mutex<()>>,
    /// Device-sync listener, started / stopped by the `device_sync` setting.
    pub sync_listener: Arc<crate::settings::SyncListener>,
}

pub fn router(state: AppState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(|origin: &HeaderValue, _| origin.to_str().is_ok_and(is_local_origin)))
        .allow_methods([Method::GET, Method::POST, Method::PUT, Method::PATCH, Method::DELETE])
        .allow_headers(tower_http::cors::Any);

    use crate::settings as cfg;
    let phone = Router::new()
        .route("/", get(web::index))
        .route("/manifest.webmanifest", get(web::manifest))
        .route("/sw.js", get(web::service_worker))
        .route("/icon-192.png", get(web::icon_192))
        .route("/icon-512.png", get(web::icon_512))
        .route("/apple-touch-icon.png", get(web::apple_touch_icon))
        .route_layer(axum::middleware::from_fn_with_state(state.clone(), cfg::phone_page_gate));
    let sync_gate = || axum::middleware::from_fn_with_state(state.clone(), cfg::device_sync_gate);

    Router::new()
        .merge(phone)
        .route("/api/settings", get(cfg::get_settings).patch(cfg::patch_settings).route_layer(axum::middleware::from_fn(require_json)))
        .route("/api/health", get(health))
        .route("/api/resolve", get(resolve::resolve))
        .route("/api/search", get(search))
        .route("/api/playlist-info", get(downloads::playlist_info))
        .route("/api/tracks", get(tracks::list))
        .route("/api/tracks/:id", get(tracks::get_one).delete(tracks::remove))
        .route("/api/tracks/:id/stream", get(tracks::stream))
        .route("/api/tracks/:id/cover", get(tracks::cover))
        .route("/api/tracks/:id/played", post(tracks::played))
        .route("/api/tracks/:id/lyrics", get(tracks::lyrics))
        .route("/api/artists", get(tracks::artists))
        .route("/api/albums", get(tracks::albums))
        .route("/api/playlists", get(playlists::list).post(playlists::create))
        .route("/api/playlists/:id", get(playlists::get_one).patch(playlists::rename).delete(playlists::remove))
        .route("/api/playlists/:id/tracks", post(playlists::add_track))
        .route("/api/playlists/:id/tracks/:track_id", delete(playlists::remove_track))
        .route("/api/playlists/:id/order", put(playlists::reorder))
        .route("/api/downloads", get(downloads::list).post(downloads::create))
        .route("/api/downloads/events", get(downloads::events))
        .route("/api/downloads/clear", post(downloads::clear))
        .route("/api/downloads/:id", delete(downloads::remove))
        .route("/api/downloads/:id/retry", post(downloads::retry))
        .route("/api/recommendations", get(recommendations::list))
        .route("/api/recommendations/refresh", post(recommendations::refresh))
        .route(
            "/api/settings/private-listening",
            get(tracks::get_private_listening).put(tracks::put_private_listening).route_layer(axum::middleware::from_fn(require_json)),
        )
        .route("/api/sync/devices", get(sync::devices))
        .route("/api/sync/devices/:id", delete(sync::remove_device).route_layer(axum::middleware::from_fn(require_json)))
        .route("/api/sync/code", post(sync::new_code).route_layer(axum::middleware::from_fn(require_json)))
        .route("/api/sync/pair", post(sync::pair).route_layer(sync_gate()).route_layer(axum::middleware::from_fn(require_json)))
        .route("/api/sync/run", post(sync::run).route_layer(sync_gate()).route_layer(axum::middleware::from_fn(require_json)))
        .layer(axum::middleware::from_fn(json_errors))
        .layer(cors)
        .with_state(state)
}

/// Only pages served from this machine may call the API: the origin's host
/// must be exactly localhost, 127.0.0.1 or [::1] (any port).
pub fn is_local_origin(origin: &str) -> bool {
    let Some(rest) = origin.strip_prefix("http://").or_else(|| origin.strip_prefix("https://")) else {
        return false;
    };
    let (host, port) = if rest.starts_with('[') {
        let Some(end) = rest.find(']') else {
            return false;
        };
        let (host, tail) = rest.split_at(end + 1);
        if tail.is_empty() {
            (host, None)
        } else if let Some(port) = tail.strip_prefix(':') {
            (host, Some(port))
        } else {
            return false;
        }
    } else {
        match rest.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (rest, None),
        }
    };
    if let Some(port) = port {
        if port.is_empty() || port.len() > 5 || !port.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
    }
    matches!(host, "localhost" | "127.0.0.1" | "[::1]")
}

/// Settings and device-sync changes must say `Content-Type: application/json`
/// (even without a body). A web page can only send that after a CORS
/// preflight, which `is_local_origin` refuses for other sites, so a page you
/// visit can't pair devices, start syncs or flip settings behind your back.
/// Reads (GET/HEAD) pass through.
pub(crate) async fn require_json(req: Request, next: Next) -> Response {
    if matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS) {
        return next.run(req).await;
    }
    let is_json = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("application/json"));
    if is_json {
        next.run(req).await
    } else {
        use axum::response::IntoResponse;
        ApiError::status(axum::http::StatusCode::UNSUPPORTED_MEDIA_TYPE, "send Content-Type: application/json").into_response()
    }
}

/// Errors produced outside our handlers (bad JSON body, bad query, unknown
/// route, unsatisfiable Range) come as plain text; reshape them to the API's
/// `{"error": "..."}` form, keeping status and headers like Content-Range.
pub(crate) async fn json_errors(req: Request, next: Next) -> Response {
    let res = next.run(req).await;
    let status = res.status();
    let is_json = res.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|v| v.starts_with("application/json"));
    if !(status.is_client_error() || status.is_server_error()) || is_json {
        return res;
    }
    let (mut parts, body) = res.into_parts();
    let bytes = axum::body::to_bytes(body, 64 * 1024).await.unwrap_or_default();
    let text = String::from_utf8_lossy(&bytes).trim().to_string();
    let message = if text.is_empty() { status.canonical_reason().unwrap_or("request failed").to_lowercase() } else { text };
    parts.headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    parts.headers.remove(header::CONTENT_LENGTH);
    Response::from_parts(parts, Body::from(json!({ "error": message }).to_string()))
}

async fn health(State(s): State<AppState>) -> Json<Value> {
    Json(json!({
        "status": "ok",
        // "worker" kept for clients that check it (phone page, old app).
        "worker": if s.music.is_running() { "running" } else { "stopped" },
    }))
}

#[derive(Deserialize)]
pub struct SearchQuery {
    q: Option<String>,
    limit: Option<usize>,
}

async fn search(State(s): State<AppState>, Query(p): Query<SearchQuery>) -> ApiResult<Json<Vec<Track>>> {
    let q = p.q.unwrap_or_default();
    if q.trim().is_empty() {
        return Err(ApiError::bad_request("query parameter q is required"));
    }
    let limit = p.limit.unwrap_or(20).clamp(1, 50);
    let data = s.music.request(json!({"cmd": "search", "query": q.trim(), "limit": limit}), Duration::from_secs(45)).await?;
    let mut tracks: Vec<Track> = serde_json::from_value(data.get("tracks").cloned().unwrap_or(data))
        .map_err(|e| ApiError::internal(format!("bad search result from provider: {e}")))?;
    for t in &tracks {
        s.db.upsert_track_meta(t)?;
    }
    let ids: Vec<String> = tracks.iter().map(|t| t.video_id.clone()).collect();
    let have = s.db.downloaded_set(&ids)?;
    for t in &mut tracks {
        if have.contains(&t.video_id) {
            if let Some(local) = s.db.get_track(&t.video_id)? {
                *t = local;
            }
        }
    }
    Ok(Json(tracks))
}

#[cfg(test)]
mod tests {
    use super::is_local_origin;

    #[test]
    fn only_exact_local_hosts_are_allowed() {
        for ok in [
            "http://localhost",
            "http://localhost:5173",
            "https://localhost:8443",
            "http://127.0.0.1:7802",
            "http://[::1]:3000",
            "http://[::1]",
        ] {
            assert!(is_local_origin(ok), "{ok} should be allowed");
        }
        for bad in [
            "http://localhost.evil.com",
            "http://localhost.evil.com:80",
            "http://127.0.0.1.nip.io",
            "http://evil.com/?localhost",
            "http://localhost:80/x",
            "http://localhost:abc",
            "file://localhost",
            "null",
            "http://[::1]evil",
            "http://user@localhost",
        ] {
            assert!(!is_local_origin(bad), "{bad} must be rejected");
        }
    }
}
