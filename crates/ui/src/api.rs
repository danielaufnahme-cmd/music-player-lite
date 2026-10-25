//! HTTP client for the Geonix server: same endpoints and SSE events as
//! app/lib/api/client.dart.

use std::time::Duration;

use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct Track {
    pub video_id: String,
    #[serde(deserialize_with = "null_default")]
    pub title: String,
    #[serde(deserialize_with = "null_default")]
    pub artist: String,
    #[serde(deserialize_with = "null_default")]
    pub album: String,
    #[serde(deserialize_with = "null_default")]
    pub duration_sec: i64,
    #[serde(deserialize_with = "null_default")]
    pub thumbnail_url: String,
    pub file_path: Option<String>,
    #[serde(deserialize_with = "null_default")]
    pub downloaded: bool,
    #[serde(deserialize_with = "null_default")]
    pub play_count: i64,
    pub last_played: Option<i64>,
    pub added_at: Option<i64>,
    pub because_artist: Option<String>,
    pub because_video_id: Option<String>,
}

fn null_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Artist {
    #[serde(deserialize_with = "null_default")]
    pub name: String,
    pub track_count: i64,
    #[serde(deserialize_with = "null_default")]
    pub cover_track_id: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Album {
    #[serde(deserialize_with = "null_default")]
    pub name: String,
    #[serde(deserialize_with = "null_default")]
    pub artist: String,
    pub track_count: i64,
    #[serde(deserialize_with = "null_default")]
    pub cover_track_id: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Playlist {
    pub id: i64,
    #[serde(deserialize_with = "null_default")]
    pub name: String,
    #[serde(deserialize_with = "null_default")]
    pub kind: String,
    pub source_url: Option<String>,
    pub track_count: i64,
    pub duration_sec: i64,
    pub cover_track_ids: Vec<String>,
    pub tracks: Vec<Track>,
}

impl Playlist {
    pub fn is_discover(&self) -> bool {
        self.kind == "discover"
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct PlaylistPreview {
    #[serde(deserialize_with = "null_default")]
    pub title: String,
    pub tracks: Vec<Track>,
    /// Songs of a Spotify / Apple Music / Deezer / SoundCloud list not found on YouTube Music.
    pub unmatched: Vec<String>,
    /// e.g. "only first 100 songs"
    pub note: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct Download {
    pub id: String,
    pub video_id: String,
    #[serde(deserialize_with = "null_default")]
    pub title: String,
    #[serde(deserialize_with = "null_default")]
    pub artist: String,
    #[serde(deserialize_with = "null_default")]
    pub status: String,
    #[serde(deserialize_with = "null_default")]
    pub progress: f64,
    pub speed: Option<String>,
    pub error: Option<String>,
    pub playlist_id: Option<i64>,
    pub created_at: Option<i64>,
}

impl Download {
    pub fn is_active(&self) -> bool {
        self.status == "queued" || self.status == "downloading"
    }
    pub fn is_failed(&self) -> bool {
        self.status == "failed"
    }
    pub fn is_finished(&self) -> bool {
        self.status == "done" || self.status == "skipped"
    }
}

#[derive(Clone, Debug, Default)]
pub struct Lyrics {
    pub synced: bool,
    pub lines: Vec<(Option<i64>, String)>,
    pub source: Option<String>,
}

impl Lyrics {
    pub fn from_json(v: &Value) -> Lyrics {
        let mut lines = Vec::new();
        if let Some(arr) = v.get("lines").and_then(|l| l.as_array()) {
            for l in arr {
                if l.is_object() {
                    let t = l.get("time_ms").and_then(|t| t.as_f64()).map(|t| t as i64);
                    let text = l.get("text").and_then(|t| t.as_str()).unwrap_or("").trim().to_string();
                    lines.push((t, text));
                }
            }
        }
        let synced = v.get("synced").and_then(|s| s.as_bool()) == Some(true) && lines.iter().all(|l| l.0.is_some());
        Lyrics { synced, lines, source: v.get("source").and_then(|s| s.as_str()).map(String::from) }
    }

    /// Index of the line being sung at `pos_ms` (+250 ms lead), or -1.
    pub fn line_at(&self, pos_ms: i64) -> i32 {
        if !self.synced || self.lines.is_empty() {
            return -1;
        }
        let ms = pos_ms + 250;
        let (mut lo, mut hi, mut found) = (0i64, self.lines.len() as i64 - 1, -1i64);
        while lo <= hi {
            let mid = (lo + hi) / 2;
            if self.lines[mid as usize].0.unwrap_or(0) <= ms {
                found = mid;
                lo = mid + 1;
            } else {
                hi = mid - 1;
            }
        }
        found as i32
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct ThisDevice {
    pub id: String,
    pub name: String,
    pub code: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct SyncDevice {
    pub id: String,
    pub name: String,
    pub address: String,
    pub online: bool,
    pub last_sync_at: Option<Value>,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct SyncStatus {
    pub this_device: ThisDevice,
    pub devices: Vec<SyncDevice>,
    pub syncing: bool,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct SyncEvent {
    pub state: String,
    pub device: String,
    pub done: i64,
    pub total: i64,
    pub message: String,
}

impl SyncEvent {
    pub fn finished(&self) -> bool {
        self.state == "done" || self.state == "error"
    }
}

#[derive(Debug)]
pub enum Event {
    Download(Download),
    Resync,
    Sync(SyncEvent),
}

#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: u16,
    pub message: String,
}

impl ApiError {
    pub fn offline(&self) -> bool {
        self.status == 0
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

pub struct ImportResult {
    pub playlist_id: i64,
    pub queued: i64,
    pub skipped: i64,
    pub already_in_playlist: i64,
}

#[derive(Clone)]
pub struct Api {
    pub base: String,
    http: reqwest::Client,
}

impl Api {
    pub fn new(base: String) -> Api {
        let http =
            reqwest::Client::builder().pool_max_idle_per_host(2).pool_idle_timeout(Duration::from_secs(20)).build().expect("http client");
        Api { base, http }
    }

    pub fn cover_url(&self, id: &str) -> String {
        format!("{}/api/tracks/{}/cover", self.base, id)
    }

    pub fn stream_url(&self, id: &str) -> String {
        format!("{}/api/tracks/{}/stream", self.base, id)
    }

    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<Value>,
        json_ct: bool,
    ) -> ApiResult<Value> {
        let q: Vec<(&str, &String)> = query.iter().filter(|(_, v)| !v.is_empty()).map(|(k, v)| (*k, v)).collect();
        let mut req = self.http.request(method, format!("{}{}", self.base, path)).query(&q).timeout(Duration::from_secs(180));
        if let Some(b) = body {
            req = req.json(&b);
        } else if json_ct {
            req = req.header("content-type", "application/json");
        }
        let res = req.send().await.map_err(|e| ApiError {
            status: 0,
            message: if e.is_timeout() {
                "The Music Player Lite server took too long to answer.".into()
            } else {
                format!("Can't reach the Music Player Lite server on {}. Is it running?", self.base)
            },
        })?;
        let status = res.status().as_u16();
        let bytes = res.bytes().await.map_err(|e| ApiError { status: 0, message: e.to_string() })?;
        let v: Value = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap_or(Value::Null) };
        if status >= 400 {
            let msg = v.get("error").and_then(|e| e.as_str()).map(String::from).unwrap_or_else(|| format!("Error {status}"));
            return Err(ApiError { status, message: msg });
        }
        Ok(v)
    }

    async fn get(&self, path: &str, query: &[(&str, String)]) -> ApiResult<Value> {
        self.send(reqwest::Method::GET, path, query, None, false).await
    }

    fn parse<T: for<'de> Deserialize<'de>>(v: Value) -> ApiResult<T> {
        serde_json::from_value(v).map_err(|e| ApiError { status: 500, message: format!("Bad answer from the backend: {e}") })
    }

    pub async fn health(&self) -> bool {
        match self.http.get(format!("{}/api/health", self.base)).timeout(Duration::from_millis(800)).send().await {
            Ok(r) => r.json::<Value>().await.map(|v| v.get("status").and_then(|s| s.as_str()) == Some("ok")).unwrap_or(false),
            Err(_) => false,
        }
    }

    pub async fn search(&self, q: &str) -> ApiResult<Vec<Track>> {
        Self::parse(self.get("/api/search", &[("q", q.into()), ("limit", "25".into())]).await?)
    }

    pub async fn tracks(&self, sort: &str) -> ApiResult<Vec<Track>> {
        Self::parse(self.get("/api/tracks", &[("sort", sort.into())]).await?)
    }

    pub async fn played(&self, id: &str) -> ApiResult<()> {
        self.send(reqwest::Method::POST, &format!("/api/tracks/{id}/played"), &[], None, false).await.map(|_| ())
    }

    pub async fn delete_track(&self, id: &str, delete_file: bool) -> ApiResult<()> {
        let q = [("delete_file", if delete_file { "true".to_string() } else { String::new() })];
        self.send(reqwest::Method::DELETE, &format!("/api/tracks/{id}"), &q, None, false).await.map(|_| ())
    }

    pub async fn artists(&self) -> ApiResult<Vec<Artist>> {
        Self::parse(self.get("/api/artists", &[]).await?)
    }

    pub async fn albums(&self) -> ApiResult<Vec<Album>> {
        Self::parse(self.get("/api/albums", &[]).await?)
    }

    pub async fn playlists(&self) -> ApiResult<Vec<Playlist>> {
        Self::parse(self.get("/api/playlists", &[]).await?)
    }

    pub async fn playlist(&self, id: i64) -> ApiResult<Playlist> {
        Self::parse(self.get(&format!("/api/playlists/{id}"), &[]).await?)
    }

    pub async fn create_playlist(&self, name: &str) -> ApiResult<Playlist> {
        Self::parse(self.send(reqwest::Method::POST, "/api/playlists", &[], Some(json!({ "name": name })), false).await?)
    }

    pub async fn rename_playlist(&self, id: i64, name: &str) -> ApiResult<()> {
        self.send(reqwest::Method::PATCH, &format!("/api/playlists/{id}"), &[], Some(json!({ "name": name })), false).await.map(|_| ())
    }

    pub async fn delete_playlist(&self, id: i64) -> ApiResult<()> {
        self.send(reqwest::Method::DELETE, &format!("/api/playlists/{id}"), &[], None, false).await.map(|_| ())
    }

    pub async fn add_to_playlist(&self, id: i64, video_id: &str) -> ApiResult<()> {
        self.send(reqwest::Method::POST, &format!("/api/playlists/{id}/tracks"), &[], Some(json!({ "track_id": video_id })), false)
            .await
            .map(|_| ())
    }

    pub async fn remove_from_playlist(&self, id: i64, video_id: &str) -> ApiResult<()> {
        self.send(reqwest::Method::DELETE, &format!("/api/playlists/{id}/tracks/{video_id}"), &[], None, false).await.map(|_| ())
    }

    pub async fn reorder_playlist(&self, id: i64, ids: &[String]) -> ApiResult<()> {
        self.send(reqwest::Method::PUT, &format!("/api/playlists/{id}/order"), &[], Some(json!({ "track_ids": ids })), false)
            .await
            .map(|_| ())
    }

    pub async fn downloads(&self) -> ApiResult<Vec<Download>> {
        Self::parse(self.get("/api/downloads", &[]).await?)
    }

    pub async fn download_track(&self, video_id: &str) -> ApiResult<Download> {
        Self::parse(self.send(reqwest::Method::POST, "/api/downloads", &[], Some(json!({ "video_id": video_id })), false).await?)
    }

    pub async fn download_playlist(&self, url: &str, name: Option<&str>, playlist_id: Option<i64>) -> ApiResult<ImportResult> {
        let mut body = json!({ "playlist_url": url });
        if let Some(id) = playlist_id {
            body["playlist_id"] = json!(id);
        } else if let Some(n) = name.map(str::trim).filter(|n| !n.is_empty()) {
            body["name"] = json!(n);
        }
        let j = self.send(reqwest::Method::POST, "/api/downloads", &[], Some(body), false).await?;
        let int = |k: &str| j.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0) as i64;
        Ok(ImportResult {
            playlist_id: int("playlist_id"),
            queued: int("queued"),
            skipped: int("skipped"),
            already_in_playlist: int("already_in_playlist"),
        })
    }

    pub async fn delete_download(&self, id: &str) -> ApiResult<()> {
        self.send(reqwest::Method::DELETE, &format!("/api/downloads/{id}"), &[], None, false).await.map(|_| ())
    }

    pub async fn clear_downloads(&self, status: Option<&str>, ids: Option<Vec<String>>) -> ApiResult<i64> {
        let mut body = json!({});
        if let Some(s) = status {
            body["status"] = json!(s);
        }
        if let Some(ids) = ids {
            body["ids"] = json!(ids);
        }
        let j = self.send(reqwest::Method::POST, "/api/downloads/clear", &[], Some(body), false).await?;
        Ok(j.get("removed").and_then(|v| v.as_f64()).unwrap_or(0.0) as i64)
    }

    pub async fn retry_download(&self, id: &str) -> ApiResult<Download> {
        Self::parse(self.send(reqwest::Method::POST, &format!("/api/downloads/{id}/retry"), &[], None, false).await?)
    }

    pub async fn lyrics(&self, id: &str) -> ApiResult<Lyrics> {
        Ok(Lyrics::from_json(&self.get(&format!("/api/tracks/{id}/lyrics"), &[]).await?))
    }

    /// The song a YouTube link points at (song links, `youtu.be`, shorts, …).
    pub async fn song_from_link(&self, url: &str) -> ApiResult<Track> {
        let v = self.get("/api/resolve", &[("url", url.into()), ("as", "song".into())]).await?;
        match v.get("kind").and_then(Value::as_str) {
            Some("song") => Self::parse(v.get("track").cloned().unwrap_or(Value::Null)),
            _ => Err(ApiError { status: 400, message: "That link isn’t a song.".into() }),
        }
    }

    pub async fn playlist_info(&self, url: &str) -> ApiResult<PlaylistPreview> {
        Self::parse(self.get("/api/playlist-info", &[("url", url.into())]).await?)
    }

    pub async fn recommendations(&self) -> ApiResult<Vec<Track>> {
        Self::parse(self.get("/api/recommendations", &[("limit", "40".into())]).await?)
    }

    pub async fn refresh_recommendations(&self) -> ApiResult<Vec<Track>> {
        Self::parse(self.send(reqwest::Method::POST, "/api/recommendations/refresh", &[], None, false).await?)
    }

    pub async fn sync_devices(&self) -> ApiResult<SyncStatus> {
        Self::parse(self.get("/api/sync/devices", &[]).await?)
    }

    pub async fn new_sync_code(&self) -> ApiResult<ThisDevice> {
        Self::parse(self.send(reqwest::Method::POST, "/api/sync/code", &[], None, true).await?)
    }

    pub async fn pair_device(&self, code: &str) -> ApiResult<SyncDevice> {
        Self::parse(self.send(reqwest::Method::POST, "/api/sync/pair", &[], Some(json!({ "code": code })), false).await?)
    }

    pub async fn remove_device(&self, id: &str) -> ApiResult<()> {
        let enc: String = id
            .bytes()
            .map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
            .collect();
        self.send(reqwest::Method::DELETE, &format!("/api/sync/devices/{enc}"), &[], None, true).await.map(|_| ())
    }

    pub async fn run_sync(&self) -> ApiResult<()> {
        self.send(reqwest::Method::POST, "/api/sync/run", &[], Some(json!({})), false).await.map(|_| ())
    }

    pub async fn private_listening(&self) -> ApiResult<bool> {
        let j = self.get("/api/settings/private-listening", &[]).await?;
        Ok(j.get("enabled").and_then(|v| v.as_bool()) == Some(true))
    }

    pub async fn set_private_listening(&self, enabled: bool) -> ApiResult<bool> {
        let j = self.send(reqwest::Method::PUT, "/api/settings/private-listening", &[], Some(json!({ "enabled": enabled })), false).await?;
        Ok(j.get("enabled").and_then(|v| v.as_bool()) == Some(true))
    }

    /// Server-side settings (`GET /api/settings`).
    pub async fn server_settings(&self) -> ApiResult<Value> {
        self.get("/api/settings", &[]).await
    }

    /// Change some server-side settings; returns all of them.
    pub async fn patch_settings(&self, changes: Value) -> ApiResult<Value> {
        self.send(reqwest::Method::PATCH, "/api/settings", &[], Some(changes), false).await
    }

    pub async fn cover_bytes(&self, id: &str) -> Option<Vec<u8>> {
        let r = self.http.get(self.cover_url(id)).timeout(Duration::from_secs(30)).send().await.ok()?;
        if !r.status().is_success() {
            return None;
        }
        r.bytes().await.ok().map(|b| b.to_vec()).filter(|b| !b.is_empty())
    }

    /// Server-Sent Events of download changes; calls `on` per event until the
    /// connection drops.
    pub async fn events(&self, mut on: impl FnMut(Event)) -> ApiResult<()> {
        let res = self
            .http
            .get(format!("{}/api/downloads/events", self.base))
            .header("accept", "text/event-stream")
            .send()
            .await
            .map_err(|e| ApiError { status: 0, message: e.to_string() })?;
        if !res.status().is_success() {
            return Err(ApiError { status: res.status().as_u16(), message: "event stream failed".into() });
        }
        let mut stream = res.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut event = String::new();
        let mut data = String::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| ApiError { status: 0, message: e.to_string() })?;
            buf.extend_from_slice(&chunk);
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&line);
                let line = line.trim_end_matches(['\n', '\r']);
                if line.is_empty() {
                    match event.as_str() {
                        "download" if !data.is_empty() => {
                            if let Ok(d) = serde_json::from_str::<Download>(&data) {
                                on(Event::Download(d));
                            }
                        }
                        "resync" => on(Event::Resync),
                        "sync" if !data.is_empty() => {
                            if let Ok(s) = serde_json::from_str::<SyncEvent>(&data) {
                                on(Event::Sync(s));
                            }
                        }
                        _ => {}
                    }
                    event.clear();
                    data.clear();
                } else if let Some(e) = line.strip_prefix("event:") {
                    event = e.trim().to_string();
                } else if let Some(d) = line.strip_prefix("data:") {
                    data.push_str(d.trim_start());
                }
            }
        }
        Ok(())
    }
}

/// A public Spotify / Apple Music / Deezer / SoundCloud link: `Some(true)` for a
/// playlist or album (short links too, the server follows them), `Some(false)`
/// for one song. The server (geonix_yt::providers) does the exact reading.
fn external_link(text: &str) -> Option<bool> {
    let t = text.trim();
    if t.is_empty() || t.contains(char::is_whitespace) {
        return None;
    }
    if let Some(rest) = t.strip_prefix("spotify:") {
        return match rest.split(':').next()? {
            "playlist" | "album" => Some(true),
            "track" => Some(false),
            _ => None,
        };
    }
    let url = reqwest::Url::parse(&if t.contains("://") { t.to_string() } else { format!("https://{t}") }).ok()?;
    let host = url.host_str()?.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    let segs: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    let has_song_param = url.query_pairs().any(|(k, _)| k == "i");
    match host {
        "spotify.link" | "spotify.app.link" | "deezer.page.link" | "link.deezer.com" | "on.soundcloud.com" | "soundcloud.app.goo.gl" => {
            Some(true)
        }
        "open.spotify.com" => match segs.iter().find(|s| !s.starts_with("intl-") && **s != "embed")? {
            &"playlist" | &"album" => Some(true),
            &"track" => Some(false),
            _ => None,
        },
        "music.apple.com" => match segs.get(1)? {
            &"playlist" => Some(true),
            &"album" => Some(!has_song_param),
            &"song" => Some(false),
            _ => None,
        },
        "deezer.com" => match segs.iter().find(|s| s.len() > 2)? {
            &"playlist" | &"album" => Some(true),
            &"track" => Some(false),
            _ => None,
        },
        "soundcloud.com" | "m.soundcloud.com" => match segs.as_slice() {
            [_, "sets", _, ..] => Some(true),
            [user, track]
                if !["discover", "stream", "charts", "search", "you", "upload", "feed", "pages", "settings"].contains(user)
                    && !["sets", "albums", "tracks", "likes", "reposts", "followers", "following", "popular-tracks", "comments"]
                        .contains(track) =>
            {
                Some(false)
            }
            _ => None,
        },
        _ => None,
    }
}

/// A YouTube / YouTube Music link (with or without `https://`), not a search.
/// Spotify, Apple Music, Deezer and SoundCloud song links count too.
pub fn looks_like_youtube_link(text: &str) -> bool {
    let t = text.trim().to_ascii_lowercase();
    (!t.contains(char::is_whitespace) && (t.contains("youtube.com/") || t.contains("youtu.be/"))) || external_link(text) == Some(false)
}

/// A playlist or album link: `list=` (except endless auto-generated mixes,
/// `list=RD…`, which count as the song) or a YouTube Music album page.
pub fn looks_like_playlist_url(text: &str) -> bool {
    let t = text.trim();
    let lower = t.to_ascii_lowercase();
    let is_link = t.starts_with("http://") || t.starts_with("https://") || looks_like_youtube_link(t);
    if external_link(t) == Some(true) {
        return true;
    }
    let list = t.split(['?', '&', '#']).find_map(|p| p.strip_prefix("list=")).filter(|l| !l.is_empty());
    is_link && (list.is_some_and(|l| !l.starts_with("RD")) || lower.contains("/browse/mpre") || lower.contains("/browse/vl"))
}

/// Turns backend errors into something a person can act on (playlist_import.dart).
pub fn friendly_import_error(e: &ApiError) -> String {
    let msg = e.message.clone();
    let lower = msg.to_lowercase();
    if e.offline() {
        return "Can’t reach the Music Player Lite server. Is it running?".into();
    }
    if ["private", "does not exist", "not found", "unavailable", "http error 4", "unable to download"].iter().any(|k| lower.contains(k)) {
        return "This playlist is private, deleted or doesn’t exist. Make sure it’s public or unlisted.".into();
    }
    if lower.contains("matched on youtube music") {
        return "None of those songs could be found on YouTube Music.".into();
    }
    if lower.contains("no available songs") {
        return "That playlist has no songs that can be downloaded.".into();
    }
    if lower.contains("list=") {
        return "That doesn’t look like a playlist link. It should contain “list=”.".into();
    }
    format!("Couldn’t load that playlist: {msg}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lyrics_line_at() {
        let l = Lyrics::from_json(
            &json!({"synced": true, "lines": [{"time_ms": 0, "text": "a"}, {"time_ms": 1000, "text": "b"}, {"time_ms": 3000, "text": "c"}]}),
        );
        assert!(l.synced);
        assert_eq!(l.line_at(0), 0);
        assert_eq!(l.line_at(800), 1);
        assert_eq!(l.line_at(2000), 1);
        assert_eq!(l.line_at(9000), 2);
    }

    #[test]
    fn track_nulls() {
        let t: Track = serde_json::from_value(json!({"video_id": "x", "title": null, "artist": "A", "downloaded": true})).unwrap();
        assert_eq!(t.title, "");
        assert!(t.downloaded);
    }

    #[test]
    fn playlist_url() {
        for ok in [
            "https://music.youtube.com/playlist?list=PL1",
            "https://www.youtube.com/playlist?list=PLAp_cIInOnUPbid-apz4wZmlvg6G42HSz",
            "https://m.youtube.com/playlist?list=PL1&feature=share",
            "music.youtube.com/playlist?list=PL1&si=abc",
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=PL1",
            "https://music.youtube.com/browse/MPREb_4pL8gzRtw1p",
            "https://music.youtube.com/browse/VLPL1",
        ] {
            assert!(looks_like_playlist_url(ok), "{ok}");
        }
        for not in [
            "lofi beats",
            "https://youtu.be/dQw4w9WgXcQ?si=x",
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=RDdQw4w9WgXcQ&start_radio=1",
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ&playlist=1",
        ] {
            assert!(!looks_like_playlist_url(not), "{not}");
        }
    }

    #[test]
    fn links_of_other_services() {
        for ok in [
            "https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M?si=abc",
            "https://open.spotify.com/intl-de/album/4aawyAB9vmqN3uQ7FjRGTy",
            "spotify:playlist:37i9dQZF1DXcBWIGoYBM5M",
            "https://music.apple.com/us/playlist/top-100-global/pl.d25f5d1181894928af76c85c967f8f31",
            "https://music.apple.com/us/album/discovery/697194953",
            "https://www.deezer.com/playlist/3155776842",
            "https://www.deezer.com/en/album/302127",
            "https://soundcloud.com/forss/sets/soulhack",
            "https://on.soundcloud.com/AbCdE",
            "https://spotify.link/AbCdEf",
        ] {
            assert!(looks_like_playlist_url(ok), "{ok}");
            assert!(!looks_like_youtube_link(ok), "{ok} is not a song link");
        }
        for song in [
            "https://open.spotify.com/track/2sFepQNyl1WjpROruP8bJ0?si=x",
            "spotify:track:2sFepQNyl1WjpROruP8bJ0",
            "https://music.apple.com/us/album/discovery/697194953?i=697195462",
            "https://music.apple.com/us/song/harder-better-faster-stronger/697195787",
            "https://www.deezer.com/en/track/3135556",
            "https://soundcloud.com/forss/flickermood",
        ] {
            assert!(looks_like_youtube_link(song), "{song}");
            assert!(!looks_like_playlist_url(song), "{song} is not a playlist link");
        }
        for not in [
            "https://open.spotify.com/artist/45dkTj5sMRSjrmBSBeiHym",
            "https://soundcloud.com/forss",
            "https://www.deezer.com/en/artist/27",
            "daft punk",
        ] {
            assert!(!looks_like_playlist_url(not) && !looks_like_youtube_link(not), "{not}");
        }
    }

    #[test]
    fn youtube_links() {
        for ok in [
            "https://youtu.be/dQw4w9WgXcQ?si=x",
            "youtu.be/dQw4w9WgXcQ",
            "https://www.youtube.com/shorts/dQw4w9WgXcQ",
            "https://music.youtube.com/watch?v=dQw4w9WgXcQ&feature=share",
            "https://m.youtube.com/watch?v=dQw4w9WgXcQ",
        ] {
            assert!(looks_like_youtube_link(ok), "{ok}");
        }
        for not in ["daft punk", "youtube.com/ songs", "https://example.com/watch?v=x"] {
            assert!(!looks_like_youtube_link(not), "{not}");
        }
    }

    /// Tiny HTTP server answering canned responses (one per connection).
    async fn mock() -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = l.accept().await else { return };
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = s.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let (status, ctype, body) = if path.starts_with("/api/tracks?sort=plays") {
                        (
                            "200 OK",
                            "application/json",
                            r#"[{"video_id":"abc","title":"Song","artist":"A","duration_sec":185,"downloaded":true,"play_count":3}]"#
                                .to_string(),
                        )
                    } else if path.starts_with("/api/downloads/events") {
                        ("200 OK", "text/event-stream", "event: download\ndata: {\"id\":\"d1\",\"video_id\":\"abc\",\"status\":\"downloading\",\"progress\":42.5}\n\nevent: resync\ndata: {}\n\nevent: sync\ndata: {\"state\":\"done\",\"device\":\"pc\",\"done\":2}\n\n".to_string())
                    } else if path.starts_with("/api/playlists/9") {
                        ("404 Not Found", "application/json", r#"{"error":"playlist not found"}"#.to_string())
                    } else {
                        ("200 OK", "application/json", r#"{"status":"ok"}"#.to_string())
                    };
                    let resp = format!(
                        "HTTP/1.1 {status}\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = s.write_all(resp.as_bytes()).await;
                });
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn client_against_mock_server() {
        let api = Api::new(mock().await);
        assert!(api.health().await);
        let t = api.tracks("plays").await.unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].video_id, "abc");
        assert_eq!(t[0].duration_sec, 185);
        let e = api.playlist(9).await.unwrap_err();
        assert_eq!(e.status, 404);
        assert_eq!(e.message, "playlist not found");
        let mut events = Vec::new();
        api.events(|e| events.push(e)).await.unwrap();
        assert_eq!(events.len(), 3);
        match &events[0] {
            Event::Download(d) => {
                assert_eq!(d.status, "downloading");
                assert!((d.progress - 42.5).abs() < 1e-9);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(events[1], Event::Resync));
        assert!(matches!(&events[2], Event::Sync(s) if s.done == 2 && s.finished()));
    }

    #[tokio::test]
    async fn offline_error_is_friendly() {
        let api = Api::new("http://127.0.0.1:9".into());
        let e = api.tracks("added").await.unwrap_err();
        assert!(e.offline());
        assert!(friendly_import_error(&e).contains("Can’t reach"));
    }
}
