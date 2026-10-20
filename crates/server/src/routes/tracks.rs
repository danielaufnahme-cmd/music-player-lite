//! Library songs, artists, albums, audio streaming and cover art.

use std::path::Path as FsPath;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Path, Query, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use tower::ServiceExt;
use tower_http::services::ServeFile;

use super::{AppState, StreamSource};
use crate::error::{ApiError, ApiResult};
use crate::models::{Album, Artist, Track};

/// YouTube video ids are short and URL-safe; anything else is rejected
/// (this also keeps ids safe to use in cache file names).
pub fn check_id(id: &str) -> ApiResult<()> {
    let ok = !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if ok {
        Ok(())
    } else {
        Err(ApiError::bad_request("invalid track id"))
    }
}

#[derive(Deserialize)]
pub struct ListQuery {
    sort: Option<String>,
    q: Option<String>,
}

pub async fn list(State(s): State<AppState>, Query(p): Query<ListQuery>) -> ApiResult<Json<Vec<Track>>> {
    Ok(Json(s.db.library_tracks(p.sort.as_deref(), p.q.as_deref())?))
}

pub async fn get_one(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Track>> {
    check_id(&id)?;
    s.db.get_track(&id)?.map(Json).ok_or_else(|| ApiError::not_found("track not found"))
}

#[derive(Deserialize)]
pub struct DeleteQuery {
    #[serde(default)]
    delete_file: bool,
}

pub async fn remove(State(s): State<AppState>, Path(id): Path<String>, Query(p): Query<DeleteQuery>) -> ApiResult<Json<Value>> {
    check_id(&id)?;
    let (file, cover) = s.db.delete_track(&id)?.ok_or_else(|| ApiError::not_found("track not found"))?;
    let mut file_deleted = false;
    if p.delete_file {
        if let Some(f) = file.filter(|f| FsPath::new(f).exists()) {
            tokio::fs::remove_file(&f).await?;
            file_deleted = true;
        }
        if let Some(c) = cover {
            let _ = tokio::fs::remove_file(c).await;
        }
    }
    Ok(Json(json!({ "deleted": id, "file_deleted": file_deleted })))
}

/// Count a listen. With private listening on, nothing is recorded (no play
/// count, last played or plays row) and the answer is 204 without a body.
pub async fn played(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Response> {
    check_id(&id)?;
    if s.db.private_listening()? {
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    let track = s.db.record_play(&id)?.ok_or_else(|| ApiError::not_found("track not found"))?;
    Ok(Json(track).into_response())
}

#[derive(Deserialize)]
pub struct PrivateListening {
    enabled: bool,
}

pub async fn get_private_listening(State(s): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({ "enabled": s.db.private_listening()? })))
}

pub async fn put_private_listening(State(s): State<AppState>, Json(body): Json<PrivateListening>) -> ApiResult<Json<Value>> {
    s.db.set_private_listening(body.enabled)?;
    Ok(Json(json!({ "enabled": s.db.private_listening()? })))
}

pub async fn artists(State(s): State<AppState>) -> ApiResult<Json<Vec<Artist>>> {
    Ok(Json(s.db.artists()?))
}

pub async fn albums(State(s): State<AppState>) -> ApiResult<Json<Vec<Album>>> {
    Ok(Json(s.db.albums()?))
}

async fn serve_file(path: &FsPath, req: Request) -> ApiResult<Response> {
    // ServeFile handles Range / If-Range / HEAD and sets the content type.
    let res = ServeFile::new(path).oneshot(req).await.map_err(|e| ApiError::internal(format!("serving file failed: {e}")))?;
    Ok(res.map(Body::new))
}

const STREAM_URL_TTL: Duration = Duration::from_secs(60 * 60);

async fn resolve_stream(s: &AppState, id: &str, force: bool) -> ApiResult<StreamSource> {
    if !force {
        let cache = s.stream_cache.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(src) = cache.get(id).filter(|src| src.resolved.elapsed() < STREAM_URL_TTL) {
            return Ok(src.clone());
        }
    }
    let data = s.music.request(json!({"cmd": "stream_url", "video_id": id}), Duration::from_secs(60)).await?;
    let url = data.get("url").and_then(Value::as_str).ok_or_else(|| ApiError::internal("provider returned no stream url"))?.to_string();
    let headers = data
        .get("http_headers")
        .and_then(Value::as_object)
        .map(|h| h.iter().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string()))).collect())
        .unwrap_or_default();
    let src = StreamSource { url, headers, resolved: Instant::now() };
    let mut cache = s.stream_cache.lock().unwrap_or_else(|p| p.into_inner());
    // Keep the cache small: drop expired URLs, and everything if it still grows.
    cache.retain(|_, v| v.resolved.elapsed() < STREAM_URL_TTL);
    if cache.len() >= 128 {
        cache.clear();
    }
    cache.insert(id.to_string(), src.clone());
    drop(cache);
    Ok(src)
}

/// Audio for a song. Downloaded songs are served from disk with full Range
/// support; other songs (search results, recommendations) are proxied from
/// YouTube so they can be previewed. The app only ever talks to this server.
pub async fn stream(State(s): State<AppState>, Path(id): Path<String>, req: Request) -> ApiResult<Response> {
    check_id(&id)?;
    let track = s.db.get_track(&id)?.ok_or_else(|| ApiError::not_found("track not found"))?;
    if let Some(path) = track.file_path.as_deref().filter(|p| FsPath::new(p).exists()) {
        return serve_file(FsPath::new(path), req).await;
    }

    let range = req.headers().get(header::RANGE).cloned();
    for attempt in 0..2 {
        let src = resolve_stream(&s, &id, attempt > 0).await?;
        let mut upstream = s.http.get(&src.url);
        for (k, v) in &src.headers {
            upstream = upstream.header(k.as_str(), v.as_str());
        }
        if let Some(r) = &range {
            upstream = upstream.header(header::RANGE, r.clone());
        }
        let resp = upstream.send().await?;
        // Signed URLs expire; resolve a fresh one once.
        if resp.status() == StatusCode::FORBIDDEN && attempt == 0 {
            continue;
        }
        if !resp.status().is_success() {
            return Err(ApiError::Unavailable(format!("YouTube returned {}", resp.status())));
        }
        let mut out = Response::builder().status(resp.status());
        for name in [header::CONTENT_TYPE, header::CONTENT_LENGTH, header::CONTENT_RANGE, header::ACCEPT_RANGES] {
            if let Some(v) = resp.headers().get(&name) {
                out = out.header(name, v.clone());
            }
        }
        if !resp.headers().contains_key(header::ACCEPT_RANGES) {
            out = out.header(header::ACCEPT_RANGES, "bytes");
        }
        return out.body(Body::from_stream(resp.bytes_stream())).map_err(|e| ApiError::internal(e.to_string()));
    }
    Err(ApiError::Unavailable("could not get a stream for this song".into()))
}

/// Cover art: the embedded cover saved at download time, or the YouTube
/// thumbnail fetched once and cached on disk.
pub async fn cover(State(s): State<AppState>, Path(id): Path<String>, req: Request) -> ApiResult<Response> {
    check_id(&id)?;
    let (cover_path, thumbnail_url) = s.db.cover_path(&id)?.ok_or_else(|| ApiError::not_found("track not found"))?;

    let local = cover_path.filter(|p| FsPath::new(p).exists());
    let cached = s.covers_dir.join(format!("{id}.jpg"));
    let path = match local {
        Some(p) => Some(std::path::PathBuf::from(p)),
        None if cached.exists() => Some(cached.clone()),
        None => None,
    };
    if let Some(p) = path {
        let mut res = serve_file(&p, req).await?;
        res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=86400"));
        return Ok(res);
    }

    if thumbnail_url.is_empty() {
        return Err(ApiError::not_found("no cover for this track"));
    }
    let resp = s.http.get(&thumbnail_url).send().await?;
    if !resp.status().is_success() {
        return Err(ApiError::not_found(format!("cover unavailable ({})", resp.status())));
    }
    let content_type = resp.headers().get(header::CONTENT_TYPE).cloned().unwrap_or_else(|| HeaderValue::from_static("image/jpeg"));
    let bytes = resp.bytes().await?;
    if let Err(e) = tokio::fs::write(&cached, &bytes).await {
        tracing::warn!("could not cache cover {}: {e}", cached.display());
    }
    Ok(([(header::CONTENT_TYPE, content_type), (header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=86400"))], bytes)
        .into_response())
}

/// Misses are re-checked after this long (lyrics get added to sources over time).
const LYRICS_MISS_TTL_MS: i64 = 3 * 24 * 60 * 60 * 1000;

/// Lyrics for any known song (downloaded or not):
/// `{"synced": bool, "lines": [{"time_ms": int|null, "text": str}], "source": str|null}`.
pub async fn lyrics(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    check_id(&id)?;
    if let Some((cached, found, at)) = s.db.get_lyrics(&id)? {
        if found || crate::models::now_ms() - at < LYRICS_MISS_TTL_MS {
            if let Ok(v) = serde_json::from_str::<Value>(&cached) {
                return Ok(Json(v));
            }
        }
    }
    let track = s.db.get_track(&id)?.ok_or_else(|| ApiError::not_found("track not found"))?;
    let mut data = s
        .music
        .request(
            json!({
                "cmd": "lyrics",
                "video_id": id,
                "title": track.title,
                "artist": track.artist,
                "duration_sec": track.duration_sec,
            }),
            Duration::from_secs(45),
        )
        .await?;
    let transient = data.get("transient").and_then(Value::as_bool).unwrap_or(false);
    if let Some(obj) = data.as_object_mut() {
        obj.remove("transient");
    }
    let found = data.get("lines").and_then(Value::as_array).is_some_and(|l| !l.is_empty());
    if found || !transient {
        s.db.set_lyrics(&id, &data.to_string(), found)?;
    }
    Ok(Json(data))
}
