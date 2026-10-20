//! Playlists: create, rename, delete, add/remove songs, reorder.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use super::tracks::check_id;
use super::AppState;
use crate::db::ReorderError;
use crate::error::{ApiError, ApiResult};
use crate::models::{Playlist, PlaylistDetail};

#[derive(Deserialize)]
pub struct NameBody {
    name: String,
}

fn clean_name(name: &str) -> ApiResult<String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(ApiError::bad_request("playlist name must not be empty"));
    }
    if name.chars().count() > 120 {
        return Err(ApiError::bad_request("playlist name is too long (max 120 characters)"));
    }
    Ok(name.to_string())
}

fn ensure_playlist(s: &AppState, id: i64) -> ApiResult<()> {
    match s.db.playlist(id) {
        Ok(_) => Ok(()),
        Err(rusqlite::Error::QueryReturnedNoRows) => Err(ApiError::not_found("playlist not found")),
        Err(e) => Err(e.into()),
    }
}

pub async fn list(State(s): State<AppState>) -> ApiResult<Json<Vec<Playlist>>> {
    Ok(Json(s.db.playlists()?))
}

pub async fn create(State(s): State<AppState>, Json(body): Json<NameBody>) -> ApiResult<(StatusCode, Json<PlaylistDetail>)> {
    let id = s.db.create_playlist(&clean_name(&body.name)?, "user", None)?;
    Ok((StatusCode::CREATED, Json(s.db.playlist(id)?)))
}

pub async fn get_one(State(s): State<AppState>, Path(id): Path<i64>) -> ApiResult<Json<PlaylistDetail>> {
    s.db.playlist(id).map(Json).map_err(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => ApiError::not_found("playlist not found"),
        other => other.into(),
    })
}

pub async fn rename(State(s): State<AppState>, Path(id): Path<i64>, Json(body): Json<NameBody>) -> ApiResult<Json<PlaylistDetail>> {
    if !s.db.rename_playlist(id, &clean_name(&body.name)?)? {
        return Err(ApiError::not_found("playlist not found"));
    }
    Ok(Json(s.db.playlist(id)?))
}

/// Deletes the playlist only — never the music files.
pub async fn remove(State(s): State<AppState>, Path(id): Path<i64>) -> ApiResult<Json<Value>> {
    if !s.db.delete_playlist(id)? {
        return Err(ApiError::not_found("playlist not found"));
    }
    Ok(Json(json!({ "deleted": id })))
}

#[derive(Deserialize)]
pub struct AddBody {
    track_id: String,
}

pub async fn add_track(State(s): State<AppState>, Path(id): Path<i64>, Json(body): Json<AddBody>) -> ApiResult<Json<PlaylistDetail>> {
    check_id(&body.track_id)?;
    ensure_playlist(&s, id)?;
    if s.db.get_track(&body.track_id)?.is_none() {
        return Err(ApiError::not_found("track not found"));
    }
    s.db.add_to_playlist(id, &body.track_id)?;
    Ok(Json(s.db.playlist(id)?))
}

pub async fn remove_track(State(s): State<AppState>, Path((id, track_id)): Path<(i64, String)>) -> ApiResult<Json<PlaylistDetail>> {
    check_id(&track_id)?;
    if !s.db.remove_from_playlist(id, &track_id)? {
        return Err(ApiError::not_found("track is not in this playlist"));
    }
    Ok(Json(s.db.playlist(id)?))
}

#[derive(Deserialize)]
pub struct OrderBody {
    track_ids: Vec<String>,
}

pub async fn reorder(State(s): State<AppState>, Path(id): Path<i64>, Json(body): Json<OrderBody>) -> ApiResult<Json<PlaylistDetail>> {
    ensure_playlist(&s, id)?;
    match s.db.reorder_playlist(id, &body.track_ids) {
        Ok(()) => Ok(Json(s.db.playlist(id)?)),
        Err(ReorderError::Mismatch) => Err(ApiError::bad_request("track_ids must contain exactly the songs of this playlist, each once")),
        Err(ReorderError::Db(e)) => Err(e.into()),
    }
}
