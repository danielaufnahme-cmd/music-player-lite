//! HTTP access behind a small trait, so tests can replay recorded responses.

use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Error of any yt operation. `Display` is the text the Python worker would
/// have put in its exception (before `friendly_error`).
#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    /// HTTP status error (urllib's `HTTPError`): code + text.
    Http(u16, String),
    /// Could not connect / timed out / broken transfer.
    Net(String),
    /// Anything else.
    Msg(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Http(_, text) | Error::Net(text) | Error::Msg(text) => f.write_str(text),
        }
    }
}

impl std::error::Error for Error {}

impl From<String> for Error {
    fn from(s: String) -> Self {
        Error::Msg(s)
    }
}

impl From<&str> for Error {
    fn from(s: &str) -> Self {
        Error::Msg(s.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Which InnerTube client a request is sent as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Client {
    /// YouTube Music web (WEB_REMIX), what ytmusicapi uses.
    Web,
    /// ANDROID_MUSIC: the only client that returns timed lyrics.
    Mobile,
}

pub trait Transport: Send + Sync {
    /// POST to `https://music.youtube.com/youtubei/v1/<endpoint>?alt=json<params>`
    /// with `body` (the context is added by the transport).
    fn innertube<'a>(&'a self, endpoint: &'a str, client: Client, body: Value, params: &'a str) -> BoxFut<'a, Result<Value>>;
    /// GET JSON with a User-Agent and timeout (LRCLIB).
    fn get_json<'a>(&'a self, url: &'a str, user_agent: &'a str, timeout: Duration) -> BoxFut<'a, Result<Value>>;
    /// GET raw bytes (cover art).
    fn get_bytes<'a>(&'a self, url: &'a str, user_agent: &'a str, timeout: Duration) -> BoxFut<'a, Result<Vec<u8>>>;
}

pub const YTM_DOMAIN: &str = "https://music.youtube.com";
const YTM_BASE_API: &str = "https://music.youtube.com/youtubei/v1/";
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:88.0) Gecko/20100101 Firefox/88.0";

/// One InnerTube request. Short, so a slow YouTube Music fails fast and the
/// yt-dlp fallback (or a clear error) follows instead of a long wait.
const INNERTUBE_TIMEOUT: Duration = Duration::from_secs(15);

/// The real network transport (reqwest + rustls).
pub struct HttpTransport {
    http: reqwest::Client,
    visitor_id: Mutex<Option<String>>,
}

impl Default for HttpTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpTransport {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .pool_idle_timeout(Duration::from_secs(30))
            .pool_max_idle_per_host(1)
            .build()
            .unwrap_or_default();
        Self { http, visitor_id: Mutex::new(None) }
    }

    /// The X-Goog-Visitor-Id from the YouTube Music home page (cached; retried on failure).
    async fn visitor_id(&self) -> String {
        if let Some(id) = self.visitor_id.lock().unwrap().clone() {
            return id;
        }
        let fetched = async {
            let text = self
                .http
                .get(YTM_DOMAIN)
                .header("user-agent", USER_AGENT)
                .header("accept", "*/*")
                .timeout(Duration::from_secs(8))
                .send()
                .await
                .ok()?
                .text()
                .await
                .ok()?;
            visitor_id_from_html(&text)
        }
        .await;
        match fetched {
            Some(id) => {
                *self.visitor_id.lock().unwrap() = Some(id.clone());
                id
            }
            None => String::new(),
        }
    }
}

pub fn visitor_id_from_html(html: &str) -> Option<String> {
    let re = regex::Regex::new(r"ytcfg\.set\s*\(\s*(\{.+?\})\s*\)\s*;").ok()?;
    // Python only looks at the first match.
    let caps = re.captures(html)?;
    let cfg = serde_json::from_str::<Value>(&caps[1]).ok()?;
    Some(cfg.get("VISITOR_DATA").and_then(Value::as_str).unwrap_or("").to_string())
}

/// "1.YYYYMMDD.01.00" for today's UTC date, like ytmusicapi.
pub fn web_client_version() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) as i64;
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    format!("1.{y:04}{m:02}{d:02}.01.00")
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub fn context(client: Client) -> Value {
    let (name, version) = match client {
        Client::Web => ("WEB_REMIX", web_client_version()),
        Client::Mobile => ("ANDROID_MUSIC", "7.21.50".to_string()),
    };
    json!({"client": {"clientName": name, "clientVersion": version, "hl": "en"}, "user": {}})
}

/// Text of a reqwest error with its causes, so "connection refused" / "timed out" show up.
pub fn describe(err: &reqwest::Error) -> String {
    let mut text = err.to_string();
    let mut src = std::error::Error::source(err);
    while let Some(s) = src {
        let part = s.to_string();
        if !text.contains(&part) {
            text.push_str(": ");
            text.push_str(&part);
        }
        src = s.source();
    }
    text
}

fn net_error(err: reqwest::Error) -> Error {
    if let Some(status) = err.status() {
        return Error::Http(status.as_u16(), format!("HTTP Error {}: {}", status.as_u16(), status.canonical_reason().unwrap_or("")));
    }
    if err.is_timeout() {
        return Error::Net(format!("timed out ({})", describe(&err)));
    }
    if err.is_decode() {
        return Error::Msg(describe(&err));
    }
    Error::Net(format!("connection failed: {}", describe(&err)))
}

impl Transport for HttpTransport {
    fn innertube<'a>(&'a self, endpoint: &'a str, client: Client, mut body: Value, params: &'a str) -> BoxFut<'a, Result<Value>> {
        Box::pin(async move {
            if let Value::Object(map) = &mut body {
                map.insert("context".into(), context(client));
            }
            let visitor = self.visitor_id().await;
            let url = format!("{YTM_BASE_API}{endpoint}?alt=json{params}");
            let resp = self
                .http
                .post(url)
                .header("user-agent", USER_AGENT)
                .header("accept", "*/*")
                .header("content-type", "application/json")
                .header("origin", YTM_DOMAIN)
                .header("X-Goog-Visitor-Id", visitor)
                .timeout(INNERTUBE_TIMEOUT)
                .body(body.to_string())
                .send()
                .await
                .map_err(net_error)?;
            let status = resp.status();
            let text = resp.text().await.map_err(net_error)?;
            let parsed: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            if status.as_u16() >= 400 {
                let detail = parsed.pointer("/error/message").and_then(Value::as_str).unwrap_or("");
                return Err(Error::Http(
                    status.as_u16(),
                    format!("Server returned HTTP {}: {}.\n{}", status.as_u16(), status.canonical_reason().unwrap_or(""), detail),
                ));
            }
            if parsed.is_null() {
                return Err(Error::Msg("Invalid response from YouTube Music".into()));
            }
            Ok(parsed)
        })
    }

    fn get_json<'a>(&'a self, url: &'a str, user_agent: &'a str, timeout: Duration) -> BoxFut<'a, Result<Value>> {
        Box::pin(async move {
            let resp = self
                .http
                .get(url)
                .header("user-agent", user_agent)
                .timeout(timeout)
                .send()
                .await
                .map_err(net_error)?
                .error_for_status()
                .map_err(net_error)?;
            let bytes = resp.bytes().await.map_err(net_error)?;
            serde_json::from_slice(&bytes).map_err(|e| Error::Msg(format!("invalid JSON: {e}")))
        })
    }

    fn get_bytes<'a>(&'a self, url: &'a str, user_agent: &'a str, timeout: Duration) -> BoxFut<'a, Result<Vec<u8>>> {
        Box::pin(async move {
            let resp = self
                .http
                .get(url)
                .header("user-agent", user_agent)
                .timeout(timeout)
                .send()
                .await
                .map_err(net_error)?
                .error_for_status()
                .map_err(net_error)?;
            Ok(resp.bytes().await.map_err(net_error)?.to_vec())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_745), (2026, 10, 19));
        assert!(web_client_version().starts_with("1.20"));
    }

    #[test]
    fn visitor_id_parsing() {
        let html = r#"<script>ytcfg.set({"VISITOR_DATA":"abc%3D","X":1});</script>"#;
        assert_eq!(visitor_id_from_html(html).as_deref(), Some("abc%3D"));
        assert_eq!(visitor_id_from_html("nothing"), None);
    }
}
