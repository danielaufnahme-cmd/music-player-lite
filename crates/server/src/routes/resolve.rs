//! `GET /api/resolve?url=…[&as=song|playlist]`: what a pasted link is, with a
//! preview. Used by the phone download page.

use std::time::Duration;

use axum::extract::{Query, State};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use super::AppState;
use crate::error::{ApiError, ApiResult};
use crate::models::{PlaylistInfo, Track};
use crate::resolve::{classify, LinkKind};

#[derive(Deserialize)]
pub struct ResolveQuery {
    url: String,
    /// For a song inside a playlist: which one the user picked.
    #[serde(rename = "as")]
    pick: Option<String>,
}

pub async fn resolve(State(s): State<AppState>, Query(q): Query<ResolveQuery>) -> ApiResult<Json<Value>> {
    let kind = classify(&q.url).map_err(ApiError::bad_request)?;
    let kind = match (kind, q.pick.as_deref()) {
        (LinkKind::Ambiguous { video_id, .. }, Some("song")) => LinkKind::Song { video_id },
        (LinkKind::Ambiguous { playlist_source, .. }, Some("playlist")) => LinkKind::Playlist { source: playlist_source },
        (k, _) => k,
    };
    match kind {
        LinkKind::Ambiguous { video_id, playlist_source } => {
            Ok(Json(json!({ "kind": "ambiguous", "video_id": video_id, "playlist_url": playlist_source })))
        }
        LinkKind::Song { video_id } => {
            let track = song_preview(&s, &video_id).await?;
            Ok(Json(json!({ "kind": "song", "video_id": video_id, "track": track })))
        }
        LinkKind::Playlist { source } => {
            let info: PlaylistInfo = s.downloads.playlist_info(&source).await?;
            if info.tracks.is_empty() {
                return Err(ApiError::bad_request("That playlist has no available songs"));
            }
            let ids: Vec<String> = info.tracks.iter().map(|t| t.video_id.clone()).collect();
            let have = s.db.downloaded_set(&ids)?;
            let tracks: Vec<Track> = info
                .tracks
                .into_iter()
                .map(|mut t| {
                    t.downloaded = have.contains(&t.video_id);
                    t
                })
                .collect();
            Ok(Json(json!({ "kind": "playlist", "playlist_url": source, "title": info.title, "tracks": tracks })))
        }
    }
}

/// The library entry if we have the song, otherwise its YouTube Music metadata
/// (saved so the download gets the right title and artist).
async fn song_preview(s: &AppState, video_id: &str) -> ApiResult<Track> {
    if let Some(t) = s.db.get_track(video_id)? {
        if t.downloaded && t.file_path.as_deref().is_some_and(|p| std::path::Path::new(p).exists()) {
            return Ok(t);
        }
    }
    let data = s.music.request(json!({"cmd": "song_info", "video_id": video_id}), Duration::from_secs(60)).await?;
    let mut track: Track = serde_json::from_value(data).map_err(|e| ApiError::internal(format!("bad song info from provider: {e}")))?;
    s.db.upsert_track_meta(&track)?;
    track.downloaded = false;
    track.file_path = None;
    Ok(track)
}
