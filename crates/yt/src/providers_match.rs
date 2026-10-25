//! Finds songs from other services on YouTube Music: search "artist title",
//! score the song results by title / artist similarity and duration, keep
//! the best one if it is good enough.

use std::collections::HashSet;

use futures_util::stream::{self, StreamExt};
use serde_json::Value;

use crate::net::{Error, Result, Transport};
use crate::providers::Source;
use crate::ytm;

/// Songs searched at the same time.
const PARALLEL: usize = 4;
const SEARCH_LIMIT: usize = 10;
/// Longest difference between the two durations that still counts as the same recording.
const MAX_DURATION_DIFF: i64 = 7;
const MIN_TITLE: f64 = 0.6;
const MIN_ARTIST: f64 = 0.5;
const MIN_TOTAL: f64 = 0.72;

/// Words that make a different recording; a result having one the original
/// lacks (a live version of a studio song) is not a match.
const VERSION_WORDS: &[&str] = &[
    "live",
    "remix",
    "instrumental",
    "karaoke",
    "acoustic",
    "cover",
    "sped",
    "slowed",
    "reverb",
    "nightcore",
    "demo",
    "reprise",
    "tribute",
    "8d",
];
/// Trailing " - …" parts that only describe the release, not the song.
const RELEASE_WORDS: &[&str] =
    &["remaster", "edit", "version", "mix", "from", "feat", "single", "mono", "stereo", "deluxe", "bonus", "live", "radio", "album"];

fn fold(c: char) -> char {
    match c {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' => 'a',
        'ç' | 'ć' | 'č' => 'c',
        'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ė' | 'ę' => 'e',
        'ì' | 'í' | 'î' | 'ï' | 'ī' => 'i',
        'ñ' | 'ń' => 'n',
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' => 'o',
        'ù' | 'ú' | 'û' | 'ü' | 'ū' => 'u',
        'ý' | 'ÿ' => 'y',
        'š' | 'ś' => 's',
        'ž' | 'ź' | 'ż' => 'z',
        'ł' => 'l',
        'ß' => 's',
        _ => c,
    }
}

/// Lowercase alphanumeric words, accents folded.
fn words(s: &str) -> Vec<String> {
    let lowered: String = s.to_lowercase().chars().map(fold).collect();
    lowered.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_string).collect()
}

/// A title without "(feat. …)", "[Remastered]" and trailing " - Radio Edit" parts.
pub fn clean_title(title: &str) -> String {
    let mut out = String::new();
    let mut depth = 0usize;
    for c in title.chars() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    if let Some((head, tail)) = out.split_once(" - ") {
        let tail_words = words(tail);
        if !head.trim().is_empty()
            && tail_words.iter().any(|w| RELEASE_WORDS.contains(&w.as_str()) || w.chars().all(|c| c.is_ascii_digit()))
        {
            out = head.to_string();
        }
    }
    out.trim().to_string()
}

/// Dice coefficient of two word lists (as sets): 1.0 = same words.
fn dice(a: &[String], b: &[String]) -> f64 {
    let (sa, sb): (HashSet<&String>, HashSet<&String>) = (a.iter().collect(), b.iter().collect());
    if sa.is_empty() || sb.is_empty() {
        return 0.0;
    }
    2.0 * sa.intersection(&sb).count() as f64 / (sa.len() + sb.len()) as f64
}

fn title_score(source: &str, candidate: &str) -> f64 {
    let (a, b) = (words(&clean_title(source)), words(&clean_title(candidate)));
    let mut score = dice(&a, &b);
    // "Artist - Title" uploads: the source title may carry the artist too.
    if score < 1.0 && !a.is_empty() && !b.is_empty() {
        let (short, long) = if a.len() <= b.len() { (&a, &b) } else { (&b, &a) };
        if short.len() >= 2 && short.iter().all(|w| long.contains(w)) {
            score = score.max(0.8);
        }
    }
    let (raw_a, raw_b) = (words(source), words(candidate));
    if VERSION_WORDS.iter().any(|v| raw_b.iter().any(|w| w == v) && !raw_a.iter().any(|w| w == v)) {
        score *= 0.4;
    }
    score
}

fn artist_score(artists: &[String], title: &str, candidate_artist: &str) -> f64 {
    if artists.is_empty() {
        return 0.7;
    }
    let haystack = words(&format!("{candidate_artist} {title}"));
    artists
        .iter()
        .map(|artist| {
            let w = words(artist);
            if w.is_empty() {
                return 0.0;
            }
            w.iter().filter(|x| haystack.contains(x)).count() as f64 / w.len() as f64
        })
        .fold(0.0, f64::max)
}

/// How well a YouTube Music song (shared track shape) is the source song, 0..1;
/// `None` when it can't be: too different a length, title or artist.
pub fn score(source: &Source, candidate: &Value) -> Option<f64> {
    let title = candidate.get("title").and_then(Value::as_str)?;
    let artist = candidate.get("artist").and_then(Value::as_str).unwrap_or("");
    let length = candidate.get("duration_sec").and_then(Value::as_i64).unwrap_or(0);
    let duration = if source.duration_sec > 0 && length > 0 {
        let diff = (source.duration_sec - length).abs();
        if diff > MAX_DURATION_DIFF {
            return None;
        }
        1.0 - 0.3 * diff as f64 / MAX_DURATION_DIFF as f64
    } else {
        0.8
    };
    let (t, a) = (title_score(&source.title, title), artist_score(&source.artists, title, artist));
    let total = 0.55 * t + 0.25 * a + 0.2 * duration;
    (t >= MIN_TITLE && a >= MIN_ARTIST && total >= MIN_TOTAL).then_some(total)
}

/// The best acceptable candidate; on a tie YouTube Music's own ranking wins.
pub fn best_match<'a>(source: &Source, candidates: &'a [Value]) -> Option<&'a Value> {
    let mut best: Option<(&Value, f64)> = None;
    for c in candidates {
        if let Some(s) = score(source, c) {
            if best.is_none_or(|(_, b)| s > b + 1e-9) {
                best = Some((c, s));
            }
        }
    }
    best.map(|(c, _)| c)
}

/// What to search for, best guess first.
fn queries(source: &Source) -> Vec<String> {
    let first = source.artists.first().map(String::as_str).unwrap_or("");
    let mut list = vec![format!("{first} {}", clean_title(&source.title)).trim().to_string()];
    let full = format!("{} {}", source.title, source.artists.join(" ")).trim().to_string();
    if !list.contains(&full) {
        list.push(full);
    }
    list
}

async fn match_one(t: &dyn Transport, source: &Source) -> Result<Option<Value>> {
    for query in queries(source) {
        let found = ytm::search(t, &query, SEARCH_LIMIT).await?;
        if let Some(best) = best_match(source, &found) {
            return Ok(Some(best.clone()));
        }
    }
    Ok(None)
}

/// YouTube Music match for every source song (same order, `None` = no good
/// match), four searches at a time. Fails only when no search worked at all
/// (offline); a song whose search failed counts as unmatched.
pub async fn match_all(t: &dyn Transport, sources: &[Source]) -> Result<Vec<Option<Value>>> {
    let results: Vec<Result<Option<Value>>> =
        stream::iter(0..sources.len()).map(|i| async move { match_one(t, &sources[i]).await }).buffered(PARALLEL).collect().await;
    if !results.is_empty() && results.iter().all(Result::is_err) {
        let first = results.into_iter().find_map(Result::err).unwrap_or_else(|| Error::Msg("search failed".into()));
        return Err(first);
    }
    Ok(results.into_iter().map(|r| r.ok().flatten()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn src(title: &str, artists: &[&str], secs: i64) -> Source {
        Source { title: title.into(), artists: artists.iter().map(|a| a.to_string()).collect(), duration_sec: secs }
    }

    fn cand(id: &str, title: &str, artist: &str, secs: i64) -> Value {
        json!({"video_id": id, "title": title, "artist": artist, "duration_sec": secs})
    }

    #[test]
    fn titles_lose_decorations() {
        assert_eq!(clean_title("Get Lucky (feat. Pharrell Williams)"), "Get Lucky");
        assert_eq!(clean_title("Yesterday - Remastered 2009"), "Yesterday");
        assert_eq!(clean_title("Song - Radio Edit"), "Song");
        assert_eq!(clean_title("Artist - Title"), "Artist - Title");
        assert_eq!(words("Beyoncé – Déjà Vu!"), ["beyonce", "deja", "vu"]);
    }

    #[test]
    fn exact_song_is_accepted_and_beats_a_live_version() {
        let s = src("One More Time", &["Daft Punk"], 320);
        let list = [cand("live", "One More Time (Live)", "Daft Punk", 322), cand("studio", "One More Time", "Daft Punk", 320)];
        assert_eq!(best_match(&s, &list).unwrap()["video_id"], "studio");
        assert!(best_match(&s, &list[..1]).is_none(), "a live version is not the studio song");
    }

    #[test]
    fn duration_must_be_within_seven_seconds() {
        let s = src("Aerodynamic", &["Daft Punk"], 212);
        assert!(score(&s, &cand("a", "Aerodynamic", "Daft Punk", 219)).is_some());
        assert!(score(&s, &cand("a", "Aerodynamic", "Daft Punk", 220)).is_none());
        assert!(score(&s, &cand("a", "Aerodynamic", "Daft Punk", 0)).is_some(), "unknown length is not held against it");
    }

    #[test]
    fn artist_may_be_one_of_several_and_decorations_are_ignored() {
        let s = src("Get Lucky - Radio Edit", &["Daft Punk", "Pharrell Williams", "Nile Rodgers"], 248);
        assert!(score(&s, &cand("a", "Get Lucky (feat. Pharrell Williams & Nile Rodgers)", "Daft Punk", 248)).is_some());
        assert!(score(&s, &cand("b", "Get Lucky", "Someone Else Entirely", 248)).is_none());
    }

    #[test]
    fn different_song_by_same_artist_is_rejected() {
        let s = src("Digital Love", &["Daft Punk"], 301);
        assert!(best_match(&s, &[cand("a", "Harder, Better, Faster, Stronger", "Daft Punk", 224)]).is_none());
        assert!(best_match(&s, &[cand("a", "Digital Love", "Cover Band", 301)]).is_none());
    }

    #[test]
    fn soundcloud_style_title_with_artist_inside() {
        let s = src("Forss - Flickermood", &["Forss"], 214);
        assert!(score(&s, &cand("a", "Flickermood", "Forss", 214)).is_some());
    }

    #[test]
    fn accents_and_case_do_not_matter() {
        let s = src("DÉJÀ VU", &["BEYONCÉ"], 200);
        assert!(score(&s, &cand("a", "Deja Vu", "Beyonce", 201)).is_some());
    }
}
