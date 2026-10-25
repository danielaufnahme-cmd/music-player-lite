//! Taste-based recommendations (port of worker/recommender.py).
//!
//! For each seed song we ask YouTube Music what it would play next. Songs that
//! show up for several seeds are the strongest matches; position in each list
//! breaks ties. Songs already in the library are removed, and no artist may
//! take over the list.

use std::collections::{HashMap, HashSet};
use std::future::Future;

use futures_util::stream::{self, StreamExt};
use serde_json::{json, Value};

use crate::net::{Error, Result};
use crate::util::py_round;

pub const MAX_PER_ARTIST: usize = 4;

/// (seed track, related tracks) per seed, in seed order.
/// (seed track, related tracks) for one seed.
pub type SeedRelated = (Option<Value>, Vec<Value>);
pub type Related = Vec<(String, SeedRelated)>;

/// Score candidates across seeds. Each appearance is worth 1 point plus up to
/// 0.5 for being early in that seed's list. The seed where a song ranked best
/// is its "because".
pub fn rank(related_by_seed: &Related, exclude: &[String], limit: usize, max_per_artist: usize) -> Vec<Value> {
    let mut excluded: HashSet<&str> = exclude.iter().map(String::as_str).collect();
    excluded.extend(related_by_seed.iter().map(|(s, _)| s.as_str()));
    let mut scores: HashMap<String, f64> = HashMap::new();
    let mut tracks: HashMap<String, Value> = HashMap::new();
    let mut best_pos: HashMap<String, (f64, String)> = HashMap::new();
    let mut order: Vec<String> = Vec::new();

    for (seed_id, (_, candidates)) in related_by_seed {
        let n = candidates.len().max(1) as f64;
        let mut seen_here: HashSet<String> = HashSet::new();
        for (pos, track) in candidates.iter().enumerate() {
            let Some(vid) = track.get("video_id").and_then(Value::as_str).filter(|v| !v.is_empty()) else {
                continue;
            };
            if excluded.contains(vid) || seen_here.contains(vid) {
                continue;
            }
            seen_here.insert(vid.to_string());
            let closeness = 1.0 - pos as f64 / n;
            if !scores.contains_key(vid) {
                order.push(vid.to_string());
            }
            *scores.entry(vid.to_string()).or_insert(0.0) += 1.0 + 0.5 * closeness;
            tracks.entry(vid.to_string()).or_insert_with(|| track.clone());
            let better = best_pos.get(vid).is_none_or(|(c, _)| closeness > *c);
            if better {
                best_pos.insert(vid.to_string(), (closeness, seed_id.clone()));
            }
        }
    }

    order.sort_by(|a, b| {
        let (sa, sb) = (scores[a], scores[b]);
        sb.partial_cmp(&sa)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| best_pos[b].0.partial_cmp(&best_pos[a].0).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| a.cmp(b))
    });

    let mut per_artist: HashMap<String, usize> = HashMap::new();
    let mut out = Vec::new();
    for vid in order {
        let mut track = tracks[&vid].clone();
        let artist_key = track.get("artist").and_then(Value::as_str).unwrap_or("").split(',').next().unwrap_or("").trim().to_lowercase();
        let count = per_artist.entry(artist_key).or_insert(0);
        if *count >= max_per_artist {
            continue;
        }
        *count += 1;
        let because = best_pos[&vid].1.clone();
        let seed_track = related_by_seed.iter().find(|(s, _)| *s == because).and_then(|(_, (t, _))| t.as_ref());
        let because_artist = seed_track.and_then(|t| t.get("artist")).filter(|a| crate::util::truthy(a)).cloned().unwrap_or(Value::Null);
        if let Value::Object(m) = &mut track {
            m.insert("score".into(), json!(py_round(scores[&vid], 3)));
            m.insert("because_video_id".into(), json!(because));
            m.insert("because_artist".into(), because_artist);
        }
        out.push(track);
        if out.len() >= limit {
            break;
        }
    }
    out
}

/// Fetch related songs for every seed (up to 6 at once) and rank them.
pub async fn recommend<F, Fut>(seed_ids: &[String], exclude_ids: &[String], limit: usize, fetch: F) -> Result<Vec<Value>>
where
    F: Fn(String) -> Fut,
    Fut: Future<Output = Result<(Option<Value>, Vec<Value>)>>,
{
    let mut seeds: Vec<String> = Vec::new();
    for s in seed_ids {
        // SoundCloud songs have no YouTube Music radio.
        if !s.is_empty() && !crate::ytm::is_soundcloud_id(s) && !seeds.contains(s) {
            seeds.push(s.clone());
        }
    }
    if seeds.is_empty() {
        return Ok(vec![]);
    }
    let width = seeds.len().min(6);
    let results: Vec<(String, Result<SeedRelated>)> = stream::iter(seeds)
        .map(|seed| {
            let fut = fetch(seed.clone());
            async move { (seed, fut.await) }
        })
        .buffered(width)
        .collect()
        .await;
    let mut related: Related = Vec::new();
    for (seed, res) in results {
        match res {
            Ok(r) => related.push((seed, r)),
            Err(e) => eprintln!("geonix-yt: related songs for {seed} failed: {e}"),
        }
    }
    if related.is_empty() {
        return Err(Error::Msg("Could not reach YouTube Music for recommendations".into()));
    }
    Ok(rank(&related, exclude_ids, limit, MAX_PER_ARTIST))
}
