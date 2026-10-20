//! Shared data types. Field names match the JSON contract used by the
//! UI, the phone page and the music provider (geonix_yt) exactly.

use serde::{Deserialize, Serialize};

/// A song. The same shape is used for library songs, search results and
/// recommendations; `downloaded` tells them apart.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Track {
    pub video_id: String,
    pub title: String,
    #[serde(default)]
    pub artist: String,
    #[serde(default)]
    pub album: String,
    #[serde(default)]
    pub duration_sec: i64,
    #[serde(default)]
    pub thumbnail_url: String,
    #[serde(default)]
    pub file_path: Option<String>,
    #[serde(default)]
    pub downloaded: bool,
    #[serde(default)]
    pub play_count: i64,
    #[serde(default)]
    pub last_played: Option<i64>,
    #[serde(default)]
    pub added_at: Option<i64>,
}

/// Extra fields the provider returns after a download. Never sent to the app.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct DownloadedTrack {
    #[serde(flatten)]
    pub track: Track,
    #[serde(default)]
    pub cover_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Playlist {
    pub id: i64,
    pub name: String,
    /// "user", "imported" (from a YouTube playlist) or "discover".
    pub kind: String,
    pub source_url: Option<String>,
    pub track_count: i64,
    pub duration_sec: i64,
    /// First four tracks, used by the app to draw the 2×2 cover mosaic.
    pub cover_track_ids: Vec<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlaylistDetail {
    #[serde(flatten)]
    pub playlist: Playlist,
    pub tracks: Vec<Track>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Download {
    pub id: String,
    pub video_id: String,
    pub title: String,
    pub artist: String,
    /// "queued", "downloading", "done", "failed" or "skipped".
    pub status: String,
    pub progress: f64,
    pub speed: Option<String>,
    pub error: Option<String>,
    pub playlist_id: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Recommendation {
    #[serde(flatten)]
    pub track: Track,
    pub score: f64,
    pub because_video_id: Option<String>,
    pub because_artist: Option<String>,
}

/// What the provider returns for each recommended song.
#[derive(Debug, Clone, Deserialize)]
pub struct WorkerRecommendation {
    #[serde(flatten)]
    pub track: Track,
    #[serde(default)]
    pub score: f64,
    #[serde(default)]
    pub because_video_id: Option<String>,
    #[serde(default)]
    pub because_artist: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Artist {
    pub name: String,
    pub track_count: i64,
    pub cover_track_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Album {
    pub name: String,
    pub artist: String,
    pub track_count: i64,
    pub cover_track_id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PlaylistInfo {
    pub title: String,
    pub tracks: Vec<Track>,
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}
