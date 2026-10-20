//! Media sources: local files and seekable HTTP(S) URLs (Range requests).

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use symphonia::core::io::MediaSource;

/// Something the player can open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// A local file.
    File(PathBuf),
    /// An `http://` or `https://` URL. Seekable when the server honours `Range`.
    Url(String),
}

impl Source {
    pub fn file(path: impl Into<PathBuf>) -> Self {
        Source::File(path.into())
    }

    pub fn url(url: impl Into<String>) -> Self {
        Source::Url(url.into())
    }

    /// Lower-case file extension from the path / URL path, used as a probe hint.
    pub(crate) fn extension(&self) -> Option<String> {
        let name = match self {
            Source::File(p) => return p.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()),
            Source::Url(u) => u.split(['?', '#']).next().unwrap_or(""),
        };
        let last = name.rsplit('/').next().unwrap_or("");
        let (_, ext) = last.rsplit_once('.')?;
        (!ext.is_empty() && ext.len() <= 5).then(|| ext.to_ascii_lowercase())
    }

    /// Opens the raw byte source. Returns the source and an extension hint
    /// (from the path, or from the HTTP Content-Type).
    pub(crate) fn open(&self) -> Result<(Box<dyn MediaSource>, Option<String>), String> {
        match self {
            Source::File(p) => {
                let f = File::open(p).map_err(|e| format!("{}: {e}", p.display()))?;
                Ok((Box::new(f), self.extension()))
            }
            Source::Url(u) => {
                let r = HttpReader::open(u)?;
                let hint = self.extension().or_else(|| r.content_type_ext());
                Ok((Box::new(r), hint))
            }
        }
    }
}

/// Size of each Range request. Small enough that CDNs (googlevideo) don't
/// throttle, large enough that a local stream needs few requests.
const CHUNK: u64 = 2 * 1024 * 1024;
/// Forward seeks up to this far are served by reading and discarding.
const SKIP_FORWARD: u64 = 128 * 1024;
const RETRIES: usize = 3;

fn client() -> &'static reqwest::blocking::Client {
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .pool_max_idle_per_host(1)
            .pool_idle_timeout(Duration::from_secs(30))
            .user_agent("geonix-lite-player")
            .build()
            .expect("http client")
    })
}

/// A `Read + Seek` view of an HTTP resource, fetched in `CHUNK`-sized Range
/// requests. Only one response body is open at a time; nothing is cached
/// beyond symphonia's own 64 KiB read buffer.
pub struct HttpReader {
    url: String,
    len: Option<u64>,
    pos: u64,
    seekable: bool,
    content_type: Option<String>,
    /// Open body and the absolute end (exclusive) of the bytes it will deliver.
    body: Option<(reqwest::blocking::Response, u64)>,
}

impl HttpReader {
    pub fn open(url: &str) -> Result<Self, String> {
        let mut r = HttpReader { url: url.to_string(), len: None, pos: 0, seekable: false, content_type: None, body: None };
        r.request(0).map_err(|e| format!("{url}: {e}"))?;
        Ok(r)
    }

    /// Content length if known.
    pub fn len(&self) -> Option<u64> {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == Some(0)
    }

    fn content_type_ext(&self) -> Option<String> {
        let ct = self.content_type.as_deref()?.to_ascii_lowercase();
        let ext = match ct.split(';').next()?.trim() {
            "audio/mpeg" | "audio/mp3" => "mp3",
            "audio/mp4" | "audio/m4a" | "audio/x-m4a" | "audio/aac" | "video/mp4" => "m4a",
            "audio/flac" | "audio/x-flac" => "flac",
            "audio/wav" | "audio/x-wav" | "audio/wave" => "wav",
            "audio/ogg" | "application/ogg" | "audio/vorbis" => "ogg",
            _ => return None,
        };
        Some(ext.to_string())
    }

    /// Starts a Range request at `start`.
    fn request(&mut self, start: u64) -> io::Result<()> {
        self.body = None;
        let mut end = start + CHUNK;
        if let Some(len) = self.len {
            end = end.min(len);
        }
        let resp = client()
            .get(&self.url)
            .header(reqwest::header::RANGE, format!("bytes={start}-{}", end.saturating_sub(1)))
            .send()
            .map_err(io::Error::other)?;
        let status = resp.status();
        if self.content_type.is_none() {
            self.content_type = resp.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).map(str::to_string);
        }
        if status == reqwest::StatusCode::PARTIAL_CONTENT {
            // Content-Range: bytes a-b/total
            let cr = resp.headers().get(reqwest::header::CONTENT_RANGE).and_then(|v| v.to_str().ok()).unwrap_or("");
            let (range, total) = cr.trim_start_matches("bytes ").split_once('/').unwrap_or(("", "*"));
            if let Ok(t) = total.trim().parse::<u64>() {
                self.len = Some(t);
            }
            let (a, b) = range.split_once('-').unwrap_or(("", ""));
            let a: u64 = a.trim().parse().unwrap_or(start);
            let b: u64 = b.trim().parse().map(|b: u64| b + 1).unwrap_or(end);
            if a != start {
                return Err(io::Error::other(format!("server returned range {a}, wanted {start}")));
            }
            self.seekable = true;
            self.pos = start;
            self.body = Some((resp, b));
            Ok(())
        } else if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
            // At/after the end.
            if let Some(total) = resp
                .headers()
                .get(reqwest::header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.rsplit('/').next()?.trim().parse::<u64>().ok())
            {
                self.len = Some(total);
            }
            self.pos = start;
            Ok(())
        } else if status.is_success() {
            // Server ignores Range: plain sequential stream.
            if start != 0 {
                return Err(io::Error::new(io::ErrorKind::Unsupported, "server does not support Range"));
            }
            self.len = resp.content_length();
            self.seekable = false;
            self.pos = 0;
            let end = self.len.unwrap_or(u64::MAX);
            self.body = Some((resp, end));
            Ok(())
        } else {
            Err(io::Error::other(format!("HTTP {status}")))
        }
    }
}

impl Read for HttpReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut tries = 0;
        loop {
            if self.len.is_some_and(|l| self.pos >= l) {
                return Ok(0);
            }
            if let Some((body, end)) = self.body.as_mut() {
                if self.pos < *end {
                    let want = buf.len().min((*end - self.pos).min(usize::MAX as u64) as usize);
                    match body.read(&mut buf[..want]) {
                        Ok(0) if !self.seekable => return Ok(0),
                        Ok(0) => {} // connection closed early: re-request below
                        Ok(n) => {
                            self.pos += n as u64;
                            return Ok(n);
                        }
                        Err(e) if !self.seekable => return Err(e),
                        Err(_) => {}
                    }
                    tries += 1;
                } else if !self.seekable {
                    return Ok(0);
                }
            } else if !self.seekable && self.pos > 0 {
                return Ok(0);
            }
            if tries > RETRIES {
                return Err(io::Error::other("stream read failed"));
            }
            if let Err(e) = self.request(self.pos) {
                tries += 1;
                if tries > RETRIES {
                    return Err(e);
                }
                std::thread::sleep(Duration::from_millis(200 * tries as u64));
            } else if self.body.is_none() {
                return Ok(0); // 416: end of resource
            }
        }
    }
}

impl Seek for HttpReader {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let target = match to {
            SeekFrom::Start(p) => p as i128,
            SeekFrom::Current(d) => self.pos as i128 + d as i128,
            SeekFrom::End(d) => match self.len {
                Some(l) => l as i128 + d as i128,
                None => return Err(io::Error::new(io::ErrorKind::Unsupported, "unknown length")),
            },
        };
        if target < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "seek before start"));
        }
        let target = target as u64;
        if target == self.pos {
            return Ok(target);
        }
        // Short forward hop inside the open body: read and discard.
        if target > self.pos && target - self.pos <= SKIP_FORWARD {
            if let Some((_, end)) = &self.body {
                if target <= *end {
                    let mut left = target - self.pos;
                    let mut scratch = [0u8; 8192];
                    while left > 0 {
                        let n = self.read(&mut scratch[..left.min(8192) as usize])?;
                        if n == 0 {
                            break;
                        }
                        left -= n as u64;
                    }
                    if self.pos == target {
                        return Ok(target);
                    }
                }
            }
        }
        if !self.seekable {
            return Err(io::Error::new(io::ErrorKind::Unsupported, "stream is not seekable"));
        }
        self.body = None; // lazily re-requested on next read
        self.pos = target;
        Ok(target)
    }
}

impl MediaSource for HttpReader {
    fn is_seekable(&self) -> bool {
        self.seekable
    }

    fn byte_len(&self) -> Option<u64> {
        self.len
    }
}
