//! Live network tests (yt-dlp, ffmpeg, YouTube Music, LRCLIB). Run with
//! `cargo test -p geonix-yt --test live -- --ignored --test-threads 1`.

use std::sync::Mutex;

use geonix_yt::{Yt, YtConfig};
use serde_json::{json, Value};

fn yt() -> Yt {
    Yt::new(YtConfig::default())
}

fn scratch(tag: &str) -> std::path::PathBuf {
    let base = std::env::var_os("GEONIX_TEST_TMP").map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let dir = base.join(format!("yt-live-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test]
#[ignore]
async fn live_search() {
    let out = yt().handle("search", json!({"query": "Bohemian Rhapsody", "limit": 10}), |_| {}).await.unwrap();
    let tracks = out["tracks"].as_array().unwrap();
    assert!(!tracks.is_empty() && tracks.len() <= 10);
    assert!(tracks.iter().any(|t| t["artist"].as_str().unwrap_or("").contains("Queen")), "{out}");
    assert!(tracks.iter().all(|t| t["duration_sec"].as_i64().unwrap() > 0));
}

#[tokio::test]
#[ignore]
async fn live_playlist() {
    let url = "https://music.youtube.com/playlist?list=PLOV8g0Lc-ZFLFx3iffsgQfUqrsDk5HOWi";
    let out = yt().handle("playlist_info", json!({ "url": url }), |_| {}).await.unwrap();
    assert!(out["tracks"].as_array().unwrap().len() > 100, "continuations followed");
    let album = yt().handle("playlist_info", json!({"url": "https://music.youtube.com/browse/MPREb_eEpQf8QskKl"}), |_| {}).await.unwrap();
    assert_eq!(album["title"], "A Night At The Opera");
}

#[tokio::test]
#[ignore]
async fn live_lyrics_and_song_info() {
    let y = yt();
    let info = y.handle("song_info", json!({"video_id": "BSTsnWoslP4"}), |_| {}).await.unwrap();
    assert_eq!(info["artist"], "Queen");
    let out = y
        .handle("lyrics", json!({"video_id": "BSTsnWoslP4", "title": "Bohemian Rhapsody", "artist": "Queen", "duration_sec": 355}), |_| {})
        .await
        .unwrap();
    assert_eq!(out["synced"], true, "{out}");
    assert!(out["lines"].as_array().unwrap().len() > 20);
    let rec = y.handle("recommend", json!({"seed_video_ids": ["BSTsnWoslP4"], "limit": 10}), |_| {}).await.unwrap();
    assert!(!rec["tracks"].as_array().unwrap().is_empty());
}

#[tokio::test]
#[ignore]
async fn live_stream_url() {
    let out = yt().handle("stream_url", json!({"video_id": "BSTsnWoslP4"}), |_| {}).await.unwrap();
    assert!(out["url"].as_str().unwrap().starts_with("https://"), "{out}");
    assert!(out["http_headers"].is_object());
}

#[tokio::test]
#[ignore]
async fn live_download_tags_and_moves() {
    let y = yt();
    // A short song keeps this quick.
    let found = y.handle("search", json!({"query": "Queen Seaside Rendezvous", "limit": 10}), |_| {}).await.unwrap();
    let track = found["tracks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["duration_sec"].as_i64().unwrap_or(0) > 30)
        .min_by_key(|t| t["duration_sec"].as_i64().unwrap())
        .cloned()
        .unwrap();
    let dir = scratch("dl");
    let events: Mutex<Vec<Value>> = Mutex::new(vec![]);
    let out = y
        .handle(
            "download",
            json!({
                "video_id": track["video_id"], "dest_dir": dir.join("music").to_string_lossy(),
                "covers_dir": dir.join("covers").to_string_lossy(), "meta": track,
            }),
            |p| events.lock().unwrap().push(p),
        )
        .await
        .unwrap();
    eprintln!("download result: {out}");
    let events = events.into_inner().unwrap();
    eprintln!("{} progress events, first {:?}, last {:?}", events.len(), events.first(), events.last());
    assert_eq!(events.last().unwrap()["percent"], 100.0);
    let path = std::path::PathBuf::from(out["file_path"].as_str().unwrap());
    assert!(path.starts_with(dir.join("music")) && path.extension().unwrap() == "mp3");
    assert!(std::fs::metadata(&path).unwrap().len() > 100_000);
    use id3::TagLike;
    let tag = id3::Tag::read_from_path(&path).unwrap();
    assert_eq!(tag.title(), track["title"].as_str());
    assert_eq!(tag.pictures().count(), 1);
    assert!(std::path::Path::new(out["cover_path"].as_str().unwrap()).exists());
    std::fs::remove_dir_all(&dir).unwrap();
}
