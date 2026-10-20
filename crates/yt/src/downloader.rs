//! Download one song as MP3 with the `yt-dlp` binary, then tag it in Rust.
//! Port of worker/downloader.py.
//!
//! Files land in `<dest_dir>/<Artist>/<Album>/<Title>.mp3` with title, artist,
//! album and embedded cover art. The cover is also saved to `covers_dir` so the
//! backend can serve it without reading the MP3.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::net::{Error, Result, Transport};
use crate::tools;
use crate::util::{expanduser, first_truthy, get, py_round, py_str, truthy};

/// YouTube sometimes rejects a single stream URL (403) or rate-limits briefly;
/// a fresh attempt usually succeeds. Each retry also switches player clients.
pub const PLAYER_CLIENTS: [Option<&[&str]>; 3] = [None, Some(&["tv_simply", "web_safari"]), Some(&["android_vr", "mweb"])];
pub const MAX_ATTEMPTS: usize = PLAYER_CLIENTS.len();
pub const RETRY_DELAYS: [f64; 2] = [2.0, 5.0];

pub fn is_retryable(message: &str) -> bool {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(
            r"(?i)HTTP Error (403|429|5\d\d)|timed out|Connection (reset|aborted|refused)|Temporary failure|IncompleteRead|Remote end closed|fragment",
        )
        .unwrap()
    })
    .is_match(message)
}

/// Player clients for a 1-based attempt number (None = yt-dlp's defaults).
pub fn attempt_clients(attempt: usize) -> Option<&'static [&'static str]> {
    PLAYER_CLIENTS[attempt.clamp(1, PLAYER_CLIENTS.len()) - 1]
}

/// Make a string safe as a single path component.
pub fn safe_name(name: &str, fallback: &str) -> String {
    static BAD: OnceLock<Regex> = OnceLock::new();
    static SPACES: OnceLock<Regex> = OnceLock::new();
    static RESERVED: OnceLock<Regex> = OnceLock::new();
    let bad = BAD.get_or_init(|| Regex::new(r#"[\\/:*?"<>|\x00-\x1f]"#).unwrap());
    let cleaned = bad.replace_all(name, "_");
    let cleaned = cleaned.trim().trim_matches('.');
    let cleaned = SPACES.get_or_init(|| Regex::new(r"\s+").unwrap()).replace_all(cleaned, " ");
    // Windows rejects names ending in a dot or space (also after cutting to
    // length) and reserves device names like CON or COM1, even with a suffix.
    let cut: String = cleaned.chars().take(120).collect();
    let mut cleaned = cut.trim_end_matches([' ', '.']).to_string();
    let reserved = RESERVED.get_or_init(|| Regex::new(r"(?is)^(CON|PRN|AUX|NUL|COM[1-9]|LPT[1-9])(\..*)?$").unwrap());
    if reserved.is_match(&cleaned) {
        cleaned = format!("_{cleaned}");
    }
    if cleaned.is_empty() {
        fallback.to_string()
    } else {
        cleaned
    }
}

pub fn human_speed(bytes_per_sec: Option<f64>) -> Option<String> {
    let mut value = bytes_per_sec.filter(|v| *v > 0.0)?;
    for unit in ["B", "KiB", "MiB", "GiB"] {
        if value < 1024.0 || unit == "GiB" {
            return Some(format!("{value:.1}{unit}/s"));
        }
        value /= 1024.0;
    }
    None
}

pub fn target_path(dest_dir: &Path, artist: &str, album: &str, title: &str, video_id: &str) -> PathBuf {
    let folder = dest_dir.join(safe_name(artist, "Unknown artist")).join(safe_name(album, "Singles"));
    let name = safe_name(title, video_id);
    let path = folder.join(format!("{name}.mp3"));
    if path.exists() {
        // Same title already on disk (e.g. a live version): keep both.
        return folder.join(format!("{name} [{video_id}].mp3"));
    }
    path
}

/// Strip yt-dlp's "ERROR: [youtube] id: " prefix from an error line.
pub fn clean_error(message: &str) -> String {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^ERROR:\s*(\[[^\]]+\]\s*)?(\S+:\s*)?").unwrap()).replace(message, "").trim().to_string()
}

/// Marker for our `--progress-template` lines.
const PROGRESS_TAG: &str = "GXPROG";

/// One parsed progress line from yt-dlp.
#[derive(Debug, Clone, PartialEq)]
pub struct Progress {
    pub status: String,
    pub downloaded: Option<f64>,
    pub total: Option<f64>,
    pub total_estimate: Option<f64>,
    pub speed: Option<f64>,
}

pub fn parse_progress_line(line: &str) -> Option<Progress> {
    let rest = line.trim().strip_prefix(PROGRESS_TAG)?;
    let mut parts = rest.split_whitespace();
    let num = |s: Option<&str>| s.and_then(|s| s.parse::<f64>().ok()).filter(|v| v.is_finite());
    Some(Progress {
        status: parts.next()?.to_string(),
        downloaded: num(parts.next()),
        total: num(parts.next()),
        total_estimate: num(parts.next()),
        speed: num(parts.next()),
    })
}

/// Turns progress lines into the worker's progress JSON (throttled to 4/s).
pub struct ProgressMapper {
    last_emit: Option<Instant>,
}

impl Default for ProgressMapper {
    fn default() -> Self {
        Self::new()
    }
}

impl ProgressMapper {
    pub fn new() -> Self {
        Self { last_emit: None }
    }

    pub fn map(&mut self, p: &Progress) -> Option<Value> {
        self.map_at(p, Instant::now())
    }

    pub fn map_at(&mut self, p: &Progress, now: Instant) -> Option<Value> {
        match p.status.as_str() {
            "downloading" => {
                let total = p.total.filter(|t| *t > 0.0).or(p.total_estimate.filter(|t| *t > 0.0)).unwrap_or(0.0);
                let done = p.downloaded.unwrap_or(0.0);
                if self.last_emit.is_some_and(|t| now.duration_since(t) < Duration::from_millis(250)) {
                    return None;
                }
                self.last_emit = Some(now);
                // Downloading is 0–95 %, the MP3 conversion is the last 5 %.
                let percent = if total > 0.0 { done / total * 95.0 } else { 0.0 };
                Some(json!({"percent": py_round(percent, 1), "speed": human_speed(p.speed)}))
            }
            "finished" => Some(json!({"percent": 95.0, "speed": null})),
            _ => None,
        }
    }
}

/// yt-dlp arguments for one attempt (best VBR quality).
pub fn ytdlp_args(video_id: &str, tmp: &Path, attempt: usize, ffmpeg: Option<&Path>) -> Vec<String> {
    ytdlp_args_with(video_id, tmp, attempt, ffmpeg, None)
}

/// MP3 bitrate for `--audio-quality`: `Some(128 | 192 | 320)` kbps, `None` = best VBR ("0").
pub fn audio_quality_arg(kbps: Option<u32>) -> String {
    match kbps {
        Some(k) if (32..=320).contains(&k) => format!("{k}K"),
        _ => "0".into(),
    }
}

/// yt-dlp arguments for one attempt with an optional fixed MP3 bitrate.
pub fn ytdlp_args_with(video_id: &str, tmp: &Path, attempt: usize, ffmpeg: Option<&Path>, kbps: Option<u32>) -> Vec<String> {
    let esc = |p: &Path| p.to_string_lossy().replace('%', "%%");
    let mut args = vec![
        "-f".into(),
        "bestaudio/best".into(),
        "-o".into(),
        format!("{}/{video_id}.%(ext)s", esc(tmp)),
        "--no-playlist".into(),
        "--no-warnings".into(),
        "--quiet".into(),
        "--progress".into(),
        "--newline".into(),
        "--progress-template".into(),
        format!(
            "download:{PROGRESS_TAG} %(progress.status)s %(progress.downloaded_bytes)s %(progress.total_bytes)s %(progress.total_bytes_estimate)s %(progress.speed)s"
        ),
        "-x".into(),
        "--audio-format".into(),
        "mp3".into(),
        "--audio-quality".into(),
        audio_quality_arg(kbps),
        "--print-to-file".into(),
        "after_move:%()j".into(),
        format!("{}/info.json", esc(tmp)),
    ];
    if let Some(clients) = attempt_clients(attempt) {
        args.push("--extractor-args".into());
        args.push(format!("youtube:player_client={}", clients.join(",")));
    }
    if let Some(ff) = ffmpeg {
        args.push("--ffmpeg-location".into());
        args.push(ff.to_string_lossy().into_owned());
    }
    args.push("--".into());
    args.push(format!("https://music.youtube.com/watch?v={video_id}"));
    args
}

/// A temp directory removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(prefix: &str) -> Result<Self> {
        let base = std::env::temp_dir();
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
        for i in 0..100u32 {
            let p = base.join(format!("{prefix}{:x}{:x}{i}", std::process::id(), nanos));
            match std::fs::create_dir(&p) {
                Ok(()) => return Ok(Self(p)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(Error::Msg(format!("cannot create temp dir: {e}"))),
            }
        }
        Err("cannot create temp dir".into())
    }

    fn clear(&self) {
        if let Ok(entries) = std::fs::read_dir(&self.0) {
            for e in entries.flatten() {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Run one yt-dlp attempt, streaming progress. Err(message) on failure.
async fn run_attempt(ytdlp: &Path, args: &[String], progress: &(dyn Fn(Value) + Send + Sync)) -> std::result::Result<(), String> {
    let mut cmd = tools::command(ytdlp);
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("could not start yt-dlp: {e}"))?;
    let stdout = child.stdout.take().expect("piped");
    let stderr = child.stderr.take().expect("piped");
    let mut mapper = ProgressMapper::new();
    let mut errors: Vec<String> = Vec::new();
    let mut out_lines = BufReader::new(stdout).lines();
    let mut err_lines = BufReader::new(stderr).lines();
    let (mut out_done, mut err_done) = (false, false);
    while !(out_done && err_done) {
        let line = tokio::select! {
            l = out_lines.next_line(), if !out_done => match l { Ok(Some(l)) => l, _ => { out_done = true; continue } },
            l = err_lines.next_line(), if !err_done => match l { Ok(Some(l)) => l, _ => { err_done = true; continue } },
        };
        if let Some(p) = parse_progress_line(&line) {
            if let Some(update) = mapper.map(&p) {
                progress(update);
            }
        } else if !line.trim().is_empty() {
            if errors.len() > 50 {
                errors.remove(0);
            }
            errors.push(line);
        }
    }
    let status = child.wait().await.map_err(|e| e.to_string())?;
    if status.success() {
        return Ok(());
    }
    let text = errors.join("\n");
    Err(tools::ytdlp_error(&text).unwrap_or_else(|| "Download failed".into()))
}

/// Covers must be JPEG for ID3; convert WebP/PNG thumbnails with ffmpeg.
pub async fn to_jpeg(data: Vec<u8>, ffmpeg: Option<&Path>) -> Option<Vec<u8>> {
    if data.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some(data);
    }
    let ffmpeg = ffmpeg.map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("ffmpeg"));
    match tools::pipe(
        &ffmpeg,
        &["-loglevel", "error", "-i", "pipe:0", "-f", "image2", "-c:v", "mjpeg", "pipe:1"],
        data,
        Duration::from_secs(30),
    )
    .await
    {
        Ok(out) if !out.is_empty() => Some(out),
        Ok(_) => None,
        Err(e) => {
            eprintln!("geonix-yt: cover conversion failed: {e}");
            None
        }
    }
}

/// Write ID3v2.3 title/artist/album and a front cover (replacing any old covers).
pub fn tag_mp3(path: &Path, title: &str, artist: &str, album: &str, cover: Option<&[u8]>) -> Result<()> {
    use id3::frame::{Picture, PictureType};
    use id3::{TagLike, Version};
    let mut tag = match id3::Tag::read_from_path(path) {
        Ok(t) => t,
        Err(e) if matches!(e.kind, id3::ErrorKind::NoTag) => id3::Tag::new(),
        Err(e) => return Err(Error::Msg(format!("cannot read tags: {e}"))),
    };
    tag.remove_all_pictures();
    tag.set_title(title);
    tag.set_artist(artist);
    if !album.is_empty() {
        tag.set_album(album);
    }
    if let Some(data) = cover {
        tag.add_frame(Picture {
            mime_type: "image/jpeg".into(),
            picture_type: PictureType::CoverFront,
            description: "Cover".into(),
            data: data.to_vec(),
        });
    }
    tag.write_to_path(path, Version::Id3v23).map_err(|e| Error::Msg(format!("cannot write tags: {e}")))
}

fn move_file(from: &Path, to: &Path) -> std::io::Result<()> {
    match std::fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(_) => {
            std::fs::copy(from, to)?;
            std::fs::remove_file(from)
        }
    }
}

fn str_of(v: Option<&Value>) -> Option<String> {
    v.filter(|x| truthy(x)).map(py_str)
}

pub struct Tools<'a> {
    pub ytdlp: Option<&'a Path>,
    pub ffmpeg: Option<&'a Path>,
}

/// Per-download options from the request (`audio_kbps`, `retry`).
#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// Fixed MP3 bitrate in kbps; None = best VBR.
    pub kbps: Option<u32>,
    /// Retry transient failures with other player clients.
    pub retry: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options { kbps: None, retry: true }
    }
}

pub async fn download(
    t: &dyn Transport,
    tools_: Tools<'_>,
    video_id: &str,
    dest_dir: &str,
    progress: &(dyn Fn(Value) + Send + Sync),
    meta: Option<&Value>,
    covers_dir: Option<&str>,
) -> Result<Value> {
    download_with(t, tools_, video_id, dest_dir, progress, meta, covers_dir, Options::default()).await
}

#[allow(clippy::too_many_arguments)]
pub async fn download_with(
    t: &dyn Transport,
    tools_: Tools<'_>,
    video_id: &str,
    dest_dir: &str,
    progress: &(dyn Fn(Value) + Send + Sync),
    meta: Option<&Value>,
    covers_dir: Option<&str>,
    opts: Options,
) -> Result<Value> {
    let ytdlp = crate::ytm::require_ytdlp(tools_.ytdlp)?;
    let dest = expanduser(dest_dir);
    std::fs::create_dir_all(&dest).map_err(|e| Error::Msg(format!("cannot create {}: {e}", dest.display())))?;
    let empty = json!({});
    let meta = meta.unwrap_or(&empty);

    let tmp = TempDir::new(&format!("geonix-{video_id}-"))?;
    for attempt in 1..=MAX_ATTEMPTS {
        let args = ytdlp_args_with(video_id, &tmp.0, attempt, tools_.ffmpeg, opts.kbps);
        match run_attempt(ytdlp, &args, progress).await {
            Ok(()) => break,
            Err(raw) => {
                let message = clean_error(&raw);
                if opts.retry && attempt < MAX_ATTEMPTS && is_retryable(&message) {
                    eprintln!("geonix-yt: download {video_id} attempt {attempt} failed ({message}); retrying");
                    tmp.clear();
                    progress(json!({"percent": 0.0, "speed": format!("retrying ({}/{MAX_ATTEMPTS})", attempt + 1)}));
                    tokio::time::sleep(Duration::from_secs_f64(RETRY_DELAYS[attempt - 1])).await;
                    continue;
                }
                return Err(Error::Msg(if message.is_empty() { "Download failed".into() } else { message }));
            }
        }
    }

    let mp3 = tmp.0.join(format!("{video_id}.mp3"));
    if !mp3.exists() {
        return Err("Conversion to MP3 failed (is ffmpeg installed?)".into());
    }
    let info: Value = std::fs::read_to_string(tmp.0.join("info.json"))
        .ok()
        .and_then(|s| s.lines().rev().find_map(|l| serde_json::from_str(l).ok()))
        .unwrap_or_else(|| json!({}));

    let title =
        str_of(get(meta, "title")).or_else(|| str_of(first_truthy(&info, &["track", "title"]))).unwrap_or_else(|| video_id.to_string());
    let artists_joined = match get(&info, "artists") {
        Some(Value::Array(a)) => a.iter().map(py_str).collect::<Vec<_>>().join(", "),
        _ => String::new(),
    };
    let artist = str_of(get(meta, "artist"))
        .or_else(|| str_of(get(&info, "artist")))
        .or_else(|| (!artists_joined.is_empty()).then_some(artists_joined))
        .or_else(|| str_of(first_truthy(&info, &["channel", "uploader"])))
        .unwrap_or_else(|| "Unknown artist".into());
    let album = str_of(get(meta, "album")).or_else(|| str_of(get(&info, "album"))).unwrap_or_default();
    let duration = first_truthy(meta, &["duration_sec"])
        .or_else(|| first_truthy(&info, &["duration"]))
        .map(|d| match d {
            Value::Number(n) => n.as_i64().unwrap_or_else(|| n.as_f64().unwrap_or(0.0) as i64),
            Value::String(s) => s.trim().parse::<f64>().map(|f| f as i64).unwrap_or(0),
            _ => 0,
        })
        .unwrap_or(0);
    let thumbnail = str_of(get(meta, "thumbnail_url")).or_else(|| str_of(get(&info, "thumbnail"))).unwrap_or_default();

    let cover = if thumbnail.is_empty() {
        None
    } else {
        match t.get_bytes(&thumbnail, "Mozilla/5.0 Geonix", Duration::from_secs(20)).await {
            Ok(raw) if !raw.is_empty() => to_jpeg(raw, tools_.ffmpeg).await,
            Ok(_) => None,
            Err(e) => {
                eprintln!("geonix-yt: cover download failed: {e}");
                None
            }
        }
    };
    tag_mp3(&mp3, &title, &artist, &album, cover.as_deref())?;

    let final_path = target_path(&dest, &artist, &album, &title, video_id);
    if let Some(parent) = final_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::Msg(format!("cannot create {}: {e}", parent.display())))?;
    }
    move_file(&mp3, &final_path).map_err(|e| Error::Msg(format!("cannot move the MP3 into place: {e}")))?;
    drop(tmp);

    let mut cover_path = Value::Null;
    if let (Some(cover), Some(cdir)) = (cover.as_ref(), covers_dir.filter(|c| !c.is_empty())) {
        let cdir = expanduser(cdir);
        std::fs::create_dir_all(&cdir).map_err(|e| Error::Msg(format!("cannot create {}: {e}", cdir.display())))?;
        let cpath = cdir.join(format!("{video_id}.jpg"));
        std::fs::write(&cpath, cover).map_err(|e| Error::Msg(format!("cannot write cover: {e}")))?;
        cover_path = json!(cpath.to_string_lossy());
    }

    progress(json!({"percent": 100.0, "speed": null}));
    Ok(json!({
        "video_id": video_id,
        "title": title,
        "artist": artist,
        "album": album,
        "duration_sec": duration,
        "thumbnail_url": thumbnail,
        "file_path": final_path.to_string_lossy(),
        "cover_path": cover_path,
        "downloaded": true,
    }))
}
