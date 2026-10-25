//! Public Spotify, Apple Music, Deezer and SoundCloud links (playlist, album,
//! song), no login and no API keys. Each reader turns a link into a title and
//! a list of songs (title, artists, duration); `providers_match` then finds
//! those songs on YouTube Music so the normal download pipeline can take over.
//!
//! - Spotify: the `open.spotify.com/embed/…` page carries the songs as JSON
//!   (capped at 100). Bigger lists are paged through api.spotify.com with the
//!   anonymous token that same page contains; if that is refused the first 100
//!   songs are used and the result says so.
//! - Apple Music: the page's `serialized-server-data` JSON.
//! - Deezer: the public API (api.deezer.com).
//! - SoundCloud: yt-dlp (`--flat-playlist` for the set, then one lookup per song).
//!   These songs are not matched on YouTube; they download from SoundCloud (`sc_<id>`).

use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use futures_util::stream::{self, StreamExt};
use serde_json::{json, Value};

use crate::net::{Error, Result, Transport};
use crate::ytm::{require_ytdlp, ytdlp_json};

const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0";
const PAGE_TIMEOUT: Duration = Duration::from_secs(25);
/// Spotify's embed page lists at most this many songs.
const SPOTIFY_EMBED_CAP: usize = 100;
/// Never read more than this many songs from one link.
const MAX_SONGS: usize = 5000;
const YTDLP_SET_TIMEOUT: Duration = Duration::from_secs(90);
const YTDLP_TRACK_TIMEOUT: Duration = Duration::from_secs(40);
/// SoundCloud songs looked up at the same time.
const SOUNDCLOUD_PARALLEL: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Service {
    Spotify,
    AppleMusic,
    Deezer,
    SoundCloud,
}

impl Service {
    pub fn name(self) -> &'static str {
        match self {
            Service::Spotify => "Spotify",
            Service::AppleMusic => "Apple Music",
            Service::Deezer => "Deezer",
            Service::SoundCloud => "SoundCloud",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Playlist,
    Album,
    Track,
    /// A short link (`spotify.link`, `on.soundcloud.com`, …): known after following it.
    Unknown,
}

/// A recognised link; `url` is the canonical form the reader fetches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub service: Service,
    pub kind: Kind,
    pub url: String,
}

/// One song as the other service describes it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Source {
    pub title: String,
    pub artists: Vec<String>,
    pub duration_sec: i64,
}

impl Source {
    /// "Artist, Artist - Title", for messages.
    pub fn label(&self) -> String {
        if self.artists.is_empty() {
            self.title.clone()
        } else {
            format!("{} - {}", self.artists.join(", "), self.title)
        }
    }
}

/// What a link contains.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Collection {
    pub title: String,
    pub tracks: Vec<Source>,
    /// The link was one song, not a playlist or album.
    pub single: bool,
    /// SoundCloud songs, ready to download from SoundCloud itself (shared track shape).
    pub direct: Vec<Value>,
    /// Songs that could not even be read (SoundCloud lookups that failed).
    pub unreadable: Vec<String>,
    /// Something the user should know (e.g. Spotify's 100 song cap).
    pub note: Option<String>,
}

// ------------------------------------------------------------ recognising links

const SHORT_HOSTS: &[(&str, Service)] = &[
    ("spotify.link", Service::Spotify),
    ("spotify.app.link", Service::Spotify),
    ("deezer.page.link", Service::Deezer),
    ("link.deezer.com", Service::Deezer),
    ("on.soundcloud.com", Service::SoundCloud),
    ("soundcloud.app.goo.gl", Service::SoundCloud),
];

fn valid_id(id: &str) -> bool {
    (4..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
}

/// Recognise a Spotify, Apple Music, Deezer or SoundCloud link (also
/// `spotify:track:ID` URIs and scheme-less pastes). `None` for anything else.
pub fn parse_link(input: &str) -> Option<Link> {
    let trimmed = input.trim();
    if trimmed.is_empty() || trimmed.contains(char::is_whitespace) {
        return None;
    }
    if let Some(rest) = trimmed.strip_prefix("spotify:") {
        let mut parts = rest.split(':');
        let (kind, id) = (parts.next()?, parts.next()?);
        return spotify_link(kind, id);
    }
    let with_scheme = if trimmed.contains("://") { trimmed.to_string() } else { format!("https://{trimmed}") };
    let url = url::Url::parse(&with_scheme).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    let segments: Vec<&str> = url.path_segments()?.filter(|p| !p.is_empty()).collect();

    if let Some((_, service)) = SHORT_HOSTS.iter().find(|(h, _)| *h == host) {
        return (!segments.is_empty() || url.query().is_some()).then(|| Link {
            service: *service,
            kind: Kind::Unknown,
            url: url.as_str().to_string(),
        });
    }
    match host {
        "open.spotify.com" | "play.spotify.com" => {
            // Skip `intl-de/` and `embed/` prefixes.
            let rest: Vec<&str> = segments.iter().copied().skip_while(|s| s.starts_with("intl-") || *s == "embed").collect();
            match rest.as_slice() {
                [kind, id, ..] => spotify_link(kind, id),
                _ => None,
            }
        }
        "music.apple.com" | "embed.music.apple.com" => apple_link(&url, &segments),
        "deezer.com" => {
            // Optional language prefix (`/en/`, `/de/`).
            let rest: Vec<&str> = segments.iter().copied().skip_while(|s| s.len() == 2 || (s.len() == 5 && s.contains('-'))).collect();
            match rest.as_slice() {
                [kind @ ("playlist" | "album" | "track"), id, ..] if id.bytes().all(|b| b.is_ascii_digit()) && !id.is_empty() => {
                    let kind = match *kind {
                        "playlist" => Kind::Playlist,
                        "album" => Kind::Album,
                        _ => Kind::Track,
                    };
                    Some(Link { service: Service::Deezer, kind, url: format!("https://www.deezer.com/{}/{id}", kind_word(kind)) })
                }
                _ => None,
            }
        }
        "soundcloud.com" | "m.soundcloud.com" => soundcloud_link(&segments),
        _ => None,
    }
}

fn kind_word(kind: Kind) -> &'static str {
    match kind {
        Kind::Playlist => "playlist",
        Kind::Album => "album",
        _ => "track",
    }
}

fn spotify_link(kind: &str, id: &str) -> Option<Link> {
    let kind = match kind {
        "playlist" => Kind::Playlist,
        "album" => Kind::Album,
        "track" => Kind::Track,
        _ => return None,
    };
    (valid_id(id) && id.bytes().all(|b| b.is_ascii_alphanumeric())).then(|| Link {
        service: Service::Spotify,
        kind,
        url: format!("https://open.spotify.com/{}/{id}", kind_word(kind)),
    })
}

/// `/{cc}/playlist/name/pl.xxx`, `/{cc}/album/name/ID[?i=TRACK]`, `/{cc}/song/name/ID`.
fn apple_link(url: &url::Url, segments: &[&str]) -> Option<Link> {
    let (cc, kind, rest) = match segments {
        [cc, kind, rest @ ..] if cc.len() == 2 && !rest.is_empty() => (*cc, *kind, rest),
        _ => return None,
    };
    let id = *rest.last()?;
    if !valid_id(id) {
        return None;
    }
    let slug = if rest.len() > 1 { rest[0] } else { "-" };
    let track = url.query_pairs().find(|(k, _)| k == "i").map(|(_, v)| v.into_owned()).filter(|v| v.bytes().all(|b| b.is_ascii_digit()));
    let (kind, tail) = match kind {
        "playlist" if id.starts_with("pl.") => (Kind::Playlist, String::new()),
        "album" if id.bytes().all(|b| b.is_ascii_digit()) => match &track {
            Some(t) => (Kind::Track, format!("?i={t}")),
            None => (Kind::Album, String::new()),
        },
        "song" if id.bytes().all(|b| b.is_ascii_digit()) => (Kind::Track, String::new()),
        _ => return None,
    };
    let word = if kind == Kind::Track && tail.is_empty() { "song" } else { segments[1] };
    Some(Link { service: Service::AppleMusic, kind, url: format!("https://music.apple.com/{cc}/{word}/{slug}/{id}{tail}") })
}

/// `/user/track` is a song, `/user/sets/name` a playlist; profiles, likes and so on are not.
fn soundcloud_link(segments: &[&str]) -> Option<Link> {
    const NOT_USERS: &[&str] = &["discover", "stream", "charts", "search", "you", "upload", "feed", "pages", "mobile", "settings"];
    let (first, rest) = segments.split_first()?;
    if NOT_USERS.contains(first) {
        return None;
    }
    match rest {
        [track]
            if !["sets", "albums", "tracks", "likes", "reposts", "followers", "following", "popular-tracks", "comments"]
                .contains(track) =>
        {
            Some(Link { service: Service::SoundCloud, kind: Kind::Track, url: format!("https://soundcloud.com/{first}/{track}") })
        }
        // A private set keeps its secret token (`/sets/name/s-xxxx`).
        ["sets", name] | ["sets", name, _] => {
            let token = if rest.len() == 3 { format!("/{}", rest[2]) } else { String::new() };
            Some(Link {
                service: Service::SoundCloud,
                kind: Kind::Playlist,
                url: format!("https://soundcloud.com/{first}/sets/{name}{token}"),
            })
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------- reading

fn http() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| reqwest::Client::builder().connect_timeout(Duration::from_secs(15)).build().unwrap_or_default())
}

fn net_error(e: reqwest::Error) -> Error {
    Error::Net(format!("connection failed: {}", crate::net::describe(&e)))
}

/// Follow a short link to the page it points at.
async fn follow_short(link: &Link) -> Result<Link> {
    let resp = http().get(&link.url).header("user-agent", USER_AGENT).timeout(PAGE_TIMEOUT).send().await.map_err(net_error)?;
    parse_link(resp.url().as_str())
        .filter(|l| l.kind != Kind::Unknown)
        .ok_or_else(|| Error::Msg(format!("That {} link doesn't point to a song, album or playlist", link.service.name())))
}

async fn page_text(t: &dyn Transport, url: &str) -> Result<String> {
    let bytes = t.get_bytes(url, USER_AGENT, PAGE_TIMEOUT).await?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// The JSON inside `<script … {marker} …>…</script>`.
fn script_json(html: &str, marker: &str) -> Option<Value> {
    let at = html.find(marker)?;
    let start = at + html[at..].find('>')? + 1;
    let end = start + html[start..].find("</script>")?;
    serde_json::from_str(&html[start..end]).ok()
}

/// Read a link into its songs. Short links are followed first.
pub async fn read(t: &dyn Transport, ytdlp: Option<&Path>, link: &Link) -> Result<Collection> {
    let link = if link.kind == Kind::Unknown { follow_short(link).await? } else { link.clone() };
    match link.service {
        Service::Spotify => read_spotify(t, &link).await,
        Service::AppleMusic => read_apple(t, &link).await,
        Service::Deezer => read_deezer(t, &link).await,
        Service::SoundCloud => read_soundcloud(ytdlp, &link).await,
    }
}

fn id_of(link: &Link) -> &str {
    link.url.split(['?', '#']).next().unwrap_or("").rsplit('/').next().unwrap_or("")
}

fn unavailable(service: Service, what: &str) -> Error {
    Error::Msg(format!("That {} {what} is private or doesn't exist", service.name()))
}

// --------------------------------------------------------------------- Spotify

/// Artists in the embed's `subtitle` are joined with ",\u{a0}".
fn split_artists(subtitle: &str) -> Vec<String> {
    subtitle.split(',').map(|a| a.replace('\u{a0}', " ").trim().to_string()).filter(|a| !a.is_empty()).collect()
}

#[derive(Debug)]
pub struct SpotifyEmbed {
    pub collection: Collection,
    /// The anonymous web-player token in the page, for paging past the cap.
    pub token: Option<String>,
    /// The page shows as many songs as it ever does, so there may be more.
    pub capped: bool,
}

pub fn spotify_from_embed(html: &str) -> Result<SpotifyEmbed> {
    let data = script_json(html, "id=\"__NEXT_DATA__\"").ok_or_else(|| Error::Msg("Couldn't read that Spotify page".into()))?;
    let state = data.pointer("/props/pageProps/state").ok_or_else(|| unavailable(Service::Spotify, "link"))?;
    let entity = state.pointer("/data/entity").ok_or_else(|| unavailable(Service::Spotify, "link"))?;
    let token = state.pointer("/settings/session/accessToken").and_then(Value::as_str).map(str::to_string);
    let title = entity.get("name").or_else(|| entity.get("title")).and_then(Value::as_str).unwrap_or("").to_string();
    let ms = |v: &Value| v.get("duration").and_then(Value::as_i64).unwrap_or(0) / 1000;

    if entity.get("type").and_then(Value::as_str) == Some("track") {
        let artists = entity
            .get("artists")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|x| x.get("name").and_then(Value::as_str)).map(str::to_string).collect())
            .unwrap_or_default();
        let track = Source { title: title.clone(), artists, duration_sec: ms(entity) };
        return Ok(SpotifyEmbed {
            collection: Collection { title, tracks: vec![track], single: true, ..Default::default() },
            token,
            capped: false,
        });
    }
    let list = entity.get("trackList").and_then(Value::as_array).ok_or_else(|| unavailable(Service::Spotify, "link"))?;
    let tracks: Vec<Source> = list
        .iter()
        .filter(|t| t.get("uri").and_then(Value::as_str).is_some_and(|u| u.starts_with("spotify:track:")))
        .filter_map(|t| {
            Some(Source {
                title: t.get("title")?.as_str()?.to_string(),
                artists: split_artists(t.get("subtitle").and_then(Value::as_str).unwrap_or("")),
                duration_sec: ms(t),
            })
        })
        .collect();
    Ok(SpotifyEmbed { collection: Collection { title, tracks, ..Default::default() }, token, capped: list.len() >= SPOTIFY_EMBED_CAP })
}

/// One page of api.spotify.com `…/tracks`: (songs, whether it was full).
pub fn spotify_api_page(page: &Value, playlist: bool) -> (Vec<Source>, usize) {
    let items = page.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
    let tracks = items
        .iter()
        .filter_map(|item| {
            let t = if playlist { item.get("track").filter(|t| t.is_object())? } else { item };
            if t.get("is_local") == Some(&Value::Bool(true)) || t.get("type").and_then(Value::as_str).is_some_and(|ty| ty != "track") {
                return None;
            }
            Some(Source {
                title: t.get("name")?.as_str()?.to_string(),
                artists: t
                    .get("artists")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(|x| x.get("name").and_then(Value::as_str)).map(str::to_string).collect())
                    .unwrap_or_default(),
                duration_sec: t.get("duration_ms").and_then(Value::as_i64).unwrap_or(0) / 1000,
            })
        })
        .collect();
    (tracks, items.len())
}

async fn spotify_api_all(kind: &str, id: &str, token: &str) -> Result<Vec<Source>> {
    let (limit, fields) = if kind == "playlist" {
        (100usize, "total,items(track(name,duration_ms,artists(name),is_local,type))")
    } else {
        (50usize, "total,items(name,duration_ms,artists(name))")
    };
    let mut all = Vec::new();
    let mut offset = 0usize;
    while offset < MAX_SONGS {
        let resp = http()
            .get(format!("https://api.spotify.com/v1/{kind}s/{id}/tracks"))
            .query(&[("limit", limit.to_string()), ("offset", offset.to_string()), ("fields", fields.to_string())])
            .bearer_auth(token)
            .header("user-agent", USER_AGENT)
            .timeout(PAGE_TIMEOUT)
            .send()
            .await
            .map_err(net_error)?;
        if !resp.status().is_success() {
            return Err(Error::Http(resp.status().as_u16(), format!("Spotify API HTTP {}", resp.status().as_u16())));
        }
        let page: Value = resp.json().await.map_err(net_error)?;
        let (tracks, count) = spotify_api_page(&page, kind == "playlist");
        all.extend(tracks);
        offset += limit;
        if count < limit {
            break;
        }
    }
    Ok(all)
}

async fn read_spotify(t: &dyn Transport, link: &Link) -> Result<Collection> {
    let id = id_of(link);
    let html = page_text(t, &format!("https://open.spotify.com/embed/{}/{id}", kind_word(link.kind))).await?;
    let embed = spotify_from_embed(&html)?;
    let mut collection = embed.collection;
    if embed.capped {
        let kind = kind_word(link.kind);
        let paged = match &embed.token {
            Some(token) => spotify_api_all(kind, id, token).await,
            None => Err(Error::Msg("no token".into())),
        };
        match paged {
            Ok(all) if all.len() > collection.tracks.len() => collection.tracks = all,
            Ok(_) => {}
            Err(_) => {
                collection.note = Some(format!(
                    "Spotify only shows the first {} songs of this {kind} without a login; the rest were not imported",
                    collection.tracks.len()
                ))
            }
        }
    }
    Ok(collection)
}

// ----------------------------------------------------------------- Apple Music

/// Songs of an Apple Music page. `want` is the song id of a song link
/// (`?i=` or `/song/…/ID`); then only that song is returned.
pub fn apple_from_page(html: &str, want: Option<&str>) -> Result<Collection> {
    let data =
        script_json(html, "id=\"serialized-server-data\"").ok_or_else(|| Error::Msg("Couldn't read that Apple Music page".into()))?;
    let sections =
        data.pointer("/data/0/data/sections").and_then(Value::as_array).ok_or_else(|| unavailable(Service::AppleMusic, "link"))?;
    let header = sections.iter().filter_map(|s| s.get("items")?.as_array()?.first()).find(|i| i.get("trackCount").is_some());
    let title = header.and_then(|h| h.get("title")).and_then(Value::as_str).unwrap_or("").to_string();
    let list = sections
        .iter()
        .find(|s| s.get("itemKind").and_then(Value::as_str) == Some("trackLockup"))
        .and_then(|s| s.get("items")?.as_array())
        .ok_or_else(|| unavailable(Service::AppleMusic, "link"))?;
    let song_id = |item: &Value| -> Option<String> {
        item.pointer("/contentDescriptor/identifiers/storeAdamID").and_then(Value::as_str).map(str::to_string)
    };
    let to_source = |item: &Value| -> Option<Source> {
        let artists = item.get("artistName").and_then(Value::as_str).unwrap_or("");
        Some(Source {
            title: item.get("title")?.as_str()?.to_string(),
            artists: if artists.is_empty() { Vec::new() } else { vec![artists.to_string()] },
            duration_sec: item.get("duration").and_then(Value::as_i64).unwrap_or(0) / 1000,
        })
    };
    if let Some(want) = want {
        let item = list.iter().find(|i| song_id(i).as_deref() == Some(want)).ok_or_else(|| unavailable(Service::AppleMusic, "song"))?;
        let track = to_source(item).ok_or_else(|| unavailable(Service::AppleMusic, "song"))?;
        return Ok(Collection { title: track.title.clone(), tracks: vec![track], single: true, ..Default::default() });
    }
    let tracks: Vec<Source> = list.iter().filter_map(to_source).collect();
    let total = header.and_then(|h| h.get("trackCount")).and_then(Value::as_u64).unwrap_or(0) as usize;
    let note = (total > tracks.len()).then(|| {
        format!(
            "Apple Music only shows the first {} of {total} songs of this link without a login; the rest were not imported",
            tracks.len()
        )
    });
    Ok(Collection { title, tracks, note, ..Default::default() })
}

async fn read_apple(t: &dyn Transport, link: &Link) -> Result<Collection> {
    let want = match link.kind {
        Kind::Track => {
            let parsed = url::Url::parse(&link.url).map_err(|e| Error::Msg(e.to_string()))?;
            parsed.query_pairs().find(|(k, _)| k == "i").map(|(_, v)| v.into_owned()).or_else(|| Some(id_of(link).to_string()))
        }
        _ => None,
    };
    apple_from_page(&page_text(t, &link.url).await?, want.as_deref())
}

// ---------------------------------------------------------------------- Deezer

fn deezer_error(v: &Value, what: &str) -> Result<()> {
    if v.get("error").is_some() {
        return Err(unavailable(Service::Deezer, what));
    }
    Ok(())
}

/// One Deezer track object. `fallback_artist` is the album artist.
pub fn deezer_track(t: &Value, fallback_artist: Option<&str>) -> Option<Source> {
    let artist = t.pointer("/artist/name").and_then(Value::as_str).or(fallback_artist).filter(|a| !a.is_empty());
    Some(Source {
        title: t.get("title")?.as_str()?.to_string(),
        artists: artist.map(|a| vec![a.to_string()]).unwrap_or_default(),
        duration_sec: t.get("duration").and_then(Value::as_i64).unwrap_or(0),
    })
}

async fn read_deezer(t: &dyn Transport, link: &Link) -> Result<Collection> {
    let id = id_of(link);
    let kind = kind_word(link.kind);
    let head = t.get_json(&format!("https://api.deezer.com/{kind}/{id}"), USER_AGENT, PAGE_TIMEOUT).await?;
    deezer_error(&head, kind)?;
    let title = head.get("title").and_then(Value::as_str).unwrap_or("").to_string();
    if link.kind == Kind::Track {
        let mut track = deezer_track(&head, None).ok_or_else(|| unavailable(Service::Deezer, kind))?;
        let names: Vec<String> = head
            .get("contributors")
            .and_then(Value::as_array)
            .map(|c| c.iter().filter_map(|x| x.get("name")?.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        if !names.is_empty() {
            track.artists = names;
        }
        return Ok(Collection { title, tracks: vec![track], single: true, ..Default::default() });
    }
    let album_artist = head.pointer("/artist/name").and_then(Value::as_str).map(str::to_string);
    let mut tracks = Vec::new();
    let mut index = 0usize;
    while index < MAX_SONGS {
        let page =
            t.get_json(&format!("https://api.deezer.com/{kind}/{id}/tracks?limit=100&index={index}"), USER_AGENT, PAGE_TIMEOUT).await?;
        deezer_error(&page, kind)?;
        let items = page.get("data").and_then(Value::as_array).cloned().unwrap_or_default();
        tracks.extend(
            items
                .iter()
                .filter(|i| i.get("readable") != Some(&Value::Bool(false)))
                .filter_map(|i| deezer_track(i, album_artist.as_deref())),
        );
        if page.get("next").is_none() || items.is_empty() {
            break;
        }
        index += 100;
    }
    Ok(Collection { title, tracks, ..Default::default() })
}

// ------------------------------------------------------------------ SoundCloud

/// A SoundCloud song as a library track. Its id is `sc_<track id>`; it downloads
/// from SoundCloud, not YouTube.
pub fn soundcloud_track(info: &Value) -> Option<Value> {
    let source = soundcloud_source(info)?;
    let id = info.get("id").map(crate::util::py_str).filter(|i| !i.is_empty() && i.bytes().all(|b| b.is_ascii_digit()))?;
    Some(json!({
        "video_id": format!("sc_{id}"),
        "title": source.title,
        "artist": source.artists.first().cloned().unwrap_or_else(|| "Unknown artist".into()),
        "album": "",
        "duration_sec": source.duration_sec,
        "thumbnail_url": info.get("thumbnail").and_then(Value::as_str).unwrap_or(""),
        "file_path": Value::Null,
        "downloaded": false,
    }))
}

/// One song from yt-dlp's JSON for a SoundCloud track.
pub fn soundcloud_source(info: &Value) -> Option<Source> {
    let text = |k: &str| info.get(k).and_then(Value::as_str).filter(|s| !s.trim().is_empty());
    let title = text("track").or_else(|| text("title"))?.to_string();
    let artist = text("artist").or_else(|| text("uploader"));
    Some(Source {
        title,
        artists: artist.map(|a| vec![a.to_string()]).unwrap_or_default(),
        duration_sec: info.get("duration").and_then(Value::as_f64).unwrap_or(0.0) as i64,
    })
}

async fn read_soundcloud(ytdlp: Option<&Path>, link: &Link) -> Result<Collection> {
    let ytdlp = require_ytdlp(ytdlp)?;
    if link.kind == Kind::Track {
        let info = ytdlp_json(ytdlp, &["-J".into(), "--no-warnings".into(), "--no-playlist".into(), link.url.clone()], YTDLP_TRACK_TIMEOUT)
            .await?;
        let track = soundcloud_track(&info).ok_or_else(|| unavailable(Service::SoundCloud, "song"))?;
        let title = track["title"].as_str().unwrap_or("").to_string();
        return Ok(Collection { title, direct: vec![track], single: true, ..Default::default() });
    }
    let set =
        ytdlp_json(ytdlp, &["-J".into(), "--flat-playlist".into(), "--no-warnings".into(), link.url.clone()], YTDLP_SET_TIMEOUT).await?;
    let urls: Vec<String> = set
        .get("entries")
        .and_then(Value::as_array)
        .map(|e| e.iter().filter_map(|x| x.get("url")?.as_str().map(str::to_string)).take(MAX_SONGS).collect())
        .unwrap_or_default();
    if urls.is_empty() {
        return Err(unavailable(Service::SoundCloud, "playlist"));
    }
    // The flat listing has no titles or durations, so look each song up.
    let looked_up: Vec<(String, Option<Value>)> = stream::iter(urls)
        .map(|url| async move {
            let info =
                ytdlp_json(ytdlp, &["-J".into(), "--no-warnings".into(), "--no-playlist".into(), url.clone()], YTDLP_TRACK_TIMEOUT).await;
            let source = info.ok().and_then(|i| soundcloud_track(&i));
            (url, source)
        })
        .buffered(SOUNDCLOUD_PARALLEL)
        .collect()
        .await;
    let mut collection = Collection {
        title: set.get("title").and_then(Value::as_str).filter(|s| !s.is_empty()).unwrap_or("SoundCloud playlist").to_string(),
        ..Default::default()
    };
    for (url, source) in looked_up {
        match source {
            Some(s) => collection.direct.push(s),
            None => collection.unreadable.push(url),
        }
    }
    Ok(collection)
}

// ------------------------------------------------------------------ the command

/// `playlist_info` for a link of another service: read it, then find each
/// song on YouTube Music. Result: `{title, tracks, unmatched, note, single}`
/// (tracks in the shared track shape, in the original order).
pub async fn playlist_info(t: &dyn Transport, ytdlp: Option<&Path>, link: &Link) -> Result<Value> {
    let collection = read(t, ytdlp, link).await?;
    if collection.tracks.is_empty() && collection.direct.is_empty() && collection.unreadable.is_empty() {
        return Err(Error::Msg("That link has no songs".into()));
    }
    let matched = crate::providers_match::match_all(t, &collection.tracks).await?;
    let mut tracks: Vec<Value> = collection.direct.clone();
    let mut seen = std::collections::HashSet::new();
    let mut unmatched: Vec<String> = Vec::new();
    for (source, found) in collection.tracks.iter().zip(matched) {
        match found {
            // Two source songs can land on the same video; keep it once.
            Some(track) => {
                if seen.insert(track.get("video_id").and_then(Value::as_str).unwrap_or("").to_string()) {
                    tracks.push(track);
                }
            }
            None => unmatched.push(source.label()),
        }
    }
    unmatched.extend(collection.unreadable.iter().map(|u| format!("(couldn't read {u})")));
    let title = if collection.title.trim().is_empty() { "Imported playlist".to_string() } else { collection.title };
    Ok(json!({
        "title": title,
        "tracks": tracks,
        "unmatched": unmatched,
        "note": collection.note,
        "single": collection.single,
    }))
}
