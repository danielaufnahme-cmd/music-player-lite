//! Offline parity: replay the InnerTube/LRCLIB responses recorded from the Python
//! worker and check the Rust crate returns exactly what the worker returned.

mod common;

use std::sync::Arc;

use common::{assert_json_eq, fixture, Replay};
use geonix_yt::{lyrics, Yt, YtConfig};
use serde_json::Value;

async fn run_fixture(name: &str) -> (Value, Arc<Replay>) {
    let f = fixture(name);
    let replay = Arc::new(Replay::from_fixture(&f));
    let yt = Yt::with_transport(YtConfig::default(), replay.clone());
    let cmd = f["cmd"].as_str().unwrap();
    let out = yt.handle(cmd, f["params"].clone(), |_| {}).await;
    let out = out.unwrap_or_else(|e| panic!("{name}: {e}; unmatched: {:?}", replay.unmatched.lock().unwrap()));
    assert!(replay.unmatched.lock().unwrap().is_empty(), "{name}: unmatched requests {:?}", replay.unmatched.lock().unwrap());
    assert_json_eq(&out, &f["expected"], name);
    (out, replay)
}

#[tokio::test]
async fn search_matches_python() {
    let (out, _) = run_fixture("search_bohemian").await;
    let first = &out["tracks"][0];
    assert_eq!(first["title"], "Bohemian Rhapsody");
    assert_eq!(first["artist"], "Queen");
    assert!(first["thumbnail_url"].as_str().unwrap().contains("=w544-h544"));
    assert_eq!(out["tracks"].as_array().unwrap().len(), 20);
}

#[tokio::test]
async fn search_respects_small_limit() {
    let (out, _) = run_fixture("search_limit3").await;
    assert_eq!(out["tracks"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn search_follows_continuations() {
    let (_, replay) = run_fixture("search_limit40").await;
    assert_eq!(replay.used.lock().unwrap().len(), 2, "second page fetched");
}

#[tokio::test]
async fn public_playlist_with_continuation() {
    let (out, replay) = run_fixture("playlist_public").await;
    assert_eq!(out["tracks"].as_array().unwrap().len(), 171);
    assert_eq!(replay.used.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn album_playlist_link() {
    run_fixture("playlist_ola").await;
}

#[tokio::test]
async fn album_browse_link() {
    let (out, _) = run_fixture("album").await;
    assert_eq!(out["title"], "A Night At The Opera");
}

#[tokio::test]
async fn song_info_matches_python() {
    run_fixture("song_info").await;
}

#[tokio::test]
async fn recommendations_match_python() {
    let (out, _) = run_fixture("recommend").await;
    assert_eq!(out["tracks"].as_array().unwrap().len(), 30);
}

#[tokio::test]
async fn youtube_music_timed_lyrics() {
    let (out, _) = run_fixture("lyrics_ytm").await;
    assert_eq!(out["synced"], true);
}

async fn lrclib_only(name: &str) {
    let f = fixture(name);
    let replay = Replay::from_fixture(&f);
    let p = &f["params"];
    let (title, artist, dur) = (p["title"].as_str().unwrap(), p["artist"].as_str().unwrap(), p["duration_sec"].as_i64().unwrap());
    let out = lyrics::combine(|| async { Ok(None) }, || lyrics::from_lrclib(&replay, title, artist, dur)).await;
    assert_eq!(
        *replay.urls.lock().unwrap(),
        f["http"].as_array().unwrap().iter().map(|h| h["url"].as_str().unwrap().to_string()).collect::<Vec<_>>()
    );
    assert_json_eq(&out, &f["expected"], name);
}

#[tokio::test]
async fn lrclib_exact_lookup() {
    lrclib_only("lyrics_lrclib").await;
}

#[tokio::test]
async fn lrclib_slowed_edit_search() {
    lrclib_only("lyrics_lrclib_slowed").await;
}
