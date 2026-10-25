//! HTTP client for the podcast endpoints of the server (`/api/podcasts...`).
//! Kept apart from api.rs; errors are plain messages for the snackbar.

use std::time::Duration;

use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct Podcast {
    pub id: i64,
    pub title: String,
    pub author: String,
    pub feed_url: String,
    pub image_url: String,
    pub subscribed: bool,
    pub last_error: Option<String>,
    pub episode_count: i64,
    pub unplayed_count: i64,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct Episode {
    pub id: i64,
    pub podcast_id: i64,
    pub title: String,
    /// ms since the epoch, 0 = unknown
    pub published_at: i64,
    pub duration_sec: i64,
    pub enclosure_url: String,
    pub file_path: Option<String>,
    /// "none", "queued", "downloading", "done", "failed"
    pub status: String,
    /// percent
    pub progress: f64,
    pub error: Option<String>,
    pub position_sec: f64,
    pub played: bool,
}

/// A podcast found by the search (not necessarily subscribed).
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct Hit {
    pub title: String,
    pub author: String,
    pub feed_url: String,
    pub image_url: String,
    pub subscribed: bool,
}

pub enum PodEvent {
    Episode(Episode),
    Resync,
}

#[derive(Clone)]
pub struct PodApi {
    pub base: String,
    http: reqwest::Client,
}

pub type PodResult<T> = Result<T, String>;

impl PodApi {
    pub fn new(base: String) -> PodApi {
        let http =
            reqwest::Client::builder().pool_max_idle_per_host(1).pool_idle_timeout(Duration::from_secs(20)).build().expect("http client");
        PodApi { base, http }
    }

    async fn send(&self, method: reqwest::Method, path: &str, query: &[(&str, &str)], body: Option<Value>) -> PodResult<Value> {
        let write = method != reqwest::Method::GET;
        let mut req = self.http.request(method, format!("{}{}", self.base, path)).query(query).timeout(Duration::from_secs(120));
        if let Some(b) = body {
            req = req.json(&b);
        } else if write {
            req = req.header("content-type", "application/json");
        }
        let res = req.send().await.map_err(|e| {
            if e.is_timeout() {
                "The server took too long to answer.".to_string()
            } else {
                format!("Can't reach the Music Player Lite server on {}. Is it running?", self.base)
            }
        })?;
        let status = res.status().as_u16();
        let bytes = res.bytes().await.map_err(|e| e.to_string())?;
        let v: Value = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap_or(Value::Null) };
        if status >= 400 {
            return Err(v.get("error").and_then(Value::as_str).map(String::from).unwrap_or_else(|| format!("Error {status}")));
        }
        Ok(v)
    }

    fn parse<T: for<'de> Deserialize<'de>>(v: Value) -> PodResult<T> {
        serde_json::from_value(v).map_err(|e| format!("Bad answer from the backend: {e}"))
    }

    pub async fn search(&self, q: &str) -> PodResult<Vec<Hit>> {
        Self::parse(self.send(reqwest::Method::GET, "/api/podcasts/search", &[("q", q)], None).await?)
    }

    pub async fn subscribed(&self) -> PodResult<Vec<Podcast>> {
        Self::parse(self.send(reqwest::Method::GET, "/api/podcasts", &[], None).await?)
    }

    pub async fn subscribe(&self, feed_url: &str, image_url: Option<&str>) -> PodResult<Podcast> {
        let body = json!({ "feed_url": feed_url, "image_url": image_url });
        Self::parse(self.send(reqwest::Method::POST, "/api/podcasts", &[], Some(body)).await?)
    }

    pub async fn unsubscribe(&self, id: i64) -> PodResult<()> {
        self.send(reqwest::Method::DELETE, &format!("/api/podcasts/{id}"), &[], None).await.map(|_| ())
    }

    pub async fn episodes(&self, id: i64) -> PodResult<Vec<Episode>> {
        Self::parse(self.send(reqwest::Method::GET, &format!("/api/podcasts/{id}/episodes"), &[("limit", "1000")], None).await?)
    }

    /// Check the feed now; the number of new episodes.
    pub async fn refresh(&self, id: i64) -> PodResult<i64> {
        let v = self.send(reqwest::Method::POST, &format!("/api/podcasts/{id}/refresh"), &[], None).await?;
        Ok(v.get("new").and_then(Value::as_i64).unwrap_or(0))
    }

    pub async fn download(&self, episode: i64) -> PodResult<Episode> {
        Self::parse(self.send(reqwest::Method::POST, &format!("/api/podcasts/episodes/{episode}/download"), &[], None).await?)
    }

    pub async fn delete_file(&self, episode: i64) -> PodResult<Episode> {
        Self::parse(self.send(reqwest::Method::DELETE, &format!("/api/podcasts/episodes/{episode}/file"), &[], None).await?)
    }

    pub async fn save_position(&self, episode: i64, position: f64, duration: f64) -> PodResult<Episode> {
        let body = json!({ "position_sec": position, "duration_sec": if duration > 0.0 { Some(duration) } else { None } });
        Self::parse(self.send(reqwest::Method::PUT, &format!("/api/podcasts/episodes/{episode}/position"), &[], Some(body)).await?)
    }

    /// Server-Sent Events of episode changes; calls `on` per event until the
    /// connection drops.
    pub async fn events(&self, mut on: impl FnMut(PodEvent)) -> PodResult<()> {
        let res = self
            .http
            .get(format!("{}/api/podcasts/events", self.base))
            .header("accept", "text/event-stream")
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !res.status().is_success() {
            return Err("event stream failed".into());
        }
        let mut stream = res.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut event = String::new();
        let mut data = String::new();
        while let Some(chunk) = stream.next().await {
            buf.extend_from_slice(&chunk.map_err(|e| e.to_string())?);
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&line);
                let line = line.trim_end_matches(['\n', '\r']);
                if line.is_empty() {
                    match event.as_str() {
                        "episode" if !data.is_empty() => {
                            if let Ok(e) = serde_json::from_str::<Episode>(&data) {
                                on(PodEvent::Episode(e));
                            }
                        }
                        "resync" => on(PodEvent::Resync),
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

/// Cover bytes for a cover key: `podcast:<id>[:<episode>]` is the podcast's
/// cover cached by the server, a link is fetched as it is (search results).
pub async fn fetch_cover(base: &str, key: &str) -> Option<Vec<u8>> {
    static HTTP: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    let url = if let Some(rest) = key.strip_prefix("podcast:") {
        format!("{base}/api/podcasts/{}/cover", rest.split(':').next()?.parse::<i64>().ok()?)
    } else if key.starts_with("http") {
        key.to_string()
    } else {
        return None;
    };
    let http = HTTP.get_or_init(|| {
        reqwest::Client::builder().pool_max_idle_per_host(1).pool_idle_timeout(Duration::from_secs(20)).build().expect("http client")
    });
    let r = http.get(url).timeout(Duration::from_secs(30)).send().await.ok()?;
    if !r.status().is_success() {
        return None;
    }
    r.bytes().await.ok().map(|b| b.to_vec()).filter(|b| !b.is_empty())
}
