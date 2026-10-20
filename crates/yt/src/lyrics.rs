//! Song lyrics: YouTube Music first, then LRCLIB (https://lrclib.net). Port of worker/lyrics.py.
//!
//! Result shape: `{"synced": bool, "lines": [{"time_ms": int|null, "text": str}],
//! "source": str|null, "transient": bool (only when nothing was found)}`.

use std::future::Future;
use std::sync::OnceLock;
use std::time::Duration;

use regex::Regex;
use serde_json::{json, Value};

use crate::innertube::{self, YtLyrics};
use crate::net::{Error, Result, Transport};
use crate::util::{splitlines, truthy, urlencode};

pub const LRCLIB: &str = "https://lrclib.net/api";
const USER_AGENT: &str = "Music Player Lite (https://github.com/danielaufnahme-cmd/music-player-lite)";

fn re(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("valid regex"))
}

fn lrc_time() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r"\[([0-9]{1,3}):([0-9]{1,2})(?:[.:]([0-9]{1,3}))?\]")
}

fn edit_markers() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(
        &R,
        r"(?i)\s*[\(\[][^\)\]]*\b(slowed|sped\s*up|speed\s*up|nightcore|reverb|8d|bass\s*boosted|pitched)\b[^\)\]]*[\)\]]|\s*-\s*(slowed|sped\s*up|nightcore)\b.*$",
    )
}

fn extra() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r"(?i)\s*[\(\[](official|lyrics?|audio|video|visuali[sz]er|explicit|remaster(ed)?)[^\)\]]*[\)\]]")
}

/// No lyrics. `transient` means a source failed (network), so don't cache the miss.
pub fn empty(transient: bool) -> Value {
    json!({"synced": false, "lines": [], "source": null, "transient": transient})
}

fn line(time_ms: Option<i64>, text: &str) -> Value {
    json!({"time_ms": time_ms, "text": text})
}

/// Parse LRC text. Lines can carry several time tags: "[00:12.30][01:40.00]Hey".
pub fn parse_lrc(text: &str) -> Vec<Value> {
    let mut lines: Vec<(i64, String)> = Vec::new();
    for raw in splitlines(text) {
        let stamps: Vec<regex::Captures> = lrc_time().captures_iter(raw).collect();
        let Some(last) = stamps.last() else { continue };
        let words = raw[last.get(0).unwrap().end()..].trim().to_string();
        for m in &stamps {
            let minutes: i64 = m[1].parse().unwrap_or(0);
            let seconds: i64 = m[2].parse().unwrap_or(0);
            let frac = m.get(3).map_or("0", |f| f.as_str());
            let ms: i64 = format!("{frac:0<3}")[..3].parse().unwrap_or(0);
            lines.push(((minutes * 60 + seconds) * 1000 + ms, words.clone()));
        }
    }
    lines.sort_by_key(|(t, _)| *t);
    lines.into_iter().map(|(t, w)| line(Some(t), &w)).collect()
}

pub fn plain_lines(text: &str) -> Vec<Value> {
    splitlines(text.trim()).into_iter().map(|l| line(None, l.trim())).collect()
}

fn strip_chars<'a>(s: &'a str, chars: &[char]) -> &'a str {
    s.trim_matches(|c| chars.contains(&c))
}

/// Strip edit markers like "(Ultra Slowed)" so the original song can be looked up.
pub fn clean_title(title: &str) -> String {
    let no_edits = edit_markers().replace_all(title, "");
    let cleaned = extra().replace_all(&no_edits, "");
    let cleaned = strip_chars(&cleaned, &[' ', '-']);
    if cleaned.is_empty() {
        title.to_string()
    } else {
        cleaned.to_string()
    }
}

/// True for slowed / sped-up / nightcore edits whose timing differs from the original.
pub fn is_edit(title: &str) -> bool {
    edit_markers().is_match(title)
}

pub fn first_artist(artist: &str) -> String {
    static R: OnceLock<Regex> = OnceLock::new();
    let split = re(&R, r"(?i),|&| feat\.? | ft\.? | x ");
    split.splitn(artist, 2).next().unwrap_or("").trim().to_string()
}

/// Stretch timestamps (slowed edits play the same lyrics over a longer time).
pub fn scale_lines(lines: &[Value], factor: f64) -> Vec<Value> {
    lines
        .iter()
        .map(|l| {
            let t = l["time_ms"].as_f64().unwrap_or(0.0);
            line(Some((t * factor).round_ties_even() as i64), l["text"].as_str().unwrap_or(""))
        })
        .collect()
}

/// Best LRCLIB search hit: synced first, then closest duration.
pub fn pick_lrclib(candidates: &[Value], duration_sec: i64) -> Option<Value> {
    let usable = candidates
        .iter()
        .filter(|c| c.is_object() && (c.get("syncedLyrics").is_some_and(truthy) || c.get("plainLyrics").is_some_and(truthy)));
    let rank = |c: &Value| -> (i32, f64) {
        let diff = if duration_sec != 0 { (num(c.get("duration")) - duration_sec as f64).abs() } else { 0.0 };
        (if c.get("syncedLyrics").is_some_and(truthy) { 0 } else { 1 }, diff)
    };
    let mut best: Option<(&Value, (i32, f64))> = None;
    for c in usable {
        let r = rank(c);
        if best.as_ref().is_none_or(|(_, br)| r.0 < br.0 || (r.0 == br.0 && r.1 < br.1)) {
            best = Some((c, r));
        }
    }
    best.map(|(c, _)| c.clone())
}

/// `float(x or 0)` for a JSON value.
fn num(v: Option<&Value>) -> f64 {
    match v {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(Value::String(s)) if !s.is_empty() => s.trim().parse().unwrap_or(0.0),
        Some(Value::Bool(b)) => *b as i32 as f64,
        _ => 0.0,
    }
}

/// GET JSON from LRCLIB, retrying once after a server error.
pub async fn get_json(t: &dyn Transport, url: &str) -> Result<Value> {
    for attempt in 1..=2 {
        match t.get_json(url, USER_AGENT, Duration::from_secs(12)).await {
            Err(Error::Http(code, _)) if code >= 500 && attempt == 1 => {
                tokio::time::sleep(Duration::from_millis(1500)).await;
                continue;
            }
            other => return other,
        }
    }
    Ok(Value::Null)
}

pub async fn from_lrclib(t: &dyn Transport, title: &str, artist: &str, duration_sec: i64) -> Result<Option<Value>> {
    let edit = is_edit(title);
    let track = clean_title(title);
    let who = first_artist(artist);
    let mut hit = Value::Null;
    if !edit && duration_sec != 0 {
        let dur = duration_sec.to_string();
        let query = urlencode(&[("track_name", &track), ("artist_name", &who), ("duration", &dur)]);
        match get_json(t, &format!("{LRCLIB}/get?{query}")).await {
            Ok(v) => hit = v,
            Err(Error::Http(404, _)) => {}
            Err(e) => return Err(e),
        }
    }
    if !truthy(&hit) {
        let query = urlencode(&[("track_name", &track), ("artist_name", &who)]);
        let mut results = get_json(t, &format!("{LRCLIB}/search?{query}")).await?;
        if !truthy(&results) {
            let q = format!("{track} {who}");
            results = get_json(t, &format!("{LRCLIB}/search?{}", urlencode(&[("q", &q)]))).await?;
        }
        let list: &[Value] = results.as_array().map_or(&[], |v| v.as_slice());
        hit = pick_lrclib(list, duration_sec).unwrap_or(Value::Null);
    }
    if !truthy(&hit) || hit.get("instrumental").is_some_and(truthy) {
        return Ok(None);
    }

    let synced_text = hit.get("syncedLyrics").and_then(Value::as_str).unwrap_or("");
    let mut synced = parse_lrc(synced_text);
    if !synced.is_empty() {
        let mut source = "LRCLIB";
        let original = num(hit.get("duration"));
        if original > 0.0 && duration_sec > 0 {
            let factor = duration_sec as f64 / original;
            if edit && (0.5..=2.0).contains(&factor) && (factor - 1.0).abs() > 0.03 {
                synced = scale_lines(&synced, factor);
                source = "LRCLIB (timed to this edit)";
            } else if (factor - 1.0).abs() > 0.1 {
                // A different cut of the song: timestamps would be misleading.
                synced.clear();
            }
        }
        if !synced.is_empty() {
            return Ok(Some(json!({"synced": true, "lines": synced, "source": source})));
        }
    }
    if let Some(plain) = hit.get("plainLyrics").filter(|p| truthy(p)).and_then(Value::as_str) {
        return Ok(Some(json!({"synced": false, "lines": plain_lines(plain), "source": "LRCLIB"})));
    }
    Ok(None)
}

pub async fn from_youtube_music(t: &dyn Transport, video_id: &str) -> Result<Option<Value>> {
    static SOURCE: OnceLock<Regex> = OnceLock::new();
    let watch = innertube::get_watch_playlist(t, video_id, 1, false).await?;
    let Some(browse_id) = watch.lyrics.filter(|b| !b.is_empty()) else {
        return Ok(None);
    };
    let Some(data) = innertube::get_lyrics(t, &browse_id).await? else {
        return Ok(None);
    };
    let clean_source = |s: Option<String>| {
        let s = s.unwrap_or_default();
        let s = re(&SOURCE, r"^\s*Source:\s*").replace(&s, "").trim().to_string();
        if s.is_empty() {
            "YouTube Music".to_string()
        } else {
            s
        }
    };
    match data {
        YtLyrics::Timed { lines, source } => {
            if lines.is_empty() {
                return Ok(None);
            }
            let mut lines: Vec<(i64, String)> = lines.into_iter().map(|(t, s)| (t, s.trim().to_string())).collect();
            lines.sort_by_key(|(t, _)| *t);
            let lines: Vec<Value> = lines.into_iter().map(|(t, s)| line(Some(t), &s)).collect();
            Ok(Some(json!({"synced": true, "lines": lines, "source": clean_source(source)})))
        }
        YtLyrics::Plain { text, source } => {
            if text.is_empty() {
                return Ok(None);
            }
            Ok(Some(json!({"synced": false, "lines": plain_lines(&text), "source": clean_source(source)})))
        }
    }
}

/// Combine the sources: synced wins, then plain, then empty (transient if a source failed).
pub async fn combine<F1, F2, Fut1, Fut2>(ytm: F1, lrclib: F2) -> Value
where
    F1: FnOnce() -> Fut1,
    F2: FnOnce() -> Fut2,
    Fut1: Future<Output = Result<Option<Value>>>,
    Fut2: Future<Output = Result<Option<Value>>>,
{
    let mut plain: Option<Value> = None;
    let mut failed = false;
    let mut consider = |name: &str, res: Result<Option<Value>>| -> Option<Value> {
        match res {
            Err(e) => {
                eprintln!("geonix-yt: lyrics from {name} failed: {e}");
                failed = true;
                None
            }
            Ok(Some(found)) if found["lines"].as_array().is_some_and(|l| !l.is_empty()) => {
                if found["synced"] == Value::Bool(true) {
                    return Some(found);
                }
                if plain.is_none() {
                    plain = Some(found);
                }
                None
            }
            Ok(_) => None,
        }
    };
    if let Some(found) = consider("youtube music", ytm().await) {
        return found;
    }
    if let Some(found) = consider("lrclib", lrclib().await) {
        return found;
    }
    plain.unwrap_or_else(|| empty(failed))
}

/// Synced lyrics when any source has them, otherwise plain, otherwise empty.
pub async fn lyrics(t: &dyn Transport, video_id: &str, title: &str, artist: &str, duration_sec: i64) -> Value {
    combine(
        || from_youtube_music(t, video_id),
        || async move {
            if title.is_empty() {
                Ok(None)
            } else {
                from_lrclib(t, title, artist, duration_sec).await
            }
        },
    )
    .await
}
