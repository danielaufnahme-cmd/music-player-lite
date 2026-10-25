//! YouTube Music lookups (port of worker/ytm.py): search, playlist contents,
//! related songs, song info and stream URLs.
//!
//! Every function returns tracks in the shared track shape:
//! `{"video_id", "title", "artist", "album", "duration_sec", "thumbnail_url",
//!   "file_path": null, "downloaded": false}`

use std::collections::HashSet;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use regex::Regex;
use serde_json::{json, Value};

use crate::innertube;
use crate::net::{Error, Result, Transport};
use crate::tools;
use crate::util::{first_truthy, get, obj, truthy};

/// Song-like video types. User uploads (UGC) and podcast episodes are not music.
pub const MUSIC_VIDEO_TYPES: [&str; 3] = ["MUSIC_VIDEO_TYPE_ATV", "MUSIC_VIDEO_TYPE_OMV", "MUSIC_VIDEO_TYPE_OFFICIAL_SOURCE_MUSIC"];

/// "3:25" / "1:02:03" / 205 -> seconds. Unknown -> 0.
pub fn parse_duration(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Number(n)) => {
            if let Some(i) = n.as_i64() {
                i.max(0)
            } else {
                (n.as_f64().unwrap_or(0.0).trunc() as i64).max(0)
            }
        }
        Some(Value::Bool(b)) => *b as i64,
        Some(Value::String(s)) if !s.trim().is_empty() => {
            let mut total = 0i64;
            for part in s.trim().split(':') {
                match part.trim().parse::<i64>() {
                    Ok(p) => total = total * 60 + p,
                    Err(_) => return 0,
                }
            }
            total
        }
        _ => 0,
    }
}

/// Largest thumbnail URL, upscaled to 544px for googleusercontent images.
pub fn best_thumbnail(thumbnails: Option<&Value>) -> String {
    static SIZE: OnceLock<Regex> = OnceLock::new();
    let Some(Value::Array(items)) = thumbnails else {
        return String::new();
    };
    let dim = |t: &Value, k: &str| t.get(k).and_then(Value::as_f64).unwrap_or(0.0);
    let mut best: Option<(&Value, f64)> = None;
    for t in items {
        if !t.is_object() || !t.get("url").is_some_and(truthy) {
            continue;
        }
        let area = dim(t, "width") * dim(t, "height");
        // Python's max() keeps the first of equal elements.
        if best.is_none_or(|(_, a)| area > a) {
            best = Some((t, area));
        }
    }
    let Some((best, _)) = best else {
        return String::new();
    };
    let url = best.get("url").and_then(Value::as_str).unwrap_or("").to_string();
    let size = SIZE.get_or_init(|| Regex::new(r"=w\d+-h\d+").unwrap());
    if url.contains("googleusercontent.com") && size.is_match(&url) {
        return size.replace_all(&url, "=w544-h544").into_owned();
    }
    url
}

pub fn join_artists(artists: Option<&Value>) -> String {
    match artists {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|a| a.get("name").filter(|n| truthy(n)))
            .map(|n| n.as_str().map(str::to_string).unwrap_or_else(|| crate::util::py_str(n)))
            .collect::<Vec<_>>()
            .join(", "),
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

/// Convert any ytmusicapi track-ish dict into the shared track shape.
pub fn normalize_track(item: &Value) -> Option<Value> {
    let video_id = get(item, "videoId").filter(|v| truthy(v))?;
    let title = get(item, "title").filter(|v| truthy(v))?;
    let album_name = match get(item, "album") {
        Some(Value::Object(a)) => a.get("name").cloned().unwrap_or(json!("")),
        Some(other) if truthy(other) => other.clone(),
        _ => json!(""),
    };
    let album_name = if truthy(&album_name) { album_name } else { json!("") };
    let duration = first_truthy(item, &["duration_seconds", "duration", "length"]);
    let artist = {
        let joined = join_artists(get(item, "artists"));
        if !joined.is_empty() {
            json!(joined)
        } else if let Some(author) = get(item, "author").filter(|a| truthy(a)) {
            author.clone()
        } else {
            json!("Unknown artist")
        }
    };
    let thumbs = first_truthy(item, &["thumbnails", "thumbnail"]);
    Some(obj(vec![
        ("video_id", video_id.clone()),
        ("title", title.clone()),
        ("artist", artist),
        ("album", album_name),
        ("duration_sec", json!(parse_duration(duration))),
        ("thumbnail_url", json!(best_thumbnail(thumbs))),
        ("file_path", Value::Null),
        ("downloaded", json!(false)),
    ]))
}

pub fn is_music(item: &Value) -> bool {
    match get(item, "videoType") {
        None | Some(Value::Null) => true,
        Some(v) => v.as_str().is_some_and(|t| MUSIC_VIDEO_TYPES.contains(&t)),
    }
}

fn vid(track: &Value) -> String {
    track.get("video_id").map(crate::util::py_str).unwrap_or_default()
}

/// Songs only — videos, podcasts and episodes are filtered out.
pub async fn search(t: &dyn Transport, query: &str, limit: usize) -> Result<Vec<Value>> {
    let results = innertube::search_songs(t, query, limit).await?;
    let mut tracks = Vec::new();
    let mut seen = HashSet::new();
    for item in &results {
        let rt = get(item, "resultType");
        let song_or_none = matches!(rt, None | Some(Value::Null)) || rt.and_then(Value::as_str) == Some("song");
        if !song_or_none || !is_music(item) {
            continue;
        }
        if let Some(track) = normalize_track(item) {
            if seen.insert(vid(&track)) {
                tracks.push(track);
            }
        }
    }
    tracks.truncate(limit);
    Ok(tracks)
}

/// Pull the `list=` id out of a YouTube / YouTube Music URL.
pub fn extract_playlist_id(url: &str) -> Result<String> {
    let url = url.trim();
    let no_frag = url.split('#').next().unwrap_or("");
    let query = no_frag.split_once('?').map(|(_, q)| q).unwrap_or("");
    let id = url::form_urlencoded::parse(query.as_bytes()).find(|(k, v)| k == "list" && !v.is_empty()).map(|(_, v)| v.into_owned());
    let Some(id) = id else {
        return Err("That link has no playlist id (list=...)".into());
    };
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return Err("Invalid playlist id in link".into());
    }
    Ok(id)
}

/// `urlparse(url).path`.
fn url_path(url: &str) -> &str {
    let mut rest = url;
    if let Some(i) = rest.find(':') {
        let scheme = &rest[..i];
        if !scheme.is_empty()
            && scheme.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
            && scheme.chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
        {
            rest = &rest[i + 1..];
        }
    }
    if let Some(after) = rest.strip_prefix("//") {
        rest = after.find(['/', '?', '#']).map_or("", |i| &after[i..]);
    }
    let end = rest.find(['?', '#']).unwrap_or(rest.len());
    &rest[..end]
}

/// The album id from a music.youtube.com/browse/MPREb… link, if it is one.
pub fn extract_album_browse_id(url: &str) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"/browse/(MPRE[A-Za-z0-9_-]+)").unwrap());
    re.captures(url_path(url.trim())).map(|c| c[1].to_string())
}

/// Title and songs of an album page; album tracks inherit its name and cover.
pub async fn album_info(t: &dyn Transport, browse_id: &str) -> Result<Value> {
    let data = innertube::get_album(t, browse_id).await?;
    let title = if truthy(&data.title) { data.title.clone() } else { json!("Imported album") };
    let cover = best_thumbnail(Some(&data.thumbnails));
    let mut tracks = Vec::new();
    let mut seen = HashSet::new();
    for item in &data.tracks {
        if get(item, "isAvailable") == Some(&Value::Bool(false)) {
            continue;
        }
        let Some(mut track) = normalize_track(item) else { continue };
        if !seen.insert(vid(&track)) {
            continue;
        }
        let m = track.as_object_mut().expect("object");
        if !m.get("album").is_some_and(truthy) {
            m.insert("album".into(), title.clone());
        }
        if !m.get("thumbnail_url").is_some_and(truthy) {
            m.insert("thumbnail_url".into(), json!(cover));
        }
        if m.get("artist").and_then(Value::as_str) == Some("Unknown artist") {
            let joined = join_artists(Some(&data.artists));
            if !joined.is_empty() {
                m.insert("artist".into(), json!(joined));
            }
        }
        tracks.push(track);
    }
    Ok(json!({"title": title, "tracks": tracks}))
}

/// Metadata of one song (title, artist, album, duration, cover).
pub async fn song_info(t: &dyn Transport, ytdlp: Option<&Path>, video_id: &str) -> Result<Value> {
    if is_soundcloud_id(video_id) {
        let info = ytdlp_json(
            require_ytdlp(ytdlp)?,
            &["-J".into(), "--no-warnings".into(), "--no-playlist".into(), watch_url(video_id)],
            YTDLP_SONG_TIMEOUT,
        )
        .await?;
        return crate::providers::soundcloud_track(&info).ok_or_else(|| "Couldn't find that song".into());
    }
    match innertube::get_watch_playlist(t, video_id, 1, false).await {
        Ok(data) => {
            for item in &data.tracks {
                if get(item, "videoId").and_then(Value::as_str) == Some(video_id) {
                    if let Some(track) = normalize_track(item) {
                        return Ok(track);
                    }
                }
            }
        }
        Err(e) => eprintln!("geonix-yt: YouTube Music could not read song {video_id} ({e}); trying yt-dlp"),
    }
    let ytdlp = require_ytdlp(ytdlp)?;
    let info = ytdlp_json(
        ytdlp,
        &[
            "-O".into(),
            "%(.{track,title,artist,artists,channel,album,duration,thumbnail})j".into(),
            "--no-warnings".into(),
            "--no-playlist".into(),
            format!("https://music.youtube.com/watch?v={video_id}"),
        ],
        YTDLP_SONG_TIMEOUT,
    )
    .await?;
    let Some(title) = first_truthy(&info, &["track", "title"]) else {
        return Err("Couldn't find that song".into());
    };
    let artists_joined = match get(&info, "artists") {
        Some(Value::Array(a)) => a.iter().map(crate::util::py_str).collect::<Vec<_>>().join(", "),
        _ => String::new(),
    };
    let artist = first_truthy(&info, &["artist"])
        .cloned()
        .or_else(|| (!artists_joined.is_empty()).then(|| json!(artists_joined)))
        .or_else(|| first_truthy(&info, &["channel"]).cloned())
        .unwrap_or(json!("Unknown artist"));
    Ok(obj(vec![
        ("video_id", json!(video_id)),
        ("title", title.clone()),
        ("artist", artist),
        ("album", first_truthy(&info, &["album"]).cloned().unwrap_or(json!(""))),
        ("duration_sec", json!(get(&info, "duration").and_then(Value::as_f64).unwrap_or(0.0) as i64)),
        ("thumbnail_url", first_truthy(&info, &["thumbnail"]).cloned().unwrap_or(json!(""))),
        ("file_path", Value::Null),
        ("downloaded", json!(false)),
    ]))
}

/// Title and songs (in order) of a playlist or an album page.
pub async fn playlist_info(t: &dyn Transport, ytdlp: Option<&Path>, url: &str) -> Result<Value> {
    if let Some(album) = extract_album_browse_id(url) {
        return album_info(t, &album).await;
    }
    let playlist_id = extract_playlist_id(url)?;
    // YouTube Music reads most public playlists, also regular YouTube ones.
    // yt-dlp covers the rest: errors and playlists YouTube Music shows empty.
    let (title, items) = match innertube::get_playlist(t, &playlist_id).await {
        Ok(p) if !dedupe_tracks(&p.tracks).is_empty() => {
            let title = if p.title.is_empty() { "Imported playlist".to_string() } else { p.title };
            (json!(title), p.tracks)
        }
        Ok(_) => {
            eprintln!("geonix-yt: YouTube Music shows playlist {playlist_id} without songs; trying yt-dlp");
            playlist_via_ytdlp(require_ytdlp(ytdlp)?, &playlist_id).await?
        }
        Err(e) => {
            eprintln!("geonix-yt: YouTube Music could not read playlist {playlist_id} ({e}); trying yt-dlp");
            playlist_via_ytdlp(require_ytdlp(ytdlp)?, &playlist_id).await?
        }
    };
    Ok(json!({"title": title, "tracks": dedupe_tracks(&items)}))
}

/// Normalise playlist items, skipping unavailable ones and duplicates.
pub fn dedupe_tracks(items: &[Value]) -> Vec<Value> {
    let mut tracks = Vec::new();
    let mut seen = HashSet::new();
    for item in items {
        if get(item, "isAvailable") == Some(&Value::Bool(false)) {
            continue;
        }
        if let Some(track) = normalize_track(item) {
            if seen.insert(vid(&track)) {
                tracks.push(track);
            }
        }
    }
    tracks
}

async fn playlist_via_ytdlp(ytdlp: &Path, playlist_id: &str) -> Result<(Value, Vec<Value>)> {
    let info = ytdlp_json(
        ytdlp,
        &["-J".into(), "--flat-playlist".into(), "--no-warnings".into(), format!("https://www.youtube.com/playlist?list={playlist_id}")],
        YTDLP_PLAYLIST_TIMEOUT,
    )
    .await?;
    Ok(playlist_from_ytdlp_info(&info))
}

/// Turn yt-dlp's flat playlist JSON into ytmusicapi-like items.
pub fn playlist_from_ytdlp_info(info: &Value) -> (Value, Vec<Value>) {
    let mut items = Vec::new();
    if let Some(Value::Array(entries)) = get(info, "entries") {
        for entry in entries {
            let Some(id) = get(entry, "id").filter(|v| truthy(v)) else { continue };
            let channel = first_truthy(entry, &["channel", "uploader"]).cloned().unwrap_or(json!(""));
            items.push(obj(vec![
                ("videoId", id.clone()),
                ("title", get(entry, "title").cloned().unwrap_or(Value::Null)),
                ("artists", json!([{"name": channel}])),
                ("duration_seconds", get(entry, "duration").cloned().unwrap_or(Value::Null)),
                ("thumbnails", first_truthy(entry, &["thumbnails"]).cloned().unwrap_or(json!([]))),
            ]));
        }
    }
    let title = first_truthy(info, &["title"]).cloned().unwrap_or(json!("Imported playlist"));
    (title, items)
}

/// (seed track, songs YouTube Music plays after it) via the radio/watch playlist.
pub async fn related(t: &dyn Transport, video_id: &str, limit: usize) -> Result<(Option<Value>, Vec<Value>)> {
    let data = innertube::get_watch_playlist(t, video_id, limit, true).await?;
    let mut seed = None;
    let mut out = Vec::new();
    for item in &data.tracks {
        let Some(track) = normalize_track(item) else { continue };
        if track.get("video_id").and_then(Value::as_str) == Some(video_id) {
            seed = Some(track);
            continue;
        }
        if is_music(item) {
            out.push(track);
        }
    }
    Ok((seed, out))
}

/// Songs from SoundCloud have ids like `sc_293` (the SoundCloud track id) and
/// are fetched from SoundCloud itself; everything else is a YouTube video id.
pub fn is_soundcloud_id(video_id: &str) -> bool {
    video_id.strip_prefix("sc_").is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// The page yt-dlp reads a song from.
pub fn watch_url(video_id: &str) -> String {
    match video_id.strip_prefix("sc_").filter(|_| is_soundcloud_id(video_id)) {
        Some(n) => format!("https://api.soundcloud.com/tracks/{n}"),
        None => format!("https://music.youtube.com/watch?v={video_id}"),
    }
}

/// Direct audio URL (plus required headers) for previewing a song.
/// m4a/AAC is preferred because the Rust decoder handles AAC (not opus).
pub async fn stream_url(ytdlp: Option<&Path>, video_id: &str) -> Result<Value> {
    let ytdlp = require_ytdlp(ytdlp)?;
    let info = ytdlp_json(
        ytdlp,
        &[
            "-O".into(),
            "%(.{url,http_headers})j".into(),
            "-f".into(),
            "bestaudio[ext=m4a]/bestaudio/best".into(),
            "--no-warnings".into(),
            "--no-playlist".into(),
            watch_url(video_id),
        ],
        YTDLP_SONG_TIMEOUT,
    )
    .await?;
    let Some(url) = first_truthy(&info, &["url"]) else {
        return Err("No playable audio stream found".into());
    };
    let headers = first_truthy(&info, &["http_headers"]).cloned().unwrap_or(json!({}));
    Ok(json!({"url": url, "http_headers": headers}))
}

pub fn require_ytdlp(ytdlp: Option<&Path>) -> Result<&Path> {
    ytdlp.ok_or_else(|| Error::Msg("yt-dlp was not found (install it or set GEONIX_YTDLP)".into()))
}

/// yt-dlp time limit for one song's metadata or stream URL.
const YTDLP_SONG_TIMEOUT: Duration = Duration::from_secs(40);
/// yt-dlp time limit for a whole (flat) playlist.
const YTDLP_PLAYLIST_TIMEOUT: Duration = Duration::from_secs(90);

/// Run yt-dlp (with the JS-challenge options) and parse the (last) JSON object it printed.
pub(crate) async fn ytdlp_json(ytdlp: &Path, args: &[String], timeout: Duration) -> Result<Value> {
    let mut all = tools::ytdlp_js_args(ytdlp).await;
    all.extend_from_slice(args);
    let out = tools::run(ytdlp, &all, timeout).await?;
    if !out.success {
        return Err(Error::Msg(tools::ytdlp_error(&out.stderr).unwrap_or_else(|| "yt-dlp failed".into())));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().rev().find(|l| l.trim_start().starts_with('{')).unwrap_or("");
    let value: Value = serde_json::from_str(line.trim()).unwrap_or(Value::Null);
    Ok(value)
}
