//! Podcast endpoints: search, subscriptions, episodes, downloads, listening
//! position and cover art. Download progress arrives on `GET /api/podcasts/events`.

use std::convert::Infallible;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::Extension;
use axum::{Json, Router};
use futures_util::stream::Stream;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

use super::{require_json, AppState};
use crate::error::{ApiError, ApiResult};
use crate::podcasts::{Episode, Podcast, PodcastManager, SearchHit};

pub fn router(podcasts: PodcastManager) -> Router<AppState> {
    let json_only = || axum::middleware::from_fn(require_json);
    Router::new()
        .route("/api/podcasts", get(list).post(subscribe).route_layer(json_only()))
        .route("/api/podcasts/search", get(search))
        .route("/api/podcasts/events", get(events))
        .route("/api/podcasts/:id", delete(unsubscribe).route_layer(json_only()))
        .route("/api/podcasts/:id/episodes", get(episodes))
        .route("/api/podcasts/:id/refresh", post(refresh).route_layer(json_only()))
        .route("/api/podcasts/:id/cover", get(cover))
        .route("/api/podcasts/episodes/:id/download", post(download).route_layer(json_only()))
        .route("/api/podcasts/episodes/:id/file", delete(delete_file).route_layer(json_only()))
        .route("/api/podcasts/episodes/:id/position", put(position).route_layer(json_only()))
        .layer(Extension(podcasts))
}

#[derive(Deserialize)]
pub struct SearchQuery {
    q: Option<String>,
}

/// Search iTunes; every hit has the feed URL to subscribe with.
async fn search(Extension(m): Extension<PodcastManager>, Query(p): Query<SearchQuery>) -> ApiResult<Json<Vec<SearchHit>>> {
    let q = p.q.unwrap_or_default();
    if q.trim().is_empty() {
        return Err(ApiError::bad_request("query parameter q is required"));
    }
    Ok(Json(m.search(q.trim()).await?))
}

/// Subscribed podcasts.
async fn list(State(s): State<AppState>) -> ApiResult<Json<Vec<Podcast>>> {
    Ok(Json(s.db.subscribed_podcasts()?))
}

#[derive(Deserialize)]
pub struct SubscribeBody {
    feed_url: String,
    /// Cover to use instead of the feed's own (e.g. the iTunes artwork of a search hit).
    image_url: Option<String>,
}

/// `{"feed_url": "..."}` subscribes to a search hit or a pasted RSS link.
async fn subscribe(Extension(m): Extension<PodcastManager>, Json(body): Json<SubscribeBody>) -> ApiResult<(StatusCode, Json<Podcast>)> {
    let p = m.subscribe(&body.feed_url, body.image_url.as_deref()).await?;
    Ok((StatusCode::CREATED, Json(p)))
}

/// Stop following a podcast. Its episodes, positions and downloaded files stay.
async fn unsubscribe(Extension(m): Extension<PodcastManager>, Path(id): Path<i64>) -> ApiResult<Json<Value>> {
    m.unsubscribe(id)?;
    Ok(Json(json!({ "unsubscribed": id })))
}

#[derive(Deserialize)]
pub struct EpisodesQuery {
    limit: Option<usize>,
}

/// Episodes of a podcast, newest first.
async fn episodes(State(s): State<AppState>, Path(id): Path<i64>, Query(p): Query<EpisodesQuery>) -> ApiResult<Json<Vec<Episode>>> {
    if s.db.podcast(id)?.is_none() {
        return Err(ApiError::not_found("podcast not found"));
    }
    Ok(Json(s.db.episodes(id, p.limit.unwrap_or(200).clamp(1, 1000))?))
}

/// Check the feed now. Answers with the number of new episodes.
async fn refresh(Extension(m): Extension<PodcastManager>, Path(id): Path<i64>) -> ApiResult<Json<Value>> {
    let new = m.refresh(id).await?;
    Ok(Json(json!({ "new": new })))
}

async fn download(Extension(m): Extension<PodcastManager>, Path(id): Path<i64>) -> ApiResult<(StatusCode, Json<Episode>)> {
    Ok((StatusCode::ACCEPTED, Json(m.download(id)?)))
}

/// Delete the downloaded file of an episode (only files the app downloaded).
async fn delete_file(Extension(m): Extension<PodcastManager>, Path(id): Path<i64>) -> ApiResult<Json<Episode>> {
    Ok(Json(m.delete_file(id).await?))
}

#[derive(Deserialize)]
pub struct PositionBody {
    position_sec: f64,
    duration_sec: Option<f64>,
}

/// Save the listening position; from 95 % on the episode is marked played.
async fn position(
    Extension(m): Extension<PodcastManager>,
    Path(id): Path<i64>,
    Json(body): Json<PositionBody>,
) -> ApiResult<Json<Episode>> {
    Ok(Json(m.save_position(id, body.position_sec, body.duration_sec)?))
}

async fn cover(Extension(m): Extension<PodcastManager>, Path(id): Path<i64>) -> ApiResult<Response> {
    let (path, content_type) = m.cover(id).await?;
    let bytes = tokio::fs::read(path).await?;
    let mut res = bytes.into_response();
    res.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=86400"));
    Ok(res)
}

/// Every episode change (download progress, finished, failed) as an SSE
/// `episode` event carrying the episode JSON. A slow client gets `resync`
/// and should re-fetch the episode list.
async fn events(Extension(m): Extension<PodcastManager>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = BroadcastStream::new(m.subscribe_events()).map(|msg| {
        Ok(match msg {
            Ok(ep) => Event::default().event("episode").json_data(&ep).unwrap_or_else(|_| Event::default().event("resync").data("{}")),
            Err(BroadcastStreamRecvError::Lagged(_)) => Event::default().event("resync").data("{}"),
        })
    });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}
