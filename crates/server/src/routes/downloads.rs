//! Download queue endpoints, including the Server-Sent Events progress stream.

use std::convert::Infallible;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::Json;
use futures_util::stream::Stream;
use serde::Deserialize;
use serde_json::Value;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

use super::tracks::check_id;
use super::AppState;
use crate::downloads::ImportTarget;
use crate::error::{ApiError, ApiResult};
use crate::models::{Download, PlaylistInfo};

#[derive(Deserialize)]
pub struct CreateBody {
    video_id: Option<String>,
    playlist_url: Option<String>,
    /// With `video_id`: add the song to this playlist when done.
    /// With `playlist_url`: import into this existing playlist instead of a new one.
    playlist_id: Option<i64>,
    /// Optional local name for a new imported playlist (defaults to the YouTube title).
    name: Option<String>,
    /// With `playlist_url`: `"songs_only"` downloads the missing songs into the
    /// library without creating or changing any playlist.
    mode: Option<String>,
}

/// The canonical playlist URL for a pasted playlist (or album) link.
pub(crate) fn playlist_source(url: &str) -> ApiResult<String> {
    crate::resolve::playlist_source_of(url)
        .ok_or_else(|| ApiError::bad_request("playlist_url must be a YouTube or YouTube Music playlist or album link"))
}

/// `{"video_id": "..."}` queues one song; `{"playlist_url": "...", "name": "..."}`
/// imports a whole playlist into a new one (`name` is optional), and
/// `{"playlist_url": "...", "playlist_id": 3}` appends it to an existing playlist.
pub async fn create(State(s): State<AppState>, Json(body): Json<CreateBody>) -> ApiResult<(StatusCode, Json<Value>)> {
    match (body.video_id, body.playlist_url) {
        (Some(id), None) => {
            check_id(&id)?;
            let d = s.downloads.enqueue(&id, body.playlist_id)?;
            Ok((StatusCode::ACCEPTED, Json(serde_json::to_value(d).map_err(|e| ApiError::internal(e.to_string()))?)))
        }
        (None, Some(url)) => {
            let source = playlist_source(&url)?;
            let target = match (body.mode.as_deref().map(str::trim), body.playlist_id) {
                (Some("songs_only"), None) => ImportTarget::LibraryOnly,
                (Some("songs_only"), Some(_)) => return Err(ApiError::bad_request("songs_only imports don't take a playlist_id")),
                (Some(m), _) if !m.is_empty() && m != "playlist" => {
                    return Err(ApiError::bad_request("mode must be songs_only or omitted"))
                }
                (_, Some(id)) => ImportTarget::Existing(id),
                (_, None) => ImportTarget::New { name: body.name.as_deref() },
            };
            let import = s.downloads.enqueue_playlist(&source, target).await?;
            Ok((StatusCode::ACCEPTED, Json(serde_json::to_value(import).map_err(|e| ApiError::internal(e.to_string()))?)))
        }
        _ => Err(ApiError::bad_request("send exactly one of video_id or playlist_url")),
    }
}

#[derive(Deserialize)]
pub struct ListQuery {
    limit: Option<usize>,
}

pub async fn list(State(s): State<AppState>, Query(p): Query<ListQuery>) -> ApiResult<Json<Vec<Download>>> {
    Ok(Json(s.db.downloads(p.limit.unwrap_or(200).clamp(1, 1000))?))
}

pub async fn retry(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Download>> {
    Ok(Json(s.downloads.retry(&id)?))
}

/// Every download change as an SSE `download` event carrying the download
/// JSON, plus device-sync progress as `sync` events. If a slow client misses
/// events it gets a `resync` event and should re-fetch `GET /api/downloads`.
pub async fn events(State(s): State<AppState>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let downloads = BroadcastStream::new(s.downloads.subscribe()).map(|msg| {
        Ok(match msg {
            Ok(d) => Event::default().event("download").json_data(&d).unwrap_or_else(|_| Event::default().event("resync").data("{}")),
            Err(BroadcastStreamRecvError::Lagged(_)) => Event::default().event("resync").data("{}"),
        })
    });
    let sync = BroadcastStream::new(s.sync.subscribe())
        .filter_map(|msg| msg.ok().and_then(|ev| Event::default().event("sync").json_data(&ev).ok()).map(Ok));
    let stream = downloads.merge(sync);
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

#[derive(Deserialize)]
pub struct InfoQuery {
    url: String,
}

/// Preview a playlist before downloading it.
pub async fn playlist_info(State(s): State<AppState>, Query(p): Query<InfoQuery>) -> ApiResult<Json<PlaylistInfoOut>> {
    let source = playlist_source(&p.url)?;
    let info: PlaylistInfo = s.downloads.playlist_info(&source).await?;
    let ids: Vec<String> = info.tracks.iter().map(|t| t.video_id.clone()).collect();
    let have = s.db.downloaded_set(&ids)?;
    let tracks = info
        .tracks
        .into_iter()
        .map(|mut t| {
            t.downloaded = have.contains(&t.video_id);
            t
        })
        .collect();
    Ok(Json(PlaylistInfoOut { title: info.title, tracks }))
}

#[derive(serde::Serialize)]
pub struct PlaylistInfoOut {
    pub(crate) title: String,
    pub(crate) tracks: Vec<crate::models::Track>,
}

/// Remove one finished download record from the list. The music file stays.
pub async fn remove(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    match s.db.get_download(&id)? {
        None => Err(ApiError::not_found("download not found")),
        Some(d) if d.status == "queued" || d.status == "downloading" => {
            Err(ApiError::bad_request("this download is still running; wait for it to finish"))
        }
        Some(_) => {
            s.db.delete_download(&id)?;
            Ok(Json(serde_json::json!({ "removed": 1 })))
        }
    }
}

#[derive(Deserialize)]
pub struct ClearBody {
    /// "done" (also clears skipped), "failed", or omitted for all finished ones.
    status: Option<String>,
    /// Only these records (e.g. the user's selection).
    ids: Option<Vec<String>>,
}

/// Bulk-remove finished download records. Running downloads and music files
/// are never touched.
pub async fn clear(State(s): State<AppState>, Json(body): Json<ClearBody>) -> ApiResult<Json<Value>> {
    let status = body.status.as_deref().map(str::trim).filter(|s| !s.is_empty());
    if let Some(st) = status {
        if !matches!(st, "done" | "failed" | "finished") {
            return Err(ApiError::bad_request("status must be done, failed or finished"));
        }
    }
    let status = status.filter(|st| *st != "finished");
    let removed = s.db.clear_downloads(status, body.ids.as_deref())?;
    Ok(Json(serde_json::json!({ "removed": removed })))
}
