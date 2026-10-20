//! The parts of ytmusicapi the worker used, ported to `serde_json::Value`:
//! search (songs filter), get_playlist, get_album, get_watch_playlist, get_lyrics.
//!
//! Parsers return "ytmusicapi-shaped" items (videoId, title, artists, album,
//! duration, duration_seconds, thumbnails, videoType, isAvailable, ...), which
//! `ytm::normalize_track` turns into the shared track shape, exactly like Python.

use std::sync::OnceLock;

use regex::Regex;
use serde_json::{json, Map, Value};

use crate::net::{Client, Error, Result, Transport};
use crate::util::{nav, nav_arr, nav_str};

const MRLIR: &str = "musicResponsiveListItemRenderer";
const MNIR: &str = "menuNavigationItemRenderer";
const TWO_COLUMN: &str = "contents.twoColumnBrowseResultsRenderer";
const THUMBNAILS: &str = "thumbnail.musicThumbnailRenderer.thumbnail.thumbnails";
const PLAY_BUTTON: &str = "overlay.musicItemThumbnailOverlayRenderer.content.musicPlayButtonRenderer";
const NAV_VIDEO_TYPE: &str = "watchEndpoint.watchEndpointMusicSupportedConfigs.watchEndpointMusicConfig.musicVideoType";

/// ytmusicapi's search params for filter="songs" (no scope, spelling not ignored).
pub const SONGS_FILTER_PARAMS: &str = "EgWKAQIIAWoMEA4QChADEAQQCRAF";

fn re(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("valid regex"))
}

fn missing(what: &str) -> Error {
    Error::Msg(format!("Unable to find '{what}' in the YouTube Music response"))
}

// ------------------------------------------------------------------ runs

#[derive(Debug, Clone, PartialEq)]
pub enum Run {
    Album(Value),
    Artist(Value),
    Duration(String),
    Year(String),
    Views(String),
}

/// ytmusicapi `parse_views`: a view count, or None when the text is something else.
pub fn parse_views(text: &str) -> Option<String> {
    static LATIN: OnceLock<Regex> = OnceLock::new();
    static PREFIX: OnceLock<Regex> = OnceLock::new();
    let mut text = text.to_string();
    let mut prefixed = false;
    if !re(&LATIN, "[a-zA-Z]").is_match(&text) {
        let prefix = re(&PREFIX, r"^\D*?[\s:\x{ff1a}\x{200e}-\x{200f}\x{202a}-\x{202e}]");
        if let Some(m) = prefix.find(&text) {
            text = text[m.end()..].to_string();
            prefixed = true;
        }
    }
    static DIGIT: OnceLock<Regex> = OnceLock::new();
    if !re(&DIGIT, r"^\d").is_match(&text) {
        return None;
    }
    if !prefixed && text.is_ascii() && !text.contains(' ') {
        return None;
    }
    let head: Vec<&str> = text.split(' ').next().unwrap_or("").split('\u{a0}').collect();
    Some(head.iter().take(2).copied().collect::<Vec<_>>().join("\u{a0}"))
}

pub fn parse_song_run(run: &Value) -> Run {
    static DURATION: OnceLock<Regex> = OnceLock::new();
    static YEAR: OnceLock<Regex> = OnceLock::new();
    let text = run.get("text").and_then(Value::as_str).unwrap_or("");
    if run.get("navigationEndpoint").is_some() {
        let id = nav(run, "navigationEndpoint.browseEndpoint.browseId").cloned().unwrap_or(Value::Null);
        let is_album = id.as_str().is_some_and(|s| s.starts_with("MPRE") || s.contains("release_detail"));
        let item = json!({"name": text, "id": id});
        return if is_album { Run::Album(item) } else { Run::Artist(item) };
    }
    if re(&DURATION, r"^(\d+:)*\d+:\d+$").is_match(text) {
        Run::Duration(text.to_string())
    } else if re(&YEAR, r"^\d{4}$").is_match(text) {
        Run::Year(text.to_string())
    } else if let Some(views) = parse_views(text) {
        Run::Views(views)
    } else {
        Run::Artist(json!({"name": text, "id": null}))
    }
}

/// ytmusicapi `parse_duration`: "3:25" -> 205; None when any part isn't digits.
pub fn parse_duration_text(duration: &str) -> Option<i64> {
    let d = duration.trim();
    if d.is_empty() {
        return None;
    }
    let parts: Vec<&str> = d.split(':').collect();
    if parts.iter().any(|p| p.is_empty() || !p.chars().all(|c| c.is_ascii_digit())) {
        return None;
    }
    let mut total = 0i64;
    for (mult, part) in [1i64, 60, 3600].iter().zip(parts.iter().rev()) {
        total += mult * part.parse::<i64>().ok()?;
    }
    Some(total)
}

fn is_dot_separator(run: &Value) -> bool {
    *run == json!({"text": " • "})
}

pub fn parse_song_runs(runs: &[Value], skip_type_spec: bool) -> Map<String, Value> {
    let mut runs = runs;
    if skip_type_spec
        && runs.len() > 2
        && runs[0].get("navigationEndpoint").is_none()
        && matches!(parse_song_run(&runs[0]), Run::Artist(_))
        && is_dot_separator(&runs[1])
        && matches!(parse_song_run(&runs[2]), Run::Artist(_) | Run::Duration(_) | Run::Views(_) | Run::Year(_))
    {
        runs = &runs[2..];
    }
    let mut parsed = Map::new();
    for (i, run) in runs.iter().enumerate() {
        if i % 2 == 1 {
            continue;
        }
        match parse_song_run(run) {
            Run::Album(a) => {
                parsed.insert("album".into(), a);
            }
            Run::Artist(a) => {
                let list = parsed.entry("artists").or_insert_with(|| Value::Array(vec![]));
                if let Value::Array(items) = list {
                    items.push(a);
                }
            }
            Run::Views(v) => {
                parsed.insert("views".into(), Value::String(v));
            }
            Run::Duration(d) => {
                parsed.insert("duration_seconds".into(), parse_duration_text(&d).map_or(Value::Null, Value::from));
                parsed.insert("duration".into(), Value::String(d));
            }
            Run::Year(y) => {
                parsed.insert("year".into(), Value::String(y));
            }
        }
    }
    parsed
}

/// Artist names and ids, skipping the separator runs.
pub fn parse_artists_runs(runs: &[Value]) -> Vec<Value> {
    (0..runs.len() / 2 + 1)
        .filter_map(|j| runs.get(j * 2))
        .map(|run| {
            json!({
                "name": run.get("text").cloned().unwrap_or(Value::Null),
                "id": nav(run, "navigationEndpoint.browseEndpoint.browseId").cloned().unwrap_or(Value::Null),
            })
        })
        .collect()
}

pub fn get_flex_column_item(item: &Value, index: usize) -> Option<&Value> {
    let col = nav(item, &format!("flexColumns.{index}.musicResponsiveListItemFlexColumnRenderer"))?;
    nav_arr(col, "text.runs")?;
    Some(col)
}

pub fn get_item_text(item: &Value, index: usize, run_index: usize) -> Option<String> {
    let col = get_flex_column_item(item, index)?;
    nav_str(col, &format!("text.runs.{run_index}.text")).map(str::to_string)
}

fn thumbnails(data: &Value) -> Value {
    nav(data, THUMBNAILS).cloned().unwrap_or(Value::Null)
}

fn is_available(data: &Value) -> bool {
    data.get("musicItemRendererDisplayPolicy").is_none_or(|p| p.as_str() != Some("MUSIC_ITEM_RENDERER_DISPLAY_POLICY_GREY_OUT"))
}

fn ctoken_params(results: &Value, ctoken_path: &str) -> Option<String> {
    let token = nav_str(results, &format!("continuations.0.next{ctoken_path}ContinuationData.continuation"))?;
    Some(format!("&ctoken={token}&continuation={token}"))
}

// ---------------------------------------------------------------- search

const RESULT_TYPES: [&str; 9] = ["album", "artist", "playlist", "song", "video", "station", "profile", "podcast", "episode"];

fn parse_top_result(data: &Value) -> Value {
    let subtitle = nav_str(data, "subtitle.runs.0.text").unwrap_or("").to_lowercase();
    let result_type = if subtitle.is_empty() {
        None
    } else if RESULT_TYPES.contains(&subtitle.as_str()) {
        Some(subtitle)
    } else {
        Some("album".to_string())
    };
    let mut out = Map::new();
    out.insert("resultType".into(), result_type.clone().map_or(Value::Null, Value::String));
    if result_type.as_deref() == Some("song") {
        out.insert("videoId".into(), nav(data, "onTap.watchEndpoint.videoId").cloned().unwrap_or(Value::Null));
        out.insert("videoType".into(), nav(data, &format!("onTap.{NAV_VIDEO_TYPE}")).cloned().unwrap_or(Value::Null));
        out.insert("title".into(), nav(data, "title.runs.0.text").cloned().unwrap_or(Value::Null));
        let runs = nav_arr(data, "subtitle.runs").map(|r| r.get(2..).unwrap_or(&[]).to_vec()).unwrap_or_default();
        out.extend(parse_song_runs(&runs, false));
        out.insert("thumbnails".into(), thumbnails(data));
    }
    Value::Object(out)
}

/// One search result row. Only songs are parsed in full: the worker drops
/// every other result type anyway.
pub fn parse_search_result(data: &Value, result_type: Option<&str>) -> Result<Value> {
    let video_type = nav(data, &format!("{PLAY_BUTTON}.playNavigationEndpoint.{NAV_VIDEO_TYPE}")).cloned();
    let result_type: String = match result_type {
        Some(t) => t.to_string(),
        None => {
            if let Some(browse_id) = nav_str(data, "navigationEndpoint.browseEndpoint.browseId") {
                let mapping = [
                    ("VM", "playlist"),
                    ("RD", "playlist"),
                    ("VL", "playlist"),
                    ("MPLA", "artist"),
                    ("MPRE", "album"),
                    ("MPSP", "podcast"),
                    ("MPED", "episode"),
                    ("UC", "artist"),
                ];
                match mapping.iter().find(|(p, _)| browse_id.starts_with(p)) {
                    Some((_, t)) => t.to_string(),
                    None => return Ok(json!({"resultType": null})),
                }
            } else {
                match video_type.as_ref().and_then(Value::as_str).unwrap_or("") {
                    "MUSIC_VIDEO_TYPE_ATV" => "song".into(),
                    "MUSIC_VIDEO_TYPE_PODCAST_EPISODE" => "episode".into(),
                    _ => "video".into(),
                }
            }
        }
    };
    if result_type != "song" {
        return Ok(json!({ "resultType": result_type }));
    }
    let mut out = Map::new();
    out.insert("resultType".into(), json!("song"));
    out.insert("title".into(), get_item_text(data, 0, 0).map_or(Value::Null, Value::String));
    out.insert("album".into(), Value::Null);
    out.insert(
        "videoId".into(),
        nav(data, &format!("{PLAY_BUTTON}.playNavigationEndpoint.watchEndpoint.videoId")).cloned().unwrap_or(Value::Null),
    );
    out.insert("videoType".into(), video_type.unwrap_or(Value::Null));
    out.insert("isAvailable".into(), Value::Bool(is_available(data)));
    out.insert("duration".into(), Value::Null);
    out.insert("year".into(), Value::Null);
    let flex = get_flex_column_item(data, 1).ok_or_else(|| Error::Msg("Expected flex column item at index 1".into()))?;
    let mut runs = nav_arr(flex, "text.runs").cloned().unwrap_or_default();
    if let Some(flex2) = get_flex_column_item(data, 2) {
        runs.push(json!({"text": ""}));
        runs.extend(nav_arr(flex2, "text.runs").cloned().unwrap_or_default());
    }
    out.extend(parse_song_runs(&runs, true));
    out.insert(
        "isExplicit".into(),
        Value::Bool(nav(data, "badges.0.musicInlineBadgeRenderer.accessibilityData.accessibilityData.label").is_some()),
    );
    out.insert("thumbnails".into(), thumbnails(data));
    Ok(Value::Object(out))
}

fn parse_search_results(contents: &[Value], result_type: Option<&str>) -> Result<Vec<Value>> {
    contents.iter().filter_map(|r| r.get(MRLIR)).map(|d| parse_search_result(d, result_type)).collect()
}

/// `YTMusic.search(query, filter="songs", limit=limit)`.
pub async fn search_songs(t: &dyn Transport, query: &str, limit: usize) -> Result<Vec<Value>> {
    let body = json!({"query": query, "params": SONGS_FILTER_PARAMS});
    let response = t.innertube("search", Client::Web, body.clone(), "").await?;
    let mut results: Vec<Value> = Vec::new();
    let Some(contents) = response.get("contents") else {
        return Ok(results);
    };
    let tab = if let Some(tabbed) = contents.get("tabbedSearchResultsRenderer") {
        nav(tabbed, "tabs.0.tabRenderer.content").ok_or_else(|| missing("tabs"))?
    } else {
        contents
    };
    let section_list = nav_arr(tab, "sectionListRenderer.contents").ok_or_else(|| missing("sectionListRenderer"))?;
    let mut result_type: Option<String> = None;
    for res in section_list {
        let mut category: Option<String> = None;
        let shelf_contents: Vec<Value>;
        let is_list_shelf;
        if let Some(card) = res.get("musicCardShelfRenderer") {
            results.push(parse_top_result(card));
            let Some(mut c) = nav_arr(card, "contents").filter(|c| !c.is_empty()).cloned() else {
                continue;
            };
            if c[0].get("messageRenderer").is_some() {
                let first = c.remove(0);
                category = nav_str(&first, "messageRenderer.text.runs.0.text").map(str::to_string);
            }
            shelf_contents = c;
            is_list_shelf = false;
        } else if let Some(shelf) = res.get("musicShelfRenderer") {
            let c = nav_arr(shelf, "contents").cloned().unwrap_or_default();
            if c.first().and_then(|f| f.get(MRLIR)).is_none() {
                continue;
            }
            category = nav_str(shelf, "title.runs.0.text").map(str::to_string);
            shelf_contents = c;
            is_list_shelf = true;
        } else if let Some(section) = res.get("itemSectionRenderer") {
            let c = nav_arr(section, "contents").cloned().unwrap_or_default();
            if c.first().and_then(|f| f.get(MRLIR)).is_none() {
                continue;
            }
            shelf_contents = c;
            is_list_shelf = true;
        } else {
            continue;
        }
        if is_list_shelf {
            // YTM sometimes pads results with a differently-categorised shelf.
            if category.as_ref().is_some_and(|c| !c.to_lowercase().contains("song")) {
                continue;
            }
            result_type = Some("song".into());
        }
        results.extend(parse_search_results(&shelf_contents, result_type.as_deref())?);

        // Continuations (only music shelves have them).
        let Some(mut shelf) = res.get("musicShelfRenderer").cloned() else {
            continue;
        };
        let want = limit as i64 - results.len() as i64;
        let mut got: Vec<Value> = Vec::new();
        while shelf.get("continuations").is_some() && (got.len() as i64) < want {
            let Some(params) = ctoken_params(&shelf, "") else { break };
            let resp = t.innertube("search", Client::Web, body.clone(), &params).await?;
            let Some(next) = nav(&resp, "continuationContents.musicShelfContinuation").cloned() else {
                break;
            };
            shelf = next;
            let items = continuation_contents(&shelf);
            let parsed = parse_search_results(items, result_type.as_deref())?;
            if parsed.is_empty() {
                break;
            }
            got.extend(parsed);
        }
        results.extend(got);
    }
    Ok(results)
}

fn continuation_contents(results: &Value) -> &[Value] {
    for term in ["contents", "items"] {
        if let Some(items) = nav_arr(results, term) {
            return items;
        }
    }
    &[]
}

// ------------------------------------------------------------- playlists

pub fn parse_playlist_items(results: &[Value], is_album: bool, is_collaborative: bool) -> Vec<Value> {
    results.iter().filter_map(|r| r.get(MRLIR)).filter_map(|d| parse_playlist_item(d, is_album, is_collaborative)).collect()
}

pub fn parse_playlist_item(data: &Value, is_album: bool, is_collaborative: bool) -> Option<Value> {
    let mut video_id: Value = Value::Null;
    if let Some(items) = nav_arr(data, "menu.menuRenderer.items") {
        for item in items {
            if let Some(service) = nav(item, "menuServiceItemRenderer.serviceEndpoint") {
                if service.get("playlistEditEndpoint").is_some() {
                    video_id = nav(service, "playlistEditEndpoint.actions.0.removedVideoId").cloned().unwrap_or(Value::Null);
                }
            }
        }
    }
    if let Some(vid) = nav(data, &format!("{PLAY_BUTTON}.playNavigationEndpoint.watchEndpoint.videoId")) {
        video_id = vid.clone();
    }
    let available = is_available(data);
    let preset = !available || is_album;
    let mut title_index: Option<usize> = preset.then_some(0);
    let mut artist_index: Option<usize> = preset.then_some(1);
    let mut duration_index: Option<usize> = None;
    let mut album_index: Option<usize> = if is_collaborative { Some(3) } else { preset.then_some(2) };
    let mut user_channel_indexes: Vec<usize> = Vec::new();
    let mut unrecognized_index: Option<usize> = None;

    let n = nav_arr(data, "flexColumns").map_or(0, Vec::len);
    for index in 0..n {
        let col = get_flex_column_item(data, index);
        let endpoint = col.and_then(|c| nav(c, "text.runs.0.navigationEndpoint"));
        let Some(endpoint) = endpoint else {
            if let Some(run) = col.and_then(|c| nav(c, "text.runs.0")) {
                if run.get("text").is_some() {
                    if matches!(parse_song_run(run), Run::Duration(_)) {
                        duration_index = Some(index);
                    } else if unrecognized_index.is_none() {
                        unrecognized_index = Some(index);
                    }
                }
            }
            continue;
        };
        if endpoint.get("watchEndpoint").is_some() {
            title_index = Some(index);
        } else if endpoint.get("browseEndpoint").is_some() {
            let page_type =
                nav_str(endpoint, "browseEndpoint.browseEndpointContextSupportedConfigs.browseEndpointContextMusicConfig.pageType")
                    .unwrap_or("");
            match page_type {
                "MUSIC_PAGE_TYPE_ARTIST" | "MUSIC_PAGE_TYPE_UNKNOWN" => artist_index = Some(index),
                "MUSIC_PAGE_TYPE_ALBUM" | "MUSIC_PAGE_TYPE_AUDIOBOOK" => album_index = Some(index),
                "MUSIC_PAGE_TYPE_USER_CHANNEL" => user_channel_indexes.push(index),
                "MUSIC_PAGE_TYPE_NON_MUSIC_AUDIO_TRACK_PAGE" => title_index = Some(index),
                _ => {}
            }
        }
    }
    if artist_index.is_none() && unrecognized_index.is_some() {
        artist_index = unrecognized_index;
    }
    if artist_index.is_none() {
        artist_index = user_channel_indexes.last().copied();
    }

    let title = title_index.and_then(|i| get_item_text(data, i, 0));
    if title.as_deref() == Some("Song deleted") {
        return None;
    }
    let artists = artist_index.map(|i| match get_flex_column_item(data, i) {
        Some(col) => Value::Array(parse_artists_runs(nav_arr(col, "text.runs").map_or(&[][..], |v| v.as_slice()))),
        None => Value::Array(vec![]),
    });
    let album = album_index.and_then(|i| {
        let col = get_flex_column_item(data, i)?;
        Some(json!({
            "name": get_item_text(data, i, 0),
            "id": nav(col, "text.runs.0.navigationEndpoint.browseEndpoint.browseId").cloned().unwrap_or(Value::Null),
        }))
    });
    let mut duration: Option<String> = duration_index.filter(|&i| i != 0).and_then(|i| get_item_text(data, i, 0));
    if data.get("fixedColumns").is_some() {
        let fixed = nav(data, "fixedColumns.0.musicResponsiveListItemFixedColumnRenderer.text");
        duration =
            fixed.and_then(|t| t.get("simpleText").and_then(Value::as_str).or_else(|| nav_str(t, "runs.0.text")).map(str::to_string));
    }
    let video_type = nav(data, &format!("menu.menuRenderer.items.0.{MNIR}.navigationEndpoint.{NAV_VIDEO_TYPE}")).cloned();

    let mut song = Map::new();
    song.insert("videoId".into(), video_id);
    song.insert("title".into(), title.map_or(Value::Null, Value::String));
    song.insert("artists".into(), artists.unwrap_or(Value::Null));
    song.insert("album".into(), album.unwrap_or(Value::Null));
    song.insert("thumbnails".into(), thumbnails(data));
    song.insert("isAvailable".into(), Value::Bool(available));
    song.insert("videoType".into(), video_type.unwrap_or(Value::Null));
    if let Some(d) = duration.filter(|d| !d.is_empty()) {
        song.insert("duration_seconds".into(), parse_duration_text(&d).map_or(Value::Null, Value::from));
        song.insert("duration".into(), Value::String(d));
    }
    Some(Value::Object(song))
}

fn continuation_token(results: &[Value]) -> Option<String> {
    let last = results.last()?;
    if let Some(token) = nav_str(last, "continuationItemRenderer.continuationEndpoint.continuationCommand.token") {
        return Some(token.to_string());
    }
    let commands = nav_arr(last, "continuationItemRenderer.continuationEndpoint.commandExecutorCommand.commands")?;
    commands.iter().find_map(|c| {
        (nav_str(c, "continuationCommand.request") == Some("CONTINUATION_REQUEST_TYPE_BROWSE"))
            .then(|| nav_str(c, "continuationCommand.token").map(str::to_string))
            .flatten()
    })
}

async fn continuations_2025(t: &dyn Transport, content_data: &Value, parse: impl Fn(&[Value]) -> Vec<Value>) -> Result<Vec<Value>> {
    let mut items = Vec::new();
    let mut token = continuation_token(nav_arr(content_data, "contents").map_or(&[][..], |v| v.as_slice()));
    while let Some(tok) = token {
        let resp = t.innertube("browse", Client::Web, json!({"continuation": tok}), "").await?;
        let Some(cont) = nav_arr(&resp, "onResponseReceivedActions.0.appendContinuationItemsAction.continuationItems") else {
            break;
        };
        if cont.is_empty() {
            break;
        }
        let parsed = parse(cont);
        if parsed.is_empty() {
            break;
        }
        items.extend(parsed);
        token = continuation_token(cont);
    }
    Ok(items)
}

pub struct Playlist {
    pub title: String,
    pub tracks: Vec<Value>,
}

/// `YTMusic.get_playlist(playlist_id, limit=None)` (title and tracks only).
pub async fn get_playlist(t: &dyn Transport, playlist_id: &str) -> Result<Playlist> {
    let browse_id = if playlist_id.starts_with("VL") { playlist_id.to_string() } else { format!("VL{playlist_id}") };
    let response = t.innertube("browse", Client::Web, json!({"browseId": browse_id}), "").await?;
    let is_ola = playlist_id.starts_with("OLA") || playlist_id.starts_with("VLOLA");
    let header_data = nav(&response, &format!("{TWO_COLUMN}.tabs.0.tabRenderer.content.sectionListRenderer.contents.0"));
    let section_list =
        nav(&response, &format!("{TWO_COLUMN}.secondaryContents.sectionListRenderer")).ok_or_else(|| missing("secondaryContents"))?;
    let content_data = nav(section_list, "contents.0.musicPlaylistShelfRenderer").ok_or_else(|| missing("musicPlaylistShelfRenderer"))?;

    let Some(header_data) = header_data else {
        if !is_ola {
            return Err(missing("header"));
        }
        // Album audio playlist without a header: title comes from the first track's album.
        let mut tracks = Vec::new();
        if let Some(contents) = nav_arr(content_data, "contents") {
            tracks = parse_playlist_items(contents, false, false);
            tracks.extend(continuations_2025(t, content_data, |c| parse_playlist_items(c, false, false)).await?);
        }
        let title = tracks.first().and_then(|t| nav_str(t, "album.name")).ok_or_else(|| missing("album name"))?.to_string();
        return Ok(Playlist { title, tracks });
    };
    let owned = header_data.get("musicEditablePlaylistDetailHeaderRenderer").is_some();
    let header = if owned {
        nav(header_data, "musicEditablePlaylistDetailHeaderRenderer.header.musicResponsiveHeaderRenderer")
    } else {
        nav(header_data, "musicResponsiveHeaderRenderer")
    }
    .ok_or_else(|| missing("musicResponsiveHeaderRenderer"))?;
    let title: String = nav_arr(header, "title.runs")
        .map(|runs| runs.iter().filter_map(|r| r.get("text").and_then(Value::as_str)).collect())
        .unwrap_or_default();
    let is_collaborative = header.get("facepile").is_some()
        && nav_str(
            header,
            "facepile.avatarStackViewModel.rendererContext.commandContext.onTap.innertubeCommand.showEngagementPanelEndpoint.identifier.tag",
        ) == Some("PAplaylist_collaborate");

    let mut tracks = Vec::new();
    if let Some(contents) = nav_arr(content_data, "contents") {
        tracks = parse_playlist_items(contents, false, is_collaborative);
        tracks.extend(continuations_2025(t, content_data, |c| parse_playlist_items(c, false, is_collaborative)).await?);
    }
    Ok(Playlist { title, tracks })
}

pub struct Album {
    pub title: Value,
    pub thumbnails: Value,
    pub artists: Value,
    pub tracks: Vec<Value>,
}

/// `YTMusic.get_album(browse_id)` (the fields the worker reads).
pub async fn get_album(t: &dyn Transport, browse_id: &str) -> Result<Album> {
    if !browse_id.starts_with("MPRE") {
        return Err("Invalid album browseId provided, must start with MPRE.".into());
    }
    let response = t.innertube("browse", Client::Web, json!({"browseId": browse_id}), "").await?;
    let header =
        nav(&response, &format!("{TWO_COLUMN}.tabs.0.tabRenderer.content.sectionListRenderer.contents.0.musicResponsiveHeaderRenderer"))
            .ok_or_else(|| missing("musicResponsiveHeaderRenderer"))?;
    let title = nav(header, "title.runs.0.text").cloned().ok_or_else(|| missing("title"))?;
    let thumbs = thumbnails(header);
    let artists = match nav_arr(header, "straplineTextOne.runs") {
        Some(runs) if !runs.is_empty() => Value::Array(parse_artists_runs(runs)),
        _ => Value::Null,
    };
    let shelf = nav(&response, &format!("{TWO_COLUMN}.secondaryContents.sectionListRenderer.contents.0.musicShelfRenderer"))
        .ok_or_else(|| missing("musicShelfRenderer"))?;
    let mut tracks = parse_playlist_items(nav_arr(shelf, "contents").map_or(&[][..], |v| v.as_slice()), true, false);
    for track in &mut tracks {
        if let Value::Object(m) = track {
            m.insert("album".into(), title.clone());
            let has_artists = m.get("artists").is_some_and(crate::util::truthy);
            if !has_artists {
                m.insert("artists".into(), artists.clone());
            }
        }
    }
    Ok(Album { title, thumbnails: thumbs, artists, tracks })
}

// ----------------------------------------------------------------- watch

pub fn parse_watch_track(data: &Value) -> Option<Value> {
    let mut track = Map::new();
    track.insert("videoId".into(), data.get("videoId")?.clone());
    track.insert("title".into(), nav(data, "title.runs.0.text")?.clone());
    track.insert("length".into(), nav(data, "lengthText.runs.0.text").cloned().unwrap_or(Value::Null));
    track.insert("thumbnail".into(), nav(data, "thumbnail.thumbnails").cloned().unwrap_or(Value::Null));
    track.insert("videoType".into(), nav(data, &format!("navigationEndpoint.{NAV_VIDEO_TYPE}")).cloned().unwrap_or(Value::Null));
    if let Some(runs) = nav_arr(data, "longBylineText.runs") {
        track.extend(parse_song_runs(runs, false));
    }
    Some(Value::Object(track))
}

pub fn parse_watch_playlist(results: &[Value]) -> Vec<Value> {
    const PPVWR: &str = "playlistPanelVideoWrapperRenderer";
    const PPVR: &str = "playlistPanelVideoRenderer";
    let mut tracks = Vec::new();
    for result in results {
        let result = match result.get(PPVWR) {
            Some(w) => match w.get("primaryRenderer") {
                Some(p) => p,
                None => continue,
            },
            None => result,
        };
        let Some(data) = result.get(PPVR) else { continue };
        if data.get("unplayableText").is_some() {
            continue;
        }
        if let Some(track) = parse_watch_track(data) {
            tracks.push(track);
        }
    }
    tracks
}

pub struct WatchPlaylist {
    pub tracks: Vec<Value>,
    pub lyrics: Option<String>,
}

/// `YTMusic.get_watch_playlist(videoId=..., limit=..., radio=...)`.
pub async fn get_watch_playlist(t: &dyn Transport, video_id: &str, limit: usize, radio: bool) -> Result<WatchPlaylist> {
    let playlist_id = format!("RDAMVM{video_id}");
    let mut body = json!({
        "enablePersistentPlaylistPanel": true,
        "isAudioOnly": true,
        "tunerSettingValue": "AUTOMIX_SETTING_NORMAL",
        "videoId": video_id,
    });
    let map = body.as_object_mut().expect("object");
    if !radio {
        map.insert(
            "watchEndpointMusicSupportedConfigs".into(),
            json!({"watchEndpointMusicConfig": {"hasPersistentPlaylistPanel": true, "musicVideoType": "MUSIC_VIDEO_TYPE_ATV"}}),
        );
    }
    map.insert("playlistId".into(), json!(playlist_id));
    if radio {
        map.insert("params".into(), json!("wAEB"));
    }
    let response = t.innertube("next", Client::Web, body.clone(), "").await?;
    let renderer = nav(&response, "contents.singleColumnMusicWatchNextResultsRenderer.tabbedRenderer.watchNextTabbedResultsRenderer")
        .ok_or_else(|| missing("watchNextTabbedResultsRenderer"))?;
    let mut lyrics = None;
    for tab in nav_arr(renderer, "tabs").map_or(&[][..], |v| v.as_slice()) {
        if nav(tab, "tabRenderer.unselectable").is_some() {
            continue;
        }
        if let Some(be) = nav(tab, "tabRenderer.endpoint.browseEndpoint") {
            let page_type = nav_str(be, "browseEndpointContextSupportedConfigs.browseEndpointContextMusicConfig.pageType");
            if page_type == Some("MUSIC_PAGE_TYPE_TRACK_LYRICS") {
                lyrics = be.get("browseId").and_then(Value::as_str).map(str::to_string);
            }
        }
    }
    let results = nav(renderer, "tabs.0.tabRenderer.content.musicQueueRenderer.content.playlistPanelRenderer")
        .filter(|r| crate::util::truthy(r))
        .ok_or_else(|| {
            Error::Msg(format!(
                "No content returned by the server.\nEnsure you have access to {playlist_id} - a private playlist may cause this."
            ))
        })?;
    let mut tracks = parse_watch_playlist(nav_arr(results, "contents").map_or(&[][..], |v| v.as_slice()));
    let want = limit as i64 - tracks.len() as i64;
    let mut more = Vec::new();
    let mut cur = results.clone();
    while cur.get("continuations").is_some() && (more.len() as i64) < want {
        let Some(params) = ctoken_params(&cur, "Radio") else { break };
        let resp = t.innertube("next", Client::Web, body.clone(), &params).await?;
        let Some(next) = nav(&resp, "continuationContents.playlistPanelContinuation").cloned() else {
            break;
        };
        cur = next;
        let parsed = parse_watch_playlist(continuation_contents(&cur));
        if parsed.is_empty() {
            break;
        }
        more.extend(parsed);
    }
    tracks.extend(more);
    Ok(WatchPlaylist { tracks, lyrics })
}

// ---------------------------------------------------------------- lyrics

pub enum YtLyrics {
    Timed { lines: Vec<(i64, String)>, source: Option<String> },
    Plain { text: String, source: Option<String> },
}

/// `YTMusic.get_lyrics(browse_id, timestamps=True)`.
pub async fn get_lyrics(t: &dyn Transport, browse_id: &str) -> Result<Option<YtLyrics>> {
    if browse_id.is_empty() {
        return Err("Invalid browseId provided. This song might not have lyrics.".into());
    }
    let response = t.innertube("browse", Client::Mobile, json!({"browseId": browse_id}), "").await?;
    let timed = nav(&response, "contents.elementRenderer.newElement.type.componentType.model.timedLyricsModel.lyricsData");
    if let Some(data) = timed.filter(|d| !d.is_null()) {
        let Some(raw) = nav_arr(data, "timedLyricsData") else {
            return Ok(None);
        };
        let source = data.get("sourceMessage").and_then(Value::as_str).map(str::to_string);
        if raw.iter().all(|l| l.get("cueRange").is_some()) {
            let lines = raw
                .iter()
                .map(|l| {
                    let start = nav(l, "cueRange.startTimeMilliseconds")
                        .and_then(|v| v.as_str().and_then(|s| s.parse::<i64>().ok()).or_else(|| v.as_i64()))
                        .unwrap_or(0);
                    (start, l.get("lyricLine").and_then(Value::as_str).unwrap_or("").to_string())
                })
                .collect();
            return Ok(Some(YtLyrics::Timed { lines, source }));
        }
        let text = raw.iter().map(|l| l.get("lyricLine").and_then(Value::as_str).unwrap_or("")).collect::<Vec<_>>().join("\n");
        return Ok(Some(YtLyrics::Plain { text, source }));
    }
    let shelf = "contents.sectionListRenderer.contents.0.musicDescriptionShelfRenderer";
    let Some(text) = nav_str(&response, &format!("{shelf}.description.runs.0.text")) else {
        return Ok(None);
    };
    let source = nav_str(&response, &format!("{shelf}.runs.0.text")).map(str::to_string);
    Ok(Some(YtLyrics::Plain { text: text.to_string(), source }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn song_runs_with_type_spec() {
        let runs = vec![
            json!({"text": "Song"}),
            json!({"text": " • "}),
            json!({"text": "Queen", "navigationEndpoint": {"browseEndpoint": {"browseId": "UC1"}}}),
            json!({"text": " • "}),
            json!({"text": "A Night At The Opera", "navigationEndpoint": {"browseEndpoint": {"browseId": "MPREb_x"}}}),
            json!({"text": " • "}),
            json!({"text": "5:55"}),
        ];
        let p = parse_song_runs(&runs, true);
        assert_eq!(p["artists"], json!([{"name": "Queen", "id": "UC1"}]));
        assert_eq!(p["album"]["name"], "A Night At The Opera");
        assert_eq!(p["duration_seconds"], 355);
        let p = parse_song_runs(&runs, false);
        assert_eq!(p["artists"].as_array().unwrap().len(), 2, "without skipping, 'Song' is an artist");
    }

    #[test]
    fn views_and_years() {
        assert_eq!(parse_song_run(&json!({"text": "1.6B plays"})), Run::Views("1.6B".into()));
        assert_eq!(parse_song_run(&json!({"text": "1975"})), Run::Year("1975".into()));
        assert!(matches!(parse_song_run(&json!({"text": "2Pac"})), Run::Artist(_)));
        assert!(matches!(parse_song_run(&json!({"text": "Maroon 5"})), Run::Artist(_)));
        assert_eq!(parse_duration_text("1:02:03"), Some(3723));
        assert_eq!(parse_duration_text("2,343"), None);
        assert_eq!(parse_duration_text(" "), None);
    }
}
