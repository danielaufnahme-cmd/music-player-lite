//! geonix-yt: the Rust replacement of the Python worker (worker/*.py).
//!
//! Same command surface and JSON in/out as `worker.py`:
//! `search`, `playlist_info`, `download`, `recommend`, `song_info`,
//! `stream_url`, `lyrics`, `ping`. Errors are the worker's friendly messages.
//!
//! YouTube Music is queried through the InnerTube WEB_REMIX API directly (what
//! ytmusicapi does); the external `yt-dlp` binary is only spawned for downloads,
//! stream URLs and rare fallbacks, and exits afterwards.

pub mod downloader;
pub mod innertube;
pub mod lyrics;
pub mod net;
pub mod recommender;
pub mod tools;
pub mod util;
pub mod ytm;

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{json, Value};

pub use net::{Client, Error, HttpTransport, Transport};

/// Configuration of [`Yt`].
#[derive(Debug, Clone, Default)]
pub struct YtConfig {
    /// Default download folder, used when a `download` request has no `dest_dir`.
    pub music_dir: Option<PathBuf>,
    /// yt-dlp binary. None = locate (GEONIX_YTDLP, PATH, next to the executable).
    pub ytdlp: Option<PathBuf>,
    /// ffmpeg binary. None = locate (GEONIX_FFMPEG, PATH, next to the executable).
    pub ffmpeg: Option<PathBuf>,
}

impl YtConfig {
    /// Config with binaries located from env/PATH and the given music folder.
    pub fn from_env(music_dir: Option<PathBuf>) -> Self {
        Self { music_dir, ytdlp: None, ffmpeg: None }
    }
}

pub struct Yt {
    transport: Arc<dyn Transport>,
    music_dir: Option<PathBuf>,
    ytdlp: Option<PathBuf>,
    ffmpeg: Option<PathBuf>,
}

impl Yt {
    pub fn new(cfg: YtConfig) -> Self {
        Self::with_transport(cfg, Arc::new(HttpTransport::new()))
    }

    /// Use a custom transport (tests replay recorded responses with this).
    pub fn with_transport(cfg: YtConfig, transport: Arc<dyn Transport>) -> Self {
        let ytdlp = cfg.ytdlp.or_else(|| tools::locate("GEONIX_YTDLP", "yt-dlp"));
        let ffmpeg = cfg.ffmpeg.or_else(|| tools::locate("GEONIX_FFMPEG", "ffmpeg"));
        Self { transport, music_dir: cfg.music_dir, ytdlp, ffmpeg }
    }

    pub fn ytdlp_path(&self) -> Option<&std::path::Path> {
        self.ytdlp.as_deref()
    }

    pub fn ffmpeg_path(&self) -> Option<&std::path::Path> {
        self.ffmpeg.as_deref()
    }

    /// Run one worker command. `params` is the request object (the Python
    /// worker's request minus `id`/`cmd`, though extra keys are ignored).
    /// `progress` receives `{"percent": f64, "speed": str|null}` updates (downloads).
    pub async fn handle(&self, cmd: &str, params: Value, progress: impl Fn(Value) + Send + Sync) -> Result<Value, String> {
        self.dispatch(cmd, &params, &progress).await.map_err(|e| friendly_error(&e))
    }

    async fn dispatch(&self, cmd: &str, req: &Value, progress: &(dyn Fn(Value) + Send + Sync)) -> net::Result<Value> {
        let t: &dyn Transport = self.transport.as_ref();
        match cmd {
            "search" => {
                let limit = int_param(req, "limit", 20)?.clamp(1, 50) as usize;
                let query = require_str(req, "query")?;
                Ok(json!({"tracks": ytm::search(t, query, limit).await?}))
            }
            "playlist_info" => ytm::playlist_info(t, self.ytdlp.as_deref(), require_str(req, "url")?).await,
            "download" => {
                let meta = req.get("meta").filter(|m| m.is_object());
                let video_id = require_str(req, "video_id")?;
                let fallback_dir = self.music_dir.as_ref().map(|p| p.to_string_lossy().into_owned());
                let dest_dir = match require_str(req, "dest_dir") {
                    Ok(d) => d.to_string(),
                    Err(e) => fallback_dir.ok_or(e)?,
                };
                let covers_dir = req.get("covers_dir").and_then(Value::as_str);
                let opts = downloader::Options {
                    kbps: req.get("audio_kbps").and_then(Value::as_u64).map(|k| k as u32).filter(|k| *k > 0),
                    retry: req.get("retry").and_then(Value::as_bool).unwrap_or(true),
                };
                downloader::download_with(
                    t,
                    downloader::Tools { ytdlp: self.ytdlp.as_deref(), ffmpeg: self.ffmpeg.as_deref() },
                    video_id,
                    &dest_dir,
                    progress,
                    meta,
                    covers_dir,
                    opts,
                )
                .await
            }
            "recommend" => {
                let seeds = str_list(req, "seed_video_ids");
                let exclude = str_list(req, "exclude_video_ids");
                let limit = int_param(req, "limit", 30)?.clamp(1, 100) as usize;
                let tracks =
                    recommender::recommend(&seeds, &exclude, limit, |seed| async move { ytm::related(t, &seed, 25).await }).await?;
                Ok(json!({ "tracks": tracks }))
            }
            "song_info" => ytm::song_info(t, self.ytdlp.as_deref(), require_str(req, "video_id")?).await,
            "stream_url" => ytm::stream_url(self.ytdlp.as_deref(), require_str(req, "video_id")?).await,
            "lyrics" => {
                let video_id = require_str(req, "video_id")?;
                let title = loose_str(req.get("title"));
                let artist = loose_str(req.get("artist"));
                let duration = int_param(req, "duration_sec", 0)?;
                Ok(lyrics::lyrics(t, video_id, &title, &artist, duration).await)
            }
            "ping" => Ok(json!({"pong": true})),
            other => Err(Error::Msg(format!("unknown command: {}", py_repr(other)))),
        }
    }
}

/// The worker's user-facing error text.
pub fn friendly_error(err: &Error) -> String {
    let text = err.to_string().trim().to_string();
    let text = if text.is_empty() { "Error".to_string() } else { text };
    if let Error::Net(_) = err {
        if !text.to_lowercase().contains("video unavailable") {
            return format!("Network error: {text}");
        }
    }
    friendly_text(&text)
}

/// `friendly_error` applied to plain exception text.
pub fn friendly_text(text: &str) -> String {
    let lowered = text.to_lowercase();
    if lowered.contains("video unavailable") || lowered.contains("not available") {
        return "Video unavailable".into();
    }
    if ["urlopen error", "timed out", "name or service not known", "connection"].iter().any(|s| lowered.contains(s)) {
        return format!("Network error: {text}");
    }
    text.to_string()
}

/// `require(req, key, str)`: a non-blank string.
pub fn require_str<'a>(req: &'a Value, key: &str) -> net::Result<&'a str> {
    match req.get(key).and_then(Value::as_str) {
        Some(s) if !s.trim().is_empty() => Ok(s),
        _ => Err(Error::Msg(format!("missing or invalid '{key}'"))),
    }
}

/// `int(req.get(key) or default)`.
pub fn int_param(req: &Value, key: &str, default: i64) -> net::Result<i64> {
    let Some(v) = req.get(key).filter(|v| util::truthy(v)) else {
        return Ok(default);
    };
    match v {
        Value::Number(n) => Ok(n.as_i64().unwrap_or_else(|| n.as_f64().unwrap_or(0.0).trunc() as i64)),
        Value::Bool(b) => Ok(*b as i64),
        Value::String(s) => {
            s.trim().parse::<i64>().map_err(|_| Error::Msg(format!("invalid literal for int() with base 10: {}", py_repr(s))))
        }
        _ => Err(Error::Msg(format!("int() argument must be a string or a number, not '{}'", type_name(v)))),
    }
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
        _ => "NoneType",
    }
}

/// `str(req.get(key) or "")`.
fn loose_str(v: Option<&Value>) -> String {
    match v {
        Some(x) if util::truthy(x) => util::py_str(x),
        _ => String::new(),
    }
}

fn str_list(req: &Value, key: &str) -> Vec<String> {
    req.get(key).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default()
}

/// Python `repr()` of a string.
fn py_repr(s: &str) -> String {
    if s.contains('\'') && !s.contains('"') {
        format!("\"{s}\"")
    } else {
        format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn friendly_errors() {
        assert_eq!(friendly_error(&Error::Msg("ERROR: Video unavailable. This video is private".into())), "Video unavailable");
        assert!(friendly_error(&Error::Msg("<urlopen error timed out>".into())).starts_with("Network error"));
        assert!(friendly_error(&Error::Net("dns failure".into())).starts_with("Network error"));
        assert_eq!(friendly_error(&Error::Msg("".into())), "Error");
    }

    #[test]
    fn require_validates_fields() {
        assert_eq!(require_str(&json!({"q": "x"}), "q").unwrap(), "x");
        for bad in [json!({}), json!({"q": ""}), json!({"q": 5}), json!({"q": "  "})] {
            assert!(require_str(&bad, "q").is_err(), "{bad} should be rejected");
        }
        assert_eq!(require_str(&json!({}), "q").unwrap_err().to_string(), "missing or invalid 'q'");
    }

    #[test]
    fn int_params() {
        assert_eq!(int_param(&json!({}), "limit", 20).unwrap(), 20);
        assert_eq!(int_param(&json!({"limit": 0}), "limit", 20).unwrap(), 20);
        assert_eq!(int_param(&json!({"limit": "7"}), "limit", 20).unwrap(), 7);
        assert_eq!(int_param(&json!({"limit": 7.9}), "limit", 20).unwrap(), 7);
        assert!(int_param(&json!({"limit": "x"}), "limit", 20).is_err());
    }

    #[test]
    fn repr_like_python() {
        assert_eq!(py_repr("fly"), "'fly'");
        assert_eq!(py_repr("it's"), "\"it's\"");
    }
}
