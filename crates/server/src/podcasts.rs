//! Podcasts: iTunes search, RSS feeds, subscriptions, automatic episode
//! downloads and per-episode listening position.
//!
//! Feeds are parsed with `quick-xml` (a streaming pull parser: no DOM, so a
//! big feed costs one pass and no tree; it was already in the build as a
//! transitive dependency). Episodes are plain HTTP GETs of the RSS enclosure,
//! streamed to `<data dir>/podcasts/<podcast>/` (never the music folder, so
//! Navidrome does not pick them up). The only background work is one feed
//! check shortly after start and then every [`CHECK_EVERY`]; nothing runs
//! while idle otherwise. Every episode change is broadcast to
//! `GET /api/podcasts/events`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::sync::{broadcast, Semaphore};

use crate::db::Db;
use crate::error::{ApiError, ApiResult};
use crate::models::now_ms;

const ITUNES_SEARCH: &str = "https://itunes.apple.com/search";
/// Feeds are checked this long after the last check (and once after start).
pub const CHECK_EVERY: Duration = Duration::from_secs(4 * 3600);
/// First check this long after start, so startup stays quiet.
const FIRST_CHECK: Duration = Duration::from_secs(20);
const FEED_MAX_BYTES: usize = 12 * 1024 * 1024;
const COVER_MAX_BYTES: usize = 3 * 1024 * 1024;
/// More episodes than this per feed are ignored (oldest ones, usually).
const MAX_EPISODES: usize = 1000;
/// At most this many episodes download at once.
const PARALLEL: usize = 2;
/// At most this many progress saves/broadcasts per download.
const PROGRESS_EVERY: Duration = Duration::from_millis(300);
/// An episode counts as played from this fraction on.
const PLAYED_FRACTION: f64 = 0.95;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Podcast {
    pub id: i64,
    pub title: String,
    pub author: String,
    pub feed_url: String,
    pub image_url: String,
    pub subscribed: bool,
    pub subscribed_at: i64,
    pub last_checked: Option<i64>,
    pub last_error: Option<String>,
    pub episode_count: i64,
    pub unplayed_count: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Episode {
    pub id: i64,
    pub podcast_id: i64,
    pub title: String,
    /// Publication time, ms since the epoch (0 = unknown).
    pub published_at: i64,
    pub duration_sec: i64,
    pub enclosure_url: String,
    /// Local file; only set once the episode is downloaded.
    pub file_path: Option<String>,
    /// "none", "queued", "downloading", "done" or "failed".
    pub status: String,
    /// Download progress in percent (0..100).
    pub progress: f64,
    pub error: Option<String>,
    pub position_sec: f64,
    pub played: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchHit {
    pub title: String,
    pub author: String,
    pub feed_url: String,
    pub image_url: String,
    pub subscribed: bool,
}

/// One episode as the feed lists it.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct FeedEpisode {
    pub guid: String,
    pub title: String,
    pub published_at: i64,
    pub duration_sec: i64,
    pub enclosure_url: String,
}

#[derive(Debug, Default, Clone)]
pub struct Feed {
    pub title: String,
    pub author: String,
    pub image_url: String,
    pub episodes: Vec<FeedEpisode>,
}

// ------------------------------------------------------------------ parsing

#[derive(Clone, Copy, PartialEq)]
enum Field {
    Title,
    Guid,
    PubDate,
    Duration,
    Author,
    ImageUrl,
}

fn attr(e: &BytesStart, name: &str) -> Option<String> {
    e.try_get_attribute(name)
        .ok()
        .flatten()
        .and_then(|a| a.normalized_value(quick_xml::XmlVersion::Implicit1_0).ok())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Parse an RSS 2.0 podcast feed. Lenient: a broken tail keeps what came
/// before it. Items without an enclosure (no audio) are skipped.
pub fn parse_feed(xml: &[u8]) -> Result<Feed, String> {
    let mut reader = Reader::from_reader(xml);
    {
        let c = reader.config_mut();
        c.check_end_names = false;
        c.allow_dangling_amp = true;
    }
    let mut feed = Feed::default();
    let mut item: Option<FeedEpisode> = None;
    let mut in_image = false;
    let mut field: Option<Field> = None;
    let mut text = String::new();
    let mut channel = false;
    let mut buf = Vec::new();
    let mut error = None;
    loop {
        buf.clear();
        let ev = match reader.read_event_into(&mut buf) {
            Ok(ev) => ev,
            Err(e) => {
                error = Some(format!("bad feed XML: {e}"));
                break;
            }
        };
        let empty = matches!(ev, Event::Empty(_));
        match ev {
            Event::Start(e) | Event::Empty(e) => {
                let name = e.name();
                match name.as_ref() {
                    b"channel" => channel = true,
                    b"item" => item = Some(FeedEpisode::default()),
                    b"image" if item.is_none() => in_image = true,
                    b"url" if in_image => field = Some(Field::ImageUrl),
                    b"title" if !in_image && (item.is_some() || feed.title.is_empty()) => field = Some(Field::Title),
                    b"guid" if item.is_some() => field = Some(Field::Guid),
                    b"pubDate" if item.is_some() => field = Some(Field::PubDate),
                    b"itunes:duration" if item.is_some() => field = Some(Field::Duration),
                    b"itunes:author" | b"author" if item.is_none() && feed.author.is_empty() => field = Some(Field::Author),
                    b"enclosure" => {
                        if let (Some(it), Some(url)) = (item.as_mut(), attr(&e, "url")) {
                            if it.enclosure_url.is_empty() {
                                it.enclosure_url = url;
                            }
                        }
                    }
                    b"itunes:image" if item.is_none() => {
                        if let Some(href) = attr(&e, "href") {
                            feed.image_url = href;
                        }
                    }
                    _ => {}
                }
                text.clear();
                // `<tag/>` has no End event: nothing to collect.
                if empty {
                    field = None;
                    match name.as_ref() {
                        b"item" => item = None,
                        b"image" => in_image = false,
                        _ => {}
                    }
                }
            }
            Event::Text(t) => {
                if field.is_some() {
                    if let Ok(s) = t.decode() {
                        text.push_str(&s);
                    }
                }
            }
            Event::CData(c) => {
                if field.is_some() {
                    if let Ok(s) = c.decode() {
                        text.push_str(&s);
                    }
                }
            }
            Event::GeneralRef(r) => {
                if field.is_some() {
                    if let Ok(Some(ch)) = r.resolve_char_ref() {
                        text.push(ch);
                    } else if let Ok(name) = r.decode() {
                        match name.as_ref() {
                            "amp" => text.push('&'),
                            "lt" => text.push('<'),
                            "gt" => text.push('>'),
                            "quot" => text.push('"'),
                            "apos" => text.push('\''),
                            _ => text.push(' '),
                        }
                    }
                }
            }
            Event::End(e) => {
                if let Some(f) = field.take() {
                    let value = squash(&text);
                    match (f, item.as_mut()) {
                        (Field::Title, Some(it)) => it.title = value,
                        (Field::Title, None) => feed.title = value,
                        (Field::Guid, Some(it)) => it.guid = value,
                        (Field::PubDate, Some(it)) => it.published_at = parse_date(&value).unwrap_or(0),
                        (Field::Duration, Some(it)) => it.duration_sec = parse_duration(&value),
                        (Field::Author, _) => feed.author = value,
                        (Field::ImageUrl, _) => {
                            if feed.image_url.is_empty() {
                                feed.image_url = value;
                            }
                        }
                        _ => {}
                    }
                }
                match e.name().as_ref() {
                    b"image" => in_image = false,
                    b"item" => {
                        if let Some(mut it) = item.take().filter(|it| !it.enclosure_url.is_empty()) {
                            if it.guid.is_empty() {
                                it.guid = it.enclosure_url.clone();
                            }
                            if it.title.is_empty() {
                                it.title = "Untitled episode".into();
                            }
                            feed.episodes.push(it);
                            if feed.episodes.len() >= MAX_EPISODES {
                                break;
                            }
                        }
                    }
                    _ => {}
                }
                text.clear();
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if !channel {
        return Err(error.unwrap_or_else(|| "this is not an RSS podcast feed".into()));
    }
    if let (Some(e), true) = (error, feed.episodes.is_empty() && feed.title.is_empty()) {
        return Err(e);
    }
    if feed.title.is_empty() {
        feed.title = "Untitled podcast".into();
    }
    Ok(feed)
}

/// `1:02:03`, `62:03` or plain seconds (`3723`, `3723.5`) to whole seconds.
pub fn parse_duration(s: &str) -> i64 {
    let mut total = 0.0f64;
    for part in s.trim().split(':') {
        match part.trim().parse::<f64>() {
            Ok(n) if n >= 0.0 && n.is_finite() => total = total * 60.0 + n,
            _ => return 0,
        }
    }
    total as i64
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn zone_offset_min(z: &str) -> i64 {
    match z {
        "" | "GMT" | "UT" | "UTC" | "Z" => 0,
        "EST" => -300,
        "EDT" => -240,
        "CST" => -360,
        "CDT" => -300,
        "MST" => -420,
        "MDT" => -360,
        "PST" => -480,
        "PDT" => -420,
        z if (z.starts_with('+') || z.starts_with('-')) && z.len() >= 5 => {
            let sign = if z.starts_with('-') { -1 } else { 1 };
            let digits: String = z[1..].chars().filter(char::is_ascii_digit).collect();
            let n: i64 = digits.parse().unwrap_or(0);
            sign * ((n / 100) * 60 + n % 100)
        }
        _ => 0,
    }
}

/// RFC 2822 (`Tue, 10 Jun 2025 14:00:00 +0200`) or ISO 8601 (`2025-06-10T14:00:00Z`)
/// to ms since the epoch.
pub fn parse_date(s: &str) -> Option<i64> {
    let s = s.trim();
    let (y, m, d, hh, mm, ss, off);
    if s.len() >= 10 && s.as_bytes().get(4) == Some(&b'-') {
        let (date, time) = s.split_at(10);
        let mut dp = date.split('-');
        y = dp.next()?.parse::<i64>().ok()?;
        m = dp.next()?.parse::<i64>().ok()?;
        d = dp.next()?.parse::<i64>().ok()?;
        let time = time.trim_start_matches(['T', ' ']);
        let split = time.find(['Z', '+', '-']).unwrap_or(time.len());
        let (clock, zone) = time.split_at(split);
        let mut tp = clock.split(':');
        hh = tp.next().and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
        mm = tp.next().and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
        ss = tp.next().and_then(|v| v.split('.').next()?.parse::<i64>().ok()).unwrap_or(0);
        off = zone_offset_min(&zone.replace(':', ""));
    } else {
        let s = s.split_once(',').map_or(s, |(_, rest)| rest).trim();
        let mut parts = s.split_whitespace();
        d = parts.next()?.parse::<i64>().ok()?;
        let mon = parts.next()?;
        const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
        m = MONTHS.iter().position(|n| mon.get(..3).is_some_and(|p| p.eq_ignore_ascii_case(n)))? as i64 + 1;
        let mut year = parts.next()?.parse::<i64>().ok()?;
        if year < 100 {
            year += if year < 70 { 2000 } else { 1900 };
        }
        y = year;
        let mut tp = parts.next().unwrap_or("0:0:0").split(':');
        hh = tp.next().and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
        mm = tp.next().and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
        ss = tp.next().and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
        off = zone_offset_min(parts.next().unwrap_or(""));
    }
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || y < 1970 {
        return None;
    }
    let secs = days_from_civil(y, m, d) * 86400 + hh * 3600 + mm * 60 + ss - off * 60;
    (secs >= 0).then_some(secs * 1000)
}

// ------------------------------------------------------------------ files

/// A file or folder name made of letters, digits and a few safe symbols.
fn safe_name(s: &str) -> String {
    let cleaned: String =
        s.chars().map(|c| if c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '(' | ')' | ',') { c } else { ' ' }).collect();
    let name = squash(&cleaned);
    let name: String = name.chars().take(80).collect();
    let name = name.trim().to_string();
    if name.is_empty() {
        "podcast".into()
    } else {
        name
    }
}

fn audio_ext(url: &str, content_type: Option<&str>) -> &'static str {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let ext = path.rsplit('/').next().and_then(|f| f.rsplit_once('.')).map(|(_, e)| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("mp3") => "mp3",
        Some("m4a") | Some("m4b") => "m4a",
        Some("aac") => "aac",
        Some("ogg") | Some("oga") => "ogg",
        Some("opus") => "opus",
        Some("wav") => "wav",
        Some("flac") => "flac",
        Some("mp4") => "mp4",
        _ => match content_type.map(|c| c.split(';').next().unwrap_or(c).trim().to_ascii_lowercase()).as_deref() {
            Some("audio/mp4") | Some("audio/x-m4a") | Some("audio/m4a") => "m4a",
            Some("audio/aac") => "aac",
            Some("audio/ogg") => "ogg",
            Some("audio/opus") => "opus",
            Some("audio/wav") | Some("audio/x-wav") => "wav",
            Some("audio/flac") => "flac",
            _ => "mp3",
        },
    }
}

fn image_type(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        "image/png"
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        "image/webp"
    } else {
        "image/jpeg"
    }
}

fn check_feed_url(url: &str) -> ApiResult<String> {
    let url = url.trim();
    if url.len() > 2000 || !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(ApiError::bad_request("feed_url must be an http(s) link to an RSS feed"));
    }
    Ok(url.to_string())
}

// ------------------------------------------------------------------ manager

#[cfg(test)]
pub(crate) static SEARCH_OVERRIDE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

fn default_search_url() -> String {
    #[cfg(test)]
    if let Some(u) = SEARCH_OVERRIDE.lock().unwrap_or_else(|p| p.into_inner()).clone() {
        return u;
    }
    ITUNES_SEARCH.to_string()
}

#[derive(Clone)]
pub struct PodcastManager {
    db: Arc<Db>,
    http: reqwest::Client,
    dir: PathBuf,
    covers_dir: PathBuf,
    search_url: String,
    events: broadcast::Sender<Episode>,
    permits: Arc<Semaphore>,
    /// One feed refresh at a time (the timer and the API share it).
    refresh_lock: Arc<tokio::sync::Mutex<()>>,
}

#[derive(Deserialize)]
struct ItunesResponse {
    #[serde(default)]
    results: Vec<ItunesHit>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ItunesHit {
    collection_name: Option<String>,
    artist_name: Option<String>,
    feed_url: Option<String>,
    artwork_url600: Option<String>,
    artwork_url100: Option<String>,
}

impl PodcastManager {
    pub fn new(db: Arc<Db>, http: reqwest::Client, dir: PathBuf, covers_dir: PathBuf) -> Self {
        let (events, _) = broadcast::channel(256);
        Self {
            db,
            http,
            dir,
            covers_dir,
            search_url: default_search_url(),
            events,
            permits: Arc::new(Semaphore::new(PARALLEL)),
            refresh_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    pub fn subscribe_events(&self) -> broadcast::Receiver<Episode> {
        self.events.subscribe()
    }

    fn emit(&self, id: i64) {
        if let Ok(Some(ep)) = self.db.episode(id) {
            let _ = self.events.send(ep);
        }
    }

    /// Episodes cut off by a crash or shutdown start over: their state goes
    /// back to "not downloaded" and half-written `.part` files are removed.
    pub fn recover(&self) -> rusqlite::Result<()> {
        let n = self.db.reset_interrupted_episodes()?;
        if n > 0 {
            tracing::info!("{n} unfinished podcast download(s) from the last run reset");
        }
        if let Ok(dirs) = std::fs::read_dir(&self.dir) {
            for d in dirs.flatten().filter(|d| d.path().is_dir()) {
                if let Ok(files) = std::fs::read_dir(d.path()) {
                    for f in files.flatten().filter(|f| f.path().extension().is_some_and(|e| e == "part")) {
                        let _ = std::fs::remove_file(f.path());
                    }
                }
            }
        }
        Ok(())
    }

    // ---- network helpers

    async fn fetch(&self, url: &str, max: usize, what: &str) -> ApiResult<Vec<u8>> {
        let res = self
            .http
            .get(url)
            .timeout(Duration::from_secs(60))
            .send()
            .await
            .map_err(|e| ApiError::Unavailable(format!("could not reach the {what}: {e}")))?;
        if !res.status().is_success() {
            return Err(ApiError::Unavailable(format!("the {what} answered {}", res.status())));
        }
        if res.content_length().is_some_and(|n| n as usize > max) {
            return Err(ApiError::Unavailable(format!("the {what} is too large")));
        }
        let mut body = Vec::new();
        let mut stream = res.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| ApiError::Unavailable(format!("reading the {what} failed: {e}")))?;
            if body.len() + chunk.len() > max {
                return Err(ApiError::Unavailable(format!("the {what} is too large")));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }

    async fn fetch_feed(&self, url: &str) -> ApiResult<Feed> {
        let body = self.fetch(url, FEED_MAX_BYTES, "feed").await?;
        // Parsing a big feed is CPU work; keep it off the async workers.
        tokio::task::spawn_blocking(move || parse_feed(&body))
            .await
            .map_err(|e| ApiError::internal(e.to_string()))?
            .map_err(|e| ApiError::bad_request(format!("{e}")))
    }

    // ---- search / subscribe

    /// Search podcasts on iTunes (no key needed); every hit carries its feed URL.
    pub async fn search(&self, term: &str) -> ApiResult<Vec<SearchHit>> {
        let res = self
            .http
            .get(&self.search_url)
            .query(&[("media", "podcast"), ("entity", "podcast"), ("limit", "25"), ("term", term)])
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .map_err(|e| ApiError::Unavailable(format!("podcast search is not reachable: {e}")))?;
        if !res.status().is_success() {
            return Err(ApiError::Unavailable(format!("podcast search answered {}", res.status())));
        }
        let found: ItunesResponse = res.json().await.map_err(|e| ApiError::Unavailable(format!("bad podcast search answer: {e}")))?;
        let mut hits = Vec::new();
        for h in found.results {
            let Some(feed_url) = h.feed_url.filter(|u| u.starts_with("http")) else { continue };
            let subscribed = self.db.podcast_by_feed(&feed_url)?.is_some_and(|p| p.subscribed);
            hits.push(SearchHit {
                title: h.collection_name.unwrap_or_default(),
                author: h.artist_name.unwrap_or_default(),
                image_url: h.artwork_url600.or(h.artwork_url100).unwrap_or_default(),
                feed_url,
                subscribed,
            });
        }
        Ok(hits)
    }

    /// Subscribe to a feed (a search hit or a pasted RSS link). Episodes that
    /// exist now are listed but never auto-downloaded; only ones published
    /// after this moment are. `image_url` (e.g. the smaller iTunes artwork)
    /// is used instead of the feed's own image when given.
    pub async fn subscribe(&self, feed_url: &str, image_url: Option<&str>) -> ApiResult<Podcast> {
        let url = check_feed_url(feed_url)?;
        let feed = self.fetch_feed(&url).await?;
        let image = image_url.map(str::trim).filter(|u| u.starts_with("http")).unwrap_or(&feed.image_url);
        let now = now_ms();
        let id = match self.db.podcast_by_feed(&url)? {
            Some(p) => {
                self.db.resubscribe_podcast(p.id, now, &feed.title, &feed.author, image)?;
                p.id
            }
            None => self.db.insert_podcast(&url, &feed.title, &feed.author, image, now)?,
        };
        self.db.upsert_episodes(id, &feed.episodes)?;
        self.db.set_podcast_checked(id, now, None)?;
        self.db.podcast(id)?.ok_or_else(|| ApiError::internal("podcast vanished"))
    }

    /// Keep the podcast, its episodes, positions and downloaded files, but
    /// stop checking its feed and downloading new episodes.
    pub fn unsubscribe(&self, id: i64) -> ApiResult<()> {
        if self.db.unsubscribe_podcast(id)? {
            Ok(())
        } else {
            Err(ApiError::not_found("podcast not found"))
        }
    }

    // ---- feed refresh

    /// Fetch the feed, add new episodes and queue the ones that came out after
    /// subscribing. Returns the number of new episodes.
    pub async fn refresh(&self, id: i64) -> ApiResult<usize> {
        let _guard = self.refresh_lock.lock().await;
        self.refresh_locked(id).await
    }

    async fn refresh_locked(&self, id: i64) -> ApiResult<usize> {
        let podcast = self.db.podcast(id)?.ok_or_else(|| ApiError::not_found("podcast not found"))?;
        let feed = match self.fetch_feed(&podcast.feed_url).await {
            Ok(f) => f,
            Err(e) => {
                self.db.set_podcast_checked(id, now_ms(), Some(&e.to_string()))?;
                return Err(e);
            }
        };
        let new = self.db.upsert_episodes(id, &feed.episodes)?;
        if (!feed.title.is_empty() && feed.title != podcast.title)
            || (!feed.author.is_empty() && feed.author != podcast.author)
            || (!feed.image_url.is_empty() && feed.image_url != podcast.image_url)
        {
            // The image the user subscribed with (iTunes artwork) stays.
            let image = if podcast.image_url.is_empty() { feed.image_url.as_str() } else { podcast.image_url.as_str() };
            self.db.update_podcast_meta(id, &feed.title, &feed.author, image)?;
        }
        self.db.set_podcast_checked(id, now_ms(), None)?;
        if podcast.subscribed {
            self.queue_new(id);
        }
        Ok(new)
    }

    /// Refresh every subscribed podcast, one after the other. A broken feed
    /// does not stop the rest.
    pub async fn refresh_all(&self) {
        let _guard = self.refresh_lock.lock().await;
        let podcasts = match self.db.subscribed_podcasts() {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("podcast check: {e}");
                return;
            }
        };
        for p in podcasts {
            match self.refresh_locked(p.id).await {
                Ok(n) if n > 0 => tracing::info!("podcast \"{}\": {n} new episode(s)", p.title),
                Ok(_) => {}
                Err(e) => tracing::warn!("podcast \"{}\": {e}", p.title),
            }
        }
    }

    /// The feed check: once shortly after start, then every [`CHECK_EVERY`].
    /// Sleeps in between; nothing else is scheduled.
    pub async fn check_loop(self) {
        tokio::time::sleep(FIRST_CHECK).await;
        loop {
            self.refresh_all().await;
            tokio::time::sleep(CHECK_EVERY).await;
        }
    }

    /// Queue downloads of episodes published after subscribing that are not
    /// downloaded yet (and were not deleted by hand).
    fn queue_new(&self, podcast_id: i64) {
        let ids = match self.db.auto_download_candidates(podcast_id) {
            Ok(ids) => ids,
            Err(e) => {
                tracing::warn!("podcast auto-download: {e}");
                return;
            }
        };
        for id in ids {
            if let Err(e) = self.download(id) {
                tracing::warn!("podcast auto-download of episode {id}: {e}");
            }
        }
    }

    // ---- downloads

    /// Queue one episode for download (any episode, also old ones). Returns
    /// right away; progress arrives as events.
    pub fn download(&self, id: i64) -> ApiResult<Episode> {
        let ep = self.db.episode(id)?.ok_or_else(|| ApiError::not_found("episode not found"))?;
        if ep.status == "done" {
            if ep.file_path.as_deref().is_some_and(|p| Path::new(p).exists()) {
                return Ok(ep);
            }
            // The file was removed behind our back: download it again.
            self.db.set_episode_state(id, "none", 0.0, None)?;
        }
        if !self.db.claim_episode(id)? {
            // already queued / downloading
            return Ok(ep);
        }
        self.emit(id);
        let me = self.clone();
        tokio::spawn(async move {
            let Ok(_permit) = me.permits.clone().acquire_owned().await else { return };
            let result = me.run_download(id).await;
            if let Err(e) = result {
                tracing::warn!("podcast episode {id}: download failed: {e}");
                let _ = me.db.set_episode_state(id, "failed", 0.0, Some(&e));
                me.emit(id);
            }
        });
        self.db.episode(id)?.ok_or_else(|| ApiError::not_found("episode not found"))
    }

    async fn run_download(&self, id: i64) -> Result<(), String> {
        let ep = self.db.episode(id).map_err(|e| e.to_string())?.ok_or("episode not found")?;
        let podcast = self.db.podcast(ep.podcast_id).map_err(|e| e.to_string())?.ok_or("podcast not found")?;
        self.db.set_episode_state(id, "downloading", 0.0, None).map_err(|e| e.to_string())?;
        self.emit(id);

        let res = self.http.get(&ep.enclosure_url).send().await.map_err(|e| format!("could not reach the episode file: {e}"))?;
        if !res.status().is_success() {
            return Err(format!("the episode file answered {}", res.status()));
        }
        let content_type = res.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).map(str::to_string);
        if content_type.as_deref().is_some_and(|c| c.starts_with("text/")) {
            return Err("the link is a web page, not an audio file".into());
        }
        let total = res.content_length().filter(|n| *n > 0);

        let folder = self.dir.join(safe_name(&podcast.title));
        tokio::fs::create_dir_all(&folder).await.map_err(|e| format!("cannot create {}: {e}", folder.display()))?;
        let (y, m, d) = civil_from_days(ep.published_at.max(0) / 86_400_000);
        let stem = format!("{y:04}-{m:02}-{d:02} {}-{id}", safe_name(&ep.title));
        let file = folder.join(format!("{stem}.{}", audio_ext(&ep.enclosure_url, content_type.as_deref())));
        let part = folder.join(format!("{stem}.part"));

        let outcome = self.stream_to(id, res, &part, total).await;
        if let Err(e) = outcome {
            let _ = tokio::fs::remove_file(&part).await;
            return Err(e);
        }
        tokio::fs::rename(&part, &file).await.map_err(|e| format!("cannot save the episode: {e}"))?;
        self.db.set_episode_done(id, &file.to_string_lossy()).map_err(|e| e.to_string())?;
        self.emit(id);
        Ok(())
    }

    async fn stream_to(&self, id: i64, res: reqwest::Response, part: &Path, total: Option<u64>) -> Result<(), String> {
        let mut out = tokio::fs::File::create(part).await.map_err(|e| format!("cannot write {}: {e}", part.display()))?;
        let mut stream = res.bytes_stream();
        let mut got: u64 = 0;
        let mut last = Instant::now();
        loop {
            // A stalled connection fails instead of hanging forever.
            let chunk = match tokio::time::timeout(Duration::from_secs(60), stream.next()).await {
                Err(_) => return Err("the download stalled".into()),
                Ok(None) => break,
                Ok(Some(c)) => c.map_err(|e| format!("download interrupted: {e}"))?,
            };
            out.write_all(&chunk).await.map_err(|e| format!("cannot write the episode: {e}"))?;
            got += chunk.len() as u64;
            if last.elapsed() >= PROGRESS_EVERY {
                last = Instant::now();
                if let Some(total) = total {
                    let pct = (got as f64 / total as f64 * 100.0).min(99.0);
                    let _ = self.db.set_episode_state(id, "downloading", pct, None);
                    self.emit(id);
                }
            }
        }
        out.flush().await.map_err(|e| e.to_string())?;
        if got == 0 {
            return Err("the episode file was empty".into());
        }
        if total.is_some_and(|t| got < t) {
            return Err("download incomplete".into());
        }
        Ok(())
    }

    /// Delete the downloaded file of an episode, only if the app put it into
    /// its podcasts folder. The episode stays listed and is not downloaded
    /// automatically again.
    pub async fn delete_file(&self, id: i64) -> ApiResult<Episode> {
        let ep = self.db.episode(id)?.ok_or_else(|| ApiError::not_found("episode not found"))?;
        if ep.status == "queued" || ep.status == "downloading" {
            return Err(ApiError::bad_request("this episode is still downloading"));
        }
        if let Some(path) = ep.file_path.as_deref() {
            let root = tokio::fs::canonicalize(&self.dir).await.unwrap_or_else(|_| self.dir.clone());
            match tokio::fs::canonicalize(path).await {
                Ok(real) if real.starts_with(&root) => tokio::fs::remove_file(&real).await?,
                Ok(_) => return Err(ApiError::bad_request("this file is not in the podcasts folder; not touching it")),
                Err(_) => {} // already gone
            }
        }
        self.db.clear_episode_file(id)?;
        self.emit(id);
        self.db.episode(id)?.ok_or_else(|| ApiError::not_found("episode not found"))
    }

    // ---- listening position

    /// Save the listening position. From [`PLAYED_FRACTION`] of the episode
    /// on it counts as played and the position resets to the start.
    pub fn save_position(&self, id: i64, position: f64, duration: Option<f64>) -> ApiResult<Episode> {
        let ep = self.db.episode(id)?.ok_or_else(|| ApiError::not_found("episode not found"))?;
        let position = if position.is_finite() { position.max(0.0) } else { 0.0 };
        let known = duration.filter(|d| d.is_finite() && *d > 0.0).unwrap_or(ep.duration_sec as f64);
        let finished = known > 0.0 && position / known >= PLAYED_FRACTION;
        self.db.set_episode_position(
            id,
            if finished { 0.0 } else { position },
            finished,
            duration.filter(|d| d.is_finite() && *d > 0.0).map(|d| d as i64),
        )?;
        self.db.episode(id)?.ok_or_else(|| ApiError::not_found("episode not found"))
    }

    // ---- covers

    /// Path of the cached cover of a podcast, fetched on first use.
    pub async fn cover(&self, id: i64) -> ApiResult<(PathBuf, &'static str)> {
        let podcast = self.db.podcast(id)?.ok_or_else(|| ApiError::not_found("podcast not found"))?;
        let path = self.covers_dir.join(format!("podcast-{id}.img"));
        let cached = tokio::fs::read(&path).await.ok().filter(|b| !b.is_empty());
        let bytes = match cached {
            Some(b) => b,
            None => {
                if podcast.image_url.is_empty() {
                    return Err(ApiError::not_found("this podcast has no cover"));
                }
                let b = self.fetch(&podcast.image_url, COVER_MAX_BYTES, "cover").await?;
                if b.is_empty() {
                    return Err(ApiError::not_found("this podcast has no cover"));
                }
                tokio::fs::write(&path, &b).await?;
                b
            }
        };
        Ok((path, image_type(&bytes)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FEED: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:itunes="http://www.itunes.com/dtds/podcast-1.0.dtd">
<channel>
  <title>Test &amp; Co</title>
  <itunes:author>Some Author</itunes:author>
  <image><url>https://example.com/plain.jpg</url><title>ignored</title></image>
  <itunes:image href="https://example.com/cover.jpg"/>
  <item>
    <title><![CDATA[Episode Two]]></title>
    <guid isPermaLink="false">g2</guid>
    <pubDate>Tue, 10 Jun 2025 14:00:00 +0200</pubDate>
    <itunes:duration>1:02:03</itunes:duration>
    <enclosure url="https://example.com/ep2.mp3?x=1" length="123" type="audio/mpeg"/>
  </item>
  <item>
    <title>Episode One</title>
    <guid>g1</guid>
    <pubDate>Mon, 09 Jun 2025 12:00:00 GMT</pubDate>
    <itunes:duration>754</itunes:duration>
    <enclosure url="https://example.com/ep1.m4a" type="audio/mp4"/>
  </item>
  <item><title>No audio</title><guid>g0</guid></item>
</channel></rss>"#;

    #[test]
    fn parses_channel_and_episodes() {
        let f = parse_feed(FEED.as_bytes()).unwrap();
        assert_eq!(f.title, "Test & Co");
        assert_eq!(f.author, "Some Author");
        assert_eq!(f.image_url, "https://example.com/cover.jpg");
        assert_eq!(f.episodes.len(), 2, "items without an enclosure are skipped");
        assert_eq!(f.episodes[0].title, "Episode Two");
        assert_eq!(f.episodes[0].guid, "g2");
        assert_eq!(f.episodes[0].duration_sec, 3723);
        assert_eq!(f.episodes[0].enclosure_url, "https://example.com/ep2.mp3?x=1");
        assert_eq!(f.episodes[0].published_at, 1_749_556_800_000); // 2025-06-10 12:00:00 UTC
        assert_eq!(f.episodes[1].duration_sec, 754);
        assert_eq!(f.episodes[1].published_at, 1_749_470_400_000);
    }

    #[test]
    fn image_url_falls_back_to_plain_image_and_html_is_rejected() {
        let f = parse_feed(b"<rss><channel><title>T</title><image><url>https://e.com/a.png</url></image></channel></rss>").unwrap();
        assert_eq!(f.image_url, "https://e.com/a.png");
        assert!(f.episodes.is_empty());
        assert!(parse_feed(b"<html><body>nope</body></html>").is_err());
        assert!(parse_feed(b"").is_err());
    }

    #[test]
    fn broken_tail_keeps_the_episodes_before_it() {
        let cut = &FEED[..FEED.find("Episode One").unwrap() - 20];
        let f = parse_feed(cut.as_bytes()).unwrap();
        assert_eq!(f.episodes.len(), 1);
    }

    #[test]
    fn dates_and_durations() {
        assert_eq!(parse_date("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        assert_eq!(parse_date("Tue, 10 Jun 2025 14:00:00 +0200"), Some(1_749_556_800_000));
        assert_eq!(parse_date("2025-06-10T12:00:00Z"), Some(1_749_556_800_000));
        assert_eq!(parse_date("2025-06-10T14:00:00+02:00"), Some(1_749_556_800_000));
        assert_eq!(parse_date("10 Jun 2025 12:00 EST"), Some(1_749_556_800_000 + 5 * 3600 * 1000));
        assert_eq!(parse_date("garbage"), None);
        assert_eq!(civil_from_days(days_from_civil(2025, 6, 10)), (2025, 6, 10));
        assert_eq!(parse_duration("1:02:03"), 3723);
        assert_eq!(parse_duration("62:03"), 3723);
        assert_eq!(parse_duration("3723.4"), 3723);
        assert_eq!(parse_duration("soon"), 0);
        assert_eq!(parse_duration(""), 0);
    }

    #[test]
    fn file_names_are_safe() {
        assert_eq!(safe_name("../../etc/passwd"), "etc passwd");
        assert_eq!(safe_name("AC/DC: Live?"), "AC DC Live");
        assert_eq!(safe_name("???"), "podcast");
        assert_eq!(audio_ext("https://x/y/ep.M4A?token=1", None), "m4a");
        assert_eq!(audio_ext("https://x/download", Some("audio/ogg; charset=x")), "ogg");
        assert_eq!(audio_ext("https://x/download", None), "mp3");
    }

    // ---- the whole thing through the HTTP API, against a local fixture server

    use axum::body::Body;
    use axum::routing::get;
    use serde_json::{json, Value};
    use std::future::IntoFuture;
    use std::sync::{Arc, Mutex};

    use crate::provider::mock::MockProvider;
    use crate::ServerConfig;

    const AUDIO_LEN: usize = 6 * 4096;

    /// Feed, audio, cover and an iTunes-like search answer on a random local port.
    struct Fixture {
        base: String,
        feed: Arc<Mutex<String>>,
    }

    fn iso(ms: i64) -> String {
        let secs = ms / 1000;
        let (y, m, d) = civil_from_days(secs / 86400);
        let rem = secs % 86400;
        format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem / 60 % 60, rem % 60)
    }

    fn item(guid: &str, title: &str, date: &str, base: &str) -> String {
        format!(
            "<item><title>{title}</title><guid>{guid}</guid><pubDate>{date}</pubDate><itunes:duration>1000</itunes:duration>\
             <enclosure url=\"{base}/audio/{guid}.mp3\" length=\"{AUDIO_LEN}\" type=\"audio/mpeg\"/></item>"
        )
    }

    fn feed_xml(items: &[String]) -> String {
        format!(
            "<?xml version=\"1.0\"?><rss version=\"2.0\" xmlns:itunes=\"http://www.itunes.com/dtds/podcast-1.0.dtd\"><channel>\
             <title>Fixture Cast</title><itunes:author>Fixture Author</itunes:author>{}</channel></rss>",
            items.concat()
        )
    }

    async fn fixture() -> Fixture {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let feed = Arc::new(Mutex::new(String::new()));
        let (f, b) = (feed.clone(), base.clone());
        let app = axum::Router::new()
            .route("/feed.xml", get(move || async move { ([("content-type", "application/rss+xml")], f.lock().unwrap().clone()) }))
            .route("/page.html", get(|| async { ([("content-type", "text/html")], "<html>not a feed</html>") }))
            .route(
                "/audio/:name",
                get(|| async {
                    // slow body, so progress events exist
                    let body = futures_util::stream::unfold(0, |i| async move {
                        if i >= 6 {
                            return None;
                        }
                        tokio::time::sleep(Duration::from_millis(150)).await;
                        Some((Ok::<_, std::io::Error>(axum::body::Bytes::from(vec![7u8; 4096])), i + 1))
                    });
                    ([("content-type", "audio/mpeg"), ("content-length", "24576")], Body::from_stream(body))
                }),
            )
            .route("/cover.png", get(|| async { ([("content-type", "image/png")], vec![0x89, b'P', b'N', b'G', 1, 2, 3]) }))
            .route(
                "/search",
                get(move || {
                    let b = b.clone();
                    async move {
                        axum::Json(json!({"resultCount": 2, "results": [
                            {"collectionName": "Fixture Cast", "artistName": "Fixture Author", "feedUrl": format!("{b}/feed.xml"),
                             "artworkUrl600": format!("{b}/cover.png")},
                            {"collectionName": "No feed", "artistName": "x"}
                        ]}))
                    }
                }),
            );
        tokio::spawn(axum::serve(listener, app).into_future());
        Fixture { base, feed }
    }

    async fn wait_for<F: Fn(&Value) -> bool>(http: &reqwest::Client, url: &str, ok: F) -> Value {
        for _ in 0..200 {
            let v: Value = http.get(url).send().await.unwrap().json().await.unwrap();
            if ok(&v) {
                return v;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("condition never met for {url}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn search_subscribe_refresh_download_position_flow() {
        let fx = fixture().await;
        let now = now_ms();
        *fx.feed.lock().unwrap() = feed_xml(&[
            item("old2", "Old Two", &iso(now - 2 * 86_400_000), &fx.base),
            item("old1", "Old One", &iso(now - 9 * 86_400_000), &fx.base),
        ]);
        *SEARCH_OVERRIDE.lock().unwrap() = Some(format!("{}/search", fx.base));

        let dir = std::env::temp_dir().join(format!("geonix-lite-pod-{}", uuid::Uuid::new_v4()));
        let cfg = ServerConfig {
            port: 0,
            sync_port: None,
            data_dir: dir.join("data"),
            music_dir: dir.join("music"),
            after_downloads: None,
            daily_recommendations: false,
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api = format!("http://{}/api/podcasts", listener.local_addr().unwrap());
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(crate::serve(cfg.clone(), listener, MockProvider::offline(), async {
            let _ = stopped.await;
        }));
        let http = reqwest::Client::new();
        let json_post = |url: String, body: Value| http.post(url).json(&body).send();

        // search: only hits with a feed link, not subscribed yet
        let hits: Value = http.get(format!("{api}/search?q=fixture")).send().await.unwrap().json().await.unwrap();
        assert_eq!(hits.as_array().unwrap().len(), 1);
        assert_eq!(hits[0]["title"], "Fixture Cast");
        assert_eq!(hits[0]["subscribed"], false);
        let feed_url = hits[0]["feed_url"].as_str().unwrap().to_string();
        assert_eq!(http.get(format!("{api}/search?q=")).send().await.unwrap().status(), 400);

        // bad feeds are refused
        let bad = json_post(api.clone(), json!({"feed_url": format!("{}/page.html", fx.base)})).await.unwrap();
        assert_eq!(bad.status(), 400);
        let bad = json_post(api.clone(), json!({"feed_url": "ftp://example.com/feed"})).await.unwrap();
        assert_eq!(bad.status(), 400);
        // changes need a JSON content type
        let res = http.post(&api).body("{}").send().await.unwrap();
        assert_eq!(res.status(), 415);

        // subscribe: existing episodes are listed, none is downloaded
        let res = json_post(api.clone(), json!({"feed_url": feed_url, "image_url": format!("{}/cover.png", fx.base)})).await.unwrap();
        assert_eq!(res.status(), 201);
        let podcast: Value = res.json().await.unwrap();
        let pid = podcast["id"].as_i64().unwrap();
        assert_eq!(podcast["title"], "Fixture Cast");
        assert_eq!(podcast["author"], "Fixture Author");
        assert_eq!(podcast["episode_count"], 2);
        let list: Value = http.get(&api).send().await.unwrap().json().await.unwrap();
        assert_eq!(list.as_array().unwrap().len(), 1);
        let hits: Value = http.get(format!("{api}/search?q=fixture")).send().await.unwrap().json().await.unwrap();
        assert_eq!(hits[0]["subscribed"], true);
        // subscribing twice keeps one podcast
        json_post(api.clone(), json!({"feed_url": feed_url})).await.unwrap();
        assert_eq!(http.get(&api).send().await.unwrap().json::<Value>().await.unwrap().as_array().unwrap().len(), 1);

        let eps: Value = http.get(format!("{api}/{pid}/episodes")).send().await.unwrap().json().await.unwrap();
        assert_eq!(eps.as_array().unwrap().len(), 2);
        assert_eq!(eps[0]["title"], "Old Two", "newest first");
        assert!(eps.as_array().unwrap().iter().all(|e| e["status"] == "none" && e["file_path"].is_null()));
        let old2 = eps[0]["id"].as_i64().unwrap();

        // the cover is fetched once and served
        let cover = http.get(format!("{api}/{pid}/cover")).send().await.unwrap();
        assert_eq!(cover.status(), 200);
        assert_eq!(cover.headers()["content-type"], "image/png");

        // a refresh without news finds nothing and downloads nothing
        let r: Value = json_post(format!("{api}/{pid}/refresh"), json!({})).await.unwrap().json().await.unwrap();
        assert_eq!(r["new"], 0);

        // a new episode appears: it is detected and downloaded by itself
        *fx.feed.lock().unwrap() = feed_xml(&[
            item("new1", "Brand New", &iso(now_ms() + 3_600_000), &fx.base),
            item("old2", "Old Two", &iso(now - 2 * 86_400_000), &fx.base),
            item("old1", "Old One", &iso(now - 9 * 86_400_000), &fx.base),
        ]);
        let r: Value = json_post(format!("{api}/{pid}/refresh"), json!({})).await.unwrap().json().await.unwrap();
        assert_eq!(r["new"], 1);
        let eps = wait_for(&http, &format!("{api}/{pid}/episodes"), |v| v[0]["status"] == "done").await;
        assert_eq!(eps[0]["title"], "Brand New");
        let file = std::path::PathBuf::from(eps[0]["file_path"].as_str().unwrap());
        assert_eq!(std::fs::metadata(&file).unwrap().len() as usize, AUDIO_LEN);
        assert!(file.starts_with(cfg.data_dir.join("podcasts")), "{file:?}");
        assert!(!file.starts_with(&cfg.music_dir));
        assert_eq!(eps[1]["status"], "none", "old episodes are not downloaded automatically");
        assert_eq!(eps[2]["status"], "none");
        assert_eq!(std::fs::read_dir(file.parent().unwrap()).unwrap().count(), 1, "no .part files left");

        // manual download of an old episode reports progress on the event stream
        let events = Arc::new(Mutex::new(String::new()));
        let sink = events.clone();
        let stream_url = format!("{api}/events");
        let client = http.clone();
        let reader = tokio::spawn(async move {
            let res = client.get(stream_url).send().await.unwrap();
            let mut body = res.bytes_stream();
            while let Some(Ok(chunk)) = body.next().await {
                sink.lock().unwrap().push_str(&String::from_utf8_lossy(&chunk));
            }
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        let res = http.post(format!("{api}/episodes/{old2}/download")).header("content-type", "application/json").send().await.unwrap();
        assert_eq!(res.status(), 202);
        wait_for(&http, &format!("{api}/{pid}/episodes"), |v| v[1]["status"] == "done").await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        reader.abort();
        let seen = events.lock().unwrap().clone();
        assert!(seen.contains("event: episode"), "{seen}");
        assert!(seen.contains("\"status\":\"queued\""), "{seen}");
        let pcts: Vec<f64> = seen
            .lines()
            .filter(|l| l.contains("\"status\":\"downloading\""))
            .filter_map(|l| serde_json::from_str::<Value>(l.trim_start_matches("data:").trim()).ok())
            .filter_map(|v| v["progress"].as_f64())
            .collect();
        assert!(pcts.iter().any(|p| *p > 0.0 && *p < 100.0), "progress events: {pcts:?}");
        assert!(seen.contains("\"status\":\"done\""), "{seen}");

        // listening position: saved, resumable, and "played" from 95 %
        let put = |id: i64, body: Value| http.put(format!("{api}/episodes/{id}/position")).json(&body).send();
        let e: Value = put(old2, json!({"position_sec": 250.0, "duration_sec": 1000.0})).await.unwrap().json().await.unwrap();
        assert_eq!(e["position_sec"], 250.0);
        assert_eq!(e["played"], false);
        let eps: Value = http.get(format!("{api}/{pid}/episodes")).send().await.unwrap().json().await.unwrap();
        assert_eq!(eps[1]["position_sec"], 250.0, "position survives a reload");
        let e: Value = put(old2, json!({"position_sec": 940.0, "duration_sec": 1000.0})).await.unwrap().json().await.unwrap();
        assert_eq!(e["played"], false, "94 % is not finished yet");
        let e: Value = put(old2, json!({"position_sec": 960.0, "duration_sec": 1000.0})).await.unwrap().json().await.unwrap();
        assert_eq!(e["played"], true);
        assert_eq!(e["position_sec"], 0.0, "a finished episode starts over");
        assert_eq!(put(999_999, json!({"position_sec": 1.0})).await.unwrap().status(), 404);

        // deleting the downloaded file: gone from disk, listed, never auto-downloaded again
        let old2_file = std::path::PathBuf::from(eps[1]["file_path"].as_str().unwrap());
        assert!(old2_file.exists());
        let res = http.delete(format!("{api}/episodes/{old2}/file")).header("content-type", "application/json").send().await.unwrap();
        assert_eq!(res.status(), 200);
        let e: Value = res.json().await.unwrap();
        assert_eq!(e["status"], "none");
        assert!(!old2_file.exists());
        assert!(file.exists(), "other episodes are untouched");
        json_post(format!("{api}/{pid}/refresh"), json!({})).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        let eps: Value = http.get(format!("{api}/{pid}/episodes")).send().await.unwrap().json().await.unwrap();
        assert_eq!(eps[1]["status"], "none");

        // unsubscribe: out of the list, files and episodes stay
        let res = http.delete(format!("{api}/{pid}")).header("content-type", "application/json").send().await.unwrap();
        assert_eq!(res.status(), 200);
        assert_eq!(http.get(&api).send().await.unwrap().json::<Value>().await.unwrap().as_array().unwrap().len(), 0);
        assert!(file.exists());
        assert_eq!(http.get(format!("{api}/{pid}/episodes")).send().await.unwrap().status(), 200);
        let hits: Value = http.get(format!("{api}/search?q=fixture")).send().await.unwrap().json().await.unwrap();
        assert_eq!(hits[0]["subscribed"], false);

        stop.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), server).await.unwrap().unwrap().unwrap();
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn delete_file_refuses_files_outside_the_podcasts_folder() {
        let dir = std::env::temp_dir().join(format!("geonix-lite-pod-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("podcasts")).unwrap();
        let foreign = dir.join("song.mp3");
        std::fs::write(&foreign, b"x").unwrap();
        let db = Arc::new(Db::open_in_memory().unwrap());
        let m = PodcastManager::new(db.clone(), reqwest::Client::new(), dir.join("podcasts"), dir.join("covers"));
        let pid = db.insert_podcast("http://x/feed", "P", "", "", 1).unwrap();
        db.upsert_episodes(
            pid,
            &[FeedEpisode { guid: "g".into(), title: "E".into(), enclosure_url: "http://x/e.mp3".into(), ..Default::default() }],
        )
        .unwrap();
        let id = db.episodes(pid, 10).unwrap()[0].id;
        db.set_episode_done(id, &foreign.to_string_lossy()).unwrap();
        assert!(m.delete_file(id).await.is_err());
        assert!(foreign.exists(), "a file outside the podcasts folder must never be deleted");
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn unfinished_downloads_reset_on_start_and_part_files_go() {
        let dir = std::env::temp_dir().join(format!("geonix-lite-pod-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("podcasts").join("Show")).unwrap();
        let part = dir.join("podcasts").join("Show").join("half.part");
        let keep = dir.join("podcasts").join("Show").join("done.mp3");
        std::fs::write(&part, b"x").unwrap();
        std::fs::write(&keep, b"x").unwrap();
        let db = Arc::new(Db::open_in_memory().unwrap());
        let m = PodcastManager::new(db.clone(), reqwest::Client::new(), dir.join("podcasts"), dir.join("covers"));
        let pid = db.insert_podcast("http://x/feed", "P", "", "", 1).unwrap();
        db.upsert_episodes(
            pid,
            &[FeedEpisode { guid: "g".into(), title: "E".into(), enclosure_url: "http://x/e.mp3".into(), ..Default::default() }],
        )
        .unwrap();
        let id = db.episodes(pid, 10).unwrap()[0].id;
        db.set_episode_state(id, "downloading", 40.0, None).unwrap();
        m.recover().unwrap();
        assert_eq!(db.episode(id).unwrap().unwrap().status, "none");
        assert!(!part.exists());
        assert!(keep.exists());
        std::fs::remove_dir_all(dir).ok();
    }
}
